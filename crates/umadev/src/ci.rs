//! `umadev ci` — run governance on eligible project files in the workspace.
//!
//! This is the CI/CD entry point: scan governance-eligible files under the
//! project root and exit non-zero if any file's blocking rule fires.
//! In a Git worktree the full scan includes tracked and untracked non-ignored
//! files; untracked ignored files and generated/vendor paths are excluded.
//!
//! ## Usage
//! ```bash
//! umadev ci                      # scan + fail on any violation
//! umadev ci --report-only        # scan but always exit 0 (for reporting)
//! umadev ci --changed-only       # scan only git-changed files
//! ```
//!
//! ## Output
//! Enforcing mode emits the first hit per file. Report-only mode emits every
//! enabled content-rule hit so its aggregate is suitable for governance audits.

use std::path::{Path, PathBuf};
use umadev_governance::{
    check_sensitive_path, pre_write_floor_decision, scan_content_findings_with_context,
    scan_content_with_context, Decision, Policy, ProjectContext,
};

/// File extensions the CI scan considers "source" (governance-eligible).
const SCAN_EXTENSIONS: &[&str] = &[
    "js", "jsx", "mjs", "cjs", "ts", "tsx", "py", "rb", "go", "rs", "java", "kt", "swift", "php",
    "vue", "svelte", "astro", "html", "htm", "css", "scss", "sass", "yml", "yaml", "sh", "bash",
    "zsh",
];

/// Production resource envelope for governance source reads. A source file is
/// never materialized above 8 MiB, one run never scans more than 256 MiB, and
/// at most four bounded readers may coexist inside a 40 MiB content-buffer
/// envelope. The extra 8 KiB per worker is the fixed drain buffer used by Git
/// blob capture.
const DEFAULT_SCAN_LIMITS: ScanLimits = ScanLimits {
    file_bytes: 8 * 1024 * 1024,
    total_bytes: 256 * 1024 * 1024,
    in_flight_bytes: 40 * 1024 * 1024,
    workers: 4,
    path_output_bytes: 16 * 1024 * 1024,
    path_count: 100_000,
    walk_entries: 500_000,
    walk_depth: 64,
};
const CAPTURE_READ_CHUNK_BYTES: usize = 8 * 1024;
const GOVERNANCE_CONTEXT_MAX_BYTES: u64 = 256 * 1024;
const WORKFLOW_STATE_MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct ScanLimits {
    file_bytes: usize,
    total_bytes: u64,
    in_flight_bytes: usize,
    workers: usize,
    path_output_bytes: usize,
    path_count: usize,
    walk_entries: usize,
    walk_depth: usize,
}

impl ScanLimits {
    fn validate(self) -> std::io::Result<()> {
        let worker_bytes = self
            .file_bytes
            .checked_add(CAPTURE_READ_CHUNK_BYTES)
            .ok_or_else(|| std::io::Error::other("CI scan byte budget overflow"))?;
        if self.file_bytes == 0
            || self.total_bytes == 0
            || self.workers == 0
            || self.in_flight_bytes < worker_bytes
            || self.path_output_bytes == 0
            || self.path_output_bytes == usize::MAX
            || self.path_count == 0
            || self.walk_entries == 0
            || self.walk_depth == 0
        {
            return Err(std::io::Error::other(
                "invalid CI scan budgets: every limit must be positive and one worker must fit",
            ));
        }
        Ok(())
    }

    fn worker_count(self, task_count: usize) -> usize {
        let worker_bytes = self.file_bytes + CAPTURE_READ_CHUNK_BYTES;
        let memory_workers = self.in_flight_bytes / worker_bytes;
        std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(self.workers)
            .min(memory_workers)
            .min(task_count)
            .max(1)
    }
}

/// Is `rel` a security-sensitive path that MUST be scanned by the bypass-immune
/// floor REGARDLESS of its extension?
///
/// The `SCAN_EXTENSIONS` allow-list silently drops exactly the files most likely
/// to leak a live secret — `.env` (no extension), `id_rsa`, `*.pem`, `credentials`,
/// anything under `.ssh/` — so a staged `.env` with a `sk_live_…` key used to scan
/// as "0 files, 0 blocked, exit 0". This predicate pulls those paths back into
/// scope so [`pre_write_floor_decision`] can block them. It is a SUPERSET of the
/// floor's own path guard ([`check_sensitive_path`]) plus the two dotenv/cert
/// forms the guard's fixed suffix list omits (`.env.<anything>`, `*.pem`), so a
/// secret in any of them reaches the content floor. Segment-aware via the reused
/// guard, so `messages.ts` never matches.
fn is_sensitive_scan_path(rel: &str) -> bool {
    let lower = rel.replace('\\', "/").to_ascii_lowercase();
    // *.pem (private keys / certs) anywhere in the tree.
    if std::path::Path::new(&lower)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pem"))
    {
        return true;
    }
    // Any dotenv variant: `.env`, `.env.local`, `.env.staging`, `.env.<x>`.
    let last = lower.rsplit('/').next().unwrap_or("");
    if last == ".env" || last.starts_with(".env.") {
        return true;
    }
    // Reuse the floor's EXACT path guard for the rest (`.ssh/` `.aws/` `.git/`
    // segments, `id_rsa` / `credentials` / `.npmrc` / … suffixes) so CI stays in
    // lockstep with the floor rather than drifting from a hand-copied list.
    check_sensitive_path(rel, "").block
}

/// Directories to skip during the scan (deps, build output, VCS).
/// Dot-directories the FULL scan DESCENDS into anyway (a leading `.` normally skips a dir).
/// These legitimately carry secrets / CI config a commit could leak, so a full `umadev ci`
/// must see them (the changed-only git path already lists their tracked files, so this keeps
/// the two scopes in sync). Kept small so the walk stays fast (no descent into arbitrary
/// dot-dirs).
const SCAN_DOT_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".docker",
    ".config",
    ".github",
    ".circleci",
    ".env.d",
];

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".output",
    ".svelte-kit",
    "vendor",
    ".cache",
    "__pycache__",
    ".venv",
    "venv",
    "coverage",
    ".turbo",
];

/// CI scan options.
#[derive(Debug, Clone)]
pub struct CiOptions {
    /// Report first hits without failing (exit 0).
    pub report_only: bool,
    /// Only scan governance-eligible staged Git blobs (vs the full worktree scope).
    pub changed_only: bool,
    /// Project root to scan.
    pub project_root: PathBuf,
}

/// Result of a CI scan.
#[derive(Debug, Default)]
pub struct CiResult {
    /// Files selected after scope, extension, ignore, and directory filters.
    pub files_selected: usize,
    /// Selected files whose UTF-8 content was actually scanned.
    pub files_scanned: usize,
    /// Number of scanned files with at least one blocking governance decision.
    pub files_blocked: usize,
    /// Governance findings emitted. Complete in report-only mode; first-hit per
    /// file in enforcing mode.
    pub governance_findings: usize,
    /// High/critical dependency findings returned by `npm audit`.
    pub npm_audit_findings: usize,
    /// Selected files that could not be scanned within the declared resource
    /// envelope or changed while being read. Enforcing mode fails closed.
    pub scan_failures: usize,
    /// How the candidate file set was obtained.
    pub scan_scope: CiScanScope,
    /// Whether enforcing mode should fail CI.
    pub failed: bool,
}

/// Reproducible source used to select files for a CI scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CiScanScope {
    /// Git's staged index (`--changed-only`).
    StagedIndex,
    /// Git tracked plus untracked non-ignored files.
    GitWorktree,
    /// Filesystem walk used when Git metadata is unavailable.
    #[default]
    FilesystemFallback,
}

impl CiScanScope {
    fn description(self) -> &'static str {
        match self {
            Self::StagedIndex => "staged Git blobs only",
            Self::GitWorktree => "Git tracked + untracked non-ignored files",
            Self::FilesystemFallback => {
                "filesystem fallback (Git metadata and ignore rules unavailable)"
            }
        }
    }
}

#[derive(Debug)]
struct FileSelection {
    files: Vec<PathBuf>,
    scope: CiScanScope,
}

struct ScanTask {
    path: PathBuf,
    rel: String,
    ctx: ProjectContext,
}

struct FileScan {
    ordinal: usize,
    rel: String,
    scanned: bool,
    findings: Vec<Decision>,
    diagnostic: Option<String>,
}

struct PreparedScanTask<'a> {
    ordinal: usize,
    task: &'a ScanTask,
    expected_bytes: usize,
    worktree_metadata: Option<std::fs::Metadata>,
    worktree_identity: Option<same_file::Handle>,
}

fn failed_scan(ordinal: usize, rel: &str, diagnostic: impl Into<String>) -> FileScan {
    FileScan {
        ordinal,
        rel: rel.to_owned(),
        scanned: false,
        findings: Vec::new(),
        diagnostic: Some(diagnostic.into()),
    }
}

struct SourcePreflight {
    bytes: usize,
    worktree_metadata: Option<std::fs::Metadata>,
    worktree_identity: Option<same_file::Handle>,
}

fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(any(unix, windows)))]
    {
        metadata.file_type().is_symlink()
    }
}

fn workspace_regular_file_metadata(root: &Path, path: &Path) -> Result<std::fs::Metadata, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "selected source escapes the workspace root".to_owned())?;
    let components: Vec<_> = relative.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("selected source has a non-normal workspace-relative path".to_owned());
    }

    let mut current = root.to_path_buf();
    let mut final_metadata = None;
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            unreachable!("components were validated above")
        };
        current.push(name);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| format!("cannot inspect selected source path: {error}"))?;
        if metadata_is_link_like(&metadata) {
            return Err(format!(
                "selected source path contains a symlink or reparse point: {}",
                current.display()
            ));
        }
        if index + 1 == components.len() {
            if !metadata.file_type().is_file() {
                return Err("selected source is not a regular file".to_owned());
            }
            final_metadata = Some(metadata);
        } else if !metadata.file_type().is_dir() {
            return Err("selected source ancestor is not a directory".to_owned());
        }
    }

    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| format!("cannot canonicalize workspace root: {error}"))?;
    let canonical_path = std::fs::canonicalize(path)
        .map_err(|error| format!("cannot canonicalize selected source: {error}"))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err("selected source resolves outside the workspace root".to_owned());
    }
    final_metadata.ok_or_else(|| "selected source metadata is unavailable".to_owned())
}

fn same_file_snapshot(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn open_source_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Opening a FIFO for reading can block before its type can be checked.
        // `O_NONBLOCK` is inert for regular files and closes that denial-of-
        // service path; `O_NOFOLLOW` rejects a linked leaf.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata_is_link_like(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "selected source is not a regular non-link file",
        ));
    }
    Ok(file)
}

fn source_preflight(
    task: &ScanTask,
    root: &Path,
    changed_only: bool,
) -> Result<SourcePreflight, String> {
    if changed_only {
        let bytes = staged_blob_size(root, &task.rel)?;
        let bytes = usize::try_from(bytes)
            .map_err(|_| format!("source is too large for this platform: {bytes} B"))?;
        Ok(SourcePreflight {
            bytes,
            worktree_metadata: None,
            worktree_identity: None,
        })
    } else {
        let metadata = workspace_regular_file_metadata(root, &task.path)?;
        let bytes = metadata.len();
        let bytes = usize::try_from(bytes)
            .map_err(|_| format!("source is too large for this platform: {bytes} B"))?;
        Ok(SourcePreflight {
            bytes,
            worktree_metadata: Some(metadata),
            worktree_identity: Some(
                same_file::Handle::from_path(&task.path)
                    .map_err(|error| format!("cannot identify selected source: {error}"))?,
            ),
        })
    }
}

