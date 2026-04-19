//! Branded identifier newtypes.
//!
//! Every ID in Elena is a distinct type wrapping a [`ulid::Ulid`]. Branding
//! prevents accidentally passing a `UserId` where a `TenantId` is expected —
//! a class of bug that compiles cleanly in TypeScript but is caught at the
//! type level here.
//!
//! IDs serialize as their Base32 ULID string (26 chars). In Postgres we store
//! them in native `uuid` columns (ULIDs are valid 128-bit UUID values), so
//! [`Self::as_uuid`] is the conversion point.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Error produced when parsing a malformed ID string.
#[derive(Debug, thiserror::Error)]
#[error("invalid {kind} id: {source}")]
pub struct IdParseError {
    /// Which branded type failed to parse (e.g., `"TenantId"`).
    pub kind: &'static str,
    /// Underlying ULID decode error.
    #[source]
    pub source: ulid::DecodeError,
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $tag:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Ulid);

        impl $name {
            /// Generate a fresh, time-sortable ID.
            #[must_use]
            pub fn new() -> Self {
                Self(Ulid::new())
            }

            /// Wrap an existing [`Ulid`] without generating a new one.
            #[must_use]
            pub const fn from_ulid(ulid: Ulid) -> Self {
                Self(ulid)
            }

            /// Extract the underlying [`Ulid`].
            #[must_use]
            pub const fn as_ulid(&self) -> Ulid {
                self.0
            }

            /// Convert to a [`uuid::Uuid`] for database persistence.
            ///
            /// ULIDs and UUIDs share the same 128-bit layout, so this is a
            /// lossless conversion.
            #[must_use]
            pub fn as_uuid(&self) -> uuid::Uuid {
                uuid::Uuid::from(self.0)
            }

            /// Reconstruct from a [`uuid::Uuid`] (assumes the bytes came from
            /// a ULID originally — any 128-bit value is accepted).
            #[must_use]
            pub fn from_uuid(uuid: uuid::Uuid) -> Self {
                Self(Ulid::from(uuid))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Tag the debug output so cross-tenant bugs in logs are easy
                // to spot: `TenantId(01J...)` vs `UserId(01J...)`.
                write!(f, "{}({})", $tag, self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ulid::from_str(s)
                    .map(Self)
                    .map_err(|source| IdParseError { kind: $tag, source })
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Ulid> for $name {
            fn from(ulid: Ulid) -> Self {
                Self(ulid)
            }
        }

        impl From<$name> for Ulid {
            fn from(id: $name) -> Ulid {
                id.0
            }
        }

        impl From<$name> for uuid::Uuid {
            fn from(id: $name) -> uuid::Uuid {
                id.as_uuid()
            }
        }
    };
}

define_id! {
    /// Identifies a tenant (an application) connected to Elena.
    ///
    /// Elena is application-agnostic — apps are identified only by this ID
    /// and optional metadata. Every store query is scoped by `TenantId`.
    TenantId, "TenantId"
}

define_id! {
    /// Identifies an end user within a tenant's application.
    UserId, "UserId"
}

define_id! {
    /// Identifies a workspace — a logical grouping of threads that share
    /// settings, files, and episodic memory.
    WorkspaceId, "WorkspaceId"
}

define_id! {
    /// Identifies a conversation thread (a single chat/generation job).
    ThreadId, "ThreadId"
}

define_id! {
    /// Identifies a single message within a thread.
    MessageId, "MessageId"
}

define_id! {
    /// Identifies a single tool call (request/response pair).
    ToolCallId, "ToolCallId"
}

define_id! {
    /// Identifies a gateway session (WebSocket/HTTP connection).
    SessionId, "SessionId"
}

define_id! {
    /// Identifies an episodic-memory record — one stored task summary scoped
    /// to a workspace.
    EpisodeId, "EpisodeId"
}

define_id! {
    /// Identifies a single transport-level work request flowing from the
    /// gateway to a worker. Phase 5 uses this as the NATS deduplication
    /// key when the same request is redelivered after a worker crash.
    RequestId, "RequestId"
}

/// Identifies a plugin connector by its user-supplied namespace.
///
/// Unlike the other IDs, this is a [`String`] — plugin IDs are author-chosen
/// (`"shopify"`, `"gmail"`) and must be stable across deployments so their
/// tool names hash the same way.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginId(pub String);

impl PluginId {
    /// Wrap an owned [`String`] as a plugin ID.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PluginId({})", self.0)
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for PluginId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for PluginId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique() {
        let a = TenantId::new();
        let b = TenantId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn display_and_from_str_roundtrip() {
        let id = ThreadId::new();
        let s = id.to_string();
        let parsed: ThreadId = s.parse().expect("valid ULID");
        assert_eq!(id, parsed);
    }

    #[test]
    fn debug_includes_tag() {
        let id = MessageId::new();
        let rendered = format!("{id:?}");
        assert!(rendered.starts_with("MessageId("), "got: {rendered}");
    }

    #[test]
    fn distinct_types_do_not_confuse() {
        // This compiles because both are distinct types. The point of this
        // test is documentary — if we ever accidentally unify branded IDs,
        // this fn stops compiling.
        fn takes_tenant(_: TenantId) {}
        let tenant = TenantId::new();
        takes_tenant(tenant);
        // takes_tenant(UserId::new()); // <- would fail to compile
    }

    #[test]
    fn serde_roundtrip_emits_bare_string() {
        let id = ThreadId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        // Transparent serde: should be a plain JSON string, not an object.
        assert!(json.starts_with('"') && json.ends_with('"'));
        let back: ThreadId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn uuid_roundtrip_is_lossless() {
        let id = UserId::new();
        let as_uuid = id.as_uuid();
        let back = UserId::from_uuid(as_uuid);
        assert_eq!(id, back);
    }

    #[test]
    fn parse_error_carries_type_tag() {
        let err = TenantId::from_str("not-a-ulid").expect_err("should fail");
        assert_eq!(err.kind, "TenantId");
    }

    #[test]
    fn plugin_id_debug_tagged() {
        let id = PluginId::new("shopify");
        assert_eq!(format!("{id:?}"), "PluginId(shopify)");
        assert_eq!(id.to_string(), "shopify");
    }

    #[test]
    fn plugin_id_serde_transparent() {
        let id = PluginId::new("gmail");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"gmail\"");
    }
}
