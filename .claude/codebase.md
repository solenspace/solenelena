# Elena — Full Codebase Walkthrough

**Audience:** you, as of 2026-04-22, owning Elena after the SCRUM-85..104 work documented in `~/Documents/docs/elena-agentic-integration.md`, with zero prior Rust experience.

**Goal:** by the time you finish this document you should be able to (a) read any `.rs` file in the repo and roughly understand it, (b) trace a user message from WebSocket through to Slack and back, (c) know which file to open for any of the common change recipes, (d) know how to ship changes safely.

The document is long on purpose — it doubles as a reference. You don't have to read it top-to-bottom in one sitting. If you already know Rust, skim Part I, read Part II/III, then use Part IV as a lookup table.

---

# Table of contents

- **Part I — Rust primer** (ch. 1–10)
- **Part II — Elena at a glance** (ch. 11–13)
- **Part III — End-to-end walkthrough of a single user message** (ch. 14–23)
- **Part IV — Crate-by-crate reference** (ch. 24–39)
- **Part V — Binaries** (ch. 40–43)
- **Part VI — Storage (Postgres / Redis / NATS)** (ch. 44–46)
- **Part VII — Deployment** (ch. 47–52)
- **Part VIII — Cookbook: how to make common changes** (ch. 53–61)
- **Part IX — Debugging & ops** (ch. 62–64)
- **Part X — Pointers & further reading** (ch. 65–66)

---

# Part I — Rust primer

A compressed but self-contained teach-the-language section, with every example taken from Elena. If you're comfortable with TypeScript, Go, or Python, you will find the concepts familiar. The unique things about Rust are (1) the compiler is much stricter than you're used to, (2) memory management is done by the compiler at compile-time rather than by a garbage collector or by you manually, and (3) errors are values rather than exceptions.

## Chapter 1 — Toolchain and project layout

**Rust** is a compiled, statically-typed systems language. Its build tool is `cargo`, its version manager is `rustup`, and its package registry is `crates.io`.

- `rustup` installs and switches compiler versions. Elena pins `rust-version = "1.85"` in `Cargo.toml`, so you need at least that.
- `cargo build` compiles, `cargo test` runs tests, `cargo run -p <bin-name>` runs a binary, `cargo fmt` formats, `cargo clippy` lints.

A **crate** is a compilation unit — one library (`lib.rs`) or binary (`main.rs`). A **module** is a subdivision inside a crate (a file or a `mod foo { ... }` block). A **workspace** is a top-level directory with a `Cargo.toml` that declares many member crates; they share one `target/` build directory and one lockfile. Elena is a workspace with 32 members (16 library crates under `crates/`, 16 binaries under `bins/`).

Edition: Elena uses `edition = "2024"`. Editions are small language-surface updates; they do not break compatibility between crates.

## Chapter 2 — Ownership, borrowing, lifetimes

Rust has no garbage collector. Every value has exactly one **owner**. When the owner goes out of scope, the value is dropped (memory freed, file handles closed, etc.).

```rust
let s = String::from("hello");  // `s` owns the heap-allocated bytes
let t = s;                       // ownership moves to `t`
// println!("{}", s);            // compile error: `s` has been moved
```

If you want to pass a value somewhere without giving up ownership, you **borrow** it by taking a reference: `&T` (shared, immutable) or `&mut T` (exclusive, mutable). At any time a value has either any number of `&T` borrows or exactly one `&mut T` borrow — never both. This rule is what makes data races impossible by construction.

**Lifetimes** (`'a`) are names for scopes that the compiler uses to make sure references never outlive their referent. Most of the time the compiler infers them; you only write them when you have multiple references in a function signature and the compiler can't figure out which outlives which.

Worked Elena example (from `crates/elena-core/src/step.rs`):

```rust
pub async fn filter_schemas_for_tenant(
    tenant_id: &TenantId,
    workspace_id: Option<&WorkspaceId>,
    schemas: Vec<ToolSchema>,
    plugins: &PluginRegistry,
    store: &Store,
) -> Result<Vec<ToolSchema>, ElenaError> { ... }
```

Read this as: "takes a borrow of a TenantId, an optional borrow of a WorkspaceId, owns (and may consume) a Vec of ToolSchema, borrows a PluginRegistry and a Store, and asynchronously returns either an owned Vec of ToolSchema or an owned ElenaError."

The three borrowed inputs (`&TenantId`, `&PluginRegistry`, `&Store`) must all live at least as long as the async future that this function returns. Since callers typically hold those values for the whole loop, this is fine.

## Chapter 3 — `Option`, `Result`, the `?` operator, and error styles

Rust has no `null` and no exceptions. Instead:

- **`Option<T>`** is `Some(T)` or `None`. Used for "this may or may not be present."
- **`Result<T, E>`** is `Ok(T)` or `Err(E)`. Used for "this might fail."

The `?` operator is syntactic sugar for "if this is `Err`, return it from the current function; otherwise unwrap the `Ok`." It is the main way Rust code propagates errors.

```rust
let rec = store.get_tenant(tenant_id).await?;      // early-return on StoreError
let workspace = store.get_workspace(tenant_id, ws_id).await?;
```

Elena's convention (enforced by lint, see `deny.toml`):
- **`thiserror`** is used inside library crates (everything under `crates/`) to define domain-specific error types. Every public error is a `#[derive(Error, Debug)]` enum with variants for each failure class. This makes errors matchable — callers can branch on exactly what went wrong.
- **`anyhow`** is allowed **only** inside `bins/`. In binaries we don't need callers to distinguish error types, so using `anyhow::Error` (a wrapped trait object) saves boilerplate.
- **`unwrap_used`** and **`expect_used`** are `deny` in the workspace. You cannot write `.unwrap()` or `.expect()` in production code. Instead you propagate with `?` or you explicitly handle the `None`/`Err` case. This is why every function in Elena returns `Result<_, _>`.

The top-level Elena error is `ElenaError` (`crates/elena-types/src/error.rs`). It bundles every sub-error (`LlmApiError`, `ToolError`, `StoreError`, `ConfigError`, plus boundary variants like `ContextOverflow`, `BudgetExceeded`, `PermissionDenied`, `Aborted`). It is `Serialize + Deserialize` so it can cross NATS/gRPC/WebSocket boundaries unchanged — foreign errors (`sqlx::Error`, `tonic::Status`) get stringified at the crate boundary.

## Chapter 4 — Traits, generics, trait objects, `Arc<dyn Trait>`

A **trait** is an interface — a named bundle of methods that a type can implement. Elena's most important traits:

- `Tool` (in `elena-tools`) — the contract every agent tool implements.
- `LlmClient` (in `elena-llm`) — the contract every LLM provider implements.
- `Embedder` (in `elena-context`) — the contract for text-embedding implementations.
- `AuditSink` (in `elena-store`) — the contract for audit log sinks.
- `SecretProvider` (in `elena-auth`) — the contract for fetching secrets.

There are two ways to use a trait:

1. **Generics (static dispatch)**: `fn handle<T: Tool>(tool: &T, input: Value)`. The compiler generates a specialized version per concrete type. Fast but increases binary size.
2. **Trait objects (dynamic dispatch)**: `fn handle(tool: &dyn Tool, input: Value)` or `fn handle(tool: Arc<dyn Tool>, input: Value)`. One copy in the binary; the call goes through a vtable. This is what Elena uses almost everywhere because the actual set of tools is known only at runtime (plugins register dynamically).

**`Arc<T>`** is an atomically-reference-counted pointer — a shared owner. Multiple threads can hold `Arc<T>` to the same value; the value is dropped when the last `Arc` goes away. `Arc<dyn Trait>` is how Elena stores registries of tools, LLM clients, and embedders. You'll see it everywhere in `LoopDeps`:

```rust
pub struct LoopDeps {
    pub store: Arc<Store>,
    pub llm: Arc<dyn LlmClient>,
    pub tools: ToolRegistry,               // already Arc-backed internally
    pub router: ModelRouter,
    pub context: ContextManager,           // Arc<dyn Embedder> inside
    pub memory: EpisodicMemory,
    pub plugins: Arc<PluginRegistry>,
    pub metrics: Arc<LoopMetrics>,
    pub defaults: Arc<DefaultsConfig>,
    pub rate_limits: Arc<RateLimitsConfig>,
    pub cache_policy: CachePolicy,
}
```

Every phase handler in `step.rs` takes `&LoopDeps` and treats it as the god-bag of everything it might need.

## Chapter 5 — Async, Tokio, channels

Elena is thoroughly async. Async functions return a `Future<Output=T>` — a value that, when `.await`ed, will eventually produce a `T`.

`async fn foo() -> Bar` desugars to `fn foo() -> impl Future<Output=Bar>`. You can `.await` inside another `async fn` or inside a Tokio task.

**Tokio** is the async runtime. It provides the thread-pool that runs futures, I/O primitives (TCP, timers), synchronization (Mutex, RwLock, semaphores), and channels. Elena bins initialize Tokio via `#[tokio::main] async fn main() -> Result<()> { ... }`.

Useful primitives you'll see:
- **`Arc<Mutex<T>>`**: shared mutable state. Lock is async-aware — `mutex.lock().await`. Use sparingly; prefer `RwLock` for read-heavy state, or channels for message passing.
- **`RwLock<T>`**: many readers or one writer. `JwksValidator` uses it to hold the current JWKS key set while a background task refreshes.
- **`mpsc`** (`tokio::sync::mpsc::channel`): multi-producer single-consumer queue. Elena uses it extensively — audit writes, progress events, WebSocket outbound frames.
- **`oneshot`**: one-shot send/receive. Good for "await this background task's result."
- **`broadcast`**: many-to-many fanout.

**`#[instrument]`** (from `tracing`) wraps an `async fn` in a span so every `tracing::info!` inside is tagged with the span's fields. Required on every async boundary in Elena — you'll see it on almost every handler.

## Chapter 6 — Error propagation across async + network boundaries (design invariant #3)

Elena's rule: every public type (including every error) is `Serialize + Deserialize`. Library errors that aren't serializable (`sqlx::Error`, `fred::Error`, `tonic::Status`, `reqwest::Error`) are converted to `String` at the crate boundary.

The mechanism is usually a small classifier function. See `crates/elena-store/src/sql_error.rs`:

```rust
pub fn classify_sqlx(e: sqlx::Error) -> StoreError { ... }
```

When a store method wants to return a `StoreError`, it maps the `sqlx::Error` through this classifier (unique-constraint violations become `StoreError::Conflict`, not-found becomes `StoreError::NotFound`, etc.) and then returns it. The sqlx type never leaks out.

This lets `ElenaError` travel intact from a failing Postgres INSERT, through `run_loop`, out over NATS, across the gateway, down the WebSocket, and into the browser — no adapters in the middle.

## Chapter 7 — Serde: `Serialize`, `Deserialize`, tagged enums

`serde` is the serialization framework. Add `#[derive(Serialize, Deserialize)]` to a struct and it gains JSON (and BSON, and MessagePack, etc.) round-trip.

Key attributes you'll see:
- `#[serde(rename_all = "camelCase")]` — wire field names are camelCase, Rust struct fields stay snake_case. Used on HTTP bodies that must match the BFF's TypeScript wire format (e.g., `AttachToolBody`).
- `#[serde(deny_unknown_fields)]` — reject JSON with fields not declared in the struct. Prevents silently dropping unknown keys.
- `#[serde(tag = "event")]` on an enum — pick a tagged representation so each variant serializes as `{"event": "text_delta", ...}`. Elena's `StreamEvent` uses this; that's why the WebSocket wire format is `{"event": "tool_use_start", "tool_use_id": "...", "name": "slack_post_message"}`.
- `#[serde(skip_serializing_if = "Option::is_none")]` — omit fields when they're `None`.

## Chapter 8 — Cargo, workspace, features, lints, `deny.toml`

The root `Cargo.toml` (see `/Cargo.toml`) is the workspace manifest. It:

- Lists `members = [...]` — every crate in the workspace.
- Declares `[workspace.package]` common fields (edition, license, repo).
- Declares `[workspace.lints]` — the lints enforced by `cargo clippy --workspace`. Elena sets `unsafe_code = "forbid"`, `unwrap_used = "deny"`, `panic = "deny"`, `print_stdout/stderr = "warn"`, plus `clippy::pedantic` at warn level.
- Declares `[workspace.dependencies]` — shared dependency versions. Member crates write `serde = { workspace = true }` to inherit the version.

Each crate has its own `Cargo.toml` that declares deps (usually `{ workspace = true }`), features, and binary/library targets.

**Features** are opt-in chunks of functionality. For example `elena-auth` has `vault` and `kms` features that pull in cloud-provider SDKs only when enabled.

