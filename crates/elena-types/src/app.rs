//! Admin-defined apps (Solen, Hannlys, Omnii, ...).
//!
//! An *app* is the operator-facing grouping above tenants. Elena's runtime is
//! application-agnostic and never references [`AppId`] on the request path —
//! this type exists purely to give the admin panel a stable handle for
//! filtering, default-template inheritance, and bulk operations.
//!
//! Tenants belong to at most one app via `tenants.app_id` (nullable). The
//! `default_plan_template` and `default_allowed_plugin_ids` fields are
//! consumed only by the onboarding endpoint when a fresh tenant is created
//! under an app — once the tenant exists, they no longer affect runtime.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::AppId;

/// Human-readable identifier for an app. Lowercase kebab-case is the convention
/// (`"solen"`, `"hannlys"`, `"omnii"`); the engine never pattern-matches on it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppSlug(pub String);

impl AppSlug {
    /// Wrap an owned [`String`] as a slug.
    #[must_use]
    pub fn new(slug: impl Into<String>) -> Self {
        Self(slug.into())
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AppSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AppSlug({})", self.0)
    }
}

impl std::fmt::Display for AppSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for AppSlug {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for AppSlug {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Full app record as persisted in the `apps` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct App {
    /// Stable app identifier.
    pub id: AppId,
    /// Globally-unique slug (lowercase kebab-case by convention).
    pub slug: AppSlug,
    /// Human-readable name shown in admin UIs.
    pub display_name: String,
    /// Plan blueprint applied when a tenant is onboarded under this app.
    /// Opaque [`serde_json::Value`] — the onboarding endpoint parses it
    /// against [`crate::Plan`] at write time. `None` means "no template;
    /// onboarding seeds a default plan from the tenant tier".
    #[serde(default)]
    pub default_plan_template: Option<serde_json::Value>,
    /// Plugin allow-list applied to tenants onboarded under this app. The
    /// runtime intersects this with the per-tenant and per-workspace lists
    /// — this field only seeds the initial value.
    #[serde(default)]
    pub default_allowed_plugin_ids: Vec<String>,
    /// App-specific metadata. Opaque to Elena — the admin panel may stash
    /// branding, billing-anchor, or marketing-copy keys here.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    /// When the row was created.
    pub created_at: DateTime<Utc>,
    /// When the row was last updated.
    pub updated_at: DateTime<Utc>,
}
