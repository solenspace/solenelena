# State Management & Bootstrap

> **Source files**:
> - `bootstrap/state.ts` — Global state store (1,758 lines, 350+ getter/setters)
> - `state/AppStateStore.ts` — UI state store (Zustand-like)
> - `state/store.ts` — Minimal store implementation
> - `state/selectors.ts` — Derived state selectors
> - `entrypoints/init.ts` — Initialization sequence
> - `utils/sessionStorage.ts` — Session persistence
> - `utils/sessionRestore.ts` — Session restoration

---

## Global State (`bootstrap/state.ts`)

Centralized singleton holding all runtime state. **350+ getter/setter functions**.

### Key State Categories

**Project/Working Directory:**
- `originalCwd`, `projectRoot`, `cwd`

**Cost & Performance Tracking:**
- `totalCostUSD`, `totalAPIDuration`, `totalToolDuration`
- `totalLinesAdded`, `totalLinesRemoved`
- Per-turn metrics: `turnHookDurationMs`, `turnToolDurationMs`, `turnToolCount`

**Model & Configuration:**
- `modelUsage: { [modelName]: ModelUsage }`
- `mainLoopModelOverride`, `initialMainLoopModel`
- `modelStrings` (canonical model name resolution)

**Session Identity:**
- `sessionId: SessionId` (branded string type)
- `parentSessionId` (for subagents)

**Authentication:**
- `sessionIngressToken`, `oauthTokenFromFd`, `apiKeyFromFd`

**Telemetry (OpenTelemetry):**
- `meter`, `sessionCounter`, `locCounter`, `prCounter`, `commitCounter`
- `costCounter`, `tokenCounter`, `activeTimeCounter`
- `meterProvider`, `loggerProvider`, `tracerProvider`

**Feature Flags & Runtime:**
- `isInteractive`, `kairosActive`, `strictToolResultPairing`
- `scheduledTasksEnabled`, `sessionCronTasks`
- `isRemoteMode`, `directConnectServerUrl`

**Prompt Caching Latches:**
- `promptCache1hAllowlist`, `afkModeHeaderLatched`
- `fastModeHeaderLatched`, `cacheEditingHeaderLatched`

**Teams/Swarm:**
- `sessionCreatedTeams: Set<string>`
- `agentColorMap: Map<string, AgentColorName>`

---

## App State Store (`state/AppStateStore.ts`)

UI-facing state store using a Zustand-like pattern.

### Store Implementation (`state/store.ts`)
```typescript
type Store<T> = {
  getState(): T                          // Synchronous read
  setState((prev: T) => T): void         // Synchronous update
  subscribe(listener): () => void        // Listener registration
  // Internal: onChange callback fires on state mutations
}
```

### AppState Shape (key fields)
```typescript
{
  // Conversation
  messages: Message[]
  userMessage?: string
  selectedMessageIndex: number

  // UI
  expandedView: 'none' | 'tasks' | 'teammates'
  statusLineText, spinnerTip
  verbose: boolean

  // Model
  mainLoopModel: ModelSetting
  mainLoopModelForSession: ModelSetting
  settings: DeepImmutable<SettingsJson>
  fastMode: boolean
  effortValue: EffortValue

  // Permissions
  toolPermissionContext: {
    mode: PermissionMode
    alwaysAllowRules, alwaysDenyRules, alwaysAskRules
    additionalWorkingDirectories: string[]
  }

  // Bridge
  replBridgeEnabled, replBridgeConnected
  replBridgeSessionUrl, replBridgeEnvironmentId

  // Remote
  remoteSessionUrl, remoteConnectionStatus
  remoteBackgroundTaskCount

  // Tasks
  tasks: Record<AgentId, TaskState>

  // MCP
  mcp: { tools: McpTool[], clients: McpClient[] }

  // Notifications
  notifications: { current: Notification | null, queue: Notification[] }

  // Speculation (AI suggestions)
  speculation: SpeculationState
}
```

### Selectors (`state/selectors.ts`)
- `getViewedTeammateTask(appState)` — Currently viewed teammate
- `getActiveAgentForInput(appState)` — Routes user input (leader | viewed | named_agent)

---

## Initialization Sequence (`entrypoints/init.ts`)

### `init()` (memoized — runs once)
1. `enableConfigs()` — Validate config files
2. `applySafeConfigEnvironmentVariables()` — Apply env vars before trust dialog
3. `applyExtraCACertsFromConfig()` — TLS cert setup
4. `setupGracefulShutdown()` — Register cleanup handlers
5. **Fire-and-forget (parallel)**:
   - OAuth account info population
   - JetBrains IDE detection
   - GitHub repository detection
   - Remote managed settings loading
   - Policy limits loading
6. `configureGlobalMTLS()` — mTLS setup
7. `configureGlobalAgents()` — Proxy + mTLS agents
8. `preconnectAnthropicApi()` — TCP+TLS warmup (overlaps with ~100ms startup work)
9. Shell/Git setup

### `initializeTelemetryAfterTrust()`
- Waits for remote managed settings (non-blocking)
- Re-applies env vars (to include remote settings)
- Initializes OpenTelemetry (metrics, logs, traces)

---

## Session Management

### Session Storage (`utils/sessionStorage.ts`)
- Stores messages to disk for session persistence
- Stores mode (coordinator/normal) for resume detection
- Content replacement records for tool result budget

### Session Restoration (`utils/sessionRestore.ts`)
- Reads messages from disk
- Validates message integrity
- Rebuilds ToolUseContext from stored state

### Session Identity (`types/ids.ts`)
```typescript
type SessionId = string & { __brand: 'SessionId' }
type AgentId = string & { __brand: 'AgentId' }
// AgentId format: 'a' + optional label + 16 hex chars
```

---

## Key Source Files

| File | What to reference |
|------|-------------------|
| `bootstrap/state.ts` | Global state — all getter/setters (350+) |
| `state/AppStateStore.ts` | UI state shape and mutations |
| `state/store.ts` | Store<T> implementation (getState/setState/subscribe) |
| `state/selectors.ts` | Derived state selectors |
| `state/AppState.tsx` | Provider wrapping AppStateStore with MCP + Voice + Mailbox |
| `entrypoints/init.ts` | Initialization sequence |
| `entrypoints/cli.tsx` | Fast-path detection at startup |
| `utils/sessionStorage.ts` | Session persistence |
| `utils/sessionRestore.ts` | Session restoration |
| `utils/sessionState.ts` | Session state tracking (idle/running/requires_action) |
| `types/ids.ts` | Branded ID types (SessionId, AgentId) |

---

## Rust Translation Notes

- **Global state** → `Arc<RwLock<GlobalState>>` or separate `OnceLock` for immutable parts
- **AppState store** → `Arc<RwLock<AppState>>` with `tokio::sync::watch` for subscriptions
- **Branded types** → Newtype pattern: `struct SessionId(String);`
- **Memoized init** → `std::sync::OnceLock` or `tokio::sync::OnceCell`
- **Session storage** → `serde_json` serialization to disk
- **Telemetry** → `tracing` + `opentelemetry` crates
- **Graceful shutdown** → `tokio::signal::ctrl_c()` + `CancellationToken` propagation
