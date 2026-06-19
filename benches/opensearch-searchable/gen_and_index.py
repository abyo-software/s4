#!/usr/bin/env python3
"""Generate realistic ECS-style structured logs and bulk-index into ES.

Usage: gen_and_index.py <index> <ndocs> [seed]
Indexes directly via the _bulk API in streaming batches (low memory).
"""
import sys, json, random, time, urllib.request

ES = "http://localhost:9200"
INDEX = sys.argv[1]
NDOCS = int(sys.argv[2])
SEED = int(sys.argv[3]) if len(sys.argv) > 3 else 42
BATCH = 10000

random.seed(SEED)

HOSTS = [f"web-{i:02d}.prod.internal" for i in range(20)]
SERVICES = ["api-gateway", "auth-svc", "checkout", "search", "catalog", "payments", "recommend", "notify"]
LEVELS = ["INFO"]*70 + ["WARN"]*18 + ["ERROR"]*8 + ["DEBUG"]*4
METHODS = ["GET"]*60 + ["POST"]*25 + ["PUT"]*8 + ["DELETE"]*4 + ["PATCH"]*3
PATHS = [f"/api/v1/{p}" for p in
         ["items","items/{id}","users","users/{id}","cart","cart/checkout","search","orders",
          "orders/{id}","payments","auth/login","auth/refresh","catalog","catalog/{id}","health",
          "metrics","recommend","notify","profile","settings"]] + ["/", "/favicon.ico", "/static/app.js"]
STATUS = [200]*70 + [201]*5 + [204]*3 + [301]*2 + [302]*3 + [400]*5 + [401]*3 + [404]*5 + [429]*1 + [500]*2 + [503]*1
UAS = [
 "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
 "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15",
 "Mozilla/5.0 (X11; Linux x86_64; rv:125.0) Gecko/20100101 Firefox/125.0",
 "Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1",
 "curl/8.4.0", "python-requests/2.31.0", "Go-http-client/2.0", "kube-probe/1.29",
 "ELB-HealthChecker/2.0", "Datadog Agent/7.52",
]

def rip():
    return f"{random.randint(10,250)}.{random.randint(0,255)}.{random.randint(0,255)}.{random.randint(1,254)}"

def post_bulk(payload):
    req = urllib.request.Request(f"{ES}/{INDEX}/_bulk", data=payload.encode(),
                                 headers={"Content-Type": "application/x-ndjson"}, method="POST")
    with urllib.request.urlopen(req) as r:
        resp = json.load(r)
    if resp.get("errors"):
        for it in resp["items"]:
            if it["index"].get("status", 200) >= 300:
                raise RuntimeError(json.dumps(it["index"]["error"]))

t0 = time.time()
base_ts = 1_717_200_000_000  # 2024-06-01, ms
buf = []
done = 0
action = json.dumps({"index": {}})
for i in range(NDOCS):
    host = random.choice(HOSTS)
    svc = random.choice(SERVICES)
    method = random.choice(METHODS)
    path = random.choice(PATHS)
    status = random.choice(STATUS)
    level = "ERROR" if status >= 500 else ("WARN" if status >= 400 else random.choice(LEVELS))
    bytes_out = random.randint(80, 60000)
    dur_us = random.randint(200, 900000)
    ts = base_ts + i * 5 + random.randint(0, 4)  # ~ascending, ms
    ip = rip()
    user = f"u{random.randint(1, 40000)}"
    trace = "%032x" % random.getrandbits(128)
    span = "%016x" % random.getrandbits(64)
    msg = f"{method} {path} {status} {bytes_out}b {dur_us}us host={host} svc={svc} user={user}"
    doc = {
        "@timestamp": ts,
        "host": {"name": host},
        "service": {"name": svc},
        "log": {"level": level},
        "http": {"request": {"method": method},
                 "response": {"status_code": status, "body": {"bytes": bytes_out}}},
        "url": {"path": path},
        "event": {"duration": dur_us * 1000},
        "source": {"ip": ip},
        "user": {"name": user},
        "user_agent": {"original": random.choice(UAS)},
        "trace": {"id": trace},
        "span": {"id": span},
        "message": msg,
    }
    buf.append(action)
    buf.append(json.dumps(doc, separators=(",", ":")))
    if len(buf) >= BATCH * 2:
        post_bulk("\n".join(buf) + "\n")
        done += BATCH
        buf.clear()
        if done % 500000 == 0:
            rate = done / (time.time() - t0)
            print(f"  {INDEX}: {done}/{NDOCS} ({rate:.0f} docs/s)", flush=True)
if buf:
    post_bulk("\n".join(buf) + "\n")
    done += len(buf)//2
# refresh
urllib.request.urlopen(urllib.request.Request(f"{ES}/{INDEX}/_refresh", method="POST")).read()
print(f"DONE {INDEX}: {done} docs in {time.time()-t0:.1f}s", flush=True)
