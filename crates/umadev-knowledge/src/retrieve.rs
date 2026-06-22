//! Unified retrieval entry point — picks the configured engine and returns
//! ranked chunks ready for prompt formatting or TUI display.
//!
//! This is the single function the agent crate calls, replacing the old
//! `phase_knowledge_digest` / `knowledge_top_files` internals. It decides:
//! 1. Which `knowledge/` subdirs are relevant for the current phase.
//! 2. Whether to use BM25 (default) or hybrid BM25+vector (when
//!    `OPENAI_EMBED_KEY` is set).
//! 3. RRF (Reciprocal Rank Fusion) to merge the two rankings when hybrid.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use umadev_spec::Phase;

use crate::chunker::Chunk;
use crate::index::{load_or_build_index_multi, Bm25Index};
use crate::vector;

/// Cross-platform home directory: `HOME` then `USERPROFILE` (Windows).
/// Returns None when neither is set (fail-open). Previously only `HOME`
/// was checked, which is usually unset on Windows.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// A retrieval hit: the chunk + a normalised 0..1 score.
#[derive(Debug, Clone)]
pub struct ScoredChunk {
    /// The matched chunk.
    pub chunk: Chunk,
    /// Normalised score in 0.0..=1.0 (1.0 = best match in this query).
    pub score: f32,
}

/// Which retrieval engine to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum RetrievalEngine {
    /// BM25 keyword retrieval only — offline, zero-dep. Opt in for air-gapped
    /// builds; otherwise the default is `Hybrid` (which degrades to exactly
    /// this when no embedding key is present).
    Bm25,
    /// BM25 + vector RRF fusion — the DEFAULT. Vector results only contribute
    /// when an embedding backend is reachable (OpenAI key or local Ollama);
    /// with neither, this behaves identically to `Bm25`, so it is always safe.
    #[default]
    Hybrid,
}

/// Per-project retrieval configuration (mirrors `[knowledge]` in .umadevrc).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RetrievalConfig {
    /// Whether the knowledge base is enabled at all.
    pub enabled: bool,
    /// Which engine to use.
    pub engine: RetrievalEngine,
    /// How many chunks to return per query.
    pub top_k: usize,
    /// Extra directories (relative to project root) to include alongside
    /// the built-in `knowledge/` tree.
    pub custom_dirs: Vec<String>,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: RetrievalEngine::default(),
            // The seeded standards library grew large (35+ focused standards,
            // hundreds of chunks). A multi-feature project legitimately needs
            // several at once (e.g. layering + API + auth + payment + the
            // platform/framework standard), so the per-phase digest returns the
            // top 12 ranked chunks — relevance still decides which win, but
            // enough land that no major applicable standard gets crowded out.
            // Tune per project via `[knowledge] top_k` in `.umadevrc`.
            top_k: 12,
            custom_dirs: Vec::new(),
        }
    }
}

/// Map a pipeline phase to the knowledge subdirectories most relevant to
/// it. Mirrors the legacy `phase_knowledge_digest` mapping (phases.rs:64)
/// so phase-aware filtering behaviour is preserved.
///
/// **These are UmaDev's built-in business assumptions** about which
/// knowledge folders each pipeline phase should consult (e.g. Docs reads
/// `experts/product-manager` + `experts/architect` + `experts/uiux-designer`).
/// They encode the default knowledge-base layout shipped with UmaDev.
/// Teams whose `knowledge/` tree uses different directory names have two
/// non-fork escape hatches:
/// - per-phase override via `UMADEV_KNOWLEDGE_PHASE_SUBDIRS` (full
///   replacement for a specific phase), and/or
/// - global extras via `UMADEV_KNOWLEDGE_EXTRA_SUBDIRS` (appended to
///   every phase).
///
/// If a phase filter finds nothing, `filter_by_phase` warns + falls back to
/// unfiltered top-k so the prompt still gets context.
#[must_use]
pub fn phase_subdirs(phase: Phase) -> &'static [&'static str] {
    match phase {
        Phase::Research => &[], // research scans the whole tree
        Phase::Docs => &[
            "experts/product-manager",
            "experts/architect",
            "experts/uiux-designer",
            "product",
            "architecture",
            "design",
            "frontend",
            "industries",
            // So the architecture doc can choose + standardize the target
            // platform (web / mobile / desktop / mini-program / HarmonyOS).
            "mobile",
            "desktop",
            "miniprogram",
            "harmony",
            "cross-platform",
        ],
        Phase::DocsConfirm | Phase::PreviewConfirm => &[],
        Phase::Spec => &[
            "experts/product-manager",
            "experts/architect",
            "development",
            "00-governance",
            "product",
        ],
        Phase::Frontend => &[
            "experts/frontend-lead",
            "experts/uiux-designer",
            "frontend",
            "design",
            // NOTE: `design-systems` is intentionally NOT retrieved here. The
            // CHOSEN archetype + the full anti-AI-slop rules are inlined as the
            // binding design contract (see coach::load_design_system_inject), so
            // BM25-retrieving the dir would only duplicate that content and risk
            // surfacing a DIFFERENT archetype's chunks that conflict with the
            // bound one.
            "seed-templates",
            // Multi-platform client standards — the "frontend" phase builds the
            // CLIENT, which may be web, mobile, desktop, mini-program, or
            // HarmonyOS. The relevant platform standard injects by BM25 once the
            // architecture doc declares the target platform.
            "mobile",
            "desktop",
            "miniprogram",
            "harmony",
            "cross-platform",
        ],
        Phase::Backend => &[
            "experts/backend-lead",
            "experts/architect",
            "backend",
            "api",
            "database",
            "security",
            "cloud-native",
        ],
        Phase::Quality => &[
            "experts/qa-lead",
            "experts/architect",
            "testing",
            "security",
            "performance",
            "observability",
            "00-governance",
        ],
        Phase::Delivery => &[
            "experts/devops",
            "cicd",
            "operations",
            "release-engineering",
            "compliance",
            "00-governance",
            "security",
        ],
    }
}

