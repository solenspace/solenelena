# elena-config

Configuration loader for Elena. Uses [figment](https://docs.rs/figment) with
layered sources:

1. Built-in defaults (via `#[serde(default)]`).
2. `/etc/elena/elena.toml` — optional system-wide config.
3. `$ELENA_CONFIG_FILE` — optional deployment-specific override.
4. `ELENA_*` environment variables (nested via `__`).

All credentials are wrapped in `secrecy::SecretString`. `ElenaConfig` does
not implement `Serialize` — this is intentional, so a logger cannot
accidentally dump secrets.

## Example

```sh
export ELENA_POSTGRES__URL="postgres://u:p@localhost:5432/elena"
export ELENA_REDIS__URL="redis://localhost:6379"
cargo run -p elena-phase1-smoke
```

## Validation

`elena_config::validate(&cfg)` is a pure function — no I/O. It checks:

- `postgres.pool_max >= postgres.pool_min`
- `postgres.statement_timeout_ms <= 300_000`
- `postgres.url` has scheme `postgres` or `postgresql`
- `redis.url` has scheme `redis`, `rediss`, or `redis-cluster`
