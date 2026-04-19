# TUI Framework (Ink)

> **Source directories**:
> - `ink/` — Custom terminal UI framework (19,842 lines)
> - `components/` — React/Ink components (81,546 lines, 144 files)
> - `hooks/` — React hooks for TUI (19,204 lines, 47 files)
> - `screens/` — Main screens (5,977 lines)
> - `vim/` — Vim mode implementation (1,513 lines)
> - `voice/` — Voice input (54 lines gate + hooks)
> - `context/` — React context providers (1,004 lines)

---

## Ink Framework Architecture

Custom React-based terminal UI with 5-stage rendering pipeline:

```
React Components (JSX)
  → React Reconciler (reconciler.ts)
    → Virtual DOM (DOMElement tree, dom.ts)
      → Yoga Layout Engine (flex layout)
        → Output Cell Grid (render-node-to-output.ts)
          → Screen Buffer (screen.ts)
            → ANSI Diff → stdout (log-update.ts)
```

### Key Files

| File | Size | Purpose |
|------|------|---------|
| `ink/dom.ts` | 15.1K | Virtual DOM model, Yoga integration, scroll state |
| `ink/render-node-to-output.ts` | 63.3K | Node → cell grid renderer (borders, padding, overflow, text wrap) |
| `ink/reconciler.ts` | 14.6K | React Reconciler (createNode, appendChild, style diffing) |
| `ink/parse-keypress.ts` | 23.5K | Keyboard parsing (CSI u, xterm modifyOtherKeys, mouse SGR) |
| `ink/App.tsx` | 98.3K | Root component (stdin/stdout, terminal mode, selection, suspend) |
| `ink/screen.ts` | 1.5K | Screen buffer with character/hyperlink pooling |
| `ink/output.ts` | 26.2K | Output cell grid with ANSI code diffing |
| `ink/selection.ts` | 917 | Text selection state machine (char/word/line modes) |
| `ink/log-update.ts` | 27.2K | Incremental stdout writing with cursor management |

### Performance Optimizations
- **Character pool interning**: `CharPool` interns strings (space=0, empty=1, ASCII fast-path)
- **Hyperlink pooling**: `HyperlinkPool` for OSC 8 hyperlinks
- **Blit regions**: Reuse unchanged regions from previous frame (dirty flag checks)
- **ANSI diffing**: Only emit delta changes between frames
- **Virtual scrolling**: Large content rendered on-demand

### Event System
- Capture/bubble phases (like browser DOM)
- `KeyboardEvent` with key, modifiers, propagation
- `ClickEvent` from mouse tracking
- `FocusEvent` with previous element tracking
- Focus manager with 32-entry focus stack for Tab cycling

---

## Component Hierarchy

### Root
- `App.tsx` → `FpsMetricsProvider` → `StatsProvider` → `AppStateProvider`

### Core Components (`components/`)
| Component | Purpose |
|-----------|---------|
| `ui/` | Base UI primitives |
| `messages/` | Message rendering (user, assistant, tool results) |
| `agents/` | Agent UI (creation wizard, status) |
| `diff/` | File diff visualization |
| `mcp/` | MCP server UI |
| `tasks/` | Task list UI |
| `shell/` | Shell command history UI |
| `memory/` | Memory management UI |
| `Settings/` | Settings dialogs |
| `grove/` | Context tree visualization |
| `wizard/` | Multi-step dialog system |
| `CustomSelect/` | Reusable select component |

### Largest Components
| File | Size | What |
|------|------|------|
| `BridgeDialog.tsx` | 34K | Bridge connection setup/status |
| `ContextVisualization.tsx` | 76K | Context window with file tree |
| `ConsoleOAuthFlow.tsx` | 79K | OAuth authentication flow |
| `CoordinatorAgentStatus.tsx` | 36K | Swarm coordinator display |
| `AutoUpdater.tsx` | 30K | Version update prompts |

---

## Main Screens