/// Run a retrieval query against the project's knowledge base.
///
/// Builds (or loads the cached) BM25 index, optionally queries the vector
/// store, fuses the rankings via RRF, and returns the top-K chunks with
/// normalised scores.
///
/// `project_root` is the workspace root (where `.umadev/` lives).
/// `knowledge_dir` is the `knowledge/` directory (usually `project_root/knowledge`).
///
/// Returns an empty vec when disabled, the index is empty, or the query
/// yields no matches — never errors (fail-open).
#[must_use]
pub fn retrieve(
    project_root: &Path,
    knowledge_dir: &Path,
    config: &RetrievalConfig,
    query: &str,
    phase: Phase,
) -> Vec<ScoredChunk> {
    retrieve_with_vector(project_root, knowledge_dir, config, query, phase, None)
}

/// Retrieval with a pre-embedded query vector. This is the **real hybrid
/// path**: when `query_vec` is `Some` AND the vector store is populated,
/// BM25 and vector rankings are fused via RRF (k=60). When `query_vec` is
/// `None` or the store is empty, this is identical to pure BM25.
///
/// The query vector must be obtained **asynchronously** (via
/// [`vector::embed_query`]) by the caller — typically the async runner
/// pre-embeds the requirement once, then passes `Some(&qvec)` into the
/// sync render functions. This keeps the network call isolated to the
/// runner seam and fail-open (a `None` vector just means BM25 only).
#[must_use]
pub fn retrieve_with_vector(
    project_root: &Path,
    knowledge_dir: &Path,
    config: &RetrievalConfig,
    query: &str,
    phase: Phase,
    query_vec: Option<&[f32]>,
) -> Vec<ScoredChunk> {
    retrieve_with_vector_and_expansion(
        project_root,
        knowledge_dir,
        config,
        query,
        phase,
        query_vec,
        None,
    )
}

/// Retrieval with an optional HyDE-style query EXPANSION — the BM25-first
/// answer to lexical mismatch.
///
/// BM25 only matches terms the user literally wrote; a requirement phrased in
/// the user's words often shares few tokens with the curated docs that answer
/// it. When `expansion` is `Some` (a base-generated *hypothetical answer /
/// relevant code passage* for the requirement), its BM25 ranking is computed
/// alongside the original query's and the two are RANK-FUSED via RRF (k=60).
/// The hypothetical, being written in the *answer's* vocabulary, recalls
/// docs the bare query would miss; fusing (rather than replacing) keeps the
/// query's own exact matches in the running too.
///
/// Fail-open / additive: `expansion = None` (or an empty/whitespace string, or
/// an expansion that matches nothing) is byte-for-byte identical to
/// [`retrieve_with_vector`]. The hypothetical-answer GENERATION lives in the
/// agent crate (where the base driver is); this crate only fuses the result.
#[must_use]
pub fn retrieve_with_vector_and_expansion(
    project_root: &Path,
    knowledge_dir: &Path,
    config: &RetrievalConfig,
    query: &str,
    phase: Phase,
    query_vec: Option<&[f32]>,
    expansion: Option<&str>,
) -> Vec<ScoredChunk> {
    if !config.enabled || query.trim().is_empty() {
        return Vec::new();
    }

    // Build / load the BM25 index over knowledge/ + any learned dirs.
    // Learned dirs (.umadev/learned/ and ~/.umadev/learned/) hold
    // auto-sedimented experience from prior runs.
    let mut dirs = vec![knowledge_dir.to_path_buf()];
    let project_learned = project_root.join(".umadev/learned");
    if project_learned.is_dir() {
        dirs.push(project_learned);
    }
    if let Some(home) = home_dir() {
        let global_learned = home.join(".umadev/learned");
        if global_learned.is_dir() {
            dirs.push(global_learned);
        }
    }
    let index = load_or_build_index_multi(project_root, &dirs);
    if index.chunks.is_empty() {
        return Vec::new();
    }

    // BM25 results over the full index (over-fetch so RRF has room).
    // Query-side cleaning: drop low-IDF / function-word tokens so the rare,
    // on-topic terms dominate the ranking instead of being diluted by filler
    // (and, for CJK bigram queries, by a flood of weak near-matches). The mask
    // is fail-open — if it would empty the query it returns the raw tokens — and
    // we additionally fall back to the unmasked search if the masked search
    // somehow finds nothing, so masking can only ever HELP, never starve.
    let over_fetch = config.top_k * 3;
    let masked_terms = index.mask_low_idf_terms(query, idf_floor());
    let bm25_masked = index.search_terms(&masked_terms, over_fetch);
    let query_bigram = if bm25_masked.is_empty() {
        index.search(query, over_fetch)
    } else {
        bm25_masked
    };
    let query_bm25 = fuse_trigram_channel(&index, query, query_bigram, over_fetch);
    // HyDE fusion: when a hypothetical-answer expansion is present and itself
    // matches something, RRF-fuse its ranking with the query's. Empty / no-match
    // expansion → identity (just the query ranking), preserving prior behaviour.
    let bm25_raw = match expansion {
        Some(exp) if !exp.trim().is_empty() => {
            let exp_bm25 = index.search(exp, over_fetch);
            if exp_bm25.is_empty() {
                query_bm25
            } else {
                rrf_fuse_bm25(&query_bm25, &exp_bm25, RRF_K, over_fetch)
            }
        }
        _ => query_bm25,
    };
    let bm25_hits = filter_by_phase(&index, &bm25_raw, phase, config.top_k);

    // Vector fusion only when: hybrid engine, vector layer enabled, a query
    // vector was provided, AND the store actually has vectors. Whichever ranked
    // list we end up with, it flows through ONE unified post-rank ([`normalise`]
    // + [`dedup_learned_chunks`]) so duplicate sedimented lessons can't be
    // injected twice (see the dedup rationale below).
    let use_vector =
        config.engine == RetrievalEngine::Hybrid && vector::is_enabled() && query_vec.is_some();
    let ranked = if use_vector {
        let query_vec = query_vec.unwrap_or(&[]);
        let store = vector::VectorStore::load(project_root);
        let vec_hits = if store.is_empty() {
            Vec::new()
        } else {
            store.search(query_vec, config.top_k * 3)
        };
        if vec_hits.is_empty() {
            bm25_hits
        } else {
            // Real RRF fusion: merge the two ranked lists. Fall back to BM25 if
            // fusion somehow empties (defensive).
            let fused = rrf_fuse(&index, &bm25_hits, &vec_hits, RRF_K, config.top_k);
            if fused.is_empty() {
                bm25_hits
            } else {
                fused
            }
        }
    } else {
        bm25_hits
    };
    dedup_learned_chunks(normalise(&index, ranked))
}

