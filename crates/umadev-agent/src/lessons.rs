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
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::fswalk::{classify_no_follow, EntryKind};
use crate::memory_control::{capture_enabled, recall_enabled, MemoryScope, MemoryStore};
use crate::phases::QualityCheck;
use umadev_contract::ApiSpec;

const MAX_SEDIMENT_MARKDOWN_BYTES: usize = 512 * 1024;

fn project_capture_enabled(project_root: &Path, store: MemoryStore) -> bool {
    capture_enabled(project_root, MemoryScope::Project, store)
}

fn project_recall_enabled(project_root: &Path, store: MemoryStore) -> bool {
    recall_enabled(project_root, MemoryScope::Project, store)
}

fn raw_lesson_store(filename: &str) -> Option<MemoryStore> {
    match filename {
        "quality-failures.jsonl" => Some(MemoryStore::QualityFailures),
        "gate-revisions.jsonl" => Some(MemoryStore::GateRevisions),
        "validated-decisions.jsonl" => Some(MemoryStore::ValidatedPatterns),
        "tech-debt.jsonl" => Some(MemoryStore::TechDebt),
        DEV_ERRORS_FILE => Some(MemoryStore::Pitfalls),
        BELIEFS_FILE => Some(MemoryStore::Beliefs),
        _ => None,
    }
}

static RAW_STORE_PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct RawStoreLock {
    _process: std::sync::MutexGuard<'static, ()>,
    _cross_process: umadev_state::store_lock::StoreLock,
}

fn acquire_raw_store_lock(project_root: &Path, filename: &str) -> Option<RawStoreLock> {
    let store = raw_lesson_store(filename)?;
    // The filesystem lease is cross-process but intentionally non-queued. A
    // tight writer loop in one thread could otherwise repeatedly reacquire it
    // and starve a sibling until the bounded lease timeout. Queue contenders
    // inside this process first, then take the authoritative cross-process lock.
    let process = RAW_STORE_PROCESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match umadev_state::store_lock::acquire(project_root, store) {
        Ok(cross_process) => Some(RawStoreLock {
            _process: process,
            _cross_process: cross_process,
        }),
        Err(error) => {
            tracing::warn!(
                store = store.id(),
                %error,
                "lesson store write not committed because its cross-process lock was unavailable"
            );
            None
        }
    }
}

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
/// Raw JSONL file holding folded **beliefs** — higher-level rules distilled from
/// N similar raw lessons (see [`fold_beliefs`]). Kept in its own file so the
/// belief layer can be rebuilt/scanned without touching the raw ledgers, and so
/// `read_all_raw_lessons` can include beliefs as first-class retrieval candidates.
pub const BELIEFS_FILE: &str = "beliefs.jsonl";
/// Where per-signature reflection logs live — the sliding window of
/// base-generated correction strategies for pitfalls that recurred after a
/// warning. One JSONL file per (normalised) signature.
pub const REFLECTIONS_DIR: &str = ".umadev/reflections";
/// How many recent reflections to retain per signature. Small — we only need
/// the latest distilled strategy plus a little history for context, not a full
/// audit trail (the audit log already covers that).
const MAX_REFLECTIONS_PER_SIG: usize = 3;
const MAX_RAW_LEDGER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RAW_LINE_BYTES: usize = 256 * 1024;
const MAX_RAW_LEDGER_LINES: usize = 10_000;
const MAX_REFLECTION_LEDGER_BYTES: u64 = 256 * 1024;
const MAX_REFLECTION_LINE_BYTES: usize = 64 * 1024;
const MAX_REFLECTION_LEDGER_LINES: usize = 64;
static ADR_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// A higher-level **belief**: one rule folded from N similar raw lessons (see
    /// [`fold_beliefs`]). A belief is denser than the lessons it summarises and
    /// carries an `evidence_count` (how many raw lessons support it) and a
    /// `last_confirmed` freshness stamp (its [`Lesson::first_seen`] is reused for
    /// the latter). Retrieval prefers beliefs and demotes their raw evidence, so
    /// the prompt sees the distilled rule instead of several near-duplicates.
    Belief,
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
    /// ISO-8601 UTC timestamp of the FIRST observation. This is immutable after
    /// capture: refreshing a field named `first_seen` on a later reward made the
    /// audit trail lie about when a pitfall was originally learned. Dev-error
    /// recency is derived from its explicit observation timeline instead.
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
    /// Pitfall observation/fix-attempt lifecycle plus legacy explicit feedback
    /// counters. Production settlement is exact only for a committed pitfall
    /// repair token. Passive lesson/chunk recall does not update `helpful`,
    /// `harmful`, or `trust`, because prompt assembly cannot yet return exact IDs
    /// that reached one host turn.
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
    /// Legacy/explicit trust signal in `[0, 1]`. Production passive recall leaves
    /// it neutral; only callers that already possess a causally exact identity may
    /// use the low-level feedback API. `0` is remapped to [`NEUTRAL_TRUST`] so old
    /// or never-settled rows are never treated as fully distrusted.
    #[serde(default)]
    pub trust: f32,
    /// For a [`LessonKind::Belief`]: how many raw lessons this belief folds. A
    /// belief with `evidence_count = 5` summarises five near-duplicate lessons
    /// into one denser rule. `0` for every non-belief row (and for legacy rows),
    /// normalised to "no evidence beyond itself" by callers. The list of which
    /// lessons it folds is tracked separately in [`Lesson::evidence`].
    #[serde(default)]
    pub evidence_count: u32,
    /// For a [`LessonKind::Belief`]: the stable internal lesson-identity keys of
    /// the raw lessons it folds, so retrieval can demote those exact originals as
    /// "evidence" and re-folding can recognise already-covered lessons. Each entry
    /// is `"domain\u{0}title"` (domain + title; `first_seen` is intentionally
    /// omitted so a recurrence under a refreshed timestamp still matches). Empty
    /// for non-belief rows. `#[serde(default)]` keeps older rows readable.
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// Neutral trust for an un-attributed lesson.
pub const NEUTRAL_TRUST: f32 = 0.5;

/// Reward used only by explicit low-level feedback callers/tests.
const TRUST_REWARD: f32 = 0.05;

/// Penalty used only by explicit low-level feedback callers/tests.
const TRUST_PENALTY: f32 = 0.10;

/// Lower clamp on trust — a lesson never reaches exactly 0 (which would zero out
/// its whole decay score and make it un-recoverable even if it later proves
/// useful). The floor keeps a heavily-distrusted lesson barely alive so a later
/// reward can resurrect it.
const TRUST_FLOOR: f32 = 0.05;

/// Minimum OUTCOME observations (helpful + harmful) before efficacy can DEMOTE a
/// lesson out of recall. Below this a lesson is never pruned on efficacy — a
/// single (or few) bad outcome is noise, not proof the advice is poison.
/// Conservative by design: pruning is irreversible-looking to the user, so it
/// must wait for a real sample.
const EFFICACY_MIN_SAMPLES: u32 = 4;

/// Helpful ratio `helpful / (helpful + harmful)` at/below which a WELL-SAMPLED
/// lesson is treated as poison and demoted below the recall cut. `0.25` ≈ it
/// coincided with a FAILURE roughly three times for every pass — strong evidence
/// its advice hurts more than it helps. Conservative so an occasionally-unlucky
/// but genuinely useful lesson is not culled.
const EFFICACY_POISON_RATIO: f64 = 0.25;

/// Tracks pitfall episodes and exact repair-attempt settlement. Older fields are
/// retained for JSON compatibility, but production never infers success from
/// absence of recurrence or from passive prompt recall.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PitfallEfficacy {
    /// `1` means outcome fields prefixed `exact_` came from an exact committed
    /// repair token. Missing/zero rows are legacy broad-attribution data and are
    /// retained for audit but behaviorally neutral.
    #[serde(default)]
    pub outcome_attribution_version: u8,
    /// Legacy/explicit count; passive retrieval does not increment it.
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
    /// Populated only by the recurrence-reflection path when the pitfall recurred after
    /// a warning, and surfaced ahead of the failed-fix ledger on the next match.
    /// Empty until reflection runs. `#[serde(default)]` keeps older rows readable.
    #[serde(default)]
    pub next_strategy: String,
    /// Legacy/explicit helpful tally. Production passive recall does not mutate
    /// it; retained for compatibility and callers with their own exact identity.
    #[serde(default)]
    pub helpful: u32,
    /// Legacy/explicit harmful tally; not written by production passive recall.
    #[serde(default)]
    pub harmful: u32,
    /// Exact-token helpful outcomes (trusted for behavior only at version 1).
    #[serde(default)]
    pub exact_helpful: u32,
    /// Exact-token same-signature failures (trusted only at version 1).
    #[serde(default)]
    pub exact_harmful: u32,
    /// Most recent independent failure episode after the first observation.
    /// Empty for a one-off and for legacy rows written before timeline tracking.
    #[serde(default)]
    pub last_recurred_at: String,
    /// Most recent exact settlement in which a committed repair attempt failed.
    #[serde(default)]
    pub last_fix_failed_at: String,
    /// Most recent time an actual post-fix build/test pass proved the fix worked.
    /// Passive recall never writes this field.
    #[serde(default)]
    pub last_verified_at: String,
    /// Versioned exact-token failure timestamp. Legacy unversioned timestamps
    /// never influence lifecycle.
    #[serde(default)]
    pub exact_last_fix_failed_at: String,
    /// Versioned exact-token mechanical-pass timestamp.
    #[serde(default)]
    pub exact_last_verified_at: String,
    /// Bounded provenance window for the episodes contributing to `occurrences`.
    /// Raw stderr is deliberately not retained: the stable evidence hash and
    /// episode id make the count auditable without copying secrets/log bulk.
    #[serde(default)]
    pub recent_observations: Vec<PitfallObservation>,
    /// Latest successful outcome attributed to this lesson for usage-decay.
    /// Kept separate so `Lesson::first_seen` remains immutable/auditable.
    #[serde(default)]
    pub last_reinforced_at: String,
    /// Versioned exact-token reinforcement timestamp used by behavior.
    #[serde(default)]
    pub exact_last_reinforced_at: String,
    /// Attempt tokens committed to a real host turn but not yet settled by its
    /// deterministic acceptance result. A token is consumed exactly once.
    #[serde(default)]
    pub pending_fix_attempts: Vec<String>,
}

/// One auditable, privacy-bounded pitfall observation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PitfallObservation {
    /// ISO-8601 UTC time at which the capture episode was committed.
    pub observed_at: String,
    /// Process-local unique id for one capture episode/turn.
    pub episode_id: String,
    /// Stable opaque hash of the representative error evidence (raw text omitted).
    pub evidence_hash: String,
    /// Typed source that produced the evidence (for example a host tool failure
    /// or UmaDev's exact mechanical repair verifier).
    #[serde(default)]
    pub source: String,
    /// Evidence-producing base/component. This is never inferred from model
    /// prose; the capture/settlement boundary writes it explicitly.
    #[serde(default)]
    pub base: String,
    /// Version of [`Self::base`] that produced the evidence.
    #[serde(default)]
    pub base_version: String,
    /// Privacy-safe workspace ownership scope (`project:<opaque hash>`).
    #[serde(default)]
    pub workspace_scope: String,
    /// Whether this row is an observation, a causally attributed repair
    /// success/failure, or a same-id contradiction.
    #[serde(default)]
    pub outcome: KnowledgeEvidenceOutcome,
    /// Exact repair-attempt token for success/failure outcomes. Empty for a
    /// plain observation.
    #[serde(default)]
    pub causal_attempt_id: String,
}

/// Typed outcome carried by one knowledge evidence record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeEvidenceOutcome {
    /// One independent failure/experience observation.
    #[default]
    Observed,
    /// A committed repair token passed its matching mechanical verifier.
    FixSucceeded,
    /// A committed repair token still failed with the same signature.
    FixFailed,
    /// The same evidence id arrived with incompatible content/outcomes.
    Conflict,
}

/// Auditable knowledge lifecycle. Similarity may group candidate evidence but
/// cannot produce [`Self::Validated`]; only a complete causal success can.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitfallStatus {
    /// One observation, or legacy aggregate data without independent evidence.
    #[default]
    Hypothesis,
    /// At least two distinct evidence ids support the same incident/rule.
    Corroborated,
    /// At least one complete, causally attributed repair success and no newer
    /// conflicting failure.
    Validated,
    /// Explicit invalidation, a same-id contradiction, or a newer causal
    /// failure that revoked a prior successful rule.
    Invalidated,
}

impl PitfallStatus {
    /// Compatibility alias for callers compiled against the pre-state-machine
    /// API. New code should use [`Self::Hypothesis`].
    #[allow(non_upper_case_globals)]
    pub const Active: Self = Self::Hypothesis;
    /// Compatibility alias for the old "recurring fix" state. A repair that
    /// failed after validation now invalidates that advice.
    #[allow(non_upper_case_globals)]
    pub const Recurring: Self = Self::Invalidated;
}

/// Evidence available when settling one committed pitfall repair turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PitfallFixAttemptResult {
    /// The corresponding mechanical gate passed (and was not skipped).
    Passed,
    /// The gate failed with its actual stderr/evidence. Settlement verifies that
    /// this evidence still contains the attempt's exact normalized signature.
    VerificationFailed(String),
    /// Skip, unavailable verifier, interrupted turn, or otherwise no causal
    /// pass/fail evidence for the attempted signature.
    Unknown,
}

/// Exact-once result of consuming an attempt token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PitfallFixSettlement {
    /// The exact attempt was mechanically verified as passing.
    Passed,
    /// Verification still contained the exact attempted pitfall signature.
    SameSignatureFailed,
    /// The token was consumed, but evidence was absent or only showed another
    /// error; no trust or fix-lifecycle state changed.
    Inconclusive,
    /// The token was empty, unknown, already consumed, or no longer actionable.
    NotFound,
}

impl Lesson {
    /// Occurrence count, normalised so legacy rows (stored 0) count as 1.
    #[must_use]
    pub fn hits(&self) -> u32 {
        self.occurrences.max(1)
    }

    /// First observation timestamp (legacy field name retained on disk).
    #[must_use]
    pub fn first_observed_at(&self) -> &str {
        self.first_seen.as_str()
    }

    /// Latest recurrence timestamp, when timeline data exists.
    #[must_use]
    pub fn last_recurred_at(&self) -> Option<&str> {
        self.efficacy
            .as_ref()
            .map(|e| e.last_recurred_at.trim())
            .filter(|s| !s.is_empty())
    }

    /// Latest mechanically verified fix timestamp, when available.
    #[must_use]
    pub fn last_verified_at(&self) -> Option<&str> {
        self.efficacy.as_ref().and_then(|efficacy| {
            efficacy
                .recent_observations
                .iter()
                .filter(|evidence| {
                    evidence.outcome == KnowledgeEvidenceOutcome::FixSucceeded
                        && evidence_has_complete_causal_provenance(evidence)
                })
                .max_by(|left, right| left.observed_at.cmp(&right.observed_at))
                .map(|evidence| evidence.observed_at.as_str())
        })
    }

    fn last_observed_at(&self) -> &str {
        self.audited_last_observed_at()
            .unwrap_or_else(|| self.first_observed_at())
    }

    fn last_reinforced_at(&self) -> Option<&str> {
        self.efficacy
            .as_ref()
            .filter(|e| e.outcome_attribution_version == 1)
            .map(|e| e.exact_last_reinforced_at.trim())
            .filter(|s| !s.is_empty())
    }

    fn audited_last_observed_at(&self) -> Option<&str> {
        self.efficacy
            .as_ref()
            .into_iter()
            .flat_map(|efficacy| efficacy.recent_observations.iter())
            .map(|evidence| evidence.observed_at.trim())
            .filter(|at| !at.is_empty())
            .chain(self.last_recurred_at())
            .chain((self.hits() <= 1).then(|| self.first_observed_at()))
            .max()
    }

    fn audited_first_observed_at(&self) -> &str {
        self.efficacy
            .as_ref()
            .into_iter()
            .flat_map(|efficacy| efficacy.recent_observations.iter())
            .map(|evidence| evidence.observed_at.trim())
            .chain(std::iter::once(self.first_observed_at()))
            .filter(|at| !at.is_empty())
            .min()
            .unwrap_or_default()
    }

    fn timeline_complete(&self) -> bool {
        usize::try_from(self.hits()).is_ok_and(|hits| hits == self.observation_evidence_count())
    }

    /// Number of distinct retained evidence ids, including contradictory rows.
    /// This intentionally does not
    /// expose the legacy aggregate `occurrences`: a row saying "226 hits" with
    /// no per-episode provenance has zero independently auditable evidence.
    #[must_use]
    pub fn knowledge_evidence_count(&self) -> usize {
        self.efficacy.as_ref().map_or(0, |efficacy| {
            efficacy
                .recent_observations
                .iter()
                .map(|evidence| evidence.episode_id.as_str())
                .filter(|id| !id.trim().is_empty())
                .collect::<std::collections::HashSet<_>>()
                .len()
        })
    }

    fn corroborating_evidence_count(&self) -> usize {
        self.efficacy.as_ref().map_or(0, |efficacy| {
            efficacy
                .recent_observations
                .iter()
                .filter(|evidence| evidence.outcome != KnowledgeEvidenceOutcome::Conflict)
                .map(|evidence| evidence.episode_id.as_str())
                .filter(|id| !id.trim().is_empty())
                .collect::<std::collections::HashSet<_>>()
                .len()
        })
    }

    fn observation_evidence_count(&self) -> usize {
        self.efficacy.as_ref().map_or(0, |efficacy| {
            efficacy
                .recent_observations
                .iter()
                .filter(|evidence| {
                    matches!(
                        evidence.outcome,
                        KnowledgeEvidenceOutcome::Observed | KnowledgeEvidenceOutcome::Conflict
                    )
                })
                .map(|evidence| evidence.episode_id.as_str())
                .filter(|id| !id.trim().is_empty())
                .collect::<std::collections::HashSet<_>>()
                .len()
        })
    }

    /// Trust in `[TRUST_FLOOR, 1]`, normalised so a legacy / never-rated row
    /// reads as [`NEUTRAL_TRUST`]. Only versioned exact-token counters influence
    /// behavior; the old `trust` float is retained solely for audit/migration.
    #[must_use]
    pub fn trust(&self) -> f32 {
        let Some(efficacy) = self
            .efficacy
            .as_ref()
            .filter(|efficacy| efficacy.outcome_attribution_version == 1)
        else {
            return NEUTRAL_TRUST;
        };
        let reward = efficacy.exact_helpful as f32 * TRUST_REWARD;
        let penalty = efficacy.exact_harmful as f32 * TRUST_PENALTY;
        (NEUTRAL_TRUST + reward - penalty).clamp(TRUST_FLOOR, 1.0)
    }

    /// Apply one trust feedback step IN PLACE: `passed` adds [`TRUST_REWARD`],
    /// otherwise subtracts the larger [`TRUST_PENALTY`] (asymmetric). Starts from
    /// the normalised [`Self::trust`] so a legacy `0.0` row starts at neutral, and
    /// clamps to `[TRUST_FLOOR, 1.0]`. Pure mutation — saving the row is the
    /// caller's job.
    fn apply_trust_feedback(&mut self, passed: bool) {
        // Legacy compatibility/audit primitive. Deliberately does not set the
        // exact attribution version, so these values remain behaviorally neutral.
        let base = if self.trust.is_finite() && self.trust > 0.0 {
            self.trust.clamp(TRUST_FLOOR, 1.0)
        } else {
            NEUTRAL_TRUST
        };
        let next = if passed {
            let reinforced_at = utc_now_iso();
            self.efficacy
                .get_or_insert_with(PitfallEfficacy::default)
                .last_reinforced_at = reinforced_at;
            base + TRUST_REWARD
        } else {
            base - TRUST_PENALTY
        };
        self.trust = next.clamp(TRUST_FLOOR, 1.0);
        // Grounded outcome tally ALONGSIDE the smoothed trust EMA above: the
        // discrete helpful/harmful counts give the prune gate a real SAMPLE SIZE
        // (trust alone can't tell "one bad outcome" from "six"). Best-effort,
        // saturating, never fallible — this is the single choke-point every
        // recall-outcome trust update (dev-error reflux, signature reflux,
        // non-pitfall identity reflux) already passes through.
        let eff = self.efficacy.get_or_insert_with(PitfallEfficacy::default);
        if passed {
            eff.helpful = eff.helpful.saturating_add(1);
        } else {
            eff.harmful = eff.harmful.saturating_add(1);
        }
    }

    /// Apply outcome feedback authorised by an exact committed repair token.
    fn apply_exact_trust_feedback(&mut self, passed: bool) {
        let efficacy = self.efficacy.get_or_insert_with(PitfallEfficacy::default);
        if efficacy.outcome_attribution_version != 1 {
            // Quarantine every broad-attribution field on first exact outcome.
            // Raw legacy counters/timestamps remain on disk for audit, but exact
            // behavior starts from a clean neutral lineage.
            efficacy.exact_helpful = 0;
            efficacy.exact_harmful = 0;
            efficacy.exact_last_reinforced_at.clear();
            efficacy.exact_last_fix_failed_at.clear();
            efficacy.exact_last_verified_at.clear();
            efficacy.recurred_after_warning = false;
            efficacy.proven_fix = false;
            efficacy.failed_fixes.clear();
            efficacy.next_strategy.clear();
            efficacy.outcome_attribution_version = 1;
        }
        if passed {
            efficacy.exact_helpful = efficacy.exact_helpful.saturating_add(1);
            efficacy.exact_last_reinforced_at = utc_now_iso();
        } else {
            efficacy.exact_harmful = efficacy.exact_harmful.saturating_add(1);
        }
    }

    /// Total OUTCOME observations recorded for this lesson (`helpful + harmful`) —
    /// the SAMPLE SIZE the efficacy prune gate reads. `0` when never observed.
    #[must_use]
    pub fn efficacy_samples(&self) -> u32 {
        self.efficacy
            .as_ref()
            .filter(|e| e.outcome_attribution_version == 1)
            .map_or(0, |e| e.exact_helpful.saturating_add(e.exact_harmful))
    }

    /// Helpful ratio `helpful / (helpful + harmful)` in `[0, 1]` once at least one
    /// outcome has been observed; `None` while un-sampled so callers treat an
    /// un-observed lesson as NEUTRAL — never poison, never re-ranked — keeping a
    /// fresh corpus's behaviour byte-for-byte unchanged.
    #[must_use]
    pub fn helpful_ratio(&self) -> Option<f64> {
        let e = self.efficacy.as_ref()?;
        if e.outcome_attribution_version != 1 {
            return None;
        }
        let total = e.exact_helpful.saturating_add(e.exact_harmful);
        if total == 0 {
            return None;
        }
        Some(f64::from(e.exact_helpful) / f64::from(total))
    }

    /// `true` when this is a precisely-recognised pitfall (a classified error
    /// family, not the `general/error/...` generic fallback). Recognised
    /// pitfalls are higher-trust for triggering and global promotion.
    #[must_use]
    pub fn is_recognized(&self) -> bool {
        !self.signature.is_empty() && !self.signature.starts_with("general/")
    }

    /// Whether a pitfall is precise enough to influence future work. Generic
    /// fallbacks stay in the raw JSONL ledger for audit, but cannot be recalled,
    /// reported as advice, sedimented, or promoted.
    #[must_use]
    pub fn is_actionable_pitfall(&self) -> bool {
        self.kind != LessonKind::DevError || self.is_recognized()
    }

    /// A precise, non-invalidated incident that may influence product behavior.
    #[must_use]
    pub fn is_live_actionable_pitfall(&self) -> bool {
        self.kind == LessonKind::DevError && self.is_recognized() && !self.invalidated
    }

    /// Derive the strict knowledge lifecycle from auditable evidence.
    /// Similarity and aggregate hit counts never enter this decision.
    #[must_use]
    pub fn pitfall_status(&self) -> PitfallStatus {
        if self.invalidated {
            return PitfallStatus::Invalidated;
        }
        let Some(efficacy) = self.efficacy.as_ref() else {
            return PitfallStatus::Hypothesis;
        };
        if efficacy
            .recent_observations
            .iter()
            .any(|evidence| evidence.outcome == KnowledgeEvidenceOutcome::Conflict)
        {
            return PitfallStatus::Invalidated;
        }
        let latest_causal = efficacy
            .recent_observations
            .iter()
            .filter(|evidence| {
                matches!(
                    evidence.outcome,
                    KnowledgeEvidenceOutcome::FixSucceeded | KnowledgeEvidenceOutcome::FixFailed
                ) && evidence_has_complete_causal_provenance(evidence)
            })
            .max_by(|left, right| left.observed_at.cmp(&right.observed_at));
        match latest_causal.map(|evidence| evidence.outcome) {
            Some(KnowledgeEvidenceOutcome::FixSucceeded) => PitfallStatus::Validated,
            Some(KnowledgeEvidenceOutcome::FixFailed) => PitfallStatus::Invalidated,
            _ if self.corroborating_evidence_count() >= 2 => PitfallStatus::Corroborated,
            _ => PitfallStatus::Hypothesis,
        }
    }
}

fn evidence_has_complete_causal_provenance(evidence: &PitfallObservation) -> bool {
    evidence.outcome != KnowledgeEvidenceOutcome::Observed
        && !evidence.episode_id.trim().is_empty()
        && !evidence.evidence_hash.trim().is_empty()
        && !evidence.source.trim().is_empty()
        && !evidence.base.trim().is_empty()
        && !evidence.base_version.trim().is_empty()
        && evidence.base_version != "unknown"
        && evidence.workspace_scope.starts_with("project:")
        && !evidence.causal_attempt_id.trim().is_empty()
        && chrono::DateTime::parse_from_rfc3339(&evidence.observed_at).is_ok()
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
    if !project_capture_enabled(project_root, MemoryStore::QualityFailures) {
        return;
    }
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let capture_id = next_capture_episode_id(&utc_now_iso());
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
            efficacy: Some(observed_lesson_efficacy(
                project_root,
                &format!("{capture_id}:{}", check.name),
                &format!("{}\0{}\0{}", check.name, check.status, check.details),
                "quality-gate-failure",
                &now,
            )),
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        });
    }
    let committed = append_raw_lessons(project_root, "quality-failures.jsonl", &lessons);
    // Record-time contradiction control: demote the lower-standing side of any
    // genuine conflict this new lesson introduces (fail-open, no-op when empty).
    if committed && !lessons.is_empty() {
        let _ = resolve_new_lesson_conflicts(project_root, &lessons);
        let _ = fold_beliefs(project_root);
    }
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
    let revision_text = umadev_governance::redaction::redact_text(revision_text);
    let requirement = umadev_governance::redaction::redact_text(requirement);
    let ts = now.format("%Y%m%dT%H%M%S%.3fZ");
    let date = now.format("%Y-%m-%d");
    let safe_gate = safe_domain_segment(gate).unwrap_or("gate");
    let sequence = ADR_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let filename = format!("{safe_gate}-{ts}-{}-{sequence}.md", std::process::id());
    let mut adr_path = project_root.join(DECISIONS_DIR).join(&filename);

    // The proof ADR and the reusable lesson are distinct leaf stores. A user may
    // retain one without authorising the other; neither toggle changes the gate's
    // actual revision flow.
    if project_capture_enabled(project_root, MemoryStore::GateAdrs) {
        if let Ok(_guard) = umadev_state::store_lock::acquire(project_root, MemoryStore::GateAdrs) {
            if let Some(dec_dir) = ensure_managed_project_dir(project_root, "decisions") {
                adr_path = dec_dir.join(&filename);
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
                let _ = write_atomic(&adr_path, &adr_body);
            }
        }
    }

    if !project_capture_enabled(project_root, MemoryStore::GateRevisions) {
        return adr_path;
    }

    // 2. Append a raw Revision lesson.
    let domain = if gate.contains("docs") {
        "docs"
    } else {
        "frontend"
    };
    let keywords = extract_keywords(gate, &revision_text, &requirement);
    let lesson = Lesson {
        kind: LessonKind::Revision,
        domain: domain.to_string(),
        title: format!("{gate} revision: {}", truncate(&revision_text, 80)),
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
        source_requirement: requirement,
        first_seen: now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        signature: String::new(),
        occurrences: 1,
        context: Vec::new(),
        efficacy: Some(observed_lesson_efficacy(
            project_root,
            &format!("gate-revision:{gate}:{ts}"),
            &format!("{gate}\0{revision_text}"),
            "human-gate-revision",
            &now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )),
        invalidated: false,
        trust: NEUTRAL_TRUST,
        evidence_count: 0,
        evidence: Vec::new(),
    };
    let lessons = [lesson];
    if append_raw_lessons(project_root, "gate-revisions.jsonl", &lessons) {
        // Record-time contradiction control (fail-open).
        let _ = resolve_new_lesson_conflicts(project_root, &lessons);
        let _ = fold_beliefs(project_root);
    }

    adr_path
}

/// Capture validated patterns (API entity decompositions with REAL
/// implementation evidence) as positive cross-run experience. Called at
/// delivery completion.
///
/// Two correctness guards close a memory-poisoning gap (the old version
/// sedimented EVERY parsed endpoint as "validated / passed the quality gate"
/// regardless of whether a single line was implemented or whether the gate
/// actually passed):
///
/// - `unimplemented_paths` is the set of planned endpoint paths with NO
///   implementation evidence in the workspace (the acceptance-gap list). Those
///   are subtracted, so only endpoints actually built are sedimented. If
///   nothing implemented remains, NOTHING is written (a plan with zero
///   delivered endpoints carries no reusable, proven pattern).
/// - `quality_passed` gates the wording: the lesson only asserts the gate
///   passed when it actually did.
///
/// Fail-open: a write error never blocks delivery.
pub fn capture_validated_patterns(
    project_root: &Path,
    slug: &str,
    requirement: &str,
    spec: &ApiSpec,
    unimplemented_paths: &[String],
    quality_passed: bool,
) {
    if !project_capture_enabled(project_root, MemoryStore::ValidatedPatterns) {
        return;
    }
    // "Validated" is an evidence claim, not a category label. Source presence
    // alone is insufficient: only a mechanically passing quality gate may mint
    // a reusable validated pattern.
    if spec.is_empty() || !quality_passed {
        return;
    }
    // Keep only endpoints whose path is NOT in the unimplemented-gap set — i.e.
    // the ones with real implementation evidence in the delivered source. A path
    // counts as a gap iff it appears as a whitespace-bounded token in any gap
    // line, so the gap-string format does not matter.
    let is_gap = |path: &str| {
        unimplemented_paths
            .iter()
            .any(|g| g.split_whitespace().any(|tok| tok == path))
    };
    let implemented: Vec<String> = spec
        .declared_paths()
        .iter()
        .map(|(_, p)| (*p).to_string())
        .filter(|p| !is_gap(p))
        .collect();
    if implemented.is_empty() {
        // Every planned endpoint is unimplemented → no proven pattern to keep.
        return;
    }
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let capture_id = next_capture_episode_id(&utc_now_iso());
    let entity_summary = implemented.join(", ");
    let keywords = extract_keywords(slug, &entity_summary, requirement);
    // Wording is evidence-accurate: "implemented (source-verified)" always; the
    // gate-passed claim is added ONLY when the gate actually passed.
    let gate_line = "These endpoints were implemented (verified against the delivered source) \
                     and the run passed the quality gate.";
    let lesson = Lesson {
        kind: LessonKind::ValidatedPattern,
        domain: "api".to_string(),
        title: format!("Implemented API contract for {slug}"),
        body: format!(
            "The {slug} run produced an OpenAPI contract with these IMPLEMENTED \
             endpoints:\n{entity_summary}\n\n\
             {gate_line} Reuse this entity decomposition for similar \
             requirements.\n\nRequirement: {requirement}",
        ),
        fix: "Reuse this proven entity decomposition for similar projects.".to_string(),
        root_cause: "This contract was generated from the requirement and the endpoints were \
                     implemented in the delivered source."
            .to_string(),
        keywords,
        source_requirement: requirement.to_string(),
        first_seen: now.clone(),
        signature: String::new(),
        occurrences: 1,
        context: Vec::new(),
        efficacy: Some(observed_lesson_efficacy(
            project_root,
            &format!("validated-pattern:{capture_id}"),
            &entity_summary,
            "source-verified-quality-pass",
            &now,
        )),
        invalidated: false,
        trust: NEUTRAL_TRUST,
        evidence_count: 0,
        evidence: Vec::new(),
    };
    let lessons = [lesson];
    if append_raw_lessons(project_root, "validated-decisions.jsonl", &lessons) {
        // Record-time contradiction control (fail-open).
        let _ = resolve_new_lesson_conflicts(project_root, &lessons);
        let _ = fold_beliefs(project_root);
    }
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
/// the internal severity threshold becomes one [`LessonKind::Failure`] lesson
/// (deduped by kind so a doc with 40 `Lorem ipsum` lines yields ONE lesson, not
/// 40), keyed under the `governance` domain. Returns how many lessons were
/// written. Fail-open: a write error never blocks the quality gate.
pub fn capture_tech_debt(
    project_root: &Path,
    items: &[crate::tech_debt::DebtItem],
    requirement: &str,
) -> usize {
    if !project_capture_enabled(project_root, MemoryStore::TechDebt) {
        return 0;
    }
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
    let capture_id = next_capture_episode_id(&utc_now_iso());
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
            efficacy: Some(observed_lesson_efficacy(
                project_root,
                &format!("tech-debt:{capture_id}:{kind_name}"),
                &format!("{kind_name}\0{count}\0{sample}"),
                "governance-tech-debt",
                &now,
            )),
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        });
    }
    let written = if append_raw_lessons(project_root, "tech-debt.jsonl", &lessons) {
        lessons.len()
    } else {
        0
    };
    // Record-time contradiction control (fail-open, no-op when empty).
    if written > 0 {
        let _ = resolve_new_lesson_conflicts(project_root, &lessons);
        let _ = fold_beliefs(project_root);
    }
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
/// Compatibility wrapper returning only the number of newly-created incident
/// signatures. Callers that surface learning progress should use
/// [`capture_dev_errors_detailed`] so a recurrence (especially the second hit
/// that creates a reusable rule) is not silent.
pub fn capture_dev_errors(
    project_root: &Path,
    raw_errors: &[String],
    slug: &str,
    requirement: &str,
) -> usize {
    capture_dev_errors_detailed(project_root, raw_errors, slug, requirement).new_incidents
}

/// Structured result of one capture episode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PitfallCaptureOutcome {
    /// Precise signatures first admitted to the actionable ledger.
    pub new_incidents: usize,
    /// Existing precise signatures observed again in this episode.
    pub recurrent_incidents: usize,
    /// Incidents that crossed from a one-off into a reusable curated rule.
    pub newly_curated_rules: usize,
    /// New privacy-safe unknown fingerprints retained for classification audit.
    pub new_unclassified_candidates: usize,
    /// Existing unknown fingerprints observed in another independent episode.
    pub recurrent_unclassified_candidates: usize,
    /// Distinct signature observations committed after within-episode dedupe.
    pub observations: usize,
}

impl PitfallCaptureOutcome {
    /// Merge another independently-bounded event outcome into this turn summary.
    pub fn absorb(&mut self, other: Self) {
        self.new_incidents = self.new_incidents.saturating_add(other.new_incidents);
        self.recurrent_incidents = self
            .recurrent_incidents
            .saturating_add(other.recurrent_incidents);
        self.newly_curated_rules = self
            .newly_curated_rules
            .saturating_add(other.newly_curated_rules);
        self.new_unclassified_candidates = self
            .new_unclassified_candidates
            .saturating_add(other.new_unclassified_candidates);
        self.recurrent_unclassified_candidates = self
            .recurrent_unclassified_candidates
            .saturating_add(other.recurrent_unclassified_candidates);
        self.observations = self.observations.saturating_add(other.observations);
    }

    /// User-visible progress notes. Recurrences are intentionally visible even
    /// when no new signature was created; the second episode announces the new
    /// pending rule instead of making self-evolution look inert.
    #[must_use]
    pub fn progress_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.new_incidents > 0 {
            notes.push(umadev_i18n::tlf(
                "lessons.progress.new_incidents",
                &[&self.new_incidents.to_string()],
            ));
        }
        if self.recurrent_incidents > 0 {
            notes.push(umadev_i18n::tlf(
                "lessons.progress.recurrent_incidents",
                &[&self.recurrent_incidents.to_string()],
            ));
        }
        if self.newly_curated_rules > 0 {
            notes.push(umadev_i18n::tlf(
                "lessons.progress.new_rules",
                &[&self.newly_curated_rules.to_string()],
            ));
        }
        if self.new_unclassified_candidates > 0 {
            notes.push(umadev_i18n::tlf(
                "lessons.progress.new_candidates",
                &[&self.new_unclassified_candidates.to_string()],
            ));
        }
        if self.recurrent_unclassified_candidates > 0 {
            notes.push(umadev_i18n::tlf(
                "lessons.progress.recurrent_candidates",
                &[&self.recurrent_unclassified_candidates.to_string()],
            ));
        }
        notes
    }
}

/// Capture one failure event/episode and return both new and recurrent progress.
/// Multiple copies of the same signature inside `raw_errors` count once; callers
/// should invoke this once per independently executed failed tool/check.
/// Fail-open: any I/O error is swallowed and the pipeline continues.
pub fn capture_dev_errors_detailed(
    project_root: &Path,
    raw_errors: &[String],
    slug: &str,
    requirement: &str,
) -> PitfallCaptureOutcome {
    let now = utc_now_iso();
    let evidence_id = next_capture_episode_id(&now);
    capture_dev_errors_detailed_with_evidence_id(
        project_root,
        raw_errors,
        slug,
        requirement,
        &evidence_id,
    )
}

/// Capture one explicitly identified failure episode. Replaying the same
/// `evidence_id` is idempotent across threads/processes; it cannot inflate
/// occurrences or move a rule from hypothesis to corroborated.
pub fn capture_dev_errors_detailed_with_evidence_id(
    project_root: &Path,
    raw_errors: &[String],
    _slug: &str,
    requirement: &str,
    evidence_id: &str,
) -> PitfallCaptureOutcome {
    if evidence_id.trim().is_empty() {
        return PitfallCaptureOutcome::default();
    }
    if !project_capture_enabled(project_root, MemoryStore::Pitfalls) {
        return PitfallCaptureOutcome::default();
    }
    // Serialize the complete RMW across every UmaDev process sharing this
    // project. Failure stays fail-open, but an unlocked stale snapshot is never
    // allowed to replace the authoritative ledger.
    let Some(_kb_guard) = acquire_raw_store_lock(project_root, DEV_ERRORS_FILE) else {
        return PitfallCaptureOutcome::default();
    };
    let now = utc_now_iso();
    let episode_id = privacy_fingerprint("umadev:knowledge-evidence-id:v1", evidence_id.trim());
    // The tech-stack fingerprint present *right now* — stamped onto each
    // pitfall so triggering can later match "same situation", not prose.
    let context = project_context_tokens(project_root);

    // Read-modify-write: a recurrence bumps `occurrences` on the existing
    // record (and merges any newly-seen context) rather than being dropped, so
    // the KB measures how often each pitfall actually bites.
    let mut store: Vec<Lesson> = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    // Key the recurrence index by the NORMALIZED signature. The lookup below
    // normalizes the incoming error's signature, so indexing by the raw stored
    // signature would MISS any pitfall recorded before normalization existed (or
    // whose stored form isn't already normalized) - its `occurrences` would freeze
    // forever (the reported "已踩 17 次" that never grows). Normalizing both sides
    // collapses old + new to one key. On a collision (an old frozen record plus a
    // newer shadow of the same root cause), point the index at the HIGHER-count
    // record so the increment lands on the canonical one, not a duplicate.
    let mut idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, l) in store.iter().enumerate() {
        let sig = normalize_signature(&l.signature);
        if sig.is_empty() {
            continue;
        }
        match idx.get(&sig) {
            Some(&j) if store[j].hits() >= l.hits() => {}
            _ => {
                idx.insert(sig, i);
            }
        }
    }

    let mut outcome = PitfallCaptureOutcome::default();
    let mut changed = false;
    // One call represents one failure episode/turn. A streaming host can emit
    // the same failed tool result repeatedly (or stderr + a summary of it), so
    // counting each raw line inflated one real failure into dozens of "hits".
    // Collapse by the stable signature before touching persistent counts.
    let mut seen_in_episode = std::collections::HashSet::new();
    for raw in raw_errors {
        let text = raw.trim();
        if text.is_empty() || !crate::error_kb::looks_like_error(text) {
            continue;
        }
        let mut insight = crate::error_kb::classify_error(text);
        let actionable = insight.recognized;
        // Stabilise the dedup key: strip volatile parts (relative-path
        // prefixes, version suffixes, line/col numbers) that leak into the
        // discriminator segment so the SAME root cause collapses to ONE
        // signature instead of drifting per file/version (see
        // [`normalize_signature`]). Without this, `occurrences` would stay
        // stuck at 1 for a recurring pitfall whose offending path or version
        // string differs run-to-run, and the frequency signal would be lost.
        if actionable {
            insight.signature = precise_actionable_signature(&insight.signature, text);
        } else {
            // Unknown errors still need auditable frequency/time evidence, but
            // must never return to the old `general/error/failed` mega-bucket.
            // Cluster only by an opaque hash of a volatility-reduced form; no
            // raw text or candidate normalization is persisted or injected.
            insight.signature = unclassified_candidate_signature(text);
            let fingerprint = insight.signature.rsplit('/').next().unwrap_or("unknown");
            insight.category = "general".to_string();
            insight.title = format!("待分类错误候选 {}", &fingerprint[..12]);
            insight.fix.clear();
            insight.root_cause.clear();
            insight.keywords.clear();
        }
        if insight.signature.is_empty() || !seen_in_episode.insert(insight.signature.clone()) {
            continue;
        }
        let observation = PitfallObservation {
            observed_at: now.clone(),
            episode_id: episode_id.clone(),
            evidence_hash: privacy_fingerprint("umadev:pitfall-evidence:v1", text),
            source: "host-tool-failure".to_string(),
            base: "umadev-host-adapter".to_string(),
            base_version: env!("CARGO_PKG_VERSION").to_string(),
            workspace_scope: project_workspace_scope(project_root),
            outcome: KnowledgeEvidenceOutcome::Observed,
            causal_attempt_id: String::new(),
        };
        // If this exact privacy fingerprint was previously unknown but a newer
        // classifier now recognises it, migrate its count/timeline into the
        // precise row. The candidate remains invalidated in raw storage as a
        // safe promotion audit record; it never becomes advice itself.
        let migrated_candidate = actionable
            .then(|| unclassified_candidate_signature(text))
            .and_then(|candidate_signature| {
                store
                    .iter()
                    .position(|lesson| {
                        !lesson.invalidated && lesson.signature == candidate_signature
                    })
                    .map(|candidate_index| {
                        let candidate = &mut store[candidate_index];
                        let migrated = (
                            candidate.hits(),
                            candidate.first_seen.clone(),
                            candidate
                                .efficacy
                                .as_ref()
                                .map(|efficacy| efficacy.recent_observations.clone())
                                .unwrap_or_default(),
                        );
                        candidate.invalidated = true;
                        candidate.body = format!(
                            "Privacy-safe candidate promoted to precise signature `{}`; \
                             retained only for audit.",
                            insight.signature
                        );
                        migrated
                    })
            });
        if let Some(&i) = idx.get(&insight.signature) {
            let was_curated = actionable
                && !store[i].invalidated
                && store[i].pitfall_status() != PitfallStatus::Hypothesis;
            if store[i].invalidated {
                // A fresh independent episode is new evidence that re-activates
                // an invalidated precise pitfall. Preserve only its auditable
                // observation tail; stale lifecycle/trust judgments do not
                // silently govern the revived incident.
                let observations = store[i]
                    .efficacy
                    .as_ref()
                    .map(|e| e.recent_observations.clone())
                    .unwrap_or_default();
                store[i].invalidated = false;
                store[i].trust = NEUTRAL_TRUST;
                store[i].efficacy = Some(PitfallEfficacy {
                    recent_observations: observations,
                    ..PitfallEfficacy::default()
                });
            }
            if let Some((candidate_hits, candidate_first, candidate_observations)) =
                &migrated_candidate
            {
                store[i].occurrences = store[i].hits().saturating_add(*candidate_hits);
                if store[i].first_seen.is_empty()
                    || (!candidate_first.is_empty() && candidate_first < &store[i].first_seen)
                {
                    store[i].first_seen.clone_from(candidate_first);
                }
                let efficacy = store[i]
                    .efficacy
                    .get_or_insert_with(PitfallEfficacy::default);
                for prior in candidate_observations {
                    let _ = remember_observation(efficacy, prior.clone());
                }
            }
            let evidence_result = {
                let efficacy = store[i]
                    .efficacy
                    .get_or_insert_with(PitfallEfficacy::default);
                remember_observation(efficacy, observation)
            };
            match evidence_result {
                RememberEvidence::Duplicate => continue,
                RememberEvidence::Conflict => {
                    changed = true;
                    continue;
                }
                RememberEvidence::Inserted => {}
            }
            // Recurrence → frequency++ and absorb any new context tokens.
            store[i].occurrences = store[i].hits().saturating_add(1);
            merge_tokens(&mut store[i].context, &context, 24);
            // Frequency/timeline always advances for the independent episode.
            let eff = store[i]
                .efficacy
                .get_or_insert_with(PitfallEfficacy::default);
            eff.last_recurred_at.clone_from(&now);
            // Do not infer a repair outcome from recurrence alone. Even an exact
            // signature may be emitted before/after an interrupted repair turn;
            // only settle_pitfall_fix_attempt(token, ...) may change the fix
            // lifecycle or failed-fix ledger.
            if actionable {
                outcome.recurrent_incidents = outcome.recurrent_incidents.saturating_add(1);
            } else {
                outcome.recurrent_unclassified_candidates =
                    outcome.recurrent_unclassified_candidates.saturating_add(1);
            }
            outcome.observations = outcome.observations.saturating_add(1);
            if actionable
                && !was_curated
                && store[i].pitfall_status() == PitfallStatus::Corroborated
            {
                outcome.newly_curated_rules = outcome.newly_curated_rules.saturating_add(1);
            }
            changed = true;
            continue;
        }
        // Persist only classifier-derived, structural terms. Raw stderr and the
        // business requirement may contain tokens, absolute paths, signed URLs,
        // or private project names; the evidence hash supplies audit identity
        // without turning the cross-project KB into a data-exfiltration surface.
        let keywords = if actionable {
            safe_dev_error_keywords(&insight)
        } else {
            vec!["unclassified".to_string(), "candidate".to_string()]
        };
        idx.insert(insight.signature.clone(), store.len());
        let (candidate_hits, candidate_first, candidate_observations) =
            migrated_candidate.unwrap_or_else(|| (0, now.clone(), Vec::new()));
        let mut initial_efficacy = PitfallEfficacy::default();
        for prior in candidate_observations {
            let _ = remember_observation(&mut initial_efficacy, prior);
        }
        let _ = remember_observation(&mut initial_efficacy, observation);
        let first_seen = if candidate_hits > 0 && !candidate_first.is_empty() {
            candidate_first
        } else {
            now.clone()
        };
        let initial_hits = 1u32.saturating_add(candidate_hits);
        if initial_hits >= 2 {
            initial_efficacy.last_recurred_at.clone_from(&now);
        }
        store.push(Lesson {
            kind: LessonKind::DevError,
            domain: insight.category.clone(),
            // Title carries the signature so sediment dedups recurrences by
            // (domain, title) too — belt and suspenders with the seen-set.
            title: format!("踩坑 [{}]: {}", insight.signature, insight.title),
            body: if actionable {
                format!(
                    "A recognised development-error episode matched signature `{}`. \
                     Raw stderr and requirement text are intentionally excluded; \
                     inspect the bounded evidence hash in the raw timeline when auditing.",
                    insight.signature
                )
            } else {
                "An unclassified error candidate was observed. Raw evidence and requirement \
                 text are intentionally excluded; this record is quarantine-only until a \
                 precise classifier can identify a root cause."
                    .to_string()
            },
            fix: insight.fix.clone(),
            root_cause: insight.root_cause.clone(),
            keywords,
            source_requirement: format!(
                "requirement-{}",
                privacy_fingerprint("umadev:pitfall-requirement:v1", requirement)
            ),
            first_seen,
            signature: insight.signature,
            occurrences: initial_hits,
            context: if actionable {
                context.clone()
            } else {
                Vec::new()
            },
            efficacy: Some(initial_efficacy),
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        });
        if actionable {
            outcome.new_incidents = outcome.new_incidents.saturating_add(1);
            if store
                .last()
                .is_some_and(|lesson| lesson.pitfall_status() == PitfallStatus::Corroborated)
            {
                outcome.newly_curated_rules = outcome.newly_curated_rules.saturating_add(1);
            }
        } else {
            outcome.new_unclassified_candidates =
                outcome.new_unclassified_candidates.saturating_add(1);
        }
        outcome.observations = outcome.observations.saturating_add(1);
        changed = true;
    }

    if changed {
        prune_pitfalls(&mut store);
        if !write_raw_lessons_unlocked(project_root, DEV_ERRORS_FILE, &store) {
            // Fail-open means development continues, not that we claim a write
            // succeeded. Returning an empty outcome suppresses every [learned]
            // note when mkdir/temp-write/rename failed (notably a locked target
            // on Windows); the next real episode can retry honestly.
            outcome = PitfallCaptureOutcome::default();
        }
    }
    outcome
}

fn safe_dev_error_keywords(insight: &crate::error_kb::ErrorInsight) -> Vec<String> {
    let mut out = Vec::new();
    for token in insight
        .signature
        .split(|c: char| !c.is_ascii_alphanumeric())
        .chain(std::iter::once(insight.category.as_str()))
    {
        let token = token.trim().to_ascii_lowercase();
        if token.len() >= 3 && !out.contains(&token) {
            out.push(token);
        }
    }
    out.truncate(16);
    out
}

fn privacy_fingerprint(domain: &str, value: &str) -> String {
    static EPHEMERAL_KEY: std::sync::OnceLock<Option<[u8; 32]>> = std::sync::OnceLock::new();
    let key = persistent_privacy_key()
        .or_else(|| *EPHEMERAL_KEY.get_or_init(umadev_state::privacy::ephemeral_key));
    let Some(key) = key else {
        // Never fall back to an unkeyed digest. An OS RNG failure loses correlation rather
        // than exposing low-entropy requirements/evidence to offline enumeration.
        return "privacy-key-unavailable".to_string();
    };
    umadev_governance::privacy_fingerprint(&key, domain.as_bytes(), value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(not(test))]
fn persistent_privacy_key() -> Option<[u8; 32]> {
    umadev_state::privacy::installation_key()
}

#[cfg(test)]
fn persistent_privacy_key() -> Option<[u8; 32]> {
    // Respect the existing thread-local TempHome harness. Tests without that harness use the
    // process-local ephemeral fallback above and therefore never touch the developer's home.
    TEST_HOME_OVERRIDE
        .with(|slot| slot.borrow().clone())
        .and_then(|home| umadev_state::privacy::installation_key_in(&home))
}

fn project_workspace_scope(project_root: &Path) -> String {
    let canonical = fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    format!(
        "project:{}",
        privacy_fingerprint(
            "umadev:knowledge-workspace-scope:v1",
            &canonical.to_string_lossy()
        )
    )
}

fn observed_lesson_efficacy(
    project_root: &Path,
    evidence_id: &str,
    evidence_material: &str,
    source: &str,
    observed_at: &str,
) -> PitfallEfficacy {
    let mut efficacy = PitfallEfficacy::default();
    let _ = remember_observation(
        &mut efficacy,
        PitfallObservation {
            observed_at: observed_at.to_string(),
            episode_id: privacy_fingerprint("umadev:lesson-evidence-id:v1", evidence_id),
            evidence_hash: privacy_fingerprint("umadev:lesson-evidence:v1", evidence_material),
            source: source.to_string(),
            base: "umadev".to_string(),
            base_version: env!("CARGO_PKG_VERSION").to_string(),
            workspace_scope: project_workspace_scope(project_root),
            outcome: KnowledgeEvidenceOutcome::Observed,
            causal_attempt_id: String::new(),
        },
    );
    efficacy
}

fn normalize_unclassified_evidence(value: &str) -> String {
    let mut normalized = String::new();
    for token in value.split_whitespace().take(160) {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        if token.contains(['/', '\\']) {
            // Do not collapse every endpoint/module/path into one mega
            // candidate. Hash the volatility-reduced token so `/foo` and
            // `/bar` stay distinct without persisting either raw value; numeric
            // ids/ports/line numbers still collapse across otherwise-identical
            // events.
            let reduced = normalize_volatile_digits(token, 96);
            let path_hash = privacy_fingerprint("umadev:candidate-path-token:v1", &reduced);
            normalized.push_str("<path:");
            normalized.push_str(&path_hash[..8]);
            normalized.push('>');
            continue;
        }
        normalized.push_str(&normalize_volatile_digits(token, 96));
    }
    normalized
}

fn normalize_volatile_digits(value: &str, max_chars: usize) -> String {
    let mut normalized = String::new();
    let mut previous_was_digit = false;
    for ch in value.chars().take(max_chars) {
        if ch.is_ascii_digit() {
            if !previous_was_digit {
                normalized.push('#');
            }
            previous_was_digit = true;
        } else {
            normalized.extend(ch.to_lowercase());
            previous_was_digit = false;
        }
    }
    normalized
}

fn unclassified_candidate_signature(value: &str) -> String {
    let fingerprint = privacy_fingerprint(
        "umadev:unclassified-error-candidate:v1",
        &normalize_unclassified_evidence(value),
    );
    format!("general/candidate/{}", &fingerprint[..20])
}

fn canonical_evidence_shape(value: &str) -> String {
    let mut lines: Vec<String> = value
        .lines()
        .take(64)
        .map(normalize_unclassified_evidence)
        .filter(|line| !line.is_empty())
        .collect();
    lines.sort();
    lines.dedup();
    if lines.is_empty() {
        normalize_unclassified_evidence(value)
    } else {
        lines.join("\n")
    }
}

/// Add a privacy-safe evidence discriminator to classifier families that have
/// no root-cause discriminator of their own. This prevents two unrelated tests,
/// type errors, panics, or syntax errors from accumulating one misleading count
/// or sharing a repair outcome merely because the classifier family is fixed.
fn precise_actionable_signature(classified_signature: &str, evidence: &str) -> String {
    let base = normalize_signature(classified_signature);
    if base.split('/').count() >= 3 {
        return base;
    }
    let shape = canonical_evidence_shape(evidence);
    let fingerprint = privacy_fingerprint("umadev:coarse-error-evidence:v1", &shape);
    let confidence = if shape.split_whitespace().count() >= 4 && shape.chars().count() >= 24 {
        "e"
    } else {
        "u"
    };
    format!("{base}/{confidence}-{}", &fingerprint[..20])
}

/// Number of recent episode records retained per pitfall. The aggregate hit
/// count remains lifetime data; this bounded tail is the inspectable evidence
/// window and prevents logs/secrets from growing without limit.
const MAX_RECENT_OBSERVATIONS: usize = 8;

static CAPTURE_EPISODE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn utc_now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn next_capture_episode_id(now: &str) -> String {
    let seq = CAPTURE_EPISODE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "{}-{}-{seq}",
        now.replace([':', '.', '-'], ""),
        std::process::id()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RememberEvidence {
    Inserted,
    Duplicate,
    Conflict,
}

fn remember_observation(
    efficacy: &mut PitfallEfficacy,
    observation: PitfallObservation,
) -> RememberEvidence {
    // Idempotence when a caller intentionally retries/commits the same evidence
    // id. Reusing that id for different content is a typed contradiction, not a
    // second vote and never a silent last-writer-wins replacement.
    if let Some(old) = efficacy
        .recent_observations
        .iter_mut()
        .find(|old| old.episode_id == observation.episode_id)
    {
        // `observed_at` is assigned by the receiver and can differ when the
        // producer retries after a timeout. Evidence identity and its typed
        // payload, not receipt time, decide idempotence.
        if same_evidence_payload(old, &observation) {
            return RememberEvidence::Duplicate;
        }
        old.outcome = KnowledgeEvidenceOutcome::Conflict;
        old.source = "conflicting-evidence-id".to_string();
        old.causal_attempt_id.clear();
        return RememberEvidence::Conflict;
    }
    efficacy.recent_observations.push(observation);
    if efficacy.recent_observations.len() > MAX_RECENT_OBSERVATIONS {
        let overflow = efficacy.recent_observations.len() - MAX_RECENT_OBSERVATIONS;
        efficacy.recent_observations.drain(0..overflow);
    }
    RememberEvidence::Inserted
}

fn same_evidence_payload(left: &PitfallObservation, right: &PitfallObservation) -> bool {
    left.episode_id == right.episode_id
        && left.evidence_hash == right.evidence_hash
        && left.source == right.source
        && left.base == right.base
        && left.base_version == right.base_version
        && left.workspace_scope == right.workspace_scope
        && left.outcome == right.outcome
        && left.causal_attempt_id == right.causal_attempt_id
}

/// Read the actionable incident ledger as one row per normalised signature.
///
/// Older releases could leave a pre-normalisation row beside a newer shadow row
/// for the same root cause. The raw JSONL remains untouched for audit, but every
/// product-facing read uses this canonical projection so counts, lifecycle, and
/// timestamps are not split across duplicate cards.
fn read_canonical_pitfalls(project_root: &Path) -> Vec<Lesson> {
    let mut canonical = Vec::<Lesson>::new();
    let mut by_signature = std::collections::HashMap::<String, usize>::new();
    for mut lesson in read_raw_lessons(project_root, DEV_ERRORS_FILE)
        .into_iter()
        .filter(|lesson| lesson.kind == LessonKind::DevError && lesson.is_recognized())
    {
        let signature = normalize_signature(&lesson.signature);
        if signature.is_empty() {
            continue;
        }
        lesson.signature.clone_from(&signature);
        if let Some(&index) = by_signature.get(&signature) {
            merge_canonical_pitfall(&mut canonical[index], lesson);
        } else {
            by_signature.insert(signature, canonical.len());
            canonical.push(lesson);
        }
    }
    canonical
}

fn read_unclassified_candidate_lessons(project_root: &Path, min_hits: u32) -> Vec<Lesson> {
    let mut candidates: Vec<Lesson> = read_raw_lessons(project_root, DEV_ERRORS_FILE)
        .into_iter()
        .filter(|lesson| {
            lesson.kind == LessonKind::DevError
                && !lesson.invalidated
                && lesson.signature.starts_with("general/candidate/")
                && lesson.hits() >= min_hits
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.hits()
            .cmp(&a.hits())
            .then_with(|| b.last_observed_at().cmp(a.last_observed_at()))
    });
    candidates
}

fn merge_canonical_pitfall(dst: &mut Lesson, src: Lesson) {
    let dst_hits = dst.hits();
    let src_hits = src.hits();
    dst.occurrences = dst_hits.saturating_add(src_hits);
    dst.invalidated |= src.invalidated;

    if dst.first_seen.is_empty() || (!src.first_seen.is_empty() && src.first_seen < dst.first_seen)
    {
        dst.first_seen.clone_from(&src.first_seen);
    }
    merge_tokens(&mut dst.context, &src.context, 24);
    if src_hits > dst_hits {
        dst.title.clone_from(&src.title);
        dst.body.clone_from(&src.body);
        dst.fix.clone_from(&src.fix);
        dst.root_cause.clone_from(&src.root_cause);
        dst.keywords.clone_from(&src.keywords);
        dst.source_requirement.clone_from(&src.source_requirement);
    }

    let Some(src_eff) = src.efficacy else {
        return;
    };
    let dst_eff = dst.efficacy.get_or_insert_with(PitfallEfficacy::default);
    dst_eff.injected = dst_eff.injected.saturating_add(src_eff.injected);
    dst_eff.occ_at_injection = dst_eff
        .occ_at_injection
        .saturating_add(src_eff.occ_at_injection);
    dst_eff.helpful = dst_eff.helpful.saturating_add(src_eff.helpful);
    dst_eff.harmful = dst_eff.harmful.saturating_add(src_eff.harmful);
    if src_eff.last_recurred_at > dst_eff.last_recurred_at {
        dst_eff.last_recurred_at = src_eff.last_recurred_at;
    }
    if src_eff.last_fix_failed_at > dst_eff.last_fix_failed_at {
        dst_eff.last_fix_failed_at = src_eff.last_fix_failed_at;
    }
    if src_eff.last_verified_at > dst_eff.last_verified_at {
        dst_eff.last_verified_at = src_eff.last_verified_at;
    }
    if src_eff.last_reinforced_at > dst_eff.last_reinforced_at {
        dst_eff.last_reinforced_at = src_eff.last_reinforced_at;
    }

    // Only versioned exact-token outcomes merge into behavior. Legacy broad
    // snapshot/signature counters and lifecycle booleans remain above as raw
    // audit fields, but cannot poison ranking/status after upgrade.
    if src_eff.outcome_attribution_version == 1 {
        if dst_eff.outcome_attribution_version != 1 {
            dst_eff.outcome_attribution_version = 1;
            dst_eff.exact_helpful = 0;
            dst_eff.exact_harmful = 0;
            dst_eff.exact_last_reinforced_at.clear();
            dst_eff.exact_last_fix_failed_at.clear();
            dst_eff.exact_last_verified_at.clear();
            dst_eff.failed_fixes.clear();
            dst_eff.next_strategy.clear();
        }
        dst_eff.exact_helpful = dst_eff.exact_helpful.saturating_add(src_eff.exact_helpful);
        dst_eff.exact_harmful = dst_eff.exact_harmful.saturating_add(src_eff.exact_harmful);
        if src_eff.exact_last_reinforced_at > dst_eff.exact_last_reinforced_at {
            dst_eff.exact_last_reinforced_at = src_eff.exact_last_reinforced_at;
        }
        if src_eff.exact_last_fix_failed_at > dst_eff.exact_last_fix_failed_at {
            dst_eff.exact_last_fix_failed_at = src_eff.exact_last_fix_failed_at;
        }
        if src_eff.exact_last_verified_at > dst_eff.exact_last_verified_at {
            dst_eff.exact_last_verified_at = src_eff.exact_last_verified_at;
        }
        if !src_eff.next_strategy.trim().is_empty() {
            dst_eff.next_strategy = src_eff.next_strategy.clone();
        }
        merge_tokens(&mut dst_eff.failed_fixes, &src_eff.failed_fixes, 8);
    }
    if dst_eff.outcome_attribution_version == 1 {
        let exact_failed = &dst_eff.exact_last_fix_failed_at;
        let exact_passed = &dst_eff.exact_last_verified_at;
        dst_eff.recurred_after_warning =
            !exact_failed.is_empty() && (exact_passed.is_empty() || exact_failed >= exact_passed);
        dst_eff.proven_fix =
            !exact_passed.is_empty() && (exact_failed.is_empty() || exact_passed > exact_failed);
    } else {
        dst_eff.recurred_after_warning = false;
        dst_eff.proven_fix = false;
    }
    merge_tokens(
        &mut dst_eff.pending_fix_attempts,
        &src_eff.pending_fix_attempts,
        4,
    );
    for observation in src_eff.recent_observations {
        remember_observation(dst_eff, observation);
    }
    dst_eff
        .recent_observations
        .sort_by(|a, b| a.observed_at.cmp(&b.observed_at));
}

/// Hard cap on distinct pitfalls kept in `dev-errors.jsonl` so a long-lived
/// commercial repo's KB never bloats. Generous — most projects stay well under.
const MAX_DEV_PITFALLS: usize = 300;
const MAX_UNCLASSIFIED_CANDIDATES: usize = 300;

/// Evict the least-valuable pitfalls when the store exceeds [`MAX_DEV_PITFALLS`].
///
/// Keep priority is tiered by the strict evidence lifecycle first: invalidated
/// repair advice is retained for audit, then independently corroborated risks,
/// then single-observation hypotheses, and finally handled (`Validated`) rows.
/// WITHIN a tier, eviction is by the recency·importance decay score
/// rather than a hard LRU: an old, low-importance lesson
/// is dropped before a recent or frequently-hit one even if their raw timestamps
/// would order them the other way. (Relevance has no query at prune time, so it
/// is the constant floor and drops out of the WITHIN-tier comparison.)
fn prune_pitfalls(store: &mut Vec<Lesson>) {
    let actionable_count = store.iter().filter(|l| l.is_recognized()).count();
    let candidate_count = store
        .iter()
        .filter(|lesson| lesson.signature.starts_with("general/candidate/"))
        .count();
    if actionable_count <= MAX_DEV_PITFALLS && candidate_count <= MAX_UNCLASSIFIED_CANDIDATES {
        return;
    }
    // Legacy generic rows remain immutable audit evidence. New privacy-safe
    // candidates have their own bound so unique noisy stderr cannot turn every
    // full-file RMW into unbounded disk/O(n²) growth.
    let mut actionable = Vec::new();
    let mut candidates = Vec::new();
    let mut legacy_quarantine = Vec::new();
    for lesson in std::mem::take(store) {
        if lesson.is_recognized() {
            actionable.push(lesson);
        } else if lesson.signature.starts_with("general/candidate/") {
            candidates.push(lesson);
        } else {
            legacy_quarantine.push(lesson);
        }
    }
    let now = Utc::now();
    let empty_query = std::collections::HashSet::new();
    let rank = |l: &Lesson| match l.pitfall_status() {
        PitfallStatus::Invalidated => 0u8,
        PitfallStatus::Corroborated => 1,
        PitfallStatus::Hypothesis => 2,
        PitfallStatus::Validated => 3,
    };
    actionable.sort_by(|a, b| {
        rank(a).cmp(&rank(b)).then_with(|| {
            // Higher decay score = keep → sort it earlier (descending).
            let sa = lesson_decay_score(a, &empty_query, now);
            let sb = lesson_decay_score(b, &empty_query, now);
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Final deterministic tiebreak so equal scores prune stably.
                .then_with(|| b.last_observed_at().cmp(a.last_observed_at()))
        })
    });
    actionable.truncate(MAX_DEV_PITFALLS);
    candidates.sort_by(|a, b| {
        b.hits()
            .cmp(&a.hits())
            .then_with(|| b.last_observed_at().cmp(a.last_observed_at()))
    });
    candidates.truncate(MAX_UNCLASSIFIED_CANDIDATES);
    store.extend(legacy_quarantine);
    store.extend(candidates);
    store.extend(actionable);
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
    if let Ok(text) = crate::bounded_fs::read_utf8_beneath(
        project_root,
        &project_root.join("package.json"),
        1024 * 1024,
    ) {
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
    if let Ok(text) = crate::bounded_fs::read_utf8_beneath(
        project_root,
        &project_root.join("Cargo.toml"),
        1024 * 1024,
    ) {
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
/// the caller falls back to the existing template path. Holds
/// the cross-process pitfalls-store lock for the complete read-modify-write so
/// it never races the capture path (or any other dev-errors mutator).
pub fn record_pitfall_strategy(project_root: &Path, signature: &str, strategy: &str) -> bool {
    if !project_capture_enabled(project_root, MemoryStore::PitfallReflections)
        || !project_capture_enabled(project_root, MemoryStore::Pitfalls)
    {
        return false;
    }
    let strategy = umadev_governance::redaction::redact_text(strategy.trim());
    if strategy.is_empty() {
        return false;
    }
    let Some(_kb_guard) = acquire_raw_store_lock(project_root, DEV_ERRORS_FILE) else {
        return false;
    };
    let sig = normalize_signature(signature);
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    if store.is_empty() {
        return false;
    }
    let mut changed = false;
    for l in &mut store {
        if l.is_live_actionable_pitfall() && normalize_signature(&l.signature) == sig {
            let occ = l.hits();
            let eff = l.efficacy.get_or_insert(PitfallEfficacy {
                occ_at_injection: occ,
                ..PitfallEfficacy::default()
            });
            eff.next_strategy = truncate(&strategy, 600);
            changed = true;
        }
    }
    if !changed {
        return false;
    }
    let reflection = store
        .iter()
        .find(|l| l.is_live_actionable_pitfall() && normalize_signature(&l.signature) == sig)
        .map(|lesson| Reflection {
            signature: sig.clone(),
            occurrences: lesson.hits(),
            strategy: truncate(&strategy, 600),
            at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        });
    // Never announce/append a reflection until the authoritative pitfall row
    // committed. A locked Windows target or failed rename is a silent no-op.
    if !write_raw_lessons_unlocked(project_root, DEV_ERRORS_FILE, &store) {
        return false;
    }
    if let Some(reflection) = reflection {
        append_reflection(project_root, &reflection);
    }
    true
}

/// Append a reflection to its per-signature sliding window
/// (`.umadev/reflections/<slug>.jsonl`), keeping only the most recent
/// [`MAX_REFLECTIONS_PER_SIG`]. Fail-open.
fn append_reflection(project_root: &Path, r: &Reflection) -> bool {
    let Some(_guard) =
        (match umadev_state::store_lock::acquire(project_root, MemoryStore::PitfallReflections) {
            Ok(guard) => Some(guard),
            Err(error) => {
                tracing::warn!(
                    store = MemoryStore::PitfallReflections.id(),
                    %error,
                    "pitfall reflection was not committed because its store lock was unavailable"
                );
                None
            }
        })
    else {
        return false;
    };
    let Some(dir) = ensure_managed_project_dir(project_root, "reflections") else {
        return false;
    };
    // Signature → filesystem-safe slug.
    let slug: String = r
        .signature
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = dir.join(format!("{slug}.jsonl"));
    let mut window = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {
            let Ok(bytes) = umadev_state::fs::read_bounded(&path, MAX_REFLECTION_LEDGER_BYTES)
            else {
                tracing::warn!(path = %path.display(), "oversized or unreadable reflection ledger was isolated from RMW");
                return false;
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                tracing::warn!(path = %path.display(), "non-UTF-8 reflection ledger was isolated from RMW");
                return false;
            };
            let mut entries = Vec::new();
            for (index, line) in text.lines().enumerate() {
                if index >= MAX_REFLECTION_LEDGER_LINES {
                    tracing::warn!(path = %path.display(), "reflection ledger exceeded its record limit");
                    return false;
                }
                if line.trim().is_empty() {
                    continue;
                }
                if line.len() > MAX_REFLECTION_LINE_BYTES {
                    tracing::warn!(path = %path.display(), "oversized reflection record was isolated from RMW");
                    return false;
                }
                if let Ok(entry) = serde_json::from_str::<Reflection>(line) {
                    entries.push(entry);
                }
            }
            entries
        }
        _ => {
            tracing::warn!(path = %path.display(), "unsafe reflection ledger was isolated from RMW");
            return false;
        }
    };
    window.push(r.clone());
    let len = window.len();
    if len > MAX_REFLECTIONS_PER_SIG {
        window.drain(0..len - MAX_REFLECTIONS_PER_SIG);
    }
    let mut buf = String::new();
    for entry in &window {
        let Ok(line) = serde_json::to_string(entry) else {
            return false;
        };
        if line.len() > MAX_REFLECTION_LINE_BYTES
            || buf.len().saturating_add(line.len()).saturating_add(1)
                > usize::try_from(MAX_REFLECTION_LEDGER_BYTES).unwrap_or(usize::MAX)
        {
            return false;
        }
        buf.push_str(&line);
        buf.push('\n');
    }
    // Atomic (temp+rename): a crash/kill between a plain truncate-write's truncate and
    // its flush would leave this learned-KB file EMPTY/torn - every recorded pitfall lost.
    write_atomic(&path, &buf).is_ok()
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
/// on a single line instead of growing one line per occurrence. Returns whether
/// the atomic rename committed, so user-facing capture code never announces a
/// lesson that only existed in memory.
#[derive(Default)]
struct RawLessonLedger {
    lessons: Vec<Lesson>,
    opaque_lines: Vec<String>,
}

fn read_raw_lesson_ledger(path: &Path) -> std::io::Result<RawLessonLedger> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RawLessonLedger::default());
        }
        Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "raw lesson ledger is not a regular file",
            ));
        }
        Err(error) => return Err(error),
    }
    let bytes = umadev_state::fs::read_bounded(path, MAX_RAW_LEDGER_BYTES)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut ledger = RawLessonLedger::default();
    let mut records = 0usize;
    for line in text.lines() {
        records = records.saturating_add(1);
        if records > MAX_RAW_LEDGER_LINES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "raw lesson ledger exceeds its record limit",
            ));
        }
        if line.len() > MAX_RAW_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "raw lesson record exceeds its byte limit",
            ));
        }
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Lesson>(line) {
            Ok(lesson) => ledger.lessons.push(lesson),
            Err(_) => ledger.opaque_lines.push(line.to_string()),
        }
    }
    Ok(ledger)
}

fn render_raw_lesson_ledger(
    lessons: &[Lesson],
    opaque_lines: &[String],
) -> std::io::Result<String> {
    if lessons.len().saturating_add(opaque_lines.len()) > MAX_RAW_LEDGER_LINES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "raw lesson ledger exceeds its record limit",
        ));
    }
    let max_total = usize::try_from(MAX_RAW_LEDGER_BYTES).unwrap_or(usize::MAX);
    let mut body = String::new();
    for lesson in lessons {
        let value = serde_json::to_value(lesson)
            .map(umadev_governance::redaction::redact_json)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let line = serde_json::to_string(&value)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if line.len() > MAX_RAW_LINE_BYTES
            || body.len().saturating_add(line.len()).saturating_add(1) > max_total
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "raw lesson output exceeds its byte limit",
            ));
        }
        body.push_str(&line);
        body.push('\n');
    }
    for line in opaque_lines {
        if line.len() > MAX_RAW_LINE_BYTES
            || body.len().saturating_add(line.len()).saturating_add(1) > max_total
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "raw lesson output exceeds its byte limit",
            ));
        }
        body.push_str(line);
        body.push('\n');
    }
    Ok(body)
}

fn write_raw_lessons_unlocked(project_root: &Path, filename: &str, lessons: &[Lesson]) -> bool {
    let Some(raw_dir) = ensure_raw_lessons_dir(project_root) else {
        return false;
    };
    let path = raw_dir.join(filename);
    // A malformed legacy line cannot influence behavior because readers skip
    // it, but silently deleting it during an unrelated read-modify-write would
    // break the append-only audit posture. Carry every unparseable non-empty
    // line forward verbatim; later valid writes neither duplicate nor repair it.
    let Ok(existing) = read_raw_lesson_ledger(&path) else {
        tracing::warn!(path = %path.display(), "raw lesson ledger was not overwritten because bounded validation failed");
        return false;
    };
    let Ok(buf) = render_raw_lesson_ledger(lessons, &existing.opaque_lines) else {
        tracing::warn!(path = %path.display(), "raw lesson ledger output exceeded a safety bound");
        return false;
    };
    // Atomic (temp+rename): a crash/kill between a plain truncate-write's truncate and
    // its flush would leave this learned-KB file EMPTY/torn - every recorded pitfall lost.
    write_atomic(&path, &buf).is_ok()
}

#[cfg(test)]
fn write_raw_lessons(project_root: &Path, filename: &str, lessons: &[Lesson]) -> bool {
    let Some(_guard) = acquire_raw_store_lock(project_root, filename) else {
        return false;
    };
    write_raw_lessons_unlocked(project_root, filename, lessons)
}

/// Atomically and durably replace `path` through the shared no-follow state
/// primitive. Returns the commit result so callers can fail open honestly.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if FORCE_ATOMIC_WRITE_FAILURE.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("forced atomic-write failure"));
    }
    umadev_state::fs::atomic_write(path, body.as_bytes())
}

#[cfg(test)]
thread_local! {
    static FORCE_ATOMIC_WRITE_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn with_forced_atomic_write_failure<T>(f: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FORCE_ATOMIC_WRITE_FAILURE.with(|forced| forced.set(false));
        }
    }
    FORCE_ATOMIC_WRITE_FAILURE.with(|forced| forced.set(true));
    let _reset = Reset;
    f()
}

/// Append lessons to a raw JSONL file. Fail-open (best-effort write).
fn append_raw_lessons(project_root: &Path, filename: &str, lessons: &[Lesson]) -> bool {
    if lessons.is_empty() {
        return true;
    }
    let Some(_guard) = acquire_raw_store_lock(project_root, filename) else {
        return false;
    };
    let Some(raw_dir) = ensure_raw_lessons_dir(project_root) else {
        return false;
    };
    let path = raw_dir.join(filename);
    // Rebuild through the no-follow atomic writer instead of opening in append
    // mode; a hostile final-component link is rejected without touching it.
    let Ok(mut ledger) = read_raw_lesson_ledger(&path) else {
        tracing::warn!(path = %path.display(), "raw lesson append was not committed because bounded validation failed");
        return false;
    };
    ledger.lessons.extend_from_slice(lessons);
    let Ok(body) = render_raw_lesson_ledger(&ledger.lessons, &ledger.opaque_lines) else {
        tracing::warn!(path = %path.display(), "raw lesson append exceeded a safety bound");
        return false;
    };
    write_atomic(&path, &body).is_ok()
}

/// Read all valid raw lessons from a file. Missing files return empty; malformed
/// lines are excluded from behavior but preserved by the internal raw-lesson writer.
#[must_use]
pub fn read_raw_lessons(project_root: &Path, filename: &str) -> Vec<Lesson> {
    let Some(raw_dir) = existing_raw_lessons_dir(project_root) else {
        return Vec::new();
    };
    let path = raw_dir.join(filename);
    match read_raw_lesson_ledger(&path) {
        Ok(ledger) => ledger.lessons,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "raw lesson ledger was isolated from behavior");
            Vec::new()
        }
    }
}

/// Read ALL raw lessons across all files. Deliberately EXCLUDES the derived
/// belief ledger ([`BELIEFS_FILE`]) — beliefs are folded FROM these raw lessons,
/// so the reconcile/sediment paths that call this must not see them as fresh
/// input (that would re-fold a fold). Retrieval uses the internal recall reader
/// instead, which adds beliefs as first-class candidates.
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

/// Read the candidate set for RECALL: every raw lesson PLUS the folded beliefs.
/// This is what [`select_relevant_lessons`] scores, so beliefs compete (and, via
/// the belief-preference in selection, win over) their own raw evidence.
#[must_use]
fn read_lessons_for_recall(project_root: &Path) -> Vec<Lesson> {
    // Prompt recall is leaf-store aware. Reporting/audit readers deliberately use
    // `read_all_raw_lessons` instead, so a recall toggle never hides inventory or
    // provenance from the user.
    let mut all = Vec::new();
    for (file, store) in [
        ("quality-failures.jsonl", MemoryStore::QualityFailures),
        ("gate-revisions.jsonl", MemoryStore::GateRevisions),
        ("validated-decisions.jsonl", MemoryStore::ValidatedPatterns),
        ("tech-debt.jsonl", MemoryStore::TechDebt),
        (DEV_ERRORS_FILE, MemoryStore::Pitfalls),
        (BELIEFS_FILE, MemoryStore::Beliefs),
    ] {
        if project_recall_enabled(project_root, store) {
            all.extend(read_raw_lessons(project_root, file));
        }
    }
    // Preserve generic dev-error rows in raw storage for provenance, but never
    // let low-information fallbacks influence a future prompt.
    all.retain(|l| !l.invalidated && l.is_actionable_pitfall());
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

/// Validate a lesson domain before using it as one filesystem path component.
/// Generated domains are short ASCII taxonomy ids (`dependency`, `api`, ...).
/// A malformed or hand-edited raw row is retained for audit but never allowed
/// to turn `../`, an absolute path, or a newline into a sediment write target.
fn safe_domain_segment(domain: &str) -> Option<&str> {
    (!domain.is_empty()
        && domain.len() <= 64
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then_some(domain)
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
    let dir = ensure_managed_learned_dir(&home_dir()?)?;
    quarantine_unsafe_global_sediments_at(&dir);
    global_learned_tree_is_real(&dir).then_some(dir)
}

fn real_dir_no_follow(path: &Path) -> bool {
    umadev_state::fs::real_dir(path)
}

/// Create one managed directory component without following an existing link.
fn ensure_real_child_dir(parent: &Path, child: &Path) -> bool {
    let Some(name) = child
        .parent()
        .filter(|candidate| *candidate == parent)
        .and_then(|_| child.file_name())
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    umadev_state::fs::ensure_real_child_dir(parent, name).is_ok()
}

fn ensure_managed_project_dir(boundary: &Path, name: &str) -> Option<PathBuf> {
    let boundary = fs::canonicalize(boundary).ok()?;
    if !real_dir_no_follow(&boundary) {
        return None;
    }
    let umadev = umadev_state::fs::ensure_real_child_dir(&boundary, ".umadev").ok()?;
    umadev_state::fs::ensure_real_child_dir(&umadev, name).ok()
}

/// Resolve a user-selected boundary while refusing links in UmaDev-managed
/// components. The boundary itself may legitimately be a symlink (workspace
/// aliases and managed home directories are common), so only `.umadev` and
/// `learned` are subject to the no-follow rule.
fn managed_learned_components(boundary: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let boundary = fs::canonicalize(boundary).ok()?;
    if !real_dir_no_follow(&boundary) {
        return None;
    }
    let umadev_root = boundary.join(".umadev");
    let learned = umadev_root.join("learned");
    Some((boundary, umadev_root, learned))
}

fn ensure_managed_learned_dir(boundary: &Path) -> Option<PathBuf> {
    let (boundary, umadev_root, learned) = managed_learned_components(boundary)?;
    if !ensure_real_child_dir(&boundary, &umadev_root)
        || !ensure_real_child_dir(&umadev_root, &learned)
    {
        return None;
    }
    Some(learned)
}

fn existing_managed_learned_dir(boundary: &Path) -> Option<PathBuf> {
    let (_, umadev_root, learned) = managed_learned_components(boundary)?;
    (real_dir_no_follow(&umadev_root) && real_dir_no_follow(&learned)).then_some(learned)
}

fn ensure_raw_lessons_dir(project_root: &Path) -> Option<PathBuf> {
    let learned = ensure_managed_learned_dir(project_root)?;
    let raw = learned.join("_raw");
    ensure_real_child_dir(&learned, &raw).then_some(raw)
}

fn existing_raw_lessons_dir(project_root: &Path) -> Option<PathBuf> {
    let learned = existing_managed_learned_dir(project_root)?;
    let raw = learned.join("_raw");
    real_dir_no_follow(&raw).then_some(raw)
}

/// Validate the HOME-owned components that quarantine may traverse.
fn global_learned_tree_is_real(root: &Path) -> bool {
    let Some(umadev_root) = root.parent() else {
        return false;
    };
    let Some(home) = umadev_root.parent() else {
        return false;
    };
    real_dir_no_follow(home) && real_dir_no_follow(umadev_root) && real_dir_no_follow(root)
}

#[cfg(test)]
thread_local! {
    static TEST_HOME_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Set the lessons-only, thread-local home used by unit tests.
#[cfg(test)]
pub(crate) fn set_test_home_override(path: Option<PathBuf>) {
    TEST_HOME_OVERRIDE.with(|slot| *slot.borrow_mut() = path);
}

/// Cross-platform home directory: `HOME` then `USERPROFILE` (Windows).
fn home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_HOME_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return Some(path);
    }
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

fn read_reconcilable_lessons_for_recall(project_root: &Path) -> Vec<Lesson> {
    let mut lessons = Vec::new();
    for file in RECONCILE_FILES {
        let Some(store) = raw_lesson_store(file) else {
            continue;
        };
        if project_recall_enabled(project_root, store) {
            lessons.extend(read_raw_lessons(project_root, file));
        }
    }
    lessons
}

// =====================================================================
// Belief layer (①): fold N similar raw lessons into one denser rule.
//
// The raw ledgers accumulate many near-duplicate lessons ("hardcoded color in
// Foo.css", "hardcoded color in Bar.css", …). Retrieving the raw rows floods
// the prompt with low-density repeats. A BELIEF folds a cluster of similar
// lessons into ONE higher-level rule carrying `evidence_count` (how many raw
// lessons back it) + a freshness stamp (`first_seen` = last confirmation).
// Retrieval prefers the belief and demotes its raw evidence, so the worker sees
// the distilled rule instead of the duplicates.
//
// Folding is DETERMINISTIC (a connected-components clustering over a fixed
// keyword/domain similarity threshold) and template-built — NO base call — so
// it runs every sediment with zero added latency and zero network. Fail-open:
// any error leaves the belief ledger untouched and recall transparently falls
// back to the raw lessons.
// =====================================================================

/// Minimum [`lesson_similarity`] for two raw lessons to be clustered into one
/// belief. `3` ≈ "shares at least one keyword AND the same domain, or shares two
/// keywords" — tight enough that only genuinely-about-the-same-thing lessons
/// fold, loose enough to catch the near-duplicates the ledgers accumulate.
const BELIEF_FOLD_THRESHOLD: i64 = 3;

/// Minimum cluster size to MINT a belief. A pair (2) is enough signal that a
/// pattern repeats; a lone lesson stays a lone lesson (no belief).
const BELIEF_MIN_CLUSTER: usize = 2;

/// Hard cap on beliefs kept, mirroring the pitfall cap so the belief ledger
/// can't bloat a long-lived repo. Lowest-evidence, oldest beliefs evict first.
const MAX_BELIEFS: usize = 200;

/// The stable evidence key a belief stores per folded lesson:
/// `"domain\u{0}title\u{0}<advice-hash>"`. `first_seen` is intentionally OMITTED so
/// a true recurrence under a refreshed timestamp still resolves to the same key
/// (the original intent). But `domain\0title` ALONE collided: the belief titles are
/// template-generated, so two genuinely-DIFFERENT lessons from different runs could
/// share `domain\0title` and then a belief covering one would wrongly demote /
/// downgrade the other (P2-7). Folding in a short hash of the ADVICE CONTENT
/// (root_cause + fix) disambiguates different lessons while a real recurrence (same
/// advice) still hashes identically — so the recurrence-matching intent is kept and
/// the collision is closed. Pure + deterministic.
fn evidence_key(l: &Lesson) -> String {
    format!(
        "{}\u{0}{}\u{0}{:016x}",
        l.domain,
        l.title,
        advice_content_hash(l)
    )
}

/// A stable 64-bit hash of a lesson's ADVICE CONTENT (`root_cause` + `fix`) — the
/// disambiguator folded into [`evidence_key`] so two lessons that merely share a
/// template-generated `domain\0title` but carry DIFFERENT advice get different
/// keys, while a true recurrence of the same advice hashes the same. Uses the std
/// `DefaultHasher` (SipHash with FIXED keys → deterministic across processes for a
/// given std version), so a re-fold in a later run still matches a previously-
/// persisted key. Fail-open even if the std algorithm ever changed: a key that no
/// longer matches just mints a fresh belief (which `prune_beliefs` caps), never a
/// panic or a wrong demotion.
fn advice_content_hash(l: &Lesson) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    l.root_cause.trim().hash(&mut h);
    "\u{0}".hash(&mut h);
    l.fix.trim().hash(&mut h);
    h.finish()
}

/// A stable 64-bit hash of an arbitrary string — the filename disambiguator for
/// promoted global lessons (see [`promote_to_global`]). Uses the same fixed-key
/// `DefaultHasher` as [`advice_content_hash`], so the SAME input hashes the same
/// across processes/runs (a re-promotion lands on the same file) while distinct
/// inputs that slugify identically get distinct files. Fail-open by nature:
/// hashing never errors.
fn stable_str_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Fold the current raw lessons into the belief ledger: cluster similar
/// non-pitfall, non-invalidated lessons (deterministic connected components over
/// the internal lesson-similarity threshold) and, for every sufficiently large
/// cluster, ADD a fresh belief or UPDATE the existing one
/// that already covers those lessons.
///
/// Reuses the ADD/UPDATE posture of the reconcile machinery: a cluster whose
/// evidence keys overlap an existing belief UPDATES that belief in place
/// (refreshing `evidence` + `evidence_count` + `first_seen`/`last_confirmed`,
/// and carrying its accumulated `trust` forward); a genuinely new cluster mints
/// a new belief. Returns how many beliefs were written/updated. Pure-local +
/// deterministic + fail-open (a write error leaves the ledger as it was). Called
/// from the sediment path; recall then prefers these beliefs over their raw
/// evidence.
pub fn fold_beliefs(project_root: &Path) -> usize {
    if !project_capture_enabled(project_root, MemoryStore::Beliefs) {
        return 0;
    }
    let Some(_belief_guard) = acquire_raw_store_lock(project_root, BELIEFS_FILE) else {
        return 0;
    };
    let raw = read_all_raw_lessons(project_root);
    // Cluster only the curatable, still-valid, non-pitfall lessons: pitfalls have
    // their own dedup/efficacy loop, and folding them would fight it.
    let pool: Vec<&Lesson> = raw
        .iter()
        .filter(|l| {
            l.kind != LessonKind::DevError && l.kind != LessonKind::Belief && !l.invalidated
        })
        .collect();
    if pool.len() < BELIEF_MIN_CLUSTER {
        return 0;
    }

    let clusters = cluster_lessons(&pool);
    if clusters.is_empty() {
        return 0;
    }

    // Load existing beliefs, indexed by the SET of evidence keys they cover, so a
    // re-fold UPDATEs the matching belief instead of minting a duplicate.
    let mut beliefs = read_raw_lessons(project_root, BELIEFS_FILE);
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut touched = 0usize;

    for cluster in &clusters {
        if cluster.len() < BELIEF_MIN_CLUSTER {
            continue;
        }
        let members: Vec<&Lesson> = cluster.iter().map(|&i| pool[i]).collect();
        let ev_keys: std::collections::BTreeSet<String> =
            members.iter().map(|l| evidence_key(l)).collect();
        // Which existing beliefs already cover ANY of these lessons? A cluster can
        // span MORE THAN ONE prior belief (two earlier folds whose evidence the new,
        // larger cluster now unifies). Pre-fix only the FIRST overlapping belief was
        // UPDATEd and the rest survived as duplicates (P2-7); now we collect ALL of
        // them, fold the cluster into the first, and REMOVE the others so the cluster
        // ends as exactly ONE belief — the union, not a leftover duplicate.
        let matching: Vec<usize> = beliefs
            .iter()
            .enumerate()
            .filter(|(_, b)| b.evidence.iter().any(|k| ev_keys.contains(k)))
            .map(|(i, _)| i)
            .collect();
        let folded = fold_one_cluster(&members, &ev_keys, &now);
        if let Some(&keep) = matching.first() {
            // Carry the STRONGEST accumulated trust across every merged belief (a
            // belief proven across folds shouldn't lose reputation on a merge), then
            // refresh the rule + evidence set + freshness stamp on the kept slot.
            let carried_trust = matching
                .iter()
                .map(|&i| beliefs[i].trust)
                .fold(folded.trust, f32::max);
            let carried_invalidation = matching
                .iter()
                .any(|&i| beliefs[i].pitfall_status() == PitfallStatus::Invalidated);
            beliefs[keep] = folded;
            beliefs[keep].trust = carried_trust;
            beliefs[keep].invalidated = carried_invalidation;
            // Drop every OTHER overlapping belief (high index → low so the earlier
            // removals don't shift the indices we still need to remove).
            for &idx in matching.iter().skip(1).rev() {
                beliefs.remove(idx);
            }
        } else {
            beliefs.push(folded);
        }
        touched += 1;
    }

    if touched == 0 {
        return 0;
    }
    prune_beliefs(&mut beliefs);
    if write_raw_lessons_unlocked(project_root, BELIEFS_FILE, &beliefs) {
        touched
    } else {
        0
    }
}

/// Build ONE belief lesson from a cluster of raw lessons. The belief's body is a
/// deterministic, template-built distillation: the shared domain, the most
/// common keywords, and the representative fix (the longest/richest member fix).
/// `evidence_count` records the cluster size; `evidence` records the per-lesson
/// keys so recall can demote those exact originals. `first_seen` is the most
/// recent member timestamp (= last confirmation / freshness).
fn fold_one_cluster(
    members: &[&Lesson],
    ev_keys: &std::collections::BTreeSet<String>,
    now: &str,
) -> Lesson {
    let domain = members
        .first()
        .map_or_else(|| "general".to_string(), |l| l.domain.clone());
    // Most recent member timestamp = the belief's last-confirmed freshness.
    let last_confirmed = members
        .iter()
        .map(|l| l.first_seen.clone())
        .max()
        .unwrap_or_else(|| now.to_string());
    // Representative fix = the richest (longest) member fix — the most actionable.
    let rep_fix = members
        .iter()
        .map(|l| l.fix.as_str())
        .max_by_key(|f| f.len())
        .unwrap_or("")
        .to_string();
    let rep_root = members
        .iter()
        .map(|l| l.root_cause.as_str())
        .max_by_key(|r| r.len())
        .unwrap_or("")
        .to_string();
    // Union of member keywords, capped, most-shared first.
    let keywords = top_shared_keywords(members, 12);
    // A short, human title summarising the cluster.
    let topic = keywords
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(" / ");
    let title = if topic.is_empty() {
        format!("Belief [{domain}] ({} lessons)", members.len())
    } else {
        format!("Belief: {topic} ({} lessons)", members.len())
    };
    let body = format!(
        "A candidate rule grouped from {n} similar prior lessons in the \
         `{domain}` domain. Similarity is only a clustering signal: consult the \
         typed evidence lifecycle before treating this hypothesis as corroborated \
         or validated.\n\n{rep_root}\n\nKeywords: {kw}",
        n = members.len(),
        kw = keywords.join(", "),
    );
    let source_requirement = members
        .iter()
        .map(|l| l.source_requirement.clone())
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let mut lifecycle = PitfallEfficacy::default();
    for evidence in members.iter().flat_map(|lesson| {
        lesson
            .efficacy
            .as_ref()
            .into_iter()
            .flat_map(|efficacy| efficacy.recent_observations.iter())
    }) {
        let _ = remember_observation(&mut lifecycle, evidence.clone());
    }
    let efficacy = (!lifecycle.recent_observations.is_empty()).then_some(lifecycle);
    Lesson {
        kind: LessonKind::Belief,
        domain,
        title,
        body,
        fix: rep_fix,
        root_cause: rep_root,
        keywords,
        source_requirement,
        first_seen: last_confirmed,
        signature: String::new(),
        occurrences: u32::try_from(members.len()).unwrap_or(u32::MAX),
        context: Vec::new(),
        efficacy,
        invalidated: false,
        trust: NEUTRAL_TRUST,
        evidence_count: u32::try_from(members.len()).unwrap_or(u32::MAX),
        evidence: ev_keys.iter().cloned().collect(),
    }
}

/// The keywords shared across the most cluster members, most-common first,
/// capped at `max`. Deterministic (count desc, then lexical) so a re-fold of the
/// same cluster yields the same belief.
fn top_shared_keywords(members: &[&Lesson], max: usize) -> Vec<String> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for l in members {
        // Count each keyword once per member (dedup within a member first).
        let uniq: std::collections::HashSet<&str> = l.keywords.iter().map(String::as_str).collect();
        for k in uniq {
            *counts.entry(k).or_insert(0) += 1;
        }
    }
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked
        .into_iter()
        .take(max)
        .map(|(k, _)| k.to_string())
        .collect()
}

/// Deterministic connected-components clustering of `pool` by
/// [`lesson_similarity`] ≥ [`BELIEF_FOLD_THRESHOLD`]. Returns clusters as index
/// lists into `pool`, each cluster's indices sorted ascending and the clusters
/// ordered by their smallest index — fully deterministic. O(n²) over the pool
/// with an upper bound so a huge ledger can't blow up (extra lessons are simply
/// not clustered this pass).
fn cluster_lessons(pool: &[&Lesson]) -> Vec<Vec<usize>> {
    let n = pool.len().min(BELIEF_SCAN_LIMIT);
    // Union-find over the first `n` lessons.
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if lesson_similarity(pool[i], pool[j]) >= BELIEF_FOLD_THRESHOLD {
                let ri = find(&mut parent, i);
                let rj = find(&mut parent, j);
                if ri != rj {
                    parent[ri.max(rj)] = ri.min(rj);
                }
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    groups
        .into_values()
        .filter(|g| g.len() >= BELIEF_MIN_CLUSTER)
        .collect()
}

/// Upper bound on lessons scanned per fold/contradiction pass — the O(n²) guard.
/// Generous; a real project's curatable ledger stays well under.
const BELIEF_SCAN_LIMIT: usize = 500;

/// Evict the lowest-value beliefs when the ledger exceeds [`MAX_BELIEFS`]: keep
/// the best by `(evidence_count, recency)` — a belief backed by more evidence
/// and confirmed more recently outranks a thin, stale one. Deterministic.
fn prune_beliefs(beliefs: &mut Vec<Lesson>) {
    if beliefs.len() <= MAX_BELIEFS {
        return;
    }
    beliefs.sort_by(|a, b| {
        b.evidence_count
            .cmp(&a.evidence_count)
            .then_with(|| b.first_seen.cmp(&a.first_seen))
    });
    beliefs.truncate(MAX_BELIEFS);
}

// =====================================================================
// Contradiction / staleness hygiene scan (②).
//
// Two lessons that talk about the SAME thing (high topic/entity overlap) but
// say DIFFERENT things (low text similarity) are a likely contradiction — the
// ledger has accumulated conflicting advice. This pass finds such pairs
// deterministically (O(n²), bounded by [`BELIEF_SCAN_LIMIT`]) and routes them
// through the EXISTING INVALIDATE path: the OLDER of the conflicting pair is
// marked invalid (kept on disk for provenance, dropped from sediment + recall).
// Pure-local; no base call. Fail-open: any error marks nothing.
// =====================================================================

/// Minimum keyword/domain overlap ([`lesson_similarity`]) for two lessons to be
/// "about the same thing" — the FIRST half of the contradiction test.
const CONTRA_TOPIC_OVERLAP: i64 = 4;

/// Maximum body text Jaccard (token-set overlap) for two same-topic lessons to
/// count as CONTRADICTORY — the SECOND half. Below this, two lessons share the
/// topic but phrase their advice so differently they MIGHT disagree. `0.25`: the
/// bodies share at most a quarter of their tokens. NOTE: low overlap alone is NOT
/// enough — two lessons that AGREE but are worded differently also score low here,
/// so a third gate ([`antonym_conflict`]) is required before we INVALIDATE.
const CONTRA_TEXT_SIM_MAX: f64 = 0.25;

/// Minimum advice-token count BOTH lessons must clear before a low-overlap pair is
/// even eligible to be judged a contradiction. Two very short / boilerplate-
/// dominated advices trivially fail the Jaccard gate (few tokens → low overlap)
/// without actually disagreeing, so a pair where EITHER side is this thin is a
/// NOOP — the heuristic abstains rather than risk a false invalidation.
const CONTRA_MIN_ADVICE_TOKENS: usize = 4;

/// Antonym pairs whose CO-OCCURRENCE across two same-topic lessons is the positive
/// signal of a genuine contradiction: one lesson says "add / enable / always /
/// use …", the other says the opposite. Requiring an explicit opposing verb makes
/// the scan demand evidence of DISAGREEMENT, not merely different phrasing — the
/// fix for "two lessons that agree but word it differently get mis-invalidated".
/// Each entry is `(positive, negative)`; the check is symmetric (either lesson may
/// hold either pole). Advice uses lowercased ASCII words plus CJK bigrams, so the
/// fixed pairs cover common English and Chinese engineering-advice verbs.
const CONTRA_ANTONYMS: &[(&str, &str)] = &[
    ("add", "remove"),
    ("add", "drop"),
    ("enable", "disable"),
    ("always", "never"),
    ("use", "avoid"),
    ("prefer", "avoid"),
    ("allow", "deny"),
    ("allow", "block"),
    ("include", "exclude"),
    ("show", "hide"),
    ("increase", "decrease"),
    ("more", "fewer"),
    ("more", "less"),
    ("keep", "delete"),
    ("create", "delete"),
    ("open", "close"),
    ("on", "off"),
    ("required", "optional"),
    ("sync", "async"),
    ("synchronous", "asynchronous"),
    ("添加", "删除"),
    ("启用", "禁用"),
    ("总是", "从不"),
    ("使用", "避免"),
    ("允许", "拒绝"),
    ("允许", "阻止"),
    ("包含", "排除"),
    ("显示", "隐藏"),
    ("增加", "减少"),
    ("保留", "删除"),
    ("创建", "删除"),
    ("开启", "关闭"),
    ("必需", "可选"),
    ("同步", "异步"),
];

/// Whether two lessons' advice carries OPPOSING verbs from [`CONTRA_ANTONYMS`] —
/// the positive contradiction signal. True iff, for some antonym pair, ONE lesson's
/// advice tokens contain the positive pole and the OTHER's contain the negative
/// pole (in either assignment). Pure; tokenised exactly like the Jaccard gate so
/// the two halves see the same token set.
fn antonym_conflict(
    a: &std::collections::HashSet<String>,
    b: &std::collections::HashSet<String>,
) -> bool {
    CONTRA_ANTONYMS.iter().any(|(pos, neg)| {
        let pos = (*pos).to_string();
        let neg = (*neg).to_string();
        (a.contains(&pos) && b.contains(&neg)) || (a.contains(&neg) && b.contains(&pos))
    })
}

/// Efficacy gap below which two contradicting lessons are treated as EQUALLY
/// proven, so the tie falls back to the historical age rule (keep the fresher
/// advice). Two un-sampled lessons both score neutral and tie EXACTLY here, so a
/// fresh corpus's contradiction behaviour is byte-for-byte unchanged; only once
/// real outcome evidence separates them does efficacy override age.
const CONTRA_EFFICACY_EPS: f64 = 1e-3;

/// The recall-standing score that decides which side of a contradiction to KEEP —
/// the SAME two axes recall ranking already trusts: the smoothed [`Lesson::trust`]
/// EMA times the discrete [`efficacy_weight`] (helpful/harmful ratio). Neutral
/// `0.5 · 1.0` for an un-sampled lesson, so two unproven lessons score equal and
/// the loser is decided by age (below), not by noise.
fn contradiction_score(l: &Lesson) -> f64 {
    f64::from(l.trust()) * efficacy_weight(l)
}

/// Pick the LOSER of a genuine contradiction — the lesson to DEMOTE so the higher-
/// standing one keeps the scarce recall slot. Efficacy-aware: the lower
/// [`contradiction_score`] loses when the two are meaningfully apart
/// ([`CONTRA_EFFICACY_EPS`]); on a tie (both un-sampled, or equally proven) it
/// falls back to the historical rule — the OLDER lesson loses, keeping the fresher
/// advice. Deterministic; total (always returns one side).
fn contradiction_loser<'a>(a: &'a Lesson, b: &'a Lesson) -> &'a Lesson {
    let (sa, sb) = (contradiction_score(a), contradiction_score(b));
    if (sa - sb).abs() > CONTRA_EFFICACY_EPS {
        if sa < sb {
            a
        } else {
            b
        }
    } else if a.first_seen <= b.first_seen {
        a
    } else {
        b
    }
}

/// Whether two same-corpus lessons GENUINELY contradict — the shared triple gate
/// both the full-corpus scan and the record-time resolver fold into: high topic
/// overlap ([`CONTRA_TOPIC_OVERLAP`]), BOTH sides carrying enough advice tokens
/// ([`CONTRA_MIN_ADVICE_TOKENS`]), low advice-text overlap ([`CONTRA_TEXT_SIM_MAX`]),
/// AND an explicit [`antonym_conflict`]. The antonym + min-token gates are the
/// false-positive guard: "agree but worded differently" and "both short /
/// boilerplate" pairs fail here and are NEVER judged a contradiction. `ta`/`tb`
/// are the pre-tokenised advice sets ([`advice_tokens`]).
fn genuine_contradiction(
    a: &Lesson,
    ta: &std::collections::HashSet<String>,
    b: &Lesson,
    tb: &std::collections::HashSet<String>,
) -> bool {
    lesson_similarity(a, b) >= CONTRA_TOPIC_OVERLAP
        && ta.len() >= CONTRA_MIN_ADVICE_TOKENS
        && tb.len() >= CONTRA_MIN_ADVICE_TOKENS
        && jaccard_of(ta, tb) <= CONTRA_TEXT_SIM_MAX
        && antonym_conflict(ta, tb)
}

/// Scan the raw ledgers for same-topic lesson pairs that GENUINELY contradict —
/// high topic overlap, low advice-text overlap, AND an explicit antonym conflict
/// (using the internal genuine-contradiction classifier) — and route each hit through
/// the INVALIDATE path, marking the lower-efficacy side (or the
/// OLDER one on a tie) invalid. Returns how many lessons were invalidated. Bounded
/// O(n²); deterministic; pure-local; fail-open.
///
/// The antonym + min-token gates are the false-positive fix: low text overlap
/// alone caught lessons that merely AGREE in different words. Now a pair must also
/// show opposing verbs (add↔remove, always↔never, use↔avoid, …) and both sides
/// must carry enough advice tokens to be judged at all — so "agree but worded
/// differently" and "both short / boilerplate" pairs are NOOPs, not invalidations.
pub fn scan_contradictions(project_root: &Path) -> usize {
    let raw = read_all_raw_lessons(project_root);
    let pool: Vec<&Lesson> = raw
        .iter()
        .filter(|l| {
            l.kind != LessonKind::DevError && l.kind != LessonKind::Belief && !l.invalidated
        })
        .take(BELIEF_SCAN_LIMIT)
        .collect();
    if pool.len() < 2 {
        return 0;
    }

    // Pre-tokenise once per lesson (each pair re-uses both token sets across the
    // Jaccard + antonym gates) so the O(n²) scan does O(n) tokenisation.
    let tokens: Vec<std::collections::HashSet<String>> =
        pool.iter().map(|l| advice_tokens(l)).collect();

    let mut to_invalidate: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    for i in 0..pool.len() {
        for j in (i + 1)..pool.len() {
            let a = pool[i];
            let b = pool[j];
            if !genuine_contradiction(a, &tokens[i], b, &tokens[j]) {
                continue;
            }
            // Demote the LOSER — the lower-efficacy side, or the OLDER one on a tie
            // (keeping the fresher advice), so two opposing lessons never both
            // survive in recall.
            to_invalidate.insert(lesson_identity(contradiction_loser(a, b)));
        }
    }
    if to_invalidate.is_empty() {
        return 0;
    }
    apply_invalidations(project_root, &to_invalidate)
}

/// Jaccard of two pre-tokenised advice sets in `[0,1]` — the SECOND half of the
/// contradiction test. The sets are tokenised the same way the trigger query is
/// (alnum words len ≥ 3 + CJK bigrams) so CJK advice is comparable too.
/// Empty-on-both → `1.0` (identical emptiness is not a contradiction). Pure; the
/// scan pre-tokenises once per lesson and reuses the sets across this gate and the
/// antonym gate.
fn jaccard_of(
    ta: &std::collections::HashSet<String>,
    tb: &std::collections::HashSet<String>,
) -> f64 {
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    let inter = ta.intersection(tb).count();
    let union = ta.union(tb).count().max(1);
    inter as f64 / union as f64
}

/// Token set of a lesson's advice text (fix + root_cause + body), for the
/// contradiction Jaccard. ASCII words ≥ 3 chars + CJK bigrams.
fn advice_tokens(l: &Lesson) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let text = format!("{} {} {}", l.fix, l.root_cause, l.body).to_ascii_lowercase();
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        if w.len() >= 3 && !is_cjk_char(w.chars().next().unwrap_or(' ')) {
            set.insert(w.to_string());
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i + 1 < chars.len() {
        if is_cjk_char(chars[i]) && is_cjk_char(chars[i + 1]) {
            set.insert(chars[i..=i + 1].iter().collect());
        }
        i += 1;
    }
    set
}

/// Mark the given lesson identities invalid across the reconcilable ledgers
/// (the same per-file rewrite the reconcile path uses). Returns how many rows
/// were newly invalidated. Fail-open per file.
fn apply_invalidations(
    project_root: &Path,
    to_invalidate: &std::collections::HashSet<(String, String, String)>,
) -> usize {
    let mut marked = 0usize;
    for file in RECONCILE_FILES {
        if raw_lesson_store(file).is_some_and(|store| !project_capture_enabled(project_root, store))
        {
            continue;
        }
        let Some(_store_guard) = acquire_raw_store_lock(project_root, file) else {
            continue;
        };
        let mut rows = read_raw_lessons(project_root, file);
        if rows.is_empty() {
            continue;
        }
        let mut file_changed = false;
        let mut file_marked = 0usize;
        for row in &mut rows {
            if !row.invalidated && to_invalidate.contains(&lesson_identity(row)) {
                row.invalidated = true;
                file_changed = true;
                file_marked += 1;
            }
        }
        if file_changed && write_raw_lessons_unlocked(project_root, file, &rows) {
            marked = marked.saturating_add(file_marked);
        }
    }
    marked
}

/// Record-time CONTRADICTION CONTROL — when NEW lessons are recorded, demote the
/// lower-standing side of any GENUINE conflict between a new lesson and an existing
/// one so two opposing lessons about the same subject never both poison recall.
///
/// The efficacy-aware sibling of [`scan_contradictions`], scoped to the just-
/// captured lessons (only pairs where at least ONE side is new are judged, so this
/// is targeted and cheap, not a full re-scan). A conflict must clear the SAME
/// triple gate (the internal genuine-contradiction classifier: high topic overlap, low advice overlap,
/// and an explicit antonym), so two lessons that merely share a tech but AGREE are
/// left alone. On a real conflict the lower-standing side (the lower
/// helpful/harmful + trust side, or the older one on a tie) is marked `invalidated`
/// (non-destructive; the row stays on disk for provenance, out of recall). Pitfalls
/// (`DevError`) and beliefs govern themselves and never participate, matching
/// `scan_contradictions`.
///
/// Bounded by the internal belief-scan limit, deterministic, pure-local, and
/// fail-open: an empty input, a `< 2` pool, or any store error marks nothing (`0`)
/// and never panics. Returns how many lessons were invalidated.
pub fn resolve_new_lesson_conflicts(project_root: &Path, new_lessons: &[Lesson]) -> usize {
    // Identities of the genuinely-new advisory lessons (pitfalls/beliefs excluded).
    let new_ids: std::collections::HashSet<(String, String, String)> = new_lessons
        .iter()
        .filter(|l| l.kind != LessonKind::DevError && l.kind != LessonKind::Belief)
        .map(lesson_identity)
        .collect();
    if new_ids.is_empty() {
        return 0;
    }

    let raw = read_all_raw_lessons(project_root);
    let pool: Vec<&Lesson> = raw
        .iter()
        .filter(|l| {
            l.kind != LessonKind::DevError && l.kind != LessonKind::Belief && !l.invalidated
        })
        .take(BELIEF_SCAN_LIMIT)
        .collect();
    if pool.len() < 2 {
        return 0;
    }

    // Pre-tokenise + pre-mark "is new" once per lesson so the pair loop is O(1) each.
    let tokens: Vec<std::collections::HashSet<String>> =
        pool.iter().map(|l| advice_tokens(l)).collect();
    let is_new: Vec<bool> = pool
        .iter()
        .map(|l| new_ids.contains(&lesson_identity(l)))
        .collect();

    let mut to_invalidate: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    for i in 0..pool.len() {
        for j in (i + 1)..pool.len() {
            // Only judge pairs that involve a NEW lesson (the record-time trigger).
            if !is_new[i] && !is_new[j] {
                continue;
            }
            if !genuine_contradiction(pool[i], &tokens[i], pool[j], &tokens[j]) {
                continue;
            }
            to_invalidate.insert(lesson_identity(contradiction_loser(pool[i], pool[j])));
        }
    }
    if to_invalidate.is_empty() {
        return 0;
    }
    apply_invalidations(project_root, &to_invalidate)
}

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
    // Candidate pairs are rendered into a base prompt by both runner paths, so
    // this is a real recall/injection boundary rather than a reporting read.
    let all = read_reconcilable_lessons_for_recall(project_root);
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
        if raw_lesson_store(file).is_some_and(|store| !project_capture_enabled(project_root, store))
        {
            continue;
        }
        let Some(_store_guard) = acquire_raw_store_lock(project_root, file) else {
            continue;
        };
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
        if file_changed && write_raw_lessons_unlocked(project_root, file, &rows) {
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
/// 1. For each fresh lesson, find a bounded set of the most similar PRIOR
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
    if judge.is_some() {
        let recallable = read_reconcilable_lessons_for_recall(project_root);
        if reconcile_lessons(project_root, &recallable, judge) {
            lessons = read_all_raw_lessons(project_root);
        }
    }

    // Memory hygiene (②) + belief fold (①) — both deterministic + pure-local, so
    // they need NO base. Gated on `judge.is_some()` only to keep the no-base
    // `sediment_lessons` byte-for-byte identical (contradiction-scan can mark
    // rows invalid, which would change the sediment set). The contradiction scan
    // runs first (it may invalidate stale/conflicting advice → re-read), then the
    // surviving lessons are folded into the belief ledger.
    if judge.is_some() {
        if scan_contradictions(project_root) > 0 {
            lessons = read_all_raw_lessons(project_root);
        }
        let _ = fold_beliefs(project_root);
    }

    // Capture EVERY domain this run's raw lessons touch BEFORE dropping invalidated ones,
    // so the sediment-clean pass below still wipes a domain whose ONLY lesson was just
    // invalidated (it is about to vanish from `lessons`/`by_key`, but its stale `lesson-*.md`
    // must still be removed or it keeps being retrieved - the ghost-sediment bug).
    let touched_domains: std::collections::HashSet<String> = lessons
        .iter()
        .filter_map(|lesson| safe_domain_segment(&lesson.domain).map(str::to_string))
        .collect();
    // Drop invalidated lessons from the sediment candidate set (they stay on
    // disk for provenance but never become retrievable markdown). No-op for the
    // no-base path, where nothing is ever marked invalid.
    lessons.retain(|l| {
        !l.invalidated && l.is_actionable_pitfall() && safe_domain_segment(&l.domain).is_some()
    });

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
    }

    // Sediment is a derived capture surface, not the authoritative audit ledger.
    // Keep reconciliation/contradiction invalidation above this gate so disabling
    // the derived Markdown view cannot freeze stale raw advice. The independent
    // global projection retains its own leaf-store gate inside `promote_to_global`.
    if !project_capture_enabled(project_root, MemoryStore::LessonSediment) {
        if promote_to_global(project_root, &lessons) > 0 {
            umadev_knowledge::invalidate_cache(project_root);
        }
        return 0;
    }

    // Clean prior auto-sediment BEFORE the empty return. This is what removes
    // already-materialised generic/invalidated advice while retaining its raw
    // audit row. The previous early return left ghost markdown retrievable.
    let _ = global_learned_dir();
    let Some(learned_root) = ensure_managed_learned_dir(project_root) else {
        // An existing managed-component link is an untrusted boundary. Keep the
        // raw audit rows, but never clean or materialise through that link.
        umadev_knowledge::invalidate_cache(project_root);
        return 0;
    };
    for domain in &touched_domains {
        let domain_dir = learned_root.join(domain);
        if ensure_real_child_dir(&learned_root, &domain_dir) {
            clear_auto_sediment_files(&domain_dir);
        }
    }
    if lessons.is_empty() {
        umadev_knowledge::invalidate_cache(project_root);
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

    let mut written = 0usize;

    // Clear stale `lesson-*.md` orphans in every domain dir this run touches
    // BEFORE re-writing. The old `lesson-<domain>-<seq>.md` numbering walked the
    // dedup map in non-deterministic HashMap order, so the same lesson could land
    // on a different seq each run, leaving orphaned files for lessons that were
    // invalidated / re-titled — and those orphans kept being RETRIEVED. We now
    // (a) wipe the prior auto-sediment files in each written domain and (b) name
    // each file by a STABLE content hash of its `(domain, title)` key, so a
    // re-sediment of the same lesson is idempotent (same file, updated in place)
    // and a vanished lesson leaves no ghost. Mirrors the skill index discipline.
    // Clear the sediment files of EVERY domain touched this run (captured in touched_domains
    // BEFORE the invalidated-drop) - INCLUDING a domain whose only lesson was invalidated and
    // so is absent from lessons/by_key. Iterating the post-retain lessons (or by_key) alone
    // left that domain stale lesson-*.md on disk, and it kept being RETRIEVED (the ghost). The
    // re-write below recreates files only for the SURVIVING lessons, so an all-invalidated
    // domain ends up correctly empty.
    for (key, lesson) in &by_key {
        let domain_dir = learned_root.join(&lesson.domain);
        if !ensure_real_child_dir(&learned_root, &domain_dir) {
            continue;
        }
        let path = domain_dir.join(format!(
            "lesson-{domain}-{:016x}.md",
            stable_str_hash(key),
            domain = lesson.domain
        ));
        let body = render_lesson_markdown(lesson);
        // The no-follow atomic writer rejects a hostile final-component link;
        // the verified real domain directory confines its temporary file.
        if write_atomic(&path, &body).is_ok() {
            written += 1;
        }
    }

    // Promote frequently-occurring lessons to the global dir.
    let promoted = promote_to_global(project_root, &lessons);

    // Close the timing race: we just wrote new `.umadev/learned/*.md`, but the
    // BM25 index is content-hash cached, so a retrieval later in THIS SAME run
    // would otherwise still load the pre-sediment cache and miss what we just
    // learned. Invalidating the cache forces the next retrieval to re-scan the
    // now-larger corpus, making this run's lessons retrievable this run.
    // Fail-open (a no-op when nothing was written / no cache exists).
    if written > 0 || promoted > 0 {
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
        LessonKind::Belief => "[belief] Folded rule",
    };
    let pitfall_safety = if lesson.kind == LessonKind::DevError && lesson.is_recognized() {
        "pitfall_safety: classifier-derived-v2\n"
    } else {
        ""
    };
    let keywords_inline = lesson.keywords.join(", ");
    format!(
        "---\nid: lesson-{domain}\ntitle: {title}\ndomain: {domain}\ncategory: learned\ntags: [{tags}]\nmaintainer: auto-sediment\n{pitfall_safety}last_updated: {date}\n---\n\
# {kind_label}: {title}\n\n\
## Symptom\n\n{body}\n\n\
Keywords: {keywords_inline}\n\n\
## Fix\n\n{fix}\n\n\
## Root cause\n\n{root_cause}\n",
        domain = lesson.domain,
        title = lesson.title,
        tags = lesson.keywords.join(", "),
        pitfall_safety = pitfall_safety,
        date = date,
        kind_label = kind_label,
        body = lesson.body,
        keywords_inline = keywords_inline,
        fix = lesson.fix,
        root_cause = lesson.root_cause,
    )
}

/// Whether a lesson group has enough evidence to cross a project boundary.
///
/// Only precisely classified development errors may be shared globally, and
/// only after exact mechanical validation. Independent recurrence may
/// corroborate a project-local hypothesis, but never authorises promotion.
/// Other lesson kinds can contain requirements, review prose, or customer
/// entities and therefore remain project-local until they have a separately
/// designed, typed redaction schema.
fn group_is_global_worthy(group: &[&Lesson]) -> bool {
    group.iter().any(|lesson| {
        lesson.kind == LessonKind::DevError
            && lesson.is_recognized()
            && lesson.pitfall_status() == PitfallStatus::Validated
    })
}

/// Classifier families whose first two signature segments are product-owned
/// constants. Any discriminator after them may be a private package, symbol,
/// path-derived token, or evidence hash and must never cross projects.
const GLOBAL_SAFE_DEV_ERROR_FAMILIES: &[&str] = &[
    "windows/powershell-execution-policy",
    "dependency/test-deps-missing",
    "dependency/module-not-found",
    "dependency/package-manager",
    "runtime/permission",
    "type/type-mismatch",
    "runtime/undefined-access",
    "runtime/panic",
    "runtime/port-in-use",
    "network/cors",
    "network/connection-refused",
    "api/http-error",
    "config/env-missing",
    "build/syntax",
    "test/assertion",
    "build/build-failed",
];

fn global_safe_dev_error_lesson(lesson: &Lesson) -> Option<Lesson> {
    if lesson.kind != LessonKind::DevError || !lesson.is_recognized() {
        return None;
    }
    let mut parts = lesson.signature.split('/');
    let family = format!("{}/{}", parts.next()?, parts.next()?);
    if !GLOBAL_SAFE_DEV_ERROR_FAMILIES.contains(&family.as_str()) {
        return None;
    }
    let (safe_root_cause, safe_fix) = crate::error_kb::classifier_owned_family_guidance(&family)?;

    let domain = family.split('/').next()?;
    let keywords = family
        .split(['/', '-'])
        .filter(|token| token.len() >= 3)
        .map(ToString::to_string)
        .collect();
    Some(Lesson {
        kind: LessonKind::DevError,
        domain: domain.to_string(),
        title: format!("Reusable development-error rule: {family}"),
        body: format!(
            "A recurring development-error family matched `{family}`. Project-local \
             identifiers, evidence, requirements, and signature discriminators were \
             intentionally omitted from global memory."
        ),
        fix: safe_fix,
        root_cause: safe_root_cause,
        keywords,
        source_requirement: String::new(),
        first_seen: utc_now_iso(),
        signature: family,
        occurrences: lesson.hits(),
        context: Vec::new(),
        efficacy: None,
        invalidated: false,
        trust: NEUTRAL_TRUST,
        evidence_count: 0,
        evidence: Vec::new(),
    })
}

/// Promote recurrent or exactly validated classifier families to the global
/// `~/.umadev/learned/` dir. The global representation is rebuilt from a
/// classifier allowlist and drops every project-specific discriminator.
fn promote_to_global(project_root: &Path, lessons: &[Lesson]) -> usize {
    if !capture_enabled(
        project_root,
        MemoryScope::Global,
        MemoryStore::GlobalLessonProjection,
    ) {
        return 0;
    }
    let Some(global_dir) = global_learned_dir() else {
        return 0; // HOME unset or dir doesn't exist yet — skip.
    };

    // Group exact project-local incidents before reducing each to a safe family.
    let mut groups: std::collections::HashMap<String, Vec<&Lesson>> =
        std::collections::HashMap::new();
    for lesson in lessons {
        let key = format!("{}::{}", lesson.domain, lesson.title);
        groups.entry(key).or_default().push(lesson);
    }

    let mut promoted = 0usize;
    for group in groups.values() {
        if !group_is_global_worthy(group) {
            continue;
        }
        // Promote the latest lesson in this group. Use the deterministic
        // total-order from lesson_precedes (first_seen → fix length → title)
        // so same-second timestamps don't make the choice non-deterministic
        // (matches the sediment_lessons dedup policy).
        let latest = group
            .iter()
            .copied()
            .reduce(|acc, l| if lesson_precedes(acc, l) { l } else { acc });
        if let Some(lesson) = latest.and_then(global_safe_dev_error_lesson) {
            let dir = global_dir.join(&lesson.domain);
            if !ensure_real_child_dir(&global_dir, &dir) {
                continue;
            }
            // Derive both filename and hash only from the allowlisted family.
            // This also makes repeated promotion idempotent and path-safe.
            let safe_key = format!("{}::{}", lesson.domain, lesson.title);
            let slug: String = safe_key
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                .collect();
            let slug = truncate(&slug, 80);
            let path = dir.join(format!("{slug}-{:016x}.md", stable_str_hash(&safe_key)));
            let body = render_global_lesson_markdown(&lesson);
            // The global learned dir is SHARED across every project + run on this
            // machine, so a plain `fs::write` (truncate-then-write) could be
            // observed torn if two processes promote the same lesson at once.
            // Write atomically (temp + rename) so a concurrent promotion never
            // leaves a partial global lesson file. Fail-open: a write error is a
            // no-op (the lesson stays project-local).
            if write_atomic(&path, &body).is_ok() {
                promoted += 1;
            }
        }
    }
    promoted
}

/// Render an already-sanitised classifier-family lesson for the cross-project
/// corpus. Callers must construct it with [`global_safe_dev_error_lesson`].
fn render_global_lesson_markdown(lesson: &Lesson) -> String {
    render_lesson_markdown(lesson).replace(
        "maintainer: auto-sediment\n",
        "maintainer: auto-sediment\nglobal_safety: classifier-family-v2\n",
    )
}

/// List all sedimented lesson files (project + global), for reporting.
#[must_use]
pub fn list_sedimented_lessons(project_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(project_learned) = existing_managed_learned_dir(project_root) {
        collect_md_files(&project_learned, &mut files, 0);
    }
    if let Some(global) = global_learned_dir() {
        collect_md_files(&global, &mut files, 0);
    }
    files
}

/// Remove this run's-predecessor auto-sediment markdown files (`lesson-*.md`)
/// from a single domain dir, so a re-sediment doesn't accumulate orphans for
/// lessons that were invalidated / re-titled. Strictly scoped to the
/// auto-sediment `lesson-` prefix — any hand-authored `.md` a user dropped in
/// the learned tree is left untouched. Non-recursive (operates on one domain
/// dir). Fail-open: a read/remove error is ignored.
fn clear_auto_sediment_files(domain_dir: &Path) {
    if !real_dir_no_follow(domain_dir) {
        return;
    }
    let Ok(rd) = fs::read_dir(domain_dir) else {
        return;
    };
    for entry in rd.flatten() {
        if !real_dir_no_follow(domain_dir) {
            return;
        }
        let p = entry.path();
        if !matches!(classify_no_follow(&p), EntryKind::File) {
            continue;
        }
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        let is_md = p.extension().and_then(|s| s.to_str()) == Some("md");
        if is_md && name.starts_with("lesson-") {
            let _ = umadev_state::fs::remove_regular_file(&p);
        }
    }
}

/// Move unsafe legacy auto-generated global markdown out of the retrieval tree.
/// Hand-authored files are preserved; current generated files carry the v2
/// classifier-family marker. The quarantined copy keeps local audit evidence
/// while its non-markdown extension prevents either BM25 or vector indexing.
fn quarantine_unsafe_global_sediments_at(root: &Path) {
    quarantine_unsafe_global_sediments_with(root, |_| {});
}

fn quarantine_unsafe_global_sediments_with(root: &Path, mut before_commit: impl FnMut(&Path)) {
    if !global_learned_tree_is_real(root) {
        return;
    }
    let Ok(domains) = fs::read_dir(root) else {
        return;
    };
    for domain in domains.flatten() {
        if !global_learned_tree_is_real(root) {
            return;
        }
        let domain_path = domain.path();
        if !matches!(classify_no_follow(&domain_path), EntryKind::Dir) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&domain_path) else {
            continue;
        };
        for entry in entries.flatten() {
            if !global_learned_tree_is_real(root)
                || !matches!(classify_no_follow(&domain_path), EntryKind::Dir)
            {
                return;
            }
            let path = entry.path();
            if !matches!(classify_no_follow(&path), EntryKind::File)
                || path.extension().and_then(|s| s.to_str()) != Some("md")
            {
                continue;
            }
            let Ok(text) =
                crate::bounded_fs::read_utf8_beneath(root, &path, MAX_SEDIMENT_MARKDOWN_BYTES)
            else {
                continue;
            };
            if !is_unsafe_global_sediment(&text) {
                continue;
            }

            // Atomically detach this exact directory entry before inspecting it
            // again. A concurrent writer may create a new file at `path`, but we
            // only move the detached staging entry and never delete `path`.
            let staged = quarantine_staging_path(&path);
            if fs::rename(&path, &staged).is_err()
                || !matches!(classify_no_follow(&staged), EntryKind::File)
            {
                continue;
            }
            let Ok(staged_text) =
                crate::bounded_fs::read_utf8_beneath(root, &staged, MAX_SEDIMENT_MARKDOWN_BYTES)
            else {
                restore_staged_file_no_replace(&staged, &path);
                continue;
            };
            if !is_unsafe_global_sediment(&staged_text) {
                restore_staged_file_no_replace(&staged, &path);
                continue;
            }

            let Some(umadev_root) = root.parent() else {
                continue;
            };
            let quarantine_root = umadev_root.join("quarantine");
            let quarantine = quarantine_root.join("learned");
            if !ensure_real_child_dir(umadev_root, &quarantine_root)
                || !ensure_real_child_dir(&quarantine_root, &quarantine)
            {
                continue;
            }
            let destination = quarantine.join(format!(
                "legacy-{:016x}.md.quarantined",
                stable_str_hash(&staged_text)
            ));
            before_commit(&path);
            if global_learned_tree_is_real(root)
                && matches!(classify_no_follow(&domain_path), EntryKind::Dir)
                && real_dir_no_follow(&quarantine_root)
                && real_dir_no_follow(&quarantine)
            {
                // Source and destination are below the same `.umadev` root, so
                // rename is atomic on the normal layout. On any failure the
                // non-markdown staging file remains as recoverable audit data.
                let _ = fs::rename(&staged, destination);
            }
        }
    }
}

fn is_unsafe_global_sediment(text: &str) -> bool {
    let auto_generated = umadev_knowledge::front_matter_field(text, "maintainer")
        == umadev_knowledge::FrontMatterField::Value("auto-sediment");
    let current_safe = umadev_knowledge::front_matter_field(text, "global_safety")
        == umadev_knowledge::FrontMatterField::Value("classifier-family-v2");
    auto_generated && !current_safe
}

fn quarantine_staging_path(path: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("lesson.md");
    path.with_file_name(format!(
        ".{name}.{}.{}.{}.quarantine-pending",
        std::process::id(),
        stamp,
        sequence
    ))
}

/// Restore a raced non-legacy file without ever replacing a newer `original`.
fn restore_staged_file_no_replace(staged: &Path, original: &Path) {
    if matches!(classify_no_follow(staged), EntryKind::File) {
        // `hard_link` is an atomic create-new operation: it fails when another
        // writer already recreated `original`. The staging link is retained as
        // recovery evidence, avoiding another check-then-delete race.
        let _ = fs::hard_link(staged, original);
    }
}

/// Maximum depth for the sedimented-lesson listing walk. The learned tree is
/// domain-shallow in practice; the cap is defense-in-depth so a stray symlink
/// cycle (already unreachable via the no-follow classification below) or a
/// pathological hand-nested tree can't recurse unbounded.
const MAX_LESSON_DEPTH: usize = 12;

fn collect_md_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_LESSON_DEPTH {
        return;
    }
    if !matches!(classify_no_follow(dir), EntryKind::Dir) {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        // No-follow: a symlink (dir or file) is skipped, so the listing can't
        // escape the learned tree or loop through a cycle.
        match classify_no_follow(&p) {
            EntryKind::Dir => {
                // Skip the _raw dir (raw JSONL, not retrievable markdown).
                if p.file_name().is_some_and(|n| n == "_raw") {
                    continue;
                }
                collect_md_files(&p, out, depth + 1);
            }
            EntryKind::File => {
                if p.extension().and_then(|s| s.to_str()) == Some("md") {
                    out.push(p);
                }
            }
            EntryKind::Skip => {}
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
        // Recognition, recurrence and frequency describe IMPORTANCE, not
        // relevance. Without a requirement/stack overlap they must never turn an
        // unrelated pitfall into prompt context.
        if score == 0 {
            return 0;
        }
        if l.is_recognized() {
            score += 1;
        }
        // Efficacy steering: a pitfall that recurred DESPITE being warned about
        // gets escalated (its fix is failing — surface it hard); one whose fix
        // is proven (validated) is damped so it stops crowding the prompt once
        // it's reliably handled.
        match l.pitfall_status() {
            PitfallStatus::Invalidated => return 0,
            PitfallStatus::Corroborated => score += 2,
            PitfallStatus::Validated => score -= 4,
            PitfallStatus::Hypothesis => {}
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
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|at| at.with_timezone(&Utc))
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
        PitfallStatus::Invalidated => imp -= 0.35,
        PitfallStatus::Corroborated => imp += 0.2,
        PitfallStatus::Validated => imp -= 0.3,
        PitfallStatus::Hypothesis => {}
    }
    imp.clamp(0.05, 1.0)
}

/// A lesson is efficacy-POISON when it has accumulated enough OUTCOME samples AND
/// its helpful ratio has fallen to/below the poison floor — well-sampled evidence
/// that reusing it hurts more than it helps.
///
/// Gated by [`EFFICACY_MIN_SAMPLES`] so it NEVER fires on a single (or thin)
/// observation — a lesson must genuinely earn its demotion. Deterministic +
/// conservative + fail-open: an un-sampled lesson (`helpful_ratio` → `None`) is
/// never poison. Recall EXCLUDES poison lessons (demoted below the cut), but they
/// stay on disk for provenance — the same non-destructive posture as `invalidated`.
fn is_efficacy_poison(l: &Lesson) -> bool {
    l.efficacy_samples() >= EFFICACY_MIN_SAMPLES
        && l.helpful_ratio()
            .is_some_and(|r| r <= EFFICACY_POISON_RATIO)
}

/// Multiplicative recall-ranking factor from OUTCOME efficacy — the outcome half
/// of "did reusing this actually HELP?", grounded in the DISCRETE helpful/harmful
/// tally (distinct from the smoothed [`Lesson::trust`] EMA, and reinforcing it).
///
/// Neutral `1.0` until observed, so an un-sampled lesson ranks EXACTLY as before
/// (no behaviour change on a fresh corpus). Once observed it scales in
/// `[~0.3, ~1.2]` with the helpful ratio, so a proven-helpful lesson floats up to
/// win the scarce injection budget and a proven-unhelpful one sinks — folded in as
/// just one more bounded multiplicative axis (it modulates, never dominates).
fn efficacy_weight(l: &Lesson) -> f64 {
    match l.helpful_ratio() {
        None => 1.0,
        Some(r) => 0.3 + r * 0.9,
    }
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
/// A zero-relevance lesson keeps a small floor so retention and reconciliation
/// can still order stored evidence by recency × importance. Recall itself
/// abstains on zero relevance in [`select_relevant_lessons`]. Higher = keep /
/// surface first.
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
    // Trust is the fourth axis: a lesson whose injected advice coincided with
    // passing gates floats up; one that coincided with failures sinks. Folded in
    // as a multiplicative factor (like the other axes) so it modulates, never
    // dominates — and clamped above 0 by `Lesson::trust`, so it can damp a
    // distrusted lesson hard without ever zeroing it out of recovery.
    //
    // Efficacy (`efficacy_weight`) is the fifth axis: the DISCRETE recalled-then-
    // passed / recalled-then-failed tally, so a lesson that has PROVEN helpful by
    // outcome outranks an equally-relevant one that has proven unhelpful. Neutral
    // (1.0) until observed, so an un-sampled lesson is unaffected.
    let recency_at = if l.kind == LessonKind::DevError {
        l.last_observed_at()
    } else {
        l.last_reinforced_at().unwrap_or(l.first_seen.as_str())
    };
    rel * lesson_importance(l)
        * recency_weight(recency_at, now)
        * f64::from(l.trust())
        * efficacy_weight(l)
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
    if !project_recall_enabled(project_root, MemoryStore::Pitfalls)
        || !project_capture_enabled(project_root, MemoryStore::Pitfalls)
        || !project_capture_enabled(project_root, MemoryStore::PitfallReflections)
    {
        return None;
    }
    let insight = crate::error_kb::classify_error(failure_detail);
    if !insight.recognized {
        return None;
    }
    let sig = precise_actionable_signature(&insight.signature, failure_detail);
    let mut hits: Vec<Lesson> = read_canonical_pitfalls(project_root)
        .into_iter()
        .filter(|l| {
            if !l.is_live_actionable_pitfall() {
                return false;
            }
            normalize_signature(&l.signature) == sig
        })
        .collect();
    // Efficacy PRUNE (step 3): a demoted (poison) pitfall is out of the loop — it
    // must not drive a reflection consult either. Sample-gated, so a genuine
    // recurrence is never culled before it has earned enough bad outcomes.
    hits.retain(|l| !is_efficacy_poison(l));
    if hits.is_empty() {
        return None;
    }
    hits.sort_by(|a, b| {
        let recurring = |l: &Lesson| u8::from(l.pitfall_status() == PitfallStatus::Recurring);
        recurring(b)
            .cmp(&recurring(a))
            .then(b.hits().cmp(&a.hits()))
    });
    let mut lesson = hits
        .into_iter()
        .next()
        .filter(|l| l.pitfall_status() == PitfallStatus::Recurring)?;
    if !project_recall_enabled(project_root, MemoryStore::PitfallReflections) {
        if let Some(efficacy) = &mut lesson.efficacy {
            efficacy.next_strategy.clear();
        }
    }
    Some(lesson)
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
    if !project_recall_enabled(project_root, MemoryStore::Pitfalls) {
        return String::new();
    }
    let insight = crate::error_kb::classify_error(failure_detail);
    // Abstain on the generic fallback — its signature is too coarse to match a
    // specific prior root cause precisely.
    if !insight.recognized {
        return String::new();
    }
    // Normalise to the SAME stable key the store dedups under, so a recurring
    // failure whose offending path/version differs run-to-run still matches the
    // recorded lesson (otherwise the lookup would miss the very pitfall it hit).
    let sig = precise_actionable_signature(&insight.signature, failure_detail);
    // Stored advice can contain discriminator-specific commands (for example a
    // package name). Match the complete normalised signature only; a family
    // neighbour's fix is unsafe to inject. The caller already has the incoming
    // classifier's generic family guidance when no exact history exists.
    let mut hits: Vec<Lesson> = read_canonical_pitfalls(project_root)
        .into_iter()
        .filter(|l| {
            if !l.is_live_actionable_pitfall() {
                return false;
            }
            normalize_signature(&l.signature) == sig
        })
        .collect();
    // Efficacy PRUNE (step 3): drop well-sampled poison pitfalls before ranking, so
    // a fix that has proven to hurt more than help stops being re-injected. If that
    // leaves nothing, abstain — injecting nothing beats injecting a proven-bad fix
    // (the same "knowledge → noise" defence this function already takes on the
    // generic-fallback path). Sample-gated, so a first recurrence is never culled.
    hits.retain(|l| !is_efficacy_poison(l));
    if hits.is_empty() {
        return String::new();
    }
    // Recurring-despite-warning first (these need a harder push), then the most
    // outcome-EFFECTIVE (proven-helpful ratio), then the most frequently-hit — so
    // among equal-relevance same-family pitfalls the one whose warning most reliably
    // led to a pass surfaces first. Un-sampled pitfalls tie at neutral, so ordering
    // is unchanged until outcomes accumulate.
    hits.sort_by(|a, b| {
        let recurring = |l: &Lesson| u8::from(l.pitfall_status() == PitfallStatus::Recurring);
        recurring(b)
            .cmp(&recurring(a))
            .then_with(|| {
                b.helpful_ratio()
                    .unwrap_or(0.5)
                    .total_cmp(&a.helpful_ratio().unwrap_or(0.5))
            })
            .then(b.hits().cmp(&a.hits()))
    });
    let top = &hits[0];
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
        // far more actionable than the bare "换个根本不同方案" template line. BUT a
        // reflected strategy is root-cause-specific: only surface it (and the
        // failed-fix ledger) when `top` is the exact same signature, never across
        // a mere family match. A family-only match falls back to the generic
        // template line so a different root cause never inherits another's fix.
        let strategy = if project_recall_enabled(project_root, MemoryStore::PitfallReflections) {
            top.efficacy
                .as_ref()
                .map(|e| e.next_strategy.trim())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
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
        // harder". This is the structured "失败修法 + 换思路" guidance — but only
        // for an exact-signature match, since a family neighbour's failed fixes
        // targeted a different root cause.
        out.push_str(&render_failed_fixes(top));
    }
    // Deliberately PURE: selecting/rendering guidance is not proof that it was
    // sent to a base. The runner/director commits the attempt through
    // `commit_pitfall_fix_attempt` only after the host accepted/ran the turn.
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

/// Hard cap on how many corrective deltas the injected playbook carries — the
/// COUNT side of the bound (the byte side is [`MEMORY_PLAYBOOK_BUDGET`]). A
/// delta-playbook is, by definition, a SMALL curated set: a handful of
/// high-signal "next time do X instead of Y" rules, not the ledger. Three is
/// enough to cover the top on-stack matches while staying compact.
/// [`select_relevant_lessons`] fills at most this many slots from positively
/// matched memories only.
const MEMORY_PLAYBOOK_MAX_DELTAS: usize = 3;

/// Hard character budget for the injected delta-playbook block (the rendered
/// memory digest). The selection is already COUNT-capped to a small ranked set
/// (see [`select_relevant_lessons`] — at most [`MEMORY_PLAYBOOK_MAX_DELTAS`]),
/// but an individual distilled delta's body is not itself byte-bounded: a
/// belief's representative fix is the LONGEST member fix (see
/// [`fold_one_cluster`]) and a captured pitfall's `fix`/`root_cause` are
/// whatever the classifier produced. Without a block-level ceiling, a few fat
/// deltas could still crowd the firmware budget / dilute signal — the exact
/// context-collapse a delta-playbook exists to avoid. This caps the whole
/// assembled block so EVERY caller (the firmware path is additionally bounded by
/// the `FirmwareBuilder`, but the runner / director-loop inject this string
/// directly) gets a compact playbook, never a wall of detail. ~3K chars ≈ a few
/// hundred tokens — room for the ranked deltas (incl. one reflective escalation
/// strategy) while staying a small, high-signal overlay.
const MEMORY_PLAYBOOK_BUDGET: usize = 3_000;

/// Render the most relevant prior-run lessons for the current phase's prompt —
/// the compact DELTA PLAYBOOK: a small, ranked, deduplicated set of high-level
/// corrective deltas (not raw episodes). Returns a formatted markdown block
/// (empty string when no lessons exist — so the prompt is unchanged for
/// first-ever runs).
///
/// Triggering matches the pitfall against the project's real tech-stack
/// fingerprint (using the internal lesson-trigger score), not just the requirement prose,
/// then ranks by the composite lesson-decay score
/// (`recency · importance · relevance`) so a fresh, important, on-stack lesson
/// outranks an old high-frequency one. We don't call BM25 here to avoid a
/// circular dependency between the agent and knowledge crates at prompt-assembly
/// time — the BM25 index already picks up learned/ files during
/// `phase_knowledge_digest`.
///
/// **Bounded by construction (count AND bytes):** the selection is count-capped
/// to the internal maximum-delta limit (near-duplicates already
/// merged into beliefs upstream, see [`fold_beliefs`]), and the assembled block
/// is capped to the internal memory-playbook character budget here — a lower-rank delta
/// that would overflow is dropped, and the single top delta is head-truncated as
/// a hard backstop so the block can never exceed the budget. Fail-open: an empty
/// or unreadable store yields an empty string, never a panic.
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
    // Assemble the ranked deltas under the hard char budget. The deltas arrive
    // highest-rank first; append each while it fits, then stop — a lower-rank
    // delta that would overflow is dropped (it is, by construction, the least
    // important). If the FIRST (top-ranked) delta alone overflows, head-keep a
    // truncated copy so the block is never just a header. This bounds the block
    // for direct callers (runner / director-loop); the firmware path is bounded
    // again by the `FirmwareBuilder`.
    let mut any_delta = false;
    for lesson in &selected {
        let frag = render_one_lesson(lesson);
        let remaining = MEMORY_PLAYBOOK_BUDGET.saturating_sub(out.chars().count());
        if frag.chars().count() <= remaining {
            out.push_str(&frag);
            any_delta = true;
        } else {
            if !any_delta && remaining > 0 {
                out.push_str(&truncate(&frag, remaining));
            }
            break;
        }
    }

    out
}

/// Structured sibling of [`relevant_lessons_for_prompt`]: returns the SAME
/// ranked selection as `(rank, Lesson)` pairs (rank 0 = best) instead of a
/// rendered string. The String API stays byte-for-byte unchanged; this variant
/// exists so a higher-altitude assembler (the coach's dual-channel reranker)
/// can fuse the fingerprint-decay channel with the BM25 knowledge channel by
/// RANK without re-deriving the selection.
///
/// Pure read: returning candidates does not prove they survived prompt budgeting
/// or reached a host turn, so it performs no outcome/injection bookkeeping.
#[must_use]
pub fn relevant_lessons_for_prompt_ranked(
    project_root: &Path,
    requirement: &str,
) -> Vec<(usize, Lesson)> {
    let selected = select_relevant_lessons(project_root, requirement);
    if selected.is_empty() {
        return Vec::new();
    }
    selected.into_iter().enumerate().collect()
}

/// Shared selection core for both the String and structured lesson APIs: builds
/// the trigger query (requirement words + project tech-stack fingerprint), scores
/// every lesson on the (relevance, composite-decay) axes, and returns at most
/// three positively matched deltas. Zero-overlap memory abstains instead of
/// injecting a merely recent or frequent experience. Pure read; production does
/// not attribute later outcomes without exact sent IDs.
fn select_relevant_lessons(project_root: &Path, requirement: &str) -> Vec<Lesson> {
    // Candidates = raw lessons PLUS the folded beliefs. Beliefs are denser
    // summaries of clusters of raw lessons; the demotion step below hides the
    // exact raw lessons a matched belief already covers, so the prompt shows the
    // distilled rule instead of its near-duplicate evidence.
    let mut lessons = read_lessons_for_recall(project_root);
    // Efficacy PRUNE (step 3): a well-sampled lesson whose advice has proven to
    // hurt more than help (see [`is_efficacy_poison`]) is demoted OUT of the recall
    // candidate set entirely — a wrong lesson must stop poisoning behaviour. Gated
    // by a minimum sample size so a single bad outcome never culls a lesson;
    // fail-open (an un-sampled / thinly-sampled lesson is untouched). The row stays
    // on disk for provenance — pruned from RECALL, never deleted.
    lessons.retain(|l| !is_efficacy_poison(l));
    if lessons.is_empty() {
        return Vec::new();
    }

    // The trigger query = the requirement's words PLUS the project's real
    // tech-stack fingerprint (dependency names). Matching on the *stack* — not
    // the prose — is what makes triggering precise: a `react-router-dom`
    // pitfall fires exactly when this project depends on react-router-dom,
    // regardless of how the requirement is worded.
    let expanded = umadev_knowledge::expand_bilingual_query(requirement);
    let mut query: std::collections::HashSet<String> = umadev_knowledge::tokenize(&expanded)
        .into_iter()
        // Single CJK characters are useful to BM25 as a weak frequency signal,
        // but too broad to authorize durable-memory recall. Require a bigram or
        // an ASCII identifier here so zero-overlap abstention stays precise.
        .filter(|word| word.chars().count() >= 2)
        .collect();
    for tok in project_context_tokens(project_root) {
        query.insert(tok);
    }

    // Belief preference (①): a belief that POSITIVELY matches the current query
    // is preferred over its raw evidence. Collect the evidence keys of every
    // matching belief and drop the raw lessons they cover from the candidate set,
    // so the dense belief surfaces instead of several near-duplicate originals. A
    // belief that does NOT match the query hides nothing (its evidence stays
    // eligible on its own merits). Fail-open: with no beliefs this is a no-op and
    // behaviour is byte-for-byte the prior raw-only selection.
    let covered: std::collections::HashSet<String> = lessons
        .iter()
        .filter(|l| l.kind == LessonKind::Belief && lesson_trigger_score(l, &query) > 0)
        .flat_map(|b| b.evidence.iter().cloned())
        .collect();
    if !covered.is_empty() {
        lessons.retain(|l| l.kind == LessonKind::Belief || !covered.contains(&evidence_key(l)));
    }

    // Score each lesson on TWO axes:
    // - `rel` (raw relevance i64) is an abstention gate: a lesson only counts as
    //   "matched right now" when its situation intersects the query/stack.
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
            .then_with(|| b.2.last_observed_at().cmp(a.2.last_observed_at()))
    });

    // Positively matched only. A chronic but unrelated pitfall is not a useful
    // reminder; exact failure-time recall remains available through
    // `lessons_for_error` when its signature actually recurs.
    let mut selected: Vec<Lesson> = scored
        .iter()
        .filter(|(score, _, _)| *score > 0)
        .take(MEMORY_PLAYBOOK_MAX_DELTAS)
        .map(|(_, _, lesson)| (**lesson).clone())
        .collect();
    if !project_recall_enabled(project_root, MemoryStore::PitfallReflections) {
        for lesson in &mut selected {
            if let Some(efficacy) = &mut lesson.efficacy {
                efficacy.next_strategy.clear();
            }
        }
    }
    selected
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
        LessonKind::Belief => "[belief]",
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
                    "\n   [warn] 上次已警示但仍复发 —— 改用这个不同的高层做法：\n   {}",
                    truncate(strategy, 600)
                )
            } else {
                "\n   [warn] 上次已警示但仍复发 —— 之前的修法不够,这次必须换更彻底的方案并验证。"
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
    } else if lesson.kind == LessonKind::Belief {
        // A belief leads with its evidence weight so the worker reads it as a
        // confirmed rule ("seen N times"), then the distilled fix.
        let n = lesson.evidence_count.max(lesson.hits());
        format!(
            "{icon} **{}** (印证 {n} 次)
   规则: {}

",
            lesson.title, lesson.fix
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

/// Commit a genuine failure-time fix attempt after the host has accepted/run the
/// turn. Selection/rendering APIs stay pure because a firmware block can be
/// truncated, prewarmed, or assembled more than once without ever reaching the
/// base. Returns an opaque attempt token for exact-once settlement. Fail-open.
#[must_use]
pub fn commit_pitfall_fix_attempt(project_root: &Path, failure_detail: &str) -> Option<String> {
    // Issuing a new attribution token is automatic capture. Recall may remain
    // enabled independently, in which case advice is still injected but no new
    // efficacy state is written. Settlement below deliberately remains allowed
    // for a token committed before capture was disabled.
    if !project_capture_enabled(project_root, MemoryStore::Pitfalls) {
        return None;
    }
    let insight = crate::error_kb::classify_error(failure_detail);
    if !insight.recognized {
        return None;
    }
    let sig = precise_actionable_signature(&insight.signature, failure_detail);
    let attempt_id = format!("fix-{}", next_capture_episode_id(&utc_now_iso()));
    let _kb_guard = acquire_raw_store_lock(project_root, DEV_ERRORS_FILE)?;
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    // Recall's canonical projection takes advice from the highest-hit raw row
    // (first row on a tie). Select that same source row here so the outcome is
    // attributed to the advice actually rendered, never to a legacy shadow.
    let mut source_index: Option<usize> = None;
    for (index, lesson) in store.iter().enumerate() {
        if lesson.is_live_actionable_pitfall()
            && normalize_signature(&lesson.signature) == sig
            && source_index.is_none_or(|current| lesson.hits() > store[current].hits())
        {
            source_index = Some(index);
        }
    }
    let committed = if let Some(source_index) = source_index {
        let lesson = &mut store[source_index];
        let hits = lesson.hits();
        let efficacy = lesson.efficacy.get_or_insert(PitfallEfficacy {
            occ_at_injection: hits,
            ..PitfallEfficacy::default()
        });
        efficacy.injected = efficacy.injected.saturating_add(1);
        efficacy.occ_at_injection = hits;
        // Committing a turn records only that advice reached the host. Preserve
        // the last settled lifecycle verdict until this exact token is settled;
        // a crash/abandoned turn must not downgrade NeedsRevision to Active.
        efficacy.pending_fix_attempts.push(attempt_id.clone());
        // A single session cannot legitimately have an unbounded number of
        // unsettled repair turns. Bound corrupt/abandoned tokens fail-open.
        if efficacy.pending_fix_attempts.len() > 4 {
            efficacy.pending_fix_attempts.remove(0);
        }
        true
    } else {
        false
    };
    if committed && write_raw_lessons_unlocked(project_root, DEV_ERRORS_FILE, &store) {
        Some(attempt_id)
    } else {
        None
    }
}

/// Settle one previously committed repair attempt exactly once.
///
/// A mechanical pass validates the attempted fix. A failed overall gate only
/// penalises it when the gate's actual evidence still contains this token's
/// exact normalised signature; a different new error, skip, unavailable check,
/// or missing evidence consumes the token as [`PitfallFixSettlement::Inconclusive`]
/// without changing trust/lifecycle. This prevents "fixed the original module
/// error, then hit a new lint error" from poisoning the original advice.
#[must_use]
pub fn settle_pitfall_fix_attempt(
    project_root: &Path,
    attempt_id: &str,
    result: PitfallFixAttemptResult,
) -> PitfallFixSettlement {
    if attempt_id.trim().is_empty() {
        return PitfallFixSettlement::NotFound;
    }
    let Some(_kb_guard) = acquire_raw_store_lock(project_root, DEV_ERRORS_FILE) else {
        return PitfallFixSettlement::Inconclusive;
    };
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    let now = utc_now_iso();
    for lesson in &mut store {
        if !lesson.is_live_actionable_pitfall() {
            continue;
        }
        let pending_pos = lesson.efficacy.as_ref().and_then(|efficacy| {
            efficacy
                .pending_fix_attempts
                .iter()
                .position(|id| id == attempt_id)
        });
        let Some(pending_pos) = pending_pos else {
            continue;
        };
        let same_signature_failed = match &result {
            PitfallFixAttemptResult::VerificationFailed(detail) => {
                evidence_matches_attempt(detail, &lesson.signature)
            }
            _ => false,
        };
        let fix = lesson.fix.clone();
        if let Some(efficacy) = lesson.efficacy.as_mut() {
            efficacy.pending_fix_attempts.remove(pending_pos);
        }
        let settlement = match result {
            PitfallFixAttemptResult::Passed => {
                lesson.apply_exact_trust_feedback(true);
                let hits = lesson.hits();
                let signature = lesson.signature.clone();
                let efficacy = lesson.efficacy.get_or_insert_with(PitfallEfficacy::default);
                efficacy.occ_at_injection = hits;
                efficacy.proven_fix = true;
                efficacy.recurred_after_warning = false;
                efficacy.last_verified_at.clone_from(&now);
                efficacy.exact_last_verified_at.clone_from(&now);
                let _ = remember_observation(
                    efficacy,
                    repair_outcome_evidence(
                        project_root,
                        attempt_id,
                        &signature,
                        &now,
                        KnowledgeEvidenceOutcome::FixSucceeded,
                    ),
                );
                PitfallFixSettlement::Passed
            }
            PitfallFixAttemptResult::VerificationFailed(detail) if same_signature_failed => {
                drop(detail);
                lesson.apply_exact_trust_feedback(false);
                let hits = lesson.hits();
                let signature = lesson.signature.clone();
                let efficacy = lesson.efficacy.get_or_insert_with(PitfallEfficacy::default);
                efficacy.occ_at_injection = hits;
                efficacy.recurred_after_warning = true;
                efficacy.proven_fix = false;
                efficacy.last_recurred_at.clone_from(&now);
                efficacy.last_fix_failed_at.clone_from(&now);
                efficacy.exact_last_fix_failed_at.clone_from(&now);
                remember_failed_fix(efficacy, &fix);
                let _ = remember_observation(
                    efficacy,
                    repair_outcome_evidence(
                        project_root,
                        attempt_id,
                        &signature,
                        &now,
                        KnowledgeEvidenceOutcome::FixFailed,
                    ),
                );
                PitfallFixSettlement::SameSignatureFailed
            }
            PitfallFixAttemptResult::VerificationFailed(detail) => {
                drop(detail);
                PitfallFixSettlement::Inconclusive
            }
            PitfallFixAttemptResult::Unknown => PitfallFixSettlement::Inconclusive,
        };
        // Even an inconclusive result consumes the token: later unrelated
        // failures must never settle this completed/abandoned turn retroactively.
        return if write_raw_lessons_unlocked(project_root, DEV_ERRORS_FILE, &store) {
            settlement
        } else {
            // The in-memory result was never committed. In particular, never
            // let callers emit "fix confirmed" over a locked/failed rename.
            PitfallFixSettlement::Inconclusive
        };
    }
    PitfallFixSettlement::NotFound
}

fn repair_outcome_evidence(
    project_root: &Path,
    attempt_id: &str,
    signature: &str,
    observed_at: &str,
    outcome: KnowledgeEvidenceOutcome,
) -> PitfallObservation {
    let outcome_id = match outcome {
        KnowledgeEvidenceOutcome::FixSucceeded => "success",
        KnowledgeEvidenceOutcome::FixFailed => "failure",
        KnowledgeEvidenceOutcome::Observed | KnowledgeEvidenceOutcome::Conflict => "other",
    };
    PitfallObservation {
        observed_at: observed_at.to_string(),
        episode_id: privacy_fingerprint("umadev:repair-outcome-evidence-id:v1", attempt_id),
        evidence_hash: privacy_fingerprint(
            "umadev:repair-outcome-evidence:v1",
            &format!("{signature}\0{outcome_id}"),
        ),
        source: "exact-mechanical-repair-verification".to_string(),
        base: "umadev-mechanical-verifier".to_string(),
        base_version: env!("CARGO_PKG_VERSION").to_string(),
        workspace_scope: project_workspace_scope(project_root),
        outcome,
        causal_attempt_id: attempt_id.to_string(),
    }
}

fn evidence_matches_attempt(evidence: &str, expected_signature: &str) -> bool {
    let expected = normalize_signature(expected_signature);
    if expected.is_empty()
        || expected
            .rsplit('/')
            .next()
            .is_some_and(|discriminator| discriminator.starts_with("u-"))
    {
        return false;
    }
    std::iter::once(evidence)
        .chain(evidence.lines())
        .any(|part| {
            let insight = crate::error_kb::classify_error(part);
            insight.recognized && precise_actionable_signature(&insight.signature, part) == expected
        })
}

/// Legacy compatibility helper that updates pre-v1 audit fields by signature.
/// It does not validate a rule; production repair settlement requires an exact
/// [`commit_pitfall_fix_attempt`] token consumed by
/// [`settle_pitfall_fix_attempt`].
pub fn mark_pitfalls_resolved(project_root: &Path, raw_errors: &[String]) -> usize {
    let want: std::collections::HashSet<String> = raw_errors
        .iter()
        .filter(|e| crate::error_kb::looks_like_error(e))
        .filter_map(|e| {
            let insight = crate::error_kb::classify_error(e);
            insight
                .recognized
                .then(|| precise_actionable_signature(&insight.signature, e))
        })
        .collect();
    if want.is_empty() {
        return 0;
    }
    // Shared dev-errors lock: keep this read-modify-write atomic against the
    // other mutators so a concurrent capture/strategy update isn't clobbered.
    let Some(_kb_guard) = acquire_raw_store_lock(project_root, DEV_ERRORS_FILE) else {
        return 0;
    };
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    if store.is_empty() {
        return 0;
    }
    let mut marked = 0;
    let verified_at = utc_now_iso();
    for l in &mut store {
        if l.is_live_actionable_pitfall() && want.contains(&normalize_signature(&l.signature)) {
            let occ = l.hits();
            let eff = l.efficacy.get_or_insert(PitfallEfficacy {
                occ_at_injection: occ,
                ..PitfallEfficacy::default()
            });
            eff.proven_fix = true;
            eff.recurred_after_warning = false;
            eff.last_verified_at.clone_from(&verified_at);
            // Re-baseline the occurrence counter to NOW so a later recurrence
            // (occurrences > occ_at_injection) is detected and flips
            // `recurred_after_warning`, demoting this from "Validated".
            eff.occ_at_injection = occ;
            marked += 1;
        }
    }
    if marked > 0 && !write_raw_lessons_unlocked(project_root, DEV_ERRORS_FILE, &store) {
        return 0;
    }
    marked
}

// =====================================================================
// Explicit compatibility trust feedback.
//
// These identity/signature mutators remain available for tests and callers that
// already own a causal identity. UmaDev's production prompt assembly does NOT
// call them: candidate selection cannot prove which memories survived budgeting
// and reached one host turn. The production pitfall repair loop instead uses
// commit_pitfall_fix_attempt → settle_pitfall_fix_attempt exactly once.
// =====================================================================

/// Explicit compatibility trust update by exact classified signature. Not used
/// by production passive recall; the attempt-token API is the authoritative
/// repair settlement path. Fail-open: unrecognised errors / empty store → 0.
pub fn apply_dev_error_trust(project_root: &Path, raw_errors: &[String], passed: bool) -> usize {
    let want: std::collections::HashSet<String> = raw_errors
        .iter()
        .filter(|e| crate::error_kb::looks_like_error(e))
        .filter_map(|e| {
            let insight = crate::error_kb::classify_error(e);
            insight
                .recognized
                .then(|| precise_actionable_signature(&insight.signature, e))
        })
        .collect();
    if want.is_empty() {
        return 0;
    }
    // Shared dev-errors lock: keep this read-modify-write atomic against the
    // other mutators so a concurrent capture/strategy update isn't clobbered.
    let Some(_kb_guard) = acquire_raw_store_lock(project_root, DEV_ERRORS_FILE) else {
        return 0;
    };
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    if store.is_empty() {
        return 0;
    }
    let mut adjusted = 0usize;
    for l in &mut store {
        if l.is_live_actionable_pitfall() && want.contains(&normalize_signature(&l.signature)) {
            l.apply_trust_feedback(passed);
            adjusted += 1;
        }
    }
    if adjusted > 0 && !write_raw_lessons_unlocked(project_root, DEV_ERRORS_FILE, &store) {
        return 0;
    }
    adjusted
}

/// Apply a trust pass/fail step to the dev-error pitfalls whose normalised
/// signature is in `signatures` (the signatures a caller confirmed were sent
/// in a prompt). Use this when the gate outcome is
/// known but the raw error strings aren't on hand: pass the signatures captured
/// at injection time. Returns how many records were adjusted. Fail-open.
pub fn apply_trust_for_signatures(
    project_root: &Path,
    signatures: &[String],
    passed: bool,
) -> usize {
    let want: std::collections::HashSet<String> =
        signatures.iter().map(|s| normalize_signature(s)).collect();
    if want.is_empty() {
        return 0;
    }
    // Shared dev-errors lock: keep this read-modify-write atomic against the
    // other mutators so a concurrent capture/strategy update isn't clobbered.
    let Some(_kb_guard) = acquire_raw_store_lock(project_root, DEV_ERRORS_FILE) else {
        return 0;
    };
    let mut store = read_raw_lessons(project_root, DEV_ERRORS_FILE);
    if store.is_empty() {
        return 0;
    }
    let mut adjusted = 0usize;
    for l in &mut store {
        if l.is_live_actionable_pitfall() && want.contains(&normalize_signature(&l.signature)) {
            l.apply_trust_feedback(passed);
            adjusted += 1;
        }
    }
    if adjusted > 0 && !write_raw_lessons_unlocked(project_root, DEV_ERRORS_FILE, &store) {
        return 0;
    }
    adjusted
}

/// Apply a trust pass/fail step to NON-pitfall lessons (failures / revisions /
/// validated patterns / beliefs) identified by their internal lesson-identity triple
/// — the identities a recall surfaced into the prompt. Operates per
/// reconcilable file PLUS the belief ledger so a surfaced belief's trust is
/// updated too. Returns how many records were adjusted. Fail-open.
pub fn apply_trust_for_identities(
    project_root: &Path,
    identities: &[(String, String, String)],
    passed: bool,
) -> usize {
    let want: std::collections::HashSet<&(String, String, String)> = identities.iter().collect();
    if want.is_empty() {
        return 0;
    }
    let mut adjusted = 0usize;
    let mut files: Vec<&str> = RECONCILE_FILES.to_vec();
    files.push(BELIEFS_FILE);
    for file in files {
        let Some(_store_guard) = acquire_raw_store_lock(project_root, file) else {
            continue;
        };
        let mut rows = read_raw_lessons(project_root, file);
        if rows.is_empty() {
            continue;
        }
        let mut file_changed = false;
        let mut file_adjusted = 0usize;
        for row in &mut rows {
            if want.contains(&lesson_identity(row)) {
                row.apply_trust_feedback(passed);
                file_changed = true;
                file_adjusted = file_adjusted.saturating_add(1);
                adjusted += 1;
            }
        }
        if file_changed && !write_raw_lessons_unlocked(project_root, file, &rows) {
            adjusted = adjusted.saturating_sub(file_adjusted);
        }
    }
    adjusted
}

/// Summary of the pitfall KB's self-verification state, for reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PitfallEfficacySummary {
    /// Distinct actionable dev-error pitfalls recorded.
    pub total: usize,
    /// Pitfalls whose fix is proven (warned, no recurrence since).
    pub validated: usize,
    /// Pitfalls that recurred despite being warned — fix insufficient.
    pub recurring: usize,
    /// Pitfalls not yet surfaced / unproven.
    pub active: usize,
    /// One-evidence/provenance-incomplete hypotheses.
    pub hypothesis: usize,
    /// Rules supported by at least two distinct evidence ids.
    pub corroborated: usize,
    /// Rules revoked by explicit conflict or a newer causal repair failure.
    pub invalidated: usize,
    /// Low-information generic rows retained only in raw JSONL for audit.
    pub quarantined_records: usize,
    /// Historical hit total stored on quarantined rows. This is explicitly not
    /// an actionable pitfall count (old versions could inflate it per log line).
    pub quarantined_hits: u64,
    /// Privacy-safe unknown fingerprints awaiting a precise classifier.
    pub unclassified_candidates: usize,
    /// Independent episodes accumulated across those candidate fingerprints.
    pub unclassified_candidate_hits: u64,
}

/// Render a human-readable overview of the pitfall KB — its self-verification
/// summary plus each recorded pitfall, sorted worst-first (invalidated →
/// corroborated → hypothesis → validated). Used by the TUI `/pitfalls` overlay
/// and any CLI view.
#[must_use]
pub fn pitfall_overview(project_root: &Path) -> String {
    let summary = pitfall_efficacy_summary(project_root);
    let mut pits = read_canonical_pitfalls(project_root);
    let candidates = read_unclassified_candidate_lessons(project_root, 2);
    if pits.is_empty() && candidates.is_empty() {
        let quarantined = if summary.quarantined_records > 0 {
            format!(
                "\n\n已隔离 {} 条历史 generic 记录(旧计数 {} 次,不作为有效踩坑;原始 JSONL 仍保留供审计)。",
                summary.quarantined_records, summary.quarantined_hits
            )
        } else {
            String::new()
        };
        let awaiting = if summary.unclassified_candidates > 0 {
            format!(
                "\n\n另有 {} 条待分类候选(累计 {} 次);达到 2 次独立复现后会在此显示时间证据,但分类前绝不生成修法。",
                summary.unclassified_candidates, summary.unclassified_candidate_hits
            )
        } else {
            String::new()
        };
        return format!(
            "踩坑知识库还是空的。\n\n开发过程中一旦遇到编译/类型/依赖/运行时等可定位报错,\
             UmaDev 会按失败回合去重记录,并在下次遇到同类问题前提醒规避。{quarantined}{awaiting}"
        );
    }

    // Worst first: invalidated advice, independently corroborated risks,
    // hypotheses, then causally validated repairs.
    let rank = |l: &Lesson| match l.pitfall_status() {
        PitfallStatus::Invalidated => 0,
        PitfallStatus::Corroborated => 1,
        PitfallStatus::Hypothesis => 2,
        PitfallStatus::Validated => 3,
    };
    pits.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.hits().cmp(&a.hits()))
            .then_with(|| b.last_observed_at().cmp(a.last_observed_at()))
    });

    let mut out = format!(
        "踩坑知识库 — 可行动 {} 条\n  hypothesis {} · corroborated {} · validated {} · invalidated {}\n  [candidate] 待分类 {} 条/历史命中 {} 次 · [quarantine] 历史 generic {} 条/旧计数 {} 次\n\n",
        summary.total,
        summary.hypothesis,
        summary.corroborated,
        summary.validated,
        summary.invalidated,
        summary.unclassified_candidates,
        summary.unclassified_candidate_hits,
        summary.quarantined_records,
        summary.quarantined_hits,
    );
    for l in &pits {
        let (icon, tag) = match l.pitfall_status() {
            PitfallStatus::Hypothesis => ("[pitfall]", "hypothesis"),
            PitfallStatus::Corroborated => ("[evidence]", "corroborated"),
            PitfallStatus::Validated => ("[ok]", "validated"),
            PitfallStatus::Invalidated => ("[warn]", "invalidated"),
        };
        let ctx = if l.context.is_empty() {
            String::new()
        } else {
            format!("  栈: {}", l.context.join(", "))
        };
        out.push_str(&format!(
            "{icon} {} (历史命中 {} 次 · 可审计证据 {} 条 · {tag})\n  签名: {}{ctx}\n  时间(UTC):\n    首次: {}\n    最近观察: {}\n    最近验证: {}\n  完整时间线: {}\n  原因: {}\n  规避: {}\n\n",
            l.title,
            l.hits(),
            l.knowledge_evidence_count(),
            l.signature,
            l.audited_first_observed_at(),
            l.audited_last_observed_at()
                .unwrap_or("—(旧数据无逐次时间)"),
            l.last_verified_at().unwrap_or("—"),
            if l.timeline_complete() { "是" } else { "否" },
            truncate(&l.root_cause, 160),
            truncate(&l.fix, 240),
        ));
    }
    if !candidates.is_empty() {
        out.push_str("待分类候选（仅隐私指纹与时间证据，不生成/注入修法）\n");
        for candidate in &candidates {
            out.push_str(&format!(
                "[candidate] {} (历史命中 {} 次 · 可审计证据 {} 条 · 需分类审核)\n  指纹: {}\n  时间(UTC):\n    首次: {}\n    最近观察: {}\n\n",
                candidate.title,
                candidate.hits(),
                candidate.knowledge_evidence_count(),
                candidate.signature,
                candidate.audited_first_observed_at(),
                candidate.last_recurred_at().unwrap_or("—"),
            ));
        }
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
    /// Grounded cause recorded for this concrete incident.
    pub root_cause: String,
    /// Tech-stack fingerprint present when it was hit (its trigger context).
    pub context: Vec<String>,
    /// Fixes already tried that still let it recur — what UmaDev now steers AWAY
    /// from. Empty unless the pitfall recurred after a warning.
    pub failed_fixes: Vec<String>,
    /// Immutable first observation time.
    pub first_observed_at: String,
    /// Latest observation time (first observation for a one-off).
    pub last_observed_at: Option<String>,
    /// Latest true recurrence, absent for a one-off/legacy row.
    pub last_recurred_at: Option<String>,
    /// Latest mechanically verified post-fix pass.
    pub last_verified_at: Option<String>,
    /// Number of bounded provenance records currently retained.
    pub recent_evidence_count: usize,
    /// Whether every lifetime hit has a retained episode record.
    pub timeline_complete: bool,
    /// Bounded provenance tail (timestamp + episode id + evidence hash).
    pub recent_observations: Vec<PitfallObservation>,
}

/// Privacy-safe unknown error fingerprint. It carries only count/time/hash
/// evidence and can never be recalled as advice until a classifier identifies
/// a precise actionable root cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnclassifiedCandidateEntry {
    /// Opaque `general/candidate/<hash>` identity (not raw error text).
    pub fingerprint: String,
    /// Independent failure episodes observed.
    pub hits: u32,
    /// First candidate observation.
    pub first_observed_at: String,
    /// Most recent repeated observation.
    pub last_observed_at: Option<String>,
    /// Bounded audit evidence currently retained.
    pub recent_evidence_count: usize,
    /// Whether the bounded timeline covers every lifetime hit.
    pub timeline_complete: bool,
    /// Bounded privacy-safe observation tail; raw error text is never exposed.
    pub recent_observations: Vec<PitfallObservation>,
}

/// One pattern that passed the quality gate — a proven, reusable success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEntry {
    /// Pattern title (e.g. "Validated API contract for blog").
    pub title: String,
    /// Short body excerpt describing what was validated.
    pub summary: String,
}

/// Lifecycle of a reusable, curated rule shown by `/lessons` (distinct from the
/// concrete incident lifecycle shown by `/pitfalls`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CuratedLessonStatus {
    /// One observation or provenance-incomplete legacy aggregate.
    Hypothesis,
    /// Two or more distinct evidence ids support the candidate rule.
    Corroborated,
    /// A complete causal repair success supports the rule.
    Validated,
    /// Contradictory evidence or a newer exact repair failure revoked the rule.
    Invalidated,
}

impl CuratedLessonStatus {
    /// Compatibility alias for the former two-state pending model.
    #[allow(non_upper_case_globals)]
    pub const Pending: Self = Self::Hypothesis;
    /// Compatibility alias for the former repair-revision state.
    #[allow(non_upper_case_globals)]
    pub const NeedsRevision: Self = Self::Invalidated;
}

/// One reusable rule distilled from repeated incidents or the belief ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedLessonEntry {
    /// Human-readable rule title.
    pub title: String,
    /// Actionable recommendation.
    pub rule: String,
    /// Grounded explanation behind the rule.
    pub root_cause: String,
    /// Number of incidents/lessons supporting it.
    pub evidence_count: u32,
    /// Current confidence/lifecycle.
    pub status: CuratedLessonStatus,
    /// `pitfall`, `belief`, or `validated_pattern`.
    pub source_kind: String,
    /// Source pitfall signatures (empty for non-pitfall beliefs/patterns).
    pub source_signatures: Vec<String>,
    /// First evidence time available for this rule.
    pub first_observed_at: String,
    /// Most recent evidence/confirmation time.
    pub last_observed_at: Option<String>,
    /// Latest mechanical verification time, when any.
    pub last_verified_at: Option<String>,
    /// Whether the displayed source times cover every supporting observation.
    /// `false` identifies legacy aggregate rows whose historical per-episode
    /// timestamps were never recorded, so the UI must not present them as a
    /// complete timeline.
    pub timeline_complete: bool,
}

/// A structured, language-neutral view of "what UmaDev has learned" — its
/// self-evolution made visible. Pure read; the CLI / TUI add i18n chrome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LessonsReport {
    /// Pitfall self-verification counters (total / validated / recurring / active).
    pub efficacy: PitfallEfficacySummary,
    /// Complete concrete incident ledger, ordered worst-first.
    pub incidents: Vec<PitfallEntry>,
    /// Reusable rules distilled from repeated pitfalls + folded experience.
    /// This is the primary `/lessons` payload; incidents remain `/pitfalls`.
    pub curated_lessons: Vec<CuratedLessonEntry>,
    /// High-frequency / noteworthy pitfalls, worst-first (recurring → most-hit).
    pub top_pitfalls: Vec<PitfallEntry>,
    /// Currently-avoided failing fixes — pitfalls whose recorded fix proved
    /// insufficient, so the base is now steered toward a different approach.
    pub recurring: Vec<PitfallEntry>,
    /// Repeated unknown fingerprints shown for classification/audit only.
    pub unclassified_candidates: Vec<UnclassifiedCandidateEntry>,
    /// Validated success patterns (passed the quality gate, reusable).
    pub validated_patterns: Vec<ValidatedEntry>,
}

impl LessonsReport {
    /// `true` when nothing has been learned yet (drives the empty state).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.curated_lessons.is_empty()
    }

    /// Whether concrete actionable incidents exist in the separate pitfall
    /// ledger. `/lessons` can truthfully be empty while `/pitfalls` is not.
    #[must_use]
    pub fn has_incidents(&self) -> bool {
        self.efficacy.total > 0
    }

    /// Whether repeated unknown errors have inspectable candidate evidence.
    #[must_use]
    pub fn has_unclassified_candidates(&self) -> bool {
        self.efficacy.unclassified_candidates > 0
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

    let mut pits = read_canonical_pitfalls(project_root);
    // Worst first: invalidated advice, independently corroborated risks,
    // hypotheses, then causally validated repairs — the same ordering the
    // `/pitfalls` overlay uses.
    let rank = |l: &Lesson| match l.pitfall_status() {
        PitfallStatus::Invalidated => 0,
        PitfallStatus::Corroborated => 1,
        PitfallStatus::Hypothesis => 2,
        PitfallStatus::Validated => 3,
    };
    pits.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.hits().cmp(&a.hits()))
            .then_with(|| b.last_observed_at().cmp(a.last_observed_at()))
    });

    let to_entry = |l: &Lesson| PitfallEntry {
        title: l.title.clone(),
        signature: l.signature.clone(),
        hits: l.hits(),
        status: l.pitfall_status(),
        fix: l.fix.clone(),
        root_cause: l.root_cause.clone(),
        context: l.context.clone(),
        failed_fixes: l
            .efficacy
            .as_ref()
            .filter(|e| e.outcome_attribution_version == 1)
            .map(|e| e.failed_fixes.clone())
            .unwrap_or_default(),
        first_observed_at: l.audited_first_observed_at().to_string(),
        last_observed_at: l.audited_last_observed_at().map(str::to_string),
        last_recurred_at: l.last_recurred_at().map(str::to_string),
        last_verified_at: l.last_verified_at().map(str::to_string),
        recent_evidence_count: l.knowledge_evidence_count(),
        timeline_complete: l.timeline_complete(),
        recent_observations: l
            .efficacy
            .as_ref()
            .map(|e| e.recent_observations.clone())
            .unwrap_or_default(),
    };

    let incidents: Vec<PitfallEntry> = pits.iter().map(to_entry).collect();
    let recurring: Vec<PitfallEntry> = incidents
        .iter()
        .filter(|incident| incident.status == PitfallStatus::Invalidated)
        .cloned()
        .collect();
    let top_pitfalls: Vec<PitfallEntry> = incidents.iter().take(LESSONS_TOP_N).cloned().collect();
    let unclassified_candidates: Vec<UnclassifiedCandidateEntry> =
        read_unclassified_candidate_lessons(project_root, 2)
            .into_iter()
            .map(|candidate| UnclassifiedCandidateEntry {
                fingerprint: candidate.signature.clone(),
                hits: candidate.hits(),
                first_observed_at: candidate.audited_first_observed_at().to_string(),
                last_observed_at: candidate.audited_last_observed_at().map(str::to_string),
                recent_evidence_count: candidate.knowledge_evidence_count(),
                timeline_complete: candidate.timeline_complete(),
                recent_observations: candidate
                    .efficacy
                    .as_ref()
                    .map(|efficacy| efficacy.recent_observations.clone())
                    .unwrap_or_default(),
            })
            .collect();

    // `/lessons` is the reusable-rule view, not another incident ledger. A
    // Every precise pitfall is visible with its explicit lifecycle. Aggregate
    // hit counts never become evidence counts and never validate a rule.
    let mut curated_lessons: Vec<CuratedLessonEntry> = pits
        .iter()
        .map(|l| CuratedLessonEntry {
            // Keep the structured report language-neutral. CLI/TUI surfaces add
            // the translated presentation prefix for generated pitfall rules.
            title: l.signature.clone(),
            rule: l.fix.clone(),
            root_cause: l.root_cause.clone(),
            evidence_count: u32::try_from(l.knowledge_evidence_count()).unwrap_or(u32::MAX),
            status: match l.pitfall_status() {
                PitfallStatus::Hypothesis => CuratedLessonStatus::Hypothesis,
                PitfallStatus::Corroborated => CuratedLessonStatus::Corroborated,
                PitfallStatus::Validated => CuratedLessonStatus::Validated,
                PitfallStatus::Invalidated => CuratedLessonStatus::Invalidated,
            },
            source_kind: "pitfall".to_string(),
            source_signatures: vec![l.signature.clone()],
            first_observed_at: l.audited_first_observed_at().to_string(),
            last_observed_at: l.audited_last_observed_at().map(str::to_string),
            last_verified_at: l.last_verified_at().map(str::to_string),
            timeline_complete: l.timeline_complete(),
        })
        .collect();

    // Merge deterministic folded beliefs. Similarity only groups the candidate;
    // lifecycle and counts come exclusively from retained typed evidence.
    let belief_rows: Vec<Lesson> = read_raw_lessons(project_root, BELIEFS_FILE)
        .into_iter()
        .filter(|l| l.kind == LessonKind::Belief)
        .collect();
    let covered_evidence: std::collections::HashSet<String> = belief_rows
        .iter()
        .filter(|belief| !belief.invalidated)
        .flat_map(|belief| belief.evidence.iter().cloned())
        .collect();
    curated_lessons.extend(belief_rows.into_iter().map(|l| {
        let evidence_count = u32::try_from(l.knowledge_evidence_count()).unwrap_or(u32::MAX);
        let status = match l.pitfall_status() {
            PitfallStatus::Hypothesis => CuratedLessonStatus::Hypothesis,
            PitfallStatus::Corroborated => CuratedLessonStatus::Corroborated,
            PitfallStatus::Validated => CuratedLessonStatus::Validated,
            PitfallStatus::Invalidated => CuratedLessonStatus::Invalidated,
        };
        let first_observed_at = l.audited_first_observed_at().to_string();
        let last_observed_at = l.audited_last_observed_at().map(str::to_string);
        let last_verified_at = l.last_verified_at().map(str::to_string);
        let timeline_complete = l.timeline_complete();
        CuratedLessonEntry {
            title: l.title,
            rule: l.fix,
            root_cause: l.root_cause,
            evidence_count,
            status,
            source_kind: "belief".to_string(),
            source_signatures: Vec::new(),
            // A belief's `first_seen` field historically means latest
            // confirmation, not the first evidence in its cluster. Do
            // not relabel it as a precise first-observation timestamp.
            first_observed_at,
            last_observed_at,
            last_verified_at,
            timeline_complete,
        }
    }));

    // Source-verified patterns remain hypotheses until a causally attributable
    // repair success exists; a green build alone is not proof of this advice.
    let validated_rows: Vec<Lesson> = read_raw_lessons(project_root, "validated-decisions.jsonl")
        .into_iter()
        .filter(|l| l.kind == LessonKind::ValidatedPattern)
        .filter(|l| {
            let legacy_key = format!("{}\u{0}{}", l.domain, l.title);
            !covered_evidence.contains(&evidence_key(l)) && !covered_evidence.contains(&legacy_key)
        })
        .collect();
    for l in &validated_rows {
        curated_lessons.push(CuratedLessonEntry {
            title: l.title.clone(),
            rule: l.fix.clone(),
            root_cause: l.root_cause.clone(),
            evidence_count: u32::try_from(l.knowledge_evidence_count()).unwrap_or(u32::MAX),
            status: match l.pitfall_status() {
                PitfallStatus::Hypothesis => CuratedLessonStatus::Hypothesis,
                PitfallStatus::Corroborated => CuratedLessonStatus::Corroborated,
                PitfallStatus::Validated => CuratedLessonStatus::Validated,
                PitfallStatus::Invalidated => CuratedLessonStatus::Invalidated,
            },
            source_kind: "validated_pattern".to_string(),
            source_signatures: Vec::new(),
            first_observed_at: l.audited_first_observed_at().to_string(),
            last_observed_at: l.audited_last_observed_at().map(str::to_string),
            last_verified_at: l.last_verified_at().map(str::to_string),
            timeline_complete: l.timeline_complete(),
        });
    }
    let mut validated_patterns: Vec<ValidatedEntry> = validated_rows
        .into_iter()
        .filter(|lesson| lesson.pitfall_status() == PitfallStatus::Validated)
        .map(|l| ValidatedEntry {
            title: l.title,
            summary: truncate(l.body.lines().next().unwrap_or("").trim(), 160),
        })
        .collect();
    validated_patterns.sort_by(|a, b| a.title.cmp(&b.title).then(a.summary.cmp(&b.summary)));
    validated_patterns.dedup();
    validated_patterns.truncate(LESSONS_TOP_N);

    curated_lessons = canonicalize_curated_lessons(curated_lessons);

    let curated_rank = |status: CuratedLessonStatus| match status {
        CuratedLessonStatus::Invalidated => 0,
        CuratedLessonStatus::Validated => 1,
        CuratedLessonStatus::Corroborated => 2,
        CuratedLessonStatus::Hypothesis => 3,
    };
    curated_lessons.sort_by(|a, b| {
        curated_rank(a.status)
            .cmp(&curated_rank(b.status))
            .then_with(|| b.evidence_count.cmp(&a.evidence_count))
            .then_with(|| b.last_observed_at.cmp(&a.last_observed_at))
    });

    LessonsReport {
        efficacy,
        incidents,
        curated_lessons,
        top_pitfalls,
        recurring,
        unclassified_candidates,
        validated_patterns,
    }
}

fn canonicalize_curated_lessons(entries: Vec<CuratedLessonEntry>) -> Vec<CuratedLessonEntry> {
    let mut canonical = Vec::<CuratedLessonEntry>::new();
    let mut index = std::collections::HashMap::<(String, String, String, String), usize>::new();
    for entry in entries {
        let key = (
            entry.source_kind.clone(),
            entry.title.clone(),
            entry.rule.clone(),
            entry.root_cause.clone(),
        );
        if let Some(&position) = index.get(&key) {
            let dst = &mut canonical[position];
            dst.evidence_count = dst.evidence_count.saturating_add(entry.evidence_count);
            if dst.first_observed_at.is_empty()
                || (!entry.first_observed_at.is_empty()
                    && entry.first_observed_at < dst.first_observed_at)
            {
                dst.first_observed_at = entry.first_observed_at;
            }
            dst.last_observed_at =
                latest_optional_time(dst.last_observed_at.take(), entry.last_observed_at);
            dst.last_verified_at =
                latest_optional_time(dst.last_verified_at.take(), entry.last_verified_at);
            dst.timeline_complete &= entry.timeline_complete;
            merge_tokens(&mut dst.source_signatures, &entry.source_signatures, 32);
            dst.status = stronger_curated_status(dst.status, entry.status);
        } else {
            index.insert(key, canonical.len());
            canonical.push(entry);
        }
    }
    canonical
}

fn latest_optional_time(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn stronger_curated_status(
    left: CuratedLessonStatus,
    right: CuratedLessonStatus,
) -> CuratedLessonStatus {
    use CuratedLessonStatus::{Corroborated, Hypothesis, Invalidated, Validated};
    match (left, right) {
        (Invalidated, _) | (_, Invalidated) => Invalidated,
        (Validated, _) | (_, Validated) => Validated,
        (Corroborated, _) | (_, Corroborated) => Corroborated,
        (Hypothesis, Hypothesis) => Hypothesis,
    }
}

/// Compute the pitfall efficacy summary for `umadev report` / `/pitfalls`.
#[must_use]
pub fn pitfall_efficacy_summary(project_root: &Path) -> PitfallEfficacySummary {
    let mut s = PitfallEfficacySummary::default();
    for l in read_raw_lessons(project_root, DEV_ERRORS_FILE) {
        if l.kind != LessonKind::DevError || l.invalidated {
            continue;
        }
        if !l.is_recognized() {
            if l.signature.starts_with("general/candidate/") {
                s.unclassified_candidates += 1;
                s.unclassified_candidate_hits = s
                    .unclassified_candidate_hits
                    .saturating_add(u64::from(l.hits()));
            } else {
                s.quarantined_records += 1;
                s.quarantined_hits = s.quarantined_hits.saturating_add(u64::from(l.hits()));
            }
        }
    }
    for l in read_canonical_pitfalls(project_root) {
        s.total += 1;
        match l.pitfall_status() {
            PitfallStatus::Validated => s.validated += 1,
            PitfallStatus::Invalidated => {
                s.invalidated += 1;
                s.recurring += 1;
            }
            PitfallStatus::Corroborated => {
                s.corroborated += 1;
                s.active += 1;
            }
            PitfallStatus::Hypothesis => {
                s.hypothesis += 1;
                s.active += 1;
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolate $HOME (hence `global_learned_dir()`) to a throwaway temp dir, so a real
    /// sediment/promotion can't READ or POLLUTE the developer's actual ~/.umadev/learned.
    ///
    /// This used to own a private `HOME_ENV_LOCK`. It does not any more, and must not
    /// again: `test_support` guards the same process-global `HOME` for the
    /// knowledge-corpus tests, and two mutexes over one global is no mutex at all — this
    /// guard's `Drop` would restore the real `HOME` underneath a `NoBundledCorpus` holder
    /// and leak the developer's staged `~/.umadev/knowledge` into tests that assert no
    /// corpus is reachable. Both now serialise on the single shared lock.
    use crate::test_support::{NoBundledCorpus, TempHome};

    use crate::phases::QualityCheck;
    use tempfile::TempDir;

    #[test]
    fn persisted_privacy_fingerprints_are_keyed_not_dictionary_testable_sha256() {
        use sha2::{Digest as _, Sha256};

        let _home = TempHome::new();
        let domain = "umadev:pitfall-requirement:v1";
        let requirement = "fix login";
        let first = privacy_fingerprint(domain, requirement);
        let second = privacy_fingerprint(domain, requirement);
        assert_eq!(first, second, "an installation keeps cross-run identity");

        let mut old = Sha256::new();
        old.update(domain.as_bytes());
        old.update([0]);
        old.update(requirement.as_bytes());
        assert_ne!(
            first,
            format!("{:x}", old.finalize()),
            "the prior unkeyed digest allowed offline requirement enumeration"
        );
    }

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

    fn test_evidence(
        id: &str,
        observed_at: &str,
        outcome: KnowledgeEvidenceOutcome,
    ) -> PitfallObservation {
        PitfallObservation {
            observed_at: observed_at.to_string(),
            episode_id: id.to_string(),
            evidence_hash: format!("hash-{id}"),
            source: "test-mechanical-verifier".to_string(),
            base: "test-base".to_string(),
            base_version: "1.2.3".to_string(),
            workspace_scope: "project:test".to_string(),
            outcome,
            causal_attempt_id: matches!(
                outcome,
                KnowledgeEvidenceOutcome::FixSucceeded | KnowledgeEvidenceOutcome::FixFailed
            )
            .then(|| format!("attempt-{id}"))
            .unwrap_or_default(),
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
    fn lesson_recall_policy_never_blocks_settlement_trust_or_reports() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        assert_eq!(
            capture_dev_errors(
                tmp.path(),
                std::slice::from_ref(&error),
                "demo",
                "dependency lodash"
            ),
            1
        );
        let token = commit_pitfall_fix_attempt(tmp.path(), &error).unwrap();
        crate::memory_control::update_recall(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Pitfalls),
            false,
        )
        .unwrap();
        assert!(lessons_for_error(tmp.path(), &error).is_empty());
        assert!(recurring_pitfall_for_error(tmp.path(), &error).is_none());

        assert_eq!(
            settle_pitfall_fix_attempt(tmp.path(), &token, PitfallFixAttemptResult::Passed),
            PitfallFixSettlement::Passed,
            "a receipt sent before recall-off still settles"
        );
        assert_eq!(
            apply_dev_error_trust(tmp.path(), std::slice::from_ref(&error), true),
            1,
            "trust bookkeeping is not prompt recall"
        );
        assert_eq!(lessons_report(tmp.path()).efficacy.total, 1);

        let older = mk_db_lesson(
            "indexing add",
            "always add a btree index on every filtered column for speed",
            "missing indexes slowed reads across the board significantly",
            "2026-06-01T00:00:00Z",
        );
        let newer = mk_db_lesson(
            "indexing drop",
            "avoid over indexing and drop redundant indexes to keep writes fast",
            "too many indexes bloated writes and storage badly here",
            "2026-06-20T00:00:00Z",
        );
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &[older, newer]);
        std::fs::write(
            tmp.path().join(".umadev/memory/policy.toml"),
            "invalid = [toml",
        )
        .unwrap();
        assert_eq!(
            scan_contradictions(tmp.path()),
            0,
            "a malformed policy is privacy-conservative and authorises no leaf rewrite"
        );
        assert!(relevant_lessons_for_prompt(tmp.path(), "database index").is_empty());
        let report = lessons_report(tmp.path());
        assert!(
            !report.is_empty(),
            "corrupt policy hides no report/inventory data"
        );
        assert_eq!(
            capture_dev_errors(
                tmp.path(),
                std::slice::from_ref(&error),
                "demo",
                "dependency lodash"
            ),
            0,
            "malformed policy disables new capture"
        );
    }

    #[test]
    fn lesson_prompt_recall_is_leaf_scoped_including_reflected_strategies() {
        let quality = TempDir::new().unwrap();
        capture_quality_failures(
            quality.path(),
            &[check("OpenAPI contract", "failed", 20)],
            "demo",
            "api contract openapi",
        );
        assert!(!relevant_lessons_for_prompt(quality.path(), "api contract openapi").is_empty());
        crate::memory_control::update_recall(
            quality.path(),
            MemoryScope::Project,
            Some(MemoryStore::QualityFailures),
            false,
        )
        .unwrap();
        assert!(relevant_lessons_for_prompt(quality.path(), "api contract openapi").is_empty());
        assert_eq!(read_all_raw_lessons(quality.path()).len(), 1);
        crate::memory_control::update_recall(
            quality.path(),
            MemoryScope::Project,
            Some(MemoryStore::QualityFailures),
            true,
        )
        .unwrap();
        assert!(!relevant_lessons_for_prompt(quality.path(), "api contract openapi").is_empty());

        let pitfalls = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        capture_dev_errors(
            pitfalls.path(),
            std::slice::from_ref(&error),
            "demo",
            "dependency lodash",
        );
        let attempt = commit_pitfall_fix_attempt(pitfalls.path(), &error).unwrap();
        capture_dev_errors(
            pitfalls.path(),
            std::slice::from_ref(&error),
            "demo",
            "dependency lodash",
        );
        assert_eq!(
            settle_pitfall_fix_attempt(
                pitfalls.path(),
                &attempt,
                PitfallFixAttemptResult::VerificationFailed(error.clone()),
            ),
            PitfallFixSettlement::SameSignatureFailed,
        );
        let strategy = "pin lodash and regenerate the lockfile from a clean install";
        assert!(record_pitfall_strategy(
            pitfalls.path(),
            "dependency/module-not-found/lodash",
            strategy,
        ));
        assert!(lessons_for_error(pitfalls.path(), &error).contains(strategy));
        crate::memory_control::update_capture(
            pitfalls.path(),
            MemoryScope::Project,
            Some(MemoryStore::PitfallReflections),
            false,
        )
        .unwrap();
        assert!(
            recurring_pitfall_for_error(pitfalls.path(), &error).is_none(),
            "capture-off prevents the optional reflection base consult",
        );
        assert!(
            lessons_for_error(pitfalls.path(), &error).contains(strategy),
            "capture and recall are independent",
        );
        crate::memory_control::update_capture(
            pitfalls.path(),
            MemoryScope::Project,
            Some(MemoryStore::PitfallReflections),
            true,
        )
        .unwrap();
        crate::memory_control::update_recall(
            pitfalls.path(),
            MemoryScope::Project,
            Some(MemoryStore::PitfallReflections),
            false,
        )
        .unwrap();
        let without_reflection = lessons_for_error(pitfalls.path(), &error);
        assert!(
            !without_reflection.is_empty(),
            "pitfall recall remains enabled"
        );
        assert!(!without_reflection.contains(strategy));
        crate::memory_control::update_recall(
            pitfalls.path(),
            MemoryScope::Project,
            Some(MemoryStore::PitfallReflections),
            true,
        )
        .unwrap();
        assert!(lessons_for_error(pitfalls.path(), &error).contains(strategy));
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

        assert!(relevant_lessons_for_prompt(tmp.path(), "完全无关的需求文本").is_empty());
        let recall = relevant_lessons_for_prompt(tmp.path(), "修复 react-router-dom 路由");
        assert!(
            recall.contains("[pitfall]"),
            "recall must surface pitfalls: {recall}"
        );
        assert!(recall.contains("规避"));
        assert!(recall.contains("已踩 2 次"), "frequency shown: {recall}");
    }

    #[test]
    fn concurrent_dev_error_mutators_do_not_lose_each_others_update() {
        // CONFIRMED MED lost-update race: capture_dev_errors and
        // record_pitfall_strategy each read the whole dev-errors store, mutate a
        // DIFFERENT record, then rewrite the WHOLE store. Run concurrently
        // WITHOUT a shared lock, the later writer's stale snapshot clobbers the
        // earlier writer's record -> a silently dropped pitfall / strategy. The
        // cross-process pitfalls-store lock around every read-modify-write must
        // make the two functions mutually exclude so BOTH mutations survive.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // Pre-seed a SECOND, distinct pitfall that record_pitfall_strategy will
        // mutate (it only writes when it finds a matching signature). Thread A
        // captures react-router; Thread B sets this record's next_strategy.
        let strat_sig = "build/type-error/seed-record";
        let seed = Lesson {
            kind: LessonKind::DevError,
            domain: "build".into(),
            title: format!("踩坑 [{strat_sig}]"),
            body: String::new(),
            fix: "fix the type".into(),
            root_cause: "type mismatch".into(),
            keywords: vec!["type".into()],
            source_requirement: "r".into(),
            first_seen: "2026-06-21T00:00:00Z".into(),
            signature: strat_sig.into(),
            occurrences: 1,
            context: vec!["ts".into()],
            efficacy: None,
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        };
        write_raw_lessons(&root, DEV_ERRORS_FILE, std::slice::from_ref(&seed));

        const ITERS: usize = 200;
        let capture_err = "Error: Cannot find module 'react-router-dom'".to_string();
        let capture_sig = "dependency/module-not-found/react-router-dom";

        std::thread::scope(|s| {
            // Thread A: capture the SAME error ITERS times. The first creates the
            // record (occurrences=1); each recurrence bumps occurrences by 1, so a
            // race-free run ends at exactly ITERS.
            let root_a = root.clone();
            let err_a = capture_err.clone();
            s.spawn(move || {
                for _ in 0..ITERS {
                    let _ =
                        capture_dev_errors(&root_a, std::slice::from_ref(&err_a), "demo", "需求");
                }
            });
            // Thread B: set the seed record's next_strategy ITERS times. Each call
            // is a full read-modify-write of the SAME file.
            let root_b = root.clone();
            s.spawn(move || {
                for _ in 0..ITERS {
                    let _ = record_pitfall_strategy(&root_b, strat_sig, "winning-strategy");
                }
            });
        });

        let store = read_raw_lessons(&root, DEV_ERRORS_FILE);

        // Thread A's update survived in full: every increment is accounted for.
        let captured = store
            .iter()
            .find(|l| l.signature == capture_sig)
            .expect("captured pitfall present (thread A's record not clobbered)");
        assert_eq!(
            captured.hits(),
            ITERS as u32,
            "no capture increment was lost to a clobbering write"
        );

        // Thread B's update survived: the seed record still carries its strategy
        // (a lost-update would have reverted next_strategy to empty / None).
        let seeded = store
            .iter()
            .find(|l| l.signature == strat_sig)
            .expect("seed pitfall present (thread B's record not clobbered)");
        assert_eq!(
            seeded.efficacy.as_ref().map(|e| e.next_strategy.as_str()),
            Some("winning-strategy"),
            "thread B's strategy write was not lost to a clobbering capture write"
        );
    }

    #[test]
    fn cross_process_lesson_writer_child() {
        let Some(root) = std::env::var_os("UMADEV_LESSONS_MP_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let privacy_home = std::env::var_os("UMADEV_LESSONS_MP_HOME")
            .map(PathBuf::from)
            .expect("parent must provide one shared privacy home");
        set_test_home_override(Some(privacy_home));
        let index = std::env::var("UMADEV_LESSONS_MP_INDEX").unwrap();
        fs::write(root.join(format!("ready-{index}")), b"").unwrap();
        let started = std::time::Instant::now();
        while !root.join("start").exists() && started.elapsed() < std::time::Duration::from_secs(10)
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            root.join("start").exists(),
            "parent did not release writer gate"
        );

        match std::env::var("UMADEV_LESSONS_MP_MODE").as_deref() {
            Ok("capture") => {
                let name = std::env::var("UMADEV_LESSONS_MP_NAME").unwrap();
                let error = format!("Error: Cannot find module 'umadev-cross-{name}'");
                let outcome = capture_dev_errors_detailed(&root, &[error], "mp", "mp");
                fs::write(
                    root.join(format!("result-{index}")),
                    outcome.observations.to_string(),
                )
                .unwrap();
            }
            Ok("same-evidence") => {
                let error = "Error: Cannot find module 'umadev-shared-evidence'".to_string();
                let outcome = capture_dev_errors_detailed_with_evidence_id(
                    &root,
                    &[error],
                    "mp",
                    "mp",
                    "shared-cross-process-evidence",
                );
                fs::write(
                    root.join(format!("result-{index}")),
                    outcome.observations.to_string(),
                )
                .unwrap();
            }
            Ok("settle") => {
                let token = std::env::var("UMADEV_LESSONS_MP_TOKEN").unwrap();
                let settlement =
                    settle_pitfall_fix_attempt(&root, &token, PitfallFixAttemptResult::Passed);
                fs::write(
                    root.join(format!("result-{index}")),
                    format!("{settlement:?}"),
                )
                .unwrap();
            }
            mode => panic!("unexpected child mode: {mode:?}"),
        }
    }

    fn spawn_lesson_children(
        root: &Path,
        mode: &str,
        token: Option<&str>,
    ) -> Vec<std::process::Child> {
        const NAMES: [&str; 8] = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        ];
        let exe = std::env::current_exe().unwrap();
        let privacy_home = root.join("privacy-home");
        fs::create_dir_all(&privacy_home).unwrap();
        let mut children = Vec::new();
        for (index, name) in NAMES.iter().enumerate() {
            let mut command = std::process::Command::new(&exe);
            command
                .args([
                    "--exact",
                    "lessons::tests::cross_process_lesson_writer_child",
                    "--nocapture",
                ])
                .env("UMADEV_LESSONS_MP_ROOT", root)
                .env("UMADEV_LESSONS_MP_HOME", &privacy_home)
                .env("UMADEV_LESSONS_MP_MODE", mode)
                .env("UMADEV_LESSONS_MP_INDEX", index.to_string())
                .env("UMADEV_LESSONS_MP_NAME", name);
            if let Some(token) = token {
                command.env("UMADEV_LESSONS_MP_TOKEN", token);
            }
            children.push(command.spawn().unwrap());
        }
        let started = std::time::Instant::now();
        loop {
            let ready = (0..NAMES.len())
                .filter(|index| root.join(format!("ready-{index}")).exists())
                .count();
            if ready == NAMES.len() || started.elapsed() >= std::time::Duration::from_secs(10) {
                fs::write(root.join("start"), b"").unwrap();
                assert_eq!(
                    ready,
                    NAMES.len(),
                    "not every writer reached the start gate"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        children
    }

    #[test]
    fn eight_process_captures_have_zero_lost_updates() {
        let temp = TempDir::new().unwrap();
        let children = spawn_lesson_children(temp.path(), "capture", None);
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        for index in 0..8 {
            assert_eq!(
                fs::read_to_string(temp.path().join(format!("result-{index}"))).unwrap(),
                "1"
            );
        }
        let rows = read_raw_lessons(temp.path(), DEV_ERRORS_FILE);
        let signatures: std::collections::HashSet<_> = rows
            .iter()
            .filter(|row| {
                row.signature
                    .starts_with("dependency/module-not-found/umadev-cross-")
            })
            .map(|row| row.signature.as_str())
            .collect();
        assert_eq!(signatures.len(), 8, "one or more process updates were lost");
    }

    #[test]
    fn eight_process_replay_of_one_evidence_id_is_exactly_once() {
        let temp = TempDir::new().unwrap();
        let children = spawn_lesson_children(temp.path(), "same-evidence", None);
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let committed: usize = (0..8)
            .map(|index| {
                fs::read_to_string(temp.path().join(format!("result-{index}")))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .sum();
        assert_eq!(
            committed, 1,
            "a replayed evidence id committed more than once"
        );
        let row = read_raw_lessons(temp.path(), DEV_ERRORS_FILE).remove(0);
        assert_eq!(row.hits(), 1);
        assert_eq!(row.knowledge_evidence_count(), 1);
        assert_eq!(row.pitfall_status(), PitfallStatus::Hypothesis);
    }

    #[test]
    fn eight_process_settlement_is_exactly_once() {
        let temp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'";
        assert_eq!(
            capture_dev_errors(temp.path(), &[error.to_string()], "mp", "mp"),
            1
        );
        let token = commit_pitfall_fix_attempt(temp.path(), error).unwrap();
        let children = spawn_lesson_children(temp.path(), "settle", Some(&token));
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let results: Vec<String> = (0..8)
            .map(|index| fs::read_to_string(temp.path().join(format!("result-{index}"))).unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|result| *result == "Passed").count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| *result == "NotFound")
                .count(),
            7
        );
        let row = read_raw_lessons(temp.path(), DEV_ERRORS_FILE).remove(0);
        let efficacy = row.efficacy.unwrap();
        assert!(efficacy.pending_fix_attempts.is_empty());
        assert_eq!(efficacy.exact_helpful, 1);
    }

    #[test]
    fn hostile_raw_ledgers_are_bounded_and_never_overwritten() {
        let cases = [
            vec![b'x'; MAX_RAW_LINE_BYTES + 1],
            "{}\n".repeat(MAX_RAW_LEDGER_LINES + 1).into_bytes(),
            vec![b'x'; usize::try_from(MAX_RAW_LEDGER_BYTES).unwrap() + 1],
        ];
        for bytes in cases {
            let temp = TempDir::new().unwrap();
            let raw = ensure_raw_lessons_dir(temp.path()).unwrap();
            let path = raw.join(DEV_ERRORS_FILE);
            fs::write(&path, &bytes).unwrap();
            assert!(read_raw_lessons(temp.path(), DEV_ERRORS_FILE).is_empty());
            let outcome = capture_dev_errors_detailed(
                temp.path(),
                &["Error: Cannot find module 'bounded-ledger'".to_string()],
                "bounds",
                "bounds",
            );
            assert_eq!(outcome, PitfallCaptureOutcome::default());
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "hostile input was replaced"
            );
        }
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

        // 2. Passive prompt assembly is pure: it is not proof the warning was
        // sent, so it cannot mutate lifecycle state.
        let _ = relevant_lessons_for_prompt(tmp.path(), "无关需求一");
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Active));

        // 3. A second independent episode corroborates, but cannot validate.
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Corroborated));

        // 4. Failure-time retrieval is pure too; only an actually run repair
        // turn commits the attempt. The next independent episode then proves
        // the warning/fix did not hold.
        assert!(!lessons_for_error(tmp.path(), &err[0]).is_empty());
        let failed_attempt = commit_pitfall_fix_attempt(tmp.path(), &err[0]).unwrap();
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &failed_attempt,
                PitfallFixAttemptResult::VerificationFailed(err[0].clone())
            ),
            PitfallFixSettlement::SameSignatureFailed
        );
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &failed_attempt,
                PitfallFixAttemptResult::VerificationFailed(err[0].clone())
            ),
            PitfallFixSettlement::NotFound
        );
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Recurring));
        assert_eq!(pitfall_efficacy_summary(tmp.path()).recurring, 1);

        // 5. A failed repair invalidates the old advice. Broad, fuzzy recall must
        //    therefore abstain instead of reinjecting a known-bad fix. Exact
        //    failure-time recall may still surface the failure as negative evidence
        //    so the next repair can deliberately choose a different strategy.
        let recall = relevant_lessons_for_prompt(tmp.path(), "修复 lodash 依赖");
        assert!(
            recall.is_empty(),
            "invalidated advice leaked into broad recall"
        );
        let exact_recall = lessons_for_error(tmp.path(), &err[0]);
        assert!(
            exact_recall.contains("上次已警示但仍复发"),
            "exact negative evidence must remain auditable: {exact_recall}"
        );
        assert_eq!(
            status(tmp.path()),
            Some(PitfallStatus::Recurring),
            "a passive recall must NOT reset the escalation flag"
        );

        // 6. Retrieval and commit preserve the last settled verdict. ONLY a
        // mechanical pass for this exact token marks Validated.
        let _ = lessons_for_error(tmp.path(), &err[0]);
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Recurring));
        let passed_attempt = commit_pitfall_fix_attempt(tmp.path(), &err[0]).unwrap();
        assert_eq!(status(tmp.path()), Some(PitfallStatus::Recurring));
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &passed_attempt,
                PitfallFixAttemptResult::Passed
            ),
            PitfallFixSettlement::Passed
        );
        assert_eq!(
            status(tmp.path()),
            Some(PitfallStatus::Validated),
            "only a mechanically passing post-fix check validates the fix"
        );
        let s = pitfall_efficacy_summary(tmp.path());
        assert_eq!(s.total, 1);
        assert_eq!(s.validated, 1);
        assert_eq!(s.recurring, 0);
    }

    #[test]
    fn passive_recall_preserves_escalation_flag() {
        // Item #2: a passive `relevant_lessons_for_prompt` recall must leave an
        // existing `recurred_after_warning` flag intact (read-only on the
        // escalation bit), whereas the OLD behaviour silently cleared it.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let recurring = Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: "踩坑 [dependency/module-not-found/lodash]".into(),
            body: String::new(),
            fix: "npm install lodash".into(),
            root_cause: "missing dependency".into(),
            keywords: vec!["lodash".into()],
            source_requirement: "r".into(),
            first_seen: "2026-06-21T00:00:00Z".into(),
            signature: "dependency/module-not-found/lodash".into(),
            occurrences: 3,
            context: vec!["lodash".into()],
            efficacy: Some(PitfallEfficacy {
                injected: 2,
                occ_at_injection: 2,
                recurred_after_warning: true,
                proven_fix: false,
                failed_fixes: vec!["npm install lodash".into()],
                next_strategy: String::new(),
                helpful: 0,
                harmful: 0,
                ..PitfallEfficacy::default()
            }),
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        };
        write_raw_lessons(root, DEV_ERRORS_FILE, std::slice::from_ref(&recurring));

        // A purely passive surfacing (no failing error in hand).
        let _ = relevant_lessons_for_prompt(root, "完全无关的需求");

        let after = read_raw_lessons(root, DEV_ERRORS_FILE)
            .into_iter()
            .find(|l| l.signature == "dependency/module-not-found/lodash")
            .and_then(|l| l.efficacy)
            .expect("pitfall survives");
        assert!(
            after.recurred_after_warning,
            "passive recall must NOT clear the escalation flag"
        );
        assert_eq!(
            after.injected, 2,
            "passive recall has no efficacy side effect"
        );
    }

    #[test]
    fn family_match_does_not_inject_other_root_causes_strategy() {
        // Stored advice is discriminator-specific. A family neighbour must not
        // leak any root/fix/strategy into a different module's error.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let neighbour = Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: "踩坑 [dependency/module-not-found/react-router-dom]".into(),
            body: String::new(),
            fix: "install react-router-dom".into(),
            root_cause: "router dep missing".into(),
            keywords: vec!["react-router-dom".into()],
            source_requirement: "r".into(),
            first_seen: "2026-06-21T00:00:00Z".into(),
            signature: "dependency/module-not-found/react-router-dom".into(),
            occurrences: 5,
            context: vec!["react-router-dom".into()],
            efficacy: Some(PitfallEfficacy {
                injected: 2,
                occ_at_injection: 2,
                recurred_after_warning: true, // Recurring → would surface strategy
                proven_fix: false,
                failed_fixes: vec!["delete node_modules and reinstall".into()],
                next_strategy: "Pin react-router-dom to v6 and migrate the data router APIs."
                    .into(),
                helpful: 0,
                harmful: 0,
                ..PitfallEfficacy::default()
            }),
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        };
        write_raw_lessons(root, DEV_ERRORS_FILE, std::slice::from_ref(&neighbour));

        // A DIFFERENT module's error in the SAME family.
        let recall = lessons_for_error(root, "Error: Cannot find module 'lodash'");
        assert!(
            recall.is_empty(),
            "a family neighbour must abstain from all stored advice: {recall}"
        );

        // Sanity: an EXACT-signature match DOES surface its own strategy.
        write_raw_lessons(
            root,
            DEV_ERRORS_FILE,
            std::slice::from_ref(&Lesson {
                title: "踩坑 [dependency/module-not-found/lodash]".into(),
                fix: "install lodash".into(),
                root_cause: "lodash missing".into(),
                keywords: vec!["lodash".into()],
                signature: "dependency/module-not-found/lodash".into(),
                context: vec!["lodash".into()],
                efficacy: Some(PitfallEfficacy {
                    outcome_attribution_version: 1,
                    injected: 2,
                    occ_at_injection: 2,
                    recurred_after_warning: true,
                    exact_last_fix_failed_at: "2026-06-22T00:00:00Z".into(),
                    proven_fix: false,
                    failed_fixes: vec![],
                    next_strategy: "Add lodash to package.json and run a clean install.".into(),
                    helpful: 0,
                    harmful: 0,
                    recent_observations: vec![test_evidence(
                        "lodash-failed",
                        "2026-06-22T00:00:00Z",
                        KnowledgeEvidenceOutcome::FixFailed,
                    )],
                    ..PitfallEfficacy::default()
                }),
                ..neighbour.clone()
            }),
        );
        let exact = lessons_for_error(root, "Error: Cannot find module 'lodash'");
        assert!(
            exact.contains("Add lodash to package.json"),
            "an exact-signature match still surfaces its own strategy: {exact}"
        );
    }

    #[test]
    fn global_projection_collapses_private_discriminators_to_one_safe_family() {
        let mut a = mk_pitfall(
            "dependency/module-not-found/acme-private-a",
            "install dependency",
            "dependency missing",
            "2026-01-01T00:00:00Z",
            2,
        );
        a.root_cause = "customer-secret-root-cause".into();
        a.fix = "run private-internal-fix-command".into();
        a.domain = "../../customer-secret-domain".into();
        a.first_seen = "customer-secret-date".into();
        let b = mk_pitfall(
            "dependency/module-not-found/customer-secret-b",
            "install dependency",
            "dependency missing",
            "2026-01-02T00:00:00Z",
            2,
        );
        let safe_a = global_safe_dev_error_lesson(&a).unwrap();
        let safe_b = global_safe_dev_error_lesson(&b).unwrap();
        assert_eq!(safe_a.signature, "dependency/module-not-found");
        assert_eq!(safe_a.title, safe_b.title);
        assert!(!safe_a.body.contains("acme"));
        assert!(!safe_b.body.contains("customer"));
        assert!(!safe_a.root_cause.contains("customer-secret"));
        assert!(!safe_a.fix.contains("private-internal"));
        assert_eq!(safe_a.domain, "dependency");
        assert!(!safe_a.first_seen.contains("customer-secret"));
        let key_a = format!("{}::{}", safe_a.domain, safe_a.title);
        let key_b = format!("{}::{}", safe_b.domain, safe_b.title);
        assert_eq!(stable_str_hash(&key_a), stable_str_hash(&key_b));
    }

    #[test]
    fn sediment_clears_stale_orphans_and_is_idempotent() {
        // Item #4 (local path): re-sediment must not leave ghost `lesson-*.md`
        // files for lessons that vanished, and re-running over the same ledger is
        // idempotent (same files, no accumulation).
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        capture_quality_failures(
            root,
            &[check("API URL consistency", "failed", 30)],
            "demo",
            "博客 api",
        );
        let first = sediment_lessons(root);
        assert_eq!(first, 1);
        let api_dir = root.join(".umadev/learned/api");
        let count_md = |d: &std::path::Path| {
            std::fs::read_dir(d)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
                        .count()
                })
                .unwrap_or(0)
        };
        let after_first = count_md(&api_dir);

        // Plant a stale orphan as if a previous run's non-deterministic seq left
        // it behind, then re-sediment: the orphan must be swept, count unchanged.
        std::fs::write(api_dir.join("lesson-api-stale.md"), "ghost").unwrap();
        let second = sediment_lessons(root);
        assert_eq!(second, 1, "re-sediment writes the same single lesson");
        assert_eq!(
            count_md(&api_dir),
            after_first,
            "stale orphan swept; no accumulation across runs"
        );
        assert!(
            !api_dir.join("lesson-api-stale.md").is_file(),
            "the planted orphan was cleared"
        );
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

        // 1. First sighting + pure retrieval, then a confirmed repair turn.
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        let _ = lessons_for_error(tmp.path(), &err[0]);
        let attempt = commit_pitfall_fix_attempt(tmp.path(), &err[0]).unwrap();
        assert!(
            failed_fix(tmp.path()).is_empty(),
            "no failed fix recorded yet"
        );

        // 2. It recurs DESPITE the warning → the recorded fix is logged as a
        //    tried-and-failed approach in the failed-fix ledger.
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &attempt,
                PitfallFixAttemptResult::VerificationFailed(err[0].clone()),
            ),
            PitfallFixSettlement::SameSignatureFailed
        );
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
    fn legacy_broad_resolution_is_audit_only_after_attribution_upgrade() {
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

        // The pre-v1 broad signature helper still records its historical audit
        // fields, but it cannot prove which rendered advice reached which repair
        // turn. It therefore stays behaviorally neutral after the attribution
        // upgrade; production uses the committed attempt-token path instead.
        let raw_path = tmp.path().join(RAW_DIR).join(DEV_ERRORS_FILE);
        let before_failed_write = fs::read_to_string(&raw_path).unwrap();
        let failed_write =
            with_forced_atomic_write_failure(|| mark_pitfalls_resolved(tmp.path(), &err));
        assert_eq!(
            failed_write, 0,
            "a failed audit write cannot report success"
        );
        assert_eq!(
            fs::read_to_string(raw_path).unwrap(),
            before_failed_write,
            "a failed atomic write must leave the complete pre-existing timeline unchanged"
        );

        let n = mark_pitfalls_resolved(tmp.path(), &err);
        assert_eq!(n, 1);
        assert_eq!(
            st(tmp.path()),
            Some(PitfallStatus::Active),
            "legacy broad resolution must not validate a pitfall"
        );
        assert_eq!(pitfall_efficacy_summary(tmp.path()).validated, 0);
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        let audit = row.efficacy.unwrap();
        assert!(
            audit.proven_fix,
            "legacy audit evidence remains inspectable"
        );
        assert_eq!(audit.outcome_attribution_version, 0);
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
    fn project_context_tokens_ignore_oversized_manifests() {
        let tmp = TempDir::new().unwrap();
        std::fs::File::create(tmp.path().join("package.json"))
            .unwrap()
            .set_len(1024 * 1024 + 1)
            .unwrap();
        std::fs::File::create(tmp.path().join("Cargo.toml"))
            .unwrap()
            .set_len(1024 * 1024 + 1)
            .unwrap();

        assert!(project_context_tokens(tmp.path()).is_empty());
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
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
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
        assert_eq!(s_without, 0, "an out-of-stack pitfall must abstain");
        let mut recurring = lesson;
        recurring.efficacy = Some(PitfallEfficacy {
            recurred_after_warning: true,
            ..PitfallEfficacy::default()
        });
        assert_eq!(
            lesson_trigger_score(&recurring, &without),
            0,
            "recurrence raises importance but cannot fabricate relevance"
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
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
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
    fn importance_damps_invalidated_advice_below_validated_history() {
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
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        };
        let recurring = mk(Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            injected: 1,
            occ_at_injection: 1,
            recurred_after_warning: true,
            exact_last_fix_failed_at: "2026-06-22T00:00:00Z".into(),
            proven_fix: false,
            failed_fixes: Vec::new(),
            next_strategy: String::new(),
            helpful: 0,
            harmful: 0,
            recent_observations: vec![test_evidence(
                "importance-failed",
                "2026-06-22T00:00:00Z",
                KnowledgeEvidenceOutcome::FixFailed,
            )],
            ..PitfallEfficacy::default()
        }));
        let validated = mk(Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            injected: 2,
            occ_at_injection: 3,
            recurred_after_warning: false,
            proven_fix: true,
            exact_last_verified_at: "2026-06-22T00:00:00Z".into(),
            failed_fixes: Vec::new(),
            next_strategy: String::new(),
            helpful: 0,
            harmful: 0,
            recent_observations: vec![test_evidence(
                "importance-success",
                "2026-06-22T00:00:00Z",
                KnowledgeEvidenceOutcome::FixSucceeded,
            )],
            ..PitfallEfficacy::default()
        }));
        assert!(
            lesson_importance(&recurring) < lesson_importance(&validated),
            "known-bad repair advice must not outrank causally validated history"
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
                outcome_attribution_version: 1,
                injected: 1,
                occ_at_injection: 1,
                recurred_after_warning: true, // ← already warned, still recurred
                exact_last_fix_failed_at: "2026-06-22T00:00:00Z".into(),
                proven_fix: false,
                failed_fixes: vec!["npm install lodash".into()],
                next_strategy: String::new(),
                helpful: 0,
                harmful: 0,
                recent_observations: vec![test_evidence(
                    "reflection-failed",
                    "2026-06-22T00:00:00Z",
                    KnowledgeEvidenceOutcome::FixFailed,
                )],
                ..PitfallEfficacy::default()
            }),
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
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
                helpful: 0,
                harmful: 0,
                ..PitfallEfficacy::default()
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
                outcome_attribution_version: 1,
                injected: 1,
                occ_at_injection: 1,
                recurred_after_warning: true,
                exact_last_fix_failed_at: "2026-06-22T00:00:00Z".into(),
                proven_fix: false,
                failed_fixes: Vec::new(),
                next_strategy: String::new(),
                helpful: 0,
                harmful: 0,
                ..PitfallEfficacy::default()
            }),
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
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
                    outcome_attribution_version: 1,
                    injected: 2,
                    occ_at_injection: 1,
                    recurred_after_warning: false,
                    proven_fix: true,
                    exact_last_verified_at: "2026-06-22T00:00:00Z".into(),
                    failed_fixes: Vec::new(),
                    next_strategy: String::new(),
                    helpful: 0,
                    harmful: 0,
                    ..PitfallEfficacy::default()
                }),
                invalidated: false,
                trust: NEUTRAL_TRUST,
                evidence_count: 0,
                evidence: Vec::new(),
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
    fn only_causally_validated_dev_errors_are_global_worthy() {
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
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        };
        assert!(!group_is_global_worthy(&[&dev]));

        let recurrent = Lesson {
            occurrences: 2,
            ..dev.clone()
        };
        assert!(!group_is_global_worthy(&[&recurrent]));

        let validated = Lesson {
            efficacy: Some(PitfallEfficacy {
                outcome_attribution_version: 1,
                proven_fix: true,
                exact_last_verified_at: "2026-01-02T00:00:00Z".into(),
                recent_observations: vec![test_evidence(
                    "global-success",
                    "2026-01-02T00:00:00Z",
                    KnowledgeEvidenceOutcome::FixSucceeded,
                )],
                ..PitfallEfficacy::default()
            }),
            ..dev.clone()
        };
        assert!(group_is_global_worthy(&[&validated]));

        // A generic-fallback dev error is NOT promoted on first sight (too noisy).
        let generic = Lesson {
            signature: "general/error/something".into(),
            ..dev.clone()
        };
        assert!(!group_is_global_worthy(&[&generic]));

        // Non-dev lessons stay local even when repeated across requirements.
        let qual = Lesson {
            kind: LessonKind::Failure,
            signature: String::new(),
            occurrences: 2,
            ..dev.clone()
        };
        assert!(!group_is_global_worthy(&[&qual]));
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
    fn lesson_capture_policy_is_leaf_scoped_and_reversible() {
        let quality = TempDir::new().unwrap();
        crate::memory_control::update_capture(
            quality.path(),
            MemoryScope::Project,
            Some(MemoryStore::QualityFailures),
            false,
        )
        .unwrap();
        capture_quality_failures(
            quality.path(),
            &[check("API contract", "failed", 20)],
            "demo",
            "api contract",
        );
        assert!(read_raw_lessons(quality.path(), "quality-failures.jsonl").is_empty());
        crate::memory_control::update_capture(
            quality.path(),
            MemoryScope::Project,
            Some(MemoryStore::QualityFailures),
            true,
        )
        .unwrap();
        capture_quality_failures(
            quality.path(),
            &[check("API contract", "failed", 20)],
            "demo",
            "api contract",
        );
        assert_eq!(
            read_raw_lessons(quality.path(), "quality-failures.jsonl").len(),
            1
        );

        let revision = TempDir::new().unwrap();
        crate::memory_control::update_capture(
            revision.path(),
            MemoryScope::Project,
            Some(MemoryStore::GateRevisions),
            false,
        )
        .unwrap();
        let adr = capture_gate_revision(revision.path(), "docs_confirm", "add detail", "r");
        assert!(
            adr.is_file(),
            "the independent gate-adrs leaf remains enabled"
        );
        assert!(read_raw_lessons(revision.path(), "gate-revisions.jsonl").is_empty());
        crate::memory_control::update_capture(
            revision.path(),
            MemoryScope::Project,
            Some(MemoryStore::GateAdrs),
            false,
        )
        .unwrap();
        let disabled_adr =
            capture_gate_revision(revision.path(), "quality_confirm", "keep moving", "r");
        assert!(!disabled_adr.exists());
        crate::memory_control::update_capture(
            revision.path(),
            MemoryScope::Project,
            Some(MemoryStore::GateRevisions),
            true,
        )
        .unwrap();
        let raw_only = capture_gate_revision(revision.path(), "delivery_confirm", "raw only", "r");
        assert!(!raw_only.exists());
        assert_eq!(
            read_raw_lessons(revision.path(), "gate-revisions.jsonl").len(),
            1,
            "gate-revisions capture remains independent from gate-adrs"
        );

        let validated = TempDir::new().unwrap();
        let spec = umadev_contract::parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | /api/posts | - | - | none | List |\n",
            "demo",
        );
        crate::memory_control::update_capture(
            validated.path(),
            MemoryScope::Project,
            Some(MemoryStore::ValidatedPatterns),
            false,
        )
        .unwrap();
        capture_validated_patterns(validated.path(), "demo", "posts", &spec, &[], true);
        assert!(read_raw_lessons(validated.path(), "validated-decisions.jsonl").is_empty());

        use crate::tech_debt::{DebtItem, DebtKind, DebtStatus};
        let debt = TempDir::new().unwrap();
        crate::memory_control::update_capture(
            debt.path(),
            MemoryScope::Project,
            Some(MemoryStore::TechDebt),
            false,
        )
        .unwrap();
        let items = [DebtItem {
            file: "output/prd.md".into(),
            line: 3,
            kind: DebtKind::FillerText,
            snippet: "Lorem ipsum".into(),
            first_seen: "2026-07-16T00:00:00Z".into(),
            status: DebtStatus::Open,
            resolved_at: String::new(),
        }];
        assert_eq!(capture_tech_debt(debt.path(), &items, "r"), 0);
        assert!(read_raw_lessons(debt.path(), "tech-debt.jsonl").is_empty());

        let pitfalls = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        crate::memory_control::update_capture(
            pitfalls.path(),
            MemoryScope::Project,
            Some(MemoryStore::Pitfalls),
            false,
        )
        .unwrap();
        assert_eq!(
            capture_dev_errors(pitfalls.path(), std::slice::from_ref(&error), "demo", "r"),
            0
        );
        assert!(read_raw_lessons(pitfalls.path(), DEV_ERRORS_FILE).is_empty());
        crate::memory_control::update_capture(
            pitfalls.path(),
            MemoryScope::Project,
            Some(MemoryStore::Pitfalls),
            true,
        )
        .unwrap();
        assert_eq!(
            capture_dev_errors(pitfalls.path(), std::slice::from_ref(&error), "demo", "r"),
            1
        );
        crate::memory_control::update_capture(
            pitfalls.path(),
            MemoryScope::Project,
            Some(MemoryStore::PitfallReflections),
            false,
        )
        .unwrap();
        assert!(!record_pitfall_strategy(
            pitfalls.path(),
            "dependency/module-not-found/lodash",
            "pin and clean-install"
        ));

        let beliefs = TempDir::new().unwrap();
        seed_cluster(
            beliefs.path(),
            BELIEF_MIN_CLUSTER,
            &["color", "token", "frontend"],
            "frontend",
        );
        crate::memory_control::update_capture(
            beliefs.path(),
            MemoryScope::Project,
            Some(MemoryStore::Beliefs),
            false,
        )
        .unwrap();
        assert_eq!(fold_beliefs(beliefs.path()), 0);
        assert!(read_raw_lessons(beliefs.path(), BELIEFS_FILE).is_empty());
        crate::memory_control::update_capture(
            beliefs.path(),
            MemoryScope::Project,
            Some(MemoryStore::Beliefs),
            true,
        )
        .unwrap();
        assert!(fold_beliefs(beliefs.path()) > 0);

        let sediment = TempDir::new().unwrap();
        capture_quality_failures(
            sediment.path(),
            &[check("API contract", "failed", 20)],
            "demo",
            "api contract",
        );
        crate::memory_control::update_capture(
            sediment.path(),
            MemoryScope::Project,
            Some(MemoryStore::LessonSediment),
            false,
        )
        .unwrap();
        assert_eq!(sediment_lessons(sediment.path()), 0);
        assert!(list_sedimented_lessons(sediment.path()).is_empty());
        assert_eq!(
            read_all_raw_lessons(sediment.path()).len(),
            1,
            "derived-capture off leaves the authoritative ledger visible",
        );
        crate::memory_control::update_capture(
            sediment.path(),
            MemoryScope::Project,
            Some(MemoryStore::LessonSediment),
            true,
        )
        .unwrap();
        assert_eq!(sediment_lessons(sediment.path()), 1);
    }

    #[test]
    fn pitfall_capture_off_allows_recall_but_issues_no_new_attempt() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        assert_eq!(
            capture_dev_errors(
                tmp.path(),
                std::slice::from_ref(&error),
                "demo",
                "dependency lodash"
            ),
            1
        );
        crate::memory_control::update_capture(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Pitfalls),
            false,
        )
        .unwrap();
        let before = std::fs::read(tmp.path().join(RAW_DIR).join(DEV_ERRORS_FILE)).unwrap();
        assert!(!lessons_for_error(tmp.path(), &error).is_empty());
        assert!(commit_pitfall_fix_attempt(tmp.path(), &error).is_none());
        assert_eq!(
            std::fs::read(tmp.path().join(RAW_DIR).join(DEV_ERRORS_FILE)).unwrap(),
            before,
            "recall with capture disabled must not update efficacy counters"
        );
    }

    #[test]
    fn automatic_invalidation_never_rewrites_a_capture_disabled_leaf() {
        let tmp = TempDir::new().unwrap();
        let older = mk_db_lesson(
            "indexing add",
            "always add a btree index on every filtered column for speed",
            "missing indexes slowed reads across the board significantly",
            "2026-06-01T00:00:00Z",
        );
        let newer = mk_db_lesson(
            "indexing drop",
            "avoid over indexing and drop redundant indexes to keep writes fast",
            "too many indexes bloated writes and storage badly here",
            "2026-06-20T00:00:00Z",
        );
        assert!(write_raw_lessons(
            tmp.path(),
            "quality-failures.jsonl",
            &[older, newer]
        ));
        crate::memory_control::update_capture(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::QualityFailures),
            false,
        )
        .unwrap();
        let path = tmp.path().join(RAW_DIR).join("quality-failures.jsonl");
        let before = std::fs::read(&path).unwrap();
        assert_eq!(scan_contradictions(tmp.path()), 0);
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn reconcile_base_prompt_respects_each_leaf_recall_policy() {
        let tmp = TempDir::new().unwrap();
        let first = mk_db_lesson(
            "indexing one",
            "add a targeted btree index for frequently filtered columns",
            "targeted indexes prevent repeated sequential scans",
            "2026-06-01T00:00:00Z",
        );
        let second = mk_db_lesson(
            "indexing two",
            "add a targeted btree index for frequently filtered fields",
            "targeted indexes prevent repeated table scans",
            "2026-06-20T00:00:00Z",
        );
        assert!(write_raw_lessons(
            tmp.path(),
            "quality-failures.jsonl",
            &[first, second]
        ));
        assert!(!reconcile_candidates(tmp.path()).is_empty());
        crate::memory_control::update_recall(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::QualityFailures),
            false,
        )
        .unwrap();
        assert!(reconcile_candidates(tmp.path()).is_empty());

        let calls = std::cell::Cell::new(0usize);
        let judge = |_: &Lesson, _: &[Lesson]| {
            calls.set(calls.get().saturating_add(1));
            ReconcileDecision::Noop
        };
        let _ = sediment_lessons_with_judge(tmp.path(), Some(&judge));
        assert_eq!(
            calls.get(),
            0,
            "recall-off lesson text must never be rendered into the base judge prompt"
        );
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
    fn persisted_revision_and_reflection_memory_redacts_secret_values() {
        let tmp = TempDir::new().unwrap();
        let secret = concat!("sk-", "live-secret-value-1234567890");
        let adr = capture_gate_revision(
            tmp.path(),
            "docs_confirm",
            &format!("api_key={secret}"),
            "password=hunter2",
        );
        let adr_body = fs::read_to_string(adr).unwrap();
        let raw_body =
            fs::read_to_string(tmp.path().join(RAW_DIR).join("gate-revisions.jsonl")).unwrap();
        assert!(!adr_body.contains(secret));
        assert!(!adr_body.contains("hunter2"));
        assert!(!raw_body.contains(secret));
        assert!(!raw_body.contains("hunter2"));

        let error = "Error: Cannot find module 'safe-memory-example'";
        capture_dev_errors(tmp.path(), &[error.to_string()], "demo", "requirement");
        assert!(record_pitfall_strategy(
            tmp.path(),
            "dependency/module-not-found/safe-memory-example",
            &format!("pin dependency with api_key={secret}"),
        ));
        let pitfall_body =
            fs::read_to_string(tmp.path().join(RAW_DIR).join(DEV_ERRORS_FILE)).unwrap();
        assert!(!pitfall_body.contains(secret));
        assert!(pitfall_body.contains("[redacted]"));
    }

    #[cfg(unix)]
    #[test]
    fn gate_adrs_and_reflections_never_follow_managed_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let project = TempDir::new().unwrap();
        let outside_decisions = TempDir::new().unwrap();
        let outside_reflections = TempDir::new().unwrap();
        fs::create_dir(project.path().join(".umadev")).unwrap();
        symlink(
            outside_decisions.path(),
            project.path().join(".umadev/decisions"),
        )
        .unwrap();
        symlink(
            outside_reflections.path(),
            project.path().join(".umadev/reflections"),
        )
        .unwrap();

        let adr = capture_gate_revision(
            project.path(),
            "../../outside",
            "keep this inside the project",
            "requirement",
        );
        assert!(!adr.exists());
        assert_eq!(fs::read_dir(outside_decisions.path()).unwrap().count(), 0);

        let reflection = Reflection {
            signature: "dependency/module-not-found/example".into(),
            occurrences: 2,
            strategy: "pin the exact dependency".into(),
            at: "2026-07-21T00:00:00Z".into(),
        };
        assert!(!append_reflection(project.path(), &reflection));
        assert_eq!(fs::read_dir(outside_reflections.path()).unwrap().count(), 0);
    }

    #[test]
    fn capture_tech_debt_feeds_significant_debt_into_kb() {
        let _home = TempHome::new();
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
        // No gaps (everything implemented), gate passed.
        capture_validated_patterns(tmp.path(), "demo", "博客系统", &spec, &[], true);
        let lessons = read_raw_lessons(tmp.path(), "validated-decisions.jsonl");
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].kind, LessonKind::ValidatedPattern);
        assert!(lessons[0].body.contains("/api/articles"));
        // Gate-passed wording is present only when the gate actually passed.
        assert!(lessons[0].body.contains("passed the quality gate"));
    }

    #[test]
    fn capture_validated_patterns_empty_spec_skips() {
        let tmp = TempDir::new().unwrap();
        capture_validated_patterns(tmp.path(), "demo", "x", &ApiSpec::default(), &[], true);
        assert!(read_raw_lessons(tmp.path(), "validated-decisions.jsonl").is_empty());
    }

    #[test]
    fn capture_validated_patterns_only_sediments_implemented_endpoints() {
        // #3: a planned endpoint with NO implementation evidence (in the gap
        // list) must NOT be sedimented as a validated fact; only the implemented
        // one survives.
        let tmp = TempDir::new().unwrap();
        let spec = umadev_contract::parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n\
             | GET | /api/articles | - | - | none | List |\n\
             | POST | /api/comments | - | - | none | Create |\n",
            "demo",
        );
        // /api/comments was planned but never built → it is a gap.
        let gaps = vec!["POST /api/comments — Create".to_string()];
        capture_validated_patterns(tmp.path(), "demo", "博客系统", &spec, &gaps, true);
        let lessons = read_raw_lessons(tmp.path(), "validated-decisions.jsonl");
        assert_eq!(lessons.len(), 1);
        assert!(lessons[0].body.contains("/api/articles"));
        assert!(
            !lessons[0].body.contains("/api/comments"),
            "unimplemented endpoint must not be sedimented as validated: {}",
            lessons[0].body
        );
    }

    #[test]
    fn capture_validated_patterns_all_unimplemented_writes_nothing() {
        // #3: if EVERY planned endpoint is a gap, nothing is sedimented.
        let tmp = TempDir::new().unwrap();
        let spec = umadev_contract::parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | /api/articles | - | - | none | List |\n",
            "demo",
        );
        let gaps = vec!["GET /api/articles — List".to_string()];
        capture_validated_patterns(tmp.path(), "demo", "x", &spec, &gaps, true);
        assert!(read_raw_lessons(tmp.path(), "validated-decisions.jsonl").is_empty());
    }

    #[test]
    fn capture_validated_patterns_gate_not_passed_writes_no_validated_claim() {
        // A failed gate cannot mint a ValidatedPattern under softer wording.
        let tmp = TempDir::new().unwrap();
        let spec = umadev_contract::parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | /api/articles | - | - | none | List |\n",
            "demo",
        );
        capture_validated_patterns(tmp.path(), "demo", "博客系统", &spec, &[], false);
        let lessons = read_raw_lessons(tmp.path(), "validated-decisions.jsonl");
        assert!(lessons.is_empty());
        assert!(lessons_report(tmp.path()).validated_patterns.is_empty());
        assert!(lessons_report(tmp.path())
            .curated_lessons
            .iter()
            .all(|entry| entry.status != CuratedLessonStatus::Validated));
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
    fn parallel_raw_appends_do_not_lose_either_capture() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for name in ["Parallel A", "Parallel B"] {
            let root = root.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                capture_quality_failures(&root, &[check(name, "failed", 10)], "demo", "req");
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        let raw = read_raw_lessons(&root, "quality-failures.jsonl");
        assert_eq!(raw.len(), 2, "atomic replacement must retain both appends");
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
        let _home = TempHome::new();
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
        let _home = TempHome::new();
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
        let _home = TempHome::new();
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
        let _home = TempHome::new();
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
        let _home = TempHome::new();
        let tmp = TempDir::new().unwrap();
        assert_eq!(sediment_lessons(tmp.path()), 0);
        assert!(list_sedimented_lessons(tmp.path()).is_empty());
    }

    #[test]
    fn list_sedimented_skips_raw_dir() {
        let _home = TempHome::new();
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
        capture_validated_patterns(tmp.path(), "blog", "做一个博客", &spec, &[], true);
        let r = lessons_report(tmp.path());
        assert!(!r.is_empty());
        assert!(
            r.validated_patterns.is_empty(),
            "a green build is observation evidence, not causal repair validation"
        );
        let candidate = r
            .curated_lessons
            .iter()
            .find(|entry| entry.title.contains("blog"))
            .expect("source-verified pattern remains visible as a candidate");
        assert_eq!(candidate.status, CuratedLessonStatus::Hypothesis);
        assert_eq!(candidate.evidence_count, 1);
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
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
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
        let _home = TempHome::new();
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
        let _home = TempHome::new();
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
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
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

    // ---- ① belief layer ------------------------------------------------

    /// Build `n` near-duplicate lessons (same domain + shared keywords) so they
    /// cluster into one belief, written to the quality-failures ledger.
    fn seed_cluster(root: &Path, n: usize, kw: &[&str], domain: &str) {
        let mut rows = Vec::new();
        for i in 0..n {
            rows.push(Lesson {
                kind: LessonKind::Failure,
                domain: domain.into(),
                title: format!("{domain} lesson {i}"),
                body: format!("body about {} number {i}", kw.join(" ")),
                fix: format!(
                    "always use design tokens for {} (variant {i})",
                    kw.join(" ")
                ),
                root_cause: format!("hardcoded {} caused the {domain} failure", kw.join(" ")),
                keywords: kw.iter().map(|k| (*k).to_string()).collect(),
                source_requirement: "做一个仪表盘".into(),
                first_seen: format!("2026-06-{:02}T00:00:00Z", 10 + i),
                signature: String::new(),
                occurrences: 1,
                context: Vec::new(),
                efficacy: None,
                invalidated: false,
                trust: NEUTRAL_TRUST,
                evidence_count: 0,
                evidence: Vec::new(),
            });
        }
        write_raw_lessons(root, "quality-failures.jsonl", &rows);
    }

    #[test]
    fn fold_beliefs_collapses_n_similar_into_one_with_evidence_count() {
        let tmp = TempDir::new().unwrap();
        seed_cluster(tmp.path(), 4, &["color", "token", "frontend"], "frontend");
        let touched = fold_beliefs(tmp.path());
        assert_eq!(touched, 1, "four near-duplicates fold into ONE belief");

        let beliefs = read_raw_lessons(tmp.path(), BELIEFS_FILE);
        assert_eq!(beliefs.len(), 1);
        let b = &beliefs[0];
        assert_eq!(b.kind, LessonKind::Belief);
        assert_eq!(
            b.evidence_count, 4,
            "belief records its N supporting lessons"
        );
        assert_eq!(b.evidence.len(), 4, "evidence keys recorded for demotion");
        // Freshness = the most recent member's timestamp (last_confirmed).
        assert_eq!(b.first_seen, "2026-06-13T00:00:00Z");
        assert_eq!(b.knowledge_evidence_count(), 0);
        assert_eq!(b.pitfall_status(), PitfallStatus::Hypothesis);
    }

    #[test]
    fn fold_beliefs_updates_existing_belief_on_refold() {
        let tmp = TempDir::new().unwrap();
        seed_cluster(tmp.path(), 3, &["auth", "session", "token"], "api");
        assert_eq!(fold_beliefs(tmp.path()), 1);
        let before = read_raw_lessons(tmp.path(), BELIEFS_FILE);
        assert_eq!(before[0].evidence_count, 3);

        // Add a fourth member to the SAME cluster, re-fold → UPDATE in place (no
        // duplicate belief), evidence_count grows.
        let mut rows = read_raw_lessons(tmp.path(), "quality-failures.jsonl");
        rows.push(Lesson {
            evidence: Vec::new(),
            title: "api lesson extra".into(),
            first_seen: "2026-06-20T00:00:00Z".into(),
            ..rows[0].clone()
        });
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &rows);
        assert_eq!(fold_beliefs(tmp.path()), 1, "re-fold updates, not appends");
        let after = read_raw_lessons(tmp.path(), BELIEFS_FILE);
        assert_eq!(after.len(), 1, "still exactly one belief (UPDATE, not ADD)");
        assert_eq!(after[0].evidence_count, 4, "evidence grew on re-fold");
    }

    #[test]
    fn belief_preferred_over_its_raw_evidence_in_recall() {
        let tmp = TempDir::new().unwrap();
        // Cluster of lessons whose keywords intersect the requirement so they
        // positively match the trigger query.
        seed_cluster(tmp.path(), 3, &["dashboard", "color", "token"], "frontend");
        fold_beliefs(tmp.path());
        // Requirement shares "dashboard" → the belief matches.
        let selected = select_relevant_lessons(tmp.path(), "build a dashboard with color tokens");
        assert!(
            selected.iter().any(|l| l.kind == LessonKind::Belief),
            "the dense belief must be selected: {:?}",
            selected.iter().map(|l| &l.title).collect::<Vec<_>>()
        );
        // Its raw evidence lessons are demoted (not also surfaced).
        assert!(
            !selected
                .iter()
                .any(|l| l.kind == LessonKind::Failure && l.domain == "frontend"),
            "raw evidence demoted in favour of the belief"
        );
    }

    // ── Delta-playbook bound invariants ──────────────────────────────────────
    // The injected memory block is a COMPACT, capped, deduped, ranked playbook —
    // never a context-collapsing wall of raw episodes. These lock the four
    // invariants: (1) count-capped, (2) byte-bounded, (3) near-dups merged,
    // (4) ranked by frequency × recency, (5) fail-open.

    /// A dev-error pitfall lesson with explicit signature/fix/recency/frequency —
    /// the unit for the playbook-bound tests.
    fn mk_pitfall(sig: &str, fix: &str, root: &str, when: &str, occ: u32) -> Lesson {
        Lesson {
            kind: LessonKind::DevError,
            domain: "dependency".into(),
            title: format!("踩坑 [{sig}]"),
            body: String::new(),
            fix: fix.into(),
            root_cause: root.into(),
            keywords: vec!["zzznomatch".into()],
            source_requirement: String::new(),
            first_seen: when.into(),
            signature: sig.into(),
            occurrences: occ,
            context: Vec::new(),
            efficacy: None,
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn injected_playbook_is_count_capped_under_many_lessons() {
        // Many recorded pitfalls must still inject at most MEMORY_PLAYBOOK_MAX_DELTAS
        // deltas — the playbook is a small curated set, not the whole ledger.
        let tmp = TempDir::new().unwrap();
        let rows: Vec<Lesson> = (0..40)
            .map(|i| {
                mk_pitfall(
                    &format!("dependency/module-not-found/pkg{i}"),
                    &format!("install pkg{i}"),
                    &format!("pkg{i} was missing"),
                    &format!("2026-06-{:02}T00:00:00Z", 1 + (i % 28)),
                    1,
                )
            })
            .collect();
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &rows);

        let block = relevant_lessons_for_prompt(tmp.path(), "zzznomatch");
        let deltas = block.matches("[pitfall]").count();
        assert!(
            deltas >= 1,
            "some pitfall surfaces from 40 recorded: {block}"
        );
        assert!(
            deltas <= MEMORY_PLAYBOOK_MAX_DELTAS,
            "injected playbook is count-capped at {MEMORY_PLAYBOOK_MAX_DELTAS}, got {deltas}"
        );
        assert!(
            block.chars().count() <= MEMORY_PLAYBOOK_BUDGET,
            "small deltas stay well within the byte budget"
        );
    }

    #[test]
    fn injected_playbook_is_byte_bounded_under_huge_deltas() {
        // A single fat delta (e.g. a belief's longest-member fix, or a verbose
        // captured fix) must NOT blow the block: the byte budget head-truncates so
        // the playbook can never crowd the firmware / dilute signal. This is the
        // unbounded-path guard — count cap alone (≤3) doesn't bound bytes.
        let tmp = TempDir::new().unwrap();
        let fat_fix = "x".repeat(8_000);
        let fat_root = "y".repeat(8_000);
        let rows: Vec<Lesson> = (0..5)
            .map(|i| {
                mk_pitfall(
                    &format!("dependency/module-not-found/big{i}"),
                    &fat_fix,
                    &fat_root,
                    &format!("2026-06-{:02}T00:00:00Z", 1 + i),
                    1,
                )
            })
            .collect();
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &rows);

        let block = relevant_lessons_for_prompt(tmp.path(), "zzznomatch");
        assert!(
            block.chars().count() <= MEMORY_PLAYBOOK_BUDGET,
            "injected playbook stays within the byte budget even with fat deltas: {} > {MEMORY_PLAYBOOK_BUDGET}",
            block.chars().count()
        );
        // …and it is NOT degraded to a header-only block — the top delta survives.
        assert!(
            block.contains("[pitfall]"),
            "the top delta is head-kept (truncated), never dropped to header-only: {block}"
        );
    }

    #[test]
    fn near_duplicate_deltas_are_deduped_in_injected_playbook() {
        // Near-duplicate raw lessons fold into ONE belief; the injected block then
        // shows the single distilled delta, NOT the N near-duplicate originals —
        // the dedup that keeps the playbook compact and high-signal.
        let tmp = TempDir::new().unwrap();
        seed_cluster(tmp.path(), 5, &["dashboard", "color", "token"], "frontend");
        fold_beliefs(tmp.path());
        let block = relevant_lessons_for_prompt(tmp.path(), "build a dashboard with color tokens");
        assert_eq!(
            block.matches("[belief]").count(),
            1,
            "exactly one distilled belief delta surfaces: {block}"
        );
        // The 5 raw frontend evidence lessons ([warn]) must be demoted, not also
        // listed alongside the belief that already covers them.
        assert!(
            !block.contains("[warn]"),
            "raw near-duplicate evidence is deduped away in favour of the belief: {block}"
        );
    }

    #[test]
    fn playbook_decay_score_favors_frequency_and_recency() {
        // The ranking axis: a more-recent and a more-frequent pitfall each outscore
        // an older / rarer one (frequency × recency), so the compact set keeps the
        // deltas that matter most.
        let now = Utc::now();
        let q = std::collections::HashSet::new(); // retention still orders unmatched evidence

        let recent = mk_pitfall("d/x/recent", "f", "r", &iso(now), 1);
        let old = mk_pitfall(
            "d/x/old",
            "f",
            "r",
            &iso(now - chrono::Duration::days(120)),
            1,
        );
        assert!(
            lesson_decay_score(&recent, &q, now) > lesson_decay_score(&old, &q, now),
            "a more RECENT pitfall outranks an older one at equal frequency"
        );

        let frequent = mk_pitfall("d/x/freq", "f", "r", &iso(now), 8);
        let rare = mk_pitfall("d/x/rare", "f", "r", &iso(now), 1);
        assert!(
            lesson_decay_score(&frequent, &q, now) > lesson_decay_score(&rare, &q, now),
            "a more FREQUENT pitfall outranks a rarer one at equal recency"
        );
    }

    #[test]
    fn injected_playbook_orders_recent_frequent_delta_first() {
        // End-to-end: the recent + frequent pitfall is rendered BEFORE the old +
        // rare one in the injected block (the ranking actually drives surfacing).
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let rows = vec![
            mk_pitfall(
                "dependency/module-not-found/hot",
                "install hot",
                "hot missing",
                &iso(now),
                9,
            ),
            mk_pitfall(
                "dependency/module-not-found/cold",
                "install cold",
                "cold missing",
                &iso(now - chrono::Duration::days(120)),
                1,
            ),
        ];
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &rows);
        let block = relevant_lessons_for_prompt(tmp.path(), "zzznomatch");
        let hot_at = block.find("hot").expect("hot delta present");
        let cold_at = block.find("cold").expect("cold delta present");
        assert!(
            hot_at < cold_at,
            "recent+frequent delta is surfaced before the old+rare one: {block}"
        );
    }

    #[test]
    fn relevant_lessons_for_prompt_is_fail_open_on_a_corrupt_store() {
        // A corrupt / unreadable ledger must degrade to an empty (or still-bounded)
        // block — never a panic. Memory failure can't break a turn.
        let tmp = TempDir::new().unwrap();
        let raw_dir = tmp.path().join(RAW_DIR);
        std::fs::create_dir_all(&raw_dir).unwrap();
        // Garbage lines + one valid pitfall: the parser skips the bad rows.
        let valid = serde_json::to_string(&mk_pitfall(
            "dependency/module-not-found/ok",
            "fix it",
            "why",
            "2026-06-10T00:00:00Z",
            1,
        ))
        .unwrap();
        std::fs::write(
            raw_dir.join(DEV_ERRORS_FILE),
            format!("{{not json at all\n\n[]\n{valid}\n"),
        )
        .unwrap();
        // Must not panic, and whatever it returns stays within the budget.
        let block = relevant_lessons_for_prompt(tmp.path(), "完全无关的需求文本占位");
        assert!(
            block.chars().count() <= MEMORY_PLAYBOOK_BUDGET,
            "fail-open recall is still bounded"
        );
    }

    /// Format a `DateTime<Utc>` as the `%Y-%m-%dT%H:%M:%SZ` stamp every capture
    /// site writes (so `recency_weight` parses it).
    fn iso(t: chrono::DateTime<Utc>) -> String {
        t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    #[test]
    fn fold_beliefs_is_noop_for_lone_lesson() {
        let tmp = TempDir::new().unwrap();
        seed_cluster(tmp.path(), 1, &["solo"], "api");
        assert_eq!(
            fold_beliefs(tmp.path()),
            0,
            "a single lesson mints no belief"
        );
        assert!(read_raw_lessons(tmp.path(), BELIEFS_FILE).is_empty());
    }

    #[test]
    fn evidence_key_disambiguates_same_title_different_content() {
        // P2-7: two lessons sharing `domain` + (template-generated) `title` but with
        // DIFFERENT advice must get DIFFERENT evidence keys, so a belief covering one
        // can never wrongly demote / downgrade the other.
        let base = mk_db_lesson(
            "Belief: index (2 lessons)",
            "create a covering index on the lookup columns",
            "missing index slowed the report",
            "2026-06-01T00:00:00Z",
        );
        let mut other = base.clone();
        other.fix = "drop the redundant index to speed writes".into();
        other.root_cause = "over-indexing bloated writes".into();
        assert_ne!(
            evidence_key(&base),
            evidence_key(&other),
            "same domain+title but different advice → different keys"
        );
        // A true recurrence (identical advice, refreshed timestamp) → SAME key, so
        // the recurrence-matching intent is preserved.
        let mut recurrence = base.clone();
        recurrence.first_seen = "2026-09-09T00:00:00Z".into();
        assert_eq!(
            evidence_key(&base),
            evidence_key(&recurrence),
            "a recurrence of the same advice still resolves to the same key"
        );
    }

    #[test]
    fn fold_beliefs_merges_all_overlapping_beliefs_no_duplicate() {
        // P2-7: when a later, larger cluster's evidence spans TWO previously-minted
        // beliefs, the re-fold must MERGE them into ONE (the union) — never leave a
        // stale duplicate. Construct two disjoint sub-clusters that each mint their
        // own belief, then add a bridging lesson so they unify into one cluster.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Helper: a lesson whose keywords decide clustering. Two pairs that DON'T
        // cross-cluster initially (disjoint keyword sets, same domain gives only a
        // +1 bonus — below the fold threshold of 3 without shared keywords).
        let mk = |title: &str, kw: &[&str], fix: &str, when: &str| Lesson {
            kind: LessonKind::Failure,
            domain: "frontend".into(),
            title: title.into(),
            body: String::new(),
            fix: fix.into(),
            root_cause: format!("root for {title}"),
            keywords: kw.iter().map(|k| (*k).to_string()).collect(),
            source_requirement: "r".into(),
            first_seen: when.into(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: None,
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        };

        // Cluster X: two lessons sharing keywords {color, token}.
        // Cluster Y: two lessons sharing keywords {layout, grid}.
        let x1 = mk(
            "x1",
            &["color", "token"],
            "use color tokens A",
            "2026-06-01T00:00:00Z",
        );
        let x2 = mk(
            "x2",
            &["color", "token"],
            "use color tokens B",
            "2026-06-02T00:00:00Z",
        );
        let y1 = mk(
            "y1",
            &["layout", "grid"],
            "use a grid layout A",
            "2026-06-03T00:00:00Z",
        );
        let y2 = mk(
            "y2",
            &["layout", "grid"],
            "use a grid layout B",
            "2026-06-04T00:00:00Z",
        );
        write_raw_lessons(root, "quality-failures.jsonl", &[x1, x2, y1, y2]);

        // First fold → two separate beliefs (X and Y don't share enough keywords).
        assert_eq!(fold_beliefs(root), 2, "two disjoint clusters → two beliefs");
        assert_eq!(read_raw_lessons(root, BELIEFS_FILE).len(), 2);

        // Now add a BRIDGE lesson sharing keywords with BOTH clusters, so the next
        // fold unifies x* + y* + bridge into ONE cluster whose evidence overlaps
        // BOTH existing beliefs.
        let mut rows = read_raw_lessons(root, "quality-failures.jsonl");
        rows.push(mk(
            "bridge",
            &["color", "token", "layout", "grid"],
            "tie color tokens to the grid layout",
            "2026-06-05T00:00:00Z",
        ));
        write_raw_lessons(root, "quality-failures.jsonl", &rows);

        fold_beliefs(root);
        let beliefs = read_raw_lessons(root, BELIEFS_FILE);
        assert_eq!(
            beliefs.len(),
            1,
            "the bridging cluster MERGES the two beliefs into one (no duplicate): {:?}",
            beliefs.iter().map(|b| &b.title).collect::<Vec<_>>()
        );
        assert_eq!(
            beliefs[0].evidence_count, 5,
            "the merged belief folds all five lessons"
        );
    }

    // ---- ② contradiction / staleness scan ------------------------------

    #[test]
    fn scan_contradictions_invalidates_older_conflicting_advice() {
        let tmp = TempDir::new().unwrap();
        // Same topic (shared keywords + domain → high overlap) but the advice
        // text barely overlaps → a likely contradiction.
        let mk = |title: &str, fix: &str, root: &str, when: &str| Lesson {
            kind: LessonKind::Failure,
            domain: "database".into(),
            title: title.into(),
            body: String::new(),
            fix: fix.into(),
            root_cause: root.into(),
            keywords: vec![
                "database".into(),
                "index".into(),
                "query".into(),
                "postgres".into(),
            ],
            source_requirement: "r".into(),
            first_seen: when.into(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: None,
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        };
        let older = mk(
            "indexing advice A",
            "always add a btree index on every column for speed",
            "missing indexes slowed reads",
            "2026-06-01T00:00:00Z",
        );
        let newer = mk(
            "indexing advice B",
            "avoid over-indexing; drop redundant indexes to keep writes fast",
            "too many indexes bloated writes",
            "2026-06-20T00:00:00Z",
        );
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &[older, newer]);

        let n = scan_contradictions(tmp.path());
        assert_eq!(n, 1, "one older conflicting lesson invalidated");
        let rows = read_raw_lessons(tmp.path(), "quality-failures.jsonl");
        let a = rows
            .iter()
            .find(|l| l.title == "indexing advice A")
            .unwrap();
        let b = rows
            .iter()
            .find(|l| l.title == "indexing advice B")
            .unwrap();
        assert!(
            a.invalidated,
            "the OLDER conflicting advice is marked stale"
        );
        assert!(!b.invalidated, "the fresher advice survives");
    }

    #[test]
    fn scan_contradictions_leaves_agreeing_lessons_alone() {
        let tmp = TempDir::new().unwrap();
        // Same topic AND the advice text strongly overlaps → NOT a contradiction.
        seed_cluster(tmp.path(), 2, &["color", "token", "frontend"], "frontend");
        let n = scan_contradictions(tmp.path());
        assert_eq!(n, 0, "agreeing same-topic lessons are not invalidated");
        assert!(read_raw_lessons(tmp.path(), "quality-failures.jsonl")
            .iter()
            .all(|l| !l.invalidated));
    }

    /// Build two same-topic lessons whose advice text barely overlaps but does NOT
    /// disagree — they AGREE, just worded differently. Pre-fix the low Jaccard alone
    /// invalidated the older one (a false positive); the antonym gate must spare it.
    fn mk_db_lesson(title: &str, fix: &str, root: &str, when: &str) -> Lesson {
        Lesson {
            kind: LessonKind::Failure,
            domain: "database".into(),
            title: title.into(),
            body: String::new(),
            fix: fix.into(),
            root_cause: root.into(),
            keywords: vec![
                "database".into(),
                "index".into(),
                "query".into(),
                "postgres".into(),
            ],
            source_requirement: "r".into(),
            first_seen: when.into(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: None,
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn scan_contradictions_spares_agreeing_but_differently_worded_advice() {
        // P2-6: same topic, LOW text overlap, but the two advices AGREE (both say
        // "create a composite index") — they merely use different vocabulary. No
        // antonym conflict → the heuristic must NOOP (no false invalidation).
        let tmp = TempDir::new().unwrap();
        let older = mk_db_lesson(
            "indexing A",
            "create a composite covering index spanning the lookup columns",
            "sequential scans dominated the slow report query plan",
            "2026-06-01T00:00:00Z",
        );
        let newer = mk_db_lesson(
            "indexing B",
            "build a multicolumn btree so the planner reaches rows directly",
            "full table reads bottlenecked dashboard latency badly here",
            "2026-06-20T00:00:00Z",
        );
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &[older, newer]);

        let n = scan_contradictions(tmp.path());
        assert_eq!(
            n, 0,
            "agreeing-but-differently-worded advice must NOT be invalidated"
        );
        assert!(read_raw_lessons(tmp.path(), "quality-failures.jsonl")
            .iter()
            .all(|l| !l.invalidated));
    }

    #[test]
    fn scan_contradictions_still_catches_real_opposing_advice() {
        // The true-positive must survive the tightening: opposing verbs (add ↔ drop,
        // always ↔ avoid) present → still invalidate the OLDER one.
        let tmp = TempDir::new().unwrap();
        let older = mk_db_lesson(
            "indexing add",
            "always add a btree index on every filtered column for speed",
            "missing indexes slowed reads across the board significantly",
            "2026-06-01T00:00:00Z",
        );
        let newer = mk_db_lesson(
            "indexing drop",
            "avoid over indexing and drop redundant indexes to keep writes fast",
            "too many indexes bloated writes and storage badly here",
            "2026-06-20T00:00:00Z",
        );
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &[older, newer]);

        let n = scan_contradictions(tmp.path());
        assert_eq!(
            n, 1,
            "genuine opposing advice still invalidates the older one"
        );
        let rows = read_raw_lessons(tmp.path(), "quality-failures.jsonl");
        assert!(
            rows.iter()
                .find(|l| l.title == "indexing add")
                .unwrap()
                .invalidated
        );
        assert!(
            !rows
                .iter()
                .find(|l| l.title == "indexing drop")
                .unwrap()
                .invalidated
        );
    }

    #[test]
    fn scan_contradictions_noops_on_thin_boilerplate_pairs() {
        // P2-6: both advices are very short / boilerplate-dominated → too few tokens
        // to judge → NOOP, even if they share the topic and overlap is low.
        let tmp = TempDir::new().unwrap();
        let older = mk_db_lesson("thin A", "fix it", "broke", "2026-06-01T00:00:00Z");
        let newer = mk_db_lesson("thin B", "redo it", "failed", "2026-06-20T00:00:00Z");
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &[older, newer]);
        assert_eq!(
            scan_contradictions(tmp.path()),
            0,
            "thin boilerplate advice must not be judged a contradiction"
        );
    }

    #[test]
    fn antonym_conflict_detects_opposing_verbs() {
        let a: std::collections::HashSet<String> = ["always", "add", "index"]
            .iter()
            .map(|s| (*s).into())
            .collect();
        let b: std::collections::HashSet<String> = ["avoid", "drop", "index"]
            .iter()
            .map(|s| (*s).into())
            .collect();
        assert!(antonym_conflict(&a, &b), "add ↔ drop is an opposing pair");
        // Agreement (no opposing pole) → no conflict.
        let c: std::collections::HashSet<String> = ["create", "composite", "index"]
            .iter()
            .map(|s| (*s).into())
            .collect();
        let d: std::collections::HashSet<String> = ["build", "multicolumn", "index"]
            .iter()
            .map(|s| (*s).into())
            .collect();
        assert!(
            !antonym_conflict(&c, &d),
            "agreeing advice carries no antonym"
        );
    }

    // ── Record-time efficacy-aware contradiction control ───────────────────────

    #[test]
    fn resolve_keeps_the_higher_efficacy_lesson_even_when_it_is_older() {
        // Efficacy — not age — decides the loser: an OLDER but PROVEN-helpful lesson
        // is KEPT above the cut, and the NEWER contradicting-but-unproven one is
        // demoted. (The historical scan would have culled the older one.)
        let tmp = TempDir::new().unwrap();
        let mut proven = mk_db_lesson(
            "indexing add",
            "always add a btree index on every filtered column for speed",
            "missing indexes slowed reads across the board significantly",
            "2026-06-01T00:00:00Z",
        );
        proven.trust = 0.9;
        proven.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            exact_helpful: 8,
            ..Default::default()
        });
        let unproven = mk_db_lesson(
            "indexing drop",
            "avoid over indexing and drop redundant indexes to keep writes fast",
            "too many indexes bloated writes and storage badly here",
            "2026-06-20T00:00:00Z",
        );
        write_raw_lessons(
            tmp.path(),
            "quality-failures.jsonl",
            &[proven, unproven.clone()],
        );

        // The unproven one is the NEW lesson being recorded → triggers resolution.
        let n = resolve_new_lesson_conflicts(tmp.path(), std::slice::from_ref(&unproven));
        assert_eq!(n, 1, "exactly the lower-efficacy side is demoted");
        let rows = read_raw_lessons(tmp.path(), "quality-failures.jsonl");
        assert!(
            rows.iter()
                .find(|l| l.title == "indexing drop")
                .unwrap()
                .invalidated,
            "the lower-efficacy (newer, unproven) lesson is demoted below recall"
        );
        assert!(
            !rows
                .iter()
                .find(|l| l.title == "indexing add")
                .unwrap()
                .invalidated,
            "the higher-efficacy (older, proven) lesson is KEPT above the cut"
        );
    }

    #[test]
    fn resolve_spares_non_contradicting_same_tech_lessons() {
        // Two lessons about the SAME tech that AGREE (no antonym) both survive — the
        // conservative guard: same subject is not enough, the guidance must oppose.
        let tmp = TempDir::new().unwrap();
        let older = mk_db_lesson(
            "indexing A",
            "create a composite covering index spanning the lookup columns",
            "sequential scans dominated the slow report query plan",
            "2026-06-01T00:00:00Z",
        );
        let newer = mk_db_lesson(
            "indexing B",
            "build a multicolumn btree so the planner reaches rows directly",
            "full table reads bottlenecked dashboard latency badly here",
            "2026-06-20T00:00:00Z",
        );
        write_raw_lessons(
            tmp.path(),
            "quality-failures.jsonl",
            &[older, newer.clone()],
        );
        assert_eq!(
            resolve_new_lesson_conflicts(tmp.path(), std::slice::from_ref(&newer)),
            0,
            "agreeing same-tech lessons are never demoted"
        );
        assert!(read_raw_lessons(tmp.path(), "quality-failures.jsonl")
            .iter()
            .all(|l| !l.invalidated));
    }

    #[test]
    fn resolve_is_fail_open_on_a_missing_store() {
        // A store that can't be read marks nothing and never panics.
        let missing = std::path::Path::new("/nonexistent/umadev/lessons/root/xyz");
        let l = mk_db_lesson(
            "indexing add",
            "always add a btree index on every filtered column for speed",
            "missing indexes slowed reads across the board significantly",
            "2026-06-01T00:00:00Z",
        );
        assert_eq!(
            resolve_new_lesson_conflicts(missing, std::slice::from_ref(&l)),
            0
        );
    }

    // ---- ③ trust score (asymmetric + folded into decay) ----------------

    #[test]
    fn trust_normalises_legacy_zero_to_neutral() {
        let tmp = TempDir::new().unwrap();
        let mut l = seed_cluster_one(tmp.path());
        l.trust = 0.0; // legacy / never-rated row
        assert!((l.trust() - NEUTRAL_TRUST).abs() < 1e-6, "0.0 → neutral");
        // A corrupt (NaN / negative) trust also maps to neutral (fail-open).
        l.trust = f32::NAN;
        assert!((l.trust() - NEUTRAL_TRUST).abs() < 1e-6);
        l.trust = -5.0;
        assert!((l.trust() - NEUTRAL_TRUST).abs() < 1e-6);
    }

    #[test]
    fn trust_feedback_is_asymmetric() {
        let tmp = TempDir::new().unwrap();
        let mut l = seed_cluster_one(tmp.path());
        l.trust = NEUTRAL_TRUST;
        // One pass: small reward.
        l.apply_trust_feedback(true);
        assert!((l.trust - (NEUTRAL_TRUST + TRUST_REWARD)).abs() < 1e-6);
        // One fail: larger penalty — the penalty must exceed the reward.
        let after_pass = l.trust;
        l.apply_trust_feedback(false);
        let pass_delta = after_pass - NEUTRAL_TRUST;
        let fail_delta = after_pass - l.trust;
        assert!(
            fail_delta > pass_delta,
            "fail penalty ({fail_delta}) must exceed pass reward ({pass_delta})"
        );
        // Trust never drops to exactly 0 (floor keeps it recoverable).
        for _ in 0..50 {
            l.apply_trust_feedback(false);
        }
        assert!(
            l.trust >= TRUST_FLOOR,
            "trust floored above zero: {}",
            l.trust
        );
        assert!(l.trust > 0.0);
    }

    #[test]
    fn trust_feedback_never_rewrites_first_observed_time() {
        let tmp = TempDir::new().unwrap();
        let mut l = seed_cluster_one(tmp.path());
        // Backdate so a refresh is observable.
        l.first_seen = "2020-01-01T00:00:00Z".to_string();
        let stale = l.first_seen.clone();
        // Neither outcome may rewrite the immutable first-observed audit time.
        l.apply_exact_trust_feedback(false);
        assert_eq!(l.first_seen, stale);
        l.apply_exact_trust_feedback(true);
        assert_eq!(l.first_seen, stale);
        assert!(l.last_reinforced_at().is_some());
    }

    #[test]
    fn trust_multiplies_into_decay_score() {
        let tmp = TempDir::new().unwrap();
        let _ = tmp;
        let base = seed_cluster_one_lesson();
        let mut trusted = base.clone();
        trusted.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            exact_helpful: 10,
            ..PitfallEfficacy::default()
        });
        let mut distrusted = base.clone();
        distrusted.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            exact_harmful: 10,
            ..PitfallEfficacy::default()
        });
        let q: std::collections::HashSet<String> = ["color", "token"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let now = Utc::now();
        let st = lesson_decay_score(&trusted, &q, now);
        let sd = lesson_decay_score(&distrusted, &q, now);
        assert!(
            st > sd,
            "a trusted lesson must outscore a distrusted one: {st} vs {sd}"
        );
        // Both exact-token axes are intentionally multiplicative: smoothed
        // trust and the discrete helpful ratio. Verify the composed factor
        // rather than pretending the same exact counters affect only trust.
        let ratio = st / sd;
        let expected = (f64::from(trusted.trust()) / f64::from(distrusted.trust()))
            * (efficacy_weight(&trusted) / efficacy_weight(&distrusted));
        assert!(
            (ratio - expected).abs() < 1e-3,
            "decay composes trust and exact efficacy multiplicatively: ratio {ratio} ~ {expected}"
        );
    }

    #[test]
    fn explicit_evidence_replay_is_idempotent_and_never_corroborates() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        let first = capture_dev_errors_detailed_with_evidence_id(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "requirement",
            "host-turn-42",
        );
        let replay = capture_dev_errors_detailed_with_evidence_id(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "requirement",
            "host-turn-42",
        );
        assert_eq!(first.observations, 1);
        assert_eq!(replay, PitfallCaptureOutcome::default());
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert_eq!(row.hits(), 1);
        assert_eq!(row.knowledge_evidence_count(), 1);
        assert_eq!(row.pitfall_status(), PitfallStatus::Hypothesis);
    }

    #[test]
    fn independent_observations_corroborate_but_cannot_validate() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        for evidence_id in ["host-turn-a", "host-turn-b"] {
            let outcome = capture_dev_errors_detailed_with_evidence_id(
                tmp.path(),
                std::slice::from_ref(&error),
                "demo",
                "requirement",
                evidence_id,
            );
            assert_eq!(outcome.observations, 1);
        }
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert_eq!(row.hits(), 2);
        assert_eq!(row.knowledge_evidence_count(), 2);
        assert_eq!(row.pitfall_status(), PitfallStatus::Corroborated);
        assert_eq!(row.last_verified_at(), None);
    }

    #[test]
    fn conflicting_payload_for_one_evidence_id_invalidates_without_inflation() {
        let tmp = TempDir::new().unwrap();
        let first = "Error: Cannot find module 'lodash'\nrequired from module-a".to_string();
        let conflicting = "Error: Cannot find module 'lodash'\nrequired from module-b".to_string();
        let _ = capture_dev_errors_detailed_with_evidence_id(
            tmp.path(),
            &[first],
            "demo",
            "requirement",
            "reused-evidence-id",
        );
        let second = capture_dev_errors_detailed_with_evidence_id(
            tmp.path(),
            &[conflicting],
            "demo",
            "requirement",
            "reused-evidence-id",
        );
        assert_eq!(second, PitfallCaptureOutcome::default());
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert_eq!(row.hits(), 1);
        assert_eq!(row.knowledge_evidence_count(), 1);
        assert_eq!(row.corroborating_evidence_count(), 0);
        assert_eq!(row.pitfall_status(), PitfallStatus::Invalidated);
        assert_eq!(
            row.efficacy.unwrap().recent_observations[0].outcome,
            KnowledgeEvidenceOutcome::Conflict
        );
    }

    #[test]
    fn legacy_aggregate_hits_are_history_not_independent_validation() {
        let tmp = TempDir::new().unwrap();
        let legacy = mk_pitfall(
            "dependency/module-not-found/lodash",
            "install lodash",
            "dependency missing",
            "2025-01-01T00:00:00Z",
            226,
        );
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[legacy]);
        let report = lessons_report(tmp.path());
        assert_eq!(report.incidents[0].hits, 226);
        assert_eq!(report.incidents[0].recent_evidence_count, 0);
        assert_eq!(report.incidents[0].status, PitfallStatus::Hypothesis);
        assert!(!report.incidents[0].timeline_complete);
        let overview = pitfall_overview(tmp.path());
        assert!(overview.contains("历史命中 226 次"));
        assert!(overview.contains("可审计证据 0 条"));
        assert!(!overview.contains("226 次 · 可审计证据 226"));
    }

    #[test]
    fn causal_success_validates_and_newer_causal_failure_revokes() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'";
        let _ = capture_dev_errors_detailed_with_evidence_id(
            tmp.path(),
            &[error.to_string()],
            "demo",
            "requirement",
            "failure-before-repair",
        );
        let passed = commit_pitfall_fix_attempt(tmp.path(), error).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(tmp.path(), &passed, PitfallFixAttemptResult::Passed),
            PitfallFixSettlement::Passed
        );
        let validated = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert_eq!(validated.pitfall_status(), PitfallStatus::Validated);
        let success = validated
            .efficacy
            .as_ref()
            .unwrap()
            .recent_observations
            .iter()
            .find(|evidence| evidence.outcome == KnowledgeEvidenceOutcome::FixSucceeded)
            .unwrap();
        assert!(evidence_has_complete_causal_provenance(success));
        assert_eq!(success.causal_attempt_id, passed);
        assert!(success.workspace_scope.starts_with("project:"));

        let failed = commit_pitfall_fix_attempt(tmp.path(), error).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &failed,
                PitfallFixAttemptResult::VerificationFailed(error.to_string()),
            ),
            PitfallFixSettlement::SameSignatureFailed
        );
        assert_eq!(
            read_raw_lessons(tmp.path(), DEV_ERRORS_FILE)[0].pitfall_status(),
            PitfallStatus::Invalidated
        );

        let recovered = commit_pitfall_fix_attempt(tmp.path(), error).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(tmp.path(), &recovered, PitfallFixAttemptResult::Passed),
            PitfallFixSettlement::Passed
        );
        assert_eq!(
            read_raw_lessons(tmp.path(), DEV_ERRORS_FILE)[0].pitfall_status(),
            PitfallStatus::Validated
        );
    }

    #[test]
    fn legacy_signature_feedback_is_retained_but_behaviorally_neutral() {
        let tmp = TempDir::new().unwrap();
        let err = vec!["Error: Cannot find module 'lodash'".to_string()];
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        let sig = "dependency/module-not-found/lodash";
        let trust_of = |t: &std::path::Path| {
            read_raw_lessons(t, DEV_ERRORS_FILE)
                .into_iter()
                .find(|l| l.signature == sig)
                .map(|l| l.trust())
        };
        let start = trust_of(tmp.path()).unwrap();
        // Historical broad signature APIs still preserve their audit counters,
        // but cannot affect behavior without exact attempt attribution v1.
        assert_eq!(apply_dev_error_trust(tmp.path(), &err, true), 1);
        apply_dev_error_trust(tmp.path(), &err, false);
        apply_dev_error_trust(tmp.path(), &err, false);
        assert!((trust_of(tmp.path()).unwrap() - start).abs() < f32::EPSILON);
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        let efficacy = row.efficacy.unwrap();
        assert_eq!(efficacy.outcome_attribution_version, 0);
        assert_eq!((efficacy.helpful, efficacy.harmful), (1, 2));
        // Unrecognised error → no-op.
        assert_eq!(
            apply_dev_error_trust(tmp.path(), &["vague noise".to_string()], true),
            0
        );
    }

    #[test]
    fn legacy_broad_outcomes_are_behaviorally_neutral() {
        let mut lesson = seed_cluster_one_lesson();
        lesson.trust = TRUST_FLOOR;
        lesson.efficacy = Some(PitfallEfficacy {
            injected: 99,
            occ_at_injection: 1,
            recurred_after_warning: true,
            proven_fix: true,
            failed_fixes: vec!["legacy broad fix".into()],
            next_strategy: "legacy broad strategy".into(),
            helpful: 1,
            harmful: 9,
            last_fix_failed_at: "2026-01-03T00:00:00Z".into(),
            last_verified_at: "2026-01-04T00:00:00Z".into(),
            last_reinforced_at: "2026-01-04T00:00:00Z".into(),
            ..PitfallEfficacy::default()
        });

        assert!((lesson.trust() - NEUTRAL_TRUST).abs() < f32::EPSILON);
        assert_eq!(lesson.efficacy_samples(), 0);
        assert_eq!(lesson.helpful_ratio(), None);
        assert!(!is_efficacy_poison(&lesson));
        assert_eq!(lesson.pitfall_status(), PitfallStatus::Active);
        assert_eq!(lesson.last_verified_at(), None);
        assert_eq!(lesson.last_reinforced_at(), None);
    }

    #[test]
    fn first_exact_settlement_starts_a_clean_outcome_lineage() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'";
        let mut legacy = mk_pitfall(
            "dependency/module-not-found/lodash",
            "install lodash",
            "missing dependency",
            "2026-01-01T00:00:00Z",
            2,
        );
        legacy.trust = TRUST_FLOOR;
        legacy.efficacy = Some(PitfallEfficacy {
            recurred_after_warning: true,
            proven_fix: true,
            failed_fixes: vec!["legacy broad fix".into()],
            next_strategy: "legacy broad strategy".into(),
            helpful: 3,
            harmful: 12,
            last_fix_failed_at: "2026-01-03T00:00:00Z".into(),
            last_verified_at: "2026-01-04T00:00:00Z".into(),
            last_reinforced_at: "2026-01-04T00:00:00Z".into(),
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[legacy]);

        let token = commit_pitfall_fix_attempt(tmp.path(), error).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &token,
                PitfallFixAttemptResult::VerificationFailed(error.into()),
            ),
            PitfallFixSettlement::SameSignatureFailed
        );

        let lesson = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        let efficacy = lesson.efficacy.as_ref().unwrap();
        assert_eq!(efficacy.outcome_attribution_version, 1);
        assert_eq!((efficacy.exact_helpful, efficacy.exact_harmful), (0, 1));
        assert_eq!(lesson.efficacy_samples(), 1);
        assert!((lesson.trust() - (NEUTRAL_TRUST - TRUST_PENALTY)).abs() < f32::EPSILON);
        assert_eq!(lesson.pitfall_status(), PitfallStatus::Recurring);
        assert_eq!(efficacy.failed_fixes, vec!["install lodash"]);
        assert!(efficacy.next_strategy.is_empty());
        // Compatibility fields remain an audit trail but did not seed v1.
        assert_eq!((efficacy.helpful, efficacy.harmful), (3, 12));
    }

    #[test]
    fn canonical_merge_does_not_mix_legacy_outcomes_into_exact_behavior() {
        let tmp = TempDir::new().unwrap();
        let sig = "dependency/module-not-found/lodash";
        let mut legacy = mk_pitfall(sig, "legacy fix", "root", "2026-01-01Z", 9);
        legacy.trust = TRUST_FLOOR;
        legacy.efficacy = Some(PitfallEfficacy {
            recurred_after_warning: true,
            proven_fix: true,
            failed_fixes: vec!["legacy failed fix".into()],
            next_strategy: "legacy strategy".into(),
            helpful: 50,
            harmful: 50,
            last_fix_failed_at: "2099-01-01T00:00:00Z".into(),
            last_verified_at: "2099-01-02T00:00:00Z".into(),
            last_reinforced_at: "2099-01-02T00:00:00Z".into(),
            ..PitfallEfficacy::default()
        });
        let mut exact = mk_pitfall(sig, "exact fix", "root", "2026-01-02Z", 2);
        exact.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            exact_helpful: 2,
            exact_harmful: 1,
            exact_last_verified_at: "2026-01-03T00:00:00Z".into(),
            exact_last_reinforced_at: "2026-01-03T00:00:00Z".into(),
            failed_fixes: vec!["exact failed fix".into()],
            recent_observations: vec![test_evidence(
                "exact-success",
                "2026-01-03T00:00:00Z",
                KnowledgeEvidenceOutcome::FixSucceeded,
            )],
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[legacy, exact]);

        let merged = read_canonical_pitfalls(tmp.path()).remove(0);
        let efficacy = merged.efficacy.as_ref().unwrap();
        assert_eq!(efficacy.outcome_attribution_version, 1);
        assert_eq!((efficacy.exact_helpful, efficacy.exact_harmful), (2, 1));
        assert_eq!(merged.efficacy_samples(), 3);
        assert_eq!(merged.pitfall_status(), PitfallStatus::Validated);
        assert_eq!(merged.last_verified_at(), Some("2026-01-03T00:00:00Z"));
        assert_eq!(efficacy.failed_fixes, vec!["exact failed fix"]);
        assert!(efficacy.next_strategy.is_empty());
    }

    /// One persisted Failure lesson, returned by value for in-memory trust tests.
    fn seed_cluster_one(root: &Path) -> Lesson {
        seed_cluster(root, 1, &["color", "token"], "frontend");
        read_raw_lessons(root, "quality-failures.jsonl")
            .into_iter()
            .next()
            .unwrap()
    }

    /// A standalone in-memory Failure lesson (no disk) for pure scoring tests.
    fn seed_cluster_one_lesson() -> Lesson {
        Lesson {
            kind: LessonKind::Failure,
            domain: "frontend".into(),
            title: "t".into(),
            body: String::new(),
            fix: "use color tokens".into(),
            root_cause: String::new(),
            keywords: vec!["color".into(), "token".into()],
            source_requirement: "r".into(),
            first_seen: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: None,
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        }
    }

    // ── Efficacy loop: outcomes earn/lose a lesson's place in recall ──

    /// A disk-free Failure lesson carrying an OUTCOME tally, keyword `login` so a
    /// "login system" query matches it.
    fn mk_failure_eff(title: &str, helpful: u32, harmful: u32) -> Lesson {
        Lesson {
            kind: LessonKind::Failure,
            domain: "frontend".into(),
            title: title.into(),
            body: String::new(),
            fix: "apply the recorded fix".into(),
            root_cause: String::new(),
            keywords: vec!["login".into()],
            source_requirement: "login system".into(),
            first_seen: "2026-06-25T00:00:00Z".into(),
            signature: String::new(),
            occurrences: 1,
            context: Vec::new(),
            efficacy: Some(PitfallEfficacy {
                outcome_attribution_version: 1,
                exact_helpful: helpful,
                exact_harmful: harmful,
                ..PitfallEfficacy::default()
            }),
            invalidated: false,
            trust: NEUTRAL_TRUST,
            evidence_count: 0,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn recall_outcome_moves_the_helpful_harmful_tally() {
        // Only a committed repair turn can move outcome efficacy.
        let tmp = TempDir::new().unwrap();
        let err = vec!["Error: Cannot find module 'lodash'".to_string()];
        capture_dev_errors(tmp.path(), &err, "demo", "需求");
        let sig = "dependency/module-not-found/lodash";
        let eff_of = |t: &std::path::Path| {
            read_raw_lessons(t, DEV_ERRORS_FILE)
                .into_iter()
                .find(|l| l.signature == sig)
                .and_then(|l| l.efficacy)
        };
        let passed_attempt = commit_pitfall_fix_attempt(tmp.path(), &err[0]).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &passed_attempt,
                PitfallFixAttemptResult::Passed
            ),
            PitfallFixSettlement::Passed
        );
        let e = eff_of(tmp.path()).unwrap();
        assert_eq!(e.outcome_attribution_version, 1);
        assert_eq!(
            (e.exact_helpful, e.exact_harmful),
            (1, 0),
            "a token-settled pass increments exact helpful"
        );
        let failed_attempt = commit_pitfall_fix_attempt(tmp.path(), &err[0]).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &failed_attempt,
                PitfallFixAttemptResult::VerificationFailed(err[0].clone())
            ),
            PitfallFixSettlement::SameSignatureFailed
        );
        let e = eff_of(tmp.path()).unwrap();
        assert_eq!(
            (e.exact_helpful, e.exact_harmful),
            (1, 1),
            "a same-signature token failure increments exact harmful"
        );
    }

    #[test]
    fn explicitly_committed_nonpitfall_identity_moves_the_tally() {
        // The low-level identity seam remains available to a caller that can prove
        // which exact non-pitfall row reached a real turn. Passive recall itself
        // deliberately writes no global "last surfaced" snapshot.
        let tmp = TempDir::new().unwrap();
        let req = "做一个登录系统";
        capture_quality_failures(tmp.path(), &[check("coverage", "failed", 20)], "demo", req);
        let row = read_raw_lessons(tmp.path(), "quality-failures.jsonl")
            .into_iter()
            .next()
            .unwrap();
        let ids = vec![lesson_identity(&row)];
        apply_trust_for_identities(tmp.path(), &ids, true);
        let eff = read_raw_lessons(tmp.path(), "quality-failures.jsonl")
            .into_iter()
            .next()
            .and_then(|l| l.efficacy)
            .expect("outcome feedback created an efficacy record");
        assert_eq!((eff.helpful, eff.harmful), (1, 0));
    }

    #[test]
    fn efficacy_ranks_proven_helpful_above_proven_unhelpful_of_equal_relevance() {
        // Step 2: two lessons equal on every OTHER axis (relevance / recency /
        // importance / trust) must be ordered by outcome efficacy — proven-helpful
        // above proven-unhelpful — with an un-sampled lesson sitting at neutral
        // between (un-sampled behaviour is unchanged).
        let base = seed_cluster_one_lesson(); // Failure, efficacy None, neutral trust
        let q: std::collections::HashSet<String> = ["color", "token"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let now = Utc::now();
        let mut high = base.clone();
        high.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            exact_helpful: 4,
            ..PitfallEfficacy::default()
        });
        let mut low = base.clone();
        low.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            exact_helpful: 1,
            exact_harmful: 2, // ratio 0.33, samples 3 < floor → ranked low but NOT pruned
            ..PitfallEfficacy::default()
        });
        let sh = lesson_decay_score(&high, &q, now);
        let su = lesson_decay_score(&base, &q, now);
        let sl = lesson_decay_score(&low, &q, now);
        assert!(
            sh > su && su > sl,
            "proven-helpful > un-sampled(neutral) > proven-unhelpful: {sh} > {su} > {sl}"
        );
    }

    #[test]
    fn efficacy_poison_is_pruned_from_recall_after_min_samples() {
        // Step 3: a well-sampled poison lesson is demoted OUT of recall while a
        // healthy one of equal relevance survives.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_raw_lessons(
            root,
            "quality-failures.jsonl",
            &[
                mk_failure_eff("HEALTHY-KEEP", 5, 0),
                mk_failure_eff("POISON-DROP", 0, 5),
            ],
        );
        let titles: Vec<String> = relevant_lessons_for_prompt_ranked(root, "login system")
            .into_iter()
            .map(|(_, l)| l.title)
            .collect();
        assert!(
            titles.iter().any(|t| t == "HEALTHY-KEEP"),
            "the healthy lesson still surfaces: {titles:?}"
        );
        assert!(
            !titles.iter().any(|t| t == "POISON-DROP"),
            "the poison lesson is pruned below the recall cut: {titles:?}"
        );
    }

    #[test]
    fn single_observation_never_prunes() {
        // The prune gate is sample-size gated: one (or a few, below the floor) bad
        // outcome NEVER culls a lesson — only a well-sampled poison ratio does.
        assert!(
            !is_efficacy_poison(&mk_failure_eff("thin", 0, 1)),
            "a single bad outcome never prunes"
        );
        assert!(
            !is_efficacy_poison(&mk_failure_eff("few", 0, 3)),
            "below the sample floor never prunes"
        );
        assert!(
            !is_efficacy_poison(&mk_failure_eff("mixed", 2, 2)),
            "a 50% ratio at the floor is not poison"
        );
        assert!(
            is_efficacy_poison(&mk_failure_eff("poison", 0, 4)),
            "all-harmful at the sample floor is poison"
        );
        assert!(
            is_efficacy_poison(&mk_failure_eff("mostly-bad", 1, 5)),
            "helpful ratio <= 0.25 with enough samples is poison"
        );
    }

    #[test]
    fn lessons_for_error_abstains_on_a_poison_pitfall_but_not_a_thin_one() {
        // The at-failure recall prunes poison too: a proven-bad fix stops being
        // re-injected (abstain), but a thinly-sampled one still surfaces.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let err = "Error: Cannot find module 'lodash'";
        let mut thin = mk_pitfall(
            "dependency/module-not-found/lodash",
            "install lodash",
            "missing dep",
            "2026-06-21T00:00:00Z",
            2,
        );
        thin.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            exact_harmful: 1,
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(root, DEV_ERRORS_FILE, std::slice::from_ref(&thin));
        assert!(
            !lessons_for_error(root, err).is_empty(),
            "a thinly-sampled pitfall still surfaces"
        );
        let mut poison = thin.clone();
        poison.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            exact_harmful: 5,
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(root, DEV_ERRORS_FILE, std::slice::from_ref(&poison));
        assert!(
            lessons_for_error(root, err).is_empty(),
            "a well-sampled poison pitfall is pruned → abstain"
        );
    }

    #[test]
    fn efficacy_recall_is_fail_open_on_a_corrupt_store() {
        // The poison filter + efficacy ranking must never panic on an unreadable /
        // corrupt store — fail-open to an empty recall.
        let tmp = TempDir::new().unwrap();
        let raw = tmp.path().join(RAW_DIR);
        std::fs::create_dir_all(&raw).unwrap();
        std::fs::write(raw.join("quality-failures.jsonl"), "not json\n{oops").unwrap();
        assert!(relevant_lessons_for_prompt(tmp.path(), "anything").is_empty());
        assert!(relevant_lessons_for_prompt_ranked(tmp.path(), "anything").is_empty());
        assert!(lessons_for_error(tmp.path(), "Error: Cannot find module 'lodash'").is_empty());
    }

    #[test]
    fn capture_counts_independent_events_but_dedupes_one_event_and_announces_rule() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();

        // One failed event may repeat stderr/summary text. It is still one
        // observation and one newly-created incident.
        let first = capture_dev_errors_detailed(
            tmp.path(),
            &[error.clone(), error.clone()],
            "demo",
            "requirement",
        );
        assert_eq!(
            first,
            PitfallCaptureOutcome {
                new_incidents: 1,
                recurrent_incidents: 0,
                newly_curated_rules: 0,
                new_unclassified_candidates: 0,
                recurrent_unclassified_candidates: 0,
                observations: 1,
            }
        );

        // A second independently-executed event is a recurrence and crosses the
        // two-evidence threshold into a visible corroborated reusable rule.
        let second = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "requirement",
        );
        assert_eq!(second.recurrent_incidents, 1);
        assert_eq!(second.newly_curated_rules, 1);
        assert!(second
            .progress_notes()
            .iter()
            .any(|note| note.contains("形成已印证经验规则")));

        let pitfall = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(pitfall.hits(), 2);
        let observations = &pitfall.efficacy.as_ref().unwrap().recent_observations;
        assert_eq!(observations.len(), 2);
        assert_ne!(
            observations[0].episode_id, observations[1].episode_id,
            "two independently captured failures need distinct episode ids"
        );
        let report = lessons_report(tmp.path());
        assert_eq!(report.curated_lessons.len(), 1);
        assert_eq!(report.curated_lessons[0].evidence_count, 2);
        assert_eq!(
            report.curated_lessons[0].status,
            CuratedLessonStatus::Corroborated
        );
        assert!(report.curated_lessons[0].timeline_complete);
    }

    #[test]
    fn capture_does_not_announce_learning_when_atomic_commit_cannot_start() {
        let tmp = TempDir::new().unwrap();
        let root_is_file = tmp.path().join("not-a-directory");
        std::fs::write(&root_is_file, "blocks .umadev directory creation").unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();

        let outcome = capture_dev_errors_detailed(&root_is_file, &[error], "demo", "requirement");
        assert_eq!(outcome, PitfallCaptureOutcome::default());
        assert!(outcome.progress_notes().is_empty());
        assert!(read_raw_lessons(&root_is_file, DEV_ERRORS_FILE).is_empty());
    }

    #[test]
    fn observation_tail_is_bounded_without_losing_lifetime_hits() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        for _ in 0..(MAX_RECENT_OBSERVATIONS + 3) {
            let _ = capture_dev_errors_detailed(
                tmp.path(),
                std::slice::from_ref(&error),
                "demo",
                "requirement",
            );
        }
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert_eq!(row.hits() as usize, MAX_RECENT_OBSERVATIONS + 3);
        assert_eq!(
            row.efficacy.unwrap().recent_observations.len(),
            MAX_RECENT_OBSERVATIONS
        );
        assert!(!lessons_report(tmp.path()).top_pitfalls[0].timeline_complete);
    }

    #[test]
    fn legacy_generic_226_is_quarantined_not_actionable() {
        let _home = TempHome::new();
        let tmp = TempDir::new().unwrap();
        let mut generic = mk_pitfall(
            "general/error/failed",
            "read the log",
            "unknown",
            "2026-01-01T00:00:00Z",
            226,
        );
        generic.domain = "general".into();
        generic.title = "开发错误: failed".into();
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[generic]);

        let summary = pitfall_efficacy_summary(tmp.path());
        assert_eq!(summary.total, 0);
        assert_eq!(summary.quarantined_records, 1);
        assert_eq!(summary.quarantined_hits, 226);
        let report = lessons_report(tmp.path());
        assert!(report.is_empty());
        assert!(!report.has_incidents());
        assert!(report.top_pitfalls.is_empty());
        assert!(relevant_lessons_for_prompt(tmp.path(), "failed").is_empty());
        assert!(lessons_for_error(tmp.path(), "failed").is_empty());
        let overview = pitfall_overview(tmp.path());
        assert!(overview.contains("旧计数 226 次"));
        assert!(!overview.contains("开发错误: failed"));

        // A legacy auto-generated generic/unsafe global chunk is removed on the
        // first sediment pass, while the raw audit row remains intact.
        let global = global_learned_dir().unwrap().join("general");
        fs::create_dir_all(&global).unwrap();
        let unsafe_md = global.join("legacy.md");
        fs::write(
            &unsafe_md,
            "---\nmaintainer: auto-sediment\n---\n# [pitfall] Dev error: x\ngeneral/error/failed\n",
        )
        .unwrap();
        assert_eq!(sediment_lessons(tmp.path()), 0);
        assert!(!unsafe_md.exists());
        assert_eq!(read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).len(), 1);
    }

    #[test]
    fn repeated_unknown_error_becomes_visible_safe_candidate_not_advice() {
        let _home = TempHome::new();
        let tmp = TempDir::new().unwrap();
        let unknown =
            "frobnicator failed while applying quux protocol at /private/acme/42".to_string();
        let first = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&unknown),
            "secret-slug",
            "secret requirement",
        );
        assert_eq!(first.new_unclassified_candidates, 1);
        let second = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&unknown),
            "secret-slug",
            "secret requirement",
        );
        assert_eq!(second.recurrent_unclassified_candidates, 1);
        assert!(second
            .progress_notes()
            .iter()
            .any(|note| note.contains("仍需分类")));

        let summary = pitfall_efficacy_summary(tmp.path());
        assert_eq!(summary.total, 0);
        assert_eq!(summary.quarantined_records, 0);
        assert_eq!(summary.unclassified_candidates, 1);
        assert_eq!(summary.unclassified_candidate_hits, 2);
        let report = lessons_report(tmp.path());
        assert!(report.is_empty());
        assert!(!report.has_incidents());
        assert!(report.has_unclassified_candidates());
        assert_eq!(report.unclassified_candidates.len(), 1);
        let candidate = &report.unclassified_candidates[0];
        assert_eq!(candidate.hits, 2);
        assert!(candidate.last_observed_at.is_some());
        assert!(candidate.timeline_complete);
        assert_eq!(candidate.recent_observations.len(), 2);
        assert!(candidate
            .recent_observations
            .iter()
            .all(|observation| !observation.observed_at.is_empty()));
        assert!(report.top_pitfalls.is_empty());
        assert!(lessons_for_error(tmp.path(), &unknown).is_empty());
        assert!(relevant_lessons_for_prompt(tmp.path(), &unknown).is_empty());
        assert!(pitfall_overview(tmp.path()).contains("需分类审核"));
        assert_eq!(sediment_lessons(tmp.path()), 0);

        let raw = fs::read_to_string(tmp.path().join(RAW_DIR).join(DEV_ERRORS_FILE)).unwrap();
        assert!(!raw.contains("/private/acme"));
        assert!(!raw.contains("secret requirement"));
        assert!(!raw.contains("frobnicator failed"));
        // `list_sedimented_lessons` intentionally merges project and global rules.
        // This candidate must create no project-local advice; unrelated global rules
        // are outside the assertion and may be populated by parallel tests.
        let mut project_files = Vec::new();
        collect_md_files(&tmp.path().join(LEARNED_DIR), &mut project_files, 0);
        assert!(project_files.is_empty());
    }

    #[test]
    fn unknown_candidate_keeps_distinct_paths_without_persisting_them() {
        let tmp = TempDir::new().unwrap();
        let foo_42 = "frobnicator failed on GET /private/acme/foo/42".to_string();
        let foo_99 = "frobnicator failed on GET /private/acme/foo/99".to_string();
        let bar_42 = "frobnicator failed on GET /private/acme/bar/42".to_string();

        let _ = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&foo_42),
            "demo",
            "requirement",
        );
        let foo_again = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&foo_99),
            "demo",
            "requirement",
        );
        let different_path = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&bar_42),
            "demo",
            "requirement",
        );

        assert_eq!(foo_again.recurrent_unclassified_candidates, 1);
        assert_eq!(different_path.new_unclassified_candidates, 1);
        let summary = pitfall_efficacy_summary(tmp.path());
        assert_eq!(summary.unclassified_candidates, 2);
        assert_eq!(summary.unclassified_candidate_hits, 3);
        let raw = fs::read_to_string(tmp.path().join(RAW_DIR).join(DEV_ERRORS_FILE)).unwrap();
        assert!(!raw.contains("/private/acme/foo"));
        assert!(!raw.contains("/private/acme/bar"));
    }

    #[test]
    fn newly_recognized_error_migrates_matching_candidate_evidence() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'";
        let candidate_signature = unclassified_candidate_signature(error);
        let mut candidate = mk_pitfall(&candidate_signature, "", "", "2026-01-01T00:00:00.000Z", 2);
        candidate.domain = "general".into();
        candidate.efficacy = Some(PitfallEfficacy {
            last_recurred_at: "2026-01-02T00:00:00.000Z".into(),
            recent_observations: vec![
                PitfallObservation {
                    observed_at: "2026-01-01T00:00:00.000Z".into(),
                    episode_id: "candidate-1".into(),
                    evidence_hash: "hash-1".into(),
                    ..PitfallObservation::default()
                },
                PitfallObservation {
                    observed_at: "2026-01-02T00:00:00.000Z".into(),
                    episode_id: "candidate-2".into(),
                    evidence_hash: "hash-2".into(),
                    ..PitfallObservation::default()
                },
            ],
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[candidate]);

        let outcome = capture_dev_errors_detailed(tmp.path(), &[error.into()], "demo", "r");
        assert_eq!(outcome.new_incidents, 1);
        assert_eq!(outcome.newly_curated_rules, 1);
        let rows = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        assert!(rows
            .iter()
            .any(|row| row.signature == candidate_signature && row.invalidated));
        let precise = rows
            .iter()
            .find(|row| row.signature == "dependency/module-not-found/lodash")
            .unwrap();
        assert_eq!(precise.hits(), 3);
        assert_eq!(precise.first_seen, "2026-01-01T00:00:00.000Z");
        let entry = lessons_report(tmp.path()).top_pitfalls.remove(0);
        assert_eq!(entry.hits, 3);
        assert!(entry.last_observed_at.is_some());
        assert!(entry.timeline_complete);
    }

    #[test]
    fn unclassified_candidate_store_is_bounded_without_deleting_legacy_generic_audit() {
        let mut rows: Vec<Lesson> = (0..(MAX_UNCLASSIFIED_CANDIDATES + 9))
            .map(|index| {
                mk_pitfall(
                    &format!("general/candidate/{index:020x}"),
                    "",
                    "",
                    "2026-01-01T00:00:00Z",
                    1,
                )
            })
            .collect();
        rows.push(mk_pitfall(
            "general/error/failed",
            "",
            "",
            "2025-01-01T00:00:00Z",
            226,
        ));
        prune_pitfalls(&mut rows);
        assert_eq!(
            rows.iter()
                .filter(|row| row.signature.starts_with("general/candidate/"))
                .count(),
            MAX_UNCLASSIFIED_CANDIDATES
        );
        assert!(rows
            .iter()
            .any(|row| row.signature == "general/error/failed" && row.hits() == 226));
    }

    #[test]
    fn legacy_json_defaults_new_timeline_and_attempt_fields() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(RAW_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(DEV_ERRORS_FILE),
            r#"{"kind":"dev_error","domain":"dependency","title":"old","body":"b","fix":"f","root_cause":"r","keywords":[],"source_requirement":"q","first_seen":"2025-01-01T00:00:00Z"}
"#,
        )
        .unwrap();
        let rows = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hits(), 1);
        assert!(rows[0].efficacy.is_none());
        assert!(!rows[0].invalidated);
    }

    #[test]
    fn corrupt_raw_lines_survive_unrelated_read_modify_write() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(RAW_DIR);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DEV_ERRORS_FILE);
        fs::write(&path, "{legacy-corrupt-audit-line\n").unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        capture_dev_errors(tmp.path(), &[error], "demo", "requirement");
        let raw = fs::read_to_string(path).unwrap();
        assert!(raw.contains("{legacy-corrupt-audit-line"));
        assert_eq!(read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn global_learned_dir_supports_a_symlinked_home_boundary() {
        use std::os::unix::fs::symlink;

        let _home = TempHome::new();
        let real_home = home_dir().unwrap();
        let link_parent = TempDir::new().unwrap();
        let linked_home = link_parent.path().join("linked-home");
        symlink(&real_home, &linked_home).unwrap();
        set_test_home_override(Some(linked_home.clone()));

        let learned = global_learned_dir().unwrap();
        assert_eq!(
            learned,
            fs::canonicalize(real_home)
                .unwrap()
                .join(GLOBAL_LEARNED_DIRNAME)
        );
        assert!(real_dir_no_follow(&learned));
    }

    #[cfg(unix)]
    #[test]
    fn global_learned_dir_rejects_symlinked_managed_components() {
        use std::os::unix::fs::symlink;

        let _home = TempHome::new();
        let home = home_dir().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_umadev = outside.path().join("external-umadev");
        let outside_learned = outside_umadev.join("learned/general");
        fs::create_dir_all(&outside_learned).unwrap();
        let legacy = outside_learned.join("legacy.md");
        fs::write(
            &legacy,
            "---\nmaintainer: auto-sediment\n---\n# private legacy\n",
        )
        .unwrap();

        // Reject a symlinked `.umadev` ancestor without traversing or cleaning it.
        symlink(&outside_umadev, home.join(".umadev")).unwrap();
        assert!(global_learned_dir().is_none());
        assert!(legacy.exists());
        fs::remove_file(home.join(".umadev")).unwrap();

        // The final `learned` component is equally untrusted.
        fs::create_dir(home.join(".umadev")).unwrap();
        symlink(outside_umadev.join("learned"), home.join(".umadev/learned")).unwrap();
        assert!(global_learned_dir().is_none());
        assert!(legacy.exists());
        assert!(!home.join(".umadev/quarantine").exists());
    }

    #[test]
    fn quarantine_commit_never_removes_a_concurrent_replacement() {
        let _home = TempHome::new();
        let home = home_dir().unwrap();
        let root = home.join(GLOBAL_LEARNED_DIRNAME);
        let domain = root.join("general");
        fs::create_dir_all(&domain).unwrap();
        let source = domain.join("legacy.md");
        let legacy = "---\nmaintainer: auto-sediment\n---\n# legacy private body\n";
        let replacement = "---\nmaintainer: auto-sediment\nglobal_safety: classifier-family-v2\n---\n# current safe body\n";
        fs::write(&source, legacy).unwrap();

        let mut hook_calls = 0usize;
        quarantine_unsafe_global_sediments_with(&root, |original| {
            hook_calls += 1;
            assert_eq!(original, source);
            fs::write(original, replacement).unwrap();
        });

        assert_eq!(hook_calls, 1);
        assert_eq!(fs::read_to_string(&source).unwrap(), replacement);
        let quarantine = home.join(".umadev/quarantine/learned");
        let quarantined = fs::read_dir(quarantine)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(quarantined.len(), 1);
        assert_eq!(fs::read_to_string(&quarantined[0]).unwrap(), legacy);
        assert!(fs::read_dir(domain).unwrap().flatten().all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .contains("quarantine-pending")));
    }

    #[test]
    fn quarantine_scan_does_not_load_or_move_oversized_markdown() {
        let _home = TempHome::new();
        let home = home_dir().unwrap();
        let root = home.join(GLOBAL_LEARNED_DIRNAME);
        let domain = root.join("general");
        fs::create_dir_all(&domain).unwrap();
        let source = domain.join("oversized.md");
        fs::write(&source, "---\nmaintainer: auto-sediment\n---\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len((MAX_SEDIMENT_MARKDOWN_BYTES + 1) as u64)
            .unwrap();

        quarantine_unsafe_global_sediments_at(&root);

        assert_eq!(
            fs::metadata(source).unwrap().len(),
            (MAX_SEDIMENT_MARKDOWN_BYTES + 1) as u64
        );
        assert!(!home.join(".umadev/quarantine").exists());
    }

    #[test]
    fn global_lesson_projection_honors_off_on_and_corrupt_policy() {
        let home = NoBundledCorpus::new();
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        capture_dev_errors(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "dependency lodash",
        );
        let attempt = commit_pitfall_fix_attempt(tmp.path(), &error).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(tmp.path(), &attempt, PitfallFixAttemptResult::Passed),
            PitfallFixSettlement::Passed
        );
        capture_dev_errors(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "dependency lodash",
        );

        crate::memory_control::update_capture(
            tmp.path(),
            MemoryScope::Global,
            Some(MemoryStore::GlobalLessonProjection),
            false,
        )
        .unwrap();
        assert!(
            sediment_lessons(tmp.path()) > 0,
            "project sediment is independent"
        );
        let global_root = home.home().join(GLOBAL_LEARNED_DIRNAME);
        let mut global_files = Vec::new();
        collect_md_files(&global_root, &mut global_files, 0);
        assert!(global_files.is_empty());

        crate::memory_control::update_capture(
            tmp.path(),
            MemoryScope::Global,
            Some(MemoryStore::GlobalLessonProjection),
            true,
        )
        .unwrap();
        assert!(sediment_lessons(tmp.path()) > 0);
        collect_md_files(&global_root, &mut global_files, 0);
        assert!(!global_files.is_empty());

        for file in global_files.drain(..) {
            std::fs::remove_file(file).unwrap();
        }
        std::fs::write(
            home.home().join(".umadev/memory/policy.toml"),
            "invalid = [toml",
        )
        .unwrap();
        assert!(
            sediment_lessons(tmp.path()) > 0,
            "project capture still proceeds"
        );
        collect_md_files(&global_root, &mut global_files, 0);
        assert!(
            global_files.is_empty(),
            "corrupt global policy disables projection"
        );
        assert!(!lessons_report(tmp.path()).is_empty());
    }

    #[test]
    fn global_promotion_omits_private_module_and_legacy_global_content() {
        let _home = TempHome::new();
        let tmp = TempDir::new().unwrap();
        let secret = "TOP_SECRET_TOKEN_123";
        let private_module = "@acme/private-payment-engine";
        let private_slug = "acme-private-payment-engine";
        let error = format!(
            "Error: Cannot find module '{private_module}' from /Users/alice/private/project ({secret})"
        );
        capture_dev_errors(
            tmp.path(),
            std::slice::from_ref(&error),
            "private-slug",
            "customer ACME secret",
        );
        capture_dev_errors(
            tmp.path(),
            std::slice::from_ref(&error),
            "private-slug",
            "customer ACME secret",
        );
        let raw_path = tmp.path().join(RAW_DIR).join(DEV_ERRORS_FILE);
        let raw = fs::read_to_string(raw_path).unwrap();
        assert!(!raw.contains(secret));
        assert!(!raw.contains("/Users/alice/private"));
        assert!(!raw.contains("customer ACME secret"));
        assert!(
            raw.contains(private_slug),
            "project-local identity is retained"
        );

        let global = global_learned_dir().unwrap();
        let legacy = global.join("dependency/legacy-v1.md");
        let hand_authored = global.join("notes/hand-authored.md");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::create_dir_all(hand_authored.parent().unwrap()).unwrap();
        fs::write(
            &legacy,
            format!(
                "---\nmaintainer: auto-sediment\nglobal_safety: classifier-only-v1\n---\n# Legacy\n\n{private_module}\n"
            ),
        )
        .unwrap();
        fs::write(
            &hand_authored,
            "# Human note\n\nThe body literally discusses `maintainer: auto-sediment` and must survive.",
        )
        .unwrap();

        assert!(sediment_lessons(tmp.path()) > 0);
        assert!(!legacy.exists(), "unsafe v1 global files are quarantined");
        assert!(
            hand_authored.exists(),
            "body text must never make a hand-authored note look generated"
        );
        let quarantine = global.parent().unwrap().join("quarantine/learned");
        let quarantined = fs::read_dir(quarantine)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(quarantined.len(), 1);
        assert!(fs::read_to_string(&quarantined[0])
            .unwrap()
            .contains(private_module));
        let mut global_files = Vec::new();
        collect_md_files(&global, &mut global_files, 0);
        assert!(!global_files.is_empty());
        for path in global_files {
            let body = fs::read_to_string(&path).unwrap();
            if path == hand_authored {
                continue;
            }
            assert!(!body.contains(secret));
            assert!(!body.contains("/Users/alice/private"));
            assert!(!body.contains("customer ACME secret"));
            assert!(!body.contains(private_module));
            assert!(!body.contains(private_slug));
            assert!(body.contains("global_safety: classifier-family-v2"));
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(!name.contains("acme"));
            assert!(!name.contains("payment"));
        }
    }

    #[test]
    fn malformed_raw_domain_cannot_escape_the_project_learned_directory() {
        let _home = TempHome::new();
        let tmp = TempDir::new().unwrap();
        let mut lesson = mk_pitfall(
            "dependency/module-not-found/private",
            "install dependency",
            "dependency missing",
            "2026-01-01T00:00:00Z",
            2,
        );
        lesson.domain = "../../escaped".into();
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[lesson]);

        assert_eq!(sediment_lessons(tmp.path()), 0);
        assert!(!tmp.path().join("escaped").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sediment_never_follows_project_or_global_domain_symlinks() {
        use std::os::unix::fs::symlink;

        let _home = TempHome::new();
        let project = TempDir::new().unwrap();
        let lesson = mk_pitfall(
            "dependency/module-not-found/private",
            "install dependency",
            "dependency missing",
            "2026-01-01T00:00:00Z",
            2,
        );
        write_raw_lessons(project.path(), DEV_ERRORS_FILE, &[lesson]);

        let project_external = TempDir::new().unwrap();
        let project_victim = project_external.path().join("lesson-victim.md");
        fs::write(&project_victim, "project victim must survive").unwrap();
        let project_domain = project.path().join(LEARNED_DIR).join("dependency");
        symlink(project_external.path(), &project_domain).unwrap();

        let global_root = global_learned_dir().unwrap();
        let global_external = TempDir::new().unwrap();
        let global_victim = global_external.path().join("lesson-victim.md");
        fs::write(&global_victim, "global victim must survive").unwrap();
        let global_domain = global_root.join("dependency");
        symlink(global_external.path(), &global_domain).unwrap();

        assert_eq!(sediment_lessons(project.path()), 0);
        assert_eq!(
            fs::read_to_string(&project_victim).unwrap(),
            "project victim must survive"
        );
        assert_eq!(
            fs::read_to_string(&global_victim).unwrap(),
            "global victim must survive"
        );
        assert_eq!(
            fs::read_dir(project_external.path())
                .unwrap()
                .flatten()
                .count(),
            1,
            "local sediment must not create files through a domain symlink"
        );
        assert_eq!(
            fs::read_dir(global_external.path())
                .unwrap()
                .flatten()
                .count(),
            1,
            "global promotion must not create files through a domain symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn raw_capture_never_follows_a_managed_learned_symlink() {
        use std::os::unix::fs::symlink;

        let _home = TempHome::new();
        let project = TempDir::new().unwrap();
        fs::create_dir(project.path().join(".umadev")).unwrap();
        let external = TempDir::new().unwrap();
        let victim = external.path().join("dev-errors.jsonl");
        fs::write(&victim, "external victim must survive").unwrap();
        symlink(
            external.path(),
            project.path().join(".umadev").join("learned"),
        )
        .unwrap();

        let error = "Error: Cannot find module 'lodash'".to_string();
        let outcome = capture_dev_errors_detailed(
            project.path(),
            std::slice::from_ref(&error),
            "demo",
            "private requirement",
        );

        assert_eq!(outcome.observations, 0, "a rejected write is not announced");
        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "external victim must survive"
        );
        assert_eq!(
            fs::read_dir(external.path()).unwrap().flatten().count(),
            1,
            "raw capture must not write through the managed learned symlink"
        );
    }

    #[test]
    fn invalidated_pitfall_is_inert_until_fresh_episode_reactivates_it() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        capture_dev_errors(tmp.path(), std::slice::from_ref(&error), "demo", "r");
        let mut rows = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        rows[0].invalidated = true;
        rows[0].trust = 0.9;
        rows[0].efficacy.get_or_insert_default().proven_fix = true;
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &rows);

        let invalidated = pitfall_efficacy_summary(tmp.path());
        assert_eq!(invalidated.total, 1);
        assert_eq!(invalidated.invalidated, 1);
        assert_eq!(
            lessons_report(tmp.path()).top_pitfalls[0].status,
            PitfallStatus::Invalidated
        );
        assert!(lessons_for_error(tmp.path(), &error).is_empty());
        assert!(recurring_pitfall_for_error(tmp.path(), &error).is_none());
        assert!(commit_pitfall_fix_attempt(tmp.path(), &error).is_none());
        assert!(!record_pitfall_strategy(
            tmp.path(),
            "dependency/module-not-found/lodash",
            "new strategy"
        ));

        let outcome =
            capture_dev_errors_detailed(tmp.path(), std::slice::from_ref(&error), "demo", "r");
        assert_eq!(outcome.recurrent_incidents, 1);
        let revived = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert!(!revived.invalidated);
        assert!((revived.trust() - NEUTRAL_TRUST).abs() < f32::EPSILON);
        assert_eq!(revived.pitfall_status(), PitfallStatus::Corroborated);
        assert!(!lessons_for_error(tmp.path(), &error).is_empty());
    }

    #[test]
    fn attempt_settlement_is_exact_once_and_targets_rendered_shadow_advice() {
        let tmp = TempDir::new().unwrap();
        let sig = "dependency/module-not-found/lodash";
        let low = mk_pitfall(sig, "wrong low-hit fix", "low", "2026-01-01T00:00:00Z", 1);
        let high = mk_pitfall(
            sig,
            "install lodash exactly",
            "high",
            "2026-02-01T00:00:00Z",
            3,
        );
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[low, high]);
        let error = "Error: Cannot find module 'lodash'";
        let rendered = lessons_for_error(tmp.path(), error);
        assert!(rendered.contains("install lodash exactly"));
        assert!(!rendered.contains("wrong low-hit fix"));

        let token = commit_pitfall_fix_attempt(tmp.path(), error).unwrap();
        let rows = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        assert!(!rows[0]
            .efficacy
            .as_ref()
            .is_some_and(|e| e.pending_fix_attempts.contains(&token)));
        assert!(rows[1]
            .efficacy
            .as_ref()
            .is_some_and(|e| e.pending_fix_attempts.contains(&token)));

        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &token,
                PitfallFixAttemptResult::VerificationFailed(error.to_string()),
            ),
            PitfallFixSettlement::SameSignatureFailed
        );
        assert_eq!(
            settle_pitfall_fix_attempt(tmp.path(), &token, PitfallFixAttemptResult::Passed),
            PitfallFixSettlement::NotFound
        );
        assert_eq!(
            settle_pitfall_fix_attempt(tmp.path(), "unknown", PitfallFixAttemptResult::Passed),
            PitfallFixSettlement::NotFound
        );
        let rows = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        let high = &rows[1];
        let efficacy = high.efficacy.as_ref().unwrap();
        assert!(efficacy.pending_fix_attempts.is_empty());
        assert!(efficacy.recurred_after_warning);
        assert!(!efficacy.proven_fix);
        assert!(efficacy
            .failed_fixes
            .iter()
            .any(|fix| fix == "install lodash exactly"));
    }

    #[test]
    fn exact_learning_apis_never_confirm_an_uncommitted_atomic_write() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'";
        let row = mk_pitfall(
            "dependency/module-not-found/lodash",
            "install lodash",
            "missing dependency",
            "2026-01-01T00:00:00Z",
            2,
        );
        assert!(write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[row]));

        let uncommitted =
            with_forced_atomic_write_failure(|| commit_pitfall_fix_attempt(tmp.path(), error));
        assert!(
            uncommitted.is_none(),
            "a failed commit cannot issue a token"
        );
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert!(row
            .efficacy
            .as_ref()
            .is_none_or(|efficacy| efficacy.pending_fix_attempts.is_empty()));

        let token = commit_pitfall_fix_attempt(tmp.path(), error).unwrap();
        let settlement = with_forced_atomic_write_failure(|| {
            settle_pitfall_fix_attempt(tmp.path(), &token, PitfallFixAttemptResult::Passed)
        });
        assert_eq!(settlement, PitfallFixSettlement::Inconclusive);
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert_eq!(row.pitfall_status(), PitfallStatus::Active);
        assert!(row
            .efficacy
            .as_ref()
            .unwrap()
            .pending_fix_attempts
            .contains(&token));

        let strategy_written = with_forced_atomic_write_failure(|| {
            record_pitfall_strategy(
                tmp.path(),
                "dependency/module-not-found/lodash",
                "try a different resolver",
            )
        });
        assert!(!strategy_written);
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert!(row.efficacy.unwrap().next_strategy.is_empty());
        assert!(!tmp.path().join(REFLECTIONS_DIR).exists());
    }

    #[test]
    fn new_verification_error_consumes_token_without_poisoning_original_fix() {
        let tmp = TempDir::new().unwrap();
        let original = "Error: Cannot find module 'lodash'";
        let row = mk_pitfall(
            "dependency/module-not-found/lodash",
            "install lodash",
            "missing dependency",
            "2026-01-01T00:00:00Z",
            2,
        );
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[row]);
        let token = commit_pitfall_fix_attempt(tmp.path(), original).unwrap();

        // The module error was fixed, but a different package now fails. The
        // overall gate is red; this is not evidence that the lodash advice failed.
        let settlement = settle_pitfall_fix_attempt(
            tmp.path(),
            &token,
            PitfallFixAttemptResult::VerificationFailed("Error: Cannot find module 'react'".into()),
        );
        assert_eq!(settlement, PitfallFixSettlement::Inconclusive);
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        let efficacy = row.efficacy.unwrap();
        assert!(efficacy.pending_fix_attempts.is_empty());
        assert!(!efficacy.proven_fix);
        assert!(!efficacy.recurred_after_warning);
        assert!(efficacy.last_fix_failed_at.is_empty());
        assert!(efficacy.failed_fixes.is_empty());
        assert_eq!((efficacy.helpful, efficacy.harmful), (0, 0));

        // The consumed token cannot later be retroactively settled by lodash.
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &token,
                PitfallFixAttemptResult::VerificationFailed(original.into()),
            ),
            PitfallFixSettlement::NotFound
        );

        let skipped = commit_pitfall_fix_attempt(tmp.path(), original).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(tmp.path(), &skipped, PitfallFixAttemptResult::Unknown,),
            PitfallFixSettlement::Inconclusive
        );
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        let efficacy = row.efficacy.unwrap();
        assert!(efficacy.pending_fix_attempts.is_empty());
        assert_eq!((efficacy.helpful, efficacy.harmful), (0, 0));
    }

    #[test]
    fn failed_evidence_matches_original_signature_even_after_another_error() {
        let tmp = TempDir::new().unwrap();
        let original = "Error: Cannot find module 'lodash'";
        let row = mk_pitfall(
            "dependency/module-not-found/lodash",
            "install lodash",
            "missing dependency",
            "2026-01-01T00:00:00Z",
            2,
        );
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[row]);
        let token = commit_pitfall_fix_attempt(tmp.path(), original).unwrap();
        let evidence = "Error: Cannot find module 'react'\nError: Cannot find module 'lodash'";
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &token,
                PitfallFixAttemptResult::VerificationFailed(evidence.into()),
            ),
            PitfallFixSettlement::SameSignatureFailed
        );
    }

    #[test]
    fn coarse_family_attempts_require_the_same_private_evidence_fingerprint() {
        for (family, original, different) in [
            (
                "test/assertion",
                "test login_redirects assertion failed: left `/login`, right `/home`",
                "test deletes_account assertion failed: left 500, right 204",
            ),
            (
                "type/type-mismatch",
                "error[E0308]: mismatched types: expected UserId, found String in create_user",
                "error[E0308]: mismatched types: expected Vec<Post>, found Option<Post> in list_posts",
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let original = original.to_string();
            for _ in 0..2 {
                let _ = capture_dev_errors_detailed(
                    tmp.path(),
                    std::slice::from_ref(&original),
                    "demo",
                    "requirement",
                );
            }
            let stored = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
            assert_eq!(stored.len(), 1);
            assert!(stored[0].signature.starts_with(&format!("{family}/e-")));
            let original_signature = stored[0].signature.clone();
            let different = different.to_string();
            let _ = capture_dev_errors_detailed(
                tmp.path(),
                std::slice::from_ref(&different),
                "demo",
                "requirement",
            );
            let stored = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
            assert_eq!(stored.len(), 2, "different coarse errors stay separate");
            assert!(stored.iter().any(|row| {
                row.signature == original_signature && row.hits() == 2
            }));
            let token = commit_pitfall_fix_attempt(tmp.path(), &original).unwrap();
            assert_eq!(
                settle_pitfall_fix_attempt(
                    tmp.path(),
                    &token,
                    PitfallFixAttemptResult::VerificationFailed(different),
                ),
                PitfallFixSettlement::Inconclusive,
                "a different assertion/type error in the same family must not poison the old fix"
            );
        }

        let tmp = TempDir::new().unwrap();
        let original = "assertion failed".to_string();
        for _ in 0..2 {
            let _ = capture_dev_errors_detailed(
                tmp.path(),
                std::slice::from_ref(&original),
                "demo",
                "requirement",
            );
        }
        let stored = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE);
        assert!(stored[0].signature.starts_with("test/assertion/u-"));
        let token = commit_pitfall_fix_attempt(tmp.path(), &original).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(
                tmp.path(),
                &token,
                PitfallFixAttemptResult::VerificationFailed(original),
            ),
            PitfallFixSettlement::Inconclusive,
            "under-specified family evidence always abstains"
        );
    }

    #[test]
    fn pending_attempt_never_downgrades_last_settled_recurring_verdict() {
        let tmp = TempDir::new().unwrap();
        let sig = "dependency/module-not-found/lodash";
        let mut recurring = mk_pitfall(sig, "install lodash", "missing", "2026-01-01Z", 2);
        recurring.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            recurred_after_warning: true,
            last_fix_failed_at: "2026-01-02T00:00:00Z".into(),
            exact_last_fix_failed_at: "2026-01-02T00:00:00Z".into(),
            recent_observations: vec![test_evidence(
                "pending-prior-failure",
                "2026-01-02T00:00:00Z",
                KnowledgeEvidenceOutcome::FixFailed,
            )],
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[recurring]);
        let token =
            commit_pitfall_fix_attempt(tmp.path(), "Error: Cannot find module 'lodash'").unwrap();
        let reloaded = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        assert_eq!(reloaded.pitfall_status(), PitfallStatus::Recurring);
        assert!(reloaded
            .efficacy
            .as_ref()
            .unwrap()
            .pending_fix_attempts
            .contains(&token));
        assert_eq!(
            lessons_report(tmp.path()).top_pitfalls[0].status,
            PitfallStatus::Recurring
        );
    }

    #[test]
    fn ordinary_recurrence_cannot_settle_a_stale_or_pending_attempt() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        let mut stale_injected = mk_pitfall(
            "dependency/module-not-found/lodash",
            "install lodash",
            "missing",
            "2026-01-01T00:00:00Z",
            1,
        );
        stale_injected.efficacy = Some(PitfallEfficacy {
            injected: 9,
            occ_at_injection: 1,
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[stale_injected]);
        let _ = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "requirement",
        );
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        let efficacy = row.efficacy.as_ref().unwrap();
        assert!(!efficacy.recurred_after_warning);
        assert!(efficacy.last_fix_failed_at.is_empty());

        // A verified fix may later regress, but a currently pending exact
        // attempt must be settled by its token rather than by generic capture.
        let mut pending = row;
        let baseline = pending.hits();
        pending.efficacy = Some(PitfallEfficacy {
            proven_fix: true,
            occ_at_injection: baseline,
            pending_fix_attempts: vec!["fix-pending".into()],
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[pending]);
        let _ = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "requirement",
        );
        let pending_row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        let pending_efficacy = pending_row.efficacy.unwrap();
        assert!(pending_efficacy.proven_fix);
        assert!(!pending_efficacy.recurred_after_warning);
        assert!(pending_efficacy.last_fix_failed_at.is_empty());
    }

    #[test]
    fn later_exact_recurrence_does_not_infer_a_repair_outcome() {
        let tmp = TempDir::new().unwrap();
        let error = "Error: Cannot find module 'lodash'".to_string();
        let validated = mk_pitfall(
            "dependency/module-not-found/lodash",
            "install lodash",
            "missing",
            "2026-01-01T00:00:00Z",
            2,
        );
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &[validated]);
        let token = commit_pitfall_fix_attempt(tmp.path(), &error).unwrap();
        assert_eq!(
            settle_pitfall_fix_attempt(tmp.path(), &token, PitfallFixAttemptResult::Passed),
            PitfallFixSettlement::Passed
        );

        let _ = capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "requirement",
        );
        let row = read_raw_lessons(tmp.path(), DEV_ERRORS_FILE).remove(0);
        let efficacy = row.efficacy.unwrap();
        assert!(!efficacy.recurred_after_warning);
        assert!(efficacy.proven_fix);
        assert!(efficacy.last_fix_failed_at.is_empty());
        assert!(efficacy.failed_fixes.is_empty());
        assert!(!efficacy.last_recurred_at.is_empty());
    }

    #[test]
    fn canonical_duplicate_lifecycle_uses_settled_event_chronology() {
        let tmp = TempDir::new().unwrap();
        let sig = "dependency/module-not-found/lodash";
        let mut ordinary_repeat = mk_pitfall(sig, "fix", "root", "2026-01-01Z", 2);
        ordinary_repeat.efficacy = Some(PitfallEfficacy {
            last_recurred_at: "2026-01-02T00:00:00Z".into(),
            ..PitfallEfficacy::default()
        });
        let active_shadow = mk_pitfall(sig, "fix", "root", "2026-01-03Z", 1);
        write_raw_lessons(
            tmp.path(),
            DEV_ERRORS_FILE,
            &[ordinary_repeat.clone(), active_shadow],
        );
        assert_eq!(
            lessons_report(tmp.path()).top_pitfalls[0].status,
            PitfallStatus::Active,
            "an observation timestamp alone is not a failed-advice verdict"
        );

        ordinary_repeat.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            recurred_after_warning: true,
            last_fix_failed_at: "2026-01-02T00:00:00Z".into(),
            exact_last_fix_failed_at: "2026-01-02T00:00:00Z".into(),
            recent_observations: vec![test_evidence(
                "failed-first",
                "2026-01-02T00:00:00Z",
                KnowledgeEvidenceOutcome::FixFailed,
            )],
            ..PitfallEfficacy::default()
        });
        let mut later_verified = mk_pitfall(sig, "fix", "root", "2026-01-03Z", 1);
        later_verified.efficacy = Some(PitfallEfficacy {
            outcome_attribution_version: 1,
            proven_fix: true,
            last_verified_at: "2026-01-04T00:00:00Z".into(),
            exact_last_verified_at: "2026-01-04T00:00:00Z".into(),
            recent_observations: vec![test_evidence(
                "success-later",
                "2026-01-04T00:00:00Z",
                KnowledgeEvidenceOutcome::FixSucceeded,
            )],
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(
            tmp.path(),
            DEV_ERRORS_FILE,
            &[ordinary_repeat.clone(), later_verified.clone()],
        );
        assert_eq!(
            lessons_report(tmp.path()).top_pitfalls[0].status,
            PitfallStatus::Validated
        );

        let efficacy = ordinary_repeat.efficacy.as_mut().unwrap();
        efficacy.last_fix_failed_at = "2026-01-05T00:00:00Z".into();
        efficacy.exact_last_fix_failed_at = "2026-01-05T00:00:00Z".into();
        efficacy.recent_observations.push(test_evidence(
            "failed-later",
            "2026-01-05T00:00:00Z",
            KnowledgeEvidenceOutcome::FixFailed,
        ));
        write_raw_lessons(
            tmp.path(),
            DEV_ERRORS_FILE,
            &[ordinary_repeat, later_verified],
        );
        assert_eq!(
            lessons_report(tmp.path()).top_pitfalls[0].status,
            PitfallStatus::Recurring
        );
    }

    #[test]
    fn curated_report_is_untruncated_and_canonicalizes_reusable_rules() {
        let tmp = TempDir::new().unwrap();
        let rows: Vec<Lesson> = (0..13)
            .map(|index| {
                let discriminator = char::from(b'a' + u8::try_from(index).unwrap());
                mk_pitfall(
                    &format!("dependency/module-not-found/pkg{discriminator}"),
                    &format!("install pkg{discriminator}"),
                    "missing dependency",
                    &format!("2026-01-{:02}T00:00:00Z", index + 1),
                    2,
                )
            })
            .collect();
        // Two legacy duplicate rows become one canonical rule. Their historical
        // aggregate rows carry no independent evidence IDs, so canonicalization
        // must not manufacture two votes from duplicated storage.
        let mut validated = seed_cluster_one_lesson();
        validated.kind = LessonKind::ValidatedPattern;
        validated.domain = "api".into();
        validated.title = "Reusable validated contract".into();
        validated.fix = "reuse the contract".into();
        validated.root_cause = "mechanical gate passed".into();
        let mut validated_later = validated.clone();
        validated_later.first_seen = "2026-07-01T00:00:00Z".into();
        write_raw_lessons(
            tmp.path(),
            "validated-decisions.jsonl",
            &[validated, validated_later],
        );
        write_raw_lessons(tmp.path(), DEV_ERRORS_FILE, &rows);

        let report = lessons_report(tmp.path());
        assert!(report.curated_lessons.len() >= 14);
        assert!(report
            .curated_lessons
            .iter()
            .any(|entry| entry.title.contains("pkgm")));
        let validated_rule = report
            .curated_lessons
            .iter()
            .find(|entry| entry.title == "Reusable validated contract")
            .unwrap();
        assert_eq!(validated_rule.evidence_count, 0);
        assert_eq!(validated_rule.status, CuratedLessonStatus::Hypothesis);
        assert_eq!(
            report
                .curated_lessons
                .iter()
                .filter(|entry| entry.title == "Reusable validated contract")
                .count(),
            1
        );

        // A folded belief covering that raw rule replaces it rather than showing
        // belief + raw evidence as two reusable cards.
        let raw_validated = read_raw_lessons(tmp.path(), "validated-decisions.jsonl").remove(0);
        let mut belief = seed_cluster_one_lesson();
        belief.kind = LessonKind::Belief;
        belief.title = "Folded validated contract".into();
        belief.fix = "reuse the contract".into();
        belief.root_cause = "mechanical gate passed".into();
        belief.evidence_count = 2;
        belief.evidence = vec![evidence_key(&raw_validated)];
        // Legacy aggregate outcome counters have no exact sent-memory/turn
        // identity, so they must not upgrade a belief to Validated.
        belief.efficacy = Some(PitfallEfficacy {
            helpful: 10,
            ..PitfallEfficacy::default()
        });
        write_raw_lessons(tmp.path(), BELIEFS_FILE, &[belief]);
        let report = lessons_report(tmp.path());
        let folded = report
            .curated_lessons
            .iter()
            .find(|entry| entry.title == "Folded validated contract")
            .unwrap();
        assert_eq!(folded.status, CuratedLessonStatus::Pending);
        assert!(!report
            .curated_lessons
            .iter()
            .any(|entry| entry.title == "Reusable validated contract"));
    }

    #[test]
    fn lesson_recall_matches_bilingual_synonyms_without_losing_abstention() {
        let _home = TempHome::new();
        let tmp = TempDir::new().unwrap();
        let mut lesson = mk_failure_eff("rotate auth credentials", 0, 0);
        lesson.fix = "rotate authentication credentials before expiry".into();
        lesson.keywords = vec![
            "authentication".into(),
            "credential".into(),
            "rotation".into(),
        ];
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &[lesson]);

        let recalled = relevant_lessons_for_prompt(tmp.path(), "登录凭证轮换");
        assert!(
            recalled.contains("rotate authentication credentials before expiry"),
            "Chinese synonyms must recall the English lesson: {recalled}"
        );
        assert!(
            relevant_lessons_for_prompt(tmp.path(), "绘制紫色三角形").is_empty(),
            "an unrelated CJK query must still abstain"
        );
    }

    #[test]
    fn invalidated_conflicting_memory_never_reenters_prompt_recall() {
        let _home = TempHome::new();
        let tmp = TempDir::new().unwrap();
        let older = mk_db_lesson(
            "indexing add",
            "always add a btree index on every filtered column for speed",
            "missing indexes slowed reads across the board significantly",
            "2026-06-01T00:00:00Z",
        );
        let newer = mk_db_lesson(
            "indexing drop",
            "avoid over indexing and drop redundant indexes to keep writes fast",
            "too many indexes bloated writes and storage badly here",
            "2026-06-20T00:00:00Z",
        );
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &[older, newer]);
        assert_eq!(scan_contradictions(tmp.path()), 1);

        let recalled = relevant_lessons_for_prompt(tmp.path(), "database index postgres query");
        assert!(recalled.contains("drop redundant indexes"), "{recalled}");
        assert!(
            !recalled.contains("always add a btree index"),
            "the invalidated conflicting rule must stay out of behavior: {recalled}"
        );
    }

    #[test]
    fn chinese_conflicting_memory_is_invalidated_and_abstains_from_recall() {
        let _home = TempHome::new();
        let tmp = TempDir::new().unwrap();
        let mut older = mk_db_lesson(
            "旧索引规则",
            "总是为每个查询字段添加索引以提升读取性能",
            "缺少索引导致数据库查询变慢",
            "2026-06-01T00:00:00Z",
        );
        let mut newer = mk_db_lesson(
            "新索引规则",
            "避免过度索引并删除冗余索引以保持写入稳定",
            "索引过多导致数据库写入延迟",
            "2026-06-20T00:00:00Z",
        );
        let topic = vec!["数据库".into(), "索引".into(), "查询".into(), "性能".into()];
        older.keywords.clone_from(&topic);
        newer.keywords = topic;
        write_raw_lessons(tmp.path(), "quality-failures.jsonl", &[older, newer]);

        assert_eq!(scan_contradictions(tmp.path()), 1);
        let recalled = relevant_lessons_for_prompt(tmp.path(), "优化数据库索引查询性能");
        assert!(recalled.contains("删除冗余索引"), "{recalled}");
        assert!(
            !recalled.contains("每个查询字段添加索引"),
            "失效的中文冲突记忆不能重新进入提示词：{recalled}"
        );
    }
}
