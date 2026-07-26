//! Deploy adapter — the engine's "ship it" capability, the optional handoff
//! step that closes the commercial-delivery loop *after* `delivery`.
//!
//! `delivery` produces the proof-pack (docs + quality + compliance + a runnable
//! build). What it does NOT do is put the product on a public URL. This module
//! is the bridge to "it's live": it
//!
//! 1. **Detects** the deploy target from the workspace's own files — a
//!    `vercel.json` / Next.js app → Vercel, `netlify.toml` → Netlify, a
//!    `Dockerfile` → a container image, `fly.toml` → Fly.io, a built static
//!    `dist/` / `out/` → a static host. Each target carries the exact CLI
//!    command a user would run.
//! 2. **Executes** that command as a subprocess **only when the user explicitly
//!    triggers a deploy** (the binary/TUI gates this behind a confirm). The
//!    actual deploy is the *user's* action against *their own* logged-in
//!    platform CLI — UmaDev never deploys on its own, owns no credentials, and
//!    injects nothing into the platform.
//! 3. Captures the **preview URL + log tail** into a structured [`DeployProof`]
//!    that is serialized to `.umadev/audit/deploy-proof.json` and folded into
//!    the delivery proof-pack (see `phases::build_and_zip_proof_pack`).
//!
//! Everything here is **fail-open**: an unrecognised platform, a missing deploy
//! CLI, or a failed/timed-out deploy degrades to a `NotDeployed(reason)` record
//! with a manual-deploy hint — never a panic, never a blocked host. User-facing
//! prose lives in the binary (which owns the i18n catalog); this crate stays
//! dependency-light and emits machine-readable data plus a neutral summary line.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Cap captured deploy-log output so a chatty CLI can't bloat the JSON.
const CAPTURE_CAP: usize = 8 * 1024;

/// How long (seconds) a deploy is allowed to run before we abort and record a
/// timeout. A first deploy that builds remotely can be slow; this is a generous
/// backstop so a hung interactive login can't block forever.
const DEPLOY_TIMEOUT_SECS: u64 = 600;

/// Cap on RAW captured deploy output held in memory while the command runs. A
/// chatty deploy (e.g. a verbose `docker build`) can print far more than we keep;
/// we retain only the last `OUTPUT_CAP` bytes (the result / URL / error lives at
/// the end) while ALWAYS draining the pipe so the child never blocks. The final
/// stored `log_tail` is capped smaller still, at [`CAPTURE_CAP`].
const OUTPUT_CAP: usize = 256 * 1024;
const MAX_DEPLOY_MANIFEST_BYTES: usize = 2 * 1024 * 1024;

/// Bounded reap / pipe-reader grace after a deploy tree is terminated.
const KILL_REAP_SECS: u64 = 5;

/// A recognised deployment platform. Detected purely from files already in the
/// workspace; each variant maps to a single canonical CLI command.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployTarget {
    /// `vercel.json` present, or a Next.js app (`next` dependency / `next.config.*`).
    Vercel,
    /// `netlify.toml` present.
    Netlify,
    /// `fly.toml` present (Fly.io).
    Fly,
    /// A Cloudflare Pages/Workers project (`wrangler.toml` / `wrangler.json`).
    CloudflarePages,
    /// A `Dockerfile` present — container image build (no auto-push target, so
    /// the command just builds the image; pushing is the user's choice).
    Docker,
    /// A pre-built static bundle with no platform config — deployable to any static host
    /// via a generic CLI. Carries the DETECTED output dir (`dist`/`out`/`build`/`public`) so
    /// the deploy command targets the real bundle, not a hardcoded `./dist`.
    StaticHost(StaticDir),
    /// No recognised target. Deploy is skipped (fail-open).
    None,
}

/// The detected static-bundle output dir for [`DeployTarget::StaticHost`]. A small enum
/// (not a `&'static str`) so `DeployTarget` stays `Copy` AND round-trips through serde (a
/// borrowed `&'static str` field can't be deserialized).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StaticDir {
    /// `dist/`
    Dist,
    /// `out/` (e.g. a Next.js static export)
    Out,
    /// `build/` (e.g. Create React App)
    Build,
    /// `public/`
    Public,
}

