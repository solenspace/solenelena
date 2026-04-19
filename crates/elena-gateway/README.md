# `elena-gateway`

axum-based HTTP + WebSocket front end for Elena. JWT-authenticated, NATS-
backed, stateless beyond the auth surface.

## Routes

| Method | Path                              | Auth | Notes |
|--------|-----------------------------------|------|-------|
| `GET`  | `/health`                         | no   | Readiness probe — always 200 OK if reachable. |
| `GET`  | `/version`                        | no   | Static `name` + `version` JSON. |
| `POST` | `/v1/threads`                     | yes  | Creates a new thread scoped to the JWT's tenant. Body `{"title": "..."}` (optional). |
| `GET`  | `/v1/threads/:id/stream`          | yes  | WebSocket upgrade for the thread's turn lifecycle. |

## WebSocket protocol

Once upgraded, the gateway sends a one-shot ack:
```json
{"event": "session_started", "thread_id": "..."}
```

Then it relays every [`StreamEvent`](../../crates/elena-types/src/stream.rs)
emitted by the worker for that thread, one per text frame, using the
existing tagged-JSON shape.

Client → server frames:
```json
{"action": "send_message", "text": "hello", "model": "claude-haiku-4-5", "max_turns": 5}
{"action": "abort"}
```

`model` and `max_turns` are optional — the router picks otherwise, and the
worker's defaults apply.

## Auth

JWT-only in Phase 5. mTLS-between-services lands in Phase 7. Tokens may
carry one of the algorithms in [`JwtAlgorithm`](src/config.rs); HS\* uses a
shared secret, RS\* / ES\* uses a PEM-encoded public key.

Required claims:
```json
{
  "tenant_id": "01J...",
  "user_id": "01J...",
  "workspace_id": "01J...",
  "tier": "pro",
  "iss": "elena-dev",
  "aud": "elena-clients",
  "exp": 1761234567
}
```

Permissions + budget are not in the JWT — they're loaded from the store at
session start so operator-driven policy changes take effect without
reissuing tokens.

## NATS

- `elena.work.incoming` (JetStream, work-queue retention) — gateway
  publishes one [`WorkRequest`](../../crates/elena-types/src/transport.rs)
  per `send_message`.
- `elena.thread.{id}.events` (core pub/sub) — gateway subscribes for the
  WebSocket's lifetime, relays each event verbatim.
- `elena.thread.{id}.abort` (core pub/sub) — gateway publishes an empty
  payload on `abort`; workers cancel the in-flight loop.

## Tests

```sh
cargo test -p elena-gateway --lib   # 11 unit tests (JWT + WS frame parsing)
```

End-to-end (gateway + worker + NATS + Postgres + Redis) is exercised by
[`bins/elena-phase5-smoke`](../../bins/elena-phase5-smoke).
