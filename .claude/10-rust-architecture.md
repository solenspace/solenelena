# Elena Backend Architecture — Complete Specification

> This is the definitive architecture for Elena, a distributed agentic loop backend serving multiple applications (Solen, Hannlys, and future apps) at 500K-1M+ concurrent sessions.

---

## Decisions Summary

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Cloud | AWS (EKS) | K8s auto-scaling, managed services, cost optimization with Spot/Graviton |
| Message broker | NATS JetStream | Sub-millisecond latency, at-least-once delivery, built-in persistence |
| Plugins | gRPC sidecars | Full isolation, independent deployment, crash can't take down Elena |
| Embeddings | Local ONNX (e5-small-v2) | <5ms per embed, no network hop, ~200MB RAM per worker |
| App→Elena auth | mTLS + JWT hybrid | mTLS for service identity, JWT for tenant context extraction — zero-latency after handshake |
| Data security | RDS encryption at rest + in-transit, ElastiCache encryption, VPC-isolated | SOC2-ready from day one |
| Crash recovery | Commit after each tool result, retry current LLM turn | Never re-executes tools, user sees brief pause then continuation |
| Database | PostgreSQL (RDS) + pgvector | Durable state, embedding similarity search co-located with data |
| Hot state | Redis (ElastiCache) | Session state, loop checkpoints, rate limiting, thread claiming |
| Model routing | ML classifier (ONNX) | <5ms task classification, 50-98% cost reduction via cascading |

---

## System Topology

```
                    ┌─────────────────────────────────────────────────┐
                    │                  AWS EKS Cluster                 │
                    │                                                  │
   Solen App ──┐   │  ┌──────────┐    NATS     ┌──────────────┐     │
               │   │  │  Gateway  │◄──────────►│    Workers    │     │
   Hannlys ───┤   │  │  (N pods) │  JetStream  │   (M pods)   │     │
               │   │  │  WebSocket│             │  Agentic Loop│     │
   Future App─┘   │  │  + HTTP   │             │  State Machine│    │
       ▲          │  └─────┬─────┘             └──────┬───────┘     │
       │ mTLS+JWT │        │                          │              │
       │          │        │                    ┌─────┴─────┐        │
       │          │        │                    │           │        │
       │          │  ┌─────▼─────┐    ┌────────▼──┐  ┌────▼─────┐  │
       │          │  │  Redis     │    │ Postgres  │  │ LLM APIs │  │
       │          │  │ ElastiCache│    │   RDS     │  │ Anthropic│  │
       │          │  │ (hot state)│    │(persist)  │  │ +fallback│  │
       │          │  └───────────┘    └───────────┘  └──────────┘  │
       │          │                                                  │
       │          │  ┌───────────────────────────────────────────┐  │
       │          │  │         Plugin Sidecars (gRPC)             │  │
       │          │  │  ┌─────────┐ ┌───────┐ ┌──────────┐      │  │
       │          │  │  │ Shopify │ │ Gmail │ │ Calendar │ ...  │  │
       │          │  │  └─────────┘ └───────┘ └──────────┘      │  │
       │          │  └───────────────────────────────────────────┘  │
       │          └─────────────────────────────────────────────────┘
       │
       └── Clients receive token-by-token SSE/WebSocket streams
```

---

## Crate Layout

```
elena/
├── Cargo.toml                              # Workspace root
├── crates/
│   ├── elena-types/                        # 1. Foundation types
│   ├── elena-store/                        # 2. Persistence (Postgres + Redis)
│   ├── elena-llm/                          # 3. LLM client + streaming
│   ├── elena-router/                       # 4. ML model routing
│   ├── elena-context/                      # 5. Context management + embeddings
│   ├── elena-memory/                       # 6. Episodic cross-session memory
│   ├── elena-tools/                        # 7. Tool system
│   ├── elena-plugins/                      # 8. Plugin connector interface
│   ├── elena-core/                         # 9. THE AGENTIC LOOP
│   ├── elena-gateway/                      # 10. API gateway binary
│   ├── elena-worker/                       # 11. Worker binary
│   └── elena-config/                       # 12. Configuration + tenant settings
```

Two binaries: `elena-gateway` (accepts connections, routes) and `elena-worker` (runs agentic loops). Connected by NATS JetStream.

---

## Crate 1: `elena-types`

**Purpose:** Zero-dependency foundation. Every other crate depends on this.

```rust
// Branded IDs — prevent mixing session/user/workspace IDs
pub struct SessionId(Ulid);       // ULID for time-sortable uniqueness
pub struct TenantId(Ulid);        // App identity (Solen, Hannlys, etc.)
pub struct UserId(Ulid);
pub struct WorkspaceId(Ulid);
pub struct ThreadId(Ulid);        // Chat thread / generation job
pub struct ToolCallId(Ulid);
pub struct PluginId(String);

// Core message types
pub enum Role { User, Assistant, System, Tool }

pub struct Message {
    pub id: Ulid,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub created_at: DateTime<Utc>,
    pub token_count: Option<u32>,    // Cached after first count
    pub embedding: Option<Vec<f32>>, // Cached embedding vector
}

pub enum ContentBlock {
    Text(String),
    ToolUse { id: ToolCallId, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: ToolCallId, content: ToolResultContent },
    Image { media_type: String, data: Bytes },
}

// Stream events — what flows from worker → gateway → client
pub enum StreamEvent {
    TextDelta(String),
    ToolStart { id: ToolCallId, name: String },
    ToolProgress { id: ToolCallId, progress: String },
    ToolResult { id: ToolCallId, result: ToolResultSummary },
    StateChange(LoopPhase),
    Error(ElenaError),
    Done(Terminal),
}

// Terminal conditions
pub enum Terminal {
    Completed,
    Aborted,
    BudgetExhausted,
    MaxTurnsReached,
    Error(ElenaError),
}

// Error hierarchy
#[derive(thiserror::Error)]
pub enum ElenaError {
    #[error("LLM API error: {0}")]
    LlmApi(#[from] LlmApiError),
    #[error("Tool execution failed: {0}")]
    ToolExecution(#[from] ToolError),
    #[error("Context overflow: {tokens} tokens exceeds {limit}")]
    ContextOverflow { tokens: u64, limit: u64 },
    #[error("Budget exceeded for tenant {tenant_id}")]
    BudgetExceeded { tenant_id: TenantId },
    #[error("Plugin error: {plugin_id}: {message}")]
    Plugin { plugin_id: PluginId, message: String },
    #[error("Permission denied: {reason}")]
    PermissionDenied { reason: String },
}

// Permissions
pub enum Permission {
    Allow,
    Deny { reason: String },
    AskUser { prompt: String },
}

pub struct TenantContext {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub workspace_id: WorkspaceId,
    pub thread_id: ThreadId,
    pub permissions: PermissionSet,
    pub budget: BudgetLimits,
    pub metadata: HashMap<String, serde_json::Value>, // App-specific data
}
```

