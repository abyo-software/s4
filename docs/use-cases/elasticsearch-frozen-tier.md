# Use case: S4 as an Elasticsearch frozen-tier backend

> **Series:** S4 use cases · **#1 — Elasticsearch frozen tier**
> **Status:** measured locally, end-to-end. Numbers below come from a real
> Elasticsearch 9.4.2 cluster snapshotting through S4 v1.2.2 into MinIO — not
> a codec micro-benchmark. Reproduce script + raw JSON in
> [§ Reproduce](#reproduce).

Elasticsearch's **frozen tier** keeps shard data in an S3 *snapshot
repository* and mounts it as a [searchable
snapshot](https://www.elastic.co/guide/en/elasticsearch/reference/current/searchable-snapshots.html)
backed by a bounded local cache. The data you pay S3 to store is the set of
**snapshot blobs** (Lucene segment files + repository metadata). S4 sits
*between* Elasticsearch's `repository-s3` client and your bucket and
transparently compresses those blobs — so the frozen tier stores fewer bytes
with **no change to Elasticsearch, the query API, or your clients**.

```
  Elasticsearch          S4 gateway            your S3 bucket
  (frozen tier)   ──▶   (compress)     ──▶    (snapshot blobs, fewer bytes)
  repository-s3          ▲
   snapshot/restore      └── frozen search range-GETs blobs back;
   + searchable mount        S4 decompresses, ES sees original bytes
```

S4 is **complementary** to Elasticsearch here — it compresses the repository
the frozen tier already uses; it is not a replacement for any Elasticsearch
component.

---

## TL;DR

On a 4-million-document structured-log index (1 shard, force-merged to a
single segment — the realistic pre-frozen state), snapshotted to a real S3
repository:

| Metric | Result |
|---|---|
| **Repository storage saved** (S4 zstd-3, the default PUT codec) | **−15–27%**: −27% on the default codec (its max), −15% on `best_compression`, −22% on LogsDB (so −15–22% once you've already tuned the codec/mode) |
| **Snapshot throughput** | S4 sustains 186–241 MB/s per shard stream at zstd-3 — **6× above** Elasticsearch's default 40 MB/s snapshot throttle, so **no added wall-clock** in normal operation |
| **Restore throughput** | S4 decode sustains ~780–870 MB/s unthrottled; at ES's default 40 MB/s recovery throttle S4 is invisible (41.0 vs 41.8 MB/s direct) |
| **Frozen search latency** (count / agg / full-text, cold cache) | **2–4 ms, S4 within ±1 ms of direct** (often equal or faster) |
| **Frozen search latency** (heavy cold `sort + fetch top-N`) | dominated by the frozen tier's own cold block-fetch; S4 adds **+6.5% to +9.5%** |
| **Compatibility** | all 4 repositories pass ES `_verify`; snapshot / restore / frozen mount / cold search all work end-to-end through S4 |
| **Compounding** | LogsDB **+** S4 zstd-9 = **510.6 MB** vs a plain standard-default repo at **1440.8 MB** → **2.82× smaller** total footprint |

**Bottom line:** S4 at its default `zstd-3` cuts a frozen-tier repository by
15–27% for free on the write path, with no measurable hit to the *analytics*
queries that dominate frozen workloads. The **−27%** figure is the
*zero-migration upside* for the (very common) clusters still on the default
codec — you keep your existing index settings and just point the repository at
S4. If you have already tuned to `best_compression` or LogsDB, S4 still adds
−15–22% on top. Push the cold repository to zstd-19 with `s4 recompact` for the
maximum ratio without ever slowing a snapshot.

> **Scope.** This is **one** S4 use case. The general thread is broader — S4 is
> a *range-GET-safe transparent compression gateway* for any S3 client (the
> searchable-snapshot range-GET surface is just a demanding instance of it), and
> [S4 Query](../benchmarks.md) pushes predicate/aggregation work down to the
> gateway. The frozen tier is a clean, end-to-end-measurable proof point, not
> the whole story.

---

## How the frozen tier uses S3 (and why compression helps)

1. **ILM rolls an index into the frozen phase** → Elasticsearch takes a
   snapshot into the `repository-s3` repository and **mounts** it as a
   partially-cached searchable snapshot (`storage: shared_cache`).
2. The shard's bytes now live in the bucket as snapshot blobs. The frozen node
   keeps only a bounded on-disk **shared cache**
   (`xpack.searchable.snapshot.shared_cache.size`); everything else is fetched
   from the repository **on demand** via S3 range-GET.
3. A query against a frozen index range-GETs exactly the blocks it needs,
   populates the cache, and answers from there.

S4 compresses the blobs on the way in (snapshot PUT / multipart) and
decompresses the range-GETs on the way out using the per-object **S4IX
sidecar** so only the covering compressed bytes are fetched. Elasticsearch
sees byte-identical data; the bucket holds fewer bytes.

Because the frozen tier is explicitly the *cold, not-latency-critical* tier,
it sits squarely in S4's sweet spot — the decompression cost is paid on rare
cold fetches, not on a hot OLTP read path.

---

## The benchmark

Everything ran **locally, end-to-end** (no AWS billing) so the storage,
throughput **and** search-latency dimensions are all measured against a live
cluster. Storage figures are **bytes actually written to the object store**;
the dollar figures below are derived from those bytes at public S3 Standard
pricing.

**Environment**

| Component | Version / spec |
|---|---|
| Host | AMD Ryzen 9 9950X (16 cores / 32 threads), Linux |
| Elasticsearch | `docker.elastic.co/elasticsearch/elasticsearch:9.4.2`, single node, 6 GB heap, trial license (frozen tier) |
| Object store (backend) | `minio/minio` (RELEASE.2025-09-07), local |
| S4 | v1.2.2 (built from `main`), `--codec cpu-zstd --dispatcher always`, one instance per zstd level |
| Frozen cache | `xpack.searchable.snapshot.shared_cache.size: 4gb` |

**Topology** — one MinIO backend, four S3 snapshot repositories:

| Repository | Path | zstd level |
|---|---|---|
| `repo_direct` | ES → MinIO directly (baseline) | — (no S4) |
| `repo_s4z3` | ES → S4 → MinIO | 3 (S4 default) |
| `repo_s4z9` | ES → S4 → MinIO | 9 |
| `repo_s4z19` | ES → S4 → MinIO | 19 |

**Dataset** — 4,000,000 ECS-style structured web/access-log documents
(timestamp, host, service, level, HTTP method/path/status/bytes, duration,
source IP, user, user-agent, trace/span id, free-text message). The **same
documents** were indexed into three index configurations, each 1 primary shard,
0 replicas, **force-merged to a single segment**:

| Index configuration | What it tests |
|---|---|
| standard, default codec | ES's default `LZ4` stored-field codec |
| standard, `index.codec: best_compression` | ES's `DEFLATE` stored-field codec |
| **LogsDB** (`index.mode: logsdb`) | ES's densest log mode — synthetic `_source`, sorted, specialized doc-value codecs |

On-disk after force-merge: standard-default **1374 MiB**, best_compression
**1009 MiB**, LogsDB **741 MiB** — i.e. LogsDB is already ~1.9× denser than
standard before S4 ever sees a byte. That fact drives the LogsDB result below.

**Method** — for the cost matrix, each (index × repository) pair was
snapshotted onto a freshly-emptied bucket and the **actual bytes stored on
MinIO** (compressed blobs + `.s4index` sidecars + repo metadata) were measured
directly on the backend. Throughput was measured **with the throttles lifted**
so S4's own ceiling is visible rather than Elasticsearch's defaults — two
*different* knobs cap it: the repository's `max_snapshot_bytes_per_sec`
(default 40 MB/s) for snapshots, and the cluster `indices.recovery.max_bytes_per_sec`
(default 40 MB/s) for restores (the repo's `max_restore_bytes_per_sec` defaults
to unlimited and is *not* the restore bottleneck). Frozen search latency cleared
the shared cache before every query to force a cold fetch from the repository.

---

## Result 1 — Storage cost

Bytes actually stored in the bucket (this is what you pay S3 for), and the
saving vs the same index snapshotted **directly** to S3 with no S4:

| Index configuration | Direct (no S4) | S4 zstd-3 | S4 zstd-9 |
|---|---:|---:|---:|
| standard, default codec | 1440.8 MB | **1051.2 MB (−27.0%)** | 995.7 MB (−30.9%) |
| standard, `best_compression` | 1057.6 MB | **901.3 MB (−14.8%)** | 895.2 MB (−15.4%) |
| LogsDB | 660.9 MB | **514.3 MB (−22.2%)** | 510.6 MB (−22.7%) |

**Reading the table**

- **Default-codec indices have the most to give** — ES only LZ4-compresses
  stored fields by default, leaving postings, doc values and term dictionaries
  for S4 to squeeze: **−27%** at zstd-3 alone.
- **`best_compression` overlaps with S4** — ES has already DEFLATE-compressed
  the stored fields, so S4 finds less (−15%). Still free savings, but the
  marginal win is smaller because two compressors are chasing the same bytes.
- **LogsDB lands in between (−22%)** and is the most interesting case — see
  next section.

### The LogsDB nuance

LogsDB is Elasticsearch's purpose-built dense log mode: synthetic `_source`
(no stored source blob), index sorting, and specialized doc-value codecs. In
this benchmark a LogsDB index was **2.18× smaller** as a repository than the
same data as standard-default (660.9 MB vs 1440.8 MB) *before any S4
compression*.

That changes what S4 adds, in two ways worth understanding:

- **S4's *percentage* gain on LogsDB (−22%) is smaller than on standard-default
  (−27%)**, because LogsDB has already removed the easiest redundancy (the
  stored `_source`). There is simply less slack left.
- **But it still beats `best_compression` (−15%)** — LogsDB drops `_source`
  rather than DEFLATE-ing it, so its doc-value and postings layout still
  carries zstd-compressible structure that S4 captures.

The two techniques **compound** — they attack different bytes:

| Configuration | Repository bytes | vs standard-default direct |
|---|---:|---:|
| standard-default, direct | 1440.8 MB | 1.00× |
| LogsDB, direct | 660.9 MB | 2.18× smaller |
| **LogsDB + S4 zstd-9** | **510.6 MB** | **2.82× smaller** |

If you are choosing between "switch to LogsDB" and "add S4," the honest answer
is **do both**: LogsDB shrinks what Elasticsearch writes, S4 shrinks what S3
stores, and the savings stack.

### Break-even (does S4 pay for its own host?)

S4 saves storage but runs as a separate host — a line item the byte counts
above don't include. The break-even footprint, with the host cost made explicit
and parameterised, is:

```
break_even_TB = (host_$/month × instances) / (saved_ratio × $23/TB-month)
```

where `saved_ratio` is the **measured** repository saving from Result 1 (not a
codec micro-benchmark) and `$23/TB-month` is S3 Standard (`$0.023/GB-month`,
us-east-1). At an illustrative **$70/month per S4 host** the break-even sizes
are (model: [`benches/.../breakeven.py`](../../benches/elasticsearch-frozen/breakeven.py),
raw: [`results/breakeven.json`](../../benches/elasticsearch-frozen/results/breakeven.json)):

| Codec / mode | saved_ratio | break-even, 1 instance | break-even, **HA (2 instances)** |
|---|---:|---:|---:|
| standard-default (zstd-3) | 27.0% | ~11 TB | **~23 TB** |
| LogsDB (zstd-3) | 22.2% | ~14 TB | ~27 TB |
| `best_compression` (zstd-3) | 14.8% | ~21 TB | ~41 TB |
| standard-default after `recompact` → zstd-19 | 33.2% | ~9 TB | ~18 TB |

So: **for a standard-default frozen tier, any repository larger than ~23 TB is
net-positive even with two HA gateway instances** (~11 TB with a single
instance). Net savings at scale, HA (2 instances), $70/host:

| Footprint | standard-default | LogsDB | `best_compression` |
|---|---:|---:|---:|
| 500 TB | ≈ **+$2,965/mo** (≈ $35.6k/yr) | ≈ +$2,413/mo | ≈ +$1,562/mo |
| 1 PB (1000 TB) | ≈ **+$6,070/mo** (≈ $72.8k/yr) | ≈ +$4,966/mo | ≈ +$3,264/mo |

> Storage bytes only. S4 also writes one small `.s4index` sidecar per blob (no
> *extra* backend round-trip per cold query — see Result 4 / B2 — and negligible
> bytes); client-visible egress is unchanged (GET returns the original bytes).
> Re-run `breakeven.py --s4-host-usd-month <your $> --instances <n>` with your
> real host price, or plug your footprint into [`s4 estimate`](../savings.md).

---

## Result 2 — Compression levels (zstd-3 / 9 / 19) and `s4 recompact`

S4's PUT path defaults to **zstd-3** because it favours latency — and on this
workload the level curve has steep diminishing returns:

| Level | standard-default saved | LogsDB saved | Single-shard snapshot throughput |
|---|---:|---:|---:|
| zstd-3 (default) | −27.0% | −22.2% | 186–241 MB/s |
| zstd-9 | −30.9% | −22.7% | 72–120 MB/s |
| zstd-19 | see below (via `recompact`) | | — (not on the live path) |

zstd-9 buys only ~4 extra points on standard-default and **<1 point** on
LogsDB, for 2–3× the CPU. zstd-3 is the right default for the snapshot path.

### zstd-19 belongs to `s4 recompact`, not the live snapshot

Driving a snapshot **directly** through an S4 gateway pinned to `zstd-19`
**failed** in this benchmark — every zstd-19 snapshot came back `PARTIAL`:

```
IOException[Unable to upload or copy object ... using multipart upload];
  nested: NoHttpResponseException[The target server failed to respond]
```

The root cause is precise and expected: compressing a multi-MB Lucene
multipart part at level 19 takes **longer than S4's 30-second per-connection
"slowloris" guard** (`--read-timeout-seconds`, default 30s), so S4 closes the
connection and `repository-s3` sees a dropped upload. This is *by design* —
S4's PUT path is tuned for latency, which is exactly why the default is zstd-3.
(Gateway log + full snapshot failure reason:
[`results/zstd19-slowloris-evidence.txt`](../../benches/elasticsearch-frozen/results/zstd19-slowloris-evidence.txt).)

The intended way to reach zstd-19 is **`s4 recompact`**: snapshot fast at
zstd-3, then rewrite the cold repository to zstd-19 in the background,
backend-direct (no client connection, no timeout):

```bash
# 1) snapshot through S4 at the zstd-3 default (fast)            -> 1051.2 MB
# 2) bake the cold repo bucket to zstd-19, backend-direct:
s4 recompact repo-s4z3 --endpoint-url https://s3.example.com \
   --target-zstd-level 19 --older-than 7d --execute
```

Measured on the standard-default repository:

| Stage | Repository bytes | Saved vs direct |
|---|---:|---:|
| direct (no S4) | 1440.8 MB | — |
| S4 zstd-3 snapshot | 1051.2 MB | −27.0% |
| **after `s4 recompact` → zstd-19** | **962.0 MB** | **−33.2%** |

`recompact` is selective: of the 27 repository objects it rewrote only the
**14** blobs that shrank by more than its `--min-gain-percent` (default 3%) and
skipped the other 13 as `insufficient-gain` — it won't burn CPU re-compressing
bytes that are already dense at zstd-3. It rewrites each blob in place (same
key, refreshed sidecar) and is **transparent to Elasticsearch** — after
recompaction the snapshot still restored cleanly and returned all
**4,000,000** documents, read back through S4 (every byte round-trip-verified
by `recompact` before it writes).

> If you genuinely must run zstd-19 inline (you don't, on a snapshot path),
> raise `--read-timeout-seconds` to fit your largest part's compress time —
> but you will be trading snapshot wall-clock for a few percent of ratio that
> `recompact` gets you for free off the critical path.

### ⚠️ Run `recompact` in a quiet window — never concurrently with the repo

`recompact` rewrites repository blobs **in place** (same key). It re-checks the
source object's ETag with a HEAD immediately before each overwrite PUT, **but S3
has no compare-and-swap**, so a writer that lands in the HEAD→PUT window is
*silently overwritten*. Elasticsearch mutates the same repository objects during
a snapshot finalize and during `POST _snapshot/<repo>/_cleanup`. Therefore:

- **Do not run `s4 recompact` while an ES snapshot or `_cleanup` is in flight on
  the same repository.** A collision can lose an ES write or have `recompact`
  overwrite an object ES just changed — corrupting the repository.
- **`--older-than` is a mitigation, not a guarantee.** It narrows which blobs
  `recompact` touches by age, but it does *not* serialize against a concurrent
  snapshot, and ES rewrites repository metadata (`index-N`) on every snapshot
  regardless of blob age. The TOCTOU window is real and silent.
- **Recommended:** schedule `recompact` inside an **exclusive quiet window** —
  pause ILM rollovers into the repo, confirm no snapshot or `_cleanup` is
  running, then recompact backend-direct. (S4 documents this hazard itself; see
  [`results/recompact-concurrency.json`](../../benches/elasticsearch-frozen/results/recompact-concurrency.json),
  which is `documented-not-tested` precisely because there is no safe way to make
  a concurrent run safe on plain S3.) `recompact` also resets per-object ACLs and
  Object Lock retention/legal-hold to bucket defaults on each rewrite, so don't
  recompact a repository that relies on those.

---

## Result 3 — Throughput

**Snapshot (write) throughput.** Unthrottled, a single shard stream sustained
**186–241 MB/s through S4 at zstd-3** vs ~0.5–0.9 GB/s writing directly to a
co-located MinIO. So at unlimited rate S4 is the narrower pipe — but it is
still **~6× faster than Elasticsearch's default `max_snapshot_bytes_per_sec`
of 40 MB/s**. In any cluster running the default snapshot throttle (almost all
of them), **S4 adds no measurable snapshot wall-clock** — the throttle, not S4,
is the bottleneck. We confirmed the throttle directly: the same 1.44 GB
snapshot took 34.6 s at the 40 MB/s default vs 1.6 s unthrottled.

**Restore (read) throughput.** Restoring the 1.44 GB index with the node
recovery throttle lifted, so S4's decode cost — not the throttle — sets the
pace:

| Source | Restore time | Throughput |
|---|---:|---:|
| direct (no S4) | 0.5 s | ~2900 MB/s (MinIO-read-bound, no decode) |
| S4 zstd-3 | 1.66 s | **869 MB/s** |
| S4 zstd-9 | 1.85 s | 778 MB/s |

S4's decompression caps restore at **~780–870 MB/s** here — well above any
realistic recovery rate. And in practice you don't lift the throttle: restore
is bounded by `indices.recovery.max_bytes_per_sec` (default 40 MB/s), at which
S4 is **invisible** — with the default throttle in place the same restore ran
**41.8 MB/s direct vs 41.0 MB/s through S4** (a 1.6% delta, inside the noise).
Either way, S4 decode never becomes the restore bottleneck.

---

## Result 4 — Frozen search performance (cold cache)

Each query was run against the mounted frozen index with the shared cache
**cleared before each run**, so any repository blocks a query needs are
fetched cold through S4. (The B2 op-count below shows the lightweight
analytics queries here were answered *without* a backend GET in this run —
only the heavy top-N fetch actually pulls blocks cold.) Median of 6 cold runs
(server-side `took`, ms); warm = cache populated:

> **Read the absolute milliseconds with care.** These were measured against a
> **co-located MinIO with effectively zero network RTT**, so the absolute values
> (2–4 ms for analytics; 1.7–7.2 s for the cold top-N fetch) are a property of
> *this host*, not of any real S3 deployment with tens of milliseconds of
> round-trip. The **transferable** metric is S4's **relative** overhead — the
> last column below — which is what a real deployment inherits. The
> RTT-injection table further down shows how that relative overhead behaves as
> the backend leg gets slower.

| Query | Index | Direct cold | S4 zstd-3 cold | Direct warm | S4 warm |
|---|---|---:|---:|---:|---:|
| count, rare term (`status:500`) | standard | 3.0 | 2.0 | 1.5 | 1.0 |
| date-histogram + terms agg | standard | 4.0 | 3.0 | 2.0 | 1.0 |
| full-text (`message:items`) | standard | 3.0 | 2.0 | 1.5 | 1.0 |
| count, rare term | LogsDB | 2.0 | 2.0 | 2.0 | 1.0 |
| date-histogram + terms agg | LogsDB | 2.0 | 2.5 | 1.0 | 1.0 |
| **top-20 + sort by `@timestamp`** | standard | 1714 | 1877 (**+9.5%**) | 8.5 | 7.0 |
| **top-20 + sort by `@timestamp`** | `best_comp` | 2328 | 2517 (**+8.1%**) | 9.5 | 9.0 |
| **top-20 + sort by `@timestamp`** | LogsDB | 7188 | 7658 (**+6.5%**) | 5.0 | 4.5 |

**Reading the table**

- **The analytics queries that dominate frozen workloads** — counts,
  aggregations, full-text filters — are **2–4 ms cold, with S4 within ±1 ms of
  direct** (and often *faster*, because there are fewer compressed bytes to pull
  before the decode). These touch a handful of doc-value / postings blocks; the
  S4 decompression is in the noise.
- **The one expensive case is a cold `sort + fetch top-N raw documents`**,
  which touches blocks scattered across the whole shard. Here the cost is
  dominated by the **frozen tier's own cold block-fetch** (1.7 s on standard,
  **7.2 s on LogsDB** — LogsDB must rebuild `_source` from many doc-value
  columns on a cold read). S4 adds **+6.5% to +9.5%** on top of that already-
  large number. Warm, both are single-digit ms.

The takeaway: S4's read-path overhead is small and lands precisely where the
frozen tier is *already* slow and rarely-hit (cold raw-document fetches), not
on the cheap analytics queries you actually run against frozen data.

### How that overhead behaves under real backend RTT

Because the numbers above are no-RTT, we re-ran the cold queries with a one-way
delay injected on the S4↔backend path (toxiproxy, 0 / 5 / 20 / 50 ms one-way),
direct and S4 zstd-3 both eating the same RTT, cache cleared each time. This is a
**separate run** (toxiproxy in-path), so its own `0 ms` baseline (+7.1% top-N
overhead) differs slightly from the headline 2026-06-18 table's +9.5% — read the
columns as *the same run's* overhead growing with RTT, not against the headline
number. Relative overhead, standard-default index (raw:
[`results/rtt-injection.json`](../../benches/elasticsearch-frozen/results/rtt-injection.json)):

| Query | 0 ms | 5 ms | 20 ms | 50 ms |
|---|---:|---:|---:|---:|
| analytics (count / agg / full-text) | ±0–1 ms | ±0–1 ms | ±0–1 ms | ±0–1 ms |
| **top-20 + sort** (relative S4 overhead) | +7.1% | +9.2% | +32.9% | +69.8% |

Two honest, opposite results:

- **The analytics queries stay flat** — they are answered without re-fetching
  from the repository, so injected RTT barely touches them and S4 stays within
  ±1 ms at *every* latency. This is the RTT-invariant, transferable win.
- **The cold top-N+sort overhead *grows* with RTT** — and this is a real caveat,
  not noise. That query issues several backend GETs, some of them
  *sidecar-partial* (S4 consults the `.s4index` to fetch only the covering
  compressed range), and each extra round-trip costs a full RTT. So on a real,
  high-latency S3 the heavy raw-document fetch pays *more* than the local
  +6.5–9.5% — budget for it if your frozen workload is dominated by cold
  `sort + fetch top-N` rather than analytics.

> **Does the sidecar add cold-path requests?** We counted backend GET ops per
> cold query (S4 vs a no-sidecar passthrough baseline:
> [`results/sidecar-overhead.json`](../../benches/elasticsearch-frozen/results/sidecar-overhead.json)).
> On this workload S4 issues the **same** number of backend GETs as the baseline
> and **no separate `.s4index`-keyed GET** per query — it folds the index into
> each data GET to fetch a smaller covering range (`path="sidecar-partial"`),
> rather than adding a round-trip. The analytics queries issue 0 backend GETs
> (ES answers them without a repo fetch); warm queries issue 0 in either arm.
> The RTT sensitivity above comes from the *number* of those GETs on the heavy
> fetch, not from an extra sidecar request.

---

## Availability & HA

S4 is a **read-path hard dependency** for cold frozen search: a query that
misses the shared cache must range-GET its blocks *through* S4 to decompress
them, so **a single S4 gateway is a single point of failure** for cold frozen
queries (warm queries already in the shared cache are unaffected; so is anything
not currently reading from the repository). Be honest about that and design for
it — don't run one gateway in front of a frozen tier you care about.

The fix is straightforward because **S4 instances are stateless**: the
`.s4index` sidecars live in the object store next to the blobs, not on the
gateway, so any instance can serve any request. Run **two or more S4 instances
behind a load balancer that health-checks and routes to healthy upstreams**, and
a dead instance is routed around. (Multi-value DNS can also work, but DNS
caching / stale pooled connections / per-IP retry behaviour in the ES JVM S3
client are environment-specific — validate that path in your own setup; what we
measured below is the load-balancer case.)

We smoke-tested exactly that (B4 below: 2 stateless S4 instances behind an nginx
round-robin upstream; raw:
[`results/ha-failover.json`](../../benches/elasticsearch-frozen/results/ha-failover.json)).
Registering the repository through the LB and then **killing one instance**, the
cold query and snapshot PUT were served through the survivor, and the warm query
stayed unaffected (it did not need the repository at all):

| Check after killing one of two instances | Result |
|---|---|
| Cold frozen query (must hit the survivor) | ✅ succeeded (79,925 hits) |
| Warm frozen query (served from the shared cache) | ✅ unaffected (3 ms) |
| Snapshot PUT issued while one instance is down | ✅ completed |

One LB caveat worth stating: AWS SigV4 signs the `Host` header, so the load
balancer must **preserve the client Host** (`proxy_set_header Host $http_host`
in nginx) — rewriting it to the upstream name returns `403
SignatureDoesNotMatch`. (S4 still re-signs to the backend with its own
credentials.)

---

## Compatibility — what was exercised

Everything Elasticsearch's `repository-s3` plugin does against the frozen tier
was driven through S4 with no errors (except the documented zstd-19/slowloris
interaction in Result 2):

| Operation | Through S4 |
|---|---|
| Repository registration + `_verify` (write/read/delete probe) | ✅ all 4 repos pass |
| Snapshot create (single-PUT + multipart blobs) | ✅ |
| Snapshot restore (full read + decompress) | ✅ |
| Searchable-snapshot **frozen mount** (`storage: shared_cache`) | ✅ |
| Cold frozen search (on-demand range-GET via `.s4index` sidecar) | ✅ |
| Snapshot still valid **after `s4 recompact`** (in-place rewrite) | ✅ restored 4,000,000 docs |
| **HA failover** — repo registered through an LB over 2 stateless instances, one killed | ✅ cold + warm query + snapshot PUT survive (see [Availability & HA](#availability--ha)) |

This matches S4's [compatibility matrix](../compatibility.md): full-spec Range
GET, multipart, HEAD and conditional requests are exactly the `repository-s3`
surface.

---

## When this pays off (and when it doesn't)

**Good fit**

- Frozen / cold tiers holding **standard or default-codec** indices — the most
  to compress.
- Large frozen repositories where 15–27% of the S3 bill is real money.
- Workloads dominated by **analytics queries** (dashboards, aggregations,
  filtered counts) over frozen data — no measurable latency cost.

**Think twice**

- **`best_compression` indices** — S4 still saves ~15%, but you are partly
  double-compressing; measure with [`s4 estimate`](../savings.md) first.
- **Cold workloads dominated by raw-document retrieval / sorting** — the frozen
  tier is already slow there, S4 adds single-digit-percent locally, and that
  relative overhead **grows with backend RTT** (see Result 4's RTT table). If
  your frozen access pattern is mostly cold `sort + fetch top-N` rather than
  analytics, weigh that.
- **Glacier / Deep Archive snapshot repositories** — Glacier already prices low
  enough that compression rarely pays for the compute; see
  [storage-class transitions](../storage-class-transitions.md) and keep the
  `.s4index` sidecar in the same class as its blob. This is also where **Elastic
  Deepfreeze** (which rotates frozen snapshots down into Glacier-class storage)
  is the better tool — for *truly* cold data you rarely search, Glacier-class
  pricing beats keeping bytes in Standard and compressing them. The two are
  **complementary layers, not alternatives**: Deepfreeze wins for archive-cold
  data; **S4's sweet spot is the "warmish-frozen" data you keep in Standard
  because it still gets search traffic**, where Glacier's retrieval latency/cost
  would hurt but a 15–27% Standard saving is real money. Use Deepfreeze to push
  the genuinely-cold tail to Glacier and S4 to shrink what stays in Standard.
- **Hot/warm tiers** — those are latency-critical; S4's frozen sweet spot is
  the cold path. (S4 never makes hot reads *wrong*, just adds decode latency
  you don't want on a hot path.)

> **LogsDB shelf-life.** The LogsDB column here reflects Elasticsearch as of
> 9.4.2; LogsDB's density (synthetic `_source`, sorted columns) is still
> evolving release to release, so re-measure with `s4 estimate` on your own ES
> version rather than treating the −22% as a fixed constant — the *relative*
> ordering (LogsDB already dense, S4 adds less than on default-codec) is the
> durable takeaway, the exact percentage is not.

---

## Recommended configuration

**S4 gateway** (CPU is plenty — no GPU needed for log/segment data):

```bash
s4 --endpoint-url https://s3.<region>.amazonaws.com \
   --host 0.0.0.0 --port 8014 \
   --codec cpu-zstd --zstd-level 3 --dispatcher always
```

For anything you care about, run **two or more of these** behind a health-
checking load balancer — S4 is a read-path hard dependency for cold frozen
search, and instances are stateless (sidecars live in S3), so this removes the
SPOF with no extra coordination. See [Availability & HA](#availability--ha).

**Elasticsearch** — point the `repository-s3` client at S4 (or its LB) instead
of S3. Endpoints live in `elasticsearch.yml`; credentials in the keystore:

```yaml
# elasticsearch.yml — point at one S4 instance, or the HA load balancer's address
s3.client.frozen.endpoint: s4.internal:8014
s3.client.frozen.protocol: http          # or https if S4 terminates TLS
s3.client.frozen.path_style_access: true
```

```bash
bin/elasticsearch-keystore add s3.client.frozen.access_key
bin/elasticsearch-keystore add s3.client.frozen.secret_key
# reload without restart:
curl -XPOST localhost:9200/_nodes/reload_secure_settings
```

```bash
# register the repository against S4
curl -XPUT localhost:9200/_snapshot/frozen_repo -H 'Content-Type: application/json' -d '{
  "type": "s3",
  "settings": { "bucket": "my-frozen-repo", "client": "frozen" }
}'
```

Then run ILM exactly as before. For maximum ratio, schedule `s4 recompact
my-frozen-repo --target-zstd-level 19 --older-than 7d --execute` against the
**backend** during a quiet window.

---

## Reproduce

The full harness — data generator, index builder, the four measurement phases,
and the raw JSON results from this run — lives in
[`benches/elasticsearch-frozen/`](../../benches/elasticsearch-frozen/) with a
copy-paste runbook. It runs entirely against local MinIO (no AWS account). The
exact steps used for the numbers on this page:

1. `docker run` MinIO + Elasticsearch 9.4.2 (single node, `shared_cache.size:
   4gb`, trial license); run three or four S4 instances at different
   `--zstd-level`s, each `--dispatcher always`, all pointing at MinIO.
2. Register one `repository-s3` per endpoint; ES `_verify` each.
3. Index 4M ECS-style docs into standard / `best_compression` / LogsDB indices;
   force-merge each to 1 segment.
4. **Phase A** — snapshot each index × each repo onto a fresh bucket; measure
   stored bytes on the backend + snapshot wall-clock.
5. **Phase B** — frozen-mount each, clear the shared cache, time cold queries.
6. **Phase C** — full restore timing per repo.
7. **Phase D** — `s4 recompact` the zstd-3 repo to zstd-19 and re-verify the
   restore.

The 2026-06-19 revision adds four non-destructive phases (raw JSON + a one-page
[`results/REVISION-NOTES.md`](../../benches/elasticsearch-frozen/results/REVISION-NOTES.md)):
**B1** cold latency under injected backend RTT (`phase_b1_rtt.{py,sh}`), **B2**
`.s4index` sidecar cold-path backend-op count (`phase_b2_sidecar.py`), **B3** the
parameterised break-even model (`breakeven.py`), **B4** an HA failover smoke test
with 2 stateless instances behind an LB (`phase_b4_ha.sh`), and **B5** the
`documented-not-tested` recompact-concurrency note.

Storage / snapshot / restore / latency numbers above are the **2026-06-18**
baseline run: AMD Ryzen 9 9950X, ES 9.4.2, MinIO RELEASE.2025-09-07, S4 v1.2.2,
local (no network RTT). Storage figures are **bytes stored on the backend**;
dollar figures are estimates at public S3 Standard pricing — the break-even
model's host price is an explicit `--s4-host-usd-month` parameter, not a billed
measurement. S4 adds a small per-blob `.s4index` sidecar (negligible bytes, and
*no* extra cold-path backend round-trip per query — B2) and runs as a separate
host (modeled in the break-even section, not in the raw byte counts).

---

*See also: [savings & `s4 estimate`](../savings.md) ·
[compatibility matrices](../compatibility.md) ·
[storage-class transitions](../storage-class-transitions.md) ·
[full codec benchmarks](../benchmarks.md).*
