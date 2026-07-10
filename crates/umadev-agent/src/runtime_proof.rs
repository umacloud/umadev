//! Runtime proof — the engine's "does the app actually RUN?" capability.
//!
//! Plain [`verify`](crate::verify) proves the project *compiles and its tests
//! pass*. That is necessary but not sufficient evidence of a working delivery:
//! a build can be green while the app fails to boot, the dev server crashes on
//! start, or the documented routes 500. This module closes that gap by
//! producing **runtime evidence** — it boots the detected dev server, waits for
//! it to answer, and probes the real routes over HTTP.
//!
//! The flow (all fail-open — any failure degrades to a recorded reason, never a
//! panic or a blocked host):
//!
//! 1. **Detect** the dev-server command via
//!    [`crate::verify::detect_dev_server`] (Vite / Next / Astro / CRA / generic
//!    `dev` script / static-file server).
//! 2. **Reclaim our own leftover**: a dev server this tool spawned on a previous
//!    run and recorded in `.umadev/preview.pid` is killed before booting again,
//!    so it can't keep holding the port. This is CONSERVATIVE — only a PID we
//!    ourselves tracked is ever killed; a foreign process is never touched.
//! 3. **Reuse-or-spawn**: if a server already answers the expected URL after the
//!    reclaim, it is a foreign holder — reuse it (probe it directly, no duplicate
//!    spawn). Otherwise spawn the dev command with its **stdout/stderr piped**.
//! 4. **Bounded boot**: read the child's output for readiness *and* conflict
//!    signals ("Port X in use, using available port Y", "already running",
//!    `EADDRINUSE`) while polling `curl`, all inside one timeout. A port fallback
//!    re-points the probe at the actual bound port; a never-binding boot stops
//!    with a typed diagnosis instead of hanging. Exactly one spawn — never a
//!    re-run loop. `curl` is used deliberately: near-universal, no new dep.
//! 5. **Probe routes**: read `.umadev/contracts/openapi.json` (written by the
//!    contract/adopt stage) and `curl` each documented path, recording
//!    `{path, status, ms}`. With no contract, at least the root path is probed.
//! 6. **Optional e2e**: if a Playwright/Cypress config or a `test:e2e` script is
//!    present, run it once and capture the outcome.
//! 7. **Tear down**: the child is killed (`kill_on_drop` plus an eager kill) and
//!    the pidfile cleared, so this preview server can never become the next
//!    run's leftover.
//!
//! The structured [`RuntimeProof`] is serialized to
//! `.umadev/audit/runtime-proof.json` and folded into the delivery proof-pack
//! (see `phases::build_and_zip_proof_pack`). User-facing prose lives in the
//! binary (which owns the i18n catalog); this crate stays dependency-light and
//! emits machine-readable data plus a neutral one-line summary.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::verify::{detect_dev_server, DevServer};

/// Cap captured e2e / probe-body output so a chatty run can't bloat the JSON.
const CAPTURE_CAP: usize = 8 * 1024;

/// How long (seconds) we wait for the dev server to answer its base URL before
/// giving up. A cold `npm run dev` (install already done) usually answers in a
/// few seconds; we allow generous headroom for slower machines / first builds.
const READY_TIMEOUT_SECS: u64 = 60;

/// Poll interval (milliseconds) while waiting for readiness.
const READY_POLL_MS: u64 = 500;

/// Per-route `curl` timeout (seconds). A live route answers fast; a hang here
/// means the route is effectively down, which is itself a finding.
const PROBE_TIMEOUT_SECS: u64 = 10;

/// Budget (seconds) for the optional e2e step.
const E2E_TIMEOUT_SECS: u64 = 600;

/// How long (seconds) to wait for the expected port to free after we kill our
/// OWN leftover preview server, before spawning a fresh one. Short — the kernel
/// releases a `SIGTERM`ed listener's socket promptly; if it does not free in
/// time, the fresh boot simply falls back to another port (still handled).
const PORT_FREE_WAIT_SECS: u64 = 5;

/// Filename (under `.umadev/`) of the pidfile tracking the dev server THIS tool
/// spawned for the runtime proof. Used to reclaim our own leftover on the next
/// run — never a foreign process.
const PREVIEW_PID_FILE: &str = "preview.pid";

/// Bounded reap after we tear down (or time out) a spawned child, so a wedged
/// `wait()` can't hang the runtime proof.
const TEARDOWN_REAP_SECS: u64 = 5;

/// Cap on RAW captured e2e output held in memory while the suite runs. We retain
/// only the last `OUTPUT_CAP` bytes (the pass/fail summary is at the end) while
/// always draining so the child never blocks. The stored value is capped smaller
/// still, at [`CAPTURE_CAP`].
const OUTPUT_CAP: usize = 256 * 1024;

/// Whether the runtime check ran end-to-end or degraded (and why). This is the
/// top-level verdict the proof-pack and the CLI surface.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "reason")]
pub enum RuntimeStatus {
    /// The dev server booted, answered its base URL, and routes were probed.
    Verified,
    /// The check could not complete; the payload is a short machine reason
    /// (e.g. `"no dev server detected"`, `"curl not found"`,
    /// `"server did not become ready within 60s"`). Fail-open: this is a
    /// neutral "not verified", never an error.
    NotVerified(String),
}

impl RuntimeStatus {
    /// `true` iff the runtime was actually exercised end-to-end.
    #[must_use]
    pub fn is_verified(&self) -> bool {
        matches!(self, RuntimeStatus::Verified)
    }

    /// Stable label for audit rows / display switches.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeStatus::Verified => "verified",
            RuntimeStatus::NotVerified(_) => "not_verified",
        }
    }
}

/// One route probe result: the path we hit, the HTTP status we got, and how
/// long it took. `status` is `0` when `curl` could not get any response at all
/// (connection refused / timeout) — distinct from a real `5xx`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteProbe {
    /// Path probed, relative to the base URL (e.g. `/` or `/api/users`).
    pub path: String,
    /// HTTP status code; `0` means "no response received".
    pub status: u16,
    /// Round-trip wall-clock duration, milliseconds.
    pub ms: u64,
    /// `true` when the status is a non-error response (`< 400`). A `2xx`/`3xx`
    /// proves the route is wired; `4xx` on a contract route (e.g. missing auth)
    /// still proves the server is *up* but is flagged for the reader.
    pub ok: bool,
}

/// The full runtime-proof record. Serialized to
/// `.umadev/audit/runtime-proof.json` and embedded in the proof-pack.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeProof {
    /// ISO-8601 timestamp the check ran.
    pub timestamp: String,
    /// Top-level verdict.
    pub status: RuntimeStatus,
    /// Human label of the dev server we tried (e.g. "Vite dev server"), if one
    /// was detected.
    pub dev_server: Option<String>,
    /// The exact command we spawned, if any.
    pub command: Option<String>,
    /// Base URL we polled / probed against.
    pub base_url: Option<String>,
    /// Milliseconds from spawn until the base URL first answered. `None` when
    /// the server never became ready.
    pub ready_ms: Option<u64>,
    /// Per-route probe results.
    pub routes: Vec<RouteProbe>,
    /// Optional e2e step outcome (`None` when no e2e suite was detected).
    pub e2e: Option<E2eResult>,
}

/// Outcome of the optional e2e suite (Playwright / Cypress / `test:e2e`).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct E2eResult {
    /// The command we ran.
    pub command: String,
    /// `true` iff the suite exited 0.
    pub passed: bool,
    /// Duration, milliseconds.
    pub ms: u64,
    /// Truncated combined output (last words, capped).
    pub output: String,
}

impl RuntimeProof {
    /// Build a "not verified" record carrying only the reason — used on every
    /// fail-open early return so the artifact is still produced.
    fn not_verified(reason: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            status: RuntimeStatus::NotVerified(reason.into()),
            dev_server: None,
            command: None,
            base_url: None,
            ready_ms: None,
            routes: Vec::new(),
            e2e: None,
        }
    }

    /// A neutral, language-agnostic one-line summary (the binary localizes the
    /// real user message; this is for logs / the proof-pack summary file).
    #[must_use]
    pub fn summary_line(&self) -> String {
        match &self.status {
            RuntimeStatus::Verified => {
                let ok = self.routes.iter().filter(|r| r.ok).count();
                let total = self.routes.len();
                let base = self.base_url.as_deref().unwrap_or("(unknown)");
                format!("runtime verified: {base} ready, {ok}/{total} route(s) answered")
            }
            RuntimeStatus::NotVerified(reason) => {
                format!("runtime not verified: {reason}")
            }
        }
    }
}

