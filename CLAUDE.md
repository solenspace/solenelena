# Elena — Engineering Notes

Elena is an application-agnostic agentic backend written in Rust. It powers
Solen (enterprise voice-first multi-service agent) and Hannlys (creator
marketplace with personalized digital products), and is designed so any
future product can plug in by registering plugins and a workspace.

This file is the entry point for engineers (and Claude Code sessions)
working in this repository. The architecture deep-dives the codebase was
modeled on live in `.claude/00-…10-`.

---

## Repository layout

```
crates/
  elena-types          domain types, transport envelopes, autonomy enum
  elena-config         figment-based config loader (file + env)
  elena-store          Postgres + Redis persistence; migrations
  elena-llm            provider trait, SSE streaming, anthropic + openai-compat
  elena-tools          tool trait, permission policy, JSON-schema validation
  elena-context        conversation context + compaction
  elena-memory         pgvector embeddings, episodic recall
  elena-router         provider tier selection, circuit breaker, failover
  elena-core           the agent loop: state machine, step driver, dispatch
  elena-gateway        axum HTTP+WS frontend, routes, auth middleware
  elena-worker         NATS JetStream subscriber, claim CAS, dispatch
  elena-plugins        gRPC plugin registry, action_tool, schema-drift
  elena-observability  metrics, tracing, OTel pipeline, mTLS helpers
  elena-auth           JWT validator, JWKS, secret providers
  elena-admin          admin HTTP routes (tenants, workspaces, plugins, ...)

bins/
  elena-server                unified gateway+worker process (Railway/Docker)
  elena-phase{1..7}-smoke     phase-by-phase end-to-end smokes
  elena-hannlys-smoke         marketplace flow E2E (creator + buyer)
  elena-solen-smoke           hero scenario E2E (Slack+Notion+Sheets+Shopify)
  elena-connector-echo        in-process plugin reference
  elena-connector-slack       Slack Web API plugin sidecar
  elena-connector-notion      Notion REST plugin sidecar
  elena-connector-sheets      Google Sheets v4 plugin sidecar
  elena-connector-shopify     Shopify Admin REST plugin sidecar

deploy/
  helm/elena                  Helm chart (AWS/EKS, GKE, etc.)
  docker/Dockerfile.gateway   per-service image
  docker/Dockerfile.worker
  docker/Dockerfile.connector-echo
  docker/Dockerfile.all-in-one  Railway-friendly single-process image
  k8s/                        raw k8s manifests
  RUNBOOK.md                  operator playbook

scripts/
  bootstrap.sh                first-deploy bring-up (helm + migrations)

.claude/
  CLAUDE.md          (legacy index — superseded by THIS file at repo root)
  00-…10-            architecture deep-dives (kept for reference)
```

---

## Build, lint, test

All commands run from the repo root.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

Unit tests are colocated; integration tests live in `crates/*/tests/`. The
expensive ones (testcontainers spin Postgres + Redis + NATS) live under
`crates/elena-store/tests/` and the smoke binaries under `bins/`.

### Smoke binaries

```sh
# Phase 7 happy path (real Groq + testcontainer infra)
GROQ_API_KEY=... cargo run --release -p elena-phase7-smoke

# Hannlys marketplace flow (real Groq + real Notion sandbox)
GROQ_API_KEY=... NOTION_TOKEN=... NOTION_PARENT_PAGE_ID=... \
  cargo run --release -p elena-hannlys-smoke

# Solen hero scenario (real Groq + 4 live sandbox services)
# Tokens come from `.env.local`; smoke skips with exit 2 if any are missing.
cargo run --release -p elena-solen-smoke
```

Smokes that require absent secrets `eprintln!` a skip notice and exit 0
so CI stays green.

---

## Deployment

### Railway (single-process, fastest)

1. Push the repo to a Railway project pointing at the repo root.
2. Railway autodetects `railway.json` → builds
   `deploy/docker/Dockerfile.all-in-one`.
