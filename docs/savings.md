# Estimating and measuring savings

### Pre-deployment savings estimate (`s4 estimate`)

`s4 estimate` answers "how many GB / dollars would S4 save on this
existing bucket?" **before** you deploy the gateway. Fully read-only
(`ListObjectsV2` + `GetObject` only, never writes); point it at the
**backend**, not an S4 gateway:

```bash
s4 estimate <bucket>[/prefix] --endpoint-url https://s3.example.com [--format json]
```

It lists the bucket (S4-internal keys excluded: `*.s4index` sidecars,
`.s4dict/` dictionaries, `*.__s4ver__/*` versioning shadows; capped at
`--max-list-keys`, default 100000), stratifies objects by extension,
excludes already-S4 objects from sampling (gateway metadata or
`S4F2`/`S4P1`/`S4E*` magic, structurally validated — re-estimating a
gateway-operated bucket won't measure framed/encrypted bytes as
plaintext; they are reported per-stratum as `already-s4`),
samples up to `--samples-per-stratum` (default 8) objects per stratum
(size-weighted, deterministic under `--seed`, default 42), runs the
**same `SamplingDispatcher` decision the gateway would run at PUT
time** (the server-side `--codec` / `--dispatcher` / `--zstd-level` /
`--gpu-min-bytes` / `--prefer-columnar-gpu` flags are honored, passed
*before* the subcommand), actually compresses the sampled bytes, and
extrapolates. Objects larger than `--max-sample-bytes` (default 8 MiB)
are measured on a Range-GET prefix. Cost lines use
`--price-per-gb-month` (default 0.023, S3 Standard us-east-1 first-50TB
tier). Example output (5-object MinIO demo bucket — your ratios depend
entirely on your data):

```
S4 storage estimate for demo
  objects: 5   total: 2.7 MiB (2808356 bytes)
  sampled: 5 object(s), 2.7 MiB read (100.0% of listed bytes)

  stratum       objects        bytes  sampled   ratio      projected  codecs
  .bin                1      1.0 MiB        1   1.000        1.0 MiB  passthrough×1
  .json               1    242.0 KiB        1   0.034        8.2 KiB  cpu-zstd×1
  .log                3      1.4 MiB        3   0.000          438 B  cpu-zstd×3

  projected total: 1.0 MiB (1057418 bytes, overall ratio 0.377)
  storage cost: $0.00/month now -> $0.00/month projected (at $0.023/GB-month, storage bytes only)
```

Honesty notes (always printed with the report):

- **Storage bytes only** — request, egress and (on GPU deployments)
  compute costs are unchanged by S4.
- It is a **sampling extrapolation**; the report states the sampled
  fraction of listed bytes, and prefix-sampled large objects can
  compress differently from their tails.
- **No GPU is required or used.** When the dispatcher would pick an
  `nvcomp-*` codec at runtime (GPU build, or `--prefer-columnar-gpu`
  passed from a CPU-only host to model a planned GPU deployment), the
  ratio is measured with a **cpu-zstd proxy** and the report says so
  explicitly — e.g. `nvcomp-bitcomp would be chosen at runtime for 1
  sample(s); ratio shown is cpu-zstd (level 3) proxy … (typically
  conservative for integer columns)`.

Exit code is 0 on any completed estimate, including an empty listing
(`no objects found`).

### Measured savings in production (`s4 savings`, v1.2)

`s4 estimate` predicts; the **savings ledger** measures. With the
ledger enabled, the gateway maintains cumulative per-bucket counters —
`original_bytes` (logical bytes clients PUT), `stored_bytes` (bytes
actually written to the backend: frames + SSE envelope + sidecars) and
`objects` — updated on PUT / CompleteMultipartUpload / CopyObject /
DELETE and flushed to the state file on every write event. Three steps:

1. **Enable the ledger** (opt-in; without the flag every code path is
   bit-for-bit unchanged):

   ```bash
   s4 --endpoint-url https://s3.example.com \
      --savings-ledger-state-file /var/lib/s4/savings-ledger.json
   ```

2. **Let it run** — days or weeks of normal traffic. The same numbers
   are exported live as the
   `s4_ledger_{original_bytes,stored_bytes,objects}{bucket}` Prometheus
   gauges, with a drop-in dashboard at
   [`contrib/grafana/s4-savings-dashboard.json`](../contrib/grafana/s4-savings-dashboard.json)
   (see [docs/observability.md](observability.md) for the import
   steps).

