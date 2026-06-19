# Kafka tiered-storage (KIP-405) compression benchmark harness

Reproduces [`docs/use-cases/kafka-tiered-storage.md`](../../docs/use-cases/kafka-tiered-storage.md):
tiered-segment storage and cold remote-fetch latency for Apache Kafka whose
tiered-storage S3 backend is fronted by S4, across producer `compression.type`
{none, snappy, lz4, zstd} × {direct → MinIO, S4 → MinIO}.

Runs locally against MinIO — **no AWS account**. Raw output from the documented
run is in [`results/kafka.json`](results/kafka.json); the `--logical-etag`
negative capture is in [`results/logical_etag_negative.txt`](results/logical_etag_negative.txt).

## Prerequisites
- Docker, `aws` CLI, `python3`, ~1 GB free disk
- A built S4 binary (`cargo build --release -p s4-server` → `target/release/s4`)

## 1. Aiven tiered-storage plugin (~100 MB of jars, not committed)
Kafka tiered storage needs a `RemoteStorageManager`; this harness uses the
open-source Aiven plugin (S3 backend). Download + consolidate the jars into
`plugins/libs/` (gitignored):
```bash
cd plugins
for p in core s3; do
  curl -sSL -O "https://github.com/Aiven-Open/tiered-storage-for-apache-kafka/releases/download/v1.1.1/$p-1.1.1.tgz"
  mkdir -p tiered-storage && tar xzf "$p-1.1.1.tgz" -C tiered-storage
done
mkdir -p libs && find tiered-storage -name '*.jar' -exec cp -n {} libs/ \;
cd ..
```

## 2. Backend + buckets + S4 gateway
```bash
docker run -d --name kafkabench-minio --network host \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio:latest server /data
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1
for b in kafka-direct kafka-s4; do aws --endpoint-url http://localhost:9000 s3 mb s3://$b; done
# S4 zstd-3 gateway on :8011 (the harness expects it here)
../../target/release/s4 --endpoint-url=http://localhost:9000 --host=127.0.0.1 --port=8011 \
    --codec=cpu-zstd --dispatcher=always --zstd-level=3 --logical-etag &
```

## 3. Run the benchmark
`kafka_bench.py` drives everything: generates payloads, and for each endpoint
(direct / S4) wipes state, (re)starts a `apache/kafka:3.9.1` KRaft broker with
the Aiven RSM, creates one tiered topic per producer codec, produces 600k records
each, waits for the rolled segments to tier, measures tiered bytes per topic,
then restarts the broker (empty chunk cache) and times a cold consume-from-earliest.
```bash
python3 kafka_bench.py      # -> results/kafka.json
```

## Cleanup
```bash
docker rm -f kafkabench kafkabench-minio
pkill -f 'target/release/s4 .*--port=801'
rm -rf data kafka-config
```

## Notes / gotchas
- **`s3.path.style.access.enabled=true` is mandatory for MinIO** — without it the
  AWS SDK uses virtual-host addressing (`bucket.localhost`) and tiering fails with
  "The specified bucket is not valid".
- **Plugin compression/encryption are OFF** (`rsm.config.compression.enabled=false`,
  `encryption.enabled=false`) so S4 is the only re-compressor. Enabling the
  plugin's own chunk compression shrinks S4's residual gain toward the zstd row.
- **`--logical-etag` is recommended, not required** for the Aiven plugin (see
  `results/logical_etag_negative.txt`): unlike OpenSearch's repository-s3, it does
  not reject S4's compressed-object ETag, in either `aws.checksum.check.enabled`
  mode. The flag is still recommended for correct ETags.
- Tiered bytes exclude the still-local **active** segment per topic (same for
  every config); the S4 columns include S4 sidecar objects. The per-codec
  saving% is the zstd ratio of the same segment data (verified by a per-segment
  GET-through-S4 vs stored-bytes check).
- Cold remote-fetch is a **single, noisy** sample per config (broker restarted to
  empty the chunk cache; rolled segments local-deleted so the read hits S3). The
  transferable signal is only "no consistent S4 penalty", not the exact ms.
- The harness asserts the full record count was produced (guards against a silent
  partial produce faking a compression ratio), and polls until tiering settles.