/// Collapse duplicate sedimented-lesson chunks so the SAME learned lesson is
/// never injected twice into one prompt.
///
/// The capture→sediment loop writes a lesson to BOTH the project dir
/// (`.umadev/learned/<domain>/lesson-*.md`) AND, once it's "global-worthy", the
/// user-home dir (`~/.umadev/learned/<domain>/<slug>.md`) with near-identical
/// content. Both dirs are indexed, so a plain BM25/RRF ranking can return the
/// project copy AND the global copy of one lesson — the worker then sees the
/// same guidance twice (noisy, and risks looking contradictory when an older
/// global copy lags a fresher project one).
///
/// This is the conservative half of the "unified reranker": rather than fuse the
/// agent crate's fingerprint channel in here (which would couple the two crates
/// and threaten the closed loop), we de-duplicate WITHIN this BM25/RRF channel
/// by CONTENT IDENTITY — `(section, title, first non-empty body line)`. Two
/// chunks sharing all three are the same material (the project + global copies
/// of one lesson have different filenames but identical content), so only the
/// first — i.e. higher-scored, since the list is already score-sorted — is kept.
///
/// Keying on content rather than a fragile `is_lesson` path heuristic means a
/// promoted-global lesson (whose filename is a slug, not `lesson-*`) still
/// collapses against its project copy. It is also safe for curated `knowledge/`:
/// distinct curated chunks never share an identical (section, title, first line)
/// triple, so they pass through untouched. Fail-open: order is otherwise
/// preserved and a unique chunk is never dropped.
fn dedup_learned_chunks(hits: Vec<ScoredChunk>) -> Vec<ScoredChunk> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<ScoredChunk> = Vec::with_capacity(hits.len());
    for hit in hits {
        let first_line = hit
            .chunk
            .body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let key = format!(
            "{}\0{}\0{}",
            hit.chunk.meta.title, hit.chunk.meta.section, first_line
        );
        if seen.insert(key) {
            out.push(hit); // first (highest-scored) copy of this content
        }
        // else: an identical-content, lower-scored copy → drop it.
    }
    out
}

/// Standard RRF constant. `k=60` is the value used by Elasticsearch and the
/// original Cormack et al. paper; it balances rank vs score contribution.
const RRF_K: u32 = 60;

/// Phase-aware retrieval — the most common entry point. Picks subdirs for
/// the phase, runs the query, returns chunks.
#[must_use]
pub fn retrieve_for_phase(
    project_root: &Path,
    knowledge_dir: &Path,
    config: &RetrievalConfig,
    query: &str,
    phase: Phase,
) -> Vec<ScoredChunk> {
    retrieve_with_vector(project_root, knowledge_dir, config, query, phase, None)
}

/// Phase-aware retrieval with a pre-embedded query vector (the hybrid path).
/// The async runner pre-embeds the requirement once, then calls this so
/// every phase gets true BM25+vector RRF fusion without re-embedding.
#[must_use]
pub fn retrieve_for_phase_with_vector(
    project_root: &Path,
    knowledge_dir: &Path,
    config: &RetrievalConfig,
    query: &str,
    phase: Phase,
    query_vec: Option<&[f32]>,
) -> Vec<ScoredChunk> {
    retrieve_with_vector(project_root, knowledge_dir, config, query, phase, query_vec)
}

/// Phase-aware retrieval with a pre-embedded query vector AND an optional
/// HyDE expansion. The single entry point the agent crate's coach seam uses
/// once it has generated a hypothetical answer: the expansion's BM25 ranking
/// is RRF-fused with the query's (see [`retrieve_with_vector_and_expansion`]),
/// composing on top of the existing BM25+vector fusion and the low-IDF mask.
/// `expansion = None` is identical to [`retrieve_for_phase_with_vector`].
#[must_use]
pub fn retrieve_for_phase_with_expansion(
    project_root: &Path,
    knowledge_dir: &Path,
    config: &RetrievalConfig,
    query: &str,
    phase: Phase,
    query_vec: Option<&[f32]>,
    expansion: Option<&str>,
) -> Vec<ScoredChunk> {
    retrieve_with_vector_and_expansion(
        project_root,
        knowledge_dir,
        config,
        query,
        phase,
        query_vec,
        expansion,
    )
}

