//! Server-Sent Events frame extractor.
//!
//! Incrementally drains a byte stream into complete SSE frames. Written
//! directly against [`bytes::BytesMut`] so partial chunks don't force a heap
//! reallocation, and so we can avoid the cancellation quirks of
//! `eventsource-stream`.
//!
//! A "frame" here is a complete event as defined by the spec
//! (<https://html.spec.whatwg.org/multipage/server-sent-events.html>):
//!
//! - `event:` sets the frame's event name (optional).
//! - `data:` lines concatenate with `\n` to form the frame body.
//! - `id:` sets the last event ID (optional).
//! - `retry:` suggested reconnect delay (ignored — not the client's concern).
//! - Lines starting with `:` are comments (skipped).
//! - An empty line terminates a frame.
//!
//! Anthropic's streams use LF endings, but we handle CR, LF, and CRLF.

use bytes::BytesMut;

/// One fully-parsed SSE frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseFrame {
    /// The `event:` name, if any.
    pub event: Option<String>,
    /// Concatenated `data:` payload (multi-line `data:` joined with `\n`).
    pub data: String,
    /// The `id:` field, if any.
    pub id: Option<String>,
}

impl SseFrame {
    /// True if the frame carries no useful information (no data, no event name).
    ///
    /// Comment-only frames and keep-alive `\n\n` sequences will produce these;
    /// callers can simply filter them out.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty() && self.event.is_none() && self.id.is_none()
    }
}

/// Incremental SSE frame extractor.
///
/// Push bytes via [`Self::push`]; any complete frames in the buffer are
/// returned. The rest stays buffered until the next call.
#[derive(Debug, Default)]
pub struct SseExtractor {
    buf: BytesMut,
    pending: SseFrame,
}

impl SseExtractor {
    /// Create a new extractor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a chunk of bytes, returning every frame that completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);

