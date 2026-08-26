//! Bundled local embedding backend — candle (pure Rust, no native ONNX/C++
//! runtime on CPU). Feature-gated behind `vector-local`.
//!
//! Loads a small bilingual BERT-family model (recommended:
//! `multilingual-e5-small`, 384-dim, zh+en) from a directory pointed to by
//! `UMADEV_EMBED_MODEL_DIR` and embeds text **fully offline** — no API key, no
//! network, no separate service. The model ships with the npm package (a
//! platform-independent `@umadev/model-e5-small` dir), so `npm i -g umadev` is
//! the only thing the user installs.
//!
//! **Fail-open by contract:** ANY problem (no model dir, missing files,
//! load/inference error) returns `None`, so the caller degrades to the HTTP
//! backend and then to BM25. The host is never blocked by the embedder.

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokenizers::Tokenizer;
use umadev_state::fs::RootedDir;

/// Env var pointing at the model directory (must hold `config.json`,
/// `model.safetensors`, `tokenizer.json`). Set by the npm `bin/cli.js` wrapper
/// to the bundled model path under `node_modules`.
const ENV_MODEL_DIR: &str = "UMADEV_EMBED_MODEL_DIR";

/// The 8-byte little-endian `u64` header-length prefix that opens a safetensors
/// file (`<N><N-byte JSON header><tensor data>`).
const SAFETENSORS_PREFIX_BYTES: u64 = 8;

/// Minimum plausible size for the bundled model weights. A real
/// `multilingual-e5-small` `model.safetensors` is tens of MB (fp16 ~224MB, f32
/// ~448MB); anything smaller is a truncated/garbage download, not a usable model.
const MIN_SAFETENSORS_BYTES: u64 = 1024 * 1024; // 1 MiB
const MAX_MODEL_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_MODEL_TOKENIZER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MODEL_WEIGHTS_BYTES: u64 = 512 * 1024 * 1024;
const MAX_LOCAL_EMBED_BATCH: usize = 128;
const MAX_LOCAL_EMBED_TEXT_BYTES: usize = 256 * 1024;
const MAX_LOCAL_EMBED_BATCH_BYTES: usize = 8 * 1024 * 1024;

struct ModelSource {
    display_path: PathBuf,
    root: RootedDir,
}

fn read_model_config(source: &ModelSource) -> std::io::Result<String> {
    let bytes = source
        .root
        .read_bounded(Path::new("config.json"), MAX_MODEL_CONFIG_BYTES)?;
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Whether a usable local model directory is configured, present on disk, AND
/// structurally intact. Beyond mere existence, each file is cheaply
/// integrity-checked (the JSON sidecars must open as a JSON object; the weights
/// must clear a size floor and carry a header-length prefix that fits inside the
/// file), so a NON-EMPTY but CORRUPT cache — a truncated download or garbage — is
/// NOT reported as usable. The caller then degrades to BM25 while the npm
/// `bin/cli.js` wrapper re-validates and re-downloads the cache on the next
/// launch (the P3 self-heal fix; previously only file existence was checked, so a
/// corrupt cache healed never). Fail-open: never panics.
#[must_use]
pub fn is_available() -> bool {
    let usable = model_source().is_some_and(|source| model_files_usable(&source));
    if !usable {
        // Honest degrade notice (#2b): a `cargo install` / self-updated user never
        // had the model fetched (only the npm `bin/cli.js` shim downloads it), so
        // without this they'd silently run keyword-only retrieval with no hint.
        note_keyword_only_once();
    }
    usable
}

/// Emit at most one honest notice that retrieval is running WITHOUT the bundled
/// local vector model — so a `cargo install` / self-updated user (for whom nothing
/// ever fetched the model; only the npm `bin/cli.js` shim does) learns their hybrid
/// search silently degraded to keyword-only BM25, and how to turn the vector track
/// on. Purely a log signal: the runtime keeps working on BM25 (fail-open) and never
/// fetches or mutates anything here. Distinct from [`warn_corrupt_cache_once`],
/// which fires only for a present-but-corrupt cache; this also covers the common
/// absent-model case. `umadev doctor` reports the same state as a check row.
fn note_keyword_only_once() {
    static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(
            "no local embedding model present — knowledge retrieval is using keyword-only (BM25). \
             To enable offline hybrid vector search, install via npm (it fetches the model) or place \
             config.json + tokenizer.json + model.safetensors under ~/.umadev/embed-model (or point \
             {ENV_MODEL_DIR} at them)."
        );
    }
}

