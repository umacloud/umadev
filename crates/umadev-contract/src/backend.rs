//! Extract **backend route registrations** from the worker's produced server
//! source — the symmetric counterpart of [`crate::extract`] (which scans
//! *frontend* `fetch`/`axios` call-sites).
//!
//! ## Why this exists
//! The task-level acceptance check ("did the base actually implement the
//! planned endpoint?") used to search the *concatenated* project source for a
//! planned path's static prefix. That is substring theatre: a planned endpoint
//! counted as "implemented" if its path appeared **anywhere** — including in a
//! frontend `fetch('/api/login')` call or a `// TODO: app.post('/api/login')`
//! comment. So a backend that existed only as frontend call-sites falsely
//! PASSED. This module fixes that by extracting only **real server route
//! REGISTRATIONS** — `app.get(...)`, `@app.route(...)`, `@GetMapping(...)`,
//! `.route("/x", get(...))`, `mux.HandleFunc(...)`, … — comment-stripped, and
//! never a `fetch`/`axios` call and never a comment.
//!
//! ## Frameworks covered (registration sites only)
//! - **JS / TS** — Express / Koa / Fastify method calls
//!   (`app.get` / `router.post` / `fastify.put` / `server.delete`), Fastify /
//!   Hapi object routes (`.route({ method, url })`), and NestJS decorators
//!   (`@Get('/x')` combined with the `@Controller('base')` class prefix).
//! - **Python** — Flask / blueprint (`@app.route('/x', methods=[...])`),
//!   FastAPI (`@app.get('/x')` / `@router.post('/x')` / `@app.api_route`), and
//!   Django URLconf (`path(...)` / `re_path(...)` / `url(...)`).
//! - **Rust** — axum (`.route("/x", get(h).post(h2))`), actix
//!   (`#[get("/x")]` attribute macros, `web::resource("/x").route(...)`).
//! - **Go** — gin / chi (`r.GET("/x", …)` / `r.Get("/x", …)`), net/http mux
//!   (`mux.HandleFunc("GET /x", …)` / `mux.Handle("/x", …)`).
//! - **Java / Kotlin** — Spring (`@GetMapping("/x")` / `@RequestMapping(...)`
//!   combined with a class-level `@RequestMapping("/base")` prefix).
//!
//! ## Safety contract
//! Fail-open, bounded, deterministic. An unknown framework yields an empty
//! result (never a panic); an unreadable / non-UTF-8 / oversized file is
//! skipped; the walk is depth- and count-bounded. A `None` method models a
//! wildcard registration (`app.all` / `app.use` / Django / Go mux / gin
//! `.Any`) that matches any planned verb.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::parse::{is_template_param, HttpVerb};

/// One backend route registration found in server source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRoute {
    /// Workspace-relative file the registration was found in.
    pub file: String,
    /// HTTP method the route is registered for. `None` is a **wildcard**
    /// registration (`app.all` / `app.use` mount / Django `path` / Go mux /
    /// gin `.Any`) that matches any planned method.
    pub method: Option<HttpVerb>,
    /// The registered path template, e.g. `/api/users/:id`. Kept verbatim as
    /// captured (leading slash optional for Django-style patterns); path
    /// parameters (`:id` / `{id}` / `<int:id>` / `*rest`) are normalised only
    /// at match time (see [`route_registered`]).
    pub path: String,
}

/// Backend source extensions worth scanning. Deliberately limited to the
/// languages we have registration extractors for — a language we cannot parse
/// contributes nothing, so scanning it is wasted work (and the caller treats
/// "no registrations extracted" as fail-open, never a false failure).
const BACKEND_EXTS: &[&str] = &[
    "js", "mjs", "cjs", "ts", "tsx", "jsx", "py", "rs", "go", "java", "kt",
];

/// Directories that never contain hand-written server source. Mirrors the
/// frontend extractor's skip set so the two walkers agree on what is vendored /
/// generated / UmaDev-owned.
const SKIP_DIRS: &[&str] = &[
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
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "target",
    "vendor",
    ".gradle",
    ".git",
    ".hg",
    ".svn",
    ".umadev",
    "output",
    "release",
    "knowledge",
    ".cache",
];

/// Maximum directory depth for the backend-source walk. Real Spring/Nest/Go
/// monorepos can nest routes/controllers deeper than 8 levels under modules;
/// file count + skip dirs are the primary guard against pathological trees.
const MAX_DEPTH: usize = 16;
/// Maximum number of source files scanned (guards a pathological monorepo).
const MAX_FILES: usize = 800;
/// Skip files larger than this (a bundled / generated file, not hand-written
/// route source). Keeps the comment-strip + regex work bounded.
const MAX_FILE_BYTES: u64 = 600_000;

/// The language family a file belongs to, used to dispatch the right set of
/// registration regexes (and the correct comment syntax).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    /// JavaScript / TypeScript — Express / Koa / Fastify / NestJS.
    JsTs,
    /// Python — Flask / FastAPI / Django.
    Py,
    /// Rust — axum / actix.
    Rust,
    /// Go — gin / chi / net-http.
    Go,
    /// Java / Kotlin — Spring.
    Java,
}

impl Lang {
    /// Map a file extension to its language family, or `None` when unsupported.
    fn from_ext(ext: &str) -> Option<Self> {
        match ext {
            "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => Some(Self::JsTs),
            "py" => Some(Self::Py),
            "rs" => Some(Self::Rust),
            "go" => Some(Self::Go),
            "java" | "kt" => Some(Self::Java),
            _ => None,
        }
    }
}

/// Scan every backend source file under `project_root` and return the route
/// registrations found, deduped by `(method, path)`.
///
/// Returns an empty vec when no recognised registration exists (fail-open —
/// the caller then treats the project as "no backend we can read" and does not
/// falsely fail it).
#[must_use]
pub fn extract_backend_routes(project_root: &Path) -> Vec<BackendRoute> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_backend_sources(project_root, &mut files, 0);
    let mut routes: Vec<BackendRoute> = Vec::new();
    for file in &files {
        let Ok(meta) = std::fs::metadata(file) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(file) else {
            continue; // non-UTF-8 / unreadable → fail-open skip
        };
        let ext = file.extension().and_then(|s| s.to_str()).unwrap_or("");
        let Some(lang) = Lang::from_ext(ext) else {
            continue;
        };
        let rel = file
            .strip_prefix(project_root)
            .map(|p| p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
            .unwrap_or_else(|_| {
                file.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            });
        routes.extend(extract_from_file(&rel, &content, lang));
    }
    // Dedupe across ALL files by (method, path); keep the first-seen file.
    let mut seen: std::collections::HashSet<(Option<HttpVerb>, String)> =
        std::collections::HashSet::new();
    routes.retain(|r| seen.insert((r.method, r.path.clone())));
    routes
}

