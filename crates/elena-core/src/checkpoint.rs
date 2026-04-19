//! Redis checkpoint helpers for [`LoopState`].
//!
//! Thin JSON-serde wrappers over
//! [`SessionCache::save_loop_state`](elena_store::SessionCache::save_loop_state)
//! and [`load_loop_state`](elena_store::SessionCache::load_loop_state).
//! The cache itself is serialization-agnostic (bytes in, bytes out); this
//! module owns the JSON encoding so `elena-store` stays free of a
//! dependency on `elena-core`'s state types.

use elena_store::SessionCache;
use elena_types::{StoreError, ThreadId};

use crate::state::LoopState;

/// Encode `state` as JSON and save it under `state.thread_id`.
pub async fn save_loop_state(cache: &SessionCache, state: &LoopState) -> Result<(), StoreError> {
    let payload =
        serde_json::to_vec(state).map_err(|e| StoreError::Serialization(e.to_string()))?;
    cache.save_loop_state(state.thread_id, &payload).await
}

/// Load a previously-saved [`LoopState`] for `thread_id`.
///
/// Returns `Ok(None)` if no snapshot is present or if the stored bytes fail
/// to decode (stale schema after an upgrade).
pub async fn load_loop_state(
    cache: &SessionCache,
    thread_id: ThreadId,
) -> Result<Option<LoopState>, StoreError> {
    let Some(bytes) = cache.load_loop_state(thread_id).await? else {
        return Ok(None);
    };
    match serde_json::from_slice::<LoopState>(&bytes) {
        Ok(state) => Ok(Some(state)),
        Err(e) => {
            tracing::warn!(
                thread_id = %thread_id,
                error = %e,
                "dropping unreadable LoopState checkpoint (schema drift?)"
            );
            // Best-effort cleanup so stale bytes don't come back next time.
            let _ = cache.drop_loop_state(thread_id).await;
            Ok(None)
        }
    }
}

/// Remove a checkpoint (typically on clean loop termination).
pub async fn drop_loop_state(cache: &SessionCache, thread_id: ThreadId) -> Result<(), StoreError> {
    cache.drop_loop_state(thread_id).await
}
