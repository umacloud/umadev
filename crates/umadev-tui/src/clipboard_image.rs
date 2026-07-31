//! Read an image directly from the local OS clipboard and materialise it as a
//! workspace file.
//!
//! A PTY only transports bytes, so an image-only clipboard never becomes a
//! bracketed-paste event. `Ctrl+V` is therefore an explicit TUI action which
//! runs the platform clipboard command off the render thread. No clipboard
//! crate is used: macOS and Windows have built-in commands; Linux uses the
//! conventional `wl-paste` / `xclip` tools and degrades honestly when absent.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::process::Stdio;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;

/// Maximum accepted clipboard image size. The complete PNG is removed when it
/// crosses this boundary; it is never silently truncated.
pub(crate) const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_CLEANUP_ENTRIES: usize = 1_024;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const WSLPATH_TIMEOUT: Duration = Duration::from_secs(1);
const WSLPATH_OUTPUT_BYTES: usize = 64 * 1024;
const HELPER_READER_GRACE: Duration = Duration::from_millis(500);
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);

const MACOS_SCRIPT: &str = r"on run argv
set targetFile to POSIX file (item 1 of argv)
set pngData to the clipboard as «class PNGf»
set fileRef to open for access targetFile with write permission
try
    set eof fileRef to 0
    write pngData to fileRef
    close access fileRef
on error errMsg number errNum
    try
        close access fileRef
    end try
    error errMsg number errNum
end try
end run";

const WINDOWS_SCRIPT: &str = "$target = [Text.Encoding]::Unicode.GetString(\
[Convert]::FromBase64String('{target_base64}')); \
$img = [Windows.Forms.Clipboard]::GetImage(); \
if ($null -eq $img) { exit 1 }; \
$img.Save($target, [System.Drawing.Imaging.ImageFormat]::Png); \
$img.Dispose()";

/// Result delivered back to the UI loop after the blocking capture finishes.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CaptureResult {
    /// A validated PNG was written under `.umadev/pasted/`.
    Image(PathBuf),
    /// The clipboard did not expose a PNG. This is intentionally silent: a
    /// normal text paste follows the existing terminal `Event::Paste` path.
    NoImage,
    /// Linux clipboard integration is not installed.
    MissingTool(&'static str),
    /// A real image exceeded [`MAX_IMAGE_BYTES`] and was removed.
    TooLarge(u64),
    /// Directory creation, command execution, or validation failed unexpectedly.
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputMode {
    /// The command itself writes the path passed in its argv.
    Direct,
    /// The command writes image bytes to stdout; Rust owns the destination file.
    Stdout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Platform {
    Macos,
    Windows,
    Wayland,
    X11,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandPlan {
    program: &'static str,
    args: Vec<OsString>,
    output: OutputMode,
    missing_hint: Option<&'static str>,
}

#[derive(Debug)]
enum HelperError {
    Io(io::Error),
    TimedOut,
    OutputTooLarge(u64),
}

impl From<io::Error> for HelperError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
struct HelperOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

/// Cheap, pure preflight used before spawning the blocking worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Preflight {
    Ready,
    Remote,
    Tmux,
    Offline,
}

pub(crate) fn preflight(remote: bool, tmux: bool, offline: bool) -> Preflight {
    if remote {
        Preflight::Remote
    } else if tmux {
        Preflight::Tmux
    } else if offline {
        Preflight::Offline
    } else {
        Preflight::Ready
    }
}

pub(crate) fn start_capture(
    app: &mut crate::app::App,
    in_flight: &mut bool,
    tx: &tokio::sync::mpsc::UnboundedSender<CaptureResult>,
) {
    let offline = matches!(app.brain_spec(), crate::BrainSpec::Offline);
    match preflight(
        crate::clipboard::clipboard_is_remote(),
        crate::clipboard::clipboard_in_tmux(),
        offline,
    ) {
        Preflight::Ready if !*in_flight => {
            *in_flight = true;
            let root = app.project_root.clone();
            let filesystem = app.project_filesystem.clone();
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = filesystem
                    .as_deref()
                    .map_or(CaptureResult::Failed, |filesystem| {
                        capture_with_root(&root, filesystem)
                    });
                let _ = tx.send(result);
            });
        }
        Preflight::Ready => {}
        Preflight::Remote => app.push_clipboard_image_notice("clipboard.image.remote", &[]),
        Preflight::Tmux => app.push_clipboard_image_notice("clipboard.image.tmux", &[]),
        Preflight::Offline => app.push_clipboard_image_notice("clipboard.image.offline", &[]),
    }
}

