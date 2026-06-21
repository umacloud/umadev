//! `umadev-spec` — the UmaDev Host Specification, V1, as Rust data.
//!
//! This crate is intentionally **dependency-light** and **pure-data**.
//! Every other UmaDev crate reads from here to know:
//!
//! - which clauses to enforce (`CLAUSES`)
//! - which phases to run, in what order (`PHASES`, `PHASE_CHAIN`)
//! - which gates to honour (`GATES`)
//!
//! The canonical normative prose ships alongside this code at
//! `spec/UMADEV_HOST_SPEC_V1.md`. The data here MUST stay in sync
//! with that document; tests pin the contract.
//!
//! Stability contract:
//! - Clause IDs are permanent. Once an ID lands here, it never changes.
//! - Phase identifiers are reserved keywords (see `UMADEV_HOST_SPEC_V1`
//!   Appendix A).

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::doc_markdown,
    clippy::must_use_candidate
)]

use serde::{Deserialize, Serialize};

/// Canonical version string. Bumped on every backwards-incompatible
/// change to the data shape or to any normative clause.
pub const SPEC_VERSION: &str = "UMADEV_HOST_SPEC_V1";

/// Conformance keywords (RFC 2119) attached to each clause.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ClauseLevel {
    /// The clause is absolute; non-compliance breaks conformance.
    Must,
    /// The clause is strongly recommended; deviation requires justification.
    Should,
    /// The clause is optional but standardised.
    May,
}

/// The five normative layers (four numbered + cross-cutting meta).
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layer {
    /// Layer 1 — code-weight constraints (`UD-CODE-*`).
    Code,
    /// Layer 2 — flow contract (`UD-FLOW-*`).
    Flow,
    /// Layer 3 — delivery artifacts (`UD-ART-*`).
    Artifacts,
    /// Layer 4 — evidence chain (`UD-EVID-*`).
    Evidence,
    /// Cross-cutting meta (`UD-META-*`).
    Meta,
}

/// One normative clause. The `id` is the stable identifier (e.g.
/// `UD-CODE-001`); it never changes once published.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Clause {
    /// Stable identifier, e.g. `UD-CODE-001`. Never renamed.
    pub id: &'static str,
    /// Layer this clause belongs to.
    pub layer: Layer,
    /// One-line summary suitable for log messages.
    pub title: &'static str,
    /// RFC 2119 conformance level.
    pub level: ClauseLevel,
    /// Section number in `UMADEV_HOST_SPEC_V1.md` (e.g. `"3.1"`).
    pub section: &'static str,
}

/// Phase of the UmaDev pipeline. Identifiers are reserved (see
/// Appendix A of the spec) and MUST NOT be renamed by conformant hosts.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Read knowledge base + research similar products.
    Research,
    /// Produce the three core documents (PRD, architecture, UIUX).
    Docs,
    /// Gate — wait for explicit user confirmation of the docs.
    DocsConfirm,
    /// Translate confirmed docs into spec + tasks.
    Spec,
    /// Frontend-first implementation.
    Frontend,
    /// Gate — wait for explicit user approval of the runnable preview.
    PreviewConfirm,
    /// Backend implementation + integration with the frontend contract.
    Backend,
    /// Quality gates, red-team, audit, evidence chain.
    Quality,
    /// Proof-pack assembly, release readiness, hand-off.
    Delivery,
}

impl Phase {
    /// Canonical, stable identifier as used in `workflow-state.json`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Docs => "docs",
            Self::DocsConfirm => "docs_confirm",
            Self::Spec => "spec",
            Self::Frontend => "frontend",
            Self::PreviewConfirm => "preview_confirm",
            Self::Backend => "backend",
            Self::Quality => "quality",
            Self::Delivery => "delivery",
        }
    }

    /// Whether this phase is a gate that pauses the pipeline awaiting
    /// explicit user confirmation.
    #[must_use]
    pub const fn is_gate(self) -> bool {
        matches!(self, Self::DocsConfirm | Self::PreviewConfirm)
    }
}

/// A confirmation gate. Both gates are MUST under `UMADEV_HOST_SPEC_V1`
/// at conformance level L2 (Enforced) and above.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// Gate after the `docs` phase. Implements `UD-FLOW-002`.
    DocsConfirm,
    /// Gate after the `frontend` phase. Implements `UD-FLOW-003`.
    PreviewConfirm,
}

