#!/usr/bin/env python3
"""Phase C: full restore throughput (read path) — direct vs S4 zstd-3 / zstd-9.

Restore exercises the sequential read+decompress path end to end. We restore
the standard-default index from each repo, timed, and report MB/s on the
logical (uncompressed) snapshot size.
"""
import json, time, subprocess, urllib.request, os

ES = "http://localhost:9200"; MINIO = "http://localhost:9000"
ENV = dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
           AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
           AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")
SRC = "bench-standard-default"
REPOS = [("repo_direct", "default", "repo-direct", "direct"),
         ("repo_s4z3", "s4z3", "repo-s4z3", "S4 zstd-3"),
         ("repo_s4z9", "s4z9", "repo-s4z9", "S4 zstd-9")]

def es(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{ES}{path}", data=data,
                                 headers={"Content-Type": "application/json"}, method=method)
    try:
        with urllib.request.urlopen(req) as r: return json.load(r)
    except urllib.error.HTTPError as e: return {"_error": e.code, "_body": e.read().decode()[:300]}

def aws(*a): return subprocess.run(["aws", "--endpoint-url", MINIO, *a], env=ENV, capture_output=True, text=True)

# Restore is throttled by the NODE recovery limit (indices.recovery.max_bytes_per_sec,
# default 40mb), not the repo's max_restore_bytes_per_sec (default unlimited). Lift it
# so the measured restore throughput reflects S4 decode + backend, not the throttle.
es("PUT", "/_cluster/settings", {"transient": {"indices.recovery.max_bytes_per_sec": "-1"}})

results = []
for repo, client, bucket, label in REPOS:
    es("DELETE", f"/_snapshot/{repo}"); aws("s3", "rm", f"s3://{bucket}", "--recursive")
    es("PUT", f"/_snapshot/{repo}", {"type": "s3", "settings": {"bucket": bucket, "client": client,
       "max_snapshot_bytes_per_sec": "-1", "max_restore_bytes_per_sec": "-1"}})
    snap = "snapc"
    es("PUT", f"/_snapshot/{repo}/{snap}?wait_for_completion=true", {"indices": SRC, "include_global_state": False})
    st = es("GET", f"/_snapshot/{repo}/{snap}/_status")["snapshots"][0]["stats"]
    logical = st["total"]["size_in_bytes"]
    dest = f"restore-{label.replace(' ', '').replace('-', '').lower()}"
    es("DELETE", f"/{dest}")
    t0 = time.time()
    r = es("POST", f"/_snapshot/{repo}/{snap}/_restore?wait_for_completion=true",
           {"indices": SRC, "rename_pattern": SRC, "rename_replacement": dest,
            "index_settings": {"index.number_of_replicas": 0}})
    dt = time.time() - t0
    mbps = (logical / 1e6) / dt if dt > 0 else 0
    rec = {"repo": label, "logical_mb": round(logical/1e6, 1), "restore_s": round(dt, 2), "mb_per_s": round(mbps, 1)}
    results.append(rec)
    print(f"{label:12s} logical={rec['logical_mb']:8} MB  restore={rec['restore_s']:6}s  {rec['mb_per_s']:7} MB/s", flush=True)
    es("DELETE", f"/{dest}")

with open("./results/phase_c.json", "w") as f:
    json.dump(results, f, indent=2)
print("\nwrote results/phase_c.json")
