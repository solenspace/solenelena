# Elena Architecture

This document describes the **current** runtime contract. The historical
phase numbering scattered across source files (`Phase 3 · Streaming`,
`Phase 7 · A4`, `PHASE5:` markers, etc.) refers to the rollout sequence
the system was built in — useful as archaeology but not load-bearing
to the present design. The [Phase appendix](#appendix-phase-history)
at the end summarises what each phase added.

---

## Runtime in one sketch

```
                                      ┌────────────┐
                  ┌─────tool_use─────▶│  plugin    │──HTTP─▶ Slack/Notion/...
                  │                   │  sidecar   │
                  │                   │  (gRPC)    │
client ─WS────▶ gateway ──NATS─▶ worker ─loop─▶ LLM provider
                  ▲                   │            (Anthropic / Groq / ...)
                  │                   │
                  └─events◀──── publisher
                                      │
                                      ├─audit_events──▶ Postgres
                                      ├─checkpoint───▶ Redis
                                      └─messages─────▶ Postgres
```

The agent loop lives in `crates/elena-core`. `step()` is the phase
dispatcher; `loop_driver::run_loop` is the outer wrapper that handles
checkpoint, claim, park-on-approval, and resume.

---

## Crate map

```
crates/
  elena-types             domain types, transport envelopes, autonomy
  elena-config            figment loader (TOML + env, layered)
  elena-store             Postgres + Redis persistence; migrations
  elena-llm               provider trait, SSE streaming, anthropic + openai-compat
  elena-tools             Tool trait, registry, batched orchestrator
  elena-context           context packing, summarisation, pgvector embeddings, episodic recall
  elena-router            tier model routing, circuit breaker, failover
  elena-core              the agent loop: state machine, step driver, dispatch
  elena-gateway           axum HTTP+WS frontend, NATS publisher, X1 fanout
  elena-worker            NATS JetStream consumer, claim CAS, dispatch, audit drain
  elena-plugins           plugin registry, gRPC client, X7 EmbeddedExecutor backend
  elena-observability     Prometheus metrics, OTel pipeline, mTLS helpers
  elena-auth              JWT/JWKS validator, admin-token + tenant-scope middleware, secret providers
  elena-admin             /admin/v1 routes (tenants, plans, workspaces, plugins, credentials)
  elena-embedded-plugins  registers in-process connectors (slack/notion/sheets/shopify/echo)
  elena-connector-echo    in-process echo plugin (reference EmbeddedExecutor)

bins/
  elena-server                unified gateway+worker process (Railway image)
  elena-phase7-smoke          end-to-end smoke (wiremock or real provider + testcontainers)
  elena-hannlys-smoke         marketplace flow E2E (creator + buyer)
  elena-solen-smoke           hero scenario E2E (Slack + Notion + Sheets + Shopify)
  elena-tri-tenant-firetest   multi-tenant saturation harness (Solen + Hannlys + Omnii)
  elena-connector-{slack,notion,sheets,shopify}    gRPC plugin sidecars
```

---

## Loop state machine

`elena-core::state::LoopPhase` enumerates the phases:

- `Received` → `Streaming`: claim the thread; route the request to a
  model tier.
- `Streaming` → `ExecutingTools` | `PostProcessing`: drive the SSE
  stream; route tool calls to execution or wrap up the turn.
- `ExecutingTools` → `PostProcessing` | `AwaitingApproval`: dispatch
  per-call (parallel where safe); pause the loop if autonomy demands
  approval.
- `AwaitingApproval` → `ExecutingTools` (resume) | `Failed` (deadline).
- `PostProcessing` → `Streaming` (more tools incoming) | `Completed` |
  `Failed`.

The driver checkpoints loop state to Redis after every successful step
and on pause; a fresh worker can resume from the checkpoint after a
crash. C4 specifies that a resume merges the checkpoint's progress
fields with the fresh request's policy fields (autonomy, system prompt,
model) — operator policy changes between pause and resume take effect.

---

## Multi-tenancy and policy

- **Tenants** are the primary isolation boundary. Every store query is
  scoped by `TenantId` in its type signature; cross-tenant reads
  surface as `StoreError::TenantMismatch`.
- **Plans** (B1) are tenant-scoped policy bundles defined via the admin
  API. They carry budget, rate limits, allowed plugins, tier-models
  override, autonomy default, cache policy, and arbitrary metadata.
  The gateway resolves the per-(tenant, user, workspace) plan on every
  request via [`PlanAssignmentStore::resolve`](../crates/elena-store/src/plan_assignment.rs).
- **Per-tenant credentials** are AES-256-GCM envelope-encrypted with
  the operator's master key. Plaintext never touches disk.
- **Per-tenant admin scope** (S2) — tenants may opt into requiring
  `X-Elena-Tenant-Scope` on top of the global `X-Elena-Admin-Token`
  for their `/admin/v1/*` routes.
- **Cascading cleanup** — `DELETE /admin/v1/tenants/:id` drops the
  tenant row and every dependent row in one transaction; the schema's
  `ON DELETE CASCADE` foreign keys (including the
  `episodes_tenant_id_fkey` added in migration
  `20260424000005_episodes_tenant_fk.sql`) handle the fan-out across
  threads → messages, workspaces, audit_events, plans,
  plan_assignments, plugin_ownerships, tenant_credentials, and
  budget_state. `DELETE /admin/v1/workspaces/:id?tenant_id=…` covers
  finer-grained cleanup.

---

## Reliability invariants