fn read_worktree_source(
    root: &Path,
    path: &Path,
    preflight: &std::fs::Metadata,
    preflight_identity: &same_file::Handle,
    max_bytes: usize,
) -> Result<String, String> {
    use std::io::Read as _;

    let before_open = workspace_regular_file_metadata(root, path)?;
    let before_identity = same_file::Handle::from_path(path)
        .map_err(|error| format!("cannot re-identify selected source: {error}"))?;
    if &before_identity != preflight_identity || !same_file_snapshot(preflight, &before_open) {
        return Err("selected source changed after size preflight".to_owned());
    }
    let mut file = open_source_no_follow(path)
        .map_err(|error| format!("cannot open selected source: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("cannot inspect opened source handle: {error}"))?;
    let opened_identity = same_file::Handle::from_file(
        file.try_clone()
            .map_err(|error| format!("cannot clone opened source handle: {error}"))?,
    )
    .map_err(|error| format!("cannot identify opened source handle: {error}"))?;
    if !opened.file_type().is_file()
        || metadata_is_link_like(&opened)
        || &opened_identity != preflight_identity
        || !same_file_snapshot(preflight, &opened)
    {
        return Err("opened source does not match the preflighted regular file".to_owned());
    }
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; CAPTURE_READ_CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|error| format!("cannot read selected source: {error}"))?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(bytes.len());
        if read > remaining {
            return Err(format!(
                "source exceeded the per-file scan budget of {} B while being read",
                max_bytes
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let after_read = file
        .metadata()
        .map_err(|error| format!("cannot recheck opened source handle: {error}"))?;
    let current_path = workspace_regular_file_metadata(root, path)?;
    let current_identity = same_file::Handle::from_path(path)
        .map_err(|error| format!("cannot re-identify selected source after read: {error}"))?;
    if opened_identity != current_identity
        || !same_file_snapshot(&opened, &after_read)
        || !same_file_snapshot(&opened, &current_path)
        || opened.len() != bytes.len() as u64
    {
        return Err("selected source changed while it was being scanned".to_owned());
    }
    String::from_utf8(bytes).map_err(|_| "selected source is not valid UTF-8".to_owned())
}

fn scan_task(
    prepared: &PreparedScanTask<'_>,
    root: &Path,
    changed_only: bool,
    report_only: bool,
    policy: &Policy,
    limits: ScanLimits,
) -> FileScan {
    let task = prepared.task;
    let content = if changed_only {
        read_staged_blob(root, &task.rel, limits.file_bytes)
    } else {
        let Some(metadata) = prepared.worktree_metadata.as_ref() else {
            return failed_scan(
                prepared.ordinal,
                &task.rel,
                "worktree source preflight metadata is missing",
            );
        };
        let Some(identity) = prepared.worktree_identity.as_ref() else {
            return failed_scan(
                prepared.ordinal,
                &task.rel,
                "worktree source preflight identity is missing",
            );
        };
        read_worktree_source(root, &task.path, metadata, identity, limits.file_bytes)
    };
    let content = match content {
        Ok(content) => content,
        Err(error) => return failed_scan(prepared.ordinal, &task.rel, error),
    };
    if content.len() != prepared.expected_bytes {
        return failed_scan(
            prepared.ordinal,
            &task.rel,
            format!(
                "source changed during CI scan (preflight {} B, read {} B); retry from a stable workspace",
                prepared.expected_bytes,
                content.len()
            ),
        );
    }

    let floor = pre_write_floor_decision(&task.rel, &content);
    let findings = if report_only {
        let mut findings = Vec::new();
        if floor.block {
            findings.push(floor);
        }
        for decision in scan_content_findings_with_context(&task.rel, &content, policy, task.ctx) {
            if !findings
                .iter()
                .any(|existing: &Decision| existing.clause == decision.clause)
            {
                findings.push(decision);
            }
        }
        findings
    } else {
        let decision = if floor.block {
            floor
        } else {
            scan_content_with_context(&task.rel, &content, policy, task.ctx)
        };
        if decision.block {
            vec![decision]
        } else {
            Vec::new()
        }
    };
    FileScan {
        ordinal: prepared.ordinal,
        rel: task.rel.clone(),
        scanned: true,
        findings,
        diagnostic: None,
    }
}

fn scan_tasks(
    tasks: &[ScanTask],
    root: &Path,
    changed_only: bool,
    report_only: bool,
    policy: &Policy,
    limits: ScanLimits,
) -> Vec<FileScan> {
    if tasks.is_empty() {
        return Vec::new();
    }

    let mut prepared = Vec::with_capacity(tasks.len());
    let mut scans = Vec::new();
    let mut selected_bytes = 0_u64;
    let mut total_budget_exhausted = false;
    for (ordinal, task) in tasks.iter().enumerate() {
        if total_budget_exhausted {
            scans.push(failed_scan(
                ordinal,
                &task.rel,
                format!(
                    "total source scan budget of {} B was already exhausted",
                    limits.total_bytes
                ),
            ));
            continue;
        }
        let preflight = match source_preflight(task, root, changed_only) {
            Ok(preflight) => preflight,
            Err(error) => {
                scans.push(failed_scan(ordinal, &task.rel, error));
                continue;
            }
        };
        let bytes = preflight.bytes;
        if bytes > limits.file_bytes {
            scans.push(failed_scan(
                ordinal,
                &task.rel,
                format!(
                    "source is {bytes} B; per-file scan budget is {} B",
                    limits.file_bytes
                ),
            ));
            continue;
        }
        let Some(next_total) = selected_bytes.checked_add(bytes as u64) else {
            scans.push(failed_scan(
                ordinal,
                &task.rel,
                "total source byte count overflowed",
            ));
            total_budget_exhausted = true;
            continue;
        };
        if next_total > limits.total_bytes {
            scans.push(failed_scan(
                ordinal,
                &task.rel,
                format!(
                    "scanning this {bytes} B source would exceed the total scan budget of {} B",
                    limits.total_bytes
                ),
            ));
            total_budget_exhausted = true;
            continue;
        }
        selected_bytes = next_total;
        prepared.push(PreparedScanTask {
            ordinal,
            task,
            expected_bytes: bytes,
            worktree_metadata: preflight.worktree_metadata,
            worktree_identity: preflight.worktree_identity,
        });
    }
    if prepared.is_empty() {
        scans.sort_by_key(|scan| scan.ordinal);
        return scans;
    }

    let workers = limits.worker_count(prepared.len());
    let chunk_size = prepared.len().div_ceil(workers);

    let mut completed = std::thread::scope(|scope| {
        let handles: Vec<_> = prepared
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|prepared| {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                scan_task(prepared, root, changed_only, report_only, policy, limits)
                            }))
                            .unwrap_or_else(|_| {
                                failed_scan(
                                    prepared.ordinal,
                                    &prepared.task.rel,
                                    "governance scanner panicked",
                                )
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut scans = Vec::with_capacity(tasks.len());
        for handle in handles {
            if let Ok(mut chunk) = handle.join() {
                scans.append(&mut chunk);
            }
        }
        scans
    });
    scans.append(&mut completed);
    scans.sort_by_key(|scan| scan.ordinal);
    scans
}

/// Run the CI governance scan. Prints findings to stdout and returns the
/// summary. Exit code is 1 when `failed` is true (the caller maps this).
///
/// # Errors
/// Returns an error only on a filesystem traversal failure.
pub fn run(opts: &CiOptions) -> std::io::Result<CiResult> {
    run_with_scan_limits(opts, DEFAULT_SCAN_LIMITS)
}

fn run_with_scan_limits(opts: &CiOptions, limits: ScanLimits) -> std::io::Result<CiResult> {
    limits.validate()?;
    let policy = Policy::load(&opts.project_root);
    // The run's own governance context — READ IT, don't assume. `umadev ci` is the surface
    // that actually BLOCKS (the PreToolUse hook downgrades every non-floor finding to a
    // pass, and `install --base pre-commit` writes `umadev ci --changed-only` into
    // `.git/hooks/pre-commit`), so a decision the run already honoured and a decision this
    // gate makes MUST be the same decision. Judging with a hardcoded `unknown()` context is
    // what let a user say "our brand is violet", watch the run accept it, and then be unable
    // to COMMIT it: the pre-commit hook blocked UD-CODE-002 on the very color they asked
    // for, with no way to converge — the finding just moved one surface over.
    //
    // Resolved PER FILE, not once for the scan root: git runs its hooks with the cwd set to
    // the repository TOP LEVEL, so in a monorepo (`/repo/apps/web/.umadev/`) the scan root is
    // `/repo` — which carries no `.umadev/` at all — while the files being judged live inside
    // a real UmaDev workspace one level down. A root-only lookup finds nothing there, falls
    // back to `unknown()`, and reproduces the exact unconvergeable block this reader exists to
    // prevent. So each file is judged by the context of the nearest workspace that CONTAINS
    // it (memoized per directory; the single-workspace case resolves once and costs nothing).
    let mut contexts = ContextCache::new(&opts.project_root);
    let selection =
        collect_source_files_with_limits(&opts.project_root, opts.changed_only, limits)?;
    let mut result = CiResult {
        files_selected: selection.files.len(),
        scan_scope: selection.scope,
        ..Default::default()
    };

    let tasks: Vec<_> = selection
        .files
        .iter()
        .map(|file| ScanTask {
            path: file.clone(),
            rel: normalized_relative_path(&opts.project_root, file),
            ctx: contexts.for_file(file),
        })
        .collect();
    for scan in scan_tasks(
        &tasks,
        &opts.project_root,
        opts.changed_only,
        opts.report_only,
        &policy,
        limits,
    ) {
        if let Some(diagnostic) = scan.diagnostic {
            result.scan_failures += 1;
            let label = if opts.report_only { "WARN" } else { "BLOCK" };
            println!("{label:<5}  UD-CI-RESOURCE  {}  {diagnostic}", scan.rel);
            continue;
        }
        if !scan.scanned {
            continue;
        }
        result.files_scanned += 1;
        if !scan.findings.is_empty() {
            result.files_blocked += 1;
            result.governance_findings += scan.findings.len();
            for decision in scan.findings {
                let label = finding_label(opts.report_only);
                println!(
                    "{label:<5}  {}  {}  {}",
                    decision.clause,
                    scan.rel,
                    finding_summary(&decision.reason),
                );
            }
        }
    }

    // UD-SEC-016: run `npm audit` if a package-lock.json is present, to catch
    // known-vulnerable dependencies (OWASP A06). Best-effort: if npm isn't
    // installed or the audit fails, skip silently (the file scan still ran).
    //
    // NOT in `--changed-only` mode (the pre-commit gate): a dependency audit judges the
    // WHOLE lockfile, so a PRE-EXISTING transitive CVE - unrelated to the staged change -
    // would fail-CLOSE every commit until it is patched upstream (possibly never),
    // contradicting the changed-only contract ("judge only the staged change"). The full
    // `umadev ci` still runs it.
    if !opts.changed_only && opts.project_root.join("package-lock.json").exists() {
        if let Ok(audit_result) = npm_audit(&opts.project_root) {
            if audit_result.critical + audit_result.high > 0 {
                result.npm_audit_findings = audit_result.critical + audit_result.high;
                let label = finding_label(opts.report_only);
                println!(
                    "{label:<5}  UD-SEC-016  package.json  {} critical, {} high vulnerabilities in dependencies",
                    audit_result.critical, audit_result.high,
                );
            } else if audit_result.total() > 0 {
                println!(
                    "WARN   UD-SEC-016  {} lower-severity vulnerabilities (moderate/low) in dependencies",
                    audit_result.moderate + audit_result.low,
                );
            }
        }
    }

    result.failed = (result.governance_findings > 0
        || result.npm_audit_findings > 0
        || result.scan_failures > 0)
        && !opts.report_only;
    println!("{}", scan_summary(&result, opts.report_only));
    Ok(result)
}

/// The run's persisted governance [`ProjectContext`] for `root` —
/// `.umadev/governance-context.json`, written by the agent runner and read by the
/// PreToolUse hook.
///
/// It carries what the RUN already established and the user already decided: whether the
/// project is a proven static frontend (so server-surface rules have nothing to guard), and
/// whether the requirement asked for a purple/violet brand (the ONE stand-down of the
/// banned-hue default-reject). `umadev ci` is the surface that actually fails a commit, so
/// it must read the same context every other surface reads — a gate that judges by a
/// different rule book than the run is unconvergeable by construction.
///
/// **Conservative & fail-open**: no context file, an unreadable one, or malformed JSON →
/// [`ProjectContext::unknown`] (full strictness). Governance is never relaxed because we
/// *couldn't read* the context.
///
/// **And never relaxed by a context we cannot attribute.** The file is a *permission*, and
/// a permission belongs to a requirement. `umadev ci` runs long after the run that wrote it
/// — from `.git/hooks/pre-commit`, on whatever the tree is now — so it checks the context's
/// provenance against the workspace's live requirement before honouring it
/// ([`ProjectContext::if_current`]): a `purple_allowed: true` left behind by last quarter's
/// violet rebrand must not stand the banned-hue band down for today's "no purple anywhere".
/// A context that still matches the current requirement is honoured whatever its age — that
/// is the legitimately-purple project, and blocking it is the very bug this reader exists
/// to avoid.
fn load_project_context(root: &Path) -> ProjectContext {
    let Ok(raw) = umadev_state::fs::read_bounded_beneath(
        root,
        Path::new(".umadev/governance-context.json"),
        GOVERNANCE_CONTEXT_MAX_BYTES,
    ) else {
        return ProjectContext::unknown();
    };
    let ctx = serde_json::from_slice::<ProjectContext>(&raw).unwrap_or_else(|_| {
        // Malformed / partial JSON → strict.
        ProjectContext::unknown()
    });
    ctx.if_current(now_secs(), workspace_requirement(root).as_deref())
}

/// Per-file governance-context resolution, memoized by directory.
///
/// The scan root is NOT necessarily the UmaDev workspace. `install --base pre-commit` writes
/// `umadev ci --changed-only` into `.git/hooks/pre-commit`, and git runs hooks with the cwd
/// set to the repository TOP LEVEL — so in a monorepo whose workspace is `/repo/apps/web`,
/// this gate runs at `/repo`, where there is no `.umadev/` at all. Judging `apps/web`'s files
/// with the resulting `unknown()` context is the same unconvergeable block as writing no
/// context: the run accepted the brand the commit gate now refuses.
///
/// So a file is judged by the nearest workspace that CONTAINS it: walk up from the file's own
/// directory, stopping at the scan root, and take the first ancestor with a `.umadev/` dir.
/// Nothing found → the scan root's own context (which is `unknown()` when it has none — the
/// strict default, unchanged).
struct ContextCache<'a> {
    root: &'a Path,
    by_dir: std::collections::HashMap<PathBuf, ProjectContext>,
}

impl<'a> ContextCache<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            by_dir: std::collections::HashMap::new(),
        }
    }

    /// The context governing `file`. Memoized per directory, so the common
    /// single-workspace scan does exactly one lookup.
    fn for_file(&mut self, file: &Path) -> ProjectContext {
        let dir = file.parent().unwrap_or(self.root).to_path_buf();
        if let Some(hit) = self.by_dir.get(&dir) {
            return *hit;
        }
        let ctx = self.resolve(&dir);
        self.by_dir.insert(dir, ctx);
        ctx
    }

    /// First ancestor of `dir` (up to and including the scan root) that carries a `.umadev/`
    /// directory; the scan root's own context when none does.
    fn resolve(&self, dir: &Path) -> ProjectContext {
        let mut at = Some(dir);
        while let Some(cur) = at {
            if umadev_state::fs::real_dir(&cur.join(".umadev")) {
                return load_project_context(cur);
            }
            if cur == self.root {
                break;
            }
            at = cur.parent();
        }
        load_project_context(self.root)
    }
}