/// Extract registrations from one file's content. Public-in-crate so tests and
/// [`crate::validate`] can exercise it without a real tree.
#[must_use]
fn extract_from_file(file: &str, content: &str, lang: Lang) -> Vec<BackendRoute> {
    // Strip comments first: a commented-out `// app.post('/api/login')` must
    // NEVER count as a registration (that is half the bug being fixed).
    let src = strip_comments(content, lang);
    let mut routes: Vec<BackendRoute> = Vec::new();
    match lang {
        Lang::JsTs => extract_js_ts(file, &src, &mut routes),
        Lang::Py => extract_py(file, &src, &mut routes),
        Lang::Rust => extract_rust(file, &src, &mut routes),
        Lang::Go => extract_go(file, &src, &mut routes),
        Lang::Java => extract_java(file, &src, &mut routes),
    }
    routes
}

/// Push a `(method, path)` registration, deduped within the current file.
fn push_route(routes: &mut Vec<BackendRoute>, file: &str, method: Option<HttpVerb>, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let path = path.to_string();
    if routes.iter().any(|r| r.method == method && r.path == path) {
        return;
    }
    routes.push(BackendRoute {
        file: file.to_string(),
        method,
        path,
    });
}

/// A parsed method token: a concrete verb or a recognised wildcard.
#[derive(Debug, Clone, Copy)]
enum ParsedVerb {
    /// A recognised catch-all (`all` / `any`) → a wildcard registration.
    Wildcard,
    /// A concrete HTTP verb.
    Verb(HttpVerb),
}

impl ParsedVerb {
    /// The registration method: `None` for a wildcard, `Some(v)` for a verb.
    fn into_method(self) -> Option<HttpVerb> {
        match self {
            Self::Wildcard => None,
            Self::Verb(v) => Some(v),
        }
    }
}

/// Parse a method token to a [`ParsedVerb`]. `all` / `any` are wildcards; a
/// standard verb parses to itself; anything else is `None` (not a method).
fn parse_method(s: &str) -> Option<ParsedVerb> {
    let lower = s.trim().to_ascii_lowercase();
    if lower == "all" || lower == "any" {
        return Some(ParsedVerb::Wildcard);
    }
    HttpVerb::parse(&lower).map(ParsedVerb::Verb)
}

// ---------------------------------------------------------------------------
// JS / TS — Express / Koa / Fastify / NestJS
// ---------------------------------------------------------------------------

/// `app.get('/x'` / `router.post('/x'` / `fastify.put('/x'` / `server.delete`.
/// The receiver is restricted to the canonical *server* handles so a frontend
/// `axios.get(...)` / `api.get(...)` / `fetch(...)` is never mistaken for a
/// backend registration (that is the other half of the bug being fixed).
fn js_method_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(?:app|router|fastify|server)\s*\.\s*(?P<method>get|post|put|delete|patch|head|options|all)\s*\(\s*[\x27\x22\x60](?P<path>/[^\x27\x22\x60\s]*)",
        )
        .expect("js method regex well-formed")
    })
}

/// Fastify / Hapi object route: `.route({ method: 'GET', url: '/x' })`.
fn js_object_route_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\.route\s*\(\s*\{(?P<body>[^}]*)\}")
            .expect("js object-route regex well-formed")
    })
}

/// The `method:` value inside a Fastify/Hapi object route (a single verb or an
/// array of verbs).
fn js_object_method_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"method\s*:\s*(?P<mval>\[[^\]]*\]|[\x27\x22\x60][A-Za-z]+[\x27\x22\x60])")
            .expect("js object-method regex well-formed")
    })
}

/// The `url:` / `path:` value inside a Fastify/Hapi object route.
fn js_object_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:path|url)\s*:\s*[\x27\x22\x60](?P<p>/[^\x27\x22\x60]*)")
            .expect("js object-path regex well-formed")
    })
}

/// NestJS `@Controller('base')` class prefix (optional; may be `@Controller()`).
fn nest_controller_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"@Controller\s*\(\s*[\x27\x22\x60](?P<p>[^\x27\x22\x60]*)")
            .expect("nest controller regex well-formed")
    })
}

/// NestJS method decorator: `@Get('/x')` / `@Post()` / `@All(':id')`.
fn nest_method_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"@(?P<m>Get|Post|Put|Delete|Patch|Head|Options|All)\s*\(\s*(?:[\x27\x22\x60](?P<path>[^\x27\x22\x60]*)[\x27\x22\x60])?",
        )
        .expect("nest method regex well-formed")
    })
}

/// Extract every JS/TS registration form from a (comment-stripped) file.
fn extract_js_ts(file: &str, src: &str, routes: &mut Vec<BackendRoute>) {
    // Express / Koa / Fastify method calls.
    for cap in js_method_regex().captures_iter(src) {
        let method = cap.name("method").map(|m| m.as_str()).unwrap_or("");
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if let Some(m) = parse_method(method) {
            push_route(routes, file, m.into_method(), path);
        }
    }
    // Fastify / Hapi object routes.
    for cap in js_object_route_regex().captures_iter(src) {
        let body = cap.name("body").map(|m| m.as_str()).unwrap_or("");
        let Some(path) = js_object_path_regex()
            .captures(body)
            .and_then(|c| c.name("p").map(|m| m.as_str().to_string()))
        else {
            continue;
        };
        let mut any_method = false;
        for mc in js_object_method_regex().captures_iter(body) {
            let mval = mc.name("mval").map(|m| m.as_str()).unwrap_or("");
            for verb in mval.split([',', '[', ']', '\'', '"', '`', ' ']) {
                if let Some(m) = parse_method(verb) {
                    push_route(routes, file, m.into_method(), &path);
                    any_method = true;
                }
            }
        }
        if !any_method {
            push_route(routes, file, None, &path);
        }
    }
    // NestJS decorators, combined with the class-level @Controller prefix.
    let prefix = nest_controller_regex()
        .captures(src)
        .and_then(|c| c.name("p").map(|m| m.as_str().to_string()))
        .unwrap_or_default();
    for cap in nest_method_regex().captures_iter(src) {
        let verb = cap.name("m").map(|m| m.as_str()).unwrap_or("");
        let sub = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        let path = combine_paths(&prefix, sub);
        if let Some(m) = parse_method(verb) {
            push_route(routes, file, m.into_method(), &path);
        }
    }
}