/// All three model files under `dir` exist AND pass a cheap integrity check.
/// Existence is checked first — an absent cache is a normal first-run state, not
/// corruption — so only when all three are present but one fails validation is a
/// one-time "corrupt cache" warning emitted. Fail-open: any problem returns
/// `false` so the caller drops to BM25.
fn model_files_usable(source: &ModelSource) -> bool {
    let files = [
        ("config.json", MAX_MODEL_CONFIG_BYTES),
        ("tokenizer.json", MAX_MODEL_TOKENIZER_BYTES),
        ("model.safetensors", MAX_MODEL_WEIGHTS_BYTES),
    ];
    if files.iter().any(|(path, max)| {
        !source
            .root
            .regular_file_len(Path::new(path))
            .ok()
            .flatten()
            .is_some_and(|len| len > 0 && len <= *max)
    }) {
        return false; // absent — normal first-run state, not corruption
    }
    let intact = json_sidecar_looks_valid(source, Path::new("config.json"), MAX_MODEL_CONFIG_BYTES)
        && json_sidecar_looks_valid(
            source,
            Path::new("tokenizer.json"),
            MAX_MODEL_TOKENIZER_BYTES,
        )
        && safetensors_looks_valid(source, Path::new("model.safetensors"));
    if !intact {
        warn_corrupt_cache_once(&source.display_path);
    }
    intact
}

/// Cheap validity check for a JSON model sidecar (`config.json` / `tokenizer.json`):
/// exists, is non-empty, and its first non-whitespace byte opens a JSON object
/// (`{`). Only the leading bytes are read, so the large `tokenizer.json` (~17MB)
/// is never fully parsed on this hot path; `config.json` is additionally parsed in
/// full downstream by [`local_dim`] / [`load_model`], both fail-open on a parse
/// error. A truncated or binary-garbage file fails the non-empty / leading-brace
/// check. Fail-open: any read error returns `false`.
fn json_sidecar_looks_valid(source: &ModelSource, path: &Path, max_bytes: u64) -> bool {
    let Ok(Some(size)) = source.root.regular_file_len(path) else {
        return false;
    };
    if size == 0 || size > max_bytes {
        return false;
    }
    let Ok(buf) = source.root.read_prefix(path, 16) else {
        return false;
    };
    buf.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{')
}

/// Cheap structural validity check for a safetensors file WITHOUT loading it. The
/// format is `<u64 LE header-length N><N-byte JSON header><tensor data>`; a
/// truncated or garbage file falls below any plausible model size, or carries a
/// header length that can't fit inside the file. Reads only the 8-byte prefix, so
/// a ~224–448MB model is never slurped into memory on this hot path. Fail-open:
/// any read error returns `false` (a file we cannot validate is treated as
/// unusable, so the caller drops to BM25).
fn safetensors_looks_valid(source: &ModelSource, path: &Path) -> bool {
    let Ok(Some(size)) = source.root.regular_file_len(path) else {
        return false;
    };
    if !(MIN_SAFETENSORS_BYTES..=MAX_MODEL_WEIGHTS_BYTES).contains(&size) {
        return false;
    }
    let Ok(prefix) = source.root.read_prefix(path, SAFETENSORS_PREFIX_BYTES) else {
        return false;
    };
    let Ok(prefix) = <[u8; 8]>::try_from(prefix) else {
        return false;
    };
    let header_len = u64::from_le_bytes(prefix);
    // A non-empty header that fits inside the file after its 8-byte length prefix.
    header_len > 0 && SAFETENSORS_PREFIX_BYTES.saturating_add(header_len) <= size
}

/// Emit at most one warning about a corrupt/incomplete local model cache, so the
/// BM25 degrade is visible in logs without repeating on every retrieval. Purely a
/// signal: the npm `bin/cli.js` wrapper re-validates and re-downloads the cache on
/// the next launch — the Rust runtime never mutates it.
fn warn_corrupt_cache_once(dir: &Path) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            "local embed model cache at {} looks corrupt/incomplete; using BM25 \
             (the npm wrapper re-downloads it on the next launch)",
            dir.display()
        );
    }
}

fn model_source() -> Option<ModelSource> {
    // 1. Explicit override — set by the npm `bin/cli.js` wrapper to the bundled
    //    `@umatech/model-e5-small` package path under `node_modules`.
    if let Some(d) = std::env::var(ENV_MODEL_DIR).ok().filter(|s| !s.is_empty()) {
        let p = PathBuf::from(d);
        if let Ok(root) = RootedDir::open_no_follow(&p) {
            return Some(ModelSource {
                display_path: p,
                root,
            });
        }
    }
    // 2. Conventional local location, auto-discovered with ZERO config: drop the
    //    three model files under `~/.umadev/embed-model` and the pure-Rust local
    //    vector track turns on — no env, no key, no network.
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.is_empty())?;
    let p = PathBuf::from(home).join(".umadev").join("embed-model");
    let root = RootedDir::open_no_follow(&p).ok()?;
    Some(ModelSource {
        display_path: p,
        root,
    })
}

