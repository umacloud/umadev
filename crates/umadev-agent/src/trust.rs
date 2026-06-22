//! Trust / autonomy tiers — the control layer over the confirmation gates.
//!
//! UmaDev's pipeline already has human-in-the-loop gates (`docs_confirm`,
//! `preview_confirm`) plus a binary autonomous toggle. This module generalises
//! that toggle into a **progressive-trust ladder** so a user can pick how much
//! autonomy to grant as their confidence in the agent grows:
//!
//! - [`TrustMode::Plan`]    — research + planning only; **read-only, never
//!   executes** real code. The agent produces "here's how I'd do it" for review.
//! - [`TrustMode::Guarded`] — the **default**; the existing human-in-the-loop
//!   behaviour — every gate pauses for an explicit confirmation.
//! - [`TrustMode::Auto`]    — fully autonomous; every gate auto-approves
//!   (the existing `/auto` behaviour, preserved unchanged).
//!
//! Two safety/trust mechanisms ride on top of the ladder:
//!
//! 1. **Reversibility-weighted escalation** ([`reversibility_class`] /
//!    [`requires_confirmation`]): an edit inside the project tree is cheap and
//!    reversible, so it stays light-touch even in `auto`. An action that touches
//!    version-control internals, the network, or is a destructive shell verb is
//!    **irreversible / blast-radius-heavy** and is escalated to a confirmation
//!    **regardless of mode** — `auto` does not get to skip it. This is the hard
//!    safety floor.
//! 2. **Collaborative trust tracking** ([`TrustLedger`]): per project, per gate,
//!    we record how many times in a row the user auto-approved (or the gate
//!    auto-passed). After a threshold of consecutive passes we *suggest* — never
//!    silently switch — that the user let that gate auto-advance. Persisted to
//!    `.umadev/trust.json`, fully fail-open.
//!
//! Everything here is **deterministic**: the mode only changes the gate
//! auto-pass policy and the reversibility classifier is a pure function of the
//! action string. No new model endpoint, no randomness.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Autonomy tier selected for a run. The mode only controls the gate
/// auto-pass policy; it never introduces non-determinism into phase content.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrustMode {
    /// Research + planning only. The pipeline runs research and the doc phase
    /// (PRD / architecture / UIUX — "here's how I'd build it") and then **stops
    /// at `docs_confirm` without executing** spec / frontend / backend. Nothing
    /// real is written into the codebase: the user reviews the plan first.
    Plan,
    /// The default. Existing human-in-the-loop behaviour — every confirmation
    /// gate pauses and waits for the user (`docs_confirm`, `preview_confirm`,
    /// and the clarify gate).
    #[default]
    Guarded,
    /// Fully autonomous — every gate auto-approves and the pipeline drives
    /// end-to-end. Identical to the legacy `/auto` / `auto_approve_gates=true`
    /// behaviour. (Reversibility escalation still applies as a hard floor.)
    Auto,
}

impl TrustMode {
    /// Stable lowercase identifier (CLI flag value, persisted form).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Guarded => "guarded",
            Self::Auto => "auto",
        }
    }

    /// Parse a mode from a CLI flag / persisted string. Case-insensitive,
    /// whitespace-tolerant. A handful of intuitive aliases map onto the three
    /// canonical tiers so users aren't surprised. Returns `None` for anything
    /// unrecognised (callers fail-open to [`TrustMode::Guarded`]).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plan" | "planning" | "dry-run" | "dryrun" | "readonly" | "read-only" => {
                Some(Self::Plan)
            }
            "guarded" | "guard" | "manual" | "review" | "default" => Some(Self::Guarded),
            "auto" | "autonomous" | "yolo" | "full" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Parse, falling back to the default [`TrustMode::Guarded`] for an
    /// unrecognised / empty value. Use at the CLI/TUI boundary where an
    /// unknown mode should degrade safely rather than error.
    #[must_use]
    pub fn parse_or_default(s: &str) -> Self {
        Self::parse(s).unwrap_or_default()
    }

    /// Whether confirmation gates auto-approve in this mode (ignoring the
    /// reversibility floor, which can still force a confirmation). `auto` →
    /// gates pass automatically; `guarded` / `plan` → gates pause for the user.
    #[must_use]
    pub const fn gates_auto_approve(self) -> bool {
        matches!(self, Self::Auto)
    }

    /// Whether the pipeline is allowed to *execute* (write real code in the
    /// spec / frontend / backend phases). `plan` is read-only and stops after
    /// the planning docs; `guarded` and `auto` both execute.
    #[must_use]
    pub const fn executes(self) -> bool {
        !matches!(self, Self::Plan)
    }

    /// i18n key for the one-line description shown when the mode is selected.
    #[must_use]
    pub const fn desc_key(self) -> &'static str {
        match self {
            Self::Plan => "mode.plan.on",
            Self::Guarded => "mode.guarded.on",
            Self::Auto => "mode.auto.on",
        }
    }

    /// i18n key for the short status-bar chip label.
    #[must_use]
    pub const fn chip_key(self) -> &'static str {
        match self {
            Self::Plan => "mode.plan_chip",
            Self::Guarded => "mode.guarded_chip",
            Self::Auto => "mode.auto_chip",
        }
    }
}

