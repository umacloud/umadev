//! File-level checkpoint / rewind over the project workspace.
//!
//! Distinct from [`crate::state`] snapshots (which capture only the workflow
//! *phase*): a checkpoint is a full snapshot of the FILES the base wrote, so the
//! user can rewind a whole phase's worth of work — the feature every hot AI
//! coding tool ships (Cline shadow-git, Claude Code `/rewind`, Aider `/undo`).
//!
//! Implemented as a SHADOW git repo at `.umadev/checkpoints.git` (a separate
//! `--git-dir`, work-tree = project root) so it never touches the user's own
//! `.git` history. Every function is FAIL-OPEN: if `git` is missing or any step
//! fails it returns `None` / `Err` quietly and the pipeline is never blocked.
//!
//! This is the director's checkpoint — a phase-level safety net that composes
//! with (does not replace) a base CLI's own per-edit checkpointing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Heavy paths excluded from checkpoints even without a project `.gitignore`, so
/// a checkpoint never tries to snapshot `node_modules`, build output, or the
/// user's real `.git`. Written to the shadow repo's `info/exclude` on init.
const SHADOW_EXCLUDES: &[&str] = &[
    "/.git/",
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    ".next/",
    ".nuxt/",
    ".output/",
    ".turbo/",
    ".venv/",
    "coverage/",
    "__pycache__/",
    ".umadev/",
    "*.log",
];

/// The same heavy/dangerous paths as [`SHADOW_EXCLUDES`], expressed as git
/// **exclude pathspecs** for the staging `git add`.
///
/// TRADE-OFF (P1/P2): the shadow `add` is now `-A --force` so it captures files
/// the project `.gitignore` hides — an AI-written `.env.local`, ignored generated
/// config, ignored snapshots — otherwise a "roll the whole run back" would
/// silently leave those behind (the base wrote them, but a plain `git add -A`
/// skips ignored paths, so they were never in a checkpoint to restore). `--force`
/// overrides BOTH the project `.gitignore` and the shadow `info/exclude`, so to
/// keep the shadow commit BOUNDED we re-assert the heavy set here as pathspecs:
/// the real `.git`, dependency trees (`node_modules`, `.venv`), Python caches,
/// build output (`target`/`dist`/`build`/`.next`/`.nuxt`/`.output`), log spew,
/// and UmaDev's own `.umadev/` (which holds THIS shadow repo — force-adding it
/// would be recursive). A user with some OTHER giant ignored artifact (a
/// multi-GB dataset) is out of scope — the common heavy offenders are covered.
/// EVERY entry here ships in BOTH a root-anchored (`x/**`) and a NESTED
/// (`**/x/**`) form. A monorepo (`apps/web/.next/…`, `packages/api/dist/…`,
/// `services/py/.venv/…`) is the mainstream layout, not the exception: a
/// root-anchored-only pathspec leaves the nested twin of the same heavy dir
/// force-added into every checkpoint — hundreds of MB per commit, two
/// `reset --hard`s per temporary rewind, and a build artifact misread by the
/// scope floor as an unclaimed new source file. The nested twin is the rule; a
/// missing one is the bug.
const SHADOW_ADD_EXCLUDE_PATHSPECS: &[&str] = &[
    ":(exclude,glob).git/**",
    ":(exclude,glob)**/.git/**",
    ":(exclude,glob)node_modules/**",
    ":(exclude,glob)**/node_modules/**",
    ":(exclude,glob)target/**",
    ":(exclude,glob)**/target/**",
    ":(exclude,glob)dist/**",
    ":(exclude,glob)**/dist/**",
    ":(exclude,glob)build/**",
    ":(exclude,glob)**/build/**",
    ":(exclude,glob).next/**",
    ":(exclude,glob)**/.next/**",
    ":(exclude,glob).nuxt/**",
    ":(exclude,glob)**/.nuxt/**",
    ":(exclude,glob).output/**",
    ":(exclude,glob)**/.output/**",
    ":(exclude,glob).turbo/**",
    ":(exclude,glob)**/.turbo/**",
    ":(exclude,glob).venv/**",
    ":(exclude,glob)**/.venv/**",
    ":(exclude,glob)coverage/**",
    ":(exclude,glob)**/coverage/**",
    ":(exclude,glob)__pycache__/**",
    ":(exclude,glob)**/__pycache__/**",
    ":(exclude,glob).umadev/**",
    ":(exclude,glob)**/.umadev/**",
    ":(exclude,glob)*.log",
    ":(exclude,glob)**/*.log",
];

/// Stage the whole work-tree into the shadow index — INCLUDING files the project
/// `.gitignore` hides (`--force`), so a rollback can restore an AI-written
/// `.env.local` / ignored config — while still excluding the heavy/dangerous
/// paths in [`SHADOW_ADD_EXCLUDE_PATHSPECS`] so the checkpoint stays bounded and
/// never pulls in `node_modules` / build output / the real `.git` / this shadow
/// repo. A leading positive `.` guards the "exclude-only pathspec" edge across
/// git versions.
///
/// **A PARTIAL add is a FAILURE, not a success.** The exit status used to be discarded, so
/// a `git add` that errored on some paths and staged the rest still produced a "successful"
/// checkpoint — one that silently does not contain everything it claims to. Everything
/// downstream trusts that claim: [`restore_checkpoint`] tells the user "we snapshotted
/// first, so this is itself undoable", and [`recover_abandoned_temp_rewind`] `reset --hard`s
/// the work-tree on the strength of a rescue snapshot. A snapshot missing the very files
/// the add could not read is exactly the one whose reset destroys them. So a non-zero exit
/// yields `None` and the caller declines to act — `--force` means an ignored path is not an
/// error, so a failure here is a real one.
///
/// Fail-open: `None` when `git` can't spawn OR the add did not fully succeed — the caller
/// `?`-propagates and simply takes no checkpoint. The pipeline is never blocked.
fn stage_all_including_ignored(project_root: &Path) -> Option<std::process::Output> {
    let mut args: Vec<&str> = vec!["add", "-A", "--force", "--", "."];
    args.extend_from_slice(SHADOW_ADD_EXCLUDE_PATHSPECS);
    let out = git(project_root, &args)?;
    if !out.status.success() {
        tracing::warn!(
            root = %project_root.display(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "the shadow-repo staging step did not fully succeed — refusing to write a \
             checkpoint that would not contain everything it claims to"
        );
        return None;
    }
    Some(out)
}

/// One checkpoint entry, newest-first in [`list_checkpoints`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Checkpoint {
    /// Short commit id — the handle passed to [`restore_checkpoint`].
    pub id: String,
    /// Human label (usually the phase or reason).
    pub label: String,
    /// ISO-8601 commit time.
    pub when: String,
}

/// Path to the shadow git directory.
fn git_dir(project_root: &Path) -> PathBuf {
    project_root.join(".umadev").join("checkpoints.git")
}

