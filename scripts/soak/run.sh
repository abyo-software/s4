#!/usr/bin/env bash
# S4 long-running soak test harness.
#
# 24h+ 持続負荷で memory leak / FD leak / connection pool 枯渇等を検出する。
# Marketplace AMI 出品前の最終 production validation 用。
#
# Topology: aws-cli (multi-process load) → S4 server (target process) → MinIO
#
# Usage:
#   ./scripts/soak/run.sh                       # default 24h, concurrency 16
#   DURATION=3600 CONCURRENCY=32 ./scripts/soak/run.sh
#   S4_ENDPOINT=http://localhost:8014 BUCKET=soak-test ./scripts/soak/run.sh
#
# Output:
#   /tmp/s4-soak/{date}/
#     ├── monitor.csv     # 1 分ごとの S4 process RSS (KiB) / FD count / open conn
#     ├── load.log        # PUT/GET 結果ログ
#     └── summary.txt     # 最終サマリ (leak verdict)
#
# Verdict: 実行終了時の RSS が初期 RSS の 2x 未満ならば "no leak detected"。

set -euo pipefail

DURATION="${DURATION:-86400}"             # default 24h
CONCURRENCY="${CONCURRENCY:-16}"
S4_ENDPOINT="${S4_ENDPOINT:-http://localhost:8014}"
BUCKET="${BUCKET:-s4-soak-$(date +%s)}"
S4_PID="${S4_PID:-}"                      # auto-detect if empty
MONITOR_INTERVAL_SECS="${MONITOR_INTERVAL_SECS:-60}"
PAYLOAD_SIZE_KB="${PAYLOAD_SIZE_KB:-128}"
OUTDIR="${OUTDIR:-/tmp/s4-soak/$(date +%Y%m%d-%H%M%S)}"

mkdir -p "$OUTDIR"
echo "[$(date)] S4 soak test starting"
echo "  duration:    ${DURATION}s ($(awk "BEGIN{printf \"%.1f\", $DURATION/3600}")h)"
echo "  concurrency: ${CONCURRENCY}"
echo "  endpoint:    ${S4_ENDPOINT}"
echo "  bucket:      ${BUCKET}"
echo "  payload:     ${PAYLOAD_SIZE_KB} KiB"
echo "  outdir:      ${OUTDIR}"

# --- 1. S4 PID auto-detect (assumes only one s4 binary running) ---
if [ -z "$S4_PID" ]; then
    S4_PID=$(pgrep -f "target/.*/s4 " | head -1 || true)
fi
if [ -z "$S4_PID" ]; then
    echo "ERROR: cannot detect S4 process PID. Set S4_PID env var explicitly." >&2
    exit 1
fi
echo "  S4 PID:      ${S4_PID}"

# --- 2. ensure bucket exists (raw aws-cli — soak test assumes IAM allows
#        bucket creation, or pass an existing bucket) ---
aws --endpoint-url "$S4_ENDPOINT" s3 mb "s3://${BUCKET}" 2>/dev/null || true

# --- 3. generate test payload ---
TEST_FILE="$OUTDIR/payload.bin"
dd if=/dev/urandom of="$TEST_FILE" bs=1024 count="$PAYLOAD_SIZE_KB" 2>/dev/null
echo "[$(date)] payload generated: $(stat -c%s "$TEST_FILE") bytes"

# --- 4. monitor in background: 1 min ごとに RSS / FD / open conn を csv 出力 ---
MONITOR_CSV="$OUTDIR/monitor.csv"
echo "timestamp_unix,rss_kib,fd_count,open_conn,vmsize_kib" > "$MONITOR_CSV"

