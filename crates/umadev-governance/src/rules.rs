//! Pre-write enforcement rules — refuse a tool call before it lands on disk.
//!
//! Each rule is pure: `(file_path, content)` in, [`Decision`] out. Hosts wire
//! these rules into their pre-edit hooks.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

mod deep_nesting;
pub use deep_nesting::check_deep_nesting;
mod debug_residue;
pub use debug_residue::check_debug_residue;
mod concurrency;
pub use concurrency::check_unsynchronized_mutation;
mod file_safety;
pub use file_safety::{check_hard_delete, check_insecure_file_perms, check_toctou_race};
mod security_rules;
pub use security_rules::{check_command_injection, check_template_injection, check_weak_crypto};
mod var_declarations;
pub use var_declarations::check_var_declarations;

/// Outcome of a governance rule.
///
/// Blocking decisions are serialized for the host; `reason` guides retries.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// `true` when the host MUST refuse the tool call.
    pub block: bool,
    /// Human-readable explanation shown to the model; empty when pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// Rule identifier that fired. Normative checks use their `UD-*` clause;
    /// independent content lints use `UG-LINT-*`. Empty on pass. The field name
    /// is retained for wire compatibility with existing hosts.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub clause: String,
}

impl Decision {
    /// Build a passing decision.
    #[must_use]
    pub const fn pass() -> Self {
        Self {
            block: false,
            reason: String::new(),
            clause: String::new(),
        }
    }

    /// Build a blocking decision with `reason` and the firing rule identifier.
    #[must_use]
    pub fn block(rule_id: &str, reason: impl Into<String>) -> Self {
        Self {
            block: true,
            reason: reason.into(),
            clause: rule_id.to_string(),
        }
    }
}

/// Legacy `UD-CODE-*` aliases accepted by policy lookup for `UG-LINT-*` rules.
pub(crate) const LEGACY_LINT_ID_ALIASES: &[(&str, &str)] = &[
    ("UD-CODE-003", "UG-LINT-001"), // inline styles
    ("UD-CODE-004", "UG-LINT-002"), // magic numbers
    ("UD-CODE-006", "UG-LINT-003"), // unused variables
    ("UD-CODE-007", "UG-LINT-004"), // deep nesting
    ("UD-CODE-008", "UG-LINT-005"), // inline event handlers
    ("UD-CODE-009", "UG-LINT-006"), // state mutation
    ("UD-CODE-010", "UG-LINT-007"), // wildcard imports
    ("UD-CODE-011", "UG-LINT-008"), // var declarations
    ("UD-CODE-012", "UG-LINT-009"), // loose equality
    ("UD-CODE-013", "UG-LINT-010"), // untyped props
    ("UD-CODE-014", "UG-LINT-011"), // render side effects
    ("UD-CODE-015", "UG-LINT-012"), // mutable default export
    ("UD-CODE-016", "UG-LINT-013"), // unsafe date parse
    ("UD-CODE-017", "UG-LINT-014"), // for-in over array
    ("UD-CODE-018", "UG-LINT-015"), // TODO/FIXME residue
];

/// The project's governed attack surface — the context that decides whether a
/// **context-relevant** rule even has something to guard.
///
/// The universal "always wrong" floor (emoji-as-icon, hardcoded colors when a
/// design system exists, swallowed errors, real hardcoded secrets, a frontend
/// reaching straight into a database, AI-slop) is independent of this context
/// and fires on EVERY project. A second, smaller class of rules only protects a
/// **server / security surface** — CSP (UD-ARCH-013), clickjacking
/// (UD-ARCH-046), structured logging (UD-ARCH-012), insecure RNG in a token
/// context (UD-ARCH-043), security headers / HSTS / HTTPS-redirect (UD-ARCH-019
/// / UD-ARCH-022 / UD-ARCH-016), CSRF (UD-ARCH-047). A purely static frontend
/// (no backend, no auth, no token/session/data plane, nothing served with
/// response headers) has **no such surface**, so those rules have nothing to
/// defend and must not fire.
///
/// This struct carries that signal so the rule engine can skip surface-bound
/// rules for a provably static frontend. It is derived from information the run
/// already has (the planner's `TaskKind`, the architecture doc, and the real
/// produced artifacts) — it adds **no** new model call. It is **fail-open and
/// conservative**: the default ([`ProjectContext::unknown`]) assumes a server
/// surface might exist, so a project whose context we can't establish is
/// governed at full strictness and a real backend/auth project is never
/// under-governed by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectContext {
    /// `true` only when the run is PROVABLY a static, frontend-only project with
    /// no server surface: no backend code, no auth, no token/session/data plane.
    /// When `true`, server/security-surface rules are skipped UNLESS the file
    /// being scanned itself shows server evidence. Defaults to `false`
    /// (conservative: assume a surface might exist).
    ///
    /// `#[serde(default)]`: when the persisted `.umadev/governance-context.json`
    /// is missing this field, deserialize it to `false` — the conservative
    /// strict posture, never an accidental skip.
    #[serde(default)]
    pub static_frontend_only: bool,

    /// The user's own words **authorized** a purple / violet / indigo brand.
    ///
    /// The banned-hue band is a DEFAULT-REJECT, never a censor, and this is the ONE
    /// permission that stands it down. It has to travel with the write governor, not
    /// just the design floor: [`check_ai_slop`] runs inside the PreToolUse hook and the
    /// in-process write scan, so without it a user who ASKED for a violet brand has the
    /// write of their own palette REJECTED, with no way to stand the rule down and no
    /// convergent fix (the design floor accepts the tokens the write governor refuses).
    ///
    /// **Who decides this, and where.** Not this crate, and not a word list. "Did the user
    /// authorize this color family?" is an INTENT question — the same class as "is this turn
    /// chat, an edit, or a build" — so the run asks the borrowed brain ONE structured
    /// question and records the verdict here (`umadev_agent::color_permission`). It is
    /// computed exactly ONCE, at the run door where the requirement first becomes known, and
    /// persisted; every later reader (the PreToolUse hook, the design floor, `umadev ci`) is
    /// a separate process with no brain and MUST read this stored decision rather than
    /// re-derive one. A lexical reader was tried for six review rounds and leaked on every
    /// one — a prohibition has unboundedly many phrasings, and a word list that grows to
    /// chase them is answering the wrong question.
    ///
    /// **Fail direction: STRICT.** Brain unreachable, offline runtime, malformed answer,
    /// timeout, an unstamped context, a context that predates the field — every one of them
    /// leaves this `false` and the rule ARMED. That is not a fail-open violation: it never
    /// blocks or crashes the host, it only declines to stand a rule DOWN. A leak writes
    /// AI-slop into the customer's repo irreversibly; a false block is one recoverable rework.
    #[serde(default)]
    pub purple_allowed: bool,

    /// **Provenance**: [`requirement_fingerprint`] of the requirement this context was
    /// derived from. `0` = unknown provenance (a legacy or hand-written file).
    ///
    /// Without this the context is two naked bools with nothing to date or attribute them
    /// to, and a permission is not a fact — it is a fact *about a specific requirement*. A
    /// `purple_allowed: true` left behind by last month's violet rebrand would otherwise
    /// stand the banned-hue band down FOREVER, including for the next requirement, whose
    /// first line is "no purple". See [`ProjectContext::if_current`].
    #[serde(default)]
    pub requirement_hash: u64,

    /// **Provenance**: UNIX seconds at which this context was derived. `0` = unknown (a
    /// legacy or hand-written file). See [`ProjectContext::if_current`].
    #[serde(default)]
    pub derived_at: u64,
}

/// A stable, dependency-light fingerprint of the requirement a [`ProjectContext`] was
/// derived from (FNV-1a over the trimmed bytes).
///
/// Not a security hash — it answers exactly one question: *is the context on disk the one
/// this workspace's current requirement produced?* Trimmed, so trailing whitespace from a
/// paste does not read as a different requirement. Never returns `0`, which is reserved
/// for "no provenance recorded".
#[must_use]
pub fn requirement_fingerprint(requirement: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in requirement.trim().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // 0 means "unstamped"; a real fingerprint must never collide with it.
    if h == 0 {
        1
    } else {
        h
    }
}

impl Default for ProjectContext {
    /// The conservative default: assume a server surface MIGHT exist, so every
    /// context-relevant rule stays on. This is the fail-open posture — when the
    /// run can't establish the context, we govern at full strictness.
    fn default() -> Self {
        Self::unknown()
    }
}

impl ProjectContext {
    /// How long a context whose requirement we CANNOT check against still counts as
    /// current (7 days). It only bounds the un-attributable case: a context that still
    /// matches the workspace's live requirement is current whatever its age (a violet
    /// brand does not expire), and one that matches nothing is not evidence for long.
    pub const MAX_UNMATCHED_AGE_SECS: u64 = 7 * 24 * 60 * 60;

    /// The conservative default — context unknown, so assume a server/security
    /// surface might exist and keep every context-relevant rule on.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            static_frontend_only: false,
            purple_allowed: false,
            requirement_hash: 0,
            derived_at: 0,
        }
    }

    /// A project the run has PROVEN to be a static, frontend-only build: no
    /// backend, no auth, no data/session plane. Surface-bound rules are skipped
    /// for it (unless a specific file shows server evidence on its own).
    #[must_use]
    pub const fn static_frontend() -> Self {
        Self {
            static_frontend_only: true,
            purple_allowed: false,
            requirement_hash: 0,
            derived_at: 0,
        }
    }

    /// The same context, with the user's explicit permission for a purple/violet brand
    /// recorded (see [`ProjectContext::purple_allowed`]).
    #[must_use]
    pub const fn with_purple_allowed(mut self, allowed: bool) -> Self {
        self.purple_allowed = allowed;
        self
    }

    /// Stamp the context with the PROVENANCE of the requirement it was derived from —
    /// which requirement, and when. Every producer of a persisted context calls this; a
    /// context with no stamp cannot be trusted to stand a rule down (see
    /// [`Self::if_current`]).
    #[must_use]
    pub fn derived_from(mut self, requirement: &str, now: u64) -> Self {
        self.requirement_hash = requirement_fingerprint(requirement);
        self.derived_at = now;
        self
    }

    /// The context AS READ FROM DISK, downgraded to [`Self::unknown`] (full strictness)
    /// unless it is provably CURRENT.
    ///
    /// A persisted context is a **permission** — it stands rules down. The gates that read
    /// it back (`umadev ci` in the pre-commit hook, the PreToolUse hook) run in a separate
    /// process, long after the run that wrote it, with no idea what it was derived from.
    /// So a permission with no provenance is not honoured:
    ///
    /// - **No stamp** (empty hash / zero timestamp — a legacy or hand-written file) →
    ///   strict. We cannot attribute it to anything.
    /// - **A requirement to check against** (the caller read the workspace's live
    ///   requirement): the hashes must match. A `purple_allowed: true` derived from last
    ///   month's "make our brand violet" must NOT stand the band down for today's "no
    ///   purple anywhere" — and a context that DOES match today's requirement is current
    ///   regardless of age, so the violet-branded project never gets falsely blocked.
    /// - **Nothing to check against** (no workspace requirement on record) → the age
    ///   fallback ([`Self::MAX_UNMATCHED_AGE_SECS`]).
    ///
    /// The strict direction is the safe one here: a false block on a color is loud and one
    /// re-run from fixed, while a silently-permitted AI palette ships.
    #[must_use]
    pub fn if_current(self, now: u64, requirement: Option<&str>) -> Self {
        if self.requirement_hash == 0 || self.derived_at == 0 {
            return Self::unknown();
        }
        match requirement.map(str::trim).filter(|r| !r.is_empty()) {
            Some(req) => {
                if requirement_fingerprint(req) == self.requirement_hash {
                    self
                } else {
                    Self::unknown()
                }
            }
            // `saturating_sub` also means a future timestamp (clock skew) reads as fresh —
            // never as "so stale it must be ignored".
            None if now.saturating_sub(self.derived_at) <= Self::MAX_UNMATCHED_AGE_SECS => self,
            None => Self::unknown(),
        }
    }

    /// `true` when surface-bound (server/security) rules should be skipped for
    /// THIS file: the project is a proven static frontend AND the file itself
    /// carries no server-surface evidence. Even inside a "static" project, a
    /// file that imports a server framework / opens a listener / handles tokens
    /// is governed normally — the per-file evidence overrides the project-level
    /// hint, so we never under-govern a stray server file.
    #[must_use]
    fn skip_server_surface(self, file_path: &str, content: &str) -> bool {
        self.static_frontend_only && !file_has_server_evidence(file_path, content)
    }
}

/// The substrings that mark a file as carrying its own server / security
/// surface. Built at first use so the literal route-handler tokens never sit
/// inline in a way that trips a content scanner over this source file itself.
fn server_evidence_needles() -> &'static [String] {
    static NEEDLES: OnceLock<Vec<String>> = OnceLock::new();
    NEEDLES.get_or_init(|| {
        let app = "app";
        let methods = ["listen", "use(", "get(", "post(", "put(", "delete("];
        let mut v: Vec<String> = methods.iter().map(|m| format!("{app}.{m}")).collect();
        let route = "export async function";
        for verb in ["post", "put", "delete", "get"] {
            v.push(format!("{route} {verb}"));
        }
        v.extend(
            [
                "createserver",
                "http.server",
                "fastapi",
                "express(",
                "from 'express'",
                "from \"express\"",
                "flask(",
                "django",
                "actix_web",
                "axum::",
                // Auth / session / token plane.
                "jsonwebtoken",
                "jwt.sign",
                "jwt.verify",
                "req.session",
                "express-session",
                "set-cookie",
                "getsession",
                "getserversession",
            ]
            .iter()
            .map(|s| (*s).to_string()),
        );
        v
    })
}

/// Heuristic: does THIS file carry its own server / security-surface evidence?
///
/// Used as the per-file override on top of the project-level
/// [`ProjectContext`]: even in a project classified static-frontend-only, a
/// file that boots a server, defines an API route, imports a backend framework,
/// or manipulates auth tokens/sessions IS a server surface and must be governed
/// normally. Conservative on the "has a surface" side — when in doubt it says
/// `true` so a real server file is never skipped.
fn file_has_server_evidence(file_path: &str, content: &str) -> bool {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Server-shaped filenames are a surface regardless of content — but only if
    // they're code/config, not a static html page.
    let ext = extension_of(file_path);
    if matches!(ext.as_str(), "ts" | "js" | "mjs" | "cjs" | "conf")
        && (name.starts_with("server.")
            || name.starts_with("next.config")
            || name.starts_with("middleware.")
            || name == "nginx.conf")
    {
        return true;
    }
    let lower = content.to_ascii_lowercase();
    server_evidence_needles()
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Public probe: does `(file_path, content)` carry its own server / security
/// surface (a listener, an API route, a backend framework import, auth/token
/// handling)? Used by the agent's [`ProjectContext`] derivation to decide
/// whether a project has grown a backend — a single server-bearing file flips
/// the whole project to strict governance. Mirrors the per-file override the
/// scanner uses internally; conservative (returns `true` when in doubt).
#[must_use]
pub fn file_has_server_surface(file_path: &str, content: &str) -> bool {
    file_has_server_evidence(file_path, content)
}

/// The **context-relevant** rules — they only protect a server / security
/// surface and so are skipped for a proven static frontend (see
/// [`ProjectContext`]). Each only fires when the file it scans actually has the
/// surface it guards (a server file, an HTML response, a token context); this
/// list is the project-level gate ON TOP of that per-rule self-check, so a
/// static-frontend project never even runs them on its plain UI files.
///
/// NOTE: these are deliberately the rules whose *only* job is a server/web
/// response header or a backend logging/token discipline. Injection / unsafe-
/// deserialization / secret / TLS / CORS rules stay in the always-on list —
/// those are dangerous wherever they appear, surface or not.
const SERVER_SURFACE_RULES: &[fn(&str, &str) -> Decision] = &[
    check_csp_required,            // UD-ARCH-013 — CSP response header
    check_clickjacking_protection, // UD-ARCH-046 — X-Frame-Options / frame-ancestors
    check_security_headers,        // UD-ARCH-019 — helmet / security headers
    check_hsts_header,             // UD-ARCH-022 — Strict-Transport-Security
    check_https_redirect,          // UD-ARCH-016 — HTTPS redirect
    check_csrf_protection,         // UD-ARCH-047 — CSRF on state-changing routes
    check_structured_logging,      // UD-ARCH-012 — structured backend logging
    check_insecure_random,         // UD-ARCH-043 — crypto RNG in a token context
];

/// Run all content-scan governance rules against a file's proposed content.
///
/// This is the programmatic entry point the agent runner uses to govern host
/// output (from ANY runtime — claude-code, codex, opencode),
/// independent of the PreToolUse hook (which only fires for CLI hosts). It
/// runs every content rule in precedence order and returns the first block.
/// Path-only rules (UD-SEC-001 sensitive-path) aren't checked here because
/// the runner already knows the output path is safe; only content scans apply.
///
/// Returns `Decision::pass()` when the content is clean.
#[must_use]
pub fn scan_content(file_path: &str, content: &str) -> Decision {
    scan_content_with_policy(file_path, content, &crate::policy::Policy::default())
}

/// Same as [`scan_content`] but honours a per-project [`crate::Policy`]:
/// - disabled rules are skipped entirely;
/// - excluded paths short-circuit to pass before any rule runs;
/// - the extra blocked-domains list is merged into the URL check.
///
/// The CLI hook loads `.umadev/rules.toml` once per invocation and passes
/// it here; the agent runner loads it once per pipeline.
///
/// Uses the conservative [`ProjectContext::unknown`] (assume a server surface
/// might exist → every context-relevant rule stays on). Callers that KNOW the
/// run is a static frontend use [`scan_content_with_context`] to skip the
/// surface-bound rules that have nothing to guard.
#[must_use]
pub fn scan_content_with_policy(
    file_path: &str,
    content: &str,
    policy: &crate::policy::Policy,
) -> Decision {
    scan_content_with_context(file_path, content, policy, ProjectContext::unknown())
}

/// Run one rule fail-open: if `check` PANICS on adversarial input (an out-of-
/// bounds slice, an unchecked index, a bad UTF-8 boundary…), catch the unwind
/// and return [`Decision::pass`] instead of crashing the host.
///
/// The whole fail-open guarantee otherwise rests on each of the ~110 `check_*`
/// fns being individually panic-free; this is the backstop that makes a single
/// future buggy rule unable to take down the host (governance is fail-open *by
/// contract*). `AssertUnwindSafe` is sound here: the closure only borrows two
/// immutable `&str`s and calls a pure fn — there is no shared mutable state that
/// could be observed in a torn condition after the unwind.
fn run_check_guarded(
    check: fn(&str, &str) -> Decision,
    file_path: &str,
    content: &str,
) -> Decision {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(file_path, content)))
        .unwrap_or_else(|_| Decision::pass())
}

const CONTENT_CHECKS: &[fn(&str, &str) -> Decision] = &[
    check_hardcoded_secret,
    check_frontend_db_access,
    check_ts_any,
    check_loose_array_types,
    check_non_null_assertion,
    check_debug_residue,
    check_bare_catch,
    check_api_error_convention,
    check_input_validation,
    check_error_boundary,
    check_i18n_required,
    check_a11y,
    check_inline_styles,
    check_ssrf,
    check_sql_injection,
    check_xpath_injection,
    check_xxe,
    check_insecure_cors,
    check_insecure_cookie,
    check_jwt_defects,
    check_csp_required,
    check_https_redirect,
    check_hsts_header,
    check_security_headers,
    check_missing_auth_guard,
    check_db_transaction_rollback,
    check_c_buffer_overflow,
    check_c_malloc_null_check,
    check_rate_limiting,
    check_structured_logging,
    check_magic_numbers,
    check_todo_residue,
    check_unused_variables,
    check_deep_nesting,
    check_python_bare_except,
    check_python_global,
    check_rust_unwrap,
    check_go_panic,
    check_java_system_exit,
    check_swift_force_unwrap,
    check_kotlin_nonnull_assertion,
    check_php_shell_exec,
    check_ruby_eval_send,
    check_malicious_urls,
    check_typosquat_packages,
    check_eval_injection,
    check_weak_crypto,
    check_template_injection,
    check_command_injection,
    check_unsafe_deserialization,
    check_unreliable_sources,
    check_hardcoded_config,
    check_plaintext_password,
    check_file_upload_validation,
    check_open_redirect,
    check_sensitive_logging,
    check_insecure_random,
    check_redos_regex,
    check_path_traversal,
    check_mass_assignment,
    check_response_splitting,
    check_info_leakage,
    check_clickjacking_protection,
    check_insecure_tls,
    check_csrf_protection,
    check_graphql_n_plus_1,
    check_graphql_depth_limit,
    check_graphql_introspection,
    check_websocket_auth,
    check_toctou_race,
    check_insecure_file_perms,
    check_unsynchronized_mutation,
    check_hard_delete,
    check_client_secret_leak,
    check_insecure_storage,
    check_unhandled_fetch_error,
    check_react_list_key,
    check_inline_event_handlers,
    check_use_effect_cleanup,
    check_state_mutation,
    check_referrer_redirect,
    check_dangerous_inner_html,
    check_prototype_pollution,
    check_insecure_jsonp,
    check_wildcard_imports,
    check_var_declarations,
    check_loose_equality,
    check_empty_deps_array,
    check_document_cookie_access,
    check_untyped_props,
    check_unsafe_window_open,
    check_render_side_effects,
    check_promise_without_catch,
    check_mutable_default_export,
    check_client_redirect_injection,
    check_unsafe_date_parse,
    check_unsafe_parse,
    check_unsafe_json_parse,
    check_unsafe_post_message,
    check_for_in_array,
    check_scala_null_return,
    check_r_hardcoded_path,
    check_lua_loadstring,
    check_perl_eval_regex,
    check_elixir_to_atom,
    check_haskell_unsafe_io,
    check_clojure_eval,
    check_ocaml_magic,
    check_fsharp_null,
    check_dart_dynamic,
    check_emoji,
    check_color_tokens,
];

/// Same as [`scan_content_with_policy`] but also honours a [`ProjectContext`].
///
/// The universal "always wrong" floor (emoji, hardcoded colors, swallowed
/// errors, real secrets, frontend→DB, AI-slop, injection, …) fires regardless
/// of context — it is independent of any attack surface. The smaller class of
/// **server/security-surface** rules (CSP, clickjacking, structured logging,
/// security headers, HSTS, HTTPS-redirect, CSRF, token-context RNG) is skipped
/// when `ctx` proves the project is a static frontend AND the file carries no
/// server evidence of its own. Conservative: an `unknown` context (the default)
/// keeps every rule on, so a real backend/auth project is never under-governed.
#[must_use]
#[allow(clippy::too_many_lines)] // it's a flat list of rule dispatches
pub fn scan_content_with_context(
    file_path: &str,
    content: &str,
    policy: &crate::policy::Policy,
    ctx: ProjectContext,
) -> Decision {
    // Excluded path → skip everything.
    if policy.is_excluded(file_path) {
        return Decision::pass();
    }
    // Server/security-surface rules only have something to guard when the
    // project has a server surface. For a proven static frontend with no
    // per-file server evidence, skip them inline — they'd flag a missing CSP /
    // structured-logger / HSTS on a project that serves none of that. The rules
    // stay in their original precedence positions; we just no-op the
    // surface-bound ones (identified via [`is_server_surface_rule`]) under a
    // static context. The universal floor is unaffected.
    let skip_surface = ctx.skip_server_surface(file_path, content);
    for &check in CONTENT_CHECKS {
        // A surface-bound rule guards nothing on a proven static frontend → skip
        // it (in place, so precedence is untouched for every other project).
        if skip_surface && is_server_surface_rule(check) {
            continue;
        }
        let d = run_check_guarded(check, file_path, content);
        if d.block {
            // Policy can disable this clause.
            if policy.is_disabled(&d.clause) {
                continue;
            }
            return d;
        }
    }

    // The AI-slop rule runs LAST, exactly where it sat in the list above — but it is the
    // one rule that needs to know what the USER asked for. It reads the banned indigo/
    // violet band, and that band is a default-REJECT, not a censor: a requirement that
    // asked for a purple brand (by name or by hex) stands it down, precisely as it stands
    // down the token-level banned-hue rule and the source-level design lint. Without the
    // stand-down this rule BLOCKS THE WRITE of a palette the user chose, and the design
    // floor then accepts the very tokens the write governor refused — an unconvergeable
    // build. Same fail-open guard as every other rule (a panic here can never take down
    // the host).
    let slop = content_scan::run_ai_slop_guarded(file_path, content, ctx);
    if slop.block && !policy.is_disabled(&slop.clause) {
        return slop;
    }

    Decision::pass()
}

mod content_scan;
pub use content_scan::scan_content_findings_with_context;

// ===================================================================
// Owned baseline SAST (Wave 4, §L4 / G8) — find security defects tool-free.
//
// `scan_content_*` is the PRE-WRITE hook: it returns the FIRST blocking
// decision and stops, because the host only needs one reason to refuse a
// write. A SAST PASS is different — `umadev security` / `report --review`
// must surface EVERY security defect in a file at once, not just the first,
// and without depending on gitleaks/semgrep being installed. So this collects
// ALL hits from the security-relevant subset of the existing rule engine
// (injection / missing-auth / hardcoded-secret / unsafe-deserialization /
// command-exec / weak-crypto …). It REUSES the rule functions verbatim — no
// re-implemented heuristics — it just runs them in collect-all mode over a
// classified severity map. gitleaks/cargo-audit remain optional UPGRADES.
// ===================================================================

/// How serious one owned-SAST finding is — used to rank the report and to let a
/// caller gate on `High` while still surfacing `Medium`/`Low` advisories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SastSeverity {
    /// A directly-exploitable defect: injection, a leaked secret, a missing auth
    /// guard, unsafe deserialization, eval/command execution of input.
    High,
    /// A real weakness that needs review but isn't a one-step exploit: weak
    /// crypto, insecure cookies/CORS/TLS, SSRF surface, path traversal.
    Medium,
    /// A hardening gap / hygiene issue (missing security header, no rate limit).
    Low,
}

impl SastSeverity {
    /// Stable lowercase id for display / serialization.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// One owned-SAST finding — a security defect found tool-free in one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SastFinding {
    /// Workspace-relative file the defect is in.
    pub file: String,
    /// The clause that fired (`UD-SEC-011` …) — the stable defect id.
    pub clause: String,
    /// How serious the defect is.
    pub severity: SastSeverity,
    /// A short, one-line description (the rule's reason, first sentence).
    pub message: String,
}

/// Classify a security clause into a severity bucket — keyed on the clause id the
/// rule function ACTUALLY returns at runtime (verified against the rule bodies).
/// Conservative: an unmapped clause defaults to `Medium` (real, review it) rather
/// than silently dropping a finding.
fn sast_severity(clause: &str) -> SastSeverity {
    use SastSeverity::{High, Low, Medium};
    match clause {
        // Directly-exploitable: injection, secrets, missing auth, code exec.
        "UD-SEC-003"  // hardcoded secret
        | "UD-SEC-007" // eval / new Function / template injection (code exec)
        | "UD-SEC-008" // unsafe deserialization (RCE vector)
        | "UD-SEC-011" // SQL injection
        | "UD-SEC-012" // XPath injection
        | "UD-SEC-013" // XXE
        | "UD-SEC-014" // command injection (string-built shell)
        | "UD-SEC-015" // JWT defects (alg:none / hardcoded secret)
        | "UD-SEC-018" // plaintext password / weak crypto over secrets
        | "UD-SEC-020" // path traversal
        | "UD-ARCH-023" // OS command injection (shell exec of input)
        | "UD-ARCH-025" // ruby eval/send metaprogramming injection
        | "UD-ARCH-026" // sensitive route missing auth guard
        => High,
        // Real weaknesses needing review.
        "UD-SEC-004"  // frontend reaching straight into a DB
        | "UD-SEC-009" // SSRF
        | "UD-SEC-010" // insecure CORS (reflected/wildcard origin)
        | "UD-SEC-019" // open redirect
        | "UD-ARCH-061" // client-side redirect injection
        | "UD-ARCH-043" // insecure RNG in a token/secret context
        => Medium,
        // Hardening gaps.
        "UD-ARCH-013" // CSP missing
        | "UD-ARCH-016" // HTTPS redirect missing
        | "UD-ARCH-019" // security headers missing
        | "UD-ARCH-022" // HSTS missing
        | "UD-ARCH-024" // shell-exec hardening
        => Low,
        // Any other security clause we route through here is a real defect we
        // simply haven't tiered — surface it as Medium, never drop it.
        _ => Medium,
    }
}

/// The security-relevant subset of the rule engine, run in COLLECT-ALL mode.
/// Deliberately omits the craft/style rules (emoji, color tokens, AI-slop,
/// magic numbers, unused vars, framework lints) — a SAST pass reports SECURITY
/// defects, not taste. Every entry is an existing rule function reused verbatim.
const SAST_CHECKS: &[fn(&str, &str) -> Decision] = &[
    check_hardcoded_secret,
    check_sql_injection,
    check_xpath_injection,
    check_xxe,
    check_command_injection,
    check_template_injection,
    check_eval_injection,
    check_missing_auth_guard,
    check_unsafe_deserialization,
    check_ssrf,
    check_weak_crypto,
    check_jwt_defects,
    check_insecure_cors,
    check_insecure_cookie,
    check_plaintext_password,
    check_path_traversal,
    check_open_redirect,
    check_client_redirect_injection,
    check_php_shell_exec,
    check_ruby_eval_send,
    check_insecure_random,
    check_frontend_db_access,
];

/// **Owned baseline SAST over one file** — every security defect, tool-free.
///
/// Runs the `SAST_CHECKS` subset in COLLECT-ALL mode (unlike the pre-write
/// hook, which stops at the first block) and returns one [`SastFinding`] per
/// firing rule, severity-classified. Pure + fail-open by construction: it only
/// calls the existing pure rule functions, dedups by clause, and never errors —
/// an empty result means "no security defect found in this file", exactly like a
/// clean external scanner. The `ctx` lets a proven static frontend skip the
/// server-surface rules (it has no auth/header surface to defend).
#[must_use]
pub fn sast_scan_file(file_path: &str, content: &str, ctx: ProjectContext) -> Vec<SastFinding> {
    let skip_surface = ctx.skip_server_surface(file_path, content);
    let mut out: Vec<SastFinding> = Vec::new();
    for check in SAST_CHECKS {
        if skip_surface && is_server_surface_rule(*check) {
            continue;
        }
        let d = run_check_guarded(*check, file_path, content);
        if !d.block {
            continue;
        }
        // Dedup by clause within a file (one rule may match several lines).
        if out.iter().any(|f| f.clause == d.clause) {
            continue;
        }
        out.push(SastFinding {
            file: file_path.to_string(),
            clause: d.clause.clone(),
            severity: sast_severity(&d.clause),
            // First sentence of the reason — the terse one-line defect summary.
            message: d
                .reason
                .split(". ")
                .next()
                .unwrap_or(&d.reason)
                .trim()
                .to_string(),
        });
    }
    out
}

/// `true` when `check` is one of the server/security-surface rules (compared by
/// function pointer). These are skipped for a static frontend with no per-file
/// server evidence; see [`SERVER_SURFACE_RULES`] / [`ProjectContext`].
fn is_server_surface_rule(check: fn(&str, &str) -> Decision) -> bool {
    SERVER_SURFACE_RULES
        .iter()
        .any(|f| std::ptr::fn_addr_eq(*f, check))
}

/// Whether a clause belongs to the **irreversible-if-written floor** — the only
/// violations the real-time WRITE hook refuses outright.
///
/// The governing principle (the product's "USB" architecture): UmaDev borrows
/// the base's brain to think and directs the base's *body* to do the work; it
/// must NOT pin the base's hands mid-write for a fixable nit. A leaked
/// secret/credential committed into source, a write into a sensitive path, or a
/// destructive shell command is **irreversible** — those must be stopped before
/// they happen. Every *other* governed defect (a11y, emoji-icon, hardcoded
/// color, injection, security-config, craft) is **fixable after the file
/// exists**, so the base is allowed to produce it and the post-write QC feedback
/// loop repairs it. This is what keeps a single a11y/emoji nit from blocking the
/// write entirely and leaving the base unable to recover (producing nothing).
///
/// `UD-SEC-001` sensitive path · `UD-SEC-002` dangerous bash · `UD-SEC-003`
/// hardcoded secret · `UD-SEC-018` plaintext password · `UD-SEC-026` client
/// secret leak.
#[must_use]
pub fn is_irreversible_write_floor(clause: &str) -> bool {
    matches!(
        clause,
        "UD-SEC-001" | "UD-SEC-002" | "UD-SEC-003" | "UD-SEC-018" | "UD-SEC-026"
    )
}

/// The bypass-immune, un-closable **irreversible write floor** — the ONE shared
/// entry point every write-governance surface runs FIRST.
///
/// A leaked secret / credential in committed source, or a write to a sensitive
/// path (`.env`, `.ssh/`, `id_rsa`, `credentials`, …), is irreversible the
/// instant it lands on disk + in git. So — exactly like Claude Code's
/// bypass-immune safetyCheck (`permissions.ts` step 1f/1g) — this floor MUST NOT
/// be switchable off by a project's `.umadev/rules.toml` disabled-clause list. It
/// therefore takes **no** [`Policy`](crate::policy::Policy) and deliberately
/// IGNORES disabled clauses: routing it through [`scan_content_with_policy`] /
/// [`scan_content_with_context`] would honour `is_disabled`, letting a rules.toml
/// quietly turn the floor off — a real bypass of the "bypass-immune" floor. That
/// is the whole point.
///
/// The Claude PreToolUse hook, the CI / pre-commit scan (`umadev ci`), the MCP
/// `govern_file` tool, and the non-Claude runner-side governance
/// (`continuous` / `director_loop`) all call this BEFORE their policy-aware
/// content scan, so every write-governance entry point enforces the identical
/// floor — including for a `.env` / `.ssh` / no-extension path a content-only
/// scan would miss.
///
/// Runs the SAME set the hook's floor uses (do NOT broaden it):
/// - `UD-SEC-001` sensitive path ([`check_sensitive_path`])
/// - `UD-SEC-003` hardcoded secret ([`check_hardcoded_secret`])
/// - `UD-SEC-018` plaintext password ([`check_plaintext_password`])
/// - `UD-SEC-026` client secret leak ([`check_client_secret_leak`])
///
/// Deterministic and **fail-open by contract**: each check runs under the same
/// panic-catching guard as the policy scan, so an adversarial input returns
/// [`Decision::pass`] rather than crashing the host. Returns the FIRST block, or
/// `Decision::pass()` when the proposed write is clean.
#[must_use]
pub fn pre_write_floor_decision(file_path: &str, content: &str) -> Decision {
    for check in [
        check_sensitive_path,
        check_hardcoded_secret_ungated,
        check_plaintext_password,
        check_client_secret_leak,
    ] {
        let decision = run_check_guarded(check, file_path, content);
        if decision.block {
            return decision;
        }
    }
    Decision::pass()
}

/// File extensions guarded by the emoji rule (UD-CODE-001).
const EMOJI_GUARDED_EXTS: &[&str] = &[
    "tsx", "ts", "jsx", "js", "mjs", "cjs", "vue", "svelte", "astro", "html", "htm", "css", "scss",
    "sass", "less", "py", "java", "kt", "go", "rs", "rb", "php", "cs", "swift", "md", "mdx",
];

/// UI source file types guarded by the color (UD-CODE-002) and AI-slop rules.
/// Narrower than EMOJI_GUARDED_EXTS: those quality checks only make sense for
/// frontend/UI source, not docs or backend code. Emoji (UD-CODE-001) is a
/// global prohibition and applies to the broader list above.
const UI_CODE_EXTS: &[&str] = &["tsx", "ts", "jsx", "js", "vue", "svelte", "astro"];

/// File extensions guarded by the color rule (UD-CODE-002).
const COLOR_GUARDED_EXTS: &[&str] = &[
    "tsx", "ts", "jsx", "js", "vue", "svelte", "astro", "css", "scss", "sass",
];

/// Path fragments that exempt a file from the color rule.
const COLOR_EXEMPT_FRAGMENTS: &[&str] = &[
    "/bin/",
    "/scripts/",
    "/tokens/",
    "/theme/",
    "/themes/",
    "/design-system/",
    "/design-tokens/",
    "/.storybook/",
    ".stories.",
    ".test.",
    ".spec.",
    "/fixtures/",
    "/mocks/",
];

