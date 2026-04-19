//! [`ToolRegistry`] — name → `Arc<dyn Tool>` lookup and schema projection.
//!
//! Keyed by tool name (not type) so dynamically-registered plugin tools can
//! coexist with compile-time built-ins. Thread-safe (`DashMap` inside an
//! `Arc`) and cheap to clone.

use std::sync::Arc;

use dashmap::DashMap;
use elena_llm::ToolSchema;
use serde_json::json;

use crate::tool::Tool;

/// Concurrent name → tool map.
///
/// Registrations are last-write-wins — re-registering a tool replaces the
/// previous implementation. The registry is cheap to clone; inner state is
/// shared behind an [`Arc`].
#[derive(Clone, Default)]
pub struct ToolRegistry {
    inner: Arc<DashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool by value. Replaces any previous registration with the
    /// same [`Tool::name`].
    pub fn register<T: Tool>(&self, tool: T) {
        let arc: Arc<dyn Tool> = Arc::new(tool);
        self.register_arc(arc);
    }

    /// Register an already-arc'd tool. Useful when several registries share
    /// one trait object (e.g., sub-agent harnesses).
    pub fn register_arc(&self, tool: Arc<dyn Tool>) {
        self.inner.insert(tool.name().to_owned(), tool);
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.inner.get(name).map(|entry| Arc::clone(entry.value()))
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True if no tools are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Produce the Anthropic-shaped tool-schema array for
    /// [`elena_llm::LlmRequest::tools`].
    ///
    /// Order is not guaranteed (`DashMap` iteration order). Callers that need
    /// stable ordering should sort by `name` themselves — Elena's prompt-
    /// cache hashing wraps the tool array in a stable-hash context already,
    /// so the order difference doesn't bust the cache.
    #[must_use]
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.inner
            .iter()
            .map(|entry| {
                let tool = entry.value();
                ToolSchema::new(json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "input_schema": tool.input_schema(),
                }))
            })
            .collect()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.inner.iter().map(|entry| entry.key().clone()).collect();
        f.debug_struct("ToolRegistry").field("tools", &names).finish()
    }
}

#[cfg(test)]
#[allow(clippy::unnecessary_literal_bound)]
mod tests {
    use async_trait::async_trait;
    use elena_types::ToolError;
    use serde_json::Value;

    use super::*;
    use crate::{context::ToolContext, tool::ToolOutput};

    struct Noop {
        name: &'static str,
        schema: Value,
    }

    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "no-op"
        }
        fn input_schema(&self) -> &Value {
            &self.schema
        }
        async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn noop(name: &'static str) -> Noop {
        Noop { name, schema: json!({"type": "object"}) }
    }

    #[test]
    fn register_and_lookup() {
        let reg = ToolRegistry::new();
        reg.register(noop("a"));
        reg.register(noop("b"));
        assert_eq!(reg.len(), 2);
        assert!(reg.get("a").is_some());
        assert!(reg.get("b").is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn re_register_replaces() {
        let reg = ToolRegistry::new();
        reg.register(noop("a"));
        reg.register(noop("a"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn schemas_contain_every_tool() {
        let reg = ToolRegistry::new();
        reg.register(noop("first"));
        reg.register(noop("second"));
        let schemas = reg.schemas();
        assert_eq!(schemas.len(), 2);
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|s| s.as_value().get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"first"));
        assert!(names.contains(&"second"));
    }

    #[test]
    fn cloned_registry_shares_state() {
        let reg = ToolRegistry::new();
        let twin = reg.clone();
        reg.register(noop("shared"));
        assert!(twin.get("shared").is_some());
    }

    #[test]
    fn empty_registry_has_no_schemas() {
        let reg = ToolRegistry::new();
        assert!(reg.is_empty());
        assert!(reg.schemas().is_empty());
    }
}