**Key crates:** `serde`, `ulid`, `chrono`, `bytes`, `thiserror`

---

## Crate 2: `elena-store`

**Purpose:** All persistence. Postgres for durable state, Redis for hot state.

```rust
// Postgres tables (via sqlx)
pub struct ThreadStore {
    pool: PgPool,
}

impl ThreadStore {
    // Messages stored with embeddings for retrieval
    async fn append_message(&self, thread_id: ThreadId, msg: &Message) -> Result<()>;
    async fn get_messages(&self, thread_id: ThreadId, limit: u32) -> Result<Vec<Message>>;
    async fn get_messages_by_similarity(
        &self, thread_id: ThreadId, query_embedding: &[f32], top_k: u32
    ) -> Result<Vec<Message>>;  // pgvector similarity search
    
    // Thread metadata
    async fn create_thread(&self, ctx: &TenantContext) -> Result<ThreadId>;
    async fn get_thread_state(&self, thread_id: ThreadId) -> Result<ThreadState>;
}

pub struct TenantStore {
    pool: PgPool,
}

impl TenantStore {
    async fn get_budget(&self, tenant_id: TenantId) -> Result<BudgetState>;
    async fn record_usage(&self, tenant_id: TenantId, usage: TokenUsage) -> Result<()>;
    async fn get_permissions(&self, tenant_id: TenantId) -> Result<PermissionSet>;
}

// Redis for hot state (via fred or redis-rs)
pub struct SessionCache {
    redis: RedisPool,
}

impl SessionCache {
    // Loop state — serialized state machine, TTL 1 hour
    async fn save_loop_state(&self, thread_id: ThreadId, state: &LoopState) -> Result<()>;
    async fn load_loop_state(&self, thread_id: ThreadId) -> Result<Option<LoopState>>;
    
    // Active stream tracking — which worker owns which thread
    async fn claim_thread(&self, thread_id: ThreadId, worker_id: WorkerId) -> Result<bool>;
    async fn release_thread(&self, thread_id: ThreadId) -> Result<()>;
    
    // Rate limiting — per tenant, sliding window
    async fn check_rate_limit(&self, tenant_id: TenantId) -> Result<RateLimitDecision>;
}
```

**Key crates:** `sqlx` (Postgres + pgvector), `fred` (Redis), `deadpool` (connection pooling)

**Postgres extensions:** `pgvector` for embedding similarity search. Keeps vector search co-located with data — no separate vector DB needed at this scale.

---

## Crate 3: `elena-llm`

**Purpose:** Multi-provider LLM client with SSE streaming.

```rust
// Provider abstraction
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<SseEvent>> + Send>>>;
    
    fn model_info(&self, model: &str) -> ModelInfo;
}

// Concrete providers
pub struct AnthropicProvider { client: reqwest::Client, api_key: SecretString }
pub struct BedrockProvider { client: aws_sdk_bedrockruntime::Client }

// Request construction
pub struct LlmRequest {
    pub model: ModelId,
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub metadata: RequestMetadata,
}

// SSE parsing — zero-copy where possible
pub async fn parse_sse_stream(
    response: reqwest::Response,
    tx: mpsc::Sender<SseEvent>,
) -> Result<LlmUsage> {
    let mut stream = response.bytes_stream();
    let mut buf = BytesMut::with_capacity(8192);
    
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(event) = extract_sse_frame(&mut buf) {
            tx.send(event).await?;
        }
    }
    // Return final usage stats
}

// Retry with exponential backoff + jitter
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub retry_on: fn(&ElenaError) -> RetryDecision,
}

pub enum RetryDecision {
    Retry { delay: Duration },
    RetryWithNewClient,  // 401/403 — refresh credentials
    Fail,
}
```

**Key crates:** `reqwest` (with `rustls-tls`), `eventsource-stream`, `secrecy` (for API keys), `backon` (retry)

---

## Crate 4: `elena-router`

**Purpose:** ML-based model selection. Saves money and improves speed.

```rust
// Model tiers — from cheapest to most capable
pub enum ModelTier {
    Fast,      // Haiku-class: simple lookups, classification, formatting
    Standard,  // Sonnet-class: most tasks, code generation, analysis
    Premium,   // Opus-class: complex reasoning, multi-step planning
}

// The router classifies incoming requests
pub struct ModelRouter {
    classifier: OrtSession,  // ONNX Runtime — small classifier model
    cascade_config: CascadeConfig,
}

impl ModelRouter {
    // Classify task complexity → select initial model tier
    pub fn route(&self, request: &RoutingContext) -> ModelTier {
        let features = extract_features(request);
        // Features: message length, tool count, conversation depth,
        // keyword signals ("analyze", "simple", "complex"), 
        // recent tool types, error recovery count
        let score = self.classifier.run(&features);
        match score {
            s if s < 0.3 => ModelTier::Fast,
            s if s < 0.7 => ModelTier::Standard,
            _ => ModelTier::Premium,
        }
    }
    
    // Cascade: if Fast model's response has low confidence, escalate
    pub async fn cascade_check(
        &self, 
        response: &LlmResponse, 
        tier: ModelTier,
    ) -> CascadeDecision {
        // Check: did the model refuse? Did it ask for clarification 
        // when the task was clear? Is the response suspiciously short?
        // Did it hallucinate a tool that doesn't exist?
        if response.confidence_signals.needs_escalation() && tier < ModelTier::Premium {
            CascadeDecision::Escalate(tier.next())
        } else {
            CascadeDecision::Accept
        }
    }
}

// Routing context — what the router sees
pub struct RoutingContext {
    pub user_message: String,
    pub conversation_depth: u32,
    pub tools_available: Vec<String>,
    pub recent_tool_calls: Vec<String>,
    pub tenant_tier: TenantTier,    // Some tenants always get Premium
    pub error_recovery_count: u32,  // Escalate if we've been retrying
}
```

