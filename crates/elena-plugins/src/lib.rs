#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! `elena-plugins` — Elena's plugin protocol + bridge.
//!
//! Plugins are out-of-process gRPC sidecars (one process per connector,
//! typically a K8s sidecar pod). At worker boot, [`PluginRegistry`] dials each
//! configured endpoint, calls `GetManifest`, and synthesises one
//! [`elena_tools::Tool`] per advertised action — registered straight into the
//! shared [`elena_tools::ToolRegistry`] so the orchestrator and LLM see them
//! as ordinary tools.
//!
//! ```text
//!   LLM                          Elena worker                       Plugin sidecar
//!    │                                │                                  │
//!    │ tool_use { name: "echo_reverse", input } ──────────────────────▶  │
//!    │                                │ Execute(PluginRequest)           │
//!    │                                │ ◀──── ProgressUpdate ── ──── ── │
//!    │                                │ ◀──── ProgressUpdate ── ──── ── │
//!    │                                │ ◀──── FinalResult ── ──── ──── │
//!    │ ◀── tool_result               │                                  │
//! ```

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]

pub mod action_tool;
pub mod client;
pub mod config;
pub mod embedded;
pub mod error;
pub mod health;
pub mod id;
pub mod manifest;
pub mod proto;
pub mod registry;

pub use action_tool::PluginActionTool;
pub use client::PluginClient;
pub use config::PluginsConfig;
pub use embedded::{EmbeddedExecutor, PluginBackend};
pub use error::PluginError;
pub use health::{HealthMonitor, HealthState};
pub use id::{PluginId, PluginIdError};
pub use manifest::{ActionDefinition, PluginManifest};
pub use registry::PluginRegistry;
