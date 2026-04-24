//! Configuration loader for Elena.
//!
//! Layers (later overrides earlier):
//! 1. Built-in defaults (via `#[serde(default = ...)]`).
//! 2. `/etc/elena/elena.toml` (if present).
//! 3. `$ELENA_CONFIG_FILE` (if set and exists).
//! 4. Environment variables prefixed `ELENA_` (nested via `__`).
//!
//! All credentials are wrapped in [`secrecy::SecretString`]. [`ElenaConfig`]
//! intentionally does not implement [`serde::Serialize`] so a logger cannot
//! accidentally dump secrets. Inspect values explicitly via
//! [`secrecy::ExposeSecret`] at the use site.

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![warn(missing_docs)]
#![cfg_attr(
    test,
    // Tests use unwrap/expect freely; figment::Error is large but unavoidable.
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::result_large_err
    )
)]

use std::path::PathBuf;

use elena_types::ConfigError;
use serde::Deserialize;

mod anthropic;
mod cache;
mod context;
mod defaults;
mod logging;
mod postgres;
mod providers;
mod rate_limits;
mod redis;
mod router;
mod validate;

pub use anthropic::AnthropicConfig;
pub use cache::CacheConfig;
pub use context::ContextConfig;
pub use defaults::{DefaultsConfig, TierEntry, TierModels};
pub use logging::{LogFormat, LoggingConfig};
pub use postgres::PostgresConfig;
pub use providers::{OpenAiCompatProviderEntry, ProvidersConfig};
pub use rate_limits::RateLimitsConfig;
pub use redis::RedisConfig;
pub use router::RouterConfig;
pub use validate::validate;

/// Top-level Elena configuration.
///
/// Load with [`load`] (reads all layers) or [`load_with`] (explicit sources).
/// Validate with [`validate`] before using.
#[derive(Debug, Clone, Deserialize)]
pub struct ElenaConfig {
    /// Postgres connection settings.
    pub postgres: PostgresConfig,

    /// Redis connection settings.
    pub redis: RedisConfig,

    /// Anthropic LLM client settings (legacy, single-provider path).
    ///
    /// Populated by `ELENA_ANTHROPIC__*` env vars and kept for backward
    /// compatibility with older single-provider callers. New deployments
    /// should prefer [`ProvidersConfig`] which supports Anthropic +
    /// OpenAI-compatible providers side by side.
    #[serde(default)]
    pub anthropic: Option<AnthropicConfig>,

    /// Multi-provider LLM configuration (Anthropic + OpenAI-compatible).
    #[serde(default)]
    pub providers: ProvidersConfig,

    /// Per-tenant + per-provider + per-plugin rate limits. Defaults to
    /// unlimited so existing deployments don't start rejecting requests
    /// after an upgrade; operators opt in via env or TOML.
    #[serde(default)]
    pub rate_limits: RateLimitsConfig,

    /// Prompt-cache policy (allowlist for 1-hour TTL eligibility).
    #[serde(default)]
    pub cache: CacheConfig,

    /// Logging / tracing settings.
    #[serde(default = "default_logging")]
    pub logging: LoggingConfig,

    /// Operator-supplied defaults (e.g., per-tier budgets).
    #[serde(default)]
    pub defaults: DefaultsConfig,

    /// Context / retrieval settings.
    #[serde(default)]
    pub context: ContextConfig,

    /// Router settings.
    #[serde(default)]
    pub router: RouterConfig,
}

fn default_logging() -> LoggingConfig {
    LoggingConfig { level: "info".into(), format: LogFormat::Pretty }
}

/// Load configuration from the default source layers.
///
/// Combines:
/// 1. `/etc/elena/elena.toml` (if present; missing is not an error).
/// 2. `$ELENA_CONFIG_FILE` (if set and file exists).
/// 3. `ELENA_*` environment variables (with `__` as the nesting separator).
///
/// Calls [`validate`] on the assembled config before returning. Use
/// [`load_with`] if you need custom source layering (e.g., in tests).
pub fn load() -> Result<ElenaConfig, ConfigError> {
    let extra = std::env::var("ELENA_CONFIG_FILE").ok().map(PathBuf::from);
    load_with(Some(PathBuf::from("/etc/elena/elena.toml")), extra, true)
}

/// Load configuration from explicit sources.
///
/// - `system_file`: an optional system config file (read if the file exists).
/// - `user_file`: an optional user-supplied config file (read if the file exists).
/// - `read_env`: if `true`, merge `ELENA_*` environment variables.
pub fn load_with(
    system_file: Option<PathBuf>,
    user_file: Option<PathBuf>,
    read_env: bool,
) -> Result<ElenaConfig, ConfigError> {
    use figment::Figment;
    use figment::providers::{Env, Format, Toml};

    let mut figment = Figment::new();

    if let Some(path) = system_file {
        if path.exists() {
            figment = figment.merge(Toml::file(path));
        }
    }
    if let Some(path) = user_file {
        if path.exists() {
            figment = figment.merge(Toml::file(path));
        }
    }
    if read_env {
        figment = figment.merge(Env::prefixed("ELENA_").split("__"));
    }

    let cfg: ElenaConfig = figment.extract().map_err(|e| {
        // Turn figment's error into our typed `ConfigError`. Missing-key
        // errors get mapped to `Missing`, everything else to `Invalid`.
        if let Some(path) = first_missing_key(&e) {
            ConfigError::Missing { key: path }
        } else {
            ConfigError::Invalid { key: error_key(&e), message: e.to_string() }
        }
    })?;

    validate(&cfg)?;
    Ok(cfg)
}

