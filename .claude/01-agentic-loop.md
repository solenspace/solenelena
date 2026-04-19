# The Agentic Loop (`query.ts`)

> **Source file**: `query.ts` (1,730 lines)
> **Helper files**: `query/config.ts`, `query/deps.ts`, `query/stopHooks.ts`, `query/tokenBudget.ts`

---

## What It Does

The agentic loop is an **async generator** that runs a `while(true)` loop. Each iteration:
1. Pre-processes context (compaction, snipping)
2. Calls the LLM API (streaming)
3. Processes the streaming response (text + tool_use blocks)
4. Executes extracted tools (parallel for read-only, serial for writes)
5. Appends tool results to messages and loops back

---

## Core Types

### QueryParams (input to the loop)
```typescript
{
  messages: Message[]                    // Conversation history
  systemPrompt: SystemPrompt            // System prompt content
  userContext: { [k: string]: string }   // User context key-value pairs
  systemContext: { [k: string]: string } // System context key-value pairs
  canUseTool: CanUseToolFn              // Permission checker function
  toolUseContext: ToolUseContext          // Tool execution context (tools, options, state)
  fallbackModel?: string                 // Model to switch to on failure
  querySource: QuerySource               // Where this query originated
  maxOutputTokensOverride?: number       // Override max output tokens
  maxTurns?: number                      // Maximum tool execution turns
  taskBudget?: { total: number }         // Token budget for the whole agentic turn
}
```

### State (mutable, carried between loop iterations)
```typescript
{
  messages: Message[]
  toolUseContext: ToolUseContext
  autoCompactTracking: AutoCompactTrackingState | undefined
  maxOutputTokensRecoveryCount: number      // Tracks recovery attempts (max 3)
  hasAttemptedReactiveCompact: boolean      // Prevents infinite compact loops
  maxOutputTokensOverride: number | undefined
  pendingToolUseSummary: Promise<ToolUseSummaryMessage | null> | undefined
  stopHookActive: boolean | undefined
  turnCount: number                         // Incremented per tool execution turn
  transition: Continue | undefined          // Why the previous iteration continued
}
```

### QueryConfig (immutable, snapshotted once at entry)
```typescript
{
  sessionId: SessionId
  gates: {
    streamingToolExecution: boolean   // Feature gate
    emitToolUseSummaries: boolean     // Generate Haiku summaries of tool use
    isAnt: boolean                    // Internal user detection
    fastModeEnabled: boolean
  }
}
```

### QueryDeps (dependency injection for testing)
```typescript
{
  callModel: typeof queryModelWithStreaming   // LLM API call
  microcompact: typeof microcompactMessages  // Lightweight compaction
  autocompact: typeof autoCompactIfNeeded    // Full LLM-based compaction
  uuid: () => string                         // UUID generator
}
```

---

## Loop Iteration Flow (per turn)

### Phase 1: Pre-Processing (lines ~307-650)

| Step | Source | What happens |
|------|--------|-------------|
| Skill prefetch | `skillPrefetch.startSkillDiscoveryPrefetch()` | Fire in background |
| Query tracking | Lines ~347-363 | Increment chain depth, generate chain ID |
| Tool result budget | `applyToolResultBudget()` | Cap per-message tool result sizes |
| Snip compact | `snipModule.snipCompactIfNeeded()` | Remove old messages (if HISTORY_SNIP) |
| Microcompact | `deps.microcompact()` | Lightweight context reduction |
| Context collapse | `contextCollapse.applyCollapsesIfNeeded()` | Project collapsed view |
| System prompt | `asSystemPrompt(appendSystemContext(...))` | Build full system prompt |
| Auto-compact | `deps.autocompact()` | Full LLM summarization if threshold exceeded |
| Blocking check | `calculateTokenWarningState()` | Prevent call if context too large |

### Phase 2: API Call (lines ~650-1000)

