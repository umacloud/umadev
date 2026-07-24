use super::*;

/// Write a raw lock file with the given contents.
fn write_lock(root: &Path, contents: &str) {
    let dir = root.join(".umadev");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("run.lock"), contents).unwrap();
}

fn owner_path(root: &Path) -> PathBuf {
    root.join(".umadev").join("run.owner")
}

fn install_v2_fence(root: &Path) {
    let dir = root.join(".umadev");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("run.lock"), V2_FENCE).unwrap();
}

fn write_owner_record(root: &Path, contents: &str) {
    install_v2_fence(root);
    std::fs::write(owner_path(root), contents).unwrap();
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
fn run_intent_reclaims_our_own_residual_lock_instead_of_would_block() {
    // THE REGRESSION: research wedged at `0/9`. A real execution block uses
    // `acquire_for_run`. When OUR OWN previous block left a same-PID lock
    // behind (its guard not yet dropped, or it aborted before Drop), the run
    // path must RECLAIM it and take over — never the `WouldBlock` queue signal
    // the routing layer wants, which the `?` would have propagated and ended
    // the run task with zero phases done.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    // Simulate v2 owner metadata left after a crash. The permanent
    // compatibility fence is separate from the real owner row.
    write_owner_record(
        root,
        &format!(
            "pid={} host={} ts={} nonce=residue",
            std::process::id(),
            hostname(),
            now_secs()
        ),
    );
    let path = owner_path(root);
    assert!(
        holder_is_self(&path),
        "fixture must be our own residual lock"
    );
    // Metadata alone is no longer ownership. The kernel guard is free, so
    // routing can safely take over a same-PID crash residue.
    let routed = RunLock::acquire(root).expect("route reclaims unlocked residue");
    assert!(routed.is_owned());
    drop(routed);
    // Re-establish the residue, then the EXECUTION intent also takes over.
    write_owner_record(
        root,
        &format!(
            "pid={} host={} ts={} nonce=residue-2",
            std::process::id(),
            hostname(),
            now_secs()
        ),
    );
    let lock = RunLock::acquire_for_run(root).expect("run intent reclaims our residue");
    assert!(lock.owned, "reclaimed lock is now owned by us");
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains(&format!("pid={}", std::process::id())),
        "the reclaimed lock records our identity"
    );
}

// A live FOREIGN holder must still be refused even under the run-execution
// intent — `acquire_for_run` only relaxes the SAME-PID case, never an
// external live run. Modelled with PID 1 (init/launchd): a Unix concept.
#[cfg(unix)]
#[test]
fn run_intent_still_refuses_a_live_foreign_holder() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    write_lock(
        root,
        &format!("pid=1 host={} ts={}", hostname(), now_secs()),
    );
    let err = RunLock::acquire_for_run(root)
        .expect_err("a live foreign run is refused even for execution");
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert!(
        err.to_string().contains("旧版"),
        "a legacy owner is never overwritten during one-way migration"
    );
}

#[test]
fn run_intent_never_guesses_that_a_legacy_dead_owner_is_safe_to_replace() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    write_lock(
        root,
        &format!("pid={DEAD_PID} host={} ts={}", hostname(), now_secs()),
    );
    let error = RunLock::acquire_for_run(root)
        .expect_err("legacy stale recovery cannot race an older create_new writer");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(error.to_string().contains("doctor"));
}

// Models a live foreign holder with PID 1 (init/launchd) — a Unix concept,
// so the whole test is unix-only (on Windows `root` would be unused).
#[cfg(unix)]
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
            err.to_string().contains("旧版"),
            "legacy ownership is surfaced instead of overwritten"
        );
    }
}

#[test]
fn stale_legacy_lock_is_classified_but_never_race_reclaimed() {
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
    let original = std::fs::read(&path).unwrap();
    let error = RunLock::acquire(root).expect_err("legacy file is fail-closed");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "migration never rewrites or removes a legacy lock"
    );
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
        err.to_string().contains("doctor"),
        "refusal must explain the explicit one-time migration"
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
    assert!(err.to_string().contains("doctor"));
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
    let fence = Owner::parse(std::str::from_utf8(V2_FENCE).unwrap()).expect("v2 fence");
    assert_eq!(fence.protocol, 2);
    assert_eq!(fence.ts, u64::MAX);
}

