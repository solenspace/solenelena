//! Redis pool construction (via `fred`).

use elena_config::RedisConfig;
use elena_types::StoreError;
use fred::{
    clients::Pool,
    prelude::{ClientLike, Config as FredConfig, ReconnectPolicy},
    types::Builder,
};
use secrecy::ExposeSecret;

/// Build and initialize a Redis pool from config.
pub(crate) async fn build_pool(cfg: &RedisConfig) -> Result<Pool, StoreError> {
    let fred_cfg = FredConfig::from_url(cfg.url.expose_secret())
        .map_err(|e| StoreError::Cache(format!("invalid redis url: {e}")))?;

    let pool = Builder::from_config(fred_cfg)
        .set_policy(ReconnectPolicy::new_exponential(0, 100, 30_000, 2))
        .build_pool(cfg.pool_max as usize)
        .map_err(|e| StoreError::Cache(format!("redis pool build: {e}")))?;

    pool.init().await.map_err(|e| StoreError::Cache(format!("redis init: {e}")))?;
    Ok(pool)
}