/// Run the full runtime-proof flow against `workspace`. Always returns a
/// [`RuntimeProof`] — on any failure it degrades to
/// [`RuntimeStatus::NotVerified`] with a reason, never an `Err`/panic. This is
/// the single entry point the CLI / runner call.
pub async fn run_runtime_proof(workspace: &Path) -> RuntimeProof {
    // 0. `curl` is the readiness/probe transport. No curl → cannot verify.
    if !has_curl() {
        return RuntimeProof::not_verified("curl not found on PATH");
    }

    // 1. Detect the dev server command. None → nothing to boot.
    let Some(dev) = detect_dev_server(workspace) else {
        return RuntimeProof::not_verified("no dev server detected");
    };
    let base_url = dev.default_url.to_string();

    // 2. Reclaim OUR OWN leftover preview server first. A dev server we spawned on
    //    a previous run that crashed before teardown would otherwise keep holding
    //    the port and force this boot onto a fallback port — or hang. CONSERVATIVE:
    //    `reclaim_tracked_preview` only kills a PID this tool recorded in its own
    //    pidfile; it never touches a foreign process.
    if reclaim_tracked_preview(workspace) {
        // Give the kernel a moment to release the socket so the fresh spawn binds
        // the expected port instead of falling back.
        wait_until_free(&base_url, PORT_FREE_WAIT_SECS).await;
    }

    // 3. Is a server STILL answering the expected URL? After reclaiming our own,
    //    anything left is a FOREIGN holder we must not kill — reuse it: probe it
    //    directly and do NOT spawn a duplicate (which would only race onto another
    //    port and risk a hang).
    let already_up = curl_status(&base_url, 3).await.is_some();
    if matches!(decide_boot_plan(already_up), BootPlan::Reuse) {
        // reused=true: this is a FOREIGN holder we did not spawn — verify strictly.
        return finish_proof(workspace, &dev, base_url, Some(0), true).await;
    }

    // 4. Spawn the dev server, capturing its output so we can read readiness and
    //    port-conflict signals (the old code discarded output, which is why a port
    //    fallback went unnoticed and the boot hung). The working directory is
    //    RESOLVED + VERIFIED up front (`resolve_spawn_plan`): a `cd <subdir> &&`
    //    prefix becomes an explicit `current_dir`, never a spawned `cd` program
    //    (the recurring Windows "cannot find the path specified"), and a
    //    nonexistent dir fails open here with a reason instead of a raw OS error.
    let plan = match resolve_spawn_plan(&dev.command, workspace) {
        Ok(plan) => plan,
        Err(reason) => {
            let mut proof =
                RuntimeProof::not_verified(format!("failed to start dev server: {reason}"));
            proof.dev_server = Some(dev.label.to_string());
            proof.command = Some(dev.command.clone());
            proof.base_url = Some(base_url);
            return proof;
        }
    };
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args)
        .current_dir(&plan.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Detach the dev server into its OWN session (no controlling terminal) so a
    // descendant that writes straight to /dev/tty — a Spring/Logback console
    // appender, Maven/npm/Docker progress — can't paint over the TUI's
    // alt-screen. Safe: all three stdio streams are piped/null above. Fail-open.
    crate::spawn_util::detach_from_controlling_terminal(&mut cmd);
    let spawn = cmd.spawn();
    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            let mut proof = RuntimeProof::not_verified(format!("failed to start dev server: {e}"));
            proof.dev_server = Some(dev.label.to_string());
            proof.command = Some(dev.command.clone());
            proof.base_url = Some(base_url);
            return proof;
        }
    };
    // Track our own PID so a crash mid-boot can't orphan the dev server: the next
    // run's `reclaim_tracked_preview` will find and kill it.
    if let Some(pid) = child.id() {
        // Record the ACTUAL spawned program (plan.program - resolve_spawn_plan already
        // stripped a `cd <dir> &&` prefix), NOT split_command(&dev.command).0 which for a
        // subdir frontend (`cd web && pnpm dev`) is literally "cd" - reclaim's cmdline match
        // would then never find "cd" in the pnpm/node process and the orphan would survive.
        write_preview_pid(workspace, pid, &plan.program);
    }

    // Drain + scan the child's output on dedicated tasks. Readers stay alive until
    // teardown so the OS pipe never fills (which would stall the server); the
    // channel carries every line to the bounded boot wait.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(out) = child.stdout.take() {
        spawn_line_reader(out, tx.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_line_reader(err, tx);
    }

    // 5. Bounded boot: read output for readiness / port-fallback / already-running
    //    signals AND poll the (possibly re-pointed) base URL, all within one
    //    timeout. NEVER an unbounded wait, and exactly ONE spawn.
    let outcome = wait_for_boot(&mut rx, &base_url, READY_TIMEOUT_SECS).await;
    // Keep draining output (discard) for the rest of the proof so the child never
    // blocks on a full pipe while we probe it.
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let proof = match outcome {
        BootOutcome::Ready {
            base_url: effective,
            ready_ms,
        } => finish_proof(workspace, &dev, effective, Some(ready_ms), false).await,
        BootOutcome::AlreadyRunning { base_url: existing } => {
            // The server reported another instance is already up at a known URL;
            // our spawn is a redundant duplicate. Probe the existing one — but it is
            // a PRE-EXISTING server we did not boot, so verify it strictly (reused=true).
            finish_proof(workspace, &dev, existing, Some(0), true).await
        }
        BootOutcome::Timeout => {
            let mut proof = RuntimeProof::not_verified(boot_timeout_reason(READY_TIMEOUT_SECS));
            proof.dev_server = Some(dev.label.to_string());
            proof.command = Some(dev.command.clone());
            proof.base_url = Some(base_url);
            proof
        }
    };

    // 6. Tear down — kill our spawned dev server AND its whole descendant tree,
    //    then drop the pidfile so this preview can never become the next run's
    //    leftover. The dev command (npm/pnpm) forks node/vite grandchildren that
    //    survive a kill of just the wrapper and keep holding the port; a
    //    process-GROUP kill (the child was spawned detached above) reaps them.
    teardown_child(&mut child).await;
    clear_preview_pid(workspace);

    proof
}

/// Tear down a spawned dev-server child AND its descendant tree, bounded. The
/// child was spawned DETACHED (its own session/process-group via
/// [`crate::spawn_util::detach_from_controlling_terminal`]), so a process-GROUP
/// kill ([`crate::spawn_util::kill_process_group`]) reaps the `npm`/`pnpm`
/// wrapper AND the `node`/`vite` grandchildren it forked — a plain
/// [`tokio::process::Child::start_kill`] would drop only the wrapper and leave
/// the real server holding the port. `start_kill` + `kill_on_drop(true)` are
/// direct-child backstops; the reap is time-bounded so a wedged `wait()` can't
/// hang the runtime proof.
async fn teardown_child(child: &mut tokio::process::Child) {
    let _ = crate::spawn_util::kill_process_group(child);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(TEARDOWN_REAP_SECS), child.wait()).await;
}

/// Whether to reuse an already-running server or spawn our own. Split out so the
/// "a server is already up → reuse it, do NOT spawn a duplicate" policy is unit
/// testable without a real process.
fn decide_boot_plan(expected_url_answers: bool) -> BootPlan {
    if expected_url_answers {
        BootPlan::Reuse
    } else {
        BootPlan::Spawn
    }
}

/// Whether [`run_runtime_proof`] reuses an existing server or spawns its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootPlan {
    /// Spawn the detected dev command.
    Spawn,
    /// A server already answers the expected URL — probe it directly.
    Reuse,
}

/// The typed diagnosis recorded when the dev server never binds within the
/// budget. Names the likely cause (a leftover process on the port) instead of
/// leaving a bare timeout, and is what the CLI surfaces via `runtime.not_verified`.
fn boot_timeout_reason(secs: u64) -> String {
    format!("dev server did not bind within {secs}s — a leftover process may hold the port")
}

/// Run the route-probe + optional e2e steps against a known-good base URL and
/// assemble the verified proof. Shared by the fresh-boot and reuse paths.
async fn finish_proof(
    workspace: &Path,
    dev: &DevServer,
    base_url: String,
    ready_ms: Option<u64>,
    reused: bool,
) -> RuntimeProof {
    let mut proof = RuntimeProof {
        timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        status: RuntimeStatus::Verified,
        dev_server: Some(dev.label.to_string()),
        command: Some(dev.command.clone()),
        base_url: Some(base_url.clone()),
        ready_ms,
        routes: Vec::new(),
        e2e: None,
    };

    // Probe the documented routes (from the contract), else just the root.
    let paths = contract_route_paths(workspace);
    let had_contract = !paths.is_empty();
    let probe_paths = if paths.is_empty() {
        vec!["/".to_string()]
    } else {
        paths
    };
    for path in &probe_paths {
        proof.routes.push(probe_route(&base_url, path).await);
    }

    // Optional e2e suite (run before the verdict so a failing suite can downgrade it).
    proof.e2e = run_e2e_if_present(workspace).await;

    // Decide the verdict from the probe results (pure, unit-tested).
    if let Some(reason) = downgrade_reason(
        &proof.routes,
        reused,
        had_contract,
        &base_url,
        proof.e2e.as_ref(),
    ) {
        proof.status = RuntimeStatus::NotVerified(reason);
    }
    proof
}

/// Decide whether probe results warrant DOWNGRADING the runtime verdict from
/// Verified. Pure + deterministic (no IO) so the verdict policy is unit-tested
/// directly. Returns the `NotVerified` reason, or `None` to keep Verified.
///
/// Precedence, most-specific first:
/// 1. **Foreign reuse (#3)** — a server we did NOT spawn this run (`reused`) that,
///    given real CONTRACT routes, answers NONE of them (every probe 404 /
///    no-response / 5xx) is almost certainly a DIFFERENT app on a colliding
///    default port, not this build. A `401/403/405` still proves the route EXISTS
///    (auth / method), so an auth-gated app the user runs themselves is NOT
///    false-failed — this only fires when every documented route is truly absent.
/// 2. **Booted-but-broken (#6)** — every probe was a `5xx` or no-response. A `4xx`
///    proves the server booted + is routing, so it keeps Verified (downgrading on
///    `!ok` wrongly failed working auth/POST-only backends).
/// 3. **Failed e2e (#5)** — the route probes pass but the e2e suite that RAN came
///    back failing; the headline verdict must not claim "verified". A `None` e2e
///    (no suite detected) keeps the route-level verdict.
fn downgrade_reason(
    routes: &[RouteProbe],
    reused: bool,
    had_contract: bool,
    base_url: &str,
    e2e: Option<&E2eResult>,
) -> Option<String> {
    if reused
        && had_contract
        && !routes.is_empty()
        && routes
            .iter()
            .all(|r| r.status == 0 || r.status == 404 || r.status >= 500)
    {
        return Some(format!(
            "reused an already-running server on {base_url} that answered NONE of the {} \
             documented route(s) — likely a different app on a colliding port, not this build",
            routes.len()
        ));
    }
    if !routes.is_empty() && routes.iter().all(|r| r.status == 0 || r.status >= 500) {
        return Some(
            "the app booted but every probed route returned a 5xx or no response".to_string(),
        );
    }
    if let Some(e2e) = e2e {
        if !e2e.passed {
            return Some(format!(
                "the app booted but its e2e suite failed (`{}`)",
                e2e.command
            ));
        }
    }
    None
}

