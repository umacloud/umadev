//! Reusable skill library — the success-compounding half of the self-evolution
//! loop, the positive mirror of the pitfall KB in [`crate::lessons`].
//!
//! A pitfall records "what bit us, avoid it next time". A SKILL records the
//! opposite: a verified, reusable capability — a diff, recipe, or contract
//! decision that already CLEARED the quality gate / contract cross-check /
//! build — distilled into `{ description, content }` so a later run can REUSE
//! the proven approach instead of re-deriving it.
//!
//! Until now a passing run only sedimented a one-line "validated pattern"
//! avoid-reminder ([`crate::lessons::LessonKind::ValidatedPattern`]); that is an
//! observation, not a reusable ability. This module upgrades it into a real
//! skill entry with a durable JSONL store, a graduation gate, top-k retrieval,
//! and utility-decay retirement.
//!
//! ## Graduation gate — the ONLY entry point
//! A skill is admitted ONLY when the producing run actually graduated:
//! - the artifact PASSED the quality gate / contract cross-check / build, AND
//! - it was a MULTI-STEP solve (the run needed revisions, quality fixes, or
//!   dev-error recoveries). A trivial one-pass result carries no reusable
//!   insight worth compounding, so it is NOT admitted.
//!
//! Both conditions are checked by [`graduate_skill`] off the existing delivery
//! capture point — no new pipeline phase.
//!
//! ## Retrieval — reuse the curated BM25/vector path
//! Each graduated skill's content is mirrored to `.umadev/learned/skills/` as a
//! markdown file so the existing [`umadev_knowledge::retrieve`] index (BM25 by
//! default, vector when an embedding backend is reachable — both fail-open)
//! picks it up with zero new machinery. [`retrieve_skills`] queries that index
//! with the *solution idea* (not the bare task) and returns the top-k skill
//! hits.
//!
//! ## Retirement — utility decay + hard cap
//! Each store entry tracks a utility counter. A skill that is retrieved AND
//! still validates is promoted (utility++, recency refreshed); one that goes
//! long unmatched decays out of the top-k and, past [`MAX_SKILLS`], is evicted
//! lowest-utility-first — the same bounded-store discipline the pitfall KB uses.
//!
//! Every function here is fail-open: an I/O or parse error is a no-op (returns
//! empty / `false`), never blocking the base or the pipeline.

use std::fs;
use std::path::Path;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::lessons::{Lesson, LessonKind};

/// Durable JSONL store of skill entries (the authoritative ledger driving
/// decay + retirement), relative to the project root.
pub const SKILLS_DIR: &str = ".umadev/skills";
/// JSONL filename inside [`SKILLS_DIR`].
pub const SKILLS_FILE: &str = "skills.jsonl";
/// Where each skill's content is MIRRORED as markdown so the existing knowledge
/// index retrieves it. Lives under the already-indexed `.umadev/learned/` tree.
pub const SKILLS_LEARNED_SUBDIR: &str = ".umadev/learned/skills";

/// Hard cap on distinct skills kept in the store, mirroring the pitfall KB's
/// `MAX_DEV_PITFALLS`. Generous; a long-lived repo stays well under.
const MAX_SKILLS: usize = 200;

/// Half-life (days) for a skill's recency weight — same 30-day decay the
/// pitfall/lesson recency uses, so an unused skill fades from the top-k rather
/// than clinging forever.
const SKILL_RECENCY_HALFLIFE_DAYS: f64 = 30.0;

/// Max chars kept for a skill description (≈6 short sentences) / content, so the
/// store and the injected prompt fragment stay bounded.
const MAX_DESC_CHARS: usize = 600;
const MAX_CONTENT_CHARS: usize = 4000;

