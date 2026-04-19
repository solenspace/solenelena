//! Per-tenant credential storage with at-rest encryption.
//!
//! Each row in `tenant_credentials` holds the credentials a single tenant
//! has provisioned for a single plugin (Slack bot token, Notion integration
//! token, Shopify admin token, etc.). The credentials are stored
//! encrypted with `Aes256Gcm` using the operator-supplied master key
//! (`ELENA_CREDENTIAL_MASTER_KEY` env var, base64-encoded 32-byte key).
//!
//! At dispatch time the worker calls [`TenantCredentialsStore::get_decrypted`]
//! to obtain the plaintext map and attaches each pair to the outgoing
//! gRPC request as `x-elena-cred-<key>: <value>` metadata. The connector
//! prefers metadata-supplied credentials over its startup env defaults,
//! so single-tenant deployments stay simple while multi-tenant ones get
//! true isolation.
//!
//! Cross-tenant reads are impossible: the primary-key composite
//! `(tenant_id, plugin_id)` is the only access path and every public
//! method takes a `TenantId` parameter.

use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use elena_types::{StoreError, TenantId};
use sqlx::{PgPool, Row};

use crate::sql_error::classify_sqlx;

/// Length of the AES-GCM nonce in bytes. 96 bits is the AES-GCM standard.
const NONCE_LEN: usize = 12;

/// CRUD over `tenant_credentials` with envelope encryption.
///
/// Cloning is cheap — the master key is wrapped in `Arc`-equivalent
/// (`Aes256Gcm` is `Clone`). The pool is also `Clone`able.
#[derive(Clone)]
pub struct TenantCredentialsStore {
    pool: PgPool,
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for TenantCredentialsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantCredentialsStore")
            .field("cipher", &"<Aes256Gcm>")
            .finish_non_exhaustive()
    }
}