**`deny.toml`** is the config for `cargo-deny`, a supply-chain linter. It enumerates:
- **Advisories**: which RUSTSEC vulnerability IDs are ignored (with justification), denies yanked crates.
- **Licenses**: explicit allowlist (Apache-2.0, MIT, BSD, ISC, etc.); anything else breaks the build.
- **Bans**: duplicate crate versions are `warn`; wildcard version specs are `warn`; unknown registries and unknown git sources are denied.

`rustfmt.toml` sets max width 100 and `use_small_heuristics = "Max"` — the `cargo fmt` is the source of truth.

## Chapter 9 — Testing

Three test styles live in Elena:

1. **Unit tests** — inline in the same file as the code under test, wrapped in `#[cfg(test)] mod tests { ... }`. Most `.rs` files have one. Runs with `cargo test`.
2. **Integration tests** — each file under `crates/<crate>/tests/` is its own binary that links the crate's public API. Run with `cargo test -p <crate> --test <filename>`. Elena uses these for Phase tests (`phase4_loop.rs`, `phase7_blockers.rs`), round-trip checks, and testcontainer-backed flows.
3. **Smoke binaries** — real end-to-end runs in `bins/elena-phase7-smoke`, `elena-solen-smoke`, `elena-hannlys-smoke`. These boot the actual stack (some via testcontainers, some against live services), exercise a scenario, exit 0 on success or 2 on "skip — missing required env vars."

Elena's integration tests rely heavily on **`testcontainers`** — a crate that spins up disposable Docker containers (Postgres+pgvector, Redis, NATS) per test. Requires local Docker; hence `cargo test` is slower than most projects.

## Chapter 10 — Reading a trace

Logs are structured via the `tracing` crate:

```rust
#[instrument(skip(deps), fields(thread_id = %state.thread_id, phase = ?state.phase))]
pub async fn step(state: &mut LoopState, deps: &LoopDeps) -> Result<StepOutcome, ElenaError> {
    tracing::info!("step begin");
    ...
}
```

- `%val` uses `Display` formatting (`ThreadId` renders as a ULID).
- `?val` uses `Debug` formatting (enum variants, etc.).
- `skip(deps)` omits that argument from the span record (too big to log).

At runtime, `RUST_LOG=info,elena_core=debug,elena_gateway=info` controls which spans and events are emitted. The `elena-observability` crate handles subscriber setup — in production, JSON output; in dev, pretty console.

For distributed tracing (OpenTelemetry), `TraceMeta` (W3C traceparent + tracestate) is carried on `WorkRequest` and propagated to plugin gRPC calls, so a Jaeger view stitches gateway → worker → plugin into one trace.

---

# Part II — Elena at a glance

## Chapter 11 — Mission in one paragraph

Elena is an **application-agnostic agentic backend**. One process can serve any tenant: a user message arrives on WebSocket, the gateway publishes it to NATS, a worker claims the thread, the agentic loop streams the LLM, dispatches tool calls (built-in or plugin sidecars over gRPC), checkpoints state to Redis, persists to Postgres, and streams events back to the client. Solen (the web app where Slack/Notion integrations live) and Hannlys (a marketplace product) both run on this engine; new products "plug in" by registering a tenant + workspace + plugin ownership through the admin API.

## Chapter 12 — Runtime diagram, annotated by crate

```
                                      ┌────────────┐
                  ┌─────tool_use─────▶│  plugin    │──HTTP─▶ Slack/Notion/...
                  │                   │  sidecar   │
                  │                   │  (gRPC)    │
client ─WS────▶ gateway ──NATS─▶ worker ─loop─▶ LLM provider
                  ▲                   │            
                  │                   │
                  └─events◀──── publisher
                                      │
                                      ├─audit_events──▶ Postgres
                                      ├─checkpoint───▶ Redis
                                      └─messages─────▶ Postgres
```

Arrows by crate ownership:
- Client ↔ gateway: axum + `tokio-tungstenite` in **`elena-gateway`**.
- Gateway → NATS: `async-nats` JetStream publish, code in `crates/elena-gateway/src/nats.rs`.
- NATS → worker: `async-nats` JetStream consumer in **`elena-worker`**.
- Worker → loop: `elena_core::run_loop`, the focus of **`elena-core`**.
- Loop → LLM provider: `dyn LlmClient` in **`elena-llm`**.
- Loop → plugin sidecar: `PluginActionTool::execute` in **`elena-plugins`** (gRPC client).
- Plugin sidecar → external API: each connector crate under `bins/elena-connector-*`.
- Worker → events: core pub/sub publish in `crates/elena-worker/src/publisher.rs`; gateway subscribes and fans out to WebSocket clients.
- Any writer → Postgres: `Store` façade in **`elena-store`**.
- Any writer → Redis: `SessionCache` in `elena-store`.

## Chapter 13 — Six design invariants and their enforcement

1. **Application-agnostic.** No app-specific types anywhere in the workspace. Tenancy is opaque; apps are differentiated by `TenantId` + plugin ownership. Enforcement: code review; every crate is a reusable library.
2. **Multi-tenant at every boundary.** Every `Store` method takes `TenantId`. Enforcement: compiler — pass `UserId` where `TenantId` is expected and it fails.
3. **Serializable everywhere.** Every public type implements `Serialize + Deserialize`. Non-serializable foreign errors (`sqlx::Error`, `tonic::Status`, etc.) are converted to `String` at the crate edge. Enforcement: compile fails if `ElenaError` borrows a non-serializable type.
4. **No `unsafe`.** `unsafe_code = "forbid"` in the workspace Cargo.toml. Enforcement: compiler.
5. **Hot path never blocks.** Audit writes go through a bounded `mpsc::channel` (capacity 10 000) to a background task. Overflow is counted via `elena_audit_drops_total`. Plugin RPCs have per-call timeouts. Credential decrypts are in-memory only. Enforcement: code review + metric alert.
6. **Validate at boundaries, trust internal code.** Arg validation lives at the gateway (JWT, JSON schemas via serde) and at plugin sidecars (their own input schemas). Internal functions take typed values and trust them. Enforcement: types.

---

# Part III — Walkthrough: "send hi to #elena-test"

Follow one user message all the way through. File-and-line cites are anchored to the repo.

## Chapter 14 — Browser → BFF → Elena gateway

The browser in `solenspace` talks to the **BFF** (`apps/server-api`), not to Elena directly — browsers hit CORS issues against Elena, and the BFF holds the Elena admin token. See SCRUM-96 in the post-mortem.

Sequence:

1. User signs in on the web app. BFF mints an Elena JWT via `apps/server-api/src/services/elena/jwt.rs::mint_elena_jwt` — the `user_id` claim is deterministically derived from the Supabase UUID via `Ulid::from_bytes(uuid.into_bytes())` so Elena's `UserId` type (a ULID) round-trips (SCRUM-97).
2. User opens a new chat. BFF's `apps/server-api/src/api/chats.rs::create_chat` runs transactionally: INSERT chat row → `admin_client.create_thread` (calls Elena's `POST /v1/threads`) → UPDATE chats SET `elena_thread_id`. Rollback on Elena failure.
3. Browser opens WebSocket to `wss://elena.../v1/threads/{thread_id}/stream?token=<jwt>`. The BFF also exposes a proxy for `POST /v1/threads/{id}/approvals`.
4. User types "send hi to #elena-test in Slack" and submits. Web app calls `elena_client.sendFrame({action: "send_message", text: "...", autonomy: "yolo"})`.

## Chapter 15 — Gateway JWT validation + frame codec

On the Elena side, the WebSocket upgrade is handled in `crates/elena-gateway/src/routes/ws.rs`. Call chain:

1. **`ws_upgrade`** extracts the JWT from `Authorization: Bearer ...` or `?token=...`.
2. **`JwtValidator::verify_to_context`** (in `crates/elena-gateway/src/auth.rs`) decodes the token against the configured `JwtConfig.secret_or_public_key`, validates `exp`/`iss`/`aud`/`nbf`, and projects the claims into a `TenantContext` (from `elena-types`). If invalid → 401.
3. The upgrade completes. Server sends `{"event": "session_started", "thread_id": "..."}` immediately.
4. A `tokio::select!` loop kicks off:
   - Inbound: deserialize each client frame (`ClientFrame` tagged enum) into `send_message`, `abort`, etc.
   - Outbound: subscribe to NATS `elena.thread.{id}.events` (core pub/sub, ephemeral), re-emit each `StreamEvent` as a text frame.
5. On `send_message`, `build_work_request` fills a `WorkRequest` struct (`elena-types::transport::WorkRequest`) with tenant/thread context, autonomy, max_turns, model override, and the user message. Then `publish_work` in `crates/elena-gateway/src/nats.rs` does a JetStream publish to subject `elena.work.incoming` and awaits the ack.

## Chapter 16 — NATS publish → worker claim → `run_loop`

The worker (`crates/elena-worker/src/consumer.rs`) subscribed to the JetStream stream `elena.work.incoming` at boot under durable consumer name `workers`.

1. Consumer pulls the next message, spawns a task (bounded by `max_concurrent_loops` semaphore), calls `dispatch::handle`.
2. **`dispatch::handle`** (in `crates/elena-worker/src/dispatch.rs`):
   - Rate-limit gate: `rate_limiter.try_take(tenant_rpm_key)` + `acquire_inflight(tenant_inflight_key)`. On reject → emit error event, ack, return.
   - **Redis CAS claim**: `store.cache.claim_thread(thread_id, worker_id, ttl_ms)` runs a Lua script that SET-NX the key `thread:claim:{thread_id}`. If another worker holds it, we ack-and-skip (idempotent).
   - Append user message to Postgres `messages` table (idempotent by message id).
   - Build `LoopState::initial(...)` from the `WorkRequest`.
   - Spawn abort listener: subscribe to `elena.thread.{id}.abort`; if received, cancel the inner token.
   - Call `elena_core::run_loop(state, deps, worker_id, inner_cancel)`.
3. **`run_loop`** returns `(JoinHandle<Terminal>, ReceiverStream<StreamEvent>)`.
4. `dispatch::handle` pipes the stream through **`publisher::pump_events`** (in `crates/elena-worker/src/publisher.rs`): each `StreamEvent` is JSON-serialized and published to `elena.thread.{id}.events`. NATS flushes before `pump_events` returns.
5. On completion, release inflight slot; JetStream ack.

## Chapter 17 — The phase state machine

`LoopState.phase` (in `crates/elena-core/src/state.rs`) is the single source of truth. Each call to `step()` (`crates/elena-core/src/step.rs`) performs one transition and returns `StepOutcome`:

- `Continue` — write a checkpoint and call `step()` again.
- `Terminal(Terminal)` — clean exit (Completed, MaxTurns, BlockingLimit, etc.). Drop checkpoint, emit `Done`, release claim.
- `Paused` — awaiting approval; leave checkpoint in place, release claim, return without a `Done` event. A later approval POST republishes a `WorkRequest{kind: ResumeFromApproval}` which any worker can pick up.

The phases:

### 17.1 Received → Streaming (`handle_received`)

First step of a fresh thread. If `ContextManager.retrieval_enabled()`, calls `EpisodicMemory::recall` and prepends summaries to the system prompt. Transitions to `Streaming`.

### 17.2 Streaming → ... (`handle_streaming`, `step.rs:341–472`)

Meat of the loop.

1. `router.route(ctx)` picks a tier (Fast/Standard/Premium) unless already overridden by cascade.
2. `router.resolve(tier)` returns `(provider_name, model_id)`.
3. `context.build_context(tenant_id, thread_id, query, store, budget)` retrieves + packs the context window (embedding similarity + recency tail + token budget with 10% headroom, tail-preserving).
4. `filter_schemas_for_tenant(...)` intersects:
   - ownership-visible plugins (a plugin with no `plugin_ownerships` rows is globally visible; otherwise only its owners can see it),
   - tenant allow-list (`tenants.allowed_plugin_ids`, empty = all),
   - workspace allow-list (`workspaces.allowed_plugin_ids`, empty = all).
   Non-plugin built-in tools (name without `{plugin_id}_` prefix) are always visible.
5. `build_llm_request(state, messages, schemas)` builds an `LlmRequest` (provider, model, system prompt, messages, tools, max_tokens, cache policy).
6. `llm.stream(req, cache_policy, cancel)` returns a `BoxStream<StreamEvent>`.
7. `consume_stream(stream)` drains it into `ConsumedStream` (assembled message, extracted `ToolInvocation`s, usage).
8. **Cascade check** (`router.cascade_check`): if the stream was a refusal, hallucination (tool-call to unregistered tool), empty turn, or too-brief-when-tools-available, escalate one tier and re-enter `Streaming` without persisting. Hard cap: 2 escalations per turn.
9. Persist assistant message. Embed for next-turn retrieval.
10. Decide:
    - No tool calls → `PostProcessing`.
    - `requires_approval(autonomy, invocations)` true → `AwaitingApproval` and set `deadline_ms = now + 300_000`. Emit `awaiting_approval` event, **park** (return `Paused`).
    - Otherwise → `ExecutingTools { remaining: invocations }`.

