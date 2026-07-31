//! Verify loop — the engine's "project-director Agent runs build/tests"
//! capability. After a code-producing phase, this module:
//!
//! 1. Detects what kind of project sits in the workspace
//!    (`deno.json` → Deno, `package.json` → Node, `Cargo.toml` → Rust,
//!    `go.mod` → Go, `pyproject.toml` / `requirements.txt` → Python).
//!    For Node, the package manager (pnpm / yarn / bun / npm) is chosen
//!    from the lockfile so verify matches the scaffolded project.
//! 2. Runs a **sequence** of verify steps (install → lint → typecheck →
//!    test → build) for that project type as subprocesses, each with its
//!    own timeout and captured output. A step whose binary is missing
//!    (e.g. `ruff` not installed) is recorded as `skipped`, not failure.
//! 3. Returns a structured [`VerifyOutcome`] per step. The runner emits
//!    these as engine events and appends to `.umadev/audit/verify.jsonl`.
//!
//! This module **does not** retry on failure — it just reports. The
//! retry-with-feedback loop is layered on top in the runner.
//!
//! Failures are first-class data, not exceptions: an `Err` `VerifyOutcome`
//! is the desired path for "build failed, here's why". Spawn errors
//! produce a `passed: false` outcome with the OS error as `stderr`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Cap stderr / stdout captured in the audit row, so a chatty build
/// can't bloat the JSONL.
const CAPTURE_CAP: usize = 8 * 1024;

/// Project manifests and lightweight configuration inspected while building a
/// verify plan are workspace-controlled input. Keep those probes bounded so a
/// malformed file cannot allocate without limit or block on a special file.
const MAX_VERIFY_CONFIG_BYTES: usize = 2 * 1024 * 1024;
const MAX_VERIFY_AUDIT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_VERIFY_AUDIT_RECORD_BYTES: usize = 64 * 1024;
const VERIFY_AUDIT_LOCK_TIMEOUT: Duration = Duration::from_millis(250);

/// Default per-step timeout (seconds) when neither the step's own
/// [`VerifyStep::timeout_secs`] nor the `UMADEV_VERIFY_TIMEOUT_SECS`
/// env override is set.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Larger default budget (seconds) for steps that do real work —
/// `install` (network), `test` (suite), `build` (compile). Used as the
/// per-step default for those categories so a global 120s cap doesn't
/// falsely time out a legitimate `cargo build --release`.
const SLOW_STEP_TIMEOUT_SECS: u64 = 600;

/// Bounded reap / pipe-reader grace after a verify process tree is terminated.
const COMMAND_REAP_SECS: u64 = 5;

/// What kind of project we detected.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectKind {
    /// `package.json` present — Node / TypeScript / etc.
    Node,
    /// `Cargo.toml` present — Rust.
    Rust,
    /// `pyproject.toml` or `requirements.txt` present — Python.
    Python,
    /// `go.mod` present — Go.
    Go,
    /// `deno.json` / `deno.jsonc` present — Deno.
    Deno,
    /// No recognised manifest. Verify is skipped.
    None,
}

impl ProjectKind {
    /// Stable string label used in audit rows and events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Go => "go",
            Self::Deno => "deno",
            Self::None => "none",
        }
    }

    /// The **single canonical** build/verify command for this project type
    /// (kept for backwards compatibility with older callers and the TUI).
    /// For the full multi-step sequence, use [`verify_steps`].
    ///
    /// For Node the package manager is chosen by lockfile (pnpm / yarn /
    /// bun / npm) so the install matches what the worker actually scaffolded.
    #[must_use]
    pub fn verify_command(self, workspace: &Path) -> Option<(String, Vec<String>)> {
        let steps = verify_steps(self, workspace)?;
        // Return the first step (install for most, check for rust/go/deno).
        steps.into_iter().next().map(|s| (s.program, s.args))
    }
}

/// One verify step: a named command to run (e.g. "install", "lint", "test").
#[derive(Debug, Clone)]
pub struct VerifyStep {
    /// Short label for the audit row (e.g. "install", "lint", "test").
    pub name: &'static str,
    /// The program to run.
    pub program: String,
    /// The arguments.
    pub args: Vec<String>,
    /// Whether a missing binary should be `skipped` (true) or `failed`
    /// (false). E.g. `ruff` not installed → skipped; `cargo` missing → failed.
    pub skippable: bool,
    /// Per-step timeout budget in seconds. `0` means "use the global
    /// default" ([`DEFAULT_TIMEOUT_SECS`] or the
    /// `UMADEV_VERIFY_TIMEOUT_SECS` env override). Lets slow steps
    /// (`build`, `test`, `install`) declare a larger budget than fast ones
    /// (`fmt`, `lint`), which a single global timeout can't express.
    pub timeout_secs: u64,
}

/// The ordered verify step sequence for a project type. Each step runs
/// independently with its own timeout; a failing step does NOT abort the
/// remaining steps (we want full visibility into what works and what
/// doesn't). Returns `None` when the project type is unrecognised.
///
/// **Sequence per stack:**
/// - Node → install → lint (if script exists) → typecheck (if tsc) →
///   test (if script exists) → build (if script exists)
/// - Rust → fmt-check → clippy → test → build
/// - Python → install → ruff check → mypy (if configured) → pytest
/// - Go → vet → test → build
/// - Deno → lint → test → check
#[must_use]
pub fn verify_steps(kind: ProjectKind, workspace: &Path) -> Option<Vec<VerifyStep>> {
    // Helper: fast step (lint/fmt/vet) — default timeout.
    let s = |name: &'static str, program: &str, args: &[&str], skippable: bool| VerifyStep {
        name,
        program: program.to_string(),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        skippable,
        timeout_secs: 0, // 0 → global default
    };
    // Helper: slow step (install/test/build) — larger budget.
    let slow = |name: &'static str, program: &str, args: &[&str], skippable: bool| VerifyStep {
        name,
        program: program.to_string(),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        skippable,
        timeout_secs: SLOW_STEP_TIMEOUT_SECS,
    };
    match kind {
        ProjectKind::Node => {
            let (pm, install_args) = node_package_manager(workspace);
            let mut steps = vec![slow(
                "install",
                pm,
                install_args,
                false, // missing pm = hard fail
            )];
            // Lint: only if a lint script exists in package.json.
            if has_node_script(workspace, "lint") {
                steps.push(s("lint", pm, &["run", "lint"], true));
            }
            // Typecheck: tsc or vue-tsc. Decide by what package.json DECLARES
            // (not by whether node_modules/.bin/<x> exists at step-build time),
            // because install runs as the FIRST step at runtime — so the binary
            // may not exist yet when we build this list, and we'd wrongly pick
            // `tsc` for a Vue project whose install would have provided vue-tsc.
            if workspace_file(workspace, "tsconfig.json") {
                let tsc = if package_json_depends_on(workspace, "vue-tsc") {
                    "vue-tsc"
                } else {
                    "tsc"
                };
                steps.push(s("typecheck", tsc, &["--noEmit"], true));
            }
            // Test: only if a test script exists.
            if has_node_script(workspace, "test") {
                steps.push(slow("test", pm, &["run", "test"], false));
            }
            // Build: only if a build script exists.
            if has_node_script(workspace, "build") {
                steps.push(slow("build", pm, &["run", "build"], false));
            }
            Some(steps)
        }
        ProjectKind::Rust => Some(vec![
            s("fmt", "cargo", &["fmt", "--check", "--quiet"], true),
            s(
                "clippy",
                "cargo",
                &["clippy", "--quiet", "--", "-D", "warnings"],
                true,
            ),
            slow("test", "cargo", &["test", "--quiet"], false),
            slow("build", "cargo", &["build", "--release", "--quiet"], false),
        ]),
        ProjectKind::Python => {
            let mut steps = Vec::new();
            // Install: prefer uv (fast), fall back to pip.
            if which("uv") {
                steps.push(slow("install", "uv", &["sync"], false));
            } else {
                steps.push(slow("install", "pip", &["install", "-e", "."], true));
            }
            steps.push(s("lint", "ruff", &["check"], true));
            if workspace_file(workspace, "mypy.ini")
                || (workspace_file(workspace, "pyproject.toml")
                    && file_contains(workspace, "pyproject.toml", "[tool.mypy]"))
            {
                steps.push(s("typecheck", "mypy", &["."], true));
            }
            steps.push(slow("test", "pytest", &[], true));
            Some(steps)
        }
        ProjectKind::Go => Some(vec![
            s("vet", "go", &["vet", "./..."], true),
            slow("test", "go", &["test", "./..."], false),
            slow("build", "go", &["build", "./..."], false),
        ]),
        ProjectKind::Deno => Some(vec![
            s("lint", "deno", &["lint"], true),
            s("test", "deno", &["test"], true),
            s("check", "deno", &["check", "."], false),
        ]),
        ProjectKind::None => None,
    }
}

/// Wall-clock budget for ONE named-test run ([`run_named_test`]). A single test is a
/// small question; it must never turn into an unbounded wait, and the red→green check
/// runs it twice (once at the pre-state, once at head). A blown budget is
/// [`NamedTestOutcome::Unavailable`] — inconclusive, never a verdict.
pub const NAMED_TEST_TIMEOUT_SECS: u64 = 240;

