//! Native clipboard routing and line-ending normalization.
//!
//! The transcript selection layer decides *what* to copy; this module owns the
//! platform boundary for local native commands and the remote/tmux routing
//! signals. Clipboard helpers are result-aware and share a hard deadline, so a
//! broken desktop integration cannot freeze the TUI indefinitely.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardFeedbackKind {
    Copied(usize),
    Failed,
    TooLarge,
}

#[derive(Debug, Clone, Copy)]
struct ClipboardFeedback {
    generation: u64,
    kind: ClipboardFeedbackKind,
    expires_at: Instant,
}

static CLIPBOARD_GENERATION: AtomicU64 = AtomicU64::new(0);
static CLIPBOARD_FEEDBACK_DIRTY: AtomicBool = AtomicBool::new(false);
static CLIPBOARD_FEEDBACK: OnceLock<Mutex<Option<ClipboardFeedback>>> = OnceLock::new();
static CLIPBOARD_WORKERS: OnceLock<Mutex<Vec<std::thread::JoinHandle<()>>>> = OnceLock::new();
#[cfg(unix)]
static SELECTION_OWNERS: OnceLock<Mutex<Vec<SelectionOwner>>> = OnceLock::new();
const MAX_CLIPBOARD_WORKERS: usize = 16;
#[cfg(unix)]
const MAX_SELECTION_OWNERS: usize = 8;

#[cfg(unix)]
struct SelectionOwner {
    child: std::process::Child,
    tree: umadev_process::StdCommandTree,
}

fn clipboard_feedback_slot() -> &'static Mutex<Option<ClipboardFeedback>> {
    CLIPBOARD_FEEDBACK.get_or_init(|| Mutex::new(None))
}

fn clipboard_workers() -> &'static Mutex<Vec<std::thread::JoinHandle<()>>> {
    CLIPBOARD_WORKERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn reap_finished_clipboard_workers() {
    if let Ok(mut workers) = clipboard_workers().lock() {
        let mut pending = Vec::with_capacity(workers.len());
        for worker in workers.drain(..) {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                pending.push(worker);
            }
        }
        *workers = pending;
    }
}

#[cfg(unix)]
fn selection_owners() -> &'static Mutex<Vec<SelectionOwner>> {
    SELECTION_OWNERS.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(unix)]
fn reap_finished_selection_owners() {
    if let Ok(mut owners) = selection_owners().lock() {
        owners.retain_mut(|owner| !matches!(owner.child.try_wait(), Ok(Some(_)) | Err(_)));
    }
}

fn publish_clipboard_feedback(generation: u64, kind: ClipboardFeedbackKind) {
    // A slower older helper must never overwrite the result of a newer copy.
    if CLIPBOARD_GENERATION.load(Ordering::Acquire) != generation {
        return;
    }
    if let Ok(mut slot) = clipboard_feedback_slot().lock() {
        *slot = Some(ClipboardFeedback {
            generation,
            kind,
            expires_at: Instant::now() + crate::selection::COPY_TOAST_TTL,
        });
        CLIPBOARD_FEEDBACK_DIRTY.store(true, Ordering::Release);
    }
}

/// Event-loop wake flag for a worker completion or feedback expiry. The 80ms
/// timer polls this; a completion causes one repaint and the TTL edge causes one
/// more to remove the status, without keeping an idle TUI on a redraw loop.
pub(crate) fn take_clipboard_feedback_redraw() -> bool {
    reap_finished_clipboard_workers();
    #[cfg(unix)]
    reap_finished_selection_owners();
    if CLIPBOARD_FEEDBACK_DIRTY.swap(false, Ordering::AcqRel) {
        return true;
    }
    let Ok(mut slot) = clipboard_feedback_slot().lock() else {
        return false;
    };
    if slot
        .as_ref()
        .is_some_and(|feedback| Instant::now() >= feedback.expires_at)
    {
        *slot = None;
        return true;
    }
    false
}

