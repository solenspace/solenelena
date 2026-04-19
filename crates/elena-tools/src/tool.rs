//! The [`Tool`] trait + its surrounding I/O types.
//!
//! Tools are the model's external-effect surface. Each one declares a name,
//! a JSON input schema, concurrency hints, and an async [`execute`](Tool::execute)
//! entry point. Logical failures (a shell command exiting non-zero, a
//! web-fetch 404) travel inside a successful `Ok(ToolOutput)` with
//! `is_error: true`; `Err(ToolError)` is reserved for infrastructure faults
//! (cancellation, timeout, invalid input) that the loop needs to see.

use async_trait::async_trait;
use elena_types::{Permission, PermissionSet, ToolCallId, ToolError, ToolResultContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::ToolContext;

/// One pending tool call extracted from an assistant turn.
///
/// Serializable so it can be parked in the [`LoopState`](elena_core) and
/// resumed from a Redis checkpoint after a worker crash without re-invoking
/// the LLM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInvocation {
    /// The model-supplied tool-call ID.
    pub id: ToolCallId,
    /// The tool name (matched against [`ToolRegistry`](crate::ToolRegistry)).
    pub name: String,
    /// The tool input as parsed JSON.
    pub input: Value,
}

/// The successful result of a tool execution.
///
/// `content` is forwarded verbatim into the next `tool_result` block sent to
/// the model. `is_error` tags a logical failure — the model sees it and can
/// recover (re-try, ask for clarification, give up).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// The body to attach to the `tool_result` block.
    pub content: ToolResultContent,
    /// True if the tool call failed logically (not an infra error).
    pub is_error: bool,
}

impl ToolOutput {
    /// Build a successful text result.
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self { content: ToolResultContent::text(s), is_error: false }
    }

    /// Build a failing text result — the model receives it as an error.
    #[must_use]
    pub fn error(s: impl Into<String>) -> Self {
        Self { content: ToolResultContent::text(s), is_error: true }
    }
}

/// A tool usable by the agentic loop.
///
/// Implementers return metadata (name, schema, description) synchronously
/// and run their effect in [`execute`](Self::execute). Concurrency hints
/// control whether the loop can run multiple calls of this tool in parallel
/// within a batch; see [`crate::execute_batch`] for the exact rules.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// The tool's identifier, as it appears in model tool-use blocks.
    fn name(&self) -> &str;

    /// Natural-language description of what the tool does, for the model.
    /// Required — the LLM needs it to decide when to call the tool.
    fn description(&self) -> &str;

    /// JSON Schema describing the accepted input shape.
    fn input_schema(&self) -> &Value;

    /// True if this call does not mutate any external state.
    ///
    /// Default: `false` (safe assumption for custom tools).
    #[must_use]
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    /// True if multiple calls of this tool can run in parallel within the
    /// same batch. Defaults to [`is_read_only`](Self::is_read_only).
    #[must_use]
    fn is_concurrent_safe(&self, input: &Value) -> bool {
        self.is_read_only(input)
    }

    /// Consult the tenant's [`PermissionSet`] and decide whether the call
    /// proceeds. Returns [`Permission::Allow`] by default.
    ///
    /// Phase 3 treats [`Permission::Ask`] as allow (no prompt channel yet);
    /// [`Permission::Deny`] surfaces as [`ToolError::Execution`] during
    /// orchestration.
    async fn check_permission(&self, _input: &Value, _permissions: &PermissionSet) -> Permission {
        Permission::allow()
    }

    /// Run the tool.
    ///
    /// `input` is the raw JSON the model emitted. `ctx` carries the tenant,
    /// thread, per-call cancellation token, and a progress channel the tool
    /// can use to stream [`StreamEvent::ToolProgress`](elena_types::StreamEvent)
    /// events while it works.
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError>;
}
