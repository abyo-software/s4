#!/usr/bin/env python3
"""Phase B1: cold frozen-search latency under injected backend RTT.

The headline cold-search numbers in phase_b.json are measured against a
co-located MinIO with effectively zero network RTT, so their *absolute* values
(2-4 ms for analytics, ~1.7-7.2 s for cold top-N+sort) are a property of THIS
host, not of any real S3 deployment. The transferable metric is S4's *relative*
overhead. This phase injects a one-way delay on the S4<->backend path and shows
the relative overhead stays ~RTT-invariant: S4 adds a roughly fixed fraction
regardless of how slow the backend leg is.

Method
------
A toxiproxy proxy sits in front of the object store. We compare two repository
clients that BOTH traverse that proxy (so both eat the same injected RTT):
  - tdirect : ES -> toxiproxy -> MinIO          (no S4)
  - ts4z3   : ES -> S4 (zstd-3) -> toxiproxy -> MinIO
For each injected one-way delay we re-mount the snapshot, clear the shared
cache before every query (cold), and record server-side `took`. Relative
overhead = (s4 - direct) / direct.

Env (defaults reproduce the canonical local run):
  ES_URL          default http://localhost:9200
  TOXI_URL        default http://localhost:8474   (toxiproxy admin API)
  TOXI_PROXY      default minio                    (proxy name to add latency to)
  REPO_TDIRECT    default tdirect  (s3 client traversing toxiproxy, no S4)
  REPO_TS4Z3      default ts4z3    (s3 client: S4 zstd-3 upstream=toxiproxy)
  BUCKET_TDIRECT  default esrev-direct
  BUCKET_TS4Z3    default esrev-s4z3
  RTT_MS          default "0,5,20,50"   one-way delays (ms) to sweep
  INDEX           default bench-standard-default
  COLD_REPS       default 5
"""
import json, os, time, statistics, subprocess, urllib.request, urllib.error

ES = os.environ.get("ES_URL", "http://localhost:9200")
TOXI = os.environ.get("TOXI_URL", "http://localhost:8474")
PROXY = os.environ.get("TOXI_PROXY", "minio")
REPO_TDIRECT_CLIENT = os.environ.get("REPO_TDIRECT", "tdirect")
REPO_TS4_CLIENT = os.environ.get("REPO_TS4Z3", "ts4z3")
BUCKET_TDIRECT = os.environ.get("BUCKET_TDIRECT", "esrev-direct")
BUCKET_TS4 = os.environ.get("BUCKET_TS4Z3", "esrev-s4z3")
RTT_MS = [int(x) for x in os.environ.get("RTT_MS", "0,5,20,50").split(",")]
INDEX = os.environ.get("INDEX", "bench-standard-default")
COLD_REPS = int(os.environ.get("COLD_REPS", "5"))
MINIO = os.environ.get("MINIO_URL", "http://localhost:9100")
ENV = dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
           AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
           AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")


def es(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{ES}{path}", data=data,
                                 headers={"Content-Type": "application/json"}, method=method)
    try:
        with urllib.request.urlopen(req) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        return {"_error": e.code, "_body": e.read().decode()[:300]}


def toxi(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{TOXI}{path}", data=data,
                                 headers={"Content-Type": "application/json"}, method=method)
    try:
        with urllib.request.urlopen(req) as r:
            return json.load(r) if r.length != 0 else {}
    except urllib.error.HTTPError as e:
        return {"_error": e.code, "_body": e.read().decode()[:300]}


def set_latency(one_way_ms):
    # remove any existing latency toxic, then add the new one (both directions).
    toxi("DELETE", f"/proxies/{PROXY}/toxics/latency_down")
    toxi("DELETE", f"/proxies/{PROXY}/toxics/latency_up")
    if one_way_ms > 0:
        toxi("POST", f"/proxies/{PROXY}/toxics",
             {"name": "latency_down", "type": "latency", "stream": "downstream",
              "attributes": {"latency": one_way_ms, "jitter": 0}})
        toxi("POST", f"/proxies/{PROXY}/toxics",
             {"name": "latency_up", "type": "latency", "stream": "upstream",
              "attributes": {"latency": one_way_ms, "jitter": 0}})


