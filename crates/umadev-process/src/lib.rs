//! Platform-native primitives kept behind narrow safe APIs.
//!
//! Host and TUI crates forbid unsafe code. Windows Job Objects and the native
//! Unicode clipboard and process-tree isolation require FFI, so this crate owns
//! those seams behind reusable lifetime guards.

#![deny(unsafe_code)]

use std::collections::VecDeque;

/// A byte buffer that keeps only the newest `capacity` bytes while callers
/// continue draining the producer to EOF.
///
/// This is the common primitive for subprocess pipes: stopping a read once a
/// display/capture limit is reached can deadlock a child on a full pipe, while
/// an ordinary `Vec` makes a flooding child an out-of-memory risk. `BoundedTail`
/// avoids both failure modes. Its allocation never exceeds the configured
/// capacity, even when one input chunk is itself larger than the capacity.
#[derive(Debug, Clone)]
pub struct BoundedTail {
    bytes: VecDeque<u8>,
    capacity: usize,
    total_seen: usize,
}

impl BoundedTail {
    /// Create an empty tail with a hard byte capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            capacity,
            total_seen: 0,
        }
    }

    /// Append bytes, discarding the oldest bytes as needed to remain bounded.
    pub fn push(&mut self, input: &[u8]) {
        self.total_seen = self.total_seen.saturating_add(input.len());
        if self.capacity == 0 {
            self.bytes.clear();
            return;
        }
        if input.len() >= self.capacity {
            self.bytes.clear();
            self.bytes
                .extend(input[input.len() - self.capacity..].iter().copied());
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(input.len())
            .saturating_sub(self.capacity);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
        self.bytes.extend(input.iter().copied());
    }

    /// Remove all retained bytes and reset the truncation counter.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.total_seen = 0;
    }

    /// Number of bytes currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether no bytes are currently retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Total bytes offered to this tail, including discarded bytes.
    #[must_use]
    pub fn total_seen(&self) -> usize {
        self.total_seen
    }

    /// Whether older bytes were discarded.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.total_seen > self.capacity
    }

    /// Materialize the retained tail in its original byte order.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes.into_iter().collect()
    }
}

/// Resource limits for [`run_bounded_command`].
#[derive(Debug, Clone, Copy)]
pub struct BoundedCommandOptions {
    /// Hard wall-clock deadline for the command.
    pub timeout: std::time::Duration,
    /// Maximum retained stdout bytes. The newest bytes are kept.
    pub stdout_bytes: usize,
    /// Maximum retained stderr bytes. The newest bytes are kept.
    pub stderr_bytes: usize,
    /// Maximum time spent reaping pipe readers after the process tree is
    /// terminated. An overrun aborts the async reader instead of detaching it.
    pub reader_grace: std::time::Duration,
}

impl Default for BoundedCommandOptions {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(30),
            stdout_bytes: 256 * 1024,
            stderr_bytes: 256 * 1024,
            reader_grace: std::time::Duration::from_secs(1),
        }
    }
}

/// Completed output from [`run_bounded_command`].
#[derive(Debug)]
pub struct BoundedCommandOutput {
    /// Direct child's exit status, or `None` when the hard deadline elapsed.
    pub status: Option<std::process::ExitStatus>,
    /// Whether the hard wall-clock deadline elapsed. Partial output tails remain
    /// available in `stdout` and `stderr` when this is true.
    pub timed_out: bool,
    /// Newest retained stdout bytes.
    pub stdout: Vec<u8>,
    /// Newest retained stderr bytes.
    pub stderr: Vec<u8>,
    /// Whether older stdout bytes were discarded.
    pub stdout_truncated: bool,
    /// Whether older stderr bytes were discarded.
    pub stderr_truncated: bool,
}

#[derive(Clone)]
struct SharedTail(std::sync::Arc<std::sync::Mutex<BoundedTail>>);

impl SharedTail {
    fn new(capacity: usize) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(
            BoundedTail::new(capacity),
        )))
    }

    fn push(&self, bytes: &[u8]) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bytes);
    }

    fn snapshot(&self) -> (Vec<u8>, bool) {
        let tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let truncated = tail.truncated();
        (tail.into_bytes(), truncated)
    }
}

struct PipeCapture {
    tail: SharedTail,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl PipeCapture {
    fn spawn<R>(mut reader: R, capacity: usize) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        use tokio::io::AsyncReadExt as _;

        let tail = SharedTail::new(capacity);
        let writer = tail.clone();
        let task = tokio::spawn(async move {
            let mut chunk = [0_u8; 8192];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => writer.push(&chunk[..read]),
                }
            }
        });
        Self {
            tail,
            task: Some(task),
        }
    }

    async fn finish(mut self, grace: std::time::Duration) -> (Vec<u8>, bool) {
        if let Some(mut task) = self.task.take() {
            if tokio::time::timeout(grace, &mut task).await.is_err() {
                task.abort();
                // The reader only awaits Tokio I/O and briefly appends to a
                // bounded in-memory tail, so cancellation is cooperative. Once
                // aborted, join it outright: dropping the handle after a second
                // timeout would detach the task and break the no-reader-leak
                // contract this grace period is meant to enforce.
                let _ = task.await;
            }
        }
        self.tail.snapshot()
    }
}

impl Drop for PipeCapture {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct IsolatedCommandTree {
    #[cfg(unix)]
    pid: u32,
    /// Unix group signaling is safe only while the direct leader is alive or
    /// remains an unreaped zombie. Once that identity anchor is lost, a numeric
    /// PGID may be recycled and must never be signaled again.
    #[cfg(unix)]
    group_signal_safe: bool,
    #[cfg(windows)]
    job: KillOnCloseJob,
    terminated: bool,
}

impl IsolatedCommandTree {
    fn attach(child: &mut tokio::process::Child) -> std::io::Result<Self> {
        #[cfg(unix)]
        let pid = child
            .id()
            .ok_or_else(|| std::io::Error::other("spawned process has no pid"))?;
        #[cfg(windows)]
        let job = KillOnCloseJob::attach(child)?;
        Ok(Self {
            #[cfg(unix)]
            pid,
            #[cfg(unix)]
            group_signal_safe: true,
            #[cfg(windows)]
            job,
            terminated: false,
        })
    }

    #[allow(unsafe_code)]
    fn terminate(&mut self, child: &mut tokio::process::Child) {
        if self.terminated {
            return;
        }
        // Set this before posting the kill so an early return can never make
        // Drop signal a recycled Unix process-group id a second time.
        self.terminated = true;
        #[cfg(unix)]
        {
            if let (true, Ok(pid)) = (self.group_signal_safe, libc::pid_t::try_from(self.pid)) {
                // SAFETY: `pid` is the process-group leader recorded immediately
                // after spawning. `group_signal_safe` remains true only while
                // that leader is alive/unreaped, so its numeric PID/PGID cannot
                // have been recycled. ESRCH is an expected no-op for an empty
                // group whose zombie leader is still the identity anchor.
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
            }
            self.group_signal_safe = false;
        }
        #[cfg(windows)]
        self.job.terminate();
        let _ = child.start_kill();
    }

    /// Permanently give up Unix group signaling when the leader's unreaped
    /// identity can no longer be proven. Direct-child kill-on-drop remains.
    #[cfg(unix)]
    fn abandon_group_identity(&mut self) {
        self.group_signal_safe = false;
    }
}

impl Drop for IsolatedCommandTree {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let (false, true, Ok(pid)) = (
                self.terminated,
                self.group_signal_safe,
                libc::pid_t::try_from(self.pid),
            ) {
                // SAFETY: best-effort backstop for any early return. The group
                // was created for this invocation and remains scoped to its PID.
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
            }
        }
        // On Windows, dropping `job` is itself the kill-on-close backstop.
    }
}

fn isolate_command(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        command
            .as_std_mut()
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
    }
}

