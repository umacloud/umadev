//! Runtime identity classification for Grok Build.
//!
//! Grok Build's version is diagnostic evidence, not a startup compatibility gate.
//! UmaDev accepts every official Grok ACP peer and negotiates standard features
//! from live ACP fields. A private wire contract needs stronger evidence: each
//! capability is enabled only on the source-audited stable compatibility line
//! (or from an explicit live marker/probe). A newer patch on that same line is
//! not disabled merely because its patch number is newer than UmaDev's latest
//! source snapshot: every private parser remains typed and fail-soft. Unknown
//! release lines keep working through standard ACP without inheriting private
//! methods whose wire compatibility has not been established.

use semver::Version;
use serde_json::Value;

/// Official Grok Build source repository.
pub const GROK_BUILD_SOURCE_REPOSITORY: &str = "https://github.com/xai-org/grok-build";

/// Exact upstream commit used by source-contract drift CI.
pub const GROK_BUILD_SOURCE_COMMIT: &str = "500129c714ad1b10e6095481f4a8387a2ec52649";

/// Release used as the current source-audited baseline, never as a runtime pin.
pub const GROK_BUILD_SOURCE_VERSION: &str = "0.2.114";

/// `agent-client-protocol` version used by the audited baseline.
pub const GROK_BUILD_SOURCE_ACP_VERSION: &str = "0.10.4";

/// `agent-client-protocol-schema` version resolved by the baseline lockfile.
pub const GROK_BUILD_SOURCE_ACP_SCHEMA_VERSION: &str = "0.11.4";

const MAX_AGENT_VERSION_BYTES: usize = 128;

/// Oldest release included in the current private-wire compatibility audit.
///
/// This is deliberately not a minimum supported CLI version. Older Grok builds
/// still start and use their live ACP advertisement; they simply do not receive
/// private xAI capabilities that UmaDev has not proved for that release.
const GROK_PRIVATE_CAPABILITY_AUDIT_FLOOR: (u64, u64, u64) = (0, 2, 101);

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
    /// Source-shaped tool permission reverse requests.
    PermissionRequests,
    /// Private `_x.ai/session/close` graceful shutdown.
    PrivateSessionClose,
    /// Standard `session/set_mode` support when the live session omits modes.
    SetModeFallback,
    /// Native subagent lifecycle carried by rich updates.
    SubagentLifecycle,
    /// Source-specific incremental terminal output semantics.
    IncrementalTerminalOutput,
    /// Model state, command catalog, and related updates.
    ModelAndCommandCatalog,
}

impl GrokSourceCapability {
    /// Every source-specific capability represented by this profile.
    pub const ALL: [Self; 18] = [
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
        Self::PermissionRequests,
        Self::PrivateSessionClose,
        Self::SetModeFallback,
        Self::SubagentLifecycle,
        Self::IncrementalTerminalOutput,
        Self::ModelAndCommandCatalog,
    ];

    const fn bit(self) -> u32 {
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
            Self::PermissionRequests => 1 << 15,
            Self::PrivateSessionClose => 1 << 16,
            Self::SetModeFallback => 1 << 17,
        }
    }

    /// Oldest release for which the capability's present wire shape was
    /// source-audited and exercised. The match stays per-capability even while
    /// the current audit floors coincide, so a later upstream change can
    /// downgrade one private method without disabling unrelated ones.
    const fn audited_floor(self) -> (u64, u64, u64) {
        match self {
            Self::ImagePromptFallback
            | Self::DefaultAuthMethod
            | Self::AskUserQuestion
            | Self::ExitPlanMode
            | Self::Interject
            | Self::PromptQueue
            | Self::FolderTrust
            | Self::RichSessionUpdates
            | Self::SessionLoadReplay
            | Self::PromptUsage
            | Self::BackgroundTasks
            | Self::BackgroundProcessControl
            | Self::PermissionRequests
            | Self::PrivateSessionClose
            | Self::SetModeFallback
            | Self::SubagentLifecycle
            | Self::IncrementalTerminalOutput
            | Self::ModelAndCommandCatalog => GROK_PRIVATE_CAPABILITY_AUDIT_FLOOR,
        }
    }
}

/// Source-shaped parsers available for an official Grok peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokSourceCapabilities {
    bits: u32,
}

