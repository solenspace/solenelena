# API & Streaming Layer

> **Source files**:
> - `services/api/claude.ts` — Main LLM streaming interface
> - `services/api/client.ts` — Multi-backend client creation
> - `services/api/withRetry.ts` — Retry logic with exponential backoff
> - `services/api/errors.ts` — Error classification
> - `services/api/usage.ts` — Token usage tracking
> - `services/api/bootstrap.ts` — Pre-execution client setup

---

## API Call Flow

```
query.ts
  → deps.callModel()
    → queryModelWithStreaming()          [services/api/claude.ts]
      → withRetry() wrapper              [services/api/withRetry.ts]
        → getAnthropicClient()           [services/api/client.ts]
          → anthropic.beta.messages.create({stream: true}).withResponse()
            → Raw SSE stream iteration
              → Event accumulation
                → yield AssistantMessage | StreamEvent | SystemAPIErrorMessage
```

---

## Streaming Event Types

Events arrive as Server-Sent Events (SSE) from the API:

| Event Type | What Happens | Key Data |
|------------|-------------|----------|
| `message_start` | Initialize partialMessage, capture input token usage | `message.usage.input_tokens` |
| `content_block_start` | Allocate content block slot | `content_block.type` (text, tool_use, thinking, server_tool_use) |
| `content_block_delta` | Accumulate incremental content | `delta.type` → text_delta, input_json_delta, thinking_delta, signature_delta |
| `content_block_stop` | Block complete — yield AssistantMessage with full content | `index` identifies which block |
| `message_delta` | Final usage stats, stop_reason, cost | `usage.output_tokens`, `stop_reason` |
| `message_stop` | Stream complete | — |

**Critical detail**: The API sends **cumulative usage totals**, not deltas. `input_tokens` is set in `message_start` and stays constant; only `output_tokens` increments in `message_delta`.

**Tool use accumulation**: `tool_use` blocks receive `input` as a JSON string via multiple `input_json_delta` events. The string is parsed to an object only when the block completes (`content_block_stop`).

---

## Retry Logic (`withRetry.ts`)

### Error → Action Map

| HTTP Status / Error | Action | Notes |
|---------------------|--------|-------|
| 401, 403 | Refresh credentials, retry | Force new client via `getAnthropicClient()` |
| 429, 529 (rate limit) | Exponential backoff | Special fast-mode handling (see below) |
| 408, 409 (timeout/lock) | Always retry | |
| 5xx (server error) | Retry | Except ClaudeAI subscribers on 429 |
| ECONNRESET, EPIPE | Disable keep-alive, reconnect | Stale connection recovery |
| Max tokens overflow | Adjust max_tokens, retry | `context_length_exceeded` → reduce by overflow amount |

### Fast Mode Fallback Logic
When fast mode is active and a 429/529 is received:
- **retry-after < 20s** → Short retry (keep fast mode, preserve prompt cache)
- **retry-after >= 20s** → Trigger cooldown (switch to standard speed model)
- **Org doesn't support fast mode** → Permanent disable

### Consecutive 529 Tracking
- Non-Opus models hitting 3+ consecutive 529s → trigger `FallbackTriggeredError`
- `query.ts` catches this and switches to `fallbackModel`
- Exception: ClaudeAI subscribers never trigger model fallback

### Persistent Retry Mode (unattended sessions)
- Enabled when `isNonInteractiveSession` is true
- 429/529 errors retry **indefinitely** with 5-minute max backoff
- Heartbeat yields every 30s to prevent idle timeout
- 6-hour reset cap for rate limit windows

---

## Authentication Backends (`client.ts`)

| Provider | Auth Method | Client Creation |
|----------|------------|-----------------|
| **Anthropic (1st party)** | API key or OAuth token | `new Anthropic({ apiKey })` |
| **AWS Bedrock** | AWS credentials (SigV4) | `new AnthropicBedrock({ region, credentials })` |
| **Azure Foundry** | API key or DefaultAzureCredential | Custom client with Azure endpoint |
| **Google Vertex** | GCP credentials + project ID | `new AnthropicVertex({ projectId, region })` |

