//! Audit trails — the evidence half of `UMADEV_HOST_SPEC_V1` layer 4.
//!
//! Two append-only logs live at:
//!
//! - `<project_root>/.umadev/audit/frontend-api-calls.jsonl`
//!   (Implements `UD-EVID-001`)
//! - `<project_root>/.umadev/audit/tool-calls.jsonl`
//!   (Implements `UD-EVID-002`)
//!
//! Both are JSONL. Both fail open: a filesystem error here MUST NOT
//! break the host.

use chrono::Utc;
use fs2::FileExt as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const FRONTEND_EXTS: &[&str] = &["tsx", "ts", "jsx", "js", "vue", "svelte", "astro"];

/// Maximum size an audit JSONL may reach before it is rotated. Default
/// 5 MiB — large enough to hold a full delivery's worth of tool/API calls
/// (typically a few hundred KB), small enough that a long-running session
/// can't bloat `.umadev/audit/` without bound. `UMADEV_AUDIT_MAX_BYTES` may
/// lower this ceiling, but cannot disable rotation or raise the hard cap.
const DEFAULT_MAX_JSONL_BYTES: u64 = 5 * 1024 * 1024;
const MIN_JSONL_BYTES: u64 = 4 * 1024;

/// How many rotated archives to keep per audit file. Older archives
/// (`*.jsonl.<n>` beyond this count) are deleted on rotation so the
/// directory stays bounded.
const MAX_ARCHIVES: usize = 3;
const AUDIT_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const AUDIT_LOCK_POLL: Duration = Duration::from_millis(1);
const MAX_AUDIT_TOOL_CHARS: usize = 256;
const MAX_AUDIT_FILE_CHARS: usize = 8 * 1024;
const MAX_AUDIT_REASON_CHARS: usize = 16 * 1024;
const MAX_AUDIT_SESSION_CHARS: usize = 2 * 1024;
const MAX_AUDIT_CLAUSE_CHARS: usize = 128;
const MAX_AUDIT_URL_CHARS: usize = 2 * 1024;
const MAX_AUDIT_URLS: usize = 1024;
const MAX_AUDIT_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// Resolve the rotation threshold from the env override, falling back to
/// the default. `0` disables rotation entirely.
fn max_jsonl_bytes() -> u64 {
    std::env::var("UMADEV_AUDIT_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|value| (MIN_JSONL_BYTES..=DEFAULT_MAX_JSONL_BYTES).contains(value))
        .unwrap_or(DEFAULT_MAX_JSONL_BYTES)
}

/// If `path` exists and exceeds the size threshold, rotate it:
/// `tool-calls.jsonl` → `tool-calls.jsonl.1` (shifting older `.n` up),
/// keeping at most `MAX_ARCHIVES` copies. Best-effort — rotation errors
/// are swallowed (audit must never block the host).
fn rotate_if_needed_locked(root: &umadev_state::fs::RootedDir, path: &Path) {
    let cap = max_jsonl_bytes();
    let Ok(Some(len)) = root.regular_file_len(path) else {
        return;
    };
    if len < cap {
        return;
    }
    // Shift archives: .{MAX-1} is dropped, .{n} → .{n+1}, then current → .1.
    // Walk from the oldest kept slot downward so we don't overwrite an
    // archive we still need to shift.
    for n in (1..MAX_ARCHIVES).rev() {
        let src = archive_path(path, n);
        if root.regular_file_exists(&src).unwrap_or(false) {
            let dst = archive_path(path, n + 1);
            if rooted_regular_file_or_absent(root, &dst) {
                let _ = root.replace_regular_file(&src, &dst);
            }
        }
    }
    // Current file becomes .1.
    let archive1 = archive_path(path, 1);
    if rooted_regular_file_or_absent(root, &archive1) {
        let _ = root.replace_regular_file(path, &archive1);
    }
    // Drop any archive beyond the keep count.
    if MAX_ARCHIVES > 0 {
        let drop_n = MAX_ARCHIVES + 1;
        let beyond = archive_path(path, drop_n);
        let _ = root.remove_regular_file(&beyond);
    }
}

fn rooted_regular_file_or_absent(root: &umadev_state::fs::RootedDir, path: &Path) -> bool {
    root.regular_file_exists(path).is_ok()
}

struct AuditLock {
    _file: std::fs::File,
}

/// Persistent hidden sibling used for an OS-released advisory lock. The file is
/// intentionally not unlinked on drop: unlinking a lock file while a waiter has
/// it open can split future lockers across two inodes.
fn rotate_lock_path(path: &Path) -> PathBuf {
    let base = path
        .file_name()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    path.with_file_name(format!(".{base}.audit-lock"))
}

fn acquire_audit_lock(
    root: &umadev_state::fs::RootedDir,
    path: &Path,
) -> std::io::Result<AuditLock> {
    let lock_path = rotate_lock_path(path);
    let file = root.open_private_lock(&lock_path, false)?;
    let started = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(AuditLock { _file: file }),
            Err(error) if umadev_state::fs::lock_error_is_contention(&error) => {
                if started.elapsed() >= AUDIT_LOCK_TIMEOUT {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "audit trail is busy in another UmaDev process",
                    ));
                }
                std::thread::sleep(AUDIT_LOCK_POLL);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
fn rotate_if_needed(path: &Path) {
    let Some((root, relative)) = test_rooted_file(path) else {
        return;
    };
    let Ok(_lock) = acquire_audit_lock(&root, &relative) else {
        return;
    };
    rotate_if_needed_locked(&root, &relative);
}

/// `tool-calls.jsonl` + `.{n}` → `tool-calls.jsonl.{n}`.
fn archive_path(path: &Path, n: usize) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    name.push('.');
    name.push_str(&n.to_string());
    path.with_file_name(name)
}

