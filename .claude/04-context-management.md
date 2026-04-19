# Context Management

> **Source files**:
> - `services/compact/autoCompact.ts` — Token-threshold triggered summarization
> - `services/compact/compact.ts` — Core compaction orchestration
> - `services/compact/microCompact.ts` — Lightweight partial reduction
> - `services/compact/reactiveCompact.ts` — Emergency compact on 413 errors
> - `services/compact/snipCompact.ts` — Remove old messages (HISTORY_SNIP)
> - `services/contextCollapse/` — Staged context collapse system
> - `utils/tokens.ts` — Token counting and estimation
> - `utils/context.ts` — Context window sizes per model
> - `query/tokenBudget.ts` — Budget tracking and continuation decisions

---

## Four Layers of Context Reduction

The system uses 4 progressively aggressive compaction strategies, applied **in this order** every loop iteration:

### Layer 1: Snip Compact (fastest, least disruptive)
**When**: Every turn (if `HISTORY_SNIP` feature enabled)
**What**: Removes the oldest messages from conversation history
**How**: `snipCompactIfNeeded(messages)` → returns `{ messages, tokensFreed, boundaryMessage? }`
**Source**: `services/compact/snipCompact.ts`

### Layer 2: Microcompact (fast, partial)
**When**: Every turn
**What**: Lightweight reduction of tool results and verbose content
**How**: `microcompactMessages(messages, toolUseContext, querySource)` → compacted messages
**Variants**:
- Standard microcompact: Truncate large tool results
- Cached microcompact (`CACHED_MICROCOMPACT` feature): Edit prompt cache directly
**Source**: `services/compact/microCompact.ts`, `services/compact/apiMicrocompact.ts`

### Layer 3: Context Collapse (moderate, staged)
**When**: Every turn (if `CONTEXT_COLLAPSE` feature enabled)
**What**: Project a collapsed view over the full REPL history — collapses are staged, then committed on overflow
**How**: `applyCollapsesIfNeeded(messages, toolUseContext, querySource)`
**Recovery**: On 413 error, `recoverFromOverflow()` commits all staged collapses
**Source**: `services/contextCollapse/`

### Layer 4: Auto-Compact (most aggressive, LLM-based)
**When**: Token threshold exceeded
**What**: Full LLM-based summarization via forked subagent
**How**: `autoCompactIfNeeded(messages, toolUseContext, cacheSafeParams, querySource, tracking, snipTokensFreed)`
**Source**: `services/compact/autoCompact.ts`, `services/compact/compact.ts`

**Compaction result**:
```typescript
{
  summaryMessages: Message[]              // LLM-generated summary
  attachments: AttachmentMessage[]        // Preserved attachments
  hookResults: Message[]                  // Hook outputs
  preCompactTokenCount: number
  postCompactTokenCount: number
  truePostCompactTokenCount: number
  compactionUsage: { input_tokens, output_tokens, cache_* }
}
```

---

## Reactive Compact (Emergency Recovery)

**Trigger**: 413 prompt-too-long error after context collapse drain failed
**Source**: `services/compact/reactiveCompact.ts`

**Flow**:
1. API returns 413 → response is "withheld" from stream consumer
2. `tryReactiveCompact()` attempts full LLM summarization
3. If successful: yield post-compact messages, retry API call
4. If failed: surface the withheld 413 error

**Guard**: `hasAttemptedReactiveCompact` flag prevents infinite loops

---

## Token Tracking

### Token Counting (`utils/tokens.ts`)
- `tokenCountWithEstimation(messages)` — Estimates tokens from last API response usage + new messages
- `finalContextTokensFromLastResponse(messages)` — Extracts final context window from last assistant's usage
- `doesMostRecentAssistantMessageExceed200k(messages)` — Used for plan mode model selection

### Token Warning States (`services/compact/autoCompact.ts`)
```typescript
calculateTokenWarningState(tokenCount, model) → {
  isAtWarningLimit: boolean      // Show yellow warning
  isAtBlockingLimit: boolean     // Block API calls (reserve space for manual /compact)
}
```

### Context Window Sizes (`utils/context.ts`)
- Per-model context windows (e.g., 200K for most Claude models, 1M for extended context)
- `ESCALATED_MAX_TOKENS` = 64K (used for max_output_tokens escalation recovery)

---

## Token Budget System (`query/tokenBudget.ts`)

Implements "+500K" style budgets for extended agentic turns.

### Budget Tracker State
```typescript
{
  continuationCount: number         // How many times we've continued
  lastDeltaTokens: number          // Tokens used in last iteration
  lastGlobalTurnTokens: number     // Previous global turn token count
  startedAt: number                // When budget tracking started
}
```

### Decision Algorithm
- **Completion threshold**: 90% of budget → stop
- **Diminishing returns**: If `continuationCount >= 3` AND last two deltas < 500 tokens → stop early
- **Subagents**: Always stop immediately (no budget continuation)
- **Under threshold**: Inject nudge message and continue

### Budget Parsing (`utils/tokenBudget.ts`)
Supports formats: `"+500k"`, `"+500K"`, `"use 500000 tokens"`, `"500k tokens"`

---

## Auto-Compact Details

### Trigger Conditions
- Token count exceeds model-specific threshold
- Not blocked by consecutive failure circuit breaker
- Not a compact/session_memory query source (prevents deadlock)

### Forked Subagent Pattern
```
Parent conversation (500K tokens)
  → Fork subagent with CacheSafeParams (shares prompt cache)
  → Subagent summarizes conversation history
  → Returns summary messages
  → Parent replaces history with summary + recent messages
```

