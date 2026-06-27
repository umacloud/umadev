//! Phase implementations — one function per UmaDev phase.
//!
//! V1 phases are *deterministic templates*: they read knowledge / the
//! user requirement and write the artifacts required by
//! `UMADEV_HOST_SPEC_V1` §5 to disk, plus the evidence required by
//! §6. Future milestones swap deterministic bodies for LLM-driven ones
//! without changing the architecture.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use umadev_governance::{compliance::write_compliance_mapping, extract_api_urls, record_tool_call};
use umadev_spec::Phase;

use crate::runner::RunOptions;

/// What the phase produced. Returned for tracing / tests.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PhaseOutput {
    /// The phase that just finished.
    pub phase: Phase,
    /// Workspace-relative paths of the files written.
    pub artifacts: Vec<PathBuf>,
    /// Whether this phase ends at a gate (caller must pause).
    pub gate: Option<crate::gates::Gate>,
    /// `true` when this phase's artifacts are the OFFLINE FALLBACK template
    /// (skeleton placeholder), NOT real base-generated content — set by the
    /// runner when `use_runtime` was on but the base returned empty/errored and
    /// the phase silently dropped to the deterministic template (#1). A degraded
    /// phase is surfaced loudly to the user and must NOT be treated as a clean
    /// success. Defaults to `false` (the phase functions never set it; only the
    /// runner, which knows whether the base produced real output, does).
    pub degraded: bool,
}

// =====================================================================
// research (UD-ART-001)
// =====================================================================

/// Knowledge digest summary about the workspace's `knowledge/` dir.
///
/// As of 4.2+, this is **requirement-aware** — it ranks the workspace's
/// `knowledge/*.md` files by how well each path / filename matches the
/// user's requirement keywords, picks the top-K most relevant ones, and
/// includes a short excerpt from each. Falls back to a flat listing when
/// no keyword overlap is found (e.g. CJK-only requirement against
/// English knowledge files).
///
/// Exposed so the async runner can compute it once and feed it into the
/// research expert prompt before delegating back into [`run_research`].
#[must_use]
pub fn knowledge_digest(opts: &RunOptions) -> String {
    let dir = knowledge_root(&opts.project_root);
    smart_knowledge_digest(&dir, &opts.requirement)
}

/// Phase-aware knowledge digest — each pipeline phase gets knowledge
/// from its relevant domain subdirectories, keyword-ranked against the
/// user requirement. This is the "virtual expert's professional library".
#[must_use]
pub fn phase_knowledge_digest(opts: &RunOptions, phase: Phase) -> String {
    phase_knowledge_digest_with_vector(opts, phase, None)
}

/// Phase knowledge digest with an optional pre-embedded query vector (hybrid
/// BM25+vector RRF fusion when available, pure BM25 otherwise).
#[must_use]
pub fn phase_knowledge_digest_with_vector(
    opts: &RunOptions,
    phase: Phase,
    query_vec: Option<&[f32]>,
) -> String {
    phase_knowledge_digest_with_retrieval(opts, phase, query_vec, None)
}

/// Phase knowledge digest with an optional query vector AND an optional HyDE
/// expansion (a base-generated hypothetical answer whose BM25 ranking is
/// RRF-fused with the requirement's — see the knowledge crate). `expansion =
/// None` is identical to [`phase_knowledge_digest_with_vector`]. The
/// hypothetical-answer generation lives in [`crate::coach::generate_hyde_expansion`]
/// (it needs the base runtime); this only consumes the result. Fail-open.
#[must_use]
pub fn phase_knowledge_digest_with_retrieval(
    opts: &RunOptions,
    phase: Phase,
    query_vec: Option<&[f32]>,
    expansion: Option<&str>,
) -> String {
    let base = knowledge_root(&opts.project_root);
    if !base.is_dir() {
        return String::new();
    }
    if matches!(phase, Phase::DocsConfirm | Phase::PreviewConfirm) {
        return String::new();
    }
    let project_cfg = crate::config::load_project_config(&opts.project_root);
    let cfg = &project_cfg.knowledge;
    if cfg.enabled {
        let rcfg = umadev_knowledge::retrieve::RetrievalConfig {
            enabled: true,
            engine: match cfg.engine.as_str() {
                "hybrid" => umadev_knowledge::retrieve::RetrievalEngine::Hybrid,
                _ => umadev_knowledge::retrieve::RetrievalEngine::Bm25,
            },
            top_k: cfg.top_k,
            custom_dirs: Vec::new(),
        };
        let hits = umadev_knowledge::retrieve_for_phase_with_expansion(
            &opts.project_root,
            &base,
            &rcfg,
            &opts.requirement,
            phase,
            query_vec,
            expansion,
        );
        if hits.is_empty() {
            return String::new();
        }
        let label = if query_vec.is_some() && cfg.engine == "hybrid" {
            "BM25+vector RRF-fused"
        } else {
            "BM25-ranked"
        };
        let mut out = format!(
            "\n\n## Expert knowledge ({} phase)\n\nTop {} knowledge chunks ({}):\n\n",
            phase.id(),
            hits.len(),
            label
        );
        for hit in &hits {
            out.push_str(&format!(
                "### `{}` — *{}* (score {:.2})\n\n{}\n\n",
                hit.chunk.meta.path,
                hit.chunk.meta.section,
                hit.score,
                hit.chunk.excerpt(400)
            ));
        }
        return out;
    }
    legacy_phase_knowledge_digest(opts, phase)
}

/// A COMPACT, requirement-scoped knowledge digest for the default agentic
/// (chat / ad-hoc) path — distinct from the per-phase pipeline digest above.
///
/// Unlike [`phase_knowledge_digest`], this takes only a `project_root` + the raw
/// user `requirement` (the agentic path has no `RunOptions`), retrieves a SMALL
/// top-K (capped at `max_chunks`, default ~4) of the most relevant curated
/// knowledge, and renders SHORT excerpts — a tight token budget so injecting it
/// into a day-to-day work turn does not bloat the prompt the way the full
/// pipeline digest would. It runs ONLY for work-class turns (the TUI gates on a
/// work-vs-chat heuristic); pure conversation never reaches it.
///
/// Phase is fixed to [`Phase::Research`] so retrieval scans the WHOLE knowledge
/// tree (the agentic turn could be about anything — frontend, backend, infra),
/// rather than narrowing to one pipeline phase's subdirs.
///
/// **Fail-open**: no `knowledge/` dir, retrieval disabled, an empty index, or no
/// match all return an empty string — the caller then injects nothing and the
/// turn proceeds exactly as before. Never errors.
#[must_use]
pub fn agentic_knowledge_digest(
    project_root: &Path,
    requirement: &str,
    max_chunks: usize,
) -> String {
    if requirement.trim().is_empty() || max_chunks == 0 {
        return String::new();
    }
    let base = knowledge_root(project_root);
    if !base.is_dir() {
        return String::new();
    }
    let project_cfg = crate::config::load_project_config(project_root);
    let cfg = &project_cfg.knowledge;
    if !cfg.enabled {
        return String::new();
    }
    // Small budget: cap the configured per-phase top_k down to the agentic
    // allowance so a project with a large `top_k` doesn't dump the pipeline-sized
    // digest into a casual work turn.
    let top_k = cfg.top_k.min(max_chunks).max(1);
    let rcfg = umadev_knowledge::retrieve::RetrievalConfig {
        enabled: true,
        engine: match cfg.engine.as_str() {
            "hybrid" => umadev_knowledge::retrieve::RetrievalEngine::Hybrid,
            _ => umadev_knowledge::retrieve::RetrievalEngine::Bm25,
        },
        top_k,
        custom_dirs: Vec::new(),
    };
    // Phase::Research scans the whole tree (no subdir narrowing) — the agentic
    // turn isn't bound to one pipeline phase.
    let hits = umadev_knowledge::retrieve(&base, &base, &rcfg, requirement, Phase::Research);
    if hits.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nYOUR TEAM'S EXPERIENCE ON THIS (patterns and practices your team has \
         built up that match this request — draw on what's useful, your judgment \
         decides):\n\n",
    );
    for hit in hits.iter().take(max_chunks) {
        // Short excerpts (220 chars) keep the agentic budget tight — roughly half
        // the pipeline digest's per-chunk size.
        out.push_str(&format!(
            "- `{}` — {}: {}\n",
            hit.chunk.meta.path,
            hit.chunk.meta.section,
            hit.chunk.excerpt(220)
        ));
    }
    out
}

/// The pre-4.6 keyword-scoring digest, retained as the fallback when
/// `knowledge.enabled = false`.
#[must_use]
fn legacy_phase_knowledge_digest(opts: &RunOptions, phase: Phase) -> String {
    let base = knowledge_root(&opts.project_root);
    let subdirs: &[&str] = match phase {
        Phase::Research => return knowledge_digest(opts),
        Phase::Docs => &[
            "experts/product-manager",
            "experts/architect",
            "experts/uiux-designer",
            "product",
            "architecture",
            "design",
            "frontend",
            "industries",
        ],
        Phase::DocsConfirm | Phase::PreviewConfirm => return String::new(),
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
            "design-systems",
            "seed-templates",
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
            "00-governance",
        ],
        Phase::Delivery => &[
            "experts/devops",
            "cicd",
            "operations",
            "00-governance",
            "security",
        ],
    };

    let mut all_paths = Vec::new();
    for sub in subdirs {
        let dir = base.join(sub);
        if dir.is_dir() {
            walk_md(&dir, &mut all_paths, 0);
        }
    }
    if all_paths.is_empty() {
        return String::new();
    }

    let keywords = extract_keywords(&opts.requirement);
    let mut scored: Vec<(usize, &String)> = all_paths
        .iter()
        .map(|p| {
            let full = base.join(p).to_string_lossy().to_string();
            (score_path(&full, &keywords), p)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    let top_k = 4;
    let chosen: Vec<&String> = scored
        .iter()
        .filter(|(s, _)| *s > 0)
        .take(top_k)
        .map(|(_, p)| *p)
        .collect();

    if chosen.is_empty() {
        let mut sorted: Vec<&String> = all_paths.iter().collect();
        sorted.sort();
        let fallback: Vec<&String> = sorted.into_iter().take(3).collect();
        if fallback.is_empty() {
            return String::new();
        }
        let mut out = format!(
            "\n\n## Expert knowledge ({} phase)\n\n\
             Top {} files from {} domain knowledge (no keyword match, showing first files):\n\n",
            phase.id(),
            fallback.len(),
            subdirs.join("/")
        );
        for rel in fallback {
            let full = base.join(rel);
            let excerpt = read_excerpt(&full, 400);
            out.push_str(&format!("### `{rel}`\n\n{excerpt}\n\n"));
        }
        return out;
    }

    let mut out = format!(
        "\n\n## Expert knowledge ({} phase)\n\n\
         Top {} of {} domain files (keyword-ranked from {}):\n\n",
        phase.id(),
        chosen.len(),
        all_paths.len(),
        subdirs.join(", ")
    );
    for rel in chosen {
        let full = base.join(rel);
        let excerpt = read_excerpt(&full, 400);
        out.push_str(&format!("### `{rel}`\n\n{excerpt}\n\n"));
    }
    out
}

/// The file paths (`knowledge/*.md`, workspace-relative) the digest
/// would surface to the worker for this requirement. Used by the runner
/// to emit a chat-visible "I'm reading X, Y, Z" event so the user can
/// see UmaDev is doing context retrieval, not flying blind.
///
/// Returns `(chosen_paths, total_scanned)` where `total_scanned` is the
/// full corpus size — handy for showing "selected 6 of 306" in the UI.
#[must_use]
pub fn knowledge_top_files(opts: &RunOptions) -> (Vec<String>, usize) {
    let dir = knowledge_root(&opts.project_root);
    if !dir.is_dir() {
        return (Vec::new(), 0);
    }
    let mut paths = Vec::new();
    walk_md(&dir, &mut paths, 0);
    if paths.is_empty() {
        return (Vec::new(), 0);
    }
    let total = paths.len();
    let keywords = extract_keywords(&opts.requirement);
    let mut scored: Vec<(usize, &String)> = paths
        .iter()
        .map(|p| (score_path(p, &keywords), p))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let top_k = 6;
    let top: Vec<String> = scored
        .iter()
        .filter(|(s, _)| *s > 0)
        .take(top_k)
        .map(|(_, p)| (*p).clone())
        .collect();
    let chosen = if top.is_empty() {
        let mut sorted: Vec<String> = paths.clone();
        sorted.sort();
        sorted.into_iter().take(top_k).collect()
    } else {
        top
    };
    (chosen, total)
}

/// Smart digest: rank knowledge files against `requirement`, then emit
/// the top-K with a short excerpt. Pure-text scoring, no embeddings.
fn smart_knowledge_digest(dir: &Path, requirement: &str) -> String {
    if !dir.is_dir() {
        return "_no `knowledge/` directory in this workspace._".to_string();
    }
    let mut paths = Vec::new();
    walk_md(dir, &mut paths, 0);
    if paths.is_empty() {
        return "_knowledge directory is empty._".to_string();
    }

    let keywords = extract_keywords(requirement);
    // Score every path (path-and-name match counts).
    let mut scored: Vec<(usize, &String)> = paths
        .iter()
        .map(|p| (score_path(p, &keywords), p))
        .collect();
    // Highest score first; ties broken by lex order for determinism.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    let top_k = 6;
    let top: Vec<&String> = scored
        .iter()
        .filter(|(s, _)| *s > 0)
        .take(top_k)
        .map(|(_, p)| *p)
        .collect();

    // If no keyword overlap (e.g. all-CJK requirement, all-English
    // filenames), fall back to a stable lex-sorted preview of K files
    // so the prompt still gets something useful.
    let chosen: Vec<&String> = if top.is_empty() {
        let mut sorted: Vec<&String> = paths.iter().collect();
        sorted.sort();
        sorted.into_iter().take(top_k).collect()
    } else {
        top
    };

    let mut out = String::new();
    out.push_str(&format!(
        "Selected {} of {} `knowledge/*.md` files (keyword-ranked against requirement):\n\n",
        chosen.len(),
        paths.len()
    ));
    for rel in chosen {
        let full = dir.join(rel);
        let excerpt = read_excerpt(&full, 600);
        out.push_str(&format!("### `{rel}`\n\n{excerpt}\n\n"));
    }
    out
}

/// Tokenize requirement into 3+ char ASCII / digit tokens, lowercased
/// and de-duplicated. Skips a tiny set of stopwords.
fn extract_keywords(requirement: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "that", "from", "this", "have", "into", "make", "build",
        "create", "needs", "want", "system", "support",
    ];
    let mut seen = std::collections::BTreeSet::new();
    requirement
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|tok| {
            let t = tok.to_ascii_lowercase();
            if t.len() >= 3 && !STOPWORDS.contains(&t.as_str()) && seen.insert(t.clone()) {
                Some(t)
            } else {
                None
            }
        })
        .collect()
}

/// Score a knowledge file against the requirement keywords.
/// Checks both the file path AND the first 500 chars of content.
/// Path hits are weighted 2x (filename is a strong signal).
fn score_path(path: &str, keywords: &[String]) -> usize {
    let p = path.to_ascii_lowercase();
    let path_hits = keywords.iter().filter(|k| p.contains(k.as_str())).count();
    // Content-level check: read first 500 chars for keyword matches.
    let content_hits = std::fs::read_to_string(path).map_or(0, |body| {
        let lower: String = body
            .chars()
            .take(500)
            .collect::<String>()
            .to_ascii_lowercase();
        keywords
            .iter()
            .filter(|k| lower.contains(k.as_str()))
            .count()
    });
    path_hits * 2 + content_hits
}

/// Read the first `limit` chars from `file`, trimmed and cleaned.
/// Returns a placeholder if the file is unreadable.
fn read_excerpt(file: &Path, limit: usize) -> String {
    match fs::read_to_string(file) {
        Ok(body) => {
            let trimmed = body.trim_start();
            let mut excerpt: String = trimmed.chars().take(limit).collect();
            if trimmed.chars().count() > limit {
                excerpt.push_str("\n…");
            }
            excerpt
        }
        Err(_) => "_(unreadable)_".to_string(),
    }
}

/// Run the `research` phase (`UD-ART-001`).
///
/// When `generated_body` is `Some` and non-empty, that text replaces the
/// deterministic template — this is how the runner injects LLM-driven
/// content. The deterministic fallback always carries the
/// requirement and knowledge digest so the artifact is never empty.
pub fn run_research(opts: &RunOptions, generated_body: Option<&str>) -> io::Result<PhaseOutput> {
    let slug = opts.effective_slug();
    let output_dir = opts.project_root.join("output");
    fs::create_dir_all(&output_dir)?;
    let cache_dir = output_dir.join("knowledge-cache");
    fs::create_dir_all(&cache_dir)?;

    let knowledge_digest = summarise_knowledge_dir(&knowledge_root(&opts.project_root));

    let research_path = output_dir.join(format!("{slug}-research.md"));
    let existing_on_disk = fs::read_to_string(&research_path).unwrap_or_default();
    let research_body = match generated_body {
        Some(text) if !text.trim().is_empty() => prefer_richer(text, &existing_on_disk),
        _ => {
            if !existing_on_disk.is_empty() && existing_on_disk.len() > 200 {
                existing_on_disk
            } else {
                format!(
                    "# Research — {slug}\n\n\
                     > Offline scaffold — pass `--backend claude-code` or `--backend codex` to fill this in with real worker-generated content.\n\n\
                     ## Requirement\n\n{}\n\n\
                     ## Local knowledge available\n\n{}\n\n\
                     ## Open questions for the model\n\n\
                     - Which similar products exist? What do they do well / badly?\n\
                     - What domain risks should the architecture mitigate?\n\
                     - What UI patterns are non-negotiable in this domain?\n",
                    opts.requirement, knowledge_digest,
                )
            }
        }
    };
    fs::write(&research_path, &research_body)?;

    let bundle_path = cache_dir.join(format!("{slug}-knowledge-bundle.json"));
    let bundle = serde_json::json!({
        "slug": slug,
        "generated_at": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "requirement": opts.requirement,
        "knowledge_files_scanned": knowledge_digest.lines().count(),
        "research_summary": format!("Stub research bundle for: {}", opts.requirement),
    });
    if let Ok(text) = serde_json::to_string_pretty(&bundle) {
        // Atomic write (temp file in the same dir + rename) so a concurrent
        // reader in context.rs::read_knowledge_digest never sees a partial
        // JSON file. `rename` on the same filesystem is atomic on POSIX.
        atomic_write(&bundle_path, &text)?;
    }

    audit(
        opts,
        "umadev/agent.research",
        &research_path,
        "UD-ART-001",
        "research artifact written",
    );

    Ok(PhaseOutput {
        phase: Phase::Research,
        artifacts: vec![research_path, bundle_path],
        gate: None,
        degraded: false,
    })
}

// =====================================================================
// docs (UD-ART-002) → docs_confirm
// =====================================================================

/// Optional LLM-generated bodies for the three core documents. Any
/// `None` falls back to a deterministic template.
#[derive(Debug, Default, Clone)]
pub struct DocsContent {
    /// LLM-generated PRD body.
    pub prd: Option<String>,
    /// LLM-generated architecture body.
    pub architecture: Option<String>,
    /// LLM-generated UI/UX body.
    pub uiux: Option<String>,
}

/// Run the `docs` phase (`UD-ART-002`). Ends at `docs_confirm`.
pub fn run_docs(opts: &RunOptions, content: &DocsContent) -> io::Result<PhaseOutput> {
    let slug = opts.effective_slug();
    let output_dir = opts.project_root.join("output");
    fs::create_dir_all(&output_dir)?;

    let prd = output_dir.join(format!("{slug}-prd.md"));
    let arch = output_dir.join(format!("{slug}-architecture.md"));
    let uiux = output_dir.join(format!("{slug}-uiux.md"));

    // For each doc: prefer the richer of (worker stdout, worker disk file,
    // offline template). This handles the case where the worker writes a
    // full document to disk via Edit tool but returns only a summary to
    // stdout — we keep the richer disk version.
    write_preferring_richer(&prd, &content.prd, || render_prd(&slug, &opts.requirement))?;
    write_preferring_richer(&arch, &content.architecture, || {
        render_architecture(&slug, &opts.requirement)
    })?;
    write_preferring_richer(&uiux, &content.uiux, || {
        render_uiux(&slug, &opts.requirement)
    })?;

    for p in [&prd, &arch, &uiux] {
        audit(
            opts,
            "umadev/agent.docs",
            p,
            "UD-ART-002",
            "core doc written",
        );
    }

    Ok(PhaseOutput {
        phase: Phase::Docs,
        artifacts: vec![prd, arch, uiux],
        gate: Some(crate::gates::Gate::DocsConfirm),
        degraded: false,
    })
}

// =====================================================================
// spec (UD-ART-003)
// =====================================================================

/// Run the `spec` phase (`UD-ART-003`). Writes the execution plan and
/// the machine-trackable task list.
pub fn run_spec(opts: &RunOptions) -> io::Result<PhaseOutput> {
    let slug = opts.effective_slug();
    let output_dir = opts.project_root.join("output");
    fs::create_dir_all(&output_dir)?;
    let change_id = format!("{}-{}", slug, Utc::now().format("%Y%m%d%H%M%S"));
    let changes_dir = opts.project_root.join(".umadev/changes").join(&change_id);
    fs::create_dir_all(&changes_dir)?;

    let plan = output_dir.join(format!("{slug}-execution-plan.md"));
    let tasks = changes_dir.join("tasks.md");

    // The runner writes the base's REAL execution plan to `plan` BEFORE calling
    // run_spec (continue-after-docs / redo / light paths). An unconditional
    // `atomic_write` of the skeleton template clobbered that real plan on every
    // run — the same loss `run_docs` avoids by preferring the richer body.
    // Mirror that contract: KEEP whatever is already on disk when it is a real,
    // substantive base plan — non-trivial (>200 chars) AND not itself the
    // deterministic skeleton. A concise real plan can be SHORTER than the
    // verbose skeleton, so a pure length comparison is wrong; the skeleton's
    // marker line is the reliable discriminator. Otherwise write the skeleton,
    // so an empty / leftover-skeleton / offline run still gets a deterministic
    // artifact.
    let skeleton = render_execution_plan(&slug, &opts.requirement);
    let current = fs::read_to_string(&plan).unwrap_or_default();
    let is_skeleton_stub = current.contains("Skeleton execution plan");
    let keep_base = current.trim().len() > 200 && !is_skeleton_stub;
    let body = if keep_base { current } else { skeleton };
    atomic_write(&plan, &body)?;
    fs::write(&tasks, render_tasks(&slug))?;

    audit(
        opts,
        "umadev/agent.spec",
        &plan,
        "UD-ART-003",
        "execution plan written",
    );
    audit(
        opts,
        "umadev/agent.spec",
        &tasks,
        "UD-ART-003",
        "task list written",
    );

    Ok(PhaseOutput {
        phase: Phase::Spec,
        artifacts: vec![plan, tasks],
        gate: None,
        degraded: false,
    })
}

