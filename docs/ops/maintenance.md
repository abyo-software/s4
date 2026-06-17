# Bucket maintenance — migrate, recompact, maintain

### Bulk retro-compression of existing buckets (`s4 migrate`)

`s4 migrate` rewrites the uncompressed objects already sitting in a
bucket into the same S4F2 framed format the gateway writes at PUT time
— the follow-up to `s4 estimate` once the numbers say yes. Like
`sweep-orphan-sidecars`, it is **dry-run by default**; like every
sidecar subcommand, point it at the **backend**, not an S4 gateway:

```bash
s4 migrate <bucket>[/prefix] --endpoint-url https://s3.example.com            # dry-run
s4 migrate <bucket>[/prefix] --endpoint-url https://s3.example.com --execute  # write
```

Per object it (1) probes the first 4 bytes + metadata and **skips
anything already in S4 format** — which makes a re-run resume
automatically with no checkpoint file; (2) runs the **same
`SamplingDispatcher` decision the gateway runs at PUT time** (the
server-side `--codec` / `--dispatcher` / `--zstd-level` /
`--gpu-min-bytes` / `--prefer-columnar-gpu` flags are honored, passed
*before* the subcommand) and skips passthrough picks / bodies the
framing doesn't shrink; (3) frames the body with the same
`streaming_compress_to_frames` call and chunk-size policy as the
gateway's PUT path; (4) **decompresses the result in-process and
byte-compares it against the original — no verify, no write, and there
is deliberately no flag to turn this off**; (5) re-checks the source
ETag with a HEAD immediately before the overwrite PUT and skips on
mismatch (`etag-raced`); (6) writes the same `<key>.s4index` sidecar
the gateway writes for multi-frame bodies, so Range GETs keep the
partial-fetch fast path. `--concurrency` (default 4) objects run in
parallel; objects above `--max-body-bytes` (default 5 GiB, same cap as
`repair-sidecar`) are skipped as `too-large` — the cap is enforced from
the GET `Content-Length` *before* buffering, so an oversized body is
never pulled into RAM.

S4-internal keys (`*.s4index` sidecars, `.s4dict/` dictionaries,
`*.__s4ver__/*` versioning shadows) are excluded from the listing and
never rewritten. The rewrite PUT inherits the source's **storage class
and object tags** in addition to content-type and user metadata; object
ACLs and Object Lock retention are **not** inherited (stated in the
report notes — re-apply them after migrating locked buckets). When the
credential can't read tags (`GetObjectTagging` denied / unimplemented)
the object skips as `tags-unreadable` rather than being rewritten
tag-less; pass `--no-tags` to explicitly rewrite without reading or
preserving tags. A roundtrip-verify failure is a hard failure (exit 1),
not a skip: it means the tool's own output didn't decode, which is a
bug worth a loud stop.

Example run (5-object MinIO demo bucket — 3 repetitive logs, one JSON
export, one random binary):

```
$ s4 migrate demo --endpoint-url http://127.0.0.1:9000 --execute
S4 migrate demo — execute
  objects: 5   total: 4.8 MiB (5032356 bytes)
  migrated: 4 object(s), 3.8 MiB -> 7.7 KiB (saves 3.8 MiB)
  skipped: 0 already-s4, 1 not-compressible, 0 too-large, 0 etag-raced, 0 verify-failed, 0 tags-unreadable
  failed: 0
  codecs: cpu-zstd×4

Notes:
  - conflict safety: the source ETag is re-checked via HEAD immediately before each overwrite, but S3 has no compare-and-swap — a writer landing between the HEAD and the PUT is silently overwritten

$ s4 migrate demo --endpoint-url http://127.0.0.1:9000 --execute   # idempotent re-run
S4 migrate demo — execute
  objects: 5   total: 1.0 MiB (1056431 bytes)
  migrated: 0 object(s), 0 B -> 0 B (saves 0 B)
  skipped: 4 already-s4, 1 not-compressible, 0 too-large, 0 etag-raced, 0 verify-failed, 0 tags-unreadable
  failed: 0
```

