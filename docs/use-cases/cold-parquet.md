# Use case: recompacting cold Parquet in a data lake

> **Series:** S4 use cases · **#5 — Cold Parquet recompaction**
> **Status:** measured locally. A new `s4 parquet-recompact` subcommand (behind
> the `parquet-recompact` build feature) reads cold Parquet objects from a bucket
> and re-encodes their column chunks to **zstd**, writing back a **native**
> Parquet — verified value-for-value with pyarrow. Runs end-to-end against local
> MinIO (no AWS billing).
> Companion to [#1 Elasticsearch frozen tier](elasticsearch-frozen-tier.md) /
> [#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md) /
> [#3 Grafana Loki chunks](grafana-loki-chunks.md) /
> [#4 Kafka tiered storage](kafka-tiered-storage.md).

Data-lake Parquet is overwhelmingly written with the **snappy** column codec —
it's the long-time default for Spark, pandas, and Arrow. Snappy optimises for
decode speed, not size, so a cold table that is queried rarely is paying for
snappy's looser ratio every month it sits in the bucket. `s4 parquet-recompact`
reads such objects and re-encodes their column chunks to **zstd**, producing a
Parquet that is **still a normal Parquet** — pyarrow / Spark / Trino / DuckDB
read it directly, **with no S4 in the read path**.

```
  cold Parquet objects        s4 parquet-recompact            same bucket, same keys
  (snappy / none / gzip)  ─▶  (read → re-encode cols → zstd) ─▶ (NATIVE zstd Parquet,
   in your bucket              verify value-for-value           pyarrow/Spark/Trino read it
                               before overwrite)                directly — no S4 at read time)
```

This is the one use case in the series that is **not** the transparent gateway.
It is an **offline recompaction** (like `s4 recompact`), run on demand against a
prefix: S4 never sits in the query path, so there is **no per-read overhead** and
no runtime dependency on S4 to read the data afterwards. S4 is **complementary**
here — it re-compresses Parquet your engines already produce; it is not a
replacement for Parquet, Spark, or your query engine.

---

## TL;DR

Building a 2,000,000-row ECS-style table (13 columns), writing it as Parquet in
each input column codec, uploading to the object store, then running
`s4 parquet-recompact … --execute` (zstd-3) in place:

| Metric | Result |
|---|---|
| **Storage saved** (native zstd Parquet over the input) | **−36.6%** over snappy (the data-lake default) · **−51.7%** over uncompressed (`none`) · **−3.8%** over gzip · **−0.0%** over zstd (skipped) |
| **The honest catch** | the *writer's* codec already shrinks the file at the source. A table written with `compression='zstd'` lands at 79.4 MB — the **same floor** `s4 parquet-recompact` reaches from snappy (80.9 MB) — and S4 saves **~nothing** on top of writer-`zstd`. So S4's wedge is the **snappy / none / gzip backlog** and tables you can't re-run the writer for (see [§ S4 vs writer-side compression](#s4-vs-writer-side-compression-read-this)). |
| **Read path** | the output is a **native** Parquet (output codec asserted `ZSTD`), read back by pyarrow with **`table.equals(original) == True`** — value-for-value identical. No S4 in the read path → **zero per-read overhead** (unlike the gateway use cases #1–4). |
| **Fidelity** | every object is **value-verified** (per row group, bounded memory, physical-schema-tree compared) **before** the in-place overwrite; any structural drift is a conservative skip, a decoded-value mismatch is a hard failure — it never overwrites with unverified data. |
| **Break-even** | a function of your input-codec mix; large for a snappy/uncompressed backlog, ~nil for an already-zstd lake (parameterised below). |

**Bottom line:** `s4 parquet-recompact` takes **−36.6%** off the snappy Parquet
that dominates real data lakes and **−51.7%** off uncompressed, producing a
native zstd Parquet your engines read with no S4 in the path. It **cannot** beat
a writer that already emits zstd — for **new** tables, set the writer's codec to
zstd. S4's value is the **existing cold snappy/none/gzip backlog** that no one is
going to re-run a Spark job to rewrite.

---

## Why cold Parquet is usually snappy

1. Spark (`spark.sql.parquet.compression.codec`), pandas
   (`to_parquet`), and Arrow have defaulted to **snappy** for years. Snappy is
   fast to decode but compresses loosely — great for hot, frequently-scanned
   tables, wasteful for cold ones.
2. Parquet is **immutable**: once a part-file is written, its column chunks keep
   whatever codec produced them. A table written snappy in 2024 is still snappy
   today, every byte of it, until something rewrites it.
3. Rewriting it means re-running the writer (a Spark/pandas job over the whole
   table) — which most teams never schedule for cold data, so the snappy backlog
   just accumulates.

`s4 parquet-recompact` rewrites the **column chunks** to zstd directly from the
object store, preserving the input **row-group boundaries** (so predicate
pushdown granularity is unchanged) and carrying the Parquet **key-value file
metadata** (the Spark/pandas schema; the `ARROW:schema` key is regenerated by the
writer and verified separately via the Arrow schema) — no Spark cluster, no
query-engine change.

---

## The benchmark

Everything ran **locally** against MinIO (no AWS billing).

| Component | Version / spec |
|---|---|
| Host | AMD Ryzen 9 9950X (16C/32T), Linux |
| Object store | `minio/minio:latest`, local |
| S4 | v1.2.2, `parquet-recompact` feature, `--target-zstd-level 3` |
| Verifier | pyarrow: recompacted object read **natively** + `table.equals(original)`; output codec asserted `ZSTD` |

**Dataset:** a 2,000,000-row ECS-style access-log table, 13 columns
(timestamp, service, level, method, path, status, bytes, duration, host, ip,
user, trace_id, user-agent; seed 42) — a synthetic but realistic
structured-log shape. It was written once per input codec
{none, snappy, gzip, zstd} with `row_group_size=200,000`, uploaded, then
recompacted in place to zstd-3.

---

## Result 1 — storage (the headline)

Bytes stored in the bucket for the table, as written by each input codec vs after
`s4 parquet-recompact --execute --target-zstd-level 3`:

| Input codec | Input | After recompact (native zstd) | vs input | `table.equals` |
|---|---:|---:|---:|:--:|
| **snappy** (data-lake default) | 127.6 MB | **80.9 MB** | **−36.6%** | ✅ |
| **none** (uncompressed) | 167.4 MB | **80.9 MB** | **−51.7%** | ✅ |
| gzip | 84.1 MB | 80.9 MB | **−3.8%** | ✅ |
| **zstd** | 79.4 MB | 79.4 MB | **−0.0%** (skipped) | ✅ |

Three honest readings:

- **Snappy — the case that matters — gives −36.6%.** Snappy is what real data
  lakes are full of, and re-encoding its column chunks to zstd-3 lands at
  80.9 MB, essentially the **same floor** as a table written zstd from scratch
  (79.4 MB). This is the headline number: a third off the default-codec backlog.
- **Uncompressed `none` gives −51.7%.** Raw column chunks have the most to gain;
  S4 over `none` reaches the same ~80.9 MB floor.
- **Already-zstd is skipped (−0.0%).** S4 reads the footer (unspoofable) and, if
  every column chunk is already zstd, **skips the re-encode and the overwrite** —
  it does not re-tune existing zstd levels. gzip is close to the floor already,
  so the residual (−3.8%) is small.

---

## S4 vs writer-side compression (read this)

The crux, stated plainly — the same shape as the
[Loki snappy-backlog split](grafana-loki-chunks.md) and the
[Kafka producer-codec split](kafka-tiered-storage.md):

- For **new** tables you control, **set the writer's codec to zstd**
  (`spark.sql.parquet.compression.codec=zstd`, or
  `df.to_parquet(compression='zstd')`). It compresses at the source and writes a
  small file (79.4 MB here) with nothing in the path. `s4 parquet-recompact` from
  snappy reaches essentially the same floor (80.9 MB) — the writer just gets
  there without a rewrite pass.
- The codec is a **writer choice**, and already-written Parquet keeps whatever
  codec produced it — nothing re-compresses it. A lake whose historical tables
  are snappy/none/gzip has a large cold body that switching one writer config
  does **nothing** for.
- **That body is S4's job.** `s4 parquet-recompact` rewrites those existing
  objects in place to native zstd Parquet — measured −36.6% over snappy, −51.7%
  over uncompressed — with no Spark job and **no change to how anything reads the
  data afterwards** (the output is plain Parquet).

So S4 is not a Parquet/Spark replacement and it doesn't compete with
writer-`zstd`: for new tables you control, prefer writer-`zstd`; reach for
`s4 parquet-recompact` on the **cold snappy / none / gzip backlog** you're not
going to re-run a writer job for.

---

## Result 2 — the read path is native (zero overhead)

This is the distinguishing property versus use cases #1–4, which put S4 in the
read path as a transparent gateway. Here the output is a **normal Parquet**:

- pyarrow opens the recompacted object directly and
  `table.equals(original)` is **True** for every codec — the decoded values,
  the full Arrow schema (including metadata), the row count, the row-group count,
  and the carried Parquet key-value metadata (excluding the regenerated
  `ARROW:schema`, which the Arrow-schema check covers) all match.
- the output's column codec reads back as **`ZSTD`** — it really is a zstd
  Parquet, not an S4 wrapper.
- **there is no S4 in the read path**, so there is **no per-read latency
  overhead** and **no runtime dependency on S4** to read the table afterwards.
  Spark / Trino / DuckDB / pyarrow read it exactly as they read any other
  Parquet.

What is **not** byte-for-byte preserved (and why it's safe): the re-encode goes
through Arrow, so column **encodings**, **page sizes**, `created_by`, and the
**page/column index** are regenerated over the identical data (functionally
equivalent, not byte-identical). The output is **decoded-value +
key-value-metadata compatible**. Objects carrying sort-order or bloom-filter
footer metadata are **skipped** (those query-planning hints aren't reproduced),
so they're never silently dropped.

---

## Fidelity & safety (it's an in-place overwrite)

`s4 parquet-recompact` rewrites objects **in place**, so it is built to refuse
rather than risk data:

- **Dry-run by default.** `--execute` is required to write, and additionally
  requires `--allow-lossy-physical-rewrite` to acknowledge that physical
  encodings/statistics/`created_by`/page-indexes are regenerated and object ACLs
  are not carried over.
- **Verify before write.** Each object is re-read and compared value-for-value
  against the input — per row group, streaming batch-by-batch (bounded memory),
  comparing the full Arrow schema, the **Parquet physical schema tree** (so an
  Arrow round-trip that would silently change an on-disk type — INT96 timestamps,
  decimal representation, field-ids, nested LIST/MAP shape — is caught even
  though the values still match), row/row-group counts, and the key-value
  metadata. A **structural** mismatch is a conservative **skip**; a decoded-value
  mismatch is a **hard failure** (downgradable with `--tolerate-value-mismatch`);
  a corrupt/unparseable footer is a hard failure. It **never** overwrites with
  unverified data.
- **Idempotent.** Objects whose columns are already entirely zstd are detected
  from the footer and skipped — re-running the command is an idempotent no-op for
  already-zstd objects (it skips the re-encode and the overwrite; each candidate
  is still read to inspect its footer).
- **Conflict-guarded.** The overwrite is conditional (`If-Match` on the source
  ETag, plus a pre-PUT re-HEAD of ETag / Last-Modified / version-id), so a
  concurrent rewrite is detected, not clobbered. **Skipped, not touched:**
  server-side-encrypted objects (SSE-S3/KMS/C), Object-Lock retention/legal-hold,
  objects with an `Expires` header, and archive-tier objects (GLACIER /
  DEEP_ARCHIVE) a plain GET can't read.
- **Bounded.** Input and output are spooled to temp files (peak RAM ≈ one decoded
  row group, independent of object size); `--max-body-bytes`, a footer-size
  guard, and live Arrow-memory caps bound resource use; `--max-objects` and
  `--older-than` bound the scan to cold data.

> Because tag-only / same-second metadata-only changes on an **unversioned**
> bucket can't be fully compare-and-swap-protected, run against **cold /
> quiescent** prefixes (that's the design — see `--older-than`). On a **versioned**
> bucket the rewrite lands as a new version, so the prior version is retained
> regardless.

---

## When this pays off (and when it doesn't)

**Good fit**
- A cold lake of **snappy** (or none/gzip) Parquet — `s4 parquet-recompact` takes
  −36.6% off snappy / −51.7% off uncompressed, producing native zstd Parquet your
  engines read unchanged.
- Tables you **won't re-run a writer job for** (one-off historical partitions,
  archived datasets, tables owned by another team).

**Think twice**
- **You control the writer and it emits compressible data: set its codec to
  zstd** — it writes small files (79.4 MB here) at the source and S4 adds ~0 on
  top. Don't recompact greenfield, all-zstd tables.
- The S4 gain is a function of **what the writer already did** — size it against
  your real tables, don't assume the snappy −36.6% case; an already-zstd lake
  won't pay for itself.
- Exotic Parquet (sort-ordered, bloom-filtered, SSE/Object-Lock/Expires, archive
  tier) is **skipped** by design — count the skips in the report before assuming
  full coverage.

> **Break-even** (storage bytes only; excludes requests/egress/ops). The only
> measured input is the per-codec saving above; supply your own storage price.
> For a **snappy** lake, monthly storage saving ≈
> `lake_TB × 0.366 × storage_price_per_TB_month`, weighed against the one-time
> recompaction compute; an uncompressed backlog scales by 0.517, gzip by ~0.038,
> an already-zstd lake by ~0. Plug in your own codec mix and prices.

---

## Recommended usage

```bash
# Build the s4 binary with the parquet-recompact feature
cargo install s4-server --features parquet-recompact   # installs the `s4` binary

# 1) Dry-run first (default): report what WOULD be recompacted, with savings,
#    against cold objects only. No writes.
s4 --endpoint-url https://s3.<region>.amazonaws.com \
   parquet-recompact my-lake/cold/events/ \
   --target-zstd-level 3 --older-than 30d --json

# 2) Execute the in-place rewrite once the dry-run looks right.
s4 --endpoint-url https://s3.<region>.amazonaws.com \
   parquet-recompact my-lake/cold/events/ \
   --target-zstd-level 3 --older-than 30d \
   --execute --allow-lossy-physical-rewrite
```

For **new** tables you control, prefer setting the writer's codec to zstd
(`spark.sql.parquet.compression.codec=zstd` /
`to_parquet(compression='zstd')`) — that's the simpler lever for greenfield data.
Only the local pyarrow/Arrow Parquet path was exercised here; validate exotic
schemas (deeply nested / dictionary-heavy) on a staging copy first — the verifier
will conservatively skip anything it can't prove identical.

---

## Reproduce

Harness: [`benches/cold-parquet/`](../../benches/cold-parquet/) — build a
2M-row ECS table, write it as Parquet in each input codec, upload to local MinIO,
run `s4 parquet-recompact --execute`, and verify the recompacted object with
pyarrow (`table.equals`, output codec `ZSTD`). Raw matrix:
[`results/cold_parquet.json`](../../benches/cold-parquet/results/cold_parquet.json).
All measurements: AMD Ryzen 9 9950X, `minio/minio:latest`, S4 v1.2.2, local,
2026-06-19. Storage figures are object bytes on the backend.

---

*See also: [#1 ES frozen tier](elasticsearch-frozen-tier.md) ·
[#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md) ·
[#3 Grafana Loki chunks](grafana-loki-chunks.md) ·
[#4 Kafka tiered storage](kafka-tiered-storage.md) ·
[savings & `s4 estimate`](../savings.md) · [compatibility](../compatibility.md).*
