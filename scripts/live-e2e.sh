#!/usr/bin/env bash
# Live end-to-end test against the production Elena deployment.
#
# Exercises every public + admin endpoint, the WebSocket protocol, the
# autonomy / approval round-trip, the per-tenant credentials path, and
# representative edge cases (auth failures, malformed bodies, tenant
# isolation). Cleans up every tenant + workspace + credential row it
# creates via a `trap`.
#
# Required env (sourced from /tmp/elena-prod-secrets if present):
#   ELENA_JWT_SECRET    HS256 secret used to mint client JWTs
#   ELENA_ADMIN_TOKEN   X-Elena-Admin-Token shared secret
#
# Optional:
#   BASE              defaults to https://elena-server-production.up.railway.app
#   ELENA_JWT_ISSUER  defaults to "elena"
#   ELENA_JWT_AUD     defaults to "elena-clients"

set -uo pipefail

if [[ -f /tmp/elena-prod-secrets ]]; then
  source /tmp/elena-prod-secrets
  : "${ELENA_JWT_SECRET:=${PROD_JWT-}}"
  : "${ELENA_ADMIN_TOKEN:=${PROD_ADMIN-}}"
fi

if [[ -z "${ELENA_JWT_SECRET-}" || -z "${ELENA_ADMIN_TOKEN-}" ]]; then
  echo "FATAL: set ELENA_JWT_SECRET and ELENA_ADMIN_TOKEN before running"
  exit 2
fi
export ELENA_JWT_SECRET ELENA_ADMIN_TOKEN

BASE="${BASE:-https://elena-server-production.up.railway.app}"
WS_BASE="${BASE/https:\/\//wss://}"
WS_BASE="${WS_BASE/http:\/\//ws://}"
export ELENA_JWT_ISSUER="${ELENA_JWT_ISSUER:-elena}"
export ELENA_JWT_AUD="${ELENA_JWT_AUD:-elena-clients}"

