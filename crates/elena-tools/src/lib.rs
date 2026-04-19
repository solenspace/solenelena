//! Tool trait, registry, and orchestration for Elena's agentic loop.
//!
//! Every tool implements [`Tool`] (async-trait). A [`ToolRegistry`] owns the
//! set of registered tools and produces the [`elena_llm::ToolSchema`] array
//! consumed by the LLM client. [`execute_batch`] runs a batch of
//! [`ToolInvocation`]s with concurrent-vs-serial partitioning — read-only
//! tools run in parallel under a semaphore, write tools run serially,
//! preserving invocation order in the returned results.
//!
//! Phase-6 plugin connectors register their actions directly into
//! [`ToolRegistry`] via [`elena_plugins::PluginRegistry`], so plugin
//! invocations reach the model as ordinary [`Tool`]s with no special case
//! in the orchestrator.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod context;
pub mod orchestrate;
pub mod registry;
pub mod tool;

pub use context::ToolContext;
pub use orchestrate::{ExecuteBatchOptions, PerCallCredentials, execute_batch};
pub use registry::ToolRegistry;
pub use tool::{Tool, ToolInvocation, ToolOutput};
