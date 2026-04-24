//! Streaming LLM client for Elena.
//!
//! Scope: Anthropic Messages API with API-key auth, SSE streaming,
//! retry with exponential backoff, and prompt-cache marker placement.
//! Every public async fn takes a [`tokio_util::sync::CancellationToken`]
//! for cooperative cancellation end-to-end.
//!
//! The public API surfaces via re-exports below; internal modules
//! (`events`, `assembler`, `sse`) are implementation detail and subject to
//! change.

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod anthropic;
pub mod assembler;
pub mod cache;
pub(crate) mod events;
pub mod multiplexer;
pub mod openai_compat;
pub mod provider;
pub mod request;
pub mod retry;
pub mod sse;
pub mod wire;

pub use anthropic::{AnthropicAuth, AnthropicClient};
pub use cache::{CacheAllowlist, CachePolicy};
pub use multiplexer::LlmMultiplexer;
pub use openai_compat::{OpenAiCompatClient, OpenAiCompatConfig};
pub use provider::LlmClient;
pub use request::{LlmRequest, RequestOptions, Thinking, ToolChoice, ToolSchema};
pub use retry::{RetryDecision, RetryPolicy};
