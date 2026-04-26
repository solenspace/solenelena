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

Production deploys are gated through GitHub Actions — Railway's
built-in GitHub auto-deploy is intentionally turned off so CI is the
only path to production.

```sh
git push origin main
# → .github/workflows/ci.yml runs fmt + clippy + test + docker-build
# → on green, the deploy job runs `railway redeploy`
# → Railway pulls origin/main and rebuilds via Dockerfile.all-in-one
```

Manual redeploy (last resort — e.g. CI is broken and you have the
operator role):

```sh
railway redeploy --service elena-server --environment production -y
```

Rollback:

```sh
# Instant — Railway keeps the prior image hot.
railway rollback --service elena-server

# Or revert the offending commit and push; CI re-runs and deploys
# the prior SHA via the same `railway redeploy` path.
git revert <bad-sha> && git push origin main
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

## One-time pipeline setup (operator)

Required after merging the CI/CD bootstrap PR. These steps are
repo-external and cannot be expressed as code.

1. **Disable Railway's GitHub auto-deploy.**
   Railway dashboard → `elena-server` service → Settings → Source →
   toggle **Auto Deploy from GitHub** to OFF. Keep the GitHub repo
   source connected; `railway redeploy` still relies on it to fetch
   `origin/main`.

2. **Provision `RAILWAY_TOKEN`.**
   Generate an account-scoped token at
   https://railway.com/account/tokens → "Create New Token". Add it as a
   GitHub Environment secret on the `production` environment (Settings
   → Environments → production → Add secret) with the name
   `RAILWAY_TOKEN`. The deploy job pins the target via explicit
   `--project`, `--service`, and `--environment` flags, so the token
   only needs account-level access — no per-project scoping required.

   The current values baked into `ci.yml`:
   - project  `50b379da-82af-42a5-b88f-fc76a2c3dfc2` (elena)
   - service  `07e3776c-6b83-41c6-a158-661f9c3022c2` (elena-server)
   - environment `production`

   If the project / service is recreated, update the IDs in
   `.github/workflows/ci.yml` (deploy job).

3. **(Recommended) Create the `production` GitHub Environment.**
   Settings → Environments → New environment → name `production`. Add
   required reviewers if you want a human gate before deploys land.
   The `deploy` job in `ci.yml` already references this environment, so
   the `RAILWAY_TOKEN` secret can be scoped to it (not exposed to
   pull-request runs).

4. **(Recommended) Branch-protection rules.**
   Settings → Branches → Add rule for `main`:
   - Require status checks to pass: `cargo fmt`, `cargo clippy`,
     `cargo test`, `docker build (Dockerfile.all-in-one)`,
     `conventional-commit lint`.
   - Require pull-request reviews before merging.
   - Require linear history (matches the team's squash-merge
     convention).

5. **Verify.**
   - Open a draft PR introducing a clippy violation → `clippy` job red.
   - Open a PR titled `chore add stuff` → `pr-title` job red.
   - Merge a green PR to `main` → `deploy` job appears in the workflow
     graph and reports `railway redeploy` success;
     `curl https://<railway-domain>/info` returns the new git SHA.