/// Persist the proof to `.umadev/audit/runtime-proof.json`. Returns the path on
/// success; fail-open (`Err`) is swallowed by callers — a write failure must
/// not block delivery.
pub fn write_runtime_proof(workspace: &Path, proof: &RuntimeProof) -> std::io::Result<PathBuf> {
    let audit_dir = workspace.join(".umadev/audit");
    std::fs::create_dir_all(&audit_dir)?;
    let path = audit_dir.join("runtime-proof.json");
    let body = serde_json::to_string_pretty(proof).unwrap_or_else(|_| "{}".into());
    std::fs::write(&path, body)?;
    Ok(path)
}

/// The canonical location of the runtime-proof artifact relative to the
/// workspace root. Used by the proof-pack assembler so it stays in sync.
#[must_use]
pub fn runtime_proof_rel_path() -> &'static str {
    ".umadev/audit/runtime-proof.json"
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// Whether `curl` is on PATH.
fn has_curl() -> bool {
    which("curl")
}

/// Split a shell-ish command string ("npm run dev") into (program, args).
/// Intentionally simple whitespace split — the dev-server commands we generate
/// in [`detect_dev_server`] never contain quotes or shell operators.
fn split_command(cmd: &str) -> (String, Vec<String>) {
    let mut parts = cmd.split_whitespace().map(str::to_string);
    let program = parts.next().unwrap_or_default();
    (program, parts.collect())
}

/// A resolved, VERIFIED spawn plan for the detected dev-server command: the
/// directory to run it in, the program to spawn, and its args. See
/// [`resolve_spawn_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpawnPlan {
    /// Working directory for the child — canonicalized + proven to exist.
    dir: PathBuf,
    /// Program to spawn (a Windows `.cmd`/`.bat` shim already routed via `cmd /c`).
    program: String,
    /// Program arguments.
    args: Vec<String>,
}

/// Turn a detected dev-server command into an explicit, verified
/// `(working_dir, program, args)` spawn plan.
///
/// [`detect_dev_server`] prefixes a subproject command with `cd <rel> && ` when
/// the frontend lives in a subdirectory (e.g. `cd web && pnpm dev`). Spawning
/// that string as-is is the recurring **Windows** failure: a naive
/// whitespace split makes the *program* the shell builtin `cd`, which
/// `CreateProcess` cannot resolve → "The system cannot find the path
/// specified". Instead we split the target directory OUT and set it as the
/// child's `current_dir` explicitly, so the working dir is real by construction
/// and no nested `cd … &&` (with its fragile quoting, or a scheduled-task /
/// `powershell` detach that silently mis-resolves the path) is ever built. The
/// bare program is routed through [`spawn_parts`] so a Windows npm/pnpm `.cmd`
/// shim runs via `cmd /c`.
///
/// The working directory is `canonicalize`d up front, which both normalizes it
/// and FAILS when it does not exist — turning a would-be raw OS path error at
/// spawn time into an actionable, fail-open `Err(reason)` the caller records as
/// `NotVerified`. Never spawns into an unresolved / nonexistent cwd.
fn resolve_spawn_plan(command: &str, workspace: &Path) -> Result<SpawnPlan, String> {
    let (raw_dir, bare) = split_cd_prefix(command, workspace);
    let dir = std::fs::canonicalize(&raw_dir)
        .map(undecorate)
        .map_err(|e| {
            format!(
                "dev-server working directory `{}` is not accessible: {e}",
                raw_dir.display()
            )
        })?;
    let (program, args) = split_command(bare);
    if program.is_empty() {
        return Err("dev-server command is empty".to_string());
    }
    let (vprog, mut lead) = spawn_parts(&program);
    lead.extend(args);
    Ok(SpawnPlan {
        dir,
        program: vprog,
        args: lead,
    })
}

/// Split a leading `cd <dir> &&` off a run command, resolving `<dir>` against
/// `workspace` (absolute paths kept as-is; quotes stripped). Returns
/// `(working_dir, remaining_command)`. With no `cd` prefix the working dir is
/// `workspace` and the whole (trimmed) command is returned unchanged. Mirrors
/// the TUI's `parse_run_command` so `/preview` and the runtime proof detach the
/// dev server identically.
fn split_cd_prefix<'a>(command: &'a str, workspace: &Path) -> (PathBuf, &'a str) {
    let trimmed = command.trim();
    if let Some(after_cd) = trimmed.strip_prefix("cd ") {
        if let Some((dir, rest)) = after_cd.split_once("&&") {
            let dir = dir.trim().trim_matches(|c| c == '\'' || c == '"');
            let resolved = if Path::new(dir).is_absolute() {
                PathBuf::from(dir)
            } else {
                workspace.join(dir)
            };
            return (resolved, rest.trim());
        }
    }
    (workspace.to_path_buf(), trimmed)
}

/// Strip the Windows verbatim prefix (`\\?\C:\x` → `C:\x`) that `canonicalize`
/// adds. Pure string logic; every non-verbatim path (all unix paths) passes
/// through unchanged. Mirrors the link opener's `undecorate` so a canonicalized
/// `current_dir` stays a plain path.
fn undecorate(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with("UNC") {
            return PathBuf::from(rest);
        }
    }
    p
}

/// Poll `base_url` until it STOPS answering (the port is free) or `budget_secs`
/// elapses. Used after killing our own leftover server so the fresh spawn can
/// bind the expected port instead of falling back.
async fn wait_until_free(base_url: &str, budget_secs: u64) {
    let started = Instant::now();
    let deadline = Duration::from_secs(budget_secs);
    while started.elapsed() < deadline {
        if curl_status(base_url, 2).await.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(READY_POLL_MS)).await;
    }
}

/// The result of the bounded boot wait.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BootOutcome {
    /// The server bound and a `curl` confirmed it answers. Carries the EFFECTIVE
    /// base URL (the port may differ from the default via a fallback) and the
    /// time-to-ready in milliseconds.
    Ready { base_url: String, ready_ms: u64 },
    /// The server reported another instance is already running at a known URL;
    /// the caller should probe that one rather than the duplicate it spawned.
    AlreadyRunning { base_url: String },
    /// Nothing answered within the budget — bounded, never a hang.
    Timeout,
}

/// What a single output line tells us about boot progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineVerdict {
    /// The line announces the server is up (subject to a confirming `curl`).
    Ready,
    /// The line announces another instance already holds the port.
    AlreadyRunning,
}

/// Wait for the dev server to become ready, bounded by `budget_secs`. Reads
/// scanned output lines from `rx` for readiness / port-fallback / already-running
/// signals while polling `curl`. A port fallback re-points the probe at the
/// actually-bound port; a "ready" line is confirmed with a `curl` before we trust
/// it (so a `Verified` proof always means the URL truly answered). Returns
/// [`BootOutcome::Timeout`] when nothing answers in time — never blocks forever.
async fn wait_for_boot(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    base_url: &str,
    budget_secs: u64,
) -> BootOutcome {
    let started = Instant::now();
    let mut effective = base_url.to_string();
    let mut detected_port: Option<u16> = None;

    let deadline = tokio::time::sleep(Duration::from_secs(budget_secs));
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(READY_POLL_MS));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            () = &mut deadline => return BootOutcome::Timeout,
            maybe = rx.recv() => {
                if let Some(line) = maybe {
                    if let Some(verdict) =
                        handle_boot_line(&line, base_url, &mut effective, &mut detected_port)
                    {
                        // Confirm with a probe before trusting a text signal: a
                        // "ready"/"already running" line can precede the socket
                        // actually accepting connections.
                        if curl_status(&effective, 3).await.is_some() {
                            return match verdict {
                                LineVerdict::Ready => BootOutcome::Ready {
                                    base_url: effective,
                                    ready_ms: elapsed_ms(started),
                                },
                                LineVerdict::AlreadyRunning => {
                                    BootOutcome::AlreadyRunning { base_url: effective }
                                }
                            };
                        }
                    }
                } else {
                    // Output is exhausted (the process closed its pipes / exited).
                    // One final probe decides ready-vs-failed; bounded either way.
                    if curl_status(&effective, 3).await.is_some() {
                        return BootOutcome::Ready {
                            base_url: effective,
                            ready_ms: elapsed_ms(started),
                        };
                    }
                    return BootOutcome::Timeout;
                }
            }
            _ = poll.tick() => {
                // Safety net for a server whose ready line we did not recognise:
                // a successful probe on the effective URL is itself readiness.
                if curl_status(&effective, 3).await.is_some() {
                    return BootOutcome::Ready {
                        base_url: effective,
                        ready_ms: elapsed_ms(started),
                    };
                }
            }
        }
    }
}

/// Milliseconds elapsed since `started`, saturating.
fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