/// One reusable skill: a verified capability the tool can REUSE next time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Stable id (slug of the title) — the dedup + mirror-filename key.
    pub id: String,
    /// Short human title (e.g. "Validated REST contract for a blog API").
    pub title: String,
    /// Base-generated summary of the solution idea, ≤6 sentences. Falls back to
    /// a deterministic template when no base reply is available (fail-open).
    pub description: String,
    /// The reusable material itself: the validated diff / recipe / contract
    /// decision the worker can adapt. Indexed for retrieval.
    pub content: String,
    /// Search keywords (also embedded in the mirrored body for BM25).
    pub keywords: Vec<String>,
    /// Domain bucket (`api`, `frontend`, …) — drives the mirror sub-path.
    pub domain: String,
    /// The requirement that produced this skill.
    pub source_requirement: String,
    /// ISO-8601 UTC timestamp of last validation (graduation OR a later reuse
    /// that still passed). Drives the recency half of the decay score.
    pub last_validated: String,
    /// Utility counter: graduation seeds it at 1; every reuse-that-still-passes
    /// increments it. Drives retirement order (lowest-utility evicted first).
    #[serde(default = "default_utility")]
    pub utility: u32,
}

fn default_utility() -> u32 {
    1
}

impl Skill {
    /// Utility, normalised so a legacy 0 row counts as 1.
    #[must_use]
    pub fn utility(&self) -> u32 {
        self.utility.max(1)
    }
}

/// The system+user prompt asking the base to distil a ≤6-sentence reusable
/// description of the solution idea (NOT a restatement of the task). Mirrors the
/// reflection-prompt seam in [`crate::lessons::reflection_prompt`]: the runner
/// calls `try_generate` with this, then passes the reply as `description` to
/// [`graduate_skill`]. Returns `(system, user)`.
///
/// The base call is OPTIONAL — `graduate_skill` accepts an empty description and
/// falls back to a deterministic template, so a missing base / empty reply
/// degrades to zero behaviour change ("底座生成" stays on the host-driver path,
/// never a new endpoint).
#[must_use]
pub fn skill_description_prompt(title: &str, content: &str, requirement: &str) -> (String, String) {
    let system = "\
You are a senior engineer writing a REUSABLE skill card from a solution that \
already passed quality gates and the build. Summarise the underlying SOLUTION \
APPROACH — the decision and why it works — so a future project can reuse it. Do \
NOT restate the task or paste the code. Answer with at most six short \
imperative sentences, no preamble, no headings."
        .to_string();
    let user = format!(
        "## Skill title\n{title}\n\n\
         ## What was built (validated material)\n{content}\n\n\
         ## Originating requirement\n{requirement}\n\n\
         Write the reusable approach in ≤6 sentences.",
        content = truncate(content, 1500),
    );
    (system, user)
}

/// Whether a run was a MULTI-STEP solve worth compounding into a skill.
///
/// Signal: the run left at least one entry in the experience ledgers that only
/// a non-trivial solve produces — a gate revision, a quality failure/warning,
/// or a recovered dev-error pitfall. A clean one-pass run leaves all three
/// empty and is intentionally NOT graduated (its result carries no reusable
/// problem-solving insight). Pure read; fail-open (missing files → not
/// multi-step).
#[must_use]
pub fn was_multi_step(project_root: &Path) -> bool {
    for f in ["gate-revisions.jsonl", "quality-failures.jsonl"] {
        if !crate::lessons::read_raw_lessons(project_root, f).is_empty() {
            return true;
        }
    }
    // Any recorded dev-error means the run hit (and worked through) a real
    // problem — a multi-step solve by definition.
    !crate::lessons::read_raw_lessons(project_root, crate::lessons::DEV_ERRORS_FILE).is_empty()
}