**Benefits**:
- Non-blocking (parent waits but doesn't lose state)
- Shared prompt cache (no re-encoding cost)
- Isolated state (subagent can't modify parent)

### Post-Compact Cleanup
- `buildPostCompactMessages(compactionResult)` — Constructs new message array
- Reset autoCompactTracking (turnCounter, turnId)
- Update taskBudgetRemaining if task_budget is active

---

## Key Source Files

| File | What to reference |
|------|-------------------|
| `services/compact/autoCompact.ts` | `autoCompactIfNeeded()`, threshold calculation, tracking state |
| `services/compact/compact.ts` | `buildPostCompactMessages()`, compaction result type |
| `services/compact/microCompact.ts` | `microcompactMessages()`, lightweight reduction |
| `services/compact/reactiveCompact.ts` | `tryReactiveCompact()`, `isReactiveCompactEnabled()` |
| `services/compact/snipCompact.ts` | `snipCompactIfNeeded()`, history trimming |
| `services/compact/grouping.ts` | Message grouping strategy for compaction |
| `services/compact/prompt.ts` | Compaction system prompts |
| `services/contextCollapse/` | Context collapse staging and commit |
| `utils/tokens.ts` | Token counting and estimation |
| `utils/context.ts` | Context window sizes, ESCALATED_MAX_TOKENS |
| `query/tokenBudget.ts` | Budget tracking and continuation decisions |
| `query.ts` (lines 400-650) | Where all compaction layers are applied per iteration |

---

## Rust Translation Notes

- **Forked subagent** → `tokio::spawn` with cloned state + shared prompt cache via `Arc`
- **Token counting** → `tiktoken-rs` or custom BPE tokenizer for your LLM
- **Auto-compact** → Background task with channel to return summary
- **Context collapse** → Staged state machine with commit log
- **Budget tracker** → Simple struct with `check()` method returning enum decision
- **Circuit breaker** → `consecutiveFailures` counter with threshold

---

## Phase 4 calibration notes (validated 2026-04-17 during `elena-context` + `elena-memory` + `elena-router` implementation)

These are the adjustments between the reference doc / TS source and what
Elena actually ships:

1. **Retrieval over compaction (confirmed).** Elena explicitly does NOT
   replicate the 4-layer compaction pipeline (snip, microcompact, collapse,
   auto-compact). `ContextManager::build_context` uses semantic retrieval
   (`ThreadStore::similar_messages` over the pgvector HNSW index) +
   chronological packing + a 10% token-headroom safety margin. On-demand
   `Summarizer` is available for thread tails > 200 turns but not wired
   into `ContextManager` by default — the hook exists so Phase 5 can
   trigger it when token overflow signals surface.

2. **Message embeddings are synchronous at commit time.** The loop
   (`step.rs::handle_streaming`) calls
   `context.embed_and_store(tenant, msg_id, text, store)` right after
   committing the assistant message. This trades ~3–5 ms per commit for
   retrieval correctness — without it, `similar_messages` would return
   empty and `build_context` would silently fall back to recency. Phase 5
   can move to a background queue if hot-path latency matters.

3. **Episodic memory scope is per-workspace.** Episodes carry
   `tenant_id + workspace_id + task_summary + actions + outcome +
   embedding` (reusing the pgvector HNSW schema provisioned in Phase 1).
   Recording is fire-and-forget (`tokio::spawn` in the loop driver). Recall
   runs in `handle_received` and injects a "Relevant prior sessions in
   this workspace:" preamble into `LoopState::system_prompt` — this keeps
   episodes orthogonal to the `messages` array so cache hashing stays
   stable for repeated threads.

4. **Routing is heuristic, not ML.** A deterministic rule set handles
   `route()` (tier based on message length, turn depth, tool count,
   tenant tier, error-recovery count). `cascade_check()` escalates on
   refusal / hallucinated-tool / empty / too-brief output signals, capped
   at 2 tier bumps per turn, always a no-op at `Premium`. The ML
   classifier the architecture doc describes waits on production training
   data — Phase 4's rules are the floor, not the ceiling.

5. **Cascade is reactive and non-destructive.** When cascade fires, the
   loop escalates `state.model_tier`, increments
   `state.recovery.model_escalations`, and **does not commit** the rejected
   assistant message. The next call at the higher tier is the one that
   persists. Clients receive the rejected turn's text deltas via the event
   stream as it happens — Phase 5 can choose to buffer-and-suppress if
   user-visibility of false starts becomes a problem.

6. **Embedding model is operator-provided.** `ContextConfig` carries
   optional `embedding_model_path` + `tokenizer_path`. `OnnxEmbedder::load`
   reads both; when either is unset a `NullEmbedder` is installed and the
   manager degrades to a pure recency window. The e5-small-v2 model file
   (~130 MB) and `libonnxruntime.dylib` are operator responsibilities —
   CI runs with `NullEmbedder`, production deployments bake both into the
   container image.

7. **Token counting is character-based.** Elena approximates tokens as
   `chars / 3.2` with a 10% headroom in the packer. Precise tokenization
   would require either Anthropic's `count_tokens` endpoint (per-message
   round trip, too expensive on hot path) or a reverse-engineered Claude
   tokenizer (not available as of Phase 4). The approximation is
   deliberately conservative — a pack that fits by this estimate will fit
   the real tokenizer.

8. **Tool-result blocks are not embedded.** Phase 4 embeds user + assistant
   text only. Tool results (especially structured JSON) would noise-dominate
   the vector index; revisit if retrieval recall proves insufficient.