impl Gate {
    /// Canonical identifier persisted to `workflow-state.json#active_gate`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::DocsConfirm => "docs_confirm",
            Self::PreviewConfirm => "preview_confirm",
        }
    }

    /// Inverse of [`Gate::id`]: parse a persisted gate id back into the
    /// typed enum. Returns `None` for unknown ids (fail-open). Case-
    /// insensitive so a workflow-state file written as `Docs_Confirm`
    /// still resolves. Replaces the ad-hoc string matches the CLI
    /// previously sprinkled across `main.rs` / `runner.rs`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "docs_confirm" => Some(Self::DocsConfirm),
            "preview_confirm" => Some(Self::PreviewConfirm),
            _ => None,
        }
    }

    /// User replies that count as explicit approval (per `UD-FLOW-002`).
    ///
    /// Hosts MAY extend this set but MUST NOT infer approval from
    /// unrelated input.
    #[must_use]
    pub const fn approval_tokens(self) -> &'static [&'static str] {
        &[
            "确认", "通过", "继续", "approved", "approve", "lgtm", "ship it",
        ]
    }
}

/// Canonical phase chain — the order MUST be honoured by conformant hosts.
pub const PHASE_CHAIN: &[Phase] = &[
    Phase::Research,
    Phase::Docs,
    Phase::DocsConfirm,
    Phase::Spec,
    Phase::Frontend,
    Phase::PreviewConfirm,
    Phase::Backend,
    Phase::Quality,
    Phase::Delivery,
];

/// All normative clauses, indexed by stable ID.
pub const CLAUSES: &[Clause] = &[
    // --- Layer 1 — code-weight constraints (UD-CODE-*) ---
    Clause {
        id: "UD-CODE-001",
        layer: Layer::Code,
        title: "Emoji as functional icons",
        level: ClauseLevel::Must,
        section: "3.1",
    },
    Clause {
        id: "UD-CODE-002",
        layer: Layer::Code,
        title: "Hardcoded color literals",
        level: ClauseLevel::Must,
        section: "3.2",
    },
    Clause {
        id: "UD-CODE-003",
        layer: Layer::Code,
        title: "Frontend-backend API path alignment",
        level: ClauseLevel::Should, // MUST at L3
        section: "3.3",
    },
    Clause {
        id: "UD-CODE-004",
        layer: Layer::Code,
        title: "Tech-stack pre-research",
        level: ClauseLevel::Should,
        section: "3.4",
    },
    // --- Layer 2 — flow contract (UD-FLOW-*) ---
    Clause {
        id: "UD-FLOW-001",
        layer: Layer::Flow,
        title: "Phase chain",
        level: ClauseLevel::Must,
        section: "4.1",
    },
    Clause {
        id: "UD-FLOW-002",
        layer: Layer::Flow,
        title: "Docs confirmation gate",
        level: ClauseLevel::Must,
        section: "4.2",
    },
    Clause {
        id: "UD-FLOW-003",
        layer: Layer::Flow,
        title: "Preview confirmation gate",
        level: ClauseLevel::Must,
        section: "4.3",
    },
    Clause {
        id: "UD-FLOW-004",
        layer: Layer::Flow,
        title: "Gate-local revisions",
        level: ClauseLevel::Must,
        section: "4.4",
    },
    Clause {
        id: "UD-FLOW-005",
        layer: Layer::Flow,
        title: "Phase-local artifact mutability",
        level: ClauseLevel::Must,
        section: "4.5",
    },
    Clause {
        id: "UD-FLOW-006",
        layer: Layer::Flow,
        title: "Session continuity",
        level: ClauseLevel::Must,
        section: "4.6",
    },
    // --- Layer 3 — delivery artifacts (UD-ART-*) ---
    Clause {
        id: "UD-ART-001",
        layer: Layer::Artifacts,
        title: "Research artifacts",
        level: ClauseLevel::Must,
        section: "5.1",
    },
    Clause {
        id: "UD-ART-002",
        layer: Layer::Artifacts,
        title: "Core documents",
        level: ClauseLevel::Must,
        section: "5.2",
    },
    Clause {
        id: "UD-ART-003",
        layer: Layer::Artifacts,
        title: "Spec + tasks",
        level: ClauseLevel::Must,
        section: "5.3",
    },
    Clause {
        id: "UD-ART-004",
        layer: Layer::Artifacts,
        title: "ADR records",
        level: ClauseLevel::Should,
        section: "5.4",
    },
    Clause {
        id: "UD-ART-005",
        layer: Layer::Artifacts,
        title: "Mutability of artifacts",
        level: ClauseLevel::Must,
        section: "5.5",
    },
    Clause {
        id: "UD-ART-006",
        layer: Layer::Artifacts,
        title: "No chat-only completion",
        level: ClauseLevel::Must,
        section: "5.6",
    },
    // --- Layer 4 — evidence chain (UD-EVID-*) ---
    Clause {
        id: "UD-EVID-001",
        layer: Layer::Evidence,
        title: "API audit log",
        level: ClauseLevel::Must, // at L3
        section: "6.1",
    },
    Clause {
        id: "UD-EVID-002",
        layer: Layer::Evidence,
        title: "Tool-call audit log",
        level: ClauseLevel::Should, // at L3
        section: "6.2",
    },
    Clause {
        id: "UD-EVID-003",
        layer: Layer::Evidence,
        title: "Quality report",
        level: ClauseLevel::Must, // at L3
        section: "6.3",
    },
    Clause {
        id: "UD-EVID-004",
        layer: Layer::Evidence,
        title: "Compliance mapping",
        level: ClauseLevel::Should, // at L3
        section: "6.4",
    },
    Clause {
        id: "UD-EVID-005",
        layer: Layer::Evidence,
        title: "Proof pack",
        level: ClauseLevel::Must, // at L3
        section: "6.5",
    },
    // --- Cross-cutting meta (UD-META-*) ---
    Clause {
        id: "UD-META-001",
        layer: Layer::Meta,
        title: "Spec manifest",
        level: ClauseLevel::Must,
        section: "8.1",
    },
    Clause {
        id: "UD-META-002",
        layer: Layer::Meta,
        title: "Version negotiation",
        level: ClauseLevel::Must,
        section: "8.2",
    },
    Clause {
        id: "UD-META-003",
        layer: Layer::Meta,
        title: "Backward compatibility",
        level: ClauseLevel::Must,
        section: "8.3",
    },
    Clause {
        id: "UD-META-004",
        layer: Layer::Meta,
        title: "Profiles",
        level: ClauseLevel::May,
        section: "8.4",
    },
];