/// The graduation gate — the ONLY way a skill enters the library.
///
/// Admits a skill IFF (a) `passed_gate` is true (the artifact cleared the
/// quality gate / contract cross-check / build) AND (b) the run was a
/// [`was_multi_step`] solve. On admission it writes/updates the JSONL store
/// (deduped by id, utility-refreshed on re-graduation) AND mirrors the content
/// to `.umadev/learned/skills/<id>.md` so [`umadev_knowledge::retrieve`] indexes
/// it. `description` may be empty — a deterministic template is used instead so
/// the base call stays optional. Caps the store at [`MAX_SKILLS`].
///
/// Returns `true` when a skill was admitted (new or refreshed). Fail-open: any
/// failed precondition or I/O error returns `false` without blocking delivery.
#[allow(clippy::too_many_arguments)]
pub fn graduate_skill(
    project_root: &Path,
    title: &str,
    content: &str,
    description: &str,
    domain: &str,
    keywords: &[String],
    requirement: &str,
    passed_gate: bool,
) -> bool {
    // Graduation gate: only proven, hard-won material is worth compounding.
    if !passed_gate || content.trim().is_empty() {
        return false;
    }
    if !was_multi_step(project_root) {
        return false;
    }

    // Process-wide lock + poison-recovery, identical to the pitfall KB's
    // DEV_KB_LOCK discipline, so the parallel docs fan-out can't clobber the
    // store mid read-modify-write.
    static SKILL_KB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SKILL_KB_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let id = slug(title);
    if id.is_empty() {
        return false;
    }
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let description = if description.trim().is_empty() {
        template_description(title, domain, requirement)
    } else {
        truncate(description.trim(), MAX_DESC_CHARS)
    };

    let mut store = read_skills(project_root);
    if let Some(existing) = store.iter_mut().find(|s| s.id == id) {
        // Re-graduation of the same skill: refresh content/description, bump
        // utility (it proved reusable enough to recur), re-baseline recency.
        existing.description.clone_from(&description);
        existing.content = truncate(content.trim(), MAX_CONTENT_CHARS);
        existing.last_validated.clone_from(&now);
        existing.utility = existing.utility().saturating_add(1);
        merge_keywords(&mut existing.keywords, keywords);
    } else {
        store.push(Skill {
            id: id.clone(),
            title: truncate(title.trim(), 160),
            description: description.clone(),
            content: truncate(content.trim(), MAX_CONTENT_CHARS),
            keywords: dedup_keywords(keywords),
            domain: if domain.trim().is_empty() {
                "general".to_string()
            } else {
                domain.trim().to_string()
            },
            source_requirement: requirement.to_string(),
            last_validated: now.clone(),
            utility: 1,
        });
    }
    retire_skills(&mut store);
    write_skills(project_root, &store);
    // Mirror every surviving skill to the indexed learned/ tree so retrieval
    // sees the current set (and drops files for evicted ones).
    mirror_skills_to_index(project_root, &store);
    true
}

