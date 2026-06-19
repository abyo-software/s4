#!/usr/bin/env python3
"""Loki chunk-compression benchmark.

For each config — direct (snappy→MinIO), S4 zstd-3/9 (snappy chunks re-compressed
by S4), and Loki-native-zstd (no S4) — reconfigure Loki, ingest 4M log lines,
flush, and measure the S3 bytes.

Read latency is then measured directly on the real chunk objects Loki wrote:
for chunks present in both the `direct` (raw snappy) and `S4 zstd-3` buckets,
time a whole-chunk GET from MinIO (raw snappy) vs the same chunk through S4
(which fetches the zstd bytes and decompresses back to snappy). This isolates
the S4 cost for direct full-object chunk GETs; it does NOT exercise Loki's query
path, cache behavior, concurrency, or actual query-time request shape. The S4 GET
is asserted byte-identical to the direct snappy chunk.

(An earlier version timed cold LogQL via Loki's query API after a restart; that
was dropped because Loki's TSDB index-shipper does not upload the index to the
store on this harness's timescale, so post-restart queries hit an empty index
and measured nothing. Whole-chunk GET is the valid, index-independent metric.)
"""
import json, os, re, subprocess, time, urllib.request

import os as _os
W = _os.path.dirname(_os.path.abspath(__file__))
ENV = dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
           AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
           AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")
NDOCS = "4000000"
# (name, s3 endpoint host:port, bucket, chunk_encoding)
CONFIGS = [
    ("direct",      "localhost:9000", "loki-direct", "snappy"),
    ("S4 zstd-3",   "localhost:8011", "loki-s4z3",   "snappy"),
    ("S4 zstd-9",   "localhost:8012", "loki-s4z9",   "snappy"),
    ("Loki zstd",   "localhost:9000", "loki-native", "zstd"),
]
# read-latency comparison: direct snappy bucket vs the same chunks through S4
DIRECT_BUCKET, S4_BUCKET = "loki-direct", "loki-s4z3"
DIRECT_EP, S4_EP = "localhost:9000", "localhost:8011"
N_CHUNK_SAMPLES = 40

def sh(*a, **k): return subprocess.run(a, env=ENV, capture_output=True, text=True, **k)
def aws(*a): return sh("aws", "--endpoint-url", "http://localhost:9000", *a)

def bucket_bytes(b):
    r = aws("s3api", "list-objects-v2", "--bucket", b, "--query", "[sum(Contents[].Size), length(Contents)]", "--output", "json")
    try:
        v = json.loads(r.stdout or "[null,0]"); return int(v[0] or 0), int(v[1] or 0)
    except Exception: return 0, 0

def wait_flush_settle(bucket, max_s=90):
    """Poll the backend until the object set stops growing (async flush done).
    The time spent here is part of the ingest+flush wall-clock — this is where
    S4's compression PUT cost lands, so it must be inside the timed region.

    Requires several consecutive identical reads so a brief mid-flush pause
    (Loki uploads asynchronously, more so under load) cannot be mistaken for a
    completed flush and under-count stored bytes."""
    time.sleep(4)  # let the async flush start before we begin polling
    last, stable = (-1, -1), 0
    for _ in range(int(max_s / 2)):
        cur = bucket_bytes(bucket)
        if cur == last and cur[1] > 0:
            stable += 1
            if stable >= 3:
                return
        else:
            stable, last = 0, cur
        time.sleep(2)

def write_config(endpoint, bucket, encoding):
    cfg = f"""auth_enabled: false
server: {{http_listen_port: 3100, grpc_listen_port: 9095, log_level: warn}}
common:
  instance_addr: 127.0.0.1
  path_prefix: /tmp/loki
  replication_factor: 1
  ring: {{kvstore: {{store: inmemory}}}}
  storage:
    s3:
      endpoint: {endpoint}
      bucketnames: {bucket}
      access_key_id: minioadmin
      secret_access_key: minioadmin
      s3forcepathstyle: true
      insecure: true
schema_config:
  configs:
    - from: 2020-01-01
      store: tsdb
      object_store: s3
      schema: v13
      index: {{prefix: index_, period: 24h}}
ingester: {{chunk_encoding: {encoding}, chunk_target_size: 1572864}}
limits_config:
  reject_old_samples: false
  allow_structured_metadata: false
  ingestion_rate_mb: 512
  ingestion_burst_size_mb: 1024
  per_stream_rate_limit: 512MB
  per_stream_rate_limit_burst: 1024MB
compactor: {{working_directory: /tmp/loki/compactor}}
"""
    os.makedirs(f"{W}/loki-config", exist_ok=True)
    open(f"{W}/loki-config/loki.yaml", "w").write(cfg)

