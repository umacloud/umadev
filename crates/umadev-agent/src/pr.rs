//! PR mode — turn a finished run's evidence into the most trustworthy PR on the
//! team.
//!
//! Most generated PRs hand a reviewer raw code and a one-line title. PR mode
//! flips that: it opens a pull request whose **body is the run's own evidence**
//! — the PR-ready review report (`review.rs`: contract / acceptance / coverage /
//! governance / security / runtime + rollback) followed by a proof-pack summary.
//! The reviewer reads a self-asserting, source-cited case for merge, not a diff
//! with no context.
//!
//! This module is **deterministic + fail-open + light-deps**. It never opens a
//! PR itself: it computes *readiness* (is this a git repo? are there uncommitted
//! changes? is there a GitHub remote? is `gh` on PATH and logged in?) and
//! *renders* the body. The binary (`umadev pr`) does the actual `git` / `gh`
//! shell-out and enforces the safety rails. The split keeps every decision here
//! a pure function the unit tests can assert on **without ever pushing or
//! opening a real PR**.
//!
//! Safety contract surfaced as data here, enforced by the caller:
//! - never commit directly on the default branch — branch first
//!   ([`PrPlan::needs_new_branch`]);
//! - never force-push, never rewrite the user's existing commits;
//! - any external probe that errors degrades to "not ready" with a manual hint,
//!   never a panic or a destructive fallback.

use std::path::Path;
use std::time::Duration;

use crate::review::{build_review_report, render_review_md};

const PR_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PR_GIT_TIMEOUT: Duration = Duration::from_secs(30);
const PR_PROBE_BYTES: usize = 4 * 1024 * 1024;

/// One readiness precondition for opening a PR automatically, and whether it
/// holds. Kept as data so the renderer + the manual-steps hint are pure
/// functions over a [`PrReadiness`] and the tests can assert on the structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadinessCheck {
    /// Short machine-ish id (e.g. `git-repo`, `gh-cli`).
    pub id: String,
    /// Human one-liner shown to the user.
    pub label: String,
    /// `true` iff the precondition is satisfied.
    pub ok: bool,
    /// What to do by hand when this check fails (empty when `ok`).
    pub remedy: String,
}

/// The full readiness picture: every precondition + the resolved branch facts.
/// `ready()` is `true` only when every *blocking* check passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrReadiness {
    /// Ordered preconditions.
    pub checks: Vec<ReadinessCheck>,
    /// The repo's default branch (e.g. `main`), best-effort; empty if unknown.
    pub default_branch: String,
    /// The branch currently checked out, best-effort; empty if unknown.
    pub current_branch: String,
    /// `true` when the working tree has uncommitted changes to commit.
    pub has_changes: bool,
}

impl PrReadiness {
    /// `true` iff every check passed — safe to drive `git` + `gh` end-to-end.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    /// The first failing check, if any (the headline reason we can't proceed).
    #[must_use]
    pub fn first_blocker(&self) -> Option<&ReadinessCheck> {
        self.checks.iter().find(|c| !c.ok)
    }
}

/// The concrete, safety-checked plan for opening the PR. Computed from a
/// [`PrReadiness`] so the caller's git/gh sequence is pure data it can print +
/// execute, and the tests can assert the safety rails without shelling out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrPlan {
    /// `true` when we must create a feature branch first because HEAD is on the
    /// default branch — **we never commit directly on the default branch**.
    pub needs_new_branch: bool,
    /// The branch we will commit + push to (an existing non-default branch, or
    /// the freshly-suggested feature branch name).
    pub head_branch: String,
    /// The base branch the PR targets (the repo default branch).
    pub base_branch: String,
}

/// Decide the branch plan from readiness. Pure: no side effects.
///
/// Rule: if we're on the default branch (or the current branch is unknown but a
/// default exists), create a fresh feature branch derived from `slug`; otherwise
/// keep the user's already-checked-out feature branch as the PR head. We never
/// return a plan that would commit onto the default branch.
#[must_use]
pub fn plan_branches(readiness: &PrReadiness, slug: &str) -> PrPlan {
    let base = if readiness.default_branch.is_empty() {
        "main".to_string()
    } else {
        readiness.default_branch.clone()
    };
    // Never commit ONTO a well-known default branch, even when default_branch detection
    // missed the repo real default (a master-default repo with no origin/HEAD and no
    // init.defaultBranch falls back to "main"): without this guard current="master" !=
    // base="main" would take the else arm and commit straight to master, ESCAPING branch
    // isolation. A checkout on any conventional default forces a fresh feature branch.
    let on_default = readiness.current_branch == base
        || readiness.current_branch.is_empty()
        || matches!(
            readiness.current_branch.as_str(),
            "main" | "master" | "trunk" | "develop" | "development"
        );
    if on_default {
        PrPlan {
            needs_new_branch: true,
            head_branch: feature_branch_name(slug),
            base_branch: base,
        }
    } else {
        PrPlan {
            needs_new_branch: false,
            head_branch: readiness.current_branch.clone(),
            base_branch: base,
        }
    }
}

/// A safe, deterministic feature-branch name for a slug. Sanitised to the subset
/// git + GitHub accept everywhere (lowercase alnum + `-`), prefixed `umadev/`.
#[must_use]
pub fn feature_branch_name(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    let stem = if trimmed.is_empty() {
        "change"
    } else {
        trimmed
    };
    format!("umadev/{stem}")
}

