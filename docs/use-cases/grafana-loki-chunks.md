# Use case: S4 in front of Grafana Loki chunk storage

> **Series:** S4 use cases · **#3 — Grafana Loki chunks**
> **Status:** measured locally. The write path (ingest → flush → store) runs
> end-to-end through a single-binary Grafana Loki 3.3.2 instance and S4 v1.2.2
> into local MinIO; read overhead is measured with object-level whole-chunk GETs
> (not Loki's query engine — see Result 3).
> Companion to [#1 Elasticsearch frozen tier](elasticsearch-frozen-tier.md) /
> [#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md).

Grafana Loki stores its log **chunks** in an object store (S3/GCS/Azure) and
fetches them back from the store when a query needs them — the same
object-store read pattern as a Lucene segment in the Elasticsearch frozen tier. Loki
compresses each chunk before upload with a configurable `chunk_encoding`;
**`snappy`** (set explicitly in this benchmark). S4 sits between
Loki's chunk client and your bucket and re-compresses those chunks, so the
store holds fewer bytes. S4 sits below Loki's query layer, so LogQL queries and
dashboards are expected to be unchanged (the only Loki change is its object-store
endpoint) — this harness verified object-level write + read identity, not LogQL
query execution.

```
  Grafana Loki         S4 gateway            your S3 bucket
  (chunk store) ──▶   (re-compress)  ──▶    (chunks, fewer bytes)
   flush / query        ▲
                        └── object GET reads chunks back; S4 decompresses,
                            Loki sees the original snappy chunk bytes
```

S4 is **complementary** to Loki — it compresses the object store Loki already
uses; it is not a replacement for Loki or its chunk store.

---

## TL;DR

On 4,000,000 ECS-style structured log lines ingested into Loki (`chunk_encoding:
snappy`, set explicitly here), chunks written through S4 into a real
S3-compatible object store (local MinIO):

| Metric | Result |
|---|---|
| **Bucket storage saved** (chunks + TSDB index + any S4 sidecars; S4 zstd-3 over snappy) | **−18.4%** (zstd-9: −19.3%) |
| **The honest catch** | switching Loki's own `chunk_encoding` to `zstd` saves **−38%** on **new** chunks — *more* than S4, and simpler. So S4's wedge is narrow (see [§ S4 vs Loki-native zstd](#s4-vs-loki-native-zstd-read-this)). |
| **Read overhead (real, not zero)** | a whole-chunk GET through S4 takes **~1.7 ms longer** than a raw snappy GET (3.8 vs 2.1 ms median locally, ~1.8×) — the cost of fetching the compressed object and decompressing it. Bytes returned are byte-identical. May be partly offset on a remote store (S4 pulls fewer bytes); validate against your store's RTT/bandwidth. |
| **Ingest+flush** | S4 zstd-3 ≈ direct (38.8 s vs 38.3 s for 4M lines); Loki-native zstd was clearly the slowest in this run |
| **Compatibility** | sampled chunk GETs read back through S4 byte-identically; a 200k-line Loki 3.3.2 probe uploaded chunks **without** `--logical-etag` (the main run used the flag; it's recommended for correct ETags) |
| **Break-even** | ~**16.5 TB** of snappy chunk backlog makes S4 net-positive at a 1-instance host (parameterised below) |

**Bottom line:** S4 shaves ~18% off the bucket bytes for the *snappy*
configuration transparently — but Loki's own `zstd` encoding shaves more (−38%),
with no S4 proxy or host, on **new** chunks. S4's real value is the **immutable
snappy backlog**: `chunk_encoding` changes
are forward-only, so past snappy chunks stay snappy forever — `s4 migrate` is
intended to recompress that backlog at the same keys with no Loki re-ingest
(bulk migrate not measured here; validate on a staging copy).

---

## How Loki uses the object store

1. Ingesters accumulate log lines per stream and, at `chunk_target_size`
   (`1572864` B ≈ 1.5 MiB here) or idle, **compress the chunk** (snappy in this
   run) and flush
   it to the object store as one blob; the TSDB index goes there too.
2. A LogQL query resolves matching chunks from the index and **reads** them
   from the store, decompresses, and runs the filter/aggregation.

S4 compresses each chunk blob on the way in and decompresses it on the way out
(full-object reads in this benchmark; S4 also serves partial ranges via the
per-object **S4IX sidecar**, which this run did not exercise). Loki sees
byte-identical snappy chunks; the bucket holds fewer bytes.

---

## The benchmark

Everything ran **locally, end-to-end** (no AWS billing).

| Component | Version / spec |
|---|---|
| Host | AMD Ryzen 9 9950X (16C/32T), Linux |
| Grafana Loki | `grafana/loki:3.3.2`, single-binary, TSDB/v13, S3 chunk store |
| Object store | `minio/minio:latest` (pulled at run time), local |
| S4 | v1.2.2, `--codec cpu-zstd --dispatcher always --logical-etag`, one per zstd level |

**Configs** — for each, Loki's chunk store endpoint pointed at a different
target, then 4,000,000 lines ingested and flushed:

| Config | Loki `chunk_encoding` | Chunk store endpoint |
|---|---|---|
| direct | snappy | MinIO (no S4) |
| S4 zstd-3 / zstd-9 | snappy | S4 → MinIO |
| Loki-native zstd | **zstd** | MinIO (no S4) |

**Dataset:** 4M ECS-style web/access-log lines. Loki labels are low-cardinality
(`service`, `level`); the high-cardinality fields (path, status, bytes, ip,
user, trace id, `ua` user-agent) live in the logfmt **line** — the realistic shape,
and what actually compresses.

---

## Result 1 — Bucket storage (chunks + index)

All bytes stored in the bucket — Loki chunks + TSDB index, **plus any S4
sidecar/metadata objects** for the S4 configs (so the savings below are net of
S4's own sidecar overhead) — and the saving vs direct snappy:

| Config | Stored | vs direct snappy |
|---|---:|---:|
| direct (snappy) | 456.6 MB | — |
| **S4 zstd-3** (over snappy) | 372.4 MB | **−18.4%** |
| S4 zstd-9 (over snappy) | 368.6 MB | −19.3% |
| **Loki-native zstd** (no S4) | 283.2 MB | **−38.0%** |

Two honest readings:

- **S4 over snappy is a double-compression residual.** snappy already removed
  the easy redundancy, so S4's zstd pass recovers ~18% more — real and
  transparent to Loki (no Loki change), though it adds an S4 gateway dependency
  and host cost. The gain is far smaller than compressing raw text. In this run,
  zstd-9 over zstd-3 bought <1 percentage point of storage for +3.2 s
  ingest+flush, so zstd-3 was the better measured tradeoff.
- **Loki's own zstd beats it (−38%).** If you can change `chunk_encoding`,
  Loki-native zstd is the bigger, simpler win for **new** chunks. That makes the
  S4 wedge specific — see below.

---

## S4 vs Loki-native zstd (read this)

This is the crux, stated plainly rather than defensively:

- For **new** chunks, **switch `chunk_encoding` to `zstd`** — it's −38% storage
  with no S4 proxy in the path (it was the slowest ingest+flush path in this run,
  so check your ingest headroom). In this run S4-over-snappy did not beat
  Loki-native zstd for new chunks.
- `chunk_encoding` is **forward-only** (documented Loki behavior, not something
  this harness re-verified): Loki keeps reading old chunks in their original
  encoding and **never rewrites them**. A cluster that has run on `snappy` for a
  year has a large **immutable snappy backlog** that switching the setting does
  *nothing* for.
- **That backlog is S4's job.** The intended S4 path for it is `s4 migrate` over
  the bucket/prefix; this run did **not** measure bulk migration, so the ~18% is
  **extrapolated** from the write-through bucket result above — validate it on a
  staging copy.
  `s4 migrate` is designed to rewrite each backend object in place (same key) —
  the chunk's logical bytes are unchanged, so Loki keeps reading it, with no
  re-ingest. Note the distinction: *pointing Loki's chunk store at S4* only affects **future**
  writes — it does not retro-compress
  objects already in the bucket; that's what `s4 migrate` is for.

So for this use case, position them by **chunk population**: Loki-native zstd
for **new** chunks, `s4 migrate` for the **already-written snappy backlog**. The
same shape as the [ES default-codec / LogsDB split](elasticsearch-frozen-tier.md).

---

## Result 2 — Ingest+flush throughput

Wall-clock for the full write path — push 4M lines, `POST /flush`, and wait for
the store to settle — so S4's compression PUT cost is **inside** the timed
region (not a push-only number). The settle step polls the bucket until it stops
growing, which adds **run-to-run noise of a few seconds**, so read these as a
band, not exact:

| Config | 4M-line ingest + flush |
|---|---:|
| direct (snappy) | 38.3 s |
| S4 zstd-3 | 38.8 s |
| S4 zstd-9 | 42.0 s |
| Loki-native zstd | 59.3 s |

The settle poll adds noise, so treat these as a band, not exact.
The robust reading: **S4 zstd-3 ≈ direct** (38.8 s vs 38.3 s — S4's compression
keeps up with the flush path at this chunk size), zstd-9 a little more, and
**Loki-native zstd clearly the slowest in this run** (59.3 s), consistent with
the cost of its denser native compression.

---

## Result 3 — Read overhead (whole-chunk GET)

This is the cost buyers actually care about: **does S4 slow down reads?** It does
add some — and here is exactly how much, measured the honest way.

A LogQL query resolves matching chunks from the index and reads them from the
store; S4 only touches that read — it pulls the (compressed) chunk object and
decompresses it back to the original snappy chunk. Rather than time Loki's query
engine (whose index lookup we couldn't stage reliably — see the note below), we
measured that object read directly as an **index-independent proxy**: for 40
chunk objects present in **both** the direct (raw snappy) and S4 buckets, a
whole-chunk GET from MinIO vs the **same** chunk through S4. We issue a
**full-object GET (no HTTP `Range` header)** directly against MinIO/S4 — the
benchmark does **not** observe Loki's actual query-time request shape; it times
full-object GETs of real chunk objects as an index-independent proxy for the
read cost. It does **not** exercise Loki's index lookup, chunk cache, query
fan-out, or concurrency, nor S4's partial-`Range` (sidecar) path.

| Whole-chunk GET (40 real chunks) | Direct (raw snappy) | Through S4 (zstd→snappy) |
|---|---:|---:|
| median | **2.1 ms** | **3.8 ms** |
| mean | 2.2 ms | 3.9 ms |
| p90 | 3.0 ms | 4.6 ms |

**S4 adds ~1.7 ms per chunk GET — about 1.8×** the raw-snappy time. That delta
is the whole S4 path: the extra gateway hop, the backend fetch, and the zstd
decompress. The bytes S4 returns are **byte-identical** to the raw snappy chunk
(0 mismatches over all 40).

Two things keep this in perspective — but it is a **real, non-zero** cost, not
the "no penalty" a naïve test would report:

- These are **no-RTT localhost** numbers, so the ~1.7 ms is the *whole*
  S4-path delta (the extra gateway hop + backend fetch + zstd decompress). A
  remote object store adds per-GET backend RTT on top of both columns, so that
  ~1.7 ms becomes a smaller *fraction* of total read time — validate against your
  store's RTT (the [ES frozen-tier doc](elasticsearch-frozen-tier.md) injects
  backend RTT to quantify exactly this for the same per-object overhead).
- S4 fetches the **compressed** object from the backend (the recompressed chunk,
  smaller than the raw snappy original), which on a remote store claws back part
  of the transfer time. This run did not record per-chunk backend transfer bytes,
  so treat the offset qualitatively.

A similar overhead may appear for uncached chunk reads in production, but the
measured +1.7 ms is specifically for **direct whole-chunk GETs** in this local
harness — actual Loki query impact (request shape, caching, fan-out) was not
measured. Validate against your
store's RTT — the
[ES frozen-tier doc](elasticsearch-frozen-tier.md) quantifies how this kind of
per-object overhead behaves under injected backend latency.

> An earlier version of this harness timed Loki's query API after a restart and
> reported "~0 ms overhead". That was **wrong**: Loki's TSDB index-shipper had
> not uploaded the index to the store on this timescale, so the post-restart
> queries hit an empty index and measured nothing. The whole-chunk GET above is
> the index-independent, valid metric — and it shows S4 *does* cost ~1.7 ms.

---

## Compatibility

- **Chunk write + read**: Loki **writes** were verified through S4 (chunks
  landed in the bucket); read-byte **identity** was verified by direct
  whole-chunk GETs back through S4 over the sampled chunk objects (byte-identical
  to raw snappy — Result 3). We did not drive the reads via Loki's own query path.
- **`--logical-etag` is *not* required for Loki** (this surprised us — it
  **is** required for OpenSearch's `repository-s3`). We pointed Loki 3.3.2 at an
  S4 gateway running **without** the flag and a 200k-line probe uploaded all its
  chunks fine: Loki's S3 client does not reject the ETag mismatch. Without the flag S4's PUT returns
  the compressed-object ETag (≠ MD5 of the original) and HEAD returns no ETag, so
  the flag is still **recommended** — it makes S4 present the correct logical
  ETag for any client or tool that *does* validate it. Captured evidence:
  [`results/logical_etag_negative.txt`](../../benches/grafana-loki/results/logical_etag_negative.txt).
- **Compactor / retention deletes** are expected to be ordinary `LIST` + `DELETE` operations,
  which S4 passes through (per the [compatibility matrix](../compatibility.md));
  **not separately exercised in this run** — verify on a staging bucket before
  enabling retention against an S4-fronted store.

---

## When this pays off (and when it doesn't)

**Good fit**
- A large **existing snappy chunk backlog** you can't or won't re-ingest —
  `s4 migrate` is the intended path (~18%, estimated from write-through; bulk
  migrate not measured here), at the same keys, with no Loki re-ingest and no
  `chunk_encoding` change.
- Operators who must not touch a working ingest pipeline.

**Think twice**
- **New chunks: just set `chunk_encoding: zstd`** — it's −38% storage with no S4
  proxy. Don't put S4 in the path for greenfield Loki.
- The S4 gain is a **double-compression residual** (~18%), smaller than
  raw-text compression — size it with `s4 estimate`, don't assume frozen-tier
  ratios.
- **Availability**: S4 becomes a read-path dependency for cold chunks — a query
  that misses caches can't read its chunks if S4 is down (queries served entirely
  from Loki's cache shouldn't need S4, though cache behavior was not exercised
  here). Run ≥2 stateless S4 instances behind a health-checking LB.
  (Multi-instance HA failover was not exercised in this Loki run; the ES doc has
  a [measured HA failover test](elasticsearch-frozen-tier.md#availability--ha)
  for the same stateless-gateway pattern.)

> **Break-even** (excludes requests, egress, and ops; includes an illustrative
> one-host S4 cost). The only
> benchmark-derived input is the **0.184** savings factor; the prices are
> illustrative, not measured — plug in your own. With example public-cloud
> figures (~$23/TB-month object storage, ~$70/month per S4 host):
> savings ≈ `backlog_TB × 0.184 × $23/TB-month`, which is net-positive from
> **~16.5 TB** of snappy backlog on a single host (HA 2-instance: ~33 TB). Only
> the snappy backlog counts — new chunks should be native-zstd.

---

## Recommended configuration

```bash
# S4 gateway — --logical-etag recommended (correct ETags; not required by Loki)
s4 --endpoint-url https://s3.<region>.amazonaws.com \
   --host 0.0.0.0 --port 8014 \
   --codec cpu-zstd --zstd-level 3 --dispatcher always --logical-etag
```

```yaml
# loki.yaml — point the chunk store at S4 instead of S3 directly
common:
  storage:
    s3:
      endpoint: s4.internal:8014
      bucketnames: loki-chunks
      s3forcepathstyle: true
      insecure: true   # or terminate TLS on S4
```

Or, to compress an existing backlog without touching Loki, run
`s4 migrate loki-chunks --endpoint-url https://s3.<region>.amazonaws.com --execute`
against the backend (the bucket-level write-through saving is what's measured
above; the bulk migrate itself was not measured — validate it on a staging copy
first). New chunks: prefer
`ingester.chunk_encoding: zstd`.

---

## Reproduce

Harness: [`benches/grafana-loki/`](../../benches/grafana-loki/) — stand up MinIO
+ S4 + Loki locally, ingest 4M lines, flush, measure stored bytes across
direct / S4 zstd-3/9 / Loki-native-zstd, then time whole-chunk GETs (direct
snappy vs the same chunk through S4). Raw data + the `--logical-etag` negative
capture are in [`results/`](../../benches/grafana-loki/results/). All
measurements: AMD Ryzen 9 9950X, Loki 3.3.2, `minio/minio:latest`, S4 v1.2.2,
local, 2026-06-19. Storage figures are bytes stored on the backend; Loki
receives the original logical chunk bytes, and this run did not separately
measure backend request counts or network egress.

---

*See also: [#1 ES frozen tier](elasticsearch-frozen-tier.md) ·
[#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md) ·
[savings & `s4 estimate`](../savings.md) · [compatibility](../compatibility.md).*
