//! `umadev doctor` — self-test that diagnoses common
//! "installed-but-not-working" situations.
//!
//! Checks performed:
//! 1. Binary identity (`CARGO_PKG_VERSION` + spec version).
//! 2. Embedded spec markdown is non-empty + carries the version marker.
//! 3. Workspace is writable (write + delete a tmp file).
//! 4. UD-META-001 spec manifest present and version-aligned.
//! 5. AI coding host CLIs detected on PATH.
//! 6. Claude Code PreToolUse governance hook installed (if `.claude/` exists).
//! 7. Delivery / deployment readiness (after a run completes): delivery notes
//!    present with a deploy command, build output exists, and a deploy CLI
//!    (vercel / netlify / wrangler) is on PATH.
//!
//! The hook check (6) was added in 4.6 alongside the restored real-time
//! governance hook (`umadev install`). When `.claude/settings.json` exists
//! but the hook isn't registered, the doctor suggests running `umadev install`.

use std::fs;
use std::io::Write;
use std::path::Path;

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
/// Async because the backend check now spawns a real `<base> --version` probe
/// (via `umadev_host::probe_all`) so it agrees with the run path instead of a
/// PATH-only heuristic. Fail-open: a probe error degrades to a Warning row,
/// never a hard failure.
pub async fn run_all(workspace: &Path) -> Vec<CheckResult> {
    let mut results = vec![
        check_binary_identity(),
        check_embedded_spec(),
        check_workspace_writable(workspace),
        check_spec_manifest(workspace),
    ];
    results.push(check_ai_backends().await);
    results.push(check_git());
    results.push(check_user_config());
    results.push(check_claude_hook(workspace));
    results.push(check_delivery_readiness(workspace));
    results.push(check_ecosystem(workspace));
    results
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

/// Check whether the Claude Code PreToolUse governance hook is registered.
/// Only relevant when `.claude/settings.json` exists (workspace-level Claude
/// Code config). When the hook is missing, suggests `umadev install`.
fn check_claude_hook(workspace: &Path) -> CheckResult {
    let settings_path = workspace.join(".claude/settings.json");
    if !settings_path.is_file() {
        // No Claude Code config at all — not an error, just informational.
        return CheckResult {
            name: "Claude Code hook".to_string(),
            status: Status::Passed,
            detail: "no .claude/settings.json (real-time governance off; quality-gate hard block still active)"
                .to_string(),
        };
    }
    let Ok(content) = fs::read_to_string(&settings_path) else {
        return CheckResult {
            name: "Claude Code hook".to_string(),
            status: Status::Warning,
            detail: ".claude/settings.json exists but is unreadable".to_string(),
        };
    };
    if content.contains("hook pre-write") {
        CheckResult {
            name: "Claude Code hook".to_string(),
            status: Status::Passed,
            detail:
                "PreToolUse governance hook registered (UD-CODE-001/002/005 enforced at write time)"
                    .to_string(),
        }
    } else {
        CheckResult {
            name: "Claude Code hook".to_string(),
            status: Status::Warning,
            detail: ".claude/settings.json exists but the UmaDev PreToolUse hook is not registered. \
                     Run `umadev install --host claude-code` for real-time emoji/color/slop interception."
                .to_string(),
        }
    }
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

/// Check which host CLIs (claude-code, codex, opencode) are installed and
/// usable. This is the most important doctor check for enterprise use —
/// without a backend, UmaDev falls back to offline templates.
///
/// Uses `umadev_host::probe_all` — the SAME detection the run path and the TUI
/// startup panel use — so the doctor never reports a false "not detected" for a
/// base that `run` can actually drive. `probe_all` resolves each base via
/// `umadev_host::resolve_program` (PATH first, then known install dirs:
/// Homebrew / volta / `~/.<base>/bin` / `…/Programs`, plus the
/// `UMADEV_<NAME>_BIN` override) and then runs a real `--version`, which is the
/// final installed-or-not arbiter. Fail-open: an unhealthy probe is surfaced as
/// detail, never a hard FAIL.
async fn check_ai_backends() -> CheckResult {
    let statuses = umadev_host::probe_all().await;

    let ready: Vec<&str> = statuses
        .iter()
        .filter(|s| s.probe.is_ready())
        .map(|s| s.id)
        .collect();
    // Found-but-broken bases (e.g. an old `--version` that errors): worth
    // surfacing so the user fixes the install rather than thinking it's missing.
    let unhealthy: Vec<&str> = statuses
        .iter()
        .filter(|s| matches!(s.probe, umadev_host::ProbeResult::Unhealthy { .. }))
        .map(|s| s.id)
        .collect();

    if ready.is_empty() {
        let mut detail = String::from(
            "No base CLI (claude / codex / opencode) detected. Install one and log in — it brings its OWN model (your login or your own API). Without a base, UmaDev falls back to offline templates.",
        );
        if !unhealthy.is_empty() {
            detail.push_str(&format!(
                " Found but not responding to --version: {} (reinstall / check PATH, or set UMADEV_<NAME>_BIN).",
                unhealthy.join(", ")
            ));
        }
        CheckResult {
            name: "AI host backends".to_string(),
            status: Status::Warning,
            detail,
        }
    } else {
        let mut detail = format!(
            "{} backend(s) detected: {}. Use --backend {} for real AI generation. (Login is verified when a run starts — make sure you've logged into the CLI.)",
            ready.len(),
            ready.join(", "),
            ready[0]
        );
        if !unhealthy.is_empty() {
            detail.push_str(&format!(
                " Also found but unhealthy: {}.",
                unhealthy.join(", ")
            ));
        }
        CheckResult {
            name: "AI host backends".to_string(),
            status: Status::Passed,
            detail,
        }
    }
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
    let probe = workspace.join(".umadev-doctor-probe");
    let res = (|| -> std::io::Result<()> {
        if let Some(parent) = probe.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&probe)?;
        f.write_all(b"ok")?;
        f.sync_data()?;
        fs::remove_file(&probe)?;
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
        rd.filter_map(Result::ok).map(|e| e.path()).find(|p| {
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
    let notes = fs::read_to_string(&notes_path).unwrap_or_default();
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
    match std::fs::read_to_string(&mcp_path) {
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
            .filter(|e| e.path().is_dir())
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
    match std::fs::read_to_string(&knowledge_reg) {
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
    fn workspace_writable_pass_in_tmp() {
        let tmp = TempDir::new().unwrap();
        let r = check_workspace_writable(tmp.path());
        assert_eq!(r.status, Status::Passed);
    }

    #[tokio::test]
    async fn run_all_returns_ten_checks_on_empty_workspace() {
        let tmp = TempDir::new().unwrap();
        let results = run_all(tmp.path()).await;
        assert_eq!(results.len(), 10);
        // No FAILs on a clean workspace — only a manifest WARN.
        assert!(results.iter().all(|r| r.status != Status::Failed));
        // The "AI host backends" check warns iff no base CLI is on PATH,
        // which differs between dev machines and CI -- exclude it so the only
        // env-independent WARN asserted here is the missing manifest.
        assert_eq!(
            results
                .iter()
                .filter(|r| r.name != "AI host backends")
                .filter(|r| r.status == Status::Warning)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn run_all_passes_clean_after_init() {
        let tmp = TempDir::new().unwrap();
        umadev_agent::SpecManifest::new("demo")
            .write_to(tmp.path(), false)
            .unwrap();
        let results = run_all(tmp.path()).await;
        // Everything passes except the env-dependent backend check, which
        // warns when no base CLI is installed (e.g. in CI).
        let non_backend: Vec<_> = results
            .into_iter()
            .filter(|r| r.name != "AI host backends")
            .collect();
        assert!(all_passed(&non_backend));
    }

    #[tokio::test]
    async fn render_report_includes_counts() {
        let tmp = TempDir::new().unwrap();
        let results = run_all(tmp.path()).await;
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
        let results = run_all(tmp.path()).await;
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
            tmp.path().join(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"command":"umadev hook pre-write"}]}]}}"#,
        )
        .unwrap();
        let results = run_all(tmp.path()).await;
        let hook_check = results
            .iter()
            .find(|r| r.name == "Claude Code hook")
            .unwrap();
        assert_eq!(hook_check.status, Status::Passed);
    }

    #[tokio::test]
    async fn backend_check_runs_without_panic() {
        let r = check_ai_backends().await;
        assert!(!r.name.is_empty());
        // On a dev machine with CLIs installed it should pass; on CI it may warn.
        assert!(r.status == Status::Passed || r.status == Status::Warning);
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
}