impl StaticDir {
    /// The on-disk directory name.
    #[must_use]
    pub const fn as_dir(self) -> &'static str {
        match self {
            Self::Dist => "dist",
            Self::Out => "out",
            Self::Build => "build",
            Self::Public => "public",
        }
    }
}

impl DeployTarget {
    /// Stable string label used in proof rows and events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vercel => "vercel",
            Self::Netlify => "netlify",
            Self::Fly => "fly",
            Self::CloudflarePages => "cloudflare-pages",
            Self::Docker => "docker",
            Self::StaticHost(_) => "static-host",
            Self::None => "none",
        }
    }

    /// Human-friendly platform name for display.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Vercel => "Vercel",
            Self::Netlify => "Netlify",
            Self::Fly => "Fly.io",
            Self::CloudflarePages => "Cloudflare Pages",
            Self::Docker => "Docker image",
            Self::StaticHost(_) => "static host",
            Self::None => "none",
        }
    }

    /// The CLI binary this target deploys through (the thing that must be on
    /// PATH). `None` for [`DeployTarget::None`].
    #[must_use]
    pub const fn cli_binary(self) -> Option<&'static str> {
        match self {
            Self::Vercel => Some("vercel"),
            Self::Netlify => Some("netlify"),
            Self::Fly => Some("flyctl"),
            Self::CloudflarePages => Some("wrangler"),
            Self::Docker => Some("docker"),
            Self::StaticHost(_) => Some("npx"),
            Self::None => None,
        }
    }

    /// The exact, copy-pasteable deploy command for this target. `None` for
    /// [`DeployTarget::None`]. These are the production-deploy forms a user runs
    /// against their *own* logged-in CLI; UmaDev only surfaces / runs them.
    #[must_use]
    pub fn deploy_command(self) -> Option<String> {
        let cmd = match self {
            Self::Vercel => "npx vercel --prod --yes",
            Self::Netlify => "npx netlify deploy --prod",
            Self::Fly => "flyctl deploy",
            // Cloudflare Pages: deploy the built output dir; `dist` is wrangler's
            // own default convention for Pages projects.
            Self::CloudflarePages => "npx wrangler pages deploy dist",
            // Docker: build the image. Pushing/running is the user's choice — we
            // do not assume a registry. Tag is a stable local name.
            Self::Docker => "docker build -t app:latest .",
            // Static host: a zero-config global deploy of the DETECTED built bundle dir
            // (dist/out/build/public) via a widely-available static-deploy CLI - NOT a
            // hardcoded ./dist, which would ship a Next.js out/ or CRA build/ wrong.
            Self::StaticHost(dir) => return Some(format!("npx surge ./{}", dir.as_dir())),
            Self::None => return None,
        };
        Some(cmd.to_string())
    }
}

/// Whether the deploy ran to completion (and a URL was captured) or degraded
/// (and why). The top-level verdict the proof-pack surfaces.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "reason")]
pub enum DeployStatus {
    /// The deploy command exited 0.
    Deployed,
    /// The deploy did not happen / did not succeed; the payload is a short
    /// machine reason (e.g. `"no deploy target detected"`, `"vercel not on
    /// PATH"`, `"deploy command exited 1"`, `"timed out after 600s"`).
    /// Fail-open: this is a neutral "not deployed", never an error.
    NotDeployed(String),
}

impl DeployStatus {
    /// `true` iff the deploy completed successfully.
    #[must_use]
    pub fn is_deployed(&self) -> bool {
        matches!(self, DeployStatus::Deployed)
    }

    /// Stable label for proof rows / display switches.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            DeployStatus::Deployed => "deployed",
            DeployStatus::NotDeployed(_) => "not_deployed",
        }
    }
}

/// The full deploy-proof record. Serialized to
/// `.umadev/audit/deploy-proof.json` and embedded in the proof-pack.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployProof {
    /// ISO-8601 timestamp the deploy ran (or was attempted).
    pub timestamp: String,
    /// Detected platform.
    pub platform: DeployTarget,
    /// Top-level verdict.
    pub status: DeployStatus,
    /// The exact command we ran, if any (`None` when no target was detected).
    pub command: Option<String>,
    /// Subprocess exit code; `-1` for spawn / timeout failures, `None` when we
    /// never ran a command.
    pub exit_code: Option<i32>,
    /// The live / preview URL parsed from the deploy output, if one was printed.
    pub url: Option<String>,
    /// Wall-clock duration of the deploy, milliseconds (`None` when nothing ran).
    pub duration_ms: Option<u64>,
    /// Truncated tail of the deploy log (stdout+stderr, capped at 8 KiB).
    pub log_tail: String,
}

