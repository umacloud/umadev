//! Scope-creep detection — the DUAL of requirement coverage.
//!
//! [`crate::coverage`] answers one half of "does the delivery match the plan?": *which
//! declared requirement has no task?* That catches UNDER-building — work that was
//! promised and quietly dropped.
//!
//! This module answers the other half: *which CHANGE belongs to no step?* That catches
//! OVER-building — work that was never planned, never sized, never reviewed, and that
//! nobody asked for. It is the more insidious direction, because over-building looks
//! like productivity: an unplanned dependency pulled in to save ten minutes, an
//! unplanned source file that becomes a second way to do the same thing, an unplanned
//! public route that becomes an unowned piece of attack surface. None of those show up
//! as a failing test. They show up months later.
//!
//! The check is a set difference, and it only exists because the plan is a machine-
//! readable artifact UmaDev owns: every step declares its file surface
//! ([`crate::plan_state::StepFiles`]), so the UNION of those surfaces is the set of
//! places the run said it would write. Diff that against the set of places it actually
//! wrote (the shadow-repo run diff — the same changed-file source the rest of the
//! deterministic floor reads) and what remains is, by construction, unclaimed.
//!
//! ## Severity follows blast radius, not line count
//! - **BLOCKING** — an unclaimed change that creates a NEW SURFACE, i.e. something that
//!   did not exist before and that the rest of the system (or the outside world) can
//!   now depend on:
//!   - a new **source file** (a new place for logic to live),
//!   - a new **dependency** in a MANIFEST — the file that carries the decision (new
//!     code you did not write, and a new supply chain). Never a lockfile: that is the
//!     resolver's transitive output, so one planned install would read as a hundred
//!     unclaimed dependencies (using the internal lockfile list),
//!   - a new **public route** (new attack surface, new contract).
//! - **BLOCKING** — an unclaimed EDIT to an existing file as well. A requested SEO
//!   change that quietly edits auth, build, or unrelated UI code is still out of
//!   scope even though it created no new file. Execution contracts make incidental
//!   wiring explicit in the responsible step instead of treating drift as harmless.
//!
//! ## Fail-open (the repo's hard rule)
//! A missing run diff/baseline remains fail-open because there is no evidence to
//! judge. A missing `files` declaration is different: it is a known malformed
//! execution contract, reported as a blocking preflight finding before mutation.

use std::collections::BTreeSet;
use std::path::Path;

use crate::bounded_fs::{read_utf8_beneath, Utf8ReadBudget};
use crate::plan_state::{Plan, StepKind};

/// One scope finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeFinding {
    /// `true` ⇒ folds into the deterministic floor's blocking list (a new unclaimed
    /// SURFACE: file / dependency / route). `false` ⇒ advisory note only.
    pub blocking: bool,
    /// Evidence-bearing message, self-prefixed `scope:`, suitable for folding straight
    /// into a rework directive.
    pub message: String,
    /// Workspace-relative, `/`-separated path the finding is about.
    pub file: String,
}

/// Source extensions that make an unclaimed NEW file a new place for LOGIC to live.
/// Deliberately narrower than the acceptance floor's list: a stray `.md` or an asset is
/// not a new code surface, and blocking on one would be crying wolf.
const CODE_EXT: &[&str] = &[
    "tsx", "jsx", "ts", "js", "mjs", "cjs", "vue", "svelte", "astro", "py", "rs", "go", "java",
    "rb", "php", "cs", "kt", "ex", "exs", "dart", "swift", "scala", "c", "cc", "cpp", "h", "hpp",
];

/// Dependency **manifests** — the files a HUMAN (or the base, on the team's behalf)
/// edits to say "this project now depends on X", by file NAME. A change here can pull
/// in code the team never wrote and never reviewed, so an unclaimed dependency ADDITION
/// is the highest-leverage thing this check can catch.
///
/// LOCKFILES ARE DELIBERATELY ABSENT (see [`LOCKFILES`]). Only the manifest that DROVE
/// a change may block on it.
const MANIFESTS: &[&str] = &[
    "package.json",
    "cargo.toml",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "gemfile",
    "composer.json",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "deno.json",
    "deno.jsonc",
];

/// Lockfiles — the RESOLVER'S OUTPUT, not a decision anyone made.
///
/// A lockfile can never be the blocking evidence for an unclaimed dependency, and the
/// reason is arithmetic: adding ONE direct dependency writes its ENTIRE TRANSITIVE
/// CLOSURE into the lockfile. `npm install express` moves ~60 names into
/// `package-lock.json`; one `go get` writes two `go.sum` rows for every module in the
/// graph. Reading those as "dependencies nobody claimed" turns a single, ordinary,
/// PLANNED install into a wall of blocking findings about packages the team never chose
/// and cannot remove — a build that cannot converge, over a file whose whole job is to
/// be regenerated. The manifest is where the intent lives, so the manifest is where the
/// finding belongs; the lockfile that followed it is at most an advisory edit.
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "cargo.lock",
    "poetry.lock",
    "uv.lock",
    "pdm.lock",
    "go.sum",
    "gemfile.lock",
    "composer.lock",
    "deno.lock",
];

