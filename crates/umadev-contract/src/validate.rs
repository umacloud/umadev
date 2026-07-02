//! Contract validation — replace substring matching with typed cross-checks.
//!
//! Two checks that the legacy code did with `String::contains`:
//! - `check_api_url_consistency` (phases.rs) — every architecture API path
//!   must appear somewhere in the frontend notes + audit log blob.
//! - `check_prd_arch_alignment` (phases.rs) — every PRD route's first
//!   segment must appear as a substring of the architecture doc.
//!
//! Both were fragile: a route mentioned in prose counted as "wired", a
//! `/login` route satisfied the check if the word "login" appeared anywhere.
//! This module does it properly against the typed [`ApiSpec`].

use crate::backend::{route_registered, BackendRoute};
use crate::extract::FrontendCall;
use crate::parse::ApiSpec;

/// One mismatch between a consumer (frontend / PRD) and the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractViolation {
    /// What kind of consumer violated the contract.
    pub kind: ViolationKind,
    /// Human-readable detail.
    pub detail: String,
}

/// The category of a contract violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    /// Frontend calls an endpoint not declared in the contract.
    UndeclaredCall,
    /// Frontend uses a different method than the contract declares.
    MethodMismatch,
    /// A PRD route has no corresponding contract endpoint.
    UnmatchedRoute,
}

impl ContractViolation {
    fn undeclared_call(call: &FrontendCall) -> Self {
        Self {
            kind: ViolationKind::UndeclaredCall,
            detail: format!(
                "{} {} — not declared in openapi contract",
                call.method.as_str().to_uppercase(),
                call.path
            ),
        }
    }

    fn method_mismatch(call: &FrontendCall, declared_path: &str) -> Self {
        Self {
            kind: ViolationKind::MethodMismatch,
            detail: format!(
                "frontend uses {} {} but contract declares a different method at {}",
                call.method.as_str().to_uppercase(),
                call.path,
                declared_path
            ),
        }
    }

    fn unmatched_route(route: &str) -> Self {
        Self {
            kind: ViolationKind::UnmatchedRoute,
            detail: format!("PRD route `{route}` has no matching contract endpoint"),
        }
    }

    fn unregistered_endpoint(method: &str, path: &str) -> Self {
        Self {
            kind: ViolationKind::UnmatchedRoute,
            detail: format!(
                "contract declares {} {path} but no backend route registration implements it",
                method.to_uppercase()
            ),
        }
    }
}

/// Validate frontend calls against the contract.
///
/// For each frontend call:
/// - If no endpoint matches the path at all → [`ViolationKind::UndeclaredCall`].
/// - If the path matches a template but the method differs →
///   [`ViolationKind::MethodMismatch`].
/// - If fully matched → no violation.
///
/// Returns the list of violations (empty = fully conformant).
#[must_use]
pub fn validate_frontend_vs_contract(
    calls: &[FrontendCall],
    spec: &ApiSpec,
) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for call in calls {
        // Does ANY endpoint match this path (any method)?
        let path_known = spec
            .endpoints
            .iter()
            .any(|e| path_matches_ignoring_method(&e.path, &call.path));
        if !path_known {
            violations.push(ContractViolation::undeclared_call(call));
            continue;
        }
        // The path matches a declared template. Only raise MethodMismatch when
        // the frontend method was actually determined from the call shape. A
        // call whose method is a best-effort default (e.g. a bare
        // `request('/x')` wrapper, where the verb is unknown) must NOT be
        // reported as a method mismatch — that would be a guess presented as a
        // defect, exactly the kind of false alarm that erodes trust.
        if !call.method_known {
            continue;
        }
        // Path is known and the method is known — does the method match?
        if !spec.has_endpoint(call.method, &call.path) {
            // Find the declared path template for a clearer message.
            let declared = spec
                .endpoints
                .iter()
                .find(|e| path_matches_ignoring_method(&e.path, &call.path))
                .map(|e| e.path.as_str())
                .unwrap_or(&call.path);
            violations.push(ContractViolation::method_mismatch(call, declared));
        }
    }
    violations
}

