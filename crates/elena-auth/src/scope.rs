//! Per-tenant admin scope hashing.
//!
//! S2 introduced a per-tenant `X-Elena-Tenant-Scope` token whose
//! SHA-256 is stored in `tenants.admin_scope_hash`. The hash function
//! lives here so any future caller (admin route layer, future BFF
//! tooling) gets the same digest from one canonical implementation.

use sha2::{Digest, Sha256};

/// SHA-256 a raw scope token down to the 32-byte digest stored in
/// `tenants.admin_scope_hash`. Stable across releases — rotating the
/// algorithm requires a migration.
#[must_use]
pub fn hash_scope_token(token: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

/// Constant-time comparison wrapper. Re-exported for callers that
/// already hashed both sides and just want a side-channel-safe
/// equality check.
#[must_use]
pub fn ct_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_32_bytes() {
        let a = hash_scope_token("op-token-v1");
        let b = hash_scope_token("op-token-v1");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn differs_for_different_inputs() {
        assert_ne!(hash_scope_token("op-token-v1"), hash_scope_token("op-token-v2"));
    }

    #[test]
    fn ct_eq_handles_match_and_mismatch() {
        let a = hash_scope_token("foo");
        let same = hash_scope_token("foo");
        let other = hash_scope_token("bar");
        assert!(ct_eq_32(&a, &same));
        assert!(!ct_eq_32(&a, &other));
    }
}
