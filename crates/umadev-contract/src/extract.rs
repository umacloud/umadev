//! Extract API calls from the worker's produced frontend source.
//!
//! Upgrades `extract_api_urls` (which returned bare `Vec<String>` path
//! strings) into typed [`FrontendCall`] records carrying the HTTP method
//! when inferable. Used by [`crate::validate`] to cross-check against the
//! contract.
//!
//! ## Method inference
//! Frontend HTTP libraries encode the method differently:
//! - `fetch('/api/x', { method: 'POST' })` — method in an options object.
//! - `axios.post('/api/x')`, `axios.get(...)` — method is the function name.
//! - `useSWR('/api/x', fetcher)` — method is always GET (SWR is read-only).
//! - `useMutation('/api/x')` — a write hook; defaults to POST (a mutation is
//!   never a read), not GET.
//!
//! We handle the common shapes; unknown call patterns default to GET (the
//! most common case) so a call is never silently dropped.
//!
//! ## Template-literal paths
//! `` fetch(`/api/users/${id}`) `` is normalised to `/api/users/:param` so it
//! matches a contract template like `/api/users/:id` instead of being
//! truncated to `/api/users/` (a 3-segment path that fails the 4-segment
//! contract match — a systematic false `UndeclaredCall`). See
//! [`normalize_template_path`].

use std::path::{Path, PathBuf};

use regex::Regex;
use std::sync::OnceLock;

use crate::parse::HttpVerb;

/// Frontend file extensions worth scanning for API calls.
const FRONTEND_EXTS: &[&str] = &["tsx", "ts", "jsx", "js", "vue", "svelte", "astro"];

/// Directories that never contain hand-written source worth scanning.
/// Kept conservative: a missing entry here means a wasted walk over a
/// potentially huge generated/vendored tree.
const SKIP_DIRS: &[&str] = &[
    // JS/TS toolchains
    "node_modules",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".vercel",
    ".astro",
    "dist",
    "build",
    "out",
    ".output",
    "coverage",
    ".nyc_output",
    ".pnpm-store",
    ".parcel-cache",
    // Python
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    // Rust / Go / others
    "target",
    "vendor",
    ".gradle",
    // VCS / meta
    ".git",
    ".hg",
    ".svn",
    // UmaDev's own output
    ".umadev",
    "output",
    "release",
    "knowledge",
    // Generic caches
    ".cache",
];

/// One API call found in frontend source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendCall {
    /// Workspace-relative file path.
    pub file: String,
    /// HTTP method, inferred from the call shape. Defaults to GET.
    pub method: HttpVerb,
    /// The path the call targets, e.g. `/api/users/123`. Template-literal
    /// interpolation is normalised to `:param` (see
    /// [`normalize_template_path`]).
    pub path: String,
    /// Whether [`Self::method`] was determined from the call shape (`fetch`
    /// options, `axios.post`, a `useMutation` write hook, …) rather than a
    /// fallback default. When `false`, the method is a best-effort tag and
    /// [`crate::validate::validate_frontend_vs_contract`] suppresses
    /// `MethodMismatch` for this call (it would be a guess, not a real defect).
    pub method_known: bool,
}

/// Regex for `fetch('/api/...')` and `fetch('/api/...', {...})`.
/// Captures the path in the `path` group, and an optional `method: 'POST'`
/// in the `method` group. Query strings are stripped after capture (not in
/// the regex — the `?` in a char class is fragile across regex versions).
fn fetch_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Path runs up to the closing quote — `${...}` interpolation is kept
        // (not truncated) so `fetch(`/api/users/${id}`)` captures the whole
        // `/api/users/${id}`, which `normalize_template_path` then rewrites to
        // `/api/users/:param`. Stopping at `$` (the old behaviour) truncated it
        // to `/api/users/`, a systematic false UndeclaredCall.
        // `lead` is a captured left boundary (the `regex` crate has no
        // look-behind): a `fetch(` whose preceding char is an identifier char
        // is really the tail of `prefetch(` / `refetch(` / `router.prefetch(`,
        // not a fetch call, and is rejected in `extract_from_file`.
        Regex::new(
            r#"(?P<lead>[A-Za-z0-9_$.]?)fetch\s*\(\s*['"`](?P<path>/[^'"`\#\s]+)['"`](?:[^)]*?method\s*:\s*['"`](?P<method>GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)['"`])?"#,
        )
        .expect("fetch regex well-formed")
    })
}

/// Regex for `axios.get('/api/...')` / `axios.post(...)` etc.
fn axios_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"axios\s*\.\s*(?P<method>get|post|put|delete|patch|head|options)\s*\(\s*['"`](?P<path>/[^'"`\#\s]+)['"`]"#,
        )
        .expect("axios regex well-formed")
    })
}