| Step | Source | What happens |
|------|--------|-------------|
| Model selection | `getRuntimeMainLoopModel()` | Based on permission mode, fast mode, tokens |
| Stream API call | `deps.callModel()` | Streaming request to LLM |
| Event processing | Inner for-await loop | Accumulate assistant messages, extract tool_use blocks |
| Streaming tools | `streamingToolExecutor.addTool()` | Start tool execution as blocks stream in |
| Fallback | `FallbackTriggeredError` catch | Switch to fallback model on error |
| Tombstones | Yield tombstone messages | Remove orphaned messages on fallback |

### Phase 3: Post-API Recovery (lines ~1000-1360)

| Step | Source | What happens |
|------|--------|-------------|
| Post-sampling hooks | `executePostSamplingHooks()` | Fire-and-forget |
| Abort handling | Lines ~1015-1052 | Yield synthetic tool_results, cleanup |
| Tool use summary | Await `pendingToolUseSummary` | Yield Haiku-generated summary |
| Prompt-too-long (413) | Context collapse drain → reactive compact | Two-stage recovery |
| Max output tokens | Escalate to 64K → multi-turn recovery (3x) | Lines ~1188-1256 |
| Stop hooks | `handleStopHooks()` | Memory extraction, hooks, teammate hooks |
| Token budget | `checkTokenBudget()` | Continue or stop based on budget |

### Phase 4: Tool Execution (lines ~1360-1520)

| Step | Source | What happens |
|------|--------|-------------|
| Execute tools | `streamingToolExecutor.getRemainingResults()` or `runTools()` | Run all tool_use blocks |
| Hook prevention | Check `shouldPreventContinuation` | Stop if hook blocked |
| Tool use summary | `generateToolUseSummary()` | Fire Haiku summary (consumed next turn) |
| Abort during tools | Lines ~1485-1516 | Yield synthetic results, check maxTurns |

### Phase 5: Post-Tool Processing (lines ~1520-1728)

| Step | Source | What happens |
|------|--------|-------------|
| Attachments | `getAttachmentMessages()` | File change notifications, queued commands |
| Memory prefetch | `filterDuplicateMemoryAttachments()` | Inject relevant memories if settled |
| Skill discovery | `skillPrefetch.collectSkillDiscoveryPrefetch()` | Inject discovered skills |
| Command drain | `getCommandsByMaxPriority()` + `removeFromQueue()` | Consume queued commands |
| MCP refresh | `refreshTools()` | Refresh tools from newly-connected MCP servers |
| Task summary | `taskSummaryModule.maybeGenerateTaskSummary()` | For `claude ps` display |
| Max turns check | Lines ~1705-1712 | Stop if limit reached |
| Loop back | Create next State → `continue` | Build messages array, increment turnCount |

---

## Recovery Mechanisms

| Recovery | Trigger | What it does | Max attempts |
|----------|---------|-------------|--------------|
| Context collapse drain | 413 prompt-too-long | Commit staged collapses, retry | 1 (then falls through) |
| Reactive compact | 413 after collapse failed | Full LLM summarization, retry | 1 (hasAttemptedReactiveCompact flag) |
| Max output escalation | Output token limit | Retry with ESCALATED_MAX_TOKENS (64K) | 1 (only if no override set) |
| Max output recovery | 64K still hit limit | Inject "continue" meta message | 3 (MAX_OUTPUT_TOKENS_RECOVERY_LIMIT) |
| Model fallback | FallbackTriggeredError | Switch to fallbackModel, retry entire API call | 1 |
| Non-streaming fallback | Streaming failure | Fall back to non-streaming API call | 1 |
| Token budget continuation | Budget < 90% used | Inject nudge message | Until 90% or diminishing returns |

---

## Terminal Conditions

| Reason | When |
|--------|------|
| `completed` | Normal — no tool_use in response, stop hooks passed |
| `aborted_streaming` | User interrupted during API streaming |
| `aborted_tools` | User interrupted during tool execution |
| `blocking_limit` | Context too large, can't compact |
| `prompt_too_long` | 413 after all recovery exhausted |
| `image_error` | Media size rejection after recovery |
| `model_error` | Non-retryable API error |
| `max_turns` | Turn limit reached |
| `hook_stopped` | Tool hook prevented continuation |
| `stop_hook_prevented` | Stop hook blocked |

