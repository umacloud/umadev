//! `umadev update` for installs that **no JS package manager owns** — a
//! `cargo install`, a release asset someone downloaded by hand, a binary copied
//! into `/usr/local/bin`.
//!
//! An npm/pnpm/yarn/bun install is upgraded by the JS shim (`npm/umadev/bin/cli.js`),
//! which intercepts `update` and never launches the binary: the binary lives inside
//! the very tree the package manager is about to replace, and on Windows a running
//! `.exe` cannot be unlinked (EPERM), so the manager leaves the renamed tree behind
//! forever. This module covers everything the shim cannot: it resolves the latest
//! GitHub Release, downloads **this** platform's asset, verifies it, and swaps it
//! over the running binary atomically.
//!
//! **HTTP client:** `reqwest` — already linked into this binary through
//! `umadev-host` (a non-optional dependency there), so using it here adds *zero*
//! new code, no new TLS stack, and no new transitive crates. It is preferred over
//! shelling out to `curl` / `Invoke-WebRequest` because neither is guaranteed to
//! exist (minimal Linux images ship without curl; PowerShell execution policy can
//! block the Windows form), and a subprocess would make the download's progress and
//! failure modes opaque.
//!
//! **Safety contract:** executable bytes come only from the official GitHub Release
//! and GitHub-owned asset hosts. The download is written to a temp file *next to*
//! the current exe (same filesystem, so the final `rename` is atomic), then checked
//! for size and executable magic before anything touches the installed binary. Any
//! failure leaves the existing binary byte-for-byte untouched and prints an official
//! npm fallback command.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// GitHub Releases API for the repo the release workflow publishes to
/// (`.github/workflows/release.yml` uploads `dist/*` on every `v*` tag).
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/umacloud/umadev/releases/latest";

/// The releases page, printed whenever an automatic update cannot proceed.
const RELEASES_PAGE: &str = "https://github.com/umacloud/umadev/releases";

/// Package-manager fallback when the official GitHub release cannot be reached.
const NPM_FALLBACK: &str =
    "npm install -g umadev@latest --force --registry=https://registry.npmjs.org";

/// Smallest plausible size, in bytes, of a real `umadev` release binary. The
/// shipped binary is tens of MB; anything under 1 MiB is a truncated download, an
/// error page, or a redirect stub — never a usable executable.
const MIN_BINARY_BYTES: u64 = 1_048_576;

/// Hard ceiling for a release executable. A missing or malicious
/// `Content-Length` must not let an updater fill the user's disk indefinitely.
const MAX_BINARY_BYTES: u64 = 512 * 1_048_576;

/// Latest-release JSON should remain small even when a release has many assets.
const MAX_RELEASE_METADATA_BYTES: usize = 1_048_576;

/// A checksum sidecar is one short SHA-256 line. Refuse an unexpectedly large
/// response instead of buffering an arbitrary release asset into memory.
const MAX_CHECKSUM_BYTES: usize = 4_096;

/// Timeout for the (tiny) GitHub Releases API call.
const API_TIMEOUT: Duration = Duration::from_secs(20);

/// Timeout for the whole asset download. Generous: the binary is tens of MB and a
/// slow link must not be cut off mid-update.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(900);

/// A release binary's `--version` probe is local and should finish almost
/// instantly. Keep it bounded so a bad artifact cannot strand the updater.
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// Where this binary lives — which decides how (or whether) it can update itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// Under a `node_modules` tree: a JS package manager owns it, and only that
    /// manager may replace it. Handled by the shim, never by this module.
    PackageManaged,
    /// A build inside a cargo `target/` dir — i.e. someone running this repo.
    /// Overwriting it would fight `cargo build`; print guidance instead.
    DevBuild,
    /// Everything else: `cargo install`, a downloaded release asset, a binary
    /// copied onto `PATH` by hand. This is the case this module actually updates.
    Standalone,
}

/// Classify an installed `umadev` binary by its path alone (pure — no I/O).
pub fn classify_install(exe: &Path) -> InstallKind {
    let parts: Vec<String> = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.iter().any(|p| p == "node_modules") {
        return InstallKind::PackageManaged;
    }
    if is_cargo_target_build(&parts) {
        return InstallKind::DevBuild;
    }
    InstallKind::Standalone
}

/// Does this path look like `…/target/<profile>/umadev` or
/// `…/target/<triple>/<profile>/umadev` — i.e. a cargo build output, not an
/// install? A `cargo install` binary lands in `~/.cargo/bin`, which has no
/// `target/` component, so it is correctly *not* matched.
fn is_cargo_target_build(parts: &[String]) -> bool {
    for (i, part) in parts.iter().enumerate() {
        if part != "target" {
            continue;
        }
        // `target/<profile>/bin` and `target/<triple>/<profile>/bin`.
        for offset in [1_usize, 2] {
            let is_profile = parts
                .get(i + offset)
                .is_some_and(|p| p == "debug" || p == "release");
            if is_profile && parts.len() > i + offset + 1 {
                return true;
            }
        }
    }
    false
}

/// Name of the GitHub Release asset for a given `std::env::consts::OS` /
/// `ARCH` pair, or `None` when we publish no binary for that platform.
///
/// This MUST stay in lockstep with `.github/workflows/release.yml`, which stages
/// each build as `dist/umadev-<target-triple><ext>` and uploads `dist/*`.
pub fn release_asset_name(os: &str, arch: &str) -> Option<String> {
    let linux_env = if cfg!(target_env = "musl") {
        "musl"
    } else {
        "gnu"
    };
    release_asset_name_for(os, arch, linux_env)
}

