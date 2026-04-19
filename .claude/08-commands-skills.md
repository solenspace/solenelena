# Commands & Skills System

> **Source files**:
> - `commands.ts` — Command registry (~300 lines)
> - `commands/` — 150+ command implementations (26,428 lines)
> - `skills/loadSkillsDir.ts` — Core skill loading (1,070+ lines)
> - `skills/bundledSkills.ts` — Bundled skill registry
> - `skills/bundled/` — 15 bundled skill implementations
> - `types/command.ts` — Command type definitions

---

## Command System

### Command Types

| Type | What it does | Return type |
|------|-------------|-------------|
| `prompt` | Sends content to LLM as context | `ContentBlockParam[]` |
| `local` | Executes locally, returns text | `{ type: 'text', value } | { type: 'compact' } | { type: 'skip' }` |
| `local-jsx` | Renders React component in TUI | `React.ReactNode` |

### Command Interface (`types/command.ts`)

```typescript
CommandBase = {
  name: string
  description: string
  aliases?: string[]
  isEnabled?: () => boolean          // Runtime feature gates
  isHidden?: boolean                 // Hide from help
  argumentHint?: string              // Gray text after command name
  whenToUse?: string                 // For skill model invocation
  version?: string
  userInvocable?: boolean            // User can type /command
  immediate?: boolean                // Execute without waiting for stop point
  isSensitive?: boolean              // Redact args from history
  loadedFrom?: 'skills' | 'plugin' | 'managed' | 'bundled' | 'mcp'
}

PromptCommand = CommandBase & {
  type: 'prompt'
  progressMessage: string
  contentLength: number
  argNames?: string[]
  allowedTools?: string[]
  model?: string
  source: 'builtin' | 'plugin' | 'bundled' | 'mcp'
  hooks?: HooksSettings
  skillRoot?: string
  context?: 'inline' | 'fork'       // inline = inject in conversation, fork = subagent
  agent?: string
  effort?: EffortValue
  paths?: string[]                   // Glob patterns for conditional activation
  getPromptForCommand(args, context): Promise<ContentBlockParam[]>
}
```

### Command Registry (`commands.ts`)

```typescript
COMMANDS()                    // Memoized: returns all enabled commands
builtInCommandNames           // Memoized: Set of built-in names
findCommand(name)             // Search by name/alias
getCommand(name)              // Lazy-load implementation
INTERNAL_ONLY_COMMANDS        // ANT-only commands
```

### Slash Command Execution Flow

```
User types: "/compact"
  → parseSlashCommand() → { name: "compact", args: "" }
  → findCommand("compact") → Command object
  → isEnabled() check
  → Type dispatch:
    prompt → getPromptForCommand() → inject into conversation (or fork subagent)
    local → load() → call(args, context) → text result
    local-jsx → load() → call(onDone, context, args) → React component
```

---

## Skills System

### Skill Sources (priority order)

| Source | Location | Notes |
|--------|----------|-------|
| Managed | `.claude/skills` (via org policy) | Controlled by enterprise |
| User | `~/.claude/skills` | Per-user customization |
| Project | `.claude/skills` (in project tree) | Project-specific |
| Additional | `--add-dir` paths | Explicit directories |
| MCP | MCP servers | Via mcpSkillBuilders bridge |
| Bundled | Compiled in binary | 15 built-in skills |

### Skill File Format (`skills/skill-name/SKILL.md`)

```markdown
---
name: Custom Skill Name
description: What the skill does
when-to-use: When the model should invoke this skill
arguments: $FIRST, $SECOND
allowed-tools: Bash, Read, Edit
model: sonnet | opus | haiku
user-invocable: true
disable-model-invocation: false
context: inline | fork
agent: agent-name
paths: src/**, tests/**
effort: low | medium | high
hooks:
  Stop:
    command: bash script
    prompt: additional context
---

# Skill Content

Instructions for the model when this skill is invoked...
```

### Bundled Skills (15)

| Skill | What it does |
|-------|-------------|
| `update-config` | Configure settings.json |
| `keybindings` | Customize keyboard shortcuts |
| `verify` | Validate code/content |
| `debug` | Debug tasks |
| `lorem-ipsum` | Generate placeholder text |
| `skillify` | Convert markdown into disk-based skills |
| `remember` | Persist memory across sessions |
| `simplify` | Review code for reuse/quality |
| `batch` | Run CLI commands in batch |
| `stuck` | Get unstuck from problems |
| `loop` | Run prompt on recurring interval |
| `schedule-remote-agents` | Manage remote agent triggers |
| `claude-api` | Build apps with Claude API |
| `claude-in-chrome` | Chrome integration |
| `dream` | Memory consolidation |

### Conditional Skill Activation

Skills with `paths` frontmatter are initially dormant:

```
conditionalSkills Map (waiting for path match)
  → File access triggers activateConditionalSkillsForPaths(filePaths, cwd)
  → If glob pattern matches → move to dynamicSkills Map (active)
  → Skill now appears in system prompt
```

### Dynamic Skills Registry

```typescript
const dynamicSkills = new Map<string, Command>()          // Active
const conditionalSkills = new Map<string, Command>()      // Waiting for path match
const activatedConditionalSkillNames = new Set<string>()  // Activation history

getDynamicSkills(): Command[]
activateConditionalSkillsForPaths(filePaths, cwd): string[]
addSkillDirectories(dirs): Promise<void>
```

---

## Command Categories

| Category | Examples |
|----------|---------|
| File management | `/add-dir`, `/copy`, `/files`, `/delete`, `/export`, `/import` |
| Session management | `/session`, `/resume`, `/clear`, `/compact`, `/rewind`, `/snapshot` |
| Git operations | `/branch`, `/commit`, `/diff`, `/merge` |
| Configuration | `/config`, `/settings`, `/model`, `/theme` |
| Skills & agents | `/skills`, `/agents` |
| Integration | `/mcp`, `/plugin`, `/install-github-app` |
| Advanced | `/plan`, `/fast`, `/auto-mode`, `/permissions` |
| Auth | `/login`, `/logout` |
| Info | `/help`, `/status`, `/version`, `/doctor`, `/cost` |

---

## Key Source Files

| File | What to reference |
|------|-------------------|
| `commands.ts` | COMMANDS(), findCommand(), getCommand() |
| `commands/` | Individual command directories |
| `types/command.ts` | PromptCommand, LocalCommand, LocalJSXCommand interfaces |
| `skills/loadSkillsDir.ts` | Skill discovery, frontmatter parsing, conditional activation |
| `skills/bundledSkills.ts` | registerBundledSkill(), file extraction |
| `skills/bundled/index.ts` | Bundled skill registration |
| `skills/mcpSkillBuilders.ts` | MCP → skill bridge (cycle-breaking) |
| `utils/processUserInput/processSlashCommand.tsx` | Slash command parsing and execution |

---

## Rust Translation Notes

- **Command registry** → `Vec<Box<dyn Command>>` with name lookup `HashMap`
- **Lazy loading** → Not needed in Rust (compiled, no module loading overhead)
- **Skill loading** → Read SKILL.md files, parse YAML frontmatter with `serde_yaml`
- **Conditional activation** → `globset` crate for path matching
- **Bundled skills** → Compile-time `include_str!()` for skill content
- **Slash command parsing** → Simple string split on first space
