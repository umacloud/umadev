/// The observable result of driving one plan step. The flags are independent
/// observations, not an enum: acceptance, a driven turn, and real progress can
/// differ when a check is neutral, a reviewer is unavailable, or a base dies.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct StepOutcome {
    pub(super) accepted: bool,
    pub(super) reply: String,
    pub(super) drove: bool,
    pub(super) made_progress: bool,
    pub(super) unavailable: bool,
    pub(super) base_agents: crate::bg_agents::BaseAgentObservation,
    /// Semantic evidence eligible for an explicit source-repair run.
    pub(super) gap_evidence: Vec<String>,
    /// Host/reviewer availability evidence that must never trigger source edits.
    pub(super) operational_unavailable: Vec<String>,
}
