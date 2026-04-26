//! App persistence — admin grouping above tenants.
//!
//! Apps are an admin-only concept. The runtime never reads from this table;
//! the admin panel uses it to manage Solen, Hannlys, Omnii, ... as distinct
//! products with their own onboarding defaults.

use chrono::{DateTime, Utc};
use elena_types::{App, AppId, AppSlug, StoreError};
use serde_json::Value;
use sqlx::{PgPool, QueryBuilder, Row, postgres::PgRow};
use tracing::instrument;

use crate::sql_error::{classify_serde, classify_sqlx};

/// Aggregate token usage across every tenant attached to one app, summed
/// over a time window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppUsageSummary {
    /// Total tokens billed (sum of `messages.token_count`).
    pub tokens_total: u64,
    /// Number of distinct tenants under the app that produced any usage
    /// in the window.
    pub tenant_count: u32,
    /// Number of distinct threads contributing to the usage.
    pub thread_count: u32,
}

/// CRUD over the `apps` table.
#[derive(Debug, Clone)]
pub struct AppStore {
    pool: PgPool,
}

impl AppStore {
    /// Build a store from an existing pg pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create or replace an app.
    ///
    /// Idempotent on `id`. The `apps_slug_uidx` unique index surfaces as
    /// [`StoreError::Conflict`] when a slug is already taken by a different
    /// app — admins must rename or pick a fresh slug.
    #[instrument(skip(self, app), fields(app_id = %app.id, slug = %app.slug))]
    pub async fn upsert(&self, app: &App) -> Result<(), StoreError> {
        let metadata = serde_json::to_value(&app.metadata).map_err(|e| classify_serde(&e))?;

        sqlx::query(
            "INSERT INTO apps (
                 id, slug, display_name, default_plan_template,
                 default_allowed_plugin_ids, metadata
             )
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE SET
                 slug                       = EXCLUDED.slug,
                 display_name               = EXCLUDED.display_name,
                 default_plan_template      = EXCLUDED.default_plan_template,
                 default_allowed_plugin_ids = EXCLUDED.default_allowed_plugin_ids,
                 metadata                   = EXCLUDED.metadata",
        )
        .bind(app.id.as_uuid())
        .bind(app.slug.as_str())
        .bind(&app.display_name)
        .bind(app.default_plan_template.as_ref())
        .bind(&app.default_allowed_plugin_ids)
        .bind(metadata)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        Ok(())
    }

    /// Fetch an app by id. `Ok(None)` for unknown ids.
    #[instrument(skip(self))]
    pub async fn get(&self, id: AppId) -> Result<Option<App>, StoreError> {
        let row = sqlx::query(SELECT_APP_BY_ID)
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(classify_sqlx)?;

        row.as_ref().map(decode_app).transpose()
    }

    /// Fetch an app by slug. `Ok(None)` for unknown slugs.
    #[instrument(skip(self), fields(slug = %slug))]
    pub async fn get_by_slug(&self, slug: &AppSlug) -> Result<Option<App>, StoreError> {
        let row = sqlx::query(
            "SELECT id, slug, display_name, default_plan_template,
                    default_allowed_plugin_ids, metadata, created_at, updated_at
             FROM apps WHERE slug = $1",
        )
        .bind(slug.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        row.as_ref().map(decode_app).transpose()
    }

    /// List apps ordered by `created_at` ascending, with offset pagination.
    ///
    /// The admin panel typically lists every app (Solen / Hannlys / Omnii
    /// is a small set), so a keyset cursor is overkill — `limit`/`offset`
    /// is enough.
    #[instrument(skip(self))]
    pub async fn list(&self, limit: u32, offset: u32) -> Result<Vec<App>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, slug, display_name, default_plan_template,
                    default_allowed_plugin_ids, metadata, created_at, updated_at
             FROM apps
             ORDER BY created_at ASC
             LIMIT $1 OFFSET $2",
        )
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        rows.iter().map(decode_app).collect()
    }

    /// Delete an app.
    ///
    /// Returns [`StoreError::Conflict`] if any live tenant still references
    /// it — admins must reassign or soft-delete those tenants first. The
    /// FK on `tenants.app_id` is `ON DELETE SET NULL`, so without this
    /// pre-check the app would silently disappear from existing tenants.
    #[instrument(skip(self))]
    pub async fn delete(&self, id: AppId) -> Result<(), StoreError> {
        let in_use: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM tenants
             WHERE app_id = $1 AND deleted_at IS NULL
             LIMIT 1",
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        if in_use.is_some() {
            return Err(StoreError::Conflict(format!(
                "app {id} has active tenants; reassign or soft-delete them first"
            )));
        }

        let rows = sqlx::query("DELETE FROM apps WHERE id = $1")
            .bind(id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(classify_sqlx)?;

        if rows.rows_affected() == 0 {
            return Err(StoreError::Database(format!("no app {id}")));
        }
        Ok(())
    }

    /// Sum token usage across every tenant under an app, optionally
    /// bounded to a `[since, until)` window. Soft-deleted tenants are
    /// excluded.
    #[instrument(skip(self))]
    pub async fn usage_summary(
        &self,
        id: AppId,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<AppUsageSummary, StoreError> {
        let mut qb = QueryBuilder::<sqlx::Postgres>::new(
            "SELECT
                 COALESCE(SUM(m.token_count), 0)::bigint AS tokens_total,
                 COUNT(DISTINCT t.id)                    AS tenant_count,
                 COUNT(DISTINCT m.thread_id)             AS thread_count
             FROM messages m
             JOIN tenants t ON t.id = m.tenant_id
             WHERE t.app_id = ",
        );
        qb.push_bind(id.as_uuid());
        qb.push(" AND t.deleted_at IS NULL");

        if let Some(since) = since {
            qb.push(" AND m.created_at >= ").push_bind(since);
        }
        if let Some(until) = until {
            qb.push(" AND m.created_at < ").push_bind(until);
        }

        let row = qb.build().fetch_one(&self.pool).await.map_err(classify_sqlx)?;
        let tokens_total: i64 = row.try_get("tokens_total").map_err(classify_sqlx)?;
        let tenant_count: i64 = row.try_get("tenant_count").map_err(classify_sqlx)?;
        let thread_count: i64 = row.try_get("thread_count").map_err(classify_sqlx)?;

        Ok(AppUsageSummary {
            tokens_total: u64::try_from(tokens_total).unwrap_or(0),
            tenant_count: u32::try_from(tenant_count).unwrap_or(0),
            thread_count: u32::try_from(thread_count).unwrap_or(0),
        })
    }
}

const SELECT_APP_BY_ID: &str = "SELECT id, slug, display_name, default_plan_template,
            default_allowed_plugin_ids, metadata, created_at, updated_at
     FROM apps WHERE id = $1";

fn decode_app(row: &PgRow) -> Result<App, StoreError> {
    let id: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let slug: String = row.try_get("slug").map_err(classify_sqlx)?;
    let display_name: String = row.try_get("display_name").map_err(classify_sqlx)?;
    let default_plan_template: Option<Value> =
        row.try_get("default_plan_template").map_err(classify_sqlx)?;
    let default_allowed_plugin_ids: Vec<String> =
        row.try_get("default_allowed_plugin_ids").map_err(classify_sqlx)?;
    let metadata: Value = row.try_get("metadata").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(classify_sqlx)?;

    Ok(App {
        id: AppId::from_uuid(id),
        slug: AppSlug::new(slug),
        display_name,
        default_plan_template,
        default_allowed_plugin_ids,
        metadata: serde_json::from_value(metadata).map_err(|e| classify_serde(&e))?,
        created_at,
        updated_at,
    })
}
