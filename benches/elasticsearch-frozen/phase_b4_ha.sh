#!/bin/bash
# Phase B4 runner: HA failover smoke — 2 stateless S4 instances behind nginx.
#
# Starts two identical S4 zstd-3 instances + an nginx round-robin upstream in
# front of them, then runs phase_b4_ha_failover.py which registers an ES repo
# through the LB, kills one instance, and checks cold query / warm query /
# snapshot-PUT-during-kill -> results/ha-failover.json.
#
# Override for an isolated stack, e.g.:
#   ES_URL=http://localhost:9305 MINIO_URL=http://localhost:9100 \
#   S4_BIN=../../target/release/s4 LB_PORT=8030 S4_PORTS=8027,8028 ./phase_b4_ha.sh
set -e
cd "$(dirname "$0")"

: "${ES_URL:=http://localhost:9200}"
: "${MINIO_URL:=http://localhost:9100}"
: "${S4_BIN:=../../target/release/s4}"
: "${LB_PORT:=8030}"
: "${S4_PORTS:=8027,8028}"
: "${LB_CLIENT:=hals3}"
: "${BUCKET:=esrev-s4z3}"
: "${INDEX:=bench-standard-default}"
: "${LOGDIR:=/tmp/esrev-logs}"
mkdir -p "$LOGDIR"

IFS=',' read -r P1 P2 <<<"$S4_PORTS"
ME_HOST="${HOST_GATEWAY:-host.docker.internal}"

echo "B4: starting 2 stateless S4 zstd-3 instances on :$P1 :$P2, nginx LB :$LB_PORT"
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1
for p in "$P1" "$P2"; do
  if ! ss -ltn 2>/dev/null | grep -q ":$p "; then
    nohup "$S4_BIN" --endpoint-url="$MINIO_URL" --host=0.0.0.0 --port="$p" \
      --codec=cpu-zstd --dispatcher=always --zstd-level=3 \
      > "$LOGDIR/s4-ha-$p.log" 2>&1 &
    echo "  started S4 :$p (pid $!)"
  fi
done
sleep 2

# nginx round-robin upstream over the two S4 instances, reachable from ES on :$LB_PORT
cat > "$LOGDIR/nginx-ha.conf" <<EOF
events {}
http {
  upstream s4pool {
    server ${ME_HOST}:${P1} max_fails=1 fail_timeout=2s;
    server ${ME_HOST}:${P2} max_fails=1 fail_timeout=2s;
  }
  server {
    listen ${LB_PORT};
    client_max_body_size 0;
    proxy_request_buffering off;
    location / {
      proxy_pass http://s4pool;
      # Preserve the client's Host EXACTLY: AWS SigV4 signs the Host header, so
      # nginx must not rewrite it to the upstream name or signatures break (403).
      proxy_set_header Host \$http_host;
      proxy_http_version 1.1;
      proxy_next_upstream error timeout http_502 http_503 http_504;
      proxy_connect_timeout 2s;
    }
  }
}
EOF
docker rm -f esrev-nginx >/dev/null 2>&1 || true
docker run -d --name esrev-nginx --add-host=host.docker.internal:host-gateway \
  -p "${LB_PORT}:${LB_PORT}" \
  -v "$LOGDIR/nginx-ha.conf:/etc/nginx/nginx.conf:ro" nginx:alpine >/dev/null
sleep 2
echo "  nginx LB up on :$LB_PORT -> ${ME_HOST}:{$P1,$P2}"

ES_URL="$ES_URL" MINIO_URL="$MINIO_URL" LB_CLIENT="$LB_CLIENT" LB_PORT="$LB_PORT" \
  S4_PORTS="$S4_PORTS" BUCKET="$BUCKET" INDEX="$INDEX" python3 phase_b4_ha_failover.py
RC=$?

echo "B4 done (rc=$RC). Tear down nginx + ha S4 instances with the stack cleanup."
exit $RC
