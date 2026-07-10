//! BM25 inverted index — the default (offline, zero-dependency) retrieval engine.
//!
//! BM25 is the industry-standard lexical ranking function (what Elasticsearch
//! / Lucene use). It outperforms plain TF-IDF by saturating term frequency,
//! so a term appearing 100× in one doc isn't 100× more relevant than 10×.
//!
//! The index is a pure data structure: build once from chunks, query N times.
//! It serialises to `.umadev/kb-index/bm25.bin` (via serde_json) and is
//! rebuilt only when source `.md` mtimes change (see [`load_or_build_index`]).
//!
//! ## Why not HNSW here?
//! HNSW (`hnsw_rs`) is the vector layer in [`crate::vector`]. BM25 is
//! keyword-exact and needs no approximate search — a flat inverted index
//! scanned per query term is both faster and deterministic for this corpus
//! size (hundreds of chunks).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::chunker::Chunk;
use crate::tokenizer::tokenize;
use crate::vector;

/// Classic BM25 tunables. These defaults (k1=1.2, b=0.75) are the values
/// every major search engine ships with; changing them is rarely warranted.
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Schema version of the on-disk BM25 cache. The corpus signature keys on file
/// CONTENT (machine-independent), but content alone doesn't capture a change to
/// the TOKENIZER / chunker / index layout: after such an upgrade the cached
/// `bm25.bin` is silently stale until a source file happens to change. Bumping
/// this constant invalidates every cache built by an older schema (the version
/// is folded into [`corpus_signature`], so an old `.sig` can no longer match).
/// Bump it whenever `tokenizer::tokenize`, the chunker, or the `Bm25Index`
/// layout changes in a way that alters indexed tokens.
const INDEX_SCHEMA_VERSION: u32 = 2;

/// One inverted-index entry: the term, and the chunks that contain it with
/// per-chunk term frequency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Posting {
    /// The tokenised term.
    pub term: String,
    /// (chunk_index, term_frequency_in_that_chunk) pairs.
    pub docs: Vec<(u32, u32)>,
}

/// The complete BM25 index, serialisable to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bm25Index {
    /// All chunks, indexed by position. Query results reference back here.
    pub chunks: Vec<Chunk>,
    /// Inverted index: term → postings.
    pub postings: Vec<Posting>,
    /// For fast term lookup during query: term → index into `postings`.
    /// Serialised as a Vec of (term, idx) and rebuilt into a HashMap on load
    /// to avoid storing a non-serde-friendly HashMap directly.
    pub term_map: Vec<(String, u32)>,
    /// Document frequency cache: term → number of chunks containing it.
    /// (Length of the matching Posting's docs vec — stored redundantly for
    /// query speed.)
    /// Average chunk length in tokens.
    pub avg_doc_len: f64,
    /// Total number of chunks. Equal to `chunks.len()`; stored explicitly
    /// so the query function doesn't borrow `self.chunks`.
    pub doc_count: u64,
}

impl Bm25Index {
    /// Build an index from a set of chunks. Pure; no I/O.
    #[must_use]
    pub fn from_chunks(chunks: Vec<Chunk>) -> Self {
        let doc_count = u64::try_from(chunks.len()).unwrap_or(0);
        // Document length for BM25 length-normalisation is the BIGRAM-channel
        // token count (`bm25_len`), NOT `tokens.len()`: the latter also includes
        // the appended CJK-trigram view (a separate channel), which would inflate
        // `avgdl` and perturb the bigram channel's per-term scores.
        let total_len: usize = chunks.iter().map(Chunk::bm25_len).sum();
        let avg_doc_len = if chunks.is_empty() {
            0.0
        } else {
            total_len as f64 / chunks.len() as f64
        };

        // Build the inverted index: term → HashMap<chunk_idx, tf>.
        let mut inverted: HashMap<String, HashMap<u32, u32>> = HashMap::new();
        for (idx, chunk) in chunks.iter().enumerate() {
            let chunk_idx = u32::try_from(idx).unwrap_or(u32::MAX);
            let mut seen_in_doc: HashMap<&str, u32> = HashMap::new();
            for tok in &chunk.tokens {
                *seen_in_doc.entry(tok.as_str()).or_insert(0) += 1;
            }
            for (term, tf) in seen_in_doc {
                inverted
                    .entry(term.to_string())
                    .or_default()
                    .insert(chunk_idx, tf);
            }
        }

        // Flatten into Posting list + a term→idx map.
        let mut postings = Vec::with_capacity(inverted.len());
        let mut term_map = Vec::with_capacity(inverted.len());
        for (i, (term, docs_map)) in inverted.into_iter().enumerate() {
            let idx = u32::try_from(i).unwrap_or(u32::MAX);
            term_map.push((term.clone(), idx));
            let mut docs: Vec<(u32, u32)> = docs_map.into_iter().collect();
            docs.sort_unstable_by_key(|(c, _)| *c);
            postings.push(Posting { term, docs });
        }
        term_map.sort_by(|a, b| a.0.cmp(&b.0));

        Self {
            chunks,
            postings,
            term_map,
            avg_doc_len,
            doc_count,
        }
    }

