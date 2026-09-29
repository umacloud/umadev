//! Evidence checks for a plan step's declared `test-fails-then-passes` and
//! `route-responds` contracts, kept out of the director loop itself.

use std::sync::Arc;

use super::EvidenceOutcome;
use crate::events::{EngineEvent, EventSink};
use crate::plan_state::{EvidenceContract, PlanStep};
use crate::runtime_proof::{self, RuntimeProof, RuntimeStatus};
use crate::verify::NamedTestOutcome;

/// The gap for a red→green test whose runner ran no test by that name at head: a
/// name filter that matched nothing (or only other tests) is not a pass.
pub(super) fn named_test_not_run(test: &str) -> String {
    format!(
        "declared red→green on test \"{test}\" but the test runner ran no test by that name at \
         head — name the test exactly as the runner reports it, and make sure it runs and passes"
    )
}

/// The RED half's verdict, given how the named test behaved at the step's PRE-state.
/// Pure — the decision, separated from the IO that produces it (see
/// `test_red_green_outcome`, which has already established that the test is present
/// and GREEN at head before asking this).
///
/// `None` ⇒ **inconclusive**: the caller must fall open to the ordinary `TestPasses`
/// bar rather than reach a verdict it could not support.
pub(super) fn red_half_verdict(test: &str, red: NamedTestOutcome) -> Option<EvidenceOutcome> {
    match red {
        // The step's test was RED before it ran — or did not exist yet, the same fact
        // stated more strongly — and is GREEN now. That is a test.
        NamedTestOutcome::Failed | NamedTestOutcome::NotFound => Some(EvidenceOutcome::Pass),
        // THE FINDING. The test passed BEFORE the step's work existed, so it cannot be
        // asserting that work — it was written to match code that was already there.
        NamedTestOutcome::Passed => Some(EvidenceOutcome::Gap(format!(
            "test \"{test}\" ALREADY PASSED at this step's pre-state — it was written after (or \
             around) the code, so it has never demonstrated that it can detect the behaviour's \
             absence. Make it a real test: assert the behaviour this step is supposed to add, \
             confirm it FAILS without that code, then make it pass"
        ))),
        // We could not run it in the rewound tree — inconclusive, never a verdict.
        NamedTestOutcome::Unavailable => None,
    }
}

/// The `(method, path)` routes a step declared in `route-responds` evidence — what
/// the runtime proof probes for this step, each with its own method.
pub(super) fn declared_routes(step: &PlanStep) -> Vec<(String, String)> {
    step.evidence
        .iter()
        .filter_map(|contract| match contract {
            EvidenceContract::RouteResponds { method, path, .. } => {
                Some((method.clone(), path.clone()))
            }
            _ => None,
        })
        .collect()
}

/// `RouteResponds` contract → read the runtime proof that probed this step's
/// declared routes, each with its own method (see
/// [`crate::runtime_proof::run_runtime_proof_probing`]). `Pass` when the route
/// answered as declared; a typed gap when the answer shows the route is absent or
/// broken; a neutral skip, with a note saying why, when the app could not be booted
/// or the probe could not reproduce the declared answer. The probe is a bare
/// request — no credentials, no request body, `1` for every path parameter — so a
/// `401`, a validation `400` on a `POST`, or a `404` for record `1` means the
/// declaration could not be checked, never that the route is broken.
pub(super) fn route_responds_outcome(
    events: &Arc<dyn EventSink>,
    runtime: Option<&RuntimeProof>,
    method: &str,
    path: &str,
    status: Option<u16>,
) -> EvidenceOutcome {
    let Some(proof) = runtime else {
        // No current proof (it went stale and was discarded, with its own note).
        return EvidenceOutcome::Skip;
    };
    let unchecked = match route_verdict(proof, method, path, status) {
        RouteVerdict::Pass => return EvidenceOutcome::Pass,
        RouteVerdict::Gap(line) => return EvidenceOutcome::Gap(line),
        RouteVerdict::Unchecked(why) => why,
    };
    events.emit(EngineEvent::Note(format!(
        "team · route evidence {} {path} not checked — {unchecked}",
        method_label(method)
    )));
    EvidenceOutcome::Skip
}