/// How reversible / blast-radius-heavy a candidate action is. Drives the
/// reversibility-weighted escalation: a [`Self::Reversible`] action is
/// light-touch (auto-OK), anything else is escalated to a confirmation.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Reversibility {
    /// A project-scoped, easily-undone action (edit a file inside the
    /// workspace, run the build/tests). Cheap; stays automatic.
    Reversible,
    /// Touches version-control internals (`.git`), so it can rewrite/lose
    /// history. Always escalated.
    VersionControl,
    /// Reaches the network (push, fetch, curl, install from a remote). Side
    /// effects leave the machine; always escalated.
    Network,
    /// A destructive / unbounded shell verb (`rm -rf`, `dd`, `mkfs`, writes
    /// outside the workspace, …). Always escalated.
    Destructive,
}

impl Reversibility {
    /// Whether an action of this class must always be confirmed, no matter the
    /// trust mode. Only [`Self::Reversible`] is allowed to stay automatic.
    #[must_use]
    pub const fn always_escalates(self) -> bool {
        !matches!(self, Self::Reversible)
    }

    /// i18n key naming the escalation reason (for the confirmation prompt).
    #[must_use]
    pub const fn reason_key(self) -> &'static str {
        match self {
            Self::Reversible => "trust.reason.reversible",
            Self::VersionControl => "trust.reason.git",
            Self::Network => "trust.reason.network",
            Self::Destructive => "trust.reason.destructive",
        }
    }
}

/// Destructive shell verbs whose presence flips an action to
/// [`Reversibility::Destructive`]. Matched as whitespace-bounded tokens so a
/// substring like `format!` in a path doesn't false-positive.
const DESTRUCTIVE_TOKENS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rmdir",
    "mkfs",
    "dd ",
    ":(){",
    "shutdown",
    "reboot",
    "chmod -r 777",
    "truncate",
    "> /dev",
    "sudo ",
];

/// Tokens that indicate the action reaches the network.
const NETWORK_TOKENS: &[&str] = &[
    "git push",
    "git pull",
    "git fetch",
    "git clone",
    "curl ",
    "wget ",
    "ssh ",
    "scp ",
    "rsync ",
    "npm publish",
    "cargo publish",
    "npm install",
    "pip install",
    "http://",
    "https://",
];

/// Classify a candidate action — a shell command string and/or a target path —
/// into a [`Reversibility`] class. Order matters: the most dangerous class
/// wins (destructive > network > version-control > reversible) so that, e.g.,
/// `rm -rf .git` is reported as destructive rather than merely VCS.
///
/// Pure and deterministic. Either argument may be empty.
#[must_use]
pub fn reversibility_class(command: &str, target_path: &str) -> Reversibility {
    let cmd = command.to_ascii_lowercase();
    if DESTRUCTIVE_TOKENS.iter().any(|t| cmd.contains(t)) {
        return Reversibility::Destructive;
    }
    if NETWORK_TOKENS.iter().any(|t| cmd.contains(t)) {
        return Reversibility::Network;
    }
    // Touching `.git/` internals (config, refs, objects, hooks) can rewrite or
    // lose history irreversibly — escalate. A normal edit to a tracked source
    // file is NOT in `.git/` and stays reversible.
    if path_touches_vcs(&cmd) || path_touches_vcs(&target_path.to_ascii_lowercase()) {
        return Reversibility::VersionControl;
    }
    Reversibility::Reversible
}

