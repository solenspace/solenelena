//! Admin API for Elena. Mounted by the gateway under `/admin/v1`.
//!
//! v1.0 scope — two concerns:
//!
//! 1. **Tenants** — create, read, budget-update via the tenant store.
//! 2. **Deep health** — synchronous pings of every dependency (Postgres,
//!    Redis, NATS, configured plugins if the gateway is aware of them).
//!
//! Operators restrict the route via network policy or ingress — the admin
//! surface is deliberately small and not exposed on the public
//! load-balancer in production. A JWT audience split (`elena-admin`) is
//! planned for v1.0.x; for v1.0 the expectation is "internal network only
//! + short-lived operator sessions."

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::unnecessary_literal_bound,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::missing_fields_in_debug,
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::used_underscore_binding,
    clippy::too_many_lines,
    clippy::unused_async
)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod apps;
pub mod audit;
pub mod auth;
pub mod budget;
pub mod health;
pub mod plan_assignments;
pub mod plans;
pub mod plugins;
pub mod router;
pub mod state;
pub mod tenant_credentials;
pub mod tenants;
pub mod threads;
pub mod workspaces;

pub use router::admin_router;
pub use state::AdminState;
