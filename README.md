# Elena

An **application-agnostic** agentic backend written in Rust. Any application
— current or future, internal or external — can connect to Elena and receive
an agentic runtime: streaming LLM responses, tool execution, context
management, cross-session memory, and budget enforcement.

Elena is **its own project**. The `.claude/` directory contains architecture
notes about Claude Code (the CLI that inspired this project); Elena
intentionally diverges wherever a cleaner design is available.

## Status: Phase 6 complete — plugin sidecars over gRPC

| Phase | Scope | State |
|-------|-------|-------|
| 1 | `elena-types`, `elena-config`, `elena-store` | **complete** — 84 unit + 12 integration tests green |
| 2 | `elena-llm` (streaming LLM client, prompt caching, retries) | **complete** — 94 unit + 6 integration tests green |
| 3 | `elena-tools`, `elena-core` (agentic loop + tool orchestration) | **complete** — 12 unit + 7 integration tests green |
| 4 | `elena-context`, `elena-memory`, `elena-router` (retrieval + memory + heuristic routing) | **complete** — 46 unit + 4 integration tests green (Docker required) |
| 5 | `elena-gateway`, `elena-worker`, NATS wiring (JWT-auth WebSocket front end + JetStream-backed worker) | **complete** — 11 gateway + 0 worker unit tests green; wiremock smoke runs gateway + worker + NATS + fake Anthropic in one process |
| 6 | `elena-plugins` (gRPC connectors — `GetManifest`/`Execute`/`Health`, registry + Tool bridge, `elena-connector-echo` reference sidecar) | **complete** — 28 unit + 1 end-to-end test green; Phase-6 smoke rounds a tool_use through the registry into an in-process echo connector |
| 7 | Production hardening (mTLS, OTel export, per-tenant rate limits, chaos) | not started |

## Workspace layout

```
elena/
├── .claude/                    # Reference docs about Claude Code (read-only).
├── crates/
│   ├── elena-types/            # Foundation types — zero infra dependencies.
│   ├── elena-config/           # Configuration loader (figment + env + TOML).
│   ├── elena-store/            # Persistence: Postgres + Redis + pgvector.
│   ├── elena-llm/              # Streaming Anthropic client + retry + prompt caching.
│   ├── elena-tools/            # Tool trait, registry, concurrent/serial orchestration.
│   ├── elena-context/          # Embedding-based retrieval, packing, summarization.
│   ├── elena-memory/           # Per-workspace episodic memory (record + recall).
│   ├── elena-router/           # Heuristic tier routing + cascade escalation.
│   ├── elena-core/             # Agentic loop state machine + Redis checkpointing.
│   ├── elena-gateway/          # axum WebSocket gateway + JWT auth + NATS publisher.
│   ├── elena-worker/           # NATS JetStream consumer running run_loop.
│   └── elena-plugins/          # gRPC plugin protocol + PluginRegistry + Tool bridge.
└── bins/
    ├── elena-phase1-smoke/     # End-to-end smoke for Phase 1 (store).
    ├── elena-phase2-smoke/     # End-to-end smoke for Phase 2 (LLM streaming).
    ├── elena-phase3-smoke/     # End-to-end smoke for Phase 3 (full loop round-trip).
    ├── elena-phase4-smoke/     # End-to-end smoke for Phase 4 (retrieval two-turn).
    ├── elena-phase5-smoke/     # End-to-end smoke for Phase 5 (gateway + worker + NATS).
    ├── elena-phase6-smoke/     # End-to-end smoke for Phase 6 (Phase-5 stack + plugin round-trip).
    └── elena-connector-echo/   # Reference gRPC plugin sidecar (one action: `reverse`).
```

## Running

### Prerequisites

- Rust stable (1.85+, edition 2024).
- Docker (for integration tests and the smoke binary) — Postgres with the
  `pgvector` extension + Redis 7.

### Building

```sh
cargo build --workspace
```

### Testing

Unit tests run without external services:

```sh
cargo test --workspace --lib
```

Integration tests spin up Postgres + Redis via `testcontainers`:

```sh
cargo test --workspace
```

### Phase 1 smoke test

The smoke binary exercises the full data path end-to-end against a live
Postgres + Redis. Start local services:

```sh
docker run -d --name elena-pg \
    -e POSTGRES_PASSWORD=elena -e POSTGRES_USER=elena -e POSTGRES_DB=elena \
    -p 5432:5432 pgvector/pgvector:pg16
docker run -d --name elena-redis -p 6379:6379 redis:7-alpine
```

Copy `.env.example` to `.env` and adjust if needed, then run:

```sh
cargo run -p elena-phase1-smoke
```

Exit code `0` means Phase 1 is healthy.

## Design invariants

1. **Application-agnostic.** No app-specific types. Apps are identified by
   `TenantId` + optional metadata.
2. **Multi-tenant at every boundary.** Every store method takes a `TenantId`
   parameter — isolation is not ambient state.
3. **Serializable types everywhere.** Every public type implements
   `Serialize + Deserialize`. Non-serializable library errors (`sqlx::Error`,
   `fred::Error`) are converted to `String` at the crate boundary so
   `ElenaError` can cross NATS/gRPC/WebSocket unchanged.
4. **No unsafe.** Workspace sets `unsafe_code = "forbid"`.
5. **Improvements over the reference.** Where Claude Code's TypeScript has
   incidental complexity, Elena simplifies. The `.claude/` docs describe the
   reference; the Rust code is what Elena actually does.

## Ground truth

Types in `elena-types` are calibrated against the Claude Code TypeScript
source (`../` relative to this workspace). Validated:

- 15-variant `Terminal` — all exit reasons from `query.ts`.
- 22-variant `LlmApiError` — 1:1 map of `services/api/errors.ts::classifyAPIError`.
- 9-field `Usage` — exact shape of `services/api/emptyUsage.ts` including
  `server_tool_use` and nested `cache_creation` TTL buckets.
- `CacheControl` with `ttl: "5m" | "1h"` and `scope: "global" | "org"`.
- Content blocks including `Thinking`/`RedactedThinking` with the required
  `signature` field.
- Three-outcome permission decisions (`allow` / `ask` / `deny`) with rich
  bodies matching `types/permissions.ts:174-246`.
