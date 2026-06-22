//! Advisory single-writer lock per workspace.
//!
//! Two concurrent `umadev` runs in the same workspace (e.g. the chat TUI plus a
//! scripted `umadev continue` in another terminal) share `workflow-state.json`,
//! `output/*`, and the provider config — running them at once silently corrupts
//! ordering and clobbers artifacts. This is the same hazard Terraform guards
//! with state locking and git with `index.lock`.
//!
//! The lock is a `.umadev/run.lock` file created with `create_new`
//! (`O_CREAT|O_EXCL`), holding the **owner identity** (`{pid, host, ts}`), and
//! removed on drop. It is **dependency-free** and **fail-open**: any IO error
//! other than "already exists" yields an un-owned guard that never blocks the
//! run (a lock bug must never stop a legitimate run).
//!
//! ## Stale-lock recovery (PID liveness)
//!
//! When the lock already exists we don't blindly refuse — that's what wedged the
//! user after a `Ctrl-C`/crash left an orphan `run.lock` behind. Instead we read
//! the owner identity and decide:
//!
//! 1. **Same host + owner PID is dead** → the previous run crashed; the lock is
//!    stale. Reclaim it and take over. This is the primary path (mirrors how a
//!    DBMS / `flock`-style supervisor reaps a dead holder).
//! 2. **Same host + owner PID is alive** → a real concurrent run; refuse with an
//!    actionable message.
//! 3. **Different host, or identity unparseable/missing** → we can't probe
//!    liveness, so fall back to a generous age threshold ([`STALE_SECS`]): an
//!    ancient lock with no heartbeat is reclaimed, otherwise refuse (with a
//!    "delete the file to force" hint).
//!
//! Liveness probing is itself **fail-open**: if we cannot determine whether the
//! PID is alive we treat it as *alive* (conservative — never reclaim a lock that
//! might be live just because the probe errored), and the age fallback still
//! frees a genuinely abandoned lock.

use std::io;
use std::path::{Path, PathBuf};

/// A same-host lock whose owner PID we could not prove dead is reclaimed only
/// once it is older than this (the cross-host / unparseable fallback). No
/// UmaDev pipeline block runs anywhere near six hours, so this never reclaims a
/// live run; a user with a genuinely longer run can delete the lock file by
/// hand (the refusal message says so).
const STALE_SECS: u64 = 6 * 3600;

/// Held for the duration of a pipeline block; releases the workspace lock on
/// drop. An un-owned guard (fail-open path) is a harmless no-op.
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
    owned: bool,
}

/// Parsed contents of a `run.lock` file: who claims to hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Owner {
    /// Process id of the holder (`0` if it could not be parsed).
    pid: u32,
    /// Hostname of the holder, or empty if absent (older lock format / corrupt).
    host: String,
    /// UNIX-seconds creation timestamp (`0` if absent / corrupt).
    ts: u64,
}

