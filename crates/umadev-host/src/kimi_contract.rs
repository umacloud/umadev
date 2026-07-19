//! Pinned, source-audited compatibility classification for Kimi Code CLI.
//!
//! Kimi Code exposes a standard ACP server, but several authority-bearing
//! methods (notably `session/set_config_option`) are source-documented rather
//! than independently capability-advertised. UmaDev enables those methods only
//! when the peer identifies as the exact upstream version audited here.

use semver::{Version, VersionReq};
use serde_json::Value;

/// Official Kimi Code source repository.
pub const KIMI_CODE_SOURCE_REPOSITORY: &str = "https://github.com/MoonshotAI/kimi-code";

/// Exact upstream commit behind the audited release tag.
pub const KIMI_CODE_SOURCE_COMMIT: &str = "36b05820cba24e09fdff19a059afc08ccea2c35e";

/// Audited Kimi Code CLI release.
pub const KIMI_CODE_SOURCE_VERSION: &str = "0.26.0";

/// Audited annotated release tag.
pub const KIMI_CODE_SOURCE_TAG: &str = "@moonshot-ai/kimi-code@0.26.0";

/// ACP SDK requirement declared by the audited adapter.
pub const KIMI_CODE_SOURCE_ACP_VERSION: &str = "0.23.0";

/// Audited Kimi ACP adapter package version.
pub const KIMI_CODE_SOURCE_ADAPTER_VERSION: &str = "0.3.4";

/// Source-shaped behavior is exact-version gated until drift tests are rerun.
pub const KIMI_CODE_AUDITED_VERSION_REQUIREMENT: &str = "=0.26.0";

const MAX_AGENT_VERSION_BYTES: usize = 128;

/// Whether `kimi --version` reports the exact source-audited release.
///
/// Commander prints the bare semantic version, so accepting labels or build
/// metadata here would weaken the same exact-source gate enforced after ACP
/// initialize.
#[must_use]
pub fn is_audited_cli_version(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_AGENT_VERSION_BYTES {
        return false;
    }
    Version::parse(raw).is_ok_and(|version| {
        version.build.is_empty()
            && VersionReq::parse(KIMI_CODE_AUDITED_VERSION_REQUIREMENT)
                .is_ok_and(|requirement| requirement.matches(&version))
    })
}

/// Why an ACP peer did or did not enter the Kimi source profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiSourceMatch {
    /// `agentInfo.name` was not the official Kimi Code identity.
    NotKimiCode,
    /// The official identity was present but no version was reported.
    MissingAgentVersion,
    /// The version was not a bounded semantic version.
    MalformedAgentVersion,
    /// The version is valid but has not been source-audited by this build.
    OutsideAuditedRange,
    /// The running peer reports the exact source-audited release.
    AuditedVersion,
}

/// Static source profile derived solely from `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiSourceProfile {
    source_match: KimiSourceMatch,
    reported_version: Option<Version>,
}

impl KimiSourceProfile {
    fn disabled(source_match: KimiSourceMatch, reported_version: Option<Version>) -> Self {
        Self {
            source_match,
            reported_version,
        }
    }

    fn audited(version: Version) -> Self {
        Self {
            source_match: KimiSourceMatch::AuditedVersion,
            reported_version: Some(version),
        }
    }

    /// Classification result.
    #[must_use]
    pub const fn source_match(&self) -> KimiSourceMatch {
        self.source_match
    }

    /// Parsed reported version, when valid.
    #[must_use]
    pub fn reported_version(&self) -> Option<&Version> {
        self.reported_version.as_ref()
    }

    /// Whether source-inferred Kimi methods may be used.
    #[must_use]
    pub const fn is_audited_version(&self) -> bool {
        matches!(self.source_match, KimiSourceMatch::AuditedVersion)
    }

    /// Whether the peer is recognizably Kimi Code at all — i.e. it reported the
    /// official `agentInfo.name` (`"Kimi Code CLI"`), regardless of whether the
    /// reported version is the audited one, a newer release, or even missing /
    /// unparseable (a git-describe string, a date, or an empty `agentVersion`).
    ///
    /// This is what a session gate should require: a genuinely DIFFERENT program
    /// (a name collision, e.g. the retired Python `kimi`) is [`KimiSourceMatch::
    /// NotKimiCode`] and stays rejected, but a real Kimi Code whose version is
    /// merely un-SemVer must NOT be hard-refused — it degrades to the baseline ACP
    /// contract ("any installed version, degrade don't block"). Only the audited
    /// enhancements stay gated, behind [`Self::is_audited_version`].
    #[must_use]
    pub const fn is_kimi_code_identity(&self) -> bool {
        // Name matched ⇒ it is Kimi Code. A missing/malformed VERSION only means we
        // cannot enable the audited enhancements, not that the peer is impostor.
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
        return KimiSourceProfile::disabled(KimiSourceMatch::NotKimiCode, None);
    }