/// Directory prefixes whose contents are never "the team's source" — UmaDev's own
/// artifacts, the doc blackboard, dependency trees, and build output. A change under
/// one of these is not scope creep by anyone's definition.
const IGNORED_PREFIXES: &[&str] = &[".umadev/", ".git/", "output/", "release/"];

/// Directory NAMES that are never the team's source, AT ANY DEPTH — dependency trees,
/// build output, caches, coverage reports.
///
/// SEGMENT-matched, not root-prefix-matched, and that distinction is the whole point:
/// the mainstream JS layout is a monorepo, so the build output is at
/// `apps/web/.next/static/chunks/main.js`, NOT at `.next/…`. A root-anchored prefix
/// test reads that file as a brand-new source file the team wrote and never planned —
/// a BLOCKING finding over a compiler artifact. If a name belongs here it belongs here
/// at every depth.
const IGNORED_DIR_SEGMENTS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".output",
    ".turbo",
    ".svelte-kit",
    ".venv",
    "venv",
    "coverage",
    "__pycache__",
    ".pytest_cache",
    ".ruff_cache",
    ".mypy_cache",
    ".hypothesis",
    // Base-owned local state/config is context for the worker, not product source.
    // It can change independently of the current requirement and may contain local
    // credentials or permission state, so it must never become a scope-repair target.
    ".claude",
    ".codex",
    ".grok",
    ".kimi",
    ".kimi-code",
    ".git",
    ".umadev",
];

/// Local machine/framework files that are never a product deliverable. Match by
/// exact basename at any depth; `.env.example` and real source/config remain visible.
const IGNORED_FILE_NAMES: &[&str] = &[".ds_store", ".coverage", ".env", ".umadevrc", "umadev.yaml"];

/// Cap on reported findings — a run that went catastrophically off-plan produces a
/// readable directive, not a wall of text.
const MAX_FINDINGS: usize = 20;
/// One hand-written source/manifest is enough evidence for this check; generated
/// bundles and oversized manifests are deliberately skipped instead of read whole.
const MAX_SCOPE_INPUT_BYTES: usize = 600_000;
/// Route extraction shares one aggregate allowance across the changed set so a
/// large monorepo cannot turn the scope floor into an unbounded read pass.
const MAX_ROUTE_INPUT_TOTAL_BYTES: usize = 12 * 1024 * 1024;

