//! HTTP + WebSocket gateway for Elena.
//!
//! Phase 5. Serves:
//! - `GET /health` + `GET /version` (unauthed)
//! - `POST /v1/threads` (JWT-auth) to create a new thread
//! - `GET /v1/threads/:id/stream` (JWT-auth) WebSocket upgrade — client
//!   sends `{"action":"send_message",...}`, server streams every
//!   [`StreamEvent`](elena_types::StreamEvent) as a text frame.
//!
//! Auth is JWT-only in Phase 5 (mTLS-between-services lands in Phase 7).
//! NATS plumbing: `elena.work.incoming` (`JetStream`, work queue) and
//! `elena.thread.{id}.events` (core pub-sub, event fanout).

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod app;
pub mod auth;
pub mod config;
pub mod error;
pub mod nats;
pub mod routes;

pub use app::{GatewayState, build_router};
pub use auth::{AuthedTenant, ElenaJwtClaims, JwtValidator};
pub use config::{GatewayConfig, JwtAlgorithm, JwtConfig};
pub use error::GatewayError;