### 17.3 ExecutingTools (`handle_executing_tools`, `step.rs:612–766`)

The idempotency guard (SCRUM-103) fires here:

1. `prior_tool_execution_index(store, thread_id)` scans persisted messages, returning:
   - `ids: HashSet<ToolCallId>` — every call that already has a `ToolResult` block persisted.
   - `succeeded_fingerprints: HashSet<String>` — `(tool_name, canonical_input)` for previously-successful calls. `canonicalize()` recursively sorts object keys so `{a:1,b:2}` and `{b:2,a:1}` fingerprint identically.
2. Partition `remaining` into `dupes` (id match OR fingerprint match) and `fresh`. Log a warn for each dupe.
3. If all dupes: set `last_turn_had_tools = false`, transition to `PostProcessing`. PostProcessing will then go to `Completed` (not `Streaming`) — this is SCRUM-103.3, the terminate-on-all-dupes fix.
4. For each fresh call: look up the plugin id from tool name prefix; fetch tenant credentials for that plugin via `TenantCredentialsStore.get_decrypted`; stash in `PerCallCredentials` map.
5. `execute_batch(fresh, registry, tenant, thread_id, cancel, progress_tx, options)` in `elena-tools::orchestrate`:
   - Classify each call by `tool.is_concurrent_safe(input)`.
   - Run consecutive concurrent-safe calls in parallel under a semaphore (default max 10); flush at the first non-concurrent-safe call, run it alone.
   - Result order matches input order.
   - Cancelled calls return `ToolError::Execution("aborted")`.
6. For each result: write `ToolResult` content block to Postgres; emit `tool_result` event; write audit row.
7. Transition to `PostProcessing`.

### 17.4 AwaitingApproval (`handle_awaiting_approval`, `step.rs:107–213`)

This handler only runs when `run_loop` is *resumed* after an approval POST.

1. Check `approval_deadline_lapsed(now_ms, deadline_ms)`. If yes → `Terminal::BlockingLimit`.
2. Read approval rows from `thread_approvals`. If count < pending, re-park (client hasn't sent all decisions yet).
3. Pair decisions with invocations by `tool_use_id`. For each `Deny`, synthesize a `ToolResult` message with a denial string. For each `Allow`, apply `edits` if any.
4. Clear approval rows.
5. If all denied → `PostProcessing`. Otherwise → `ExecutingTools` with allowed calls.

### 17.5 PostProcessing (`handle_post_processing`, `step.rs:768–814`)

Bookkeeping.

1. Best-effort: update `budget_state` in Postgres with cumulative usage.
2. If `usage > budget.max_tokens_per_thread` → `Terminal::BlockingLimit`.
3. If `turn_count >= max_turns` → `Terminal::MaxTurns`.
4. Reset `recovery.consecutive_errors = 0`.
5. If `last_turn_had_tools` is true: set it to false and → `Streaming` (so the model sees tool results and can compose a reply).
6. Otherwise → `Completed`.

### 17.6 Completed / Failed

`run_loop`'s outer driver (`crates/elena-core/src/loop_driver.rs::drive_loop`) handles the Terminal cases:

- Drop checkpoint, release claim, emit `Done{reason}`.
- Fire-and-forget `spawn_episode_record()`: summarize thread, embed, store in `episodes`.

## Chapter 18 — LLM streaming: Anthropic and OpenAI-compatible

The `LlmClient` trait (`crates/elena-llm/src/provider.rs`):

```rust
pub trait LlmClient: Send + Sync {
    fn provider(&self) -> &str;
    fn stream(&self, req: LlmRequest, policy: CachePolicy, cancel: CancellationToken)
        -> BoxStream<'static, Result<StreamEvent, ElenaError>>;
}
```

Two implementations:

- **`AnthropicClient`** (`anthropic.rs`): POSTs to `/v1/messages` with `stream: true`, applies `cache_control: { type: "ephemeral" }` markers per the `CachePolicy`, parses Anthropic's SSE events (`message_start`, `content_block_start`, `content_block_delta`, `message_delta`, `message_stop`) via `StreamAssembler` and emits Elena `StreamEvent`s.
- **`OpenAiCompatClient`** (`openai_compat.rs`): POSTs to `/chat/completions` with `stream: true` and a translated body. Adapts tool-call deltas (`choice.delta.tool_calls[i].function.name/arguments`) to Elena's `ToolUseStart`/`ToolUseInputDelta`/`ToolUseComplete` events. No prompt-cache headers (not supported by OpenRouter/Groq).

Both share:
- **`SseExtractor`** (`sse.rs`) — incremental, spec-compliant SSE frame parser.
- **`decide_retry`** (`retry.rs`) — classifies HTTP status → retry with exponential backoff for 5xx/timeouts, fail-fast on 4xx.

**`LlmMultiplexer`** (`multiplexer.rs`) holds a `HashMap<String, Arc<dyn LlmClient>>` + default provider name. It is itself an `LlmClient` — when `stream()` is called, it reads `req.provider`, dispatches to the matching client, else falls back to default.

At boot (`bins/elena-server/src/main.rs::build_llm`), each configured provider (Anthropic, Groq, OpenRouter) is registered. Current prod: OpenRouter via `openai/gpt-oss-120b:free`.

## Chapter 19 — Dispatch decision: autonomy × tool policy

`crates/elena-core/src/dispatch_decision.rs`:

```rust
pub fn requires_approval(autonomy: AutonomyMode, invocations: &[ToolInvocation]) -> bool {
    match autonomy {
        AutonomyMode::Yolo => false,
        AutonomyMode::Cautious => !invocations.is_empty(),
        AutonomyMode::Moderate => invocations.iter().any(|i| is_decision_fork(&i.name)),
    }
}
```

`is_decision_fork(name)` is a heuristic. Read-only verbs — `list`, `get`, `read`, `search`, `query`, `find`, `fetch`, `describe`, `lookup`, `show`, `view`, `summarize`, `count` — and their `list_*` / `get_*` prefix variants return `false` (not a fork, auto-run). Everything else (including unknown verbs) returns `true` (pause).

Effective matrix:

| Autonomy | `slack_list_channels` | `slack_post_message` |
|---|---|---|
| Yolo | run | run |
| Moderate | run | pause |
| Cautious | pause | pause |

## Chapter 20 — Tool execution: built-ins vs plugin gRPC

The `Tool` trait (`crates/elena-tools/src/tool.rs`):

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &serde_json::Value;
    fn is_read_only(&self, input: &Value) -> bool { false }
    fn is_concurrent_safe(&self, input: &Value) -> bool { self.is_read_only(input) }
    async fn check_permission(&self, input: &Value, p: &PermissionSet) -> Permission { Permission::allow() }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError>;
}
```

Two kinds of implementations:

1. **Built-in Rust tools** — compile-time registration. Today the workspace has none (every user-visible tool is a plugin), but the crate structure supports them (`ToolRegistry::register<T: Tool>(tool)`).

2. **Plugin-backed tools** — `PluginActionTool` in `crates/elena-plugins/src/action_tool.rs`. One per plugin *action*. Its `execute(input, ctx)` builds a gRPC `PluginRequest` (action name, input JSON, `tool_use_id`, a `PluginContext` with tenant_id, per-tenant creds as `x-elena-cred-*` metadata, optional trace headers) and calls the plugin sidecar. The sidecar streams `ProgressUpdate` events (forwarded as `ToolProgress` on `ctx.progress_tx`) and a final `FinalResult`. Result is truncated to 200 KiB (`MAX_TOOL_RESULT_BYTES`) and wrapped in `ToolOutput`.

Plugins expose their actions via the gRPC `ElenaPlugin` service's `GetManifest()` RPC (returns `PluginManifest{plugin_id, name, version, actions: [{name, description, input_schema, is_read_only, ...}]}`). At boot, `PluginRegistry::register(endpoint)` dials the sidecar, fetches the manifest, validates it (including Phase-7 schema-drift check: new schemas cannot narrow on re-registration), and synthesizes a `PluginActionTool` per action into the shared `ToolRegistry`.

Plugin endpoints come from two sources merged at boot (see SCRUM-101):
- `ELENA_PLUGIN_ENDPOINTS=http://a:50071,http://b:50072` — external sidecars.
- `ELENA_EMBEDDED_PLUGINS=slack,notion,sheets,shopify,echo` — in-process sidecars; `elena-embedded-plugins::spawn_embedded` binds `127.0.0.1:0` for each, spawns a tonic Server as a tokio task, returns the loopback URL which is appended to the endpoints list so the registry dials them uniformly.

## Chapter 21 — The idempotency guard in detail

Already covered in 17.3. The important takeaway: Groq/OpenAI-compatible providers sometimes re-propose identical tool calls after seeing a tool result. Without the guard, `ExecutingTools → PostProcessing → Streaming → (LLM proposes same tool) → ExecutingTools → ...` loops until `max_turns`. The guard short-circuits on both ID-match (same `tool_use_id` already has a `ToolResult`) and fingerprint-match (`(name, canonicalize(input))` matches a prior successful call).

When *all* new calls are dupes (meaning the LLM got stuck), we set `last_turn_had_tools = false` so PostProcessing routes straight to `Completed` rather than back to Streaming. This is SCRUM-103.3.

## Chapter 22 — Audit + checkpoint

Two durable writes per turn:

- **Postgres `audit_events`** — one row per interesting event: `tool_use`, `tool_result`, `approval_decision`, `rate_limit_reject`, `llm_call`. Written through `PostgresAuditSink` which pulls from a bounded `mpsc::channel` (cap 10 000). Overflow events increment the `elena_audit_drops_total` metric.
- **Redis checkpoint** — after every `step()` that returns `Continue`, `save_loop_state(cache, state)` stores JSON at key `loop_state:{thread_id}` with TTL. A crashed worker resumes from here; a clean Terminal drops the key.

## Chapter 23 — Termination and resume

- **`Terminal::Completed`** — happy path. Drop checkpoint, emit `Done{reason: "completed"}`, release claim.
- **`Terminal::MaxTurns{turn_count}`** — loop ran out. Same cleanup.
- **`Terminal::BlockingLimit`** — either token budget exceeded, rate limit sustained, or approval deadline lapsed.
- **`Terminal::Aborted{kind}`** — user sent `abort` frame; cancel token fired.
- **`StepOutcome::Paused`** — park for approval. Checkpoint stays, claim is released, no `Done` event. Client POSTs to `/v1/threads/{id}/approvals`; gateway publishes `WorkRequest{kind: ResumeFromApproval}`; any worker claims + hydrates + transitions `AwaitingApproval → ExecutingTools`.

---

# Part IV — Crate-by-crate reference

Order: foundational → agent machinery → transport → glue. Every `.rs` file in every crate is listed.

---

## Chapter 24 — `elena-types`

**Role.** The foundation. Defines every ID type, every error type, every wire envelope, every enum that crosses a process boundary. Zero async, zero infra deps — pure serde + thiserror + ulid + uuid + chrono.