// ---------------------------------------------------------------------------
// Python — Flask / FastAPI / Django
// ---------------------------------------------------------------------------

/// FastAPI method decorator: `@app.get('/x')` / `@router.post('/x')`.
fn py_fastapi_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"@\s*\w+\s*\.\s*(?P<method>get|post|put|delete|patch|head|options)\s*\(\s*[\x27\x22](?P<path>/[^\x27\x22]*)",
        )
        .expect("py fastapi regex well-formed")
    })
}

/// Flask / blueprint route: `@app.route('/x', methods=[...])` (also FastAPI's
/// `@app.api_route`). Method defaults to GET (Flask's default) when no
/// `methods=` list is present.
fn py_flask_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"@\s*\w+\s*\.\s*(?:route|api_route)\s*\(\s*[\x27\x22](?P<path>/[^\x27\x22]*)[\x27\x22](?P<rest>[^)]*)",
        )
        .expect("py flask regex well-formed")
    })
}

/// The `methods=[...]` list of a Flask / api_route decorator.
fn py_methods_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"methods\s*=\s*\[(?P<list>[^\]]*)\]").expect("py methods regex well-formed")
    })
}

/// Django URLconf entry: `path('users/', …)` / `re_path(r'^users/$', …)` /
/// `url(...)`. Method-less → wildcard.
fn py_django_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:path|re_path|url)\s*\(\s*[rRbuU]?[\x27\x22](?P<path>[^\x27\x22]*)")
            .expect("py django regex well-formed")
    })
}

/// Extract every Python registration form from a (comment-stripped) file.
fn extract_py(file: &str, src: &str, routes: &mut Vec<BackendRoute>) {
    for cap in py_fastapi_regex().captures_iter(src) {
        let method = cap.name("method").map(|m| m.as_str()).unwrap_or("");
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if let Some(m) = parse_method(method) {
            push_route(routes, file, m.into_method(), path);
        }
    }
    for cap in py_flask_regex().captures_iter(src) {
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        let rest = cap.name("rest").map(|m| m.as_str()).unwrap_or("");
        let mut any_method = false;
        if let Some(mc) = py_methods_regex().captures(rest) {
            let list = mc.name("list").map(|m| m.as_str()).unwrap_or("");
            for verb in list.split([',', '\'', '"', ' ']) {
                if let Some(m) = parse_method(verb) {
                    push_route(routes, file, m.into_method(), path);
                    any_method = true;
                }
            }
        }
        if !any_method {
            // Flask's default when no methods= is GET (plus implicit HEAD).
            push_route(routes, file, Some(HttpVerb::Get), path);
        }
    }
    for cap in py_django_regex().captures_iter(src) {
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        // Django dispatches methods inside the view → wildcard registration.
        push_route(routes, file, None, path);
    }
}

// ---------------------------------------------------------------------------
// Rust — axum / actix
// ---------------------------------------------------------------------------

/// axum router: `.route("/x", get(h).post(h2))`. Captures the path plus a
/// bounded window of the handler expression, which is then scanned for method
/// constructor fns.
fn rust_axum_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\.route\s*\(\s*\x22(?P<path>/[^\x22]*)\x22\s*,\s*(?P<handlers>[^;]{0,240})")
            .expect("rust axum regex well-formed")
    })
}

/// actix resource: `web::resource("/x").route(web::get().to(h))`.
fn rust_actix_resource_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"web::resource\s*\(\s*\x22(?P<path>/[^\x22]*)\x22\s*\)(?P<handlers>[^;]{0,240})",
        )
        .expect("rust actix resource regex well-formed")
    })
}

/// actix attribute macro: `#[get("/x")]` / `#[post("/x")]`.
fn rust_actix_attr_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"#\[\s*(?P<m>get|post|put|delete|patch|head)\s*\(\s*\x22(?P<path>/[^\x22]*)\x22",
        )
        .expect("rust actix attr regex well-formed")
    })
}

/// Method constructor fns inside an axum / actix handler expression:
/// `get(` / `post(` / `web::put(` / `.any(`.
fn rust_verb_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?P<m>get|post|put|delete|patch|head|options|any)\s*\(")
            .expect("rust verb regex well-formed")
    })
}

/// Extract every Rust registration form from a (comment-stripped) file.
fn extract_rust(file: &str, src: &str, routes: &mut Vec<BackendRoute>) {
    for regex in [rust_axum_regex(), rust_actix_resource_regex()] {
        for cap in regex.captures_iter(src) {
            let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
            let handlers = cap.name("handlers").map(|m| m.as_str()).unwrap_or("");
            let mut any_method = false;
            for vc in rust_verb_regex().captures_iter(handlers) {
                let verb = vc.name("m").map(|m| m.as_str()).unwrap_or("");
                if let Some(m) = parse_method(verb) {
                    push_route(routes, file, m.into_method(), path);
                    any_method = true;
                }
            }
            if !any_method {
                push_route(routes, file, None, path);
            }
        }
    }
    for cap in rust_actix_attr_regex().captures_iter(src) {
        let verb = cap.name("m").map(|m| m.as_str()).unwrap_or("");
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if let Some(m) = parse_method(verb) {
            push_route(routes, file, m.into_method(), path);
        }
    }
}

// ---------------------------------------------------------------------------
// Go — gin / chi / net-http
// ---------------------------------------------------------------------------

/// gin / chi method call: `r.GET("/x", …)` / `router.Get("/x", …)`.
fn go_method_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b\w+\s*\.\s*(?P<method>GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS|Any|Get|Post|Put|Delete|Patch|Head|Options)\s*\(\s*\x22(?P<path>/[^\x22]*)\x22",
        )
        .expect("go method regex well-formed")
    })
}

/// net/http mux: `mux.HandleFunc("GET /x", …)` / `mux.Handle("/x", …)`. The
/// method may be embedded in the pattern (Go 1.22+ `"GET /x"`); otherwise the
/// registration is a wildcard.
fn go_mux_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\.(?:HandleFunc|Handle)\s*\(\s*\x22(?P<pat>[^\x22]*)\x22")
            .expect("go mux regex well-formed")
    })
}