/// The requirement this workspace is currently being built from
/// (`.umadev/workflow-state.json`), or `None` when no run has recorded one (a hand-written
/// repo, a fresh clone). `None` means "nothing to match against" — the context then falls
/// back to its age ([`ProjectContext::MAX_UNMATCHED_AGE_SECS`]) rather than being trusted
/// forever. Fail-open: an unreadable / corrupt state file reads as `None`.
fn workspace_requirement(root: &Path) -> Option<String> {
    let bytes = umadev_state::fs::read_bounded_beneath(
        root,
        Path::new(".umadev/workflow-state.json"),
        WORKFLOW_STATE_MAX_BYTES,
    )
    .ok()?;
    serde_json::from_slice::<umadev_agent::state::WorkflowState>(&bytes)
        .ok()
        .map(|state| state.requirement)
        .filter(|r| !r.trim().is_empty())
}

/// UNIX seconds, or 0 when the clock is unreadable (which ages every unmatched context out
/// — the strict direction).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Render an honest summary of scan scope and counting semantics.
fn scan_summary(result: &CiResult, report_only: bool) -> String {
    let unscanned = result.files_selected.saturating_sub(result.files_scanned);
    let audit = if result.npm_audit_findings == 0 {
        String::new()
    } else {
        format!(
            " {} high/critical npm-audit finding(s) reported separately.",
            result.npm_audit_findings
        )
    };
    let mode = if report_only {
        " Report-only mode: findings do not change the exit status."
    } else {
        ""
    };
    let governance = if report_only {
        format!(
            "{} file(s) with a governance hit, {} governance finding(s). The count is complete across all enabled content rules.",
            result.files_blocked, result.governance_findings
        )
    } else {
        format!(
            "{} file(s) with a governance hit, {} first-hit finding(s). The enforcing gate stops after the first hit per file; run --report-only for the complete count.",
            result.files_blocked, result.governance_findings
        )
    };
    format!(
        "\nUmaDev scope: {}.\n\
         UmaDev excluded: untracked ignored files (full Git-worktree scope), unsupported types, generated/vendor, symlink/reparse paths, and non-allowlisted dot-directories.\n\
         UmaDev policy: path exclusions apply after the irreversible security floor.\n\
         UmaDev: {} file(s) selected, {} scanned, {} unscanned, {} scan failure(s); \
         {governance}{audit}{mode}",
        result.scan_scope.description(),
        result.files_selected,
        result.files_scanned,
        unscanned,
        result.scan_failures,
    )
}

fn finding_label(report_only: bool) -> &'static str {
    if report_only {
        "HIT"
    } else {
        "BLOCK"
    }
}

/// Keep the first diagnostic sentence without treating a dot in `file.rs`,
/// `package.json`, or a decimal as the sentence boundary.
fn finding_summary(reason: &str) -> &str {
    let first_line = reason.lines().next().unwrap_or("finding").trim();
    first_line
        .find(". ")
        .map_or(first_line, |boundary| &first_line[..boundary])
}

/// Result of an `npm audit --json` scan.
#[derive(Debug, Default)]
pub struct NpmAuditResult {
    pub critical: usize,
    pub high: usize,
    pub moderate: usize,
    pub low: usize,
}

impl NpmAuditResult {
    fn total(&self) -> usize {
        self.critical + self.high + self.moderate + self.low
    }
}

/// How long to wait for `npm audit` before giving up. `npm audit` reaches out
/// to the registry and can stall indefinitely (a hung registry, a proxy, a
/// broken lockfile). 60s is generous for a real audit yet bounds a stuck one.
const NPM_AUDIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const NPM_AUDIT_OUTPUT_MAX_BYTES: usize = 8 * 1024 * 1024;
const CAPTURE_READER_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const CAPTURE_STDERR_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct CapturedCommandOutput {
    status: Option<std::process::ExitStatus>,
    stdout: Vec<u8>,
    timed_out: bool,
    stdout_truncated: bool,
}

/// Run `cmd` with a hard wall-clock deadline, a hard retained-output limit, and
/// whole-process-tree ownership. Stdout is continuously drained so a flooding
/// producer cannot deadlock, but only `max_stdout_bytes` are retained.
/// The tree is terminated even after the direct child exits, closing pipes held
/// by background descendants before the bounded reader grace is awaited.
fn run_capturing_with_timeout(
    cmd: std::process::Command,
    timeout: std::time::Duration,
    max_stdout_bytes: usize,
) -> std::io::Result<CapturedCommandOutput> {
    let output = umadev_process::run_bounded_std_command(
        cmd,
        umadev_process::BoundedCommandOptions {
            timeout,
            stdout_bytes: max_stdout_bytes,
            stderr_bytes: CAPTURE_STDERR_MAX_BYTES,
            reader_grace: CAPTURE_READER_GRACE,
        },
    )?;
    Ok(CapturedCommandOutput {
        status: output.status,
        stdout: output.stdout,
        timed_out: output.timed_out,
        // Callers parse stdout as a complete protocol value. Any truncated
        // pipe means the helper exceeded its execution contract and must be
        // rejected rather than partially trusted.
        stdout_truncated: output.stdout_truncated || output.stderr_truncated,
    })
}

/// Run `npm audit --json` and count vulnerabilities by severity (UD-SEC-016).
/// Returns an error only if npm isn't available or the command can't be
/// spawned; a successful run with zero vulns returns an all-zero result.
///
/// **Bounded + fail-open.** The subprocess is capped at [`NPM_AUDIT_TIMEOUT`]
/// and [`NPM_AUDIT_OUTPUT_MAX_BYTES`]. A timeout skips the optional audit; a
/// truncated or malformed response is an error and is never reported as clean.
fn npm_audit(project_root: &Path) -> std::io::Result<NpmAuditResult> {
    let mut cmd = umadev_host::std_command("npm");
    cmd.args(["audit", "--json"]).current_dir(project_root);
    // npm audit exits non-zero when vulns are found, but stdout still has JSON.
    let output = run_capturing_with_timeout(cmd, NPM_AUDIT_TIMEOUT, NPM_AUDIT_OUTPUT_MAX_BYTES)?;
    if output.timed_out {
        return Ok(NpmAuditResult::default());
    }
    if output.stdout_truncated {
        return Err(std::io::Error::other(format!(
            "npm audit JSON exceeded the {} B capture budget",
            NPM_AUDIT_OUTPUT_MAX_BYTES
        )));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| std::io::Error::other("npm audit returned non-UTF-8 JSON"))?;
    parse_npm_audit(&text).ok_or_else(|| std::io::Error::other("npm audit returned invalid JSON"))
}

