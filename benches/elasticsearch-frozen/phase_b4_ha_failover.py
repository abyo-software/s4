#!/usr/bin/env python3
"""Phase B4: HA failover smoke — 2 stateless S4 instances behind a load balancer.

S4 instances are stateless: the .s4index sidecars live in the object store, so
any instance can serve any request. This phase puts two identical S4 zstd-3
instances behind an nginx upstream, registers an ES snapshot repository through
the LB's address, then kills one instance and checks that:
  - a cold frozen query still succeeds (served by the survivor),
  - a warm query is unaffected,
  - a snapshot PUT issued while one instance is down still completes.

This is a smoke test of the A2 "read-path hard dependency -> mitigate with
multiple stateless instances behind multi-value DNS / a load balancer" claim,
NOT a throughput or fault-injection matrix.

Driven by env (the runner phase_b4.sh wires the defaults):
  ES_URL       default http://localhost:9200
  LB_CLIENT    default hals3      (ES s3 client pointing at the nginx LB)
  LB_PORT      default 8030       (host port nginx listens on)
  S4_PORTS     default 8027,8028  (the two backing S4 instances)
  BUCKET       default esrev-s4z3
  INDEX        default bench-standard-default
"""
import json, os, time, subprocess, urllib.request, urllib.error

ES = os.environ.get("ES_URL", "http://localhost:9200")
LB_CLIENT = os.environ.get("LB_CLIENT", "hals3")
LB_PORT = int(os.environ.get("LB_PORT", "8030"))
S4_PORTS = [int(p) for p in os.environ.get("S4_PORTS", "8027,8028").split(",")]
BUCKET = os.environ.get("BUCKET", "esrev-s4z3")
INDEX = os.environ.get("INDEX", "bench-standard-default")
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
        return {"_error": e.code, "_body": e.read().decode()[:400]}


def aws(*args):
    return subprocess.run(["aws", "--endpoint-url", MINIO, *args], env=ENV,
                          capture_output=True, text=True)


def clear_cache():
    es("POST", "/_searchable_snapshots/cache/clear")


def port_pid(port):
    # find the s4 process listening on this port (best effort)
    r = subprocess.run(["bash", "-c", f"ss -ltnp 2>/dev/null | grep ':{port} '"],
                       capture_output=True, text=True)
    import re
    m = re.search(r"pid=(\d+)", r.stdout)
    return int(m.group(1)) if m else None


QUERY_COUNT = {"size": 0, "track_total_hits": True,
               "query": {"term": {"http.response.status_code": 500}}}
QUERY_TOPN = {"size": 20, "query": {"term": {"log.level": "ERROR"}},
              "sort": [{"@timestamp": "desc"}]}

steps = []


def record(name, ok, detail):
    steps.append({"step": name, "ok": bool(ok), "detail": detail})
    print(f"[{'PASS' if ok else 'FAIL'}] {name}: {detail}", flush=True)


# 1) register repo through the LB + verify
repo = "ha_repo"
es("DELETE", f"/_snapshot/{repo}")
aws("s3", "rm", f"s3://{BUCKET}", "--recursive")
es("PUT", f"/_snapshot/{repo}", {"type": "s3", "settings": {
    "bucket": BUCKET, "client": LB_CLIENT,
    "max_snapshot_bytes_per_sec": "-1", "max_restore_bytes_per_sec": "-1"}})
v = es("POST", f"/_snapshot/{repo}/_verify")
record("register_repo_via_LB+verify", "nodes" in v, "verify returned nodes" if "nodes" in v else str(v)[:200])

# 2) snapshot through the LB (both instances up)
snap1 = f"ha-{INDEX}-pre"
r = es("PUT", f"/_snapshot/{repo}/{snap1}?wait_for_completion=true",
       {"indices": INDEX, "include_global_state": False})
state = (r.get("snapshot") or {}).get("state")
record("snapshot_both_up", state == "SUCCESS", f"snapshot state={state}")

