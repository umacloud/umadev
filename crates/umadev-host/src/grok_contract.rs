//! Runtime identity classification for Grok Build.
//!
//! Grok Build's version is diagnostic evidence, not a compatibility gate. UmaDev
//! accepts every official Grok ACP peer. Standard features are negotiated from ACP
//! and Grok-specific messages are accepted only through typed, bounded parsers.
//! The source metadata below records the baseline used by drift CI; it never
//! decides whether an installed version may run.

use semver::Version;
use serde_json::Value;

/// Official Grok Build source repository.
pub const GROK_BUILD_SOURCE_REPOSITORY: &str = "https://github.com/xai-org/grok-build";

/// Exact upstream commit used by source-contract drift CI.
pub const GROK_BUILD_SOURCE_COMMIT: &str = "47348d13ec4508dcfe440e34c6d511bb02998fb2";

/// Release used as the current source-audited baseline, never as a runtime pin.
pub const GROK_BUILD_SOURCE_VERSION: &str = "0.2.112";

/// `agent-client-protocol` version used by the audited baseline.
pub const GROK_BUILD_SOURCE_ACP_VERSION: &str = "0.10.4";

/// `agent-client-protocol-schema` version resolved by the baseline lockfile.
pub const GROK_BUILD_SOURCE_ACP_SCHEMA_VERSION: &str = "0.11.4";

const MAX_AGENT_VERSION_BYTES: usize = 128;

/// What could be learned about an ACP peer claiming to be Grok Build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokSourceMatch {
    /// `_meta.grokShell` was not exactly `true`.
    NotGrokShell,
    /// The official identity was present but no version was reported.
    MissingAgentVersion,
    /// The official identity reported a non-SemVer version label.
    UnparsedAgentVersion,
    /// The official identity reported a bounded semantic version.
    VersionReported,
}

/// One Grok-private behavior with a typed, source-backed UmaDev parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokSourceCapability {
    /// Image prompt blocks supported despite an omitted standard image flag.
    ImagePromptFallback,
    /// Agent-selected authentication through `_meta.defaultAuthMethodId`.
    DefaultAuthMethod,
    /// Private `x.ai/ask_user_question` reverse requests.
    AskUserQuestion,
    /// Private `x.ai/exit_plan_mode` reverse requests.
    ExitPlanMode,
    /// Private `x.ai/interject` requests.
    Interject,
    /// Server-authoritative `x.ai/queue/*` operations.
    PromptQueue,
    /// Reverse `x.ai/folder_trust/request` settlement.
    FolderTrust,
    /// Rich live and persisted `x.ai/session_*` updates.
    RichSessionUpdates,
    /// Source-shaped replay ordering around standard `session/load`.
    SessionLoadReplay,
    /// Whole-prompt `_meta.usage` semantics.
    PromptUsage,
    /// Background task lifecycle carried by rich updates.
    BackgroundTasks,
    /// Native `x.ai/task/list` and `x.ai/task/kill` control.
    BackgroundProcessControl,
    /// Native subagent lifecycle carried by rich updates.
    SubagentLifecycle,
    /// Source-specific incremental terminal output semantics.
    IncrementalTerminalOutput,
    /// Model state, command catalog, and related updates.
    ModelAndCommandCatalog,
}

impl GrokSourceCapability {
    /// Every source-specific capability represented by this profile.
    pub const ALL: [Self; 15] = [
        Self::ImagePromptFallback,
        Self::DefaultAuthMethod,
        Self::AskUserQuestion,
        Self::ExitPlanMode,
        Self::Interject,
        Self::PromptQueue,
        Self::FolderTrust,
        Self::RichSessionUpdates,
        Self::SessionLoadReplay,
        Self::PromptUsage,
        Self::BackgroundTasks,
        Self::BackgroundProcessControl,
        Self::SubagentLifecycle,
        Self::IncrementalTerminalOutput,
        Self::ModelAndCommandCatalog,
    ];

    const fn bit(self) -> u16 {
        match self {
            Self::ImagePromptFallback => 1 << 0,
            Self::DefaultAuthMethod => 1 << 1,
            Self::AskUserQuestion => 1 << 2,
            Self::ExitPlanMode => 1 << 3,
            Self::Interject => 1 << 4,
            Self::RichSessionUpdates => 1 << 5,
            Self::SessionLoadReplay => 1 << 6,
            Self::PromptUsage => 1 << 7,
            Self::BackgroundTasks => 1 << 8,
            Self::SubagentLifecycle => 1 << 9,
            Self::IncrementalTerminalOutput => 1 << 10,
            Self::ModelAndCommandCatalog => 1 << 11,
            Self::PromptQueue => 1 << 12,
            Self::FolderTrust => 1 << 13,
            Self::BackgroundProcessControl => 1 << 14,
        }
    }
}

/// Source-shaped parsers available for an official Grok peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokSourceCapabilities {
    bits: u16,
}

impl GrokSourceCapabilities {
    /// No Grok-specific source parser is enabled.
    pub const NONE: Self = Self { bits: 0 };
    const OFFICIAL_SOURCE_LINEAGE: Self = Self {
        bits: (1 << 15) - 1,
    };