/// Achromatic literals tolerated under the color rule.
const COLOR_ALLOWED: &[&str] = &[
    "#fff",
    "#ffffff",
    "#000",
    "#000000",
    // 8-digit (with alpha) and 4-digit pure white/black are equally achromatic.
    "#ffffffff",
    "#000000ff",
    "#00000000",
    "#ffffff00",
    "#ffff",
    "#0000",
];

fn emoji_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // TRUE pictographic-emoji ranges only. Leaves CJK ideographs and CJK
        // punctuation alone, and deliberately EXCLUDES three symbol blocks that
        // carry legitimate typographic / technical text, not emoji icons:
        //   - U+2300-23FF (Miscellaneous Technical): keyboard glyphs `⌥⌫⏎⎋`;
        //   - U+2460-24FF (Enclosed Alphanumerics): CJK numbering `①②③`;
        //   - U+25A0-25FF (Geometric Shapes): doc bullets `● ▶ ■ □ ▲ ▼ ◆`.
        // The remaining `\x{2600}-\x{27BF}` (Misc Symbols + Dingbats) DOES hold
        // real emoji (U+2705/U+274C/U+26A0), so it stays — the few typographic marks inside
        // it (`★ ☆ ♪ ✓ ✗`) are exempted per-char by `is_typographic_symbol`.
        // Covers: misc symbols + dingbats, pictographs, transport/map,
        // supplemental symbols, flags, skin-tone modifiers, and the keycap /
        // variation selectors that turn plain chars into emoji.
        Regex::new(concat!(
            r"[",
            r"\x{2600}-\x{27BF}",   // misc symbols + dingbats
            r"\x{2B00}-\x{2BFF}",   // misc symbols and arrows
            r"\x{1F000}-\x{1F0FF}", // mahjong + dominoes + playing cards
            r"\x{1F100}-\x{1F1FF}", // enclosed alphanumeric supplement + flags
            r"\x{1F200}-\x{1F2FF}", // enclosed ideographic supplement
            r"\x{1F300}-\x{1F5FF}", // misc symbols and pictographs
            r"\x{1F600}-\x{1F64F}", // emoticons
            r"\x{1F680}-\x{1F6FF}", // transport and map
            r"\x{1F700}-\x{1F77F}", // alchemical symbols
            r"\x{1F780}-\x{1F7FF}", // geometric shapes extended
            r"\x{1F800}-\x{1F8FF}", // supplemental arrows-C
            r"\x{1F900}-\x{1F9FF}", // supplemental symbols and pictographs
            r"\x{1FA00}-\x{1FA6F}", // chess symbols
            r"\x{1FA70}-\x{1FAFF}", // symbols and pictographs extended-A
            r"\x{1F3FB}-\x{1F3FF}", // skin-tone modifiers
            r"]",
        ))
        .expect("emoji regex is well-formed at compile time")
    })
}

/// `true` for typographic / technical glyphs that fall inside the emoji regex's
/// remaining code-point range (`\x{2600}-\x{27BF}`) but are legitimate symbols /
/// marks — bullets, rating stars, music notes, check/cross dingbats — NOT
/// emoji-as-functional-icons, so they must not trip UD-CODE-001:
/// - `★ ☆` (U+2605..U+2606) black / white star — rating & list marks
/// - `♩ ♪ ♫ ♬` (U+2669..U+266C) music notes
/// - `✓ ✔ ✕ ✖ ✗ ✘` (U+2713..U+2718) check / cross / multiply dingbats
///
/// The `⌈⌉⌊⌋` (U+2308..U+230B) / `⌘` (U+2318) entries are retained as a
/// belt-and-braces guard even though their block (U+2300-23FF) is now excluded
/// from the regex entirely.
///
/// Note: U+2705, U+274C, U+26A0, and U+2B50 are not in this set; those
/// colourful emoji marks remain blocked.
fn is_typographic_symbol(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2308..=0x230B | 0x2318 | 0x2605..=0x2606 | 0x2669..=0x266C | 0x2713..=0x2718,
    )
}

fn hex_color_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // EXACT CSS hex-color lengths only: 3, 4, 6, or 8 hex digits. The old
    // `{3,8}` matched 5- and 7-digit runs (never valid colors) and greedily
    // over-ran into longer id fragments, bouncing legit output into rework.
    // The trailing `\b` stops a partial match inside a longer token
    // (`#section-2`, a git SHA). A non-word LEFT boundary + the anchor filter
    // live in `check_color_tokens` (the `regex` crate has no look-behind), so a
    // fragment href (`href="#abc"`) is not flagged as a color.
    RE.get_or_init(|| {
        Regex::new(r"(?i)#(?:[0-9a-f]{8}|[0-9a-f]{6}|[0-9a-f]{4}|[0-9a-f]{3})\b")
            .expect("hex regex is well-formed")
    })
}

fn rgb_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\brgba?\s*\(").expect("rgb regex is well-formed"))
}

fn hsl_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bhsla?\s*\(").expect("hsl regex is well-formed"))
}

/// Unambiguous modern color functions; ambiguous short names stay stylesheet-only.
fn modern_color_fn_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:oklch|oklab|color-mix)\s*\(")
            .expect("modern color regex is well-formed")
    })
}

/// Stylesheet-only color detection (css / scss / sass): a curated set of
/// chromatic CSS *named* colors used as a color-property value (`color: red`,
/// `background: blue`, `border-color: green`...), plus the short modern color
/// functions (`lab()`/`lch()`/`hwb()`) whose names could collide with a JS
/// identifier. Gated to real stylesheets so `{ background: red }` in a JS/TS
/// object (where `red` is a variable) is never a false positive. White / black /
/// transparent / `currentColor` are intentionally absent (neutral, like the hex
/// allow-list).
fn css_color_value_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?ix)
            (?:
                \b(?: color | background(?:-color)? | border(?:-color)?
                    | outline(?:-color)? | fill | stroke | caret-color
                    | accent-color | text-decoration-color | column-rule-color
                    | stop-color | flood-color )
                \s* : \s*
                (?: red|blue|green|yellow|orange|purple|pink|violet|indigo|magenta
                  | cyan|teal|lime|maroon|navy|olive|aqua|fuchsia|crimson|gold|coral
                  | salmon|turquoise|tomato|orchid|plum|brown|gray|grey|silver
                  | lavender|khaki|beige|gainsboro|tan )
                \b
              |
                \b(?: lab | lch | hwb ) \s* \(
            )
            ",
        )
        .expect("css color value regex is well-formed")
    })
}

/// Matches token definitions, not fallback use such as `var(--brand, #ff0000)`.
fn css_custom_property_declaration_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?im)(^|[;{])[\t ]*--[-_a-z0-9]+[\t ]*:[^;{}\r\n]*(;|$)")
            .expect("CSS custom-property declaration regex is well-formed")
    })
}

/// `true` when the text immediately before a `#hex` match is an HTML/JSX
/// attribute-value opener (`="`, `='`, or a backtick), i.e. the hex is the
/// value of an `href`/`to`/anchor attribute (`href="#abc"`) — a fragment, not a
/// color. A real hardcoded color is written as a CSS value (`color:#abc`,
/// `color: '#abc'`) or is 6/8 digits (handled unconditionally by the caller).
fn is_attr_value_fragment(prefix: &str) -> bool {
    let mut it = prefix.chars().rev();
    matches!((it.next(), it.next()), (Some('"' | '\'' | '`'), Some('=')))
}