#[test]
fn staleness_uses_owner_ts_not_mtime() {
    // P0-5/P1-4: a cross-host lock (can't probe its PID) whose recorded
    // `ts` is ancient must be reclaimable EVEN THOUGH the file mtime is
    // brand-new (we just wrote it). This proves age comes from owner.ts,
    // not the file's mtime. Use a foreign host so the PID-probe branch is
    // skipped and only the age path decides.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let ancient = now_secs().saturating_sub(STALE_SECS + 60);
    write_lock(
        root,
        &format!("pid=12345 host=some-other-host ts={ancient}"),
    );
    let path = root.join(".umadev").join("run.lock");
    assert!(
        is_stale(&path),
        "an ancient owner.ts must be stale despite a fresh file mtime"
    );

    // Inverse: a FRESH ts on a foreign host is NOT stale even if the file
    // is touched/old — the owner heartbeat is recent.
    let fresh = now_secs();
    write_lock(root, &format!("pid=12345 host=some-other-host ts={fresh}"));
    assert!(
        !is_stale(&path),
        "a fresh owner.ts must not be reclaimable on the age path"
    );
}

/// THE SINGLE-WRITER BLOCKER: a boot-id mismatch must NEVER reclaim a LIVE lock.
///
/// The rule used to test the boot id BEFORE the host — so `owner.boot != boot_id()`
/// reclaimed the lock outright, and the same-host check below it was unreachable. Three
/// ways that deletes a live holder's lock and puts two writers on one tree:
///
/// 1. A shared / network workspace: machine B NEVER has machine A's boot id, so B
///    reclaimed A's live lock on every single acquire.
/// 2. Our own `boot_id()` returns "" when every platform provider is unavailable
///    → "" ≠ the recorded boot → a LIVE LOCAL lock
///    reclaimed.
/// 3. macOS recomputes `kern.boottime` on every clock correction, so the boot string can
///    change WITHIN one boot, under a live holder.
///
/// Decided by the shared rule ([`classify_claim_owner`]) — host, then boot, then pid,
/// then age — so the run lock and the temp-rewind marker can never disagree about the
/// liveness of the same process.
#[test]
fn a_boot_id_mismatch_never_reclaims_a_live_lock() {
    let me = std::process::id();
    let live_local = ClaimOwner {
        pid: me,
        host: "our-host",
        boot: "our-boot",
    };
    // (a) A DIFFERENT boot string over a live same-host owner (macOS clock correction,
    //     a re-read that drifted): the pid probe says ALIVE — that answer stands.
    let other_pid = ClaimOwner {
        pid: 4321,
        host: "our-host",
        boot: "a-DIFFERENT-boot-string",
    };
    assert_eq!(
        classify_claim_owner(other_pid, "our-host", "our-boot", me, Some(true)),
        OwnerLiveness::AgeOnly,
        "a live same-host owner under a different boot string is NEVER reclaimed on the \
             spot — the boot string is not trustworthy enough to kill a live claim"
    );
    // (b) An EMPTY LOCAL boot id (the Windows-11 `wmic` case): unknown ≠ mismatch.
    assert_eq!(
        classify_claim_owner(live_local, "our-host", "", me, Some(true)),
        OwnerLiveness::Live,
        "an unreadable local boot id must not make us reclaim our own live lock"
    );
    // (c) An EMPTY RECORDED boot (a legacy lock line): likewise.
    let legacy = ClaimOwner {
        pid: 4321,
        host: "our-host",
        boot: "",
    };
    assert_eq!(
        classify_claim_owner(legacy, "our-host", "our-boot", me, Some(true)),
        OwnerLiveness::Live,
        "a lock with no recorded boot id is judged by its PID, not by a phantom reboot"
    );
    // A genuinely rebooted same-host owner IS reclaimed: after a reboot its pid is gone.
    let rebooted = ClaimOwner {
        pid: 4321,
        host: "our-host",
        boot: "boot-BEFORE-the-reboot",
    };
    assert_eq!(
        classify_claim_owner(rebooted, "our-host", "boot-AFTER", me, Some(false)),
        OwnerLiveness::Abandoned,
        "a rebooted owner's dead pid is reclaimed at once"
    );
    // …and if the reboot RECYCLED its pid onto a live process, the age window still
    // frees it (bounded), so a workspace can never wedge forever.
    assert_eq!(
        classify_claim_owner(rebooted, "our-host", "boot-AFTER", me, Some(true)),
        OwnerLiveness::AgeOnly,
        "a recycled pid falls to the age window — freed, but never by yanking a live claim"
    );
    // A LIVE owner on ANOTHER host (a shared / NFS workspace) is respected: its process
    // table is unreachable and its boot id is unrelated to ours, so age is all we have.
    let remote = ClaimOwner {
        pid: 4321,
        host: "their-host",
        boot: "their-boot",
    };
    assert_eq!(
        classify_claim_owner(remote, "our-host", "our-boot", me, Some(false)),
        OwnerLiveness::AgeOnly,
        "another host's lock is decided by AGE — never by our own process table, and never \
             by a boot id that means nothing across machines"
    );
}

