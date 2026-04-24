//! Q7 — Provider-specific wire-body builders.
//!
//! Pre-Q7 the file was named `wire.rs` but only built Anthropic
//! Messages API bodies. The `OpenAI`-compat path's body builder lives
//! in [`crate::openai_compat`] (private to that module). The
//! restructure makes the naming honest: `wire::anthropic` is
//! Anthropic's body shape; `OpenAI`'s lives next to its caller.
//!
//! Anthropic-only fields (`thinking`, `cache_control`) appear *only*
//! in the Anthropic builder. The `OpenAI` request-body builder ignores
//! these on the request side; the SSE-deserializer side translates
//! `OpenAI`'s `reasoning` field into a Thinking event for symmetry but
//! never emits cache markers.

pub mod anthropic;

pub use anthropic::{beta_headers, build_wire_body};
