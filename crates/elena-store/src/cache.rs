//! Redis-backed hot state: thread claims, session data, rate-limit windows.
//!
//! The claim primitives use a Lua CAS script so a stale `release` from a
//! worker that lost connection cannot unclaim a thread that was legitimately
//! reassigned to a new worker.

use std::collections::HashSet;

use elena_types::{RequestId, StoreError, ThreadId, ToolCallId};
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

    /// S4 — Persist the set of tool-call IDs the loop is currently
    /// awaiting approvals for. Stored as a JSON-encoded array of ULID
    /// strings under a per-thread key with a `ttl_ms` expiry that
    /// matches the pause deadline. The gateway's approvals POST loads
    /// this set and rejects any decision whose `tool_use_id` isn't in
    /// it, defending against replays of an old approval batch.
    pub async fn save_pending_approvals(
        &self,
        thread_id: ThreadId,
        ids: &HashSet<ToolCallId>,
        ttl_ms: u64,
    ) -> Result<(), StoreError> {
        let payload =
            serde_json::to_vec(ids).map_err(|e| StoreError::Serialization(e.to_string()))?;
        let _: Option<String> = self
            .pool
            .set(
                pending_approvals_key(thread_id),
                payload,
                Some(Expiration::PX(i64::try_from(ttl_ms).unwrap_or(i64::MAX))),
                None,
                false,
            )
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(())
    }

    /// Load the pending-approvals set. `Ok(None)` means the loop is
    /// not currently awaiting approval (or the key TTL'd out).
    pub async fn load_pending_approvals(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<HashSet<ToolCallId>>, StoreError> {
        let bytes: Option<Vec<u8>> = self
            .pool
            .get(pending_approvals_key(thread_id))
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        let Some(bytes) = bytes else { return Ok(None) };
        let set: HashSet<ToolCallId> =
            serde_json::from_slice(&bytes).map_err(|e| StoreError::Serialization(e.to_string()))?;
        Ok(Some(set))
    }

    /// X4 — Atomic per-thread event-offset INCR.
    ///
    /// Used by the worker's event publisher to stamp every published
    /// `StreamEvent` with a monotonic offset. Survives worker /
    /// gateway restarts because the counter is in Redis. The key has
    /// no TTL — a long-lived thread keeps a stable offset namespace.
    pub async fn next_event_offset(&self, thread_id: ThreadId) -> Result<u64, StoreError> {
        let key = event_offset_key(thread_id);
        let n: i64 = self.pool.incr(key).await.map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(u64::try_from(n).unwrap_or(u64::MAX))
    }

    /// S5 — `WorkRequest` idempotency guard.
    ///
    /// `JetStream` is at-least-once: a worker that crashes between
    /// processing a request and ack-ing it will see the same
    /// `WorkRequest` redelivered. Without this guard the redelivery
    /// re-runs every side effect (rate-limit decrement, claim attempt,
    /// even tool execution if the dedup fingerprint isn't yet written).
    ///
    /// Returns `true` if this is the first time we've seen `request_id`
    /// (caller proceeds). Returns `false` if the request has already
    /// been processed (caller acks immediately and skips). Failure to
    /// reach Redis falls open (treats it as first-seen) so a Redis
    /// outage degrades to the pre-S5 at-least-once posture rather than
    /// dropping work entirely.
    pub async fn try_claim_request(
        &self,
        request_id: RequestId,
        ttl_seconds: u64,
    ) -> Result<bool, StoreError> {
        let key = work_request_key(request_id);
        let res: Option<String> = self
            .pool
            .set(
                key,
                "1",
                Some(Expiration::EX(i64::try_from(ttl_seconds).unwrap_or(i64::MAX))),
                Some(fred::types::SetOptions::NX),
                false,
            )
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        Ok(res.is_some())
    }

    /// Drop the pending-approvals set — called when the loop transitions
    /// out of `AwaitingApproval` (resumed, completed, or failed). Failure
    /// to clear is non-fatal; the TTL eventually reaps the key.
    pub async fn clear_pending_approvals(&self, thread_id: ThreadId) -> Result<(), StoreError> {
        let _: i64 = self
            .pool
            .del(pending_approvals_key(thread_id))
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

fn pending_approvals_key(thread_id: ThreadId) -> String {
    format!("elena:thread:pending_approvals:{thread_id}")
}

fn work_request_key(request_id: RequestId) -> String {
    format!("elena:wr:{request_id}")
}

fn event_offset_key(thread_id: ThreadId) -> String {
    format!("elena:thread:event_offset:{thread_id}")
}