/// Read the completed asynchronous clipboard result for the status area. The
/// expiry is lazy so no event-loop mutation or transcript message is needed.
pub(crate) fn clipboard_feedback_text(lang: umadev_i18n::Lang) -> Option<(String, bool)> {
    let mut slot = clipboard_feedback_slot().lock().ok()?;
    let feedback = *slot.as_ref()?;
    if Instant::now() >= feedback.expires_at
        || CLIPBOARD_GENERATION.load(Ordering::Acquire) != feedback.generation
    {
        *slot = None;
        return None;
    }
    Some(match feedback.kind {
        ClipboardFeedbackKind::Copied(count) => (
            umadev_i18n::tf(lang, "tui.copied", &[&count.to_string()]),
            true,
        ),
        ClipboardFeedbackKind::Failed => {
            (umadev_i18n::t(lang, "tui.copy_failed").to_string(), false)
        }
        ClipboardFeedbackKind::TooLarge => (
            umadev_i18n::tf(
                lang,
                "tui.copy_too_large",
                &[&crate::selection::OSC52_MAX_TEXT_BYTES.to_string()],
            ),
            false,
        ),
    })
}

pub(crate) fn clipboard_feedback_active() -> bool {
    let Ok(mut slot) = clipboard_feedback_slot().lock() else {
        return false;
    };
    let Some(feedback) = slot.as_ref().copied() else {
        return false;
    };
    if Instant::now() >= feedback.expires_at
        || CLIPBOARD_GENERATION.load(Ordering::Acquire) != feedback.generation
    {
        *slot = None;
        return false;
    }
    true
}

/// Whether this is a remote session where a native clipboard command would
/// target the far host instead of the user's terminal.
pub(crate) fn clipboard_is_remote() -> bool {
    clipboard_remote_from_env(
        std::env::var_os("SSH_CONNECTION").is_some(),
        std::env::var_os("SSH_TTY").is_some(),
    )
}

pub(crate) fn clipboard_remote_from_env(ssh_connection: bool, _ssh_tty: bool) -> bool {
    // `SSH_CONNECTION` is authoritative. A locally reattached tmux pane may
    // retain stale `SSH_TTY`, which must not disable the native clipboard.
    ssh_connection
}

/// Whether OSC 52 needs tmux DCS passthrough to reach the outer terminal.
pub(crate) fn clipboard_in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

fn clipboard_is_wsl() -> bool {
    let env_hint =
        std::env::var_os("WSL_INTEROP").is_some() || std::env::var_os("WSL_DISTRO_NAME").is_some();
    if env_hint {
        return true;
    }
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    clipboard_wsl_from_signals(false, false, &release)
}

fn clipboard_wsl_from_signals(interop: bool, distro: bool, kernel_release: &str) -> bool {
    interop || distro || kernel_release.to_ascii_lowercase().contains("microsoft")
}

/// Copy `text` through the native OS path. The call returns only after the value
/// is committed (so an immediate paste sees it) or after one shared 500 ms
/// helper deadline; pipe writes and process reaping never run on the TUI thread.
pub(crate) fn copy_to_clipboard_native(text: &str) -> bool {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let plan = native_clipboard_plan_for(std::env::consts::OS, clipboard_is_wsl());
    let text = normalize_clipboard_newlines(
        text,
        matches!(
            plan,
            NativeClipboardPlan::Windows | NativeClipboardPlan::Wsl
        ),
    );
    let payload: Arc<[u8]> = Arc::from(text.as_bytes());
    let helper_deadline = Instant::now()
        .checked_add(Duration::from_millis(500))
        .unwrap_or_else(Instant::now);
    match plan {
        NativeClipboardPlan::Windows => copy_to_clipboard_windows(&text),
        NativeClipboardPlan::Macos => try_native_clipboard(
            "pbcopy",
            &[],
            Arc::clone(&payload),
            helper_deadline,
            NativeClipboardLifetime::Exit,
        ),
        NativeClipboardPlan::Wsl => try_native_clipboard(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::InputEncoding=[Text.UTF8Encoding]::new($false); Set-Clipboard -Value ([Console]::In.ReadToEnd())",
            ],
            payload,
            helper_deadline,
            NativeClipboardLifetime::Exit,
        ),
        NativeClipboardPlan::UnixLike => {
            try_native_clipboard(
                "wl-copy",
                &[],
                Arc::clone(&payload),
                helper_deadline,
                NativeClipboardLifetime::SelectionOwner,
            ) || try_native_clipboard(
                "xclip",
                &["-selection", "clipboard"],
                Arc::clone(&payload),
                helper_deadline,
                NativeClipboardLifetime::SelectionOwner,
            ) || try_native_clipboard(
                "xsel",
                &["--clipboard", "--input"],
                payload,
                helper_deadline,
                NativeClipboardLifetime::SelectionOwner,
            )
        }
    }
}

