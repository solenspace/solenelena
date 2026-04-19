//! [`PluginId`] — a plugin's self-declared identifier.
//!
//! Plugin IDs are human-readable ASCII strings chosen by the connector
//! author (for example `"echo"`, `"shopify"`). They become the prefix for
//! every synthetic tool this plugin exposes (`{plugin_id}_{action}`), so
//! they must match Anthropic's tool-name regex and be unique within an
//! Elena worker.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// A plugin's self-declared identifier.
///
/// Must match `[a-z0-9][a-z0-9_-]{0,31}` — lowercase ASCII, up to 32 bytes,
/// starting with a letter or digit.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginId(String);

impl PluginId {
    /// Validate and wrap a raw string.
    pub fn new(raw: impl Into<String>) -> Result<Self, PluginIdError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(PluginIdError::Empty);
        }
        if raw.len() > 32 {
            return Err(PluginIdError::TooLong { len: raw.len() });
        }
        let mut chars = raw.chars();
        // Non-empty was checked above; this must succeed.
        let Some(first) = chars.next() else {
            return Err(PluginIdError::Empty);
        };
        if !first.is_ascii_alphanumeric() {
            return Err(PluginIdError::BadFirstChar { got: first });
        }
        for c in chars {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
                return Err(PluginIdError::BadChar { got: c });
            }
        }
        if first.is_ascii_uppercase() {
            return Err(PluginIdError::BadFirstChar { got: first });
        }
        Ok(Self(raw))
    }

    /// Access the raw string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the raw string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PluginId({})", self.0)
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PluginId {
    type Err = PluginIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Error produced by [`PluginId::new`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PluginIdError {
    /// The input was empty.
    #[error("plugin id must not be empty")]
    Empty,
    /// The input exceeded 32 bytes.
    #[error("plugin id longer than 32 bytes ({len})")]
    TooLong {
        /// Length of the offending input.
        len: usize,
    },
    /// First character must be an ASCII letter or digit (and lowercase).
    #[error("plugin id must start with a lowercase ASCII letter or digit, got {got:?}")]
    BadFirstChar {
        /// Offending character.
        got: char,
    },
    /// Other characters must be lowercase, digit, `_`, or `-`.
    #[error("plugin id contains illegal character {got:?}")]
    BadChar {
        /// Offending character.
        got: char,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_ids() {
        assert!(PluginId::new("echo").is_ok());
        assert!(PluginId::new("shopify").is_ok());
        assert!(PluginId::new("gmail_v2").is_ok());
        assert!(PluginId::new("plugin-01").is_ok());
        assert!(PluginId::new("x").is_ok());
        assert!(PluginId::new("0plugin").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(PluginId::new(""), Err(PluginIdError::Empty));
    }

    #[test]
    fn rejects_too_long() {
        let thirty_three = "a".repeat(33);
        assert!(matches!(PluginId::new(thirty_three), Err(PluginIdError::TooLong { len: 33 })));
    }

    #[test]
    fn rejects_uppercase() {
        assert!(matches!(PluginId::new("Echo"), Err(PluginIdError::BadFirstChar { .. })));
        assert!(matches!(PluginId::new("echO"), Err(PluginIdError::BadChar { .. })));
    }

    #[test]
    fn rejects_leading_symbol() {
        assert!(matches!(PluginId::new("-echo"), Err(PluginIdError::BadFirstChar { got: '-' })));
        assert!(matches!(PluginId::new("_echo"), Err(PluginIdError::BadFirstChar { got: '_' })));
    }

    #[test]
    fn rejects_spaces_and_symbols() {
        assert!(matches!(PluginId::new("ec ho"), Err(PluginIdError::BadChar { got: ' ' })));
        assert!(matches!(PluginId::new("ec.ho"), Err(PluginIdError::BadChar { got: '.' })));
    }

    #[test]
    fn roundtrips_through_serde() {
        let id = PluginId::new("echo").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"echo\"");
        let back: PluginId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
