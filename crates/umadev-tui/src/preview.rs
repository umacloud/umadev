use std::sync::Arc;

use umadev_agent::{ChannelSink, EngineEvent, EventSink};

/// Keep the Unix process-group leader alive for as long as the stored preview
/// handle exists. The actual dev command runs as its child; if an npm/pnpm
/// wrapper exits after launching node/vite, this owner remains an unreaped,
/// unambiguous PGID anchor until `ManagedChild` tears the group down.
#[cfg(unix)]
const PREVIEW_OWNER_SCRIPT: &str =
    "\"$@\" & child=$!; wait \"$child\"; while :; do sleep 3600; done";

/// Split a worker-recorded run command like `cd web && npm run dev` into
/// (`working_dir`, `program`, `args`), ready to feed a raw
/// `tokio::process::Command::new(program).args(args)`.
///
/// Windows-aware (mirrors `deploy.rs` / `verify.rs` / `runtime_proof.rs`): the
/// program is resolved through [`umadev_host::spawn_parts`], so a Windows
/// npm/pnpm `.cmd` shim is found on `PATH` and Rust's hardened batch-argument
/// encoder handles its argv. A command without a `cd X &&` prefix runs in the
/// workspace root, through `sh -c` on Unix so a recorded command keeps its shell
/// syntax. Windows never uses `cmd /c`: `cmd.exe` would look the program up in
/// the workspace before `PATH`, so a static-HTML repository could ship a
/// `python3.bat` that the automatic post-build preview then ran on the host.
pub(super) fn parse_run_command(
    command: &str,
    project_root: &std::path::Path,
) -> (std::path::PathBuf, String, Vec<String>) {
    // Strip a leading `cd <dir> &&` and resolve it relative to the workspace.
    if let Some(after_cd) = command.trim().strip_prefix("cd ") {
        if let Some((dir, rest)) = after_cd.split_once("&&") {
            let dir = dir.trim().trim_matches(|c| c == '\'' || c == '"');
            let resolved = if std::path::Path::new(dir).is_absolute() {
                std::path::PathBuf::from(dir)
            } else {
                project_root.join(dir)
            };
            if let Some((program, args)) = direct_program(rest) {
                return (resolved, program, args);
            }
        }
    }
    if cfg!(windows) {
        if let Some((program, args)) = direct_program(command) {
            return (project_root.to_path_buf(), program, args);
        }
    }
    (
        project_root.to_path_buf(),
        "sh".to_string(),
        vec!["-c".to_string(), command.to_string()],
    )
}

/// Split `words` on whitespace and resolve the first word through
/// [`umadev_host::spawn_parts`], appending the rest after whatever lead it
/// produced. `None` for an empty command.
fn direct_program(words: &str) -> Option<(String, Vec<String>)> {
    let mut words = words.split_whitespace();
    let (program, mut args) = umadev_host::spawn_parts(words.next()?);
    args.extend(words.map(str::to_string));
    Some((program, args))
}

/// Extract the host:port from a `http://host:port/...` URL, returning None
/// when parsing fails. Used by [`wait_for_port`] so we only open the browser
/// after the dev server is actually accepting connections — not 0ms after
/// spawn, when Vite is still compiling and the page would 404.
pub(super) fn url_host_port(url: &str) -> Option<String> {
    let after_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let host_port = after_scheme.split('/').next()?;
    Some(host_port.to_string())
}