Exit code is 0 when every object was migrated or skipped, 1 when any
object failed (failed objects are left untouched; re-running resumes).
`--format json` emits the full report
(`s4_server::migrate::MigrateReport` serde shape).

Honest limitations (the report prints the run-specific ones):

- **The ETag re-check narrows but does not close the overwrite race.**
  S3 has no compare-and-swap, so a writer landing between migrate's
  HEAD and its PUT is silently overwritten. Migrate buckets during a
  write-quiet window, or scope with `<bucket>/<prefix>` to cold data.
- **SSE-enabled deployments are rejected** (`--sse-s4-key` /
  `--kms-local-dir`): `migrate does not support SSE-enabled deployments
  yet; route writes through a running gateway instead`.
- **Versioned buckets work but double-bill**: the overwrite PUT leaves
  the previous (uncompressed) version in place until lifecycle rules
  expire it. The report prints a `WARNING` line when
  `GetBucketVersioning` reports `Enabled`.
- **CPU-only writes.** When the dispatcher's pick is a GPU
  (`nvcomp-*`) or non-streaming (`cpu-gzip`) codec, migrate really
  falls back to `cpu-zstd` at `--zstd-level` — same direction as a
  non-GPU gateway build — and the codec breakdown shows
  `picked != wrote_with` with a note. Frames are self-describing, so a
  GPU gateway reads the cpu-zstd frames unchanged.
- **Objects above 5 GiB are skipped**, not re-split into multipart —
  migrate buffers the whole body for the mandatory roundtrip verify.

### Background recompaction to higher zstd levels (`s4 recompact`)

The gateway's PUT path favours latency: bodies are framed with
`cpu-zstd` at `--zstd-level` (default 3). `s4 recompact` is the LSM
take on that trade — during a quiet window it "bakes" cold S4-framed
cpu-zstd objects at a higher level (`--target-zstd-level`, default 19),
shrinking the backend bill without touching the read path: compression
level is encode-side only, so every gateway build reads level-19 frames
exactly like level-3 frames. Like `migrate`, it is **dry-run by
default** and must point at the **backend**, not an S4 gateway:

```bash
s4 recompact <bucket>[/prefix] --endpoint-url https://s3.example.com            # dry-run
s4 recompact <bucket>[/prefix] --endpoint-url https://s3.example.com --execute  # write
```

Per object it (1) probes the first 4 bytes + metadata and selects
**only S4-framed cpu-zstd objects** — the exact inverse of `migrate`'s
selection: plain objects skip as `not-s4` (run `s4 migrate` first),
`passthrough` / `cpu-gzip` / `nvcomp-*` / `cpu-zstd-dict` skip as
`unsupported-codec`; (2) skips objects already stamped
`s4-zstd-level >= target` (`already-compacted`) — **the idempotency
core**: a re-run resumes automatically with no checkpoint file;
(3) decodes the existing frames in-process with the same `FrameIter` +
registry path the gateway's GET uses (recovering the original bytes
doubles as an integrity check on the stored frames); (4) re-frames the
original with the same `streaming_compress_to_frames` call and
chunk-size policy as the PUT path, and **only rewrites when the new
frames shrink the currently stored bytes by `--min-gain-percent`
(default 3%)** — smaller wins skip as `insufficient-gain`, so the run
never churns objects for noise; (5) decompresses the new frames back
and byte-compares against the decoded original — **no verify, no
write, no off switch** — then re-checks the source ETag with a HEAD
immediately before the overwrite PUT (`etag-raced` on mismatch);
(6) refreshes the `<key>.s4index` sidecar for multi-frame bodies (and
deletes a now-stale sidecar when the rewrite came out single-frame).