    let Some(raw) = initialize
        .pointer("/agentInfo/version")
        .or_else(|| initialize.pointer("/agent_info/version"))
        .and_then(Value::as_str)
    else {
        return KimiSourceProfile::disabled(KimiSourceMatch::MissingAgentVersion, None);
    };
    if raw.is_empty() {
        return KimiSourceProfile::disabled(KimiSourceMatch::MissingAgentVersion, None);
    }
    if raw.len() > MAX_AGENT_VERSION_BYTES {
        return KimiSourceProfile::disabled(KimiSourceMatch::MalformedAgentVersion, None);
    }
    let Ok(version) = Version::parse(raw) else {
        return KimiSourceProfile::disabled(KimiSourceMatch::MalformedAgentVersion, None);
    };
    let audited = VersionReq::parse(KIMI_CODE_AUDITED_VERSION_REQUIREMENT)
        .is_ok_and(|requirement| requirement.matches(&version));
    if audited && version.build.is_empty() {
        KimiSourceProfile::audited(version)
    } else {
        KimiSourceProfile::disabled(KimiSourceMatch::OutsideAuditedRange, Some(version))
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
    fn only_the_exact_official_release_enables_the_source_profile() {
        let audited = profile("Kimi Code CLI", Some(KIMI_CODE_SOURCE_VERSION));
        assert_eq!(audited.source_match(), KimiSourceMatch::AuditedVersion);
        assert!(audited.is_audited_version());

        for version in ["0.25.9", "0.26.1", "0.27.0", "0.26.0+local"] {
            let outside = profile("Kimi Code CLI", Some(version));
            assert_eq!(outside.source_match(), KimiSourceMatch::OutsideAuditedRange);
            assert!(!outside.is_audited_version());
        }
    }

    #[test]
    fn cli_version_gate_matches_the_initialize_source_gate() {
        assert!(is_audited_cli_version("0.26.0\n"));
        for version in ["v0.26.0", "0.26.0+local", "0.26.1", "latest", ""] {
            assert!(!is_audited_cli_version(version), "{version}");
        }
    }

    #[test]
    fn lookalikes_missing_and_malformed_versions_fail_closed() {
        assert_eq!(
            profile("Kimi CLI", Some(KIMI_CODE_SOURCE_VERSION)).source_match(),
            KimiSourceMatch::NotKimiCode
        );
        assert_eq!(
            profile("Kimi Code CLI", None).source_match(),
            KimiSourceMatch::MissingAgentVersion
        );
        assert_eq!(
            profile("Kimi Code CLI", Some("latest")).source_match(),
            KimiSourceMatch::MalformedAgentVersion
        );
    }

    #[test]
    fn a_real_kimi_with_an_unparseable_version_degrades_instead_of_being_refused() {
        // Fix #4: "any installed version, degrade don't block". A genuine Kimi Code
        // whose reported version is not strict SemVer — a git-describe string, a
        // date, a label, an empty string, or a missing `agentVersion` — must still
        // be RECOGNIZED as Kimi Code (so the session gate lets it run on the
        // baseline contract) rather than hard-refused. Only the audited enhancements
        // stay gated behind `is_audited_version`.
        for version in [
            Some("2024-05-01"),        // a date
            Some("v1.2.3-9-gabc1234"), // git-describe
            Some("nightly"),           // a label
            Some(""),                  // empty agentVersion
            None,                      // missing agentVersion
        ] {
            let p = profile("Kimi Code CLI", version);
            assert!(
                p.is_kimi_code_identity(),
                "a real Kimi Code with version {version:?} must be recognized, not refused"
            );
            assert!(
                !p.is_audited_version(),
                "a non-SemVer/missing version must NOT unlock the audited enhancements"
            );
        }
        // A genuinely DIFFERENT program on PATH (a name collision) is still refused,
        // even with the exact audited version string.
        let impostor = profile("kimi", Some(KIMI_CODE_SOURCE_VERSION));
        assert!(
            !impostor.is_kimi_code_identity(),
            "a different `kimi` (name mismatch) must still be rejected"
        );
        assert_eq!(impostor.source_match(), KimiSourceMatch::NotKimiCode);
    }

    #[test]
    fn snake_case_initialize_identity_is_accepted() {
        let profile = source_profile_from_initialize(&json!({
            "agent_info":{"name":"Kimi Code CLI","version":KIMI_CODE_SOURCE_VERSION}
        }));
        assert!(profile.is_audited_version());
    }
}