/// The set of changes this run made that belong to NO plan step.
///
/// Returns contract findings; current execution-scope violations are blocking.
/// Empty when the run diff is unreadable/no baseline exists, a review-only plan has
/// no writer, or every changed path was claimed.
///
/// Bounded: one shadow-repo diff, one backend-route extraction over the changed set,
/// and at most one before/after manifest comparison per changed manifest.
#[must_use]
pub fn unclaimed_changes(root: &Path, plan: &Plan) -> Vec<ScopeFinding> {
    // EXECUTION-CONTRACT GATE: every mutating step contributes to the denominator.
    // Missing declarations are not an IO/parser surprise; they are an explicit plan
    // defect. Report it instead of switching the whole floor off.
    let missing: Vec<&str> = plan
        .steps
        .iter()
        .filter(|step| step.kind == StepKind::Build && step.files.is_empty())
        .map(|step| step.id.as_str())
        .collect();
    if !missing.is_empty() {
        return vec![ScopeFinding {
            blocking: true,
            message: format!(
                "scope: execution contract incomplete — build step(s) [{}] declared no file surface; re-plan them with explicit files.create/files.modify before writing",
                missing.join(", ")
            ),
            file: String::new(),
        }];
    }

    let claims: Vec<&str> = plan
        .steps
        .iter()
        .flat_map(|s| s.files.all())
        .filter(|p| !p.trim().is_empty())
        .collect();
    if claims.is_empty() {
        // A review-only plan has no writer and therefore no source scope to check.
        return Vec::new();
    }

    // FAIL-OPEN GATE #2: the run diff.
    //
    // `Unavailable` (git missing / no run baseline / unreadable diff) is silent — we
    // simply could not look, and an unknown is never a finding.
    //
    // `TooLarge` is DIFFERENT, and it must not be silent. A monorepo run that touches
    // more files than the diff can analyze gets NO scope enforcement — and the old code
    // returned an empty vec, which reads here as "nothing changed", so the floor stood
    // down with no warn: the user never learned that a floor they were relying on had
    // gone dark. This known analysis-limit condition blocks completion until the run
    // is split; it is not an unexpected IO error and must not masquerade as clean.
    let changed = match crate::checkpoint::run_diff_since_baseline(root) {
        crate::checkpoint::RunDiff::Changed(files) => files,
        crate::checkpoint::RunDiff::Unavailable => return Vec::new(),
        crate::checkpoint::RunDiff::TooLarge(count) => {
            return vec![ScopeFinding {
                blocking: true,
                message: format!(
                    "scope: the scope floor stood down for this run — {count}+ changed files \
                     exceed the {cap}-file analysis cap, so no unclaimed file / dependency / \
                     route can be established. An unverifiable scope cannot be called \
                     complete. Split/re-plan the run (fewer steps or a smaller workspace \
                     root) to restore enforcement.",
                    cap = crate::checkpoint::MAX_CHANGED_FILES,
                ),
                file: String::new(),
            }];
        }
    };
    if changed.is_empty() {
        return Vec::new();
    }
    let baseline_id = crate::checkpoint::run_baseline(root).map(|c| c.id);
    // The unclaimed set: everything that changed, minus everything some step claimed,
    // minus the paths that are not the team's source in the first place.
    let unclaimed: Vec<&crate::checkpoint::ChangedFile> = changed
        .iter()
        .filter(|c| !is_ignored(&c.path))
        .filter(|c| !claims.iter().any(|claim| claim_covers(claim, &c.path)))
        .collect();
    if unclaimed.is_empty() {
        return Vec::new();
    }

    // A route registered in an unclaimed file is a new PUBLIC surface regardless of
    // whether the file itself is new — reuse the contract crate's own extractor rather
    // than inventing a second, divergent notion of "a route". (Its `file` is already
    // workspace-relative and `/`-separated, matching the run diff's paths.)
    let routes = routes_in_unclaimed_files(root, &unclaimed);

    let mut out: Vec<ScopeFinding> = Vec::new();
    for c in &unclaimed {
        if out.len() >= MAX_FINDINGS {
            break;
        }
        let path = c.path.as_str();

        // 1. A new DEPENDENCY — the highest blast radius: code nobody on the team
        //    wrote, entering the build because it was convenient.
        if is_manifest(path) {
            let added = added_dependencies(root, baseline_id.as_deref(), path, c.added);
            if !added.is_empty() {
                out.push(ScopeFinding {
                    blocking: true,
                    message: format!(
                        "scope: `{path}` gained dependency/dependencies ({}) that NO plan step \
                         claimed — a dependency is code the team did not write and did not \
                         review. Either put it in a step's declared surface (and say why the \
                         build needs it), or remove it and solve the problem in code you own",
                        added.join(", ")
                    ),
                    file: path.to_string(),
                });
                continue;
            }
        }

        // 2. A new PUBLIC ROUTE in a file no step claimed — new attack surface and a
        //    new contract, owned by nobody.
        let route_here: Vec<String> = routes
            .iter()
            .filter(|r| r.file == path)
            .map(|r| {
                let m = r.method.map_or("ANY", umadev_contract::HttpVerb::as_str);
                format!("{m} {}", r.path)
            })
            .take(4)
            .collect();
        if !route_here.is_empty() {
            out.push(ScopeFinding {
                blocking: true,
                message: format!(
                    "scope: `{path}` registers public route(s) ({}) that NO plan step claimed \
                     — an unplanned route is unowned public surface (nothing sized it, the API \
                     contract does not describe it, no reviewer looked at its authorisation). \
                     Put it in a step's declared surface and the architecture contract, or drop it",
                    route_here.join(", ")
                ),
                file: path.to_string(),
            });
            continue;
        }

        // 3. A new SOURCE FILE nobody claimed — a new place for logic to live.
        if c.added && is_code_file(path) {
            out.push(ScopeFinding {
                blocking: true,
                message: format!(
                    "scope: new source file `{path}` belongs to NO plan step — the run built \
                     something it was not asked for. Claim it in the step that needs it (and \
                     take the review that comes with it), or delete it"
                ),
                file: path.to_string(),
            });
            continue;
        }

        // 4. Everything else: an unclaimed EDIT to a file that already existed.
        //    It is still work nobody authorised. Incidental wiring belongs in the
        //    responsible step's declared surface; otherwise the delivery blocks.
        out.push(ScopeFinding {
            blocking: true,
            message: format!(
                "scope: `{path}` was edited but no plan step declared it — remove the change or \
                 explicitly re-plan it as required wiring before continuing"
            ),
            file: path.to_string(),
        });
    }
    out
}

/// Whether a declared claim covers a changed path. A claim is either an exact
/// (normalised) path or a DIRECTORY prefix — `src/api/`, or a bare `src/api` that the
/// changed path sits under — so a step can claim a subtree without enumerating it.
///
/// Compared CASE-INSENSITIVELY: macOS and Windows both ship case-insensitive
/// filesystems by default, so a step that claimed `src/Api/` and a diff that reports
/// `src/api/login.ts` are the SAME PLACE — and a case-sensitive test would call the
/// team's own planned file unclaimed and block on it.
fn claim_covers(claim: &str, path: &str) -> bool {
    let claim = claim
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_ascii_lowercase();
    if claim.is_empty() {
        return false;
    }
    let path = path.to_ascii_lowercase();
    let dir = claim.trim_end_matches('/');
    path == dir || path.starts_with(&format!("{dir}/"))
}

