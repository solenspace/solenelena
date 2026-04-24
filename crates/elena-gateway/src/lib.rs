//! HTTP + WebSocket gateway for Elena.
//!
//! Serves:
//! - `GET /health` + `GET /version` (unauthed)
//! - `POST /v1/threads` (JWT-auth) to create a new thread
//! - `GET /v1/threads/:id/stream` (JWT-auth) WebSocket upgrade — client
//!   sends `{"action":"send_message",...}`, server streams every
//!   [`StreamEvent`](elena_types::StreamEvent) as a text frame.
//!
//! Auth is JWT for client-facing routes; mTLS between services is handled
//! separately by the worker / connector wiring.
//! NATS plumbing: `elena.work.incoming` (`JetStream`, work queue) and
//! `elena.thread.{id}.events` (core pub-sub, event fanout).

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod app;
pub mod auth;
pub mod config;
pub mod error;
pub mod fanout;
pub mod nats;
pub mod routes;

pub use app::{GatewayState, build_router};
pub use auth::{AuthedTenant, ElenaJwtClaims, JwtValidator};
pub use config::{CorsConfig, GatewayConfig, JwtAlgorithm, JwtConfig};
pub use error::GatewayError;