3. **Read the answer** — no gateway restart, no network; the CLI only
   reads the state file (`--format json` for machines):

   ```bash
   s4 savings --state-file /var/lib/s4/savings-ledger.json
   ```

   Example output (MinIO demo run from the `ledger_minio` e2e — one
   passthrough blob, one 8 MiB + 1 MiB multipart, one 32 KiB blob;
   your ratios depend entirely on your data):

   ```
   S4 measured savings (gateway-written objects)

     bucket                objects       original         stored          saved  saved%      $/month
     ledgerbkt                   3        9.1 MiB        6.1 MiB        3.0 MiB   33.0%         0.00

     total: 3 objects, 9.1 MiB original -> 6.1 MiB stored (33.0% saved, 3145558 bytes)
     monthly savings: $0.00 (at $0.023/GB-month, storage bytes only)

   Notes:
     - the ledger observes gateway-traversing writes only: backend-direct writes, `s4 migrate`, and `s4 recompact` (both backend-direct) are not reflected; `recompact` savings appear only after the gateway next rewrites the object
     - aborted multipart uploads are never counted (parts are recorded at Complete time only); cross-bucket replication replicas are not counted
     - DELETE / overwrite subtraction applies only to objects the gateway itself accounted (internal `s4-ledger` marker); removals of non-ledger-managed objects are skipped and tallied separately. The HEAD probe is best-effort — a raced probe leaves the counters slightly stale rather than failing the request. The marker records that the ledger was enabled at write time, not that the bytes are in the counters: a multipart Complete skipped for an oversized/unfetchable body, or a flag toggled off->on, can leave marker-carrying objects that were never added — their later removal subtracts with clamping at zero (under-claim, surfaced by the drift note when it floors a bucket)
     - storage bytes only: request, egress, and (on GPU deployments) compute costs are unchanged by S4
     - column semantics: `objects` / `original` / `stored` are cumulative accounted-write counters, not a point-in-time bucket inventory — an overwrite of an already-accounted object adds 0 objects and only the footprint delta, so after churn (notably retried multipart Completes) the per-column split can diverge from what a bucket listing shows. `saved` (= original - stored) is the byte-accurate net figure — the `--marketplace-metered-savings` billing quantity — and is the number to quote
   ```

Honesty notes (always printed with the report, repeated here because
they bound what the numbers mean):

- The ledger sees **gateway-traversing writes only**. Backend-direct
  writes, `s4 migrate` and `s4 recompact` (both talk to the backend
  directly) are not reflected — an `s4 recompact` shrink shows up only
  after the gateway itself next rewrites that object. Replication
  replicas and aborted-multipart part bytes are likewise not counted.
- Overwrite / DELETE subtraction adds **one best-effort HEAD probe per
  write-shaped request** (plus a sidecar HEAD where relevant; a
  ledger-enabled CopyObject probes up to three — source, old
  destination, new destination) — this extra backend traffic exists
  *only* when the flag is set.
- **Only ledger-accounted objects are ever subtracted** (audit rounds
  1-2): gateway writes made while the ledger is enabled carry an
  internal `s4-ledger` metadata marker (client-supplied copies are
  stripped — including via access-point copy sources — so it can't be
  forged; replication replicas are written marker-*stripped* because
  they are never counted), and deletes/overwrites of objects *without*
  the marker — backend-direct, `s4fs`-written, `migrate`/`recompact`
  output, pre-ledger writes — skip subtraction and are tallied per
  bucket as `skipped_unaccounted` with a report note. The marker means
  "the ledger was enabled at write time", not "the bytes are in the
  counters" — a cap-exceeded multipart or a flag toggle can strand a
  marker without an add (zero-clamp + drift note are the guard rails).
  Ledger-enabled SSE/versioned multipart completes and REPLACE copies
  also stamp `s4-original-size` so the add and the eventual subtract
  resolve the same logical size (no phantom savings on churn); ratio
  and $/month floor at 0 with a drift note if counters ever disagree.