/// Regex for `ky.get(...)` / `http.get(...)` — same shape as axios.
fn method_client_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:ky|http)\s*\.\s*(?P<method>get|post|put|delete|patch)\s*\(\s*['"`](?P<path>/[^'"`\#\s]+)['"`]"#,
        )
        .expect("method-client regex well-formed")
    })
}

/// Regex for read hooks `useSWR('/api/...')` / `useQuery('/api/...')` /
/// `useFetch(...)` — always GET. `useMutation` is handled separately (it is a
/// write hook → POST), see [`use_mutation_regex`].
fn swr_query_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Path keeps `${...}` interpolation (normalised later), matching the
        // fetch/axios regexes — see `fetch_regex` rationale. `lead` is the same
        // captured left-boundary guard as `fetch_regex`: a hook name that is a
        // suffix of a longer identifier (`myUseQuery(`) is rejected in
        // `extract_from_file`.
        Regex::new(
            r#"(?P<lead>[A-Za-z0-9_$.]?)(?:useSWR|useSWRInfinite|useQuery|useFetch)\s*\(\s*['"`](?P<path>/[^'"`\#\s]+)['"`]"#,
        )
        .expect("swr/query regex well-formed")
    })
}

/// Regex for `useMutation('/api/...')` — a React-Query/SWR *write* hook.
/// A mutation is never a read, so we infer POST (the dominant write verb),
/// not GET. Inferring GET here produced a false `MethodMismatch` against any
/// POST-only endpoint the mutation actually targets.
fn use_mutation_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"useMutation\s*\(\s*['"`](?P<path>/[^'"`\#\s]+)['"`]"#)
            .expect("use-mutation regex well-formed")
    })
}

/// Regex for a DIRECT `axios('/api/x', {...})` call (no `.method`).
/// Method defaults to GET unless a `method:` option is present.
fn axios_direct_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"axios\s*\(\s*['"`](?P<path>/[^'"`\#\s]+)['"`](?:[^)]*?method\s*:\s*['"`](?P<method>GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)['"`])?"#,
        )
        .expect("axios-direct regex well-formed")
    })
}

/// Regex for object-style wrapped/SDK clients: `api.get('/api/x')`,
/// `httpClient.post(...)`, `client.delete(...)`, `service.put(...)`. These are
/// the common names projects give a typed wrapper around fetch/axios; without
/// them a whole app's API surface would be invisible to UD-CODE-003.
///
/// The `lead` group is a left identifier boundary the engine *captures*
/// (the `regex` crate has no look-behind): when it matches an identifier char
/// (`x` of `xapi.get(...)`, `.` of `foo.client.get(...)`) the call is rejected
/// in [`extract_from_file`]. The `.method` is **required** here: a bare
/// `service('/foo')` / `client('/x')` is far more often a DI lookup or a
/// factory than an HTTP call, so we do not treat it as one (that was a
/// systematic false GET → false `UndeclaredCall`/`MethodMismatch`).
fn wrapped_client_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?P<lead>[A-Za-z0-9_$.]?)(?:api|httpClient|client|service)\s*\.\s*(?P<method>get|post|put|delete|patch)\s*\(\s*['"`](?P<path>/[^'"`\#\s]+)['"`]"#,
        )
        .expect("wrapped-client regex well-formed")
    })
}

/// Regex for bare request-function wrappers: `request('/api/x')`,
/// `fetcher('/api/x')`. Unlike the object-style clients these names are
/// unambiguously an HTTP request, so the bare form (no `.method`) is allowed —
/// but the method is unknown. The caller tags it [`HttpVerb::Get`] and treats
/// an unknown-method call leniently so it never raises a false
/// `MethodMismatch` (see [`extract_from_file`]).
///
/// The captured `lead` boundary stops `superRequest('/x')`, `apiFetcher('/x')`,
/// `obj.request('/x')` from matching — only a free-standing `request(` /
/// `fetcher(` qualifies (the `regex` crate has no look-behind, so the boundary
/// char is captured and checked in code).
fn bare_request_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?P<lead>[A-Za-z0-9_$.]?)(?:request|fetcher)\s*\(\s*['"`](?P<path>/[^'"`\#\s]+)['"`]"#,
        )
        .expect("bare-request regex well-formed")
    })
}

/// Whether a wrapped/bare-client match must be **rejected** because its `lead`
/// boundary group captured an identifier char — that means the client name was
/// actually a *suffix* of a longer identifier (`superRequest`, `myApiClient`)
/// or a deeper member access (`foo.client`), neither of which is the HTTP
/// wrapper we mean to match.
///
/// Returns `true` (reject) only when `lead` is a single identifier-ish char
/// (`[A-Za-z0-9_$.]`). An empty `lead` (start of string) or a non-identifier
/// separator (`(`, `;`, space, `=`, newline) is a genuine free-standing call →
/// `false` (keep).
fn reject_for_identifier_lead(lead: &str) -> bool {
    !lead.is_empty()
        && lead
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.')
}

/// Strip a query string / fragment from a captured path:
/// `/api/search?q=test#sec` → `/api/search`.
fn strip_query(path: &str) -> &str {
    path.split(['?', '#']).next().unwrap_or(path)
}

/// Normalise template-literal interpolation in a captured path so it can match
/// a contract path template.
///
/// A `${...}` interpolation is rewritten to `:param`, and a whole segment that
/// *is* an interpolation (`/api/users/${id}` → `/api/users/:param`) maps onto
/// the contract's `/api/users/:id` style. An interpolation embedded in a
/// segment (`/api/users/${id}/posts` → `/api/users/:param/posts`) is handled
/// segment-by-segment so the segment count is preserved.
///
/// Returns the input unchanged when there is no `${`, so non-template paths pay
/// nothing. Fail-open: an unterminated `${` (no closing `}`) collapses the rest
/// of that segment to `:param` rather than erroring.
fn normalize_template_path(path: &str) -> String {
    if !path.contains("${") {
        return path.to_string();
    }
    let segments: Vec<String> = path
        .split('/')
        .map(|seg| {
            if !seg.contains("${") {
                return seg.to_string();
            }
            // A segment that is exactly one interpolation → `:param`.
            if seg.starts_with("${") && seg.ends_with('}') && seg.matches("${").count() == 1 {
                return ":param".to_string();
            }
            // Mixed segment (`v${n}` / `${id}.json` / unterminated) → rewrite
            // each `${...}` run to `:param`, char-by-char (no nesting in real
            // template paths), so the segment stays one segment.
            rewrite_interpolations_in_segment(seg)
        })
        .collect();
    segments.join("/")
}

/// Replace every `${...}` run inside a single path segment with `:param`.
/// An unterminated `${` (missing `}`) consumes to end-of-segment → `:param`.
/// UTF-8 safe: walks by byte index but only ever pushes whole `&str` slices.
fn rewrite_interpolations_in_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    let mut rest = seg;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        out.push_str(":param");
        match rest[start + 2..].find('}') {
            Some(rel) => rest = &rest[start + 2 + rel + 1..],
            None => return out, // unterminated — drop the rest of the segment
        }
    }
    out.push_str(rest);
    out
}

