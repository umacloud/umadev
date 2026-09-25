//! Parse the architecture Markdown's API surface table into a typed
//! [`ApiSpec`].
//!
//! The current architecture doc (`render_architecture` in umadev-agent)
//! emits a Markdown table:
//!
//! ```text
//! | Method | Path | Request | Response | Auth | Description |
//! |---|---|---|---|---|---|
//! | POST | /api/auth/login | { email, password } | { token, user } | none | Login |
//! ```
//!
//! This module upgrades the fragile `line.starts_with('|')` + `split('|')`
//! parser (which mis-extracted any `/`-containing cell as a path) into a
//! column-aware parser that validates method verbs, dedupes endpoints, and
//! produces typed [`Endpoint`] records.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// An HTTP method, restricted to the verbs OpenAPI allows in a path item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpVerb {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `PATCH`
    Patch,
    /// `OPTIONS`
    Options,
    /// `HEAD`
    Head,
}

impl HttpVerb {
    /// Parse a method string (case-insensitive). Returns `None` for anything
    /// that isn't a standard HTTP verb.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            "OPTIONS" => Some(Self::Options),
            "HEAD" => Some(Self::Head),
            _ => None,
        }
    }

    /// Lowercase identifier used as the OpenAPI path-item key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
            Self::Put => "put",
            Self::Delete => "delete",
            Self::Patch => "patch",
            Self::Options => "options",
            Self::Head => "head",
        }
    }
}

/// How an endpoint authenticates. Upgrades the free-text Auth column
/// (`bearer` / `none` / `jwt`) into a typed enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecurityKind {
    /// No authentication required (public endpoint).
    None,
    /// Bearer token (JWT or opaque) in the Authorization header.
    Bearer,
    /// API key in a header / query / cookie.
    ApiKey,
    /// OAuth 2.0 with a named flow.
    OAuth2,
    /// Session cookie (server-rendered apps).
    Session,
    /// Unrecognised auth description — kept as a catch-all so parsing never
    /// drops an endpoint entirely.
    Other,
}

impl SecurityKind {
    /// Parse the Auth column text into a security kind. Tolerant: common
    /// synonyms (`jwt` → Bearer, `token` → Bearer) are mapped, and placeholder
    /// or negative markers (`-`, `n/a`, `无`, `否`, `公开`, `None (public)`,
    /// `No token`) mean a public endpoint.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        let lower = s.trim().to_ascii_lowercase();
        if marks_public_endpoint(&lower) {
            return Self::None;
        }
        if lower.contains("bearer") || lower.contains("jwt") || lower.contains("token") {
            return Self::Bearer;
        }
        if lower.contains("api key")
            || lower.contains("apikey")
            || lower.contains("api-key")
            || lower == "key"
        {
            return Self::ApiKey;
        }
        if lower.contains("oauth") {
            return Self::OAuth2;
        }
        if lower.contains("session") || lower.contains("cookie") {
            return Self::Session;
        }
        Self::Other
    }
}

/// Whether a (lower-cased) Auth cell marks the endpoint as public: an empty or
/// placeholder cell (`-`, `—`, `n/a`, a cross mark), a Chinese negative or public
/// marker (`无`, `否`, `无需登录`, `不需要`, `公开`, `匿名`), or a cell whose first
/// word is negative or public (`None (public)`, `No token`, `Not required`). It is
/// checked before the substring rules, so `No token` is not read as Bearer.
fn marks_public_endpoint(lower: &str) -> bool {
    const PLACEHOLDERS: &[&str] = &["", "-", "--", "—", "–", "na", "✗", "✘", "×", "❌"];
    const PREFIXES: &[&str] = &[
        "n/a", "无需", "無需", "无须", "無須", "不需", "不用", "免登", "公开", "公開", "匿名", "否",
    ];
    const FIRST_WORDS: &[&str] = &[
        "none",
        "no",
        "not",
        "false",
        "public",
        "anonymous",
        "无",
        "無",
    ];
    if PLACEHOLDERS.contains(&lower) || PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    let first_word = lower
        .split(|c: char| {
            c.is_whitespace() || c.is_ascii_punctuation() || matches!(c, '（' | '，' | '、' | '；')
        })
        .find(|word| !word.is_empty())
        .unwrap_or("");
    FIRST_WORDS.contains(&first_word)
}

/// One API endpoint: method + path + metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// HTTP method.
    pub method: HttpVerb,
    /// Path template, e.g. `/api/users/:id`. Always starts with `/`.
    pub path: String,
    /// Stable operation identifier (e.g. `listUsers`). Derived from the
    /// description when the doc doesn't give one.
    pub operation_id: String,
    /// Human-readable description (the Description column).
    pub description: String,
    /// Request body shape as free text (the Request column). Empty for GET.
    pub request_shape: String,
    /// Response body shape as free text (the Response column).
    pub response_shape: String,
    /// Auth requirement.
    pub security: SecurityKind,
}