3. Add the Postgres, Redis, and NATS plugins from Railway's catalog.
   They expose `DATABASE_URL`, `REDIS_URL`, `NATS_URL` automatically.
4. Set the env vars Elena needs (see the table below). The
   `.env.local` file in this repo lists the same vars in
   `KEY=value` form — copy what you need into Railway's secrets.
5. Health-check path `/admin/v1/health/deep`. Restart policy
   `ON_FAILURE` (already set in `railway.json`).

### Helm / Kubernetes (production)

```sh
helm install elena deploy/helm/elena \
  --namespace elena --create-namespace \
  --set image.tag=v1 \
  --values your-prod-values.yaml
```

Migrations run on pod startup via the gateway's `--migrate` flag. See
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
| `JWT_JWKS_URL` | yes (RS256) | Remote JWKS endpoint, alternative to HS256 |
| `ELENA_CREDENTIAL_MASTER_KEY` | yes | Base64-encoded 32-byte AES-256 key for at-rest credential encryption |
| `GROQ_API_KEY` | per provider | Anthropic-style chat completions endpoint |
| `ANTHROPIC_API_KEY` | per provider | Anthropic native SDK |
| `OPENROUTER_API_KEY` | per provider | OpenRouter cascade target |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | optional | Enable OTel pipeline |
| `RUST_LOG` | optional | tracing-subscriber filter (default: info) |

Connector sidecars (Slack, Notion, Sheets, Shopify) read env vars at
startup as a single-tenant default. For multi-tenant deployments the
worker injects per-tenant credentials from `tenant_credentials` (encrypted
at rest) as `x-elena-cred-*` gRPC metadata; connectors prefer the
metadata over env when both are present.

---

## Per-tenant credential model

`tenant_credentials(tenant_id, plugin_id, kv_ciphertext, kv_nonce)` stores
each tenant's per-plugin credential map encrypted with `Aes256Gcm` using
the operator-supplied master key. The admin endpoint
`PUT /admin/v1/tenants/{tenant_id}/credentials/{plugin_id}` accepts a
plaintext JSON `{ "key": "value" }` body and encrypts before insert.

At dispatch time, the worker:

1. Looks up `(tenant_id, plugin_id)` in `tenant_credentials`.
2. Decrypts in-memory.
3. Attaches each `(key, value)` pair to the outgoing gRPC request as
   `x-elena-cred-<key>: <value>` metadata.

Connectors extract these via `tonic::Request::metadata()`. If a key is
absent from metadata, the connector falls back to its env-default. This
keeps single-tenant ops simple while enabling multi-tenant isolation.

---

## Real-services smoke prerequisites

The live-services smokes (`elena-solen-smoke`,
`elena-hannlys-smoke` in live mode) read tokens from `.env.local`
(gitignored). The bottom of that file has a "Live-services smoke
tokens" block — fill in:

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

## Conventions

- **Errors**: `thiserror` for crate-public errors, `anyhow` only in
  `bins/`.
- **Tracing**: every async boundary gets a `#[instrument]` span.
- **Migrations**: append-only — never destructive `ALTER` / `DROP`.
- **Tests**: unit tests inline; integration tests in `tests/`; use
  testcontainers for anything touching Postgres/Redis/NATS.
- **Lints**: workspace-wide `pedantic` + `unwrap_used = deny` +
  `panic = deny`. New code must pass `clippy --all-targets -D warnings`.
- **Comments**: only when the *why* isn't obvious. No restating WHAT
  the code does. No marketing copy.
- **Commit messages**: terse, conventional-commit style
  (`feat(crate): …`, `fix(crate): …`, `chore: …`, `docs: …`,
  `test: …`). **Never** add `Co-Authored-By` trailers — every commit
  is authored by the human running this repo.

---

## Architecture deep-dives

The reference docs in `.claude/` describe the Claude Code TypeScript
codebase that informed Elena's design. Read the relevant doc when
touching the matching subsystem; do not duplicate them here.

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
