//! Redaction for records the interactive surfaces write to disk.
//!
//! Base adapters pass model output and tool traffic through unredacted, so
//! the transcript, governance and trust decisions see what actually ran (see
//! [`umadev_governance::redaction`]). A surface that persists that material,
//! such as a saved chat session or a bug report, redacts it here, at the
//! point it is written.

use serde::Serialize;

pub use umadev_governance::redaction::{redact_json, redact_text};

/// Serialize `value` as pretty JSON with its secret values redacted and its
/// shape unchanged.
///
/// # Errors
///
/// Returns the serializer's error when `value` cannot be represented as JSON.
pub fn to_redacted_json_pretty<T: Serialize>(value: &T) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&redact_json(serde_json::to_value(value)?))
}
