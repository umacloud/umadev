//! Optional semantic vector layer — only active when an OpenAI API key
//! is set AND the `vector` cargo feature is enabled at compile time.
//!
//! When enabled, chunks are embedded via the user's existing OpenAI
//! subscription (`/v1/embeddings`, `text-embedding-3-small`, 1536-dim) and
//! the vectors are searched with brute-force cosine similarity.
//!
//! ## Activation contract
//! 1. **Compile time**: the `vector` cargo feature is on (`--features
//!    umadev-knowledge/vector`). Without it, this whole module compiles
//!    to the offline stub and pulls in zero HTTP dependencies.
//! 2. **Local first (the shipped default)**: with the `vector-local` feature
//!    and the bundled candle model present, embedding runs fully on-device —
//!    no key, no network. Zero setup; this is what a default install uses.
//! 3. **Cloud embedding is OFF by default — explicit opt-in only**: sending
//!    corpus or query text to a REMOTE embeddings endpoint requires BOTH the
//!    dedicated `OPENAI_EMBED_KEY` **and** an explicit `UMADEV_ALLOW_CLOUD_EMBED=1`
//!    opt-in (the single decision seam is [`cloud_embed_key`]). The generic
//!    `OPENAI_API_KEY` NEVER authorizes an upload — a user who set it for some
//!    unrelated OpenAI tool must never have their curated corpus silently
//!    shipped to the cloud.
//! 4. **Config**: `.umadevrc [knowledge] engine = "hybrid"`.
//!
//! `is_enabled()` reports whether a vector *retrieval* channel is plausibly
//! available (a local model, or any OpenAI key for a pre-built / remote store);
//! it gates fusion, NOT uploads. The actual network embed calls are gated
//! strictly by [`cloud_embed_key`], so when cloud embedding is not explicitly
//! opted in the retriever transparently uses the local model or BM25 only —
//! never the cloud.
//!
//! ## Why not HNSW?
//! `hnsw_rs` requires edition-2024 / Rust ≥1.85 and adds a non-trivial
//! dependency. For a corpus of hundreds-to-low-thousands of chunks, a flat
//! `Vec<Vec<f32>>` cosine scan is sub-millisecond and has zero dependencies.
//! HNSW only matters at millions of vectors — out of scope here. (If the
//! corpus ever grows that large, this module is the single swap point.)
//!
//! ## Network policy (fail-open, local-first)
//! - Default install → LOCAL candle embedding, else BM25. No corpus ever leaves
//!   the machine.
//! - Cloud embedding runs ONLY when the user explicitly opts in
//!   (`OPENAI_EMBED_KEY` + `UMADEV_ALLOW_CLOUD_EMBED=1`); a generic
//!   `OPENAI_API_KEY` alone never triggers a network embed.
//! - Cloud opted-in but network fails → returns empty results, logs a warning.
//!   Retrieval NEVER blocks the pipeline; the local model / BM25 is always the
//!   fallback.
//!
//! ## Storage
//! Vectors are cached at `.umadev/kb-index/vectors.bin` (a serde blob).
//! Each stored vector carries a `body_hash` so cache entries are invalidated
//! per-chunk when the source markdown changes, and a `chunk_idx` to align
//! with the BM25 index's positional model (avoiding `(path, section)`
//! collisions when two H2 headings share a name). Re-embedding only happens
//! for chunks whose `body_hash` differs from the cached value.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Embedding dimension for `text-embedding-3-small`.
const EMBED_DIM: usize = 1536;

/// Known model → embedding dimension. Used to validate that a configured
/// model matches the dimension the store was built with — previously
/// `EMBED_DIM` was hardcoded 1536, so switching `active_model()` to
/// `text-embedding-3-large` (3072-dim) would silently reject every query
/// in `search` (which checks `query_vec.len() != self.dim`) with no hint
/// as to why.
const KNOWN_MODEL_DIMS: &[(&str, usize)] = &[
    ("text-embedding-3-small", 1536),
    ("text-embedding-3-large", 3072),
    ("text-embedding-ada-002", 1536),
    ("text-embedding-2", 1536),
];

/// Resolve the effective embedding dimension, in priority order:
/// 1. `UMADEV_EMBED_DIM` env override (explicit user pin),
/// 2. the bundled LOCAL backend's real width when it is the active vector
///    source (`vector-local` + a usable model on disk),
/// 3. the known dimension for [`active_model`] (if it's a recognised model),
/// 4. [`EMBED_DIM`] (1536, the small-model default).
///
/// Returning the env override first lets a user force a non-standard dim
/// even for an unknown model.
///
/// Step 2 is the H3 fix: the bundled local model (e5-small, 384-dim) is tried
/// FIRST at embed time (see [`embed_query`] / [`embed_batch`]), so on a default
/// install the vectors are 384-long — but the HTTP-model default is 1536. If
/// `active_dim()` reported 1536 here, the dim-invalidation guard
/// (`build_vector_store_if_enabled`) would discard the local store on every
/// rebuild (store dim 384 != 1536), and a store mistakenly tagged 1536 would
/// reject every 384-long query. Consulting the local backend's real dimension
/// keeps the whole pipeline on the width the active embedder actually emits.
#[must_use]
pub fn active_dim() -> usize {
    if let Ok(v) = std::env::var("UMADEV_EMBED_DIM") {
        if let Ok(d) = v.parse::<usize>() {
            if d > 0 {
                return d;
            }
        }
    }
    #[cfg(feature = "vector-local")]
    {
        // The local backend is consulted first at embed time, so when it is
        // usable its real width is what the store actually contains.
        if let Some(d) = crate::local_embed::local_dim() {
            return d;
        }
    }
    expected_dim_for_model(active_model()).unwrap_or(EMBED_DIM)
}

/// The embedding dimension a store should be tagged with for a freshly built
/// set of `entries`: the width of the FIRST embedded vector (the real width
/// the active backend produced), or [`active_dim`] when there is nothing to
/// measure. This keeps `store.dim()` aligned with the vectors it holds so
/// `search` accepts queries of the same (real) width. See [`VectorStore::from_embedded`].
fn store_dim(entries: &[(u32, String, String, u64, Vec<f32>)]) -> usize {
    entries.first().map_or_else(active_dim, |e| e.4.len())
}