**Public surface** (what other crates `use`): `TenantId`, `UserId`, `WorkspaceId`, `ThreadId`, `MessageId`, `ToolCallId`, `SessionId`, `EpisodeId`, `RequestId`, `PluginId`; `ElenaError`, `LlmApiError`, `ToolError`, `StoreError`, `ConfigError`; `Message`, `MessageKind`, `ContentBlock`, `Role`, `StopReason`; `AutonomyMode`, `ApprovalDecision`, `ApprovalVerdict`, `PendingApproval`; `CacheControl`, `CacheControlKind`, `CacheScope`, `CacheTtl`; `Permission`, `PermissionSet`, `PermissionBehavior`, `PermissionMode`, `PermissionRule`; `Usage`, `CacheCreation`, `ServerToolUse`, `ServiceTier`, `Speed`; `TenantContext`, `TenantTier`, `BudgetLimits`; `Terminal`; `StreamEvent`; `WorkRequest`, `WorkRequestKind`, `subjects`; `ModelId`, `ModelTier`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports every pub item. |
| `src/id.rs` | The `define_id!` macro; ULID-backed branded newtypes (`TenantId`, `UserId`, `ThreadId`, etc.) with `Display`, `FromStr`, `From<Ulid>`, `Into<Uuid>`. `PluginId` is a String newtype (stable across deploys). |
| `src/error.rs` | `ElenaError` + sub-errors (`LlmApiError`, `ToolError`, `StoreError`, `ConfigError`); `is_retryable()` / `kind()` classifiers. Mirrors Claude Code TS `classifyAPIError`. |
| `src/message.rs` | `Message{id, thread_id, tenant_id, parent_id, role, kind, content, token_count, embedding, created_at}`, `Role`, `MessageKind` (Assistant/User/Attachment/Tombstone/ToolUseSummary/System), `ContentBlock` (Text/Image/Thinking/RedactedThinking/ToolUse/ToolResult), `ToolResultContent` (untagged: string or blocks), `ImageSource`, `StopReason`. |
| `src/permission.rs` | Permission system: `PermissionBehavior`, `PermissionMode`, `PermissionRuleSource`, `PermissionRuleValue`, `PermissionSet` (BTreeMap for deterministic serialization), `PermissionRule`, `Permission`, `PermissionDecisionReason`, `PermissionUpdate`. |
| `src/tenant.rs` | `TenantContext`, `TenantTier` (Free/Pro/Team/Enterprise), `BudgetLimits`. Every store method takes `TenantId` from here. |
| `src/usage.rs` | `Usage` struct matching Anthropic's `NonNullableUsage` shape; `CacheCreation`, `ServerToolUse`, `ServiceTier`, `Speed`. |
| `src/cache.rs` | `CacheControl`/`CacheControlKind::Ephemeral`/`CacheTtl::FiveMinutes|OneHour`/`CacheScope::Global|Account`. Prompt-cache markers. |
| `src/autonomy.rs` | `AutonomyMode::{Cautious,Moderate,Yolo}`, `PendingApproval`, `ApprovalDecision`, `ApprovalVerdict`. |
| `src/transport.rs` | `WorkRequest` (NATS message body), `WorkRequestKind`, `subjects::WORK_INCOMING` / `subjects::thread_events(ThreadId)` / `subjects::thread_abort(ThreadId)`. |
| `src/stream.rs` | `StreamEvent` tagged enum — MessageStart / TextDelta / ThinkingDelta / ToolUseStart / ToolUseInputDelta / ToolUseComplete / MessageDelta / AwaitingApproval / LoopEnd / Error / Usage / ToolResult / ToolProgress. The WebSocket wire format. |
| `src/terminal.rs` | `Terminal` enum: Completed, MaxTurns, BlockingLimit, TokenBudgetContinuation, PromptTooLong, ImageError, AbortedStreaming, AbortedTools, StopHookPrevented, HookStopped, ToolTimeout, PermissionDenied, etc. `is_terminal_success()`, `is_retryable()`. |
| `src/model.rs` | `ModelTier::{Fast,Standard,Premium}` with `escalate()`/`deescalate()`, `ModelId` String newtype. |
| `src/memory.rs` | `Outcome::{Completed,Failed{terminal}}` — episodic memory classification. |

**Who imports it.** Every other crate. Foundation.

**Tests.** Inline `#[cfg(test)] mod tests` in each file; `proptest` generators for IDs, serde round-trips, permission-rule matching.

---

## Chapter 25 — `elena-config`

**Role.** Figment-backed layered config loader. Reads `/etc/elena/elena.toml` → `$ELENA_CONFIG_FILE` → env vars prefixed `ELENA_`, each layer overriding the last, then validates. Credentials wrapped in `secrecy::SecretString` so they cannot accidentally `Serialize` or `Debug`-print.

**Public surface.** `ElenaConfig`, `load()`, `load_with(system, user, read_env)`, `validate()`; plus per-section structs `AnthropicConfig`, `CacheConfig`, `ContextConfig`, `DefaultsConfig`, `TierEntry`, `TierModels`, `LogFormat`, `LoggingConfig`, `PostgresConfig`, `ProvidersConfig`, `OpenAiCompatProviderEntry`, `RateLimitsConfig`, `RedisConfig`, `RouterConfig`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | `ElenaConfig` top-level struct; `load()`, `load_with(...)`; figment jail tests covering layering + env override. |
| `src/postgres.rs` | `PostgresConfig{url, pool_max=20, pool_min=2, connect_timeout_ms=5000, statement_timeout_ms=30000}`. |
| `src/redis.rs` | `RedisConfig{url, pool_max=10, thread_claim_ttl_ms=60000}`. |
| `src/anthropic.rs` | Legacy single-provider `AnthropicConfig{api_key, base_url, api_version, request_timeout_ms, connect_timeout_ms, max_attempts}`. |
| `src/providers.rs` | Phase-6.5 `ProvidersConfig{default, anthropic, openrouter, groq}`; `OpenAiCompatProviderEntry{api_key, base_url, organization}`. |
| `src/cache.rs` | Prompt-cache allowlist (patterns; trailing `*` = prefix match). |
| `src/rate_limits.rs` | `RateLimitsConfig` — all unlimited by default. Helpers `effective_tenant_burst()`, `is_unlimited()`. |
| `src/logging.rs` | `LoggingConfig{level, format: Pretty|Json}`. |
| `src/defaults.rs` | Per-tier budgets + tier-to-model map (`TierModels: BTreeMap<ModelTier, ModelId>`). |
| `src/context.rs` | `ContextConfig{embedding_model_path, tokenizer_path, recency_window, retrieval_top_k, summarize_turn_threshold}`. |
| `src/router.rs` | `RouterConfig{max_cascade_escalations}` and related router knobs. |
| `src/validate.rs` | Cross-field checks (URL schemes, tier consistency, provider URL well-formedness). |

**Who imports it.** `elena-store` (pool config), `bins/elena-server` (top-level config load), tests.

---

## Chapter 26 — `elena-auth`

**Role.** Three separable concerns: (1) `SecretProvider` trait with env/Vault/KMS backends; (2) TLS/mTLS rustls config builders; (3) JWKS-backed JWT validation with periodic refresh.

**Public surface.** `SecretProvider`, `SecretRef`, `SecretError`, `EnvSecretProvider`, `CachedSecret`; `TlsConfig`, `TlsError`, `build_client_config`, `build_server_config`; `JwksValidator`, `JwksValidatorConfig`, `JwksError`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Module re-exports. |
| `src/secret.rs` | `SecretProvider` async trait; `EnvSecretProvider` (the v1.0 path); `CachedSecret<T>` with TTL; `EnvSource` trait + `ProcessEnv` + `InMemoryEnv` (tests). Vault/KMS providers are behind `vault` / `kms` cargo features. |
| `src/jwks.rs` | `JwksValidator` spawns a refresh task; holds `RwLock<KeySet>`. `verify(token) → Result<Claims, JwksError>`. `JwksValidator::static_key(decoding_key)` for non-refreshing HS256. |
| `src/tls.rs` | `TlsConfig{cert_file, key_file, client_ca_file}`; `build_client_config(cfg)` and `build_server_config(cfg)` → rustls configs. mTLS is enabled whenever `client_ca_file` is set. |

**Who imports it.** `elena-gateway` (JWT + TLS), `elena-worker` (mTLS to plugins), connectors (TLS).

---

## Chapter 27 — `elena-observability`

**Role.** Prometheus metrics, OpenTelemetry OTLP traces, W3C trace-context propagation. Provides `LoopMetrics` (handles thread through to every metric increment in the loop) and `TraceMeta` (carrier serialized onto every `WorkRequest`).

**Public surface.** `OtelConfig`, `LoopMetrics`, `TraceMeta`, `TelemetryGuard`, `init_tracing`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/metrics.rs` | `LoopMetrics{turns_total, turn_duration_seconds, tool_calls_total, llm_tokens_total, plugin_rpc_duration_seconds, rate_limit_rejections_total, loops_in_flight, audit_drops_total}` all Arc-wrapped inside. Constructor `new()` builds a fresh `Registry`; `with_registry(Arc<Registry>)` shares one. Renders Prometheus text. |
| `src/trace.rs` | `TraceMeta{traceparent, tracestate}`, `from_headers`, `looks_valid`. |
| `src/config.rs` | `OtelConfig{endpoint, service_name, service_version, deployment_environment, protocol, sample_ratio}`. `Protocol::{Grpc,Http}`. |
| `src/tracing_init.rs` | `init_tracing(cfg, default_filter) → TelemetryGuard`. `TelemetryGuard` flushes OTel batch on drop. |

**Who imports it.** `bins/elena-server` (init + `/metrics` endpoint), `elena-core` (turn metrics), `elena-worker` (trace propagation).

---

## Chapter 28 — `elena-store`

**Role.** Persistence façade — Postgres for durable state, Redis for hot state. Every method takes `TenantId` and enforces isolation at SQL. Audit writes go through a bounded mpsc. Credentials are AES-256-GCM at rest with a master key.

**Public surface.** `Store` (the façade); `ApprovalsStore`, `AuditEvent`, `AuditSink`, `DropCallback`, `NullAuditSink`, `PostgresAuditSink`, `AUDIT_CHANNEL_CAP`, `SessionCache`, `Episode`, `EpisodeStore`, `PluginOwnershipStore`, `RateDecision`, `RateLimiter`, `TenantRecord`, `TenantStore`, `TenantCredentialsStore`, `ThreadStore`, `WorkspaceRecord`, `WorkspaceStore`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | `Store` struct, `Store::connect(cfg)`, `connect_with_audit_drops(cfg, on_drop)`, `run_migrations()`, `with_audit()`, `with_tenant_credentials()`. |
| `src/thread.rs` | `ThreadStore` — create_thread, append_message (idempotent by id), load_thread, list_threads, get_message. Enforces tenant isolation; cross-tenant reads → `StoreError::TenantMismatch`. |
| `src/tenant.rs` | `TenantRecord`, `BudgetState`, `TenantStore::{upsert_tenant, get_tenant, update_budget_state, reset_budget_daily}`. |
| `src/cache.rs` | `SessionCache` — Redis claim/release/refresh via embedded Lua scripts (CAS by worker_id). |
| `src/episode.rs` | `Episode`, `EpisodeStore::{insert_episode, similar_episodes}` (pgvector cosine HNSW). |
| `src/audit.rs` | `AuditEvent`, `AuditSink` trait, `PostgresAuditSink` (mpsc drain task), `NullAuditSink`, `AUDIT_CHANNEL_CAP = 10_000`, `DropCallback`. |
| `src/rate_limit.rs` | `RateLimiter` — Redis Lua token-bucket + inflight counter; `RateDecision::{Allow,Reject{retry_after_ms}}`; key builders. |
| `src/approvals.rs` | `ApprovalsStore` — write/get/clear approval decisions (PK: thread_id + tool_use_id). |
| `src/workspace.rs` | `WorkspaceRecord`, `WorkspaceStore` — create_or_upsert, get, set_global_instructions, set_allowed_plugins. |
| `src/plugin_ownership.rs` | `PluginOwnershipStore::{set_owners, get_owners, get_plugins_for_tenant}`. Replace-all semantics. |
| `src/tenant_credentials.rs` | `TenantCredentialsStore` — AES-256-GCM encryption; `put`, `get_decrypted`; `from_env_key(pool, env_var)` loads master key from base64 env. |
| `src/sql_error.rs` | `classify_sqlx(sqlx::Error) → StoreError` and `classify_serde(serde_json::Error) → StoreError`. Crate-boundary error stringification. |
| `src/pg.rs` | Internal — Postgres pool construction. |
| `src/redis.rs` | Internal — `fred::Pool` construction. |

**Migrations** — see Chapter 44.

**Tests.** `tests/smoke.rs` (CRUD + tenant isolation), `tests/phase7_blockers.rs` (approvals, workspace, ownership, credentials).

---

## Chapter 29 — `elena-llm`

**Role.** Streaming LLM client abstraction. Supports Anthropic Messages API and OpenAI-compatible endpoints. Handles SSE, retry, cache markers, usage, cancellation.

**Public surface.** `LlmClient` trait; `AnthropicClient`, `OpenAiCompatClient`, `LlmMultiplexer`; `LlmRequest`, `RequestOptions`, `Thinking`, `ToolChoice`, `ToolSchema`; `CachePolicy`, `CacheAllowlist`; `RetryPolicy`, `RetryDecision`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/provider.rs` | `LlmClient` trait (object-safe). |
| `src/multiplexer.rs` | `LlmMultiplexer` — HashMap<String, Arc<dyn LlmClient>> + default; itself implements `LlmClient`. |
| `src/anthropic.rs` | `AnthropicClient`, `AnthropicAuth::ApiKey(SecretString)`; SSE streaming, retry, cache-marker placement. |
| `src/openai_compat.rs` | `OpenAiCompatClient` — OpenAI chat-completions adapter; tool-call delta translation; no cache markers. |
| `src/sse.rs` | `SseExtractor` — spec-compliant incremental frame parser. Handles LF/CRLF/CR, multi-line data, comments. |
| `src/request.rs` | `LlmRequest`, `RequestOptions`, `Thinking`, `ToolChoice`. |
| `src/wire.rs` | Anthropic wire-format builder, `beta_headers()`. |
| `src/cache.rs` | `CachePolicy`, `CacheAllowlist` — decides which blocks get `cache_control`. |
| `src/retry.rs` | `RetryPolicy`, `RetryDecision`, `classify_http`, `decide_retry`. |
| `src/assembler.rs` | `StreamAssembler` — accumulates Anthropic SSE events into `StreamEvent`. |
| `src/events.rs` | `AnthropicEvent` serde types (message_start, content_block_*, message_delta, message_stop). |

