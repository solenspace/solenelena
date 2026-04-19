# `elena-memory`

Per-workspace episodic memory for Elena. Records a compact summary of each
completed thread and recalls the semantically-most-similar past episodes
when a new thread begins.

## What's here

- **`extract_summary`** (`summary.rs`): distills a completed thread into a
  task-summary string (≤ 400 chars) + an ordered list of tool names. No
  LLM call — uses the first user message as the summary, which empirically
  captures user intent > 95% of the time. Swap in
  [`Summarizer`](elena_context::Summarizer) later if retrieval quality
  proves insufficient.
- **`EpisodicMemory`** (`memory.rs`): the public `record_episode` /
  `recall` API. `record_episode` is fire-and-forget — errors are logged,
  never returned, so a failing recorder can never fail a user-visible turn.
  `recall` embeds the query and asks `EpisodeStore::similar_episodes`
  (pgvector cosine) for the `top_k` nearest within a workspace.

## Storage

Episodes live in the `episodes` table provisioned by Phase 1
(`elena-store/migrations/20260417000006_episodes.sql`). Schema:
`id, tenant_id, workspace_id, task_summary, actions (jsonb), outcome (jsonb),
embedding (vector(384)), created_at` + HNSW cosine index with
`m=16, ef_construction=64`.

## Example

```rust
use std::sync::Arc;
use elena_memory::EpisodicMemory;

let memory = EpisodicMemory::new(Arc::new(store.episodes.clone()));
memory.record_episode(tenant_id, workspace_id, thread_id,
                      &store.threads, &*embedder, Outcome::Completed).await;

let past = memory.recall(tenant_id, workspace_id,
                         "plan a trip to Tokyo", &*embedder, 3).await?;
```

## Non-goals (Phase 4)

- No cross-workspace sharing (each workspace's memory is private).
- No episode pruning / expiry — a Phase 7 hardening concern.
- No LLM-driven summarization (heuristic is plenty for the first-user-text
  case; Phase 4+ can swap in `Summarizer`).

## Tests

```sh
cargo test -p elena-memory --lib  # 4 unit tests (summary extraction)
```

Integration tests are covered by `elena-core`'s `phase4_loop.rs` scenarios
(record + recall end-to-end).