fn release_asset_name_for(os: &str, arch: &str, linux_env: &str) -> Option<String> {
    let target = match (os, arch, linux_env) {
        ("macos", "aarch64", _) => "aarch64-apple-darwin",
        ("macos", "x86_64", _) => "x86_64-apple-darwin",
        ("linux", "x86_64", "gnu") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64", "gnu") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64", "musl") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64", "musl") => "aarch64-unknown-linux-musl",
        // Windows on ARM runs x64 binaries under built-in emulation, and we publish
        // no arm64 Windows build — reuse the x64 asset, exactly as the npm shim's
        // PLATFORM_PACKAGES maps `win32-arm64` to `@umacloud/cli-win32-x64`.
        ("windows", "x86_64" | "aarch64", _) => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    let ext = if os == "windows" { ".exe" } else { "" };
    Some(format!("umadev-{target}{ext}"))
}

/// Sidecar published beside every release binary.
#[must_use]
pub fn checksum_asset_name(binary_asset: &str) -> String {
    format!("{binary_asset}.sha256")
}

/// Parse a `major.minor.patch` version, tolerating a leading `v` and ignoring any
/// pre-release / build suffix. `None` when it is not a plain three-part version.
fn semver_triple(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Exact stable version encoded by an official release tag.
///
/// Release CI publishes only `v<major>.<minor>.<patch>`. Keep the updater just
/// as strict: a malformed API response must never become the version authority
/// for the executable swap.
fn release_version(tag: &str) -> Option<&str> {
    if tag.trim() != tag {
        return None;
    }
    let version = tag.strip_prefix('v')?;
    let mut parts = version.split('.');
    for _ in 0..3 {
        let part = parts.next()?;
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
            || part.parse::<u64>().is_err()
        {
            return None;
        }
    }
    parts.next().is_none().then_some(version)
}

fn version_order(latest: &str, current: &str) -> Option<Ordering> {
    Some(semver_triple(latest)?.cmp(&semver_triple(current)?))
}

/// Is the published `latest` release actually newer than what is installed?
///
/// Equal versions are **not** newer (that is the "already on the latest version"
/// short-circuit), and a *local* build that is ahead of the release (a dev machine
/// mid-release) is not downgraded. If either side is not a plain semver, fall back
/// to string inequality — an unknown-but-different tag is treated as an update.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match version_order(latest, current) {
        Some(order) => order == Ordering::Greater,
        _ => latest.trim().trim_start_matches('v') != current.trim().trim_start_matches('v'),
    }
}

/// Does the downloaded blob actually start like a native executable for `os`?
///
/// This format check complements (and never replaces) the required SHA-256
/// sidecar verification. It gives a clearer error for an HTML/captive-portal
/// response that happened to arrive before the digest comparison.
/// Unknown platforms fail open (`true`) — we never publish for them anyway.
pub fn looks_like_executable(os: &str, head: &[u8]) -> bool {
    match os {
        // PE/COFF (`MZ`).
        "windows" => head.starts_with(b"MZ"),
        // ELF.
        "linux" => head.starts_with(b"\x7fELF"),
        // Mach-O 64-bit (LE/BE) or a universal ("fat") binary.
        "macos" => {
            head.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
                || head.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
                || head.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
                || head.starts_with(&[0xbe, 0xba, 0xfe, 0xca])
        }
        _ => true,
    }
}

/// The path a running binary is renamed **aside** to on Windows: `<exe>.old`
/// (appended, so `umadev.exe` → `umadev.exe.old`).
pub fn backup_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_os_string();
    name.push(".old");
    PathBuf::from(name)
}

/// One filesystem move in the binary swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapStep {
    /// Move the running exe out of the way. Windows only: a mapped `.exe` cannot be
    /// *unlinked*, but it **can** be *renamed* — so the new binary can take its
    /// place while the old image keeps running. The leftover is swept next launch.
    RenameAside {
        /// The running binary.
        from: PathBuf,
        /// Its `<exe>.old` parking spot.
        to: PathBuf,
    },
    /// Move the verified download into place. On Unix this single `rename` IS the
    /// update: replacing a running binary is legal, the running process keeps its
    /// already-mapped inode.
    Install {
        /// The verified temp file (same filesystem as the exe).
        from: PathBuf,
        /// The installed binary path.
        to: PathBuf,
    },
}

/// The ordered moves that replace `exe` with the verified download at `tmp`.
///
/// `windows` is a parameter (not `cfg!`) so both plans stay unit-testable from any
/// host.
pub fn swap_plan(exe: &Path, tmp: &Path, windows: bool) -> Vec<SwapStep> {
    let mut steps = Vec::new();
    if windows {
        steps.push(SwapStep::RenameAside {
            from: exe.to_path_buf(),
            to: backup_path(exe),
        });
    }
    steps.push(SwapStep::Install {
        from: tmp.to_path_buf(),
        to: exe.to_path_buf(),
    });
    steps
}