fn extension_of(file_path: &str) -> String {
    file_path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Exclude a conventional trailing Rust `#[cfg(test)] mod tests` module from
/// shipping-source checks. Test fixtures intentionally contain bad examples;
/// scanning them as product UI or endpoint code makes a rule self-trigger.
fn rust_shipping_prefix(content: &str) -> &str {
    content
        .rmatch_indices("#[cfg(test)]")
        .find_map(|(index, marker)| {
            content[index + marker.len()..]
                .lines()
                .find(|line| !line.trim().is_empty())
                .filter(|line| line.trim_start().starts_with("mod tests"))
                .map(|_| &content[..index])
        })
        .unwrap_or(content)
}

/// Check whether `content` would land emoji-as-functional-icons in a UI file.
///
/// Implements **UD-CODE-001** (`UMADEV_HOST_SPEC_V1` §3.1).
#[must_use]
pub fn check_emoji(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !EMOJI_GUARDED_EXTS.contains(&ext.as_str()) || looks_like_secret_test_path(file_path) {
        return Decision::pass();
    }
    // 4.6: tokenise the source and scan every region EXCEPT comments.
    // Emoji-as-icon violations can legitimately appear in JSX text nodes
    // (`<button>🚀</button>`), string literals (`const ICON = "🚀"`), or
    // code — all of which are kept by `without_comments`. Only comments
    // (`// 🚀 todo`) are documentation noise and must be skipped. Scoping
    // to `jsx_text()` alone would MISS string-literal emoji, so
    // `without_comments` is the correct (broader) view here.
    let content = if ext == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let tz = crate::tokenizer::Tokenized::new(content);
    let scan_text = tz.without_comments(content);
    // Each regex match is a single char (the class matches one code point). A
    // match that is ONLY a legit typographic/technical glyph (⌘, ⌈⌉⌊⌋, ✓/✔)
    // is not an emoji-as-icon and must not block.
    let has_emoji = emoji_regex()
        .find_iter(&scan_text)
        .flat_map(|m| m.as_str().chars())
        .any(|c| !is_typographic_symbol(c));
    if !has_emoji {
        return Decision::pass();
    }
    let reason = format!(
        "UmaDev: emoji detected in {ext} source file ({file_path}). \
         Use a declared icon library (Lucide / Heroicons / Tabler) instead \
         of emoji as functional icons. Replace the emoji before retrying."
    );
    Decision::block("UD-CODE-001", reason)
}

/// Check whether `content` contains hardcoded chromatic literals in a UI file.
///
/// Implements **UD-CODE-002** (`UMADEV_HOST_SPEC_V1` §3.2).
#[must_use]
pub fn check_color_tokens(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !COLOR_GUARDED_EXTS.contains(&ext.as_str()) {
        return Decision::pass();
    }
    let lower_path = file_path.to_ascii_lowercase();
    if COLOR_EXEMPT_FRAGMENTS
        .iter()
        .any(|frag| lower_path.contains(frag))
    {
        return Decision::pass();
    }

    // 4.6: scan the tokenised source, skipping comments. A color in a
    // comment (`/* placeholder #fff */`) is documentation, not a violation.
    let tz = crate::tokenizer::Tokenized::new(content);
    let is_stylesheet = matches!(ext.as_str(), "css" | "scss" | "sass");
    let without_comments = tz.without_comments(content);
    // CSS custom properties are the intended design-token definitions. Scan
    // every other declaration as before, including chromatic fallback values
    // at usage sites (`color: var(--brand, #ff0000)`).
    let scan_text = if is_stylesheet {
        let mut stripped = without_comments;
        loop {
            let next = css_custom_property_declaration_regex()
                .replace_all(&stripped, "$1 $2")
                .into_owned();
            if next == stripped {
                break stripped;
            }
            stripped = next;
        }
    } else {
        without_comments
    };
    let mut violations: Vec<String> = Vec::new();
    for m in hex_color_regex().find_iter(&scan_text) {
        let token = m.as_str().to_ascii_lowercase();
        if COLOR_ALLOWED.contains(&token.as_str()) {
            continue;
        }
        // Non-word LEFT boundary: a `#hex` glued to a word char (`id#abc`) or
        // an HTML numeric entity (`&#123;`) is not a color literal.
        let prefix = &scan_text[..m.start()];
        if let Some(p) = prefix.chars().next_back() {
            if p.is_alphanumeric() || p == '_' || p == '&' {
                continue;
            }
        }
        // A SHORT (3/4-digit) hex that is an HTML/JSX attribute value
        // (`href="#abc"`) is a fragment/anchor, not a color. 6/8-digit hexes are
        // unambiguous colors (e.g. SVG `fill="#ff0000"`) and stay flagged.
        let hex_digits = token.len().saturating_sub(1);
        if (hex_digits == 3 || hex_digits == 4) && is_attr_value_fragment(prefix) {
            continue;
        }
        if !violations.contains(&token) {
            violations.push(token);
        }
        if violations.len() >= 5 {
            break;
        }
    }
    if rgb_regex().is_match(&scan_text) && !violations.contains(&"rgb()/rgba()".to_string()) {
        violations.push("rgb()/rgba()".to_string());
    }
    if hsl_regex().is_match(&scan_text) && !violations.contains(&"hsl()/hsla()".to_string()) {
        violations.push("hsl()/hsla()".to_string());
    }
    // Modern color functions (oklch/oklab/color-mix) — bypass the hex/rgb/hsl
    // detector entirely, so add them explicitly. Safe in any UI source.
    if let Some(m) = modern_color_fn_regex().find(&scan_text) {
        let label = format!(
            "{}()",
            m.as_str()
                .trim_end_matches(['(', ' ', '\t'])
                .to_ascii_lowercase()
        );
        if !violations.contains(&label) {
            violations.push(label);
        }
    }
    // Named colors + the JS-collision-prone short functions (lab/lch/hwb) are
    // detected only in a real stylesheet, where `property: red` is unambiguous.
    if is_stylesheet {
        if let Some(m) = css_color_value_regex().find(&scan_text) {
            let label = format!(
                "hardcoded color '{}'",
                m.as_str().split_whitespace().collect::<Vec<_>>().join(" ")
            );
            if !violations.contains(&label) {
                violations.push(label);
            }
        }
    }

    if violations.is_empty() {
        return Decision::pass();
    }

    let reason = format!(
        "UmaDev: hardcoded colors detected in {file_path}: {}. \
         Use design tokens (CSS vars, theme constants, or Tailwind theme \
         keys) from output/*-uiux.md instead. If this is a tokens / theme \
         / design-system file, move it under tokens/ or theme/ to exempt \
         the check.",
        violations.join(", ")
    );
    Decision::block("UD-CODE-002", reason)
}

/// Every `linear-gradient(…)` / `radial-gradient(…)` / `conic-gradient(…)` argument list
/// in `lower` — the gradient's OWN stops, not the file around it.
///
/// This is what makes the purple→pink test evidence-based. The old test asked whether the
/// FILE contained a gradient anywhere, a purple anywhere, and a pink anywhere — so a
/// neutral radial-gradient glow, plus a `--brand-violet` token, plus an `--accent-pink`
/// token (three unrelated things, no purple→pink gradient in sight) had the write
/// REJECTED. A rule that fires on co-occurrence is not detecting the tell; it is
/// detecting the palette.
///
/// Bounded + panic-free: balanced-paren scan, at most [`MAX_GRADIENTS`] fragments per file.
///
/// There is deliberately **no per-gradient length cap**. There used to be one (2 KB), and it
/// was a silent BYPASS: a gradient whose argument list ran past it never reached `depth == 0`,
/// so the fragment was DROPPED and the file read as gradient-free. Padding a
/// `linear-gradient(135deg, #8b5cf6 …, #ec4899)` past the cap — with perfectly legitimate
/// stops, or just by minifying the stylesheet into one long line — walked the classic AI hero
/// straight through the write governor. The cap also bought nothing: a fragment here is a
/// BORROWED slice, not a copy, so its length costs no memory. The real bound is the file, and
/// the scan is linear in it (`MAX_GRADIENTS` × file length, worst case).
///
/// An UNTERMINATED call yields the remainder of the file rather than nothing — the stops that
/// are there are still the stops that are there.
fn gradient_stops(lower: &str) -> Vec<&str> {
    /// Cap on gradients examined per file.
    const MAX_GRADIENTS: usize = 32;

    let mut out: Vec<&str> = Vec::new();
    let bytes = lower.as_bytes();
    let mut from = 0usize;
    while from < lower.len() && out.len() < MAX_GRADIENTS {
        // The next gradient call of any flavour (`repeating-` prefixes match too).
        let Some(rel) = lower[from..].find("-gradient(") else {
            break;
        };
        let open = from + rel + "-gradient(".len(); // first byte INSIDE the parens
        let mut depth = 1usize;
        let mut i = open;
        while i < lower.len() {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        // Closed → the exact argument list. Unterminated → everything that is left (`i` is at
        // the end of the string, a char boundary). Never silently nothing.
        if lower.is_char_boundary(open) && i > open {
            out.push(&lower[open..i]);
        }
        from = open;
    }
    out
}

/// Whether a gradient's own stop list carries a purple/violet hue — by name, or by any
/// color literal ([`crate::color::parse_color`]) that lands in the same indigo/violet
/// band ([`crate::color::is_ai_purple`]) the design rules read. One band, one answer: a
/// stop written `rgb(124, 58, 237)` or `oklch(0.55 0.22 290)` is the same hue as
/// `#7c3aed`, and a rule that only recognises the hex form is trivially side-stepped.
fn stops_have_purple(stops: &str) -> bool {
    stops.contains("purple")
        || stops.contains("violet")
        || crate::design::ai_purple_literal(stops).is_some()
}

/// Whether a gradient's own stop list carries a rose/pink/fuchsia hue — by name, or by any
/// color literal that lands in the [`crate::color::is_ai_pink`] BAND.
///
/// A hex list is how this half of the rule was walked past: it knew `#ec4899` and `#f472b6`
/// and nothing else, so `linear-gradient(#7c3aed, #db2777)` and `linear-gradient(#7c3aed,
/// #f43f5e)` — the two commonest purple→pink heroes in the wild — did not block, while the
/// purple half of the same rule had been a band for a while. Both ends read the same way now.
fn stops_have_pink(stops: &str) -> bool {
    stops.contains("pink")
        || stops.contains("fuchsia")
        || stops.contains("magenta")
        || crate::design::ai_pink_literal(stops).is_some()
}

/// Check for common "AI slop" visual anti-patterns in UI source files.
///
/// P0-level checks (cardinal sins that make output look AI-generated):
/// - A purple→pink gradient (the classic AI template hero) — scoped to the gradient's
///   OWN stops, and stood down when the user asked for a purple brand
/// - "Lorem ipsum" placeholder text
/// - `Welcome to [App]` generic hero headings
///
/// The design intent is UNKNOWN on this path, so the banned-hue band applies at its
/// default-reject strength. A caller that knows what the user asked for must use
/// [`check_ai_slop_with_intent`] — a requested purple must stand this rule down exactly
/// as it stands down the token-level and source-level design checks, or the three
/// disagree and the build cannot converge.
///
/// Implements an extension of **UD-CODE-001/002** focused on visual
/// quality beyond just emoji and color tokens.
#[must_use]
pub fn check_ai_slop(file_path: &str, content: &str) -> Decision {
    check_ai_slop_with_intent(file_path, content, crate::design::DesignIntent::default())
}

/// [`check_ai_slop`], honouring what the user already decided (see
/// [`crate::design::DesignIntent`]).
///
/// This is the form the WRITE path uses ([`scan_content_with_context`] → the PreToolUse
/// hook + the in-process write governor), because a write blocker with no stand-down is
/// worse than a gate: the user who said "our brand is violet `#7c3aed`" cannot write the
/// palette they asked for, and there is no fix — the design floor accepts the very tokens
/// this rule refuses.
#[must_use]
pub fn check_ai_slop_with_intent(
    file_path: &str,
    content: &str,
    intent: crate::design::DesignIntent,
) -> Decision {
    let ext = extension_of(file_path);
    // The gradient half of this rule is a COLOR rule, so it is scoped like one
    // (`COLOR_GUARDED_EXTS` — which includes css/scss/sass). Gating the whole check on
    // `UI_CODE_EXTS` meant the purple→pink gradient rule never ran on a STYLESHEET: the
    // single most natural place in any codebase to write the gradient it exists to catch.
    // The content half (lorem ipsum / "Welcome to" / console.log / placeholder copy) stays
    // scoped to component source, where those tells actually live.
    let is_ui_code = UI_CODE_EXTS.contains(&ext.as_str());
    let is_color_guarded = COLOR_GUARDED_EXTS.contains(&ext.as_str());
    if !is_ui_code && !is_color_guarded {
        return Decision::pass();
    }
    // Test / fixture / mock / story files legitimately carry the very patterns
    // this rule flags — `example.com` (RFC-2606 reserved), `console.log(`, fake
    // emails, placeholder copy — as test data. Exempt them exactly like
    // [`check_color_tokens`] does (same `COLOR_EXEMPT_FRAGMENTS`), so legit
    // fixtures don't bounce into rework.
    let lower_path = file_path.to_ascii_lowercase();
    if COLOR_EXEMPT_FRAGMENTS
        .iter()
        .any(|frag| lower_path.contains(frag))
    {
        return Decision::pass();
    }

    // Tokenize once and scan code+strings+JSX-text (skip comments), the
    // same view `check_emoji` / `check_color_tokens` use. Previously this
    // rule lowercased the RAW source, so a comment like
    // `// TODO: replace the lorem ipsum` would falsely block — the very
    // class of false positive the other two rules were upgraded to avoid.
    let tz = crate::tokenizer::Tokenized::new(content);
    let body = tz.without_comments(content);
    let lower = body.to_ascii_lowercase();

    let mut issues: Vec<&str> = Vec::new();
    if is_ui_code && (lower.contains("lorem ipsum") || lower.contains("dolor sit amet")) {
        issues.push("Lorem ipsum placeholder text");
    }
    if is_ui_code
        && lower.contains("welcome to")
        && (lower.contains("<h1") || lower.contains("<h2") || lower.contains("heading"))
    {
        issues.push("Generic 'Welcome to [App]' heading");
    }
    // THE BANNED HUE — a DEFAULT-REJECT, not a censor.
    //
    // Two things make this safe to run on the WRITE path:
    //
    // 1. A requested purple stands it down (`intent.purple_allowed`), the same condition
    //    the token-level banned-hue rule and the source-level design lint stand down on.
    //    Three checks over one band must agree, or the fix for one is the violation of
    //    another and the build cannot converge.
    // 2. The test is scoped to a REAL gradient declaration — the tell is a purple→pink
    //    gradient, so we look inside the gradient's own stops. A file-wide co-occurrence
    //    (any gradient + any purple + any pink, anywhere) is not that tell: a neutral
    //    radial-gradient glow next to a `--brand-violet` and an `--accent-pink` token is
    //    a legitimately-chosen palette with no purple→pink gradient in it, and rejecting
    //    that write leaves the author nothing to fix.
    if !intent.purple_allowed {
        for stops in gradient_stops(&lower) {
            if stops_have_purple(stops) && stops_have_pink(stops) {
                issues.push("Purple-to-pink gradient (classic AI template pattern)");
                break;
            }
        }
        // The single most recognizable AI-generated hero gradient — the
        // `#667eea → #764ba2` indigo-purple pairing (and its `#5a67d8` kin).
        // These specific hexes co-occurring IN ONE GRADIENT's stops are a near-certain
        // AI tell on their own, no pink companion required.
        for stops in gradient_stops(&lower) {
            if (stops.contains("#667eea") || stops.contains("#5a67d8")) && stops.contains("#764ba2")
            {
                issues.push("Canonical AI hero gradient (#667eea→#764ba2 indigo-purple)");
                break;
            }
        }
    }

    // Placeholder / fake-data / debug-residue tells — component-source concerns (a
    // stylesheet has no `console.log` and its `content:` strings are not app copy).
    if is_ui_code {
        if lower.contains("your code here")
            || lower.contains("your message here")
            || lower.contains("your text here")
            || lower.contains("replace this")
            || lower.contains("your-api-key-here")
        {
            issues.push("Unfilled placeholder text");
        }
        // Flag a bare `example.com` placeholder HOST (`https://example.com/…`) but
        // NOT a subdomain reference like `docs.example.com` / `api.example.com`,
        // which is a legitimate documentation host. The old `// docs.example.com`
        // exemption was dead code (comments are already stripped from `lower`).
        if lower.contains("://example.com") {
            issues.push("example.com placeholder URL (use a real domain)");
        }
        if lower.contains("test@test.com")
            || lower.contains("user@example")
            || lower.contains("john@example")
        {
            issues.push("Fake placeholder email (use realistic sample data)");
        }
        // Debug residue left in shipped code.
        if lower.contains("console.log(") {
            issues.push("console.log() debug residue (remove before shipping)");
        }
    }

    if issues.is_empty() {
        return Decision::pass();
    }

    let reason = format!(
        "UmaDev anti-slop: {} detected in {file_path}. \
         These patterns make output look AI-generated. \
         Use real content and design tokens from output/*-uiux.md.",
        issues.join("; ")
    );
    // Attribute to UD-CODE-002 (hardcoded color literals / design tokens):
    // the design part of this check (purple→pink gradient) IS a color-token
    // violation, and the content part (Lorem ipsum / "Welcome to") shares the
    // same "looks auto-generated" design-quality concern. We deliberately do
    // NOT use UD-CODE-005 — that id is reserved by the spec (§10) for the
    // future V2 accessibility-token clause and is non-normative in V1.
    Decision::block("UD-CODE-002", reason)
}

/// Directory names that mark any write *inside* them as sensitive. Matched
/// as a path segment (so `.git/` matches `a/.git/b` AND `.git/b` but not
/// `digit.ts`). Part of the bypass-immune safety check (UD-SEC-001).
const SENSITIVE_DIRS: &[&str] = &[".git", ".ssh", ".aws", ".claude", ".vscode"];

/// Specific sensitive path *suffixes* (file/dir names) matched against the
/// normalized path. Each is matched as a trailing path component so it works
/// for both absolute (`/x/.env`) and relative (`.env`) targets.
const SENSITIVE_PATH_SUFFIXES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    ".umadevrc",
    "credentials",
    "credentials.json",
    "service-account.json",
    ".npmrc",
    ".netrc",
    ".pypirc",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
];

/// Check whether a write targets a security-sensitive path. Implements
/// **UD-SEC-001**: a bypass-immune guard that blocks the host from writing
/// into version-control internals (`.git/`), secret stores (`.env`,
/// `~/.ssh/`, `~/.aws/`), or the host's own configuration (`.claude/settings`,
/// `.vscode/settings`). Unlike the code-style rules this is a SAFETY check,
/// not a quality check — it fires first and is exempt from any future
/// "skip governance" toggle, mirroring Claude Code's bypass-immune
/// safetyCheck (`utils/permissions/permissions.ts` step 1f/1g).
#[must_use]
pub fn check_sensitive_path(file_path: &str, _content: &str) -> Decision {
    let normalized = file_path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    // 1. Segment match for sensitive directories: any path component equal to
    //    a SENSITIVE_DIRS entry (or settings.json *inside* .claude/.vscode)
    //    is blocked. Splitting on '/' avoids the `digit.ts` false positive a
    //    naive `.contains(".git")` would produce.
    for seg in lower.split('/') {
        if SENSITIVE_DIRS.contains(&seg) {
            return Decision::block(
 "UD-SEC-001",
                format!(
 "UmaDev: write to sensitive path `{file_path}` blocked (UD-SEC-001).                      A parent segment (`{seg}`) holds version-control internals, secrets,                      or toolchain config — overwriting it can corrupt the repo or leak                      credentials. If this is intentional, exclude this path from the                      governance hook or run the host outside UmaDev's supervision."
                ),
            );
        }
    }
    // 2. Trailing-path-suffix match: `.env`, `id_rsa`, `settings.json`, etc.
    //    matched against the END of the normalized path so both `.env` and
    // `apps/api/.env` are caught.
    for suffix in SENSITIVE_PATH_SUFFIXES {
        if lower == *suffix || lower.ends_with(&format!("/{suffix}")) {
            return Decision::block(
 "UD-SEC-001",
                format!(
 "UmaDev: write to sensitive file `{file_path}` blocked (UD-SEC-001). `{suffix}` typically holds secrets, credentials, or toolchain config.                      If this is intentional and not a real secret, rename the file or                      exclude it from the governance hook."
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-SEC-002**: block destructive shell commands before the host runs them.
///
/// This is the real-time guard for `Bash` tool calls (the hook also intercepts
/// `Write`/`Edit` via UD-SEC-001/UD-CODE-*). It pattern-matches the command
/// string against known catastrophic patterns and denies them with a concrete
/// reason the host can act on. Like UD-SEC-001 it is bypass-immune and runs
/// before any "skip governance" toggle could apply.
///
/// Fail-open: unparseable / non-string commands pass. We only block what we
/// can confidently identify as dangerous.
#[must_use]
pub fn check_dangerous_bash(command: &str) -> Decision {
    // Equivalent-form-robust structured floor FIRST. The fixed substring table
    // below only matches ONE spelling of each verb, so alternate flag
    // orders/spellings (`rm -fr /`, `rm -rf -- /`, `rm --recursive --force /`),
    // a `git -C <dir>` global-option prefix before `push`, or `git clean -fdx`
    // slip straight past it. Tokenize + match on intent so those equivalents
    // can't bypass the floor. Fail-open: `None` → fall through to the table.
    if let Some(decision) = check_dangerous_bash_structured(command) {
        return decision;
    }

    // Normalize: collapse runs of whitespace so `rm  -rf` and `rm\t-rf` match.
    let collapsed: String = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = collapsed.to_ascii_lowercase();

    for pattern in DESTRUCTIVE_BASH_PATTERNS {
        if lower.contains(pattern.trigger) {
            // Precision for the root-delete patterns: `rm -rf /` / `rm -rf ~`
            // are substrings of perfectly legitimate `rm -rf /tmp/foo` and
            // `rm -rf ~/.cache/x`. Only fire when the target really IS the root
            // / whole home, not a subpath under it.
            if (pattern.trigger == "rm -rf /" || pattern.trigger == "rm -rf ~")
                && !rm_target_is_catastrophic(&lower, pattern.trigger)
            {
                continue;
            }
            // Precision for command-NAME triggers (`shutdown`, `mkfs`, `halt`,
            // …): fire only when the word is actually INVOKED as a command, not
            // when it merely appears inside an argument or a quoted string — e.g.
            // `echo shutdown`, or a `git commit -m "… shutdown …"`. Only applies
            // to clean alphanumeric command words; path / multi-word triggers
            // (`/dev/sd`, `rm -rf /`, `dd if=`, `| sh`) keep matching as
            // substrings (they legitimately appear as arguments).
            if pattern.trigger.bytes().all(|b| b.is_ascii_alphanumeric())
                && !appears_as_command(&lower, pattern.trigger)
            {
                continue;
            }
            // Allow explicit "dry-run" for git commands (e.g. --dry-run).
            if pattern.git_only && !lower.contains("git ") && !lower.starts_with("git") {
                continue;
            }
            if pattern.allow_if.iter().any(|a| lower.contains(a)) {
                continue;
            }
            return Decision::block(
                "UD-SEC-002",
                format!(
                    "UmaDev: destructive command blocked (UD-SEC-002). \
                     The command matches a known catastrophic pattern (`{trigger}`). \
                     {why} {fix}",
                    trigger = pattern.trigger,
                    why = pattern.why,
                    fix = pattern.fix,
                ),
            );
        }
    }
    Decision::pass()
}

/// Does `word` appear as an actual command invocation in `lower` — at a command
/// position — rather than merely as a substring inside an argument or a quoted
/// string? A bare command-name trigger (`shutdown`, `mkfs`, …) should fire on
/// `shutdown -h`, `sudo shutdown`, `… ; shutdown`, but NOT on `echo shutdown`
/// or a `git commit -m "… shutdown …"`.
fn appears_as_command(lower: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = lower[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        // Whole word: the char after the name (if any) must not CONTINUE a word
        // — so `shutdownify` / `mkfstool` don't match, but `mkfs.ext4`,
        // `shutdown -h`, `halt;` do (`.`/`-`/`;`/space are all non-alphanumeric).
        let after_ok = lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        // Command position: preceded by nothing, a separator, or a privilege /
        // exec wrapper — not by another command's name or an opening quote
        // (which would make it an argument).
        let before = lower[..start].trim_end();
        let before_ok = before.is_empty()
            || before.ends_with([';', '|', '&', '\n', '('])
            || matches!(
                before.rsplit(char::is_whitespace).next().unwrap_or(""),
                "sudo" | "doas" | "exec" | "nohup" | "env" | "xargs" | "time" | "command"
            );
        if after_ok && before_ok {
            return true;
        }
        from = end;
    }
    false
}

/// For an `rm -rf /` / `rm -rf ~` trigger match: is the deletion target the
/// actual root / whole home (catastrophic), or merely a subpath under it
/// (`/tmp/foo`, `~/.cache`) that should be allowed? Looks at the char that
/// FOLLOWS the trigger's `/` or `~`.
fn rm_target_is_catastrophic(lower: &str, trigger: &str) -> bool {
    let Some(pos) = lower.find(trigger) else {
        return true;
    };
    let after = &lower[pos + trigger.len()..];
    match after.chars().next() {
        // `rm -rf /` / `rm -rf ~` exactly, or followed by a separator/glob.
        None => true,
        Some(c) if c.is_whitespace() => true,
        Some(';' | '&' | '|' | '*') => true,
        // `~/` then nothing more = the whole home; `~/foo` = a subpath.
        Some('/') => {
            let rest = &after[1..];
            rest.is_empty() || rest.starts_with(char::is_whitespace)
        }
        // A continuing path char (`/tmp`, `~bar`) — a subpath, not root.
        _ => false,
    }
}

/// **UD-SEC-002** (equivalent-form-robust floor): match destructive INTENT for
/// the highest-risk verbs so alternate spellings/flag orders can't bypass the
/// fixed [`DESTRUCTIVE_BASH_PATTERNS`] substring table. Tokenizes each shell
/// segment and matches:
/// - a recursive+force `rm` at a catastrophic root — any flag order/spelling
///   (`-rf`, `-fr`, `-r -f`, `-f -r`, `--recursive --force`), a `--`
///   end-of-options separator, targeting `/`, `~`, `$HOME`, or a wildcard
///   directly under one;
/// - `git push` even behind a `git -C <dir>` / `-c k=v` / `--git-dir=…`
///   global-option prefix that the `git push` substring can't see;
/// - a forced `git clean` (`-fd`/`-fdx`/`-xdf`/`--force -d`) in any flag order.
///
/// In-tree targets (`./build`, `target/`) stay allowed — this only closes the
/// ROOT / equivalent-form bypass, preserving the existing in-tree-vs-root
/// policy. Returns `Some(block)` on a catastrophic match, else `None` (fall
/// through to the substring table). Fail-open: any parse ambiguity yields
/// `None` and never blocks a benign command.
fn check_dangerous_bash_structured(command: &str) -> Option<Decision> {
    // Track whether an earlier pipeline segment was a NETWORK DOWNLOADER so a later segment
    // that is a bare shell interpreter is caught as a pipe-to-shell RCE (`curl ... | sh`)
    // regardless of the spacing around `|` - the literal-substring "| sh" trigger missed
    // `curl x|sh`, `curl x |sh`, and `curl x | sudo bash`.
    // A downloader → shell RCE lives inside ONE pipeline. `saw_downloader` therefore resets
    // at every SEQUENCE boundary (`&&` / `||` / `;` / `&` / newline / subshell) and only
    // persists across PIPE (`|`) stages WITHIN a statement. This is the fix for a false-BLOCK:
    // `curl -fsSL <url> -o s.sh && less s.sh && sh s.sh` (download → inspect → run — the exact
    // remediation the block message recommends) and `curl ... -o data.json && bash deploy.sh`
    // (fetch data, then run a PRE-EXISTING local script) are SAFE — the shell is sequenced
    // AFTER the download, not piped from it. Only `curl <url> | sh` (bytes piped straight into
    // an interpreter) is the RCE, and that pipe keeps `saw_downloader` set across the stage.
    for statement in shell_statements(command) {
        let mut saw_downloader = false;
        for segment in pipe_stages(&statement) {
            let tokens = tokenize_segment(&segment);
            if tokens.is_empty() {
                continue;
            }
            // Read the real command name past a sudo/env prefix or `VAR=val` assignment.
            let cmd0 = tokens.iter().find(|t| {
                let s = t.as_str();
                !matches!(
                    s,
                    "sudo" | "doas" | "env" | "command" | "exec" | "nohup" | "time" | "xargs"
                ) && !s.contains('=')
            });
            if let Some(cmd0) = cmd0 {
                let base = cmd0.rsplit(['/', '\\']).next().unwrap_or(cmd0);
                if matches!(base, "curl" | "wget" | "fetch") {
                    saw_downloader = true;
                } else if saw_downloader
                    && matches!(base, "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash")
                {
                    return Some(Decision::block(
                        "UD-SEC-002",
                        "UmaDev: remote-code-execution blocked (UD-SEC-002). This pipes a                          network download straight into a shell interpreter (`curl ... | sh`),                          which runs untrusted code with no integrity check - caught for every                          spelling (`|sh`, `| sh`, `| sudo bash`). fix: download to a file,                          inspect it, then run it: `curl -fsSL <url> -o s.sh && less s.sh && sh                          s.sh`.",
                    ));
                }
            }
            if catastrophic_rm(&tokens) {
                return Some(Decision::block(
                    "UD-SEC-002",
                    "UmaDev: destructive command blocked (UD-SEC-002). This is a \
                     recursive, forced `rm` targeting the filesystem root or the \
                     home directory — every equivalent form is caught (`-rf`, \
                     `-fr`, `-r -f`, `--recursive --force`, and `--` separators). \
                     fix: scope the deletion to a project-local directory, e.g. \
                     `rm -rf ./build` or `rm -rf target/`.",
                ));
            }
            if git_push_behind_globals(&tokens) {
                return Some(Decision::block(
                    "UD-SEC-002",
                    "UmaDev: destructive command blocked (UD-SEC-002). `git push` \
                     reaches a remote and (per UmaDev's trust contract) UmaDev never \
                     auto-pushes — this holds even behind a `git -C <dir>` or other \
                     global-option prefix. fix: let the user run the push, or use \
                     `git push --dry-run` to inspect.",
                ));
            }
            if git_force_clean(&tokens) {
                return Some(Decision::block(
                    "UD-SEC-002",
                    "UmaDev: destructive command blocked (UD-SEC-002). `git clean \
                     -f…` irreversibly deletes untracked files (and with `-d`/`-x`, \
                     whole untracked directories and ignored files) in any flag \
                     order. fix: inspect first with `git clean -n` (dry run), then \
                     remove only what you mean to.",
                ));
            }
        }
    }
    None
}

/// Split a command line into top-level STATEMENTS on the SEQUENCE separators
/// (`;`, `&&`, `||`, `&`, newline, and `(`/`)` subshell boundaries) — but NOT on
/// the PIPE `|`, which stays inside a statement so [`pipe_stages`] can walk it.
/// Keeping the pipe intra-statement is what lets `curl … | sh` (one pipeline) be
/// told apart from `curl … -o s.sh && sh s.sh` (two sequenced statements: the
/// SAFE download → inspect → run pattern). Lightweight: it does not fully honour
/// quoting, which is fine for the intent match — the substring table backstops
/// any odd split.
fn shell_statements(command: &str) -> Vec<String> {
    let mut normalized = command.replace("&&", "\n").replace("||", "\n");
    for sep in [';', '&', '(', ')'] {
        normalized = normalized.replace(sep, "\n");
    }
    normalized
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Split ONE statement into its pipeline stages on `|`. Stages within a statement
/// are connected by the pipe, so a network download in an earlier stage feeding a
/// shell interpreter in a later stage is the `curl … | sh` RCE. A statement with
/// no pipe yields a single stage (itself).
fn pipe_stages(statement: &str) -> Vec<String> {
    statement
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Whitespace-tokenize a single shell segment, stripping a pair of matching
/// surrounding quotes from each token. Enough to read a command name, its
/// flags, and its path arguments for the intent match.
fn tokenize_segment(segment: &str) -> Vec<String> {
    segment
        .split_whitespace()
        .map(|tok| {
            let bytes = tok.as_bytes();
            if bytes.len() >= 2
                && (bytes[0] == b'"' || bytes[0] == b'\'')
                && bytes[bytes.len() - 1] == bytes[0]
            {
                tok[1..tok.len() - 1].to_string()
            } else {
                tok.to_string()
            }
        })
        .collect()
}

/// Drop leading privilege/exec wrappers (`sudo`, `env FOO=bar`, `nohup`, …) so
/// the intent matcher sees the real command (`sudo rm -rf /` → `rm -rf /`).
fn strip_command_wrappers(tokens: &[String]) -> &[String] {
    let mut i = 0;
    while i < tokens.len() {
        let word = tokens[i].to_ascii_lowercase();
        let base = word.rsplit('/').next().unwrap_or(word.as_str());
        match base {
            "sudo" | "doas" | "nohup" | "exec" | "command" | "time" | "stdbuf" | "nice" => i += 1,
            "env" | "xargs" => {
                i += 1;
                // Skip any `VAR=value` assignments before the real command.
                while i < tokens.len() && tokens[i].contains('=') && !tokens[i].starts_with('-') {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    &tokens[i..]
}

/// Is this segment a recursive+force `rm` aimed at a catastrophic root? Accepts
/// every flag order/spelling — combined (`-rf`/`-fr`), separated (`-r -f`),
/// long (`--recursive --force`), and a `--` end-of-options separator — and
/// treats `/`, `~`, `$HOME`, or a wildcard directly under one as catastrophic.
fn catastrophic_rm(tokens: &[String]) -> bool {
    let tokens = strip_command_wrappers(tokens);
    let Some((cmd, rest)) = tokens.split_first() else {
        return false;
    };
    let cmd = cmd.to_ascii_lowercase();
    if cmd.rsplit('/').next().unwrap_or(cmd.as_str()) != "rm" {
        return false;
    }
    let mut recursive = false;
    let mut force = false;
    let mut end_of_opts = false;
    let mut dangerous_target = false;
    for tok in rest {
        if !end_of_opts && tok == "--" {
            end_of_opts = true;
            continue;
        }
        if !end_of_opts && tok.len() > 1 && tok.starts_with('-') {
            if let Some(long) = tok.strip_prefix("--") {
                match long {
                    "recursive" => recursive = true,
                    "force" => force = true,
                    _ => {}
                }
            } else {
                for c in tok.chars().skip(1) {
                    match c {
                        'r' | 'R' => recursive = true,
                        'f' => force = true,
                        _ => {}
                    }
                }
            }
            continue;
        }
        if is_dangerous_rm_target(tok) {
            dangerous_target = true;
        }
    }
    recursive && force && dangerous_target
}

/// A deletion target that means "the whole filesystem root or home dir", which
/// makes a recursive+force `rm` catastrophic. In-tree targets (`./build`,
/// `target/`, `node_modules`) are deliberately NOT dangerous — that preserves
/// the existing in-tree-vs-root policy; only the root / equivalent forms fire.
fn is_dangerous_rm_target(target: &str) -> bool {
    let trimmed = target.trim_matches(|c| c == '"' || c == '\'');
    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "/" | "/*"
            | "/."
            | "~"
            | "~/"
            | "~/*"
            | "$home"
            | "$home/"
            | "$home/*"
            | "${home}"
            | "${home}/"
            | "${home}/*"
    )
}

/// Extract `(subcommand, args_after_it)` from a `git …` segment, skipping any
/// global options between `git` and the subcommand — including the ones that
/// consume a following argument (`-C <dir>`, `-c <k=v>`, `--git-dir <p>`, …).
/// Returns `None` when the segment is not a git invocation. This is what lets
/// the floor see the real subcommand behind a `git -C <dir>` prefix.
fn git_subcommand(tokens: &[String]) -> Option<(String, Vec<String>)> {
    let tokens = strip_command_wrappers(tokens);
    let (cmd, rest) = tokens.split_first()?;
    let cmd = cmd.to_ascii_lowercase();
    if cmd.rsplit('/').next().unwrap_or(cmd.as_str()) != "git" {
        return None;
    }
    let mut i = 0;
    while i < rest.len() {
        let tok = &rest[i];
        if tok.starts_with('-') {
            // Global options taking a SEPARATE argument (space-form) must skip
            // that argument too, so we don't mistake it for the subcommand.
            let takes_arg = matches!(
                tok.as_str(),
                "-C" | "-c"
                    | "--git-dir"
                    | "--work-tree"
                    | "--namespace"
                    | "--super-prefix"
                    | "--config-env"
            );
            i += if takes_arg { 2 } else { 1 };
            continue;
        }
        return Some((tok.to_ascii_lowercase(), rest[i + 1..].to_vec()));
    }
    None
}

/// Is this segment a `git push` — even behind a global-option prefix the fixed
/// `git push` substring can't see? Mirrors the substring table's allow-list:
/// `--dry-run` (inspection) and `--force-with-lease` still pass.
fn git_push_behind_globals(tokens: &[String]) -> bool {
    let Some((sub, args)) = git_subcommand(tokens) else {
        return false;
    };
    if sub != "push" {
        return false;
    }
    let allowed = args.iter().any(|a| {
        a == "--dry-run" || a == "--force-with-lease" || a.starts_with("--force-with-lease=")
    });
    !allowed
}

/// Is this segment a forced `git clean` (irreversible untracked-file wipe) in
/// any flag order — `-fd`, `-fdx`, `-xdf`, `--force -d`? A dry run (`-n` /
/// `--dry-run`) passes.
fn git_force_clean(tokens: &[String]) -> bool {
    let Some((sub, args)) = git_subcommand(tokens) else {
        return false;
    };
    if sub != "clean" {
        return false;
    }
    let mut force = false;
    let mut dry_run = false;
    for arg in &args {
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "force" => force = true,
                "dry-run" => dry_run = true,
                _ => {}
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            for c in arg.chars().skip(1) {
                match c {
                    'f' => force = true,
                    'n' => dry_run = true,
                    _ => {}
                }
            }
        }
    }
    force && !dry_run
}

/// **UD-SEC-003**: block hardcoded secrets in source files.
///
/// Catches API keys, tokens, private keys, and passwords embedded directly in
/// code or config instead of read from environment variables. Scans shipping
/// source AND the config / IaC / env files where secrets are most commonly
/// leaked (`.env`, JSON/YAML/TOML, Terraform, Dockerfiles, shell) — see
/// [`is_secret_scanned_path`]. Runs as part of the `pre-write` hook on
/// Write/Edit tool calls and in the owned baseline SAST.
///
/// Layered, highest-signal first, returning the first hit:
/// 1. a PEM `-----BEGIN … PRIVATE KEY-----` block (an unambiguous key);
/// 2. a named key (`api_key`/`secret`/`token`/`password`/…) `=`/`:`-assigned a
///    quoted value — covers the spaced (`const API_KEY = "…"`) and JSON
///    (`"apiKey": "…"`) forms a contiguous-prefix scan misses;
/// 3. the contiguous assignment prefixes (`api_key=…`, env-style);
/// 4. bare provider key shapes (`sk-…`/`ghp_…`/`glpat-…`/`AIza…`/`SG.…`/…);
/// 5. a DB connection string with an embedded password;
/// 6. an entropy FALLBACK — a high-entropy quoted literal with no known name
///    (tuned to skip hashes / UUIDs / URLs / prose so it does not flood);
/// 7. a hardcoded long-lived JWT literal.
///
/// Steps 6–7 (the noisiest) are suppressed on test / fixture / example paths.
/// Fail-open on non-scanned files (docs, images, data) — `Decision::pass()`.
#[must_use]
#[allow(clippy::too_many_lines)] // a flat, ordered list of secret-detector dispatches
pub fn check_hardcoded_secret(file_path: &str, content: &str) -> Decision {
    if !is_secret_scanned_path(file_path) {
        return Decision::pass();
    }
    check_hardcoded_secret_ungated(file_path, content)
}

/// The secret detection WITHOUT the path-extension gate. The irreversible
/// pre-write FLOOR ([`pre_write_floor_decision`]) calls this so a leaked secret in
/// ANY written file — a `Makefile`, a no-extension config, a `.env`, an `.ssh`
/// key — is caught, not only the recognized code/config extensions that the
/// normal (overridable) content scan is gated to. UD-SEC-003; bypass-immune.
#[allow(clippy::too_many_lines)] // one sequential detector chain; splitting it hurts readability
pub(crate) fn check_hardcoded_secret_ungated(file_path: &str, content: &str) -> Decision {
    let test_path = looks_like_secret_test_path(file_path);
    // Rust files conventionally keep adversarial fixtures in one trailing
    // `#[cfg(test)] mod tests`. Keep unambiguous provider/PEM detection on the
    // whole file, but run name/entropy/co-occurrence heuristics only on shipping
    // code so a rule's own test vectors do not masquerade as leaked secrets.
    let heuristic_content = if extension_of(file_path) == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let lower = heuristic_content.to_ascii_lowercase();

    // 0. PEM private-key block — never a placeholder; the gravest, clearest leak.
    if let Some(label) = pem_private_key_label(content) {
        return Decision::block(
            "UD-SEC-003",
            format!(
                "UmaDev: hardcoded private key detected (UD-SEC-003). \
                 `{file_path}` embeds a {label} private-key block \
                 (`-----BEGIN … PRIVATE KEY-----`). A private key must NEVER live in \
                 source — load it from a secret store / an env var / a mounted file \
                 (gitignored) and rotate this key immediately if it was committed.",
            ),
        );
    }

    // 1. Named key + separator + quoted value: `const API_KEY = "…"`,
    //    `"apiKey": "…"`, `password: "…"` — the spaced / JSON-key forms the
    //    contiguous `SECRET_PREFIXES` scan cannot see.
    if let Some((name, len)) = named_secret_match(heuristic_content) {
        return Decision::block(
            "UD-SEC-003",
            format!(
                "UmaDev: hardcoded secret detected (UD-SEC-003). \
                 `{file_path}` assigns what looks like a real `{name}` a literal value \
                 (length {len}). Secrets must come from environment variables, never \
                 source code. Read it from `process.env.<NAME>` / `std::env::var(...)` \
                 and move the value to `.env` (gitignored).",
            ),
        );
    }

    // 2. Contiguous assignment-style prefixes (`api_key=value`, env files).
    for prefix in SECRET_PREFIXES {
        // Look for `prefix=...` or `prefix: ...` followed by a value that
        // looks like a real key (length > 20, not a placeholder).
        if let Some(idx) = lower.find(prefix) {
            let after = &heuristic_content[idx + prefix.len()..];
            let value: String = after
                .trim_start_matches(['=', ':', ' ', '"', '\''])
                .chars()
                .take_while(|c| !matches!(c, '"' | '\'' | '\n' | '\r'))
                .collect();
            // Skip obvious placeholders / examples.
            if value.chars().count() > 20 && !is_placeholder_value(&value) {
                return Decision::block(
                    "UD-SEC-003",
                    format!(
                        "UmaDev: hardcoded secret detected (UD-SEC-003). \
                         `{file_path}` embeds what looks like a real `{}` (value length {}). \
                         Secrets must come from environment variables, never source code. \
                         Replace with `process.env.{}` / `std::env::var(...)` and move the \
                         value to `.env` (gitignored).",
                        prefix.trim_end_matches(['=', ':']).to_uppercase(),
                        value.chars().count(),
                        prefix
                            .trim_end_matches(['=', ':'])
                            .replace(' ', "_")
                            .to_uppercase(),
                    ),
                );
            }
        }
    }
    // 3. Bare key-shape prefixes carry no `=`/`:` separator, so a raw substring
    //    match would fire on ordinary identifiers. `bare_secret_matches`
    //    enforces a leading word boundary plus the real trailing key shape
    //    before reporting a hit.
    if let Some((label, len)) = bare_secret_matches(content) {
        return Decision::block(
            "UD-SEC-003",
            format!(
                "UmaDev: hardcoded secret detected (UD-SEC-003). \
                 `{file_path}` embeds what looks like a real {label} key \
                 (value length {len}). Secrets must come from environment \
                 variables, never source code. Read it from \
                 `process.env.<NAME>` / `std::env::var(...)` and move the \
                 value to `.env` (gitignored).",
            ),
        );
    }
    // 4. Connection strings with embedded credentials.
    //    `postgres://user:password@host` / `mongodb://user:pass@host`
    for captures in db_credential_url_regex().captures_iter(heuristic_content) {
        let (Some(scheme), Some(password)) = (captures.get(1), captures.get(3)) else {
            continue;
        };
        let password = password.as_str();
        if !password.eq_ignore_ascii_case("password") && !is_placeholder_value(password) {
            return Decision::block(
                "UD-SEC-003",
                format!(
                    "UmaDev: credentials in DB connection string (UD-SEC-003). \
                     `{file_path}` has a `{}` URL with an embedded password. \
                     Use an env var: `process.env.DATABASE_URL` populated from `.env`.",
                    scheme.as_str(),
                ),
            );
        }
    }
    // 5. Entropy FALLBACK — a high-entropy quoted literal with no known name.
    //    The lowest-signal detector, so it is suppressed on test/fixture/example
    //    paths and tuned (length + entropy + hash/UUID/URL skips) not to flood.
    if !test_path {
        if let Some(len) = high_entropy_secret_literal(heuristic_content) {
            return Decision::block(
                "UD-SEC-003",
                format!(
                    "UmaDev: high-entropy secret literal detected (UD-SEC-003). \
                     `{file_path}` embeds a long, random-looking string literal (length {len}) \
                     with no recognizable key name — the shape of a leaked credential. If this \
                     is a secret, read it from an env var and move the value to `.env` \
                     (gitignored); if it is genuinely not a secret, keep it out of a key-shaped \
                     literal.",
                ),
            );
        }
        // 6. Hardcoded long-lived JWT literal (low priority).
        if let Some(len) = hardcoded_jwt_literal(heuristic_content) {
            return Decision::block(
                "UD-SEC-003",
                format!(
                    "UmaDev: hardcoded JWT detected (UD-SEC-003). \
                     `{file_path}` embeds a literal JSON Web Token (length {len}). A signed \
                     token is a bearer credential — never commit one; mint it at runtime or read \
                     it from an env var / secret store.",
                ),
            );
        }
    }
    Decision::pass()
}

/// `true` when [`check_hardcoded_secret`] will scan a file at `file_path`: a
/// shipping source file (`SECRET_SCAN_EXTENSIONS`), a config / IaC / env file
/// (`SECRET_CONFIG_EXTENSIONS` — the #1 real-world leak locations), or a
/// secret-bearing well-known filename (`Dockerfile`/`Containerfile`, any
/// `.env`-family file). The owned SAST uses this to decide which non-code files
/// to also walk for secrets.
#[must_use]
pub fn is_secret_scanned_path(file_path: &str) -> bool {
    let ext = extension_of(file_path);
    SECRET_SCAN_EXTENSIONS.contains(&ext.as_str()) || is_config_secret_path(file_path)
}

/// `true` when `file_path` is a CONFIG / IaC / env / shell file that is
/// secret-scanned but is NOT general code source (`SECRET_CONFIG_EXTENSIONS`
/// or a well-known secret-bearing filename: `Dockerfile`/`Containerfile`, any
/// `.env`-family file). The owned SAST uses this for its second, secret-only
/// pass — it already covers code source through its own source collector, so
/// this predicate is the disjoint config surface.
#[must_use]
pub fn is_config_secret_path(file_path: &str) -> bool {
    let ext = extension_of(file_path);
    if SECRET_CONFIG_EXTENSIONS.contains(&ext.as_str()) {
        return true;
    }
    let name = file_name_of(file_path).to_ascii_lowercase();
    // (`.env` and `foo.env` already match via the `env` extension above; only the
    // `.env.local` / `.env.production` family needs an explicit name check.)
    name == "dockerfile"
        || name.starts_with("dockerfile.")
        || name == "containerfile"
        || name.starts_with(".env.")
        // Canonical DOTLESS key / credential files carry a real private key or cloud
        // credential but have NO extension and aren't a dockerfile/.env, so the ext +
        // name checks above missed them entirely. EXACT names only (not a prefix), so an
        // `id_rsa.pub` PUBLIC key — safe — does not match `id_rsa`.
        || matches!(
            name.as_str(),
            "id_rsa"
                | "id_dsa"
                | "id_ecdsa"
                | "id_ed25519"
                | "credentials"
                | ".netrc"
                | "netrc"
                | ".pgpass"
        )
}

/// The final path component of `file_path` (handles `/` and `\` separators).
fn file_name_of(file_path: &str) -> &str {
    file_path.rsplit(['/', '\\']).next().unwrap_or(file_path)
}

/// `true` for a path where the NOISIEST secret detectors (the entropy + JWT
/// fallback) must be suppressed to avoid flooding: a test / fixture / example /
/// sample / template path (realistic-but-fake secrets), a generated LOCKFILE
/// (full of SRI integrity hashes), or a minified bundle (one giant high-entropy
/// line). The high-signal detectors (PEM, named keys, provider shapes) still fire
/// on these, so a REAL key here is not a free pass.
fn looks_like_secret_test_path(file_path: &str) -> bool {
    let l = file_path.to_ascii_lowercase();
    if l.contains(".test.")
        || l.contains(".spec.")
        || l.contains("_test.")
        || l.contains("_tests.")
        || l.ends_with("tests.rs")
        || l.contains("test_")
        || l.starts_with("tests/")
        || l.starts_with("test/")
        || l.contains("/tests/")
        || l.contains("/test/")
        || l.contains("/__tests__/")
        || l.contains("/testdata/")
        || l.contains("/fixtures/")
        || l.contains("/fixture/")
        || l.contains("/mocks/")
        || l.contains("/examples/")
        || l.contains("/example/")
        || l.contains(".example")
        || l.contains(".sample")
        || l.contains(".template")
        || l.contains(".dist")
        || l.contains(".mock")
        || l.contains(".min.")
    {
        return true;
    }
    // Generated lockfiles: high-entropy integrity hashes everywhere, no secrets.
    // (`*.lock` covers Cargo.lock / yarn.lock / poetry.lock / composer.lock / …)
    let name = file_name_of(&l);
    if matches!(name, "test.rs" | "tests.rs") {
        return true;
    }
    extension_of(name) == "lock"
        || name == "package-lock.json"
        || name == "npm-shrinkwrap.json"
        || name == "pnpm-lock.yaml"
        || name == "go.sum"
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
}

/// Scan `content` for a bare key-shape secret (a Stripe-style `sk_`/`pk_` key,
/// an AWS `AKIA` id, a GitHub `ghp_`/`gho_` token, a Slack `xoxb-` token, or a
/// `stripe_`-prefixed key) that carries no `=`/`:` separator.
///
/// Returns `Some((label, value_len))` for the first hit, or `None`.
///
/// Each candidate is found by [`bare_secret_regex`], then re-checked here for a
/// **leading word boundary** — the byte before the match must not be
/// `[A-Za-z0-9_]`. That boundary is what stops `risk_score`, `task_runner`,
/// `disk_usage`, `ask_user`, `spike_`, and `nakia`/`balalaika` from being read
/// as secrets, while a genuine `const KEY = "..."` still fires. The trailing
/// key shape is already enforced by the regex (length-gated alphanumerics for
/// the `_` prefixes; the exact 16-char form for AWS).
fn bare_secret_matches(content: &str) -> Option<(&'static str, usize)> {
    let re = bare_secret_regex();
    for m in re.captures_iter(content) {
        let whole = m.get(0)?;
        let start = whole.start();
        // Leading word boundary: the byte before the match must not continue a
        // word. (ASCII-only check — every prefix here is ASCII, so inspecting
        // the preceding byte is sufficient and avoids a char-boundary walk.)
        let prev_ok = content[..start]
            .bytes()
            .next_back()
            .is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_'));
        if !prev_ok {
            continue;
        }
        let matched = whole.as_str();
        // Re-check placeholders so example keys still pass, matching the
        // separator-prefix path's policy (anchored, never substring — a real
        // token that merely contains `test`/`foo` is NOT whitelisted).
        if is_placeholder_value(matched) {
            continue;
        }
        let label = bare_secret_label(&matched.to_ascii_lowercase());
        return Some((label, matched.len()));
    }
    None
}

/// Human-readable label for a bare-secret hit, derived from its prefix.
fn bare_secret_label(lower: &str) -> &'static str {
    if lower.starts_with("akia") || lower.starts_with("asia") {
        "AWS access-key"
    } else if lower.starts_with("github_pat_") {
        "GitHub fine-grained PAT"
    } else if lower.starts_with("ghp_")
        || lower.starts_with("gho_")
        || lower.starts_with("ghs_")
        || lower.starts_with("ghu_")
        || lower.starts_with("ghr_")
    {
        "GitHub token"
    } else if lower.starts_with("glpat-") {
        "GitLab PAT"
    } else if lower.starts_with("xox") {
        "Slack token"
    } else if lower.starts_with("aiza") {
        "Google API key"
    } else if lower.starts_with("sg.") {
        "SendGrid key"
    } else if lower.starts_with("npm_") {
        "npm token"
    } else if lower.starts_with("sk-") {
        "OpenAI key"
    } else if lower.starts_with("stripe_") {
        "Stripe"
    } else {
        // Stripe-style `sk_`/`pk_` publishable/secret key.
        "secret/publishable"
    }
}

/// Compiled detector for bare key-shape secrets (no `=`/`:` separator).
///
/// The leading word boundary is verified separately in [`bare_secret_matches`]
/// (the `regex` crate has no look-behind). Shapes:
/// - `sk-(proj-)?…{20,}` — OpenAI keys (HYPHEN, distinct from Stripe's `sk_`).
/// - `(sk_|pk_)…{16,}` — Stripe-style keys (incl. live/test variants); the
///   16-char floor keeps it off short identifiers.
/// - `stripe_…{16,}` — a `stripe_`-prefixed key value.
/// - `(ghp_|gho_|ghs_|ghu_|ghr_)…{20,}` / `github_pat_…{20,}` — GitHub tokens.
/// - `glpat-…{20,}` — GitLab personal-access tokens.
/// - `xox[bpars]-…{10,}` — Slack bot/user/app tokens.
/// - `AIza…{30,}` — Google API keys.
/// - `SG.…{16,}.…{16,}` — SendGrid keys.
/// - `npm_…{36}` — npm automation tokens.
/// - the exact AWS access-key-id forms `AKIA`/`ASIA` (case-insensitive), which
///   no longer fire on `nakia` / `balalaika`.
fn bare_secret_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)(?:",
            r"sk-(?:proj-)?[A-Za-z0-9_-]{20,}",
            r"|(?:sk_|pk_)[A-Za-z0-9_]{16,}",
            r"|stripe_[A-Za-z0-9]{16,}",
            r"|github_pat_[A-Za-z0-9_]{20,}",
            r"|(?:ghp_|gho_|ghs_|ghu_|ghr_)[A-Za-z0-9]{20,}",
            r"|glpat-[A-Za-z0-9_-]{20,}",
            r"|xox[bpars]-[A-Za-z0-9-]{10,}",
            r"|AIza[A-Za-z0-9_-]{30,}",
            r"|SG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}",
            r"|npm_[A-Za-z0-9]{36}",
            r"|(?:AKIA|ASIA)[0-9A-Z]{16}",
            r")",
        ))
        .expect("bare-secret regex is well-formed")
    })
}

/// Match a NAMED secret assignment: a key NAME (`api_key`/`secret`/`token`/
/// `password`/…) followed by `=`/`:` (with any spacing, and optionally a quoted
/// name as in JSON) and a QUOTED value. This is the form a contiguous
/// `name=value` prefix scan misses: `const API_KEY = "…"` (spaces) and
/// `"apiKey": "…"` (quote-colon). Returns `(matched_name, value_char_len)` for
/// the first non-placeholder hit. The quoted-value requirement keeps it off
/// `process.env.X` references and bare code expressions.
fn named_secret_match(content: &str) -> Option<(String, usize)> {
    for caps in named_secret_regex().captures_iter(content) {
        let (Some(name), Some(value)) = (caps.get(1), caps.get(2)) else {
            continue;
        };
        let value = value.as_str();
        if is_placeholder_value(value) {
            continue;
        }
        // Same guards the entropy fallback already applies (see
        // [`is_high_entropy_secret`]): a value that is a URL / data-URI /
        // filesystem path, or a low-entropy lowercase kebab-/snake-case slug
        // (a design token like `color-primary-strong`, an identifier, a
        // pagination cursor) is NOT a credential — it must not hard-block on
        // the un-overridable secret floor merely because it sits under a
        // `token`/`auth`/`secret` name. A genuine secret-shaped value
        // (`sk-ant-…`, `AKIA…`, a mixed-case / high-entropy base64 or hex blob)
        // has no `://`/`/` and mixes case or entropy, so it still blocks here.
        if looks_like_url_or_path(value) || looks_like_low_entropy_slug(value) {
            continue;
        }
        return Some((name.as_str().to_string(), value.chars().count()));
    }
    None
}

/// `true` when `s` is a low-entropy lowercase kebab-/snake-case slug — a design
/// token / identifier / cursor (`color-primary-strong`, `pagination-cursor-abc`,
/// `page_size_default`), NOT a credential.
///
/// The named-secret branch uses this (alongside [`looks_like_url_or_path`]) so a
/// slug assigned to a `token`/`auth`/`secret` name is not mistaken for a leaked
/// secret. Three conditions, each a distinguisher from a real credential:
/// - contains a `-`/`_`/`.` word separator (a slug is segmented; a raw key is not);
/// - only lowercase letters, digits, and those separators — real provider keys
///   mix case (`sk-ant-a1B2c3…`) or are uppercase (`AKIA…`), so they never match;
/// - Shannon entropy below the same 4.0-bit floor the entropy fallback treats as
///   secret-like, so a genuine all-lowercase HIGH-entropy blob still reads as a
///   secret and blocks.
fn looks_like_low_entropy_slug(s: &str) -> bool {
    if !s.contains(['-', '_', '.']) {
        return false;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    {
        return false;
    }
    shannon_entropy(s) < 4.0
}

/// Compiled detector for a named secret key assigned a quoted literal value.
///
/// `["']?` around the name allows a JSON quoted key (`"apiKey":`); `\s*[:=]\s*`
/// allows any spacing (`const API_KEY = "…"`); the value class excludes
/// whitespace and structural punctuation so it stops at the literal's end and
/// never runs into surrounding code. The 12-char value floor keeps it off short,
/// low-signal values. The NAME (`\b`-bounded) is the high-signal part — `secret`
/// will not match inside `secret_key`, which forces the longer alternative.
fn named_secret_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r#"(?i)["']?\b("#,
            r"api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|refresh[_-]?token",
            r"|access[_-]?key|client[_-]?secret|private[_-]?key|password|passwd|pwd",
            r"|secret|token|auth",
            r#")\b["']?\s*[:=]\s*["']([^\s"',;(){}]{12,})["']"#,
        ))
        .expect("named-secret regex is well-formed")
    })
}

/// Compiled detector for a PEM private-key block — an unambiguous leaked key.
/// Covers plain `PRIVATE KEY` plus the `RSA`/`EC`/`DSA`/`OPENSSH`/`PGP`/
/// `ENCRYPTED` variants (and the `PGP PRIVATE KEY BLOCK` form).
fn pem_private_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"-----BEGIN (?:(?:RSA|EC|DSA|OPENSSH|PGP|ENCRYPTED) )?PRIVATE KEY(?: BLOCK)?-----",
        )
        .expect("pem private-key regex is well-formed")
    })
}

/// Label for a PEM private-key hit (the key kind), or `None` when absent.
fn pem_private_key_label(content: &str) -> Option<&'static str> {
    for marker in pem_private_key_regex().find_iter(content) {
        let begin = marker.as_str();
        let end = begin.replacen("BEGIN", "END", 1);
        let Some(end_offset) = content[marker.end()..].find(&end) else {
            continue;
        };
        let body = &content[marker.end()..marker.end() + end_offset];
        // A marker in documentation or a short fixture is not a private key.
        // Real PEM payloads contain at least one full 64-character base64 row.
        let encoded_chars = body
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '='))
            .count();
        if encoded_chars < 64 {
            continue;
        }
        return Some(if begin.contains("RSA") {
            "RSA"
        } else if begin.contains("OPENSSH") {
            "OpenSSH"
        } else if begin.contains("EC ") {
            "EC"
        } else if begin.contains("DSA") {
            "DSA"
        } else if begin.contains("PGP") {
            "PGP"
        } else {
            "PKCS8"
        });
    }
    None
}

/// Compiled detector for a hardcoded 3-part JWT literal (`header.payload.sig`,
/// each base64url, both header and payload starting `eyJ`).
fn jwt_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]*")
            .expect("jwt regex is well-formed")
    })
}

/// Length of the first hardcoded JWT literal in `content`, or `None`.
fn hardcoded_jwt_literal(content: &str) -> Option<usize> {
    jwt_regex().find(content).map(|m| m.as_str().len())
}

/// Compiled detector that captures the inside of a quoted string literal of at
/// least 20 non-quote chars — the candidate surface for the entropy fallback.
fn quoted_literal_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"["']([^"'\r\n]{20,})["']"#).expect("quoted-literal regex is well-formed")
    })
}

/// Length of the first quoted string literal in `content` that looks like a
/// high-entropy secret with no recognizable key name, or `None`. The entropy
/// fallback's scan front-end.
fn high_entropy_secret_literal(content: &str) -> Option<usize> {
    for caps in quoted_literal_regex().captures_iter(content) {
        let val = caps.get(1)?.as_str();
        if is_high_entropy_secret(val) {
            return Some(val.chars().count());
        }
    }
    None
}

/// Whether `val` is a high-entropy string with the SHAPE of a leaked credential.
/// Deliberately conservative so the always-on scan does not flood: requires a
/// 20-char floor, no whitespace, an upper+lower+digit MIX, an opaque-token
/// alphabet, and a Shannon entropy >= 4.0 bits/byte. Requiring all three
/// character classes and excluding source punctuation prevents CSS snippets,
/// regex source, protocol identifiers, and code between quotes from becoming
/// irreversible false blocks. Known provider shapes are detected earlier and
/// do not rely on this fallback.
fn is_high_entropy_secret(val: &str) -> bool {
    if val.chars().count() < 20 {
        return false;
    }
    if val.chars().any(char::is_whitespace) {
        return false;
    }
    // An unnamed entropy fallback needs stronger evidence than a known provider
    // prefix or a secret-shaped variable name. Ordinary versioned identifiers
    // often contain letters + one digit; opaque credentials usually mix case.
    let has_upper = val.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = val.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = val.chars().any(|c| c.is_ascii_digit());
    if !(has_upper && has_lower && has_digit) {
        return false;
    }
    if !val.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'/' | b'=' | b'.')
    }) {
        return false;
    }
    if looks_like_hex_hash(val)
        || looks_like_uuid(val)
        || looks_like_url_or_path(val)
        || looks_like_integrity_or_digest(val)
        || crate::security_context::is_stable_non_secret_literal(val)
        || is_filler_value(&val.to_ascii_lowercase())
        || is_placeholder_value(val)
    {
        return false;
    }
    shannon_entropy(val) >= 4.0
}

/// `true` when `s` is a Subresource-Integrity hash (`sha512-…`) or an OCI image
/// digest (`sha256:…`) — high-entropy, but a content hash, not a credential.
fn looks_like_integrity_or_digest(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    [
        "sha1-", "sha256-", "sha384-", "sha512-", "md5-", "sha1:", "sha256:", "sha512:",
    ]
    .iter()
    .any(|p| l.starts_with(p))
}

/// Shannon entropy (bits per byte) of `s`; `0.0` for empty. Called only on short
/// bounded literals, so a `u32` byte count is sufficient.
fn shannon_entropy(s: &str) -> f64 {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[usize::from(b)] += 1;
    }
    let total = f64::from(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    let mut h = 0.0_f64;
    for &c in &counts {
        if c == 0 {
            continue;
        }
        let p = f64::from(c) / total;
        h -= p * p.log2();
    }
    h
}

