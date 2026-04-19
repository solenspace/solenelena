//! Postgres pool construction.

use std::time::Duration;

use elena_config::PostgresConfig;
use elena_types::StoreError;
use secrecy::ExposeSecret;
use sqlx::{
    Executor, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::sql_error::classify_sqlx;

/// Build a Postgres pool from the given config.
///
/// Applies a `SET statement_timeout = <ms>` on every acquired connection so
/// a runaway query can't hold a pool connection indefinitely.
pub(crate) async fn build_pool(cfg: &PostgresConfig) -> Result<PgPool, StoreError> {
    let conn_opts: PgConnectOptions = cfg
        .url
        .expose_secret()
        .parse()
        .map_err(|e: sqlx::Error| StoreError::Database(format!("invalid postgres url: {e}")))?;

    let stmt_timeout_ms = cfg.statement_timeout_ms;

    let pool = PgPoolOptions::new()
        .max_connections(cfg.pool_max)
        .min_connections(cfg.pool_min)
        .acquire_timeout(Duration::from_millis(cfg.connect_timeout_ms))
        .after_connect(move |conn, _meta| {
            let sql = format!("SET statement_timeout = {stmt_timeout_ms}");
            Box::pin(async move {
                conn.execute(sql.as_str()).await?;
                Ok(())
            })
        })
        .connect_with(conn_opts)
        .await
        .map_err(classify_sqlx)?;

    Ok(pool)
}