    /// Validate the index's internal cross-references: every `term_map`
    /// posting index points at a real `Posting`, and every posting's
    /// `(chunk_idx, _)` points at a real `Chunk`.
    ///
    /// serde validates the SHAPE of a deserialised `bm25.bin`, NOT its internal
    /// consistency — a corrupt-but-shape-valid cache could carry an index past
    /// `postings.len()` / `chunks.len()` that would OOB-panic in `search`,
    /// violating the crate's fail-open contract (retrieval must never panic into
    /// the engine). The cache loader runs this check and DISCARDS + rebuilds an
    /// inconsistent cache instead of querying it. An empty index is consistent.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        let n_postings = self.postings.len();
        let n_chunks = self.chunks.len();
        if self
            .term_map
            .iter()
            .any(|(_, idx)| *idx as usize >= n_postings)
        {
            return false;
        }
        self.postings
            .iter()
            .all(|p| p.docs.iter().all(|(c, _)| (*c as usize) < n_chunks))
    }

    /// HashMap view of `term_map` for query-time lookup. Cheap to build.
    fn term_index(&self) -> HashMap<&str, u32> {
        self.term_map
            .iter()
            .map(|(t, i)| (t.as_str(), *i))
            .collect()
    }

    /// Re-tokenise `query` and drop the tokens that carry the least
    /// discriminative signal, returning the surviving tokens. This is a
    /// purely lexical, zero-dependency query-cleaning pass that addresses
    /// BM25's main lexical weakness from the *query* side: filler / very
    /// common terms add almost no ranking signal but still dilute the
    /// accumulated score (and, for CJK bigram queries, flood it with weak
    /// near-matches). Stripping them BEFORE search lets the rare, on-topic
    /// terms dominate the ranking.
    ///
    /// A token is masked when ANY of these hold:
    /// - it is a hard-coded function word (a tiny English + CJK stop list of
    ///   terms that are common in *every* corpus, so their corpus IDF is
    ///   unreliable on a small index), or
    /// - its corpus IDF is below `idf_floor` AND it is also below the query's
    ///   own median token IDF — i.e. it is both globally common and the least
    ///   useful token in THIS query. The relative test means a query made
    ///   entirely of common words is judged against itself, so it never gets
    ///   wiped to nothing on that branch.
    ///
    /// A token absent from the corpus (IDF undefined) is KEPT — it may be an
    /// exact identifier the corpus simply doesn't contain yet, and dropping it
    /// would lose a potential exact match.
    ///
    /// Fail-open: if masking would remove EVERY token (e.g. an all-stopword
    /// query), the original token list is returned unchanged so the caller's
    /// search is never starved. Pure function over the index stats — no I/O.
    #[must_use]
    pub fn mask_low_idf_terms(&self, query: &str, idf_floor: f64) -> Vec<String> {
        let tokens = tokenize(query);
        if tokens.len() <= 1 {
            return tokens; // nothing to gain from masking a single term
        }
        let term_idx = self.term_index();
        let n = self.doc_count.max(1) as f64;
        // IDF for a token: None when it isn't in the corpus (keep it — could be
        // an exact identifier). Uses the same BM25 +1-smoothed IDF as `search`.
        let idf_of = |tok: &str| -> Option<f64> {
            let pidx = *term_idx.get(tok)?;
            // Checked indexing: a corrupt (but shape-valid) cache could carry an
            // out-of-range posting index. Bounds-check rather than panic — the
            // crate is fail-open by contract (an absent posting just yields None,
            // and the token is then kept like any out-of-corpus token).
            let df = self.postings.get(pidx as usize)?.docs.len() as f64;
            Some(((n - df + 0.5) / (df + 0.5) + 1.0).ln())
        };
        // Median IDF over the tokens that ARE in the corpus — the per-query
        // relative bar. When no token is in the corpus, the relative test is a
        // no-op (every token is kept by the absent-token rule anyway).
        let mut known_idfs: Vec<f64> = tokens.iter().filter_map(|t| idf_of(t)).collect();
        known_idfs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if known_idfs.is_empty() {
            0.0
        } else {
            known_idfs[known_idfs.len() / 2]
        };
        let kept: Vec<String> = tokens
            .iter()
            .filter(|tok| {
                if is_stop_token(tok) {
                    return false;
                }
                match idf_of(tok) {
                    // In corpus: drop only when BOTH globally common (below the
                    // absolute floor) AND the least useful in this query (below
                    // its median). Either test failing keeps the token.
                    Some(idf) => !(idf < idf_floor && idf < median),
                    // Not in corpus: keep (possible exact identifier).
                    None => true,
                }
            })
            .cloned()
            .collect();
        // Fail-open: never starve the search. If masking emptied the query,
        // return the original tokens so BM25 still has something to match.
        if kept.is_empty() {
            tokens
        } else {
            kept
        }
    }

    /// Run a BM25 query. Returns `(chunk_idx, score)` pairs, highest score
    /// first. Scores are unbounded positive floats (BM25 has no fixed scale).
    #[must_use]
    pub fn search(&self, query: &str, top_k: usize) -> Vec<(usize, f64)> {
        self.search_terms(&tokenize(query), top_k)
    }

    /// BM25 over a PRE-TOKENISED query. Same scoring as [`Self::search`] but
    /// skips the tokeniser, so a caller that has already cleaned the query
    /// (e.g. via [`Self::mask_low_idf_terms`]) can search the surviving terms
    /// directly without re-stringifying + re-tokenising them.
    #[must_use]
    pub fn search_terms(&self, query_terms: &[String], top_k: usize) -> Vec<(usize, f64)> {
        if self.chunks.is_empty() || top_k == 0 {
            return Vec::new();
        }
        let term_idx = self.term_index();
        if query_terms.is_empty() {
            return Vec::new();
        }

        // Accumulate per-chunk score.
        let mut scores: Vec<f64> = vec![0.0; self.chunks.len()];
        let n = self.doc_count.max(1) as f64;

        for term in query_terms {
            let Some(&pidx) = term_idx.get(term.as_str()) else {
                continue; // term not in corpus
            };
            // Checked indexing (fail-open): a corrupt-but-shape-valid cache could
            // carry a `term_map` index past `postings` (or a posting whose
            // `chunk_idx` is past `chunks`). serde validates shape, not internal
            // consistency, so a bad index would otherwise panic here — violating
            // the crate's never-panic-into-the-engine contract. Skip instead.
            let Some(posting) = self.postings.get(pidx as usize) else {
                continue;
            };
            let df = posting.docs.len() as f64;
            // IDF with BM25's +1 smoothing (never negative).
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &(chunk_idx, tf) in &posting.docs {
                let ci = chunk_idx as usize;
                // BM25 length normalisation uses the BIGRAM document length
                // (`bm25_len`), excluding the appended CJK-trigram tokens — so a
                // chunk rich in trigrams is not falsely treated as "long" and
                // down-weighted in the bigram channel.
                let Some(chunk) = self.chunks.get(ci) else {
                    continue; // stale/corrupt chunk_idx — skip, never panic
                };
                let dl = chunk.bm25_len() as f64;
                // Standard BM25: tf*(k1+1) / (tf + k1*(1 - b + b*dl/avgdl)).
                let denom = f64::from(tf) + K1 * (1.0 - B + B * (dl / self.avg_doc_len.max(1.0)));
                let tf_component = (f64::from(tf) * (K1 + 1.0)) / denom;
                scores[ci] += idf * tf_component;
            }
        }

        // Collect (idx, score) for chunks with score > 0, sort desc, take top_k.
        let mut ranked: Vec<(usize, f64)> = scores
            .iter()
            .enumerate()
            .filter(|(_, s)| **s > 0.0)
            .map(|(i, s)| (i, *s))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(top_k);
        ranked
    }
}

/// A deliberately TINY function-word list, used only by
/// [`Bm25Index::mask_low_idf_terms`]. These words are common in essentially
/// every corpus, so on a small index their measured IDF is noisy/unreliable —
/// hard-listing them is safer than trusting a per-corpus IDF that may be
/// inflated just because the curated knowledge base happens to be small.
/// Kept short on purpose: only words that are PURE structure (never a topical
/// keyword) are listed, so the mask never removes a term a user might mean.
/// The CJK entries are single function characters whose own bigram tokens are
/// already topical and survive separately.
const STOP_TOKENS: &[&str] = &[
    // English articles / conjunctions / prepositions / auxiliaries — never topical.
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "you",
    "are",
    "was",
    "were",
    "from",
    "have",
    "has",
    "had",
    "but",
    "not",
    "all",
    "any",
    "can",
    "will",
    "would",
    "should",
    "could",
    "into",
    "out",
    "use",
    "using",
    "via",
    "per",
    "其中",
    "我们",
    "可以",
    "需要",
    "一个",
    "一种",
    "进行",
    "通过",
    "以及",
    "或者",
    "这个",
    "那个",
    "做一个",
    "做个",
];

/// Whether `tok` is a hard-coded function word that should never carry
/// retrieval signal. See [`STOP_TOKENS`].
fn is_stop_token(tok: &str) -> bool {
    STOP_TOKENS.contains(&tok)
}

/// Convenience: build an index then search in one call (used by tests).
#[must_use]
pub fn build_index(chunks: Vec<Chunk>) -> Bm25Index {
    Bm25Index::from_chunks(chunks)
}

/// Search a pre-built index, returning chunk references. Thin wrapper over
/// [`Bm25Index::search`] returning `&Chunk` for ergonomic prompt formatting.
#[must_use]
pub fn bm25_search<'a>(index: &'a Bm25Index, query: &str, top_k: usize) -> Vec<(&'a Chunk, f64)> {
    index
        .search(query, top_k)
        .into_iter()
        .map(|(i, s)| (&index.chunks[i], s))
        .collect()
}

/// On-disk index path: `<project_root>/.umadev/kb-index/bm25.bin`.
fn index_path(project_root: &Path) -> PathBuf {
    project_root.join(super::KB_INDEX_DIR).join("bm25.bin")
}

/// Force the next [`load_or_build_index_multi`] to REBUILD from source instead
/// of loading the cache, by removing the on-disk signature file.
///
/// The cache is keyed on a content-hash signature of the corpus. That makes a
/// rebuild happen whenever a source `.md` changes — but a caller that has just
/// written NEW lesson files and wants them retrievable *within the same run*
/// can't wait for an organic content change to be noticed: it must invalidate
/// the cache explicitly so the very next retrieval re-scans the (now larger)
/// corpus. This is the write-side half of closing the sediment→index→retrieve
/// loop inside one run (see `lessons::sediment_lessons`). Fail-open: a missing
/// or unremovable signature file is ignored (a stale `.sig` only costs one
/// extra organic rebuild later, never correctness).
pub fn invalidate_cache(project_root: &Path) {
    let sig_path = index_path(project_root).with_extension("sig");
    let _ = std::fs::remove_file(&sig_path);
}

/// Build (or rebuild) the index from all `.md` files under `knowledge_dir`,
/// serialise it to disk, and return it. Always overwrites the on-disk copy.
///
/// Walks the directory tree to depth 6 (matching the legacy `walk_md`
/// guard), chunks each `.md`, and indexes the resulting chunks.
pub fn load_or_build_index(project_root: &Path, knowledge_dir: &Path) -> Bm25Index {
    load_or_build_index_multi(project_root, &[knowledge_dir.to_path_buf()])
}