/// What the runtime proof says about one declared route.
#[derive(Debug, PartialEq, Eq)]
enum RouteVerdict {
    Pass,
    Gap(String),
    /// The route could not be checked; the reason for the note.
    Unchecked(String),
}

/// The declared method as the proof records it (`GET` when none was declared).
fn method_label(method: &str) -> String {
    let method = method.trim().to_ascii_uppercase();
    if method.is_empty() {
        "GET".to_string()
    } else {
        method
    }
}

fn route_verdict(
    proof: &RuntimeProof,
    method: &str,
    path: &str,
    status: Option<u16>,
) -> RouteVerdict {
    if let RuntimeStatus::NotVerified(reason) = &proof.status {
        // The app couldn't be booted/probed at all — neutral, not a false failure.
        return RouteVerdict::Unchecked(format!("the app was not booted and probed ({reason})"));
    }
    let method = method_label(method);
    let Some(want) = runtime_proof::probe_path(path) else {
        return RouteVerdict::Unchecked("it is not a plain request path".to_string());
    };
    let Some(probe) = proof
        .routes
        .iter()
        .find(|r| r.method == method && r.path == want)
    else {
        return RouteVerdict::Unchecked(match method.as_str() {
            "DELETE" => "UmaDev never sends a DELETE (it could delete the app's data)".to_string(),
            "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT" | "PATCH" => {
                "the runtime proof did not probe it (a server UmaDev did not start only gets GET, \
                 HEAD and OPTIONS requests)"
                    .to_string()
            }
            _ => format!("UmaDev does not send {method} requests"),
        });
    };
    // L2: `None` = any non-error response; `Some(code)` = require exactly `code`
    // (including a required error status like 401).
    let answered = match status {
        None => probe.ok,
        Some(want) => probe.status == want,
    };
    if answered {
        return RouteVerdict::Pass;
    }
    let got = probe.status;
    if let Some(why) = unreproducible(probe, &method, path, status) {
        return RouteVerdict::Unchecked(why);
    }
    let server = match (&proof.dev_server, &proof.base_url) {
        (Some(label), Some(url)) => format!(" (probed on the {label} at {url})"),
        _ => String::new(),
    };
    let expected = status.map_or_else(|| "OK".to_string(), |want| want.to_string());
    RouteVerdict::Gap(format!(
        "declared {method} {path} responds {expected} but it returned {}{server}",
        returned(got)
    ))
}

/// Why a probe's answer, which differs from the declared one, does not show the
/// route is broken: an answer a bare request cannot get past, or a failure of
/// something other than the app itself.
fn unreproducible(
    probe: &runtime_proof::RouteProbe,
    method: &str,
    path: &str,
    status: Option<u16>,
) -> Option<String> {
    let got = probe.status;
    if probe.proxy_error {
        return Some(format!(
            "the dev server could not reach the backend it proxies {} to",
            probe.path
        ));
    }
    // A bare probe carries no credentials: a 401/403 proves the route is wired and
    // guarded, not that it fails to answer an authorised caller.
    if matches!(got, 401 | 403) {
        return Some(format!("it answered {got}; the probe sends no credentials"));
    }
    if (300..400).contains(&got) && status.is_some_and(|want| (200..300).contains(&want)) {
        return Some(format!(
            "it answered {got}, a redirect the probe does not follow"
        ));
    }
    if matches!(got, 502..=504) {
        return Some(format!(
            "it answered {got}, a gateway error from something between the probe and the app"
        ));
    }
    // A path parameter is probed as `1`; a 404 there can mean "no record 1".
    if got == 404 && runtime_proof::is_templated_path(path) {
        return Some(format!(
            "it answered 404 for {}, and there may be no record with that id",
            probe.path
        ));
    }
    // A request that needs a body cannot be reproduced by a bodiless probe. Only a
    // missing handler (404) or a method the path does not accept (405) shows the
    // route is not there.
    let bodiless = !matches!(method, "GET" | "HEAD" | "OPTIONS");
    if bodiless && !matches!(got, 404 | 405) {
        return Some(format!(
            "it answered {}; the probe sends no request body, so the declared answer could not \
             be reproduced",
            returned(got)
        ));
    }
    None
}