/// Apply one scanned output line to the boot state. Updates `effective` (the URL
/// we will probe) when the port changes, and returns a [`LineVerdict`] when the
/// line is decisive. Pure (no I/O) so it is unit-testable.
fn handle_boot_line(
    line: &str,
    base_url: &str,
    effective: &mut String,
    detected_port: &mut Option<u16>,
) -> Option<LineVerdict> {
    match scan_dev_line(line)? {
        // The chosen port was busy; the server fell back to `port`. Re-point the
        // probe there — that is the port our spawned server actually bound.
        DevSignal::PortFallback(port) => {
            *detected_port = Some(port);
            *effective = replace_port(base_url, port);
            None
        }
        // Readiness with an explicit port (e.g. a "Local: http://…:PORT" line).
        DevSignal::Ready(Some(port)) => {
            *detected_port = Some(port);
            *effective = replace_port(base_url, port);
            Some(LineVerdict::Ready)
        }
        // Readiness without a port — use whatever port we already detected.
        DevSignal::Ready(None) => {
            if let Some(port) = *detected_port {
                *effective = replace_port(base_url, port);
            }
            Some(LineVerdict::Ready)
        }
        // Another instance is already running at a KNOWN port → reuse it.
        DevSignal::Conflict(Some(port)) => {
            *effective = replace_port(base_url, port);
            Some(LineVerdict::AlreadyRunning)
        }
        // A conflict with no usable port (e.g. "already running … PID: 7928") is
        // informational; the matching port-fallback line carries the real port.
        DevSignal::Conflict(None) => None,
    }
}

/// A signal parsed from one line of dev-server output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DevSignal {
    /// The server announced it is ready/listening; carries the bound port when
    /// the line revealed one.
    Ready(Option<u16>),
    /// The chosen port was busy and the server fell back to this other port
    /// ("Port 3000 is in use … using available port 3002 instead").
    PortFallback(u16),
    /// Another server is already running / the port is taken at bind time
    /// ("Another … server is already running", `EADDRINUSE`); carries a port when
    /// the line named one.
    Conflict(Option<u16>),
}

/// Scan ONE line of dev-server output for a readiness or conflict signal.
/// Case-insensitive and pure so it is unit-testable without spawning anything.
/// Precedence: an explicit port fallback first (most actionable), then conflict
/// markers, then readiness — so a "using available port Y" line is read as a
/// port change rather than a bare "in use" conflict.
fn scan_dev_line(line: &str) -> Option<DevSignal> {
    let lower = line.to_ascii_lowercase();

    // Port fallback: "...using available port 3002 instead" (Next.js).
    const FALLBACK: &str = "using available port";
    if let Some(idx) = lower.find(FALLBACK) {
        if let Some(port) = parse_uint_after(&lower, idx + FALLBACK.len()) {
            return Some(DevSignal::PortFallback(port));
        }
    }

    // Bind-time conflict ("Port 5173 is in use …", `EADDRINUSE`, "already running").
    // A fallback port named on the same line ("…on port 3002") still wins.
    let is_conflict = lower.contains("is in use")
        || lower.contains("already running")
        || lower.contains("eaddrinuse")
        || lower.contains("address already in use");
    if is_conflict {
        const ON_PORT: &str = "on port ";
        if let Some(idx) = lower.find(ON_PORT) {
            if let Some(port) = parse_uint_after(&lower, idx + ON_PORT.len()) {
                return Some(DevSignal::PortFallback(port));
            }
        }
        return Some(DevSignal::Conflict(port_from_url_in(&lower)));
    }

    // Readiness signals — strongest (a Local: URL with a port) first.
    if lower.contains("local:") {
        return Some(DevSignal::Ready(port_from_url_in(&lower)));
    }
    if lower.contains("ready") || lower.contains("started server") {
        return Some(DevSignal::Ready(ready_port(&lower)));
    }
    if let Some(idx) = lower.find("listening on") {
        let port = port_from_url_in(&lower)
            .or_else(|| parse_uint_after(&lower, idx + "listening on".len()));
        return Some(DevSignal::Ready(port));
    }
    if lower.contains("running at") || lower.contains("server running") {
        return Some(DevSignal::Ready(port_from_url_in(&lower)));
    }
    None
}

/// Best-effort port from a readiness line: a URL port, else the integer after
/// the word "port".
fn ready_port(lower: &str) -> Option<u16> {
    port_from_url_in(lower).or_else(|| {
        lower
            .find("port ")
            .and_then(|i| parse_uint_after(lower, i + "port ".len()))
    })
}

