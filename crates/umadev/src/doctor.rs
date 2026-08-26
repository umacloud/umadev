//! `umadev doctor` — self-test that diagnoses common
//! "installed-but-not-working" situations.
//!
//! Checks performed:
//! 1. Binary identity (`CARGO_PKG_VERSION` + spec version).
//! 2. Embedded spec markdown is non-empty + carries the version marker.
//! 3. Workspace is writable (write + delete a tmp file).
//! 4. UD-META-001 spec manifest present and version-aligned.
//! 5. AI coding host CLIs detected on PATH.
//! 6. Claude non-interactive auth: when `claude-code` is the selected backend, a
//!    headless credential (`CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`)
//!    is available — an interactive `claude login` alone can 401 on UmaDev's
//!    background calls.
//! 7. Native real-time governance hooks: Claude Code's project settings and
//!    Kimi Code's exact project-scoped rows in its user-level registry.
//! 8. Delivery / deployment readiness (after a run completes): delivery notes
//!    present with a deploy command, build output exists, and a deploy CLI
//!    (vercel / netlify / wrangler) is on PATH.
//! 9. npm delivery health (`check_npm_install`): a `sudo`-installed (root-owned)
//!    global tree or a root-owned `~/.npm` cache — which wedge every LATER
//!    non-root npm operation on that prefix, including the user's *other* global
//!    packages — and the "installed locally, so the command is not on PATH"
//!    confusion (`npm i @umatech/umadev` without `-g` → run it via `npx umadev`).
//! 10. Workspace run-lock compatibility fence (`check_run_lock_fence`) — diagnoses
//!     a legacy/incomplete fence and, with `--fix`, performs an explicit offline
//!     migration after the user has stopped every older UmaDev process.
//! 11. Stale temp-rewind marker (`check_workspace_rewind_marker`) — a marker naming
//!     a snapshot the workspace can no longer identify makes every `umadev run`
//!     abort on every start; `--fix` can clear only that unrecoverable marker.
//!
//! The hook checks never rely on substring matches: they parse the vendor's
//! native configuration and validate the exact rows UmaDev owns.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use umadev_state::fs::RootedDir;

const MAX_DOCTOR_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DOCTOR_NOTES_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DOCTOR_ENTRIES: usize = 1_024;

fn doctor_utf8(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let bytes = umadev_state::fs::read_bounded(path, max_bytes)?;
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Single check result row.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CheckResult {
    /// Short check name shown in the report.
    pub name: String,
    /// `passed` | `warning` | `failed`.
    pub status: Status,
    /// Human-readable detail.
    pub detail: String,
}

/// Status verbs.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Status {
    /// The check passed.
    Passed,
    /// The check produced a warning but the binary still functions.
    Warning,
    /// The check failed — user intervention needed.
    Failed,
}

impl Status {
    /// Short label used in the report header column.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "PASS",
            Self::Warning => "WARN",
            Self::Failed => "FAIL",
        }
    }
}

/// Run every doctor check, returning the rows in a stable order.
///
/// `fix` lets the two narrowly-scoped repair checks act: the explicit offline
/// run-lock fence migration ([`check_run_lock_fence`]) and an unrecoverable stale
/// rewind marker ([`check_workspace_rewind_marker`]). Every other check remains
/// read-only.
///
/// Async because the backend check now spawns a real `<base> --version` probe
/// (via `umadev_host::probe_all`) so it agrees with the run path instead of a
/// PATH-only heuristic. Fail-open: a probe error degrades to a Warning row,
/// never a hard failure.
pub async fn run_all(workspace: &Path, fix: bool) -> Vec<CheckResult> {
    let mut results = vec![
        check_binary_identity(),
        check_embedded_spec(),
        check_workspace_writable(workspace),
        check_run_lock_fence(workspace, fix),
        check_workspace_rewind_marker(workspace, fix),
        check_spec_manifest(workspace),
    ];
    results.push(check_ai_backends().await);
    // Distinct from the reachability check above: when `claude-code` is the
    // SELECTED backend, confirm UmaDev's NON-INTERACTIVE drive of `claude` can
    // actually authenticate (an interactive `claude login` alone is NOT enough —
    // see `check_claude_noninteractive_auth`). The configured backend is read from
    // the user config (fail-open to `None` / not-applicable).
    let configured_backend = umadev_tui::config::load().backend;
    results.push(check_claude_noninteractive_auth(
        configured_backend.as_deref(),
    ));
    results.push(check_git());
    results.push(check_user_config());
    results.push(check_claude_hook(workspace));
    results.push(check_kimi_hook(workspace, configured_backend.as_deref()));
    results.push(check_kimi_windows_shell(configured_backend.as_deref()));
    results.push(check_delivery_readiness(workspace));
    results.push(check_ecosystem(workspace));
    results.push(check_npm_install());
    results.push(check_embed_model());
    results.push(check_node_version());
    results
}

/// Diagnose and explicitly migrate the one-way v2 run-lock compatibility fence.
///
/// This is intentionally an **offline** repair. Released legacy clients do not
/// share the v2 kernel guard, so no new binary can prove that a legacy process
/// has not already entered its old check→unlink takeover window. `--fix` is the
/// user's explicit assertion that every older UmaDev process has been stopped
/// and will remain stopped during migration. The agent layer still refuses a
/// live/unattributable recorded owner and rewrites only a provably-dead legacy
/// row or a non-empty prefix of the exact v2 fence.
fn check_run_lock_fence(workspace: &Path, fix: bool) -> CheckResult {
    use umadev_agent::run_lock::{
        RunLockFenceMigration as Migration, RunLockFenceStatus as FenceStatus,
    };

    let name = umadev_i18n::tl("doctor.run_lock_fence_name").to_string();
    match umadev_agent::run_lock::inspect_fence(workspace) {
        Ok(FenceStatus::Absent) => CheckResult {
            name,
            status: Status::Passed,
            detail: umadev_i18n::tl("doctor.run_lock_fence_absent").to_string(),
        },
        Ok(FenceStatus::Current) => CheckResult {
            name,
            status: Status::Passed,
            detail: umadev_i18n::tl("doctor.run_lock_fence_current").to_string(),
        },
        Ok(FenceStatus::LegacyOrIncomplete) if !fix => CheckResult {
            name,
            status: Status::Failed,
            detail: umadev_i18n::tl("doctor.run_lock_fence_needs_fix").to_string(),
        },
        Ok(FenceStatus::LegacyOrIncomplete) => {
            match umadev_agent::run_lock::migrate_fence(workspace) {
                Ok(Migration::MigratedLegacy) => CheckResult {
                    name,
                    status: Status::Warning,
                    detail: umadev_i18n::tl("doctor.run_lock_fence_migrated").to_string(),
                },
                Ok(Migration::RepairedPartial) => CheckResult {
                    name,
                    status: Status::Warning,
                    detail: umadev_i18n::tl("doctor.run_lock_fence_repaired").to_string(),
                },
                Ok(Migration::AlreadyCurrent) => CheckResult {
                    name,
                    status: Status::Passed,
                    detail: umadev_i18n::tl("doctor.run_lock_fence_current").to_string(),
                },
                Ok(Migration::Absent) => CheckResult {
                    name,
                    status: Status::Passed,
                    detail: umadev_i18n::tl("doctor.run_lock_fence_absent").to_string(),
                },
                Err(error) => CheckResult {
                    name,
                    status: Status::Failed,
                    detail: umadev_i18n::tlf(
                        "doctor.run_lock_fence_refused",
                        &[&error.to_string()],
                    ),
                },
            }
        }
        Err(error) => CheckResult {
            name,
            status: Status::Failed,
            detail: umadev_i18n::tlf("doctor.run_lock_fence_refused", &[&error.to_string()]),
        },
    }
}

/// Doctor row name for the local embedding-model presence check.
const EMBED_MODEL_CHECK: &str = "local embedding model";

/// Doctor row name for the Node.js runtime version check.
const NODE_CHECK: &str = "node.js runtime";

/// Smallest plausible `model.safetensors`. A real `multilingual-e5-small` is tens
/// of MB (fp16 ~224MB); anything under 1 MiB is a truncated/garbage download, not
/// a usable model. Mirrors `umadev-knowledge`'s `MIN_SAFETENSORS_BYTES`.
const EMBED_MODEL_MIN_SAFETENSORS_BYTES: u64 = 1_048_576;
const EMBED_MODEL_MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const EMBED_MODEL_MAX_TOKENIZER_BYTES: u64 = 64 * 1024 * 1024;
const EMBED_MODEL_MAX_WEIGHTS_BYTES: u64 = 512 * 1024 * 1024;
const SAFETENSORS_PREFIX_BYTES: u64 = 8;

/// Minimum Node.js major version the npm launcher (`bin/cli.js`) and its `engines`
/// floor require. Below this the launch shim prints an upgrade notice; the doctor
/// warns so an npm-managed user learns before hitting it.
const MIN_NODE_MAJOR: u64 = 18;

/// Resolve the directory the runtime will look for the local embedding model in,
/// mirroring `umadev-knowledge`'s `model_dir()`: the `UMADEV_EMBED_MODEL_DIR`
/// override (set by the npm `bin/cli.js` shim) first, then the zero-config
/// `~/.umadev/embed-model` fallback that a `cargo install` user would populate by
/// hand. Returns `None` when neither exists.
fn embed_model_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var("UMADEV_EMBED_MODEL_DIR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        let p = PathBuf::from(d);
        if RootedDir::open_no_follow(&p).is_ok() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.is_empty())?;
    let p = PathBuf::from(home).join(".umadev").join("embed-model");
    RootedDir::open_no_follow(&p).ok().map(|_| p)
}

/// Mirror the runtime's cheap structural gate without loading the model.
fn embed_model_present(dir: Option<&Path>) -> bool {
    let Some(dir) = dir else {
        return false;
    };
    let Ok(root) = RootedDir::open_no_follow(dir) else {
        return false;
    };
    json_model_sidecar_present(
        &root,
        Path::new("config.json"),
        EMBED_MODEL_MAX_CONFIG_BYTES,
    ) && json_model_sidecar_present(
        &root,
        Path::new("tokenizer.json"),
        EMBED_MODEL_MAX_TOKENIZER_BYTES,
    ) && safetensors_model_present(&root)
}