/// Isolate a long-running child while also dropping its controlling terminal.
///
/// Unix uses a new session, which is also a dedicated process group. Windows
/// uses the same suspended-process + Job Object handshake as
/// [`isolate_command`]. Unlike the agent crate's historical best-effort
/// detachment helper, Unix session creation fails closed: without the session we
/// could neither guarantee whole-tree cleanup nor prevent descendants from
/// reopening the caller's terminal.
fn isolate_detached_command(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        // SAFETY: `setsid` is async-signal-safe and is the only operation in the
        // post-fork/pre-exec closure. Returning the OS error prevents an
        // unisolated child from escaping the lifetime guard.
        #[allow(unsafe_code)]
        unsafe {
            command.as_std_mut().pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    isolate_command(command);
    #[cfg(not(any(unix, windows)))]
    let _ = command;
}

/// Prepare a synchronous helper for whole-tree ownership. Call
/// [`StdCommandTree::attach`] immediately after spawning it.
pub fn isolate_std_command(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = command;
}

/// Whole-tree lifetime guard for short synchronous helpers.
pub struct StdCommandTree {
    #[cfg(unix)]
    pid: u32,
    /// A numeric Unix PGID may be signaled only while its leader is still our
    /// live child or an unreaped zombie. `waitid(WNOWAIT)` revalidates that
    /// identity immediately before every group signal.
    #[cfg(unix)]
    group_signal_safe: bool,
    terminated: bool,
    #[cfg(windows)]
    job: KillOnCloseJob,
}

impl StdCommandTree {
    /// Attach the just-spawned child. Windows assignment resumes a child that
    /// [`isolate_std_command`] created suspended, closing the wrapper-fork race.
    pub fn attach(child: &mut std::process::Child) -> std::io::Result<Self> {
        #[cfg(unix)]
        let pid = child.id();
        #[cfg(windows)]
        let job = KillOnCloseJob::attach_std(child)?;
        Ok(Self {
            #[cfg(unix)]
            pid,
            #[cfg(unix)]
            group_signal_safe: true,
            terminated: false,
            #[cfg(windows)]
            job,
        })
    }

    /// Terminate the helper and every descendant. Idempotent.
    pub fn terminate(&mut self, child: &mut std::process::Child) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        #[cfg(unix)]
        {
            // Revalidate at the point of use. Callers historically exposed the
            // raw `Child` and could reap it before this guard was dropped; in
            // that state the cached number may already name an unrelated group.
            if self.group_signal_safe && unix_child_exited_unreaped(self.pid).is_err() {
                self.group_signal_safe = false;
            }
            if let (true, Ok(pid)) = (self.group_signal_safe, libc::pid_t::try_from(self.pid)) {
                // SAFETY: successful `waitid(P_PID, ..., WNOWAIT)` immediately
                // above proves this PID is still our child, live or unreaped.
                // Consequently its numeric PID/PGID cannot have been recycled.
                #[allow(unsafe_code)]
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
            }
            self.group_signal_safe = false;
        }
        #[cfg(windows)]
        self.job.terminate();
        let _ = child.kill();
    }

    /// Poll the direct child without losing the process-tree identity anchor.
    ///
    /// A plain [`std::process::Child::try_wait`] reaps an exited Unix leader.
    /// Once reaped, its numeric PID/PGID can be reused and it is no longer safe
    /// to signal the original process group. This method observes exit with
    /// `waitid(WNOWAIT)`, terminates descendants while the leader is still an
    /// unreaped identity anchor, and only then consumes the exit status.
    pub fn try_wait(
        &mut self,
        child: &mut std::process::Child,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        #[cfg(unix)]
        {
            match self.leader_exited_unreaped() {
                Ok(false) => Ok(None),
                Ok(true) => {
                    self.terminate(child);
                    child.wait().map(Some)
                }
                Err(error) => {
                    // Identity can no longer be proven, so group signaling is
                    // permanently disabled. Polling the owned direct handle is
                    // still safe and gives the caller an actionable OS error
                    // only when that operation itself fails.
                    self.group_signal_safe = false;
                    child.try_wait().map_err(|poll_error| {
                        std::io::Error::new(
                            poll_error.kind(),
                            format!(
                                "lost Unix process-group identity ({error}); direct-child poll failed: {poll_error}"
                            ),
                        )
                    })
                }
            }
        }
        #[cfg(windows)]
        {
            match child.try_wait()? {
                Some(status) => {
                    // The Job Object remains an identity-bearing tree handle
                    // even after the direct process exits.
                    self.terminate(child);
                    Ok(Some(status))
                }
                None => Ok(None),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            child.try_wait()
        }
    }

    /// Stop the Unix drop backstop without terminating the group.
    ///
    /// This is only for desktop selection owners such as `wl-copy`/`xclip`:
    /// their descendants must remain alive after the launcher accepts stdin so
    /// another application can request the clipboard contents later.
    #[cfg(unix)]
    pub fn retain_descendants(&mut self) {
        self.terminated = true;
        self.group_signal_safe = false;
    }

    /// Observe whether the Unix leader has exited without consuming its wait
    /// status. Keeping the zombie unreaped preserves the PID/PGID identity until
    /// the caller has terminated every descendant.
    #[cfg(unix)]
    fn leader_exited_unreaped(&mut self) -> std::io::Result<bool> {
        if !self.group_signal_safe {
            return Err(std::io::Error::other(
                "Unix process-group identity is no longer available",
            ));
        }
        match unix_child_exited_unreaped(self.pid) {
            Ok(exited) => Ok(exited),
            Err(error) => {
                self.group_signal_safe = false;
                Err(error)
            }
        }
    }
}

impl Drop for StdCommandTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.terminated && self.group_signal_safe {
            // A raw `Child::wait`/`try_wait` may already have consumed the
            // leader. Never signal the cached number unless WNOWAIT proves it is
            // still our child at this exact point.
            if unix_child_exited_unreaped(self.pid).is_ok() {
                if let Ok(pid) = libc::pid_t::try_from(self.pid) {
                    // SAFETY: the immediately preceding waitid check anchors the
                    // process-group identity against numeric PGID reuse.
                    #[allow(unsafe_code)]
                    unsafe {
                        libc::killpg(pid, libc::SIGKILL);
                    }
                }
            }
        }
        // On Windows, KillOnCloseJob::drop is the corresponding backstop.
    }
}

const STD_COMMAND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

enum ReapChild {
    Std(std::process::Child),
    Tokio(Box<tokio::process::Child>),
}

impl ReapChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self {
            Self::Std(child) => child.try_wait(),
            Self::Tokio(child) => child.try_wait(),
        }
    }
}

struct ChildReaper {
    sender: std::sync::mpsc::Sender<ReapChild>,
    _worker: std::thread::JoinHandle<()>,
}

#[derive(Clone)]
struct ChildReaperInitError {
    kind: std::io::ErrorKind,
    message: String,
}

fn child_reaper() -> std::io::Result<&'static ChildReaper> {
    static REAPER: std::sync::OnceLock<Result<ChildReaper, ChildReaperInitError>> =
        std::sync::OnceLock::new();
    match REAPER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("umadev-child-reaper".to_string())
            .spawn(move || run_child_reaper(receiver))
            .map_err(|error| ChildReaperInitError {
                kind: error.kind(),
                message: error.to_string(),
            })?;
        Ok(ChildReaper {
            sender,
            _worker: worker,
        })
    }) {
        Ok(reaper) => Ok(reaper),
        Err(error) => Err(std::io::Error::new(error.kind, error.message.clone())),
    }
}

fn enqueue_child_reap(child: ReapChild) {
    let Ok(reaper) = child_reaper() else {
        unreachable!("child reaper is initialized before every managed spawn");
    };
    if reaper.sender.send(child).is_err() {
        unreachable!("process-wide child reaper cannot disconnect");
    }
}

