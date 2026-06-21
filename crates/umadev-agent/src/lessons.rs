//! Auto-sediment capture layer — turns development experience into persistent
//! knowledge that makes the tool stronger with every run.
//!
//! Until 4.8 UmaDev was stateless across runs: quality-gate failures were
//! overwritten, gate-revision feedback was consumed once then discarded, and
//! the `.umadev/decisions/` directory the spec promised was never written
//! to. This module closes that loop:
//!
//! - [`capture_quality_failures`] — appends every failed/warning quality
//!   check to `.umadev/learned/_raw/quality-failures.jsonl`.
//! - [`capture_gate_revision`] — writes a real ADR (Architecture Decision
//!   Record) to `.umadev/decisions/<gate>-<ts>.md`, fulfilling the spec's
//!   long-standing empty promise. Also appends a raw lesson.
//! - [`capture_validated_patterns`] — records schemas/decisions that passed
//!   the quality gate, so future runs can reuse proven patterns.
//!
//! All captures are fail-open: a write error is logged but never blocks the
//! pipeline. The raw JSONL files are consumed by [`sediment_lessons`] (step
//! 2) which turns them into retrievable markdown.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::phases::QualityCheck;
use umadev_contract::ApiSpec;

/// Where raw captured experience lives (before sediment turns it into .md).
pub const RAW_DIR: &str = ".umadev/learned/_raw";
/// Where the ADR (decision) records live — read by the proof-pack.
pub const DECISIONS_DIR: &str = ".umadev/decisions";
/// Where sedimented markdown lessons live (project-level).
pub const LEARNED_DIR: &str = ".umadev/learned";
/// Where global (cross-project) lessons live, under the user's home.
pub const GLOBAL_LEARNED_DIRNAME: &str = ".umadev/learned";
/// Raw JSONL file holding captured development errors (the "踩坑" log).
pub const DEV_ERRORS_FILE: &str = "dev-errors.jsonl";

/// The kind of captured experience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonKind {
    /// A quality-gate check failed or warned.
    Failure,
    /// A human revision request at a gate (what the user wanted changed).
    Revision,
    /// A pattern that passed the quality gate (positive experience).
    ValidatedPattern,
    /// A real development error hit during a run — a failed tool call, a
    /// non-zero build/test exit, a runtime stack trace — recognised by
    /// [`crate::error_kb`] and distilled into an avoid-next-time lesson.
    DevError,
}

/// One captured lesson — written to raw JSONL, later sedimented to .md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    /// What kind of experience.
    pub kind: LessonKind,
    /// Domain directory (api, database, frontend, ...). Derived from the
    /// requirement entities so the sedimented file lands in the right place.
    pub domain: String,
    /// Short, human-readable title (becomes the H1 + operationId).
    pub title: String,
    /// Detailed body — symptom, fix, root cause. Keywords that future BM25
    /// queries should match MUST appear in this text (tags alone aren't
    /// indexed by BM25).
    pub body: String,
    /// The actionable fix / recommendation.
    pub fix: String,
    /// The root-cause explanation.
    pub root_cause: String,
    /// Search keywords (also embedded in body for BM25 discoverability).
    pub keywords: Vec<String>,
    /// The requirement that triggered this lesson.
    pub source_requirement: String,
    /// ISO-8601 UTC timestamp when first seen.
    pub first_seen: String,
    /// Stable dedup signature (populated for [`LessonKind::DevError`] from
    /// [`crate::error_kb::ErrorInsight::signature`]; empty for older kinds that
    /// dedup by `(domain, title)`). `#[serde(default)]` keeps pre-existing
    /// JSONL rows (written before this field existed) readable.
    #[serde(default)]
    pub signature: String,
    /// How many times this exact pitfall has been hit (across phases/runs).
    /// Recurrences increment this instead of being dropped, so the KB knows
    /// what bites *repeatedly* — frequency drives recall priority. Stored 0 in
    /// legacy rows; treat as ≥1 via [`Lesson::hits`].
    #[serde(default)]
    pub occurrences: u32,
    /// Tech-stack context present when the pitfall was hit (e.g. `react`,
    /// `vite`, `typescript`, `axum`). This is the *trigger fingerprint*: a
    /// pitfall fires next time only when the current project's context
    /// intersects it — precise, prose-independent triggering.
    #[serde(default)]
    pub context: Vec<String>,
    /// Efficacy tracking for `DevError` pitfalls — closes the loop on whether
    /// the recorded fix actually achieved "一次过". `None` for non-pitfall
    /// kinds and for pitfalls never yet surfaced to the worker.
    #[serde(default)]
    pub efficacy: Option<PitfallEfficacy>,
}

/// Tracks whether a pitfall's fix actually works once we start warning about it.
///
/// The mechanism is self-contained per record (no global run counter): each
/// time the pitfall is surfaced into a worker prompt we snapshot its hit count
/// in [`Self::occ_at_injection`]. If the count later grows, the warning failed
/// to prevent recurrence ([`Self::recurred_after_warning`]); if it stays flat
/// across a later injection, the fix is working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PitfallEfficacy {
    /// How many times this pitfall has been surfaced to the worker as a warning.
    pub injected: u32,
    /// Hit count at the moment of the last injection — the baseline we compare
    /// against to detect "recurred despite being warned about".
    pub occ_at_injection: u32,
    /// `true` once the pitfall recurred AFTER having been warned about, i.e. the
    /// recorded fix is insufficient and needs to be escalated.
    pub recurred_after_warning: bool,
    /// `true` once an in-run auto-fix made the build/test pass again — direct,
    /// immediate proof the recorded fix works (vs. inferring it from the
    /// absence of recurrence over later runs).
    #[serde(default)]
    pub proven_fix: bool,
}

/// Lifecycle of a pitfall's fix, derived from its efficacy record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PitfallStatus {
    /// Newly recorded, fix unproven (never surfaced, or surfaced this round).
    Active,
    /// Warned about and did NOT recur since — the fix is working ("一次过").
    Validated,
    /// Recurred despite being warned about — the fix is insufficient, escalate.
    Recurring,
}

impl Lesson {
    /// Occurrence count, normalised so legacy rows (stored 0) count as 1.
    #[must_use]
    pub fn hits(&self) -> u32 {
        self.occurrences.max(1)
    }

    /// `true` when this is a precisely-recognised pitfall (a classified error
    /// family, not the `general/error/...` generic fallback). Recognised
    /// pitfalls are higher-trust for triggering and global promotion.
    #[must_use]
    pub fn is_recognized(&self) -> bool {
        !self.signature.is_empty() && !self.signature.starts_with("general/")
    }

    /// Derive the fix lifecycle from the efficacy record.
    #[must_use]
    pub fn pitfall_status(&self) -> PitfallStatus {
        match &self.efficacy {
            Some(e) if e.recurred_after_warning => PitfallStatus::Recurring,
            // Direct proof: an in-run fix made the build pass again.
            Some(e) if e.proven_fix => PitfallStatus::Validated,
            // Inferred: survived a full inject→run→inject cycle (≥2 warnings)
            // with no recurrence — so a single optimistic warning never
            // prematurely damps a pitfall that hasn't truly been beaten.
            Some(e) if e.injected >= 2 && self.hits() <= e.occ_at_injection => {
                PitfallStatus::Validated
            }
            _ => PitfallStatus::Active,
        }
    }
}