/// Run a shadow-git command (work-tree = project root). `None` if `git` can't be
/// spawned at all.
///
/// The shadow repo is a BYTE-EXACT snapshot store, not a repository the user
/// collaborates in — so it must be hermetic, and never inherit the user's global
/// git configuration. Each setting pinned here is one that silently breaks the
/// safety net otherwise:
///
/// - **identity** — a user who has never run `git config --global user.email` (very
///   common on Windows) cannot commit at all. `create_checkpoint` would return
///   `None`, and since a rescue snapshot that FAILS means we decline to heal, the
///   entire workspace-protection net would never start, silently, for exactly the
///   users least likely to notice.
/// - **`core.autocrlf`** — `true` is the DEFAULT of Git for Windows. It would
///   normalise CRLF to LF on the way into a snapshot and write CRLF back out on
///   restore, so a heal would rewrite the line ending of every line of every LF file
///   it restores. A snapshot store that does not return the bytes it was given is
///   not a snapshot store.
/// - **`commit.gpgsign`** — a user who signs by default would have every internal
///   snapshot try to sign, which can prompt, stall, or simply fail.
/// - **hooks** — committing a snapshot must never fire the user's pre-commit hooks
///   (`--no-verify` is passed at the commit site).
fn git(project_root: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("--git-dir")
        .arg(git_dir(project_root))
        .arg("--work-tree")
        .arg(project_root)
        .args([
            "-c",
            "user.name=UmaDev",
            "-c",
            "user.email=umadev@local",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.safecrlf=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .output()
        .ok()
}

/// `true` when at least one checkpoint exists.
#[must_use]
pub fn has_checkpoints(project_root: &Path) -> bool {
    git_dir(project_root).join("HEAD").exists()
        && git(project_root, &["rev-parse", "--verify", "-q", "HEAD"])
            .is_some_and(|o| o.status.success())
}

/// Initialise the shadow repo on first use. Returns `false` (fail-open) when
/// `git` is missing or init fails.
fn ensure_init(project_root: &Path) -> bool {
    let gd = git_dir(project_root);
    if gd.join("HEAD").exists() {
        return true;
    }
    if std::fs::create_dir_all(&gd).is_err() {
        return false;
    }
    let ok = git(project_root, &["init", "-q"]).is_some_and(|o| o.status.success());
    if ok {
        // The shadow repo's own ignore file — keeps heavy dirs out of every
        // checkpoint regardless of whether the project ships a `.gitignore`.
        let info = gd.join("info");
        let _ = std::fs::create_dir_all(&info);
        let _ = std::fs::write(info.join("exclude"), SHADOW_EXCLUDES.join("\n"));
    }
    ok
}

/// Snapshot the whole workspace as a checkpoint labelled `label`. Returns the
/// short commit id, or `None` (fail-open) if `git` is unavailable. An empty
/// snapshot (nothing changed) still produces a checkpoint via `--allow-empty`.
///
/// A FAILED commit returns `None`, not the id of whatever HEAD happened to be. The
/// difference is load-bearing: callers treat the returned id as "the state I just saved",
/// and one of them ([`recover_abandoned_temp_rewind`]) then `reset --hard`s the work-tree
/// on the strength of that promise. Handing back a stale HEAD after an unwritable-shadow
/// -repo commit failure would turn "I saved your files" into a lie, and the reset that
/// trusted it would destroy them. `--allow-empty` means a *successful* commit always
/// exits 0, so a non-zero status is a real failure, never "nothing to do".
#[must_use]
pub fn create_checkpoint(project_root: &Path, label: &str) -> Option<String> {
    if !ensure_init(project_root) {
        return None;
    }
    stage_all_including_ignored(project_root)?;
    let commit = git(
        project_root,
        &[
            "-c",
            "user.email=umadev@local",
            "-c",
            "user.name=UmaDev",
            "commit",
            "-q",
            "--allow-empty",
            // A snapshot of the user's tree must never fire the user's own git hooks.
            "--no-verify",
            "-m",
            label,
        ],
    )?;
    if !commit.status.success() {
        return None;
    }
    let head = git(project_root, &["rev-parse", "--short", "HEAD"])?;
    head.status
        .success()
        .then(|| String::from_utf8_lossy(&head.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Snapshot the workspace at a PHASE boundary, but ONLY if the working tree
/// changed since the last checkpoint — an empty phase boundary produces no new
/// checkpoint. This is what the runner calls on every phase start so the
/// headless `run` / `continue` paths get the same per-phase rewind points the
/// TUI builds from its `PhaseStarted` handler, WITHOUT cluttering the rewind
/// list with a run of identical empty commits when several phases start
/// back-to-back before any file is written (e.g. the docs→spec gap).
///
/// Returns `Some(id)` when a NEW checkpoint was written, `None` when nothing
/// changed (or `git` is unavailable — fail-open, never blocks the pipeline).
/// The very first checkpoint of a fresh shadow repo is always written (so a
/// run always has a baseline to rewind to), even if the tree looks "clean"
/// relative to an empty HEAD.
#[must_use]
pub fn create_phase_checkpoint(project_root: &Path, label: &str) -> Option<String> {
    if !ensure_init(project_root) {
        return None;
    }
    // Stage everything (including .gitignore'd product files, minus the heavy
    // set — see `stage_all_including_ignored`), then ask whether the index
    // differs from HEAD. `git diff --cached --quiet` exits 0 = no staged changes,
    // 1 = changes. When there is no HEAD yet (fresh repo) the diff reports changes
    // (or errors), so the baseline checkpoint is taken.
    stage_all_including_ignored(project_root)?;
    let has_head = has_checkpoints(project_root);
    if has_head {
        let clean =
            git(project_root, &["diff", "--cached", "--quiet"]).is_some_and(|o| o.status.success());
        if clean {
            return None; // nothing changed since the last checkpoint
        }
    }
    git(
        project_root,
        &[
            "-c",
            "user.email=umadev@local",
            "-c",
            "user.name=UmaDev",
            "commit",
            "-q",
            "--allow-empty",
            // A snapshot of the user's tree must never fire the user's own git hooks.
            "--no-verify",
            "-m",
            label,
        ],
    )?;
    let head = git(project_root, &["rev-parse", "--short", "HEAD"])?;
    head.status
        .success()
        .then(|| String::from_utf8_lossy(&head.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The label of the snapshot [`begin_temp_rewind`] takes of the PRESENT before it
/// briefly rewinds the tree for evidence. Machinery, not a user rewind point.
pub const TEMP_REWIND_HEAD_LABEL: &str = "auto: 临时回退取证前的现场";

/// The label prefix of the per-step pre-state snapshot a red→green evidence contract
/// takes. Machinery, not a user rewind point.
pub const RED_GREEN_PRE_PREFIX: &str = "pre-step (red→green): ";

/// The label of the RESCUE snapshot the workspace heal
/// ([`recover_abandoned_temp_rewind`]) takes of the CURRENT work-tree *before* it resets
/// that tree back to the present.
///
/// Deliberately NOT an internal label (see the internal `is_internal_label` classifier): this snapshot is the
/// only copy of anything the user wrote while their tree was silently in the past, so it
/// MUST be visible in the rewind picker / `umadev history` and reachable by
/// `umadev rollback <id>`. The heal names it in the note it hands the user.
///
/// **Localized**, unlike [`TEMP_REWIND_HEAD_LABEL`] / [`RED_GREEN_PRE_PREFIX`] /
/// [`RUN_BASELINE_PREFIX`], and the difference is load-bearing. Those three are MACHINE
/// KEYS — the internal `is_internal_label` classifier and [`run_baseline`] match on them, so a label written under
/// one locale and matched under another would stop being recognised. This one is only ever
/// READ BY A HUMAN, in `umadev history` / the rewind picker — the very surface the (already
/// localized) heal note tells them to go look at. An English user being pointed at a
/// Chinese row is the bug.
#[must_use]
pub fn heal_rescue_label() -> String {
    umadev_i18n::tl("checkpoint.heal_rescue_label").to_string()
}

/// Whether a checkpoint label names an INTERNAL machinery snapshot — the temp-rewind
/// head snapshot and the per-step red→green pre-state. A run that verifies evidence
/// takes one of each PER STEP PER FIX ROUND, so leaving them in the user-facing list
/// buries the handful of checkpoints a human actually wants (`run-baseline`, per-phase)
/// under dozens of identical machine commits. They stay fully REACHABLE (the
/// restore/rewind validation reads the unfiltered set) — they are only hidden from the
/// list a person reads.
fn is_internal_label(label: &str) -> bool {
    label.starts_with(TEMP_REWIND_HEAD_LABEL) || label.starts_with(RED_GREEN_PRE_PREFIX)
}

/// How many user-facing checkpoints the picker shows.
const DISPLAY_CHECKPOINTS: usize = 50;

/// Scan windows for the user-facing list, widened until the DISPLAY cap is filled.
///
/// The cap must be applied AFTER the internal snapshots are filtered out, or a long run
/// hides the user's own checkpoints behind its machinery: each step × fix round writes
/// TWO internal commits (the temp-rewind head snapshot + the red→green pre-state), so
/// past ~100 step-rounds a fixed 200-commit window contains nothing BUT internal
/// commits, and `/rewind` shows the user an EMPTY list — with every one of their
/// checkpoints still sitting right there in the shadow repo.
///
/// So: widen the window until 50 user checkpoints are found or the history is exhausted.
/// Bounded (at most three `git log` calls, and only on a history deep enough to need
/// them) — a run that never reaches the first window pays exactly what it paid before.
const CHECKPOINT_SCAN_WINDOWS: &[usize] = &[200, 1_000, 5_000];

/// A shadow-repo history read is attempted this many times before it is reported
/// as unreadable. The read can transiently fail under load — a brief index lock, a
/// slow fsync — and a false "unreadable" on the WIDEST scan window makes
/// [`list_user_checkpoints_with`] give up and drop every real checkpoint out of the
/// `/rewind` picker (the newest window can legitimately be all-internal, so the
/// wider read is the only thing between a long run and an empty picker).
const GIT_READ_ATTEMPTS: usize = 3;

/// List checkpoints a USER may want to rewind to, newest first (capped at
/// the display-checkpoint limit). Internal machinery snapshots (see the internal
/// `is_internal_label` classifier)
/// are filtered out; they remain restorable by id. Empty when none / `git` missing.
#[must_use]
pub fn list_checkpoints(project_root: &Path) -> Vec<Checkpoint> {
    list_user_checkpoints_with(|window| try_list_checkpoints_limited(project_root, window))
}

fn list_user_checkpoints_with(
    mut scan: impl FnMut(usize) -> Option<Vec<Checkpoint>>,
) -> Vec<Checkpoint> {
    let mut newest: Vec<Checkpoint> = Vec::new();
    for &window in CHECKPOINT_SCAN_WINDOWS {
        let Some(scanned) = scan(window) else {
            // A transient read failure is not proof that history is exhausted.
            // The next wider window doubles as a bounded retry.
            continue;
        };
        let exhausted = scanned.len() < window; // the window covered the whole history
        newest = scanned
            .into_iter()
            .filter(|c| !is_internal_label(&c.label))
            .take(DISPLAY_CHECKPOINTS)
            .collect();
        // Enough to fill the picker, or there is simply nothing more to find.
        if newest.len() >= DISPLAY_CHECKPOINTS || exhausted {
            break;
        }
    }
    newest
}

/// [`list_checkpoints`] with an explicit cap. The 50-cap public form is for DISPLAY; the
/// restore/rollback VALIDATION uses a much larger cap so an older-but-valid checkpoint (a
/// long run with >50 commits) is still resettable-to - the display cap must never shrink the
/// set of ids a user is allowed to rewind to.
fn list_checkpoints_limited(project_root: &Path, limit: usize) -> Vec<Checkpoint> {
    try_list_checkpoints_limited(project_root, limit).unwrap_or_default()
}

/// A SINGLE shadow-repo history read, or `None` when the read itself failed (git
/// could not be spawned, or it exited non-zero). A successful read of an empty
/// range returns `Some(vec![])`: an UNREADABLE window and an EXHAUSTED one are
/// different facts, and only the former is a candidate for retry.
fn read_history_once(project_root: &Path, limit: usize) -> Option<Vec<Checkpoint>> {
    // `--all` so checkpoints that a rewind moved off the linear HEAD history
    // (the pre-rewind snapshot + any forward checkpoints, preserved under a
    // `umadev-saved-*` ref by restore_checkpoint) are still listed.
    let out = git(
        project_root,
        &[
            "log",
            "--all",
            "--pretty=%h%x1f%cI%x1f%s",
            "-n",
            &limit.to_string(),
        ],
    )?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\u{1f}');
                Some(Checkpoint {
                    id: parts.next()?.to_string(),
                    when: parts.next()?.to_string(),
                    label: parts.next().unwrap_or("").to_string(),
                })
            })
            .collect(),
    )
}

/// [`read_history_once`] with a bounded retry so a transient read failure on a
/// scan window is not mistaken for "no history" — see [`GIT_READ_ATTEMPTS`]. A
/// genuinely empty range returns on the first attempt (it is `Some`, not a
/// failure), so this never spins on real history.
fn try_list_checkpoints_limited(project_root: &Path, limit: usize) -> Option<Vec<Checkpoint>> {
    read_history_with_retry(|| read_history_once(project_root, limit))
}

fn read_history_with_retry(
    mut once: impl FnMut() -> Option<Vec<Checkpoint>>,
) -> Option<Vec<Checkpoint>> {
    for _ in 0..GIT_READ_ATTEMPTS {
        if let Some(history) = once() {
            return Some(history);
        }
    }
    None
}

/// Resolve `id` to the CANONICAL short id of a checkpoint in `list` — the only commits a
/// reset may ever move the work-tree to. `None` when `id` names none of them.
///
/// A caller may pass the short handle we printed or a fuller SHA they pasted, so a prefix
/// match in EITHER direction resolves. Everything else is refused.
///
/// # Why this returns the id instead of a bool
///
/// It used to answer `bool`, and the caller then `reset --hard`ed the RAW `id` it had been
/// handed. That made the guard a rubber stamp, because `id.starts_with(cid)` is true of
/// every git revision EXPRESSION built on a known checkpoint: `umadev rollback 3b599bd^`
/// and `umadev rollback 3b599bd~1` both passed the check and then reset the work-tree to a
/// commit that is NOT a checkpoint at all — the exact thing the docstring swore could not
/// happen. Resolving to the canonical `cid` and resetting to THAT closes it structurally:
/// the reset target is a commit this workspace named, never a string the caller composed.
///
/// The syntax gate makes it explicit anyway: a checkpoint id is pure hex, so `^`, `~1`,
/// `@{1}`, `HEAD`, `refs/heads/…`, `:/message` and every other revision operator is
/// rejected before any matching is attempted.
///
/// **Ambiguity is a refusal, not a coin-flip.** If a short id prefixes two different
/// checkpoints we return `None` rather than pick one — a `reset --hard` to the wrong commit
/// discards the user's files.
fn resolve_checkpoint_id(id: &str, list: &[Checkpoint]) -> Option<String> {
    let id = id.trim().to_ascii_lowercase();
    // A commit id is hex and nothing else. This one line is what rejects `3b599bd^`,
    // `3b599bd~1`, `HEAD~999`, a branch name, and every other revision expression.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut hit: Option<&str> = None;
    for c in list {
        let cid = c.id.as_str();
        if !(cid == id || cid.starts_with(&id) || id.starts_with(cid)) {
            continue;
        }
        match hit {
            None => hit = Some(cid),
            // The same commit listed twice (`git log --all` can reach it by several refs).
            Some(prev) if prev == cid => {}
            // Two DIFFERENT checkpoints match — the handle is ambiguous. Refuse.
            Some(_) => return None,
        }
    }
    hit.map(str::to_string)
}

/// `true` when `id` resolves to a known checkpoint (see [`resolve_checkpoint_id`]) — the
/// predicate form, for callers that only need to know whether a reset would be legitimate.
fn id_is_known_checkpoint(id: &str, list: &[Checkpoint]) -> bool {
    resolve_checkpoint_id(id, list).is_some()
}

/// Rewind the workspace files to checkpoint `id` (`git reset --hard`). The
/// CURRENT state is auto-checkpointed first, so a rewind is itself undoable.
/// Untracked / excluded paths (`node_modules`, …) are left untouched.
///
/// `id` MUST name a checkpoint returned by [`list_checkpoints`]. It is RESOLVED to that
/// checkpoint's canonical id (via the internal resolver) and the reset targets the RESOLVED
/// commit — never the raw string the caller passed. That is what makes the guarantee real:
/// a bool guard plus a reset to the caller's own text let `rollback 3b599bd^` and
/// `rollback 3b599bd~1` through, because a revision EXPRESSION built on a known checkpoint
/// starts with a known checkpoint. Resolving first means the reset target is always a
/// commit this workspace itself named.
///
/// # The pre-snapshot is load-bearing
///
/// `cmd_rollback` tells the user "we snapshotted first, so this is itself undoable". If the
/// snapshot cannot be taken, that sentence is a lie and the `reset --hard` that follows it
/// destroys their uncommitted work with no way back. So a failed pre-snapshot ABORTS the
/// restore and says so — exactly as [`begin_temp_rewind`] (`?` on the same call) and
/// [`recover_abandoned_temp_rewind`] (stands down on the same call) already did. Losing a
/// rollback is always better than losing the user's code.
///
/// # Errors
/// Returns `Err` with a localized human message when no checkpoints exist, the id is not a
/// known checkpoint, the pre-restore snapshot could not be taken, or `git` is unavailable /
/// the reset failed.
pub fn restore_checkpoint(project_root: &Path, id: &str) -> Result<(), String> {
    if !has_checkpoints(project_root) {
        return Err(umadev_i18n::tl("checkpoint.none_to_restore").to_string());
    }
    // P1-3: resolve `id` against the known-checkpoint set BEFORE doing anything destructive.
    // `git reset --hard <rev>` accepts ANY revision (`HEAD~999`, a branch, `<known>^`), so
    // without this the work-tree could be rewound to an unintended commit and drop files.
    // Validate against a MUCH larger set than the 50 we DISPLAY: a long run can create >50
    // checkpoints, and a user must still be able to rewind to an older valid one (the display
    // cap is cosmetic, not a security boundary - the id must still be a real UmaDev-reachable
    // commit, just not necessarily in the newest 50).
    let known = list_checkpoints_limited(project_root, 1000);
    let Some(target) = resolve_checkpoint_id(id, &known) else {
        return Err(umadev_i18n::tlf("checkpoint.unknown_id", &[id]));
    };
    // Make the rewind reversible AND keep forward checkpoints reachable: snapshot
    // the present, then anchor that snapshot (whose history includes every
    // checkpoint newer than `target`) under a `umadev-saved-<sha>` ref BEFORE the
    // reset. Otherwise `reset --hard` would orphan them and `list_checkpoints`
    // (which uses `git log --all`) couldn't show them.
    //
    // No snapshot ⇒ no reset. See the "load-bearing" section above.
    if create_checkpoint(
        project_root,
        &umadev_i18n::tlf("checkpoint.rollback_pre_label", &[&target]),
    )
    .is_none()
    {
        return Err(umadev_i18n::tl("checkpoint.restore_snapshot_failed").to_string());
    }
    if let Some(sha) = git(project_root, &["rev-parse", "--short", "HEAD"])
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
    {
        let branch = format!("umadev-saved-{sha}");
        let _ = git(project_root, &["branch", "-f", &branch, "HEAD"]);
    }
    let out = git(project_root, &["reset", "--hard", &target])
        .ok_or_else(|| umadev_i18n::tl("checkpoint.git_unavailable").to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

// =====================================================================
// Run-level rollback — "undo the whole run" over the SHADOW repo
// (Wave 6 deliverable 2: real-git rollback, decoupled from the user's .git)
// =====================================================================

/// The label prefix that marks a **run baseline** checkpoint — the snapshot of
/// the workspace taken the instant a workspace-mutating run begins, BEFORE the
/// base writes anything. `rollback` rewinds to the newest of these, so the user
/// can undo an entire run's worth of edits in one move.
///
/// Kept distinct from ordinary per-phase checkpoints (which use a `phase: …` /
/// freeform label) so [`run_baseline`] can find it deterministically.
pub const RUN_BASELINE_PREFIX: &str = "run-baseline: ";

/// Take a **run-baseline** checkpoint — the pre-run snapshot `rollback` rewinds
/// to. Call this once at the very start of a workspace-mutating run, before any
/// phase writes. It is a normal shadow-repo checkpoint with a recognisable
/// label, so it composes with the existing per-phase rewind points and never
/// touches the user's own `.git`. Fail-open: `None` when `git` is unavailable.
///
/// `slug` only flavours the label; the baseline is identified by its prefix.
#[must_use]
pub fn create_run_baseline(project_root: &Path, slug: &str) -> Option<String> {
    let label = format!("{RUN_BASELINE_PREFIX}{slug}");
    create_checkpoint(project_root, &label)
}

/// The newest run-baseline checkpoint, if one exists — the target `rollback`
/// rewinds to. Newest-first scan of [`list_checkpoints`] for the baseline
/// prefix. `None` when no run has recorded a baseline yet.
#[must_use]
pub fn run_baseline(project_root: &Path) -> Option<Checkpoint> {
    // Scan the UNFILTERED, deep list: a long run interleaves many internal machinery
    // snapshots (red→green pre-states, temp-rewind heads) between the baseline and
    // HEAD, and the baseline must stay findable behind all of them — the run diff, the
    // scope floor, and `rollback` all hang off it.
    list_checkpoints_limited(project_root, 1000)
        .into_iter()
        .find(|c| c.label.starts_with(RUN_BASELINE_PREFIX))
}

/// Ensure THIS run has its own fresh run-baseline — the pre-run snapshot
/// `rollback` rewinds to — so a rollback only ever reverts the CURRENT run's
/// changes, never a prior run's (P1/P2).
///
/// The old logic took a baseline only when `run_baseline().is_none()`, i.e. once
/// per WORKSPACE: a second run in the same workspace reused the first run's
/// baseline, so rolling back the second run over-reverted everything since the
/// first run started. Here we instead take a **fresh** baseline at each new run's
/// start, while staying idempotent WITHIN a single run:
///
/// - If the newest run-baseline is ALSO the shadow repo's current HEAD, we are
///   still sitting at this run's start — no phase checkpoint has been committed
///   since the baseline was taken (e.g. the legacy `run_clarify` → `run_initial_block`
///   hand-off calls this twice at the Research boundary). Re-use it; do NOT stack
///   a second baseline that would move `rollback`'s target to mid-run state.
/// - Otherwise (no baseline yet, OR HEAD has advanced past the last baseline
///   because a prior run committed phase checkpoints) this is a NEW run's start:
///   snapshot fresh so `rollback` targets only this run.
///
/// Fail-open: `git` unavailable → `None`, never blocks the run. `slug` only
/// flavours the label; the baseline is identified by [`RUN_BASELINE_PREFIX`].
#[must_use]
pub fn ensure_run_baseline(project_root: &Path, slug: &str) -> Option<String> {
    if let Some(existing) = run_baseline(project_root) {
        if baseline_is_current_head(project_root, &existing.id) {
            // Still at this run's start (nothing checkpointed since) → keep it.
            return Some(existing.id);
        }
    }
    create_run_baseline(project_root, slug)
}

/// `true` when `baseline_id` names the shadow repo's current HEAD commit — i.e.
/// no checkpoint has been committed since that baseline was taken, so we are
/// still at the run's start rather than mid-run. Prefix-tolerant in either
/// direction (short-id abbreviations, like [`id_is_known_checkpoint`]).
///
/// Fail-open: an unreadable HEAD reads as "not current" so [`ensure_run_baseline`]
/// takes a fresh baseline — the safe direction (never silently reuse a stale one).
fn baseline_is_current_head(project_root: &Path, baseline_id: &str) -> bool {
    let Some(head) = git(project_root, &["rev-parse", "--short", "HEAD"])
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    let id = baseline_id.trim();
    !id.is_empty() && (id == head || id.starts_with(&head) || head.starts_with(id))
}

/// Roll the workspace back to the most recent **run baseline** — a true,
/// reversible "undo this whole run". This is the user-facing `rollback`:
///
/// - Rewinds tracked files to the pre-run snapshot via the SHADOW repo (so the
///   user's own `.git` history is never rewritten — only the working-tree files
///   the run wrote are reverted).
/// - Is itself undoable: [`restore_checkpoint`] auto-snapshots the present and
///   anchors forward checkpoints under a `umadev-saved-*` ref first, so a
///   rollback can be rolled forward again.
/// - Leaves untracked/excluded paths (`node_modules`, the user's `.git`, the
///   isolation branch pointer) untouched.
///
/// Fail-open by contract: when no run baseline exists (no run has started, or
/// `git` is unavailable) it returns an actionable `Err` string and changes
/// nothing — it NEVER deletes a remote, force-pushes, or rewrites user commits.
///
/// # Errors
/// Returns `Err` with a human message when there is no recorded baseline to
/// roll back to, or the underlying restore fails.
pub fn rollback_run(project_root: &Path) -> Result<Checkpoint, String> {
    let baseline = run_baseline(project_root)
        .ok_or_else(|| "还没有可回滚的运行基线(本工作区尚未开始过会改动文件的运行)".to_string())?;
    restore_checkpoint(project_root, &baseline.id)?;
    Ok(baseline)
}

// =====================================================================
// Scoped, reversible TEMPORARY rewind — "what was true BEFORE this step?"
// =====================================================================

/// A **scoped, reversible rewind** of the workspace to an earlier checkpoint, used to
/// ask a question about the PAST that only the past can answer: *did this test fail
/// before the change?*
///
/// The value is an RAII guard. On [`begin_temp_rewind`] the CURRENT tree is first
/// snapshotted as its own checkpoint (so the present is never at risk), anchored under
/// a `umadev-saved-*` ref (so it can never be orphaned), and only then is the tree
/// reset to the target. [`Self::restore`] — and, as a backstop, `Drop` — puts the tree
/// back exactly as it was, so an early return, an `?`, or a panic in the middle of the
/// rewound window still leaves the workspace at head.
///
/// Only files the SHADOW repo tracks move. Dependency trees (`node_modules`,
/// `target`, `.venv`), the user's own `.git`, and `.umadev/` itself are excluded from
/// the shadow repo, so they are untouched — which is precisely what makes the rewound
/// tree *runnable*: the source goes back in time, the toolchain does not.
#[derive(Debug)]
pub struct TempRewind {
    root: PathBuf,
    /// The checkpoint id of the snapshot taken at [`begin_temp_rewind`] — the state
    /// [`Self::restore`] returns to.
    head: String,
    /// Set once the tree is back at `head`, so `Drop` does not restore twice.
    restored: bool,
}

impl TempRewind {
    /// The checkpoint id of the pre-rewind (head) snapshot this guard restores to.
    #[must_use]
    pub fn head_id(&self) -> &str {
        &self.head
    }

    /// Put the workspace back at the state captured when the rewind began. `true` on
    /// success. Idempotent with `Drop` (which restores only if this was not called).
    ///
    /// A FAILED restore is never quiet: it raises the workspace-in-the-past flag
    /// ([`workspace_is_in_past`]) and a user-facing notice, because the caller is about
    /// to go on writing onto a tree that is silently in the past (see
    /// the internal failed-restore reporter).
    pub fn restore(mut self) -> bool {
        let ok = Self::reset_hard(&self.root, &self.head);
        self.restored = ok;
        if ok {
            clear_temp_rewind_marker(&self.root);
            clear_workspace_in_past(&self.root);
        } else {
            report_failed_restore(&self.root, &self.head);
        }
        ok
    }

    /// `git reset --hard <id>` over the shadow repo. `true` iff git ran and succeeded.
    fn reset_hard(root: &Path, id: &str) -> bool {
        git(root, &["reset", "--hard", "-q", id]).is_some_and(|o| o.status.success())
    }
}

impl Drop for TempRewind {
    /// SAFETY NET: if the guard is dropped without an explicit [`Self::restore`] — an
    /// early return, a `?`, a panic inside the rewound window — the workspace is still
    /// put back at head. A rewind must never be able to survive its own scope.
    ///
    /// `Drop` covers everything the PROCESS can still observe. It does NOT cover a
    /// SIGKILL / OOM / a closed terminal / a power loss — no destructor runs then, and
    /// the user's tracked source would be left sitting in the past with no marker and no
    /// way back. That hole is closed OUTSIDE the process by the crash marker written
    /// before the reset (see [`begin_temp_rewind`] / [`recover_abandoned_temp_rewind`]).
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        if Self::reset_hard(&self.root, &self.head) {
            clear_temp_rewind_marker(&self.root);
            clear_workspace_in_past(&self.root);
        } else {
            report_failed_restore(&self.root, &self.head);
        }
    }
}

/// A rewind we could NOT undo, in-process — the loudest thing this module can say.
///
/// The workspace is sitting IN THE PAST and the process is still running: whatever the
/// caller does next writes onto a tree that is not the user's present. A `tracing::warn!`
/// is not enough — under the TUI it goes to a log FILE, so the run went on driving further
/// steps onto a reverted tree while the only record of it was invisible. This raises the
/// process-wide flag the driver checks ([`workspace_is_in_past`]) AND leaves a
/// user-facing notice, so the run STOPS instead of accumulating writes on a past tree.
///
/// The crash marker is deliberately KEPT (not cleared) so the next start's heal
/// ([`recover_abandoned_temp_rewind`]) can still put the tree back.
fn report_failed_restore(root: &Path, head: &str) {
    tracing::warn!(
        head = %head,
        root = %root.display(),
        "temporary rewind could not be restored; the workspace is at an earlier \
         checkpoint — the crash marker is kept so the next start restores the present, \
         and `umadev rollback` / the saved checkpoint refs still hold every state"
    );
    // RETRYABLE: `head` is a checkpoint we just took, the marker is kept, and the next
    // start's heal resets to it. "Restart UmaDev and it will put the tree back" is TRUE here.
    mark_workspace_in_past(root, InPastReason::Retryable);
    record_workspace_notice(umadev_i18n::tlf(
        "checkpoint.temp_rewind_restore_failed",
        &[head],
    ));
}

// ---------------------------------------------------------------------
// "The workspace is in the past" — a STOP signal, not just a log line
// ---------------------------------------------------------------------

/// The workspaces this process knows to be stuck at an earlier checkpoint: a temporary
/// rewind was taken and could NOT be undone. Cleared per-root the moment that tree is
/// provably back at the present.
///
/// Keyed by ROOT rather than a bare process-wide flag because a process legitimately
/// touches several workspaces (`--project-root elsewhere`), and one stranded tree must
/// not stop a run in an unrelated, healthy one.
///
/// It has to be an out-of-band signal at all because the failure is discovered DEEP inside
/// a step's evidence check (a `TempRewind::restore` / `Drop` several frames below a
/// verifier), while the decision it must change — *drive no further steps* — belongs to
/// the scheduler at the top. A return value cannot cross `Drop`. So the fact travels the
/// same way the workspace notices above do, and the scheduler reads it once per step.
///
/// Fail-open by shape: a poisoned lock reads as "not stuck" (today's behaviour), and a
/// signal that is never raised changes nothing.
static WORKSPACE_IN_PAST: std::sync::Mutex<Vec<(PathBuf, InPastReason)>> =
    std::sync::Mutex::new(Vec::new());

/// WHY a workspace is stranded in the past — and therefore what the user must do about it.
///
/// The halt note used to be one string for both branches, and it said *"Restart UmaDev and
/// it will put the tree back."* That is true of exactly one of them. In the other it is a
/// lie that costs the user their product: a stale marker whose recorded head this workspace
/// cannot name (they deleted `.umadev/checkpoints.git` to reclaim disk, or rsynced the
/// workspace without it) re-raises the halt on EVERY process start, forever, and every
/// restart reaches the identical verdict. The only escape was to hand-`rm` a file nothing
/// ever told them about.
///
/// So the reason travels with the flag, and the note tells the truth about the branch the
/// user is actually in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InPastReason {
    /// A restore that FAILED but can be retried: the marker names a head this workspace can
    /// still identify, so the next start's heal will try again — and "restart UmaDev" is
    /// honest advice.
    Retryable,
    /// A halt no restart can lift: the marker's recorded head is not a checkpoint this
    /// workspace can name, so [`recover_abandoned_temp_rewind`] refuses to reset (correctly
    /// — it will never `reset --hard` to an unvalidated ref) and reaches the same refusal on
    /// every later start. The user has to clear it, and needs to be told how
    /// (`umadev doctor --fix`, which is what [`clear_temp_rewind_state`] serves).
    Unrecoverable,
}

/// The KEY a root is remembered under — canonicalized, so every spelling of the same
/// directory names the same entry.
///
/// The raiser and the reader reach this signal by different routes: the heal is handed
/// `--project-root .` (or a symlinked path, or a trailing slash), while the run driver
/// polls `options.project_root`. A raw `PathBuf` compare makes those two MISS each other
/// — the flag is raised on one spelling and read on another, so the halt silently never
/// fires and the tree goes on taking writes. Canonicalize both ends and they agree.
///
/// Fail-open: an unresolvable path (a root that no longer exists) is used verbatim — the
/// worst case is the old behaviour, never an error.
fn in_past_key(project_root: &Path) -> PathBuf {
    std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf())
}

/// Record that `project_root` is stuck at an earlier checkpoint, and WHY (see
/// the internal workspace-in-past marker / [`InPastReason`]). Anything written to that tree after this point
/// is written on top of files that are not the user's present.
///
/// [`InPastReason::Unrecoverable`] WINS over [`InPastReason::Retryable`] if both are raised
/// for the same root: the escape hatch must be shown whenever any branch needs it, and
/// telling a stuck user "just restart" when a restart cannot help is the failure this exists
/// to fix.
pub fn mark_workspace_in_past(project_root: &Path, reason: InPastReason) {
    let key = in_past_key(project_root);
    if let Ok(mut roots) = WORKSPACE_IN_PAST.lock() {
        if let Some(entry) = roots.iter_mut().find(|(r, _)| *r == key) {
            if reason == InPastReason::Unrecoverable {
                entry.1 = InPastReason::Unrecoverable;
            }
            return;
        }
        // Bounded: a handful of roots per process, at most.
        if roots.len() < 16 {
            roots.push((key, reason));
        }
    }
}

/// `true` while a failed rewind restore has left `project_root` at an earlier checkpoint.
/// The run driver polls this and STOPS scheduling work — a tree in the past must never
/// accumulate more writes.
#[must_use]
pub fn workspace_is_in_past(project_root: &Path) -> bool {
    workspace_in_past_reason(project_root).is_some()
}

/// WHY `project_root` is stranded, or `None` when it is not. The halt note reads this to
/// tell the user the truth about which branch they are in (see [`InPastReason`]).
#[must_use]
pub fn workspace_in_past_reason(project_root: &Path) -> Option<InPastReason> {
    let key = in_past_key(project_root);
    WORKSPACE_IN_PAST
        .lock()
        .ok()?
        .iter()
        .find(|(r, _)| *r == key)
        .map(|(_, reason)| *reason)
}

/// The loud, actionable halt note for a tree stranded in the past — or `None` when the tree
/// is fine (the overwhelming default).
///
/// **ONE definition, for every write-capable surface.** The halt is a promise —
/// *"no further work will be driven onto this tree until it is back at the present"* — and a
/// promise kept on one surface and broken on another is not a promise. It was: only the
/// `/run` director loop checked it, while the DEFAULT surface people actually live on (the
/// TUI chat turn, which is write-capable — the base reaches for `Write`/`Edit` and
/// `react_to_first_write` promotes the turn to a build) checked nothing at all. So the heal
/// stood down, the flag went up, the user typed "fix the login bug", and the base wrote onto
/// a tree in the past.
///
/// Every surface that can let the base write MUST call this and refuse when it answers.
#[must_use]
pub fn workspace_in_past_note(project_root: &Path) -> Option<String> {
    let key = match workspace_in_past_reason(project_root)? {
        InPastReason::Retryable => "checkpoint.workspace_in_past_halt",
        InPastReason::Unrecoverable => "checkpoint.workspace_in_past_halt_unrecoverable",
    };
    Some(umadev_i18n::tl(key).to_string())
}

/// Clear the in-the-past signal for `project_root` — called ONLY where that tree is
/// provably back at the present.
///
/// There are exactly TWO such places, and both have just verified the tree they are
/// clearing for: [`TempRewind::restore`] / its `Drop` (which only clear after a
/// `reset --hard` back to the very head snapshot THEY took — the present they themselves
/// stranded), and [`recover_abandoned_temp_rewind`] (which only clears after resetting to
/// the head the crash marker recorded). Nothing else may call it: a second rewind of an
/// already-stranded tree would otherwise "restore" to a head that snapshots the PAST and
/// clear the halt on a tree that is still in the past — which is precisely why
/// [`begin_temp_rewind`] refuses to start while this flag is up.
///
/// The ONE other caller is [`clear_temp_rewind_state`] — the user's explicit escape from a
/// halt that no restart can lift. It is not "nothing else may call it" being broken: it is
/// the human saying, deliberately, that this tree is now as they want it.
pub fn clear_workspace_in_past(project_root: &Path) {
    let key = in_past_key(project_root);
    if let Ok(mut roots) = WORKSPACE_IN_PAST.lock() {
        roots.retain(|(r, _)| *r != key);
    }
}

/// What [`clear_temp_rewind_state`] found, and what it did about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TempRewindState {
    /// No crash marker — nothing to clear, nothing wrong.
    Clean,
    /// A marker whose recorded head IS a known checkpoint. **Left alone.** The automatic
    /// heal can still put this tree back at the present, and deleting the marker would throw
    /// that away — the user would be stranded in the past with the only map to the present
    /// torn up. Carries the head it names, so the caller can say so.
    Recoverable {
        /// The checkpoint that holds the user's present.
        head: String,
    },
    /// A marker naming a head this workspace cannot identify — the shadow repo is gone or the
    /// marker is corrupt. The heal will refuse to reset to it on every start, forever, so the
    /// halt is permanent until a human lifts it. **Cleared.** Deleting the marker touches no
    /// file in the work-tree; it only stops UmaDev from re-raising a halt it can never act on.
    ClearedUnrecoverable {
        /// The unidentifiable head the marker named.
        head: String,
    },
}

/// Diagnose the temp-rewind crash marker for `project_root`, and clear it **only** when it
/// is provably unrecoverable (see [`TempRewindState`]).
///
/// This is the way out of the permanent halt, and it exists because there wasn't one. Repro:
/// a stale marker plus a deleted `.umadev/checkpoints.git` (reclaimed disk, or a workspace
/// rsynced without it) means [`recover_abandoned_temp_rewind`] finds a head it cannot name,
/// correctly refuses to `reset --hard` to an unvalidated ref, and raises the halt — on EVERY
/// process start. Every `umadev run` then aborts immediately, no verb cleared it, and
/// `umadev doctor` did not even look. The only escape was to hand-`rm` a file the user was
/// never told about.
///
/// **It never deletes a marker the heal could still act on.** A `Recoverable` marker is the
/// only record of where the user's present is; clearing that would be the corruption, not
/// the cure. Fail-open at every edge: an unreadable / unparseable marker is treated as
/// unrecoverable (it is), and a delete that fails leaves the state exactly as it was.
///
/// `dry_run` reports without touching anything — what `umadev doctor` does before the user
/// asks for `--fix`.
pub fn clear_temp_rewind_state(project_root: &Path, dry_run: bool) -> TempRewindState {
    let path = project_root.join(TEMP_REWIND_MARKER_REL);
    let Ok(body) = std::fs::read_to_string(&path) else {
        return TempRewindState::Clean;
    };
    // A marker we cannot even parse names no head at all — it can never be acted on.
    let head = serde_json::from_str::<TempRewindMarker>(&body)
        .map(|m| m.head)
        .unwrap_or_default();
    let recoverable = !head.is_empty()
        && has_checkpoints(project_root)
        && id_is_known_checkpoint(&head, &list_checkpoints_limited(project_root, 1000));
    if recoverable {
        return TempRewindState::Recoverable { head };
    }
    if !dry_run {
        let _ = std::fs::remove_file(&path);
        clear_workspace_in_past(project_root);
    }
    TempRewindState::ClearedUnrecoverable { head }
}

// ---------------------------------------------------------------------
// The temp-rewind CRASH MARKER — the only thing that survives a SIGKILL
// ---------------------------------------------------------------------

/// Workspace-relative path of the temp-rewind crash marker.
pub const TEMP_REWIND_MARKER_REL: &str = ".umadev/temp-rewind.json";

/// How long an UNPROBEABLE marker (we could not tell whether its owner is alive)
/// must sit before it is treated as abandoned. Far beyond the rewound window
/// itself (bounded by `verify::RED_TEST_TIMEOUT_SECS`), so a live run is never
/// mistaken for a corpse.
const TEMP_REWIND_STALE_SECS: u64 = 900;

/// The on-disk crash marker: written BEFORE the tree is reset into the past, and
/// deleted the moment it is back at head. Its ONLY job is to make a rewind that
/// outlived its process **recoverable**: it names the commit that holds the
/// present (`head`), so any later start can put the source tree back.
///
/// The owner identity is `{pid, host, boot}` — the SAME triple the single-writer lock
/// records, and for the same reason. A bare PID is not an identity: after a reboot the
/// OS restarts PIDs low, so the pid this marker recorded is very likely handed to some
/// unrelated daemon. A liveness probe would then answer "alive", the recovery would
/// stand down, and the user's tracked source would sit in the past FOREVER (every later
/// start reaching the same verdict). `boot` makes that impossible: a marker from a
/// prior boot is abandoned by definition, whatever its PID now answers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TempRewindMarker {
    /// The checkpoint holding the PRESENT — what a recovery resets back to.
    pub head: String,
    /// The checkpoint the tree was rewound TO (diagnostic only).
    pub to: String,
    /// The pid of the process that took the rewind — the liveness probe's subject.
    /// Only meaningful together with `host` + `boot` (see the struct docs).
    pub pid: u32,
    /// UNIX seconds at which the rewind began (the age fallback's clock).
    pub started_at: u64,
    /// The BOOT the rewind was taken in (from the run-lock boot identity). A marker whose
    /// boot differs from ours was written before a reboot — its owner is dead, and its
    /// PID now belongs to someone else. Empty = unknown (an older marker, or a platform
    /// with no boot id): reboot-detection then simply does not apply and the PID/age
    /// rules stand alone.
    #[serde(default)]
    pub boot: String,
    /// The HOST the rewind was taken on (from the run-lock host identity). A workspace on
    /// a shared/network filesystem can carry a marker from another machine, whose
    /// process table we cannot probe (and whose boot id naturally differs from ours —
    /// which must NOT be read as "rebooted"). Empty = unknown (treated as this host).
    #[serde(default)]
    pub host: String,
}

/// UNIX seconds, or 0 when the clock is unreadable.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Write the crash marker. Fail-open: an unwritable marker does not stop the rewind
/// (the in-process `Drop` guard is still the primary restore path) — it only removes
/// the out-of-process backstop.
fn write_temp_rewind_marker(root: &Path, head: &str, to: &str) {
    let marker = TempRewindMarker {
        head: head.to_string(),
        to: to.to_string(),
        pid: std::process::id(),
        started_at: now_secs(),
        boot: crate::run_lock::boot_id(),
        host: crate::run_lock::hostname(),
    };
    let path = root.join(TEMP_REWIND_MARKER_REL);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_string(&marker) {
        let _ = std::fs::write(path, body);
    }
}

/// Delete the crash marker — called ONLY after the tree is verified back at head.
fn clear_temp_rewind_marker(root: &Path) {
    let _ = std::fs::remove_file(root.join(TEMP_REWIND_MARKER_REL));
}

/// Workspace-integrity notices raised OUT OF BAND — outside any turn, before any UI exists.
///
/// The workspace heal ([`recover_abandoned_temp_rewind`]) runs at process start and again
/// when a run takes the single-writer lock. Both points are *underneath* the surface the user
/// is actually looking at: a `tracing::warn!` goes to a log FILE when the TUI owns the
/// terminal, and a `eprintln!` at startup is wiped by the alternate screen a moment later. So
/// "your source tree was silently in the past and we put it back" — the single most important
/// thing UmaDev can say about a user's own files — was being said to nobody.
///
/// This is the hand-off: whoever raises the note leaves it here, and the surface that CAN
/// speak (the TUI transcript) drains it. Fail-open: a poisoned lock drops the note rather than
/// panicking a recovery path.
static WORKSPACE_NOTICES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Leave a workspace-integrity note for the user-facing surface to show (see
/// the internal workspace-notice queue). Bounded — a note is only ever raised by a recovery attempt.
pub fn record_workspace_notice(note: String) {
    if let Ok(mut q) = WORKSPACE_NOTICES.lock() {
        // A hard cap so a pathological loop can never grow this without bound.
        if q.len() < 16 && !q.contains(&note) {
            q.push(note);
        }
    }
}

/// Take every pending workspace-integrity note (see the internal notice queue). The caller MUST
/// surface what it takes — these are drained, not copied.
#[must_use]
pub fn take_workspace_notices() -> Vec<String> {
    WORKSPACE_NOTICES
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

/// Whether a marker's owner is **provably gone**, so its rewind may be undone.
///
/// The owner verdict is [`crate::run_lock::classify_claim_owner`] — the SINGLE
/// owner-liveness rule (host → boot → PID → age), shared with the single-writer run lock.
/// One question ("is the process that wrote this claim still alive?"), one answer: the
/// lock and this marker describe the same process, and two rules that disagree end with
/// one of them reclaiming a claim whose owner is alive.
///
/// Pure + fully injectable (the boot id, the hostname, our pid, the clock, and the
/// liveness answer are all parameters) — the reboot / PID-reuse / shared-workspace /
/// age-fallback branches are all DECIDABLE in a test, without a reboot and without a real
/// corpse to probe.
///
/// The one thing this adds on top of the shared rule is the age window itself
/// ([`TEMP_REWIND_STALE_SECS`], measured from the marker's own `started_at`), which is
/// this claim's — a rewind window is minutes, a pipeline block is hours.
fn marker_is_abandoned(
    m: &TempRewindMarker,
    now: u64,
    boot: &str,
    host: &str,
    self_pid: u32,
    alive: Option<bool>,
) -> bool {
    // Never let a pid-0 marker be "probed" (on Unix `kill -0 0` hits the process GROUP).
    let alive = if m.pid == 0 { None } else { alive };
    match crate::run_lock::classify_claim_owner(
        crate::run_lock::ClaimOwner {
            pid: m.pid,
            host: &m.host,
            boot: &m.boot,
        },
        host,
        boot,
        self_pid,
        alive,
    ) {
        crate::run_lock::OwnerLiveness::Live => false,
        crate::run_lock::OwnerLiveness::Abandoned => true,
        crate::run_lock::OwnerLiveness::AgeOnly => older_than_temp_rewind_stale(m, now),
    }
}

/// The age fallback: a marker we cannot attribute to a live owner is abandoned once it
/// is older than [`TEMP_REWIND_STALE_SECS`]. A marker with no usable clock (`started_at
/// == 0`) is never aged out — we would be guessing.
fn older_than_temp_rewind_stale(m: &TempRewindMarker, now: u64) -> bool {
    m.started_at != 0 && now.saturating_sub(m.started_at) > TEMP_REWIND_STALE_SECS
}

/// **Restore the present** after a temporary rewind whose process died inside the
/// rewound window (SIGKILL / OOM / a closed terminal / a reboot — no destructor ran).
///
/// Without this, the user's own tracked source files are left reverted to an earlier
/// step's state with no marker, no message, and no recovery — and `rollback` moves in
/// the WRONG direction. So: read the marker, and if the process that wrote it is
/// **provably gone** (as determined by the internal abandonment classifier), `reset --hard` back to the head snapshot
/// it recorded. Called at process start (against the verb's resolved workspace) and
/// again when a run takes the single-writer lock.
///
/// Returns a localized note for the caller to SURFACE whenever there is something the
/// user must know — the tree was put back, **or** we found a rewind we could not undo.
/// A workspace stuck in the past is never a silent condition.
///
/// SAFETY, in order:
/// - No marker → `None`, nothing touched (the overwhelmingly common case).
/// - The owner is alive / not provably gone → `None`, nothing touched. We never yank the
///   tree out from under a live run.
/// - The recorded head is not a known checkpoint → `None` and the marker is KEPT: a
///   corrupt marker can never `reset --hard` the tree to an arbitrary ref.
/// - **The CURRENT tree is snapshotted BEFORE the reset** ([`heal_rescue_label`]), and if
///   that snapshot cannot be taken we STAND DOWN and do not reset at all (see below).
/// - The reset FAILS → the marker is kept (a later start can try again) and the caller
///   gets a note naming the marker and the manual way out. Fail-open at every edge.
///
/// # The heal must not destroy what it heals
///
/// A `reset --hard` is a DESTRUCTIVE act on the working tree, and the tree it acts on here
/// is not the one the crashed run left behind — it is whatever the user has since made of
/// it. The window between the crash and this heal is arbitrarily long, it is armed on
/// EVERY process start (including `umadev hook`, fired on every base tool call, and
/// `umadev ci`, fired from `.git/hooks/pre-commit`), and in that window the user sees their
/// source reverted and *does the work again by hand*. Resetting straight to `marker.head`
/// then throws that reconstruction away irreversibly — the heal becomes the corruption.
///
/// So the present is snapshotted FIRST, into the same shadow repo (never the user's
/// `.git`), anchored under a `umadev-saved-*` ref so the reset cannot orphan it, and NAMED
/// in the note the user is shown — reachable from `umadev history` / `rollback <id>`.
/// If the snapshot cannot be taken, the reset does not happen: losing the heal is always
/// better than losing the user's code.
#[must_use]
pub fn recover_abandoned_temp_rewind(project_root: &Path) -> Option<String> {
    let path = project_root.join(TEMP_REWIND_MARKER_REL);
    let body = std::fs::read_to_string(&path).ok()?;
    let marker: TempRewindMarker = serde_json::from_str(&body).ok()?;

    let alive = if marker.pid == 0 {
        None
    } else {
        crate::run_lock::pid_is_alive(marker.pid)
    };
    if !marker_is_abandoned(
        &marker,
        now_secs(),
        &crate::run_lock::boot_id(),
        &crate::run_lock::hostname(),
        std::process::id(),
        alive,
    ) {
        return None;
    }

    // Only ever reset to a commit WE wrote and can still name (the same validation
    // `restore_checkpoint` applies) — a corrupt marker can never rewind the tree to an
    // arbitrary ref.
    //
    // Both refusals below leave the tree POSSIBLY still in the past, and a silent return
    // here is the same failure as a silent failed reset: the user sees reverted source,
    // no explanation, and a `rollback` that moves the wrong way. So neither path is
    // silent — each warns and hands the caller a note to SURFACE.
    let head_is_known = has_checkpoints(project_root)
        && id_is_known_checkpoint(&marker.head, &list_checkpoints_limited(project_root, 1000));
    if !head_is_known {
        tracing::warn!(
            head = %marker.head,
            to = %marker.to,
            marker = %path.display(),
            "an abandoned temporary rewind names a head snapshot this workspace cannot \
             identify (no checkpoint history, or an unknown id) — refusing to reset the tree \
             to an unvalidated ref; the marker is kept"
        );
        // We cannot say where this tree stands: a rewind was taken, and the commit that
        // held the present is not one we can name. UNKNOWN is not "fine" — it is exactly
        // the state in which we must stop writing. Raise the halt (the same one a failed
        // restore raises) so the run STOPS instead of layering new code onto a tree that
        // may well be in the past, and so no further rewind can compound it.
        //
        // UNRECOVERABLE. This branch reaches the SAME refusal on every later start — the
        // head cannot become nameable again — so the marker re-raises the halt forever and
        // "restart UmaDev and it will put the tree back" would be a lie. The user needs a
        // real way out (`umadev doctor --fix` → `clear_temp_rewind_state`), and the note has
        // to name it. A stop nobody can lift is a lockout, not a safeguard.
        mark_workspace_in_past(project_root, InPastReason::Unrecoverable);
        return Some(umadev_i18n::tlf(
            "checkpoint.temp_rewind_unrecoverable",
            &[&marker.head, &path.display().to_string()],
        ));
    }
    // THE PRESENT IS NOT DISPOSABLE. Whatever is in the work-tree right now — the
    // rewound source PLUS everything the user has written since, by hand, on top of it —
    // is about to be `reset --hard`ed away. Snapshot it first, into our own shadow repo,
    // and ANCHOR it (the reset below moves the shadow branch back to `head`, which would
    // otherwise orphan this commit and hide it from `list_checkpoints`' `--all` scan).
    let rescue = create_checkpoint(project_root, &heal_rescue_label());
    let Some(rescue) = rescue else {
        // We cannot promise the user's current files back, so we do not touch them.
        // The tree stays in the past — visibly, loudly, with the manual way out — which
        // is strictly better than a heal that silently eats the work they redid by hand.
        let manual = format!(
            "git --git-dir={} --work-tree={} reset --hard {}",
            git_dir(project_root).display(),
            project_root.display(),
            marker.head
        );
        tracing::warn!(
            head = %marker.head,
            to = %marker.to,
            marker = %path.display(),
            "an abandoned temporary rewind was found, but the CURRENT work-tree could not be \
             snapshotted first — standing down rather than resetting over files we could not \
             save; the marker is kept for the next start"
        );
        // RETRYABLE: the head IS a checkpoint we can name (we just validated it) — only the
        // snapshot of the present failed. The next start retries both, so a restart is honest
        // advice here.
        mark_workspace_in_past(project_root, InPastReason::Retryable);
        return Some(umadev_i18n::tlf(
            "checkpoint.temp_rewind_snapshot_failed",
            &[&marker.head, &path.display().to_string(), &manual],
        ));
    };
    let _ = git(
        project_root,
        &["branch", "-f", &format!("umadev-saved-{rescue}"), "HEAD"],
    );
    // Did the user actually WORK on the tree while it was in the past? That is the
    // dangerous case this snapshot exists for, and it is cheap to detect: the rescue
    // commit differs from the commit the rewind left the tree at.
    let edited = tree_edited_since_rewind(project_root, &marker.to, &rescue);

    if !TempRewind::reset_hard(project_root, &marker.head) {
        // The tree is STILL in the past and we could not put it back. Silence here is
        // the worst outcome of all: the user sees mangled source, no explanation, and a
        // `rollback` that would move them further backwards. Keep the marker (the next
        // start retries) and TELL them — with the commit that holds their work and the
        // one command that recovers it by hand.
        let manual = format!(
            "git --git-dir={} --work-tree={} reset --hard {}",
            git_dir(project_root).display(),
            project_root.display(),
            marker.head
        );
        tracing::warn!(
            head = %marker.head,
            to = %marker.to,
            marker = %path.display(),
            "an abandoned temporary rewind could NOT be restored; the workspace is still at an \
             earlier checkpoint — the marker is kept for the next start"
        );
        // RETRYABLE: the head is known and the rescue snapshot was taken; only the reset
        // failed. The marker is kept and the next start retries it.
        mark_workspace_in_past(project_root, InPastReason::Retryable);
        return Some(umadev_i18n::tlf(
            "checkpoint.temp_rewind_recovery_failed",
            &[&marker.head, &path.display().to_string(), &manual],
        ));
    }
    let _ = std::fs::remove_file(&path);
    clear_workspace_in_past(project_root);
    tracing::warn!(
        head = %marker.head,
        to = %marker.to,
        pid = marker.pid,
        rescue = %rescue,
        edited,
        "restored the workspace from an abandoned temporary rewind"
    );
    // Both notes NAME the rescue snapshot AND spell out the command that brings it back —
    // `umadev rollback <rescue>`, which `cmd_rollback` resolves against this shadow repo
    // (the third `{}`; `umadev history` lists it). A note that says "your work is safe"
    // and then names a verb that answers "no snapshots available" is worse than saying
    // nothing: it is the ONE sentence the whole heal exists to be able to say.
    //
    // The `edited` variant is the LOUD one: the user was working on the tree while it sat
    // in the past, so their edits were just moved out from under them into that snapshot —
    // they must be told plainly, not reassured.
    let key = if edited {
        "checkpoint.temp_rewind_recovered_with_edits"
    } else {
        "checkpoint.temp_rewind_recovered"
    };
    Some(umadev_i18n::tlf(key, &[&marker.head, &rescue, &rescue]))
}

/// Did the work-tree change while it sat in the past? `to` is the commit the rewind left
/// it at; `rescue` is the snapshot of the tree as the heal found it. Any difference is an
/// edit made in the rewound window (or after the crash) — the case where a naive
/// `reset --hard` would destroy real work.
///
/// Fail-open toward LOUD: a `git diff` we cannot run reads as "edited", because the only
/// thing that rides on it is how emphatic the note is. Never a decision to reset or not.
fn tree_edited_since_rewind(project_root: &Path, to: &str, rescue: &str) -> bool {
    // `--quiet` exits 0 = identical, 1 = differences, >1 = git itself failed.
    git(project_root, &["diff", "--quiet", to, rescue]).is_none_or(|o| !o.status.success())
}

/// Begin a scoped temporary rewind of the workspace to checkpoint `to` (see
/// [`TempRewind`]). The returned guard restores head on `restore()` / `Drop`.
///
/// FAIL-OPEN AT EVERY EDGE — `None` (and a completely untouched workspace) when:
/// the workspace is ALREADY stranded in the past (see below), `git` is unavailable, the
/// shadow repo has no checkpoints, `to` is not a known checkpoint id (the same validation
/// [`restore_checkpoint`] applies, so a stray handle can never `reset --hard` the tree to
/// an unintended commit), the head snapshot cannot be taken (without it the rewind would
/// be irreversible — so we refuse to start), or the reset itself fails. A caller that gets
/// `None` must simply skip whatever it wanted to learn from the past; it never blocks.
///
/// # A tree in the past is never rewound again
///
/// A step declares its evidence as a LIST, and each `TestFailsThenPasses` item takes its
/// own rewind — while the halt that reads [`workspace_is_in_past`] is checked once per
/// STEP. So a first rewind whose restore FAILED (tree stranded in the past, halt raised,
/// crash marker naming the TRUE present) could be followed, inside the very same step, by
/// a second rewind that:
///
/// 1. OVERWRITES the crash marker with a head that snapshots the **in-past** tree — so the
///    only record of the user's real present is gone, and no later start can heal it; and
/// 2. on a successful restore-to-that-head, CLEARS both the marker and the halt — leaving
///    the tree in the past with every alarm silenced.
///
/// The tree is not ours to move at that point, and the marker is not ours to rewrite. So
/// this refuses outright: the halt stands, the marker still names the true present, and
/// the next start's heal puts the tree back.
#[must_use]
pub fn begin_temp_rewind(project_root: &Path, to: &str) -> Option<TempRewind> {
    if workspace_is_in_past(project_root) {
        tracing::warn!(
            root = %project_root.display(),
            to = %to,
            "refusing a temporary rewind: this workspace is already stranded at an earlier \
             checkpoint by a rewind we could not undo — a second rewind would overwrite the \
             crash marker that names the user's real present"
        );
        return None;
    }
    if !has_checkpoints(project_root) {
        return None;
    }
    let known = list_checkpoints_limited(project_root, 1000);
    // RESOLVE, don't merely validate — and rewind to the RESOLVED commit below. The same
    // reason `restore_checkpoint` does: a bool guard followed by a reset to the caller's raw
    // string lets any revision EXPRESSION built on a known checkpoint (`<known>^`, `<known>~1`)
    // through the gate and then moves the work-tree to a commit that is not a checkpoint at all.
    let to = resolve_checkpoint_id(to, &known)?;
    // 1. Snapshot the PRESENT first. No snapshot ⇒ no way back ⇒ we do not rewind at
    //    all. This is the whole safety argument: we only ever reset to a commit we
    //    ourselves just wrote, and we always hold the id of the state we left.
    let head = create_checkpoint(project_root, TEMP_REWIND_HEAD_LABEL)?;
    // 2. Anchor it under a ref so it can never be orphaned/GC'd (mirrors
    //    `restore_checkpoint`), and stays listable/rewindable for the user.
    let _ = git(
        project_root,
        &["branch", "-f", &format!("umadev-saved-{head}"), "HEAD"],
    );
    // 3. CRASH MARKER, before the reset — the ONE artifact that survives a SIGKILL /
    //    OOM / a closed terminal, where no `Drop` runs. It names the commit holding the
    //    present, so a later start can put the user's source tree back instead of
    //    leaving it silently reverted to an earlier step. Written FIRST: a marker with
    //    no rewind is harmless (the recovery validates and no-ops); a rewind with no
    //    marker is the corruption we are closing.
    write_temp_rewind_marker(project_root, &head, &to);
    // 4. Now go back. A failed reset leaves the tree where it was (git is atomic
    //    enough here) — clear the marker we just wrote and let the caller skip.
    if !TempRewind::reset_hard(project_root, &to) {
        clear_temp_rewind_marker(project_root);
        return None;
    }
    Some(TempRewind {
        root: project_root.to_path_buf(),
        head,
        restored: false,
    })
}

// =====================================================================
// Run-scoped working-tree diff — "what did THIS run change?"
// =====================================================================

/// One file the working tree changed relative to a checkpoint.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChangedFile {
    /// Workspace-relative, `/`-separated path.
    pub path: String,
    /// `true` when the file did NOT exist at the baseline — a genuinely NEW file, as
    /// opposed to an edit or deletion of one that was already there. The distinction
    /// lets a scope check treat "a new file nobody planned" (a new surface)
    /// differently from a change to an existing surface. The path is retained for a
    /// deletion so rollback, scope checks, and PR staging do not lose that change.
    pub added: bool,
}

/// What the run diff can tell us — and, when it can tell us nothing, WHY.
///
/// A check that reads "no files changed" and a check that reads "we could not look" must
/// behave differently, and a floor that goes dark must be able to SAY so. Collapsing all
/// three cases into an empty `Vec` is what let a 2500-file monorepo silently receive zero
/// scope enforcement, with no warning and nothing to learn from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunDiff {
    /// The files that changed since the baseline. An EMPTY vec here is a positive fact:
    /// the run really did not touch anything.
    Changed(Vec<ChangedFile>),
    /// We could not look: `git` is unavailable, there is no run baseline, or the diff
    /// could not be read. No view ⇒ no finding, ever.
    Unavailable,
    /// The diff blew [`MAX_CHANGED_FILES`]. A PARTIAL view is worse than none (it would
    /// misread every unlisted file as "unchanged"), so we discard it — and carry the
    /// count we got to, so the caller can tell the user the floor stood down and why.
    TooLarge(usize),
}

/// Every file the working tree changed since the current **run baseline** — the
/// authoritative "what did this run touch?" set, taken from the same shadow repo the
/// rollback/rewind machinery already owns (so it sees exactly the files a rollback
/// would revert, including ones the project `.gitignore` hides).
///
/// See [`RunDiff`]: the caller is told the DIFFERENCE between "nothing changed", "we
/// could not look", and "too large to analyze".
#[must_use]
pub fn run_diff_since_baseline(project_root: &Path) -> RunDiff {
    let Some(baseline) = run_baseline(project_root) else {
        return RunDiff::Unavailable;
    };
    run_diff_since(project_root, &baseline.id)
}

/// [`run_diff_since_baseline`], flattened to a plain list — `Some(files)` for a real answer,
/// **`None` for every no-view case** ("we could not look" / "too large to analyze").
///
/// The flattening is the whole hazard here, so it is not hidden: an empty `Vec` for a diff
/// we never saw reads as the positive fact "the run touched nothing", and a floor that
/// believes it enforces NOTHING while reporting itself green. Returning `None` makes the
/// no-view case impossible to mistake for the no-changes case at the call site. A caller
/// that must TELL the user why the view went dark still wants
/// [`run_diff_since_baseline`] — it carries the reason.
#[must_use]
pub fn changed_since_run_baseline(project_root: &Path) -> Option<Vec<ChangedFile>> {
    match run_diff_since_baseline(project_root) {
        RunDiff::Changed(files) => Some(files),
        RunDiff::Unavailable | RunDiff::TooLarge(_) => None,
    }
}

/// Cap on the run-diff. Past this the diff is discarded entirely
/// ([`RunDiff::TooLarge`]) — never truncated into a half-truth.
///
/// `pub` so the scope floor can NAME the cap in the advisory it raises when it stands
/// down: a check that goes dark has to be able to tell the user exactly why.
pub const MAX_CHANGED_FILES: usize = 2000;

/// [`changed_since_run_baseline`] against an explicit checkpoint id — `Some(files)` for a
/// real answer, `None` when there was no view (see [`run_diff_since`] for the variant that
/// also carries WHY).
#[must_use]
pub fn changed_since(project_root: &Path, baseline_id: &str) -> Option<Vec<ChangedFile>> {
    match run_diff_since(project_root, baseline_id) {
        RunDiff::Changed(files) => Some(files),
        RunDiff::Unavailable | RunDiff::TooLarge(_) => None,
    }
}

/// [`run_diff_since_baseline`] against an explicit checkpoint id.
///
/// Stages the work-tree into the shadow index first (the same `--force` staging every
/// checkpoint uses, so untracked-but-real files are visible), then reads
/// `diff --cached --name-status` against the baseline commit. The index is a scratch
/// surface for the shadow repo — every checkpoint operation re-stages from scratch —
/// so this is a read-only question as far as the user's workspace is concerned.
#[must_use]
pub fn run_diff_since(project_root: &Path, baseline_id: &str) -> RunDiff {
    if !has_checkpoints(project_root) {
        return RunDiff::Unavailable;
    }
    if stage_all_including_ignored(project_root).is_none() {
        return RunDiff::Unavailable;
    }
    let Some(out) = git(
        project_root,
        &[
            "diff",
            "--cached",
            "--name-status",
            "--no-renames",
            baseline_id,
        ],
    ) else {
        return RunDiff::Unavailable;
    };
    if !out.status.success() {
        return RunDiff::Unavailable;
    }
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next().map(str::trim) else {
            continue;
        };
        let Some(path) = parts.next().map(str::trim).filter(|p| !p.is_empty()) else {
            continue;
        };
        let added = match status.chars().next() {
            Some('A') => true,
            Some('M' | 'T' | 'D') => false,
            _ => continue,
        };
        files.push(ChangedFile {
            path: path.replace('\\', "/"),
            added,
        });
        if files.len() > MAX_CHANGED_FILES {
            // An enormous diff → no view, never a partial one. But SAY which it is: a
            // floor that stands down silently teaches the user nothing.
            tracing::warn!(
                changed = files.len(),
                cap = MAX_CHANGED_FILES,
                "the run diff exceeded the analysis cap; scope checks stand down for this run"
            );
            return RunDiff::TooLarge(files.len());
        }
    }
    RunDiff::Changed(files)
}

/// The contents of `rel` AS OF checkpoint `id`, or `None` when the file did not exist
/// there / is binary / `git` is unavailable (fail-open). Lets a caller compare a
/// manifest's before-and-after without leaving the present.
#[must_use]
pub fn file_at(project_root: &Path, id: &str, rel: &str) -> Option<String> {
    let spec = format!("{id}:{rel}");
    let out = git(project_root, &["show", &spec])?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn create_list_restore_roundtrip() {
        if !git_available() {
            return; // fail-open environment without git — nothing to assert
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        let c1 = create_checkpoint(root, "phase: frontend").expect("checkpoint 1");

        // Mutate + add a file, then checkpoint again.
        std::fs::write(root.join("a.txt"), "v2").unwrap();
        std::fs::write(root.join("b.txt"), "new").unwrap();
        let _c2 = create_checkpoint(root, "phase: backend").expect("checkpoint 2");

        let list = list_checkpoints(root);
        assert_eq!(list.len(), 2, "two checkpoints, newest first");
        assert_eq!(list[0].label, "phase: backend");

        // Rewind to c1: a.txt back to v1, b.txt removed.
        restore_checkpoint(root, &c1).expect("restore");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1");
        assert!(!root.join("b.txt").exists(), "file added after c1 is gone");

        // The rewind must NOT lose history: the forward checkpoint ("phase:
        // backend") and the auto pre-rewind snapshot are still listed (preserved
        // under a umadev-saved-* ref), so the rewind itself stays undoable.
        let after = list_checkpoints(root);
        assert!(
            after.iter().any(|c| c.label == "phase: backend"),
            "forward checkpoint survives the rewind"
        );
        assert!(
            after.iter().any(|c| c.label.contains("回滚到")),
            "pre-rewind snapshot is reachable"
        );
    }

    #[test]
    fn phase_checkpoint_baselines_then_skips_unchanged() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        // First phase boundary → baseline checkpoint is taken even though there's
        // no prior HEAD.
        let c1 = create_phase_checkpoint(root, "phase: research");
        assert!(c1.is_some(), "first phase checkpoint is the baseline");
        // A second phase boundary with NO file change → no new checkpoint (the
        // headless run starts several phases back-to-back before any file lands;
        // we must not clutter the rewind list with identical empty commits).
        let c2 = create_phase_checkpoint(root, "phase: docs");
        assert!(c2.is_none(), "unchanged tree yields no new checkpoint");
        assert_eq!(list_checkpoints(root).len(), 1, "still one checkpoint");
        // Now the base writes a file → the next phase boundary DOES checkpoint.
        std::fs::write(root.join("b.txt"), "new").unwrap();
        let c3 = create_phase_checkpoint(root, "phase: frontend");
        assert!(c3.is_some(), "a changed tree produces a new checkpoint");
        assert_eq!(list_checkpoints(root).len(), 2);
    }

    #[test]
    fn restore_without_checkpoints_errors() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        assert!(restore_checkpoint(tmp.path(), "deadbeef").is_err());
    }

    #[test]
    fn has_checkpoints_false_on_empty() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        assert!(!has_checkpoints(tmp.path()));
    }

    #[test]
    fn restore_rejects_unknown_id_without_resetting() {
        // P1-3: an id that is NOT a known checkpoint must be refused BEFORE any
        // `git reset --hard`, so a stray ref (HEAD~999, a random SHA) can never
        // rewind the work-tree to an unintended commit and discard files.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        let _c1 = create_checkpoint(root, "phase: frontend").expect("checkpoint");
        // Mutate the live tree AFTER the checkpoint — this is the work an out-of-
        // range reset would have destroyed.
        std::fs::write(root.join("a.txt"), "v2-live-work").unwrap();

        // An arbitrary ref that git WOULD accept for `reset --hard` but is not a
        // known checkpoint → refused.
        for bad in ["HEAD~999", "deadbeef", "main", "refs/heads/whatever", ""] {
            let r = restore_checkpoint(root, bad);
            assert!(r.is_err(), "unknown id {bad:?} must be refused");
        }
        // The live work is intact — no reset happened.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "v2-live-work",
            "a refused restore must not touch the work-tree"
        );
    }

    #[test]
    fn rollback_run_rewinds_whole_run_to_baseline() {
        // Wave 6: `rollback` = real-git undo of the entire run, over the shadow
        // repo (the user's own `.git` is never touched).
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        // Pre-run state.
        std::fs::write(root.join("src.rs"), "original").unwrap();
        // Run start → baseline snapshot.
        let baseline = create_run_baseline(root, "demo").expect("baseline");
        assert!(run_baseline(root).is_some(), "baseline is findable");
        // The run writes a couple of phases' worth of edits.
        std::fs::write(root.join("src.rs"), "rewritten by the run").unwrap();
        std::fs::write(root.join("new_file.rs"), "added by the run").unwrap();
        let _ = create_phase_checkpoint(root, "phase: frontend");
        std::fs::write(root.join("another.rs"), "more run output").unwrap();
        let _ = create_phase_checkpoint(root, "phase: backend");
        // Roll the WHOLE run back.
        let rolled = rollback_run(root).expect("rollback to baseline");
        assert!(
            rolled.id == baseline
                || baseline.starts_with(&rolled.id)
                || rolled.id.starts_with(&baseline)
        );
        // Pre-run file restored, run-added files gone.
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "original"
        );
        assert!(!root.join("new_file.rs").exists(), "run-added file removed");
        assert!(!root.join("another.rs").exists(), "run-added file removed");
        // The rollback is itself undoable: forward checkpoints survive under a
        // saved ref (so list still shows the run's work).
        let after = list_checkpoints(root);
        assert!(
            after.iter().any(|c| c.label == "phase: backend"),
            "forward run checkpoints survive the rollback (it stays reversible)"
        );
    }

    #[test]
    fn rollback_run_without_baseline_errors_and_changes_nothing() {
        // Fail-open: no baseline → an actionable error, never a destructive op.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "untouched").unwrap();
        let r = rollback_run(root);
        assert!(r.is_err(), "no baseline → Err, not a silent reset");
        // The working tree was not touched.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "untouched"
        );
    }

    #[test]
    fn run_baseline_picks_newest_and_ignores_phase_checkpoints() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("f.txt"), "1").unwrap();
        let _ = create_run_baseline(root, "first-run");
        std::fs::write(root.join("f.txt"), "2").unwrap();
        let _ = create_phase_checkpoint(root, "phase: spec");
        std::fs::write(root.join("f.txt"), "3").unwrap();
        let second = create_run_baseline(root, "second-run").expect("second baseline");
        // The newest baseline is the rollback target, not the phase checkpoint.
        let target = run_baseline(root).expect("a baseline exists");
        assert!(target.label.starts_with(RUN_BASELINE_PREFIX));
        assert!(target.label.contains("second-run"));
        assert!(
            second.starts_with(&target.id) || target.id.starts_with(&second) || second == target.id
        );
    }

    #[test]
    fn ensure_run_baseline_is_fresh_per_run_and_deduped_within_a_run() {
        // P1/P2: a 2nd run must take its OWN baseline (not reuse the 1st run's), so
        // `rollback` reverts ONLY the current run's changes — while staying
        // idempotent within one run (a re-entry block at the same start does not
        // stack a second baseline).
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "pre-run-1").unwrap();

        // Run 1 start → the first baseline.
        let b1 = ensure_run_baseline(root, "run1").expect("run1 baseline");
        // A SECOND setup call at the same run's start (no phase checkpoint yet) must
        // NOT stack a new baseline — it reuses b1 (still sitting at the run start).
        let b1_again = ensure_run_baseline(root, "run1").expect("dedup within run");
        assert!(
            b1_again == b1 || b1.starts_with(&b1_again) || b1_again.starts_with(&b1),
            "within one run the baseline is re-used, not re-taken"
        );

        // Run 1 writes + checkpoints a phase → shadow HEAD moves past the baseline.
        std::fs::write(root.join("src.rs"), "run-1-output").unwrap();
        std::fs::write(root.join("run1_file.rs"), "added by run 1").unwrap();
        let _ = create_phase_checkpoint(root, "phase: frontend");

        // Run 2 start → a FRESH baseline (not run 1's), capturing run 1's output.
        let b2 = ensure_run_baseline(root, "run2").expect("run2 baseline");
        assert!(
            !(b2 == b1 || b1.starts_with(&b2) || b2.starts_with(&b1)),
            "a 2nd run takes its OWN baseline, not run 1's"
        );
        assert!(
            run_baseline(root).unwrap().label.contains("run2"),
            "the newest run baseline is run 2's"
        );

        // Run 2 writes.
        std::fs::write(root.join("src.rs"), "run-2-output").unwrap();
        std::fs::write(root.join("run2_file.rs"), "added by run 2").unwrap();
        let _ = create_phase_checkpoint(root, "phase: backend");

        // Rollback reverts ONLY run 2 (back to run 1's output), never run 1.
        let _ = rollback_run(root).expect("rollback run 2");
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "run-1-output",
            "rollback reverts run 2 to run 1's output, not to before run 1"
        );
        assert!(
            !root.join("run2_file.rs").exists(),
            "run 2's added file is removed by the rollback"
        );
        assert!(
            root.join("run1_file.rs").exists(),
            "run 1's file SURVIVES — only the current run was reverted"
        );
    }

    #[test]
    fn checkpoint_captures_and_restores_gitignored_files() {
        // P1/P2: the shadow checkpoint force-adds the project's .gitignore'd product
        // files (an AI-written `.env.local`) so a rollback RESTORES them — but still
        // excludes heavy dirs so the shadow commit can't balloon.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        // A project .gitignore that hides .env.local AND node_modules.
        std::fs::write(root.join(".gitignore"), "node_modules/\n.env.local\n").unwrap();
        std::fs::write(root.join("app.js"), "v1").unwrap();
        std::fs::write(root.join(".env.local"), "SECRET=original").unwrap();
        std::fs::create_dir_all(root.join("node_modules").join("pkg")).unwrap();
        std::fs::write(root.join("node_modules").join("pkg").join("i.js"), "heavy").unwrap();

        let c1 = create_checkpoint(root, "baseline").expect("checkpoint");
        // The ignored .env.local WAS captured; the heavy node_modules was NOT.
        let tracked = git(root, &["ls-files"])
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        assert!(
            tracked.lines().any(|l| l == ".env.local"),
            "the .gitignore'd .env.local is captured in the checkpoint"
        );
        assert!(
            !tracked.lines().any(|l| l.contains("node_modules")),
            "the heavy node_modules dir stays out of the checkpoint"
        );

        // The base overwrites the ignored file, then we roll back to c1.
        std::fs::write(root.join(".env.local"), "SECRET=rewritten-by-ai").unwrap();
        let _c2 = create_checkpoint(root, "after edit");
        restore_checkpoint(root, &c1).expect("restore");
        assert_eq!(
            std::fs::read_to_string(root.join(".env.local")).unwrap(),
            "SECRET=original",
            "rollback restored the .gitignore'd file the base had overwritten"
        );
    }

    #[test]
    fn id_is_known_checkpoint_matches_prefixes_both_directions() {
        let list = vec![Checkpoint {
            id: "abc1234".to_string(),
            label: "x".to_string(),
            when: "t".to_string(),
        }];
        // Exact, listed-id-as-prefix-of-input (full SHA), input-as-prefix-of-id.
        assert_eq!(
            resolve_checkpoint_id("abc1234", &list).as_deref(),
            Some("abc1234")
        );
        assert_eq!(
            resolve_checkpoint_id("abc1234def567", &list).as_deref(), // fuller SHA
            Some("abc1234")
        );
        assert_eq!(
            resolve_checkpoint_id("abc", &list).as_deref(),
            Some("abc1234")
        ); // abbreviation
           // Unrelated / empty → rejected.
        assert!(!id_is_known_checkpoint("zzz9999", &list));
        assert!(!id_is_known_checkpoint("", &list));
        assert!(!id_is_known_checkpoint("HEAD~5", &list));
    }

    #[test]
    fn the_id_guard_rejects_git_revision_syntax_built_on_a_known_checkpoint() {
        // THE RUBBER STAMP. The guard used to be `id.starts_with(cid)` and the caller then
        // reset to the RAW id — so every revision EXPRESSION built on a real checkpoint
        // sailed through, because it starts with a real checkpoint. Proven against the real
        // binary: `umadev rollback 3b599bd^` and `umadev rollback 3b599bd~1` both reset the
        // work-tree, to commits that are not checkpoints at all — the exact thing the
        // docstring swore could never happen.
        let list = vec![Checkpoint {
            id: "3b599bd".to_string(),
            label: "x".to_string(),
            when: "t".to_string(),
        }];
        assert_eq!(
            resolve_checkpoint_id("3b599bd", &list).as_deref(),
            Some("3b599bd")
        );
        for rev in [
            "3b599bd^",     // the parent — reported
            "3b599bd~1",    // the parent — reported
            "3b599bd~999",  // deep history
            "3b599bd^{}",   // peel
            "3b599bd@{1}",  // reflog
            "3b599bd:/msg", // commit-message search
            "3b599bd..HEAD",
            "HEAD~3",
            "main",
            "refs/heads/main",
        ] {
            assert!(
                resolve_checkpoint_id(rev, &list).is_none(),
                "revision syntax is not a checkpoint id: {rev:?}"
            );
        }
    }

    #[test]
    fn an_ambiguous_abbreviation_is_refused_not_guessed() {
        // Two checkpoints share a prefix. Picking one would `reset --hard` the work-tree to
        // a commit the user did not name, discarding their files. Refuse.
        let list = vec![
            Checkpoint {
                id: "abc1111".to_string(),
                label: "x".to_string(),
                when: "t".to_string(),
            },
            Checkpoint {
                id: "abc2222".to_string(),
                label: "y".to_string(),
                when: "t".to_string(),
            },
        ];
        assert!(
            resolve_checkpoint_id("abc", &list).is_none(),
            "ambiguous → refuse"
        );
        // …and an unambiguous one still resolves.
        assert_eq!(
            resolve_checkpoint_id("abc1111", &list).as_deref(),
            Some("abc1111")
        );
    }

    #[test]
    fn rollback_to_a_revision_expression_does_not_move_the_work_tree() {
        // The same defect, end to end over a real shadow repo: `<known>^` must not reset.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        let c1 = create_checkpoint(root, "phase: one").expect("checkpoint");
        std::fs::write(root.join("a.txt"), "v2").unwrap();
        let _c2 = create_checkpoint(root, "phase: two").expect("checkpoint");
        std::fs::write(root.join("a.txt"), "v3-uncommitted").unwrap();

        for rev in [format!("{c1}^"), format!("{c1}~1")] {
            assert!(
                restore_checkpoint(root, &rev).is_err(),
                "a revision expression must be refused: {rev}"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("a.txt")).unwrap(),
                "v3-uncommitted",
                "the work-tree must not have moved: {rev}"
            );
        }
        // …and the legitimate handle still works — the guard is not a wall.
        restore_checkpoint(root, &c1).expect("a real checkpoint id still restores");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1");
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_pre_snapshot_aborts_the_restore_instead_of_resetting_anyway() {
        // LOW-5. `cmd_rollback` tells the user "we snapshotted first, so this is itself
        // undoable". The pre-snapshot's result was DISCARDED (`let _ = create_checkpoint`)
        // and the reset ran regardless — so when the snapshot failed, that sentence was a
        // lie and the `reset --hard` destroyed uncommitted work with nothing to restore it
        // from. `begin_temp_rewind` (`?`) and `recover_abandoned_temp_rewind` (stands down)
        // both treat exactly this as load-bearing; this one did not.
        use std::os::unix::fs::PermissionsExt;
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        let c1 = create_checkpoint(root, "phase: one").expect("checkpoint");
        std::fs::write(root.join("a.txt"), "precious-uncommitted-work").unwrap();

        // Make the shadow repo READ-ONLY: `git log` / `rev-parse` still answer (so the id
        // validates), but the pre-restore snapshot cannot be written.
        let gd = git_dir(root);
        let orig = std::fs::metadata(&gd).unwrap().permissions();
        std::fs::set_permissions(&gd, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = restore_checkpoint(root, &c1);
        let after = std::fs::read_to_string(root.join("a.txt")).unwrap();
        std::fs::set_permissions(&gd, orig).unwrap();

        // A root-run test can still write into a 0555 dir; skip rather than assert a false
        // pass (the reset would then legitimately have succeeded).
        if result.is_ok() {
            return;
        }
        assert_eq!(
            after, "precious-uncommitted-work",
            "no snapshot ⇒ no reset: the user's uncommitted work must still be on disk"
        );
        assert!(
            !result.unwrap_err().is_empty(),
            "and the refusal has to SAY so — a silent no-op is the same bug"
        );
    }

    #[test]
    fn the_recovery_surface_speaks_the_users_language() {
        // LOW-6. These labels + errors surface in `umadev history` / `umadev rollback` — the
        // exact commands the (localized) heal note tells the user to run. They were hardcoded
        // zh, so an English user was pointed at a Chinese row.
        //
        // Locale-independent invariant (the test cannot assume which locale CI runs in): each
        // one resolves to a non-empty string that is NOT the raw i18n key — i.e. it went
        // through the catalog, in whatever language this machine speaks.
        for s in [
            heal_rescue_label(),
            umadev_i18n::tl("checkpoint.none_to_restore").to_string(),
            umadev_i18n::tl("checkpoint.git_unavailable").to_string(),
            umadev_i18n::tl("checkpoint.restore_snapshot_failed").to_string(),
            umadev_i18n::tlf("checkpoint.rollback_pre_label", &["abc1234"]),
            umadev_i18n::tlf("checkpoint.unknown_id", &["abc1234"]),
        ] {
            assert!(!s.is_empty(), "the catalog must carry it");
            assert!(!s.starts_with("checkpoint."), "unresolved i18n key: {s}");
        }
        // The two that interpolate must actually place the id.
        assert!(
            umadev_i18n::tlf("checkpoint.rollback_pre_label", &["abc1234"]).contains("abc1234")
        );
        assert!(umadev_i18n::tlf("checkpoint.unknown_id", &["abc1234"]).contains("abc1234"));
        // And the rescue snapshot must stay VISIBLE in the user-facing list — it is the only
        // copy of what they wrote while the tree was in the past. Localizing it must not have
        // turned it into an internal (hidden) label.
        assert!(
            !is_internal_label(&heal_rescue_label()),
            "the rescue snapshot must remain listable + rollback-able"
        );
        // The MIRROR: the machine-matched labels must NOT be localized, or a label written
        // under one locale would stop being recognised under another.
        assert!(is_internal_label(TEMP_REWIND_HEAD_LABEL));
        assert!(is_internal_label(&format!("{RED_GREEN_PRE_PREFIX}step 1")));
        assert!(RUN_BASELINE_PREFIX.is_ascii());
    }

    #[test]
    fn an_unrecoverable_marker_can_be_cleared_and_a_recoverable_one_is_left_alone() {
        // MED-4. The permanent, unclearable halt: a stale marker + a deleted
        // `.umadev/checkpoints.git` ⇒ the heal cannot name the recorded head ⇒ it refuses to
        // reset (correctly) and raises the halt — on EVERY process start, forever. No verb
        // cleared it and `umadev doctor` did not even look. The only escape was a hand-`rm`.
        if !git_available() {
            return;
        }
        // (a) RECOVERABLE — the head IS a known checkpoint. The automatic heal can still put
        //     this tree back, and the marker is the only map to the present, so the escape
        //     must NOT touch it. (This is the mirror image: an over-eager "fix" that deletes
        //     every marker strands the user in the past permanently.)
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        let head = create_checkpoint(root, "phase: one").expect("checkpoint");
        write_temp_rewind_marker(root, &head, &head);
        assert_eq!(
            clear_temp_rewind_state(root, false),
            TempRewindState::Recoverable { head: head.clone() },
            "a marker the heal can act on must be left in place"
        );
        assert!(
            root.join(TEMP_REWIND_MARKER_REL).exists(),
            "…and really still be on disk"
        );

        // (b) UNRECOVERABLE — the user deleted the shadow repo. Nothing can ever act on this
        //     marker, so it is pure lockout. It is cleared, and NO work-tree file is touched.
        std::fs::remove_dir_all(git_dir(root)).unwrap();
        assert_eq!(
            clear_temp_rewind_state(root, true),
            TempRewindState::ClearedUnrecoverable { head: head.clone() },
            "a dry run reports it…"
        );
        assert!(
            root.join(TEMP_REWIND_MARKER_REL).exists(),
            "…and changes nothing"
        );
        assert_eq!(
            clear_temp_rewind_state(root, false),
            TempRewindState::ClearedUnrecoverable { head }
        );
        assert!(
            !root.join(TEMP_REWIND_MARKER_REL).exists(),
            "the lockout marker is gone — `umadev run` can start again"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "v1",
            "clearing the halt must not touch a single file in the work-tree"
        );
        // No marker at all → nothing to report.
        assert_eq!(clear_temp_rewind_state(root, false), TempRewindState::Clean);
    }

    #[test]
    fn the_halt_note_tells_the_truth_about_which_branch_the_user_is_in() {
        // MED-4. One note served both branches and it said "Restart UmaDev and it will put
        // the tree back." That is TRUE of a retryable restore and FALSE of an unrecoverable
        // marker — where every restart reaches the identical refusal and the user is told to
        // do the one thing that cannot possibly work.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        assert!(
            workspace_in_past_note(root).is_none(),
            "a healthy tree says nothing"
        );

        mark_workspace_in_past(root, InPastReason::Retryable);
        let retry = workspace_in_past_note(root).expect("halted");
        assert_eq!(
            retry,
            umadev_i18n::tl("checkpoint.workspace_in_past_halt"),
            "the retryable branch keeps the restart advice — it is true there"
        );

        // Unrecoverable WINS over retryable for the same root: the escape must be shown
        // whenever any branch needs it.
        mark_workspace_in_past(root, InPastReason::Unrecoverable);
        assert_eq!(
            workspace_in_past_reason(root),
            Some(InPastReason::Unrecoverable)
        );
        let stuck = workspace_in_past_note(root).expect("halted");
        assert_ne!(stuck, retry, "the two branches must not read the same");
        assert!(
            stuck.contains("umadev doctor"),
            "the branch no restart can fix must name the verb that can: {stuck}"
        );
        clear_workspace_in_past(root);
        assert!(workspace_in_past_note(root).is_none());
    }

    // ── Temporary rewind (the red→green pre-state replay) ──────────────────────

    #[test]
    fn temp_rewind_shows_the_past_and_always_puts_the_present_back() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "before").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre checkpoint");

        // The step does its work: edits a file and adds a new one.
        std::fs::write(root.join("src.rs"), "after").unwrap();
        std::fs::write(root.join("new.rs"), "brand new").unwrap();

        {
            let rewind = begin_temp_rewind(root, &pre).expect("rewind");
            // INSIDE the window: the tree is exactly as it was before the step.
            assert_eq!(
                std::fs::read_to_string(root.join("src.rs")).unwrap(),
                "before"
            );
            assert!(
                !root.join("new.rs").exists(),
                "a file the step created must not exist at the pre-state"
            );
            assert!(rewind.restore(), "restore succeeds");
        }

        // OUTSIDE: the present is back, byte for byte — the edit AND the new file.
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "after"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("new.rs")).unwrap(),
            "brand new"
        );
    }

    #[test]
    fn a_dropped_temp_rewind_still_restores_the_present() {
        // THE SAFETY PROPERTY: a rewind must never survive its own scope. An early
        // return / a `?` / a panic inside the window still leaves the workspace at head.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "before").unwrap();
        let pre = create_checkpoint(root, "pre-step").unwrap();
        std::fs::write(root.join("src.rs"), "after").unwrap();

        {
            let _rewind = begin_temp_rewind(root, &pre).expect("rewind");
            assert_eq!(
                std::fs::read_to_string(root.join("src.rs")).unwrap(),
                "before"
            );
            // NO explicit restore — just drop the guard.
        }
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "after",
            "Drop must restore the present even when restore() was never called"
        );
    }

    #[test]
    fn temp_rewind_refuses_an_unknown_id_and_changes_nothing() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "live").unwrap();
        let _ = create_checkpoint(root, "c1");
        // A stray handle can never reset the work-tree to an unintended commit.
        assert!(begin_temp_rewind(root, "HEAD~9").is_none());
        assert!(begin_temp_rewind(root, "deadbeef").is_none());
        assert!(begin_temp_rewind(root, "").is_none());
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "live"
        );
    }

    #[test]
    fn temp_rewind_fails_open_without_a_shadow_repo() {
        // No checkpoints ever taken → no rewind (and nothing touched). The caller
        // degrades; it never blocks.
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(begin_temp_rewind(tmp.path(), "anything").is_none());
    }

    // ── Run-scoped diff (the scope-creep changed-file source) ──────────────────

    #[test]
    fn changed_since_run_baseline_separates_new_files_from_edits() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("existing.ts"), "v1").unwrap();
        create_run_baseline(root, "demo").expect("baseline");

        std::fs::write(root.join("existing.ts"), "v2").unwrap(); // edit
        std::fs::write(root.join("fresh.ts"), "new").unwrap(); // add

        // `Some(files)` = a real view. `None` would be "we could not look" — a distinction
        // the flattened-to-`Vec` form used to erase (a no-view read as "nothing changed").
        let changed = changed_since_run_baseline(root).expect("the diff is viewable");
        let edited = changed.iter().find(|c| c.path == "existing.ts").unwrap();
        let added = changed.iter().find(|c| c.path == "fresh.ts").unwrap();
        assert!(
            !edited.added,
            "an edit of an existing file is not an addition"
        );
        assert!(
            added.added,
            "a file that did not exist at the baseline is new"
        );

        // The baseline's own content is readable without leaving the present.
        assert_eq!(
            file_at(root, &run_baseline(root).unwrap().id, "existing.ts").as_deref(),
            Some("v1")
        );
    }

    #[test]
    fn changed_since_run_baseline_keeps_deleted_paths() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("obsolete.ts"), "export const old = true;\n").unwrap();
        create_run_baseline(root, "demo").expect("baseline");
        std::fs::remove_file(root.join("obsolete.ts")).unwrap();

        let changed = changed_since_run_baseline(root).expect("the diff is viewable");
        assert_eq!(
            changed,
            vec![ChangedFile {
                path: "obsolete.ts".to_string(),
                added: false,
            }],
            "a deletion is still part of the run diff and must reach rollback/PR/scope consumers"
        );
    }

    // ── BLOCKER: a killed process must not leave the source tree IN THE PAST ──

    #[test]
    fn a_temp_rewind_writes_a_crash_marker_and_clears_it_on_restore() {
        // The marker is the ONLY thing that survives a SIGKILL. It must exist for
        // exactly as long as the tree is in the past, and not one moment longer.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let marker = root.join(TEMP_REWIND_MARKER_REL);
        std::fs::write(root.join("src.rs"), "before").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre");
        std::fs::write(root.join("src.rs"), "after").unwrap();
        assert!(!marker.exists(), "no marker before any rewind");

        {
            let rewind = begin_temp_rewind(root, &pre).expect("rewind");
            // INSIDE the window the tree is in the past — and the marker says so, and
            // names the commit that holds the present.
            assert!(
                marker.exists(),
                "the crash marker exists for the whole window"
            );
            let m: TempRewindMarker =
                serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
            assert_eq!(m.pid, std::process::id());
            assert!(rewind.head_id().starts_with(&m.head) || m.head.starts_with(rewind.head_id()));
            assert!(m.started_at > 0);
            assert!(rewind.restore());
        }
        assert!(
            !marker.exists(),
            "a restored rewind leaves NO marker — there is nothing to recover"
        );
    }

    #[test]
    fn a_crashed_temp_rewind_is_recovered_on_the_next_start() {
        // THE BLOCKER. Simulate exactly what a SIGKILL leaves behind: the tree reset to
        // the pre-state, and a marker whose owner PID is dead. The next start must put
        // the user's files back and SAY so — with no help from any destructor.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "the user's real work").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre");
        std::fs::write(root.join("src.rs"), "the user's NEWER real work").unwrap();
        std::fs::write(root.join("new.rs"), "a file the step created").unwrap();

        // Take the rewind, then FORGET the guard (mem::forget = no Drop, exactly like a
        // process that was killed) and rewrite the marker with a dead PID.
        let rewind = begin_temp_rewind(root, &pre).expect("rewind");
        let head = rewind.head_id().to_string();
        std::mem::forget(rewind);
        let marker_path = root.join(TEMP_REWIND_MARKER_REL);
        let dead: TempRewindMarker = TempRewindMarker {
            head: head.clone(),
            to: pre.clone(),
            // A PID that cannot be alive.
            pid: 4_294_967_294,
            started_at: now_secs(),
            boot: crate::run_lock::boot_id(),
            host: crate::run_lock::hostname(),
        };
        std::fs::write(&marker_path, serde_json::to_string(&dead).unwrap()).unwrap();
        // The corruption, as the user would find it: their tracked files are in the past.
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "the user's real work"
        );
        assert!(!root.join("new.rs").exists());

        // The next start.
        let note = recover_abandoned_temp_rewind(root).expect("an abandoned rewind is recovered");
        assert!(!note.is_empty(), "the user is TOLD, not silently repaired");
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "the user's NEWER real work",
            "the present is back"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("new.rs")).unwrap(),
            "a file the step created"
        );
        assert!(!marker_path.exists(), "the marker is consumed");
        // Idempotent: a second start has nothing to do.
        assert!(recover_abandoned_temp_rewind(root).is_none());
    }

    #[test]
    fn a_recovery_note_reaches_a_surface_that_can_actually_speak() {
        // The heal runs where nobody is listening: at process start (its `eprintln!` is wiped
        // a moment later when the TUI takes the alternate screen) and inside
        // `RunLock::acquire_for_run` (whose `tracing::warn!` goes to a log FILE under the
        // TUI). So the single most important thing UmaDev can say about a user's own files —
        // "they were silently in the past; they are back" — was being said to nobody. The
        // note is now handed to the queue the transcript drains.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "the user's real work").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre");
        std::fs::write(root.join("src.rs"), "the user's NEWER real work").unwrap();

        let rewind = begin_temp_rewind(root, &pre).expect("rewind");
        let head = rewind.head_id().to_string();
        std::mem::forget(rewind); // killed: no Drop ran
        let dead = TempRewindMarker {
            head,
            to: pre,
            pid: 4_294_967_294, // cannot be alive
            started_at: now_secs(),
            boot: crate::run_lock::boot_id(),
            host: crate::run_lock::hostname(),
        };
        std::fs::write(
            root.join(TEMP_REWIND_MARKER_REL),
            serde_json::to_string(&dead).unwrap(),
        )
        .unwrap();

        // Clear anything a neighbouring test left, then take the run lock — the path a real
        // run takes, and the one whose note used to die in a log file.
        let _ = take_workspace_notices();
        let lock = crate::run_lock::RunLock::acquire_for_run(root).expect("lock");
        drop(lock);

        let notices = take_workspace_notices();
        assert!(
            notices.iter().any(|n| !n.is_empty()),
            "the workspace was put back — the user has to be able to SEE that"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "the user's NEWER real work",
            "and the tree really is back at the present"
        );
        // Drained, not copied: the surface that took it is the surface that shows it.
        assert!(take_workspace_notices().is_empty());
    }

    /// The rescue snapshot the heal just took, found by its label. `None` when the heal
    /// took none — which is itself the thing several tests below assert.
    fn rescue_checkpoint(root: &Path) -> Option<Checkpoint> {
        list_checkpoints_limited(root, 1000)
            .into_iter()
            .find(|c| c.label.starts_with(&heal_rescue_label()))
    }

    #[test]
    fn the_heal_snapshots_the_users_own_work_before_resetting_over_it() {
        // THE BLOCKER. The heal is armed on EVERY process start — including `umadev hook`
        // (fired on every base tool call) and `umadev ci` (fired from .git/hooks/pre-commit).
        // So the tree it `reset --hard`s is NOT the one the crashed run left behind: the user
        // saw their source reverted, REDID THE WORK BY HAND, and then typed `git commit`. The
        // old heal reset straight to `marker.head` and that reconstruction was gone —
        // irreversibly, with a note that cheerfully said "your code is no longer stuck in the
        // past" and never mentioned the edits it had just overwritten.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "the user's real work").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre");
        std::fs::write(root.join("src.rs"), "the user's NEWER real work").unwrap();

        // A run killed inside the rewound window: no Drop ran, the tree is in the past.
        let rewind = begin_temp_rewind(root, &pre).expect("rewind");
        let head = rewind.head_id().to_string();
        std::mem::forget(rewind);
        let dead = TempRewindMarker {
            head: head.clone(),
            to: pre.clone(),
            pid: 4_294_967_294, // cannot be alive
            started_at: now_secs(),
            boot: crate::run_lock::boot_id(),
            host: crate::run_lock::hostname(),
        };
        std::fs::write(
            root.join(TEMP_REWIND_MARKER_REL),
            serde_json::to_string(&dead).unwrap(),
        )
        .unwrap();

        // THE USER, seeing their source reverted, does the work again by hand — and adds
        // something new. This is what the heal is about to `reset --hard` over.
        std::fs::write(root.join("src.rs"), "REDONE BY HAND after the crash").unwrap();
        std::fs::write(root.join("rescued.rs"), "a file only the user wrote").unwrap();

        // The next start (a `git commit` → pre-commit → `umadev ci` → this).
        let note = recover_abandoned_temp_rewind(root).expect("an abandoned rewind is recovered");

        // The heal still does its job: the present is back.
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "the user's NEWER real work",
            "the present is back"
        );

        // (a) …and the work the user did in the meantime is RECOVERABLE, not destroyed.
        let rescue = rescue_checkpoint(root).expect("the heal snapshotted the tree it reset over");
        // (b) …and the note NAMES that snapshot, so `umadev history` / `rollback` can reach it.
        assert!(
            note.contains(&rescue.id),
            "the note must name the snapshot holding the user's edits: {note}"
        );
        assert!(
            note.contains(&head),
            "…and the snapshot holding the present: {note}"
        );
        // (c) The note is the LOUD variant: the tree had been edited while it sat in the
        // past, so the user is told plainly that their edits moved — not reassured.
        assert_eq!(
            note,
            umadev_i18n::tlf(
                "checkpoint.temp_rewind_recovered_with_edits",
                &[&head, &rescue.id, &rescue.id]
            ),
            "an edited tree gets the louder note"
        );
        // …and it SPELLS OUT the command that brings the work back. The note used to name
        // `umadev history` / `umadev rollback` without an id, and neither verb could even
        // see a shadow checkpoint — so the one sentence the heal exists to say was false.
        assert!(
            note.contains(&format!("umadev rollback {}", rescue.id)),
            "the note must name the exact command that restores the rescue snapshot: {note}"
        );
        // The rescue snapshot is a USER-facing rewind point (not filtered as machinery),
        // or `umadev history` could not show them the way back.
        assert!(
            list_checkpoints(root).iter().any(|c| c.id == rescue.id),
            "the rescue snapshot must be visible in the rewind list"
        );
        // And it really does hold their work: rewinding to it brings both files back.
        restore_checkpoint(root, &rescue.id).expect("the rescue snapshot is restorable");
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "REDONE BY HAND after the crash",
            "the hand-redone work is recoverable"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("rescued.rs")).unwrap(),
            "a file only the user wrote"
        );
    }

    #[test]
    fn an_untouched_rewound_tree_still_gets_the_quiet_note_naming_its_snapshot() {
        // The ordinary case: the user did NOT touch the tree while it was in the past. The
        // heal still snapshots first (it cannot know that until it looks), but the note is
        // the calm one — and it STILL names the snapshot, so the promise "nothing was
        // discarded" is checkable rather than a claim.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "past").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre");
        std::fs::write(root.join("src.rs"), "present").unwrap();

        let rewind = begin_temp_rewind(root, &pre).expect("rewind");
        let head = rewind.head_id().to_string();
        std::mem::forget(rewind);
        let dead = TempRewindMarker {
            head: head.clone(),
            to: pre,
            pid: 4_294_967_294,
            started_at: now_secs(),
            boot: crate::run_lock::boot_id(),
            host: crate::run_lock::hostname(),
        };
        std::fs::write(
            root.join(TEMP_REWIND_MARKER_REL),
            serde_json::to_string(&dead).unwrap(),
        )
        .unwrap();

        let note = recover_abandoned_temp_rewind(root).expect("recovered");
        let rescue = rescue_checkpoint(root).expect("the present is snapshotted either way");
        assert_eq!(
            note,
            umadev_i18n::tlf(
                "checkpoint.temp_rewind_recovered",
                &[&head, &rescue.id, &rescue.id]
            ),
            "an untouched tree gets the calm note — which still names the snapshot"
        );
        assert!(
            note.contains(&format!("umadev rollback {}", rescue.id)),
            "…and the command that actually brings it back: {note}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "present"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_heal_that_cannot_snapshot_the_present_refuses_to_reset_anything() {
        // LOSING THE HEAL BEATS LOSING THE CODE. If the current tree cannot be snapshotted,
        // the `reset --hard` has no undo — so it must not happen at all. The tree stays in
        // the past (visibly, with a note and the manual way out) rather than being reset over
        // files we could not save first.
        use std::os::unix::fs::PermissionsExt;
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "the user's real work").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre");
        std::fs::write(root.join("src.rs"), "the user's NEWER real work").unwrap();

        let rewind = begin_temp_rewind(root, &pre).expect("rewind");
        let head = rewind.head_id().to_string();
        std::mem::forget(rewind);
        let marker_path = root.join(TEMP_REWIND_MARKER_REL);
        let dead = TempRewindMarker {
            head: head.clone(),
            to: pre,
            pid: 4_294_967_294,
            started_at: now_secs(),
            boot: crate::run_lock::boot_id(),
            host: crate::run_lock::hostname(),
        };
        std::fs::write(&marker_path, serde_json::to_string(&dead).unwrap()).unwrap();
        // The user redid the work by hand on the rewound tree.
        std::fs::write(root.join("src.rs"), "REDONE BY HAND").unwrap();

        // Now make the SHADOW repo unwritable: git can still read it (so the marker's head
        // validates), but no new commit can be written — i.e. the present cannot be saved.
        let gd = git_dir(root);
        let original = std::fs::metadata(&gd).unwrap().permissions();
        let objects = gd.join("objects");
        let obj_original = std::fs::metadata(&objects).unwrap().permissions();
        std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(&gd, std::fs::Permissions::from_mode(0o555)).unwrap();
        let blocked = create_checkpoint(root, "probe").is_none();
        let result = if blocked {
            recover_abandoned_temp_rewind(root)
        } else {
            None // running as root: permissions do not bind, nothing to assert
        };
        std::fs::set_permissions(&gd, original).unwrap();
        std::fs::set_permissions(&objects, obj_original).unwrap();
        if !blocked {
            return;
        }

        let note = result.expect("standing down is not silent — the user must be told");
        assert!(
            note.contains(&head) && note.contains(&marker_path.display().to_string()),
            "the note names the snapshot with their pre-crash work and the marker: {note}"
        );
        // THE POINT: no reset happened, so the hand-redone work is still on disk.
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "REDONE BY HAND",
            "a heal that cannot save the present must not overwrite it"
        );
        assert!(
            marker_path.exists(),
            "the marker is kept so a later start can heal properly"
        );
        clear_workspace_in_past(root);
    }

    #[test]
    fn a_live_owners_rewind_is_never_touched() {
        // A concurrent run sitting INSIDE its window owns that tree. Yanking it back
        // would corrupt a healthy run — the exact mistake this recovery must not make.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "past").unwrap();
        let pre = create_checkpoint(root, "pre").expect("pre");
        std::fs::write(root.join("src.rs"), "present").unwrap();

        let rewind = begin_temp_rewind(root, &pre).expect("rewind");
        // The marker names US — a live pid.
        assert!(
            recover_abandoned_temp_rewind(root).is_none(),
            "a live owner's rewind is left alone"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "past",
            "the live run's rewound tree is untouched"
        );
        assert!(rewind.restore());
    }

    #[test]
    fn a_long_run_never_empties_the_user_facing_rewind_list() {
        // N4: each step × fix round writes TWO internal snapshots. Past ~100 step-rounds
        // the newest 200 commits are ALL internal, and a fixed 200-commit scan window
        // left the `/rewind` picker showing NOTHING — while every one of the user's
        // checkpoints sat right there in the shadow repo. The display cap must be applied
        // AFTER the internal filter, over a window wide enough to reach real history.
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v0").unwrap();
        create_checkpoint(root, "run-baseline").expect("the user's own checkpoint");

        // Bare empty commits preserve the deep-history shape this test needs without
        // multiplying git subprocesses through checkpoint staging and HEAD re-reads.
        for i in 0..(CHECKPOINT_SCAN_WINDOWS[0] + 20) {
            let label = if i % 2 == 0 {
                TEMP_REWIND_HEAD_LABEL.to_string()
            } else {
                format!("{RED_GREEN_PRE_PREFIX}step {i}")
            };
            let out = git(
                root,
                &["commit", "-q", "--allow-empty", "--no-verify", "-m", &label],
            )
            .expect("git runs");
            assert!(out.status.success(), "internal snapshot {i}");
        }

        let list = list_checkpoints(root);
        assert_eq!(
            list.len(),
            1,
            "the user's checkpoint is still listed under {} internal commits",
            CHECKPOINT_SCAN_WINDOWS[0] + 20
        );
        assert_eq!(list[0].label, "run-baseline");
        assert!(
            !list.iter().any(|c| is_internal_label(&c.label)),
            "internal machinery snapshots stay hidden"
        );
    }

    #[test]
    fn a_transient_history_read_failure_does_not_claim_history_is_exhausted() {
        let internal = Checkpoint {
            id: "internal".to_string(),
            label: TEMP_REWIND_HEAD_LABEL.to_string(),
            when: "2026-07-17T00:00:00Z".to_string(),
        };
        let user = Checkpoint {
            id: "user".to_string(),
            label: "run-baseline".to_string(),
            when: "2026-07-16T00:00:00Z".to_string(),
        };
        let mut reads = 0;
        let list = list_user_checkpoints_with(|window| {
            reads += 1;
            match reads {
                1 => Some(vec![internal.clone(); window]),
                2 => None,
                _ => Some(vec![internal.clone(), user.clone()]),
            }
        });

        assert_eq!(reads, 3, "the wider window is also a bounded retry");
        assert_eq!(list, vec![user]);
    }

    #[test]
    fn a_transiently_unreadable_window_is_retried_before_being_called_empty() {
        // The regression this guards: a shadow-repo read that fails a couple of
        // times under load (an index lock, a slow fsync) must not be reported as
        // "no history" — on the widest window that would empty the /rewind picker
        // while the user's checkpoints sit right there in the shadow repo.
        let mut attempts = 0;
        let got = read_history_with_retry(|| {
            attempts += 1;
            if attempts < GIT_READ_ATTEMPTS {
                None
            } else {
                Some(vec![Checkpoint {
                    id: "abc".to_string(),
                    label: "run-baseline".to_string(),
                    when: "2026-07-19T00:00:00Z".to_string(),
                }])
            }
        });
        assert_eq!(
            attempts, GIT_READ_ATTEMPTS,
            "the read is retried, not abandoned on the first failure"
        );
        assert_eq!(
            got.map(|v| v.len()),
            Some(1),
            "the retry recovers the history"
        );

        // A window that never becomes readable still fails open to None, bounded.
        let mut calls = 0;
        let never = read_history_with_retry(|| {
            calls += 1;
            None::<Vec<Checkpoint>>
        });
        assert!(
            never.is_none(),
            "a persistently unreadable window fails open"
        );
        assert_eq!(calls, GIT_READ_ATTEMPTS, "the retry is bounded");
    }

    /// A marker as a given process/boot/host would have written it.
    fn marker_owned_by(pid: u32, boot: &str, host: &str, started_at: u64) -> TempRewindMarker {
        TempRewindMarker {
            head: "aaaaaaa".to_string(),
            to: "bbbbbbb".to_string(),
            pid,
            started_at,
            boot: boot.to_string(),
            host: host.to_string(),
        }
    }

    #[test]
    fn a_reboot_frees_a_marker_without_ever_yanking_a_live_owners_tree() {
        // Killed mid-rewind → the machine reboots → PIDs restart low → the marker's pid can
        // be handed to an unrelated daemon that probes ALIVE. If we believed that probe
        // forever, the user's tracked source would stay reverted forever.
        //
        // But the mirror-image failure is WORSE, and it is the one a boot-id override
        // causes: a boot id is not trustworthy enough to kill a live claim on its own (it
        // reads EMPTY where the OS won't tell us — `wmic` is gone in current Windows — and
        // on macOS `kern.boottime` is recomputed on every clock correction, so it MOVES
        // within a single boot). "Different boot ⇒ dead" then resets a LIVE run's tree.
        //
        // So a boot mismatch never overrules a live pid; it downgrades to the age window.
        // Both hazards are closed: a live owner is never touched, and a genuinely rebooted
        // one is still freed — instantly when its pid is gone (the normal case), and by the
        // age window in the pid-recycled case.
        let now = 1_000_000;
        let pre_reboot = marker_owned_by(4321, "boot-BEFORE-the-reboot", "same-host", now - 10);

        // The ordinary reboot: the holder is gone, so its pid is gone → recovered at once.
        assert!(
            marker_is_abandoned(
                &pre_reboot,
                now,
                "boot-AFTER-the-reboot",
                "same-host",
                99,
                Some(false)
            ),
            "a rebooted owner's dead pid is abandoned immediately"
        );
        // The pid was RECYCLED onto a live process. Fresh → we do NOT reset the tree yet
        // (this could equally be a live owner under a boot id that merely moved) …
        assert!(
            !marker_is_abandoned(
                &pre_reboot,
                now,
                "boot-AFTER-the-reboot",
                "same-host",
                99,
                Some(true)
            ),
            "a boot-id mismatch alone must never yank a tree from an owner that probes ALIVE"
        );
        // … and past the window it IS recovered, so the user's source can never be stranded.
        let aged = marker_owned_by(
            4321,
            "boot-BEFORE-the-reboot",
            "same-host",
            now - TEMP_REWIND_STALE_SECS - 1,
        );
        assert!(
            marker_is_abandoned(
                &aged,
                now,
                "boot-AFTER-the-reboot",
                "same-host",
                99,
                Some(true)
            ),
            "a rebooted owner with a recycled pid is still freed — by age"
        );
        // The same marker, in the SAME boot, with a live pid → a real concurrent run.
        assert!(
            !marker_is_abandoned(
                &pre_reboot,
                now,
                "boot-BEFORE-the-reboot",
                "same-host",
                99,
                Some(true)
            ),
            "within one boot a live pid is a live run — never touch its tree"
        );
        // Ours, mid-window.
        assert!(
            !marker_is_abandoned(
                &pre_reboot,
                now,
                "boot-BEFORE-the-reboot",
                "same-host",
                4321,
                Some(true)
            ),
            "our own marker is never 'abandoned'"
        );
        // Same boot, dead pid → the ordinary crash path still works.
        assert!(
            marker_is_abandoned(
                &pre_reboot,
                now,
                "boot-BEFORE-the-reboot",
                "same-host",
                99,
                Some(false)
            ),
            "a dead owner in this boot is abandoned"
        );
    }

    #[test]
    fn an_unprobeable_marker_is_only_recovered_after_the_stale_window() {
        // The 900s fallback: liveness cannot be established (an unsupported platform, a
        // probe that errored). We do NOT steal the tree from a process that might be
        // running — we wait out the window, which is far beyond any real rewind.
        let now = 10_000_000;
        let fresh = marker_owned_by(4321, "b", "h", now - 5);
        assert!(
            !marker_is_abandoned(&fresh, now, "b", "h", 99, None),
            "an unprobeable but YOUNG marker is left alone (it may be a live run)"
        );
        let old = marker_owned_by(4321, "b", "h", now - TEMP_REWIND_STALE_SECS - 1);
        assert!(
            marker_is_abandoned(&old, now, "b", "h", 99, None),
            "past the stale window an unprobeable marker is abandoned — the tree comes back"
        );
        // A marker with no usable clock is never aged out: that would be a guess.
        let clockless = marker_owned_by(4321, "b", "h", 0);
        assert!(
            !marker_is_abandoned(&clockless, now, "b", "h", 99, None),
            "no timestamp ⇒ no age verdict"
        );
        // A marker from ANOTHER machine (shared / network workspace): its process table
        // is unreachable and its boot id is unrelated to ours — age is the only signal,
        // and a different boot must NOT be read as "rebooted".
        let other_host = marker_owned_by(4321, "their-boot", "their-host", now - 5);
        assert!(
            !marker_is_abandoned(&other_host, now, "our-boot", "our-host", 99, Some(false)),
            "another host's young marker is left alone (our process table says nothing about it)"
        );
        let other_host_old = marker_owned_by(
            4321,
            "their-boot",
            "their-host",
            now - TEMP_REWIND_STALE_SECS - 1,
        );
        assert!(
            marker_is_abandoned(&other_host_old, now, "our-boot", "our-host", 99, Some(true)),
            "another host's marker is abandoned once it ages out"
        );
    }

    #[test]
    fn a_legacy_marker_without_boot_or_host_still_behaves_exactly_as_before() {
        // FAIL-OPEN / BACKWARD-COMPAT: a marker written by an older build has no boot and
        // no host. Empty = unknown = "matches", so the PID + age rules stand alone —
        // never an accidental "rebooted, therefore abandoned" on a live run.
        let now = 500_000;
        let legacy = TempRewindMarker {
            head: "h".into(),
            to: "t".into(),
            pid: 4321,
            started_at: now - 5,
            boot: String::new(),
            host: String::new(),
        };
        assert!(!marker_is_abandoned(
            &legacy,
            now,
            "boot",
            "host",
            99,
            Some(true)
        ));
        assert!(marker_is_abandoned(
            &legacy,
            now,
            "boot",
            "host",
            99,
            Some(false)
        ));
        assert!(!marker_is_abandoned(
            &legacy,
            now,
            "boot",
            "host",
            4321,
            Some(true)
        ));
        // It also DESERIALIZES (serde default) — an old on-disk marker must still load.
        let old_json = r#"{"head":"abc","to":"def","pid":7,"started_at":11}"#;
        let m: TempRewindMarker = serde_json::from_str(old_json).unwrap();
        assert_eq!(m.head, "abc");
        assert!(m.boot.is_empty() && m.host.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_recovery_that_cannot_reset_the_tree_tells_the_user_instead_of_going_silent() {
        // A failed `reset --hard` leaves the tree in the past. Returning None with no
        // trace is the worst possible outcome: mangled source, no explanation, and a
        // `rollback` that would move them further backwards. The marker is KEPT and the
        // user gets a note naming the snapshot that holds their work.
        use std::os::unix::fs::PermissionsExt;
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "the user's real work").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre");
        std::fs::write(root.join("src.rs"), "the user's NEWER real work").unwrap();
        // A file the step CREATED: restoring it means git must create an entry in the
        // work-tree root, which a read-only root refuses outright.
        std::fs::write(root.join("new.rs"), "a file the step created").unwrap();

        // A killed rewind: the tree is in the past, no destructor ran, only the marker
        // survives (rewritten to a dead owner — the live one is this test process).
        let rewind = begin_temp_rewind(root, &pre).expect("rewind");
        let head = rewind.head_id().to_string();
        std::mem::forget(rewind);
        let marker_path = root.join(TEMP_REWIND_MARKER_REL);
        let dead = TempRewindMarker {
            head: head.clone(),
            to: pre.clone(),
            pid: 4_294_967_294,
            started_at: now_secs(),
            boot: crate::run_lock::boot_id(),
            host: crate::run_lock::hostname(),
        };
        std::fs::write(&marker_path, serde_json::to_string(&dead).unwrap()).unwrap();
        assert!(
            !root.join("new.rs").exists(),
            "precondition: tree is in the past"
        );

        // …and the working tree cannot be written back. git can still read its (separate,
        // still-writable) shadow repo under `.umadev/`, so `has_checkpoints` and the id
        // validation pass — only the `reset --hard` itself fails.
        let original = std::fs::metadata(root).unwrap().permissions();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o555)).unwrap();
        let unwritable = std::fs::write(root.join("probe.tmp"), "x").is_err();
        let result = if unwritable {
            recover_abandoned_temp_rewind(root)
        } else {
            None // running as root: permissions do not bind, nothing to assert
        };
        std::fs::set_permissions(root, original).unwrap();
        if !unwritable {
            return;
        }

        let note =
            result.expect("a rewind we could not undo is NEVER silent — the user must be told");
        assert!(
            note.contains(&head),
            "the note names the snapshot holding their work: {note}"
        );
        assert!(
            marker_path.exists(),
            "the marker is KEPT so the next start can try again"
        );
        assert!(
            note.contains(&marker_path.display().to_string()),
            "…and names the marker, so the user can see the evidence: {note}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "the user's real work",
            "the tree really is still in the past — which is exactly why silence was unacceptable"
        );
    }

    #[test]
    fn recovery_is_harmless_without_a_marker_and_never_silent_when_it_stands_down() {
        // FAIL-OPEN: no marker, a junk marker, a marker naming an unknown commit — each
        // is a no-op, never a reset to something we did not write.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "live").unwrap();
        assert!(recover_abandoned_temp_rewind(root).is_none());

        std::fs::create_dir_all(root.join(".umadev")).unwrap();
        std::fs::write(root.join(TEMP_REWIND_MARKER_REL), "{{{ not json").unwrap();
        assert!(recover_abandoned_temp_rewind(root).is_none());

        // A well-formed marker pointing at a commit that is NOT one of ours.
        let bogus = TempRewindMarker {
            head: "HEAD~999".to_string(),
            to: "deadbeef".to_string(),
            pid: 4_294_967_294,
            started_at: now_secs(),
            boot: crate::run_lock::boot_id(),
            host: crate::run_lock::hostname(),
        };
        std::fs::write(
            root.join(TEMP_REWIND_MARKER_REL),
            serde_json::to_string(&bogus).unwrap(),
        )
        .unwrap();
        // We refuse to reset the tree to a ref we cannot validate — AND WE SAY SO. A silent
        // `None` here left a user whose tree really WAS in the past with mangled source, no
        // explanation, and a `rollback` that moves further backwards. The safety property
        // (never reset to a commit we did not write) and the honesty property (never go
        // quiet about a tree that may be stranded) are BOTH required.
        let note = recover_abandoned_temp_rewind(root)
            .expect("a marker we cannot act on is reported, not swallowed");
        assert!(
            note.contains("HEAD~999") && note.contains("temp-rewind.json"),
            "the note names the snapshot and the marker: {note}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "live",
            "a marker can only ever restore to a checkpoint WE wrote — the tree is untouched"
        );
        assert!(
            root.join(TEMP_REWIND_MARKER_REL).exists(),
            "the marker is KEPT so a later start can try again"
        );
    }

    // ── BLOCKER: a NESTED build dir must never enter a checkpoint ─────────────

    #[test]
    fn a_nested_build_dir_is_never_staged_into_a_checkpoint() {
        // The mainstream JS layout is a monorepo: the build output is at
        // `apps/web/.next/…`, not `.next/…`. A root-anchored-only exclude pathspec
        // force-adds hundreds of MB of compiler output into EVERY checkpoint (and each
        // temporary rewind resets it twice).
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        for heavy in [
            "apps/web/.next/static/chunks/main.js",
            "apps/web/.turbo/cache/x.log",
            "packages/api/dist/index.js",
            "services/py/.venv/lib/site.py",
            "apps/web/coverage/lcov-report/index.html",
            "apps/web/node_modules/react/index.js",
            "apps/docs/.nuxt/app.js",
            "apps/edge/.output/server/index.mjs",
            "nested/deep/build.log",
        ] {
            let p = root.join(heavy);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "// generated").unwrap();
        }
        // Real source, at the same depth.
        let src = root.join("apps/web/src/page.tsx");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "export default () => null;").unwrap();

        create_checkpoint(root, "baseline").expect("checkpoint");
        let tracked = git(root, &["ls-files"])
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        for heavy in [
            ".next/",
            ".turbo/",
            "/dist/",
            ".venv/",
            "coverage/",
            "node_modules/",
            ".nuxt/",
            ".output/",
            ".log",
        ] {
            assert!(
                !tracked.contains(heavy),
                "nested `{heavy}` must stay out of the checkpoint; tracked:\n{tracked}"
            );
        }
        assert!(
            tracked.lines().any(|l| l == "apps/web/src/page.tsx"),
            "real source at the same depth IS captured: {tracked}"
        );
    }

    // ── the user-facing checkpoint list is not flooded by machinery ───────────

    #[test]
    fn internal_rewind_snapshots_are_hidden_from_the_list_but_stay_restorable() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "v1").unwrap();
        let _ = create_run_baseline(root, "demo");
        std::fs::write(root.join("a.rs"), "v2").unwrap();
        let _ = create_phase_checkpoint(root, "phase: frontend");
        // A step's red→green machinery: a pre-state snapshot + a temp-rewind head.
        let pre = create_checkpoint(root, &format!("{RED_GREEN_PRE_PREFIX}impl-1")).expect("pre");
        std::fs::write(root.join("a.rs"), "v3").unwrap();
        let rewind = begin_temp_rewind(root, &pre).expect("rewind");
        let head = rewind.head_id().to_string();
        assert!(rewind.restore());

        let listed = list_checkpoints(root);
        assert!(
            listed
                .iter()
                .all(|c| !c.label.starts_with(RED_GREEN_PRE_PREFIX)
                    && !c.label.starts_with(TEMP_REWIND_HEAD_LABEL)),
            "machinery snapshots are not user rewind points: {listed:?}"
        );
        assert!(
            listed.iter().any(|c| c.label == "phase: frontend"),
            "the real ones still are: {listed:?}"
        );
        assert!(
            run_baseline(root).is_some(),
            "the run baseline stays findable behind the machinery"
        );
        // Hidden from the LIST is not the same as unreachable: both are still valid
        // restore targets by id.
        restore_checkpoint(root, &head).expect("a hidden snapshot is still restorable");
    }

    #[test]
    fn changed_since_run_baseline_says_no_view_rather_than_no_changes() {
        // FAIL-OPEN, and HONEST about it: no baseline ⇒ we could not look. The flattened
        // form used to answer that with an empty `Vec` — indistinguishable from the
        // positive fact "the run touched nothing", which is how a floor enforces nothing
        // and still reports itself green. `None` cannot be misread that way.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.ts"), "x").unwrap();
        assert!(
            changed_since_run_baseline(tmp.path()).is_none(),
            "no baseline is NO VIEW — never an empty change set"
        );
        assert!(
            changed_since(tmp.path(), "deadbeef").is_none(),
            "an unknown baseline id is NO VIEW — never an empty change set"
        );
    }

    // ── a tree already in the past is never rewound AGAIN ─────────────────────

    #[cfg(unix)]
    #[test]
    fn a_second_rewind_cannot_erase_the_halt_and_the_marker_of_the_first() {
        // THE EXACT REPRO. A step declares its evidence as a LIST and takes one rewind per
        // red→green item, while the halt is read once per STEP. So a first rewind whose
        // restore FAILED (tree in the past, halt up, marker naming the TRUE present) was
        // followed, INSIDE THE SAME STEP, by a second rewind that overwrote the marker with
        // a head snapshotting the IN-PAST tree and then, on a successful restore to that
        // head, cleared both the marker and the halt:
        //
        //   after the step:  tree IN THE PAST | halt GONE | marker GONE
        //
        // — the user's real present unreachable, every alarm silenced, and no future start
        // able to heal it. The second rewind must simply REFUSE.
        use std::os::unix::fs::PermissionsExt;
        if !git_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("src.rs"), "the user's real work").unwrap();
        let pre = create_checkpoint(root, "pre-step").expect("pre");
        std::fs::write(root.join("src.rs"), "THE USER'S PRESENT").unwrap();
        // A file that exists only in the present: restoring it needs a write to the
        // work-tree root, which a read-only root refuses — that is how we make restore fail.
        std::fs::write(root.join("new.rs"), "created in the present").unwrap();

        // ── rewind 1: restore() FAILS → the tree is stranded, the halt goes up ──
        let rewind = begin_temp_rewind(root, &pre).expect("rewind 1");
        let true_present = rewind.head_id().to_string();
        let original = std::fs::metadata(root).unwrap().permissions();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o555)).unwrap();
        let unwritable = std::fs::write(root.join("probe.tmp"), "x").is_err();
        let restored = rewind.restore();
        if !unwritable {
            std::fs::set_permissions(root, original).unwrap();
            return; // running as root: permissions do not bind
        }
        assert!(!restored, "precondition: the restore really did fail");
        assert!(
            workspace_is_in_past(root),
            "precondition: the failed restore raised the halt"
        );

        // ── rewind 2, in the SAME step (the next `TestFailsThenPasses` item) ──
        let second = begin_temp_rewind(root, &pre);
        std::fs::set_permissions(root, original).unwrap();
        assert!(
            second.is_none(),
            "a tree already stranded in the past must NEVER be rewound again"
        );

        // AFTER THE STEP: the halt still stands and the marker still names the TRUE
        // present, so the next start's heal can put the user's files back.
        assert!(
            workspace_is_in_past(root),
            "the halt must still be raised — the run has to STOP"
        );
        let marker_path = root.join(TEMP_REWIND_MARKER_REL);
        let body = std::fs::read_to_string(&marker_path).expect("the marker must still exist");
        let marker: TempRewindMarker = serde_json::from_str(&body).unwrap();
        assert_eq!(
            marker.head, true_present,
            "the marker must still name the TRUE present, not a snapshot of the in-past tree"
        );

        // And the heal — the ONE path allowed to clear the halt — really does put the tree
        // back, BECAUSE the marker survived intact. (Re-point the marker at a dead owner:
        // the live one is this test process.)
        let dead = TempRewindMarker {
            pid: 4_294_967_294,
            ..marker
        };
        std::fs::write(&marker_path, serde_json::to_string(&dead).unwrap()).unwrap();
        let note = recover_abandoned_temp_rewind(root).expect("the heal speaks");
        assert!(
            note.contains(&true_present),
            "the note names the head: {note}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "THE USER'S PRESENT",
            "the user's present is recoverable precisely because the marker survived"
        );
        assert!(
            !workspace_is_in_past(root),
            "a genuine restore clears the halt"
        );
    }

    #[test]
    fn a_head_we_cannot_identify_raises_the_halt_and_not_just_a_note() {
        // If we cannot name the commit that held the present, the tree's state is UNKNOWN —
        // and UNKNOWN is exactly when we must stop writing. The note alone left the loop
        // running: it never halted, so a full build's worth of new code could be layered
        // onto a tree that may well be in the past.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "live").unwrap();
        std::fs::create_dir_all(root.join(".umadev")).unwrap();
        let bogus = TempRewindMarker {
            head: "HEAD~999".to_string(),
            to: "deadbeef".to_string(),
            pid: 4_294_967_294,
            started_at: now_secs(),
            boot: crate::run_lock::boot_id(),
            host: crate::run_lock::hostname(),
        };
        std::fs::write(
            root.join(TEMP_REWIND_MARKER_REL),
            serde_json::to_string(&bogus).unwrap(),
        )
        .unwrap();

        assert!(
            recover_abandoned_temp_rewind(root).is_some(),
            "an unidentifiable head is reported"
        );
        assert!(
            workspace_is_in_past(root),
            "…AND it halts the run — an unknown tree state is not a safe tree state"
        );
        // …and nothing may rewind it further while it is in that state.
        assert!(begin_temp_rewind(root, "anything").is_none());
        clear_workspace_in_past(root);
    }

    #[test]
    fn the_in_past_signal_survives_a_different_spelling_of_the_same_root() {
        // The raiser and the reader reach this flag by different routes (the heal is handed
        // whatever `--project-root` said; the run driver polls `options.project_root`). A raw
        // path compare made them MISS each other — the halt raised on one spelling, read on
        // another, so it silently never fired.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        mark_workspace_in_past(root, InPastReason::Retryable);
        for spelling in [
            root.join("."),
            root.join("sub").join(".."),
            PathBuf::from(format!("{}/", root.display())),
        ] {
            assert!(
                workspace_is_in_past(&spelling),
                "the same directory, spelled differently, is the same workspace: {}",
                spelling.display()
            );
        }
        clear_workspace_in_past(&root.join("."));
        assert!(
            !workspace_is_in_past(root),
            "…and clearing through one spelling clears it for all of them"
        );
    }
}