fn run_child_reaper(receiver: std::sync::mpsc::Receiver<ReapChild>) {
    let mut children: Vec<ReapChild> = Vec::new();
    loop {
        if children.is_empty() {
            let Ok(child) = receiver.recv() else {
                return;
            };
            children.push(child);
        } else {
            match receiver.recv_timeout(STD_COMMAND_POLL_INTERVAL) {
                Ok(child) => children.push(child),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        children.extend(receiver.try_iter());
        let mut index = 0;
        while index < children.len() {
            let keep = match children[index].try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => true,
                Err(_) => false,
            };
            if keep {
                index += 1;
            } else {
                children.swap_remove(index);
            }
        }
    }
}

struct StdPipeCapture {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    truncated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: std::thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
}

impl StdPipeCapture {
    fn is_truncated(&self) -> bool {
        self.truncated.load(std::sync::atomic::Ordering::Acquire)
    }

    fn finish(self) -> std::io::Result<(Vec<u8>, bool)> {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.task
            .join()
            .map_err(|_| std::io::Error::other("bounded pipe reader panicked"))?
    }
}

#[cfg(unix)]
fn spawn_std_pipe_capture<R>(
    mut reader: R,
    capacity: usize,
    name: &str,
) -> std::io::Result<StdPipeCapture>
where
    R: std::io::Read + std::os::fd::AsRawFd + Send + 'static,
{
    let fd = reader.as_raw_fd();
    // SAFETY: fcntl only reads/updates status flags on the owned pipe fd. Making
    // reads nonblocking is what lets `finish` join rather than detach a reader
    // after the process tree has been terminated.
    #[allow(unsafe_code)]
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same owned fd; existing flags are retained.
    #[allow(unsafe_code)]
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop = std::sync::Arc::clone(&stop);
    let truncated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_truncated = std::sync::Arc::clone(&truncated);
    let task = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut tail = BoundedTail::new(capacity);
            let mut chunk = [0_u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        tail.push(&chunk[..read]);
                        if tail.truncated() {
                            reader_truncated.store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if reader_stop.load(std::sync::atomic::Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(STD_COMMAND_POLL_INTERVAL);
                    }
                    Err(error) => return Err(error),
                }
            }
            let truncated = tail.truncated();
            Ok((tail.into_bytes(), truncated))
        })?;
    Ok(StdPipeCapture {
        stop,
        truncated,
        task,
    })
}

#[cfg(windows)]
fn spawn_std_pipe_capture<R>(
    mut reader: R,
    capacity: usize,
    name: &str,
) -> std::io::Result<StdPipeCapture>
where
    R: std::io::Read + std::os::windows::io::AsRawHandle + Send + 'static,
{
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop = std::sync::Arc::clone(&stop);
    let truncated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_truncated = std::sync::Arc::clone(&truncated);
    let task = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let handle = reader.as_raw_handle();
            let mut tail = BoundedTail::new(capacity);
            let mut chunk = [0_u8; 8192];
            loop {
                let mut available = 0_u32;
                // SAFETY: `handle` is the live pipe owned by `reader`; all
                // optional output pointers are null except `available`.
                #[allow(unsafe_code)]
                let peeked = unsafe {
                    PeekNamedPipe(
                        handle,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                };
                if peeked == 0 {
                    let error = std::io::Error::last_os_error();
                    if matches!(error.raw_os_error(), Some(109 | 233)) {
                        break;
                    }
                    return Err(error);
                }
                if available == 0 {
                    if reader_stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(STD_COMMAND_POLL_INTERVAL);
                    continue;
                }
                let read_capacity = usize::try_from(available)
                    .unwrap_or(usize::MAX)
                    .min(chunk.len());
                match reader.read(&mut chunk[..read_capacity]) {
                    Ok(0) => break,
                    Ok(read) => {
                        tail.push(&chunk[..read]);
                        if tail.truncated() {
                            reader_truncated.store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            let truncated = tail.truncated();
            Ok((tail.into_bytes(), truncated))
        })?;
    Ok(StdPipeCapture {
        stop,
        truncated,
        task,
    })
}

fn reap_std_child_bounded(
    mut child: std::process::Child,
    budget: std::time::Duration,
) -> std::io::Result<()> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => return Ok(()),
            None if started.elapsed() < budget => {
                std::thread::sleep(STD_COMMAND_POLL_INTERVAL);
            }
            None => {
                enqueue_child_reap(ReapChild::Std(child));
                return Ok(());
            }
        }
    }
}

enum StdCommandInput {
    Null,
    File(std::fs::File),
}

/// Synchronous counterpart to [`run_bounded_command`].
///
/// The child is isolated in a dedicated Unix process group / Windows Job,
/// polled against a hard wall-clock deadline, and always torn down as a complete
/// tree. Stdout and stderr are drained continuously into fixed-size newest-byte
/// tails. Reader pipes are made pollable, so teardown can signal them to stop
/// and join both OS threads outright; no timeout branch ever drops a live
/// `JoinHandle` or detaches a reader.
pub fn run_bounded_std_command(
    command: std::process::Command,
    options: BoundedCommandOptions,
) -> std::io::Result<BoundedCommandOutput> {
    run_bounded_std_command_with_input(command, StdCommandInput::Null, options, false)
}

/// Run a synchronous command like [`run_bounded_std_command`], but terminate
/// its complete process tree as soon as stdout exceeds `stdout_bytes`.
///
/// This is intended for binary capture protocols where truncation makes the
/// entire payload unusable and continuing to drain a flooding producer would
/// only delay an already-known failure.
pub fn run_bounded_std_command_strict_stdout(
    command: std::process::Command,
    options: BoundedCommandOptions,
) -> std::io::Result<BoundedCommandOutput> {
    run_bounded_std_command_with_input(command, StdCommandInput::Null, options, true)
}

/// [`run_bounded_std_command`] with stdin supplied by an already-open regular
/// file. This avoids a potentially blocking writer thread for plumbing commands
/// that consume a bounded request body.
pub fn run_bounded_std_command_with_stdin_file(
    command: std::process::Command,
    stdin: std::fs::File,
    options: BoundedCommandOptions,
) -> std::io::Result<BoundedCommandOutput> {
    run_bounded_std_command_with_input(command, StdCommandInput::File(stdin), options, false)
}