/// The full typed API contract extracted from the architecture doc.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApiSpec {
    /// All endpoints, deduped by `(method, path)`.
    pub endpoints: Vec<Endpoint>,
    /// API title from the architecture doc H1 (best-effort).
    pub title: String,
}

impl ApiSpec {
    /// Whether the contract declares an endpoint matching `method` + `path`.
    /// Path templates match literal segments: `/api/users/:id` matches a
    /// call to `/api/users/123` but not `/api/users`.
    #[must_use]
    pub fn has_endpoint(&self, method: HttpVerb, call_path: &str) -> bool {
        self.endpoints
            .iter()
            .any(|e| e.method == method && path_template_matches(&e.path, call_path))
    }

    /// All unique `(method, path)` pairs declared. Used by validators.
    #[must_use]
    pub fn declared_paths(&self) -> Vec<(HttpVerb, &str)> {
        self.endpoints
            .iter()
            .map(|e| (e.method, e.path.as_str()))
            .collect()
    }

    /// Number of endpoints.
    #[must_use]
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Whether the contract is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }
}

/// Does a declared path template match an actual call path?
///
/// - `/api/users` matches only `/api/users`.
/// - `/api/users/:id` matches `/api/users/123` and `/api/users/abc`.
/// - `/api/:org/:repo` matches `/api/foo/bar`.
/// - A literal segment does NOT match a different literal (`/api/users`
///   ≠ `/api/orders`).
fn path_template_matches(template: &str, call_path: &str) -> bool {
    // Strip query strings / fragments from the call path.
    let call_path = call_path.split(['?', '#']).next().unwrap_or(call_path);
    let template_segments: Vec<&str> = template.trim_end_matches('/').split('/').collect();
    let call_segments: Vec<&str> = call_path.trim_end_matches('/').split('/').collect();
    if template_segments.len() != call_segments.len() {
        return false;
    }
    template_segments
        .iter()
        .zip(call_segments.iter())
        .all(|(t, c)| is_template_param(t) || t == c)
}