---

## Key Source Files to Reference

| File | What to look at |
|------|----------------|
| `query.ts` | The entire agentic loop — read top to bottom |
| `query/config.ts` | QueryConfig type and buildQueryConfig() |
| `query/deps.ts` | QueryDeps type (DI interface) |
| `query/stopHooks.ts` | Post-turn lifecycle (474 lines) — memory, hooks, teammate hooks |
| `query/tokenBudget.ts` | Budget continuation logic with diminishing returns detection |
| `services/tools/toolOrchestration.ts` | Tool partitioning (concurrent vs serial) |
| `services/tools/StreamingToolExecutor.ts` | Execute tools as they stream in |
| `utils/messages.ts` | Message creation utilities used throughout |
| `utils/tokens.ts` | Token counting and estimation |

---

## Rust Translation Notes

- **Async generator** → Use `tokio::sync::mpsc::channel<Event>` where the loop body sends events to a receiver
- **`yield`** → `tx.send(event).await`
- **State struct** → Pass explicitly between iterations (Rust has no closures over mutable state like JS)
- **AbortController** → `tokio_util::sync::CancellationToken` — check `.is_cancelled()` at the same points
- **Promise.all for concurrent tools** → `tokio::spawn` + `FuturesUnordered` or `tokio::join!`
- **Feature flags** → `#[cfg(feature = "history_snip")]` etc.
- **Forked subagent** → Spawn a `tokio::task` with its own state clone
- **Error recovery** → Match on error enum variants, use `continue` for retry paths

---

## Phase 3 calibration notes (validated 2026-04-17 during `elena-core` implementation)

These corrections emerged while translating the reference TS loop into Rust:

1. **Checkpoint invariant**: `query.ts` is in-memory only — state lives in a
   closure variable and is lost on crash. Elena's distributed worker model
   requires persisting `LoopState` to Redis. The loop driver does this
   after each successful `step()`; committed Postgres messages are the
   authoritative record, the Redis snapshot is a fast-resume hint that
   expires with the thread claim TTL.

2. **Phase simplification for Phase 3**: The architecture doc's 9 phases
   (`Received → BuildingContext → Routing → Streaming → …`) collapsed to
   6 because `elena-router`, `elena-context`, and `elena-memory` are
   Phase 4. A single `Streaming` handler fetches recent messages directly
   from `ThreadStore` (last-N recency window, default 100).

3. **Inner `Done` must not leak out**: `AnthropicClient::stream` emits
   `StreamEvent::Done(Terminal::Completed)` when the LLM turn ends. The
   loop's outer `Done` represents end-of-loop (multi-turn), not
   end-of-LLM-call. The `stream_consumer` filters the inner `Done` so
   callers see exactly one terminal event from `run_loop`.

4. **Terminal variants for budget / cancellation**: The source enum lacks
   a dedicated `BudgetExhausted`. Phase 3 reuses `Terminal::BlockingLimit`
   (semantically "tenant-level limit hit") with a preceding
   `StreamEvent::Error(ElenaError::BudgetExceeded { .. })` so clients can
   distinguish budget from other blocking limits.

5. **Mid-turn tool-loop signal**: The state machine needs to know whether
   the previous turn emitted tool calls so `PostProcessing` can decide
   "loop back to stream" vs "complete". Stored as
   `LoopState::last_turn_had_tools`; reset to `false` at the start of
   every `Streaming` phase.

6. **`Permission::Ask` handling**: No prompt channel exists until Phase 5
   (gateway). Phase 3 treats `Ask` as `Allow` and logs at `WARN` with a
   `PHASE5:` marker so all sites that need a real prompt route are easy
   to find later.

7. **Thinking blocks are not persisted**: The wire `ThinkingDelta`
   doesn't carry the `signature` field that `ContentBlock::Thinking`
   requires, and Phase 3 has no compaction layer that would need it.
   Thinking deltas are tracked for delta-grouping but dropped from the
   assembled message. Phase 4 can surface signatures when
   microcompact / cache-break detection needs them.
