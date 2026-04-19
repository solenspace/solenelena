# `elena-router`

Heuristic model-tier router for Elena's agentic loop, plus a reactive
cascade check that can re-issue a turn at a higher tier when the output
looks bad.

Phase 4 ships the deterministic ruleset; the architecture's target ONNX
classifier slots in later when production training data exists.

## What's here

- **`route`** (`rules.rs`): pure function over a [`RoutingContext`] that
  picks `Fast` / `Standard` / `Premium` based on message length, turn
  depth, tool count, recent tool names, tenant tier (Enterprise →
  Premium; Free → Fast), and error-recovery count.
- **`cascade_check`** (`cascade.rs`): runs *after* the LLM stream
  completes. Escalates the tier when the output (a) refuses, (b) calls a
  tool that isn't registered, (c) is empty, or (d) is suspiciously brief
  when tools were available. Capped at 2 escalations per turn, always a
  no-op at `Premium`.
- **`ModelRouter`** (`router.rs`): bundles `route` + `cascade_check` +
  tier → `ModelId` resolution via [`TierModels`](elena_config::TierModels).

## Non-goals (Phase 4)

- No ML classifier.
- No learned cascade signals.
- No subscriber-aware routing beyond `TenantTier`.

## Tests

```sh
cargo test -p elena-router --lib  # 22 unit tests table-driving signals
```

Integration with the loop is exercised by `elena-core`'s `phase4_loop.rs`
tests (cascade escalation on refusal).
