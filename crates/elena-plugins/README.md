# `elena-plugins`

Plugin protocol + in-process bridge for Elena. A plugin is an out-of-process
gRPC sidecar (K8s pod pattern). `elena-plugins` dials each endpoint at
boot, fetches its manifest via `GetManifest`, validates the advertised
actions, and **synthesises one `elena_tools::Tool` per action** — registered
straight into the shared `ToolRegistry` so the orchestrator and LLM see
them as ordinary tools.

## Wire protocol (`elena.plugin.v1`)

```protobuf
service ElenaPlugin {
    rpc GetManifest(Empty) returns (PluginManifest);
    rpc Execute(PluginRequest) returns (stream PluginResponse);
    rpc Health(Empty) returns (HealthResponse);
}
```

Key points:

- `Execute` is server-streaming. A response stream is zero or more
  `ProgressUpdate`s followed by exactly one `FinalResult`.
- `ProgressUpdate`s surface to clients as
  [`StreamEvent::ToolProgress`](../../crates/elena-types/src/stream.rs) —
  both `message` (human-readable) and `data_json` (structured) are
  forwarded verbatim.
- `FinalResult.output_json` is returned as a single
  [`ToolResultContent::Text`](../../crates/elena-types/src/content.rs)
  block. If `is_error` is true the tool result is tagged as a logical
  failure (the model sees it and can recover).

## Boot

```rust
use std::sync::Arc;
use elena_plugins::{PluginRegistry, PluginsConfig};
use elena_tools::ToolRegistry;

let tools = ToolRegistry::new();
let cfg = PluginsConfig {
    endpoints: vec!["http://echo-sidecar:50061".into()],
    ..PluginsConfig::default()
};
let plugins = Arc::new(PluginRegistry::with_config(tools.clone(), &cfg));
plugins.register_all(&cfg).await?;
// `tools` now contains one synthesised tool per advertised action;
// the LLM sees them alongside every built-in tool.
```

Pass `plugins: Arc<PluginRegistry>` into
[`elena_core::LoopDeps`](../../crates/elena-core/src/deps.rs) along with
the same `tools: ToolRegistry` — the worker owns the registry for its
lifetime so the background `HealthMonitor` can be shut down cleanly on
`CancellationToken::cancel()`.

## Tool-name synthesis

Each advertised action becomes a tool named `{plugin_id}_{action_name}`
(lowercase, dash → underscore so Anthropic's `^[a-zA-Z0-9_-]{1,64}$`
regex is satisfied). Two plugins exposing actions that synthesise to the
same name return `PluginError::NameCollision` at registration time.

## Read-only opt-in

Plugin actions default to `is_read_only = false`. Connectors that want
Elena to run the action in the concurrent-read-only batch flip the flag
in the proto (`ActionDefinition.is_read_only`). Conservative-by-default
avoids accidental parallel writes from misconfigured connectors.

## Health

A single background `tokio::time::interval` (30 s default) probes every
plugin's `Health` RPC and flips per-plugin `AtomicU8` gauges (
`Unknown=0 / Up=1 / Down=2`). Execute-time `tonic::Code::Unavailable`
also flips the gauge to `Down` lazily. Reading the gauge is lock-free:

```rust
plugin_registry.health(&plugin_id); // HealthState::{Unknown, Up, Down}
```

## Reference connector

[`bins/elena-connector-echo`](../../bins/elena-connector-echo) exposes a
single action `reverse(word: string)` that returns the input reversed
after emitting two `ProgressUpdate`s. Useful both as an illustration and
as the test sidecar for `cargo test -p elena-plugins --test end_to_end`
and `cargo run -p elena-phase6-smoke`.

## Tests

```sh
cargo test -p elena-plugins              # 28 unit tests
cargo test -p elena-plugins --test end_to_end  # bridge + echo connector round-trip
```

End-to-end round-trip (gateway + worker + NATS + Postgres + Redis + echo
connector) lives in
[`bins/elena-phase6-smoke`](../../bins/elena-phase6-smoke).

## Phase-7 backlog

- mTLS between worker and sidecar.
- Per-tenant credential injection / OAuth token encryption at rest.
- Per-tenant rate-limits on plugin calls.
- Plugin-level OpenTelemetry metrics / traces.
- Hot manifest reload without re-register.
- A `/v1/plugins` admin endpoint on the gateway.
