//! Redis-backed hot state: thread claims, session data, rate-limit windows.
//!
//! The claim primitives use a Lua CAS script so a stale `release` from a
//! worker that lost connection cannot unclaim a thread that was legitimately
//! reassigned to a new worker.

use elena_types::{StoreError, ThreadId};
use fred::{
    clients::Pool,
    prelude::{KeysInterface, LuaInterface},
    types::Expiration,
};

/// Lua script: claim the key if free, or refresh-in-place if this worker
/// already owns it. Returns 1 on success, 0 if a different worker holds it.
///
/// Idempotent re-claim by the same worker is a deliberate property — the
/// worker layer claims at message-receive time, then `run_loop` claims again
/// inside the loop driver, and both paths must succeed.
const CLAIM_SCRIPT: &str = r"
local cur = redis.call('GET', KEYS[1])
if cur == false then
    redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
    return 1
elseif cur == ARGV[1] then
    redis.call('PEXPIRE', KEYS[1], ARGV[2])
    return 1
else
    return 0
end
";

/// Lua script: `if GET(key) == arg then DEL(key); return 1 else return 0`.
const RELEASE_SCRIPT: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
    return redis.call('DEL', KEYS[1])
else
    return 0
end
";

/// Lua script: `if GET(key) == arg then PEXPIRE(key, ttl); return 1 else return 0`.
const REFRESH_SCRIPT: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
    return redis.call('PEXPIRE', KEYS[1], ARGV[2])
else
    return 0
end
";

/// Handle to Redis session state.
#[derive(Debug, Clone)]
pub struct SessionCache {
    pool: Pool,
    claim_ttl_ms: u64,
}

impl SessionCache {
    pub(crate) fn new(pool: Pool, claim_ttl_ms: u64) -> Self {
        Self { pool, claim_ttl_ms }
    }

    /// Attempt to claim a thread for a worker.
    ///
    /// Returns `true` if the claim was acquired *or* this worker already owns
    /// it (the TTL is refreshed on re-claim). Returns `false` only when a
    /// *different* worker currently holds the claim. The claim expires after
    /// `claim_ttl_ms` unless refreshed via this method or [`Self::refresh_claim`].
    pub async fn claim_thread(
        &self,
        thread_id: ThreadId,
        worker_id: &str,
    ) -> Result<bool, StoreError> {
        let key = claim_key(thread_id);
        let result: i64 = self
            .pool
            .eval(
                CLAIM_SCRIPT,
                vec![key],
                vec![worker_id.to_owned(), self.claim_ttl_ms.to_string()],
            )
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(result == 1)
    }

    /// Extend the TTL on a claim this worker owns.
    ///
    /// Returns `true` if the claim was refreshed; `false` if it expired or
    /// was taken by another worker (via CAS — the caller should stop work).
    pub async fn refresh_claim(
        &self,
        thread_id: ThreadId,
        worker_id: &str,
    ) -> Result<bool, StoreError> {
        let key = claim_key(thread_id);
        let result: i64 = self
            .pool
            .eval(
                REFRESH_SCRIPT,
                vec![key],
                vec![worker_id.to_owned(), self.claim_ttl_ms.to_string()],
            )
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(result == 1)
    }

    /// Release a claim, provided this worker still owns it.
    ///
    /// A stale release from a worker that already lost its claim is a
    /// no-op thanks to the CAS check.
    pub async fn release_thread(
        &self,
        thread_id: ThreadId,
        worker_id: &str,
    ) -> Result<(), StoreError> {
        let key = claim_key(thread_id);
        let _result: i64 = self
            .pool
            .eval(RELEASE_SCRIPT, vec![key], vec![worker_id.to_owned()])
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(())
    }

    /// Return the current owner of a thread's claim, if any.
    pub async fn claim_owner(&self, thread_id: ThreadId) -> Result<Option<String>, StoreError> {
        self.pool.get(claim_key(thread_id)).await.map_err(|e| StoreError::Cache(e.to_string()))
    }

    /// Persist an opaque loop-state snapshot for `thread_id`.
    ///
    /// The bytes are stored with the same TTL as a thread claim — the
    /// snapshot lives only while there's a reasonable chance the worker is
    /// coming back for the thread. Callers supply the encoded payload
    /// (`elena-core` uses JSON); the cache itself is agnostic.
    pub async fn save_loop_state(
        &self,
        thread_id: ThreadId,
        payload: &[u8],
    ) -> Result<(), StoreError> {
        let key = loop_state_key(thread_id);
        let ttl = self.claim_ttl_ms;
        let _: Option<String> = self
            .pool
            .set(
                key,
                payload,
                Some(Expiration::PX(i64::try_from(ttl).unwrap_or(i64::MAX))),
                None,
                false,
            )
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(())
    }

    /// Load a previously-saved loop-state snapshot for `thread_id`.
    ///
    /// Returns `None` if no snapshot is present (fresh thread, or the TTL
    /// expired).
    pub async fn load_loop_state(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let bytes: Option<Vec<u8>> = self
            .pool
            .get(loop_state_key(thread_id))
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(bytes)
    }

    /// Drop the checkpoint for `thread_id` — typically called when a loop
    /// terminates cleanly so the key doesn't linger.
    pub async fn drop_loop_state(&self, thread_id: ThreadId) -> Result<(), StoreError> {
        let _: i64 = self
            .pool
            .del(loop_state_key(thread_id))
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(())
    }
}

fn claim_key(thread_id: ThreadId) -> String {
    format!("elena:thread:claim:{thread_id}")
}

fn loop_state_key(thread_id: ThreadId) -> String {
    format!("elena:thread:loop:{thread_id}")
}