| Screen | File | Size | Purpose |
|--------|------|------|---------|
| **REPL** | `screens/REPL.tsx` | 895.8K | Main interaction loop (largest file in codebase!) |
| **Doctor** | `screens/Doctor.tsx` | 73.3K | Diagnostic tool |
| **ResumeConversation** | `screens/ResumeConversation.tsx` | 59.7K | Session resumption |

---

## React Hooks (47 files)

### Major Hooks
| Hook | Size | Purpose |
|------|------|---------|
| `useTypeahead.tsx` | 212K | Autocomplete (commands, files, paths, shell history) |
| `useReplBridge.tsx` | 115K | Bridge connection management |
| `useVoiceIntegration.tsx` | 99K | Voice I/O (recording, transcription) |
| `useVoice.ts` | 45K | Voice state management |
| `useVirtualScroll.ts` | 35K | Virtual scrolling for long lists |
| `useRemoteSession.ts` | 23K | Remote session management |
| `useTextInput.ts` | 17K | Terminal text input with cursor |
| `useSearchInput.ts` | 10K | Search box input |
| `useVimInput.ts` | 9K | Vim keybinding support |

---

## Vim Mode (`vim/`)

Pure functional state machine implementation:

| File | Lines | Purpose |
|------|-------|---------|
| `types.ts` | 200 | VimState (INSERT/NORMAL), CommandState union |
| `operators.ts` | ~400 | delete, change, yank with motions |
| `motions.ts` | 83 | h/j/k/l, w/b/e, W/B/E, 0/^/$, G/gg |
| `textObjects.ts` | 126 | inner/around word, quotes, brackets, parens |
| `transitions.ts` | 338 | processKey() state machine, dot-repeat |

**Key types**:
```typescript
VimState = INSERT { insertedText } | NORMAL { commandState }
CommandState = idle | count | operator(delete|change|yank) | find | g | replace | indent | ...
PersistentState = { lastChange, lastFind, register, registerIsLinewise }
```

---

## Voice Input

- `voice/voiceModeEnabled.ts` — GrowthBook gate + OAuth auth check
- `hooks/useVoice.ts` — Core voice state management
- `hooks/useVoiceIntegration.tsx` — Recording, transcription, audio levels
- Streaming speech-to-text via Anthropic API (requires OAuth, not API key)

---

## Context Providers (`context/`)

| Provider | Purpose |
|----------|---------|
| `VoiceState` | Voice recording/processing state |
| `Notifications` | Notification queue with priority |
| `Modal` | Modal dimensions and scroll |
| `Overlay` | Overlay registration (autocomplete, etc.) |
| `Stats` | Histogram/counter statistics (reservoir sampling) |
| `Mailbox` | Signal-based pub/sub between components |
| `FpsMetrics` | Frame-rate monitoring |
| `PromptOverlay` | Permission prompt overlay |
| `QueuedMessage` | Inter-component message queue |

---

## Rust Translation Notes

**For Elena, the TUI layer is the biggest divergence.** You won't replicate Ink — you'll use native Rust TUI:

| TypeScript (Ink) | Rust Equivalent |
|-----------------|-----------------|
| React + Ink | `ratatui` + `crossterm` |
| Virtual DOM + Reconciler | Direct rendering (no reconciliation) |
| Yoga layout | `ratatui::layout::Layout` (Constraint-based) |
| `useInput` hook | `crossterm::event::read()` polling |
| ANSI diffing | `ratatui` handles this internally |
| Selection state machine | Port directly (pure state machine) |
| Vim mode | Port directly — pure functions translate 1:1 |
| Voice | gRPC/WebSocket to speech API |
| Hooks | Regular Rust state + channels |
| Components | `ratatui::Widget` implementations |

**The key insight**: The Ink framework is ~100K lines of React terminal rendering infrastructure. In Rust with `ratatui`, you get equivalent functionality in ~5K lines because `ratatui` handles the rendering pipeline natively. Focus engineering effort on the agentic loop, not the TUI.