/// Execute a [`swap_plan`]. Every move is a same-filesystem `rename`, so each step
/// is atomic; the download was already verified, so nothing here can produce a
/// half-written binary.
fn apply_swap(steps: &[SwapStep]) -> Result<()> {
    let mut parked: Option<(&Path, &Path)> = None;
    for step in steps {
        match step {
            SwapStep::RenameAside { from, to } => {
                // A `<exe>.old` from a previous update may still exist (the sweep is
                // best-effort). Windows `rename` will not clobber it, so drop it first.
                let _ = std::fs::remove_file(to);
                std::fs::rename(from, to).with_context(|| {
                    format!(
                        "could not move the running binary aside to {}",
                        to.display()
                    )
                })?;
                parked = Some((from, to));
            }
            SwapStep::Install { from, to } => {
                if let Err(install_err) = std::fs::rename(from, to) {
                    if let Some((original, backup)) = parked {
                        if let Err(rollback_err) = std::fs::rename(backup, original) {
                            bail!(
                                "could not install the new binary at {} ({install_err}); \
                                 rollback from {} also failed ({rollback_err})",
                                to.display(),
                                backup.display()
                            );
                        }
                    }
                    return Err(install_err).with_context(|| {
                        format!("could not install the new binary at {}", to.display())
                    });
                }
            }
        }
    }
    Ok(())
}

/// Delete a `<exe>.old` left behind by a Windows self-update. Called once at
/// startup; strictly best-effort and never fails the launch (the old image may
/// still be mapped by a process that is exiting — the next launch retries).
pub fn sweep_stale_backup() {
    if !cfg!(windows) {
        return; // the Unix swap never creates one
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(backup_path(&exe));
    }
}

/// Tag + assets of the latest GitHub Release — only the fields the updater needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRelease {
    /// The release tag, e.g. `v1.0.41`.
    pub tag: String,
    /// `(asset name, browser_download_url)` for every asset on the release.
    pub assets: Vec<(String, String)>,
}

impl LatestRelease {
    /// The download URL of the asset named `name`, if the release published it.
    pub fn asset_url(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, u)| u.as_str())
    }
}

/// Parse the GitHub Releases API `latest` payload. Pure, so the whole
/// release-resolution path is testable without touching the network.
pub fn parse_latest_release(body: &str) -> Result<LatestRelease> {
    let v: serde_json::Value = serde_json::from_str(body)
        .context("the release API returned something that is not JSON")?;
    let tag = v
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .context("the release API returned no tag_name")?
        .to_string();
    let assets = v
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let name = a.get("name")?.as_str()?.to_string();
                    let url = a.get("browser_download_url")?.as_str()?.to_string();
                    Some((name, url))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(LatestRelease { tag, assets })
}

/// Trusted download candidates for a release asset.
///
/// There is deliberately one candidate. The matching SHA-256 sidecar is fetched
/// independently from the same official release; community proxies are never an
/// executable source.
pub fn download_urls(browser_url: &str) -> Vec<String> {
    reqwest::Url::parse(browser_url)
        .ok()
        .filter(is_official_release_asset_url)
        .map(|_| vec![browser_url.to_string()])
        .unwrap_or_default()
}

/// The API's browser URL must be this repository's HTTPS release-download path.
fn is_official_release_asset_url(url: &reqwest::Url) -> bool {
    is_https_default_port(url)
        && url.host_str() == Some("github.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .path()
            .starts_with("/umacloud/umadev/releases/download/")
}

/// GitHub redirects browser download URLs to a signed URL on one of its asset
/// hosts. No redirect to a community proxy or unrelated host is accepted.
fn is_official_download_target(url: &reqwest::Url) -> bool {
    if !is_https_default_port(url) || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.host_str() {
        Some("github.com") => is_official_release_asset_url(url),
        Some(
            "release-assets.githubusercontent.com"
            | "objects.githubusercontent.com"
            | "github-releases.githubusercontent.com",
        ) => true,
        _ => false,
    }
}

fn is_https_default_port(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && url.port_or_known_default() == Some(443)
}

fn official_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            attempt.error("too many redirects while downloading the GitHub release asset")
        } else if is_official_download_target(attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("refusing a release-asset redirect outside official GitHub hosts")
        }
    })
}

/// Redirect policy for the release-*API* client. Unlike the asset download (which
/// intentionally hops `api.github.com` → `github.com` → a CDN asset host), the
/// releases-API call should never leave its own host: a redirect that moves the
/// query to an unrelated origin is a sign of interception, not a legitimate CDN
/// hand-off. So this follows only same-host HTTPS hops and caps their number,
/// mirroring the bounded, host-scoped shape of [`official_redirect_policy`].
/// Without any policy the client followed redirects to arbitrary hosts by default.
fn api_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects while querying the GitHub release API");
        }
        let same_host = attempt
            .previous()
            .last()
            .and_then(reqwest::Url::host_str)
            .zip(attempt.url().host_str())
            .is_some_and(|(prev, next)| prev.eq_ignore_ascii_case(next));
        if is_https_default_port(attempt.url()) && same_host {
            attempt.follow()
        } else {
            attempt.error("refusing a release-API redirect to a different host")
        }
    })
}

/// Fetch + parse the latest release from the GitHub API.
async fn fetch_latest_release() -> Result<LatestRelease> {
    let client = reqwest::Client::builder()
        .user_agent("umadev-cli")
        .timeout(API_TIMEOUT)
        .redirect(api_redirect_policy())
        .build()
        .context("could not build an HTTP client")?;
    let resp = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("could not reach the GitHub release API")?;
    if !resp.status().is_success() {
        bail!("the GitHub release API returned HTTP {}", resp.status());
    }
    let body = response_bytes_bounded(resp, MAX_RELEASE_METADATA_BYTES, "release metadata").await?;
    let text = std::str::from_utf8(&body).context("the release API response is not UTF-8")?;
    parse_latest_release(text)
}

