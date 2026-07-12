//! `umadev-agent` — the spec-aware orchestrator.
//!
//! Drives the `UMADEV_HOST_SPEC_V1` 9-phase pipeline (research → docs
//! → `docs_confirm` → spec → frontend → `preview_confirm` → backend →
//! quality → delivery), honours both confirmation gates, and emits the
//! Layer-4 evidence chain along the way.
//!
//! V1 skeleton — `runner.rs` will be fleshed out as the runtime
//! integration lands. The shape stabilises now so downstream crates
//! (CLI, CI plugins) can already import it.

// `deny`, not `forbid`: the crate is unsafe-free EXCEPT the single audited
// `pre_exec` seam in `spawn_util` (a `#[allow(unsafe_code)]` island), which
// lets the `#![forbid(unsafe_code)]` crates — notably `umadev-tui` — detach a
// spawned child from the controlling terminal without relaxing their policy.
#![deny(unsafe_code)]
#![warn(missing_docs, clippy::all, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::format_push_string,
    clippy::match_same_arms,
    clippy::format_collect,
    clippy::unused_async,
    clippy::ref_option,
    clippy::single_char_pattern,
    clippy::items_after_statements,
    clippy::let_unit_value,
    clippy::match_single_binding,
    clippy::map_unwrap_or,
    clippy::cast_possible_wrap
)]

pub mod acceptance;
pub mod adopt;
pub mod agents;
pub mod app_runtime;
pub mod arch_fitness;
pub mod ask_question;
pub mod base_error;
pub mod base_gate;
pub mod checkpoint;
pub mod coach;
pub mod compaction;
pub mod config;
pub mod constitution;
pub mod context;
pub mod continuous;
pub mod coverage;
pub mod critics;
pub mod deploy;
pub mod director;
pub mod director_loop;
pub mod error_kb;
pub mod events;
pub mod experts;
pub mod fact_extract;
pub mod first_pass;
pub(crate) mod fswalk;
pub mod gates;
pub mod interaction;
pub mod knowledge_feedback;
pub mod lessons;
pub mod manifest;
pub mod materialize;
pub mod open_decisions;
pub mod phases;
pub mod plan_state;
pub mod planner;
pub mod pr;
pub mod project_facts;
pub mod recipes;
pub mod review;
pub mod router;
pub mod run_lock;
pub mod runner;
pub mod runtime_proof;
pub mod scaffolding;
pub mod security;
pub mod self_evolve;
pub mod sizing_calibration;
pub mod skills;
pub mod spawn_util;
pub mod state;
pub mod tech_debt;
pub mod test_integrity;
pub mod trust;
pub mod verify;

#[cfg(test)]
mod test_support;