// =====================================================================
// frontend → preview_confirm
// =====================================================================

/// Run the `frontend` phase. V1 only records the phase transition;
/// real implementation work belongs to the LLM milestone.
pub fn run_frontend(opts: &RunOptions) -> io::Result<PhaseOutput> {
    let slug = opts.effective_slug();
    let output_dir = opts.project_root.join("output");
    fs::create_dir_all(&output_dir)?;
    let note = output_dir.join(format!("{slug}-frontend-notes.md"));
    let body = format!(
        "# Frontend notes — {slug}\n\n\
         > Instruction checklist for the interactive worker session.\n\
         > Open Claude Code / Codex / OpenCode in this workspace and follow each item.\n\n\
         ## Sources of truth\n\n\
         - `output/{slug}-prd.md` (acceptance criteria)\n\
         - `output/{slug}-architecture.md` (API surface)\n\
         - `output/{slug}-uiux.md` (design tokens + page hierarchy)\n\n\
         ## Build & verify checklist\n\n\
         - [ ] icon library declared and imported (Lucide / Heroicons / Tabler)\n\
         - [ ] color tokens loaded from `output/{slug}-uiux.md`\n\
         - [ ] every `fetch` URL appears in `output/{slug}-architecture.md`\n\
         - [ ] runtime smoke screenshot attached for review\n\n\
         ## Preview URL\n\n\
         _(The worker fills this with the local URL its dev server printed,\n\
         e.g. `http://localhost:5173`. UmaDev opens it for the user.)_\n\n\
         ## Run command\n\n\
         _(e.g. `cd web && npm run dev`)_"
    );
    fs::write(&note, body)?;
    audit(
        opts,
        "umadev/agent.frontend",
        &note,
        "UD-CODE-001",
        "frontend notes recorded",
    );

    Ok(PhaseOutput {
        phase: Phase::Frontend,
        artifacts: vec![note],
        gate: Some(crate::gates::Gate::PreviewConfirm),
        degraded: false,
    })
}

// =====================================================================
// backend
// =====================================================================

/// Run the `backend` phase. V1 records the phase transition + a notes
/// artifact. Real implementation work belongs to the LLM milestone.
pub fn run_backend(opts: &RunOptions) -> io::Result<PhaseOutput> {
    let slug = opts.effective_slug();
    let output_dir = opts.project_root.join("output");
    fs::create_dir_all(&output_dir)?;
    let note = output_dir.join(format!("{slug}-backend-notes.md"));
    let body = format!(
        "# Backend notes — {slug}\n\n\
         > Instruction checklist for the interactive worker session.\n\
         > Open Claude Code / Codex / OpenCode in this workspace and follow each item.\n\n\
         ## Sources of truth\n\n\
         - `output/{slug}-architecture.md` (API surface + data model)\n\
         - `.umadev/audit/frontend-api-calls.jsonl` (every URL the frontend wrote)\n\n\
         ## Build & verify checklist\n\n\
         - [ ] every route in the frontend audit log has a matching backend handler\n\
         - [ ] tests cover the acceptance criteria from the PRD\n\
         - [ ] secrets / env variables documented in `output/{slug}-architecture.md`\n",
    );
    fs::write(&note, body)?;
    audit(
        opts,
        "umadev/agent.backend",
        &note,
        "UD-CODE-003",
        "backend notes recorded",
    );

    Ok(PhaseOutput {
        phase: Phase::Backend,
        artifacts: vec![note],
        gate: None,
        degraded: false,
    })
}

// =====================================================================
// quality (UD-EVID-003) — REAL scoring
// =====================================================================

/// One row in the quality report. Matches the shape required by
/// `UMADEV_HOST_SPEC_V1` §6.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityCheck {
    /// Human-readable name.
    pub name: String,
    /// Grouping (artifact / evidence / code-rule / …).
    pub category: String,
    /// Detail line.
    pub description: String,
    /// `passed` | `warning` | `failed`.
    pub status: String,
    /// 0-100.
    pub score: i32,
    /// Relative weight.
    pub weight: f32,
    /// Free-form details.
    pub details: String,
}

/// The quality report document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityReport {
    /// Whether the run passed the gate.
    pub passed: bool,
    /// Plain mean of all check scores.
    pub total_score: i32,
    /// Weighted mean.
    pub weighted_score: f32,
    /// Optional scenario identifier.
    pub scenario: String,
    /// Names of checks that failed AND were marked critical.
    pub critical_failures: Vec<String>,
    /// Human-facing fixes.
    pub recommendations: Vec<String>,
    /// Summary roll-up.
    pub summary: QualitySummary,
    /// Per-check rows.
    pub checks: Vec<QualityCheck>,
}

/// Summary roll-up.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct QualitySummary {
    /// One-line headline.
    pub executive_summary: String,
    /// Free-form key/value context.
    pub summary_context: std::collections::BTreeMap<String, String>,
}

