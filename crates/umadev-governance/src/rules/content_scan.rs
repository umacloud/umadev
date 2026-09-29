use super::{
    check_ai_slop_with_intent, is_server_surface_rule, run_check_guarded, Decision, ProjectContext,
    CONTENT_CHECKS,
};
use std::collections::HashSet;

pub(super) fn run_ai_slop_guarded(file_path: &str, content: &str, ctx: ProjectContext) -> Decision {
    let intent = crate::design::DesignIntent {
        purple_allowed: ctx.purple_allowed,
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_ai_slop_with_intent(file_path, content, intent)
    }))
    .unwrap_or_else(|_| Decision::pass())
}

/// Collect every enabled content-governance finding for one file.
///
/// The real-time write path returns after the first block. Audits and
/// report-only CI need the full set, so this uses the same ordered registry,
/// policy, context, panic isolation, and design intent in one pass.
#[must_use]
pub fn scan_content_findings_with_context(
    file_path: &str,
    content: &str,
    policy: &crate::policy::Policy,
    ctx: ProjectContext,
) -> Vec<Decision> {
    if policy.is_excluded(file_path) {
        return Vec::new();
    }

    let skip_surface = ctx.skip_server_surface(file_path, content);
    let mut clauses = HashSet::new();
    let mut findings = Vec::new();
    for &check in CONTENT_CHECKS {
        if (skip_surface && is_server_surface_rule(check)) || super::i18n::stands_down(check, ctx) {
            continue;
        }
        let decision = run_check_guarded(check, file_path, content);
        if decision.block
            && !policy.is_disabled(&decision.clause)
            && clauses.insert(decision.clause.clone())
        {
            findings.push(decision);
        }
    }

    let slop = run_ai_slop_guarded(file_path, content, ctx);
    if slop.block && !policy.is_disabled(&slop.clause) && clauses.insert(slop.clause.clone()) {
        findings.push(slop);
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::scan_content;

    #[test]
    fn uses_the_write_gate_order_policy_and_context() {
        let source = "export function echo(value: any) { console.log(value); return value; }";
        let findings = scan_content_findings_with_context(
            "src/echo.ts",
            source,
            &crate::policy::Policy::default(),
            ProjectContext::unknown(),
        );
        let clauses: Vec<&str> = findings
            .iter()
            .map(|finding| finding.clause.as_str())
            .collect();
        assert!(clauses.contains(&"UD-ARCH-001"), "{clauses:?}");
        assert!(clauses.contains(&"UD-ARCH-002"), "{clauses:?}");
        assert_eq!(
            scan_content("src/echo.ts", source).clause,
            findings[0].clause
        );

        let mut policy = crate::policy::Policy::default();
        policy.disabled.clauses.push("UD-ARCH-001".to_string());
        let filtered = scan_content_findings_with_context(
            "src/echo.ts",
            source,
            &policy,
            ProjectContext::unknown(),
        );
        assert!(filtered
            .iter()
            .all(|finding| finding.clause != "UD-ARCH-001"));
        assert!(filtered
            .iter()
            .any(|finding| finding.clause == "UD-ARCH-002"));
    }
}