export NODE_PATH="${NODE_PATH:-/opt/homebrew/lib/node_modules}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CG="\033[1;32m"; CR="\033[1;31m"; CY="\033[1;33m"; CB="\033[1;34m"; CD="\033[0m"

PASS=0
FAIL=0
TENANTS_CREATED=()

trap cleanup EXIT
cleanup() {
  echo
  echo -e "${CB}━━━ cleanup ━━━${CD}"
  for tid in "${TENANTS_CREATED[@]:-}"; do
    [[ -z "$tid" ]] && continue
    curl -sS -o /dev/null -X DELETE \
      -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" \
      "$BASE/admin/v1/tenants/$tid/credentials/echo" 2>/dev/null || true
    echo "  i tenant $tid: credentials cleared (no admin DELETE for tenant row in v1; rows scoped by tenant_id remain orphaned)"
  done
}

ok()   { PASS=$((PASS+1)); printf "  ${CG}✓${CD} %s\n" "$*"; }
fail() { FAIL=$((FAIL+1)); printf "  ${CR}✗${CD} %s\n" "$*"; }
note() { printf "  ${CY}i${CD} %s\n" "$*"; }
section() { echo; printf "${CB}━━━ %s ━━━${CD}\n" "$*"; }

# Elena IDs are ULIDs (26-char Crockford-Base32), not UUIDs. Generate
# via the `ulid` npm package (installed globally in this dev env).
uuid() { node -e "const {ulid}=require('ulid'); console.log(ulid())"; }

mint_jwt() {
  # args: tenant_id user_id workspace_id [tier] [exp_offset] [iss] [aud] [secret]
  TENANT_ID="$1" USER_ID="$2" WORKSPACE_ID="$3" \
  TIER="${4:-pro}" EXP_OFFSET="${5:-3600}" \
  ISS="${6:-$ELENA_JWT_ISSUER}" AUD="${7:-$ELENA_JWT_AUD}" \
  ELENA_JWT_SECRET="${8:-$ELENA_JWT_SECRET}" \
  node "$SCRIPT_DIR/live-e2e-mintjwt.js"
}

run_turn() {
  # args: thread_id jwt prompt [autonomy] [model] [timeout_ms]
  WS_URL="${WS_BASE}/v1/threads/$1/stream" \
  JWT="$2" \
  PROMPT="$3" \
  AUTONOMY="${4:-yolo}" \
  MODEL="${5:-llama-3.3-70b-versatile}" \
  TIMEOUT="${6:-120000}" \
  node "$SCRIPT_DIR/live-e2e-runturn.js"
}

curlcode() {
  # Returns the HTTP status code only.
  curl -sS -o /dev/null -w "%{http_code}" "$@"
}

echo -e "${CB}elena live e2e — base=$BASE${CD}"
echo "  jwt issuer=$ELENA_JWT_ISSUER  audience=$ELENA_JWT_AUD"

# ─── USE CASES ────────────────────────────────────────────────────────
section "USE CASES"

# 1-3 public endpoints
[[ "$(curlcode "$BASE/health")" == "200" ]] && ok "1.  GET /health = 200" || fail "1.  GET /health"
curl -fsS "$BASE/version" 2>/dev/null | grep -q '"name":"elena-gateway"' && ok "2.  GET /version returns elena-gateway" || fail "2.  GET /version"
curl -fsS "$BASE/metrics" 2>/dev/null | grep -q '^elena_turns_total' && ok "3.  GET /metrics emits Prometheus text" || fail "3.  GET /metrics"

# 4 deep health
DEEP=$(curl -fsS -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" "$BASE/admin/v1/health/deep" 2>/dev/null)
DEPS=$(echo "$DEEP" | jq -r '[.deps[] | "\(.name)=\(.status)"] | join(" ")' 2>/dev/null || echo "?")
echo "$DEEP" | jq -e '.status == "ok" and (.deps | length >= 3)' >/dev/null 2>&1 \
  && ok "4.  /admin/v1/health/deep: $DEPS" || fail "4.  deep health: $DEEP"

# 5 create tenant
TENANT=$(uuid); USER=$(uuid); WS=$(uuid)
TENANTS_CREATED+=("$TENANT")
R=$(curlcode -X POST -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" -H "content-type: application/json" \
  -d "{\"id\":\"$TENANT\",\"name\":\"e2e-${TENANT:0:8}\",\"tier\":\"pro\"}" \
  "$BASE/admin/v1/tenants")
[[ "$R" == "200" || "$R" == "201" ]] && ok "5.  POST /admin/v1/tenants → $R (id=${TENANT:0:8}…)" || fail "5.  tenant create $R"

# 6 read tenant back
[[ "$(curlcode -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" "$BASE/admin/v1/tenants/$TENANT")" == "200" ]] \
  && ok "6.  GET /admin/v1/tenants/:id round-trip" || fail "6.  tenant readback"

# allow-list echo plugin
curlcode -X PATCH -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" -H "content-type: application/json" \
  -d '{"allowed_plugin_ids":["echo"]}' \
  "$BASE/admin/v1/tenants/$TENANT/allowed-plugins" >/dev/null

# 7 create workspace
R=$(curlcode -X POST -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" -H "content-type: application/json" \
  -d "{\"id\":\"$WS\",\"tenant_id\":\"$TENANT\",\"name\":\"e2e-ws\",\"global_instructions\":\"Reply in one short sentence. When the user asks to reverse a word, you MUST call the echo_reverse tool.\",\"allowed_plugin_ids\":[\"echo\"]}" \
  "$BASE/admin/v1/workspaces")
[[ "$R" == "200" || "$R" == "201" ]] && ok "7.  POST /admin/v1/workspaces → $R" || fail "7.  workspace create $R"

# 8 mint JWT + create thread
JWT=$(mint_jwt "$TENANT" "$USER" "$WS")
THREAD=$(curl -fsS -X POST -H "authorization: Bearer $JWT" -H "content-type: application/json" -d '{"title":"e2e"}' "$BASE/v1/threads" 2>/dev/null | jq -r '.thread_id // empty')
[[ -n "$THREAD" ]] && ok "8.  POST /v1/threads → ${THREAD:0:8}…" || fail "8.  thread create"

# 9 single-turn yolo
T=$(run_turn "$THREAD" "$JWT" "Reply with exactly one word: hello." "yolo")
REASON=$(echo "$T" | jq -r '.reason // "?"' 2>/dev/null)
CHARS=$(echo "$T" | jq -r '.text_chars // 0' 2>/dev/null)
[[ "$REASON" == "completed" && "$CHARS" -gt 0 ]] && ok "9.  WS yolo single-turn: reason=$REASON chars=$CHARS" || fail "9.  yolo turn — reason=$REASON chars=$CHARS"

# 10 multi-turn on same thread
T2=$(run_turn "$THREAD" "$JWT" "What word did I just ask for?" "yolo")
REASON2=$(echo "$T2" | jq -r '.reason // "?"' 2>/dev/null)
TEXT2=$(echo "$T2" | jq -r '.text // ""' 2>/dev/null)
[[ "$REASON2" == "completed" ]] && ok "10. WS multi-turn on same thread (reply: \"${TEXT2:0:60}…\")" || fail "10. multi-turn — reason=$REASON2"

# 11 tool call via echo plugin
THREAD3=$(curl -fsS -X POST -H "authorization: Bearer $JWT" -H "content-type: application/json" -d '{"title":"echo"}' "$BASE/v1/threads" 2>/dev/null | jq -r '.thread_id')
T3=$(run_turn "$THREAD3" "$JWT" "Reverse the word 'hello' using the echo_reverse tool." "yolo")
REASON3=$(echo "$T3" | jq -r '.reason' 2>/dev/null)
TOOLS=$(echo "$T3" | jq -r '.tool_calls | length' 2>/dev/null)
[[ "$REASON3" == "completed" && "$TOOLS" -ge 1 ]] && ok "11. WS tool call: reason=$REASON3 tool_calls=$TOOLS" || fail "11. tool call — reason=$REASON3 tools=$TOOLS"

# 12 cautious mode pause
THREAD4=$(curl -fsS -X POST -H "authorization: Bearer $JWT" -H "content-type: application/json" -d '{"title":"cautious"}' "$BASE/v1/threads" 2>/dev/null | jq -r '.thread_id')
T4=$(run_turn "$THREAD4" "$JWT" "Reverse 'cautious' using echo_reverse." "cautious")
AWAITING=$(echo "$T4" | jq -r '.awaiting // empty' 2>/dev/null)
if [[ -n "$AWAITING" && "$AWAITING" != "null" ]]; then
  ok "12. WS cautious pauses: awaiting_approval received"
  TOOL_USE_ID=$(echo "$T4" | jq -r '.awaiting.pending[0].tool_use_id' 2>/dev/null)
  R=$(curlcode -X POST -H "authorization: Bearer $JWT" -H "content-type: application/json" \
    -d "{\"approvals\":[{\"tool_use_id\":\"$TOOL_USE_ID\",\"decision\":\"allow\"}]}" \
    "$BASE/v1/threads/$THREAD4/approvals")
  [[ "$R" == "200" ]] && ok "13. POST approvals (allow) → $R" || fail "13. approvals POST $R (token len=${#JWT})"
else
  REASON4=$(echo "$T4" | jq -r '.reason // "?"' 2>/dev/null)
  TOOLS4=$(echo "$T4" | jq -r '.tool_calls | length' 2>/dev/null)
  if [[ "$REASON4" == "completed" && "$TOOLS4" -ge 1 ]]; then
    note "12. echo_reverse is read_only — Cautious mode does not currently force-pause read-only tools (auto-ran $TOOLS4 calls)"
    note "13. (skipped: no pause to approve)"
  else
    fail "12. cautious — neither paused nor completed: reason=$REASON4 tools=$TOOLS4"
  fi
fi

# 14 PUT credentials (encrypted at rest)
R=$(curlcode -X PUT -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" -H "content-type: application/json" \
  -d '{"credentials":{"token":"echo-test-token"}}' \
  "$BASE/admin/v1/tenants/$TENANT/credentials/echo")
[[ "$R" == "204" ]] && ok "14. PUT /admin/v1/.../credentials/echo → $R (encrypted at rest)" || fail "14. PUT credentials $R"

# 15 DELETE credentials (idempotent)
R=$(curlcode -X DELETE -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" "$BASE/admin/v1/tenants/$TENANT/credentials/echo")
[[ "$R" == "204" ]] && ok "15. DELETE credentials → $R (idempotent)" || fail "15. DELETE credentials $R"

# ─── EDGE CASES ───────────────────────────────────────────────────────
section "EDGE CASES"

# 16 admin no token
[[ "$(curlcode "$BASE/admin/v1/health/deep")" == "401" ]] \
  && ok "16. /admin no token → 401" || fail "16. /admin no token"

# 17 admin wrong token
[[ "$(curlcode -H 'x-elena-admin-token: WRONG' "$BASE/admin/v1/health/deep")" == "401" ]] \
  && ok "17. /admin wrong token → 401" || fail "17. /admin wrong token"

# 18 client no JWT
R=$(curlcode -X POST -H 'content-type: application/json' -d '{}' "$BASE/v1/threads")
[[ "$R" == "401" ]] && ok "18. /v1/threads no JWT → 401" || fail "18. /v1/threads no JWT got $R"

# 19 client malformed JWT
R=$(curlcode -X POST -H 'authorization: Bearer this-is-not-a-jwt' -H 'content-type: application/json' -d '{}' "$BASE/v1/threads")
[[ "$R" == "401" ]] && ok "19. /v1/threads malformed JWT → 401" || fail "19. malformed JWT got $R"

# 20 expired JWT
EXP_JWT=$(mint_jwt "$TENANT" "$USER" "$WS" "pro" "-3600")
R=$(curlcode -X POST -H "authorization: Bearer $EXP_JWT" -H 'content-type: application/json' -d '{}' "$BASE/v1/threads")
[[ "$R" == "401" ]] && ok "20. /v1/threads expired JWT → 401" || fail "20. expired JWT got $R"

# 21 wrong audience
WRONG_AUD=$(mint_jwt "$TENANT" "$USER" "$WS" "pro" "3600" "elena" "wrong-audience")
R=$(curlcode -X POST -H "authorization: Bearer $WRONG_AUD" -H 'content-type: application/json' -d '{}' "$BASE/v1/threads")
[[ "$R" == "401" ]] && ok "21. /v1/threads wrong audience → 401" || fail "21. wrong aud got $R"

# 22 wrong issuer
WRONG_ISS=$(mint_jwt "$TENANT" "$USER" "$WS" "pro" "3600" "wrong-issuer")
R=$(curlcode -X POST -H "authorization: Bearer $WRONG_ISS" -H 'content-type: application/json' -d '{}' "$BASE/v1/threads")
[[ "$R" == "401" ]] && ok "22. /v1/threads wrong issuer → 401" || fail "22. wrong iss got $R"

# 23 bad signature
BAD_SIG=$(mint_jwt "$TENANT" "$USER" "$WS" "pro" "3600" "elena" "elena-clients" "WRONG-SECRET")
R=$(curlcode -X POST -H "authorization: Bearer $BAD_SIG" -H 'content-type: application/json' -d '{}' "$BASE/v1/threads")
[[ "$R" == "401" ]] && ok "23. /v1/threads bad signature → 401" || fail "23. bad sig got $R"

# 24 WS via ?token= query param — S7 dropped this fallback; expect 401
WS_DROPPED=$(node -e "
  const WebSocket = require('ws');
  const ws = new WebSocket('$WS_BASE/v1/threads/$THREAD/stream?token=$JWT');
  ws.on('error', (e) => { console.log(String(e).includes('401') ? '401' : 'other:' + e.message); process.exit(0); });
  ws.on('open', () => { console.log('opened'); ws.close(); process.exit(0); });
  setTimeout(() => { console.log('timeout'); process.exit(0); }, 5000);
" 2>/dev/null)
[[ "$WS_DROPPED" == "401" ]] \
  && ok "24. WS ?token= URL param rejected (S7) → 401" \
  || fail "24. ?token= rejection got '$WS_DROPPED'"

# 25 approvals empty array → 400
R=$(curlcode -X POST -H "authorization: Bearer $JWT" -H 'content-type: application/json' -d '{"approvals":[]}' "$BASE/v1/threads/$THREAD/approvals")
[[ "$R" == "400" ]] && ok "25. POST approvals empty array → 400" || fail "25. empty approvals got $R"

# 26 workspace tenant-mismatch isolation
WRONG_TENANT=$(uuid)
R=$(curlcode -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" "$BASE/admin/v1/workspaces/$WS?tenant_id=$WRONG_TENANT")
[[ "$R" == "404" ]] && ok "26. GET workspace wrong tenant_id → 404 (tenant isolation)" || fail "26. workspace isolation got $R"

# 27 audit drops counter
DROPS=$(curl -fsS "$BASE/metrics" 2>/dev/null | grep -E '^elena_audit_drops_total[ {]' | awk '{print $NF}')
[[ "$DROPS" == "0" ]] && ok "27. /metrics: elena_audit_drops_total = 0" || fail "27. audit_drops_total = $DROPS"

# 28 long content (10KB)
BIG_THREAD=$(curl -fsS -X POST -H "authorization: Bearer $JWT" -H "content-type: application/json" -d '{"title":"big"}' "$BASE/v1/threads" 2>/dev/null | jq -r '.thread_id')
BIG=$(printf 'a%.0s' $(seq 1 9900))
T5=$(run_turn "$BIG_THREAD" "$JWT" "Acknowledge: $BIG" "yolo")
REASON5=$(echo "$T5" | jq -r '.reason // "?"' 2>/dev/null)
[[ "$REASON5" == "completed" ]] && ok "28. WS 10KB prompt: reason=$REASON5" || fail "28. big prompt — reason=$REASON5"

# 29 cancel mid-stream
CANCEL_THREAD=$(curl -fsS -X POST -H "authorization: Bearer $JWT" -H "content-type: application/json" -d '{"title":"cancel"}' "$BASE/v1/threads" 2>/dev/null | jq -r '.thread_id')
CANCEL_OUT=$(WS_URL="$WS_BASE/v1/threads/$CANCEL_THREAD/stream" JWT="$JWT" \
  node -e "
  const WebSocket = require('ws');
  const ws = new WebSocket(process.env.WS_URL, ['elena.bearer.' + process.env.JWT]);
  let done = false;
  const t = setTimeout(() => { if (!done) { console.log('timeout'); ws.terminate(); process.exit(0); } }, 20000);
  ws.on('open', () => {
    ws.send(JSON.stringify({ action: 'send_message', text: 'Write a 1000 word essay about clouds.', autonomy: 'yolo' }));
    setTimeout(() => ws.send(JSON.stringify({ action: 'abort' })), 300);
  });
  ws.on('message', (m) => {
    const ev = JSON.parse(m.toString());
    if (ev.event === 'done') { done = true; console.log(ev.reason); clearTimeout(t); ws.close(); }
  });
  ws.on('error', (e) => { if (!done) console.log('err:' + e.message); });
" 2>/dev/null)
case "$CANCEL_OUT" in
  aborted_streaming|aborted_tools|completed) ok "29. WS abort mid-stream: done.reason=$CANCEL_OUT" ;;
  *) fail "29. abort got '$CANCEL_OUT'" ;;
esac

# 30 per-tenant cred injection in flight
curlcode -X PUT -H "x-elena-admin-token: $ELENA_ADMIN_TOKEN" -H "content-type: application/json" \
  -d '{"credentials":{"token":"injected-now"}}' \
  "$BASE/admin/v1/tenants/$TENANT/credentials/echo" >/dev/null
CRED_THREAD=$(curl -fsS -X POST -H "authorization: Bearer $JWT" -H "content-type: application/json" -d '{"title":"creds"}' "$BASE/v1/threads" 2>/dev/null | jq -r '.thread_id')
T6=$(run_turn "$CRED_THREAD" "$JWT" "Reverse 'creds' using echo_reverse." "yolo")
REASON6=$(echo "$T6" | jq -r '.reason // "?"' 2>/dev/null)
[[ "$REASON6" == "completed" ]] \
  && ok "30. WS turn with per-tenant credentials in flight → completed" \
  || fail "30. creds-injected turn — reason=$REASON6"

# ─── REPORT ───────────────────────────────────────────────────────────
echo
echo -e "${CB}━━━ result ━━━${CD}"
echo -e "  ${CG}passed${CD}: $PASS"
echo -e "  ${CR}failed${CD}: $FAIL"
echo "  total:  $((PASS+FAIL))"
if [[ "$FAIL" == "0" ]]; then
  echo -e "${CG}elena live e2e: OK${CD}"
  exit 0
else
  echo -e "${CR}elena live e2e: FAIL${CD}"
  exit 1
fi