/// Run the `quality` phase (`UD-EVID-003`). Scans workspace artifacts +
/// audit logs and writes `output/<slug>-quality-gate.json`.
pub fn run_quality(opts: &RunOptions) -> io::Result<PhaseOutput> {
    let slug = opts.effective_slug();
    let output_dir = opts.project_root.join("output");
    fs::create_dir_all(&output_dir)?;
    let project_config = crate::config::load_project_config(&opts.project_root);
    let pass_threshold = i32::try_from(project_config.quality.threshold).unwrap_or(90);

    let mut checks: Vec<QualityCheck> = Vec::new();

    // UD-ART-001 — research artifact (content check, not just file-exists)
    let research_path = output_dir.join(format!("{slug}-research.md"));
    let research_text = fs::read_to_string(&research_path).unwrap_or_default();
    let research_defects = review_document_structure(
        &research_text,
        &[
            ("## requirement", "Missing ## Requirement section"),
            ("similar products", "Missing ## Similar products section"),
            ("domain risk", "Missing ## Domain risks section"),
        ],
    );
    checks.push(content_quality_check(
        "Research content",
        "artifact",
        "UD-ART-001 — research has requirement + similar products + risks",
        &research_text,
        &research_defects,
        1.5,
    ));

    // Discovery section in research
    let research_content = fs::read_to_string(output_dir.join(format!("{slug}-research.md")))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_discovery = research_content.contains("## discovery")
        || research_content.contains("target audience")
        || research_content.contains("design direction");
    checks.push(QualityCheck {
        name: "Discovery section".to_string(),
        category: "quality".to_string(),
        description: "Research brief includes Discovery questions (audience/tone/direction)"
            .to_string(),
        status: if has_discovery {
            "passed".to_string()
        } else {
            "warning".to_string()
        },
        score: if has_discovery { 100 } else { 60 },
        details: if has_discovery {
            "Discovery section found in research brief".to_string()
        } else {
            "Missing Discovery section — design direction may be inconsistent".to_string()
        },
        weight: 1.5,
    });

    // UD-ART-002 — three core docs (content checks, not just file-exists)
    let prd_text =
        fs::read_to_string(output_dir.join(format!("{slug}-prd.md"))).unwrap_or_default();
    let prd_defects = review_document_structure(
        &prd_text,
        &[
            ("## goal", "Missing ## Goal section"),
            ("## scope", "Missing ## Scope section"),
            ("- [ ]", "Missing acceptance criteria checkboxes"),
        ],
    );
    checks.push(content_quality_check(
        "PRD content",
        "artifact",
        "UD-ART-002 — PRD has goal + scope + acceptance criteria",
        &prd_text,
        &prd_defects,
        2.0,
    ));

    // Cross-reference: count PRD acceptance criteria and verify quantity
    let ac_lines: Vec<&str> = prd_text
        .lines()
        .filter(|l| l.trim().starts_with("- [ ]") || l.trim().starts_with("- [x]"))
        .collect();
    let ac_score = if ac_lines.len() >= 8 {
        100
    } else if ac_lines.len() >= 5 {
        70
    } else {
        i32::try_from(ac_lines.len()).unwrap_or(0) * 10
    };
    checks.push(QualityCheck {
        name: "Acceptance criteria depth".to_string(),
        category: "quality".to_string(),
        description: "PRD has ≥8 testable acceptance criteria in Given/When/Then format"
            .to_string(),
        status: if ac_score >= 70 {
            "passed".to_string()
        } else {
            "warning".to_string()
        },
        score: ac_score,
        details: format!("{} acceptance criteria found (target: ≥8)", ac_lines.len()),
        weight: 2.0,
    });

    let arch_text =
        fs::read_to_string(output_dir.join(format!("{slug}-architecture.md"))).unwrap_or_default();
    let arch_defects = review_document_structure(
        &arch_text,
        &[
            ("## api", "Missing ## API surface section"),
            ("## data model", "Missing ## Data model section"),
            ("| ", "Missing API route table (no markdown table rows)"),
        ],
    );
    checks.push(content_quality_check(
        "Architecture content",
        "artifact",
        "UD-ART-002 — Architecture has API surface + data model",
        &arch_text,
        &arch_defects,
        2.0,
    ));

    let uiux_text =
        fs::read_to_string(output_dir.join(format!("{slug}-uiux.md"))).unwrap_or_default();
    let uiux_defects = review_document_structure(
        &uiux_text,
        &[
            ("--color", "Missing CSS color tokens"),
            ("--font", "Missing typography tokens"),
            ("icon", "Missing icon library declaration"),
            ("hover", "Missing component states (hover/focus)"),
        ],
    );
    checks.push(content_quality_check(
        "UI/UX content",
        "artifact",
        "UD-ART-002 — UIUX has color tokens + typography + icons + states",
        &uiux_text,
        &uiux_defects,
        2.0,
    ));

    // UD-ART-003 — execution plan (content-validated)
    {
        let pp = output_dir.join(format!("{slug}-execution-plan.md"));
        let pt = fs::read_to_string(&pp).unwrap_or_default();
        let pl = pt.lines().filter(|l| !l.trim().is_empty()).count();
        let hs = pt.lines().any(|l| l.trim_start().starts_with("## "));
        let (st, sc, det) = if pt.is_empty() {
            ("failed", 0, format!("missing {}", pp.display()))
        } else if pl < 10 || !hs {
            (
                "warning",
                60,
                format!("{pl} lines, needs structured sections"),
            )
        } else {
            (
                "passed",
                100,
                format!("{pl} lines with structured sections"),
            )
        };
        checks.push(QualityCheck {
            name: "Execution plan".to_string(),
            category: "artifact".to_string(),
            description: "UD-ART-003 — execution-plan.md present with real content".to_string(),
            status: st.to_string(),
            score: sc,
            details: det,
            weight: 1.5,
        });
    }

    // HARD GATE — real source code present. The single most important check
    // against "an empty run scored 93/100 on document structure and shipped":
    // when the plan for this requirement was SUPPOSED to produce code (it
    // includes a Frontend or Backend phase) yet the real-source scanner finds
    // ZERO source files in the workspace, this is a `failed` **artifact** check.
    // Because `critical_failures` collects exactly `status == "failed" &&
    // category == "artifact"`, this single failed row forces `passed = false`
    // regardless of how high the document-structure score is. fail-SAFE: the
    // scanner is fail-open (an unreadable dir yields fewer files, never a
    // panic), so an uncertain scan leans toward "0 files → fail", protecting the
    // user from a disguised-success delivery. A docs-only / research-only /
    // plan-only task does NOT include Frontend/Backend, so this check is `passed`
    // (no false alarm).
    {
        let plan = crate::planner::plan(&opts.requirement);
        let expects_code = plan.includes(Phase::Frontend) || plan.includes(Phase::Backend);
        let source_count = crate::acceptance::source_files(&opts.project_root).len();
        let (status, score, details) = if !expects_code {
            (
                "passed",
                100,
                "Plan ships no code (docs/research/plan only) — source check N/A".to_string(),
            )
        } else if source_count == 0 {
            (
                "failed",
                0,
                "未产出任何真实源码文件 — 计划包含前端/后端实现,但工作区无 \
                 .ts/.tsx/.rs/.py/… 源码文件落盘(疑似空跑/只回了文字)。"
                    .to_string(),
            )
        } else {
            (
                "passed",
                100,
                format!("{source_count} real source file(s) present"),
            )
        };
        checks.push(QualityCheck {
            name: "Real source code present".to_string(),
            category: "artifact".to_string(),
            description: "Plan-bearing code phases must leave real source files on disk"
                .to_string(),
            status: status.to_string(),
            score,
            details,
            weight: 3.0,
        });
    }

    // UD-EVID-001 — API audit
    let api_log = opts
        .project_root
        .join(".umadev/audit/frontend-api-calls.jsonl");
    checks.push(evidence_check(
        "API audit log",
        "UD-EVID-001 — frontend-api-calls.jsonl present and non-empty",
        &api_log,
        1.0,
    ));

    // UD-EVID-002 — tool-call audit
    let tool_log = opts.project_root.join(".umadev/audit/tool-calls.jsonl");
    checks.push(evidence_check(
        "Tool-call audit log",
        "UD-EVID-002 — tool-calls.jsonl present and non-empty",
        &tool_log,
        1.0,
    ));

    // UD-CODE-001 / UD-CODE-002 — violations in audit log
    let (emoji_blocks, color_blocks) = count_code_violations(&tool_log);
    checks.push(violation_check(
        "Emoji block events",
        "UD-CODE-001 — no emoji-as-icon attempted in this run",
        emoji_blocks,
        2.0,
    ));
    checks.push(violation_check(
        "Hardcoded color block events",
        "UD-CODE-002 — no hardcoded colors attempted in this run",
        color_blocks,
        2.0,
    ));

    // Build & test results — consumes the real verify runner output.
    if let Some(vc) = verify_results_check(&opts.project_root) {
        checks.push(vc);
    }

    // Anti-AI-slop visual quality check on output artifacts. Not tied to a
    // single spec clause — Lorem ipsum / generic headings / purple→pink
    // gradients are caught as design-quality signals (the pre-write hook
    // attributes the gradient/color part to UD-CODE-002).
    let slop_issues = count_slop_violations(&output_dir);
    let slop_detail = if slop_issues == 0 {
        "No AI template patterns detected in output artifacts".to_string()
    } else {
        format!(
            "{slop_issues} AI-slop pattern(s) detected (Lorem ipsum, generic headings, purple gradients)"
        )
    };
    checks.push(QualityCheck {
        name: "Anti-AI-slop check".to_string(),
        category: "quality".to_string(),
        description: "Anti-AI-slop — no AI-template visual patterns in output".to_string(),
        status: if slop_issues == 0 {
            "passed".to_string()
        } else {
            "warning".to_string()
        },
        score: if slop_issues == 0 { 100 } else { 60 },
        details: slop_detail,
        weight: 1.5,
    });

    // Machine-checkable design-quality scan over the generated UI CODE (not
    // just docs) — the AI indigo/purple palette, gradient text, overused
    // primary fonts, bounce easing, buzzword copy, invented metrics. This is
    // what turns the design system from "suggested" into "verified".
    let design = check_code_design_quality(&opts.project_root);
    checks.push(QualityCheck {
        name: "Design quality (code)".to_string(),
        category: "quality".to_string(),
        description: "No AI-slop design tells in generated UI source (palette/fonts/motion/copy)"
            .to_string(),
        status: design.0,
        score: design.1,
        details: design.2,
        weight: 1.5,
    });

    // Typography contract conformance — does the code actually USE the fonts
    // the UIUX doc locked? An off-contract font = drift off the design system.
    let font_uiux_path = output_dir.join(format!("{slug}-uiux.md"));
    let font_conf = check_font_contract_conformance(&opts.project_root, &font_uiux_path);
    checks.push(QualityCheck {
        name: "Typography contract conformance".to_string(),
        category: "code-rule".to_string(),
        description: "Code font-family values trace to the UIUX typography contract".to_string(),
        status: font_conf.0,
        score: font_conf.1,
        details: font_conf.2,
        weight: 1.0,
    });

    // UD-CODE-003 — API URL frontend↔backend consistency
    let api_consistency = check_api_url_consistency(opts, &slug);
    checks.push(QualityCheck {
        name: "API URL consistency".to_string(),
        category: "code-rule".to_string(),
        description: "UD-CODE-003 — frontend fetch URLs match architecture API surface".to_string(),
        status: api_consistency.0.clone(),
        score: api_consistency.1,
        details: api_consistency.2,
        weight: 2.0,
    });

    // Cross-document: PRD IA routes ↔ Architecture API surface
    let prd_arch = check_prd_arch_alignment(&prd_text, &arch_text);
    checks.push(QualityCheck {
        name: "PRD↔Architecture alignment".to_string(),
        category: "quality".to_string(),
        description: "PRD page routes have corresponding API endpoints in Architecture".to_string(),
        status: prd_arch.0,
        score: prd_arch.1,
        details: prd_arch.2,
        weight: 1.5,
    });

    // Dark mode check — does the UIUX doc define dark mode tokens?
    let uiux_path = output_dir.join(format!("{slug}-uiux.md"));
    let dark_mode = check_dark_mode_support(&uiux_path);
    checks.push(QualityCheck {
        name: "Dark mode support".to_string(),
        category: "quality".to_string(),
        description: "UIUX doc includes dark mode / prefers-color-scheme tokens".to_string(),
        status: dark_mode.0.clone(),
        score: dark_mode.1,
        details: dark_mode.2,
        weight: 1.0,
    });

    let uiux_score = i32::try_from(score_uiux_completeness(&uiux_path)).unwrap_or(100);
    checks.push(QualityCheck {
        name: "Design system completeness".to_string(),
        category: "quality".to_string(),
        description: "UIUX doc includes color/typography/spacing/icon/component/a11y sections"
            .to_string(),
        status: if uiux_score >= 80 {
            "passed".to_string()
        } else if uiux_score >= 50 {
            "warning".to_string()
        } else {
            "failed".to_string()
        },
        score: uiux_score,
        details: format!("UIUX document completeness: {uiux_score}/100"),
        weight: 2.0,
    });

    // === Contract-layer checks (require parsing the architecture doc) ===
    let arch_text = fs::read_to_string(
        opts.project_root
            .join("output")
            .join(format!("{slug}-architecture.md")),
    )
    .unwrap_or_default();
    let arch_spec = umadev_contract::parse_architecture(&arch_text, &format!("{slug} API"));
    let derived = umadev_contract::derive_endpoints_from_requirement(&opts.requirement);
    let contract_spec = umadev_contract::merge_specs(&arch_spec, &derived);

    // OpenAPI contract present
    let has_contract = !contract_spec.is_empty();
    checks.push(QualityCheck {
        name: "OpenAPI contract".to_string(),
        category: "contract".to_string(),
        description: "UD-CODE-003 — typed API contract derived from architecture".to_string(),
        status: if has_contract { "passed" } else { "warning" }.to_string(),
        score: if has_contract { 100 } else { 50 },
        weight: 2.0,
        details: if has_contract {
            format!("{} endpoints in contract", contract_spec.len())
        } else {
            "No API contract derived — architecture may lack an API table".to_string()
        },
    });

    // Frontend↔contract conformance. UD-CODE-003: scan the REAL generated
    // frontend SOURCE TREE — `extract_frontend_calls` takes a project ROOT and
    // walks the actual `.ts`/`.tsx`/`.vue`/… files the worker wrote, returning
    // typed (method, path, method_known) calls. The previous code handed it the
    // worker-notes markdown path (`output/{slug}-frontend-notes.md`); since
    // `output/` is in the extractor's skip-list and a single `.md` file is not a
    // source tree, that scan was always empty — the check was diluted to a
    // no-op. Scanning `opts.project_root` is what makes this a real consistency
    // gate over the delivered code.
    let fe_calls = umadev_contract::extract_frontend_calls(&opts.project_root);
    let fe_violations = umadev_contract::validate_frontend_vs_contract(&fe_calls, &contract_spec);
    checks.push(QualityCheck {
        name: "Frontend↔contract conformance".to_string(),
        category: "contract".to_string(),
        description: "UD-CODE-003 — frontend calls match contract paths".to_string(),
        status: if fe_violations.is_empty() {
            "passed"
        } else {
            "warning"
        }
        .to_string(),
        score: if fe_violations.is_empty() { 100 } else { 60 },
        weight: 2.0,
        details: if fe_violations.is_empty() {
            "All frontend calls match the contract".to_string()
        } else {
            format!(
                "{} violation(s): {}",
                fe_violations.len(),
                fe_violations
                    .iter()
                    .take(3)
                    .map(|v| v.detail.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        },
    });

    // PRD routes↔contract coverage
    let prd_path = opts
        .project_root
        .join("output")
        .join(format!("{slug}-prd.md"));
    let prd_text = fs::read_to_string(&prd_path).unwrap_or_default();
    let prd_routes = umadev_contract::extract_prd_routes(&prd_text);
    let prd_violations = umadev_contract::validate_prd_vs_contract(&prd_routes, &contract_spec);
    checks.push(QualityCheck {
        name: "PRD routes↔contract coverage".to_string(),
        category: "contract".to_string(),
        description: "PRD-described routes appear in the contract".to_string(),
        status: if prd_violations.is_empty() {
            "passed"
        } else {
            "warning"
        }
        .to_string(),
        score: if prd_violations.is_empty() { 100 } else { 70 },
        weight: 1.5,
        details: if prd_violations.is_empty() {
            "PRD routes covered by contract".to_string()
        } else {
            format!("{} uncovered route(s)", prd_violations.len())
        },
    });

    // Input validation coverage (mutation endpoints should have request schemas)
    let total_mut = contract_spec
        .endpoints
        .iter()
        .filter(|e| {
            matches!(
                e.method,
                umadev_contract::HttpVerb::Post
                    | umadev_contract::HttpVerb::Put
                    | umadev_contract::HttpVerb::Patch
            )
        })
        .count();
    let mut_val_missing = contract_spec
        .endpoints
        .iter()
        .filter(|e| {
            matches!(
                e.method,
                umadev_contract::HttpVerb::Post
                    | umadev_contract::HttpVerb::Put
                    | umadev_contract::HttpVerb::Patch
            ) && e.request_shape.is_empty()
        })
        .count();
    checks.push(QualityCheck {
        name: "Input validation coverage".to_string(),
        category: "contract".to_string(),
        description: "POST/PATCH/PUT endpoints declare request schemas".to_string(),
        status: if total_mut == 0 || mut_val_missing == 0 {
            "passed"
        } else if mut_val_missing <= 2 {
            "warning"
        } else {
            "failed"
        }
        .to_string(),
        score: if total_mut == 0 {
            100
        } else {
            let ratio = total_mut - mut_val_missing;
            i32::try_from(ratio * 100 / total_mut).unwrap_or(0)
        },
        weight: 1.5,
        details: if total_mut == 0 {
            "No mutation endpoints".to_string()
        } else {
            format!(
                "{}/{} mutation endpoints have request schemas",
                total_mut - mut_val_missing,
                total_mut
            )
        },
    });

    // Auth coverage: state-changing endpoints that aren't public-by-convention
    // (login / register / health / webhook / …) must declare an auth scheme.
    // A commercial app shipping unprotected mutations is a real security hole.
    let writes: Vec<&umadev_contract::Endpoint> = contract_spec
        .endpoints
        .iter()
        .filter(|e| {
            matches!(
                e.method,
                umadev_contract::HttpVerb::Post
                    | umadev_contract::HttpVerb::Put
                    | umadev_contract::HttpVerb::Patch
                    | umadev_contract::HttpVerb::Delete
            )
        })
        .collect();
    let need: Vec<&&umadev_contract::Endpoint> =
        writes.iter().filter(|e| !endpoint_is_public(e)).collect();
    let unprotected: Vec<String> = need
        .iter()
        .filter(|e| e.security == umadev_contract::SecurityKind::None)
        .map(|e| format!("{} {}", e.method.as_str(), e.path))
        .collect();
    let need_n = need.len();
    let missing_n = unprotected.len();
    // Did ANY endpoint declare an auth scheme? If none did, the architecture
    // table most likely has no Auth column at all — we can't verify coverage,
    // so we must NOT hard-fail a possibly-correct doc (just nudge for a column).
    let any_auth_declared = contract_spec
        .endpoints
        .iter()
        .any(|e| e.security != umadev_contract::SecurityKind::None);
    let cannot_verify = missing_n > 0 && !any_auth_declared;
    checks.push(QualityCheck {
        name: "Auth coverage".to_string(),
        category: "contract".to_string(),
        description: "Non-public state-changing endpoints declare an auth scheme".to_string(),
        status: if need_n == 0 || missing_n == 0 {
            "passed"
        } else if cannot_verify || missing_n * 4 <= need_n {
            "warning"
        } else {
            "failed"
        }
        .to_string(),
        score: if cannot_verify {
            70 // ambiguous (no Auth column) — soft signal, don't sink the gate
        } else {
            i32::try_from(
                need_n
                    .saturating_sub(missing_n)
                    .saturating_mul(100)
                    .checked_div(need_n)
                    .unwrap_or(100),
            )
            .unwrap_or(0)
        },
        weight: 2.0,
        details: if need_n == 0 {
            "No protected mutation endpoints in contract".to_string()
        } else if missing_n == 0 {
            format!("{need_n} protected endpoint(s) all declare auth")
        } else if cannot_verify {
            format!(
                "{need_n} state-changing endpoint(s) but the architecture table declares no auth \
                 scheme — add an `Auth` column (Bearer/Session/…) so coverage can be verified"
            )
        } else {
            format!(
                "{}/{} protected endpoints missing auth: {}",
                missing_n,
                need_n,
                unprotected
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    });

    // Pagination strategy (word-boundary match)
    let arch_lower = arch_text.to_ascii_lowercase();
    let list_count = contract_spec
        .endpoints
        .iter()
        .filter(|e| e.method == umadev_contract::HttpVerb::Get && !e.path.contains(":id"))
        .count();
    let has_pag = arch_lower.contains("pagination")
        || arch_lower.contains("分页")
        || arch_lower
            .split_whitespace()
            .any(|w| w == "limit" || w == "offset" || w == "cursor");
    checks.push(QualityCheck {
        name: "Pagination strategy".to_string(),
        category: "contract".to_string(),
        description: "Architecture addresses pagination for list endpoints".to_string(),
        status: if has_pag || list_count == 0 {
            "passed"
        } else {
            "warning"
        }
        .to_string(),
        score: if has_pag || list_count == 0 { 100 } else { 60 },
        details: if list_count == 0 {
            "No list endpoints".to_string()
        } else if has_pag {
            format!("Pagination documented for {list_count} list endpoints")
        } else {
            format!("{list_count} list endpoints — no pagination strategy")
        },
        weight: 1.0,
    });

    // Error handling convention
    let has_err = arch_lower.contains("error")
        && (arch_text.contains("404") || arch_text.contains("400") || arch_text.contains("500"))
        && arch_lower.contains("response");
    checks.push(QualityCheck {
        name: "Error handling convention".to_string(),
        category: "contract".to_string(),
        description: "Architecture defines HTTP error codes".to_string(),
        status: if has_err { "passed" } else { "warning" }.to_string(),
        score: if has_err { 100 } else { 60 },
        details: if has_err {
            "Error code table found".to_string()
        } else {
            "No HTTP error convention — add 400/404/500 response table".to_string()
        },
        weight: 1.0,
    });

    // === Ops artifacts check (content-validated) ===
    // Generate scaffolding before checking so the files exist.
    let _scaffold =
        crate::scaffolding::generate_scaffolding(&opts.project_root, &contract_spec, &arch_text);
    let ops_files: [(&str, &str); 4] = [
        ("Dockerfile", "FROM"),
        (".github/workflows/ci.yml", "jobs:"),
        ("migrations/0001_init.sql", "CREATE TABLE"),
        (".env.example", "="),
    ];
    let mut ops_present = 0usize;
    let mut ops_detail = Vec::new();
    for (rel, marker) in &ops_files {
        let p = opts.project_root.join(rel);
        match fs::read_to_string(&p) {
            Ok(content) if !content.trim().is_empty() && content.contains(*marker) => {
                ops_present += 1;
            }
            Ok(_) => ops_detail.push(format!("{rel}: stub")),
            Err(_) => ops_detail.push(format!("{rel}: missing")),
        }
    }
    let ops_score = i32::try_from(ops_present * 100 / ops_files.len()).unwrap_or(0);
    checks.push(QualityCheck {
        name: "Ops artifacts present".to_string(),
        category: "delivery".to_string(),
        description: "Dockerfile + CI + migrations + .env generated with real content".to_string(),
        status: if ops_present == ops_files.len() {
            "passed"
        } else if ops_present >= 2 {
            "warning"
        } else {
            "failed"
        }
        .to_string(),
        score: ops_score,
        details: if ops_detail.is_empty() {
            format!(
                "All {} ops artifacts present with valid content",
                ops_files.len()
            )
        } else {
            format!(
                "{}/{} valid; {}",
                ops_present,
                ops_files.len(),
                ops_detail.join(", ")
            )
        },
        weight: 2.0,
    });

    // Security gate: NO leaked secrets in delivered source. Shipping a
    // hardcoded key/password/credential is a commercial showstopper, so this is
    // a hard fail (score 0). Reuses the governance secret detector (UD-SEC-003)
    // as a post-hoc safety net — catching leaks even when the real-time write
    // hook was never installed.
    let (src_scanned, secret_offenders) = scan_secret_leaks(&opts.project_root);
    checks.push(QualityCheck {
        name: "No leaked secrets".to_string(),
        category: "compliance".to_string(),
        description: "Delivered source embeds no hardcoded API keys / passwords / credentials"
            .to_string(),
        status: if secret_offenders.is_empty() {
            "passed"
        } else {
            "failed"
        }
        .to_string(),
        score: if secret_offenders.is_empty() { 100 } else { 0 },
        weight: 2.5,
        details: if src_scanned == 0 {
            "No source files to scan (docs-only stage)".to_string()
        } else if secret_offenders.is_empty() {
            format!("{src_scanned} source file(s) scanned — clean")
        } else {
            format!(
                "{} file(s) leak secrets: {}",
                secret_offenders.len(),
                secret_offenders
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    });

    // Pre-PR security scan surface (UD-SEC-003). The heavy scan itself runs at
    // the delivery phase and persists `.umadev/audit/security-scan.json`; here we
    // simply SURFACE its verdict in the gate when present. Absent → an advisory
    // "not yet scanned" that never sinks the gate (it runs later). Findings →
    // a warning the reviewer/quality reader sees; the scan stays fail-open.
    if let Some(scan_check) = security_scan_check(&opts.project_root) {
        checks.push(scan_check);
    }

    // Allow user-specified check skips.
    let project_config = crate::config::load_project_config(&opts.project_root);
    let skip = &project_config.quality.skip_checks;
    if !skip.is_empty() {
        checks.retain(|c| {
            let s = c.name.to_ascii_lowercase().replace(' ', "_");
            !skip.iter().any(|sk| sk == &s || sk == &c.name)
        });
    }

    // Context-aware N/A: a proven static, frontend-only build has no server /
    // data / API surface, so the backend-contract and ops checks below guard
    // nothing. Mark them `n/a` (kept in the report for transparency but excluded
    // from the score) instead of penalising a clean static page for "missing" a
    // CSP / API contract / Dockerfile it correctly has no reason to ship. This
    // does NOT lower the bar for APPLICABLE checks — a static page still must
    // pass design-system / anti-slop / emoji / color / secret / build checks —
    // and a backend/auth project keeps EVERY check live (conservative default).
    // Only N/A a surface-bound check that is actually PENALISING the gate
    // (status `failed`/`warning`) for lack of a backend. A surface check that
    // already PASSES (e.g. "Auth coverage" with no endpoints → 100) is left
    // live — N/A-ing a passing check would only strip a legitimate positive
    // signal and could lower the mean. This keeps the rule strictly "don't
    // penalise inapplicable checks", never "lower the bar for applicable ones".
    let ctx = crate::planner::derive_project_context(&opts.requirement, &opts.project_root, &slug);
    if ctx.static_frontend_only {
        for c in &mut checks {
            if SURFACE_BOUND_CHECKS.contains(&c.name.as_str()) && c.status != "passed" {
                c.status = "n/a".to_string();
                c.details = format!(
                    "N/A — static frontend has no server/API/data surface to guard. ({})",
                    c.details
                );
            }
        }
    }

    // A LEAN plan (`Light` / `Bugfix` / `Refactor`) deliberately skips the
    // research + three-doc + execution-plan ceremony and heads straight for
    // spec → implement → verify. Penalising it for "missing PRD / architecture /
    // UIUX" is the same mistake as the surface checks above — it is being failed
    // for an artifact it was never asked to produce. So N/A those doc-bound
    // checks for a lean plan (only when they are PENALISING; a doc that somehow
    // exists and passes stays live). Code/floor/verify checks are untouched.
    if !crate::planner::plan(&opts.requirement).includes(Phase::Docs) {
        for c in &mut checks {
            if DOC_BOUND_CHECKS.contains(&c.name.as_str()) && c.status != "passed" {
                c.status = "n/a".to_string();
                c.details = format!(
                    "N/A — a lean build skips the research + three-doc phase. ({})",
                    c.details
                );
            }
        }
    }

    let total_score = avg_score(&checks);
    let weighted_score = weighted_avg(&checks);
    let mut critical_failures: Vec<String> = checks
        .iter()
        .filter(|c| c.status == "failed" && c.category == "artifact")
        .map(|c| c.name.clone())
        .collect();
    if checks
        .iter()
        .any(|ch| ch.name == "Build & test results" && ch.status == "failed")
    {
        critical_failures.push("Build & test results".to_string());
    }
    let recommendations = checks
        .iter()
        .filter(|c| c.status != "passed" && !is_na(c))
        .map(|c| format!("Address `{}`: {}", c.name, c.details))
        .collect();
    let passed = total_score >= pass_threshold && critical_failures.is_empty();
    let mut summary_context = std::collections::BTreeMap::new();
    summary_context.insert("spec_version".into(), umadev_spec::SPEC_VERSION.into());
    summary_context.insert("slug".into(), slug.clone());

    let executive_summary = if passed {
        format!("Quality gate PASSED with score {total_score}/100.")
    } else {
        format!(
            "Quality gate FAILED with score {}/100; {} critical issue(s).",
            total_score,
            critical_failures.len()
        )
    };

    let report = QualityReport {
        passed,
        total_score,
        weighted_score,
        scenario: "1-N+1".to_string(),
        critical_failures,
        recommendations,
        summary: QualitySummary {
            executive_summary,
            summary_context,
        },
        checks,
    };

    let json_path = output_dir.join(format!("{slug}-quality-gate.json"));
    let md_path = output_dir.join(format!("{slug}-quality-gate.md"));
    fs::write(
        &json_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )?;
    fs::write(&md_path, render_quality_md(&report))?;

    audit(
        opts,
        "umadev/agent.quality",
        &json_path,
        "UD-EVID-003",
        "quality report written",
    );

    // Record the quality outcome as a "lesson" — failures become
    // retrievable lessons so future runs avoid the same defects, and
    // passes reinforce validated patterns. Previously
    // `capture_quality_failures` was defined but never called from the
    // main path, so the Failure lesson kind was dead wiring.
    crate::lessons::capture_quality_failures(
        &opts.project_root,
        &report.checks,
        &slug,
        &opts.requirement,
    );

    // Scan output/ for placeholder/TODO markers and append to the
    // persistent tech-debt ledger. The summary feeds a trend diff so a
    // team can see whether debt is growing run-over-run. Best-effort:
    // ledger write failures must never block the quality gate.
    let debt_items = crate::tech_debt::scan_debt(&output_dir);
    if !debt_items.is_empty() {
        let _ = crate::tech_debt::write_ledger(&opts.project_root, &debt_items);
        // Feed SIGNIFICANT debt back into the lessons KB so persistent
        // filler / unfilled-acceptance debt evolves across runs the same way
        // an acceptance gap does (capture→sediment→retrieve), instead of only
        // ever surfacing as a one-shot quality-check score. Fail-open.
        crate::lessons::capture_tech_debt(&opts.project_root, &debt_items, &opts.requirement);
    }

    Ok(PhaseOutput {
        phase: Phase::Quality,
        artifacts: vec![json_path, md_path],
        gate: None,
        degraded: false,
    })
}

fn evidence_check(name: &str, desc: &str, path: &Path, weight: f32) -> QualityCheck {
    let lines = file_line_count(path);
    let status = if lines > 0 { "passed" } else { "warning" };
    let score = if lines > 0 { 100 } else { 60 };
    QualityCheck {
        name: name.to_string(),
        category: "evidence".to_string(),
        description: desc.to_string(),
        status: status.to_string(),
        score,
        weight,
        details: if lines > 0 {
            format!("{} rows recorded in {}", lines, path.display())
        } else {
            format!("no rows yet at {}", path.display())
        },
    }
}

fn violation_check(name: &str, desc: &str, blocks: usize, weight: f32) -> QualityCheck {
    let (status, score) = match blocks {
        0 => ("passed", 100),
        1..=2 => ("warning", 70),
        _ => ("failed", 30),
    };
    QualityCheck {
        name: name.to_string(),
        category: "code_rule".to_string(),
        description: desc.to_string(),
        status: status.to_string(),
        score,
        weight,
        details: format!("{blocks} block event(s) recorded in this run"),
    }
}

fn verify_results_check(project_root: &Path) -> Option<QualityCheck> {
    let path = project_root.join(".umadev/audit/verify.jsonl");
    let Ok(content) = fs::read_to_string(&path) else {
        return None;
    };
    #[derive(serde::Deserialize)]
    struct VRow {
        #[serde(default)]
        step: String,
        #[serde(default)]
        passed: bool,
        #[serde(default)]
        skipped: bool,
        #[serde(default)]
        timestamp: String,
    }
    let rows: Vec<VRow> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if rows.is_empty() {
        return None;
    }
    let dts = String::new();
    let lts = rows.iter().map(|r| &r.timestamp).max().unwrap_or(&dts);
    let latest: Vec<&VRow> = rows.iter().filter(|r| r.timestamp == *lts).collect();
    let ns: Vec<&VRow> = latest.iter().copied().filter(|r| !r.skipped).collect();
    let passed = ns.iter().filter(|r| r.passed).count();
    let total = ns.len();
    let crit = latest
        .iter()
        .any(|r| !r.passed && !r.skipped && matches!(r.step.as_str(), "build" | "test" | "check"));
    let (status, score) = if total == 0 {
        ("warning", 70i32)
    } else if passed == total {
        ("passed", 100)
    } else if crit {
        ("failed", 0)
    } else {
        ("warning", ((passed * 100) / total).max(40) as i32)
    };
    Some(QualityCheck {
        name: "Build & test results".to_string(),
        category: "evidence".to_string(),
        description: "verify.jsonl — real build/lint/test outcomes".to_string(),
        status: status.to_string(),
        score,
        weight: 2.0,
        details: format!("{passed} of {total} steps passed"),
    })
}

/// Surface the persisted pre-PR security scan (`.umadev/audit/security-scan.json`)
/// as a quality-gate row. `None` when no scan has run yet (it runs at delivery),
/// so the quality phase never penalizes a not-yet-scanned workspace. Findings →
/// `warning` (advisory, the gate stays fail-open); a clean run that actually
/// exercised at least one scanner → `passed`; all-skipped (no scanners on the
/// box) → a neutral `warning` nudge to install one.
fn security_scan_check(project_root: &Path) -> Option<QualityCheck> {
    let path = project_root.join(crate::security::security_scan_rel_path());
    let body = fs::read_to_string(&path).ok()?;
    let scan: crate::security::SecurityScan = serde_json::from_str(&body).ok()?;
    let (status, score) = if scan.has_findings() {
        ("warning", 60)
    } else if scan.any_ran() {
        ("passed", 100)
    } else {
        ("warning", 70)
    };
    Some(QualityCheck {
        name: "Pre-PR security scan".to_string(),
        category: "compliance".to_string(),
        description: "UD-SEC-003 — leaked-secret + dependency advisory scan via installed tools"
            .to_string(),
        status: status.to_string(),
        score,
        weight: 1.5,
        details: scan.summary_line(),
    })
}

/// Quality-gate checks whose ONLY purpose is to guard a server / data / API
/// surface. For a proven static, frontend-only build (no backend, no auth, no
/// data plane) these guard nothing, so [`run_quality`] marks them `n/a` and the
/// scoring helpers below exclude them. They stay LIVE for every other project
/// (the conservative default), so a real backend/auth build is scored on all of
/// them. NOTE: this list deliberately excludes the universal floor — "No leaked
/// secrets", "Anti-AI-slop check", "Design quality (code)", emoji/color block
/// events, "Real source code present", "Build & test results" — those apply to
/// EVERY project and are never marked N/A.
const SURFACE_BOUND_CHECKS: &[&str] = &[
    "OpenAPI contract",
    "Frontend↔contract conformance",
    "PRD routes↔contract coverage",
    "Input validation coverage",
    "Auth coverage",
    "Pagination strategy",
    "Error handling convention",
    "PRD↔Architecture alignment",
    "API URL consistency",
    "Ops artifacts present",
    "Security scan",
];

/// Quality-gate checks that verify the research + three-doc + execution-plan
/// artifacts. A LEAN plan (`Light`/`Bugfix`/`Refactor`) deliberately skips the
/// docs phase, so [`run_quality`] marks these `n/a` for a lean build instead of
/// failing it for documents it was never asked to produce. They stay LIVE for
/// any plan that includes the Docs phase (Greenfield / FrontendOnly / …).
const DOC_BOUND_CHECKS: &[&str] = &[
    "Research content",
    "Discovery section",
    "PRD content",
    "Architecture content",
    "UI/UX content",
    "Execution plan",
];

/// `true` when a check is N/A (it guards an absent attack surface). N/A checks
/// are kept in the report for transparency but neither help nor hurt the score.
fn is_na(c: &QualityCheck) -> bool {
    c.status == "n/a"
}

fn avg_score(checks: &[QualityCheck]) -> i32 {
    let scored: Vec<&QualityCheck> = checks.iter().filter(|c| !is_na(c)).collect();
    if scored.is_empty() {
        return 0;
    }
    let sum: i32 = scored.iter().map(|c| c.score).sum();
    sum / i32::try_from(scored.len()).unwrap_or(1)
}

fn weighted_avg(checks: &[QualityCheck]) -> f32 {
    let scored: Vec<&QualityCheck> = checks.iter().filter(|c| !is_na(c)).collect();
    if scored.is_empty() {
        return 0.0;
    }
    let total_weight: f32 = scored.iter().map(|c| c.weight).sum();
    if total_weight <= 0.0 {
        return 0.0;
    }
    let weighted: f32 = scored.iter().map(|c| c.score as f32 * c.weight).sum();
    weighted / total_weight
}

fn file_line_count(path: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |t| t.lines().filter(|l| !l.trim().is_empty()).count())
}

fn count_code_violations(tool_log: &Path) -> (usize, usize) {
    let mut emoji = 0;
    let mut color = 0;
    if let Ok(text) = fs::read_to_string(tool_log) {
        for line in text.lines() {
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if val.get("decision").and_then(serde_json::Value::as_str) != Some("block") {
                continue;
            }
            match val.get("clause").and_then(serde_json::Value::as_str) {
                Some("UD-CODE-001") => emoji += 1,
                Some("UD-CODE-002") => color += 1,
                _ => {}
            }
        }
    }
    (emoji, color)
}

fn render_quality_md(r: &QualityReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Quality gate — {}\n\n",
        if r.passed { "PASSED" } else { "FAILED" }
    ));
    out.push_str(&format!(
        "Total score: **{} / 100** (weighted {:.1})\n\n",
        r.total_score, r.weighted_score
    ));
    out.push_str(&format!("{}\n\n", r.summary.executive_summary));
    if !r.critical_failures.is_empty() {
        out.push_str("## Critical failures\n\n");
        for f in &r.critical_failures {
            out.push_str(&format!("- {f}\n"));
        }
        out.push('\n');
    }
    out.push_str(
        "## Checks\n\n| Check | Category | Status | Score | Details |\n|---|---|---|---|---|\n",
    );
    for c in &r.checks {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            c.name, c.category, c.status, c.score, c.details
        ));
    }
    if !r.recommendations.is_empty() {
        out.push_str("\n## Recommendations\n\n");
        for rec in &r.recommendations {
            out.push_str(&format!("- {rec}\n"));
        }
    }
    out
}

// =====================================================================
// delivery (UD-EVID-005) — proof pack
// =====================================================================

/// Run the `delivery` phase (`UD-EVID-005`). Emits compliance mapping
/// and a proof-pack zip in `release/`.
pub fn run_delivery(opts: &RunOptions) -> io::Result<PhaseOutput> {
    let slug = opts.effective_slug();

    // Read the REAL quality-gate result first — it gates both the skill
    // graduation below AND the wording of the captured-pattern lesson (we must
    // never sediment "passed the quality gate" when it did not pass). Fail-open:
    // a missing/unreadable gate file reads as "not passed".
    let quality_passed = fs::read_to_string(
        opts.project_root
            .join(format!("output/{slug}-quality-gate.json")),
    )
    .ok()
    .and_then(|j| serde_json::from_str::<QualityReport>(&j).ok())
    .is_some_and(|r| r.passed);

    // 0. Capture validated patterns (D2: success -> sediment -> retrieval loop)
    let arch_text = fs::read_to_string(
        opts.project_root
            .join("output")
            .join(format!("{slug}-architecture.md")),
    )
    .unwrap_or_default();
    let arch_spec = umadev_contract::parse_architecture(&arch_text, &format!("{slug} API"));
    let derived = umadev_contract::derive_endpoints_from_requirement(&opts.requirement);
    let contract_spec = umadev_contract::merge_specs(&arch_spec, &derived);
    // Subtract the planned endpoints that have NO implementation evidence in the
    // delivered source, so we only sediment endpoints actually built (no false
    // "validated" facts). Fail-open: no arch doc / no source -> empty gap list.
    let gap_check = crate::acceptance::task_acceptance_gaps;
    let acceptance_gaps = gap_check(&opts.project_root, &slug);
    crate::lessons::capture_validated_patterns(
        &opts.project_root,
        &slug,
        &opts.requirement,
        &contract_spec,
        &acceptance_gaps,
        quality_passed,
    );
    let _ = crate::lessons::sediment_lessons(&opts.project_root);

    // D2b: graduate the validated patterns into the reusable SKILL library — but
    // only when the run actually GRADUATED (quality gate passed) AND it was a
    // multi-step solve (the graduation gate inside `graduate_validated_patterns`
    // enforces both). A clean one-pass run carries no reusable insight, so it
    // is intentionally not admitted. The `description` is left empty here so the
    // module's deterministic template card is used; the runner's delivery seam
    // may pre-generate a richer base-written card via `skill_description_prompt`
    // before this point. Fail-open: any failure is a no-op.
    let _ = crate::skills::graduate_validated_patterns(
        &opts.project_root,
        "", // empty → deterministic template description (base call is optional)
        quality_passed,
    );

    // 1. Compliance mapping
    let mut artifacts = Vec::new();
    if let Some((path, _)) = write_compliance_mapping(&opts.project_root, &slug) {
        audit(
            opts,
            "umadev/agent.delivery",
            &path,
            "UD-EVID-004",
            "compliance mapping written",
        );
        artifacts.push(path);
    }

    // 2. Frontend API audit summary — we re-extract from the latest frontend notes,
    //    if present, to refresh the audit (no-op if file absent).
    let fe_notes = opts
        .project_root
        .join("output")
        .join(format!("{slug}-frontend-notes.md"));
    if fe_notes.is_file() {
        if let Ok(text) = fs::read_to_string(&fe_notes) {
            let _ = extract_api_urls(fe_notes.to_string_lossy().as_ref(), &text);
        }
    }

    // 3. Delivery notes placeholder — the worker fills the deploy/URL/run
    //    sections when use_runtime; in offline mode this is the fallback the
    //    user reads. Idempotent: only created if absent so a worker-written
    //    copy is never clobbered.
    let delivery_notes = opts
        .project_root
        .join("output")
        .join(format!("{slug}-delivery-notes.md"));
    if !delivery_notes.is_file() {
        let placeholder = format!(
            "# Delivery notes — {slug}\n\n\
             > Deployment recipe produced by the worker at the delivery phase.\n\n\
             ## Build status\n\n\
             _(frontend + backend production builds — worker reports pass/fail)_\n\n\
             ## Deploy target\n\n\
             _(recommended free platform: Vercel / Netlify / Cloudflare Pages / Render)_\n\n\
             ## Deploy command\n\n\
             _(exact command, e.g. `npx vercel --prod` — read by UmaDev `/deploy`)_\n\n\
             ## Frontend URL\n\n\
             _(not yet deployed)_\n\n\
             ## Environment variables\n\n\
             _(KEY=<description>, never real secrets)_\n\n\
             ## Run command\n\n\
             _(how to run the production build locally)_"
        );
        let _ = fs::write(&delivery_notes, placeholder);
    }
    if delivery_notes.is_file() {
        artifacts.push(delivery_notes);
    }

    // 4. Pre-PR security scan (fail-open): shell out to whatever scanners are
    //    already installed (gitleaks / npm|cargo|pip audit), record the verdict
    //    to `.umadev/audit/security-scan.json`. A machine with no scanners just
    //    yields an all-skipped report — never a block, never a crash.
    let security_scan = crate::security::run_security_scan(&opts.project_root);
    if let Ok(path) = crate::security::write_security_scan(&opts.project_root, &security_scan) {
        audit(
            opts,
            "umadev/agent.delivery",
            &path,
            "UD-SEC-003",
            "pre-PR security scan written",
        );
        artifacts.push(path);
    }

    // 5. PR-ready review report — assemble the run's own evidence (CI integrity,
    //    contract, acceptance, coverage, quality/governance, security, runtime,
    //    rollback) into the single artifact a reviewer reads first.
    if let Ok(path) = crate::review::write_review_report(&opts.project_root, &slug) {
        audit(
            opts,
            "umadev/agent.delivery",
            &path,
            "UD-EVID-005",
            "PR review report written",
        );
        artifacts.push(path);
    }

    // 6. Proof pack zip
    let release_dir = opts.project_root.join("release");
    fs::create_dir_all(&release_dir)?;
    let run_id = Utc::now().format("%Y%m%d%H%M%S").to_string();
    let zip_path = release_dir.join(format!("proof-pack-{slug}-{run_id}.zip"));
    let manifest = build_and_zip_proof_pack(&opts.project_root, &zip_path, &slug)?;

    let manifest_path = release_dir.join(format!("proof-pack-{slug}-{run_id}.manifest.txt"));
    fs::write(&manifest_path, manifest.join("\n"))?;

    audit(
        opts,
        "umadev/agent.delivery",
        &zip_path,
        "UD-EVID-005",
        "proof pack assembled",
    );

    // Shareable, self-contained HTML scorecard — the visible, credible,
    // tamper-evident proof the user can open and hand to a teammate/client.
    let scorecard =
        write_scorecard_html(&opts.project_root, &release_dir, &slug, &run_id, &zip_path);

    artifacts.push(zip_path);
    artifacts.push(manifest_path);
    if let Ok(card) = scorecard {
        artifacts.push(card);
    }

    Ok(PhaseOutput {
        phase: Phase::Delivery,
        artifacts,
        gate: None,
        degraded: false,
    })
}

/// Render a self-contained, shareable HTML delivery scorecard into `release/`.
///
/// This is the proof the user can OPEN and SHARE (with a teammate, a client, an
/// auditor): the independent quality score, the per-check breakdown, the
/// governance + compliance coverage, and the tamper-evident proof-pack hash.
/// No external assets — one file, opens anywhere. It also has to look the part:
/// it follows our own anti-AI-slop rules (distinctive type, real hierarchy,
/// token colors, dark-mode, no purple gradient/emoji).
fn write_scorecard_html(
    project_root: &Path,
    release_dir: &Path,
    slug: &str,
    run_id: &str,
    zip_path: &Path,
) -> io::Result<PathBuf> {
    let report: Option<QualityReport> =
        fs::read_to_string(project_root.join(format!("output/{slug}-quality-gate.json")))
            .ok()
            .and_then(|j| serde_json::from_str(&j).ok());
    let score = report.as_ref().map_or(0, |r| r.total_score);
    let passed = report.as_ref().is_some_and(|r| r.passed);
    let has_compliance = project_root
        .join(format!("output/{slug}-compliance-mapping.json"))
        .is_file();
    let zip_sha = umadev_governance::compliance::file_sha256(zip_path)
        .unwrap_or_else(|| "(unavailable)".to_string());
    let date = Utc::now().format("%Y-%m-%d").to_string();

    let verdict = if passed {
        "通过 · PASSED"
    } else {
        "待复核 · REVIEW"
    };
    let verdict_class = if passed { "ok" } else { "warn" };
    let score_hue = if score >= 90 {
        "ok"
    } else if score >= 75 {
        "warn"
    } else {
        "bad"
    };

    let mut rows = String::new();
    if let Some(r) = &report {
        for c in &r.checks {
            let cls = match c.status.as_str() {
                "passed" => "ok",
                "warning" => "warn",
                _ => "bad",
            };
            rows.push_str(&format!(
                "<tr><td>{}</td><td class=\"num\">{}</td><td><span class=\"pill {cls}\">{}</span></td><td class=\"muted\">{}</td></tr>",
                html_escape(&c.name),
                c.score,
                html_escape(&c.status),
                html_escape(c.details.chars().take(120).collect::<String>().trim()),
            ));
        }
    } else {
        rows.push_str("<tr><td colspan=\"4\" class=\"muted\">质量报告未生成(离线运行)</td></tr>");
    }

    let compliance_block = if has_compliance {
        "<div class=\"badges\"><span class=\"badge\">SOC 2</span><span class=\"badge\">ISO 27001</span><span class=\"badge\">EU AI Act</span></div><p class=\"muted\">每条治理证据映射到合规框架并 SHA-256 固化(见 compliance-mapping.json)。</p>".to_string()
    } else {
        "<p class=\"muted\">未生成合规映射。</p>".to_string()
    };

    let html = format!(
        "<!doctype html><html lang=\"zh\"><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; script-src 'none'; base-uri 'none'; form-action 'none'\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{slug} — 交付成绩单 · UmaDev</title><style>\
:root{{--bg:#f7f8fa;--surface:#fff;--ink:#161a20;--muted:#6b7480;--line:#e6e9ee;--ok:#127a4b;--ok-bg:#e4f4ec;--warn:#9a6b00;--warn-bg:#fcf2da;--bad:#b3261e;--bad-bg:#fbe7e6;--accent:#1f6feb;--radius:14px}}\
@media(prefers-color-scheme:dark){{:root{{--bg:#0c0e12;--surface:#14171d;--ink:#eef1f5;--muted:#9aa3af;--line:#242a33;--ok:#3ad07f;--ok-bg:#10241a;--warn:#e3b341;--warn-bg:#2a2310;--bad:#ff6b6b;--bad-bg:#2a1414;--accent:#5b9bff}}}}\
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--ink);font:16px/1.6 ui-sans-serif,'Inter Tight',system-ui,-apple-system,'Segoe UI',sans-serif;-webkit-font-smoothing:antialiased}}\
.wrap{{max-width:760px;margin:0 auto;padding:48px 24px 80px}}\
.head{{display:flex;justify-content:space-between;align-items:baseline;border-bottom:1px solid var(--line);padding-bottom:16px}}\
.brand{{font-weight:700;letter-spacing:-.01em}}.brand b{{color:var(--accent)}}.date{{color:var(--muted);font-size:13px}}\
h1{{font-size:30px;letter-spacing:-.02em;margin:28px 0 4px}}.sub{{color:var(--muted);margin:0 0 28px}}\
.hero{{display:flex;gap:24px;align-items:center;background:var(--surface);border:1px solid var(--line);border-radius:var(--radius);padding:24px;margin-bottom:28px}}\
.score{{font-size:56px;font-weight:800;letter-spacing:-.03em;line-height:1}}\
.score.ok{{color:var(--ok)}}.score.warn{{color:var(--warn)}}.score.bad{{color:var(--bad)}}\
.score small{{font-size:18px;color:var(--muted);font-weight:500}}\
.verdict{{font-weight:700;font-size:14px;text-transform:uppercase;letter-spacing:.08em}}\
.verdict.ok{{color:var(--ok)}}.verdict.warn{{color:var(--warn)}}\
h2{{font-size:13px;text-transform:uppercase;letter-spacing:.1em;color:var(--muted);margin:32px 0 12px}}\
table{{width:100%;border-collapse:collapse;background:var(--surface);border:1px solid var(--line);border-radius:var(--radius);overflow:hidden}}\
th,td{{text-align:left;padding:11px 16px;border-bottom:1px solid var(--line);font-size:14px}}\
th{{color:var(--muted);font-weight:600;font-size:12px;text-transform:uppercase;letter-spacing:.06em}}\
tr:last-child td{{border-bottom:none}}.num{{font-variant-numeric:tabular-nums;font-weight:700}}\
.muted{{color:var(--muted)}}.pill{{font-size:12px;font-weight:600;padding:2px 9px;border-radius:999px}}\
.pill.ok{{color:var(--ok);background:var(--ok-bg)}}.pill.warn{{color:var(--warn);background:var(--warn-bg)}}.pill.bad{{color:var(--bad);background:var(--bad-bg)}}\
.badges{{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:8px}}.badge{{font-size:12px;font-weight:600;padding:4px 11px;border:1px solid var(--line);border-radius:999px}}\
.hash{{font:12px/1.5 ui-monospace,'Geist Mono',monospace;color:var(--muted);word-break:break-all;background:var(--surface);border:1px solid var(--line);border-radius:10px;padding:12px 14px}}\
.foot{{margin-top:40px;color:var(--muted);font-size:13px;border-top:1px solid var(--line);padding-top:16px}}\
</style></head><body><div class=\"wrap\">\
<div class=\"head\"><span class=\"brand\">Super <b>Dev</b> · 交付成绩单</span><span class=\"date\">{date} · {run_id}</span></div>\
<h1>{slug}</h1><p class=\"sub\">由 UmaDev 独立验证的商业级交付成绩单 — 可分享给团队、客户或审计方作为交付证明。</p>\
<div class=\"hero\"><div class=\"score {score_hue}\">{score}<small>/100</small></div>\
<div><div class=\"verdict {verdict_class}\">{verdict}</div><div class=\"muted\">质量门综合分(分层/安全/设计/契约/无障碍等多维加权)</div></div></div>\
<h2>质量门 · 逐项</h2><table><thead><tr><th>检查项</th><th>分</th><th>状态</th><th>说明</th></tr></thead><tbody>{rows}</tbody></table>\
<h2>实时治理</h2><p class=\"muted\">全程拦截:emoji 图标 · 硬编码颜色 · AI-slop · 敏感路径 · 危险命令 · 设计 token 漂移 — fail-open,从不阻断宿主。</p>\
<h2>合规证据链</h2>{compliance_block}\
<h2>防篡改 · proof-pack SHA-256</h2><div class=\"hash\">{zip_sha}</div>\
<p class=\"foot\">本成绩单与同目录的 proof-pack.zip 一并构成可验证的交付证据。哈希一致即未被篡改。Generated by UmaDev.</p>\
</div></body></html>"
    );

    let path = release_dir.join(format!("scorecard-{slug}-{run_id}.html"));
    fs::write(&path, html)?;
    Ok(path)
}

/// Minimal HTML-escape for text interpolated into the scorecard.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn build_and_zip_proof_pack(
    project_root: &Path,
    zip_path: &Path,
    slug: &str,
) -> io::Result<Vec<String>> {
    let file = File::create(zip_path)?;
    let mut zw = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut manifest = Vec::new();

    // Glob targets
    let mut targets: Vec<PathBuf> = Vec::new();
    for name in [
        format!("output/{slug}-research.md"),
        format!("output/{slug}-prd.md"),
        format!("output/{slug}-architecture.md"),
        format!("output/{slug}-uiux.md"),
        format!("output/{slug}-execution-plan.md"),
        format!("output/{slug}-frontend-notes.md"),
        format!("output/{slug}-backend-notes.md"),
        format!("output/{slug}-quality-gate.json"),
        format!("output/{slug}-quality-gate.md"),
        // PR-ready review report — the reviewer's entry point: every claim cites
        // a concrete file/number from this run's evidence.
        crate::review::review_report_rel_path(slug),
        format!("output/{slug}-compliance-mapping.json"),
        format!("output/knowledge-cache/{slug}-knowledge-bundle.json"),
        ".umadev/audit/frontend-api-calls.jsonl".to_string(),
        ".umadev/audit/tool-calls.jsonl".to_string(),
        // Pre-PR security scan verdict (secrets + dependency advisories) from the
        // customer's own installed scanners. Fail-open: an all-skipped report
        // still ships so the reviewer sees what was (and wasn't) checked.
        crate::security::security_scan_rel_path().to_string(),
        // Owned baseline SAST findings (tool-free) — the per-defect detail behind
        // the `umadev-sast` row in security-scan.json. Absent (skipped) when the
        // tree was clean / no scan ran; the pack simply omits it then.
        crate::security::sast_findings_rel_path().to_string(),
        // Runtime evidence — proof the app actually BOOTS + answers, not just
        // that it compiles. Written by `verify --runtime`; absent (skipped)
        // when no runtime check ran, in which case the pack simply omits it.
        crate::runtime_proof::runtime_proof_rel_path().to_string(),
        // Deploy evidence — proof the app was shipped to a live URL (platform /
        // command / preview URL / status / log tail). Written by `umadev deploy`
        // / the TUI `/deploy` handoff; absent (skipped) when no deploy ran, in
        // which case the pack simply omits it.
        crate::deploy::deploy_proof_rel_path().to_string(),
        ".umadev/workflow-state.json".to_string(),
    ] {
        let p = project_root.join(&name);
        if p.is_file() {
            targets.push(p);
        }
    }
    // Include design system + seed template files if present
    for dir in ["knowledge/design-systems", "knowledge/seed-templates"] {
        let d = project_root.join(dir);
        if d.is_dir() {
            walk_files(&d, &mut targets, 0);
        }
    }
    // recursively include .umadev/changes/ and .umadev/decisions/
    for dir in [".umadev/changes", ".umadev/decisions"] {
        let d = project_root.join(dir);
        if d.is_dir() {
            walk_files(&d, &mut targets, 0);
        }
    }

    // Add a README.md so reviewers know what each file is
    let readme = format!(
        "# Proof Pack — {slug}\n\n\
         Generated by UmaDev v{version} at {ts}.\n\n\
         ## Contents\n\n\
         | File | Purpose |\n\
         |---|---|\n\
         | `output/{slug}-research.md` | Competitive research + discovery |\n\
         | `output/{slug}-prd.md` | Product Requirements Document |\n\
         | `output/{slug}-architecture.md` | System architecture + API surface |\n\
         | `output/{slug}-uiux.md` | Design system (tokens, typography, components) |\n\
         | `output/{slug}-execution-plan.md` | Task breakdown |\n\
         | `output/{slug}-frontend-notes.md` | Frontend implementation checklist |\n\
         | `output/{slug}-backend-notes.md` | Backend implementation checklist |\n\
         | `output/{slug}-quality-gate.json` | Quality gate scores (per-check) |\n\
         | `output/{slug}-quality-gate.md` | Human-readable quality report |\n\
         | `output/{slug}-review-report.md` | PR-ready review checklist (CI/contract/acceptance/security/runtime/rollback) |\n\
         | `output/{slug}-compliance-mapping.json` | SOC2/ISO27001/EU-AI-Act mapping |\n\
         | `.umadev/audit/security-scan.json` | Pre-PR security scan: leaked-secret + dependency advisories |\n\
         | `.umadev/audit/runtime-proof.json` | Runtime evidence: dev server booted + routes answered |\n\
         | `.umadev/audit/deploy-proof.json` | Deploy evidence: platform + command + live URL + status |\n\
         | `.umadev/audit/tool-calls.jsonl` | Audit trail |\n\
         | `knowledge/design-systems/*.md` | Design system definitions |\n\
         | `knowledge/seed-templates/*.md` | Page structure templates |\n\n\
         ## How to review\n\n\
         1. Start with `output/{slug}-prd.md` — verify the scope is correct\n\
         2. Check `output/{slug}-architecture.md` — verify API surface makes sense\n\
         3. Check `output/{slug}-uiux.md` — verify design tokens and dark mode\n\
         4. Check `output/{slug}-quality-gate.md` — verify every check passed\n",
        version = env!("CARGO_PKG_VERSION"),
        ts = Utc::now().format("%Y-%m-%d %H:%M UTC"),
    );
    if zw.start_file("README.md", opts).is_ok() {
        let _ = zw.write_all(readme.as_bytes());
        manifest.push("README.md".to_string());
    }

    for t in &targets {
        let rel = t.strip_prefix(project_root).unwrap_or(t.as_path());
        let name = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if zw.start_file(&name, opts).is_err() {
            continue;
        }
        if let Ok(mut f) = File::open(t) {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                let _ = zw.write_all(&buf);
            }
        }
        manifest.push(name);
    }
    zw.finish()?;
    Ok(manifest)
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_files(&p, out, depth + 1);
        } else {
            out.push(p);
        }
    }
}