def aws(*args):
    return subprocess.run(["aws", "--endpoint-url", MINIO, *args], env=ENV,
                          capture_output=True, text=True)


QUERIES = {
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
    r = es("GET", f"/{idx}/_search", body)
    return r.get("took", -1)


def mount(repo_name, client, bucket, label):
    # fresh snapshot through this client onto its bucket, then frozen-mount.
    es("DELETE", f"/_snapshot/{repo_name}")
    aws("s3", "rm", f"s3://{bucket}", "--recursive")
    es("PUT", f"/_snapshot/{repo_name}", {"type": "s3", "settings": {
        "bucket": bucket, "client": client,
        "max_snapshot_bytes_per_sec": "-1", "max_restore_bytes_per_sec": "-1"}})
    snap = f"b1-{INDEX}"
    es("PUT", f"/_snapshot/{repo_name}/{snap}?wait_for_completion=true",
       {"indices": INDEX, "include_global_state": False})
    frozen = f"frozenb1-{label}-{INDEX}".lower().replace(" ", "").replace("-", "x")
    es("DELETE", f"/{frozen}")
    m = es("POST", f"/_snapshot/{repo_name}/{snap}/_mount?wait_for_completion=true&storage=shared_cache",
           {"index": INDEX, "renamed_index": frozen})
    if "_error" in m:
        print(f"MOUNT FAIL {label}: {m}", flush=True)
        return None
    for _ in range(30):
        h = es("GET", f"/_cluster/health/{frozen}?wait_for_status=yellow&timeout=5s")
        if h.get("status") in ("yellow", "green"):
            break
        time.sleep(1)
    return frozen


results = []
for rtt in RTT_MS:
    set_latency(rtt)
    time.sleep(0.5)
    per_arm = {}
    for client, bucket, label in [(REPO_TDIRECT_CLIENT, BUCKET_TDIRECT, "direct"),
                                  (REPO_TS4_CLIENT, BUCKET_TS4, "S4 zstd-3")]:
        repo = f"b1_{label.replace(' ', '').replace('-', '')}".lower()
        frozen = mount(repo, client, bucket, label)
        if frozen is None:
            per_arm[label] = None
            continue
        qmeds = {}
        for qname, qbody in QUERIES.items():
            cold = []
            for _ in range(COLD_REPS):
                clear_cache(); time.sleep(0.3)
                t = run_query(frozen, qbody)
                if t >= 0:
                    cold.append(t)
            qmeds[qname] = round(statistics.median(cold), 1) if cold else None
        per_arm[label] = qmeds
        es("DELETE", f"/{frozen}")
        clear_cache()
    # compute relative overhead per query
    direct = per_arm.get("direct") or {}
    s4 = per_arm.get("S4 zstd-3") or {}
    for qname in QUERIES:
        d = direct.get(qname)
        s = s4.get(qname)
        rel = None
        if d and s and d > 0:
            rel = round((s - d) / d * 100, 1)
        rec = {"rtt_one_way_ms": rtt, "index": INDEX, "query": qname,
               "direct_cold_ms_median": d, "s4_cold_ms_median": s,
               "rel_overhead_pct": rel}
        results.append(rec)
        print(f"rtt={rtt:3d}ms {qname:26s} direct={str(d):>7} s4={str(s):>7} "
              f"rel={'' if rel is None else f'{rel:+.1f}%'}", flush=True)

# clean up latency toxics
set_latency(0)

out = {
    "measurement": "B1 - cold frozen-search latency under injected backend RTT",
    "method": ("toxiproxy injects symmetric one-way delay on the object-store leg; "
               "both arms (direct, S4 zstd-3) traverse it so they eat the same RTT. "
               "Shared cache cleared before every query (cold). Relative overhead = "
               "(s4 - direct)/direct. Local MinIO base, no AWS billing."),
    "host": "AMD Ryzen 9 9950X, ES 9.4.2, MinIO RELEASE.2025-09-07, S4 v1.2.2",
    "rtt_levels_one_way_ms": RTT_MS,
    "rows": results,
}
os.makedirs("./results", exist_ok=True)
with open("./results/rtt-injection.json", "w") as f:
    json.dump(out, f, indent=2)
print("\nwrote results/rtt-injection.json", flush=True)
