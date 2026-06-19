#!/usr/bin/env python3
"""Generate realistic ECS-style structured log records for the Kafka tiered-storage
benchmark, one per line, for kafka-producer-perf-test.sh --payload-file.

Same shape as the Grafana Loki bench: low-cardinality service/level plus
high-cardinality logfmt fields (path, status, bytes, ip, user, trace id, ua).
This is what actually compresses — Kafka's producer batches these, the segments
tier to S3, and S4 re-compresses the segment bytes.

Usage: gen_payloads.py <nlines> [seed] > payloads.txt
"""
import sys, random

NLINES = int(sys.argv[1]) if len(sys.argv) > 1 else 50000
SEED = int(sys.argv[2]) if len(sys.argv) > 2 else 42
random.seed(SEED)

HOSTS = [f"web-{i:02d}.prod.internal" for i in range(20)]
SERVICES = ["api-gateway", "auth-svc", "checkout", "search", "catalog", "payments", "recommend", "notify"]
LEVELS = ["INFO"] * 70 + ["WARN"] * 18 + ["ERROR"] * 8 + ["DEBUG"] * 4
METHODS = ["GET"] * 60 + ["POST"] * 25 + ["PUT"] * 8 + ["DELETE"] * 4 + ["PATCH"] * 3
PATHS = [f"/api/v1/{p}" for p in
         ["items", "items/{id}", "users", "users/{id}", "cart", "cart/checkout", "search", "orders",
          "orders/{id}", "payments", "auth/login", "auth/refresh", "catalog", "catalog/{id}", "health",
          "metrics", "recommend", "notify", "profile", "settings"]] + ["/", "/favicon.ico", "/static/app.js"]
STATUS = [200] * 70 + [201] * 5 + [204] * 3 + [301] * 2 + [302] * 3 + [400] * 5 + [401] * 3 + [404] * 5 + [429] * 1 + [500] * 2 + [503] * 1
UAS = [
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/124.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Version/17.4 Safari/605.1.15",
    "Mozilla/5.0 (X11; Linux x86_64; rv:125.0) Firefox/125.0",
    "curl/8.4.0", "python-requests/2.31.0", "Go-http-client/2.0", "kube-probe/1.29",
]

def rip():
    return f"{random.randint(10,250)}.{random.randint(0,255)}.{random.randint(0,255)}.{random.randint(1,254)}"

out = sys.stdout
for _ in range(NLINES):
    svc = random.choice(SERVICES); host = random.choice(HOSTS)
    method = random.choice(METHODS); path = random.choice(PATHS); status = random.choice(STATUS)
    level = "ERROR" if status >= 500 else ("WARN" if status >= 400 else random.choice(LEVELS))
    nbytes = random.randint(80, 60000); dur_us = random.randint(200, 900000)
    ip = rip(); user = f"u{random.randint(1,40000)}"
    trace = "%032x" % random.getrandbits(128); span = "%016x" % random.getrandbits(64)
    ua = random.choice(UAS)
    out.write(f'service={svc} level={level} method={method} path={path} status={status} '
              f'bytes={nbytes} duration_us={dur_us} host={host} ip={ip} user={user} '
              f'trace_id={trace} span_id={span} ua="{ua}"\n')
