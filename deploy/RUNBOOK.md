# Elena v1.0 — Operator Runbook

## Quick reference

| Scenario | First step | Recovery path |
|---|---|---|
| Mass 429s (rate-limit rejections) | `curl /metrics \| grep elena_rate_limit_rejections_total` | Raise `rateLimits.tenantRpm` in values.yaml; `helm upgrade`. |
| Provider outage | `curl /admin/v1/health/deep \| jq .providers` | Router auto-fails over (circuit breaker). Edit tier routing to pin another provider. |
| DB failover (Postgres primary down) | Check managed-DB console | Workers reconnect automatically; no restart needed. Confirm with `kubectl logs deploy/elena-worker \| grep -i postgres`. |
| Cert expiry (mTLS to plugin) | `kubectl get secret $CERT_SECRET -o jsonpath='{.data.tls\.crt}' \| base64 -d \| openssl x509 -noout -enddate` | Rotate via cert-manager / external-secrets. Workers re-read certs on next connect (no restart). |
| Bad deploy | `helm rollback elena` | Automatic; no data loss. Old pods drain within `terminationGracePeriodSeconds` (default 30s). |
| NATS JetStream corrupt | `nats stream info elena-work` shows inconsistent state | Delete + recreate stream; in-flight messages lost but worker claim prevents duplicate work on recovery. |
| Redis flap | Consumer sees connection errors briefly | Fred auto-reconnects. Claim TTLs (60 s default) ensure stuck claims release. |
| LLM key leak | `curl /admin/v1/plugins/deregister` if plugin-scoped | Rotate via `SecretProvider`. `CachedSecret` refreshes on the next 401. |

## Deployment

```sh
# Build images and push.
docker build -f deploy/docker/Dockerfile.gateway -t ghcr.io/solen/elena/gateway:1.0.0 .
docker build -f deploy/docker/Dockerfile.worker -t ghcr.io/solen/elena/worker:1.0.0 .
docker build -f deploy/docker/Dockerfile.connector-echo -t ghcr.io/solen/elena/connector-echo:1.0.0 .

# Deploy.
helm upgrade --install elena deploy/helm/elena \
  --namespace elena --create-namespace \
  -f values.prod.yaml

# Bootstrap (first time only).
bash scripts/bootstrap.sh
```

## Observability

- **Traces:** Jaeger at `https://jaeger.internal/elena`. Root span starts at gateway's `ws.stream`; every child step carries the trace-parent through NATS.
- **Metrics:** Prometheus scrapes `/metrics` every 15 s. Dashboard: `grafana.internal/d/elena-red`.
- **Logs:** Cluster log pipeline (Loki / CloudWatch / Datadog). Every `error!` carries a `trace_id` attribute — drop into Jaeger to see the full causal path.

## SLOs

- First-token p95: < 1.2 s
- Turn-completion p95: < 15 s
- Availability (gateway 200 OK + `/health` 200 OK): 99.9%
- Error budget burn: alert at 2× burn over 5 min OR 1× over 1 h.