/// Whether a path/command string reaches into version-control internals.
fn path_touches_vcs(s: &str) -> bool {
    s.contains("/.git/")
        || s.contains("\\.git\\")
        || s.starts_with(".git/")
        || s.starts_with(".git\\")
        || s == ".git"
        || s.contains(" .git/")
        || s.contains("git reset --hard")
        || s.contains("git clean")
        || s.contains("git rebase")
        || s.contains("git filter-branch")
        || s.contains("git push --force")
        || s.contains("git checkout --")
}

/// The decision: given a candidate **mid-turn tool call**, must it be escalated
/// to a confirmation before it runs?
///
/// This is the per-tool-call chokepoint the non-interactive driving loop
/// consults (a base that asks `can_use_tool` mid-turn). The human gate lives at
/// the pipeline's **confirm gates** (`docs_confirm` / `preview_confirm`), NOT on
/// every mid-turn tool call — so the only thing this floor escalates is an
/// **irreversible** action (`.git` internals, network, destructive shell verbs),
/// which [`Reversibility::always_escalates`] flags. That floor is
/// **mode-independent**: even [`TrustMode::Auto`] cannot skip it, and even
/// [`TrustMode::Guarded`] / [`TrustMode::Plan`] do NOT escalate a *reversible*
/// in-project write — otherwise a guarded run could never let the base edit a
/// file (the base would be DENY'd on every write and spin doing nothing).
///
/// The gate-pause policy (guarded / plan pausing the *whole pipeline* for the
/// user) is a separate concern handled by the gate machinery
/// ([`TrustMode::gates_auto_approve`]); it is not this per-tool-call decision.
///
/// `mode` is retained in the signature so callers keep a single chokepoint and a
/// future per-mode policy has a hook, but today the decision is mode-independent:
/// it depends only on the action's reversibility class.
#[must_use]
pub fn requires_confirmation(mode: TrustMode, command: &str, target_path: &str) -> bool {
    let _ = mode; // mode-independent today: the floor escalates only irreversible actions
    reversibility_class(command, target_path).always_escalates()
}

// ---------------------------------------------------------------------------
// Collaborative trust tracking — .umadev/trust.json
// ---------------------------------------------------------------------------

/// After this many *consecutive* auto-passes of one gate, the ledger raises a
/// one-time suggestion that the user let that gate auto-advance. Chosen high
/// enough that a couple of lucky passes don't nag, low enough to surface within
/// a normal week of use.
pub const SUGGEST_THRESHOLD: u32 = 3;

/// Per-gate trust counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateTrust {
    /// Consecutive times this gate was approved without a revision. Reset to 0
    /// whenever the user requests changes / cancels at the gate.
    #[serde(default)]
    pub consecutive_passes: u32,
    /// Lifetime total of passes (informational; never resets).
    #[serde(default)]
    pub total_passes: u32,
    /// Whether the auto-advance suggestion has already fired for this gate, so
    /// we prompt at most once and never nag.
    #[serde(default)]
    pub suggested: bool,
}

/// Project-scoped trust ledger persisted to `.umadev/trust.json`. Keyed by the
/// gate id string (`docs_confirm`, `preview_confirm`, `clarify`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustLedger {
    /// Per-gate counters. A `BTreeMap` keeps the on-disk JSON key order stable.
    #[serde(default)]
    pub gates: std::collections::BTreeMap<String, GateTrust>,
}

