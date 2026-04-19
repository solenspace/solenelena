//! [`TraceMeta`] — W3C trace-context carrier.
//!
//! Serializable over NATS (`WorkRequest.trace`) and gRPC (plugin
//! `RequestContext.trace_parent` / `trace_state`) so one logical turn
//! produces one distributed trace regardless of process boundaries.
//!
//! Format follows the W3C Trace Context spec:
//! - `traceparent`: `00-<32-hex-trace-id>-<16-hex-span-id>-<2-hex-flags>`
//! - `tracestate`: comma-separated `key=value` pairs; opaque to Elena

use serde::{Deserialize, Serialize};

/// Propagated W3C trace context. Cheap (two short strings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceMeta {
    /// `traceparent` header value.
    pub traceparent: String,
    /// `tracestate` header value (may be empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tracestate: String,
}

impl TraceMeta {
    /// Construct from explicit header values.
    #[must_use]
    pub fn new(traceparent: impl Into<String>, tracestate: impl Into<String>) -> Self {
        Self { traceparent: traceparent.into(), tracestate: tracestate.into() }
    }

    /// Read from an iterator of `(name, value)` header pairs (case-insensitive
    /// match on name). Returns `None` if no `traceparent` header is present.
    pub fn from_headers<'a, I>(headers: I) -> Option<Self>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut parent: Option<String> = None;
        let mut state = String::new();
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("traceparent") {
                parent = Some(value.to_owned());
            } else if name.eq_ignore_ascii_case("tracestate") {
                state = value.to_owned();
            }
        }
        parent.map(|p| Self::new(p, state))
    }

    /// Minimal structural sanity check — *not* a full W3C validator.
    /// Valid examples:
    /// `00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01`.
    #[must_use]
    pub fn looks_valid(&self) -> bool {
        let parts: Vec<&str> = self.traceparent.split('-').collect();
        parts.len() == 4
            && parts[0].len() == 2
            && parts[1].len() == 32
            && parts[2].len() == 16
            && parts[3].len() == 2
            && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    #[test]
    fn roundtrips_through_serde() {
        let m = TraceMeta::new(VALID, "vendor=foo");
        let j = serde_json::to_string(&m).unwrap();
        let back: TraceMeta = serde_json::from_str(&j).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn omits_empty_tracestate_from_json() {
        let m = TraceMeta::new(VALID, "");
        let j = serde_json::to_string(&m).unwrap();
        assert!(!j.contains("tracestate"));
    }

    #[test]
    fn extracts_from_header_iterator() {
        let headers = vec![("x-other", "ignored"), ("TraceParent", VALID), ("tracestate", "a=1")];
        let m = TraceMeta::from_headers(headers.iter().map(|(k, v)| (*k, *v))).unwrap();
        assert_eq!(m.traceparent, VALID);
        assert_eq!(m.tracestate, "a=1");
    }

    #[test]
    fn returns_none_without_traceparent() {
        let headers = vec![("x-other", "ignored")];
        let m = TraceMeta::from_headers(headers.iter().map(|(k, v)| (*k, *v)));
        assert!(m.is_none());
    }

    #[test]
    fn looks_valid_catches_structural_errors() {
        assert!(TraceMeta::new(VALID, "").looks_valid());
        assert!(!TraceMeta::new("bad", "").looks_valid());
        assert!(!TraceMeta::new("00-zzz-xxx-yy", "").looks_valid());
        // Too few hex chars in span id.
        assert!(!TraceMeta::new("00-0af7651916cd43dd8448eb211c80319c-short-01", "").looks_valid());
    }
}
