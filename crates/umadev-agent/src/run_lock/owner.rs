//! Run-owner metadata and the shared process-liveness classifier.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Age fallback used by the shared crash-marker/legacy-owner classifier.
pub(super) const STALE_SECS: u64 = 6 * 3600;

static NONCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Parsed owner row (`run.owner`, a legacy `run.lock`, or the v2 fence).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Owner {
    /// Process id of the holder (`0` if it could not be parsed).
    pub(super) pid: u32,
    /// Hostname of the holder, or empty if absent (older lock format / corrupt).
    pub(super) host: String,
    /// UNIX-seconds creation timestamp (`0` if absent / corrupt).
    pub(super) ts: u64,
    /// A per-BOOT identifier (empty if unavailable). Lets a lock left by a PRE-REBOOT run be
    /// told apart from a live run that merely reused the same recycled PID after a reboot.
    pub(super) boot: String,
    /// Per-acquisition identity. An old guard may remove metadata only while
    /// this nonce still matches.
    pub(super) nonce: String,
    /// Compatibility protocol marker (`0` for legacy owner rows).
    pub(super) protocol: u8,
}
pub(super) fn unique_nonce() -> String {
    let sequence = NONCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{:x}-{nanos:x}-{sequence:x}", std::process::id())
}

pub(super) fn write_owner(path: &Path, nonce: &str) -> io::Result<()> {
    let contents = format!(
        "pid={} host={} ts={} boot={} nonce={}",
        std::process::id(),
        hostname(),
        now_secs(),
        boot_id(),
        nonce
    );
    umadev_state::fs::atomic_write(path, contents.as_bytes())
}

pub(super) fn read_owner(path: &Path) -> io::Result<Owner> {
    const MAX_OWNER_BYTES: u64 = 4 * 1024;
    let bytes = umadev_state::fs::read_bounded(path, MAX_OWNER_BYTES)?;
    let contents = std::str::from_utf8(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Owner::parse(contents)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid run-lock owner"))
}

pub(super) fn holder_nonce_matches(path: &Path, nonce: &str) -> bool {
    !nonce.is_empty() && read_owner(path).is_ok_and(|owner| owner.nonce == nonce)
}

/// `true` when the lock at `path` is held by **this very process** on this
/// host — i.e. the current session already has a run in flight. Used to turn the
/// misleading "another umadev is running" refusal into an accurate
/// "your input will be queued to the existing run" signal. Fail-open: an
/// unreadable/unparseable lock is NOT attributed to us (so we never silently
/// swallow a foreign lock as our own).
pub(super) fn holder_is_self(path: &Path) -> bool {
    let Ok(owner) = read_owner(path) else {
        return false;
    };
    let local_host = hostname();
    let local_boot = boot_id();
    let same_host = owner.host.is_empty() || local_host.is_empty() || owner.host == local_host;
    // A recorded boot-id that differs from THIS boot means the lock predates a reboot, so a
    // matching PID is a RECYCLED pid, not us - never treat that as self (else the routing
    // layer queues input into a run that no longer exists). Empty on EITHER side = unknown =
    // "matches": an unreadable local boot id (platform provider unavailable)
    // must not make us stop recognising our OWN lock.
    let same_boot = owner.boot.is_empty() || local_boot.is_empty() || owner.boot == local_boot;
    owner.pid == std::process::id() && same_host && same_boot
}

/// The recorded owner of a workspace claim: a `.umadev/run.lock` line, or a temporary-rewind
/// crash marker ([`crate::checkpoint`]). Both files exist to answer ONE question — "is the
/// process that wrote this still alive?" — and getting two different answers out of them is
/// how a live single-writer lock gets reclaimed under a live holder.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClaimOwner<'a> {
    /// The owner's process id (`0` = none recorded).
    pub pid: u32,
    /// The owner's hostname (empty = not recorded / unreadable).
    pub host: &'a str,
    /// The owner's boot id (empty = not recorded / unreadable).
    pub boot: &'a str,
}

/// What we can PROVE about a claim's owner. Deliberately three-valued: "I could not tell"
/// is a distinct answer from "it is dead", and collapsing them is what reclaims live locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerLiveness {
    /// Alive (proved, or conservatively assumed) — the claim stands. NEVER reclaim.
    Live,
    /// Provably gone — the claim may be reclaimed right now.
    Abandoned,
    /// Unattributable. The caller's own age window is the only honest signal left.
    AgeOnly,
}

