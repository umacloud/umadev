//! Process-spawn hygiene shared by every site that boots a *foreign* child
//! (a dev server, a backend, a `/preview` server) while UmaDev's TUI owns the
//! alt-screen.
//!
//! The one primitive here — [`detach_from_controlling_terminal`] — puts the
//! child in its OWN session with NO controlling terminal, so a descendant that
//! writes straight to `/dev/tty` (a Spring/Logback console appender,
//! Maven/npm/Docker progress bars) can no longer paint over ratatui's
//! alt-screen. This is the crate that owns the `unsafe` `pre_exec` seam so the
//! `#![forbid(unsafe_code)]` crates (notably `umadev-tui`) can call it without
//! relaxing their own policy.

/// What [`detach_from_controlling_terminal`] does on the current platform. Pure
/// and `const`, so the per-platform intent is unit-testable without spawning a
/// process or reaching into a `Command`'s private state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachKind {
    /// Unix: a brand-new session via `setsid(2)` — the child loses the
    /// controlling terminal entirely, so even a raw `/dev/tty` write can't
    /// reach the parent's screen.
    NewSession,
    /// Windows: `CREATE_NEW_PROCESS_GROUP` — the nearest equivalent; the child
    /// leads its own process group so console Ctrl events aren't shared.
    NewProcessGroup,
    /// An unknown target: nothing is applied (the spawn is unchanged).
    None,
}

/// The detachment this platform applies. Pure — the truth table is unit-tested.
#[must_use]
pub const fn detach_kind() -> DetachKind {
    if cfg!(unix) {
        DetachKind::NewSession
    } else if cfg!(windows) {
        DetachKind::NewProcessGroup
    } else {
        DetachKind::None
    }
}

/// Detach a to-be-spawned child from the controlling terminal so its output —
/// and its descendants' output — can never paint over UmaDev's alt-screen.
///
/// On Unix the child gets a NEW SESSION (`setsid(2)`), dropping the controlling
/// terminal outright: a process *group* alone does NOT stop a `/dev/tty` write,
/// only losing the controlling terminal does. On Windows it sets
/// `CREATE_NEW_PROCESS_GROUP`. Elsewhere it is a no-op.
///
/// **Only safe when the child's stdio is already piped or null** — after this
/// the child has no controlling terminal, so a READ from the tty would hit
/// EOF / `SIGTTIN`. Every call site (dev-server / backend boot, `/preview`)
/// nulls or pipes all three streams. Deliberately NOT applied to the base-CLI
/// session spawns (`claude` / `codex` / `opencode`), some of which probe the
/// tty — those need separate handling.
///
/// **Fail-open**: the pre-exec closure ignores the `setsid` result so the spawn
/// always proceeds, and it calls ONLY an async-signal-safe syscall (no alloc,
/// no lock) as required between `fork` and `exec`.
#[allow(unsafe_code)] // the pre_exec seam; the rest of the crate stays unsafe-free (deny, not forbid)
pub fn detach_from_controlling_terminal(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: the closure runs in the forked child before `exec` and calls
        // ONLY `setsid(2)` — async-signal-safe, allocates nothing, takes no
        // lock. Its result is discarded (fail-open): failing to start a new
        // session must never abort the spawn.
        unsafe {
            cmd.as_std_mut().pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NEW_PROCESS_GROUP — the child leads its own process group so a
        // console Ctrl event delivered to the parent is not forwarded to it.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.as_std_mut().creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::{detach_from_controlling_terminal, detach_kind, DetachKind};

    #[test]
    fn detach_kind_matches_the_platform() {
        if cfg!(unix) {
            assert_eq!(detach_kind(), DetachKind::NewSession);
        } else if cfg!(windows) {
            assert_eq!(detach_kind(), DetachKind::NewProcessGroup);
        } else {
            assert_eq!(detach_kind(), DetachKind::None);
        }
    }

    // Behavioral seam: applying the detach to a real Command must still produce
    // a child that spawns and runs to completion — i.e. the `pre_exec`/`setsid`
    // closure is valid and fail-open, not a spawn-breaking hazard. Unix only:
    // `true` is not a Windows program.
    #[cfg(unix)]
    #[tokio::test]
    async fn detached_child_still_spawns_and_exits() {
        use std::process::Stdio;
        let mut cmd = tokio::process::Command::new("true");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach_from_controlling_terminal(&mut cmd);
        let status = cmd
            .spawn()
            .expect("detached `true` should spawn")
            .wait()
            .await
            .expect("detached `true` should exit");
        assert!(status.success(), "detached child ran in its own session");
    }
}