/// Retrieve the top-k reusable skills for a solution idea, via the SAME curated
/// retrieval path the knowledge base uses ([`umadev_knowledge::retrieve`] — BM25
/// by default, BM25+vector RRF when an embedding backend is reachable, both
/// fail-open). Query with the *solution idea* — what you're trying to achieve —
/// not the bare task string, so matches are by approach, not surface wording.
///
/// Returns up to `top_k` skills ordered by retrieval relevance, then by the
/// utility·recency decay score so a fresher, more-reused skill wins ties.
/// **Side effect:** each returned skill is PROMOTED (utility++, recency
/// refreshed) — surfacing a skill for reuse is itself a use, which keeps it in
/// the top-k and pushes never-matched skills toward retirement. Empty when the
/// library is empty or nothing matches. Fail-open.
#[must_use]
pub fn retrieve_skills(
    project_root: &Path,
    knowledge_dir: &Path,
    solution_idea: &str,
    top_k: usize,
) -> Vec<Skill> {
    let store = read_skills(project_root);
    if store.is_empty() || solution_idea.trim().is_empty() || top_k == 0 {
        return Vec::new();
    }

    // Reuse the curated retrieval engine. Over-fetch (top_k*3) so the path
    // filter + the in-store join still leave room for top_k results. The
    // `Backend` phase keeps API/contract-style skills in scope while still
    // admitting the cross-cutting learned/ tree (lessons + skills are always
    // allowed through the phase filter).
    let cfg = umadev_knowledge::RetrievalConfig {
        enabled: true,
        engine: umadev_knowledge::RetrievalEngine::default(),
        top_k: top_k.saturating_mul(3).max(top_k),
        custom_dirs: Vec::new(),
    };
    let hits = umadev_knowledge::retrieve_for_phase(
        project_root,
        knowledge_dir,
        &cfg,
        solution_idea,
        umadev_spec::Phase::Backend,
    );

    // Join retrieval hits back to store entries by skill id (the mirror file is
    // `skills/<id>.md`, so the chunk path carries the id). Preserve retrieval
    // order; de-dup so the same skill isn't returned twice from multiple chunks.
    let mut picked: Vec<Skill> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for hit in &hits {
        let Some(id) = skill_id_from_path(&hit.chunk.meta.path) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(s) = store.iter().find(|s| s.id == id) {
            picked.push(s.clone());
            if picked.len() >= top_k {
                break;
            }
        }
    }
    if picked.is_empty() {
        return Vec::new();
    }

    // Promote the surfaced skills: a retrieval IS a use, so they earn utility +
    // a recency refresh. This is what makes "retrieved-and-still-valid → up,
    // long-unmatched → down out of top-k" hold over time.
    promote_skills(
        project_root,
        &picked.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
    );

    picked
}

/// Render the retrieved skills as a worker-prompt block: title + reusable
/// approach, so the base reuses proven material instead of re-deriving it. Empty
/// string when none retrieved (prompt unchanged on first runs). Fail-open.
#[must_use]
pub fn skills_for_prompt(
    project_root: &Path,
    knowledge_dir: &Path,
    solution_idea: &str,
    top_k: usize,
) -> String {
    let skills = retrieve_skills(project_root, knowledge_dir, solution_idea, top_k);
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n## 可复用技能（过往验证通过、可直接借鉴的解法）\n\
         以下能力已通过质量门/契约对照/构建并多次复用，优先沿用其思路：\n",
    );
    for s in &skills {
        out.push_str(&format!(
            "- **{}**（已复用 {} 次）\n  思路：{}\n",
            s.title,
            s.utility(),
            truncate(&s.description, MAX_DESC_CHARS),
        ));
    }
    out
}

/// Convert the legacy [`crate::lessons::LessonKind::ValidatedPattern`] raw
/// ledger into real skill candidates, then graduate them. This is the upgrade
/// path: the existing `capture_validated_patterns` already wrote
/// `validated-decisions.jsonl` at delivery; here we promote those entries into
/// the skill library when the run graduated (gate passed + multi-step).
///
/// `passed_gate` comes from the delivery caller. `description` is the optional
/// base-generated card (empty → template). Returns the number of skills
/// admitted. Fail-open.
pub fn graduate_validated_patterns(
    project_root: &Path,
    description: &str,
    passed_gate: bool,
) -> usize {
    let patterns: Vec<Lesson> =
        crate::lessons::read_raw_lessons(project_root, "validated-decisions.jsonl")
            .into_iter()
            .filter(|l| l.kind == LessonKind::ValidatedPattern)
            .collect();
    if patterns.is_empty() {
        return 0;
    }
    let mut admitted = 0usize;
    for p in &patterns {
        // The validated pattern's body IS the reusable material (the endpoint
        // decomposition that cleared the gate); its fix line is the one-line
        // reuse hint. Use the body as content so retrieval matches on substance.
        let content = if p.body.trim().is_empty() {
            p.fix.clone()
        } else {
            p.body.clone()
        };
        if graduate_skill(
            project_root,
            &p.title,
            &content,
            description,
            &p.domain,
            &p.keywords,
            &p.source_requirement,
            passed_gate,
        ) {
            admitted += 1;
        }
    }
    admitted
}