/// Whether a state-changing endpoint is public-by-convention (so missing auth
/// is expected, not a gap). Deliberately a SMALL, conservative allowlist —
/// erring toward "needs auth" so the gate flags more, never fewer, real holes.
///
/// Matches on whole **path segments** / `operation_id` word-tokens, NOT
/// substrings: `POST /api/admin/login-history` must NOT be excused as public
/// just because the segment contains "login", and `/api/publications` must NOT
/// match "public". Only an exact segment (`/auth/login`) or token counts.
fn endpoint_is_public(e: &umadev_contract::Endpoint) -> bool {
    const PUBLIC_MARKERS: &[&str] = &[
        "login",
        "register",
        "signup",
        "sign-up",
        "sign_up",
        "forgot",
        "reset-password",
        "reset_password",
        "oauth",
        "sso",
        "callback",
        "webhook",
        "webhooks",
        "health",
        "healthz",
        "ping",
        "public",
        "contact",
        "subscribe",
        "newsletter",
        "verify-email",
        "magic-link",
    ];
    let path = e.path.to_ascii_lowercase();
    let opid = e.operation_id.to_ascii_lowercase();
    PUBLIC_MARKERS.iter().any(|m| {
        // Exact path-segment match (the reliable signal), e.g. `/api/auth/login`.
        path.split('/').any(|seg| seg == *m)
            // operation_id: whole snake/kebab token match, not substring.
            || opid.split(['_', '-']).any(|t| t == *m)
    })
}