/// One audited frontend API call.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApiCallRecord {
    /// Unix seconds.
    pub ts: i64,
    /// Unix milliseconds — sub-second resolution so two calls in the same
    /// second can still be ordered. `#[serde(default)]` keeps old JSONL rows
    /// (pre-4.6, which only had `ts`) deserialisable.
    #[serde(default)]
    pub ts_ms: i64,
    /// Workspace-relative path of the file being written.
    pub file: String,
    /// Host tool name, e.g. `Write` or `Edit`.
    pub tool: String,
    /// Sorted, deduped list of API paths extracted from `content`.
    pub urls: Vec<String>,
    /// Opaque host session identifier; empty when absent.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
}

/// One audited host tool call (a wider trail than just API audit).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Unix seconds.
    pub ts: i64,
    /// Unix milliseconds — sub-second resolution for deterministic ordering
    /// of calls that share a second. `#[serde(default)]` for old rows.
    #[serde(default)]
    pub ts_ms: i64,
    /// Host tool name (e.g. `Write`, `Edit`, `Bash`).
    pub tool: String,
    /// Workspace-relative target file (empty when not applicable).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
    /// Outcome: `allow` | `block` | `warn` | `audit`.
    pub decision: String,
    /// Firing clause id (e.g. `UD-CODE-001`); empty when not gated.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub clause: String,
    /// Human-readable note shown to the model.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// Opaque host session identifier; empty when absent.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
}

