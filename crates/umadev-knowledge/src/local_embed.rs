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
use tokenizers::Tokenizer;

/// Env var pointing at the model directory (must hold `config.json`,
/// `model.safetensors`, `tokenizer.json`). Set by the npm `bin/cli.js` wrapper
/// to the bundled model path under `node_modules`.
const ENV_MODEL_DIR: &str = "UMADEV_EMBED_MODEL_DIR";

/// Whether a usable local model directory is configured and present on disk.
#[must_use]
pub fn is_available() -> bool {
    model_dir().is_some_and(|d| {
        d.join("tokenizer.json").is_file()
            && d.join("config.json").is_file()
            && d.join("model.safetensors").is_file()
    })
}

fn model_dir() -> Option<PathBuf> {
    let d = std::env::var(ENV_MODEL_DIR)
        .ok()
        .filter(|s| !s.is_empty())?;
    let p = PathBuf::from(d);
    p.is_dir().then_some(p)
}

/// Embed `texts` with the bundled local model. `is_query` selects the e5
/// instruction prefix. Returns `None` (fail-open) on any error so the caller
/// can fall back to HTTP / BM25.
#[must_use]
pub fn embed_texts(texts: &[String], is_query: bool) -> Option<Vec<Vec<f32>>> {
    let dir = model_dir()?;
    match embed_inner(&dir, texts, is_query) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!("local embed failed, falling back: {e}");
            None
        }
    }
}

fn embed_inner(dir: &Path, texts: &[String], is_query: bool) -> candle_core::Result<Vec<Vec<f32>>> {
    let device = Device::Cpu;
    let to_msg =
        |e: Box<dyn std::error::Error + Send + Sync>| candle_core::Error::Msg(e.to_string());

    let config_text = std::fs::read_to_string(dir.join("config.json"))
        .map_err(|e| candle_core::Error::Msg(e.to_string()))?;
    let config: Config =
        serde_json::from_str(&config_text).map_err(|e| candle_core::Error::Msg(e.to_string()))?;

    let weights = dir.join("model.safetensors");
    // Safe (non-mmap) load — the crate forbids `unsafe`: read the whole
    // safetensors file into tensors, then build the model.
    let tensors = candle_core::safetensors::load(&weights, &device)?;
    let vb = VarBuilder::from_tensors(tensors, DTYPE, &device);
    let model = BertModel::load(vb, &config)?;
    let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(to_msg)?;

    let prefix = if is_query { "query: " } else { "passage: " };
    let mut out = Vec::with_capacity(texts.len());
    for t in texts {
        let enc = tokenizer
            .encode(format!("{prefix}{t}"), true)
            .map_err(to_msg)?;
        let ids = Tensor::new(enc.get_ids(), &device)?.unsqueeze(0)?;
        let type_ids = ids.zeros_like()?;
        let mask = Tensor::new(enc.get_attention_mask(), &device)?.unsqueeze(0)?;
        let hidden = model.forward(&ids, &type_ids, Some(&mask))?;
        let pooled = mean_pool(&hidden, &mask)?;
        let normed = l2_normalize(&pooled)?;
        out.push(normed.squeeze(0)?.to_vec1::<f32>()?);
    }
    Ok(out)
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