/// Whether a changed path is outside the team's source surface entirely (UmaDev's own
/// artifacts, generated lockfiles, the doc blackboard, vendored/build/cache trees) —
/// the ignored DIRECTORY NAMES matched at ANY depth (see [`IGNORED_DIR_SEGMENTS`]).
fn is_ignored(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    LOCKFILES.contains(&name)
        || IGNORED_FILE_NAMES.contains(&name)
        || IGNORED_PREFIXES.iter().any(|p| lower.starts_with(p))
        || lower
            .split('/')
            .any(|seg| IGNORED_DIR_SEGMENTS.contains(&seg))
}

/// Whether the path names a dependency MANIFEST — the file that carries the decision.
/// A lockfile is not one (see [`LOCKFILES`]): it is the resolver's transitive output,
/// and blocking on it would block a planned install for the closure it dragged in.
fn is_manifest(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    MANIFESTS.contains(&name.as_str()) && !LOCKFILES.contains(&name.as_str())
}

/// Whether the path is a code file (a new one is a new place for logic to live).
fn is_code_file(path: &str) -> bool {
    let ext = path
        .rsplit('.')
        .next()
        .filter(|e| *e != path)
        .unwrap_or("")
        .to_ascii_lowercase();
    CODE_EXT.contains(&ext.as_str())
}

/// The dependency names a manifest GAINED since the run baseline.
///
/// Compares the manifest's dependency-name set at the baseline against its set now.
/// A brand-new manifest (`added`) contributes ALL of its dependencies — the whole file
/// arrived unplanned. Deliberately NOT a line diff: a version bump, a reformat, or a
/// script edit is not a new dependency, and blocking on those would make the check
/// useless noise.
///
/// Fail-open: an unreadable manifest (either version) yields an EMPTY set — no finding.
/// Bounded: at most [`MAX_NEW_DEPS_REPORTED`] names are reported.
fn added_dependencies(
    root: &Path,
    baseline_id: Option<&str>,
    rel: &str,
    added_file: bool,
) -> Vec<String> {
    let Ok(now_text) = read_utf8_beneath(root, Path::new(rel), MAX_SCOPE_INPUT_BYTES) else {
        return Vec::new();
    };
    let now = dependency_names(rel, &now_text);
    if now.is_empty() {
        return Vec::new();
    }
    let before = if added_file {
        BTreeSet::new() // the manifest itself is new → every dep in it is new
    } else {
        let Some(id) = baseline_id else {
            return Vec::new(); // cannot see the past → say nothing
        };
        let crate::checkpoint::FileAt::Content(before_text) =
            crate::checkpoint::file_at(root, id, rel)
        else {
            return Vec::new();
        };
        dependency_names(rel, &before_text)
    };
    now.difference(&before)
        .take(MAX_NEW_DEPS_REPORTED)
        .cloned()
        .collect()
}

/// Extract routes only from the already-known unclaimed changed files. Besides
/// avoiding a second full-workspace walk, the shared budget and no-follow read
/// boundary keep a hostile FIFO/symlink/large generated file from hanging or
/// exhausting the governance pass.
fn routes_in_unclaimed_files(
    root: &Path,
    unclaimed: &[&crate::checkpoint::ChangedFile],
) -> Vec<umadev_contract::BackendRoute> {
    let mut budget = Utf8ReadBudget::new(MAX_ROUTE_INPUT_TOTAL_BYTES, MAX_SCOPE_INPUT_BYTES);
    let mut routes = Vec::new();
    for changed in unclaimed {
        if budget.remaining_bytes() == 0 {
            break;
        }
        let path = changed.path.as_str();
        if !is_code_file(path) {
            continue;
        }
        let Ok(content) = budget.read_utf8_beneath(root, Path::new(path)) else {
            continue;
        };
        routes.extend(umadev_contract::extract_backend_routes_from_content(
            path, &content,
        ));
    }
    routes
}

/// Cap on how many new dependency names one finding names.
const MAX_NEW_DEPS_REPORTED: usize = 6;