fn first_missing_key(err: &figment::Error) -> Option<String> {
    // figment `MissingField` errors surface the struct path in `kind`.
    err.clone().into_iter().find_map(|e| match e.kind {
        figment::error::Kind::MissingField(k) => Some(k.to_string()),
        _ => None,
    })
}

fn error_key(err: &figment::Error) -> String {
    let path = err.path.join(".");
    if path.is_empty() { "config".to_owned() } else { path }
}

#[cfg(test)]
mod tests {
    use figment::Jail;
    use secrecy::ExposeSecret;

    use super::*;

    const BASIC_TOML: &str = r#"
[postgres]
url = "postgres://u:p@localhost:5432/db"

[redis]
url = "redis://localhost:6379"
"#;

    #[test]
    fn loads_from_toml_file() {
        Jail::expect_with(|jail| {
            jail.create_file("elena.toml", BASIC_TOML)?;
            let cfg =
                load_with(None, Some(jail.directory().join("elena.toml")), false).expect("load ok");
            assert_eq!(cfg.postgres.pool_max, 20); // default
            assert_eq!(cfg.postgres.pool_min, 2);
            assert_eq!(cfg.postgres.url.expose_secret(), "postgres://u:p@localhost:5432/db");
            Ok(())
        });
    }

    #[test]
    fn env_overrides_toml() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "elena.toml",
                r#"
[postgres]
url = "postgres://toml:p@localhost:5432/db"

[redis]
url = "redis://localhost:6379"
"#,
            )?;
            jail.set_env("ELENA_POSTGRES__URL", "postgres://env:p@localhost:5432/db");
            let cfg =
                load_with(None, Some(jail.directory().join("elena.toml")), true).expect("load ok");
            assert_eq!(cfg.postgres.url.expose_secret(), "postgres://env:p@localhost:5432/db");
            Ok(())
        });
    }

    #[test]
    fn env_only_loading_works() {
        Jail::expect_with(|jail| {
            jail.set_env("ELENA_POSTGRES__URL", "postgres://u:p@localhost:5432/db");
            jail.set_env("ELENA_REDIS__URL", "redis://localhost:6379");
            let cfg = load_with(None, None, true).expect("load ok");
            assert_eq!(cfg.postgres.pool_max, 20);
            assert_eq!(cfg.redis.thread_claim_ttl_ms, 60_000);
            Ok(())
        });
    }

    #[test]
    fn missing_required_field_reports_missing() {
        // No TOML, no relevant env vars — postgres.url is required.
        Jail::expect_with(|_jail| {
            let err = load_with(None, None, false).unwrap_err();
            assert!(matches!(err, ConfigError::Missing { .. } | ConfigError::Invalid { .. }));
            Ok(())
        });
    }

    #[test]
    fn bad_scheme_reports_invalid() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "elena.toml",
                r#"
[postgres]
url = "mysql://u:p@localhost:3306/db"

[redis]
url = "redis://localhost:6379"
"#,
            )?;
            let err =
                load_with(None, Some(jail.directory().join("elena.toml")), false).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }));
            Ok(())
        });
    }

    #[test]
    fn defaults_are_filled_in() {
        Jail::expect_with(|jail| {
            jail.create_file("elena.toml", BASIC_TOML)?;
            let cfg =
                load_with(None, Some(jail.directory().join("elena.toml")), false).expect("load ok");
            assert_eq!(cfg.logging.level, "info");
            assert_eq!(cfg.logging.format, LogFormat::Pretty);
            assert_eq!(cfg.redis.pool_max, 10);
            assert_eq!(cfg.redis.thread_claim_ttl_ms, 60_000);
            Ok(())
        });
    }

    #[test]
    fn nonexistent_file_is_not_an_error() {
        // Only the `user_file` — nonexistent — with env providing the required fields.
        Jail::expect_with(|jail| {
            jail.set_env("ELENA_POSTGRES__URL", "postgres://u:p@localhost:5432/db");
            jail.set_env("ELENA_REDIS__URL", "redis://localhost:6379");
            let cfg = load_with(Some(jail.directory().join("does-not-exist.toml")), None, true)
                .expect("load ok");
            assert_eq!(cfg.postgres.url.expose_secret(), "postgres://u:p@localhost:5432/db");
            Ok(())
        });
    }
}