/// Extract every Go registration form from a (comment-stripped) file.
fn extract_go(file: &str, src: &str, routes: &mut Vec<BackendRoute>) {
    for cap in go_method_regex().captures_iter(src) {
        let method = cap.name("method").map(|m| m.as_str()).unwrap_or("");
        let path = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        if let Some(m) = parse_method(method) {
            push_route(routes, file, m.into_method(), path);
        }
    }
    for cap in go_mux_regex().captures_iter(src) {
        let pat = cap.name("pat").map(|m| m.as_str()).unwrap_or("");
        // Go 1.22 patterns may be `"METHOD /path"`; split a leading verb off.
        let (method, path) = match pat.split_once(' ') {
            Some((verb, rest)) if HttpVerb::parse(verb).is_some() => {
                (HttpVerb::parse(verb), rest.trim())
            }
            _ => (None, pat),
        };
        if path.starts_with('/') {
            push_route(routes, file, method, path);
        }
    }
}

// ---------------------------------------------------------------------------
// Java / Kotlin — Spring
// ---------------------------------------------------------------------------

/// Spring method mapping: `@GetMapping("/x")` / `@PostMapping(value = "/x")`.
/// The path is optional (`@GetMapping` alone maps the class prefix).
fn spring_method_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Parens are optional: `@GetMapping` alone maps the class prefix, while
        // `@GetMapping("/x")` / `@PostMapping(value = "/x")` carry a sub-path. A
        // word boundary after `Mapping` stops a spurious `@GetMappingXyz` match.
        Regex::new(
            r"@\s*(?P<m>Get|Post|Put|Delete|Patch)Mapping\b\s*(?:\(\s*(?:(?:value|path)\s*=\s*)?\{?\s*(?:[\x27\x22](?P<path>[^\x27\x22]*)[\x27\x22])?)?",
        )
        .expect("spring method regex well-formed")
    })
}

/// Spring `@RequestMapping(...)` — the class-level form is the shared prefix;
/// a method-level form is an endpoint. Captures the whole arg list for parsing.
fn spring_request_mapping_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"@\s*RequestMapping\s*\(\s*(?P<body>[^)]*)\)")
            .expect("spring request-mapping regex well-formed")
    })
}

/// The first quoted path inside a `@RequestMapping` arg body.
fn spring_rm_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[\x27\x22](?P<path>/[^\x27\x22]*)").expect("spring rm-path regex well-formed")
    })
}

/// The `method = RequestMethod.GET` verb inside a `@RequestMapping` arg body.
fn spring_rm_method_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"RequestMethod\s*\.\s*(?P<m>GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)")
            .expect("spring rm-method regex well-formed")
    })
}

/// Extract every Spring registration form from a (comment-stripped) file.
fn extract_java(file: &str, src: &str, routes: &mut Vec<BackendRoute>) {
    // Gather every @RequestMapping in document order. The first with a path is
    // taken as the class-level prefix (a common Spring convention); the rest
    // are treated as method endpoints combined with that prefix.
    let request_mappings: Vec<regex::Captures> =
        spring_request_mapping_regex().captures_iter(src).collect();
    let mut prefix = String::new();
    let mut prefix_taken = false;
    for cap in &request_mappings {
        let body = cap.name("body").map(|m| m.as_str()).unwrap_or("");
        let rm_path = spring_rm_path_regex()
            .captures(body)
            .and_then(|c| c.name("path").map(|m| m.as_str().to_string()));
        let rm_method = spring_rm_method_regex()
            .captures(body)
            .and_then(|c| c.name("m").map(|m| m.as_str().to_string()));
        if !prefix_taken {
            // The first path-bearing @RequestMapping with NO explicit method is
            // the class prefix; consume it and move on.
            if let Some(p) = rm_path.clone() {
                if rm_method.is_none() {
                    prefix = p;
                    prefix_taken = true;
                    continue;
                }
            }
        }
        // A method-bearing (or later) @RequestMapping is an endpoint.
        if let Some(p) = rm_path {
            let full = combine_paths(&prefix, &p);
            let method = rm_method.as_deref().and_then(HttpVerb::parse);
            push_route(routes, file, method, &full);
        }
    }
    for cap in spring_method_regex().captures_iter(src) {
        let verb = cap.name("m").map(|m| m.as_str()).unwrap_or("");
        let sub = cap.name("path").map(|m| m.as_str()).unwrap_or("");
        let full = combine_paths(&prefix, sub);
        if let Some(m) = parse_method(verb) {
            push_route(routes, file, m.into_method(), &full);
        }
    }
}

// ---------------------------------------------------------------------------
// Path helpers + matching
// ---------------------------------------------------------------------------

/// Join a class/controller prefix and a method sub-path into one path with a
/// single leading slash and no doubled separators. Either part may be empty.
fn combine_paths(prefix: &str, sub: &str) -> String {
    let mut segs: Vec<&str> = Vec::new();
    for part in [prefix, sub] {
        for seg in part.split('/') {
            if !seg.is_empty() {
                segs.push(seg);
            }
        }
    }
    if segs.is_empty() {
        return "/".to_string();
    }
    format!("/{}", segs.join("/"))
}

/// One normalised path segment: a concrete literal or a parameter placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    /// A concrete literal segment (lower-cased for case-insensitive compare).
    Static(String),
    /// A path parameter (`:id` / `{id}` / `<int:id>` / `*rest`).
    Param,
}

/// Split a path into normalised segments, dropping empty parts (so a missing /
/// doubled leading slash does not matter) and classifying each as literal or
/// parameter.
fn normalize_segments(path: &str) -> Vec<Seg> {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if is_param_segment(s) {
                Seg::Param
            } else {
                Seg::Static(s.to_ascii_lowercase())
            }
        })
        .collect()
}

/// Whether a raw path segment is a route parameter across the frameworks we
/// parse: Express/gin `:id`, OpenAPI/FastAPI/Spring `{id}`, Django `<int:id>`,
/// axum/gin wildcard `*rest`. Reuses [`is_template_param`] for the `:id` form.
fn is_param_segment(seg: &str) -> bool {
    is_template_param(seg)
        || seg.starts_with('*')
        || (seg.starts_with('{') && seg.ends_with('}'))
        || (seg.starts_with('<') && seg.ends_with('>'))
}

