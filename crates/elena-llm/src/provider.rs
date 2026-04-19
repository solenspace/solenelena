//! [`LlmClient`] — provider-agnostic streaming LLM interface.
//!
//! The [`AnthropicClient`](crate::AnthropicClient) and
//! [`OpenAiCompatClient`](crate::OpenAiCompatClient) both implement this
//! trait, and a [`LlmMultiplexer`](crate::LlmMultiplexer) dispatches by
//! [`LlmRequest::provider`] so callers can hold a single
//! `Arc<dyn LlmClient>` and transparently cross providers.
//!
//! The trait is object-safe: [`stream`](LlmClient::stream) returns a boxed
//! stream (matching the concrete clients' signatures), and there are no
//! associated types.

use elena_types::StreamEvent;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::{cache::CachePolicy, request::LlmRequest};

/// A streaming LLM client.
pub trait LlmClient: Send + Sync + std::fmt::Debug + 'static {
    /// Short, stable identifier for this provider.
    ///
    /// Used by [`LlmMultiplexer`](crate::LlmMultiplexer) to pick the right
    /// implementation for an incoming [`LlmRequest::provider`].
    fn provider(&self) -> &str;

    /// Stream a completion. Returns a boxed stream of [`StreamEvent`].
    ///
    /// Cancellation: firing `cancel` or dropping the returned stream
    /// terminates the in-flight request.
    fn stream(
        &self,
        req: LlmRequest,
        policy: CachePolicy,
        cancel: CancellationToken,
    ) -> BoxStream<'static, StreamEvent>;
}
