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
  elena-server                unified gateway+worker process (Railway image)
  elena-phase7-smoke          end-to-end happy-path + cautious-approval smoke
  elena-hannlys-smoke         marketplace flow E2E (creator + buyer)
  elena-solen-smoke           hero scenario E2E (Slack + Notion + Sheets + Shopify)
  elena-connector-{slack,notion,sheets,shopify}   live-API plugin sidecars

deploy/
  docker/Dockerfile.all-in-one  Railway single-process image
  alerts.yaml                 Prometheus alert rules
  RUNBOOK.md                  operator playbook

scripts/
  live-e2e.sh                 production validation suite
  live-e2e-{mintjwt,runturn}.js  helpers used by live-e2e.sh

.claude/
  ARCHITECTURE.md             Elena runtime contract
  {codebase,solen,hannlys}.md per-app deep-dives
```

---

## HTTP surface

The gateway mounts public paths at the root and the admin API under
`/admin/v1`. Every admin route requires `X-Elena-Admin-Token` (set via
`ELENA_ADMIN_TOKEN`); routes scoped to a single tenant additionally
honour the optional per-tenant `X-Elena-Tenant-Scope` header (S2).

### Public

| Method | Path | Purpose |
|---|---|---|
| GET    | `/health`  | Liveness probe (always 200) |
| GET    | `/ready`   | Readiness — Postgres + NATS reachable; 503 on any miss |
| GET    | `/version` | Crate version |
| GET    | `/info`    | Version + git SHA (`VERGEN_GIT_SHA` if set, else `"unknown"`); snapshot-tested for secret-leak prevention |
| GET    | `/metrics` | Prometheus exposition |
| POST   | `/v1/threads` | Create a thread for the JWT's tenant |
| GET    | `/v1/threads/{thread_id}/stream` | WebSocket upgrade — turn streaming + `SendMessage`/`Abort` |
| POST   | `/v1/threads/{thread_id}/approvals` | Submit tool-approval decisions to a paused loop |

### Admin · apps (Solen / Hannlys / Omnii grouping above tenants)

| Method | Path | Purpose |
|---|---|---|
| POST   | `/admin/v1/apps` | Register an app |
| GET    | `/admin/v1/apps` | List apps (drives the admin-panel dropdown) |
| GET    | `/admin/v1/apps/{id}` | Fetch one |
| PATCH  | `/admin/v1/apps/{id}` | Partial update; `null` clears nullable JSONB fields |
| DELETE | `/admin/v1/apps/{id}` | Delete; **409 if any live tenant references it** |
| GET    | `/admin/v1/apps/{id}/tenants` | List tenants under an app (`?include_deleted=true`, `?name=`) |
| POST   | `/admin/v1/apps/{id}/tenants` | **Atomic onboarding**: create tenant + seed default plan from `default_plan_template` |
| GET    | `/admin/v1/apps/{id}/usage-summary` | `SUM(messages.token_count)` across the app's tenants; `?since=&until=` window |

### Admin · tenants

| Method | Path | Purpose |
|---|---|---|
| POST   | `/admin/v1/tenants` | Upsert tenant (optional `app_id`) |
| GET    | `/admin/v1/tenants` | List with `?app_id=&name=&limit=&offset=&include_deleted=` |
| GET    | `/admin/v1/tenants/{id}` | Fetch (excludes soft-deleted) |
| DELETE | `/admin/v1/tenants/{id}` | **Hard cascade** — drops dependent rows; use for cleanup paths |
| POST   | `/admin/v1/tenants/{id}/soft-delete` | **Production offboarding** — sets `deleted_at`, preserves audit trail |
| PATCH  | `/admin/v1/tenants/{id}/budget` | Replace budget limits |
| PATCH  | `/admin/v1/tenants/{id}/allowed-plugins` | Replace tenant-level plugin allow-list |
| PATCH  | `/admin/v1/tenants/{id}/app` | Attach (`{app_id: "..."}`) or detach (`{app_id: null}`) from an app |
| PUT    | `/admin/v1/tenants/{id}/admin-scope` | Provision/rotate per-tenant scope token |
| GET    | `/admin/v1/tenants/{id}/budget-state` | Daily token burn + rollover + active threads |
| GET    | `/admin/v1/tenants/{id}/credentials` | List provisioned plugins (plaintext never returned) |
| PUT    | `/admin/v1/tenants/{id}/credentials/{plugin_id}` | Provision encrypted credentials |
| DELETE | `/admin/v1/tenants/{id}/credentials/{plugin_id}` | Drop credentials |
| GET    | `/admin/v1/tenants/{id}/audit-events` | Paginated keyset reader; `?since=&until=&kind=&thread_id=&workspace_id=&limit=&before_cursor=` |
| GET    | `/admin/v1/tenants/{id}/threads` | List threads (`?workspace_id=&before=&limit=`) |

### Admin · plans + assignments

| Method | Path | Purpose |
|---|---|---|
| POST   | `/admin/v1/tenants/{tenant_id}/plans` | Create plan |
| GET    | `/admin/v1/tenants/{tenant_id}/plans` | List |
| GET    | `/admin/v1/tenants/{tenant_id}/plans/{plan_id}` | Fetch |
| PATCH  | `/admin/v1/tenants/{tenant_id}/plans/{plan_id}` | Partial update |
| DELETE | `/admin/v1/tenants/{tenant_id}/plans/{plan_id}` | Delete; 409 if assignments reference it |
| PATCH  | `/admin/v1/tenants/{tenant_id}/default-plan` | Atomic default swap |
| PUT    | `/admin/v1/tenants/{tenant_id}/assignments` | Upsert (user, workspace) → plan binding |
| GET    | `/admin/v1/tenants/{tenant_id}/assignments` | List |
| DELETE | `/admin/v1/tenants/{tenant_id}/assignments` | Delete by query scope |

### Admin · threads observability

Tenant scope arrives as `?tenant_id=`; cross-tenant probes return 404
(no existence leak).

| Method | Path | Purpose |
|---|---|---|
| GET | `/admin/v1/threads/{thread_id}` | Fetch one |
| GET | `/admin/v1/threads/{thread_id}/messages` | Paginated message summaries (no content payload) |
| GET | `/admin/v1/threads/{thread_id}/usage` | Cumulative tokens billed to the thread |

### Admin · workspaces + plugins + health

| Method | Path | Purpose |
|---|---|---|
| POST   | `/admin/v1/workspaces` | Upsert workspace |
| GET    | `/admin/v1/workspaces` | List with `?tenant_id=&app_id=&limit=&offset=` |
| GET    | `/admin/v1/workspaces/{id}?tenant_id=` | Fetch |
| PATCH  | `/admin/v1/workspaces/{id}/instructions?tenant_id=` | Replace global-instructions guardrail |
| PATCH  | `/admin/v1/workspaces/{id}/allowed-plugins?tenant_id=` | Replace allow-list |
| DELETE | `/admin/v1/workspaces/{id}?tenant_id=` | Cascade-delete workspace + threads |
| GET    | `/admin/v1/plugins` | List registered plugins (manifest + actions) |
| PUT    | `/admin/v1/plugins/{plugin_id}/owners` | Set owning tenants (empty = global) |
| GET    | `/admin/v1/health/deep` | Sync probe of Postgres + Redis + NATS |

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

### Local hooks

One-time setup after clone:

```sh
bash scripts/install-hooks.sh
```

This sets `git config core.hooksPath .githooks` and installs three
hooks that mirror the canonical commands above:

- **pre-commit** — `cargo fmt --check` + `cargo clippy -D warnings`
  (skipped automatically when no Rust files are staged).
- **commit-msg** — conventional-commit subject validation
  (`<type>(<scope>)?(!)?: <subject>`; allowed types: `feat`, `fix`,
  `chore`, `docs`, `style`, `refactor`, `test`, `perf`, `build`,
  `ci`, `revert`).
- **pre-push** — fmt + clippy + `cargo test --workspace`. The full
  test suite needs the Docker daemon for testcontainer-managed
  Postgres / Redis / NATS; the hook fails fast with a clear message
  when Docker is down.

Bypass for emergencies: `git commit --no-verify` (universal) or
`SKIP_HOOKS=1 git push` (for non-interactive scripts). CI re-runs
every check, so bypassing locally only delays the failure to GitHub.

### Smoke binaries

```sh
# Happy-path turn + Cautious approval round-trip (wiremock by default;
# real Groq when GROQ_API_KEY / ELENA_PROVIDERS__GROQ__API_KEY is set).
cargo run --release -p elena-phase7-smoke

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
5. Container health-check is `/health`. Operator-facing deep probe is
   `GET /admin/v1/health/deep` (Postgres + Redis + NATS); Kubernetes
   readiness probe is `GET /ready` (Postgres + NATS only, no admin
   token required). Restart policy is `ON_FAILURE` with up to 10
   retries (already in `railway.json`).