/// The embedding width the bundled local model emits, read from its
/// `config.json` (`hidden_size`). Returns `None` when no usable local model is
/// configured or the config can't be read/parsed (fail-open).
///
/// [`crate::vector::active_dim`] consults this so the vector store + the
/// dim-invalidation guard track the LOCAL width (e5-small = 384) rather than
/// the HTTP-model default (1536) — see the H3 fix.
#[must_use]
pub fn local_dim() -> Option<usize> {
    // Minimal view of `config.json` — only the embedding width matters here.
    #[derive(serde::Deserialize)]
    struct HiddenSize {
        hidden_size: usize,
    }
    let source = model_source()?;
    if !model_files_usable(&source) {
        return None;
    }
    let text = read_model_config(&source).ok()?;
    let cfg: HiddenSize = serde_json::from_str(&text).ok()?;
    (cfg.hidden_size > 0).then_some(cfg.hidden_size)
}

/// A loaded model + tokenizer, cached process-wide so the ~220MB safetensors
/// load + BERT graph build + tokenizer parse happens ONCE, not on every query.
struct LoadedModel {
    model: BertModel,
    tokenizer: Tokenizer,
    /// The model's maximum sequence length (`max_position_embeddings` from
    /// `config.json`; e5-small = 512). A section tokenising to MORE than this
    /// would push `seq_len` past the position-embedding table and make candle
    /// error the whole forward pass — which previously nulled the ENTIRE batch
    /// and silently disabled the bundled local vector layer for any corpus
    /// holding even one long section. Token ids are capped to this before the
    /// forward pass (HIGH #1).
    max_seq_len: usize,
}

struct CacheEntry<T> {
    root: RootedDir,
    value: Arc<T>,
}

struct SingleFlightCache<T> {
    entries: Mutex<Vec<CacheEntry<T>>>,
}

impl<T> SingleFlightCache<T> {
    const fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    fn get_or_try_insert_with<E>(
        &self,
        root: &RootedDir,
        load: impl FnOnce() -> Result<T, E>,
        clone_root: impl FnOnce() -> Result<RootedDir, E>,
    ) -> Result<Arc<T>, E> {
        // Hold one mutex across the first expensive load. This deliberately
        // serializes concurrent cache misses so two queries cannot each
        // materialize a several-hundred-megabyte weights buffer.
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.root.same_directory(root).unwrap_or(false))
        {
            return Ok(Arc::clone(&entry.value));
        }
        let cached_root = clone_root()?;
        let value = Arc::new(load()?);
        // The launcher selects one model directory. Evicting an older
        // generation keeps repeated path replacement from retaining an
        // unbounded number of model graphs.
        entries.clear();
        entries.push(CacheEntry {
            root: cached_root,
            value: Arc::clone(&value),
        });
        Ok(value)
    }
}

/// Process-wide cache. Its single-flight insertion prevents duplicate
/// multi-hundred-megabyte first loads, and directory identity (not an ambient
/// path string) prevents reuse across root replacement.
fn model_cache() -> &'static SingleFlightCache<LoadedModel> {
    static CACHE: OnceLock<SingleFlightCache<LoadedModel>> = OnceLock::new();
    CACHE.get_or_init(SingleFlightCache::new)
}

/// Fetch the cached model for `dir`, loading + caching it on first use. Returns
/// `None` (fail-open) on any load error, WITHOUT caching the failure so a
/// transient problem can be retried.
fn cached_model(source: &ModelSource) -> candle_core::Result<Arc<LoadedModel>> {
    model_cache().get_or_try_insert_with(
        &source.root,
        || load_model(source),
        || {
            source
                .root
                .try_clone()
                .map_err(|error| candle_core::Error::Msg(error.to_string()))
        },
    )
}