async fn response_bytes_bounded(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("the {label} response is unexpectedly large");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("the {label} response was interrupted"))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            bail!("the {label} response exceeds {max_bytes} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Stream one URL to `dest`, with a coarse progress line on stderr. Returns the
/// number of bytes written. The caller verifies the result *before* installing it.
async fn download_to(client: &reqwest::Client, url: &str, dest: &Path) -> Result<u64> {
    use std::io::Write as _;

    let mut resp = client
        .get(url)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?;
    if !resp.status().is_success() {
        bail!("HTTP {} from {url}", resp.status());
    }
    let total = resp.content_length().unwrap_or(0);
    if total > MAX_BINARY_BYTES {
        bail!("the release asset is unexpectedly large ({total} bytes)");
    }

    let mut file = umadev_state::fs::create_new_private(dest).with_context(|| {
        format!(
            "could not write next to the installed binary ({}) — is the install dir writable?",
            dest.display()
        )
    })?;
    let mut got: u64 = 0;
    let mut last_report: u64 = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("the download from {url} was interrupted"))?
    {
        file.write_all(&chunk)
            .with_context(|| format!("could not write to {}", dest.display()))?;
        got = got
            .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
            .context("release asset byte count overflow")?;
        if got > MAX_BINARY_BYTES {
            bail!("the release asset exceeds {MAX_BINARY_BYTES} bytes");
        }
        // One line per ~4 MiB — enough to show life on a slow link, quiet enough for
        // a piped/CI log. Integer math throughout: a byte count is exact in u64 and
        // an approximate percentage needs no float.
        if got - last_report >= 4 * 1_048_576 {
            last_report = got;
            let mb = |b: u64| b / 1_048_576;
            // `checked_div` is None exactly when the server sent no Content-Length
            // (total == 0) — then report bytes only, with no percentage.
            if let Some(pct) = got.saturating_mul(100).checked_div(total) {
                eprint!("\r  downloading  {pct}%  ({}/{} MB)   ", mb(got), mb(total));
            } else {
                eprint!("\r  downloading  {} MB   ", mb(got));
            }
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
    }
    file.flush()
        .with_context(|| format!("could not flush {}", dest.display()))?;
    file.sync_all()
        .with_context(|| format!("could not sync {}", dest.display()))?;
    drop(file);
    if last_report > 0 {
        eprintln!();
    }
    Ok(got)
}

/// Download and parse the checksum sidecar for `asset`. The accepted format is
/// the conventional `64-hex  filename` line generated by the release workflow.
async fn fetch_checksum(client: &reqwest::Client, url: &str, asset: &str) -> Result<String> {
    if download_urls(url).is_empty() {
        bail!("refusing a checksum URL outside the official GitHub release source");
    }
    let resp = client
        .get(url)
        .header("Accept", "text/plain")
        .send()
        .await
        .with_context(|| format!("could not reach checksum {url}"))?;
    if !resp.status().is_success() {
        bail!("HTTP {} from checksum {url}", resp.status());
    }
    let body = response_bytes_bounded(resp, MAX_CHECKSUM_BYTES, "checksum sidecar").await?;
    let text = std::str::from_utf8(&body).context("the checksum sidecar is not UTF-8")?;
    parse_sha256_sidecar(text, asset)
}

fn parse_sha256_sidecar(body: &str, asset: &str) -> Result<String> {
    for line in body.lines() {
        let mut fields = line.split_whitespace();
        let Some(hash) = fields.next() else { continue };
        let Some(name) = fields.next() else { continue };
        let name = name.trim_start_matches('*');
        if name != asset {
            continue;
        }
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(hash.to_ascii_lowercase());
        }
        bail!("the checksum for `{asset}` is not a 64-digit SHA-256 value");
    }
    bail!("the checksum sidecar contains no entry for `{asset}`")
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read as _;

    let mut file =
        std::fs::File::open(path).with_context(|| format!("could not hash {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1_024].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("could not hash {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Checksum + size + magic-byte gate on a finished download. Runs BEFORE any
/// rename, so a rejected file never reaches the installed path.
fn verify_download(path: &Path, os: &str, expected_sha256: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("the download vanished: {}", path.display()))?;
    if !umadev_state::fs::metadata_is_real_file(&metadata) {
        bail!("the staged download is linked, reparsed, or not a regular file");
    }
    let size = metadata.len();
    if size < MIN_BINARY_BYTES {
        bail!(
            "the download is only {size} bytes — that is not a umadev binary (a truncated \
             transfer or an error page served with HTTP 200)"
        );
    }
    if size > MAX_BINARY_BYTES {
        bail!("the staged download exceeds {MAX_BINARY_BYTES} bytes");
    }
    let head = {
        use std::io::Read as _;
        let mut f = std::fs::File::open(path)
            .with_context(|| format!("could not re-open {}", path.display()))?;
        let mut buf = [0_u8; 8];
        let n = f.read(&mut buf).unwrap_or(0);
        buf[..n].to_vec()
    };
    if !looks_like_executable(os, &head) {
        bail!("the download does not look like a native executable for this platform");
    }
    let actual = sha256_file(path)?;
    if actual != expected_sha256.to_ascii_lowercase() {
        bail!(
            "the downloaded binary failed SHA-256 verification (expected {expected_sha256}, got {actual})"
        );
    }
    Ok(())
}

/// Execute the fully downloaded candidate and bind its own embedded version to
/// the selected release tag before replacing the installed executable.
fn verify_staged_version(path: &Path, expected: &str) -> Result<()> {
    let mut command = std::process::Command::new(path);
    command.arg("--version");
    let output = umadev_process::run_bounded_std_command(
        command,
        umadev_process::BoundedCommandOptions {
            timeout: VERSION_CHECK_TIMEOUT,
            stdout_bytes: 4_096,
            stderr_bytes: 4_096,
            reader_grace: Duration::from_secs(1),
        },
    )
    .with_context(|| format!("could not run staged binary {}", path.display()))?;
    if output.timed_out {
        bail!("the staged binary timed out while reporting its version");
    }
    if output.stdout_truncated || output.stderr_truncated {
        bail!("the staged binary produced excessive version output");
    }
    if !output.status.is_some_and(|status| status.success()) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "the staged binary could not report its version{}",
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        );
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .context("the staged binary's version output is not UTF-8")?;
    let mut fields = stdout.split_whitespace();
    let product = fields.next().unwrap_or_default();
    let reported = fields
        .next()
        .unwrap_or_default()
        .trim_start_matches(['v', 'V']);
    if !product.eq_ignore_ascii_case("umadev") || reported != expected || fields.next().is_some() {
        bail!(
            "the staged binary reports `{}`, expected `UmaDev {expected}`",
            stdout.trim()
        );
    }
    Ok(())
}

/// Make the staged file executable before it is renamed into place, so the binary
/// is runnable the instant it appears at the installed path (no window where
/// `umadev` exists but is not `+x`).
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("could not chmod +x {}", path.display()))
}