impl DeployProof {
    /// Build a "not deployed" record carrying only the platform + reason — used
    /// on every fail-open early return so the artifact is still produced.
    fn not_deployed(platform: DeployTarget, reason: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            platform,
            status: DeployStatus::NotDeployed(reason.into()),
            command: platform.deploy_command(),
            exit_code: None,
            url: None,
            duration_ms: None,
            log_tail: String::new(),
        }
    }

    /// A neutral, language-agnostic one-line summary (the binary localizes the
    /// real user message; this is for logs / the proof-pack summary).
    #[must_use]
    pub fn summary_line(&self) -> String {
        match &self.status {
            DeployStatus::Deployed => {
                let url = self.url.as_deref().unwrap_or("(no URL printed)");
                format!("deployed to {}: {url}", self.platform.label())
            }
            DeployStatus::NotDeployed(reason) => {
                format!("not deployed ({}): {reason}", self.platform.label())
            }
        }
    }
}

/// Detect the deploy target from the workspace's files. Pure file-presence /
/// manifest inspection — no network, no spawning. Order is by specificity:
/// an explicit platform config wins over a generic Dockerfile, which wins over
/// a bare built bundle.
#[must_use]
pub fn detect_deploy_target(workspace: &Path) -> DeployTarget {
    // 1. Explicit platform configs (most specific).
    if crate::bounded_fs::is_real_file_beneath(workspace, &workspace.join("vercel.json"))
        || is_next_app(workspace)
    {
        return DeployTarget::Vercel;
    }
    if crate::bounded_fs::is_real_file_beneath(workspace, &workspace.join("netlify.toml")) {
        return DeployTarget::Netlify;
    }
    if crate::bounded_fs::is_real_file_beneath(workspace, &workspace.join("fly.toml")) {
        return DeployTarget::Fly;
    }
    if crate::bounded_fs::is_real_file_beneath(workspace, &workspace.join("wrangler.toml"))
        || crate::bounded_fs::is_real_file_beneath(workspace, &workspace.join("wrangler.json"))
    {
        return DeployTarget::CloudflarePages;
    }
    // 2. A Dockerfile — container build (no platform config above it).
    if crate::bounded_fs::is_real_file_beneath(workspace, &workspace.join("Dockerfile")) {
        return DeployTarget::Docker;
    }
    // 3. A pre-built static bundle with no platform config — any static host.
    for (name, dir) in [
        ("dist", StaticDir::Dist),
        ("out", StaticDir::Out),
        ("build", StaticDir::Build),
        ("public", StaticDir::Public),
    ] {
        if crate::bounded_fs::is_real_directory_beneath(workspace, &workspace.join(name)) {
            return DeployTarget::StaticHost(dir);
        }
    }
    DeployTarget::None
}

/// Whether the workspace is a Next.js app (which deploys to Vercel even without
/// a `vercel.json`). Detected by a `next.config.*` file or a `next` dependency.
fn is_next_app(workspace: &Path) -> bool {
    for cfg in ["next.config.js", "next.config.mjs", "next.config.ts"] {
        if crate::bounded_fs::is_real_file_beneath(workspace, &workspace.join(cfg)) {
            return true;
        }
    }
    package_json_depends_on(workspace, "next")
}

/// Whether `package.json` declares a dependency on `pkg` (in `dependencies` /
/// `devDependencies`). Best-effort; a missing / malformed manifest → `false`.
fn package_json_depends_on(workspace: &Path, pkg: &str) -> bool {
    let Ok(content) = crate::bounded_fs::read_utf8_beneath(
        workspace,
        &workspace.join("package.json"),
        MAX_DEPLOY_MANIFEST_BYTES,
    ) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    let in_obj = |key: &str| {
        json.get(key)
            .and_then(|v| v.as_object())
            .is_some_and(|o| o.contains_key(pkg))
    };
    in_obj("dependencies") || in_obj("devDependencies")
}

