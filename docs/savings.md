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

     bucket                objects       original         stored    saved      $/month
     ledgerbkt                   3        9.1 MiB        6.1 MiB    33.0%         0.00

     total: 3 objects, 9.1 MiB original -> 6.1 MiB stored (33.0% saved, 3145558 bytes)
     monthly savings: $0.00 (at $0.023/GB-month, storage bytes only)

   Notes:
     - the ledger observes gateway-traversing writes only: backend-direct writes, `s4 migrate`, and `s4 recompact` (both backend-direct) are not reflected; `recompact` savings appear only after the gateway next rewrites the object
     - aborted multipart uploads are never counted (parts are recorded at Complete time only); cross-bucket replication replicas are not counted
     - DELETE / overwrite subtraction applies only to objects the gateway itself accounted (internal `s4-ledger` marker); removals of non-ledger-managed objects are skipped and tallied separately. The HEAD probe is best-effort — a raced probe leaves the counters slightly stale rather than failing the request. The marker records that the ledger was enabled at write time, not that the bytes are in the counters: a multipart Complete skipped for an oversized/unfetchable body, or a flag toggled off->on, can leave marker-carrying objects that were never added — their later removal subtracts with clamping at zero (under-claim, surfaced by the drift note when it floors a bucket)
     - storage bytes only: request, egress, and (on GPU deployments) compute costs are unchanged by S4
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
- State-file durability matches the other `--*-state-file` managers
  plus an event-driven flush (atomic tmp+rename on every mutation;
  SIGUSR1 re-dumps it too) — a crash loses at most the in-flight
  event.
