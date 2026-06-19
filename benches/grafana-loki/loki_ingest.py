#!/usr/bin/env python3
"""Push N ECS-style structured log lines into Loki via /loki/api/v1/push.

Labels are low-cardinality (service, level) per Loki best practice; the
high-cardinality fields (path, status, bytes, ip, user, trace id, ...) live in
the log *line* (logfmt) — the realistic shape, and what S4 actually compresses.

Usage: loki_ingest.py <ndocs> [seed]
"""
import sys, json, random, time, urllib.request
from collections import defaultdict

LOKI = "http://localhost:3100"
NDOCS = int(sys.argv[1])
SEED = int(sys.argv[2]) if len(sys.argv) > 2 else 42
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
 "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/124.0 Safari/537.36",
 "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Version/17.4 Safari/605.1.15",
 "Mozilla/5.0 (X11; Linux x86_64; rv:125.0) Firefox/125.0",
 "curl/8.4.0", "python-requests/2.31.0", "Go-http-client/2.0", "kube-probe/1.29",
]

def rip():
    return f"{random.randint(10,250)}.{random.randint(0,255)}.{random.randint(0,255)}.{random.randint(1,254)}"

def post(streams):
    body = json.dumps({"streams": streams}).encode()
    req = urllib.request.Request(f"{LOKI}/loki/api/v1/push", data=body,
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req) as r:
        if r.status not in (204, 200):
            raise RuntimeError(f"push status {r.status}")

t0 = time.time()
base_ns = 1_717_200_000_000_000_000  # 2024-06-01 in ns
done = 0
buf = defaultdict(list)  # (service, level) -> [[ts_ns, line], ...]
buf_n = 0
for i in range(NDOCS):
    host = random.choice(HOSTS); svc = random.choice(SERVICES)
    method = random.choice(METHODS); path = random.choice(PATHS); status = random.choice(STATUS)
    level = "ERROR" if status >= 500 else ("WARN" if status >= 400 else random.choice(LEVELS))
    nbytes = random.randint(80, 60000); dur_us = random.randint(200, 900000)
    ip = rip(); user = f"u{random.randint(1,40000)}"
    trace = "%032x" % random.getrandbits(128); span = "%016x" % random.getrandbits(64)
    ua = random.choice(UAS)
    ts = base_ns + i * 1_000_000  # 1ms apart, ascending
    line = (f'method={method} path={path} status={status} bytes={nbytes} '
            f'duration_us={dur_us} host={host} ip={ip} user={user} '
            f'trace_id={trace} span_id={span} ua="{ua}"')
    buf[(svc, level)].append([str(ts), line])
    buf_n += 1
    if buf_n >= BATCH:
        streams = [{"stream": {"service": s, "level": lv}, "values": v} for (s, lv), v in buf.items()]
        post(streams); done += buf_n; buf.clear(); buf_n = 0
        if done % 500000 == 0:
            print(f"  {done}/{NDOCS} ({done/(time.time()-t0):.0f} lines/s)", flush=True)
if buf_n:
    streams = [{"stream": {"service": s, "level": lv}, "values": v} for (s, lv), v in buf.items()]
    post(streams); done += buf_n
print(f"DONE: {done} lines in {time.time()-t0:.1f}s", flush=True)