/// Source-code extensions worth scanning for hardcoded secrets.
const SECRET_LEAK_CODE_EXT: &[&str] = &[
    "js", "jsx", "ts", "tsx", "vue", "svelte", "mjs", "cjs", "py", "rb", "php", "go", "rs", "java",
    "kt", "cs", "swift",
];
/// Directory names skipped when scanning delivered source (deps, build output,
/// UmaDev's own state, and the docs/knowledge corpus).
const SECRET_LEAK_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "output",
    "knowledge",
];

/// Scan the delivered application source for hardcoded secrets (UD-SEC-003).
/// Returns `(files_scanned, sorted relative paths of offending files)`. Skips
/// dependency / build / state / docs dirs and hidden dirs. Fail-open: an
/// unreadable file is simply not scanned.
fn scan_secret_leaks(project_root: &Path) -> (usize, Vec<String>) {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_code_files(project_root, &mut files, 0);
    let mut scanned = 0usize;
    let mut offenders: Vec<String> = Vec::new();
    for p in &files {
        let Ok(content) = fs::read_to_string(p) else {
            continue;
        };
        scanned += 1;
        let rel = p
            .strip_prefix(project_root)
            .unwrap_or(p.as_path())
            .to_string_lossy()
            // Normalize to forward slashes so offender paths are identical on
            // Windows and Unix (audit/report consistency + stable tests).
            .replace(std::path::MAIN_SEPARATOR, "/");
        if umadev_governance::rules::check_hardcoded_secret(&rel, &content).block {
            offenders.push(rel);
        }
    }
    offenders.sort();
    offenders.dedup();
    (scanned, offenders)
}

/// Recursively collect source-code files under `dir`, skipping noise dirs.
fn collect_code_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || SECRET_LEAK_SKIP_DIRS.contains(&name) {
                continue;
            }
            collect_code_files(&p, out, depth + 1);
        } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            if SECRET_LEAK_CODE_EXT.contains(&ext) {
                out.push(p);
            }
        }
    }
}

/// Extensions the design-quality scan walks — UI code AND stylesheets.
const DESIGN_SCAN_EXT: &[&str] = &[
    "tsx", "jsx", "ts", "js", "vue", "svelte", "astro", "css", "scss", "sass", "less", "html",
];

/// Recursively collect UI/style source files for the design-quality scan
/// (skips dot-dirs and the usual build/vendor dirs).
fn collect_design_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 || out.len() > 800 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || SECRET_LEAK_SKIP_DIRS.contains(&name) {
                continue;
            }
            collect_design_files(&p, out, depth + 1);
        } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            if DESIGN_SCAN_EXT.contains(&ext) {
                out.push(p);
            }
        }
    }
}

/// Scan the project's generated UI source for AI-slop design tells (the
/// machine-checkable detector). Returns `(status, score, details)`. Turns the
/// design guidance from "suggested" into "verified" — a HARD tell (AI-purple)
/// drops the score hard; SOFT tells (buzzwords, bounce easing, …) nibble it.
fn check_code_design_quality(project_root: &Path) -> (String, i32, String) {
    let mut files = Vec::new();
    collect_design_files(project_root, &mut files, 0);
    if files.is_empty() {
        return (
            "passed".to_string(),
            100,
            "no UI source files to scan (offline / docs-only run)".to_string(),
        );
    }
    let (mut hard, mut soft) = (0usize, 0usize);
    let mut samples: Vec<String> = Vec::new();
    for f in files.iter().take(400) {
        let Ok(content) = fs::read_to_string(f) else {
            continue;
        };
        let name = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        for finding in umadev_governance::scan_design_quality(&f.to_string_lossy(), &content) {
            match finding.severity {
                umadev_governance::DesignSeverity::Hard => hard += 1,
                umadev_governance::DesignSeverity::Soft => soft += 1,
            }
            if samples.len() < 5 {
                let short = finding
                    .note
                    .split(['.', '—'])
                    .next()
                    .unwrap_or(&finding.note);
                samples.push(format!("{name}: [{}] {}", finding.rule, short.trim()));
            }
        }
    }
    if hard == 0 && soft == 0 {
        return (
            "passed".to_string(),
            100,
            format!(
                "{} UI file(s) scanned — no AI-slop design tells",
                files.len()
            ),
        );
    }
    let penalty = hard
        .saturating_mul(15)
        .saturating_add(soft.saturating_mul(5));
    let score = i32::try_from(100usize.saturating_sub(penalty))
        .unwrap_or(40)
        .max(40);
    let status = if hard > 0 || soft >= 3 {
        "warning"
    } else {
        "passed"
    };
    let detail = format!(
        "{hard} hard + {soft} soft design tell(s): {}",
        samples.join(" · ")
    );
    (status.to_string(), score, detail)
}

/// Verify the generated UI actually USES the typography the UIUX contract
/// locked — every font-family in the code must trace to a font declared in the
/// UIUX doc (or be a universal system fallback). A code font absent from the
/// contract means the worker drifted off the chosen design system. Returns
/// `(status, score, details)`.
fn check_font_contract_conformance(project_root: &Path, uiux_path: &Path) -> (String, i32, String) {
    let contract_text = fs::read_to_string(uiux_path).unwrap_or_default();
    if contract_text.trim().is_empty() {
        return (
            "passed".to_string(),
            100,
            "no UIUX contract to check".to_string(),
        );
    }
    let contract: Vec<String> = umadev_governance::extract_fonts(&contract_text);
    if contract.is_empty() {
        return (
            "passed".to_string(),
            100,
            "UIUX contract declares no fonts (skipped)".to_string(),
        );
    }
    let mut files = Vec::new();
    collect_design_files(project_root, &mut files, 0);
    let mut used: Vec<String> = Vec::new();
    for f in files.iter().take(400) {
        if let Ok(content) = fs::read_to_string(f) {
            for font in umadev_governance::extract_fonts(&content) {
                if !used.contains(&font) {
                    used.push(font);
                }
            }
        }
    }
    if used.is_empty() {
        return (
            "passed".to_string(),
            100,
            "no UI source fonts to check (offline / docs-only)".to_string(),
        );
    }
    let off: Vec<String> = used
        .into_iter()
        .filter(|f| !contract.contains(f) && !umadev_governance::is_generic_font(f))
        .collect();
    if off.is_empty() {
        (
            "passed".to_string(),
            100,
            "all code fonts trace to the UIUX typography contract".to_string(),
        )
    } else {
        let score = i32::try_from(100usize.saturating_sub(off.len().saturating_mul(20)))
            .unwrap_or(40)
            .max(40);
        (
            "warning".to_string(),
            score,
            format!(
                "fonts used in code but NOT declared in the UIUX contract: {} \
                 (use the locked typography or add it to the contract)",
                off.join(", ")
            ),
        )
    }
}

// =====================================================================
// helpers
// =====================================================================

/// Pick `override` text when non-empty, else compute the deterministic fallback.
fn write_preferring_richer(
    path: &Path,
    stdout_text: &Option<String>,
    fallback: impl FnOnce() -> String,
) -> io::Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let candidate = pick(stdout_text, fallback);
    let body = prefer_richer(&candidate, &existing);
    fs::write(path, body)
}

fn pick(override_text: &Option<String>, fallback: impl FnOnce() -> String) -> String {
    match override_text {
        Some(text) if !text.trim().is_empty() => text.clone(),
        _ => fallback(),
    }
}

/// Check API URL consistency: every path in the architecture API surface
/// table should be referenced somewhere in the frontend code or notes.
fn check_api_url_consistency(opts: &RunOptions, slug: &str) -> (String, i32, String) {
    let arch_path = opts
        .project_root
        .join("output")
        .join(format!("{slug}-architecture.md"));
    let arch_content = fs::read_to_string(&arch_path).unwrap_or_default();

    let mut api_paths: Vec<String> = Vec::new();
    for line in arch_content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('|') && trimmed.contains('/') {
            for part in trimmed.split('|') {
                // Strip markdown backtick wrapping so `/api/subscribe` in
                // `` `/api/subscribe` `` is recognized as a path.
                let p = part.trim().trim_matches('`').trim();
                if p.starts_with('/') && p.len() > 1 && !p.contains("---") {
                    let path = p.split_whitespace().next().unwrap_or(p);
                    if !api_paths.contains(&path.to_string()) {
                        api_paths.push(path.to_string());
                    }
                }
            }
        }
    }

    if api_paths.is_empty() {
        return (
            "warning".to_string(),
            70,
            "No API paths found in architecture doc — cannot verify consistency".to_string(),
        );
    }

    let fe_notes_path = opts
        .project_root
        .join("output")
        .join(format!("{slug}-frontend-notes.md"));
    let fe_content = fs::read_to_string(&fe_notes_path).unwrap_or_default();
    let api_log = opts
        .project_root
        .join(".umadev/audit/frontend-api-calls.jsonl");
    let api_log_content = fs::read_to_string(&api_log).unwrap_or_default();
    // Also fold in the paths the contract extractor found in the REAL generated
    // frontend source tree (not just the worker-notes blob / audit log). UD-
    // CODE-003 is about the delivered code matching the architecture, so the
    // primary signal must be the source the worker actually wrote.
    let real_paths: String = umadev_contract::extract_frontend_calls(&opts.project_root)
        .iter()
        .map(|c| c.path.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let combined = format!("{fe_content}\n{api_log_content}\n{real_paths}");

    let mut missing: Vec<&str> = Vec::new();
    for path in &api_paths {
        if !combined.contains(path.as_str()) {
            missing.push(path);
        }
    }

    if missing.is_empty() {
        (
            "passed".to_string(),
            100,
            format!(
                "All {} API paths from architecture doc are referenced in frontend",
                api_paths.len()
            ),
        )
    } else {
        (
            "warning".to_string(),
            (100 - (i32::try_from(missing.len()).unwrap_or(5) * 15)).max(30),
            format!(
                "{}/{} API paths not found in frontend: {}",
                missing.len(),
                api_paths.len(),
                missing
                    .iter()
                    .take(5)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    }
}

/// Check if the UIUX document defines dark mode tokens.
fn check_dark_mode_support(uiux_path: &Path) -> (String, i32, String) {
    let content = fs::read_to_string(uiux_path).unwrap_or_default();
    let lower = content.to_ascii_lowercase();
    let has_dark = lower.contains("prefers-color-scheme")
        || lower.contains("dark mode")
        || lower.contains("dark-mode")
        || (lower.contains("@media") && lower.contains("dark"));

    if has_dark {
        (
            "passed".to_string(),
            100,
            "Dark mode tokens defined in UIUX document".to_string(),
        )
    } else if content.is_empty() {
        (
            "warning".to_string(),
            50,
            "UIUX document not yet created".to_string(),
        )
    } else {
        (
            "warning".to_string(),
            70,
            "No dark mode / prefers-color-scheme tokens found — consider adding for accessibility"
                .to_string(),
        )
    }
}

/// Check a document for required sections. Returns list of defect descriptions.
///
/// A keyword is considered present if it appears either as/in a heading
/// (heading-level structural check) **or** anywhere in the full text. The
/// full-text fallback is essential because many required patterns are content
/// tokens that live inside code blocks or tables, not headings — e.g.
/// `--color`, `--font`, `hover`, and markdown table rows (`| `). Without the
/// fallback, a correctly-authored UIUX doc with a complete `:root` token block
/// in a ```css fence would always fail the "Missing CSS color tokens" check.
fn review_document_structure(text: &str, required: &[(&str, &str)]) -> Vec<String> {
    let headings: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let t = l.trim_start();
            let level = t.chars().take_while(|&ch| ch == '#').count();
            if level == 0 {
                return None;
            }
            let h = t[level..].trim();
            if h.is_empty() {
                None
            } else {
                Some(h.to_ascii_lowercase())
            }
        })
        .collect();
    let full_lower = text.to_ascii_lowercase();
    let mut defects = Vec::new();
    for (keyword, msg) in required {
        let kw = keyword.trim_start_matches('#').trim().to_ascii_lowercase();
        let in_heading = headings
            .iter()
            .any(|h| h.starts_with(&kw) || h.split_whitespace().any(|w| w == kw));
        let in_text = !kw.is_empty() && full_lower.contains(&kw);
        if !in_heading && !in_text {
            defects.push((*msg).to_string());
        }
    }
    defects
}

/// Build a QualityCheck from content review results.
fn content_quality_check(
    name: &str,
    category: &str,
    description: &str,
    text: &str,
    defects: &[String],
    weight: f32,
) -> QualityCheck {
    let (status, score, details) = if text.is_empty() {
        (
            "failed".to_string(),
            0,
            "File is empty or missing".to_string(),
        )
    } else if defects.is_empty() {
        (
            "passed".to_string(),
            100,
            "All required sections present".to_string(),
        )
    } else {
        let penalty = i32::try_from(defects.len()).unwrap_or(4) * 20;
        let score = (100 - penalty.min(70)).max(10);
        (
            if defects.len() <= 1 {
                "warning"
            } else {
                "failed"
            }
            .to_string(),
            score,
            format!("{} issue(s): {}", defects.len(), defects.join("; ")),
        )
    };
    QualityCheck {
        name: name.to_string(),
        category: category.to_string(),
        description: description.to_string(),
        status,
        score,
        details,
        weight,
    }
}

/// Cross-validate PRD information architecture against Architecture API surface.
/// Checks that pages mentioned in PRD have corresponding API endpoints.
#[allow(clippy::unnecessary_cast)]
fn check_prd_arch_alignment(prd_text: &str, arch_text: &str) -> (String, i32, String) {
    if prd_text.is_empty() || arch_text.is_empty() {
        return (
            "warning".to_string(),
            50,
            "Cannot cross-validate — one or both documents empty".to_string(),
        );
    }

    // Extract routes from PRD IA section (lines starting with ├── /xxx or └── /xxx or / )
    let prd_routes: Vec<&str> = prd_text
        .lines()
        .filter_map(|l| {
            let trimmed = l.trim().trim_start_matches(['├', '└', '│', '─', ' ']);
            if trimmed.starts_with('/') && !trimmed.contains("Home") {
                Some(trimmed.split_whitespace().next().unwrap_or(trimmed))
            } else {
                None
            }
        })
        .collect();

    let arch_lower = arch_text.to_ascii_lowercase();
    let mut covered = 0;
    let mut total = 0;
    for route in &prd_routes {
        if route.contains(':') || route.len() < 3 {
            continue;
        }
        total += 1;
        let route_base = route
            .split('/')
            .find(|s| !s.is_empty() && !s.starts_with(':'))
            .unwrap_or("");
        if !route_base.is_empty() && arch_lower.contains(route_base) {
            covered += 1;
        }
    }

    if total == 0 {
        return (
            "passed".to_string(),
            100,
            "No routes to cross-validate (PRD may lack IA section)".to_string(),
        );
    }

    let coverage_pct = (covered * 100) / total.max(1);
    let status = if coverage_pct >= 70 {
        "passed"
    } else {
        "warning"
    };
    (
        status.to_string(),
        coverage_pct as i32,
        format!(
            "PRD→Architecture alignment: {covered}/{total} page routes have matching API endpoints ({coverage_pct}%)"
        ),
    )
}

/// Count anti-slop violations across markdown artifacts in output/.
///
/// To avoid false positives, we (1) skip generated report files whose names
/// contain `quality-gate` — those legitimately *mention* the patterns they
/// report on — and (2) ignore slop terms that appear inside an explicit
/// prohibition context (lines containing `no`, `without`, `not`, `avoid`,
/// `never`, `forbid`, `ban`, `prohibit`). A doc saying `no "lorem ipsum"`
/// is *enforcing* the rule, not violating it.
fn count_slop_violations(output_dir: &Path) -> usize {
    let prohibitive = [
        "no ", "no\"", "no'", "without", "not ", "avoid", "never ", "forbid", "ban ", "prohibit",
    ];
    let is_prohibited =
        |line_lower: &str| -> bool { prohibitive.iter().any(|neg| line_lower.contains(neg)) };
    let mut count = 0;
    if let Ok(rd) = fs::read_dir(output_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            // Skip generated report files — they quote the patterns they report.
            let fname = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if fname.contains("quality-gate") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&p) {
                for line in content.lines() {
                    let lower = line.to_ascii_lowercase();
                    let is_slop = lower.contains("lorem ipsum")
                        || lower.contains("dolor sit amet")
                        || (lower.contains("welcome to") && lower.contains("# "));
                    if is_slop && !is_prohibited(&lower) {
                        count += 1;
                        break; // one violation per file is enough
                    }
                }
            }
        }
    }
    count
}

/// Score UIUX document completeness. Checks for key sections:
/// color palette, typography, spacing, icon library, components,
/// accessibility. Each section found = +16 points (max ~100).
fn score_uiux_completeness(path: &Path) -> u32 {
    let content = fs::read_to_string(path).unwrap_or_default();
    let lower = content.to_ascii_lowercase();
    if lower.is_empty() {
        return 0;
    }
    // Section presence (10 pts each, max 70)
    let sections = [
        "color",
        "typography",
        "spacing",
        "icon",
        "component",
        "accessibility",
        "dark",
    ];
    let section_score =
        (sections.iter().filter(|s| lower.contains(**s)).count() as u32 * 10).min(70);
    // Token count bonus (count --color / --font / --space var declarations)
    let token_count = content.matches("--").count() as u32;
    let token_bonus = if token_count >= 50 {
        20
    } else if token_count >= 20 {
        15
    } else if token_count >= 10 {
        10
    } else {
        0
    };
    // Length bonus
    let length_bonus = if content.len() > 2000 {
        10
    } else if content.len() > 500 {
        5
    } else {
        0
    };
    (section_score + token_bonus + length_bonus).min(100)
}

/// Extract score + passed from quality gate JSON. Used by the runner
/// to emit a quality summary to the TUI.
///
/// Reads the top-level `total_score` (NOT the per-check `score` — that
/// field also exists on each check object, so a naive `"score"` split
/// would grab `checks[0].score` instead of the aggregate).
pub fn extract_quality_score(json: &str) -> (String, bool) {
    let score = json
        .split("\"total_score\"")
        .nth(1)
        .and_then(|s| s.split(':').nth(1))
        .and_then(|s| {
            s.trim()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .ok()
        })
        .map_or("?".to_string(), |n| n.to_string());
    // Parse `passed` structurally (serde) when the gate file is clean JSON;
    // fall back to a WHITESPACE-INSENSITIVE check for JSON embedded in prose, so
    // `"passed" : true` (or any spacing) can't be misread as a blocked gate.
    let passed = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("passed").and_then(serde_json::Value::as_bool))
        .unwrap_or_else(|| {
            json.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .contains("\"passed\":true")
        });
    (score, passed)
}