/// Scan frontend source under `project_root` and return every API call
/// found, deduped by `(file, method, path)`. Walks the tree (skipping
/// vendored / generated dirs), bounded by `MAX_FRONTEND_DEPTH` +
/// `MAX_FRONTEND_FILES`.
///
/// Returns an empty vec when no frontend source is present (fail-open —
/// the quality gate reports "no frontend calls to validate").
#[must_use]
pub fn extract_frontend_calls(project_root: &Path) -> Vec<FrontendCall> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_frontend_sources(project_root, &mut files, 0);
    let mut calls: Vec<FrontendCall> = Vec::new();
    for file in &files {
        // Skip oversized files (a minified / bundled `app.min.js`, not
        // hand-written source): running the multi-regex + comment-strip over a
        // megabyte of minified code both wastes work and floods phantom calls.
        // Mirrors `backend::MAX_FILE_BYTES`.
        let Ok(meta) = std::fs::metadata(file) else {
            continue;
        };
        if meta.len() > MAX_FRONTEND_FILE_BYTES {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        let rel = file
            .strip_prefix(project_root)
            .map(|p| p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
            .unwrap_or_else(|_| {
                file.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            });
        calls.extend(extract_from_file(&rel, &content));
    }
    // Dedupe across ALL files by (method, path). `Vec::dedup` only removes
    // *consecutive* duplicates, so the old `calls.dedup()` was a no-op for
    // the same call made in two different files (they're never adjacent).
    // We keep the first-seen `file` so audit can still point at a source.
    let mut seen: std::collections::HashSet<(HttpVerb, String)> = std::collections::HashSet::new();
    calls.retain(|c| seen.insert((c.method, c.path.clone())));
    calls
}

/// Extract calls from one file's content.
fn extract_from_file(file: &str, content: &str) -> Vec<FrontendCall> {
    // Strip JS/TS comments FIRST: a commented-out `// fetch('/api/deprecated')`
    // or `/* axios.get('/api/gone') */` must NEVER become a phantom
    // `UndeclaredCall` (the frontend mirror of the backend false-PASS the
    // `backend` module fixes). The strip is string/template-literal aware, so a
    // `//` inside a real URL string is preserved.
    let stripped = crate::backend::strip_comments_jsts(content);
    let content = stripped.as_str();
    let mut calls: Vec<FrontendCall> = Vec::new();
    // `method_known` records whether the verb came from the call shape (true)
    // or is a best-effort default (false). The path is query-stripped then
    // template-normalised before storage so it can match a contract template.
    let push = |calls: &mut Vec<FrontendCall>, method: HttpVerb, method_known: bool, path: &str| {
        let path = normalize_template_path(strip_query(path));
        if !calls
            .iter()
            .any(|c| c.method == method && c.path == path && c.method_known == method_known)
        {
            calls.push(FrontendCall {
                file: file.to_string(),
                method,
                path,
                method_known,
            });
        }
    };

    // fetch('/api/x') or fetch('/api/x', { method: 'POST' }).
    // The verb is known only when a `method:` option is present; a bare
    // fetch('/x') is GET by spec default → `method_known = true` still, because
    // GET *is* fetch's defined default (not a guess).
    for cap in fetch_regex().captures_iter(content) {
        if reject_for_identifier_lead(cap.name("lead").map(|m| m.as_str()).unwrap_or("")) {
            continue; // `prefetch(` / `refetch(` / `router.prefetch(`, not `fetch(`
        }
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if path.is_empty() {
            continue;
        }
        let method = cap
            .name("method")
            .and_then(|m| HttpVerb::parse(m.as_str()))
            .unwrap_or(HttpVerb::Get);
        push(&mut calls, method, true, path);
    }
    // axios.get / axios.post / ... — verb is the function name, always known.
    for cap in axios_regex().captures_iter(content) {
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        let method = cap
            .name("method")
            .and_then(|m| HttpVerb::parse(m.as_str()))
            .unwrap_or(HttpVerb::Get);
        if !path.is_empty() {
            push(&mut calls, method, true, path);
        }
    }
    // ky.get / http.get / ... — verb is the function name, always known.
    for cap in method_client_regex().captures_iter(content) {
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        let method = cap
            .name("method")
            .and_then(|m| HttpVerb::parse(m.as_str()))
            .unwrap_or(HttpVerb::Get);
        if !path.is_empty() {
            push(&mut calls, method, true, path);
        }
    }
    // useSWR / useQuery / useFetch — read hooks, always GET (known).
    for cap in swr_query_regex().captures_iter(content) {
        if reject_for_identifier_lead(cap.name("lead").map(|m| m.as_str()).unwrap_or("")) {
            continue; // suffix of a longer identifier, not a real hook call
        }
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if !path.is_empty() {
            push(&mut calls, HttpVerb::Get, true, path);
        }
    }
    // useMutation('/api/x') — write hook, POST (known: a mutation is never a
    // read). Previously defaulted to GET → false MethodMismatch on POST-only
    // endpoints.
    for cap in use_mutation_regex().captures_iter(content) {
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if !path.is_empty() {
            push(&mut calls, HttpVerb::Post, true, path);
        }
    }
    // Direct axios('/api/x') (no .method) — GET unless a method: option is set.
    for cap in axios_direct_regex().captures_iter(content) {
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if path.is_empty() {
            continue;
        }
        let method = cap
            .name("method")
            .and_then(|m| HttpVerb::parse(m.as_str()))
            .unwrap_or(HttpVerb::Get);
        push(&mut calls, method, true, path);
    }
    // Object-style wrapped clients: api.get / httpClient.post / client.delete.
    // Reject a match whose `lead` boundary captured an identifier char (the
    // name was a suffix of a longer identifier or a deeper member access).
    for cap in wrapped_client_regex().captures_iter(content) {
        if reject_for_identifier_lead(cap.name("lead").map(|m| m.as_str()).unwrap_or("")) {
            continue;
        }
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if path.is_empty() {
            continue;
        }
        // `.method` is required by the regex, so the verb is always known here.
        let method = cap
            .name("method")
            .and_then(|m| HttpVerb::parse(m.as_str()))
            .unwrap_or(HttpVerb::Get);
        push(&mut calls, method, true, path);
    }
    // Bare request-function wrappers: request('/x') / fetcher('/x'). The verb
    // is UNKNOWN (no `.method`), so we tag GET but mark `method_known = false`
    // → the validator suppresses MethodMismatch for these.
    for cap in bare_request_regex().captures_iter(content) {
        if reject_for_identifier_lead(cap.name("lead").map(|m| m.as_str()).unwrap_or("")) {
            continue;
        }
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if path.is_empty() {
            continue;
        }
        push(&mut calls, HttpVerb::Get, false, path);
    }

    calls
}

/// Maximum directory depth for the frontend-source walk. Enterprise admin
/// frontends commonly place real views under deep trees such as
/// `src/views/biz/app/page/component/widgets/...`; keep this high enough to scan
/// those while the file-count cap + skip dirs still bound pathological trees.
const MAX_FRONTEND_DEPTH: usize = 16;

/// Maximum number of frontend source files collected before the walk stops.
/// Mirrors the sibling walkers (`acceptance.rs` `MAX_SOURCE_FILES = 600`,
/// `backend.rs` `MAX_FILES = 800`) so a deep + wide enterprise frontend can't
/// make contract extraction read + multi-regex an unbounded file set.
const MAX_FRONTEND_FILES: usize = 800;

/// Skip frontend files larger than this. A bundled / minified asset
/// (`app.min.js`, a vendored `*.bundle.js`) is not hand-written call source;
/// scanning it wastes work and, on a single-line minified blob, would flood
/// phantom calls. Mirrors `backend::MAX_FILE_BYTES`.
const MAX_FRONTEND_FILE_BYTES: u64 = 600_000;

fn collect_frontend_sources(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_FRONTEND_DEPTH || out.len() >= MAX_FRONTEND_FILES {
        // Library code must never write directly to stdout/stderr: the TUI owns
        // the terminal. A deep or wide tree is bounded silently here and
        // higher-level verification remains fail-open.
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if out.len() >= MAX_FRONTEND_FILES {
            return;
        }
        let p = entry.path();
        // Do NOT follow symlinks: `p.is_dir()` stats the symlink TARGET, so a
        // symlinked directory was recursed into — it could point outside the
        // project, or form a cycle. `symlink_metadata` stats the link itself, so
        // a symlink (to a dir or a file) is classified here and skipped, matching
        // the no-follow contract of UmaDev's other tree walkers.
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
            collect_frontend_sources(&p, out, depth + 1);
        } else if meta.is_file() {
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
            if FRONTEND_EXTS.contains(&ext) {
                out.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_fetch_get() {
        let calls = extract_from_file("src/api.ts", "fetch('/api/users')");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, HttpVerb::Get);
        assert_eq!(calls[0].path, "/api/users");
    }

    #[test]
    fn extract_fetch_with_method() {
        let calls = extract_from_file(
            "src/api.ts",
            "fetch('/api/orders', { method: 'POST', body: JSON.stringify(data) })",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, HttpVerb::Post);
        assert_eq!(calls[0].path, "/api/orders");
    }

    #[test]
    fn extract_axios_methods() {
        let calls = extract_from_file(
            "src/api.ts",
            "axios.get('/api/users'); axios.post('/api/orders', body); axios.delete('/api/x')",
        );
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].method, HttpVerb::Get);
        assert_eq!(calls[1].method, HttpVerb::Post);
        assert_eq!(calls[2].method, HttpVerb::Delete);
    }

    #[test]
    fn extract_ky_and_http() {
        let calls = extract_from_file(
            "src/api.ts",
            "ky.put('/api/items'); http.patch('/api/items/1')",
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].method, HttpVerb::Put);
        assert_eq!(calls[1].method, HttpVerb::Patch);
    }

    #[test]
    fn extract_swr_always_get() {
        let calls = extract_from_file("src/api.ts", "useSWR('/api/profile', fetcher)");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, HttpVerb::Get);
    }

    #[test]
    fn dedupes_within_file() {
        let calls = extract_from_file(
            "src/api.ts",
            "fetch('/api/users'); fetch('/api/users'); fetch('/api/users')",
        );
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn ignores_external_urls() {
        // Only paths starting with `/` are captured (the regex requires it).
        let calls = extract_from_file("src/api.ts", "fetch('https://cdn.example.com/img.png')");
        assert!(calls.is_empty());
    }

    #[test]
    fn ignores_non_frontend_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("server.py"), "fetch('/api/x')").unwrap();
        assert!(extract_frontend_calls(tmp.path()).is_empty());
    }

    #[test]
    fn skips_vendored_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/lib")).unwrap();
        std::fs::write(
            tmp.path().join("node_modules/lib/x.ts"),
            "fetch('/api/evil')",
        )
        .unwrap();
        assert!(extract_frontend_calls(tmp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_directory_symlinks() {
        // Regression: the walk used `p.is_dir()`, which follows symlinks, so a
        // symlinked directory (a cycle, or an escape OUTSIDE the project) was
        // recursed into. The walk must not follow directory symlinks.
        let tmp = tempfile::TempDir::new().unwrap();
        // A real frontend file OUTSIDE the tree we scan.
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("leak.ts"), "fetch('/api/leak')").unwrap();
        // The project tree we DO scan.
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::write(proj.join("src/app.ts"), "fetch('/api/real')").unwrap();
        // A symlink inside the project pointing at the outside dir.
        std::os::unix::fs::symlink(&outside, proj.join("src/linked")).unwrap();

        let calls = extract_frontend_calls(&proj);
        let paths: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(
            paths.contains(&"/api/real"),
            "real in-tree source must still be scanned: {paths:?}"
        );
        assert!(
            !paths.contains(&"/api/leak"),
            "a symlinked directory must NOT be followed: {paths:?}"
        );
    }

    #[test]
    fn extracts_from_real_project_layout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src/api");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("client.ts"),
            "fetch('/api/users'); axios.post('/api/auth/login', creds)",
        )
        .unwrap();
        let calls = extract_frontend_calls(tmp.path());
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().any(|c| c.path == "/api/users"));
        assert!(calls.iter().any(|c| c.path == "/api/auth/login"));
    }

    #[test]
    fn extracts_from_deep_enterprise_frontend_layout() {
        // Enterprise admin projects often nest components deeper than 8 levels
        // under src/views/...; those API calls still belong to the real frontend
        // contract surface and must not be silently skipped.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp
            .path()
            .join("src/views/biz/app/page/component/widgets/customer/detail/panels");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ProfilePanel.vue"),
            "<script setup>fetch('/api/customer/profile')</script>",
        )
        .unwrap();

        let calls = extract_frontend_calls(tmp.path());
        assert!(
            calls.iter().any(|c| c.path == "/api/customer/profile"),
            "deep frontend calls must be scanned: {calls:?}"
        );
    }

    #[test]
    fn frontend_scan_stops_at_file_cap() {
        // A deep + wide enterprise frontend must not make the walk collect an
        // unbounded file set: collection stops at MAX_FRONTEND_FILES.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("src/views");
        std::fs::create_dir_all(&dir).unwrap();
        let over = MAX_FRONTEND_FILES + 50;
        for i in 0..over {
            std::fs::write(dir.join(format!("view{i}.ts")), "fetch('/api/x')").unwrap();
        }

        let mut files: Vec<PathBuf> = Vec::new();
        collect_frontend_sources(tmp.path(), &mut files, 0);
        assert_eq!(
            files.len(),
            MAX_FRONTEND_FILES,
            "collection must be bounded by MAX_FRONTEND_FILES, got {}",
            files.len()
        );
    }

    #[test]
    fn strips_query_strings_from_path() {
        let calls = extract_from_file("src/api.ts", "fetch('/api/search?q=test')");
        assert_eq!(calls[0].path, "/api/search");
    }

    #[test]
    fn extracts_direct_axios_call() {
        // axios('/api/x') with no .method — previously missed.
        let calls = extract_from_file("src/a.ts", "axios('/api/upload', { onUploadProgress })");
        assert!(
            calls.iter().any(|c| c.path == "/api/upload"),
            "direct axios() must be captured"
        );
    }

    // ---- Fix 1: template-literal normalisation (no more truncation) ----

    #[test]
    fn template_literal_fetch_normalised_to_param() {
        // Fix 1: fetch(`/api/users/${id}`) must normalise to
        // /api/users/:param (NOT truncate to /api/users/, which is a different
        // segment count and a systematic false UndeclaredCall).
        let calls = extract_from_file("src/a.ts", "fetch(`/api/users/${id}`)");
        let matched: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(
            matched.contains(&"/api/users/:param"),
            "template-literal fetch must normalise to :param, got {matched:?}"
        );
        assert!(
            !matched.iter().any(|p| p.contains("${")),
            "must not keep raw interpolation, got {matched:?}"
        );
        assert!(
            !matched.contains(&"/api/users/"),
            "must not truncate to the static prefix, got {matched:?}"
        );
    }

    #[test]
    fn template_normalised_path_matches_contract_param() {
        // Fix 1 end-to-end: the normalised call must validate clean against a
        // `:id` contract template — proving the old false UndeclaredCall is gone.
        use crate::parse::parse_architecture;
        use crate::validate::validate_frontend_vs_contract;
        let spec = parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n\
             |---|---|---|---|---|---|\n\
             | GET | /api/users/:id | - | - | none | Get one |\n",
            "demo",
        );
        let calls = extract_from_file("src/a.ts", "fetch(`/api/users/${id}`)");
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert!(
            v.is_empty(),
            "normalised template path must match /api/users/:id, got {v:?}"
        );
    }

    #[test]
    fn template_interpolation_mid_segment_and_multi() {
        // Mixed segment `v${n}` and a trailing literal segment are preserved
        // segment-by-segment; segment count stays correct.
        let calls = extract_from_file("src/a.ts", "fetch(`/api/v${n}/users/${id}/posts`)");
        let p: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(
            p.contains(&"/api/v:param/users/:param/posts"),
            "mixed + multi interpolation must normalise per-segment, got {p:?}"
        );
    }

    #[test]
    fn normalize_template_path_is_noop_without_interpolation() {
        // A plain path is returned unchanged (no allocation surprises).
        assert_eq!(normalize_template_path("/api/users"), "/api/users");
        assert_eq!(normalize_template_path("/api/users/:id"), "/api/users/:id");
    }

    #[test]
    fn normalize_template_path_unterminated_interpolation() {
        // Fail-open: an unterminated `${` collapses the rest of the segment.
        assert_eq!(normalize_template_path("/api/${id"), "/api/:param");
    }

    // ---- Fix 3: useMutation infers POST, not GET ----

    #[test]
    fn use_mutation_infers_post() {
        // Fix 3: useMutation is a write hook → POST and method_known = true.
        let calls = extract_from_file("src/a.ts", "useMutation('/api/posts')");
        let m = calls.iter().find(|c| c.path == "/api/posts");
        let m = m.expect("useMutation must be captured");
        assert_eq!(m.method, HttpVerb::Post, "useMutation must infer POST");
        assert!(m.method_known, "useMutation method is known (write hook)");
    }

    #[test]
    fn use_mutation_no_false_method_mismatch_on_post_endpoint() {
        // Fix 3 end-to-end: a POST-only contract endpoint must NOT raise a
        // MethodMismatch for a useMutation targeting it (the old GET default did).
        use crate::parse::parse_architecture;
        use crate::validate::validate_frontend_vs_contract;
        let spec = parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n\
             |---|---|---|---|---|---|\n\
             | POST | /api/posts | - | - | none | Create |\n",
            "demo",
        );
        let calls = extract_from_file("src/a.ts", "useMutation('/api/posts')");
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert!(
            v.is_empty(),
            "useMutation→POST must match a POST-only endpoint, got {v:?}"
        );
    }

    #[test]
    fn use_query_still_get() {
        // Read hooks remain GET (regression guard for the split).
        let calls = extract_from_file("src/a.ts", "useQuery('/api/profile')");
        let c = calls.iter().find(|c| c.path == "/api/profile").unwrap();
        assert_eq!(c.method, HttpVerb::Get);
        assert!(c.method_known);
    }

    // ---- Fix 2 + 3: wrapped/bare client tightening ----

    #[test]
    fn wrapped_client_object_style_captured() {
        // api.get / httpClient.post / client.delete / service.put — still work.
        let calls = extract_from_file(
            "src/a.ts",
            "api.get('/api/products'); httpClient.post('/api/orders'); \
             client.delete('/api/x'); service.put('/api/y')",
        );
        let paths: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        for want in ["/api/products", "/api/orders", "/api/x", "/api/y"] {
            assert!(paths.contains(&want), "{want} must be captured: {paths:?}");
        }
        // Methods come from the call shape → all known.
        assert!(calls.iter().all(|c| c.method_known));
        let post = calls.iter().find(|c| c.path == "/api/orders").unwrap();
        assert_eq!(post.method, HttpVerb::Post);
    }

    #[test]
    fn bare_request_function_captured_unknown_method() {
        // Fix 3: request('/x') / fetcher('/x') captured but method UNKNOWN.
        let calls = extract_from_file("src/a.ts", "request('/api/health'); fetcher('/api/cfg')");
        let h = calls.iter().find(|c| c.path == "/api/health").unwrap();
        assert!(
            !h.method_known,
            "bare request() method must be marked unknown"
        );
        assert!(calls.iter().any(|c| c.path == "/api/cfg"));
    }

    #[test]
    fn bare_request_no_false_method_mismatch() {
        // Fix 3 end-to-end: a bare request('/x') against a POST-only endpoint
        // must NOT raise MethodMismatch (verb is a guess, not a defect).
        use crate::parse::parse_architecture;
        use crate::validate::validate_frontend_vs_contract;
        let spec = parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n\
             |---|---|---|---|---|---|\n\
             | POST | /api/health | - | - | none | Ping |\n",
            "demo",
        );
        let calls = extract_from_file("src/a.ts", "request('/api/health')");
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert!(
            v.is_empty(),
            "unknown-method bare request must not mismatch, got {v:?}"
        );
    }

    #[test]
    fn wrapped_client_rejects_identifier_suffix() {
        // Fix 2: superRequest('/x') and apiFetcher('/x') must NOT be treated
        // as HTTP calls (the name is a suffix of a longer identifier).
        let calls = extract_from_file(
            "src/a.ts",
            "superRequest('/api/evil'); apiFetcher('/api/evil2')",
        );
        let paths: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(
            !paths.contains(&"/api/evil"),
            "superRequest must not match: {paths:?}"
        );
        assert!(
            !paths.contains(&"/api/evil2"),
            "apiFetcher must not match: {paths:?}"
        );
    }

    #[test]
    fn wrapped_client_rejects_member_access_suffix() {
        // Fix 2: foo.client.get('/x') / obj.request('/x') — the client name is
        // a deeper member of another receiver; reject (lead char is `.`).
        let calls = extract_from_file(
            "src/a.ts",
            "thing.client.get('/api/inner'); obj.request('/api/inner2')",
        );
        let paths: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(
            !paths.contains(&"/api/inner"),
            "member-access client.get must not match: {paths:?}"
        );
        assert!(
            !paths.contains(&"/api/inner2"),
            "member-access request must not match: {paths:?}"
        );
    }

    #[test]
    fn wrapped_client_rejects_bare_non_http_factory() {
        // Fix 2: a bare service('/foo') / client('/x') (no `.method`) is far
        // more often a DI lookup / factory than an HTTP call → must NOT match.
        let calls = extract_from_file("src/a.ts", "service('/foo'); client('/bar'); api('/baz')");
        let paths: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(
            paths.is_empty(),
            "bare factory-style call must not be an HTTP call: {paths:?}"
        );
    }

    #[test]
    fn wrapped_client_free_standing_still_matches() {
        // Negative-control: a genuinely free-standing request('/x') after a `;`
        // or at line start still matches (the boundary check is not too strict).
        let calls = extract_from_file("src/a.ts", "const x = request('/api/ok');");
        assert!(
            calls.iter().any(|c| c.path == "/api/ok"),
            "free-standing request() must still match"
        );
    }

    #[test]
    fn reject_for_identifier_lead_helper() {
        // Empty lead (start of string) is a genuine boundary → keep.
        assert!(!reject_for_identifier_lead(""));
        // Non-identifier separators are genuine boundaries → keep.
        for sep in ["(", ";", " ", "=", "\n", "\t"] {
            assert!(
                !reject_for_identifier_lead(sep),
                "{sep:?} should be a valid boundary (kept)"
            );
        }
        // Identifier chars / member-access dot mean the name was a suffix → reject.
        for bad in ["a", "Z", "_", "$", ".", "9"] {
            assert!(
                reject_for_identifier_lead(bad),
                "{bad:?} should be rejected as an identifier-suffix lead"
            );
        }
    }

    // ---- Fix #1: comment-strip before the call regexes ----

    #[test]
    fn commented_fetch_is_not_a_phantom_call() {
        // A line-commented fetch must NOT become an UndeclaredCall.
        let calls = extract_from_file(
            "src/a.ts",
            "// fetch('/api/deprecated')\nfetch('/api/real')",
        );
        let p: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(p.contains(&"/api/real"), "real call kept: {p:?}");
        assert!(
            !p.contains(&"/api/deprecated"),
            "commented call must not be a phantom: {p:?}"
        );
    }

    #[test]
    fn block_commented_axios_is_not_a_phantom_call() {
        let calls = extract_from_file(
            "src/a.ts",
            "/* axios.get('/api/gone') */ axios.post('/api/live')",
        );
        let p: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(p.contains(&"/api/live"), "real call kept: {p:?}");
        assert!(
            !p.contains(&"/api/gone"),
            "block-commented call must not be a phantom: {p:?}"
        );
    }

    #[test]
    fn double_slash_inside_url_string_does_not_break_extraction() {
        // Regression: the comment strip is string-aware, so a `//` inside a URL
        // string must not truncate the following real call. (The absolute-path
        // `fetch('/api/ok')` is still captured.)
        let calls = extract_from_file(
            "src/a.ts",
            "const base = 'https://cdn.example.com'; fetch('/api/ok')",
        );
        assert!(
            calls.iter().any(|c| c.path == "/api/ok"),
            "real call after a URL string must still extract: {calls:?}"
        );
    }

    // ---- Fix #4: fetch/swr left-boundary guard ----

    #[test]
    fn prefetch_is_not_a_fetch_call() {
        // `router.prefetch('/route')` and `refetch('/x')` contain `fetch(` but
        // are NOT fetch calls — the identifier lead must reject them.
        let calls = extract_from_file(
            "src/a.ts",
            "router.prefetch('/route'); refetch('/x'); const p = prefetch('/y');",
        );
        let p: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(
            p.is_empty(),
            "prefetch/refetch must not match as fetch: {p:?}"
        );
    }

    #[test]
    fn genuine_fetch_still_matches_after_boundary_guard() {
        // Negative-control for #4: real fetch calls at various boundaries still
        // match (guard is not too strict).
        let calls = extract_from_file(
            "src/a.ts",
            "fetch('/api/a'); await fetch('/api/b'); const r = fetch('/api/c');",
        );
        let p: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        for want in ["/api/a", "/api/b", "/api/c"] {
            assert!(p.contains(&want), "{want} must still match: {p:?}");
        }
    }

    #[test]
    fn suffix_hook_is_not_a_swr_call() {
        // A hook name that is a suffix of a longer identifier must be rejected.
        let calls = extract_from_file("src/a.ts", "const d = myuseQuery('/api/x');");
        let p: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(!p.contains(&"/api/x"), "suffix hook must not match: {p:?}");
    }

    // ---- Fix #3: {id} / <int:id> contract params match a real call ----

    #[test]
    fn brace_param_contract_matches_concrete_call() {
        // An OpenAPI-style `/api/users/{id}` contract must match a real
        // `/api/users/123` call — previously a systematic false UndeclaredCall.
        use crate::parse::parse_architecture;
        use crate::validate::validate_frontend_vs_contract;
        let spec = parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n\
             |---|---|---|---|---|---|\n\
             | GET | /api/users/{id} | - | - | none | Get one |\n",
            "demo",
        );
        let calls = extract_from_file("src/a.ts", "fetch('/api/users/123')");
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert!(
            v.is_empty(),
            "a {{id}} contract must match /api/users/123, got {v:?}"
        );
    }

    #[test]
    fn angle_param_contract_matches_concrete_call() {
        // Django-style `<int:id>` param segment must match a real call too.
        use crate::parse::parse_architecture;
        use crate::validate::validate_frontend_vs_contract;
        let spec = parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n\
             |---|---|---|---|---|---|\n\
             | GET | /api/orders/<int:id> | - | - | none | Get one |\n",
            "demo",
        );
        let calls = extract_from_file("src/a.ts", "fetch('/api/orders/42')");
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert!(
            v.is_empty(),
            "an <int:id> contract must match /api/orders/42, got {v:?}"
        );
    }

    #[test]
    fn undeclared_call_still_caught_after_param_broadening() {
        // Regression guard: broadening param vocab must NOT hide a genuine
        // undeclared call to a path with no matching contract template.
        use crate::parse::parse_architecture;
        use crate::validate::validate_frontend_vs_contract;
        use crate::validate::ViolationKind;
        let spec = parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |\n\
             |---|---|---|---|---|---|\n\
             | GET | /api/users/{id} | - | - | none | Get one |\n",
            "demo",
        );
        let calls = extract_from_file("src/a.ts", "fetch('/api/nope/1')");
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert_eq!(
            v.len(),
            1,
            "an undeclared call must still be flagged: {v:?}"
        );
        assert_eq!(v[0].kind, ViolationKind::UndeclaredCall);
    }

    // ---- Fix #6: oversize file skipped ----

    #[test]
    fn oversize_file_is_skipped() {
        // A minified / bundled file over MAX_FRONTEND_FILE_BYTES must not be
        // scanned (it would flood phantom calls and waste work).
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // One real small file that MUST still be scanned.
        std::fs::write(src.join("app.ts"), "fetch('/api/real')").unwrap();
        // One oversize minified blob whose calls must be ignored.
        let mut big = String::new();
        big.push_str("fetch('/api/flood');");
        while (big.len() as u64) <= MAX_FRONTEND_FILE_BYTES {
            big.push_str("/*pad*/const x=1;");
        }
        std::fs::write(src.join("app.min.js"), &big).unwrap();

        let calls = extract_frontend_calls(tmp.path());
        let p: Vec<&str> = calls.iter().map(|c| c.path.as_str()).collect();
        assert!(
            p.contains(&"/api/real"),
            "small file must be scanned: {p:?}"
        );
        assert!(
            !p.contains(&"/api/flood"),
            "oversize file must be skipped: {p:?}"
        );
    }
}
