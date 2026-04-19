# `elena-context`

Embedding-based context retrieval, packing, and long-session summarization
for Elena's agentic loop.

## What's here

- **`Embedder` trait** (`embedder.rs`): async `embed(text) -> Vec<f32>`
  plus a `dimension()` advertisement. Three ships-with impls — `OnnxEmbedder`
  (real `ort` + `tokenizers`), `NullEmbedder` (returns empty, disables
  retrieval), and `FakeEmbedder` (deterministic hash-based, for tests).
- **`TokenCounter`** (`tokens.rs`): conservative char-based token
  approximation (≈ 3.2 chars/token) so budget math doesn't need a
  round-trip to a real tokenizer.
- **`pack`** (`packer.rs`): drops middle-aged messages while preserving the
  chronological tail — respects a tokens budget with a 10% headroom.
- **`ContextManager`** (`context_manager.rs`): glues the embedder +
  retrieval (`similar_messages` from `elena-store`) + packer. Falls back to
  pure recency when the embedder is `NullEmbedder`.
- **`Summarizer`** (`summarize.rs`): LLM-backed on-demand thread
  summarization for 200+ turn histories. Produces a single text blob; the
  caller persists it as a `MessageKind::System(CompactBoundary)` row.
- **`OnnxEmbedder`** (`onnx.rs`): real inference via the `ort` crate. Loads
  e5-small-v2-compatible BERT models, mask-weighted mean pools the
  `last_hidden_state`, L2-normalizes for cosine comparability in Postgres.

## Non-goals (Phase 4)

- No auto-download of the ONNX model — operator supplies the path.
- No `Message::token_count` backfill — the counter runs on demand.
- No proactive compaction (replaces Claude Code's snip / microcompact /
  collapse / auto-compact; retrieval handles the normal case).

## Example

```rust
use std::sync::Arc;
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder, Embedder};

let embedder: Arc<dyn Embedder> = Arc::new(NullEmbedder);
let cm = ContextManager::new(Arc::clone(&embedder), ContextManagerOptions::default());
// `cm.build_context(...)` falls back to a recency window when the embedder
// is Null; with a real `OnnxEmbedder` it performs semantic retrieval.
```

## Tests

```sh
cargo test -p elena-context --lib          # 20 unit tests
cargo test -p elena-context --test summarize  # wiremock-based summarizer
cargo test -p elena-context --test retrieval  # Postgres-backed, needs Docker
```

One test (`onnx::tests::load_errors_on_missing_model_file`) is marked
`#[ignore]` — it requires `libonnxruntime.dylib` on the host, which
Elena operators provide alongside the model file.