/// Validate the **backend** against the contract — the symmetric counterpart
/// of [`validate_frontend_vs_contract`]. Every endpoint the contract declares
/// must have a real backend route REGISTRATION (see [`route_registered`]); a
/// contract endpoint with no matching registration is flagged. This strengthens
/// the UD-CODE-003 cross-check: the frontend side proves callers match the
/// contract, this side proves the server actually serves it (not just that the
/// path appears in a `fetch` call or a comment).
///
/// Fail-open: when `routes` is empty (a pure-frontend project, or a backend in
/// a framework we cannot parse) this returns no violations rather than flagging
/// every endpoint — the same fail-open stance the acceptance check takes.
/// Contract endpoints whose path is too generic to check (`/`, `/api`, `/:id`)
/// are skipped.
#[must_use]
pub fn validate_backend_vs_contract(
    routes: &[BackendRoute],
    spec: &ApiSpec,
) -> Vec<ContractViolation> {
    if routes.is_empty() {
        return Vec::new();
    }
    let mut violations = Vec::new();
    for e in &spec.endpoints {
        if !crate::backend::path_has_checkable_segment(&e.path) {
            continue;
        }
        if !route_registered(routes, e.method, &e.path) {
            violations.push(ContractViolation::unregistered_endpoint(
                e.method.as_str(),
                &e.path,
            ));
        }
    }
    violations
}

/// Validate PRD routes against the contract. Each route (e.g. `/dashboard`,
/// `/settings/profile`) should have at least one contract endpoint whose
/// path references the same resource. This is looser than the frontend check
/// (a route doesn't map 1:1 to an endpoint) but still catches a PRD that
/// promises pages the backend never serves.
#[must_use]
pub fn validate_prd_vs_contract(prd_routes: &[String], spec: &ApiSpec) -> Vec<ContractViolation> {
    let mut violations = Vec::new();
    for route in prd_routes {
        // Extract the resource segment: the last non-parameter, non-"api",
        // non-version-prefix segment of the path. E.g. "/api/users/:id" →
        // "users"; "/api/v2/users" → "users" (the `v2` version prefix is
        // skipped so versioned routes still match their resource).
        let segments: Vec<&str> = route
            .trim_matches('/')
            .split('/')
            .filter(|s| {
                !s.is_empty() && !s.starts_with(':') && *s != "api" && !is_version_prefix(s)
            })
            .collect();
        let route_base = segments.last().copied().unwrap_or("");
        if route_base.is_empty() {
            continue;
        }
        // Does any contract endpoint path mention this resource?
        let matched = spec
            .endpoints
            .iter()
            .any(|e| path_contains_segment(&e.path, route_base));
        if !matched {
            violations.push(ContractViolation::unmatched_route(route));
        }
    }
    violations
}

/// Path-template match ignoring the HTTP method.
fn path_matches_ignoring_method(template: &str, call_path: &str) -> bool {
    let call_path = call_path.split(['?', '#']).next().unwrap_or(call_path);
    let template_segments: Vec<&str> = template.trim_end_matches('/').split('/').collect();
    let call_segments: Vec<&str> = call_path.trim_end_matches('/').split('/').collect();
    if template_segments.len() != call_segments.len() {
        return false;
    }
    template_segments
        .iter()
        .zip(call_segments.iter())
        .all(|(t, c)| crate::parse::is_template_param(t) || t == c)
}

/// Whether a path segment is a version prefix like `v1`, `v2`, `v10`.
/// Matched case-insensitively so `V2` also counts.
fn is_version_prefix(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.len() >= 2 && lower.starts_with('v') && lower[1..].chars().all(|c| c.is_ascii_digit())
}

/// Does `path` contain a non-parameter segment equal to `segment`?
fn path_contains_segment(path: &str, segment: &str) -> bool {
    path.split('/').any(|s| !s.starts_with(':') && s == segment)
}