fn capture_with_root(
    project_root: &Path,
    filesystem: &umadev_state::fs::RootedDir,
) -> CaptureResult {
    let Some(relative) = next_relative_target(filesystem) else {
        return CaptureResult::Failed;
    };
    let target = project_root.join(&relative);
    let Ok(staging) = tempfile::Builder::new()
        .prefix("umadev-clipboard-")
        .tempdir()
    else {
        return CaptureResult::Failed;
    };
    let staging_target = staging.path().join("clipboard.png");

    let platform = select_platform(
        std::env::consts::OS,
        is_wsl(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
    );
    let plan = match platform {
        Platform::Macos => macos_plan(&staging_target),
        Platform::Windows => {
            let Some(platform_path) = windows_target_path(&staging_target) else {
                return CaptureResult::Failed;
            };
            windows_plan(&platform_path)
        }
        Platform::Wayland => linux_plan(true),
        Platform::X11 => linux_plan(false),
    };

    match run_plan(&plan, staging.path(), &staging_target) {
        CaptureResult::Image(_) => {
            let Ok(bytes) = umadev_state::fs::read_bounded_beneath(
                staging.path(),
                Path::new("clipboard.png"),
                MAX_IMAGE_BYTES,
            ) else {
                return CaptureResult::Failed;
            };
            if filesystem
                .publish_new_private(&relative, &bytes, true)
                .is_err()
            {
                return CaptureResult::Failed;
            }
            if !filesystem.matches_path(project_root).unwrap_or(false) {
                let _ = filesystem.remove_regular_file(&relative);
                return CaptureResult::Failed;
            }
            CaptureResult::Image(target)
        }
        other => other,
    }
}

/// Best-effort retention sweep. Only generated `.png` regular files older than
/// seven days are touched; directories, symlinks, and unrelated files survive.
pub(crate) fn cleanup_old(project_root: &Path) {
    let Ok(filesystem) = umadev_state::fs::RootedDir::open(project_root) else {
        return;
    };
    cleanup_old_rooted(&filesystem);
}

/// Retention sweep through the workspace capability captured at TUI launch.
/// This is used on shutdown so replacing the ambient project path while UmaDev
/// is running cannot redirect cleanup into another directory.
pub(crate) fn cleanup_old_rooted(filesystem: &umadev_state::fs::RootedDir) {
    let relative_dir = Path::new(".umadev/pasted");
    let Ok(entries) = filesystem.list_regular_files(relative_dir, MAX_CLEANUP_ENTRIES) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries {
        let name = Path::new(&entry.name);
        if generated_png_expired(name, true, entry.modified, now) {
            let _ = filesystem.remove_regular_file(&relative_dir.join(name));
        }
    }
}

/// Run the rooted retention sweep only when launch captured a workspace capability.
pub(crate) fn cleanup_old_if_available(filesystem: Option<&umadev_state::fs::RootedDir>) {
    if let Some(filesystem) = filesystem {
        cleanup_old_rooted(filesystem);
    }
}

fn generated_png_expired(
    path: &Path,
    is_file: bool,
    modified: Option<SystemTime>,
    now: SystemTime,
) -> bool {
    is_file
        && path.extension().and_then(|e| e.to_str()) == Some("png")
        && modified
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age > RETENTION)
}