/// Run a deploy against `workspace`. **Caller must have obtained explicit user
/// consent** — this spawns a real, outward-facing command. Always returns a
/// [`DeployProof`]; on any failure it degrades to [`DeployStatus::NotDeployed`]
/// with a reason, never an `Err`/panic (fail-open).
///
/// `command` is the exact command to run. When `None`, the canonical command
/// for the detected platform is used; when no platform is detected, a
/// `NotDeployed("no deploy target detected")` record is returned without
/// spawning anything.
///
/// stdin is `/dev/null`: a deploy CLI that needs an interactive login must
/// fail fast on EOF rather than hang invisibly. The login is the user's job in
/// their own terminal; this adapter records the outcome of a non-interactive
/// attempt.
pub async fn run_deploy(workspace: &Path, command: Option<&str>) -> DeployProof {
    let platform = detect_deploy_target(workspace);
    let command = match command {
        Some(c) if !c.trim().is_empty() => c.trim().to_string(),
        _ => match platform.deploy_command() {
            Some(c) => c,
            None => return DeployProof::not_deployed(platform, "no deploy target detected"),
        },
    };

    // The first token is the binary that must be on PATH (e.g. `npx`, `docker`,
    // `flyctl`). If it's missing, record a neutral skip + manual hint.
    let bin = command.split_whitespace().next().unwrap_or_default();
    if !bin.is_empty() && !which(bin) {
        return DeployProof::not_deployed(platform, format!("{bin} not found on PATH"));
    }

    run_deploy_command(workspace, platform, command, DEPLOY_TIMEOUT_SECS).await
}

/// Spawn + drive one deploy command against `workspace`, racing its exit against
/// `timeout_secs`. Always returns a [`DeployProof`] — fail-open, never hangs.
///
/// The shared detached-command runner continuously drains both pipes into
/// fixed-size tails and owns the full Unix process group / Windows Job Object.
/// It tears down descendants on success, failure, timeout, or caller
/// cancellation, and aborts then reaps pipe readers within a fixed grace.
async fn run_deploy_command(
    workspace: &Path,
    platform: DeployTarget,
    command: String,
    timeout_secs: u64,
) -> DeployProof {
    let started = Instant::now();
    // Run through `sh -c` (Unix) / `cmd /c` (Windows) so multi-token commands
    // like `npx vercel --prod --yes` execute as written.
    let (shell, shell_arg) = if cfg!(windows) {
        ("cmd", "/c")
    } else {
        ("sh", "-c")
    };
    let mut dcmd = Command::new(shell);
    dcmd.arg(shell_arg).arg(&command).current_dir(workspace);
    let output = match umadev_process::run_bounded_detached_command(
        dcmd,
        umadev_process::BoundedCommandOptions {
            timeout: Duration::from_secs(timeout_secs),
            stdout_bytes: OUTPUT_CAP / 2,
            stderr_bytes: OUTPUT_CAP / 2,
            reader_grace: Duration::from_secs(KILL_REAP_SECS),
        },
    )
    .await
    {
        Ok(output) => output,
        Err(e) => {
            let mut proof =
                DeployProof::not_deployed(platform, format!("could not run deploy command: {e}"));
            proof.command = Some(command);
            proof.exit_code = Some(-1);
            return proof;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    if output.timed_out {
        let mut proof =
            DeployProof::not_deployed(platform, format!("timed out after {timeout_secs}s"));
        proof.command = Some(command);
        proof.exit_code = Some(-1);
        proof.duration_ms = Some(timeout_secs.saturating_mul(1000));
        proof.log_tail = log_tail(&stdout, &stderr);
        return proof;
    }

    if let Some(status) = output.status {
        let exit = status.code().unwrap_or(-1);
        // Many deploy CLIs print the live URL on stdout; some on stderr.
        let url = extract_url(&stdout).or_else(|| extract_url(&stderr));
        let log_tail = log_tail(&stdout, &stderr);
        if status.success() {
            DeployProof {
                timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                platform,
                status: DeployStatus::Deployed,
                command: Some(command),
                exit_code: Some(exit),
                url,
                duration_ms: Some(ms),
                log_tail,
            }
        } else {
            DeployProof {
                timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                platform,
                status: DeployStatus::NotDeployed(format!("deploy command exited {exit}")),
                command: Some(command),
                exit_code: Some(exit),
                url,
                duration_ms: Some(ms),
                log_tail,
            }
        }
    } else {
        let mut proof = DeployProof::not_deployed(
            platform,
            "deploy command exited without a status".to_string(),
        );
        proof.command = Some(command);
        proof.exit_code = Some(-1);
        proof.log_tail = log_tail(&stdout, &stderr);
        proof
    }
}

/// Persist the proof to `.umadev/audit/deploy-proof.json`. Returns the path on
/// success; a write failure is fail-open (callers swallow the `Err`) — it must
/// not block delivery.
pub fn write_deploy_proof(workspace: &Path, proof: &DeployProof) -> std::io::Result<PathBuf> {
    let audit_dir = workspace.join(".umadev/audit");
    std::fs::create_dir_all(&audit_dir)?;
    let path = audit_dir.join("deploy-proof.json");
    let body = serde_json::to_string_pretty(proof).unwrap_or_else(|_| "{}".into());
    std::fs::write(&path, body)?;
    Ok(path)
}

/// The canonical location of the deploy-proof artifact relative to the
/// workspace root. Used by the proof-pack assembler so it stays in sync.
#[must_use]
pub fn deploy_proof_rel_path() -> &'static str {
    ".umadev/audit/deploy-proof.json"
}

// ---------------------------------------------------------------------------
// internals — pure, unit-tested
// ---------------------------------------------------------------------------

/// Pull the first `http(s)://…` URL out of deploy output. Deploy CLIs print the
/// live URL on a line; we take the first well-formed one. Trailing punctuation
/// / quotes / ANSI-ish trailers are trimmed so the captured URL is clickable.
fn extract_url(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(idx) = line.find("https://").or_else(|| line.find("http://")) {
            let rest = &line[idx..];
            // Stop at the first whitespace; trim trailing noise punctuation.
            let token = rest.split_whitespace().next().unwrap_or(rest);
            let cleaned = token.trim_end_matches(['.', ',', ')', ']', '"', '\'', '`', '>']);
            if cleaned.len() > "https://".len() {
                return Some(cleaned.to_string());
            }
        }
    }
    None
}

