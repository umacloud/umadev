//! Runtime identity classification for Kimi Code CLI.
//!
//! Kimi Code's version is diagnostic evidence, not a compatibility gate. UmaDev
//! accepts every official Kimi Code ACP peer and discovers optional controls from
//! each live session's advertised `configOptions`. The audited source metadata below
//! records the baseline used by drift CI; it never decides whether an installed
//! version may run.

use semver::Version;
use serde_json::Value;

/// Official Kimi Code source repository.
pub const KIMI_CODE_SOURCE_REPOSITORY: &str = "https://github.com/MoonshotAI/kimi-code";

/// Exact upstream commit used by source-contract drift CI.
pub const KIMI_CODE_AUDITED_BASELINE_COMMIT: &str = "17dfd49768f753a4f0fe97d8e7d3317dab560575";

/// Release used as the current source-audited baseline, never as a runtime pin.
pub const KIMI_CODE_AUDITED_BASELINE_VERSION: &str = "0.31.0";

/// ACP SDK requirement declared by the audited baseline adapter.
pub const KIMI_CODE_AUDITED_BASELINE_ACP_VERSION: &str = "0.23.0";

/// ACP adapter package version in the audited baseline.
pub const KIMI_CODE_AUDITED_BASELINE_ADAPTER_VERSION: &str = "0.3.6";

const MAX_AGENT_VERSION_BYTES: usize = 128;

/// What could be learned about an ACP peer claiming to be Kimi Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiSourceMatch {
    /// `agentInfo.name` was not the official Kimi Code identity.
    NotKimiCode,
    /// The official identity was present but no version was reported.
    MissingAgentVersion,
    /// The official identity reported a non-SemVer version label.
    UnparsedAgentVersion,
    /// The official identity reported a bounded semantic version.
    VersionReported,
}

/// Runtime identity profile derived solely from ACP `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiSourceProfile {
    source_match: KimiSourceMatch,
    reported_version: Option<Version>,
}

impl KimiSourceProfile {
    fn new(source_match: KimiSourceMatch, reported_version: Option<Version>) -> Self {
        Self {
            source_match,
            reported_version,
        }
    }

    /// Classification result, retained for diagnostics and tests.
    #[must_use]
    pub const fn source_match(&self) -> KimiSourceMatch {
        self.source_match
    }

    /// Parsed reported version, when the peer used SemVer.
    #[must_use]
    pub fn reported_version(&self) -> Option<&Version> {
        self.reported_version.as_ref()
    }

    /// Whether the peer is the official Kimi Code ACP program.
    ///
    /// Version presence, format, age, prerelease labels, and build metadata do
    /// not affect this decision. Optional session controls are negotiated later
    /// from the live `session/new`/`session/resume` response.
    #[must_use]
    pub const fn is_kimi_code_identity(&self) -> bool {
        !matches!(self.source_match, KimiSourceMatch::NotKimiCode)
    }
}

/// Classify the official `agentInfo` identity in an ACP initialize response.
#[must_use]
pub fn source_profile_from_initialize(initialize: &Value) -> KimiSourceProfile {
    if initialize
        .pointer("/agentInfo/name")
        .or_else(|| initialize.pointer("/agent_info/name"))
        .and_then(Value::as_str)
        != Some("Kimi Code CLI")
    {
        return KimiSourceProfile::new(KimiSourceMatch::NotKimiCode, None);
    }

    let Some(raw) = initialize
        .pointer("/agentInfo/version")
        .or_else(|| initialize.pointer("/agent_info/version"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    else {
        return KimiSourceProfile::new(KimiSourceMatch::MissingAgentVersion, None);
    };
    if raw.len() > MAX_AGENT_VERSION_BYTES {
        return KimiSourceProfile::new(KimiSourceMatch::UnparsedAgentVersion, None);
    }
    match Version::parse(raw) {
        Ok(version) => KimiSourceProfile::new(KimiSourceMatch::VersionReported, Some(version)),
        Err(_) => KimiSourceProfile::new(KimiSourceMatch::UnparsedAgentVersion, None),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn profile(name: &str, version: Option<&str>) -> KimiSourceProfile {
        let mut initialize = json!({"agentInfo":{"name":name}});
        if let Some(version) = version {
            initialize["agentInfo"]["version"] = Value::String(version.to_string());
        }
        source_profile_from_initialize(&initialize)
    }

    #[test]
    fn every_official_version_is_accepted_as_the_same_identity() {
        for version in [
            Some("0.1.0"),
            Some(KIMI_CODE_AUDITED_BASELINE_VERSION),
            Some("0.28.1+local"),
            Some("0.99.0-beta.1"),
            Some("1.0.0"),
            Some("2026-07-22-nightly"),
            Some(""),
            None,
        ] {
            assert!(
                profile("Kimi Code CLI", version).is_kimi_code_identity(),
                "official Kimi version {version:?} must not be rejected"
            );
        }
    }

    #[test]
    fn semantic_versions_are_diagnostic_only() {
        let baseline = profile("Kimi Code CLI", Some(KIMI_CODE_AUDITED_BASELINE_VERSION));
        assert_eq!(baseline.source_match(), KimiSourceMatch::VersionReported);
        assert_eq!(
            baseline.reported_version().map(ToString::to_string),
            Some(KIMI_CODE_AUDITED_BASELINE_VERSION.to_string())
        );

        let build = profile("Kimi Code CLI", Some("0.28.1+local.7"));
        assert_eq!(build.source_match(), KimiSourceMatch::VersionReported);
        assert!(build.is_kimi_code_identity());
    }

    #[test]
    fn only_a_different_program_is_rejected() {
        let impostor = profile("kimi", Some(KIMI_CODE_AUDITED_BASELINE_VERSION));
        assert!(!impostor.is_kimi_code_identity());
        assert_eq!(impostor.source_match(), KimiSourceMatch::NotKimiCode);
    }

    #[test]
    fn snake_case_initialize_identity_is_accepted() {
        let profile = source_profile_from_initialize(&json!({
            "agent_info":{"name":"Kimi Code CLI","version":"999.0.0+future"}
        }));
        assert!(profile.is_kimi_code_identity());
        assert_eq!(profile.source_match(), KimiSourceMatch::VersionReported);
    }
}
