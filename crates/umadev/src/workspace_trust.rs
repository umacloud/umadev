//! Whether a CLI verb runs its project as trusted, and `umadev trust`.
//!
//! The decision is kept by [`umadev_agent::workspace_trust`], outside the
//! project. A verb that runs the pipeline settles it once per project root:
//!
//! 1. `--trust-project` or `UMADEV_TRUST_PROJECT=1` trusts the project for this
//!    command only, without recording anything;
//! 2. otherwise the user's recorded decision applies;
//! 3. otherwise, on a terminal, the user is asked and the answer is recorded;
//! 4. otherwise (CI, a pipe, a script) the project is untrusted, with a note.
//!
//! The answer is published to the host, so every base the verb launches loads
//! or ignores the project's own vendor configuration accordingly.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use umadev_agent::TrustMode;

/// Environment switch trusting the project for one command, like `--trust-project`.
pub(crate) const TRUST_ENV: &str = "UMADEV_TRUST_PROJECT";

/// `--trust-project` on this command line.
static TRUST_FLAG: AtomicBool = AtomicBool::new(false);

/// Project roots already settled in this process, so a verb that resumes
/// through another asks and notes once.
static SETTLED: Mutex<Vec<(PathBuf, bool)>> = Mutex::new(Vec::new());

/// Record whether the command line passed `--trust-project`.
pub(crate) fn set_trust_flag(trust: bool) {
    TRUST_FLAG.store(trust, Ordering::Relaxed);
}

/// Where this command's answer comes from when the user has not decided.
enum Ask<'a> {
    /// Ask on the terminal: the prompt is written to `out`, the answer read from `input`.
    Terminal {
        input: &'a mut dyn BufRead,
        out: &'a mut dyn Write,
    },
    /// Nobody to ask.
    Nobody,
}

/// Whether this command runs `project_root` as trusted. Settled once per root
/// per process and published to the host.
pub(crate) fn settle(project_root: &Path) -> bool {
    if let Some(trusted) = SETTLED.lock().ok().and_then(|settled| {
        settled
            .iter()
            .find(|(root, _)| root == project_root)
            .map(|(_, trusted)| *trusted)
    }) {
        return trusted;
    }
    let overridden = TRUST_FLAG.load(Ordering::Relaxed)
        || std::env::var(TRUST_ENV).is_ok_and(|value| env_trusts(&value));
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let trusted = if interactive {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let mut out = std::io::stderr();
        settle_with(
            project_root,
            overridden,
            Ask::Terminal {
                input: &mut input,
                out: &mut out,
            },
        )
    } else {
        settle_with(project_root, overridden, Ask::Nobody)
    };
    if let Ok(mut settled) = SETTLED.lock() {
        settled.push((project_root.to_path_buf(), trusted));
    }
    trusted
}

fn settle_with(project_root: &Path, overridden: bool, ask: Ask<'_>) -> bool {
    let trusted = if overridden {
        true
    } else if let Some(decided) = umadev_agent::workspace_trust::decision(project_root) {
        decided
    } else if let Ask::Terminal { input, out } = ask {
        let answer = ask_on_terminal(project_root, input, out);
        if let Err(e) = umadev_agent::workspace_trust::record(project_root, answer) {
            eprintln!("[warn] could not save the trust decision, so it will be asked again: {e}");
        }
        answer
    } else {
        eprintln!(
            "[trust] {} is not a trusted project, so this runs at most in guarded mode and \
             bases ignore the project's own settings, hooks and MCP servers. Run `umadev \
             trust` to trust it, or pass --trust-project (or {TRUST_ENV}=1) for one command.",
            project_root.display()
        );
        false
    };
    umadev_host::project_config::set_project_trusted(project_root, trusted);
    trusted
}