        let mut frames = Vec::new();
        while let Some(line) = self.take_line() {
            if line.is_empty() {
                // Empty line terminates a frame.
                let done = std::mem::take(&mut self.pending);
                if !done.is_empty() {
                    frames.push(done);
                }
                continue;
            }
            if line.starts_with(':') {
                // Comment; skip.
                continue;
            }
            self.apply_line(&line);
        }
        frames
    }

    /// Drain any frame that has been started but not yet terminated by an
    /// empty line.
    ///
    /// Useful for testing or for signaling end-of-stream cleanly when the
    /// upstream closed without a trailing blank line. Any bytes still in the
    /// buffer are treated as a final, un-terminated line and applied to the
    /// pending frame before it's returned.
    #[must_use]
    pub fn finalize(mut self) -> Option<SseFrame> {
        if !self.buf.is_empty() {
            let remainder = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            if !remainder.starts_with(':') {
                self.apply_line(&remainder);
            }
        }
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() { None } else { Some(pending) }
    }

    /// Peel off one line from `self.buf`. Returns `None` if no full line is
    /// yet available.
    ///
    /// Handles `\n`, `\r\n`, and lone `\r` line endings. Defers consuming a
    /// trailing lone `\r` until we see the byte that follows, so a `\r\n`
    /// that splits across two `push()` calls is still one terminator.
    fn take_line(&mut self) -> Option<String> {
        let bytes = &self.buf[..];
        let pos = bytes.iter().position(|&b| b == b'\n' || b == b'\r')?;

        // If the terminator is a lone `\r` at the end of the buffer, wait —
        // the next byte (not yet arrived) decides whether this is a `\r\n`
        // or a lone `\r`.
        if bytes[pos] == b'\r' && pos + 1 == bytes.len() {
            return None;
        }

        let line = String::from_utf8_lossy(&bytes[..pos]).into_owned();
        let drop =
            if bytes[pos] == b'\r' && bytes.get(pos + 1).copied() == Some(b'\n') { 2 } else { 1 };
        let _ = self.buf.split_to(pos + drop);

        Some(line)
    }

    fn apply_line(&mut self, raw: &str) {
        // Field name is everything up to the first `:`; the value is the rest,
        // with a single leading space stripped (per spec).
        let (field, value) = match raw.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // Field name with no value — per spec, treat as `field: ""`.
            None => (raw, ""),
        };

        match field {
            "event" => {
                self.pending.event = Some(value.to_owned());
            }
            "data" => {
                if self.pending.data.is_empty() {
                    self.pending.data.push_str(value);
                } else {
                    self.pending.data.push('\n');
                    self.pending.data.push_str(value);
                }
            }
            "id" if !value.contains('\0') => {
                // Spec: ignore if the value contains a NUL. In practice
                // Anthropic never sends NULs here.
                self.pending.id = Some(value.to_owned());
            }
            // `retry` is a client-reconnect hint; unknown fields are silently
            // ignored (per spec). Both drop to the same no-op branch.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_frame_lf_endings() {
        let mut x = SseExtractor::new();
        let frames = x.push(b"event: message_start\ndata: {\"hello\":1}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("message_start"));
        assert_eq!(frames[0].data, r#"{"hello":1}"#);
    }

    #[test]
    fn crlf_line_endings_work() {
        let mut x = SseExtractor::new();
        let frames = x.push(b"event: ping\r\ndata: {}\r\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("ping"));
        assert_eq!(frames[0].data, "{}");
    }

    #[test]
    fn cr_only_line_endings_work() {
        // SSE spec allows lone CR as a line terminator. The trailing `\r` is
        // deferred (we can't tell if the next byte is `\n` yet); a second
        // push of any non-LF byte — or finalize() — unsticks it.
        let mut x = SseExtractor::new();
        let part1 = x.push(b"event: ping\rdata: a\r\r");
        assert!(part1.is_empty(), "last \\r is deferred pending next byte");
        let part2 = x.push(b"event: done\r\r");
        // The deferred `\r` completes as a lone-CR terminator; the blank line
        // after `a` terminates the frame.
        assert_eq!(part2.len(), 1);
        assert_eq!(part2[0].event.as_deref(), Some("ping"));
        assert_eq!(part2[0].data, "a");
    }

    #[test]
    fn multi_line_data_joined_with_newline() {
        let mut x = SseExtractor::new();
        let frames = x.push(b"data: line1\ndata: line2\ndata: line3\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn comment_lines_are_ignored() {
        let mut x = SseExtractor::new();
        let frames = x.push(b": keep-alive\nevent: ping\ndata: {}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("ping"));
    }

    #[test]
    fn partial_chunks_reassemble() {
        let mut x = SseExtractor::new();
        let f1 = x.push(b"event: message_start\nda");
        let f2 = x.push(b"ta: {\"input\":5}\n");
        let f3 = x.push(b"\n");
        assert!(f1.is_empty());
        assert!(f2.is_empty());
        assert_eq!(f3.len(), 1);
        assert_eq!(f3[0].event.as_deref(), Some("message_start"));
        assert_eq!(f3[0].data, r#"{"input":5}"#);
    }

    #[test]
    fn partial_chunk_splitting_a_line_terminator() {
        let mut x = SseExtractor::new();
        // CRLF split across two pushes — '\r' arrives, then '\n' alone.
        let a = x.push(b"event: ping\r");
        let b = x.push(b"\ndata: {}\r\n\r\n");
        // The CR-only line breaker ends the line; the lone LF at the start of
        // the next push is processed as the next line terminator (an empty
        // line, but since no data has accumulated the frame is still empty-
        // valid — or rather, the blank line between lines is tolerated).
        // Either way: we should see exactly one complete frame total.
        let total: Vec<_> = a.into_iter().chain(b).collect();
        assert_eq!(total.len(), 1, "got: {total:?}");
        assert_eq!(total[0].event.as_deref(), Some("ping"));
        assert_eq!(total[0].data, "{}");
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        let mut x = SseExtractor::new();
        let frames = x.push(b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\nevent: c\ndata: 3\n\n");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(frames[1].event.as_deref(), Some("b"));
        assert_eq!(frames[2].event.as_deref(), Some("c"));
    }

    #[test]
    fn field_without_value_dropped_per_spec() {
        // Per WHATWG SSE: a bare `data` with no value appends "" to the data
        // buffer; at dispatch the empty data buffer causes the frame to be
        // skipped entirely.
        let mut x = SseExtractor::new();
        let frames = x.push(b"data\n\n");
        assert!(frames.is_empty(), "bare field with no value should drop frame");
    }

    #[test]
    fn leading_single_space_stripped() {
        let mut x = SseExtractor::new();
        // Spec says exactly one leading space is trimmed, not more.
        let frames = x.push(b"data:  hi\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, " hi"); // one leading space preserved
    }

    #[test]
    fn id_field_roundtrip() {
        let mut x = SseExtractor::new();
        let frames = x.push(b"id: 42\nevent: ping\ndata: {}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id.as_deref(), Some("42"));
    }

    #[test]
    fn finalize_returns_pending_frame() {
        let mut x = SseExtractor::new();
        let inline = x.push(b"event: ping\ndata: {}");
        assert!(inline.is_empty(), "no terminator yet");
        let tail = x.finalize().expect("pending frame");
        assert_eq!(tail.event.as_deref(), Some("ping"));
        assert_eq!(tail.data, "{}");
    }

    #[test]
    fn keep_alive_blank_line_yields_no_frame() {
        let mut x = SseExtractor::new();
        let frames = x.push(b"\n\n");
        assert!(frames.is_empty());
    }

    #[test]
    fn comment_only_frame_is_dropped() {
        let mut x = SseExtractor::new();
        let frames = x.push(b": ka\n\n");
        assert!(frames.is_empty(), "comment-only frame should vanish");
    }

    #[test]
    fn unknown_field_ignored() {
        let mut x = SseExtractor::new();
        let frames = x.push(b"retry: 1000\ndata: hi\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hi");
    }

    #[test]
    fn utf8_payload_preserved() {
        let mut x = SseExtractor::new();
        let frames = x.push("data: héllo 🎉\n\n".as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "héllo 🎉");
    }
}