fn api_url_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Callers that target an API path. Covers modern patterns:
        // - fetch / axios.METHOD / axios() direct / ky.METHOD / http.METHOD
        // - React Query: useQuery / useMutation / useSWR / useSWRInfinite
        // - Wrapped clients: api.METHOD, httpClient.METHOD, client.METHOD,
        //   request(...), fetcher(...), service.METHOD — common names people
        //   give a typed/SDK wrapper around fetch.
        // The URL must start with `/` and runs to the next quote / ? / # /
        // space / `${` (so template-literal fetch(`/api/${id}`) captures the
        // static prefix `/api/`).
        Regex::new(
            r#"(?x)
                (?:
                    fetch | axios | ky | http
                  | useSWR | useSWRInfinite | useQuery | useMutation
                  | api | httpClient | client | request | fetcher | service
                )
                (?:\.\w+)?
                \s*\(\s*
                ['"`]
                (?P<url>/[^'"`?\#\s$]+)
            "#,
        )
        .expect("api url regex is well-formed")
    })
}

fn ext_of(file_path: &str) -> String {
    file_path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Extract sorted, deduplicated frontend API paths from `content`.
///
/// Implements the extraction half of `UD-CODE-003`. Returns an empty
/// `Vec` for non-frontend file extensions.
#[must_use]
pub fn extract_api_urls(file_path: &str, content: &str) -> Vec<String> {
    let ext = ext_of(file_path);
    if !FRONTEND_EXTS.contains(&ext.as_str()) {
        return Vec::new();
    }
    let mut end = content.len().min(MAX_AUDIT_SOURCE_BYTES);
    while !content.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut urls = BTreeSet::new();
    for cap in api_url_regex().captures_iter(&content[..end]) {
        if let Some(url) = cap.name("url") {
            urls.insert(url.as_str().to_string());
            if urls.len() >= MAX_AUDIT_URLS {
                break;
            }
        }
    }
    urls.into_iter().collect()
}

fn audit_dir(project_root: &Path) -> std::io::Result<umadev_state::fs::RootedDir> {
    let root = umadev_state::fs::RootedDir::open_no_follow(project_root)?;
    root.ensure_dir(Path::new(".umadev"), false)?;
    root.ensure_dir(Path::new(".umadev/audit"), false)?;
    root.open_dir(Path::new(".umadev/audit"))
}

/// Normalize a tool-call decision to the documented vocabulary
/// (`allow` | `block` | `warn` | `audit`). Unknown / empty → `allow`
/// (fail-open: an unrecognized decision must never block the host).
/// Matching is case-insensitive so `BLOCK` / `Block` collapse correctly.
pub(crate) fn normalize_decision(decision: &str) -> String {
    let lower = decision.trim().to_ascii_lowercase();
    match lower.as_str() {
        "block" | "warn" | "audit" => lower,
        _ => "allow".to_string(),
    }
}

fn audit_timestamp(now: Option<i64>) -> (i64, i64) {
    now.map_or_else(
        || {
            let current = Utc::now();
            (current.timestamp(), current.timestamp_millis())
        },
        |seconds| (seconds, seconds.saturating_mul(1_000)),
    )
}

fn sanitize_audit_text(text: &str, max_chars: usize) -> String {
    let redacted = crate::redaction::redact_text(text);
    if redacted.chars().count() <= max_chars {
        return redacted;
    }
    let mut clipped: String = redacted.chars().take(max_chars.saturating_sub(3)).collect();
    clipped.push_str("...");
    clipped
}

fn append_jsonl(
    root: &umadev_state::fs::RootedDir,
    path: &Path,
    line: &str,
) -> std::io::Result<()> {
    let _lock = acquire_audit_lock(root, path).map_err(|error| {
        std::io::Error::new(error.kind(), format!("acquire audit lock: {error}"))
    })?;
    // Hold the same OS lock across rotation and append. Otherwise a peer can
    // rename the live file after this process opens it, sending the new record
    // into an archive while another peer appends to a new live inode.
    rotate_if_needed_locked(root, path);
    let record = format!("{line}\n");
    root.append_private(path, record.as_bytes(), false)
        .map_err(|error| std::io::Error::new(error.kind(), format!("append audit row: {error}")))
}

#[cfg(test)]
fn test_rooted_file(path: &Path) -> Option<(umadev_state::fs::RootedDir, PathBuf)> {
    let root = umadev_state::fs::RootedDir::open_no_follow(path.parent()?).ok()?;
    Some((root, PathBuf::from(path.file_name()?)))
}

#[cfg(test)]
fn append_jsonl_at(path: &Path, line: &str) -> std::io::Result<()> {
    let (root, relative) = test_rooted_file(path).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "audit path has no parent")
    })?;
    append_jsonl(&root, &relative, line)
}