/// Filter raw BM25 `(chunk_idx, score)` results to only chunks whose path
/// falls under a phase-relevant subdir, then take top_k.
fn filter_by_phase(
    index: &Bm25Index,
    raw: &[(usize, f64)],
    phase: Phase,
    top_k: usize,
) -> Vec<(usize, f64)> {
    // Phase subdirs: a per-phase OVERRIDE (UMADEV_KNOWLEDGE_PHASE_SUBDIRS)
    // replaces the static default when present; otherwise use the static map.
    // Either way, UMADEV_KNOWLEDGE_EXTRA_SUBDIRS is appended so a team can
    // both override specific phases AND add global extras.
    let extras: &[String] = extra_phase_subdirs();
    let base: Vec<&str> = match phase_subdirs_override(phase) {
        Some(override_dirs) => override_dirs.iter().map(String::as_str).collect(),
        None => phase_subdirs(phase).to_vec(),
    };
    let subdirs: Vec<&str> = base
        .into_iter()
        .chain(extras.iter().map(String::as_str))
        .collect();
    let subdirs: &[&str] = &subdirs;
    // Research scans the whole tree (empty subdirs = no filter).
    if subdirs.is_empty() || matches!(phase, Phase::Research) {
        return raw.iter().take(top_k).copied().collect();
    }
    let filtered: Vec<(usize, f64)> = raw
        .iter()
        .filter(|(idx, _)| {
            let path = &index.chunks[*idx].meta.path;
            // Always allow sedimented lessons through (they're cross-cutting
            // experience from prior runs). Lessons are pathed `<domain>/lesson-*`
            // after the .umadev/learned/ prefix is stripped, so we detect
            // them by the `lesson-` filename marker.
            let is_lesson = path.contains("lesson-");
            // Match on a full path SEGMENT, not a loose prefix: the subdir
            // `design` must match `design/x` but NOT `design-systems/x` (which
            // is inlined as the binding contract, not retrieved). Likewise
            // `mobile` must not match `mobile-foo`.
            let in_subdir = subdirs
                .iter()
                .any(|s| path == *s || path.starts_with(&format!("{s}/")));
            // Also accept the legacy `learned/` prefix (defensive).
            is_lesson || path.starts_with("learned") || in_subdir
        })
        .copied()
        .collect();
    // If phase-filtering wipes out everything, fall back to unfiltered top_k
    // so the prompt still gets context (better irrelevant than empty).
    if filtered.is_empty() && !raw.is_empty() {
        // Surface that phase-filtering found nothing — previously this was
        // completely silent, so a user whose `knowledge/` layout doesn't
        // match the hardcoded phase_subdirs had no way to know filtering
        // failed and they were getting unfiltered fallback results.
        eprintln!(
            "warn: knowledge phase-filter for `{phase:?}` matched 0 chunks (expected subdirs: {subdirs:?}); \
             falling back to unfiltered top-{top_k}. If your knowledge/ layout uses different \
             directory names, results may be less phase-relevant."
        );
        raw.iter().take(top_k).copied().collect()
    } else {
        filtered.into_iter().take(top_k).collect()
    }
}

/// Normalise raw BM25/RRF scores to 0.0..=1.0 (best = 1.0) and attach chunks.
/// Extra knowledge subdirs to include in phase filtering, parsed from the
/// `UMADEV_KNOWLEDGE_EXTRA_SUBDIRS` env var (comma-separated). Cached for
/// the process lifetime. These are ADDED to every phase's static subdir list
/// so a custom knowledge/ layout can opt into filtering.
fn extra_phase_subdirs() -> &'static [String] {
    static CACHE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        std::env::var("UMADEV_KNOWLEDGE_EXTRA_SUBDIRS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Per-phase subdir OVERRIDE map parsed from
/// `UMADEV_KNOWLEDGE_PHASE_SUBDIRS`. Format: `phase:dir1,dir2;phase2:dir3`
/// (semicolon-separated `phase:dirs` entries; dirs comma-separated). When a
/// phase has an entry here, it FULLY REPLACES the static default subdirs for
/// that phase (the extras still apply). Lets a team whose knowledge/ layout
/// diverges from the built-in map override specific phases without forking.
/// Returns `Some(dirs)` when an override exists for `phase`, else `None`.
fn phase_subdirs_override(phase: Phase) -> Option<&'static [String]> {
    static CACHE: std::sync::OnceLock<std::collections::HashMap<String, Vec<String>>> =
        std::sync::OnceLock::new();
    let map = CACHE.get_or_init(|| {
        let raw = std::env::var("UMADEV_KNOWLEDGE_PHASE_SUBDIRS").unwrap_or_default();
        let mut m: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for entry in raw.split(';') {
            let entry = entry.trim();
            let Some((phase_part, dirs_part)) = entry.split_once(':') else {
                continue;
            };
            let dirs: Vec<String> = dirs_part
                .split(',')
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty())
                .collect();
            if !dirs.is_empty() {
                m.insert(phase_part.trim().to_ascii_lowercase(), dirs);
            }
        }
        m
    });
    map.get(phase.id()).map(Vec::as_slice)
}

