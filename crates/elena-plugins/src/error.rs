//! Error types for the plugin layer.

use crate::id::PluginId;

/// Top-level error variant for [`PluginRegistry`](crate::PluginRegistry)
/// operations.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// gRPC connection to the plugin endpoint failed.
    #[error("connect to plugin endpoint failed: {0}")]
    Connect(#[from] tonic::transport::Error),
    /// `GetManifest` returned, but the manifest didn't validate.
    #[error("manifest invalid for plugin {plugin_id:?}: {reason}")]
    Manifest {
        /// Plugin advertising the bad manifest.
        plugin_id: String,
        /// Why it's invalid.
        reason: String,
    },
    /// A plugin's `id` couldn't be parsed by [`PluginId::new`].
    #[error("plugin id invalid: {0}")]
    Id(#[from] crate::id::PluginIdError),
    /// JSON-Schema string in an action definition couldn't be parsed.
    #[error("invalid JSON schema for action {action:?} on plugin {plugin_id}: {reason}")]
    Schema {
        /// Owning plugin.
        plugin_id: PluginId,
        /// Action whose schema is malformed.
        action: String,
        /// Why it's invalid.
        reason: String,
    },
    /// Synthesised tool name collides with a tool already in the registry.
    #[error("tool name collision: {tool_name:?} already registered")]
    NameCollision {
        /// The duplicate `{plugin_id}_{action}` name.
        tool_name: String,
    },
    /// Re-registering a plugin with a schema that narrows an existing
    /// action's input type. Refused; the old tool stays live.
    #[error("schema drift on {plugin_id}.{action}: {reason}")]
    SchemaDrift {
        /// Plugin whose re-registration was refused.
        plugin_id: PluginId,
        /// Action whose schema narrowed.
        action: String,
        /// Why the new schema is not a superset of the old.
        reason: String,
    },
}
