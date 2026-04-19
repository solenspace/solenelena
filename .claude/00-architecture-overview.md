# Architecture Overview

## Codebase Identity

- **What**: Claude Code CLI — an agentic loop that streams LLM responses, extracts tool calls, executes tools, and loops
- **Runtime**: Bun (TypeScript, not Node.js)
- **UI**: React + Ink (custom terminal UI with virtual DOM and Yoga layout)
- **Build**: Bun bundler with `feature('FLAG')` for dead code elimination
- **Total**: ~512,664 lines across ~1000+ TypeScript files

---

## Directory Map (by size)

| Directory | LOC | Purpose |
|-----------|-----|---------|
| `utils/` | 180,472 | Core utilities — messages, tokens, permissions, settings, hooks, model selection, bash parsing, plugins, swarm coordination |
| `components/` | 81,546 | React/Ink TUI components (144 files, 15+ subdirectories) |
| `services/` | 53,680 | API layer, MCP, compaction, analytics, session memory, team sync, plugins, OAuth |
| `tools/` | 50,828 | 44+ tools (Read, Edit, Bash, Agent, etc.) + tool interface definition |
| `commands/` | 26,428 | 150+ slash commands (session, git, config, skills, MCP, auth) |
| `ink/` | 19,842 | Custom terminal UI framework — DOM, rendering pipeline, events, reconciler |
| `hooks/` | 19,204 | 47 React hooks for TUI (typeahead, voice, bridge, virtual scroll) |
| `bridge/` | 12,613 | Remote execution bridge connecting local CLI to claude.ai web |
| `cli/` | 12,353 | CLI handlers, output formatting, network transports (WS/SSE) |
| `screens/` | 5,977 | Main screens — REPL (895KB!), Doctor, ResumeConversation |
| `native-ts/` | 4,081 | Native TypeScript helpers |
| `skills/` | 4,066 | Skill loading (bundled/disk/MCP), 15 bundled skills, conditional activation |
| `entrypoints/` | 4,051 | CLI bootstrap with fast-path detection, SDK type definitions |
| `types/` | 3,446 | Message, command, permission, hook, plugin type definitions |
| `tasks/` | 3,286 | Task types — LocalShellTask, LocalAgentTask, RemoteAgentTask, DreamTask |
| `keybindings/` | 3,159 | Keyboard shortcut system |
| `constants/` | 2,648 | Prompt constants, tool name constants, query source constants |
| `bootstrap/` | 1,758 | Global state store with 350+ getter/setter functions |
| `memdir/` | 1,736 | Memory directory management |
| `vim/` | 1,513 | Complete vim mode — pure functional state machine |
| `buddy/` | 1,298 | Buddy system |
| `state/` | 1,190 | AppState store (Zustand-like pattern), selectors |
| `remote/` | 1,127 | Remote CCR session management (WebSocket + HTTP) |
| `context/` | 1,004 | React context providers (voice, notifications, modal, stats, mailbox) |
| `query/` | 652 | Query loop helpers — config, deps, stopHooks, tokenBudget |
| `coordinator/` | 369 | Multi-agent coordinator mode |

---

## Top-Level Source Files

| File | LOC | Purpose |
|------|-----|---------|
| `query.ts` | 1,730 | **THE AGENTIC LOOP** — async generator, streaming + tool execution + recovery |
| `main.tsx` | ~4,500+ | Application entry — Commander.js setup, 52+ subcommands, session launch |
| `Tool.ts` | ~300 | Tool interface definition, `findToolByName`, `ToolUseContext` type |
| `tools.ts` | ~400 | Tool registry — `getAllBaseTools()`, `getTools()`, `assembleToolPool()` |
| `commands.ts` | ~300 | Command registry — `COMMANDS()`, `findCommand()`, `getCommand()` |
| `context.ts` | ~200 | Context window management |
| `tasks.ts` | ~200 | Task management |
| `cost-tracker.ts` | ~150 | Cost tracking utilities |
| `setup.ts` | ~100 | Setup utilities |

---

## Application Startup Flow

```
bun run claude [options] [prompt]
│
├── entrypoints/cli.tsx
│   ├── Fast-path checks (zero-import for --version)
│   │   --version, --daemon, remote-control, ps/logs/attach,
│   │   --bg/--background, new/list/reply, environment-runner
│   └── Full initialization ↓
│
├── main.tsx
│   ├── Commander.js program initialization
│   ├── Register 52+ subcommands (mcp, auth, plugin, doctor, etc.)
│   ├── Parse options (--model, --permission-mode, --print, etc.)
│   └── Main action handler ↓
│
├── entrypoints/init.ts
│   ├── enableConfigs() — validate config files
│   ├── applySafeConfigEnvironmentVariables()
│   ├── applyExtraCACertsFromConfig() — TLS setup
│   ├── setupGracefulShutdown()
│   ├── Fire-and-forget: OAuth, JetBrains detection, git repo detection
│   ├── configureGlobalMTLS(), configureGlobalAgents()
│   ├── preconnectAnthropicApi() — TCP+TLS warmup
│   └── initializeTelemetryAfterTrust() — OpenTelemetry
│
├── bootstrap/state.ts — Global state initialization
│
├── screens/REPL.tsx — Interactive session loop
│
└── query.ts — Agentic loop (per user turn)
```

---

## Design Patterns Used Throughout

### 1. Feature Flags (Build-Time DCE)
```typescript
if (feature('COORDINATOR_MODE')) {
  // Dead code eliminated in external builds
}
```

### 2. Lazy Loading
```typescript
const status = {
  type: 'local-jsx',
  load: () => import('./status.js'),  // Deferred until invoked
}
```

### 3. Memoization
```typescript
const COMMANDS = memoize((): Command[] => [ ... ])
```

### 4. Forked Agent Pattern
Background work via isolated subagent sharing parent's prompt cache:
```typescript
await runForkedAgent({ ...cacheSafeParams, systemPrompt, userMessage })
```
Used by: SessionMemory, ExtractMemories, AutoDream, AgentSummary, MagicDocs

### 5. Async Generator (Core Loop)
```typescript
async function* query(params): AsyncGenerator<StreamEvent | Message, Terminal> {
  while (true) { /* stream, execute tools, loop */ }
}
```

### 6. Fail-Safe / Fail-Open
Network/service errors never crash. Graceful degradation with stale cache fallback.

### 7. ETag Caching
HTTP caching for remote settings, policies, team memory. Minimizes bandwidth.

### 8. Stream Partitioning
Read-only tools (Read, Grep, Glob) execute concurrently; write tools (Edit, Bash) execute serially.

---

## Rust Equivalent Quick Reference

| TypeScript Pattern | Rust Equivalent |
|-------------------|-----------------|
| `async function*` (generator) | `tokio::sync::mpsc::channel` or `async_stream::stream!` |
| `yield` | `tx.send(event).await` |
| `AbortController` | `tokio_util::sync::CancellationToken` |
| Zod schemas | `serde` + custom validation |
| React/Ink TUI | `ratatui` + `crossterm` |
| `Promise.all` for tools | `tokio::join!` or `FuturesUnordered` |
| `feature('FLAG')` | `#[cfg(feature = "...")]` |
| Lazy `import()` | Compile-time with feature gates |
| Zustand store | `Arc<RwLock<State>>` with `tokio::sync::watch` |
| Anthropic SDK | Custom `reqwest` client with SSE parser |
| MCP protocol | `rmcp` crate or custom implementation |
| Memoize | `once_cell::sync::Lazy` or `std::sync::OnceLock` |