/// Read + build the model and tokenizer from `dir`. The expensive part that the
/// [`model_cache`] memoises.
fn load_model(source: &ModelSource) -> candle_core::Result<LoadedModel> {
    let device = Device::Cpu;
    let to_msg =
        |e: Box<dyn std::error::Error + Send + Sync>| candle_core::Error::Msg(e.to_string());

    if !model_files_usable(source) {
        return Err(candle_core::Error::Msg(
            "local model files are missing, linked, corrupt, or oversized".to_string(),
        ));
    }
    let config_text =
        read_model_config(source).map_err(|e| candle_core::Error::Msg(e.to_string()))?;
    let config: Config =
        serde_json::from_str(&config_text).map_err(|e| candle_core::Error::Msg(e.to_string()))?;

    // Safe (non-mmap) load — the crate forbids `unsafe`: read the whole
    // safetensors file into tensors, then build the model.
    let weights = source
        .root
        .read_bounded(Path::new("model.safetensors"), MAX_MODEL_WEIGHTS_BYTES)
        .map_err(|error| candle_core::Error::Msg(error.to_string()))?;
    let tensors = candle_core::safetensors::load_buffer(&weights, &device)?;
    drop(weights);
    let vb = VarBuilder::from_tensors(tensors, DTYPE, &device);
    let model = BertModel::load(vb, &config)?;
    let tokenizer_bytes = source
        .root
        .read_bounded(Path::new("tokenizer.json"), MAX_MODEL_TOKENIZER_BYTES)
        .map_err(|error| candle_core::Error::Msg(error.to_string()))?;
    let tokenizer = Tokenizer::from_bytes(tokenizer_bytes).map_err(to_msg)?;
    let max_seq_len = read_max_seq_len(&config_text);
    Ok(LoadedModel {
        model,
        tokenizer,
        max_seq_len,
    })
}

/// The model's position-embedding limit, read from the raw `config.json` text
/// (NOT via a candle struct field, so it is robust to candle config-shape
/// changes). Falls back to 512 (the e5/BERT-family default) when the field is
/// absent, unparseable, or zero. This is the cap [`embed_one`] truncates token
/// ids to so an over-long section can't error the forward pass (HIGH #1).
fn read_max_seq_len(config_text: &str) -> usize {
    #[derive(serde::Deserialize)]
    struct MaxPos {
        #[serde(default)]
        max_position_embeddings: usize,
    }
    serde_json::from_str::<MaxPos>(config_text)
        .ok()
        .map(|m| m.max_position_embeddings)
        .filter(|&n| n > 0)
        .unwrap_or(512)
}

/// Embed `texts` with the bundled local model. `is_query` selects the e5
/// instruction prefix. Returns `None` (fail-open) on any error so the caller
/// can fall back to HTTP / BM25.
#[must_use]
pub fn embed_texts(texts: &[String], is_query: bool) -> Option<Vec<Vec<f32>>> {
    let bounded = bounded_embed_inputs(texts)?;
    let source = model_source()?;
    match embed_inner(&source, &bounded, is_query) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!("local embed failed, falling back: {e}");
            None
        }
    }
}

fn embed_inner(
    source: &ModelSource,
    texts: &[&str],
    is_query: bool,
) -> candle_core::Result<Vec<Vec<f32>>> {
    let device = Device::Cpu;
    // Reuse the cached (model, tokenizer) — loaded ONCE per process, not per
    // query. The heavy safetensors load + graph build happens on first use only.
    let loaded = cached_model(source)?;
    let prefix = if is_query { "query: " } else { "passage: " };
    // Embed each text INDEPENDENTLY so one bad text (a tokeniser quirk, an
    // unexpected per-row inference error) can't null the whole batch — a failed
    // row is zero-filled below to the width of a good one, keeping the
    // input-aligned length the caller checks. Over-long sections are token-capped
    // inside `embed_one` so they no longer error at all (HIGH #1).
    let rows: Vec<Option<Vec<f32>>> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| match embed_one(&loaded, &device, prefix, t) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!(
                    "local embed: text {i} failed ({e}); zero-filling to keep the batch"
                );
                None
            }
        })
        .collect();
    assemble_batch(rows).ok_or_else(|| {
        candle_core::Error::Msg("local embed produced no usable vectors (every text failed)".into())
    })
}

fn bounded_embed_inputs(texts: &[String]) -> Option<Vec<&str>> {
    if texts.len() > MAX_LOCAL_EMBED_BATCH {
        return None;
    }
    let mut total = 0usize;
    let mut bounded = Vec::with_capacity(texts.len());
    for text in texts {
        let text = bounded_text_prefix(text, MAX_LOCAL_EMBED_TEXT_BYTES);
        total = total.checked_add(text.len())?;
        if total > MAX_LOCAL_EMBED_BATCH_BYTES {
            return None;
        }
        bounded.push(text);
    }
    Some(bounded)
}