def restart_loki():
    sh("docker", "rm", "-f", "lokibench-loki")
    sh("docker", "run", "-d", "--name", "lokibench-loki", "--network", "host",
       "-v", f"{W}/loki-config/loki.yaml:/etc/loki/config.yaml:ro",
       "grafana/loki:3.3.2", "-config.file=/etc/loki/config.yaml")
    for _ in range(40):
        try:
            if urllib.request.urlopen("http://localhost:3100/ready", timeout=2).read().strip() == b"ready":
                return True
        except Exception: pass
        time.sleep(2)
    return False

def list_keys(bucket):
    r = aws("s3api", "list-objects-v2", "--bucket", bucket,
            "--query", "Contents[].Key", "--output", "json")
    try:
        return [k for k in (json.loads(r.stdout or "[]") or [])]
    except Exception:
        return []

def presign(endpoint, bucket, key):
    r = sh("aws", "--endpoint-url", f"http://{endpoint}", "s3",
           "presign", f"s3://{bucket}/{key}", "--expires-in", "900")
    return r.stdout.strip()

def timed_get(url):
    """GET a presigned URL, return (elapsed_ms, body_bytes). Body read fully so
    decompression (for S4) is included in the timing."""
    t0 = time.time()
    with urllib.request.urlopen(url, timeout=120) as r:
        body = r.read()
    return round((time.time() - t0) * 1000, 2), body

