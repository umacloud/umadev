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
/// Where per-signature reflection logs live — the sliding window of
/// base-generated correction strategies for pitfalls that recurred after a
/// warning. One JSONL file per (normalised) signature.
pub const REFLECTIONS_DIR: &str = ".umadev/reflections";

/// How many recent reflections to retain per signature. Small — we only need
/// the latest distilled strategy plus a little history for context, not a full
/// audit trail (the audit log already covers that).
const MAX_REFLECTIONS_PER_SIG: usize = 3;

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
    /// `true` once the memory-reconcile step judged this lesson superseded /
    /// contradicted by a newer one. INVALIDATE marks, never physically deletes
    /// (audit posture): an invalidated lesson is excluded from sediment + the
    /// retrieval candidate set but stays on disk for provenance. `#[serde(default)]`
    /// keeps every pre-existing JSONL row (written before this field existed)
    /// readable, and a row that has never been reconciled stays `false`.
    #[serde(default)]
    pub invalidated: bool,
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
    /// Failed-fix ledger: the recorded fixes that were ALREADY tried and still
    /// let the pitfall recur. On the next injection these are surfaced verbatim
    /// as "已试过但无效的修法" so the base is steered AWAY from re-running a known
    /// failure toward a different approach — instead of just re-loading the same
    /// failed fix more loudly. Capped + deduped; empty for fixes that never
    /// recurred. `#[serde(default)]` keeps older efficacy rows readable.
    #[serde(default)]
    pub failed_fixes: Vec<String>,
    /// A higher-level corrective STRATEGY, produced by the base on a true
    /// recurrence (not a template). Where [`Self::failed_fixes`] records what NOT
    /// to re-run, this records what to do INSTEAD: a different, simple,
    /// higher-altitude approach that sidesteps the way the previous fixes failed.
    /// Populated only by [`reflect_on_recurrence`] when the pitfall recurred after
    /// a warning, and surfaced ahead of the failed-fix ledger on the next match.
    /// Empty until reflection runs. `#[serde(default)]` keeps older rows readable.
    #[serde(default)]
    pub next_strategy: String,
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
            invalidated: false,
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
        invalidated: false,
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
        invalidated: false,
    };
    append_raw_lessons(project_root, "validated-decisions.jsonl", &[lesson]);
}

/// Minimum [`crate::tech_debt::DebtKind::severity`] a debt item must reach to
/// be fed back into the lessons KB. `4` keeps only the findings that mean a doc
/// can't be acted on — filler text (5) and unfilled acceptance criteria (4) —
/// so the KB learns from *significant* debt, not every stray `TODO` note.
const TECH_DEBT_LESSON_MIN_SEVERITY: u8 = 4;