fn select_platform(os: &str, wsl: bool, wayland: bool) -> Platform {
    match (os, wsl, wayland) {
        ("macos", _, _) => Platform::Macos,
        ("windows", _, _) | (_, true, _) => Platform::Windows,
        (_, false, true) => Platform::Wayland,
        _ => Platform::X11,
    }
}

/// Generate the path exclusively from process/time/counter state. Clipboard
/// bytes never participate in path construction (the path-injection floor).
#[cfg(test)]
fn next_target(project_root: &Path) -> Option<PathBuf> {
    let filesystem = umadev_state::fs::RootedDir::open(project_root).ok()?;
    Some(project_root.join(next_relative_target(&filesystem)?))
}

fn next_relative_target(filesystem: &umadev_state::fs::RootedDir) -> Option<PathBuf> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    let seq = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
    let relative =
        Path::new(".umadev/pasted").join(format!("{millis}-{}-{seq}.png", std::process::id()));
    filesystem.validate_path(&relative).ok()?;
    Some(relative)
}

fn macos_plan(target: &Path) -> CommandPlan {
    CommandPlan {
        program: "osascript",
        args: vec![
            OsString::from("-e"),
            OsString::from(MACOS_SCRIPT),
            OsString::from("--"),
            target.as_os_str().to_owned(),
        ],
        output: OutputMode::Direct,
        missing_hint: None,
    }
}

fn windows_plan(target: &OsString) -> CommandPlan {
    // Windows PowerShell does not populate `$args` from tokens placed after
    // `-Command`; it appends those tokens to the command text instead. Embed
    // the generated path as UTF-16LE Base64 so the script remains one final,
    // ASCII-only argument and path characters can never become PowerShell.
    let target_base64 = windows_path_base64(target);
    let script = WINDOWS_SCRIPT.replace("{target_base64}", &target_base64);
    CommandPlan {
        program: "powershell.exe",
        args: vec![
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            // Load System.Windows.Forms before touching Clipboard. `-STA` is a
            // process switch, not a script option; omitting it silently yields no image.
            OsString::from("-STA"),
            OsString::from("-Command"),
            OsString::from(format!(
                "Add-Type -AssemblyName System.Windows.Forms; {script}"
            )),
        ],
        output: OutputMode::Direct,
        missing_hint: None,
    }
}

