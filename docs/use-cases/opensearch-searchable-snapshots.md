# Use case: S4 as an OpenSearch searchable-snapshot backend

> **Series:** S4 use cases · **#2 — OpenSearch searchable snapshots**
> **Status:** measured locally, end-to-end. Numbers below come from a real
> OpenSearch 2.19 cluster snapshotting through S4 v1.2.2 into MinIO.
> Companion to [#1 — Elasticsearch frozen tier](elasticsearch-frozen-tier.md).

OpenSearch's [searchable snapshots](https://opensearch.org/docs/latest/tuning-your-cluster/availability-and-recovery/snapshots/searchable_snapshot/)
keep shard data in an S3 *snapshot repository* and mount it (`storage_type:
remote_snapshot`) with a bounded local file cache — the open-source equivalent
of a cold/frozen tier. As with Elasticsearch, the bytes you pay S3 to store are
the **snapshot blobs** (Lucene segment files + repository metadata), and S4 sits
between OpenSearch's `repository-s3` client and your bucket to compress them.

```
  OpenSearch              S4 gateway            your S3 bucket
  searchable snapshot ─▶ (compress)   ─▶   (snapshot blobs, fewer bytes)
  (repository-s3)         ▲
   snapshot / restore     └── remote_snapshot search range-GETs blobs back;
   + remote_snapshot          S4 decompresses, OpenSearch sees original bytes
```

OpenSearch is Apache-2.0 and searchable snapshots need **no commercial
license** (unlike the Elasticsearch frozen tier, which needs an Enterprise
trial). That, plus OpenSearch's Apache-2.0 alignment with S4, makes this a
natural fit.

---

## TL;DR

On a 4-million-document structured-log index (1 shard, force-merged to a single
segment), snapshotted to a real S3 repository through S4:

| Metric | Result |
|---|---|
| **Repository storage saved** (S4 zstd-3) | **−16.5% to −28.1%** — biggest on the `default` (LZ4) codec; **~17%** even on OpenSearch's **native `zstd` codec** |
| **The catch** | requires S4's **`--logical-etag`** flag — without it, OpenSearch's `repository-s3` rejects every blob (`Data read has a different checksum than expected`). See [Compatibility](#compatibility-logical-etag-is-required). |
| **Searchable-snapshot search** (count / agg / full-text) | **S4 within ~1.5 ms of direct** in this local run (equal or faster on every query) |
| **End-to-end** | repo `_verify`, snapshot (SUCCESS), `remote_snapshot` mount + cold search all work through S4 |
| **Compounding** | native `zstd` codec **+** S4 zstd-3 = **909.9 MB** vs `default`-codec direct **1484.7 MB** → **1.63× smaller** |

**Bottom line:** with `--logical-etag` on, S4 shaves 16–28% off an OpenSearch
searchable-snapshot repository — and because OpenSearch's `index.codec` only
compresses *stored fields*, S4 still finds ~17% even on a native-`zstd` index by
squeezing the doc-values, postings and term dictionaries the codec leaves alone.

---

## The benchmark

Everything ran **locally, end-to-end** (no AWS billing).

| Component | Version / spec |
|---|---|
| Host | AMD Ryzen 9 9950X (16C/32T), Linux |
| OpenSearch | `opensearchproject/opensearch:2` (2.19.5), single node, 6 GB heap, security disabled, `node.search.cache.size: 4gb` |
| Object store | `minio/minio` (RELEASE.2025-09-07), local |
| S4 | v1.2.2, `--codec cpu-zstd --dispatcher always --logical-etag`, one instance per zstd level |

**Repositories:** one MinIO backend; repos `direct` (no S4), `s4z3`, `s4z9`.

**Dataset:** 4,000,000 ECS-style structured web/access-log documents — the
*same* docs indexed into four `index.codec` configurations, each 1 primary
shard / 0 replicas / force-merged to one segment:

| Index codec | What it is |
|---|---|
| `default` | LZ4 stored-field codec (OpenSearch default) |
| `best_compression` | zlib/DEFLATE stored-field codec |
| **`zstd`** | native zstd codec (OpenSearch 2.9+), stored fields |
| **`zstd_no_dict`** | native zstd without the trained dictionary |

`repository-s3` is not bundled with OpenSearch — install it
(`opensearch-plugin install repository-s3`) and set `s3.client.<name>.region`
(OpenSearch's AWS SDK v2 requires a region; the ES plugin does not).

---

## Compatibility: `--logical-etag` is required

This use case **surfaced and fixed a real S4 interoperability gap.** On a
compressed PUT, S4 returned the backend's MD5 ETag of the *compressed* bytes.
OpenSearch's `repository-s3` (AWS SDK v2) validates each blob upload against
`MD5(original payload)`, so it rejected every blob:

```
repository_verification_exception: [repo] path not accessible
  caused_by io_exception: Unable to upload object [.../master.dat] using a single upload
  caused_by retryable_exception: Data read has a different checksum than expected.
```

(Elasticsearch's `repository-s3` does not validate this way, which is why
[the ES frozen-tier use case](elasticsearch-frozen-tier.md) works without it.)

The fix is S4's opt-in **`--logical-etag`** flag: it presents `MD5(original)`
as the object ETag on PUT / HEAD / GET (and evaluates `If-Match` /
`If-None-Match` against it), so the SDK's integrity check passes. With the flag
on, the full flow works end-to-end:

| Operation through S4 (`--logical-etag`) | Result |
|---|---|
| Repository registration + `_verify` | ✅ |
| Snapshot create | ✅ SUCCESS (was `PARTIAL` without the flag) |
| Restore | ✅ |
| `remote_snapshot` mount + cold search | ✅ |

> **Run S4 with `--logical-etag` for OpenSearch.** It is off by default (it
> costs one MD5 pass per PUT and is unnecessary for clients that don't validate
> upload integrity this way, e.g. Elasticsearch). One documented limitation:
> ETag preconditions on the *write* path (`If-Match` on PUT/CopyObject) are not
> translated — irrelevant to the snapshot flow.

---

## Result 1 — Storage cost

Bytes actually stored in the bucket (what you pay S3 for), vs the same index
snapshotted directly with no S4:

| Index codec | Direct (no S4) | S4 zstd-3 | S4 zstd-9 |
|---|---:|---:|---:|
| `default` (LZ4) | 1484.7 MB | **1067.3 MB (−28.1%)** | 1010.9 MB (−31.9%) |
| `best_compression` (zlib) | 1068.7 MB | **884.6 MB (−17.2%)** | 877.7 MB (−17.9%) |
| **`zstd`** (native) | 1094.1 MB | **909.9 MB (−16.8%)** | 903.1 MB (−17.5%) |
| `zstd_no_dict` | 1116.5 MB | **932.4 MB (−16.5%)** | 913.9 MB (−18.1%) |

**Reading the table — the native-codec nuance**

OpenSearch's `index.codec` (`zstd` / `best_compression`) only compresses the
**stored fields** (including `_source`, usually the largest). The doc values,
postings, term dictionaries and points — often the bulk of a log index — are
untouched by it. So **S4 is complementary, not redundant**: even on a
native-`zstd` index it still finds **~17%** by compressing everything the index
codec leaves alone. The biggest win is on the `default` (LZ4) codec (−28%),
where stored fields are compressed for speed rather than maximum density —
that's "free" savings for any cluster that hasn't switched codecs.

The two layers **compound** (they attack different bytes):

| Configuration | Repository bytes | vs `default` direct |
|---|---:|---:|
| `default`, direct | 1484.7 MB | 1.00× |
| native `zstd`, direct | 1094.1 MB | 1.36× smaller |
| **native `zstd` + S4 zstd-3** | **909.9 MB** | **1.63× smaller** |

> **Dollar intuition** — storage bytes only (request count, transfer, and the
> S4 host are separate). At S3 Standard `$0.023/GB-month`, a 100 TB **pre-S4**
> searchable-snapshot repo at −17% (native zstd + S4) saves ~17 TB ≈
> **$390/month ≈ $4.7k/year**; the `default`-codec −28% case saves ~28 TB ≈
> **$640/month**. Validate with `s4 estimate`.

---

## Result 2 — Searchable-snapshot search performance

Each query was run against the mounted `remote_snapshot` index with the request
cache cleared (`_cache/clear`) before each run. Note OpenSearch's
`remote_snapshot` **file cache is not forcibly purged** by that call, so these
are **cold-ish local-MinIO medians**, not guaranteed cold-from-S3 reads. Median
server-side `took` (ms) of 4 runs, direct vs S4 zstd-3:

| Query | codec | Direct | S4 zstd-3 |
|---|---|---:|---:|
| count, rare term (`status:500`) | default | 1.0 | 1.0 |
| date-histogram + terms agg | default | 29.0 | 27.5 |
| full-text (`message:items`) | default | 1.0 | 1.0 |
| date-histogram + terms agg | `zstd` | 26.0 | 25.0 |
| count / full-text | `zstd` | 0.5 | 0.5 |

**In this local run, S4's server-side `took` stayed within ~1.5 ms of direct on
every query — equal or a hair faster** (the largest gap, the `default`
date-histogram agg, was 27.5 ms through S4 vs 29.0 direct; fewer compressed
bytes to pull before the decode). The decompression cost lands on the cold, rarely-hit
read path, where the searchable-snapshot tier already expects latency. Validate
against your own object-store RTT and cache behaviour — the absolute
milliseconds here are no-RTT local-MinIO values.

> The transferable signal is S4's **relative** overhead (≈ 0 on these queries),
> not the absolute ms; the analogous Elasticsearch path quantifies how that
> relative overhead behaves under injected RTT in
> [the frozen-tier doc](elasticsearch-frozen-tier.md).

---

## When this pays off (and when it doesn't)

**Good fit**
- OpenSearch searchable-snapshot / cold repositories on **`default`-codec**
  indices — the most to compress.
- Workloads dominated by analytics queries over the cold tier — no measurable
  latency cost.
- Apache-2.0 shops wanting compression without a commercial license.

**Think twice**
- Indices already on native `zstd` / `best_compression` — S4 still saves ~17%,
  but measure with `s4 estimate` first.
- Remember `--logical-etag` is required and adds an MD5 pass per PUT.
- Glacier-tier repositories — Glacier already prices low enough that
  compression rarely pays for the compute.

---

## Recommended configuration

```bash
# S4 gateway — note --logical-etag (required for OpenSearch repository-s3)
s4 --endpoint-url https://s3.<region>.amazonaws.com \
   --host 0.0.0.0 --port 8014 \
   --codec cpu-zstd --zstd-level 3 --dispatcher always --logical-etag
```

```yaml
# opensearch.yml — repository-s3 client pointed at S4
s3.client.snap.endpoint: s4.internal:8014
s3.client.snap.protocol: http
s3.client.snap.path_style_access: true
s3.client.snap.region: us-east-1     # OpenSearch's SDK v2 requires a region
```

```bash
opensearch-plugin install repository-s3   # not bundled by default
opensearch-keystore add s3.client.snap.access_key
opensearch-keystore add s3.client.snap.secret_key
curl -XPOST localhost:9200/_nodes/reload_secure_settings
curl -XPUT localhost:9200/_snapshot/snap_repo -H 'Content-Type: application/json' \
  -d '{"type":"s3","settings":{"bucket":"my-snap-repo","client":"snap"}}'
```

For a higher ratio on cold data, snapshot at zstd-3 then consider
`s4 recompact my-snap-repo --target-zstd-level 19 --older-than 7d --execute`
against the backend during a quiet window — estimate and test on your own repo
first (this doc measured only zstd-3 and zstd-9 on OpenSearch; the ES frozen-tier
doc has measured zstd-19 recompact numbers).

---

## Reproduce

Harness: [`benches/opensearch-searchable/`](../../benches/opensearch-searchable/)
(stand up MinIO + S4 + OpenSearch locally, index 4M docs into the four codecs,
snapshot through each repo, measure stored bytes + searchable-snapshot cold
search). All measurements: AMD Ryzen 9 9950X, OpenSearch 2.19.5, MinIO
RELEASE.2025-09-07, S4 v1.2.2 (`--logical-etag`), local, 2026-06-19. Storage
figures are bytes stored on the backend; request/egress unchanged by S4.

---

*See also: [#1 Elasticsearch frozen tier](elasticsearch-frozen-tier.md) ·
[savings & `s4 estimate`](../savings.md) · [compatibility](../compatibility.md).*