impl RunLock {
    /// Acquire the workspace run lock.
    ///
    /// # Errors
    /// Returns `AlreadyExists` with an actionable message when another **live**
    /// run holds the lock. A lock left behind by a crashed/killed run on this
    /// host is detected via PID liveness, reclaimed automatically, and taken
    /// over. Any other IO problem fails open (returns an un-owned guard) so a
    /// lock bug can never block a legitimate run.
    pub fn acquire(project_root: &Path) -> io::Result<RunLock> {
        let dir = project_root.join(".umadev");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("run.lock");
        // A BOUNDED loop (at most one stale-reclaim retry) — never recurse, so a
        // wedged-but-undeletable stale lock can't blow the stack.
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    // Owner identity: PID + host + creation timestamp. The host
                    // lets us avoid probing a PID that belongs to a *different*
                    // machine's process table (a shared/NFS workspace).
                    let _ = writeln!(
                        file,
                        "pid={} host={} ts={}",
                        std::process::id(),
                        hostname(),
                        now_secs()
                    );
                    return Ok(RunLock { path, owned: true });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // The lock is taken. Classify the holder into exactly three
                    // cases (the misleading "another umadev" message used to fire
                    // for all of them):
                    //
                    //   1. holder == THIS process  → our own session already has a
                    //      run in flight. Not an error to escalate as "another
                    //      umadev"; the caller should queue the input INTO that
                    //      run. Signalled with `WouldBlock` + an accurate message.
                    //   2. holder is a dead PID on this host → crashed/killed run;
                    //      reclaim and take over (handled by is_stale below).
                    //   3. holder is a live foreign run → the genuine
                    //      "another umadev is running" refusal.
                    if holder_is_self(&path) {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "本会话已有一个 umadev run 正在进行中 —— \
                             你的输入会排队发给这个 run,而不是另起新 run。"
                                .to_string(),
                        ));
                    }
                    // Only retry if we actually RECLAIMED a stale leftover; if the
                    // remove fails (undeletable lock), fall through to refusal.
                    if attempt == 0 && is_stale(&path) && std::fs::remove_file(&path).is_ok() {
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "另一个 umadev 运行正在占用该工作区(锁文件 {}).\n\
                             请等它结束。如果确定没有其他运行(上次异常退出残留),\
                             删除该文件后重试:\n  rm {}",
                            path.display(),
                            path.display()
                        ),
                    ));
                }
                // Fail-open: a permissions/IO problem must not block a real run.
                Err(_) => return Ok(RunLock { path, owned: false }),
            }
        }
        // Both attempts hit AlreadyExists-but-couldn't-reclaim in a tight race —
        // fail open rather than spin.
        Ok(RunLock { path, owned: false })
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `true` when the lock at `path` is held by **this very process** on this
/// host — i.e. the current session already has a run in flight. Used to turn the
/// misleading "another umadev is running" refusal into an accurate
/// "your input will be queued to the existing run" signal. Fail-open: an
/// unreadable/unparseable lock is NOT attributed to us (so we never silently
/// swallow a foreign lock as our own).
fn holder_is_self(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(owner) = Owner::parse(&contents) else {
        return false;
    };
    let same_host = owner.host.is_empty() || owner.host == hostname();
    owner.pid == std::process::id() && same_host
}

/// `true` when the lock at `path` belongs to a crashed/abandoned run and may be
/// reclaimed. Decision order (PID liveness first, age fallback second):
///
/// 1. Same host + dead PID  → stale (reclaim).
/// 2. Same host + live PID  → NOT stale (real concurrent run).
/// 3. Cross-host / unparseable owner → stale only if older than [`STALE_SECS`].
///
/// Fail-open at every branch: an unreadable file is treated as stale (the holder
/// can no longer be identified, so it can't be live), and a PID we cannot probe
/// is treated as *alive* so we never steal a lock from a process that might be
/// running — the age fallback still frees a truly abandoned lock.
fn is_stale(path: &Path) -> bool {
    // Can't read it → owner is unidentifiable → safe to reclaim.
    let Ok(contents) = std::fs::read_to_string(path) else {
        return true;
    };
    // Corrupt / legacy-without-fields → fall back to age only.
    let Some(owner) = Owner::parse(&contents) else {
        return older_than_stale(path);
    };

    let same_host = !owner.host.is_empty() && owner.host == hostname();
    if same_host && owner.pid != 0 {
        // Primary path: probe the actual holder. Dead → stale; alive → live.
        return match pid_is_alive(owner.pid) {
            Some(true) => false,            // a real run holds it
            Some(false) => true,            // crashed/killed → reclaim
            None => older_than_stale(path), // can't probe → conservative + age fallback
        };
    }

    // Different host (can't probe its process table) or no usable PID: the only
    // safe signal left is age.
    older_than_stale(path)
}

/// `true` when the lock file's mtime is older than [`STALE_SECS`] (or can't be
/// stat'd — an unstattable file can't be a live heartbeat).
fn older_than_stale(path: &Path) -> bool {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime
            .elapsed()
            .map(|age| age.as_secs() > STALE_SECS)
            .unwrap_or(false),
        Err(_) => true,
    }
}

impl Owner {
    /// Parse `pid=<n> host=<name> ts=<n>` (whitespace-separated, any order, extra
    /// keys ignored). Returns `None` if no `pid=` key is present at all — older
    /// `pid=.. ts=..` lines without a host still parse (host = empty).
    fn parse(s: &str) -> Option<Owner> {
        let line = s.lines().next().unwrap_or("");
        let mut pid: Option<u32> = None;
        let mut host = String::new();
        let mut ts: u64 = 0;
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("pid=") {
                pid = v.parse().ok();
            } else if let Some(v) = tok.strip_prefix("host=") {
                host = v.to_string();
            } else if let Some(v) = tok.strip_prefix("ts=") {
                ts = v.parse().unwrap_or(0);
            }
        }
        pid.map(|pid| Owner { pid, host, ts })
    }
}