# 3) frozen-mount + warm one query (populate cache)
frozen = f"haxfrozenx{INDEX}".lower().replace("-", "x")
es("DELETE", f"/{frozen}")
m = es("POST", f"/_snapshot/{repo}/{snap1}/_mount?wait_for_completion=true&storage=shared_cache",
       {"index": INDEX, "renamed_index": frozen})
mounted = "_error" not in m
for _ in range(30):
    h = es("GET", f"/_cluster/health/{frozen}?wait_for_status=yellow&timeout=5s")
    if h.get("status") in ("yellow", "green"):
        break
    time.sleep(1)
# warm it
es("GET", f"/{frozen}/_search", QUERY_TOPN)
warm_hot = es("GET", f"/{frozen}/_search", QUERY_TOPN)
record("frozen_mount+warm", mounted and "_error" not in warm_hot,
       f"warm took={warm_hot.get('took')}ms" if mounted else str(m)[:200])

# 4) KILL one S4 instance
victim_port = S4_PORTS[0]
victim_pid = port_pid(victim_port)
killed = False
if victim_pid:
    subprocess.run(["kill", "-9", str(victim_pid)])
    time.sleep(2)
    killed = port_pid(victim_port) is None
record("kill_one_s4_instance", killed,
       f"killed pid={victim_pid} on :{victim_port}; survivor :{S4_PORTS[1]}")

# 5) WARM query unaffected (answered from local cache, no backend needed)
warm_after = es("GET", f"/{frozen}/_search", QUERY_TOPN)
record("warm_query_after_kill", "_error" not in warm_after,
       f"warm took={warm_after.get('took')}ms (served from shared cache)")

# 6) COLD query succeeds (must hit the survivor through the LB)
clear_cache(); time.sleep(0.5)
cold_after = es("GET", f"/{frozen}/_search", QUERY_COUNT)
cold_ok = "_error" not in cold_after and cold_after.get("hits", {}).get("total") is not None
record("cold_query_after_kill", cold_ok,
       f"cold took={cold_after.get('took')}ms hits={cold_after.get('hits',{}).get('total')}"
       if cold_ok else str(cold_after)[:200])

# 7) snapshot PUT while one instance down (LB routes to survivor)
snap2 = f"ha-{INDEX}-during-kill"
r2 = es("PUT", f"/_snapshot/{repo}/{snap2}?wait_for_completion=true",
        {"indices": INDEX, "include_global_state": False})
state2 = (r2.get("snapshot") or {}).get("state")
record("snapshot_PUT_during_kill", state2 == "SUCCESS", f"snapshot state={state2}")

# cleanup the frozen index (leave instances/LB for the runner to tear down)
es("DELETE", f"/{frozen}")
clear_cache()

all_ok = all(s["ok"] for s in steps)
out = {
    "measurement": "B4 - HA failover smoke (2 stateless S4 instances behind nginx LB)",
    "topology": (f"ES -> nginx:{LB_PORT} (round-robin upstream) -> S4 :{S4_PORTS[0]} + :{S4_PORTS[1]} "
                 f"-> MinIO. Both S4 instances stateless (sidecars in S3)."),
    "host": "AMD Ryzen 9 9950X, ES 9.4.2, MinIO RELEASE.2025-09-07, S4 v1.2.2",
    "overall_pass": all_ok,
    "steps": steps,
    "interpretation": ("S4 is a read-path hard dependency for cold frozen search; a single "
                       "gateway is a SPOF. With >=2 stateless instances behind a load balancer "
                       "(or multi-value DNS), losing one instance leaves cold search, warm "
                       "search and snapshot PUT all working through the survivor."),
}
os.makedirs("./results", exist_ok=True)
with open("./results/ha-failover.json", "w") as f:
    json.dump(out, f, indent=2)
print(f"\noverall_pass={all_ok} -> wrote results/ha-failover.json", flush=True)
