//! Redis-backed rate limiting for Elena.
//!
//! Four scopes, all served by one atomic Lua script:
//!
//! | Scope | Key format | Semantics |
//! |---|---|---|
//! | Tenant requests/min | `elena:rl:tenant:{id}:rpm` | Refills at `rate_per_sec` to a cap of `burst`. |
//! | Tenant inflight | `elena:rl:tenant:{id}:inflight` | Simple counter; caller acquires + releases. |
//! | Provider concurrency | `elena:rl:provider:{name}` | Cluster-global semaphore — useful when you have many workers sharing one provider quota. |
//! | Plugin concurrency | `elena:rl:plugin:{id}` | Same as provider. |
//!
//! The token-bucket variant ([`try_take`]) is the hot path; the inflight
//! counter ([`acquire_inflight`] / [`release_inflight`]) is used by the
//! gateway for long-lived connections.

use elena_types::{StoreError, TenantId};
use fred::{
    clients::Pool,
    prelude::{KeysInterface, LuaInterface},
};

/// Atomic Lua token bucket: take 1 token if available, else report the
/// nearest refill time in millis. The bucket auto-refills at
/// `rate_per_sec` and caps at `burst`.
///
/// Returns `{1, <remaining>}` on success or `{0, <retry_after_ms>}` on miss.
const TOKEN_BUCKET_SCRIPT: &str = r"
local key = KEYS[1]
local rate = tonumber(ARGV[1])
local burst = tonumber(ARGV[2])
local now_ms = tonumber(ARGV[3])
local data = redis.call('HMGET', key, 'tokens', 'ts')
local tokens = tonumber(data[1])
local ts = tonumber(data[2])
if tokens == nil then
    tokens = burst
    ts = now_ms
end
local elapsed = math.max(0, now_ms - ts) / 1000.0
tokens = math.min(burst, tokens + elapsed * rate)
if tokens >= 1 then
    tokens = tokens - 1
    redis.call('HSET', key, 'tokens', tokens, 'ts', now_ms)
    -- Keep the key alive for a while past idle.
    redis.call('PEXPIRE', key, 60000)
    return {1, math.floor(tokens)}
else
    local deficit = 1 - tokens
    local retry_after = math.floor((deficit / rate) * 1000)
    return {0, retry_after}
end
";

/// Atomic Lua increment-if-below-cap: acquire one inflight slot if the
/// counter is below `max`. Returns `{1, <new_count>}` or `{0, 0}`.
const INFLIGHT_ACQUIRE_SCRIPT: &str = r"
local key = KEYS[1]
local max = tonumber(ARGV[1])
local current = tonumber(redis.call('GET', key) or 0)
if current < max then
    current = redis.call('INCR', key)
    redis.call('EXPIRE', key, 300)
    return {1, current}
else
    return {0, current}
end
";

/// Handle to Redis-backed rate-limit state.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    pool: Pool,
}

/// Decision from [`RateLimiter::try_take`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateDecision {
    /// Caller may proceed. `remaining` is the approximate remaining
    /// token count (informational).
    Allow {
        /// Remaining tokens after this call.
        remaining: u64,
    },
    /// Caller must back off for at least `retry_after_ms` before retrying.
    Deny {
        /// Milliseconds until the next token is available.
        retry_after_ms: u64,
    },
}

impl RateLimiter {
    /// Build a rate-limiter handle against an existing Redis pool.
    #[must_use]
    pub(crate) const fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Take one token from `key`'s bucket, refilling at `rate_per_sec`,
    /// capping at `burst`. Intended for per-request scopes like
    /// `tenant:{id}:rpm`.
    pub async fn try_take(
        &self,
        key: &str,
        rate_per_sec: u32,
        burst: u32,
    ) -> Result<RateDecision, StoreError> {
        let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        let result: Vec<i64> = self
            .pool
            .eval(
                TOKEN_BUCKET_SCRIPT,
                vec![key.to_owned()],
                vec![rate_per_sec.to_string(), burst.to_string(), now_ms.to_string()],
            )
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;

        let allowed = result.first().copied().unwrap_or(0) == 1;
        let payload = u64::try_from(result.get(1).copied().unwrap_or(0).max(0)).unwrap_or(0);
        Ok(if allowed {
            RateDecision::Allow { remaining: payload }
        } else {
            RateDecision::Deny { retry_after_ms: payload }
        })
    }

    /// Acquire one "inflight" slot. Pair with [`Self::release_inflight`]
    /// when the in-flight work completes.
    pub async fn acquire_inflight(&self, key: &str, max: u32) -> Result<RateDecision, StoreError> {
        let result: Vec<i64> = self
            .pool
            .eval(INFLIGHT_ACQUIRE_SCRIPT, vec![key.to_owned()], vec![max.to_string()])
            .await
            .map_err(|e| StoreError::Cache(e.to_string()))?;
        let allowed = result.first().copied().unwrap_or(0) == 1;
        let current = u64::try_from(result.get(1).copied().unwrap_or(0).max(0)).unwrap_or(0);
        Ok(if allowed {
            RateDecision::Allow { remaining: current }
        } else {
            RateDecision::Deny { retry_after_ms: 1_000 }
        })
    }

    /// Release one "inflight" slot previously taken via
    /// [`Self::acquire_inflight`]. Clamps at 0 so stray releases can't
    /// drive the counter negative.
    pub async fn release_inflight(&self, key: &str) -> Result<(), StoreError> {
        let _: i64 = self.pool.decr(key).await.map_err(|e| StoreError::Cache(e.to_string()))?;
        // Best-effort: if the decr drove the value below 0 (e.g. stale
        // release after a TTL expiry), clamp back to 0.
        let cur: Option<i64> =
            self.pool.get(key).await.map_err(|e| StoreError::Cache(e.to_string()))?;
        if cur.unwrap_or(0) < 0 {
            let _: Option<String> = self
                .pool
                .set(key, 0_i64, None, None, false)
                .await
                .map_err(|e| StoreError::Cache(e.to_string()))?;
        }
        Ok(())
    }
}

/// Build the canonical per-tenant RPM key.
#[must_use]
pub fn tenant_rpm_key(tenant: TenantId) -> String {
    format!("elena:rl:tenant:{tenant}:rpm")
}

/// Build the canonical per-tenant inflight key.
#[must_use]
pub fn tenant_inflight_key(tenant: TenantId) -> String {
    format!("elena:rl:tenant:{tenant}:inflight")
}

/// Build the canonical per-provider concurrency key.
#[must_use]
pub fn provider_concurrency_key(name: &str) -> String {
    format!("elena:rl:provider:{name}")
}

/// Build the canonical per-plugin concurrency key.
#[must_use]
pub fn plugin_concurrency_key(id: &str) -> String {
    format!("elena:rl:plugin:{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_formats_are_stable() {
        let t = TenantId::new();
        assert_eq!(tenant_rpm_key(t), format!("elena:rl:tenant:{t}:rpm"));
        assert_eq!(tenant_inflight_key(t), format!("elena:rl:tenant:{t}:inflight"));
        assert_eq!(provider_concurrency_key("groq"), "elena:rl:provider:groq");
        assert_eq!(plugin_concurrency_key("echo"), "elena:rl:plugin:echo");
    }
}