/// Capture quality-gate failures + warnings as raw lessons.
///
/// Called at the end of `run_quality`. Writes one JSONL line per failed or
/// warning check to `RAW_DIR/quality-failures.jsonl`. Fail-open: any I/O
/// error is silently ignored.
pub fn capture_quality_failures(
    project_root: &Path,
    checks: &[QualityCheck],
    slug: &str,
    requirement: &str,
) {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut lessons: Vec<Lesson> = Vec::new();
    for check in checks
        .iter()
        .filter(|c| c.status == "failed" || c.status == "warning")
    {
        let domain = domain_for_check(&check.name);
        let keywords = extract_keywords(&check.name, &check.details, requirement);
        lessons.push(Lesson {
            kind: LessonKind::Failure,
            domain: domain.clone(),
            title: format!("Quality gate: {} ({})", check.name, check.status),
            body: format!(
                "During the {slug} run, the quality check \"{name}\" scored {score}/100 \
                 with status {status}.\n\nDetails: {details}\n\nRequirement: {requirement}",
                slug = slug,
                name = check.name,
                score = check.score,
                status = check.status,
                details = check.details,
                requirement = requirement,
            ),
            fix: fix_suggestion_for_check(&check.name),
            root_cause: format!(
                "The {} check scored {}/100 (status: {}). This is a {} issue — {}",
                check.name,
                check.score,
                check.status,
                if check.status == "failed" {
                    "blocking"
                } else {
                    "quality"
                },
                if check.score < 40 {
                    "the artifact is substantially incomplete"
                } else if check.score < 70 {
                    "the artifact is partially complete"
                } else {
                    "the artifact is mostly complete but needs polish"
                }
            ),
            keywords: keywords.clone(),
            source_requirement: requirement.to_string(),
            first_seen: now.clone(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: None,
        });
    }
    append_raw_lessons(project_root, "quality-failures.jsonl", &lessons);
}

/// Capture a gate revision as both an ADR record AND a raw lesson.
///
/// Called from `cmd_revise`. Writes a real ADR markdown file to
/// `DECISIONS_DIR/<gate>-<timestamp>.md` (fulfilling the spec's promise),
/// then appends a Revision lesson to the raw ledger.
pub fn capture_gate_revision(
    project_root: &Path,
    gate: &str,
    revision_text: &str,
    requirement: &str,
) -> PathBuf {
    let now = Utc::now();
    let ts = now.format("%Y%m%dT%H%M%SZ");
    let date = now.format("%Y-%m-%d");

    // 1. Write the ADR (decision record) — fulfills spec §5.4.
    let dec_dir = project_root.join(DECISIONS_DIR);
    let _ = fs::create_dir_all(&dec_dir);
    let adr_path = dec_dir.join(format!("{gate}-{ts}.md"));
    let adr_body = format!(
        "# ADR: {gate} revision\n\n\
         **Date:** {date}\n\n\
         **Status:** Revised\n\n\
         **Requirement:** {requirement}\n\n\
         ## Decision\n\n\
         The user requested the following revision at the {gate} gate:\n\n\
         > {revision_text}\n\n\
         ## Context\n\n\
         This revision feedback is captured as a decision record so future runs \
         of the pipeline understand why the artifacts changed at this gate. The \
         underlying worker will regenerate the block with this feedback folded \
         into the requirement.\n",
    );
    let _ = fs::write(&adr_path, adr_body);

    // 2. Append a raw Revision lesson.
    let domain = if gate.contains("docs") {
        "docs"
    } else {
        "frontend"
    };
    let keywords = extract_keywords(gate, revision_text, requirement);
    let lesson = Lesson {
        kind: LessonKind::Revision,
        domain: domain.to_string(),
        title: format!("{gate} revision: {}", truncate(revision_text, 80)),
        body: format!(
            "At the {gate} gate, the user revised with: \"{revision_text}\".\n\n\
             This indicates the generated artifacts did not meet expectations in \
             this area. The worker should address this feedback directly.\n\n\
             Requirement context: {requirement}",
        ),
        fix: format!("Address the revision feedback: {revision_text}"),
        root_cause: "The generated artifact did not meet the user's expectations at this gate."
            .to_string(),
        keywords,
        source_requirement: requirement.to_string(),
        first_seen: now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        signature: String::new(),
        occurrences: 1,
        context: Vec::new(),
        efficacy: None,
    };
    append_raw_lessons(project_root, "gate-revisions.jsonl", &[lesson]);

    adr_path
}

/// Capture validated patterns (schemas/decisions that passed the gate) as
/// positive experience. Called at delivery completion.
pub fn capture_validated_patterns(
    project_root: &Path,
    slug: &str,
    requirement: &str,
    spec: &ApiSpec,
) {
    if spec.is_empty() {
        return;
    }
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let entity_summary = spec
        .declared_paths()
        .iter()
        .map(|(_, p)| (*p).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let keywords = extract_keywords(slug, &entity_summary, requirement);
    let lesson = Lesson {
        kind: LessonKind::ValidatedPattern,
        domain: "api".to_string(),
        title: format!("Validated API contract for {slug}"),
        body: format!(
            "The {slug} run produced a validated OpenAPI contract with these endpoints:\n\
             {entity_summary}\n\n\
             This schema passed the quality gate. Reuse this entity decomposition \
             for similar requirements.\n\nRequirement: {requirement}",
        ),
        fix: "Reuse this proven entity decomposition for similar projects.".to_string(),
        root_cause: "This contract was generated from the requirement and validated.".to_string(),
        keywords,
        source_requirement: requirement.to_string(),
        first_seen: now,
        signature: String::new(),
        occurrences: 1,
        context: Vec::new(),
        efficacy: None,
    };
    append_raw_lessons(project_root, "validated-decisions.jsonl", &[lesson]);
}

/// Capture real development errors hit during a run into the lessons KB.
///
/// Each raw error string (a failed tool-call summary, a non-zero build/test
/// stderr, a runtime stack trace) is recognised by [`crate::error_kb`] and
/// distilled into a [`LessonKind::DevError`] lesson. Deduped by
/// [`crate::error_kb::ErrorInsight::signature`] — both within this batch and
/// against already-captured dev errors — so the SAME pitfall is recorded once,
/// not once per occurrence. Returns the number of NEW lessons written.
///
/// Fail-open: any I/O error is swallowed and the pipeline continues.
pub fn capture_dev_errors(
    project_root: &Path,
    raw_errors: &[String],
    slug: &str,
    requirement: &str,
) -> usize {
    // Process-wide lock serializing this KB read-modify-write so concurrent
    // pipeline steps (the parallel docs fan-out's two forked bases) can't
    // clobber each other. Recover from poison so a panic elsewhere never
    // blocks or panics this fail-open path.
    static DEV_KB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _kb_guard = DEV_KB_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    // The tech-stack fingerprint present *right now* — stamped onto each
    // pitfall so triggering can later match "same situation", not prose.
    let context = project_context_tokens(project_root);

    // Read-modify-write: a recurrence bumps `occurrences` on the existing
    // record (and merges any newly-seen context) rather than being dropped, so
    // the KB measures how often each pitfall actually bites.
    let mut store: Vec<Lesson> = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    let mut idx: std::collections::HashMap<String, usize> = store
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.signature.is_empty())
        .map(|(i, l)| (l.signature.clone(), i))
        .collect();

    let mut new_count = 0usize;
    let mut changed = false;
    for raw in raw_errors {
        let text = raw.trim();
        if text.is_empty() || !crate::error_kb::looks_like_error(text) {
            continue;
        }
        let insight = crate::error_kb::classify_error(text);
        if let Some(&i) = idx.get(&insight.signature) {
            // Recurrence → frequency++ and absorb any new context tokens.
            store[i].occurrences = store[i].hits().saturating_add(1);
            merge_tokens(&mut store[i].context, &context, 24);
            // Efficacy: if we had ALREADY warned the worker about this pitfall
            // (it was injected) and it recurred anyway, the recorded fix is
            // insufficient — flag it so recall escalates it next time.
            let occ_now = store[i].occurrences;
            if let Some(eff) = store[i].efficacy.as_mut() {
                // Recurred after we either warned the worker (injected) OR
                // marked an in-run fix as proven — both mean the recorded fix
                // did NOT hold. Without the `proven_fix` arm a pitfall that was
                // auto-fix-validated (injected still 0) would keep reporting as
                // "Validated" even as it bites again every run.
                if (eff.injected >= 1 || eff.proven_fix) && occ_now > eff.occ_at_injection {
                    eff.recurred_after_warning = true;
                    eff.proven_fix = false;
                }
            }
            changed = true;
            continue;
        }
        let mut keywords = insight.keywords.clone();
        for kw in extract_keywords(&insight.title, text, requirement) {
            if !keywords.contains(&kw) {
                keywords.push(kw);
            }
        }
        keywords.truncate(20);
        idx.insert(insight.signature.clone(), store.len());
        store.push(Lesson {
            kind: LessonKind::DevError,
            domain: insight.category.clone(),
            // Title carries the signature so sediment dedups recurrences by
            // (domain, title) too — belt and suspenders with the seen-set.
            title: format!("踩坑 [{}]: {}", insight.signature, insight.title),
            body: format!(
                "During the {slug} run, this error was hit:\n\n{snippet}\n\n\
                 Signature: {sig}\n\nRequirement: {requirement}",
                snippet = truncate(text, 500),
                sig = insight.signature,
            ),
            fix: insight.fix.clone(),
            root_cause: insight.root_cause.clone(),
            keywords,
            source_requirement: requirement.to_string(),
            first_seen: now.clone(),
            signature: insight.signature,
            occurrences: 1,
            context: context.clone(),
            efficacy: None,
        });
        new_count += 1;
        changed = true;
    }

    if changed {
        prune_pitfalls(&mut store);
        write_raw_lessons(project_root, DEV_ERRORS_FILE, &store);
    }
    new_count
}

/// Hard cap on distinct pitfalls kept in `dev-errors.jsonl` so a long-lived
/// commercial repo's KB never bloats. Generous — most projects stay well under.
const MAX_DEV_PITFALLS: usize = 300;

/// Evict the least-valuable pitfalls when the store exceeds [`MAX_DEV_PITFALLS`].
/// Keep priority: still-failing (`Recurring`) > unproven (`Active`) > solved
/// (`Validated`), then most-frequently-hit, then most-recent. Solved pitfalls
/// are dropped first — their fix is proven, so losing the record costs little.
fn prune_pitfalls(store: &mut Vec<Lesson>) {
    if store.len() <= MAX_DEV_PITFALLS {
        return;
    }
    let rank = |l: &Lesson| match l.pitfall_status() {
        PitfallStatus::Recurring => 0u8,
        PitfallStatus::Active => 1,
        PitfallStatus::Validated => 2,
    };
    store.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.hits().cmp(&a.hits()))
            .then_with(|| b.first_seen.cmp(&a.first_seen))
    });
    store.truncate(MAX_DEV_PITFALLS);
}