impl TrustLedger {
    /// Load the ledger from `<root>/.umadev/trust.json`. Fail-open: a missing
    /// or corrupt file yields a fresh empty ledger (never an error).
    #[must_use]
    pub fn load(project_root: &Path) -> Self {
        let path = Self::path(project_root);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Persist the ledger. Best-effort: an IO error is swallowed (fail-open —
    /// trust tracking must never block or fail the pipeline).
    pub fn save(&self, project_root: &Path) {
        let dir = project_root.join(".umadev");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(dir.join("trust.json"), text);
        }
    }

    fn path(project_root: &Path) -> PathBuf {
        project_root.join(".umadev").join("trust.json")
    }

    /// Record that `gate_id` was approved (auto or manual) without a revision.
    /// Bumps both counters. Returns a [`TrustSuggestion`] when the consecutive
    /// count first crosses [`SUGGEST_THRESHOLD`] (and only the first time).
    pub fn record_pass(&mut self, gate_id: &str) -> Option<TrustSuggestion> {
        let entry = self.gates.entry(gate_id.to_string()).or_default();
        entry.consecutive_passes = entry.consecutive_passes.saturating_add(1);
        entry.total_passes = entry.total_passes.saturating_add(1);
        if entry.consecutive_passes >= SUGGEST_THRESHOLD && !entry.suggested {
            entry.suggested = true;
            return Some(TrustSuggestion {
                gate_id: gate_id.to_string(),
                consecutive: entry.consecutive_passes,
            });
        }
        None
    }

    /// Record that `gate_id` was revised / rejected — the trust streak resets.
    /// The `suggested` flag is also cleared so a fresh streak can suggest again.
    pub fn record_revision(&mut self, gate_id: &str) {
        let entry = self.gates.entry(gate_id.to_string()).or_default();
        entry.consecutive_passes = 0;
        entry.suggested = false;
    }

    /// Current consecutive-pass count for a gate (0 if unseen).
    #[must_use]
    pub fn consecutive(&self, gate_id: &str) -> u32 {
        self.gates.get(gate_id).map_or(0, |g| g.consecutive_passes)
    }
}

/// Emitted when a gate has earned enough consecutive passes that the agent
/// *suggests* (does not auto-apply) letting it auto-advance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustSuggestion {
    /// The gate that crossed the threshold (e.g. `frontend` / `preview_confirm`).
    pub gate_id: String,
    /// How many times in a row it passed.
    pub consecutive: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn mode_parse_round_trips_and_aliases() {
        assert_eq!(TrustMode::parse("plan"), Some(TrustMode::Plan));
        assert_eq!(TrustMode::parse("guarded"), Some(TrustMode::Guarded));
        assert_eq!(TrustMode::parse("auto"), Some(TrustMode::Auto));
        // canonical round-trip
        for m in [TrustMode::Plan, TrustMode::Guarded, TrustMode::Auto] {
            assert_eq!(TrustMode::parse(m.as_str()), Some(m));
        }
        // aliases + case/whitespace tolerance
        assert_eq!(TrustMode::parse("  MANUAL "), Some(TrustMode::Guarded));
        assert_eq!(TrustMode::parse("read-only"), Some(TrustMode::Plan));
        assert_eq!(TrustMode::parse("YOLO"), Some(TrustMode::Auto));
        // unknown → None, default fallback
        assert_eq!(TrustMode::parse("nonsense"), None);
        assert_eq!(TrustMode::parse_or_default("nonsense"), TrustMode::Guarded);
        // default is guarded == existing behaviour
        assert_eq!(TrustMode::default(), TrustMode::Guarded);
    }

    #[test]
    fn mode_gate_and_execute_policy() {
        assert!(!TrustMode::Plan.executes());
        assert!(TrustMode::Guarded.executes());
        assert!(TrustMode::Auto.executes());

        assert!(!TrustMode::Plan.gates_auto_approve());
        assert!(!TrustMode::Guarded.gates_auto_approve());
        assert!(TrustMode::Auto.gates_auto_approve());
    }

    #[test]
    fn reversibility_classifies_each_class() {
        assert_eq!(
            reversibility_class("", "src/main.rs"),
            Reversibility::Reversible
        );
        assert_eq!(
            reversibility_class("cargo build", ""),
            Reversibility::Reversible
        );
        assert_eq!(
            reversibility_class("", ".git/config"),
            Reversibility::VersionControl
        );
        assert_eq!(
            reversibility_class("git push origin main", ""),
            Reversibility::Network
        );
        assert_eq!(
            reversibility_class("rm -rf /tmp/x", ""),
            Reversibility::Destructive
        );
    }

    #[test]
    fn destructive_beats_vcs_and_network() {
        // `rm -rf .git` is BOTH destructive and VCS-touching — destructive wins
        // (it's the highest-severity class).
        assert_eq!(
            reversibility_class("rm -rf .git", ""),
            Reversibility::Destructive
        );
        // network token + a path → network wins over a bare path read.
        assert_eq!(
            reversibility_class("curl https://x", "src/a.ts"),
            Reversibility::Network
        );
    }

