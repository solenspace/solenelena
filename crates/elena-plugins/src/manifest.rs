//! Native Rust mirror of [`pb::PluginManifest`](crate::proto::pb::PluginManifest).
//!
//! The proto-generated structs are fine for the wire, but we want native
//! types in Elena — `serde::{Serialize, Deserialize}`, validated `PluginId`,
//! schemas pre-parsed into `serde_json::Value` so the registry doesn't
//! re-parse on every tool execution.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{error::PluginError, id::PluginId, proto::pb};

/// A plugin's identity + action catalog, validated and ready for use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginManifest {
    /// Stable identifier (e.g. `"echo"`).
    pub id: PluginId,
    /// Human-readable name.
    pub name: String,
    /// Plugin version string (free-form; semver recommended).
    pub version: String,
    /// Actions advertised by this plugin.
    pub actions: Vec<ActionDefinition>,
    /// Credential names the operator should provision (informational only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_credentials: Vec<String>,
}

/// One callable action exposed by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionDefinition {
    /// Action name (e.g. `"reverse"`).
    pub name: String,
    /// LLM-facing description.
    pub description: String,
    /// JSON-Schema for the input payload.
    pub input_schema: Value,
    /// JSON-Schema for the output payload.
    pub output_schema: Value,
    /// True if this action is safe to run in the concurrent batch.
    pub is_read_only: bool,
}

impl PluginManifest {
    /// Validate and convert a wire-format manifest. Returns
    /// [`PluginError::Manifest`] / [`PluginError::Schema`] / [`PluginError::Id`]
    /// on the first problem found.
    pub fn from_wire(wire: pb::PluginManifest) -> Result<Self, PluginError> {
        let id = PluginId::new(wire.id.clone())?;

        if wire.name.trim().is_empty() {
            return Err(PluginError::Manifest {
                plugin_id: wire.id.clone(),
                reason: "name is empty".into(),
            });
        }
        if wire.version.trim().is_empty() {
            return Err(PluginError::Manifest {
                plugin_id: wire.id.clone(),
                reason: "version is empty".into(),
            });
        }

        if wire.actions.is_empty() {
            return Err(PluginError::Manifest {
                plugin_id: wire.id.clone(),
                reason: "no actions advertised".into(),
            });
        }

        let mut seen = std::collections::BTreeSet::new();
        let mut actions = Vec::with_capacity(wire.actions.len());
        for a in wire.actions {
            if a.name.trim().is_empty() {
                return Err(PluginError::Manifest {
                    plugin_id: wire.id.clone(),
                    reason: "action with empty name".into(),
                });
            }
            if !seen.insert(a.name.clone()) {
                return Err(PluginError::Manifest {
                    plugin_id: wire.id.clone(),
                    reason: format!("duplicate action name {:?}", a.name),
                });
            }
            let input_schema: Value = parse_schema(&a.input_schema, &id, &a.name)?;
            let output_schema: Value = parse_schema(&a.output_schema, &id, &a.name)?;
            if !input_schema.is_object() {
                return Err(PluginError::Schema {
                    plugin_id: id.clone(),
                    action: a.name.clone(),
                    reason: "input_schema is not a JSON object".into(),
                });
            }
            actions.push(ActionDefinition {
                name: a.name,
                description: a.description,
                input_schema,
                output_schema,
                is_read_only: a.is_read_only,
            });
        }

        Ok(Self {
            id,
            name: wire.name,
            version: wire.version,
            actions,
            required_credentials: wire.required_credentials,
        })
    }

    /// Encode back into the wire form (mostly for connector binaries that
    /// want to construct one without hand-writing the proto type).
    #[must_use]
    pub fn to_wire(&self) -> pb::PluginManifest {
        pb::PluginManifest {
            id: self.id.as_str().to_owned(),
            name: self.name.clone(),
            version: self.version.clone(),
            actions: self
                .actions
                .iter()
                .map(|a| pb::ActionDefinition {
                    name: a.name.clone(),
                    description: a.description.clone(),
                    input_schema: serde_json::to_string(&a.input_schema).unwrap_or_default(),
                    output_schema: serde_json::to_string(&a.output_schema).unwrap_or_default(),
                    is_read_only: a.is_read_only,
                })
                .collect(),
            required_credentials: self.required_credentials.clone(),
        }
    }

    /// Tool name Elena synthesises for `action`: `{plugin_id}_{action}`.
    #[must_use]
    pub fn tool_name_for(&self, action: &str) -> String {
        format!("{}_{}", self.id, action)
    }
}

fn parse_schema(raw: &str, id: &PluginId, action: &str) -> Result<Value, PluginError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({"type": "object"}));
    }
    serde_json::from_str(trimmed).map_err(|e| PluginError::Schema {
        plugin_id: id.clone(),
        action: action.to_owned(),
        reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_action(name: &str) -> pb::ActionDefinition {
        pb::ActionDefinition {
            name: name.into(),
            description: format!("does {name}"),
            input_schema: r#"{"type":"object","properties":{"x":{"type":"string"}}}"#.into(),
            output_schema: r#"{"type":"object"}"#.into(),
            is_read_only: false,
        }
    }

    fn good_manifest() -> pb::PluginManifest {
        pb::PluginManifest {
            id: "echo".into(),
            name: "Echo".into(),
            version: "0.1.0".into(),
            actions: vec![good_action("reverse")],
            required_credentials: vec![],
        }
    }

    #[test]
    fn happy_path_roundtrip() {
        let wire = good_manifest();
        let native = PluginManifest::from_wire(wire.clone()).unwrap();
        let back = native.to_wire();
        // input/output_schema serialize compactly; compare structurally.
        let parsed_in: Value = serde_json::from_str(&back.actions[0].input_schema).unwrap();
        let parsed_in_orig: Value = serde_json::from_str(&wire.actions[0].input_schema).unwrap();
        assert_eq!(parsed_in, parsed_in_orig);
        assert_eq!(back.id, wire.id);
        assert_eq!(back.name, wire.name);
        assert_eq!(back.actions.len(), 1);
    }

    #[test]
    fn rejects_bad_id() {
        let mut wire = good_manifest();
        wire.id = "Echo".into();
        let err = PluginManifest::from_wire(wire).unwrap_err();
        assert!(matches!(err, PluginError::Id(_)));
    }

    #[test]
    fn rejects_no_actions() {
        let mut wire = good_manifest();
        wire.actions.clear();
        let err = PluginManifest::from_wire(wire).unwrap_err();
        assert!(matches!(err, PluginError::Manifest { .. }));
    }

    #[test]
    fn rejects_duplicate_actions() {
        let mut wire = good_manifest();
        wire.actions.push(good_action("reverse"));
        let err = PluginManifest::from_wire(wire).unwrap_err();
        assert!(matches!(err, PluginError::Manifest { .. }));
    }

    #[test]
    fn rejects_malformed_schema() {
        let mut wire = good_manifest();
        wire.actions[0].input_schema = "{ not json".into();
        let err = PluginManifest::from_wire(wire).unwrap_err();
        assert!(matches!(err, PluginError::Schema { .. }));
    }

    #[test]
    fn empty_schema_defaults_to_object() {
        let mut wire = good_manifest();
        wire.actions[0].input_schema = String::new();
        let native = PluginManifest::from_wire(wire).unwrap();
        assert_eq!(native.actions[0].input_schema, serde_json::json!({"type":"object"}));
    }

    #[test]
    fn synthesises_tool_name() {
        let wire = good_manifest();
        let native = PluginManifest::from_wire(wire).unwrap();
        assert_eq!(native.tool_name_for("reverse"), "echo_reverse");
    }
}