/// Parse `npm audit --json` output into a severity-count summary.
/// Handles both npm 7+ format (top-level `vulnerabilities` map) and the
/// legacy `metadata.vulnerabilities` format.
fn parse_npm_audit(text: &str) -> Option<NpmAuditResult> {
    let val: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut result = NpmAuditResult::default();
    // npm 7+: top-level "vulnerabilities" object with per-advisory "severity".
    if let Some(vulns) = val.get("vulnerabilities").and_then(|v| v.as_object()) {
        for (_, info) in vulns {
            let severity = info.get("severity").and_then(|s| s.as_str()).unwrap_or("");
            match severity {
                "critical" => result.critical += 1,
                "high" => result.high += 1,
                "moderate" => result.moderate += 1,
                "low" => result.low += 1,
                _ => {}
            }
        }
        return Some(result);
    }
    // Legacy: "metadata.vulnerabilities" with counts.
    if let Some(meta) = val.get("metadata").and_then(|m| m.get("vulnerabilities")) {
        let get = |k: &str| meta.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        result.critical = usize::try_from(get("critical")).unwrap_or(0);
        result.high = usize::try_from(get("high")).unwrap_or(0);
        result.moderate = usize::try_from(get("moderate")).unwrap_or(0);
        result.low = usize::try_from(get("low")).unwrap_or(0);
        return Some(result);
    }
    None
}

/// Select governance-eligible files with an explicit, reproducible scope.
fn collect_source_files(root: &Path, changed_only: bool) -> std::io::Result<FileSelection> {
    collect_source_files_with_limits(root, changed_only, DEFAULT_SCAN_LIMITS)
}

fn collect_source_files_with_limits(
    root: &Path,
    changed_only: bool,
    limits: ScanLimits,
) -> std::io::Result<FileSelection> {
    if changed_only {
        return Ok(FileSelection {
            files: git_changed_files_with_limits(root, limits)?,
            scope: CiScanScope::StagedIndex,
        });
    }
    if let Some(files) = git_worktree_files(root, limits)? {
        return Ok(FileSelection {
            files,
            scope: CiScanScope::GitWorktree,
        });
    }
    let mut files = Vec::new();
    walk_dir(root, &mut files, limits)?;
    sort_scan_files(root, &mut files);
    Ok(FileSelection {
        files,
        scope: CiScanScope::FilesystemFallback,
    })
}

fn selection_resource_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(format!("UD-CI-RESOURCE: {}", message.into()))
}

fn normalized_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn sort_scan_files(root: &Path, files: &mut Vec<PathBuf>) {
    files.sort_by_key(|path| normalized_relative_path(root, path));
    files.dedup();
}

const GIT_CONTROL_FILE_MAX_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone)]
struct TrustedGitRepository {
    worktree: PathBuf,
    git_dir: PathBuf,
    index_file: Option<PathBuf>,
}

fn canonical_real_directory(path: &Path, label: &str) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        selection_resource_error(format!(
            "cannot resolve {label} {}: {error}",
            path.display()
        ))
    })?;
    if !umadev_state::fs::real_dir(&canonical) {
        return Err(selection_resource_error(format!(
            "{label} is not a real directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn read_git_control_file(root: &Path, relative: &Path) -> std::io::Result<Vec<u8>> {
    umadev_state::fs::read_bounded_beneath(root, relative, GIT_CONTROL_FILE_MAX_BYTES).map_err(
        |error| {
            selection_resource_error(format!(
                "cannot safely read Git control file {}: {error}",
                root.join(relative).display()
            ))
        },
    )
}

fn parse_git_dir_file(worktree: &Path) -> std::io::Result<PathBuf> {
    let bytes = read_git_control_file(worktree, Path::new(".git"))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| selection_resource_error("the .git control file is not valid UTF-8"))?;
    let value = text
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.chars().any(|c| matches!(c, '\r' | '\n')))
        .ok_or_else(|| selection_resource_error("the .git control file is malformed"))?;
    let value = Path::new(value);
    let candidate = if value.is_absolute() {
        value.to_path_buf()
    } else {
        worktree.join(value)
    };
    canonical_real_directory(&candidate, "Git directory")
}

fn discover_git_repository(
    root: &Path,
    honour_index_environment: bool,
) -> std::io::Result<Option<TrustedGitRepository>> {
    let worktree = canonical_real_directory(root, "CI project root")?;
    let dot_git = worktree.join(".git");
    let dot_git_metadata = match std::fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(selection_resource_error(format!(
                "cannot inspect {}: {error}",
                dot_git.display()
            )))
        }
    };
    if metadata_is_link_like(&dot_git_metadata) {
        return Err(selection_resource_error(
            "the repository .git entry is a symlink or reparse point",
        ));
    }
    let git_dir = if dot_git_metadata.file_type().is_dir() {
        canonical_real_directory(&dot_git, "Git directory")?
    } else if dot_git_metadata.file_type().is_file() {
        parse_git_dir_file(&worktree)?
    } else {
        return Err(selection_resource_error(
            "the repository .git entry is neither a directory nor a gitdir file",
        ));
    };

    let commondir_path = git_dir.join("commondir");
    let common_dir = match std::fs::symlink_metadata(&commondir_path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata_is_link_like(&metadata) => {
            let bytes = read_git_control_file(&git_dir, Path::new("commondir"))?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| selection_resource_error("Git commondir is not valid UTF-8"))?;
            let value = text.trim();
            if value.is_empty() || value.chars().any(|c| matches!(c, '\r' | '\n')) {
                return Err(selection_resource_error("Git commondir is malformed"));
            }
            let value = Path::new(value);
            let candidate = if value.is_absolute() {
                value.to_path_buf()
            } else {
                git_dir.join(value)
            };
            canonical_real_directory(&candidate, "Git common directory")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
        Ok(_) => {
            return Err(selection_resource_error(
                "Git commondir is not a regular non-link file",
            ))
        }
        Err(error) => {
            return Err(selection_resource_error(format!(
                "cannot inspect Git commondir: {error}"
            )))
        }
    };

    let index_file = if honour_index_environment {
        match std::env::var_os("GIT_INDEX_FILE") {
            Some(raw) => Some(validate_git_index(&worktree, &git_dir, &common_dir, &raw)?),
            None => None,
        }
    } else {
        None
    };
    Ok(Some(TrustedGitRepository {
        worktree,
        git_dir,
        index_file,
    }))
}