/// `true` when `s` is a hex string at least 32 chars long — an MD5/SHA digest /
/// commit hash / checksum, not a secret. (High entropy, but a known non-secret.)
fn looks_like_hex_hash(s: &str) -> bool {
    s.len() >= 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `true` when `s` is a canonical `8-4-4-4-12` hex UUID.
fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// `true` when `s` is a URL / data-URI / filesystem-style path — high-entropy but
/// not a credential. Excluding any `/` also drops standard-base64 false anchors;
/// real keys are caught by the named/provider/PEM detectors above.
fn looks_like_url_or_path(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("://") || l.starts_with("data:") || l.starts_with("www.") || s.contains('/')
}

/// `true` when `v` is a single repeated alphanumeric character (>=4 of them,
/// ignoring separators) — `xxxxxxxx`, `00000000`, `--------`: filler, not a key.
fn is_filler_value(v: &str) -> bool {
    let mut chars = v.chars().filter(char::is_ascii_alphanumeric);
    let Some(first) = chars.next() else {
        return false;
    };
    let mut count = 1usize;
    let mut all_same = true;
    for c in chars {
        count += 1;
        if c != first {
            all_same = false;
        }
    }
    all_same && count >= 4
}

/// `true` when `value` is an example / placeholder, not a real secret.
///
/// Two tiers (the M7 fix): long unambiguous markers ([`SECRET_EXAMPLE_MARKERS`])
/// match as a SUBSTRING; short ambiguous words ([`SECRET_PLACEHOLDER_WORDS`])
/// match ONLY when they are essentially the whole value (the word optionally
/// followed by digits / separators), never as a substring — so a real
/// `mytestkey…secret` is not whitelisted by a stray `test`.
fn is_placeholder_value(value: &str) -> bool {
    let vl = value.to_ascii_lowercase();
    if vl.is_empty() {
        return true;
    }
    // Ellipsis / angle-bracket / shell-var / template markers.
    if vl.contains("...")
        || (vl.contains('<') && vl.contains('>'))
        || vl.contains("${")
        || vl.contains("{{")
    {
        return true;
    }
    if SECRET_EXAMPLE_MARKERS.iter().any(|m| vl.contains(m)) {
        return true;
    }
    if is_filler_value(&vl) {
        return true;
    }
    SECRET_PLACEHOLDER_WORDS.iter().any(|w| {
        vl == *w
            || vl.strip_prefix(w).is_some_and(|rest| {
                !rest.is_empty()
                    && rest
                        .chars()
                        .all(|c| c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
            })
    })
}

/// **UD-SEC-004**: block direct database access from frontend code.
///
/// Frontend code (browser bundles) must NEVER import a DB driver or open a
/// DB connection — that's a server-only concern and exposes credentials to
/// clients. Flags imports of pg/mongoose/mysql/mongodb/redis in frontend
/// source files.
#[must_use]
pub fn check_frontend_db_access(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !FRONTEND_EXTENSIONS.contains(&ext.as_str()) {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    for driver in FRONTEND_DB_DRIVERS {
        if lower.contains(driver) {
            return Decision::block(
                "UD-SEC-004",
                format!(
                    "UmaDev: database driver in frontend code (UD-SEC-004). \
                     `{file_path}` imports `{driver}` — a database driver must NEVER ship to the \
                     browser bundle (it leaks credentials and bypasses your API layer). \
                     Move DB access to a server route/API handler; the frontend should call \
                     your REST/GraphQL endpoints instead.",
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-ARCH-001**: ban `any` in TypeScript/TypeScript-JSX source.
///
/// `any` defeats the type system — a commercial codebase must use `unknown`
/// + a narrowing guard, or a concrete type. Flags `: any`, `as any`, and
/// angle-bracket `any` annotations. Allows `any` inside comments/strings
/// (best-effort). Runs as part of the `pre-write` hook on `.ts`/`.tsx` files.
#[must_use]
pub fn check_ts_any(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "tsx") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        // Skip comment-only lines.
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        // Strip out string literals so `any` inside a string doesn't count.
        // Best-effort: cut between quotes on this line.
        let no_strings = strip_string_literals(line);
        for pat in TS_ANY_PATTERNS {
            if no_strings.contains(pat) {
                hits += 1;
            }
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-001",
            format!(
                "UmaDev: `any` type banned in TypeScript (UD-ARCH-001). \
                 `{file_path}` uses `any` ({hits} occurrence{}). `any` disables the type \
                 checker — use `unknown` + a narrowing guard, or a concrete type. \
                 Example: `function f(x: unknown) {{ if (typeof x === \"string\") {{...}} }}`.",
                if hits == 1 { "" } else { "s" },
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-003**: enforce a structured API error-response convention.
///
/// Catches API route handlers that throw raw errors or return inconsistent
/// shapes. Flags Next.js App-Router route files (`route.ts`/`route.js`) that
/// call `NextResponse.json(...)` without an error-status branch, and Express
/// handlers that `throw` instead of catching. Conservative: only flags when
/// the pattern strongly suggests a missing error path.
#[must_use]
pub fn check_api_error_convention(file_path: &str, content: &str) -> Decision {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Only API route handlers.
    let is_api_route = name == "route.ts"
        || name == "route.js"
        || name.starts_with("route.")
        || content.contains("NextResponse")
        || content.contains("export async function POST")
        || content.contains("export async function GET");
    if !is_api_route {
        return Decision::pass();
    }
    let has_json_response = content.contains(".json(");
    let has_catch = content.contains("catch");
    let has_error_status = content.contains("status: 4")
        || content.contains("status: 5")
        || content.contains(".status(4")
        || content.contains(".status(5");
    // If it returns JSON but never catches and never sets an error status,
    // the error path is missing.
    if has_json_response && !has_catch && !has_error_status {
        return Decision::block(
            "UD-ARCH-003",
            format!(
                "UmaDev: API route missing error response (UD-ARCH-003). \
                 `{file_path}` is an API handler that returns JSON but has no \
                 `catch` block and never sets a 4xx/5xx status. Wrap the handler \
                 body in try/catch and return a structured error on failure: \
                 `catch (e) {{ return NextResponse.json({{ error: \"...\" }}, {{ status: 500 }}) }}`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-004**: ban TypeScript non-null assertion (`!`) operator.
///
/// `x!.foo` silently asserts `x` is non-null — if it ever is null, the app
/// crashes at runtime with no error boundary. Commercial code must use
/// optional chaining (`x?.foo`) or an explicit null check. Flags `)!` (call
/// assertion) and `.!` (property assertion), but allows `!=` (loose not-equal)
/// and logical-not `!`. Runs on `.ts`/`.tsx`.
#[must_use]
pub fn check_non_null_assertion(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "tsx") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        let no_strings = strip_string_literals(line);
        for pat in NON_NULL_PATTERNS {
            if no_strings.contains(pat) {
                hits += 1;
            }
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-004",
            format!(
                "UmaDev: non-null assertion `!` banned (UD-ARCH-004). \
                 `{file_path}` uses `{hits}` non-null assertion{} (`x!`). If the value \
                 IS null at runtime the app crashes with no recovery. Use optional \
                 chaining `x?.prop` or an explicit guard: `if (!x) return;`.",
                if hits == 1 { "" } else { "s" },
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-005**: require a React error boundary in app-root components.
///
/// A production React app MUST wrap its tree in an `ErrorBoundary` so render
/// crashes show a fallback instead of a white screen. Flags the root layout
/// component (`App.tsx`/`layout.tsx`/`_app.tsx`) that renders `<App />` or a
/// `<Provider>` without an `<ErrorBoundary>` / `<ErrorBoundary>` wrapper.
/// Conservative: only checks the known root-layout filenames.
#[must_use]
pub fn check_error_boundary(file_path: &str, content: &str) -> Decision {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Only check root-layout / app-shell files.
    let is_root = matches!(
        name,
        "App.tsx"
            | "App.jsx"
            | "_app.tsx"
            | "_app.jsx"
            | "main.tsx"
            | "main.jsx"
            | "index.tsx"
            | "index.jsx"
    );
    if !is_root {
        return Decision::pass();
    }
    // Must render something (not just re-export).
    if !content.contains("return") && !content.contains("=>") {
        return Decision::pass();
    }
    let has_boundary = content.contains("ErrorBoundary")
        || content.contains("errorElement") // React Router 6
        || content.contains("componentError");
    if !has_boundary {
        return Decision::block(
            "UD-ARCH-005",
            format!(
                "UmaDev: root component missing ErrorBoundary (UD-ARCH-005). \
                 `{file_path}` is an app root that renders without an `<ErrorBoundary>` \
                 wrapper. A render crash shows a blank white screen in production. \
                 Wrap the app tree: `<ErrorBoundary fallback={{<Crash />}}>{{<App />}}</ErrorBoundary>`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-005**: flag malicious / phishing / piracy domains in content.
///
/// When the host researches via WebSearch and then writes findings into code
/// or docs, this catches known-malicious domains (piracy, malware C2, typo-
/// squatting imitations of real package registries). It scans URLs and bare
/// domains in any source file. Conservative: only high-confidence blocklist
/// entries (never a fuzzy heuristic that could false-positive on legitimate
/// text).
#[must_use]
pub fn check_malicious_urls(file_path: &str, content: &str) -> Decision {
    let lower = content.to_ascii_lowercase();
    for entry in MALICIOUS_DOMAINS {
        if lower.contains(entry.domain) {
            return Decision::block(
                "UD-SEC-005",
                format!(
                    "UmaDev: known-malicious domain in content (UD-SEC-005). \
                     `{file_path}` references `{}` — {}. \
                     Do not fetch from or link to this domain. Find a legitimate \
                     alternative (the official package registry, vendor docs, or \
                     a trusted mirror).",
                    entry.domain, entry.reason,
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-ARCH-006**: ban bare `catch` blocks that swallow errors.
///
/// `catch (e) {}` (empty) or `catch { }` silently eats failures — the bug
/// stays invisible in production. Flags catch blocks with an empty body or a
/// body that only has a `console.log`/comment/return (no rethrow, no recovery).
/// Runs on JS/TS source. Conservative: only flags truly empty or log-only
/// catches; a catch that does ANY real work (throw, call a handler, set state)
/// passes.
#[must_use]
pub fn check_bare_catch(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    let lower_path = file_path.to_ascii_lowercase();
    if !matches!(ext.as_str(), "js" | "jsx" | "ts" | "tsx")
        || lower_path.contains("/bin/")
        || lower_path.contains("/scripts/")
    {
        return Decision::pass();
    }
    let mut hits = 0usize;
    // Walk the file as a single string; find each `catch` keyword, then the
    // `{` that opens its block (skipping the `(e)` clause), then capture the
    // balanced body.
    let chars: Vec<char> = strip_string_literals(content).chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if matches_at(&chars, i, "catch") {
            // Find the opening `{` of the catch block (skip past `(e)` / ` (e)`).
            let mut j = i + 5;
            let mut paren_depth = 0i32;
            while j < chars.len() {
                match chars[j] {
                    '(' => paren_depth += 1,
                    ')' => paren_depth -= 1,
                    '{' if paren_depth <= 0 => break,
                    _ => {}
                }
                j += 1;
            }
            if j < chars.len() && chars[j] == '{' {
                // Collect the balanced body from j.
                let (body, end) = collect_balanced(&chars, j);
                if is_bare_body(&body) {
                    hits += 1;
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-006",
            format!(
                "UmaDev: bare/swallowed catch block (UD-ARCH-006). \
                 `{file_path}` has {hits} catch block{} that swallow errors without \
                 recovery. An empty or log-only catch hides bugs in production. \
                 Either rethrow (`throw e`), handle the error, or add a comment \
                 explaining why it's intentionally ignored.",
                if hits == 1 { "" } else { "s" },
            ),
        )
    } else {
        Decision::pass()
    }
}

/// Does `chars` contain the keyword `kw` at position `i` (word boundary)?
fn matches_at(chars: &[char], i: usize, kw: &str) -> bool {
    let kw: Vec<char> = kw.chars().collect();
    if i + kw.len() > chars.len() {
        return false;
    }
    for (k, c) in kw.iter().enumerate() {
        if chars[i + k] != *c {
            return false;
        }
    }
    // Word boundary check: char before must not be alphanumeric.
    if i > 0 {
        let prev = chars[i - 1];
        if prev.is_alphanumeric() || prev == '_' {
            return false;
        }
    }
    true
}

/// Collect the balanced `{ ... }` block starting at `open` (which is `{`).
/// Returns the body text (between braces) and the index after the closing `}`.
fn collect_balanced(chars: &[char], open: usize) -> (String, usize) {
    let mut depth = 0i32;
    let mut body = String::new();
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return (body, i + 1);
                }
            }
            _ => {}
        }
        if depth >= 1 && i != open {
            body.push(chars[i]);
        }
        i += 1;
    }
    (body, i)
}

/// `true` when a catch body is "bare" — empty, comment-only, or just a
/// console.log/bare-return (no real error handling).
fn is_bare_body(body: &str) -> bool {
    let meaningful: Vec<String> = body
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("//")
                && !l.starts_with('*')
                && !l.starts_with("/*")
                && !l.starts_with('}')
                && !l.starts_with('{')
        })
        .collect();
    if meaningful.is_empty() {
        return true; // empty catch
    }
    // If every meaningful line is a console.* or a bare return, it's bare.
    meaningful.iter().all(|l| {
        l.starts_with("console.")
            || l.starts_with("console ")
            || matches!(l.as_str(), "return;" | "return undefined;" | "return null;")
    })
}

/// **UD-ARCH-007**: require input validation on API route handlers.
///
/// A POST/PUT/PATCH handler that reads `req.body`/`request.json()` without
/// validating it (via zod, joi, yup, or a manual guard) is vulnerable to bad
/// data. Flags handlers that parse a body but have no validation call. Runs
/// on API route files (`route.ts`/`route.js` or files with POST/PUT handlers).
#[must_use]
pub fn check_input_validation(file_path: &str, content: &str) -> Decision {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let is_api = name == "route.ts"
        || name == "route.js"
        || name.starts_with("route.")
        || content.contains("NextResponse")
        || (content.contains("export async function POST")
            || content.contains("export async function PUT")
            || content.contains("export async function PATCH"));
    if !is_api {
        return Decision::pass();
    }
    let reads_body = content.contains("req.body")
        || content.contains("req.json()")
        || content.contains("request.json()")
        || content.contains("await request.json")
        || content.contains("await req.json")
        || content.contains("ctx.request.body")
        || content.contains("ctx.req.body");
    let has_validation = content.contains("zod")
        || content.contains("safeParse")
        || content.contains(".parse(")
        || content.contains("joi")
        || content.contains("schema.validate")
        || content.contains("yup")
        || content.contains("assert(")
        || content.contains("typeof ")
        || content.contains("if (!");
    if reads_body && !has_validation {
        return Decision::block(
            "UD-ARCH-007",
            format!(
                "UmaDev: API handler missing input validation (UD-ARCH-007). \
                 `{file_path}` reads the request body without validating it. \
                 Unvalidated input causes crashes, injection, and data corruption. \
                 Validate with a schema: `const parsed = Schema.safeParse(await \
                 request.json()); if (!parsed.success) return NextResponse.json({{error: \
                 parsed.error}}, {{status: 400}});`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-006**: detect npm typosquatting / suspicious package names.
///
/// Flags `package.json` (and import statements) that reference known
/// typosquatted package names — imitations of popular packages planted on the
/// registry to catch typo'd installs (e.g. `lodahs` instead of `lodash`,
/// `reactt` instead of `react`). Uses a curated blocklist + edit-distance
/// heuristic for the top-50 most-typosquatted packages.
#[must_use]
pub fn check_typosquat_packages(file_path: &str, content: &str) -> Decision {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let is_manifest = name == "package.json";
    // Only scan package.json manifests + import lines in source.
    if !is_manifest && !content.contains("from \"") && !content.contains("require(\"") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // 1. Exact-match against known typosquat blocklist.
    for bad in TYPOSQUAT_BLOCKLIST {
        let needle = format!("\"{bad}\"");
        if lower.contains(&needle) {
            return Decision::block(
                "UD-SEC-006",
                format!(
                    "UmaDev: known typosquatted package (UD-SEC-006). \
                     `{file_path}` references `{bad}` — a known imitation of a \
                     popular package, planted to catch typo'd installs. It may \
                     contain malware. Replace with the correct package name. \
                     (If this IS the real name, add it to .umadev/rules.toml \
                     exclusions.)",
                ),
            );
        }
    }
    // 2. Edit-distance heuristic for the top packages: flag any token within
    //    edit distance 1 of a top-50 package that ISN'T the real package.
    if is_manifest {
        for pkg in extract_package_names(content) {
            let pkg_lower = pkg.to_ascii_lowercase();
            for &real in TOP_PACKAGES {
                if pkg_lower == real {
                    continue; // exact match is fine
                }
                if edit_distance(&pkg_lower, real) == 1 && pkg_lower.len() >= 4 {
                    return Decision::block(
                        "UD-SEC-006",
                        format!(
                            "UmaDev: possible typosquat (UD-SEC-006). \
                             `{file_path}` has `{pkg}` which is one character from the \
                             popular package `{real}`. This is a common typosquatting \
                             pattern. Confirm this is the intended package — if it's a \
                             typo, install `{real}` instead.",
                        ),
                    );
                }
            }
        }
    }
    Decision::pass()
}

/// Extract package names from a package.json `dependencies`/`devDependencies` block.
fn extract_package_names(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_deps = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("\"dependencies\"") || trimmed.starts_with("\"devDependencies\"") {
            in_deps = true;
            continue;
        }
        if in_deps {
            if trimmed == "}" || (trimmed.starts_with('}') && !trimmed.contains("\":")) {
                in_deps = false;
                continue;
            }
            // "pkgname": "version"
            if let Some(start) = trimmed.find('"') {
                if let Some(end) = trimmed[start + 1..].find('"') {
                    let name = &trimmed[start + 1..start + 1 + end];
                    if !name.is_empty() && !name.starts_with('/') {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    names
}

/// Levenshtein edit distance (bounded at 2 — we only care about distance 1).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return 2;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// **UD-ARCH-008**: ban `any[]` and loose array/object types in TypeScript.
///
/// Extends UD-ARCH-001 to catch the array variants `any[]`, `Array<any>`,
/// `object[]`, and `{}[]` that the basic `: any` pattern misses. These types
/// defeat the type checker just as `any` does — an element access on them
/// returns `any`. Runs on `.ts`/`.tsx`.
#[must_use]
pub fn check_loose_array_types(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "tsx") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        let no_str = strip_string_literals(line);
        for pat in LOOSE_ARRAY_PATTERNS {
            if no_str.contains(pat) {
                hits += 1;
            }
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-008",
            format!(
                "UmaDev: loose array type banned (UD-ARCH-008). \
                 `{file_path}` uses a loose array/object type ({hits} hit{}) like \
                 `any[]` or `Array<any>` — element access returns `any`, defeating \
                 the type checker. Use a concrete element type: `string[]`, \
                 `User[]`, or `Array<Result<T, E>>`.",
                if hits == 1 { "" } else { "s" },
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-SEC-007**: ban `eval()` and `new Function()` — code injection vectors.
///
/// `eval` and the `Function` constructor execute arbitrary strings as code,
/// opening an XSS / RCE hole if the string is ever user-influenced. Commercial
/// code must parse data (JSON.parse) or use a safe expression evaluator.
/// Flags `eval(`, `Function(`, `new Function`, `setTimeout("..."` (string-arg
/// setTimeout is eval-equivalent). Runs on JS/TS source.
#[must_use]
pub fn check_eval_injection(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "js" | "jsx" | "ts" | "tsx") {
        return Decision::pass();
    }
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        // eval/Function/setTimeout-string are dangerous precisely BECAUSE of
        // the string argument, so scan the RAW line (not string-stripped).
        for pat in EVAL_PATTERNS {
            if line.contains(pat.trigger) {
                hits.push(pat.label);
            }
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-SEC-007",
        format!(
            "UmaDev: eval/Function constructor banned (UD-SEC-007). \
             `{file_path}` uses {} — a code-injection vector. If the argument \
             is ever user-influenced it's an XSS/RCE hole. Parse data with \
             `JSON.parse`, or use a safe sandboxed evaluator. String-arg \
             `setTimeout(\"...\")` is eval-equivalent and also banned.",
            labels.join(" / "),
        ),
    )
}

/// **UD-ARCH-009**: require i18n for hardcoded user-facing strings.
///
/// A commercial product must not hardcode UI text — it needs an i18n layer
/// (react-intl / i18next / formatjs) so strings can be localized. Flags JSX
/// files that contain CJK characters in JSX text nodes or string literals
/// passed to user-facing props (`placeholder`/`label`/`title`/`<button>` text),
/// when no i18n import is present. Conservative: only flags CJK (the clearest
/// "this is a hardcoded UI string" signal) and only when no i18n setup exists.
#[must_use]
pub fn check_i18n_required(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "jsx" | "tsx" | "vue" | "svelte") {
        return Decision::pass();
    }
    // If the file already imports an i18n library, it's set up correctly.
    if content.contains("react-intl")
        || content.contains("i18next")
        || content.contains("useTranslation")
        || content.contains("FormattedMessage")
        || content.contains("@formatjs")
        || content.contains("vue-i18n")
        || content.contains("$t(")
        || content.contains("i18n[")
        || (content.contains("_zh") && content.contains("_en"))
    {
        return Decision::pass();
    }
    // Scan for CJK characters in user-facing contexts (JSX text / string props).
    let has_cjk_ui = content
        .lines()
        .filter(|l| {
            // Skip comment lines.
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with('*') && !t.starts_with("/*")
        })
        .any(|line| {
            // CJK between `>` and `<` (JSX text node) or in a UI prop string.
            (line.contains('>') && line.contains('<') && has_cjk(line)) || has_cjk_in_prop(line)
        });
    if has_cjk_ui {
        return Decision::block(
            "UD-ARCH-009",
            format!(
                "UmaDev: hardcoded UI string without i18n (UD-ARCH-009). \
                 `{file_path}` has CJK user-facing text but no i18n import. A \
                 commercial product must localize UI strings. Wrap text with \
                 `<FormattedMessage>` / `t(\"key\")` from react-intl or i18next, \
                 and move the string to a locale file. (If this file is a test \
                 or demo, disable this clause in .umadev/rules.toml.)",
            ),
        );
    }
    Decision::pass()
}

/// `true` when the line contains a CJK ideograph (Unicode CJK Unified block).
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// `true` when a UI-prop string literal contains CJK (placeholder/label/title).
fn has_cjk_in_prop(line: &str) -> bool {
    for prop in [
        "placeholder=\"",
        "placeholder='",
        "label=\"",
        "label='",
        "title=\"",
        "title='",
    ] {
        if let Some(start) = line.find(prop) {
            let after = &line[start + prop.len()..];
            let end_quote = after.find(if prop.ends_with('"') { '"' } else { '\'' });
            if let Some(end) = end_quote {
                if has_cjk(&after[..end]) {
                    return true;
                }
            }
        }
    }
    false
}

/// **UD-SEC-008**: ban unsafe deserialization.
///
/// `pickle.loads`, `yaml.load` (without `SafeLoader`), `Marshal.load`, and
/// `unserialize` execute arbitrary code from untrusted data — a classic RCE
/// vector. Flags these in Python/Ruby/PHP source. The safe variants
/// (`yaml.safe_load`, `json.loads`) pass.
#[must_use]
pub fn check_unsafe_deserialization(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    let lang = match ext.as_str() {
        "py" => "py",
        "rb" => "rb",
        "php" => "php",
        _ => return Decision::pass(),
    };
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        for pat in DESERIALIZE_PATTERNS {
            if pat.lang == lang && line.contains(pat.trigger) {
                // Allow the safe variant if present in the same line.
                if line.contains(pat.safe_if) {
                    continue;
                }
                hits.push(pat.label);
            }
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-SEC-008",
        format!(
            "UmaDev: unsafe deserialization banned (UD-SEC-008). \
             `{file_path}` uses {} — it can execute arbitrary code from \
             untrusted data (RCE). Use the safe variant: `yaml.safe_load`, \
             `json.loads`, or `Marshal.restore` with a verified schema.",
            labels.join(" / "),
        ),
    )
}

/// **UD-ARCH-010**: require a11y (accessibility) attributes on interactive/
/// visual elements.
///
/// Flags `<img>` without `alt`, `<button>`/`<a>` without an accessible name
/// (aria-label or visible text), and `<input>` without a `label` association.
/// A commercial product must be screen-reader accessible. Runs on JSX/TSX/HTML.
#[must_use]
pub fn check_a11y(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "jsx" | "tsx" | "html" | "vue") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let mut hits: Vec<&str> = Vec::new();

    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find("<img") {
        let start = cursor + relative;
        let boundary = lower.as_bytes().get(start + 4).copied();
        if !matches!(boundary, Some(b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')) {
            cursor = start + 4;
            continue;
        }
        let Some(end_relative) = lower[start..].find('>') else {
            hits.push("<img> missing alt");
            break;
        };
        let end = start + end_relative + 1;
        if !lower[start..end].contains("alt=") {
            hits.push("<img> missing alt");
        }
        cursor = end;
    }

    cursor = 0;
    while let Some(relative) = lower[cursor..].find("<button") {
        let start = cursor + relative;
        let boundary = lower.as_bytes().get(start + 7).copied();
        if !matches!(boundary, Some(b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')) {
            cursor = start + 7;
            continue;
        }
        let Some(open_end_relative) = lower[start..].find('>') else {
            hits.push("<button> missing accessible name");
            break;
        };
        let open_end = start + open_end_relative + 1;
        let opening = &lower[start..open_end];
        let named_by_attribute =
            opening.contains("aria-label") || opening.contains("aria-labelledby");
        let close = lower[open_end..]
            .find("</button>")
            .map(|relative| open_end + relative);
        let named_by_body =
            close.is_some_and(|end| has_accessible_button_body(&content[open_end..end]));
        if !named_by_attribute && !named_by_body {
            hits.push("<button> missing accessible name");
        }
        cursor = close.map_or(open_end, |end| end + "</button>".len());
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-ARCH-010",
        format!(
            "UmaDev: accessibility (a11y) violation (UD-ARCH-010). \
             `{file_path}` has {} — a commercial product must be screen-reader \
             accessible. Add `alt=\"description\"` to images, and ensure \
             buttons have visible text or `aria-label`.",
            labels.join(" / "),
        ),
    )
}

fn has_accessible_button_body(body: &str) -> bool {
    let mut inside_tag = false;
    for ch in body.chars() {
        match ch {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            '{' if !inside_tag => return true,
            _ if !inside_tag && ch.is_alphanumeric() => return true,
            _ => {}
        }
    }
    false
}

/// **UG-LINT-001**: ban inline styles (`style={{...}}` / `style="..."`).
///
/// Inline styles bypass the design system (CSS tokens / Tailwind classes) and
/// make theming impossible. Commercial code must use semantic classes or CSS
/// modules. Flags `style={{` and `style="` in JSX/HTML. Runs on JSX/TSX/HTML.
#[must_use]
pub fn check_inline_styles(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "jsx" | "tsx" | "html" | "vue") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        if line.contains("style={{") || line.contains("style=\"") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UG-LINT-001",
            format!(
                "UmaDev: inline style banned (UG-LINT-001). \
                 `{file_path}` uses inline styles ({hits} hit{}) — they bypass the \
                 design system and make theming impossible. Use a CSS class \
                 (`className=\"btn\"`), CSS module, or a Tailwind utility instead. \
                 Move one-off values to a `.css` file referencing design tokens.",
                if hits == 1 { "" } else { "s" },
            ),
        )
    } else {
        Decision::pass()
    }
}

/// One unsafe-deserialization pattern, keyed by language.
struct DeserializePattern {
    lang: &'static str,
    trigger: &'static str,
    safe_if: &'static str,
    label: &'static str,
}

/// **UD-SEC-009**: block SSRF — requests to internal/private/metadata addresses.
///
/// When a server-side handler fetches a URL derived from user input, it must
/// never reach internal IPs (`10.x`, `172.16-31.x`, `192.168.x`, `127.x`,
/// `169.254.169.254` cloud metadata) — that's an SSRF vector. Flags `fetch(`
/// / `axios.get` / `requests.get` with a URL variable (not a hardcoded public
/// host) in backend code. Conservative: only flags when the fetch target
/// looks dynamic (`${...}` template, variable, or concatenation) AND no
/// allowlist guard is present.
#[must_use]
pub fn check_ssrf(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go") {
        return Decision::pass();
    }
    // Must be backend code (not frontend — fetch from browser is different).
    if matches!(ext.as_str(), "jsx" | "tsx" | "vue" | "svelte") {
        return Decision::pass();
    }
    // Does the file make outbound requests?
    let makes_request = content.contains("fetch(")
        || content.contains("axios")
        || content.contains("requests.get")
        || content.contains("requests.post")
        || content.contains("http.Get")
        || content.contains("reqwest");
    if !makes_request {
        return Decision::pass();
    }
    // Is the URL dynamic (user-influenced)?
    let dynamic_url = content.contains("${")
        || content.contains("url +")
        || content.contains("+ path")
        || content.contains("targetUrl")
        || content.contains("userUrl")
        || content.contains("callbackUrl");
    // Is there an allowlist / validation guard?
    let has_guard = content.contains("allowlist")
        || content.contains("allowList")
        || content.contains("allowedHosts")
        || content.contains("isPublicIp")
        || content.contains("validateUrl")
        || content.contains("blockPrivate")
        || content.contains("169.254.169.254"); // explicit metadata block
    if dynamic_url && !has_guard {
        return Decision::block(
            "UD-SEC-009",
            format!(
                "UmaDev: potential SSRF (UD-SEC-009). \
                 `{file_path}` makes an outbound request with a dynamic URL but has \
                 no allowlist or private-IP guard. An attacker can target internal \
                 services or the cloud metadata endpoint (169.254.169.254). \
                 Validate the URL against an allowlist of public hosts, and reject \
                 private/loopback/link-local addresses before fetching.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-011**: require rate limiting on public API endpoints.
///
/// A public API without rate limiting is a DoS/abuse vector. Flags API route
/// files (route.ts/route.js or files with GET/POST handlers) that have no
/// rate-limiter present (`rateLimit`, `ratelimit`, `@upstash/ratelimit`,
/// `express-rate-limit`, a middleware reference, or a Redis-based limiter).
#[must_use]
pub fn check_rate_limiting(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    // These endpoint shapes are JavaScript/TypeScript framework syntax.
    // Matching `app.post(` inside a Rust prompt is not an API endpoint.
    if !matches!(ext.as_str(), "ts" | "js" | "mjs" | "cjs") {
        return Decision::pass();
    }
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let is_api = name == "route.ts"
        || name == "route.js"
        || name.starts_with("route.")
        || content.contains("NextResponse")
        || content.contains("export async function GET")
        || content.contains("app.get(")
        || content.contains("app.post(");
    if !is_api {
        return Decision::pass();
    }
    let has_limiter = content.contains("rateLimit")
        || content.contains("ratelimit")
        || content.contains("rate-limit")
        || content.contains("RateLimiter")
        || content.contains("throttle")
        || content.contains("upstash")
        || content.contains("express-rate-limit")
        || content.contains("@upstash/ratelimit")
        || content.contains("tooManyRequests")
        || content.contains("429");
    if !has_limiter {
        return Decision::block(
            "UD-ARCH-011",
            format!(
                "UmaDev: API endpoint missing rate limiting (UD-ARCH-011). \
                 `{file_path}` is a public API handler with no rate limiter. \
                 Without it, a single client can DoS the endpoint or abuse it. \
                 Add a limiter: `@upstash/ratelimit` (edge), `express-rate-limit` \
                 (Node), or a Redis token-bucket. Return HTTP 429 when exceeded.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-012**: require structured logging (no bare `console.log` as log).
///
/// In production, logs must be structured (JSON key-value) so they're
/// searchable/aggregatable — `console.log(\"user did x\")` is debug residue
/// (caught by UD-ARCH-002), but even `console.log(\`user ${id} action\`)` as
/// a "log" is unstructured. Flags backend files that use console.* for
/// application logging instead of a structured logger (pino, winston,
/// structlog, tracing). Runs on backend `.ts`/`.js`/`.py`/`.rs`.
#[must_use]
pub fn check_structured_logging(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if looks_like_secret_test_path(file_path) {
        return Decision::pass();
    }
    let lower_path = file_path.to_ascii_lowercase().replace('\\', "/");
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let backend_path = lower_path.starts_with("server/")
        || lower_path.starts_with("api/")
        || lower_path.starts_with("backend/")
        || lower_path.contains("/server/")
        || lower_path.contains("/api/")
        || lower_path.contains("/backend/")
        || name.starts_with("server.");
    let node_server = matches!(ext.as_str(), "ts" | "js" | "mjs" | "cjs")
        && (backend_path
            || content.contains("app.listen(")
            || content.contains("createServer(")
            || content.contains("from 'express'")
            || content.contains("from \"express\""));
    let python_server = ext == "py"
        && (backend_path
            || content.contains("Flask(")
            || content.contains("FastAPI(")
            || content.contains("django."));
    if !node_server && !python_server {
        return Decision::pass();
    }
    // Must use console.* or print() as logging (not already structured).
    let uses_console_log = content.contains("console.log")
        || content.contains("console.info")
        || content.contains("console.error")
        || content.contains("console.warn");
    let uses_print_log = python_server && content.contains("print(");
    if !uses_console_log && !uses_print_log {
        return Decision::pass();
    }
    // Is a structured logger already imported?
    let has_logger = content.contains("pino")
        || content.contains("winston")
        || content.contains("bunyan")
        || content.contains("loglevel")
        || content.contains("structlog")
        || content.contains("logging.getLogger")
        || content.contains("log.Printf")
        || content.contains("tracing::")
        || content.contains("log::")
        || content.contains("Logger")
        || content.contains("logger");
    if !has_logger {
        return Decision::block(
            "UD-ARCH-012",
            format!(
                "UmaDev: unstructured logging (UD-ARCH-012). \
                 `{file_path}` uses console.*/print() for application logging but \
                 has no structured logger. Production logs must be JSON key-value \
                 for searchability. Use `pino`/`winston` (Node), `structlog` (Python), \
                 or `tracing` (Rust): `logger.info({{ event: \"login\", userId: id }})`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-010**: block insecure CORS configuration (`origin: "*"`, `*` in
/// Access-Control-Allow-Origin, `cors({ origin: "*" })`).
///
/// A wildcard CORS policy lets ANY website make authenticated requests to
/// your API — a credential-leak vector. Flags `*` in CORS config across Node
/// (express/cors), Python (Flask-CORS), and Next.js. Runs on backend source.
#[must_use]
pub fn check_insecure_cors(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Backend config or server files.
    let is_backend = matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go")
        || name == "next.config.js"
        || name == "next.config.ts"
        || name == "next.config.mjs";
    if !is_backend {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Patterns that set CORS origin to wildcard.
    let wildcard_cors = [
        "access-control-allow-origin: *",
        "access-control-allow-origin:*",
        "origin: \"*\"",
        "origin: '*'",
        "origin: \"*\",",
        "cors({ origin: \"*\" })",
        "cors({origin:\"*\"})",
        "cors({ origin: '*' })",
        "origins: [\"*\"]",
        "resources: { \"*\" }",
        "allow_all_origins",
        "allow_origins=*",
    ];
    for pat in wildcard_cors {
        if lower.contains(pat) {
            return Decision::block(
                "UD-SEC-010",
                format!(
                    "UmaDev: insecure CORS wildcard (UD-SEC-010). \
                     `{file_path}` sets CORS origin to `*` — this lets ANY website \
                     make authenticated requests to your API. Specify an explicit \
                     allowlist of origins: `cors({{ origin: [\"https://app.com\"] }})`.",
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-ARCH-013**: require Content-Security-Policy header on web responses.
///
/// A web app without CSP is vulnerable to XSS injection. Flags HTML files
/// and server files that serve HTML but set no CSP header (or `<meta>` CSP
/// tag). Conservative: only flags when the file clearly serves HTML (contains
/// `<html>` or `text/html`) but has no CSP.
#[must_use]
pub fn check_csp_required(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let lower = content.to_ascii_lowercase();
    let is_static_document =
        ext == "html" || name == "_document.tsx" || name == "_document.jsx" || name == "index.html";
    let is_server_html = matches!(ext.as_str(), "ts" | "js")
        && (lower.contains("text/html")
            || ((lower.contains("res.send") || lower.contains("response("))
                && lower.contains("<html")));
    let is_web = is_static_document || is_server_html;
    if !is_web {
        return Decision::pass();
    }
    let has_csp = lower.contains("content-security-policy")
        || lower.contains("http-equiv=\"content-security-policy\"")
        || lower.contains("csp()");
    if !has_csp {
        return Decision::block(
            "UD-ARCH-013",
            format!(
                "UmaDev: missing Content-Security-Policy (UD-ARCH-013). \
                 `{file_path}` serves HTML but sets no CSP header — the app is \
                 vulnerable to XSS injection. Add a CSP header: in a `<meta>` tag \
                 (`<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'self'\">`) \
                 or as a response header on the server.",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-002**: ban magic numbers in source logic.
///
/// Magic numbers (`if (status === 404)`, `setTimeout(fn, 86400000)`) make code
/// unreadable and bug-prone. They must be named constants. Flags bare numeric
/// literals in comparison/assignment contexts (not array indices, not 0/1).
/// Conservative: only flags numbers ≥ 2 digits that appear in `===`/`==`/`!==`
/// comparisons or as function-call arguments, excluding well-known HTTP codes
/// and test files. Runs on JS/TS/Python.
#[must_use]
pub fn check_magic_numbers(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py") {
        return Decision::pass();
    }
    // Skip test files — magic numbers are normal in tests.
    if file_path.contains(".test.") || file_path.contains(".spec.") || file_path.contains("test_") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }
        let no_str = strip_string_literals(line);
        // Look for `=== <number>` / `== <number>` / `!== <number>` comparisons
        // with a 2+ digit number that isn't a well-known HTTP status.
        for op in ["=== ", "== ", "!== ", "===\t"] {
            if let Some(idx) = no_str.find(op) {
                let after = &no_str[idx + op.len()..];
                if let Some(num) = extract_leading_number(after) {
                    if num >= 10 && !WELL_KNOWN_NUMBERS.contains(&num) {
                        hits += 1;
                    }
                }
            }
        }
    }
    if hits > 3 {
        Decision::block(
            "UG-LINT-002",
            format!(
                "UmaDev: too many magic numbers (UG-LINT-002). \
                 `{file_path}` has {hits} numeric literals in comparison contexts. \
                 Magic numbers make code unreadable — extract them to named \
                 constants: `const NOT_FOUND = 404; if (status === NOT_FOUND)`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// Extract a leading integer from a string slice (for magic-number detection).
fn extract_leading_number(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Well-known numbers that don't count as "magic" — universally recognized
/// domain constants whose meaning is obvious at the comparison site, so
/// flagging them is a false positive. Covers:
/// - HTTP status codes (`200`/`404`/`500` …),
/// - common ages / thresholds (`13`/`16`/`18`/`21`/`65`),
/// - percentages and round bases (`50`/`100`/`1000`),
/// - powers of two used for sizes (`16`/`32`/`64`/`128`/`255`/`256`/`512`/`1024`/`2048`/`4096`),
/// - common time/port constants (`12`/`24`/`60`/`3600`/`8080`/`3000`).
///
/// Conservative by design: only numbers with a single obvious real-world
/// meaning. Anything outside this set still counts toward the magic-number
/// budget.
const WELL_KNOWN_NUMBERS: &[u64] = &[
    // HTTP status codes
    100, 101, 200, 201, 202, 204, 301, 302, 304, 400, 401, 403, 404, 405, 409, 410, 422, 429, 500,
    502, 503, 504, // common ages / legal thresholds
    13, 16, 18, 21, 65, // percentages / round decimal bases
    50, 1000, 10000, // powers of two (sizes, masks, buffer lengths)
    16, 32, 64, 128, 255, 256, 512, 1024, 2048, 4096, 8192,
    // common time units & well-known ports
    12, 24, 60, 90, 360, 365, 3600, 86400, 3000, 5000, 8000, 8080,
];

/// **UD-ARCH-014** (Python): ban bare `except:` clauses.
///
/// `except:` (no exception type) catches EVERYTHING — including SystemExit,
/// KeyboardInterrupt, and bugs you meant to propagate. Must catch a specific
/// exception (`except ValueError:`) or at least `except Exception:`. Runs on
/// `.py` files.
#[must_use]
pub fn check_python_bare_except(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "py" {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        // `except:` with a bare colon (no exception type).
        if trimmed == "except:" || trimmed.starts_with("except:") {
            // Check it's truly bare (no exception name after the colon-free part).
            let after = trimmed.strip_prefix("except").unwrap_or("");
            if after.starts_with(':') && !after.contains(" as ") {
                // `except:` or `except: # comment` — bare.
                hits += 1;
            }
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-014",
            format!(
                "UmaDev: bare except clause banned (UD-ARCH-014). \
                 `{file_path}` has {hits} bare `except:` — it catches EVERYTHING \
                 (including KeyboardInterrupt and SystemExit). Catch a specific \
                 exception: `except ValueError:` or at least `except Exception:`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-015** (Python): ban `global` statements.
///
/// `global` mutable state makes functions impure, untestable, and race-prone.
/// Commercial code should pass state explicitly or use a class. Flags `global`
/// keyword usage. Runs on `.py` files.
#[must_use]
pub fn check_python_global(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "py" {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        // `global varname` — the global keyword (not the word in a string/comment).
        if trimmed.starts_with("global ") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-015",
            format!(
                "UmaDev: `global` keyword banned (UD-ARCH-015). \
                 `{file_path}` uses `global` ({hits} time{}) — global mutable state \
                 makes functions impure, untestable, and race-prone. Pass state \
                 explicitly as a parameter, or encapsulate it in a class.",
                if hits == 1 { "" } else { "s" },
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-SEC-011**: ban SQL injection — string-concatenated SQL queries.
///
/// Building SQL by concatenating user input (`"SELECT ... WHERE id = " + id`,
/// `f"SELECT ... {user_id}"`) is the #1 injection vector. Must use
/// parameterized queries (`?` placeholders / prepared statements). Flags SQL
/// keywords next to string interpolation/concatenation in backend code.
/// Runs on JS/TS/Python/Ruby.
fn sql_query_shape_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?is)\b(?:select\s+(?:\*|[a-z_][a-z0-9_."`\[\]]*)\s+from|insert\s+into\s+[a-z_"`\[]|update\s+[a-z_"`\[][^;\n]{0,240}\s+set\s+|delete\s+from\s+[a-z_"`\[])"#,
        )
        .expect("SQL query-shape regex is well-formed")
    })
}

fn dynamic_sql_fragment(fragment: &str) -> bool {
    let lower = fragment.to_ascii_lowercase();
    if !sql_query_shape_regex().is_match(&lower) {
        return false;
    }
    let dynamic = lower.contains("${")
        || lower.contains("f\"")
        || lower.contains("f'")
        || lower.contains("\" +")
        || lower.contains("' +")
        || lower.contains("+ \"")
        || lower.contains("+ '")
        || lower.contains(".format(")
        || lower.contains("% (")
        || lower.contains("%s");
    let parameterized = lower.contains("execute(")
        && (lower.contains("?,")
            || lower.contains("? ")
            || lower.contains("?)")
            || lower.contains("$1")
            || lower.contains(":id")
            || lower.contains("params")
            || lower.contains("args"));
    dynamic && !parameterized
}

/// Check an executable source file for a dynamically constructed SQL query.
#[must_use]
pub fn check_sql_injection(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb") {
        return Decision::pass();
    }
    let tokenized = crate::tokenizer::Tokenized::new(content);
    let source = tokenized.without_comments(content);
    let lines = source.lines().collect::<Vec<_>>();
    // SQL text and its construction signal must share a bounded expression
    // neighborhood. Whole-file co-occurrence made release notes containing the
    // words "update" and "+" look like executable SQL.
    let unsafe_query = (0..lines.len()).any(|start| {
        let end = (start + 3).min(lines.len());
        dynamic_sql_fragment(&lines[start..end].join("\n"))
    });
    if unsafe_query {
        return Decision::block(
            "UD-SEC-011",
            format!(
                "UmaDev: potential SQL injection (UD-SEC-011). \
                 `{file_path}` builds SQL with string interpolation/concatenation — \
                 user input can break out of the query. Use parameterized queries: \
                 `db.query(\"SELECT * FROM users WHERE id = ?\", [userId])`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-016**: require HTTPS redirect on web servers.
///
/// A production web server must redirect HTTP → HTTPS so traffic is encrypted.
/// Flags server config files (next.config, express, nginx) that serve HTTP
/// without a redirect to HTTPS. Conservative: only flags when the file clearly
/// configures a server AND has no `https`/`redirect`/`forceSsl` mention.
#[must_use]
pub fn check_https_redirect(file_path: &str, content: &str) -> Decision {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let ext = extension_of(file_path);
    let is_server_config = name == "nginx.conf"
        || name == "server.ts"
        || name == "server.js"
        || name.starts_with("middleware.")
        || (matches!(ext.as_str(), "ts" | "js")
            && (content.contains("app.listen")
                || content.contains("createServer")
                || content.contains("app.get(")));
    if !is_server_config {
        return Decision::pass();
    }
    let has_https = content.contains("https")
        || content.contains("forceSsl")
        || content.contains("forceSSL")
        || content.contains("redirect")
        || content.contains("HSTS")
        || content.contains("Strict-Transport-Security");
    if !has_https {
        return Decision::block(
            "UD-ARCH-016",
            format!(
                "UmaDev: server missing HTTPS redirect (UD-ARCH-016). \
                 `{file_path}` configures a web server but has no HTTPS redirect. \
                 Production traffic must be encrypted. Add a redirect: \
                 `if (req.headers['x-forwarded-proto'] !== 'https') return res.redirect(301, 'https://...')`, \
                 or set `forceSsl: true` / HSTS header.",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-015**: ban leftover TODO/FIXME/HACK/XXX comments.
///
/// TODO/FIXME in shipped code indicates incomplete work. Flags these markers
/// in source files (not test files — TODOs are normal in tests). Conservative:
/// only flags when there are more than 2, to avoid blocking a single legitimate
/// note. Runs on all source extensions.
#[must_use]
pub fn check_todo_residue(file_path: &str, content: &str) -> Decision {
    // Skip test files — TODOs are normal there.
    if looks_like_secret_test_path(file_path) {
        return Decision::pass();
    }
    let content = if extension_of(file_path) == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let mut hits = 0usize;
    for line in content.lines() {
        // Must be in a comment context (// # /* *) not a string or code.
        let trimmed = line.trim_start();
        let documentation_comment = trimmed.starts_with("///")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("/**")
            || trimmed.starts_with("/*!");
        let ordinary_comment = trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with('*')
            || trimmed.starts_with("/*")
            || trimmed.starts_with("<!--");
        if !documentation_comment && ordinary_comment && comment_has_debt_marker(trimmed) {
            hits += 1;
        }
    }
    if hits > 2 {
        Decision::block(
            "UG-LINT-015",
            format!(
                "UmaDev: too many TODO/FIXME comments (UG-LINT-015). \
                 `{file_path}` has {hits} TODO/FIXME/HACK/XXX markers. \
                 Incomplete work in shipped code causes bugs. Resolve the markers \
                 or track them in your issue tracker instead of leaving them in source.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// Recognize a work marker in an ordinary comment without treating prose that
/// merely documents `` `TODO` `` or identifiers such as `todos` as debt.
fn comment_has_debt_marker(comment: &str) -> bool {
    let mut visible = String::with_capacity(comment.len());
    let mut in_inline_code = false;
    for ch in comment.chars() {
        if ch == '`' {
            in_inline_code = !in_inline_code;
            visible.push(' ');
        } else if in_inline_code {
            visible.push(' ');
        } else {
            visible.push(ch.to_ascii_lowercase());
        }
    }

    ["todo", "fixme", "hack", "xxx"].iter().any(|marker| {
        visible.match_indices(marker).any(|(start, matched)| {
            let end = start + matched.len();
            let before = visible[..start].chars().next_back();
            let after = visible[end..].chars().next();
            let starts_token = before.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
            let ends_marker = after.is_none_or(|c| c.is_ascii_whitespace() || c == ':');
            starts_token && ends_marker
        })
    })
}

/// **UD-ARCH-017** (Rust): ban `unwrap()` / `expect()` in non-test code.
///
/// `.unwrap()` panics on `None`/`Err` — in production this crashes the process.
/// Must use `?`, `unwrap_or`, `unwrap_or_else`, or match. Flags `unwrap()` and
/// `expect(` in `.rs` files (excluding tests). Runs on Rust source.
#[must_use]
pub fn check_rust_unwrap(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "rs" {
        return Decision::pass();
    }
    // Skip test files and build scripts.
    if looks_like_secret_test_path(file_path) || file_path.ends_with("build.rs") {
        return Decision::pass();
    }
    let content = rust_shipping_prefix(content);
    let lines: Vec<&str> = content.lines().collect();
    let mut hits = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains(".unwrap()")
            || (no_str.contains(".expect(") && !literal_regex_expect(&lines, index))
        {
            hits += 1;
        }
    }
    if hits > 2 {
        Decision::block(
            "UD-ARCH-017",
            format!(
                "UmaDev: too many unwrap()/expect() in Rust (UD-ARCH-017). \
                 `{file_path}` has {hits} unwrap/expect calls — they panic on \
                 None/Err and crash the process in production. Use `?` for \
                 error propagation, `unwrap_or(default)` for fallbacks, or \
                 `match` for explicit handling.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// A literal regular expression is static program data: if its syntax is bad,
/// startup cannot continue and tests catch the programmer error. Treating the
/// conventional `OnceLock + Regex::new(literal).expect(...)` construction as a
/// runtime unwrap defect floods every parser module while finding no recoverable
/// failure. Dynamic patterns remain governed.
fn literal_regex_expect(lines: &[&str], index: usize) -> bool {
    let start = index.saturating_sub(64);
    let compact: String = lines[start..=index]
        .iter()
        .flat_map(|line| line.chars())
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let Some(regex_start) = compact.rfind("Regex::new(") else {
        return false;
    };
    let argument = &compact[regex_start + "Regex::new(".len()..];
    argument.starts_with('"')
        || argument.starts_with("r\"")
        || argument.starts_with("r#")
        || argument.starts_with("concat!(")
}

/// **UD-ARCH-018** (Go): ban `panic()` in non-test Go code.
///
/// `panic()` crashes the goroutine with no recovery. Production Go must return
/// errors. Flags `panic(` in `.go` files (excluding tests and main's init).
#[must_use]
pub fn check_go_panic(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "go" {
        return Decision::pass();
    }
    if file_path.ends_with("_test.go") || file_path.ends_with("main.go") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains("panic(") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-018",
            format!(
                "UmaDev: panic() in Go code (UD-ARCH-018). \
                 `{file_path}` uses `panic()` ({hits} time{}) — it crashes the \
                 goroutine. Production Go must return errors: \
                 `if err != nil {{ return err }}`.",
                if hits == 1 { "" } else { "s" },
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-SEC-012**: ban XPath injection — string-concatenated XPath queries.
///
/// Like SQL injection, building XPath with string interpolation (`//user[@id='${id}']`)
/// lets an attacker break out of the query. Must use parameterized XPath variables.
/// Flags XPath construction with `${}` / `+` / `format()` in backend code.
#[must_use]
pub fn check_xpath_injection(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "java" | "kt") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_xpath = lower.contains("xpath")
        || lower.contains("//user[")
        || lower.contains("//node[")
        || lower.contains("//item[");
    if !has_xpath {
        return Decision::pass();
    }
    let dynamic = lower.contains("${")
        || lower.contains("\" + ")
        || lower.contains("'+")
        || lower.contains(".format(")
        || lower.contains("f\"");
    if dynamic {
        return Decision::block(
            "UD-SEC-012",
            format!(
                "UmaDev: potential XPath injection (UD-SEC-012). \
                 `{file_path}` builds an XPath query with string interpolation — \
                 user input can break out of the expression. Use XPath variables \
                 or a safe evaluator that escapes input, never concatenation.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-019**: require security headers (helmet or equivalent) on web servers.
///
/// A Node/Express server must set security headers (X-Content-Type-Options,
/// X-Frame-Options, Strict-Transport-Security). Flags `app.listen` /
/// `createServer` without `helmet` or manual security-header setup.
#[must_use]
pub fn check_security_headers(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js") {
        return Decision::pass();
    }
    let is_server = content.contains("app.listen")
        || content.contains("createServer")
        || content.contains("app.use(");
    if !is_server {
        return Decision::pass();
    }
    let has_headers = content.contains("helmet")
        || content.contains("X-Content-Type-Options")
        || content.contains("X-Frame-Options")
        || content.contains("Strict-Transport-Security")
        || content.contains("X-XSS-Protection");
    if !has_headers {
        return Decision::block(
            "UD-ARCH-019",
            format!(
                "UmaDev: server missing security headers (UD-ARCH-019). \
                 `{file_path}` starts a web server but sets no security headers. \
                 Add `app.use(helmet())` (Express) or manually set \
                 X-Content-Type-Options, X-Frame-Options, and HSTS.",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-003**: detect unused variables (conservative heuristic).
///
/// Flags `const`/`let` declarations whose name never appears again in the file.
/// This is a heuristic (can't see cross-file usage) so it's conservative: only
/// flags when the variable name is unique enough to be confident it's unused,
/// and skips `_`-prefixed (intentionally unused) and ALL_CAPS constants.
/// Runs on JS/TS.
#[must_use]
pub fn check_unused_variables(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js") {
        return Decision::pass();
    }
    if file_path.contains(".test.") || file_path.contains(".spec.") {
        return Decision::pass();
    }
    let mut hits = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        // Match `const name =` or `let name =` (not destructuring, not `_`-prefixed).
        for kw in ["const ", "let "] {
            if let Some(rest) = trimmed.strip_prefix(kw) {
                let rest = rest.trim_start();
                // Extract the variable name (up to = , ; or :).
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() || name.starts_with('_') || name.chars().all(char::is_uppercase)
                {
                    continue; // intentionally unused or a constant
                }
                // Count occurrences of the name in the whole file (excluding the decl line).
                let count = content.matches(&name).count().saturating_sub(1);
                if count == 0 {
                    hits.push(name);
                }
            }
        }
    }
    if hits.len() > 2 {
        Decision::block(
            "UG-LINT-003",
            format!(
                "UmaDev: unused variables detected (UG-LINT-003). \
                 `{file_path}` declares variables that are never referenced: {}. \
                 Remove dead code, or prefix with `_` if intentionally unused.",
                hits.iter().take(5).cloned().collect::<Vec<_>>().join(", "),
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-020** (Java): ban `System.exit()` outside `main`.
///
/// `System.exit()` kills the JVM — in a library or service handler this
/// terminates the whole process with no cleanup. Must throw or return an
/// error. Flags `System.exit` in `.java` files (excluding the main class).
#[must_use]
pub fn check_java_system_exit(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "java" {
        return Decision::pass();
    }
    // Allow in a Main class.
    if content.contains("public static void main") && content.matches("System.exit").count() <= 1 {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains("System.exit") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-020",
            format!(
                "UmaDev: System.exit() in Java code (UD-ARCH-020). \
                 `{file_path}` calls System.exit() — it kills the JVM with no cleanup. \
                 Throw an exception or return an error status instead; only `main` \
                 should call System.exit().",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-021** (Swift): ban force-unwrap (`!`) on optionals.
///
/// `optional!` crashes the app if the value is nil — like Rust's `unwrap()`.
/// Must use `if let`, `guard let`, or `??` (nil-coalescing). Flags `!` after
/// a variable/paren in `.swift` files (excluding `!=`).
#[must_use]
pub fn check_swift_force_unwrap(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "swift" {
        return Decision::pass();
    }
    if file_path.contains("Test") || file_path.contains("test") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let no_str = strip_string_literals(line);
        // Force-unwrap patterns: `)!`, `name!` (end of line), `!.` (chained).
        // Exclude `!=` (inequality) and `!` as logical-not (leading).
        let has_force = no_str.contains(")!")
            || no_str.contains("!.")
            || no_str.ends_with('!')
            || no_str.contains("!;");
        if has_force && !no_str.contains("!=") {
            hits += 1;
        }
    }
    if hits > 2 {
        Decision::block(
            "UD-ARCH-021",
            format!(
                "UmaDev: force-unwrap in Swift (UD-ARCH-021). \
                 `{file_path}` has {hits} force-unwrap (`!`) calls — they crash if \
                 the optional is nil. Use `if let`, `guard let`, or `??` (nil-coalescing) \
                 instead.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-SEC-013**: ban XXE — XML External Entity injection.
///
/// XML parsers that resolve external entities (`<!ENTITY x SYSTEM "file:...">`)
/// can read local files or make SSRF requests. Flags XML parsing without
/// `noent`/`disallow-doctype-decl`/`XML_PARSE_NOENT` disable. Also flags raw
/// `DOCTYPE` in user-supplied XML strings. Runs on backend source.
#[must_use]
pub fn check_xxe(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "java" | "rb" | "php") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must involve XML parsing.
    let has_xml = lower.contains("xml")
        || lower.contains("domparser")
        || lower.contains("sax")
        || lower.contains("lxml")
        || lower.contains("documentbuilder");
    if !has_xml {
        return Decision::pass();
    }
    // XXE-vulnerable patterns: external entity declaration, or parsing without
    // disabling external entities.
    let has_entity = lower.contains("<!entity")
        || lower.contains("system \"file:")
        || lower.contains("system \"http:");
    let has_disabler = lower.contains("disallow-doctype-decl")
        || lower.contains("disallow_doctype_decl")
        || lower.contains("xml_parse_noent")
        || lower.contains("setfeature(")
        || lower.contains("external-general-entities")
        || lower.contains("resolveexternals")
        || lower.contains("noent")
        || lower.contains("xml.setfeature");
    if has_entity || (has_xml && !has_disabler && lower.contains("doctype")) {
        return Decision::block(
            "UD-SEC-013",
            format!(
                "UmaDev: potential XXE injection (UD-SEC-013). \
                 `{file_path}` parses XML that may resolve external entities — \
                 an attacker can read local files or trigger SSRF. Disable \
                 external entities: `factory.setFeature(\"http://apache.org/xml/features/disallow-doctype-decl\", true)` \
                 (Java), `XML_PARSE_NOENT` flag off (C/lxml), or use a SAX \
                 parser with external entities disabled.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-022**: require HSTS (Strict-Transport-Security) header.
///
/// Distinct from UD-ARCH-016 (HTTPS redirect): HSTS tells the browser to
/// ALWAYS use HTTPS for this domain, preventing SSL-strip attacks. Flags web
/// servers that have `https` configured but no HSTS header. Conservative: only
/// flags when the server clearly handles HTTPS but omits HSTS.
#[must_use]
pub fn check_hsts_header(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let is_server = matches!(ext.as_str(), "ts" | "js")
        && (content.contains("app.listen")
            || content.contains("createServer")
            || content.contains("app.use"));
    if !is_server && name != "next.config.js" && name != "next.config.ts" {
        return Decision::pass();
    }
    // Must be HTTPS-aware to require HSTS.
    let has_https = content.contains("https")
        || content.contains("forceSsl")
        || content.contains("redirect")
        || content.contains("helmet");
    if !has_https {
        return Decision::pass();
    }
    let has_hsts = content.contains("Strict-Transport-Security")
        || content.contains("hsts")
        || content.contains("HSTS")
        || content.contains("maxAge")
        || content.contains("max-age");
    if !has_hsts {
        return Decision::block(
            "UD-ARCH-022",
            format!(
                "UmaDev: server missing HSTS header (UD-ARCH-022). \
                 `{file_path}` serves HTTPS but sets no Strict-Transport-Security \
                 header — without HSTS, a man-in-the-middle can SSL-strip the \
                 connection on the first request. Add: \
                 `res.setHeader('Strict-Transport-Security', 'max-age=31536000; includeSubDomains')` \
                 or enable `hsts` in helmet().",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-023** (PHP): ban `exec`/`shell_exec`/`system`/`passthru`.
///
/// These PHP functions execute shell commands — if the argument is
/// user-influenced it's a command-injection vector. Must use `escapeshellarg`
/// or avoid shell calls entirely. Flags raw shell-exec calls in `.php`.
#[must_use]
pub fn check_php_shell_exec(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "php" {
        return Decision::pass();
    }
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        let no_str = strip_string_literals(line);
        for pat in PHP_SHELL_FUNCS {
            if no_str.contains(pat)
                && !no_str.contains("escapeshellarg")
                && !no_str.contains("escapeshellcmd")
            {
                hits.push(pat);
            }
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-ARCH-023",
        format!(
            "UmaDev: PHP shell-exec function (UD-ARCH-023). \
             `{file_path}` uses {} — if the argument is user-influenced it's a \
             command-injection vector. Wrap input with `escapeshellarg()` or \
             avoid shell calls; use a library that does the operation natively.",
            labels.join(" / "),
        ),
    )
}

/// PHP shell-execution functions to flag.
const PHP_SHELL_FUNCS: &[&str] = &[
    "exec(",
    "shell_exec(",
    "system(",
    "passthru(",
    "popen(",
    "proc_open(",
];

/// **UD-ARCH-024** (Kotlin): ban `!!` (non-null assertion).
///
/// `x!!` in Kotlin throws NullPointerException if x is null — same as Swift's
/// `!` and Rust's `unwrap()`. Must use `?.` (safe call) or `?:` (elvis).
/// Flags `!!` in `.kt` files (excluding tests).
#[must_use]
pub fn check_kotlin_nonnull_assertion(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "kt" {
        return Decision::pass();
    }
    if file_path.contains("Test") || file_path.contains("test") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let no_str = strip_string_literals(line);
        // `!!` is the non-null assertion. Exclude `!!!` (logical not + assertion, rare).
        if no_str.contains("!!") && !no_str.contains("!!!") {
            hits += 1;
        }
    }
    if hits > 2 {
        Decision::block(
            "UD-ARCH-024",
            format!(
                "UmaDev: Kotlin `!!` non-null assertion (UD-ARCH-024). \
                 `{file_path}` has {hits} `!!` assertions — they throw NPE if the \
                 value is null. Use `?.` (safe call), `?:` (elvis operator for \
                 defaults), or `requireNotNull()` for explicit validation.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-025** (Ruby): ban `eval` and `send` with dynamic input.
///
/// Ruby's `eval` and `send` execute arbitrary code/methods. `send` with
/// user-controlled input is a metaprogramming injection vector. Flags `eval(`
/// and `send(` with variable arguments in `.rb` files.
#[must_use]
pub fn check_ruby_eval_send(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "rb" {
        return Decision::pass();
    }
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains("eval(") {
            hits.push("eval()");
        }
        // `send` with a variable (not a symbol/string literal).
        if no_str.contains(".send(")
            && !no_str.contains(".send(:")
            && !no_str.contains(".send('")
            && !no_str.contains(".send(\"")
        {
            hits.push(".send(variable)");
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-ARCH-025",
        format!(
            "UmaDev: Ruby eval/send with dynamic input (UD-ARCH-025). \
             `{file_path}` uses {} — they execute arbitrary code/methods. \
             Avoid `eval` entirely; for `send`, restrict to a known allowlist \
             of method names, never pass user input directly.",
            labels.join(" / "),
        ),
    )
}

/// **UD-SEC-014**: ban insecure session-cookie configuration (OWASP A01/A07).
///
/// Session cookies MUST set `HttpOnly`, `Secure`, and `SameSite`. A cookie
/// without `HttpOnly` is readable by XSS; without `Secure` it's sent over
/// HTTP; without `SameSite` it's vulnerable to CSRF. Flags `Set-Cookie` /
/// `res.cookie` / `document.cookie` calls that miss these flags. Runs on
/// backend source.
#[must_use]
pub fn check_insecure_cookie(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Find cookie-setting calls.
    let cookie_calls = ["set-cookie", "res.cookie(", "set_cookie(", "cookies.set("];
    let has_cookie = cookie_calls.iter().any(|c| lower.contains(c));
    if !has_cookie {
        return Decision::pass();
    }
    // Check for required security flags.
    let has_httponly =
        lower.contains("httponly") || lower.contains("http_only") || lower.contains("httponly:");
    let has_secure = lower.contains("secure: true")
        || lower.contains("secure=true")
        || lower.contains("', secure");
    let has_samesite = lower.contains("samesite");
    if !has_httponly || !has_secure || !has_samesite {
        return Decision::block(
            "UD-SEC-014",
            format!(
                "UmaDev: insecure session cookie (UD-SEC-014). \
                 `{file_path}` sets a cookie without all of HttpOnly + Secure + \
                 SameSite. Missing HttpOnly → XSS can steal it; missing Secure → \
                 sent over plain HTTP; missing SameSite → CSRF. \
                 `res.cookie('sid', token, {{ httpOnly: true, secure: true, sameSite: 'strict' }})`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-015**: ban JWT verification defects (OWASP A02/A07).
///
/// Flags two critical JWT mistakes: (1) `algorithm: "none"` or accepting the
/// `none` alg — an attacker can forge tokens with no signature; (2) verifying
/// with a hardcoded secret instead of a key/env var. Runs on backend source.
#[must_use]
pub fn check_jwt_defects(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_jwt = lower.contains("jwt") || lower.contains("jsonwebtoken") || lower.contains("jwt.");
    if !has_jwt {
        return Decision::pass();
    }
    // Defect 1: algorithm "none".
    if lower.contains("\"none\"")
        || lower.contains("'none'")
        || lower.contains("algorithms: ['none']")
        || lower.contains("algorithm: none")
    {
        return Decision::block(
            "UD-SEC-015",
            format!(
                "UmaDev: JWT accepts 'none' algorithm (UD-SEC-015). \
                 `{file_path}` configures JWT verification to accept the `none` \
                 algorithm — an attacker can forge tokens with no signature. \
                 Always specify a concrete algorithm: `{{ algorithms: ['HS256'] }}`.",
            ),
        );
    }
    // Defect 2: verify with a hardcoded secret (not an env var).
    if (lower.contains("jwt.verify(") || lower.contains(".verify(")) && lower.contains("secret") {
        // Check if the secret is a string literal (hardcoded) vs env/process.env.
        let no_str = strip_string_literals(content);
        let uses_env = content.contains("process.env")
            || content.contains("os.environ")
            || content.contains("ENV[")
            || content.contains("getenv")
            || content.contains("System.getenv");
        // "secret" appears in content but NOT in the string-stripped version →
        // it was inside a string literal (hardcoded secret value).
        if !uses_env && !no_str.to_ascii_lowercase().contains("secret") {
            return Decision::block(
                "UD-SEC-015",
                format!(
                    "UmaDev: JWT verified with hardcoded secret (UD-SEC-015). \
                     `{file_path}` verifies JWT with a hardcoded secret string — \
                     anyone with source access can forge tokens. Load the secret \
                     from an env var: `jwt.verify(token, process.env.JWT_SECRET)`.",
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-ARCH-026**: require auth guard on sensitive API routes (OWASP A01).
///
/// Flags API route handlers (GET/POST/PUT/DELETE) that have no authentication
/// guard — no `auth`/`session`/`requireAuth`/`@Authorized`/middleware check.
/// Conservative: only flags mutation endpoints (POST/PUT/PATCH/DELETE) that
/// access sensitive data (`user`/`admin`/`payment`/`order` in the path or body).
#[must_use]
pub fn check_missing_auth_guard(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    // These handler recognizers are JavaScript/TypeScript framework syntax.
    // Other languages need framework-aware rules instead of string matching.
    if !matches!(ext.as_str(), "ts" | "js" | "mjs" | "cjs") {
        return Decision::pass();
    }
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let is_api = name == "route.ts"
        || name == "route.js"
        || name.starts_with("route.")
        || content.contains("export async function POST")
        || content.contains("export async function PUT")
        || content.contains("export async function DELETE")
        || content.contains("app.post(")
        || content.contains("app.put(")
        || content.contains("app.delete(");
    if !is_api {
        return Decision::pass();
    }
    // Must be a sensitive endpoint (user/admin/payment/order/account).
    let lower = content.to_ascii_lowercase();
    let is_sensitive = lower.contains("user")
        || lower.contains("admin")
        || lower.contains("payment")
        || lower.contains("order")
        || lower.contains("account")
        || lower.contains("profile")
        || lower.contains("delete");
    if !is_sensitive {
        return Decision::pass();
    }
    // Check for auth guard presence.
    let has_auth = lower.contains("getsession")
        || lower.contains("getserverSession".to_ascii_lowercase().as_str())
        || lower.contains("requireauth")
        || lower.contains("require_role")
        || lower.contains("auth(")
        || lower.contains("authmiddleware")
        || lower.contains("isauthenticated")
        || lower.contains("checkauth")
        || lower.contains("verifytoken")
        || lower.contains("jwtauth")
        || lower.contains("@authorized")
        || lower.contains("@preauthorize")
        || lower.contains("@requiresauthentication")
        || lower.contains("useauth")
        || lower.contains("withauth")
        || lower.contains("session.user");
    if !has_auth {
        return Decision::block(
            "UD-ARCH-026",
            format!(
                "UmaDev: sensitive API route missing auth guard (UD-ARCH-026). \
                 `{file_path}` is a mutation endpoint handling sensitive data \
                 (user/admin/payment) but has no authentication check. Add an \
                 auth guard: `const session = await getSession(); if (!session) \
                 return NextResponse.json({{error: 'Unauthorized'}}, {{status: 401}});` \
                 or wrap with `withAuth()` / `@PreAuthorize`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-027**: require transaction rollback on DB error paths.
///
/// A `BEGIN`/`transaction` without a matching `ROLLBACK`/`catch-and-rollback`
/// leaves partial writes on failure. Flags transaction blocks that have no
/// `rollback`/`revert`/catch handler. Runs on backend source.
#[must_use]
pub fn check_db_transaction_rollback(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "java" | "rs"
    ) {
        return Decision::pass();
    }
    // Scan code only — a "begin"/"transaction" inside a comment or doc string
    // is prose, not a transaction. Same comment-stripped view the emoji/color
    // rules use.
    let tz = crate::tokenizer::Tokenized::new(content);
    let code = tz.without_comments(content);
    let lower = code.to_ascii_lowercase();
    // Must contain a *real transaction-start API form*, not the bare English
    // words `begin` / `transaction` (those false-positive on `beginLoad()`,
    // `transactionId`, prose, etc.). We require an actual call/statement shape.
    let has_tx = lower.contains(".transaction(")   // ORM: db.transaction(...)
        || has_db_begin_call(&lower)                // tx.begin() / conn.begin() / db.begin()
        || lower.contains("begin transaction")     // SQL: BEGIN TRANSACTION
        || lower.contains("start transaction")     // SQL: START TRANSACTION
        || lower.contains("begin;")                // SQL: bare BEGIN; statement
        || lower.contains("start_transaction")     // python/driver: start_transaction()
        || lower.contains("begintransaction")      // begin_transaction / beginTransaction
        || lower.contains("begin_transaction");
    if !has_tx {
        return Decision::pass();
    }
    // Must have a rollback / commit with error handling.
    let has_rollback =
        lower.contains("rollback") || lower.contains("revert") || lower.contains("abort");
    let has_catch = lower.contains("catch")
        || lower.contains("except")
        || lower.contains("defer")
        || lower.contains("recover");
    if !has_rollback || (!has_catch && !lower.contains("commit")) {
        return Decision::block(
            "UD-ARCH-027",
            format!(
                "UmaDev: transaction missing rollback (UD-ARCH-027). \
                 `{file_path}` starts a database transaction but has no explicit \
                 rollback on error. A failure mid-transaction leaves partial writes. \
                 Wrap in try/catch and call `rollback()`, or use an ORM transaction \
                 helper that auto-rolls-back: `await tx.rollback()` in the catch block.",
            ),
        );
    }
    Decision::pass()
}

fn has_db_begin_call(code: &str) -> bool {
    code.match_indices(".begin(").any(|(dot, _)| {
        let receiver = code[..dot]
            .rsplit(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
            .next()
            .unwrap_or_default();
        matches!(
            receiver,
            "tx" | "transaction" | "db" | "conn" | "connection" | "session"
        )
    })
}

/// **UD-ARCH-028** (C/C++): ban `strcpy`, `strcat`, `gets`, `sprintf` — buffer overflow.
///
/// These C functions don't check buffer bounds — they're the #1 cause of
/// buffer-overflow vulnerabilities. Must use `strncpy`/`strncat`/`fgets`/`snprintf`.
/// Flags these in `.c`/`.cpp`/`.h` files.
#[must_use]
pub fn check_c_buffer_overflow(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "c" | "cpp" | "h" | "hpp" | "cc") {
        return Decision::pass();
    }
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        let no_str = strip_string_literals(line);
        for pat in UNSAFE_C_FUNCS {
            if no_str.contains(pat.trigger) {
                hits.push(pat.label);
            }
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-ARCH-028",
        format!(
            "UmaDev: unsafe C function — buffer overflow risk (UD-ARCH-028). \
             `{file_path}` uses {} — these don't check buffer bounds and cause \
             overflow vulnerabilities. Use the bounded variants: `strncpy`, \
             `strncat`, `fgets`, `snprintf`.",
            labels.join(" / "),
        ),
    )
}

/// One unsafe-C-function pattern.
struct UnsafeCFunc {
    trigger: &'static str,
    label: &'static str,
}

/// Unsafe C functions that cause buffer overflows.
const UNSAFE_C_FUNCS: &[UnsafeCFunc] = &[
    UnsafeCFunc {
        trigger: "strcpy(",
        label: "strcpy() (use strncpy)",
    },
    UnsafeCFunc {
        trigger: "strcat(",
        label: "strcat() (use strncat)",
    },
    UnsafeCFunc {
        trigger: "gets(",
        label: "gets() (use fgets)",
    },
    UnsafeCFunc {
        trigger: "sprintf(",
        label: "sprintf() (use snprintf)",
    },
    UnsafeCFunc {
        trigger: "scanf(\"%s",
        label: "scanf(\"%s\") (use %ns with width)",
    },
];

/// **UD-ARCH-029** (C/C++): ban `malloc` without NULL check.
///
/// `malloc` returns NULL on out-of-memory — dereferencing it is a null-pointer
/// crash. Flags `malloc(` not followed by a NULL check within a few lines.
/// Conservative: only flags when `malloc` is called but `NULL`/`!`/`== 0`
/// doesn't appear in the same function body.
#[must_use]
pub fn check_c_malloc_null_check(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "c" | "cpp" | "h" | "hpp" | "cc") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains("malloc(") || no_str.contains("calloc(") || no_str.contains("realloc(") {
            // Check if a NULL check appears within the next 3 lines.
            hits += 1;
        }
    }
    // Count NULL checks — if fewer than malloc calls, flag.
    let null_checks = content.matches("NULL").count()
        + content.matches("== 0").count()
        + content.matches("if (!").count();
    if hits > 0 && null_checks < hits {
        Decision::block(
            "UD-ARCH-029",
            format!(
                "UmaDev: malloc without NULL check (UD-ARCH-029). \
                 `{file_path}` calls malloc/calloc/realloc {hits} time(s) but has \
                 only {null_checks} NULL check(s). Dereferencing a NULL return \
                 crashes the process. Always check: \
                 `char *p = malloc(n); if (!p) {{ /* handle */ }}`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-SEC-017**: WebSearch result provenance — flag unreliable sources in
/// research output.
///
/// When the host researches via WebSearch and writes findings into a research
/// document, it must cite authoritative sources (official docs, peer-reviewed
/// papers, reputable tech publications). Flags research docs that cite known-
/// unreliable sources (Wikipedia as primary, random blogs, forums) without a
/// primary/authoritative citation. Also flags "I couldn't find" cop-outs.
/// Conservative: only flags research-markdown files (`-research.md`) so it
/// doesn't interfere with code.
#[must_use]
pub fn check_unreliable_sources(file_path: &str, content: &str) -> Decision {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Only scan research output files.
    if !name.contains("research") && !name.contains("competitor") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let mut issues = Vec::new();
    // 1. Wikipedia as a primary/only source.
    let wiki_count = lower.matches("wikipedia").count();
    if wiki_count > 0 {
        let has_authoritative = lower.contains("official documentation")
            || lower.contains("docs.")
            || lower.contains("developer.mozilla.org")
            || lower.contains("w3.org")
            || lower.contains("ieee.org")
            || lower.contains("arxiv.org")
            || lower.contains("github.com/")
            || lower.contains("doi.org");
        if !has_authoritative {
            issues.push("cites Wikipedia without any authoritative primary source");
        }
    }
    // 2. "I couldn't find" / "no information available" cop-out.
    if lower.contains("could not find")
        || lower.contains("couldn't find")
        || lower.contains("no information available")
        || lower.contains("unable to find")
        || lower.contains("no results found")
    {
        issues.push(
            "contains 'couldn't find' cop-out — research should cite what exists, not give up",
        );
    }
    // 3. Generic blog citations without specific URLs (heuristic: "blog"
    //    mentioned but no http link nearby).
    let blog_count = lower.matches("blog").count();
    let url_count = lower.matches("http").count();
    if blog_count > 2 && url_count == 0 {
        issues.push("references 'blog' posts but includes zero URLs — unverifiable claims");
    }
    if issues.is_empty() {
        return Decision::pass();
    }
    Decision::block(
        "UD-SEC-017",
        format!(
            "UmaDev: unreliable research sources (UD-SEC-017). \
             `{file_path}` — {}. Research output must cite authoritative \
             primary sources (official docs, peer-reviewed papers, reputable \
             publications with URLs). Re-run the research with WebSearch and \
             cite concrete URLs.",
            issues.join("; "),
        ),
    )
}

/// `true` when the string/comment-stripped `line` assigns config key `name`.
/// Member reads (`self.base_url`), format arguments, function parameters, and
/// Rust module paths are not assignments and must not be reported merely
/// because another string literal happens to share their line.
fn contains_config_assignment(line: &str, name: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(name) {
        let at = from + rel;
        let starts_token = at == 0
            || !lower[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let end = at + name.len();
        let ends_token = lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        if starts_token && ends_token {
            let rest = line[end..].trim_start();
            let assignment = if let Some(after_equals) = rest.strip_prefix('=') {
                !after_equals.starts_with(['=', '>'])
            } else if rest.starts_with("::") {
                false
            } else {
                rest.strip_prefix(':').is_some()
            };
            if assignment {
                return true;
            }
        }
        from = end;
        if from >= line.len() {
            break;
        }
    }
    false
}

/// **UD-ARCH-030**: require environment-based configuration (no hardcoded URLs/ports).
///
/// Production code must read config (DB URLs, ports, API endpoints) from
/// environment variables — never hardcode them. Flags backend files that
/// assign a URL/port/host as a string literal to a variable (not via env).
/// Conservative: only flags clear config patterns (`const DATABASE_URL =`,
/// `const PORT =`, `host: "..."`), not string literals in general logic.
#[must_use]
pub fn check_hardcoded_config(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "rs" | "java"
    ) || looks_like_secret_test_path(file_path)
    {
        return Decision::pass();
    }
    let content = if ext == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let ll = line.to_ascii_lowercase();
        let trimmed = ll.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }
        // Rust uses apostrophes for lifetimes (`&'a str`). Treating those as
        // single-quoted strings makes an unrelated `host:` field appear to
        // share a line with a hardcoded literal. Rust string/config values are
        // double-quoted, so its view only removes double-quoted literals.
        let no_str = if ext == "rs" {
            crate::tokenizer::strip_double_quoted_literals(line)
        } else {
            strip_string_literals(line)
        };
        // Pattern: a known config-name variable assigned a string literal.
        for name in CONFIG_VAR_NAMES {
            if contains_config_assignment(&no_str, name) && no_str != line {
                // Check it's NOT reading from env.
                if !line.contains("process.env")
                    && !line.contains("os.environ")
                    && !line.contains("ENV[")
                    && !line.contains("getenv")
                    && !line.contains("System.getenv")
                    && !line.contains("std::env")
                    && !line.contains("env::")
                    && !line.contains("dotenv")
                {
                    hits.push(name);
                }
            }
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-ARCH-030",
        format!(
            "UmaDev: hardcoded configuration (UD-ARCH-030). \
             `{file_path}` hardcodes {} as a string literal instead of reading \
             from an environment variable. Production config must be externalized: \
             `const port = process.env.PORT || 3000`. Move values to `.env` \
             (gitignored) and read via `dotenv`.",
            labels.join(", "),
        ),
    )
}

/// **UD-ARCH-031** (Scala): ban `null` and `return` in Scala source.
///
/// Scala has `Option`/`Either` for absence and expression-based returns —
/// `null` causes NPEs and `return` breaks expression-flow. Flags these in
/// `.scala` files. Conservative: only flags when there are more than 1 (a
/// single null in interop code is acceptable).
#[must_use]
pub fn check_scala_null_return(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "scala" {
        return Decision::pass();
    }
    let mut null_hits = 0usize;
    let mut return_hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let no_str = strip_string_literals(line);
        // `null` as a value (not in a comment/string).
        if no_str.contains("null") && !no_str.contains("nullable") && !no_str.contains("notNull") {
            null_hits += 1;
        }
        // `return` statement (Scala prefers expression-based).
        if no_str.contains("return ") || no_str.trim_end().ends_with("return") {
            return_hits += 1;
        }
    }
    if null_hits > 1 || return_hits > 2 {
        Decision::block(
            "UD-ARCH-031",
            format!(
                "UmaDev: null/return in Scala (UD-ARCH-031). \
                 `{file_path}` uses `null` ({null_hits}x) and/or `return` ({return_hits}x) \
                 — Scala has `Option`/`Either` for absence and is expression-based. \
                 Use `None`/`Some(x)` instead of null, and remove explicit `return` \
                 (the last expression is the return value).",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-032** (R): ban hardcoded paths via `setwd()`.
///
/// `setwd("/Users/john/data")` makes a script non-portable — it breaks on
/// any other machine. Must use relative paths or `here::here()`. Flags
/// `setwd(` with an absolute path string in `.R`/`.r` files.
#[must_use]
pub fn check_r_hardcoded_path(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "r") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        // setwd with an absolute path (starts with / or ~ or drive letter).
        if line.contains("setwd(") {
            let after = line.split("setwd(").nth(1).unwrap_or("");
            let path_part = after.trim_start_matches(['"', '\'', ' ']);
            if path_part.starts_with('/')
                || path_part.starts_with('~')
                || (path_part.len() > 1 && path_part.as_bytes()[1] == b':')
            {
                hits += 1;
            }
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-032",
            format!(
                "UmaDev: hardcoded path in R setwd() (UD-ARCH-032). \
                 `{file_path}` calls `setwd()` with an absolute path — the script \
                 won't run on any other machine. Use `here::here()` for project-\
                 relative paths, or pass the data directory as a parameter.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-033** (Lua): ban `loadstring()` — code injection.
///
/// `loadstring()` compiles and executes an arbitrary string as Lua code —
/// equivalent to `eval()`. If the string is user-influenced, it's an RCE
/// vector. Must use `load()` with a sandboxed environment or parse data
/// instead. Flags `loadstring(` in `.lua` files.
#[must_use]
pub fn check_lua_loadstring(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "lua" {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains("loadstring(") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-033",
            format!(
                "UmaDev: loadstring() in Lua (UD-ARCH-033). \
                 `{file_path}` uses `loadstring()` — it compiles and executes \
                 arbitrary strings as code (RCE if user-influenced). Use `load()` \
                 with a restricted environment, or parse structured data instead.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-034** (Perl): ban regex with `e` flag (code execution in regex).
///
/// The `/e` flag in Perl's `s///e` evaluates the replacement as Perl code —
/// equivalent to `eval()`. If the pattern or replacement is user-influenced,
/// it's an RCE vector. Flags `s/.../.../e` and `=~ s/.../e` in `.pl`/`.pm`.
#[must_use]
pub fn check_perl_eval_regex(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "pl" | "pm") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        // `s/.../.../e` — substitution with eval flag.
        // Look for `s/` ... `/e` or `s{...}{...}e`.
        if (line.contains("s/") || line.contains("s{")) && line.contains("/e") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-034",
            format!(
                "UmaDev: Perl regex with /e flag (UD-ARCH-034). \
                 `{file_path}` uses `s/.../.../e` — the `/e` flag executes the \
                 replacement as Perl code (RCE if user-influenced). Use a plain \
                 substitution `/s/.../.../` or `eval` in a controlled block.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-035** (Elixir): ban `String.to_atom` / `to_charlist` → atom.
///
/// Converting user input to atoms is a memory-exhaustion DoS — atoms are
/// never garbage-collected in Elixir/Erlang. An attacker sending unique
/// strings can fill the atom table and crash the VM. Flags `to_atom` in
/// `.ex`/`.exs` files.
#[must_use]
pub fn check_elixir_to_atom(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ex" | "exs") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains("to_atom(") || no_str.contains("String.to_atom") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-035",
            format!(
                "UmaDev: String.to_atom in Elixir (UD-ARCH-035). \
                 `{file_path}` converts strings to atoms — atoms are never \
                 garbage-collected, so user input can exhaust the atom table \
                 and crash the VM (DoS). Use `String.to_existing_atom` (safe — \
                 raises on unknown atoms) or keep data as strings.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-036** (Haskell): ban `unsafePerformIO`.
///
/// `unsafePerformIO` runs an IO action in a pure context — it breaks
/// referential transparency, makes results non-deterministic, and can cause
/// subtle concurrency bugs. Commercial Haskell code must keep IO in the IO
/// monad. Flags `unsafePerformIO` in `.hs` files.
#[must_use]
pub fn check_haskell_unsafe_io(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "hs" | "lhs") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains("unsafePerformIO") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-036",
            format!(
                "UmaDev: unsafePerformIO in Haskell (UD-ARCH-036). \
                 `{file_path}` uses `unsafePerformIO` ({hits}x) — it breaks \
                 referential transparency and introduces non-determinism. \
                 Keep side effects in the IO monad: `main :: IO ()` and \
                 thread the IO through your function signatures.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-037** (Clojure): ban `eval` and `(read-string)` on untrusted input.
///
/// Clojure's `eval` and `clojure.edn/read-string` / `read` execute arbitrary
/// code/data — if the input is user-supplied, it's a code-injection vector.
/// Must use `clojure.edn/read-string` (safe EDN parsing) instead of `read`.
/// Flags `eval` and `(read` in `.clj`/`.cljs`/`.cljc` files.
#[must_use]
pub fn check_clojure_eval(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "clj" | "cljs" | "cljc") {
        return Decision::pass();
    }
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(';') {
            continue;
        }
        let no_str = strip_string_literals(line);
        // `(eval ` — Clojure eval function call.
        if no_str.contains("(eval ") || no_str.contains("(eval)") {
            hits.push("(eval ...)");
        }
        // `(read ` or `(read-string ` — unsafe reader (not clojure.edn).
        if (no_str.contains("(read ") || no_str.contains("(read-string "))
            && !no_str.contains("clojure.edn")
        {
            hits.push("(read/read-string)");
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-ARCH-037",
        format!(
            "UmaDev: eval/read in Clojure (UD-ARCH-037). \
             `{file_path}` uses {} — they execute arbitrary code/data. If the \
             input is user-supplied it's a code-injection vector. Use \
             `clojure.edn/read-string` for safe data parsing (no code eval).",
            labels.join(" / "),
        ),
    )
}

/// **UD-ARCH-038** (OCaml): ban `Obj.magic` — unsafe type cast.
///
/// `Obj.magic` casts any value to any type with no runtime check — it's
/// OCaml's `unsafeCoerce`. It causes undefined behavior / crashes if the
/// types don't actually match. Commercial OCaml must use proper type
/// conversions. Flags `Obj.magic` in `.ml`/`.mli` files.
#[must_use]
pub fn check_ocaml_magic(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ml" | "mli") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        if line.trim_start().starts_with("(*") || line.trim_start().starts_with("//") {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains("Obj.magic") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-038",
            format!(
                "UmaDev: Obj.magic in OCaml (UD-ARCH-038). \
                 `{file_path}` uses `Obj.magic` ({hits}x) — it casts any value \
                 to any type with no runtime check, causing undefined behavior \
                 on mismatch. Use proper type conversions, GADTs, or polymorphic \
                 records instead.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-039** (F#): ban `null` in F# source.
///
/// F# has `Option` for absence — `null` causes NullReferenceException and
/// breaks the type system's null-safety. Flags `null` in `.fs`/`.fsx` files
/// (excluding interop `allowNullLiteral` annotations). Conservative: only
/// flags when there are more than 1 (single null in interop is acceptable).
#[must_use]
pub fn check_fsharp_null(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "fs" | "fsx") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        if line.trim_start().starts_with("//") || line.trim_start().starts_with("(*") {
            continue;
        }
        let no_str = strip_string_literals(line);
        if no_str.contains(" null") && !no_str.contains("AllowNullLiteral") {
            hits += 1;
        }
    }
    if hits > 1 {
        Decision::block(
            "UD-ARCH-039",
            format!(
                "UmaDev: null in F# (UD-ARCH-039). \
                 `{file_path}` uses `null` ({hits}x) — F# has `Option` for \
                 absence. `null` causes NullReferenceException and breaks \
                 null-safety. Use `None`/`Some(x)` instead. For .NET interop, \
                 annotate with `[<AllowNullLiteral>]`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-040** (Dart): ban `dynamic` type.
///
/// Dart's `dynamic` disables all type checking — it's the Dart equivalent of
/// TypeScript's `any`. Runtime errors that the compiler would have caught
/// become silent bugs. Must use `Object?` or a concrete type. Flags
/// `dynamic` as a type annotation in `.dart` files (excluding test files).
#[must_use]
pub fn check_dart_dynamic(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "dart" {
        return Decision::pass();
    }
    if file_path.contains("_test.dart") || file_path.contains("test/") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let no_str = strip_string_literals(line);
        // `dynamic` as a type annotation: `dynamic x =`, `dynamic foo(`,
        // `Map<String, dynamic>`, `List<dynamic>`.
        if no_str.contains("dynamic ")
            || no_str.contains("dynamic>")
            || no_str.contains("dynamic,")
            || no_str.contains("dynamic)")
            || no_str.contains(": dynamic")
            || no_str.trim_end().ends_with("dynamic")
        {
            hits += 1;
        }
    }
    if hits > 2 {
        Decision::block(
            "UD-ARCH-040",
            format!(
                "UmaDev: dynamic type in Dart (UD-ARCH-040). \
                 `{file_path}` uses `dynamic` ({hits}x) — it disables all type \
                 checking, turning compile errors into runtime crashes. Use \
                 `Object?` (any typed value) or a concrete type: \
                 `Map<String, Object?>`, `List<User>`, etc.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-SEC-018**: ban plaintext password handling — insecure storage/comparison.
///
/// Passwords must be hashed with bcrypt/argon2/scrypt — never stored in plain
/// text or compared with `==`. Flags: (1) password assignment to a string
/// literal or DB column without hashing; (2) `==` comparison of a password
/// variable; (3) `password` field in a DB insert without a hash function.
/// Runs on backend source.
///
/// Persistence is correlated inside one logical statement or an adjacent
/// `owner.password = value; owner.save()` pair. An unrelated `HashMap::insert`,
/// plan save, or API example elsewhere in the file is not evidence of password
/// storage. Multi-line call arguments stay in one statement. A direct hash call,
/// a hash-named password value, or a value assigned from a supported hasher is
/// treated as hashed. This is intentionally lexical and fail-open: uncertain
/// cross-function data flow is left to a semantic analyzer.
/// Distant keywords are never combined into a synthetic finding.
#[must_use]
pub fn check_plaintext_password(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "java" | "rs"
    ) || looks_like_secret_test_path(file_path)
    {
        return Decision::pass();
    }
    let content = if ext == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let mut issues: Vec<&str> = Vec::new();

    // 1. Password compared with == / === (should use bcrypt.compare).
    for line in content.lines() {
        let ll = line.to_ascii_lowercase();
        let trimmed = ll.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }
        let no_str = strip_string_literals(line);
        // `password ==` or `== password` or `password ===`
        if (no_str.to_ascii_lowercase().contains("password ==")
            || no_str.to_ascii_lowercase().contains("password ===")
            || no_str.to_ascii_lowercase().contains("== password")
            || no_str.to_ascii_lowercase().contains("=== password"))
            && !no_str.to_ascii_lowercase().contains("bcrypt")
            && !no_str.to_ascii_lowercase().contains("compare")
        {
            issues.push("password compared with == (use bcrypt.compare)");
        }
    }

    // 2. Password storage and its hash proof must share local data-flow context.
    if crate::security_context::contains_unhashed_password_storage(content) {
        issues.push("stores/creates a password without a hashing function (bcrypt/argon2)");
    }

    if issues.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = issues
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-SEC-018",
        format!(
            "UmaDev: insecure password handling (UD-SEC-018). \
             `{file_path}` — {}. Passwords must be hashed with bcrypt/argon2 \
             before storage, and verified with `bcrypt.compare(input, hash)`, \
             never `==`. Plaintext storage or comparison is a credential-breach \
             vector.",
            labels.join("; "),
        ),
    )
}

/// **UD-ARCH-041**: require file-upload validation (type + size checks).
///
/// A file-upload endpoint that doesn't validate type and size is a vector for
/// malicious file uploads (web shells, oversized DoS). Flags handlers that
/// accept file uploads (`multer`/`formData`/`request.files`/`multipart`) but
/// have no `maxFileSize`/`allowedTypes`/`mimetype`/`size` validation.
#[must_use]
pub fn check_file_upload_validation(file_path: &str, content: &str) -> Decision {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let tokenized = crate::tokenizer::Tokenized::new(content);
    let lower = tokenized.without_comments(content).to_ascii_lowercase();
    // Must handle file uploads.
    let handles_upload = lower.contains("multer")
        || lower.contains("formdata")
        || lower.contains("form-data")
        || lower.contains("request.files")
        || lower.contains("req.files")
        || lower.contains("upload.single(")
        || lower.contains("upload.array(")
        || lower.contains("multipart")
        || lower.contains("uploadedfile")
        || lower.contains("multipartfile")
        || lower.contains("formfile(")
        || lower.contains("parsemultipartform(");
    if !handles_upload {
        return Decision::pass();
    }
    let lower_name = name.to_ascii_lowercase();
    let is_endpoint = matches!(lower_name.as_str(), "route.ts" | "route.js")
        || lower_name.starts_with("route.")
        || lower_name.contains("controller")
        || lower.contains("export async function post")
        || lower.contains("app.post(")
        || lower.contains("router.post(")
        || lower.contains("@postmapping")
        || lower.contains("@post(")
        || lower.contains("request.files")
        || lower.contains("req.files")
        || lower.contains("multipartfile")
        || lower.contains("formfile(")
        || lower.contains("parsemultipartform(");
    if !is_endpoint {
        return Decision::pass();
    }
    // Must have validation.
    let has_validation = lower.contains("maxfilesize")
        || lower.contains("max_size")
        || lower.contains("maxsize")
        || lower.contains("limit:")
        || lower.contains("allowedtypes")
        || lower.contains("allowed_types")
        || lower.contains("mimetype")
        || lower.contains("mime_type")
        || lower.contains("content-type")
        || lower.contains("filesize")
        || lower.contains("file.size")
        || lower.contains("limits:")
        || lower.contains("accept:")
        || lower.contains("validate(");
    if !has_validation {
        return Decision::block(
            "UD-ARCH-041",
            format!(
                "UmaDev: file upload without validation (UD-ARCH-041). \
                 `{file_path}` accepts file uploads but has no type/size \
                 validation. An attacker can upload web shells or oversized \
                 files (DoS). Configure limits: `multer({{ limits: {{ \
                 fileSize: 5_000_000 }}, fileFilter }})` or validate \
                 `mimetype` and `size` manually before saving.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-019**: ban open redirect — redirect to user-supplied URL without validation.
///
/// A redirect to a URL derived from user input (`?next=` / `?redirect=`)
/// without allowlist validation is an open-redirect vulnerability — attackers
/// can use it for phishing. Flags `redirect(` / `res.redirect(` with a
/// dynamic variable and no allowlist/starts-with check. Runs on backend.
#[must_use]
pub fn check_open_redirect(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must contain a redirect call.
    let has_redirect = lower.contains("redirect(")
        || lower.contains("location:")
        || lower.contains("res.redirect")
        || lower.contains("response.redirect")
        || lower.contains("header(\"location");
    if !has_redirect {
        return Decision::pass();
    }
    // Dynamic target: redirect uses a variable/query param.
    let dynamic_target = lower.contains("redirect(req.")
        || lower.contains("redirect(query")
        || lower.contains("redirect(params")
        || lower.contains("redirect(body")
        || lower.contains("redirect(next")
        || lower.contains("redirect(redirect")
        || lower.contains("redirect(target")
        || lower.contains("redirect(callback")
        || lower.contains("redirect(returnurl")
        || lower.contains("redirect(return_url");
    // Safe: has an allowlist / startsWith check.
    let has_guard = lower.contains("allowlist")
        || lower.contains("startswith")
        || lower.contains("starts_with")
        || lower.contains("includes(")
        || lower.contains("isvalidurl")
        || lower.contains("validateurl")
        || lower.contains("url.parse")
        || lower.contains("new url(");
    if dynamic_target && !has_guard {
        return Decision::block(
            "UD-SEC-019",
            format!(
                "UmaDev: potential open redirect (UD-SEC-019). \
                 `{file_path}` redirects to a user-supplied URL without \
                 validation — attackers can craft phishing links like \
                 `?next=https://evil.com`. Validate the redirect target \
                 against an allowlist: `if (!ALLOWED_HOSTS.includes(host)) \
                 return res.redirect('/')`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-042**: ban logging of sensitive data (passwords/tokens/keys).
///
/// Logging `password`, `token`, `secret`, `creditCard`, or `ssn` to any log
/// output (`console.log`, `logger.info`, `print(`) leaks credentials into
/// log aggregation systems. Flags logging calls that reference these field
/// names. Runs on backend source.
#[must_use]
pub fn check_sensitive_logging(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "java" | "rs"
    ) {
        return Decision::pass();
    }
    if looks_like_secret_test_path(file_path) {
        return Decision::pass();
    }
    let content = if ext == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with('*')
            || trimmed.starts_with("/*")
        {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        // Must be a logging call.
        let is_log = lower.contains("console.log")
            || lower.contains("console.info")
            || lower.contains("console.warn")
            || lower.contains("console.error")
            || lower.contains("logger.")
            || lower.contains("log.info")
            || lower.contains("log.debug")
            || lower.contains("log.error")
            || lower.contains("log.warn")
            || lower.contains("log.printf")
            || contains_bare_call(&lower, "print")
            || lower.contains("fmt.print")
            || lower.contains("system.out.print");
        if !is_log {
            continue;
        }
        // Check for sensitive field names in the log call.
        for field in SENSITIVE_LOG_FIELDS {
            if contains_sensitive_log_field(&lower, field) {
                hits.push(field);
            }
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-ARCH-042",
        format!(
            "UmaDev: sensitive data in log output (UD-ARCH-042). \
             `{file_path}` logs {} — credentials in logs are a breach vector \
             (log aggregation, shared dashboards, accidental screen-shares). \
             Strip or mask sensitive fields before logging: \
             `{{ ...user, password: '[REDACTED]' }}`.",
            labels.join(" / "),
        ),
    )
}

fn contains_bare_call(line: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    line.match_indices(&needle).any(|(index, _)| {
        line[..index]
            .chars()
            .next_back()
            .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'))
    })
}

/// Sensitive field names that must never appear in log output.
const SENSITIVE_LOG_FIELDS: &[&str] = &[
    "password",
    "passwd",
    "token",
    "accesstoken",
    "access_token",
    "refreshtoken",
    "refresh_token",
    "apikey",
    "api_key",
    "secret",
    "creditcard",
    "credit_card",
    "cardnumber",
    "card_number",
    "ssn",
    "social_security",
    "privatekey",
    "private_key",
    "authorization",
    "cookie",
    "sessionid",
    "session_id",
];

fn contains_sensitive_log_field(line: &str, field: &str) -> bool {
    line.match_indices(field).any(|(index, matched)| {
        let before = line[..index].chars().next_back();
        let after = line[index + matched.len()..].chars().next();
        let is_identifier = |ch: char| ch.is_ascii_alphanumeric() || ch == '_';
        before.is_none_or(|ch| !is_identifier(ch)) && after.is_none_or(|ch| !is_identifier(ch))
    })
}

/// **UD-ARCH-043**: ban insecure random number generation in security contexts.
///
/// `Math.random()`, Python's `random`, and Ruby's `rand` are NOT
/// cryptographically secure — their output is predictable. Token/key/nonce
/// generation must use `crypto.getRandomValues` / `secrets` / `SecureRandom`.
/// Flags insecure RNG when the surrounding code mentions token/key/secret/
/// nonce/session. Runs on backend source.
#[must_use]
pub fn check_insecure_random(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "java" | "rs"
    ) {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must use an insecure RNG.
    let insecure_rng = lower.contains("math.random")
        || lower.contains("math.random()")
        || lower.contains("random.random")
        || lower.contains("random.randint")
        || lower.contains("random.choice")
        || lower.contains("rand.int")
        || lower.contains("math/rand")
        || lower.contains("java.util.random");
    if !insecure_rng {
        return Decision::pass();
    }
    // AND must be in a security context (token/key/secret/nonce/session/password).
    let security_context = lower.contains("token")
        || lower.contains("key")
        || lower.contains("secret")
        || lower.contains("nonce")
        || lower.contains("session")
        || lower.contains("password")
        || lower.contains("otp")
        || lower.contains("verification")
        || lower.contains("csrf");
    if !security_context {
        return Decision::pass();
    }
    // Safe: uses a crypto-safe RNG.
    let has_crypto_rng = lower.contains("crypto.getrandomvalues")
        || lower.contains("crypto.randombytes")
        || lower.contains("secrets.")
        || lower.contains("secrets.token")
        || lower.contains("securerandom")
        || lower.contains("crypto/rand")
        || lower.contains("os.urandom")
        || lower.contains("rand::thread_rng")
        || lower.contains("rand::rngs");
    if !has_crypto_rng {
        return Decision::block(
            "UD-ARCH-043",
            format!(
                "UmaDev: insecure RNG in security context (UD-ARCH-043). \
                 `{file_path}` uses a non-cryptographic random generator \
                 (Math.random / random / rand) in code that handles tokens, \
                 keys, or sessions — its output is predictable and guessable. \
                 Use a crypto-safe RNG: `crypto.getRandomValues()` (JS), \
                 `secrets.token_hex()` (Python), `SecureRandom` (Java/Ruby), \
                 `crypto/rand` (Go).",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-044**: ban catastrophic-backtracking regexes (ReDoS).
///
/// Certain regex patterns cause exponential backtracking on crafted input —
/// `(a+)+`, `(a*)*`, `(a|a)*`, nested quantifiers like `(.*+)+`. An attacker
/// can send a 30-char string that hangs the regex engine for hours (DoS).
/// Flags nested quantifier patterns in regex literals/strings.
#[must_use]
pub fn check_redos_regex(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "rs" | "java"
    ) {
        return Decision::pass();
    }
    let mut hits: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        for pattern in REDOS_PATTERNS {
            if lower.contains(pattern) {
                hits.push(pattern);
            }
        }
    }
    if hits.is_empty() {
        return Decision::pass();
    }
    let labels: Vec<&str> = hits
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Decision::block(
        "UD-ARCH-044",
        format!(
            "UmaDev: potential ReDoS regex (UD-ARCH-044). \
             `{file_path}` contains nested-quantifier regex pattern(s) ({}) — \
             these cause catastrophic backtracking on crafted input (DoS). \
             Rewrite with a single quantifier, use a non-backtracking engine, \
             or add an input-length limit before matching.",
            labels.join(", "),
        ),
    )
}

/// Regex patterns that cause catastrophic backtracking (ReDoS).
/// These are the classic evil patterns from the OWASP ReDoS guidance.
const REDOS_PATTERNS: &[&str] = &[
    concat!("(a+)", "+"),
    concat!("(a*)", "*"),
    concat!("(a|a)", "*"),
    concat!("(.*+)", "+"),
    concat!("(.+)", "+"),
    concat!("(.*)", "+"),
    concat!("(.+)", "*"),
    concat!("(.*)", "*"),
    concat!("([a-zA-Z]+)", "*"),
    concat!("([a-zA-Z]*)", "*"),
    concat!("(\\w+)", "+"),
    concat!("(\\w*)", "*"),
    concat!("(\\d+)", "+"),
    concat!("([\\w-]+)", "+"),
];

/// **UD-SEC-020**: ban path traversal — file access with user-supplied paths.
///
/// Building a file path from user input (`fs.readFile(req.query.file)`,
/// `path.join(base, userInput)`) without sanitization lets an attacker
/// escape the base directory (`../../etc/passwd`). Flags path-building/
/// reading calls that use a dynamic variable without a path-containment
/// check. Runs on backend source.
#[must_use]
pub fn check_path_traversal(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "java" | "rs"
    ) {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must read/build a file path with a dynamic source.
    let file_ops = [
        "readfile(",
        "read_file(",
        "fs.readfile",
        "fs.writefile",
        "createreadstream",
        "path.join(",
        "path.resolve(",
        "os.path.join",
        "open(",
        "fopen(",
        "file_path(",
        "filepath.join",
    ];
    let has_file_op = file_ops.iter().any(|op| lower.contains(op));
    if !has_file_op {
        return Decision::pass();
    }
    // Dynamic source: user-supplied path.
    let dynamic_source = lower.contains("req.query")
        || lower.contains("req.params")
        || lower.contains("req.body")
        || lower.contains("request.get")
        || lower.contains("user_input")
        || lower.contains("userinput")
        || lower.contains("filename")
        || lower.contains("filepath")
        || lower.contains("file_name")
        || lower.contains("file_path");
    if !dynamic_source {
        return Decision::pass();
    }
    // Safe: has a sanitization/containment check.
    let has_guard = lower.contains("..")
        && (lower.contains("reject")
            || lower.contains("deny")
            || lower.contains("forbidden")
            || lower.contains("invalid"));
    let has_resolve_check = lower.contains("startswith")
        || lower.contains("starts_with")
        || lower.contains("realpath")
        || lower.contains("canonicalize")
        || lower.contains("issubpath")
        || lower.contains("contain")
        || lower.contains("normalize")
        || lower.contains("sanitize");
    if !has_guard && !has_resolve_check {
        return Decision::block(
            "UD-SEC-020",
            format!(
                "UmaDev: potential path traversal (UD-SEC-020). \
                 `{file_path}` reads/builds a file path from user input without \
                 sanitization — an attacker can escape the base dir with \
                 `../../etc/passwd`. Validate: reject `..` segments, resolve \
                 and check `resolvedPath.startsWith(baseDir)`, or use \
                 `path.normalize` + containment check.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-021**: ban mass assignment — passing all user input to a model/DB.
///
/// `User.create(req.body)` / `update(req.body)` lets a user set ANY field
/// (including `role`, `isAdmin`, `password`) — a privilege-escalation vector.
/// Must whitelist specific fields (`pick`/`permit`/destructuring). Flags
/// model create/update/save calls that pass `req.body`/`request.json()`
/// directly without field filtering. Runs on backend source.
#[must_use]
pub fn check_mass_assignment(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Direct pass of raw user input to a model/DB operation.
    let mass_patterns = [
        "create(req.body",
        "update(req.body",
        "save(req.body",
        "create(request.json",
        "update(request.json",
        "insert(req.body",
        "create(req.data",
        "update(req.data",
        ".create({...req.body})",
        ".update({...req.body})",
        "user.create(req.body",
        "user.update(req.body",
    ];
    let has_mass = mass_patterns.iter().any(|p| lower.contains(p));
    if !has_mass {
        return Decision::pass();
    }
    // Safe: has field whitelisting / pick / permit / destructuring.
    let has_whitelist = lower.contains("pick(")
        || lower.contains(".pick(")
        || lower.contains("permit(")
        || lower.contains("allowlist")
        || lower.contains("whitelist")
        || lower.contains("allowedfields")
        || lower.contains("allowed_fields")
        || lower.contains("select(")
        || lower.contains("const { ");
    if !has_whitelist {
        return Decision::block(
            "UD-SEC-021",
            format!(
                "UmaDev: mass assignment (UD-SEC-021). \
                 `{file_path}` passes raw user input to a model/DB operation — \
                 an attacker can set `role`, `isAdmin`, or other privileged \
                 fields. Whitelist specific fields: `const {{ name, email }} = \
                 req.body; User.create({{ name, email }})` or use `.pick()`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-022**: ban HTTP response splitting — header injection via CRLF.
///
/// Setting a response header (`setHeader`, `Location`, `Set-Cookie`) with
/// user-supplied input that contains `\r\n` (CRLF) lets an attacker inject
/// arbitrary headers or split the HTTP response. Flags header-setting calls
/// with a dynamic variable and no CRLF-sanitization. Runs on backend source.
#[must_use]
pub fn check_response_splitting(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must set a response header.
    let header_set = lower.contains("setheader(")
        || lower.contains("set_header(")
        || lower.contains("location:")
        || lower.contains("res.set")
        || lower.contains("addheader(")
        || lower.contains("response.set_header")
        || lower.contains("header(\"location")
        || lower.contains("header('location");
    if !header_set {
        return Decision::pass();
    }
    // Dynamic value from user input.
    let dynamic_val = lower.contains("req.query")
        || lower.contains("req.params")
        || lower.contains("req.body")
        || lower.contains("request.get")
        || lower.contains("userinput")
        || lower.contains("user_input")
        || lower.contains("redirecturl")
        || lower.contains("redirect_url");
    if !dynamic_val {
        return Decision::pass();
    }
    // Safe: has CRLF sanitization.
    let has_sanitizer = lower.contains("replace(")
        || lower.contains("encodeuri")
        || lower.contains("sanitize")
        || lower.contains("strip")
        || lower.contains("\\r\\n")
        || lower.contains("crlf");
    if !has_sanitizer {
        return Decision::block(
            "UD-SEC-022",
            format!(
                "UmaDev: potential HTTP response splitting (UD-SEC-022). \
                 `{file_path}` sets a response header with user-supplied input — \
                 if the input contains `\\r\\n` (CRLF), an attacker can inject \
                 arbitrary headers or split the response. Sanitize the value: \
                 strip `\\r` and `\\n`, or use `encodeURIComponent` before \
                 setting it as a header.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-045**: ban information leakage — raw errors/stack traces to client.
///
/// Returning `err.message`, `err.stack`, `str(e)`, or `error: e` in an API
/// response leaks internal structure (file paths, SQL, library versions) that
/// helps attackers. Must return a generic error message and log details
/// server-side. Flags response/send calls that include raw error objects.
/// Runs on backend API source.
#[must_use]
pub fn check_info_leakage(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must be in an error-handling + response context.
    let has_error_ctx = lower.contains("catch")
        || lower.contains("except")
        || lower.contains("recover")
        || lower.contains("throw");
    let has_response = lower.contains("json(")
        || lower.contains("send(")
        || lower.contains("return {")
        || lower.contains("return{")
        || lower.contains("res.")
        || lower.contains("response.");
    if !has_error_ctx || !has_response {
        return Decision::pass();
    }
    // Raw error object sent to client.
    let raw_error = lower.contains("err.message")
        || lower.contains("err.stack")
        || lower.contains("error.message")
        || lower.contains("error.stack")
        || lower.contains("e.message")
        || lower.contains("e.stack")
        || lower.contains("error: err")
        || lower.contains("error: e")
        || lower.contains("error: error")
        || lower.contains("message: err")
        || lower.contains("message: e")
        || lower.contains("detail: err")
        || lower.contains("str(e)")
        || lower.contains("traceback")
        || lower.contains("stacktrace");
    // Safe: logs the error server-side instead.
    let logs_server_side = lower.contains("logger.error")
        || lower.contains("console.error")
        || lower.contains("log.error")
        || lower.contains("log.errorf")
        || lower.contains("print(e)")
        || lower.contains("logging.error");
    if raw_error && !logs_server_side {
        return Decision::block(
            "UD-ARCH-045",
            format!(
                "UmaDev: error details leaked to client (UD-ARCH-045). \
                 `{file_path}` returns raw error messages/stack traces in the \
                 API response — this leaks internal structure (file paths, SQL, \
                 versions) that helps attackers. Return a generic message: \
                 `return res.json({{ error: 'Internal error' }}, {{ status: 500 }})` \
                 and log the full error server-side with `logger.error(e)`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-046**: require clickjacking protection (X-Frame-Options / CSP frame-ancestors).
///
/// A web server that serves HTML must set `X-Frame-Options: DENY` (or CSP
/// `frame-ancestors`) so the page can't be embedded in an invisible iframe
/// and clicked by an attacker (clickjacking). Distinct from UD-ARCH-019
/// (helmet) — this is a focused check for the clickjacking-specific header,
/// catching servers that have helmet but disabled the frameGuard option.
/// Flags HTML-serving servers without any frame protection.
#[must_use]
pub fn check_clickjacking_protection(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let is_server = matches!(ext.as_str(), "ts" | "js")
        && (content.contains("app.listen")
            || content.contains("createServer")
            || content.contains("app.use"));
    let is_html = ext == "html" || name == "_document.tsx" || name == "index.html";
    if !is_server && !is_html {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_frame_protection = lower.contains("x-frame-options")
        || lower.contains("frame-ancestors")
        || lower.contains("frameguard")
        || (lower.contains("helmet") && !lower.contains("frameguard: false"));
    if !has_frame_protection {
        return Decision::block(
            "UD-ARCH-046",
            format!(
                "UmaDev: missing clickjacking protection (UD-ARCH-046). \
                 `{file_path}` serves web content but sets no X-Frame-Options \
                 or CSP frame-ancestors header — the page can be embedded in \
                 an invisible iframe and clickjacked. Add: \
                 `res.setHeader('X-Frame-Options', 'DENY')` or ensure \
                 `helmet()` is active (it sets this by default).",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-023**: ban insecure TLS/SSL configuration (OWASP A02).
///
/// Setting `rejectUnauthorized` to false, zeroing Node's TLS verification
/// environment flag, and disabling SSL verification enable MITM attacks.
/// Flags these in backend source, including empty certificate-identity callbacks.
#[must_use]
pub fn check_insecure_tls(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "java" | "rs"
    ) {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let insecure_patterns = [
        concat!("rejectunauthorized:", " false"),
        concat!("rejectunauthorized:", "false"),
        concat!("reject_unauthorized =", " false"),
        concat!("reject_unauthorized=", "false"),
        concat!("node_tls_reject_", "unauthorized"),
        concat!("ssl verify=", "false"),
        concat!("sslverify=", "false"),
        concat!("verify_mode = ssl_verify_", "none"),
        concat!("insecure:", " true"),
        concat!("insecure=", "true"),
        concat!("checkserveridentity:", " () =>"),
        concat!("checkserveridentity:", "()=>"),
        concat!("cert_check=", "off"),
        concat!("ssl_check=", "false"),
        concat!("tls.check=", "false"),
    ];
    for pat in insecure_patterns {
        if lower.contains(pat) {
            return Decision::block(
                "UD-SEC-023",
                format!(
                    "UmaDev: insecure TLS configuration (UD-SEC-023). \
                     `{file_path}` disables certificate verification (`{pat}`) — \
                     this allows man-in-the-middle attacks on HTTPS connections. \
                     Enable cert verification: `rejectUnauthorized: true` (Node), \
                     `verify_mode: ssl.CERT_REQUIRED` (Python), or the default \
                     secure settings. Add internal CA certs to the trust store \
                     instead of disabling verification.",
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-ARCH-047**: require CSRF protection on state-changing endpoints.
///
/// POST/PUT/DELETE endpoints must have CSRF middleware (`csurf`/`csrf()`/
/// `SameSite` cookies/`X-CSRF-Token`). Flags state-changing handlers with no
/// CSRF protection. Conservative: only flags form-submit endpoints (not
/// JSON-API with SameSite cookie, which is inherently CSRF-protected).
#[must_use]
pub fn check_csrf_protection(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    let is_server = matches!(ext.as_str(), "ts" | "js")
        && (content.contains("app.post(")
            || content.contains("app.put(")
            || content.contains("app.delete(")
            || content.contains("export async function POST")
            || content.contains("export async function PUT")
            || content.contains("export async function DELETE"));
    if !is_server {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Safe: has CSRF protection.
    let has_csrf = lower.contains("csurf")
        || lower.contains("csrf(")
        || lower.contains("csrfmiddleware")
        || lower.contains("x-csrf-token")
        || lower.contains("x-xsrf-token")
        || lower.contains("samesite")
        || lower.contains("same_site")
        || lower.contains("protect_from_forgery")
        || lower.contains("@csrf")
        || lower.contains("verifier(")
        || lower.contains("antiforgery");
    if !has_csrf {
        return Decision::block(
            "UD-ARCH-047",
            format!(
                "UmaDev: missing CSRF protection (UD-ARCH-047). \
                 `{file_path}` has state-changing endpoints (POST/PUT/DELETE) \
                 but no CSRF middleware. A cross-site form POST can trigger \
                 actions on behalf of a logged-in user. Add `csurf()` middleware \
                 (Express), `SameSite` cookie, or a `X-CSRF-Token` header check.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-048**: detect GraphQL N+1 query risk (missing DataLoader).
///
/// A GraphQL resolver that does a DB query inside a list field resolver
/// triggers N+1 queries (one per item). Must use DataLoader to batch.
/// Flags GraphQL resolver files (`resolver.ts`/`schema.ts`) that do DB
/// queries (`prisma.`/`db.`/`findMany`/`findOne`) without `DataLoader`.
#[must_use]
pub fn check_graphql_n_plus_1(file_path: &str, content: &str) -> Decision {
    let name = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let is_graphql = name.contains("resolver")
        || name.contains("schema")
        || content.contains("@Resolver")
        || content.contains("graphql")
        || content.contains("GraphQL");
    if !is_graphql {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must do per-item DB queries inside a resolver.
    let has_db_query = lower.contains("prisma.")
        || lower.contains("db.query")
        || lower.contains("db.find")
        || lower.contains("findmany")
        || lower.contains("findone")
        || lower.contains("findunique")
        || lower.contains("collection.find")
        || lower.contains("model.findby");
    if !has_db_query {
        return Decision::pass();
    }
    // Safe: uses DataLoader or batching.
    let has_dataloader = lower.contains("dataloader")
        || lower.contains("data_loader")
        || lower.contains("batchload")
        || lower.contains("batch(")
        || lower.contains("include:")  // Prisma include/eager loading
        || lower.contains("include(")
        || lower.contains("select_related")
        || lower.contains("prefetch");
    if !has_dataloader {
        return Decision::block(
            "UD-ARCH-048",
            format!(
                "UmaDev: GraphQL N+1 query risk (UD-ARCH-048). \
                 `{file_path}` has a GraphQL resolver that does DB queries \
                 without DataLoader or eager loading — each item in a list \
                 triggers a separate query (N+1). Use DataLoader to batch \
                 requests, or `include`/`select_related`/`prefetch` for eager \
                 loading.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-049**: require GraphQL query depth/complexity limits (DoS prevention).
///
/// A public GraphQL endpoint without a max-depth or max-complexity limit
/// is vulnerable to nested-query DoS — an attacker sends a deeply nested
/// query that exhausts server resources. Flags GraphQL server setup
/// (`ApolloServer`/`graphqlHTTP`/`makeExecutableSchema`) without
/// `maxDepth`/`depthLimit`/`costAnalysis`/`validationRules`.
#[must_use]
pub fn check_graphql_depth_limit(file_path: &str, content: &str) -> Decision {
    let lower = content.to_ascii_lowercase();
    let has_gql_server = lower.contains("apolloserver")
        || lower.contains("new apolloserver")
        || lower.contains("graphqlhttp")
        || lower.contains("graphql-server")
        || lower.contains("makeexecutableschema")
        || lower.contains("graphqlmiddleware");
    if !has_gql_server {
        return Decision::pass();
    }
    let has_depth_limit = lower.contains("maxdepth")
        || lower.contains("depthlimit")
        || lower.contains("depth_limit")
        || lower.contains("costanalysis")
        || lower.contains("cost_analysis")
        || lower.contains("validationrules")
        || lower.contains("maxcomplexity")
        || lower.contains("max_complexity")
        || lower.contains("rate limit")
        || lower.contains("ratelimit");
    if !has_depth_limit {
        return Decision::block(
            "UD-ARCH-049",
            format!(
                "UmaDev: GraphQL server missing depth/complexity limit (UD-ARCH-049). \
                 `{file_path}` sets up a GraphQL server without `maxDepth` or \
                 complexity analysis — an attacker can send deeply nested queries \
                 to exhaust resources (DoS). Add `validationRules: [depthLimit(10)]` \
                 or use `graphql-cost-analysis` middleware.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-024**: ban GraphQL introspection enabled in production.
///
/// Introspection exposes the full schema (all types/fields/mutations) to
/// anyone — an attacker maps your API for vulnerabilities. Must disable in
/// production (`introspection: false` when `NODE_ENV=production`). Flags
/// ApolloServer configs that explicitly enable introspection or don't disable
/// it when `production` is mentioned.
#[must_use]
pub fn check_graphql_introspection(file_path: &str, content: &str) -> Decision {
    let lower = content.to_ascii_lowercase();
    let has_gql_server = lower.contains("apolloserver") || lower.contains("graphqlhttp");
    if !has_gql_server {
        return Decision::pass();
    }
    // Is this a production context?
    let is_production = lower.contains("production")
        || lower.contains("node_env")
        || lower.contains("process.env.node_env");
    // Explicitly enabled introspection.
    let introspection_on = lower.contains(concat!("introspection:", " true"))
        || lower.contains(concat!("introspection:", "true"));
    // Explicitly disabled (safe).
    let introspection_off = lower.contains(concat!("introspection:", " false"))
        || lower.contains(concat!("introspection:", "false"));
    // Block: production + introspection on, OR production + no explicit disable.
    if is_production && (introspection_on || !introspection_off) {
        return Decision::block(
            "UD-SEC-024",
            format!(
                "UmaDev: GraphQL introspection exposed in production (UD-SEC-024). \
                 `{file_path}` runs a GraphQL server in production without disabling \
                 introspection — the full schema is visible to attackers. Set \
                 `introspection: process.env.NODE_ENV !== 'production'` in the \
                 ApolloServer config.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-050**: require authentication on WebSocket connections.
///
/// A WebSocket server (`ws.WebSocketServer`/`socket.io`/`new WebSocketServer`)
/// without an auth check on `connection` lets anyone connect and receive
/// real-time data. Must verify the token/session before accepting the
/// connection. Flags WS servers with no `verifyClient`/`auth`/token check.
#[must_use]
pub fn check_websocket_auth(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_ws = lower.contains("websocketserver")
        || lower.contains("new ws(")
        || lower.contains("socket.io")
        || lower.contains("io.on(")
        || lower.contains(".on('connection')")
        || lower.contains(".on(\"connection\")")
        || lower.contains("wss.on(");
    if !has_ws {
        return Decision::pass();
    }
    let has_auth = lower.contains("verifyclient")
        || lower.contains("verify_client")
        || lower.contains("use(require")
        || lower.contains("io.use(")
        || lower.contains("socket.handshake")
        || lower.contains("socket.request")
        || lower.contains("authtoken")
        || lower.contains("auth_token")
        || lower.contains("authorization")
        || lower.contains("cookie")
        || lower.contains("session")
        || lower.contains("verifytoken")
        || lower.contains("jwt.verify");
    if !has_auth {
        return Decision::block(
            "UD-ARCH-050",
            format!(
                "UmaDev: WebSocket server without auth (UD-ARCH-050). \
                 `{file_path}` creates a WebSocket server but has no auth check \
                 on incoming connections — anyone can connect and receive data. \
                 Add `verifyClient` (ws) or `io.use()` middleware (socket.io) \
                 that checks the auth token before accepting the connection.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-026**: ban server-side env secrets leaked into client bundles.
///
/// `process.env.SECRET_KEY` / `process.env.DATABASE_URL` in frontend code
/// (`.tsx`/`.jsx`/`.vue`) gets bundled into the client-side JS — anyone can
/// read it from the browser. Only `NEXT_PUBLIC_*` / `VITE_*` prefixed vars
/// are safe for client. Flags sensitive env var access in frontend files.
#[must_use]
pub fn check_client_secret_leak(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "jsx" | "tsx" | "vue" | "svelte" | "html") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Sensitive env var names that must never reach the client.
    let sensitive_env = [
        "process.env.secret",
        "process.env.database_url",
        "process.env.db_url",
        "process.env.private_key",
        "process.env.api_key",
        "process.env.jwt_secret",
        "process.env.stripe",
        "process.env.aws_secret",
        "process.env.password",
        "process.env.token",
        "process.env.redis",
    ];
    for pattern in sensitive_env {
        if lower.contains(pattern) {
            return Decision::block(
                "UD-SEC-026",
                format!(
                    "UmaDev: server secret leaked into client bundle (UD-SEC-026). \
                     `{file_path}` accesses `{pattern}` in frontend code — this \
                     gets bundled into the browser JS where anyone can read it. \
                     Only `NEXT_PUBLIC_*` / `VITE_*` prefixed vars are safe for \
                     client. Move the secret to a server-side API route.",
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-SEC-027**: ban sensitive data in localStorage / sessionStorage.
///
/// Storing tokens, passwords, or API keys in `localStorage` /
/// `sessionStorage` exposes them to XSS — any script on the page can read
/// them. Must use HttpOnly cookies for session tokens. Flags
/// `localStorage.setItem` / `sessionStorage.setItem` with sensitive key
/// names. Runs on frontend source.
#[must_use]
pub fn check_insecure_storage(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "jsx" | "tsx" | "vue" | "svelte" | "html") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_storage = lower.contains("localstorage.setitem")
        || lower.contains("sessionstorage.setitem")
        || lower.contains("localstorage[")
        || lower.contains("sessionstorage[");
    if !has_storage {
        return Decision::pass();
    }
    // Sensitive key names that must never be in client storage.
    for key in SENSITIVE_STORAGE_KEYS {
        if lower.contains(key) {
            return Decision::block(
                "UD-SEC-027",
                format!(
                    "UmaDev: sensitive data in client storage (UD-SEC-027). \
                     `{file_path}` stores `{key}` in localStorage/sessionStorage — \
                     any XSS script can read it. Use HttpOnly Secure cookies for \
                     session tokens. For non-sensitive UI state, localStorage is fine.",
                ),
            );
        }
    }
    Decision::pass()
}

/// Sensitive keys that must never be stored in client-side storage.
const SENSITIVE_STORAGE_KEYS: &[&str] = &[
    "\"token\"",
    "'token'",
    "\"access_token\"",
    "'access_token'",
    "\"refresh_token\"",
    "\"password\"",
    "\"secret\"",
    "\"api_key\"",
    "\"apikey\"",
    "\"private_key\"",
    "\"jwt\"",
    "\"sessionid\"",
    "\"session_id\"",
    "\"auth\"",
    "\"credential\"",
];

/// **UD-ARCH-054**: ban unhandled promise rejections from fetch/async calls.
///
/// `await fetch()` without try/catch causes an unhandled promise rejection
/// on network failure — the app crashes or shows a blank screen. Every
/// `await fetch` / `await axios` must be in a try/catch or have a `.catch()`.
/// Flags async HTTP calls without error handling. Runs on TS/JS.
#[must_use]
pub fn check_unhandled_fetch_error(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "tsx" | "js" | "jsx") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must have `await fetch(` or `await axios.` — an unguarded HTTP call.
    let has_fetch = lower.contains("await fetch(")
        || lower.contains("await axios.get(")
        || lower.contains("await axios.post(")
        || lower.contains("await axios.put(")
        || lower.contains("await axios.delete(")
        || lower.contains("await axios.patch(");
    if !has_fetch {
        return Decision::pass();
    }
    // Safe: has try/catch or .catch() anywhere in the file.
    let has_error_handling = lower.contains("try")
        || lower.contains("catch")
        || lower.contains(".catch(")
        || lower.contains(".catch (");
    if !has_error_handling {
        return Decision::block(
            "UD-ARCH-054",
            format!(
                "UmaDev: unhandled fetch error (UD-ARCH-054). \
                 `{file_path}` uses `await fetch()` without try/catch — a network \
                 failure causes an unhandled promise rejection and app crash. \
                 Wrap in try/catch: `try {{ const r = await fetch(url); }} \
                 catch (e) {{ setError(e.message); }}`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-055**: require React `key` prop on list-rendered elements.
///
/// Rendering a list without `key` (`.map()` returning JSX without `key=`)
/// causes React to inefficiently re-render and can corrupt state. Every
/// element in a `.map()` must have a unique `key`. Flags `.map(` patterns
/// in JSX that don't include `key=`. Runs on `.tsx`/`.jsx`.
#[must_use]
pub fn check_react_list_key(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "tsx" | "jsx") {
        return Decision::pass();
    }
    let code = crate::tokenizer::Tokenized::new(content).code_only(content);
    let mut hits = 0usize;
    let lines: Vec<&str> = code.lines().collect();
    for i in 0..lines.len() {
        let line = lines[i];
        // Look for `.map(` which returns JSX (has `<` on the same or next line).
        let has_map = line.contains(".map(") || line.contains(".map (");
        if !has_map {
            continue;
        }
        // A formatted callback often puts `key` several lines after `.map(`.
        // Keep the window bounded, and only require a key when that callback
        // actually contains a JSX opening tag; data-only maps need no key.
        let window: String = lines[i..]
            .iter()
            .take(20)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        if contains_jsx_open_tag(&window) && !window.contains("key=") && !window.contains("key =") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-055",
            format!(
                "UmaDev: React list render without key (UD-ARCH-055). \
                 `{file_path}` has {hits} `.map()` call(s) returning JSX \
                 without a `key` prop — React needs a unique key per item for \
                 efficient re-rendering. Add `key={{item.id}}` to each \
                 rendered element.",
            ),
        )
    } else {
        Decision::pass()
    }
}

fn contains_jsx_open_tag(content: &str) -> bool {
    let bytes = content.as_bytes();
    bytes
        .windows(2)
        .any(|pair| pair[0] == b'<' && pair[1].is_ascii_alphabetic())
}

/// **UG-LINT-005**: ban inline event-handler functions in JSX (performance).
///
/// `onClick={() => handleClick()}` creates a new function every render,
/// causing unnecessary child re-renders. Must use `useCallback` or extract
/// the handler. Conservative: only flags when there are more than 3 inline
/// arrow functions in JSX (a single one is acceptable for small components).
#[must_use]
pub fn check_inline_event_handlers(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "tsx" | "jsx") {
        return Decision::pass();
    }
    // Inline closures on native DOM nodes are idiomatic and do not by
    // themselves trigger child re-renders. The performance smell applies to
    // custom components, especially memoized ones, whose handler prop identity
    // changes on every parent render.
    let mut hits = 0usize;
    let mut in_custom_component = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if contains_custom_component_open(trimmed) {
            in_custom_component = true;
        }
        if in_custom_component
            && (line.contains("onClick={()")
                || line.contains("onChange={()")
                || line.contains("onSubmit={()")
                || line.contains("onKeyPress={()")
                || line.contains("onKeyDown={()")
                || line.contains("onMouseEnter={()")
                || line.contains("onMouseLeave={()")
                || line.contains("onFocus={()")
                || line.contains("onBlur={()"))
        {
            hits += 1;
        }
        if in_custom_component && (trimmed.ends_with('>') || trimmed.ends_with("/>")) {
            in_custom_component = false;
        }
    }
    if hits > 3 {
        Decision::block(
            "UG-LINT-005",
            format!(
                "UmaDev: too many inline event handlers (UG-LINT-005). \
                 `{file_path}` has {hits} inline arrow functions in JSX event \
                 handlers — each creates a new function every render, causing \
                 unnecessary child re-renders. Wrap with `useCallback`: \
                 `const handleClick = useCallback(() => {{...}}, [deps])`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

fn contains_custom_component_open(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes
        .windows(2)
        .any(|pair| pair[0] == b'<' && pair[1].is_ascii_uppercase())
}

/// **UD-ARCH-056**: require cleanup in useEffect with subscriptions/timers.
///
/// A `useEffect` that adds an event listener, setInterval, setTimeout, or
/// WebSocket without returning a cleanup function leaks memory — the listener
/// persists after unmount. Flags effects that set up subscriptions/timers but
/// don't `return () =>`. Runs on `.tsx`/`.jsx`.
#[must_use]
pub fn check_use_effect_cleanup(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "tsx" | "jsx") {
        return Decision::pass();
    }
    // Find useEffect blocks that set up subscriptions/timers.
    let lower = content.to_ascii_lowercase();
    let has_effect = lower.contains("useeffect(") || lower.contains("useeffect (");
    if !has_effect {
        return Decision::pass();
    }
    // Effect body sets up something that needs cleanup.
    let needs_cleanup = lower.contains("addeventlistener")
        || lower.contains("setinterval")
        || lower.contains("settimeout")
        || lower.contains("new websocket")
        || lower.contains("new eventsource")
        || lower.contains("subscribe(")
        || lower.contains("addeventlistener(");
    if !needs_cleanup {
        return Decision::pass();
    }
    // Has a cleanup return?
    let has_cleanup = lower.contains("return () =>")
        || lower.contains("return () =>{")
        || lower.contains("return function")
        || lower.contains("return() =>");
    if !has_cleanup {
        return Decision::block(
            "UD-ARCH-056",
            format!(
                "UmaDev: useEffect missing cleanup (UD-ARCH-056). \
                 `{file_path}` has a useEffect that sets up a subscription/timer \
                 without returning a cleanup function — the listener persists \
                 after unmount, leaking memory. Add `return () => \
                 clearInterval(id)` / `removeEventListener(...)` etc.",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-006**: ban direct mutation of React state (immutability violation).
///
/// `state.push(x)` / `state.name = x` mutates the existing object — React
/// won't detect the change (stale UI). Must create a new object:
/// `setState([...state, x])` / `setState({...state, name: x})`. Flags `.push(`
/// `.pop(` `.splice(` `.sort(` directly on state variables in TSX/JSX.
#[must_use]
pub fn check_state_mutation(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "tsx" | "jsx") {
        return Decision::pass();
    }
    let code = crate::tokenizer::Tokenized::new(content).code_only(content);
    let state_names = react_state_binding_regex()
        .captures_iter(&code)
        .filter_map(|captures| captures.get(1).map(|value| value.as_str().to_string()))
        .collect::<HashSet<_>>();
    if state_names.is_empty() {
        return Decision::pass();
    }

    let mut hits = 0usize;
    for line in code.lines() {
        for method in [
            ".push(",
            ".pop(",
            ".splice(",
            ".shift(",
            ".unshift(",
            ".sort(",
            ".reverse(",
        ] {
            let mut cursor = 0usize;
            while let Some(relative) = line[cursor..].find(method) {
                let method_start = cursor + relative;
                let receiver = mutation_receiver_root(&line[..method_start]);
                if receiver.is_some_and(|root| state_names.contains(root)) {
                    hits += 1;
                }
                cursor = method_start + method.len();
            }
        }
    }
    if hits > 0 {
        Decision::block(
            "UG-LINT-006",
            format!(
                "UmaDev: direct state mutation (UG-LINT-006). \
                 `{file_path}` mutates state directly ({hits}x) — React can't \
                 detect the change and the UI won't update. Create a new \
                 array/object: `setState([...items, newItem])` / \
                 `setState({{...user, name: x}})` instead of `.push()` / \
                 property assignment.",
            ),
        )
    } else {
        Decision::pass()
    }
}

fn react_state_binding_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)\[\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*,\s*[A-Za-z_$][A-Za-z0-9_$]*\s*\]\s*=\s*(?:React\.)?useState",
        )
        .expect("valid React state binding regex")
    })
}

fn mutation_receiver_root(prefix: &str) -> Option<&str> {
    let start = prefix
        .char_indices()
        .rev()
        .find(|(_, ch)| !(ch.is_alphanumeric() || matches!(ch, '_' | '$' | '.')))
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    prefix[start..]
        .split('.')
        .next()
        .filter(|value| !value.is_empty())
}

/// **UD-ARCH-057**: ban insecure redirect chains (open redirect via Referer).
///
/// Using `Referer` / `referrer` header for redirects is insecure — an
/// attacker can spoof it. Also flags redirects that chain through multiple
/// user-controlled hops. Flags `req.headers.referer` used as redirect target.
#[must_use]
pub fn check_referrer_redirect(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_redirect = lower.contains("redirect(") || lower.contains("location:");
    let uses_referrer =
        lower.contains("referer") || lower.contains("referrer") || lower.contains("referrer");
    if has_redirect && uses_referrer {
        let has_guard = lower.contains("allowlist")
            || lower.contains("whitelist")
            || lower.contains("validate")
            || lower.contains("startswith");
        if !has_guard {
            return Decision::block(
                "UD-ARCH-057",
                format!(
                    "UmaDev: redirect using Referer header (UD-ARCH-057). \
                     `{file_path}` uses the Referer/referrer header as a redirect \
                     target — it's client-controlled and spoofable. Use a server-\
                     side validated URL or a query param checked against an \
                     allowlist instead.",
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-SEC-028**: ban `dangerouslySetInnerHTML` / `v-html` / `innerHTML` with
/// dynamic content (XSS).
///
/// Setting HTML from a variable bypasses React/Vue's escaping — if the
/// variable contains user input, it's an XSS vector. Flags these without a
/// sanitization call (`DOMPurify`/`sanitize`/`escapeHtml`).
#[must_use]
pub fn check_dangerous_inner_html(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "tsx" | "jsx" | "vue" | "js" | "ts" | "html") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_dangerous = lower.contains("dangerouslysetinnerhtml")
        || lower.contains("v-html")
        || lower.contains("innerhtml");
    if !has_dangerous {
        return Decision::pass();
    }
    let has_sanitizer = lower.contains("dompurify")
        || lower.contains("sanitize")
        || lower.contains("escapehtml")
        || lower.contains("escape(")
        || lower.contains("xss")
        || lower.contains("isSafe");
    if !has_sanitizer {
        return Decision::block(
            "UD-SEC-028",
            format!(
                "UmaDev: dynamic HTML injection risk (UD-SEC-028). \
                 `{file_path}` uses dangerouslySetInnerHTML/innerHTML/v-html \
                 without sanitization — if the content is user-influenced it's \
                 an XSS vector. Sanize first: `DOMPurify.sanitize(html)` or \
                 use a text node instead of HTML.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-029**: ban prototype pollution — merging user input into objects.
///
/// `Object.assign({}, req.body)` / `{...req.body}` / `lodash.merge({}, input)`
/// with user input can inject `__proto__` properties, polluting every object
/// in the app (prototype pollution). Must sanitize keys before merging.
/// Flags merge/assign calls with user input that don't sanitize `__proto__`.
#[must_use]
pub fn check_prototype_pollution(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_merge = lower.contains("object.assign(")
        || lower.contains("lodash.merge")
        || lower.contains("lodash.set")
        || lower.contains("_.merge(")
        || lower.contains("_.set(")
        || lower.contains("deepmerge(")
        || lower.contains("{...req.body")
        || lower.contains("{...req.query");
    if !has_merge {
        return Decision::pass();
    }
    let has_sanitizer = lower.contains("sanitize")
        || lower.contains("__proto__")
        || lower.contains("hasownproperty")
        || lower.contains("object.create(null")
        || lower.contains("map(")
        || lower.contains("filter(");
    if !has_sanitizer {
        return Decision::block(
            "UD-SEC-029",
            format!(
                "UmaDev: prototype pollution risk (UD-SEC-029). \
                 `{file_path}` merges user input into objects without sanitizing \
                 `__proto__` — an attacker can pollute every object prototype. \
                 Filter keys before merging: `Object.fromEntries(Object.entries(\
                 input).filter(([k]) => !k.startsWith('__')))` or use \
                 `Object.create(null)`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-030**: ban insecure JSONP — callback parameter injection.
///
/// JSONP endpoints (`?callback=`) execute the callback name as JavaScript.
/// If the callback name is user-supplied and not sanitized, it's a script-
/// injection vector. Flags JSONP patterns (`callback(` in response,
/// `res.jsonp`, `jsonp()`) without callback-name validation.
#[must_use]
pub fn check_insecure_jsonp(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_jsonp = lower.contains("res.jsonp(")
        || lower.contains(".jsonp(")
        || lower.contains("callback(")
        || lower.contains("jsonpcallback")
        || lower.contains("jsonp");
    if !has_jsonp {
        return Decision::pass();
    }
    let has_validation = lower.contains("replace(")
        || lower.contains("regex")
        || lower.contains("match(")
        || lower.contains("isvalid")
        || lower.contains("sanitize")
        || lower.contains("allowlist")
        || lower.contains("whitelist");
    if !has_validation {
        return Decision::block(
            "UD-SEC-030",
            format!(
                "UmaDev: insecure JSONP callback (UD-SEC-030). \
                 `{file_path}` uses JSONP with a user-supplied callback name \
                 without validation — an attacker can inject arbitrary script. \
                 Validate the callback name against `^[a-zA-Z_$][0-9a-zA-Z_$]*$` \
                 before using it, or switch to CORS + fetch (JSONP is deprecated).",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-007**: ban `import *` — breaks tree-shaking (bundle bloat).
///
/// `import * as utils from './utils'` prevents bundlers from removing unused
/// exports, bloating the bundle. Must use named imports:
/// `import { formatDate } from './utils'`. Conservative: only flags when
/// there are more than 2 wildcard imports. Runs on TS/JS.
#[must_use]
pub fn check_wildcard_imports(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "tsx" | "jsx") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        if line.contains("import * as") || line.contains("import * from") {
            hits += 1;
        }
    }
    if hits > 2 {
        Decision::block(
            "UG-LINT-007",
            format!(
                "UmaDev: wildcard imports break tree-shaking (UG-LINT-007). \
                 `{file_path}` has {hits} `import *` statements — this prevents \
                 bundlers from removing unused code, bloating the bundle. Use \
                 named imports: `import {{ formatDate, parseDate }} from './utils'`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UG-LINT-009**: ban loose equality `==` / `!=` (use `===` / `!==`).
///
/// `==` performs type coercion (`0 == ""` is `true`, `null == undefined` is
/// `true`) — this causes subtle bugs. Commercial code must use strict
/// equality `===`/`!==`. Conservative: only flags when there are more than 3
/// loose-equality comparisons. Runs on JS/TS. Excludes `==` in JSDoc/comments.
#[must_use]
pub fn check_loose_equality(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "jsx" | "tsx") {
        return Decision::pass();
    }
    if file_path.contains(".test.") || file_path.contains(".spec.") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        let no_str = strip_string_literals(line);
        hits += count_loose_equality_operators(&no_str);
    }
    if hits > 3 {
        Decision::block(
            "UG-LINT-009",
            format!(
                "UmaDev: loose equality banned (UG-LINT-009). \
                 `{file_path}` uses `==`/`!=` ({hits}x) — these perform type \
                 coercion causing subtle bugs (`0 == ''` is true). Use strict \
                 equality `===`/`!==` instead.",
            ),
        )
    } else {
        Decision::pass()
    }
}

fn count_loose_equality_operators(code: &str) -> usize {
    let bytes = code.as_bytes();
    let mut hits = 0usize;
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        if bytes[index] == b'=' && bytes[index + 1] == b'=' {
            let strict = bytes.get(index + 2) == Some(&b'=')
                || index.checked_sub(1).and_then(|i| bytes.get(i)) == Some(&b'=')
                || index.checked_sub(1).and_then(|i| bytes.get(i)) == Some(&b'!');
            if !strict {
                hits += 1;
            }
            index += if bytes.get(index + 2) == Some(&b'=') {
                3
            } else {
                2
            };
            continue;
        }
        if bytes[index] == b'!' && bytes[index + 1] == b'=' && bytes.get(index + 2) != Some(&b'=') {
            hits += 1;
            index += 2;
            continue;
        }
        index += 1;
    }
    hits
}

/// **UD-ARCH-058**: ban empty dependency arrays in React hooks (stale closure).
///
/// `useEffect(() => { ... }, [])` with an empty deps array when the callback
/// uses state/props captures stale values. Conversely, `useEffect(() => {}, [a, b, c])`
/// with every variable listed causes infinite loops. Flags useEffect/useMemo/
/// useCallback with an empty `[]` dependency array when the body references
/// external variables. Runs on `.tsx`/`.jsx`.
#[must_use]
pub fn check_empty_deps_array(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "tsx" | "jsx") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    // Find lines with `useEffect(`/`useMemo(`/`useCallback(` that have `[]`
    // as the dependency array on the same or next line.
    let lines: Vec<&str> = content.lines().collect();
    for i in 0..lines.len() {
        let line = lines[i];
        let has_hook = line.contains("useEffect(")
            || line.contains("useMemo(")
            || line.contains("useCallback(");
        if !has_hook {
            continue;
        }
        // Check current + next 3 lines for `, [])` — empty deps array.
        let window: String = lines[i..]
            .iter()
            .take(4)
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        if window.contains(", [])") || window.contains(",[])") {
            // Does the hook body reference variables? If it's just
            // `console.log('mount')` with no external refs, [] is fine.
            // Heuristic: if the hook body is short (< 50 chars) and has no
            // function calls or variable refs, skip.
            let body: String = lines[i..]
                .iter()
                .take(4)
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            // If body has `req.` / `data` / `user` / `state` etc → likely stale.
            if body.contains("state")
                || body.contains("props")
                || body.contains("user")
                || body.contains("data")
                || body.contains("count")
                || body.contains("items")
                || body.contains("form")
                || body.contains("value")
            {
                hits += 1;
            }
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-058",
            format!(
                "UmaDev: empty deps array with state references (UD-ARCH-058). \
                 `{file_path}` has a React hook with `[]` deps that references \
                 state/props — the callback captures stale values on re-render. \
                 List the dependencies: `useEffect(() => {{...}}, [userId, data])`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-SEC-031**: ban reading auth tokens from `document.cookie` (XSS theft).
///
/// Accessing `document.cookie` from client code means the cookie is NOT
/// HttpOnly — any XSS script can steal it. Session tokens must be in
/// HttpOnly cookies (inaccessible to JS). Flags `document.cookie` reads in
/// frontend files. Conservative: only flags in `.tsx`/`.jsx`/`.vue`/`.svelte`
/// (clearly frontend), not `.ts`/`.js` (could be SSR).
#[must_use]
pub fn check_document_cookie_access(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "tsx" | "jsx" | "vue" | "svelte" | "html") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    if lower.contains("document.cookie") {
        return Decision::block(
            "UD-SEC-031",
            format!(
                "UmaDev: document.cookie access in frontend (UD-SEC-031). \
                 `{file_path}` reads `document.cookie` — this means session \
                 cookies are not HttpOnly, so any XSS can steal them. Set \
                 cookies with `httpOnly: true` on the server and access them \
                 via server-side APIs. The client should never need to read \
                 auth cookies directly.",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-010**: ban untyped React component props (`.jsx` without TS).
///
/// A `.jsx` React component with `props` but no PropTypes or TS interface
/// has zero prop validation — typos and wrong types crash at runtime.
/// Commercial code must either use `.tsx` (TypeScript) or declare PropTypes.
/// Flags `.jsx` files that destructure `props` without `PropTypes` or
/// `interface`/`type` declarations.
#[must_use]
pub fn check_untyped_props(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if ext != "jsx" {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must accept props.
    let uses_props = lower.contains("props")
        && (lower.contains("({ props }") || lower.contains("(props)") || lower.contains("props."));
    if !uses_props {
        return Decision::pass();
    }
    // Has type declaration?
    let has_types = lower.contains("proptypes")
        || lower.contains("interface ")
        || lower.contains("type props")
        || lower.contains("fc<")
        || lower.contains("react.fc");
    if !has_types {
        return Decision::block(
            "UG-LINT-010",
            format!(
                "UmaDev: untyped component props (UG-LINT-010). \
                 `{file_path}` uses `props` without PropTypes or a TypeScript \
                 interface — wrong prop types crash at runtime with no warning. \
                 Either rename to `.tsx` and type the props: \
                 `type Props = {{ title: string }}`, or add \
                 `Component.propTypes = {{ title: PropTypes.string }}`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-059**: ban `window.open` without sanitization (popup injection).
///
/// `window.open(userUrl)` with unvalidated input can open malicious sites,
/// execute `javascript:` URIs, or create phishing popups. Must validate
/// the URL scheme (only `http:`/`https:`). Flags `window.open(` with a
/// variable argument in frontend files.
#[must_use]
pub fn check_unsafe_window_open(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "tsx" | "jsx" | "vue" | "svelte" | "js" | "ts" | "html"
    ) {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    if !lower.contains("window.open(") {
        return Decision::pass();
    }
    // Dynamic URL (variable, not a static string).
    let dynamic = lower.contains("window.open(url")
        || lower.contains("window.open(target")
        || lower.contains("window.open(link")
        || lower.contains("window.open(href")
        || lower.contains("window.open(redirect")
        || lower.contains("window.open(`")
        || lower.contains("window.open(${")
        || lower.contains("window.open(req.");
    let has_sanitizer = lower.contains("startswith")
        || lower.contains("includes(")
        || lower.contains("match(")
        || lower.contains("url.parse")
        || lower.contains("new url(")
        || lower.contains("isvalid")
        || lower.contains("allowlist");
    if dynamic && !has_sanitizer {
        return Decision::block(
            "UD-ARCH-059",
            format!(
                "UmaDev: window.open with unvalidated URL (UD-ARCH-059). \
                 `{file_path}` calls `window.open()` with a dynamic URL — an \
                 attacker can inject `javascript:` or phishing URLs. Validate \
                 the URL scheme before opening: \
                 `if (url.startsWith('http')) window.open(url)`.",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-011**: ban async operations directly in component render body.
///
/// Calling `fetch()` / `await` / async functions directly in a React
/// component body (not inside `useEffect`) executes on every render —
/// causing infinite loops and wasted network requests. Async side effects
/// must go in `useEffect`. Flags top-level `await fetch` / `await axios`
/// in `.tsx`/`.jsx` when not inside a `useEffect` block.
#[must_use]
pub fn check_render_side_effects(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "tsx" | "jsx") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Must have an async call outside useEffect.
    let has_async_call = lower.contains("await fetch(")
        || lower.contains("await axios")
        || lower.contains("await api.")
        || lower.contains("await db.");
    if !has_async_call {
        return Decision::pass();
    }
    // Check if ALL async calls are inside useEffect blocks.
    // Heuristic: if `useEffect` is present and the file is structured with
    // the async calls indented inside it, it's fine. If no useEffect at all,
    // the calls are in the render body.
    let has_use_effect = lower.contains("useeffect");
    if !has_use_effect {
        return Decision::block(
            "UG-LINT-011",
            format!(
                "UmaDev: async side effect in render body (UG-LINT-011). \
                 `{file_path}` calls async functions (`await fetch/axios`) in \
                 the component body without `useEffect` — this runs on every \
                 render, causing infinite loops. Move side effects into: \
                 `useEffect(() => {{ fetchData(); }}, [])`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-060**: ban Promise chains without `.catch()` (unhandled rejection).
///
/// A `.then()` chain without `.catch()` causes an unhandled promise rejection
/// on failure — crashing the process (Node) or silently swallowing errors
/// (browser). Every `.then()` chain must end with `.catch()`. Flags `.then(`
/// without a matching `.catch(` on the same or next line. Runs on JS/TS.
#[must_use]
pub fn check_promise_without_catch(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "tsx" | "jsx") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_then = lower.contains(".then(");
    if !has_then {
        return Decision::pass();
    }
    // Safe: has .catch( or await or async.
    let has_catch = lower.contains(".catch(")
        || lower.contains("await ")
        || lower.contains("try ")
        || lower.contains("try{");
    if !has_catch {
        return Decision::block(
            "UD-ARCH-060",
            format!(
                "UmaDev: Promise chain without .catch() (UD-ARCH-060). \
                 `{file_path}` has `.then()` chains with no `.catch()` — a \
                 rejection becomes an unhandled promise error. Add `.catch(err => \
                 console.error(err))` or convert to `async/await` with \
                 `try/catch`.",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-012**: ban mutable default export objects (should be frozen).
///
/// `export default { config, routes }` exports a mutable object — any importer
/// can accidentally modify it, causing cross-module state corruption. Must
/// use `Object.freeze()` or `as const`. Flags default-exported object
/// literals that aren't frozen. Runs on JS/TS.
#[must_use]
pub fn check_mutable_default_export(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_default_export_object =
        lower.contains("export default {") || lower.contains("export default{");
    if !has_default_export_object {
        return Decision::pass();
    }
    let is_frozen =
        lower.contains("object.freeze") || lower.contains("as const") || lower.contains("readonly");
    if !is_frozen {
        return Decision::block(
            "UG-LINT-012",
            format!(
                "UmaDev: mutable default export (UG-LINT-012). \
                 `{file_path}` exports a mutable object as default — importers \
                 can accidentally mutate it, causing cross-module corruption. \
                 Freeze it: `export default Object.freeze({{ config, routes }})` \
                 or `export default {{ config, routes }} as const`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-061**: ban `window.location` assignment from user input (open redirect via JS).
///
/// `window.location = userInput` redirects the page to an attacker-controlled
/// URL — same as server-side open redirect but client-side. Must validate
/// the URL scheme. Flags `window.location` assignment with a dynamic variable.
#[must_use]
pub fn check_client_redirect_injection(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "tsx" | "jsx" | "vue" | "svelte" | "js" | "ts" | "html"
    ) {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_redirect = lower.contains("window.location =")
        || lower.contains("window.location.href =")
        || lower.contains("window.location.assign(")
        || lower.contains("window.location.replace(")
        || lower.contains("location.href =");
    if !has_redirect {
        return Decision::pass();
    }
    let dynamic = lower.contains("window.location = url")
        || lower.contains("window.location = target")
        || lower.contains("window.location = redirect")
        || lower.contains("window.location.href = url")
        || lower.contains("window.location.href = req.")
        || lower.contains("window.location.href = `")
        || lower.contains("location.assign(url")
        || lower.contains("location.replace(url")
        || lower.contains("location.href = data");
    let has_guard = lower.contains("startswith")
        || lower.contains("includes(")
        || lower.contains("match(")
        || lower.contains("isvalid")
        || lower.contains("encodeuri");
    if dynamic && !has_guard {
        return Decision::block(
            "UD-ARCH-061",
            format!(
                "UmaDev: client-side redirect injection (UD-ARCH-061). \
                 `{file_path}` assigns `window.location` from a dynamic URL \
                 without validation — an attacker can craft a phishing redirect. \
                 Validate: `if (url.startsWith('https')) window.location = url`.",
            ),
        );
    }
    Decision::pass()
}

/// **UG-LINT-013**: ban `new Date()` without validation in date-parsing.
///
/// `new Date(userInput)` with arbitrary strings produces `Invalid Date` silently
/// — downstream code crashes when it tries to use the date. Must validate
/// `isNaN(date.getTime())` before use. Flags `new Date(` with a variable
/// argument in backend code. Conservative: only flags when no `isNaN` guard.
#[must_use]
pub fn check_unsafe_date_parse(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "tsx" | "jsx") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_new_date = lower.contains("new date(") && !lower.contains("new date()");
    if !has_new_date {
        return Decision::pass();
    }
    let has_guard = lower.contains("isnan")
        || lower.contains("isfinite")
        || lower.contains("isvalid")
        || lower.contains("date.isvalid")
        || lower.contains("try")
        || lower.contains("|| new date")
        || lower.contains("?? new date");
    if !has_guard {
        return Decision::block(
            "UG-LINT-013",
            format!(
                "UmaDev: unsafe Date parse without validation (UG-LINT-013). \
                 `{file_path}` parses `new Date(variable)` without checking \
                 validity — invalid input produces `Invalid Date` silently, \
                 crashing downstream code. Validate: \
                 `const d = new Date(input); if (isNaN(d.getTime())) throw ...`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-062**: ban `parseInt` / `parseFloat` without radix / validation.
///
/// `parseInt("08")` returns `0` (not `8`) in old engines without radix.
/// `parseFloat(userInput)` returns `NaN` silently. Both must specify radix
/// (`parseInt(x, 10)`) and check `isNaN` for `parseFloat`. Flags
/// `parseInt(` without `, 10)` radix and `parseFloat(` without `isNaN` guard.
#[must_use]
pub fn check_unsafe_parse(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "tsx" | "jsx") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // parseInt without radix.
    if lower.contains("parseint(") && !lower.contains("parseint(") {
        // never reached — just check below
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let ll = line.to_ascii_lowercase();
        let trimmed = ll.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        // `parseInt(x)` without `, 10)` radix.
        if ll.contains("parseint(")
            && !ll.contains(", 10)")
            && !ll.contains(",10)")
            && !ll.contains(", 0x")
        {
            hits += 1;
        }
        // `parseFloat(x)` without isNaN guard on same line.
        if ll.contains("parsefloat(") && !ll.contains("isnan") && !ll.contains("number.isnan") {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UD-ARCH-062",
            format!(
                "UmaDev: unsafe parseInt/parseFloat (UD-ARCH-062). \
                 `{file_path}` uses parseInt without radix or parseFloat without \
                 NaN check. Always pass radix: `parseInt(x, 10)`, and validate: \
                 `const n = parseFloat(x); if (Number.isNaN(n)) ...`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// **UD-ARCH-063**: ban `JSON.parse` without try/catch (crash on invalid JSON).
///
/// `JSON.parse(userInput)` throws on malformed JSON, crashing the process.
/// Must wrap in try/catch. Flags `JSON.parse(` calls without a surrounding
/// try/catch. Runs on JS/TS.
#[must_use]
pub fn check_unsafe_json_parse(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "tsx" | "jsx") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    if !lower.contains("json.parse(") {
        return Decision::pass();
    }
    let has_guard = lower.contains("try")
        || lower.contains("catch")
        || lower.contains("try {")
        || lower.contains("try{");
    if !has_guard {
        return Decision::block(
            "UD-ARCH-063",
            format!(
                "UmaDev: JSON.parse without try/catch (UD-ARCH-063). \
                 `{file_path}` calls `JSON.parse()` without error handling — \
                 malformed JSON throws and crashes. Wrap: `try {{ return \
                 JSON.parse(x); }} catch {{ return null; }}`.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-ARCH-064**: ban `postMessage` without origin validation (XSS via messages).
///
/// `window.postMessage(data, '*')` with a wildcard target origin sends the
/// message to ANY window that's listening — including malicious iframes.
/// And `window.addEventListener('message', handler)` without checking
/// `event.origin` accepts messages from any source. Must specify the
/// exact origin: `postMessage(data, 'https://app.com')` and check
/// `event.origin` in the handler. Flags wildcard postMessage and
/// unvalidated message handlers. Runs on frontend files.
#[must_use]
pub fn check_unsafe_post_message(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "tsx" | "jsx" | "vue" | "svelte" | "js" | "ts" | "html"
    ) {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    // Wildcard target origin in postMessage.
    if lower.contains("postmessage(") && lower.contains("'*'") {
        return Decision::block(
            "UD-ARCH-064",
            format!(
                "UmaDev: postMessage with wildcard origin (UD-ARCH-064). \
                 `{file_path}` uses `postMessage(data, '*')` — the message goes \
                 to any listening window, including malicious iframes. Specify \
                 the exact origin: `postMessage(data, 'https://app.com')`.",
            ),
        );
    }
    // Message handler without origin check.
    if lower.contains("addeventlistener('message'")
        || lower.contains("addeventlistener(\"message\"")
    {
        let has_origin_check = lower.contains("event.origin")
            || lower.contains("e.origin")
            || lower.contains("origin ===")
            || lower.contains("origin == ");
        if !has_origin_check {
            return Decision::block(
                "UD-ARCH-064",
                format!(
                    "UmaDev: message handler without origin check (UD-ARCH-064). \
                     `{file_path}` listens for `message` events without checking \
                     `event.origin` — a malicious iframe can send arbitrary \
                     messages. Add: `if (event.origin !== 'https://app.com') return;`.",
                ),
            );
        }
    }
    Decision::pass()
}

/// **UG-LINT-014**: ban `for...in` loops over arrays (unreliable iteration order).
///
/// `for (const i in array)` iterates over ALL enumerable properties
/// including inherited ones, not just indices — it causes subtle bugs and
/// incorrect iteration. Must use `for...of` (values), `for (let i = 0; ...)`
/// (indices), or `.forEach()` / `.map()`. Flags `for...in` on arrays in
/// JS/TS. Conservative: only flags when the loop variable name suggests an
/// array (`items`, `list`, `data`, `array`, `arr`).
#[must_use]
pub fn check_for_in_array(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "tsx" | "jsx") {
        return Decision::pass();
    }
    let mut hits = 0usize;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        // `for (const x in ...)` or `for (let x in ...)`.
        let lower = line.to_ascii_lowercase();
        if (lower.contains(" in items")
            || lower.contains(" in list")
            || lower.contains(" in data")
            || lower.contains(" in array")
            || lower.contains(" in arr")
            || lower.contains(" in users")
            || lower.contains(" in results"))
            && (lower.contains("for (") || lower.contains("for("))
        {
            hits += 1;
        }
    }
    if hits > 0 {
        Decision::block(
            "UG-LINT-014",
            format!(
                "UmaDev: for...in over array (UG-LINT-014). \
                 `{file_path}` uses `for...in` to iterate an array — it \
                 enumerates ALL properties (including inherited), not just \
                 indices, causing subtle bugs. Use `for...of` (values), \
                 `for (let i = 0; i < arr.length; i++)` (indices), or \
                 `.forEach()`.",
            ),
        )
    } else {
        Decision::pass()
    }
}

/// Configuration variable names that must be env-based.
const CONFIG_VAR_NAMES: &[&str] = &[
    "database_url",
    "db_url",
    "db_host",
    "api_url",
    "base_url",
    "redis_url",
    "connection_string",
    "secret_key",
    "jwt_secret",
    "stripe_key",
    "port",
    "host",
];

/// Unsafe-deserialization calls per language.
const DESERIALIZE_PATTERNS: &[DeserializePattern] = &[
    DeserializePattern {
        lang: "py",
        trigger: "yaml.load(",
        safe_if: "safe_load",
        label: "yaml.load() (use yaml.safe_load)",
    },
    DeserializePattern {
        lang: "py",
        trigger: "pickle.loads(",
        safe_if: "json.loads",
        label: "pickle.loads()",
    },
    DeserializePattern {
        lang: "py",
        trigger: "pickle.load(",
        safe_if: "json.load",
        label: "pickle.load()",
    },
    DeserializePattern {
        lang: "rb",
        trigger: "Marshal.load",
        safe_if: "JSON.parse",
        label: "Marshal.load",
    },
    DeserializePattern {
        lang: "rb",
        trigger: "YAML.load",
        safe_if: "safe_load",
        label: "YAML.load (use YAML.safe_load)",
    },
    DeserializePattern {
        lang: "php",
        trigger: "unserialize(",
        safe_if: "json_decode",
        label: "unserialize()",
    },
];

/// Loose array/object type patterns (beyond what UD-ARCH-001 catches).
const LOOSE_ARRAY_PATTERNS: &[&str] = &["Array<any>", "Array<object>", ": object[]", ": {}[]"];

/// One eval-injection pattern.
struct EvalPattern {
    trigger: &'static str,
    label: &'static str,
}

/// Code-injection patterns to flag.
const EVAL_PATTERNS: &[EvalPattern] = &[
    EvalPattern {
        trigger: "eval(",
        label: "eval()",
    },
    EvalPattern {
        trigger: "new Function(",
        label: "new Function()",
    },
    EvalPattern {
        trigger: "Function(\"",
        label: "Function(\"...\")",
    },
    EvalPattern {
        trigger: "setTimeout(\"",
        label: "setTimeout(\"...\")",
    },
    EvalPattern {
        trigger: "setInterval(\"",
        label: "setInterval(\"...\")",
    },
];

/// Known typosquat package names (curated from npm security advisories).
const TYPOSQUAT_BLOCKLIST: &[&str] = &[
    "lodahs",
    "lodas",
    "reactt",
    "vuexx",
    "momen",
    "expres",
    "axio",
    "chokudar",
    "babelcli",
    "cross-envv",
    "mocha2",
    "mongose",
    "receact",
    "vuee",
    "ngular",
    "expresjs",
    "bluebrid",
    "asyc",
    "chalkk",
    "commandeer",
    "download-cli",
    "fscc",
];

/// Top-50 npm packages most frequently typosquatted (for edit-distance check).
const TOP_PACKAGES: &[&str] = &[
    "react",
    "vue",
    "angular",
    "express",
    "lodash",
    "axios",
    "chalk",
    "mocha",
    "mongoose",
    "moment",
    "async",
    "bluebird",
    "request",
    "webpack",
    "babel",
    "eslint",
    "typescript",
    "jest",
    "dotenv",
    "cors",
    "helmet",
    "jsonwebtoken",
    "passport",
    "sequelize",
    "mysql",
    "pg",
    "redis",
    "grpc",
    "kafka",
    "ramda",
    "zod",
    "yup",
    "joi",
    "prisma",
    "drizzle",
    "tailwind",
    "postcss",
    "vite",
    "rollup",
    "esbuild",
    "swc",
    "rxjs",
    "immer",
    "zustand",
    "recoil",
    "jotai",
    "swr",
    "tanstack",
    "headlessui",
    "radix",
];

/// Non-null assertion patterns to flag. In TypeScript the `!` comes AFTER the
/// operand and BEFORE the accessor: `obj!.prop` (`!.`), `func()!.x` (`)!`),
/// `arr[0]!` (`]!`). Excludes `!=` (loose inequality) and leading `!`
/// (logical not) which are different operators.
const NON_NULL_PATTERNS: &[&str] = &["!.", ")!.", "]!."];

/// One malicious-domain blocklist entry.
struct MaliciousDomain {
    /// The domain substring (lowercase) to match.
    domain: &'static str,
    /// Why it's blocked (shown in the deny reason).
    reason: &'static str,
}

/// High-confidence malicious / phishing / piracy domains. Only entries where
/// there is no legitimate reason for a UmaDev-managed project to reference
/// the domain. Extend cautiously — false positives that block real work are
/// worse than false negatives here.
const MALICIOUS_DOMAINS: &[MaliciousDomain] = &[
    MaliciousDomain {
        domain: concat!("media", "fire.com"),
        reason: "pirated-software / malware distribution hub",
    },
    MaliciousDomain {
        domain: concat!("files", "bags.com"),
        reason: "known malware download mirror",
    },
    MaliciousDomain {
        domain: concat!("sourceforge.net", "/p/"),
        reason: "SourceForge project pages host outdated/bundled-installers — use the upstream GitHub release",
    },
    MaliciousDomain {
        domain: concat!("ru.", "nodvd"),
        reason: "piracy / crack distribution",
    },
    MaliciousDomain {
        domain: concat!("game", "crack.net"),
        reason: "software-crack distribution domain",
    },
    MaliciousDomain {
        domain: concat!("keygen", ".sh"),
        reason: "keygen / license-crack distribution domain",
    },
    MaliciousDomain {
        domain: concat!("warez", ".com"),
        reason: "warez / pirated-software distribution domain",
    },
    MaliciousDomain {
        domain: concat!("torrent", "downloads.me"),
        reason: "pirated-content torrent tracker",
    },
    MaliciousDomain {
        domain: concat!("ngrok", "-free"),
        reason: concat!(
            "ngrok",
            "-free tunnel URLs are ephemeral and can be hijacked — use a stable domain"
        ),
    },
];

/// Best-effort strip of `"..."` and `'...'` string literals from a line so
/// `any`/`console` inside strings don't trip the checks.
fn strip_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_str = false;
    let mut quote = '\0';
    let mut prev = '\0';
    for ch in line.chars() {
        if !in_str && (ch == '"' || ch == '\'') && prev != '\\' {
            in_str = true;
            quote = ch;
            out.push(' ');
        } else if in_str && ch == quote && prev != '\\' {
            in_str = false;
            quote = '\0';
            out.push(' ');
        } else if !in_str {
            out.push(ch);
        } else {
            out.push(' '); // blank out string content
        }
        prev = ch;
    }
    out
}

use std::collections::HashSet;

/// `any`-type patterns to flag in TypeScript.
const TS_ANY_PATTERNS: &[&str] = &[
    ": any", " as any", "<any>", ": any[", ": any,", ": any;", ": any)",
];

/// Source-file extensions where hardcoded-secret scanning applies. Covers the
/// shipping languages whose source the team produces — including the families a
/// code-only list used to miss (`cs` / `dart` / `ex` / `exs` / `c` / `cpp` /
/// `scala`), which are collected as source elsewhere but were secret-blind here.
const SECRET_SCAN_EXTENSIONS: &[&str] = &[
    "js", "jsx", "ts", "tsx", "mjs", "cjs", "py", "rb", "go", "rs", "java", "kt", "swift", "php",
    "vue", "svelte", "cs", "dart", "ex", "exs", "c", "cpp", "cc", "h", "hpp", "scala",
];

/// Config / IaC / env / shell extensions where secrets are MOST commonly leaked
/// (`.env`, JSON/YAML/TOML config, Terraform, shell scripts, `.properties`/`.ini`,
/// CI/Docker fragments). A code-only secret scan is blind to exactly these — the
/// #1 real-world leak locations — so they get the same hardcoded-secret pass even
/// though they are not general source. PEM/key material files are included so a
/// pasted private key is caught wherever it lands.
const SECRET_CONFIG_EXTENSIONS: &[&str] = &[
    "env",
    "json",
    "json5",
    "yaml",
    "yml",
    "toml",
    "tf",
    "tfvars",
    "hcl",
    "properties",
    "ini",
    "cfg",
    "conf",
    "config",
    "sh",
    "bash",
    "zsh",
    "ksh",
    "xml",
    "gradle",
    "pem",
    "key",
    "crt",
    "cert",
    "pfx",
    "p12",
    "asc",
    "ps1",
    "bat",
];

/// Assignment-style key prefixes that indicate a hardcoded secret. Lowercase,
/// each carries the separator (`=` or `:`) the scan looks for, so they only
/// match a real `key = <value>` assignment and never a bare identifier
/// substring.
///
/// The bare key-shape prefixes used to live here too, but matched as raw
/// substrings they false-positived on ordinary identifiers (risk scores, task
/// runners, disk usage, AWS-shaped names embedded mid-word). They now live in
/// [`bare_secret_matches`], which requires a leading word boundary plus the
/// real trailing key shape before it fires.
const SECRET_PREFIXES: &[&str] = &[
    "api_key=",
    "apikey=",
    "api_key:",
    "secret_key=",
    "secret=",
    "access_token=",
    "accesstoken=",
    "auth_token=",
    "private_key=",
];

/// Long, unambiguous example/placeholder MARKERS — safe to match as a SUBSTRING
/// of a value because they do not occur inside a real key by chance. A value
/// containing any of these is an example, not a secret.
///
/// (The old flat `SECRET_PLACEHOLDERS` list mixed these with 3-char words like
/// `foo`/`test` and matched the whole set via `.contains()`, so any REAL key
/// that merely contained `test`/`foo` — `mytestkey…`, `…foobar…` — was silently
/// whitelisted: an insider-bypass and accidental-drop hole. The short words now
/// live in [`SECRET_PLACEHOLDER_WORDS`] and only match a whole/anchored value.)
const SECRET_EXAMPLE_MARKERS: &[&str] = &[
    "your_",
    "your-",
    "yourapi",
    "youraccount",
    "example",
    "placeholder",
    "changeme",
    "change_me",
    "change-me",
    "redacted",
    "replace_",
    "replace-",
    "replaceme",
    "insert_",
    "insert-",
    "dummy",
    "notreal",
    "fakekey",
    "fake_",
    "sample",
    "<your",
    "<api",
    "<token",
    "<secret",
    "<key",
];

/// Short, AMBIGUOUS placeholder words — matched only when they are (essentially)
/// the WHOLE value (optionally followed by digits/separators), never as a
/// substring, so a real `mytestkey…` / `…foobar…` secret is not whitelisted by a
/// 3-letter coincidence. See [`is_placeholder_value`].
const SECRET_PLACEHOLDER_WORDS: &[&str] = &[
    "foo", "bar", "baz", "qux", "xxx", "test", "demo", "mock", "abc", "todo", "tbd", "none",
    "null", "nil",
];

/// A single credential-bearing DB authority. Keeping user, password, and `@`
/// in one bounded match prevents a scheme mentioned in documentation from
/// correlating with unrelated punctuation much later in the file.
fn db_credential_url_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(postgres(?:ql)?|mongodb(?:\+srv)?|mysql|redis|amqp)://([^\s:/@]+):([^\s/@]+)@",
        )
        .expect("DB credential URL regex is well-formed")
    })
}

/// Frontend source extensions.
const FRONTEND_EXTENSIONS: &[&str] =
    &["jsx", "tsx", "vue", "svelte", "astro", "html", "browser.js"];

/// DB driver package names that must never appear in frontend code.
const FRONTEND_DB_DRIVERS: &[&str] = &[
    "from \"pg\"",
    "require(\"pg\")",
    "from 'pg'",
    "require('pg')",
    "from \"mongoose\"",
    "require(\"mongoose\")",
    "from 'mongoose'",
    "require('mongoose')",
    "from \"mysql",
    "require(\"mysql",
    "from 'mysql",
    "require('mysql",
    "from \"mongodb\"",
    "require(\"mongodb\")",
    "from 'mongodb'",
    "require('mongodb')",
    "from \"redis\"",
    "require(\"redis\")",
    "from 'redis'",
    "require('redis')",
    "createconnection",
    "createclient(\"pg",
];

/// One catastrophic-shell pattern. `allow_if` lists substrings that, if
/// present, downgrade the match to a pass (e.g. `--dry-run`).
struct BashPattern {
    /// Lowercase substring that triggers the block.
    trigger: &'static str,
    /// Why this is dangerous (shown to the host so it can correct course).
    why: &'static str,
    /// Concrete fix suggestion (the actionable half of the deny reason).
    fix: &'static str,
    /// If true, only block when the command is a git command.
    git_only: bool,
    /// Allow-list substrings that downgrade to pass.
    allow_if: &'static [&'static str],
}

/// The catastrophic-pattern catalogue. Conservative by design: only patterns
/// that cause irreversible damage (data loss, credential exfiltration,
/// system compromise). False negatives are acceptable here (the quality gate
/// still runs); false positives that block legitimate work are not.
const DESTRUCTIVE_BASH_PATTERNS: &[BashPattern] = &[
    // rm -rf /  —  root wipe. The classic.
    BashPattern {
        trigger: "rm -rf /",
        why: "`rm -rf /` (or variants like `rm -rf ~`) deletes the entire filesystem or home directory.",
        fix: "If you meant to clean a build dir, target it explicitly, e.g. `rm -rf target/` or `rm -rf node_modules/`.",
        git_only: false,
        allow_if: &[],
    },
    BashPattern {
        trigger: "rm -rf ~",
        why: "`rm -rf ~` wipes the user's home directory.",
        fix: "Target a specific subdirectory, e.g. `rm -rf ~/.cache/umadev`.",
        git_only: false,
        allow_if: &[],
    },
    BashPattern {
        trigger: "rm -rf /*",
        why: "`rm -rf /*` attempts to delete every top-level filesystem entry.",
        fix: "Scope the deletion to a project-local directory.",
        git_only: false,
        allow_if: &[],
    },
    // curl | sh / wget | sh  —  remote code execution.
    BashPattern {
        trigger: "| sh",
        why: "Piping a remote download straight into a shell (`curl … | sh`) runs untrusted code with no integrity check.",
        fix: "Download to a file first, inspect it, then run: `curl -fsSL <url> -o install.sh && less install.sh && sh install.sh`.",
        git_only: false,
        allow_if: &[],
    },
    BashPattern {
        trigger: "| bash",
        why: "Piping a remote download straight into bash (`curl … | bash`) runs untrusted code with no integrity check.",
        fix: "Download to a file first, inspect it, then run: `curl -fsSL <url> -o install.sh && less install.sh && bash install.sh`.",
        git_only: false,
        allow_if: &[],
    },
    // chmod 777  —  world-writable security hole.
    BashPattern {
        trigger: "chmod 777",
        why: "`chmod 777` makes a file world-readable/writable/executable — a security hole.",
        fix: "Grant only the needed bits, e.g. `chmod 755` (owner rwx, others rx) or `chmod +x`.",
        git_only: false,
        allow_if: &[],
    },
    // git push --force to main/master  —  history rewrite on protected branches.
    BashPattern {
        trigger: "push --force",
        why: "`git push --force` rewrites remote history and can clobber teammates' work.",
        fix: "Use `git push --force-with-lease` (it aborts if the remote moved) and never force-push to main/master.",
        git_only: true,
        allow_if: &["--force-with-lease"],
    },
    BashPattern {
        trigger: "push -f",
        why: "`git push -f` is a force-push that rewrites remote history.",
        fix: "Use `git push --force-with-lease` instead.",
        git_only: true,
        allow_if: &["--force-with-lease"],
    },
    // git reset --hard (no ref)  —  discards uncommitted work silently.
    BashPattern {
        trigger: "reset --hard",
        why: "`git reset --hard` discards all uncommitted changes with no recovery.",
        fix: "Stash first (`git stash`) or target a specific file. If you truly mean it, this is expected — but UmaDev flags it so the decision is conscious.",
        git_only: true,
        allow_if: &[],
    },
    // --- Irreversible / network VCS verbs the trust floor catches on the
    // NeedApproval path but a hook-less base (codex/opencode `approvalPolicy=never`)
    // would otherwise run directly via Bash. These mirror `trust::path_touches_vcs`
    // + `NETWORK_TOKENS` so the pre-bash floor protects BOTH the claude PreToolUse
    // hook AND the codex/opencode `govern_tool_call` path. All `git_only` so they
    // never fire outside a git invocation; triggers carry a trailing space (or the
    // explicit verb) so read-only neighbours are NOT caught:
    //   `git push`     → blocked   |  `git push --dry-run` → allowed (allow_if)
    //   `git merge `   → blocked   |  `git merge-base …`    → NOT caught (no space)
    //   `git rm `      → blocked
    //   `git branch -d`/`-D`/`--delete` → blocked (a branch drop loses commits)
    //   `git stash drop`/`clear`        → blocked (stashed work lost)
    //   `git update-ref -d`/`reflog delete`/`worktree remove` → blocked (history)
    // A plain `git push` reaches the network and rewrites the remote, so it
    // escalates even though it's not a `--force`.
    BashPattern {
        trigger: "git push",
        why: "`git push` sends commits to a remote and (per UmaDev's trust contract) UmaDev never auto-pushes — the customer reviews and pushes themselves.",
        fix: "Let the user run the push, or confirm the branch + remote explicitly. `git push --dry-run` is allowed for inspection.",
        git_only: true,
        // `--dry-run` is inspection-only; `--force-with-lease` stays consistent
        // with the dedicated `push --force` pattern above (which already allows it).
        allow_if: &["--dry-run", "--force-with-lease"],
    },
    BashPattern {
        trigger: "git merge ",
        why: "`git merge` mutates the current branch's history — UmaDev isolates work on `umadev/<slug>` and never auto-merges into the user's branch.",
        fix: "Leave the merge to the user after they review the diff. (Read-only `git merge-base` is not affected.)",
        git_only: true,
        allow_if: &[],
    },
    BashPattern {
        trigger: "git rm ",
        why: "`git rm` deletes tracked files from the working tree and the index.",
        fix: "If a file must go, delete it in a reviewed change; UmaDev flags `git rm` so the removal is conscious.",
        git_only: true,
        allow_if: &[],
    },
    BashPattern {
        trigger: "git branch -d",
        why: "`git branch -d`/`-D` deletes a branch; `-D` force-deletes even unmerged commits, losing work.",
        fix: "Confirm the branch is fully merged/pushed before deleting it.",
        git_only: true,
        allow_if: &[],
    },
    BashPattern {
        trigger: "git branch --delete",
        why: "`git branch --delete` (the long form of `-d`/`-D`) deletes a branch and can drop unmerged commits.",
        fix: "Confirm the branch is fully merged/pushed before deleting it.",
        git_only: true,
        allow_if: &[],
    },
    BashPattern {
        trigger: "git stash drop",
        why: "`git stash drop` permanently discards a stashed change with no recovery.",
        fix: "Apply or inspect the stash first (`git stash show -p`); drop only when you're sure.",
        git_only: true,
        allow_if: &[],
    },
    BashPattern {
        trigger: "git stash clear",
        why: "`git stash clear` deletes ALL stashed changes irreversibly.",
        fix: "Review each stash entry before clearing; this loses every stash at once.",
        git_only: true,
        allow_if: &[],
    },
    BashPattern {
        trigger: "git update-ref -d",
        why: "`git update-ref -d` deletes a ref directly, bypassing the usual branch/tag safety — history can become unreachable.",
        fix: "Delete branches/tags via `git branch`/`git tag` instead, or confirm the ref is recoverable from a reflog.",
        git_only: true,
        allow_if: &[],
    },
    BashPattern {
        trigger: "git reflog delete",
        why: "`git reflog delete` removes reflog entries, the last safety net for recovering rewritten/lost commits.",
        fix: "Avoid pruning the reflog; it's what lets you undo a bad reset/rebase.",
        git_only: true,
        allow_if: &[],
    },
    BashPattern {
        trigger: "git worktree remove",
        why: "`git worktree remove` deletes a linked worktree and any uncommitted changes inside it.",
        fix: "Commit or stash inside the worktree first; UmaDev flags the removal so it's conscious.",
        git_only: true,
        allow_if: &[],
    },
    // dd of=/dev/...  —  raw disk write, can brick the system. Match on
    // `of=/dev/` (not `dd of=`) since flags interleave (`dd if=… of=/dev/sda`).
    BashPattern {
        trigger: "of=/dev/",
        why: "Writing to a device node (`of=/dev/…`) can overwrite a disk, partition, or memory device — `dd` makes this destructive and silent.",
        fix: "Confirm the `of=` target is correct and intended. This is flagged so a typo doesn't brick the machine.",
        git_only: false,
        allow_if: &[],
    },
    // mkfs on a real device  —  formats a disk/partition.
    BashPattern {
        trigger: "mkfs",
        why: "`mkfs` formats a filesystem — running it on the wrong device destroys data.",
        fix: "Triple-check the device path. UmaDev flags any `mkfs` so it's a conscious decision.",
        git_only: false,
        allow_if: &[],
    },
    // Drop / delete database / table (destructive SQL via psql/mysql inline).
    BashPattern {
        trigger: "drop database",
        why: "`DROP DATABASE` is irreversible in most engines.",
        fix: "Back up first (`pg_dump`/`mysqldump`). UmaDev flags this so it's intentional.",
        git_only: false,
        allow_if: &[],
    },
    BashPattern {
        trigger: "drop table",
        why: "`DROP TABLE` deletes the table and all its rows irreversibly.",
        fix: "Use `DROP TABLE IF EXISTS` in a migration, or back up first. UmaDev flags raw `drop table`.",
        git_only: false,
        allow_if: &[],
    },
    // Shutdown / reboot  —  not appropriate inside a dev agent.
    BashPattern {
        trigger: "shutdown",
        why: "`shutdown` powers off the machine — not something a dev agent should do.",
        fix: "Remove the shutdown command; it halts the user's machine.",
        git_only: false,
        allow_if: &[],
    },
    BashPattern {
        trigger: "init 0",
        why: "`init 0` halts the system.",
        fix: "Remove the command; it powers off the user's machine.",
        git_only: false,
        allow_if: &[],
    },
];

#[cfg(test)]
#[path = "rules/tests.rs"]
mod tests;