/// The documented dimension for a known embedding model, or `None` when the
/// model isn't in [`KNOWN_MODEL_DIMS`].
#[must_use]
pub fn expected_dim_for_model(model: &str) -> Option<usize> {
    KNOWN_MODEL_DIMS
        .iter()
        .find(|(name, _)| *name == model)
        .map(|(_, dim)| *dim)
}
/// The DEDICATED, embedding-specific key. This is the ONLY key that can
/// authorize a CLOUD embed (see [`cloud_embed_key`]) — and only when the
/// explicit opt-in flag [`ENV_ALLOW_CLOUD`] is also set. Only referenced when
/// the `vector` feature is on.
#[cfg(feature = "vector")]
const ENV_KEY: &str = "OPENAI_EMBED_KEY";
/// The generic OpenAI key. Consulted ONLY by [`is_enabled`] to decide whether a
/// vector *retrieval* channel is plausibly available (so a pre-built / remote
/// store is still searched) — it NEVER authorizes a cloud upload. The
/// upload-authorizing gate ([`cloud_embed_key`]) deliberately ignores it, so a
/// user who set `OPENAI_API_KEY` for some unrelated tool never has their corpus
/// shipped to the cloud.
#[cfg(feature = "vector")]
const ENV_KEY_FALLBACK: &str = "OPENAI_API_KEY";
/// The explicit "yes, send my corpus/query to a cloud embeddings endpoint"
/// opt-in. Cloud embedding stays OFF unless this is truthy AND [`ENV_KEY`] is
/// set — making leaving the local-only default a loud, intentional act.
#[cfg(feature = "vector")]
const ENV_ALLOW_CLOUD: &str = "UMADEV_ALLOW_CLOUD_EMBED";
#[cfg(feature = "vector")]
const ENV_BASE: &str = "OPENAI_EMBED_BASE";
/// Embedding model. `text-embedding-3-small` is the cheapest high-quality
/// option (~$0.02/M tokens as of 2026) and 1536-dim.
const DEFAULT_MODEL: &str = "text-embedding-3-small";