/// Append an API-audit record. Implements `UD-EVID-001`.
///
/// Returns `Some(record)` when something was extracted (regardless of
/// disk-write success — audit failure must never bubble up). Returns
/// `None` when the file has no URLs to log.
#[must_use]
pub fn record_api_calls(
    project_root: &Path,
    file_path: &str,
    content: &str,
    tool_name: &str,
    session_id: &str,
    now: Option<i64>,
) -> Option<ApiCallRecord> {
    let mut urls: Vec<String> = extract_api_urls(file_path, content)
        .into_iter()
        .map(|url| sanitize_audit_text(&url, MAX_AUDIT_URL_CHARS))
        .collect();
    urls.sort();
    urls.dedup();
    if urls.is_empty() {
        return None;
    }
    let (ts, ts_ms) = audit_timestamp(now);
    let record = ApiCallRecord {
        ts,
        ts_ms,
        file: sanitize_audit_text(file_path, MAX_AUDIT_FILE_CHARS),
        tool: sanitize_audit_text(tool_name, MAX_AUDIT_TOOL_CHARS),
        urls,
        session_id: sanitize_audit_text(session_id, MAX_AUDIT_SESSION_CHARS),
    };
    if let (Ok(dir), Ok(line)) = (audit_dir(project_root), serde_json::to_string(&record)) {
        let _ = append_jsonl(&dir, Path::new("frontend-api-calls.jsonl"), &line);
    }
    Some(record)
}