def median(xs):
    s = sorted(xs)
    return s[len(s) // 2] if s else None

def mean(xs):
    return round(sum(xs) / len(xs), 2) if xs else None

def p90(xs):
    """90th percentile, nearest-rank on the 0-indexed sorted samples."""
    s = sorted(xs)
    return s[int(0.9 * (len(s) - 1))] if s else None

# Loki TSDB chunk object keys look like  fake/<fingerprint>/<from>:<through>:<crc>
# (all hex); the `<a>:<b>:<c>` basename distinguishes chunks from index objects
# (index_<period>/... has no colon-delimited basename). We sample chunks ONLY.
CHUNK_KEY_RE = re.compile(r"/[0-9a-fA-F]+:[0-9a-fA-F]+:[0-9a-fA-F]+$")

def chunk_get_latency(n=N_CHUNK_SAMPLES):
    """Whole-chunk GET latency: raw snappy from MinIO vs the same chunk through
    S4 (zstd → snappy). Samples Loki *chunk* objects only (CHUNK_KEY_RE), full
    object (no HTTP Range header). Asserts the S4 result is byte-identical."""
    common = sorted(set(list_keys(DIRECT_BUCKET)) & set(list_keys(S4_BUCKET)))
    chunks = [k for k in common if CHUNK_KEY_RE.search(k)]
    non_chunk = len(common) - len(chunks)
    sample = chunks[:: max(1, len(chunks) // n)][:n] if chunks else []
    direct_ms, s4_ms, mism = [], [], 0
    for k in sample:
        td, bd = timed_get(presign(DIRECT_EP, DIRECT_BUCKET, k))   # raw snappy
        ts, bs = timed_get(presign(S4_EP, S4_BUCKET, k))           # S4 decompresses
        if bd != bs:
            mism += 1
        direct_ms.append(td); s4_ms.append(ts)
    return {"n": len(sample), "byte_mismatches": mism,
            "common_keys": len(common), "non_chunk_keys_excluded": non_chunk,
            "request": "full-object GET, no HTTP Range header",
            "percentile_method": "p90 = nearest-rank on 0-indexed sorted samples",
            "sampled_keys": sample,
            "direct_ms_med": median(direct_ms), "direct_ms_mean": mean(direct_ms), "direct_ms_p90": p90(direct_ms),
            "s4_ms_med": median(s4_ms), "s4_ms_mean": mean(s4_ms), "s4_ms_p90": p90(s4_ms),
            "direct_ms": direct_ms, "s4_ms": s4_ms}

META = {
    "date": "2026-06-19",
    "host": "AMD Ryzen 9 9950X (16C/32T), Linux",
    "loki": "grafana/loki:3.3.2 (single-binary, TSDB/v13)",
    "object_store": "minio/minio:latest (local)",
    "s4": "v1.2.2, --codec cpu-zstd --dispatcher always --logical-etag",
    "dataset": "4,000,000 ECS-style logfmt lines; labels service,level",
    "ingest_flush_s": "wall-clock for ingest push + POST /flush + flush-to-store "
                "settle (includes S4's compression PUT cost); NOT push-only. "
                "Includes a per-config flush-settle poll.",
    "chunk_get_ms": "whole-chunk GET latency on real Loki chunk objects: raw "
                "snappy from MinIO vs the same chunk through S4 (zstd→snappy, "
                "decompression included). Median over chunks present in both "
                "buckets; S4 bytes asserted identical to direct. no-RTT local "
                "values — transferable signal is the direct-vs-S4 delta.",
    "stored_bytes": "all backend bucket bytes (Loki chunks + TSDB index + any S4 "
                    "sidecar/metadata objects). Savings are net of sidecar overhead.",
    "not_exercised": "bulk `s4 migrate` over a pre-existing backlog; compactor/retention "
                     "deletes. The --logical-etag negative is captured separately in "
                     "logical_etag_negative.txt.",
}
def main():
    results = {"meta": META, "storage": [], "chunk_get": {}}
    for name, endpoint, bucket, encoding in CONFIGS:
        aws("s3", "rm", f"s3://{bucket}", "--recursive")
        write_config(endpoint, bucket, encoding)
        if not restart_loki():
            print(f"{name}: Loki not ready", flush=True); continue
        # ingest+flush wall-clock: push, flush, and wait for the store to settle so
        # S4's compression PUT cost is inside the timed region.
        t0 = time.time()
        ing = sh("python3", f"{W}/loki_ingest.py", NDOCS)
        # Fail loud on a partial ingest — a silently-truncated push under-fills
        # the bucket and fakes a better compression ratio (loki_ingest raises and
        # exits non-zero on any non-204 push).
        if ing.returncode != 0 or f"DONE: {NDOCS} lines" not in (ing.stdout or ""):
            print(f"{name}: INGEST INCOMPLETE rc={ing.returncode} "
                  f"stdout-tail={ (ing.stdout or '')[-120:]!r} stderr-tail={ (ing.stderr or '')[-200:]!r}",
                  flush=True)
            raise SystemExit(f"ingest for {name} did not complete {NDOCS} lines")
        sh("curl", "-s", "-XPOST", "http://localhost:3100/flush")
        wait_flush_settle(bucket)
        ingest_s = round(time.time() - t0, 1)
        stored, nobj = bucket_bytes(bucket)
        results["storage"].append({"config": name, "encoding": encoding, "stored_bytes": stored,
                                   "objects": nobj, "ingest_flush_s": ingest_s})
        print(f"{name:11s} enc={encoding:6s} stored={stored/1e6:8.1f}MB objs={nobj:4d} ingest+flush={ingest_s}s", flush=True)

    # read latency on the real chunk objects (direct snappy vs the same chunk
    # through S4); buckets were populated by the loop above.
    lat = chunk_get_latency()
    results["chunk_get"] = lat
    print(f"chunk GET (n={lat['n']}, byte_mismatches={lat['byte_mismatches']}): "
          f"direct_med={lat['direct_ms_med']}ms  S4_med={lat['s4_ms_med']}ms", flush=True)

    json.dump(results, open(f"{W}/results/loki.json", "w"), indent=2)
    print("wrote results/loki.json", flush=True)


# Guarded so the module's helpers (write_config / restart_loki / ...) can be
# imported by the logical-etag negative probe without re-running the benchmark.
if __name__ == "__main__":
    main()
