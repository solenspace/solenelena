# Multi-Agent Architecture

> **Source files**:
> - `tools/AgentTool/AgentTool.ts` — Subagent spawning
> - `coordinator/coordinatorMode.ts` — Multi-worker orchestration (369 lines)
> - `bridge/` — Remote execution bridge (31 files, ~12.6K lines)
> - `remote/` — Remote CCR session management (4 files, ~1.1K lines)
> - `utils/swarm/` — Teammate coordination (22 files)
> - `tasks/` — Task types for background agents

---

## Three Multi-Agent Patterns

### Pattern 1: Subagents (Agent Tool)

**What**: Spawned in-process as isolated async tasks sharing the parent's prompt cache
**Source**: `tools/AgentTool/AgentTool.ts`

```
Main agent
  → Agent({ description: "Research auth bug", prompt: "..." })
    → Spawns subagent with own message history
    → Subagent executes tools, streams results
    → Returns summary to parent
```

**Key constraints**:
- **No recursion**: Agents cannot spawn agents (prevents infinite depth)
- **Context isolation**: Subagent starts fresh (no parent conversation history)
- **Prompt cache sharing**: Uses `CacheSafeParams` to reuse parent's cached system prompt
- **Tool filtering**: Subagents get a restricted tool set via `filterToolsForAgent()`

### Pattern 2: Coordinator Mode

**What**: Primary Claude orchestrates parallel workers for research, implementation, and verification
**Source**: `coordinator/coordinatorMode.ts`

```
Coordinator (primary Claude)
  │
  ├─ Agent(research task 1) ─────┐
  │                                ├─ [parallel research]
  ├─ Agent(research task 2) ─────┘
  │
  ├─ Coordinator synthesizes findings
  │
  ├─ SendMessage(worker, "implement fix in file.ts:42")  [serial implementation]
  │
  ├─ Agent(verification task 1) ──┐
  │                                 ├─ [parallel verification]
  └─ Agent(verification task 2) ──┘
```

**Key principles**:
1. **Synthesis before delegation** — Coordinator understands findings before directing work
2. **Worker context isolation** — Every worker prompt is self-contained (exact file paths, line numbers)
3. **Continue vs Spawn** — `SendMessage` reuses worker context; `Agent` starts fresh
4. **Read-only parallel, write serial** — Research in parallel, implementation one-at-a-time
5. **Explicit parallelism** — Multiple `Agent` calls in single response to launch concurrently

**Activation**: `CLAUDE_CODE_COORDINATOR_MODE` env var or feature flag

**Permission handler**: Sequential (hooks → classifier → dialog) for deterministic worker decisions

### Pattern 3: Bridge (Remote Execution)

**What**: Connects local CLI to claude.ai web sessions for remote execution
**Source**: `bridge/` directory

```
claude.ai (web browser)
  → OAuth authentication
  → Environments API (register, poll, acknowledge)
  → Bridge (local machine)
    → Poll for work
    → Spawn child CLI process per session
    → Bidirectional transport (WebSocket v1 or SSE v2)
    → Child executes tools locally
    → Results sent back to web
```

**Transports**:
| Version | Read Channel | Write Channel | Notes |
|---------|-------------|---------------|-------|
| V1 | WebSocket | WebSocket | Stateless, replay via WS |
| V2 | SSE (Server-Sent Events) | CCR Client (HTTP POST) | More efficient, state tracking |

**Spawn modes**:
| Mode | Behavior |
|------|----------|
| `single-session` | One session, bridge shuts down when done |
| `worktree` | Persistent, each session gets isolated git worktree |
| `same-dir` | Persistent, sessions share working directory |

**Authentication**:
- JWT tokens with proactive refresh (5 min before expiry)
- Trusted device tokens (biometric enrollment for elevated security)
- OAuth via keychain

---

## Swarm/Teammate System

**Source**: `utils/swarm/` (22 files)

**What**: Multiple Claude instances running in parallel terminals, sharing permission state

**Backends**:
| Backend | How workers execute |
|---------|-------------------|
| `TmuxBackend` | Tmux panes |
| `ITermBackend` | iTerm2 split panes |
| `InProcessBackend` | In-process (same Node.js process) |

**Key features**:
- Permission synchronization between leader and teammates
- Mailbox-based messaging between agents
- Color-coded agent identification
- Reconnection logic for dropped connections

---

## Task Types for Background Agents

**Source**: `tasks/` directory

| Task Type | Source | What it tracks |
|-----------|--------|---------------|
| `LocalShellTask` | `tasks/LocalShellTask.tsx` | Shell command execution (exit code, output) |
| `LocalAgentTask` | `tasks/LocalAgentTask.tsx` | In-process agent (progress, result, error) |
| `RemoteAgentTask` | `tasks/RemoteAgentTask.tsx` | Cloud session agent (ultraplan phase, diamond UI) |
| `LocalMainSessionTask` | `tasks/LocalMainSessionTask.ts` | Backgrounded main session (Ctrl+B) |
| `InProcessTeammateTask` | `tasks/InProcessTeammateTask.ts` | Teammate agent (mailbox, message cap 50) |
| `DreamTask` | `tasks/DreamTask.ts` | Memory consolidation agent |

---

## Key Source Files

| File | What to reference |
|------|-------------------|
| `tools/AgentTool/AgentTool.ts` | Subagent spawning logic, tool filtering |
| `coordinator/coordinatorMode.ts` | Coordinator system prompt, user context, mode detection |
| `bridge/bridgeMain.ts` | Standalone bridge (poll loop, session spawning) |
| `bridge/replBridge.ts` | REPL bridge (2406 LOC, primary orchestrator) |
| `bridge/bridgeApi.ts` | Environment/work management HTTP client |
| `bridge/sessionRunner.ts` | Child CLI process spawning |
| `remote/RemoteSessionManager.ts` | Remote CCR session coordinator |
| `remote/SessionsWebSocket.ts` | WebSocket client for CCR |
| `remote/sdkMessageAdapter.ts` | SDK → REPL message conversion |
| `utils/swarm/` | Teammate coordination (22 files) |
| `tasks/` | All task types |

---

## Rust Translation Notes

- **Subagents** → `tokio::spawn` with own state + shared `Arc<PromptCache>`
- **Coordinator** → State machine orchestrating spawned tasks
- **Bridge** → WebSocket client (`tokio-tungstenite`) + SSE client (`reqwest` stream)
- **Message routing** → `tokio::sync::mpsc` channels between agents
- **Abort/cancel** → `CancellationToken` propagated to child tasks
- **Swarm** → Separate processes communicating via Unix domain sockets or shared state
- **Task tracking** → `Arc<RwLock<HashMap<AgentId, TaskState>>>` with watch channels for UI