// =====================================================================
// readiness probes (fail-open shell-out to git / gh)
// =====================================================================

/// Probe the workspace and the environment for PR readiness. Every probe is
/// fail-open: a spawn error / non-zero exit is read as "precondition not met"
/// and surfaced as a failing [`ReadinessCheck`] with a manual remedy — never a
/// panic, never a destructive assumption.
#[must_use]
pub fn assess_readiness(project_root: &Path) -> PrReadiness {
    let is_repo = git_is_repo(project_root);
    let default_branch = if is_repo {
        git_default_branch(project_root)
    } else {
        String::new()
    };
    let current_branch = if is_repo {
        git_current_branch(project_root)
    } else {
        String::new()
    };
    let has_changes = is_repo && git_has_pr_changes(project_root);
    let has_remote = is_repo && git_has_github_remote(project_root);
    let gh_present = gh_on_path(project_root);
    let gh_authed = gh_present && gh_logged_in(project_root);

    let checks = vec![
        ReadinessCheck {
            id: "git-repo".to_string(),
            label: "Inside a git repository".to_string(),
            ok: is_repo,
            remedy: if is_repo {
                String::new()
            } else {
                "Run `git init` (and commit a baseline) before opening a PR.".to_string()
            },
        },
        ReadinessCheck {
            id: "has-changes".to_string(),
            label: "There are changes to put in the PR".to_string(),
            ok: has_changes,
            remedy: if has_changes {
                String::new()
            } else {
                "Nothing to commit — run the pipeline so it writes artifacts/code first."
                    .to_string()
            },
        },
        ReadinessCheck {
            id: "github-remote".to_string(),
            label: "A GitHub remote is configured".to_string(),
            ok: has_remote,
            remedy: if has_remote {
                String::new()
            } else {
                "Add a GitHub remote: `git remote add origin <github-url>`.".to_string()
            },
        },
        ReadinessCheck {
            id: "gh-cli".to_string(),
            label: "GitHub CLI (`gh`) is installed".to_string(),
            ok: gh_present,
            remedy: if gh_present {
                String::new()
            } else {
                "Install GitHub CLI from https://cli.github.com/ to open the PR automatically."
                    .to_string()
            },
        },
        ReadinessCheck {
            id: "gh-auth".to_string(),
            label: "GitHub CLI is logged in".to_string(),
            ok: gh_authed,
            remedy: if gh_authed {
                String::new()
            } else {
                "Authenticate once with `gh auth login`, then re-run `umadev pr`.".to_string()
            },
        },
    ];

    PrReadiness {
        checks,
        default_branch,
        current_branch,
        has_changes,
    }
}

/// Render the manual fallback steps for when automation isn't ready. Pure over
/// [`PrReadiness`] + the rendered body, so the user can always finish the PR by
/// hand. Lists exactly the failing preconditions and their remedies, then the
/// generic git/gh recipe, and points at the body we *would* have used.
#[must_use]
pub fn manual_steps(readiness: &PrReadiness, slug: &str, body_path_rel: &str) -> String {
    let plan = plan_branches(readiness, slug);
    let mut out = String::from("Could not open the PR automatically. Resolve these, then retry:\n");
    for c in readiness.checks.iter().filter(|c| !c.ok) {
        out.push_str(&format!("  - {} — {}\n", c.label, c.remedy));
    }
    out.push_str("\nOr open it by hand (UmaDev never force-pushes or rewrites your commits):\n");
    if plan.needs_new_branch {
        out.push_str(&format!(
            "  git switch -c {head}            # never commit on `{base}` directly\n",
            head = plan.head_branch,
            base = plan.base_branch
        ));
    }
    out.push_str(&format!(
        "  git add -- output release <your changed source paths>   \
         # stage the run's evidence + your code explicitly (never the whole tree — \
         a blanket stage can sweep in .env / temp / build junk)\n  \
         git commit -m \"{slug}: UmaDev pipeline output\"\n  \
         git push -u origin {head}\n  \
         gh pr create --base {base} --head {head} --title \"{slug}\" --body-file {body}\n",
        slug = slug,
        head = plan.head_branch,
        base = plan.base_branch,
        body = body_path_rel,
    ));
    out
}

// =====================================================================
// PR body rendering (reuse the review-report + proof-pack summary)
// =====================================================================

/// Workspace-relative path of the rendered PR body (so the manual recipe and the
/// `gh --body-file` path agree).
#[must_use]
pub fn pr_body_rel_path(slug: &str) -> String {
    // Sanitize (see `crate::runner::sanitize_slug`) so a slug like `../x` or
    // `/tmp/x` can't move the PR body outside `output/` or make it absolute.
    let slug = crate::runner::sanitize_slug(slug);
    format!("output/{slug}-pr-body.md")
}