/// Parse the first base-10 `u16` appearing in `s` at or after byte index `from`.
fn parse_uint_after(s: &str, from: usize) -> Option<u16> {
    let bytes = s.as_bytes();
    let mut i = from.min(bytes.len());
    while i < bytes.len() && !bytes[i].is_ascii_digit() {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if start == i {
        return None;
    }
    s[start..i].parse().ok()
}

/// Extract the port from the first `http(s)://host:PORT` URL in `s`, if any.
fn port_from_url_in(s: &str) -> Option<u16> {
    let lower = s.to_ascii_lowercase();
    let scheme = lower.find("://")?;
    let after = scheme + 3;
    let rest = &lower[after..];
    // Host runs to the next ':' (port), '/' (path) or end.
    let colon = rest.find(':')?;
    // A '/' before the ':' means the ':' is not the port separator.
    if let Some(slash) = rest.find('/') {
        if slash < colon {
            return None;
        }
    }
    parse_uint_after(rest, colon + 1)
}

/// Replace (or insert) the port in a base URL like `http://localhost:3000`,
/// preserving the scheme, host and any trailing path.
fn replace_port(base_url: &str, port: u16) -> String {
    let Some(scheme_end) = base_url.find("://") else {
        return base_url.to_string();
    };
    let after = scheme_end + 3;
    let rest = &base_url[after..];
    let host_end = rest.find([':', '/']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    let tail = &rest[host_end..];
    // `tail` is ':oldport/path', '/path', or '' — keep only the path part.
    let path = if let Some(stripped) = tail.strip_prefix(':') {
        stripped.trim_start_matches(|c: char| c.is_ascii_digit())
    } else {
        tail
    };
    format!("{}://{host}:{port}{path}", &base_url[..scheme_end])
}

/// Spawn a detached task that reads `reader` line by line and forwards each line
/// to `tx`, until EOF or the receiver is gone. Drains the pipe so the child never
/// stalls on a full output buffer.
fn spawn_line_reader<R>(reader: R, tx: tokio::sync::mpsc::UnboundedSender<String>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// preview-pid tracking + conservative reclaim of our OWN leftover dev server
// ---------------------------------------------------------------------------

/// Path of the pidfile tracking the dev server THIS tool spawned for the proof.
fn preview_pid_path(workspace: &Path) -> PathBuf {
    workspace.join(".umadev").join(PREVIEW_PID_FILE)
}

/// Record `pid` as our live preview server. Fail-open: a write error is ignored
/// (the pidfile is a best-effort cleanup aid, never a correctness dependency).
fn write_preview_pid(workspace: &Path, pid: u32, program: &str) {
    let path = preview_pid_path(workspace);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Record the spawned PROGRAM name alongside the pid so reclaim can verify the live
    // process is still OURS (not a foreign process the OS handed the recycled pid to).
    let _ = std::fs::write(path, format!("{pid}\n{program}"));
}

/// Read the tracked preview `(pid, program)`, if a valid non-zero pid is recorded. The
/// program line may be absent in a legacy pidfile (then it is empty -> reclaim stays
/// conservative and does not kill).
fn read_preview_pid(workspace: &Path) -> Option<(u32, String)> {
    let body = std::fs::read_to_string(preview_pid_path(workspace)).ok()?;
    let mut lines = body.lines();
    let pid = lines
        .next()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|p| *p != 0)?;
    let program = lines.next().unwrap_or("").trim().to_string();
    Some((pid, program))
}

/// Remove the pidfile (best-effort).
fn clear_preview_pid(workspace: &Path) {
    let _ = std::fs::remove_file(preview_pid_path(workspace));
}

/// Reclaim UmaDev's OWN previously-spawned preview server if it is still alive.
/// Returns `true` iff it killed one. CONSERVATIVE by design: it only ever targets
/// a PID this tool itself recorded in [`preview_pid_path`], and only when that PID
/// is confirmed alive — a foreign process is never killed, and an unknown-liveness
/// PID is left running. The pidfile is cleared either way.
fn reclaim_tracked_preview(workspace: &Path) -> bool {
    let Some((pid, program)) = read_preview_pid(workspace) else {
        return false;
    };
    // Kill only a LIVE pid whose current command line still contains the PROGRAM we
    // recorded. A crash can leave a stale pidfile; if the OS recycled that pid to an
    // UNRELATED process, its cmdline won't match, so we leave it alone (leaking a stray
    // process is far better than SIGTERM-ing the user's editor/browser on a recycled pid).
    // An empty recorded program (legacy pidfile) or an unreadable cmdline -> do NOT kill.
    let killed = if !program.is_empty()
        && pid_is_alive(pid) == Some(true)
        && pid_cmdline_contains(pid, &program)
    {
        kill_pid(pid);
        true
    } else {
        false
    };
    clear_preview_pid(workspace);
    killed
}

/// Best-effort: does the LIVE process at `pid` have `needle` in its command line? Used to
/// confirm a tracked pid is still the process we spawned (vs a recycled-pid foreigner).
/// Conservative: unreadable -> `false` (do NOT kill). Unix: `ps`; other platforms: `false`.
#[cfg(unix)]
fn pid_cmdline_contains(pid: u32, needle: &str) -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .arg("-o")
        .arg("command=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).contains(needle)
}

#[cfg(not(unix))]
fn pid_cmdline_contains(_pid: u32, _needle: &str) -> bool {
    false
}

/// `Some(true)` alive, `Some(false)` provably gone, `None` could-not-determine.
/// Dependency-free (mirrors the run-lock helper, kept local so this module stays
/// self-contained). Unix: `kill -0`. Windows: `tasklist`.
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> Option<bool> {
    let out = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if out.status.success() {
        return Some(true);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    if stderr.contains("no such process")
        || stderr.contains("illegal")
        || stderr.contains("invalid")
    {
        Some(false)
    } else if stderr.contains("not permitted") || stderr.contains("permission") {
        Some(true)
    } else {
        None
    }
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> Option<bool> {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
    if stdout.contains("no tasks") {
        Some(false)
    } else if stdout.contains(&format!("\"{pid}\"")) || stdout.contains(&pid.to_string()) {
        Some(true)
    } else {
        Some(false)
    }
}

#[cfg(not(any(unix, windows)))]
fn pid_is_alive(_pid: u32) -> Option<bool> {
    None
}

/// Terminate `pid` (best-effort). Unix: `kill` (SIGTERM — dev servers handle it
/// and free the port). Windows: `taskkill /F /T`. Errors are ignored; the caller
/// only kills PIDs it has confirmed it owns.
#[cfg(unix)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .output();
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F", "/T"])
        .output();
}

#[cfg(not(any(unix, windows)))]
fn kill_pid(_pid: u32) {}

/// Probe one route: `curl` `base + path`, recording status + duration.
async fn probe_route(base_url: &str, path: &str) -> RouteProbe {
    let url = join_url(base_url, path);
    let started = Instant::now();
    let status = curl_status(&url, PROBE_TIMEOUT_SECS).await.unwrap_or(0);
    let ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    RouteProbe {
        path: path.to_string(),
        status,
        ms,
        ok: status != 0 && status < 400,
    }
}

/// Run `curl -s -o /dev/null -w "%{http_code}" --max-time <secs> <url>` and
/// parse the printed status code. Returns `None` when curl can't connect (exit
/// non-zero, or a `000` status — curl's "no response" sentinel).
async fn curl_status(url: &str, max_time_secs: u64) -> Option<u16> {
    let null_sink = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let out = Command::new("curl")
        .arg("-s")
        .arg("-o")
        .arg(null_sink)
        .arg("-w")
        .arg("%{http_code}")
        .arg("--max-time")
        .arg(max_time_secs.to_string())
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let code = String::from_utf8_lossy(&out.stdout);
    parse_http_code(code.trim())
}

/// Parse curl's `%{http_code}` output. `000` is curl's "no response" sentinel →
/// `None`. A real `2xx`–`5xx` → `Some(code)`.
fn parse_http_code(s: &str) -> Option<u16> {
    let code: u16 = s.parse().ok()?;
    if code == 0 {
        None
    } else {
        Some(code)
    }
}

/// Join a base URL and a path without doubling or dropping the `/`.
fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if path.is_empty() {
        return base.to_string();
    }
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// Read the route paths from the adopt/contract-stage `openapi.json` in
/// `.umadev/contracts/`. Returns a de-duplicated, ordered list of paths. An
/// absent / malformed contract yields an empty list (caller falls back to `/`).
///
/// Templated path segments (`{id}`, `:id`) are substituted with a placeholder
/// so the probe hits a concrete URL rather than a literal `{id}` that would
/// always 404.
fn contract_route_paths(workspace: &Path) -> Vec<String> {
    let openapi = workspace.join(".umadev/contracts/openapi.json");
    let Ok(body) = std::fs::read_to_string(&openapi) else {
        return Vec::new();
    };
    parse_openapi_paths(&body)
}

/// Pure parse of an OpenAPI JSON document's `paths` keys. Split out from disk
/// I/O so it's unit-testable. Only `GET`-safe probing is intended, but here we
/// just collect the path keys; the prober treats every path as a read.
fn parse_openapi_paths(body: &str) -> Vec<String> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(paths) = doc.get("paths").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for key in paths.keys() {
        let concrete = concretize_path(key);
        if !out.contains(&concrete) {
            out.push(concrete);
        }
    }
    out
}

/// Replace templated path params with a concrete placeholder so a probe lands
/// on a real handler instead of a literal `{id}` / `:id` (which 404s).
fn concretize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for seg in path.split('/') {
        if seg.is_empty() {
            continue;
        }
        out.push('/');
        if (seg.starts_with('{') && seg.ends_with('}')) || seg.starts_with(':') {
            out.push('1');
        } else {
            out.push_str(seg);
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Detect + run an e2e suite once. Returns `None` when no suite is present.
/// Detection order: a `test:e2e` npm script → Playwright config → Cypress
/// config. Fail-open: a missing runner binary records a `passed:false` outcome
/// rather than erroring.
async fn run_e2e_if_present(workspace: &Path) -> Option<E2eResult> {
    let cmd = detect_e2e_command(workspace)?;
    let (program, args) = split_command(&cmd);
    let (vprog, vlead) = spawn_parts(&program);
    let started = Instant::now();

    let mut ecmd = Command::new(vprog);
    ecmd.args(&vlead)
        .args(&args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Detach into its OWN session/process-group so a timeout can kill the WHOLE
    // e2e tree — Playwright/Cypress fork browser processes that survive a kill of
    // just the runner. Safe: stdin null, stdout/stderr piped. Fail-open.
    crate::spawn_util::detach_from_controlling_terminal(&mut ecmd);
    let mut child = match ecmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Some(E2eResult {
                command: cmd,
                passed: false,
                ms: elapsed_ms(started),
                output: format!("failed to spawn e2e runner: {e}"),
            });
        }
    };

    // Capped-tail readers: bound memory on a chatty suite while always draining
    // so the child never blocks on a full pipe.
    let stdout_task = child
        .stdout
        .take()
        .map(|h| tokio::spawn(read_capped_tail(h, OUTPUT_CAP)));
    let stderr_task = child
        .stderr
        .take()
        .map(|h| tokio::spawn(read_capped_tail(h, OUTPUT_CAP)));

    let wait_result =
        tokio::time::timeout(Duration::from_secs(E2E_TIMEOUT_SECS), child.wait()).await;
    if wait_result.is_err() {
        // Timed out: kill the WHOLE group so browser descendants die too, not just
        // the runner (dropping the `Child` alone would leave them running).
        // Bounded reap so a wedged wait() can't hang.
        let _ = crate::spawn_util::kill_process_group(&child);
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(TEARDOWN_REAP_SECS), child.wait()).await;
    }

    let raw_stdout = join_capped(stdout_task).await;
    let raw_stderr = join_capped(stderr_task).await;
    let mut combined = String::from_utf8_lossy(&raw_stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&raw_stderr));
    truncate(&mut combined, CAPTURE_CAP);
    let ms = elapsed_ms(started);

    match wait_result {
        Ok(Ok(status)) => Some(E2eResult {
            command: cmd,
            passed: status.success(),
            ms,
            output: combined,
        }),
        Ok(Err(e)) => Some(E2eResult {
            command: cmd,
            passed: false,
            ms,
            output: format!("failed to spawn e2e runner: {e}"),
        }),
        Err(_) => {
            // Keep the killed suite's last words alongside the timeout marker.
            let marker = format!("e2e timed out after {E2E_TIMEOUT_SECS}s");
            let output = if combined.trim().is_empty() {
                marker
            } else {
                let mut o = combined;
                o.push_str(&format!("\n...[{marker}]"));
                truncate(&mut o, CAPTURE_CAP);
                o
            };
            Some(E2eResult {
                command: cmd,
                passed: false,
                ms,
                output,
            })
        }
    }
}

/// Read `reader` to EOF, retaining only the LAST `cap` bytes (the e2e pass/fail
/// summary lives at the end) while always draining so the child never blocks on
/// a full pipe. Memory is bounded to `2*cap` between trims, so a huge stream
/// costs O(total), not O(total²). Mirrors the deploy module's private helper.
async fn read_capped_tail<R>(mut reader: R, cap: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if cap > 0 && buf.len() > cap.saturating_mul(2) {
                    let drop_to = buf.len() - cap;
                    buf.drain(..drop_to);
                }
            }
        }
    }
    if cap > 0 && buf.len() > cap {
        let drop_to = buf.len() - cap;
        buf.drain(..drop_to);
    }
    buf
}

/// Join a capped-tail reader task, BOUNDED so a wedged descendant that still
/// holds a pipe open after a kill can't hang us. Fail-open: a missing task or a
/// panic yields an empty buffer.
async fn join_capped(task: Option<tokio::task::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    match task {
        Some(t) => match tokio::time::timeout(Duration::from_secs(TEARDOWN_REAP_SECS), t).await {
            Ok(Ok(buf)) => buf,
            Ok(Err(_)) | Err(_) => Vec::new(),
        },
        None => Vec::new(),
    }
}

/// Decide the e2e command for `workspace`, or `None` if no e2e setup is found.
/// Pure-ish (only reads files) so it's testable via a temp dir.
fn detect_e2e_command(workspace: &Path) -> Option<String> {
    // 1. An explicit `test:e2e` script wins — it's what the project author meant.
    if package_json_has_script(workspace, "test:e2e") {
        return Some("npm run test:e2e".to_string());
    }
    // 2. Playwright config (any of the conventional names).
    for name in [
        "playwright.config.ts",
        "playwright.config.js",
        "playwright.config.mjs",
    ] {
        if workspace.join(name).is_file() {
            return Some("npx playwright test".to_string());
        }
    }
    // 3. Cypress config.
    for name in ["cypress.config.ts", "cypress.config.js"] {
        if workspace.join(name).is_file() {
            return Some("npx cypress run".to_string());
        }
    }
    None
}

/// Whether `package.json` declares a given script. Local copy (the verify
/// module's is private) — kept tiny on purpose.
fn package_json_has_script(workspace: &Path, script: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(workspace.join("package.json")) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    json.get("scripts").and_then(|s| s.get(script)).is_some()
}

/// Truncate a captured buffer at a char boundary, appending a marker.
fn truncate(s: &mut String, cap: usize) {
    if s.len() > cap {
        let mut idx = cap;
        while !s.is_char_boundary(idx) {
            idx -= 1;
        }
        s.truncate(idx);
        s.push_str("\n...[truncated]");
    }
}