(
    while kill -0 "$S4_PID" 2>/dev/null; do
        TS=$(date +%s)
        RSS=$(awk '/VmRSS:/ {print $2}' "/proc/${S4_PID}/status" 2>/dev/null || echo 0)
        VMSIZE=$(awk '/VmSize:/ {print $2}' "/proc/${S4_PID}/status" 2>/dev/null || echo 0)
        FD=$(ls "/proc/${S4_PID}/fd/" 2>/dev/null | wc -l || echo 0)
        CONN=$(ss -tnp 2>/dev/null | grep "pid=${S4_PID}" | wc -l || echo 0)
        echo "$TS,$RSS,$FD,$CONN,$VMSIZE" >> "$MONITOR_CSV"
        sleep "$MONITOR_INTERVAL_SECS"
    done
) &
MONITOR_PID=$!
echo "[$(date)] monitor started (pid $MONITOR_PID)"

# --- 5. record initial RSS for leak verdict ---
INITIAL_RSS=$(awk '/VmRSS:/ {print $2}' "/proc/${S4_PID}/status")
echo "[$(date)] initial RSS: ${INITIAL_RSS} KiB"

# --- 6. spawn N concurrent workers doing PUT/GET loop ---
LOAD_LOG="$OUTDIR/load.log"
END_TIME=$(($(date +%s) + DURATION))

worker() {
    local id=$1
    local count=0
    while [ "$(date +%s)" -lt "$END_TIME" ]; do
        local key="worker-${id}/obj-${count}"
        if ! aws --endpoint-url "$S4_ENDPOINT" --quiet \
            s3 cp "$TEST_FILE" "s3://${BUCKET}/${key}" 2>>"$LOAD_LOG"; then
            echo "$(date +%s) PUT_ERR id=$id count=$count" >> "$LOAD_LOG"
        fi
        if ! aws --endpoint-url "$S4_ENDPOINT" --quiet \
            s3 cp "s3://${BUCKET}/${key}" /dev/null 2>>"$LOAD_LOG"; then
            echo "$(date +%s) GET_ERR id=$id count=$count" >> "$LOAD_LOG"
        fi
        # 一定数経ったら DELETE (bucket 容量爆発防止)
        if [ $((count % 100)) -eq 99 ]; then
            aws --endpoint-url "$S4_ENDPOINT" --quiet \
                s3 rm "s3://${BUCKET}/worker-${id}/" --recursive 2>>"$LOAD_LOG" || true
        fi
        count=$((count + 1))
    done
}

echo "[$(date)] spawning ${CONCURRENCY} workers"
PIDS=()
for i in $(seq 1 "$CONCURRENCY"); do
    worker "$i" &
    PIDS+=($!)
done

# --- 7. wait for all workers ---
for p in "${PIDS[@]}"; do
    wait "$p" || true
done
echo "[$(date)] all workers finished"

# --- 8. stop monitor ---
kill "$MONITOR_PID" 2>/dev/null || true

# --- 9. final RSS + leak verdict ---
FINAL_RSS=$(awk '/VmRSS:/ {print $2}' "/proc/${S4_PID}/status")
RATIO_X100=$(( FINAL_RSS * 100 / (INITIAL_RSS == 0 ? 1 : INITIAL_RSS) ))
SUMMARY="$OUTDIR/summary.txt"
{
    echo "S4 soak test summary"
    echo "===================="
    echo "duration:      ${DURATION}s"
    echo "concurrency:   ${CONCURRENCY}"
    echo "payload size:  ${PAYLOAD_SIZE_KB} KiB"
    echo "S4 PID:        ${S4_PID}"
    echo "initial RSS:   ${INITIAL_RSS} KiB"
    echo "final RSS:     ${FINAL_RSS} KiB"
    echo "RSS ratio:     ${RATIO_X100}%"
    echo
    if [ "$RATIO_X100" -lt 200 ]; then
        echo "VERDICT: ✅ no leak detected (final RSS < 2x initial)"
        VERDICT_RC=0
    else
        echo "VERDICT: ❌ POSSIBLE LEAK (final RSS ≥ 2x initial)"
        VERDICT_RC=1
    fi
    echo
    echo "load errors:   $(grep -c "_ERR" "$LOAD_LOG" 2>/dev/null || echo 0)"
    echo "monitor file:  $MONITOR_CSV"
} | tee "$SUMMARY"

exit "${VERDICT_RC:-0}"