/// The current project's tech-stack fingerprint: lowercased dependency names
/// from `package.json` (deps + devDeps) and `Cargo.toml` (`[dependencies]`).
///
/// These tokens are the bridge between a recorded pitfall and "right now": a
/// `dependency/module-not-found/react-router-dom` pitfall triggers precisely
/// when `react-router-dom` is in *this* project's manifest, no matter what the
/// natural-language requirement says. Fail-open: missing/unreadable manifests
/// just yield fewer tokens.
#[must_use]
pub fn project_context_tokens(project_root: &Path) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();

    // package.json — parse the dependency maps' keys.
    if let Ok(text) = fs::read_to_string(project_root.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            for field in ["dependencies", "devDependencies", "peerDependencies"] {
                if let Some(map) = json.get(field).and_then(serde_json::Value::as_object) {
                    for name in map.keys() {
                        merge_one_token(&mut tokens, name);
                    }
                }
            }
        }
    }

    // Cargo.toml — line-scan the [dependencies] / [dev-dependencies] tables
    // (no `toml` dep needed for this crate).
    if let Ok(text) = fs::read_to_string(project_root.join("Cargo.toml")) {
        let mut in_deps = false;
        for raw in text.lines() {
            let line = raw.trim();
            if line.starts_with('[') {
                in_deps = line.contains("dependencies");
                continue;
            }
            if in_deps {
                if let Some((name, _)) = line.split_once('=') {
                    merge_one_token(&mut tokens, name.trim());
                }
            }
        }
    }

    tokens.truncate(60);
    tokens
}

/// Slugify + push a context token (deduped). A scoped npm name like
/// `@tanstack/react-query` contributes both the slug and the bare package part.
fn merge_one_token(tokens: &mut Vec<String>, name: &str) {
    let name = name.trim().trim_matches('"');
    if name.is_empty() {
        return;
    }
    let slug = name
        .to_ascii_lowercase()
        .replace(['@', '/', '_', ' ', '"'], "-");
    for tok in slug.split('-').filter(|t| t.len() >= 2) {
        if !tokens.iter().any(|x| x == tok) {
            tokens.push(tok.to_string());
        }
    }
    let full = slug.trim_matches('-').to_string();
    if full.len() >= 3 && !tokens.iter().any(|x| x == &full) {
        tokens.push(full);
    }
}

/// Merge `incoming` tokens into `dst` (deduped), capping at `max`.
fn merge_tokens(dst: &mut Vec<String>, incoming: &[String], max: usize) {
    for t in incoming {
        if dst.len() >= max {
            break;
        }
        if !dst.iter().any(|x| x == t) {
            dst.push(t.clone());
        }
    }
}