impl GrokSourceCapabilities {
    /// No Grok-specific source parser is enabled.
    pub const NONE: Self = Self { bits: 0 };
    fn for_compatible_release_lineage(version: &Version) -> Self {
        // Prereleases can carry an already-audited numeric version while still
        // containing a different private wire. Keep them on standard ACP until
        // their exact source is audited or a live capability marker exists.
        if !version.pre.is_empty() {
            return Self::NONE;
        }
        let Ok(baseline) = Version::parse(GROK_BUILD_SOURCE_VERSION) else {
            // A malformed compiled-in baseline is a build defect; fail closed
            // rather than granting every private method.
            return Self::NONE;
        };
        let mut bits = 0;
        for capability in GrokSourceCapability::ALL {
            let (major, minor, patch) = capability.audited_floor();
            let floor = Version::new(major, minor, patch);
            // Grok is still in the 0.x SemVer era, where a minor-number change
            // may intentionally break compatibility. Preserve every stable
            // future PATCH on the source-audited major/minor line, but require
            // live evidence (or a new source audit) before crossing a minor or
            // major boundary. This avoids a patch-version lock without blindly
            // opting an unknown protocol generation into private mutations.
            if version.major == baseline.major
                && version.minor == baseline.minor
                && version >= &floor
            {
                bits |= capability.bit();
            }
        }
        Self { bits }
    }

    /// Whether a typed parser exists for one Grok-specific behavior.
    #[must_use]
    pub const fn contains(self, capability: GrokSourceCapability) -> bool {
        self.bits & capability.bit() != 0
    }

    /// Compact representation used by the ACP reader's atomic capability
    /// snapshot. Only [`Self::encoded_contains`] should interpret it.
    #[must_use]
    pub const fn encoded(self) -> u32 {
        self.bits
    }

    /// Test one capability in an atomically transported snapshot.
    #[must_use]
    pub const fn encoded_contains(bits: u32, capability: GrokSourceCapability) -> bool {
        bits & capability.bit() != 0
    }

    #[cfg(test)]
    pub(crate) const fn only(capability: GrokSourceCapability) -> Self {
        Self {
            bits: capability.bit(),
        }
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
        let capabilities = reported_version.as_ref().map_or(
            GrokSourceCapabilities::NONE,
            GrokSourceCapabilities::for_compatible_release_lineage,
        );
        Self::new(source_match, reported_version, capabilities)
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

    /// Whether this exact private behavior has source evidence for the reported
    /// release. This never controls standard ACP startup or advertised methods.
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
    fn compatible_stable_patch_line_enables_private_contracts_without_a_ceiling() {
        for version in [
            "0.2.101",
            "0.2.109+local.7",
            GROK_BUILD_SOURCE_VERSION,
            "0.2.115",
            "0.2.999+future.patch",
        ] {
            let profile = profile(Some(version));
            assert!(profile.is_grok_shell_identity(), "{version}");
            for capability in GrokSourceCapability::ALL {
                assert!(profile.supports(capability), "{version}: {capability:?}");
            }
        }
    }

    #[test]
    fn unverified_versions_keep_identity_but_not_private_capabilities() {
        for version in [
            Some("0.2.100"),
            Some("0.2.109-alpha.1"),
            Some("0.3.0"),
            Some("99.7.3+future.adapter"),
            Some("2026-07-22-nightly"),
            Some(""),
            None,
        ] {
            let profile = profile(version);
            assert!(profile.is_grok_shell_identity(), "{version:?}");
            assert!(profile.capabilities().is_empty(), "{version:?}");
            for capability in GrokSourceCapability::ALL {
                assert!(!profile.supports(capability), "{version:?}: {capability:?}");
            }
        }
    }

    #[test]
    fn one_atomic_capability_bit_never_unlocks_a_sibling() {
        for enabled in GrokSourceCapability::ALL {
            let snapshot = GrokSourceCapabilities::only(enabled).encoded();
            for checked in GrokSourceCapability::ALL {
                assert_eq!(
                    GrokSourceCapabilities::encoded_contains(snapshot, checked),
                    checked == enabled,
                    "{enabled:?} unexpectedly changed {checked:?}"
                );
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