/// Whether a template segment is a path-parameter placeholder, across the
/// vocabularies a contract may be written in:
/// - `:id` (Express / Rails / gin) — `:` followed by at least one valid
///   param-name char (letter / digit / underscore), so a bare `:`, `::`, or
///   `:#` stays a literal segment that must match exactly (previously anything
///   starting with `:` matched anything, so `/api/:` wrongly matched
///   `/api/foo`).
/// - `{id}` (OpenAPI / FastAPI / Spring) and `<int:id>` (Django) — a whole
///   segment wrapped in braces / angle brackets. Without these, an
///   OpenAPI-style contract path `/api/users/{id}` failed to match a real call
///   `/api/users/123` and raised a systematic false `UndeclaredCall`. Mirrors
///   [`crate::backend::is_param_segment`], which already accepts these forms
///   on the backend-route side, so the two sides share one param vocabulary.
///
/// A bare `{}` / `<>` (nothing between the brackets) is NOT a parameter and
/// stays a literal.
pub(crate) fn is_template_param(segment: &str) -> bool {
    let colon = {
        let mut chars = segment.chars();
        matches!(chars.next(), Some(':'))
            && chars
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    colon
        || (segment.len() > 2 && segment.starts_with('{') && segment.ends_with('}'))
        || (segment.len() > 2 && segment.starts_with('<') && segment.ends_with('>'))
}

/// Parse the architecture Markdown into an [`ApiSpec`]. Looks for Markdown
/// tables whose header row contains `Method` and `Path` columns — every such
/// table in the API section, so a surface split into one table per module is
/// read whole — then parses each data row into a typed [`Endpoint`].
///
/// Tolerant: malformed rows are skipped, never cause failure. An architecture
/// doc with no API table yields an empty spec (the quality gate then reports
/// "no contract found").
#[must_use]
pub fn parse_architecture(arch_markdown: &str, title: &str) -> ApiSpec {
    let mut endpoints = extract_endpoints_from_table(arch_markdown);
    // Ensure operationIds are unique across the whole spec — OpenAPI requires
    // this, but `derive_operation_id` slugifies the description, so two
    // endpoints sharing a description (e.g. both "List") would collide.
    dedupe_operation_ids(&mut endpoints);
    ApiSpec {
        endpoints,
        title: title.to_string(),
    }
}

/// Disambiguate duplicate `operation_id`s in-place by appending a numeric
/// suffix (`_2`, `_3`, …) to every occurrence after the first. An endpoint
/// whose id is already unique is left untouched.
pub(crate) fn dedupe_operation_ids(endpoints: &mut [Endpoint]) {
    use std::collections::HashSet;
    let mut taken: HashSet<String> = HashSet::new();
    for ep in endpoints.iter_mut() {
        if taken.insert(ep.operation_id.clone()) {
            continue; // first occurrence of this id — keep it as-is
        }
        // Collision: pick the smallest `_N` suffix that isn't ALREADY taken, so
        // a renamed id can't collide with a pre-existing `<base>_N` (the OpenAPI
        // uniqueness this function exists to guarantee).
        let base = ep.operation_id.clone();
        let mut n = 2;
        let mut candidate = format!("{base}_{n}");
        while !taken.insert(candidate.clone()) {
            n += 1;
            candidate = format!("{base}_{n}");
        }
        ep.operation_id = candidate;
    }
}

/// Synonyms that mark a header cell as the **Method** column (case-insensitive
/// substring match).
const METHOD_SYNONYMS: &[&str] = &["method", "verb"];
/// Synonyms that mark a header cell as the **Path** column (case-insensitive
/// substring match).
const PATH_SYNONYMS: &[&str] = &["path", "endpoint", "url", "route"];

/// Whether a line is the header row of an API table: it starts with `|` and
/// names both a method-ish and a path-ish column. A whole-line `contains` of the
/// synonyms is enough to locate the row (the per-column resolution in
/// [`TableColumns::resolve`] is precise).
fn is_api_header_row(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    line.trim().starts_with('|')
        && METHOD_SYNONYMS.iter().any(|s| lower.contains(s))
        && PATH_SYNONYMS.iter().any(|s| lower.contains(s))
}

/// The column layout of one API table, resolved from its header row.
struct TableColumns {
    method: usize,
    path: usize,
    request: Option<usize>,
    response: Option<usize>,
    auth: Option<usize>,
    description: Option<usize>,
}

impl TableColumns {
    /// Resolve the columns of a header row; `None` when it has no Method or
    /// Path column.
    fn resolve(header_row: &str) -> Option<Self> {
        let headers = split_table_row(header_row);
        let col = |name: &str| -> Option<usize> {
            headers
                .iter()
                .position(|h| h.to_ascii_lowercase().trim() == name)
        };
        // Resolve a column by ANY of a set of synonyms via `contains` — mirroring
        // the tolerant Auth match below. The header row is found permissively, so
        // Method/Path resolution must be permissive too: a descriptive header like
        // `HTTP Method` / `API Path` / `Endpoint` / `Route` previously FOUND the
        // header (it contains "method"+"path") but resolved NO columns under an
        // exact `==` test, yielding an empty spec that VACUOUSLY passed the
        // UD-CODE-003 contract gate.
        let col_any = |synonyms: &[&str]| -> Option<usize> {
            headers.iter().position(|h| {
                let h = h.to_ascii_lowercase();
                let h = h.trim();
                synonyms.iter().any(|s| h.contains(s))
            })
        };
        // Auth column: accept common header variants so a correctly-authored doc
        // ("Authentication" / "Authorization" / "Security" / "Protected" / 鉴权)
        // isn't misread as all-public — which would falsely sink the auth-coverage
        // quality gate.
        let auth = headers.iter().position(|h| {
            let h = h.to_ascii_lowercase();
            let h = h.trim();
            h.contains("auth")
                || h == "security"
                || h == "protected"
                || h.contains("鉴权")
                || h.contains("权限")
        });
        Some(Self {
            method: col_any(METHOD_SYNONYMS)?,
            path: col_any(PATH_SYNONYMS)?,
            request: col("request"),
            response: col("response"),
            auth,
            description: col("description"),
        })
    }
}

/// Whether a table row is the `|---|:---:|` separator under a header.
fn is_separator_row(line: &str) -> bool {
    let cells = line.trim().replace('|', "");
    cells.contains('-')
        && cells
            .chars()
            .all(|c| c == '-' || c == ':' || c.is_whitespace())
}

/// The ATX heading (`## API surface`) on each line as `(level, text)`. A `#` line
/// inside a fenced code block (a `# comment` in a curl example) is not a heading.
fn markdown_headings<'a>(lines: &[&'a str]) -> Vec<Option<(usize, &'a str)>> {
    let mut in_fence = false;
    lines
        .iter()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                return None;
            }
            let level = trimmed.bytes().take_while(|b| *b == b'#').count();
            let text = &trimmed[level..];
            (!in_fence
                && (1..=6).contains(&level)
                && (text.is_empty() || text.starts_with([' ', '\t'])))
            .then(|| (level, text.trim()))
        })
        .collect()
}