fn json_model_sidecar_present(root: &RootedDir, path: &Path, max_bytes: u64) -> bool {
    let Ok(Some(size)) = root.regular_file_len(path) else {
        return false;
    };
    if size == 0 || size > max_bytes {
        return false;
    }
    root.read_prefix(path, 16)
        .ok()
        .and_then(|prefix| prefix.into_iter().find(|byte| !byte.is_ascii_whitespace()))
        == Some(b'{')
}

fn safetensors_model_present(root: &RootedDir) -> bool {
    let path = Path::new("model.safetensors");
    let Ok(Some(size)) = root.regular_file_len(path) else {
        return false;
    };
    if !(EMBED_MODEL_MIN_SAFETENSORS_BYTES..=EMBED_MODEL_MAX_WEIGHTS_BYTES).contains(&size) {
        return false;
    }
    let Ok(prefix) = root.read_prefix(path, SAFETENSORS_PREFIX_BYTES) else {
        return false;
    };
    let Ok(prefix) = <[u8; 8]>::try_from(prefix) else {
        return false;
    };
    let header_len = u64::from_le_bytes(prefix);
    header_len > 0 && SAFETENSORS_PREFIX_BYTES.saturating_add(header_len) <= size
}

/// Turn "is the local model present?" into a doctor row. Present → PASS (full
/// hybrid vector+BM25 retrieval). Absent → WARN with the honest degrade note: a
/// `cargo install` / self-updated binary never had the model fetched (only the npm
/// shim downloads it), so retrieval silently ran keyword-only. Never a FAIL —
/// missing model is fail-open by contract.
fn embed_model_row(present: bool) -> CheckResult {
    if present {
        CheckResult {
            name: EMBED_MODEL_CHECK.to_string(),
            status: Status::Passed,
            detail: "present — hybrid vector + BM25 retrieval is active".to_string(),
        }
    } else {
        CheckResult {
            name: EMBED_MODEL_CHECK.to_string(),
            status: Status::Warning,
            detail:
                "not found — knowledge retrieval falls back to keyword-only (BM25). npm installs \
                 fetch it on the next `umadev run`, `umadev quick`, `umadev continue`, `umadev \
                 redo`, or `umadev revise`; for a cargo/native/manual install, place compatible \
                 config.json + tokenizer.json + model.safetensors under ~/.umadev/embed-model (or \
                 set UMADEV_EMBED_MODEL_DIR). `umadev update` updates UmaDev itself and does not \
                 install this model."
                    .to_string(),
        }
    }
}

/// Doctor row: is the local embedding model present so hybrid retrieval works, or
/// is the host silently degraded to keyword-only search?
fn check_embed_model() -> CheckResult {
    embed_model_row(embed_model_present(embed_model_dir().as_deref()))
}

/// Extract the major version from `node --version` output (e.g. `"v18.17.0\n"` →
/// `Some(18)`). Returns `None` for empty/garbage output.
fn parse_node_major(raw: &str) -> Option<u64> {
    let trimmed = raw.trim().trim_start_matches('v');
    trimmed.split('.').next()?.parse::<u64>().ok()
}

/// Turn the `node --version` result into a doctor row. `raw` is the command's
/// stdout, or `None` when `node` is not on PATH / failed. Node is required only for
/// npm-managed installs (the `bin/cli.js` launcher) and `umadev update` via npm, so
/// its absence is informational, not a failure. A version BELOW the floor is a
/// warning — that is exactly the case where the launch shim now prints an upgrade
/// notice instead of a cryptic parse error.
fn node_version_row(raw: Option<&str>) -> CheckResult {
    let name = NODE_CHECK.to_string();
    let Some(raw) = raw else {
        return CheckResult {
            name,
            status: Status::Passed,
            detail: format!(
                "node not on PATH — only npm-managed installs and `umadev update` via npm need it \
                 (Node >= {MIN_NODE_MAJOR})"
            ),
        };
    };
    match parse_node_major(raw) {
        Some(major) if major >= MIN_NODE_MAJOR => CheckResult {
            name,
            status: Status::Passed,
            detail: format!("node {} — meets the >= {MIN_NODE_MAJOR} floor", raw.trim()),
        },
        Some(major) => CheckResult {
            name,
            status: Status::Warning,
            detail: format!(
                "node {} is below the supported floor (>= {MIN_NODE_MAJOR}). The npm launcher needs \
                 Node >= {MIN_NODE_MAJOR}; upgrade Node.js to avoid a startup failure.",
                major
            ),
        },
        None => CheckResult {
            name,
            status: Status::Warning,
            detail: format!("could not parse `node --version` output ({:?})", raw.trim()),
        },
    }
}

/// Doctor row: the Node.js runtime version, warning when it is below the npm
/// launcher's supported floor.
fn check_node_version() -> CheckResult {
    const NODE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    const NODE_PROBE_STDOUT_BYTES: usize = 1024;
    const NODE_PROBE_STDERR_BYTES: usize = 4 * 1024;

    let mut command = umadev_host::std_command("node");
    command.arg("--version").env("CI", "1");
    let output = match umadev_process::run_bounded_std_command(
        command,
        umadev_process::BoundedCommandOptions {
            timeout: NODE_PROBE_TIMEOUT,
            stdout_bytes: NODE_PROBE_STDOUT_BYTES,
            stderr_bytes: NODE_PROBE_STDERR_BYTES,
            reader_grace: std::time::Duration::from_millis(500),
        },
    ) {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return node_version_row(None);
        }
        Err(_) => return node_probe_warning("could not start the version probe"),
    };
    if output.timed_out {
        return node_probe_warning("version probe timed out");
    }
    if output.stdout_truncated || output.stderr_truncated {
        return node_probe_warning("version probe output exceeded its safety limit");
    }
    if !output.status.is_some_and(|status| status.success()) {
        return node_probe_warning("version probe exited unsuccessfully");
    }
    let Ok(raw) = std::str::from_utf8(&output.stdout) else {
        return node_probe_warning("version probe returned non-UTF-8 output");
    };
    node_version_row(Some(raw))
}

fn node_probe_warning(reason: &str) -> CheckResult {
    CheckResult {
        name: NODE_CHECK.to_string(),
        status: Status::Warning,
        detail: format!("could not safely inspect `node --version`: {reason}"),
    }
}

/// Check whether `git` is available — `/checkpoint` and `/rewind` use a shadow
/// git repo. Missing git just disables checkpoints (fail-open), so this is a
/// Warning, not an error.
fn check_git() -> CheckResult {
    if which_on_path("git") {
        CheckResult {
            name: "git (file checkpoints)".to_string(),
            status: Status::Passed,
            detail: "found — /checkpoint and /rewind are available".to_string(),
        }
    } else {
        CheckResult {
            name: "git (file checkpoints)".to_string(),
            status: Status::Warning,
            detail: "git not on PATH — phase-level file checkpoints (/checkpoint, /rewind) are disabled. Install git to enable them.".to_string(),
        }
    }
}

/// Check that the user config (`~/.umadev/config.toml`) parses. A corrupt config
/// silently resets to defaults at load time, losing the user's backend / model /
/// provider — surface it here with the exact file to fix instead.
fn check_user_config() -> CheckResult {
    let path = umadev_tui::config::default_path();
    match umadev_tui::config::load_strict(&path) {
        Ok(_) => CheckResult {
            name: "user config".to_string(),
            status: Status::Passed,
            detail: if path.is_file() {
                format!("valid: {}", path.display())
            } else {
                "none yet — the first run will create it".to_string()
            },
        },
        Err(e) => CheckResult {
            name: "user config".to_string(),
            status: Status::Warning,
            detail: format!(
                "config.toml is corrupt ({e}) — UmaDev would reset to defaults and lose your backend / model. Fix or delete {}.",
                path.display()
            ),
        },
    }
}