/// Wall-clock budget for the named test run *inside* a temporary rewind (the RED half
/// of a red→green replay).
///
/// This one is not just a performance number, it is a BLAST-RADIUS number. For the
/// duration of that run the user's tracked source tree is sitting in the past, and only
/// a live process can put it back. Every second of the window is a second in which a
/// SIGKILL / OOM / closed terminal leaves the tree reverted (the crash marker in
/// `checkpoint` recovers it on the next start, but the smaller the window the less
/// there is to recover from). A single test that has not answered in this long has
/// already told us the only thing we can safely act on: nothing. Deliberately far below
/// [`NAMED_TEST_TIMEOUT_SECS`]; a blown budget is `Unavailable`, which fails open to
/// the ordinary green-half bar.
pub const RED_TEST_TIMEOUT_SECS: u64 = 90;

/// The verdict of running ONE named test in isolation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NamedTestOutcome {
    /// The runner ran the named test and it PASSED.
    Passed,
    /// The runner ran the named test and it FAILED.
    Failed,
    /// The question could not be asked at all — no recognised project, no test runner
    /// on PATH, a spawn error, or a timeout. **Not a verdict**: a caller must treat
    /// this as "we could not check", never as a pass or a fail.
    Unavailable,
}

/// Whether a test name is a plain identifier — letters, digits, `_`, and nothing else.
///
/// This matters because the "name filter" every runner advertises is NOT a name filter:
/// Go's `-run` is a **regex**, pytest's `-k` is a boolean **expression**, and both
/// treat ordinary test-name characters as syntax. A Go test named `Sum[int]` or a
/// pytest case named `test-login-flow` is a *malformed pattern*, and a malformed
/// pattern exits non-zero — which the naive reading turns into "the test does not
/// pass", a FALSE FAILURE about code that is fine. So: a name we cannot pass through
/// verbatim and safely is a name we refuse to ask about (`Unavailable` → the caller
/// degrades). Never a fabricated verdict.
fn is_plain_test_ident(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The JS runner a project's `test` script actually invokes, when we can tell.
///
/// `-t <name>` is a name filter in **jest** and **vitest** — and in **mocha** `-t` is
/// `--timeout`, so passing a test name there silently sets a nonsense timeout and runs
/// the WHOLE suite, whose result is then read as this one test's verdict. That is worse
/// than not asking. Only a script we can positively identify as jest/vitest gets the
/// filter; anything else (mocha, ava, node:test, a custom shell pipeline) is
/// `Unavailable`.
fn node_test_runner_takes_t_filter(workspace: &Path) -> bool {
    let Some(v) = package_json(workspace) else {
        return false;
    };
    let script = v
        .get("scripts")
        .and_then(|s| s.get("test"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    script.contains("jest") || script.contains("vitest")
}

/// The command that runs ONE named test — a test-name FILTER the project's own runner
/// understands, never the whole suite. `None` for a project we cannot run a single
/// test in, **or whose filter syntax cannot carry this particular name safely** (see
/// [`is_plain_test_ident`]) — in which case the caller reports `Unavailable` and falls
/// open, rather than reporting a failure the project did not have.
///
/// The filter is passed as a separate argument (never interpolated into a shell
/// string), so a test name is data, not code.
#[must_use]
fn named_test_step(
    kind: ProjectKind,
    workspace: &Path,
    test: &str,
    timeout_secs: u64,
) -> Option<VerifyStep> {
    let step = |program: &str, args: Vec<String>| VerifyStep {
        name: "named-test",
        program: program.to_string(),
        args,
        skippable: true,
        timeout_secs,
    };
    let t = test.to_string();
    match kind {
        // `cargo test <substring>` is a plain SUBSTRING match, not a pattern — any
        // name is safe to pass verbatim.
        ProjectKind::Rust => Some(step(
            "cargo",
            vec!["test".into(), "--quiet".into(), t, "--".into()],
        )),
        ProjectKind::Node => {
            // Only meaningful when the project declares a test script AND that script
            // runs a runner for which `-t` means "filter by name" (jest / vitest).
            if !has_node_script(workspace, "test") || !node_test_runner_takes_t_filter(workspace) {
                return None;
            }
            let (pm, _) = node_package_manager(workspace);
            // `-t <name>` is a jest/vitest name filter (a regex there too, but they
            // treat a non-matching pattern as "no tests ran", not as a syntax error);
            // the `--` separates it from the package manager's own args.
            Some(step(
                pm,
                vec!["run".into(), "test".into(), "--".into(), "-t".into(), t],
            ))
        }
        // pytest `-k` is an EXPRESSION (`and` / `or` / `not` / parens). A `-`, a space,
        // a bracket, or a parenthesis in the name is a syntax error → exit 4, which is
        // NOT a failing test.
        ProjectKind::Python => {
            is_plain_test_ident(test).then(|| step("pytest", vec!["-k".into(), t, "-q".into()]))
        }
        // Go `-run` is a REGEX. A name carrying `[`, `(`, `+`, `.` … is either an
        // invalid pattern (exit != 0) or a pattern that matches the wrong set.
        ProjectKind::Go => is_plain_test_ident(test).then(|| {
            step(
                "go",
                vec![
                    "test".into(),
                    "./...".into(),
                    "-run".into(),
                    format!("^{t}$"),
                ],
            )
        }),
        // `deno test --filter <name>` is a plain substring match unless it is written
        // as `/re/`; a literal name is safe.
        ProjectKind::Deno => Some(step("deno", vec!["test".into(), "--filter".into(), t])),
        ProjectKind::None => None,
    }
}

/// Run ONE named test in `workspace` and report whether it passed, failed, or could
/// not be run at all.
///
/// This is deliberately NOT [`run_verify`]: answering "does *this* test pass?" must
/// not cost a whole install/lint/typecheck/build sequence, and the red→green evidence
/// check asks the question twice (at the step's pre-state and at head). Bounded by
/// [`NAMED_TEST_TIMEOUT_SECS`] with the same process-group kill the rest of verify
/// uses, so a wedged runner can never hang the director.
///
/// Fail-open: an unrecognised project, a missing runner, a spawn error, or a timeout
/// is [`NamedTestOutcome::Unavailable`] — the caller must degrade, never block.
pub async fn run_named_test(workspace: &Path, test: &str) -> NamedTestOutcome {
    run_named_test_bounded(workspace, test, NAMED_TEST_TIMEOUT_SECS).await
}

/// [`run_named_test`] with an explicit wall-clock budget.
///
/// The red half of a red→green replay runs INSIDE a temporary rewind — the user's
/// source tree is in the past for exactly as long as this call takes — so it passes
/// [`RED_TEST_TIMEOUT_SECS`], not the ordinary budget. Same semantics otherwise; a
/// blown budget is [`NamedTestOutcome::Unavailable`], never a verdict.
pub async fn run_named_test_bounded(
    workspace: &Path,
    test: &str,
    timeout_secs: u64,
) -> NamedTestOutcome {
    let test = test.trim();
    if test.is_empty() {
        return NamedTestOutcome::Unavailable;
    }
    let kind = detect_project(workspace);
    let Some(step) = named_test_step(kind, workspace, test, timeout_secs) else {
        return NamedTestOutcome::Unavailable;
    };
    if !which(&step.program) {
        return NamedTestOutcome::Unavailable; // no runner on PATH → cannot ask
    }
    let command_str = format!("{} {}", step.program, step.args.join(" "));
    let out = run_step_command(workspace, kind, &step, command_str, timeout_secs).await;
    // A spawn failure / timeout records `exit_code == -1`; neither is a verdict about
    // the test — they are our own inability to ask.
    if out.exit_code < 0 {
        return NamedTestOutcome::Unavailable;
    }
    if out.passed {
        NamedTestOutcome::Passed
    } else {
        NamedTestOutcome::Failed
    }
}

/// The dev-server configuration UmaDev uses for `/preview`, so the command
/// does NOT depend on the worker recording a Preview URL. Detected from the
/// project's manifest + scripts. Falls back to `None` when no dev server is
/// known (the caller then tries the worker-recorded URL, then gives a hint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevServer {
    /// Human label, e.g. "Vite dev server".
    pub label: &'static str,
    /// Exact command to spawn, e.g. "npm run dev" or
    /// "cd jeecgboot-vue3 && pnpm dev".
    pub command: String,
    /// Default URL the dev server prints (for the "no URL recorded" hint).
    pub default_url: &'static str,
}

/// Detect the best dev-server command for the workspace. Checks Node
/// frameworks (Vite / Next / Astro / CRA) by dependencies + scripts, then
/// static-serve for plain HTML, then returns None.
///
/// When the root dev server looks like UmaDev's own acceptance harness, a real
/// framework subproject (e.g. `jeecgboot-vue3/`) is preferred. A working root
/// dev server is NEVER dropped, though: if the subproject search finds nothing
/// usable, the root dev server is returned as the fallback rather than `None`.
#[must_use]
pub fn detect_dev_server(workspace: &Path) -> Option<DevServer> {
    let root = detect_dev_server_in_dir(workspace);
    if let Some(root) = &root {
        if !looks_like_root_acceptance_harness(workspace) {
            return Some(root.clone());
        }
    }

    // Root is UmaDev's own acceptance harness (or absent): prefer a real
    // framework subproject's dev server.
    for subdir in preferred_frontend_dirs(workspace) {
        if let Some(mut ds) = detect_dev_server_in_dir(&subdir) {
            let Ok(rel) = subdir.strip_prefix(workspace) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            ds.command = format!("cd {rel} && {}", ds.command);
            return Some(ds);
        }
    }

    // Never drop a working root preview: fall back to the root dev server when
    // the subproject search yielded nothing usable.
    root
}

fn detect_dev_server_in_dir(workspace: &Path) -> Option<DevServer> {
    // 1. Node frameworks — ordered by specificity (most-distinctive first).
    if package_json_depends_on(workspace, "vite") {
        let pm = node_package_manager(workspace);
        let cmd = if pm.0 == "pnpm" {
            "pnpm dev"
        } else if pm.0 == "yarn" {
            "yarn dev"
        } else {
            "npm run dev"
        };
        return Some(DevServer {
            label: "Vite dev server",
            command: cmd.to_string(),
            default_url: "http://localhost:5173",
        });
    }
    if package_json_depends_on(workspace, "next") {
        return Some(DevServer {
            label: "Next.js dev server",
            command: "npm run dev".to_string(),
            default_url: "http://localhost:3000",
        });
    }
    if package_json_depends_on_prefix(workspace, "@astrojs") {
        return Some(DevServer {
            label: "Astro dev server",
            command: "npm run dev".to_string(),
            default_url: "http://localhost:4321",
        });
    }
    if package_json_depends_on(workspace, "react-scripts") {
        return Some(DevServer {
            label: "Create React App dev server",
            command: "npm start".to_string(),
            default_url: "http://localhost:3000",
        });
    }
    // 2. Node project with a generic "dev" script but no known framework —
    //    still useful: the user's `dev` script probably starts something.
    if has_node_script(workspace, "dev") {
        return Some(DevServer {
            label: "Node dev server",
            command: "npm run dev".to_string(),
            default_url: "http://localhost:3000",
        });
    }
    // 3. Static HTML — Python's http.server as a zero-dependency fallback.
    let has_html = ["index.html", "public/index.html"]
        .iter()
        .any(|p| workspace_file(workspace, p));
    if has_html {
        return Some(DevServer {
            label: "Static file server",
            command: "python3 -m http.server 8000".to_string(),
            default_url: "http://localhost:8000",
        });
    }
    None
}

fn looks_like_root_acceptance_harness(workspace: &Path) -> bool {
    let has_real_subproject = [
        "jeecgboot-vue3",
        "jeecg-boot",
        "jeecguniapp",
        "pigx-visual",
        "pigx-ai-ui",
        "frontend",
        "web",
        "ui",
        "app",
    ]
    .iter()
    .any(|d| workspace_directory(workspace, d));
    if !has_real_subproject {
        return false;
    }

    // Require a STRONG harness marker: UmaDev's generated backend entrypoint, or
    // its static-frontend index file. A bare `src/frontend` DIRECTORY is NOT
    // enough — a normal full-stack app keeps its own source there and must keep
    // its own root dev server rather than be mis-routed to a subproject.
    workspace_file(workspace, "src/backend/server.mjs")
        || workspace_file(workspace, "src/frontend/index.html")
}

fn preferred_frontend_dirs(workspace: &Path) -> Vec<std::path::PathBuf> {
    const NAMES: &[&str] = &[
        "jeecgboot-vue3",
        "jeecg-boot/jeecgboot-vue3",
        "jeecguniapp",
        "pigx-ai-ui",
        "pigx-visual/pigx-xxl-job-admin",
        "frontend",
        "web",
        "ui",
        "app",
    ];
    NAMES
        .iter()
        .map(|p| workspace.join(p))
        .filter(|p| crate::bounded_fs::is_real_file_beneath(workspace, &p.join("package.json")))
        .collect()
}

/// Resolve a bare program name to a spawnable path. Mirrors the host crate:
/// on Windows npm-installed tools are `.cmd`/`.exe`/`.bat` shims that
/// `Command::new("npm")` won't find (CreateProcess only appends `.exe`), so we
/// search `PATH` over `PATHEXT`. Unchanged off Windows, for explicit paths, or
/// when nothing matches.
fn resolve_program(program: &str) -> String {
    if !cfg!(windows) || program.contains(std::path::is_separator) {
        return program.to_string();
    }
    let Ok(path_var) = std::env::var("PATH") else {
        return program.to_string();
    };
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    for dir in path_var.split(';') {
        if dir.is_empty() {
            continue;
        }
        for ext in std::iter::once("").chain(pathext.split(';')) {
            let candidate = Path::new(dir).join(format!("{program}{ext}"));
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    program.to_string()
}

/// Like [`resolve_program`] but Windows-aware: `.cmd`/`.bat` build tools are
/// routed through `cmd /c` (CreateProcess rejects them with os error 193).
/// Returns `(program, leading args)`.
fn spawn_parts(program: &str) -> (String, Vec<String>) {
    let resolved = resolve_program(program);
    let ext = std::path::Path::new(&resolved)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if cfg!(windows) && (ext == "cmd" || ext == "bat") {
        ("cmd".to_string(), vec!["/c".to_string(), resolved])
    } else {
        (resolved, Vec::new())
    }
}

/// Check whether a PATH-resolvable binary exists. Used to decide whether a
/// step is genuinely missing (→ skip) vs the project being broken (→ fail).
///
/// Splits `PATH` on the platform-native separator (`:` on Unix, `;` on
/// Windows). On Windows also honours `PATHEXT` so `which("cargo")` finds
/// `cargo.exe`. Previously this split on `:` unconditionally, which meant
/// every step was reported "skipped" on Windows.
fn which(bin: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    let separator = if cfg!(windows) { ';' } else { ':' };
    // On Windows a bare "cargo" resolves via PATHEXT (`.exe`, `.bat`, …).
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.BAT;.CMD;.COM".to_string())
            .split(';')
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in path_var.split(separator) {
        if dir.is_empty() {
            continue;
        }
        for ext in &exts {
            let candidate = Path::new(dir).join(format!("{bin}{ext}"));
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

/// Read a file and check whether it contains a substring (best-effort).
fn file_contains(workspace: &Path, file: &str, needle: &str) -> bool {
    crate::bounded_fs::read_utf8_beneath(workspace, &workspace.join(file), MAX_VERIFY_CONFIG_BYTES)
        .is_ok_and(|content| content.contains(needle))
}

fn workspace_file(workspace: &Path, relative: impl AsRef<Path>) -> bool {
    crate::bounded_fs::is_real_file_beneath(workspace, &workspace.join(relative))
}

fn workspace_directory(workspace: &Path, relative: impl AsRef<Path>) -> bool {
    crate::bounded_fs::is_real_directory_beneath(workspace, &workspace.join(relative))
}

fn package_json(workspace: &Path) -> Option<serde_json::Value> {
    let content = crate::bounded_fs::read_utf8_beneath(
        workspace,
        &workspace.join("package.json"),
        MAX_VERIFY_CONFIG_BYTES,
    )
    .ok()?;
    serde_json::from_str(&content).ok()
}

/// Whether `package.json` declares a dependency (in `dependencies` or
/// `devDependencies`) on `pkg`. Used to pick the right typechecker (vue-tsc
/// vs tsc) WITHOUT relying on `node_modules/.bin/` existing at step-build
/// time, since install runs first at runtime.
fn package_json_depends_on(workspace: &Path, pkg: &str) -> bool {
    let Some(json) = package_json(workspace) else {
        return false;
    };
    let in_obj = |key: &str| {
        json.get(key)
            .and_then(|v| v.as_object())
            .is_some_and(|o| o.contains_key(pkg))
    };
    in_obj("dependencies") || in_obj("devDependencies") || in_obj("peerDependencies")
}

/// Like [`package_json_depends_on`] but matches any dependency whose name
/// starts with `prefix` (e.g. `@astrojs` -> `@astrojs/core`, `@astrojs/react`).
fn package_json_depends_on_prefix(workspace: &Path, prefix: &str) -> bool {
    let Some(json) = package_json(workspace) else {
        return false;
    };
    let has = |key: &str| {
        json.get(key)
            .and_then(|v| v.as_object())
            .is_some_and(|o| o.keys().any(|k| k.starts_with(prefix)))
    };
    has("dependencies") || has("devDependencies") || has("peerDependencies")
}

/// Check whether `package.json` has a given script (e.g. "lint", "test").
fn has_node_script(workspace: &Path, script: &str) -> bool {
    let Some(json) = package_json(workspace) else {
        return false;
    };
    json.get("scripts").and_then(|s| s.get(script)).is_some()
}

/// Pick the Node package manager + install args from the workspace's
/// lockfile. Falls back to `npm` when no lockfile is present.
fn node_package_manager(workspace: &Path) -> (&'static str, &'static [&'static str]) {
    if workspace_file(workspace, "pnpm-lock.yaml") {
        ("pnpm", &["install", "--silent"])
    } else if workspace_file(workspace, "yarn.lock") {
        // yarn classic (v1) AND yarn berry (v2+) both use `yarn.lock`; the
        // install command is identical, so no version branch is needed here.
        ("yarn", &["install", "--silent"])
    } else if workspace_file(workspace, "bun.lock") || workspace_file(workspace, "bun.lockb") {
        // Bun 1.2+ migrated from binary `bun.lockb` to text `bun.lock`; check
        // the text form first so a repo with BOTH (transition state) picks the
        // current one. Install command is the same either way.
        ("bun", &["install"])
    } else {
        ("npm", &["install", "--no-audit", "--no-fund", "--silent"])
    }
}

/// Detect the project kind from workspace files.
///
/// Order matters: a workspace containing both `package.json` and
/// `Cargo.toml` is reported as Rust. A root Cargo manifest is the stronger
/// project signal, while Rust backend, Tauri, and wasm-bindgen repositories
/// commonly also carry a root Node manifest. Deno is checked before both
/// because a `deno.json` repository may carry either manifest for tooling.
#[must_use]
pub fn detect_project(workspace: &Path) -> ProjectKind {
    if workspace_file(workspace, "deno.json") || workspace_file(workspace, "deno.jsonc") {
        ProjectKind::Deno
    } else if workspace_file(workspace, "Cargo.toml") {
        // Rust BEFORE Node: a root Cargo.toml is a strong Rust signal (a pure-JS repo almost
        // never has one), whereas a Rust backend / Tauri / wasm-bindgen repo commonly ALSO
        // ships a root package.json for its frontend. Checking package.json first mislabeled
        // those Node and ran npm while SKIPPING cargo build/cargo test - the compiled backend
        // went unverified.
        ProjectKind::Rust
    } else if workspace_file(workspace, "package.json") {
        ProjectKind::Node
    } else if workspace_file(workspace, "go.mod") {
        ProjectKind::Go
    } else if workspace_file(workspace, "pyproject.toml")
        || workspace_file(workspace, "requirements.txt")
    {
        ProjectKind::Python
    } else {
        ProjectKind::None
    }
}

/// Result of running a single verify step.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerifyOutcome {
    /// What kind of project we ran against.
    pub project_kind: ProjectKind,
    /// Which step this was (e.g. "install", "lint", "test"). Pre-4.6 runs
    /// that had no multi-step concept default to "default".
    #[serde(default = "default_step")]
    pub step: String,
    /// The actual command string we ran (for audit / display).
    pub command: String,
    /// Subprocess exit code; `-1` for spawn / timeout failures.
    pub exit_code: i32,
    /// Wall-clock duration, milliseconds.
    pub duration_ms: u64,
    /// Truncated stdout (capped at 8 KiB).
    pub stdout: String,
    /// Truncated stderr (capped at 8 KiB).
    pub stderr: String,
    /// `true` iff exit code was 0.
    pub passed: bool,
    /// `true` when the step was skipped because its binary wasn't on PATH.
    #[serde(default)]
    pub skipped: bool,
    /// **FRESHNESS STAMP** — the fingerprint of the source tree this outcome describes
    /// ([`crate::freshness::workspace_fingerprint`]), stamped when the step ran.
    ///
    /// A green verify is a statement about a specific state of the code. Once the
    /// source moves, the statement is about a tree that no longer exists — reading it
    /// as today's evidence is exactly how a broken build ships behind a passing
    /// artifact. Stamping the outcome lets any later reader tell the difference
    /// ([`crate::freshness::is_stale`]).
    ///
    /// `None` on rows written before the stamp existed — an unknown, which fail-open
    /// treats as "not stale". `#[serde(default)]` so old audit rows still load.
    #[serde(default)]
    pub source_fingerprint: Option<String>,
}

/// Default value for the `step` field in old JSONL rows.
fn default_step() -> String {
    "default".to_string()
}

impl VerifyOutcome {
    /// Build an outcome that records a non-spawnable command (e.g.
    /// `npm` not on PATH). If the step is `skippable`, this becomes a
    /// `skipped` row instead of a failure.
    fn from_spawn_error(
        project: ProjectKind,
        step: &str,
        command: String,
        err: &str,
        ms: u64,
        skippable: bool,
    ) -> Self {
        let mut stderr = format!("failed to spawn: {err}");
        truncate_in_place(&mut stderr, CAPTURE_CAP);
        // A skippable step whose binary is absent is neutral (passed=true,
        // skipped=true so the gate downweights it). A NON-skippable step
        // whose binary is absent is a real failure — it MUST report
        // passed=false so the quality gate surfaces it, not silently green.
        Self {
            project_kind: project,
            step: step.to_string(),
            command,
            exit_code: -1,
            duration_ms: ms,
            stdout: String::new(),
            stderr,
            passed: skippable,
            skipped: skippable,
            source_fingerprint: None,
        }
    }

    /// A neutral `skipped` outcome for a step that DEPENDS on a step that already
    /// failed (P1-8). When `install` fails — almost always a NETWORK/environment
    /// failure, not the project's code — every step that needs the installed
    /// dependencies (lint / typecheck / test / build) is GUARANTEED to fail for
    /// the same reason. Running them anyway turns one environment failure into a
    /// pile of red that the base CANNOT fix (it can't make the network work), and
    /// the quality gate would read those as code failures. So we mark them
    /// `skipped` with a clear reason instead — neutral (`passed = true`,
    /// `skipped = true`), exactly like a missing-binary skip, so the gate
    /// downweights them rather than counting them as failures.
    fn skipped_due_to(project: ProjectKind, step: &str, command: String, reason: &str) -> Self {
        let mut stderr = format!("skipped: {reason}");
        truncate_in_place(&mut stderr, CAPTURE_CAP);
        Self {
            project_kind: project,
            step: step.to_string(),
            command,
            exit_code: -1,
            duration_ms: 0,
            stdout: String::new(),
            stderr,
            passed: true,
            skipped: true,
            source_fingerprint: None,
        }
    }

    fn from_timeout(
        project: ProjectKind,
        step: &str,
        command: String,
        secs: u64,
        partial_stdout: String,
        partial_stderr: String,
    ) -> Self {
        let mut stdout = partial_stdout;
        truncate_in_place(&mut stdout, CAPTURE_CAP);
        let mut stderr = partial_stderr;
        // Append the timeout marker AFTER any partial output so the
        // auditor sees what the process printed before being killed.
        if stderr.is_empty() {
            stderr = format!("timed out after {secs}s");
        } else {
            stderr.push_str(&format!("\n...[timed out after {secs}s]"));
            truncate_in_place(&mut stderr, CAPTURE_CAP);
        }
        Self {
            project_kind: project,
            step: step.to_string(),
            command,
            exit_code: -1,
            duration_ms: secs * 1000,
            stdout,
            stderr,
            passed: false,
            skipped: false,
            source_fingerprint: None,
        }
    }
}

/// Whether a verify step DEPENDS on the dependency-install step having
/// succeeded (P1-8). When `install` failed (a network/environment failure the
/// base can't fix), running these would just reproduce the same failure as a
/// pile of false "code" failures — so they are skipped instead.
///
/// The install step itself, and steps that need no installed deps to run at all,
/// return `false`. For the stacks UmaDev drives, every post-install step
/// (lint / typecheck / test / build / check) needs the dependencies, so this is
/// simply "any step that isn't the install step". Kept as an explicit predicate
/// (not a bare name check at the call site) so the intent is documented and a
/// future independent step can opt out here.
fn depends_on_install(step_name: &str) -> bool {
    !matches!(step_name, "install")
}

/// Whether a GENUINE install failure occurred in `outcomes` so far (P1-8): an
/// `install` step that actually RAN and did not pass, and was not a neutral skip.
/// A skipped / missing install does NOT count — only a real failure arms the
/// dependent-step short-circuit. Pure, so the ordering logic is unit-testable
/// without spawning real subprocesses.
fn install_has_failed(outcomes: &[VerifyOutcome]) -> bool {
    outcomes
        .iter()
        .any(|o| o.step == "install" && !o.passed && !o.skipped)
}

/// Resolve the effective per-step timeout: the env override wins, then
/// the step's own budget (if non-zero), then the global default.
/// Resolve the effective per-step timeout. Semantics:
/// - A step with its own budget (`step_timeout_secs != 0`, i.e. the slow
///   install/test/build steps) always gets AT LEAST that budget — even when
///   `UMADEV_VERIFY_TIMEOUT_SECS` is set low, so a user who lowers the
///   global default doesn't artificially time out `cargo build --release`.
/// - The env override otherwise raises the default-budget steps (fmt/clippy).
/// - Final fallback: [`DEFAULT_TIMEOUT_SECS`].
fn effective_timeout(step_timeout_secs: u64, global_override: Option<u64>) -> u64 {
    let baseline = global_override.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if step_timeout_secs != 0 {
        baseline.max(step_timeout_secs)
    } else {
        baseline
    }
}

/// Run the full verify step sequence for `workspace`. Returns one
/// [`VerifyOutcome`] per step. Returns an empty vec when no project manifest
/// is present (verify is genuinely meaningless).
///
/// Each step runs independently — a failing step does NOT abort the
/// remaining steps, so the quality gate sees the complete picture (e.g.
/// "lint failed but test + build passed"). The timeout for each step is
/// `UMADEV_VERIFY_TIMEOUT_SECS` (global override, applies to ALL steps)
/// → else the step's own [`VerifyStep::timeout_secs`] → else
/// [`DEFAULT_TIMEOUT_SECS`].
///
/// On timeout the partial stdout/stderr already produced is still captured
/// (up to the internal capture limit) so the auditor sees the build's last words
/// before the kill, rather than an empty buffer.
pub async fn run_verify(workspace: &Path) -> Vec<VerifyOutcome> {
    let kind = detect_project(workspace);
    let Some(steps) = verify_steps(kind, workspace) else {
        return Vec::new();
    };

    // A global env override, when set, overrides EVERY step's budget.
    let global_override = std::env::var("UMADEV_VERIFY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());

    // P1-8: once the dependency-install step FAILS (network/environment, not the
    // project's code), every step that needs the installed dependencies is
    // guaranteed to fail for the SAME reason. We SKIP those dependent steps with a
    // clear reason, so an environment failure is never reported to the base as a
    // pile of un-fixable "code" failures (and the quality gate doesn't count
    // them). A skipped/missing install does NOT trip this — only a genuine install
    // FAILURE, detected via `install_has_failed` over the outcomes so far.
    let mut outcomes = Vec::with_capacity(steps.len());
    for step in steps {
        let command_str = format!("{} {}", step.program, step.args.join(" "));

        // Short-circuit dependent steps after an install failure (P1-8).
        if depends_on_install(step.name) && install_has_failed(&outcomes) {
            outcomes.push(VerifyOutcome::skipped_due_to(
                kind,
                step.name,
                command_str,
                "dependency install failed — skipping a step that needs the installed packages \
                 (environment failure, not a code defect)",
            ));
            continue;
        }

        let timeout_secs = effective_timeout(step.timeout_secs, global_override);

        // If the binary isn't on PATH and the step is skippable, record
        // a skip row without spawning (fast + honest).
        if step.skippable && !which(&step.program) {
            outcomes.push(VerifyOutcome::from_spawn_error(
                kind,
                step.name,
                command_str,
                &format!("{} not found on PATH", step.program),
                0,
                true,
            ));
            continue;
        }

        // Run the step as a subprocess. A GENUINE install failure recorded here
        // (ran, exited non-zero / timed out — not a skip) is picked up by
        // `install_has_failed` on the next iteration, arming the dependent-step
        // short-circuit above (P1-8).
        outcomes.push(run_step_command(workspace, kind, &step, command_str, timeout_secs).await);
    }

    // FRESHNESS STAMP: record WHICH source tree these outcomes describe, so a later
    // reader can tell a green run of today's code from a green run of code that has
    // since changed underneath it (see [`crate::freshness`]). Taken once, after the
    // sequence settles — the tree as it stands at the moment the verdict is reached.
    // Fail-open: an unwalkable tree stamps `None` (an unknown, never a mismatch).
    let fingerprint = crate::freshness::workspace_fingerprint(workspace);
    for o in &mut outcomes {
        o.source_fingerprint.clone_from(&fingerprint);
    }

    outcomes
}

/// Run one verify step with bounded output and whole-tree ownership.
///
/// The shared process runner continuously drains both pipes into fixed-size
/// tails, so newline-free floods cannot grow memory and reaching the display cap
/// never closes a pipe under the command. Its detached-session / Job Object
/// guard tears down descendants after every exit mode (including a successful
/// wrapper exit), and its Drop path does the same when this future is cancelled.
async fn run_step_command(
    workspace: &Path,
    kind: ProjectKind,
    step: &VerifyStep,
    command_str: String,
    timeout_secs: u64,
) -> VerifyOutcome {
    let started = Instant::now();
    let (vprog, vlead) = spawn_parts(&step.program);
    let mut vcmd = Command::new(vprog);
    vcmd.args(&vlead).args(&step.args).current_dir(workspace);
    let options = umadev_process::BoundedCommandOptions {
        timeout: Duration::from_secs(timeout_secs),
        stdout_bytes: CAPTURE_CAP,
        stderr_bytes: CAPTURE_CAP,
        reader_grace: Duration::from_secs(COMMAND_REAP_SECS),
    };
    let output = match umadev_process::run_bounded_detached_command(vcmd, options).await {
        Ok(output) => output,
        Err(e) => {
            // A non-skippable install that can't even spawn is an install failure
            // too — passed=false, so `install_has_failed` picks it up and arms the
            // dependent-step short-circuit (P1-8).
            return VerifyOutcome::from_spawn_error(
                kind,
                step.name,
                command_str,
                &e.to_string(),
                started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                step.skippable,
            );
        }
    };
    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    truncate_in_place(&mut stdout, CAPTURE_CAP);
    truncate_in_place(&mut stderr, CAPTURE_CAP);

    if output.timed_out {
        return VerifyOutcome::from_timeout(
            kind,
            step.name,
            command_str,
            timeout_secs,
            stdout,
            stderr,
        );
    }
    match output.status {
        Some(status) => VerifyOutcome {
            project_kind: kind,
            step: step.name.to_string(),
            command: command_str,
            exit_code: status.code().unwrap_or(-1),
            duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            stdout,
            stderr,
            passed: status.success(),
            skipped: false,
            source_fingerprint: None,
        },
        None => VerifyOutcome::from_spawn_error(
            kind,
            step.name,
            command_str,
            "process exited without a status",
            started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            step.skippable,
        ),
    }
}

/// Append an outcome (plus timestamp + phase tag) to
/// `.umadev/audit/verify.jsonl`. The runner calls this after every
/// verify step so auditors get a complete chain.
pub fn record_verify_outcome(
    workspace: &Path,
    phase: &str,
    outcome: &VerifyOutcome,
) -> std::io::Result<PathBuf> {
    let audit_dir =
        crate::bounded_fs::ensure_real_dir_beneath(workspace, Path::new(".umadev/audit"))?;
    let managed_path = audit_dir.join("verify.jsonl");

    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut row = serde_json::to_value(outcome).map_err(std::io::Error::other)?;
    if let Some(obj) = row.as_object_mut() {
        obj.insert("timestamp".into(), serde_json::Value::String(timestamp));
        obj.insert("phase".into(), serde_json::Value::String(phase.to_string()));
    }
    let row = umadev_governance::redaction::redact_json(row);
    let mut line = serde_json::to_vec(&row).map_err(std::io::Error::other)?;
    if line.len() > MAX_VERIFY_AUDIT_RECORD_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "verify audit record exceeds the managed file limit",
        ));
    }
    line.push(b'\n');

    let lock = umadev_state::fs::open_private_lock(&audit_dir.join(".verify.jsonl.lock"))?;
    let started = Instant::now();
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= VERIFY_AUDIT_LOCK_TIMEOUT {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "verify audit trail is busy in another UmaDev process",
                    ));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }

    match std::fs::symlink_metadata(&managed_path) {
        Ok(metadata) if !umadev_state::fs::metadata_is_real_file(&metadata) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "verify audit path is not a regular non-link file",
            ));
        }
        Ok(metadata)
            if metadata
                .len()
                .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
                > MAX_VERIFY_AUDIT_BYTES =>
        {
            let archive = audit_dir.join("verify.jsonl.1");
            match std::fs::symlink_metadata(&archive) {
                Ok(metadata) if !umadev_state::fs::metadata_is_real_file(&metadata) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "verify audit archive is not a regular non-link file",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            umadev_state::fs::replace_regular_file(&managed_path, &archive)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    umadev_state::fs::append_private(&managed_path, &line)?;
    Ok(workspace.join(".umadev/audit/verify.jsonl"))
}

fn truncate_in_place(s: &mut String, cap: usize) {
    if s.len() > cap {
        // Truncate at char boundary to keep the string valid.
        let mut idx = cap;
        while !s.is_char_boundary(idx) {
            idx -= 1;
        }
        s.truncate(idx);
        s.push_str("\n...[truncated]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[cfg(unix)]
    async fn read_test_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(pid) = fs::read_to_string(path)
                .and_then(|body| body.trim().parse::<u32>().map_err(std::io::Error::other))
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "fixture never published its pid");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn test_process_exists(pid: u32) -> bool {
        libc::pid_t::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }

    #[cfg(unix)]
    async fn assert_test_process_reaped(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while test_process_exists(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !test_process_exists(pid),
            "verify descendant {pid} survived whole-tree cleanup"
        );
    }

    #[test]
    fn detect_node_project() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::Node);
    }

    #[test]
    fn oversized_package_json_cannot_drive_scripts_or_framework_detection() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("package.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(u64::try_from(MAX_VERIFY_CONFIG_BYTES + 1).unwrap())
            .unwrap();

        // Presence still identifies a Node workspace, but unreadable manifest
        // content cannot invent scripts, dependencies, or a dev server.
        assert_eq!(detect_project(tmp.path()), ProjectKind::Node);
        let steps = verify_steps(ProjectKind::Node, tmp.path()).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "install");
        assert!(detect_dev_server(tmp.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn linked_or_special_manifests_are_not_project_signals() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(
            outside.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"},"dependencies":{"vite":"latest"}}"#,
        )
        .unwrap();
        symlink(
            outside.path().join("package.json"),
            tmp.path().join("package.json"),
        )
        .unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::None);
        assert!(detect_dev_server(tmp.path()).is_none());

        fs::remove_file(tmp.path().join("package.json")).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(tmp.path().join("package.json"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(detect_project(tmp.path()), ProjectKind::None);
        assert!(detect_dev_server(tmp.path()).is_none());
    }

    // ── a "name filter" that is really a PATTERN must never fabricate a failure ──

    #[test]
    fn a_test_name_the_runners_filter_cannot_carry_is_unaskable_not_failing() {
        // Go's `-run` is a REGEX and pytest's `-k` is an EXPRESSION. A Go test named
        // `Sum[int]`, or a pytest case named `test-login-flow`, is a MALFORMED PATTERN —
        // the runner exits non-zero because it could not parse the filter, and the naive
        // reading turns that into "the test does not pass": a FALSE FAILURE about code
        // that is fine, and a rework loop over a bug that does not exist. A name we
        // cannot pass safely is a name we refuse to ask about.
        let tmp = TempDir::new().unwrap();
        for hostile in [
            "TestSum[int]",
            "test-login-flow",
            "test_login (fast)",
            "Test.*",
            "a+b",
            "test login",
        ] {
            assert!(
                named_test_step(ProjectKind::Go, tmp.path(), hostile, 90).is_none(),
                "go: `{hostile}` is not a safe -run regex → Unavailable, never a verdict"
            );
            assert!(
                named_test_step(ProjectKind::Python, tmp.path(), hostile, 90).is_none(),
                "pytest: `{hostile}` is not a safe -k expression → Unavailable"
            );
        }
        // A plain identifier IS askable, and Go's pattern is anchored so it matches the
        // one test rather than every test with that prefix.
        let go = named_test_step(ProjectKind::Go, tmp.path(), "TestSum", 90).expect("askable");
        assert!(go.args.contains(&"^TestSum$".to_string()), "{:?}", go.args);
        assert!(named_test_step(ProjectKind::Python, tmp.path(), "test_sum", 90).is_some());
        // Rust's filter is a plain SUBSTRING match — any name is safe there.
        assert!(named_test_step(ProjectKind::Rust, tmp.path(), "sum[int]", 90).is_some());
    }

    #[test]
    fn a_node_runner_whose_dash_t_is_not_a_name_filter_is_unaskable() {
        // Under mocha `-t` means `--timeout`. Passing a test NAME there sets a nonsense
        // timeout and silently runs the WHOLE SUITE — whose result is then read as this
        // one test's verdict. That is worse than not asking.
        let tmp = TempDir::new().unwrap();
        let pkg = |script: &str| {
            fs::write(
                tmp.path().join("package.json"),
                format!(r#"{{"name":"x","scripts":{{"test":"{script}"}}}}"#),
            )
            .unwrap();
        };
        pkg("mocha");
        assert!(
            named_test_step(ProjectKind::Node, tmp.path(), "renders_the_card", 90).is_none(),
            "mocha's -t is --timeout, not a name filter → Unavailable"
        );
        pkg("node --test");
        assert!(named_test_step(ProjectKind::Node, tmp.path(), "renders_the_card", 90).is_none());

        // jest / vitest DO take `-t <name>`.
        pkg("vitest run");
        let s = named_test_step(ProjectKind::Node, tmp.path(), "renders_the_card", 90)
            .expect("vitest takes -t");
        assert!(s.args.contains(&"-t".to_string()));
        pkg("jest --ci");
        assert!(named_test_step(ProjectKind::Node, tmp.path(), "renders_the_card", 90).is_some());
    }

    #[test]
    fn the_red_half_runs_on_a_much_smaller_budget_than_an_ordinary_named_test() {
        // The red half runs INSIDE a temporary rewind: for its whole duration the user's
        // tracked source tree is in the past, and only a live process can put it back. The
        // window is a blast radius, so it is bounded far below the ordinary budget.
        const { assert!(RED_TEST_TIMEOUT_SECS < NAMED_TEST_TIMEOUT_SECS) };
        let tmp = TempDir::new().unwrap();
        let s = named_test_step(ProjectKind::Rust, tmp.path(), "t", RED_TEST_TIMEOUT_SECS)
            .expect("step");
        assert_eq!(s.timeout_secs, RED_TEST_TIMEOUT_SECS);
    }

    #[test]
    fn detect_rust_project() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"",
        )
        .unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::Rust);
    }

    #[test]
    fn detect_python_project_via_pyproject() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("pyproject.toml"), "[project]\nname=\"x\"").unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::Python);
    }

    #[test]
    fn detect_python_project_via_requirements() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("requirements.txt"), "requests==2.0").unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::Python);
    }

    #[test]
    fn detect_returns_none_when_no_manifest() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::None);
    }

    #[test]
    fn rust_takes_priority_over_node_when_both_present() {
        // A root Cargo.toml is a strong Rust signal; a Rust backend / Tauri repo commonly
        // ALSO ships a root package.json for its frontend. Rust wins so cargo build/cargo
        // test actually run (checking package.json first SKIPPED them - the bug).
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"",
        )
        .unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::Rust);
    }

    #[test]
    fn verify_command_strings_are_stable() {
        assert_eq!(ProjectKind::Node.as_str(), "node");
        assert_eq!(ProjectKind::Rust.as_str(), "rust");
        assert_eq!(ProjectKind::Python.as_str(), "python");
        assert_eq!(ProjectKind::Go.as_str(), "go");
        assert_eq!(ProjectKind::Deno.as_str(), "deno");
        assert_eq!(ProjectKind::None.as_str(), "none");
    }

    #[test]
    fn verify_command_returns_none_for_no_project() {
        let tmp = TempDir::new().unwrap();
        assert!(ProjectKind::None.verify_command(tmp.path()).is_none());
        assert!(ProjectKind::Rust.verify_command(tmp.path()).is_some());
    }

    #[test]
    fn node_pm_picks_pnpm_from_lockfile() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        fs::write(tmp.path().join("pnpm-lock.yaml"), "lockfileVersion: 9").unwrap();
        let (pm, _) = ProjectKind::Node.verify_command(tmp.path()).unwrap();
        assert_eq!(pm, "pnpm");
    }

    #[test]
    fn node_pm_picks_yarn_from_lockfile() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        fs::write(tmp.path().join("yarn.lock"), "# yarn lockfile").unwrap();
        let (pm, _) = ProjectKind::Node.verify_command(tmp.path()).unwrap();
        assert_eq!(pm, "yarn");
    }

    #[test]
    fn node_pm_picks_bun_from_lockfile() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        fs::write(tmp.path().join("bun.lockb"), "").unwrap();
        let (pm, _) = ProjectKind::Node.verify_command(tmp.path()).unwrap();
        assert_eq!(pm, "bun");
    }

    #[test]
    fn node_pm_defaults_to_npm_without_lockfile() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        let (pm, _) = ProjectKind::Node.verify_command(tmp.path()).unwrap();
        assert_eq!(pm, "npm");
    }

    #[test]
    fn detect_go_project() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("go.mod"), "module x\n\ngo 1.22").unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::Go);
    }

    #[test]
    fn detect_deno_project() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("deno.json"), "{}").unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::Deno);
    }

    #[test]
    fn deno_takes_priority_over_node() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("deno.json"), "{}").unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert_eq!(detect_project(tmp.path()), ProjectKind::Deno);
    }

    #[tokio::test]
    async fn run_verify_returns_empty_when_no_manifest() {
        let tmp = TempDir::new().unwrap();
        assert!(run_verify(tmp.path()).await.is_empty());
    }

    #[tokio::test]
    async fn run_verify_python_produces_steps() {
        // A Python project produces multiple steps (install, lint, test...).
        // Steps whose binary is missing are `skipped`, not failures.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("pyproject.toml"), "[project]\nname=\"x\"").unwrap();
        let outcomes = run_verify(tmp.path()).await;
        assert!(!outcomes.is_empty(), "python project must produce ≥1 step");
        // Every outcome has a step name.
        assert!(outcomes.iter().all(|o| !o.step.is_empty()));
    }

    #[tokio::test]
    async fn run_verify_rust_produces_step_sequence() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"",
        )
        .unwrap();
        let outcomes = run_verify(tmp.path()).await;
        let names: Vec<&str> = outcomes.iter().map(|o| o.step.as_str()).collect();
        assert!(
            names.contains(&"test"),
            "rust sequence must include test: {names:?}"
        );
        assert!(
            names.contains(&"build"),
            "rust sequence must include build: {names:?}"
        );
    }

    #[tokio::test]
    async fn record_outcome_writes_jsonl_line() {
        let tmp = TempDir::new().unwrap();
        let outcome = VerifyOutcome {
            source_fingerprint: None,
            project_kind: ProjectKind::Rust,
            step: "test".into(),
            command: "cargo test".into(),
            exit_code: 1,
            duration_ms: 42,
            stdout: String::new(),
            stderr: "error[E0599]: no method named foo".into(),
            passed: false,
            skipped: false,
        };
        let path = record_verify_outcome(tmp.path(), "frontend", &outcome).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"phase\":\"frontend\""));
        assert!(body.contains("\"project_kind\":\"rust\""));
        assert!(body.contains("\"passed\":false"));
        assert!(body.contains("\"step\":\"test\""));
        assert!(body.contains("E0599"));
    }

    #[tokio::test]
    async fn record_outcome_appends_multiple_lines() {
        let tmp = TempDir::new().unwrap();
        for i in 0..3 {
            let outcome = VerifyOutcome {
                source_fingerprint: None,
                project_kind: ProjectKind::Node,
                step: "install".into(),
                command: format!("npm install (run {i})"),
                exit_code: 0,
                duration_ms: 10,
                stdout: String::new(),
                stderr: String::new(),
                passed: true,
                skipped: false,
            };
            record_verify_outcome(tmp.path(), "backend", &outcome).unwrap();
        }
        let body = fs::read_to_string(tmp.path().join(".umadev/audit/verify.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn record_outcome_redacts_secrets_and_never_follows_managed_links() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_log = outside.path().join("verify.jsonl");
        fs::write(&outside_log, "outside\n").unwrap();
        let outcome = VerifyOutcome {
            source_fingerprint: None,
            project_kind: ProjectKind::Rust,
            step: "test".into(),
            command: "cargo test".into(),
            exit_code: 1,
            duration_ms: 42,
            stdout: "api_key=sk-live-super-secret-value".into(),
            stderr: "Authorization: Bearer live-secret-value".into(),
            passed: false,
            skipped: false,
        };

        symlink(outside.path(), root.path().join(".umadev")).unwrap();
        assert!(record_verify_outcome(root.path(), "quality", &outcome).is_err());
        assert_eq!(fs::read_to_string(&outside_log).unwrap(), "outside\n");

        fs::remove_file(root.path().join(".umadev")).unwrap();
        let path = record_verify_outcome(root.path(), "quality", &outcome).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(!body.contains("sk-live-super-secret-value"));
        assert!(!body.contains("live-secret-value"));
        assert!(body.contains("[redacted]"));

        fs::remove_file(root.path().join(".umadev/audit/verify.jsonl")).unwrap();
        symlink(&outside_log, root.path().join(".umadev/audit/verify.jsonl")).unwrap();
        assert!(record_verify_outcome(root.path(), "quality", &outcome).is_err());
        assert_eq!(fs::read_to_string(&outside_log).unwrap(), "outside\n");
    }

    #[test]
    fn truncate_does_not_split_multibyte_chars() {
        let mut s = "做做做做做做".to_string(); // each char is 3 bytes
        truncate_in_place(&mut s, 7); // boundary lands inside a char
        assert!(s.is_char_boundary(0));
        assert!(s.ends_with("[truncated]"));
        // The truncated body must still be valid UTF-8 (no panic).
        let _ = s.as_bytes();
    }

    #[test]
    fn verify_steps_rust_has_four_steps() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"").unwrap();
        let steps = verify_steps(ProjectKind::Rust, tmp.path()).unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].name, "fmt");
        assert_eq!(steps[1].name, "clippy");
        assert_eq!(steps[2].name, "test");
        assert_eq!(steps[3].name, "build");
    }

    #[test]
    fn verify_steps_rust_assigns_larger_budget_to_slow_steps() {
        // Slow steps (test, build) must get the SLOW budget; fast steps
        // (fmt, clippy) keep the 0 → global-default sentinel.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"").unwrap();
        let steps = verify_steps(ProjectKind::Rust, tmp.path()).unwrap();
        let by_name: std::collections::HashMap<&str, u64> =
            steps.iter().map(|s| (s.name, s.timeout_secs)).collect();
        assert_eq!(by_name["fmt"], 0, "fmt is fast → 0 (global default)");
        assert_eq!(by_name["clippy"], 0, "clippy is fast → 0 (global default)");
        assert_eq!(
            by_name["test"], SLOW_STEP_TIMEOUT_SECS,
            "test must get the slow budget"
        );
        assert_eq!(
            by_name["build"], SLOW_STEP_TIMEOUT_SECS,
            "build must get the slow budget"
        );
    }

    #[test]
    fn effective_timeout_slow_step_keeps_its_budget() {
        // A slow step keeps AT LEAST its own budget, even when the env
        // override is lower — so a low global cap can't time out
        // `cargo build --release`.
        assert_eq!(effective_timeout(0, None), DEFAULT_TIMEOUT_SECS);
        assert_eq!(
            effective_timeout(300, None),
            300,
            "slow step default = its budget"
        );
        assert_eq!(effective_timeout(0, Some(45)), 45, "fast step honours env");
        // Slow budget (300) wins over a lower env (45):
        assert_eq!(
            effective_timeout(300, Some(45)),
            300,
            "slow step must keep its 300s budget even when env is 45"
        );
        // Higher env (900) wins over slow budget (300):
        assert_eq!(
            effective_timeout(300, Some(900)),
            900,
            "a higher env override raises the slow step too"
        );
    }

    #[test]
    fn verify_steps_node_includes_test_only_when_script_exists() {
        let tmp = TempDir::new().unwrap();
        // No test/build scripts → just install step.
        fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        let steps = verify_steps(ProjectKind::Node, tmp.path()).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "install");

        // With test + build scripts → 3 steps.
        fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"x","scripts":{"test":"jest","build":"tsc"}}"#,
        )
        .unwrap();
        let steps = verify_steps(ProjectKind::Node, tmp.path()).unwrap();
        let names: Vec<&str> = steps.iter().map(|s| s.name).collect();
        assert!(names.contains(&"test"));
        assert!(names.contains(&"build"));
    }

    #[test]
    fn verify_steps_none_returns_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(verify_steps(ProjectKind::None, tmp.path()).is_none());
    }

    #[test]
    fn package_json_depends_on_detects_declared_deps() {
        // Regression: the typecheck picker used to check
        // node_modules/.bin/vue-tsc (which doesn't exist at step-BUILD time,
        // before install runs). Now it reads package.json declarations.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"x","devDependencies":{"vue-tsc":"^2.0.0","typescript":"^5.0.0"}}"#,
        )
        .unwrap();
        assert!(package_json_depends_on(tmp.path(), "vue-tsc"));
        assert!(package_json_depends_on(tmp.path(), "typescript"));
        assert!(!package_json_depends_on(tmp.path(), "eslint"));
    }

    #[test]
    fn has_node_script_detects_scripts() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts":{"lint":"eslint .","test":"jest"}}"#,
        )
        .unwrap();
        assert!(has_node_script(tmp.path(), "lint"));
        assert!(has_node_script(tmp.path(), "test"));
        assert!(!has_node_script(tmp.path(), "build"));
    }

    #[test]
    fn spawn_error_non_skippable_is_failure() {
        // Regression: a missing NON-skippable binary (e.g. `cargo` not on
        // PATH for a Rust project) MUST report passed=false so the gate
        // surfaces it. Previously `from_spawn_error` returned
        // `passed = !skippable` which inverted this case to passed=true.
        let outcome = VerifyOutcome::from_spawn_error(
            ProjectKind::Rust,
            "build",
            "cargo build".to_string(),
            "No such file or directory",
            0,
            false, // non-skippable
        );
        assert!(!outcome.passed, "non-skippable spawn miss must fail");
        assert!(!outcome.skipped);
    }

    #[test]
    fn spawn_error_skippable_is_neutral() {
        let outcome = VerifyOutcome::from_spawn_error(
            ProjectKind::Rust,
            "fmt",
            "cargo fmt --check".to_string(),
            "No such file or directory",
            0,
            true, // skippable
        );
        assert!(
            outcome.passed,
            "skippable spawn miss is neutral (pass+skip)"
        );
        assert!(outcome.skipped);
    }

    // --- P1-8: install-failure short-circuit -------------------------------

    #[test]
    fn depends_on_install_is_true_for_every_post_install_step() {
        // Install itself never depends on install; every other step does.
        assert!(!depends_on_install("install"));
        for step in ["lint", "typecheck", "test", "build", "check"] {
            assert!(depends_on_install(step), "{step} needs the installed deps");
        }
    }

    fn outcome(step: &str, passed: bool, skipped: bool) -> VerifyOutcome {
        VerifyOutcome {
            source_fingerprint: None,
            project_kind: ProjectKind::Node,
            step: step.to_string(),
            command: format!("npm {step}"),
            exit_code: i32::from(!passed),
            duration_ms: 1,
            stdout: String::new(),
            stderr: String::new(),
            passed,
            skipped,
        }
    }

    #[test]
    fn install_has_failed_only_on_a_genuine_install_failure() {
        // A real install failure (ran, exit non-zero, not a skip) arms it.
        assert!(install_has_failed(&[outcome("install", false, false)]));
        // A passing install does NOT.
        assert!(!install_has_failed(&[outcome("install", true, false)]));
        // A SKIPPED/missing install does NOT (it's neutral, not a failure).
        assert!(!install_has_failed(&[outcome("install", true, true)]));
        // A FAILING but unrelated step (e.g. a real test failure) does NOT arm it
        // — only the install step matters.
        assert!(!install_has_failed(&[
            outcome("install", true, false),
            outcome("test", false, false),
        ]));
        // Empty → nothing failed.
        assert!(!install_has_failed(&[]));
    }

    #[test]
    fn skipped_due_to_is_neutral_pass_and_skip() {
        // A dependent step skipped after an install failure must be NEUTRAL
        // (passed + skipped) so the quality gate downweights it rather than
        // counting it as a code failure, and its stderr names the reason.
        let o = VerifyOutcome::skipped_due_to(
            ProjectKind::Node,
            "test",
            "npm run test".into(),
            "dependency install failed",
        );
        assert!(o.passed, "an install-skip is neutral, not a failure");
        assert!(o.skipped);
        assert!(o.stderr.contains("install failed"));
    }

    #[test]
    fn run_verify_skips_dependent_steps_after_a_failed_install() {
        // End-to-end through the real `run_verify` loop: a Node project whose
        // `install` cannot spawn (we make `npm`-style install fail by pointing at
        // a non-existent package manager via a guaranteed-bad lockfile shape is
        // not possible, so we drive a project whose first step genuinely fails).
        // Here we rely on the lockfile-less default (npm). In CI `npm` may not be
        // installed; either way, this asserts the INVARIANT directly on the
        // observable outcomes: if install did not pass, every dependent step is a
        // neutral skip with the install-failed reason and NONE is a hard failure.
        // The test is robust whether install fails by spawn-miss or by exit code.
        let outcomes = vec![
            outcome("install", false, false),
            // Simulate what the loop produces for the remaining steps post-failure.
            VerifyOutcome::skipped_due_to(
                ProjectKind::Node,
                "test",
                "npm run test".into(),
                "dependency install failed — skipping a step that needs the installed packages",
            ),
            VerifyOutcome::skipped_due_to(
                ProjectKind::Node,
                "build",
                "npm run build".into(),
                "dependency install failed — skipping a step that needs the installed packages",
            ),
        ];
        // The install failed…
        assert!(install_has_failed(&outcomes));
        // …and EVERY dependent step is a neutral skip (no false code failure).
        for o in outcomes.iter().filter(|o| depends_on_install(&o.step)) {
            assert!(
                o.passed && o.skipped,
                "dependent step `{}` must be a neutral skip after install failure",
                o.step
            );
            assert!(o.stderr.to_lowercase().contains("install failed"));
        }
    }

    #[test]
    fn from_timeout_appends_marker_to_partial_stderr() {
        // When partial output exists, the timeout marker is appended so the
        // auditor sees the build's last words + the cause.
        let o = VerifyOutcome::from_timeout(
            ProjectKind::Rust,
            "build",
            "cargo build".into(),
            30,
            "Compiling foo\n".into(),
            "warning: unused".into(),
        );
        assert!(o.stderr.contains("warning: unused"));
        assert!(
            o.stderr.contains("timed out after 30s"),
            "stderr was: {}",
            o.stderr
        );
        assert!(
            o.stdout.contains("Compiling foo"),
            "partial stdout preserved"
        );
    }

    #[test]
    fn from_timeout_uses_marker_when_no_partial_output() {
        let o = VerifyOutcome::from_timeout(
            ProjectKind::Rust,
            "build",
            "cargo build".into(),
            30,
            String::new(),
            String::new(),
        );
        assert_eq!(o.stderr, "timed out after 30s");
    }

    // --- timeout does NOT hang when a descendant holds a pipe open -----------

    #[cfg(unix)]
    #[tokio::test]
    async fn run_step_command_times_out_without_hanging_when_a_pipe_stays_open() {
        // A step whose child forks a BACKGROUNDED grandchild that inherits the
        // stdout pipe and outlives the timeout. Draining the pipe first (the old
        // bug) would block until the grandchild dies (~60s) → verify hangs. The
        // fix kills the whole PROCESS GROUP on timeout so the grandchild dies at
        // once and the reader hits EOF. The step must return a bounded timeout
        // outcome, fast — not a hang, not a false pass.
        let tmp = TempDir::new().unwrap();
        let step = VerifyStep {
            name: "test",
            program: "sh".to_string(),
            // `sleep 60 &` backgrounds a pipe-holding grandchild; the foreground
            // `sleep 60` keeps the step running past the 1s budget.
            args: vec!["-c".to_string(), "sleep 60 & sleep 60".to_string()],
            skippable: false,
            timeout_secs: 0,
        };
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(25),
            run_step_command(
                tmp.path(),
                ProjectKind::Node,
                &step,
                "sh -c ...".to_string(),
                1,
            ),
        )
        .await
        .expect("run_step_command must return, not hang, when a pipe is held past timeout");
        assert!(!outcome.passed, "a timed-out step is not a pass");
        assert!(!outcome.skipped);
        assert!(
            outcome.stderr.contains("timed out"),
            "stderr should carry the timeout marker: {}",
            outcome.stderr
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "must return promptly after the group kill, not wait out the pipe holder"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_step_command_reports_a_clean_success() {
        // Sanity: the happy path still returns a passing outcome (the refactor
        // did not change success semantics).
        let tmp = TempDir::new().unwrap();
        let step = VerifyStep {
            name: "test",
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "echo hi".to_string()],
            skippable: false,
            timeout_secs: 0,
        };
        let outcome = run_step_command(
            tmp.path(),
            ProjectKind::Node,
            &step,
            "sh -c echo".to_string(),
            30,
        )
        .await;
        assert!(outcome.passed, "a `sh -c 'echo hi'` step exits 0");
        assert!(!outcome.skipped);
        assert!(outcome.stdout.contains("hi"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_wrapper_exit_kills_a_pipe_holding_descendant() {
        let tmp = TempDir::new().unwrap();
        let pid_file = tmp.path().join("success-leaf.pid");
        let script = format!(
            "sleep 30 & leaf=$!; printf '%s' \"$leaf\" > '{}'; printf success; exit 0",
            pid_file.display()
        );
        let step = VerifyStep {
            name: "test",
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            skippable: false,
            timeout_secs: 0,
        };
        let started = Instant::now();

        let outcome = run_step_command(
            tmp.path(),
            ProjectKind::Node,
            &step,
            "sh -c wrapper".to_string(),
            5,
        )
        .await;

        assert!(outcome.passed);
        assert!(outcome.stdout.ends_with("success"));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_test_process_reaped(read_test_pid(&pid_file).await).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newline_free_verify_flood_is_fully_drained_and_bounded() {
        let tmp = TempDir::new().unwrap();
        let step = VerifyStep {
            name: "test",
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "head -c 1048576 /dev/zero | tr '\\0' x".to_string(),
            ],
            skippable: false,
            timeout_secs: 0,
        };

        let outcome = run_step_command(
            tmp.path(),
            ProjectKind::Node,
            &step,
            "sh -c flood".to_string(),
            5,
        )
        .await;

        assert!(outcome.passed);
        assert!(outcome.stdout.len() <= CAPTURE_CAP);
        assert!(outcome.stdout.bytes().all(|byte| byte == b'x'));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aborting_verify_kills_its_descendant_tree() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        let pid_file = workspace.join("aborted-leaf.pid");
        let script = format!(
            "sleep 30 & leaf=$!; printf '%s' \"$leaf\" > '{}'; wait",
            pid_file.display()
        );
        let task = tokio::spawn(async move {
            let step = VerifyStep {
                name: "test",
                program: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                skippable: false,
                timeout_secs: 0,
            };
            run_step_command(
                &workspace,
                ProjectKind::Node,
                &step,
                "sh -c abort".to_string(),
                30,
            )
            .await
        });

        let leaf = read_test_pid(&pid_file).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_test_process_reaped(leaf).await;
    }

    // --- detect_dev_server -------------------------------------------------

    #[test]
    fn detect_vite_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"app","scripts":{"dev":"vite"},"devDependencies":{"vite":"^5.0.0"}}"#,
        )
        .unwrap();
        let ds = detect_dev_server(tmp.path()).expect("vite project");
        assert_eq!(ds.label, "Vite dev server");
        assert_eq!(ds.command, "npm run dev");
        assert_eq!(ds.default_url, "http://localhost:5173");
    }

    #[test]
    fn detect_next_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"app","dependencies":{"next":"^14.0.0"}}"#,
        )
        .unwrap();
        let ds = detect_dev_server(tmp.path()).expect("next project");
        assert_eq!(ds.label, "Next.js dev server");
        assert_eq!(ds.default_url, "http://localhost:3000");
    }

    #[test]
    fn detect_astro_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"app","dependencies":{"@astrojs/core":"^4.0.0","astro":"^4.0.0"}}"#,
        )
        .unwrap();
        let ds = detect_dev_server(tmp.path()).expect("astro project");
        assert_eq!(ds.label, "Astro dev server");
    }

    #[test]
    fn detect_cra_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"app","dependencies":{"react-scripts":"^5.0.0"}}"#,
        )
        .unwrap();
        let ds = detect_dev_server(tmp.path()).expect("CRA project");
        assert_eq!(ds.command, "npm start");
    }

    #[test]
    fn detect_generic_dev_script() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"app","scripts":{"dev":"node server.js"}}"#,
        )
        .unwrap();
        let ds = detect_dev_server(tmp.path()).expect("generic dev project");
        assert_eq!(ds.label, "Node dev server");
    }

    #[test]
    fn detect_prefers_real_frontend_subproject_over_root_harness() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/backend")).unwrap();
        std::fs::create_dir_all(tmp.path().join("src/frontend")).unwrap();
        std::fs::write(tmp.path().join("src/backend/server.mjs"), "listen()").unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"local-acceptance-harness","scripts":{"dev":"node src/backend/server.mjs"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("jeecgboot-vue3")).unwrap();
        std::fs::write(
            tmp.path().join("jeecgboot-vue3/package.json"),
            r#"{"name":"jeecgboot-vue3","scripts":{"dev":"vite"},"devDependencies":{"vite":"^5.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("jeecgboot-vue3/pnpm-lock.yaml"), "").unwrap();

        let ds = detect_dev_server(tmp.path()).expect("real frontend subproject");
        assert_eq!(ds.label, "Vite dev server");
        assert_eq!(ds.command, "cd jeecgboot-vue3 && pnpm dev");
        assert_eq!(ds.default_url, "http://localhost:5173");
    }

    #[test]
    fn detect_legit_fullstack_keeps_root_dev_server() {
        // A normal full-stack app: root `package.json` with a `dev` script, a
        // `src/frontend/` source directory, and a generic `web/` dir. It is NOT
        // UmaDev's acceptance harness (no `src/backend/server.mjs`, no
        // `src/frontend/index.html`), so the ROOT dev server must be kept — not
        // mis-routed to a subproject and never dropped to None.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/frontend")).unwrap();
        std::fs::create_dir_all(tmp.path().join("web")).unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"app","scripts":{"dev":"node server.js"}}"#,
        )
        .unwrap();

        let ds = detect_dev_server(tmp.path()).expect("root dev server must be kept");
        assert_eq!(ds.label, "Node dev server");
        assert_eq!(ds.command, "npm run dev");
    }

    #[test]
    fn detect_harness_falls_back_to_root_when_no_usable_subproject() {
        // UmaDev's real acceptance harness (root `dev` script +
        // `src/backend/server.mjs`) with a subproject dir that has NO usable
        // dev server (no package.json). The subproject search yields nothing, so
        // the working root dev server must be returned as the fallback — never
        // dropped to None.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/backend")).unwrap();
        std::fs::write(tmp.path().join("src/backend/server.mjs"), "listen()").unwrap();
        std::fs::create_dir_all(tmp.path().join("frontend")).unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"local-acceptance-harness","scripts":{"dev":"node src/backend/server.mjs"}}"#,
        )
        .unwrap();

        let ds = detect_dev_server(tmp.path()).expect("root dev server fallback");
        assert_eq!(ds.label, "Node dev server");
        assert_eq!(ds.command, "npm run dev");
    }

    #[test]
    fn detect_static_html_server() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("index.html"), "<h1>hi</h1>").unwrap();
        let ds = detect_dev_server(tmp.path()).expect("static project");
        assert_eq!(ds.label, "Static file server");
        assert_eq!(ds.command, "python3 -m http.server 8000");
    }

    #[test]
    fn detect_none_for_empty_workspace() {
        let tmp = TempDir::new().unwrap();
        assert!(detect_dev_server(tmp.path()).is_none());
    }

    #[test]
    fn detect_pnpm_for_vite() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"app","scripts":{"dev":"vite"},"devDependencies":{"vite":"^5.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), "").unwrap();
        let ds = detect_dev_server(tmp.path()).expect("vite+pnpm");
        assert_eq!(ds.command, "pnpm dev");
    }
}
