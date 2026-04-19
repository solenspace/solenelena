# elena-llm

Streaming LLM client for Elena. Phase 2 scope: Anthropic Messages API with
API-key auth, SSE streaming, retry with exponential backoff, prompt-cache
marker placement, and end-to-end `CancellationToken` cancellation.

## Crate layout

| Module | Purpose |
|---|---|
| `anthropic` | `AnthropicClient::stream` — the public entry point. Wraps retry, cancellation, and the HTTP request. |
| `assembler` | Stateful per-block accumulator. Turns `AnthropicEvent` into `StreamEvent`. Handles tool-input JSON concatenation and cumulative usage accounting. |
| `cache` | `CachePolicy` latching (TTL eligibility + allowlist), `decide_message_marker`, `decide_system_marker`. |
| `events` | Internal wire enum (`AnthropicEvent`). Consumed by the assembler; not part of the public API. |
| `request` | `LlmRequest`, `RequestOptions`, `Thinking`, `ToolChoice`, `ToolSchema`. |
| `retry` | `RetryPolicy`, `classify_http`, `decide_retry`. Exponential backoff with jitter, 529 cap, `Retry-After` parsing. |
| `sse` | Hand-rolled SSE frame extractor. Spec-compliant on line endings, multi-line `data:`, comments, trailing `\r` across chunks. |
| `wire` | `build_wire_body` — assembles the Anthropic request JSON with cache_control markers and tenant metadata. |

## Public API

```rust
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_config::AnthropicConfig;
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

let cfg = AnthropicConfig { /* ... */ };
let client = AnthropicClient::new(&cfg, AnthropicAuth::ApiKey(cfg.api_key.clone()))?;
let policy = CachePolicy::new(tenant.tier, CacheAllowlist::from(config.cache.clone()));

let mut stream = client.stream(request, policy, CancellationToken::new());
while let Some(event) = stream.next().await { /* ... */ }
```

## Ground truth calibrated against Claude Code

- SSE events accumulate per-block-index; deltas never reset between blocks.
- Tool-use `input_json_delta` chunks are concatenated as raw JSON and parsed only at `content_block_stop`.
- Usage is cumulative on the wire. Input fields use a `>0` guard — an explicit 0 in `message_delta` means "unset", not "zero tokens."
- `temperature` is omitted (not sent as 1.0) when `thinking` is set.
- `metadata.user_id` is required and carries a JSON string of tenant/user/workspace/session IDs.
- Exponential backoff: `base·2^(n-1) + uniform(0, jitter·base·2^(n-1))`, capped at `max_delay_ms`.
- 529 retries are capped at 3 after the initial failure (4 attempts total), then surfaced as `LlmApiError::Repeated529`.
- `Retry-After` header is parsed as integer seconds and overrides the computed delay for that attempt.
- Prompt caching: one message-level `cache_control` marker (on the last text block of the last message) plus one system-level marker (on the final system block). Tools never receive cache markers.
- Beta header `prompt-caching-scope-2026-01-05` is sent whenever a marker with `scope: global` could be emitted.

## Non-goals in Phase 2

- OAuth / Bedrock / Vertex / Azure Foundry auth — Phase 2 is API-key only.
- Model fallback on repeated 529 — lands with `elena-router` in Phase 4.
- Streaming idle watchdog — ships when a real hang motivates it.
- `cache_reference` on `tool_result` blocks — microcompact-specific, Phase 4.
- MCP-aware cache scope downgrade — Phase 6.
- Subscriber-aware 429 gating (Pro/Max retry rules) — ships with billing state.
- Persistent-mode extended retry cap (5min/6h) — REPL-specific.
- Non-streaming fallback on 404 — edge case, deferred.

## Testing

- **Unit tests** (94 of them): SSE parser, retry backoff/classification, cache policy, assembler state machine, wire body builder, auth header injection.
- **Integration tests** (`tests/streaming.rs`, 6 scenarios) run against `wiremock` — no real network: happy path, 529 retries then success, Repeated529 give-up, `Retry-After` honored, cancellation mid-flight, cache markers in outgoing body.
- **Real API smoke** (`bins/elena-phase2-smoke`) exercises the full path against `api.anthropic.com`. Gracefully skips if `ANTHROPIC_API_KEY` is unset.

Run the fast feedback loop:

```sh
cargo test -p elena-llm --lib
cargo test -p elena-llm --test streaming
cargo clippy -p elena-llm --all-targets -- -D warnings
```

Or the real API (optional):

```sh
ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase2-smoke
```