/// Overwrite a raw JSONL file with `lessons` (one per line). Fail-open.
/// Used by the read-modify-write dev-error path so per-pitfall frequency stays
/// on a single line instead of growing one line per occurrence.
fn write_raw_lessons(project_root: &Path, filename: &str, lessons: &[Lesson]) {
    let raw_dir = project_root.join(RAW_DIR);
    let _ = fs::create_dir_all(&raw_dir);
    let path = raw_dir.join(filename);
    let mut buf = String::new();
    for lesson in lessons {
        if let Ok(line) = serde_json::to_string(lesson) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    let _ = fs::write(&path, buf);
}

/// Append lessons to a raw JSONL file. Fail-open (best-effort write).
fn append_raw_lessons(project_root: &Path, filename: &str, lessons: &[Lesson]) {
    if lessons.is_empty() {
        return;
    }
    let raw_dir = project_root.join(RAW_DIR);
    let _ = fs::create_dir_all(&raw_dir);
    let path = raw_dir.join(filename);
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        for lesson in lessons {
            if let Ok(line) = serde_json::to_string(lesson) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

/// Read all raw lessons from a file. Returns empty vec on missing/malformed.
#[must_use]
pub fn read_raw_lessons(project_root: &Path, filename: &str) -> Vec<Lesson> {
    let path = project_root.join(RAW_DIR).join(filename);
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Lesson>(l).ok())
        .collect()
}

/// Read ALL raw lessons across all files.
#[must_use]
pub fn read_all_raw_lessons(project_root: &Path) -> Vec<Lesson> {
    let mut all = Vec::new();
    for f in &[
        "quality-failures.jsonl",
        "gate-revisions.jsonl",
        "validated-decisions.jsonl",
        DEV_ERRORS_FILE,
    ] {
        all.extend(read_raw_lessons(project_root, f));
    }
    all
}

/// Map a quality-check name to a domain directory slug.
fn domain_for_check(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.contains("api") || lower.contains("contract") || lower.contains("openapi") {
        "api".to_string()
    } else if lower.contains("color")
        || lower.contains("emoji")
        || lower.contains("design")
        || lower.contains("dark")
        || lower.contains("uiux")
        || lower.contains("ui/")
    {
        "frontend".to_string()
    } else if lower.contains("placeholder") || lower.contains("slop") {
        "governance".to_string()
    } else if lower.contains("ops") || lower.contains("docker") || lower.contains("ci") {
        "devops".to_string()
    } else if lower.contains("architecture") || lower.contains("alignment") {
        "architecture".to_string()
    } else if lower.contains("acceptance") || lower.contains("prd") {
        "product".to_string()
    } else {
        "general".to_string()
    }
}

/// Extract search keywords from text (for BM25 discoverability).
fn extract_keywords(source: &str, details: &str, requirement: &str) -> Vec<String> {
    let mut kws: Vec<String> = Vec::new();
    for text in [source, details, requirement] {
        // ASCII words: split on non-alphanumeric, keep len>=3.
        for word in text.split(|c: char| !c.is_alphanumeric()) {
            let w = word.trim().to_ascii_lowercase();
            if w.len() >= 3 && !kws.contains(&w) {
                kws.push(w);
            }
        }
        // CJK: the split above yields one giant token per CJK run (all CJK
        // chars are alphanumeric), which is useless for BM25 discoverability.
        // Emit CJK unigrams + bigrams so a Chinese requirement like
        // "登录系统" produces "登录" / "系统" / "登录系统" keywords. Mirrors the
        // knowledge crate's tokenizer strategy.
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if is_cjk_char(chars[i]) {
                // unigram
                let uni = chars[i].to_string();
                if !kws.contains(&uni) {
                    kws.push(uni);
                }
                // bigram with next CJK char
                if i + 1 < chars.len() && is_cjk_char(chars[i + 1]) {
                    let bi: String = chars[i..=i + 1].iter().collect();
                    if !kws.contains(&bi) {
                        kws.push(bi);
                    }
                }
            }
            i += 1;
        }
    }
    kws.truncate(20);
    kws
}

/// Whether a char is in the common CJK unified ideograph ranges (same set
/// the knowledge tokenizer uses). Inline copy to avoid a cross-crate dep.
fn is_cjk_char(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF
        | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

/// Generate an actionable fix suggestion based on the check name.
fn fix_suggestion_for_check(name: &str) -> String {
    let l = name.to_ascii_lowercase();
    if l.contains("placeholder") {
        "Replace EVERY TODO/placeholder marker with real content. Use --backend claude-code so the worker fills in actual requirements and API details.".to_string()
    } else if l.contains("conformance") {
        "Ensure every frontend fetch/axios call hits a declared OpenAPI endpoint with the correct method. Run the contract validator before submitting.".to_string()
    } else if l.contains("openapi") || l.contains("contract") {
        "Generate .umadev/contracts/openapi.yaml from the architecture API table. Verify frontend calls map to declared endpoints (method + path templates).".to_string()
    } else if l.contains("consistency") || l.contains("alignment") {
        "Cross-check three artifacts: PRD routes, OpenAPI paths, and frontend calls must all reference the same entities (e.g. /api/articles).".to_string()
    } else if l.contains("color") {
        "Replace hardcoded hex/rgb/hsl with CSS custom properties (design tokens). Define --color-primary in :root. Only #fff/#000 allowed.".to_string()
    } else if l.contains("emoji") {
        "Replace emoji-as-icons with a declared icon library (Lucide, Heroicons). Emoji in JSX text is blocked.".to_string()
    } else if l.contains("slop") {
        "Remove Lorem ipsum and generic 'Welcome to App' titles. Write real, requirement-specific copy.".to_string()
    } else if l.contains("acceptance") {
        "Write 3+ Given/When/Then criteria per entity: GET returns list, POST creates with id, DELETE removes.".to_string()
    } else if l.contains("discovery") {
        "Add a ## Discovery section: target audience, similar products, design direction. This grounds the PRD.".to_string()
    } else if l.contains("uiux") || l.contains("ui/ux") || l.contains("design system") {
        "Complete the UIUX doc: color tokens, typography, icon set, interactive states (hover/focus/disabled).".to_string()
    } else if l.contains("dark") {
        "Add @media (prefers-color-scheme: dark) overrides for all color tokens. Test both themes."
            .to_string()
    } else if l.contains("ops") {
        "Generate: Dockerfile (multi-stage, non-root), docker-compose (app+postgres), CI workflow (lint+test+quality gate), migrations, .env.example.".to_string()
    } else if l.contains("audit") {
        "Ensure audit JSONL logs are populated. frontend-api-calls.jsonl records every fetch(). tool-calls.jsonl records governance decisions.".to_string()
    } else if l.contains("research") {
        "Enrich research doc: domain risks, similar products, discovery (audience + design direction).".to_string()
    } else if l.contains("prd") {
        "Complete PRD: Goal (what+why+metric), personas, Scope, functional requirements table, acceptance criteria.".to_string()
    } else if l.contains("architecture") {
        "Complete architecture: API surface table, data model (entity field tables), auth method, tech-stack rationale.".to_string()
    } else {
        format!("Address the '{name}' check — see details in quality-gate.json and fix the specific issue.")
    }
}

/// Truncate a string to `max` chars with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

// =====================================================================
// Step 2: Sediment — turn raw JSONL lessons into retrievable markdown.
// =====================================================================

/// Resolve the global learned dir: `~/.umadev/learned/`.
/// Returns None when no home directory can be determined (fail-open).
///
/// Cross-platform: prefers `HOME` (Unix + most shells), falls back to
/// `USERPROFILE` (Windows). Previously only `HOME` was checked, which is
/// usually unset on Windows — so global experience silently never loaded.
#[must_use]
pub fn global_learned_dir() -> Option<PathBuf> {
    let home = home_dir()?;
    let dir = home.join(GLOBAL_LEARNED_DIRNAME);
    // Bootstrap: create the dir so a fresh machine can accumulate global
    // experience (before this fix, promote_to_global silently did nothing
    // on machines where ~/.umadev/learned/ didn't exist yet).
    if dir.is_dir() {
        Some(dir)
    } else {
        let _ = std::fs::create_dir_all(&dir);
        Some(dir)
    }
}

/// Cross-platform home directory: `HOME` then `USERPROFILE` (Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// Sediment all raw lessons into markdown knowledge files under
/// `.umadev/learned/<domain>/`. Each unique `(domain, title)` produces one
/// file (latest wins). Called from `run_quality` after the capture step.
///
/// Total-ordering predicate for sediment dedup: `true` if `a` should be
/// considered "before / less desirable to keep" than `b`. Newer
/// `first_seen` wins; on a same-second tie, the record with the richer
/// `fix` (longer) wins; on a fix-length tie, the lexicographically-larger
/// title wins as a final stable deterministic tiebreak. This makes dedup
/// fully deterministic even when timestamps collide at second resolution.
fn lesson_precedes(a: &Lesson, b: &Lesson) -> bool {
    match a.first_seen.cmp(&b.first_seen) {
        std::cmp::Ordering::Equal => {
            // Same second → compare richness, then title.
            match a.fix.len().cmp(&b.fix.len()) {
                std::cmp::Ordering::Equal => a.title < b.title,
                ord => ord.is_lt(),
            }
        }
        ord => ord.is_lt(),
    }
}

/// Returns the number of markdown files written. Fail-open: errors return 0.
#[must_use]
pub fn sediment_lessons(project_root: &Path) -> usize {
    let lessons = read_all_raw_lessons(project_root);
    if lessons.is_empty() {
        return 0;
    }

    // Dedupe by (domain, title) — keep the latest first_seen. On a
    // same-second tie (first_seen has only second resolution), break
    // deterministically by the richer record: longer `fix` text wins, then
    // lexicographically-greater `title` as a final stable tiebreak. This
    // replaces the previous `existing.first_seen >= lesson.first_seen`
    // guard, which on equal timestamps kept whichever happened to iterate
    // first — deterministic given a fixed Vec order, but with no signal
    // that the kept record was actually the "latest" content.
    let mut by_key: std::collections::HashMap<String, &Lesson> = std::collections::HashMap::new();
    for lesson in &lessons {
        let key = format!("{}::{}", lesson.domain, lesson.title);
        match by_key.get(&key) {
            Some(existing) if lesson_precedes(lesson, existing) => {}
            _ => {
                by_key.insert(key, lesson);
            }
        }
    }

    let learned_root = project_root.join(LEARNED_DIR);
    let _ = fs::create_dir_all(&learned_root);
    let mut written = 0usize;
    let mut seq_by_domain: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for lesson in by_key.values() {
        let domain_dir = learned_root.join(&lesson.domain);
        let _ = fs::create_dir_all(&domain_dir);
        let seq = seq_by_domain.entry(lesson.domain.clone()).or_insert(0);
        *seq += 1;
        let path = domain_dir.join(format!("lesson-{domain}-{seq}.md", domain = lesson.domain));
        let body = render_lesson_markdown(lesson);
        if fs::write(&path, body).is_ok() {
            written += 1;
        }
    }

    // Promote frequently-occurring lessons to the global dir.
    let _ = promote_to_global(project_root, &lessons);

    written
}

/// Render a Lesson as a markdown knowledge file matching the chunker's
/// expectations: YAML front-matter (tags), H1 title, H2 sections (症状/修复/原因).
/// Keywords are deliberately embedded in the body text so BM25 can find them
/// (front-matter tags alone are NOT indexed).
fn render_lesson_markdown(lesson: &Lesson) -> String {
    // char-safe — `first_seen` is normally an ASCII timestamp, but lessons are
    // read back from hand-editable JSONL; byte-slicing a corrupted multibyte
    // value at index 10 would panic and break the fail-open sediment contract.
    let date: String = lesson.first_seen.chars().take(10).collect();
    let kind_label = match lesson.kind {
        LessonKind::Failure => "[warn] Failure",
        LessonKind::Revision => "[write] Revision",
        LessonKind::ValidatedPattern => "[ok] Validated pattern",
        LessonKind::DevError => "[pitfall] Dev error",
    };
    let keywords_inline = lesson.keywords.join(", ");
    format!(
        "---\nid: lesson-{domain}\ntitle: {title}\ndomain: {domain}\ncategory: learned\ntags: [{tags}]\nmaintainer: auto-sediment\nlast_updated: {date}\n---\n\
# {kind_label}: {title}\n\n\
## Symptom\n\n{body}\n\n\
Keywords: {keywords_inline}\n\n\
## Fix\n\n{fix}\n\n\
## Root cause\n\n{root_cause}\n",
        domain = lesson.domain,
        title = lesson.title,
        tags = lesson.keywords.join(", "),
        date = date,
        kind_label = kind_label,
        body = lesson.body,
        keywords_inline = keywords_inline,
        fix = lesson.fix,
        root_cause = lesson.root_cause,
    )
}

/// Whether a lesson group is general enough to share across ALL projects.
///
/// Two routes to "global-worthy":
/// - the same `(domain, title)` recurred across ≥2 distinct requirements
///   (the original signal — a pattern, not a one-off), OR
/// - it's a **recognised development error** (a `DevError` whose signature is
///   not the generic `general/error/...` fallback). A classified technical
///   pitfall — `cannot find module`, a CORS block, a type mismatch — is
///   inherently cross-project knowledge, so it promotes on first sight. This is
///   what makes "next time, first try" work even in a brand-new project.
fn group_is_global_worthy(group: &[&Lesson], distinct_reqs: usize) -> bool {
    if distinct_reqs >= 2 {
        return true;
    }
    group.iter().any(|l| {
        l.kind == LessonKind::DevError
            && !l.signature.is_empty()
            && !l.signature.starts_with("general/")
    })
}

/// Promote lessons that appear across multiple distinct requirements to the
/// global `~/.umadev/learned/` dir, so all projects benefit. A lesson is
/// "global-worthy" if its domain+title appears with ≥2 different source
/// requirements (indicating it's a general pattern, not project-specific), or
/// if it's a recognised development error (see [`group_is_global_worthy`]).
fn promote_to_global(_project_root: &Path, lessons: &[Lesson]) -> usize {
    let Some(global_dir) = global_learned_dir() else {
        return 0; // HOME unset or dir doesn't exist yet — skip.
    };

    // Group by (domain, title) and count distinct requirements.
    let mut groups: std::collections::HashMap<String, Vec<&Lesson>> =
        std::collections::HashMap::new();
    for lesson in lessons {
        let key = format!("{}::{}", lesson.domain, lesson.title);
        groups.entry(key).or_default().push(lesson);
    }

    let mut promoted = 0usize;
    for (key, group) in &groups {
        let distinct_reqs: std::collections::HashSet<&str> = group
            .iter()
            .map(|l| l.source_requirement.as_str())
            .collect();
        if !group_is_global_worthy(group, distinct_reqs.len()) {
            continue; // one-off, project-specific — not general enough.
        }
        // Promote the latest lesson in this group. Use the deterministic
        // total-order from lesson_precedes (first_seen → fix length → title)
        // so same-second timestamps don't make the choice non-deterministic
        // (matches the sediment_lessons dedup policy).
        let latest = group
            .iter()
            .copied()
            .reduce(|acc, l| if lesson_precedes(acc, l) { l } else { acc });
        if let Some(lesson) = latest {
            let dir = global_dir.join(&lesson.domain);
            let _ = fs::create_dir_all(&dir);
            let slug = key.replace("::", "-").replace(' ', "-");
            let path = dir.join(format!("{slug}.md"));
            let body = render_lesson_markdown(lesson);
            if fs::write(&path, body).is_ok() {
                promoted += 1;
            }
        }
    }
    promoted
}

/// List all sedimented lesson files (project + global), for reporting.
#[must_use]
pub fn list_sedimented_lessons(project_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let project_learned = project_root.join(LEARNED_DIR);
    if project_learned.is_dir() {
        collect_md_files(&project_learned, &mut files);
    }
    if let Some(global) = global_learned_dir() {
        collect_md_files(&global, &mut files);
    }
    files
}

fn collect_md_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            // Skip the _raw dir (raw JSONL, not retrievable markdown).
            if p.file_name().is_some_and(|n| n == "_raw") {
                continue;
            }
            collect_md_files(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(p);
        }
    }
}