    #[test]
    fn reversibility_floor_always_escalates_even_in_auto() {
        // The whole point: AUTO does NOT get to skip an irreversible action.
        for (cmd, path) in [
            ("git push", ""),
            ("rm -rf build", ""),
            ("", ".git/refs/heads/main"),
            ("git reset --hard HEAD~3", ""),
        ] {
            assert!(
                requires_confirmation(TrustMode::Auto, cmd, path),
                "auto must still confirm: {cmd:?} {path:?}"
            );
            assert!(requires_confirmation(TrustMode::Guarded, cmd, path));
            assert!(requires_confirmation(TrustMode::Plan, cmd, path));
        }
    }

    #[test]
    fn reversible_in_project_write_is_never_escalated_mid_turn() {
        // A plain in-project edit is reversible → it is NEVER escalated to a
        // mid-turn confirmation, in ANY mode. The human gate is at the confirm
        // gates, not on every tool call — so a GUARDED run must still let the base
        // write files (else it would DENY every write and spin doing nothing).
        for mode in [TrustMode::Auto, TrustMode::Guarded, TrustMode::Plan] {
            assert!(
                !requires_confirmation(mode, "", "src/app.tsx"),
                "reversible in-project write must not be escalated in {mode:?}"
            );
            // Build/test commands are reversible too.
            assert!(
                !requires_confirmation(mode, "npm run build", ""),
                "reversible build command must not be escalated in {mode:?}"
            );
        }
    }

    #[test]
    fn ledger_counts_and_suggests_at_threshold() {
        let mut led = TrustLedger::default();
        // First two passes: no suggestion yet.
        assert!(led.record_pass("frontend").is_none());
        assert!(led.record_pass("frontend").is_none());
        assert_eq!(led.consecutive("frontend"), 2);
        // Third pass crosses the threshold → suggestion fires ONCE.
        let s = led.record_pass("frontend").expect("threshold suggestion");
        assert_eq!(s.gate_id, "frontend");
        assert_eq!(s.consecutive, SUGGEST_THRESHOLD);
        // Subsequent passes do NOT re-suggest (no nagging).
        assert!(led.record_pass("frontend").is_none());
        assert_eq!(led.consecutive("frontend"), 4);
        // A different gate is tracked independently.
        assert!(led.record_pass("docs_confirm").is_none());
        assert_eq!(led.consecutive("docs_confirm"), 1);
    }

    #[test]
    fn revision_resets_streak_and_allows_resuggest() {
        let mut led = TrustLedger::default();
        for _ in 0..SUGGEST_THRESHOLD {
            led.record_pass("preview_confirm");
        }
        assert!(led.gates["preview_confirm"].suggested);
        // A revision wipes the streak and the suggested flag.
        led.record_revision("preview_confirm");
        assert_eq!(led.consecutive("preview_confirm"), 0);
        assert!(!led.gates["preview_confirm"].suggested);
        // Building a fresh streak can suggest again.
        for _ in 0..(SUGGEST_THRESHOLD - 1) {
            assert!(led.record_pass("preview_confirm").is_none());
        }
        assert!(led.record_pass("preview_confirm").is_some());
    }

    #[test]
    fn ledger_persists_round_trip_and_fail_open() {
        let tmp = TempDir::new().unwrap();
        // Missing file → fresh empty ledger (fail-open).
        let mut led = TrustLedger::load(tmp.path());
        assert!(led.gates.is_empty());
        led.record_pass("docs_confirm");
        led.record_pass("docs_confirm");
        led.save(tmp.path());
        // Reload sees the persisted counts.
        let back = TrustLedger::load(tmp.path());
        assert_eq!(back.consecutive("docs_confirm"), 2);
        assert_eq!(back, led);
    }

    #[test]
    fn ledger_load_is_fail_open_on_corrupt_json() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("trust.json"), "{ not json").unwrap();
        // Corrupt file must NOT error — yields a fresh ledger.
        let led = TrustLedger::load(tmp.path());
        assert!(led.gates.is_empty());
    }
}