fn bounded_text_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Embed ONE text. The token ids are capped to the model's `max_seq_len`
/// (e5-small = 512) before the forward pass, so a section longer than the
/// model's context window embeds (truncated) rather than making the BertModel
/// error — the root of HIGH #1, where one long curated section nulled the whole
/// batch and silently disabled the marketed local fp16 layer.
fn embed_one(
    loaded: &LoadedModel,
    device: &Device,
    prefix: &str,
    text: &str,
) -> candle_core::Result<Vec<f32>> {
    let to_msg =
        |e: Box<dyn std::error::Error + Send + Sync>| candle_core::Error::Msg(e.to_string());
    let enc = loaded
        .tokenizer
        .encode(format!("{prefix}{text}"), true)
        .map_err(to_msg)?;
    let cap = capped_len(enc.get_ids().len(), loaded.max_seq_len);
    let ids = Tensor::new(&enc.get_ids()[..cap], device)?.unsqueeze(0)?;
    let type_ids = ids.zeros_like()?;
    let mask = Tensor::new(&enc.get_attention_mask()[..cap], device)?.unsqueeze(0)?;
    let hidden = loaded.model.forward(&ids, &type_ids, Some(&mask))?;
    let pooled = mean_pool(&hidden, &mask)?;
    let normed = l2_normalize(&pooled)?;
    normed.squeeze(0)?.to_vec1::<f32>()
}

/// The number of leading tokens to feed the model: at most `max` (the model's
/// position-embedding limit). Capping here instead of letting the BertModel
/// error on `seq_len > max_position_embeddings` is the HIGH #1 fix. `max.max(1)`
/// guards against a degenerate (zero) limit ever producing an empty slice.
fn capped_len(len: usize, max: usize) -> usize {
    len.min(max.max(1))
}

/// Assemble per-text embedding outcomes into a dense, input-aligned batch. A
/// failed row (`None`) is zero-filled to the width of the first successful row,
/// so a single bad text can't null the whole batch. Returns `None` (fail-open)
/// ONLY when every row failed (no width to fill to) — the caller then drops to
/// the HTTP backend / BM25.
fn assemble_batch(rows: Vec<Option<Vec<f32>>>) -> Option<Vec<Vec<f32>>> {
    let dim = rows.iter().flatten().map(Vec::len).next()?;
    Some(
        rows.into_iter()
            .map(|r| r.unwrap_or_else(|| vec![0.0; dim]))
            .collect(),
    )
}

/// Attention-masked mean pooling over the token dimension. `hidden` is
/// `[1, n_tokens, dim]`, `mask` is `[1, n_tokens]`.
fn mean_pool(hidden: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
    let mask_f = mask.to_dtype(DTYPE)?.unsqueeze(2)?;
    let summed = hidden.broadcast_mul(&mask_f)?.sum(1)?;
    let counts = mask_f.sum(1)?;
    summed.broadcast_div(&counts)
}

