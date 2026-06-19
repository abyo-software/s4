# Grafana Loki chunk-compression benchmark harness

Reproduces [`docs/use-cases/grafana-loki-chunks.md`](../../docs/use-cases/grafana-loki-chunks.md):
chunk storage and whole-chunk GET read latency for Grafana Loki whose S3 chunk
store is fronted by S4, across `direct` (snappy → MinIO), `S4 zstd-3/9` (snappy
chunks re-compressed by S4), and `Loki-native zstd` (no S4).

Runs locally against MinIO — **no AWS account**. Raw output from the documented
run is in [`results/loki.json`](results/loki.json).

## Prerequisites
- Docker, `aws` CLI, `python3`
- A built S4 binary (`cargo build --release -p s4-server` → `target/release/s4`)

## 1. Backend + buckets
```bash
docker run -d --name lokibench-minio -p 9000:9000 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio:latest server /data
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1
for b in loki-direct loki-s4z3 loki-s4z9 loki-native; do
  aws --endpoint-url http://localhost:9000 s3 mb s3://$b; done
```

## 2. S4 gateways — `--logical-etag` recommended (not required by Loki)
Unlike OpenSearch's `repository-s3` (which rejects every blob without it), Loki
3.3.2 uploads chunks fine through an S4 gateway **without** `--logical-etag` — its
S3 client does not reject the ETag mismatch (see
[`results/logical_etag_negative.txt`](results/logical_etag_negative.txt) for the
captured probe). The flag is still recommended: without it S4's PUT returns the
compressed-object ETag (≠ MD5 of the original) and HEAD returns no ETag, so any
client/tool that *does* validate the ETag would break.
```bash
S4=../../target/release/s4
$S4 --endpoint-url=http://localhost:9000 --host=127.0.0.1 --port=8011 \
    --codec=cpu-zstd --dispatcher=always --zstd-level=3 --logical-etag &
$S4 --endpoint-url=http://localhost:9000 --host=127.0.0.1 --port=8012 \
    --codec=cpu-zstd --dispatcher=always --zstd-level=9 --logical-etag &
```

## 3. Run the benchmark
`loki_bench.py` drives everything: for each config it rewrites `loki.yaml`,
(re)starts a `grafana/loki:3.3.2` single-binary container (`--network host`),
ingests 4M lines via `loki_ingest.py`, flushes and waits for the store to settle
(that whole push+flush span is the timed `ingest_flush_s`), then measures bucket
bytes. After all configs, it measures **read latency** by sampling 40 chunk keys
present in **both** the `loki-direct` and `loki-s4z3` buckets and timing a
whole-chunk GET of each — raw snappy from MinIO vs the same chunk through S4
(zstd→snappy) — asserting the S4 bytes are identical to direct (`chunk_get_latency()`).
```bash
python3 loki_bench.py     # -> results/loki.json
```

## Cleanup
```bash
docker rm -f lokibench-loki lokibench-minio
pkill -f 'target/release/s4 .*--port=801'
```

## Notes / gotchas
- **`--logical-etag` recommended, not required by Loki** (see §2).
- `chunk_encoding` is set explicitly per Loki config (`ingester.chunk_encoding`):
  `snappy` is what this benchmark uses; `zstd` is the native alternative the doc
  compares against.
- `chunk_encoding` changes are **forward-only** — existing chunks keep their
  encoding, which is exactly the backlog S4 targets.
- `reject_old_samples: false` is set so the 2024-06-stamped synthetic dataset
  ingests; Loki labels are low-cardinality (`service`, `level`), the rest is in
  the logfmt line.
- Read latency is measured with presigned whole-chunk GETs of 40 real chunk
  objects (direct snappy vs the same chunk through S4); Loki's query engine and
  TSDB index lookup are **not** exercised. Absolute ms are no-RTT local values —
  the transferable metric is the direct-vs-S4 delta (the decompress overhead).
  (An earlier version timed Loki's query API after a restart, but Loki's index
  shipper had not uploaded the index, so those queries hit an empty index and
  measured nothing — dropped in favour of the GET probe.)
- `ingest_flush_s` is the wall-clock for the push + `POST /flush` + flush-to-store
  settle, so S4's compression PUT cost is inside the timed region (it is **not**
  push-only). The settle poll waits per-config until the bucket stops growing,
  which adds a few seconds of run-to-run noise.
- Compactor/retention (`LIST`+`DELETE`) is S4-passthrough but **not exercised**
  by this harness — verify before enabling retention against an S4 store.
