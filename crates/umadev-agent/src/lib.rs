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

#![forbid(unsafe_code)]
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
pub mod checkpoint;
pub mod coach;
pub mod config;
pub mod continuous;
pub mod coverage;
pub mod critics;
pub mod deploy;
pub mod error_kb;
pub mod events;
pub mod experts;
pub mod gates;
pub mod lessons;
pub mod manifest;
pub mod phases;
pub mod planner;
pub mod pr;
pub mod review;
pub mod run_lock;
pub mod runner;
pub mod runtime_proof;
pub mod scaffolding;
pub mod security;
pub mod skills;
pub mod state;
pub mod tech_debt;
pub mod trust;
pub mod verify;

pub use adopt::{
    is_adopted, load_project_source_index, read_adopt_marker, run_adopt, AdoptReport,
    DetectedCommand,
};
pub use continuous::{continuous_enabled_from_env, run_block as run_continuous_block, RunOutcome};
pub use critics::{
    append_team_ledger, docs_team_for_kind, preview_team_for_kind, quality_team_for_kind,
    ArchitectureCritic, BackendCritic, CriticArtifacts, CriticConsult, DevOpsCritic,
    FrontendCritic, PmCritic, QaCritic, RoleCritic, RoleVerdict, SecurityCritic, UiuxCritic,
};
pub use deploy::{
    deploy_proof_rel_path, detect_deploy_target, run_deploy, write_deploy_proof, DeployProof,
    DeployStatus, DeployTarget,
};
pub use events::{ChannelSink, EngineEvent, EventSink, NullSink, RecordingSink};
pub use gates::{classify_reply, Gate, GateOutcome};
pub use lessons::{
    apply_dev_error_trust, apply_trust_for_identities, apply_trust_for_signatures,
    capture_dev_errors, capture_gate_revision, capture_quality_failures,
    capture_validated_patterns, fold_beliefs, lessons_report, list_sedimented_lessons,
    parse_reconcile_decision, pitfall_efficacy_summary, pitfall_overview, reconcile_candidates,
    reconcile_prompt, scan_contradictions, sediment_lessons, sediment_lessons_with_judge, Lesson,
    LessonsReport, PitfallEfficacySummary, PitfallEntry, PitfallStatus, ReconcileDecision,
    ValidatedEntry,
};
pub use manifest::{ConformanceLevel, Profile, SpecManifest};
pub use phases::{knowledge_top_files, phase_knowledge_digest, PhaseOutput};
pub use planner::{
    phase_from_id, plan as plan_phases, plan_light, redoable_phase_ids, PhasePlan, TaskKind,
};
pub use pr::{
    assess_readiness, feature_branch_name, latest_proof_pack, manual_steps, plan_branches,
    pr_body_rel_path, proof_pack_summary, render_pr_body, PrPlan, PrReadiness, ReadinessCheck,
};
pub use review::{
    build_review_report, render_review_md, review_report_rel_path, scan_ci_weakening,
    write_review_report, ReviewClaim, ReviewReport, Verdict,
};
pub use runner::{strict_coverage_from_env, AgentRunner, RunOptions, RunReport};
pub use runtime_proof::{
    run_runtime_proof, runtime_proof_rel_path, write_runtime_proof, E2eResult, RouteProbe,
    RuntimeProof, RuntimeStatus,
};
pub use security::{
    run_security_scan, security_scan_rel_path, write_security_scan, ScanResult, ScanStatus,
    SecurityScan,
};
pub use skills::{
    graduate_skill, graduate_validated_patterns, read_skills, retrieve_skills,
    skill_description_prompt, skills_for_prompt, skills_report, Skill,
};
pub use state::{
    list_snapshots, read_workflow_state, read_workflow_state_diagnostic, restore_snapshot,
    write_workflow_state, ReadState, WorkflowState,
};
pub use trust::{
    requires_confirmation, reversibility_class, GateTrust, Reversibility, TrustLedger, TrustMode,
    TrustSuggestion,
};
pub use verify::{detect_project, run_verify, ProjectKind, VerifyOutcome};