fn windows_path_base64(target: &OsString) -> String {
    #[cfg(windows)]
    let wide = {
        use std::os::windows::ffi::OsStrExt as _;
        target.as_os_str().encode_wide().collect::<Vec<_>>()
    };
    #[cfg(not(windows))]
    let wide = target.to_string_lossy().encode_utf16().collect::<Vec<_>>();

    let bytes = wide
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn linux_plan(wayland: bool) -> CommandPlan {
    if wayland {
        CommandPlan {
            program: "wl-paste",
            args: vec![OsString::from("--type"), OsString::from("image/png")],
            output: OutputMode::Stdout,
            missing_hint: Some("wl-clipboard"),
        }
    } else {
        CommandPlan {
            program: "xclip",
            args: ["-selection", "clipboard", "-t", "image/png", "-o"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            output: OutputMode::Stdout,
            missing_hint: Some("xclip"),
        }
    }
}

fn run_bounded_stdout_helper(
    program: &str,
    args: &[OsString],
    timeout: Duration,
    max_bytes: usize,
) -> Result<HelperOutput, HelperError> {
    let mut command = Command::new(program);
    command.args(args);
    let output = umadev_process::run_bounded_std_command_strict_stdout(
        command,
        umadev_process::BoundedCommandOptions {
            timeout,
            stdout_bytes: max_bytes,
            stderr_bytes: 0,
            reader_grace: HELPER_READER_GRACE,
        },
    )?;
    if output.stdout_truncated {
        return Err(HelperError::OutputTooLarge(
            u64::try_from(max_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        ));
    }
    if output.timed_out {
        return Err(HelperError::TimedOut);
    }
    let status = output.status.ok_or_else(|| {
        HelperError::Io(io::Error::other("clipboard helper exited without status"))
    })?;
    Ok(HelperOutput {
        status,
        stdout: output.stdout,
    })
}

fn run_bounded_direct_helper(
    plan: &CommandPlan,
    target: &Path,
    timeout: Duration,
    max_bytes: u64,
) -> Result<ExitStatus, HelperError> {
    let mut command = Command::new(plan.program);
    command.args(&plan.args);
    let output = umadev_process::run_bounded_std_command(
        command,
        umadev_process::BoundedCommandOptions {
            timeout,
            stdout_bytes: 0,
            stderr_bytes: 0,
            reader_grace: HELPER_READER_GRACE,
        },
    )?;
    if let Ok(meta) = fs::symlink_metadata(target) {
        if !umadev_state::fs::metadata_is_real_file(&meta) {
            return Err(HelperError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "clipboard image target changed identity",
            )));
        }
        if meta.len() > max_bytes {
            return Err(HelperError::OutputTooLarge(meta.len()));
        }
    }
    if output.timed_out {
        return Err(HelperError::TimedOut);
    }
    output
        .status
        .ok_or_else(|| HelperError::Io(io::Error::other("clipboard helper exited without status")))
}

fn run_plan(plan: &CommandPlan, project_root: &Path, target: &Path) -> CaptureResult {
    run_plan_with_limits(plan, project_root, target, CAPTURE_TIMEOUT, MAX_IMAGE_BYTES)
}

fn run_plan_with_limits(
    plan: &CommandPlan,
    project_root: &Path,
    target: &Path,
    timeout: Duration,
    max_bytes: u64,
) -> CaptureResult {
    // A generated name must never overwrite an existing user file.
    if fs::symlink_metadata(target).is_ok()
        || target.strip_prefix(project_root).is_err()
        || !target_parent_is_beneath(project_root, target)
    {
        return CaptureResult::Failed;
    }

    if plan.output == OutputMode::Direct && create_target_exclusive(target).is_err() {
        return CaptureResult::Failed;
    }

    let status = match plan.output {
        OutputMode::Direct => match run_bounded_direct_helper(plan, target, timeout, max_bytes) {
            Ok(status) => status,
            Err(error) => return helper_failure_result(error, plan.missing_hint, target),
        },
        OutputMode::Stdout => {
            let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
            let output = match run_bounded_stdout_helper(plan.program, &plan.args, timeout, limit) {
                Ok(output) => output,
                Err(error) => return helper_failure_result(error, plan.missing_hint, target),
            };
            if !output.status.success() {
                let _ = fs::remove_file(target);
                return CaptureResult::NoImage;
            }
            let written =
                create_target_exclusive(target).and_then(|mut file| file.write_all(&output.stdout));
            if written.is_err() {
                let _ = fs::remove_file(target);
                return CaptureResult::Failed;
            }
            output.status
        }
    };
    if !status.success() {
        let _ = fs::remove_file(target);
        return CaptureResult::NoImage;
    }
    finish_capture_with_limit(project_root, target, max_bytes)
}

fn target_parent_is_beneath(project_root: &Path, target: &Path) -> bool {
    let Some(parent) = target.parent() else {
        return false;
    };
    let (Ok(root), Ok(parent)) = (project_root.canonicalize(), parent.canonicalize()) else {
        return false;
    };
    parent.starts_with(root)
        && fs::symlink_metadata(parent)
            .is_ok_and(|meta| umadev_state::fs::metadata_is_real_dir(&meta))
}

fn create_target_exclusive(target: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(target)?;
    if !file
        .metadata()
        .is_ok_and(|meta| umadev_state::fs::metadata_is_real_file(&meta))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "clipboard image target is not a regular file",
        ));
    }
    Ok(file)
}