/// Look up a clause by its stable ID. `None` if the ID is not in V1.
#[must_use]
pub fn get_clause(id: &str) -> Option<&'static Clause> {
    CLAUSES.iter().find(|c| c.id == id)
}

/// Return all clauses in a given layer.
#[must_use]
pub fn clauses_in_layer(layer: Layer) -> Vec<&'static Clause> {
    CLAUSES.iter().filter(|c| c.layer == layer).collect()
}

/// Officially supported host runtimes — the two families with first-
/// party Agent SDKs.
///
/// Hosts outside this enum are explicitly out of scope for UmaDev 3.x;
/// they require shallow rules-only adaptation which UmaDev does not
/// pursue.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    /// Anthropic Claude — Claude Agent SDK; surfaces: Claude Code, Claude Desktop.
    Anthropic,
    /// `OpenAI` Codex — `OpenAI` Agents SDK; surfaces: Codex CLI, Codex Desktop.
    Openai,
}

impl RuntimeKind {
    /// Stable lower-case identifier used as CLI flag value and config key.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
        }
    }

    /// Display name for human-facing output.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (Claude Agent SDK)",
            Self::Openai => "OpenAI (Agents SDK)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn clause_ids_are_unique() {
        let ids: HashSet<_> = CLAUSES.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), CLAUSES.len(), "duplicate clause IDs");
    }

    #[test]
    fn clause_ids_follow_format() {
        for c in CLAUSES {
            let parts: Vec<_> = c.id.split('-').collect();
            assert_eq!(
                parts.len(),
                3,
                "clause id {} not in UD-LAYER-NNN form",
                c.id
            );
            assert_eq!(parts[0], "UD");
            assert_eq!(parts[2].len(), 3);
            assert!(parts[2].chars().all(|ch| ch.is_ascii_digit()));
        }
    }

    #[test]
    fn phase_chain_starts_at_research_ends_at_delivery() {
        assert_eq!(PHASE_CHAIN.first().copied(), Some(Phase::Research));
        assert_eq!(PHASE_CHAIN.last().copied(), Some(Phase::Delivery));
    }

    #[test]
    fn phase_chain_is_complete_and_ids_unique() {
        // Lock the chain shape so a future edit can't silently drop a phase or
        // alias two `Phase::id`s (which would corrupt planner execution plans).
        assert_eq!(PHASE_CHAIN.len(), 9, "the canonical chain has nine phases");
        let ids: std::collections::HashSet<&str> = PHASE_CHAIN.iter().map(|p| p.id()).collect();
        assert_eq!(ids.len(), PHASE_CHAIN.len(), "every phase id is distinct");
    }

    #[test]
    fn phase_chain_includes_both_gates() {
        let gates: Vec<_> = PHASE_CHAIN.iter().filter(|p| p.is_gate()).collect();
        assert_eq!(gates.len(), 2);
    }

    #[test]
    fn get_clause_finds_known_id() {
        let c = get_clause("UD-CODE-001").expect("UD-CODE-001 should exist");
        assert_eq!(c.layer, Layer::Code);
        assert_eq!(c.level, ClauseLevel::Must);
    }

    #[test]
    fn get_clause_returns_none_for_unknown_id() {
        assert!(get_clause("UD-CODE-999").is_none());
    }

    #[test]
    fn every_layer_has_at_least_one_clause() {
        for layer in [
            Layer::Code,
            Layer::Flow,
            Layer::Artifacts,
            Layer::Evidence,
            Layer::Meta,
        ] {
            assert!(
                !clauses_in_layer(layer).is_empty(),
                "layer {layer:?} has no clauses"
            );
        }
    }

    #[test]
    fn spec_version_constant_matches_marker() {
        assert_eq!(SPEC_VERSION, "UMADEV_HOST_SPEC_V1");
    }

    #[test]
    fn runtime_ids_are_distinct() {
        let ids: HashSet<_> = [RuntimeKind::Anthropic, RuntimeKind::Openai]
            .iter()
            .map(|r| r.id())
            .collect();
        assert_eq!(ids.len(), 2);
    }

    /// The normative prose, embedded at compile time. The relative path is
    /// from this file (`crates/umadev-spec/src/lib.rs`) up to the workspace
    /// root, then into `spec/`. If the prose is ever moved, this `include_str!`
    /// fails the build — which is exactly the lockstep guarantee we want.
    const SPEC_PROSE: &str = include_str!("../../../spec/UMADEV_HOST_SPEC_V1.md");

    /// The boundary marker after which clause IDs are non-normative V2
    /// candidates (§10 "Future work"). IDs that appear only beyond this point
    /// are intentionally *not* in [`CLAUSES`].
    const FUTURE_WORK_MARKER: &str = "## 10. Future work";

    /// Forward lockstep: every clause carried as data MUST appear, verbatim,
    /// in the normative prose. This is the half that actually caught the HOST
    /// drift — data and `.md` can no longer diverge silently. Enforces the
    /// CLAUDE.md "Spec sync contract".
    #[test]
    fn every_clause_id_appears_in_the_prose() {
        for c in CLAUSES {
            assert!(
                SPEC_PROSE.contains(c.id),
                "clause {} is in CLAUSES but not in spec/UMADEV_HOST_SPEC_V1.md \
                 — the data and the normative prose have drifted",
                c.id
            );
        }
    }

    /// Forward lockstep, part two: every clause's declared `section` number
    /// MUST head a real `### <section>` heading in the prose, so a clause
    /// can't point at a section that doesn't exist (or was renumbered).
    #[test]
    fn every_clause_section_heading_exists_in_the_prose() {
        for c in CLAUSES {
            let heading = format!("### {} ", c.section);
            assert!(
                SPEC_PROSE.contains(&heading),
                "clause {} claims section {} but no `### {} …` heading exists \
                 in spec/UMADEV_HOST_SPEC_V1.md",
                c.id,
                c.section,
                c.section
            );
        }
    }

    /// Reverse lockstep: every `UD-XXX-NNN` identifier mentioned in the
    /// normative body of the prose (everything *before* §10 "Future work")
    /// MUST be backed by a clause in [`CLAUSES`]. IDs that live only in §10
    /// are documented V2 candidates and are deliberately excluded. This is
    /// the half that locks out a `UD-HOST-*` (or any other) layer being
    /// described in prose without a data entry.
    #[test]
    fn every_normative_prose_id_is_a_known_clause() {
        let normative = SPEC_PROSE
            .split_once(FUTURE_WORK_MARKER)
            .map_or(SPEC_PROSE, |(before, _)| before);

        let known: HashSet<&str> = CLAUSES.iter().map(|c| c.id).collect();

        // Scan for `UD-<UPPER>-<3 digits>` tokens without pulling in a regex
        // dep (this crate is deliberately dependency-light).
        for raw in normative.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-')) {
            let token = raw.trim_matches('-');
            let mut parts = token.split('-');
            let is_clause_id = parts.next() == Some("UD")
                && parts.next().is_some_and(|layer| {
                    !layer.is_empty() && layer.chars().all(|c| c.is_ascii_uppercase())
                })
                && parts
                    .next()
                    .is_some_and(|num| num.len() == 3 && num.chars().all(|c| c.is_ascii_digit()))
                && parts.next().is_none();
            if is_clause_id {
                assert!(
                    known.contains(token),
                    "{token} appears in the normative prose (before §10) but is \
                     not in CLAUSES — either add the clause data or move the \
                     mention into §10 Future work"
                );
            }
        }
    }

    #[test]
    fn gate_from_id_roundtrips_and_is_case_insensitive() {
        for g in [Gate::DocsConfirm, Gate::PreviewConfirm] {
            assert_eq!(Gate::from_id(g.id()), Some(g));
        }
        // Case-insensitive + whitespace tolerant.
        assert_eq!(Gate::from_id("Docs_Confirm"), Some(Gate::DocsConfirm));
        assert_eq!(
            Gate::from_id("  preview_confirm  "),
            Some(Gate::PreviewConfirm)
        );
        // Unknown → None (fail-open).
        assert_eq!(Gate::from_id("nope"), None);
        assert_eq!(Gate::from_id(""), None);
    }
}
