# @umacloud/model-e5-small

Platform-independent embedding model bundled with [UmaDev](https://github.com/umacloud/umadev)
for **offline, zero-setup** local retrieval. The main `umadev` package depends on
this one (a regular dependency, not platform-specific), so the model ships once
for all OSes.

Contents (filled by the release CI from Hugging Face, SHA-pinned):
- `config.json` — BERT config for `intfloat/multilingual-e5-small`
- `model.safetensors` — f16 weights (~220 MB), 384-dim, bilingual zh + en
- `tokenizer.json` — the tokenizer

The `umadev` launcher sets `UMADEV_EMBED_MODEL_DIR` to this directory; the Rust
binary (built with `--features vector-local`) loads it via candle and embeds
fully offline. If this package is absent, UmaDev fails open to BM25 lexical
retrieval — no error, no network.