**Client headers injected**:
- `x-app: cli`
- `User-Agent: <version>`
- `X-Claude-Code-Session-Id: <uuid>`
- `x-client-request-id: <uuid>` (1st party only, for timeout correlation)
- Custom headers from `ANTHROPIC_CUSTOM_HEADERS` env
- `x-anthropic-additional-protection: true` (if enabled)

**Client caching**: Same client reused across retries unless auth error (401/403) forces re-creation.

---

## Request Parameter Construction

```typescript
{
  model: normalizeModelStringForAPI(model),
  messages: addCacheBreakpoints(normalizedMessages),
  system: buildSystemPromptBlocks(systemPrompt),
  tools: [...toolSchemas, ...extraToolSchemas],
  tool_choice: options.toolChoice,
  betas: [...dynamicBetas],                              // Beta features enabled
  metadata: getAPIMetadata(),
  max_tokens: retryContext.maxTokensOverride || options.maxOutputTokensOverride,
  thinking: { type: 'adaptive' | 'enabled', budget_tokens?: N },
  temperature: hasThinking ? undefined : 1,               // No temperature with thinking
  context_management: { type: 'auto', budget_tokens?: N },
  speed: isFastMode ? 'fast' : undefined,
  output_config: { effort, task_budget, format },
}
```

### Sticky Beta Latches (cache preservation)
Certain beta headers are "latched" once enabled per session to avoid prompt cache breaks:
- Fast mode header → latched once fast mode used
- Adaptive thinking header → latched for session
- Cache editing header → latched when cached microcompact enabled
- Thinking clear header → latched when thinking signature changes

These survive mid-conversation toggles. The pattern prevents cache invalidation from header changes.

---

## Tool Schema Generation

`toolToAPISchema()` in `utils/api.ts` converts Tool objects to API-compatible JSON:

```typescript
{
  name: tool.name,
  description: await tool.description(),
  input_schema: zodToJsonSchema(tool.inputSchema),
  // OR for MCP tools:
  input_schema: tool.inputJSONSchema,
  cache_control: { type: 'ephemeral' },   // On last tool (prompt cache breakpoint)
  defer_loading: true,                      // For deferred tools (ToolSearch system)
}
```

### Deferred Tool Loading Flow
1. System prompt includes ToolSearchTool
2. Model calls `ToolSearch` with query keywords
3. ToolSearch returns matching tool names as `tool_reference` blocks
4. `extractDiscoveredToolNames()` scans message history for `tool_reference` blocks
5. Next API call includes discovered tools in filtered schema
6. Model can now invoke those tools directly

---

## Non-Streaming Fallback

When streaming fails:
1. Catch error in stream iteration
2. Check `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK` env
3. Call `executeNonStreamingRequest()` with same message context
4. Pre-seed 529 count if streaming error was 529

**Triggers**: 404 during stream creation, 90s idle timeout (no chunks), streaming library errors

---

## Key Source Files

| File | Lines | What to reference |
|------|-------|-------------------|
| `services/api/claude.ts` | Large | `queryModelWithStreaming()`, `paramsFromContext()`, streaming event processing |
| `services/api/client.ts` | ~390 | `getAnthropicClient()`, multi-backend creation, header injection |
| `services/api/withRetry.ts` | Large | `withRetry()`, error classification, backoff logic, fast mode fallback |
| `services/api/errors.ts` | ~60 | Error types, rate limit parsing, `isPromptTooLongMessage()` |
| `services/api/usage.ts` | ~100 | `logAPIQuery()`, `logAPISuccessAndDuration()`, `logAPIError()` |
| `services/api/dumpPrompts.ts` | ~50 | Debug: dump full request body to disk |
| `utils/api.ts` | ~600 | `toolToAPISchema()`, `prependUserContext()`, `appendSystemContext()`, cache breakpoints |
| `utils/tokens.ts` | ~200 | `tokenCountWithEstimation()`, `finalContextTokensFromLastResponse()` |