/// Append a tool-call audit record. Implements `UD-EVID-002`.
///
/// Returns `None` for an empty `tool_name` (nothing to log); otherwise
/// returns the record. Disk-write errors are swallowed by design.
#[must_use]
pub fn record_tool_call(
    project_root: &Path,
    tool_name: &str,
    file_path: &str,
    decision: &str,
    clause: &str,
    reason: &str,
    session_id: &str,
    now: Option<i64>,
) -> Option<ToolCallRecord> {
    if tool_name.is_empty() {
        return None;
    }
    // Normalize the decision to the documented vocabulary
    // (allow | block | warn | audit). An unknown value used to be stored
    // verbatim, which then polluted the decisions BTreeMap in the
    // compliance mapping with arbitrary keys.
    let decision_norm = normalize_decision(decision);
    let (ts, ts_ms) = audit_timestamp(now);
    let record = ToolCallRecord {
        ts,
        ts_ms,
        tool: sanitize_audit_text(tool_name, MAX_AUDIT_TOOL_CHARS),
        file: sanitize_audit_text(file_path, MAX_AUDIT_FILE_CHARS),
        decision: decision_norm,
        clause: sanitize_audit_text(clause, MAX_AUDIT_CLAUSE_CHARS),
        reason: sanitize_audit_text(reason, MAX_AUDIT_REASON_CHARS),
        session_id: sanitize_audit_text(session_id, MAX_AUDIT_SESSION_CHARS),
    };
    if let (Ok(dir), Ok(line)) = (audit_dir(project_root), serde_json::to_string(&record)) {
        let _ = append_jsonl(&dir, Path::new("tool-calls.jsonl"), &line);
    }
    Some(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    struct EnvRestore {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prior }
        }

        fn remove(key: &'static str) -> Self {
            let prior = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prior }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn extract_fetch_axios_ky_swr() {
        let urls = extract_api_urls(
            "src/X.tsx",
            "fetch('/api/users'); axios.post('/api/orders', body); ky.get('/api/k'); useSWR('/api/s', f)",
        );
        assert_eq!(urls, vec!["/api/k", "/api/orders", "/api/s", "/api/users"]);
    }

    #[test]
    fn extract_dedupes() {
        let urls = extract_api_urls("src/X.tsx", "fetch('/api/u'); fetch('/api/u')");
        assert_eq!(urls, vec!["/api/u"]);
    }

    #[test]
    fn extract_ignores_external() {
        let urls = extract_api_urls("src/X.tsx", "fetch('https://cdn.example.com/i.png')");
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_ignores_non_frontend_extension() {
        let urls = extract_api_urls("server.py", "fetch('/api/u')");
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_handles_empty_content() {
        assert!(extract_api_urls("src/x.tsx", "").is_empty());
    }

    #[test]
    fn extract_caps_unique_urls_during_collection() {
        use std::fmt::Write as _;

        let mut content = String::new();
        for index in 0..(MAX_AUDIT_URLS + 200) {
            let _ = write!(content, "fetch('/api/item-{index}');");
        }

        let urls = extract_api_urls("src/X.tsx", &content);
        assert_eq!(urls.len(), MAX_AUDIT_URLS);
        assert!(urls.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn extract_does_not_scan_past_the_source_byte_budget() {
        let mut content = "fetch('/api/inside');".to_string();
        content.push_str(&"x".repeat(MAX_AUDIT_SOURCE_BYTES));
        content.push_str("fetch('/api/outside');");

        assert_eq!(extract_api_urls("src/X.tsx", &content), vec!["/api/inside"]);
    }

    #[test]
    fn record_api_calls_persists_jsonl() {
        let tmp = TempDir::new().unwrap();
        let r = record_api_calls(
            tmp.path(),
            "src/U.tsx",
            "fetch('/api/users'); axios.post('/api/orders', b)",
            "Write",
            "sess-123",
            Some(1_700_000_000),
        )
        .unwrap();
        assert_eq!(r.urls, vec!["/api/orders", "/api/users"]);
        let log = tmp.path().join(".umadev/audit/frontend-api-calls.jsonl");
        assert!(log.exists());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("/api/users"));
        assert!(text.contains("sess-123"));
    }

    #[test]
    fn record_api_calls_skips_when_empty() {
        let tmp = TempDir::new().unwrap();
        let r = record_api_calls(tmp.path(), "src/X.tsx", "const x = 1", "Write", "", None);
        assert!(r.is_none());
        assert!(!tmp.path().join(".umadev/audit").exists());
    }

    #[test]
    fn record_api_calls_appends() {
        let _guard = ROTATE_TEST_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = EnvRestore::remove("UMADEV_AUDIT_MAX_BYTES");
        let tmp = TempDir::new().unwrap();
        let _ = record_api_calls(
            tmp.path(),
            "src/A.tsx",
            "fetch('/api/a')",
            "Write",
            "",
            Some(1),
        );
        let _ = record_api_calls(
            tmp.path(),
            "src/B.tsx",
            "fetch('/api/b')",
            "Write",
            "",
            Some(2),
        );
        let log = tmp.path().join(".umadev/audit/frontend-api-calls.jsonl");
        let lines = std::fs::read_to_string(&log).unwrap();
        assert_eq!(lines.lines().count(), 2);
        // Rotation tests mutate UMADEV_AUDIT_MAX_BYTES; run governance
        // tests with --test-threads=1 to avoid env-var races.
    }

    #[test]
    fn record_tool_call_full_record() {
        let tmp = TempDir::new().unwrap();
        let r = record_tool_call(
            tmp.path(),
            "Write",
            "src/X.tsx",
            "block",
            "UD-CODE-001",
            "emoji used",
            "sess-xyz",
            Some(1_700_000_001),
        )
        .unwrap();
        assert_eq!(r.tool, "Write");
        assert_eq!(r.decision, "block");
        assert_eq!(r.clause, "UD-CODE-001");
        let log = tmp.path().join(".umadev/audit/tool-calls.jsonl");
        assert!(log.exists());
    }

    #[test]
    fn record_tool_call_empty_tool_name_skipped() {
        let tmp = TempDir::new().unwrap();
        let r = record_tool_call(tmp.path(), "", "x", "block", "", "", "", None);
        assert!(r.is_none());
    }

    #[test]
    fn record_tool_call_default_decision_is_allow() {
        let tmp = TempDir::new().unwrap();
        let r = record_tool_call(tmp.path(), "Edit", "x", "", "", "", "", Some(1)).unwrap();
        assert_eq!(r.decision, "allow");
    }

    #[test]
    fn record_tool_call_normalizes_unknown_decision() {
        // An unrecognized decision must collapse to "allow" (fail-open)
        // rather than polluting the compliance decisions map.
        let tmp = TempDir::new().unwrap();
        let r = record_tool_call(tmp.path(), "Edit", "x", "BANANA", "", "", "", Some(1)).unwrap();
        assert_eq!(r.decision, "allow");
    }

    #[test]
    fn record_tool_call_preserves_known_decisions_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let r = record_tool_call(tmp.path(), "Edit", "x", "BLOCK", "", "", "", Some(1)).unwrap();
        assert_eq!(r.decision, "block");
        let r2 = record_tool_call(tmp.path(), "Edit", "x", "  Warn ", "", "", "", Some(1)).unwrap();
        assert_eq!(r2.decision, "warn");
        let r3 = record_tool_call(tmp.path(), "Edit", "x", "Audit", "", "", "", Some(1)).unwrap();
        assert_eq!(r3.decision, "audit");
    }

    #[test]
    fn concurrent_appends_never_tear_a_line() {
        // P1-1: every hook invocation is a SEPARATE process appending to the
        // SAME JSONL. Model that with many threads each appending its own
        // record at once. The OS lock must cover each complete append, so no
        // partial `write_all` can interleave with another record or a rotation.
        // Assert each line round-trips and every record is present exactly once.
        let _guard = ROTATE_TEST_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Invalid zero falls back to the 5 MiB hard cap; this test stays far
        // below it and therefore never rotates mid-test.
        let _env = EnvRestore::set("UMADEV_AUDIT_MAX_BYTES", "0");
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&dir).unwrap();
        let live = std::sync::Arc::new(dir.join("tool-calls.jsonl"));

        let writers = 16;
        let per_writer = 40;
        let mut handles = Vec::new();
        for w in 0..writers {
            let live = std::sync::Arc::clone(&live);
            handles.push(std::thread::spawn(move || {
                for i in 0..per_writer {
                    // A realistic record whose body contains no newline so the
                    // ONLY newline in the file comes from our terminator.
                    let rec = ToolCallRecord {
                        ts: 1,
                        ts_ms: 1,
                        tool: "Write".to_string(),
                        file: format!("w{w}-line-{i}.tsx"),
                        decision: "allow".to_string(),
                        clause: String::new(),
                        reason: String::new(),
                        session_id: String::new(),
                    };
                    let serialized = serde_json::to_string(&rec).unwrap();
                    append_jsonl_at(&live, &serialized).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let body = fs::read_to_string(&*live).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines.len(),
            writers * per_writer,
            "every record must be its own intact line — no tears, no merges"
        );
        // Every line is independently parseable (a torn line would fail here).
        for line in &lines {
            serde_json::from_str::<ToolCallRecord>(line)
                .unwrap_or_else(|e| panic!("torn/invalid JSONL line {line:?}: {e}"));
        }
        // Every unique record we wrote is present exactly once.
        let mut files: Vec<String> = lines
            .iter()
            .map(|l| serde_json::from_str::<ToolCallRecord>(l).unwrap().file)
            .collect();
        files.sort();
        files.dedup();
        assert_eq!(
            files.len(),
            writers * per_writer,
            "no record dropped or duplicated under concurrency"
        );
    }

    #[test]
    fn concurrent_process_append_child() {
        let Some(root) = std::env::var_os("UMADEV_AUDIT_PROCESS_TEST_ROOT") else {
            return;
        };
        let writer = std::env::var("UMADEV_AUDIT_PROCESS_TEST_WRITER").unwrap();
        let live = Path::new(&root).join("tool-calls.jsonl");
        for line in 0..50 {
            append_jsonl_at(&live, &format!(r#"{{"writer":"{writer}","line":{line}}}"#)).unwrap();
        }
    }

    #[test]
    fn concurrent_process_appends_are_complete_and_unique() {
        let tmp = TempDir::new().unwrap();
        let writers = 6;
        let mut children = Vec::new();
        for writer in 0..writers {
            children.push(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "audit::tests::concurrent_process_append_child",
                        "--nocapture",
                    ])
                    .env("UMADEV_AUDIT_PROCESS_TEST_ROOT", tmp.path())
                    .env("UMADEV_AUDIT_PROCESS_TEST_WRITER", writer.to_string())
                    .env("UMADEV_AUDIT_MAX_BYTES", "0")
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }

        let body = fs::read_to_string(tmp.path().join("tool-calls.jsonl")).unwrap();
        let mut rows: Vec<(usize, usize)> = body
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                (
                    value["writer"].as_str().unwrap().parse().unwrap(),
                    usize::try_from(value["line"].as_u64().unwrap()).unwrap(),
                )
            })
            .collect();
        rows.sort_unstable();
        rows.dedup();
        assert_eq!(rows.len(), writers * 50);
    }

    #[test]
    fn concurrent_rotation_rotates_exactly_once() {
        // Two concurrent hook PROCESSES could both observe an over-cap file and
        // both rotate (stat-then-rename TOCTOU), double-shifting archives and
        // pruning real records early. With the rotation lock + re-check, many
        // racing rotations collapse to exactly ONE: the original payload lands
        // in a single archive, is never double-shifted into .2, and is not lost.
        let _guard = ROTATE_TEST_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = EnvRestore::set("UMADEV_AUDIT_MAX_BYTES", MIN_JSONL_BYTES.to_string());
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&dir).unwrap();
        let live = dir.join("tool-calls.jsonl");
        let original = format!(
            "ORIGINAL-AUDIT-RECORDS-PAYLOAD{}",
            "x".repeat(usize::try_from(MIN_JSONL_BYTES).unwrap())
        );
        fs::write(&live, &original).unwrap();

        let live = std::sync::Arc::new(live);
        let racers = 16;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(racers));
        let mut handles = Vec::new();
        for _ in 0..racers {
            let live = std::sync::Arc::clone(&live);
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                rotate_if_needed(&live);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // The original payload was rotated EXACTLY once.
        let mut copies = 0;
        for n in 1..=MAX_ARCHIVES {
            if let Ok(body) = fs::read_to_string(archive_path(&live, n)) {
                if body.contains(&original) {
                    copies += 1;
                }
            }
        }
        assert_eq!(
            copies, 1,
            "original must rotate exactly once — no loss, no double-rotate"
        );
        assert!(
            !archive_path(&live, 2).exists(),
            "must not premature-shift into .2 under concurrency"
        );
        assert!(
            umadev_state::fs::real_file(&rotate_lock_path(&live)),
            "persistent OS lock file must remain a safe regular file"
        );
    }

    // NOTE: these mutate the process-global UMADEV_AUDIT_MAX_BYTES env
    // var, so they must run in ONE test (serially) — parallel #[test]s on
    // the same env var race and flake.
    /// Test-only guard serializing the env-mutating rotate test against
    /// `record_api_calls_appends` (which reads the rotation cap). Held for
    /// the whole test body.
    static ROTATE_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn rotate_serial_under_cap_and_invalid_overrides() {
        use super::*;
        let _guard = ROTATE_TEST_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // --- (1) rotate when over cap ---
        let _env = EnvRestore::set("UMADEV_AUDIT_MAX_BYTES", MIN_JSONL_BYTES.to_string());
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev/audit");
        fs::create_dir_all(&dir).unwrap();
        let live = dir.join("tool-calls.jsonl");
        let old = format!(
            "already-big-content-here{}",
            "x".repeat(usize::try_from(MIN_JSONL_BYTES).unwrap())
        );
        fs::write(&live, &old).unwrap();
        append_jsonl_at(&live, r#"{"ts":1}"#).unwrap();
        let body = fs::read_to_string(&live).unwrap();
        assert!(
            body.contains(r#"{"ts":1}"#),
            "new line must be in live file"
        );
        assert!(
            !body.contains("already-big-content"),
            "old content rotated out"
        );
        let archive1 = archive_path(&live, 1);
        assert!(fs::read_to_string(&archive1)
            .unwrap()
            .contains("already-big-content"));

        // --- (2) keeps at most MAX_ARCHIVES ---
        std::env::set_var("UMADEV_AUDIT_MAX_BYTES", MIN_JSONL_BYTES.to_string());
        for i in 0..(MAX_ARCHIVES + 3) {
            append_jsonl_at(
                &live,
                &format!(
                    "line-{i}-content{}",
                    "y".repeat(usize::try_from(MIN_JSONL_BYTES).unwrap())
                ),
            )
            .unwrap();
        }
        let archived_files: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("tool-calls.jsonl.")
            })
            .collect();
        assert!(
            archived_files.len() <= MAX_ARCHIVES,
            "should keep at most {MAX_ARCHIVES} archives, got {}",
            archived_files.len()
        );

        // --- (3) zero cannot disable rotation or lower the cap to zero ---
        std::env::set_var("UMADEV_AUDIT_MAX_BYTES", "0");
        let tmp2 = TempDir::new().unwrap();
        let live2 = tmp2.path().join("tool-calls.jsonl");
        let over_default = "z".repeat(usize::try_from(DEFAULT_MAX_JSONL_BYTES + 1).unwrap());
        fs::write(&live2, &over_default).unwrap();
        append_jsonl_at(&live2, "new").unwrap();
        let body2 = fs::read_to_string(&live2).unwrap();
        assert_eq!(body2, "new\n");
        assert_eq!(
            fs::metadata(archive_path(&live2, 1)).unwrap().len(),
            DEFAULT_MAX_JSONL_BYTES + 1
        );

        // --- (4) an oversized override cannot raise the hard cap ---
        std::env::set_var(
            "UMADEV_AUDIT_MAX_BYTES",
            (DEFAULT_MAX_JSONL_BYTES + 1).to_string(),
        );
        assert_eq!(max_jsonl_bytes(), DEFAULT_MAX_JSONL_BYTES);

        // Restore the sentinel so other tests see the real default.
        // Rotation tests mutate UMADEV_AUDIT_MAX_BYTES; run governance
        // tests with --test-threads=1 to avoid env-var races.
    }

    #[test]
    fn audit_records_redact_and_bound_untrusted_fields() {
        let tmp = TempDir::new().unwrap();
        let secret = "sk-live-1234567890abcdef";
        let reason = format!("api_key={secret}\n{}", "x".repeat(20_000));
        let record = record_tool_call(
            tmp.path(),
            "Bash",
            &format!("curl -H 'Authorization: Bearer abcdefghijklmnop' /{secret}"),
            "block",
            "UD-SEC-003",
            &reason,
            "session-token=abcdefghijklmnop",
            Some(42),
        )
        .unwrap();

        assert_eq!((record.ts, record.ts_ms), (42, 42_000));
        let serialized = serde_json::to_string(&record).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("abcdefghijklmnop"));
        assert!(record.reason.chars().count() <= MAX_AUDIT_REASON_CHARS);
        assert!(record.reason.ends_with("..."));
        let disk = fs::read_to_string(tmp.path().join(".umadev/audit/tool-calls.jsonl")).unwrap();
        assert!(!disk.contains(secret));
        assert!(!disk.contains("abcdefghijklmnop"));
    }

    #[cfg(unix)]
    #[test]
    fn audit_writer_rejects_linked_directories_and_log_files() {
        use std::os::unix::fs::symlink;

        let linked_root = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        symlink(outside_dir.path(), linked_root.path().join(".umadev")).unwrap();
        assert!(record_tool_call(
            linked_root.path(),
            "Write",
            "src/x.rs",
            "audit",
            "",
            "safe",
            "",
            Some(1),
        )
        .is_some());
        assert!(outside_dir.path().read_dir().unwrap().next().is_none());

        let linked_file_root = TempDir::new().unwrap();
        let audit = linked_file_root.path().join(".umadev/audit");
        fs::create_dir_all(&audit).unwrap();
        let outside_file = linked_file_root.path().join("outside.jsonl");
        fs::write(&outside_file, "keep\n").unwrap();
        symlink(&outside_file, audit.join("tool-calls.jsonl")).unwrap();
        let _ = record_tool_call(
            linked_file_root.path(),
            "Write",
            "src/x.rs",
            "audit",
            "",
            "safe",
            "",
            Some(1),
        );
        assert_eq!(fs::read_to_string(outside_file).unwrap(), "keep\n");
    }

    #[cfg(unix)]
    #[test]
    fn opened_audit_directory_never_writes_to_a_replacement_project() {
        let parent = TempDir::new().unwrap();
        let project = parent.path().join("project");
        fs::create_dir(&project).unwrap();
        let audit = audit_dir(&project).unwrap();

        let moved = parent.path().join("project-moved");
        fs::rename(&project, &moved).unwrap();
        fs::create_dir(&project).unwrap();
        fs::create_dir_all(project.join(".umadev/audit")).unwrap();

        append_jsonl(&audit, Path::new("tool-calls.jsonl"), r#"{"safe":true}"#).unwrap();

        assert!(
            fs::read_to_string(moved.join(".umadev/audit/tool-calls.jsonl"))
                .unwrap()
                .contains(r#""safe":true"#)
        );
        assert!(!project.join(".umadev/audit/tool-calls.jsonl").exists());
    }
}
