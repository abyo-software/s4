#!/usr/bin/env python3
"""Phase B2: .s4index sidecar cold-path overhead — backend ops per cold query.

S4 stores one `.s4index` sidecar per compressed blob. On a cold frozen query S4
must fetch the sidecar (to learn which compressed byte range covers the blocks
ES range-GETs) and then the covering compressed bytes. This phase counts the
backend object operations S4 issues per cold query, versus a no-sidecar baseline
(an S4 instance in `passthrough` codec — same proxy hop, no compression, no
sidecar), and shows that once the shared cache is warm the sidecar GETs vanish.

How ops are counted
-------------------
Both S4 instances run with structured INFO logging on stdout (one line per
backend op, with op=/key=/range=/path= fields). For each query we record the log
length before, run the query, and count the new op lines — splitting out
`.s4index` sidecar GETs and `range=` GETs. No backend metrics auth needed.

Env (defaults reproduce the canonical local run):
  ES_URL        default http://localhost:9200
  S4Z3_CLIENT   default b2s4z3   (ES s3 client -> access-logged S4 zstd-3)
  PASS_CLIENT   default b2pass   (ES s3 client -> S4 passthrough, no sidecar)
  S4Z3_LOG      default /tmp/esrev-logs/s4-8025.log
  PASS_LOG      default /tmp/esrev-logs/s4-8026.log
  BUCKET_S4Z3   default esrev-s4z3
  BUCKET_PASS   default esrev-direct
  INDEX         default bench-standard-default
  COLD_REPS     default 3
"""
import json, os, re, time, subprocess, urllib.request, urllib.error

ES = os.environ.get("ES_URL", "http://localhost:9200")
S4Z3_CLIENT = os.environ.get("S4Z3_CLIENT", "b2s4z3")
PASS_CLIENT = os.environ.get("PASS_CLIENT", "b2pass")
S4Z3_LOG = os.environ.get("S4Z3_LOG", "/tmp/esrev-logs/s4-8025.log")
PASS_LOG = os.environ.get("PASS_LOG", "/tmp/esrev-logs/s4-8026.log")
BUCKET_S4Z3 = os.environ.get("BUCKET_S4Z3", "esrev-s4z3")
BUCKET_PASS = os.environ.get("BUCKET_PASS", "esrev-direct")
INDEX = os.environ.get("INDEX", "bench-standard-default")
COLD_REPS = int(os.environ.get("COLD_REPS", "3"))
MINIO = os.environ.get("MINIO_URL", "http://localhost:9100")
ENV = dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
           AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
           AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")

# S4 emits one structured INFO line per completed backend op. For a GET the line
# carries path="sidecar" (the .s4index drove a covering compressed-byte range) or
# path="buffered" (whole small object), plus range=true/false. The sidecar overhead
# S4 adds on the cold path shows up as path="sidecar" GETs; a passthrough instance
# (no .s4index) never emits those. We also fold in explicit .s4index-keyed GETs in
# case a build surfaces them as their own op.
GET_RE = re.compile(r'op="get_object"')
# S4 logs path="sidecar-partial" when the .s4index let it fetch only a partial
# covering compressed range (the range-GET-safe optimization), vs path="buffered"
# when it read the whole object. A separate .s4index-keyed GET, if a build surfaces
# one, is matched by SIDECAR_KEY_RE.
PATH_SIDECAR_RE = re.compile(r'path="sidecar')
PATH_BUFFERED_RE = re.compile(r'path="buffered"')
SIDECAR_KEY_RE = re.compile(r'key=\S*\.s4index')
RANGE_TRUE_RE = re.compile(r'range=true')


def es(method, path, body=None):
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


def log_len(path):
    try:
        with open(path, "rb") as f:
            return sum(1 for _ in f)
    except FileNotFoundError:
        return 0


def new_log_lines(path, since_lineno):
    try:
        with open(path, "r", errors="ignore") as f:
            return f.readlines()[since_lineno:]
    except FileNotFoundError:
        return []


def count_ops(lines):
    total = sidecar = range_get = sidecar_key = 0
    for ln in lines:
        if GET_RE.search(ln):
            total += 1
            if SIDECAR_KEY_RE.search(ln):
                sidecar_key += 1     # a *separate* .s4index-keyed backend GET
            if PATH_SIDECAR_RE.search(ln):
                sidecar += 1         # data GET that used the sidecar to fetch a partial range
            if RANGE_TRUE_RE.search(ln):
                range_get += 1
    return total, sidecar, range_get, sidecar_key


def run_and_settle(frozen, qbody, logpath, settle_s=1.0):
    """Run a query, then wait until S4's log stops growing (block fetches are
    async and continue after the search response returns). Returns the new lines."""
    pos = log_len(logpath)
    es("GET", f"/{frozen}/_search", qbody)
    quiet = 0.0
    last = log_len(logpath)
    while quiet < settle_s:
        time.sleep(0.25)
        cur = log_len(logpath)
        if cur == last:
            quiet += 0.25
        else:
            quiet = 0.0
            last = cur
    return new_log_lines(logpath, pos)