- **`saved` is the byte-accurate column; the others are counters, not
  an inventory** (#151). `objects` / `original` / `stored` accumulate
  accounted-write deltas, and a marker-gated overwrite swap subtracts
  equal amounts from `original` and `stored` — so the *difference*
  (`saved`, which is also the `--marketplace-metered-savings`
  GBSavedHours quantity) stays exact even when the per-column split
  does not match a bucket listing. The live signature (verified to the
  byte, 2026-07-08): a multipart Complete interrupted after the backend
  commit (crash/OOM) leaves the assembled object marker-stamped but
  never added; the client's retried upload is then accounted as an
  overwrite of it, so the bucket row shows `objects` 0, `stored` ≈
  sidecar bytes only, and `original` = the net delta. The report
  detects bytes-with-zero-objects buckets and discloses them in a
  dedicated note.
- State-file durability matches the other `--*-state-file` managers
  plus an event-driven flush (atomic tmp+rename on every mutation;
  SIGUSR1 re-dumps it too) — a crash loses at most the in-flight
  event.

### Multipart uploads: the 5 MiB part floor caps at-rest savings

S3's multipart protocol requires every part except the last to be at
least 5 MiB (`EntityTooSmall` otherwise). S4 compresses each
`UploadPart` body into one `S4F2` frame — and when the compressed
frame lands **under** 5 MiB, the gateway pads it back up to the
minimum with a zero-filled `S4P1` padding frame so the backend accepts
the part. Parts whose *original* size is already under 5 MiB (in
practice the final part) are exempt and never padded; parts whose
framed size stays at or above 5 MiB (incompressible data) need no
padding. The floor therefore bites **exactly the compressible data S4
is deployed for**, and it is always on — unlike the opt-in
`--uniform-multipart-parts` mode below, it needs no flag.

The consequence is a hard floor on multipart at-rest savings **while
the object stays in multipart form**, set purely by the client's part
size:

| client part size | stored floor (5 MiB ÷ part size) | best-case savings |
|---|---|---|
| 8 MiB (aws-cli / boto3 default `multipart_chunksize`) | 62.5% | **37.5%** |
| 16 MiB | 31.3% | 68.7% |
| 64 MiB | 7.8% | 92.2% |
| 128 MiB | 3.9% | 96.1% |

Worked example: a 2 GiB object of **zeros** — the most compressible
input possible — uploaded with the aws-cli default 8 MiB
`multipart_chunksize` is 256 parts; every part compresses to a few KiB
and is padded back to 5 MiB, so the backend stores 256 × 5 MiB =
1.25 GiB. **Stored is ≥62.5% of original regardless of
compressibility.** The same object at 64 MiB parts stores 32 × 5 MiB =
160 MiB (7.8%) — this exact figure (167,772,160 bytes) was measured
live in the 2026-07-08 Metered Savings E2E. Single-PUT objects have no
floor at all, and the final part of a multipart is exempt (the v0.2
final-part padding trim — see [architecture.md](architecture.md)).

**Mitigation: raise the client's part size.** For aws-cli:

```bash
aws configure set default.s3.multipart_chunksize 64MB
# aws-cli parses "64MB" as binary units (64 MiB); also consider
# default.s3.multipart_threshold to keep mid-size objects single-PUT
```

For boto3, pass
`TransferConfig(multipart_chunksize=64 * 1024 * 1024)`. Trade-offs of
larger parts: the per-part compression ratio *improves* (bigger zstd
window per frame — see the multipart note in
[benchmarks.md](benchmarks.md)), while Range-GET granularity
*coarsens* (each part is one compressed frame, and a range read
decodes its enclosing frames), and per-part client memory grows.
64 MiB is a reasonable balance for log/analytics workloads.

**Reclaiming the padding**: `s4 recompact <bucket>[/prefix]
--endpoint-url <backend-url> --execute` (dry-run without `--execute`)
rewrites multipart-written objects as single-PUT framed objects with
the padding frames dropped — see
[ops/maintenance.md](ops/maintenance.md). Scope caveat: recompact
is cpu-zstd → cpu-zstd only; GPU-codec (`nvcomp-*`), gzip, dictionary
and passthrough objects are skipped (`unsupported-codec`). `s4
migrate` does **not** reclaim padding — it skips anything already in
S4 format (`already-s4`). Both tools talk to the backend directly, so
the ledger shows the reclaim only after the gateway itself next
rewrites the object.

**The ledger and metering are already honest about the floor**:
`stored_bytes` counts the padded assembled size plus the sidecar
(verified live to the byte, 2026-07-08), so a low multipart savings
figure in `s4 savings` — and in the Marketplace GBSavedHours quantity
derived from it — is a real measurement of the padding overhead, not a
reporting artifact.

Related: the opt-in `--uniform-multipart-parts` mode (required by
uniform-part-size backends such as Cloudflare R2, #143) pads further —
every non-final part to ≈ its original size, making at-rest multipart
savings ~zero until recompacted; its overhead is documented in
[use-cases/s3-compatible-backends.md](use-cases/s3-compatible-backends.md#cloudflare-r2-lower-storage-cost-with-compression-not-yet-validated).
The 5 MiB floor described here is the always-on default that applies
even without that flag.