**Key crates:** `ort` (ONNX Runtime), `ndarray`

**Training the classifier:** Collect (request, model_that_succeeded) pairs from production. Train offline with Python/sklearn. Export to ONNX. Deploy as model file alongside Elena. Retrain monthly.

---

## Crate 5: `elena-context`

**Purpose:** Intelligent context window management using embeddings.

```rust
pub struct ContextManager {
    embedder: OrtSession,       // e5-small-v2 ONNX model
    tokenizer: Tokenizer,       // tiktoken-compatible BPE
}

impl ContextManager {
    // Build context window for LLM call
    pub async fn build_context(
        &self,
        thread_id: ThreadId,
        current_query: &str,
        model_context_limit: u32,
        store: &ThreadStore,
    ) -> Result<Vec<Message>> {
        let budget = model_context_limit - RESERVED_FOR_OUTPUT;
        
        // Always include: system prompt + last N messages (recency window)
        let recent = store.get_messages(thread_id, RECENCY_WINDOW).await?;
        let mut used_tokens = count_tokens(&recent);
        
        if used_tokens < budget {
            // Room for retrieval — find semantically relevant older messages
            let query_embedding = self.embed(current_query)?;
            let relevant = store.get_messages_by_similarity(
                thread_id, &query_embedding, TOP_K_RETRIEVE
            ).await?;
            
            // Add relevant messages until budget exhausted
            for msg in relevant {
                let msg_tokens = msg.token_count.unwrap_or(estimate_tokens(&msg));
                if used_tokens + msg_tokens > budget { break; }
                used_tokens += msg_tokens;
                // Insert in chronological order
            }
        }
        
        Ok(assembled_context)
    }
    
    // Embed text — runs locally, <5ms
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let tokens = self.tokenizer.encode(text)?;
        let output = self.embedder.run(tokens)?;
        Ok(normalize(output))
    }
}
```

**Why this replaces Claude Code's 4 compaction layers:**
- Snip compact → unnecessary, retrieval naturally skips old irrelevant messages
- Microcompact → unnecessary, we store full messages and only retrieve what fits
- Context collapse → unnecessary, embedding similarity handles this
- Auto-compact → replaced by on-demand LLM summarization only when sessions are extremely long (200+ turns)

---

## Crate 6: `elena-memory`

**Purpose:** Cross-session episodic memory. Elena learns from past interactions.

```rust
// Memory is stored per-workspace (shared across threads in a workspace)
pub struct EpisodicMemory {
    store: PgPool,  // Uses pgvector for similarity search
}

pub struct Episode {
    pub id: Ulid,
    pub workspace_id: WorkspaceId,
    pub task_summary: String,        // What was asked
    pub actions_taken: Vec<String>,  // Tools used, in order
    pub outcome: Outcome,            // Success/failure + details
    pub embedding: Vec<f32>,         // Embedding of task_summary
    pub created_at: DateTime<Utc>,
}

impl EpisodicMemory {
    // After a successful thread completion, extract and store episode
    pub async fn record_episode(
        &self,
        thread: &CompletedThread,
        embedder: &ContextManager,
    ) -> Result<()> {
        let summary = extract_task_summary(thread);
        let embedding = embedder.embed(&summary)?;
        // Store in Postgres with pgvector
    }
    
    // Before starting a new thread, retrieve relevant past episodes
    pub async fn recall(
        &self,
        workspace_id: WorkspaceId,
        current_task: &str,
        embedder: &ContextManager,
        top_k: u32,
    ) -> Result<Vec<Episode>> {
        let embedding = embedder.embed(current_task)?;
        // pgvector cosine similarity search, filtered by workspace
    }
}
```

Elena gets smarter per workspace over time. If a Solen user regularly asks "send Shopify orders to the team," Elena recalls that the Shopify connector + Gmail connector were used successfully before, and pre-routes to the right tools and model tier.

---

## Crate 7: `elena-tools`

**Purpose:** Tool trait + built-in tools + orchestration.

```rust
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    
    fn is_read_only(&self) -> bool { false }
    fn is_concurrent_safe(&self) -> bool { self.is_read_only() }
    
    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
        progress: mpsc::Sender<ToolProgress>,  // Real-time progress
    ) -> Result<ToolResult>;
    
    fn check_permission(
        &self,
        input: &serde_json::Value,
        permissions: &PermissionSet,
    ) -> Permission;
}

// Tool context — everything a tool needs
pub struct ToolContext {
    pub tenant: TenantContext,
    pub thread_id: ThreadId,
    pub working_dir: PathBuf,        // For file tools (Hannlys seed building)
    pub plugin_registry: Arc<PluginRegistry>,  // Access to connectors
    pub store: Arc<dyn Store>,       // For tools that need persistence
}

// Orchestration — parallel read-only, serial write
pub async fn execute_tool_calls(
    calls: Vec<ToolCall>,
    registry: &ToolRegistry,
    ctx: &ToolContext,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<StreamEvent>,
) -> Vec<ToolResult> {
    let (concurrent, serial) = partition(&calls, registry);
    let mut results = Vec::with_capacity(calls.len());
    
    // Run read-only tools concurrently (with semaphore cap)
    if !concurrent.is_empty() {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_TOOLS));
        let mut set = JoinSet::new();
        for call in concurrent {
            let permit = semaphore.clone().acquire_owned().await?;
            let tool = registry.get(&call.name)?;
            let ctx = ctx.clone();
            let cancel = cancel.clone();
            let tx = event_tx.clone();
            set.spawn(async move {
                let _permit = permit;
                let result = tool.execute(call.input, &ctx, cancel, tx).await;
                (call.id, result)
            });
        }
        while let Some(res) = set.join_next().await {
            let (id, result) = res?;
            results.push((id, result));
        }
    }
    
    // Run write tools serially
    for call in serial {
        let tool = registry.get(&call.name)?;
        let result = tool.execute(call.input, ctx, cancel.clone(), event_tx.clone()).await;
        results.push((call.id, result));
    }
    
    results
}
```