- Hot path never blocks. Audit writes go through a bounded mpsc with
  overflow tracked via `elena_audit_drops_total` (alert on any
  non-zero value).
- Tool execution is idempotent: the loop guards against
  same-`tool_use_id` re-proposals AND against `(tool_name,
  canonical-input)` re-proposals across a thread (prevents Slack
  double-posts).
- Approval replay is blocked: the gateway loads the pending
  `tool_use_id` set from Redis and rejects decisions whose id isn't in
  the current pause window (S4).
- `WorkRequest` redelivery is suppressed: SETNX-guarded by
  `request_id` with 24h TTL (S5).
- Checkpoints survive worker crashes; the resumer re-claims the thread
  via Redis CAS, loads the checkpoint, and continues.

---

## Scalability invariants

- One NATS subscription per gateway pod (X1). The pod fans events into
  per-thread broadcast channels via `DashMap`; WS clients subscribe to
  the local channel. NATS interest scales with pod count, not client
  count.
- Per-tenant credential cache (S8): 60 s TTL `moka::Cache` in front of
  the AES-GCM decrypt path. Admin route invalidates synchronously
  in-process; cross-pod consistency relies on the TTL.
- JetStream caps (X5): work stream bounded by `max_age=1h` and
  `max_bytes=1GiB`; alert at 80%.
- Audit writes batched (X10): up to 500 events per multi-row INSERT;
  tool-result inserts also bulk-batched per turn.
- Connection pool sized for concurrent loops (X8): default
  `pool_max_idle_per_host=64` on every LLM provider.

---

## Security gates

- JWT verification at every WS upgrade and admin call. Tokens carry
  identity only — policy (budget, plan, allowed plugins) loads
  server-side. Mid-session JWT expiry is detected on the next verify
  (validators are stateless; see C5.5 in elena-auth tests).
- CORS layer is opt-in (S6). Operators enumerate origins; wildcard
  rejected at boot.
- Admin token is a hard requirement (S3). Boot fails unless
  `ELENA_ADMIN_TOKEN` is set or `ELENA_ALLOW_OPEN_ADMIN=true` is
  explicitly passed.
- Plugin schemas are filtered fail-closed (C10). DB outage during
  plugin allow-list resolution drops every tool from the model's
  schema list and emits a structured error.

---

## Appendix: phase history

The codebase grew in numbered phases. The historical context lives
here so file-level comments can describe the current contract without
repeating the rollout story.

| Phase | What landed | Notes |
|-------|-------------|-------|
| 1     | Core types + ID branding | `elena-types` foundation |
| 2     | LLM provider abstraction + retry policy | `elena-llm` with Anthropic client |
| 3     | Streaming + tool dispatcher + agent loop | `elena-core::step`, `consume_stream`, parallel tool exec |
| 4     | Context packer + cascade router + episodic memory | `elena-context` (the original `elena-memory` crate was inlined here in Q14), `elena-router` |
| 5     | Gateway + worker + JetStream transport | `elena-gateway`, `elena-worker` |
| 6     | Plugin sidecars (gRPC) + admin routes | `elena-plugins`, `elena-admin` |
| 7     | Multi-tenant rate limits + workspace allow-lists + per-tenant credentials + per-tenant audit | A1–A5 sub-tasks |

Sub-task tags inside phases (e.g., `Phase 7 · A4`, `D5`) refer to the
phase's internal task list. Treat them as historical signposts; the
behaviour they describe is documented in the section above that
references the relevant subsystem.

The hardening pass that followed (B1 / S* / C* / X* / Q* tags throughout
the source) is documented inline at each touchpoint and consolidated in
`.claude/codebase.md` (when present) and `CLAUDE.md`.

---

## Where to look for X

| If you want to … | Read first |
|---|---|
| Add a new built-in tool | `crates/elena-tools/src/tool.rs` (the trait), then register at boot in `bins/elena-server/src/main.rs` |
| Add a new plugin connector | Copy `bins/elena-connector-echo/`, implement `pb::elena_plugin_server::ElenaPlugin`, register endpoint via `ELENA_PLUGIN_ENDPOINTS` |
| Add an admin route | `crates/elena-admin/src/router.rs` + a new module under `crates/elena-admin/src/` |
| Change loop behaviour (phases, approvals, retry) | `crates/elena-core/src/{state,step,loop_driver}.rs` |
| Add a database migration | New `.sql` file in `crates/elena-store/migrations/` (timestamped, additive only) |
| Add a Prometheus metric | `crates/elena-observability/src/metrics.rs`; thread it through `LoopDeps.metrics` |
| Add an LLM provider | Implement `LlmClient` in `crates/elena-llm/src/`, register in the multiplexer in `bins/elena-server/src/main.rs::build_llm` |
| Tune the autonomy / approval policy | `crates/elena-core/src/dispatch_decision.rs` |
| Wire per-tenant credentials for a new connector | Add `resolve_token` (or per-key helper) at the top of the connector's `lib.rs`; read from `tonic::Request::metadata()` and fall back to env |
| Investigate a failing turn | Query `audit_events` by `(tenant_id, thread_id)`; the `kind` column tags `tool_use`, `tool_result`, `approval_decision`, etc. |
| Drop a tenant + all its data | `DELETE /admin/v1/tenants/:id` (cascading; `crates/elena-admin/src/tenants.rs::delete_tenant`) |
| Run the multi-tenant fire-test | `cargo run --release -p elena-tri-tenant-firetest -- --threads-per-tenant 50 --duration-secs 90` (testcontainer harness; report at `/tmp/elena-firetest-report.md`) |