/// The minimum normalised score (fraction of the top hit's score) a chunk
/// must reach to be kept. Default 0.5 — chunks scoring below 50% of the
/// best hit are treated as noise and dropped. Override with
/// `UMADEV_KNOWLEDGE_MIN_SCORE` (0.0 = keep everything, 1.0 = only exact
/// top-score ties). Useful for weak-match-heavy queries (CJK bigram queries
/// match many chunks loosely) where 0.5 drops everything.
fn min_score_filter() -> f32 {
    std::env::var("UMADEV_KNOWLEDGE_MIN_SCORE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(0.5)
}

/// The IDF below which a query token is a candidate for low-IDF masking (the
/// absolute half of the test — see [`Bm25Index::mask_low_idf_terms`]). Default
/// `1.0`: with BM25's +1-smoothed IDF, a token appearing in roughly more than
/// ~40% of chunks falls under this, so only genuinely common terms qualify
/// (and they are still kept unless ALSO below the query's median IDF). Override
/// with `UMADEV_KNOWLEDGE_IDF_FLOOR` (e.g. `0.0` to effectively disable the
/// relative-IDF branch and mask on the stop list only).
fn idf_floor() -> f64 {
    std::env::var("UMADEV_KNOWLEDGE_IDF_FLOOR")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.max(0.0))
        .unwrap_or(1.0)
}

/// Applies a weak `quality_score` boost: a chunk with `quality_score: 95`
/// gets ~1.24× its normalised score (clamped to 1.0), so curated docs rank
/// slightly above equally-matching un-scored ones. Missing quality_score is
/// treated as 50 (neutral).
fn normalise(index: &Bm25Index, hits: Vec<(usize, f64)>) -> Vec<ScoredChunk> {
    if hits.is_empty() {
        return Vec::new();
    }
    let max = hits
        .iter()
        .map(|(_, s)| *s)
        .fold(0.0_f64, f64::max)
        .max(1e-9);
    let min_score = min_score_filter();
    // `hits` arrives sorted by score (BM25 / RRF), so rank 0 is the best match.
    hits.into_iter()
        .enumerate()
        .map(|(rank, (idx, score))| {
            let base = (score / max) as f32;
            let qs = index.chunks[idx].quality_score.unwrap_or(50).clamp(0, 100);
            // Weak boost: score × (1 + quality/200). quality=50 → ×1.0 (neutral),
            // quality=100 → ×1.5, quality=0 → ×0.5. Clamped to 1.0.
            let boosted = (base * (1.0 + qs as f32 / 200.0)).min(1.0);
            (
                rank,
                ScoredChunk {
                    chunk: index.chunks[idx].clone(),
                    score: boosted,
                },
            )
        })
        // Drop noise below the (configurable) threshold — but NEVER drop the
        // top hit, so a real match can't vanish when min_score is raised high
        // (e.g. 1.0 would otherwise return empty unless quality_score == 100).
        .filter(|(rank, sc)| *rank == 0 || sc.score >= min_score)
        .map(|(_, sc)| sc)
        .collect()
}

/// Fuse the CJK trigram channel into a bigram-channel ranking.
///
/// The trigram channel is a parallel BM25 ranking over the query's 3-char-CJK
/// windows, matched against the trigram tokens the chunker appended to each
/// chunk. Trigrams carry one more character of local context than bigrams, so
/// substring / short-phrase CJK matches land more precisely. The two rankings
/// are RRF-fused (the same rank fusion HyDE uses).
///
/// Fail-open / identity-preserving: a query with no 3-char-CJK window yields no
/// trigram terms, and a trigram search that matches nothing both return the
/// bigram ranking unchanged — so non-CJK and short-CJK queries are byte-for-byte
/// the prior behaviour.
fn fuse_trigram_channel(
    index: &Bm25Index,
    query: &str,
    bigram: Vec<(usize, f64)>,
    over_fetch: usize,
) -> Vec<(usize, f64)> {
    // Gate on a genuine CJK trigram: an ASCII-only (or short-CJK) query carries
    // no 3-char-CJK window, so fusing would only re-rank on the SAME ASCII terms
    // the bigram channel already used — a no-op for order but it would perturb
    // scores. Keeping the gate makes non-CJK retrieval byte-for-byte unchanged.
    let trigram_terms = crate::tokenizer::cjk_trigrams_only(query);
    if trigram_terms.is_empty() {
        return bigram;
    }
    // Search the FULL trigram view (CJK windows + any ASCII identifiers in the
    // query) so a mixed-script phrase contributes both channels' signal.
    let trigram = index.search_terms(&crate::tokenizer::tokenize_trigram(query), over_fetch);
    if trigram.is_empty() {
        return bigram;
    }
    rrf_fuse_bm25(&bigram, &trigram, RRF_K, over_fetch)
}