### Built-in Tools

| Tool | Purpose | Used by |
|------|---------|---------|
| `PluginCall` | Call a gRPC plugin connector | Solen (Shopify, Gmail, etc.) |
| `FileRead` | Read files from workspace sandbox | Hannlys (seed templates) |
| `FileWrite` | Write files to workspace sandbox | Hannlys (generated output) |
| `FileEdit` | Edit files in workspace sandbox | Hannlys (personalization) |
| `WebFetch` | Fetch URL content | Both |
| `Search` | Search workspace files/history | Both |
| `SubAgent` | Spawn a sub-loop for complex subtasks | Both |
| `TemplateRender` | Render a template with variables | Hannlys (seed building) |

---

## Crate 8: `elena-plugins`

**Purpose:** gRPC connector interface. Plugins are separate processes.

```protobuf
// elena_plugin.proto — the contract every connector implements
service ElenaPlugin {
    rpc GetManifest(Empty) returns (PluginManifest);
    rpc Execute(PluginRequest) returns (stream PluginResponse);
    rpc Health(Empty) returns (HealthResponse);
}

message PluginManifest {
    string id = 1;
    string name = 2;
    string version = 3;
    repeated ActionDefinition actions = 4;
    repeated string required_credentials = 5;
}

message ActionDefinition {
    string name = 1;           // e.g., "get_orders", "send_email"
    string description = 2;    // For the LLM to understand
    string input_schema = 3;   // JSON Schema
    string output_schema = 4;  // JSON Schema
}
```

```rust
pub struct PluginRegistry {
    plugins: DashMap<PluginId, PluginClient>,
    manifests: DashMap<PluginId, PluginManifest>,
}

impl PluginRegistry {
    pub async fn register(&self, endpoint: Uri) -> Result<PluginId> {
        let client = PluginClient::connect(endpoint).await?;
        let manifest = client.get_manifest().await?;
        let id = PluginId(manifest.id.clone());
        self.plugins.insert(id.clone(), client);
        self.manifests.insert(id.clone(), manifest);
        Ok(id)
    }
    
    // Convert plugin actions to Tool schemas for LLM
    pub fn as_tool_schemas(&self) -> Vec<ToolSchema> {
        self.manifests.iter().flat_map(|entry| {
            entry.value().actions.iter().map(|action| ToolSchema {
                name: format!("{}_{}", entry.key().0, action.name),
                description: action.description.clone(),
                input_schema: serde_json::from_str(&action.input_schema).unwrap(),
            })
        }).collect()
    }
}
```

**How a Shopify connector works:**
1. `shopify-connector` pod runs in K8s as a sidecar
2. On startup, it registers with Elena via gRPC `GetManifest`
3. Elena adds `shopify_get_orders`, `shopify_create_order`, etc. as available tools
4. When the LLM calls `shopify_get_orders`, Elena routes to the sidecar via gRPC
5. The sidecar handles OAuth, API calls, pagination, and returns structured data
6. If the sidecar crashes, Elena gets a gRPC error and reports it gracefully

---

## Crate 9: `elena-core` — THE AGENTIC LOOP

The heart of Elena. A clean state machine replacing Claude Code's 1,730-line `query.ts`.

### Loop State (fully serializable, stored in Redis between phases)

```rust
#[derive(Serialize, Deserialize)]
pub struct LoopState {
    pub phase: LoopPhase,
    pub thread_id: ThreadId,
    pub tenant: TenantContext,
    pub turn_count: u32,
    pub total_tokens_used: u64,
    pub model_tier: ModelTier,
    pub committed_messages: Vec<MessageId>,
    pub pending_tool_calls: Vec<ToolCall>,
    pub pending_tool_results: Vec<ToolResult>,
    pub recovery: RecoveryState,
}

#[derive(Serialize, Deserialize)]
pub enum LoopPhase {
    Received,
    BuildingContext,
    Routing,
    Streaming { model: ModelId },
    ExecutingTools { remaining: Vec<ToolCall> },
    PostProcessing,
    Completed,
    Failed(ElenaError),
}

#[derive(Serialize, Deserialize)]
pub struct RecoveryState {
    pub consecutive_errors: u32,
    pub model_escalations: u32,
    pub context_rebuilds: u32,
    pub max_output_retries: u32,
}
```

### State Machine (each call processes one phase transition)

