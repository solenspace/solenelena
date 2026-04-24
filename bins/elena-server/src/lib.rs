//! Library surface for `elena-server`.
//!
//! Q5 — pre-Q5 every smoke binary reconstructed `LoopDeps` field by
//! field, which made changes to `LoopDeps`'s shape an O(N-smokes)
//! coordination problem. The [`bootstrap::build_loop_deps`] helper
//! consolidates the assembly so future fields land in exactly one
//! place.

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![warn(missing_docs)]

pub mod bootstrap;

pub use bootstrap::{BuildLoopDepsOptions, build_loop_deps};