/// The ONE owner-liveness rule, shared by the run lock ([`is_stale`]) and the temporary-rewind
/// crash marker (`checkpoint::marker_is_abandoned`). Pure + fully injectable (the local host,
/// the local boot id, our pid and the liveness answer are all parameters), so every branch —
/// reboot, PID reuse, a same-identity shared workspace with a coherent lock
/// manager, unreadable boot id — is decidable in a test.
///
/// Decision order — **host, then boot, then PID, then age**:
///
/// 1. **It is us** (our live pid, same host, no boot conflict) → [`OwnerLiveness::Live`].
/// 2. **Another host** (within the supported same-identity/coherent-lock
///    deployment boundary): its process table is unreachable and
///    its boot id is unrelated to ours, so NEITHER probe means anything →
///    [`OwnerLiveness::AgeOnly`]. This is the ordering that matters: a boot check placed
///    ahead of the host check reclaims a *live* remote holder's lock on every acquire (machine
///    B never has machine A's boot id), putting two writers on one tree.
/// 3. **No usable pid** → [`OwnerLiveness::AgeOnly`].
/// 4. **Same host, pid probe** — the primary and STRONGEST signal:
///    - dead → [`OwnerLiveness::Abandoned`] (the ordinary crash path, and the ordinary
///      post-reboot path: a reboot kills the holder, so its pid is normally gone);
///    - alive **and** no boot conflict → [`OwnerLiveness::Live`];
///    - alive **but** the recorded boot id differs → [`OwnerLiveness::AgeOnly`]. The mismatch
///      has TWO readings we cannot tell apart: a reboot recycled the pid onto an unrelated
///      live process, **or** the boot id itself moved under a still-live owner (macOS
///      recomputes `kern.boottime` on every clock correction, and an unreadable id reads as
///      empty). Reclaiming on a boot STRING alone would delete a live holder's claim — the
///      single-writer invariant, gone. The age window still frees a genuinely rebooted owner;
///      it just never yanks a live one.
///    - unprobeable → [`OwnerLiveness::AgeOnly`] (conservative: an errored probe is not proof
///      of death; a boot mismatch is not proof either).
///
/// Empty = **unknown**, and unknown reads as "matches" on both host and boot: a claim written
/// by an older build, or on a machine whose boot id / hostname cannot be read (`sysctl`
/// missing; `wmic` is gone in current Windows), must keep behaving exactly as it did — never
/// as "a different boot ⇒ dead", which would reclaim a LIVE local lock.
pub(crate) fn classify_claim_owner(
    owner: ClaimOwner<'_>,
    local_host: &str,
    local_boot: &str,
    self_pid: u32,
    alive: Option<bool>,
) -> OwnerLiveness {
    let same_host = owner.host.is_empty() || local_host.is_empty() || owner.host == local_host;
    let boot_conflict =
        !owner.boot.is_empty() && !local_boot.is_empty() && owner.boot != local_boot;

    // 1. Ours, right now.
    if owner.pid != 0 && owner.pid == self_pid && same_host && !boot_conflict {
        return OwnerLiveness::Live;
    }
    // 2. Another machine — unprobeable by construction.
    if !same_host {
        return OwnerLiveness::AgeOnly;
    }
    // 3. Nothing to probe.
    if owner.pid == 0 {
        return OwnerLiveness::AgeOnly;
    }
    // 4. This host: the pid probe decides, and a boot mismatch may only DOWNGRADE a live
    //    answer to the age window — never upgrade it to "reclaim".
    match alive {
        Some(false) => OwnerLiveness::Abandoned,
        Some(true) if !boot_conflict => OwnerLiveness::Live,
        Some(true) | None => OwnerLiveness::AgeOnly,
    }
}

/// A best-effort identifier that CHANGES on every reboot (whitespace-stripped to a single
/// token). Linux: `/proc/sys/kernel/random/boot_id`. macOS: the `kern.boottime` sysctl. Empty
/// when neither is available (reboot-detection then simply doesn't apply). Lets an abandoned
/// pre-reboot lock be reclaimed even when its PID was recycled by a live process.
///
/// `pub(crate)` because the temp-rewind crash marker
/// ([`crate::checkpoint::TempRewindMarker`]) needs the SAME reboot rule: after a reboot a
/// PID is meaningless, so an owner-liveness probe against a recycled PID must never be
/// allowed to keep a dead owner's claim alive.
pub(crate) fn boot_id() -> String {
    static BOOT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT_ID.get_or_init(detect_boot_id).clone()
}

