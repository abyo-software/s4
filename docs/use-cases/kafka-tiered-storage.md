# Use case: S4 in front of Kafka tiered storage

> **Series:** S4 use cases · **#4 — Kafka tiered storage (KIP-405)**
> **Status:** measured locally. A single-binary KRaft **Kafka 3.9.1** broker
> tiers log segments to S3 through the Aiven tiered-storage plugin. For the S4
> endpoint the write path (produce → roll → tier) and the read path (cold consume
> from remote) both run end-to-end through S4 v1.2.2 into local MinIO; direct
> MinIO is the control.
> Companion to [#1 Elasticsearch frozen tier](elasticsearch-frozen-tier.md) /
> [#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md) /
> [#3 Grafana Loki chunks](grafana-loki-chunks.md).

Apache Kafka's **tiered storage** (KIP-405, GA in 3.9) offloads older log
segments from broker disks to an object store (S3/GCS/Azure) via a pluggable
`RemoteStorageManager`; a consumer reading old offsets fetches those segments
back from the store. S4 sits between the broker's RSM and your bucket and
re-compresses the tiered segments. In this benchmark that held fewer bytes for
the synthetic `none`/snappy/lz4 segments (real savings depend on payload
compressibility) — with no producer or consumer change; on the broker you point
the RSM's S3 endpoint at S4 and keep the plugin's own compression/encryption off.

```
  Kafka broker          S4 gateway            your S3 bucket
  (RemoteStorageMgr) ─▶ (re-compress)  ─▶    (segments, compressed objects)
   tier / fetch          ▲
                         └── consumer reads old offsets; S4 decompresses and
                             returns the segment to the broker
```

S4 is **complementary** to Kafka — it compresses the object store tiered storage
already uses; it is not a replacement for Kafka, its brokers, or its tiered-storage
plugin.

---

## TL;DR

Producing 600,000 ECS-style structured records into each of four tiered topics —
one per **producer `compression.type`** {none, snappy, lz4, zstd} — and letting
the rolled segments tier to S3 through S4 (zstd-3):

| Metric | Result |
|---|---|
| **Tiered storage saved** (S4 zstd-3 over the tiered segments) | **−74.7%** on `none` (uncompressed) · **−22.6%** snappy · **−20.6%** lz4 · **−0.0%** zstd |
| **The honest catch** | the producer's own `compression.type` already shrinks segments at the source. Producer-`zstd` tiers at 43.9 MB — the **same floor** S4 reaches over uncompressed segments (42.0 MB) — and S4 saves **~nothing** on top of producer-`zstd`. So S4's wedge is `none`/snappy/lz4 and can't-change-the-producer (see [§ S4 vs producer-side compression](#s4-vs-producer-side-compression-read-this)). |
| **Cold remote-fetch** | single noisy cold-consume samples were within ~10% of direct across all four codecs (not a latency guarantee); S4 also stores far fewer bytes on the backend for `none`: 42 vs 166 MB |
| **Compatibility** | tier + cold-fetch verified end-to-end through S4; a 200k-record `compression.type=none` tiering probe succeeded **without** `--logical-etag` in both checksum modes (the main run used the flag; recommended for correct ETags) |
| **Break-even** | depends on your producer codec mix *and* your storage and host cost assumptions — big for uncompressed/`none` backlogs, ~nil for already-`zstd` topics (parameterised below) |

**Bottom line:** S4 squeezes Kafka's tiered segments hard when the producer
left them uncompressed (−74.7%) and modestly over snappy/lz4 (~20%), but it
cannot beat a producer that already sends `zstd`. The cleanest win for **new**
traffic is to set `compression.type=zstd` on the producer; S4's real value is
the **tiered segments of uncompressed/legacy producers** (every segment they
tier through S4 is stored compressed, with no producer change) and clusters where
you can't change every producer.

---

## How Kafka tiered storage uses the object store

1. A producer appends records to a partition; the broker writes them to local
   **log segments** (record batches are compressed with the *producer's*
   `compression.type`). When a segment rolls (`segment.bytes`), it becomes
   eligible for tiering.
2. The `RemoteStorageManager` copies rolled segments (the `.log` plus its
   indexes) to the object store; after `local.retention.*` the broker deletes
   the local copy.
3. A consumer reading offsets that are no longer local triggers a **remote
   fetch**: the RSM reads the segment back from the store and serves it.

S4 compresses each tiered object on the way in and decompresses it on the
remote-fetch path, returning the original segment to the broker (this run
verified Kafka cold-consumed the expected 600,000 records per topic); the bucket
holds fewer bytes. The Aiven plugin's own chunk compression/encryption is left
**off** in this benchmark, so S4 is the only re-compressor.

---

## The benchmark

Everything ran **locally, end-to-end** (no AWS billing).

| Component | Version / spec |
|---|---|
| Host | AMD Ryzen 9 9950X (16C/32T), Linux |
| Kafka | `apache/kafka:3.9.1`, KRaft single-node, KIP-405 tiered storage |
| RSM plugin | Aiven `tiered-storage-for-apache-kafka` v1.1.1 (S3 backend; plugin compression + encryption OFF) |
| Object store | `minio/minio:latest` (pulled at run time), local |
| S4 | v1.2.2, `--codec cpu-zstd --dispatcher always --zstd-level 3 --logical-etag` |

**Configs** — for each S3 endpoint (direct MinIO, and S4 → MinIO) the broker
created one tiered topic per producer codec and produced 600,000 records:

| Topic | Producer `compression.type` | Tiered via |
|---|---|---|
| tnone / tsnappy / tlz4 / tzstd | none / snappy / lz4 / zstd | direct → MinIO, then S4 → MinIO |

Each topic: `remote.storage.enable=true`, `segment.bytes=8 MiB`,
`local.retention.ms=1000` (so rolled segments tier and are dropped locally fast).
**Dataset:** 50,000 seeded synthetic ECS-style logfmt lines (service/level labels + a
high-cardinality logfmt line), cycled by the producer — a synthetic but
realistic shape; real compressibility is workload-dependent.

---

## Result 1 — Tiered storage (the headline)

Bytes stored in the bucket for each topic's tiered (rolled) segments, direct vs
through S4. Tiered bytes exclude the still-local active segment in every config;
the S4 columns include S4's own sidecar objects.

| Producer codec | Direct (tiered bytes) | Through S4 (zstd-3) | vs direct |
|---|---:|---:|---:|
| **none** (uncompressed) | 166.2 MB | **42.0 MB** | **−74.7%** |
| snappy | 74.2 MB | 57.4 MB | **−22.6%** |
| lz4 | 70.5 MB | 56.0 MB | **−20.6%** |
| **zstd** | 43.9 MB | 43.9 MB | **−0.0%** |

This isn't a roll-point counting artifact: fetching one tiered `none` segment
back through S4 returns **8,384,517 bytes** (the logical segment, from a
`segment.bytes=8 MiB` topic) for a
**2,123,576-byte** stored object — **−74.67% on that single segment**, which
rounds to the same −74.7% as the table row.

Two honest readings:

- **In this synthetic run, uncompressed segments were S4's sweet spot (−74.7%).** Kafka segments hold
  the producer's record batches; with `compression.type=none` they are raw,
  text-heavy bytes that zstd-3 squeezes hard. S4 over `none` (42.0 MB) lands at
  essentially the **same floor** as producer-`zstd` (43.9 MB).
- **In this run, S4 did not beat the producer's own `zstd` (−0.0%).** Once the producer
  compresses batches with zstd, the tiered segments are already dense; S4 has
  nothing left to take. Over snappy/lz4 it recovers a residual ~20%.

---

## S4 vs producer-side compression (read this)

This is the crux, stated plainly:

- For **new** traffic, **set the producer `compression.type` to `zstd`** — it
  compresses batches at the source and tiers small segments (43.9 MB here)
  without a proxy. S4 over `none` actually lands a touch lower (42.0 MB), so the
  two reach essentially the same floor — producer-`zstd` just gets there with no
  proxy in the path.
- `compression.type` is a **producer choice**, and already-tiered segments keep
  whatever codec produced them — Kafka never re-compresses them. A cluster whose
  producers send `none`/snappy/lz4 has a large body of tiered segments that
  switching one producer does *nothing* for.
- **That body is S4's job.** Point the RSM's S3 endpoint at S4 and from then on
  every segment the broker tiers is **stored through S4's compressed-object
  format** — measured savings were −74.7% over `none` and ~20% over snappy/lz4
  (no material gain over already-`zstd` segments) — with no producer or consumer
  change; it also covers
  clusters where you simply can't coordinate a codec change across every
  producer (and, by the same mechanism, topics with a **mixed** codec history —
  though only one-codec-per-topic was exercised here). (Segments
  already tiered to the bucket *before* S4 was inserted would need a bulk
  `s4 migrate`, which this benchmark did not exercise — it measured segments
  written through S4.)

So S4 is not a Kafka replacement, and it doesn't compete with producer-`zstd`: for new traffic you control, prefer producer-`zstd`; reach for S4 on tiered segments from `none`/snappy/lz4 or mixed legacy histories. Position them by segment
population: producer-`zstd` for new traffic you control, S4 for uncompressed /
snappy / lz4 / legacy / mixed tiered segments. The same shape as the
[Loki snappy-backlog split](grafana-loki-chunks.md) and the
[ES default-codec / LogsDB split](elasticsearch-frozen-tier.md).

---

## Result 2 — Cold remote-fetch latency

After tiering, the broker was **restarted** (empty plugin chunk cache) and each
topic consumed from earliest. The rolled segments are local-deleted, so that
portion is served from S3; only the final **active** segment may still be read
locally. `fetch.time.ms` from `kafka-consumer-perf-test` (600,000 records each):

| Producer codec | Direct | Through S4 | Δ |
|---|---:|---:|---:|
| none | 1233 ms | 1350 ms | +117 ms |
| snappy | 961 ms | 905 ms | −56 ms |
| lz4 | 571 ms | 628 ms | +57 ms |
| zstd | 538 ms | 527 ms | −11 ms |

These are **single cold samples and noisy**, so don't over-read any one row.
These single samples **did not show a consistent S4 remote-fetch penalty** —
across all four codecs S4 landed within ~10% of direct, sometimes faster,
sometimes slower by a little — but repeat on your own storage/network before
relying on latency behavior. For `none`, S4 **stored** far fewer bytes on the
backend (42 vs 166 MB) — but this run measured stored bytes and fetch time, not
backend transfer bytes or the decompression/IO split, so don't over-attribute the
mechanism. This table measures only cold remote-fetch latency — S4's tier-**write**
compression cost, broker tiering delay, and CPU were not measured. The read cost
lands on the cold remote-read path, which already expects store latency.

> The absolute milliseconds are local-MinIO/loopback values (not WAN object-store RTT); the **transferable**
> comparison in this local run is the **direct-vs-S4 delta**; validate separately
> on your object store and network. On a remote store each fetch also pays
> backend RTT — validate against your store's RTT (the
> [ES frozen-tier doc](elasticsearch-frozen-tier.md) injects backend RTT to
> quantify this kind of per-object overhead).

---

## Compatibility

- **Tier + cold-fetch**: verified end-to-end through S4 — the broker tiered
  rolled segments through S4 and a cold consume-from-earliest read them back and
  returned all 600,000 records per topic.
- **`--logical-etag` is *not* required for the tested Kafka 3.9.1 + Aiven plugin
  setup** (unlike OpenSearch's
  `repository-s3`, which rejects every blob without it). We pointed the broker at
  an S4 gateway running **without** the flag and a 200k-record probe tiered all
  its segments — with the plugin's checksum check both **off** (default) and
  **on**. The probe did not reject S4's compressed-object ETag in either mode (unlike
  OpenSearch's repository-s3, which validates the PUT ETag against the payload),
  and S4 echoes the SDK's CRC32 checksums correctly. The flag is
  still **recommended** so S4 presents the correct logical ETag to any other tool
  that reads the bucket and does validate it. Captured evidence:
  [`results/logical_etag_negative.txt`](../../benches/kafka-tiered/results/logical_etag_negative.txt).
- **Plugin-level compression/encryption** were left **off** so S4 is the only
  re-compressor; if you enable the Aiven plugin's own chunk compression, S4's
  residual gain is expected to shrink toward the zstd row (not exercised here).
  **Multi-broker replication** was not exercised (single-node KRaft).

---

## When this pays off (and when it doesn't)

**Good fit**
- Topics whose producers send **`none`** (or snappy/lz4) — S4 takes −74.7% off
  uncompressed tiered segments, ~20% off snappy/lz4, transparently.
- Clusters where you **can't change every producer's** `compression.type`, or
  topics with a **mixed-codec** tiered history.

**Think twice**
- **You control the producers and they send compressible data: set
  `compression.type=zstd`** — it tiers small segments (43.9 MB here) without a
  proxy and S4 adds ~0 on top. Don't put S4 in the path purely for greenfield,
  all-zstd topics.
- The S4 gain is a function of **what the producer already did** — size it with
  `s4 estimate` against your real topics, don't assume the −74.7% `none` case.
- **Availability**: with the RSM pointed at S4, S4 is on **both** the tier-upload
  and the remote-fetch paths — if S4 is down, tiering and cold consumes of tiered
  offsets can fail or retry (local reads of recent offsets are unaffected).
  Outage/failover behavior was not exercised here. Run ≥2 stateless S4 instances
  behind a health-checking LB.

> **Break-even** (storage bytes only; excludes requests/egress/ops; includes an
> illustrative one-host S4 cost). The benchmark-derived inputs are the per-codec
> savings above; the prices are illustrative, not measured. For **uncompressed** (`none`) tiered
> data written through S4 at ~$23/TB-month object storage and a ~$70/month S4
> host (existing backlog migration not measured), savings ≈ `tiered_TB × 0.747 × $23/TB-month` is net-positive from
> **~4 TB**; for a **snappy/lz4** backlog it's **~13.5 TB (snappy) / ~14.8 TB (lz4)**; for an
> already-`zstd` backlog S4 does not pay for itself. Plug in your own codec mix,
> price, and host cost.

---

## Recommended configuration

```properties
# broker server.properties — Aiven RSM pointed at S4 (S4 is the only re-compressor)
remote.log.storage.system.enable=true
remote.log.storage.manager.class.name=io.aiven.kafka.tieredstorage.RemoteStorageManager
remote.log.storage.manager.class.path=/opt/tiered-storage/*
remote.log.storage.manager.impl.prefix=rsm.config.
rsm.config.storage.backend.class=io.aiven.kafka.tieredstorage.storage.s3.S3Storage
rsm.config.storage.s3.bucket.name=kafka-tiered
rsm.config.storage.s3.endpoint.url=http://s4.internal:8014    # the S4 gateway (or https:// if you terminate TLS on/in front of S4)
rsm.config.storage.s3.path.style.access.enabled=true
rsm.config.compression.enabled=false   # let S4 do the compression
rsm.config.encryption.enabled=false
```

```bash
# S4 gateway — --logical-etag recommended (correct ETags; not required by Kafka)
s4 --endpoint-url https://s3.<region>.amazonaws.com \
   --host 0.0.0.0 --port 8014 \
   --codec cpu-zstd --zstd-level 3 --dispatcher always --logical-etag
```

For new high-volume topics you control, also set the producer
`compression.type=zstd` — that is the simpler lever for greenfield traffic. Only Apache Kafka 3.9.1 + the Aiven plugin was exercised here; other
Kafka-compatible tiered-storage implementations may work the same way but were
not tested and should be validated separately.

---

## Reproduce

Harness: [`benches/kafka-tiered/`](../../benches/kafka-tiered/) — stand up MinIO
+ S4 + a KRaft Kafka 3.9.1 broker with the Aiven tiered-storage plugin locally,
produce 600k records per codec, tier the segments, measure tiered bytes per
topic, then cold-consume from remote. Raw data + the `--logical-etag` negative
capture are in [`results/`](../../benches/kafka-tiered/results/). All
measurements: AMD Ryzen 9 9950X, Kafka 3.9.1, Aiven plugin v1.1.1,
`minio/minio:latest`, S4 v1.2.2, local, 2026-06-19. Storage figures are tiered
bytes on the backend; request/egress not separately measured.

---

*See also: [#1 ES frozen tier](elasticsearch-frozen-tier.md) ·
[#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md) ·
[#3 Grafana Loki chunks](grafana-loki-chunks.md) ·
[savings & `s4 estimate`](../savings.md) · [compatibility](../compatibility.md).*