/// The dependency NAMES declared in a manifest — a deliberately shallow, format-aware
/// scan (no ecosystem parsers pulled into a dependency-light crate).
///
/// Recognises the shapes that actually carry dependency names across the stacks UmaDev
/// builds for; anything it cannot read yields an empty set, which (per the caller's
/// fail-open contract) produces no finding rather than a wrong one.
fn dependency_names(rel: &str, text: &str) -> BTreeSet<String> {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_ascii_lowercase();
    let mut out = BTreeSet::new();
    // A LOCKFILE NEVER YIELDS A NAME. Belt-and-braces against the caller: a lockfile
    // holds the whole transitive closure, so harvesting names from one turns a single
    // planned `npm install` / `go get` into a wall of blocking findings about packages
    // nobody chose. `is_manifest` already refuses to route one here; this makes it
    // impossible to reintroduce by adding a match arm.
    if LOCKFILES.contains(&name.as_str()) {
        return out;
    }
    match name.as_str() {
        // JSON manifests — matched by EXACT file name (never "any JSON with a
        // `dependencies` key": an npm v2 lockfile has a top-level `dependencies`
        // object too, and it lists the closure).
        "package.json" | "composer.json" | "deno.json" | "deno.jsonc" => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
                return out;
            };
            let Some(obj) = v.as_object() else {
                return out;
            };
            for (k, val) in obj {
                let k = k.to_ascii_lowercase();
                let is_deps = k.ends_with("dependencies") || k == "imports";
                if !is_deps {
                    continue;
                }
                if let Some(deps) = val.as_object() {
                    out.extend(deps.keys().cloned());
                }
            }
        }
        // Lockfiles + TOML/text manifests: a line-shaped scan. A `[dependencies]`-style
        // TOML table lists `name = …`; a requirements/go.mod line leads with the name.
        "cargo.toml" | "pyproject.toml" => {
            let mut in_deps = false;
            for line in text.lines() {
                let l = line.trim();
                if l.starts_with('[') {
                    let lower = l.to_ascii_lowercase();
                    in_deps = lower.contains("dependencies");
                    continue;
                }
                if !in_deps || l.is_empty() || l.starts_with('#') {
                    continue;
                }
                if let Some((k, _)) = l.split_once('=') {
                    let k = k.trim().trim_matches('"').trim();
                    if !k.is_empty() {
                        out.insert(k.to_string());
                    }
                }
            }
        }
        "requirements.txt" => {
            for line in text.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with('#') || l.starts_with('-') {
                    continue;
                }
                let end = l
                    .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
                    .unwrap_or(l.len());
                let name = &l[..end];
                if !name.is_empty() {
                    out.insert(name.to_ascii_lowercase());
                }
            }
        }
        // `go.mod` ONLY — never `go.sum`, which carries a checksum row for every module
        // in the transitive graph (two per module), so one `go get` would read as
        // hundreds of unclaimed dependencies.
        "go.mod" => {
            for line in text.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with("//") {
                    continue;
                }
                // `require x/y v1` / a `require (` block body: the module path is the
                // first token that looks like a path.
                let tok = l.strip_prefix("require ").unwrap_or(l).trim();
                if let Some(first) = tok.split_whitespace().next() {
                    if first.contains('/') && !first.starts_with('(') {
                        out.insert(first.to_string());
                    }
                }
            }
        }
        // Anything else: we do not claim to parse it. Empty ⇒ no finding (fail-open).
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::critics::Seat;
    use crate::plan_state::{AcceptanceSpec, PlanStep, StepFiles, StepStatus};

    /// A workspace with a shadow-repo RUN BASELINE already taken — the state every
    /// mutating run starts from, and the state the scope check diffs against.
    fn baselined_workspace() -> Option<tempfile::TempDir> {
        if !std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return None; // fail-open environment without git — nothing to assert
        }
        let tmp = tempfile::TempDir::new().unwrap();
        write(tmp.path(), "src/existing.ts", "export const a = 1;\n");
        write(
            tmp.path(),
            "package.json",
            r#"{"dependencies":{"react":"18"}}"#,
        );
        crate::checkpoint::create_run_baseline(tmp.path(), "demo")?;
        Some(tmp)
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// A one-step plan whose surface claims `claim` (empty ⇒ declares nothing).
    fn plan_claiming(claim: &[&str]) -> Plan {
        Plan {
            steps: vec![PlanStep {
                id: "impl".into(),
                title: "build it".into(),
                seat: Seat::BackendEngineer,
                kind: StepKind::Build,
                depends_on: vec![],
                acceptance: AcceptanceSpec::SourcePresent,
                evidence: vec![],
                files: StepFiles {
                    create: claim.iter().map(|s| (*s).to_string()).collect(),
                    modify: vec![],
                },
                status: StepStatus::Pending,
            }],
            risks: vec![],
            open_questions: vec![],
        }
    }

    #[test]
    fn missing_surface_is_an_explicit_contract_failure() {
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        write(tmp.path(), "src/rogue.ts", "export const rogue = 1;\n");
        write(
            tmp.path(),
            "package.json",
            r#"{"dependencies":{"react":"18","left-pad":"1"}}"#,
        );
        let plan = plan_claiming(&[]);
        let findings = unclaimed_changes(tmp.path(), &plan);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].blocking);
        assert!(findings[0].message.contains("contract incomplete"));
    }

    #[test]
    fn a_diff_too_large_to_analyze_says_the_floor_stood_down_instead_of_going_dark() {
        // N2: past the analysis cap the diff is discarded — correctly, since a PARTIAL
        // view would misread every unlisted file as "unchanged". But the old code
        // returned an empty vec, which reads here as "nothing changed": a monorepo
        // touching 2500 files got ZERO scope enforcement, silently, and never learned
        // why. A known analysis cap must fail completion until the work is split.
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        let root = tmp.path();
        for i in 0..=crate::checkpoint::MAX_CHANGED_FILES {
            write(root, &format!("src/gen/f{i}.ts"), "export const x = 1;\n");
        }
        let findings = unclaimed_changes(root, &plan_claiming(&["src/planned.ts"]));
        assert_eq!(
            findings.len(),
            1,
            "exactly one stand-down failure: {findings:?}"
        );
        assert!(
            findings[0].blocking,
            "known unverifiable scope cannot be called complete"
        );
        assert!(
            findings[0].message.contains("stood down"),
            "the message says the floor went dark: {}",
            findings[0].message
        );
        assert!(
            findings[0]
                .message
                .contains(&crate::checkpoint::MAX_CHANGED_FILES.to_string()),
            "…and names the cap that caused it: {}",
            findings[0].message
        );
    }

    #[test]
    fn blocks_an_unclaimed_new_source_file() {
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        // The plan claimed src/planned.ts; the run ALSO wrote src/rogue.ts.
        write(tmp.path(), "src/planned.ts", "export const planned = 1;\n");
        write(tmp.path(), "src/rogue.ts", "export const rogue = 1;\n");
        let f = unclaimed_changes(tmp.path(), &plan_claiming(&["src/planned.ts"]));
        let blocking: Vec<_> = f.iter().filter(|x| x.blocking).collect();
        assert_eq!(blocking.len(), 1, "{f:?}");
        assert_eq!(blocking[0].file, "src/rogue.ts");
        assert!(blocking[0].message.contains("new source file"), "{f:?}");
        // The CLAIMED new file is not a finding at all.
        assert!(
            !f.iter().any(|x| x.file == "src/planned.ts"),
            "a claimed file is in scope: {f:?}"
        );
    }

    #[test]
    fn blocks_an_unclaimed_manifest_edit_and_identifies_new_dependencies() {
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        // A VERSION BUMP is not mislabelled as a newly-added dependency, but the
        // manifest edit is still outside a plan that only claimed source code.
        write(
            tmp.path(),
            "package.json",
            r#"{"dependencies":{"react":"19"}}"#,
        );
        let bumped = unclaimed_changes(tmp.path(), &plan_claiming(&["src/planned.ts"]));
        assert_eq!(bumped.len(), 1, "one generic scope finding: {bumped:?}");
        assert!(bumped[0].blocking);
        assert!(!bumped[0].message.contains("gained dependency"));

        // A genuinely NEW dependency nobody claimed blocks.
        write(
            tmp.path(),
            "package.json",
            r#"{"dependencies":{"react":"19","left-pad":"1.3.0"}}"#,
        );
        let f = unclaimed_changes(tmp.path(), &plan_claiming(&["src/planned.ts"]));
        let blocking: Vec<_> = f.iter().filter(|x| x.blocking).collect();
        assert_eq!(blocking.len(), 1, "{f:?}");
        assert!(blocking[0].message.contains("left-pad"), "{f:?}");
        assert!(blocking[0].message.contains("dependency"), "{f:?}");

        // …and it is NOT a finding once a step claims the manifest.
        let claimed = unclaimed_changes(tmp.path(), &plan_claiming(&["package.json"]));
        assert!(
            !claimed.iter().any(|x| x.blocking),
            "a claimed manifest is in scope: {claimed:?}"
        );
    }

    #[test]
    fn blocks_an_unclaimed_public_route() {
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        // The route is added to a file that ALREADY existed — so the "new file" rule
        // cannot catch it. A public route is a surface regardless of file age.
        write(
            tmp.path(),
            "src/existing.ts",
            "export const a = 1;\napp.post('/api/admin/purge', purge);\n",
        );
        let f = unclaimed_changes(tmp.path(), &plan_claiming(&["src/planned.ts"]));
        let blocking: Vec<_> = f.iter().filter(|x| x.blocking).collect();
        assert_eq!(blocking.len(), 1, "{f:?}");
        assert_eq!(blocking[0].file, "src/existing.ts");
        assert!(blocking[0].message.contains("/api/admin/purge"), "{f:?}");
        assert!(blocking[0].message.contains("route"), "{f:?}");
    }

    #[test]
    fn an_unclaimed_line_edit_blocks_and_a_claimed_one_is_silent() {
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        // An ordinary edit (no new file, no dep, no route) to an unclaimed file.
        write(
            tmp.path(),
            "src/existing.ts",
            "export const a = 1;\nexport const b = 2;\n",
        );
        let f = unclaimed_changes(tmp.path(), &plan_claiming(&["src/planned.ts"]));
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].blocking, "an unclaimed edit is outside the contract");
        assert!(f[0].message.contains("edited"), "{f:?}");

        // The same edit, CLAIMED by the step's surface → silent.
        let claimed = unclaimed_changes(tmp.path(), &plan_claiming(&["src/"]));
        assert!(
            claimed.is_empty(),
            "a directory claim covers its subtree: {claimed:?}"
        );
    }

    #[test]
    fn claim_covers_exact_paths_and_directory_prefixes_only() {
        assert!(claim_covers("src/api/login.ts", "src/api/login.ts"));
        assert!(claim_covers("src/api/", "src/api/login.ts"));
        assert!(claim_covers("src/api", "src/api/deep/login.ts"));
        assert!(claim_covers("./src/api", "src/api/login.ts"));
        // A prefix that is not a DIRECTORY boundary must not match — `src/api` does
        // not claim `src/apikeys.ts`.
        assert!(!claim_covers("src/api", "src/apikeys.ts"));
        assert!(!claim_covers("src/api", "src/other.ts"));
        assert!(!claim_covers("", "src/a.ts"));
    }

    #[test]
    fn dependency_names_reads_the_common_manifest_shapes() {
        let pkg = dependency_names(
            "package.json",
            r#"{"dependencies":{"react":"18"},"devDependencies":{"vitest":"1"},"scripts":{"test":"vitest"}}"#,
        );
        assert!(pkg.contains("react") && pkg.contains("vitest"), "{pkg:?}");
        assert!(
            !pkg.contains("test"),
            "scripts are not dependencies: {pkg:?}"
        );

        let cargo = dependency_names(
            "Cargo.toml",
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n",
        );
        assert!(
            cargo.contains("serde") && cargo.contains("tempfile"),
            "{cargo:?}"
        );
        assert!(
            !cargo.contains("name"),
            "the [package] table is not deps: {cargo:?}"
        );

        let req = dependency_names(
            "requirements.txt",
            "# comment\nfastapi==0.1\nuvicorn\n-e .\n",
        );
        assert!(
            req.contains("fastapi") && req.contains("uvicorn"),
            "{req:?}"
        );

        let gomod = dependency_names(
            "go.mod",
            "module x\n\nrequire github.com/gin-gonic/gin v1.9.1\n",
        );
        assert!(gomod.contains("github.com/gin-gonic/gin"), "{gomod:?}");

        // An unparseable manifest yields NOTHING — fail-open, never a wrong finding.
        assert!(dependency_names("package.json", "{{{ not json").is_empty());
    }

    #[test]
    fn reported_regression_local_artifacts_are_never_scope_creep() {
        assert!(is_ignored(".umadev/plan.json"));
        assert!(is_ignored("output/demo-prd.md"));
        assert!(is_ignored("node_modules/react/index.js"));
        assert!(is_ignored("packages/app/node_modules/x/y.js"));
        assert!(is_ignored(".DS_Store"));
        assert!(is_ignored("services/api/.pytest_cache/v/cache/nodeids"));
        assert!(is_ignored(".ruff_cache/0.1/file"));
        assert!(is_ignored(".claude/settings.json"));
        assert!(is_ignored("apps/api/.env"));
        assert!(is_ignored(".umadevrc"));
        assert!(is_ignored("umadev.yaml"));
        assert!(!is_ignored(".env.example"));
        assert!(!is_ignored(".gitignore"));
        assert!(!is_ignored("src/api/login.ts"));
    }

    // ── BLOCKER: a NESTED build dir is not a new source file ──────────────────

    #[test]
    fn a_nested_build_artifact_is_never_read_as_a_new_source_file() {
        // The mainstream JS layout is a monorepo, so the build output lives at
        // `apps/web/.next/static/chunks/main.js` — NOT at `.next/…`. A root-anchored
        // prefix test reads that compiler artifact as a brand-new source file the team
        // wrote and nobody planned, and BLOCKS on it. If a directory name is ignored, it
        // is ignored at every depth.
        for artifact in [
            "apps/web/.next/static/chunks/main.js",
            "apps/web/.turbo/daemon/x.js",
            "packages/api/dist/index.js",
            "packages/ui/build/bundle.js",
            "services/py/.venv/lib/site.py",
            "apps/docs/.nuxt/app.js",
            "apps/edge/.output/server/index.mjs",
            "apps/web/coverage/lcov-report/block.js",
            "apps/web/node_modules/react/index.js",
            "backend/vendor/github.com/x/y.go",
            "apps/svelte/.svelte-kit/generated/root.js",
        ] {
            assert!(
                is_ignored(artifact),
                "`{artifact}` is generated/vendored output, not the team's source"
            );
        }
        // Real source at the same depth is still the team's source.
        assert!(!is_ignored("apps/web/src/app/page.tsx"));
        assert!(!is_ignored("packages/api/src/routes.ts"));
        // A file that merely CONTAINS an ignored name in a segment is not ignored.
        assert!(!is_ignored("src/build-config.ts"));
        assert!(!is_ignored("src/distance.ts"));
    }

    #[test]
    fn a_nested_build_artifact_produces_no_finding_end_to_end() {
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        write(tmp.path(), "src/planned.ts", "export const planned = 1;\n");
        write(
            tmp.path(),
            "apps/web/.next/static/chunks/main.js",
            "console.log('generated by the compiler')\n",
        );
        let f = unclaimed_changes(tmp.path(), &plan_claiming(&["src/planned.ts"]));
        assert!(
            f.is_empty(),
            "a compiler artifact is neither a blocking new surface nor a note: {f:?}"
        );
    }

    // ── HIGH: a LOCKFILE's transitive closure must not block ──────────────────

    #[test]
    fn a_lockfiles_transitive_closure_is_never_an_unclaimed_dependency() {
        // Adding ONE direct dependency writes its ENTIRE transitive closure into the
        // lockfile. Reading those as "dependencies nobody claimed" turns a single,
        // ordinary, PLANNED `npm install` into a wall of blocking findings about packages
        // the team never chose and cannot remove — a build that cannot converge, over a
        // file whose whole job is to be regenerated.
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        // The manifest change the team DID plan (and claimed).
        write(
            tmp.path(),
            "package.json",
            r#"{"dependencies":{"react":"18","express":"4"}}"#,
        );
        // What npm wrote next: an npm-lockfile-v2, whose top-level `dependencies` object
        // and `packages` map both carry the whole closure.
        write(
            tmp.path(),
            "package-lock.json",
            r#"{
  "name": "app", "lockfileVersion": 2,
  "packages": {
    "node_modules/express": {"version": "4.18.2"},
    "node_modules/body-parser": {"version": "1.20.1"},
    "node_modules/qs": {"version": "6.11.0"}
  },
  "dependencies": {
    "express": {"version": "4.18.2"},
    "body-parser": {"version": "1.20.1"},
    "qs": {"version": "6.11.0"},
    "raw-body": {"version": "2.5.1"}
  }
}"#,
        );
        let f = unclaimed_changes(tmp.path(), &plan_claiming(&["package.json"]));
        assert!(
            !f.iter().any(|x| x.blocking),
            "the lockfile's closure must not block a planned install: {f:?}"
        );
        assert!(
            !f.iter()
                .any(|x| x.message.contains("body-parser") || x.message.contains("qs")),
            "no transitive package is ever named as an unclaimed dependency: {f:?}"
        );
        // A lockfile is not a manifest, and it harvests no names — in either direction.
        assert!(!is_manifest("package-lock.json"));
        assert!(!is_manifest("pnpm-lock.yaml"));
        assert!(!is_manifest("go.sum"));
        assert!(!is_manifest("cargo.lock"));
        assert!(dependency_names("package-lock.json", "{\"dependencies\":{\"qs\":{}}}").is_empty());
    }

    #[test]
    fn go_sum_never_harvests_the_module_graph() {
        // `go.sum` carries two checksum rows for EVERY module in the transitive graph, so
        // one `go get` used to read as hundreds of unclaimed dependencies.
        let sum = "github.com/gin-gonic/gin v1.9.1 h1:aaa=\n\
                   github.com/gin-gonic/gin v1.9.1/go.mod h1:bbb=\n\
                   github.com/bytedance/sonic v1.9.1 h1:ccc=\n\
                   golang.org/x/net v0.10.0/go.mod h1:ddd=\n";
        assert!(
            dependency_names("go.sum", sum).is_empty(),
            "go.sum is the resolver's output, not a decision"
        );
        // `go.mod` — where the decision actually lives — still reads.
        let m = dependency_names(
            "go.mod",
            "module x\n\nrequire github.com/gin-gonic/gin v1.9.1\n",
        );
        assert!(m.contains("github.com/gin-gonic/gin"), "{m:?}");
    }

    #[test]
    fn a_manifest_dependency_still_blocks_when_nobody_claimed_it() {
        // The check must keep its teeth: excluding lockfiles narrows WHERE a dependency
        // finding may come from, it does not switch the rule off.
        let Some(tmp) = baselined_workspace() else {
            return;
        };
        write(
            tmp.path(),
            "package.json",
            r#"{"dependencies":{"react":"18","left-pad":"1.3.0"}}"#,
        );
        write(tmp.path(), "package-lock.json", r#"{"lockfileVersion":2}"#);
        let f = unclaimed_changes(tmp.path(), &plan_claiming(&["src/planned.ts"]));
        let blocking: Vec<_> = f.iter().filter(|x| x.blocking).collect();
        assert_eq!(blocking.len(), 1, "{f:?}");
        assert_eq!(
            blocking[0].file, "package.json",
            "the MANIFEST is the finding"
        );
        assert!(blocking[0].message.contains("left-pad"), "{f:?}");
    }

    // ── a claim is a place, not a spelling ───────────────────────────────────

    #[test]
    fn claim_covers_is_case_insensitive() {
        // macOS and Windows both ship case-insensitive filesystems by default: a step
        // that claimed `src/Api/` and a diff that reports `src/api/login.ts` are THE SAME
        // PLACE, and a case-sensitive test would call the team's own planned file
        // unclaimed and block on it.
        assert!(claim_covers("src/Api/", "src/api/login.ts"));
        assert!(claim_covers("src/api", "src/API/login.ts"));
        assert!(claim_covers("SRC/Api/Login.ts", "src/api/login.ts"));
        // It stays a DIRECTORY-boundary test, not a loose prefix.
        assert!(!claim_covers("src/api", "src/apikeys.ts"));
    }
}