/// Build the full PR body markdown: the PR-ready review report (verbatim from
/// `review.rs`) followed by a proof-pack summary. Pure assembly over artifacts
/// UmaDev already produced — fail-open: missing artifacts degrade to honest
/// "not available" lines inside the review report, and an absent proof-pack
/// renders a one-line note rather than an error.
#[must_use]
pub fn render_pr_body(project_root: &Path, slug: &str) -> String {
    // Sanitize at the boundary so both the review report and the proof-pack
    // summary below build only in-`output/` paths from the slug.
    let slug = &crate::runner::sanitize_slug(slug);
    let report = build_review_report(project_root, slug);
    let review_md = render_review_md(&report);

    let mut out = String::new();
    out.push_str(&review_md);
    out.push_str("\n---\n\n## Proof pack\n\n");
    out.push_str(&proof_pack_summary(project_root, slug));
    out.push_str(
        "\n_This PR body was generated by UmaDev from the run's own evidence. \
         Every claim above cites the file or number it derives from._\n",
    );
    out
}

/// Summarise the delivery proof-pack(s) in `release/` for the PR body. Names the
/// latest zip + its size, or an honest note when none exists yet. Pure read.
#[must_use]
pub fn proof_pack_summary(project_root: &Path, _slug: &str) -> String {
    match latest_proof_pack(project_root) {
        Some((path, size_bytes)) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!(
                "Full evidence bundle: `release/{name}` ({} KiB) — docs + quality gate + \
                 compliance mapping + a runnable scorecard, retained for post-merge audit.\n",
                size_bytes / 1024
            )
        }
        None => "No proof-pack zip yet (it is produced at the `delivery` phase). \
             The review checklist above still reflects the current evidence.\n"
            .to_string(),
    }
}

/// The newest `release/proof-pack-*.zip` and its size in bytes, or `None`.
#[must_use]
pub fn latest_proof_pack(project_root: &Path) -> Option<(std::path::PathBuf, u64)> {
    let release = project_root.join("release");
    let mut packs: Vec<std::path::PathBuf> = std::fs::read_dir(&release)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let is_zip = p
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
            let named = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.starts_with("proof-pack-"));
            is_zip && named
        })
        .collect();
    packs.sort();
    let latest = packs.pop()?;
    let size = std::fs::metadata(&latest).map_or(0, |m| m.len());
    Some((latest, size))
}

// =====================================================================
// Branch isolation — git as the trust substrate (Wave 6 deliverable 2)
// =====================================================================

/// The outcome of trying to move a workspace-mutating run onto its own derived
/// `umadev/<slug>` branch BEFORE the base writes anything.
///
/// This is the safety primitive behind "commercial-grade = the user can trust
/// it": a run never edits the user's `main`/default branch or their currently
/// checked-out working branch in place — it derives a sibling branch from the
/// current `HEAD` and works there, so the user reviews + merges on their own
/// terms. We **never auto-merge, never push, never touch a remote, never delete
/// anything**. Every step is fail-open: a non-git directory, an unavailable
/// `git`, a dirty tree we won't clobber, or any probe error → [`Self::Skipped`]
/// (the run proceeds exactly as it does today, in the working tree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchIsolation {
    /// We are now on a derived `umadev/<slug>` branch (created fresh from the
    /// prior HEAD, or already checked out from a previous run). Carries the
    /// branch name and the branch we derived FROM, for the audit note.
    Isolated {
        /// The `umadev/<slug>` branch the run now operates on.
        branch: String,
        /// The branch we derived from (the user's prior HEAD), for the note.
        from: String,
        /// `true` when this call created the branch; `false` when we merely
        /// re-entered a branch a prior run already created.
        created: bool,
    },
    /// Isolation was deliberately skipped — fail-open. The run proceeds in the
    /// working tree exactly as before. Carries a short machine-ish reason
    /// (`not-a-repo` / `git-unavailable` / `dirty-tree` / `detached` / `error`)
    /// for the audit note; never an error the host has to handle.
    Skipped(&'static str),
}

impl BranchIsolation {
    /// `true` when the run is now on its own derived branch.
    #[must_use]
    pub fn is_isolated(&self) -> bool {
        matches!(self, Self::Isolated { .. })
    }

    /// The branch the run operates on, or empty when skipped.
    #[must_use]
    pub fn branch(&self) -> &str {
        match self {
            Self::Isolated { branch, .. } => branch,
            Self::Skipped(_) => "",
        }
    }
}

/// Whether `branch` is one of UmaDev's derived isolation branches. Used so a
/// second block of the SAME run (continue / resume) that is already sitting on
/// `umadev/<slug>` is recognised and left alone rather than deriving a branch
/// off a branch.
#[must_use]
pub fn is_isolation_branch(branch: &str) -> bool {
    branch.starts_with("umadev/")
}