/// A language-neutral view of the skill library for reporting (`umadev lessons`
/// / a TUI panel can add chrome). Pure read; fail-open.
#[must_use]
pub fn skills_report(project_root: &Path) -> Vec<Skill> {
    let mut store = read_skills(project_root);
    // Most-useful, most-recent first.
    let now = Utc::now();
    store.sort_by(|a, b| {
        skill_decay_score(b, now)
            .partial_cmp(&skill_decay_score(a, now))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.last_validated.cmp(&a.last_validated))
    });
    store
}

// =====================================================================
// Store I/O — JSONL ledger + indexed markdown mirror.
// =====================================================================

/// Read the skill store. Empty on missing/malformed (fail-open).
#[must_use]
pub fn read_skills(project_root: &Path) -> Vec<Skill> {
    let path = project_root.join(SKILLS_DIR).join(SKILLS_FILE);
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Skill>(l).ok())
        .collect()
}

/// Overwrite the skill store JSONL. Fail-open.
fn write_skills(project_root: &Path, skills: &[Skill]) {
    let dir = project_root.join(SKILLS_DIR);
    let _ = fs::create_dir_all(&dir);
    let mut buf = String::new();
    for s in skills {
        if let Ok(line) = serde_json::to_string(s) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    let _ = fs::write(dir.join(SKILLS_FILE), buf);
}

/// Rewrite the indexed markdown mirror under `.umadev/learned/skills/` so it
/// reflects exactly the current store (writes survivors, removes evicted). This
/// is what the existing BM25/vector index retrieves — no new index machinery.
/// Invalidates the kb cache so a retrieval later in THIS run sees the change.
/// Fail-open.
fn mirror_skills_to_index(project_root: &Path, store: &[Skill]) {
    let dir = project_root.join(SKILLS_LEARNED_SUBDIR);
    let _ = fs::create_dir_all(&dir);
    let keep: std::collections::HashSet<String> =
        store.iter().map(|s| format!("{}.md", s.id)).collect();
    // Drop mirror files for skills that no longer exist (retired/evicted).
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("md") {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or_default();
                if !keep.contains(name) {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }
    for s in store {
        let _ = fs::write(dir.join(format!("{}.md", s.id)), render_skill_markdown(s));
    }
    // The BM25 index is content-hash cached; force a rebuild so the just-written
    // skills are retrievable within this same run.
    umadev_knowledge::invalidate_cache(project_root);
}

/// Render a skill as a markdown knowledge file the chunker understands: YAML
/// front-matter tags, H1 title, then sections. Keywords are embedded in the body
/// so BM25 can find them (front-matter tags alone are not indexed). The filename
/// carries the id (`skills/<id>.md`) so retrieval can join back to the store.
fn render_skill_markdown(s: &Skill) -> String {
    let date: String = s.last_validated.chars().take(10).collect();
    let kw = s.keywords.join(", ");
    format!(
        "---\nid: skill-{id}\ntitle: {title}\ndomain: {domain}\ncategory: skill\ntags: [{tags}]\nmaintainer: auto-skill\nlast_updated: {date}\n---\n\
# [skill] {title}\n\n\
## Approach\n\n{description}\n\n\
Keywords: {kw}\n\n\
## Reusable material\n\n{content}\n",
        id = s.id,
        title = s.title,
        domain = s.domain,
        tags = kw,
        date = date,
        description = s.description,
        kw = kw,
        content = s.content,
    )
}

// =====================================================================
// Decay + retirement.
// =====================================================================

/// Recency weight in `(0, 1]` — `2^(-age_days / halflife)`, identical shape to
/// the lesson recency curve. An unparseable timestamp is treated as "now"
/// (weight 1.0) so a corrupt row is never silently buried (fail-open).
fn skill_recency_weight(last_validated: &str, now: chrono::DateTime<Utc>) -> f64 {
    let age_days = chrono::NaiveDateTime::parse_from_str(last_validated, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|naive| (now - naive.and_utc()).num_seconds() as f64 / 86_400.0)
        .unwrap_or(0.0)
        .max(0.0);
    2.0_f64.powf(-age_days / SKILL_RECENCY_HALFLIFE_DAYS)
}

/// Composite keep/rank score: `utility · recency`. A frequently-reused, recent
/// skill scores high; a once-seen, ancient one tends to 0 and is evicted first.
fn skill_decay_score(s: &Skill, now: chrono::DateTime<Utc>) -> f64 {
    let util = (f64::from(s.utility().min(16)) / 16.0).mul_add(0.9, 0.1); // (0.1, 1.0]
    util * skill_recency_weight(&s.last_validated, now)
}

/// Evict the lowest-value skills when the store exceeds [`MAX_SKILLS`], by the
/// utility·recency decay score (lowest first). Mirrors the pitfall prune's
/// bounded-store discipline.
fn retire_skills(store: &mut Vec<Skill>) {
    if store.len() <= MAX_SKILLS {
        return;
    }
    let now = Utc::now();
    store.sort_by(|a, b| {
        skill_decay_score(b, now)
            .partial_cmp(&skill_decay_score(a, now))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.last_validated.cmp(&a.last_validated))
    });
    store.truncate(MAX_SKILLS);
}

/// Promote the named skills: utility++ and recency refreshed, because surfacing
/// a skill for reuse is itself a use. Rewrites store + mirror. Fail-open.
fn promote_skills(project_root: &Path, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    static SKILL_KB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SKILL_KB_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut store = read_skills(project_root);
    if store.is_empty() {
        return;
    }
    let want: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut changed = false;
    for s in &mut store {
        if want.contains(s.id.as_str()) {
            s.utility = s.utility().saturating_add(1);
            s.last_validated.clone_from(&now);
            changed = true;
        }
    }
    if changed {
        write_skills(project_root, &store);
        mirror_skills_to_index(project_root, &store);
    }
}

// =====================================================================
// Small helpers.
// =====================================================================

/// A deterministic ≤6-sentence-ish description used when no base reply exists —
/// keeps "底座生成" optional so a missing base never blocks graduation.
fn template_description(title: &str, domain: &str, requirement: &str) -> String {
    truncate(
        &format!(
            "Reusable {domain} approach distilled from a validated solution: {title}. \
             It cleared the quality gate / contract cross-check and build on a real, \
             multi-step task ({req}). Adapt the recorded material to a similar \
             requirement instead of re-deriving the approach.",
            req = truncate(requirement, 120),
        ),
        MAX_DESC_CHARS,
    )
}

/// Slugify a title into a filesystem- and id-safe key.
fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in title.trim().to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    truncate(&s, 80)
}

