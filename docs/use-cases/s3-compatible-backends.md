# Cut storage costs on S3-compatible object stores: MinIO, Cloudflare R2, Backblaze B2, Wasabi

> **Series:** S4 use cases · **#6 — S3-compatible backends**
> **Status:** MinIO is the **CI-verified** backend — every per-PR run exercises
> the "E2E (MinIO Docker)" job, the weekly
> [compat-matrix workflow](../../.github/workflows/compat-matrix.yml) round-trips
> against it, and every measured benchmark in this series (#1–#5) ran against a
> MinIO backend. **Cloudflare R2, Backblaze B2, and Wasabi are wire-compatible
> targets that this project has *not* yet validated against** — their sections
> below say exactly why they should work, what to verify first, and how to run
> the same smoke test our CI runs.
> Companion to [#1 Elasticsearch frozen tier](elasticsearch-frozen-tier.md) /
> [#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md) /
> [#3 Grafana Loki chunks](grafana-loki-chunks.md) /
> [#4 Kafka tiered storage](kafka-tiered-storage.md) /
> [#5 Cold Parquet recompaction](cold-parquet.md).

How do you reduce storage usage on a MinIO cluster, or shrink a Cloudflare R2 /
Backblaze B2 / Wasabi bill, without changing your applications? Put a
compression gateway in front of the bucket. **S4** is an S3-compatible gateway
that transparently compresses every object on the way in and decompresses it on
the way out — your apps keep speaking the ordinary S3 API to S4, and the
backend stores **50–80% fewer bytes on compressible data** (logs, JSON,
columnar — the [README](../../README.md)'s published planning range; measured
application-format results in this series ran −15% to −75% depending on what
the application had already compressed). Nothing about S4 is AWS-specific: the
backend is just an `--endpoint-url`, and everything S4 needs is API surface
that MinIO, R2, B2, and Wasabi all advertise.

```
 your apps (unchanged)           S4 gateway                 any S3-compatible store
 boto3 / aws-cli / Spark ──▶   compress on PUT    ──▶      MinIO · R2 · B2 · Wasabi
 Loki / Kafka / OpenSearch     decompress on GET           (fewer stored bytes)
                                  ▲
                                  └── clients keep seeing the original bytes,
                                      original Content-Length, and MD5 ETag
```

S4 is **complementary** to these stores — it compresses into the bucket you
already run or rent; it is not a replacement for MinIO, R2, B2, or Wasabi.

---

## What S4 needs from an S3-compatible backend

S4's backend client is the plain `aws-sdk-s3` Rust SDK pointed at your
`--endpoint-url`, always in **path-style addressing**
(`force_path_style(true)` is hard-wired), with credentials from the standard
AWS environment chain (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
`AWS_REGION` / `AWS_PROFILE`). The operations S4 issues against the backend:

- **Object I/O:** `PutObject`, `GetObject` (incl. ranged reads of its own
  framed objects), `HeadObject`, `DeleteObject`, `CopyObject`
- **Multipart:** `CreateMultipartUpload`, `UploadPart`,
  `CompleteMultipartUpload`, `AbortMultipartUpload`, `ListParts`
- **Listing:** `ListObjectsV2` (and `ListBuckets` for `/ready` checks)
- **Sidecars:** compressed objects get a small `<key>.s4index` sidecar object —
  a second backend `PutObject` per compressed write (this matters for
  per-operation billing; see the R2 section)

Everything else in S4's [45+-operation S3 surface](../compatibility.md)
(versioning, lifecycle, tagging, ACLs, …) is served or delegated at the
gateway. If a backend handles the list above correctly, S4 can front it. The
authoritative per-backend verification posture lives in the
[backend compatibility matrix](../compatibility.md#backend-compatibility-matrix)
— what CI actually exercises, not "should work" claims.

One concrete reason we insist on testing rather than asserting: the matrix's
Garage row documents a real round-trip failure from
`STREAMING-AWS4-HMAC-SHA256-PAYLOAD` signature drift between `aws-sdk-rust`
and Garage v1.1.0. Streaming-signature and checksum-header handling is exactly
the kind of wire shape that varies across S3-compatible implementations — which
is why the R2 / B2 / Wasabi sections below come with a checklist instead of a
promise.

---

## Reduce MinIO storage usage with transparent compression (CI-verified)

MinIO is S4's most-tested backend, full stop:

- **Per-PR CI** runs the "E2E (MinIO Docker)" job (`.github/workflows/ci.yml`)
  plus multipart E2E testcontainers against `quay.io/minio/minio:latest`.
- The **weekly compat-matrix** does a live PUT + GET + sidecar-HEAD round-trip.
- Against the Ceph `s3-tests` conformance suite, S4-introduced regressions
  vs MinIO-direct are down to **11 of 784** cases
  ([the remaining gaps are listed](../compatibility.md#client-transparency-compression-is-invisible-to-the-client)).
- **Every measured benchmark in this series ran S4 into a MinIO backend:**

| Workload (doc) | Measured saving on the MinIO bucket | Conditions |
|---|---|---|
| [#1 Elasticsearch frozen tier](elasticsearch-frozen-tier.md) | **−15% to −27%** repository storage across index codecs | ES 9.4.2, MinIO RELEASE.2025-09-07, S4 v1.2.2, Ryzen 9 9950X, local, 2026-06-18 |
| [#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md) | **−16.5% to −28.1%** across index codecs | OpenSearch 2.19.5, MinIO RELEASE.2025-09-07, S4 v1.2.2, Ryzen 9 9950X, local, 2026-06-19 |
| [#3 Grafana Loki chunks](grafana-loki-chunks.md) | **−18.4%** (zstd-3 over snappy chunks, net of sidecars) | Loki 3.3.2, `minio/minio:latest`, S4 v1.2.2, Ryzen 9 9950X, local, 2026-06-19 |
| [#4 Kafka tiered storage](kafka-tiered-storage.md) | **−74.7%** on uncompressed segments / −22.6% snappy / −0.0% producer-zstd | Kafka 3.9.1, Aiven RSM v1.1.1, `minio/minio:latest`, S4 v1.2.2, Ryzen 9 9950X, local, 2026-06-19 |
| [#5 Cold Parquet recompaction](cold-parquet.md) (offline, not the gateway) | **−36.6%** over snappy / −51.7% over uncompressed | `minio/minio:latest`, S4 v1.2.2, Ryzen 9 9950X, local, 2026-06-19 |

Read those numbers honestly: they are **application-format** results — the
application had often already compressed its data (snappy chunks, Lucene
segments), so S4 recovered the double-compression residual. Raw text is the
other end: uncompressed Kafka segments gave −74.7%, and plain log-like text
hits far higher single-pass ratios (the README's
[headline table](../../README.md#proof) measured 155× on an nginx access log,
single-pass cpu-zstd-3, Ryzen 9 9950X, 2026-05-13 — a codec ceiling, not a
production claim). Size your own mix with `s4 estimate` before believing any
range.

**Why this beats "just add disks" arithmetic on MinIO:** you own the hardware,
so a stored byte costs whatever your drives, chassis, and replication factor
cost. With erasure coding, each logical byte occupies `(data + parity) / data`
raw bytes — e.g. EC 8+4 stores 1.5 raw bytes per logical byte — so **every
byte S4 removes saves 1.5× that in raw disk**, and pushes back the date of the
next capacity expansion. MinIO also ships its own optional inline compression
(S2, a Snappy-family codec — see the
[README comparison matrix](../../README.md#how-it-compares)); it's simpler if
it fits your needs. S4's difference is zstd-class ratios (levels 1–22),
per-object codec dispatch, and behaving identically across every backend in
this doc — compare both on your own data rather than trusting either claim.

### Setup

This is exactly the pairing the repo's [`docker-compose.yml`](../../docker-compose.yml)
ships for local testing. Standalone:

```bash
# Backend credentials come from the standard AWS env chain.
export AWS_ACCESS_KEY_ID=<minio-access-key>
export AWS_SECRET_ACCESS_KEY=<minio-secret-key>
export AWS_REGION=us-east-1

s4 --endpoint-url http://minio.internal:9000 \
   --host 0.0.0.0 --port 8014 \
   --codec cpu-zstd --dispatcher sampling
```

Point clients at `http://<s4-host>:8014` instead of the MinIO endpoint —
that's the whole application change. To retro-compress objects already in a
bucket (dry-run by default; see [ops/maintenance.md](../ops/maintenance.md)):

```bash
s4 estimate my-bucket --endpoint-url http://minio.internal:9000            # project savings first
s4 migrate  my-bucket --endpoint-url http://minio.internal:9000            # dry-run report
s4 migrate  my-bucket --endpoint-url http://minio.internal:9000 --execute  # rewrite in place
```

---

## Cloudflare R2: lower storage cost with compression (not yet validated)

> **Not yet CI-validated.** No R2 round-trip has run in this project's CI (the
> [backend matrix](../compatibility.md#backend-compatibility-matrix) row is a
> hook awaiting operator credentials). Everything below is S3-wire-compatibility
> reasoning plus provider-documented pricing — run the
> [validation checklist](#before-production-on-r2--b2--wasabi-a-validation-checklist)
> before trusting it with real data.

**R2 pricing (as of July 2026, per [Cloudflare's pricing page](https://developers.cloudflare.com/r2/pricing/)):**
Standard storage **$0.015/GB-month** (≈ $15/TB-month), Infrequent Access
$0.01/GB-month; Class A operations (writes, lists) $4.50/million, Class B
(reads) $0.36/million; **egress is free**; free tier of 10 GB-month storage,
1M Class A and 10M Class B operations per month.

**The savings math — and the honest R2-specific caveat.** R2's headline is
zero egress fees, which means the "compression also cuts your egress line"
argument **does not apply here** — there is no egress line. On R2 the entire
S4 payoff is the storage line: at $15/TB-month, the README's 50–80% range on
compressible data is **$7.50–12.00 saved per logical TB per month**, minus
S4's host cost (see the [README cost table](../../README.md#does-s4-make-sense-for-your-bill))
and minus a small operations delta:

- Every compressed PUT writes the object **plus** its `.s4index` sidecar —
  budget **~2× Class A** on writes ($9.00 instead of $4.50 per million
  compressed PUTs).
- S4's client-transparent listings (the default since v1.4.1) rewrite each
  listed key's size/ETag via one backend `HeadObject` per key — Class B at
  $0.36/million, negligible for most workloads, but list-heavy traffic can opt
  out with `--physical-listings`.

**The single most important thing to test on R2: multipart.** Cloudflare
[documents](https://developers.cloudflare.com/r2/objects/multipart-objects/)
that "all parts except the last must be the same size." S4 compresses each
part before it reaches the backend, so backend-side part sizes are
content-dependent: highly compressible parts get padded up to the 5 MiB S3
minimum (uniform), but parts that compress to *different* sizes above that
floor will arrive at R2 **non-uniform** — exactly what R2 says it rejects.
Until this project or you have validated multipart against R2, treat large
multipart uploads through S4 → R2 as **unproven and likely to fail for mixed
compressibility data**. Single-PUT objects don't have this constraint.

R2's 10 GB free tier makes it the cheapest of the three hosted providers to
validate — the full smoke test below fits inside it.

```bash
export AWS_ACCESS_KEY_ID=<r2-access-key-id>
export AWS_SECRET_ACCESS_KEY=<r2-secret-access-key>
export AWS_REGION=auto              # per Cloudflare's S3-API docs

s4 --endpoint-url https://<account-id>.r2.cloudflarestorage.com \
   --host 0.0.0.0 --port 8014 \
   --codec cpu-zstd --dispatcher sampling
```

---

## Backblaze B2: compress objects before they land (not yet validated)

> **Not yet CI-validated.** Same posture as R2: the
> [compat-matrix hook](../compatibility.md#backend-compatibility-matrix)
> exists, no real B2 round-trip has run in this project's CI.

**B2 pricing (as of July 2026, per [Backblaze's pricing page](https://www.backblaze.com/cloud-storage/pricing)):**
storage **$6.95/TB-month** pay-as-you-go; egress free up to **3× your monthly
average stored data**, then $0.01/GB; Class A/B/C API calls free (Class D
$0.004 per 10,000, first 2,500/day free); first 10 GB of storage free.

**The savings math.** At $6.95/TB-month, 50–80% off the compressible share is
**$3.48–5.56 saved per logical TB per month**. Because B2 doesn't bill
Class A/B/C calls, the sidecar's extra write per object costs nothing here —
only the sidecar's (small) stored bytes count, and the measured results above
are already net of sidecar overhead.

**Egress cuts both ways — do this arithmetic for your workload.** Bytes leave
B2 in *compressed* form (S4 decompresses at the gateway, on your side of the
meter), so download-heavy workloads move less billable data. But the free
allowance is **3× stored bytes, and compression shrinks stored bytes** — a
smaller allowance against smaller egress. To first order the ratio is
preserved for S4-compressed traffic; workloads mixing compressed and
passthrough objects should check whether they cross the 3× line after
migration.

```bash
export AWS_ACCESS_KEY_ID=<b2-key-id>
export AWS_SECRET_ACCESS_KEY=<b2-application-key>
export AWS_REGION=<region>          # e.g. us-west-004

s4 --endpoint-url https://s3.<region>.backblazeb2.com \
   --host 0.0.0.0 --port 8014 \
   --codec cpu-zstd --dispatcher sampling
```

Backblaze documents its S3-compatible API's supported and unsupported features
separately from AWS's — run the checklist below rather than assuming parity,
paying particular attention to the multipart and HEAD/ETag steps.

---

## Wasabi: fewer stored bytes under flat-rate pricing (not yet validated)

> **Not yet CI-validated.** Same posture:
> [compat-matrix hook](../compatibility.md#backend-compatibility-matrix)
> present, no real Wasabi round-trip has run in this project's CI.

**Wasabi pricing (as of July 2026, per [wasabi.com/pricing](https://wasabi.com/pricing)
and their [pricing FAQ](https://wasabi.com/pricing/faq)):** pay-as-you-go
starting at **$7.99/TB-month**; **no egress or API request fees**, under a
fair-use policy (monthly egress should not exceed active storage);
**90-day minimum storage duration** — objects deleted or overwritten earlier
incur a "Timed Deleted Storage" charge for the remaining days; **1 TB minimum
monthly charge**.

**The savings math — with three Wasabi-specific catches.** At $7.99/TB-month,
50–80% is **$4.00–6.39 saved per logical TB per month**, but:

1. **The 90-day minimum works against `s4 migrate`.** Retro-compressing an
   existing Wasabi bucket rewrites each object, and Wasabi bills the replaced
   bytes as Timed Deleted Storage for the remainder of their 90 days. The
   saving is real but arrives after a transition window of up to ~3 months —
   during which you briefly pay for *both* the old and (smaller) new bytes.
   Migrating data younger than 90 days is the worst case; a backlog already
   older than 90 days pays no deletion penalty. New writes through the gateway
   have no such issue.
2. **The 1 TB minimum charge is a floor.** If compression takes your active
   storage below 1 TB, the bill stops falling. Small deployments should check
   whether they'd just be moving from "small" to "below the minimum".
3. **The fair-use egress ratio shifts.** Free egress is judged against active
   storage, and compression shrinks active storage — while egress bytes (which
   leave Wasabi compressed) shrink roughly in step for S4-compressed traffic.
   Download-heavy workloads near the 1:1 line should re-check the ratio after
   compression.

```bash
export AWS_ACCESS_KEY_ID=<wasabi-access-key>
export AWS_SECRET_ACCESS_KEY=<wasabi-secret-key>
export AWS_REGION=<region>          # e.g. us-east-1

s4 --endpoint-url https://s3.<region>.wasabisys.com \
   --host 0.0.0.0 --port 8014 \
   --codec cpu-zstd --dispatcher sampling
```

---

## The per-provider arithmetic, side by side

Per **logical TB per month** of compressible data (as of July 2026 pricing;
50–80% is the [README](../../README.md)'s published range for compressible
data — logs, JSON, columnar; already-compressed objects pass through at ~1×
and save nothing):

| Backend | Storage price | Saved at −50% | Saved at −80% | What compression *doesn't* change here |
|---|---:|---:|---:|---|
| MinIO (self-hosted) | your disk × EC factor | 0.5 TB × EC raw disk | 0.8 TB × EC raw disk | hardware you already bought — savings arrive as deferred expansion |
| Cloudflare R2 | $0.015/GB-mo (≈$15/TB-mo) | **$7.50** | **$12.00** | egress was already $0 — storage is the whole win; writes cost ~2× Class A |
| Backblaze B2 | $6.95/TB-mo | **$3.48** | **$5.56** | API calls already free; the 3× free-egress allowance shrinks with stored bytes |
| Wasabi | from $7.99/TB-mo | **$4.00** | **$6.39** | the 1 TB monthly minimum; 90-day minimum billing on migrated (rewritten) objects |

Net all of this against an S4 host (a small CPU instance or an existing VM —
see the [README cost table](../../README.md#does-s4-make-sense-for-your-bill)
for host sizing) and run `s4 estimate` against your real bucket before
committing: it reads object sizes and samples, and projects savings with no
gateway deployed.

---

## Before production on R2 / B2 / Wasabi: a validation checklist

These providers advertise S3 compatibility, but S3-compatible implementations
genuinely differ in the places S4 touches. This project's own posture is
"verified means CI ran it" — hold your deployment to the same bar. What to
verify, and why:

1. **Path-style addressing** — S4 always calls the backend path-style; confirm
   your endpoint accepts it (all four providers document that they do).
2. **PUT/GET round-trip integrity** — the basic compress → store → decompress
   loop, hash-compared.
3. **Multipart** — part-size rules (R2's uniform-part-size constraint above),
   minimum part size, and `ListParts` behavior during S4's
   `CompleteMultipartUpload` reverse-mapping.
4. **ETag semantics** — S4 stamps the client-transparent ETag in `s4-*` object
   metadata on the backend; verify HEAD/GET through S4 return `MD5(original)`
   on your backend.
5. **Checksum / streaming-signature handling** — `Content-MD5`,
   `x-amz-checksum-*`, and `aws-chunked` (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`)
   uploads are where the Garage drift was observed; test with your real SDKs.
6. **Range GET** — S4's sidecar-indexed partial reads issue ranged GETs against
   the backend's compressed object.
7. **Listing** — `ListObjectsV2` pagination, and that `.s4index` sidecars are
   correctly filtered from S4's listings (they will be visible when you list
   the backend directly; that's expected).

A smoke test that covers all seven, using only `aws-cli` (R2's and B2's free
10 GB tiers cover this entirely; on Wasabi remember the 90-day minimum applies
even to test objects):

```bash
export S4=http://localhost:8014                  # the S4 gateway
export BACKEND=https://<your-backend-endpoint>   # direct backend, for inspection
B=s4-smoke-$RANDOM

aws --endpoint-url $S4 s3 mb s3://$B             # or pre-create in the provider console

# 1) PUT/GET round-trip hash compare (compressible payload)
yes '2026-07-06T00:00:00Z INFO tenant=demo path=/api/v1/items status=200 bytes=1842' \
  | head -n 400000 > in.log                      # ~30 MiB, compresses heavily
aws --endpoint-url $S4 s3 cp in.log s3://$B/in.log
aws --endpoint-url $S4 s3 cp s3://$B/in.log out.log
sha256sum in.log out.log                         # MUST match

# 2) Range GET through the sidecar index
aws --endpoint-url $S4 s3api get-object --bucket $B --key in.log \
    --range bytes=1000000-1000999 range.bin
cmp <(tail -c +1000001 in.log | head -c 1000) range.bin   # MUST match

# 3) Multipart, both compressibility extremes (aws-cli splits at 8 MiB by default)
head -c 64M /dev/urandom > rand.bin              # incompressible → passthrough parts
cat in.log in.log in.log > big.log               # compressible → padded/framed parts
aws --endpoint-url $S4 s3 cp rand.bin s3://$B/rand.bin
aws --endpoint-url $S4 s3 cp big.log  s3://$B/big.log
aws --endpoint-url $S4 s3 cp s3://$B/rand.bin rand.out && cmp rand.bin rand.out
aws --endpoint-url $S4 s3 cp s3://$B/big.log  big.out  && cmp big.log  big.out

# 4) Client-transparent HEAD: ETag == MD5(original), Content-Length == original size
aws --endpoint-url $S4 s3api head-object --bucket $B --key in.log
md5sum in.log

# 5) Listing via S4: original keys only, no .s4index sidecars
aws --endpoint-url $S4 s3api list-objects-v2 --bucket $B --query 'Contents[].[Key,Size]'

# 6) Inspect the backend directly: compressed object + sidecar landed
aws --endpoint-url $BACKEND s3 ls s3://$B/       # in.log is smaller; in.log.s4index exists
```

**Then make it continuous.** The repo's weekly
[`compat-matrix.yml`](../../.github/workflows/compat-matrix.yml) workflow
already has real-backend jobs for all three providers that run when a fork
configures credentials, and silently skip otherwise — set repository variables
`R2_BUCKET` / `R2_ENDPOINT` / `R2_REGION` and secrets `R2_ACCESS_KEY_ID` /
`R2_SECRET_ACCESS_KEY` (analogously `B2_*` with `B2_KEY_ID` /
`B2_APPLICATION_KEY`, and `WASABI_*`) and your fork will round-trip
PUT + GET + sidecar-HEAD against the live backend every week, through an
`s4 --codec cpu-zstd --dispatcher always` server. If you run this and it's
green (or not), please report back — an issue with the run link is enough to
upgrade the [backend matrix](../compatibility.md#backend-compatibility-matrix)
row from "configurable" to "verified".

---

## Caveats (read before quoting numbers)

- **R2 / B2 / Wasabi are unvalidated by this project.** No CI round-trip has
  run against them here; the pricing math above is arithmetic on provider list
  prices, not a measured deployment. MinIO is the only backend in this doc
  with per-PR CI evidence and measured benchmarks behind it.
- **All measured savings in this doc were measured against MinIO, locally**
  (AMD Ryzen 9 9950X, S4 v1.2.2, 2026-06-18/19, no network RTT), and reused
  here with attribution — they were not re-measured for this page. Savings are
  a property of your data, not the backend, so ratios should carry over; read
  overheads will differ with your backend's RTT (see the
  [Loki doc's measured ~1.7 ms local per-GET overhead](grafana-loki-chunks.md#result-3--read-overhead-whole-chunk-get)
  and the [ES doc's injected-RTT analysis](elasticsearch-frozen-tier.md)).
- **50–80% applies to the compressible share.** Already-compressed objects
  (media, archives, snappy/zstd application formats) route to passthrough —
  no harm, little gain; the application-format benchmarks above (−15% to −28%)
  show what "the app already compressed it" looks like.
- **S4 becomes a read-path dependency** — a gateway outage blocks reads of
  compressed objects until it's back (data remains safe and decodable on the
  backend via the Apache-2.0 [`s4-codec` tools](../wire-format.md), with no
  gateway runtime). Run ≥2 stateless instances behind a load balancer for
  production; multipart uploads mid-flight prefer a single gateway (see
  [multipart ETag notes](../compatibility.md#client-transparency-compression-is-invisible-to-the-client)).
- **Prices dated 2026-07-06** from the providers' public pricing pages; they
  change. Recompute with your invoice, not this page.

---

*See also: [#1 ES frozen tier](elasticsearch-frozen-tier.md) ·
[#2 OpenSearch searchable snapshots](opensearch-searchable-snapshots.md) ·
[#3 Grafana Loki chunks](grafana-loki-chunks.md) ·
[#4 Kafka tiered storage](kafka-tiered-storage.md) ·
[#5 Cold Parquet recompaction](cold-parquet.md) ·
[backend compatibility matrix](../compatibility.md#backend-compatibility-matrix) ·
[savings & `s4 estimate`](../savings.md) · [ops: `s4 migrate`](../ops/maintenance.md).*