fn helper_failure_result(
    error: HelperError,
    missing_hint: Option<&'static str>,
    target: &Path,
) -> CaptureResult {
    let _ = fs::remove_file(target);
    match error {
        HelperError::OutputTooLarge(len) => CaptureResult::TooLarge(len),
        HelperError::Io(error) if error.kind() == io::ErrorKind::NotFound => {
            missing_hint.map_or(CaptureResult::Failed, CaptureResult::MissingTool)
        }
        HelperError::Io(_) | HelperError::TimedOut => CaptureResult::Failed,
    }
}

#[cfg(test)]
fn finish_capture(project_root: &Path, target: &Path) -> CaptureResult {
    finish_capture_with_limit(project_root, target, MAX_IMAGE_BYTES)
}

fn finish_capture_with_limit(project_root: &Path, target: &Path, max_bytes: u64) -> CaptureResult {
    let Ok(relative) = target.strip_prefix(project_root) else {
        return CaptureResult::Failed;
    };
    let Ok(meta) = fs::symlink_metadata(target) else {
        return CaptureResult::Failed;
    };
    if !umadev_state::fs::metadata_is_real_file(&meta) {
        let _ = fs::remove_file(target);
        return CaptureResult::Failed;
    }
    let len = meta.len();
    if len > max_bytes {
        let _ = fs::remove_file(target);
        return CaptureResult::TooLarge(len);
    }
    let Ok(bytes) = umadev_state::fs::read_bounded_beneath(project_root, relative, max_bytes)
    else {
        let _ = fs::remove_file(target);
        return CaptureResult::Failed;
    };
    let valid = bytes.starts_with(&PNG_SIGNATURE);
    if !valid {
        let _ = fs::remove_file(target);
        return CaptureResult::NoImage;
    }
    CaptureResult::Image(target.to_path_buf())
}

fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
        || fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|s| s.to_ascii_lowercase().contains("microsoft"))
}

