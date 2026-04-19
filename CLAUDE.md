# Elena

Elena is an application-agnostic agentic backend in Rust. One process
serves any tenant: a user message arrives over WebSocket, the gateway
publishes it to NATS, a worker claims the thread, the agentic loop
streams the LLM, dispatches tool calls (built-in or plugin sidecars),
checkpoints to Redis, persists to Postgres, and streams events back to
the client. Solen and Hannlys both ride this engine; new products plug
in by registering a workspace + plugins.

This file is the entry point for engineers (and Claude Code sessions)
working in this repository.

---

## Runtime, in one sketch

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

## Repository layout

```
crates/
  elena-types          domain types, transport envelopes, autonomy enum
  elena-config         figment loader (TOML + env, layered)
  elena-store          Postgres + Redis persistence; migrations
  elena-llm            provider trait, SSE streaming, anthropic + openai-compat
  elena-tools          Tool trait, registry, batched orchestrator
  elena-context        context packing + summarisation
  elena-memory         pgvector embeddings, episodic recall
  elena-router         tier model routing, circuit breaker, failover
  elena-core           the agent loop: state machine, step driver, dispatch
  elena-gateway        axum HTTP+WS frontend, JWT auth, NATS publisher
  elena-worker         NATS JetStream consumer, claim CAS, dispatch
  elena-plugins        gRPC plugin registry, action_tool, schema-drift
  elena-observability  Prometheus metrics, OTel pipeline, mTLS helpers
  elena-auth           JWT validator, JWKS, secret providers
  elena-admin          /admin/v1 routes (tenants, workspaces, plugins, ...)

bins/
  elena-server                unified gateway+worker process (Railway, Docker)
  elena-phase{1..7}-smoke     phase-by-phase end-to-end smokes
  elena-hannlys-smoke         marketplace flow E2E (creator + buyer)
  elena-solen-smoke           hero scenario E2E (Slack + Notion + Sheets + Shopify)
  elena-connector-echo        in-process plugin reference
  elena-connector-{slack,notion,sheets,shopify}   live-API plugin sidecars

deploy/
  helm/elena                  Helm chart (EKS, GKE, AKS, kind)
  docker/Dockerfile.all-in-one  Railway-friendly single-process image
  docker/Dockerfile.{gateway,worker,connector-echo}   per-service images
  k8s/                        raw manifests
  RUNBOOK.md                  operator playbook

scripts/
  bootstrap.sh                first-deploy bring-up

.claude/
  00-…10-                     architecture deep-dives of the
                              Claude Code TypeScript codebase that
                              informed Elena's design (read for context
                              when touching the matching subsystem)
```

---

## Build, lint, test

All commands run from the repo root.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

Unit tests are colocated; integration tests live in `crates/*/tests/`.
The expensive ones (Postgres + Redis + NATS via `testcontainers`)
require Docker. Smoke binaries live under `bins/elena-*-smoke`.

### Smoke binaries

```sh
# Phase 7 happy path (real Groq + testcontainer infra)
GROQ_API_KEY=... cargo run --release -p elena-phase7-smoke

# Hannlys marketplace flow (real Groq; real Notion when secrets are set)
cargo run --release -p elena-hannlys-smoke

# Solen hero scenario (real Groq + 4 live sandbox services)
cargo run --release -p elena-solen-smoke
```

Each smoke checks for required secrets at startup. Missing secrets
print a `SKIP — missing env vars: …` message and exit with code `2`,
which CI can treat as "skipped" without failing the build.

---

## Deployment

### Railway (single-process, fastest)

1. Push the repo to a Railway project pointing at the repo root.
2. Railway autodetects `railway.json` and builds
   `deploy/docker/Dockerfile.all-in-one`.
3. Add Postgres, Redis, and NATS plugins from Railway's catalog. They
   expose `DATABASE_URL`, `REDIS_URL`, and `NATS_URL` automatically.
4. Set the env vars from the table below. The `.env.local` file in
   this repo lists them all in `KEY=value` form.
5. Health-check path is `/admin/v1/health/deep`. Restart policy is
   `ON_FAILURE` with up to 10 retries (already in `railway.json`).

### Helm / Kubernetes (production)

```sh
helm install elena deploy/helm/elena \
  --namespace elena --create-namespace \
  --values your-prod-values.yaml
```

