#!/usr/bin/env python3
"""Phase A: snapshot each index through each repo, measure stored bytes + time.

For every (index, repo) pair: re-register the repo + empty its bucket (clean
slate), snapshot the single index (timed), then read the *actual* bytes stored
on MinIO (compressed blobs + S4 sidecars) via the direct backend endpoint.
"""
import json, time, subprocess, urllib.request, os

ES = "http://localhost:9200"
MINIO = "http://localhost:9000"
ENV = dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
           AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
           AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")

INDICES = ["bench-standard-default", "bench-standard-bestcomp", "bench-logsdb"]
REPOS = [  # (repo, client, bucket, label)
    ("repo_direct", "default", "repo-direct", "direct (no S4)"),
    ("repo_s4z3",  "s4z3",  "repo-s4z3",  "S4 zstd-3"),
    ("repo_s4z9",  "s4z9",  "repo-s4z9",  "S4 zstd-9"),
    ("repo_s4z19", "s4z19", "repo-s4z19", "S4 zstd-19"),
]

def es(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{ES}{path}", data=data,
                                 headers={"Content-Type": "application/json"}, method=method)
    with urllib.request.urlopen(req) as r:
        return json.load(r)

def aws(*args):
    return subprocess.run(["aws", "--endpoint-url", MINIO, *args],
                          env=ENV, capture_output=True, text=True)

def bucket_bytes(bucket):
    r = aws("s3api", "list-objects-v2", "--bucket", bucket,
            "--query", "[sum(Contents[].Size), length(Contents)]", "--output", "json")
    if r.returncode != 0:
        return 0, 0
    v = json.loads(r.stdout or "[null,0]")
    return int(v[0] or 0), int(v[1] or 0)

def empty_bucket(bucket):
    aws("s3", "rm", f"s3://{bucket}", "--recursive")

def register(repo, client, bucket):
    es("PUT", f"/_snapshot/{repo}", {"type": "s3", "settings": {"bucket": bucket, "client": client,
       "max_snapshot_bytes_per_sec": "-1", "max_restore_bytes_per_sec": "-1"}})

results = []
for ix in INDICES:
    for repo, client, bucket, label in REPOS:
        # clean slate: drop ES's cached repo state + wipe the bucket
        try: es("DELETE", f"/_snapshot/{repo}")
        except Exception: pass
        empty_bucket(bucket)
        register(repo, client, bucket)
        snap = f"snap-{ix}"
        try: es("DELETE", f"/_snapshot/{repo}/{snap}")
        except Exception: pass
        # snapshot (timed)
        t0 = time.time()
        es("PUT", f"/_snapshot/{repo}/{snap}?wait_for_completion=true",
           {"indices": ix, "include_global_state": False})
        wall = time.time() - t0
        st = es("GET", f"/_snapshot/{repo}/{snap}/_status")["snapshots"][0]
        es_total = st["stats"]["total"]["size_in_bytes"]
        es_files = st["stats"]["total"]["file_count"]
        es_dur_ms = st["stats"].get("total", {}).get("processing_time_in_millis") or \
                    st["stats"].get("processing_time_in_millis") or int(wall*1000)
        stored, nobj = bucket_bytes(bucket)
        rec = {"index": ix, "repo": repo, "label": label,
               "es_snapshot_bytes": es_total, "es_files": es_files,
               "stored_bytes": stored, "stored_objects": nobj,
               "snapshot_wall_s": round(wall, 2), "es_dur_ms": es_dur_ms}
        results.append(rec)
        print(f"{ix:26s} {label:14s} es={es_total/1e6:8.1f}MB stored={stored/1e6:8.1f}MB "
              f"files={es_files:4d} objs={nobj:4d} wall={wall:6.2f}s", flush=True)
        # delete snapshot to keep things tidy for the next pair
        try: es("DELETE", f"/_snapshot/{repo}/{snap}")
        except Exception: pass

with open("./results/phase_a.json", "w") as f:
    json.dump(results, f, indent=2)
print("\nwrote results/phase_a.json")