/// Reciprocal Rank Fusion of TWO BM25 ranked lists that share the same
/// address space (both key on the index's `chunk_idx`). Used to fuse the
/// original query's ranking with a HyDE expansion's ranking: a chunk surfaced
/// by EITHER list scores `1/(k+rank)` from that list, and a chunk surfaced by
/// BOTH (the strongest signal — query AND hypothetical agree) sums the two.
///
/// Simpler than [`rrf_fuse`] (the BM25↔vector fuser) because no `(path,
/// section) → idx` remapping is needed — both inputs already speak chunk
/// indices. Returns chunk indices ranked by fused score, truncated to `top_k`.
fn rrf_fuse_bm25(
    primary: &[(usize, f64)],
    secondary: &[(usize, f64)],
    k: u32,
    top_k: usize,
) -> Vec<(usize, f64)> {
    let kf = f64::from(k);
    let mut scores: HashMap<usize, f64> = HashMap::new();
    for (rank, (idx, _)) in primary.iter().enumerate() {
        *scores.entry(*idx).or_insert(0.0) += 1.0 / (kf + rank as f64 + 1.0);
    }
    for (rank, (idx, _)) in secondary.iter().enumerate() {
        *scores.entry(*idx).or_insert(0.0) += 1.0 / (kf + rank as f64 + 1.0);
    }
    let mut fused: Vec<(usize, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused.truncate(top_k);
    fused
}

/// Reciprocal Rank Fusion — merge BM25 and vector ranked lists by
/// `1/(k + rank)`. `k=60` is the standard RRF constant (Elasticsearch,
/// original Cormack et al. paper).
///
/// BM25 hits are addressed by chunk index; vector hits by `(path, section)`.
/// We build a `(path, section) → chunk_idx` map from the index to unify the
/// two address spaces, then fuse. A chunk appearing in both lists gets a
/// higher fused score (the whole point of hybrid retrieval). Returns chunk
/// indices ranked by fused score, truncated to `top_k`.
fn rrf_fuse(
    index: &Bm25Index,
    bm25: &[(usize, f64)],
    vector_hits: &[(&str, &str, f32)],
    k: u32,
    top_k: usize,
) -> Vec<(usize, f64)> {
    // Map (path\0section) → chunk_idx so vector hits can be normalised to
    // the same address space as BM25 hits.
    // When the merged corpus (knowledge/ + learned dirs) contains two chunks
    // with the SAME (path, section) — e.g. knowledge/security/x.md and
    // .umadev/learned/security/x.md both strip to security/x.md — a vector hit
    // can't be unambiguously mapped back. Track those collisions and skip the
    // vector boost for them rather than boost the WRONG chunk (BM25, which keys
    // on the real chunk_idx, still ranks them correctly).
    let mut key_to_idx: HashMap<String, usize> = HashMap::new();
    let mut ambiguous: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, chunk) in index.chunks.iter().enumerate() {
        let key = format!("{}\0{}", chunk.meta.path, chunk.meta.section);
        if key_to_idx.insert(key.clone(), i).is_some() {
            ambiguous.insert(key);
        }
    }

    let mut scores: HashMap<usize, f64> = HashMap::new();
    let kf = f64::from(k);

    // BM25 contribution: rank 0 is the top hit.
    for (rank, (chunk_idx, _)) in bm25.iter().enumerate() {
        *scores.entry(*chunk_idx).or_insert(0.0) += 1.0 / (kf + rank as f64 + 1.0);
    }

    // Vector contribution: rank 0 is the top hit. Only count hits that map
    // to a known chunk (drops stale vectors whose source chunk was removed).
    for (rank, (path, section, _)) in vector_hits.iter().enumerate() {
        let key = format!("{path}\0{section}");
        if ambiguous.contains(&key) {
            continue; // colliding key — don't risk boosting the wrong chunk
        }
        if let Some(&chunk_idx) = key_to_idx.get(&key) {
            *scores.entry(chunk_idx).or_insert(0.0) += 1.0 / (kf + rank as f64 + 1.0);
        }
    }

    let mut fused: Vec<(usize, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused.truncate(top_k);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn seed_corpus(root: &Path) -> PathBuf {
        let kd = root.join("knowledge");
        fs::create_dir_all(kd.join("security")).unwrap();
        fs::write(
            kd.join("security/login.md"),
            "# Login Playbook\n\n## OAuth\n\nUse OAuth2 with PKCE for login authentication.\n\n## Risks\n\nToken theft.",
        )
        .unwrap();
        fs::create_dir_all(kd.join("database")).unwrap();
        fs::write(
            kd.join("database/postgres.md"),
            "# Postgres\n\n## Tuning\n\nshared_buffers and work_mem tuning for the database.",
        )
        .unwrap();
        kd
    }

    #[test]
    fn retrieve_returns_relevant_chunk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig::default();
        let hits = retrieve(tmp.path(), &kd, &cfg, "login oauth", Phase::Research);
        assert!(!hits.is_empty());
        assert!(hits[0].chunk.meta.path.contains("login"));
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn cjk_query_retrieves_relevant_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge");
        fs::create_dir_all(kd.join("security")).unwrap();
        fs::write(
            kd.join("security/login.md"),
            "# 登录系统\n\n## 流程\n\n使用 OAuth2 做登录认证",
        )
        .unwrap();
        let cfg = RetrievalConfig::default();
        let hits = retrieve(tmp.path(), &kd, &cfg, "做一个登录系统", Phase::Research);
        assert!(
            !hits.is_empty(),
            "CJK requirement must retrieve CJK content"
        );
    }

    #[test]
    fn disabled_config_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig {
            enabled: false,
            ..RetrievalConfig::default()
        };
        assert!(retrieve(tmp.path(), &kd, &cfg, "login", Phase::Research).is_empty());
    }

    #[test]
    fn empty_query_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig::default();
        assert!(retrieve(tmp.path(), &kd, &cfg, "   ", Phase::Research).is_empty());
    }

    #[test]
    fn phase_filter_narrows_to_subdirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig::default();
        // Quality phase maps to testing/security/00-governance — should still
        // find the security/login.md doc.
        let hits = retrieve(tmp.path(), &kd, &cfg, "login", Phase::Quality);
        assert!(hits.iter().any(|h| h.chunk.meta.path.contains("security")));
    }

    #[test]
    fn phase_filter_falls_back_when_no_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig::default();
        // Backend phase subdirs are backend/api/database/security — a query
        // matching only the postgres doc should still hit.
        let hits = retrieve(
            tmp.path(),
            &kd,
            &cfg,
            "postgres database tuning",
            Phase::Backend,
        );
        assert!(!hits.is_empty());
    }

    #[test]
    fn top_k_limits_results() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig {
            top_k: 1,
            ..RetrievalConfig::default()
        };
        let hits = retrieve(tmp.path(), &kd, &cfg, "auth login", Phase::Research);
        assert!(hits.len() <= 1);
    }

    #[test]
    fn scores_normalised_to_top_is_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig::default();
        let hits = retrieve(tmp.path(), &kd, &cfg, "login", Phase::Research);
        if !hits.is_empty() {
            assert!((hits[0].score - 1.0).abs() < 1e-5, "top hit should be ~1.0");
        }
    }

    #[test]
    fn phase_subdirs_known_phases() {
        assert!(!phase_subdirs(Phase::Backend).is_empty());
        assert!(!phase_subdirs(Phase::Frontend).is_empty());
        assert!(phase_subdirs(Phase::Research).is_empty()); // whole-tree scan
        assert!(phase_subdirs(Phase::DocsConfirm).is_empty()); // gate, no retrieval
    }

    #[test]
    fn config_round_trips() {
        let cfg = RetrievalConfig {
            enabled: false,
            engine: RetrievalEngine::Hybrid,
            top_k: 12,
            custom_dirs: vec!["team/".into()],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RetrievalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.engine, RetrievalEngine::Hybrid);
        assert_eq!(back.top_k, 12);
        assert!(!back.enabled);
    }

    #[test]
    fn dedup_learned_chunks_collapses_duplicate_lessons() {
        // Two copies of ONE sedimented lesson (project + global) with different
        // paths but identical title + body → must collapse to a single hit.
        let proj = crate::chunker::chunk_text(
            "frontend/lesson-frontend-1.md",
            "# Lesson\n\n## Symptom\n\nthe avoid-color pitfall body here",
        );
        let glob = crate::chunker::chunk_text(
            "frontend/api-Validated-color.md",
            "# Lesson\n\n## Symptom\n\nthe avoid-color pitfall body here",
        );
        // A distinct curated knowledge chunk that must NOT be touched.
        let curated = crate::chunker::chunk_text(
            "design/tokens.md",
            "# Tokens\n\n## Color\n\nuse design tokens not hex",
        );
        let mk = |c: &Chunk, s: f32| ScoredChunk {
            chunk: c.clone(),
            score: s,
        };
        let hits = vec![
            mk(&proj[0], 1.0),    // higher-scored copy → kept
            mk(&glob[0], 0.8),    // duplicate lesson → dropped
            mk(&curated[0], 0.7), // distinct knowledge → kept
        ];
        let out = dedup_learned_chunks(hits);
        assert_eq!(out.len(), 2, "duplicate lesson collapsed, curated kept");
        // The kept lesson copy is the higher-scored project one.
        assert!(out[0].chunk.meta.path.contains("lesson-frontend-1"));
        assert!(out
            .iter()
            .any(|h| h.chunk.meta.path.contains("design/tokens")));
    }

    #[test]
    fn dedup_keeps_distinct_lessons() {
        // Two genuinely different lessons (different titles) must both survive.
        let a =
            crate::chunker::chunk_text("frontend/lesson-frontend-1.md", "# A\n\n## S\n\nbody a");
        let b =
            crate::chunker::chunk_text("frontend/lesson-frontend-2.md", "# B\n\n## S\n\nbody b");
        let mk = |c: &Chunk, s: f32| ScoredChunk {
            chunk: c.clone(),
            score: s,
        };
        let out = dedup_learned_chunks(vec![mk(&a[0], 1.0), mk(&b[0], 0.9)]);
        assert_eq!(out.len(), 2, "distinct lessons must not be merged");
    }

    #[test]
    fn rrf_fuse_merges_and_promotes_overlap() {
        // Build a small index so vector (path,section) hits can map back.
        let chunks = crate::chunker::chunk_text(
            "security/login.md",
            "# Login\n\n## OAuth\n\noauth pkce\n\n## Risks\ntoken theft",
        );
        let index = Bm25Index::from_chunks(chunks);
        // BM25 ranks chunk 0 (OAuth) first, chunk 1 (Risks) second.
        let bm25: Vec<(usize, f64)> = vec![(0, 5.0), (1, 1.0)];
        // Vector also ranks chunk 0's (path, section) first.
        let vec_hits: Vec<(&str, &str, f32)> = vec![
            ("security/login.md", "OAuth", 0.98),
            ("security/login.md", "Risks", 0.70),
        ];
        let fused = rrf_fuse(&index, &bm25, &vec_hits, 60, 5);
        // Chunk 0 appears at rank 0 in both lists → highest fused score.
        assert_eq!(fused[0].0, 0);
        assert!(fused[0].1 > fused[1].1, "overlap chunk must outrank solo");
    }

    #[test]
    fn rrf_fuse_drops_unknown_vector_hits() {
        let chunks = crate::chunker::chunk_text("a.md", "# A\n\n## S\n\nbody");
        let index = Bm25Index::from_chunks(chunks);
        let bm25: Vec<(usize, f64)> = vec![(0, 3.0)];
        // A vector hit whose (path, section) doesn't exist in the index.
        let vec_hits: Vec<(&str, &str, f32)> = vec![("gone.md", "X", 0.9)];
        let fused = rrf_fuse(&index, &bm25, &vec_hits, 60, 5);
        // Only the known BM25 chunk survives.
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].0, 0);
    }

    #[test]
    fn rrf_fuse_bm25_sums_overlap_and_unions() {
        // chunk 0 is top of BOTH lists → highest fused score; chunk 2 appears in
        // only the secondary list → still included (union), but lower.
        let primary: Vec<(usize, f64)> = vec![(0, 5.0), (1, 2.0)];
        let secondary: Vec<(usize, f64)> = vec![(0, 4.0), (2, 1.0)];
        let fused = rrf_fuse_bm25(&primary, &secondary, 60, 5);
        assert_eq!(fused[0].0, 0, "the chunk in both lists must lead");
        let ids: Vec<usize> = fused.iter().map(|(i, _)| *i).collect();
        assert!(ids.contains(&1) && ids.contains(&2), "union of both lists");
        // chunk 0 (in both) outscores chunk 1 and chunk 2 (each in one).
        assert!(fused[0].1 > fused[1].1);
    }

    #[test]
    fn expansion_none_equals_plain_retrieval() {
        // The HyDE entry point with expansion=None must be byte-for-byte the
        // prior behaviour.
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig::default();
        let plain = retrieve(tmp.path(), &kd, &cfg, "login oauth", Phase::Research);
        let with_none = retrieve_with_vector_and_expansion(
            tmp.path(),
            &kd,
            &cfg,
            "login oauth",
            Phase::Research,
            None,
            None,
        );
        let paths_a: Vec<_> = plain.iter().map(|h| h.chunk.meta.path.clone()).collect();
        let paths_b: Vec<_> = with_none
            .iter()
            .map(|h| h.chunk.meta.path.clone())
            .collect();
        assert_eq!(paths_a, paths_b, "expansion=None must not change results");
    }

    #[test]
    fn expansion_recalls_a_doc_the_query_misses() {
        // The query shares NO tokens with the postgres doc; the HyDE expansion
        // (answer vocabulary) does. Fusing must surface postgres that the bare
        // query alone would never reach.
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig::default();
        // Bare query: only the login doc shares tokens.
        let bare = retrieve(tmp.path(), &kd, &cfg, "sign-in flow", Phase::Research);
        assert!(
            !bare.iter().any(|h| h.chunk.meta.path.contains("postgres")),
            "bare query should not reach the postgres doc"
        );
        // With a hypothetical answer mentioning the DB-tuning vocabulary.
        let fused = retrieve_with_vector_and_expansion(
            tmp.path(),
            &kd,
            &cfg,
            "sign-in flow",
            Phase::Research,
            None,
            Some("Use shared_buffers and work_mem tuning for the database to scale logins."),
        );
        assert!(
            fused.iter().any(|h| h.chunk.meta.path.contains("postgres")),
            "HyDE expansion must recall the doc the bare query missed"
        );
    }

    #[test]
    fn empty_expansion_is_identity() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let cfg = RetrievalConfig::default();
        let none = retrieve_with_vector_and_expansion(
            tmp.path(),
            &kd,
            &cfg,
            "login",
            Phase::Research,
            None,
            None,
        );
        let blank = retrieve_with_vector_and_expansion(
            tmp.path(),
            &kd,
            &cfg,
            "login",
            Phase::Research,
            None,
            Some("   "),
        );
        let a: Vec<_> = none.iter().map(|h| h.chunk.meta.path.clone()).collect();
        let b: Vec<_> = blank.iter().map(|h| h.chunk.meta.path.clone()).collect();
        assert_eq!(a, b, "whitespace expansion must be a no-op");
    }

    #[test]
    fn trigram_channel_recalls_precise_cjk_phrase() {
        // Two CJK docs: one is about the exact phrase "用户鉴权码", the other
        // merely contains the characters scattered. The trigram channel should
        // help the exact-phrase doc rank, and a CJK phrase query must retrieve.
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge");
        fs::create_dir_all(kd.join("security")).unwrap();
        fs::write(
            kd.join("security/auth.md"),
            "# 鉴权\n\n## 令牌\n\n用户鉴权码用于校验用户身份与会话令牌。",
        )
        .unwrap();
        let cfg = RetrievalConfig::default();
        let hits = retrieve(tmp.path(), &kd, &cfg, "用户鉴权码生成", Phase::Research);
        assert!(
            !hits.is_empty(),
            "CJK trigram phrase query must retrieve the phrase doc"
        );
        assert!(hits[0].chunk.meta.path.contains("auth"));
    }

    #[test]
    fn trigram_channel_is_identity_for_ascii_query() {
        // A non-CJK query produces no trigram terms → the trigram fusion is a
        // no-op and results match the pre-trigram bigram-only behaviour.
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = seed_corpus(tmp.path());
        let index = load_or_build_index_multi(tmp.path(), std::slice::from_ref(&kd));
        let over_fetch = 12;
        let bigram = index.search("login oauth", over_fetch);
        let fused = fuse_trigram_channel(&index, "login oauth", bigram.clone(), over_fetch);
        assert_eq!(
            bigram, fused,
            "ASCII query trigram fusion must be byte-for-byte identity"
        );
    }

    #[test]
    fn rrf_fuse_respects_top_k() {
        let chunks = crate::chunker::chunk_text(
            "a.md",
            "# A\n\n## One\n\nx\n\n## Two\n\ny\n\n## Three\n\nz",
        );
        let index = Bm25Index::from_chunks(chunks);
        let bm25: Vec<(usize, f64)> = vec![(0, 3.0), (1, 2.0), (2, 1.0)];
        let vec_hits: Vec<(&str, &str, f32)> = vec![
            ("a.md", "One", 0.9),
            ("a.md", "Two", 0.7),
            ("a.md", "Three", 0.5),
        ];
        let fused = rrf_fuse(&index, &bm25, &vec_hits, 60, 2);
        assert_eq!(fused.len(), 2);
    }
}