**Who imports it.** `elena-core` (loop calls `.stream()`), `bins/elena-server` (builds multiplexer).

---

## Chapter 30 — `elena-tools`

**Role.** Tool trait + registry + batched orchestrator.

**Public surface.** `Tool`, `ToolInvocation`, `ToolOutput`, `ToolContext`, `ToolRegistry`, `execute_batch`, `ExecuteBatchOptions`, `PerCallCredentials`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/tool.rs` | `Tool` async trait; `ToolInvocation{id, name, input}`; `ToolOutput{content, is_error}` + `text()`/`error()` constructors. |
| `src/registry.rs` | `ToolRegistry` — `Arc<DashMap<String, Arc<dyn Tool>>>`; `register`, `get`, `schemas() → Vec<ToolSchema>`. |
| `src/orchestrate.rs` | `execute_batch(calls, registry, tenant, thread_id, cancel, progress_tx, options) → Vec<(id, result)>`; concurrent-safe partitioning under semaphore; `PerCallCredentials = HashMap<ToolCallId, BTreeMap<String, String>>`; `ExecuteBatchOptions{max_concurrent=10, credentials}`. |
| `src/context.rs` | `ToolContext{tenant, thread_id, cancel, progress_tx}`. |

**Who imports it.** `elena-core` (handle_executing_tools), `elena-plugins` (PluginActionTool implements Tool).

---

## Chapter 31 — `elena-context`

**Role.** Embedding-based retrieval, token-aware packing, long-session summarization. Pluggable `Embedder`.

**Public surface.** `ContextManager`, `ContextManagerOptions`, `Embedder`, `NullEmbedder`, `FakeEmbedder`, `OnnxEmbedder`, `EMBED_DIM`, `pack`, `Summarizer`, `summary_message`, `TokenCounter`, `ContextError`, `EmbedError`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/context_manager.rs` | `ContextManager::build_context(tenant, thread, query, store, budget)` (retrieval if embedder.dim > 0, else recency fallback). `embed_and_store` on each assistant message. |
| `src/embedder.rs` | `Embedder` trait, `NullEmbedder` (no-op, dim=0), `FakeEmbedder` (deterministic hash, dim=384 for tests). `EMBED_DIM=384`. |
| `src/onnx.rs` | `OnnxEmbedder` — loads an ONNX model (e.g., e5-small-v2), tokenizes via `tokenizers` crate. Production path. |
| `src/packer.rs` | `pack(messages, counter, budget) → Vec<Message>`. `HEADROOM_RATIO=0.10`, `TAIL_KEEP=4`. |
| `src/tokens.rs` | `TokenCounter` — chars/4.5 approximation. |
| `src/summarize.rs` | `Summarizer` trait + synthetic summary message constructor. |
| `src/error.rs` | `ContextError`, `EmbedError`. |

---

## Chapter 32 — `elena-memory`

**Role.** Workspace-scoped episodic memory. Records thread summaries on completion; recalls similar past episodes when a new thread starts.

**Public surface.** `EpisodicMemory`, `Summary`, `extract_summary`, `MAX_SUMMARY_LEN`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/memory.rs` | `EpisodicMemory::{record_episode, recall}`. `record_episode` is fire-and-forget from `loop_driver::spawn_episode_record`. |
| `src/summary.rs` | `extract_summary(messages) → Summary{task_summary, actions}`; `MAX_SUMMARY_LEN`. |

---

## Chapter 33 — `elena-router`

**Role.** Tier model router + reactive cascade escalation + circuit breaker per provider.

**Public surface.** `ModelRouter`, `RoutingContext`, `CascadeInputs`, `CascadeDecision`, `CircuitBreaker`, `CircuitState`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/router.rs` | `ModelRouter::{new(tier_models, max_cascade_escalations), route(ctx), cascade_check(inputs), resolve(tier) → TierEntry, tier_models()}`. |
| `src/rules.rs` | `RoutingContext{user_message, conversation_depth, tools_available, recent_tool_names, tenant_tier, error_recovery_count}`; deterministic heuristics picking Fast/Standard/Premium. |
| `src/cascade.rs` | `CascadeInputs`, `CascadeDecision::{Accept, Escalate(tier)}`, `cascade_check(inp)`. Escalates on refusal, hallucination (tool not in registry), empty turn, brevity-with-tools. |
| `src/failover.rs` | `CircuitBreaker` — Closed/Open/HalfOpen state machine; atomic counters; `allow()`, `record_success`, `record_failure`. |

---

## Chapter 34 — `elena-core`

**Role.** The agent loop. See Part III for the narrative walk.

**Public surface.** `LoopState`, `LoopPhase`, `RecoveryState`, `LoopDeps`, `step`, `run_loop`, `load_loop_state`, `save_loop_state`, `drop_loop_state`, `build_llm_request`, `consume_stream`, `requires_approval`, `is_decision_fork`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/state.rs` | `LoopState{thread_id, tenant, phase, pending_tool_calls, last_turn_had_tools, autonomy, model_tier, recovery, turn_count, usage, ...}`; `LoopPhase` enum; `RecoveryState{consecutive_errors, context_rebuilds, max_output_retries, model_escalations}`. |
| `src/step.rs` | Phase dispatcher. All phase handlers. Idempotency guard (`prior_tool_execution_index`, `tool_call_fingerprint`, `canonicalize`). `filter_schemas_for_tenant`. `pending_from_invocations`, `summarize_input`, `brief_value`. SCRUM-103 logic at ~line 619–664; `APPROVAL_DEADLINE_SECS=300`. Extensive unit tests (Unicode, deep nesting, fingerprint normalization, approval deadlines). |
| `src/loop_driver.rs` | `run_loop(initial, deps, worker_id, cancel) → (JoinHandle, ReceiverStream)`; `drive_loop` — claim, resume, step, checkpoint, release claim, emit Done. `spawn_episode_record`. Classifies ElenaError → Terminal. |
| `src/dispatch_decision.rs` | `requires_approval(autonomy, invocations)`, `is_decision_fork(name)`. |
| `src/checkpoint.rs` | `save_loop_state`, `load_loop_state`, `drop_loop_state`. |
| `src/context_builder.rs` | Helper to fetch recent messages (called by `request_builder`). |
| `src/request_builder.rs` | `build_llm_request(state, messages, schemas) → LlmRequest`. Applies cache markers. |
| `src/stream_consumer.rs` | `consume_stream(stream) → ConsumedStream{assistant_message, tool_invocations, usage}`. |
| `src/deps.rs` | `LoopDeps` struct. |

**Tests.** `tests/phase4_loop.rs` (end-to-end), `tests/round_trip.rs` (state serde).

---

## Chapter 35 — `elena-plugins`

**Role.** gRPC plugin protocol, manifest validation, registry that synthesizes `Tool` per action.

**Public surface.** `PluginActionTool`, `PluginClient`, `PluginsConfig`, `PluginError`, `HealthMonitor`, `HealthState`, `PluginId`, `PluginIdError`, `ActionDefinition`, `PluginManifest`, `PluginRegistry`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/id.rs` | `PluginId` newtype (alphanumeric + hyphen). |
| `src/manifest.rs` | `PluginManifest`, `ActionDefinition`, `from_wire` validation (non-empty id/name/version, ≥1 action, unique names, schemas parse). |
| `src/config.rs` | `PluginsConfig{endpoints, connect_timeout_ms, execute_timeout_ms, health_interval_ms, tls}`. |
| `src/client.rs` | `PluginClient` — tonic Channel with HTTP/2 keepalive (30s) + TCP keepalive (20s); `connect`, `connect_with_tls`, `rpc()`. |
| `src/registry.rs` | `PluginRegistry` (DashMaps of clients, manifests, health gauges); `register(endpoint)`, `register_all(cfg)`, `manifests()`, `health(id)`, `shutdown()`. Schema-drift check on re-registration. |
| `src/action_tool.rs` | `PluginActionTool` implements `Tool`. Builds gRPC `PluginRequest` with action, input JSON, tool_use_id, tenant context, `x-elena-cred-*` metadata. Streams `ProgressUpdate` → `ToolProgress`. Collects `FinalResult`. Truncates to 200 KiB. Maps `tonic::Code::Unavailable`/`DeadlineExceeded` to health-down. |
| `src/health.rs` | `HealthMonitor` background task; `HealthState`; constants `HEALTH_UP=1`, `HEALTH_DOWN=0`, `HEALTH_UNKNOWN=255`. |
| `src/error.rs` | `PluginError` enum. |
| `src/proto.rs` | tonic-generated gRPC client+server stubs (from proto files). |

---

## Chapter 36 — `elena-embedded-plugins`

**Role.** Spawn plugin connectors as in-process tonic servers on loopback ports. Added in SCRUM-101.

**Public surface.** `enabled_from_env() → Vec<String>`, `spawn_embedded(enabled) → Result<Vec<String>>`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | `enabled_from_env()` parses `ELENA_EMBEDDED_PLUGINS` (comma, trim, lowercase, dedupe). `spawn_embedded(enabled)` calls `spawn_one(id)` per entry. `spawn_one(id)` binds `127.0.0.1:0`, matches on id (`echo`, `slack`, `notion`, `sheets`, `shopify`) to construct the right connector via `<Connector>::from_env()`, wraps in `ElenaPluginServer::new`, `tokio::spawn`s `Server::builder().add_service(...).serve_with_incoming(...)`, returns `http://127.0.0.1:{port}`. Unknown id → hard-fail at boot (surfaces typos early). |

**Who imports it.** `bins/elena-server` only.

---

## Chapter 37 — `elena-admin`

**Role.** Admin-only HTTP routes mounted under `/admin/v1`. Gated by optional `X-Elena-Admin-Token` header.