/// Best-effort hostname, dependency-free. Reads the usual env vars and, on Unix,
/// falls back to `/etc/hostname`-equivalent via `uname -n`. An empty string when
/// nothing is available — callers treat empty as "host unknown" (no same-host
/// PID probe, age fallback only), which is the safe direction.
fn hostname() -> String {
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(h) = std::env::var(key) {
            let h = h.trim();
            if !h.is_empty() {
                return h.to_string();
            }
        }
    }
    // Unix `HOSTNAME` is often unexported; ask the OS directly. Fail-open: if the
    // command is missing or errors we just return empty.
    #[cfg(unix)]
    {
        if let Ok(out) = std::process::Command::new("uname").arg("-n").output() {
            if out.status.success() {
                let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !h.is_empty() {
                    return h;
                }
            }
        }
    }
    String::new()
}

/// Is the process `pid` currently alive on **this** host?
///
/// `Some(true)` alive, `Some(false)` provably gone, `None` could-not-determine
/// (caller stays conservative). Dependency-free:
/// - **Unix**: `kill -0 <pid>` — exit 0 means the process exists (or exists but
///   we lack permission, which still proves it is alive); a "no such process"
///   failure proves it is gone. Implemented via `/bin/kill` semantics through
///   `Command`, with no `libc` dependency.
/// - **Windows**: `tasklist /FI "PID eq <pid>"` and look for the PID in output.
/// - Anything else / probe error → `None`.
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> Option<bool> {
    // `kill -0` sends no signal but performs the permission/existence check.
    // Exit status 0 → exists. Non-zero → distinguish "no such process" (gone)
    // from other errors (unknown). We run the standalone `kill` utility so this
    // stays free of a libc dependency.
    let out = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if out.status.success() {
        return Some(true);
    }
    // `kill -0` failed. Classify by the failure reason:
    //  - "no such process"   → the PID is valid but gone (reclaimable).
    //  - "illegal/invalid process id" → an impossible PID; it can't be live.
    //  - "not permitted"/"permission" → the process EXISTS but is owned by
    //    someone else (alive — never reclaim).
    //  - anything else → unknown; stay conservative (caller uses age fallback).
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    if stderr.contains("no such process") {
        Some(false)
    } else if stderr.contains("illegal") || stderr.contains("invalid") {
        // A PID outside the valid range / unparseable by `kill` — not a real,
        // running process.
        Some(false)
    } else if stderr.contains("not permitted") || stderr.contains("permission") {
        // Process exists but is owned by someone else.
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    // tasklist prints "INFO: No tasks ..." when nothing matches; a real row
    // contains the PID. Look for the pid as a CSV field.
    if stdout.to_lowercase().contains("no tasks") {
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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a raw lock file with the given contents.
    fn write_lock(root: &Path, contents: &str) {
        let dir = root.join(".umadev");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("run.lock"), contents).unwrap();
    }

    /// A PID that is essentially guaranteed never to be live. It is inside the
    /// valid PID range on every platform we target (so `kill`/`tasklist` report
    /// "no such process" rather than rejecting it as out-of-range), yet far above
    /// any PID a real run would have, so liveness probes report "gone". PIDs are
    /// recycled, but nothing in CI is anywhere near this value.
    const DEAD_PID: u32 = 4_000_000;

    #[test]
    fn second_acquire_in_same_session_is_queue_signal_not_another_umadev() {
        // CASE 1: the lock is held by THIS process (our own session already has a
        // run in flight). A second acquire must NOT report "another umadev" —
        // it returns a WouldBlock "queue your input to the existing run" signal so
        // the caller routes the input into the running pipeline.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        let lock = RunLock::acquire(root).expect("first acquire");
        let second = RunLock::acquire(root).expect_err("second same-session acquire is signalled");
        assert_eq!(
            second.kind(),
            io::ErrorKind::WouldBlock,
            "our own session's lock is a queue signal, not a hard refusal"
        );
        let msg = second.to_string();
        assert!(
            !msg.contains("另一个 umadev"),
            "must NOT claim another umadev is running for our own lock"
        );
        assert!(
            msg.contains("排队"),
            "message must explain the input will be queued to the existing run"
        );
        // Dropping the first releases the lock; a later acquire succeeds.
        drop(lock);
        assert!(RunLock::acquire(root).is_ok(), "lock released on drop");
    }

    #[test]
    fn foreign_live_run_is_the_real_another_umadev_refusal() {
        // CASE 3: a DIFFERENT, still-alive process on this host holds the lock —
        // the genuine "another umadev is running" refusal. Modelled with PID 1
        // (init/launchd): present and alive on every Unix host, and never us.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        #[cfg(unix)]
        {
            write_lock(
                root,
                &format!("pid=1 host={} ts={}", hostname(), now_secs()),
            );
            let path = root.join(".umadev").join("run.lock");
            assert!(!holder_is_self(&path), "PID 1 is not our process");
            assert!(
                !is_stale(&path),
                "a live foreign PID must not be reclaimable"
            );
            let err = RunLock::acquire(root).expect_err("foreign live run refused");
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
            assert!(
                err.to_string().contains("另一个 umadev"),
                "a live foreign run is the genuine 'another umadev' refusal"
            );
            assert!(err.to_string().contains("rm "), "refusal stays actionable");
        }
    }

    #[test]
    fn stale_lock_with_dead_pid_is_reclaimed_and_taken_over() {
        // The user's bug: a crashed run left a fresh lock with a dead PID on this
        // host. PID liveness must classify it stale even though it is brand new.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        write_lock(
            root,
            &format!("pid={DEAD_PID} host={} ts={}", hostname(), now_secs()),
        );
        let path = root.join(".umadev").join("run.lock");
        assert!(
            is_stale(&path),
            "a fresh lock whose owner PID is dead must be reclaimable"
        );
        // End-to-end: acquire reclaims it and takes over, then owns the new lock.
        let lock = RunLock::acquire(root).expect("stale lock auto-reclaimed");
        assert!(lock.owned, "reclaimed lock is owned by us");
        // The new lock records OUR identity, not the dead holder's.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains(&format!("pid={}", std::process::id())));
    }

    #[test]
    fn corrupt_lock_fails_open_via_age_and_hint() {
        // A garbage / truncated lock with no parseable owner: PID-liveness can't
        // run, so we fall back to age. A FRESH corrupt lock is conservatively
        // respected (refused) but the refusal tells the user how to force-clear.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        write_lock(root, "\u{0}\u{0}garbage-not-a-lock");
        let path = root.join(".umadev").join("run.lock");
        // Fresh + unparseable → not yet age-stale → refuse, but actionably.
        assert!(!is_stale(&path), "fresh corrupt lock is not age-stale");
        let err = RunLock::acquire(root).expect_err("fresh corrupt lock refused");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(
            err.to_string().contains("rm "),
            "refusal must tell the user how to force-clear the lock"
        );
    }

    #[test]
    fn unreadable_owner_treated_as_reclaimable() {
        // An empty lock file (no owner at all): owner is unidentifiable, so it
        // cannot be a live holder — reclaimable so a truncated write doesn't wedge
        // the workspace forever.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        write_lock(root, "");
        // Empty → Owner::parse None → age fallback. An empty file is unparseable
        // but fresh, so this asserts the parse boundary, not reclaim.
        assert!(Owner::parse("").is_none(), "empty contents have no owner");
        // A whitespace-only first line likewise yields no owner.
        assert!(Owner::parse("   \n").is_none());
        // Acquire on an empty fresh lock: unparseable + fresh → refused with hint.
        let err = RunLock::acquire(root).expect_err("fresh empty lock refused");
        assert!(err.to_string().contains("rm "));
    }

    #[test]
    fn owner_parse_handles_legacy_and_new_formats() {
        // New format.
        let o = Owner::parse("pid=4321 host=mybox ts=1700000000").expect("parses");
        assert_eq!(o.pid, 4321);
        assert_eq!(o.host, "mybox");
        assert_eq!(o.ts, 1_700_000_000);
        // Legacy format (no host) still parses; host empty → no same-host probe.
        let legacy = Owner::parse("pid=99 ts=0").expect("legacy parses");
        assert_eq!(legacy.pid, 99);
        assert!(legacy.host.is_empty());
        // Reordered / extra keys tolerated.
        let reordered = Owner::parse("ts=5 extra=x pid=7 host=h").expect("parses");
        assert_eq!((reordered.pid, reordered.host.as_str()), (7, "h"));
    }

    #[test]
    fn pid_liveness_self_is_alive() {
        // Our own PID must probe as alive on every supported platform; if the
        // probe is unavailable it returns None (never a false "dead").
        match pid_is_alive(std::process::id()) {
            Some(true) | None => {}
            Some(false) => panic!("our own running process must not probe as dead"),
        }
        // A clearly-invalid PID must never probe as *alive*.
        assert_ne!(
            pid_is_alive(DEAD_PID),
            Some(true),
            "an impossible PID must not probe as alive"
        );
    }
}