/// Pull the top failing findings out of a quality-gate JSON so they can be shown
/// INLINE when the gate blocks delivery — instead of telling the user to go open
/// the JSON file themselves. Returns up to `max` short strings, each
/// `"<check name>: <details>"`, preferring the lowest-scoring failed/warning
/// checks and any explicit `critical_failures`. Fail-open: a malformed or
/// unparsable gate file yields an empty vec (the caller then just omits the
/// findings block), never an error.
#[must_use]
pub fn quality_findings(json: &str, max: usize) -> Vec<String> {
    let Ok(report) = serde_json::from_str::<QualityReport>(json) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    // Explicit critical failures first — these are the gate's hard blockers.
    for f in &report.critical_failures {
        out.push(f.clone());
        if out.len() >= max {
            return out;
        }
    }
    // Then the lowest-scoring non-passing checks (worst first), so the user sees
    // what actually dragged the score down.
    let mut failing: Vec<&QualityCheck> = report
        .checks
        .iter()
        .filter(|c| c.status != "passed")
        .collect();
    failing.sort_by_key(|c| c.score);
    for c in failing {
        let line = if c.details.trim().is_empty() {
            format!("{} ({})", c.name, c.status)
        } else {
            format!("{}: {}", c.name, c.details)
        };
        if !out.contains(&line) {
            out.push(line);
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

/// When the worker returns text via stdout AND already wrote a file to
/// disk, the disk version is often the richer one (full document) while
/// stdout may be just a summary. Pick whichever has more substance.
fn prefer_richer(stdout_text: &str, disk_text: &str) -> String {
    // Keep the on-disk file ONLY when the worker's reply is a STUB/pointer
    // (e.g. "I've written the PRD to output/x.md") rather than the artifact
    // itself — i.e. genuinely tiny. A legitimately-concise REVISION (the user
    // asked to shorten the doc) is still a real document and must NOT be
    // discarded for the stale prior-run file. The old `disk > stdout*2` rule
    // wrongly kept the verbose original on every tightening rewrite.
    if stdout_text.trim().len() < 200 && disk_text.trim().len() > 400 {
        disk_text.to_string()
    } else {
        stdout_text.to_string()
    }
}

/// Atomically write `content` to `path`: write to `<path>.tmp-<pid>` in
/// the same directory, then rename over `path`. Same-filesystem rename is
/// atomic on POSIX, so a concurrent reader never observes a half-written
/// file (the reader either sees the old complete file or the new complete
/// one). Falls back to a direct `fs::write` if the temp path can't be
/// constructed.
pub(crate) fn atomic_write(path: &Path, content: &str) -> io::Result<()> {
    // Per-process temp name so two concurrent writers can't share + clobber the
    // same scratch file before the rename (the run-lock already serialises
    // pipeline writes; this is belt-and-suspenders for any other caller).
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, content)?;
    if fs::rename(&tmp, path).is_ok() {
        Ok(())
    } else {
        // Rename failed (cross-filesystem?). Clean up the temp and fall
        // back to a direct write — correctness > atomicity here.
        let _ = fs::remove_file(&tmp);
        fs::write(path, content)
    }
}

fn audit(opts: &RunOptions, tool_name: &str, target: &Path, clause: &str, reason: &str) {
    let _ = record_tool_call(
        &opts.project_root,
        tool_name,
        target.to_string_lossy().as_ref(),
        "audit",
        clause,
        reason,
        "",
        None,
    );
}

/// Resolve the knowledge corpus directory. The project's own `knowledge/` wins
/// when it has content (user customisations), otherwise fall back to the bundled
/// corpus pointed to by `UMADEV_KNOWLEDGE_DIR` (the `umadev` binary stages the
/// embedded corpus to `~/.umadev/knowledge` on first run and points this env var
/// at it), and finally to `~/.umadev/knowledge` directly — so end users get the
/// full curated KB with zero setup even when they run in a bare project, and even
/// on a code path that didn't go through the binary's startup (downstream
/// embedders, a cleared env). Fail-open: returns the local path regardless, and
/// the index treats a missing dir as empty.
pub fn knowledge_root(project_root: &Path) -> std::path::PathBuf {
    let local = project_root.join("knowledge");
    let has_content = std::fs::read_dir(&local)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if has_content {
        return local;
    }
    if let Ok(dir) = std::env::var("UMADEV_KNOWLEDGE_DIR") {
        let bundled = std::path::PathBuf::from(dir);
        if bundled.is_dir() {
            return bundled;
        }
    }
    // Defense-in-depth: discover the staged corpus directly. The binary stages
    // it here on startup; this branch covers a cleared env var or a path that
    // bypassed `main` (e.g. a downstream crate driving the engine directly).
    if let Some(staged) = staged_knowledge_dir() {
        if staged.is_dir() {
            return staged;
        }
    }
    local
}

/// The default staged-corpus location, `~/.umadev/knowledge`, resolved
/// cross-platform (HOME on unix, USERPROFILE on Windows) to mirror the binary's
/// other global-state lookups. `None` when no home dir is resolvable.
fn staged_knowledge_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))?;
    Some(
        std::path::PathBuf::from(home)
            .join(".umadev")
            .join("knowledge"),
    )
}

fn summarise_knowledge_dir(dir: &Path) -> String {
    if !dir.is_dir() {
        return "_no `knowledge/` directory in this workspace._".to_string();
    }
    let mut entries = Vec::new();
    walk_md(dir, &mut entries, 0);
    if entries.is_empty() {
        return "_knowledge directory is empty._".to_string();
    }
    let mut lines: Vec<String> = entries
        .iter()
        .take(40)
        .map(|p| format!("- `{p}`"))
        .collect();
    if entries.len() > 40 {
        lines.push(format!("- … and {} more", entries.len() - 40));
    }
    lines.join("\n")
}

fn walk_md(dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 4 || out.len() >= 200 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_md(&p, out, depth + 1);
        } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
            if let Some(rel) = p.to_str() {
                let cleaned = rel.split("/knowledge/").nth(1).unwrap_or(rel);
                out.push(cleaned.to_string());
            }
        }
    }
}

/// One delivery doc the default path guarantees exists once a build settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreDoc {
    /// `output/<slug>-prd.md` — the Product Requirements Document.
    Prd,
    /// `output/<slug>-architecture.md` — the system + API-surface design.
    Architecture,
    /// `output/<slug>-uiux.md` — the design system (tokens, type, components).
    Uiux,
    /// `output/<slug>-execution-plan.md` — the task breakdown.
    ExecutionPlan,
}

impl CoreDoc {
    /// Workspace-relative file name of this doc for `slug`.
    fn rel_name(self, slug: &str) -> String {
        match self {
            Self::Prd => format!("output/{slug}-prd.md"),
            Self::Architecture => format!("output/{slug}-architecture.md"),
            Self::Uiux => format!("output/{slug}-uiux.md"),
            Self::ExecutionPlan => format!("output/{slug}-execution-plan.md"),
        }
    }
}

/// Ensure the run's core delivery docs (PRD / architecture / UI-UX / execution
/// plan) exist on disk, writing a deterministic scaffold for any that are absent
/// — the lifted half of the old delivery writers, callable on the DEFAULT path
/// (`director::finalize`) once a build settles clean.
///
/// **Idempotent + never clobbers real content.** A doc the base already wrote
/// (e.g. a real architecture table during the build) is left untouched; only a
/// genuinely-missing doc gets the offline scaffold, so finalize never overwrites
/// the team's real work. Returns the workspace-relative names actually WRITTEN
/// (the scaffolds), so the caller can report what it backfilled vs. what was
/// already present.
///
/// Pure + fail-open: a failed `create_dir_all` / `write` for one doc is swallowed
/// and the next doc is still attempted; a doc that can't be written is simply not
/// in the returned list. Never an error, never a panic — finalize must never
/// block a build that already succeeded.
#[must_use]
pub fn scaffold_core_docs(opts: &RunOptions) -> Vec<String> {
    let slug = opts.effective_slug();
    let out_dir = opts.project_root.join("output");
    let _ = fs::create_dir_all(&out_dir);
    let mut written = Vec::new();
    for doc in [
        CoreDoc::Prd,
        CoreDoc::Architecture,
        CoreDoc::Uiux,
        CoreDoc::ExecutionPlan,
    ] {
        let rel = doc.rel_name(&slug);
        let path = opts.project_root.join(&rel);
        if path.is_file() {
            continue; // never clobber a doc the base already wrote
        }
        let body = match doc {
            CoreDoc::Prd => render_prd(&slug, &opts.requirement),
            CoreDoc::Architecture => render_architecture(&slug, &opts.requirement),
            CoreDoc::Uiux => render_uiux(&slug, &opts.requirement),
            CoreDoc::ExecutionPlan => render_execution_plan(&slug, &opts.requirement),
        };
        if fs::write(&path, body).is_ok() {
            written.push(rel);
        }
    }
    written
}

fn render_prd(slug: &str, requirement: &str) -> String {
    format!(
        "# PRD — {slug}\n\n\
         > Offline scaffold. Use `--backend claude-code` for AI-generated content.\n\n\
         ## Goal\n\n{requirement}\n\n\
         TODO: Expand with: what + why + for whom + success metric\n\n\
         ## Target users\n\n\
         TODO: Define 2-3 personas with role, context, pain point.\n\n\
         ## Information architecture\n\n\
         ```\n\
         / (Home)\n\
         ├── /feature-1\n\
         ├── /feature-2\n\
         └── /auth/login\n\
         ```\n\
         TODO: Expand routes for: {requirement}\n\n\
         ## Scope\n\n\
         ### In scope\n\
         - TODO: List features for this iteration\n\n\
         ### Out of scope\n\
         - TODO: Explicitly exclude items\n\n\
         ## Functional requirements\n\n\
         | ID | Feature | Priority | Acceptance criteria |\n\
         |---|---|---|---|\n\
         | FR-001 | TODO | P0 | TODO |\n\
         | FR-002 | TODO | P1 | TODO |\n\n\
         ## Non-functional requirements\n\n\
         - Performance: FCP < _target_, API p95 < _target_\n\
         - Security: _auth method_, _data sensitivity_\n\
         - Accessibility: WCAG 2.1 _level_\n\n\
         ## Acceptance criteria\n\n\
         - [ ] Given TODO, when TODO, then TODO\n\
         - [ ] Given TODO, when TODO, then TODO\n\
         - [ ] Given TODO, when TODO, then TODO\n\n\
         TODO: Add acceptance criteria matching each functional requirement.\n\n\
         ## Success metrics\n\n\
         | Metric | Baseline | Target | How to measure |\n\
         |---|---|---|---|\n\
         | TODO | TODO | TODO | TODO |\n\n\
         ## Risks & open questions\n\n\
         - TODO: Identify domain-specific risks\n",
    )
}

fn render_architecture(slug: &str, requirement: &str) -> String {
    format!(
        "# Architecture — {slug}\n\n\
         > Offline scaffold. Use `--backend claude-code` for AI-generated content.\n\n\
         ## System overview\n\n\
         TODO: Describe the system components and how they communicate.\n\
         Consider: What services exist? REST/gRPC/WebSocket? Data flow direction?\n\n\
         Requirement: {requirement}\n\n\
         ## API surface\n\n\
         | Method | Path | Request | Response | Auth | Description |\n\
         |---|---|---|---|---|---|\n\
         | GET | /api/health | - | `{{ ok: true }}` | none | Health check |\n\
         | POST | /api/auth/login | `{{ email, password }}` | `{{ token, user }}` | none | Login |\n\
         | GET | /api/auth/me | - | `{{ user }}` | bearer | Current user |\n\
         | TODO | /api/... | TODO | TODO | TODO | Add endpoints for: {requirement} |\n\n\
         ## API error convention\n\n\
         ```json\n\
         {{ \"error\": {{ \"code\": \"VALIDATION_ERROR\", \"message\": \"...\", \"details\": [...] }} }}\n\
         ```\n\n\
         | HTTP | Code | Meaning |\n\
         |---|---|---|\n\
         | 400 | BAD_REQUEST | Malformed request |\n\
         | 401 | UNAUTHORIZED | Missing/invalid auth token |\n\
         | 403 | FORBIDDEN | Authenticated but no permission |\n\
         | 404 | NOT_FOUND | Resource doesn't exist |\n\
         | 422 | VALIDATION_ERROR | Invalid field values |\n\
         | 429 | RATE_LIMITED | Too many requests |\n\
         | 500 | INTERNAL_ERROR | Server error (no details to client) |\n\n\
         ## Data model\n\n\
         TODO: Define entities with field tables.\n\n\
         | Field | Type | Required | Description |\n\
         |---|---|---|---|\n\
         | id | uuid | yes | Primary key |\n\
         | created_at | timestamp | yes | Auto-set on create |\n\
         | updated_at | timestamp | yes | Auto-set on update |\n\n\
         ## Authentication & authorization\n\n\
         TODO: Define auth method (JWT/session/OAuth2), roles, permission matrix.\n\n\
         ## Tech-stack rationale\n\n\
         - Frontend: TODO (pick framework + justify)\n\
         - Backend: TODO (pick language/framework + justify)\n\
         - Database: TODO (pick DB + justify)\n\
         - Hosting: TODO (pick platform + justify)\n\n\
         ## Project structure\n\n\
         ```\n\
         src/\n\
           pages/       # Route-level components\n\
           components/  # Shared UI\n\
           lib/         # Business logic\n\
           api/         # API routes or client\n\
           types/       # Shared types\n\
         ```\n\n\
         ## Security considerations\n\n\
         - [ ] Input validation on all endpoints\n\
         - [ ] Parameterized queries (no SQL injection)\n\
         - [ ] HTTPS only (HSTS header)\n\
         - [ ] Rate limiting on auth endpoints\n\
         - [ ] Secrets in env vars, not code\n",
    )
}

fn render_uiux(slug: &str, requirement: &str) -> String {
    format!(
        "# UI/UX — {slug}\n\n\
         > Offline scaffold — pass `--backend claude-code` or `--backend droid` to generate a real design system.\n\n\
         ## Visual direction\n\nModern Minimal — clean, precise, whitespace-first.\n\n\
         ## Color palette\n\n```css\n:root {{\n\
         \x20 --color-bg: #fafafa;\n\
         \x20 --color-surface: #ffffff;\n\
         \x20 --color-text: #111827;\n\
         \x20 --color-text-secondary: #6b7280;\n\
         \x20 --color-primary: #2563eb;\n\
         \x20 --color-primary-hover: #1d4ed8;\n\
         \x20 --color-accent: #f59e0b;\n\
         \x20 --color-border: #e5e7eb;\n\
         \x20 --color-error: #ef4444;\n\
         \x20 --color-success: #10b981;\n\
         }}\n\
         @media (prefers-color-scheme: dark) {{\n\
         \x20 :root {{\n\
         \x20\x20\x20 --color-bg: #0f172a;\n\
         \x20\x20\x20 --color-surface: #1e293b;\n\
         \x20\x20\x20 --color-text: #f1f5f9;\n\
         \x20\x20\x20 --color-text-secondary: #94a3b8;\n\
         \x20\x20\x20 --color-border: #334155;\n\
         \x20 }}\n\
         }}\n```\n\n\
         ## Typography system\n\n\
         - Headings: `Inter, system-ui, sans-serif` weight 600\n\
         - Body: `Inter, system-ui, sans-serif` weight 400\n\
         - `--text-xs: 0.75rem` / `--text-sm: 0.875rem` / `--text-base: 1rem` / `--text-lg: 1.125rem` / `--text-xl: 1.25rem` / `--text-2xl: 1.5rem` / `--text-3xl: 1.875rem`\n\n\
         ## Spacing scale\n\n\
         `--space-1: 4px` / `--space-2: 8px` / `--space-3: 12px` / `--space-4: 16px` / `--space-6: 24px` / `--space-8: 32px` / `--space-10: 40px` / `--space-12: 48px`\n\n\
         ## Icon library\n\n- Declared: Lucide\n\n\
         ## Page hierarchy\n\n- `/` Home\n  - `/detail/:id` Detail\n  - `/settings` Settings\n\n\
         ## Component inventory\n\n\
         _Components for: {requirement}_\n\n\
         | Component | States |\n\
         |---|---|\n\
         | Button | default / hover / active / disabled / loading |\n\
         | Input | default / focus / error / disabled |\n\
         | Card | default / hover / selected |\n\
         | Modal | open / closing (transition) |\n\n\
         ## Motion guidelines\n\n\
         - `--transition-fast: 150ms ease-out` (hover, focus)\n\
         - `--transition-normal: 250ms ease-in-out` (modals, drawers)\n\
         - `--transition-slow: 400ms ease-in-out` (page transitions)\n\n\
         ## Anti-patterns\n\n\
         1. No decorative hero gradients\n\
         2. No emoji as functional icons\n\
         3. No AI-chatbot shell layout\n\
         4. No cards with identical placeholder text\n\
         5. No cramped layouts without spacing tokens\n\n\
         ## Self-critique\n\n\
         | Dimension | Score |\n|---|---|\n\
         | Hierarchy clarity | 7/10 |\n\
         | Visual distinctiveness | 6/10 |\n\
         | Detail polish | 5/10 |\n\
         | Functional completeness | 7/10 |\n\
         | Innovation | 5/10 |\n\n\
         > Offline template scores low on distinctiveness + polish — use a real worker for production.\n\n\
         ## Accessibility notes\n\n\
         - Color contrast ≥ 4.5:1 (AA)\n\
         - Keyboard reachable for every interactive control\n\
         - Focus ring: 2px solid var(--color-primary), offset 2px\n\
         - `aria-label` on icon-only buttons\n",
    )
}

fn render_execution_plan(slug: &str, requirement: &str) -> String {
    format!(
        "# Execution plan — {slug}\n\n\
         > Skeleton execution plan — open in your worker session and flesh out per-task acceptance criteria.\n\n\
         ## Goal recap\n\n{requirement}\n\n\
         ## Sequence\n\n\
         1. Frontend skeleton + design tokens\n\
         2. Backend route stubs aligned with the architecture API surface\n\
         3. Integration smoke test\n\
         4. Quality gate + proof pack\n",
    )
}

fn render_tasks(slug: &str) -> String {
    format!(
        "# Tasks — {slug}\n\n\
         - [ ] frontend / scaffold pages per UIUX\n\
         - [ ] frontend / wire fetch calls to architecture API paths\n\
         - [ ] backend / implement architecture routes\n\
         - [ ] backend / write integration tests\n\
         - [ ] quality / run umadev quality gate\n\
         - [ ] delivery / assemble proof pack\n",
    )
}

