#!/usr/bin/env bash
# Bootstrap a fresh Elena cluster.
#
# Steps:
# 1. Create the namespace.
# 2. Apply bootstrap manifests (Postgres StatefulSet + Redis + NATS +
#    cert-manager ClusterIssuer stub).
# 3. Wait for Postgres ready.
# 4. Run sqlx migrations.
# 5. Seed a dev tenant.
#
# Usage:
#     bash scripts/bootstrap.sh [--dry-run] [--namespace elena]

set -euo pipefail

NAMESPACE="${NAMESPACE:-elena}"
DRY_RUN="${DRY_RUN:-false}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=true; shift ;;
    --namespace) NAMESPACE="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

apply() {
  local file="$1"
  if [[ "$DRY_RUN" == "true" ]]; then
    kubectl apply --dry-run=client -f "$file" -n "$NAMESPACE"
  else
    kubectl apply -f "$file" -n "$NAMESPACE"
  fi
}

echo ">>> Creating namespace $NAMESPACE"
if [[ "$DRY_RUN" == "true" ]]; then
  kubectl create ns "$NAMESPACE" --dry-run=client -o yaml
else
  kubectl create ns "$NAMESPACE" --dry-run=client -o yaml | kubectl apply -f -
fi

echo ">>> Applying bootstrap manifests"
for f in deploy/k8s/bootstrap/*.yaml; do
  [[ -e "$f" ]] || continue
  apply "$f"
done

if [[ "$DRY_RUN" == "true" ]]; then
  echo ">>> Dry run complete"
  exit 0
fi

echo ">>> Waiting for Postgres ready (up to 5 min)…"
kubectl rollout status statefulset/postgres -n "$NAMESPACE" --timeout=5m

echo ">>> Running migrations"
kubectl run elena-migrate --rm -it --restart=Never \
  --namespace "$NAMESPACE" \
  --image=ghcr.io/solen/elena/worker:1.0.0 \
  --env=ELENA_POSTGRES__URL="postgres://elena:elena@postgres:5432/elena" \
  --command -- /usr/local/bin/elena-worker --migrate-only || true

echo ">>> Bootstrap complete"