Like `migrate`, internal keys (`*.s4index`, `.s4dict/`, `*.__s4ver__/*`)
are excluded, storage class + object tags are inherited on rewrite
(ACLs / Object Lock retention are not; unreadable tags skip as
`tags-unreadable`, `--no-tags` opts out), and the `--max-body-bytes`
cap is enforced before buffering. Backend-written framed objects that carry
**no gateway metadata** skip as `unstamped-framed` by default — pass
`--assume-unstamped-framed` only when you know such objects are genuine
S4 frames, because recompacting one changes what a gateway GET serves
for that key (raw frames before, decoded payload after).
User metadata and Content-Type survive the rewrite; the `s4-*`
manifest keys are re-stamped for the new frames plus the
`s4-zstd-level` marker.

`--older-than <DUR>` (`30d`, `12h`, `45m`, `90s`) restricts the run to
objects whose backend `LastModified` is at least that old — newer ones
skip as `too-recent`. That makes a nightly cron the natural way to run
it ("recompact what has gone cold this month"):

```cron
# /etc/cron.d/s4-recompact — nightly at 03:30, only objects idle 30+ days
30 3 * * *  s4  s4 recompact mybucket --endpoint-url https://s3.example.com \
    --older-than 30d --execute --format json >> /var/log/s4-recompact.log 2>&1
```

Re-runs are cheap by design: everything already at the target level
skips in one probe GET per object.

Example run (the `recompact_minio` e2e seed: two varied-text log
objects framed at zstd-3 by `s4 migrate`, one never-migrated plain
object, one passthrough-stamped random binary — output verbatim):

```
S4 recompact s4-recompact-test — execute
  target zstd level: 19   min gain: 3%
  objects: 4   total: 285.0 KiB (291883 bytes)
  recompacted: 2 object(s), 218.0 KiB -> 187.6 KiB (saves 30.4 KiB)
  skipped: 1 not-s4, 0 already-compacted, 1 unsupported-codec, 0 unstamped-framed, 0 insufficient-gain, 0 too-large, 0 etag-raced, 0 too-recent, 0 tags-unreadable
  failed: 0

Notes:
  - conflict safety: the source ETag is re-checked via HEAD immediately before each overwrite, but S3 has no compare-and-swap — a writer landing between the HEAD and the PUT is silently overwritten
  - 1 object(s) skipped as not-s4 — they are not S4-framed; run `s4 migrate` first to frame them, then recompact
```

(That ~14% shrink on already-compressed bytes is specific to this
varied-log corpus at zstd-3 → 19; your gain depends entirely on the
data — run the dry-run first, its sizes are measured on the real
re-framed output, not estimated.)

Exit code is 0 when every object was recompacted or skipped, 1 when
any object failed (failed objects are left untouched; re-running
resumes). `--format json` emits the full report
(`s4_server::recompact::RecompactReport` serde shape).

Honest limitations (the report prints the run-specific ones):

- **cpu-zstd → cpu-zstd only.** GPU-written (`nvcomp-*`), gzip,
  dictionary (`cpu-zstd-dict`) and passthrough objects are skipped,
  not converted.
- **The ETag re-check narrows but does not close the overwrite race**
  — same caveat as `migrate`. Recompact during a write-quiet window,
  or rely on `--older-than` to keep the run on cold keys.
- **SSE-enabled deployments are rejected** (`--sse-s4-key` /
  `--kms-local-dir`); encrypted bodies never carry the frame magic and
  classify as `not-s4` defensively anyway.
- **Versioned buckets work but double-bill**: the overwrite PUT leaves
  the previous version in place until lifecycle rules expire it. The
  report prints a `WARNING` line when versioning is `Enabled`.
- **The `s4-zstd-level` stamp is recompact-only and not propagated by
  CopyObject** — a copied object is simply re-examined on the next run
  and typically skips as `insufficient-gain` (its frames are already
  high-level), at the cost of one decode + recompress.
- **Multipart-written objects are rewritten as single-PUT framed
  objects** (padding frames and the `s4-multipart` flag dropped) —
  byte-identical through the gateway, but the multipart ETag shape is
  lost (any overwrite PUT changes the ETag regardless).