#[test]
fn windows_boot_provider_outputs_share_one_strict_dmtf_identity() {
    let expected = "20260724010203.123456+480";
    assert_eq!(
        owner::windows_dmtf_boot_id(format!("LastBootUpTime={expected}\r\n").as_bytes()).as_deref(),
        Some(expected)
    );
    assert_eq!(
        owner::windows_dmtf_boot_id(format!("{expected}\r\n").as_bytes()).as_deref(),
        Some(expected)
    );

    let mut utf16 = vec![0xff, 0xfe];
    for unit in expected.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    assert_eq!(
        owner::windows_dmtf_boot_id(&utf16).as_deref(),
        Some(expected)
    );
    for invalid in [
        b"warning only".as_slice(),
        b"20260724010203.123456Z480",
        b"20260724010203.12345+480",
        b"20260724010203.123456+48x",
        b"2026\xff0724010203.123456+480",
        b"\xff\xfe2",
    ] {
        assert!(owner::windows_dmtf_boot_id(invalid).is_none());
    }
}

/// The same blocker, end-to-end through the real lock file: an ALIVE local owner
/// (pid 1 = init/launchd) whose recorded boot id differs from ours must not be stale.
#[cfg(unix)]
#[test]
fn a_live_local_lock_with_a_foreign_boot_id_is_not_reclaimable() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    write_lock(
        root,
        &format!(
            "pid=1 host={} ts={} boot=not-the-boot-id-we-have-now",
            hostname(),
            now_secs()
        ),
    );
    let path = root.join(".umadev").join("run.lock");
    assert!(
        !is_stale(&path),
        "a LIVE holder's lock must survive a boot-id mismatch — reclaiming it is two \
             writers on one tree"
    );
    let err = RunLock::acquire(root).expect_err("the live holder still owns the workspace");
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

    // The same lock, but ANCIENT: the age window is the honest way out, and it still works.
    write_lock(
        root,
        &format!(
            "pid=1 host={} ts={} boot=not-the-boot-id-we-have-now",
            hostname(),
            now_secs().saturating_sub(STALE_SECS + 60)
        ),
    );
    assert!(
        is_stale(&path),
        "past the age window even a boot-conflicted live pid is reclaimable"
    );
}

#[test]
fn staleness_falls_back_to_mtime_when_ts_is_zero() {
    // A legacy/corrupt owner with ts=0 has no heartbeat → mtime fallback.
    // A freshly-written file is young, so it is NOT age-stale.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    write_lock(root, "pid=12345 host=some-other-host ts=0");
    let path = root.join(".umadev").join("run.lock");
    assert!(
        !is_stale(&path),
        "ts=0 + fresh mtime → not age-stale (mtime fallback)"
    );
}

