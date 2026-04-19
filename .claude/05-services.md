# Services Layer

> **Source directory**: `services/` (20 subdirectories + 15 root-level modules, ~53,680 lines)

---

## Service Map

### Critical Path Services (used every query)

| Service | Directory | Purpose | Key Files |
|---------|-----------|---------|-----------|
| **API** | `services/api/` | LLM API calls with streaming | `claude.ts`, `client.ts`, `withRetry.ts` |
| **Tool Execution** | `services/tools/` | Tool orchestration and execution | `toolOrchestration.ts`, `StreamingToolExecutor.ts`, `toolExecution.ts` |
| **Compaction** | `services/compact/` | Context reduction (4 layers) | `autoCompact.ts`, `microCompact.ts`, `compact.ts` |

### Background Services (run periodically via forked agents)

| Service | Directory | Purpose | Key Files |
|---------|-----------|---------|-----------|
| **Session Memory** | `services/SessionMemory/` | Auto-maintained conversation notes | `sessionMemory.ts`, `prompts.ts` |
| **Extract Memories** | `services/extractMemories/` | Durable memory extraction at query end | Single module |
| **Auto Dream** | `services/autoDream/` | Background memory consolidation | Time/session gated |
| **Agent Summary** | `services/AgentSummary/` | 3-5 word agent progress summaries (~30s) | Forked agent |
| **Magic Docs** | `services/MagicDocs/` | Auto-updating documentation files | Detects "# MAGIC DOC" headers |
| **Prompt Suggestion** | `services/PromptSuggestion/` | Next-query suggestions | Feature-gated |

### Integration Services

| Service | Directory | Purpose | Key Files |
|---------|-----------|---------|-----------|
| **MCP** | `services/mcp/` | Model Context Protocol (23 files) | `client.ts`, `config.ts`, `auth.ts` |
| **Analytics** | `services/analytics/` | Event logging + feature flags | `index.ts`, `sink.ts`, `growthbook.ts` |
| **OAuth** | `services/oauth/` | PKCE OAuth 2.0 flow | `index.ts`, `client.ts`, `crypto.ts` |
| **Plugins** | `services/plugins/` | Plugin lifecycle management | Background install, reconciliation |

### Enterprise Services

| Service | Directory | Purpose | Key Files |
|---------|-----------|---------|-----------|
| **Remote Managed Settings** | `services/remoteManagedSettings/` | Org policy distribution | ETag caching, hourly polling |
| **Policy Limits** | `services/policyLimits/` | Feature restrictions per org | Same pattern as managed settings |
| **Team Memory Sync** | `services/teamMemorySync/` | Shared memory across team | Secret scanning, delta upload |
| **Settings Sync** | `services/settingsSync/` | Cross-environment settings sync | Incremental upload, full download |
| **LSP** | `services/lsp/` | Language Server Protocol integration | Server manager, diagnostic registry |

---

## MCP (Model Context Protocol) — Key Service

**23 files**, supports 5 transport types:

| Transport | Use Case |
|-----------|----------|
| `stdio` | Local MCP servers (subprocess) |
| `sse` | Server-sent events (remote) |
| `http` | HTTP request/response |
| `websocket` | Persistent connection |
| `in-process` | VS Code SDK integration |

**Key operations**:
- List tools/resources/prompts from servers
- Execute tool calls on remote servers
- Image resizing + content truncation for responses
- Secret detection (prevent credential leaks)
- OAuth authentication for protected servers

**Key files**: `services/mcp/client.ts`, `services/mcp/config.ts`, `services/mcp/auth.ts`

---

## Analytics System

**Pattern**: Lazy initialization with event queue

```
logEvent('event_name', metadata)
  → If sink attached: send immediately
  → If sink not attached: queue (drained when sink attaches)
```

**Backends**: Datadog + first-party (structured proto events)
**Feature flags**: GrowthBook (cached, non-blocking reads)
**Type safety**: `AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS` forces explicit annotation

---

## Forked Agent Pattern (used by 6+ services)

```typescript
const cacheSafeParams = {
  systemPrompt, userContext, systemContext, toolUseContext,
  forkContextMessages: currentMessages
}
await runForkedAgent({
  ...cacheSafeParams,
  userMessage: "Extract key findings...",
  canUseTool: restrictedCanUseTool  // e.g., deny file writes
})
```

**Benefits**: Non-blocking, shared prompt cache, isolated state, natural cleanup
**Used by**: SessionMemory, ExtractMemories, AutoDream, AgentSummary, MagicDocs, PromptSuggestion

---

## Key Architecture Patterns

### 1. Lazy Initialization
```typescript
const eventQueue: QueuedEvent[] = []
export function attachSink(newSink) { sink = newSink; drainQueue() }
```

### 2. Cached Dynamic Config
```typescript
getFeatureValue_CACHED_MAY_BE_STALE('flag', false)  // Returns immediately, may be stale
```

### 3. Fail-Safe / Fail-Open
```typescript
if (!result.success) {
  return cachedSettings ?? null  // Continue without feature
}
```

### 4. ETag Caching
```typescript
headers: { 'If-None-Match': cachedETag }
if (response.status === 304) return cachedData
```

---

## Rust Translation Notes

- **MCP** → `rmcp` crate or custom implementation over stdio/SSE/HTTP
- **Analytics** → `tracing` crate + custom sink (Datadog, OpenTelemetry)
- **OAuth** → `oauth2` crate with PKCE support
- **Forked agent** → `tokio::spawn` with `Arc<PromptCache>` sharing
- **Lazy init** → `tokio::sync::OnceCell` or `once_cell::sync::Lazy`
- **ETag caching** → `reqwest` with conditional headers
- **Feature flags** → Compile-time `#[cfg]` + runtime `Arc<AtomicBool>` for dynamic flags