/// Probe whether ANY OpenAI key is configured (dedicated or generic). Used
/// ONLY by [`is_enabled`] to decide whether a vector *retrieval* channel is
/// plausibly available — it does NOT authorize a network upload. The
/// upload-authorizing gate is the strictly stricter [`cloud_embed_key`]. Only
/// compiled when the `vector` feature is on (the constants it reads are
/// feature-gated).
#[cfg(feature = "vector")]
fn resolve_api_key() -> Option<String> {
    for var in [ENV_KEY, ENV_KEY_FALLBACK] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Whether the user has explicitly opted in to CLOUD embedding via
/// `UMADEV_ALLOW_CLOUD_EMBED`. Truthy = `1` / `true` / `yes` / `on`
/// (case-insensitive, trimmed); anything else (or unset) means NO. This is one
/// half of the two-part cloud gate; the other half is the dedicated key. See
/// [`cloud_embed_key`].
#[cfg(feature = "vector")]
fn cloud_embed_opt_in() -> bool {
    std::env::var(ENV_ALLOW_CLOUD).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Resolve the key authorized to CLOUD-embed, or `None` when cloud embedding is
/// not explicitly enabled. This is the SINGLE decision seam for "should we send
/// corpus/query text to a remote embeddings endpoint", and it is intentionally
/// strict — BOTH must hold:
/// 1. the explicit opt-in flag `UMADEV_ALLOW_CLOUD_EMBED` is truthy, AND
/// 2. the DEDICATED `OPENAI_EMBED_KEY` is set (non-empty).
///
/// The generic `OPENAI_API_KEY` is deliberately NOT consulted here: the product
/// promises local-only RAG, so a key a user set for an unrelated OpenAI tool
/// must never cause their curated corpus to be uploaded. When this returns
/// `None`, embedding stays on the local candle model (if present) or falls back
/// to BM25 — never the cloud. Only compiled with the `vector` feature.
#[cfg(feature = "vector")]
fn cloud_embed_key() -> Option<String> {
    if !cloud_embed_opt_in() {
        return None;
    }
    let key = std::env::var(ENV_KEY).ok()?;
    let key = key.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

/// Whether a vector *retrieval* channel is plausibly available — the single
/// switch the retriever checks before fusing a (pre-built or freshly embedded)
/// vector store. True when a bundled local model is present OR any OpenAI key is
/// configured.
///
/// This is deliberately looser than the CLOUD-upload gate ([`cloud_embed_key`]):
/// it may report `true` for a generic `OPENAI_API_KEY`, but that alone never
/// causes a network embed — the actual embed calls ([`embed_query`] /
/// [`embed_batch`]) authorize an upload strictly, so a generic key degrades to
/// the local model or BM25 rather than shipping corpus to the cloud.
///
/// Without the `vector` cargo feature this is a compile-time `false`.
#[must_use]
pub fn is_enabled() -> bool {
    // A bundled local model (candle) makes vectors available with ZERO user
    // setup — no key, no network. Checked first; the HTTP backend is the
    // fallback for users who supply their own endpoint + key.
    #[cfg(feature = "vector-local")]
    {
        if crate::local_embed::is_available() {
            return true;
        }
    }
    #[cfg(feature = "vector")]
    {
        resolve_api_key().is_some()
    }
    #[cfg(not(feature = "vector"))]
    {
        false
    }
}

/// Resolve the embeddings API base URL. Defaults to OpenAI's public endpoint.
/// Only used when the `vector` feature compiles the HTTP transport in.
#[cfg(feature = "vector")]
fn api_base() -> String {
    std::env::var(ENV_BASE).unwrap_or_else(|_| "https://api.openai.com".to_string())
}

/// One stored embedding with enough metadata to map back to a chunk.
///
/// `body_hash` drives per-chunk cache invalidation: the index builder hashes
/// each chunk's `body` and, on reload, re-embeds only entries whose hash
/// differs. `chunk_idx` aligns this entry with the BM25 index's positional
/// model so two same-named H2 sections in one file don't collide.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredVector {
    /// Chunk path (e.g. `security/login.md`).
    path: String,
    /// H2 section heading.
    section: String,
    /// The embedding vector.
    vec: Vec<f32>,
    /// Content hash of the chunk body at embed time (cache invalidation).
    #[serde(default)]
    body_hash: u64,
    /// Positional index into the BM25 `chunks` vec (collision-safe key).
    #[serde(default)]
    chunk_idx: u32,
}

/// The cached vector store on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStore {
    /// Model used to produce these vectors (invalidation key).
    model: String,
    /// Embedding dimension (sanity check on load).
    dim: usize,
    /// All stored vectors.
    #[serde(default)]
    vectors: Vec<StoredVector>,
    /// Fingerprint of the index's chunk-position mapping at the time the store
    /// was built (`index::corpus_fingerprint`). The retriever compares this to
    /// the live index's fingerprint before keying vector hits on positional
    /// `chunk_idx`; a mismatch means the corpus shifted since the store was
    /// built, so vector fusion is skipped to avoid attributing a stale hit to
    /// the WRONG chunk (MED #4). `#[serde(default)]` keeps old cache blobs (which
    /// have an empty signature) readable — they simply read as "mismatched" until
    /// the next rebuild restamps them, degrading safely to BM25.
    #[serde(default)]
    corpus_sig: String,
}

impl VectorStore {
    /// An empty, disabled store. All operations are no-ops.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            model: String::new(),
            dim: EMBED_DIM,
            vectors: Vec::new(),
            corpus_sig: String::new(),
        }
    }

    /// Load the cached store from disk. Returns the disabled sentinel
    /// (empty) when the file is missing or malformed — never errors.
    #[must_use]
    pub fn load(project_root: &Path) -> Self {
        let path = vectors_path(project_root);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::disabled();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|_| Self::disabled())
    }

    /// Persist the store to disk (best-effort; never errors).
    pub fn save(&self, project_root: &Path) {
        let path = vectors_path(project_root);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = std::fs::write(&path, bytes);
        }
    }

    /// Number of vectors currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Whether the store holds any vectors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// Search the store with a pre-embedded query vector, returning the
    /// `(path, section, score)` triples ranked by descending cosine
    /// similarity. Returns empty when the store is empty.
    ///
    /// This is intentionally synchronous + pure: the query vector must be
    /// obtained separately (via [`embed_query`]) so that the network call
    /// is isolated to the async runner seam and fail-open.
    #[must_use]
    pub fn search(&self, query_vec: &[f32], top_k: usize) -> Vec<(&str, &str, f32)> {
        if self.vectors.is_empty() || query_vec.len() != self.dim || top_k == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(&str, &str, f32)> = self
            .vectors
            .iter()
            .map(|v| {
                let s = cosine(&v.vec, query_vec);
                (v.path.as_str(), v.section.as_str(), s)
            })
            .collect();
        scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        scored
    }

    /// Like [`VectorStore::search`] but returns the collision-safe `chunk_idx`
    /// for each hit as `(chunk_idx, score)` ranked by descending cosine
    /// similarity (P0-2).
    ///
    /// `(path, section)` is NOT a unique chunk key — `Overview`/`Document`
    /// synthetic sections and the `knowledge/` vs `learned/` path overlap make
    /// collisions the norm — so the BM25↔vector fuser must NOT remap vector hits
    /// through it (a collision there silently dropped a legitimate, differently
    /// indexed chunk). Each `StoredVector` already carries the `chunk_idx` it was
    /// built at; this exposes it directly so the fuser keys on the SAME positional
    /// address space as BM25, with no lossy `(path, section)` round-trip.
    #[must_use]
    pub fn search_with_idx(&self, query_vec: &[f32], top_k: usize) -> Vec<(u32, f32)> {
        if self.vectors.is_empty() || query_vec.len() != self.dim || top_k == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(u32, f32)> = self
            .vectors
            .iter()
            .map(|v| (v.chunk_idx, cosine(&v.vec, query_vec)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        scored
    }

    /// Expose the stored entries as `(chunk_idx, path, section, body_hash,
    /// vec)` tuples, for the index builder to diff against current chunks
    /// during incremental re-embedding. This is the cache-reuse accessor.
    #[must_use]
    pub fn cached_for_reuse(&self) -> Vec<(u32, String, String, u64, Vec<f32>)> {
        self.vectors
            .iter()
            .map(|v| {
                (
                    v.chunk_idx,
                    v.path.clone(),
                    v.section.clone(),
                    v.body_hash,
                    v.vec.clone(),
                )
            })
            .collect()
    }

    /// Return all stored vectors as `(chunk_idx, path, section, body_hash)`
    /// so the index builder can diff against current chunks for incremental
    /// re-embedding. Public so `index.rs` can drive cache invalidation.
    #[must_use]
    pub fn entries(&self) -> Vec<(u32, &str, &str, u64)> {
        self.vectors
            .iter()
            .map(|v| {
                (
                    v.chunk_idx,
                    v.path.as_str(),
                    v.section.as_str(),
                    v.body_hash,
                )
            })
            .collect()
    }

    /// Build a fresh store from a list of (chunk_idx, path, section,
    /// body_hash, vec) tuples. Used by the index builder after embedding.
    #[must_use]
    pub fn from_embedded(model: &str, entries: Vec<(u32, String, String, u64, Vec<f32>)>) -> Self {
        // H3 fix: tag the store with the ACTUAL embedding width produced
        // (`vec[0].len()`), NOT `active_dim()`. The active backend (e.g. the
        // bundled 384-dim local model) can emit a different width than the
        // HTTP-model default — baking the 1536 default into a store of 384-long
        // vectors made `search` reject every query on the length mismatch,
        // silently disabling the marketed local semantic layer. Fall back to
        // `active_dim()` only when there is no vector to measure.
        let dim = store_dim(&entries);
        let vectors = entries
            .into_iter()
            .map(|(chunk_idx, path, section, body_hash, vec)| StoredVector {
                path,
                section,
                vec,
                body_hash,
                chunk_idx,
            })
            .collect();
        Self {
            model: model.to_string(),
            dim,
            vectors,
            // Stamped separately by the index builder via `set_corpus_sig` once
            // it has the live index in hand (MED #4); empty here means "unstamped".
            corpus_sig: String::new(),
        }
    }

    /// Replace all stored vectors from raw embedded tuples (used after a
    /// rebuild). Takes the same shape as [`from_embedded`] to avoid leaking
    /// the private [`StoredVector`] type.
    pub fn replace(&mut self, model: &str, entries: Vec<(u32, String, String, u64, Vec<f32>)>) {
        self.model = model.to_string();
        // H3 fix: see [`from_embedded`] — dim follows the real vector width.
        self.dim = store_dim(&entries);
        self.vectors = entries
            .into_iter()
            .map(|(chunk_idx, path, section, body_hash, vec)| StoredVector {
                path,
                section,
                vec,
                body_hash,
                chunk_idx,
            })
            .collect();
    }

    /// Expose the model name this store was built with.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Embedding dimension of the cached vectors. Used to invalidate the cache
    /// when the configured dimension changes (else `search` silently returns
    /// empty on the length mismatch).
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The fingerprint of the chunk-position mapping this store was built over
    /// (`index::corpus_fingerprint`). Empty for an unstamped / legacy store. The
    /// retriever compares this to the live index before fusing vector hits (MED
    /// #4).
    #[must_use]
    pub fn corpus_sig(&self) -> &str {
        &self.corpus_sig
    }

    /// Stamp the store with the live index's chunk-position fingerprint. Called
    /// by the index builder once it has the index in hand (MED #4).
    pub fn set_corpus_sig(&mut self, sig: String) {
        self.corpus_sig = sig;
    }
}

/// Cosine similarity between two equal-length vectors. Returns 0.0 when
/// either vector has zero magnitude (avoids NaN) OR when the result is not
/// finite — a corrupt vector carrying a `NaN`/`inf` component would otherwise
/// poison the ranking (a `NaN` score sorts arbitrarily, an `inf` always wins),
/// so a non-finite score is clamped to 0.0 / treated as "no similarity".
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }
    let score = dot / (mag_a * mag_b);
    if score.is_finite() {
        score
    } else {
        0.0
    }
}