/// Whether a planned path has at least one **checkable** (concrete, non-generic)
/// static segment — a segment that is not a parameter, not `api`, and not a
/// version prefix (`v1` / `v2`). A path with none (e.g. `/`, `/api`, `/:id`) is
/// too generic to verify against registrations and is skipped by the acceptance
/// check (mirroring the legacy `needle.len() < 4` skip).
#[must_use]
pub fn path_has_checkable_segment(path: &str) -> bool {
    normalize_segments(path).iter().any(|s| match s {
        Seg::Static(v) => !is_generic_segment(v),
        Seg::Param => false,
    })
}

/// Whether a lower-cased static segment is too generic to anchor a match:
/// `api` or a version prefix (`v1`, `v2`, `v10`).
fn is_generic_segment(seg: &str) -> bool {
    seg == "api"
        || (seg.len() >= 2 && seg.starts_with('v') && seg[1..].chars().all(|c| c.is_ascii_digit()))
}

/// Whether a registered method (`None` = wildcard) covers a planned verb.
fn method_covers(registered: Option<HttpVerb>, planned: HttpVerb) -> bool {
    registered.map_or(true, |m| m == planned)
}

/// Do a registration path and a planned path align?
///
/// The two are aligned from the **right** over their shared tail: a parameter
/// on either side matches anything, two literals must be equal, and at least
/// one concrete literal must actually coincide. Right-alignment tolerates the
/// prefix differences that regex can't reconstruct — a sub-router mounted under
/// `app.use('/api', r)`, a FastAPI `APIRouter(prefix=...)`, a NestJS/Spring
/// class prefix, a global `/api` prefix — WITHOUT the false pass that a bare
/// prefix match (`/api/users` "implements" `/api/users/:id`) would give: a
/// conflicting literal in the aligned tail rejects the match.
fn paths_align(reg_path: &str, planned_path: &str) -> bool {
    let a = normalize_segments(reg_path);
    let b = normalize_segments(planned_path);
    let n = a.len().min(b.len());
    if n == 0 {
        return false;
    }
    let a_tail = &a[a.len() - n..];
    let b_tail = &b[b.len() - n..];
    let mut static_hit = false;
    for (x, y) in a_tail.iter().zip(b_tail.iter()) {
        // A parameter on either side is compatible with anything; two literals
        // must be equal (a conflicting literal means a different route).
        if let (Seg::Static(sx), Seg::Static(sy)) = (x, y) {
            if sx != sy {
                return false;
            }
            static_hit = true;
        }
    }
    static_hit
}

/// Whether a planned endpoint `(method, path)` has a real backend route
/// registration among `routes`.
///
/// This is the deterministic replacement for the old "does the path's static
/// prefix appear anywhere in the source" substring check: a frontend `fetch`
/// call or a comment is not a [`BackendRoute`], so it can no longer satisfy the
/// endpoint. A registration matches when its method covers the planned verb
/// (or is a wildcard) and its path aligns (see [`paths_align`]).
#[must_use]
pub fn route_registered(routes: &[BackendRoute], method: HttpVerb, path: &str) -> bool {
    routes
        .iter()
        .any(|r| method_covers(r.method, method) && paths_align(&r.path, path))
}

// ---------------------------------------------------------------------------
// Comment stripping + tree walk
// ---------------------------------------------------------------------------