impl TenantCredentialsStore {
    /// Build a store using the supplied 32-byte master key.
    ///
    /// In production the key comes from `ELENA_CREDENTIAL_MASTER_KEY`
    /// (see [`Self::from_env_key`]); tests can pass any 32-byte key.
    #[must_use]
    pub fn new(pool: PgPool, master_key: [u8; 32]) -> Self {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&master_key));
        Self { pool, cipher }
    }

    /// Build a store from `ELENA_CREDENTIAL_MASTER_KEY`.
    ///
    /// Errors if the env var is missing, not valid base64, or not exactly
    /// 32 bytes after decoding.
    pub fn from_env_key(pool: PgPool, env_var: &str) -> Result<Self, StoreError> {
        let raw = std::env::var(env_var).map_err(|_| {
            StoreError::Configuration(format!("missing env var {env_var} (base64 32-byte key)"))
        })?;
        let bytes = BASE64.decode(raw.trim()).map_err(|e| {
            StoreError::Configuration(format!("{env_var} is not valid base64: {e}"))
        })?;
        let key: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            StoreError::Configuration(format!("{env_var} must decode to 32 bytes, got {}", v.len()))
        })?;
        Ok(Self::new(pool, key))
    }

    /// Insert or replace the credentials a tenant has provisioned for a
    /// plugin. The map is serialized to JSON, encrypted with a freshly
    /// generated nonce, and persisted.
    pub async fn upsert(
        &self,
        tenant_id: TenantId,
        plugin_id: &str,
        creds: &BTreeMap<String, String>,
    ) -> Result<(), StoreError> {
        let plaintext = serde_json::to_vec(creds)
            .map_err(|e| StoreError::Database(format!("serialize creds: {e}")))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext.as_ref())
            .map_err(|e| StoreError::Database(format!("encrypt: {e}")))?;

        sqlx::query(
            "INSERT INTO tenant_credentials (tenant_id, plugin_id, kv_ciphertext, kv_nonce)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant_id, plugin_id) DO UPDATE
                SET kv_ciphertext = EXCLUDED.kv_ciphertext,
                    kv_nonce      = EXCLUDED.kv_nonce,
                    updated_at    = now()",
        )
        .bind(tenant_id.as_uuid())
        .bind(plugin_id)
        .bind(&ciphertext)
        .bind(&nonce_bytes[..])
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        Ok(())
    }

    /// Decrypt and return the credentials map for `(tenant_id, plugin_id)`.
    ///
    /// Returns an empty map when the row is missing — callers fall back
    /// to env defaults in that case. Returns a hard error only on decrypt
    /// failure (e.g. the master key was rotated and old rows are now
    /// unreadable).
    pub async fn get_decrypted(
        &self,
        tenant_id: TenantId,
        plugin_id: &str,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let row = sqlx::query(
            "SELECT kv_ciphertext, kv_nonce FROM tenant_credentials
             WHERE tenant_id = $1 AND plugin_id = $2",
        )
        .bind(tenant_id.as_uuid())
        .bind(plugin_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let Some(row) = row else {
            return Ok(BTreeMap::new());
        };
        let ciphertext: Vec<u8> = row.try_get("kv_ciphertext").map_err(classify_sqlx)?;
        let nonce_bytes: Vec<u8> = row.try_get("kv_nonce").map_err(classify_sqlx)?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(StoreError::Database(format!(
                "tenant_credentials.kv_nonce wrong length: {} (expected {NONCE_LEN})",
                nonce_bytes.len()
            )));
        }
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = self
            .cipher
            .decrypt(nonce, ciphertext.as_ref())
            .map_err(|e| StoreError::Database(format!("decrypt: {e}")))?;
        let creds: BTreeMap<String, String> = serde_json::from_slice(&plaintext)
            .map_err(|e| StoreError::Database(format!("deserialize creds: {e}")))?;
        Ok(creds)
    }

    /// Drop all credentials for a tenant's view of a plugin. Returns the
    /// number of rows deleted (0 or 1).
    pub async fn clear(&self, tenant_id: TenantId, plugin_id: &str) -> Result<u64, StoreError> {
        let result =
            sqlx::query("DELETE FROM tenant_credentials WHERE tenant_id = $1 AND plugin_id = $2")
                .bind(tenant_id.as_uuid())
                .bind(plugin_id)
                .execute(&self.pool)
                .await
                .map_err(classify_sqlx)?;
        Ok(result.rows_affected())
    }

    /// List the plugin IDs a tenant has credentials for. Used by the
    /// admin UI to show which integrations are wired up.
    pub async fn list_for_tenant(&self, tenant_id: TenantId) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT plugin_id FROM tenant_credentials WHERE tenant_id = $1
             ORDER BY plugin_id",
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(row.try_get::<String, _>("plugin_id").map_err(classify_sqlx)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> [u8; 32] {
        // Deterministic test key. Real deployments use 32 random bytes.
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap_or(0);
        }
        k
    }

    #[test]
    fn base64_round_trip_decodes_to_original_key() {
        // Equivalent invariant to from_env_key (which mutates env and so
        // can't be tested here under the workspace's `forbid(unsafe)`):
        // the base64 in the env var must decode losslessly to a 32-byte
        // key.
        let key = fixed_key();
        let b64 = BASE64.encode(key);
        let decoded = BASE64.decode(b64.trim()).unwrap();
        let arr: [u8; 32] = decoded.try_into().unwrap();
        assert_eq!(arr, key);
    }

    #[test]
    fn nonce_length_constant_matches_aes_gcm_default() {
        // AES-GCM standard mandates 96-bit nonce.
        assert_eq!(NONCE_LEN, 12);
    }

    #[test]
    fn encrypt_then_decrypt_round_trips_a_credentials_map() {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&fixed_key()));
        let creds: BTreeMap<String, String> = [
            ("token".to_owned(), "xoxb-secret".to_owned()),
            ("workspace".to_owned(), "T0123".to_owned()),
        ]
        .into_iter()
        .collect();
        let plaintext = serde_json::to_vec(&creds).unwrap();
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, plaintext.as_ref()).unwrap();
        let decrypted = cipher.decrypt(nonce, ciphertext.as_ref()).unwrap();
        let restored: BTreeMap<String, String> = serde_json::from_slice(&decrypted).unwrap();
        assert_eq!(restored, creds);
    }

    #[test]
    fn wrong_key_fails_decrypt() {
        let cipher_a = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&fixed_key()));
        let mut other = fixed_key();
        other[0] ^= 0xff;
        let cipher_b = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&other));

        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher_a.encrypt(nonce, b"hello".as_ref()).unwrap();
        assert!(cipher_b.decrypt(nonce, ciphertext.as_ref()).is_err());
    }
}