/// Build/load the BM25 index over MULTIPLE source directories (e.g. the
/// static `knowledge/`, project `.umadev/learned/`, and global
/// `~/.umadev/learned/`). All `.md` files across all dirs are merged into
/// one index with a combined mtime signature for cache invalidation.
#[must_use]
pub fn load_or_build_index_multi(project_root: &Path, knowledge_dirs: &[PathBuf]) -> Bm25Index {
    // Collect all .md files from every dir.
    let mut paths: Vec<PathBuf> = Vec::new();
    for dir in knowledge_dirs {
        if dir.is_dir() {
            walk_md(dir, &mut paths, 0);
        }
    }
    // Sort the collected paths so chunk POSITIONS are stable run-to-run: `walk_md`
    // collects in `read_dir` order, which is unspecified and can vary between
    // machines / after a file is added or removed. A stable position is what the
    // vector store's positional `chunk_idx` keys on, so an unstable order would
    // desync BM25 from the cached vector store and attribute a vector hit to the
    // WRONG chunk (MED #4).
    paths.sort();
    let signature = corpus_signature(&paths, knowledge_dirs);

    // Cache check: if the signature matches the stored one, load directly.
    let sig_path = index_path(project_root).with_extension("sig");
    if let Ok(stored_sig) = std::fs::read_to_string(&sig_path) {
        if stored_sig == signature {
            let idx_path = index_path(project_root);
            if let Ok(bytes) = std::fs::read(&idx_path) {
                if let Ok(index) = serde_json::from_slice::<Bm25Index>(&bytes) {
                    // Only trust a cache whose cross-references are in range — a
                    // corrupt-but-shape-valid blob would otherwise OOB-panic in
                    // `search`. An inconsistent cache falls through to a rebuild.
                    if index.is_consistent() {
                        return index;
                    }
                }
            }
        }
    }

    // Cache miss (or corrupt) — rebuild. Chunk each file using the dir it
    // came from as the strip-prefix root (so ChunkMeta.path is relative).
    let mut chunks: Vec<Chunk> = Vec::new();
    for abs in &paths {
        let root = knowledge_dirs
            .iter()
            .find(|d| abs.starts_with(d))
            .cloned()
            .unwrap_or_else(|| knowledge_dirs[0].clone());
        // A file from any corpus dir PAST the curated knowledge dir (index 0) is learned
        // SEDIMENT (.umadev/learned + ~/.umadev/learned from corpus_dirs). Stamp its chunks
        // so the phase/seat filter always lets them through - including a promoted GLOBAL
        // lesson whose slug filename lacks the `lesson-` marker (knowledge #1).
        let is_learned = knowledge_dirs.first().is_none_or(|k| &root != k);
        let mut file_chunks = crate::chunker::chunk_file(&root, abs);
        if is_learned {
            for ch in &mut file_chunks {
                ch.meta.is_learned = true;
            }
        }
        chunks.extend(file_chunks);
    }
    let index = Bm25Index::from_chunks(chunks);

    // Persist index + signature (best-effort).
    let path = index_path(project_root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_vec(&index) {
        // Write the signature ONLY when the index write actually SUCCEEDED. Writing
        // the sig unconditionally after a FAILED index write (disk full, or a Windows
        // lock / transient EACCES while the previous valid index file survives) leaves
        // an inconsistent old-index + new-sig pair on disk — every later load then sees
        // `stored_sig == signature` and serves the STALE pre-change index until the
        // corpus changes again. Gating the sig on the index write keeps the pair
        // consistent: a failed index write leaves the old sig, so the next load's
        // signature mismatch rebuilds instead of trusting a stale cache.
        if std::fs::write(&path, text).is_ok() {
            let _ = std::fs::write(&sig_path, &signature);
        }
    }

    index
}

/// Build a machine-INDEPENDENT corpus signature: one sorted line per file,
/// `<relative_path>\t<truncated_sha256_of_content>`, prefixed with the schema
/// version. Keying on CONTENT (not mtime / absolute path) keeps the signature —
/// and thus the on-disk `.umadev/kb-index/` cache — identical across
/// clones/machines, so a copied cache still hits. The output is byte-identical
/// for an unchanged corpus and differs the moment any file's content changes, a
/// file is added, or a file is removed.
///
/// PERF (the bug this docs the fix for): `retrieve()` runs ~10-30× per run and
/// every call recomputes this signature BEFORE the cache-hit check. A naive
/// implementation `std::fs::read` + SHA-256'd the WHOLE corpus on each of those
/// calls, so retrieval latency scaled O(total corpus bytes) PER QUERY even on a
/// cache hit. The content hash is now memoized per file in an in-process cache
/// keyed on the cheap `(mtime, size)` `stat` ([`file_content_hash`]): on a warm
/// memo with unchanged metadata the stored hash is reused WITHOUT reading the
/// file, so a repeat call over an unchanged corpus does only one cheap `stat`
/// per file (O(file count), zero byte reads) and returns the byte-identical
/// signature. A file is re-read + re-hashed only when its `(mtime, size)`
/// differs from the last-seen state (a real edit), so the content-based output —
/// and its cross-machine portability — is preserved exactly. Invalidation
/// guarantee: a real edit (mtime OR size change), a new file, or a removed file
/// still changes the signature and forces a rebuild.
///
/// Correctness tradeoff: the memo trusts `(mtime, size)` within a single process
/// run. A content edit that leaves BOTH mtime and size unchanged would reuse the
/// stale hash for the rest of that run — an astronomically rare case that also
/// self-heals on the next run (the memo starts cold, so the first retrieval
/// re-reads real content). Everything is fail-open: a `stat`/read error is
/// treated as "changed" → re-read (never a panic), and a rebuilt index is itself
/// fail-open, so at worst a false miss costs one extra rebuild.
///
/// `knowledge_dirs` is the list of roots to strip (so `ChunkMeta.path`-style
/// relative keys land in the signature). Falls back to the file name when no
/// root matches.
fn corpus_signature(paths: &[PathBuf], knowledge_dirs: &[PathBuf]) -> String {
    // Take the process-wide per-file hash memo. Fail-open on a poisoned lock:
    // fall back to a fresh empty memo (correct, just not accelerated).
    match signature_memo().lock() {
        Ok(mut memo) => compute_signature(paths, knowledge_dirs, &mut memo).0,
        Err(_) => compute_signature(paths, knowledge_dirs, &mut HashMap::new()).0,
    }
}

/// One in-process per-file content-hash memo entry: the cheap freshness keys
/// (`mtime`, `size`) captured at hash time, plus the truncated-SHA-256 hex the
/// signature keys on. Reused verbatim while `(mtime, size)` is unchanged.
struct CachedFileHash {
    /// Last-seen modification time (a cheap `stat`, no read).
    mtime: std::time::SystemTime,
    /// Last-seen file size in bytes (a cheap `stat`, no read).
    size: u64,
    /// The content hash computed when this entry was last (re)read.
    hash: String,
}

/// The process-wide per-file content-hash memo, keyed by absolute path. Purely
/// an in-process accelerator for [`corpus_signature`]: it never touches disk, so
/// it changes nothing about the portable, content-based on-disk `.sig`. Fail-open
/// — a poisoned lock just takes the un-memoized (still correct) path.
fn signature_memo() -> &'static Mutex<HashMap<PathBuf, CachedFileHash>> {
    static MEMO: OnceLock<Mutex<HashMap<PathBuf, CachedFileHash>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One cheap `stat` of a file's `(mtime, size)` — no read, no hash. Fail-open to
/// `None` on any error (a missing file, or a platform without `modified()`),
/// which callers treat as "changed" and re-read.
fn file_stat(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

/// Truncated (first 8 bytes → 16 hex chars) SHA-256 of a file's CONTENT, with a
/// per-file `(mtime, size)` in-process memo. Returns the cached hash WITHOUT
/// reading the file when the cheap `stat` matches the last-seen state; otherwise
/// reads the file once, hashes it, and records the result. Increments
/// `content_reads` exactly when it performs a real byte read — the testable seam
/// that proves an unchanged corpus is signed without re-reading every file.
/// Fail-open: a read failure returns `None` (the file is skipped from the
/// signature, matching the prior behaviour).
fn file_content_hash(
    path: &Path,
    memo: &mut HashMap<PathBuf, CachedFileHash>,
    content_reads: &mut usize,
) -> Option<String> {
    use sha2::{Digest, Sha256};
    let stat = file_stat(path);
    // Fast path: cheap `stat` matches the memoized `(mtime, size)` → reuse the
    // stored content hash, no byte read.
    if let Some((mtime, size)) = stat {
        if let Some(cached) = memo.get(path) {
            if cached.mtime == mtime && cached.size == size {
                return Some(cached.hash.clone());
            }
        }
    }
    // Slow path: memo miss / changed metadata / unstat-able → read + hash once.
    // A read failure short-circuits BEFORE the counter bumps, so `content_reads`
    // counts only files actually read from disk (fail-open: the file is skipped).
    let bytes = std::fs::read(path).ok()?;
    *content_reads += 1;
    let digest = Sha256::digest(&bytes);
    let hash_hex: String = digest.iter().take(8).fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    // Only cacheable when we have a `(mtime, size)` to key on; otherwise the next
    // call re-reads (fail-open, still correct).
    if let Some((mtime, size)) = stat {
        memo.insert(
            path.to_path_buf(),
            CachedFileHash {
                mtime,
                size,
                hash: hash_hex.clone(),
            },
        );
    }
    Some(hash_hex)
}

/// The pure, testable core of [`corpus_signature`]: builds the content-based
/// signature over `paths` using (and updating) `memo`, and returns it alongside
/// the number of files that needed a real byte read (`content_reads`) — `0` when
/// every file was served from the warm memo. Taking the memo as a parameter lets
/// a test drive the fast/slow paths deterministically without the process-global
/// memo.
fn compute_signature(
    paths: &[PathBuf],
    knowledge_dirs: &[PathBuf],
    memo: &mut HashMap<PathBuf, CachedFileHash>,
) -> (String, usize) {
    let mut content_reads = 0usize;
    let mut entries: Vec<(String, String)> = Vec::with_capacity(paths.len());
    for p in paths {
        // Content hash first: a read failure skips the file (fail-open).
        let Some(hash) = file_content_hash(p, memo, &mut content_reads) else {
            continue;
        };
        // Relative path: strip the matching knowledge dir prefix.
        let rel = knowledge_dirs
            .iter()
            .find_map(|d| p.strip_prefix(d).ok())
            .or_else(|| p.file_name().map(std::path::Path::new))
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| p.to_string_lossy().replace('\\', "/"));
        entries.push((rel, hash));
    }
    entries.sort();
    // Fold in the schema version so a tokenizer/chunker/layout upgrade
    // invalidates every older cache even when no source file changed.
    let body = entries
        .iter()
        .map(|(p, h)| format!("{p}\t{h}"))
        .collect::<Vec<_>>()
        .join("\n");
    (
        format!("schema=v{INDEX_SCHEMA_VERSION}\n{body}"),
        content_reads,
    )
}

/// Maximum number of `.md` files the index will scan. A guard against a
/// pathological corpus (e.g. a vendored docs dump) ballooning index build
/// time + memory. When hit, the extra files are silently skipped — but a
/// warning is emitted (see [`walk_md`]) so the user knows coverage is
/// partial. Override via `UMADEV_KNOWLEDGE_MAX_FILES` (0 = unlimited).
const DEFAULT_MAX_MD_FILES: usize = 2000;

/// Effective file cap, honouring the `UMADEV_KNOWLEDGE_MAX_FILES` env
/// override (`0` = unlimited). Read once and cached for the process.
fn max_md_files() -> usize {
    std::env::var("UMADEV_KNOWLEDGE_MAX_FILES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_MD_FILES)
}

/// Recursively collect `.md` file paths under `dir`, up to `depth` 6.
/// Matches the legacy `walk_md` behaviour (phases.rs:1851) so the corpus
/// coverage is identical.
///
/// No-follow: entries are classified with `symlink_metadata` (lstat), so a
/// symlinked directory INSIDE the knowledge tree is never descended and a
/// symlinked `.md` is never collected — a link can't pull markdown from OUTSIDE
/// the corpus into the RAG index, and a symlink cycle can't recurse. Fail-open:
/// an entry whose metadata can't be read is skipped, never aborting the walk.
/// (umadev-knowledge deliberately does not depend on umadev-agent, so this
/// mirrors that crate's `fswalk` policy inline rather than sharing the helper.)
fn walk_md(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let cap = max_md_files();
    if depth > 6 {
        return;
    }
    // 0 = unlimited; otherwise this is a hard ceiling enforced PER-PUSH below
    // (so a single large directory can't overshoot it).
    if cap != 0 && out.len() >= cap {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk_md(&p, out, depth + 1);
        } else if ft.is_file() && p.extension().and_then(|s| s.to_str()) == Some("md") {
            if cap != 0 && out.len() >= cap {
                warn_md_cap_once(cap);
                return;
            }
            out.push(p);
        }
    }
}

/// Emit the "knowledge index hit its file cap" warning at most once per process
/// (it's reached from every directory that overflows, so a naive `eprintln!`
/// would spam). Coverage being partial is worth surfacing exactly once.
fn warn_md_cap_once(cap: usize) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            cap,
            "knowledge index hit the file cap; files beyond the cap are not indexed"
        );
    }
}