fn run_bounded_std_command_with_input(
    mut command: std::process::Command,
    input: StdCommandInput,
    options: BoundedCommandOptions,
    stop_on_stdout_truncation: bool,
) -> std::io::Result<BoundedCommandOutput> {
    use std::process::Stdio;

    match input {
        StdCommandInput::Null => {
            command.stdin(Stdio::null());
        }
        StdCommandInput::File(file) => {
            command.stdin(Stdio::from(file));
        }
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    isolate_std_command(&mut command);
    child_reaper()?;
    let mut child = command.spawn()?;
    let mut tree = match StdCommandTree::attach(&mut child) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            let _ = reap_std_child_bounded(child, options.reader_grace);
            return Err(error);
        }
    };

    let stdout_pipe = match child.stdout.take() {
        Some(pipe) => pipe,
        None => {
            tree.terminate(&mut child);
            let _ = reap_std_child_bounded(child, options.reader_grace);
            return Err(std::io::Error::other("bounded child stdout was not piped"));
        }
    };
    let stdout = match spawn_std_pipe_capture(
        stdout_pipe,
        options.stdout_bytes,
        "umadev-std-command-stdout",
    ) {
        Ok(reader) => reader,
        Err(error) => {
            tree.terminate(&mut child);
            let _ = reap_std_child_bounded(child, options.reader_grace);
            return Err(error);
        }
    };
    let stderr_pipe = match child.stderr.take() {
        Some(pipe) => pipe,
        None => {
            tree.terminate(&mut child);
            let _ = reap_std_child_bounded(child, options.reader_grace);
            let _ = stdout.finish();
            return Err(std::io::Error::other("bounded child stderr was not piped"));
        }
    };
    let stderr = match spawn_std_pipe_capture(
        stderr_pipe,
        options.stderr_bytes,
        "umadev-std-command-stderr",
    ) {
        Ok(reader) => reader,
        Err(error) => {
            tree.terminate(&mut child);
            let _ = reap_std_child_bounded(child, options.reader_grace);
            let _ = stdout.finish();
            return Err(error);
        }
    };

    let started = std::time::Instant::now();
    let deadline = started.checked_add(options.timeout).unwrap_or(started);
    let mut failure = None;
    let (status, timed_out) = loop {
        if stop_on_stdout_truncation && stdout.is_truncated() {
            tree.terminate(&mut child);
            break (None, false);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            tree.terminate(&mut child);
            break (None, true);
        }

        #[cfg(unix)]
        let observed = match tree.leader_exited_unreaped() {
            Ok(true) => {
                tree.terminate(&mut child);
                match child.wait() {
                    Ok(status) => Some(status),
                    Err(error) => {
                        failure = Some(error);
                        None
                    }
                }
            }
            Ok(false) => None,
            Err(error) => {
                // Identity cannot be proven, so group signaling is permanently
                // disabled. The direct handle remains safe to poll/kill.
                match child.try_wait() {
                    Ok(status) => status,
                    Err(_) => {
                        failure = Some(error);
                        None
                    }
                }
            }
        };
        #[cfg(not(unix))]
        let observed = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                failure = Some(error);
                None
            }
        };

        if let Some(status) = observed {
            tree.terminate(&mut child);
            break (Some(status), false);
        }
        if failure.is_some() {
            tree.terminate(&mut child);
            break (None, false);
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(STD_COMMAND_POLL_INTERVAL),
        );
    };

    // Tree teardown precedes this signal, so no descendant can keep producing
    // after the readers drain the bytes already resident in the pipes.
    let stdout_result = stdout.finish();
    let stderr_result = stderr.finish();
    if status.is_none() {
        let _ = reap_std_child_bounded(child, options.reader_grace);
    }
    if let Some(error) = failure {
        let _ = stdout_result;
        let _ = stderr_result;
        return Err(error);
    }
    let (stdout, stdout_truncated) = stdout_result?;
    let (stderr, stderr_truncated) = stderr_result?;
    Ok(BoundedCommandOutput {
        status,
        timed_out,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

/// A Tokio child owned together with its dedicated process tree.
///
/// Dropping this guard (including future cancellation or task abort) terminates
/// the complete Unix process group / Windows Job Object. The direct child is
/// transferred to one process-wide polling reaper which does not depend on the
/// caller's Tokio runtime and never creates a per-child detached task.
pub struct ManagedChild {
    child: Option<tokio::process::Child>,
    tree: IsolatedCommandTree,
}

impl std::fmt::Debug for ManagedChild {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedChild")
            .field("id", &self.id())
            .field("terminated", &self.tree.terminated)
            .finish_non_exhaustive()
    }
}

impl ManagedChild {
    /// Spawn a child in a dedicated process group / Job Object.
    pub fn spawn(mut command: tokio::process::Command) -> std::io::Result<Self> {
        isolate_command(&mut command);
        Self::spawn_prepared(command)
    }

    /// Spawn a child with no controlling terminal and a dedicated process tree.
    ///
    /// This is intended for build tools and dev servers whose descendants may
    /// otherwise write directly to a TUI's terminal.
    pub fn spawn_detached(mut command: tokio::process::Command) -> std::io::Result<Self> {
        isolate_detached_command(&mut command);
        Self::spawn_prepared(command)
    }

    fn spawn_prepared(mut command: tokio::process::Command) -> std::io::Result<Self> {
        child_reaper()?;
        command.kill_on_drop(true);
        let mut child = spawn_managed_child(&mut command)?;
        let tree = match IsolatedCommandTree::attach(&mut child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.start_kill();
                enqueue_child_reap(ReapChild::Tokio(Box::new(child)));
                return Err(error);
            }
        };
        Ok(Self {
            child: Some(child),
            tree,
        })
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child
            .as_mut()
            .expect("managed child is present until its guard is dropped")
    }

    /// Take the configured stdout pipe without exposing raw wait/reap methods
    /// that could invalidate the Unix process-group identity anchor.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child_mut().stdout.take()
    }

    /// Take the configured stderr pipe without exposing raw wait/reap methods
    /// that could invalidate the Unix process-group identity anchor.
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child_mut().stderr.take()
    }

    /// Take the configured stdin pipe without exposing raw wait/reap methods
    /// that could invalidate the Unix process-group identity anchor.
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child_mut().stdin.take()
    }

    /// The direct child's process id while Tokio still owns its handle.
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(tokio::process::Child::id)
    }

    /// Wait for the direct child while preserving process-tree identity.
    ///
    /// On Unix this observes exit with `waitid(WNOWAIT)`, terminates the group
    /// while the unreaped leader still prevents PID/PGID reuse, and only then
    /// reaps the leader. Therefore a later Drop can never blindly signal a
    /// recycled PGID. Windows retains identity through the owned Job handle.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(unix)]
        {
            loop {
                match unix_child_exited_unreaped(self.tree.pid) {
                    Ok(true) => {
                        // No await between observing the unreaped leader and
                        // signaling its group: the PID remains an identity
                        // anchor throughout this critical section.
                        self.terminate();
                        return self.child_mut().wait().await;
                    }
                    Ok(false) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                    Err(_) => {
                        // We cannot prove the leader remains waitable. Fail
                        // closed for group signaling; a direct-child wait/kill
                        // remains safe through Tokio's owned handle.
                        self.tree.abandon_group_identity();
                        return self.child_mut().wait().await;
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            self.child_mut().wait().await
        }
    }

    /// Terminate the direct child and every descendant. Idempotent.
    pub fn terminate(&mut self) {
        if let Some(child) = self.child.as_mut() {
            self.tree.terminate(child);
        }
    }

    /// Terminate the full tree and spend at most `budget` reaping the direct
    /// child. `Ok(None)` means the reap budget elapsed; Drop transfers the
    /// killed child to the process-wide reaper without blocking the caller.
    pub async fn terminate_and_reap(
        &mut self,
        budget: std::time::Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.terminate();
        match tokio::time::timeout(budget, self.wait()).await {
            Ok(result) => result.map(Some),
            Err(_) => Ok(None),
        }
    }
}

fn spawn_managed_child(
    command: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    #[cfg(unix)]
    for _ in 0..30 {
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => return result,
        }
    }
    command.spawn()
}

/// Observe an owned Unix child exit without consuming its wait status.
///
/// Keeping the exited leader unreaped preserves its PID/PGID identity so a
/// caller can terminate the complete process group before finally waiting on
/// the direct child. `pid` must identify a child owned by this process;
/// otherwise the OS returns an error and no signal is sent by this function.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn unix_child_exited_unreaped(pid: u32) -> std::io::Result<bool> {
    let native_pid = libc::id_t::try_from(pid)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid child pid"))?;
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: `info` points to writable siginfo storage. WNOWAIT observes the
    // owned child without consuming its status, which keeps the PID/PGID from
    // being recycled until `Child::wait` runs after group termination.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            native_pid,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    // SAFETY: successful waitid initialized `info`; si_pid==0 is the specified
    // WNOHANG result when the child has not exited yet.
    Ok(unsafe { info.assume_init().si_pid() } != 0)
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        self.tree.terminate(&mut child);
        if child.id().is_some() {
            enqueue_child_reap(ReapChild::Tokio(Box::new(child)));
        }
    }
}

async fn finish_pipe(capture: Option<PipeCapture>, grace: std::time::Duration) -> (Vec<u8>, bool) {
    match capture {
        Some(capture) => capture.finish(grace).await,
        None => (Vec::new(), false),
    }
}

/// Run a non-interactive command with bounded wall time, bounded output tails,
/// and whole-tree teardown.
///
/// The command is placed in its own process group on Unix and a kill-on-close
/// Job Object on Windows. Stdout and stderr are always drained concurrently,
/// but only their fixed-size newest tails are retained. On timeout, wait error,
/// normal parent exit, or caller cancellation, descendants are terminated and
/// reader tasks are either joined within `reader_grace` or aborted. Stdin is
/// always null; interactive commands are intentionally outside this API.
///
/// A timeout is a successful structured return with `timed_out == true`, no
/// exit status, and whatever bounded output tails were captured before tree
/// termination. Spawn/wait failures remain `Err`. On Windows the function fails
/// closed if Job Object attachment is unavailable, because a direct-child-only
/// fallback would violate the process-tree guarantee.
pub async fn run_bounded_command(
    command: tokio::process::Command,
    options: BoundedCommandOptions,
) -> std::io::Result<BoundedCommandOutput> {
    run_bounded_command_with(command, options, false).await
}