- **Objects above `--max-body-bytes` (default 5 GiB) are skipped** —
  recompact buffers the stored body, the decoded original, and the
  re-framed output for the decode + roundtrip verify.
- **CPU cost is real**: zstd-19 encodes orders of magnitude slower
  than zstd-3 (`zstd -b3` vs `-b19` on the e2e log corpus: ~1930 MB/s
  vs ~3.4 MB/s on one desktop core; decode speed is unaffected) — that
  is exactly why this runs nightly on cold data instead of on the PUT
  hot path.

### Policy-driven maintenance (`s4 maintain`, v1.2)

`migrate` and `recompact` are one-bucket, one-action invocations; in
practice you chain several of them in cron. `s4 maintain` lifts that
into a single declarative TOML policy that also adds a third action,
`transition` (storage-class changes with sidecar pairing — see below):

```toml
# s4-maintain.toml — rules run sequentially, top to bottom
[[rule]]
name = "compress-new-logs"        # required, unique
bucket = "prod-logs"              # required
prefix = "app/"                   # optional
action = "migrate"                # migrate | recompact | transition
older-than = "7d"                 # optional age gate, all actions

[[rule]]
name = "bake-cold-logs"
bucket = "prod-logs"
prefix = "archive/"
action = "recompact"              # action params = the CLI flags:
target-zstd-level = 19            #   no-tags / concurrency / max-objects /
older-than = "30d"                #   min-gain-percent / … same names, same defaults

[[rule]]
name = "cool-app-logs"
bucket = "prod-logs"
prefix = "app/"
action = "transition"
older-than = "90d"
storage-class = "GLACIER_IR"      # required for transition
```

```bash
s4 maintain --policy s4-maintain.toml --endpoint-url https://s3.example.com            # dry-run
s4 maintain --policy s4-maintain.toml --endpoint-url https://s3.example.com --execute  # apply
```

Like every offline tool here it is **dry-run by default** and must
point at the **backend**, not an S4 gateway. The policy is fully
validated up front — unknown keys, unknown actions, duplicate rule
names, malformed durations and action/parameter mismatches are all
reported in one pass before any rule runs. `migrate` / `recompact`
rules call the exact same library paths as the stand-alone subcommands
(identical selection, mandatory roundtrip verify, ETag race guard,
sidecar handling, skip taxonomy); `older-than` on a migrate rule
applies the same conservative `LastModified` gate as
`recompact --older-than`.