/// Normalize the internal LF representation at the native-OS boundary. Bare CR
/// is treated as a line break and an existing CRLF pair is never doubled.
pub(crate) fn normalize_clipboard_newlines(text: &str, windows: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if windows {
                    out.push_str("\r\n");
                } else {
                    out.push('\n');
                }
            }
            '\n' => {
                if windows {
                    out.push_str("\r\n");
                } else {
                    out.push('\n');
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeClipboardPlan {
    Windows,
    Macos,
    Wsl,
    UnixLike,
}

#[cfg(test)]
pub(crate) fn native_clipboard_plan(os: &str) -> NativeClipboardPlan {
    native_clipboard_plan_for(os, false)
}

fn native_clipboard_plan_for(os: &str, wsl: bool) -> NativeClipboardPlan {
    match os {
        "windows" => NativeClipboardPlan::Windows,
        "macos" => NativeClipboardPlan::Macos,
        _ if wsl => NativeClipboardPlan::Wsl,
        _ => NativeClipboardPlan::UnixLike,
    }
}

/// Queue the native helper away from the TUI thread. Completion is reported by
/// [`clipboard_feedback_text`]; generation ordering prevents a late old helper
/// from replacing a newer copy result.
fn dispatch_native_clipboard(text: &str) -> bool {
    let count = text.chars().count();
    let owned = text.to_string();
    dispatch_clipboard_job(count, move || copy_to_clipboard_native(&owned))
}

fn dispatch_clipboard_job<F>(count: usize, job: F) -> bool
where
    F: FnOnce() -> bool + Send + 'static,
{
    let generation = begin_clipboard_operation();
    let Ok(mut workers) = clipboard_workers().lock() else {
        publish_clipboard_feedback(generation, ClipboardFeedbackKind::Failed);
        return false;
    };
    let mut pending = Vec::with_capacity(workers.len());
    for worker in workers.drain(..) {
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            pending.push(worker);
        }
    }
    *workers = pending;
    if workers.len() >= MAX_CLIPBOARD_WORKERS {
        publish_clipboard_feedback(generation, ClipboardFeedbackKind::Failed);
        return false;
    }
    let spawned = std::thread::Builder::new()
        .name("umadev-clipboard-dispatch".to_string())
        .spawn(move || {
            let kind = if job() {
                ClipboardFeedbackKind::Copied(count)
            } else {
                ClipboardFeedbackKind::Failed
            };
            publish_clipboard_feedback(generation, kind);
        });
    let Ok(worker) = spawned else {
        publish_clipboard_feedback(generation, ClipboardFeedbackKind::Failed);
        return false;
    };
    workers.push(worker);
    true
}

fn begin_clipboard_operation() -> u64 {
    let generation = CLIPBOARD_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    if let Ok(mut slot) = clipboard_feedback_slot().lock() {
        *slot = None;
    }
    generation
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeClipboardLifetime {
    Exit,
    SelectionOwner,
}

const SELECTION_OWNER_SETTLE: std::time::Duration = std::time::Duration::from_millis(50);

fn try_native_clipboard(
    cmd: &str,
    args: &[&str],
    payload: std::sync::Arc<[u8]>,
    deadline: std::time::Instant,
    lifetime: NativeClipboardLifetime,
) -> bool {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::sync::mpsc::TryRecvError;
    use std::time::Duration;

    if std::time::Instant::now() >= deadline {
        return false;
    }
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    umadev_process::isolate_std_command(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let Ok(mut tree) = umadev_process::StdCommandTree::attach(&mut child) else {
        terminate_clipboard_helper(child, None);
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        terminate_clipboard_helper(child, Some(tree));
        return false;
    };
    let (write_tx, write_rx) = std::sync::mpsc::sync_channel(1);
    let writer = std::thread::Builder::new()
        .name("umadev-clipboard-writer".to_string())
        .spawn(move || {
            let wrote = stdin.write_all(&payload).is_ok();
            drop(stdin);
            let _ = write_tx.send(wrote);
        });
    let Ok(writer) = writer else {
        terminate_clipboard_helper(child, Some(tree));
        return false;
    };
    let mut writer = Some(writer);

    // The writer is separate from the TUI thread because a helper which never
    // reads stdin can fill the pipe and block `write_all`. Every fallback shares
    // one deadline, so Linux cannot accumulate three independent 500 ms stalls.
    let mut wrote = None;
    let mut wrote_at = None;
    loop {
        match write_rx.try_recv() {
            Ok(value) => {
                wrote = Some(value);
                if value {
                    wrote_at = Some(std::time::Instant::now());
                }
            }
            Err(TryRecvError::Disconnected) if wrote.is_none() => wrote = Some(false),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        if wrote == Some(false) {
            terminate_clipboard_helper(child, Some(tree));
            join_clipboard_writer(&mut writer);
            return false;
        }
        let polled = match lifetime {
            // Exit-style helpers must not be allowed to daemonize. Preserve the
            // Unix leader as an unreaped identity anchor until its whole tree
            // has been terminated.
            NativeClipboardLifetime::Exit => tree.try_wait(&mut child),
            // X11 clipboard owners are intentionally allowed to daemonize so
            // they can continue serving the selection after the launcher exits.
            NativeClipboardLifetime::SelectionOwner => child.try_wait(),
        };
        match polled {
            Ok(Some(status)) => {
                if !status.success() {
                    tree.terminate(&mut child);
                    let _ = child.wait();
                    join_clipboard_writer(&mut writer);
                    return false;
                }
                if wrote == Some(true) {
                    join_clipboard_writer(&mut writer);
                    finish_clipboard_success(child, tree, lifetime);
                    return true;
                }
            }
            Ok(None) => {
                if lifetime == NativeClipboardLifetime::SelectionOwner
                    && wrote_at.is_some_and(|at| at.elapsed() >= SELECTION_OWNER_SETTLE)
                {
                    finish_clipboard_success(child, tree, lifetime);
                    return true;
                }
            }
            Err(_) => {
                terminate_clipboard_helper(child, Some(tree));
                join_clipboard_writer(&mut writer);
                return false;
            }
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            terminate_clipboard_helper(child, Some(tree));
            join_clipboard_writer(&mut writer);
            return false;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(5)),
        );
    }
}

fn join_clipboard_writer(writer: &mut Option<std::thread::JoinHandle<()>>) {
    if let Some(writer) = writer.take() {
        let _ = writer.join();
    }
}

fn finish_clipboard_success(
    mut child: std::process::Child,
    mut tree: umadev_process::StdCommandTree,
    lifetime: NativeClipboardLifetime,
) {
    if lifetime == NativeClipboardLifetime::Exit {
        tree.terminate(&mut child);
        let _ = child.wait();
        return;
    }
    #[cfg(unix)]
    register_selection_owner(child, tree);
    #[cfg(not(unix))]
    {
        tree.terminate(&mut child);
        let _ = child.wait();
    }
}

#[cfg(unix)]
fn register_selection_owner(child: std::process::Child, tree: umadev_process::StdCommandTree) {
    let Ok(mut owners) = selection_owners().lock() else {
        return;
    };
    owners.retain_mut(|owner| !matches!(owner.child.try_wait(), Ok(Some(_)) | Err(_)));
    if owners.len() >= MAX_SELECTION_OWNERS {
        let mut stale = owners.remove(0);
        stale.tree.terminate(&mut stale.child);
        let _ = stale.child.wait();
    }
    owners.push(SelectionOwner { child, tree });
}

fn terminate_clipboard_helper(
    mut child: std::process::Child,
    mut tree: Option<umadev_process::StdCommandTree>,
) {
    if let Some(tree) = tree.as_mut() {
        tree.terminate(&mut child);
    } else {
        let _ = child.kill();
    }
    let _ = child.wait();
    drop(tree);
}

pub(crate) fn shutdown_clipboard_workers() {
    if let Ok(mut workers) = clipboard_workers().lock() {
        for worker in workers.drain(..) {
            let _ = worker.join();
        }
    }
    #[cfg(unix)]
    if let Ok(mut owners) = selection_owners().lock() {
        for mut owner in owners.drain(..) {
            owner.tree.retain_descendants();
        }
    }
}

#[cfg(windows)]
fn copy_to_clipboard_windows(text: &str) -> bool {
    // Publish UTF-16 directly as `CF_UNICODETEXT`. Shell clipboard helpers
    // decode stdin through the active console code page and corrupt CJK.
    umadev_host::set_windows_clipboard_text(text)
}

#[cfg(not(windows))]
fn copy_to_clipboard_windows(_text: &str) -> bool {
    false
}

pub(crate) fn copy_text_to_clipboard(
    app: &mut crate::app::App,
    terminal: &mut crate::Term,
    text: &str,
) -> bool {
    if clipboard_is_remote() {
        use std::io::Write as _;

        let generation = begin_clipboard_operation();
        let Some(sequence) = crate::selection::osc52_for(text, clipboard_in_tmux()) else {
            publish_clipboard_feedback(generation, ClipboardFeedbackKind::TooLarge);
            return false;
        };
        let backend = terminal.backend_mut();
        let copied = backend
            .write_all(sequence.as_bytes())
            .and_then(|()| backend.flush())
            .is_ok();
        app.contaminate_terminal();
        if !copied {
            publish_clipboard_feedback(generation, ClipboardFeedbackKind::Failed);
        }
        copied
    } else {
        dispatch_native_clipboard(text)
    }
}

pub(crate) fn finish_mouse_selection_copy(app: &mut crate::app::App, terminal: &mut crate::Term) {
    let copied = if app.input_selection_dragging {
        app.input_selection_finish_copy()
    } else {
        app.selection_finish_copy()
    };
    if let Some(text) = copied {
        let count = text.chars().count();
        if copy_text_to_clipboard(app, terminal, &text) {
            // Remote OSC 52 completes synchronously; local native copy reports
            // its real result asynchronously through the status-area feedback.
            if clipboard_is_remote() {
                app.show_copy_toast(count);
            }
        } else {
            app.transient_status = Some(umadev_i18n::t(app.lang, "tui.copy_failed").to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static FEEDBACK_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn native_plan_never_routes_windows_to_unix_commands() {
        assert_eq!(
            native_clipboard_plan("windows"),
            NativeClipboardPlan::Windows
        );
        assert_eq!(native_clipboard_plan("macos"), NativeClipboardPlan::Macos);
        assert_eq!(
            native_clipboard_plan("linux"),
            NativeClipboardPlan::UnixLike
        );
        assert_eq!(
            native_clipboard_plan("freebsd"),
            NativeClipboardPlan::UnixLike
        );
    }

    #[test]
    fn wsl_routes_text_to_the_windows_clipboard_bridge() {
        assert!(clipboard_wsl_from_signals(true, false, ""));
        assert!(clipboard_wsl_from_signals(false, true, ""));
        assert!(clipboard_wsl_from_signals(
            false,
            false,
            "5.15.153.1-microsoft-standard-WSL2"
        ));
        assert!(!clipboard_wsl_from_signals(false, false, "6.8.0-generic"));
        assert_eq!(
            native_clipboard_plan_for("linux", true),
            NativeClipboardPlan::Wsl
        );
        assert_eq!(
            native_clipboard_plan_for("windows", true),
            NativeClipboardPlan::Windows,
            "native Windows keeps the direct UTF-16 API"
        );
    }

    #[test]
    fn completed_async_failure_is_visible_in_the_idle_status_source() {
        let _guard = FEEDBACK_TEST_LOCK.lock().expect("feedback test lock");
        let generation = CLIPBOARD_GENERATION
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        publish_clipboard_feedback(generation, ClipboardFeedbackKind::Failed);
        assert!(clipboard_feedback_active());
        assert_eq!(
            clipboard_feedback_text(umadev_i18n::Lang::En),
            Some((
                umadev_i18n::t(umadev_i18n::Lang::En, "tui.copy_failed").to_string(),
                false
            ))
        );
        assert!(take_clipboard_feedback_redraw());
        assert!(!take_clipboard_feedback_redraw());
        *clipboard_feedback_slot().lock().expect("feedback lock") = Some(ClipboardFeedback {
            generation,
            kind: ClipboardFeedbackKind::Failed,
            expires_at: Instant::now(),
        });
        assert!(
            take_clipboard_feedback_redraw(),
            "the TTL edge schedules the one frame that removes the status"
        );
        assert!(!clipboard_feedback_active());
        *clipboard_feedback_slot().lock().expect("feedback lock") = None;
    }

    #[test]
    fn oversized_remote_copy_has_specific_localized_feedback() {
        let _guard = FEEDBACK_TEST_LOCK.lock().expect("feedback test lock");
        let generation = CLIPBOARD_GENERATION
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        publish_clipboard_feedback(generation, ClipboardFeedbackKind::TooLarge);
        let (message, copied) =
            clipboard_feedback_text(umadev_i18n::Lang::En).expect("oversized copy feedback");
        assert!(!copied);
        assert!(message.contains(&crate::selection::OSC52_MAX_TEXT_BYTES.to_string()));
        assert!(message.contains("nothing was copied"));
        *clipboard_feedback_slot().lock().expect("feedback lock") = None;
    }

    #[test]
    fn native_copy_dispatch_never_waits_for_the_helper() {
        use std::sync::mpsc;

        let _guard = FEEDBACK_TEST_LOCK.lock().expect("feedback test lock");

        let (release_tx, release_rx) = mpsc::sync_channel::<()>(0);
        let started = Instant::now();
        assert!(dispatch_clipboard_job(73, move || {
            let _ = release_rx.recv();
            true
        }));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "dispatch must return before the clipboard worker completes"
        );
        release_tx.send(()).expect("release clipboard worker");

        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if clipboard_feedback_text(umadev_i18n::Lang::En)
                .is_some_and(|(text, ok)| ok && text.contains("73"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker result never reached status"
            );
            std::thread::yield_now();
        }
        assert!(take_clipboard_feedback_redraw());
        assert!(!take_clipboard_feedback_redraw());
        *clipboard_feedback_slot().lock().expect("feedback lock") = None;
    }

    #[test]
    fn newlines_match_each_platform_without_doubling_crlf() {
        let mixed = "第一行\r\nsecond\rthird\n";
        assert_eq!(
            normalize_clipboard_newlines(mixed, false),
            "第一行\nsecond\nthird\n"
        );
        assert_eq!(
            normalize_clipboard_newlines(mixed, true),
            "第一行\r\nsecond\r\nthird\r\n"
        );
    }

    #[test]
    fn remote_detection_ignores_a_stale_ssh_tty() {
        assert!(clipboard_remote_from_env(true, true));
        assert!(clipboard_remote_from_env(true, false));
        assert!(!clipboard_remote_from_env(false, true));
        assert!(!clipboard_remote_from_env(false, false));
    }

    #[cfg(unix)]
    #[test]
    fn native_helper_deadline_covers_a_blocked_stdin_write() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("clipboard-blocker.pid");
        let script = format!(
            "sleep 30 <&0 & printf '%s' $! > '{}'; wait",
            pid_file.display()
        );
        let payload: Arc<[u8]> = Arc::from(vec![b'x'; 2 * 1024 * 1024]);
        let started = Instant::now();
        assert!(!try_native_clipboard(
            "sh",
            &["-c", &script],
            payload,
            started + Duration::from_millis(100),
            NativeClipboardLifetime::Exit,
        ));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a helper which does not read stdin must not freeze the TUI"
        );

        let blocker = std::fs::read_to_string(&pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let reaped_by = Instant::now() + Duration::from_secs(2);
        while unix_process_exists(blocker) && Instant::now() < reaped_by {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !unix_process_exists(blocker),
            "the blocked helper descendant survived deadline cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_helper_delivers_a_payload_larger_than_the_pipe_buffer() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let payload: Arc<[u8]> = Arc::from(vec![b'x'; 2 * 1024 * 1024]);
        assert!(try_native_clipboard(
            "cat",
            &[],
            payload,
            Instant::now() + Duration::from_secs(2),
            NativeClipboardLifetime::Exit,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn exit_helper_cleans_up_a_background_descendant_after_success() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("clipboard-descendant.pid");
        let script = format!(
            "cat >/dev/null; sleep 30 </dev/null & printf '%s' $! > '{}'",
            pid_file.display()
        );
        assert!(try_native_clipboard(
            "sh",
            &["-c", &script],
            Arc::from(b"clipboard payload".as_slice()),
            Instant::now() + Duration::from_secs(2),
            NativeClipboardLifetime::Exit,
        ));

        let descendant = std::fs::read_to_string(&pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let reaped_by = Instant::now() + Duration::from_secs(2);
        while unix_process_exists(descendant) && Instant::now() < reaped_by {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !unix_process_exists(descendant),
            "successful exit helper left a background descendant alive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn selection_owner_survives_a_successful_launcher_exit() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("clipboard-owner.pid");
        let script = format!(
            "cat >/dev/null; sleep 30 </dev/null & printf '%s' $! > '{}'",
            pid_file.display()
        );
        assert!(try_native_clipboard(
            "sh",
            &["-c", &script],
            Arc::from(b"clipboard payload".as_slice()),
            Instant::now() + Duration::from_secs(2),
            NativeClipboardLifetime::SelectionOwner,
        ));

        let owner = std::fs::read_to_string(&pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert!(unix_process_exists(owner));
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &owner.to_string()])
            .status();
    }

    #[cfg(unix)]
    fn unix_process_exists(pid: i32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
