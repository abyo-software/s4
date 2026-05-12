#!/usr/bin/env bash
# v0.3 #14 — compression-ratio comparison driver.
#
# Brings up the docker-compose stack (S4 cpu / S4 gpu / Garage / MinIO),
# uploads three workloads to each, measures stored bytes via HEAD, and
# writes bench-result.csv with (workload, system, original, stored, ratio,
# put_secs, get_secs, peak_rss_mb).
#
# Usage:
#   ./benches/comparison/run.sh
#
# Requires: docker compose, aws-cli, gnu-coreutils (du, dd, time).
# Skips systems that don't bring up cleanly (e.g. s4-gpu without --gpus).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

OUT="${1:-bench-result.csv}"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

# --- Workload generators --------------------------------------------------

gen_nginx_log() {
  # Realistic-ish nginx access log lines, repeated to fill `$1` bytes.
  local target="$1" out="$2"
  python3 - "$target" "$out" <<'PY'
import sys, random
target = int(sys.argv[1]); out = sys.argv[2]
random.seed(42)
ips = [f"203.0.113.{i}" for i in range(2, 250)]
paths = ["/api/v1/users/", "/api/v1/orders/", "/static/main.js", "/static/style.css", "/health"]
n = 0
with open(out, "w") as f:
    while n < target:
        ip = random.choice(ips)
        path = random.choice(paths)
        bid = random.randint(1, 99999)
        line = f'{ip} - - [12/May/2026:10:30:45 +0000] "GET {path}{bid} HTTP/1.1" 200 4521 "https://example.com/" "Mozilla/5.0"\n'
        f.write(line)
        n += len(line)
PY
}

gen_parquet_like() {
  # Mixed numeric column + text metadata, ~2x compressible.
  local target="$1" out="$2"
  python3 - "$target" "$out" <<'PY'
import sys, struct
target = int(sys.argv[1]); out = sys.argv[2]
n = 0
with open(out, "wb") as f:
    counter = 0
    while n < target:
        block = bytearray()
        for _ in range(1024):
            block += struct.pack("<I", counter)
            counter += 1
        for _ in range(32):
            block += b"col=user_id,type=u32,encoding=plain\n"
        f.write(block)
        n += len(block)
PY
}

gen_random() {
  # Random binary, no compression possible.
  local target="$1" out="$2"
  dd if=/dev/urandom of="$out" bs=1M count=$((target / 1024 / 1024)) status=none
}

# --- Backend definitions --------------------------------------------------

# Format: name|endpoint|access_key|secret_key|bucket
declare -a BACKENDS=()

probe_backend() {
  local name="$1" endpoint="$2" ak="$3" sk="$4"
  AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" \
    aws --endpoint-url "$endpoint" s3 ls 2>/dev/null >/dev/null
}

setup_backends() {
  echo "Probing backends..."
  for entry in \
    "garage|http://localhost:9100|garage_test|garage_secret|bench-garage" \
    "minio|http://localhost:9101|minioadmin|minioadmin|bench-minio" \
    "s4-cpu|http://localhost:9102|minioadmin|minioadmin|bench-minio" \
    "s4-gpu|http://localhost:9103|minioadmin|minioadmin|bench-minio" \
  ; do
    IFS='|' read -r name endpoint ak sk bucket <<< "$entry"
    if probe_backend "$name" "$endpoint" "$ak" "$sk"; then
      BACKENDS+=("$entry")
      echo "  ✓ $name @ $endpoint"
      AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" \
        aws --endpoint-url "$endpoint" s3 mb "s3://$bucket" 2>/dev/null || true
    else
      echo "  ✗ $name @ $endpoint (skipped — not reachable)"
    fi
  done
}

# --- Measure one (backend, workload) cell --------------------------------