/// On-disk path for the cached vector store.
fn vectors_path(project_root: &Path) -> PathBuf {
    project_root.join(super::KB_INDEX_DIR).join("vectors.bin")
}

// ---------------------------------------------------------------------------
// HTTP transport — only compiled when the `vector` feature is on. Without
// it, embed_query/embed_batch compile to stubs returning None, and the crate
// has no reqwest dependency at all.
// ---------------------------------------------------------------------------

/// Embed a single query string via the OpenAI embeddings API. Returns
/// `None` on any failure (feature off, network, parse, missing key) so the
/// caller can fall back to BM25. This is the query-time embed call; it is
/// `async` and isolated to the runner seam.
#[cfg_attr(not(feature = "vector"), allow(clippy::unused_async))]
pub async fn embed_query(text: &str) -> Option<Vec<f32>> {
    // Local bundled model first (zero setup). candle inference is sync CPU work,
    // so run it off the async executor.
    #[cfg(feature = "vector-local")]
    {
        if crate::local_embed::is_available() {
            let owned = text.to_string();
            let local = tokio::task::spawn_blocking(move || {
                crate::local_embed::embed_texts(std::slice::from_ref(&owned), true)
            })
            .await
            .ok()
            .flatten();
            if let Some(mut v) = local {
                if v.len() == 1 {
                    return Some(v.swap_remove(0));
                }
            }
        }
    }
    #[cfg(feature = "vector")]
    {
        // CLOUD embed only on an explicit opt-in (dedicated key + allow flag).
        // A generic OPENAI_API_KEY resolves to None here → no network, BM25.
        let key = cloud_embed_key()?;
        let url = format!("{}/v1/embeddings", api_base());
        let body = serde_json::json!({ "model": DEFAULT_MODEL, "input": text });
        let mut vecs = http_embed(&url, &key, body).await?;
        if vecs.len() == 1 {
            Some(vecs.pop()?)
        } else {
            None
        }
    }
    #[cfg(not(feature = "vector"))]
    {
        let _ = text;
        None
    }
}

/// Embed many texts in one (or a few batched) API call(s). Returns vectors
/// in input order, or `None` on any failure. Batches internally at
/// [`EMBED_BATCH_MAX`] texts per request to stay within API limits.
#[cfg_attr(not(feature = "vector"), allow(clippy::unused_async))]
pub async fn embed_batch(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    // Local bundled model first (zero setup), off the async executor.
    #[cfg(feature = "vector-local")]
    {
        if crate::local_embed::is_available() {
            if texts.is_empty() {
                return Some(Vec::new());
            }
            let owned = texts.to_vec();
            let local =
                tokio::task::spawn_blocking(move || crate::local_embed::embed_texts(&owned, false))
                    .await
                    .ok()
                    .flatten();
            if let Some(v) = local {
                if v.len() == texts.len() {
                    return Some(v);
                }
            }
        }
    }
    #[cfg(feature = "vector")]
    {
        // CLOUD embed only on an explicit opt-in (dedicated key + allow flag).
        // A generic OPENAI_API_KEY resolves to None here, so a batch of corpus
        // chunks is NEVER uploaded off the back of an unrelated key — the whole
        // point of the local-only promise. Falls open to the local model / BM25.
        let key = cloud_embed_key()?;
        if texts.is_empty() {
            return Some(Vec::new());
        }
        let url = format!("{}/v1/embeddings", api_base());
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(EMBED_BATCH_MAX) {
            let body = serde_json::json!({ "model": DEFAULT_MODEL, "input": chunk });
            let mut vecs = http_embed(&url, &key, body).await?;
            out.append(&mut vecs);
        }
        if out.len() == texts.len() {
            Some(out)
        } else {
            tracing::warn!(
                "embeddings count mismatch: got {} expected {} — discarding batch",
                out.len(),
                texts.len()
            );
            None
        }
    }
    #[cfg(not(feature = "vector"))]
    {
        let _ = texts;
        None
    }
}