/// Check whether the Claude Code PreToolUse governance hook is registered in the
/// machine-local project settings. Older UmaDev versions wrote an absolute binary
/// path into shareable `settings.json`; detect that legacy state and ask the user
/// to migrate rather than reporting a misleading green result.
fn check_claude_hook(workspace: &Path) -> CheckResult {
    let claude_dir = workspace.join(".claude");
    let local_path = claude_dir.join("settings.local.json");
    let shared_path = claude_dir.join("settings.json");
    if !local_path.is_file() && !shared_path.is_file() {
        return CheckResult {
            name: "Claude Code hook".to_string(),
            status: Status::Passed,
            detail: "no Claude project settings (real-time governance off; quality-gate hard block still active)"
                .to_string(),
        };
    }

    let inspect = |path: &Path| -> Result<Vec<(&'static str, &'static str, String)>, String> {
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let content =
            doctor_utf8(path, MAX_DOCTOR_CONFIG_BYTES).map_err(|error| error.to_string())?;
        let value = serde_json::from_str::<serde_json::Value>(&content)
            .map_err(|error| format!("invalid JSON: {error}"))?;
        Ok(value
            .get("hooks")
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|hooks| {
                ["PreToolUse", "PostToolUse"]
                    .into_iter()
                    .filter_map(|phase| {
                        hooks
                            .get(phase)
                            .and_then(serde_json::Value::as_array)
                            .map(|groups| (phase, groups))
                    })
            })
            .flat_map(|(phase, groups)| groups.iter().map(move |group| (phase, group)))
            .filter_map(|(phase, group)| {
                group
                    .get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .map(|handlers| (phase, handlers))
            })
            .flat_map(|(phase, handlers)| handlers.iter().map(move |handler| (phase, handler)))
            .filter_map(|(phase, handler)| {
                let name = crate::hook::umadev_hook_name(handler, None)?;
                let command = handler.get("command")?.as_str()?.to_string();
                Some((phase, name, command))
            })
            .collect())
    };

    let local = match inspect(&local_path) {
        Ok(value) => value,
        Err(error) => {
            return CheckResult {
                name: "Claude Code hook".to_string(),
                status: Status::Warning,
                detail: format!(
                    "{} cannot be read safely ({error}). Fix it, then run `umadev install --base claude-code`.",
                    local_path.display()
                ),
            };
        }
    };
    if !local.is_empty() {
        let missing: Vec<_> = [
            ("PreToolUse", "pre-write"),
            ("PreToolUse", "pre-bash"),
            ("PostToolUse", "tool-audit"),
        ]
        .into_iter()
        .filter(|(expected_phase, expected_name)| {
            !local
                .iter()
                .any(|(phase, name, _)| phase == expected_phase && name == expected_name)
        })
        .map(|(phase, name)| format!("{phase}/{name}"))
        .collect();
        if !missing.is_empty() {
            return CheckResult {
                name: "Claude Code hook".to_string(),
                status: Status::Warning,
                detail: format!(
                    "{} has an incomplete UmaDev hook set (missing {}). Run `umadev install --base claude-code` to repair it.",
                    local_path.display(),
                    missing.join(", ")
                ),
            };
        }
        return if local
            .iter()
            .all(|(_, _, command)| hook_command_resolves(command))
        {
            CheckResult {
                name: "Claude Code hook".to_string(),
                status: Status::Passed,
                detail: format!(
                    "machine-local PreToolUse and PostToolUse governance hooks registered: {}",
                    local_path.display()
                ),
            }
        } else {
            CheckResult {
                name: "Claude Code hook".to_string(),
                status: Status::Warning,
                detail: format!(
                    "{} registers UmaDev but its command does not resolve to a live executable. Run `umadev install --base claude-code` to repair it.",
                    local_path.display()
                ),
            }
        };
    }

    match inspect(&shared_path) {
        Err(error) => CheckResult {
            name: "Claude Code hook".to_string(),
            status: Status::Warning,
            detail: format!(
                "{} cannot be read safely ({error}). Fix it, then run `umadev install --base claude-code`.",
                shared_path.display()
            ),
        },
        Ok(hooks) if !hooks.is_empty() => CheckResult {
            name: "Claude Code hook".to_string(),
            status: Status::Warning,
            detail: format!(
                "legacy UmaDev hook is stored in shareable {}. Run `umadev install --base claude-code` to migrate it to settings.local.json and safe exec-form arguments.",
                shared_path.display()
            ),
        },
        Ok(_) => CheckResult {
            name: "Claude Code hook".to_string(),
            status: Status::Warning,
            detail: "Claude project settings exist but the UmaDev PreToolUse hook is not registered (absent). Run `umadev install --base claude-code` for real-time governance."
                .to_string(),
        },
    }
}

/// Check the three exact Kimi Code native hook rows for this project. Kimi's
/// registry is user-level, so the commands themselves carry a canonical project
/// scope; rows belonging to another project must not satisfy this check.
fn check_kimi_hook(workspace: &Path, backend: Option<&str>) -> CheckResult {
    let name = "Kimi Code hooks".to_string();
    if backend != Some("kimi-code") {
        return CheckResult {
            name,
            status: Status::Passed,
            detail: "not applicable — Kimi Code is not the selected backend".to_string(),
        };
    }
    match crate::hook::kimi_hook_registration(workspace) {
        Ok((path, true)) if which_on_path("umadev") => CheckResult {
            name,
            status: Status::Passed,
            detail: format!(
                "project-scoped PreToolUse/PostToolUse rows active: {}",
                path.display()
            ),
        },
        Ok((path, true)) => CheckResult {
            name,
            status: Status::Warning,
            detail: format!(
                "the exact project-scoped rows exist in {}, but their upgrade-safe `umadev` command is not on PATH. Repair PATH, then run `umadev install --host kimi-code`.",
                path.display()
            ),
        },
        Ok((path, false)) => CheckResult {
            name,
            status: Status::Warning,
            detail: format!(
                "the exact hook rows for this project are absent from {}. Run `umadev install --host kimi-code` from this project.",
                path.display()
            ),
        },
        Err(error) => CheckResult {
            name,
            status: Status::Warning,
            detail: format!(
                "Kimi Code hook configuration cannot be verified ({error}). Fix its config.toml, then run `umadev install --host kimi-code`."
            ),
        },
    }
}

/// Kimi Code executes tools and hooks through Git Bash on Windows. Its own ACP
/// process can initialize before the first shell tool exposes a missing/custom
/// shell, so surface this prerequisite in doctor rather than during a build.
fn check_kimi_windows_shell(backend: Option<&str>) -> CheckResult {
    let name = "Kimi Code shell".to_string();
    if backend != Some("kimi-code") {
        return CheckResult {
            name,
            status: Status::Passed,
            detail: "not applicable — Kimi Code is not the selected backend".to_string(),
        };
    }
    #[cfg(not(windows))]
    {
        CheckResult {
            name,
            status: Status::Passed,
            detail: "native shell available (Git Bash is required only on Windows)".to_string(),
        }
    }
    #[cfg(windows)]
    {
        match find_kimi_windows_shell() {
            Some(path) => CheckResult {
                name,
                status: Status::Passed,
                detail: format!("Git Bash available: {}", path.display()),
            },
            None => CheckResult {
                name,
                status: Status::Warning,
                detail: "Git Bash was not found. Install Git for Windows; for a custom install set KIMI_SHELL_PATH to the absolute bash.exe path before starting UmaDev."
                    .to_string(),
            },
        }
    }
}

