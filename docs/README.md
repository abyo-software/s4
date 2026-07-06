# S4 documentation

Product overview, pitch, and quick start live in the top-level
[README.md](../README.md). This folder holds the detailed reference.

## Use cases
- [use-cases/elasticsearch-frozen-tier.md](use-cases/elasticsearch-frozen-tier.md) — S4 as an Elasticsearch frozen-tier backend: measured storage and snapshot throughput across standard / `best_compression` / LogsDB and zstd levels, plus cold frozen-search latency (at zstd-3)
- [use-cases/opensearch-searchable-snapshots.md](use-cases/opensearch-searchable-snapshots.md) — S4 as an OpenSearch searchable-snapshot backend: −16–28% across `default` / `best_compression` / `zstd` / `zstd_no_dict` index codecs; requires `--logical-etag`
- [use-cases/grafana-loki-chunks.md](use-cases/grafana-loki-chunks.md) — S4 in front of Grafana Loki chunk storage: −18.4% on the immutable snappy backlog, with an honest split vs Loki-native `zstd` (−38% for new chunks) and a measured ~1.7 ms read overhead per chunk fetch
- [use-cases/kafka-tiered-storage.md](use-cases/kafka-tiered-storage.md) — S4 in front of Kafka tiered storage (KIP-405): −74.7% on uncompressed (`none`) tiered segments / ~20% over snappy/lz4 / ~0% over producer-`zstd`, with an honest split vs producer-side compression; no consistent cold-fetch penalty; works without `--logical-etag`
- [use-cases/cold-parquet.md](use-cases/cold-parquet.md) — `s4 parquet-recompact` rewrites cold lake Parquet to native zstd: −36.6% over snappy / −51.7% over uncompressed, value-verified, no S4 in the read path
- [use-cases/s3-compatible-backends.md](use-cases/s3-compatible-backends.md) — S4 in front of S3-compatible stores: MinIO (the CI-verified backend, with the series' measured results), plus Cloudflare R2 / Backblaze B2 / Wasabi pricing math and an honest not-yet-validated checklist

## Getting started
- [install.md](install.md) — cargo / pip / WASM / build from source / supported targets
- [gpu.md](gpu.md) — GPU trial + `--gpu-batch-small-puts` tuning
- [deployment.md](deployment.md) — Kubernetes / Helm
- [configuration.md](configuration.md) — CLI flags, HTTPS/ACME, bucket-policy enforcement

## Cost & operations
- [savings.md](savings.md) — `s4 estimate` (pre-deployment) + `s4 savings` (measured)
- [ops/maintenance.md](ops/maintenance.md) — `s4 migrate` / `s4 recompact` / `s4 maintain`
- [ops/dictionaries.md](ops/dictionaries.md) — `s4 train-dict` + `--zstd-dict`
- [ops/repair.md](ops/repair.md) — durability, corruption recovery, repair tool
- [ops/runbook.md](ops/runbook.md) — operational runbook
- [observability.md](observability.md) — metrics / logs / tracing
- [storage-class-transitions.md](storage-class-transitions.md) — Standard ↔ IA ↔ Glacier
- [orphan-sidecar-recovery.md](orphan-sidecar-recovery.md) — `.s4index` cleanup recipe

## Reference
- [compatibility.md](compatibility.md) — S3 API / SDK / backend matrices
- [architecture.md](architecture.md) — data flow + streaming I/O
- [wire-format.md](wire-format.md) — S4F2 frame + S4IX sidecar
- [features.md](features.md) — SSE Range GET, observability, data integrity, storage class

## Proof & trust
- [trust.md](trust.md) — why trust S4 with your data: the escape hatch, byte-integrity design, verification tooling, and the testing evidence in one page
- [benchmarks.md](benchmarks.md) — full codec / throughput / SSE tables + reproduction
- [testing.md](testing.md) — test & validation matrix
- [stability.md](stability.md) — v1.x SemVer freeze contract
- [status.md](status.md) — project status, audit history, fuzz evidence
- [security/threat-model.md](security/threat-model.md)
- [security/overview.md](security/overview.md)
- [security/sse-partial-fetch-constraint.md](security/sse-partial-fetch-constraint.md)
- [security/streaming-checksum-coverage.md](security/streaming-checksum-coverage.md)
- [security/cargo-audit-ignores.md](security/cargo-audit-ignores.md)

## AWS Marketplace
- [marketplace/metering.md](marketplace/metering.md) — `RegisterUsage` opt-in metering
- [marketplace/listing.md](marketplace/listing.md) — listing source-of-truth