/// Extract route paths from PRD markdown (the information-architecture
/// tree using `├── /route` / `└── /route` markers). Used by the agent to
/// feed [`validate_prd_vs_contract`].
#[must_use]
pub fn extract_prd_routes(prd_markdown: &str) -> Vec<String> {
    let mut routes = Vec::new();
    for line in prd_markdown.lines() {
        // Strip box-drawing chars + leading whitespace.
        let stripped: String = line
            .trim()
            .trim_start_matches(['├', '└', '│', '─', ' '])
            .to_string();
        if !stripped.starts_with('/') {
            continue;
        }
        // Take the path up to the first whitespace (ignore trailing labels).
        let path = stripped.split_whitespace().next().unwrap_or(&stripped);
        // Skip the root + param-only routes (too generic to validate).
        // `/Home` (and `/home`) is the conventional landing page that every
        // app has but rarely maps to a single REST resource — skip case-
        // insensitively so a lowercase `/home` route isn't flagged.
        if path.len() < 3 || path.to_ascii_lowercase().contains("/home") {
            continue;
        }
        routes.push(path.to_string());
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse_architecture, HttpVerb};

    fn spec() -> ApiSpec {
        parse_architecture(
            "| Method | Path | Request | Response | Auth | Description |
|---|---|---|---|---|---|
| GET | /api/users | - | - | none | List |
| POST | /api/users | - | - | none | Create |
| GET | /api/users/:id | - | - | bearer | Get one |
| DELETE | /api/users/:id | - | - | bearer | Delete |
",
            "demo",
        )
    }

    fn call(method: HttpVerb, path: &str) -> FrontendCall {
        FrontendCall {
            file: "src/api.ts".into(),
            method,
            path: path.into(),
            method_known: true,
        }
    }

    /// A call whose method is a best-effort default (verb not determinable
    /// from the call shape), e.g. a bare `request('/x')` wrapper.
    fn call_unknown_method(method: HttpVerb, path: &str) -> FrontendCall {
        FrontendCall {
            file: "src/api.ts".into(),
            method,
            path: path.into(),
            method_known: false,
        }
    }

    #[test]
    fn fully_conformant_calls_yield_no_violations() {
        let spec = spec();
        let calls = vec![
            call(HttpVerb::Get, "/api/users"),
            call(HttpVerb::Post, "/api/users"),
            call(HttpVerb::Get, "/api/users/42"),
            call(HttpVerb::Delete, "/api/users/42"),
        ];
        assert!(validate_frontend_vs_contract(&calls, &spec).is_empty());
    }

    #[test]
    fn undeclared_call_flagged() {
        let spec = spec();
        let calls = vec![call(HttpVerb::Get, "/api/nonexistent")];
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::UndeclaredCall);
    }

    #[test]
    fn method_mismatch_flagged() {
        let spec = spec();
        // Contract declares GET /api/users, frontend calls DELETE /api/users.
        let calls = vec![call(HttpVerb::Delete, "/api/users")];
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::MethodMismatch);
        assert!(v[0].detail.contains("DELETE"));
    }

    #[test]
    fn unknown_method_suppresses_method_mismatch() {
        // Fix 3: a call whose method is a best-effort default (method_known =
        // false, e.g. a bare `request('/x')` wrapper) must NOT raise a
        // MethodMismatch even when the guessed verb differs from the contract.
        let spec = spec();
        // Contract has GET+POST /api/users (no DELETE without :id). A
        // DELETE-tagged but unknown-method call to /api/users would mismatch if
        // we trusted the guess — we must not.
        let calls = vec![call_unknown_method(HttpVerb::Delete, "/api/users")];
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert!(
            v.is_empty(),
            "unknown-method call must not raise MethodMismatch, got {v:?}"
        );
    }

    #[test]
    fn unknown_method_still_flags_undeclared_path() {
        // The method_known leniency only suppresses MethodMismatch — an
        // entirely unknown PATH is still a real UndeclaredCall.
        let spec = spec();
        let calls = vec![call_unknown_method(HttpVerb::Get, "/api/nonexistent")];
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::UndeclaredCall);
    }

    #[test]
    fn param_path_matched_correctly() {
        let spec = spec();
        // /api/users/123 matches the :id template — no violation.
        let calls = vec![call(HttpVerb::Get, "/api/users/123")];
        assert!(validate_frontend_vs_contract(&calls, &spec).is_empty());
        // /api/users/123/posts does NOT match (extra segment) — undeclared.
        let calls = vec![call(HttpVerb::Get, "/api/users/123/posts")];
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert_eq!(v[0].kind, ViolationKind::UndeclaredCall);
    }

    #[test]
    fn query_string_stripped_before_match() {
        let spec = spec();
        let calls = vec![call(HttpVerb::Get, "/api/users?include=email")];
        assert!(validate_frontend_vs_contract(&calls, &spec).is_empty());
    }

    #[test]
    fn empty_contract_flags_everything_as_undeclared() {
        let spec = ApiSpec::default();
        let calls = vec![call(HttpVerb::Get, "/api/x")];
        let v = validate_frontend_vs_contract(&calls, &spec);
        assert_eq!(v[0].kind, ViolationKind::UndeclaredCall);
    }

    #[test]
    fn prd_routes_validated() {
        let spec = spec();
        let routes = vec![
            "/dashboard".to_string(),
            "/settings/profile".to_string(),
            "/users".to_string(), // matches /api/users
        ];
        let v = validate_prd_vs_contract(&routes, &spec);
        // dashboard + settings have no matching contract endpoint.
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].kind, ViolationKind::UnmatchedRoute);
        assert!(v[0].detail.contains("dashboard"));
    }

    #[test]
    fn extract_prd_routes_from_markdown_tree() {
        let prd = "## Information architecture\n\n```\n/ (Home)\n├── /dashboard\n├── /settings\n│   └── /settings/profile\n└── /users\n```";
        let routes = extract_prd_routes(prd);
        assert!(routes.contains(&"/dashboard".to_string()));
        assert!(routes.contains(&"/settings".to_string()));
        assert!(routes.contains(&"/users".to_string()));
        // Home is excluded (too generic).
        assert!(!routes.iter().any(|r| r.contains("Home")));
    }

    #[test]
    fn validate_prd_handles_versioned_routes() {
        // Regression: a versioned route like /api/v2/users used to extract
        // `v2` as the route_base (the version prefix), so it never matched
        // a contract endpoint and was flagged as an unmatched violation.
        // Now version prefixes are skipped → route_base = "users".
        use super::*;
        use crate::parse::{Endpoint, HttpVerb, SecurityKind};
        let spec = ApiSpec {
            endpoints: vec![Endpoint {
                method: HttpVerb::Get,
                path: "/api/users".into(),
                operation_id: "list_users".into(),
                description: "list".into(),
                request_shape: String::new(),
                response_shape: String::new(),
                security: SecurityKind::None,
            }],
            title: "t".into(),
        };
        let routes = vec!["/api/v2/users".to_string()];
        let violations = validate_prd_vs_contract(&routes, &spec);
        assert!(
            violations.is_empty(),
            "versioned /api/v2/users must match the /api/users contract, got {violations:?}"
        );
    }

    #[test]
    fn is_version_prefix_detection() {
        use super::is_version_prefix;
        assert!(is_version_prefix("v1"));
        assert!(is_version_prefix("v2"));
        assert!(is_version_prefix("v10"));
        assert!(is_version_prefix("V2")); // case-insensitive
        assert!(!is_version_prefix("users"));
        assert!(!is_version_prefix("api"));
        assert!(!is_version_prefix("v")); // too short
        assert!(!is_version_prefix("vx")); // not all digits after v
    }

    #[test]
    fn extract_prd_routes_skips_home_case_insensitive() {
        // Regression: `/home` (lowercase) was previously NOT skipped because
        // the check was case-sensitive (`contains("/Home")`), so a legitimate
        // landing-page route got flagged as an unmatched contract violation.
        let prd = "/\n├── /home\n├── /dashboard\n└── /users\n";
        let routes = extract_prd_routes(prd);
        assert!(
            !routes.iter().any(|r| r.eq_ignore_ascii_case("/home")),
            "lowercase /home must be skipped, got {routes:?}"
        );
        assert!(routes.contains(&"/dashboard".to_string()));
    }

    #[test]
    fn path_contains_segment_works() {
        assert!(path_contains_segment("/api/users/:id", "users"));
        assert!(!path_contains_segment("/api/users/:id", "id")); // :id is a param
        assert!(!path_contains_segment("/api/orders", "users"));
    }

    #[test]
    fn empty_contract_and_empty_calls_yield_no_violations() {
        let spec = ApiSpec::default();
        assert!(validate_frontend_vs_contract(&[], &spec).is_empty());
        assert!(validate_prd_vs_contract(&[], &spec).is_empty());
    }

    // ---- Fix 4: lock the (currently zero-call) PRD validators' logic ----

    #[test]
    fn prd_route_matched_when_resource_is_present() {
        // A PRD route whose leaf resource is served by SOME endpoint is clean.
        let spec = spec();
        let routes = vec!["/users".to_string(), "/users/edit".to_string()];
        // "/users" → users (present); "/users/edit" → edit (NOT present).
        let v = validate_prd_vs_contract(&routes, &spec);
        assert_eq!(v.len(), 1);
        assert!(v[0].detail.contains("/users/edit"));
    }

    #[test]
    fn prd_route_base_skips_param_and_api_and_version() {
        // The resource segment is the last NON-param, NON-"api", NON-version
        // segment. `/api/v3/users/:id` → "users", which the contract serves.
        let spec = spec();
        let v = validate_prd_vs_contract(&["/api/v3/users/:id".to_string()], &spec);
        assert!(
            v.is_empty(),
            "version + param + api prefixes must be skipped, got {v:?}"
        );
    }

    #[test]
    fn prd_route_all_param_segments_is_skipped_not_flagged() {
        // A route that is purely `/:id` (no resource segment) is too generic to
        // validate → silently skipped, never a false UnmatchedRoute.
        let spec = spec();
        assert!(validate_prd_vs_contract(&["/:id".to_string()], &spec).is_empty());
        assert!(validate_prd_vs_contract(&["/api".to_string()], &spec).is_empty());
    }

    #[test]
    fn extract_prd_routes_takes_path_before_label() {
        // A tree line `├── /dashboard  Main dashboard` keeps only `/dashboard`.
        let prd = "├── /dashboard  Main dashboard\n└── /orders  Order list";
        let routes = extract_prd_routes(prd);
        assert_eq!(
            routes,
            vec!["/dashboard".to_string(), "/orders".to_string()]
        );
    }

    #[test]
    fn extract_prd_routes_ignores_non_path_and_too_short() {
        // Prose lines and the bare root `/` are not routes.
        let prd = "This app has pages.\n/\n/x\n├── /reports";
        let routes = extract_prd_routes(prd);
        // `/` and `/x` (len < 3) dropped; only `/reports` survives.
        assert_eq!(routes, vec!["/reports".to_string()]);
    }

    // ---- validate_backend_vs_contract (symmetric BE↔contract cross-check) ----

    #[test]
    fn backend_vs_contract_flags_unregistered_endpoint() {
        use crate::backend::BackendRoute;
        let spec = spec(); // GET/POST /api/users, GET/DELETE /api/users/:id
                           // Backend only registers the list + create, not the item routes.
        let routes = vec![
            BackendRoute {
                file: "s.js".into(),
                method: Some(HttpVerb::Get),
                path: "/api/users".into(),
            },
            BackendRoute {
                file: "s.js".into(),
                method: Some(HttpVerb::Post),
                path: "/api/users".into(),
            },
        ];
        let v = validate_backend_vs_contract(&routes, &spec);
        // GET + DELETE /api/users/:id are declared but not registered.
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v.iter().all(|x| x.kind == ViolationKind::UnmatchedRoute));
        assert!(v.iter().any(|x| x.detail.contains("/api/users/:id")));
    }

    #[test]
    fn backend_vs_contract_clean_when_all_registered() {
        use crate::backend::BackendRoute;
        let spec = spec();
        let routes = vec![
            BackendRoute {
                file: "s.js".into(),
                method: Some(HttpVerb::Get),
                path: "/api/users".into(),
            },
            BackendRoute {
                file: "s.js".into(),
                method: Some(HttpVerb::Post),
                path: "/api/users".into(),
            },
            BackendRoute {
                file: "s.js".into(),
                method: Some(HttpVerb::Get),
                path: "/api/users/:id".into(),
            },
            BackendRoute {
                file: "s.js".into(),
                method: Some(HttpVerb::Delete),
                path: "/api/users/:id".into(),
            },
        ];
        assert!(validate_backend_vs_contract(&routes, &spec).is_empty());
    }

    #[test]
    fn backend_vs_contract_fail_open_when_no_routes() {
        // No backend registrations → fail-open (do not flag every endpoint).
        let spec = spec();
        assert!(validate_backend_vs_contract(&[], &spec).is_empty());
    }

    #[test]
    fn extract_then_validate_prd_round_trip() {
        // End-to-end for the (soon-to-be-wired) pair: extract from markdown,
        // then validate against the contract.
        let spec = spec();
        let prd = "## IA\n```\n/ (Home)\n├── /users\n└── /billing\n```";
        let routes = extract_prd_routes(prd);
        assert!(routes.contains(&"/users".to_string()));
        assert!(routes.contains(&"/billing".to_string()));
        let v = validate_prd_vs_contract(&routes, &spec);
        // /users is served; /billing is not.
        assert_eq!(v.len(), 1);
        assert!(v[0].detail.contains("/billing"));
        assert_eq!(v[0].kind, ViolationKind::UnmatchedRoute);
    }
}