/// Build the capped log tail from stdout + stderr. Keeps the *end* of the
/// combined output (where the result / error / URL lives), capped at
/// [`CAPTURE_CAP`] on a char boundary.
fn log_tail(stdout: &str, stderr: &str) -> String {
    let mut combined = String::new();
    if !stdout.trim().is_empty() {
        combined.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(stderr.trim_end());
    }
    tail_capped(&combined, CAPTURE_CAP)
}

/// Keep the last `cap` bytes of `s`, trimmed to a char boundary, prefixed with
/// a marker when truncation happened.
fn tail_capped(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("...[truncated]\n{}", &s[start..])
}

/// Check whether a PATH-resolvable binary exists. Splits `PATH` on the
/// platform-native separator and honours `PATHEXT` on Windows so `which("npx")`
/// finds `npx.cmd`. Mirrors the verify/runtime-proof helpers.
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
            if Path::new(dir).join(format!("{bin}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn detect_vercel_via_config() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("vercel.json"), "{}").unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::Vercel);
    }

    #[test]
    fn detect_vercel_via_next_dependency() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"app","dependencies":{"next":"^14.0.0"}}"#,
        )
        .unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::Vercel);
    }

    #[test]
    fn oversized_package_json_cannot_invent_a_next_deploy_target() {
        let tmp = TempDir::new().unwrap();
        fs::File::create(tmp.path().join("package.json"))
            .unwrap()
            .set_len(u64::try_from(MAX_DEPLOY_MANIFEST_BYTES + 1).unwrap())
            .unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::None);
    }

    #[cfg(unix)]
    #[test]
    fn linked_deploy_markers_are_ignored() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("vercel.json"), "{}").unwrap();
        fs::create_dir(outside.path().join("dist")).unwrap();
        symlink(
            outside.path().join("vercel.json"),
            tmp.path().join("vercel.json"),
        )
        .unwrap();
        symlink(outside.path().join("dist"), tmp.path().join("dist")).unwrap();

        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::None);
    }

    #[test]
    fn detect_vercel_via_next_config() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("next.config.mjs"), "export default {}").unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::Vercel);
    }

    #[test]
    fn detect_netlify() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("netlify.toml"), "[build]").unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::Netlify);
    }

    #[test]
    fn detect_fly() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("fly.toml"), "app = \"x\"").unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::Fly);
    }

    #[test]
    fn detect_cloudflare_pages() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("wrangler.toml"), "name = \"x\"").unwrap();
        assert_eq!(
            detect_deploy_target(tmp.path()),
            DeployTarget::CloudflarePages
        );
    }

    #[test]
    fn detect_docker() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Dockerfile"), "FROM scratch").unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::Docker);
    }

    #[test]
    fn detect_static_host_from_dist() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("dist")).unwrap();
        assert_eq!(
            detect_deploy_target(tmp.path()),
            DeployTarget::StaticHost(StaticDir::Dist)
        );
    }

    #[test]
    fn detect_none_for_empty_workspace() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::None);
    }

    #[test]
    fn platform_config_wins_over_dockerfile() {
        // A repo with BOTH a vercel.json and a Dockerfile picks the explicit
        // platform config — it is the more specific signal.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("vercel.json"), "{}").unwrap();
        fs::write(tmp.path().join("Dockerfile"), "FROM scratch").unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::Vercel);
    }

    #[test]
    fn dockerfile_wins_over_bare_static_bundle() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Dockerfile"), "FROM scratch").unwrap();
        fs::create_dir(tmp.path().join("dist")).unwrap();
        assert_eq!(detect_deploy_target(tmp.path()), DeployTarget::Docker);
    }

    #[test]
    fn every_real_target_has_a_command_and_cli() {
        for t in [
            DeployTarget::Vercel,
            DeployTarget::Netlify,
            DeployTarget::Fly,
            DeployTarget::CloudflarePages,
            DeployTarget::Docker,
            DeployTarget::StaticHost(StaticDir::Dist),
        ] {
            assert!(t.deploy_command().is_some(), "{t:?} must have a command");
            assert!(t.cli_binary().is_some(), "{t:?} must name a CLI binary");
        }
        assert!(DeployTarget::None.deploy_command().is_none());
        assert!(DeployTarget::None.cli_binary().is_none());
    }

    #[test]
    fn target_labels_are_stable() {
        assert_eq!(DeployTarget::Vercel.as_str(), "vercel");
        assert_eq!(DeployTarget::Netlify.as_str(), "netlify");
        assert_eq!(DeployTarget::Fly.as_str(), "fly");
        assert_eq!(DeployTarget::CloudflarePages.as_str(), "cloudflare-pages");
        assert_eq!(DeployTarget::Docker.as_str(), "docker");
        assert_eq!(
            DeployTarget::StaticHost(StaticDir::Dist).as_str(),
            "static-host"
        );
        assert_eq!(DeployTarget::None.as_str(), "none");
    }

    #[test]
    fn extract_url_finds_https_in_log() {
        let log = "Building...\nDeployed to https://my-app-abc123.vercel.app in 12s\nDone";
        assert_eq!(
            extract_url(log).as_deref(),
            Some("https://my-app-abc123.vercel.app")
        );
    }

    #[test]
    fn extract_url_trims_trailing_punctuation() {
        let log = "Live at: https://app.netlify.app.";
        assert_eq!(extract_url(log).as_deref(), Some("https://app.netlify.app"));
        let parened = "See (https://app.fly.dev)";
        assert_eq!(extract_url(parened).as_deref(), Some("https://app.fly.dev"));
    }

    #[test]
    fn extract_url_returns_none_without_url() {
        assert!(extract_url("no url here, just text").is_none());
        // A bare scheme with no host is not a usable URL.
        assert!(extract_url("https://").is_none());
    }

    #[test]
    fn log_tail_keeps_the_end_and_truncates_long_output() {
        let long = "x".repeat(CAPTURE_CAP + 500);
        let tail = log_tail(&long, "");
        assert!(tail.starts_with("...[truncated]"));
        assert!(tail.len() <= CAPTURE_CAP + "...[truncated]\n".len());
    }

    #[test]
    fn log_tail_combines_stdout_and_stderr() {
        let tail = log_tail("out line", "err line");
        assert!(tail.contains("out line"));
        assert!(tail.contains("err line"));
    }

    #[test]
    fn tail_capped_does_not_split_multibyte_chars() {
        let s = "做".repeat(20); // each char is 3 bytes
        let tail = tail_capped(&s, 10);
        // Must still be valid UTF-8 (no panic on slicing).
        assert!(tail.ends_with('做'));
    }

    #[test]
    fn not_deployed_record_carries_platform_command_and_reason() {
        let p = DeployProof::not_deployed(DeployTarget::Vercel, "vercel not on PATH");
        assert_eq!(p.platform, DeployTarget::Vercel);
        assert!(!p.status.is_deployed());
        assert_eq!(p.status.as_str(), "not_deployed");
        // Even a not-deployed record surfaces the command the user can run.
        assert_eq!(p.command.as_deref(), Some("npx vercel --prod --yes"));
        assert!(p.summary_line().contains("vercel not on PATH"));
    }

    #[tokio::test]
    async fn run_deploy_no_target_is_fail_open() {
        // Empty workspace → no target → neutral NotDeployed, no spawn, no panic.
        let tmp = TempDir::new().unwrap();
        let proof = run_deploy(tmp.path(), None).await;
        assert_eq!(proof.platform, DeployTarget::None);
        assert!(!proof.status.is_deployed());
        if let DeployStatus::NotDeployed(reason) = &proof.status {
            assert!(reason.contains("no deploy target"));
        } else {
            panic!("expected NotDeployed");
        }
    }

    #[tokio::test]
    async fn run_deploy_missing_cli_is_fail_open() {
        // A Vercel project but the `npx`/binary path is a guaranteed-absent
        // command → NotDeployed("... not found on PATH"), never a crash.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("vercel.json"), "{}").unwrap();
        let proof = run_deploy(tmp.path(), Some("definitely-not-a-real-binary-xyz deploy")).await;
        assert!(!proof.status.is_deployed());
        if let DeployStatus::NotDeployed(reason) = &proof.status {
            assert!(reason.contains("not found on PATH"), "got: {reason}");
        } else {
            panic!("expected NotDeployed");
        }
    }

    #[tokio::test]
    async fn run_deploy_captures_url_and_writes_proof() {
        // A trivially-succeeding command that prints a URL: proves the success
        // path captures the URL + writes the artifact. Uses `printf`/`echo`,
        // which exists on the CI runners; skip cleanly if neither is present.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("netlify.toml"), "[build]").unwrap();
        if !which("echo") && !which("sh") {
            return;
        }
        let proof = run_deploy(
            tmp.path(),
            Some("echo Deployed to https://demo.example.app"),
        )
        .await;
        // On any platform with a working shell this deploys cleanly.
        if proof.status.is_deployed() {
            assert_eq!(proof.url.as_deref(), Some("https://demo.example.app"));
            let path = write_deploy_proof(tmp.path(), &proof).unwrap();
            assert!(path.ends_with("deploy-proof.json"));
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("\"platform\": \"netlify\""));
            assert!(body.contains("demo.example.app"));
        }
    }

    #[test]
    fn deploy_proof_rel_path_is_stable() {
        assert_eq!(deploy_proof_rel_path(), ".umadev/audit/deploy-proof.json");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_deploy_command_times_out_kills_and_does_not_hang() {
        // A command that runs FAR past the (tiny) budget, with a backgrounded
        // grandchild holding the stdout pipe open. On timeout the whole group is
        // killed, so this returns a bounded NotDeployed(timeout) promptly instead
        // of blocking on the pipe holder. (Fixes: tokio dropping the Child on
        // timeout without killing it, and unbounded output().)
        let tmp = TempDir::new().unwrap();
        let started = Instant::now();
        let proof = tokio::time::timeout(
            Duration::from_secs(25),
            run_deploy_command(
                tmp.path(),
                DeployTarget::Netlify,
                "sleep 60 & sleep 60".to_string(),
                1,
            ),
        )
        .await
        .expect("run_deploy_command must return, not hang, on timeout");
        assert!(!proof.status.is_deployed());
        match &proof.status {
            DeployStatus::NotDeployed(reason) => {
                assert!(reason.contains("timed out"), "reason: {reason}");
            }
            DeployStatus::Deployed => panic!("expected NotDeployed(timeout)"),
        }
        assert_eq!(proof.exit_code, Some(-1));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "must return promptly after killing the group, not wait out the pipe holder"
        );
    }

    #[cfg(unix)]
    async fn wait_for_test_pid(path: &Path) -> i32 {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(raw) = fs::read_to_string(path) {
                    if let Ok(pid) = raw.trim().parse::<i32>() {
                        if pid > 0 {
                            return pid;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("deploy test process must publish its pid promptly")
    }

    #[cfg(unix)]
    fn unix_process_is_running(pid: i32) -> bool {
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|output| {
                let state = String::from_utf8_lossy(&output.stdout);
                let state = state.trim();
                !state.is_empty() && !state.starts_with('Z')
            })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aborting_deploy_future_kills_detached_process_tree() {
        let tmp = TempDir::new().unwrap();
        let shell_pid_path = tmp.path().join("deploy-shell.pid");
        let grandchild_pid_path = tmp.path().join("deploy-grandchild.pid");
        let workspace = tmp.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_deploy_command(
                &workspace,
                DeployTarget::Docker,
                concat!(
                    "printf '%s\\n' \"$$\" > deploy-shell.pid; ",
                    "sh -c 'printf \"%s\\n\" \"$$\" > deploy-grandchild.pid; ",
                    "while :; do sleep 30; done' & wait"
                )
                .to_string(),
                30,
            )
            .await
        });

        let shell_pid = wait_for_test_pid(&shell_pid_path).await;
        let grandchild_pid = wait_for_test_pid(&grandchild_pid_path).await;
        assert!(unix_process_is_running(shell_pid));
        assert!(unix_process_is_running(grandchild_pid));

        task.abort();
        assert!(
            task.await
                .expect_err("the deploy task was aborted")
                .is_cancelled(),
            "Tokio must cancel and drop the running deploy future"
        );

        let stopped = tokio::time::timeout(Duration::from_secs(3), async {
            while unix_process_is_running(shell_pid) || unix_process_is_running(grandchild_pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            stopped.is_ok(),
            "dropping the deploy future must stop shell {shell_pid} and descendant {grandchild_pid}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_deploy_command_success_captures_url() {
        // Sanity: the happy path still deploys + captures the URL after the
        // switch off `Command::output()` to a bounded reader.
        let tmp = TempDir::new().unwrap();
        let proof = run_deploy_command(
            tmp.path(),
            DeployTarget::Netlify,
            "echo Deployed to https://demo.example.app".to_string(),
            30,
        )
        .await;
        assert!(proof.status.is_deployed(), "echo exits 0 → Deployed");
        assert_eq!(proof.url.as_deref(), Some("https://demo.example.app"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_deploy_wrapper_kills_a_pipe_holding_descendant() {
        let tmp = TempDir::new().unwrap();
        let pid_path = tmp.path().join("deploy-success-leaf.pid");
        let proof = run_deploy_command(
            tmp.path(),
            DeployTarget::Netlify,
            concat!(
                "sleep 30 & leaf=$!; printf '%s' \"$leaf\" > deploy-success-leaf.pid; ",
                "printf 'https://success.example.app'; exit 0"
            )
            .to_string(),
            5,
        )
        .await;

        assert!(proof.status.is_deployed());
        assert_eq!(proof.url.as_deref(), Some("https://success.example.app"));
        let leaf = wait_for_test_pid(&pid_path).await;
        let stopped = tokio::time::timeout(Duration::from_secs(3), async {
            while unix_process_is_running(leaf) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(stopped.is_ok(), "successful wrapper left descendant {leaf}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newline_free_deploy_flood_is_drained_and_bounded() {
        let tmp = TempDir::new().unwrap();
        let proof = run_deploy_command(
            tmp.path(),
            DeployTarget::Netlify,
            concat!(
                "head -c 1048576 /dev/zero | tr '\\0' x; ",
                "printf 'https://flood.example.app'"
            )
            .to_string(),
            5,
        )
        .await;

        assert!(proof.status.is_deployed());
        assert_eq!(proof.url.as_deref(), Some("https://flood.example.app"));
        assert!(
            proof.log_tail.len() <= CAPTURE_CAP + "...[truncated]\n".len(),
            "stored deploy output exceeded its fixed envelope"
        );
    }
}