/// Poll a `host:port` with a TCP connect until it succeeds or `timeout`
/// elapses. Returns Ok(()) when the dev server is reachable. Mirrors what a
/// browser does — so opening the URL after this returns won't hit a 404 from
/// a half-started server. Runs in the async task so it never blocks the TUI.
pub(super) async fn wait_for_port(url: &str, timeout: std::time::Duration) -> bool {
    let Some(addr) = url_host_port(url) else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Check whether the port in `url` is currently FREE (nothing listening). We
/// bind to it briefly — if binding fails the port is occupied (by the user's
/// other Vite/Node service), so spawning our dev server would either fail or
/// silently bind a different port while we open the wrong URL. Returning
/// false here tells the caller to NOT spawn and instead hint to the user.
pub(super) fn port_is_free(url: &str) -> bool {
    let Some(addr) = url_host_port(url) else {
        return false; // can't parse → assume not free (conservative)
    };
    std::net::TcpListener::bind(&addr).is_ok()
}

/// Cross-platform best-effort browser open (sync variant for the event loop).
pub(super) fn open_url(url: &str) -> std::io::Result<()> {
    if !crate::link::is_safe_url(url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preview target must be a safe http(s) URL",
        ));
    }
    // One opener implementation for transcript links and previews. In
    // particular Windows uses `explorer <url>` as a literal argv value, never
    // `cmd /C start`, so `&` in a URL cannot become a command separator. The
    // shared helper also nulls stdio and reaps the short-lived launcher.
    crate::link::spawn_opener(url)
}

/// Start a preview dev server in the background and optionally open its URL once
/// the port is up. Shared by the manual `/preview` ([`Action::StartPreview`]) path
/// and the automatic post-build preview, so both behave identically: the
/// port-conflict guard, the background `wait_for_port` + browser-open, and the
/// `preview_server` child handle (parked for exit-cleanup) are defined exactly
/// once here.
///
/// **Fail-open / non-blocking by contract**: spawning the dev server is
/// best-effort and never blocks the TUI — `wait_for_port` runs in a detached
/// task, a spawn failure only emits a hint, and a busy port opens what is
/// already running instead of starting a second server. The child is stored in
/// `preview_server` so the run-exit cleanup (`run()`) kills it and no process
/// leaks. `open_browser` controls whether the URL is auto-opened in a browser
/// (the manual `/preview` opens it; the automatic post-build preview does NOT —
/// it only surfaces the clickable URL so the build flow never steals focus).
pub(super) fn start_preview_server(
    preview_server: &std::sync::Arc<std::sync::Mutex<Option<umadev_process::ManagedChild>>>,
    sink: &Arc<ChannelSink>,
    url: &str,
    command: &str,
    project_root: &std::path::Path,
    open_browser: bool,
) {
    let (dir, prog, args) = parse_run_command(command, project_root);
    #[cfg(unix)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(PREVIEW_OWNER_SCRIPT)
            .arg("umadev-preview-owner")
            .arg(prog)
            .args(args);
        cmd
    };
    #[cfg(not(unix))]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args);
        cmd
    };
    cmd.current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Port-conflict guard: if the port is already bound (the user's own
    // Vite/Next/Express), DON'T spawn a second server — it would either fail or
    // bind a different port while we open the wrong URL. Open / surface what's
    // already running instead.
    if port_is_free(url) {
        match umadev_process::ManagedChild::spawn_detached(cmd) {
            Ok(child) => {
                if let Ok(mut g) = preview_server.lock() {
                    // Dropping the previous managed handle synchronously kills
                    // its whole group/job and schedules a bounded direct-child
                    // reap before this slot starts owning the replacement.
                    drop(g.take());
                    *g = Some(child);
                }
                sink.emit(EngineEvent::Note(
                    umadev_i18n::tl("preview.dev_starting").into(),
                ));
                let url2 = url.to_string();
                tokio::spawn(async move {
                    let up = wait_for_port(&url2, std::time::Duration::from_secs(15)).await;
                    if up && open_browser {
                        let _ = open_url(&url2);
                    }
                    // Do not append a readiness Note from this detached task. It
                    // can finish after `/clear` or `/resume` and would then write
                    // old-preview output into the replacement conversation. The
                    // synchronous starting note and completion-card URL already
                    // make the preview discoverable without crossing that boundary.
                });
            }
            Err(e) => {
                sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "preview.dev_spawn_failed",
                    &[command, &e.to_string(), url],
                )));
            }
        }
    } else {
        if open_browser {
            let _ = open_url(url);
        }
        sink.emit(EngineEvent::Note(umadev_i18n::tlf(
            "preview.port_busy",
            &[url],
        )));
    }
}