/// No-op on Windows: executability is decided by the `.exe` extension.
#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// The manual fallback, printed on every failure path so the user is never stuck.
fn manual_instructions() -> String {
    format!(
        "\nUpdate through the official npm package:\n  {NPM_FALLBACK}\n\
         Or download this platform's binary from the official GitHub release:\n  \
         {RELEASES_PAGE}"
    )
}

fn print_manual_instructions() {
    println!("{}", manual_instructions());
}

fn staging_path(exe: &Path) -> Result<PathBuf> {
    use std::fmt::Write as _;

    let parent = exe
        .parent()
        .context("the installed binary has no parent directory")?;
    let mut nonce = [0_u8; 16];
    getrandom::getrandom(&mut nonce)
        .map_err(|error| anyhow::anyhow!("could not allocate a secure update filename: {error}"))?;
    let mut token = String::with_capacity(nonce.len() * 2);
    for byte in nonce {
        write!(&mut token, "{byte:02x}")
            .map_err(|_| anyhow::anyhow!("could not encode a secure update filename"))?;
    }
    Ok(parent.join(staging_filename(cfg!(windows), std::process::id(), &token)))
}

fn staging_filename(windows: bool, process_id: u32, token: &str) -> String {
    let extension = if windows { ".exe" } else { "" };
    format!(".umadev-update-{process_id}-{token}{extension}")
}

/// `umadev update` for a [`InstallKind::Standalone`] install: resolve the latest
/// release, download + verify this platform's asset, and swap it over `exe`.
///
/// `confirm` is the caller's `y/N` prompt (skipped when `yes`). `force` reinstalls
/// even when already on the latest version. On ANY failure the installed binary is
/// left untouched and the manual command is printed.
pub async fn run(
    exe: &Path,
    yes: bool,
    force: bool,
    confirm: impl FnOnce(&str) -> bool,
) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let Some(asset) = release_asset_name(os, arch) else {
        println!("No prebuilt binary is published for {os}-{arch}.");
        print_manual_instructions();
        bail!("unsupported platform: {os}-{arch}");
    };

    println!("Checking for a newer release…");
    let release = match fetch_latest_release().await {
        Ok(r) => r,
        Err(e) => {
            println!("Could not check for updates: {e:#}");
            print_manual_instructions();
            return Err(e);
        }
    };
    let Some(release_version) = release_version(&release.tag).map(str::to_string) else {
        println!(
            "The latest GitHub release has an invalid tag: {}.",
            release.tag
        );
        print_manual_instructions();
        bail!("invalid stable release tag `{}`", release.tag);
    };

    if version_order(&release.tag, current) == Some(Ordering::Less) {
        println!(
            "Installed version {current} is newer than the latest release ({}). Refusing to downgrade.",
            release.tag
        );
        return Ok(());
    }

    if !is_newer(&release.tag, current) && !force {
        println!("Already on the latest version ({current}). Nothing to do.");
        return Ok(());
    }

    let Some(url) = release.asset_url(&asset).map(str::to_string) else {
        println!("Release {} publishes no `{asset}` asset.", release.tag);
        print_manual_instructions();
        bail!("no asset `{asset}` in release {}", release.tag);
    };
    if download_urls(&url).is_empty() {
        println!("Release {} returned an untrusted asset URL.", release.tag);
        print_manual_instructions();
        bail!("refusing to download `{asset}` from a non-official GitHub release URL");
    }
    let checksum_asset = checksum_asset_name(&asset);
    let Some(checksum_url) = release.asset_url(&checksum_asset).map(str::to_string) else {
        println!(
            "Release {} publishes no `{checksum_asset}` integrity sidecar.",
            release.tag
        );
        print_manual_instructions();
        bail!(
            "no checksum asset `{checksum_asset}` in release {}",
            release.tag
        );
    };
    if download_urls(&checksum_url).is_empty() {
        println!(
            "Release {} returned an untrusted checksum URL.",
            release.tag
        );
        print_manual_instructions();
        bail!("refusing a non-official checksum URL for `{asset}`");
    }

    if is_newer(&release.tag, current) {
        println!(
            "A newer release is available: {} (you have {current}).",
            release.tag
        );
    } else {
        println!("Reinstalling {} (--force).", release.tag);
    }
    if !yes && !confirm(&format!("Download and install {} now?", release.tag)) {
        println!("Aborted.");
        return Ok(());
    }

    // Stage NEXT TO the exe: same filesystem, so the final rename is atomic (a temp
    // dir is often a different mount, where `rename` fails with EXDEV).
    let tmp = staging_path(exe)?;

    let result = install(&tmp, exe, &url, &checksum_url, &asset, os, &release_version).await;
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp); // never leave a partial download behind
        print_manual_instructions();
    }
    result?;

    println!(
        "[ok] UmaDev updated to {}. Run `umadev --version` to confirm.",
        release.tag
    );
    Ok(())
}