/// A probe status for a message: `0` is curl's "no response".
fn returned(status: u16) -> String {
    if status == 0 {
        "no response".to_string()
    } else {
        format!("status {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_proof::RouteProbe;

    fn proof(routes: Vec<RouteProbe>) -> RuntimeProof {
        RuntimeProof {
            timestamp: "2026-09-25T00:00:00Z".to_string(),
            status: RuntimeStatus::Verified,
            dev_server: Some("Node dev server".to_string()),
            command: Some("npm run dev".to_string()),
            base_url: Some("http://localhost:3000".to_string()),
            ready_ms: Some(800),
            routes,
            e2e: None,
            source_fingerprint: None,
        }
    }

    fn probe(method: &str, path: &str, status: u16) -> RouteProbe {
        RouteProbe {
            method: method.to_string(),
            path: path.to_string(),
            status,
            ms: 3,
            ok: status != 0 && status < 400,
            proxy_error: false,
        }
    }

    fn sink() -> (Arc<dyn EventSink>, Arc<crate::events::RecordingSink>) {
        let rec = Arc::new(crate::events::RecordingSink::default());
        (rec.clone() as Arc<dyn EventSink>, rec)
    }

    fn notes(rec: &crate::events::RecordingSink) -> Vec<String> {
        rec.events()
            .into_iter()
            .filter_map(|e| match e {
                EngineEvent::Note(n) => Some(n),
                _ => None,
            })
            .collect()
    }

    fn show(outcome: &EvidenceOutcome) -> String {
        match outcome {
            EvidenceOutcome::Pass => "Pass".to_string(),
            EvidenceOutcome::Skip => "Skip".to_string(),
            EvidenceOutcome::Gap(line) => format!("Gap({line})"),
        }
    }

    #[test]
    fn a_test_that_did_not_exist_before_the_step_was_red() {
        assert!(matches!(
            red_half_verdict("adds_numbers", NamedTestOutcome::NotFound),
            Some(EvidenceOutcome::Pass)
        ));
        assert!(named_test_not_run("adds_numbers").contains("\"adds_numbers\""));
    }

    #[test]
    fn route_responds_on_a_booted_app_probes_the_declared_route() {
        // The planner's own example: `POST /api/login responds 200`. A booted app whose
        // proof never probed that route is not evidence the route is missing.
        let (events, rec) = sink();
        let booted = proof(vec![probe("GET", "/", 200)]);
        let outcome =
            route_responds_outcome(&events, Some(&booted), "POST", "/api/login", Some(200));
        assert!(
            matches!(outcome, EvidenceOutcome::Skip),
            "{}",
            show(&outcome)
        );
        assert!(
            notes(&rec).iter().any(|n| n.contains("POST /api/login")),
            "the skip is said, not silent: {:?}",
            notes(&rec)
        );

        // Probed with its own method, the declared answer passes.
        let answered = proof(vec![
            probe("GET", "/", 200),
            probe("POST", "/api/login", 200),
        ]);
        assert!(matches!(
            route_responds_outcome(&events, Some(&answered), "POST", "/api/login", Some(200)),
            EvidenceOutcome::Pass
        ));
        // A GET probe of the same path says nothing about the POST route.
        let get_only = proof(vec![probe("GET", "/api/login", 404)]);
        assert!(matches!(
            route_responds_outcome(&events, Some(&get_only), "POST", "/api/login", Some(200)),
            EvidenceOutcome::Skip
        ));
    }

    #[test]
    fn a_bare_probe_that_cannot_reproduce_the_declaration_is_not_a_gap() {
        let (events, _rec) = sink();
        for (method, path, got) in [
            // Validation rejected the empty body.
            ("POST", "/api/login", 400),
            ("POST", "/api/login", 422),
            ("PUT", "/api/users/:id", 415),
            // A crash on an absent body is not the declared contract failing.
            ("POST", "/api/orders", 500),
            // Credentials the probe does not have.
            ("GET", "/api/me", 401),
            ("GET", "/api/admin", 403),
            // A login redirect the probe does not follow.
            ("GET", "/dashboard", 302),
            // Something between the probe and the app failed.
            ("GET", "/api/health", 502),
            // `:id` probed as `1`, which may not exist.
            ("GET", "/api/users/{id}", 404),
        ] {
            let concrete = runtime_proof::probe_path(path).unwrap();
            let p = proof(vec![probe(method, &concrete, got)]);
            let outcome = route_responds_outcome(&events, Some(&p), method, path, Some(200));
            assert!(
                matches!(outcome, EvidenceOutcome::Skip),
                "{method} {path} → {got}: {}",
                show(&outcome)
            );
        }
        // A route the dev server could not proxy to its (unstarted) backend.
        let mut proxied = probe("GET", "/api/health", 500);
        proxied.proxy_error = true;
        let p = proof(vec![proxied]);
        assert!(matches!(
            route_responds_outcome(&events, Some(&p), "GET", "/api/health", Some(200)),
            EvidenceOutcome::Skip
        ));
    }

    #[test]
    fn a_route_the_app_does_not_serve_is_still_a_gap() {
        let (events, _rec) = sink();
        for (method, path, got) in [
            ("POST", "/api/login", 404),
            ("POST", "/api/login", 405),
            ("GET", "/api/health", 404),
            ("GET", "/api/health", 500),
            ("GET", "/api/health", 0),
        ] {
            let p = proof(vec![probe(method, path, got)]);
            let outcome = route_responds_outcome(&events, Some(&p), method, path, Some(200));
            assert!(
                matches!(&outcome, EvidenceOutcome::Gap(line)
                    if line.contains(path) && line.contains("http://localhost:3000")),
                "{method} {path} → {got}: {}",
                show(&outcome)
            );
        }
        // Any-OK declarations too.
        let p = proof(vec![probe("GET", "/api/health", 503)]);
        assert!(matches!(
            route_responds_outcome(&events, Some(&p), "GET", "/api/health", None),
            EvidenceOutcome::Skip
        ));
        let p = proof(vec![probe("GET", "/api/health", 500)]);
        assert!(matches!(
            route_responds_outcome(&events, Some(&p), "GET", "/api/health", None),
            EvidenceOutcome::Gap(_)
        ));
        // A required error status is still required.
        let p = proof(vec![probe("GET", "/api/me", 200)]);
        assert!(matches!(
            route_responds_outcome(&events, Some(&p), "GET", "/api/me", Some(401)),
            EvidenceOutcome::Gap(_)
        ));
    }

    #[test]
    fn a_templated_route_matches_its_concrete_probe() {
        let (events, _rec) = sink();
        let p = proof(vec![probe("GET", "/api/users/1", 200)]);
        assert!(matches!(
            route_responds_outcome(&events, Some(&p), "get", "api/users/:id/", Some(200)),
            EvidenceOutcome::Pass
        ));
    }

    #[test]
    fn an_app_that_could_not_boot_is_a_noted_skip() {
        let (events, rec) = sink();
        let mut p = proof(Vec::new());
        p.status = RuntimeStatus::NotVerified("no dev server detected".to_string());
        assert!(matches!(
            route_responds_outcome(&events, Some(&p), "GET", "/api/x", Some(200)),
            EvidenceOutcome::Skip
        ));
        assert!(notes(&rec)
            .iter()
            .any(|n| n.contains("no dev server detected")));
        assert!(matches!(
            route_responds_outcome(&events, None, "GET", "/api/x", Some(200)),
            EvidenceOutcome::Skip
        ));
    }

    #[test]
    fn declared_routes_lists_every_route_contract_with_its_method() {
        let step = PlanStep {
            files: crate::plan_state::StepFiles::default(),
            id: "login-api".into(),
            title: "login api".into(),
            seat: crate::critics::Seat::BackendEngineer,
            kind: crate::plan_state::StepKind::Build,
            depends_on: vec![],
            acceptance: crate::plan_state::AcceptanceSpec::SourcePresent,
            evidence: vec![
                EvidenceContract::BuildClean,
                EvidenceContract::RouteResponds {
                    method: "POST".to_string(),
                    path: "/api/login".to_string(),
                    status: Some(200),
                },
            ],
            status: crate::plan_state::StepStatus::Active,
        };
        assert_eq!(
            declared_routes(&step),
            vec![("POST".to_string(), "/api/login".to_string())]
        );
    }
}