# Args: name endpoint ak sk bucket workload_name local_file
measure() {
  local name="$1" endpoint="$2" ak="$3" sk="$4" bucket="$5" wl="$6" file="$7"
  local key="bench/$wl-$(date +%s)"
  local original
  original=$(stat -c %s "$file")

  # PUT (timed)
  local t0 t1 put_secs
  t0=$(date +%s.%N)
  if ! AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" \
        aws --endpoint-url "$endpoint" s3 cp "$file" "s3://$bucket/$key" \
        --no-progress >/dev/null 2>&1; then
    echo "$wl,$name,$original,,,,," >> "$OUT"
    return
  fi
  t1=$(date +%s.%N)
  put_secs=$(awk "BEGIN { print $t1 - $t0 }")

  # Stored size: HEAD via aws-cli, then look at Content-Length the
  # backend returns (= raw stored bytes for Garage / MinIO with
  # native compression on the wire). For S4-fronted backends, we hit
  # the underlying MinIO directly to see post-compression bytes.
  local stored
  if [[ "$name" == s4-* ]]; then
    stored=$(AWS_ACCESS_KEY_ID="minioadmin" AWS_SECRET_ACCESS_KEY="minioadmin" \
      aws --endpoint-url "http://localhost:9101" s3api head-object \
      --bucket "$bucket" --key "$key" 2>/dev/null \
      | python3 -c "import sys, json; d=json.load(sys.stdin); print(d.get('ContentLength', ''))")
  else
    stored=$(AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" \
      aws --endpoint-url "$endpoint" s3api head-object \
      --bucket "$bucket" --key "$key" 2>/dev/null \
      | python3 -c "import sys, json; d=json.load(sys.stdin); print(d.get('ContentLength', ''))")
  fi
  if [[ -z "$stored" ]]; then
    stored="$original"  # MinIO might not reflect compressed bytes in head; fall back
  fi

  # GET (timed)
  local out_file="$TMPDIR/$wl-$name.out"
  t0=$(date +%s.%N)
  AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" \
    aws --endpoint-url "$endpoint" s3 cp "s3://$bucket/$key" "$out_file" \
    --no-progress >/dev/null 2>&1 || true
  t1=$(date +%s.%N)
  local get_secs
  get_secs=$(awk "BEGIN { print $t1 - $t0 }")

  # Cleanup
  AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" \
    aws --endpoint-url "$endpoint" s3 rm "s3://$bucket/$key" >/dev/null 2>&1 || true

  local ratio
  if [[ -n "$stored" && "$stored" -gt 0 ]]; then
    ratio=$(awk "BEGIN { printf \"%.2f\", $original / $stored }")
  else
    ratio="n/a"
  fi
  echo "$wl,$name,$original,$stored,$ratio,$put_secs,$get_secs," >> "$OUT"
}

# --- Main -----------------------------------------------------------------

echo "workload,system,original_bytes,stored_bytes,ratio,put_secs,get_secs,peak_rss_mb" > "$OUT"
setup_backends
if [[ ${#BACKENDS[@]} -eq 0 ]]; then
  echo "No backends reachable. Did you 'docker compose up -d' first?" >&2
  exit 1
fi

# Generate workloads
SIZE_TEXT="${SIZE_TEXT:-67108864}"   # 64 MiB default; big workloads via SIZE_TEXT=...
SIZE_RAND="${SIZE_RAND:-16777216}"   # 16 MiB random

echo "Generating workloads..."
WL_NGINX="$TMPDIR/nginx.log"
WL_PARQUET="$TMPDIR/parquet.bin"
WL_RANDOM="$TMPDIR/random.bin"
gen_nginx_log "$SIZE_TEXT" "$WL_NGINX"
gen_parquet_like "$SIZE_TEXT" "$WL_PARQUET"
gen_random "$SIZE_RAND" "$WL_RANDOM"

declare -a WORKLOADS=(
  "nginx-log|$WL_NGINX"
  "parquet-like|$WL_PARQUET"
  "random-bytes|$WL_RANDOM"
)

for wl_entry in "${WORKLOADS[@]}"; do
  IFS='|' read -r wl file <<< "$wl_entry"
  echo "=== $wl ($(stat -c %s "$file") bytes) ==="
  for be in "${BACKENDS[@]}"; do
    IFS='|' read -r name endpoint ak sk bucket <<< "$be"
    echo -n "  $name ... "
    measure "$name" "$endpoint" "$ak" "$sk" "$bucket" "$wl" "$file"
    echo "done"
  done
done

echo ""
echo "Result: $OUT"
column -t -s, "$OUT" | head -30