/// Feed SIGNIFICANT tech-debt findings back into the lessons KB, so persistent
/// placeholder/filler debt participates in cross-run evolution the same way an
/// acceptance gap or a dev-error pitfall does.
///
/// Until now `scan_debt` results only fed a transient quality-check score and a
/// JSONL ledger — they never reached the capture→sediment→retrieve loop, so the
/// worker was never *reminded* "you keep shipping docs with filler text; write
/// real content". This closes that gap: each debt KIND that crosses
/// [`TECH_DEBT_LESSON_MIN_SEVERITY`] becomes one [`LessonKind::Failure`] lesson
/// (deduped by kind so a doc with 40 `Lorem ipsum` lines yields ONE lesson, not
/// 40), keyed under the `governance` domain. Returns how many lessons were
/// written. Fail-open: a write error never blocks the quality gate.
pub fn capture_tech_debt(
    project_root: &Path,
    items: &[crate::tech_debt::DebtItem],
    requirement: &str,
) -> usize {
    use crate::tech_debt::DebtKind;
    // Group significant items by kind so each distinct debt KIND yields one
    // lesson regardless of how many lines carry it. Keep a sample file:line and
    // the running count for the lesson body.
    let mut by_kind: std::collections::BTreeMap<DebtKind, (usize, String)> =
        std::collections::BTreeMap::new();
    for it in items {
        if it.kind.severity() < TECH_DEBT_LESSON_MIN_SEVERITY {
            continue;
        }
        let entry = by_kind
            .entry(it.kind)
            .or_insert_with(|| (0, format!("{}:{}", it.file, it.line)));
        entry.0 += 1;
    }
    if by_kind.is_empty() {
        return 0;
    }
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut lessons: Vec<Lesson> = Vec::new();
    for (kind, (count, sample)) in by_kind {
        let kind_name = serde_json::to_string(&kind)
            .unwrap_or_else(|_| "\"debt\"".into())
            .trim_matches('"')
            .to_string();
        let (label, fix, root_cause) = match kind {
            DebtKind::FillerText => (
                "Filler / Lorem ipsum text in delivered docs",
                "Replace EVERY Lorem-ipsum / filler passage with real, \
                 requirement-specific content. Filler signals a doc that can't be \
                 acted on downstream.",
                "Placeholder filler was shipped instead of real content — the \
                 artifact looks complete but carries no actionable detail.",
            ),
            DebtKind::UnfilledAcceptance => (
                "Unfilled Given/When/Then acceptance criteria",
                "Fill in every Given/When/Then with concrete pre-conditions, \
                 actions and observable outcomes (GET returns list, POST creates \
                 with id, …). Unfilled criteria can't gate delivery.",
                "Acceptance criteria were left as `Given TODO` templates — there \
                 is nothing for the quality gate or a reviewer to verify against.",
            ),
            _ => (
                "Unresolved placeholder debt in delivered docs",
                "Replace the placeholder markers with real content before \
                 delivery; track remaining debt in the ledger.",
                "Placeholder/TODO markers were left in the delivered artifacts.",
            ),
        };
        let keywords = extract_keywords(label, &sample, requirement);
        lessons.push(Lesson {
            kind: LessonKind::Failure,
            domain: "governance".to_string(),
            title: format!("Tech debt: {label} ({count}×)"),
            body: format!(
                "The latest run left {count} `{kind_name}` debt item(s) in the \
                 delivered docs (e.g. {sample}). High-severity debt like this \
                 means the artifact reads as finished but isn't actionable.\n\n\
                 Requirement: {requirement}"
            ),
            fix: fix.to_string(),
            root_cause: root_cause.to_string(),
            keywords,
            source_requirement: requirement.to_string(),
            first_seen: now.clone(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: None,
            invalidated: false,
        });
    }
    let written = lessons.len();
    append_raw_lessons(project_root, "tech-debt.jsonl", &lessons);
    written
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
        let mut insight = crate::error_kb::classify_error(text);
        // Stabilise the dedup key: strip volatile parts (relative-path
        // prefixes, version suffixes, line/col numbers) that leak into the
        // discriminator segment so the SAME root cause collapses to ONE
        // signature instead of drifting per file/version (see
        // [`normalize_signature`]). Without this, `occurrences` would stay
        // stuck at 1 for a recurring pitfall whose offending path or version
        // string differs run-to-run, and the frequency signal would be lost.
        insight.signature = normalize_signature(&insight.signature);
        if let Some(&i) = idx.get(&insight.signature) {
            // Recurrence → frequency++ and absorb any new context tokens.
            store[i].occurrences = store[i].hits().saturating_add(1);
            merge_tokens(&mut store[i].context, &context, 24);
            // Efficacy: if we had ALREADY warned the worker about this pitfall
            // (it was injected) and it recurred anyway, the recorded fix is
            // insufficient — flag it so recall escalates it next time.
            let occ_now = store[i].occurrences;
            let recorded_fix = store[i].fix.clone();
            if let Some(eff) = store[i].efficacy.as_mut() {
                // Recurred after we either warned the worker (injected) OR
                // marked an in-run fix as proven — both mean the recorded fix
                // did NOT hold. Without the `proven_fix` arm a pitfall that was
                // auto-fix-validated (injected still 0) would keep reporting as
                // "Validated" even as it bites again every run.
                if (eff.injected >= 1 || eff.proven_fix) && occ_now > eff.occ_at_injection {
                    eff.recurred_after_warning = true;
                    eff.proven_fix = false;
                    // Remember that THIS fix was already tried and failed, so
                    // the next injection steers the base away from it toward a
                    // different approach instead of re-loading it.
                    remember_failed_fix(eff, &recorded_fix);
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
            invalidated: false,
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
///
/// Keep priority is tiered by fix lifecycle first — still-failing (`Recurring`)
/// outranks unproven (`Active`), which outranks solved (`Validated`) — so a
/// pitfall whose fix is still failing is NEVER evicted before a handled one.
/// WITHIN a tier, eviction is by the recency·importance decay score
/// rather than a hard LRU: an old, low-importance lesson
/// is dropped before a recent or frequently-hit one even if their raw timestamps
/// would order them the other way. (Relevance has no query at prune time, so it
/// is the constant floor and drops out of the WITHIN-tier comparison.)
fn prune_pitfalls(store: &mut Vec<Lesson>) {
    if store.len() <= MAX_DEV_PITFALLS {
        return;
    }
    let now = Utc::now();
    let empty_query = std::collections::HashSet::new();
    let rank = |l: &Lesson| match l.pitfall_status() {
        PitfallStatus::Recurring => 0u8,
        PitfallStatus::Active => 1,
        PitfallStatus::Validated => 2,
    };
    store.sort_by(|a, b| {
        rank(a).cmp(&rank(b)).then_with(|| {
            // Higher decay score = keep → sort it earlier (descending).
            let sa = lesson_decay_score(a, &empty_query, now);
            let sb = lesson_decay_score(b, &empty_query, now);
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Final deterministic tiebreak so equal scores prune stably.
                .then_with(|| b.first_seen.cmp(&a.first_seen))
        })
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

/// Max distinct failed-fix entries kept in a pitfall's failed-fix ledger.
/// Small — we only need to tell the base what NOT to re-try, not keep a history.
const MAX_FAILED_FIXES: usize = 3;

/// Record a fix that was tried and let the pitfall recur, into the failed-fix
/// ledger. Deduped (a fix already known-failed isn't re-added) and capped at
/// [`MAX_FAILED_FIXES`] (oldest dropped). Empty/whitespace fixes are ignored.
fn remember_failed_fix(eff: &mut PitfallEfficacy, fix: &str) {
    let fix = fix.trim();
    if fix.is_empty() || eff.failed_fixes.iter().any(|f| f == fix) {
        return;
    }
    eff.failed_fixes.push(fix.to_string());
    if eff.failed_fixes.len() > MAX_FAILED_FIXES {
        eff.failed_fixes.remove(0);
    }
}

// =====================================================================
// Reflection: base-generated correction strategy on a TRUE recurrence.
//
// The first time a pitfall is hit we hand the worker a cheap template-built
// diagnosis (root cause + recorded fix). Only when a pitfall recurs *after*
// we already warned about it (`recurred_after_warning`) is the template
// clearly insufficient — that is the moment, and the only moment, worth
// spending one extra base call to ask for a DIFFERENT, higher-altitude
// approach. The product (a short strategy) is stored on the efficacy record
// and snapshotted to a per-signature sliding window for auditing/inspection.
// =====================================================================

/// One reflection: a base-generated corrective strategy for a recurring
/// pitfall, snapshotted to `.umadev/reflections/<signature>.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reflection {
    /// The normalised pitfall signature this strategy targets.
    pub signature: String,
    /// Occurrence count at the moment of reflection (how chronic it was).
    pub occurrences: u32,
    /// The base-generated strategy: a different, simple, high-level approach
    /// that avoids the way the recorded fixes failed.
    pub strategy: String,
    /// ISO-8601 UTC timestamp the reflection was produced.
    pub at: String,
}

/// Build the reflection prompt for a recurring pitfall. The system half pins
/// the base to a strategy designer that produces a *different approach*, not a
/// restatement of the error; the user half supplies the concrete context (root
/// cause, the fixes already proven to fail). Returns `(system, user)`.
///
/// Deliberately demands ONE short, high-altitude strategy — "what to do
/// differently", not "what the error said" — so the output is actionable
/// avoid-next-time guidance rather than a louder stderr echo.
#[must_use]
pub fn reflection_prompt(l: &Lesson) -> (String, String) {
    let failed = l
        .efficacy
        .as_ref()
        .map(|e| e.failed_fixes.as_slice())
        .unwrap_or(&[]);
    let failed_block = if failed.is_empty() {
        "(none recorded)".to_string()
    } else {
        failed
            .iter()
            .map(|f| format!("- {}", truncate(f, 200)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let system = "\
You are a senior engineer doing a blameless post-mortem on a defect that KEEPS \
RECURRING even after the obvious fix was applied. Your job is NOT to restate the \
error or repeat the previous fix. Diagnose, in one or two sentences, WHY the \
earlier fix failed to hold, then design ONE different, simple, higher-level \
approach that sidesteps that failure mode entirely. Answer with the strategy \
only — a few sentences, imperative voice, no preamble, no code dump."
        .to_string();
    let user = format!(
        "## Recurring pitfall\nSignature: {sig}\nTimes hit: {hits}\n\n\
         ## Root cause (recorded)\n{root}\n\n\
         ## Fixes already tried that STILL let it recur (do not repeat these)\n{failed}\n\n\
         Design a different, simple, high-level approach to avoid this from now on.",
        sig = l.signature,
        hits = l.hits(),
        root = if l.root_cause.is_empty() {
            "(not recorded)"
        } else {
            &l.root_cause
        },
        failed = failed_block,
    );
    (system, user)
}

/// Persist a base-generated correction `strategy` for the pitfall whose
/// signature is `signature`: store it on the pitfall's efficacy record (so
/// recall can surface it) AND append it to the per-signature reflection sliding
/// window. `signature` is normalised internally so a caller can pass a raw
/// classified signature. Returns `true` if a matching pitfall was updated.
///
/// Fail-open: an empty strategy, a missing store, or any I/O error is a no-op —
/// the caller falls back to the existing template path. Holds [`DEV_KB_LOCK`]
/// for the dev-errors read-modify-write so it never races the capture path.
pub fn record_pitfall_strategy(project_root: &Path, signature: &str, strategy: &str) -> bool {
    let strategy = strategy.trim();
    if strategy.is_empty() {
        return false;
    }
    static DEV_KB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _kb_guard = DEV_KB_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let sig = normalize_signature(signature);
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    if store.is_empty() {
        return false;
    }
    let mut hit: Option<&Lesson> = None;
    let mut changed = false;
    for l in &mut store {
        if l.kind == LessonKind::DevError && l.signature == sig {
            let occ = l.hits();
            let eff = l.efficacy.get_or_insert(PitfallEfficacy {
                injected: 0,
                occ_at_injection: occ,
                recurred_after_warning: false,
                proven_fix: false,
                failed_fixes: Vec::new(),
                next_strategy: String::new(),
            });
            eff.next_strategy = truncate(strategy, 600);
            changed = true;
        }
    }
    if !changed {
        return false;
    }
    // Snapshot for the sliding window BEFORE the write borrow ends.
    if let Some(l) = store
        .iter()
        .find(|l| l.kind == LessonKind::DevError && l.signature == sig)
    {
        hit = Some(l);
    }
    if let Some(l) = hit {
        append_reflection(
            project_root,
            &Reflection {
                signature: sig.clone(),
                occurrences: l.hits(),
                strategy: truncate(strategy, 600),
                at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            },
        );
    }
    write_raw_lessons(project_root, DEV_ERRORS_FILE, &store);
    true
}

/// Append a reflection to its per-signature sliding window
/// (`.umadev/reflections/<slug>.jsonl`), keeping only the most recent
/// [`MAX_REFLECTIONS_PER_SIG`]. Fail-open.
fn append_reflection(project_root: &Path, r: &Reflection) {
    let dir = project_root.join(REFLECTIONS_DIR);
    let _ = fs::create_dir_all(&dir);
    // Signature → filesystem-safe slug.
    let slug: String = r
        .signature
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = dir.join(format!("{slug}.jsonl"));
    let mut window: Vec<Reflection> = fs::read_to_string(&path)
        .ok()
        .map(|text| {
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<Reflection>(l).ok())
                .collect()
        })
        .unwrap_or_default();
    window.push(r.clone());
    let len = window.len();
    if len > MAX_REFLECTIONS_PER_SIG {
        window.drain(0..len - MAX_REFLECTIONS_PER_SIG);
    }
    let mut buf = String::new();
    for entry in &window {
        if let Ok(line) = serde_json::to_string(entry) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    let _ = fs::write(&path, buf);
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
        "tech-debt.jsonl",
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

/// Tokens that are clearly part of a *path* (a relative/source-dir prefix), not
/// the module/export symbol itself. When the offending quoted token in an error
/// was a path like `./components/Foo` rather than a bare package name, slugify
/// folds the whole path into the discriminator — so the SAME missing symbol
/// drifts per importing file. These leading tokens are stripped to recover the
/// stable trailing symbol. Kept deliberately small + conservative.
const PATH_PREFIX_TOKENS: &[&str] = &[
    "src",
    "app",
    "lib",
    "components",
    "component",
    "pages",
    "page",
    "utils",
    "util",
    "hooks",
    "hook",
    "services",
    "service",
    "modules",
    "module",
    "node",
    "dist",
    "build",
    "public",
    "assets",
    "styles",
    "test",
    "tests",
    "spec",
];

/// Normalise an error signature into a STABLE dedup key by stripping the
/// volatile parts that leak into the discriminator (last) segment, so the same
/// root cause collapses to one signature instead of drifting per run.
///
/// Only the discriminator segment (everything after `category/family/`) is
/// touched — the family always stays intact so unrelated families never
/// collide, and a clean package name (`react-router-dom`) is preserved whole.
/// What gets stripped is exactly what changes run-to-run for the *same* pitfall:
/// - **bare line/column numbers** — pure-digit tokens (`42`, `10`) are dropped.
/// - **version suffixes** — a trailing `v4` / `18` / `1` token is dropped so
///   `lodash-v4` → `lodash` and `react-18` → `react` (a version bump is not a
///   new pitfall).
/// - **relative/source path prefixes** — leading `src` / `components` / `..`
///   style tokens are dropped so `components-foo` and `pages-foo` both reduce to
///   `foo` (the same missing symbol imported from two files must not fork).
///
/// A signature with fewer than three `/`-segments (e.g. the family-only
/// `type/type-mismatch`) is returned unchanged — there is no discriminator. If
/// stripping empties the discriminator entirely, the family alone is returned so
/// it still dedups instead of staying unique forever. Idempotent — the
/// recall/resolve paths rely on normalising twice being a no-op.
#[must_use]
pub fn normalize_signature(signature: &str) -> String {
    let parts: Vec<&str> = signature.splitn(3, '/').collect();
    if parts.len() < 3 {
        return signature.to_string();
    }
    let (category, family, disc) = (parts[0], parts[1], parts[2]);
    let mut tokens: Vec<&str> = disc
        .split('-')
        // Drop empty + pure-digit (line/col) tokens up front.
        .filter(|t| !t.is_empty() && !t.chars().all(|c| c.is_ascii_digit()))
        .collect();
    // Drop a trailing version token like `v4` (a `v` followed by digits only).
    while let Some(last) = tokens.last() {
        let rest = last.trim_start_matches('v');
        if rest.len() < last.len() && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            tokens.pop();
        } else {
            break;
        }
    }
    // Drop leading path-prefix tokens, but never strip the final token (that
    // IS the symbol). `..` / `.` slugify to empty and are already gone.
    while tokens.len() > 1 && PATH_PREFIX_TOKENS.contains(&tokens[0]) {
        tokens.remove(0);
    }
    if tokens.is_empty() {
        format!("{category}/{family}")
    } else {
        format!("{category}/{family}/{}", tokens.join("-"))
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

// =====================================================================
// Memory reconcile: keep the sedimented corpus from rotting.
//
// Before each sediment write we reconcile every fresh lesson against the
// most-similar PRIOR ones, deciding ADD / UPDATE / INVALIDATE / NOOP. The
// JUDGEMENT is delegated to the base via [`reconcile_prompt`] +
// [`parse_reconcile_decision`] (the same host-driver seam reflection uses); with
// NO base (no judge supplied / no key) every decision is NOOP, which is
// byte-for-byte the previous pure-append behaviour — zero change. INVALIDATE
// marks the conflicting old lesson invalid (audit posture: never a physical
// delete) so it leaves the retrieval candidate set; a separate decay pass drops
// long-unmatched stale lessons from the sediment top-k via the existing 30-day
// recency curve.
// =====================================================================

/// What the reconcile step decides for a fresh lesson vs. its most-similar prior
/// lessons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileDecision {
    /// Genuinely new knowledge — keep it alongside the existing corpus.
    Add,
    /// A better/fresher version of an existing lesson — supersede the old one
    /// (mark the most-similar prior lesson invalid, keep the new one).
    Update,
    /// The fresh lesson CONTRADICTS a prior one that is now wrong — mark the
    /// conflicting prior lesson invalid (kept on disk for provenance).
    Invalidate,
    /// No change — duplicate or not worth touching the corpus over. This is the
    /// default and the no-base fallback (pure-append behaviour).
    Noop,
}

/// How many similar prior lessons the reconcile step considers per fresh lesson.
/// Small — a handful of nearest neighbours is enough to judge ADD vs UPDATE vs
/// INVALIDATE without ballooning the base prompt.
const RECONCILE_TOP_S: usize = 10;

/// Below this recency weight, a lesson that NEVER positively matched its own
/// reconcile neighbourhood is treated as decayed-out and skipped from the
/// sediment top-k (it stays in the raw ledger). 0.03 ≈ 5 half-lives ≈ 150 days,
/// so only genuinely ancient, never-reinforced lessons fade. Pitfalls
/// (`DevError`) are exempt — their own efficacy/frequency loop governs them.
const RECONCILE_DECAY_FLOOR: f64 = 0.03;

/// Build the reconcile prompt for a fresh lesson against its similar priors. The
/// system half pins the base to a librarian that OUTPUTS ONE verdict word; the
/// user half lays out the new lesson and the candidates. Returns `(system,
/// user)`. Mirrors [`reflection_prompt`]'s seam so the runner can drive it with
/// the same `try_generate` call and feed the reply to [`parse_reconcile_decision`].
#[must_use]
pub fn reconcile_prompt(new_lesson: &Lesson, similar: &[Lesson]) -> (String, String) {
    let system = "\
You are curating a long-lived engineering lesson library so it does not rot. \
Given a NEW lesson and the most similar EXISTING lessons, decide ONE action: \
ADD (genuinely new knowledge), UPDATE (the new one is a better version of an \
existing lesson — supersede it), INVALIDATE (the new one shows an existing \
lesson is now WRONG/contradicted), or NOOP (duplicate or not worth a change). \
Answer with exactly one of: ADD, UPDATE, INVALIDATE, NOOP — nothing else."
        .to_string();
    let mut sim_block = String::new();
    for (i, l) in similar.iter().enumerate() {
        sim_block.push_str(&format!(
            "{n}. [{domain}] {title}\n   fix: {fix}\n",
            n = i + 1,
            domain = l.domain,
            title = truncate(&l.title, 120),
            fix = truncate(&l.fix, 200),
        ));
    }
    if sim_block.is_empty() {
        sim_block.push_str("(no similar existing lessons)\n");
    }
    let user = format!(
        "## New lesson\n[{domain}] {title}\nfix: {fix}\nroot cause: {root}\n\n\
         ## Similar existing lessons\n{sim_block}\n\
         Reply with one word: ADD, UPDATE, INVALIDATE, or NOOP.",
        domain = new_lesson.domain,
        title = truncate(&new_lesson.title, 120),
        fix = truncate(&new_lesson.fix, 200),
        root = truncate(&new_lesson.root_cause, 200),
    );
    (system, user)
}

/// Parse a base reply into a [`ReconcileDecision`]. Tolerant: scans for the
/// verdict word anywhere in the reply (bases sometimes add a sentence). Anything
/// unrecognised → [`ReconcileDecision::Noop`] (fail-open: an unclear verdict
/// never mutates the corpus).
#[must_use]
pub fn parse_reconcile_decision(reply: &str) -> ReconcileDecision {
    let up = reply.to_ascii_uppercase();
    // Order matters: check the rarer, more-specific verbs before NOOP so a reply
    // like "INVALIDATE — it's wrong" isn't shadowed.
    if up.contains("INVALIDATE") {
        ReconcileDecision::Invalidate
    } else if up.contains("UPDATE") {
        ReconcileDecision::Update
    } else if up.contains("ADD") {
        ReconcileDecision::Add
    } else {
        ReconcileDecision::Noop
    }
}

/// A reconcile judge: given a fresh lesson and its similar priors, return the
/// action. The runner supplies one that calls the base (host-driver subprocess)
/// via [`reconcile_prompt`] + [`parse_reconcile_decision`]; `None` means "no
/// base" → every decision is NOOP (pure append, zero behaviour change).
pub type ReconcileJudge<'a> = &'a dyn Fn(&Lesson, &[Lesson]) -> ReconcileDecision;

/// Cosine-free cheap similarity between two lessons: shared keyword count plus a
/// same-domain bonus. Used only to pick the top-s neighbours to hand the judge —
/// the judge (base) makes the actual semantic call.
fn lesson_similarity(a: &Lesson, b: &Lesson) -> i64 {
    let bset: std::collections::HashSet<&str> = b.keywords.iter().map(String::as_str).collect();
    let shared = a
        .keywords
        .iter()
        .filter(|k| bset.contains(k.as_str()))
        .count() as i64;
    let domain_bonus = i64::from(a.domain == b.domain);
    shared * 2 + domain_bonus
}

/// Stable per-lesson identity used to mark a specific prior lesson invalid
/// across the read-merge-rewrite boundary (`read_all_raw_lessons` loses the
/// source file, so we re-open each file and match on this triple).
fn lesson_identity(l: &Lesson) -> (String, String, String) {
    (l.domain.clone(), l.title.clone(), l.first_seen.clone())
}

/// The raw files that hold reconcilable lessons. Pitfalls (`dev-errors.jsonl`)
/// are governed by their own efficacy loop and excluded — reconcile only curates
/// the append-only failure/revision/validated/tech-debt ledgers.
const RECONCILE_FILES: &[&str] = &[
    "quality-failures.jsonl",
    "gate-revisions.jsonl",
    "validated-decisions.jsonl",
    "tech-debt.jsonl",
];

/// Build the reconcile candidate pairs `(fresh_lesson, its top-s older similar
/// lessons)` for the current raw corpus. Each fresh lesson is paired with the
/// most-similar OLDER non-pitfall lessons (zero-similarity neighbours dropped).
/// This is the read-only half the runner uses to drive the BASE judge
/// asynchronously: it calls [`reconcile_prompt`] per pair, parses the verdict,
/// and feeds the resulting decision map back via [`sediment_lessons_with_judge`].
///
/// Pure read; pitfalls (`DevError`) are excluded (their own efficacy loop
/// governs them), as are already-invalidated rows. Empty when nothing has enough
/// neighbours to reconcile.
#[must_use]
pub fn reconcile_candidates(project_root: &Path) -> Vec<(Lesson, Vec<Lesson>)> {
    let all = read_all_raw_lessons(project_root);
    // Newest-first so a fresher lesson is judged against its older neighbours.
    let mut pool: Vec<&Lesson> = all
        .iter()
        .filter(|l| l.kind != LessonKind::DevError && !l.invalidated)
        .collect();
    pool.sort_by(|a, b| b.first_seen.cmp(&a.first_seen));

    let mut out: Vec<(Lesson, Vec<Lesson>)> = Vec::new();
    for (i, fresh) in pool.iter().enumerate() {
        let fresh_id = lesson_identity(fresh);
        let mut similar: Vec<&Lesson> = pool[i + 1..]
            .iter()
            .filter(|c| lesson_identity(c) != fresh_id)
            .copied()
            .collect();
        similar.sort_by_key(|c| std::cmp::Reverse(lesson_similarity(fresh, c)));
        similar.retain(|c| lesson_similarity(fresh, c) > 0);
        similar.truncate(RECONCILE_TOP_S);
        if similar.is_empty() {
            continue;
        }
        out.push((
            (*fresh).clone(),
            similar.iter().map(|l| (*l).clone()).collect(),
        ));
    }
    out
}

/// Reconcile the corpus: for each lesson (treated as the FRESH one), find its
/// top-[`RECONCILE_TOP_S`] OLDER similar lessons and ask `judge` whether the new
/// lesson ADDs, UPDATEs, INVALIDATEs, or NOOPs. On UPDATE/INVALIDATE the single
/// most-similar OLDER conflicting lesson is marked `invalidated` in its raw file
/// (never physically deleted). Returns `true` if anything was marked (so the
/// caller re-reads). Fail-open: a `None` judge or any error marks nothing.
///
/// Operates per-file (re-opening each [`RECONCILE_FILES`] entry) so the invalid
/// mark lands on the right on-disk row, matched by [`lesson_identity`].
fn reconcile_lessons(project_root: &Path, all: &[Lesson], judge: Option<ReconcileJudge>) -> bool {
    let Some(judge) = judge else {
        return false;
    };
    // Reconcilable (non-pitfall, not-already-invalid) lessons, newest first so a
    // fresher lesson is the one judged against its older neighbours.
    let mut pool: Vec<&Lesson> = all
        .iter()
        .filter(|l| l.kind != LessonKind::DevError && !l.invalidated)
        .collect();
    pool.sort_by(|a, b| b.first_seen.cmp(&a.first_seen));

    // Identities to mark invalid (the superseded/contradicted priors).
    let mut to_invalidate: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();

    for (i, fresh) in pool.iter().enumerate() {
        // Older candidates = everything after this one in the newest-first order,
        // excluding ones already slated for invalidation and exact self-identity.
        let fresh_id = lesson_identity(fresh);
        let mut similar: Vec<&Lesson> = pool[i + 1..]
            .iter()
            .filter(|c| {
                let cid = lesson_identity(c);
                cid != fresh_id && !to_invalidate.contains(&cid)
            })
            .copied()
            .collect();
        if similar.is_empty() {
            continue;
        }
        // Keep the top-s most-similar (drop zero-similarity neighbours so the
        // judge isn't asked to compare unrelated lessons).
        similar.sort_by_key(|c| std::cmp::Reverse(lesson_similarity(fresh, c)));
        similar.retain(|c| lesson_similarity(fresh, c) > 0);
        similar.truncate(RECONCILE_TOP_S);
        if similar.is_empty() {
            continue;
        }
        let owned: Vec<Lesson> = similar.iter().map(|l| (*l).clone()).collect();
        match judge(fresh, &owned) {
            ReconcileDecision::Update | ReconcileDecision::Invalidate => {
                // Supersede / contradict the single most-similar older lesson.
                if let Some(target) = similar.first() {
                    to_invalidate.insert(lesson_identity(target));
                }
            }
            ReconcileDecision::Add | ReconcileDecision::Noop => {}
        }
    }

    if to_invalidate.is_empty() {
        return false;
    }

    // Apply the marks per-file (the only place a raw file is rewritten by the
    // reconcile path). Fail-open per file.
    let mut changed = false;
    for file in RECONCILE_FILES {
        let mut rows = read_raw_lessons(project_root, file);
        if rows.is_empty() {
            continue;
        }
        let mut file_changed = false;
        for row in &mut rows {
            if !row.invalidated && to_invalidate.contains(&lesson_identity(row)) {
                row.invalidated = true;
                file_changed = true;
            }
        }
        if file_changed {
            write_raw_lessons(project_root, file, &rows);
            changed = true;
        }
    }
    changed
}

/// Returns the number of markdown files written. Fail-open: errors return 0.
///
/// This is the no-base entry point: it reconciles with a `None` judge, so every
/// decision is NOOP and the behaviour is byte-for-byte the historical
/// pure-append sediment. The runner calls [`sediment_lessons_with_judge`] with a
/// base-backed judge when a host driver is available.
#[must_use]
pub fn sediment_lessons(project_root: &Path) -> usize {
    sediment_lessons_with_judge(project_root, None)
}

/// Sediment all raw lessons into markdown, running the memory-reconcile step
/// first when `judge` is `Some`. With `judge == None` this is identical to the
/// historical [`sediment_lessons`] (NOOP for every lesson → pure append).
///
/// Reconcile flow (only when a judge is supplied):
/// 1. For each fresh lesson, find the top-[`RECONCILE_TOP_S`] similar PRIOR
///    lessons and ask the judge for ADD/UPDATE/INVALIDATE/NOOP.
/// 2. UPDATE / INVALIDATE → mark the most-similar conflicting prior lesson
///    `invalidated` in the raw ledger (never a physical delete).
/// 3. A decay pass drops long-unmatched, ancient non-pitfall lessons from the
///    sediment top-k (they remain in the raw ledger).
///
/// Returns the number of markdown files written. Fail-open throughout.
#[must_use]
pub fn sediment_lessons_with_judge(project_root: &Path, judge: Option<ReconcileJudge>) -> usize {
    let mut lessons = read_all_raw_lessons(project_root);
    if lessons.is_empty() {
        return 0;
    }

    // Reconcile (base-judged) — may mark some prior lessons invalid in the raw
    // ledger. Re-read afterwards so the sediment set reflects the marks.
    if judge.is_some() && reconcile_lessons(project_root, &lessons, judge) {
        lessons = read_all_raw_lessons(project_root);
    }

    // Drop invalidated lessons from the sediment candidate set (they stay on
    // disk for provenance but never become retrievable markdown). No-op for the
    // no-base path, where nothing is ever marked invalid.
    lessons.retain(|l| !l.invalidated);
    if lessons.is_empty() {
        return 0;
    }

    // Decay pass: an ancient, never-reinforced NON-pitfall lesson whose recency
    // weight has fallen below the floor drops out of the sediment top-k (it
    // stays in the raw ledger, so it is recoverable, just not re-surfaced).
    // Pitfalls are exempt — their efficacy/frequency loop governs their fate.
    // Skipped entirely for the no-base path so existing behaviour is unchanged.
    if judge.is_some() {
        let now = Utc::now();
        lessons.retain(|l| {
            l.kind == LessonKind::DevError
                || recency_weight(&l.first_seen, now) >= RECONCILE_DECAY_FLOOR
        });
        if lessons.is_empty() {
            return 0;
        }
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

    // Close the timing race: we just wrote new `.umadev/learned/*.md`, but the
    // BM25 index is content-hash cached, so a retrieval later in THIS SAME run
    // would otherwise still load the pre-sediment cache and miss what we just
    // learned. Invalidating the cache forces the next retrieval to re-scan the
    // now-larger corpus, making this run's lessons retrievable this run.
    // Fail-open (a no-op when nothing was written / no cache exists).
    if written > 0 {
        umadev_knowledge::invalidate_cache(project_root);
    }

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

/// Half-life (in days) of a lesson's recency weight. After this many days the
/// recency factor halves. 30 days ≈ "a lesson learned last month is worth half
/// a lesson learned today" — tuned so old, never-recurring lessons gracefully
/// fade rather than clinging to the top forever.
const RECENCY_HALFLIFE_DAYS: f64 = 30.0;

/// Recency weight in `(0, 1]` — `2^(-age_days / halflife)`. A lesson seen today
/// scores ~1.0; one seen a half-life ago scores ~0.5; an ancient one tends to 0.
/// An unparseable / future `first_seen` is treated as "now" (weight 1.0) so a
/// corrupted timestamp never silently buries a lesson (fail-open).
fn recency_weight(first_seen: &str, now: chrono::DateTime<Utc>) -> f64 {
    let age_days = parse_iso_utc(first_seen)
        .map(|t| (now - t).num_seconds() as f64 / 86_400.0)
        .unwrap_or(0.0)
        .max(0.0);
    2.0_f64.powf(-age_days / RECENCY_HALFLIFE_DAYS)
}

/// Parse an ISO-8601 `%Y-%m-%dT%H:%M:%SZ` UTC timestamp (the format every
/// capture site writes). Returns `None` for legacy/hand-edited rows that don't
/// match — callers treat that as "now" so the lesson isn't penalised.
fn parse_iso_utc(s: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Intrinsic importance in `[0, 1]` — how much this lesson *matters* regardless
/// of the current query or its age (the importance axis of the composite score).
///
/// Pitfalls whose recorded fix is still FAILING (`Recurring`) are the most
/// important to keep surfacing; recognised (classified) pitfalls outrank generic
/// ones; repeatedly-hit lessons matter more than one-offs; a proven/validated
/// fix is damped because it's effectively handled. Non-pitfall kinds get a flat
/// mid importance so they participate but don't dominate the pitfall channel.
fn lesson_importance(l: &Lesson) -> f64 {
    if l.kind != LessonKind::DevError {
        // Failures / revisions / validated patterns: steady, modest weight.
        return 0.4;
    }
    let mut imp: f64 = 0.4;
    if l.is_recognized() {
        imp += 0.2;
    }
    // Frequency: saturating contribution so a 50-hit pitfall isn't 50× a 1-hit.
    imp += (f64::from(l.hits().min(8)) / 8.0) * 0.3;
    match l.pitfall_status() {
        PitfallStatus::Recurring => imp += 0.4, // fix is failing — keep it loud
        PitfallStatus::Validated => imp -= 0.3, // handled — let it fade
        PitfallStatus::Active => {}
    }
    imp.clamp(0.05, 1.0)
}

/// Composite retrieval score: `recency · importance · relevance`, the product
/// of the three normalised axes.
///
/// - **relevance** comes from [`lesson_trigger_score`] (query + tech-stack
///   overlap), squashed to `(0, 1]` so a strong stack match dominates but never
///   alone pins an irrelevant-but-recent lesson to the top.
/// - **importance** is [`lesson_importance`] (intrinsic poignancy).
/// - **recency** is [`recency_weight`] (exponential age decay).
///
/// A zero-relevance lesson keeps a small floor so the universal-fallback tier
/// (recent pitfalls regardless of overlap) can still be ordered by recency ×
/// importance — that's what makes pruning/eviction graceful rather than a hard
/// LRU. Higher = keep / surface first.
fn lesson_decay_score(
    l: &Lesson,
    query: &std::collections::HashSet<String>,
    now: chrono::DateTime<Utc>,
) -> f64 {
    // Map the unbounded i64 relevance into (0,1]: a small floor (0.1) keeps
    // unmatched lessons orderable by recency×importance, while matches climb
    // toward 1.0. `.max(0)` so the validated-pitfall penalty (-4) can't make the
    // product negative. Cap at a small ceiling before the f64 cast — relevance
    // scores never exceed ~20, so this only guards against pathological input.
    let raw_rel = f64::from(lesson_trigger_score(l, query).clamp(0, 1_000) as i32);
    let rel = 0.1 + (raw_rel / (raw_rel + 6.0)) * 0.9;
    rel * lesson_importance(l) * recency_weight(&l.first_seen, now)
}

/// Find the best-matching pitfall for `failure_detail` IFF it has TRULY recurred
/// after a warning — i.e. the recorded fix already failed and a different,
/// reflected strategy is warranted.
///
/// Mirrors the matching of [`lessons_for_error`] (recognised-only, normalised
/// signature, full-signature-or-family filter, recurring-first ordering) but
/// returns the matched [`Lesson`] only when its status is
/// [`PitfallStatus::Recurring`]. The runner uses this to gate the single extra
/// reflection base call: first failures (status `Active`) return `None`, so they
/// keep the cheap template path and add no cost. Fail-open: an unrecognised error
/// or no match returns `None`.
#[must_use]
pub fn recurring_pitfall_for_error(project_root: &Path, failure_detail: &str) -> Option<Lesson> {
    let insight = crate::error_kb::classify_error(failure_detail);
    if !insight.recognized {
        return None;
    }
    let sig = normalize_signature(&insight.signature);
    let family: String = sig.splitn(3, '/').take(2).collect::<Vec<_>>().join("/");
    let mut hits: Vec<Lesson> = read_raw_lessons(project_root, DEV_ERRORS_FILE)
        .into_iter()
        .filter(|l| l.signature == sig || (!family.is_empty() && l.signature.starts_with(&family)))
        .collect();
    if hits.is_empty() {
        return None;
    }
    hits.sort_by(|a, b| {
        let recurring = |l: &Lesson| u8::from(l.pitfall_status() == PitfallStatus::Recurring);
        recurring(b)
            .cmp(&recurring(a))
            .then(b.hits().cmp(&a.hits()))
    });
    hits.into_iter()
        .next()
        .filter(|l| l.pitfall_status() == PitfallStatus::Recurring)
}

/// Retrieve prior lessons whose error signature matches `failure_detail` — the
/// HIGHEST-precision retrieval trigger in the whole loop: it fires on a CONCRETE
/// failure (retrieve only when failing / uncertain), so the match key is an
/// exact error signature, not fuzzy prose.
///
/// Used to inject "you have hit this exact pitfall N times before — here is what
/// worked, and it keeps recurring" into the single auto-fix attempt, closing the
/// loop at the moment it matters most (stage 5 of the SOTA self-evolution loop).
///
/// **Fingerprint-gated + abstaining:** matches by error-signature family, never
/// by fuzzy text, and returns an EMPTY string when the error is only a generic
/// fallback or there is no recorded match. This is the deliberate defence
/// against the "knowledge → noise" failure mode: a similar-looking stack
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
    // Normalise to the SAME stable key the store dedups under, so a recurring
    // failure whose offending path/version differs run-to-run still matches the
    // recorded lesson (otherwise the lookup would miss the very pitfall it hit).
    let sig = normalize_signature(&insight.signature);
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
        // Prefer the base-generated correction STRATEGY when reflection has
        // produced one — it says concretely "switch to THIS approach", which is
        // far more actionable than the bare "换个根本不同方案" template line.
        // Fall back to the template line only when no strategy exists yet.
        let strategy = top
            .efficacy
            .as_ref()
            .map(|e| e.next_strategy.trim())
            .filter(|s| !s.is_empty());
        if let Some(strategy) = strategy {
            out.push_str(&format!(
                "  [!] 上次已警示但仍复发——之前的修法不够彻底。改用这个不同的高层做法：\n    {}\n",
                truncate(strategy, 600),
            ));
        } else {
            out.push_str(
                "  [!] 上次已警示但仍复发——之前的修法不够彻底。这次必须换一个根本性的不同方案，并在修完后自检确认。\n",
            );
        }
        // Name the specific fixes that were ALREADY tried and failed, so the
        // base is steered AWAY from re-running them, not just told "try
        // harder". This is the structured "失败修法 + 换思路" guidance.
        out.push_str(&render_failed_fixes(top));
    }
    // Snapshot the hit count NOW so that, if this exact pitfall recurs after the
    // fix attempt, `capture_dev_errors` can flag `recurred_after_warning` — the
    // efficacy half of the closed loop.
    record_pitfall_injections(project_root, std::slice::from_ref(&top_sig));
    out
}

/// Render a pitfall's failed-fix ledger — the fixes already tried that still let
/// it recur — as a "do NOT re-run these, change approach" prompt block. Empty
/// string when the ledger is empty (older pitfalls, or one that never recurred),
/// so the prompt is unchanged in the common case.
fn render_failed_fixes(l: &Lesson) -> String {
    let Some(eff) = &l.efficacy else {
        return String::new();
    };
    if eff.failed_fixes.is_empty() {
        return String::new();
    }
    let mut out = String::from("  已试过但无效的修法（不要再重复，请换思路）：\n");
    for f in &eff.failed_fixes {
        out.push_str(&format!("    - {}\n", truncate(f, 200)));
    }
    out
}

/// Render the most relevant prior-run lessons for the current phase's prompt.
/// Returns a formatted markdown block (empty string when no lessons exist —
/// so the prompt is unchanged for first-ever runs).
///
/// Triggering matches the pitfall against the project's real tech-stack
/// fingerprint (see [`lesson_trigger_score`]), not just the requirement prose,
/// then ranks by the composite [`lesson_decay_score`]
/// (`recency · importance · relevance`) so a fresh, important, on-stack lesson
/// outranks an old high-frequency one. We don't call BM25 here to avoid a
/// circular dependency between the agent and knowledge crates at prompt-assembly
/// time — the BM25 index already picks up learned/ files during
/// `phase_knowledge_digest`.
#[must_use]
pub fn relevant_lessons_for_prompt(project_root: &Path, requirement: &str) -> String {
    let selected = select_relevant_lessons(project_root, requirement);
    if selected.is_empty() {
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
    for lesson in &selected {
        out.push_str(&render_one_lesson(lesson));
    }

    // Efficacy bookkeeping: mark the dev-error pitfalls we just surfaced as
    // "injected" so a later capture can tell whether the warning actually
    // prevented recurrence. Fail-open — purely advisory state.
    record_pitfall_injections(project_root, &surfaced_signatures(&selected));

    out
}

/// Structured sibling of [`relevant_lessons_for_prompt`]: returns the SAME
/// ranked selection as `(rank, Lesson)` pairs (rank 0 = best) instead of a
/// rendered string. The String API stays byte-for-byte unchanged; this variant
/// exists so a higher-altitude assembler (the coach's dual-channel reranker)
/// can fuse the fingerprint-decay channel with the BM25 knowledge channel by
/// RANK without re-deriving the selection.
///
/// Performs the same efficacy "injected" bookkeeping as the String API, because
/// returning these pairs to the caller means they ARE being surfaced into a
/// prompt. Empty for first-ever runs.
#[must_use]
pub fn relevant_lessons_for_prompt_ranked(
    project_root: &Path,
    requirement: &str,
) -> Vec<(usize, Lesson)> {
    let selected = select_relevant_lessons(project_root, requirement);
    if selected.is_empty() {
        return Vec::new();
    }
    record_pitfall_injections(project_root, &surfaced_signatures(&selected));
    selected.into_iter().enumerate().collect()
}

/// The dev-error signatures among `selected`, for efficacy "injected" bookkeeping.
fn surfaced_signatures(selected: &[Lesson]) -> Vec<String> {
    selected
        .iter()
        .filter(|l| l.kind == LessonKind::DevError && !l.signature.is_empty())
        .map(|l| l.signature.clone())
        .collect()
}

/// Shared selection core for both the String and structured lesson APIs: builds
/// the trigger query (requirement words + project tech-stack fingerprint), scores
/// every lesson on the (relevance, composite-decay) axes, and applies the same
/// two-tier pick (≤2 positively-matched, then top up to 3 with recent dev-errors
/// then quality failures). Returns the chosen lessons in final rank order. Pure
/// read — efficacy bookkeeping is the caller's responsibility so it happens
/// exactly once per surfacing.
fn select_relevant_lessons(project_root: &Path, requirement: &str) -> Vec<Lesson> {
    let lessons = read_all_raw_lessons(project_root);
    if lessons.is_empty() {
        return Vec::new();
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

    // Score each lesson on TWO axes:
    // - `rel` (raw relevance i64) gates the Tier-1 (`> 0`) vs Tier-2 (`== 0`)
    //   split below — a lesson only counts as "matched right now" when its
    //   situation actually intersects the query/stack.
    // - `decay` is the composite (`recency · importance ·
    //   relevance`) that ORDERS lessons within each tier, so a newer + more
    //   important + more relevant lesson sorts first instead of a pure
    //   occurrences/mtime ordering. An old validated pitfall no longer crowds
    //   out a fresh, still-failing one just because it was hit more times.
    let now = Utc::now();
    let mut scored: Vec<(i64, f64, &Lesson)> = lessons
        .iter()
        .map(|l| {
            (
                lesson_trigger_score(l, &query),
                lesson_decay_score(l, &query, now),
                l,
            )
        })
        .collect();
    // Highest composite decay score first; deterministic mtime tiebreak.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.first_seen.cmp(&a.2.first_seen))
    });

    // Tier 1: positively-matched (the current situation hit a recorded one).
    // `s` is the raw relevance; `> 0` means the query/stack actually intersected
    // this lesson. They're already ordered by the composite decay score above.
    let mut top_idx: Vec<usize> = scored
        .iter()
        .enumerate()
        .filter(|(_, (s, _, _))| *s > 0)
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
        for (i, (s, _, l)) in scored.iter().enumerate() {
            if top_idx.len() >= 3 {
                break;
            }
            if *s == 0 && l.kind == want_kind && !top_idx.contains(&i) {
                top_idx.push(i);
            }
        }
    }
    top_idx.iter().map(|&i| scored[i].2.clone()).collect()
}

/// Render ONE lesson into its prompt fragment — the public entry point used by
/// the coach's dual-channel reranker to render a fused lesson item with the SAME
/// formatting the String API uses (so a lesson reads identically whether it
/// arrives via the stacked path or the rank-fused path).
#[must_use]
pub fn render_lesson_for_prompt(lesson: &Lesson) -> String {
    render_one_lesson(lesson)
}

/// Render ONE selected lesson into the prompt block (shared by the String API
/// and any caller that surfaces a single lesson). Identical formatting to the
/// previous inline renderer so existing snapshots/tests are unaffected.
fn render_one_lesson(lesson: &Lesson) -> String {
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
        // Prefer the base-reflected strategy when one exists; otherwise list the
        // SPECIFIC failed fixes so it changes approach instead of re-running a
        // known-failure (see [`render_failed_fixes`]).
        let escalate = if lesson.pitfall_status() == PitfallStatus::Recurring {
            let strategy = lesson
                .efficacy
                .as_ref()
                .map(|e| e.next_strategy.trim())
                .filter(|s| !s.is_empty());
            let lead = if let Some(strategy) = strategy {
                format!(
                    "\n   ⚠ 上次已警示但仍复发 —— 改用这个不同的高层做法：\n   {}",
                    truncate(strategy, 600)
                )
            } else {
                "\n   ⚠ 上次已警示但仍复发 —— 之前的修法不够,这次必须换更彻底的方案并验证。"
                    .to_string()
            };
            format!("{lead}\n{}", render_failed_fixes(lesson))
        } else {
            String::new()
        };
        format!(
            "{icon} **{}**{freq}
   原因: {}
   规避: {}{escalate}

",
            lesson.title, lesson.root_cause, lesson.fix
        )
    } else {
        format!(
            "{icon} **{}**
   {}

",
            lesson.title, lesson.fix
        )
    }
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
                failed_fixes: Vec::new(),
                next_strategy: String::new(),
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
        .map(|e| normalize_signature(&crate::error_kb::classify_error(e).signature))
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
                failed_fixes: Vec::new(),
                next_strategy: String::new(),
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

/// One high-frequency or noteworthy pitfall, distilled for the lessons view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PitfallEntry {
    /// Short human title (already carries the signature for recognised errors).
    pub title: String,
    /// Stable dedup signature (`dependency/module-not-found/react-router-dom`).
    pub signature: String,
    /// How many times this exact pitfall has been hit across runs.
    pub hits: u32,
    /// Fix lifecycle: still-failing / unproven / validated.
    pub status: PitfallStatus,
    /// The recorded avoid-next-time fix.
    pub fix: String,
    /// Tech-stack fingerprint present when it was hit (its trigger context).
    pub context: Vec<String>,
    /// Fixes already tried that still let it recur — what UmaDev now steers AWAY
    /// from. Empty unless the pitfall recurred after a warning.
    pub failed_fixes: Vec<String>,
}

/// One pattern that passed the quality gate — a proven, reusable success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEntry {
    /// Pattern title (e.g. "Validated API contract for blog").
    pub title: String,
    /// Short body excerpt describing what was validated.
    pub summary: String,
}

/// A structured, language-neutral view of "what UmaDev has learned" — its
/// self-evolution made visible. Pure read; the CLI / TUI add i18n chrome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LessonsReport {
    /// Pitfall self-verification counters (total / validated / recurring / active).
    pub efficacy: PitfallEfficacySummary,
    /// High-frequency / noteworthy pitfalls, worst-first (recurring → most-hit).
    pub top_pitfalls: Vec<PitfallEntry>,
    /// Currently-avoided failing fixes — pitfalls whose recorded fix proved
    /// insufficient, so the base is now steered toward a different approach.
    pub recurring: Vec<PitfallEntry>,
    /// Validated success patterns (passed the quality gate, reusable).
    pub validated_patterns: Vec<ValidatedEntry>,
}

impl LessonsReport {
    /// `true` when nothing has been learned yet (drives the empty state).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.top_pitfalls.is_empty() && self.validated_patterns.is_empty()
    }
}

/// How many top pitfalls the lessons view surfaces. Generous enough to be
/// useful, capped so the view stays a digest rather than a dump.
const LESSONS_TOP_N: usize = 12;

/// Build a structured [`LessonsReport`] from the pitfall + validated-pattern KB.
///
/// Pure read — never mutates the learning state. Reads `dev-errors.jsonl`
/// (pitfalls) and `validated-decisions.jsonl` (proven patterns) under
/// `.umadev/learned/_raw/`. Fail-open: missing/empty files yield an empty
/// report so the caller shows a friendly empty state. Powers `umadev lessons`
/// and the TUI `/lessons` command.
#[must_use]
pub fn lessons_report(project_root: &Path) -> LessonsReport {
    let efficacy = pitfall_efficacy_summary(project_root);

    let mut pits: Vec<Lesson> = read_raw_lessons(project_root, DEV_ERRORS_FILE)
        .into_iter()
        .filter(|l| l.kind == LessonKind::DevError)
        .collect();
    // Worst first: recurring fixes, then most-frequently-hit, then recent —
    // the same ordering the `/pitfalls` overlay uses.
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

    let to_entry = |l: &Lesson| PitfallEntry {
        title: l.title.clone(),
        signature: l.signature.clone(),
        hits: l.hits(),
        status: l.pitfall_status(),
        fix: l.fix.clone(),
        context: l.context.clone(),
        failed_fixes: l
            .efficacy
            .as_ref()
            .map(|e| e.failed_fixes.clone())
            .unwrap_or_default(),
    };

    let recurring: Vec<PitfallEntry> = pits
        .iter()
        .filter(|l| l.pitfall_status() == PitfallStatus::Recurring)
        .map(to_entry)
        .collect();
    let top_pitfalls: Vec<PitfallEntry> = pits.iter().take(LESSONS_TOP_N).map(to_entry).collect();

    // Validated patterns: positive experience that cleared the quality gate.
    let mut validated_patterns: Vec<ValidatedEntry> =
        read_raw_lessons(project_root, "validated-decisions.jsonl")
            .into_iter()
            .filter(|l| l.kind == LessonKind::ValidatedPattern)
            .map(|l| ValidatedEntry {
                title: l.title,
                summary: truncate(l.body.lines().next().unwrap_or("").trim(), 160),
            })
            .collect();
    validated_patterns.truncate(LESSONS_TOP_N);

    LessonsReport {
        efficacy,
        top_pitfalls,
        recurring,
        validated_patterns,
    }
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
    fn normalize_signature_collapses_drift_but_keeps_distinct() {
        // Clean package names are preserved whole (no false merging).
        assert_eq!(
            normalize_signature("dependency/module-not-found/react-router-dom"),
            "dependency/module-not-found/react-router-dom"
        );
        // Path-prefixed module keys collapse to the trailing symbol, so the SAME
        // missing symbol imported from two different files dedups.
        assert_eq!(
            normalize_signature("dependency/module-not-found/components-foo"),
            "dependency/module-not-found/foo"
        );
        assert_eq!(
            normalize_signature("dependency/module-not-found/src-utils-foo"),
            normalize_signature("dependency/module-not-found/pages-foo")
        );
        // Version suffixes are volatile — a bump is not a new pitfall.
        assert_eq!(
            normalize_signature("dependency/module-not-found/lodash-v4"),
            "dependency/module-not-found/lodash"
        );
        assert_eq!(
            normalize_signature("dependency/module-not-found/react-18"),
            "dependency/module-not-found/react"
        );
        // Family-only signatures (no discriminator) are untouched.
        assert_eq!(
            normalize_signature("type/type-mismatch"),
            "type/type-mismatch"
        );
        // Idempotent — recall/resolve normalise the same key twice.
        let once = normalize_signature("dependency/module-not-found/components-foo");
        assert_eq!(normalize_signature(&once), once);
        // An all-volatile discriminator collapses to the family, never unique.
        assert_eq!(normalize_signature("general/error/42"), "general/error");
    }

    #[test]
    fn drifting_path_signatures_accumulate_occurrences() {
        // Regression for the signature-drift bug: the same root cause whose
        // offending path differs per file must bump `occurrences`, not fork into
        // separate single-hit rows. We can't easily make `classify_error` emit a
        // path discriminator, so assert the normalization the capture path uses
        // directly merges them.
        let a = normalize_signature("dependency/module-not-found/components-widget");
        let b = normalize_signature("dependency/module-not-found/pages-widget");
        assert_eq!(a, b, "same symbol, different importing dir → one signature");
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
    fn records_failed_fix_and_steers_away_next_time() {
        let tmp = TempDir::new().unwrap();
        let sig = "dependency/module-not-found/lodash";
        let err = vec!["Error: Cannot find module 'lodash'".to_string()];
        let failed_fix = |t: &std::path::Path| {
            read_raw_lessons(t, DEV_ERRORS_FILE)
                .into_iter()
                .find(|l| l.signature == sig)
                .and_then(|l| l.efficacy)
                .map(|e| e.failed_fixes)
                .unwrap_or_default()
        };

        // 1. First sighting + warn the worker.
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        let _ = relevant_lessons_for_prompt(tmp.path(), "无关一");
        assert!(
            failed_fix(tmp.path()).is_empty(),
            "no failed fix recorded yet"
        );

        // 2. It recurs DESPITE the warning → the recorded fix is logged as a
        //    tried-and-failed approach in the failed-fix ledger.
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        let ff = failed_fix(tmp.path());
        assert_eq!(ff.len(), 1, "the failed fix must be remembered: {ff:?}");
        assert!(
            ff[0].contains("install") || !ff[0].is_empty(),
            "recorded fix text captured"
        );

        // 3. The at-failure recall now explicitly steers AWAY from the failed
        //    fix instead of merely re-injecting it.
        let recall = lessons_for_error(tmp.path(), &err[0]);
        assert!(
            recall.contains("已试过但无效的修法"),
            "recurring recall must list the failed approach: {recall}"
        );

        // 4. The same failed fix is NOT recorded twice (deduped).
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        assert_eq!(failed_fix(tmp.path()).len(), 1, "failed fix deduped");
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
            invalidated: false,
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
    fn recency_weight_decays_with_age() {
        let now = Utc::now();
        let today = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let month_ago = (now - chrono::Duration::days(30))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let year_ago = (now - chrono::Duration::days(365))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let w_today = recency_weight(&today, now);
        let w_month = recency_weight(&month_ago, now);
        let w_year = recency_weight(&year_ago, now);
        assert!(
            w_today > w_month && w_month > w_year,
            "older → smaller weight"
        );
        // 30-day half-life → ~0.5 at one month.
        assert!((w_month - 0.5).abs() < 0.05, "half-life ≈ 30d: {w_month}");
        // Unparseable timestamp is treated as "now" (fail-open, weight 1.0).
        assert!((recency_weight("not-a-date", now) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn decay_score_prefers_recent_over_stale_same_relevance() {
        let now = Utc::now();
        let recent = (now - chrono::Duration::days(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let stale = (now - chrono::Duration::days(200))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let base = Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: "踩坑".into(),
            body: String::new(),
            fix: String::new(),
            root_cause: String::new(),
            keywords: vec!["dependency".into()],
            source_requirement: String::new(),
            first_seen: recent.clone(),
            signature: "dependency/module-not-found/lodash".into(),
            occurrences: 1,
            context: vec!["react".into()],
            efficacy: None,
            invalidated: false,
        };
        let stale_lesson = Lesson {
            first_seen: stale,
            ..base.clone()
        };
        // Same query/relevance, only age differs → recent must score higher.
        let q: std::collections::HashSet<String> = ["react".to_string(), "lodash".to_string()]
            .into_iter()
            .collect();
        let s_recent = lesson_decay_score(&base, &q, now);
        let s_stale = lesson_decay_score(&stale_lesson, &q, now);
        assert!(
            s_recent > s_stale,
            "recent lesson must outrank stale (same relevance): {s_recent} vs {s_stale}"
        );
    }

    #[test]
    fn importance_rewards_recurring_and_damps_validated() {
        let mk = |eff: Option<PitfallEfficacy>| Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: "踩坑".into(),
            body: String::new(),
            fix: String::new(),
            root_cause: String::new(),
            keywords: vec![],
            source_requirement: String::new(),
            first_seen: "2026-06-21T00:00:00Z".into(),
            signature: "dependency/module-not-found/lodash".into(),
            occurrences: 3,
            context: vec![],
            efficacy: eff,
            invalidated: false,
        };
        let recurring = mk(Some(PitfallEfficacy {
            injected: 1,
            occ_at_injection: 1,
            recurred_after_warning: true,
            proven_fix: false,
            failed_fixes: Vec::new(),
            next_strategy: String::new(),
        }));
        let validated = mk(Some(PitfallEfficacy {
            injected: 2,
            occ_at_injection: 3,
            recurred_after_warning: false,
            proven_fix: true,
            failed_fixes: Vec::new(),
            next_strategy: String::new(),
        }));
        assert!(
            lesson_importance(&recurring) > lesson_importance(&validated),
            "a failing (recurring) fix must outweigh a handled (validated) one"
        );
    }

    #[test]
    fn reflection_records_strategy_only_for_recurring_and_surfaces_it() {
        // A recurring pitfall (recurred after a warning) is the ONLY case that
        // warrants a reflected strategy; record it, then confirm it backfills the
        // efficacy record, the per-signature sliding window, and the recall text.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let recurring = Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: "踩坑 [dependency/module-not-found/lodash]".into(),
            body: String::new(),
            fix: "npm install lodash".into(),
            root_cause: "missing dependency".into(),
            keywords: vec![],
            source_requirement: "r".into(),
            first_seen: "2026-06-21T00:00:00Z".into(),
            signature: "dependency/module-not-found/lodash".into(),
            occurrences: 4,
            context: vec!["lodash".into()],
            efficacy: Some(PitfallEfficacy {
                injected: 1,
                occ_at_injection: 1,
                recurred_after_warning: true, // ← already warned, still recurred
                proven_fix: false,
                failed_fixes: vec!["npm install lodash".into()],
                next_strategy: String::new(),
            }),
            invalidated: false,
        };
        write_raw_lessons(root, DEV_ERRORS_FILE, std::slice::from_ref(&recurring));

        // A FIRST failure (status Active) must NOT trigger reflection — the
        // gate returns None so the caller keeps the cheap template path.
        let active = Lesson {
            efficacy: Some(PitfallEfficacy {
                injected: 0,
                occ_at_injection: 4,
                recurred_after_warning: false,
                proven_fix: false,
                failed_fixes: Vec::new(),
                next_strategy: String::new(),
            }),
            ..recurring.clone()
        };
        write_raw_lessons(tmp.path().join("active").as_path(), DEV_ERRORS_FILE, &[]);
        let active_root = tmp.path().join("active");
        std::fs::create_dir_all(active_root.join(RAW_DIR)).unwrap();
        write_raw_lessons(&active_root, DEV_ERRORS_FILE, std::slice::from_ref(&active));
        assert!(
            recurring_pitfall_for_error(&active_root, "Cannot find module 'lodash'").is_none(),
            "a first failure (Active) must not gate a reflection"
        );

        // The recurring store DOES gate reflection.
        let matched = recurring_pitfall_for_error(root, "Cannot find module 'lodash'")
            .expect("recurring pitfall must gate reflection");
        assert_eq!(matched.signature, "dependency/module-not-found/lodash");
        let (system, user) = reflection_prompt(&matched);
        assert!(system.to_lowercase().contains("different"));
        // The prompt must steer AWAY from the already-failed fix.
        assert!(user.contains("npm install lodash"));

        // Record a base-generated strategy and confirm the backfill.
        let strategy = "Pin the dependency in package.json and run a clean install so the lockfile resolves it deterministically.";
        assert!(record_pitfall_strategy(
            root,
            "dependency/module-not-found/lodash",
            strategy
        ));

        // 1) efficacy.next_strategy is set on the stored pitfall.
        let stored = read_raw_lessons(root, DEV_ERRORS_FILE);
        let eff = stored[0].efficacy.as_ref().unwrap();
        assert_eq!(eff.next_strategy, strategy);

        // 2) the sliding window has exactly one reflection for this signature.
        let win_dir = root.join(REFLECTIONS_DIR);
        let entries: Vec<_> = std::fs::read_dir(&win_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "one reflection file for the signature");

        // 3) recall surfaces the strategy ahead of the bare template line.
        let recall = lessons_for_error(root, "Cannot find module 'lodash'");
        assert!(
            recall.contains(strategy),
            "lessons_for_error must surface the reflected strategy: {recall}"
        );

        // An empty strategy is a no-op (fail-open).
        assert!(!record_pitfall_strategy(
            root,
            "dependency/module-not-found/lodash",
            "   "
        ));

        // The sliding window keeps only the most recent MAX_REFLECTIONS_PER_SIG.
        for n in 0..MAX_REFLECTIONS_PER_SIG + 2 {
            record_pitfall_strategy(
                root,
                "dependency/module-not-found/lodash",
                &format!("strategy variant {n}"),
            );
        }
        let slug: String = "dependency/module-not-found/lodash"
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let win = std::fs::read_to_string(win_dir.join(format!("{slug}.jsonl"))).unwrap();
        let kept = win.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(kept, MAX_REFLECTIONS_PER_SIG, "window is capped");
    }

    #[test]
    fn prune_evicts_stale_validated_before_recent_failing() {
        let now = Utc::now();
        let recent = (now - chrono::Duration::days(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let ancient = (now - chrono::Duration::days(400))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let mut store: Vec<Lesson> = Vec::new();
        // One recent, still-failing pitfall we MUST keep.
        store.push(Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: "KEEP-recurring".into(),
            body: String::new(),
            fix: String::new(),
            root_cause: String::new(),
            keywords: vec![],
            source_requirement: "r".into(),
            first_seen: recent,
            signature: "dependency/module-not-found/keep".into(),
            occurrences: 5,
            context: vec![],
            efficacy: Some(PitfallEfficacy {
                injected: 1,
                occ_at_injection: 1,
                recurred_after_warning: true,
                proven_fix: false,
                failed_fixes: Vec::new(),
                next_strategy: String::new(),
            }),
            invalidated: false,
        });
        // Fill past the cap with ancient, validated (handled) pitfalls.
        for n in 0..MAX_DEV_PITFALLS + 10 {
            store.push(Lesson {
                kind: LessonKind::DevError,
                domain: "dependency".into(),
                title: format!("drop-{n}"),
                body: String::new(),
                fix: String::new(),
                root_cause: String::new(),
                keywords: vec![],
                source_requirement: "r".into(),
                first_seen: ancient.clone(),
                signature: format!("dependency/module-not-found/old-{n}"),
                occurrences: 1,
                context: vec![],
                efficacy: Some(PitfallEfficacy {
                    injected: 2,
                    occ_at_injection: 1,
                    recurred_after_warning: false,
                    proven_fix: true,
                    failed_fixes: Vec::new(),
                    next_strategy: String::new(),
                }),
                invalidated: false,
            });
        }
        prune_pitfalls(&mut store);
        assert!(store.len() <= MAX_DEV_PITFALLS);
        assert!(
            store.iter().any(|l| l.title == "KEEP-recurring"),
            "a recent still-failing pitfall must survive eviction of stale validated ones"
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
            invalidated: false,
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
    fn capture_tech_debt_feeds_significant_debt_into_kb() {
        use crate::tech_debt::{DebtItem, DebtKind, DebtStatus};
        let tmp = TempDir::new().unwrap();
        let mk = |kind: DebtKind, line: u32| DebtItem {
            file: "output/demo-prd.md".into(),
            line,
            kind,
            snippet: "x".into(),
            first_seen: "2026-06-21T00:00:00Z".into(),
            status: DebtStatus::Open,
            resolved_at: String::new(),
        };
        let items = vec![
            mk(DebtKind::FillerText, 3),          // sev 5 → captured
            mk(DebtKind::FillerText, 7),          // same kind → folded into one lesson
            mk(DebtKind::UnfilledAcceptance, 12), // sev 4 → captured
            mk(DebtKind::Todo, 20),               // sev 2 → BELOW threshold, skipped
        ];
        let n = capture_tech_debt(tmp.path(), &items, "博客系统");
        assert_eq!(
            n, 2,
            "two significant kinds captured, low-severity TODO skipped"
        );
        let raw = read_raw_lessons(tmp.path(), "tech-debt.jsonl");
        assert_eq!(raw.len(), 2);
        assert!(raw.iter().all(|l| l.kind == LessonKind::Failure));
        assert!(raw.iter().all(|l| l.domain == "governance"));
        // The filler lesson folds the 2 occurrences into one row with a count.
        let filler = raw
            .iter()
            .find(|l| l.title.contains("Filler"))
            .expect("filler lesson present");
        assert!(
            filler.title.contains("2×"),
            "occurrences folded: {}",
            filler.title
        );

        // It joins the sediment loop: read_all + sediment must surface it.
        let all = read_all_raw_lessons(tmp.path());
        assert!(all.iter().any(|l| l.title.contains("Tech debt")));
        let written = sediment_lessons(tmp.path());
        assert!(written >= 2, "tech-debt lessons sediment to markdown");
    }

    #[test]
    fn capture_tech_debt_skips_when_no_significant_debt() {
        use crate::tech_debt::{DebtItem, DebtKind, DebtStatus};
        let tmp = TempDir::new().unwrap();
        let items = vec![DebtItem {
            file: "output/x.md".into(),
            line: 1,
            kind: DebtKind::Todo, // sev 2, below threshold
            snippet: "TODO".into(),
            first_seen: "t".into(),
            status: DebtStatus::Open,
            resolved_at: String::new(),
        }];
        assert_eq!(capture_tech_debt(tmp.path(), &items, "r"), 0);
        assert!(read_raw_lessons(tmp.path(), "tech-debt.jsonl").is_empty());
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
    fn sediment_invalidates_kb_index_cache() {
        let tmp = TempDir::new().unwrap();
        // Pre-seed a stale kb-index signature file (as if an index was already
        // built+cached BEFORE this run learned anything new).
        let kb_dir = tmp.path().join(".umadev/kb-index");
        fs::create_dir_all(&kb_dir).unwrap();
        let sig = kb_dir.join("bm25.sig");
        fs::write(&sig, "stale-signature").unwrap();
        assert!(sig.is_file());

        // Sediment new lessons → the stale cache signature must be removed so the
        // next retrieval rebuilds and can see what we just learned this run.
        capture_quality_failures(
            tmp.path(),
            &[check("OpenAPI contract", "failed", 0)],
            "demo",
            "api contract openapi",
        );
        let written = sediment_lessons(tmp.path());
        assert!(written > 0, "sediment should write at least one lesson");
        assert!(
            !sig.exists(),
            "sediment must invalidate the stale kb-index cache signature"
        );
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

    #[test]
    fn lessons_report_empty_for_fresh_workspace() {
        let tmp = TempDir::new().unwrap();
        let r = lessons_report(tmp.path());
        assert!(r.is_empty());
        assert!(r.top_pitfalls.is_empty());
        assert!(r.validated_patterns.is_empty());
        assert_eq!(r.efficacy.total, 0);
    }

    #[test]
    fn lessons_report_surfaces_pitfalls_and_frequency() {
        let tmp = TempDir::new().unwrap();
        // Seed a recurring pitfall (hit twice) + a one-off pitfall.
        let first = vec![
            "Error: Cannot find module 'react-router-dom'".to_string(),
            "TypeError: Cannot read properties of undefined (reading 'map')".to_string(),
        ];
        capture_dev_errors(tmp.path(), &first, "demo", "需求");
        let again = vec!["Error: Cannot find module 'react-router-dom'".to_string()];
        capture_dev_errors(tmp.path(), &again, "demo", "需求");

        let r = lessons_report(tmp.path());
        assert!(!r.is_empty());
        assert_eq!(r.efficacy.total, 2, "two distinct pitfalls recorded");
        // Worst-first ordering puts the twice-hit react-router pitfall first.
        let top = &r.top_pitfalls[0];
        assert_eq!(
            top.signature,
            "dependency/module-not-found/react-router-dom"
        );
        assert_eq!(top.hits, 2, "frequency reflects the recurrence");
    }

    #[test]
    fn lessons_report_surfaces_validated_patterns() {
        let tmp = TempDir::new().unwrap();
        let spec = umadev_contract::parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | /api/posts | - | - | none | List |\n",
            "blog",
        );
        capture_validated_patterns(tmp.path(), "blog", "做一个博客", &spec);
        let r = lessons_report(tmp.path());
        assert!(!r.is_empty());
        assert_eq!(r.validated_patterns.len(), 1);
        assert!(r.validated_patterns[0].title.contains("blog"));
    }

    // ---- Memory reconcile (Task 2) ------------------------------------------

    /// Hand-write two same-domain, keyword-overlapping lessons with an OLD and a
    /// NEW timestamp into the quality-failures ledger, so the newer one is the
    /// "fresh" lesson and the older one is its similar neighbour.
    fn seed_two_similar(root: &Path) -> (Lesson, Lesson) {
        let mk = |title: &str, when: &str| Lesson {
            kind: LessonKind::Failure,
            domain: "api".into(),
            title: title.into(),
            body: String::new(),
            fix: format!("fix for {title}"),
            root_cause: String::new(),
            keywords: vec!["api".into(), "contract".into(), "openapi".into()],
            source_requirement: "做一个博客".into(),
            first_seen: when.into(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: None,
            invalidated: false,
        };
        let old = mk("OLD api lesson", "2026-06-01T00:00:00Z");
        let fresh = mk("FRESH api lesson", "2026-06-20T00:00:00Z");
        write_raw_lessons(
            root,
            "quality-failures.jsonl",
            &[old.clone(), fresh.clone()],
        );
        (old, fresh)
    }

    #[test]
    fn parse_reconcile_decision_is_tolerant() {
        assert_eq!(parse_reconcile_decision("ADD"), ReconcileDecision::Add);
        assert_eq!(
            parse_reconcile_decision("verdict: UPDATE — better version"),
            ReconcileDecision::Update
        );
        assert_eq!(
            parse_reconcile_decision("INVALIDATE, the old one is wrong"),
            ReconcileDecision::Invalidate
        );
        // Unknown / empty → NOOP (fail-open: never mutate on an unclear verdict).
        assert_eq!(
            parse_reconcile_decision("hmm not sure"),
            ReconcileDecision::Noop
        );
        assert_eq!(parse_reconcile_decision(""), ReconcileDecision::Noop);
    }

    #[test]
    fn reconcile_candidates_pairs_fresh_with_older_similar() {
        let tmp = TempDir::new().unwrap();
        seed_two_similar(tmp.path());
        let pairs = reconcile_candidates(tmp.path());
        // Only the fresh lesson has an OLDER similar neighbour; the oldest one has
        // none after it, so exactly one pair is produced.
        assert_eq!(pairs.len(), 1, "one fresh→older pair: {pairs:?}");
        assert_eq!(pairs[0].0.title, "FRESH api lesson");
        assert_eq!(pairs[0].1.len(), 1);
        assert_eq!(pairs[0].1[0].title, "OLD api lesson");
    }

    #[test]
    fn reconcile_noop_and_no_base_keep_pure_append() {
        let tmp = TempDir::new().unwrap();
        let (_old, _fresh) = seed_two_similar(tmp.path());

        // No-base path: sediment_lessons must behave EXACTLY like pure append —
        // both lessons survive (nothing invalidated) and both sediment.
        let written = sediment_lessons(tmp.path());
        assert_eq!(written, 2, "no-base sediment writes both lessons");
        assert!(read_raw_lessons(tmp.path(), "quality-failures.jsonl")
            .iter()
            .all(|l| !l.invalidated));

        // An explicit NOOP judge is identical — nothing is invalidated.
        let noop = |_f: &Lesson, _s: &[Lesson]| ReconcileDecision::Noop;
        let _ = sediment_lessons_with_judge(tmp.path(), Some(&noop));
        assert!(read_raw_lessons(tmp.path(), "quality-failures.jsonl")
            .iter()
            .all(|l| !l.invalidated));
    }

    #[test]
    fn reconcile_add_keeps_both_lessons() {
        let tmp = TempDir::new().unwrap();
        seed_two_similar(tmp.path());
        let add = |_f: &Lesson, _s: &[Lesson]| ReconcileDecision::Add;
        let written = sediment_lessons_with_judge(tmp.path(), Some(&add));
        // ADD = genuinely new knowledge → neither lesson invalidated; both kept.
        assert_eq!(written, 2, "ADD keeps both lessons");
        assert!(read_raw_lessons(tmp.path(), "quality-failures.jsonl")
            .iter()
            .all(|l| !l.invalidated));
    }

    #[test]
    fn reconcile_update_supersedes_the_older_lesson() {
        let tmp = TempDir::new().unwrap();
        seed_two_similar(tmp.path());
        // UPDATE: the fresh lesson is a better version of the older one → the
        // OLDER similar lesson is marked invalid (kept on disk, not deleted).
        let update = |_f: &Lesson, _s: &[Lesson]| ReconcileDecision::Update;
        let written = sediment_lessons_with_judge(tmp.path(), Some(&update));
        let rows = read_raw_lessons(tmp.path(), "quality-failures.jsonl");
        // The old lesson is still ON DISK (audit posture) but marked invalid.
        let old = rows.iter().find(|l| l.title == "OLD api lesson").unwrap();
        assert!(old.invalidated, "UPDATE marks the superseded prior invalid");
        let fresh = rows.iter().find(|l| l.title == "FRESH api lesson").unwrap();
        assert!(!fresh.invalidated, "the fresh lesson stays valid");
        // Only the surviving fresh lesson sediments to markdown.
        assert_eq!(
            written, 1,
            "the invalidated prior is excluded from sediment"
        );
    }

    #[test]
    fn reconcile_invalidate_marks_contradicted_prior() {
        let tmp = TempDir::new().unwrap();
        seed_two_similar(tmp.path());
        // INVALIDATE: the fresh lesson shows the older one is now WRONG.
        let inval = |_f: &Lesson, _s: &[Lesson]| ReconcileDecision::Invalidate;
        let _ = sediment_lessons_with_judge(tmp.path(), Some(&inval));
        let rows = read_raw_lessons(tmp.path(), "quality-failures.jsonl");
        assert_eq!(rows.len(), 2, "both rows kept on disk (never deleted)");
        let old = rows.iter().find(|l| l.title == "OLD api lesson").unwrap();
        assert!(old.invalidated, "INVALIDATE marks the contradicted prior");
    }

    #[test]
    fn reconcile_skips_pitfalls() {
        // Dev-error pitfalls are governed by their own efficacy loop — reconcile
        // must never touch them, even with an aggressive INVALIDATE judge.
        let tmp = TempDir::new().unwrap();
        let err = vec!["Error: Cannot find module 'react-router-dom'".to_string()];
        capture_dev_errors(tmp.path(), &err, "demo", "做一个博客");
        let pairs = reconcile_candidates(tmp.path());
        assert!(pairs.is_empty(), "pitfalls are excluded from reconcile");
        let inval = |_f: &Lesson, _s: &[Lesson]| ReconcileDecision::Invalidate;
        let _ = sediment_lessons_with_judge(tmp.path(), Some(&inval));
        assert!(read_raw_lessons(tmp.path(), DEV_ERRORS_FILE)
            .iter()
            .all(|l| !l.invalidated));
    }

    #[test]
    fn reconcile_decay_drops_ancient_unreinforced_lesson() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // One ancient lesson (well past 5 half-lives) with a unique domain so it
        // has no similar neighbour to reconcile against — only the decay pass can
        // affect it.
        let ancient = Lesson {
            kind: LessonKind::Failure,
            domain: "devops".into(),
            title: "ancient devops lesson".into(),
            body: String::new(),
            fix: "f".into(),
            root_cause: String::new(),
            keywords: vec!["docker".into()],
            source_requirement: "r".into(),
            first_seen: "2000-01-01T00:00:00Z".into(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: None,
            invalidated: false,
        };
        write_raw_lessons(
            root,
            "quality-failures.jsonl",
            std::slice::from_ref(&ancient),
        );
        // No-base path: decay is OFF, so the ancient lesson still sediments
        // (behaviour preserved).
        assert_eq!(sediment_lessons(root), 1, "no-base keeps ancient lesson");
        // With a judge active, the decay pass drops the ancient, never-reinforced
        // lesson from the sediment top-k (it stays in the raw ledger).
        let noop = |_f: &Lesson, _s: &[Lesson]| ReconcileDecision::Noop;
        let written = sediment_lessons_with_judge(root, Some(&noop));
        assert_eq!(written, 0, "decay drops the ancient lesson from sediment");
        // Still recoverable in the raw ledger (not physically removed).
        assert_eq!(read_raw_lessons(root, "quality-failures.jsonl").len(), 1);
    }
}