The new `transition` action changes the storage class of cold objects
via a same-key server-side `CopyObject` — the programmatic twin of the
lifecycle configuration in [`../storage-class-transitions.md`](../storage-class-transitions.md), with
one S4-specific guarantee a generic lifecycle filter cannot give you:
**the `<key>.s4index` sidecar always accompanies its main object into
the same class** (and a sidecar that drifted in an earlier interrupted
run is realigned), so the pair never splits the way a size- or
suffix-filtered lifecycle rule can. Sidecars are never transitioned on
their own. Skip taxonomy follows the house style:
`already-target-class` (the idempotency core), `too-recent`,
`etag-raced`, `too-large` (single `CopyObject` caps at 5 GiB). The
copy itself is pinned with `x-amz-copy-source-if-match` (audit round
1), so a concurrent overwrite makes the backend refuse atomically
(counted as `etag-raced`) instead of stamping stale metadata onto new
bytes. `Expires` / `WebsiteRedirectLocation` are re-sent alongside
content headers and user metadata; caveats that remain (stated in the
report notes): a backend-SSE original is re-encrypted under the bucket
default key, and a multipart-uploaded original becomes single-part
(backend recomputes the checksum, the ETag changes, and an existing
sidecar's ETag binding falls back to full-read until the next rewrite).

Example run against MinIO (the `maintain_minio` e2e seed shape: two
plain text logs under `app/`, one zstd-3-framed log under `archive/`).
The capture used the policy above **minus the `older-than` gates and
with `storage-class = "REDUCED_REDUNDANCY"`** — freshly seeded demo
objects would all skip as `too-recent` under the age gates, and
`REDUCED_REDUNDANCY` is the only non-STANDARD class MinIO accepts
(keep `GLACIER_IR` for AWS). Output verbatim, per-rule note blocks
elided for space:

```
S4 maintain — execute
  rules: 3 (3 run, 0 failed)

=== rule "compress-new-logs" — migrate prod-logs/app/ ===
S4 migrate prod-logs/app/ — execute
  objects: 2   total: 4.8 MiB (5075120 bytes)
  migrated: 2 object(s), 4.8 MiB -> 218.0 KiB (saves 4.6 MiB)
  skipped: 0 already-s4, 0 not-compressible, 0 too-large, 0 etag-raced, 0 verify-failed, 0 tags-unreadable
  failed: 0
  codecs: cpu-zstd×2
  …

=== rule "bake-cold-logs" — recompact prod-logs/archive/ ===
S4 recompact prod-logs/archive/ — execute
  target zstd level: 19   min gain: 3%
  objects: 1   total: 212.1 KiB (217223 bytes)
  recompacted: 1 object(s), 212.1 KiB -> 183.0 KiB (saves 29.2 KiB)
  …

=== rule "cool-app-logs" — transition prod-logs/app/ ===
S4 transition prod-logs/app/ — execute
  target storage class: REDUCED_REDUNDANCY
  objects: 2   total: 218.0 KiB (223247 bytes)
  transitioned: 2 object(s) + 1 sidecar(s)
  skipped: 0 already-target-class, 0 too-recent, 0 etag-raced, 0 too-large
  failed: 0
  …

Notes:
  - rules run sequentially against the bucket's current state; a dry-run cannot simulate the effects of earlier rules in the same policy (e.g. a transition rule's dry-run does not see the sidecars a preceding migrate rule would create)
```

A second `--execute` run skips everything — all three actions are
idempotent with no checkpoint file (same run, output verbatim):

```
  skipped: 2 already-s4, 0 not-compressible, 0 too-large, 0 etag-raced, 0 verify-failed, 0 tags-unreadable
  skipped: 0 not-s4, 1 already-compacted, 0 unsupported-codec, 0 unstamped-framed, 0 insufficient-gain, 0 too-large, 0 etag-raced, 0 too-recent, 0 tags-unreadable
  skipped: 2 already-target-class, 0 too-recent, 0 etag-raced, 0 too-large
```

`--interval 24h` replaces the cron line entirely: the command stays
resident (run → sleep → re-run), logs each cycle structurally instead
of printing reports, and exits gracefully on SIGTERM / SIGINT —
finishing the rule in flight first, never mid-rule. Rule failures in
resident mode are logged and the loop keeps cycling (idempotence makes
the next cycle the retry); in one-shot mode any failed rule exits 1.
`--format json` emits the full structured report
(`s4_server::maintain::MaintainReport` serde shape, per-rule
`MigrateReport` / `RecompactReport` / `TransitionReport` nested).

Honest limitations:

- **A dry-run cannot simulate rule interactions** — each rule's
  dry-run sees the bucket as it is now, not as earlier rules would
  leave it (the report repeats this in `notes`).
- **`transition` is a `CopyObject`**, so on versioning-enabled buckets
  the previous version stays behind (double-billed until expired), the
  ETag can change for multipart-uploaded or SSE-encrypted originals
  (sidecar ETag binding falls back to full-read until the next gateway
  write — perf-only), objects already in `GLACIER` / `DEEP_ARCHIVE`
  need a restore before they can move, and single-op copies cap at
  5 GiB (`too-large`).
- **SSE-enabled deployments are rejected** (`--sse-s4-key` /
  `--kms-local-dir`) — same scope guard as `migrate` / `recompact`.
- **One endpoint per run**: every rule in a policy file runs against
  the same `--endpoint-url` backend.
