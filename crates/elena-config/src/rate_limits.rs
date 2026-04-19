//! Operator-facing rate-limit configuration.
//!
//! Every limit is off-by-default (`u32::MAX`) so upgrading to v1.0 doesn't
//! start rejecting requests from existing tenants until the operator opts
//! in via env / TOML.

use serde::Deserialize;

/// Per-tenant + per-provider + per-plugin rate limits.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitsConfig {
    /// Per-tenant sustained rate (requests / minute). Default: unlimited.
    #[serde(default = "u32_max")]
    pub tenant_rpm: u32,
    /// Per-tenant burst cap (bucket size). Default: `tenant_rpm`.
    #[serde(default)]
    pub tenant_burst: Option<u32>,
    /// Per-tenant concurrent-WS cap. Default: unlimited.
    #[serde(default = "u32_max")]
    pub tenant_inflight_max: u32,
    /// Per-provider global concurrency cap across the cluster.
    /// `None` = unlimited.
    #[serde(default)]
    pub provider_concurrency: Option<u32>,
    /// Per-plugin global concurrency cap across the cluster.
    /// `None` = unlimited.
    #[serde(default)]
    pub plugin_concurrency: Option<u32>,
}

impl Default for RateLimitsConfig {
    fn default() -> Self {
        Self {
            tenant_rpm: u32::MAX,
            tenant_burst: None,
            tenant_inflight_max: u32::MAX,
            provider_concurrency: None,
            plugin_concurrency: None,
        }
    }
}

impl RateLimitsConfig {
    /// Effective burst size — falls back to `tenant_rpm` when unset.
    #[must_use]
    pub fn effective_tenant_burst(&self) -> u32 {
        self.tenant_burst.unwrap_or(self.tenant_rpm)
    }

    /// True when no limits are configured — short-circuits the check.
    #[must_use]
    pub fn is_unlimited(&self) -> bool {
        self.tenant_rpm == u32::MAX
            && self.tenant_inflight_max == u32::MAX
            && self.provider_concurrency.is_none()
            && self.plugin_concurrency.is_none()
    }
}

const fn u32_max() -> u32 {
    u32::MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unlimited() {
        assert!(RateLimitsConfig::default().is_unlimited());
    }

    #[test]
    fn tenant_burst_falls_back_to_rpm() {
        let cfg =
            RateLimitsConfig { tenant_rpm: 600, tenant_burst: None, ..RateLimitsConfig::default() };
        assert_eq!(cfg.effective_tenant_burst(), 600);
    }

    #[test]
    fn explicit_tenant_burst_wins() {
        let cfg = RateLimitsConfig {
            tenant_rpm: 600,
            tenant_burst: Some(1200),
            ..RateLimitsConfig::default()
        };
        assert_eq!(cfg.effective_tenant_burst(), 1200);
    }

    #[test]
    fn deserialises_from_partial_json() {
        let cfg: RateLimitsConfig = serde_json::from_value(serde_json::json!({
            "tenant_rpm": 600,
            "provider_concurrency": 40
        }))
        .unwrap();
        assert_eq!(cfg.tenant_rpm, 600);
        assert_eq!(cfg.tenant_inflight_max, u32::MAX);
        assert_eq!(cfg.provider_concurrency, Some(40));
    }
}