// =====================================================================
// Step 4: Feed back — render lessons into the coach prompt.
// =====================================================================

/// Score how strongly a lesson's situation matches the current trigger query.
///
/// The signals, in priority order:
/// - **discriminator hit** (weight 6) — the pitfall's offending symbol (the
///   last signature segment, e.g. `react-router-dom`) is present in the current
///   project's stack. This is the precise "this exact thing is in play now".
/// - **context overlap** — the stack present when the pitfall was hit overlaps
///   the current stack (same framework family).
/// - **keyword overlap** — classic term match against requirement + stack.
/// - **recognised bonus** (+1) — a classified pitfall outranks a generic one.
/// - **frequency** (+min(hits,5)) — pitfalls that bite repeatedly rank higher,
///   but only once the lesson already matched (so frequency never pulls in an
///   irrelevant lesson on its own).
fn lesson_trigger_score(l: &Lesson, query: &std::collections::HashSet<String>) -> i64 {
    let mut score: i64 = 0;
    score += l
        .keywords
        .iter()
        .filter(|k| query.contains(k.as_str()))
        .count() as i64;
    score += l
        .context
        .iter()
        .filter(|c| query.contains(c.as_str()))
        .count() as i64;
    if l.kind == LessonKind::DevError {
        if let Some(disc) = l.signature.rsplit('/').next() {
            if !disc.is_empty() && query.contains(disc) {
                score += 6;
            }
        }
        if l.is_recognized() {
            score += 1;
        }
        // Efficacy steering: a pitfall that recurred DESPITE being warned about
        // gets escalated (its fix is failing — surface it hard); one whose fix
        // is proven (validated) is damped so it stops crowding the prompt once
        // it's reliably handled.
        match l.pitfall_status() {
            PitfallStatus::Recurring => score += 8,
            PitfallStatus::Validated => score -= 4,
            PitfallStatus::Active => {}
        }
    }
    if score > 0 {
        score += i64::from(l.hits().min(5));
    }
    score
}

/// Retrieve prior lessons whose error signature matches `failure_detail` — the
/// HIGHEST-precision retrieval trigger in the whole loop: it fires on a CONCRETE
/// failure (FLARE / Self-RAG "retrieve when failing / uncertain"), so the match
/// key is an exact error signature, not fuzzy prose.
///
/// Used to inject "you have hit this exact pitfall N times before — here is what
/// worked, and it keeps recurring" into the single auto-fix attempt, closing the
/// loop at the moment it matters most (stage 5 of the SOTA self-evolution loop).
///
/// **Fingerprint-gated + abstaining:** matches by error-signature family, never
/// by fuzzy text, and returns an EMPTY string when the error is only a generic
/// fallback or there is no recorded match. This is the deliberate defence
/// against the "knowledge → noise" failure (CTIM-Rover): a similar-looking stack
/// trace often hides a different root cause, so injecting nothing beats injecting
/// a misleading prior fix.
#[must_use]
pub fn lessons_for_error(project_root: &Path, failure_detail: &str) -> String {
    let insight = crate::error_kb::classify_error(failure_detail);
    // Abstain on the generic fallback — its signature is too coarse to match a
    // specific prior root cause precisely.
    if !insight.recognized {
        return String::new();
    }
    let sig = insight.signature;
    // Match the full signature, or the same family (first two path segments,
    // e.g. `dependency/module-not-found`).
    let family: String = sig.splitn(3, '/').take(2).collect::<Vec<_>>().join("/");
    let mut hits: Vec<Lesson> = read_raw_lessons(project_root, DEV_ERRORS_FILE)
        .into_iter()
        .filter(|l| l.signature == sig || (!family.is_empty() && l.signature.starts_with(&family)))
        .collect();
    if hits.is_empty() {
        return String::new();
    }
    // Recurring-despite-warning first (these need a harder push), then the most
    // frequently-hit.
    hits.sort_by(|a, b| {
        let recurring = |l: &Lesson| u8::from(l.pitfall_status() == PitfallStatus::Recurring);
        recurring(b)
            .cmp(&recurring(a))
            .then(b.hits().cmp(&a.hits()))
    });
    let top = &hits[0];
    let top_sig = top.signature.clone();
    let recurring = top.pitfall_status() == PitfallStatus::Recurring;
    let mut out = String::from("\n\n## 历史踩坑（同类错误你之前遇到过）\n");
    out.push_str(&format!(
        "- 已累计 {} 次；签名 `{}`\n  根因：{}\n  上次修法：{}\n",
        top.hits(),
        top.signature,
        if top.root_cause.is_empty() {
            "(未记录)"
        } else {
            &top.root_cause
        },
        if top.fix.is_empty() {
            "(未记录)"
        } else {
            &top.fix
        },
    ));
    if recurring {
        out.push_str(
            "  [!] 上次已警示但仍复发——之前的修法不够彻底。这次必须换一个根本性的不同方案，并在修完后自检确认。\n",
        );
    }
    // Snapshot the hit count NOW so that, if this exact pitfall recurs after the
    // fix attempt, `capture_dev_errors` can flag `recurred_after_warning` — the
    // efficacy half of the closed loop.
    record_pitfall_injections(project_root, std::slice::from_ref(&top_sig));
    out
}

/// Render the most relevant prior-run lessons for the current phase's prompt.
/// Returns a formatted markdown block (empty string when no lessons exist —
/// so the prompt is unchanged for first-ever runs).
///
/// Triggering matches the pitfall against the project's real tech-stack
/// fingerprint (see [`lesson_trigger_score`]), not just the requirement prose,
/// then ranks by frequency + recency. We don't call BM25 here to avoid a
/// circular dependency between the agent and knowledge crates at prompt-assembly
/// time — the BM25 index already picks up learned/ files during
/// `phase_knowledge_digest`.
#[must_use]
pub fn relevant_lessons_for_prompt(project_root: &Path, requirement: &str) -> String {
    let lessons = read_all_raw_lessons(project_root);
    if lessons.is_empty() {
        return String::new();
    }

    // The trigger query = the requirement's words PLUS the project's real
    // tech-stack fingerprint (dependency names). Matching on the *stack* — not
    // the prose — is what makes triggering precise: a `react-router-dom`
    // pitfall fires exactly when this project depends on react-router-dom,
    // regardless of how the requirement is worded.
    let req_lower = requirement.to_ascii_lowercase();
    let mut query: std::collections::HashSet<String> = req_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(str::to_string)
        .collect();
    for tok in project_context_tokens(project_root) {
        query.insert(tok);
    }

    // Score each lesson by how strongly its situation intersects "right now".
    let mut scored: Vec<(i64, &Lesson)> = lessons
        .iter()
        .map(|l| (lesson_trigger_score(l, &query), l))
        .collect();
    // Highest relevance first, then most-frequently-hit, then most-recent.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.hits().cmp(&a.1.hits()))
            .then_with(|| b.1.first_seen.cmp(&a.1.first_seen))
    });

    // Tier 1: positively-matched (the current situation hit a recorded one).
    let mut top_idx: Vec<usize> = scored
        .iter()
        .enumerate()
        .filter(|(_, (s, _))| *s > 0)
        .take(2)
        .map(|(i, _)| i)
        .collect();
    // Tier 2: universal fallback — recent pitfalls apply regardless of overlap.
    // Dev errors (real "踩坑") are the highest-value avoid-next-time signal, so
    // they fill the remaining slots FIRST, then quality failures.
    for want_kind in [LessonKind::DevError, LessonKind::Failure] {
        if top_idx.len() >= 3 {
            break;
        }
        for (i, (s, l)) in scored.iter().enumerate() {
            if top_idx.len() >= 3 {
                break;
            }
            if *s == 0 && l.kind == want_kind && !top_idx.contains(&i) {
                top_idx.push(i);
            }
        }
    }
    if top_idx.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "
## Lessons from prior runs

",
    );
    out.push_str("Experiences captured from previous runs on this project. ");
    out.push_str(
        "Apply these to avoid repeating mistakes:

",
    );
    for &i in &top_idx {
        let lesson = scored[i].1;
        let icon = match lesson.kind {
            LessonKind::Failure => "[warn]",
            LessonKind::Revision => "[write]",
            LessonKind::ValidatedPattern => "[ok]",
            LessonKind::DevError => "[pitfall]",
        };
        // Dev errors carry their root cause too, so the worker understands WHY
        // to avoid the pitfall, not just the fix. The hit count signals how
        // chronic it is — a pitfall hit many times deserves extra care.
        if lesson.kind == LessonKind::DevError {
            let freq = if lesson.hits() > 1 {
                format!(" (已踩 {} 次)", lesson.hits())
            } else {
                String::new()
            };
            // Escalate a pitfall whose previous fix failed — tell the worker
            // the obvious fix didn't hold and to take a different, deeper tack.
            let escalate = if lesson.pitfall_status() == PitfallStatus::Recurring {
                "\n   ⚠ 上次已警示但仍复发 —— 之前的修法不够,这次必须换更彻底的方案并验证。"
            } else {
                ""
            };
            out.push_str(&format!(
                "{icon} **{}**{freq}
   原因: {}
   规避: {}{escalate}

",
                lesson.title, lesson.root_cause, lesson.fix
            ));
        } else {
            out.push_str(&format!(
                "{icon} **{}**
   {}

",
                lesson.title, lesson.fix
            ));
        }
    }

    // Efficacy bookkeeping: mark the dev-error pitfalls we just surfaced as
    // "injected" so a later capture can tell whether the warning actually
    // prevented recurrence. Fail-open — purely advisory state.
    let surfaced: Vec<String> = top_idx
        .iter()
        .map(|&i| scored[i].1)
        .filter(|l| l.kind == LessonKind::DevError && !l.signature.is_empty())
        .map(|l| l.signature.clone())
        .collect();
    record_pitfall_injections(project_root, &surfaced);

    out
}

/// Mark dev-error pitfalls as surfaced-to-the-worker, snapshotting their hit
/// count so a later [`capture_dev_errors`] can detect "recurred despite being
/// warned". Resets any prior `recurred_after_warning` flag — each fresh warning
/// gives the fix a clean chance to prove itself (self-healing). Fail-open.
fn record_pitfall_injections(project_root: &Path, signatures: &[String]) {
    if signatures.is_empty() {
        return;
    }
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    if store.is_empty() {
        return;
    }
    let want: std::collections::HashSet<&str> = signatures.iter().map(String::as_str).collect();
    let mut changed = false;
    for l in &mut store {
        if l.kind == LessonKind::DevError && want.contains(l.signature.as_str()) {
            let occ = l.hits();
            let eff = l.efficacy.get_or_insert(PitfallEfficacy {
                injected: 0,
                occ_at_injection: occ,
                recurred_after_warning: false,
                proven_fix: false,
            });
            eff.injected = eff.injected.saturating_add(1);
            eff.occ_at_injection = occ;
            eff.recurred_after_warning = false;
            changed = true;
        }
    }
    if changed {
        write_raw_lessons(project_root, DEV_ERRORS_FILE, &store);
    }
}

/// Mark the pitfall(s) matching `raw_errors` as having a proven fix — called
/// when an in-run auto-repair made the build/test pass again. This is the
/// strongest efficacy signal: we directly observed the recorded fix work, so
/// the pitfall is validated immediately rather than after several quiet runs.
/// Fail-open. Returns how many records were marked.
pub fn mark_pitfalls_resolved(project_root: &Path, raw_errors: &[String]) -> usize {
    let want: std::collections::HashSet<String> = raw_errors
        .iter()
        .filter(|e| crate::error_kb::looks_like_error(e))
        .map(|e| crate::error_kb::classify_error(e).signature)
        .collect();
    if want.is_empty() {
        return 0;
    }
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    if store.is_empty() {
        return 0;
    }
    let mut marked = 0;
    for l in &mut store {
        if l.kind == LessonKind::DevError && want.contains(&l.signature) {
            let occ = l.hits();
            let eff = l.efficacy.get_or_insert(PitfallEfficacy {
                injected: 0,
                occ_at_injection: occ,
                recurred_after_warning: false,
                proven_fix: false,
            });
            eff.proven_fix = true;
            eff.recurred_after_warning = false;
            // Re-baseline the occurrence counter to NOW so a later recurrence
            // (occurrences > occ_at_injection) is detected and flips
            // `recurred_after_warning`, demoting this from "Validated".
            eff.occ_at_injection = occ;
            marked += 1;
        }
    }
    if marked > 0 {
        write_raw_lessons(project_root, DEV_ERRORS_FILE, &store);
    }
    marked
}

/// Summary of the pitfall KB's self-verification state, for reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PitfallEfficacySummary {
    /// Distinct dev-error pitfalls recorded.
    pub total: usize,
    /// Pitfalls whose fix is proven (warned, no recurrence since).
    pub validated: usize,
    /// Pitfalls that recurred despite being warned — fix insufficient.
    pub recurring: usize,
    /// Pitfalls not yet surfaced / unproven.
    pub active: usize,
}

/// Render a human-readable overview of the pitfall KB — its self-verification
/// summary plus each recorded pitfall, sorted worst-first (recurring →
/// most-hit → recent). Used by the TUI `/pitfalls` overlay and any CLI view.
#[must_use]
pub fn pitfall_overview(project_root: &Path) -> String {
    let mut pits: Vec<Lesson> = read_raw_lessons(project_root, DEV_ERRORS_FILE)
        .into_iter()
        .filter(|l| l.kind == LessonKind::DevError)
        .collect();
    if pits.is_empty() {
        return "踩坑知识库还是空的。\n\n开发过程中一旦遇到编译/类型/依赖/运行时等报错,\
                UmaDev 会自动识别、记录,并在下次遇到同类问题前提醒规避。"
            .to_string();
    }

    // Worst first: recurring fixes, then most-frequently-hit, then recent.
    let rank = |l: &Lesson| match l.pitfall_status() {
        PitfallStatus::Recurring => 0,
        PitfallStatus::Active => 1,
        PitfallStatus::Validated => 2,
    };
    pits.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.hits().cmp(&a.hits()))
            .then_with(|| b.first_seen.cmp(&a.first_seen))
    });

    let s = pitfall_efficacy_summary(project_root);
    let mut out = format!(
        "踩坑知识库 — 共 {} 条\n  [ok] 已验证(修复有效) {} · [warn] 仍复发(需加强) {} · 待验证 {}\n\n",
        s.total, s.validated, s.recurring, s.active
    );
    for l in &pits {
        let (icon, tag) = match l.pitfall_status() {
            PitfallStatus::Validated => ("[ok]", "已验证"),
            PitfallStatus::Recurring => ("[warn]", "仍复发"),
            PitfallStatus::Active => ("[pitfall]", "待验证"),
        };
        let ctx = if l.context.is_empty() {
            String::new()
        } else {
            format!("  栈: {}", l.context.join(", "))
        };
        out.push_str(&format!(
            "{icon} {} (已踩 {} 次 · {tag})\n  签名: {}{ctx}\n  原因: {}\n  规避: {}\n\n",
            l.title,
            l.hits(),
            l.signature,
            truncate(&l.root_cause, 160),
            truncate(&l.fix, 240),
        ));
    }
    out
}

