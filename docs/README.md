# S4 documentation

Product overview, pitch, and quick start live in the top-level
[README.md](../README.md). This folder holds the detailed reference.

## Use cases
- [use-cases/elasticsearch-frozen-tier.md](use-cases/elasticsearch-frozen-tier.md) — S4 as an Elasticsearch frozen-tier backend: measured storage and snapshot throughput across standard / `best_compression` / LogsDB and zstd levels, plus cold frozen-search latency (at zstd-3)
- [use-cases/opensearch-searchable-snapshots.md](use-cases/opensearch-searchable-snapshots.md) — S4 as an OpenSearch searchable-snapshot backend: −16–28% across `default` / `best_compression` / `zstd` / `zstd_no_dict` index codecs; requires `--logical-etag`

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
