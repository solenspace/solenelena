# elena-store

Persistence layer for Elena.

- `ThreadStore` — thread and message persistence in Postgres.
- `TenantStore` — tenant records, budget state, usage recording.
- `SessionCache` — Redis-backed hot state: thread claims (with Lua CAS),
  session context, rate-limit windows.

## Multi-tenant discipline

Every public method takes a `TenantId` parameter. Queries filter on
`tenant_id` in SQL; the ambient, thread-local, or handle-level tenant is
intentionally absent. Cross-tenant reads surface as either:

- empty list (for `list_messages`), or
- `StoreError::TenantMismatch { owner, requested }` (for single-row lookups).

## Migrations

Six migrations under `migrations/`:

1. `enable_extensions` — `vector`, `pgcrypto`.
2. `tenants` — one row per application.
3. `threads` — conversation contexts.
4. `messages` — individual turns with `vector(384)` embeddings + HNSW index.
5. `budget_state` — running counters.
6. `episodes` — cross-session memory (Phase 4 will populate).

Apply via `Store::run_migrations()` once per process lifetime.

## Vector search

The `messages` and `episodes` tables use HNSW indexes
(`m=16, ef_construction=64`) on cosine similarity. Partial indexes skip
`NULL` embeddings, so Phase 1's `NULL` writes don't bloat the index.

## Redis claim CAS

`SessionCache::claim_thread` uses `SET … NX PX <ttl>`. Release and refresh
use a Lua CAS script that compares the stored `worker_id` before acting —
so a stale release from a worker that lost its claim is a safe no-op.

## Integration tests

`tests/smoke.rs` uses `testcontainers` to spin up `pgvector/pgvector:pg16`
and `redis:7-alpine` — no local services required. Run with Docker
available:

```sh
cargo test -p elena-store -- --test-threads=1
```