/// Maximum input texts per embeddings request. OpenAI allows up to 2048;
/// we keep a conservative cap to bound per-request latency and payload.
#[cfg(feature = "vector")]
const EMBED_BATCH_MAX: usize = 100;

/// Extract the model name the vector layer uses (for cache invalidation).
/// Honours the `UMADEV_EMBED_MODEL` env override so a user can point at
/// `text-embedding-3-large` (or a self-hosted model) without recompiling.
#[must_use]
pub fn active_model() -> &'static str {
    // Resolve the override ONCE into a process-lived `&'static str`. (The old
    // code `Box::leak`ed on every call — bounded but a needless per-call leak
    // since this is read from `active_dim`/`build_*` repeatedly.)
    static MODEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let m = MODEL.get_or_init(|| {
        std::env::var("UMADEV_EMBED_MODEL")
            .ok()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    });
    m.as_str()
}

// ---------------------------------------------------------------------------
// reqwest integration — isolated behind the `vector` feature so the default
// (BM25-only) build of this crate has zero HTTP dependencies.
// ---------------------------------------------------------------------------

/// Exponential backoff sleep for retry attempt `n` (1-indexed):
/// attempt 1 → 0.5s, 2 → 1.0s, 3 → 2.0s. Capped so a slow provider can't
/// stall the pipeline for long. Async so it yields the runtime while waiting.
/// Max attempts for transient-HTTP retries inside [`http_embed`].
#[cfg(feature = "vector")]
const EMBED_MAX_ATTEMPTS: u32 = 3;

#[cfg(feature = "vector")]
async fn backoff_sleep(n: u32) {
    // Exponential backoff: attempt 1 → 0.5s, 2 → 1.0s, … capped at 4s.
    let secs = 0.5_f64 * 2_f64.powi(i32::try_from(n.saturating_sub(1)).unwrap_or(i32::MAX));
    tokio::time::sleep(std::time::Duration::from_secs_f64(secs.min(4.0))).await;
}

#[cfg(feature = "vector")]
async fn http_embed(url: &str, key: &str, body: serde_json::Value) -> Option<Vec<Vec<f32>>> {
    // Pooled client reused across all calls (connection keep-alive + TLS
    // reuse). Built once on first use so startup cost is zero when vectors
    // are never activated.
    let client = EMBED_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(EMBED_TIMEOUT_SECS))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    });

    // Retry transient failures (429 Too Many Requests, 5xx) with exponential
    // backoff. Previously a single 429/500 failed the whole batch and fell
    // back to BM25 for the entire run — even though the provider would have
    // served it a second later. Non-transient errors (4xx other than 429)
    // are NOT retried. Connection errors are retried once (often a transient
    // TLS/dns blip).
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let send_result = client.post(url).bearer_auth(key).json(&body).send().await;
        let resp = match send_result {
            Ok(r) => r,
            Err(e) => {
                if attempt < EMBED_MAX_ATTEMPTS {
                    tracing::warn!("embeddings request error (attempt {attempt}): {e}; retrying");
                    backoff_sleep(attempt).await;
                    continue;
                }
                tracing::warn!(
                    "embeddings request failed after {attempt} attempts (fail-open → BM25): {e}"
                );
                return None;
            }
        };
        let status = resp.status();
        let transient = status.as_u16() == 429 || status.is_server_error();
        if !status.is_success() {
            if transient && attempt < EMBED_MAX_ATTEMPTS {
                tracing::warn!(
                    "embeddings API returned {status} (attempt {attempt}); retrying with backoff"
                );
                backoff_sleep(attempt).await;
                continue;
            }
            tracing::warn!("embeddings API returned {status} (fail-open → BM25)");
            return None;
        }
        let json: EmbedResponse = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("embeddings response parse failed (fail-open → BM25): {e}");
                return None;
            }
        };
        let mut items = json.data;
        // OpenAI may return embeddings out of input order; sort by the `index`
        // field to guarantee alignment with the input batch.
        items.sort_by_key(|d| d.index);
        return Some(items.into_iter().map(|d| d.embedding).collect());
    }
}

/// Per-request timeout for embeddings calls. Generous because a full corpus
/// batch can take a few seconds.
#[cfg(feature = "vector")]
const EMBED_TIMEOUT_SECS: u64 = 60;

/// A pooled reqwest client reused across all embeddings calls. Built lazily
/// so the crate has zero startup cost when vectors are never activated.
#[cfg(feature = "vector")]
static EMBED_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

/// Minimal subset of the OpenAI embeddings response we care about.
#[cfg(feature = "vector")]
#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedItem>,
}