/// Whether a PATH-resolvable binary exists. Mirrors the verify module's helper
/// (kept local so this module doesn't widen verify's surface). Honours
/// `PATHEXT` on Windows.
fn which(bin: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    let separator = if cfg!(windows) { ';' } else { ':' };
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

/// Resolve a bare program name to a spawnable path on Windows (npm shims are
/// `.cmd`/`.bat` that `Command::new` won't find), routing `.cmd`/`.bat` through
/// `cmd /c`. No-op off Windows. Returns `(program, leading_args)`. Mirrors the
/// verify module's private helper so dev-server spawn behaves the same.
fn spawn_parts(program: &str) -> (String, Vec<String>) {
    if !cfg!(windows) || program.contains(std::path::is_separator) {
        return (program.to_string(), Vec::new());
    }
    let Ok(path_var) = std::env::var("PATH") else {
        return (program.to_string(), Vec::new());
    };
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    for dir in path_var.split(';') {
        if dir.is_empty() {
            continue;
        }
        for ext in std::iter::once("").chain(pathext.split(';')) {
            let candidate = Path::new(dir).join(format!("{program}{ext}"));
            if candidate.is_file() {
                let resolved = candidate.to_string_lossy().into_owned();
                let lower_ext = ext.to_ascii_lowercase();
                if lower_ext == ".cmd" || lower_ext == ".bat" {
                    return ("cmd".to_string(), vec!["/c".to_string(), resolved]);
                }
                return (resolved, Vec::new());
            }
        }
    }
    (program.to_string(), Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_http_code_treats_000_as_no_response() {
        assert_eq!(parse_http_code("000"), None);
        assert_eq!(parse_http_code("0"), None);
        assert_eq!(parse_http_code("200"), Some(200));
        assert_eq!(parse_http_code("404"), Some(404));
        assert_eq!(parse_http_code("503"), Some(503));
        assert_eq!(parse_http_code(""), None);
        assert_eq!(parse_http_code("not-a-number"), None);
    }

    #[test]
    fn join_url_handles_slashes() {
        assert_eq!(join_url("http://x:3000", "/"), "http://x:3000/");
        assert_eq!(join_url("http://x:3000/", "/api"), "http://x:3000/api");
        assert_eq!(join_url("http://x:3000", "api"), "http://x:3000/api");
        assert_eq!(join_url("http://x:3000/", ""), "http://x:3000");
        assert_eq!(
            join_url("http://x:3000", "/api/users"),
            "http://x:3000/api/users"
        );
    }

    #[test]
    fn split_command_splits_program_and_args() {
        assert_eq!(
            split_command("npm run dev"),
            (
                "npm".to_string(),
                vec!["run".to_string(), "dev".to_string()]
            )
        );
        assert_eq!(
            split_command("python3 -m http.server 8000"),
            (
                "python3".to_string(),
                vec!["-m".into(), "http.server".into(), "8000".into()]
            )
        );
        assert_eq!(split_command(""), (String::new(), Vec::new()));
    }

    #[test]
    fn split_cd_prefix_splits_dir_and_strips_operator() {
        let root = Path::new("/workspace");

        // Relative subdir → resolved against workspace; the `cd … &&` glue is gone.
        let (dir, rest) = split_cd_prefix("cd web && pnpm dev", root);
        assert_eq!(dir, root.join("web"));
        assert_eq!(rest, "pnpm dev");
        assert!(!rest.contains("cd") && !rest.contains("&&"));

        // Quoted dir with a space survives, un-quoted.
        let (dir, rest) = split_cd_prefix("cd 'my app' && npm run dev", root);
        assert_eq!(dir, root.join("my app"));
        assert_eq!(rest, "npm run dev");

        // No `cd` prefix → workspace root, command returned unchanged (trimmed).
        let (dir, rest) = split_cd_prefix("  npm run dev  ", root);
        assert_eq!(dir, root.to_path_buf());
        assert_eq!(rest, "npm run dev");

        // Absolute target dir is kept as-is (unix-only: `/abs` is not absolute on
        // Windows, which needs a drive letter).
        #[cfg(unix)]
        {
            let (dir, rest) = split_cd_prefix("cd /abs/app && pnpm dev", root);
            assert_eq!(dir, PathBuf::from("/abs/app"));
            assert_eq!(rest, "pnpm dev");
        }
    }

    #[test]
    fn resolve_spawn_plan_cd_prefix_runs_in_subdir_never_spawns_cd() {
        // The recurring Windows bug: `detect_dev_server` returns
        // `cd <subdir> && <cmd>` for a subproject frontend. The old naive split
        // spawned a program literally named `cd` (a shell builtin, not an exe) in
        // the WRONG cwd → "The system cannot find the path specified". The plan
        // must instead set the subdir as `current_dir` and spawn the real program.
        let tmp = TempDir::new().unwrap();
        let web = tmp.path().join("web");
        fs::create_dir_all(&web).unwrap();

        let plan =
            resolve_spawn_plan("cd web && pnpm dev", tmp.path()).expect("subdir exists → resolves");

        // cwd is the SUBDIR, resolved + set explicitly — not the workspace root.
        assert_eq!(plan.dir, undecorate(web.canonicalize().unwrap()));

        // The program is NEVER the shell builtin `cd`, and no fragile shell glue
        // (a `&&`, a nested `cd`, a schtasks/powershell scheduled-task detach)
        // leaks into the argv — the exact failure shapes we must not build.
        assert_ne!(plan.program, "cd");
        let argv: Vec<String> = std::iter::once(plan.program.clone())
            .chain(plan.args.iter().cloned())
            .collect();
        assert!(!argv.iter().any(|t| t == "cd"), "no `cd` program: {argv:?}");
        assert!(!argv.iter().any(|t| t == "&&"), "no shell op: {argv:?}");
        assert!(
            !argv.iter().any(|t| t.contains("schtasks")),
            "no scheduled task: {argv:?}"
        );
        // The real run command survived intact (its last token is the run arg).
        assert_eq!(plan.args.last().map(String::as_str), Some("dev"));
    }

    #[test]
    fn resolve_spawn_plan_no_cd_runs_in_workspace_root() {
        let tmp = TempDir::new().unwrap();
        let plan = resolve_spawn_plan("npm run dev", tmp.path()).expect("workspace exists");

        assert_eq!(plan.dir, undecorate(tmp.path().canonicalize().unwrap()));
        assert_ne!(plan.program, "cd");
        // The run tokens are preserved in order after any (possibly empty)
        // `spawn_parts` lead — off Windows / when no `.cmd` shim resolves the lead
        // is empty and program is `npm`; on Windows with an `npm.cmd` shim the lead
        // is `cmd /c <npm.cmd>` — either way `run`/`dev` are still there, in order.
        assert!(plan.args.iter().any(|a| a == "run"), "{:?}", plan.args);
        assert_eq!(plan.args.last().map(String::as_str), Some("dev"));
    }

    #[test]
    fn resolve_spawn_plan_missing_dir_fails_open_with_reason() {
        let tmp = TempDir::new().unwrap();
        // `nope/` does not exist: resolve up front with an actionable reason rather
        // than spawning into a bad cwd and surfacing a raw OS path error.
        let err = resolve_spawn_plan("cd nope && npm run dev", tmp.path())
            .expect_err("missing dir must fail open");
        assert!(err.contains("nope"), "reason names the dir: {err}");
        assert!(
            err.to_lowercase().contains("not accessible"),
            "actionable reason: {err}"
        );
    }

    #[test]
    fn undecorate_strips_windows_verbatim_prefix_only() {
        // Non-verbatim paths (every unix path) pass through untouched.
        assert_eq!(
            undecorate(PathBuf::from("/plain/path")),
            PathBuf::from("/plain/path")
        );
        // A `\\?\C:\x` verbatim prefix is stripped; a `\\?\UNC\...` share is kept.
        assert_eq!(
            undecorate(PathBuf::from(r"\\?\C:\proj\web")),
            PathBuf::from(r"C:\proj\web")
        );
        assert_eq!(
            undecorate(PathBuf::from(r"\\?\UNC\server\share")),
            PathBuf::from(r"\\?\UNC\server\share")
        );
    }

    #[test]
    fn concretize_path_substitutes_templates() {
        assert_eq!(concretize_path("/"), "/");
        assert_eq!(concretize_path("/api/users"), "/api/users");
        assert_eq!(concretize_path("/api/users/{id}"), "/api/users/1");
        assert_eq!(concretize_path("/api/users/:id"), "/api/users/1");
        assert_eq!(concretize_path("/api/{org}/repos/:repo"), "/api/1/repos/1");
    }

    #[test]
    fn parse_openapi_paths_extracts_and_dedups() {
        let doc = r#"{
            "openapi": "3.1.0",
            "paths": {
                "/api/users": { "get": {} },
                "/api/users/{id}": { "get": {} },
                "/health": { "get": {} }
            }
        }"#;
        let paths = parse_openapi_paths(doc);
        assert!(paths.contains(&"/api/users".to_string()));
        assert!(paths.contains(&"/api/users/1".to_string()));
        assert!(paths.contains(&"/health".to_string()));
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn parse_openapi_paths_handles_garbage_and_missing() {
        assert!(parse_openapi_paths("not json").is_empty());
        assert!(parse_openapi_paths("{}").is_empty());
        assert!(parse_openapi_paths(r#"{"paths": "not-an-object"}"#).is_empty());
        assert!(parse_openapi_paths(r#"{"paths": {}}"#).is_empty());
    }

    #[test]
    fn contract_route_paths_reads_from_disk() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev/contracts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("openapi.json"),
            r#"{"paths":{"/api/ping":{"get":{}}}}"#,
        )
        .unwrap();
        let paths = contract_route_paths(tmp.path());
        assert_eq!(paths, vec!["/api/ping".to_string()]);
    }

    #[test]
    fn contract_route_paths_empty_when_no_contract() {
        let tmp = TempDir::new().unwrap();
        assert!(contract_route_paths(tmp.path()).is_empty());
    }

    #[test]
    fn detect_e2e_prefers_test_e2e_script() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts":{"test:e2e":"playwright test"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_e2e_command(tmp.path()),
            Some("npm run test:e2e".to_string())
        );
    }

    #[test]
    fn detect_e2e_finds_playwright_config() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("playwright.config.ts"), "export default {}").unwrap();
        assert_eq!(
            detect_e2e_command(tmp.path()),
            Some("npx playwright test".to_string())
        );
    }

    #[test]
    fn detect_e2e_finds_cypress_config() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("cypress.config.js"), "module.exports = {}").unwrap();
        assert_eq!(
            detect_e2e_command(tmp.path()),
            Some("npx cypress run".to_string())
        );
    }

    #[test]
    fn detect_e2e_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        assert!(detect_e2e_command(tmp.path()).is_none());
    }

    #[test]
    fn status_helpers() {
        assert!(RuntimeStatus::Verified.is_verified());
        assert!(!RuntimeStatus::NotVerified("x".into()).is_verified());
        assert_eq!(RuntimeStatus::Verified.as_str(), "verified");
        assert_eq!(
            RuntimeStatus::NotVerified("x".into()).as_str(),
            "not_verified"
        );
    }

    #[test]
    fn not_verified_summary_includes_reason() {
        let p = RuntimeProof::not_verified("no dev server detected");
        assert!(p.summary_line().contains("no dev server detected"));
        assert!(!p.status.is_verified());
        assert!(p.routes.is_empty());
        assert!(p.ready_ms.is_none());
    }

    #[test]
    fn verified_summary_counts_ok_routes() {
        let proof = RuntimeProof {
            timestamp: "2026-06-22T00:00:00Z".into(),
            status: RuntimeStatus::Verified,
            dev_server: Some("Vite dev server".into()),
            command: Some("npm run dev".into()),
            base_url: Some("http://localhost:5173".into()),
            ready_ms: Some(1200),
            routes: vec![
                RouteProbe {
                    path: "/".into(),
                    status: 200,
                    ms: 12,
                    ok: true,
                },
                RouteProbe {
                    path: "/api/users".into(),
                    status: 500,
                    ms: 30,
                    ok: false,
                },
            ],
            e2e: None,
        };
        let line = proof.summary_line();
        assert!(line.contains("1/2 route(s) answered"), "line was: {line}");
        assert!(line.contains("http://localhost:5173"));
    }

    #[test]
    fn write_runtime_proof_serializes_json() {
        let tmp = TempDir::new().unwrap();
        let proof = RuntimeProof::not_verified("curl not found on PATH");
        let path = write_runtime_proof(tmp.path(), &proof).unwrap();
        assert_eq!(path, tmp.path().join(".umadev/audit/runtime-proof.json"));
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("not_verified"));
        assert!(body.contains("curl not found on PATH"));
        // Round-trips back to the same struct.
        let parsed: RuntimeProof = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, proof);
    }

    #[test]
    fn rel_path_matches_write_location() {
        let tmp = TempDir::new().unwrap();
        let proof = RuntimeProof::not_verified("x");
        let written = write_runtime_proof(tmp.path(), &proof).unwrap();
        let expected = tmp.path().join(runtime_proof_rel_path());
        assert_eq!(written, expected);
    }

    #[tokio::test]
    async fn run_runtime_proof_no_dev_server_is_not_verified() {
        // An empty workspace has no dev server → fail-open "not verified",
        // never a panic. (If curl is missing on the CI box, the reason differs
        // but it's still NotVerified — assert on the negative, not the text.)
        let tmp = TempDir::new().unwrap();
        let proof = run_runtime_proof(tmp.path()).await;
        assert!(!proof.status.is_verified());
        assert!(proof.routes.is_empty());
    }

    #[tokio::test]
    async fn probe_route_unreachable_is_status_zero() {
        // Probing a port nothing listens on yields status 0 (no response), ok=false.
        // Skip when curl is unavailable (the function would early-return None upstream).
        if !has_curl() {
            return;
        }
        let probe = probe_route("http://127.0.0.1:1", "/").await;
        assert_eq!(probe.status, 0);
        assert!(!probe.ok);
        assert_eq!(probe.path, "/");
    }

    #[test]
    fn truncate_keeps_char_boundary() {
        let mut s = "做做做做做".to_string();
        truncate(&mut s, 7);
        assert!(s.ends_with("[truncated]"));
        let _ = s.as_bytes(); // valid UTF-8, no panic
    }

    // -------------------------------------------------------------------
    // conflict / port-fallback detection (the leftover-process boot fix)
    // -------------------------------------------------------------------

    #[test]
    fn scan_detects_next_port_fallback() {
        // "Port X in use, using available port Y" → probe Y.
        let line = "⚠ Port 3000 is in use by process 7928, using available port 3002 instead.";
        assert_eq!(scan_dev_line(line), Some(DevSignal::PortFallback(3002)));
    }

    #[test]
    fn scan_detects_already_running_without_port() {
        // The "another server is already running … PID: 7928" line names a PID,
        // not a port → informational conflict, not a bogus port.
        let line = "⚠ Another next dev server is already running on this machine. PID: 7928";
        assert_eq!(scan_dev_line(line), Some(DevSignal::Conflict(None)));
    }

    #[test]
    fn scan_detects_already_running_with_url_port() {
        let line = "Server already running at http://localhost:3000";
        assert_eq!(scan_dev_line(line), Some(DevSignal::Conflict(Some(3000))));
    }

    #[test]
    fn scan_detects_local_url_ready() {
        assert_eq!(
            scan_dev_line("  - Local:        http://localhost:3002"),
            Some(DevSignal::Ready(Some(3002)))
        );
        assert_eq!(
            scan_dev_line("  ➜  Local:   http://localhost:5174/"),
            Some(DevSignal::Ready(Some(5174)))
        );
    }

    #[test]
    fn scan_detects_ready_without_port() {
        assert_eq!(
            scan_dev_line("✓ Ready in 1798ms"),
            Some(DevSignal::Ready(None))
        );
    }

    #[test]
    fn scan_detects_listening_and_addr_in_use() {
        assert_eq!(
            scan_dev_line("Server listening on port 4000"),
            Some(DevSignal::Ready(Some(4000)))
        );
        // Vite's busy-port notice carries no fallback on the same line.
        assert_eq!(
            scan_dev_line("Port 5173 is in use, trying another one..."),
            Some(DevSignal::Conflict(None))
        );
        // A bare EADDRINUSE has no URL → informational conflict.
        assert_eq!(
            scan_dev_line("Error: listen EADDRINUSE: address already in use :::3000"),
            Some(DevSignal::Conflict(None))
        );
    }

    #[test]
    fn scan_ignores_unremarkable_lines() {
        assert_eq!(scan_dev_line("info  - Loaded env from .env"), None);
        assert_eq!(scan_dev_line(""), None);
    }

    #[test]
    fn handle_port_fallback_repoints_probe_url() {
        let mut effective = "http://localhost:3000".to_string();
        let mut detected = None;
        let v = handle_boot_line(
            "Port 3000 is in use by process 7928, using available port 3002 instead.",
            "http://localhost:3000",
            &mut effective,
            &mut detected,
        );
        assert_eq!(v, None); // a fallback is not yet readiness
        assert_eq!(effective, "http://localhost:3002"); // probe the bound port, not 3000
        assert_eq!(detected, Some(3002));
    }

    #[test]
    fn handle_local_line_is_ready_on_new_port() {
        let mut effective = "http://localhost:3000".to_string();
        let mut detected = None;
        let v = handle_boot_line(
            "- Local: http://localhost:3002",
            "http://localhost:3000",
            &mut effective,
            &mut detected,
        );
        assert_eq!(v, Some(LineVerdict::Ready));
        assert_eq!(effective, "http://localhost:3002");
    }

    #[test]
    fn handle_ready_without_port_uses_detected_fallback() {
        let mut effective = "http://localhost:3000".to_string();
        let mut detected = Some(3002);
        let v = handle_boot_line(
            "✓ Ready in 2s",
            "http://localhost:3000",
            &mut effective,
            &mut detected,
        );
        assert_eq!(v, Some(LineVerdict::Ready));
        assert_eq!(effective, "http://localhost:3002");
    }

    #[test]
    fn handle_conflict_without_port_is_noop() {
        let mut effective = "http://localhost:3000".to_string();
        let mut detected = None;
        let v = handle_boot_line(
            "Another dev server is already running PID: 7928",
            "http://localhost:3000",
            &mut effective,
            &mut detected,
        );
        assert_eq!(v, None);
        assert_eq!(effective, "http://localhost:3000"); // untouched
    }

    #[test]
    fn parse_uint_after_reads_first_integer() {
        assert_eq!(parse_uint_after("port 3002 instead", 0), Some(3002));
        assert_eq!(parse_uint_after("abc", 0), None);
        assert_eq!(parse_uint_after("x999y", 1), Some(999));
        assert_eq!(parse_uint_after("", 5), None);
    }

    #[test]
    fn port_from_url_extracts_port() {
        assert_eq!(port_from_url_in("http://localhost:3002/"), Some(3002));
        assert_eq!(port_from_url_in("see https://127.0.0.1:8080/x"), Some(8080));
        assert_eq!(port_from_url_in("http://localhost/"), None); // no port
        assert_eq!(port_from_url_in("no url here"), None);
    }

    #[test]
    fn replace_port_rewrites_host_port() {
        assert_eq!(
            replace_port("http://localhost:3000", 3002),
            "http://localhost:3002"
        );
        assert_eq!(
            replace_port("http://localhost:3000/", 3002),
            "http://localhost:3002/"
        );
        assert_eq!(
            replace_port("http://localhost", 4000),
            "http://localhost:4000"
        );
        assert_eq!(
            replace_port("http://127.0.0.1:5173/app", 5174),
            "http://127.0.0.1:5174/app"
        );
    }

    #[test]
    fn decide_boot_plan_reuses_when_already_up() {
        // A server already answering the expected URL → reuse it, do NOT spawn a
        // duplicate.
        assert_eq!(decide_boot_plan(true), BootPlan::Reuse);
        assert_eq!(decide_boot_plan(false), BootPlan::Spawn);
    }

    fn probe(path: &str, status: u16) -> RouteProbe {
        RouteProbe {
            path: path.to_string(),
            status,
            ms: 1,
            ok: status < 400,
        }
    }

    #[test]
    fn downgrade_reused_foreign_server_that_answers_no_contract_route() {
        // HIGH #3: a reused server (not spawned this run) that 404s every documented
        // contract route is a different app on a colliding port — NOT verified.
        let routes = vec![probe("/api/users", 404), probe("/api/health", 404)];
        let r = downgrade_reason(&routes, true, true, "http://localhost:3000", None);
        assert!(r.unwrap().contains("colliding port"));
        // But a reused server that DOES answer a route (or auth-gates it) stays verified:
        let auth = vec![probe("/api/users", 401), probe("/api/health", 200)];
        assert!(downgrade_reason(&auth, true, true, "http://localhost:3000", None).is_none());
        // An auth-gated app where EVERY route is 401 still proves routes exist → verified.
        let all_auth = vec![probe("/api/users", 401), probe("/api/admin", 403)];
        assert!(downgrade_reason(&all_auth, true, true, "http://localhost:3000", None).is_none());
        // The SAME 404s on a server WE spawned (reused=false) are not the foreign case —
        // a 404 keeps Verified (route-quirk tolerance), only 5xx/no-response downgrades.
        assert!(downgrade_reason(&routes, false, true, "http://localhost:3000", None).is_none());
    }

    #[test]
    fn downgrade_booted_but_broken_all_5xx() {
        // #6: every route 5xx / no-response → booted-but-broken, NotVerified.
        let routes = vec![probe("/", 500), probe("/api", 0)];
        assert!(downgrade_reason(&routes, false, false, "u", None)
            .unwrap()
            .contains("5xx or no response"));
        // A single 4xx keeps Verified (server is up + routing).
        let ok = vec![probe("/", 500), probe("/api", 401)];
        assert!(downgrade_reason(&ok, false, false, "u", None).is_none());
    }

    #[test]
    fn downgrade_when_e2e_suite_failed() {
        // MED #5: route probes pass but a RAN e2e suite failed → NotVerified.
        let routes = vec![probe("/", 200)];
        let failed = E2eResult {
            command: "npm run test:e2e".to_string(),
            passed: false,
            ms: 10,
            output: String::new(),
        };
        let r = downgrade_reason(&routes, false, true, "u", Some(&failed));
        assert!(r.unwrap().contains("e2e suite failed"));
        // A passing suite keeps Verified; None (no suite) keeps Verified.
        let passed = E2eResult {
            passed: true,
            ..failed.clone()
        };
        assert!(downgrade_reason(&routes, false, true, "u", Some(&passed)).is_none());
        assert!(downgrade_reason(&routes, false, true, "u", None).is_none());
    }

    #[test]
    fn boot_timeout_reason_names_the_leftover_cause() {
        let r = boot_timeout_reason(60);
        assert!(r.contains("did not bind"), "reason was: {r}");
        assert!(r.contains("leftover"), "reason was: {r}");
        assert!(r.contains("60s"), "reason was: {r}");
    }

    // -------------------------------------------------------------------
    // preview-pid tracking + conservative reclaim
    // -------------------------------------------------------------------

    #[test]
    fn preview_pid_roundtrips() {
        let tmp = TempDir::new().unwrap();
        assert!(read_preview_pid(tmp.path()).is_none());
        write_preview_pid(tmp.path(), 4242, "sleep");
        assert_eq!(
            read_preview_pid(tmp.path()),
            Some((4242, "sleep".to_string()))
        );
        clear_preview_pid(tmp.path());
        assert!(read_preview_pid(tmp.path()).is_none());
    }

    #[test]
    fn read_preview_pid_rejects_zero_and_garbage() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        fs::write(preview_pid_path(tmp.path()), "0").unwrap();
        assert!(read_preview_pid(tmp.path()).is_none());
        fs::write(preview_pid_path(tmp.path()), "not-a-pid").unwrap();
        assert!(read_preview_pid(tmp.path()).is_none());
    }

    #[test]
    fn reclaim_no_pidfile_is_noop() {
        let tmp = TempDir::new().unwrap();
        assert!(!reclaim_tracked_preview(tmp.path()));
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_kills_our_own_tracked_alive_pid() {
        use std::os::unix::process::ExitStatusExt;
        let tmp = TempDir::new().unwrap();
        // A live process we DID record in our own pidfile = ours to clean up.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        write_preview_pid(tmp.path(), pid, "sleep");
        assert_eq!(pid_is_alive(pid), Some(true));

        let killed = reclaim_tracked_preview(tmp.path());
        assert!(killed, "our own alive tracked pid should be reclaimed");
        assert!(
            read_preview_pid(tmp.path()).is_none(),
            "pidfile must be cleared after reclaim"
        );
        // It really died (by signal), and the wait() reaps it.
        let status = child.wait().unwrap();
        assert!(
            status.signal().is_some(),
            "the tracked dev server must be killed by a signal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_does_not_kill_foreign_process() {
        let tmp = TempDir::new().unwrap();
        // A live process we did NOT record = foreign. Reclaim must never touch it.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();

        let killed = reclaim_tracked_preview(tmp.path());
        assert!(!killed);
        assert_eq!(
            pid_is_alive(pid),
            Some(true),
            "a foreign port holder must be left alive"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_clears_dead_pid_without_killing() {
        let tmp = TempDir::new().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("0")
            .spawn()
            .unwrap();
        let pid = child.id();
        let _ = child.wait(); // reap → the PID is now gone
        write_preview_pid(tmp.path(), pid, "sleep");

        let killed = reclaim_tracked_preview(tmp.path());
        assert!(!killed, "a dead tracked pid is not a kill");
        assert!(read_preview_pid(tmp.path()).is_none());
    }

    // -------------------------------------------------------------------
    // bounded boot wait (never a hang)
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn wait_for_boot_times_out_without_binding() {
        // Output channel stays open but silent and nothing binds → must return a
        // bounded Timeout, NOT hang. Independent of whether curl exists.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let start = std::time::Instant::now();
        let outcome = wait_for_boot(&mut rx, "http://127.0.0.1:1", 1).await;
        assert_eq!(outcome, BootOutcome::Timeout);
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "boot wait must be bounded, not a multi-minute hang"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn wait_for_boot_times_out_when_output_closes_with_no_server() {
        // Sender dropped → channel closed → final probe fails → bounded Timeout.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        drop(tx);
        let outcome = wait_for_boot(&mut rx, "http://127.0.0.1:1", 60).await;
        assert_eq!(outcome, BootOutcome::Timeout);
    }

    // -------------------------------------------------------------------
    // teardown kills the WHOLE process group (npm/pnpm → node/vite descendants)
    // -------------------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn teardown_child_kills_the_whole_group() {
        // A wrapper that prints a BACKGROUNDED grandchild's PID, then lingers. The
        // grandchild shares the wrapper's process group (the wrapper is a group
        // leader via the detach), standing in for the node/vite server an
        // `npm run dev` forks. teardown_child must reap BOTH via a group kill — a
        // plain start_kill would leave the grandchild alive, holding the port.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 300 & echo $!; sleep 300")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::spawn_util::detach_from_controlling_terminal(&mut cmd);
        let mut child = cmd.spawn().expect("sh should spawn");

        // Read the backgrounded grandchild's PID from the wrapper's stdout.
        let gpid = {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let out = child.stdout.take().expect("piped stdout");
            let mut lines = BufReader::new(out).lines();
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .expect("grandchild pid line");
            line.trim().parse::<u32>().expect("a pid")
        };
        assert_eq!(
            pid_is_alive(gpid),
            Some(true),
            "grandchild must be alive before teardown"
        );

        teardown_child(&mut child).await;

        // The group kill reaps the grandchild too; poll briefly for the
        // reparent+reap so the PID is provably gone (not just a zombie).
        let mut gone = false;
        for _ in 0..40 {
            if pid_is_alive(gpid) == Some(false) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(gone, "group teardown must reap the backgrounded grandchild");
    }

    #[tokio::test]
    async fn read_capped_tail_keeps_the_last_cap_bytes() {
        // MORE than `cap` bytes in → only the last `cap` kept (the tail, where the
        // e2e summary lives), bounding memory.
        let data = vec![b'z'; 10_000];
        let cap = 1_000;
        let got = read_capped_tail(&data[..], cap).await;
        assert_eq!(got.len(), cap, "keeps exactly the last cap bytes");
        assert!(got.iter().all(|&b| b == b'z'));
    }

    #[tokio::test]
    async fn read_capped_tail_returns_everything_when_under_cap() {
        let got = read_capped_tail(&b"tiny"[..], 1_000).await;
        assert_eq!(got, b"tiny");
    }
}