```rust
pub async fn step(
    state: &mut LoopState,
    deps: &LoopDeps,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<StreamEvent>,
) -> Result<StepResult> {
    match &state.phase {
        LoopPhase::Received => {
            // Recall episodic memory for this workspace
            let episodes = deps.memory.recall(
                state.tenant.workspace_id,
                &last_user_message(state, deps).await?,
                &deps.context_manager,
                3,
            ).await?;
            
            if !episodes.is_empty() {
                inject_episode_context(state, episodes);
            }
            
            state.phase = LoopPhase::BuildingContext;
            Ok(StepResult::Continue)
        }
        
        LoopPhase::BuildingContext => {
            let messages = deps.context_manager.build_context(
                state.thread_id,
                &current_query_text(state),
                deps.llm.model_info(&state.model_tier.to_model_id()).context_window,
                &deps.store.threads,
            ).await?;
            
            state.phase = LoopPhase::Routing;
            Ok(StepResult::Continue)
        }
        
        LoopPhase::Routing => {
            let tier = deps.router.route(&RoutingContext {
                user_message: current_query_text(state),
                conversation_depth: state.turn_count,
                tools_available: deps.tools.available_tool_names(),
                recent_tool_calls: recent_tools(state),
                tenant_tier: state.tenant.budget.tier,
                error_recovery_count: state.recovery.consecutive_errors,
            });
            
            state.model_tier = tier;
            let model = tier.to_model_id();
            state.phase = LoopPhase::Streaming { model };
            event_tx.send(StreamEvent::StateChange(state.phase.clone())).await?;
            Ok(StepResult::Continue)
        }
        
        LoopPhase::Streaming { model } => {
            let request = build_llm_request(state, deps, model).await?;
            
            let (stream_tx, mut stream_rx) = mpsc::channel(64);
            let stream_handle = tokio::spawn({
                let provider = deps.llm.provider_for(model);
                let cancel = cancel.clone();
                async move { provider.stream(request, cancel).await }
            });
            
            let mut assistant_content = Vec::new();
            let mut tool_calls = Vec::new();
            
            // Forward SSE events to client as they arrive
            while let Some(event) = stream_rx.recv().await {
                match event {
                    SseEvent::TextDelta(text) => {
                        assistant_content.push(ContentBlock::Text(text.clone()));
                        event_tx.send(StreamEvent::TextDelta(text)).await?;
                    }
                    SseEvent::ToolUseStart { id, name } => {
                        event_tx.send(StreamEvent::ToolStart { id, name }).await?;
                    }
                    SseEvent::ToolUseComplete { id, name, input } => {
                        tool_calls.push(ToolCall { id, name, input });
                    }
                    SseEvent::Usage(usage) => {
                        state.total_tokens_used += usage.total();
                    }
                    _ => {}
                }
            }
            
            // Commit assistant message to Postgres
            let msg = Message::assistant(assistant_content);
            deps.store.threads.append_message(state.thread_id, &msg).await?;
            state.committed_messages.push(msg.id);
            
            if tool_calls.is_empty() {
                state.phase = LoopPhase::PostProcessing;
            } else {
                state.pending_tool_calls = tool_calls;
                state.phase = LoopPhase::ExecutingTools { 
                    remaining: state.pending_tool_calls.clone() 
                };
            }
            
            // Cascade check — should we escalate model?
            if let CascadeDecision::Escalate(new_tier) = 
                deps.router.cascade_check(&assistant_response, state.model_tier).await 
            {
                state.model_tier = new_tier;
                state.recovery.model_escalations += 1;
                state.phase = LoopPhase::Streaming { model: new_tier.to_model_id() };
            }
            
            Ok(StepResult::Continue)
        }
        
        LoopPhase::ExecutingTools { remaining } => {
            let results = execute_tool_calls(
                remaining.clone(),
                &deps.tools,
                &build_tool_context(state, deps),
                cancel.clone(),
                event_tx.clone(),
            ).await;
            
            // Commit EACH tool result individually — crash recovery point
            for result in &results {
                let msg = Message::tool_result(result);
                deps.store.threads.append_message(state.thread_id, &msg).await?;
                state.committed_messages.push(msg.id);
            }
            
            state.pending_tool_results = results;
            state.turn_count += 1;
            state.phase = LoopPhase::PostProcessing;
            Ok(StepResult::Continue)
        }
        
        LoopPhase::PostProcessing => {
            // Budget check
            if state.total_tokens_used > state.tenant.budget.max_tokens {
                state.phase = LoopPhase::Failed(ElenaError::BudgetExceeded {
                    tenant_id: state.tenant.tenant_id,
                });
                return Ok(StepResult::Terminal);
            }
            
            // Max turns check
            if state.turn_count >= MAX_TURNS {
                state.phase = LoopPhase::Completed;
                return Ok(StepResult::Terminal);
            }
            
            // Record usage
            deps.store.tenants.record_usage(
                state.tenant.tenant_id,
                TokenUsage { tokens: state.total_tokens_used },
            ).await?;
            
            // Save loop state to Redis (crash recovery checkpoint)
            deps.cache.save_loop_state(state.thread_id, state).await?;
            
            state.recovery.consecutive_errors = 0;
            state.phase = LoopPhase::BuildingContext;
            Ok(StepResult::Continue)
        }
        
        LoopPhase::Completed => {
            // Record episodic memory (fire-and-forget)
            deps.memory.record_episode(
                &CompletedThread::from_state(state),
                &deps.context_manager,
            ).await.ok();
            
            event_tx.send(StreamEvent::Done(Terminal::Completed)).await?;
            Ok(StepResult::Terminal)
        }
        
        LoopPhase::Failed(err) => {
            event_tx.send(StreamEvent::Error(err.clone())).await?;
            Ok(StepResult::Terminal)
        }
    }
}

// The outer loop — runs on a worker
pub async fn run_loop(
    initial_state: LoopState,
    deps: Arc<LoopDeps>,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<StreamEvent>,
) -> Result<Terminal> {
    let mut state = initial_state;
    
    loop {
        match step(&mut state, &deps, cancel.clone(), event_tx.clone()).await {
            Ok(StepResult::Continue) => continue,
            Ok(StepResult::Terminal) => {
                deps.cache.release_thread(state.thread_id).await?;
                return Ok(state.into_terminal());
            }
            Err(e) => {
                state.recovery.consecutive_errors += 1;
                if state.recovery.consecutive_errors > MAX_CONSECUTIVE_ERRORS {
                    state.phase = LoopPhase::Failed(e);
                    continue;
                }
                
                match classify_error(&e) {
                    ErrorClass::ContextOverflow => {
                        state.recovery.context_rebuilds += 1;
                        state.phase = LoopPhase::BuildingContext;
                    }
                    ErrorClass::RateLimit { retry_after } => {
                        tokio::time::sleep(retry_after).await;
                    }
                    ErrorClass::ModelError => {
                        state.model_tier = state.model_tier.escalate();
                        state.phase = LoopPhase::Routing;
                    }
                    ErrorClass::Fatal => {
                        state.phase = LoopPhase::Failed(e);
                    }
                }
            }
        }
    }
}
```

### Comparison with Claude Code

| Aspect | Claude Code `query.ts` | Elena `elena-core` |
|--------|------------------------|---------------------|
| Lines | 1,730 (nested, hard to follow) | ~400 (flat match arms) |
| State | Mutable closure variables | Explicit serializable struct |
| Recovery | Nested try/catch with flags | Error classification → state transition |
| Crash recovery | None (state lost) | Redis checkpoint after each tool |
| Scaling | Single process | Any worker picks up any session |
| Model routing | Static (configured) | ML-driven per request |
| Context | 4 heuristic layers | Embedding retrieval + on-demand summary |

---

## Crate 10: `elena-gateway`

**Purpose:** API gateway binary. Accepts connections, routes to workers via NATS.