fn windows_target_path(target: &Path) -> Option<OsString> {
    if cfg!(windows) {
        return Some(target.as_os_str().to_owned());
    }
    // WSL's PowerShell is a Windows process and cannot open a Linux `/home/...`
    // path. `wslpath` is part of WSL and performs the boundary conversion. It is
    // still an external PATH helper, so run it under the same whole-tree,
    // wall-time, and output-size envelope as image capture rather than using the
    // unbounded `Command::output()` convenience API.
    let args = [OsString::from("-w"), OsString::from("--"), target.into()];
    let output =
        run_bounded_stdout_helper("wslpath", &args, WSLPATH_TIMEOUT, WSLPATH_OUTPUT_BYTES).ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| OsString::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn windows_argv_locks_sta_and_never_interpolates_the_path_into_script() {
        let path = OsString::from(r"C:\Users\我\shot '); Write-Output pwned; #.png");
        let plan = windows_plan(&path);
        let args = strings(&plan.args);
        assert_eq!(plan.program, "powershell.exe");
        assert_eq!(
            &args[..4],
            ["-NoProfile", "-NonInteractive", "-STA", "-Command"]
        );
        assert!(args[4].contains("System.Windows.Forms"));
        assert!(args[4].contains("[Convert]::FromBase64String"));
        assert!(args[4].contains("[Text.Encoding]::Unicode.GetString"));
        assert!(!args[4].contains("Write-Output pwned"));
        assert!(!args[4].contains(r"C:\Users"));
        assert_eq!(args.len(), 5, "-Command must be the final argument");

        let encoded = windows_path_base64(&path);
        assert!(args[4].contains(&format!("'{encoded}'")));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let wide = bytes
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect::<Vec<_>>();
        assert_eq!(String::from_utf16(&wide).unwrap(), path.to_string_lossy());
    }

    #[test]
    fn macos_argv_passes_the_generated_path_as_data() {
        let target = Path::new("/tmp/a path/图.png");
        let plan = macos_plan(target);
        let args = strings(&plan.args);
        assert_eq!(plan.program, "osascript");
        assert_eq!(args[0], "-e");
        assert!(args[1].contains("the clipboard as «class PNGf»"));
        assert!(args[1].contains("item 1 of argv"));
        assert!(!args[1].contains("/tmp/a path"));
        assert_eq!(&args[2..], ["--", "/tmp/a path/图.png"]);
    }

    #[test]
    fn linux_argv_is_exact_for_wayland_and_x11() {
        let wayland = linux_plan(true);
        assert_eq!(wayland.program, "wl-paste");
        assert_eq!(strings(&wayland.args), ["--type", "image/png"]);
        assert_eq!(wayland.missing_hint, Some("wl-clipboard"));

        let x11 = linux_plan(false);
        assert_eq!(x11.program, "xclip");
        assert_eq!(
            strings(&x11.args),
            ["-selection", "clipboard", "-t", "image/png", "-o"]
        );
        assert_eq!(x11.missing_hint, Some("xclip"));
    }

    #[test]
    fn platform_selection_covers_native_and_wsl_desktops() {
        assert_eq!(select_platform("macos", false, false), Platform::Macos);
        assert_eq!(select_platform("windows", false, false), Platform::Windows);
        assert_eq!(select_platform("linux", true, false), Platform::Windows);
        assert_eq!(select_platform("linux", false, true), Platform::Wayland);
        assert_eq!(select_platform("linux", false, false), Platform::X11);
    }

    #[test]
    fn preflight_is_honest_and_remote_wins_over_everything() {
        assert_eq!(preflight(false, false, false), Preflight::Ready);
        assert_eq!(preflight(true, false, false), Preflight::Remote);
        assert_eq!(preflight(false, true, false), Preflight::Tmux);
        assert_eq!(preflight(false, false, true), Preflight::Offline);
        assert_eq!(preflight(true, true, true), Preflight::Remote);
    }

    #[test]
    fn generated_targets_are_inside_workspace_and_ignore_hostile_clipboard_text() {
        let root = tempfile::tempdir().unwrap();
        let a = next_target(root.path()).unwrap();
        let b = next_target(root.path()).unwrap();
        assert!(a.starts_with(root.path().join(".umadev/pasted")));
        assert!(b.starts_with(root.path().join(".umadev/pasted")));
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy();
        assert!(!name.contains(".."));
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
    }

    #[test]
    fn symlinked_destination_is_rejected() {
        #[cfg(unix)]
        {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join(".umadev")).unwrap();
            std::os::unix::fs::symlink(outside.path(), root.path().join(".umadev/pasted")).unwrap();
            assert!(next_target(root.path()).is_none());
        }
    }

    #[test]
    fn valid_png_finishes_and_oversize_or_invalid_files_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let valid = root.path().join("valid.png");
        fs::write(&valid, [PNG_SIGNATURE.as_slice(), b"body"].concat()).unwrap();
        assert_eq!(
            finish_capture(root.path(), &valid),
            CaptureResult::Image(valid.clone())
        );

        let invalid = root.path().join("invalid.png");
        fs::write(&invalid, b"not a png").unwrap();
        assert_eq!(
            finish_capture(root.path(), &invalid),
            CaptureResult::NoImage
        );
        assert!(!invalid.exists());

        let large = root.path().join("large.png");
        let file = File::create(&large).unwrap();
        file.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        assert_eq!(
            finish_capture(root.path(), &large),
            CaptureResult::TooLarge(MAX_IMAGE_BYTES + 1)
        );
        assert!(!large.exists());
    }

    #[cfg(unix)]
    #[test]
    fn target_symlink_never_reads_or_writes_outside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.png");
        let outside_bytes = [PNG_SIGNATURE.as_slice(), b"outside"].concat();
        fs::write(&outside_file, &outside_bytes).unwrap();
        let target = root.path().join("capture.png");
        std::os::unix::fs::symlink(&outside_file, &target).unwrap();

        assert_eq!(finish_capture(root.path(), &target), CaptureResult::Failed);
        assert_eq!(fs::read(&outside_file).unwrap(), outside_bytes);

        std::os::unix::fs::symlink(&outside_file, &target).unwrap();
        let plan = CommandPlan {
            program: "sh",
            args: vec![
                OsString::from("-c"),
                OsString::from("printf overwritten > \"$1\""),
                OsString::from("umadev-clipboard-image-test"),
                target.as_os_str().to_owned(),
            ],
            output: OutputMode::Direct,
            missing_hint: None,
        };
        assert_eq!(
            run_plan_with_limits(
                &plan,
                root.path(),
                &target,
                Duration::from_secs(1),
                MAX_IMAGE_BYTES,
            ),
            CaptureResult::Failed
        );
        assert_eq!(fs::read(&outside_file).unwrap(), outside_bytes);
    }

    #[cfg(unix)]
    fn shell_plan(script: &str, args: &[&Path]) -> CommandPlan {
        let mut command_args = vec![
            OsString::from("-c"),
            OsString::from(script),
            OsString::from("umadev-clipboard-image-test"),
        ];
        command_args.extend(args.iter().map(|path| path.as_os_str().to_owned()));
        CommandPlan {
            program: "sh",
            args: command_args,
            output: OutputMode::Stdout,
            missing_hint: None,
        }
    }

    #[cfg(unix)]
    fn read_test_pid(path: &Path) -> u32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(pid) = fs::read_to_string(path)
                .and_then(|body| body.trim().parse::<u32>().map_err(io::Error::other))
            {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "helper never published its descendant pid"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    fn unix_process_exists(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    fn assert_process_reaped(pid: u32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unix_process_exists(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !unix_process_exists(pid),
            "clipboard helper descendant {pid} survived whole-tree cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn successful_stdout_wrapper_kills_pipe_holding_descendant() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("captured.png");
        let fixture = root.path().join("fixture.png");
        let pid_file = root.path().join("owner.pid");
        fs::write(&fixture, [PNG_SIGNATURE.as_slice(), b"body"].concat()).unwrap();
        let plan = shell_plan(
            r#"cat "$2"; sleep 30 & printf '%s' "$!" > "$1"; exit 0"#,
            &[&pid_file, &fixture],
        );

        assert_eq!(
            run_plan_with_limits(
                &plan,
                root.path(),
                &target,
                Duration::from_secs(2),
                MAX_IMAGE_BYTES,
            ),
            CaptureResult::Image(target.clone())
        );
        assert_eq!(fs::read(&target).unwrap(), fs::read(&fixture).unwrap());
        assert_process_reaped(read_test_pid(&pid_file));
    }

    #[cfg(unix)]
    #[test]
    fn failed_stdout_wrapper_reaps_pipe_holding_descendant_and_reader() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("failed.png");
        let fixture = root.path().join("fixture.png");
        let pid_file = root.path().join("failed-owner.pid");
        fs::write(&fixture, [PNG_SIGNATURE.as_slice(), b"body"].concat()).unwrap();
        let plan = shell_plan(
            r#"cat "$2"; sleep 30 & printf '%s' "$!" > "$1"; exit 7"#,
            &[&pid_file, &fixture],
        );
        let started = std::time::Instant::now();

        assert_eq!(
            run_plan_with_limits(
                &plan,
                root.path(),
                &target,
                Duration::from_secs(2),
                MAX_IMAGE_BYTES,
            ),
            CaptureResult::NoImage
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!target.exists());
        assert_process_reaped(read_test_pid(&pid_file));
    }

    #[cfg(unix)]
    #[test]
    fn successful_direct_wrapper_kills_background_descendant() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("direct.png");
        let fixture = root.path().join("fixture.png");
        let pid_file = root.path().join("direct.pid");
        fs::write(&fixture, [PNG_SIGNATURE.as_slice(), b"body"].concat()).unwrap();
        let plan = CommandPlan {
            program: "sh",
            args: vec![
                OsString::from("-c"),
                OsString::from(r#"cat "$3" > "$2"; sleep 30 & printf '%s' "$!" > "$1"; exit 0"#),
                OsString::from("umadev-clipboard-image-test"),
                pid_file.as_os_str().to_owned(),
                target.as_os_str().to_owned(),
                fixture.as_os_str().to_owned(),
            ],
            output: OutputMode::Direct,
            missing_hint: None,
        };

        assert_eq!(
            run_plan_with_limits(
                &plan,
                root.path(),
                &target,
                Duration::from_secs(2),
                MAX_IMAGE_BYTES,
            ),
            CaptureResult::Image(target.clone())
        );
        assert_eq!(fs::read(&target).unwrap(), fs::read(&fixture).unwrap());
        assert_process_reaped(read_test_pid(&pid_file));
    }

    #[cfg(unix)]
    #[test]
    fn blocked_stdout_wrapper_times_out_and_kills_descendant() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("blocked.png");
        let pid_file = root.path().join("blocked.pid");
        let plan = shell_plan(r#"sleep 30 & printf '%s' "$!" > "$1"; wait"#, &[&pid_file]);
        let started = std::time::Instant::now();

        assert_eq!(
            run_plan_with_limits(
                &plan,
                root.path(),
                &target,
                Duration::from_millis(100),
                MAX_IMAGE_BYTES,
            ),
            CaptureResult::Failed
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!target.exists());
        assert_process_reaped(read_test_pid(&pid_file));
    }

    #[cfg(unix)]
    #[test]
    fn flooding_stdout_wrapper_is_capped_and_kills_descendant() {
        const TEST_LIMIT: u64 = 32 * 1024;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("flood.png");
        let fixture = root.path().join("fixture.png");
        let pid_file = root.path().join("flood.pid");
        fs::write(&fixture, PNG_SIGNATURE).unwrap();
        let plan = shell_plan(
            r#"cat "$2"; sh -c 'while [ ! -s "$1" ]; do :; done; exec yes x' sh "$1" & child=$!; printf '%s' "$child" > "$1"; wait"#,
            &[&pid_file, &fixture],
        );
        let started = std::time::Instant::now();

        let result = run_plan_with_limits(
            &plan,
            root.path(),
            &target,
            Duration::from_secs(2),
            TEST_LIMIT,
        );
        assert!(matches!(result, CaptureResult::TooLarge(len) if len > TEST_LIMIT));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!target.exists(), "oversize output is never materialised");
        assert_process_reaped(read_test_pid(&pid_file));
    }

    #[test]
    fn cleanup_only_removes_old_generated_png_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".umadev/pasted");
        fs::create_dir_all(&dir).unwrap();
        let fresh = dir.join("fresh.png");
        let unrelated = dir.join("keep.txt");
        fs::write(&fresh, PNG_SIGNATURE).unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        cleanup_old(root.path());
        assert!(fresh.exists());
        assert!(unrelated.exists());

        let now = UNIX_EPOCH + Duration::from_secs(20 * 24 * 60 * 60);
        let old = now - Duration::from_secs(8 * 24 * 60 * 60);
        assert!(generated_png_expired(
            Path::new("old.png"),
            true,
            Some(old),
            now
        ));
        assert!(!generated_png_expired(
            Path::new("old.txt"),
            true,
            Some(old),
            now
        ));
        assert!(!generated_png_expired(
            Path::new("old.png"),
            false,
            Some(old),
            now
        ));
    }
}