/// Whether a heading names the app's own API section: `API surface`, `REST
/// APIs`, `Endpoints`, `Routes`, `接口设计`, `路由`. A heading about who may call
/// the API (auth, roles, permissions) or about someone else's API (external,
/// third-party) does not, even when it says "API".
fn names_api_section(heading: &str) -> bool {
    const NOT_OWN_SURFACE: &[&str] = &[
        "auth",
        "role",
        "permission",
        "external",
        "third",
        "upstream",
        "vendor",
        "鉴权",
        "鑑權",
        "认证",
        "認證",
        "授权",
        "授權",
        "权限",
        "權限",
        "角色",
        "外部",
        "第三方",
        "上游",
    ];
    let lower = heading.to_ascii_lowercase();
    if NOT_OWN_SURFACE.iter().any(|word| lower.contains(word)) {
        return false;
    }
    lower.contains("接口")
        || lower.contains("介面")
        || lower.contains("路由")
        || lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| {
                matches!(word, "api" | "apis" | "openapi")
                    || word.starts_with("endpoint")
                    || word.starts_with("route")
            })
}

/// The line index where the API section holding the first API table ends. The
/// section starts at the nearest heading above that table which names the API
/// (`## API surface`, `# 接口文档`), or failing that the nearest heading, and runs
/// until a later heading of the same or a higher level that does not name the API
/// itself. Further Method/Path tables are read only inside it, so a role matrix
/// or a table of third-party endpoints elsewhere in the doc is never taken for the
/// app's own API.
fn api_section_end(lines: &[&str], first_header: usize) -> usize {
    let headings = markdown_headings(lines);
    let above = || headings[..first_header].iter().rev().flatten();
    let level = above()
        .find(|(_, text)| names_api_section(text))
        .or_else(|| above().next())
        .map_or(6, |(level, _)| *level);
    headings
        .iter()
        .enumerate()
        .skip(first_header + 1)
        .find(|(_, heading)| {
            heading.is_some_and(|(l, text)| l <= level && !names_api_section(text))
        })
        .map_or(lines.len(), |(idx, _)| idx)
}

/// Walk the markdown and parse the rows of every Method+Path table in the API
/// section (see [`api_section_end`]), deduped by `(method, path)`.
fn extract_endpoints_from_table(md: &str) -> Vec<Endpoint> {
    let lines: Vec<&str> = md.lines().collect();
    let Some(header_idx) = lines.iter().position(|l| is_api_header_row(l)) else {
        return Vec::new();
    };
    let section_end = api_section_end(&lines, header_idx);

    let mut endpoints: Vec<Endpoint> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    // The current table's columns (`None` for a non-API table), whether it has
    // reached its data rows, and whether a non-pipe line came since the last row.
    let mut columns = TableColumns::resolve(lines[header_idx]);
    let mut has_rows = false;
    let mut after_gap = false;

    for line in lines.iter().take(section_end).skip(header_idx + 1) {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            after_gap = true;
            continue;
        }
        // A table ends at its first non-pipe line, except that an API table
        // whose rows have not started yet tolerates a gap before them. The pipe
        // line after a gap otherwise starts a new table, whose rows count only
        // when it is another API table, with its own column layout.
        if std::mem::take(&mut after_gap)
            && (has_rows || columns.is_none() || is_api_header_row(trimmed))
        {
            has_rows = false;
            columns = if is_api_header_row(trimmed) {
                TableColumns::resolve(trimmed)
            } else {
                None
            };
            continue;
        }
        if is_separator_row(trimmed) {
            continue; // separator row
        }
        has_rows = true;
        let Some(cols) = columns.as_ref() else {
            continue; // a non-API table
        };
        let (method_col, path_col) = (cols.method, cols.path);
        let cells = split_table_row(trimmed);
        if cells.len() <= method_col.max(path_col) {
            continue;
        }
        // Unwrap a verb written as a code span or in bold / italics (`` `GET` ``,
        // `**POST**`, `_PUT_`) before parsing it, like the path cell below.
        let method_cell = cells[method_col].trim().trim_matches(['`', '*', '_']);
        let Some(method) = HttpVerb::parse(method_cell) else {
            continue; // skip rows whose method isn't a real verb (e.g. "TODO")
        };
        // Strip markdown backtick wrapping so `/api/subscribe` in
        // `` `/api/subscribe` `` is recognized as a real path. Query parameters
        // or a fragment documented inline (`/api/products?page=&size=`) are not
        // part of the route, so they are dropped from the declared path.
        let path = cells[path_col].trim().trim_matches('`').trim();
        let path = path
            .split(['?', '#'])
            .next()
            .unwrap_or(path)
            .trim()
            .to_string();
        if !path.starts_with('/') {
            continue; // not a real API path
        }
        // Dedupe by (method, path).
        let key = (method.as_str().to_string(), path.clone());
        if !seen.insert(key) {
            continue;
        }
        let description = cols
            .description
            .and_then(|i| cells.get(i))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let request_shape = cols
            .request
            .and_then(|i| cells.get(i))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let response_shape = cols
            .response
            .and_then(|i| cells.get(i))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let security = cols
            .auth
            .and_then(|i| cells.get(i))
            .map(|s| SecurityKind::parse(s))
            .unwrap_or(SecurityKind::None);
        let operation_id = derive_operation_id(method, &path, &description);
        endpoints.push(Endpoint {
            method,
            path,
            operation_id,
            description,
            request_shape,
            response_shape,
            security,
        });
    }

    endpoints
}