#[cfg(windows)]
fn find_kimi_windows_shell() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("KIMI_SHELL_PATH")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
    {
        return Some(path);
    }
    let mut candidates = Vec::new();
    for root in [
        std::env::var_os("ProgramFiles"),
        std::env::var_os("ProgramFiles(x86)"),
    ]
    .into_iter()
    .flatten()
    {
        candidates.push(std::path::PathBuf::from(root.clone()).join("Git/bin/bash.exe"));
        candidates.push(std::path::PathBuf::from(root).join("Git/usr/bin/bash.exe"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(std::path::PathBuf::from(&local).join("Programs/Git/bin/bash.exe"));
        candidates.push(std::path::PathBuf::from(local).join("Programs/Git/usr/bin/bash.exe"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("bash.exe"));
            if dir
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("cmd"))
            {
                if let Some(root) = dir.parent() {
                    candidates.push(root.join("bin/bash.exe"));
                    candidates.push(root.join("usr/bin/bash.exe"));
                }
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Does this UmaDev hook `command` point at a program that resolves on disk? The
/// program token is everything before the `hook <sub>` tail; an absolute/relative
/// path must be an existing file, a bare name must be on `PATH`. Fail-open: an
/// empty token is treated as unresolved (a Warning, never a crash).
fn hook_command_resolves(command: &str) -> bool {
    let cmd = command.trim();
    let prog = ["hook pre-write", "hook pre-bash", "hook tool-audit"]
        .iter()
        .find_map(|s| cmd.strip_suffix(s))
        .map_or(cmd, str::trim_end);
    if prog.is_empty() {
        return false;
    }
    let path = Path::new(prog);
    if prog.contains(std::path::MAIN_SEPARATOR) || path.is_absolute() {
        return path.is_file();
    }
    which_on_path(prog)
}

fn check_spec_manifest(workspace: &Path) -> CheckResult {
    // UD-META-001: a conformant workspace declares its spec level.
    match umadev_agent::SpecManifest::read_from(workspace) {
        Some(m) if m.spec_version == umadev_spec::SPEC_VERSION => CheckResult {
            name: "spec manifest (UD-META-001)".to_string(),
            status: Status::Passed,
            detail: format!(
                "umadev.yaml present: level {}, profile {}",
                m.level.as_str(),
                m.profile.as_str()
            ),
        },
        Some(m) => CheckResult {
            name: "spec manifest (UD-META-001)".to_string(),
            status: Status::Warning,
            detail: format!(
                "umadev.yaml declares spec `{}` but this binary speaks `{}`",
                m.spec_version,
                umadev_spec::SPEC_VERSION
            ),
        },
        None => CheckResult {
            name: "spec manifest (UD-META-001)".to_string(),
            status: Status::Warning,
            detail: "no umadev.yaml — run `umadev init` to declare conformance".to_string(),
        },
    }
}

/// Check which of the five first-class host CLIs are installed and
/// usable. This is the most important doctor check for enterprise use —
/// without a backend, UmaDev falls back to offline templates.
///
/// Uses `umadev_host::probe_all` — the SAME detection the run path and the TUI
/// startup panel use — so the doctor never reports a false "not detected" for a
/// base that `run` can actually drive. `probe_all` resolves each base via
/// `umadev_host::resolve_program` (PATH first, then known install dirs:
/// Homebrew / volta / `~/.<base>/bin` / `…/Programs`, plus the
/// `UMADEV_<NAME>_BIN` override) and then runs a real `--version`, which is the
/// final installed-or-not arbiter. An unhealthy probe is surfaced with its exact
/// diagnostic (including an unsafe-version upgrade command), never a hard FAIL.
async fn check_ai_backends() -> CheckResult {
    let statuses = umadev_host::probe_all().await;

    let ready: Vec<&str> = statuses
        .iter()
        .filter(|s| s.probe.is_ready())
        .map(|s| s.id)
        .collect();
    // Found-but-broken bases (e.g. an old `--version` that errors): worth
    // surfacing so the user fixes the install rather than thinking it's missing.
    let unhealthy: Vec<String> = statuses
        .iter()
        .filter_map(|s| match &s.probe {
            umadev_host::ProbeResult::Unhealthy { detail } => Some(format!("{}: {detail}", s.id)),
            _ => None,
        })
        .collect();

    if ready.is_empty() {
        let mut detail = String::from(
            "No supported base CLI detected. Install and authenticate one of: claude-code, codex, opencode, grok-build, kimi-code. It brings its OWN model and credentials; without a base, UmaDev falls back to offline templates.",
        );
        if !unhealthy.is_empty() {
            detail.push_str(&format!(
                " Found but unavailable: {}",
                unhealthy.join(" | ")
            ));
        }
        return CheckResult {
            name: "AI host backends".to_string(),
            status: Status::Warning,
            detail,
        };
    }

    let mut detail = ready_backends_detail(&ready);
    if !unhealthy.is_empty() {
        detail.push_str(&format!(
            " Also found but unhealthy: {}.",
            unhealthy.join(" | ")
        ));
    }
    let status = if unhealthy.is_empty() {
        Status::Passed
    } else {
        Status::Warning
    };
    CheckResult {
        name: "AI host backends".to_string(),
        status,
        detail,
    }
}

fn ready_backends_detail(ready: &[&str]) -> String {
    format!(
        "{} backend(s) detected: {}. Start `umadev` to use or switch bases in the TUI, or run `umadev run \"<requirement>\" --backend {}` for a one-shot pipeline. (Login is verified when a run starts — make sure you've logged into the CLI.)",
        ready.len(),
        ready.join(", "),
        ready[0]
    )
}

/// Environment credentials that let `claude` authenticate in UmaDev's
/// NON-INTERACTIVE drive (`claude --print …`), in the order we report them.
/// `CLAUDE_CODE_OAUTH_TOKEN` (minted by `claude setup-token`) is the canonical
/// fix for the user-reported 401; the API-key / custom-auth-token / cloud-routing
/// vars also satisfy headless auth, so any one of them clears the check.
const CLAUDE_NONINTERACTIVE_AUTH_ENV: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// `true` iff `key` is set to a non-empty (after-trim) value in the environment.
fn env_is_set(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !v.trim().is_empty())
}

/// Check whether the `claude-code` backend can authenticate UmaDev's
/// **non-interactive** calls to `claude`.
///
/// The trap this catches: UmaDev drives `claude` headlessly (`claude --print
/// --output-format stream-json …`), and that path authenticates from an **env
/// credential**, NOT from the interactive `claude login` session. A user who has
/// only run `claude login` therefore looks "logged in" to the reachability /
/// `claude auth status` probe (so `check_ai_backends` PASSes), yet UmaDev's
/// background turn comes back `401 Invalid authentication credentials` at
/// runtime. The long-lived token from `claude setup-token` (exported as
/// `CLAUDE_CODE_OAUTH_TOKEN`) is what makes the headless call work.
///
/// This is a distinct, clearly-worded row so the user sees the gap BEFORE a
/// run 401s instead of after. Fail-open by contract: it is a **Warning**, never
/// a hard FAIL (a missing token doesn't stop `doctor`), and it never penalizes
/// the other backends — for any `backend` other than `claude-code` (including a
/// not-yet-picked `None`) it is an informational PASS.
fn check_claude_noninteractive_auth(backend: Option<&str>) -> CheckResult {
    let name = "Claude non-interactive auth".to_string();
    // Only meaningful when claude-code is the selected backend. Every other base,
    // offline, and not-yet-picked are unaffected (fail-open: never a spurious WARN
    // for a backend that doesn't use this credential).
    if backend != Some("claude-code") {
        return CheckResult {
            name,
            status: Status::Passed,
            detail: match backend {
                Some(b) => format!("not applicable — selected backend is `{b}`"),
                None => "not applicable — no backend selected yet".to_string(),
            },
        };
    }
    // claude-code IS selected: look for a credential that works headlessly.
    match CLAUDE_NONINTERACTIVE_AUTH_ENV
        .iter()
        .copied()
        .find(|&k| env_is_set(k))
    {
        Some(var) => CheckResult {
            name,
            status: Status::Passed,
            detail: format!(
                "{var} is set — UmaDev's non-interactive `claude` calls can authenticate."
            ),
        },
        None => CheckResult {
            name,
            status: Status::Warning,
            detail: "CLAUDE_CODE_OAUTH_TOKEN is not set. UmaDev drives `claude` NON-INTERACTIVELY \
                 (`claude --print …`), which needs a long-lived token — an interactive `claude \
                 login` alone can still return `401 Invalid authentication credentials` on \
                 UmaDev's background calls. Fix: run `claude setup-token` to mint a long-lived \
                 token, then export CLAUDE_CODE_OAUTH_TOKEN=<token> (add it to your shell rc, \
                 e.g. ~/.zshrc / ~/.bashrc, so it persists across sessions)."
                .to_string(),
        },
    }
}

/// Name of the npm-delivery check row (shared by the check and its tests).
const NPM_CHECK: &str = "npm install health";

/// Is `path` inside an npm `node_modules` tree? That is how we recognise an
/// npm-delivered umadev (the JS shim exec's the platform sub-package binary at
/// `…/node_modules/@umatech/cli-<plat>/bin/umadev`) versus a source / release
/// binary the user placed on PATH themselves.
fn under_node_modules(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

/// Is `path` inside an npm **global** tree (`<prefix>/lib/node_modules/…`, or
/// `<prefix>/node_modules/…` on Windows) as opposed to a project-local
/// `./node_modules`? Global installs put a command on PATH; local ones do not.
fn under_global_node_modules(path: &Path) -> bool {
    let parts: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts
        .windows(2)
        .any(|w| w[0] == "lib" && w[1] == "node_modules")
}

/// The uid that owns `path`, or `None` off unix / on a stat error.
#[cfg(unix)]
fn owner_uid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|m| m.uid())
}
#[cfg(not(unix))]
fn owner_uid(_path: &Path) -> Option<u32> {
    None
}

/// The current user's uid, derived dependency-free from the ownership of `$HOME`
/// (stat'ing our own home is a std-only stand-in for `geteuid()`, which would
/// otherwise pull in `libc`). `None` when there is no home dir or off unix.
fn current_uid() -> Option<u32> {
    if !cfg!(unix) {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    owner_uid(Path::new(&home))
}

/// Walk `dir` breadth-first, up to `cap` entries, and return the first entry
/// owned by root (uid 0). Bounded on purpose: `~/.npm/_cacache` can hold tens of
/// thousands of files and the doctor must stay instant. Fail-open: any I/O error
/// is treated as "nothing found".
#[cfg(unix)]
fn first_root_owned(dir: &Path, cap: usize) -> Option<std::path::PathBuf> {
    let mut queue = std::collections::VecDeque::from([dir.to_path_buf()]);
    let mut seen = 0usize;
    while let Some(next) = queue.pop_front() {
        if seen >= cap {
            return None;
        }
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen >= cap {
                return None;
            }
            let path = entry.path();
            if owner_uid(&path) == Some(0) {
                return Some(path);
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                queue.push_back(path);
            }
        }
    }
    None
}
#[cfg(not(unix))]
fn first_root_owned(_dir: &Path, _cap: usize) -> Option<std::path::PathBuf> {
    None
}

/// Check 12: npm delivery health — the two ways an npm-installed umadev ends up
/// "installed but not working", both of which are npm/OS setup rather than a
/// umadev bug, and both of which have an exact fix we can print:
///
/// 1. **Root-owned global install** (`sudo npm i -g @umatech/umadev`). It works *today*,
///    but every later NON-root npm operation on that prefix — including the
///    tree-wide `npm update -g` — then fails with `EACCES`, which wedges the
///    user's *other* global packages (their base CLI: `@anthropic-ai/claude-code`,
///    `@openai/codex`) too, since npm aborts the whole transaction. Same for a
///    root-owned `~/.npm` cache (what `sudo -E npm …` leaves behind).
/// 2. **Local install** (`npm i @umatech/umadev`, no `-g`). npm deliberately does not put
///    a locally-installed command on PATH — `umadev` then reports "command not
///    found" and the install looks broken when it is in fact fine: it is reached
///    via `npx umadev`.
///
/// Fail-open: anything we cannot determine (no home dir, stat error, non-npm
/// build) reports `Passed` — the doctor never invents a problem.
fn check_npm_install() -> CheckResult {
    let row = |status: Status, detail: String| CheckResult {
        name: NPM_CHECK.to_string(),
        status,
        detail,
    };
    let Ok(exe) = std::env::current_exe() else {
        return row(
            Status::Passed,
            "could not resolve the running binary — skipped".to_string(),
        );
    };
    if !under_node_modules(&exe) {
        return row(
            Status::Passed,
            "not an npm-delivered binary (source / release build) — nothing to check".to_string(),
        );
    }

    // A local (non-global) install: npm never links its command onto PATH. This
    // is npm's design, not a bug — but nothing tells the user, so `umadev` reads
    // as "did not install". Point them at the invocation that does work.
    if !under_global_node_modules(&exe) && !which_on_path("umadev") {
        return row(
            Status::Warning,
            "installed locally (`npm i @umatech/umadev`, no `-g`): npm does NOT put a local command on PATH, \
             so bare `umadev` says \"command not found\" — the install is fine. Run it as `npx umadev`, \
             or install it as a command with a user-owned prefix (no sudo): \
             `npm config set prefix ~/.npm-global && npm i -g @umatech/umadev` \
             (then add ~/.npm-global/bin to PATH)."
                .to_string(),
        );
    }

    let Some(me) = current_uid() else {
        return row(
            Status::Passed,
            "npm-delivered install detected; ownership checks are unix-only".to_string(),
        );
    };
    // Running AS root — a root-owned tree is then self-consistent; say nothing.
    if me == 0 {
        return row(
            Status::Passed,
            "running as root — npm ownership checks not applicable".to_string(),
        );
    }

    // (1) A root-owned install tree means it was installed with `sudo`.
    if owner_uid(&exe) == Some(0) {
        return row(
            Status::Warning,
            "installed with `sudo` (the binary is root-owned). It runs, but every later NON-root \
             npm command on that prefix — including the tree-wide `npm update -g` — will fail with \
             EACCES, and npm aborts the WHOLE transaction, so your other global packages (e.g. your \
             base CLI @anthropic-ai/claude-code / @openai/codex) can no longer be updated either. \
             Fix — reinstall without sudo into a user-owned prefix: \
             `sudo npm un -g @umatech/umadev` then `npm config set prefix ~/.npm-global` \
             (add ~/.npm-global/bin to PATH) then `npm i -g @umatech/umadev`."
                .to_string(),
        );
    }

    // (2) A root-owned npm cache (the `sudo -E npm …` footgun) breaks later
    // non-root installs of ANY package, not just umadev.
    if let Some(home) = std::env::var_os("HOME") {
        let cache = Path::new(&home).join(".npm");
        if cache.is_dir() {
            if let Some(hit) = first_root_owned(&cache, 4000) {
                return row(
                    Status::Warning,
                    format!(
                        "your npm cache has root-owned files (e.g. {}) — a past `sudo npm …`. Later \
                         non-root `npm install` of ANY package can fail with EACCES there. \
                         Fix: `sudo chown -R $(whoami) ~/.npm`.",
                        hit.display()
                    ),
                );
            }
        }
    }

    // (3) Globally installed, no sudo damage — but is the command reachable?
    if !which_on_path("umadev") {
        return row(
            Status::Warning,
            "installed globally but `umadev` is not on PATH — npm's global bin dir is not in your \
             shell PATH (common with Homebrew node). Add it: `export PATH=\"$(npm prefix -g)/bin:$PATH\"` \
             (put it in ~/.zshrc / ~/.bashrc to persist)."
                .to_string(),
        );
    }

    row(
        Status::Passed,
        "npm install is user-owned and on PATH".to_string(),
    )
}

/// Check if an executable is on PATH (without spawning a subprocess).
fn which_on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        // Check common executable extensions on the current platform.
        let candidates = if cfg!(windows) {
            vec![
                dir.join(format!("{cmd}.exe")),
                dir.join(format!("{cmd}.bat")),
                dir.join(format!("{cmd}.cmd")),
            ]
        } else {
            vec![dir.join(cmd)]
        };
        candidates.iter().any(|p| p.is_file())
    })
}