**Public surface.** `admin_router`, `AdminState`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/state.rs` | `AdminState{store, nats, admin_token, plugins: Option<Arc<PluginRegistry>>}`; `with_admin_token()`, `with_plugins()` builders. |
| `src/router.rs` | `admin_router(state)` — mounts all routes, applies `require_admin_token` middleware. |
| `src/auth.rs` | `require_admin_token` middleware. |
| `src/tenants.rs` | `POST /tenants`, `GET /tenants/{id}`, `PATCH /tenants/{id}/budget`, `PATCH /tenants/{id}/allowed-plugins`. |
| `src/tenant_credentials.rs` | `PUT /tenants/{tid}/credentials/{pid}` (AES-GCM encrypt + upsert; 503 if master key missing), `DELETE /tenants/{tid}/credentials/{pid}` (idempotent). |
| `src/workspaces.rs` | `POST /workspaces`, `GET /workspaces/{id}`, `PATCH /workspaces/{id}/instructions`, `PATCH /workspaces/{id}/allowed-plugins`. |
| `src/plugins.rs` | `GET /plugins` — returns registered manifests (plugin_id, name, version, actions, action_count). 503 if registry not attached. Added in SCRUM-101. `PUT /plugins/{pid}/owners` — replace-all ownership set (empty = globally visible). |
| `src/health.rs` | `GET /health/deep` — synchronous Postgres + Redis + NATS + plugin health probes. |

---

## Chapter 38 — `elena-gateway`

**Role.** Public HTTP + WebSocket API. axum-based. Serves `/v1/*` (JWT-auth) + `/admin/v1/*` (admin-token) + `/metrics` + `/health` + `/version`.

**Public surface.** `GatewayState`, `GatewayConfig`, `JwtConfig`, `JwtAlgorithm`, `JwtValidator`, `ElenaJwtClaims`, `AuthedTenant`, `GatewayError`, `build_router`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports. |
| `src/main.rs` | Standalone gateway binary (also callable as a separate image). Usually not used in prod; `bins/elena-server` is the unified process. |
| `src/app.rs` | `GatewayState` (JWT validator, store, NATS client + JetStream context, metrics, optional admin token + plugin registry); `build_router(state)` assembles axum routes + admin sub-router. |
| `src/auth.rs` | `ElenaJwtClaims`, `JwtValidator` (stateless), `AuthedTenant` (axum extractor → `TenantContext`). URL-decode helper. Extensive tests. |
| `src/nats.rs` | `connect(url)`; `bootstrap_work_stream(jet)` idempotent stream creation; `publish_work(jet, req)` JetStream publish with ack. |
| `src/config.rs` | `GatewayConfig`, `JwtConfig`, `JwtAlgorithm` (HS256/384/512, RS256/384/512, ES256/384), helpers. |
| `src/error.rs` | `GatewayError` enum + `IntoResponse`. |
| `src/routes/mod.rs` | Submodule re-exports. |
| `src/routes/health.rs` | `GET /health` (`{"ok": true}`), `GET /version`. |
| `src/routes/metrics.rs` | `GET /metrics` — Prometheus text, `Content-Type: text/plain; version=0.0.4`. |
| `src/routes/threads.rs` | `POST /v1/threads` — create thread row, return 201 + `{thread_id}`. |
| `src/routes/approvals.rs` | `POST /v1/threads/{id}/approvals` — persist decisions, republish `WorkRequest{kind: ResumeFromApproval}`. |
| `src/routes/ws.rs` | `GET /v1/threads/{id}/stream` (WebSocket upgrade). `ClientFrame`/`GatewayFrame` tagged enums. `tokio::select!` loop: inbound client frames → JetStream publish; NATS `thread.{id}.events` → outbound text frames. 500 ms per-send timeout (drop slow clients). Injects workspace `global_instructions` as a system block when building the WorkRequest. Extensive tests. |

---

## Chapter 39 — `elena-worker`

**Role.** NATS JetStream consumer that dispatches `WorkRequest`s into `elena_core::run_loop`.

**Public surface.** `WorkerConfig`, `WorkerError`, `run_worker`.

**Files.**

| File | Purpose |
|---|---|
| `src/lib.rs` | Re-exports, `run_worker` entry. |
| `src/main.rs` | Standalone worker binary. `bins/elena-server` spawns the same logic inside a shared runtime. |
| `src/config.rs` | `WorkerConfig{nats_url, worker_id, max_concurrent_loops, durable_name, stream_name}`; `DEFAULT_DURABLE_NAME = "workers"`. |
| `src/error.rs` | `WorkerError`. |
| `src/consumer.rs` | `run_consumer` — connects, ensures JetStream stream `elena.work.incoming` exists, creates/retrieves durable consumer `workers` (`DeliverPolicy::All`, `max_ack_pending = max_concurrent_loops * 2`, `ack_wait = 120s`). Pulls messages, spawns `dispatch::handle` under semaphore. Graceful shutdown on cancel. |
| `src/dispatch.rs` | `handle(nats, deps, worker_id, request, parent_cancel)` — rate-limit gate, claim thread, append user message (idempotent), build `LoopState`, spawn abort listener, call `run_loop`, pipe events through `pump_events`, release inflight on exit. |
| `src/publisher.rs` | `pump_events(nats, thread_id, stream)` — forwards `StreamEvent`s to `elena.thread.{id}.events`, flushes NATS before returning. |

---

# Part V — Binaries

## Chapter 40 — `bins/elena-server` (unified process)

**One file: `src/main.rs`.** This is the production entry point (Railway image runs this).

**Boot sequence:**

1. `init_tracing()` — `RUST_LOG` filter, JSON or pretty format.
2. `elena_config::load()` → `ElenaConfig`, then `validate()`.
3. `LoopMetrics::new()` wrapped in `Arc`; `DropCallback` connects `audit_drops_total` to the store's audit overflow hook.
4. `Store::connect_with_audit_drops(cfg, on_drop)` → Postgres pool + Redis pool + audit sink + credentials store (if `ELENA_CREDENTIAL_MASTER_KEY` set).
5. `store.run_migrations()` — applies all migrations unconditionally.
6. `build_llm(&cfg)` → `(LlmMultiplexer, TierModels)`. Registers each configured provider (`anthropic`, `groq`, `openrouter`). Default from `cfg.providers.default`. `TierModels` from `[defaults.tier_models]` or derived from `ELENA_DEFAULT_MODEL`.
7. `ToolRegistry::new()` (empty; plugins will fill it).
8. Merge plugin endpoints:
   - external list from `ELENA_PLUGIN_ENDPOINTS` (comma-separated URLs)
   - embedded list: `enabled_from_env()` → `spawn_embedded(&enabled)` returns `Vec<String>` of loopback URLs.
   - concat the two.
9. `PluginRegistry::with_config(tools, &plugins_cfg)`; `plugins.register_all(&plugins_cfg)` — dials each endpoint, fetches manifest, synthesizes `PluginActionTool`s into `tools`. Logs `plugin registered plugin=slack actions=2` etc.
10. `ContextManager::new(Arc::new(NullEmbedder), ContextManagerOptions::default())` — ONNX embedder off by default.
11. `EpisodicMemory::new(Arc::new(store.episodes.clone()))`.
12. `ModelRouter::new(tier_routing, 2)` — cap 2 escalations per turn.
13. Build `LoopDeps` struct.
14. Parse `ELENA_LISTEN_ADDR` (default `0.0.0.0:8080`), bind `TcpListener`.
15. Parse NATS URL (`ELENA_NATS_URL` or `NATS_URL` — required).
16. Build `GatewayConfig` (JWT from `ELENA_JWT_SECRET` or `JWT_HS256_SECRET`, issuer/audience/leeway).
17. `GatewayState::connect(&gw_cfg, store.clone(), metrics.clone())` — attaches NATS, bootstraps work stream.
18. Optional `.with_admin_token(...)` from `ELENA_ADMIN_TOKEN`.
19. `.with_plugins(Arc::clone(&plugins))` so `/admin/v1/plugins` can answer.
20. `build_router(gateway_state)` — assembles routes incl. admin sub-router.
21. `spawn_signal_handler(cancel)` — SIGINT/SIGTERM → cancel token.
22. Spawn worker task with a `WorkerConfig` derived from `ELENA_WORKER_ID`, `ELENA_MAX_CONCURRENT_LOOPS` (default 8), `ELENA_DURABLE_NAME`, `ELENA_STREAM_NAME`.
23. Spawn `axum::serve(listener, app).with_graceful_shutdown(cancel.cancelled_owned())`.
24. `tokio::join!` both.

## Chapter 41 — Plugin connector bins

Each of `bins/elena-connector-{echo,slack,notion,sheets,shopify}` has a single `src/main.rs` that:

1. `init_tracing()`.
2. Parses its own listen address env var (`ELENA_ECHO_ADDR=127.0.0.1:50061`, `ELENA_SLACK_ADDR=127.0.0.1:50071`, etc.).
3. Constructs the connector via `<Connector>::from_env()` which reads plugin-specific secrets.
4. Wraps in `ElenaPluginServer::new(connector)`.
5. `tonic::transport::Server::builder().add_service(svc).serve(addr)`.

**Per-connector env vars:**

| Bin | Secrets read | Default listen |
|---|---|---|
| `elena-connector-echo` | none | `127.0.0.1:50061` |
| `elena-connector-slack` | `SLACK_BOT_TOKEN`, `SLACK_API_BASE_URL` (opt) | `127.0.0.1:50071` |
| `elena-connector-notion` | `NOTION_TOKEN`, `NOTION_API_BASE_URL` (opt) | `127.0.0.1:50072` |
| `elena-connector-sheets` | Google Sheets creds, `SHEETS_API_BASE_URL` (opt) | `127.0.0.1:50073` |
| `elena-connector-shopify` | `SHOPIFY_TOKEN`, `SHOPIFY_API_BASE_URL` (opt) | `127.0.0.1:50074` |

All connectors read `x-elena-cred-*` gRPC metadata at execute time and prefer those values over the env defaults (multi-tenant override).

**Note.** Each connector is also available as a library crate (same cargo package), so `elena-embedded-plugins::spawn_one` can embed it in-process.

## Chapter 42 — End-to-end smoke binaries

Each `bins/elena-*-smoke/src/main.rs` boots the full stack via testcontainers
and exercises one production scenario. CI treats exit code 0 as success and
2 as "skip — missing env vars."

| Bin | Scenario | Required env | Notes |
|---|---|---|---|
| `elena-phase7-smoke` | Production-readiness surface: admin API, `/metrics`, audit, workspace guardrail, cautious-mode approval | none (wiremock fallback); real provider with `GROQ_API_KEY` or `ANTHROPIC_API_KEY` | Testcontainers |
| `elena-hannlys-smoke` | Marketplace: two tenants (creator + buyer), plugin ownership, two-turn Notion page creation | `GROQ_API_KEY`; optional `NOTION_TOKEN`/`NOTION_PARENT_PAGE_ID` | Testcontainers + real Groq + Notion (or wiremock) |
| `elena-solen-smoke` | Hero scenario: single prompt drives Shopify list → Notion page → Slack post → Sheets append | All four services' tokens + `GROQ_API_KEY` + `ELENA_CREDENTIAL_MASTER_KEY` | Testcontainers + 4 live sandbox APIs |

Each exits 2 on missing required env (CI "skip-safe" convention).

---

# Part VI — Storage

## Chapter 44 — Postgres schema

Migrations live in `crates/elena-store/migrations/` and are embedded at compile-time via `sqlx::migrate!("./migrations")`. Applied unconditionally at server boot.

| Timestamp / file | Effect |
|---|---|
| `20260417000001_enable_extensions.sql` | `CREATE EXTENSION vector, pgcrypto`. |
| `20260417000002_tenants.sql` | `tenants` table; `set_updated_at()` trigger function. |
| `20260417000003_threads.sql` | `threads` table + indexes (`threads_tenant_user_recent_idx`, `threads_workspace_idx`, JSON metadata index). |
| `20260417000004_messages.sql` | `messages` table + indexes (`messages_thread_created_idx` chronological, `messages_tenant_created_idx` audit, `messages_embedding_hnsw_idx` vector). |
| `20260417000005_budget_state.sql` | `budget_state` table (tenant PK; tokens_used_today, day_rollover_at, threads_active). |
| `20260417000006_episodes.sql` | `episodes` table + two indexes (recency, HNSW embedding). |
| `20260418000001_thread_approvals.sql` | `thread_approvals` (PK thread_id + tool_use_id). Phase 7 A1. |
| `20260418000002_workspaces.sql` | `workspaces` (global_instructions text, allowed_plugin_ids text[]). Phase 7 A2/A4. |
| `20260418000003_audit_events.sql` | `audit_events` append-only log + two indexes (tenant-time, thread partial). Phase 7 A3. |
| `20260418000004_tenant_allowed_plugins.sql` | `ALTER tenants ADD allowed_plugin_ids text[]`. Phase 7 A4. |
| `20260418000005_plugin_ownerships.sql` | `plugin_ownerships` (PK plugin_id + tenant_id). Phase 7 A5. |
| `20260418000006_tenant_credentials.sql` | `tenant_credentials` (PK tenant_id + plugin_id; kv_ciphertext bytea, kv_nonce bytea). Phase 7 follow-up. |

**Resulting tables** (at a glance):

1. `tenants` — id, name, tier, budget, permissions, metadata, allowed_plugin_ids, timestamps.
2. `threads` — id, tenant_id FK, user_id, workspace_id, title, metadata, timestamps, last_message_at.
3. `messages` — id, thread_id FK, tenant_id (redundant), parent_id FK, role, kind jsonb, content jsonb, token_count, embedding vector(384), created_at.
4. `episodes` — id, tenant_id, workspace_id, task_summary, actions jsonb, outcome jsonb, embedding vector(384), created_at.
5. `budget_state` — tenant_id PK FK, tokens_used_today, day_rollover_at, threads_active, updated_at.
6. `thread_approvals` — thread_id FK, tool_use_id, tenant_id FK, decision, edits jsonb, created_at. PK (thread_id, tool_use_id).
7. `workspaces` — id PK, tenant_id FK, name, global_instructions, allowed_plugin_ids text[], timestamps.
8. `audit_events` — id PK, tenant_id FK, workspace_id nullable, thread_id nullable, actor, kind, payload jsonb, created_at.
9. `plugin_ownerships` — plugin_id, tenant_id FK, created_at. PK (plugin_id, tenant_id). Empty owner set = globally visible.
10. `tenant_credentials` — tenant_id FK, plugin_id, kv_ciphertext bytea, kv_nonce bytea, updated_at. PK (tenant_id, plugin_id).

**Migration convention.** Append-only — never destructive `ALTER`/`DROP`. Phase-7 artifacts coexist with pre-Phase-7 rows (missing workspace row = no override).

## Chapter 45 — Redis keys

| Key pattern | Purpose | TTL |
|---|---|---|
| `thread:claim:{thread_id}` | Worker ownership CAS; value = worker_id | `thread_claim_ttl_ms` (60 s default) |
| `loop_state:{thread_id}` | Checkpointed `LoopState` JSON | Rolling; dropped on clean Terminal |
| `ratelimit:tenant:{tenant_id}:rpm` | Token-bucket (Lua) for tenant RPM | Implicit (per-window) |
| `ratelimit:tenant:{tenant_id}:inflight` | Inflight counter | Per-op |
| `ratelimit:provider:{name}:concurrency` | Inflight counter | Per-op |
| `ratelimit:plugin:{plugin_id}:concurrency` | Inflight counter | Per-op |

All atomic ops are Lua scripts (see `crates/elena-store/src/cache.rs` and `rate_limit.rs`).

## Chapter 46 — NATS topology

One JetStream stream, many core pub/sub subjects.

- **Stream `elena.work.incoming`** — durable work queue. Retention: work queue. Max messages: 1M. Subjects: `elena.work.incoming`.
  - Producer: gateway `publish_work`.
  - Consumer: `workers` durable (queue group), `DeliverPolicy::All`, `max_ack_pending = max_concurrent_loops * 2`, `ack_wait = 120s`.
- **Core subject `elena.thread.{id}.events`** — ephemeral event fanout per thread. Worker publishes `StreamEvent`s; gateway subscribes per WebSocket connection. No stream (clients that reconnect replay from Postgres messages, not NATS).
- **Core subject `elena.thread.{id}.abort`** — ephemeral abort signal. Gateway publishes empty message on `{action: "abort"}`; worker's abort listener task triggers its cancel token.

---

# Part VII — Deployment

## Chapter 47 — Railway (single-process, current production)

`railway.json` at the repo root:

```json
{
  "build": { "builder": "DOCKERFILE", "dockerfilePath": "deploy/docker/Dockerfile.all-in-one" },
  "deploy": {
    "startCommand": "/usr/local/bin/elena-server",
    "restartPolicyType": "ON_FAILURE",
    "restartPolicyMaxRetries": 10,
    "healthcheckPath": "/admin/v1/health/deep",
    "healthcheckTimeout": 30
  }
}
```

**To deploy:** push to the Railway-connected branch. Railway builds the Dockerfile, adds Postgres/Redis/NATS via its catalog plugins (which export `DATABASE_URL`, `REDIS_URL`, `NATS_URL`), then you set:

- `JWT_HS256_SECRET` — HS256 signing secret (or `ELENA_JWT_SECRET`).
- `ELENA_ADMIN_TOKEN` — admin header secret.
- `ELENA_CREDENTIAL_MASTER_KEY` — base64-encoded 32-byte AES-256 key (required if per-tenant creds are used).
- `ELENA_EMBEDDED_PLUGINS=slack` (or `slack,notion,sheets,shopify`).
- `SLACK_BOT_TOKEN=xoxb-...`, `SLACK_API_BASE_URL=https://slack.com/api` (plus Notion/Sheets/Shopify equivalents if embedded).
- `ELENA_PROVIDERS__DEFAULT=openrouter`
- `ELENA_PROVIDERS__OPENROUTER__API_KEY=sk-or-v1-...`
- `ELENA_PROVIDERS__OPENROUTER__BASE_URL=https://openrouter.ai/api/v1`
- `ELENA_DEFAULT_MODEL=openai/gpt-oss-120b:free`
- `ELENA_DEFAULTS__TIER_MODELS__{FAST,STANDARD,PREMIUM}__PROVIDER=openrouter`
- `ELENA_DEFAULTS__TIER_MODELS__{FAST,STANDARD,PREMIUM}__MODEL=openai/gpt-oss-120b:free`

Notice the double-underscore convention: it maps nested TOML keys into env vars. `ELENA_PROVIDERS__OPENROUTER__API_KEY` = `[providers.openrouter] api_key = ...`.

## Chapter 48 — Dockerfile.all-in-one

`deploy/docker/Dockerfile.all-in-one`: two-stage.

- Stage 1 (`rust:1.90-bookworm` builder): apt-get installs `protobuf-compiler`, copies workspace, runs `cargo build --release --bin elena-server`.
- Stage 2 (`gcr.io/distroless/cc-debian12:nonroot`): copies the binary + `crates/elena-store/migrations/` into the image. User `nonroot`. Exposes 8080.

Env defaults in the image: `ELENA_LISTEN_ADDR=0.0.0.0:8080`, `RUST_LOG=info,elena_gateway=info,elena_worker=info`.

## Chapter 49 — Operator runbook

`deploy/RUNBOOK.md` — operator playbook for Railway. Quick-reference scenarios:
1. Mass 429s → check `/metrics`, raise `ELENA_RATE_LIMITS__*` env.
2. Provider outage → swap `ELENA_DEFAULTS__TIER_MODELS__*__PROVIDER` env.
3. DB failover → worker reconnects on next dispatch.
4. Bad deploy → Railway → previous deployment → "Redeploy".
5. NATS corrupt → delete + recreate stream.
6. Redis flap → fred auto-reconnects; 60 s claim TTL releases stuck claims.
7. Audit drops nonzero → investigate worker pressure.
8. JWT secret leak → rotate `ELENA_JWT_SECRET` and redeploy.

SLOs stated: first-token p95 < 1.2 s, turn-completion p95 < 15 s, 99.9 % availability.

`deploy/alerts.yaml` — Prometheus alert rules, including the canary
`ElenaAuditDropsNonzero` (any non-zero `elena_audit_drops_total`).

## Chapter 52 — Env var reference

Merged from CLAUDE.md and SCRUM-101 Railway block.

| Variable | Required | Purpose |
|---|---|---|
| `DATABASE_URL` / `ELENA_POSTGRES__URL` | yes | Postgres connection |
| `REDIS_URL` / `ELENA_REDIS__URL` | yes | Redis connection |
| `NATS_URL` / `ELENA_NATS_URL` | yes | NATS connection |
| `JWT_HS256_SECRET` / `ELENA_JWT_SECRET` | HS256 | HS256 signing key |
| `JWT_JWKS_URL` / `ELENA_JWT_JWKS_URL` | RS256 | Remote JWKS endpoint |
| `ELENA_JWT_ISSUER` | no (default `elena`) | JWT `iss` |
| `ELENA_JWT_AUDIENCE` | no (default `elena-clients`) | JWT `aud` |
| `ELENA_GATEWAY__JWT__LEEWAY_SECONDS` | no (default 60) | Expiry leeway |
| `ELENA_ADMIN_TOKEN` | recommended | `/admin/v1` auth header |
| `ELENA_CREDENTIAL_MASTER_KEY` | recommended | Base64 32-byte AES-256 key. Required for per-tenant creds; absence = env-only single-tenant mode |
| `ELENA_PLUGIN_ENDPOINTS` | optional | Comma-separated external plugin URLs |
| `ELENA_EMBEDDED_PLUGINS` | optional | Comma-separated in-process plugin ids |
| `ELENA_LISTEN_ADDR` | no (default `0.0.0.0:8080`) | HTTP/WS listen |
| `ELENA_WORKER_ID` | no (default `elena-server`) | Worker identity |
| `ELENA_MAX_CONCURRENT_LOOPS` | no (default 8) | Loop concurrency cap |
| `ELENA_DURABLE_NAME` | no | JetStream consumer name override |
| `ELENA_STREAM_NAME` | no | JetStream stream name override |
| `GROQ_API_KEY` / `ELENA_PROVIDERS__GROQ__API_KEY` | per provider | Groq |
| `ANTHROPIC_API_KEY` / `ELENA_PROVIDERS__ANTHROPIC__API_KEY` | per provider | Anthropic |
| `ELENA_PROVIDERS__OPENROUTER__API_KEY` | per provider | OpenRouter |
| `ELENA_PROVIDERS__OPENROUTER__BASE_URL` | OpenRouter | `https://openrouter.ai/api/v1` |
| `ELENA_PROVIDERS__DEFAULT` | yes if multiple | `openrouter` / `groq` / `anthropic` |
| `ELENA_DEFAULT_MODEL` | if tier_models empty | Fallback model id |
| `ELENA_DEFAULTS__TIER_MODELS__{FAST,STANDARD,PREMIUM}__PROVIDER` | with multi-provider | Per-tier provider |
| `ELENA_DEFAULTS__TIER_MODELS__{FAST,STANDARD,PREMIUM}__MODEL` | with multi-provider | Per-tier model |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | optional | OTel collector |
| `RUST_LOG` | optional (default `info`) | Tracing filter |
| `SLACK_BOT_TOKEN` / `SLACK_API_BASE_URL` / `ELENA_SLACK_ADDR` | slack | Slack connector (env-default token) |
| `NOTION_TOKEN` / `NOTION_API_BASE_URL` | notion | Notion connector |
| `SHEETS_TOKEN` / `SHEETS_SPREADSHEET_ID` | sheets | Sheets connector |
| `SHOPIFY_TOKEN` / `SHOPIFY_DOMAIN` / `SHOPIFY_ADMIN_TOKEN` | shopify | Shopify connector |
| `ELENA_ECHO_ADDR` | optional (default `127.0.0.1:50061`) | Echo sidecar addr |

---

# Part VIII — Cookbook

Step-by-step recipes for the common change types. Each lists files in edit order, the tests to add, and how to verify.

## Chapter 53 — Add a new built-in tool

1. Create `crates/elena-tools/src/tools/my_tool.rs` (or a new crate if the tool has heavy deps).
2. Implement the `Tool` trait: `name`, `description`, `input_schema` (JSON schema), `is_read_only`, `execute`.
3. Register it at boot: in `bins/elena-server/src/main.rs`, after `let tools = ToolRegistry::new();`, call `tools.register_arc(Arc::new(MyTool::new()))`.
4. Add unit tests inline in `my_tool.rs`.
5. Verify: `cargo test -p elena-tools`, then `cargo run -p elena-phase7-smoke` with a provider key.

## Chapter 54 — Add a new plugin connector (embedded path, current prod)

1. Create `bins/elena-connector-<name>/` with `Cargo.toml` declaring a library + binary.
2. Implement `<Name>Connector` — a struct with `from_env()` constructor and a gRPC server impl of `ElenaPlugin` (`get_manifest` + `execute`). Pattern copy from `bins/elena-connector-slack/`.
3. The connector should read `x-elena-cred-*` request metadata at `execute` time and fall back to env if absent. See SCRUM-101 for the metadata contract.
4. Wire into `crates/elena-embedded-plugins/src/lib.rs`:
   - Add `<name>` to the `spawn_one` match arm.
   - Add `elena-connector-<name>` as a path dep in `crates/elena-embedded-plugins/Cargo.toml` and in the root workspace deps table.
5. Operator work: set `ELENA_EMBEDDED_PLUGINS=slack,<name>` and any connector-specific env (`<NAME>_TOKEN`, etc.).
6. Per-tenant creds: `PUT /admin/v1/tenants/{tid}/credentials/<name>` with `{"credentials": {"token": "..."}}`.
7. Plugin visibility: `PUT /admin/v1/plugins/<name>/owners` with `[]` (globally visible) or list of tenants.
8. Verify: `GET /admin/v1/plugins` lists the new plugin with its action count. Kick off a smoke.

## Chapter 55 — Add a new LLM provider

1. If the provider is OpenAI-compatible, no code needed — just add to `ProvidersConfig` + register in `bins/elena-server/src/main.rs::build_llm` with `OpenAiCompatClient::new(cfg)`.
2. If it's a custom protocol, implement `LlmClient` in a new `crates/elena-llm/src/<name>.rs`. Required: `provider()`, `stream()`. Reuse `SseExtractor` + `decide_retry` if the upstream uses SSE.
3. Register in `LlmMultiplexer` at boot.
4. Extend `bins/elena-phase7-smoke` (or copy it) to exercise the new provider's streaming path against a wiremock or live key.

## Chapter 56 — Add an admin route

1. Create/edit a module under `crates/elena-admin/src/` (e.g., `my_feature.rs`).
2. Define axum handler fn, state extractor, request/response types.
3. Wire in `crates/elena-admin/src/router.rs` (add the route to `admin_router`).
4. If it needs a new dependency on the registry or store, extend `AdminState` (`state.rs`) and the builder in `elena-gateway::app` that constructs it.
5. Add a test under `crates/elena-admin/tests/` using axum's `Router::oneshot`.
6. Verify: `curl -H "X-Elena-Admin-Token: $ELENA_ADMIN_TOKEN" https://elena.../admin/v1/my-route`.

## Chapter 57 — Add a DB migration

1. New `.sql` file in `crates/elena-store/migrations/` with timestamp prefix (`YYYYMMDDHHMMSS_<name>.sql`).
2. **Additive only** — no `DROP`, no destructive `ALTER`. To "remove" a column, write a new migration that stops writing to it and leave the column in place.
3. If it adds a table, extend `Store` with a new sub-store (e.g., `MyTableStore`) — create a module in `crates/elena-store/src/`, export via `lib.rs`, add field to `Store`.
4. Run `cargo test -p elena-store --test smoke` to confirm testcontainer migration runs.
5. `elena-server` boot will apply migrations on first start. For Railway deploys that's automatic.

## Chapter 58 — Add a Prometheus metric

1. Add a field to `LoopMetrics` in `crates/elena-observability/src/metrics.rs`. Use `IntCounterVec` for labels, `HistogramVec` for timings.
2. Register it in `LoopMetrics::new` via the registry.
3. Thread through to `LoopDeps.metrics` (already an `Arc<LoopMetrics>`) — every phase handler already has access.
4. Increment at the relevant call site in `elena-core` or wherever.
5. Verify: `curl http://localhost:8080/metrics | grep <your_metric>`.

## Chapter 59 — Change loop behaviour (new phase, new terminal, etc.)

1. Add the phase to `LoopPhase` in `crates/elena-core/src/state.rs`.
2. Add the handler fn in `crates/elena-core/src/step.rs` and the match arm in `step()`.
3. Update `StepOutcome` if needed (usually not; reuse `Continue`/`Terminal`/`Paused`).
4. If it needs persisted data, checkpoint it into `LoopState` (remember: serializable).
5. Add phase-level unit tests inline + an integration test under `crates/elena-core/tests/`.

## Chapter 60 — Tune autonomy or approval policy

File: `crates/elena-core/src/dispatch_decision.rs`. Edit `requires_approval` or extend `is_decision_fork` heuristics.

Note: changing `AutonomyMode`'s variant set is a wire-format change — update `elena-types::autonomy`, the BFF wire types in `@solen/shared-types`, and the web-app autonomy selector all in lockstep.

## Chapter 61 — Wire per-tenant credentials for a new connector

1. At the top of the connector's `src/lib.rs`, add `fn resolve_token(req: &Request<_>) -> SecretString` that checks `req.metadata().get("x-elena-cred-token")` first, falls back to env.
2. No Elena-side changes beyond the admin route already in place — `PUT /admin/v1/tenants/{tid}/credentials/{pid}` handles encrypted storage; the worker-side orchestrator in `elena-core::handle_executing_tools` fetches via `TenantCredentialsStore::get_decrypted` and injects into `PerCallCredentials`, which `PluginActionTool::execute` serializes as `x-elena-cred-*` metadata.
3. Smoke-test via `bins/elena-solen-smoke` (which exercises the per-tenant credential injection path end-to-end against Slack/Notion/Sheets/Shopify); copy its harness for your connector.

---

# Part IX — Debugging & ops

## Chapter 62 — Investigating a failing turn

1. Find the thread in the BFF: `SELECT elena_thread_id FROM chats WHERE id = $user_chat_id;`.
2. Query audit: 

    ```sql
    SELECT kind, actor, payload, created_at
      FROM audit_events
     WHERE tenant_id = $tid AND thread_id = $threadid
     ORDER BY created_at;
    ```

3. Look for `tool_use`, `tool_result` (is_error?), `approval_decision`, `rate_limit_reject`, `llm_call` rows.
4. Read `messages` for the same thread to see the full conversation with tool_use + tool_result blocks.
5. Check Redis: `GET loop_state:{thread_id}` — if present, the loop parked; the `phase` field tells you where.
6. Railway logs: `railway logs | grep <thread_id>`. Look for:
    - `claimed thread` / `released claim` — claim churn means workers are fighting.
    - `plugin registered` / `plugins registered count=N` at boot.
    - `skipping duplicate tool invocation` — SCRUM-103 guard fired.
    - `approval deadline lapsed` — parked too long.

## Chapter 63 — Diagnostic endpoints

- `GET /admin/v1/plugins` (admin-token) — which connectors answered `GetManifest()` at boot. If `slack` is missing, the plugin didn't register.
- `GET /admin/v1/health/deep` (admin-token) — Postgres + Redis + NATS + per-plugin health.
- `GET /metrics` (no auth) — Prometheus text. Key counters:
  - `elena_turns_total` — turn count per tenant.
  - `elena_tool_calls_total` — tool count per tenant per tool.
  - `elena_rate_limit_rejections_total` — alert on any non-zero.
  - `elena_audit_drops_total` — alert on any non-zero (audit queue overflow).
  - `elena_plugin_rpc_duration_seconds` — plugin latency histogram.
- BFF diagnostic: `GET /api/v1/elena/me` (user-auth) — returns the user's Elena tenant, workspace, and `plugins_registered` (the raw `/admin/v1/plugins` response). Primary SCRUM-99 diagnostic.

## Chapter 64 — Failure modes from the post-mortem

Lookup table — see `~/Documents/docs/elena-agentic-integration.md` for the full story.

| Symptom | Root cause | Fix location |
|---|---|---|
| All JWTs rejected with `UlidDecodeError` | BFF put a UUID in `user_id` claim; Elena expects ULID | `apps/server-api/src/services/elena/jwt.rs::mint_elena_jwt` (SCRUM-97) |
| 401 from Elena signs user out of web app | BFF passed Elena's 401 through to browser; browser's generic handler ran | `apps/server-api/src/api/elena.rs::forward_to_elena` maps non-2xx → 502 (SCRUM-97) |
| Orphan chat rows, null `elena_thread_id` | Non-atomic chat + thread creation | Transactional `create_chat` in BFF (SCRUM-97) |
| `I'm just an LLM, I can't send Slack messages` | Empty system prompt; LLM refused on training priors | `DEFAULT_GLOBAL_INSTRUCTIONS` in BFF provisioner (SCRUM-100) |
| `"Message sent"` text with no tool_use events | Identity prompt pressured LLM to fabricate; plugin not registered | `ELENA_EMBEDDED_PLUGINS=slack` + embedded plugins crate (SCRUM-101) |
| Slack blank UI during tool execution | Web-app didn't bind `onToolUseStart/Progress/Result` WS callbacks | `apps/web-app/src/hooks/use-websocket.ts` (SCRUM-102) |
| Duplicate Slack posts; `"I don't have access"` after success | LLM re-proposed identical tool call; loop re-ran it; then denied capability | `handle_executing_tools` idempotency guard + `last_turn_had_tools = false` (SCRUM-103) |
| Tool ran but UI shows blank after `done` | Web-app `onDone` only finalized when `streamingText` non-empty | `apps/web-app/src/hooks/use-websocket.ts::summarizeToolTurn` (SCRUM-104) |
| Long "thinking" pauses, eventual success | Groq free tier rate-limit (30 RPM / 14400 TPM) | Switch provider: `ELENA_PROVIDERS__DEFAULT=openrouter`, `ELENA_DEFAULT_MODEL=openai/gpt-oss-120b:free` |

---

# Part X — Pointers & further reading

## Chapter 65 — Repo-internal references

- `CLAUDE.md` — entry point for engineers + Claude Code sessions. Run `cargo fmt && cargo clippy --workspace --all-targets -D warnings && cargo test --workspace` before every commit.
- `.claude/00-architecture-overview.md` through `.claude/10-rust-architecture.md` — deep dives of the **Claude Code TypeScript** codebase that Elena was modeled on. Useful when working on a subsystem; Elena diverges wherever Rust patterns are cleaner, so do not treat these as Elena docs.
  - 00: whole-codebase map
  - 01: the while-true loop
  - 02: LLM streaming + retry
  - 03: tool system + permissions
  - 04: context + compaction
  - 05: service subsystems
  - 06: coordinator + bridge (multi-agent)
  - 07: app state + bootstrap
  - 08: commands + skills
  - 09: Ink TUI
  - 10: Rust crate layout
- `deploy/RUNBOOK.md` — operator playbook.
- `scripts/live-e2e.sh`, `scripts/live-e2e-mintjwt.js`, `scripts/live-e2e-runturn.js` — production-URL validation suite.
- `~/Documents/docs/elena-agentic-integration.md` — SCRUM-85..104 post-mortem. Most of this document's "debugging" section is summarized from there.

## Chapter 66 — External references

- **The Rust Book** — <https://doc.rust-lang.org/book/>. Chapters 1–10 are a good three-day primer. Chapter 16 (concurrency) and Chapter 19 (advanced features) cover what Elena uses.
- **Tokio tutorial** — <https://tokio.rs/tokio/tutorial>. Essential for understanding `async fn` and the runtime.
- **sqlx docs** — <https://docs.rs/sqlx/>. Compile-time-checked SQL.
- **tonic book** — <https://github.com/hyperium/tonic>. gRPC in Rust (the framework that plugins use).
- **axum docs** — <https://docs.rs/axum/>. The HTTP framework the gateway uses.
- **serde docs** — <https://serde.rs/>.
- **Anthropic Messages API** — <https://docs.anthropic.com/en/api/messages>.
- **OpenRouter models** — <https://openrouter.ai/models>. Current production model: `openai/gpt-oss-120b:free`.

---

# Appendix — Crate → file index

Quick lookup of every `.rs` file in the workspace. (Tests in `tests/` directories are listed once per crate.)

**`crates/elena-types/src/`**: lib.rs, id.rs, error.rs, message.rs, permission.rs, tenant.rs, usage.rs, cache.rs, autonomy.rs, transport.rs, stream.rs, terminal.rs, model.rs, memory.rs.

**`crates/elena-config/src/`**: lib.rs, postgres.rs, redis.rs, anthropic.rs, providers.rs, cache.rs, rate_limits.rs, logging.rs, defaults.rs, context.rs, router.rs, validate.rs.

**`crates/elena-auth/src/`**: lib.rs, secret.rs, jwks.rs, tls.rs.

**`crates/elena-observability/src/`**: lib.rs, metrics.rs, trace.rs, config.rs, tracing_init.rs.

**`crates/elena-store/src/`**: lib.rs, thread.rs, tenant.rs, cache.rs, episode.rs, audit.rs, rate_limit.rs, approvals.rs, workspace.rs, plugin_ownership.rs, tenant_credentials.rs, sql_error.rs, pg.rs, redis.rs. Tests: smoke.rs, phase7_blockers.rs.

**`crates/elena-llm/src/`**: lib.rs, provider.rs, multiplexer.rs, anthropic.rs, openai_compat.rs, sse.rs, request.rs, wire.rs, cache.rs, retry.rs, assembler.rs, events.rs.

**`crates/elena-tools/src/`**: lib.rs, tool.rs, registry.rs, orchestrate.rs, context.rs.

**`crates/elena-context/src/`**: lib.rs, context_manager.rs, embedder.rs, onnx.rs, packer.rs, tokens.rs, summarize.rs, error.rs.

**`crates/elena-memory/src/`**: lib.rs, memory.rs, summary.rs.

**`crates/elena-router/src/`**: lib.rs, router.rs, rules.rs, cascade.rs, failover.rs.

**`crates/elena-core/src/`**: lib.rs, state.rs, step.rs, loop_driver.rs, dispatch_decision.rs, checkpoint.rs, context_builder.rs, request_builder.rs, stream_consumer.rs, deps.rs. Tests: phase4_loop.rs, round_trip.rs.

**`crates/elena-plugins/src/`**: lib.rs, id.rs, manifest.rs, config.rs, client.rs, registry.rs, action_tool.rs, health.rs, error.rs, proto.rs.

**`crates/elena-embedded-plugins/src/`**: lib.rs.

**`crates/elena-admin/src/`**: lib.rs, state.rs, router.rs, auth.rs, tenants.rs, tenant_credentials.rs, workspaces.rs, plugins.rs, health.rs.

**`crates/elena-gateway/src/`**: lib.rs, main.rs, app.rs, auth.rs, nats.rs, config.rs, error.rs, routes/mod.rs, routes/health.rs, routes/metrics.rs, routes/threads.rs, routes/approvals.rs, routes/ws.rs.

**`crates/elena-worker/src/`**: lib.rs, main.rs, config.rs, error.rs, consumer.rs, dispatch.rs, publisher.rs.

**Bins (each has `src/main.rs`, plus `src/lib.rs` for connector crates)**:
- `bins/elena-server/src/main.rs`
- `bins/elena-connector-echo/src/{main.rs, lib.rs}`
- `bins/elena-connector-slack/src/{main.rs, lib.rs}`
- `bins/elena-connector-notion/src/{main.rs, lib.rs}`
- `bins/elena-connector-sheets/src/{main.rs, lib.rs}`
- `bins/elena-connector-shopify/src/{main.rs, lib.rs}`
- `bins/elena-phase7-smoke/src/main.rs`
- `bins/elena-hannlys-smoke/src/main.rs`
- `bins/elena-solen-smoke/src/main.rs`

That's 162 Rust source files covering 16 library crates and 16 binary crates, plus 12 SQL migrations.

---

*End of document.* If you find this description is out of date — renamed files, removed types, changed env vars — trust the code, update the doc, and commit.
