//! Platform process-lifetime primitives kept behind one narrow safe API.
//!
//! Host and TUI crates forbid unsafe code. Windows Job Objects require FFI, so
//! this crate owns that seam just as `umadev-agent::spawn_util` owns `setsid`.

#![deny(unsafe_code)]

/// A Windows Job Object configured to kill every assigned descendant when its
/// final handle closes. The native Grok process is attached before any protocol
/// traffic, eliminating wrapper/child cleanup races and executable file locks.
#[cfg(windows)]
pub struct KillOnCloseJob {
    handle: usize,
}

// Keep the cross-thread contract explicit. The handle is stored as a
// pointer-sized integer so the safe wrapper can move with an async session;
// Win32 Job Object handles themselves are valid from any process thread.
#[cfg(windows)]
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<KillOnCloseJob>();
};

#[cfg(windows)]
impl KillOnCloseJob {
    /// Create, configure, and attach a kill-on-close Job Object.
    ///
    /// Returns `None` when Windows rejects nested job assignment; callers retain
    /// their explicit process-tree fallback in that case.
    #[allow(unsafe_code)]
    pub fn attach(child: &tokio::process::Child) -> Option<Self> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let process = child.raw_handle()? as windows_sys::Win32::Foundation::HANDLE;
        // SAFETY: null optional inputs and the information pointer match the
        // synchronous Win32 signatures and remain valid throughout each call.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return None;
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0;
            let assigned = configured && AssignProcessToJobObject(handle, process) != 0;
            if !assigned {
                windows_sys::Win32::Foundation::CloseHandle(handle);
                return None;
            }
            Some(Self {
                handle: handle as usize,
            })
        }
    }

    /// Force every process in the job to terminate without releasing ownership.
    #[allow(unsafe_code)]
    pub fn terminate(&self) {
        // SAFETY: the owned handle is valid until Drop closes it.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(
                self.handle as windows_sys::Win32::Foundation::HANDLE,
                1,
            );
        }
    }
}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: this is the one close of the owned Job Object handle.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(
                self.handle as windows_sys::Win32::Foundation::HANDLE,
            );
        }
    }
}

