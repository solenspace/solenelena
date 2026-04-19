# `elena-core`

The agentic loop state machine for Elena. Given an initial [`LoopState`],
this crate drives the conversation forward: stream an LLM turn, execute
tools, react to results, loop, and return when the model is done or a
limit trips.

## Phases

```
Received → Streaming → [tool_use?] → ExecutingTools → PostProcessing
                │                                             │
                └────────────── text only ────────────────────┤
                                                              │
                                            tools ran? ───────┤
                                                              │
                                   no  ↓          yes ↓       │
                                  Completed    Streaming ←────┘
```

- **`Received`**: thread just arrived; advance to streaming.
- **`Streaming`**: build context + request, stream from `elena-llm`,
  assemble the assistant message, commit to Postgres, extract tool calls.
- **`ExecutingTools`**: run the batch via `elena_tools::execute_batch`;
  commit each `tool_result` message; bump `turn_count`.
- **`PostProcessing`**: record usage, check budget + max-turns, save
  checkpoint, decide: loop back to `Streaming` (tools just ran) or finish
  (pure text turn).
- **`Completed` / `Failed`**: cleanup and emit `Done(Terminal)`.

## Crash recovery

After each successful `step()`, the outer driver (`run_loop`) serializes
`LoopState` to JSON and writes it to Redis at `elena:thread:loop:{thread}`.
On worker startup, `run_loop` checks for an existing checkpoint and resumes
from the recorded phase. Committed Postgres messages are the authoritative
record; the Redis snapshot is a fast-resume hint.

## Public API

```rust
use std::sync::Arc;
use elena_core::{LoopState, LoopDeps, run_loop};
use elena_types::ModelId;
use tokio_util::sync::CancellationToken;

# async fn example(deps: Arc<LoopDeps>, state: LoopState) {
let (handle, mut stream) = run_loop(
    state,
    deps,
    "worker-1".into(),
    CancellationToken::new(),
);

// Consume `stream` for StreamEvent events (forward to WebSocket, etc.).
let terminal = handle.await.expect("loop task");
# }
```

## Non-goals (Phase 3)

- No model routing / cascade check (Phase 4).
- No embedding retrieval / compaction (Phase 4).
- No episodic memory recall (Phase 4).
- No subagent spawning (Phase 6).
- No NATS / gateway / worker binary (Phase 5).
- No permission UI — `Permission::Ask` is auto-allowed (Phase 5 wires a
  prompt channel).
- No tool timeout enforcement.

## Tests

```sh
cargo test -p elena-core --lib                # 13 unit tests
cargo test -p elena-core --test round_trip \  # 7 integration tests
    -- --test-threads=1                        # (Docker required)
```

Integration tests spin up throwaway Postgres (pgvector) + Redis via
`testcontainers` and mock Anthropic with `wiremock`. Scenarios covered:

1. Text-only completion (no tools).
2. Single `tool_use` → tool → text round-trip.
3. Parallel tool calls (three concurrent-safe tools).
4. Max-turns cap trips cleanly.
5. Budget exhaustion emits `BudgetExceeded` + `BlockingLimit` terminal.
6. Mid-stream cancellation yields `AbortedStreaming`.
7. Resume-from-checkpoint continues at the recorded phase.

## Real-API smoke

```sh
ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase3-smoke
```

Spins throwaway infra, registers a `reverse_text` tool, asks the model to
use it, and exits 0 on a clean round-trip.