/// Move a workspace-mutating run onto its own derived `umadev/<slug>` branch,
/// fail-open and **extremely conservative** (this is the one irreversible-ish
/// piece, so it never does anything the user can't trivially undo):
///
/// 1. Not a git repo / `git` unavailable → [`BranchIsolation::Skipped`] — run
///    in the working tree exactly as today.
/// 2. Already on a `umadev/<slug>` branch → stay there ([`BranchIsolation::Isolated`]
///    with `created: false`); a later block of the same run re-enters cleanly.
/// 3. On the user's default / working branch with a CLEAN-enough tree → create
///    (or fast-forward-checkout) `umadev/<slug>` **from the current HEAD** and
///    switch to it. We do NOT commit, push, merge, or touch any remote — we only
///    move the local branch pointer so subsequent writes land off the user's
///    branch. The user's prior branch is untouched and still points where it did.
/// 4. A DIRTY working tree on the user's branch → we will NOT risk carrying or
///    losing their uncommitted edits across a branch switch, so we **skip**
///    (`dirty-tree`) and run in place. Safer to not isolate than to disturb
///    pending user work.
///
/// `git switch -c` / `git checkout -b` are pointer moves on the SAME work-tree
/// content (the new branch starts at the current commit), so no file is deleted
/// and the user's other branch keeps its commits. We never use `-f`/`--force`.
#[must_use]
pub fn ensure_isolation_branch(project_root: &Path, slug: &str) -> BranchIsolation {
    if !git_is_repo(project_root) {
        return BranchIsolation::Skipped("not-a-repo");
    }
    let current = git_current_branch(project_root);
    // Detached HEAD (empty) → no branch to derive a name-stable sibling from
    // safely; stay put rather than create a branch off a detached state.
    if current.is_empty() {
        return BranchIsolation::Skipped("detached");
    }
    // Already isolated (a continue/resume block of the same run) → nothing to do.
    if is_isolation_branch(&current) {
        return BranchIsolation::Isolated {
            branch: current.clone(),
            from: current,
            created: false,
        };
    }
    let target = feature_branch_name(slug);
    // If that exact branch already exists (a prior run's branch) we only REUSE it
    // when it still descends from (or equals) the current HEAD — otherwise a new
    // run would base its work on the prior run's STALE history (the user has since
    // advanced their branch, or the old isolation branch diverged). A safe reuse
    // switches with no `-c` (which refuses to clobber local changes); a stale one
    // gets a FRESH, uniquely-suffixed sibling derived from the real current HEAD.
    if git_branch_exists(project_root, &target) {
        if branch_descends_from_head(project_root, &target) {
            if run_git(project_root, &["switch", &target]).is_some()
                || run_git(project_root, &["checkout", &target]).is_some()
            {
                return BranchIsolation::Isolated {
                    branch: target,
                    from: current,
                    created: false,
                };
            }
            // Couldn't switch (dirty tree / git refused) → fail-open, stay in place.
            return BranchIsolation::Skipped("dirty-tree");
        }
        // Stale branch (diverged from HEAD). Only branch fresh from a CLEAN tree
        // (same conservative guard as the create path below); a dirty tree → skip
        // rather than risk carrying the user's uncommitted edits onto a new branch.
        if git_has_changes(project_root) {
            return BranchIsolation::Skipped("dirty-tree");
        }
        let fresh = unique_branch_name(project_root, &target);
        let created = run_git(project_root, &["switch", "-c", &fresh]).is_some()
            || run_git(project_root, &["checkout", "-b", &fresh]).is_some();
        return if created {
            BranchIsolation::Isolated {
                branch: fresh,
                from: current,
                created: true,
            }
        } else {
            BranchIsolation::Skipped("error")
        };
    }
    // A dirty tree + a real branch switch risks the user's uncommitted edits.
    // Be conservative: do not isolate, run in place. (A clean tree carries
    // nothing across, so `switch -c` is a pure pointer move.)
    if git_has_changes(project_root) {
        return BranchIsolation::Skipped("dirty-tree");
    }
    // Create the sibling branch FROM the current HEAD and switch to it. Try the
    // modern `switch -c` first, fall back to `checkout -b` for older git. No
    // `-f`/`--force`: a refusal means we leave the user where they were.
    let created = run_git(project_root, &["switch", "-c", &target]).is_some()
        || run_git(project_root, &["checkout", "-b", &target]).is_some();
    if created {
        BranchIsolation::Isolated {
            branch: target,
            from: current,
            created: true,
        }
    } else {
        BranchIsolation::Skipped("error")
    }
}

/// `true` when `branch` descends from (or exactly matches) the current HEAD —
/// i.e. HEAD is an ancestor of `branch`, so reusing `branch` builds this run on
/// the CURRENT history, not a prior run's stale snapshot. Uses
/// `git merge-base --is-ancestor HEAD <branch>` (exit 0 = ancestor; `run_git`
/// maps a non-zero exit to `None`). Fail-open: a probe error reads as "does NOT
/// descend" so the caller branches fresh rather than risk reusing a stale branch
/// (the safe direction).
fn branch_descends_from_head(project_root: &Path, branch: &str) -> bool {
    run_git(
        project_root,
        &["merge-base", "--is-ancestor", "HEAD", branch],
    )
    .is_some()
}