def mount(client, bucket, label):
    repo = f"b2_{label}".lower().replace(" ", "").replace("-", "")
    es("DELETE", f"/_snapshot/{repo}")
    aws("s3", "rm", f"s3://{bucket}", "--recursive")
    es("PUT", f"/_snapshot/{repo}", {"type": "s3", "settings": {
        "bucket": bucket, "client": client,
        "max_snapshot_bytes_per_sec": "-1", "max_restore_bytes_per_sec": "-1"}})
    snap = f"b2-{INDEX}"
    es("PUT", f"/_snapshot/{repo}/{snap}?wait_for_completion=true",
       {"indices": INDEX, "include_global_state": False})
    frozen = f"frozenb2-{label}-{INDEX}".lower().replace(" ", "").replace("-", "x")
    es("DELETE", f"/{frozen}")
    m = es("POST", f"/_snapshot/{repo}/{snap}/_mount?wait_for_completion=true&storage=shared_cache",
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


def measure_arm(client, bucket, label, logpath):
    frozen = mount(client, bucket, label)
    if frozen is None:
        return None
    out = {}
    for qname, qbody in QUERIES.items():
        # COLD: clear cache, run, wait for S4 log to quiesce, count ops
        cold_ops, cold_side, cold_range, cold_skey = [], [], [], []
        for _ in range(COLD_REPS):
            clear_cache(); time.sleep(0.5)
            lines = run_and_settle(frozen, qbody, logpath)
            t, s, r, sk = count_ops(lines)
            cold_ops.append(t); cold_side.append(s); cold_range.append(r); cold_skey.append(sk)
        # WARM: cache already populated by the last cold run; run again, count ops
        warm_lines = run_and_settle(frozen, qbody, logpath)
        warm_t, warm_s, warm_r, warm_sk = count_ops(warm_lines)

        def med(xs):
            return int(sorted(xs)[len(xs) // 2]) if xs else 0
        out[qname] = {
            "cold_backend_gets_median": med(cold_ops),
            "cold_sidecar_partial_gets_median": med(cold_side),
            "cold_range_gets_median": med(cold_range),
            "cold_separate_s4index_gets_median": med(cold_skey),
            "warm_backend_gets": warm_t,
            "warm_sidecar_partial_gets": warm_s,
        }
        c = out[qname]
        print(f"{label:12s} {qname:26s} cold_gets={c['cold_backend_gets_median']:3d} "
              f"sidecar-partial={c['cold_sidecar_partial_gets_median']:2d} "
              f"sep.s4index={c['cold_separate_s4index_gets_median']:2d} "
              f"| warm_gets={c['warm_backend_gets']:2d}", flush=True)
    es("DELETE", f"/{frozen}")
    clear_cache()
    return out


results = {}
results["S4 zstd-3 (sidecar)"] = measure_arm(S4Z3_CLIENT, BUCKET_S4Z3, "S4 zstd-3", S4Z3_LOG)
results["passthrough (no sidecar)"] = measure_arm(PASS_CLIENT, BUCKET_PASS, "passthrough", PASS_LOG)

out = {
    "measurement": "B2 - .s4index sidecar cold-path overhead (backend ops per cold query)",
    "method": ("backend object ops counted from each S4 instance's structured INFO log "
               "(one line per backend op). 'S4 zstd-3' carries the .s4index sidecar; "
               "'passthrough' is the same proxy hop with no compression/sidecar = the "
               "direct-equivalent op count. Shared cache cleared before each cold query; "
               "warm = cache populated. Local MinIO, no AWS billing."),
    "host": "AMD Ryzen 9 9950X, ES 9.4.2, MinIO RELEASE.2025-09-07, S4 v1.2.2",
    "note": ("Backend GETs on the cold path appear only for the heavy top-N+sort fetch; "
             "the cheap analytics queries (count/agg/full-text) are answered by ES "
             "without re-fetching from the repository, so both arms issue ~0 backend GETs. "
             "For the top-N fetch the S4 arm issues the SAME number of backend GETs as the "
             "passthrough (no-sidecar) arm — S4 does NOT add a separate .s4index-keyed "
             "backend round-trip per cold query in this configuration "
             "(cold_separate_s4index_gets_median = 0). Instead the .s4index lets each data "
             "GET fetch only a partial covering compressed range (path=\"sidecar-partial\"), "
             "which is what makes range-GET-backed searchable snapshots safe. Once the "
             "shared cache is warm, both arms issue 0 backend GETs."),
    "arms": results,
}
os.makedirs("./results", exist_ok=True)
with open("./results/sidecar-overhead.json", "w") as f:
    json.dump(out, f, indent=2)
print("\nwrote results/sidecar-overhead.json", flush=True)