`elena-server` runs `Store::run_migrations()` unconditionally at
startup, so a fresh deployment applies migrations on first boot. See
`deploy/RUNBOOK.md` for cluster prerequisites, mTLS provisioning, and
upgrade procedure.

---

## Environment variables

| Variable | Required | Purpose |
|---|---|---|
| `DATABASE_URL` | yes | Postgres connection (e.g. `postgres://...`) |
| `REDIS_URL` | yes | Redis connection (e.g. `redis://...`) |
| `NATS_URL` | yes | NATS connection (e.g. `nats://...`) |
| `JWT_HS256_SECRET` | yes (HS256) | Symmetric secret for JWT validation |
| `JWT_JWKS_URL` | yes (RS256) | Remote JWKS endpoint (alternative to HS256) |
| `ELENA_CREDENTIAL_MASTER_KEY` | recommended | Base64-encoded 32-byte AES-256 key. Required if any tenant uses per-tenant credential injection; absence falls back to env-only single-tenant mode. |
| `GROQ_API_KEY` / `ANTHROPIC_API_KEY` / `OPENROUTER_API_KEY` | per provider | Set the ones you need; the multiplexer registers each one configured |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | optional | Enable OTel pipeline export |
| `RUST_LOG` | optional | tracing-subscriber filter (default: `info`) |

Connector sidecars (Slack, Notion, Sheets, Shopify) read their own env
vars at startup as a single-tenant default. For multi-tenant
deployments the worker injects per-tenant credentials from
`tenant_credentials` (encrypted at rest) as `x-elena-cred-*` gRPC
metadata; connectors prefer the metadata over env when both are present.

---

## Per-tenant credential model

`tenant_credentials(tenant_id, plugin_id, kv_ciphertext, kv_nonce)`
stores each tenant's per-plugin credential map encrypted with
`Aes256Gcm` using the operator-supplied master key. Provision via:

```
PUT /admin/v1/tenants/{tenant_id}/credentials/{plugin_id}
{ "credentials": { "token": "xoxb-..." } }
```

The body is plaintext on the wire (use mTLS or VPN); the server
encrypts before insert. Plaintext never touches disk.

At dispatch time the worker:

1. Looks up `(tenant_id, plugin_id)` in `tenant_credentials`.
2. Decrypts in-memory.
3. Attaches each `(key, value)` pair to the outgoing gRPC request as
   `x-elena-cred-<key>: <value>` metadata.

Connectors extract these via `tonic::Request::metadata()`. If a key is
absent from metadata, the connector falls back to its env-default. This
keeps single-tenant ops simple while enabling multi-tenant isolation.

---

## Real-services smoke prerequisites

The live-services smokes (`elena-solen-smoke` and `elena-hannlys-smoke`
in live mode) read tokens from `.env.local` (gitignored). The bottom of
that file has a "Live-services smoke tokens" block — fill in:

- A Slack workspace with a bot token whose scopes include
  `chat:write`, `channels:read`, plus a dedicated test channel id.
- A Notion integration token + a parent page id where smokes can
  create children.
- A Google service-account or user OAuth access token + a test
  spreadsheet id.
- A Shopify dev store + custom-app admin API token.
- A Groq API key (free tier is fine).
- `ELENA_CREDENTIAL_MASTER_KEY` (base64 32-byte AES-256 key).

Each smoke prints a SKIP message and exits 2 if its required tokens
are missing — partial provisioning is fine.

---

## Where to look for X

| If you want to … | Read first |
|---|---|
| Add a new built-in tool | `crates/elena-tools/src/tool.rs` (the trait), then register in the boot path in `bins/elena-server/src/main.rs` |
| Add a new plugin connector | Copy `bins/elena-connector-echo/`, implement `pb::elena_plugin_server::ElenaPlugin`, register endpoint via `ELENA_PLUGIN_ENDPOINTS` |
| Add an admin route | `crates/elena-admin/src/router.rs` + a new module under `crates/elena-admin/src/` |
| Change loop behaviour (phases, approvals, retry) | `crates/elena-core/src/{state,step,loop_driver}.rs` |
| Add a database migration | New `.sql` file in `crates/elena-store/migrations/` (timestamped, additive only — never destructive ALTER/DROP) |
| Add a Prometheus metric | `crates/elena-observability/src/metrics.rs`; thread it through `LoopDeps.metrics` |
| Add an LLM provider | Implement `LlmClient` in `crates/elena-llm/src/`, register in the multiplexer in `bins/elena-server/src/main.rs::build_llm` |
| Tune the autonomy / approval policy | `crates/elena-core/src/dispatch_decision.rs` |
| Wire per-tenant credentials for a new connector | Add `resolve_token` (or per-key helper) at the top of the connector's `lib.rs`; read from `tonic::Request::metadata()` and fall back to env |
| Investigate a failing turn | Query `audit_events` by `(tenant_id, thread_id)`; the `kind` column tags `tool_use`, `tool_result`, `approval_decision`, etc. |