/// Ask `Trust this project? [y/N]`. Anything but a yes, including an
/// unreadable answer, is no.
fn ask_on_terminal(project_root: &Path, input: &mut dyn BufRead, out: &mut dyn Write) -> bool {
    let _ = write!(
        out,
        "UmaDev has not run in {} on this machine before.\n\
         A trusted project may run in auto mode, and its bases load the project's own \
         settings, hooks and MCP servers, which can run code. Only trust a project whose \
         files you trust.\n\
         Trust this project? [y/N] ",
        project_root.display()
    );
    let _ = out.flush();
    let mut answer = String::new();
    input.read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn env_trusts(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `mode` as this command may run it in `project_root`: Auto only in a trusted
/// project, with a note when it was lowered.
pub(crate) fn cap_mode(project_root: &Path, mode: TrustMode) -> TrustMode {
    let capped = umadev_agent::workspace_trust::cap_tier(mode, settle(project_root));
    if capped != mode {
        eprintln!(
            "[trust] --mode {} needs a trusted project; running in {} instead.",
            mode.as_str(),
            capped.as_str()
        );
    }
    capped
}

/// The tier to resume `project_root`'s saved run at (see
/// [`umadev_agent::workspace_trust::resume_tier`]).
pub(crate) fn resume_mode(project_root: &Path, saved: TrustMode) -> TrustMode {
    umadev_agent::workspace_trust::resume_tier(project_root, saved, settle(project_root))
}

/// One line on where `project_root` stands, for `umadev trust` and `doctor`.
pub(crate) fn describe(project_root: &Path) -> String {
    match umadev_agent::workspace_trust::decision(project_root) {
        Some(true) => "trusted on this machine: bases load the project's own settings, and \
                       auto mode is available (`umadev trust --revoke` to stop)"
            .to_string(),
        Some(false) => "not trusted: at most guarded mode, and bases ignore the project's own \
                        settings, hooks and MCP servers (`umadev trust` to trust it)"
            .to_string(),
        None => "not decided yet, so untrusted: UmaDev asks on the first interactive run \
                 (`umadev trust` to trust it now)"
            .to_string(),
    }
}

/// `umadev trust [--revoke]`: record the user's decision for `project_root`.
pub(crate) fn cmd_trust(project_root: &Path, revoke: bool) -> Result<()> {
    umadev_agent::workspace_trust::record(project_root, !revoke)?;
    println!("{}: {}", project_root.display(), describe(project_root));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_nobody_can_answer_runs_an_undecided_project_untrusted() {
        crate::tests::isolate_state_directory();
        let project = tempfile::TempDir::new().unwrap();
        assert!(!settle_with(project.path(), false, Ask::Nobody));
        assert!(!umadev_host::project_config::loads_project_config(
            project.path()
        ));
        // Nothing was recorded: the next interactive run still asks.
        assert_eq!(
            umadev_agent::workspace_trust::decision(project.path()),
            None
        );
        assert_eq!(
            umadev_agent::workspace_trust::cap_tier(TrustMode::Auto, false),
            TrustMode::Guarded
        );
    }

    #[test]
    fn the_terminal_answer_is_recorded_and_anything_but_yes_is_no() {
        crate::tests::isolate_state_directory();
        for (typed, trusted) in [
            ("y\n", true),
            ("YES\n", true),
            ("\n", false),
            ("sure\n", false),
        ] {
            let project = tempfile::TempDir::new().unwrap();
            let mut input = typed.as_bytes();
            let mut out = Vec::new();
            let ask = Ask::Terminal {
                input: &mut input,
                out: &mut out,
            };
            assert_eq!(
                settle_with(project.path(), false, ask),
                trusted,
                "{typed:?}"
            );
            assert!(String::from_utf8(out).unwrap().contains("[y/N]"));
            assert_eq!(
                umadev_agent::workspace_trust::decision(project.path()),
                Some(trusted)
            );
            assert_eq!(
                umadev_host::project_config::loads_project_config(project.path()),
                trusted
            );
        }
    }

    #[test]
    fn the_flag_trusts_one_command_without_recording_it() {
        crate::tests::isolate_state_directory();
        let project = tempfile::TempDir::new().unwrap();
        assert!(settle_with(project.path(), true, Ask::Nobody));
        assert_eq!(
            umadev_agent::workspace_trust::decision(project.path()),
            None
        );
        for value in ["1", "true", " Yes ", "on"] {
            assert!(env_trusts(value));
        }
        for value in ["", "0", "false", "no"] {
            assert!(!env_trusts(value));
        }
    }

    #[test]
    fn umadev_trust_records_and_revokes() {
        crate::tests::isolate_state_directory();
        let project = tempfile::TempDir::new().unwrap();
        cmd_trust(project.path(), false).unwrap();
        assert!(umadev_agent::workspace_trust::is_trusted(project.path()));
        assert!(settle_with(project.path(), false, Ask::Nobody));

        cmd_trust(project.path(), true).unwrap();
        assert_eq!(
            umadev_agent::workspace_trust::decision(project.path()),
            Some(false)
        );
        assert!(!settle_with(project.path(), false, Ask::Nobody));
        assert!(!umadev_host::project_config::loads_project_config(
            project.path()
        ));
    }
}
