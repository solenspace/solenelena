# `elena-tools`

Tool trait, registry, and batch orchestration for Elena's agentic loop.

## What's here

- **`Tool` trait** (`tool.rs`): the async interface every tool implements —
  `name`, `description`, `input_schema`, concurrency hints, permission
  check, and `execute`.
- **`ToolContext`** (`context.rs`): what a tool sees at call time — tenant,
  thread id, per-call `CancellationToken`, and a progress channel.
- **`ToolRegistry`** (`registry.rs`): thread-safe name → `Arc<dyn Tool>`
  map with cheap clone semantics. Produces the Anthropic-shaped tool-schema
  array via `schemas()` for `elena_llm::LlmRequest`.
- **`execute_batch`** (`orchestrate.rs`): the scheduler. Greedy batches of
  consecutive concurrent-safe tools run in parallel under a semaphore;
  non-concurrent tools flush the batch and run serially. Default cap: 10
  (matches the reference TS source's `CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`).
  Results are returned in invocation order regardless of schedule.
- **`PluginCall`** (`plugin_call.rs`): Phase-6 placeholder so models can
  include a plugin-call tool in schemas today. Always errors on execute
  until `elena-plugins` ships.

## Non-goals (Phase 3)

- No real built-in tools (FileRead / WebFetch / etc.) — feature crates that
  need them will add their own `impl Tool`.
- No timeout enforcement — `ToolError::Timeout` exists in `elena-types` but
  isn't wired.
- No subagent / `SubAgent` tool.

## Example

```rust
use async_trait::async_trait;
use elena_tools::{Tool, ToolContext, ToolOutput};
use elena_types::{ToolError, ToolResultContent};
use serde_json::{Value, json};

struct Echo { description: String, schema: Value }

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { &self.description }
    fn input_schema(&self) -> &Value { &self.schema }
    fn is_read_only(&self, _: &Value) -> bool { true }
    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: ToolResultContent::text(input["text"].as_str().unwrap_or("")),
            is_error: false,
        })
    }
}
```

## Tests

```sh
cargo test -p elena-tools --lib
```

12 unit tests: registry ops, `execute_batch` partition rules, parallel
concurrency window, result ordering, `PluginCall` stub behavior.