/// Strip line and block comments from source so a commented-out registration
/// never counts. Quote-aware (single / double / backtick) so a `//` or `#`
/// inside a string literal (e.g. a `"http://"` URL or a path) is preserved.
/// Language-aware only for the `#` line comment (Python). Fail-open: any
/// unusual input just yields best-effort output, never a panic.
fn strip_comments(src: &str, lang: Lang) -> String {
    let hash_line = matches!(lang, Lang::Py);
    // Rust `'` is a lifetime tick (`&'a`, `'_`, `'static`) far more often than
    // a char-literal delimiter, and a lifetime is a LONE `'` with no closer.
    // Treating it as a string open leaves `quote` stuck (often to EOF), and
    // while a quote is open no `//` comment is stripped — so a commented-out
    // `.route("/api/old", get(old))` after a `&'static str` survives and is
    // falsely extracted as an implemented route (a false acceptance PASS). See
    // `rust_char_literal_len`.
    let rust = matches!(lang, Lang::Rust);
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let len = chars.len();
    let mut quote: Option<char> = None;
    while i < len {
        let c = chars[i];
        if let Some(q) = quote {
            out.push(c);
            if c == '\\' && i + 1 < len {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        let next = if i + 1 < len { chars[i + 1] } else { '\0' };
        // `//` line comment (all supported languages).
        if c == '/' && next == '/' {
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // `#` line comment (Python).
        if hash_line && c == '#' {
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // `/* ... */` block comment.
        if c == '/' && next == '*' {
            i += 2;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(len);
            out.push(' ');
            continue;
        }
        // `<!-- ... -->` block comment (HTML-in-JSX, rare but cheap to cover).
        if c == '<' && next == '!' && i + 3 < len && chars[i + 2] == '-' && chars[i + 3] == '-' {
            i += 4;
            while i + 2 < len && !(chars[i] == '-' && chars[i + 1] == '-' && chars[i + 2] == '>') {
                i += 1;
            }
            i = (i + 3).min(len);
            out.push(' ');
            continue;
        }
        // Rust: do NOT treat `'` as a string delimiter (`"` / backtick still
        // cover real Rust strings). Consume a *balanced* char literal
        // (`'x'`, `'\n'`, `'\u{..}'`) verbatim so an inner `"` / `/` can't open
        // a string or comment; a lone tick (a lifetime) is emitted as an
        // ordinary char and never opens a quote.
        if rust && c == '\'' {
            if let Some(consumed) = rust_char_literal_len(&chars, i) {
                out.extend(chars[i..i + consumed].iter().copied());
                i += consumed;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' || c == '`' {
            quote = Some(c);
        }
        out.push(c);
        i += 1;
    }
    out
}

/// If `chars[i]` (which must be `'`) begins a Rust **char literal**
/// (`'x'`, `'\n'`, `'\''`, `'\\'`, `'\u{1F600}'`), return its length in
/// `char`s including both quotes; otherwise `None`.
///
/// A `None` result means the `'` is a **lifetime** tick (`'a`, `'_`,
/// `'static`) — a lone `'` that must NOT be treated as a string delimiter (it
/// has no closer, so opening a quote there gets stuck open and stops comment
/// stripping). Bounded: never scans past a short fixed window, never panics on
/// truncated input (a dangling `'` at EOF simply returns `None`).
fn rust_char_literal_len(chars: &[char], i: usize) -> Option<usize> {
    // chars[i] == '\''.
    let c1 = *chars.get(i + 1)?;
    if c1 == '\\' {
        // Escaped char literal.
        let c2 = *chars.get(i + 2)?;
        if c2 == 'u' {
            // Unicode escape `'\u{XXXX}'`: require the brace group then a close.
            if chars.get(i + 3) != Some(&'{') {
                return None;
            }
            // Up to 6 hex digits fit in a `char`; scan a bounded window for `}`.
            let cap = (i + 4 + 8).min(chars.len());
            let mut j = i + 4;
            while j < cap {
                if chars[j] == '}' {
                    return (chars.get(j + 1) == Some(&'\'')).then_some(j + 2 - i);
                }
                j += 1;
            }
            None
        } else {
            // Simple escape `'\n'` → `'` `\` c2 `'`.
            (chars.get(i + 3) == Some(&'\'')).then_some(4)
        }
    } else if c1 == '\'' {
        // `''` is not a valid char literal (and not a lifetime) — leave the
        // first tick as an ordinary char.
        None
    } else {
        // Plain char literal `'x'` → `'` c1 `'`. A lifetime `'a` has a
        // non-quote char after the name (`,`, `>`, ` `, `:`), so it fails here
        // and the tick is emitted as an ordinary char.
        (chars.get(i + 2) == Some(&'\'')).then_some(3)
    }
}

/// Strip line + block comments from JS/TS source (`//`, `/* */`, string- and
/// template-literal aware). A thin wrapper over [`strip_comments`] so the
/// frontend call extractor ([`crate::extract`]) can drop commented-out
/// `fetch`/`axios` call-sites before running its regexes — without which a
/// `// fetch('/api/deprecated')` becomes a phantom `UndeclaredCall`, the
/// frontend mirror of the backend false-PASS this module fixes.
pub(crate) fn strip_comments_jsts(src: &str) -> String {
    strip_comments(src, Lang::JsTs)
}

/// Recursively collect backend source files (bounded by depth + count; skips
/// vendored / generated dirs; never follows symlinks, matching the frontend
/// walker's no-follow contract).
fn collect_backend_sources(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let p = entry.path();
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
            collect_backend_sources(&p, out, depth + 1);
        } else if meta.is_file() {
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
            if BACKEND_EXTS.contains(&ext) {
                out.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(routes: &[BackendRoute]) -> Vec<(Option<HttpVerb>, &str)> {
        routes.iter().map(|r| (r.method, r.path.as_str())).collect()
    }

    // ---- Express / Koa / Fastify ----

    #[test]
    fn express_method_calls_extracted() {
        let routes = extract_from_file(
            "server.js",
            "app.get('/api/todos', list); router.post('/api/todos', create); \
             app.delete('/api/todos/:id', del);",
            Lang::JsTs,
        );
        assert!(routes.contains(&BackendRoute {
            file: "server.js".into(),
            method: Some(HttpVerb::Get),
            path: "/api/todos".into(),
        }));
        assert!(routes.contains(&BackendRoute {
            file: "server.js".into(),
            method: Some(HttpVerb::Post),
            path: "/api/todos".into(),
        }));
        assert!(routes.contains(&BackendRoute {
            file: "server.js".into(),
            method: Some(HttpVerb::Delete),
            path: "/api/todos/:id".into(),
        }));
    }

    #[test]
    fn frontend_fetch_and_axios_are_not_registrations() {
        // The whole point: a frontend call-site must NOT be a backend route.
        let routes = extract_from_file(
            "web.ts",
            "fetch('/api/login'); axios.get('/api/users'); api.post('/api/orders');",
            Lang::JsTs,
        );
        assert!(
            routes.is_empty(),
            "fetch/axios/api are not registrations: {routes:?}"
        );
    }

    #[test]
    fn app_all_and_use_are_wildcard() {
        let routes = extract_from_file(
            "server.js",
            "app.all('/api/health', h); app.use('/api/mounted', r);",
            Lang::JsTs,
        );
        // app.all → wildcard route; app.use isn't a method call form here.
        assert!(routes.contains(&BackendRoute {
            file: "server.js".into(),
            method: None,
            path: "/api/health".into(),
        }));
    }

    #[test]
    fn fastify_object_route_extracted() {
        let routes = extract_from_file(
            "server.js",
            "fastify.route({ method: 'PUT', url: '/api/items/:id' })",
            Lang::JsTs,
        );
        assert!(routes.contains(&BackendRoute {
            file: "server.js".into(),
            method: Some(HttpVerb::Put),
            path: "/api/items/:id".into(),
        }));
    }

    #[test]
    fn fastify_object_route_method_array() {
        let routes = extract_from_file(
            "s.js",
            "server.route({ method: ['GET','POST'], url: '/api/x' })",
            Lang::JsTs,
        );
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/x")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Post), "/api/x")));
    }

    #[test]
    fn nest_decorators_combined_with_controller_prefix() {
        let src = "@Controller('users')\nclass C {\n  @Get()\n list(){}\n  @Get(':id')\n one(){}\n  @Post()\n create(){}\n}";
        let routes = extract_from_file("users.controller.ts", src, Lang::JsTs);
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/users")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/users/:id")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Post), "/users")));
    }

    // ---- Python ----

    #[test]
    fn flask_route_default_get_and_methods() {
        let routes = extract_from_file(
            "app.py",
            "@app.route('/api/login', methods=['POST'])\ndef login(): pass\n@app.route('/api/health')\ndef health(): pass",
            Lang::Py,
        );
        assert!(paths(&routes).contains(&(Some(HttpVerb::Post), "/api/login")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/health")));
    }

    #[test]
    fn fastapi_decorators_extracted() {
        let routes = extract_from_file(
            "main.py",
            "@app.get('/api/users')\ndef list(): ...\n@router.post('/api/users')\ndef create(): ...",
            Lang::Py,
        );
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/users")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Post), "/api/users")));
    }

    #[test]
    fn django_urls_are_wildcard_routes() {
        let routes = extract_from_file(
            "urls.py",
            "urlpatterns = [ path('api/users/', views.users), re_path(r'^api/orders/$', views.orders) ]",
            Lang::Py,
        );
        assert!(paths(&routes).contains(&(None, "api/users/")));
        assert!(routes
            .iter()
            .any(|r| r.method.is_none() && r.path.contains("orders")));
    }

    // ---- Rust ----

    #[test]
    fn axum_route_multiple_methods() {
        let routes = extract_from_file(
            "main.rs",
            "let app = Router::new().route(\"/api/users\", get(list).post(create));",
            Lang::Rust,
        );
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/users")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Post), "/api/users")));
    }

    #[test]
    fn actix_attribute_macro() {
        let routes = extract_from_file(
            "handlers.rs",
            "#[get(\"/api/ping\")]\nasync fn ping() {}",
            Lang::Rust,
        );
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/ping")));
    }

    #[test]
    fn rust_lifetime_does_not_swallow_commented_route() {
        // Finding B: a Rust lifetime `&'static str` is a LONE `'`. If it were
        // treated as a string open, the quote would get stuck to EOF and stop
        // comment stripping, so the commented-out `.route(...)` below would be
        // extracted → a commented endpoint reported as IMPLEMENTED (false PASS).
        let src = "const NAME: &'static str = \"x\";\n\
                   // let app = Router::new().route(\"/api/old\", get(old));\n\
                   let app = Router::new().route(\"/api/real\", get(real));";
        let routes = extract_from_file("main.rs", src, Lang::Rust);
        let p = paths(&routes);
        assert!(
            p.contains(&(Some(HttpVerb::Get), "/api/real")),
            "the real route must still register: {p:?}"
        );
        assert!(
            !p.iter().any(|(_, path)| *path == "/api/old"),
            "the commented-out route must NOT be extracted: {p:?}"
        );
    }

    #[test]
    fn rust_generic_lifetime_and_anon_lifetime_do_not_stick() {
        // `<'a>` and `'_` are lifetimes (lone ticks); a `//` comment AFTER them
        // must still be stripped.
        let src = "struct S<'a> { r: &'a str, m: PhantomData<'_> }\n\
                   // app has .route(\"/api/ghost\", get(g))\n\
                   fn build<'a>() { Router::new().route(\"/api/keep\", post(k)); }";
        let routes = extract_from_file("s.rs", src, Lang::Rust);
        let p = paths(&routes);
        assert!(
            p.contains(&(Some(HttpVerb::Post), "/api/keep")),
            "route after lifetimes must register: {p:?}"
        );
        assert!(
            !p.iter().any(|(_, path)| *path == "/api/ghost"),
            "commented route must not survive: {p:?}"
        );
    }

    #[test]
    fn rust_char_literal_with_quote_does_not_open_string() {
        // A char literal `'"'` contains a `"`. It must be consumed as a literal
        // so the inner `"` does NOT open a string that swallows a following
        // `//` comment (which would leak the commented route below).
        let src = "let q = '\"';\n\
                   // Router::new().route(\"/api/hidden\", get(h));\n\
                   Router::new().route(\"/api/shown\", get(s));";
        let routes = extract_from_file("q.rs", src, Lang::Rust);
        let p = paths(&routes);
        assert!(
            p.contains(&(Some(HttpVerb::Get), "/api/shown")),
            "route after char literal must register: {p:?}"
        );
        assert!(
            !p.iter().any(|(_, path)| *path == "/api/hidden"),
            "commented route after a '\"' char literal must not survive: {p:?}"
        );
    }

    #[test]
    fn rust_char_literal_len_recognises_forms() {
        // Positive: real char literals report their full length.
        let cases = [("'x'", 3usize), ("'\\n'", 4), ("'\\''", 4), ("'\\\\'", 4)];
        for (lit, want) in cases {
            let chars: Vec<char> = lit.chars().collect();
            assert_eq!(
                rust_char_literal_len(&chars, 0),
                Some(want),
                "char literal {lit:?} should consume {want}"
            );
        }
        // Unicode escape.
        let u: Vec<char> = "'\\u{1F600}'".chars().collect();
        assert_eq!(rust_char_literal_len(&u, 0), Some(u.len()));
        // Negative: lifetimes are lone ticks, not char literals.
        for life in ["'a", "'_", "'static", "'a,", "'a>"] {
            let chars: Vec<char> = life.chars().collect();
            assert_eq!(
                rust_char_literal_len(&chars, 0),
                None,
                "lifetime {life:?} must not read as a char literal"
            );
        }
    }

    #[test]
    fn rust_string_double_slash_still_preserved() {
        // Regression guard: `"` is STILL a Rust string delimiter, so a `//`
        // inside a string literal must not be stripped as a comment.
        let stripped = strip_comments("let u = \"http://x/y\"; foo();", Lang::Rust);
        assert!(
            stripped.contains("http://x/y"),
            "rust string // preserved: {stripped}"
        );
    }

    // ---- Go ----

    #[test]
    fn gin_and_chi_methods() {
        let routes = extract_from_file(
            "main.go",
            "r.GET(\"/api/users\", h); router.Post(\"/api/orders\", h)",
            Lang::Go,
        );
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/users")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Post), "/api/orders")));
    }

    #[test]
    fn go_mux_with_and_without_method() {
        let routes = extract_from_file(
            "server.go",
            "mux.HandleFunc(\"GET /api/items\", h); mux.HandleFunc(\"/api/legacy\", h)",
            Lang::Go,
        );
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/items")));
        assert!(paths(&routes).contains(&(None, "/api/legacy")));
    }

    // ---- Java Spring ----

    #[test]
    fn spring_mappings_combined_with_class_prefix() {
        let src = "@RestController\n@RequestMapping(\"/api/users\")\nclass C {\n  @GetMapping\n List list(){}\n  @GetMapping(\"/{id}\")\n One one(){}\n  @PostMapping\n Void create(){}\n}";
        let routes = extract_from_file("C.java", src, Lang::Java);
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/users")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Get), "/api/users/{id}")));
        assert!(paths(&routes).contains(&(Some(HttpVerb::Post), "/api/users")));
    }

    // ---- comment stripping ----

    #[test]
    fn commented_out_registration_is_ignored() {
        let routes = extract_from_file(
            "server.js",
            "// app.post('/api/login', h)\n/* app.get('/api/secret', h) */\napp.get('/api/real', h)",
            Lang::JsTs,
        );
        assert_eq!(paths(&routes), vec![(Some(HttpVerb::Get), "/api/real")]);
    }

    #[test]
    fn hash_comment_ignored_in_python() {
        let routes = extract_from_file(
            "app.py",
            "# @app.get('/api/commented')\n@app.get('/api/real')\ndef r(): ...",
            Lang::Py,
        );
        assert_eq!(paths(&routes), vec![(Some(HttpVerb::Get), "/api/real")]);
    }

    #[test]
    fn url_with_double_slash_in_string_survives_comment_strip() {
        // A `//` inside a string literal must NOT be treated as a comment.
        let stripped = strip_comments(
            "const u = \"http://x/y\"; app.get('/api/ok', h)",
            Lang::JsTs,
        );
        assert!(
            stripped.contains("http://x/y"),
            "string // preserved: {stripped}"
        );
        assert!(stripped.contains("/api/ok"));
    }

    // ---- matching ----

    #[test]
    fn route_registered_full_path() {
        let routes = vec![BackendRoute {
            file: "s.js".into(),
            method: Some(HttpVerb::Get),
            path: "/api/login".into(),
        }];
        assert!(route_registered(&routes, HttpVerb::Get, "/api/login"));
        assert!(!route_registered(&routes, HttpVerb::Post, "/api/login"));
        assert!(!route_registered(&routes, HttpVerb::Get, "/api/logout"));
    }

    #[test]
    fn route_registered_param_normalization() {
        // /users/:id (planned) matches /users/{id} (registered) and vice versa.
        let reg = vec![BackendRoute {
            file: "s.py".into(),
            method: Some(HttpVerb::Get),
            path: "/users/{id}".into(),
        }];
        assert!(route_registered(&reg, HttpVerb::Get, "/users/:id"));
        let reg2 = vec![BackendRoute {
            file: "s.rs".into(),
            method: Some(HttpVerb::Get),
            path: "/users/:id".into(),
        }];
        assert!(route_registered(&reg2, HttpVerb::Get, "/users/{id}"));
    }

    #[test]
    fn route_registered_mount_prefix_tolerance() {
        // A sub-router `router.get('/users')` mounted under `/api` still matches
        // a planned `/api/users` (right-aligned tail).
        let reg = vec![BackendRoute {
            file: "s.js".into(),
            method: Some(HttpVerb::Get),
            path: "/users".into(),
        }];
        assert!(route_registered(&reg, HttpVerb::Get, "/api/users"));
    }

    #[test]
    fn route_registered_rejects_conflicting_literal() {
        // A list route must NOT be counted as implementing the item route.
        let reg = vec![BackendRoute {
            file: "s.js".into(),
            method: Some(HttpVerb::Get),
            path: "/api/users".into(),
        }];
        assert!(!route_registered(&reg, HttpVerb::Get, "/api/users/:id"));
        // Different resource entirely.
        assert!(!route_registered(&reg, HttpVerb::Get, "/api/orders"));
    }

    #[test]
    fn wildcard_registration_covers_any_method() {
        let reg = vec![BackendRoute {
            file: "urls.py".into(),
            method: None,
            path: "/api/users".into(),
        }];
        assert!(route_registered(&reg, HttpVerb::Get, "/api/users"));
        assert!(route_registered(&reg, HttpVerb::Post, "/api/users"));
        assert!(route_registered(&reg, HttpVerb::Delete, "/api/users"));
    }

    #[test]
    fn path_has_checkable_segment_skips_generic() {
        assert!(!path_has_checkable_segment("/"));
        assert!(!path_has_checkable_segment("/api"));
        assert!(!path_has_checkable_segment("/api/v1"));
        assert!(!path_has_checkable_segment("/api/:id"));
        assert!(path_has_checkable_segment("/api/users"));
        assert!(path_has_checkable_segment("/api/v1/users/:id"));
    }

    // ---- tree walk + fail-open ----

    #[test]
    fn extract_from_tree_and_skips_vendored() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/server.js"), "app.get('/api/real', h)").unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/lib")).unwrap();
        std::fs::write(
            tmp.path().join("node_modules/lib/x.js"),
            "app.get('/api/evil', h)",
        )
        .unwrap();
        let routes = extract_backend_routes(tmp.path());
        assert!(routes.iter().any(|r| r.path == "/api/real"));
        assert!(!routes.iter().any(|r| r.path == "/api/evil"));
    }

    #[test]
    fn extracts_routes_from_deep_backend_module_layout() {
        // Real Java/Spring and generated admin modules can nest controllers well
        // past 8 directories under a workspace. The route scanner must still see
        // them so backend presence and API coverage are judged from reality.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp
            .path()
            .join("services/medical/modules/mes/src/main/java/com/acme/mes/app/api/controller");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("PatientController.java"),
            r#"
                @RestController
                @RequestMapping("/api/patients")
                class PatientController {
                    @GetMapping("/{id}")
                    String get() { return "ok"; }
                }
            "#,
        )
        .unwrap();

        let routes = extract_backend_routes(tmp.path());
        assert!(
            routes.iter().any(|r| r.path == "/api/patients/{id}"),
            "deep backend route must be scanned: {routes:?}"
        );
    }

    #[test]
    fn multiple_frameworks_in_one_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("server.js"), "app.get('/api/js', h)").unwrap();
        std::fs::write(d.join("main.py"), "@app.get('/api/py')\ndef f(): ...").unwrap();
        std::fs::write(d.join("main.go"), "r.GET(\"/api/go\", h)").unwrap();
        std::fs::write(
            d.join("lib.rs"),
            "Router::new().route(\"/api/rs\", get(h));",
        )
        .unwrap();
        let routes = extract_backend_routes(d);
        for p in ["/api/js", "/api/py", "/api/go", "/api/rs"] {
            assert!(
                routes.iter().any(|r| r.path == p),
                "missing {p}: {routes:?}"
            );
        }
    }

    #[test]
    fn fail_open_on_unparseable_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A non-UTF-8 file must be skipped, not panic.
        std::fs::write(tmp.path().join("bin.js"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        std::fs::write(tmp.path().join("ok.js"), "app.get('/api/ok', h)").unwrap();
        let routes = extract_backend_routes(tmp.path());
        assert!(routes.iter().any(|r| r.path == "/api/ok"));
    }

    #[test]
    fn empty_tree_yields_no_routes() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(extract_backend_routes(tmp.path()).is_empty());
    }
}