#[cfg(feature = "vector")]
#[derive(Debug, Deserialize)]
struct EmbedItem {
    embedding: Vec<f32>,
    #[allow(dead_code)]
    index: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_store_is_empty() {
        let s = VectorStore::disabled();
        assert!(s.is_empty());
        assert!(s.search(&[0.0; EMBED_DIM], 5).is_empty());
    }

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![0.1, 0.2, 0.3, 0.4];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine(&a, &b).abs() < 1e-5);
    }

    #[test]
    fn cosine_zero_magnitude_returns_zero() {
        let zero = vec![0.0, 0.0, 0.0];
        let v = vec![1.0, 2.0, 3.0];
        assert!(cosine(&zero, &v).abs() < 1e-5);
    }

    #[test]
    fn cosine_non_finite_component_scores_zero() {
        // A corrupt vector carrying NaN/inf must not poison the ranking: cosine
        // returns 0.0 rather than a NaN (sorts arbitrarily) or inf (always wins).
        let good = vec![1.0f32, 0.0, 0.0];
        let nan = vec![f32::NAN, 0.0, 0.0];
        let inf = vec![f32::INFINITY, 0.0, 0.0];
        assert!(cosine(&nan, &good).abs() < 1e-9, "NaN component -> 0.0");
        assert!(cosine(&inf, &good).abs() < 1e-9, "inf component -> 0.0");
        assert!(cosine(&good, &good).is_finite());
    }

    #[test]
    fn store_dim_follows_actual_vector_width_not_model_default() {
        // H3 regression: the bundled local backend emits 384-dim vectors while
        // the HTTP-model default (`active_dim()`) is 1536. A store built from
        // 384-long vectors must be tagged dim=384 (the REAL width) so `search`
        // accepts a 384-long query and returns hits — not silently reject every
        // query on a 384 != 1536 length mismatch (which dead-ended the marketed
        // local semantic layer on every default install).
        let dim = 384usize;
        let mut v0 = vec![0.0f32; dim];
        v0[0] = 1.0;
        let mut v1 = vec![0.0f32; dim];
        v1[1] = 1.0;
        let store = VectorStore::from_embedded(
            "text-embedding-3-small", // default model => active_dim() == 1536
            vec![
                (0, "a".into(), "s".into(), 0, v0.clone()),
                (1, "b".into(), "s".into(), 0, v1),
            ],
        );
        assert_eq!(
            store.dim(),
            dim,
            "store dim must follow the real vector width, not active_dim()"
        );
        let hits = store.search(&v0, 5);
        assert!(
            !hits.is_empty(),
            "a 384-dim query must be accepted and return hits"
        );
        assert_eq!(hits[0].0, "a", "the identical vector ranks first");
        let hits_idx = store.search_with_idx(&v0, 5);
        assert_eq!(
            hits_idx.len(),
            2,
            "search_with_idx must also accept 384-dim"
        );
        assert_eq!(hits_idx[0].0, 0);
    }

    #[test]
    fn replace_dim_follows_actual_vector_width() {
        // Same H3 invariant for the in-place `replace` path the index builder uses.
        let dim = 384usize;
        let mut q = vec![0.0f32; dim];
        q[3] = 1.0;
        let mut store = VectorStore::disabled();
        store.replace(
            "text-embedding-3-small",
            vec![(0, "a".into(), "s".into(), 0, q.clone())],
        );
        assert_eq!(store.dim(), dim, "replace must tag the real vector width");
        assert!(
            !store.search(&q, 3).is_empty(),
            "search must accept the replaced store's real width"
        );
    }

    #[test]
    fn from_embedded_empty_falls_back_to_active_dim() {
        // No vectors to measure => dim follows active_dim() (and `.first()` on
        // an empty entry list must not panic).
        let store = VectorStore::from_embedded("text-embedding-3-small", Vec::new());
        assert_eq!(store.dim(), active_dim());
        assert!(store.is_empty());
    }

    #[test]
    fn search_ranks_by_similarity() {
        let store = VectorStore {
            model: "test".into(),
            dim: 3,
            vectors: vec![
                StoredVector {
                    path: "a".into(),
                    section: "s".into(),
                    vec: vec![1.0, 0.0, 0.0],
                    body_hash: 0,
                    chunk_idx: 0,
                },
                StoredVector {
                    path: "b".into(),
                    section: "s".into(),
                    vec: vec![0.0, 1.0, 0.0],
                    body_hash: 0,
                    chunk_idx: 1,
                },
                StoredVector {
                    path: "c".into(),
                    section: "s".into(),
                    vec: vec![0.9, 0.1, 0.0],
                    body_hash: 0,
                    chunk_idx: 2,
                },
            ],
            corpus_sig: String::new(),
        };
        let query = vec![1.0, 0.0, 0.0];
        let hits = store.search(&query, 3);
        assert_eq!(hits.len(), 3);
        // "a" (identical) ranks first, "c" (close) second, "b" (orthogonal) last.
        assert_eq!(hits[0].0, "a");
        assert_eq!(hits[1].0, "c");
        assert_eq!(hits[2].0, "b");
        assert!((hits[0].2 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn search_with_idx_returns_chunk_idx_in_similarity_order() {
        // P0-2: the collision-safe accessor returns each hit's `chunk_idx` (NOT
        // the lossy (path, section)), in the SAME similarity order as `search`.
        // Two entries deliberately share (path, section) but have distinct
        // chunk_idx — both must be returned distinctly.
        let store = VectorStore {
            model: "test".into(),
            dim: 3,
            vectors: vec![
                StoredVector {
                    path: "security/x.md".into(),
                    section: "Document".into(),
                    vec: vec![1.0, 0.0, 0.0],
                    body_hash: 0,
                    chunk_idx: 5,
                },
                StoredVector {
                    // SAME (path, section) as above — a collision the old remap
                    // would have dropped; here it keeps its own chunk_idx.
                    path: "security/x.md".into(),
                    section: "Document".into(),
                    vec: vec![0.9, 0.1, 0.0],
                    body_hash: 0,
                    chunk_idx: 9,
                },
            ],
            corpus_sig: String::new(),
        };
        let hits = store.search_with_idx(&[1.0, 0.0, 0.0], 3);
        assert_eq!(hits.len(), 2, "both colliding-section entries returned");
        assert_eq!(
            hits[0].0, 5,
            "the identical vector ranks first by chunk_idx"
        );
        assert_eq!(hits[1].0, 9, "the colliding sibling is kept, not dropped");
    }

    #[test]
    fn search_wrong_dim_returns_empty() {
        let store = VectorStore {
            model: "test".into(),
            dim: 3,
            vectors: vec![StoredVector {
                path: "a".into(),
                section: "s".into(),
                vec: vec![1.0; 3],
                body_hash: 0,
                chunk_idx: 0,
            }],
            corpus_sig: String::new(),
        };
        // Query of wrong dimension → empty, not a panic.
        assert!(store.search(&[0.0; 5], 1).is_empty());
    }

    #[test]
    fn store_serialises_round_trip() {
        let store = VectorStore {
            model: "test".into(),
            dim: 2,
            vectors: vec![StoredVector {
                path: "a".into(),
                section: "s".into(),
                vec: vec![1.0, 0.0],
                body_hash: 123,
                chunk_idx: 7,
            }],
            corpus_sig: String::new(),
        };
        let bytes = serde_json::to_vec(&store).unwrap();
        let back: VectorStore = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.dim, 2);
        assert_eq!(back.len(), 1);
        assert_eq!(back.entries()[0].0, 7); // chunk_idx round-trips
        assert_eq!(back.entries()[0].3, 123); // body_hash round-trips
    }

    #[test]
    fn store_backwards_compatible_with_old_cache() {
        // An old cache blob (pre body_hash/chunk_idx) must still load via serde
        // defaults. Simulate by omitting the new fields from the JSON.
        let old_json =
            r#"{"model":"m","dim":2,"vectors":[{"path":"a","section":"s","vec":[1.0,0.0]}]}"#;
        let back: VectorStore = serde_json::from_str(old_json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.entries()[0].0, 0); // chunk_idx defaulted to 0
        assert_eq!(back.entries()[0].3, 0); // body_hash defaulted to 0
                                            // MED #4: an old cache blob with no corpus_sig defaults to empty, so it
                                            // reads as "mismatched" and degrades safely to BM25 until restamped.
        assert_eq!(back.corpus_sig(), "");
    }

    #[test]
    fn corpus_sig_is_set_and_round_trips() {
        // MED #4: the chunk-position fingerprint stamp must persist through serde.
        let mut store =
            VectorStore::from_embedded("m", vec![(0, "a".into(), "s".into(), 0, vec![1.0, 0.0])]);
        assert_eq!(store.corpus_sig(), "", "unstamped by from_embedded");
        store.set_corpus_sig("fp-abc123".into());
        assert_eq!(store.corpus_sig(), "fp-abc123");
        let bytes = serde_json::to_vec(&store).unwrap();
        let back: VectorStore = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.corpus_sig(), "fp-abc123", "corpus_sig round-trips");
    }

    #[test]
    fn from_embedded_builds_store() {
        let store = VectorStore::from_embedded(
            "text-embedding-3-small",
            vec![(0, "a".into(), "s".into(), 42, vec![1.0; EMBED_DIM])],
        );
        assert_eq!(store.len(), 1);
        assert_eq!(store.model(), "text-embedding-3-small");
        assert_eq!(store.entries()[0].3, 42);
    }

    #[test]
    fn is_enabled_false_without_env() {
        // Neutralise any installed local model + hold the env lock, so
        // is_enabled() reflects purely "no HTTP key" (under `vector-local` an
        // installed ~/.umadev model would otherwise make it true).
        let _no_local = crate::testsupport::without_local_model();
        // Clear any API key vars so is_enabled() reflects "no key". These
        // constants only exist under the `vector` feature; without it,
        // is_enabled() is compile-time false regardless.
        #[cfg(feature = "vector")]
        {
            std::env::remove_var(ENV_KEY);
            std::env::remove_var(ENV_KEY_FALLBACK);
            assert!(!is_enabled());
        }
        #[cfg(not(feature = "vector"))]
        {
            assert!(!is_enabled());
        }
    }

    #[tokio::test]
    async fn embed_query_returns_none_without_key() {
        // No API key AND no local backend → None (fail-open to BM25). Neutralise
        // any installed local model so this holds under `vector-local` too.
        let _no_local = crate::testsupport::without_local_model();
        #[cfg(feature = "vector")]
        {
            std::env::remove_var(ENV_KEY);
            std::env::remove_var(ENV_KEY_FALLBACK);
        }
        assert!(embed_query("login").await.is_none());
    }

    #[tokio::test]
    async fn embed_batch_empty_returns_empty() {
        let _no_local = crate::testsupport::without_local_model();
        #[cfg(feature = "vector")]
        {
            std::env::remove_var(ENV_KEY);
            std::env::remove_var(ENV_KEY_FALLBACK);
        }
        // No key + empty input → None (no point embedding nothing).
        let out = embed_batch(&[]).await;
        // Without the feature, always None.
        #[cfg(not(feature = "vector"))]
        assert!(out.is_none());
        #[allow(unused_must_use)]
        {
            let _ = out;
        }
    }

    // P2: the generic OPENAI_API_KEY must NEVER authorize a CLOUD embed. Only the
    // dedicated OPENAI_EMBED_KEY + the explicit UMADEV_ALLOW_CLOUD_EMBED opt-in
    // may. `cloud_embed_key()` is the testable seam on that decision.
    #[cfg(feature = "vector")]
    #[test]
    fn generic_openai_api_key_does_not_authorize_cloud_embed() {
        // Hold the env lock + neutralise any installed local model so the test
        // reflects purely the cloud-gate decision.
        let _no_local = crate::testsupport::without_local_model();
        std::env::remove_var(ENV_KEY);
        std::env::remove_var(ENV_ALLOW_CLOUD);
        // A generic key set for some UNRELATED OpenAI tool.
        std::env::set_var(ENV_KEY_FALLBACK, "sk-generic-unrelated");
        assert!(
            cloud_embed_key().is_none(),
            "a generic OPENAI_API_KEY must NOT authorize cloud embedding"
        );
        // Even with the allow flag on, a generic key alone is NOT the dedicated
        // key, so still no cloud upload.
        std::env::set_var(ENV_ALLOW_CLOUD, "1");
        assert!(
            cloud_embed_key().is_none(),
            "allow flag + generic key (no dedicated key) must still NOT authorize cloud"
        );
        std::env::remove_var(ENV_KEY_FALLBACK);
        std::env::remove_var(ENV_ALLOW_CLOUD);
    }

    // The dedicated OPENAI_EMBED_KEY ALONE is not enough — cloud embedding must be
    // a loud, intentional act, so the explicit opt-in flag is also required.
    #[cfg(feature = "vector")]
    #[test]
    fn embed_key_without_allow_flag_stays_local() {
        let _no_local = crate::testsupport::without_local_model();
        std::env::remove_var(ENV_ALLOW_CLOUD);
        std::env::set_var(ENV_KEY, "sk-embed-specific");
        assert!(
            cloud_embed_key().is_none(),
            "the dedicated key without UMADEV_ALLOW_CLOUD_EMBED must NOT enable cloud"
        );
        std::env::remove_var(ENV_KEY);
    }

    // The intentional path still works: dedicated key + explicit allow flag.
    #[cfg(feature = "vector")]
    #[test]
    fn explicit_opt_in_authorizes_cloud_embed() {
        let _no_local = crate::testsupport::without_local_model();
        // A generic key present alongside must NOT be the one used.
        std::env::set_var(ENV_KEY_FALLBACK, "sk-generic-unrelated");
        std::env::set_var(ENV_KEY, "sk-embed-specific");
        std::env::set_var(ENV_ALLOW_CLOUD, "1");
        assert_eq!(
            cloud_embed_key().as_deref(),
            Some("sk-embed-specific"),
            "dedicated key + explicit opt-in authorizes cloud embedding with the DEDICATED key"
        );
        std::env::remove_var(ENV_KEY);
        std::env::remove_var(ENV_KEY_FALLBACK);
        std::env::remove_var(ENV_ALLOW_CLOUD);
    }

    // The opt-in flag parses a small set of truthy tokens; everything else is NO.
    #[cfg(feature = "vector")]
    #[test]
    fn cloud_embed_opt_in_only_truthy_tokens() {
        let _env = crate::testsupport::env_guard();
        for v in ["1", "true", "TRUE", " yes ", "on"] {
            std::env::set_var(ENV_ALLOW_CLOUD, v);
            assert!(cloud_embed_opt_in(), "{v:?} must read as opted-in");
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            std::env::set_var(ENV_ALLOW_CLOUD, v);
            assert!(!cloud_embed_opt_in(), "{v:?} must read as NOT opted-in");
        }
        std::env::remove_var(ENV_ALLOW_CLOUD);
    }

    // End-to-end: with ONLY a generic key and no opt-in, the corpus batch embed
    // short-circuits to None BEFORE any network call — no corpus leaves the box.
    #[cfg(feature = "vector")]
    #[tokio::test]
    async fn embed_batch_generic_key_only_uploads_nothing() {
        let _no_local = crate::testsupport::without_local_model();
        std::env::remove_var(ENV_KEY);
        std::env::remove_var(ENV_ALLOW_CLOUD);
        std::env::set_var(ENV_KEY_FALLBACK, "sk-generic-unrelated");
        let out = embed_batch(&["a curated corpus chunk".to_string()]).await;
        assert!(
            out.is_none(),
            "a generic OPENAI_API_KEY alone must not upload corpus chunks"
        );
        std::env::remove_var(ENV_KEY_FALLBACK);
    }

    #[test]
    fn expected_dim_maps_known_models() {
        assert_eq!(expected_dim_for_model("text-embedding-3-small"), Some(1536));
        assert_eq!(expected_dim_for_model("text-embedding-3-large"), Some(3072));
        assert_eq!(expected_dim_for_model("text-embedding-ada-002"), Some(1536));
        assert_eq!(expected_dim_for_model("unknown-model"), None);
    }

    // NOTE: these assertions read/write the process-global UMADEV_EMBED_DIM
    // env var, so they must run serially — two parallel #[test]s mutating the
    // same env var race and flake. The env lock + local-backend neutralisation
    // make this deterministic regardless of a model installed at
    // ~/.umadev/embed-model (which would otherwise drive active_dim() to the
    // LOCAL width under the `vector-local` feature).
    #[test]
    fn active_dim_default_and_override() {
        // Hold the env lock + neutralise any installed local model so the
        // model-default branch is what's exercised here.
        let _no_local = crate::testsupport::without_local_model();
        // Clean slate.
        std::env::remove_var("UMADEV_EMBED_DIM");
        std::env::remove_var("UMADEV_EMBED_MODEL");
        assert_eq!(active_dim(), 1536, "default = small-model dim");
        // Explicit override wins.
        std::env::set_var("UMADEV_EMBED_DIM", "3072");
        assert_eq!(active_dim(), 3072, "UMADEV_EMBED_DIM must win");
        // Invalid (0) → fall back to model default.
        std::env::set_var("UMADEV_EMBED_DIM", "0");
        assert_eq!(active_dim(), 1536, "invalid dim falls back");
        std::env::remove_var("UMADEV_EMBED_DIM");
    }

    // H3: with the bundled local backend usable, active_dim() must adopt its
    // REAL width (e5-small = 384), NOT the 1536 HTTP-model default — else the
    // store + dim-invalidation guard disagree and the local layer dead-ends.
    // Only meaningful when the local backend is compiled in.
    #[cfg(feature = "vector-local")]
    #[test]
    fn active_dim_adopts_local_backend_width() {
        let _env = crate::testsupport::env_guard();
        let prev = std::env::var("UMADEV_EMBED_MODEL_DIR").ok();
        std::env::remove_var("UMADEV_EMBED_DIM");
        std::env::remove_var("UMADEV_EMBED_MODEL");
        // A fake model dir advertising hidden_size 384 (the three files only
        // need to EXIST for is_available()).
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"hidden_size":384}"#).unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), "{}").unwrap();
        std::fs::write(dir.path().join("model.safetensors"), b"").unwrap();
        std::env::set_var("UMADEV_EMBED_MODEL_DIR", dir.path());
        assert_eq!(active_dim(), 384, "active_dim must follow the local width");
        match prev {
            Some(v) => std::env::set_var("UMADEV_EMBED_MODEL_DIR", v),
            None => std::env::remove_var("UMADEV_EMBED_MODEL_DIR"),
        }
    }
}