/// Return true iff every result in `results` is `Passed`.
#[must_use]
pub fn all_passed(results: &[CheckResult]) -> bool {
    results.iter().all(|r| r.status == Status::Passed)
}

fn check_binary_identity() -> CheckResult {
    let version = env!("CARGO_PKG_VERSION");
    let spec = umadev_spec::SPEC_VERSION;
    CheckResult {
        name: "binary identity".to_string(),
        status: Status::Passed,
        detail: format!("umadev {version}, conformant to {spec}"),
    }
}

fn check_embedded_spec() -> CheckResult {
    let spec_body = include_str!("../../../spec/UMADEV_HOST_SPEC_V1.md");
    if spec_body.is_empty() {
        return CheckResult {
            name: "embedded spec markdown".to_string(),
            status: Status::Failed,
            detail: "spec/UMADEV_HOST_SPEC_V1.md was empty at build time".to_string(),
        };
    }
    if !spec_body.contains("UMADEV_HOST_SPEC_V1") {
        return CheckResult {
            name: "embedded spec markdown".to_string(),
            status: Status::Warning,
            detail: format!(
                "embedded spec lacks the SPEC_VERSION marker ({} bytes)",
                spec_body.len()
            ),
        };
    }
    CheckResult {
        name: "embedded spec markdown".to_string(),
        status: Status::Passed,
        detail: format!("{} bytes, carries SPEC_VERSION marker", spec_body.len()),
    }
}

fn check_workspace_writable(workspace: &Path) -> CheckResult {
    let probe_name = format!(".umadev-doctor-probe-{}", std::process::id());
    let res = (|| -> std::io::Result<()> {
        let workspace = fs::canonicalize(workspace)?;
        if !umadev_state::fs::real_dir(&workspace) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace is not a real directory",
            ));
        }
        let probe = workspace.join(&probe_name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let mut file = options.open(&probe)?;
        if !umadev_state::fs::metadata_is_real_file(&file.metadata()?) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "writability probe is not a regular file",
            ));
        }
        let write_result = file.write_all(b"ok").and_then(|()| file.sync_data());
        drop(file);
        let remove_result = umadev_state::fs::remove_regular_file(&probe).map(|_| ());
        write_result?;
        remove_result?;
        Ok(())
    })();
    match res {
        Ok(()) => CheckResult {
            name: "workspace writable".to_string(),
            status: Status::Passed,
            detail: format!("write + delete OK at {}", workspace.display()),
        },
        Err(e) => CheckResult {
            name: "workspace writable".to_string(),
            status: Status::Failed,
            detail: format!("{} ({})", e, workspace.display()),
        },
    }
}

/// Check: a STALE TEMP-REWIND MARKER — the one condition that can wedge UmaDev permanently,
/// and the only check here that can repair anything.
///
/// The failure it catches: a marker left by a killed run, plus a `.umadev/checkpoints.git`
/// the user has since deleted (to reclaim disk, or by rsyncing the workspace without it).
/// The heal then finds a recorded head it cannot identify, correctly refuses to `reset --hard`
/// the work-tree to an unvalidated ref, and raises the workspace-in-the-past halt — on EVERY
/// process start. Every `umadev run` aborts immediately, forever. No verb cleared it, and this
/// doctor did not even look for it; the only escape was hand-`rm`ing a file nothing ever
/// mentioned.
///
/// The discrimination is the whole check, and it is not symmetric:
/// - **Recoverable** (the head IS a known checkpoint) → PASS, and `--fix` deliberately leaves
///   it ALONE. That marker is the only map back to the user's present; deleting it would strand
///   them in the past permanently, which is the mirror image of the bug being fixed.
/// - **Unrecoverable** (the head names nothing this workspace can identify) → FAIL, with the
///   repair named. `--fix` deletes the marker and clears the halt. It touches no file in the
///   work-tree — it only stops UmaDev from re-raising a stop it can never act on.
///
/// Fail-open: an unreadable marker is treated as unrecoverable (it is — nothing can act on it).
fn check_workspace_rewind_marker(workspace: &Path, fix: bool) -> CheckResult {
    use umadev_agent::checkpoint::TempRewindState;
    let name = umadev_i18n::tl("doctor.rewind_marker_name").to_string();
    match umadev_agent::checkpoint::clear_temp_rewind_state(workspace, !fix) {
        TempRewindState::Clean => CheckResult {
            name,
            status: Status::Passed,
            detail: umadev_i18n::tl("doctor.rewind_marker_clean").to_string(),
        },
        // The automatic heal can still do this itself — say so, and do NOT remove the marker
        // it needs. This is a PASS: nothing is broken, a restart genuinely fixes it.
        TempRewindState::Recoverable { head } => CheckResult {
            name,
            status: Status::Passed,
            detail: umadev_i18n::tlf("doctor.rewind_marker_recoverable", &[&head]),
        },
        TempRewindState::ClearedUnrecoverable { head } if fix => CheckResult {
            name,
            status: Status::Warning,
            detail: umadev_i18n::tlf("doctor.rewind_marker_cleared", &[&head]),
        },
        TempRewindState::ClearedUnrecoverable { head } => CheckResult {
            name,
            status: Status::Failed,
            detail: umadev_i18n::tlf("doctor.rewind_marker_unrecoverable", &[&head]),
        },
        TempRewindState::Invalid {
            detail,
            cleared: true,
        } => CheckResult {
            name,
            status: Status::Warning,
            detail: umadev_i18n::tlf("doctor.rewind_marker_invalid_cleared", &[&detail]),
        },
        TempRewindState::Invalid {
            detail,
            cleared: false,
        } => CheckResult {
            name,
            status: Status::Failed,
            detail: umadev_i18n::tlf("doctor.rewind_marker_invalid", &[&detail]),
        },
    }
}

/// Pretty-print one report block.
#[must_use]
pub fn render_report(workspace: &Path, results: &[CheckResult]) -> String {
    let mut out = String::new();
    out.push_str(&format!("umadev doctor — {}\n\n", workspace.display()));
    out.push_str("status | check\n");
    out.push_str("-------|------\n");
    for r in results {
        out.push_str(&format!("{:6} | {}\n", r.status.label(), r.name));
        out.push_str(&format!("       │  {}\n", r.detail));
    }
    let passed = results
        .iter()
        .filter(|r| r.status == Status::Passed)
        .count();
    let warn = results
        .iter()
        .filter(|r| r.status == Status::Warning)
        .count();
    let failed = results
        .iter()
        .filter(|r| r.status == Status::Failed)
        .count();
    out.push_str(&format!(
        "\n{passed} passed, {warn} warning, {failed} failed.\n"
    ));
    out
}

/// Check 7: delivery / deployment readiness. After a pipeline run reaches
/// the delivery phase, this verifies the worker produced a deployable state:
/// delivery notes with a `## Deploy command`, a build output directory, and at
/// least one deploy-platform CLI on PATH. Before any run it reports "not started".
fn check_delivery_readiness(workspace: &Path) -> CheckResult {
    let output = workspace.join("output");
    // No output dir → pipeline hasn't run; not an error, just informational.
    if !output.is_dir() {
        return CheckResult {
            name: "Deployment readiness".to_string(),
            status: Status::Passed,
            detail: "no run yet (run `umadev` and enter a requirement to start)".to_string(),
        };
    }
    // Find any delivery-notes file.
    let delivery_notes = fs::read_dir(&output).ok().and_then(|rd| {
        rd.filter_map(Result::ok)
            .take(MAX_DOCTOR_ENTRIES)
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("delivery-notes"))
            })
    });
    let Some(notes_path) = delivery_notes else {
        return CheckResult {
            name: "Deployment readiness".to_string(),
            status: Status::Passed,
            detail: "pipeline has not reached delivery phase yet".to_string(),
        };
    };
    let notes = doctor_utf8(&notes_path, MAX_DOCTOR_NOTES_BYTES).unwrap_or_default();
    // Does the worker record a concrete deploy command (not the placeholder)?
    let has_deploy_cmd = notes
        .split("## Deploy command")
        .nth(1)
        .is_some_and(|after| {
            after
                .lines()
                .any(|l| !l.trim().is_empty() && !l.trim().starts_with('_'))
        });
    // Is a deploy-platform CLI on PATH?
    let deploy_cli = ["vercel", "netlify", "wrangler"]
        .iter()
        .find(|c| which_on_path(c))
        .copied();
    let mut detail = String::new();
    let mut status = Status::Passed;
    if has_deploy_cmd {
        detail.push_str("delivery notes have a deploy command; ");
    } else {
        detail.push_str("delivery notes missing a concrete deploy command; ");
        status = Status::Warning;
    }
    if let Some(cli) = deploy_cli {
        detail.push_str(&format!("`{cli}` on PATH (run /deploy to ship)"));
    } else {
        detail.push_str(
            "no deploy CLI (vercel/netlify/wrangler) on PATH — install one to /deploy, \
             or run the recorded command manually",
        );
        if status == Status::Passed {
            status = Status::Warning;
        }
    }
    CheckResult {
        name: "Deployment readiness".to_string(),
        status,
        detail,
    }
}