// ---------------------------------------------------------------------------
// Vector store build — batch-embeds all chunks and caches them with
// per-chunk content-hash invalidation. Only does real work when the `vector`
// feature is on AND an API key is set; otherwise it's a no-op that leaves
// the store empty (BM25 dominates).
//
// This is the piece that was an honest stub before: the index was built but
// never embedded, so VectorStore was always empty and hybrid retrieval
// silently degraded to BM25 even when the user set OPENAI_API_KEY.
// ---------------------------------------------------------------------------

/// Compute a stable content hash of a chunk's body for cache invalidation.
///
/// Uses **truncated SHA-256** (first 8 bytes → u64), NOT the stdlib
/// `DefaultHasher`. SHA-256 is fixed by spec, so the cache survives Rust
/// toolchain bumps that change the stdlib's default SipHash
/// algorithm/seeds — which would otherwise silently invalidate the ENTIRE
/// `.umadev/kb-index/` cache and force a full re-embed. The value only
/// needs determinism + collision-resistance for a chunk body, both of which
/// SHA-256 provides; truncation to 64 bits is plenty for a corpus of
/// thousands of chunks (birthday bound ≈ 4 billion).
#[must_use]
fn body_hash(body: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(body.as_bytes());
    // First 8 bytes, big-endian → u64.
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 is 32 bytes"))
}

