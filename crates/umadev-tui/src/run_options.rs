//! Run-option construction and fresh-run state settlement.

use crate::app::App;
use crate::LaunchOptions;
use umadev_agent::RunOptions;

/// Build the user-facing note for a failed runner start.
pub(super) fn start_failed_note(error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        umadev_i18n::tl("run.busy_reopen").to_string()
    } else {
        umadev_i18n::tlf("pipeline.start_failed", &[&error.to_string()])
    }
}

/// Close a parked review before a fresh single-shot run replaces its identity.
pub(super) fn settle_operational_review_before_fresh_block(
    options: &RunOptions,
    resume_existing_state: bool,
) -> Result<(), String> {
    if resume_existing_state || !options.mode.executes() {
        return Ok(());
    }
    umadev_agent::cancel_operational_review_pause(
        &options.project_root,
        "superseded by an explicitly fresh run",
    )
    .map(|_| ())
}

/// Build options for a fresh run from the current TUI state.
pub(super) fn current_run_options(app: &App, options: &LaunchOptions) -> RunOptions {
    RunOptions {
        project_root: options.project_root.clone(),
        requirement: app.requirement.clone(),
        slug: app.slug.clone(),
        model: String::new(),
        backend: app.backend.clone().unwrap_or_default(),
        design_system: app.config.design_system.clone().unwrap_or_default(),
        seed_template: app.config.seed_template.clone().unwrap_or_default(),
        mode: app.effective_trust_mode(),
        // Snapshot the opt-in once; parallel runners never race on live env reads.
        strict_coverage: umadev_agent::strict_coverage_from_env(),
    }
}

/// Resolve a continuation's permission posture from the workflow that created it.
pub(super) fn persisted_run_mode(
    project_root: &std::path::Path,
    fallback: umadev_agent::TrustMode,
) -> umadev_agent::TrustMode {
    // The current Plan selection is a hard non-widening ceiling.
    if fallback == umadev_agent::TrustMode::Plan {
        return fallback;
    }
    umadev_agent::read_workflow_state(project_root).map_or(fallback, |state| {
        umadev_agent::TrustMode::from_base_permissions(state.resolved_permission_profile())
    })
}

/// Build options for `/continue`, gate revision, and `/redo`.
pub(super) fn resume_run_options(app: &App, options: &LaunchOptions) -> RunOptions {
    let mut run_options = current_run_options(app, options);
    run_options.mode = persisted_run_mode(&options.project_root, run_options.mode);
    run_options
}