/// Check 8: MCP / Skill / Knowledge ecosystem. Reports whether any MCP
/// servers, skills, or custom knowledge are configured so the user knows
/// the extension surface is available.
pub fn check_ecosystem(workspace: &Path) -> CheckResult {
    let mut parts: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // MCP servers. A file that EXISTS but won't parse is CORRUPT — don't fold
    // it into a cheerful "0 servers" (which `unwrap_or(0)` did): a malformed
    // `.mcp.json` means the host discovers NONE of the user's servers, so it's
    // a Warning, not silently OK.
    let mcp_path = workspace.join(".mcp.json");
    match doctor_utf8(&mcp_path, MAX_DOCTOR_CONFIG_BYTES) {
        Ok(text) if text.trim().is_empty() => {}
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => {
                let mcp_count = v
                    .get("mcpServers")
                    .and_then(|s| s.as_object())
                    .map_or(0, serde_json::Map::len);
                if mcp_count > 0 {
                    parts.push(format!("{mcp_count} MCP server(s) configured (.mcp.json)"));
                }
            }
            Err(e) => {
                warnings.push(format!(
                    ".mcp.json exists but isn't valid JSON ({e}) — the host will load NO MCP \
                     servers from it; fix or remove it"
                ));
            }
        },
        Err(_) => {} // missing file: nothing configured, not an error
    }

    // Skills.
    let skills_dir = workspace.join(".umadev").join("skills");
    let skill_count = std::fs::read_dir(&skills_dir).map_or(0, |rd| {
        rd.filter_map(Result::ok)
            .take(MAX_DOCTOR_ENTRIES)
            .filter(|entry| {
                std::fs::symlink_metadata(entry.path())
                    .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_dir(&metadata))
            })
            .count()
    });
    if skill_count > 0 {
        parts.push(format!(
            "{skill_count} skill(s) installed (.umadev/skills/)"
        ));
    }

    // Custom knowledge registry (`{ "entries": { name: ... } }`). Same rule:
    // a corrupt registry is a Warning, not a silent zero.
    let knowledge_reg = workspace.join(".umadev").join("knowledge.json");
    match doctor_utf8(&knowledge_reg, MAX_DOCTOR_CONFIG_BYTES) {
        Ok(text) if text.trim().is_empty() => {}
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => {
                let knowledge_count = v
                    .get("entries")
                    .and_then(|s| s.as_object())
                    .map_or(0, serde_json::Map::len);
                if knowledge_count > 0 {
                    parts.push(format!("{knowledge_count} custom knowledge set(s)"));
                }
            }
            Err(e) => {
                warnings.push(format!(
                    ".umadev/knowledge.json is corrupt ({e}) — custom knowledge won't load; \
                     fix or remove it"
                ));
            }
        },
        Err(_) => {}
    }

    // A corrupt config downgrades the whole check to Warning so the user is
    // told their extensions are silently failing.
    let (status, detail) = if warnings.is_empty() {
        let detail = if parts.is_empty() {
            "no extensions configured. Use `umadev mcp-manage install` / `skill install` / `knowledge-manage add` to extend."
                .to_string()
        } else {
            parts.join("; ")
        };
        (Status::Passed, detail)
    } else {
        let mut detail = warnings.join("; ");
        if !parts.is_empty() {
            detail.push_str(&format!(" (working: {})", parts.join("; ")));
        }
        (Status::Warning, detail)
    };

    CheckResult {
        name: "Ecosystem (MCP/Skill/Knowledge)".to_string(),
        status,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn binary_identity_always_passes() {
        let r = check_binary_identity();
        assert_eq!(r.status, Status::Passed);
        assert!(r.detail.contains(env!("CARGO_PKG_VERSION")));
        assert!(r.detail.contains("UMADEV_HOST_SPEC_V1"));
    }

    #[test]
    fn embedded_spec_check_passes() {
        let r = check_embedded_spec();
        assert_eq!(r.status, Status::Passed);
    }

    #[test]
    fn backend_guidance_names_a_real_command_path() {
        let detail = ready_backends_detail(&["codex", "kimi-code"]);
        assert!(detail.contains("Start `umadev`"));
        assert!(detail.contains("`umadev run \"<requirement>\" --backend codex`"));
        assert!(!detail.contains("Use --backend"));
    }

    #[test]
    fn workspace_writable_pass_in_tmp() {
        let tmp = TempDir::new().unwrap();
        let r = check_workspace_writable(tmp.path());
        assert_eq!(r.status, Status::Passed);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_writable_probe_never_follows_an_existing_link() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "keep").unwrap();
        symlink(
            &victim,
            tmp.path()
                .join(format!(".umadev-doctor-probe-{}", std::process::id())),
        )
        .unwrap();

        assert_eq!(check_workspace_writable(tmp.path()).status, Status::Failed);
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "keep");
    }

    #[tokio::test]
    async fn run_all_returns_all_checks_on_empty_workspace() {
        let tmp = TempDir::new().unwrap();
        let results = run_all(tmp.path(), false).await;
        let names = results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "binary identity",
                "embedded spec markdown",
                "workspace writable",
                umadev_i18n::tl("doctor.run_lock_fence_name"),
                umadev_i18n::tl("doctor.rewind_marker_name"),
                "spec manifest (UD-META-001)",
                "AI host backends",
                "Claude non-interactive auth",
                "git (file checkpoints)",
                "user config",
                "Claude Code hook",
                "Kimi Code hooks",
                "Kimi Code shell",
                "Deployment readiness",
                "Ecosystem (MCP/Skill/Knowledge)",
                NPM_CHECK,
                EMBED_MODEL_CHECK,
                NODE_CHECK,
            ]
        );
        // No FAILs on a clean workspace — only a manifest WARN.
        assert!(results.iter().all(|r| r.status != Status::Failed));
        // The "AI host backends" check warns iff no base CLI is on PATH, and the
        // auth / native-hook checks depend on the ambient selected backend and
        // credentials, so exclude them; likewise the embed-model and node checks
        // depend on the machine (a dev box without the model warns, an old node
        // warns). The only env-independent WARN asserted here is the missing
        // manifest.
        assert_eq!(
            results
                .iter()
                .filter(|r| r.name != "AI host backends")
                .filter(|r| r.name != "Claude non-interactive auth")
                .filter(|r| r.name != "Kimi Code hooks")
                .filter(|r| r.name != EMBED_MODEL_CHECK)
                .filter(|r| r.name != NODE_CHECK)
                .filter(|r| r.status == Status::Warning)
                .count(),
            1
        );
    }

    #[test]
    fn npm_tree_detection_separates_global_from_local() {
        // Global install: the shim exec's the platform sub-package binary under
        // <prefix>/lib/node_modules/… — a command IS linked onto PATH.
        let global = Path::new(
            "/home/dev/.npm-global/lib/node_modules/umadev/node_modules/@umatech/cli-linux-x64/bin/umadev",
        );
        assert!(under_node_modules(global));
        assert!(under_global_node_modules(global));

        // Local install (`npm i @umatech/umadev`): a project ./node_modules — npm links NO
        // command onto PATH, which is exactly the "it didn't install" confusion.
        let local = Path::new("/home/dev/proj/node_modules/@umatech/cli-linux-x64/bin/umadev");
        assert!(under_node_modules(local));
        assert!(!under_global_node_modules(local));

        // A source / release build is not npm-delivered at all.
        let source = Path::new("/home/dev/umadev/target/release/umadev");
        assert!(!under_node_modules(source));
        assert!(!under_global_node_modules(source));
    }

    #[test]
    fn npm_check_passes_for_a_source_build() {
        // The test binary itself is a cargo build (never under node_modules), so
        // the check must stay silent — the doctor never invents a problem.
        let r = check_npm_install();
        assert_eq!(r.status, Status::Passed);
        assert_eq!(r.name, NPM_CHECK);
        assert!(r.detail.contains("not an npm-delivered binary"));
    }

    #[cfg(unix)]
    #[test]
    fn first_root_owned_finds_nothing_in_a_user_owned_tree() {
        // A tmpdir we just created is owned by us, so the scan must come back
        // empty — a false "root-owned cache" WARN would be worse than no check.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("_cacache/content-v2")).unwrap();
        fs::write(tmp.path().join("_cacache/content-v2/blob"), b"x").unwrap();
        assert!(first_root_owned(tmp.path(), 4000).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn first_root_owned_is_bounded_by_the_cap() {
        // The cap keeps `umadev doctor` instant even against a huge npm cache:
        // with a cap of 1 the walk must give up immediately, not scan the tree.
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            fs::write(tmp.path().join(format!("f{i}")), b"x").unwrap();
        }
        assert!(first_root_owned(tmp.path(), 1).is_none());
    }

    #[tokio::test]
    async fn run_all_passes_clean_after_init() {
        let tmp = TempDir::new().unwrap();
        umadev_agent::SpecManifest::new("demo")
            .write_to(tmp.path(), false)
            .unwrap();
        let results = run_all(tmp.path(), false).await;
        // Everything passes except the env-dependent checks: the backend check
        // warns when no base CLI is installed (e.g. in CI), and the Claude
        // non-interactive auth check warns when claude-code is the ambient
        // configured backend with no token env — both are environment-dependent.
        let non_backend: Vec<_> = results
            .into_iter()
            .filter(|r| r.name != "AI host backends")
            .filter(|r| r.name != "Claude non-interactive auth")
            // The embed-model row warns on a machine that never fetched the model
            // (any CI/dev box without it), and the node row warns on an old/absent
            // node — both environment-dependent, like the backend checks above.
            .filter(|r| r.name != EMBED_MODEL_CHECK)
            .filter(|r| r.name != NODE_CHECK)
            .collect();
        assert!(all_passed(&non_backend));
    }

    #[tokio::test]
    async fn render_report_includes_counts() {
        let tmp = TempDir::new().unwrap();
        let results = run_all(tmp.path(), false).await;
        let report = render_report(tmp.path(), &results);
        assert!(report.contains("passed"));
        assert!(report.contains("failed"));
        assert!(report.contains("umadev doctor"));
    }

    #[tokio::test]
    async fn claude_hook_warns_when_settings_exist_without_hook() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        fs::write(tmp.path().join(".claude/settings.json"), r#"{"hooks":{}}"#).unwrap();
        let results = run_all(tmp.path(), false).await;
        let hook_check = results
            .iter()
            .find(|r| r.name == "Claude Code hook")
            .unwrap();
        assert_eq!(hook_check.status, Status::Warning);
        assert!(hook_check.detail.contains("umadev install"));
    }

    #[tokio::test]
    async fn claude_hook_passes_when_registered() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        fs::write(
            tmp.path().join(".claude/settings.local.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"command":"umadev","args":["hook","pre-write"]}]},{"matcher":"Bash","hooks":[{"command":"umadev","args":["hook","pre-bash"]}]}],"PostToolUse":[{"matcher":"Write|Bash","hooks":[{"command":"umadev","args":["hook","tool-audit"]}]}]}}"#,
        )
        .unwrap();
        let results = run_all(tmp.path(), false).await;
        let hook_check = results
            .iter()
            .find(|r| r.name == "Claude Code hook")
            .unwrap();
        // The hook IS registered. Whether the bare `umadev` command resolves
        // depends on the environment: a dev machine has it on PATH -> Passed; CI
        // does not -> a "registered but command does not resolve" Warning. Both are
        // correct outcomes; the only real bug is failing to RECOGNIZE the hook.
        assert!(
            hook_check.status == Status::Passed || hook_check.detail.contains("does not resolve"),
            "a registered hook must be recognized, got {:?}: {}",
            hook_check.status,
            hook_check.detail
        );
    }

    #[tokio::test]
    async fn claude_hook_warns_when_commands_are_registered_in_the_wrong_phase() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        fs::write(
            tmp.path().join(".claude/settings.local.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":"Write|Bash","hooks":[{"command":"umadev","args":["hook","pre-write"]},{"command":"umadev","args":["hook","pre-bash"]},{"command":"umadev","args":["hook","tool-audit"]}]}]}}"#,
        )
        .unwrap();
        let results = run_all(tmp.path(), false).await;
        let hook_check = results
            .iter()
            .find(|r| r.name == "Claude Code hook")
            .unwrap();
        assert_eq!(hook_check.status, Status::Warning);
        assert!(hook_check.detail.contains("PreToolUse/pre-write"));
        assert!(hook_check.detail.contains("PreToolUse/pre-bash"));
    }

    #[tokio::test]
    async fn backend_check_runs_without_panic() {
        let r = check_ai_backends().await;
        assert!(!r.name.is_empty());
        // On a dev machine with CLIs installed it should pass; on CI it may warn.
        assert!(r.status == Status::Passed || r.status == Status::Warning);
    }

    #[test]
    fn claude_noninteractive_auth_not_applicable_for_other_backends() {
        // codex / opencode / offline must NEVER warn about a Claude token — the
        // check is informational (PASS) and env-independent for them. (Reads no
        // env on this path, so it's safe to run in parallel with the env-mutating
        // test below.)
        for b in ["codex", "opencode", "offline"] {
            let r = check_claude_noninteractive_auth(Some(b));
            assert_eq!(r.status, Status::Passed, "backend {b} must not warn");
            assert!(r.detail.contains("not applicable"));
            assert!(r.detail.contains(b));
        }
        // No backend picked yet → also a not-applicable PASS, never a WARN.
        let none = check_claude_noninteractive_auth(None);
        assert_eq!(none.status, Status::Passed);
        assert!(none.detail.contains("not applicable"));
    }

    #[test]
    fn claude_noninteractive_auth_warns_without_token_passes_with_it() {
        // All env-mutating assertions live in ONE test (env is process-global) so
        // they can't race a sibling. Snapshot + restore every credential var.
        let saved: Vec<(&str, Option<String>)> = CLAUDE_NONINTERACTIVE_AUTH_ENV
            .iter()
            .map(|&k| (k, std::env::var(k).ok()))
            .collect();
        for &k in CLAUDE_NONINTERACTIVE_AUTH_ENV {
            std::env::remove_var(k);
        }

        // No headless credential → WARN that points at `claude setup-token` +
        // `CLAUDE_CODE_OAUTH_TOKEN`, and names the 401 the user actually hits.
        let warn = check_claude_noninteractive_auth(Some("claude-code"));
        assert_eq!(warn.status, Status::Warning);
        assert!(warn.detail.contains("claude setup-token"));
        assert!(warn.detail.contains("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(warn.detail.contains("401"));
        assert!(warn.detail.contains("non-interactive") || warn.detail.contains("NON-INTERACTIVE"));

        // The setup-token credential present → PASS naming the satisfying var.
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat-test");
        let pass = check_claude_noninteractive_auth(Some("claude-code"));
        assert_eq!(pass.status, Status::Passed);
        assert!(pass.detail.contains("CLAUDE_CODE_OAUTH_TOKEN"));
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");

        // An API key also satisfies headless auth (any credential clears it).
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test");
        let pass_key = check_claude_noninteractive_auth(Some("claude-code"));
        assert_eq!(pass_key.status, Status::Passed);
        assert!(pass_key.detail.contains("ANTHROPIC_API_KEY"));
        std::env::remove_var("ANTHROPIC_API_KEY");

        // A blank token is treated as unset (fail-open: no false PASS).
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "   ");
        let blank = check_claude_noninteractive_auth(Some("claude-code"));
        assert_eq!(blank.status, Status::Warning, "blank token must not pass");

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn embed_model_row_reflects_presence() {
        // Present → PASS + the "hybrid" wording; absent → WARN with the fix recipe
        // (npm / ~/.umadev/embed-model / UMADEV_EMBED_MODEL_DIR) and NEVER a FAIL.
        let present = embed_model_row(true);
        assert_eq!(present.status, Status::Passed);
        assert_eq!(present.name, EMBED_MODEL_CHECK);
        assert!(present.detail.contains("hybrid"));

        let absent = embed_model_row(false);
        assert_eq!(absent.status, Status::Warning);
        assert!(absent.detail.contains("BM25"));
        assert!(absent.detail.contains("umadev run"));
        assert!(!absent.detail.contains("run `umadev update`"));
        assert!(absent.detail.contains("does not install this model"));
        assert!(absent.detail.contains("~/.umadev/embed-model"));
        assert!(absent.detail.contains("UMADEV_EMBED_MODEL_DIR"));
    }

    #[test]
    fn embed_model_present_matches_the_runtime_structural_gate() {
        assert!(!embed_model_present(None), "no dir → absent");
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        assert!(!embed_model_present(Some(dir)), "empty dir → absent");

        fs::write(dir.join("config.json"), r#"{"hidden_size":384}"#).unwrap();
        fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        // A too-small safetensors (under the size floor) must still count as absent.
        fs::write(dir.join("model.safetensors"), vec![0u8; 1024]).unwrap();
        assert!(
            !embed_model_present(Some(dir)),
            "safetensors under the size floor → absent"
        );

        // Size alone is not enough: an empty safetensors header is corrupt.
        let weights = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(dir.join("model.safetensors"))
            .unwrap();
        weights
            .set_len(EMBED_MODEL_MIN_SAFETENSORS_BYTES + 16)
            .unwrap();
        assert!(!embed_model_present(Some(dir)), "zero header → absent");

        fs::write(
            dir.join("model.safetensors"),
            [
                2_u64.to_le_bytes().as_slice(),
                b"{}",
                &vec![0_u8; usize::try_from(EMBED_MODEL_MIN_SAFETENSORS_BYTES).unwrap()],
            ]
            .concat(),
        )
        .unwrap();
        assert!(embed_model_present(Some(dir)), "structurally valid model");

        fs::write(dir.join("config.json"), "not json").unwrap();
        assert!(!embed_model_present(Some(dir)), "invalid config → absent");
    }

    #[cfg(unix)]
    #[test]
    fn embed_model_present_rejects_linked_model_files() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(tmp.path().join("config.json"), "{}").unwrap();
        fs::write(tmp.path().join("tokenizer.json"), "{}").unwrap();
        let outside_weights = outside.path().join("model.safetensors");
        let file = fs::File::create(&outside_weights).unwrap();
        file.set_len(EMBED_MODEL_MIN_SAFETENSORS_BYTES + 16)
            .unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&outside_weights)
            .unwrap();
        file.write_all(&2_u64.to_le_bytes()).unwrap();
        symlink(&outside_weights, tmp.path().join("model.safetensors")).unwrap();

        assert!(
            !embed_model_present(Some(tmp.path())),
            "the doctor must not pass a model the runtime refuses"
        );
    }

    #[test]
    fn node_version_row_gates_on_the_floor() {
        // >= floor → PASS.
        let ok = node_version_row(Some("v18.17.0\n"));
        assert_eq!(ok.status, Status::Passed);
        assert_eq!(ok.name, NODE_CHECK);
        assert_eq!(node_version_row(Some("v20.1.2")).status, Status::Passed);

        // Below the floor → WARN with an upgrade nudge.
        let old = node_version_row(Some("v16.20.0"));
        assert_eq!(old.status, Status::Warning);
        assert!(old.detail.contains("below the supported floor"));

        // Unparseable version → WARN.
        assert_eq!(node_version_row(Some("garbage")).status, Status::Warning);

        // Node absent → informational PASS (only npm-managed installs need it).
        let none = node_version_row(None);
        assert_eq!(none.status, Status::Passed);
        assert!(none.detail.contains("not on PATH"));

        let unsafe_probe = node_probe_warning("version probe timed out");
        assert_eq!(unsafe_probe.status, Status::Warning);
        assert!(unsafe_probe.detail.contains("timed out"));
    }

    #[test]
    fn parse_node_major_reads_the_leading_integer() {
        assert_eq!(parse_node_major("v18.17.0\n"), Some(18));
        assert_eq!(parse_node_major("20.1.2"), Some(20));
        assert_eq!(parse_node_major("v8.9.4"), Some(8));
        assert_eq!(parse_node_major(""), None);
        assert_eq!(parse_node_major("not-a-version"), None);
    }

    #[test]
    fn which_on_path_finds_known_commands() {
        // 'ls' / 'cmd' should be on PATH on any unix/windows dev machine.
        assert!(which_on_path("ls") || which_on_path("cmd"));
    }

    #[test]
    fn delivery_readiness_passes_when_no_run_yet() {
        let tmp = tempfile::TempDir::new().unwrap();
        let r = check_delivery_readiness(tmp.path());
        assert_eq!(r.status, Status::Passed);
        assert!(r.detail.contains("no run yet") || r.detail.contains("not reached"));
    }

    #[test]
    fn delivery_readiness_warns_when_no_deploy_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("output");
        std::fs::create_dir_all(&out).unwrap();
        // delivery notes present but only the placeholder (no real command).
        std::fs::write(
            out.join("demo-delivery-notes.md"),
            "## Deploy command\n\n_(exact command — read by UmaDev)_\n",
        )
        .unwrap();
        let r = check_delivery_readiness(tmp.path());
        // Placeholder-only → warning (missing concrete command).
        assert!(r.status == Status::Warning || r.status == Status::Passed);
        assert!(r.name.contains("Deployment"));
    }

    #[test]
    fn delivery_readiness_detects_concrete_deploy_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("output");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            out.join("demo-delivery-notes.md"),
            "## Deploy command\n\nnpx vercel --prod\n",
        )
        .unwrap();
        let r = check_delivery_readiness(tmp.path());
        assert!(r.detail.contains("deploy command"));
    }

    #[test]
    fn ecosystem_empty_workspace_passes() {
        let tmp = TempDir::new().unwrap();
        let r = check_ecosystem(tmp.path());
        assert_eq!(r.status, Status::Passed);
        assert!(r.detail.contains("no extensions"));
    }

    #[test]
    fn ecosystem_corrupt_mcp_json_warns_not_ok() {
        // A malformed .mcp.json must WARN (the host loads zero servers), not be
        // folded into a cheerful "0 servers / no extensions".
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".mcp.json"), "{not valid json,,,").unwrap();
        let r = check_ecosystem(tmp.path());
        assert_eq!(r.status, Status::Warning, "corrupt .mcp.json must warn");
        assert!(r.detail.contains(".mcp.json"));
        assert!(!r.detail.contains("no extensions configured"));
    }

    #[test]
    fn ecosystem_valid_mcp_json_counts_servers() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            r#"{"mcpServers":{"gh":{"command":"npx"}}}"#,
        )
        .unwrap();
        let r = check_ecosystem(tmp.path());
        assert_eq!(r.status, Status::Passed);
        assert!(r.detail.contains("1 MCP server"));
    }

    #[test]
    fn ecosystem_corrupt_knowledge_json_warns() {
        let tmp = TempDir::new().unwrap();
        let udir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&udir).unwrap();
        std::fs::write(udir.join("knowledge.json"), "}{ broken").unwrap();
        let r = check_ecosystem(tmp.path());
        assert_eq!(r.status, Status::Warning);
        assert!(r.detail.contains("knowledge.json"));
    }

    fn write_claude_settings(tmp: &Path, command: &str) {
        let dir = tmp.join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        let program = command
            .strip_suffix("hook pre-write")
            .map_or(command, str::trim_end);
        let json = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Write|Edit|MultiEdit", "hooks": [{"type": "command", "command": program, "args": ["hook", "pre-write"]}]},
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": program, "args": ["hook", "pre-bash"]}]}
                ],
                "PostToolUse": [{
                    "matcher": "Write|Edit|MultiEdit|Bash",
                    "hooks": [{"type": "command", "command": program, "args": ["hook", "tool-audit"]}]
                }]
            }
        });
        std::fs::write(dir.join("settings.local.json"), json.to_string()).unwrap();
    }

    #[test]
    fn claude_hook_passes_only_for_a_live_resolving_umadev_command() {
        let tmp = TempDir::new().unwrap();
        // Point the registered hook at a real on-disk "binary" so it resolves.
        let bin = tmp.path().join("umadev");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let cmd = format!("{} hook pre-write", bin.display());
        write_claude_settings(tmp.path(), &cmd);
        let r = check_claude_hook(tmp.path());
        assert_eq!(r.status, Status::Passed, "{}", r.detail);
    }

    #[test]
    fn claude_hook_warns_when_registered_command_points_at_dead_binary() {
        let tmp = TempDir::new().unwrap();
        // A precisely-ours command, but the binary path does not exist.
        write_claude_settings(tmp.path(), "/nonexistent/path/umadev hook pre-write");
        let r = check_claude_hook(tmp.path());
        assert_eq!(r.status, Status::Warning);
        assert!(r.detail.contains("does not resolve"), "{}", r.detail);
    }

    #[test]
    fn claude_hook_does_not_false_pass_on_substring_or_foreign_wrapper() {
        // A substring match would PASS on a user's unrelated wrapper; the JSON
        // parse + precise program match must NOT.
        let tmp = TempDir::new().unwrap();
        write_claude_settings(tmp.path(), "/usr/bin/my-wrapper hook pre-write");
        let r = check_claude_hook(tmp.path());
        assert_eq!(r.status, Status::Warning);
        assert!(r.detail.contains("not registered"), "{}", r.detail);
    }

    #[test]
    fn claude_hook_warns_on_invalid_json_instead_of_false_pass() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        // Substring "hook pre-write" present, but the file is not valid JSON.
        std::fs::write(
            dir.join("settings.local.json"),
            "{ not json: hook pre-write ",
        )
        .unwrap();
        let r = check_claude_hook(tmp.path());
        assert_eq!(r.status, Status::Warning);
        assert!(r.detail.contains("invalid JSON"), "{}", r.detail);
    }

    #[test]
    fn claude_hook_warns_when_only_legacy_shared_hook_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"command":"umadev hook pre-write"}]}]}}"#,
        )
        .unwrap();
        let r = check_claude_hook(tmp.path());
        assert_eq!(r.status, Status::Warning);
        assert!(r.detail.contains("legacy"), "{}", r.detail);
        assert!(r.detail.contains("settings.local.json"), "{}", r.detail);
    }

    #[test]
    fn the_rewind_marker_check_detects_the_permanent_halt_and_fix_clears_it() {
        // MED-4. `umadev doctor` did not even LOOK for the one condition that permanently
        // wedges the product — a stale marker whose snapshot the workspace can no longer
        // identify, which aborts every `umadev run` on every start with no verb to clear it.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // A clean workspace: nothing to say.
        let clean = check_workspace_rewind_marker(root, false);
        assert_eq!(clean.status, Status::Passed);

        // A marker naming a head this workspace cannot identify (no shadow repo at all —
        // the user deleted `.umadev/checkpoints.git` to reclaim disk).
        let marker = root.join(".umadev").join("temp-rewind.json");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(
            &marker,
            r#"{"head":"deadbee","to":"cafe123","pid":0,"started_at":1}"#,
        )
        .unwrap();

        // Without `--fix` it is a FAILURE that names the repair — and changes nothing.
        let broken = check_workspace_rewind_marker(root, false);
        assert_eq!(broken.status, Status::Failed);
        assert!(broken.detail.contains("deadbee"), "{}", broken.detail);
        assert!(marker.exists(), "a report must not mutate anything");

        // With `--fix` the lockout is lifted.
        let fixed = check_workspace_rewind_marker(root, true);
        assert_eq!(fixed.status, Status::Warning);
        assert!(!marker.exists(), "the stale marker is gone");
        // …and it is idempotent.
        assert_eq!(
            check_workspace_rewind_marker(root, true).status,
            Status::Passed
        );
    }

    #[test]
    fn doctor_surfaces_and_removes_a_damaged_rewind_marker() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join(".umadev/temp-rewind.json");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "{{ damaged").unwrap();

        let report = check_workspace_rewind_marker(tmp.path(), false);
        assert_eq!(report.status, Status::Failed);
        assert!(
            report.detail.contains("damaged")
                || report.detail.contains("损坏")
                || report.detail.contains("損壞")
        );
        assert!(marker.exists());

        let fixed = check_workspace_rewind_marker(tmp.path(), true);
        assert_eq!(fixed.status, Status::Warning);
        assert!(!marker.exists());
    }

    #[test]
    fn run_lock_fence_check_requires_explicit_offline_fix() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let dir = root.join(".umadev");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("run.lock"),
            format!(
                "pid=4000000 host={} ts=1\n",
                std::env::var("HOSTNAME").unwrap_or_default()
            ),
        )
        .unwrap();

        let report_only = check_run_lock_fence(root, false);
        assert_eq!(report_only.status, Status::Failed);
        assert!(
            report_only.detail.contains("doctor --fix"),
            "{}",
            report_only.detail
        );
        assert_ne!(
            std::fs::read(dir.join("run.lock")).unwrap(),
            b"pid=0 host=__umadev_v2__ ts=18446744073709551615 boot=__v2__ protocol=2\n"
        );

        let fixed = check_run_lock_fence(root, true);
        assert_eq!(fixed.status, Status::Warning, "{}", fixed.detail);
        assert_eq!(
            check_run_lock_fence(root, false).status,
            Status::Passed,
            "the exact v2 fence is accepted after migration"
        );
    }
}