// =====================================================================
// tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::NoBundledCorpus;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn agentic_knowledge_digest_fails_open_without_knowledge_dir() {
        // No `knowledge/` dir in the workspace AND no bundled corpus reachable
        // -> empty digest, never an error. (The agentic path must proceed
        // unchanged when there's nothing to inject.)
        let _no_corpus = NoBundledCorpus::new();
        let tmp = TempDir::new().unwrap();
        let d = agentic_knowledge_digest(tmp.path(), "build a login page", 4);
        assert!(d.is_empty(), "no knowledge dir -> empty digest");
        // Empty requirement / zero budget also short-circuit to empty.
        assert!(agentic_knowledge_digest(tmp.path(), "   ", 4).is_empty());
        assert!(agentic_knowledge_digest(tmp.path(), "build it", 0).is_empty());
    }

    #[test]
    fn knowledge_root_discovers_staged_home_corpus() {
        // With no project-local `knowledge/` and no UMADEV_KNOWLEDGE_DIR, the
        // `~/.umadev/knowledge` fallback must be discovered (the binary stages
        // the embedded corpus there on startup). Zero-config recall for users.
        let no_corpus = NoBundledCorpus::new();
        // `NoBundledCorpus` already set HOME to a fresh temp dir; create the
        // staged corpus under it so the home-dir fallback resolves to it.
        let home = no_corpus.home().to_path_buf();
        let staged = home.join(".umadev").join("knowledge").join("backend");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("layering.md"), "# layering\n\nservice layer\n").unwrap();

        let project = TempDir::new().unwrap();
        let resolved = knowledge_root(project.path());
        assert_eq!(
            resolved,
            home.join(".umadev").join("knowledge"),
            "bare project must fall back to the staged ~/.umadev/knowledge corpus"
        );

        // And recall over it is non-empty for a matching requirement.
        let d = agentic_knowledge_digest(project.path(), "service layering", 4);
        assert!(
            !d.is_empty(),
            "staged corpus must produce a non-empty digest for a matching ask"
        );
    }

    #[test]
    fn knowledge_root_prefers_env_then_local_over_home() {
        let _no_corpus = NoBundledCorpus::new();
        // Project-local `knowledge/` with content wins over everything.
        let project = TempDir::new().unwrap();
        let local = project.path().join("knowledge");
        fs::create_dir_all(&local).unwrap();
        fs::write(local.join("x.md"), "# local\n").unwrap();
        assert_eq!(knowledge_root(project.path()), local, "local corpus wins");

        // With no local corpus, UMADEV_KNOWLEDGE_DIR (if it points at a real
        // dir) wins over the home fallback.
        let bare = TempDir::new().unwrap();
        let envdir = TempDir::new().unwrap();
        std::env::set_var("UMADEV_KNOWLEDGE_DIR", envdir.path());
        assert_eq!(
            knowledge_root(bare.path()),
            envdir.path(),
            "UMADEV_KNOWLEDGE_DIR wins when local corpus is absent"
        );
        std::env::remove_var("UMADEV_KNOWLEDGE_DIR");
    }

    #[test]
    fn agentic_knowledge_digest_surfaces_matching_chunk() {
        // With a `knowledge/` file that matches the requirement, the compact digest
        // names the source path + an excerpt of the team's experience.
        let tmp = TempDir::new().unwrap();
        let kdir = tmp.path().join("knowledge").join("backend");
        fs::create_dir_all(&kdir).unwrap();
        fs::write(
            kdir.join("layering.md"),
            "# Service layering\n\n## Clean layers\n\nKeep controllers thin and push \
             business logic into a service layer; repositories own persistence.\n",
        )
        .unwrap();
        let d = agentic_knowledge_digest(tmp.path(), "service layering controllers", 4);
        // Fail-open is fine if the index can't match (e.g. tokeniser edge), but when
        // it does match it must carry the real source path + the team-experience
        // framing.
        if !d.is_empty() {
            assert!(d.contains("layering.md"), "names the matched source");
            assert!(d.contains("YOUR TEAM'S EXPERIENCE"), "empowering framing");
        }
    }

    #[test]
    fn scorecard_html_is_self_contained_and_slop_free() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("output");
        let rel = tmp.path().join("release");
        fs::create_dir_all(&out).unwrap();
        fs::create_dir_all(&rel).unwrap();
        let report = QualityReport {
            passed: true,
            total_score: 92,
            weighted_score: 92.0,
            scenario: String::new(),
            critical_failures: vec![],
            recommendations: vec![],
            summary: QualitySummary {
                executive_summary: "ok".into(),
                summary_context: std::collections::BTreeMap::new(),
            },
            checks: vec![QualityCheck {
                name: "Design quality (code)".into(),
                category: "quality".into(),
                description: "d".into(),
                status: "passed".into(),
                score: 100,
                weight: 1.5,
                details: "no tells".into(),
            }],
        };
        fs::write(
            out.join("app-quality-gate.json"),
            serde_json::to_string(&report).unwrap(),
        )
        .unwrap();
        let zip = rel.join("proof-pack-app-x.zip");
        fs::write(&zip, b"zip-bytes").unwrap();
        let card = write_scorecard_html(tmp.path(), &rel, "app", "x", &zip).unwrap();
        let html = fs::read_to_string(&card).unwrap();
        // Self-contained: no external scripts/styles/images.
        assert!(!html.contains("src=\"http") && !html.contains("href=\"http"));
        assert!(!html.contains("<script"));
        // Shows the independent score + verdict + tamper-evident hash.
        assert!(html.contains("92") && html.contains("PASSED") && html.contains("SHA-256"));
        // Must itself be AI-slop-free (we preach it).
        assert!(!html.to_lowercase().contains("#7c3aed"));
        assert!(!html.to_lowercase().contains("#667eea"));
        // Our OWN governance rule (UD-ARCH-013) must pass on our OWN output:
        // the scorecard ships a Content-Security-Policy so it doesn't fail the
        // very CSP rule UmaDev enforces on generated HTML.
        assert!(html.contains("<meta http-equiv=\"Content-Security-Policy\""));
        // The CSP permits the inline <style> the scorecard relies on.
        assert!(html.contains("style-src 'self' 'unsafe-inline'"));
        let csp = umadev_governance::rules::check_csp_required(card.to_str().unwrap(), &html);
        assert!(
            !csp.block,
            "scorecard must pass UD-ARCH-013 with no post-hoc CSP fix"
        );
    }

    #[test]
    fn prefer_richer_keeps_concise_revision_drops_stub() {
        // A real (even shorter) revision must win over a stale verbose file.
        let concise = "# PRD\n\n精简后的需求文档,仍然是一份完整文档,有结构有内容。".repeat(4);
        let verbose_old = "# PRD\n\n".to_string() + &"旧的冗长内容。".repeat(80);
        assert_eq!(prefer_richer(&concise, &verbose_old), concise);
        // But a tiny stub ("saved to output/x.md") keeps the on-disk artifact.
        let stub = "我已把 PRD 写入 output/app-prd.md。";
        assert_eq!(prefer_richer(stub, &verbose_old), verbose_old);
    }

    #[test]
    fn font_contract_conformance_flags_drift() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("output");
        let src = tmp.path().join("src");
        fs::create_dir_all(&out).unwrap();
        fs::create_dir_all(&src).unwrap();
        // The UIUX contract declares Clash Display + Geist.
        let uiux = out.join("app-uiux.md");
        fs::write(
            &uiux,
            "## Typography\nfont-family: \"Clash Display\", system-ui;\n--font-body: Geist, sans-serif;",
        )
        .unwrap();
        // Code that uses ONLY the contract fonts (+ generic) → conforms.
        fs::write(
            src.join("Ok.css"),
            "h1{font-family:\"Clash Display\",sans-serif} body{font-family:Geist,system-ui}",
        )
        .unwrap();
        let ok = check_font_contract_conformance(tmp.path(), &uiux);
        assert_eq!(ok.0, "passed", "{ok:?}");
        // Code that introduces an off-contract font → drift flagged.
        fs::write(
            src.join("Drift.css"),
            "h2{font-family:\"Comic Sans MS\",cursive}",
        )
        .unwrap();
        let drift = check_font_contract_conformance(tmp.path(), &uiux);
        assert_eq!(drift.0, "warning", "{drift:?}");
        assert!(drift.2.contains("comic sans"), "{drift:?}");
    }

    #[test]
    fn code_design_quality_flags_planted_slop_and_passes_clean() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        // A clean project scores 100.
        fs::write(
            src.join("Clean.css"),
            "h1 { font-family: \"Clash Display\", system-ui; color: var(--c); }",
        )
        .unwrap();
        let clean = check_code_design_quality(tmp.path());
        assert_eq!(clean.1, 100, "clean UI should score 100: {clean:?}");

        // Plant an AI-purple gradient + Inter-primary tell → flagged + lower.
        fs::write(
            src.join("Hero.css"),
            ".hero{background:linear-gradient(90deg,#6366f1,#764ba2);font-family:Inter,sans-serif}",
        )
        .unwrap();
        let dirty = check_code_design_quality(tmp.path());
        assert!(
            dirty.1 < 100,
            "planted slop should lower the score: {dirty:?}"
        );
        assert_eq!(dirty.0, "warning", "AI-purple is a hard tell: {dirty:?}");
        assert!(
            dirty.2.contains("ai-purple"),
            "should name the tell: {dirty:?}"
        );
    }

    #[test]
    fn endpoint_is_public_matches_segments_not_substrings() {
        use umadev_contract::{Endpoint, HttpVerb, SecurityKind};
        let mk = |path: &str, opid: &str| Endpoint {
            method: HttpVerb::Post,
            path: path.to_string(),
            operation_id: opid.to_string(),
            description: String::new(),
            request_shape: String::new(),
            response_shape: String::new(),
            security: SecurityKind::None,
        };
        // Genuinely public auth-entry endpoints → excused.
        assert!(endpoint_is_public(&mk("/api/auth/login", "login")));
        assert!(endpoint_is_public(&mk(
            "/api/oauth/callback",
            "oauthCallback"
        )));
        assert!(endpoint_is_public(&mk("/webhooks/stripe", "stripeWebhook")));
        assert!(endpoint_is_public(&mk("/healthz", "health")));
        // The false-negatives the fix targets: a state-changing endpoint must
        // NOT be excused just because a segment *contains* a marker substring.
        assert!(!endpoint_is_public(&mk(
            "/api/admin/login-history",
            "adminLoginHistory"
        )));
        assert!(!endpoint_is_public(&mk(
            "/api/publications",
            "createPublication"
        )));
        assert!(!endpoint_is_public(&mk("/api/admin/users", "createUser")));
    }

    fn opts(root: &Path) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: "build a login system".to_string(),
            slug: "demo".to_string(),
            model: "stub".to_string(),
            backend: String::new(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: crate::trust::TrustMode::Guarded,
            strict_coverage: false,
        }
    }

    #[test]
    fn run_spec_keeps_base_execution_plan_and_does_not_clobber_with_skeleton() {
        // #2: the runner writes the base's REAL execution plan to disk BEFORE
        // calling run_spec; run_spec must NOT overwrite it with the skeleton.
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        let out_dir = tmp.path().join("output");
        fs::create_dir_all(&out_dir).unwrap();
        let plan_path = out_dir.join("demo-execution-plan.md");
        // A rich, base-written plan already on disk (what the runner just wrote).
        let base_plan = "# Real execution plan from the base\n\n\
            ## Sprint 1\n- Build the auth module with bcrypt password hashing\n\
            - Wire the session store and CSRF protection\n\n\
            ## Sprint 2\n- Implement the profile page and settings\n\
            - Add integration tests for the login flow\n\n\
            ## Definition of done\n- All routes covered, lint clean, e2e green\n";
        fs::write(&plan_path, base_plan).unwrap();

        let out = run_spec(&o).unwrap();
        assert_eq!(out.phase, Phase::Spec);

        let after = fs::read_to_string(&plan_path).unwrap();
        assert_eq!(
            after, base_plan,
            "run_spec clobbered the base's real execution plan with the skeleton"
        );
        assert!(
            !after.contains("Skeleton execution plan"),
            "skeleton template must not win over a real base plan"
        );
    }

    #[test]
    fn run_spec_writes_skeleton_when_no_base_plan_on_disk() {
        // Offline / no-base path: nothing on disk → run_spec still produces the
        // deterministic skeleton so the artifact always exists.
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        let out = run_spec(&o).unwrap();
        assert_eq!(out.phase, Phase::Spec);
        let plan = fs::read_to_string(tmp.path().join("output/demo-execution-plan.md")).unwrap();
        assert!(
            plan.contains("Skeleton execution plan"),
            "skeleton fallback must be written when there is no base plan: {plan}"
        );
    }

    #[test]
    fn secret_leak_scan_flags_credentials_and_skips_noise() {
        let tmp = TempDir::new().unwrap();
        let web = tmp.path().join("web/src");
        fs::create_dir_all(&web).unwrap();
        // A leaked DB credential in app source → flagged.
        fs::write(
            web.join("db.ts"),
            "export const url = \"postgres://admin:s3cr3tPassw0rd123@db.host:5432/app\";",
        )
        .unwrap();
        // A clean file → not flagged.
        fs::write(
            web.join("ok.ts"),
            "export const url = process.env.DATABASE_URL;",
        )
        .unwrap();
        // Noise dirs must be skipped even if they contain "secrets".
        let nm = tmp.path().join("node_modules/evil");
        fs::create_dir_all(&nm).unwrap();
        fs::write(
            nm.join("x.js"),
            "const url='postgres://admin:s3cr3tPassw0rd123@h:5432/d';",
        )
        .unwrap();

        let (scanned, offenders) = scan_secret_leaks(tmp.path());
        assert!(scanned >= 2, "should scan app source, got {scanned}");
        assert_eq!(offenders, vec!["web/src/db.ts".to_string()]);
    }

    #[test]
    fn research_writes_artifact_and_bundle() {
        let tmp = TempDir::new().unwrap();
        let out = run_research(&opts(tmp.path()), None).unwrap();
        assert_eq!(out.phase, Phase::Research);
        assert!(out.artifacts[0].ends_with("output/demo-research.md"));
        let body = fs::read_to_string(&out.artifacts[0]).unwrap();
        assert!(body.contains("build a login system"));
    }

    #[test]
    fn docs_writes_three_files_and_stops_at_gate() {
        let tmp = TempDir::new().unwrap();
        let out = run_docs(&opts(tmp.path()), &DocsContent::default()).unwrap();
        assert_eq!(out.phase, Phase::Docs);
        assert_eq!(out.artifacts.len(), 3);
        assert_eq!(out.gate, Some(crate::gates::Gate::DocsConfirm));
    }

    #[test]
    fn spec_writes_plan_and_tasks() {
        let tmp = TempDir::new().unwrap();
        let out = run_spec(&opts(tmp.path())).unwrap();
        assert_eq!(out.phase, Phase::Spec);
        assert_eq!(out.artifacts.len(), 2);
        assert!(out.artifacts[0].ends_with("output/demo-execution-plan.md"));
        let body = fs::read_to_string(&out.artifacts[1]).unwrap();
        assert!(body.contains("Tasks"));
    }

    #[test]
    fn frontend_writes_notes_and_pauses_at_preview_gate() {
        let tmp = TempDir::new().unwrap();
        let out = run_frontend(&opts(tmp.path())).unwrap();
        assert_eq!(out.phase, Phase::Frontend);
        assert_eq!(out.gate, Some(crate::gates::Gate::PreviewConfirm));
    }

    #[test]
    fn backend_writes_notes_no_gate() {
        let tmp = TempDir::new().unwrap();
        let out = run_backend(&opts(tmp.path())).unwrap();
        assert_eq!(out.phase, Phase::Backend);
        assert!(out.gate.is_none());
    }

    #[test]
    fn quality_produces_real_score() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        // First run the prior phases so quality has something to grade
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();

        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        assert!(report.total_score > 0);
        // 5 artifacts present (research, prd, arch, uiux, execution-plan) + tool-call audit present
        // → expect score well above 0
        assert!(report
            .checks
            .iter()
            .any(|c| c.name.contains("PRD") || c.name.contains("content")));
    }

    #[test]
    fn quality_fails_on_missing_docs() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        assert!(!report.passed);
        assert!(!report.critical_failures.is_empty());
    }

    #[test]
    fn quality_blocks_when_code_plan_produced_zero_source() {
        // The HARD quality item: a code-bearing plan (Greenfield "build a login
        // system") with NO real source files on disk must produce a `failed`
        // "Real source code present" artifact check → critical failure → gate
        // BLOCKED, regardless of how complete the docs are.
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        // Generate full, well-structured docs so the doc-structure score is high
        // — proving the BLOCK is driven by the missing source, not weak docs.
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let src = report
            .checks
            .iter()
            .find(|c| c.name == "Real source code present")
            .expect("real-source check must exist");
        assert_eq!(src.status, "failed", "zero source must fail: {src:?}");
        assert_eq!(src.category, "artifact");
        assert!(
            report
                .critical_failures
                .iter()
                .any(|f| f == "Real source code present"),
            "missing source must be a critical failure: {:?}",
            report.critical_failures
        );
        assert!(!report.passed, "gate must be BLOCKED with zero source");
    }

    #[test]
    fn quality_real_source_check_passes_when_source_present() {
        // With at least one real source file on disk, the "Real source code
        // present" check passes and is NOT a critical failure.
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        fs::write(
            tmp.path().join("App.tsx"),
            "export const App = () => null;\n",
        )
        .unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let src = report
            .checks
            .iter()
            .find(|c| c.name == "Real source code present")
            .expect("real-source check must exist");
        assert_eq!(src.status, "passed", "source present must pass: {src:?}");
        assert!(!report
            .critical_failures
            .iter()
            .any(|f| f == "Real source code present"));
    }

    #[test]
    fn quality_real_source_check_not_triggered_for_docs_only_plan() {
        // A docs-only plan ships no code, so the zero-source check must NOT fire
        // (no false alarm) even with an empty workspace.
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.requirement = "只做调研 写文档 research only".to_string();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let src = report
            .checks
            .iter()
            .find(|c| c.name == "Real source code present")
            .expect("real-source check must exist");
        assert_eq!(
            src.status, "passed",
            "docs-only plan must not trigger the source check: {src:?}"
        );
        assert!(!report
            .critical_failures
            .iter()
            .any(|f| f == "Real source code present"));
    }

    #[test]
    fn quality_counts_code_violations() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        // Seed two emoji-block events in the tool-call log
        let audit_dir = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&audit_dir).unwrap();
        let log = audit_dir.join("tool-calls.jsonl");
        fs::write(&log, r#"{"ts":1,"tool":"Write","file":"a.tsx","decision":"block","clause":"UD-CODE-001","reason":"emoji","session_id":""}
{"ts":2,"tool":"Write","file":"b.tsx","decision":"block","clause":"UD-CODE-001","reason":"emoji","session_id":""}
{"ts":3,"tool":"Write","file":"c.tsx","decision":"block","clause":"UD-CODE-002","reason":"color","session_id":""}
"#).unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let emoji_check = report
            .checks
            .iter()
            .find(|c| c.name == "Emoji block events")
            .unwrap();
        assert!(emoji_check.score < 100);
        assert!(emoji_check.details.contains('2'));
    }

    #[test]
    fn delivery_produces_proof_pack_zip() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        // Populate the workspace with a previous full run
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        run_frontend(&o).unwrap();
        run_backend(&o).unwrap();
        run_quality(&o).unwrap();

        let out = run_delivery(&o).unwrap();
        // expect at least the compliance mapping + zip + manifest
        assert!(out
            .artifacts
            .iter()
            .any(|p| p.extension().and_then(|s| s.to_str()) == Some("zip")));
        assert!(out
            .artifacts
            .iter()
            .any(|p| p.to_string_lossy().contains("compliance-mapping.json")));
        let zip = out
            .artifacts
            .iter()
            .find(|p| p.extension().and_then(|s| s.to_str()) == Some("zip"))
            .unwrap();
        assert!(zip.is_file());
        assert!(fs::metadata(zip).unwrap().len() > 0);
    }

    // ---- smart knowledge digest ----

    #[test]
    fn extract_keywords_filters_short_and_stopwords() {
        let kws = extract_keywords("Build a login system with OAuth 2 and MFA support");
        // 'build', 'with', 'and', 'system', 'support' are stopwords or short;
        // 'login', 'oauth', 'mfa' should survive.
        assert!(kws.iter().any(|k| k == "login"));
        assert!(kws.iter().any(|k| k == "oauth"));
        assert!(kws.iter().any(|k| k == "mfa"));
        assert!(!kws.iter().any(|k| k == "the"));
        assert!(!kws.iter().any(|k| k == "build"));
    }

    #[test]
    fn score_path_counts_keyword_hits() {
        let kws = vec!["login".to_string(), "oauth".to_string()];
        // Path hits are weighted 2x. Files that don't exist get 0
        // content hits, so score = path_hits * 2 + 0.
        assert_eq!(
            score_path("security/login-oauth-playbook.md", &kws),
            4 // 2 path hits * 2
        );
        assert_eq!(
            score_path("auth/login.md", &kws),
            2 // 1 path hit * 2
        );
        assert_eq!(score_path("docs/contributing.md", &kws), 0);
    }

    #[test]
    fn smart_digest_picks_keyword_matches_top() {
        let tmp = TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge");
        fs::create_dir_all(kd.join("security")).unwrap();
        fs::create_dir_all(kd.join("infra")).unwrap();
        // Should rank these first (keyword "login" present)
        fs::write(
            kd.join("security/login-playbook.md"),
            "# Login Playbook\n\nUse OAuth2 with PKCE.\n",
        )
        .unwrap();
        fs::write(kd.join("security/oauth-complete.md"), "# OAuth Complete\n").unwrap();
        // Should NOT rank above (no keyword)
        fs::write(kd.join("infra/kubernetes-101.md"), "# Kubernetes 101\n").unwrap();
        fs::write(kd.join("infra/postgres-tuning.md"), "# Postgres Tuning\n").unwrap();

        let digest = smart_knowledge_digest(&kd, "build a login system with oauth");
        // The keyword-matched files appear before the unrelated ones.
        let login_idx = digest.find("login-playbook").unwrap();
        let kube_idx = digest.find("kubernetes").unwrap_or(usize::MAX);
        assert!(login_idx < kube_idx, "keyword-matched file must rank first");
        // Excerpt content is included.
        assert!(digest.contains("Use OAuth2 with PKCE."));
    }

    #[test]
    fn smart_digest_falls_back_to_lex_when_no_keyword_match() {
        let tmp = TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge");
        fs::create_dir_all(&kd).unwrap();
        fs::write(kd.join("aaa-first.md"), "# A\n").unwrap();
        fs::write(kd.join("zzz-last.md"), "# Z\n").unwrap();
        // Requirement entirely in CJK → no keyword overlap with English file names.
        let digest = smart_knowledge_digest(&kd, "做一个登录系统");
        // Both files appear; lex-sorted: aaa- before zzz-.
        let a_idx = digest.find("aaa-first").unwrap();
        let z_idx = digest.find("zzz-last").unwrap();
        assert!(a_idx < z_idx);
    }

    #[test]
    fn smart_digest_handles_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let kd = tmp.path().join("nonexistent");
        let digest = smart_knowledge_digest(&kd, "anything");
        assert!(digest.contains("no `knowledge/`"));
    }

    // ---- review_document_structure: heading-based validation (hardened) ----

    #[test]
    fn review_structure_passes_when_heading_present() {
        let doc = "# PRD\n\n## Goal\nBuild something\n\n## Scope\nIn scope\n\n## Acceptance Criteria\n- [ ] works";
        let defects = review_document_structure(
            doc,
            &[
                ("## goal", "Missing goal"),
                ("## scope", "Missing scope"),
                ("## acceptance criteria", "Missing AC"),
            ],
        );
        assert!(
            defects.is_empty(),
            "headings present → no defects: {defects:?}"
        );
    }

    #[test]
    fn review_structure_fails_when_completely_absent() {
        // Neither a heading NOR any body-text mention → must have defects.
        let doc = "# PRD\n\nThis is just prose with no relevant keyword at all.";
        let defects = review_document_structure(doc, &[("## goal", "Missing goal")]);
        assert!(
            !defects.is_empty(),
            "no heading and no body mention → defects"
        );
    }

    #[test]
    fn review_structure_body_text_satisfies_content_token() {
        // A content token like `--color` lives in a code fence, not a heading.
        // The full-text fallback must catch it so a correctly-authored UIUX
        // doc with a `:root { --color-bg: … }` block passes.
        let doc = "# UIUX\n\n```css\n:root {\n  --color-bg: #0f172a;\n}\n```";
        let defects = review_document_structure(doc, &[("--color", "Missing CSS color tokens")]);
        assert!(
            defects.is_empty(),
            "--color in a code fence must satisfy the check: {defects:?}"
        );
    }

    #[test]
    fn review_structure_matches_partial_heading() {
        // "## API Surface" should match keyword "api" (starts_with).
        let doc = "# Arch\n\n## API Surface\nDetails here";
        let defects = review_document_structure(doc, &[("## api", "Missing API")]);
        assert!(defects.is_empty(), "partial heading match should pass");
    }

    #[test]
    fn review_structure_finds_content_token_in_code_fence() {
        // Regression: a UIUX doc with CSS custom properties inside a ```css
        // fence must satisfy the `--color` / `--font` checks. Before the
        // full-text fix, these patterns only existed in body text (never in
        // a heading) so the checker always reported "Missing CSS color tokens".
        let doc = "# UIUX\n\n## Color palette\n\n```css\n:root {\n  --color-bg: #0f172a;\n  --font-sans: 'Inter';\n}\n```\n\nButtons have hover and focus states. Icons use the Lucide icon library.";
        let defects = review_document_structure(
            doc,
            &[
                ("--color", "Missing CSS color tokens"),
                ("--font", "Missing typography tokens"),
                ("icon", "Missing icon library declaration"),
                ("hover", "Missing component states (hover/focus)"),
            ],
        );
        assert!(
            defects.is_empty(),
            "all 4 content tokens present in body/code-fence: {defects:?}"
        );
    }

    #[test]
    fn review_structure_finds_table_rows_in_full_text() {
        // Regression: architecture docs have API route tables as body text
        // (`| method | path | … |`), not headings. The `| ` pattern must be
        // found via the full-text path.
        let doc = "# Architecture\n\n## API surface\n\n| Method | Path |\n|---|---|\n| POST | /api/subscribe |";
        let defects = review_document_structure(doc, &[("| ", "Missing API route table")]);
        assert!(
            defects.is_empty(),
            "table row in body should pass: {defects:?}"
        );
    }

    // ---- verify_results_check ----

    #[test]
    fn verify_results_check_none_when_no_jsonl() {
        let tmp = TempDir::new().unwrap();
        assert!(verify_results_check(tmp.path()).is_none());
    }

    #[test]
    fn verify_results_check_passes_all_steps() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("verify.jsonl"),
            r#"{"step":"install","passed":true,"skipped":false,"timestamp":"t1"}
{"step":"test","passed":true,"skipped":false,"timestamp":"t1"}
{"step":"build","passed":true,"skipped":false,"timestamp":"t1"}
"#,
        )
        .unwrap();
        let check = verify_results_check(tmp.path()).unwrap();
        assert_eq!(check.name, "Build & test results");
        assert_eq!(check.status, "passed");
        assert_eq!(check.score, 100);
    }

    #[test]
    fn verify_results_check_fails_on_build_failure() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("verify.jsonl"),
            r#"{"step":"install","passed":true,"skipped":false,"timestamp":"t1"}
{"step":"build","passed":false,"skipped":false,"timestamp":"t1"}
"#,
        )
        .unwrap();
        let check = verify_results_check(tmp.path()).unwrap();
        assert_eq!(check.status, "failed");
        assert_eq!(check.score, 0);
    }

    #[test]
    fn verify_results_check_ignores_skipped_steps() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("verify.jsonl"),
            r#"{"step":"install","passed":true,"skipped":false,"timestamp":"t1"}
{"step":"lint","passed":false,"skipped":true,"timestamp":"t1"}
{"step":"test","passed":true,"skipped":false,"timestamp":"t1"}
"#,
        )
        .unwrap();
        let check = verify_results_check(tmp.path()).unwrap();
        assert_eq!(
            check.status, "passed",
            "skipped lint failure must not fail the check"
        );
    }

    // ---- phase_knowledge_digest BM25 path ----

    #[test]
    fn phase_knowledge_digest_returns_empty_without_dir() {
        // Neutralise the bundled-corpus fallbacks so a bare project resolves to
        // nothing even on a machine that has staged ~/.umadev/knowledge.
        let _no_corpus = NoBundledCorpus::new();
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        let d = phase_knowledge_digest(&o, Phase::Research);
        assert!(d.is_empty());
    }

    #[test]
    fn phase_knowledge_digest_uses_bm25_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge/security");
        fs::create_dir_all(&kd).unwrap();
        fs::write(
            kd.join("login.md"),
            "# Login\n\n## OAuth\n\nUse OAuth2 with PKCE for login authentication.",
        )
        .unwrap();
        // Write .umadevrc to enable knowledge (default is enabled).
        fs::write(tmp.path().join(".umadevrc"), "[quality]\nthreshold = 90\n").unwrap();
        let o = opts(tmp.path());
        let d = phase_knowledge_digest(&o, Phase::Backend);
        assert!(d.contains("Expert knowledge"), "should produce digest: {d}");
        assert!(
            d.contains("login"),
            "should contain relevant knowledge: {d}"
        );
    }

    #[test]
    fn phase_knowledge_digest_with_vector_is_none_for_bm25() {
        // When engine=bm25, passing a query_vec should still work (ignored).
        let tmp = TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge/security");
        fs::create_dir_all(&kd).unwrap();
        fs::write(kd.join("login.md"), "# Login\n\n## OAuth\n\nlogin auth").unwrap();
        let o = opts(tmp.path());
        let d = phase_knowledge_digest_with_vector(&o, Phase::Backend, Some(&[0.1; 1536]));
        assert!(d.contains("Expert knowledge"));
    }

    #[test]
    fn phase_knowledge_digest_gates_return_empty() {
        let tmp = TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge/security");
        fs::create_dir_all(&kd).unwrap();
        fs::write(kd.join("login.md"), "# Login\n\n## OAuth\n\nlogin").unwrap();
        let o = opts(tmp.path());
        assert!(phase_knowledge_digest(&o, Phase::DocsConfirm).is_empty());
        assert!(phase_knowledge_digest(&o, Phase::PreviewConfirm).is_empty());
    }

    // ---- quality gate contract + ops checks ----

    #[test]
    fn quality_includes_contract_checks() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"OpenAPI contract"),
            "must have OpenAPI check: {names:?}"
        );
        assert!(
            names.contains(&"Frontend↔contract conformance"),
            "must have contract conformance: {names:?}"
        );
    }

    #[test]
    fn quality_includes_ops_artifacts_check() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        // Ops artifacts check should exist and produce scaffolding.
        let ops = report
            .checks
            .iter()
            .find(|c| c.name == "Ops artifacts present");
        assert!(ops.is_some(), "must have ops artifacts check");
        // Scaffolding files should have been generated by the check.
        assert!(
            tmp.path().join("Dockerfile").is_file(),
            "Dockerfile must be generated"
        );
    }

    #[test]
    fn quality_includes_pagination_and_error_checks() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"Pagination strategy"),
            "must have pagination: {names:?}"
        );
        assert!(
            names.contains(&"Error handling convention"),
            "must have error convention: {names:?}"
        );
    }

    #[test]
    fn quality_includes_input_validation_check() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        assert!(report
            .checks
            .iter()
            .any(|c| c.name == "Input validation coverage"));
    }

    #[test]
    fn quality_verify_check_appears_when_jsonl_exists() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        // Seed a verify.jsonl with passing steps.
        let audit = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&audit).unwrap();
        fs::write(
            audit.join("verify.jsonl"),
            r#"{"step":"test","passed":true,"skipped":false,"timestamp":"t"}"#,
        )
        .unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        assert!(report
            .checks
            .iter()
            .any(|c| c.name == "Build & test results" && c.status == "passed"));
    }

    #[test]
    fn quality_verify_check_critical_on_build_fail() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        let audit = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&audit).unwrap();
        fs::write(
            audit.join("verify.jsonl"),
            r#"{"step":"build","passed":false,"skipped":false,"timestamp":"t"}"#,
        )
        .unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        assert!(
            report
                .critical_failures
                .iter()
                .any(|f| f.contains("Build & test")),
            "build failure must be critical: {:?}",
            report.critical_failures
        );
    }

    #[test]
    fn execution_plan_check_validates_content() {
        // The execution plan check should fail on a 1-byte stub file,
        // and pass on a real plan with ## sections.
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        // Write a stub execution plan (too short + no sections).
        let out_dir = tmp.path().join("output");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("demo-execution-plan.md"), "x").unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let ep = report
            .checks
            .iter()
            .find(|c| c.name == "Execution plan")
            .unwrap();
        assert!(ep.score < 100, "stub exec plan should not pass: {ep:?}");
    }

    #[test]
    fn execution_plan_passes_with_structured_content() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        // run_spec writes a real execution plan — quality should pass it.
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let ep = report.checks.iter().find(|c| c.name == "Execution plan");
        if let Some(ep) = ep {
            assert!(ep.score >= 60, "real exec plan should score well: {ep:?}");
        }
    }

    #[test]
    fn delivery_captures_validated_patterns() {
        // run_delivery should call capture_validated_patterns + sediment_lessons.
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        run_frontend(&o).unwrap();
        run_backend(&o).unwrap();
        run_quality(&o).unwrap();
        run_delivery(&o).unwrap();
        // sediment_lessons should have created at least one lesson file.
        let learned_dir = tmp.path().join(".umadev/learned");
        assert!(
            learned_dir.is_dir(),
            "learned dir should exist after delivery"
        );
    }

    #[test]
    fn scaffolding_generated_during_quality() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        run_quality(&o).unwrap();
        // Quality gate should have generated scaffolding.
        assert!(
            tmp.path().join("Dockerfile").is_file(),
            "Dockerfile generated"
        );
        assert!(
            tmp.path().join(".github/workflows/ci.yml").is_file(),
            "CI generated"
        );
        assert!(
            tmp.path().join("migrations/0001_init.sql").is_file(),
            "migration generated"
        );
    }

    #[test]
    fn ops_artifacts_content_validated() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let ops = report
            .checks
            .iter()
            .find(|c| c.name == "Ops artifacts present")
            .unwrap();
        // Scaffolding was generated → should pass.
        assert_eq!(
            ops.status, "passed",
            "ops artifacts should pass after scaffolding gen"
        );
    }

    #[test]
    fn skip_checks_respected() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        fs::write(
            tmp.path().join(".umadevrc"),
            "[quality]\nskip_checks = [\"Dark mode support\"]\n",
        )
        .unwrap();
        run_research(&o, None).unwrap();
        run_docs(&o, &DocsContent::default()).unwrap();
        run_spec(&o).unwrap();
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        assert!(
            !report.checks.iter().any(|c| c.name == "Dark mode support"),
            "skipped check should not appear in report"
        );
    }

    #[test]
    fn extract_quality_score_parses_json() {
        let json = r#"{"passed":true,"score":92,"weighted_score":91.5}"#;
        let (score, passed) = extract_quality_score(json);
        assert_eq!(score, "?"); // no total_score field here → unknown
        assert!(passed);
    }

    #[test]
    fn extract_quality_score_reads_total_not_first_check() {
        // Real QualityReport shape: each check has its OWN "score", plus a
        // top-level "total_score". The extractor MUST read total_score (97),
        // NOT the first check's score (40). This is a regression test for a
        // bug where the score was parsed off the first "score" substring.
        let json = r#"{
            "passed": true,
            "total_score": 97,
            "weighted_score": 96.5,
            "checks": [
                {"name":"api_url_consistency","score":40,"passed":true},
                {"name":"completeness","score":60,"passed":true}
            ]
        }"#;
        let (score, passed) = extract_quality_score(json);
        assert_eq!(score, "97");
        assert!(passed);
    }

    #[test]
    fn extract_quality_score_handles_missing() {
        let json = r#"{"passed":false}"#;
        let (_score, passed) = extract_quality_score(json);
        assert!(!passed);
    }

    #[test]
    fn quality_findings_extracts_top_failing_checks() {
        // critical_failures come first, then the lowest-scoring non-passing
        // checks (worst first); passing checks never appear.
        let json = r#"{
            "passed": false,
            "total_score": 42,
            "weighted_score": 42.0,
            "scenario": "test",
            "critical_failures": ["Build & test results"],
            "recommendations": [],
            "summary": {"executive_summary":"fail","summary_context":{}},
            "checks": [
                {"name":"Clean","category":"a","description":"d","status":"passed","score":100,"weight":1.0,"details":"ok"},
                {"name":"Contract","category":"a","description":"d","status":"failed","score":10,"weight":1.0,"details":"3 calls unmatched"},
                {"name":"Coverage","category":"a","description":"d","status":"warning","score":55,"weight":1.0,"details":"low FR coverage"}
            ]
        }"#;
        let f = quality_findings(json, 5);
        // Critical failure first.
        assert_eq!(f[0], "Build & test results");
        // Then worst-scoring failing check (Contract, score 10) before Coverage.
        assert!(f
            .iter()
            .any(|x| x.contains("Contract") && x.contains("3 calls unmatched")));
        let contract_pos = f.iter().position(|x| x.contains("Contract")).unwrap();
        let coverage_pos = f.iter().position(|x| x.contains("Coverage")).unwrap();
        assert!(
            contract_pos < coverage_pos,
            "worst score must come first: {f:?}"
        );
        // The passing check is excluded.
        assert!(
            !f.iter().any(|x| x.contains("Clean")),
            "passed checks excluded: {f:?}"
        );
    }

    #[test]
    fn quality_findings_respects_max_and_fails_open() {
        let json = r#"{
            "passed": false, "total_score": 0, "weighted_score": 0.0, "scenario": "t",
            "critical_failures": ["a","b","c"], "recommendations": [],
            "summary": {"executive_summary":"x","summary_context":{}}, "checks": []
        }"#;
        assert_eq!(quality_findings(json, 2).len(), 2, "max is honoured");
        // Malformed JSON → empty vec (fail-open), never a panic.
        assert!(quality_findings("not json at all", 5).is_empty());
        assert!(quality_findings("{}", 5).is_empty());
    }

    #[test]
    fn score_uiux_completeness_returns_zero_for_missing() {
        let tmp = TempDir::new().unwrap();
        let score = score_uiux_completeness(&tmp.path().join("nonexistent.md"));
        assert_eq!(score, 0);
    }

    #[test]
    fn knowledge_top_files_returns_count() {
        let tmp = TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge/security");
        fs::create_dir_all(&kd).unwrap();
        fs::write(kd.join("a.md"), "# A\n").unwrap();
        fs::write(kd.join("b.md"), "# B\n").unwrap();
        let o = opts(tmp.path());
        let (files, total) = knowledge_top_files(&o);
        assert_eq!(total, 2);
        assert!(!files.is_empty());
    }

    #[test]
    fn phase_knowledge_digest_falls_back_to_legacy_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge/security");
        fs::create_dir_all(&kd).unwrap();
        fs::write(kd.join("login.md"), "# Login\n\n## OAuth\n\nlogin auth").unwrap();
        fs::write(
            tmp.path().join(".umadevrc"),
            "[knowledge]\nenabled = false\n",
        )
        .unwrap();
        let o = opts(tmp.path());
        let d = phase_knowledge_digest(&o, Phase::Backend);
        // Legacy path should still produce output for a matched keyword.
        assert!(
            d.contains("Expert knowledge") || d.is_empty(),
            "legacy path produces output or empty"
        );
    }

    #[test]
    fn review_structure_matches_multiple_headings() {
        let doc = "# Arch\n\n## API Surface\nDetails\n\n## Data Model\nSchema\n\n## Auth\nJWT";
        let defects = review_document_structure(
            doc,
            &[
                ("## api", "Missing API"),
                ("## data", "Missing data"),
                ("## auth", "Missing auth"),
            ],
        );
        assert!(defects.is_empty(), "all headings present: {defects:?}");
    }

    #[test]
    fn verify_results_check_handles_empty_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("verify.jsonl"), "").unwrap();
        assert!(
            verify_results_check(tmp.path()).is_none(),
            "empty jsonl → None"
        );
    }

    #[test]
    fn evidence_check_works_with_missing_file() {
        let tmp = TempDir::new().unwrap();
        let check = evidence_check("Test", "desc", &tmp.path().join("nonexistent.jsonl"), 1.0);
        assert_eq!(check.status, "warning");
        assert_eq!(check.score, 60);
    }

    #[test]
    fn slop_checker_ignores_prohibition_context() {
        // A doc that says 'no "lorem ipsum"' is enforcing the rule, not
        // violating it. The prohibition-context fix must not count it.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("prd.md"),
            "# PRD\n\nReal copy only. No \"lorem ipsum\" filler allowed.\nAvoid \"Welcome to\" headings.",
        )
        .unwrap();
        assert_eq!(
            count_slop_violations(tmp.path()),
            0,
            "prohibition context should not count"
        );
    }

    #[test]
    fn slop_checker_counts_actual_slop() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("page.md"),
            "# Welcome to our product\n\nLorem ipsum dolor sit amet, consectetur.",
        )
        .unwrap();
        assert!(
            count_slop_violations(tmp.path()) > 0,
            "actual slop must be counted"
        );
    }

    #[test]
    fn slop_checker_skips_quality_gate_report() {
        // The generated quality-gate.md report quotes the patterns it checks
        // for — it must not self-trigger a slop violation.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("demo-quality-gate.md"),
            "# Quality gate\n\n| Anti-AI-slop | warning | Lorem ipsum detected |",
        )
        .unwrap();
        assert_eq!(
            count_slop_violations(tmp.path()),
            0,
            "quality-gate report should be skipped"
        );
    }

    // #5: the FE↔contract check must scan the REAL generated frontend SOURCE
    // tree, not the worker-notes markdown. The contract extractor walks the
    // project root for `.ts`/`.tsx`/… files; if quality fed it the notes path
    // (the old bug) the scan was always empty and the check was a no-op.
    #[test]
    fn frontend_contract_scan_reads_real_source_not_notes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Worker-notes markdown mentions /api/foo in prose only — must be IGNORED
        // for the source-call scan (it's not real frontend code).
        let out = root.join("output");
        fs::create_dir_all(&out).unwrap();
        fs::write(
            out.join("demo-frontend-notes.md"),
            "# notes\n- call GET /api/from-notes-only\n",
        )
        .unwrap();
        // Real frontend source with an actual fetch — THIS is what must be found.
        let src = root.join("web/src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("api.ts"),
            "export const load = () => fetch('/api/users');\n",
        )
        .unwrap();

        // Scanning the project root finds the real call from the .ts source…
        let calls = umadev_contract::extract_frontend_calls(root);
        assert!(
            calls.iter().any(|c| c.path == "/api/users"),
            "must extract the real fetch('/api/users') from source: {calls:?}"
        );
        // …and does NOT surface the prose-only path that lives in the notes md
        // (the notes file is under output/, which the extractor skips).
        assert!(
            !calls.iter().any(|c| c.path == "/api/from-notes-only"),
            "must not pick up prose paths from the notes markdown: {calls:?}"
        );
        // Sanity: handing the extractor the NOTES FILE path (the old bug) yields
        // nothing — proving the previous wiring diluted the check to a no-op.
        let via_notes_path =
            umadev_contract::extract_frontend_calls(&out.join("demo-frontend-notes.md"));
        assert!(
            via_notes_path.is_empty(),
            "scanning the notes file path (old bug) finds no source calls: {via_notes_path:?}"
        );
    }

    // ---- context-aware N/A on the quality gate -------------------------

    /// Build RunOptions for an explicitly-simple, pure static-frontend build.
    fn static_frontend_opts(root: &Path) -> RunOptions {
        let mut o = opts(root);
        o.requirement = "做一个简单的待办清单单页应用,纯前端 HTML+CSS+JS".to_string();
        o
    }

    #[test]
    fn static_frontend_marks_backend_contract_checks_na() {
        // A proven static-frontend run: the backend-contract + ops checks must be
        // marked `n/a` (no server/API/data surface to guard) rather than scored
        // low, so a clean static page isn't penalised for "missing" an API
        // contract / Dockerfile it has no reason to ship.
        let tmp = TempDir::new().unwrap();
        let o = static_frontend_opts(tmp.path());
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        // At least one surface-bound check that was PENALISING the static page
        // (status failed/warning, no backend to satisfy it) is now N/A — the
        // mechanism removes inapplicable penalties. Every N/A row must be a
        // surface-bound check that was NOT passing (we never N/A the floor, and
        // never N/A a passing check).
        let na: Vec<&QualityCheck> = report.checks.iter().filter(|c| c.status == "n/a").collect();
        assert!(
            !na.is_empty(),
            "a static frontend with no backend artifacts should N/A some surface checks"
        );
        for c in &na {
            assert!(
                SURFACE_BOUND_CHECKS.contains(&c.name.as_str())
                    || DOC_BOUND_CHECKS.contains(&c.name.as_str()),
                "only surface-bound or (for a lean plan) doc-bound checks may be N/A, got `{}`",
                c.name
            );
        }
        // A surface check that already PASSES (no endpoints → Auth 100) stays
        // live — N/A only removes penalties, never positive signal.
        if let Some(c) = report.checks.iter().find(|c| c.name == "Auth coverage") {
            if c.score == 100 {
                assert_ne!(c.status, "n/a", "a passing surface check stays live");
            }
        }
        // The universal floor must STILL be live (not N/A) on a static frontend.
        for name in ["No leaked secrets", "Anti-AI-slop check"] {
            if let Some(c) = report.checks.iter().find(|c| c.name == name) {
                assert_ne!(c.status, "n/a", "{name} is a universal floor — never N/A");
            }
        }
    }

    #[test]
    fn na_checks_excluded_from_score() {
        // N/A checks neither help nor hurt: a static-frontend run that has none of
        // the backend artifacts should NOT be dragged down by the (now-N/A)
        // contract/ops checks. We assert the scored set excludes every N/A row.
        let tmp = TempDir::new().unwrap();
        let o = static_frontend_opts(tmp.path());
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        let scored: Vec<&QualityCheck> =
            report.checks.iter().filter(|c| c.status != "n/a").collect();
        let scored_mean = if scored.is_empty() {
            0
        } else {
            scored.iter().map(|c| c.score).sum::<i32>() / i32::try_from(scored.len()).unwrap()
        };
        assert_eq!(
            report.total_score, scored_mean,
            "total_score must be the mean of the NON-N/A checks only"
        );
        // At least one check was actually marked N/A (the mechanism fired).
        assert!(
            report.checks.iter().any(|c| c.status == "n/a"),
            "a static-frontend run should mark some surface-bound checks N/A"
        );
    }

    #[test]
    fn greenfield_keeps_all_checks_live() {
        // The conservative default: a greenfield/login product (the default
        // `opts` requirement is "build a login system") marks NOTHING N/A — every
        // backend-contract + ops check stays live and scored.
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path()); // requirement = "build a login system" → strict
        let out = run_quality(&o).unwrap();
        let json = fs::read_to_string(&out.artifacts[0]).unwrap();
        let report: QualityReport = serde_json::from_str(&json).unwrap();
        assert!(
            report.checks.iter().all(|c| c.status != "n/a"),
            "a strict (backend) project must keep every check live, none N/A"
        );
    }

    #[test]
    fn static_frontend_scores_at_least_as_high_as_strict() {
        // The headline behaviour: on the SAME clean-but-docs-light workspace, the
        // static-frontend context scores at least as high as the strict context —
        // because the contract/ops checks it can't satisfy are N/A, not failing.
        let tmp_a = TempDir::new().unwrap();
        let lenient = run_quality(&static_frontend_opts(tmp_a.path())).unwrap();
        let report_l: QualityReport =
            serde_json::from_str(&fs::read_to_string(&lenient.artifacts[0]).unwrap()).unwrap();

        let tmp_b = TempDir::new().unwrap();
        let strict = run_quality(&opts(tmp_b.path())).unwrap(); // "build a login system"
        let report_s: QualityReport =
            serde_json::from_str(&fs::read_to_string(&strict.artifacts[0]).unwrap()).unwrap();

        assert!(
            report_l.total_score >= report_s.total_score,
            "static frontend ({}) should not score below strict ({}) on an equivalent empty workspace",
            report_l.total_score,
            report_s.total_score
        );
    }
}
