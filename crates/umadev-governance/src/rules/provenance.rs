//! Keyed provenance for permission-bearing project context.

use super::ProjectContext;
use sha2::{Digest as _, Sha256};

/// A stable HMAC-SHA-256 fingerprint of a normalized requirement.
///
/// The installation key lives outside the workspace, so copying cached context does not make
/// short requirements dictionary-testable.
#[must_use]
pub fn requirement_fingerprint(key: &[u8], requirement: &str) -> [u8; 32] {
    privacy_fingerprint(
        key,
        b"umadev.requirement-provenance.v1",
        requirement.trim().as_bytes(),
    )
}

/// HMAC-SHA-256 for a domain-separated persisted identity.
#[must_use]
pub fn privacy_fingerprint(key: &[u8], domain: &[u8], value: &[u8]) -> [u8; 32] {
    hmac_sha256(key, &[domain, b"\0", value])
}

impl ProjectContext {
    /// Stamp a permission-bearing context with its requirement and creation time.
    #[must_use]
    pub fn derived_from(mut self, requirement: &str, key: &[u8], now: u64) -> Self {
        self.requirement_hash = 0;
        self.requirement_fingerprint = requirement_fingerprint(key, requirement);
        self.derived_at = now;
        self.provenance_auth = self.authentication_tag(key);
        self
    }

    /// Downgrade cached context to strict defaults unless its provenance is current and valid.
    #[must_use]
    pub fn if_current(self, now: u64, requirement: Option<&str>, key: Option<&[u8]>) -> Self {
        let Some(key) = key else {
            return Self::unknown();
        };
        if self.requirement_fingerprint == [0; 32]
            || self.provenance_auth == [0; 32]
            || self.derived_at == 0
            || !fingerprints_equal(&self.authentication_tag(key), &self.provenance_auth)
        {
            return Self::unknown();
        }
        match requirement.map(str::trim).filter(|r| !r.is_empty()) {
            Some(req)
                if fingerprints_equal(
                    &requirement_fingerprint(key, req),
                    &self.requirement_fingerprint,
                ) =>
            {
                self
            }
            Some(_) => Self::unknown(),
            // A future timestamp from clock skew reads as fresh, never as expired.
            None if now.saturating_sub(self.derived_at) <= Self::MAX_UNMATCHED_AGE_SECS => self,
            None => Self::unknown(),
        }
    }

    fn authentication_tag(self, key: &[u8]) -> [u8; 32] {
        const DOMAIN: &[u8] = b"umadev.governance-context.v1\0";
        let flags = [
            u8::from(self.static_frontend_only),
            u8::from(self.purple_allowed),
        ];
        hmac_sha256(
            key,
            &[
                DOMAIN,
                &flags,
                &self.requirement_fingerprint,
                &self.derived_at.to_be_bytes(),
            ],
        )
    }
}

pub(super) fn hmac_sha256(key: &[u8], segments: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0_u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for ((inner, outer), key_byte) in inner_pad
        .iter_mut()
        .zip(outer_pad.iter_mut())
        .zip(key_block)
    {
        *inner ^= key_byte;
        *outer ^= key_byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for segment in segments {
        inner.update(segment);
    }
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn fingerprints_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}