`elena-server` runs `Store::run_migrations()` unconditionally at
startup, so a fresh deployment applies migrations on first boot. See
`deploy/RUNBOOK.md` for the operator quick-reference.

If you migrate off Railway: Elena currently ships only the all-in-one
process. Splitting back into per-process gateway + worker bins (or
running the all-in-one image with separate `ELENA_WORKER__*` envs in
each replica) is a re-introduction job — the prior Helm chart and
per-service Dockerfiles were removed because they referenced bin
targets that no longer exist.

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
| `ELENA_ADMIN_TOKEN` | yes | Shared secret enforced as `X-Elena-Admin-Token` on every `/admin/v1/*` call. Boot fails if unset, unless `ELENA_ALLOW_OPEN_ADMIN=true` is also set (dev-only escape hatch). |
| `ELENA_ALLOW_OPEN_ADMIN` | optional | Set to `true` to boot `elena-server` without an admin token, exposing `/admin/v1/*` unauthenticated. Local dev / smoke runs only. |
| `ELENA_CORS_ALLOW_ORIGINS` | optional | Comma-separated list of exact-match origins (`https://app.example.com,https://staging.example.com`) the gateway will echo back in `Access-Control-Allow-Origin`. Empty / unset disables CORS (default). The wildcard `*` is rejected at boot — operators must enumerate origins. |
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
| Add or manage an app (Solen / Hannlys / Omnii) | `crates/elena-admin/src/apps.rs`. Apps are an admin grouping above tenants — runtime stays tenant-scoped |
| Onboard a tenant under an app | `POST /admin/v1/apps/{id}/tenants` (`apps::onboard_tenant`) — atomic create + default plan seed |
| Read audit history over HTTP | `crates/elena-store/src/audit_read.rs` (keyset cursor) + `crates/elena-admin/src/audit.rs` |
| Observe a tenant's threads / messages / usage | `crates/elena-admin/src/threads.rs` (`/admin/v1/tenants/{id}/threads`, `/threads/{id}/{messages,usage}`) |
| Read current consumption | `/admin/v1/tenants/{id}/budget-state`, `/admin/v1/apps/{id}/usage-summary` (`crates/elena-admin/src/budget.rs`) |
| Soft-delete a tenant | `POST /admin/v1/tenants/{id}/soft-delete` — sets `deleted_at`, runtime treats as nonexistent, audit trail preserved |
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