fn detect_boot_id() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(id) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        return id.split_whitespace().collect();
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.boottime"])
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .collect();
        }
    }
    #[cfg(windows)]
    if let Some(id) = windows_boot_id() {
        return id;
    }
    String::new()
}

#[cfg(windows)]
fn windows_boot_id() -> Option<String> {
    let wmic = umadev_process::windows_system_command_stdout(
        std::path::Path::new("wbem/WMIC.exe"),
        &["os", "get", "lastbootuptime", "/value"],
        std::time::Duration::from_secs(2),
        4 * 1024,
    );
    if let Some(value) = wmic.as_deref().and_then(windows_dmtf_boot_id) {
        return Some(value);
    }

    let powershell = umadev_process::windows_system_command_stdout(
        std::path::Path::new("WindowsPowerShell/v1.0/powershell.exe"),
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$boot=(Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop).LastBootUpTime; [System.Management.ManagementDateTimeConverter]::ToDmtfDateTime($boot)",
        ],
        std::time::Duration::from_secs(5),
        4 * 1024,
    )?;
    windows_dmtf_boot_id(&powershell)
}

#[cfg(any(windows, test))]
pub(super) fn windows_dmtf_boot_id(output: &[u8]) -> Option<String> {
    let text = if output.contains(&0) || output.starts_with(&[0xff, 0xfe]) {
        let bytes = output.strip_prefix(&[0xff, 0xfe]).unwrap_or(output);
        if !bytes.len().is_multiple_of(2) {
            return None;
        }
        let units = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&units).ok()?
    } else {
        std::str::from_utf8(output).ok()?.to_string()
    };
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(|line| {
            let line = line.trim();
            line.strip_prefix("LastBootUpTime=").unwrap_or(line)
        })
        .find(|value| valid_windows_dmtf_boot_id(value))
        .map(str::to_string)
}

#[cfg(any(windows, test))]
pub(super) fn valid_windows_dmtf_boot_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 25
        && bytes[..14].iter().all(u8::is_ascii_digit)
        && bytes[14] == b'.'
        && bytes[15..21].iter().all(u8::is_ascii_digit)
        && matches!(bytes[21], b'+' | b'-')
        && bytes[22..].iter().all(u8::is_ascii_digit)
}

/// `true` when the lock at `path` belongs to a crashed/abandoned run and may be
/// reclaimed. The owner verdict is the SHARED rule ([`classify_claim_owner`] —
/// host → boot → PID → age), so the run lock and the temporary-rewind marker can
/// never disagree about whether the same process is alive:
///
/// 1. Same host + dead PID → stale (reclaim).
/// 2. Same host + live PID → NOT stale (a real concurrent run) — and no boot-id
///    string may override that: a live holder's lock is never reclaimed out from
///    under it, because two writers on one tree is the failure this lock exists
///    to prevent.
/// 3. Cross-host / unattributable / boot-conflicted owner → stale only if older
///    than [`STALE_SECS`] (measured from the owner's own recorded `ts`).
///
/// Fail-open at every branch: an unreadable file is treated as stale (the holder
/// can no longer be identified, so it can't be live), and a PID we cannot probe
/// falls to the age window rather than being reclaimed.
#[cfg(test)]
pub(super) fn is_stale(path: &Path) -> bool {
    // Can't read it → owner is unidentifiable → safe to reclaim.
    let Ok(bytes) = umadev_state::fs::read_bounded(path, 4 * 1024) else {
        return true;
    };
    let contents = String::from_utf8_lossy(&bytes);
    // Corrupt / legacy-without-fields → no parseable owner at all, so there is
    // no recorded `ts` heartbeat. Force the mtime fallback with a ts==0 owner.
    let Some(owner) = Owner::parse(&contents) else {
        return older_than_stale(&Owner::default(), path);
    };

    // Never probe pid 0 (on Unix `kill -0 0` signals the whole process GROUP and
    // would read back as "alive"); an owner with no pid is decided by age.
    let alive = if owner.pid == 0 {
        None
    } else {
        pid_is_alive(owner.pid)
    };
    match classify_claim_owner(
        ClaimOwner {
            pid: owner.pid,
            host: &owner.host,
            boot: &owner.boot,
        },
        &hostname(),
        &boot_id(),
        std::process::id(),
        alive,
    ) {
        OwnerLiveness::Live => false,
        OwnerLiveness::Abandoned => true,
        // Age from the OWNER's recorded `ts` (the lock's own heartbeat), NOT the
        // file's mtime.
        OwnerLiveness::AgeOnly => older_than_stale(&owner, path),
    }
}