/// A deterministic fingerprint of an index's CHUNK-POSITION MAPPING: the ordered
/// `(path, section)` identity of every chunk plus the chunk count, hashed
/// (truncated SHA-256). The vector store is stamped with this at build time
/// ([`build_vector_store_if_enabled`]); the retriever compares it against the
/// live index before keying vector hits on positional `chunk_idx`.
///
/// MED #4: BM25 rebuilds lazily at query time while the vector store rebuilds
/// separately (async), so after a knowledge file is added/removed a
/// stale-yet-in-range `chunk_idx` would map a vector hit onto a DIFFERENT chunk.
/// A fingerprint mismatch detects exactly that (a file add/remove changes the
/// ordered identity sequence) so the retriever can skip vector fusion (or wait
/// for the rebuild) rather than attribute a hit to the wrong chunk. Keyed on
/// `(path, section)` + count — the positional mapping — NOT body content (which
/// the store's per-chunk `body_hash` already invalidates).
#[must_use]
pub fn corpus_fingerprint(index: &Bm25Index) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update((index.chunks.len() as u64).to_be_bytes());
    for c in &index.chunks {
        hasher.update(c.meta.path.as_bytes());
        hasher.update([0u8]);
        hasher.update(c.meta.section.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    digest.iter().take(16).fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// The text that gets embedded for a chunk: title + section + body. This is
/// the exact string the embedding model sees, so it must be deterministic
/// across rebuilds for the cache to hit.
#[must_use]
fn embed_text(chunk: &Chunk) -> String {
    format!(
        "{}\n{}\n{}",
        chunk.meta.title, chunk.meta.section, chunk.body
    )
}

/// Build (or incrementally update) the cached vector store for an index.
///
/// This is the async companion to [`load_or_build_index_multi`]: it runs
/// AFTER the BM25 index is ready, embeds every chunk, and persists the store
/// to `.umadev/kb-index/vectors.bin`. Returns `None` (and leaves the
/// store empty) when the vector layer is off or embedding fails — the
/// retriever then transparently uses BM25 only.
///
/// **Incremental**: the cached store is loaded first; chunks whose
/// `body_hash` matches the cache are reused, and only new/changed chunks
/// are re-embedded. This keeps re-indexing after a small doc edit cheap.
pub async fn build_vector_store_if_enabled(
    project_root: &Path,
    index: &Bm25Index,
) -> Option<vector::VectorStore> {
    if !vector::is_enabled() || index.chunks.is_empty() {
        return None;
    }

    let model = vector::active_model();
    let mut store = vector::VectorStore::load(project_root);

    // Discard the cached store if it was built with a different model OR a
    // different embedding dimension — a dim change otherwise leaves cached
    // wrong-length vectors that make `search` silently return empty forever.
    if !store.is_empty() && (store.model() != model || store.dim() != vector::active_dim()) {
        store = vector::VectorStore::disabled();
    }

    // MED #4: stamp the store with the live index's chunk-position fingerprint so
    // the retriever can detect a corpus that shifted since the store was built
    // (a file added/removed → every later chunk_idx shifts) and skip keying
    // vector hits on a now-misaligned positional chunk_idx. Set ONCE here, before
    // the branches below: `replace` preserves it and every save path persists it.
    store.set_corpus_sig(corpus_fingerprint(index));

    // Content-addressed reuse: key on (path, section, body_hash), NOT the
    // volatile positional chunk index. Keying on position re-embedded identical
    // content whenever an earlier chunk was inserted/removed (every later index
    // shifts); keying on content identity reuses it regardless of position.
    let cached: HashMap<(String, String, u64), Vec<f32>> = store
        .cached_for_reuse()
        .into_iter()
        .map(|(_idx, path, section, hash, vec)| ((path, section, hash), vec))
        .collect();

    let mut to_embed: Vec<(usize, String)> = Vec::new(); // (chunk_idx, text)
    let mut kept: Vec<(u32, String, String, u64, Vec<f32>)> = Vec::new();

    for (i, chunk) in index.chunks.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        let hash = body_hash(&chunk.body);
        let key = (chunk.meta.path.clone(), chunk.meta.section.clone(), hash);
        if let Some(cached_vec) = cached.get(&key) {
            // Cache hit — identical content, reuse the existing vector.
            kept.push((
                idx,
                chunk.meta.path.clone(),
                chunk.meta.section.clone(),
                hash,
                cached_vec.clone(),
            ));
            continue;
        }
        to_embed.push((i, embed_text(chunk)));
    }

    if to_embed.is_empty() {
        // Everything cached — just refresh the store from `kept`.
        store.replace(model, kept);
        store.save(project_root);
        return Some(store);
    }

    // Embed the stale/new chunks in batches.
    let texts: Vec<String> = to_embed.iter().map(|(_, t)| t.clone()).collect();
    tracing::info!(
        "embedding {} chunk(s) ({} cached, model {model})",
        texts.len(),
        kept.len()
    );
    let vecs = if let Some(v) = vector::embed_batch(&texts).await {
        v
    } else {
        tracing::warn!(
            "batch embedding failed — vector store left {} cached entries, BM25 will dominate",
            kept.len()
        );
        if kept.is_empty() {
            return None;
        }
        store.replace(model, kept);
        store.save(project_root);
        return Some(store);
    };

    if vecs.len() != to_embed.len() {
        tracing::warn!(
            "embedding count mismatch ({} != {}) — discarding new vectors, keeping cache",
            vecs.len(),
            to_embed.len()
        );
        if kept.is_empty() {
            return None;
        }
        store.replace(model, kept);
        store.save(project_root);
        return Some(store);
    }

    // Merge freshly-embedded chunks with the cached ones.
    for ((chunk_i, _), vec) in to_embed.into_iter().zip(vecs) {
        let chunk = &index.chunks[chunk_i];
        let idx = u32::try_from(chunk_i).unwrap_or(u32::MAX);
        kept.push((
            idx,
            chunk.meta.path.clone(),
            chunk.meta.section.clone(),
            body_hash(&chunk.body),
            vec,
        ));
    }

    store.replace(model, kept);
    store.save(project_root);
    tracing::info!("vector store built: {} vectors", store.len());
    Some(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::chunk_text;

    fn idx_from(texts: &[(&str, &str)]) -> Bm25Index {
        let chunks: Vec<Chunk> = texts
            .iter()
            .flat_map(|(path, body)| chunk_text(path, body))
            .collect();
        Bm25Index::from_chunks(chunks)
    }

    #[test]
    fn empty_index_returns_nothing() {
        let idx = Bm25Index::from_chunks(Vec::new());
        assert!(idx.search("anything", 5).is_empty());
    }

    #[test]
    fn empty_query_returns_nothing() {
        let idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin auth")]);
        assert!(idx.search("   ", 5).is_empty());
    }

    #[test]
    fn term_not_in_corpus_returns_nothing() {
        let idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin")]);
        assert!(idx.search("nonexistentterm", 5).is_empty());
    }

    #[test]
    fn exact_term_match_ranks_relevant_doc() {
        let idx = idx_from(&[
            (
                "login.md",
                "# Login\n\n## Flow\n\nUse OAuth2 PKCE for login authentication.",
            ),
            (
                "postgres.md",
                "# Postgres\n\n## Tuning\n\nshared_buffers tuning for database.",
            ),
        ]);
        let results = idx.search("login", 5);
        assert!(!results.is_empty());
        // The login doc (idx 0's chunk) should rank first.
        let top_path = &idx.chunks[results[0].0].meta.path;
        assert!(
            top_path.contains("login"),
            "expected login doc, got {top_path}"
        );
    }

    #[test]
    fn rarer_term_scores_higher_than_common_term() {
        // Two docs both mention "auth"; only one mentions "pkce".
        let idx = idx_from(&[
            ("a.md", "# A\n\n## S\n\nauth auth auth auth pkce"),
            ("b.md", "# B\n\n## S\n\nauth auth auth auth"),
        ]);
        let auth_results = idx.search("auth", 2);
        let pkce_results = idx.search("pkce", 2);
        // "pkce" should only match doc a.
        assert_eq!(pkce_results.len(), 1);
        // "auth" matches both (a has higher TF so ranks first, but both score).
        assert_eq!(auth_results.len(), 2);
    }

    #[test]
    fn multi_term_query_accumulates_score() {
        let idx = idx_from(&[
            (
                "login.md",
                "# Login\n\n## OAuth\n\noauth2 pkce login security",
            ),
            (
                "other.md",
                "# Other\n\n## S\n\nunrelated content about cooking",
            ),
        ]);
        let results = idx.search("login oauth2", 5);
        // The login doc (whichever chunk idx) must rank first.
        let top_path = &idx.chunks[results[0].0].meta.path;
        assert!(
            top_path.contains("login"),
            "expected login doc to win, got {top_path}"
        );
    }

    #[test]
    fn cjk_query_matches_cjk_content() {
        // The headline fix: CJK requirements now retrieve CJK content.
        let idx = idx_from(&[("login.md", "# 登录\n\n## 流程\n\n使用 OAuth2 做登录认证")]);
        let results = idx.search("登录系统", 5);
        assert!(!results.is_empty(), "CJK query must hit CJK content");
    }

    #[test]
    fn top_k_limits_results() {
        let idx = idx_from(&[
            ("a.md", "# A\n\n## S\n\nauth auth"),
            ("b.md", "# B\n\n## S\n\nauth auth"),
            ("c.md", "# C\n\n## S\n\nauth auth"),
        ]);
        assert_eq!(idx.search("auth", 2).len(), 2);
        assert_eq!(idx.search("auth", 10).len(), 3);
    }

    #[test]
    fn index_serialises_round_trip() {
        let idx = idx_from(&[("login.md", "# Login\n\n## OAuth\n\noauth2 pkce login")]);
        let bytes = serde_json::to_vec(&idx).unwrap();
        let back: Bm25Index = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.chunks.len(), idx.chunks.len());
        // Functional equivalence: same query yields same top doc.
        let orig = idx.search("login", 1);
        let reload = back.search("login", 1);
        assert_eq!(orig[0].0, reload[0].0);
    }

    #[test]
    fn bm25_search_returns_chunk_refs() {
        let idx = idx_from(&[("login.md", "# Login\n\n## S\n\nauth login")]);
        let hits = bm25_search(&idx, "login", 5);
        assert!(!hits.is_empty());
        // The matched chunk must mention the query terms.
        assert!(
            hits[0].0.body.contains("login") || hits[0].0.meta.title.contains("Login"),
            "hit body should reference the query"
        );
        assert!(hits[0].1 > 0.0);
    }

    #[test]
    fn load_or_build_writes_and_returns() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let kd = root.join("knowledge");
        fs::create_dir_all(kd.join("security")).unwrap();
        fs::write(
            kd.join("security/login.md"),
            "# Login\n\n## OAuth\n\nUse OAuth2 PKCE for login.",
        )
        .unwrap();

        let idx = load_or_build_index(root, &kd);
        assert!(!idx.chunks.is_empty());
        // Index file was written.
        assert!(root.join(".umadev/kb-index/bm25.bin").is_file());
        // Query works.
        let results = idx.search("login", 5);
        assert!(!results.is_empty());
    }

    #[test]
    fn load_or_build_reuses_cache_when_unchanged() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let kd = root.join("knowledge");
        fs::create_dir_all(kd.join("security")).unwrap();
        fs::write(
            kd.join("security/login.md"),
            "# Login\n\n## OAuth\n\nUse OAuth2 PKCE for login.",
        )
        .unwrap();

        // First call builds + writes index + signature.
        let idx1 = load_or_build_index(root, &kd);
        let sig_path = root.join(".umadev/kb-index/bm25.sig");
        assert!(sig_path.is_file(), "signature file must be written");

        // Second call with unchanged corpus loads from cache (functional equiv).
        let idx2 = load_or_build_index(root, &kd);
        assert_eq!(idx1.chunks.len(), idx2.chunks.len());
        assert_eq!(idx1.search("login", 1)[0].0, idx2.search("login", 1)[0].0);

        // Touching a file invalidates the cache (mtime changes).
        fs::write(
            kd.join("security/login.md"),
            "# Login\n\n## OAuth\n\nUpdated content about login authentication.",
        )
        .unwrap();
        let idx3 = load_or_build_index(root, &kd);
        assert!(!idx3.chunks.is_empty());
    }

    #[test]
    fn invalidate_cache_forces_rebuild_picking_up_new_files() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let kd = root.join("knowledge");
        fs::create_dir_all(&kd).unwrap();
        fs::write(kd.join("a.md"), "# A\n\n## S\n\nalpha content").unwrap();

        // Build + cache the index over the initial corpus.
        let idx1 = load_or_build_index(root, &kd);
        let chunks1 = idx1.chunks.len();
        assert!(root.join(".umadev/kb-index/bm25.sig").is_file());

        // Add a NEW file. Without invalidation, simulate the in-run race: the
        // signature file still describes the old corpus. Invalidate it, then the
        // next load MUST rebuild and include the new file's content.
        fs::write(kd.join("b.md"), "# B\n\n## S\n\nbeta brandnewterm").unwrap();
        invalidate_cache(root);

        let idx2 = load_or_build_index(root, &kd);
        assert!(
            idx2.chunks.len() > chunks1,
            "rebuild must include the newly-written file"
        );
        assert!(
            !idx2.search("brandnewterm", 5).is_empty(),
            "the freshly-added content must be retrievable after invalidation"
        );
    }

    #[test]
    fn invalidate_cache_missing_sig_is_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No index built yet → no .sig file. Must not panic / error (fail-open).
        invalidate_cache(tmp.path());
    }

    #[test]
    fn corpus_signature_changes_with_content() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let f = dir.join("a.md");
        fs::write(&f, "content v1").unwrap();
        let s1 = corpus_signature(std::slice::from_ref(&f), std::slice::from_ref(&dir));
        fs::write(&f, "content v2 changed").unwrap();
        let s2 = corpus_signature(std::slice::from_ref(&f), std::slice::from_ref(&dir));
        assert_ne!(s1, s2, "signature must change when content/size changes");
    }

    #[test]
    fn corpus_signature_is_machine_independent() {
        // Regression: the signature used absolute paths + mtime, so two
        // clones of the same knowledge/ tree on different machines produced
        // DIFFERENT signatures and the kb-index cache couldn't be shared.
        // Now it's relative-path + content-hash: identical content → identical
        // signature regardless of where the tree lives.
        use std::fs;
        let dir_a = tempfile::TempDir::new().unwrap();
        let dir_b = tempfile::TempDir::new().unwrap();
        // Same relative layout + identical content in both.
        fs::create_dir_all(dir_a.path().join("security")).unwrap();
        fs::create_dir_all(dir_b.path().join("security")).unwrap();
        fs::write(dir_a.path().join("a.md"), "same content").unwrap();
        fs::write(dir_b.path().join("a.md"), "same content").unwrap();
        fs::write(
            dir_a.path().join("security/login.md"),
            "# Login

## OAuth
",
        )
        .unwrap();
        fs::write(
            dir_b.path().join("security/login.md"),
            "# Login

## OAuth
",
        )
        .unwrap();
        let paths_a = vec![
            dir_a.path().join("a.md"),
            dir_a.path().join("security/login.md"),
        ];
        let paths_b = vec![
            dir_b.path().join("a.md"),
            dir_b.path().join("security/login.md"),
        ];
        let sa = corpus_signature(&paths_a, &[dir_a.path().to_path_buf()]);
        let sb = corpus_signature(&paths_b, &[dir_b.path().to_path_buf()]);
        assert_eq!(
            sa, sb,
            "same content under different roots must produce the SAME signature              (machine-independent); got
A: {sa}
B: {sb}"
        );
        // And the signature must contain RELATIVE paths (no tempdir prefix).
        assert!(
            !sa.contains("tmp") && !sa.contains("/var/") && !sa.contains("/private/"),
            "signature must not leak absolute temp paths: {sa}"
        );
        assert!(
            sa.contains("a.md"),
            "signature must contain relative file name"
        );
        assert!(
            sa.contains("security/login.md"),
            "signature must contain nested relative path"
        );
    }

    #[test]
    fn walk_md_paths_are_sorted_for_stable_chunk_positions() {
        // MED #4 (b): chunk POSITIONS must be stable regardless of read_dir order.
        // Files are written in non-sorted order; the resulting chunk paths must
        // come out sorted ascending so the positional chunk_idx (the vector store
        // keys on it) is deterministic across machines / after a file add/remove.
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let kd = root.join("knowledge");
        fs::create_dir_all(&kd).unwrap();
        // Deliberately reverse creation order.
        fs::write(kd.join("c.md"), "# C\n\n## S\n\ngamma").unwrap();
        fs::write(kd.join("a.md"), "# A\n\n## S\n\nalpha").unwrap();
        fs::write(kd.join("b.md"), "# B\n\n## S\n\nbeta").unwrap();

        let idx = load_or_build_index(root, &kd);
        let paths: Vec<&str> = idx.chunks.iter().map(|c| c.meta.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(
            paths, sorted,
            "chunk paths must be in stable sorted order: {paths:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_md_no_follow_symlinks_out_and_cycle_terminates() {
        use std::os::unix::fs::symlink;
        // OUTSIDE the corpus: a markdown file that must never enter the index.
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("outside.md"), "# outside\n").unwrap();

        // The corpus dir: a real in-tree .md, a dir symlink escaping OUTSIDE,
        // and a self-cycle symlink.
        let corpus = tempfile::TempDir::new().unwrap();
        std::fs::write(corpus.path().join("inside.md"), "# inside\n").unwrap();
        symlink(outside.path(), corpus.path().join("escape")).unwrap();
        symlink(corpus.path(), corpus.path().join("loop")).unwrap();

        // Terminates: an escaping / cyclic dir symlink is never descended.
        let mut out = Vec::new();
        walk_md(corpus.path(), &mut out, 0);

        assert!(
            out.iter().any(|p| p.ends_with("inside.md")),
            "in-tree markdown must still be indexed: {out:?}"
        );
        assert!(
            !out.iter().any(|p| p.ends_with("outside.md")),
            "a symlink must not pull markdown from outside the corpus: {out:?}"
        );
        assert!(
            !out.iter().any(|p| p.to_string_lossy().contains("escape")),
            "walk must not traverse an escaping symlink: {out:?}"
        );
    }

    #[test]
    fn corpus_fingerprint_changes_when_a_chunk_is_added_or_removed() {
        // MED #4: the fingerprint keys on the ordered (path, section) identity +
        // count, so adding/removing a chunk (which shifts every later chunk_idx)
        // changes it — exactly the desync the retriever's alignment gate detects.
        let one = idx_from(&[("a.md", "# A\n\n## S\n\nalpha")]);
        let two = idx_from(&[
            ("a.md", "# A\n\n## S\n\nalpha"),
            ("b.md", "# B\n\n## S\n\nbeta"),
        ]);
        assert_ne!(
            corpus_fingerprint(&one),
            corpus_fingerprint(&two),
            "adding a chunk must change the corpus fingerprint"
        );
        // Identical corpora → identical fingerprint (deterministic).
        let one_again = idx_from(&[("a.md", "# A\n\n## S\n\nalpha")]);
        assert_eq!(corpus_fingerprint(&one), corpus_fingerprint(&one_again));
    }

    #[test]
    fn body_hash_is_deterministic() {
        assert_eq!(body_hash("hello world"), body_hash("hello world"));
        assert_ne!(body_hash("hello world"), body_hash("hello earth"));
        assert_ne!(body_hash(""), body_hash("x"));
    }

    #[test]
    fn body_hash_is_stable_across_versions() {
        // Regression: body_hash used to use stdlib DefaultHasher, whose
        // algorithm/seeds can change between Rust toolchain versions — silently
        // invalidating the entire .umadev/kb-index/ cache. Now it's
        // truncated SHA-256, which is fixed by spec. Pin the exact u64 for a
        // known input so a regression to a non-stable hasher is caught:
        //   SHA-256("hello world")[0..8] big-endian = 13352372148217134600.
        assert_eq!(
            body_hash("hello world"),
            13_352_372_148_217_134_600u64,
            "body_hash must be stable truncated-SHA256; if this changed, the              cache will silently invalidate across Rust versions."
        );
        // Multibyte input must hash the UTF-8 bytes (stable, not char-based).
        assert_eq!(body_hash("用户登录系统"), 1_142_734_754_577_198_762u64);
        // Empty string has its own stable value.
        assert_eq!(body_hash(""), 16_406_829_232_824_261_652u64);
    }

    #[test]
    fn embed_text_format_is_stable() {
        let chunk = crate::chunker::chunk_text("docs/login.md", "# Login\n\n## OAuth\n\nbody");
        let text = embed_text(&chunk[0]);
        assert!(text.contains("Login"));
        assert!(text.contains("OAuth"));
        assert!(text.contains("body"));
    }

    #[test]
    fn mask_keeps_rare_terms_drops_common_ones() {
        // "auth" appears in EVERY doc (common, low IDF); "pkce" in one (rare,
        // high IDF). Masking must keep pkce and drop the common auth.
        let idx = idx_from(&[
            ("a.md", "# A\n\n## S\n\nauth auth pkce"),
            ("b.md", "# B\n\n## S\n\nauth auth"),
            ("c.md", "# C\n\n## S\n\nauth auth"),
            ("d.md", "# D\n\n## S\n\nauth auth"),
        ]);
        let kept = idx.mask_low_idf_terms("auth pkce", 1.0);
        assert!(kept.contains(&"pkce".to_string()), "rare term must survive");
        assert!(
            !kept.contains(&"auth".to_string()),
            "common low-IDF term must be masked: {kept:?}"
        );
    }

    #[test]
    fn mask_drops_stopwords() {
        // Hard-coded function words go regardless of corpus stats.
        let idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin oauth pkce session")]);
        let kept = idx.mask_low_idf_terms("the login system with oauth", 1.0);
        assert!(!kept.contains(&"the".to_string()), "stopword dropped");
        assert!(!kept.contains(&"with".to_string()), "stopword dropped");
        assert!(kept.contains(&"login".to_string()), "content term kept");
        assert!(kept.contains(&"oauth".to_string()), "content term kept");
    }

    #[test]
    fn mask_keeps_out_of_corpus_terms() {
        // A term the corpus doesn't contain (IDF undefined) must be KEPT — it
        // could be the exact identifier the user means.
        let idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin oauth")]);
        let kept = idx.mask_low_idf_terms("login brandnewidentifier", 1.0);
        assert!(
            kept.contains(&"brandnewidentifier".to_string()),
            "out-of-corpus term must be kept as a possible exact match: {kept:?}"
        );
    }

    #[test]
    fn mask_falls_back_when_all_low() {
        // An all-stopword query would mask to empty → fail-open returns the
        // original tokens so the search is never starved.
        let idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin")]);
        let kept = idx.mask_low_idf_terms("the and for with", 1.0);
        assert!(
            !kept.is_empty(),
            "all-stopword query must fall back to the original tokens, not empty"
        );
    }

    #[test]
    fn mask_single_term_is_untouched() {
        let idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin login login")]);
        // A single (even common) term is returned as-is — masking it would
        // leave nothing useful to search.
        assert_eq!(idx.mask_low_idf_terms("login", 1.0), vec!["login"]);
    }

    #[test]
    fn avg_doc_len_uses_bigram_length_not_trigram_inflated_tokens() {
        // A CJK corpus produces trigram tokens appended to `tokens`. `avg_doc_len`
        // must be the mean of the BIGRAM lengths (`bm25_len`), NOT of the
        // trigram-inflated `tokens.len()` — else the bigram channel's length
        // normalisation is perturbed.
        let idx = idx_from(&[
            (
                "a.md",
                "# 鉴权\n\n## 令牌\n\n用户鉴权码用于校验用户身份与会话令牌",
            ),
            (
                "b.md",
                "# 登录\n\n## 流程\n\n使用密码与验证码完成登录认证流程",
            ),
        ]);
        let mean_bigram: f64 =
            idx.chunks.iter().map(|c| c.bm25_len() as f64).sum::<f64>() / idx.chunks.len() as f64;
        assert!(
            (idx.avg_doc_len - mean_bigram).abs() < 1e-9,
            "avg_doc_len must be the mean bigram length: {} vs {}",
            idx.avg_doc_len,
            mean_bigram
        );
        // And it must be strictly LESS than the trigram-inflated mean, proving the
        // trigram tokens were excluded.
        let mean_all: f64 = idx
            .chunks
            .iter()
            .map(|c| c.tokens.len() as f64)
            .sum::<f64>()
            / idx.chunks.len() as f64;
        assert!(
            idx.avg_doc_len < mean_all,
            "trigram tokens must not inflate avg_doc_len: {} !< {}",
            idx.avg_doc_len,
            mean_all
        );
    }

    #[test]
    fn bigram_scoring_is_unchanged_by_appended_trigram_tokens() {
        // Build the REAL index (chunks carry appended trigram tokens) and a
        // REFERENCE index over the same chunks with the trigram tokens stripped
        // (so `tokens` == bigram tokens and `bigram_len` already matches). A
        // bigram-channel query (`search`, which tokenises to bigrams/unigrams)
        // must score IDENTICALLY against both — i.e. the trigram tokens never
        // touch the bigram channel's `dl`/`avgdl`.
        let real = idx_from(&[
            (
                "a.md",
                "# 鉴权\n\n## 令牌\n\n用户鉴权码用于校验用户身份与会话令牌",
            ),
            (
                "b.md",
                "# 登录\n\n## 流程\n\n使用密码与验证码完成登录认证流程",
            ),
        ]);
        // Reference: strip the trigram tail from each chunk, keeping ONLY the
        // first `bigram_len` tokens (the bigram channel). `bm25_len` then equals
        // `tokens.len()`, so this index has no trigram contamination at all.
        let stripped: Vec<Chunk> = real
            .chunks
            .iter()
            .map(|c| {
                let mut c2 = c.clone();
                c2.tokens.truncate(c.bm25_len());
                c2
            })
            .collect();
        let reference = Bm25Index::from_chunks(stripped);

        // A pure-bigram CJK query must yield identical (idx, score) rankings.
        let q = "登录认证";
        let a = real.search(q, 10);
        let b = reference.search(q, 10);
        assert_eq!(
            a, b,
            "appended trigram tokens must not change bigram-channel BM25 scores"
        );
    }

    #[test]
    fn search_terms_matches_search() {
        // The pre-tokenised path must score identically to the string path.
        let idx = idx_from(&[
            ("login.md", "# Login\n\n## S\n\noauth pkce login"),
            ("db.md", "# DB\n\n## S\n\npostgres tuning"),
        ]);
        let a = idx.search("login oauth", 5);
        let b = idx.search_terms(&tokenize("login oauth"), 5);
        assert_eq!(a, b, "search_terms must equal search for the same query");
    }

    #[test]
    fn is_consistent_accepts_fresh_and_empty_indices() {
        assert!(
            Bm25Index::from_chunks(Vec::new()).is_consistent(),
            "empty index is consistent"
        );
        let idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin auth pkce")]);
        assert!(idx.is_consistent(), "freshly built index is consistent");
    }

    #[test]
    fn is_consistent_detects_out_of_range_posting_index() {
        let mut idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin auth")]);
        let oob = idx.postings.len() as u32 + 5;
        idx.term_map.push(("ghost".into(), oob));
        assert!(
            !idx.is_consistent(),
            "term_map index past postings detected"
        );
    }

    #[test]
    fn search_does_not_panic_on_out_of_range_posting_index() {
        // M9: a corrupt-but-shape-valid index whose term_map points past
        // `postings` must be SKIPPED, never OOB-panic (fail-open by contract).
        let mut idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin auth")]);
        let oob = idx.postings.len() as u32 + 100;
        for e in &mut idx.term_map {
            e.1 = oob;
        }
        assert!(!idx.is_consistent());
        // No panic; the OOB posting is skipped → empty result.
        assert!(idx.search("login", 5).is_empty());
        // The query-cleaning pass must be equally safe.
        let _ = idx.mask_low_idf_terms("login auth", 1.0);
    }

    #[test]
    fn search_does_not_panic_on_out_of_range_chunk_idx() {
        // M9: a posting whose `chunk_idx` points past `chunks` must be skipped,
        // never index `self.chunks` out of bounds.
        let mut idx = idx_from(&[("a.md", "# A\n\n## S\n\nlogin auth")]);
        let oob_chunk = idx.chunks.len() as u32 + 50;
        for p in &mut idx.postings {
            for d in &mut p.docs {
                d.0 = oob_chunk;
            }
        }
        assert!(!idx.is_consistent());
        assert!(idx.search("login", 5).is_empty(), "no panic, empty result");
    }

    #[test]
    fn cache_loader_discards_inconsistent_index_and_rebuilds() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let kd = root.join("knowledge");
        fs::create_dir_all(&kd).unwrap();
        fs::write(kd.join("a.md"), "# A\n\n## S\n\nlogin auth pkce").unwrap();

        // Build + cache a good index (writes bm25.bin + bm25.sig).
        let idx1 = load_or_build_index(root, &kd);
        assert!(!idx1.chunks.is_empty());

        // Overwrite bm25.bin with a shape-valid-but-INCONSISTENT index, leaving
        // the matching .sig so the loader takes the cache-hit path.
        let mut bad = idx1.clone();
        let oob = bad.postings.len() as u32 + 100;
        for e in &mut bad.term_map {
            e.1 = oob;
        }
        assert!(!bad.is_consistent());
        let idx_path = root.join(".umadev/kb-index/bm25.bin");
        fs::write(&idx_path, serde_json::to_vec(&bad).unwrap()).unwrap();

        // The loader must NOT return the corrupt cache (which would panic on
        // search); it discards it and rebuilds a consistent, queryable index.
        let idx2 = load_or_build_index(root, &kd);
        assert!(
            idx2.is_consistent(),
            "loader must discard + rebuild an inconsistent cache"
        );
        assert!(
            !idx2.search("login", 5).is_empty(),
            "the rebuilt index must be queryable"
        );
    }

    #[test]
    fn corpus_signature_carries_schema_version() {
        // The schema version is folded into the signature, so an old cache built
        // by a prior tokenizer/layout can't match after a version bump.
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let f = dir.join("a.md");
        fs::write(&f, "content").unwrap();
        let sig = corpus_signature(std::slice::from_ref(&f), std::slice::from_ref(&dir));
        assert!(
            sig.starts_with(&format!("schema=v{INDEX_SCHEMA_VERSION}")),
            "signature must be prefixed with the schema version: {sig}"
        );
    }

    #[test]
    fn signature_unchanged_corpus_reuses_memo_without_reading() {
        // PERF regression guard: the old code `read` + SHA-256'd the WHOLE corpus
        // on EVERY retrieval. With the per-file `(mtime, size)` memo, a repeat
        // signature over an unchanged corpus must read ZERO files and return the
        // byte-identical signature. `compute_signature` reports the real read
        // count via a LOCAL memo, so the assertion is deterministic (no shared
        // process-global state).
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        fs::write(dir.join("a.md"), "alpha content").unwrap();
        fs::write(dir.join("b.md"), "beta content longer").unwrap();
        let paths = vec![dir.join("a.md"), dir.join("b.md")];
        let dirs = [dir.clone()];

        let mut memo: HashMap<PathBuf, CachedFileHash> = HashMap::new();
        // Cold memo: both files are read + hashed once.
        let (sig1, reads1) = compute_signature(&paths, &dirs, &mut memo);
        assert_eq!(reads1, 2, "cold memo must read both files exactly once");
        // Warm memo, unchanged corpus: NO byte reads, identical signature.
        let (sig2, reads2) = compute_signature(&paths, &dirs, &mut memo);
        assert_eq!(
            reads2, 0,
            "an unchanged corpus must be signed from the memo without reading any file"
        );
        assert_eq!(
            sig1, sig2,
            "the memoized signature must be byte-identical to the freshly-read one"
        );
    }

    #[test]
    fn signature_rereads_only_the_edited_file_and_invalidates() {
        // A real edit (content + size change → mtime/size differ) must re-read
        // ONLY the changed file and produce a DIFFERENT signature (→ rebuild).
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        fs::write(dir.join("a.md"), "alpha").unwrap();
        fs::write(dir.join("b.md"), "beta").unwrap();
        let paths = vec![dir.join("a.md"), dir.join("b.md")];
        let dirs = [dir.clone()];

        let mut memo: HashMap<PathBuf, CachedFileHash> = HashMap::new();
        let (sig1, _) = compute_signature(&paths, &dirs, &mut memo);

        // Edit b.md — a longer body guarantees a size change even if the
        // filesystem's mtime granularity is coarse.
        fs::write(dir.join("b.md"), "beta edited with more bytes").unwrap();
        let (sig2, reads) = compute_signature(&paths, &dirs, &mut memo);
        assert_eq!(reads, 1, "only the edited file may be re-read");
        assert_ne!(
            sig1, sig2,
            "a real edit must change the signature (→ rebuild)"
        );
    }

    #[test]
    fn signature_invalidates_on_added_or_removed_file() {
        // A new or removed file must change the signature regardless of the memo.
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        fs::write(dir.join("a.md"), "alpha").unwrap();
        let dirs = [dir.clone()];

        let mut memo: HashMap<PathBuf, CachedFileHash> = HashMap::new();
        let one = vec![dir.join("a.md")];
        let (sig_one, _) = compute_signature(&one, &dirs, &mut memo);

        // Added file.
        fs::write(dir.join("b.md"), "beta").unwrap();
        let two = vec![dir.join("a.md"), dir.join("b.md")];
        let (sig_two, _) = compute_signature(&two, &dirs, &mut memo);
        assert_ne!(sig_one, sig_two, "an added file must change the signature");

        // Removed file (back to just a.md); a.md is served from the warm memo.
        let (sig_removed, reads) = compute_signature(&one, &dirs, &mut memo);
        assert_eq!(reads, 0, "the surviving file is unchanged → no re-read");
        assert_eq!(
            sig_one, sig_removed,
            "removing the added file must restore the original signature"
        );
    }

    #[test]
    fn signature_fail_open_on_missing_or_unreadable_file() {
        // A missing/unreadable file must be skipped (fail-open), never panic, and
        // must not be memoized (no `stat`), so the readable files still sign.
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        fs::write(dir.join("a.md"), "alpha").unwrap();
        let dirs = [dir.clone()];
        // b.md does not exist on disk.
        let paths = vec![dir.join("a.md"), dir.join("b.md")];

        let mut memo: HashMap<PathBuf, CachedFileHash> = HashMap::new();
        let (sig, reads) = compute_signature(&paths, &dirs, &mut memo);
        // Only a.md contributes; the missing file is skipped.
        assert_eq!(
            reads, 1,
            "only the readable file is read; the missing one is skipped"
        );
        assert!(
            sig.contains("a.md"),
            "the readable file must be in the signature: {sig}"
        );
        assert!(
            !sig.contains("b.md"),
            "the missing file must be skipped: {sig}"
        );
    }

    #[tokio::test]
    async fn build_vector_store_is_noop_without_key() {
        // No API key AND no local backend → the store build is a no-op,
        // returning None. This is the fail-open contract: BM25 dominates.
        // Neutralise any installed local model so this holds under
        // `vector-local` too.
        let _no_local = crate::testsupport::without_local_model();
        let idx = idx_from(&[("login.md", "# Login\n\n## OAuth\n\nlogin auth")]);
        let tmp = tempfile::TempDir::new().unwrap();
        let store = build_vector_store_if_enabled(tmp.path(), &idx).await;
        assert!(
            store.is_none(),
            "without a key the vector build must be a no-op"
        );
    }
}