## Deprecation window: `TenantTier`

B1 introduced admin-defined Plans (`Plan` / `ResolvedPlan`) as the
single source of truth for budget, allowed plugins, and routing
preferences. The legacy `TenantTier` enum (and the
`BudgetLimits::DEFAULT_FREE` / `DEFAULT_PRO` constants and the
`default_budget_for_tier` fallback) survive only because the JWT claim
shape still carries `tier`. They're marked `#[deprecated]` and every
crate that references them carries a `#![allow(deprecated)]` tagged
`B1.6` so workspace clippy stays green.

**To finish the removal** (a future PR):

1. Drop `tier` from `ElenaJwtClaims` and update BFFs to omit it.
2. Migrate `elena-router::RoutingContext.tenant_tier` to `plan.tier_models`.
3. Migrate `CachePolicy::new(tier, ...)` to `CachePolicy::from_plan(...)`.
4. Drop the `tier` column from `tenants` (new migration).
5. Delete `TenantTier`, `BudgetLimits::DEFAULT_FREE/PRO`,
   `default_budget_for_tier`, `TenantContext.tier`.
6. Remove every `#![allow(deprecated)]` tagged `B1.6` (15+ files).

The `#[deprecated]` annotations document the migration target inline.

---

## CI/CD pipeline

`.github/workflows/ci.yml` runs on every push and pull request:

```
fmt | clippy | test | docker-build      (parallel)
                  └─ deploy              (push to main only, after all four pass)
```

- **fmt** — `cargo fmt --all -- --check`
- **clippy** — `cargo clippy --workspace --all-targets --locked -- -D warnings`
- **test** — `cargo test --workspace --no-fail-fast --locked`
  (ubuntu-latest ships Docker; testcontainer-based integration tests
  Just Work)
- **docker-build** — buildx-builds `deploy/docker/Dockerfile.all-in-one`
  with GHA cache. Validates the production image per-PR without pushing
- **deploy** (main only) — installs the Railway CLI and runs
  `railway redeploy --service elena-server -y` using the
  `RAILWAY_TOKEN` repo secret. Scoped to the `production`
  GitHub Environment (required-reviewer-friendly)

`.github/workflows/pr-title.yml` lints PR titles against the same
conventional-commit type list as the local `commit-msg` hook —
required because squash-merges replace individual commits with the
PR title.

`.github/workflows/audit.yml` runs `cargo audit` (RUSTSEC advisories)
and `cargo deny check` (`deny.toml`) every Monday and on `Cargo.lock`
changes. Advisory: surfaces in the Actions tab but does not gate PRs.

`.github/dependabot.yml` opens grouped weekly PRs for cargo + GitHub
Actions updates. Major bumps of `tonic` / `sqlx` / `axum` are pinned
out (those need a design pass, not a Dependabot PR).

Railway's dashboard auto-deploy is **off** — GitHub Actions is the
only path to production. See `deploy/RUNBOOK.md` for the one-time
operator setup (disabling Railway auto-deploy, provisioning
`RAILWAY_TOKEN`).

Toolchain pin: `rust-toolchain.toml` channel `1.90`. CI inherits this
via `actions-rust-lang/setup-rust-toolchain`. The Dockerfile builder
also uses `rust:1.90-bookworm`, so dev / CI / prod compile against
the same compiler.

---

## Architecture

[`.claude/ARCHITECTURE.md`](.claude/ARCHITECTURE.md) describes the
current Elena runtime contract — crate map, loop state machine,
multi-tenancy / reliability / scalability / security invariants, and
a phase-history appendix for the historical numbering scattered
through source comments.

App-specific deep-dives live next to it: [`.claude/solen.md`](.claude/solen.md),
[`.claude/hannlys.md`](.claude/hannlys.md), [`.claude/codebase.md`](.claude/codebase.md).