#[test]
fn reclaim_read_back_surrenders_when_lock_clobbered() {
    // P0-2: prove the read-back self-check. Simulate a racing reclaimer
    // that overwrote our just-created lock with a FOREIGN owner before we
    // read it back. `holder_is_self` then returns false → we must surrender
    // ownership (owned:false) and NOT delete the foreign lock.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let path = root.join(".umadev").join("run.lock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A foreign owner already in the file (as if a racer clobbered us).
    std::fs::write(&path, "pid=999999 host=racer ts=1\n").unwrap();
    // The read-back self-check must classify this as NOT us.
    assert!(
        !holder_is_self(&path),
        "a foreign-owner lock must not be attributed to us on read-back"
    );
    // And the foreign lock must remain untouched (fail-open: never delete
    // someone else's lock on the surrender path).
    assert!(path.exists(), "we must not delete a foreign lock");
}

#[test]
fn reclaim_read_back_confirms_self_on_clean_acquire() {
    // The happy path: a clean acquire writes our identity and the read-back
    // confirms it, so we own the lock end-to-end.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let lock = RunLock::acquire_for_run(root).expect("clean acquire");
    assert!(lock.owned, "a clean acquire owns the lock after read-back");
    let path = owner_path(root);
    assert!(holder_is_self(&path), "read-back confirms our identity");
    assert_eq!(
        std::fs::read(root.join(".umadev/run.lock")).unwrap(),
        V2_FENCE
    );
}

#[test]
fn kernel_guard_allows_only_one_stale_reclaimer_at_a_time() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();

    let first = RunLock::acquire_for_run(root).expect("first reclaimer");
    assert!(first.is_owned());
    let second = RunLock::acquire_for_run(root)
        .expect_err("a second writer cannot reclaim while the kernel guard is held");
    assert!(
        matches!(
            second.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::AlreadyExists
        ),
        "{second}"
    );

    drop(first);
    let successor = RunLock::acquire_for_run(root).expect("released kernel guard");
    assert!(successor.is_owned());
}

#[cfg(unix)]
#[test]
fn external_namespace_guard_survives_managed_lock_path_replacement() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let first = RunLock::acquire_for_run(root).expect("first writer");
    let managed = root.join(".umadev");
    let moved = root.join(".umadev.old");
    std::fs::rename(&managed, &moved).unwrap();
    std::fs::create_dir(&managed).unwrap();

    let second = RunLock::acquire_for_run(root)
        .expect_err("replacing .umadev must not create a second writer");
    assert!(
        matches!(
            second.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::AlreadyExists
        ),
        "{second}"
    );

    drop(first);
    let successor = RunLock::acquire_for_run(root).expect("outer guard released");
    assert!(successor.is_owned());
}

#[cfg(windows)]
#[test]
fn windows_blocks_managed_lock_path_replacement_while_the_guard_is_open() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let first = RunLock::acquire_for_run(root).expect("first writer");
    let managed = root.join(".umadev");
    let moved = root.join(".umadev.old");

    let error = std::fs::rename(&managed, &moved)
        .expect_err("Windows must not replace a directory containing an open guard");
    assert!(
        error.kind() == io::ErrorKind::PermissionDenied
            || matches!(error.raw_os_error(), Some(5 | 32)),
        "{error}"
    );
    assert!(managed.is_dir());
    assert!(!moved.exists());
    let second = RunLock::acquire_for_run(root)
        .expect_err("the first writer must remain exclusive after the rejected replacement");
    assert_eq!(second.kind(), io::ErrorKind::WouldBlock, "{second}");

    drop(first);
    umadev_state::fs::retry_transient(|| std::fs::rename(&managed, &moved))
        .expect("the namespace is replaceable after guard release");
    std::fs::create_dir(&managed).unwrap();
    let successor = RunLock::acquire_for_run(root).expect("outer guard released");
    assert!(successor.is_owned());
}

#[test]
fn external_namespace_guard_survives_git_clean_style_inner_file_deletion() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let first = RunLock::acquire_for_run(root).expect("first writer");
    std::fs::remove_file(root.join(".umadev/run.lock.guard")).unwrap();
    std::fs::remove_file(root.join(".umadev/run.lock")).unwrap();

    let second = RunLock::acquire_for_run(root)
        .expect_err("deleted ignored lock files must not create a second writer");
    assert!(
        matches!(
            second.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::AlreadyExists
        ),
        "{second}"
    );
    drop(first);
}