/// Recover a skill id from a mirror chunk path (`skills/<id>.md`, possibly with a
/// `learned/` prefix already stripped by the index). `None` for non-skill paths.
fn skill_id_from_path(path: &str) -> Option<String> {
    // The index strips the `.umadev/learned/` prefix, so a skill chunk's path is
    // `skills/<id>.md`. Be lenient: match the `skills/` segment anywhere.
    let after = path.rsplit_once("skills/").map(|(_, rest)| rest)?;
    let file = after.split('/').next_back().unwrap_or(after);
    let id = file.strip_suffix(".md").unwrap_or(file);
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Deduplicate keyword list, capped.
fn dedup_keywords(kws: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in kws {
        let k = k.trim().to_string();
        if !k.is_empty() && !out.contains(&k) {
            out.push(k);
        }
        if out.len() >= 20 {
            break;
        }
    }
    out
}

/// Merge new keywords into an existing list (deduped, capped).
fn merge_keywords(dst: &mut Vec<String>, incoming: &[String]) {
    for k in incoming {
        if dst.len() >= 20 {
            break;
        }
        let k = k.trim().to_string();
        if !k.is_empty() && !dst.contains(&k) {
            dst.push(k);
        }
    }
}

/// Truncate a string to `max` chars with an ellipsis (char-safe).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Seed a multi-step signal so the graduation gate's `was_multi_step` arm
    /// passes (a real solve leaves a quality-failure / revision / dev-error).
    fn seed_multi_step(root: &Path) {
        crate::lessons::capture_quality_failures(
            root,
            &[crate::phases::QualityCheck {
                name: "API URL consistency".into(),
                category: "contract".into(),
                description: "t".into(),
                status: "failed".into(),
                score: 30,
                details: "d".into(),
                weight: 2.0,
            }],
            "demo",
            "需求",
        );
    }

    #[test]
    fn graduation_gate_rejects_one_pass_and_failed_runs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let kws = vec!["api".to_string(), "rest".to_string()];

        // 1. A clean one-pass run (no ledgers) is NOT multi-step → rejected even
        //    when the gate passed.
        assert!(!graduate_skill(
            root,
            "REST contract",
            "GET /api/x ...",
            "",
            "api",
            &kws,
            "需求",
            true,
        ));
        assert!(read_skills(root).is_empty());

        // 2. A multi-step run whose gate FAILED is rejected (gate is mandatory).
        seed_multi_step(root);
        assert!(!graduate_skill(
            root,
            "REST contract",
            "GET /api/x ...",
            "",
            "api",
            &kws,
            "需求",
            false,
        ));
        assert!(read_skills(root).is_empty());

        // 3. Multi-step AND gate passed → admitted.
        assert!(graduate_skill(
            root,
            "REST contract",
            "GET /api/x ...",
            "",
            "api",
            &kws,
            "需求",
            true,
        ));
        let store = read_skills(root);
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].utility(), 1);
        // A template description was filled in (base call optional).
        assert!(!store[0].description.trim().is_empty());
        // The indexed mirror exists so retrieval can see it.
        assert!(root
            .join(SKILLS_LEARNED_SUBDIR)
            .join(format!("{}.md", store[0].id))
            .is_file());
    }

    #[test]
    fn empty_content_is_rejected() {
        let tmp = TempDir::new().unwrap();
        seed_multi_step(tmp.path());
        assert!(!graduate_skill(
            tmp.path(),
            "x",
            "   ",
            "",
            "api",
            &[],
            "需求",
            true
        ));
        assert!(read_skills(tmp.path()).is_empty());
    }

    #[test]
    fn re_graduation_bumps_utility_and_refreshes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        seed_multi_step(root);
        let kws = vec!["api".to_string()];
        assert!(graduate_skill(
            root,
            "REST contract",
            "v1",
            "",
            "api",
            &kws,
            "r",
            true
        ));
        assert!(graduate_skill(
            root,
            "REST contract",
            "v2",
            "",
            "api",
            &kws,
            "r",
            true
        ));
        let store = read_skills(root);
        assert_eq!(store.len(), 1, "same title dedups to one skill");
        assert_eq!(store[0].utility(), 2, "re-graduation bumps utility");
        assert_eq!(store[0].content, "v2", "content refreshed");
    }

    #[test]
    fn retrieve_top_k_matches_by_solution_idea_and_promotes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // An empty knowledge dir is fine — retrieval indexes the learned/ mirror.
        let kdir = root.join("knowledge");
        std::fs::create_dir_all(&kdir).unwrap();
        seed_multi_step(root);

        assert!(graduate_skill(
            root,
            "Validated REST contract for blog API",
            "GET /api/articles returns a paginated list; POST creates with an id; \
             auth via bearer token.",
            "",
            "api",
            &[
                "api".to_string(),
                "articles".to_string(),
                "rest".to_string()
            ],
            "做一个博客系统",
            true,
        ));
        // A DIFFERENT skill that should NOT match an article-list query.
        assert!(graduate_skill(
            root,
            "Dark mode design tokens",
            "Define --color-bg per prefers-color-scheme; toggle on root attribute.",
            "",
            "frontend",
            &["dark".to_string(), "tokens".to_string(), "css".to_string()],
            "做一个博客系统",
            true,
        ));

        let before = read_skills(root)
            .into_iter()
            .find(|s| s.title.contains("REST"))
            .unwrap()
            .utility();

        // Query by the SOLUTION IDEA, not the bare task.
        let hits = retrieve_skills(
            root,
            &kdir,
            "expose a paginated REST endpoint that lists articles",
            3,
        );
        assert!(!hits.is_empty(), "a matching skill must be retrieved");
        assert_eq!(
            hits[0].title, "Validated REST contract for blog API",
            "the on-topic skill ranks first"
        );

        // Retrieval promoted the surfaced skill (utility refreshed upward).
        let after = read_skills(root)
            .into_iter()
            .find(|s| s.title.contains("REST"))
            .unwrap()
            .utility();
        assert!(
            after > before,
            "retrieval promotes the reused skill: {after} > {before}"
        );
    }

    #[test]
    fn retire_evicts_lowest_utility_when_over_cap() {
        let now = Utc::now();
        let recent = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let ancient = (now - chrono::Duration::days(400))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let mut store: Vec<Skill> = Vec::new();
        // A recent, high-utility skill we MUST keep.
        store.push(Skill {
            id: "keep".into(),
            title: "KEEP".into(),
            description: "d".into(),
            content: "c".into(),
            keywords: vec![],
            domain: "api".into(),
            source_requirement: "r".into(),
            last_validated: recent,
            utility: 12,
        });
        // Fill past the cap with ancient, low-utility skills.
        for n in 0..MAX_SKILLS + 10 {
            store.push(Skill {
                id: format!("old-{n}"),
                title: format!("old-{n}"),
                description: "d".into(),
                content: "c".into(),
                keywords: vec![],
                domain: "api".into(),
                source_requirement: "r".into(),
                last_validated: ancient.clone(),
                utility: 1,
            });
        }
        retire_skills(&mut store);
        assert!(store.len() <= MAX_SKILLS);
        assert!(
            store.iter().any(|s| s.id == "keep"),
            "a recent high-utility skill survives eviction of stale low-utility ones"
        );
    }

    #[test]
    fn graduate_validated_patterns_upgrades_legacy_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        seed_multi_step(root);
        // The delivery hook already wrote a ValidatedPattern via the existing
        // capture_validated_patterns path.
        let spec = umadev_contract::parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | /api/articles | - | - | none | List |\n",
            "demo",
        );
        crate::lessons::capture_validated_patterns(root, "demo", "做一个博客", &spec);

        let n = graduate_validated_patterns(root, "", true);
        assert_eq!(n, 1, "the validated pattern graduates into one skill");
        let store = read_skills(root);
        assert_eq!(store.len(), 1);
        assert!(store[0].content.contains("/api/articles"));

        // A FAILED gate does not graduate.
        let tmp2 = TempDir::new().unwrap();
        seed_multi_step(tmp2.path());
        crate::lessons::capture_validated_patterns(tmp2.path(), "demo", "做一个博客", &spec);
        assert_eq!(graduate_validated_patterns(tmp2.path(), "", false), 0);
        assert!(read_skills(tmp2.path()).is_empty());
    }

    #[test]
    fn slug_and_id_roundtrip() {
        assert_eq!(
            slug("Validated REST contract for blog API"),
            "validated-rest-contract-for-blog-api"
        );
        assert_eq!(
            skill_id_from_path("skills/validated-rest-contract.md").as_deref(),
            Some("validated-rest-contract")
        );
        assert_eq!(
            skill_id_from_path(".umadev/learned/skills/foo.md").as_deref(),
            Some("foo")
        );
        assert_eq!(skill_id_from_path("api/lesson-api-1.md"), None);
    }

    #[test]
    fn was_multi_step_false_for_clean_workspace() {
        let tmp = TempDir::new().unwrap();
        assert!(!was_multi_step(tmp.path()));
    }
}