/// [`run_bounded_command`] plus detachment from the controlling terminal.
///
/// Use this for build tools, browser runners, and dev-server wrappers whose
/// descendants may try to open `/dev/tty`. It otherwise has identical timeout,
/// output, whole-tree, and cancellation guarantees.
pub async fn run_bounded_detached_command(
    command: tokio::process::Command,
    options: BoundedCommandOptions,
) -> std::io::Result<BoundedCommandOutput> {
    run_bounded_command_with(command, options, true).await
}

async fn run_bounded_command_with(
    mut command: tokio::process::Command,
    options: BoundedCommandOptions,
    detached: bool,
) -> std::io::Result<BoundedCommandOutput> {
    use std::process::Stdio;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = if detached {
        ManagedChild::spawn_detached(command)?
    } else {
        ManagedChild::spawn(command)?
    };
    let stdout = child
        .take_stdout()
        .map(|pipe| PipeCapture::spawn(pipe, options.stdout_bytes));
    let stderr = child
        .take_stderr()
        .map(|pipe| PipeCapture::spawn(pipe, options.stderr_bytes));

    let (status, timed_out) = match tokio::time::timeout(options.timeout, child.wait()).await {
        Ok(Ok(status)) => (Some(status), false),
        Ok(Err(error)) => {
            let _ = child.terminate_and_reap(options.reader_grace).await;
            // A wait error is still a terminal ownership path. Do not rely on
            // `PipeCapture::drop` (which can abort but cannot join); explicitly
            // finish both readers so no JoinHandle is detached on return.
            let _ = tokio::join!(
                finish_pipe(stdout, options.reader_grace),
                finish_pipe(stderr, options.reader_grace)
            );
            return Err(error);
        }
        Err(_) => {
            let _ = child.terminate_and_reap(options.reader_grace).await;
            (None, true)
        }
    };

    // The direct child may have exited after launching a background descendant.
    // Tear down the still-owned group/job before waiting for inherited pipe
    // handles to close, otherwise the reader grace would be the only bound.
    child.terminate();
    let ((stdout, stdout_truncated), (stderr, stderr_truncated)) = tokio::join!(
        finish_pipe(stdout, options.reader_grace),
        finish_pipe(stderr, options.reader_grace)
    );
    Ok(BoundedCommandOutput {
        status,
        timed_out,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

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
    /// Create and attach a kill-on-close Job Object, then resume the process.
    ///
    /// Commands that need a race-free process-tree boundary must be spawned with
    /// `CREATE_SUSPENDED`. Assignment therefore happens before the child can run
    /// user code or create descendants. A non-suspended child is also accepted;
    /// resuming a thread whose suspend count is zero is a harmless no-op.
    #[allow(unsafe_code)]
    pub fn attach(child: &mut tokio::process::Child) -> std::io::Result<Self> {
        let process = child
            .raw_handle()
            .ok_or_else(|| std::io::Error::other("spawned process has no Windows handle"))?
            as windows_sys::Win32::Foundation::HANDLE;
        match Self::attach_handle(process) {
            Ok(job) => Ok(job),
            Err(error) => {
                let _ = child.start_kill();
                Err(error)
            }
        }
    }

    /// Create, configure, and attach a kill-on-close Job Object to a synchronous
    /// standard-library child.
    #[allow(unsafe_code)]
    pub fn attach_std(child: &mut std::process::Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle as _;

        let process = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        match Self::attach_handle(process) {
            Ok(job) => Ok(job),
            Err(error) => {
                let _ = child.kill();
                Err(error)
            }
        }
    }

    #[allow(unsafe_code)]
    fn attach_handle(process: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: null optional inputs and the information pointer match the
        // synchronous Win32 signatures and remain valid throughout each call.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
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
                let error = std::io::Error::last_os_error();
                windows_sys::Win32::Foundation::CloseHandle(handle);
                return Err(error);
            }
            if let Err(error) = resume_process_threads(process) {
                // KILL_ON_JOB_CLOSE terminates the still-suspended assigned
                // process before this error escapes.
                windows_sys::Win32::Foundation::CloseHandle(handle);
                return Err(error);
            }
            Ok(Self {
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
#[allow(unsafe_code)]
fn resume_process_threads(process: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessId, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    // SAFETY: the process handle is owned by the live Child. Snapshot/thread
    // handles are closed exactly once, and THREADENTRY32 advertises its size as
    // required by the ToolHelp API.
    unsafe {
        let pid = GetProcessId(process);
        if pid == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        let mut found = false;
        let mut next = Thread32First(snapshot, &mut entry) != 0;
        while next {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(snapshot);
                    return Err(error);
                }
                let resumed = ResumeThread(thread);
                CloseHandle(thread);
                if resumed == u32::MAX {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(snapshot);
                    return Err(error);
                }
                found = true;
            }
            next = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if found {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "suspended child had no resumable process thread",
            ))
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

/// Determine whether `pid` names a live process using native, locale-neutral
/// operating-system state.
///
/// Windows opens the process for synchronization and performs a zero-duration
/// wait. A signaled process handle is exited, `WAIT_TIMEOUT` is live, and access
/// denied is conservatively treated as live because the PID demonstrably names
/// a protected process. Other platforms return `None`; their callers retain
/// their native Unix probe.
#[must_use]
pub fn process_is_alive(pid: u32) -> Option<bool> {
    #[cfg(windows)]
    {
        process_is_alive_windows(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// Determine whether one exact argument belongs to a live process.
///
/// The lookup is native, bounded, and fail-closed: Linux/Android read at most
/// 1 MiB from `/proc/<pid>/cmdline`, while macOS reads at most 1 MiB through
/// `KERN_PROCARGS2`. `None` means the process disappeared, access was denied,
/// the operating system does not expose a supported native lookup, or argv
/// could not be read completely. Callers must not treat `None` as proof of
/// ownership.
#[must_use]
pub fn process_has_exact_argument(pid: u32, argument: &str) -> Option<bool> {
    if pid == 0 || argument.is_empty() || argument.bytes().any(|byte| byte == 0) {
        return None;
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        process_has_exact_argument_procfs(pid, argument.as_bytes())
    }
    #[cfg(target_os = "macos")]
    {
        process_has_exact_argument_macos(pid, argument.as_bytes())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = argument;
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
const PROCESS_ARGUMENT_BYTES: usize = 1024 * 1024;

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_has_exact_argument_procfs(pid: u32, argument: &[u8]) -> Option<bool> {
    use std::io::Read as _;

    let file = std::fs::File::open(format!("/proc/{pid}/cmdline")).ok()?;
    let limit = u64::try_from(PROCESS_ARGUMENT_BYTES).ok()?.checked_add(1)?;
    let mut bytes = Vec::with_capacity(PROCESS_ARGUMENT_BYTES.min(64 * 1024));
    file.take(limit).read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() || bytes.len() > PROCESS_ARGUMENT_BYTES {
        return None;
    }
    Some(
        bytes
            .split(|byte| *byte == 0)
            .any(|entry| entry == argument),
    )
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_has_exact_argument_macos(pid: u32, argument: &[u8]) -> Option<bool> {
    let native_pid = libc::c_int::try_from(pid).ok()?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, native_pid];
    let mib_len = libc::c_uint::try_from(mib.len()).ok()?;
    let mut byte_len = 0_usize;

    // SAFETY: the MIB points to three initialized integers. A null output
    // pointer asks the kernel for the argv buffer length.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib_len,
            std::ptr::null_mut(),
            &mut byte_len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || byte_len == 0
        || byte_len > PROCESS_ARGUMENT_BYTES
    {
        return None;
    }

    let mut bytes = vec![0_u8; byte_len];
    let mut written = bytes.len();
    // SAFETY: `bytes` is writable for `written` bytes, the MIB remains valid,
    // and no input buffer is supplied. The returned length is checked before
    // the initialized byte slice is parsed.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib_len,
            bytes.as_mut_ptr().cast(),
            &mut written,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || written == 0
        || written > bytes.len()
    {
        return None;
    }
    bytes.truncate(written);
    macos_procargs_has_exact_argument(&bytes, argument)
}

#[cfg(target_os = "macos")]
fn macos_procargs_has_exact_argument(bytes: &[u8], argument: &[u8]) -> Option<bool> {
    let argc_bytes: [u8; std::mem::size_of::<libc::c_int>()] = bytes
        .get(..std::mem::size_of::<libc::c_int>())?
        .try_into()
        .ok()?;
    let argc = usize::try_from(libc::c_int::from_ne_bytes(argc_bytes)).ok()?;
    let mut cursor = std::mem::size_of::<libc::c_int>();

    // KERN_PROCARGS2 starts with argc, then the executable path, NUL padding,
    // and exactly argc argument strings.
    cursor = bytes.get(cursor..)?.iter().position(|byte| *byte == 0)? + cursor + 1;
    while bytes.get(cursor) == Some(&0) {
        cursor += 1;
    }
    for _ in 0..argc {
        let tail = bytes.get(cursor..)?;
        let end = tail.iter().position(|byte| *byte == 0)?;
        let entry = tail.get(..end)?;
        if entry == argument {
            return Some(true);
        }
        cursor = cursor.checked_add(end)?.checked_add(1)?;
    }
    Some(false)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn process_is_alive_windows(pid: u32) -> Option<bool> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    // SAFETY: `OpenProcess` returns a new owned handle or null. A successful
    // handle is synchronously inspected and closed exactly once below.
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if handle.is_null() {
            return match GetLastError() {
                ERROR_INVALID_PARAMETER => Some(false),
                ERROR_ACCESS_DENIED => Some(true),
                _ => None,
            };
        }
        let wait = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        match wait {
            WAIT_OBJECT_0 => Some(false),
            WAIT_TIMEOUT => Some(true),
            _ => None,
        }
    }
}

#[cfg(any(windows, test))]
fn windows_clipboard_payload(text: &str) -> Option<Vec<u16>> {
    if text.contains('\0') {
        return None;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.try_reserve(1).ok()?;
    wide.push(0);
    wide.len()
        .checked_mul(std::mem::size_of::<u16>())
        .map(|_| wide)
}

/// Copy Unicode text through the native Windows `CF_UNICODETEXT` clipboard.
///
/// The API accepts UTF-8 at the safe Rust boundary and publishes UTF-16
/// directly, avoiding console code pages and shell processes. Returns `false`
/// on non-Windows targets, embedded NUL input, a busy clipboard, or any native
/// API failure.
#[must_use]
pub fn set_windows_clipboard_text(text: &str) -> bool {
    #[cfg(windows)]
    {
        set_windows_clipboard_text_inner(text)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        false
    }
}

#[cfg(windows)]
struct OpenClipboardGuard;

#[cfg(windows)]
struct ClipboardOwnerGuard {
    handle: windows_sys::Win32::Foundation::HWND,
    destroy: bool,
}

#[cfg(windows)]
impl ClipboardOwnerGuard {
    #[allow(unsafe_code)]
    fn for_current_terminal() -> Option<Self> {
        use windows_sys::Win32::System::Console::GetConsoleWindow;
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, HWND_MESSAGE};

        // SAFETY: `GetConsoleWindow` takes no inputs. If a terminal (for
        // example MSYS/mintty) has no attached console HWND, create a private
        // message-only window from Windows' built-in STATIC class. All pointers
        // are valid for the duration of this synchronous call.
        unsafe {
            let console = GetConsoleWindow();
            if !console.is_null() {
                return Some(Self {
                    handle: console,
                    destroy: false,
                });
            }
            let class = "STATIC\0".encode_utf16().collect::<Vec<_>>();
            let handle = CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            (!handle.is_null()).then_some(Self {
                handle,
                destroy: true,
            })
        }
    }
}

#[cfg(windows)]
impl Drop for ClipboardOwnerGuard {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if self.destroy {
            // SAFETY: this is the one destroy of the private message-only
            // window created by `for_current_terminal`, on the same thread.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.handle);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for OpenClipboardGuard {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: this guard exists only after `OpenClipboard` succeeds, and is
        // the sole matching close on every return path.
        unsafe {
            windows_sys::Win32::System::DataExchange::CloseClipboard();
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn set_windows_clipboard_text_inner(text: &str) -> bool {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    const CF_UNICODETEXT: u32 = 13;
    const OPEN_ATTEMPTS: usize = 10;
    const OPEN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

    let Some(wide) = windows_clipboard_payload(text) else {
        return false;
    };
    let Some(bytes) = wide.len().checked_mul(std::mem::size_of::<u16>()) else {
        return false;
    };

    // SAFETY: the movable global allocation is kept owned locally until a
    // successful `SetClipboardData` transfers ownership to Windows.
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if memory.is_null() {
            return false;
        }
        let destination = GlobalLock(memory).cast::<u16>();
        if destination.is_null() {
            GlobalFree(memory);
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), destination, wide.len());
        let _ = GlobalUnlock(memory);

        // `EmptyClipboard` assigns ownership to the window passed to
        // `OpenClipboard`. Microsoft documents that opening with a null HWND
        // leaves the owner null and makes the following `SetClipboardData`
        // fail. Use the console HWND or a private message-only fallback.
        let Some(owner) = ClipboardOwnerGuard::for_current_terminal() else {
            GlobalFree(memory);
            return false;
        };
        let mut opened = false;
        for attempt in 0..OPEN_ATTEMPTS {
            if OpenClipboard(owner.handle) != 0 {
                opened = true;
                break;
            }
            if attempt + 1 < OPEN_ATTEMPTS {
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
        }
        if !opened {
            GlobalFree(memory);
            return false;
        }
        let _clipboard = OpenClipboardGuard;
        if EmptyClipboard() == 0 {
            GlobalFree(memory);
            return false;
        }
        if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
            GlobalFree(memory);
            return false;
        }
        true
    }
}

/// Run a trusted executable below the Windows system directory with bounded
/// time and output. Relative paths containing anything except normal
/// components are rejected.
#[cfg(windows)]
pub fn windows_system_command_stdout(
    relative_program: &std::path::Path,
    args: &[&str],
    timeout: std::time::Duration,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    use std::path::Component;

    if max_bytes == 0
        || relative_program.is_absolute()
        || relative_program
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    let program = windows_system_directory()?.join(relative_program);
    let mut command = std::process::Command::new(program);
    command.args(args);
    let output = run_bounded_std_command_strict_stdout(
        command,
        BoundedCommandOptions {
            timeout,
            stdout_bytes: max_bytes,
            stderr_bytes: 0,
            reader_grace: std::time::Duration::from_secs(1),
        },
    )
    .ok()?;
    if output.timed_out
        || output.stdout_truncated
        || !output.status.is_some_and(|status| status.success())
    {
        return None;
    }
    Some(output.stdout)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_system_directory() -> Option<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = vec![0_u16; 260];
    loop {
        // SAFETY: `buffer` is writable for the advertised length. The API
        // returns either a copied length or the required capacity.
        let length =
            unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), u32::try_from(buffer.len()).ok()?) };
        if length == 0 {
            return None;
        }
        let length = usize::try_from(length).ok()?;
        if length < buffer.len() {
            buffer.truncate(length);
            return Some(std::path::PathBuf::from(OsString::from_wide(&buffer)));
        }
        buffer.resize(length.saturating_add(1), 0);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use std::path::{Path, PathBuf};
    #[cfg(windows)]
    use std::process::Stdio;
    use std::time::Duration;
    #[cfg(windows)]
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn assert_pid_reaped(pid: libc::pid_t) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid, 0) } == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "killed child {pid} was not reaped"
        );
    }

    #[test]
    fn job_support_matches_target() {
        assert_eq!(super::has_kill_on_close_job(), cfg!(windows));
    }

    #[test]
    fn bounded_tail_keeps_only_the_newest_bytes() {
        let mut tail = super::BoundedTail::new(5);
        tail.push(b"abc");
        tail.push(b"defgh");
        assert_eq!(tail.len(), 5);
        assert_eq!(tail.total_seen(), 8);
        assert!(tail.truncated());
        assert_eq!(tail.into_bytes(), b"defgh");

        let mut zero = super::BoundedTail::new(0);
        zero.push(b"never retained");
        assert!(zero.is_empty());
        assert!(zero.truncated());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_std_command_drains_newline_free_floods_into_fixed_tails() {
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "head -c 524288 /dev/zero | tr '\\0' x; printf 'STDOUT-TAIL'; \
             head -c 524288 /dev/zero | tr '\\0' y >&2; printf 'STDERR-TAIL' >&2",
        ]);
        let output = super::run_bounded_std_command(
            command,
            super::BoundedCommandOptions {
                timeout: Duration::from_secs(5),
                stdout_bytes: 4096,
                stderr_bytes: 4096,
                reader_grace: Duration::from_secs(1),
            },
        )
        .expect("bounded synchronous flood completes");
        assert!(output.status.is_some_and(|status| status.success()));
        assert!(!output.timed_out);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
        assert!(output.stdout.len() <= 4096);
        assert!(output.stderr.len() <= 4096);
        assert!(output.stdout.ends_with(b"STDOUT-TAIL"));
        assert!(output.stderr.ends_with(b"STDERR-TAIL"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_std_command_timeout_kills_and_reaps_without_reader_detach() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf before-timeout; sleep 30"]);
        let started = std::time::Instant::now();
        let output = super::run_bounded_std_command(
            command,
            super::BoundedCommandOptions {
                timeout: Duration::from_millis(100),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
                reader_grace: Duration::from_secs(1),
            },
        )
        .expect("timeout is a structured result");
        assert!(output.timed_out);
        assert!(output.status.is_none());
        assert_eq!(output.stdout, b"before-timeout");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "hard timeout or reader join exceeded its fixed teardown envelope"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_std_timeout_reaps_the_direct_child() {
        let temp = tempfile::TempDir::new().unwrap();
        let pid_file = temp.path().join("timed-out-leader.pid");
        let script = format!(
            "printf '%s' \"$$\" > '{}'; exec sleep 30",
            pid_file.display()
        );
        let mut command = std::process::Command::new("sh");
        command.args(["-c", &script]);
        let output = super::run_bounded_std_command(
            command,
            super::BoundedCommandOptions {
                timeout: Duration::from_millis(100),
                stdout_bytes: 0,
                stderr_bytes: 0,
                reader_grace: Duration::from_millis(100),
            },
        )
        .expect("timeout returns after transferring or completing the reap");
        assert!(output.timed_out);
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert_pid_reaped(pid);
    }

    #[cfg(unix)]
    #[test]
    fn strict_stdout_cap_stops_a_flood_before_the_command_deadline() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "yes x"]);
        let started = std::time::Instant::now();
        let output = super::run_bounded_std_command_strict_stdout(
            command,
            super::BoundedCommandOptions {
                timeout: Duration::from_secs(10),
                stdout_bytes: 4096,
                stderr_bytes: 0,
                reader_grace: Duration::from_secs(1),
            },
        )
        .expect("strict stdout limit is a structured result");
        assert!(output.stdout_truncated);
        assert!(!output.timed_out);
        assert!(output.status.is_none());
        assert!(output.stdout.len() <= 4096);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "strict output rejection waited for the command deadline"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_std_command_kills_descendant_after_wrapper_exit() {
        fn process_is_running(pid: i32) -> bool {
            std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .is_ok_and(|output| {
                    let state = String::from_utf8_lossy(&output.stdout);
                    !state.trim().is_empty() && !state.trim().starts_with('Z')
                })
        }

        let temp = tempfile::TempDir::new().unwrap();
        let pid_file = temp.path().join("std-leaf.pid");
        let script = format!(
            "sleep 30 & leaf=$!; printf '%s' \"$leaf\" > '{}'; printf done; exit 0",
            pid_file.display()
        );
        let mut command = std::process::Command::new("sh");
        command.args(["-c", &script]);
        let output = super::run_bounded_std_command(
            command,
            super::BoundedCommandOptions {
                timeout: Duration::from_secs(5),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
                reader_grace: Duration::from_secs(1),
            },
        )
        .expect("exited wrapper is reaped after its group is terminated");
        assert!(output.status.is_some_and(|status| status.success()));
        assert_eq!(output.stdout, b"done");

        let leaf = std::fs::read_to_string(pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while process_is_running(leaf) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_is_running(leaf),
            "normal wrapper exit left descendant {leaf} alive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reaped_std_leader_never_signals_a_reused_numeric_group() {
        let mut foreign_command = std::process::Command::new("sleep");
        foreign_command.arg("30");
        super::isolate_std_command(&mut foreign_command);
        let mut foreign = foreign_command.spawn().unwrap();
        let mut foreign_tree = super::StdCommandTree::attach(&mut foreign).unwrap();

        let mut owned_command = std::process::Command::new("sleep");
        owned_command.arg("30");
        super::isolate_std_command(&mut owned_command);
        let mut owned = owned_command.spawn().unwrap();
        let mut owned_tree = super::StdCommandTree::attach(&mut owned).unwrap();
        owned.kill().unwrap();
        owned.wait().unwrap();

        // Model PID/PGID reuse after the leader was consumed. The cached number
        // now names a foreign group, but waitid must reject it as not our child.
        owned_tree.pid = foreign.id();
        owned_tree.terminate(&mut owned);
        assert!(foreign.try_wait().unwrap().is_none());

        foreign_tree.terminate(&mut foreign);
        let _ = foreign.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_command_caps_a_newline_free_flood_and_keeps_its_tail() {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "head -c 524288 /dev/zero | tr '\\0' x; printf 'TAIL-SENTINEL'",
        ]);
        let started = std::time::Instant::now();
        let output = super::run_bounded_command(
            command,
            super::BoundedCommandOptions {
                timeout: Duration::from_secs(5),
                stdout_bytes: 4096,
                stderr_bytes: 1024,
                reader_grace: Duration::from_secs(1),
            },
        )
        .await
        .expect("bounded flood command completes");
        assert!(output.status.is_some_and(|status| status.success()));
        assert!(!output.timed_out);
        assert!(output.stdout_truncated);
        assert!(output.stdout.len() <= 4096);
        assert!(output.stdout.ends_with(b"TAIL-SENTINEL"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn bounded_command_kills_a_descendant_that_holds_pipes_open() {
        let temp = tempfile::TempDir::new().unwrap();
        let pid_file = temp.path().join("leaf.pid");
        let script = format!(
            "sleep 30 & leaf=$!; printf '%s' \"$leaf\" > '{}'; printf done; exit 0",
            pid_file.display()
        );
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", &script]);
        let started = std::time::Instant::now();
        let output = super::run_bounded_command(
            command,
            super::BoundedCommandOptions {
                timeout: Duration::from_secs(5),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
                reader_grace: Duration::from_secs(1),
            },
        )
        .await
        .expect("exited wrapper must not leave inherited-pipe readers blocked");
        assert!(output.status.is_some_and(|status| status.success()));
        assert!(!output.timed_out);
        assert_eq!(output.stdout, b"done");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "reader teardown waited for the 30-second descendant"
        );

        let leaf = std::fs::read_to_string(&pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(leaf, 0) } == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_ne!(
            unsafe { libc::kill(leaf, 0) },
            0,
            "whole-tree teardown left the descendant alive"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn managed_wait_terminates_group_before_reaping_its_leader() {
        let temp = tempfile::TempDir::new().unwrap();
        let pid_file = temp.path().join("anchored-leaf.pid");
        let script = format!(
            "sleep 30 & leaf=$!; printf '%s' \"$leaf\" > '{}'; exit 0",
            pid_file.display()
        );
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", &script]);
        let mut child = super::ManagedChild::spawn_detached(command).unwrap();

        assert!(child.wait().await.unwrap().success());
        assert!(child.tree.terminated);
        assert!(!child.tree.group_signal_safe);
        let leaf = std::fs::read_to_string(&pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(leaf, 0) } == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_ne!(
            unsafe { libc::kill(leaf, 0) },
            0,
            "anchored wait left a real descendant alive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_drop_reaps_after_its_origin_runtime_is_gone() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let child = runtime.block_on(async {
            let mut command = tokio::process::Command::new("sleep");
            command.arg("30");
            super::ManagedChild::spawn_detached(command).unwrap()
        });
        let pid = libc::pid_t::try_from(child.id().unwrap()).unwrap();
        drop(runtime);
        drop(child);
        assert_pid_reaped(pid);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lost_group_identity_never_signals_a_reused_numeric_group() {
        let mut foreign_command = std::process::Command::new("sleep");
        foreign_command.arg("30");
        super::isolate_std_command(&mut foreign_command);
        let mut foreign = foreign_command.spawn().unwrap();
        let mut foreign_tree = super::StdCommandTree::attach(&mut foreign).unwrap();
        let foreign_group = foreign.id();

        let mut owned_command = tokio::process::Command::new("sleep");
        owned_command.arg("30");
        let mut owned = super::ManagedChild::spawn_detached(owned_command).unwrap();
        // Model the only safe state after an external/unknown reap: the cached
        // number may now identify a foreign group, so the guard must retain only
        // its direct-child handle and permanently suppress killpg.
        owned.tree.pid = foreign_group;
        owned.tree.abandon_group_identity();
        owned.terminate();
        assert!(foreign.try_wait().unwrap().is_none());

        foreign_tree.terminate(&mut foreign);
        let _ = foreign.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_command_timeout_returns_partial_output_tail() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "printf 'before-timeout'; sleep 30"]);
        let output = super::run_bounded_command(
            command,
            super::BoundedCommandOptions {
                timeout: Duration::from_millis(100),
                stdout_bytes: 1024,
                stderr_bytes: 1024,
                reader_grace: Duration::from_secs(1),
            },
        )
        .await
        .expect("timeout is a structured output, not a lossy error");
        assert!(output.timed_out);
        assert!(output.status.is_none());
        assert_eq!(output.stdout, b"before-timeout");
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn native_process_argument_lookup_is_exact() {
        const VALUE: &str = "umadev-owner-value-9273";

        let mut command = std::process::Command::new("sh");
        command.args(["-c", "while :; do sleep 30; done", VALUE]);
        super::isolate_std_command(&mut command);
        let mut child = command.spawn().expect("spawn argument probe child");
        let mut tree = super::StdCommandTree::attach(&mut child).expect("attach child tree");
        let pid = child.id();

        // `spawn` returns after fork/clone, so Linux may expose the child before
        // it has exec'd `sh` and published its final argv in `/proc`. Treat the
        // documented transient `None` as unavailable and wait for the real
        // argument surface instead of making the test depend on scheduler luck.
        // The child lives 30s, so a GENEROUS ceiling (not a tight 2s that a
        // loaded CI runner's fork/exec latency can blow past — the observed
        // flake) costs nothing on the common fast path and only bounds a genuine
        // hang.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let exact = loop {
            match super::process_has_exact_argument(pid, VALUE) {
                Some(found) => break Some(found),
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => break None,
            }
        };
        assert_eq!(exact, Some(true));
        assert_eq!(
            super::process_has_exact_argument(pid, "umadev-owner-value"),
            Some(false),
            "an argument prefix must not match"
        );

        tree.terminate(&mut child);
        let _ = child.wait();
    }

    #[cfg(windows)]
    #[test]
    fn native_process_liveness_detects_current_and_exited_processes() {
        assert_eq!(super::process_is_alive(std::process::id()), Some(true));
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "exit", "0"])
            .spawn()
            .expect("spawn short Windows child");
        let pid = child.id();
        child.wait().expect("reap short Windows child");
        assert_eq!(super::process_is_alive(pid), Some(false));
    }

    #[test]
    fn windows_clipboard_payload_is_utf16_nul_terminated() {
        let text = "继续补充 MOM e\u{301} 🙂\r\n第二行";
        let payload = super::windows_clipboard_payload(text).expect("valid clipboard text");
        assert_eq!(payload.last(), Some(&0));
        assert_eq!(
            String::from_utf16(&payload[..payload.len() - 1]).unwrap(),
            text
        );
        assert_eq!(super::windows_clipboard_payload("a\0b"), None);
    }

    #[cfg(windows)]
    #[test]
    fn trusted_windows_system_command_is_bounded_and_confined() {
        // This spawns a REAL PowerShell. Its cold start (JIT + module load) can take several
        // seconds on a loaded CI runner, so the deadline here is only generous flake headroom —
        // NOT a behavioural assertion. The tight-deadline kill + output cap are proven
        // separately by `trusted_windows_system_command_enforces_zero_timeout_and_output_cap`.
        // The command itself does no sleeping so the happy path returns as soon as PowerShell
        // is warm; a too-tight 3s budget was the source of an intermittent CI kill.
        let output = super::windows_system_command_stdout(
            Path::new("WindowsPowerShell/v1.0/powershell.exe"),
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Write-Output umadev-system-command",
            ],
            Duration::from_secs(60),
            1024,
        )
        .expect("run PowerShell from the OS-reported system directory");
        assert!(String::from_utf8_lossy(&output).contains("umadev-system-command"));
        assert!(super::windows_system_command_stdout(
            Path::new("../cmd.exe"),
            &["/C", "echo rejected"],
            Duration::from_secs(1),
            1024,
        )
        .is_none());
    }

    #[cfg(windows)]
    #[test]
    fn trusted_windows_system_command_enforces_zero_timeout_and_output_cap() {
        let started = Instant::now();
        assert!(super::windows_system_command_stdout(
            Path::new("cmd.exe"),
            &["/D", "/C", "echo should-not-complete"],
            Duration::ZERO,
            1024,
        )
        .is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "zero timeout was not bounded"
        );

        assert!(super::windows_system_command_stdout(
            Path::new("WindowsPowerShell/v1.0/powershell.exe"),
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Milliseconds 100; [Console]::Out.Write(('x' * 1024))",
            ],
            Duration::from_secs(3),
            32,
        )
        .is_none());
    }

    #[cfg(windows)]
    #[test]
    fn trusted_windows_system_command_keeps_output_closed_before_exit() {
        let output = super::windows_system_command_stdout(
            Path::new("WindowsPowerShell/v1.0/powershell.exe"),
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::Out.Write('output-before-exit'); \
                 [Console]::Out.Close(); \
                 Start-Sleep -Milliseconds 300",
            ],
            Duration::from_secs(3),
            1024,
        )
        .expect("retain complete output while waiting for the process to exit");
        assert_eq!(output, b"output-before-exit");
    }

    #[cfg(windows)]
    #[test]
    fn trusted_windows_system_command_timeout_kills_stdout_descendant() {
        let fixture_dir = FixtureDir::new();
        let leaf_pid_path = fixture_dir.0.join("system-command-leaf-pid");
        let escaped_pid_path = leaf_pid_path.to_string_lossy().replace('\'', "''");
        // Keep the leader alive explicitly. `Start-Process` returns immediately
        // on some Windows/PowerShell versions, and a successful leader exit is
        // intentionally reported as success after its Job descendants are
        // terminated; that path is not a timeout and cannot test this contract.
        let script = format!(
            "Start-Sleep -Milliseconds 100; \
             $p=Start-Process -PassThru -NoNewWindow \
             -FilePath \"$env:SystemRoot\\System32\\ping.exe\" \
             -ArgumentList @('-n','30','127.0.0.1'); \
             [IO.File]::WriteAllText('{escaped_pid_path}', [string]$p.Id); \
             Start-Sleep -Seconds 30"
        );

        let started = Instant::now();
        assert!(super::windows_system_command_stdout(
            Path::new("WindowsPowerShell/v1.0/powershell.exe"),
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script
            ],
            Duration::from_secs(3),
            4096,
        )
        .is_none());
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "descendant-held stdout exceeded the command timeout bound"
        );

        wait_for_path_sync(&leaf_pid_path, Duration::from_secs(2));
        let leaf_pid = std::fs::read_to_string(&leaf_pid_path)
            .expect("read system command leaf pid")
            .trim()
            .parse::<u32>()
            .expect("parse system command leaf pid");
        if let Ok(leaf) = ProcessWaitHandle::open(leaf_pid) {
            assert_eq!(
                leaf.wait(Duration::from_secs(2)),
                windows_sys::Win32::Foundation::WAIT_OBJECT_0,
                "timed-out system command left its stdout descendant alive"
            );
        }
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
        let job = super::KillOnCloseJob::attach(&mut root)
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