---

## Design invariants

1. **Application-agnostic.** No app-specific types in any crate. Apps
   are identified by `TenantId` + opaque metadata.
2. **Multi-tenant at every boundary.** Every store method takes a
   `TenantId`. Cross-tenant reads surface as `StoreError::TenantMismatch`.
3. **Serializable types everywhere.** Every public type implements
   `Serialize + Deserialize`. Non-serializable library errors
   (`sqlx::Error`, `fred::Error`, `tonic::Status`) are converted to
   `String` at the crate boundary so `ElenaError` can cross
   NATS / gRPC / WebSocket unchanged.
4. **No `unsafe`.** Workspace sets `unsafe_code = "forbid"`.
5. **Hot path never blocks.** Audit writes go through a bounded mpsc
   with overflow tracked via `elena_audit_drops_total` (alert on any
   non-zero value). Plugin RPCs run with a per-call timeout. The
   credential store decrypt is in-memory.
6. **Validate at boundaries, trust internal code.** Argument
   validation lives at the gateway (JWT, JSON schemas) and at the
   plugin (input_schema). Internal calls take typed values and trust
   them.

---

## Conventions

- **Errors**: `thiserror` for library-public errors; `anyhow` allowed
  only in `bins/`.
- **Tracing**: every async boundary gets a `#[instrument]` span.
- **Migrations**: append-only — never destructive `ALTER` / `DROP`.
- **Tests**: unit tests inline; integration tests under `crates/*/tests/`;
  use `testcontainers` for anything touching Postgres / Redis / NATS.
- **Lints**: workspace-wide `pedantic` + `unwrap_used = deny` +
  `panic = deny` + `print_stdout/stderr = warn`. New code must pass
  `clippy --all-targets -D warnings`.
- **Comments**: only when the *why* isn't obvious. No restating WHAT
  the code does. No marketing copy. No emojis in code or commits.
- **Commit messages**: terse, conventional-commit style
  (`feat(crate): …`, `fix(crate): …`, `chore: …`, `docs: …`,
  `test: …`, `style: …`). **Never** add `Co-Authored-By` trailers —
  every commit is authored by the human running this repo.

---

## Architecture deep-dives

The reference docs in `.claude/` describe the Claude Code TypeScript
codebase Elena's design was modelled on. They are not Elena
documentation — Elena diverges wherever Rust patterns or the
distributed runtime point to a cleaner design. Read the relevant doc
when touching the matching subsystem; do not duplicate them here.

| File | Subsystem |
|---|---|
| [.claude/00-architecture-overview.md](.claude/00-architecture-overview.md) | Whole-codebase map |
| [.claude/01-agentic-loop.md](.claude/01-agentic-loop.md) | The while-true loop |
| [.claude/02-api-streaming.md](.claude/02-api-streaming.md) | LLM streaming + retry |
| [.claude/03-tool-system.md](.claude/03-tool-system.md) | Tools + permissions |
| [.claude/04-context-management.md](.claude/04-context-management.md) | Context + compaction |
| [.claude/05-services.md](.claude/05-services.md) | Service subsystems |
| [.claude/06-multi-agent.md](.claude/06-multi-agent.md) | Coordinator + bridge |
| [.claude/07-state-bootstrap.md](.claude/07-state-bootstrap.md) | App state + bootstrap |
| [.claude/08-commands-skills.md](.claude/08-commands-skills.md) | Commands + skills |
| [.claude/09-tui-framework.md](.claude/09-tui-framework.md) | Ink TUI |
| [.claude/10-rust-architecture.md](.claude/10-rust-architecture.md) | Rust crate layout |
