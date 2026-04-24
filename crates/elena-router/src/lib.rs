//! Heuristic model-tier routing + reactive cascade escalation.
//!
//! Pure-Rust, deterministic, table-testable. No ML model; the
//! architecture's target ONNX classifier will slot in later when production
//! training data exists.
//!
//! Public entry points:
//!
//! - [`ModelRouter::route`] — pick a [`ModelTier`](elena_types::ModelTier)
//!   from a [`RoutingContext`].
//! - [`ModelRouter::cascade_check`] — after the stream completes, decide
//!   whether to re-issue the turn at a higher tier.
//! - [`ModelRouter::resolve`] — translate a tier to the operator-configured
//!   [`ModelId`](elena_types::ModelId).

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod cascade;
pub mod failover;
pub mod router;
pub mod rules;

pub use cascade::{CascadeDecision, CascadeInputs, cascade_check};
pub use failover::{CircuitBreaker, CircuitState};
pub use router::ModelRouter;
pub use rules::{RoutingContext, route};
