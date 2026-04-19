# `elena-worker`

NATS JetStream consumer that runs Elena's agentic loop. One process can
host many concurrent loops (semaphore-capped); multiple processes share
load via the JetStream consumer's queue group.

## Boot

```rust
use std::sync::Arc;
use elena_worker::{WorkerConfig, run_worker};
use tokio_util::sync::CancellationToken;

let cfg = WorkerConfig {
    nats_url: "nats://127.0.0.1:4222".into(),
    worker_id: "worker-pod-7".into(),
    max_concurrent_loops: 500,
    durable_name: None,
    stream_name: None,
};
run_worker(cfg, deps, CancellationToken::new()).await?;
```

`deps: Arc<elena_core::LoopDeps>` is the same bundle every other Elena
caller uses — store, LLM client, tool registry, context manager, memory,
router. The worker doesn't care how it's assembled.

## Per-request handler

For every `WorkRequest` pulled off `elena.work.incoming`:

1. **Claim the thread** via `SessionCache::claim_thread`. If another
   worker already owns it, ack + skip — no double processing.
2. **Append the user message idempotently** via
   `ThreadStore::append_message_idempotent`, so JetStream redelivery after
   a crash is safe.
3. **Build a `LoopState`** with the request's overrides (`model`,
   `system_prompt`, `max_turns`, `max_tokens_per_turn`).
4. **Subscribe to `elena.thread.{id}.abort`** in parallel with the loop;
   any message there cancels the loop's `CancellationToken`.
5. **Call `run_loop`** and pump every emitted `StreamEvent` to
   `elena.thread.{id}.events`.
6. **Ack the JetStream message** after the loop joins.

## Non-goals (Phase 5)

- No retry budget separate from JetStream's own redelivery.
- No per-thread fairness — the queue group does round-robin.
- No metrics / OTel — Phase 7.

## Tests

```sh
cargo test -p elena-worker --lib   # smoke-only unit tests today
```

End-to-end exercised by the Phase-5 smoke binary, which spins NATS via
testcontainers and runs both the gateway and worker in-process.