    /// Whether a typed parser exists for one Grok-specific behavior.
    #[must_use]
    pub const fn contains(self, capability: GrokSourceCapability) -> bool {
        self.bits & capability.bit() != 0
    }

    /// Whether no Grok-specific parser is enabled.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }
}

impl Default for GrokSourceCapabilities {
    fn default() -> Self {
        Self::NONE
    }
}

/// Runtime identity profile derived solely from ACP `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokSourceProfile {
    source_match: GrokSourceMatch,
    reported_version: Option<Version>,
    capabilities: GrokSourceCapabilities,
}

impl GrokSourceProfile {
    fn new(
        source_match: GrokSourceMatch,
        reported_version: Option<Version>,
        capabilities: GrokSourceCapabilities,
    ) -> Self {
        Self {
            source_match,
            reported_version,
            capabilities,
        }
    }

    fn official(source_match: GrokSourceMatch, reported_version: Option<Version>) -> Self {
        Self::new(
            source_match,
            reported_version,
            GrokSourceCapabilities::OFFICIAL_SOURCE_LINEAGE,
        )
    }

    /// Identity classification retained for diagnostics and tests.
    #[must_use]
    pub const fn source_match(&self) -> GrokSourceMatch {
        self.source_match
    }

    /// Parsed reported version, when the peer used SemVer.
    #[must_use]
    pub fn reported_version(&self) -> Option<&Version> {
        self.reported_version.as_ref()
    }

    /// Source-shaped parsers enabled for this identity.
    #[must_use]
    pub const fn capabilities(&self) -> GrokSourceCapabilities {
        self.capabilities
    }

    /// A source-shaped parser may run for every official Grok version. Standard
    /// method calls still require ACP advertisement or a successful response.
    #[must_use]
    pub const fn supports(&self, capability: GrokSourceCapability) -> bool {
        self.capabilities.contains(capability)
    }

    /// Whether this is the official Grok source lineage, independent of version.
    #[must_use]
    pub const fn is_grok_shell_identity(&self) -> bool {
        !matches!(self.source_match, GrokSourceMatch::NotGrokShell)
    }
}

/// Classify the official Grok identity in an ACP initialize response.
#[must_use]
pub fn source_profile_from_initialize(initialize: &Value) -> GrokSourceProfile {
    if initialize
        .pointer("/_meta/grokShell")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return GrokSourceProfile::new(
            GrokSourceMatch::NotGrokShell,
            None,
            GrokSourceCapabilities::NONE,
        );
    }

    let Some(raw) = initialize
        .pointer("/_meta/agentVersion")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    else {
        return GrokSourceProfile::official(GrokSourceMatch::MissingAgentVersion, None);
    };
    if raw.len() > MAX_AGENT_VERSION_BYTES {
        return GrokSourceProfile::official(GrokSourceMatch::UnparsedAgentVersion, None);
    }
    match Version::parse(raw) {
        Ok(version) => GrokSourceProfile::official(GrokSourceMatch::VersionReported, Some(version)),
        Err(_) => GrokSourceProfile::official(GrokSourceMatch::UnparsedAgentVersion, None),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn profile(version: Option<&str>) -> GrokSourceProfile {
        let mut initialize = json!({"_meta":{"grokShell":true}});
        if let Some(version) = version {
            initialize["_meta"]["agentVersion"] = Value::String(version.to_string());
        }
        source_profile_from_initialize(&initialize)
    }

    #[test]
    fn every_official_version_enables_typed_source_parsers() {
        for version in [
            Some("0.1.0"),
            Some(GROK_BUILD_SOURCE_VERSION),
            Some("0.2.109-alpha.1"),
            Some("0.2.109+local.7"),
            Some("0.99.0"),
            Some("1.0.0"),
            Some("2026-07-22-nightly"),
            Some(""),
            None,
        ] {
            let profile = profile(version);
            assert!(profile.is_grok_shell_identity(), "{version:?}");
            for capability in GrokSourceCapability::ALL {
                assert!(profile.supports(capability), "{version:?}: {capability:?}");
            }
        }
    }

    #[test]
    fn semantic_versions_are_diagnostic_only() {
        let profile = profile(Some(GROK_BUILD_SOURCE_VERSION));
        assert_eq!(profile.source_match(), GrokSourceMatch::VersionReported);
        assert_eq!(
            profile.reported_version(),
            Some(&Version::parse(GROK_BUILD_SOURCE_VERSION).unwrap())
        );
    }

    #[test]
    fn only_a_non_grok_identity_is_rejected() {
        for initialize in [
            json!({"_meta":{"grokShell":false,"agentVersion":GROK_BUILD_SOURCE_VERSION}}),
            json!({"_meta":{"agentVersion":GROK_BUILD_SOURCE_VERSION}}),
            json!({"grokShell":true,"agentVersion":GROK_BUILD_SOURCE_VERSION}),
            json!({"_meta":{"grokShell":"true","agentVersion":GROK_BUILD_SOURCE_VERSION}}),
        ] {
            let profile = source_profile_from_initialize(&initialize);
            assert_eq!(profile.source_match(), GrokSourceMatch::NotGrokShell);
            assert!(!profile.is_grok_shell_identity());
            assert!(profile.capabilities().is_empty());
        }
    }
}