/// Child half of [`killed_process_releases_the_native_run_lock_for_a_successor`].
///
/// Invoking the current libtest binary gives this test a genuinely distinct PID
/// and OS handle table. Without the opt-in environment variable it is an inert
/// ordinary unit test, so the normal test suite can still enumerate/run it.
#[test]
fn cross_process_run_lock_holder_child() {
    let Some(root) = std::env::var_os("UMADEV_RUN_LOCK_PROCESS_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let _lock = RunLock::acquire_for_run(&root).expect("child acquires the native run lock");
    std::fs::write(root.join("run-lock-child-ready"), b"ready")
        .expect("child reports that acquisition completed");

    // The parent normally terminates this process. This deadline is only a
    // backstop for a parent that itself disappears, so a failed test never
    // leaves a permanently-running child behind.
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(30) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("run-lock child was not terminated by its parent");
}

/// Owns a spawned test process and guarantees that every panic/early-return
/// path terminates and reaps it. `Child::kill` maps to process termination on
/// Windows and SIGKILL on Unix, which is exactly the crash boundary this test
/// needs to exercise.
struct KillChildOnDrop(Option<std::process::Child>);

impl KillChildOnDrop {
    fn child_mut(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("child is still owned")
    }

    fn terminate_and_wait(&mut self) -> io::Result<std::process::ExitStatus> {
        let mut child = self.0.take().expect("child is still owned");
        if child.try_wait()?.is_none() {
            child.kill()?;
        }
        child.wait()
    }
}

impl Drop for KillChildOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn killed_process_releases_the_native_run_lock_for_a_successor() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let exe = std::env::current_exe().expect("current libtest executable");
    let child = std::process::Command::new(exe)
        .args([
            "--exact",
            "run_lock::tests::cross_process_run_lock_holder_child",
        ])
        .env("UMADEV_RUN_LOCK_PROCESS_ROOT", root)
        .spawn()
        .expect("spawn run-lock holder child");
    let mut child = KillChildOnDrop(Some(child));

    let ready = root.join("run-lock-child-ready");
    let started = std::time::Instant::now();
    loop {
        if ready.exists() {
            break;
        }
        if let Some(status) = child.child_mut().try_wait().expect("poll run-lock child") {
            panic!("run-lock child exited before reporting ready: {status}");
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "run-lock child did not report ready within the bounded startup window"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let contention = RunLock::acquire_for_run(root)
        .expect_err("parent must contend while the child process owns the lock");
    assert_eq!(
        contention.kind(),
        io::ErrorKind::AlreadyExists,
        "a distinct live process is reported as the foreign writer"
    );

    let status = child
        .terminate_and_wait()
        .expect("terminate and reap run-lock holder child");
    assert!(
        !status.success(),
        "the holder must have been terminated rather than releasing normally"
    );

    let successor =
        RunLock::acquire_for_run(root).expect("process death releases the native run lock");
    assert!(
        successor.is_owned(),
        "the successor proves exclusive ownership after the child dies"
    );
}

#[test]
fn native_lock_contention_is_never_misclassified_as_fail_open_io() {
    let native = fs2::lock_contended_error();
    assert!(
        lock_error_is_contention(&native),
        "the platform-native contention error must take the busy path"
    );
    assert!(!lock_error_is_contention(&io::Error::new(
        io::ErrorKind::PermissionDenied,
        "ordinary io failure"
    )));
}

#[test]
fn permanent_v2_fence_blocks_the_legacy_create_new_protocol() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let lock = RunLock::acquire_for_run(root).expect("v2 lock");
    let fence = root.join(".umadev/run.lock");
    assert_eq!(std::fs::read(&fence).unwrap(), V2_FENCE);
    assert!(!holder_is_self(&fence));
    assert!(
        !is_stale(&fence),
        "pid=0 + foreign host + future timestamp is never reclaimable by v1"
    );
    let legacy_attempt = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&fence)
        .expect_err("the old create_new protocol is atomically fenced");
    assert_eq!(legacy_attempt.kind(), io::ErrorKind::AlreadyExists);

    drop(lock);
    assert!(fence.is_file(), "the compatibility fence is permanent");
    assert!(
        !owner_path(root).exists(),
        "only the transient v2 owner row is released"
    );
}

