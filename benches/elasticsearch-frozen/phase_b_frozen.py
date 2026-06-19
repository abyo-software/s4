#!/usr/bin/env python3
"""Phase B: mount snapshots as frozen searchable snapshots, measure COLD search.

For each index mode we mount the snapshot from the `direct` repo and from the
`S4 zstd-3` repo, clear the shared cache before every query (forcing a cold
fetch from the repository through S4), and record server-side `took`. This
isolates the decompression + sidecar-range overhead S4 adds on the frozen read
path. A warm pass (cache populated) is also recorded.
"""
import json, time, subprocess, urllib.request, os, statistics

ES = "http://localhost:9200"
MINIO = "http://localhost:9000"
ENV = dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
           AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
           AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")

INDICES = ["bench-standard-default", "bench-standard-bestcomp", "bench-logsdb"]
# repos to compare for read latency
REPOS = [("repo_direct", "default", "repo-direct", "direct"),
         ("repo_s4z3", "s4z3", "repo-s4z3", "S4 zstd-3"),
         ("repo_s4z19", "s4z19", "repo-s4z19", "S4 zstd-19")]
COLD_REPS = 6

def es(method, path, body=None, ok=(200, 201)):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{ES}{path}", data=data,
                                 headers={"Content-Type": "application/json"}, method=method)
    try:
        with urllib.request.urlopen(req) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        return {"_error": e.code, "_body": e.read().decode()[:300]}

def aws(*args):
    return subprocess.run(["aws", "--endpoint-url", MINIO, *args], env=ENV,
                          capture_output=True, text=True)

def empty_bucket(b): aws("s3", "rm", f"s3://{b}", "--recursive")

def queries():
    return {
        "term_rare(status:500)": {"size": 0, "track_total_hits": True,
            "query": {"term": {"http.response.status_code": 500}}},
        "agg(date_hist+terms)": {"size": 0,
            "aggs": {"svc": {"terms": {"field": "service.name", "size": 10}},
                     "t": {"date_histogram": {"field": "@timestamp", "fixed_interval": "1h"}}}},
        "fulltext(message:items)": {"size": 0, "track_total_hits": True,
            "query": {"match": {"message": "items"}}},
        "topN(level:ERROR sort ts)": {"size": 20, "query": {"term": {"log.level": "ERROR"}},
            "sort": [{"@timestamp": "desc"}]},
    }

def clear_cache():
    es("POST", "/_searchable_snapshots/cache/clear")

def run_query(idx, body):
    t0 = time.time()
    r = es("GET", f"/{idx}/_search", body)
    wall = (time.time() - t0) * 1000
    return r.get("took", -1), wall, r

results = []
for ix in INDICES:
    for repo, client, bucket, label in REPOS:
        # fresh snapshot for this pair
        es("DELETE", f"/_snapshot/{repo}")
        empty_bucket(bucket)
        es("PUT", f"/_snapshot/{repo}", {"type": "s3", "settings": {"bucket": bucket, "client": client,
           "max_snapshot_bytes_per_sec": "-1", "max_restore_bytes_per_sec": "-1"}})
        snap = f"snapb-{ix}"
        es("PUT", f"/_snapshot/{repo}/{snap}?wait_for_completion=true",
           {"indices": ix, "include_global_state": False})
        frozen = f"frozen-{label.replace(' ', '').replace('-', '')}-{ix}".lower()
        es("DELETE", f"/{frozen}")
        m = es("POST", f"/_snapshot/{repo}/{snap}/_mount?wait_for_completion=true&storage=shared_cache",
               {"index": ix, "renamed_index": frozen})
        if "_error" in m:
            print(f"MOUNT FAIL {ix} {label}: {m}", flush=True)
            continue
        # wait until searchable
        for _ in range(30):
            h = es("GET", f"/_cluster/health/{frozen}?wait_for_status=yellow&timeout=5s")
            if h.get("status") in ("yellow", "green"): break
            time.sleep(1)
        for qname, qbody in queries().items():
            cold = []
            for _ in range(COLD_REPS):
                clear_cache(); time.sleep(0.3)
                took, wall, r = run_query(frozen, qbody)
                if took >= 0: cold.append(took)
            # warm
            warm = []
            for _ in range(COLD_REPS):
                took, wall, r = run_query(frozen, qbody)
                if took >= 0: warm.append(took)
            rec = {"index": ix, "repo": label, "query": qname,
                   "cold_ms_median": round(statistics.median(cold), 1) if cold else None,
                   "cold_ms_min": min(cold) if cold else None,
                   "warm_ms_median": round(statistics.median(warm), 1) if warm else None}
            results.append(rec)
            print(f"{ix:26s} {label:10s} {qname:26s} cold_med={rec['cold_ms_median']:7} "
                  f"warm_med={rec['warm_ms_median']:6}", flush=True)
        es("DELETE", f"/{frozen}")
        clear_cache()

with open("./results/phase_b.json", "w") as f:
    json.dump(results, f, indent=2)
print("\nwrote results/phase_b.json")
