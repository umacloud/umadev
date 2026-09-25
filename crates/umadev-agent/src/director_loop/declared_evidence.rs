//! Evidence checks for a plan step's declared `test-fails-then-passes` contract,
//! kept out of the director loop itself.

use super::EvidenceOutcome;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_that_did_not_exist_before_the_step_was_red() {
        assert!(matches!(
            red_half_verdict("adds_numbers", NamedTestOutcome::NotFound),
            Some(EvidenceOutcome::Pass)
        ));
        assert!(named_test_not_run("adds_numbers").contains("\"adds_numbers\""));
    }
}
