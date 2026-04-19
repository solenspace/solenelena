//! [`LlmMultiplexer`] — picks an underlying [`LlmClient`] per-request.
//!
//! A single `Arc<dyn LlmClient>` held on `LoopDeps.llm` lets the orchestrator
//! dispatch across any provider simply by setting
//! [`LlmRequest::provider`](crate::LlmRequest::provider). The multiplexer
//! is itself an [`LlmClient`], so it nests without any special casing.

use std::collections::HashMap;
use std::sync::Arc;

use elena_types::{ElenaError, LlmApiError, StreamEvent, Terminal};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio_util::sync::CancellationToken;

use crate::{cache::CachePolicy, provider::LlmClient, request::LlmRequest};

/// Provider-name → client router. Cheap to clone (inner state is an `Arc`).
#[derive(Clone)]
pub struct LlmMultiplexer {
    inner: Arc<MuxInner>,
}

struct MuxInner {
    clients: HashMap<String, Arc<dyn LlmClient>>,
    default: String,
}

impl std::fmt::Debug for LlmMultiplexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&String> = self.inner.clients.keys().collect();
        f.debug_struct("LlmMultiplexer")
            .field("providers", &keys)
            .field("default", &self.inner.default)
            .finish()
    }
}

impl LlmMultiplexer {
    /// Build an empty multiplexer that falls back to `default` when
    /// [`LlmRequest::provider`] isn't registered.
    #[must_use]
    pub fn new(default: impl Into<String>) -> Self {
        Self { inner: Arc::new(MuxInner { clients: HashMap::new(), default: default.into() }) }
    }

    /// Add a provider. The key must match exactly what
    /// [`LlmRequest::provider`] will carry.
    ///
    /// Typical: `"anthropic"`, `"openrouter"`, `"groq"`.
    pub fn register(&mut self, name: impl Into<String>, client: Arc<dyn LlmClient>) {
        let mut new_map = self.inner.clients.clone();
        new_map.insert(name.into(), client);
        self.inner = Arc::new(MuxInner { clients: new_map, default: self.inner.default.clone() });
    }

    /// Convenience: build with a handful of providers up-front.
    #[must_use]
    pub fn with_providers(
        default: impl Into<String>,
        providers: impl IntoIterator<Item = (String, Arc<dyn LlmClient>)>,
    ) -> Self {
        let mut m = Self::new(default);
        for (k, v) in providers {
            m.register(k, v);
        }
        m
    }

    /// Provider keys known to the multiplexer.
    #[must_use]
    pub fn provider_names(&self) -> Vec<String> {
        self.inner.clients.keys().cloned().collect()
    }

    /// Look up a client by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn LlmClient>> {
        self.inner.clients.get(name).cloned()
    }
}

impl LlmClient for LlmMultiplexer {
    fn provider(&self) -> &str {
        &self.inner.default
    }

    fn stream(
        &self,
        req: LlmRequest,
        policy: CachePolicy,
        cancel: CancellationToken,
    ) -> BoxStream<'static, StreamEvent> {
        let target = if self.inner.clients.contains_key(&req.provider) {
            &req.provider
        } else {
            &self.inner.default
        };
        match self.inner.clients.get(target) {
            Some(client) => client.stream(req, policy, cancel),
            None => error_stream(format!(
                "no LLM provider registered for '{}' (and default '{}' also unknown)",
                req.provider, self.inner.default
            )),
        }
    }
}

fn error_stream(message: String) -> BoxStream<'static, StreamEvent> {
    let err = LlmApiError::Unknown { message };
    let kind = err.kind();
    stream::iter(vec![
        StreamEvent::Error(ElenaError::LlmApi(err)),
        StreamEvent::Done(Terminal::ModelError { classified: kind }),
    ])
    .boxed()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use elena_types::{
        BudgetLimits, ModelId, PermissionSet, SessionId, StreamEvent, TenantContext, TenantId,
        TenantTier, ThreadId, UserId, WorkspaceId,
    };
    use futures::StreamExt;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{CachePolicy, LlmRequest, RequestOptions};

    #[derive(Debug)]
    struct EchoClient {
        name: String,
    }

    impl EchoClient {
        fn arc(name: &str) -> Arc<dyn LlmClient> {
            Arc::new(Self { name: name.to_owned() })
        }
    }

    impl LlmClient for EchoClient {
        fn provider(&self) -> &str {
            &self.name
        }
        fn stream(
            &self,
            _req: LlmRequest,
            _policy: CachePolicy,
            _cancel: CancellationToken,
        ) -> BoxStream<'static, StreamEvent> {
            let name = self.name.clone();
            futures::stream::iter(vec![StreamEvent::TextDelta { delta: format!("from:{name}") }])
                .boxed()
        }
    }

    fn tenant() -> TenantContext {
        TenantContext {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            permissions: PermissionSet::default(),
            budget: BudgetLimits::default(),
            tier: TenantTier::Pro,
            metadata: std::collections::HashMap::new(),
        }
    }

    fn req_for(provider: &str) -> LlmRequest {
        LlmRequest {
            provider: provider.to_owned(),
            model: ModelId::new("test"),
            tenant: tenant(),
            messages: Vec::new(),
            system: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 16,
            thinking: None,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            options: RequestOptions::default(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatches_to_named_provider() {
        let mut mux = LlmMultiplexer::new("anthropic");
        mux.register("anthropic", EchoClient::arc("anthropic"));
        mux.register("groq", EchoClient::arc("groq"));

        let policy = CachePolicy::new(TenantTier::Pro, crate::CacheAllowlist::default());
        let events: Vec<StreamEvent> =
            mux.stream(req_for("groq"), policy, CancellationToken::new()).collect().await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { delta } if delta == "from:groq"))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn falls_back_to_default_when_unknown_provider() {
        let mut mux = LlmMultiplexer::new("anthropic");
        mux.register("anthropic", EchoClient::arc("anthropic"));

        let policy = CachePolicy::new(TenantTier::Pro, crate::CacheAllowlist::default());
        let events: Vec<StreamEvent> =
            mux.stream(req_for("nonexistent"), policy, CancellationToken::new()).collect().await;
        assert!(
            events.iter().any(
                |e| matches!(e, StreamEvent::TextDelta { delta } if delta == "from:anthropic")
            )
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_multiplexer_errors_cleanly() {
        let mux = LlmMultiplexer::new("anthropic");
        let policy = CachePolicy::new(TenantTier::Pro, crate::CacheAllowlist::default());
        let events: Vec<StreamEvent> =
            mux.stream(req_for("anthropic"), policy, CancellationToken::new()).collect().await;
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Error(_))));
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done(Terminal::ModelError { .. }))));
    }
}