#[test]
fn deleting_the_v2_fence_removes_legacy_client_exclusion() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let lock = RunLock::acquire_for_run(root).expect("v2 lock");
    let fence = root.join(".umadev/run.lock");

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&fence)
        .expect_err("the present fence must block the legacy create-new protocol");

    std::fs::remove_file(&fence).unwrap();
    let legacy_claim = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&fence)
        .expect(
            "without the fence an old client can claim the path while the v2 run is active; \
             this old/new boundary is unsupported",
        );
    drop(legacy_claim);
    std::fs::remove_file(&fence).unwrap();
    drop(lock);
}

#[test]
fn old_guard_drop_never_removes_replaced_owner_metadata() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let lock = RunLock::acquire_for_run(root).expect("lock");
    let path = owner_path(root);
    let replacement = format!(
        "pid=1 host=external ts={} boot=external nonce=successor\n",
        now_secs()
    );
    std::fs::write(&path, &replacement).unwrap();

    drop(lock);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        replacement,
        "nonce mismatch makes the old guard's Drop a no-op"
    );
}

#[cfg(unix)]
#[test]
fn managed_lock_paths_never_follow_symlinks() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().expect("tmp");
    let outside = tempfile::TempDir::new().expect("outside");
    symlink(outside.path(), tmp.path().join(".umadev")).unwrap();
    let linked_dir = RunLock::acquire_for_run(tmp.path()).expect_err("writer must fail closed");
    assert_eq!(linked_dir.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        !outside.path().join("run.lock.guard").exists(),
        "an external target must not receive managed lock files"
    );

    let tmp = tempfile::TempDir::new().expect("tmp");
    let managed = tmp.path().join(".umadev");
    std::fs::create_dir(&managed).unwrap();
    let outside_file = outside.path().join("outside-guard");
    std::fs::write(&outside_file, b"untouched").unwrap();
    symlink(&outside_file, managed.join("run.lock.guard")).unwrap();
    let linked_file = RunLock::acquire_for_run(tmp.path()).expect_err("writer must fail closed");
    assert_eq!(linked_file.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read(&outside_file).unwrap(), b"untouched");
}

#[test]
fn doctor_migrates_only_a_provably_dead_legacy_owner_in_place() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    write_lock(
        root,
        &format!(
            "pid={DEAD_PID} host={} ts={} boot={}",
            hostname(),
            now_secs(),
            boot_id()
        ),
    );
    assert_eq!(
        inspect_fence(root).unwrap(),
        RunLockFenceStatus::LegacyOrIncomplete
    );
    assert_eq!(
        migrate_fence(root).unwrap(),
        RunLockFenceMigration::MigratedLegacy
    );
    assert_eq!(
        std::fs::read(root.join(".umadev/run.lock")).unwrap(),
        V2_FENCE
    );
    assert_eq!(
        migrate_fence(root).unwrap(),
        RunLockFenceMigration::AlreadyCurrent,
        "migration is idempotent"
    );
}

#[test]
fn doctor_never_rewrites_a_live_or_unattributable_legacy_owner() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let live = format!(
        "pid={} host={} ts={} boot={}",
        std::process::id(),
        hostname(),
        now_secs(),
        boot_id()
    );
    write_lock(root, &live);
    let error = migrate_fence(root).expect_err("live legacy owner must survive");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read_to_string(root.join(".umadev/run.lock")).unwrap(),
        live
    );

    std::fs::write(root.join(".umadev/run.lock"), b"").unwrap();
    let error = migrate_fence(root).expect_err("empty owner is not proof of death");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(std::fs::read(root.join(".umadev/run.lock"))
        .unwrap()
        .is_empty());
}

#[test]
fn doctor_never_migrates_a_live_same_host_owner_that_lacks_boot_metadata() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let local_host = hostname();
    assert!(
        !local_host.is_empty(),
        "fixture requires a known local hostname"
    );
    assert_eq!(
        pid_is_alive(std::process::id()),
        Some(true),
        "fixture requires the current process to be provably live"
    );
    let live_legacy = format!("pid={} host={} ts=1", std::process::id(), local_host);
    write_lock(root, &live_legacy);

    let error = migrate_fence(root)
        .expect_err("missing boot metadata must not override a live same-host PID");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read_to_string(root.join(".umadev/run.lock")).unwrap(),
        live_legacy,
        "doctor must preserve the live legacy owner's fence byte-for-byte"
    );
}