fn validate_git_index(
    worktree: &Path,
    git_dir: &Path,
    common_dir: &Path,
    raw: &std::ffi::OsStr,
) -> std::io::Result<PathBuf> {
    let value = Path::new(raw);
    if value.as_os_str().is_empty() {
        return Err(selection_resource_error("GIT_INDEX_FILE is empty"));
    }
    let candidate = if value.is_absolute() {
        value.to_path_buf()
    } else {
        worktree.join(value)
    };
    let metadata = std::fs::symlink_metadata(&candidate).map_err(|error| {
        selection_resource_error(format!(
            "cannot inspect GIT_INDEX_FILE {}: {error}",
            candidate.display()
        ))
    })?;
    if !metadata.file_type().is_file() || metadata_is_link_like(&metadata) {
        return Err(selection_resource_error(
            "GIT_INDEX_FILE is not a regular non-link file",
        ));
    }
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        selection_resource_error(format!(
            "cannot resolve GIT_INDEX_FILE {}: {error}",
            candidate.display()
        ))
    })?;
    if !canonical.starts_with(git_dir) && !canonical.starts_with(common_dir) {
        return Err(selection_resource_error(format!(
            "GIT_INDEX_FILE is outside this repository: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn sanitize_git_environment(command: &mut std::process::Command) {
    // Explicit removals also scrub values a caller may already have assigned
    // to this `Command`; the loop below covers future/unknown `GIT_*` names
    // inherited from the parent process.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_GLOBAL",
        "GIT_EXTERNAL_DIFF",
        "GIT_DIFF_OPTS",
        "GIT_PAGER",
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
    ] {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn trusted_git_command(repo: &TrustedGitRepository) -> std::process::Command {
    let mut command = umadev_host::std_command("git");
    sanitize_git_environment(&mut command);
    if let Some(index_file) = &repo.index_file {
        command.env("GIT_INDEX_FILE", index_file);
    }
    command
        .arg("--no-pager")
        .arg("--literal-pathspecs")
        .arg("--git-dir")
        .arg(&repo.git_dir)
        .arg("--work-tree")
        .arg(&repo.worktree)
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.untrackedCache=false"])
        .args(["-c", "core.preloadIndex=false"])
        .args(["-c", "core.quotePath=false"])
        .args(["-c", "core.excludesFile="])
        .args(["-c", "core.attributesFile="])
        .current_dir(&repo.worktree);
    command
}

/// In a Git worktree, scan tracked files plus untracked files not excluded by
/// standard ignore rules. A non-successful Git command means metadata is not
/// available and selects the bounded filesystem fallback; timeout/truncation is
/// a resource failure and never silently broadens the scan.
fn git_worktree_files(root: &Path, limits: ScanLimits) -> std::io::Result<Option<Vec<PathBuf>>> {
    let Some(repo) = discover_git_repository(root, false)? else {
        return Ok(None);
    };
    let mut probe = trusted_git_command(&repo);
    probe.args(["rev-parse", "--is-inside-work-tree"]);
    let probe = capture_git_path_output(probe, limits, "git rev-parse")?;
    if !probe.status.is_some_and(|status| status.success()) {
        return Ok(None);
    }
    if probe.stdout != b"true\n" && probe.stdout != b"true\r\n" {
        return Err(selection_resource_error(
            "git rev-parse returned an unexpected worktree response",
        ));
    }

    let mut command = trusted_git_command(&repo);
    command
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root);
    let output = capture_git_path_output(command, limits, "git ls-files")?;
    if !output.status.is_some_and(|status| status.success()) {
        return Err(selection_resource_error(
            "git ls-files failed inside a detected worktree",
        ));
    }
    scan_paths_from_git_output(root, &output.stdout, limits).map(Some)
}

/// Iterative, non-symlink-following filesystem fallback with hard traversal and
/// selection budgets. Every read-dir/metadata error fails closed rather than
/// producing an incomplete clean result.
fn walk_dir(root: &Path, files: &mut Vec<PathBuf>, limits: ScanLimits) -> std::io::Result<()> {
    let mut stack = vec![(root.to_path_buf(), 0_usize)];
    let mut entries_seen = 0_usize;
    while let Some((dir, depth)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| {
            selection_resource_error(format!("cannot read {}: {error}", dir.display()))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                selection_resource_error(format!(
                    "cannot enumerate an entry below {}: {error}",
                    dir.display()
                ))
            })?;
            entries_seen = entries_seen.checked_add(1).ok_or_else(|| {
                selection_resource_error("filesystem traversal entry count overflowed")
            })?;
            if entries_seen > limits.walk_entries {
                return Err(selection_resource_error(format!(
                    "filesystem fallback exceeded its {} entry budget",
                    limits.walk_entries
                )));
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                selection_resource_error(format!("cannot inspect {}: {error}", path.display()))
            })?;
            if metadata_is_link_like(&metadata) {
                continue;
            }
            if metadata.file_type().is_dir() {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                if should_skip_directory(name) {
                    continue;
                }
                let next_depth = depth.checked_add(1).ok_or_else(|| {
                    selection_resource_error("filesystem traversal depth overflowed")
                })?;
                if next_depth > limits.walk_depth {
                    return Err(selection_resource_error(format!(
                        "filesystem fallback exceeded its depth budget of {} at {}",
                        limits.walk_depth,
                        path.display()
                    )));
                }
                stack.push((path, next_depth));
            } else if metadata.file_type().is_file() {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if SCAN_EXTENSIONS.contains(&ext.as_str())
                    || is_sensitive_scan_path(&path.to_string_lossy())
                {
                    files.push(path);
                    if files.len() > limits.path_count {
                        return Err(selection_resource_error(format!(
                            "filesystem fallback selected more than {} files",
                            limits.path_count
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn should_skip_directory(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || (name.starts_with('.') && !SCAN_DOT_DIRS.contains(&name))
}

/// Whether a repository-relative path lives under a directory excluded from
/// governance scans. Only parent components are inspected, so a sensitive file
/// such as `.env` is not mistaken for a dot-directory.
fn has_skipped_directory(path: &Path) -> bool {
    path.parent().is_some_and(|parent| {
        parent.components().any(|component| {
            let std::path::Component::Normal(name) = component else {
                return false;
            };
            name.to_str().is_some_and(should_skip_directory)
        })
    })
}

fn is_scan_candidate(path: &str) -> bool {
    let path_obj = Path::new(path);
    if has_skipped_directory(path_obj) {
        return false;
    }
    let ext = path_obj
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    SCAN_EXTENSIONS.contains(&ext.as_str()) || is_sensitive_scan_path(path)
}

fn scan_paths_from_git_output(
    root: &Path,
    output: &[u8],
    limits: ScanLimits,
) -> std::io::Result<Vec<PathBuf>> {
    if output.len() > limits.path_output_bytes {
        return Err(selection_resource_error(format!(
            "Git path output exceeded {} B",
            limits.path_output_bytes
        )));
    }
    let text = std::str::from_utf8(output)
        .map_err(|_| selection_resource_error("Git returned a non-UTF-8 path list"))?;
    let mut files = Vec::new();
    let mut path_count = 0_usize;
    for path in text.split('\0').filter(|path| !path.is_empty()) {
        path_count = path_count
            .checked_add(1)
            .ok_or_else(|| selection_resource_error("Git path count overflowed"))?;
        if path_count > limits.path_count {
            return Err(selection_resource_error(format!(
                "Git returned more than {} paths",
                limits.path_count
            )));
        }
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(selection_resource_error(format!(
                "Git returned a path outside the workspace: {path}"
            )));
        }
        if is_scan_candidate(path) {
            files.push(root.join(relative));
        }
    }
    sort_scan_files(root, &mut files);
    Ok(files)
}

const GIT_PATH_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn capture_git_path_output(
    command: std::process::Command,
    limits: ScanLimits,
    operation: &str,
) -> std::io::Result<CapturedCommandOutput> {
    let capture_bytes = limits
        .path_output_bytes
        .checked_add(1)
        .ok_or_else(|| selection_resource_error("Git path capture budget overflowed"))?;
    let output = run_capturing_with_timeout(command, GIT_PATH_LIST_TIMEOUT, capture_bytes)
        .map_err(|error| selection_resource_error(format!("{operation} failed: {error}")))?;
    if output.timed_out {
        return Err(selection_resource_error(format!("{operation} timed out")));
    }
    if output.stdout_truncated || output.stdout.len() > limits.path_output_bytes {
        return Err(selection_resource_error(format!(
            "{operation} exceeded its {} B path-output budget",
            limits.path_output_bytes
        )));
    }
    Ok(output)
}

/// Get the files in the STAGED index that differ from `HEAD` — the exact set a
/// commit would capture. This powers the pre-commit hook, so it must be the
/// staged scope (`--cached`), NOT the working tree: a `git diff HEAD` would also
/// include unstaged edits, blocking a commit on a violation that isn't part of
/// it. With no commits yet, `--cached` compares against the empty tree (all
/// staged files appear as new). Git failure is fail-closed because returning an
/// empty set would incorrectly certify an unscanned commit as clean.
fn git_changed_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    git_changed_files_with_limits(root, DEFAULT_SCAN_LIMITS)
}

fn git_changed_files_with_limits(root: &Path, limits: ScanLimits) -> std::io::Result<Vec<PathBuf>> {
    // `-c core.quotePath=false` + `-z`: emit NUL-separated, UNQUOTED paths so a
    // staged file with a non-ASCII (`café.tsx`) or spaced name is scanned rather
    // than dropped. At git's default (`core.quotePath=true`) such a path is
    // octal-escaped + double-quoted (`"caf\303\251.tsx"`), so `extension()`
    // yields `tsx"` and it silently falls out of SCAN_EXTENSIONS — a real
    // violation would never be scanned. `-z` also removes the quoting entirely,
    // so the raw path round-trips to `git show :<rel>` in `read_staged_blob`.
    let repo = discover_git_repository(root, true)?.ok_or_else(|| {
        selection_resource_error("--changed-only requires a .git entry at the project root")
    })?;
    let mut command = trusted_git_command(&repo);
    command
        .args([
            "diff",
            "--no-ext-diff",
            "--name-only",
            "-z",
            "--cached",
            "--diff-filter=ACMR",
        ])
        .current_dir(root);
    let output = capture_git_path_output(command, limits, "git diff --cached")?;
    if !output.status.is_some_and(|status| status.success()) {
        return Err(selection_resource_error("git diff --cached failed"));
    }
    scan_paths_from_git_output(root, &output.stdout, limits)
}

const GIT_BLOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const GIT_SIZE_OUTPUT_MAX_BYTES: usize = 128;

fn staged_blob_size(root: &Path, rel: &str) -> Result<u64, String> {
    let repo = discover_git_repository(root, true)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "cannot find the Git repository for staged content".to_owned())?;
    let mut command = trusted_git_command(&repo);
    command
        .args(["cat-file", "-s", &format!(":{rel}")])
        .current_dir(root);
    let output = run_capturing_with_timeout(command, GIT_BLOB_TIMEOUT, GIT_SIZE_OUTPUT_MAX_BYTES)
        .map_err(|error| format!("cannot inspect staged blob size: {error}"))?;
    if output.timed_out {
        return Err("timed out while inspecting staged blob size".to_owned());
    }
    if output.stdout_truncated {
        return Err("staged blob size response exceeded its capture budget".to_owned());
    }
    if !output.status.is_some_and(|status| status.success()) {
        return Err("git could not inspect the staged blob size".to_owned());
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "git returned a non-UTF-8 staged blob size".to_owned())?;
    text.trim()
        .parse::<u64>()
        .map_err(|_| "git returned an invalid staged blob size".to_owned())
}

/// Read staged content through a bounded `git show` capture. The size is
/// preflighted separately for the deterministic run-wide budget, while the
/// capture limit independently closes the race where the index changes between
/// preflight and read.
fn read_staged_blob(root: &Path, rel: &str, max_bytes: usize) -> Result<String, String> {
    let capture_bytes = max_bytes
        .checked_add(1)
        .ok_or_else(|| "staged blob capture budget overflowed".to_owned())?;
    let repo = discover_git_repository(root, true)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "cannot find the Git repository for staged content".to_owned())?;
    let mut command = trusted_git_command(&repo);
    command.args(["show", &format!(":{rel}")]).current_dir(root);
    let output = run_capturing_with_timeout(command, GIT_BLOB_TIMEOUT, capture_bytes)
        .map_err(|error| format!("cannot read staged blob: {error}"))?;
    if output.timed_out {
        return Err("timed out while reading staged blob".to_owned());
    }
    if output.stdout_truncated || output.stdout.len() > max_bytes {
        return Err(format!(
            "staged blob exceeded the per-file scan budget of {max_bytes} B"
        ));
    }
    if !output.status.is_some_and(|status| status.success()) {
        return Err("git could not read the staged blob".to_owned());
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "selected staged blob is not valid UTF-8".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_scans_clean_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("clean.ts"), "export const x: number = 1;").unwrap();
        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.files_blocked, 0);
        assert!(!result.failed);
    }

    /// Run `umadev ci` over `root` and return how many files it blocked.
    fn ci_blocked(root: &Path) -> usize {
        run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: root.to_path_buf(),
        })
        .unwrap()
        .files_blocked
    }

    /// The gate must judge by the SAME rule book the run does — and the DEFAULT run path
    /// must actually write that rule book.
    ///
    /// `umadev ci` is the surface that actually blocks — the PreToolUse hook downgrades every
    /// non-floor finding to a pass, and `install --base pre-commit` writes
    /// `umadev ci --changed-only` into `.git/hooks/pre-commit`. So while the run honoured
    /// "our brand is violet" and wrote the palette, this gate blocked UD-CODE-002 on that
    /// very color and the user COULD NOT COMMIT the brand they asked for. There is no fix
    /// from inside that loop; the finding had just been relocated one surface over.
    ///
    /// The context file was only ever written by the legacy gated walk and the single-shot
    /// runner — never by the DEFAULT director path — so this is driven here through the same
    /// entry point the director loop calls, not a hand-written JSON blob that could pass while
    /// the product path still wrote nothing.
    #[test]
    fn ci_honours_the_runs_governance_context() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // The hue the user asked for, written as the run wrote it. Named hues, so the ONLY
        // rule with anything to say about this file is the banned-hue one — the one the
        // permission governs (a hardcoded-color finding would be a different, still-correct
        // complaint about literals vs tokens).
        let requested_purple = "export const hero = 'linear-gradient(135deg, purple, pink)';";
        std::fs::write(root.join("brand.ts"), requested_purple).unwrap();

        // No context ⇒ default-REJECT: a purple nobody asked for is still caught.
        assert_eq!(
            ci_blocked(root),
            1,
            "with no recorded permission the banned hue still blocks (default-reject)"
        );

        // THE RUN. The same call `director_loop` makes at its door, before it writes a single
        // file — carrying the BRAIN's verdict on the requirement (`color_permission`), which is
        // the only thing that may grant this permission. A word list used to answer it here and
        // leaked on every review round; a run whose brain says "yes, they chose violet" records
        // that, and this gate must read the same decision.
        let requirement = "做一个品牌落地页,主色用紫色 #7c3aed 的渐变";
        let ctx = umadev_agent::planner::persist_project_context_with_color(
            requirement,
            root,
            "brand",
            true,
        );
        assert!(
            ctx.purple_allowed,
            "the run recorded the brain's grant — the context must carry it"
        );

        // …and the commit gate now reads the SAME decision.
        assert_eq!(
            ci_blocked(root),
            0,
            "the user asked for this color and the run agreed — the commit gate cannot be the \
             one surface that says no"
        );

        // A LEGITIMATELY CURRENT context stays honoured however old it gets: the workspace's
        // live requirement still matches the one the permission was derived from.
        let state = umadev_agent::state::WorkflowState {
            requirement: requirement.to_string(),
            ..umadev_agent::state::WorkflowState::new(umadev_spec::Phase::Frontend)
        };
        umadev_agent::state::write_workflow_state(root, &state).unwrap();
        assert_eq!(
            ci_blocked(root),
            0,
            "a context that matches the workspace's own requirement is current — blocking it \
             would re-open the very bug this test exists for"
        );
    }

    /// A permission belongs to the requirement it was derived from. `umadev ci` runs from
    /// `.git/hooks/pre-commit` — long after the run that wrote the context, on whatever the
    /// tree is now — so a `purple_allowed: true` left behind by an OLD run must not stand the
    /// banned-hue band down for a NEW requirement whose first line is "no purple".
    #[test]
    fn a_stale_context_from_a_different_requirement_does_not_stand_the_rule_down() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("brand.ts"),
            "export const hero = 'linear-gradient(135deg, purple, pink)';",
        )
        .unwrap();

        // LAST quarter's run: the brand really was violet, the brain said so, and the run
        // recorded the permission.
        let old = umadev_agent::planner::persist_project_context_with_color(
            "品牌主色用紫色",
            root,
            "brand",
            true,
        );
        assert!(old.purple_allowed);
        assert_eq!(ci_blocked(root), 0, "that run's own commit was fine");

        // THIS quarter's requirement is the opposite — and it is what the workspace is being
        // built from now. The permission on disk belongs to a requirement that is no longer
        // the one in force, so it is not evidence for anything.
        let state = umadev_agent::state::WorkflowState {
            requirement: "重做品牌:不要任何紫色".to_string(),
            ..umadev_agent::state::WorkflowState::new(umadev_spec::Phase::Frontend)
        };
        umadev_agent::state::write_workflow_state(root, &state).unwrap();
        assert_eq!(
            ci_blocked(root),
            1,
            "a permission derived from a DIFFERENT requirement is not a permission for this one"
        );

        // And the new run's own door re-decides it (its brain reads a requirement that forbids
        // the hue), so the band is armed on disk too — not merely ignored at read time.
        let fresh = umadev_agent::planner::persist_project_context_with_color(
            "重做品牌:不要任何紫色",
            root,
            "brand",
            false,
        );
        assert!(!fresh.purple_allowed);
        assert_eq!(ci_blocked(root), 1);

        // THE CARRY-FORWARD, which is what every later surface depends on: the per-tool-call
        // refresh (`persist_project_context`) has no brain and must NEVER invent a permission.
        // For the requirement the door already decided, it reproduces that decision; for any
        // other, it grants nothing.
        let carried =
            umadev_agent::planner::persist_project_context("重做品牌:不要任何紫色", root, "brand");
        assert!(
            !carried.purple_allowed,
            "the refresh carries the door's verdict forward — it does not re-derive one"
        );
        assert_eq!(ci_blocked(root), 1);
    }

    /// Git runs its hooks with the cwd set to the repository TOP LEVEL. In a monorepo whose
    /// UmaDev workspace is `apps/web`, the pre-commit gate therefore runs at `/repo` — where
    /// there is no `.umadev/` at all — while the files it judges live inside a real workspace
    /// one level down. A root-only context lookup finds nothing, falls back to strict, and
    /// blocks the color the run in `apps/web` had accepted: HIGH 1 all over again, one
    /// directory deeper.
    #[test]
    fn a_workspace_in_a_monorepo_subdir_is_still_governed_by_its_own_context() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path(); // the git top-level: no .umadev of its own
        let web = repo.join("apps").join("web");
        std::fs::create_dir_all(&web).unwrap();
        let purple = "export const hero = 'linear-gradient(135deg, purple, pink)';";
        std::fs::write(web.join("brand.ts"), purple).unwrap();

        // Nothing recorded anywhere → strict, as always.
        assert_eq!(ci_blocked(repo), 1);

        // The run happened INSIDE apps/web, and wrote its context there.
        let ctx = umadev_agent::planner::persist_project_context_with_color(
            "做一个品牌落地页,主色用紫色",
            &web,
            "brand",
            true,
        );
        assert!(ctx.purple_allowed);

        // The pre-commit gate still runs at the repo top level — and must find it.
        assert_eq!(
            ci_blocked(repo),
            0,
            "the gate at the git top level must judge apps/web by apps/web's own rule book"
        );

        // A sibling package with NO workspace of its own is still governed strictly: the
        // permission belongs to apps/web, not to the whole monorepo.
        let api = repo.join("apps").join("api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(api.join("theme.ts"), purple).unwrap();
        assert_eq!(
            ci_blocked(repo),
            1,
            "a permission recorded in one package does not leak into its siblings"
        );
    }

    /// An UNSTAMPED context (hand-written, or from a build that predates the provenance
    /// fields) has nothing to date it or attribute it to — so it cannot stand a rule down.
    #[test]
    fn an_unstamped_context_is_not_a_permission() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("brand.ts"),
            "export const hero = 'linear-gradient(135deg, purple, pink)';",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".umadev")).unwrap();
        std::fs::write(
            root.join(".umadev").join("governance-context.json"),
            r#"{"static_frontend_only":false,"purple_allowed":true}"#,
        )
        .unwrap();
        assert_eq!(
            ci_blocked(root),
            1,
            "a permission with no provenance is not honoured — anyone could drop that file in"
        );
    }

    #[test]
    fn ci_context_is_conservative_when_unreadable() {
        // FAIL-OPEN, in the SAFE direction: a malformed / partial context is not a permission.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".umadev")).unwrap();
        for body in ["{ not json", "{}", ""] {
            std::fs::write(root.join(".umadev").join("governance-context.json"), body).unwrap();
            let ctx = load_project_context(root);
            assert!(
                !ctx.purple_allowed,
                "an unreadable context is never a stand-down: {body:?}"
            );
        }
        // …and a missing file, likewise.
        std::fs::remove_file(root.join(".umadev").join("governance-context.json")).unwrap();
        assert!(!load_project_context(root).purple_allowed);
    }

    #[test]
    fn oversized_governance_and_workflow_state_cannot_relax_ci() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("governance-context.json"),
            vec![b'x'; usize::try_from(GOVERNANCE_CONTEXT_MAX_BYTES).unwrap() + 1],
        )
        .unwrap();
        std::fs::write(
            dir.join("workflow-state.json"),
            vec![b'x'; usize::try_from(WORKFLOW_STATE_MAX_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(!load_project_context(tmp.path()).purple_allowed);
        assert!(workspace_requirement(tmp.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn linked_and_fifo_ci_context_inputs_are_rejected_without_blocking() {
        use std::os::unix::fs::symlink;
        use std::time::{Duration, Instant};

        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir(&dir).unwrap();
        let outside = tmp.path().join("outside.json");
        std::fs::write(&outside, r#"{"purple_allowed":true}"#).unwrap();
        symlink(&outside, dir.join("governance-context.json")).unwrap();
        assert!(!load_project_context(tmp.path()).purple_allowed);

        assert!(std::process::Command::new("mkfifo")
            .arg(dir.join("workflow-state.json"))
            .status()
            .unwrap()
            .success());
        let started = Instant::now();
        assert!(workspace_requirement(tmp.path()).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn ci_flags_violation() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bad.tsx"), "<b>🔍</b>").unwrap();
        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 1);
        assert_eq!(result.governance_findings, 1);
        assert!(result.failed);
    }

    #[test]
    fn ci_report_only_does_not_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bad.tsx"), "<b>🔍</b>").unwrap();
        let result = run(&CiOptions {
            report_only: true,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 1);
        assert_eq!(result.governance_findings, 1);
        assert_eq!(finding_label(true), "HIT");
        assert_eq!(finding_label(false), "BLOCK");
        assert!(!result.failed); // report-only → exit 0
    }

    #[test]
    fn finding_summary_preserves_file_extensions_and_line_numbers() {
        assert_eq!(
            finding_summary(
                "UmaDev: deep nesting at `src/app.rs:42` (UG-LINT-004). Extract a helper."
            ),
            "UmaDev: deep nesting at `src/app.rs:42` (UG-LINT-004)"
        );
        assert_eq!(finding_summary("one-line finding"), "one-line finding");
    }

    #[test]
    fn ci_report_only_emits_all_enabled_findings_per_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("bad.ts"),
            "export function echo(value: any) { console.log(value); return value; }",
        )
        .unwrap();
        let result = run(&CiOptions {
            report_only: true,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 1);
        assert!(result.governance_findings >= 2, "{result:?}");
        assert!(!result.failed);
    }

    #[test]
    fn ci_skips_node_modules() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        // A violation inside node_modules must NOT be scanned.
        std::fs::write(tmp.path().join("node_modules/x.tsx"), "<b>🔍</b>").unwrap();
        std::fs::write(tmp.path().join("clean.ts"), "export const x = 1;").unwrap();
        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 0);
        assert!(!result.failed);
    }

    #[test]
    fn ci_respects_disabled_policy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sd_dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&sd_dir).unwrap();
        std::fs::write(
            sd_dir.join("rules.toml"),
            "[disabled]\nclauses = [\"UD-CODE-001\"]\n",
        )
        .unwrap();
        // Emoji is UD-CODE-001 — disabled → should pass.
        std::fs::write(tmp.path().join("bad.tsx"), "<b>🔍</b>").unwrap();
        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 0);
    }

    #[test]
    fn walk_collects_only_source_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("app.ts"), "x").unwrap();
        std::fs::write(tmp.path().join("readme.md"), "x").unwrap();
        std::fs::write(tmp.path().join("data.json"), "x").unwrap();
        let mut files = Vec::new();
        walk_dir(tmp.path(), &mut files, DEFAULT_SCAN_LIMITS).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"app.ts".to_string()));
        assert!(!names.contains(&"readme.md".to_string()));
        assert!(!names.contains(&"data.json".to_string()));
    }

    #[test]
    fn filesystem_fallback_selection_is_sorted_and_explicit() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        std::fs::write(tmp.path().join("z.ts"), "export const z = 1;").unwrap();
        std::fs::write(
            tmp.path().join("nested").join("m.ts"),
            "export const m = 1;",
        )
        .unwrap();
        std::fs::write(tmp.path().join("a.ts"), "export const a = 1;").unwrap();

        let selection = collect_source_files(tmp.path(), false).unwrap();
        assert_eq!(selection.scope, CiScanScope::FilesystemFallback);
        let paths: Vec<String> = selection
            .files
            .iter()
            .map(|path| normalized_relative_path(tmp.path(), path))
            .collect();
        assert_eq!(paths, ["a.ts", "nested/m.ts", "z.ts"]);
    }

    #[test]
    fn selected_and_actually_scanned_counts_are_distinct() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("clean.ts"), "export const clean = 1;").unwrap();
        std::fs::write(tmp.path().join("binary.ts"), [0xff, 0xfe]).unwrap();

        let result = run(&CiOptions {
            report_only: true,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_selected, 2);
        assert_eq!(result.files_scanned, 1);
    }

    #[test]
    fn walk_collects_file_types_with_active_governance_rules() {
        let tmp = tempfile::TempDir::new().unwrap();
        let expected = [
            "workflow.yml",
            "config.yaml",
            "script.sh",
            "script.bash",
            "script.zsh",
            "styles.css",
            "module.mjs",
            "index.html",
        ];
        for name in expected {
            std::fs::write(tmp.path().join(name), "clean fixture").unwrap();
        }

        let mut files = Vec::new();
        walk_dir(tmp.path(), &mut files, DEFAULT_SCAN_LIMITS).unwrap();
        let names: std::collections::HashSet<String> = files
            .iter()
            .map(|file| file.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        for name in expected {
            assert!(names.contains(name), "{name} must be scanned: {names:?}");
        }
    }

    #[test]
    fn ci_skips_generated_out_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("out")).unwrap();
        std::fs::write(
            tmp.path().join("out/generated.mjs"),
            "export const icon = '🚀';",
        )
        .unwrap();
        std::fs::write(tmp.path().join("clean.mjs"), "export const label = 'Save';").unwrap();

        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_scanned, 1, "generated out/ must be skipped");
        assert_eq!(result.files_blocked, 0);
        assert!(!result.failed);
    }

    // --- M2: changed-only uses the STAGED index, not the working tree -------

    /// Run a git command in `dir`; returns false if git is missing/fails.
    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Init a throwaway repo with a committed identity, or `false` if git is
    /// unavailable (the caller then skips — no hard git dependency in tests).
    fn init_repo(dir: &Path) -> bool {
        git(dir, &["init", "-q"])
            && git(dir, &["config", "user.email", "t@t.test"])
            && git(dir, &["config", "user.name", "test"])
    }

    #[test]
    fn git_environment_redirect_child() {
        let Some(root) = std::env::var_os("UMADEV_CI_GIT_ENV_CHILD_ROOT") else {
            return;
        };
        let result = git_changed_files(Path::new(&root));
        if std::env::var_os("UMADEV_CI_GIT_EXPECT_REJECT").is_some() {
            assert!(
                result.is_err(),
                "an external GIT_INDEX_FILE must fail closed"
            );
            return;
        }
        let files = result.unwrap();
        assert!(files.iter().any(|path| path.ends_with("trusted.ts")));
        assert!(files.iter().all(|path| !path.ends_with("attacker.ts")));
    }

    #[test]
    fn staged_scope_ignores_git_repository_environment_redirects() {
        let trusted = tempfile::TempDir::new().unwrap();
        let attacker = tempfile::TempDir::new().unwrap();
        if !init_repo(trusted.path()) || !init_repo(attacker.path()) {
            return;
        }
        std::fs::write(
            trusted.path().join("trusted.ts"),
            "export const trusted = 1;\n",
        )
        .unwrap();
        std::fs::write(
            attacker.path().join("attacker.ts"),
            "export const attacker = 1;\n",
        )
        .unwrap();
        assert!(git(trusted.path(), &["add", "trusted.ts"]));
        assert!(git(attacker.path(), &["add", "attacker.ts"]));

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ci::tests::git_environment_redirect_child",
                "--nocapture",
            ])
            .env("UMADEV_CI_GIT_ENV_CHILD_ROOT", trusted.path())
            .env("GIT_DIR", attacker.path().join(".git"))
            .env("GIT_WORK_TREE", attacker.path())
            .env("GIT_OBJECT_DIRECTORY", attacker.path().join(".git/objects"))
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn staged_scope_preserves_only_a_valid_repository_local_index() {
        let trusted = tempfile::TempDir::new().unwrap();
        if !init_repo(trusted.path()) {
            return;
        }
        std::fs::write(
            trusted.path().join("trusted.ts"),
            "export const trusted = 1;\n",
        )
        .unwrap();
        assert!(git(trusted.path(), &["add", "trusted.ts"]));
        let alternate_index = trusted.path().join(".git/alternate-index");
        std::fs::copy(trusted.path().join(".git/index"), &alternate_index).unwrap();

        let valid = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ci::tests::git_environment_redirect_child",
                "--nocapture",
            ])
            .env("UMADEV_CI_GIT_ENV_CHILD_ROOT", trusted.path())
            .env("GIT_INDEX_FILE", &alternate_index)
            .output()
            .unwrap();
        assert!(
            valid.status.success(),
            "repository-local alternate index was rejected:\n{}",
            String::from_utf8_lossy(&valid.stderr)
        );

        let outside = tempfile::NamedTempFile::new().unwrap();
        let rejected = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ci::tests::git_environment_redirect_child",
                "--nocapture",
            ])
            .env("UMADEV_CI_GIT_ENV_CHILD_ROOT", trusted.path())
            .env("UMADEV_CI_GIT_EXPECT_REJECT", "1")
            .env("GIT_INDEX_FILE", outside.path())
            .output()
            .unwrap();
        assert!(
            rejected.status.success(),
            "external alternate index did not fail closed:\n{}",
            String::from_utf8_lossy(&rejected.stderr)
        );
    }

    #[test]
    fn full_git_scope_includes_tracked_and_untracked_nonignored_in_stable_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return;
        }
        std::fs::write(root.join(".gitignore"), "ignored.ts\nforced.ts\n").unwrap();
        std::fs::write(root.join("tracked.ts"), "export const tracked = 1;\n").unwrap();
        std::fs::write(root.join("forced.ts"), "export const forced = 1;\n").unwrap();
        std::fs::write(root.join("untracked.ts"), "export const local = 1;\n").unwrap();
        std::fs::write(root.join("ignored.ts"), "export const ignored = 1;\n").unwrap();
        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        std::fs::write(root.join(".github/workflows/ci.yml"), "name: CI\n").unwrap();
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".hidden/local.ts"), "export const hidden = 1;\n").unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(
            root.join("node_modules/generated.ts"),
            "export const generated = 1;\n",
        )
        .unwrap();
        assert!(git(root, &["add", "tracked.ts"]));
        assert!(git(root, &["add", "-f", "forced.ts"]));

        let selection = collect_source_files(root, false).unwrap();
        assert_eq!(selection.scope, CiScanScope::GitWorktree);
        let paths: Vec<String> = selection
            .files
            .iter()
            .map(|path| normalized_relative_path(root, path))
            .collect();
        assert_eq!(
            paths,
            [
                ".github/workflows/ci.yml",
                "forced.ts",
                "tracked.ts",
                "untracked.ts",
            ]
        );
    }

    #[test]
    fn changed_only_scans_staged_blob_not_dirty_working_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        let file = root.join("app.tsx");
        // Commit a clean baseline.
        std::fs::write(&file, "export const x = 1;\n").unwrap();
        assert!(git(root, &["add", "app.tsx"]));
        assert!(git(root, &["commit", "-q", "-m", "base"]));
        // STAGE a different but still CLEAN version (so it appears in --cached).
        std::fs::write(&file, "export const y = 2;\n").unwrap();
        assert!(git(root, &["add", "app.tsx"]));
        // Dirty the WORKING TREE with an emoji violation — but do NOT stage it.
        std::fs::write(&file, "<b>\u{1f50d}</b>\n").unwrap();

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        // The staged version is clean → no block, even though the working copy
        // (which the OLD `git diff HEAD` + on-disk read judged) is dirty.
        assert_eq!(
            result.files_blocked, 0,
            "must judge the STAGED blob, not the dirty working copy"
        );
        assert!(!result.failed);
    }

    #[test]
    fn changed_only_scans_new_rule_extensions_and_skips_generated_out() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        std::fs::create_dir(root.join("out")).unwrap();
        std::fs::write(root.join("workflow.yaml"), "name: clean\n").unwrap();
        std::fs::write(
            root.join("out/generated.mjs"),
            "export const icon = '🚀';\n",
        )
        .unwrap();
        assert!(git(root, &["add", "."]));

        let listed = git_changed_files(root).unwrap();
        assert!(
            listed.iter().any(|path| path.ends_with("workflow.yaml")),
            "YAML must enter changed-only governance: {listed:?}"
        );
        assert!(
            listed
                .iter()
                .all(|path| !path.ends_with("out/generated.mjs")),
            "generated out/ must stay outside changed-only governance: {listed:?}"
        );

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.files_blocked, 0);
        assert!(!result.failed);
    }

    #[test]
    fn changed_only_flags_a_violation_in_the_staged_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        let file = root.join("app.tsx");
        std::fs::write(&file, "export const x = 1;\n").unwrap();
        assert!(git(root, &["add", "app.tsx"]));
        assert!(git(root, &["commit", "-q", "-m", "base"]));
        // STAGE a version WITH a violation; clean it up in the working tree.
        std::fs::write(&file, "<b>\u{1f50d}</b>\n").unwrap();
        assert!(git(root, &["add", "app.tsx"]));
        std::fs::write(&file, "export const ok = 3;\n").unwrap(); // clean working copy

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        // The STAGED blob carries the violation → blocked, regardless of the
        // now-clean working copy.
        assert_eq!(result.files_blocked, 1, "staged violation must be flagged");
        assert!(result.failed);
    }

    #[test]
    fn changed_only_blocks_a_staged_dotenv_secret() {
        // REPRODUCTION: a staged `.env` carrying a live Stripe key. `.env` has no
        // source extension, so the OLD collect-by-extension scan never saw it —
        // "0 file(s) scanned, 0 blocked, exit 0". The floor must now pull the
        // sensitive path into scope and BLOCK it (UD-SEC-001 path guard), failing
        // CI with a non-zero exit.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        std::fs::write(
            root.join(".env"),
            "STRIPE_SECRET_KEY=aB3xK9pQ7mNr2WvT5sZ8dF1gH4jL6cE0\n",
        )
        .unwrap();
        assert!(git(root, &["add", ".env"]));

        // Sanity: the sensitive path is now in the changed-file scan set despite
        // having no source extension.
        let listed = git_changed_files(root).unwrap();
        assert!(
            listed.iter().any(|p| p.ends_with(".env")),
            "a staged .env must be listed for scanning, got {listed:?}"
        );

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        assert!(
            result.files_blocked >= 1,
            "a staged .env secret must be blocked, not silently skipped"
        );
        assert!(result.failed, "and it must fail CI (non-zero exit)");
    }

    #[test]
    fn changed_only_blocks_a_secret_in_a_no_extension_file() {
        // A staged `credentials` file (no extension) with a live secret must be
        // scanned + blocked too — the sensitive-path scope is not limited to
        // dotenv files.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        std::fs::write(
            root.join("credentials"),
            "aws_secret_access_key = aB3xK9pQ7mNr2WvT5sZ8dF1gH4jL6cE0\n",
        )
        .unwrap();
        assert!(git(root, &["add", "credentials"]));
        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        assert!(
            result.files_blocked >= 1,
            "a staged credentials secret must block"
        );
        assert!(result.failed);
    }

    #[test]
    fn is_sensitive_scan_path_matches_the_floor_set() {
        // The extension-agnostic scan predicate covers the dotenv/cert/key set …
        assert!(is_sensitive_scan_path(".env"));
        assert!(is_sensitive_scan_path("apps/api/.env.production"));
        assert!(is_sensitive_scan_path(".env.staging")); // arbitrary dotenv variant
        assert!(is_sensitive_scan_path("certs/server.pem"));
        assert!(is_sensitive_scan_path("deploy/id_rsa"));
        assert!(is_sensitive_scan_path("secrets/credentials"));
        assert!(is_sensitive_scan_path(".ssh/known_hosts"));
        // … and does NOT sweep in ordinary source (that rides SCAN_EXTENSIONS).
        assert!(!is_sensitive_scan_path("src/messages.ts"));
        assert!(!is_sensitive_scan_path("README.md"));
        // A dotenv TEMPLATE is in scan SCOPE (any `.env.*`), but the floor's path
        // guard never auto-blocks it — only its content is judged, so a
        // placeholder file passes while a real `.env` is blocked on the path.
        assert!(is_sensitive_scan_path(".env.example"));
        assert!(!check_sensitive_path(".env.example", "").block);
    }

    #[test]
    fn changed_only_scans_non_ascii_staged_filename() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        // A non-ASCII filename: at git's default core.quotePath=true, `git diff
        // --name-only --cached` would emit `"caf\303\251.tsx"` (octal-escaped +
        // quoted), so `extension()` sees `tsx"` and the file drops out of the
        // scan. The -c core.quotePath=false + -z fix must scan it.
        let file = root.join("café.tsx");
        std::fs::write(&file, "<b>\u{1f50d}</b>\n").unwrap(); // emoji violation
        assert!(git(root, &["add", "café.tsx"]));

        // Sanity: git_changed_files must surface the non-ASCII path (unquoted).
        let listed = git_changed_files(root).unwrap();
        assert!(
            listed.iter().any(|p| p.ends_with("café.tsx")),
            "the non-ASCII staged path must be listed unquoted, got {listed:?}"
        );

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        assert_eq!(
            result.files_blocked, 1,
            "a violation in a non-ASCII staged filename must be scanned + blocked"
        );
        assert!(result.failed);
    }

    #[test]
    fn scan_summary_distinguishes_enforcing_and_complete_report_counts() {
        let result = CiResult {
            files_selected: 4,
            files_scanned: 3,
            files_blocked: 1,
            governance_findings: 3,
            npm_audit_findings: 2,
            scan_failures: 1,
            scan_scope: CiScanScope::GitWorktree,
            failed: true,
        };
        let line = scan_summary(&result, true);
        assert!(line.contains("tracked + untracked non-ignored"), "{line}");
        assert!(line.contains("4 file(s) selected, 3 scanned"), "{line}");
        assert!(line.contains("1 unscanned, 1 scan failure(s)"), "{line}");
        assert!(line.contains("1 file(s) with a governance hit"), "{line}");
        assert!(line.contains("3 governance finding(s)"), "{line}");
        assert!(line.contains("count is complete"), "{line}");
        assert!(
            line.contains("2 high/critical npm-audit finding(s)"),
            "{line}"
        );
        assert!(line.contains("Report-only mode"), "{line}");
        let enforcing = scan_summary(&result, false);
        assert!(enforcing.contains("3 first-hit finding(s)"), "{enforcing}");
        assert!(enforcing.contains("run --report-only"), "{enforcing}");
    }

    // --- UD-SEC-016: npm audit parsing ----------------------------------

    #[test]
    fn npm_audit_parses_npm7_format() {
        let json = r#"{"vulnerabilities":{"lodash":{"severity":"high"},"react":{"severity":"critical"},"left-pad":{"severity":"low"}}}"#;
        let result = parse_npm_audit(json).unwrap();
        assert_eq!(result.critical, 1);
        assert_eq!(result.high, 1);
        assert_eq!(result.low, 1);
    }

    #[test]
    fn npm_audit_parses_legacy_format() {
        let json =
            r#"{"metadata":{"vulnerabilities":{"critical":2,"high":3,"moderate":1,"low":0}}}"#;
        let result = parse_npm_audit(json).unwrap();
        assert_eq!(result.critical, 2);
        assert_eq!(result.high, 3);
        assert_eq!(result.moderate, 1);
    }

    #[test]
    fn npm_audit_parses_clean() {
        let json = r#"{"vulnerabilities":{}}"#;
        let result = parse_npm_audit(json).unwrap();
        assert_eq!(result.total(), 0);
    }

    #[test]
    fn npm_audit_returns_none_on_garbage() {
        assert!(parse_npm_audit("not json").is_none());
    }

    // --- bounded npm-audit wait (never hangs CI) ------------------------

    #[cfg(unix)]
    #[test]
    fn capturing_timeout_kills_a_stuck_child_fast() {
        use std::time::{Duration, Instant};
        // A child that would run for 10s is torn down well before its runtime.
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 10");
        let started = Instant::now();
        let out = run_capturing_with_timeout(cmd, Duration::from_millis(200), 1024).unwrap();
        let elapsed = started.elapsed();
        assert!(out.timed_out, "an overrunning helper must report timeout");
        assert!(
            elapsed < Duration::from_secs(3),
            "the wait must be bounded, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capturing_returns_stdout_when_child_exits_in_time() {
        use std::time::Duration;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("printf 'hello-audit'");
        let out = run_capturing_with_timeout(cmd, Duration::from_secs(10), 1024).unwrap();
        assert!(!out.timed_out);
        assert!(!out.stdout_truncated);
        assert_eq!(out.stdout, b"hello-audit");
    }

    #[cfg(unix)]
    #[test]
    fn capturing_closes_pipe_inherited_by_background_descendant() {
        use std::time::{Duration, Instant};

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & printf 'parent-done'");
        let started = Instant::now();
        let out = run_capturing_with_timeout(cmd, Duration::from_secs(5), 1024).unwrap();
        assert!(!out.timed_out, "the direct shell exited normally");
        assert_eq!(out.stdout, b"parent-done");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a descendant holding stdout must not keep the reader at EOF"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capturing_no_newline_flood_is_memory_and_time_bounded() {
        use std::time::{Duration, Instant};

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg("while :; do printf '0123456789abcdef'; done");
        let started = Instant::now();
        let out = run_capturing_with_timeout(cmd, Duration::from_millis(150), 1024).unwrap();
        assert!(out.timed_out);
        assert!(out.stdout_truncated);
        assert_eq!(out.stdout.len(), 1024);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn source_open_rejects_fifo_without_blocking() {
        use std::time::{Duration, Instant};

        let tmp = tempfile::TempDir::new().unwrap();
        let fifo = tmp.path().join("source.ts");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let started = Instant::now();
        assert!(open_source_no_follow(&fifo).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn git_command_scrubs_redirect_and_execution_environment() {
        let mut command = std::process::Command::new("git");
        command
            .env("GIT_DIR", "attacker")
            .env("GIT_WORK_TREE", "attacker")
            .env("GIT_OBJECT_DIRECTORY", "attacker")
            .env("GIT_EXTERNAL_DIFF", "attacker")
            .env("GIT_CONFIG_GLOBAL", "attacker");
        sanitize_git_environment(&mut command);
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_EXTERNAL_DIFF",
        ] {
            assert_eq!(env.get(std::ffi::OsStr::new(key)), Some(&None));
        }
        assert_eq!(
            env.get(std::ffi::OsStr::new("GIT_CONFIG_GLOBAL")),
            Some(&Some(std::ffi::OsStr::new(null_device())))
        );
    }

    fn tiny_scan_limits(max_file_bytes: usize, max_total_bytes: u64) -> ScanLimits {
        ScanLimits {
            file_bytes: max_file_bytes,
            total_bytes: max_total_bytes,
            in_flight_bytes: max_file_bytes + CAPTURE_READ_CHUNK_BYTES,
            workers: 1,
            path_output_bytes: 1024,
            path_count: 64,
            walk_entries: 256,
            walk_depth: 16,
        }
    }

    #[test]
    fn full_scan_fails_closed_on_oversized_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("huge.ts"), vec![b'x'; 65]).unwrap();

        let result = run_with_scan_limits(
            &CiOptions {
                report_only: false,
                changed_only: false,
                project_root: tmp.path().to_path_buf(),
            },
            tiny_scan_limits(64, 1024),
        )
        .unwrap();
        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.scan_failures, 1);
        assert!(
            result.failed,
            "an unscanned selected source must fail closed"
        );
    }

    #[test]
    fn changed_only_fails_closed_on_oversized_staged_blob() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return;
        }
        std::fs::write(root.join("huge.ts"), vec![b'x'; 65]).unwrap();
        assert!(git(root, &["add", "huge.ts"]));

        let result = run_with_scan_limits(
            &CiOptions {
                report_only: false,
                changed_only: true,
                project_root: root.to_path_buf(),
            },
            tiny_scan_limits(64, 1024),
        )
        .unwrap();
        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.scan_failures, 1);
        assert!(result.failed, "an oversized staged blob must fail closed");
    }

    #[test]
    fn full_scan_fails_closed_when_total_budget_is_exhausted() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.ts"), vec![b'a'; 40]).unwrap();
        std::fs::write(tmp.path().join("b.ts"), vec![b'b'; 40]).unwrap();
        std::fs::write(tmp.path().join("c.ts"), vec![b'c'; 40]).unwrap();

        let result = run_with_scan_limits(
            &CiOptions {
                report_only: false,
                changed_only: false,
                project_root: tmp.path().to_path_buf(),
            },
            tiny_scan_limits(64, 80),
        )
        .unwrap();
        assert_eq!(result.files_scanned, 2);
        assert_eq!(result.scan_failures, 1);
        assert!(result.failed);
    }

    #[test]
    fn git_path_listing_fails_closed_at_the_path_count_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return;
        }
        for name in ["a.ts", "b.ts", "c.ts"] {
            std::fs::write(root.join(name), "export const value = 1;\n").unwrap();
        }
        assert!(git(root, &["add", "."]));
        let mut limits = tiny_scan_limits(1024, 4096);
        limits.path_count = 2;
        let error = collect_source_files_with_limits(root, false, limits).unwrap_err();
        assert!(error.to_string().contains("more than 2 paths"), "{error}");
    }

    #[test]
    fn filesystem_fallback_fails_closed_at_depth_and_entry_budgets() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("one/two")).unwrap();
        std::fs::write(tmp.path().join("one/two/deep.ts"), "export const x = 1;\n").unwrap();
        let mut depth_limits = tiny_scan_limits(1024, 4096);
        depth_limits.walk_depth = 1;
        let depth_error =
            collect_source_files_with_limits(tmp.path(), false, depth_limits).unwrap_err();
        assert!(
            depth_error.to_string().contains("depth budget"),
            "{depth_error}"
        );

        let entries_tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(entries_tmp.path().join("a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(entries_tmp.path().join("b.ts"), "export const b = 1;\n").unwrap();
        let mut entry_limits = tiny_scan_limits(1024, 4096);
        entry_limits.walk_entries = 1;
        let entry_error =
            collect_source_files_with_limits(entries_tmp.path(), false, entry_limits).unwrap_err();
        assert!(
            entry_error.to_string().contains("entry budget"),
            "{entry_error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tracked_symlink_to_external_source_is_rejected_without_reading_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return;
        }
        let outside_file = outside.path().join("secret.ts");
        std::fs::write(&outside_file, "export const leaked = '🚀';\n").unwrap();
        std::os::unix::fs::symlink(&outside_file, root.join("linked.ts")).unwrap();
        assert!(git(root, &["add", "linked.ts"]));

        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.files_blocked, 0, "external content must not be read");
        assert_eq!(result.scan_failures, 1);
        assert!(result.failed);
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_fallback_never_descends_through_directory_symlinks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(
            outside.path().join("outside.ts"),
            "export const x = '🚀';\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked-dir")).unwrap();

        let selection =
            collect_source_files_with_limits(tmp.path(), false, DEFAULT_SCAN_LIMITS).unwrap();
        assert!(selection.files.is_empty());
        assert_eq!(selection.scope, CiScanScope::FilesystemFallback);
    }
}