/// `true` when the lock is older than [`STALE_SECS`].
///
/// P0-5/P1-4: age is measured from the OWNER's recorded `ts` (UNIX seconds
/// written into the lock at creation) against the current wall clock — NOT the
/// file's mtime. mtime is unreliable across hosts and on coherently-locked network
/// filesystems (clock skew, `noatime`
/// /relatime quirks, a `touch`/rsync bumping it), and `Owner.ts` was already
/// parsed but went unused. Only when the owner has no usable timestamp (`ts == 0`:
/// a legacy/corrupt line) do we fall back to the file mtime as a last resort. An
/// unstattable file on that fallback can't be a live heartbeat → treat as stale.
pub(super) fn older_than_stale(owner: &Owner, path: &Path) -> bool {
    if owner.ts != 0 {
        // now - ts > STALE_SECS  (saturating: a future-dated ts → age 0 → not stale).
        return now_secs().saturating_sub(owner.ts) > STALE_SECS;
    }
    // No recorded heartbeat — last-resort mtime fallback.
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
    pub(super) fn parse(s: &str) -> Option<Owner> {
        let line = s.lines().next().unwrap_or("");
        let mut pid: Option<u32> = None;
        let mut host = String::new();
        let mut ts: u64 = 0;
        let mut boot = String::new();
        let mut nonce = String::new();
        let mut protocol = 0;
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("pid=") {
                pid = v.parse().ok();
            } else if let Some(v) = tok.strip_prefix("host=") {
                host = v.to_string();
            } else if let Some(v) = tok.strip_prefix("ts=") {
                ts = v.parse().unwrap_or(0);
            } else if let Some(v) = tok.strip_prefix("boot=") {
                boot = v.to_string();
            } else if let Some(v) = tok.strip_prefix("nonce=") {
                nonce = v.to_string();
            } else if let Some(v) = tok.strip_prefix("protocol=") {
                protocol = v.parse().unwrap_or(0);
            }
        }
        pid.map(|pid| Owner {
            pid,
            host,
            ts,
            boot,
            nonce,
            protocol,
        })
    }
}

/// Best-effort hostname, dependency-free. Reads the usual env vars and, on Unix,
/// falls back to `/etc/hostname`-equivalent via `uname -n`. An empty string when
/// nothing is available — callers treat empty as "host unknown" (no same-host
/// PID probe, age fallback only), which is the safe direction.
///
/// `pub(crate)`: the temp-rewind crash marker records the same owner identity, so a
/// marker written on another machine in a supported shared workspace is never judged by
/// THIS host's process table.
pub(crate) fn hostname() -> String {
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
pub(crate) fn pid_is_alive(pid: u32) -> Option<bool> {
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
pub(crate) fn pid_is_alive(pid: u32) -> Option<bool> {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .ok()?;
    // Classify by what tasklist SAID, not by how it exited. "No tasks are running
    // which match the specified criteria" — the answer we care about most, a dead
    // pid — is printed on stdout while the exit status is NON-ZERO on the Windows
    // builds that matter. Bailing on the status first turned every dead pid into
    // "unknown", so a crashed owner fell through to the age window: a stranded work
    // tree stayed stranded for the whole rewind window, and a dead run lock held the
    // tree for hours. The Unix arm has always classified by the message; this one
    // now does too.
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_lowercase();
    if said.contains("no tasks") {
        return Some(false);
    }
    // A pid tasklist itself rejects as unusable cannot name a running process.
    if said.contains("invalid") || said.contains("illegal") {
        return Some(false);
    }
    // A real match is a CSV row carrying the pid as its own quoted field. Match that
    // exactly — a bare substring search hits the pid inside an image name, a memory
    // figure, or a session id, and reports a dead pid as alive (which is the one
    // mistake that must never happen: it makes a stale lock permanent).
    if said.contains(&format!("\"{pid}\"")) {
        return Some(true);
    }
    // Understood nothing. Say so — the caller falls back to the age window rather
    // than guessing in either direction.
    if out.status.success() {
        Some(false)
    } else {
        None
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn pid_is_alive(_pid: u32) -> Option<bool> {
    None
}

pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