#[test]
fn doctor_offline_fix_migrates_old_cross_host_and_boot_conflicted_rows() {
    let remote = tempfile::TempDir::new().expect("tmp");
    write_lock(
        remote.path(),
        &format!("pid={DEAD_PID} host=retired-build-host ts=1 boot=old-boot"),
    );
    assert_eq!(
        migrate_fence(remote.path()).unwrap(),
        RunLockFenceMigration::MigratedLegacy,
        "an explicit offline fix must not permanently wedge a moved workspace"
    );

    let rebooted = tempfile::TempDir::new().expect("tmp");
    let local_boot = boot_id();
    let legacy = format!(
        "pid={} host={} ts=1 boot=definitely-not-this-boot",
        std::process::id(),
        hostname()
    );
    write_lock(rebooted.path(), &legacy);
    if local_boot.is_empty() {
        let error = migrate_fence(rebooted.path())
            .expect_err("an unknown local boot id must keep a live local owner fail-closed");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(rebooted.path().join(".umadev/run.lock")).unwrap(),
            legacy
        );
    } else {
        assert_eq!(
            migrate_fence(rebooted.path()).unwrap(),
            RunLockFenceMigration::MigratedLegacy,
            "an old boot-conflicted row is recoverable under explicit offline authority"
        );
    }

    let host_unknown = tempfile::TempDir::new().expect("tmp");
    write_lock(host_unknown.path(), &format!("pid={DEAD_PID} ts=1"));
    assert_eq!(
        migrate_fence(host_unknown.path()).unwrap(),
        RunLockFenceMigration::MigratedLegacy
    );
}

#[cfg(windows)]
#[test]
fn windows_boot_id_provider_supports_reboot_safe_owner_classification() {
    let first = boot_id();
    assert!(
        !first.is_empty(),
        "Windows must provide a stable boot id through WMIC or PowerShell CIM"
    );
    assert_eq!(boot_id(), first);
}

#[cfg(unix)]
#[test]
fn doctor_offline_fix_recovers_an_expired_hostless_reused_pid() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    write_lock(tmp.path(), "pid=1 ts=1");
    assert_eq!(
        pid_is_alive(1),
        Some(true),
        "fixture requires the long-lived init/launchd PID"
    );
    assert_eq!(
        migrate_fence(tmp.path()).unwrap(),
        RunLockFenceMigration::MigratedLegacy,
        "an old hostless legacy row must not wedge a moved workspace forever"
    );
}

#[test]
fn doctor_completes_a_nonempty_interrupted_v2_fence() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let partial = &V2_FENCE[..V2_FENCE.len() / 2];
    let dir = root.join(".umadev");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("run.lock"), partial).unwrap();

    assert_eq!(
        migrate_fence(root).unwrap(),
        RunLockFenceMigration::RepairedPartial
    );
    assert_eq!(std::fs::read(dir.join("run.lock")).unwrap(), V2_FENCE);
}

#[test]
fn doctor_refuses_to_race_an_active_v2_run() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let root = tmp.path();
    let lock = RunLock::acquire_for_run(root).expect("active writer");
    let error = migrate_fence(root).expect_err("doctor cannot race the active writer");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    drop(lock);
    assert_eq!(
        migrate_fence(root).unwrap(),
        RunLockFenceMigration::AlreadyCurrent
    );
}

#[cfg(unix)]
#[test]
fn doctor_rejects_a_linked_or_non_regular_fence() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().expect("tmp");
    let dir = tmp.path().join(".umadev");
    std::fs::create_dir(&dir).unwrap();
    let outside = tmp.path().join("outside");
    std::fs::write(&outside, b"untouched").unwrap();
    symlink(&outside, dir.join("run.lock")).unwrap();

    assert_eq!(
        inspect_fence(tmp.path()).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        migrate_fence(tmp.path()).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
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