/// Whether this target has native kill-on-close Job Object support.
#[must_use]
pub const fn has_kill_on_close_job() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use std::path::{Path, PathBuf};
    #[cfg(windows)]
    use std::process::Stdio;
    #[cfg(windows)]
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn job_support_matches_target() {
        assert_eq!(super::has_kill_on_close_job(), cfg!(windows));
    }

    #[cfg(windows)]
    const FIXTURE_ROLE_ENV: &str = "UMADEV_PROCESS_JOB_FIXTURE_ROLE";
    #[cfg(windows)]
    const FIXTURE_READY_ENV: &str = "UMADEV_PROCESS_JOB_FIXTURE_READY";
    #[cfg(windows)]
    const FIXTURE_GO_ENV: &str = "UMADEV_PROCESS_JOB_FIXTURE_GO";
    #[cfg(windows)]
    const FIXTURE_LEAF_PID_ENV: &str = "UMADEV_PROCESS_JOB_FIXTURE_LEAF_PID";
    #[cfg(windows)]
    const FIXTURE_TEST_NAME: &str = "tests::job_tree_fixture";

    /// Child entrypoint used by [`kill_on_close_job_terminates_the_whole_tree`].
    ///
    /// The root waits for a file gate before spawning its leaf. The parent test
    /// attaches the root to the Job Object before opening that gate, removing the
    /// usual spawn-to-attach race from this contract test.
    #[cfg(windows)]
    #[test]
    fn job_tree_fixture() {
        let Some(role) = std::env::var_os(FIXTURE_ROLE_ENV) else {
            return;
        };
        match role.to_string_lossy().as_ref() {
            "root" => run_fixture_root(),
            "leaf" => std::thread::sleep(Duration::from_secs(30)),
            other => panic!("unknown Job Object fixture role: {other}"),
        }
    }

    #[cfg(windows)]
    fn run_fixture_root() {
        let ready = fixture_path(FIXTURE_READY_ENV);
        let go = fixture_path(FIXTURE_GO_ENV);
        let leaf_pid = fixture_path(FIXTURE_LEAF_PID_ENV);
        std::fs::write(&ready, b"ready").expect("publish root readiness");
        wait_for_path_sync(&go, Duration::from_secs(10));

        let executable = std::env::current_exe().expect("resolve fixture executable");
        let mut leaf = std::process::Command::new(executable)
            .args([
                "--exact",
                FIXTURE_TEST_NAME,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FIXTURE_ROLE_ENV, "leaf")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Job Object leaf fixture");
        std::fs::write(&leaf_pid, leaf.id().to_string()).expect("publish leaf pid");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = leaf.try_wait().expect("poll leaf fixture") {
                panic!("leaf fixture exited before Job teardown: {status}");
            }
            if Instant::now() >= deadline {
                let _ = leaf.kill();
                let _ = leaf.wait();
                panic!("parent did not close the Job Object within 30 seconds");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn kill_on_close_job_terminates_the_whole_tree() {
        let fixture_dir = FixtureDir::new();
        let ready = fixture_dir.0.join("ready");
        let go = fixture_dir.0.join("go");
        let leaf_pid_path = fixture_dir.0.join("leaf-pid");
        let executable = std::env::current_exe().expect("resolve test executable");
        let mut root = tokio::process::Command::new(executable)
            .args([
                "--exact",
                FIXTURE_TEST_NAME,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FIXTURE_ROLE_ENV, "root")
            .env(FIXTURE_READY_ENV, &ready)
            .env(FIXTURE_GO_ENV, &go)
            .env(FIXTURE_LEAF_PID_ENV, &leaf_pid_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn Job Object root fixture");

        wait_for_path(&ready, Duration::from_secs(5)).await;
        let job = super::KillOnCloseJob::attach(&root)
            .expect("GitHub Windows runner must permit Job Object attachment");
        std::fs::write(&go, b"go").expect("release root fixture spawn gate");
        wait_for_path(&leaf_pid_path, Duration::from_secs(5)).await;
        let leaf_pid = std::fs::read_to_string(&leaf_pid_path)
            .expect("read leaf pid")
            .trim()
            .parse::<u32>()
            .expect("parse leaf pid");
        let leaf = ProcessWaitHandle::open(leaf_pid).expect("open live leaf process");

        assert!(
            root.try_wait()
                .expect("poll root fixture before Job teardown")
                .is_none(),
            "root fixture exited before Job teardown"
        );
        // Closing the final Job handle is the behavior used by session teardown.
        drop(job);
        assert_eq!(
            leaf.wait(Duration::from_secs(5)),
            windows_sys::Win32::Foundation::WAIT_OBJECT_0,
            "Job close did not terminate the descendant process"
        );
        tokio::time::timeout(Duration::from_secs(5), root.wait())
            .await
            .expect("Job close did not terminate the root process in time")
            .expect("wait for root fixture");
    }

    #[cfg(windows)]
    async fn wait_for_path(path: &Path, budget: Duration) {
        let deadline = tokio::time::Instant::now() + budget;
        while !path.is_file() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for fixture marker {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(windows)]
    fn wait_for_path_sync(path: &Path, budget: Duration) {
        let deadline = Instant::now() + budget;
        while !path.is_file() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for fixture marker {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    fn fixture_path(key: &str) -> PathBuf {
        std::env::var_os(key)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("missing fixture environment variable {key}"))
    }

    #[cfg(windows)]
    struct FixtureDir(PathBuf);

    #[cfg(windows)]
    impl FixtureDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("umadev-process-job-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create Job Object fixture directory");
            Self(path)
        }
    }

    #[cfg(windows)]
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(windows)]
    struct ProcessWaitHandle(usize);

    #[cfg(windows)]
    impl ProcessWaitHandle {
        #[allow(unsafe_code)]
        fn open(pid: u32) -> std::io::Result<Self> {
            // SAFETY: OpenProcess returns a new owned handle or null. The owned
            // handle is closed exactly once by this test helper's Drop.
            let handle = unsafe {
                windows_sys::Win32::System::Threading::OpenProcess(
                    windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE,
                    0,
                    pid,
                )
            };
            if handle.is_null() {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(Self(handle as usize))
            }
        }

        #[allow(unsafe_code)]
        fn wait(&self, budget: Duration) -> u32 {
            let millis = u32::try_from(budget.as_millis()).unwrap_or(u32::MAX - 1);
            // SAFETY: the handle remains owned by `self` for this synchronous wait.
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(
                    self.0 as windows_sys::Win32::Foundation::HANDLE,
                    millis,
                )
            }
        }
    }

    #[cfg(windows)]
    impl Drop for ProcessWaitHandle {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: this is the one close of the handle returned by OpenProcess.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(
                    self.0 as windows_sys::Win32::Foundation::HANDLE,
                );
            }
        }
    }
}
