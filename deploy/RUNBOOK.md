# Elena — Operator Runbook

Elena is deployed as a single all-in-one process on Railway: gateway +
worker + embedded plugins in one container, fronted by managed
Postgres / Redis / NATS plugins from Railway's catalog.

## Quick reference

| Scenario | First step | Recovery path |
|---|---|---|
| Mass 429s (rate-limit rejections) | `curl /metrics \| grep elena_rate_limit_rejections_total` | Raise the relevant `ELENA_RATE_LIMITS__*` env var in Railway and redeploy. |
| Provider outage (e.g. OpenRouter 5xx) | `curl /admin/v1/health/deep` (deps) and tail `/metrics` for `elena_llm_tokens_total` flatlining | Swap `ELENA_DEFAULTS__TIER_MODELS__*__PROVIDER` env vars to a healthy provider (e.g. `groq`) and redeploy. |
| DB failover (managed Postgres down) | Check Railway / Supabase console | Worker reconnects on the next dispatch; pool is lazy. Confirm with deploy logs. |
| Bad deploy | Railway → service → previous deployment → "Redeploy" | Railway keeps the prior image hot for instant rollback. |
| NATS JetStream corrupt | `nats stream info elena-work` shows inconsistent state | Delete + recreate the stream; in-flight messages lost but the worker's claim CAS prevents duplicate work on recovery. |
| Redis flap | Fred logs reconnect | `fred` auto-reconnects; claim TTLs (60 s default) release stuck claims. |
| Audit drops nonzero | `curl /metrics \| grep elena_audit_drops_total` non-zero | Investigate worker pressure (`elena_loops_in_flight`, Postgres latency); raise `AUDIT_CHANNEL_CAP` only after root-causing the back-pressure. |
| JWT secret leak | Rotate `ELENA_JWT_SECRET` (or `ELENA_GATEWAY__JWT__SECRET_OR_PUBLIC_KEY`) in Railway | All in-flight JWTs invalidated on next request after redeploy. |

## Deployment

```sh
# Push to main → Railway auto-deploys via Dockerfile.all-in-one.
git push origin main

# Manual redeploy (if auto-deploy is disconnected):
railway up --service elena-server --environment production --ci
```

The image is built from `deploy/docker/Dockerfile.all-in-one` per
`railway.json`. Healthcheck: `GET /health` with a 30 s timeout, restart
policy `ON_FAILURE` capped at 10 retries.

## Required env

Per `CLAUDE.md`, the boot path requires:
`DATABASE_URL` / `REDIS_URL` / `NATS_URL` (or the Railway-style
`ELENA_POSTGRES__URL` / `ELENA_REDIS__URL` / `ELENA_NATS_URL`),
`ELENA_ADMIN_TOKEN`, `ELENA_JWT_SECRET` (or `ELENA_GATEWAY__JWT__JWKS_URL`),
`ELENA_CREDENTIAL_MASTER_KEY` (only if any tenant uses per-tenant
credentials), and at least one provider key
(`ELENA_PROVIDERS__{ANTHROPIC,GROQ,OPENROUTER}__API_KEY`).

## Observability

- **Metrics:** `GET /metrics` returns Prometheus text. Alert rules ship in
  `deploy/alerts.yaml`. The canary metric is `elena_audit_drops_total` —
  any non-zero value means the audit log is incomplete.
- **OTel traces:** Set `OTEL_EXPORTER_OTLP_ENDPOINT` to wire the worker's
  `#[instrument]` spans to a collector. The gateway tags every span with
  the thread id; trace IDs are echoed into structured log attributes.
- **Logs:** stdout/stderr captured by Railway. Filter by `level:error` or
  `level:warn` from the Railway dashboard or via `railway logs`.

## SLOs

- First-token p95: < 1.2 s
- Turn-completion p95: < 15 s
- Availability (`/health` 200 OK): 99.9 %
- Error budget burn: alert at 2× over 5 min OR 1× over 1 h.

## Multi-cluster / k8s

Elena currently ships only the Railway single-process deploy. The earlier
Helm chart and per-service Dockerfiles were removed; if you migrate off
Railway, plan to split `elena-server` back into per-process bins
(`gateway`, `worker`) or wrap the all-in-one image with separate
deployments that pin different `ELENA_WORKER__*` envs.
