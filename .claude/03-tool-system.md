# Tool System

> **Source files**:
> - `Tool.ts` — Tool interface definition (~300 lines)
> - `tools.ts` — Tool registry and pool assembly (~400 lines)
> - `tools/` directory — 44+ tool implementations
> - `services/tools/toolOrchestration.ts` — Concurrent/serial partitioning
> - `services/tools/StreamingToolExecutor.ts` — Execute tools as they stream in
> - `services/tools/toolExecution.ts` — Individual tool invocation
> - `services/tools/toolHooks.ts` — Pre/post execution hooks

---

## Tool Interface

Every tool implements this interface (defined in `Tool.ts`):

### Core Methods
| Method | Purpose |
|--------|---------|
| `name` | Unique identifier (e.g., "Read", "Bash", "Agent") |
| `inputSchema` | Zod schema for input validation |
| `call(input, context, canUseTool, parentMessage, onProgress)` | Execute the tool, return ToolResult |
| `description(input?, options?)` | Dynamic tool description for system prompt |
| `prompt()` | Additional system prompt content |

### Classification Methods
| Method | Purpose |
|--------|---------|
| `isConcurrencySafe(input?)` | Can run in parallel with other tools (default: false) |
| `isReadOnly(input?)` | No side effects (default: false) |
| `isDestructive(input?)` | Irreversible operation (default: false) |
| `shouldDefer` | Defer via ToolSearch (don't include in initial prompt) |
| `alwaysLoad` | Always include in system prompt regardless |
| `maxResultSizeChars` | Result truncation threshold |

### Permission Methods
| Method | Purpose |
|--------|---------|
| `checkPermissions(input, context)` | Permission check → allow/deny/ask |
| `preparePermissionMatcher(input)` | Pattern matching for shell rules |
| `toAutoClassifierInput(input)` | Compact input for ML-based approval |
| `getPath(input)` | Extract file path for filesystem permission checks |

### UI Methods
| Method | Purpose |
|--------|---------|
| `renderToolUseMessage(input, options)` | Display when tool is called |
| `renderToolResultMessage(output, progress, options)` | Display results |
| `renderToolUseProgressMessage(progress, options)` | Progress during execution |
| `getActivityDescription(input)` | Human-readable status text |
| `userFacingName(input)` | Display name |

---

## Complete Tool Catalog (44 tools)

### File Operations
| Tool | Source | Read-Only | Concurrent | Notes |
|------|--------|-----------|------------|-------|
| **Read** | `tools/FileReadTool/` | Yes | Yes | Files, images, PDFs, Jupyter notebooks, line ranges |
| **Edit** | `tools/FileEditTool/` | No | No | Find/replace with strict mode, git diff tracking |
| **Write** | `tools/FileWriteTool/` | No | No | Create/overwrite files, file history support |

### Search
| Tool | Source | Read-Only | Concurrent | Notes |
|------|--------|-----------|------------|-------|
| **Glob** | `tools/GlobTool/` | Yes | Yes | File pattern matching |
| **Grep** | `tools/GrepTool/` | Yes | Yes | Ripgrep backend, regex, context lines, output modes |

### Shell
| Tool | Source | Read-Only | Concurrent | Notes |
|------|--------|-----------|------------|-------|
| **Bash** | `tools/BashTool/` | Varies | Varies | Shell execution, timeout, permission-controlled |
| **PowerShell** | `tools/PowerShellTool/` | Varies | Varies | Windows only, feature-gated |

### Notebook
| Tool | Source | Read-Only | Concurrent | Notes |
|------|--------|-----------|------------|-------|
| **NotebookEdit** | `tools/NotebookEditTool/` | No | No | Jupyter cells: insert/replace/delete |

### Web
| Tool | Source | Read-Only | Concurrent | Notes |
|------|--------|-----------|------------|-------|
| **WebFetch** | `tools/WebFetchTool/` | Yes | Yes | URL fetching, content extraction, deferred |
| **WebSearch** | `tools/WebSearchTool/` | Yes | Yes | API beta tool, progress streaming |

### Agent & Task Management
| Tool | Source | Notes |
|------|--------|-------|
| **Agent** | `tools/AgentTool/` | Spawn subagents, not allowed in subagents (no recursion) |
| **TaskCreate** | `tools/TaskCreateTool/` | Create tasks, deferred |
| **TaskGet** | `tools/TaskGetTool/` | Retrieve task by ID |
| **TaskList** | `tools/TaskListTool/` | List tasks with filters |
| **TaskUpdate** | `tools/TaskUpdateTool/` | Update task properties, dependencies |
| **TaskStop** | `tools/TaskStopTool/` | Kill running background tasks |
| **TaskOutput** | — | Internal UI-only tool |
| **TodoWrite** | `tools/TodoWriteTool/` | Legacy v1 checklist (disabled when Task v2 enabled) |
| **SendMessage** | `tools/SendMessageTool/` | Team/swarm communication |

### Skills & Extensions
| Tool | Source | Notes |
|------|--------|-------|
| **Skill** | `tools/SkillTool/` | Invoke skills/plugins, deferred |
| **ToolSearch** | `tools/ToolSearchTool/` | Search deferred tools by keyword |

### Code Analysis
| Tool | Source | Notes |
|------|--------|-------|
| **LSP** | `tools/LSPTool/` | goToDefinition, findReferences, hover, documentSymbol, callHierarchy |

### MCP Integration
| Tool | Source | Notes |
|------|--------|-------|
| **ListMcpResources** | `tools/ListMcpResourcesTool/` | List MCP server resources |
| **ReadMcpResource** | `tools/ReadMcpResourceTool/` | Read MCP server resources |
| **MCPTool** | `tools/MCPTool/` | Generic MCP tool wrapper |

### Plan & Worktree
| Tool | Source | Notes |
|------|--------|-------|
| **EnterPlanMode** | `tools/EnterPlanModeTool/` | Enter plan design mode, deferred |
| **ExitPlanMode** | `tools/ExitPlanModeTool/` | Exit plan mode, not allowed in agents |
| **EnterWorktree** | `tools/EnterWorktreeTool/` | Create isolated git worktree, deferred |
| **ExitWorktree** | `tools/ExitWorktreeTool/` | Exit/cleanup worktree |

### User Interaction
| Tool | Source | Notes |
|------|--------|-------|
| **AskUserQuestion** | `tools/AskUserQuestionTool/` | Interactive prompts, not in agents |
| **Brief** | `tools/BriefTool/` | Send message to user |

### Scheduling
| Tool | Source | Notes |
|------|--------|-------|
| **CronCreate/Delete/List** | `tools/ScheduleCronTool/` | Feature-gated: AGENT_TRIGGERS |
| **RemoteTrigger** | `tools/RemoteTriggerTool/` | Remote agent trigger management |

### Team
| Tool | Source | Notes |
|------|--------|-------|
| **TeamCreate/Delete** | `tools/TeamCreateTool/`, `TeamDeleteTool/` | Feature-gated: AGENT_SWARMS |

---

## Tool Execution Orchestration

### Partitioning (`toolOrchestration.ts`)

```
All tool_use blocks from assistant response
  → partitionToolCalls()
    → Group 1: [Read, Grep, Glob] — isConcurrencySafe=true → RUN CONCURRENTLY (max 10)
    → Group 2: [Edit] — isConcurrencySafe=false → RUN SERIALLY
    → Group 3: [Bash, Read] — mixed → split and repeat
```

**Concurrency limit**: 10 by default, configurable via `CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`

### Streaming Tool Executor (`StreamingToolExecutor.ts`)

Starts executing tools **before the full API response is complete**:
1. `addTool(toolBlock, assistantMessage)` — queued as tool_use blocks stream in
2. `processQueue()` — starts tools when concurrency slots available
3. `getCompletedResults()` — yields finished tools immediately (called during streaming)
4. `getRemainingResults()` — awaits in-progress tools (called after streaming ends)

### Per-Tool Execution Flow (`toolExecution.ts`)
1. Parse input via Zod schema (`tool.inputSchema.safeParse()`)
2. Tool-specific validation (`tool.validateInput()`)
3. Backfill observable fields for UI (`tool.backfillObservableInput()`)
4. Pre-tool hooks (`runPreToolUseHooks()`) — can modify input, grant/deny permission
5. Permission check (`canUseTool()`) — can result in allow/deny/ask
6. **Execute tool** (`tool.call()`)
7. Post-tool hooks (`runPostToolUseHooks()`) — process results
8. Map result (`tool.mapToolResultToToolResultBlockParam()`)

---

## Permission System

### Permission Modes
| Mode | Behavior |
|------|----------|
| `default` | Ask user for each tool use |
| `acceptEdits` | Auto-allow file edits, ask for others |
| `bypassPermissions` | Allow all tools |
| `plan` | Read-only tools only |
| `auto` | ML classifier decides (YOLO mode) |

### Permission Rules (from settings)
- `alwaysAllow`: Patterns that auto-approve (e.g., `"Bash(git *)"`)
- `alwaysDeny`: Patterns that auto-reject
- `alwaysAsk`: Patterns that always prompt user

### Classifiers
- **Bash classifier** (`utils/permissions/bashClassifier.ts`): Analyzes shell commands for safety
- **YOLO classifier** (`utils/permissions/yoloClassifier.ts`): Auto-mode quick decision

---

## Tool Availability by Mode

| Mode | Available Tools |
|------|----------------|
| **Simple** (`CLAUDE_CODE_SIMPLE`) | Bash, Read, Edit only |
| **REPL** (`isReplModeEnabled`) | REPL wrapper (hides primitives), agents, tasks |
| **Coordinator** | Agent, TaskStop, SendMessage, SyntheticOutput |
| **Async agent** | Most tools except Agent (no recursion), AskUserQuestion |

---

## Tool Registration (`tools.ts`)

```typescript
getAllBaseTools()      // Returns complete exhaustive tool list
getTools(context)     // Filtered by permissions + mode
assembleToolPool()    // Combines built-in + MCP tools with deduplication
filterToolsByDenyRules()  // Permission-based filtering
```

---

## Key Source Files

| File | What to reference |
|------|-------------------|
| `Tool.ts` | Tool interface, ToolUseContext type, findToolByName() |
| `tools.ts` | getAllBaseTools(), getTools(), assembleToolPool() |
| `tools/utils.ts` | Shared tool utilities |
| `services/tools/toolOrchestration.ts` | partitionToolCalls(), runTools(), concurrent execution |
| `services/tools/StreamingToolExecutor.ts` | Streaming execution class |
| `services/tools/toolExecution.ts` | Single tool invocation with hooks |
| `utils/permissions/permissions.ts` | Main permission enforcement |
| `utils/permissions/bashClassifier.ts` | Bash command safety analysis |
| `constants/tools.ts` | Tool name constants, availability rules |
| `hooks/useCanUseTool.ts` | Permission checking hook |

---

## Rust Translation Notes

- **Tool trait**: `#[async_trait] trait Tool { async fn call(&self, input: Value, ctx: &ToolContext) -> ToolResult; ... }`
- **Tool registry**: `Vec<Box<dyn Tool>>` or enum dispatch for compile-time known tools
- **Zod → serde**: Input validation via `serde_json::from_value::<ToolInput>(input)?`
- **Concurrent execution**: `tokio::spawn` per tool + `FuturesUnordered` to collect results
- **Serial execution**: Sequential `.await` calls
- **Permission system**: Enum-based modes with pattern matching on tool name + input
- **Streaming executor**: `tokio::sync::mpsc::channel` for tool result streaming

---

## Phase 3 calibration notes (validated 2026-04-17 during `elena-tools` implementation)

1. **Trait slimming**: The reference `Tool.ts` interface has ~11 methods
   including `userFacingName`, `renderToolResultMessage`, `prompt`, and
   other UX concerns. Elena's `Tool` trait is 6 methods (plus one
   defaulted permission check) — the UX methods are rendering concerns
   the backend doesn't own.

2. **Max concurrent default**: Matched the TS default of 10
   (`CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`), exposed via
   `DefaultsConfig.max_concurrent_tools`. Operators can raise or lower
   per deployment.

3. **Result ordering guarantee**: `execute_batch` returns results in the
   original invocation order regardless of schedule (concurrent or
   serial). Tests pin this — the reorder bug is the one way to
   accidentally desynchronize `tool_result` blocks from their `tool_use`
   counterparts.

4. **Logical vs infrastructure failures**: Tools return
   `Ok(ToolOutput { is_error: true })` for logical failures (shell
   exited non-zero, 404, validation fail) and `Err(ToolError)` only for
   infra failures (timeout, cancel, registry miss). The loop commits
   `is_error` tool-result messages and keeps going; infra errors wrap
   into a tool-result with `is_error: true` so the model still sees a
   block for the call it made.

5. **`PluginCall` as a stub**: Registered so the plugin wire contract is
   visible in LLM tool schemas starting Phase 3, but always errors until
   `elena-plugins` (Phase 6) wires the gRPC bridge.