```rust
#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/v1/threads", post(create_thread))
        .route("/v1/threads/:id/messages", post(send_message))
        .route("/v1/threads/:id", get(get_thread))
        .route("/v1/threads/:id/stream", get(ws_upgrade))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .layer(mtls_layer())
        .layer(jwt_extraction())
        .layer(rate_limit_layer())
        .layer(tracing_layer())
        .layer(compression_layer());
    
    axum::serve(listener, app).await;
}

async fn handle_ws(
    mut socket: WebSocket,
    state: GatewayState,
    thread_id: ThreadId,
    tenant: TenantContext,
) {
    // Subscribe to NATS subject for this thread's events
    let mut subscriber = state.nats
        .subscribe(format!("elena.thread.{}.events", thread_id))
        .await?;
    
    loop {
        tokio::select! {
            // Client → Elena: new messages
            Some(msg) = socket.recv() => {
                let user_msg: UserMessage = serde_json::from_str(&msg.to_text()?)?;
                state.nats.publish(
                    "elena.work.incoming",
                    WorkRequest { thread_id, tenant: tenant.clone(), message: user_msg },
                ).await?;
            }
            // Elena → Client: stream events (token by token)
            Some(event) = subscriber.next() => {
                let stream_event: StreamEvent = serde_json::from_slice(&event.payload)?;
                socket.send(Message::Text(serde_json::to_string(&stream_event)?)).await?;
            }
        }
    }
}
```

---

## Crate 11: `elena-worker`

**Purpose:** Worker binary. Runs agentic loops, consumes work from NATS.

```rust
#[tokio::main]
async fn main() {
    let deps = Arc::new(build_deps().await);
    
    // Competing consumers pattern — NATS distributes work automatically
    let mut subscriber = deps.nats
        .queue_subscribe("elena.work.incoming", "workers")
        .await?;
    
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_LOOPS));
    
    while let Some(msg) = subscriber.next().await {
        let permit = semaphore.clone().acquire_owned().await?;
        let deps = deps.clone();
        
        tokio::spawn(async move {
            let _permit = permit;
            let request: WorkRequest = serde_json::from_slice(&msg.payload)?;
            
            // Claim thread (atomic Redis op — prevents double-processing)
            if !deps.cache.claim_thread(request.thread_id, WORKER_ID).await? {
                return Ok(());
            }
            
            // Create event channel → NATS publisher
            let (event_tx, mut event_rx) = mpsc::channel(128);
            let nats = deps.nats.clone();
            let thread_id = request.thread_id;
            
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    nats.publish(
                        format!("elena.thread.{}.events", thread_id),
                        serde_json::to_vec(&event)?,
                    ).await?;
                }
                Ok::<_, anyhow::Error>(())
            });
            
            // Build or resume loop state
            let state = match deps.cache.load_loop_state(request.thread_id).await? {
                Some(existing) => existing,
                None => LoopState::new(request),
            };
            
            let cancel = CancellationToken::new();
            run_loop(state, deps, cancel, event_tx).await
        });
    }
}
```

---

## Infrastructure — AWS EKS Deployment

```
┌─────────────────────────────────────────────────────────┐
│                     AWS Region                           │
│                                                          │
│  ┌──────────────────────────────────────────────────┐   │
│  │               EKS Cluster                         │   │
│  │                                                    │   │
│  │  ┌────────────┐  NLB (Network Load Balancer)      │   │
│  │  │            │  ← TLS termination                │   │
│  │  └─────┬──────┘                                    │   │
│  │        │                                           │   │
│  │  ┌─────▼──────────────────────────────────────┐   │   │
│  │  │  Gateway Pods (HPA: 5-50 pods)              │   │   │
│  │  │  Instance: c7g.xlarge (Graviton, 4 vCPU)    │   │   │
│  │  │  ~20K WebSocket connections per pod          │   │   │
│  │  └─────┬──────────────────────────────────────┘   │   │
│  │        │ NATS                                      │   │
│  │  ┌─────▼──────────────────────────────────────┐   │   │
│  │  │  NATS Cluster (3 pods, JetStream)           │   │   │
│  │  │  Instance: r7g.large (Graviton, 2 vCPU)     │   │   │
│  │  └─────┬──────────────────────────────────────┘   │   │
│  │        │                                           │   │
│  │  ┌─────▼──────────────────────────────────────┐   │   │
│  │  │  Worker Pods (HPA: 10-200 pods)             │   │   │
│  │  │  Instance: c7g.2xlarge (Graviton, 8 vCPU)   │   │   │
│  │  │  ~500 concurrent loops per pod               │   │   │
│  │  │  + ONNX models loaded in memory              │   │   │
│  │  └────────────────────────────────────────────┘   │   │
│  │                                                    │   │
│  │  ┌────────────────────────────────────────────┐   │   │
│  │  │  Plugin Sidecars (per-connector deployment) │   │   │
│  │  │  shopify-connector: 2-10 pods               │   │   │
│  │  │  gmail-connector: 2-10 pods                 │   │   │
│  │  │  calendar-connector: 2-10 pods              │   │   │
│  │  └────────────────────────────────────────────┘   │   │
│  └──────────────────────────────────────────────────┘   │
│                                                          │
│  ┌──────────────────┐  ┌────────────────────────────┐   │
│  │ ElastiCache       │  │ RDS PostgreSQL              │   │
│  │ Redis Cluster     │  │ db.r7g.2xlarge              │   │
│  │ r7g.xlarge        │  │ Multi-AZ, pgvector          │   │
│  │ 3 shards          │  │ Read replicas: 2            │   │
│  │ Encryption: yes   │  │ Encryption: yes             │   │
│  └──────────────────┘  └────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

### Scaling Math at 1M Concurrent Sessions

- ~50 gateway pods x 20K connections = 1M WebSocket connections
- At 10% active = 100K concurrent loops
- 200 worker pods x 500 loops = 100K concurrent loops
- NATS handles millions of messages/sec — not the bottleneck
- Redis: 3 shards handle ~300K ops/sec — sufficient for state read/write per loop turn
- Postgres: read replicas for retrieval, primary for writes. pgvector IVFFlat indexing for fast similarity search

### Cost Optimization

- Graviton (ARM) instances: ~20% cheaper than x86 for same performance
- Spot instances for worker pods (graceful drain — loop state in Redis, interruption = resume on another pod)
- Gateway pods must be on-demand (WebSocket connections can't migrate)

---

## Security Architecture

```
App (Solen/Hannlys)
  │
  │ mTLS (mutual TLS) — service identity verification
  │ + JWT (short-lived, contains TenantContext)
  │
  ▼