/// Split a markdown table row `| a | b | c |` into `["a", "b", "c"]`.
/// Handles the outer pipes and trims each cell.
/// Split a markdown table row `| a | b | c |` into `["a", "b", "c"]`.
///
/// Handles the outer pipes, trims each cell, and — unlike a naive `split('|')`
/// — does NOT split on an escaped pipe `\|` inside a cell (e.g. a description
/// containing `OR\|fallback`), un-escaping it back to `|` in the result.
fn split_table_row(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            // Escaped pipe — literal '|' in the cell, not a delimiter.
            chars.next();
            current.push('|');
        } else if c == '|' {
            cells.push(current.trim().to_string());
            current = String::new();
        } else {
            current.push(c);
        }
    }
    cells.push(current.trim().to_string());
    cells
}

/// Derive a stable operationId from method + path when the doc doesn't
/// provide one. E.g. `POST /api/auth/login` → `postApiAuthLogin`.
fn derive_operation_id(method: HttpVerb, path: &str, description: &str) -> String {
    // Prefer a slugified description when present.
    if !description.trim().is_empty() {
        let slug: String = description
            .trim()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");
        if !slug.is_empty() {
            return slug.to_lowercase();
        }
    }
    // Fall back to method + path segments.
    let path_slug: String = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty() && !s.starts_with(':'))
        .collect::<Vec<_>>()
        .join("_");
    format!("{}_{}", method.as_str(), path_slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_column_accepts_header_variants() {
        // A table whose auth column is named "Authentication" (not exactly
        // "Auth") must still populate `security`, not default everything to
        // None (which would falsely sink the auth-coverage gate).
        for header in ["Authentication", "Authorization", "Security", "Protected"] {
            let doc = format!(
                "## API\n\n| Method | Path | {header} | Description |\n\
                 |---|---|---|---|\n\
                 | POST | /api/orders | Bearer | Create order |\n"
            );
            let spec = parse_architecture(&doc, "demo");
            assert_eq!(
                spec.endpoints.first().map(|e| e.security),
                Some(SecurityKind::Bearer),
                "header `{header}` should be read as the auth column",
            );
        }
    }

    #[test]
    fn resolves_columns_with_descriptive_method_path_headers() {
        // M6 regression: a header like `HTTP Method | API Path` used to FIND the
        // header row (it contains "method" + "path") but resolve NO columns under
        // the exact `==` match — producing an empty spec that VACUOUSLY passed the
        // UD-CODE-003 contract gate. Method/Path now resolve permissively
        // (`contains`), like the Auth column.
        let md = "## API\n\n\
                  | HTTP Method | API Path | Auth | Description |\n\
                  |---|---|---|---|\n\
                  | POST | /api/orders | Bearer | Create order |\n\
                  | GET | /api/orders/:id | none | Get order |\n";
        let spec = parse_architecture(md, "demo");
        assert_eq!(
            spec.len(),
            2,
            "descriptive `HTTP Method`/`API Path` headers must resolve real columns"
        );
        assert_eq!(spec.endpoints[0].method, HttpVerb::Post);
        assert_eq!(spec.endpoints[0].path, "/api/orders");
        assert_eq!(spec.endpoints[0].security, SecurityKind::Bearer);
        assert_eq!(spec.endpoints[1].method, HttpVerb::Get);
        assert_eq!(spec.endpoints[1].path, "/api/orders/:id");
    }

    #[test]
    fn resolves_columns_with_method_path_synonym_headers() {
        // The same tolerance covers the documented synonyms: Verb (method) and
        // Endpoint / Route / Request URL (path).
        for (method_header, path_header) in [
            ("Verb", "Endpoint"),
            ("Method", "Route"),
            ("HTTP Verb", "Request URL"),
        ] {
            let md = format!(
                "| {method_header} | {path_header} | Description |\n\
                 |---|---|---|\n\
                 | GET | /api/x | List |\n"
            );
            let spec = parse_architecture(&md, "t");
            assert_eq!(
                spec.len(),
                1,
                "headers `{method_header}`/`{path_header}` must resolve columns"
            );
            assert_eq!(spec.endpoints[0].method, HttpVerb::Get);
            assert_eq!(spec.endpoints[0].path, "/api/x");
        }
    }

    const SAMPLE_ARCH: &str = "# Architecture — demo

## API surface

| Method | Path | Request | Response | Auth | Description |
|---|---|---|---|---|---|
| GET | /api/health | - | { ok: true } | none | Health check |
| POST | /api/auth/login | { email, password } | { token, user } | none | Login |
| GET | /api/auth/me | - | { user } | bearer | Current user |
| DELETE | /api/users/:id | - | { deleted: true } | bearer | Delete user |
| TODO | /api/... | TODO | TODO | TODO | Add endpoints |

## Data model
";

    #[test]
    fn parses_real_table() {
        let spec = parse_architecture(SAMPLE_ARCH, "demo");
        assert_eq!(spec.len(), 4); // the TODO row is dropped
        assert_eq!(spec.endpoints[0].method, HttpVerb::Get);
        assert_eq!(spec.endpoints[0].path, "/api/health");
        assert_eq!(spec.endpoints[1].method, HttpVerb::Post);
        assert_eq!(spec.endpoints[1].path, "/api/auth/login");
    }

    #[test]
    fn parse_architecture_reads_every_api_table() {
        // Multi-module docs split the API surface into one table per module.
        // Every table in the API section counts, not just the first one.
        let md = "# Architecture — shop\n\n\
                  ## API surface\n\n\
                  ### 用户模块\n\n\
                  | Method | Path | Auth | Description |\n|---|---|---|---|\n\
                  | POST | /api/auth/login | 无 | 登录 |\n\
                  | GET | /api/users/:id | Bearer | 用户详情 |\n\n\
                  调用示例：\n\n\
                  ```bash\n# 获取订单列表\ncurl /api/orders\n```\n\n\
                  ### 订单模块\n\n\
                  | Path | Method | Description |\n|:---|:---:|---|\n\
                  | /api/orders | GET | 订单列表 |\n\
                  | /api/orders | POST | 下单 |\n\n\
                  | Method | Path | Description |\n|---|---|---|\n\
                  | DELETE | /api/orders/:id | 取消订单 |\n\n\
                  ## API error convention\n\n\
                  | HTTP | Code | Meaning |\n|---|---|---|\n| 404 | NOT_FOUND | missing |\n\n\
                  ## Data model\n\n\
                  | Field | Type | Description |\n|---|---|---|\n| id | uuid | key |\n";
        let spec = parse_architecture(md, "shop");
        let got: Vec<(HttpVerb, &str)> = spec.declared_paths();
        assert_eq!(
            got,
            vec![
                (HttpVerb::Post, "/api/auth/login"),
                (HttpVerb::Get, "/api/users/:id"),
                (HttpVerb::Get, "/api/orders"),
                (HttpVerb::Post, "/api/orders"),
                (HttpVerb::Delete, "/api/orders/:id"),
            ]
        );
        assert_eq!(spec.endpoints[0].security, SecurityKind::None);
        assert_eq!(spec.endpoints[1].security, SecurityKind::Bearer);
    }

    #[test]
    fn method_path_table_outside_the_api_section_is_not_the_contract() {
        // A role matrix (or a list of third-party endpoints) elsewhere in the
        // doc may also have Method and Path columns. It must not add planned
        // endpoints the app never promised to serve.
        let md = "## API surface\n\n\
                  | Method | Path | Description |\n|---|---|---|\n\
                  | GET | /api/todos | List |\n\n\
                  ## Authentication & authorization\n\n\
                  | Role | Method | Path |\n|---|---|---|\n\
                  | admin | DELETE | /api/admin/* |\n\n\
                  ## External services\n\n\
                  | Method | Path | Provider |\n|---|---|---|\n\
                  | POST | /v1/chat/completions | OpenAI |\n";
        let spec = parse_architecture(md, "t");
        assert_eq!(spec.declared_paths(), vec![(HttpVerb::Get, "/api/todos")]);
        // Saying "API" does not make a permissions or third-party section part
        // of the app's own surface.
        let md = "## API surface\n\n\
                  | Method | Path | Description |\n|---|---|---|\n\
                  | GET | /api/todos | List |\n\n\
                  ## API 权限矩阵\n\n\
                  | Role | Method | Path |\n|---|---|---|\n\
                  | admin | DELETE | /api/admin/* |\n\n\
                  ## Third-party APIs\n\n\
                  | Method | Path | Provider |\n|---|---|---|\n\
                  | POST | /v1/charges | Stripe |\n";
        let spec = parse_architecture(md, "t");
        assert_eq!(spec.declared_paths(), vec![(HttpVerb::Get, "/api/todos")]);
    }

    #[test]
    fn blank_line_before_the_first_row_does_not_end_the_table() {
        // The parser always tolerated a gap between an API table's header and
        // its first row; reading every table must not lose that.
        let md = "| Method | Path | Description |\n|---|---|---|\n\n\
                  | GET | /api/todos | List |\n\
                  | POST | /api/todos | Create |\n\n\
                  | Field | Type |\n|---|---|\n| id | uuid |\n";
        let spec = parse_architecture(md, "t");
        assert_eq!(
            spec.declared_paths(),
            vec![
                (HttpVerb::Get, "/api/todos"),
                (HttpVerb::Post, "/api/todos")
            ]
        );
    }

    #[test]
    fn parses_backticked_and_bold_verbs() {
        // LLM-written tables often wrap the verb in a code span or bold. Every
        // row used to be skipped, leaving an empty contract.
        let md = "| Method | Path | Description |\n|---|---|---|\n\
                  | `GET` | `/api/products` | List |\n\
                  | **POST** | /api/orders | Create |\n\
                  | __PUT__ | /api/orders/:id | Update |\n\
                  | **TODO** | /api/later | Not a verb |\n";
        let spec = parse_architecture(md, "t");
        assert!(spec.has_endpoint(HttpVerb::Get, "/api/products"));
        assert!(spec.has_endpoint(HttpVerb::Post, "/api/orders"));
        assert!(spec.has_endpoint(HttpVerb::Put, "/api/orders/7"));
        // A cell that is still not a verb once unwrapped stays skipped.
        assert_eq!(spec.len(), 3, "{:?}", spec.declared_paths());
    }

    #[test]
    fn parses_security_kinds() {
        let spec = parse_architecture(SAMPLE_ARCH, "demo");
        assert_eq!(spec.endpoints[0].security, SecurityKind::None); // health
        assert_eq!(spec.endpoints[2].security, SecurityKind::Bearer); // me
        assert_eq!(spec.endpoints[3].security, SecurityKind::Bearer); // delete user
    }

    #[test]
    fn derives_operation_ids() {
        let spec = parse_architecture(SAMPLE_ARCH, "demo");
        assert_eq!(spec.endpoints[0].operation_id, "health_check");
        assert_eq!(spec.endpoints[1].operation_id, "login");
    }

    #[test]
    fn split_table_row_handles_escaped_pipe() {
        // Regression: a description cell containing an escaped pipe (common
        // when noting alternatives like "POST\|GET") used to be split into
        // two cells, corrupting the column alignment.
        let cells = split_table_row("| GET | /api/x | do thing OR\\|fallback | none |");
        assert_eq!(
            cells.len(),
            4,
            "escaped pipe must not add a cell: {cells:?}"
        );
        assert!(
            cells[2].contains("OR|fallback"),
            "escaped pipe must be un-escaped to literal | in the cell: {cells:?}"
        );
    }

    #[test]
    fn operation_ids_disambiguated_when_descriptions_collide() {
        // Regression: two endpoints with the SAME description ("List") used
        // to both get operation_id = "list", violating OpenAPI uniqueness.
        // The table uses distinct paths so both rows survive dedup.
        let md = "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | /api/users | - | - | none | List |\n| GET | /api/posts | - | - | none | List |\n";
        let spec = parse_architecture(md, "t");
        assert_eq!(spec.len(), 2);
        let ids: Vec<&str> = spec
            .endpoints
            .iter()
            .map(|e| e.operation_id.as_str())
            .collect();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "operationIds must be unique, got {ids:?}"
        );
        // First keeps "list", second becomes "list_2".
        assert_eq!(spec.endpoints[0].operation_id, "list");
        assert_eq!(spec.endpoints[1].operation_id, "list_2");
    }

    #[test]
    fn dedupes_repeated_endpoints() {
        let md = "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | /api/x | - | - | none | First |\n| GET | /api/x | - | - | none | Duplicate |\n";
        let spec = parse_architecture(md, "t");
        assert_eq!(spec.len(), 1);
    }

    #[test]
    fn no_table_yields_empty_spec() {
        let md = "# Architecture\n\n## API surface\n\nNo table here.";
        assert!(parse_architecture(md, "t").is_empty());
    }

    #[test]
    fn skips_non_path_rows() {
        // A row whose path column doesn't start with `/` is dropped.
        let md = "| Method | Path | Request | Response | Auth | Description |\n|---|---|---|---|---|---|\n| GET | health | - | - | none | No slash |\n";
        assert!(parse_architecture(md, "t").is_empty());
    }

    #[test]
    fn path_template_matches_literal() {
        assert!(path_template_matches("/api/users", "/api/users"));
        assert!(!path_template_matches("/api/users", "/api/orders"));
    }

    #[test]
    fn path_template_matches_param() {
        assert!(path_template_matches("/api/users/:id", "/api/users/123"));
        assert!(path_template_matches(
            "/api/users/:id",
            "/api/users/abc-xyz"
        ));
        assert!(!path_template_matches("/api/users/:id", "/api/users"));
        assert!(!path_template_matches(
            "/api/users/:id",
            "/api/users/123/posts"
        ));
    }

    #[test]
    fn path_template_matches_multi_param() {
        assert!(path_template_matches("/api/:org/:repo", "/api/foo/bar"));
        assert!(!path_template_matches("/api/:org/:repo", "/api/foo"));
    }

    #[test]
    fn path_template_matches_brace_and_angle_params() {
        // Fix #3: OpenAPI `{id}` and Django `<int:id>` param segments match a
        // concrete call segment, sharing the backend param vocabulary.
        assert!(path_template_matches("/api/users/{id}", "/api/users/123"));
        assert!(path_template_matches(
            "/api/orders/<int:id>",
            "/api/orders/42"
        ));
        assert!(path_template_matches("/api/{org}/{repo}", "/api/foo/bar"));
        // A conflicting literal still rejects.
        assert!(!path_template_matches("/api/users/{id}", "/api/orders/1"));
    }

    #[test]
    fn is_template_param_vocabulary() {
        for yes in [":id", ":userId", "{id}", "<int:id>", "<slug>"] {
            assert!(is_template_param(yes), "{yes:?} should be a param");
        }
        // Not params: bare punctuation, empty brackets, plain literals.
        for no in [":", "::", "{}", "<>", "users", "v1", "{", "id}"] {
            assert!(!is_template_param(no), "{no:?} must NOT be a param");
        }
    }

    #[test]
    fn path_template_strips_query_string() {
        assert!(path_template_matches(
            "/api/users",
            "/api/users?include=email"
        ));
        assert!(path_template_matches("/api/users", "/api/users#section"));
    }

    #[test]
    fn path_template_trailing_slash_normalised() {
        assert!(path_template_matches("/api/users/", "/api/users"));
        assert!(path_template_matches("/api/users", "/api/users/"));
    }

    #[test]
    fn has_endpoint_finds_declared() {
        let spec = parse_architecture(SAMPLE_ARCH, "demo");
        assert!(spec.has_endpoint(HttpVerb::Get, "/api/health"));
        assert!(spec.has_endpoint(HttpVerb::Post, "/api/auth/login"));
        assert!(spec.has_endpoint(HttpVerb::Delete, "/api/users/42"));
        assert!(!spec.has_endpoint(HttpVerb::Put, "/api/health"));
        assert!(!spec.has_endpoint(HttpVerb::Get, "/api/nonexistent"));
    }

    #[test]
    fn security_kind_synonyms() {
        assert_eq!(SecurityKind::parse("none"), SecurityKind::None);
        assert_eq!(SecurityKind::parse(""), SecurityKind::None);
        assert_eq!(SecurityKind::parse("bearer token"), SecurityKind::Bearer);
        assert_eq!(SecurityKind::parse("JWT"), SecurityKind::Bearer);
        assert_eq!(SecurityKind::parse("api-key"), SecurityKind::ApiKey);
        assert_eq!(SecurityKind::parse("OAuth2"), SecurityKind::OAuth2);
        assert_eq!(SecurityKind::parse("session cookie"), SecurityKind::Session);
    }

    #[test]
    fn security_kind_public_markers() {
        // Auth cells are free text, and Chinese docs write Chinese markers. A
        // placeholder or negative marker means the endpoint is public; before,
        // these fell through to `Other` (counted as protected) or, for
        // `No token`, to `Bearer`.
        for public in [
            "-",
            "—",
            "n/a",
            "N/A",
            "无",
            "無",
            "否",
            "否（公开）",
            "不需要",
            "无需登录",
            "無需登入",
            "公开",
            "公開接口",
            "匿名",
            "免登录",
            "✗",
            "×",
            "None (public)",
            "No token",
            "no-auth",
            "Not required",
            "Public (rate limited)",
            "anonymous",
        ] {
            assert_eq!(
                SecurityKind::parse(public),
                SecurityKind::None,
                "{public:?} marks a public endpoint"
            );
        }
        // Cells that require auth keep their protected kind.
        assert_eq!(SecurityKind::parse("Bearer token"), SecurityKind::Bearer);
        assert_eq!(SecurityKind::parse("需要 token"), SecurityKind::Bearer);
        for protected in [
            "是",
            "需要登录",
            "登录用户",
            "required",
            "admin only",
            "无效即拒绝",
        ] {
            assert_eq!(
                SecurityKind::parse(protected),
                SecurityKind::Other,
                "{protected:?} requires auth"
            );
        }
    }

    #[test]
    fn httpverb_round_trips() {
        assert_eq!(HttpVerb::parse("post"), Some(HttpVerb::Post));
        assert_eq!(HttpVerb::parse("DELETE"), Some(HttpVerb::Delete));
        assert_eq!(HttpVerb::parse("bogus"), None);
        assert_eq!(HttpVerb::Post.as_str(), "post");
    }
}
