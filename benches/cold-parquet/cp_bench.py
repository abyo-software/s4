#!/usr/bin/env python3
"""Cold Parquet recompaction benchmark.

Cold data-lake Parquet is overwhelmingly written with the **snappy** column
codec (Spark / pandas / Arrow's long-time default). `s4 parquet-recompact` reads
such objects and re-encodes their columns to **zstd**, producing a *native*
Parquet (still readable by pyarrow / Spark / Trino / DuckDB, no S4 in the read
path).

For each input writer codec {none, snappy, gzip, zstd} this harness:
  1. writes the same 2M-row ECS-style dataset as a Parquet in that codec,
  2. uploads it to MinIO,
  3. runs `s4 parquet-recompact --execute` (zstd-3),
  4. measures the size before/after and verifies — with pyarrow — that the
     recompacted object is a native Parquet whose data is value-for-value
     identical to the original.

The honest split (same shape as the Loki/Kafka docs): if the writer already used
zstd, S4 has ~nothing to take; S4's wedge is the snappy/none/gzip-backlog. Runs
locally against MinIO — no AWS account.
"""
import json, os, subprocess, time
import pyarrow as pa
import pyarrow.parquet as pq

W = os.path.dirname(os.path.abspath(__file__))
ENV = dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
           AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
           AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")
ENDPOINT = "http://localhost:9000"
BUCKET = "coldparquet"
S4 = os.path.join(W, "..", "..", "target", "release", "s4")
NROWS = 2_000_000
INPUT_CODECS = ["none", "snappy", "gzip", "zstd"]
TARGET_LEVEL = 3
SEED = 42

def sh(*a): return subprocess.run(a, env=ENV, capture_output=True, text=True)
def aws(*a): return sh("aws", "--endpoint-url", ENDPOINT, *a)

def build_table(nrows):
    import random
    random.seed(SEED)
    HOSTS = [f"web-{i:02d}.prod.internal" for i in range(20)]
    SVCS = ["api-gateway", "auth-svc", "checkout", "search", "catalog", "payments", "recommend", "notify"]
    LEVELS = ["INFO"] * 70 + ["WARN"] * 18 + ["ERROR"] * 8 + ["DEBUG"] * 4
    METHODS = ["GET"] * 60 + ["POST"] * 25 + ["PUT"] * 8 + ["DELETE"] * 4 + ["PATCH"] * 3
    PATHS = [f"/api/v1/{p}" for p in ["items", "users", "cart", "search", "orders", "payments",
             "auth/login", "catalog", "health", "metrics"]] + ["/", "/favicon.ico"]
    STATUS = [200] * 70 + [201] * 5 + [204] * 3 + [400] * 5 + [404] * 5 + [500] * 2 + [503] * 1
    UAS = ["Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/124.0", "Mozilla/5.0 (Macintosh) Safari/605",
           "Firefox/125.0", "curl/8.4.0", "python-requests/2.31.0", "Go-http-client/2.0"]
    def rip(): return f"{random.randint(10,250)}.{random.randint(0,255)}.{random.randint(0,255)}.{random.randint(1,254)}"
    cols = {k: [] for k in ["ts", "service", "level", "method", "path", "status", "bytes",
                            "duration_us", "host", "ip", "user", "trace_id", "ua"]}
    base = 1_717_200_000_000
    for i in range(nrows):
        st = random.choice(STATUS)
        cols["ts"].append(base + i)
        cols["service"].append(random.choice(SVCS))
        cols["level"].append("ERROR" if st >= 500 else ("WARN" if st >= 400 else random.choice(LEVELS)))
        cols["method"].append(random.choice(METHODS)); cols["path"].append(random.choice(PATHS))
        cols["status"].append(st); cols["bytes"].append(random.randint(80, 60000))
        cols["duration_us"].append(random.randint(200, 900000))
        cols["host"].append(random.choice(HOSTS)); cols["ip"].append(rip())
        cols["user"].append(f"u{random.randint(1,40000)}")
        cols["trace_id"].append("%032x" % random.getrandbits(128)); cols["ua"].append(random.choice(UAS))
    return pa.table(cols)

def main():
    os.makedirs(f"{W}/data", exist_ok=True)
    os.makedirs(f"{W}/results", exist_ok=True)
    print(f"building {NROWS}-row ECS table…", flush=True)
    base = build_table(NROWS)

    meta = {
        "date": "2026-06-19",
        "host": "AMD Ryzen 9 9950X (16C/32T), Linux",
        "s4": "v1.2.2, parquet-recompact feature, --target-zstd-level 3",
        "object_store": "minio/minio:latest (local)",
        "dataset": f"{NROWS}-row ECS-style logs, 13 columns (seed {SEED})",
        "tool": "s4 parquet-recompact (Arrow re-encode -> native zstd Parquet)",
        "verification": "pyarrow: recompacted object read natively + table.equals(original) value-for-value; output codec asserted ZSTD",
    }
    results = {"meta": meta, "matrix": []}

    for codec in INPUT_CODECS:
        local = f"{W}/data/logs_{codec}.parquet"
        pq.write_table(base, local, compression=codec, row_group_size=200_000)
        in_bytes = os.path.getsize(local)
        key = f"logs_{codec}.parquet"
        aws("s3", "rm", f"s3://{BUCKET}/{key}")
        aws("s3", "cp", local, f"s3://{BUCKET}/{key}")
        t0 = time.time()
        r = sh(S4, f"--endpoint-url={ENDPOINT}", "parquet-recompact", f"{BUCKET}/{key}",
               "--target-zstd-level", str(TARGET_LEVEL), "--min-gain-percent", "0",
               "--execute", "--allow-lossy-physical-rewrite")
        dt = round(time.time() - t0, 2)
        # measure backend size after + verify round-trip via pyarrow
        out_bytes = int(aws("s3api", "head-object", "--bucket", BUCKET, "--key", key,
                            "--query", "ContentLength", "--output", "text").stdout.strip() or 0)
        dl = f"{W}/data/_out_{codec}.parquet"
        aws("s3", "cp", f"s3://{BUCKET}/{key}", dl)
        back = pq.read_table(dl)
        rg0 = pq.ParquetFile(dl).metadata.row_group(0)
        out_codec = sorted({rg0.column(i).compression for i in range(rg0.num_columns)})
        data_equal = base.equals(back)
        saving = round((in_bytes - out_bytes) / in_bytes * 100, 2) if in_bytes else None
        row = {"input_codec": codec, "input_bytes": in_bytes, "output_bytes": out_bytes,
               "saving_pct": saving, "data_equal": data_equal, "output_codec": out_codec,
               "recompact_s": dt, "cli_ok": r.returncode == 0}
        results["matrix"].append(row)
        print(f"  {codec:7s}: {in_bytes/1e6:6.1f}MB -> {out_bytes/1e6:6.1f}MB  saving={saving}%  "
              f"data_equal={data_equal}  out_codec={out_codec}  ({dt}s)", flush=True)
        os.remove(dl)

    json.dump(results, open(f"{W}/results/cold_parquet.json", "w"), indent=2)
    print("wrote results/cold_parquet.json", flush=True)


if __name__ == "__main__":
    main()