Gateway
  │ Validates: mTLS cert chain, JWT signature + expiry
  │ Extracts: TenantId, UserId, WorkspaceId, Permissions
  │ Enforces: rate limits per tenant
  │
  ▼
Worker
  │ Inherits TenantContext from gateway
  │ Enforces: per-tool permission checks
  │ Enforces: budget limits per tenant
  │ Sandboxes: file operations to workspace directory
  │
  ▼
Plugin Sidecars
  │ mTLS between worker and sidecar
  │ Per-tenant OAuth tokens (stored encrypted in Postgres)
  │ Secret scanning on all plugin responses
```

**Key security properties:**
- mTLS everywhere internally — zero trust between services
- JWT for tenant context — no database lookup per request
- Workspace sandboxing — file tools operate in `/workspaces/{workspace_id}/` only
- Secret scanning — all tool results and plugin responses scanned before returning to LLM
- Encryption at rest — RDS and ElastiCache with AWS KMS
- VPC isolation — all services in private subnets, only NLB is public-facing
- Audit logging — every tool execution, LLM call, permission decision logged

---

## Build Order

Dependency-ordered sequence. Each phase produces a working, testable system.

### Phase 1: Foundation
```
elena-types → elena-config → elena-store
```
- All types, IDs, errors defined
- Postgres migrations (threads, messages, tenants, episodes)
- Redis connection + basic ops
- **Deliverable:** Can create threads and store messages

### Phase 2: LLM Client
```
elena-llm (Anthropic provider only)
```
- SSE streaming parser
- Retry with backoff
- Token usage tracking
- **Deliverable:** Can stream a response from Claude API

### Phase 3: Minimal Loop
```
elena-tools (PluginCall only) → elena-core (state machine)
```
- The agentic loop state machine — all phases
- Tool execution orchestration
- Error recovery
- **Deliverable:** Can receive a message, call LLM, execute tools, loop, return result

### Phase 4: Intelligence
```
elena-context → elena-memory → elena-router
```
- Embedding model integration (ONNX)
- Context retrieval from pgvector
- Episodic memory record/recall
- Model routing classifier
- **Deliverable:** Loop uses intelligent context management and model routing

### Phase 5: Networking
```
elena-gateway → elena-worker → NATS integration
```
- WebSocket gateway with axum
- Worker with NATS queue consumption
- Token-by-token streaming end-to-end
- mTLS + JWT auth
- **Deliverable:** Full distributed system — apps can connect and use Elena

### Phase 6: Plugins
```
elena-plugins → first connector
```
- gRPC proto definition
- Plugin registry
- First real connector sidecar
- **Deliverable:** Apps can call external services through Elena

### Phase 7: Production Hardening
- Load testing at 100K → 500K → 1M sessions
- Observability (tracing, metrics, dashboards)
- Chaos testing (kill workers, kill Redis, network partitions)
- Cost optimization (Spot instances, right-sizing)
- **Deliverable:** Production-ready Elena

---

## Key Cargo Dependencies

```toml
[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
axum = { version = "0.8", features = ["ws"] }
reqwest = { version = "0.12", features = ["stream", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres", "uuid", "chrono"] }
fred = "9"                                    # Redis
async-nats = "0.37"                           # NATS client
tonic = "0.12"                                # gRPC
prost = "0.13"                                # Protobuf
ort = "2"                                     # ONNX Runtime
dashmap = "6"                                 # Concurrent HashMap
tokio-util = "0.7"                            # CancellationToken
futures = "0.3"                               # Stream, FuturesUnordered
tracing = "0.1"
tracing-subscriber = "0.3"
tracing-opentelemetry = "0.27"
opentelemetry-otlp = "0.26"
thiserror = "2"
anyhow = "1"
ulid = "1"
chrono = { version = "0.4", features = ["serde"] }
secrecy = "0.10"                              # Secret strings
backon = "1"                                  # Retry logic
tower = "0.5"                                 # Middleware
tower-http = "0.6"                            # HTTP middleware
eventsource-stream = "0.2"                    # SSE parsing
bytes = "1"
clap = { version = "4", features = ["derive"] }
```

---

## Phase 5 calibration notes (validated 2026-04-17 during `elena-gateway` + `elena-worker` implementation)

What landed and what got pushed:

1. **JWT-only auth in Phase 5.** The doc's `mTLS + JWT hybrid` is deferred
   to Phase 7 — Phase 5 ships JWT validation only (HS256/HS384/HS512
   shared-secret + RS256/RS384/RS512 PEM public key + ES256/ES384). The
   gateway extracts a `TenantContext` from the claims; permissions and
   budget are loaded from `TenantStore` at session start so operator
   policy changes take effect without reissuing tokens.

2. **WebSocket-primary surface.** Phase 5 ships only `POST /v1/threads`
   (create) + `GET /v1/threads/:id/stream` (WebSocket upgrade) + `/health`
   + `/version`. HTTP POST `/messages` and SSE fallbacks are deferred —
   they're nice-to-have variants of the WebSocket path that nothing
   internal needs.

3. **`WorkRequest` lives in `elena-types::transport`.** New shared type
   carrying `request_id` (NATS dedup key), `tenant`, `thread_id`, the user
   `message`, and per-turn overrides (`model`, `system_prompt`,
   `max_turns`, `max_tokens_per_turn`). Keeps the wire schema between
   gateway and worker in one place.

4. **NATS subjects** are exactly what the doc specifies:
   - `elena.work.incoming` (`JetStream`, work-queue retention) — gateway
     publishes one `WorkRequest` per `send_message`.
   - `elena.thread.{id}.events` (core pub/sub) — worker publishes every
     `StreamEvent`; gateway WebSocket handler subscribes for the
     connection's lifetime.
   - `elena.thread.{id}.abort` (core pub/sub) — gateway publishes empty
     payload on `{"action":"abort"}`; worker fires the loop's
     `CancellationToken`.

5. **At-least-once + thread claim = effectively once.** JetStream
   redelivery is safe because (a) the worker calls
   `cache.claim_thread(thread, worker_id)` first and bails on `false`
   (another worker already owns it), and (b) the user message goes through
   `ThreadStore::append_message_idempotent` (`INSERT ... ON CONFLICT DO
   NOTHING`).

6. **Worker concurrency = `Arc<Semaphore>` over a JetStream pull
   consumer.** `WorkerConfig::max_concurrent_loops` (default 500) caps
   `tokio::spawn`s. Each spawn awaits a permit, then `handle()` claims +
   appends + runs the loop + publishes events + acks.

7. **Gateway is stateless beyond JWT + NATS.** No DB writes from the
   gateway except `create_thread`. Everything else is "publish a
   `WorkRequest`, subscribe to events." A reconnecting client refetches
   missed history from `ThreadStore::list_messages` — events on NATS are
   ephemeral by design.

8. **No `/metrics` endpoint, no OTel exporter.** `tracing` to stdout
   only. Real metrics + OTLP export are Phase 7 hardening.

9. **Single-process Phase-5 smoke.** `bins/elena-phase5-smoke` boots the
   gateway and worker in the same process, brings up NATS + Postgres +
   Redis via testcontainers, and exchanges one turn over a real
   WebSocket. Operators running the real binaries point them at
   `nats://127.0.0.1:54222` from `docker-compose.yml`.

10. **No `axum` `metrics` route**. The doc lists `GET /metrics`; Phase 5
    skips it entirely. Ops tooling that pokes the route will get a 404 —
    real Prometheus integration lands with Phase 7's observability work.

---

## Phase 6 calibration notes (validated 2026-04-18 during `elena-plugins` implementation)

1. **One synthesised `Tool` per action, not a multiplexed `PluginCall`.**
   The doc sketch's `as_tool_schemas()` returning one schema per action
   is the right surface. Per-action descriptions and schemas cut
   malformed tool calls dramatically vs a single `plugin_call` tool that
   forces the model to pick an action via a selector field. Implemented
   via `PluginActionTool` in `elena_plugins::action_tool`, registered
   directly into the existing `ToolRegistry` so the orchestrator has no
   special case.

2. **The Phase-3 placeholder `elena_tools::PluginCall` was deleted.**
   No deprecation shim — it was always labelled as a Phase-6
   placeholder, and left nothing in production calling it.

3. **`claim_thread` is now idempotent for the same owner.** Caught
   during Phase-5 smoke validation: the worker's `dispatch::handle`
   claims the thread, then `run_loop` claims again inside `drive_loop`,
   and the original `SET NX PX` rejected the second attempt. Replaced
   with a Lua CAS that claims if free, refreshes if this owner already
   holds it, and rejects only a *different* owner. Regression guarded
   by `claim_thread_is_idempotent_for_same_owner`.

4. **Out-of-process gRPC sidecars.** `tonic` 0.12 + `prost` 0.13. Each
   plugin gets one `tonic::transport::Channel` built with TCP keepalive
   (30 s) + HTTP/2 keepalive (20 s); reconnection is transparent on the
   next RPC. The vendored protoc (`protoc-bin-vendored`) ships inside
   `build.rs` so contributors don't need a system protobuf compiler.

5. **Proto additions vs the doc sketch.** Two pragmatic extras:
   `ActionDefinition.is_read_only` (connectors opt their actions into
   Elena's concurrent-read-only batch; default false) and
   `PluginRequest.context` with `tenant_id` + `thread_id` (for the
   sidecar's own logging/audit — Elena does not inject credentials).

6. **Schema synthesis = `{plugin_id}_{action_name}`.** Matches
   Anthropic's `^[a-zA-Z0-9_-]{1,64}$` tool-name regex. Cross-plugin
   name collisions are detected at registration time and return
   `PluginError::NameCollision`. Example collision: plugin `a` with
   action `b_c` and plugin `a_b` with action `c` both map to `a_b_c`.

7. **`PluginRegistry` on `LoopDeps`, not a replacement for
   `ToolRegistry`.** The synthesised tools live in the shared
   `ToolRegistry`; the registry itself is on `LoopDeps.plugins` so the
   worker can `shutdown()` its background `HealthMonitor` on
   `CancellationToken::cancel()` and future surfaces (a
   `/v1/plugins` admin endpoint) can introspect manifests. Lifecycle
   ownership drove the decision.

8. **Streaming `Execute` → `StreamEvent::ToolProgress` + `ToolResult`.**
   Per-`PluginActionTool::execute`: `tokio::select!` races the gRPC
   stream against `ctx.cancel` and a per-call timeout. `Code::Unavailable`
   and `Code::DeadlineExceeded` map to `ToolError::Execution` (and flip
   the plugin's health gauge); `Code::InvalidArgument` maps to
   `ToolError::InvalidInput`.

9. **Background health + lazy mark-down.** Single
   `tokio::time::interval(30s)` + `JoinSet` runs `Health(Empty)` on
   every registered plugin in parallel with a 5-s per-call timeout.
   Failed RPC calls also flip the gauge lazily. Gauge is
   `Arc<AtomicU8>` (`Unknown=0 / Up=1 / Down=2`) — lock-free read path
   for operator surfaces.

10. **Reference connector: `bins/elena-connector-echo`.** Lib + bin —
    the lib exposes `EchoConnector` (one action, `reverse`, emits two
    `ProgressUpdate`s then a `FinalResult`) so Phase-6 tests can spin
    the real server impl in-process instead of subprocess-spawning a
    binary. Zero external deps, exercises every bridge code path.

11. **Phase-6 smoke: wiremock fallback when no `ANTHROPIC_API_KEY`.**
    Both Phase-5 and Phase-6 smokes auto-detect the key and fall back
    to wiremock-served SSE transcripts that cover the full stack
    without an outbound API call. The Phase-6 transcript is two
    responses: first a `tool_use` for `echo_reverse`, then a text
    response containing the reversed word. This lets CI validate the
    distributed system without provisioning API keys.