/// Download → verify → swap. Split out of [`run`] so every failure funnels through
/// one cleanup path (delete the temp file, keep the installed binary).
async fn install(
    tmp: &Path,
    exe: &Path,
    url: &str,
    checksum_url: &str,
    asset: &str,
    os: &str,
    expected_version: &str,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("umadev-cli")
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(official_redirect_policy())
        .build()
        .context("could not build an HTTP client")?;

    let expected_sha256 = fetch_checksum(&client, checksum_url, asset).await?;
    let candidates = download_urls(url);
    if candidates.is_empty() {
        bail!("refusing a release asset URL outside the official GitHub release source");
    }
    let mut last: Option<anyhow::Error> = None;
    let mut downloaded = false;
    for candidate in candidates {
        match download_to(&client, &candidate, tmp).await {
            Ok(_) => {
                downloaded = true;
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    if !downloaded {
        return Err(last.unwrap_or_else(|| anyhow::anyhow!("no download source was reachable")));
    }

    verify_download(tmp, os, &expected_sha256)?;
    make_executable(tmp)?;
    verify_staged_version(tmp, expected_version)?;
    apply_swap(&swap_plan(exe, tmp, cfg!(windows)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_match_the_release_workflow() {
        // These strings are the contract with `.github/workflows/release.yml`, which
        // stages `dist/umadev-<target><ext>`.
        assert_eq!(
            release_asset_name("macos", "aarch64").unwrap(),
            "umadev-aarch64-apple-darwin"
        );
        assert_eq!(
            release_asset_name("macos", "x86_64").unwrap(),
            "umadev-x86_64-apple-darwin"
        );
        assert_eq!(
            release_asset_name_for("linux", "x86_64", "gnu").unwrap(),
            "umadev-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            release_asset_name_for("linux", "aarch64", "gnu").unwrap(),
            "umadev-aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            release_asset_name_for("linux", "x86_64", "musl").unwrap(),
            "umadev-x86_64-unknown-linux-musl"
        );
        assert_eq!(
            release_asset_name_for("linux", "aarch64", "musl").unwrap(),
            "umadev-aarch64-unknown-linux-musl"
        );
        assert_eq!(
            release_asset_name("windows", "x86_64").unwrap(),
            "umadev-x86_64-pc-windows-msvc.exe"
        );
    }

    #[test]
    fn checksum_sidecar_name_is_derived_from_the_exact_binary_asset() {
        assert_eq!(
            checksum_asset_name("umadev-x86_64-pc-windows-msvc.exe"),
            "umadev-x86_64-pc-windows-msvc.exe.sha256"
        );
    }

    #[test]
    fn parses_only_the_checksum_for_the_requested_asset() {
        let wanted = "umadev-aarch64-apple-darwin";
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let body = format!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  other\n{hash}  {wanted}\n"
        );
        assert_eq!(parse_sha256_sidecar(&body, wanted).unwrap(), hash);
        assert!(parse_sha256_sidecar(&body, "missing").is_err());
        assert!(parse_sha256_sidecar("not-a-hash  wanted\n", "wanted").is_err());
    }

    #[test]
    fn downloaded_binary_must_match_its_sha256_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("umadev");
        let mut bytes = vec![
            0_u8;
            usize::try_from(MIN_BINARY_BYTES)
                .expect("minimum binary size fits every supported target")
        ];
        bytes[..4].copy_from_slice(b"\x7fELF");
        std::fs::write(&binary, &bytes).unwrap();
        let expected = sha256_file(&binary).unwrap();
        verify_download(&binary, "linux", &expected).unwrap();
        assert!(verify_download(
            &binary,
            "linux",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        )
        .is_err());
    }

    #[test]
    fn staged_binary_size_is_bounded_even_if_the_file_changes_after_download() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("oversized");
        let file = std::fs::File::create(&binary).unwrap();
        file.set_len(MAX_BINARY_BYTES + 1).unwrap();
        assert!(verify_download(&binary, "linux", &"0".repeat(64)).is_err());
    }

    #[test]
    fn windows_on_arm_reuses_the_x64_asset() {
        assert_eq!(
            release_asset_name("windows", "aarch64").unwrap(),
            "umadev-x86_64-pc-windows-msvc.exe"
        );
    }

    #[test]
    fn unpublished_platforms_have_no_asset() {
        assert!(release_asset_name("freebsd", "x86_64").is_none());
        assert!(release_asset_name("linux", "riscv64").is_none());
    }

    #[test]
    fn the_running_platform_always_has_an_asset() {
        // Every platform we can actually build+test on must be self-updatable.
        assert!(release_asset_name(std::env::consts::OS, std::env::consts::ARCH).is_some());
    }

    #[test]
    fn already_latest_is_not_newer() {
        assert!(!is_newer("v1.0.40", "1.0.40"));
        assert!(!is_newer("1.0.40", "1.0.40"));
    }

    #[test]
    fn a_higher_release_is_newer() {
        assert!(is_newer("v1.0.41", "1.0.40"));
        assert!(is_newer("v1.1.0", "1.0.99"));
        assert!(is_newer("v2.0.0", "1.9.9"));
    }

    #[test]
    fn a_local_build_ahead_of_the_release_is_not_downgraded() {
        assert!(!is_newer("v1.0.39", "1.0.40"));
    }

    #[test]
    fn a_non_semver_tag_falls_back_to_string_inequality() {
        assert!(is_newer("nightly", "1.0.40"));
        assert!(!is_newer("weird", "weird"));
    }

    #[test]
    fn updater_accepts_only_exact_stable_release_tags() {
        assert_eq!(release_version("v1.2.3"), Some("1.2.3"));
        for invalid in [
            "1.2.3",
            "v1.2",
            "v1.2.3.4",
            "v1.2.3-beta.1",
            "v1.2.3+local",
            "v01.2.3",
            "v1.02.3",
            "vnext",
            " v1.2.3 ",
        ] {
            assert_eq!(release_version(invalid), None, "accepted {invalid}");
        }
    }

    #[test]
    fn windows_staging_binary_keeps_an_executable_suffix() {
        assert_eq!(
            staging_filename(true, 42, "abc"),
            ".umadev-update-42-abc.exe"
        );
        assert_eq!(staging_filename(false, 42, "abc"), ".umadev-update-42-abc");
    }

    #[test]
    fn force_reinstall_comparison_can_distinguish_equal_from_downgrade() {
        assert_eq!(version_order("v1.2.3", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(version_order("v1.2.2", "1.2.3"), Some(Ordering::Less));
        assert_eq!(version_order("v1.2.4", "1.2.3"), Some(Ordering::Greater));
    }

    #[cfg(unix)]
    #[test]
    fn staged_binary_version_must_match_the_release_before_swap() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("umadev-candidate");
        std::fs::write(&candidate, "#!/bin/sh\nprintf 'UmaDev 1.2.3\\n'\n").unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700)).unwrap();
        verify_staged_version(&candidate, "1.2.3").unwrap();
        assert!(verify_staged_version(&candidate, "1.2.4").is_err());

        std::fs::write(
            &candidate,
            "#!/bin/sh\nprintf 'not-umadev 1.2.3 extra\\n'\n",
        )
        .unwrap();
        assert!(verify_staged_version(&candidate, "1.2.3").is_err());
    }

    #[test]
    fn swap_plan_on_unix_is_one_atomic_rename() {
        let exe = Path::new("/usr/local/bin/umadev");
        let tmp = Path::new("/usr/local/bin/umadev.new-1");
        assert_eq!(
            swap_plan(exe, tmp, false),
            vec![SwapStep::Install {
                from: tmp.to_path_buf(),
                to: exe.to_path_buf(),
            }]
        );
    }

    #[test]
    fn swap_plan_on_windows_parks_the_running_exe_first() {
        let exe = Path::new(r"C:\bin\umadev.exe");
        let tmp = Path::new(r"C:\bin\umadev.exe.new-1");
        assert_eq!(
            swap_plan(exe, tmp, true),
            vec![
                SwapStep::RenameAside {
                    from: exe.to_path_buf(),
                    to: PathBuf::from(r"C:\bin\umadev.exe.old"),
                },
                SwapStep::Install {
                    from: tmp.to_path_buf(),
                    to: exe.to_path_buf(),
                },
            ]
        );
    }

    #[test]
    fn a_failed_windows_install_rolls_the_running_binary_back() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("umadev.exe");
        let missing_download = tmp.path().join("umadev.exe.new-missing");
        std::fs::write(&exe, b"old executable").unwrap();

        let result = apply_swap(&swap_plan(&exe, &missing_download, true));

        assert!(result.is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old executable");
        assert!(
            !backup_path(&exe).exists(),
            "rollback restores the original path instead of stranding only .old"
        );
    }

    #[test]
    fn backup_path_appends_dot_old() {
        assert_eq!(
            backup_path(Path::new("/usr/local/bin/umadev")),
            PathBuf::from("/usr/local/bin/umadev.old")
        );
        assert_eq!(
            backup_path(Path::new(r"C:\bin\umadev.exe")),
            PathBuf::from(r"C:\bin\umadev.exe.old")
        );
    }

    #[test]
    fn an_npm_install_is_package_managed() {
        assert_eq!(
            classify_install(Path::new(
                "/usr/local/lib/node_modules/@umacloud/cli-linux-x64/bin/umadev"
            )),
            InstallKind::PackageManaged
        );
        // pnpm's content-addressed layout still lives under node_modules.
        assert_eq!(
            classify_install(Path::new(
                "/home/u/Library/pnpm/global/5/.pnpm/@umacloud+cli-linux-x64@1.0.0/node_modules/@umacloud/cli-linux-x64/bin/umadev"
            )),
            InstallKind::PackageManaged
        );
    }

    #[test]
    fn a_cargo_target_build_is_a_dev_build() {
        assert_eq!(
            classify_install(Path::new("/home/u/UmaDev/target/debug/umadev")),
            InstallKind::DevBuild
        );
        assert_eq!(
            classify_install(Path::new("/home/u/UmaDev/target/release/umadev")),
            InstallKind::DevBuild
        );
        assert_eq!(
            classify_install(Path::new(
                "/home/u/UmaDev/target/aarch64-apple-darwin/release/umadev"
            )),
            InstallKind::DevBuild
        );
    }

    #[test]
    fn a_cargo_install_or_manual_copy_is_standalone() {
        assert_eq!(
            classify_install(Path::new("/home/u/.cargo/bin/umadev")),
            InstallKind::Standalone
        );
        assert_eq!(
            classify_install(Path::new("/usr/local/bin/umadev")),
            InstallKind::Standalone
        );
        assert_eq!(
            classify_install(Path::new(r"C:\Users\u\bin\umadev.exe")),
            InstallKind::Standalone
        );
        // A directory literally named `target` that is NOT a cargo build output.
        assert_eq!(
            classify_install(Path::new("/opt/target/umadev")),
            InstallKind::Standalone
        );
    }

    #[test]
    fn executable_magic_rejects_an_html_error_page() {
        let html = b"<!DOCTYPE html><html>";
        assert!(!looks_like_executable("linux", html));
        assert!(!looks_like_executable("macos", html));
        assert!(!looks_like_executable("windows", html));
    }

    #[test]
    fn executable_magic_accepts_each_native_format() {
        assert!(looks_like_executable("linux", b"\x7fELF\x02\x01\x01\x00"));
        assert!(looks_like_executable(
            "macos",
            &[0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01]
        ));
        assert!(looks_like_executable(
            "macos",
            &[0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x02]
        ));
        assert!(looks_like_executable("windows", b"MZ\x90\x00"));
        // An unpublished platform fails open rather than blocking an update.
        assert!(looks_like_executable("freebsd", b"anything"));
    }

    #[test]
    fn parses_a_release_payload() {
        let body = r#"{
          "tag_name": "v1.0.41",
          "assets": [
            {"name": "umadev-aarch64-apple-darwin",
             "browser_download_url": "https://github.com/umacloud/umadev/releases/download/v1.0.41/umadev-aarch64-apple-darwin"},
            {"name": "model.safetensors",
             "browser_download_url": "https://example.invalid/model"}
          ]
        }"#;
        let r = parse_latest_release(body).unwrap();
        assert_eq!(r.tag, "v1.0.41");
        assert_eq!(r.assets.len(), 2);
        assert_eq!(
            r.asset_url("umadev-aarch64-apple-darwin").unwrap(),
            "https://github.com/umacloud/umadev/releases/download/v1.0.41/umadev-aarch64-apple-darwin"
        );
        assert!(r.asset_url("umadev-x86_64-pc-windows-msvc.exe").is_none());
    }

    #[test]
    fn a_release_without_assets_still_parses() {
        let r = parse_latest_release(r#"{"tag_name":"v9.9.9"}"#).unwrap();
        assert_eq!(r.tag, "v9.9.9");
        assert!(r.assets.is_empty());
    }

    #[test]
    fn junk_from_the_release_api_is_an_error_not_a_panic() {
        assert!(parse_latest_release("<html>rate limited</html>").is_err());
        assert!(parse_latest_release(r#"{"message":"Not Found"}"#).is_err());
    }

    #[test]
    fn download_candidates_contain_only_the_official_github_release() {
        let official = "https://github.com/umacloud/umadev/releases/download/v1/x";
        let urls = download_urls(official);
        assert_eq!(urls, vec![official]);
        assert!(urls.iter().all(|url| {
            let parsed = reqwest::Url::parse(url).unwrap();
            parsed.scheme() == "https" && parsed.host_str() == Some("github.com")
        }));
        assert!(urls
            .iter()
            .all(|url| !url.contains("ghproxy.net") && !url.contains("ghfast.top")));
    }

    #[test]
    fn download_candidates_reject_third_party_http_and_lookalike_sources() {
        for url in [
            "https://ghproxy.net/https://github.com/umacloud/umadev/releases/download/v1/x",
            "https://ghfast.top/https://github.com/umacloud/umadev/releases/download/v1/x",
            "http://github.com/umacloud/umadev/releases/download/v1/x",
            "https://github.com.evil.invalid/umacloud/umadev/releases/download/v1/x",
            "https://github.com/another/repo/releases/download/v1/x",
        ] {
            assert!(
                download_urls(url).is_empty(),
                "accepted untrusted URL: {url}"
            );
        }
    }

    #[test]
    fn redirects_are_limited_to_official_github_asset_hosts() {
        for url in [
            "https://github.com/umacloud/umadev/releases/download/v1/x",
            "https://release-assets.githubusercontent.com/github-production-release-asset/x",
            "https://objects.githubusercontent.com/github-production-release-asset/x",
        ] {
            assert!(
                is_official_download_target(&reqwest::Url::parse(url).unwrap()),
                "rejected official target: {url}"
            );
        }
        for url in [
            "https://ghproxy.net/x",
            "https://ghfast.top/x",
            "https://example.com/x",
            "http://release-assets.githubusercontent.com/x",
        ] {
            assert!(
                !is_official_download_target(&reqwest::Url::parse(url).unwrap()),
                "accepted untrusted redirect: {url}"
            );
        }
    }

    #[test]
    fn failure_instructions_include_the_official_npm_fallback() {
        let instructions = manual_instructions();
        assert!(instructions.contains("official npm package"));
        assert!(instructions.contains(NPM_FALLBACK));
        assert!(instructions.contains(RELEASES_PAGE));
    }
}