---

## Rust Translation Notes

- **Anthropic SDK** → Custom HTTP client with `reqwest`. No official Rust SDK exists.
- **SSE streaming** → Parse `reqwest::Response::bytes_stream()` manually or use `eventsource-stream` crate
- **Event types** → Define as Rust enum: `enum StreamEvent { MessageStart, ContentBlockStart, ContentBlockDelta, ... }`
- **withRetry** → Custom retry loop or use `tower::retry::Retry` middleware
- **Client caching** → `Arc<tokio::sync::RwLock<Client>>` for auth refresh
- **Beta latches** → `AtomicBool` or session-level `Arc<Mutex<SessionHeaders>>`
- **Backoff** → `tokio::time::sleep()` with exponential + jitter

---

## Phase 2 calibration notes (validated 2026-04-17 against real TS source)

Ten discrepancies between the architecture doc above and the actual Claude
Code implementation, captured while building `elena-llm`:

1. **SSE event sequence is strict and deterministic.** Order is
   `message_start → content_block_start → (content_block_delta)* →
   content_block_stop → ... → message_delta → message_stop`. Deltas
   *accumulate per `index`* and never reset between blocks.

2. **Tool-use input assembly is plain string concat.** The client accumulates
   `input_json_delta.partial_json` as a raw string across events and only
   parses it when `content_block_stop` arrives. Partial JSON can't be
   validated in isolation.

3. **Usage is cumulative, not delta.** `message_start` and `message_delta`
   both carry full totals. Input-related fields (`input_tokens`,
   `cache_*_input_tokens`) use a `>0` guard: an explicit 0 is "unset," not
   "zero tokens used." Output tokens are always latest-wins.

4. **`metadata.user_id` is required, not optional.** Claude Code always sends
   it as a JSON-encoded string of `{device_id, account_uuid, session_id,
   ...}`. Elena puts `{tenant_id, user_id, workspace_id, session_id}` here.

5. **`temperature` is omitted when `thinking` is set.** Setting
   `temperature: 1.0` alongside `thinking` errors; the idiomatic fix is to
   omit the field entirely and let the server default apply.

6. **Retry backoff curve**: `base · 2^(n-1) + uniform(0, 0.25·base·2^(n-1))`,
   clamped to `max_delay_ms`. Base 500ms, max 32s, 10 attempts.

7. **429 retry is subscriber-conditional** in Claude Code (Pro/Max don't
   retry unless `x-should-retry: true`; Enterprise always retries). Elena
   Phase 2 always retries 429 — tenant-tier-aware gating lands with billing.

8. **`MAX_529_RETRIES = 3` means 3 retries after the initial failure** — four
   total attempts. The 4th consecutive 529 surfaces as `Repeated529`.

9. **`cache_control` goes on exactly one message-level location**: the last
   text block of the last message. Tools never get cache markers. The
   reference enforces this discipline because multiple markers leave stale
   KV pages alive an extra turn.

10. **`cache_reference` on `tool_result` blocks is a field, not a new block
    type.** It points to `tool_use_id` and is used by microcompact to
    selectively evict. Elena defers this to Phase 4 (microcompact lives in
    `elena-context`).

Beta headers seen in the live TS source (sticky-on once enabled):
`prompt-caching-scope-2026-01-05`, `context-management-2025-06-27`,
`fast-mode-2026-02-01`, `advanced-tool-use-2025-11-20`, `afk-mode-2026-01-31`.
Phase 2 emits only the first (when global cache scope applies).
- **Token counting** → `tiktoken-rs` crate or custom BPE tokenizer