/// Compute the pitfall efficacy summary for `umadev report` / `/pitfalls`.
#[must_use]
pub fn pitfall_efficacy_summary(project_root: &Path) -> PitfallEfficacySummary {
    let mut s = PitfallEfficacySummary::default();
    for l in read_raw_lessons(project_root, DEV_ERRORS_FILE) {
        if l.kind != LessonKind::DevError {
            continue;
        }
        s.total += 1;
        match l.pitfall_status() {
            PitfallStatus::Validated => s.validated += 1,
            PitfallStatus::Recurring => s.recurring += 1,
            PitfallStatus::Active => s.active += 1,
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phases::QualityCheck;
    use tempfile::TempDir;

    fn check(name: &str, status: &str, score: i32) -> QualityCheck {
        QualityCheck {
            name: name.to_string(),
            category: "contract".to_string(),
            description: "test".to_string(),
            status: status.to_string(),
            score,
            details: format!("details for {name}"),
            weight: 2.0,
        }
    }

    #[test]
    fn lessons_for_error_matches_signature_and_abstains() {
        let tmp = TempDir::new().unwrap();
        let err = "Error: Cannot find module 'react-router-dom'".to_string();
        capture_dev_errors(
            tmp.path(),
            std::slice::from_ref(&err),
            "demo",
            "build an app",
        );
        // A same-signature failure surfaces the prior lesson at fix time.
        let hit = lessons_for_error(tmp.path(), &err);
        assert!(!hit.is_empty(), "matching prior lesson must surface");
        assert!(hit.contains("历史踩坑"));
        // An unrecognised / generic failure ABSTAINS (knowledge->noise defence).
        let miss = lessons_for_error(tmp.path(), "something vague happened, no signature");
        assert!(
            miss.is_empty(),
            "must abstain when there is no confident signature match"
        );
    }

    #[test]
    fn capture_dev_errors_distills_dedups_and_recalls() {
        let tmp = TempDir::new().unwrap();
        let errors = vec![
            "Error: Cannot find module 'react-router-dom'".to_string(),
            "Compiled successfully".to_string(), // not an error → skipped
            "npm ERR! ERESOLVE unable to resolve dependency tree".to_string(),
        ];
        let n = capture_dev_errors(tmp.path(), &errors, "demo", "做一个后台管理系统");
        assert_eq!(n, 2, "two real errors captured, the success line skipped");

        let raw = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        assert_eq!(raw.len(), 2);
        assert!(raw.iter().all(|l| l.kind == LessonKind::DevError));
        assert!(raw
            .iter()
            .any(|l| l.signature == "dependency/module-not-found/react-router-dom"));

        // Re-capturing the SAME pitfalls (plus a genuinely new one) appends
        // only the new one — recurrence is deduped by signature across runs.
        let again = vec![
            "Error: Cannot find module 'react-router-dom'".to_string(),
            "TypeError: Cannot read properties of undefined (reading 'map')".to_string(),
        ];
        let n2 = capture_dev_errors(tmp.path(), &again, "demo", "做一个后台管理系统");
        assert_eq!(n2, 1, "only the new undefined-access pitfall is added");
        let store = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        assert_eq!(store.len(), 3, "recurrence bumps count, not a new row");

        // The recurring pitfall's frequency was incremented (hit in both calls).
        let rr = store
            .iter()
            .find(|l| l.signature == "dependency/module-not-found/react-router-dom")
            .expect("react-router pitfall present");
        assert_eq!(rr.hits(), 2, "recurrence incremented occurrences");

        // Dev-error pitfalls surface in the recalled prompt block even when the
        // requirement shares no keywords with the error text.
        let recall = relevant_lessons_for_prompt(tmp.path(), "完全无关的需求文本");
        assert!(
            recall.contains("[pitfall]"),
            "recall must surface pitfalls: {recall}"
        );
        assert!(recall.contains("规避"));
        assert!(recall.contains("已踩 2 次"), "frequency shown: {recall}");
    }

    #[test]
    fn efficacy_loop_escalates_recurrence_and_validates_fixes() {
        let tmp = TempDir::new().unwrap();
        let sig = "dependency/module-not-found/lodash";
        let err = vec!["Error: Cannot find module 'lodash'".to_string()];
        let status = |t: &std::path::Path| {
            read_raw_lessons(t, DEV_ERRORS_FILE)
                .into_iter()
                .find(|l| l.signature == sig)
                .map(|l| l.pitfall_status())
        };

        // 1. First sighting → Active (never warned).
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Active));

        // 2. Warn the worker once — a single optimistic warning is not yet proof.
        let _ = relevant_lessons_for_prompt(tmp.path(), "无关需求一");
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Active));

        // 3. It recurs DESPITE the warning → escalated to Recurring.
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Recurring));
        assert_eq!(pitfall_efficacy_summary(tmp.path()).recurring, 1);

        // 4. The next recall surfaces it LOUDLY (escalation annotation) and, by
        //    re-warning, gives the fix a fresh chance (self-healing reset).
        let recall = relevant_lessons_for_prompt(tmp.path(), "无关需求二");
        assert!(
            recall.contains("⚠ 上次已警示"),
            "recurrence must escalate: {recall}"
        );

        // 5. Having now been warned twice and NOT recurred since, its fix is
        //    Validated — the loop confirms it's beaten and damps it.
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Validated));
        let s = pitfall_efficacy_summary(tmp.path());
        assert_eq!(s.total, 1);
        assert_eq!(s.validated, 1);
        assert_eq!(s.recurring, 0);
    }

    #[test]
    fn in_run_fix_proves_pitfall_immediately() {
        let tmp = TempDir::new().unwrap();
        let err = vec!["Error: Cannot find module 'lodash'".to_string()];
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        let sig = "dependency/module-not-found/lodash";
        let st = |t: &std::path::Path| {
            read_raw_lessons(t, DEV_ERRORS_FILE)
                .into_iter()
                .find(|l| l.signature == sig)
                .map(|l| l.pitfall_status())
        };
        assert_eq!(st(tmp.path()), Some(PitfallStatus::Active));

        // An in-run auto-fix made the build pass → mark proven directly.
        let n = mark_pitfalls_resolved(tmp.path(), &err);
        assert_eq!(n, 1);
        assert_eq!(
            st(tmp.path()),
            Some(PitfallStatus::Validated),
            "a proven in-run fix validates the pitfall immediately"
        );
        assert_eq!(pitfall_efficacy_summary(tmp.path()).validated, 1);
    }

    #[test]
    fn pitfall_store_is_bounded() {
        let tmp = TempDir::new().unwrap();
        // Generate more distinct pitfalls than the cap.
        let errors: Vec<String> = (0..MAX_DEV_PITFALLS + 25)
            .map(|n| format!("Error: Cannot find module 'pkg-{n}'"))
            .collect();
        capture_dev_errors(tmp.path(), &errors, "demo", "需求");
        let store = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        assert!(
            store.len() <= MAX_DEV_PITFALLS,
            "store must be capped at {MAX_DEV_PITFALLS}, got {}",
            store.len()
        );
    }

    #[test]
    fn project_context_tokens_reads_dependency_manifests() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies":{"react":"^18","react-router-dom":"^6"},
                "devDependencies":{"vite":"^5","typescript":"^5"}}"#,
        )
        .unwrap();
        let toks = project_context_tokens(tmp.path());
        assert!(toks.iter().any(|t| t == "react-router-dom"));
        assert!(toks.iter().any(|t| t == "react"));
        assert!(toks.iter().any(|t| t == "vite"));
        assert!(toks.iter().any(|t| t == "typescript"));
    }

    #[test]
    fn trigger_score_rewards_stack_discriminator() {
        let lesson = Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: "踩坑".into(),
            body: String::new(),
            fix: String::new(),
            root_cause: String::new(),
            keywords: vec!["dependency".into(), "module-not-found".into()],
            source_requirement: String::new(),
            first_seen: "2026-06-19T00:00:00Z".into(),
            signature: "dependency/module-not-found/react-router-dom".into(),
            occurrences: 3,
            context: vec!["react".into()],
            efficacy: None,
        };
        let with: std::collections::HashSet<String> =
            ["react-router-dom".to_string(), "react".to_string()]
                .into_iter()
                .collect();
        let s_with = lesson_trigger_score(&lesson, &with);
        let without: std::collections::HashSet<String> = ["vue".to_string(), "vite".to_string()]
            .into_iter()
            .collect();
        let s_without = lesson_trigger_score(&lesson, &without);
        assert!(
            s_with >= 6,
            "discriminator present should score high: {s_with}"
        );
        assert!(
            s_with > s_without,
            "in-stack pitfall must outrank out-of-stack: {s_with} vs {s_without}"
        );
    }

    #[test]
    fn recognized_dev_error_is_global_worthy_on_first_sight() {
        // A classified pitfall seen in ONE project is still cross-project
        // knowledge → promotes from a single requirement.
        let dev = Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: "踩坑 [dependency/module-not-found/lodash]".into(),
            body: String::new(),
            fix: String::new(),
            root_cause: String::new(),
            keywords: vec![],
            source_requirement: "proj-a".into(),
            first_seen: "2026-06-19T00:00:00Z".into(),
            signature: "dependency/module-not-found/lodash".into(),
            occurrences: 1,
            context: vec![],
            efficacy: None,
        };
        assert!(group_is_global_worthy(&[&dev], 1));

        // A generic-fallback dev error is NOT promoted on first sight (too noisy).
        let generic = Lesson {
            signature: "general/error/something".into(),
            ..dev.clone()
        };
        assert!(!group_is_global_worthy(&[&generic], 1));

        // A quality failure still needs ≥2 distinct requirements to promote.
        let qual = Lesson {
            kind: LessonKind::Failure,
            signature: String::new(),
            ..dev.clone()
        };
        assert!(!group_is_global_worthy(&[&qual], 1));
        assert!(group_is_global_worthy(&[&qual], 2));
    }

    #[test]
    fn capture_quality_failures_writes_raw_jsonl() {
        let tmp = TempDir::new().unwrap();
        let checks = vec![
            check("API URL consistency", "failed", 30),
            check("OpenAPI contract", "passed", 100),
            check("No placeholder content", "warning", 60),
        ];
        capture_quality_failures(tmp.path(), &checks, "demo", "博客系统");
        let raw = read_raw_lessons(tmp.path(), "quality-failures.jsonl");
        // 2 lessons (failed + warning; passed is skipped).
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0].kind, LessonKind::Failure);
        assert!(raw[0].title.contains("API URL consistency"));
        assert!(raw[1].title.contains("placeholder"));
    }

    #[test]
    fn capture_quality_failures_no_failures_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let checks = vec![check("All good", "passed", 100)];
        capture_quality_failures(tmp.path(), &checks, "demo", "x");
        assert!(read_raw_lessons(tmp.path(), "quality-failures.jsonl").is_empty());
    }

    #[test]
    fn capture_gate_revision_writes_adr_and_lesson() {
        let tmp = TempDir::new().unwrap();
        let adr_path = capture_gate_revision(
            tmp.path(),
            "docs_confirm",
            "需要更多数据库设计的细节",
            "博客系统",
        );
        // ADR file written.
        assert!(adr_path.is_file());
        let adr = fs::read_to_string(&adr_path).unwrap();
        assert!(adr.contains("ADR"));
        assert!(adr.contains("docs_confirm"));
        assert!(adr.contains("数据库设计"));
        // Raw lesson written.
        let lessons = read_raw_lessons(tmp.path(), "gate-revisions.jsonl");
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].kind, LessonKind::Revision);
        assert!(lessons[0].body.contains("数据库设计"));
    }

    #[test]
    fn capture_validated_patterns_records_contract() {
        let tmp = TempDir::new().unwrap();
        let spec = umadev_contract::parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | /api/articles | - | - | none | List |\n",
            "demo",
        );
        capture_validated_patterns(tmp.path(), "demo", "博客系统", &spec);
        let lessons = read_raw_lessons(tmp.path(), "validated-decisions.jsonl");
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].kind, LessonKind::ValidatedPattern);
        assert!(lessons[0].body.contains("/api/articles"));
    }

    #[test]
    fn capture_validated_patterns_empty_spec_skips() {
        let tmp = TempDir::new().unwrap();
        capture_validated_patterns(tmp.path(), "demo", "x", &ApiSpec::default());
        assert!(read_raw_lessons(tmp.path(), "validated-decisions.jsonl").is_empty());
    }

    #[test]
    fn domain_for_check_maps_correctly() {
        assert_eq!(domain_for_check("API URL consistency"), "api");
        assert_eq!(domain_for_check("OpenAPI contract"), "api");
        assert_eq!(domain_for_check("No placeholder content"), "governance");
        assert_eq!(domain_for_check("Hardcoded color block events"), "frontend");
        assert_eq!(domain_for_check("Ops artifacts present"), "devops");
        assert_eq!(
            domain_for_check("PRD↔Architecture alignment"),
            "architecture"
        );
        assert_eq!(domain_for_check("Unknown check"), "general");
    }

    #[test]
    fn fix_suggestion_is_actionable() {
        let fix = fix_suggestion_for_check("No placeholder content");
        assert!(fix.contains("TODO"));
        let fix = fix_suggestion_for_check("OpenAPI contract");
        assert!(fix.contains("contract"));
    }

    #[test]
    fn keywords_extracted_from_multiple_sources() {
        let kws = extract_keywords(
            "API URL consistency",
            "frontend calls /api/x",
            "博客系统 articles",
        );
        assert!(kws.contains(&"api".to_string()));
        assert!(kws.contains(&"consistency".to_string()));
        assert!(kws.contains(&"articles".to_string()));
    }

    #[test]
    fn raw_lessons_persist_across_calls() {
        let tmp = TempDir::new().unwrap();
        let checks1 = vec![check("Check A", "failed", 20)];
        let checks2 = vec![check("Check B", "failed", 10)];
        capture_quality_failures(tmp.path(), &checks1, "demo", "req");
        capture_quality_failures(tmp.path(), &checks2, "demo", "req");
        let raw = read_raw_lessons(tmp.path(), "quality-failures.jsonl");
        assert_eq!(raw.len(), 2);
    }

    #[test]
    fn read_all_raw_lessons_merges_files() {
        let tmp = TempDir::new().unwrap();
        capture_quality_failures(tmp.path(), &[check("X", "failed", 10)], "d", "r");
        capture_gate_revision(tmp.path(), "docs_confirm", "fix it", "r");
        let all = read_all_raw_lessons(tmp.path());
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn adr_filename_includes_gate_and_timestamp() {
        let tmp = TempDir::new().unwrap();
        let path = capture_gate_revision(tmp.path(), "preview_confirm", "redo", "req");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("preview_confirm-"));
        assert!(name.ends_with(".md"));
    }

    #[test]
    fn read_missing_file_returns_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(read_raw_lessons(tmp.path(), "nonexistent.jsonl").is_empty());
    }

    #[test]
    fn sediment_creates_markdown_files() {
        let tmp = TempDir::new().unwrap();
        let checks = vec![
            check("API URL consistency", "failed", 30),
            check("No placeholder content", "warning", 60),
        ];
        capture_quality_failures(tmp.path(), &checks, "demo", "博客系统 articles api");
        let count = sediment_lessons(tmp.path());
        assert_eq!(count, 2, "should write 2 markdown files");
        // Files exist under learned/<domain>/.
        let learned = tmp.path().join(".umadev/learned");
        assert!(learned.join("api").is_dir() || learned.join("governance").is_dir());
    }

    #[test]
    fn sediment_dedupes_by_domain_title() {
        let tmp = TempDir::new().unwrap();
        let checks = vec![check("API URL consistency", "failed", 30)];
        // Capture the same failure twice.
        capture_quality_failures(tmp.path(), &checks, "demo", "req");
        capture_quality_failures(tmp.path(), &checks, "demo", "req");
        let count = sediment_lessons(tmp.path());
        assert_eq!(count, 1, "dedupe should produce 1 file for repeated lesson");
    }

    #[test]
    fn sediment_markdown_has_correct_structure() {
        let tmp = TempDir::new().unwrap();
        capture_quality_failures(
            tmp.path(),
            &[check("OpenAPI contract", "failed", 0)],
            "d",
            "api contract openapi",
        );
        let _ = sediment_lessons(tmp.path());
        let files = list_sedimented_lessons(tmp.path());
        assert!(!files.is_empty());
        let content = fs::read_to_string(&files[0]).unwrap();
        // Has front-matter tags.
        assert!(content.contains("tags:"));
        // Has H1 + H2 sections.
        assert!(content.contains("# "));
        assert!(content.contains("## Symptom"));
        assert!(content.contains("## Fix"));
        assert!(content.contains("## Root cause"));
        // Keywords in body (for BM25).
        assert!(content.contains("Keywords:"));
        assert!(content.contains("openapi"));
    }

    #[test]
    fn sediment_empty_raw_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(sediment_lessons(tmp.path()), 0);
        assert!(list_sedimented_lessons(tmp.path()).is_empty());
    }

    #[test]
    fn list_sedimented_skips_raw_dir() {
        let tmp = TempDir::new().unwrap();
        capture_quality_failures(tmp.path(), &[check("X", "failed", 0)], "d", "r");
        let _ = sediment_lessons(tmp.path());
        let files = list_sedimented_lessons(tmp.path());
        // No file should be under _raw.
        assert!(files.iter().all(|f| !f.to_string_lossy().contains("_raw")));
    }
}