pub use adopt::{
    is_adopted, load_project_source_index, read_adopt_marker, run_adopt, AdoptReport,
    DetectedCommand,
};
pub use app_runtime::{app_calls_llm_at_runtime, runtime_model_directive, stated_runtime_model};
pub use ask_question::{
    exit_plan_note, exit_plan_surface, note_for as ask_question_note,
    prefers_text_questions as prefers_text_questions_flag,
    relay_directive as ask_question_relay_directive,
    relay_or_passthrough as ask_question_relay_or_passthrough, set_prefer_text_questions,
    should_wait_for_question, surface as ask_question_surface, AskQuestionSurface, ExitPlanSurface,
};
pub use checkpoint::{
    create_run_baseline, rollback_run, run_baseline, Checkpoint, RUN_BASELINE_PREFIX,
};
pub use constitution::{
    constitution_rel_path, ensure_constitution, read_constitution, regenerate_constitution,
    render_constitution, user_charter_firmware_block, ConstitutionDoc,
};
pub use context::{compose_firmware, project_context, FIRMWARE_BUDGET};
pub use continuous::{
    continuous_enabled_from_env, legacy_pipeline_from_env, run_block as run_continuous_block,
    ReviewKind, RunOutcome,
};
pub use critics::{
    append_team_ledger, docs_team_for_kind, preview_team_for_kind, quality_team_for_kind,
    ArchitectureCritic, BackendCritic, CriticArtifacts, CriticConsult, DevOpsCritic,
    FrontendCritic, PmCritic, QaCritic, RoleCritic, RoleVerdict, Seat, SecurityCritic, UiuxCritic,
};
pub use deploy::{
    deploy_proof_rel_path, detect_deploy_target, run_deploy, write_deploy_proof, DeployProof,
    DeployStatus, DeployTarget,
};
pub use director::{
    checkpoint as director_checkpoint, review as director_review, summon as director_summon,
    verify as director_verify, CheckpointDecision, ReviewResult, SummonMode, SummonResult,
    VerifyKind, VerifyResult,
};
pub use director_loop::{
    drive_director_loop, drive_director_loop_resume, drive_director_loop_routed,
    has_resumable_director_plan, has_resumable_run, run_post_build_qc, DirectorLoopOutcome,
};
pub use events::{ChannelSink, EngineEvent, EventSink, NullSink, RecordingSink};
pub use first_pass::{
    autonomy_default as first_pass_autonomy_default, class_kind as first_pass_class_kind,
    first_pass_rate, low_confidence_nudge as first_pass_low_confidence_nudge,
    seat_kind as first_pass_seat_kind, FirstPassStats, KindStat,
};
pub use gates::{
    claims_code_changes, classify_reply, Gate, GateChoice, GateChoiceOption, GateDecision,
    GateOutcome,
};
pub use interaction::{
    hosted as hosted_interaction, ApprovalFn, ApprovalFuture, RunInteraction, SteerIntake,
};
pub use lessons::{
    apply_dev_error_trust, apply_trust_for_identities, apply_trust_for_signatures,
    capture_dev_errors, capture_gate_revision, capture_quality_failures,
    capture_validated_patterns, fold_beliefs, lessons_report, list_sedimented_lessons,
    parse_reconcile_decision, pitfall_efficacy_summary, pitfall_overview, reconcile_candidates,
    reconcile_prompt, resolve_new_lesson_conflicts, scan_contradictions, sediment_lessons,
    sediment_lessons_with_judge, Lesson, LessonsReport, PitfallEfficacySummary, PitfallEntry,
    PitfallStatus, ReconcileDecision, ValidatedEntry,
};
pub use manifest::{ConformanceLevel, Profile, SpecManifest};
pub use open_decisions::{
    append_decision, counts as open_decision_counts, decisions_directive, decisions_recall_block,
    load_decisions, unresolved as unresolved_decisions, DecisionStatus, NewDecision, OpenDecision,
    DECISIONS_FIRMWARE_BUDGET, REGISTER_REL_PATH as OPEN_DECISIONS_REL_PATH,
};
pub use phases::{
    agentic_knowledge_digest, knowledge_top_files, phase_knowledge_digest, PhaseOutput,
};
pub use plan_state::{
    load as load_plan, save as save_plan, synthesize_plan, AcceptanceSpec, EvidenceContract, Plan,
    PlanStep, StepKind, StepStatus,
};
pub use planner::{
    advisory_prior, phase_from_id, plan as plan_phases, plan_light, redoable_phase_ids, PhasePlan,
    TaskKind,
};
pub use pr::{
    assess_readiness, ensure_isolation_branch, feature_branch_name, is_isolation_branch,
    latest_proof_pack, manual_steps, plan_branches, pr_body_rel_path, proof_pack_summary,
    render_pr_body, BranchIsolation, PrPlan, PrReadiness, ReadinessCheck,
};
pub use recipes::{
    capture_recipe, fingerprint_for, load_recipes, recall_best, recall_prior_block,
    recipe_prior_block, recipes_dir, Fingerprint, OutcomeStats, Recipe, RECIPE_PRIOR_BUDGET,
};
pub use review::{
    build_review_report, render_review_md, review_report_rel_path, scan_ci_weakening,
    write_review_report, ReviewClaim, ReviewReport, Verdict,
};
pub use router::{
    looks_like_work_request, route, Budget, ClarifyQuestion, Depth, RouteClass, RoutePlan,
};
pub use runner::{
    setup_run_isolation, strict_coverage_from_env, AgentRunner, RunOptions, RunReport,
};
pub use runtime_proof::{
    run_runtime_proof, runtime_proof_rel_path, write_runtime_proof, E2eResult, RouteProbe,
    RuntimeProof, RuntimeStatus,
};
pub use security::{
    run_security_scan, security_scan_rel_path, write_security_scan, ScanResult, ScanStatus,
    SecurityScan,
};
pub use sizing_calibration::{
    advisory_nudge as sizing_advisory_nudge, calibrated_default as sizing_calibrated_default,
    predicted_size, record_route as record_run_sizing, sizing_calibration, ClassSizing, SizeRank,
    SizingAdjustment, SizingStats,
};
pub use skills::{
    graduate_skill, graduate_validated_patterns, read_skills, retrieve_skills,
    skill_description_prompt, skills_for_prompt, skills_report, Skill,
};
pub use spawn_util::{
    detach_from_controlling_terminal, detach_kind, kill_process_group, DetachKind,
};
pub use state::{
    list_snapshots, read_workflow_state, read_workflow_state_diagnostic, restore_snapshot,
    unfinished_plan_summary, write_workflow_state, ReadState, WorkflowState,
};
pub use test_integrity::{
    check as check_test_integrity, snapshot as snapshot_test_surface, TestSnapshot,
};
pub use trust::{
    capability_class, capability_requires_confirmation, guarded_should_pause_item,
    remember_project_approval, requires_confirmation, requires_confirmation_with_ledger,
    reversibility_class, Capability, CapabilityPolicy, CircuitBreaker, ConsecutiveFailureBreaker,
    GateTrust, Reversibility, TrustLedger, TrustMode, TrustSuggestion, CIRCUIT_THRESHOLD,
    CIRCUIT_WINDOW_SECS, CONSECUTIVE_FAILURE_THRESHOLD,
};
pub use verify::{detect_project, record_verify_outcome, run_verify, ProjectKind, VerifyOutcome};
