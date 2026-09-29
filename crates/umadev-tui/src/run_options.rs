//! Run-option construction and fresh-run state settlement.

use crate::app::App;
use crate::LaunchOptions;
use umadev_agent::{RunOptions, TrustMode};

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

/// The tier a continuation runs under: always the session's current tier.
///
/// A resume never acts with more authority than the footer chip shows, and a
/// tier the user picks at a paused gate applies from that gate on. When the
/// saved run had more authority, say once how to give it back; the resumed
/// run saves the current tier, so the next resume is silent.
pub(super) fn resume_run_mode(app: &mut App, project_root: &std::path::Path) -> TrustMode {
    let current = app.effective_trust_mode();
    if let Some(saved) = saved_run_mode_wider_than(project_root, current) {
        app.push_workspace_notice(umadev_i18n::tf(
            app.lang,
            "run.resume_mode_narrowed",
            &[saved.as_str(), current.as_str(), saved.as_str()],
        ));
    }
    current
}

/// The tier saved with the workflow being resumed, when it granted more
/// authority than `current`. A saved tier counts only as far as workspace
/// trust honours it: run state in a project the user does not trust, or that
/// this installation did not write, never asks for more than Guarded.
pub(super) fn saved_run_mode_wider_than(
    project_root: &std::path::Path,
    current: TrustMode,
) -> Option<TrustMode> {
    let state = umadev_agent::read_workflow_state(project_root)?;
    let saved = umadev_agent::workspace_trust::resume_tier(
        project_root,
        TrustMode::from_base_permissions(state.resolved_permission_profile()),
        umadev_agent::workspace_trust::is_trusted(project_root),
    );
    saved.is_downgrade_to(current).then_some(saved)
}

/// Build options for `/continue`, gate revision, and `/redo`.
pub(super) fn resume_run_options(app: &mut App, options: &LaunchOptions) -> RunOptions {
    let mut run_options = current_run_options(app, options);
    run_options.mode = resume_run_mode(app, &options.project_root);
    run_options
}
