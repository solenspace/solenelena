//! Pure configuration validation.
//!
//! Consulting only the struct fields (no I/O). Returns the first error it
//! finds — operators should fix violations one at a time.

use elena_types::ConfigError;
use secrecy::ExposeSecret;

use crate::ElenaConfig;

/// Validate a loaded configuration.
///
/// Checks for:
/// - `postgres.pool_max >= postgres.pool_min`
/// - `postgres.statement_timeout_ms <= 300_000` (5 min upper bound)
/// - `postgres.url` parses as a URL with a recognized scheme
/// - `redis.url` parses as a URL with a recognized scheme
pub fn validate(cfg: &ElenaConfig) -> Result<(), ConfigError> {
    if cfg.postgres.pool_max < cfg.postgres.pool_min {
        return Err(ConfigError::Invalid {
            key: "postgres.pool_max".into(),
            message: format!(
                "pool_max ({}) must be >= pool_min ({})",
                cfg.postgres.pool_max, cfg.postgres.pool_min
            ),
        });
    }

    if cfg.postgres.statement_timeout_ms > 300_000 {
        return Err(ConfigError::Invalid {
            key: "postgres.statement_timeout_ms".into(),
            message: format!(
                "statement_timeout_ms ({}) exceeds safe upper bound 300000",
                cfg.postgres.statement_timeout_ms
            ),
        });
    }

    validate_url_scheme(
        cfg.postgres.url.expose_secret(),
        "postgres.url",
        &["postgres", "postgresql"],
    )?;

    validate_url_scheme(
        cfg.redis.url.expose_secret(),
        "redis.url",
        &["redis", "rediss", "redis-cluster"],
    )?;

    Ok(())
}

fn validate_url_scheme(url: &str, key: &str, allowed_schemes: &[&str]) -> Result<(), ConfigError> {
    let Some((scheme, _)) = url.split_once("://") else {
        return Err(ConfigError::Invalid {
            key: key.to_owned(),
            message: format!("must be a URL with a scheme (got {url:?})"),
        });
    };
    if !allowed_schemes.contains(&scheme) {
        return Err(ConfigError::Invalid {
            key: key.to_owned(),
            message: format!("unexpected scheme {scheme:?}; expected one of {allowed_schemes:?}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;
    use crate::{
        CacheConfig, ContextConfig, DefaultsConfig, LoggingConfig, PostgresConfig, RedisConfig,
        RouterConfig, logging::LogFormat,
    };

    fn valid() -> ElenaConfig {
        ElenaConfig {
            postgres: PostgresConfig {
                url: SecretString::from("postgres://u:p@localhost:5432/db"),
                pool_max: 20,
                pool_min: 2,
                connect_timeout_ms: 5000,
                statement_timeout_ms: 30_000,
            },
            redis: RedisConfig {
                url: SecretString::from("redis://localhost:6379"),
                pool_max: 10,
                thread_claim_ttl_ms: 60_000,
            },
            anthropic: None,
            cache: CacheConfig::default(),
            logging: LoggingConfig { level: "info".into(), format: LogFormat::Pretty },
            defaults: DefaultsConfig::default(),
            context: ContextConfig::default(),
            router: RouterConfig::default(),
            providers: crate::ProvidersConfig::default(),
            rate_limits: crate::RateLimitsConfig::default(),
        }
    }

    #[test]
    fn valid_config_passes() {
        validate(&valid()).unwrap();
    }

    #[test]
    fn pool_max_less_than_min_is_rejected() {
        let mut c = valid();
        c.postgres.pool_max = 1;
        c.postgres.pool_min = 5;
        let err = validate(&c).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
    }

    #[test]
    fn statement_timeout_upper_bound_enforced() {
        let mut c = valid();
        c.postgres.statement_timeout_ms = 400_000;
        let err = validate(&c).unwrap_err();
        if let ConfigError::Invalid { key, .. } = err {
            assert_eq!(key, "postgres.statement_timeout_ms");
        } else {
            panic!("expected Invalid, got {err:?}");
        }
    }

    #[test]
    fn postgres_url_must_have_known_scheme() {
        let mut c = valid();
        c.postgres.url = SecretString::from("mysql://host/db");
        assert!(validate(&c).is_err());
    }

    #[test]
    fn postgresql_scheme_is_accepted() {
        let mut c = valid();
        c.postgres.url = SecretString::from("postgresql://u:p@localhost:5432/db");
        validate(&c).unwrap();
    }

    #[test]
    fn redis_rediss_scheme_accepted() {
        let mut c = valid();
        c.redis.url = SecretString::from("rediss://host:6379");
        validate(&c).unwrap();
    }

    #[test]
    fn malformed_url_rejected() {
        let mut c = valid();
        c.postgres.url = SecretString::from("not-a-url");
        assert!(validate(&c).is_err());
    }
}