/// A fresh isolation-branch name that does NOT already exist, derived by suffixing
/// `-2`, `-3`, … onto `base`. Used when the natural `umadev/<slug>` branch already
/// exists but has DIVERGED from the current HEAD (a stale prior-run branch), so a
/// new run gets its own branch off the real HEAD instead of reusing stale history.
/// Bounded (never spins); the extremely unlikely all-taken case falls back to a
/// wall-clock suffix.
fn unique_branch_name(project_root: &Path, base: &str) -> String {
    for n in 2..=99 {
        let candidate = format!("{base}-{n}");
        if !git_branch_exists(project_root, &candidate) {
            return candidate;
        }
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{base}-{suffix}")
}

/// `true` iff a local branch named `branch` already exists. Fail-open: a probe
/// error reads as "does not exist" so we attempt a fresh create rather than a
/// switch (and the create itself fails open).
fn git_branch_exists(project_root: &Path, branch: &str) -> bool {
    run_git(
        project_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .map(|s| !s.trim().is_empty())
    .unwrap_or(false)
}

// =====================================================================
// git / gh helpers — each fail-open
// =====================================================================

/// `true` iff `project_root` is inside a git work tree.
fn git_is_repo(project_root: &Path) -> bool {
    run_git(project_root, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// The repo's default branch. Tries the remote HEAD symref first
/// (`origin/HEAD` → e.g. `main`), then falls back to the current branch, then
/// to `main`. Best-effort: a failure returns an empty string (caller treats
/// empty as "unknown" and still avoids committing on a guessed default).
fn git_default_branch(project_root: &Path) -> String {
    if let Some(out) = run_git(
        project_root,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        // e.g. `origin/main` → `main`
        if let Some(name) = out.trim().rsplit('/').next() {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    // Fall back to the configured init default, else the common `main`.
    run_git(project_root, &["config", "--get", "init.defaultBranch"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string())
}

/// The branch currently checked out, or empty (detached HEAD / failure).
fn git_current_branch(project_root: &Path) -> String {
    run_git(project_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD")
        .unwrap_or_default()
}

/// `true` iff `git status --porcelain` reports any change (staged, unstaged, or
/// untracked) — i.e. there is something to commit into the PR.
fn git_has_changes(project_root: &Path) -> bool {
    run_git(project_root, &["status", "--porcelain"])
        .map(|s| {
            s.lines().any(|line| {
                // Porcelain line: "XY <path>" (2 status chars + space, then path;
                // a rename is "XY old -> new"). IGNORE UmaDev's OWN artifact dirs
                // (`.umadev/`, `output/`): a run-lock / governance-context / plan we
                // just wrote is NOT the user's uncommitted work, and must not make
                // the tree read as "dirty" and skip branch isolation (the run lock
                // under `.umadev/` is written before isolation runs). We only care
                // whether the USER has uncommitted edits.
                let path = line.get(3..).unwrap_or("").trim().trim_matches('"');
                let path = path.rsplit(" -> ").next().unwrap_or(path);
                // UmaDev's own tooling dirs, written before isolation runs and never
                // the user's product code: `.umadev/` (run-lock/plan/audit),
                // `output/` (artifacts), `.claude/` (the governance PreToolUse-hook
                // settings UmaDev installs). A `switch -c` carries any of these over
                // to the isolation branch harmlessly anyway.
                let our_dir = |d: &str| {
                    path.starts_with(&format!("{d}/")) || path.starts_with(&format!("{d}\\"))
                };
                !path.is_empty() && !our_dir(".umadev") && !our_dir("output") && !our_dir(".claude")
            })
        })
        .unwrap_or(false)
}

/// PR-specific change probe. Unlike run-isolation's [`git_has_changes`], this
/// counts `output/` and `release/`: those are review evidence and can be the
/// entire deliverable for a documentation-only run. UmaDev's transient state
/// and installed Claude hook remain excluded. The caller assesses readiness
/// before writing the new PR body, so that body cannot manufacture readiness.
fn git_has_pr_changes(project_root: &Path) -> bool {
    run_git(project_root, &["status", "--porcelain"])
        .map(|status| {
            status.lines().any(|line| {
                let path = line.get(3..).unwrap_or("").trim().trim_matches('"');
                let path = path.rsplit(" -> ").next().unwrap_or(path);
                let our_transient_dir = |dir: &str| {
                    path.starts_with(&format!("{dir}/")) || path.starts_with(&format!("{dir}\\"))
                };
                !path.is_empty() && !our_transient_dir(".umadev") && !our_transient_dir(".claude")
            })
        })
        .unwrap_or(false)
}

/// `true` iff any configured remote URL points at github.com.
fn git_has_github_remote(project_root: &Path) -> bool {
    run_git(project_root, &["remote", "-v"])
        .map(|s| s.to_ascii_lowercase().contains("github.com"))
        .unwrap_or(false)
}

/// `true` iff `gh` resolves on PATH (a `gh --version` succeeds).
fn gh_on_path(project_root: &Path) -> bool {
    crate::external_command::bounded_gh_output(
        project_root,
        &["--version"],
        PR_PROBE_TIMEOUT,
        64 * 1024,
    )
    .map(|output| output.status.success())
    .unwrap_or(false)
}

/// `true` iff `gh auth status` reports a logged-in account.
fn gh_logged_in(project_root: &Path) -> bool {
    crate::external_command::bounded_gh_output(
        project_root,
        &["auth", "status"],
        PR_PROBE_TIMEOUT,
        256 * 1024,
    )
    .map(|output| output.status.success())
    .unwrap_or(false)
}

/// Run a git subcommand in `project_root`, returning stdout on success. Any
/// spawn error / non-zero exit → `None` (fail-open).
fn run_git(project_root: &Path, args: &[&str]) -> Option<String> {
    let out = crate::external_command::bounded_git_output(
        project_root,
        args,
        PR_GIT_TIMEOUT,
        PR_PROBE_BYTES,
    )?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn pr_body_path_is_sanitized_against_traversal() {
        // Normal slug unchanged.
        assert_eq!(pr_body_rel_path("app"), "output/app-pr-body.md");
        // Hostile slugs stay inside output/ and never go absolute.
        for hostile in ["../etc/passwd", "/tmp/x", "..\\..\\x"] {
            let rel = pr_body_rel_path(hostile);
            assert!(rel.starts_with("output/"), "{hostile:?} -> {rel:?}");
            assert!(!rel.contains(".."), "{hostile:?} -> {rel:?}");
            assert!(
                !std::path::Path::new(&rel).is_absolute(),
                "{hostile:?} -> absolute {rel:?}"
            );
        }
    }

    fn readiness(default: &str, current: &str, all_ok: bool) -> PrReadiness {
        let mk = |id: &str| ReadinessCheck {
            id: id.to_string(),
            label: id.to_string(),
            ok: all_ok,
            remedy: if all_ok {
                String::new()
            } else {
                format!("fix {id}")
            },
        };
        PrReadiness {
            checks: vec![
                mk("git-repo"),
                mk("has-changes"),
                mk("github-remote"),
                mk("gh-cli"),
                mk("gh-auth"),
            ],
            default_branch: default.to_string(),
            current_branch: current.to_string(),
            has_changes: all_ok,
        }
    }

    #[test]
    fn ready_only_when_every_check_passes() {
        assert!(readiness("main", "feature/x", true).ready());
        let mut r = readiness("main", "feature/x", true);
        r.checks[2].ok = false;
        assert!(!r.ready());
        assert_eq!(r.first_blocker().unwrap().id, "github-remote");
    }

    #[test]
    fn on_default_branch_forces_a_new_feature_branch() {
        // SAFETY: HEAD on the default branch must never be committed onto.
        let r = readiness("main", "main", true);
        let plan = plan_branches(&r, "my-app");
        assert!(plan.needs_new_branch);
        assert_eq!(plan.head_branch, "umadev/my-app");
        assert_eq!(plan.base_branch, "main");
    }

    #[test]
    fn a_master_default_repo_never_commits_onto_master_even_if_base_misdetected() {
        // D-H1: git_default_branch fell back to "main" (no origin/HEAD), but the repo real
        // default is "master" and HEAD is on master. Committing there would ESCAPE branch
        // isolation, so a checkout on any conventional default forces a fresh branch.
        let r = readiness("main", "master", true);
        let plan = plan_branches(&r, "my-app");
        assert!(
            plan.needs_new_branch,
            "on master must branch even if base=main"
        );
        assert_eq!(plan.head_branch, "umadev/my-app");
    }

    #[test]
    fn unknown_current_branch_also_forces_a_new_branch() {
        // Detached HEAD / unknown current branch → never reuse, branch fresh.
        let r = readiness("main", "", true);
        let plan = plan_branches(&r, "app");
        assert!(plan.needs_new_branch);
        assert_eq!(plan.head_branch, "umadev/app");
    }

    #[test]
    fn existing_feature_branch_is_reused_as_head() {
        let r = readiness("main", "feat/login", true);
        let plan = plan_branches(&r, "app");
        assert!(!plan.needs_new_branch);
        assert_eq!(plan.head_branch, "feat/login");
        assert_eq!(plan.base_branch, "main");
    }

    #[test]
    fn unknown_default_branch_defaults_base_to_main() {
        let r = readiness("", "", true);
        let plan = plan_branches(&r, "x");
        assert_eq!(plan.base_branch, "main");
        assert!(plan.needs_new_branch); // current empty == treated as on-default
    }

    #[test]
    fn feature_branch_name_is_sanitised() {
        assert_eq!(feature_branch_name("My App!"), "umadev/my-app");
        assert_eq!(feature_branch_name("  ---  "), "umadev/change");
        assert_eq!(feature_branch_name(""), "umadev/change");
        assert_eq!(feature_branch_name("clean-slug-1"), "umadev/clean-slug-1");
    }

    #[test]
    fn manual_steps_list_failing_checks_and_safe_recipe() {
        let r = readiness("main", "main", false);
        let steps = manual_steps(&r, "demo", "output/demo-pr-body.md");
        // Every failing check is surfaced with its remedy.
        assert!(steps.contains("fix github-remote"));
        // On the default branch the recipe MUST branch first (no direct commit).
        assert!(steps.contains("git switch -c umadev/demo"));
        assert!(steps.contains("never commit on `main` directly"));
        // Never instructs a force-push: the recipe must use a plain `push`,
        // never `--force` / `-f` / `+ref` (the safety promise in the header text
        // may *mention* "force-push", but no actual force command is emitted).
        assert!(steps.contains("git push -u origin"));
        assert!(!steps.contains("--force"));
        assert!(!steps.contains("push -f"));
        assert!(steps.contains("--body-file output/demo-pr-body.md"));
        // Consistent with the auto path (`pr --create` stages ONLY output/+release/,
        // never the whole tree): the manual recipe must NOT recommend a blanket
        // `git add -A`, which can sweep in .env / temp / build junk.
        assert!(
            !steps.contains("git add -A"),
            "manual steps must not recommend a blanket `git add -A`"
        );
        assert!(
            steps.contains("git add -- output release"),
            "manual steps stage the run's evidence explicitly"
        );
    }

    #[test]
    fn manual_steps_skip_branch_line_on_feature_branch() {
        let r = readiness("main", "feat/x", false);
        let steps = manual_steps(&r, "demo", "output/demo-pr-body.md");
        assert!(!steps.contains("git switch -c"));
        assert!(steps.contains("gh pr create --base main --head feat/x"));
    }

    #[test]
    fn body_embeds_review_report_and_proof_pack_section() {
        // Bare workspace: review report degrades to fail-open claims, proof-pack
        // section reports "no zip yet" — nothing panics, body is well-formed.
        let tmp = TempDir::new().unwrap();
        let body = render_pr_body(tmp.path(), "demo");
        assert!(body.contains("# Review report — demo"));
        assert!(body.contains("## Proof pack"));
        assert!(body.contains("No proof-pack zip yet"));
        assert!(body.contains("generated by UmaDev"));
    }

    #[test]
    fn proof_pack_summary_names_latest_zip() {
        let tmp = TempDir::new().unwrap();
        let release = tmp.path().join("release");
        fs::create_dir_all(&release).unwrap();
        fs::write(release.join("proof-pack-demo-001.zip"), vec![0u8; 2048]).unwrap();
        fs::write(release.join("proof-pack-demo-002.zip"), vec![0u8; 4096]).unwrap();
        // Non-pack files are ignored.
        fs::write(release.join("notes.txt"), "x").unwrap();
        let summary = proof_pack_summary(tmp.path(), "demo");
        // Latest by sort order is ...-002.
        assert!(summary.contains("proof-pack-demo-002.zip"));
        assert!(summary.contains("4 KiB"));
    }

    #[test]
    fn assess_on_non_repo_is_fail_open_not_ready() {
        // A plain temp dir is not a git repo → not ready, but no panic, and the
        // first blocker is the git-repo check.
        let tmp = TempDir::new().unwrap();
        let r = assess_readiness(tmp.path());
        assert_eq!(r.checks.len(), 5);
        assert!(!r.ready());
        assert_eq!(r.first_blocker().unwrap().id, "git-repo");
        // Rendering the manual steps over a not-ready assessment never panics.
        let steps = manual_steps(&r, "demo", &pr_body_rel_path("demo"));
        assert!(steps.contains("git init"));
    }

    #[test]
    fn body_rel_path_is_stable() {
        assert_eq!(pr_body_rel_path("app"), "output/app-pr-body.md");
    }

    // ---- branch isolation (Wave 6) ---------------------------------------

    fn git_available() -> bool {
        run_git(Path::new("."), &["--version"]).is_some()
    }

    /// Init a real git repo with one commit on `main`, returning the temp dir.
    fn init_repo(default: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let run = |args: &[&str]| {
            run_git(root, args).unwrap();
        };
        run(&["init", "-q", "-b", default]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        fs::write(root.join("seed.txt"), "v1").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "seed"]);
        tmp
    }

    #[test]
    fn pr_readiness_counts_output_only_deliverables_but_not_transient_state() {
        if !git_available() {
            return;
        }
        let tmp = init_repo("main");
        let root = tmp.path();
        fs::create_dir_all(root.join(".umadev")).unwrap();
        fs::write(root.join(".umadev/plan.json"), "{}\n").unwrap();
        assert!(
            !assess_readiness(root).has_changes,
            "transient state alone is not a PR deliverable"
        );

        fs::create_dir_all(root.join("output")).unwrap();
        fs::write(root.join("output/demo-prd.md"), "# PRD\n").unwrap();
        assert!(
            assess_readiness(root).has_changes,
            "a docs-only run must be eligible for a PR"
        );
    }

    #[test]
    fn isolation_skipped_on_non_repo() {
        // Fail-open: a plain dir is not a git repo → never error, just skip so the
        // run proceeds in the working tree exactly as today.
        let tmp = TempDir::new().unwrap();
        let iso = ensure_isolation_branch(tmp.path(), "my-app");
        assert_eq!(iso, BranchIsolation::Skipped("not-a-repo"));
        assert!(!iso.is_isolated());
        assert!(iso.branch().is_empty());
    }

    #[test]
    fn isolation_creates_umadev_branch_from_default_and_never_touches_it() {
        if !git_available() {
            return;
        }
        let tmp = init_repo("main");
        let root = tmp.path();
        // On `main`, clean tree → derive `umadev/<slug>` from HEAD and switch.
        let iso = ensure_isolation_branch(root, "my-app");
        assert!(iso.is_isolated(), "clean repo on default must isolate");
        assert_eq!(iso.branch(), "umadev/my-app");
        match &iso {
            BranchIsolation::Isolated { from, created, .. } => {
                assert_eq!(from, "main");
                assert!(created, "first isolation creates the branch");
            }
            BranchIsolation::Skipped(_) => panic!("expected isolation"),
        }
        // We are NOW on the isolation branch, never on `main`.
        assert_eq!(git_current_branch(root), "umadev/my-app");
        // `main` still exists and still points at the original seed commit — the
        // user's default branch was NOT mutated, merged into, or deleted.
        assert!(git_branch_exists(root, "main"), "default branch untouched");
    }

    #[test]
    fn isolation_is_idempotent_when_already_on_umadev_branch() {
        if !git_available() {
            return;
        }
        let tmp = init_repo("main");
        let root = tmp.path();
        let first = ensure_isolation_branch(root, "task");
        assert!(matches!(
            first,
            BranchIsolation::Isolated { created: true, .. }
        ));
        // A second block of the same run is already on umadev/task → stay put,
        // created:false (never derive a branch off a branch).
        let second = ensure_isolation_branch(root, "task");
        assert_eq!(
            second,
            BranchIsolation::Isolated {
                branch: "umadev/task".to_string(),
                from: "umadev/task".to_string(),
                created: false,
            }
        );
    }

    #[test]
    fn isolation_reuses_existing_branch_without_recreating() {
        if !git_available() {
            return;
        }
        let tmp = init_repo("main");
        let root = tmp.path();
        // First run creates umadev/x and we hop back to main to simulate a later run.
        let _ = ensure_isolation_branch(root, "x");
        run_git(root, &["switch", "main"]).unwrap();
        assert_eq!(git_current_branch(root), "main");
        // Second run with the same slug must SWITCH to the existing branch, not
        // recreate it (created:false), and never force.
        let again = ensure_isolation_branch(root, "x");
        assert_eq!(
            again,
            BranchIsolation::Isolated {
                branch: "umadev/x".to_string(),
                from: "main".to_string(),
                created: false,
            }
        );
        assert_eq!(git_current_branch(root), "umadev/x");
    }

    #[test]
    fn isolation_does_not_reuse_a_stale_branch_that_diverged_from_head() {
        // A same-name `umadev/<slug>` branch left by a PRIOR run that no longer
        // descends from the current HEAD (the user advanced their branch since)
        // must NOT be switched to — that would base new work on stale history.
        // Instead a fresh, uniquely-suffixed sibling is derived from the real HEAD.
        if !git_available() {
            return;
        }
        let tmp = init_repo("main");
        let root = tmp.path();
        let run = |args: &[&str]| {
            run_git(root, args).unwrap();
        };
        // First run makes umadev/app from the seed commit.
        let first = ensure_isolation_branch(root, "app");
        assert!(matches!(
            first,
            BranchIsolation::Isolated { created: true, .. }
        ));
        // Back to main and ADVANCE it with a new commit, so umadev/app no longer
        // contains main's latest history → it is now STALE.
        run(&["switch", "main"]);
        fs::write(root.join("advance.txt"), "new main work").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "advance main"]);

        // A new run with the same slug must create a FRESH unique branch off the
        // advanced HEAD, never switch to the stale umadev/app.
        let again = ensure_isolation_branch(root, "app");
        match &again {
            BranchIsolation::Isolated {
                branch,
                from,
                created,
            } => {
                assert_ne!(branch, "umadev/app", "must not reuse the stale branch");
                assert!(
                    branch.starts_with("umadev/app-"),
                    "a fresh uniquely-suffixed sibling is derived: {branch}"
                );
                assert!(created, "the fresh branch was created this call");
                assert_eq!(from, "main");
            }
            BranchIsolation::Skipped(r) => panic!("expected a fresh isolation branch, got {r}"),
        }
        // The fresh branch descends from the advanced main (contains its new commit).
        assert_eq!(git_current_branch(root), again.branch());
        assert!(
            branch_descends_from_head(root, "main"),
            "the fresh branch was cut from the CURRENT HEAD, not stale history"
        );
    }

    #[test]
    fn isolation_reuses_a_branch_that_still_descends_from_head() {
        // The safe-reuse path: a same-name branch that is at (or ahead of, but
        // still contains) the current HEAD is reused as-is — no stale-history risk.
        if !git_available() {
            return;
        }
        let tmp = init_repo("main");
        let root = tmp.path();
        let run = |args: &[&str]| {
            run_git(root, args).unwrap();
        };
        let _ = ensure_isolation_branch(root, "x");
        run(&["switch", "main"]); // main and umadev/x are at the same commit here
        let again = ensure_isolation_branch(root, "x");
        assert_eq!(
            again,
            BranchIsolation::Isolated {
                branch: "umadev/x".to_string(),
                from: "main".to_string(),
                created: false,
            },
            "an up-to-date same-name branch is reused, not duplicated"
        );
    }

    #[test]
    fn isolation_skips_dirty_tree_to_protect_uncommitted_work() {
        if !git_available() {
            return;
        }
        let tmp = init_repo("main");
        let root = tmp.path();
        // Uncommitted edit on main → we must NOT risk carrying/losing it across a
        // branch switch. Conservative: skip, run in place.
        fs::write(root.join("seed.txt"), "uncommitted change").unwrap();
        let iso = ensure_isolation_branch(root, "app");
        assert_eq!(iso, BranchIsolation::Skipped("dirty-tree"));
        // Still on main, edit intact — nothing disturbed.
        assert_eq!(git_current_branch(root), "main");
        assert_eq!(
            fs::read_to_string(root.join("seed.txt")).unwrap(),
            "uncommitted change"
        );
    }

    #[test]
    fn is_isolation_branch_recognises_prefix() {
        assert!(is_isolation_branch("umadev/my-app"));
        assert!(!is_isolation_branch("main"));
        assert!(!is_isolation_branch("feature/umadev-thing"));
    }
}