/// L2-normalise each row of a `[1, dim]` tensor (cosine-ready).
fn l2_normalize(v: &Tensor) -> candle_core::Result<Tensor> {
    let norm = v.sqr()?.sum_keepdim(1)?.sqrt()?;
    v.broadcast_div(&norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(dir: &Path) -> ModelSource {
        ModelSource {
            display_path: dir.to_path_buf(),
            root: RootedDir::open_no_follow(dir).unwrap(),
        }
    }

    fn write_structurally_valid_model(dir: &Path) {
        std::fs::write(dir.join("config.json"), r#"{"hidden_size":384}"#).unwrap();
        std::fs::write(dir.join("tokenizer.json"), r#"{"model":{}}"#).unwrap();
        write_valid_safetensors(&dir.join("model.safetensors"));
    }

    /// Write a size-plausible, structurally-valid `model.safetensors`: an 8-byte
    /// little-endian header-length prefix, that many header bytes, then zero
    /// padding past the 1 MiB size floor. Enough to pass [`safetensors_looks_valid`]
    /// (which never loads tensors) — NOT a loadable model.
    fn write_valid_safetensors(path: &Path) {
        let header = br#"{"weight":{"dtype":"F32","shape":[1],"data_offsets":[0,4]}}"#;
        let mut bytes = u64::try_from(header.len()).unwrap().to_le_bytes().to_vec();
        bytes.extend_from_slice(header);
        bytes.resize(usize::try_from(MIN_SAFETENSORS_BYTES).unwrap() + 16, 0);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn local_dim_reads_hidden_size_and_drives_active_dim() {
        // H3: with a usable local model present, the REAL embedding width
        // (config.json `hidden_size`, e5-small = 384) must govern — both
        // local_dim() directly AND vector::active_dim() (which consults it),
        // so the store + dim-guard don't default to the 1536 HTTP-model width.
        // Hold the process-wide env lock so the ENV_MODEL_DIR / UMADEV_EMBED_DIM
        // mutations don't race the vector/index tests.
        let _env = crate::testsupport::env_guard();
        let prev = std::env::var(ENV_MODEL_DIR).ok();
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        // A minimal but parseable config carrying hidden_size, a JSON tokenizer
        // sidecar, and a size-plausible, structurally-valid safetensors so
        // is_available()'s integrity check (not just existence) passes.
        std::fs::write(dir.join("config.json"), r#"{"hidden_size": 384}"#).unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        write_valid_safetensors(&dir.join("model.safetensors"));

        std::env::set_var(ENV_MODEL_DIR, dir);
        std::env::remove_var("UMADEV_EMBED_DIM");
        std::env::remove_var("UMADEV_EMBED_MODEL");

        assert!(is_available(), "all three model files present and intact");
        assert_eq!(local_dim(), Some(384), "hidden_size read from config.json");
        assert_eq!(
            crate::vector::active_dim(),
            384,
            "active_dim() must adopt the local backend's real width (H3)"
        );

        match prev {
            Some(v) => std::env::set_var(ENV_MODEL_DIR, v),
            None => std::env::remove_var(ENV_MODEL_DIR),
        }
    }

    #[test]
    fn capped_len_truncates_oversize_sections() {
        // HIGH #1: a section tokenising to MORE than the model's position limit
        // is capped to the limit (so it embeds, truncated, instead of erroring
        // the forward pass and nulling the whole batch).
        assert_eq!(capped_len(1000, 512), 512, "over-limit length is capped");
        assert_eq!(capped_len(10, 512), 10, "under-limit length is untouched");
        assert_eq!(capped_len(512, 512), 512, "exactly-at-limit is kept");
        // Defensive: a degenerate (zero) limit must never produce an empty slice.
        assert_eq!(capped_len(5, 0), 1);
    }

    #[test]
    fn raw_embed_inputs_are_bounded_before_tokenization() {
        let multibyte = format!("{}界", "a".repeat(MAX_LOCAL_EMBED_TEXT_BYTES - 1));
        let inputs = vec![multibyte];
        let bounded = bounded_embed_inputs(&inputs).unwrap();
        assert_eq!(bounded.len(), 1);
        assert_eq!(bounded[0].len(), MAX_LOCAL_EMBED_TEXT_BYTES - 1);
        assert!(bounded[0].is_char_boundary(bounded[0].len()));

        let too_many = vec![String::new(); MAX_LOCAL_EMBED_BATCH + 1];
        assert!(bounded_embed_inputs(&too_many).is_none());

        let over_aggregate = vec![
            "x".repeat(MAX_LOCAL_EMBED_TEXT_BYTES);
            MAX_LOCAL_EMBED_BATCH_BYTES / MAX_LOCAL_EMBED_TEXT_BYTES + 1
        ];
        assert!(bounded_embed_inputs(&over_aggregate).is_none());
    }

    #[test]
    fn assemble_batch_zero_fills_a_failed_row_not_the_whole_batch() {
        // HIGH #1: one bad text (None) must NOT null the batch — it is zero-filled
        // to the width of a good row, and the batch length stays input-aligned
        // (so the caller's `len == texts.len()` check still passes and the rest of
        // the corpus keeps its real vectors).
        let good = vec![0.1f32, 0.2, 0.3];
        let rows = vec![Some(good.clone()), None, Some(good.clone())];
        let out = assemble_batch(rows).expect("a good row exists -> Some");
        assert_eq!(out.len(), 3, "length stays aligned with the input");
        assert_eq!(out[0], good);
        assert_eq!(
            out[1],
            vec![0.0; 3],
            "the failed row is zero-filled to width 3"
        );
        assert_eq!(out[2], good);
    }

    #[test]
    fn read_max_seq_len_reads_config_and_falls_back() {
        // HIGH #1: the truncation cap comes from config.json; a missing/zero
        // field falls back to 512 (the e5/BERT default) so capping always happens.
        assert_eq!(read_max_seq_len(r#"{"max_position_embeddings": 512}"#), 512);
        assert_eq!(read_max_seq_len(r#"{"max_position_embeddings": 256}"#), 256);
        assert_eq!(
            read_max_seq_len(r#"{"hidden_size": 384}"#),
            512,
            "absent -> 512"
        );
        assert_eq!(read_max_seq_len("not json"), 512, "unparseable -> 512");
        assert_eq!(
            read_max_seq_len(r#"{"max_position_embeddings": 0}"#),
            512,
            "zero -> 512"
        );
    }

    #[test]
    fn assemble_batch_all_failed_is_none() {
        // Only when EVERY row failed (no width to fill to) do we fail open to None
        // so the caller drops to the HTTP backend / BM25.
        let rows: Vec<Option<Vec<f32>>> = vec![None, None];
        assert!(
            assemble_batch(rows).is_none(),
            "all-failed -> None (fail-open)"
        );
        assert!(
            assemble_batch(Vec::new()).is_none(),
            "an empty batch has no width -> None"
        );
    }

    #[test]
    fn local_dim_is_none_without_model_files() {
        // An existing dir that is MISSING the three model files => is_available()
        // is false => local_dim() is None (fail-open), so active_dim() falls back
        // to the model default. `without_local_model` points ENV_MODEL_DIR at an
        // empty dir (and holds the env lock), so this is deterministic regardless
        // of the machine's ~/.umadev fallback.
        let _no_local = crate::testsupport::without_local_model();
        assert!(!is_available());
        assert_eq!(local_dim(), None);
    }

    #[test]
    fn corrupt_cache_is_not_usable_and_falls_back() {
        // P3: a NON-EMPTY but corrupt model set (valid JSON sidecars, a >1 MiB but
        // truncated safetensors whose header-length prefix points past EOF) must
        // NOT be treated as usable — is_available() is false so the caller degrades
        // to BM25 while the npm wrapper re-downloads on the next launch. Previously
        // is_available() only checked the three files EXIST, so this healed never.
        let _env = crate::testsupport::env_guard();
        let prev = std::env::var(ENV_MODEL_DIR).ok();
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("config.json"), r#"{"hidden_size":384}"#).unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        // > 1 MiB (clears the size floor) but the 8-byte header-length prefix claims
        // a header far larger than the file — the signature of a truncated download.
        let mut bad = u64::MAX.to_le_bytes().to_vec();
        bad.resize(usize::try_from(MIN_SAFETENSORS_BYTES).unwrap() + 16, 0);
        std::fs::write(dir.join("model.safetensors"), &bad).unwrap();

        std::env::set_var(ENV_MODEL_DIR, dir);
        assert!(!is_available(), "corrupt safetensors -> not usable (P3)");
        assert_eq!(local_dim(), None, "not usable -> local_dim None -> BM25");

        match prev {
            Some(v) => std::env::set_var(ENV_MODEL_DIR, v),
            None => std::env::remove_var(ENV_MODEL_DIR),
        }
    }

    #[test]
    fn valid_complete_cache_is_usable() {
        // Counterpart to the corrupt case: a size-plausible safetensors whose
        // header fits, plus JSON sidecars that open as objects, IS usable — a
        // HEALTHY cache still loads without being falsely flagged corrupt.
        let _env = crate::testsupport::env_guard();
        let prev = std::env::var(ENV_MODEL_DIR).ok();
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("config.json"), r#"{"hidden_size":384}"#).unwrap();
        std::fs::write(dir.join("tokenizer.json"), r#"{"model":{}}"#).unwrap();
        write_valid_safetensors(&dir.join("model.safetensors"));

        std::env::set_var(ENV_MODEL_DIR, dir);
        assert!(is_available(), "complete + intact set is usable");

        match prev {
            Some(v) => std::env::set_var(ENV_MODEL_DIR, v),
            None => std::env::remove_var(ENV_MODEL_DIR),
        }
    }

    #[test]
    fn safetensors_looks_valid_rejects_corrupt_accepts_sane() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = source(tmp.path());
        let good = tmp.path().join("good.safetensors");
        write_valid_safetensors(&good);
        assert!(
            safetensors_looks_valid(&source, Path::new("good.safetensors")),
            "size + fitting header -> valid"
        );

        // Empty: below the 8-byte prefix and the size floor.
        let empty = tmp.path().join("empty.safetensors");
        std::fs::write(&empty, b"").unwrap();
        assert!(
            !safetensors_looks_valid(&source, Path::new("empty.safetensors")),
            "empty -> invalid"
        );

        // Non-empty but under the 1 MiB size floor (a truncated download).
        let tiny = tmp.path().join("tiny.safetensors");
        std::fs::write(&tiny, vec![0u8; 1024]).unwrap();
        assert!(
            !safetensors_looks_valid(&source, Path::new("tiny.safetensors")),
            "under size floor -> invalid"
        );

        // > 1 MiB but the header length points past EOF (garbage/truncated prefix).
        let trunc = tmp.path().join("trunc.safetensors");
        let mut bad = u64::MAX.to_le_bytes().to_vec();
        bad.resize(usize::try_from(MIN_SAFETENSORS_BYTES).unwrap() + 16, 0);
        std::fs::write(&trunc, &bad).unwrap();
        assert!(
            !safetensors_looks_valid(&source, Path::new("trunc.safetensors")),
            "header past EOF -> invalid"
        );

        // Missing file.
        assert!(!safetensors_looks_valid(
            &source,
            Path::new("nope.safetensors")
        ));
    }

    #[test]
    fn json_sidecar_looks_valid_checks_open_and_nonempty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = source(tmp.path());
        let ok = tmp.path().join("config.json");
        std::fs::write(&ok, r#"{"hidden_size":384}"#).unwrap();
        assert!(json_sidecar_looks_valid(
            &source,
            Path::new("config.json"),
            MAX_MODEL_CONFIG_BYTES
        ));

        let ws = tmp.path().join("ws.json");
        std::fs::write(&ws, "  \n\t {\"a\":1}").unwrap();
        assert!(
            json_sidecar_looks_valid(&source, Path::new("ws.json"), MAX_MODEL_CONFIG_BYTES),
            "leading whitespace tolerated"
        );

        let empty = tmp.path().join("empty.json");
        std::fs::write(&empty, b"").unwrap();
        assert!(
            !json_sidecar_looks_valid(&source, Path::new("empty.json"), MAX_MODEL_CONFIG_BYTES),
            "empty -> invalid"
        );

        let garbage = tmp.path().join("garbage.json");
        std::fs::write(&garbage, b"\x00\x01\x02not json").unwrap();
        assert!(
            !json_sidecar_looks_valid(&source, Path::new("garbage.json"), MAX_MODEL_CONFIG_BYTES),
            "binary garbage -> invalid"
        );

        assert!(!json_sidecar_looks_valid(
            &source,
            Path::new("missing.json"),
            MAX_MODEL_CONFIG_BYTES
        ));
    }

    #[test]
    fn oversized_model_files_are_rejected_before_materialization() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_structurally_valid_model(tmp.path());

        std::fs::OpenOptions::new()
            .write(true)
            .open(tmp.path().join("config.json"))
            .unwrap()
            .set_len(MAX_MODEL_CONFIG_BYTES + 1)
            .unwrap();
        assert!(!model_files_usable(&source(tmp.path())));

        write_structurally_valid_model(tmp.path());
        std::fs::OpenOptions::new()
            .write(true)
            .open(tmp.path().join("tokenizer.json"))
            .unwrap()
            .set_len(MAX_MODEL_TOKENIZER_BYTES + 1)
            .unwrap();
        assert!(!model_files_usable(&source(tmp.path())));

        write_structurally_valid_model(tmp.path());
        std::fs::OpenOptions::new()
            .write(true)
            .open(tmp.path().join("model.safetensors"))
            .unwrap()
            .set_len(MAX_MODEL_WEIGHTS_BYTES + 1)
            .unwrap();
        assert!(!model_files_usable(&source(tmp.path())));
    }

    #[cfg(unix)]
    #[test]
    fn model_roots_and_files_reject_symlinks() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::TempDir::new().unwrap();
        let model = parent.path().join("model");
        std::fs::create_dir(&model).unwrap();
        write_structurally_valid_model(&model);

        let linked_root = parent.path().join("linked-model");
        symlink(&model, &linked_root).unwrap();
        assert!(RootedDir::open_no_follow(&linked_root).is_err());

        let outside = parent.path().join("outside-tokenizer.json");
        std::fs::write(&outside, r#"{"model":{}}"#).unwrap();
        std::fs::remove_file(model.join("tokenizer.json")).unwrap();
        symlink(&outside, model.join("tokenizer.json")).unwrap();
        assert!(!model_files_usable(&source(&model)));
    }

    #[cfg(unix)]
    #[test]
    fn opened_model_source_stays_on_one_directory_generation() {
        let parent = tempfile::TempDir::new().unwrap();
        let model = parent.path().join("model");
        std::fs::create_dir(&model).unwrap();
        write_structurally_valid_model(&model);
        let source = source(&model);

        let moved = parent.path().join("model-moved");
        std::fs::rename(&model, &moved).unwrap();
        std::fs::create_dir(&model).unwrap();
        std::fs::write(model.join("config.json"), r#"{"hidden_size":999}"#).unwrap();

        assert!(model_files_usable(&source));
        assert!(read_model_config(&source).unwrap().contains("384"));
    }

    #[test]
    fn model_cache_single_flights_concurrent_first_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = Arc::new(SingleFlightCache::<usize>::new());
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut threads = Vec::new();

        for _ in 0..12 {
            let cache = Arc::clone(&cache);
            let loads = Arc::clone(&loads);
            let root = RootedDir::open_no_follow(tmp.path()).unwrap();
            threads.push(std::thread::spawn(move || {
                cache
                    .get_or_try_insert_with(
                        &root,
                        || -> std::io::Result<usize> {
                            loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            Ok(384)
                        },
                        || root.try_clone(),
                    )
                    .unwrap()
            }));
        }

        for thread in threads {
            assert_eq!(*thread.join().unwrap(), 384);
        }
        assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
