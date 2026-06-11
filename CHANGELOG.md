# Changelog

All notable changes to S4 will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`s4 maintain --policy <FILE> [--execute] [--interval <DUR>]
  [--format table|json]`**: policy-driven bucket maintenance. A TOML
  file of `[[rule]]` entries (unique `name`, `bucket`, optional
  `prefix`, common `older-than` age gate) runs sequentially top to
  bottom; `action = "migrate" | "recompact"` reuse the v1.1 library
  paths with the same parameters as their CLI flags (`no-tags`,
  `target-zstd-level`, `min-gain-percent`, …), and the new
  `action = "transition"` (`storage-class = "GLACIER_IR"` etc.)
  changes cold objects' storage class via same-key server-side
  CopyObject with the `<key>.s4index` sidecar always accompanying its
  main object into the same class (drift from earlier partial runs is
  realigned; sidecars are never moved on their own). Dry-run by
  default; policy validation reports every problem in one pass;
  `--interval` keeps the command resident (run → sleep → re-run,
  structured per-cycle logs, graceful SIGTERM/SIGINT that finishes the
  in-flight rule). All three actions are idempotent, so re-runs and
  resident cycles skip settled objects
  (`already-s4` / `already-compacted` / `already-target-class`).
- **Savings ledger** (`--savings-ledger-state-file <PATH>`, opt-in,
  default-off): the gateway maintains measured per-bucket cumulative
  counters — `original_bytes` (logical client-PUT bytes),
  `stored_bytes` (backend bytes actually written: frames + SSE
  envelope + sidecars) and `objects` — updated on PUT /
  CompleteMultipartUpload / CopyObject / DELETE (overwrite = footprint
  swap via a best-effort HEAD probe; the extra HEADs exist only with
  the flag set). State is loaded with the standard `--*-state-file`
  fault isolation, flushed atomically on every write event, and
  re-dumped on SIGUSR1. Scope (honest): gateway-traversing writes only
  — backend-direct writes, `s4 migrate` / `s4 recompact`,
  aborted-multipart part bytes and replication replicas are not
  observed.
- **`s4 savings --state-file <PATH> [--price-per-gb-month 0.023]
  [--format table|json]`**: read-only report over the ledger state
  file (per-bucket + total original/stored bytes, savings ratio,
  $/month at the given price) — the measured twin of `s4 estimate`.
  Works while the gateway is running; fixed honesty notes are part of
  the output.
- **Prometheus gauges
  `s4_ledger_{original_bytes,stored_bytes,objects}{bucket}`** mirroring
  the ledger state file (never registered when the flag is off), plus
  a drop-in Grafana dashboard at
  `contrib/grafana/s4-savings-dashboard.json` (saved bytes / savings
  ratio / per-bucket split / $-per-month with a `price_per_gb_month`
  variable; import steps in `docs/observability.md`).

## [1.1.0] — 2026-06-11

**v1.1 — adoption tooling + small-object compression.** Six additive
features (`s4 estimate` / `s4 migrate` / zstd dictionaries +
`s4 train-dict` / `s4fs` fsspec adapter / `s4 recompact` / GPU batched
small-PUT compression) hardened by a 3-round dual-reviewer audit
(Claude ×3 + Codex; findings 20 → 7 → 5, P1/P2 zero at round 3). The
v1.0 freeze contract holds: every change below is additive and
default-off; flag-less PUT/GET behavior is bit-for-bit unchanged.

### Fixed (audit round 2 — adversarial verification of the round-1 fix wave)
- **P2** `CreateMultipartUpload` now strips client-supplied `s4-*`
  metadata like `put_object` does — a forged `x-amz-meta-s4-encrypted`
  could otherwise survive onto a completed multipart object and 5xx a
  flag-less GET (multipart re-open of the round-1 PUT fix).
- **P2** `migrate` / `recompact` no longer hard-fail every object when
  `GetObjectTagging` is denied or unimplemented: such objects skip as
  `tags-unreadable` (data is never rewritten tag-less), `NoSuchTagSet`
  counts as "no tags", and a new `--no-tags` flag opts out of tag
  inheritance entirely. Transient tagging errors still fail hard.
- **P2** Version-pinned CopyObject (`?versionId=`) probes the *pinned*
  source version — not the latest — for both the REPLACE metadata merge
  and cross-bucket dictionary propagation.
- **P3** Dictionary size cap (1 MiB) is now one consistent contract:
  `train-dict --max-dict-bytes` and `--zstd-dict` boot preload reject
  what a flag-less gateway's lazy fetch would refuse.
- **P3** Boot-preloaded dictionaries are bucket-scoped, fetched per
  `(bucket, id)` with `s4-dict-sha256` verification, and the server
  refuses to boot when one dict-id resolves to different bytes across
  buckets (16-hex prefix collision).
- **P3** `s4 estimate` excludes already-S4 objects (gateway metadata or
  `S4F2`/`S4P1`/`S4E*` magic) from sampling so re-estimating a
  gateway-operated bucket doesn't measure framed/encrypted bytes as if
  they were compressible plaintext (`already_s4` count + note).
- **P3** (s4fs) the sidecar staleness check reuses a cached live-info
  snapshot instead of issuing a second backend HEAD per `info()`.
  Trade-off disclosed: external overwrites during one filesystem
  instance's lifetime are detected on the next `invalidate_cache()` /
  new instance, not per-read (same contract as the metadata cache).

### Fixed (audit round 3 — convergence check)
- **P3** `s4 estimate`'s already-S4 body detection is structurally
  validated (known codec id + payload fits the object for `S4F2`,
  plausible padding length for `S4P1`) so customer data that merely
  starts with the 4-byte magic isn't silently dropped from sampling.
- **P3** README/CHANGELOG drift from the round-1/2 fixes corrected:
  dictionary 1 MiB cap is documented as one three-surface contract,
  migrate/recompact sample outputs show the full current skip taxonomy,
  `--no-tags` / `tags-unreadable` / `already-s4` estimate exclusions
  documented.

### Fixed (audit round 1 — 4 reviewers over v1.0.0..HEAD, 2026-06-11)
- **P1** `s4 migrate` could rewrite `.s4dict/<id>` dictionary objects as
  S4F2-framed data, breaking every `cpu-zstd-dict` object in the bucket
  (lazy fetch fails fingerprint verification). All three bulk tools
  (`estimate` / `migrate` / `recompact`) now exclude S4-internal keys:
  `*.s4index`, `.s4dict/`, and `*.__s4ver__/*` versioning shadows.
- **P1** A client-supplied `x-amz-meta-s4-dict-id` on a plain PUT made
  the subsequent GET fail 5xx even with `--zstd-dict` unset (default-off
  behavior regression). The GET dict branch is now gated on the
  gateway-managed manifest codec (`cpu-zstd-dict`), and `put_object`
  strips client-supplied `s4-*` metadata keys up front.
- **P1** (s4fs) SSE-encrypted objects could return AES-GCM ciphertext
  bytes silently (`passthrough` + SSE). s4fs now refuses with
  `NotImplementedError` via three layers: `s4-encrypted` metadata,
  sidecar SSE binding, and `S4E1`–`S4E6` magic sniff.
- **P1** (s4fs) `<key>.__s4ver__/<version>` shadow objects were not
  hidden from `ls`/`find`/glob (prefix check instead of infix), so
  directory dataset scans could silently include stale versions.
- **P2** `migrate` / `recompact` rewrites dropped the source object's
  storage class (silent promotion to STANDARD) and object tags; both
  are now inherited. ACLs / Object Lock retention remain uninherited
  (stated in report notes).
- **P2** `migrate` treated a roundtrip-verify failure as a skip
  (exit 0); it is now a hard failure (exit 1), matching `recompact`.
  The `skipped_verify_failed` JSON field remains (always 0) for shape
  compatibility.
- **P2** Cross-bucket CopyObject of a dict-compressed object now
  propagates `.s4dict/<id>` to the destination bucket (idempotent,
  content-addressed); previously the copy succeeded but every GET on
  the destination failed 5xx.
- **P2** `.s4dict/` joined the reserved-key guard: gateway PUT / DELETE
  are rejected with `InvalidObjectName` (reads still allowed) so a
  bucket-wide dictionary can't be destroyed through the data path.
- **P2** (s4fs) `info()` no longer trusts a stale sidecar for object
  size (staleness-checked first), and binding-less legacy v1 sidecars
  are no longer used for size or partial range reads.
- **P2** (s4fs) dependency floor corrected to `s4-codec>=1.1.0,<2` —
  the binding APIs s4fs imports don't exist in the 1.0.0 wheel.
- **P3** `estimate` no longer aborts the whole run when a sampled
  object 404s mid-run (skip + note); module/report now disclose the
  single-stream measurement bias vs the server's 4 MiB chunking.
- **P3** `migrate` / `recompact` enforce `--max-body-bytes` from the
  GET `Content-Length` before buffering; `migrate` now also cleans up a
  stale multi-frame sidecar when its rewrite comes out single-frame.
- **P3** `recompact` no longer auto-promotes backend-written framed
  objects that lack gateway metadata (`unstamped-framed` skip; opt back
  in with `--assume-unstamped-framed`).
- **P3** Dict hardening: `DictCache` is bucket-scoped, `train-dict`
  stamps `s4-dict-sha256` (full-digest verification when present), and
  lazy fetch caps dictionaries at 1 MiB. (s4fs) `open()` on a framed
  object with inexact size raises instead of silently truncating
  (`allow_inexact_open=True` restores the old clamp).
- **P3** `nvcomp_batched` validates device-reported chunk sizes on the
  host before the unsafe copy (typed per-item error instead of a
  potential OOB read on driver misbehavior).

### Added
- **`--gpu-batch-small-puts`** (opt-in, requires the `nvcomp-gpu` build +
  a CUDA-capable GPU at boot — the server refuses to start otherwise) —
  batch **concurrent small PUTs** into a single nvCOMP batched-zstd
  kernel launch so the GPU pays its fixed launch + PCIe cost once per
  batch instead of once per object. Eligibility: sampling dispatcher
  picked `cpu-zstd`, no `--zstd-dict` prefix match, declared
  `Content-Length` in `[--gpu-batch-floor-bytes (default 4 KiB),
  --gpu-min-bytes (default 1 MiB))`. Companion knobs:
  `--gpu-batch-max-items` (flush at N pending bodies, default 32) and
  `--gpu-batch-window-ms` (flush after T ms, default 4 — also the
  worst-case latency the batch path adds to a PUT). **Wire format is
  unchanged**: batched objects are byte-layout-identical standard
  `nvcomp-zstd` bodies (same FCG1 framing + `CodecKind::NvcompZstd`
  manifest as the per-object GPU path; no new codec id, no new
  metadata) and the GET path has zero batch awareness — proven by
  GPU-gated tests that decompress batch output through the unmodified
  per-object path, plus a MinIO e2e (`tests/gpu_batch_e2e.rs`).
  Fail-open semantics: queue full (backpressure), GPU error, or a
  batched result that is not smaller than the input all fall back to
  the pre-existing cpu-zstd framed path — observable via the new
  `s4_gpu_batch_total{result="batched"|"fallback"}` counter. Measured
  on 1000 × 8 KiB log-like objects (RTX 4070 Ti SUPER, nvCOMP
  5.2.0.10): batched GPU = 29.7 ms vs 702 ms per-object GPU (~24×) vs
  15.7–19.5 ms single-thread cpu-zstd-3; GPU output ~10% smaller
  (12.31× vs 11.14× ratio). Honest verdict in README §"GPU small-PUT
  batching": this offloads CPU and improves ratio — it does not beat a
  free CPU core on raw wall time at 8 KiB. New public surface:
  `s4_codec::nvcomp_batched::NvcompZstdBatchEncoder` (feature-gated),
  `s4_server::gpu_batch` (aggregator + `GpuBatchHandle`),
  `S4Service::with_gpu_batch`, and the `gpu_small_batch` bench. Flag
  off (default) = bit-for-bit unchanged PUT behaviour.
- **`s4 recompact <bucket>[/prefix] --endpoint-url <BACKEND> [--execute]`** —
  rewrite cpu-zstd framed objects at a higher zstd level during a quiet
  window (LSM-compaction for S3). The gateway's PUT path favours latency
  (`--zstd-level`, default 3); recompact decodes each S4-framed cpu-zstd
  object in-process (same `FrameIter` walk as the GET path — doubles as
  an integrity check on the stored frames), re-frames the original bytes
  with the same `streaming_compress_to_frames` + `pick_chunk_size` pair
  the PUT path uses at `--target-zstd-level` (default 19), and overwrites
  only when the new frames shrink the **stored** bytes by
  `--min-gain-percent` (default 3%). Rewritten objects are stamped with
  new `s4-zstd-level` metadata (recompact-only stamp — the gateway
  neither reads nor writes it), making re-runs idempotent
  (`already-compacted` skip) with no checkpoint file.
  `--older-than <DUR>` (`30d` / `12h` / `45m` / `90s`) restricts the run
  to cold objects by backend `LastModified`. Dry-run by default;
  mandatory decompress-roundtrip byte comparison before every write (no
  off switch) and a pre-PUT HEAD ETag re-check (narrows, does not close,
  the concurrent-writer race). Skip taxonomy: `not-s4` (run `s4 migrate`
  first) / `already-compacted` / `unsupported-codec` (passthrough,
  `cpu-gzip`, `nvcomp-*`, `cpu-zstd-dict` — this tool is cpu-zstd →
  cpu-zstd only) / `unstamped-framed` (audit round 1: backend-written
  frames without gateway metadata; opt in with
  `--assume-unstamped-framed`) / `insufficient-gain` / `too-large`
  (`--max-body-bytes`, default 5 GiB) / `etag-raced` / `too-recent` /
  `tags-unreadable` (audit round 2; `--no-tags` opts out of tag
  inheritance). Multi-frame rewrites
  refresh the `<key>.s4index` sidecar; single-frame rewrites delete a
  now-stale one. `--concurrency` (default 4), `--max-objects`,
  `--format table|json`; exit 1 iff any object failed. SSE-enabled
  deployments are rejected (same guard as migrate). New library module
  `s4_server::recompact` (`run_recompact`, `RecompactParams`,
  `RecompactReport`, `RecompactError` `#[non_exhaustive]`,
  `parse_duration_suffix`). Additive only — no existing flag, metadata
  key, or default changed (`s4-server` internals: a handful of private
  `migrate` helpers became `pub(crate)` for reuse, behaviour unchanged).
- **`s4 estimate <bucket>[/prefix] --endpoint-url <BACKEND>`** — read-only
  pre-deployment savings simulator. Lists the bucket (`.s4index` excluded,
  capped at `--max-list-keys`), stratifies objects by extension, samples
  `--samples-per-stratum` objects per stratum (size-weighted, deterministic
  under `--seed`), compresses the sampled bytes with the same
  `SamplingDispatcher` pick the gateway would make at PUT time (honoring
  `--codec` / `--dispatcher` / `--zstd-level` / `--gpu-min-bytes` /
  `--prefer-columnar-gpu`), and extrapolates projected storage bytes and
  $/month (`--price-per-gb-month`, default 0.023). `--format table|json`.
  Never executes GPU codecs: `nvcomp-*` picks are measured via a cpu-zstd
  proxy with an explicit report note. New library module
  `s4_server::estimate` (`run_estimate`, `EstimateParams`,
  `EstimateReport`, `EstimateError` `#[non_exhaustive]`). Additive only —
  no existing flag or default changed.
- **`s4 migrate <bucket>[/prefix] --endpoint-url <BACKEND> [--execute]`** —
  bulk retro-compression of pre-existing objects into the gateway's S4F2
  framed format (same `SamplingDispatcher` decision, same
  `streaming_compress_to_frames` framing + chunk-size policy, same
  `s4-codec`/`s4-framed` metadata and `<key>.s4index` sidecar contract as
  the PUT path — gateway GETs decompress migrated objects transparently).
  Dry-run by default; `--execute` to write. Already-S4 objects (frame
  magic or `s4-codec` metadata) are skipped, so re-runs resume
  automatically without a checkpoint file. Every write requires an
  in-process decompress-roundtrip byte comparison (no off switch) and a
  pre-PUT HEAD ETag re-check (narrows, does not close, the concurrent-
  writer race — documented). Skip taxonomy: `already-s4` /
  `not-compressible` (passthrough pick or no size gain; object untouched)
  / `too-large` (`--max-body-bytes`, default 5 GiB) / `etag-raced` /
  `tags-unreadable` (audit round 2; `--no-tags` opts out of tag
  inheritance). A roundtrip-verify failure is a hard failure (exit 1)
  since the round-1 audit — the `skipped_verify_failed` JSON field
  remains for shape compatibility but is always 0.
  `--concurrency` (default 4), `--max-objects`,
  `--format table|json`; exit 1 iff any object failed. GPU / `cpu-gzip`
  dispatcher picks really fall back to `cpu-zstd` at `--zstd-level`
  (reported as `picked != wrote_with`). SSE-configured invocations are
  rejected; versioning-Enabled buckets get a double-billing `WARNING`
  note. New library module `s4_server::migrate` (`run_migrate`,
  `MigrateParams`, `MigrateReport`, `MigrateError` / `SkipReason`
  `#[non_exhaustive]`). Additive only — no existing flag, default, or
  PUT/GET behavior changed.
- **Shared zstd dictionaries for small objects** (`s4 train-dict` +
  `--zstd-dict`) — new codec `cpu-zstd-dict` (**codec id 8**; additive:
  the S4F2 frame layout is unchanged, only a new id is allocated).
  `s4 train-dict <bucket>/<prefix> --endpoint-url <BACKEND>
  [--max-samples 1000] [--max-dict-bytes 112640] [--min-samples 8]
  [--sample-max-bytes 65536]` samples small raw objects under the prefix
  (already-S4 bodies skipped), trains a stock zstd dictionary
  (`zstd::dict::from_samples` / ZDICT), stores it at the content-addressed
  in-bucket object `.s4dict/<dict-id>` (`<dict-id>` = first 16 hex of the
  dictionary's SHA-256; immutable, idempotent re-train), and prints the
  gateway flag. The gateway flag `--zstd-dict
  '<bucket>/<key-prefix>=<dict-id>'` (repeatable; dictionaries fetched +
  fingerprint-verified at boot, missing dict = boot error) makes
  single-PUT cpu-zstd bodies ≤ `--zstd-dict-max-bytes` (default 1 MiB)
  whose key longest-prefix-matches compress against the dictionary —
  **only when it actually beats dict-less cpu-zstd** (both compressed and
  compared per small PUT; ties / losses fall back to a plain `cpu-zstd`
  frame with no dict reference). The dictionary id travels in the new
  `s4-dict-id` object-metadata key, never in the frame. GETs resolve the
  dictionary preloaded → LRU → lazy backend fetch of `.s4dict/<id>`
  (fingerprint-verified, ~16-entry cache), so a gateway booted **without**
  the flag still reads dict-compressed objects; fetch failures are 5xx +
  the new `s4_dict_fetch_total{result}` counter. `.s4dict/` keys are
  hidden from gateway listings (same treatment as `.s4index` /
  `.__s4ver__/`). Measured on the minio E2E (100 × ~300-byte
  same-schema JSON events): 8 903 bytes stored vs 21 923 dict-less =
  2.46×. No lock-in: the payload is a stock zstd frame and `.s4dict/<id>`
  is raw zstd dictionary bytes — `zstd -D <dictfile> -d` decodes without
  any S4 software (pinned by the E2E against the real CLI). New modules
  `s4_codec::cpu_zstd_dict` (`CpuZstdDict`, `train_from_samples`,
  blocking helpers) and `s4_server::dict` (`DictStore`, `DictCache`,
  `run_train_dict`, …). **Compatibility note:** pre-v1.1 readers fail a
  GET of a `cpu-zstd-dict` object with the existing *unknown codec id*
  error (graceful typed failure, no silent corruption) — roll mixed
  fleets forward before enabling the flag. Multipart parts and
  `s4-codec-wasm` native decode are out of scope (follow-ups);
  `s4-codec-py` decodes dict objects via the `CpuZstdDict` binding
  added in this release, and cross-bucket CopyObject propagates the
  dictionary (see Fixed). Without `--zstd-dict`, PUT/GET behavior is
  bit-for-bit unchanged.
- **`s4fs` — fsspec filesystem for reading S4 objects without the gateway**
  (new pure-Python package [`python/s4fs/`](python/s4fs/), protocol
  `s4://`). pandas / pyarrow / DuckDB / Polars read gateway-written
  objects straight off the backend: S4F2 frames are decoded transparently
  (`passthrough` / `cpu-zstd` / `cpu-gzip` / `cpu-zstd-dict`, with
  `.s4dict/<id>` fetch + SHA-256-fingerprint verify), unframed
  metadata-manifest objects (`cpu-gzip`, legacy raw zstd) decode via the
  `s4-codec` / `s4-original-size` / `s4-crc32c` stamps, and non-S4
  objects pass through byte-for-byte. `ls` / `info` hide `.s4index` /
  `.s4dict/` / `.__s4ver__/` internals and report **original**
  (decompressed) sizes (sidecar → `s4-original-size` metadata →
  compressed size with `s4_size_exact: False`). Range reads / seeks use
  the `.s4index` sidecar (with source-ETag staleness check) to fetch only
  the overlapping frames; verified by the MinIO e2e to transfer fewer
  backend bytes than a full read. Read-only by design — every write API
  raises `NotImplementedError("s4fs is read-only; write through the S4
  gateway")`; GPU frames (`nvcomp-*` / `dietgpu-ans`) raise
  `NotImplementedError` instead of decoding wrong. The underlying
  filesystem defaults to s3fs (`[s3]` extra) and is injectable
  (`S4FileSystem(fs=...)`). Unit fixtures are real gateway-written bytes
  captured off MinIO (`tests/fixtures/generate_fixtures.py`); e2e
  (`pytest -m e2e`) covers pandas / pyarrow / DuckDB round-trips against
  MinIO + the real gateway.
- **`s4-codec` Python binding: wire-format read helpers** (additive,
  `crates/s4-codec-py`) — `read_frame(bytes)` / `frame_iter(bytes)`
  (S4F2 frame parse, S4P1 padding skipped; header dicts carry
  `codec` / `original_size` / `compressed_size` / `crc32c`),
  `decode_index(bytes)` (`.s4index` sidecar v1/v2/v3 → dict with
  `entries` / `total_original_size` / `source_etag` / `sse`),
  `crc32c(bytes)`, the `CpuZstdDict(dict_bytes, level=3)` codec class
  (same `compress` / `decompress` shape as `CpuZstd`), module constants
  `FRAME_MAGIC` / `PADDING_MAGIC` / `FRAME_HEADER_BYTES` /
  `SIDECAR_SUFFIX`, and exception classes `S4FrameError` / `S4IndexError`
  (⊂ `S4Error`). Existing API unchanged.

## [1.0.0] — 2026-06-09

**v1.0 — SemVer-stable surface freeze.** From v1.0 onward the items
enumerated in [`README.md` §"Stability — v1.0 guarantees"](README.md#stability--v10-guarantees)
are frozen for the v1.x line; any incompatible change to them ships
in a v2.0.0 release with migration recipes under `docs/migration/`.
**v1.0 is *not* a marketing claim that "S4 has been battle-tested at
every Fortune 500."** It is a contract that downstream consumers can
pin `s4-server = "1"` (or `s4-codec = "1"`, or `s4-config = "1"`, or
`ghcr.io/abyo-software/s4:1`) and rely on the surface listed in
`README.md`. First public production deployment reference is still
being collected — file an issue tagged `production-reference` if
you are running S4 at TB scale.

### Surface freeze — what's in the v1.0 contract

See `README.md` for the table. Briefly:
- Wire formats: `S4F2` framed body, `S4P1` padding, `S4IX` v1/v2/v3
  sidecars, `S4E1`/`S4E2`/`S4E3`/`S4E4`/`S4E5`/`S4E6` SSE envelopes
- `s4` binary subcommands (`verify-sidecar`, `repair-sidecar`,
  `sweep-orphan-sidecars`, `verify-audit-log`, plus the server's
  documented `--<flag>` set)
- `s4_server::repair::*` public API (verify/repair/sweep + all
  related error / report / policy types)
- `s4_server::service::S4Service` shape — `new(backend, registry,
  dispatcher)` constructor + every `pub fn with_*` builder signature
  (23 of them — exact list in README); + the `SharedService` newtype
  at `s4_server::service_arc::SharedService`; + `SigV4aGate` /
  `SigV4aGateError` / `resolve_range` / `DEFAULT_MAX_BODY_BYTES` /
  `DEFAULT_REPLICATION_MAX_CONCURRENT`
- `s4_server::sse` public surface (frozen types, functions, constants)
- `s4_server::streaming` public surface (frozen constants + functions)
- `s4-codec` codec trait + format constants (Codec trait shape;
  CodecKind / CodecError / IndexError / FrameError / GpuSelectError /
  CompareOp enums all `#[non_exhaustive]`; index module's pub structs
  + functions + constants; multipart::FrameHeader layout)
- `s4-config`: `CompressionMode` enum (`#[non_exhaustive]`) +
  `BackendConfig` / `S4Config` struct field sets
- HTTP API surface: `s3s 0.13` trait set (S3 wire compatibility)
- Container image tags + Helm chart `values.yaml` key set (full
  enumeration of 28 top-level keys in README)

### Added

- Stability section in `README.md` (§"Stability — v1.0 guarantees")
  enumerating the v1.0 freeze surface with explicit scope rules.
- `docs/security/cargo-audit-ignores.md` — per-advisory rationale +
  mitigation + upstream-tracking for the 4 accepted RUSTSEC ignores
  (2026-0098 / 2026-0099 / 2026-0104 / 2025-0134), with verification
  commands to re-check each fact.
- README "Backend compatibility matrix" sub-section inside §Stability
  documenting CI-verified state honestly: ✓ gating for MinIO; ⚠ opt-in
  for AWS/B2/R2/Wasabi (gate only when operator-configured secrets
  are set); ⚠ claimed-but-not-CI-verified for Garage + Ceph RGW with
  the specific drift symptoms documented.
- README "Modules NOT in the freeze list" sub-section enumerating
  the 25 `s4_server::*` modules that exist as `pub mod` for binary
  + tests needs but are NOT part of the v1.0 contract.
- README "How to read the freeze table — scope of 'frozen'"
  sub-section: items named in the table ARE the v1.0 contract; other
  `pub` items in those modules are NOT; pin `=1.x.y` if depending on
  unlisted items.
- README "v0.x → v1.0 source compatibility note" sub-section listing
  all 34 enums annotated `#[non_exhaustive]` (6 s4-codec + 27
  s4-server + 1 s4-config) + the mechanical consumer-side fix
  (add `_ =>` arm) for exhaustive matches.

### Changed

- 34 public enums on the frozen surface gained `#[non_exhaustive]`
  for forward-compat additive variants. **Source-level breaking
  change** for downstream code with exhaustive `match` arms; fix is
  mechanical (add `_ =>`). See README §"v0.x → v1.0 source
  compatibility note" for the full enum list and rationale.
- `pub fn encode_index_v1_for_test` (and other `_for_test` helpers)
  gated out of the v1.0 public API via `#[cfg(test)] pub(crate)`
  visibility + `#[doc(hidden)]`.
- `crates/s4-codec-py/pyproject.toml` PyPI trove classifier bumped
  from `Development Status :: 3 - Alpha` → `5 - Production/Stable`
  to match the v1.0 frozen-API contract.
- `SECURITY.md` Supported Versions section rewritten from "pre-1.0,
  latest commit on main" → "v1.x rolling window of latest minor +
  previous minor; patch releases on the affected minor's release
  branch".
- Backend compat matrix table in `compat-matrix.yml` now reflects
  the round-trip-vs-provisioning gate distinction; Garage and Ceph
  round-trips are `continue-on-error` with explicit warning steps
  documenting the wire-shape drift symptoms.
- README disclaimers updated from alpha / early-access / pre-1.0
  framing to the v1.0 "surface freeze ≠ production track record"
  narrative.
- Helm chart `values.yaml` key set is now frozen at v1.0; key shape
  changes are v2.0 territory. Chart's own `version` stays in 0.2.x
  (Helm-side SemVer, independent of appVersion); `appVersion` bumps
  to `1.0.0`.
- `crates/s4-codec-py/README.md` + Cargo.toml + pyproject.toml
  metadata updated from "GPU/CPU compression" to "CPU compression"
  to match what the Python module actually exports in v1.0
  (`CpuZstd` + `CpuGzip` only; GPU codec classes are intentionally
  NOT exposed in v1.0).
- `crates/s4-codec-wasm/README.md` status header updated from
  "v0.4 #24 — initial cut" to "v1.0 — frozen public API".
- `.github/workflows/ci.yml` `security-audit` job comment corrected:
  `rustls-pemfile` is a runtime dep (used by the production HTTPS
  listener in `tls.rs`), not "dev-only" as the prior comment claimed.

### Fixed

- `compat-matrix.yml` Garage start step: replaced over-broad
  `awk '/HEALTHY|UNHEALTHY|NO ROLE/'` that matched the
  `==== HEALTHY NODES ====` table header line in
  `dxflrs/garage:v1.1.0` output (producing `NODE_ID="===="` and a
  hard-fail at `layout assign`). Now uses `garage node id -q`
  directly, which returns `<hex>@<addr>`.
- `compat-matrix.yml` Ceph RGW + Garage round-trip steps: marked
  `continue-on-error` because `quay.io/ceph/demo:latest-quincy` is
  unmaintained upstream (XAmzContentSHA256Mismatch) and
  `dxflrs/garage:v1.1.0` rejects current aws-sdk-rust's
  STREAMING-AWS4-HMAC-SHA256-PAYLOAD (Invalid payload signature).
  Provisioning steps still gate for both.

### Roadmap candidates (v1.x, additive only)

- Chunked SSE-KMS envelope (provisional `S4E7`) + chunked SSE-C
  (provisional `S4E8`) for Range GET partial-fetch fast-path.
- `S4F3` streaming frame format enabling streaming PUT checksum
  verify for multipart `upload_part`.
- 32-bit runtime smoke promoted from advisory to required CI gate.
- Per-action SHA pinning on GHA workflows.
- Cross-region replication promoted from experimental scaffolding
  to production-grade with Jepsen-style consistency tests.
- Re-introducing Garage + Ceph as `✓ gating` once upstream signature
  / image issues resolve.
- GPU codec exposure in the Python module.
- Streaming decoder API in the WASM module.
- npm publish automation for the WASM package.
- Japanese README (`README.ja.md`) brought current to v1.0.

### Audit history

7 rounds of dual-reviewer (Opus + Codex) adversarial audit drove
~30 individual findings to closure across this cycle:
- R0 (pre-session, on v1.0 draft README): Opus + Codex, 13 findings
  spanning enum non_exhaustive coverage, README freeze accuracy,
  s3s 0.13 policy, cargo-audit ignores doc, compat-matrix evidence,
  cross-major back-compat caveats.
- R1: Cluster A (F1 + F2 + F3 sub-agent parallel fixes) + Cluster B
  (main-session README + audit-ignores doc rewrite) + Cluster C
  (compat-matrix manual triggers + Garage / Ceph best-effort wrap).
- R2: NF-1 — `SharedService` path correction (`s4_server::service` →
  `s4_server::service_arc`).
- R3 (dual reviewer): 11 new findings → fix wave including
  `S4Service::default` fabrication removal, cloud-backend opt-in
  honest qualifiers, S4Service builder-param contradiction caveat,
  FrameIndex inner-type freeze, v0.x→v1.0 source-break caveat.
- R4: 4 P2 + 1 P3 — Python class name correction, enum list
  completeness, SECURITY.md update, FrameIndex own-field freeze.
- R5 (dual): scope-explicit freeze sub-section + Python exception
  enumeration + binding README updates.
- R6 (dual, split verdict): Codex P1/P2/P3 closures — Python pkg
  GPU marketing removal, PyPI classifier bump, CompressionMode
  non_exhaustive.
- R7 (dual): `s4 = "1"` → `s4-server / s4-codec / s4-config = "1"`,
  freeze-scope enum-list wording correction, Python README GPU
  build-recipe v1.0 caveat, EOF whitespace, SOCIAL_POSTS.md
  historical-artifact banner.

### Cut-commit changes

- `Cargo.toml`: workspace.version `0.11.0` → `1.0.0`
- `crates/s4-server/Cargo.toml`: internal-dep pins
  `s4-codec`, `s4-config` `"0.11"` → `"1"`
- `crates/s4-codec-wasm/Cargo.toml`: internal-dep pin
  `s4-codec` `"0.11"` → `"1"`
- `crates/s4-codec-py/Cargo.toml`: internal-dep pin
  `s4-codec-rs` `"0.11"` → `"1"` (already landed in round-7 wave;
  noted here for completeness)
- `charts/s4/Chart.yaml`: `appVersion` `0.11.0` → `1.0.0`;
  chart's own `version` `0.2.2` → `0.2.3` (appVersion bump only,
  no chart-shape change)

## [0.11.0] — 2026-06-08

Polish + maintenance cut. Wave-1 three-theme delivery (32-bit
end-to-end smoke, GHA Node.js 24 migration, backend compat
matrix) converged by a 6-round integrated audit (4 P2 + 1 P1
real fixes, 2 false-positive rounds caused by Codex sandbox
network limits — documented inline). Net diff vs v0.10.0:
~12 files / ~1,400 lines across the GHA workflows + docs +
composite actions. No production code changes.

Headline additions:
- **32-bit `s4-server` runtime end-to-end PUT/GET smoke** —
  the v0.10 #A4 `--help`/`--version` smoke is now a full
  MinIO-backed PUT/GET round-trip exercising the i686
  hyper/rustls listener, aws-sdk-rust SigV4 signer, and
  CPU-zstd codec paths.
- **GHA Node.js 24 migration** — 11 JavaScript actions
  bumped to their Node 24-ready majors ahead of the 2026-09
  deprecation deadline. actionlint clean.
- **Backend compatibility matrix CI** — new weekly workflow
  exercises a PUT/GET + sidecar HEAD round-trip across
  MinIO, Garage, Ceph RGW, Backblaze B2, Cloudflare R2, and
  Wasabi. Docker tier runs every cron; real-cloud tier
  gates on operator-provided secrets and skips silently
  otherwise.

Audit posture: per-feature audits (A4=3R, A5=1R, A7=2R) +
6-round integrated audit catching SLSA/SBOM regression,
OCI label regression, run-key staleness, merge-job
coupling, partial multi-arch publish (P1). v0.11.0
publishes from R6 (effective convergence after two
sandbox-limited false-positive rounds).

Cleanup recipe for shipped v0.9.0 / v0.10.0 images missing
the labels + attestations the docker.yml regressions
dropped: re-trigger `docker.yml` from this commit (`gh
workflow run docker.yml --ref main -f build_ref=v0.10.0 -f
image_tag_override=0.10.0 -f push=true`) — per-arch
rebuilds attach the labels + SLSA + SBOM, merged manifest
overwrites the prior labels-less manifest.

### Added

- **v0.11 #A4 — 32-bit end-to-end PUT/GET smoke in CI.** The
  existing `i686-runtime-smoke` job (added in v0.10 wave-2 #A4 to
  cover `cargo test` on the codec / config crates + `--help` /
  `--version` runtime of the `s4` binary) now also runs a stock
  MinIO container on the host, starts the **i686 `s4` binary** in
  front of it, and exercises a full `aws s3 mb` + `aws s3 cp` PUT
  + `aws s3 cp` GET round-trip with byte-equality check on the
  body. The PUT/GET step lands as advisory (`continue-on-error: true`)
  so a first-time 32-bit runtime bug surfaces in the job log
  without turning CI red while a fix follows in a v0.11.x commit;
  the README §"Supported targets" 32-bit row is updated to reflect
  the new claim and the advisory caveat. Job timeout bumped from
  the implicit default to **40 min** to absorb MinIO pull + start
  + the round-trip itself. Server log is uploaded as a CI artifact
  (`s4-i686-server-log-${{ github.run_id }}`) for post-mortem.

- **v0.11 #A7 — backend compatibility matrix CI.** New
  [`compat-matrix.yml`](.github/workflows/compat-matrix.yml) workflow
  closes the long-standing "should work" gap on the README's S3-
  compatible backend list. Prior to this, only MinIO (via per-PR
  testcontainers) and real AWS S3 (via nightly `aws-e2e.yml`) had
  CI-verified compat evidence; Garage / Ceph RGW / Backblaze B2 /
  Cloudflare R2 / Wasabi rested on wire-shape similarity alone.
  Two tiers, weekly cron (Sunday 06:00 UTC) + `workflow_dispatch`:
  (1) docker tier with no secrets — MinIO
  (`quay.io/minio/minio:latest`), Garage (`dxflrs/garage:v1.1.0`,
  CLI-provisioned single-node `replication_mode = "none"` cluster),
  and Ceph RGW (`quay.io/ceph/demo:latest-quincy`, best-effort with
  `continue-on-error` because the upstream demo image is no longer
  actively maintained — pull / startup failures surface as warnings
  rather than blocking the matrix); (2) real-cloud tier for B2 / R2
  / Wasabi, each gated on operator-provided `vars.*_BUCKET` /
  `*_ENDPOINT` / `*_REGION` + `secrets.*_ACCESS_KEY_ID` /
  `*_SECRET_ACCESS_KEY` (silent skip when the backend isn't
  configured — same opt-in pattern aws-e2e.yml uses). Each job
  builds `s4` once via a shared `build-s4` job + artifact, then
  runs a 1 PUT + 1 GET + sidecar HEAD round-trip through `s4
  --codec cpu-zstd --dispatcher always` against the live backend;
  the sidecar HEAD on the **backend** (not s4) is the load-bearing
  assertion that the second backend PUT — where most S3-API-shape
  divergences would surface — landed cleanly. Shared step logic
  factored into a new `./.github/actions/compat-roundtrip`
  composite action so adding a 7th / 8th backend in the future
  doesn't require copy-pasting bash. README §"How it Compares"
  gains a new "Backend compatibility matrix" subsection enumerating
  each backend's verification posture (✅ verified / ⚠️ best-effort
  / 🔧 configurable in operator CI).

### Changed

- **v0.11 #A5 — GitHub Actions Node.js 24 migration.** GitHub is
  forcing all JavaScript actions to run on Node.js 24 by default
  on 2026-06-16, and removing the Node.js 20 runtime from runners
  on 2026-09-16 (deprecation announced 2026 spring). Every action
  reference across all nine workflows (`ci.yml`, `ci-close-resolved.yml`,
  `bench.yml`, `docker.yml`, `docker-smoke.yml`, `aws-e2e.yml`,
  `aws-kms-e2e.yml`, `fuzz-nightly.yml`, `compat-matrix.yml`) has
  been bumped to the first major release that runs natively on
  Node.js 24, so the deprecation warning is silenced and the
  workflows continue working past the September runtime removal.
  Concretely: `actions/checkout` @v4 → @v5, `actions/upload-artifact`
  @v4 → @v6 (v5 still ran on Node.js 20), `actions/download-artifact`
  @v4 → @v7 (v5 + v6 still ran on Node.js 20),
  `actions/github-script` @v7 → @v8, `codecov/codecov-action`
  @v4 → @v5, `docker/build-push-action` @v5 → @v7 (v6 still ran
  on Node.js 20), `docker/login-action` @v3 → @v4,
  `docker/setup-buildx-action` @v3 → @v4, `docker/metadata-action`
  @v5 → @v6, `aws-actions/configure-aws-credentials` @v4 → @v6
  (v5 still ran on Node.js 20), `azure/setup-helm` @v4 → @v5.
  Three actions already serve Node.js 24 under their existing
  floating major tag and were left untouched: `Swatinem/rust-cache@v2`
  (resolves to v2.9.1 = Node.js 24), `benchmark-action/github-action-benchmark@v1`
  (the `v1` branch is Node.js 24), and `dtolnay/rust-toolchain@stable`
  (composite action, no Node.js runtime). Action input / output
  contracts at the bumped majors are call-compat with the prior
  usage in this repo (verified per action's release notes — the
  most invasive jump, `aws-actions/configure-aws-credentials` @v4
  → @v6, only changed invalid-boolean handling we don't trip; the
  `codecov-action` v5 `file` → `files` rename was already adopted).

### Documentation

## [0.10.0] — 2026-06-07

Second v0.10-line cut (= first v0.10). Two-wave delivery of the
encryption-aware sidecar completion + Docker image distribution
theme + bench-driven hardening, converged by a 4-round integrated
audit (2 P2 fixes, clean R3 + R4). Net diff vs v0.9.0:
~12 files / ~1,800 lines across `s4-server`, the chart, the
distribution workflows, and the docs.

Headline additions (= wave-1):
- **`s4 repair-sidecar` now supports `--sse-s4-key`** — the v0.9
  encryption-aware sidecar (v3 SSE-S4 chunked) is now repairable
  from the CLI by supplying the SSE keyring. Closes the
  `EncryptedSidecarUnsupported` reject path v0.9 audit P2-INT-1
  introduced as a placeholder. New
  `s4_server::repair::repair_sidecar_with_keyring` lib entry +
  `RepairReport::sse_v3_binding`.
- **Official container images on ghcr.io** —
  `ghcr.io/abyo-software/s4:<version>` (multi-arch CPU) +
  `ghcr.io/abyo-software/s4:<version>-gpu` (nvCOMP GPU,
  amd64). SLSA provenance + SPDX SBOM. Helm chart default
  `image.repository` flipped to ghcr (chart `version` 0.1.0 →
  0.2.0, `appVersion` 0.9.0).
- **SSE partial-fetch AEAD constraint docs** — new
  `docs/security/sse-partial-fetch-constraint.md` explains why
  the v3 sidecar fast-path is SSE-S4 chunked (S4E6) only and
  why SSE-KMS / SSE-C / S4E2 stay buffered (AEAD whole-body
  tag).

Wave-2 hardening:
- **i686 runtime smoke in CI** — `s4-codec` / `s4-config` tests
  + `s4-server --help` / `--version` on i686. README §"Supported
  targets" cell flips from "⚠️ compiles, untested at runtime"
  to "✅ compiles + smoke (CI)".
- **Docker / Helm distribution smoke CI** — `helm lint` +
  `helm template` + `docker compose config` + `docker pull +
  --help` on every chart / Dockerfile / compose push. Catches
  distribution-surface regressions before operators hit them.
- **Streaming PUT checksum coverage matrix doc** — companion
  to the AEAD constraint doc; explains why the v0.9 streaming
  verify tee covers single-PUT cpu-zstd/nvcomp-zstd only (codec
  trait takes `Bytes`, not `Stream<Bytes>`).

Audit posture: per-feature audits (A1 5R + B1 4R + B2 1R + A2-doc
1R + A3-doc 0R + A4 0R) + 4-round integrated audit on the full
v0.9.0..main range. Zero P1 across all rounds. 2 P2 integrated-
audit fixes (Dockerfile `s4 s4 --help` arg dup, back-fill
`:main` mis-tag — both caught BEFORE the corresponding image
actually shipped). v0.10.0 publishes from clean R4.

### Added

- **#A1 SSE-S4 keyring for `s4 repair-sidecar`** — the v0.9 repair
  tool intentionally fell back to `EncryptedSidecarUnsupported` on
  every SSE-S4 chunked (S4E6) object because reconstructing the v3
  `sse_v3` binding (key_id / salt / chunk_size / chunk_count /
  plaintext_len / header_bytes) requires the SSE keyring. v0.10
  plumbs `--sse-s4-key <PATH>` and `--sse-s4-key-rotated id=N,key=PATH`
  (same shape as the server flags) onto the `repair-sidecar`
  subcommand, plus a new `s4_server::repair::repair_sidecar_with_keyring`
  lib entry point. When the body is an S4E6 envelope AND a keyring
  is supplied, the repair path now decrypts the body in-process via
  `decrypt_chunked_buffered`, frame-scans the recovered plaintext,
  and stamps a v3 sidecar so subsequent Range GETs take the
  encryption-aware partial-fetch fast-path. Non-S4E6 envelopes
  (S4E1/E2/E3/E4/E5) and missing-keyring cases keep returning
  `EncryptedSidecarUnsupported`; decrypt failures (key mismatch,
  chunk auth-tag verify) surface the new typed
  `RepairError::SseDecryptFailed` so the CLI can point at
  `--sse-s4-key` instead of bubbling a generic `Backend` error.
  Back-compat: the existing `repair_sidecar` signature is preserved
  as a `None`-keyring shim around `repair_sidecar_with_keyring`;
  `RepairReport` gains a new `sse_v3_binding: Option<RepairSseBinding>`
  field (`Some(..)` only on the SSE-S4 chunked rebuild path). E2E
  coverage adds `repair_sidecar_rebuilds_sse_s4_chunked_object_with_keyring`
  (success path — asserts the rebuilt binding matches the on-disk
  S4E6 header byte-for-byte + the full-body GET round-trips cleanly)
  and `repair_sidecar_wrong_keyring_surfaces_sse_decrypt_failed`
  (wrong-key path — asserts the typed variant + pre-existing sidecar
  state is preserved across the failed repair). Closes the v0.9
  audit-R2 P2-INT-1 follow-up. SSE-S4 buffered (S4E2),
  SSE-C, SSE-KMS, and per-part multipart SSE remain out of scope —
  partial-fetch fundamentally needs a chunked envelope, KMS / SSE-C
  need different key-material plumbing.

- **v0.10 wave-2 #B2 Docker image + Helm chart smoke CI** —
  new per-push `.github/workflows/docker-smoke.yml` (path-
  filtered to `charts/**`, `Dockerfile*`, `docker-compose*.yml`,
  and the docker / docker-smoke workflow files) validates the
  distribution surface added by wave-1 #B1. Three independent
  jobs:

  - `helm-lint-template`: `helm lint` + three `helm template`
    invocations (default values, `image.tag=0.9.0` pinned, and
    the `image.tag=0.9.0-gpu` `-gpu` suffix variant) against
    `./charts/s4` with a placeholder `backend.endpointUrl`.
    Asserts the rendered manifest references the expected ghcr
    repo / tag for each variant.
  - `docker-compose-config`: `docker compose config` on both
    `docker-compose.yml` + `docker-compose.gpu.yml` plus a
    grep for the `ghcr.io/abyo-software/s4` image refs the
    wave-1 #B1 work added (catches a regression that silently
    drops the `image:` line and forces consumers back into
    `build:`-only mode).
  - `image-smoke`: pulls `ghcr.io/abyo-software/s4:latest`
    (overrideable via `workflow_dispatch -f image_tag=...`)
    and runs `s4 --help` + `s4 --version` against it.
    `continue-on-error: true` on the pull tolerates the
    not-yet-published case (= before v0.10.0 cut) by skipping
    the rest of the job — the chart + compose jobs above
    still gate.

  The workflow is NOT wired into `notify-on-failure` (`ci.yml`)
  by design: distribution-surface regressions surface in the
  workflow run UI without auto-filing issues that may be noisy
  during the v0.10 distribution-ramp phase. README §"Kubernetes
  (Helm)" gains a "Verifying the image / chart locally"
  subsection that mirrors the CI checks for operators who want
  to reproduce them pre-deploy.

- **v0.10 #B1 ghcr.io container image publishing** — new
  `.github/workflows/docker.yml` builds and pushes
  `ghcr.io/abyo-software/s4:<version>` (CPU, multi-arch
  `linux/amd64` + `linux/arm64`) and
  `ghcr.io/abyo-software/s4:<version>-gpu` (nvCOMP GPU build,
  `linux/amd64` only — nvCOMP redist only ships an x86_64 tarball)
  on every `v*.*.*` push, plus `workflow_dispatch` for back-filling
  images for tags that pre-date the workflow (e.g. v0.9.0). Images
  carry SLSA build provenance (`provenance: mode=max`) + SPDX SBOM
  and OCI labels (`source`, `description`, `vendor=abyo-software`,
  `licenses=Apache-2.0`). GHA-cache-backed Buildx layer reuse
  (`cache-from/to: type=gha,scope=docker-<flavor>`) keeps
  incremental rebuilds fast. Auth uses the workflow's `GITHUB_TOKEN`
  with `packages: write` — no PAT. The ghcr.io package is public,
  so `helm install` / `docker pull` work with no pull secrets.
  `charts/s4/values.yaml` `image.repository` now defaults to
  `ghcr.io/abyo-software/s4` (was the never-published
  `docker.io/abyosoftware/s4`); `docker-compose.yml` /
  `docker-compose.gpu.yml` gain an `image:` line alongside the
  existing `build:` so `up` works without a local build once the
  release image is cached. README §"Kubernetes (Helm)" + chart
  README rewritten to show the official `helm install --set
  image.tag=0.9.0` invocation (CPU and `-gpu` variants) instead of
  "build it yourself"; legacy-image warning in `NOTES.txt` now
  points at the new default instead of "not yet published". Chart
  `appVersion` bumped `0.3.0` → `0.9.0` so the default
  `image.tag` (which falls back to `.Chart.AppVersion`) resolves to
  an image that actually exists in ghcr.io. CPU `Dockerfile`
  runtime stage now installs `wget` alongside `ca-certificates` —
  the existing `HEALTHCHECK CMD wget …` would otherwise exit 127
  on every probe because `debian:bookworm-slim` ships neither
  `wget` nor `curl` (the GPU `Dockerfile.gpu` already installed it;
  CPU image had been silently unhealthy since v0.8.x but no one
  pulled it because no image had been published — surfaces now
  that publishing is real).

- **v0.10 wave-2 #A4 i686 runtime smoke in CI** — the v0.9 #106-32bit
  work proved `cargo check --target i686-unknown-linux-gnu` passes
  across the workspace, but the README qualified it as
  "compiles, untested at runtime" because no CI step actually
  exercised the i686 binary. v0.10 adds a per-push
  `i686-runtime-smoke` job to `.github/workflows/ci.yml` that
  (1) executes `cargo test --target i686-unknown-linux-gnu -p
  s4-codec -p s4-config --release`, (2) builds `s4-server`
  for i686 (`continue-on-error: true` — the aws-sdk-rust /
  rustls / ring stack may not link cleanly with stock i386
  multilib; a failure here surfaces in the log without going
  red because it doesn't invalidate the codec/config test
  results above), (3) invokes `s4 --help` + `s4 --version`
  against the i686 binary when build succeeded. README §"Supported
  targets" upgraded: `s4-server` cell flips from "⚠️ compiles,
  untested at runtime" to "✅ compiles + `--help` / `--version`
  smoke (CI)". Full end-to-end PUT/GET on 32-bit is still not
  exercised — operators on i686 should treat `--max-body-bytes`
  carefully (auto-clamps to `isize::MAX as usize` ≈ 2 GiB on
  32-bit per the v0.9 #106-32bit fix).

### Documentation

- **v0.10 wave-2 #A3-doc streaming PUT checksum coverage matrix** —
  new [`docs/security/streaming-checksum-coverage.md`](docs/security/streaming-checksum-coverage.md)
  walks the codec-API constraint that limits the v0.9 #streaming-checksum
  tee-into-hasher fast-path to single-PUT `cpu-zstd` / `nvcomp-zstd`
  (= `Codec::supports_streaming_compress() == true`). Same "fundamental
  contract, not deferred plumbing" framing as #A2-doc on the SSE side.
  Captures:
    - Coverage matrix across PUT-shape × codec branch (5 rows × verify
      mode + reason).
    - Three preconditions a "streaming win" actually requires (streaming
      codec + streaming downstream + no full-body framing dependency)
      and why multipart `upload_part` only meets the first two
      (`pad_to_minimum` needs the framed length).
    - Where each path lives in `s4-server` (`put_object` streaming vs
      buffered branch, `upload_part` buffered, `verify_client_body_checksums`
      + `verify_client_trailer_checksums` shared helpers).
    - v0.11+ candidates (`S4F3` streaming frame format, streaming
      `nvcomp-bitcomp` / `gdeflate`, multipart streaming `upload_part`)
      with the upstream API constraints that block each one — tracked
      here, not in the README, to keep the README's "Streaming I/O"
      section focused on what operators get today.
  README §"Streaming I/O" `**Streaming PUT**` bullet gains a link to
  the dedicated doc.

- **v0.10 #A2-doc SSE partial-fetch constraint clarification** —
  documents why the v0.9 #106 encryption-aware Range GET fast-path
  covers **only** SSE-S4 chunked (`S4E6` / `--sse-chunk-size > 0`)
  and not SSE-KMS (`S4E4`) / SSE-C (`S4E3`) / SSE-S4 buffered
  (`S4E2`) / multipart-with-SSE. The other modes wrap the entire
  body under one AES-256-GCM authentication tag, so AEAD decrypt is
  only defined over the full ciphertext + AAD + tag quadruple —
  partial-fetch is **algorithm-level impossible**, not "follow-up
  plumbing". Lifting it requires designing new chunked envelopes
  (provisional `S4E7` for KMS, `S4E8` for SSE-C) plus a part-aligned
  sidecar variant for multipart — v0.11+ roadmap candidates, not
  promised features. New `docs/security/sse-partial-fetch-constraint.md`
  walks through the per-mode constraint + operator guidance on when
  the fast-path matters and how to scope a workload to SSE-S4
  chunked to get it. `docs/security/threat-model.md` §2 row + the
  Known-residual-risks #3 entry now cite the constraint explicitly
  instead of reading as "we'll get to it"; new README §"Server-side
  encryption — Range GET fast-path matrix" surfaces the same matrix
  + recommendation at the top-of-funnel level. Doc-only — zero code
  / config / wire change.

### Fixed

## [0.9.0] — 2026-06-07

First v0.9 cut. Six roadmap items shipped + 7-round integrated
audit converged (clean bill of health on round 7). Net diff
vs v0.8.22: 26 files / +8,500 lines across `s4-codec` and
`s4-server`, all behind opt-in flags or new subcommands — no
behavioral change on existing CLI surface or default-config
deployments.

Headline additions:
- Operator tooling — `s4 verify-sidecar` / `s4 repair-sidecar` /
  `s4 sweep-orphan-sidecars` subcommands close the gap that
  v0.8.x `orphan-sidecar-recovery.md` left as manual aws-cli.
- Performance regression gate — criterion-based bench targets +
  GitHub-Pages-backed trend chart via
  `benchmark-action/github-action-benchmark`.
- Encryption-aware sidecar (SSE-S4 chunked / S4E6) — Range GET
  on `--sse-chunk-size > 0` objects now hits a partial-fetch
  fast path via the new v3 sidecar; SSE-KMS / SSE-C / S4E2
  / multipart remain on the v0.8.12 #120 buffered fallback
  (deferred to v0.10+).
- True streaming PUT checksum verify (tee-into-hasher) for
  `cpu-zstd` / `nvcomp-zstd` single-PUT — closes the v0.8.13
  #127 regression that v0.8.14 #129 reverted to a buffered
  fallback. Multipart `upload_part` keeps the buffered
  per-part verify (bytes are already in memory there for
  framing).
- Chaos infrastructure — 5 deterministic backend-fault
  scenarios replace the v0.8.18 P7 scaffold.
- 32-bit cross-compile (`i686-unknown-linux-gnu`) across
  every workspace crate. Runtime is NOT claimed —
  `cargo check --target` parity only.

Audit posture: zero P1 findings across 6 per-feature audits
(11 Codex rounds) + 7-round integrated audit (7 P2 + 1
self-review fix). v0.9.0 publishes from a clean R7.

### Added

- **#106 32-bit target support** — `s4-codec` / `s4-config` /
  `s4-server` now cross-compile cleanly to
  `i686-unknown-linux-gnu` (32-bit x86 Linux) in addition to the
  existing tier-1 64-bit targets and `wasm32-unknown-unknown`
  (the browser decoder's native target, also 32-bit).

  The blocker was the `usize` 5 GiB AWS-S3-single-PUT ceiling
  baked into `DEFAULT_MAX_BODY_BYTES` (`crates/s4-server/src/sse.rs`
  and `crates/s4-server/src/service.rs`) and the corresponding
  `--max-body-bytes` clap default in `crates/s4-server/src/main.rs`:
  the literal `5 * 1024 * 1024 * 1024` const-overflows `usize` on
  any 32-bit target (`u32::MAX` ≈ 4 GiB < 5 GiB). v0.8.20 R5-8
  attempted to fix this with `(5_u64 * 1024 * 1024 * 1024) as
  usize` and was reverted in v0.8.21 #194 because the cast silently
  truncates to 1 GiB on 32-bit (`0x1_4000_0000 & 0xFFFF_FFFF =
  0x4000_0000`).

  The v0.9 fix splits both constants on
  `target_pointer_width`: 64-bit keeps the bare 5 GiB literal,
  32-bit clamps to `isize::MAX as usize` (≈ 2 GiB on 32-bit —
  Rust caps single-allocation byte counts at `isize::MAX`, not
  `usize::MAX`, so the gateway guard has to match or oversized
  inputs will pass the guard and then OOM-panic inside
  `Vec::with_capacity`). The clap `default_value_t` now references
  the cfg-gated constant so the CLI flag inherits the same
  arm. Two new `target_pointer_width`-gated regression tests pin
  the 64-bit (= 5 GiB) and 32-bit (= `isize::MAX`) values so a
  future refactor can't quietly drop either arm.

  Codex review caught a P2 on the initial cut: the 32-bit arm
  originally used `usize::MAX` (≈ 4 GiB), which would have let
  the SSE buffered-decrypt pre-alloc accept sizes that subsequently
  panic inside `Vec::with_capacity` (the Rust ABI caps any single
  allocation at `isize::MAX` bytes, not `usize::MAX`). Closed by
  switching all three arm definitions + the regression test +
  README §"Supported targets" to `isize::MAX as usize` in round 2.

  The sidecar repair CLI flags (`verify-sidecar
  --max-body-bytes` / `repair-sidecar --max-body-bytes`, added in
  #106's earlier landing) are typed `u64` and already platform-
  independent; they now reference a shared `DEFAULT_REPAIR_BODY_BYTES_CLI`
  constant for discoverability symmetry with the server flag.

  `s4-codec` itself never carried the `usize` cap (it uses
  `MAX_DECOMPRESSED_BYTES: u64` and `usize::try_from`-narrows at
  alloc sites — the v0.8.15 H-b / H-c / v0.8.16 F-3 sweeps closed
  the cast hazards in `multipart::read_frame` / `index::decode_index`
  / `build_index_from_body`), so this change is purely a const-
  expression fix at the server level; the codec already passes
  its full test suite on `i686-unknown-linux-gnu` and on
  `wasm32-unknown-unknown`.

  Runtime support for `i686` `s4-server` is **not** claimed (the
  binary isn't smoked end-to-end on 32-bit; the AWS SDK / rustls /
  tokio dep stack hasn't been audited there). What is claimed is
  that the workspace `cargo check --target` no longer trips on
  the const literals, which unblocks downstream crates that
  depend on `s4-codec` and want to verify their own 32-bit
  builds against the workspace.

  README §"Supported targets" documents the per-crate / per-target
  matrix.

- **#106 sidecar repair / verify / sweep CLI** — three new
  subcommands on the `s4` binary cover the orphan-sidecar /
  stale-sidecar / missing-sidecar operator stories that were
  previously manual-aws-cli recipes:

  - `s4 verify-sidecar <bucket>/<key> --endpoint-url <BACKEND>` —
    read-only check. Reports `Ok` / `LegacyV1` / `Missing` /
    `StaleEtag` / `StaleSize` / `DecodeError`. Exits 0 on
    Ok / LegacyV1, 1 on any divergence so CI / cron jobs can
    branch on "needs action".
  - `s4 repair-sidecar <bucket>/<key> --endpoint-url <BACKEND>
    [--max-body-bytes BYTES]` — rebuild the sidecar by
    re-scanning the main object's frame layout. Overwrites any
    existing sidecar (including stale or corrupt). Body cap
    default 5 GiB matches the server's `--max-body-bytes`.
  - `s4 sweep-orphan-sidecars <bucket> --endpoint-url <BACKEND>
    [--delete]` — walk every `*.s4index` in the bucket and
    report (and optionally delete) sidecars whose paired key
    is missing or whose recorded ETag / size disagrees with
    the live HEAD. Dry-run by default.

  All three point at the **backend** (not the S4 gateway) — the
  gateway hides `.s4index` from listings by design and
  decompresses bodies on GET, both of which would break this
  tooling. Replaces the manual aws-cli recipe in
  `docs/orphan-sidecar-recovery.md` (the manual recipe is kept
  as a "Pre-v0.9 fallback" appendix). README §"Repair tool
  status" updated to drop the "v0.9 roadmap" qualifier.

  Library API: `s4_server::repair::{verify_sidecar,
  repair_sidecar, sweep_orphan_sidecars}` for programmatic use
  alongside the CLI.

  Two review rounds caught four correctness / safety issues that
  landed in the initial cut.

  Self-review:

  - **TOCTOU race in `repair-sidecar`** — HEAD → GET could
    straddle an overwrite, stamping the sidecar with E1 while
    the body is actually at E2 (silently produces an
    immediately-stale sidecar). Fixed by adding `If-Match` to
    the GET, with a typed `OverwrittenDuringRepair` error so
    the operator re-runs instead of trusting the write.
  - **`SidecarUndecodable` auto-delete data-loss** — a
    legitimate user-PUT object whose key happens to end in
    `.s4index` (the v0.8.17 `--allow-legacy-reserved-key-reads`
    migration scenario) would also fail to decode and would be
    silently deleted by `--delete`. Split the deletion policy
    into `DryRun` / `PairBoundOnly` / `IncludeUndecodable`; CLI
    `--delete` is the safe pair-bound subset, and operators
    must opt in with `--delete-undecodable` to escalate.

  Defensive: `HEAD` with no `Content-Length` now returns
  `MissingContentLength` instead of treating absence as zero
  (would have silently bypassed `--max-body-bytes`).

  Codex review (P1):

  - **P1-A: `PairedMissing` data-loss for legacy reserved-name
    user data** — the self-review fix above only protected
    `SidecarUndecodable`, but the classifier checked the paired
    HEAD before reading the candidate. A legacy user object at
    `legacy.s4index` whose stripped pair `legacy` doesn't exist
    would be classified as `PairedMissing` and the default
    `--delete` would silently destroy it. Restructured
    `classify_one` to **decode first**: bytes that don't parse
    as S4IX magic are always classified as `SidecarUndecodable`
    regardless of pair status, so the safe `PairBoundOnly`
    policy can never delete non-sidecar bytes. New E2E asserts
    the worst case (legacy data with no pair) classifies
    correctly.
  - **P1-B: `If-Match` must send the quoted entity-tag form
    (RFC 7232)** — `head_main` was passing the normalized
    (unquoted) ETag to the conditional GET; strict
    S3-compatible backends reject `If-Match: abc-2` with 412
    and repair never succeeds. Split `head_main` to return
    both `raw_etag` (for `If-Match` headers) and
    `normalized_etag` (for stamping into
    `FrameIndex::source_etag`, matching the server-side
    `s3s::ETag::value()` stripped form).

  Codex review round 2 (P2):

  - **P2-A: `s4 verify-sidecar bucket/key --endpoint-url ...`
    fails to parse** — `--endpoint-url` was defined only on
    the root `Opt`, so clap rejected it when supplied after a
    subcommand even though every doc example uses that form.
    Marked the flag `global = true` so post-subcommand
    placement parses (matches docs without breaking the
    existing pre-subcommand server-mode usage).
  - **P2-B: `If-Match` doesn't cover the post-GET / pre-PUT
    window** — the GET conditional only proves the body
    matched at GET time; the main object can still be
    overwritten during `build_index_from_body` or the sidecar
    PUT itself, leaving a freshly-written sidecar stamped with
    the OLD ETag (server's `sidecar_version_binding_ok` would
    silently reject it on every subsequent Range GET). Added a
    final HEAD after the sidecar PUT; if the live ETag / size
    diverges from the initial HEAD, delete the just-written
    sidecar and surface `OverwrittenDuringRepair` so the
    operator re-runs under quieter conditions.

  Codex review round 3 (P2):

  - **P2-C: `verify-sidecar` false-alerts on healthy small
    objects** — the server intentionally skips sidecar
    emission for `entries.len() <= 1` (single-PUTs / single-
    chunk multiparts have no Range GET fast-path to lose), so
    a missing `.s4index` on a small object is normal. The
    initial `Missing` verdict + exit 1 broke the documented
    CI / cron use case for those objects. Split `Missing`
    into three variants resolved by a body scan:
    `MissingHarmless` (1 frame, no-op, exit 0),
    `MissingDivergent` (2+ frames, real bug, exit 1), and
    `MissingUnknown` (body > `--max-body-bytes`, ambiguous,
    exit 0 with hint). The CLI's `--max-body-bytes` flag now
    also controls the verify-time deep-scan cap so operators
    can tune one knob.

  Codex review round 4 (P2):

  - **P2-D: rebuilt sidecar is always stale on ETag-less
    backends** — `head_main` was coercing a missing `ETag`
    response header to `""`, so `repair-sidecar` would stamp
    `idx.source_etag = Some("")`. The server-side
    `sidecar_version_binding_ok` treats `None` as the
    back-compat "no binding" path (best-effort, Range GET
    fast-path engages); `Some("")` falls into the strict
    compare branch and always trips stale, leaving the
    sidecar useless. Refactored `HeadInfo` to
    `Option<String>` for both `raw_etag` and `normalized_etag`,
    skip `If-Match` when the backend has no ETag, stamp
    `source_etag = None` in that case, and made the verify /
    sweep classifiers compare `Option<&str>` so `None == None`
    holds. Matches the server PUT path's
    `resp.e_tag.as_ref().map(...)` shape exactly.

  Codex review round 5 (P3):

  - **P3-A: repaired ETag-less v2 sidecar misclassified as
    `LegacyV1`** — the verify match required `(Some, Some)`
    for `Ok`, so `(None, Some(size))` (the shape `repair-
    sidecar` writes on an ETag-less backend, P2-D fallout)
    fell through to the `LegacyV1` wildcard arm. CLI then
    told the operator the sidecar was "legacy v1, run
    repair-sidecar to upgrade" — except repair already wrote
    the highest-binding-level sidecar the backend supports.
    Loosened the `Ok` arm to `(_, Some(_))` (any present size
    binding = v2 enough) and narrowed `LegacyV1` to `(_, None)`
    (no size binding at all = true legacy). Also reworded
    the CLI "OK" message to drop the "ETag + size" claim
    (would mis-advertise on ETag-less backends).

  Coverage: 11 lib unit tests (parsing, ETag quote
  normalization, `Option<&str>` etag-equality semantics for
  the P2-D None-vs-Some-empty guard, P3-A status truth table
  pinning the `(_, Some(_)) → Ok` arm so a regression here
  can't quietly mis-route ETag-less v2 sidecars back to
  `LegacyV1`, `DeletePolicy::allows` truth table, expanded
  status truth table including `MissingHarmless` /
  `MissingDivergent` / `MissingUnknown`, body-cap constant
  guard) + 7 MinIO E2E tests (`tests/sidecar_repair_via_minio.rs`)
  covering verify-clean, repair-after-backend-delete,
  repair-after-clobber, sweep-finds-and-deletes-orphan, the
  P1-A regression (legacy reserved-name user data with no
  pair classifies as `SidecarUndecodable`, not
  `PairedMissing`), the P2-B post-GET race detector (spawns a
  parallel overwrite to drive `OverwrittenDuringRepair` and
  confirms the stale sidecar is cleaned up), and the P2-C
  small-object verdict (PUT a 512 B single-frame object,
  verify it reports `MissingHarmless` exit 0, plus a `cap=0`
  edge case asserting `MissingUnknown`).

- **v0.9 criterion regression-tracking benches** — new
  `crates/s4-codec/benches/` directory carries three criterion
  suites driven by a `.github/workflows/bench.yml` push-to-main
  job. Results are published per-commit to the `gh-pages`
  branch via
  [`benchmark-action/github-action-benchmark`](https://github.com/benchmark-action/github-action-benchmark);
  any tracked target that goes ≥ 110% of its previous best
  drops a comment on the offending commit. The trend chart
  surfaces at `https://abyo-software.github.io/s4/dev/bench/`
  once the first successful main run initialises the
  `gh-pages` branch.

  Suites:

  - `codec_roundtrip` — `cpu-zstd` (levels 1 / 3 / 22) /
    `cpu-gzip` / `passthrough` compress + decompress at
    1 KiB / 1 MiB / 16 MiB inputs.
  - `frame_codec` — `multipart::write_frame` and `FrameIter`
    with the `S4P1` padding-skip branch exercised across
    16 × 64 KiB and 256 × 4 KiB frame shapes.
  - `index_codec` — `index::encode_index` / `decode_index`
    plus `FrameIndex::lookup_range` at 128 / 1024 / 4096
    sidecar entry counts.

  GPU codecs (`nvcomp-*`, `dietgpu-*`) are deliberately not
  in the suite because GitHub-hosted runners have no
  CUDA-capable GPU; the manual `examples/bench_codecs.rs`
  table in README §Benchmarks remains canonical for those
  numbers. README §"Performance regression tracking
  (criterion + GitHub Pages)" describes the workflow.

- **#106 chaos / fault-injection infrastructure** —
  `crates/s4-server/tests/chaos.rs` graduates from the v0.8.18
  P7 scaffold (`chaos_scaffold_smoke`, 45 LOC) into a full
  fault-injection harness with a reusable `ChaosBackend` mock
  (`ChaosHandle` + `ChaosConfig`) and five concrete scenarios
  that pin gateway invariants which would otherwise only
  surface in production under hostile backend conditions:

  - **Scenario 1** (`chaos_get_5xx_mid_stream`): a backend
    GET whose body stream errors after the first chunk must
    surface to the client as a 5xx, never as a truncated
    200. A regression here would silently hand clients half
    a file labelled "complete".
  - **Scenario 2** (`chaos_head_latency_timeout_fails_close`):
    a backend HEAD that blocks indefinitely must not be able
    to pin a gateway handler — `tokio::time::timeout` must
    fire and cancellation must propagate through the await
    chain. Validates the fail-close discipline the production
    AWS-SDK read-timeout knob relies on.
  - **Scenario 3** (`chaos_concurrent_put_same_key_no_mix`):
    two PUTs at the same key sequenced through a
    `tokio::sync::Notify` (deterministic, NOT random) leave
    the backend with one of the two bodies byte-for-byte,
    never a spliced mix; the second PUT wins under serial
    ordering.
  - **Scenario 4** (`chaos_keyring_rotation_mid_put`): an
    SSE-S4 keyring rotated between two PUTs (active=1 →
    active=2, id=1 retained for read) must let both objects
    round-trip — the per-object S4E2 key_id embed picks the
    right keyring entry on read-back.
  - **Scenario 5** (`chaos_complete_mpu_fails_state_unchanged`):
    a backend CompleteMultipartUpload that returns 500 must
    leave the gateway-side multipart state intact (retry-able)
    and must not materialise a partial object at the
    destination key.

  All faults are armed deterministically (ordinal counters
  + `Notify` hand-off, no wall-clock racing or random
  scheduling), so the suite reproduces under
  `--test-threads=1` and `cargo nextest`. Confirmed flake-
  free across 5 consecutive `cargo test -p s4-server
  --test chaos -- --test-threads=1` runs. No new
  `dev-dependencies` were added — the mock layer reuses the
  same `s3s` / `async-trait` / `bytes` / `futures` /
  `tokio` versions the existing `tests/multipart_audit_71.rs`
  fixture already brings in. Production code (`crates/s4-server/src/`)
  is unchanged; the harness lives entirely in test scope.

- **#106 streaming PUT checksum verify (tee-into-hasher)** —
  closes the long-standing fail-open on the streaming-framed
  PUT path documented in `docs/security/threat-model.md §
  Limitations [2]`. The v0.8.13 #127 attempt to "force
  buffered when a checksum is supplied" regressed sidecar
  correctness for AWS-SDK PUTs (which auto-attach
  `x-amz-checksum-crc32`) and had to be reverted in v0.8.14
  #129; until #106 the streaming-framed path silently
  passed client-supplied whole-body checksums through
  without verifying them.

  New module `crates/s4-server/src/streaming_checksum.rs`:

  - `ClientChecksums::from_request_fields(...)` parses the
    six AWS-spec headers (`Content-MD5`,
    `x-amz-checksum-{crc32, crc32c, sha1, sha256, crc64nvme}`)
    pre-stream and rejects malformed values with
    `InvalidDigest` before any body bytes flow.
  - `tee_into_hashers(blob, claims)` wraps the inbound
    `StreamingBlob` in a `TeeStream` that feeds each chunk
    into every active hasher (only hashers whose claim is
    set incur per-byte cost). On EOF the wrapper finalises
    and compares; mismatch synthesises a typed
    `io::Error(InvalidData, StreamingChecksumError)`.
  - `extract_streaming_checksum_error(...)` walks the
    `io::Error` source chain so the PUT handler can recover
    the failed algorithm name across the
    `blob_to_async_read` → `StreamReader` → `CodecError::Io`
    wrap layers.

  Wired into `service.rs::put_object`'s streaming-framed
  branch (single-PUT cpu-zstd / nvcomp-zstd). Bodies that
  fail verify surface as `400 BadDigest` with the same
  message shape the buffered path uses. Sidecar emission is
  unaffected — the tee sits **upstream** of
  `streaming_compress_to_frames`, so a verify failure short-
  circuits the call before any backend write or sidecar
  build. Non-checksummed PUTs skip the wrapper entirely
  (`ClientChecksums::any() == false` AND no
  `x-amz-trailer` checksum) so the pre-#106 throughput is
  preserved.

  **Trailer-deferred verify** for the chunked /
  SigV4-streaming SDK case: AWS SDKs that send the body
  via `aws-chunked` encoding attach the checksum value in
  the request **trailers** (post-body) rather than as a
  request header, and announce which algorithm will follow
  via `x-amz-trailer`. The PUT handler reads
  `x-amz-trailer` at body-start (`WhichHashers::
  from_trailer_header`) to spin up the matching hasher,
  then — after `streaming_compress_to_frames` returns —
  reads the actual value out of `req.trailing_headers` and
  compares via `ComputedDigests::compare_b64`. A trailer
  claim against an algorithm the tee did NOT hash returns
  `BadDigest` (we cannot verify what the client promised,
  so we must refuse the PUT). Without this branch a bad
  trailer checksum on the streaming-framed path would
  silently pass — the same fail-open #106 is closing,
  just via the other delivery mechanism.

  `blob_to_async_read` (`streaming.rs`) was updated to
  preserve typed error sources via `io::Error::other(e)`
  instead of `io::Error::other(e.to_string())` — the
  string-cast dropped the `Box<dyn Error>` chain and made
  downcast-recovery impossible. The
  `extract_streaming_checksum_error` helper handles both
  the direct and the one-deep-via-StreamReader chain shapes.

  Multipart `UploadPart` keeps the buffered per-part verify
  it already had (#122 / #128 still active there — the part
  body is already in memory for framing / padding so
  streaming verify wouldn't save anything). GPU codecs that
  fall into the bytes-buffered single-PUT branch
  (`!supports_streaming_compress(kind)`) continue to use
  `verify_client_body_checksums` directly. Encrypted-object
  PUTs are unaffected — the tee runs on plaintext upstream
  of the encrypt step.

- **#106 encryption-aware sidecar (SSE-S4 chunked Range
  GET fast-path)** — closes the v0.8.12 #120 buffered-
  fallback regression on SSE-encrypted Range GETs for the
  scope-in `--sse-chunk-size > 0` (= S4E6 chunked frame)
  case. Pre-#106, a Range GET on a 5 GiB SSE-S4 object
  would fetch the full encrypted body from backend, decrypt
  the entire thing, frame-parse, and slice — turning a
  100 B Range request into a 5 GiB transfer. With #106 the
  GET path partial-fetches just the enclosing S4E6
  chunk(s), decrypts them independently, and slices.

  New `index.rs` v3 sidecar format. v3 extends v2 with a
  fixed 30-byte SSE chunk-geometry block appended after
  the etag payload (before the entries table): `enc_chunk_size`,
  `enc_chunk_count`, `enc_key_id`, `enc_salt` (8 B for
  S4E6), `enc_plaintext_len`, `enc_header_bytes`. The
  writer emits v3 only when an SSE binding is attached
  (`FrameIndex::sse_v3 = Some(..)`) — non-SSE PUTs keep
  emitting v2 bit-for-bit. The on-wire `version` field
  dispatches in `decode_index` so v0.8.x readers
  encountering a v3 sidecar surface `UnsupportedVersion(3)`
  → the GET path treats the sidecar as missing and falls
  back to the existing buffered full-GET (forward-safe
  degradation, not corruption).

  New `FrameIndex::encrypted_lookup(&RangePlan)` returns
  `EncryptedRangePlan { chunk_idx_start, chunk_idx_last_inclusive,
  enc_byte_start, enc_byte_end_exclusive,
  pre_encrypt_slice_start_in_concat,
  pre_encrypt_slice_end_in_concat }` — handles non-final-
  chunk stride (= `chunk_size + 16`-byte tag) vs final-
  chunk residual sizing (= `plaintext_len - prior * chunk_size + 16`).
  New `sse::decrypt_s4e6_chunk_range(...)` decrypts a
  contiguous chunk range out of a partial-fetched body
  slice — fail-closed length check (`expected_len ==
  body_slice.len()`) refuses forged / truncated bodies
  before any AES-GCM verify runs.

  PUT path (`service.rs` `put_object`) — the
  `sidecar_index` build condition extends to admit the
  SSE-S4 chunked branch (`sse_s4_chunked_path = sse_c_material.is_none()
  && kms_key_id.is_none() && self.sse_keyring.is_some()
  && self.sse_chunk_size > 0`); after the S4E6 encrypt
  runs, `parse_s4e6_header` reads back the per-PUT salt /
  key_id / chunk_count out of the encrypted body's fixed
  header and stamps the binding onto `idx.sse_v3` before
  `write_sidecar`. Any parse failure leaves
  `sse_binding = None` (= sidecar stays at v2 layout, GET
  falls back to buffered — degradation, not data loss).

  GET path (`service.rs` `get_object`) — the existing
  sidecar fast-path branch now checks for `idx.sse_v3.is_some()`
  and dispatches into the new `partial_range_get_encrypted`
  which: (1) maps the `RangePlan` to an `EncryptedRangePlan`,
  (2) issues a backend `Range: bytes=enc_byte_start-...`
  GET, (3) calls `decrypt_s4e6_chunk_range`, (4) slices
  the decrypted concatenation down to the pre-encrypt
  range, (5) frame-parses + decompresses + final-slice via
  the existing pre-encrypt machinery. Bandwidth saved is
  visible on `s4_request_latency_seconds{op="get_object",
  codec="sse-s4-chunked-partial"}` + the `bytes_in` field
  in the access log (= partial enc range, not full body).

  Scope-out (still buffered fallback, no v3 sidecar
  emitted): SSE-KMS (separate DEK envelope shape), SSE-C
  (per-request customer-key plumbing), SSE-S4 buffered
  (`--sse-chunk-size 0`, S4E2 frame), multipart (per-part
  SSE applied at Complete time — separate design).
  Multi-mode plumbing is the v0.10+ roadmap.

  Back-compat strategy: v1 / v2 sidecar decode is
  bit-for-bit unchanged (the version dispatcher in
  `decode_index` keeps v1 / v2 / v3 arms separate;
  `encode_index` only bumps to v3 when an SSE binding is
  attached). Four new tests in
  `crates/s4-server/tests/roundtrip.rs::sse_s4_chunked_range_get`:
  `sse_s4_chunked_range_get_uses_v3_sidecar_partial_fetch`
  (asserts the v3 sidecar is written + the binding is
  populated + Range GET returns the right bytes),
  `sse_s4_chunked_range_get_round_trip_correctness` (three
  ranges: in-chunk, crossing an S4E6 chunk boundary,
  suffix range), `sse_s4_buffered_range_get_still_falls_back`
  (regression fence: SSE-S4 `--sse-chunk-size 0` keeps
  emitting S4E2 + no sidecar), `non_sse_put_still_emits_v2_sidecar`
  (regression fence: plain CpuZstd PUT keeps emitting v2).
  Five new tests in `crates/s4-codec/src/index.rs::tests`
  cover the v3 encode/decode round-trip, v2 → v3-reader
  back-compat, the v3 forward-safe (`enc_chunk_size = 0`)
  shape, and the `encrypted_lookup` math (single chunk,
  crossing chunks, final-chunk residual). The
  `MemoryBackend` mock in `tests/roundtrip.rs` was
  extended to honour `Range: bytes=N-M` (pre-#106 it
  returned the full body and downstream tolerated trailing
  junk — fine for the unencrypted partial path because
  `FrameIter` stops at the requested frame's end, but the
  encrypted chunk-walk needs the slice length to match
  exactly or the AES-GCM tag walk mis-aligns).

  Touched files: `crates/s4-codec/src/index.rs` (v3 format
  + `SseChunkBinding` + `EncryptedRangePlan` +
  `encrypted_lookup` + v3 round-trip tests),
  `crates/s4-server/src/sse.rs` (new
  `decrypt_s4e6_chunk_range` helper),
  `crates/s4-server/src/service.rs` (sidecar build
  condition + post-encrypt binding stamp + new
  `partial_range_get_encrypted` method + GET dispatch),
  `crates/s4-server/tests/roundtrip.rs` (4 new tests +
  Range-honouring `MemoryBackend`),
  `crates/s4-codec/tests/{fuzz_bolero,fuzz_parsers,fuzz_advanced}.rs`
  + `crates/s4-codec/benches/index_codec.rs` (`sse_v3:
  None` in `FrameIndex` literal sites — fuzz / bench
  fixtures stay on v2 layout),
  `docs/security/threat-model.md` + `README.md` (status
  updated, follow-up roadmap clarified).

  Six new E2E tests in
  `crates/s4-server/tests/roundtrip.rs::streaming_checksum_e2e`
  pin the behaviour:
  `streaming_checksum_crc32c_match_succeeds`,
  `streaming_checksum_crc32c_mismatch_rejects` (asserts the
  failure surface is `BadDigest`, not `InternalError`),
  `streaming_checksum_sha256_match_succeeds`,
  `streaming_checksum_multiple_algorithms_match_all_required`
  (both correct → pass; one wrong → reject),
  `streaming_checksum_5gib_class_no_memory_blowup` (8 MiB
  CI-budget stand-in for the 5 GiB class), and
  `streaming_checksum_keeps_framed_path_for_sdk_default`
  (regression fence for v0.8.13 #127 → v0.8.14 #129:
  AWS-SDK-style `x-amz-checksum-crc32` PUT must still
  produce an `S4F2` framed body, not raw zstd). Plus 10
  unit tests in the new module covering header parsing,
  multi-algorithm tee, mismatch / mid-stream error paths,
  and the CRC-64/NVME accumulator cross-check against the
  buffered helper. All ignored MinIO E2E tests for the
  touched code paths (`minio_roundtrip_through_s4_with_cpu_zstd`,
  `range_get_falls_back_to_full_when_sidecar_etag_stale`,
  `upload_part_copy_propagates_source_version_id`,
  `streaming_compress_truncated_input_returns_400`)
  continue to pass.

### Fixed

- **#106-audit-R6 P2-R6** — the R5 P2-R5 bounded sidecar fetch
  HEADed first and GETed second, leaving a TOCTOU window where a
  sidecar swap between the two could bypass the cap (race
  HEAD-tiny → swap-massive → GET would still let `collect()`
  pull the full new body into memory). Closed by pinning the GET
  to the HEAD ETag via `If-Match` so the swap surfaces as 412
  PreconditionFailed before any bytes are read, plus a
  defense-in-depth post-GET length check that catches races on
  ETag-less / If-Match-non-honouring backends. Race → typed
  `SidecarFetchOutcome::Other` with a re-run hint; post-GET
  length overrun → `SidecarTooLarge` (same surface as the
  HEAD-time rejection so callers can branch uniformly).

- **#106-audit-R5 P2-R5** — `s4 verify-sidecar` /
  `sweep-orphan-sidecars` used to do an unbounded GET of every
  `<key>.s4index` body before `decode_index` could reject it. A
  multi-GiB corrupt sidecar or legacy reserved-name user object
  (the `--allow-legacy-reserved-key-reads` migration scenario)
  could OOM the operator's repair process — same DoS shape the
  codec already defends against on the server side via
  `MAX_FRAMES` / `MAX_ETAG_BYTES`. Closed by a new bounded
  `get_sidecar_bytes_capped` helper that HEADs first and
  refuses to GET if Content-Length exceeds
  `MAX_SIDECAR_BODY_BYTES = 600 MiB` (comfortably above the
  codec spec's max legitimate sidecar of ~512 MiB, well below
  attacker payload sizes). `verify-sidecar` surfaces a new
  typed `RepairError::SidecarTooLarge { bucket, key, size,
  cap }`; sweep surfaces oversized entries as
  `SidecarUndecodable` with a size-explaining message (so one
  bad sidecar doesn't abort the whole sweep). Two new lib
  unit tests (`sidecar_too_large_error_shape`,
  `max_sidecar_body_bytes_cap_value_pinned`) pin both the
  variant Display and the cap value relative to the codec
  spec ceiling computed from `MAX_FRAMES * ENTRY_BYTES +
  HEADER_FIXED_V2 + MAX_ETAG_BYTES`. New MinIO E2E
  `sweep_classifies_oversized_lookalike_sidecar_as_undecodable`
  walks the sweep path with a 1 MiB lookalike (the full-size
  600 MiB+ exercise would be too slow for CI; the cap value
  itself is pinned by the lib unit test).

- **#106-audit-R4 P2-R4** — `s4 verify-sidecar` on a passthrough /
  raw-bytes object (no `S4F2` magic, body ≥ 28 bytes so the inner
  frame parser reaches `BadMagic`) used to exit 1 with a confusing
  `FrameScan` error. The server never sidecars those objects by
  design, so absence of a sidecar is the correct steady state —
  CI / cron jobs would false-alert on healthy passthrough
  workloads. Closed in `classify_missing_sidecar` by catching the
  `FrameError::BadMagic` variant from `build_index_from_body` and
  surfacing `MissingHarmless { frame_count: 0 }` (exit 0) instead.
  Twin of R3 P2-R3 on the verify-side. New MinIO E2E
  `verify_sidecar_reports_missing_harmless_for_non_framed_body`
  plants raw bytes directly via the backend (long enough to clear
  the 28-byte FRAME_HEADER_BYTES probe) and proves the verdict.
  Non-`BadMagic` `FrameScan` errors still propagate so genuine
  corruption surfaces loud.

- **#106-audit-R3 P2-R3** — `s4 repair-sidecar` against a
  passthrough / raw-bytes object (no `S4F2` frame magic in the
  body) used to silently write an empty `<key>.s4index`
  sidecar — `build_index_from_body` returns `Ok` with an empty
  entries vec rather than an error for non-framed bodies, and
  the repair tool encoded that anyway. The resulting sidecar
  broke Range GET on the same key (`FrameIndex::lookup_range`
  over zero entries returns `None`, and the GET path then took
  the "no plan" branch instead of the passthrough-range
  fallback that exists for sidecar-less objects). Closed by
  adding an `idx.entries.is_empty()` guard before the
  `encode_index` call; rejects with a new typed
  `RepairError::NotFramed { bucket, key }` whose Display tells
  the operator the object isn't a sidecar-repair candidate
  (`verify-sidecar` separately classifies it as
  `MissingHarmless` with `frame_count = 0`, which is correct).
  New lib unit test `not_framed_error_shape` pins the
  variant's wire shape + Display; new MinIO E2E
  `repair_sidecar_rejects_zero_frame_body` plants an empty
  body (the exact case `build_index_from_body` returns
  `Ok` with zero entries) AND a non-trivial raw-bytes body
  (which trips the inner BadMagic / `FrameScan` path), and
  proves BOTH paths reject cleanly without writing a sidecar.

- **#106-audit-R2 P2-INT-1** — `s4 repair-sidecar` on an SSE-S4
  encrypted object (S4E1..S4E6 envelope, written by a gateway
  configured with `--sse-s4-key` and `--sse-chunk-size > 0`) used
  to feed the ciphertext to `build_index_from_body`, surface a
  confusing `FrameScan` error, and leave the operator without a
  recovery path. The repair binary runs against the BACKEND (not
  the gateway), so the body it sees is the post-encrypt envelope;
  it has no access to the SSE keyring needed to decrypt + walk
  the chunk layout for a v3 sidecar's `sse_v3` binding. Closed
  by detecting the S4Ex magic prefix before frame scanning and
  surfacing a typed `RepairError::EncryptedSidecarUnsupported
  { bucket, key, message }` whose `Display` directs the operator
  to a server-mode rebuild path (re-PUT the object) until v0.10
  plumbs `--sse-s4-key <path>` through the CLI. New unit tests
  (`detect_sse_magic_covers_all_envelope_variants`,
  `repair_sidecar_rejects_encrypted_body_with_typed_error`) pin
  the magic table and the Display text; new MinIO E2E test
  `repair_sidecar_rejects_sse_s4_chunked_object_cleanly` proves
  the rejection on a real S4E6 object and asserts the pre-
  existing sidecar state is byte-equal after the failed repair.

- **#106-audit-R2 P2-INT-2** — `verify_client_body_checksums` on
  the buffered PUT branch (passthrough codec / non-streaming-
  framed dispatch) verified the six header-supplied AWS checksum
  algorithms but silently dropped `x-amz-trailer`-announced
  SigV4-streaming trailer checksums. A client could declare
  `x-amz-trailer: x-amz-checksum-crc32c` and then omit the
  trailer value to bypass verification on any PUT routed through
  a GPU codec or passthrough. Closed by routing the buffered
  branch through a new shared `verify_client_trailer_checksums`
  helper (also adopted by the streaming-framed branch via an
  in-line refactor) that re-uses `ComputedDigests::compare_b64`
  and the `WhichHashers::from_trailer_header` parser. The helper
  fails closed when announced trailers are missing the value /
  block / handle. New buffered-path roundtrip tests
  (`buffered_path_trailer_checksum_announced_without_handle_rejected`,
  `buffered_path_trailer_only_signature_does_not_reject`) and 5
  unit tests on the helper (`verify_client_trailer_checksums_*`)
  pin the announce-parsing, case-insensitive filter, fail-closed
  paths, and the legitimate non-checksum-trailer pass-through.

### Notes

- **v0.10 roadmap — encrypted sidecar repair**: the P2-INT-1
  fix above intentionally rejects rather than supporting
  encrypted-body repair. The full fix requires plumbing the
  SSE keyring into the standalone sidecar binary so it can
  decrypt the S4E1..S4E6 envelope and walk the chunk layout
  to rebuild the v3 sidecar's `sse_v3` binding. Tracked as a
  v0.10 follow-up: add `--sse-s4-key <path>` to `s4
  repair-sidecar` (and the matching verify / sweep
  subcommands), mirroring the server CLI's existing flag.

## [0.8.22] — 2026-06-07

Seventh-round review caught that R6-6 introduced a fresh
fabrication (the SIGUSR1 grep target didn't match the real log
line) plus 2 stale stamps the audit cycles had left behind.

### Fixed

- **#200 R7-1** — Runbook §1 SIGUSR1 grep target corrected to
  `"S4 SIGUSR1: dumped attached-manager snapshots"` (the real
  substring in `main.rs:1830`). R6-6 used
  `"SIGUSR1: dumped all state snapshots"`, which never matches
  — the recipe would hang on `grep -m1` until the operator
  gave up and fell back to the `sleep 5` floor.
- **#201 R7-2** — README §roadmap "v0.8.8 released
  (2026-05-20)" bullet replaced with a moving-target
  reference to CHANGELOG + GitHub Releases. The pinned bullet
  was 13 patches stale (we are on v0.8.22) and would have
  drifted again on the next cut.
- **#202 R7-3** — Threat-model and runbook "Last reviewed"
  stamps now both read `v0.8.22` and carry a one-line
  **Stamp policy** note declaring that future cuts bump both
  stamps in lockstep. v0.8.20 had bumped threat-model only
  (R5-6); v0.8.21 had bumped runbook only (R6-3); the
  divergence was itself a finding in R7.

### Tests

- 449 lib + 45 integration + 11 SigV4 vectors + 2 bolero + 1
  chaos unchanged; clippy + fmt clean.

### Notes

- Round 7 surfaced **1 HIGH + 2 LOW** vs Round 6's **1 HIGH +
  4 MED + 1 LOW**. The findings curve is genuinely converging.
  Round 8 expected to hit 0 or 1; if it finds more, the cause
  is almost certainly a class of bug we haven't been grepping
  for yet.

## [0.8.21] — 2026-06-07

Sixth-round review caught that v0.8.20 R5-8 walked the
--max-body-bytes fix in the wrong direction (silent truncation
on 32-bit instead of the original loud compile error), plus 5
other doc / cosmetic items. v0.8.21 reverts R5-8 and sweeps the
rest.

### Fixed

- **#194 R6-1** — `--max-body-bytes` default reverted to the
  bare `5 * 1024 * 1024 * 1024` literal. The v0.8.20 R5-8
  `(5_u64 * 1024 * 1024 * 1024) as usize` would have silently
  truncated to 1 GiB on a 32-bit target instead of the
  original loud compile-time const-overflow. Loud failure is
  the correct mode: s4-server only ships 64-bit Linux per
  README §Supported targets, and a future 32-bit port needs
  to think about the cap explicitly rather than rely on a
  cast. CHANGELOG #193's "stays honest on cross-compile"
  claim was wrong.
- **#195 R6-2** — Runbook "Metric-naming note" at the bottom
  of the metric reference still referenced
  `s4_requests_total{status=~"5..."}` (a label that doesn't
  exist on the metric). Corrected to
  `s4_requests_total{result="err"}` — `result` is the actual
  label, values `"ok"` / `"err"`. R5-2 swept §1/§2/§3/§7/§8
  but missed this trailing paragraph.
- **#196 R6-3** — Runbook "Last reviewed" stamp advanced
  v0.8.18 → v0.8.21 (R5-1 / R5-2 / etc. edited §1/§2/§3/§7/§8
  in v0.8.20 but didn't bump the stamp; the threat-model
  companion was bumped in R5-6).
- **#197 R6-4** — AWS SigV4 vectors provenance reverted
  R5-7's `get-utf8-path` rename back to `get-utf8`. The AWS
  upstream suite vector is called `get-utf8`; the `_path`
  suffix is our private fn-name convention. R5-7 broke the
  module docstring's "vector names match the public AWS
  reference suite" claim.
- **#198 R6-5** — `docs/orphan-sidecar-recovery.md` future-
  release note advanced from "post-v1.0" (R5-5) to "v0.9
  roadmap, paired with #106 `s4-tool repair-sidecar` /
  `s4-tool verify`". The two adjacent sidecar-maintenance
  promises now have consistent target versions.
- **#199 R6-6** — Runbook §1 SIGUSR1-before-restart recipe
  no longer relies on a fixed `sleep 1` (which can race
  against a multi-second JSON dump on a large state file).
  Recommends tailing `journalctl ... | grep -m1 "SIGUSR1:
  dumped all state snapshots"` instead; `sleep 5` is the
  fallback floor for non-interactive cases.

### Tests

- 449 lib + 45 integration + 11 SigV4 vectors + 2 bolero + 1
  chaos unchanged; clippy + fmt clean.

### Notes

- The v0.8.20 → v0.8.21 sequence (and the v0.8.19 D-6 →
  v0.8.20 R5-2 sequence before it) keeps illustrating the
  same pattern: every doc fix needs to grep the rest of the
  tree for the same fabrication. v0.8.21 is also the
  sixth-round closeout; if round 7 finds more it's almost
  certainly a fabrication R6 missed.

## [0.8.20] — 2026-06-07

Fifth-round review caught that v0.8.19 D-6 only fixed runbook §12;
the same metric-fabrications survived in **§1 / §2 / §3 / §7 / §8**
plus README + SOCIAL_POSTS. v0.8.20 sweeps every remaining
fabrication + tightens a 32-bit overflow corner.

### Fixed

- **#186 R5-1** — Runbook §1 "graceful shutdown also dumps state
  files" claim removed. Only SIGUSR1 dumps manager state;
  shutdown only drains the access-log buffer. Adds the
  `kill -USR1 ... && systemctl restart` pre-restart recipe so
  operators don't lose state since the last dump.
- **#187 R5-2** — Runbook metric names across §2 / §3 / §7 / §8
  (D-6 only covered §12's dedicated table). Fabricated names
  removed: `s4_gpu_compress_oom_total` (real:
  `s4_gpu_oom_total`), `s4_backend_error_total` (real:
  `s4_requests_total{result="err"}`),
  `s4_replication_pending_total`,
  `s4_replication_completed_total`,
  `s4_replication_failed_total` (real:
  `s4_replication_dropped_total`,
  `s4_replication_replicated_total`,
  `s4_replication_status_swept_total`),
  `s4_tls_cert_reload_failed_total` (real:
  `s4_tls_cert_reload_total{result="err"}`).
- **#188 R5-3** — README §metrics + SOCIAL_POSTS metric list
  drop the fabricated `s4_codec_chosen_total{codec}`. Per-codec
  request distribution comes via the `codec` label on the real
  `s4_requests_total` counter:
  `sum by (codec) (rate(s4_requests_total[5m]))`.
- **#189 R5-4** — `docs/orphan-sidecar-recovery.md` shell
  recipe defines `BACKEND_ENDPOINT` alongside `ENDPOINT` (was
  undefined — copy-paste would fail with empty `--endpoint-url
  ""`).
- **#190 R5-5** — Stale "v0.8.17 may add" claim in
  `docs/orphan-sidecar-recovery.md` advanced to "post-v1.0
  release" since v0.8.17 has shipped without the subcommand.
- **#191 R5-6** — `docs/security/threat-model.md` "Last
  reviewed" stamp advanced from v0.8.18 to v0.8.20 (the v0.8.19
  D-12 change to residual risk #4 made the v0.8.18 stamp
  stale).
- **#192 R5-7** — AWS SigV4 vectors provenance docstring says
  `get-utf8-path` (matches actual fn name) instead of
  `get-utf8`.
- **#193 R5-8** — `--max-body-bytes` default literal computed
  through `u64` then cast to `usize`, so the const-overflow
  hazard on a 32-bit target (5 GiB > `u32::MAX`) is gone.
  s4-server only ships 64-bit Linux today, but cross-compile
  paths now stay honest.

### Tests

- 449 lib + 45 integration + 11 SigV4 vectors + 2 bolero + 1
  chaos unchanged; all green under `RUSTFLAGS="-D warnings"`;
  clippy + fmt clean.

### Notes

- v0.8.20 is the **fifth audit-cycle closeout**. The
  D-6 → R5-2 progression illustrates the gotcha: fix one
  occurrence of a documentation defect, miss the rest. v0.8.20
  swept every remaining `grep -rn 's4_<wrong-name>'` hit.

## [0.8.19] — 2026-06-07

Fourth-round review caught fabrications in the v0.8.18 runbook +
threat-model + bolero module doc, plus a missing CLI flag. Closes
11 doc / minor items and ships `--max-body-bytes` as a first-class
operator knob (the threat model already documented it).

### Added

- **#174 D-1** — `--max-body-bytes <BYTES>` CLI flag. The cap was
  builder-only before v0.8.19 (`with_max_body_bytes`), but the
  threat model already advertised it as an operator-tunable
  defence — the doc was right; the missing piece was the CLI
  flag. Default `5 GiB` matches the AWS S3 single-PUT max.

### Fixed (docs / minor)

- **#175 D-2** — `docs/security/threat-model.md` no longer
  references a non-existent `--state-dir`. Replaced with the
  per-manager `--<x>-state-file` list (versioning,
  object_lock, mfa_delete, cors, inventory, notifications,
  tagging, replication, lifecycle).
- **#176 D-3** — `docs/ops/runbook.md` §1 (disk full) rewritten.
  The pre-D-3 text told operators that `systemctl reload` would
  "stop accepting new connections" — SIGHUP only rotates TLS
  (see §8). Mitigation path now correctly says front S4 with a
  load balancer + drain there, or change
  `--max-concurrent-connections` and **restart** (not reload).
- **#177 D-4** — Runbook §6 (MFA-Delete recovery) now points at
  the `--mfa-delete-state-file <PATH>` operator-supplied file,
  not the fictitious `mfa.json` under a fictitious
  `--state-dir`.
- **#178 D-5** — Runbook §12 (signals) SIGUSR1 description was
  wrong: pre-D-5 it claimed access-log flush; reality is the
  v0.8.5 #86 helper atomically dumps every in-memory state
  manager (versioning / object_lock / mfa_delete / cors /
  inventory / notifications / tagging / replication /
  lifecycle) to its `--<x>-state-file`. Access-log buffer
  drains on shutdown, not on SIGUSR1.
- **#179 D-6** — Runbook metric reference table renamed every
  metric to its canonical name in
  `crates/s4-server/src/metrics.rs`. The pre-D-6 table cited
  `s4_backend_error_total`, `s4_replication_pending_total`,
  `s4_replication_completed_total`,
  `s4_replication_failed_total`,
  `s4_tls_cert_reload_failed_total`,
  `s4_gpu_compress_oom_total` — none of those exist. Real
  names: `s4_replication_dropped_total`,
  `s4_replication_replicated_total`,
  `s4_tls_cert_reload_total{result="err"}`,
  `s4_gpu_oom_total`. The `s4_backend_error_total` shape is
  noted as future-wire-up follow-up rather than silently
  recommended.
- **#180 D-7** — Runbook PromQL alert syntax corrected:
  `action="s3:Bypass*"` (literal `*`, never matches) →
  `action=~"s3:Bypass.*"` (regex matcher).
- **#181 D-8** — Runbook §4 SSE-S4 rotation typo `retiredsl` →
  `retired slots`.
- **#182 D-9** — `crates/s4-server/tests/fuzz_bolero.rs` module
  doc trimmed to the 2 targets actually shipped
  (`sigv4a_auth_header_bolero`, `policy_json_bolero`). The
  pre-D-9 text claimed 4 targets (including a "SigV4 canonical
  query string canonicaliser via the `pub(crate)` test
  re-export" that doesn't exist — `canonical_query_string` is
  private). The two missing targets are explicitly tagged as
  v0.8.19+ roadmap.
- **#183 D-10** — `crates/s4-server/tests/chaos.rs` placeholder
  smoke test now carries concrete `assert_eq!` checks so a
  future refactor can't accidentally leave the file
  compiling-but-useless.
- **#184 D-11** — AWS SigV4 vectors module doc no longer claims
  every vector comes from the AWS-published suite. Split
  honestly into "AWS-published" (4 vectors) and "S3
  spec-derived edge vectors" (7 vectors, motivated by the
  v0.8.16 #150 byte-level fix).
- **#185 D-12** — Threat-model residual risk #4 (versioned
  multipart Range GET fall-back to full read) now includes the
  cost note about large multipart objects + range-heavy
  workloads so operators can weigh leaving versioning Disabled
  against the full-read cost.

### Tests

- 449 lib + 45 integration + 11 SigV4 vectors + 2 bolero + 1
  chaos = unchanged from v0.8.18; all green under
  `RUSTFLAGS="-D warnings"`; `cargo clippy --workspace
  --all-targets` clean; `cargo fmt --all --check` clean.

### Notes

- v0.8.19 has **no code-correctness changes** outside the
  `--max-body-bytes` CLI flag plumbing (`#[clap(long)]` + one
  `with_max_body_bytes(opt.max_body_bytes)` call). The
  remaining 11 items are doc / cosmetic.
- v0.8.19 closes the fourth-round audit. The doc fabrications
  (#175–#180) were a reminder that runbooks written from memory
  are unreliable; future doc work will be verified against the
  source tree before each commit.

## [0.8.18] — 2026-06-07

Production-readiness sweep. Three audit cycles closed every
CRIT / HIGH / MED security finding; v0.8.18 lifts the operational
maturity, conformance posture, and quality-gate infrastructure
to match. No code-correctness changes outside the dispatcher
(small ordering refinement); the rest is **docs, tests, and CI**.

### Added

- **#165 P1** — `docs/security/threat-model.md`. STRIDE-shape
  threat model covering 5 attack surfaces (public S3 wire,
  compressed payload at rest, key handling, backend trust
  boundary, Object Lock posture). Every mitigation traces to a
  shipped issue number from the three audit cycles. Documents
  the explicit non-goals + known residual risks (the
  rustls-webpki CVE chain etc.) so reviewers don't have to
  reverse-engineer them.
- **#166 P2** — `docs/ops/runbook.md`. 12 operational procedures
  (disk full, GPU OOM, backend 5xx storm, SSE key rotation, KMS
  KEK loss, MFA secret loss, replication backlog, TLS rotation,
  orphan sweep, legacy reserved-key migration, audit advisory,
  graceful shutdown) — each in Symptom → Diagnose → Mitigate →
  Recover → Prevent shape. Closes the "no runbook" gap the
  third audit flagged.
- **#167 P3** — AWS SigV4 canonical-request test vectors
  (`crates/s4-server/src/routing.rs::aws_sigv4_canonical_vectors`).
  11 vectors covering vanilla / vanilla-query-order-key-case /
  vanilla-query-order-value / utf8 / non-UTF8 byte round-trip /
  reserved-char encoding / mixed-case percent normalisation /
  bare key / unreserved set / S3 ListObjectsV2 / path with
  spaces. Closes the "no AWS test vector coverage" gap by
  pinning the v0.8.16 #150 byte-level helpers to AWS-published
  expected outputs.
- **#168 P4** — server-side bolero fuzz targets
  (`crates/s4-server/tests/fuzz_bolero.rs`):
  `sigv4a_auth_header_bolero` (SigV4a Authorization parser),
  `policy_json_bolero` (IAM bucket-policy JSON parser). Pairs
  with the existing 7 codec-layer bolero targets so the fuzz
  farm now covers every untrusted parser on the listener edge.
  Corpora seed under
  `crates/s4-server/tests/__fuzz__/<target>/corpus/`.
- **#170 P6** — code coverage CI job (`cargo-llvm-cov` + Codecov
  upload, push-to-main only) and bench smoke job (build + run
  the three `examples/bench_*` binaries to surface bit-rot, not
  to gate on numbers). A criterion-based regression-tracking
  bench is roadmap; the smoke job is the floor.
- **#171 P7** — chaos / fault-injection test scaffold
  (`crates/s4-server/tests/chaos.rs`). Placeholder establishing
  the test target; backend-method-level fault injection
  populates v0.8.19+.

### Changed

- **#169 P5** — README proptest claim corrected from 38 → 39
  properties. The recount tallies functions inside `proptest!
  { ... }` blocks across `fuzz_advanced` (9) + `fuzz_canary`
  (1) + `fuzz_parsers` (19) + `fuzz_server` (10) = 39, matching
  the README within ±1.
- **#172** — `.github/workflows/ci.yml` `notify-on-failure`
  step now deduplicates by SHA prefix before opening an issue,
  so a single failing commit with N failing jobs no longer
  produces N duplicate `ci-failure` issues. The companion
  workflow `.github/workflows/ci-close-resolved.yml`
  auto-closes ci-failure issues once a subsequent main commit
  lands with a green CI run. Closes the "auto-issue spam" the
  user flagged after the v0.8.13 / v0.8.14 retries.
### Fixed

- Stale `ci-failure` GitHub issues #115 / #116 / #117 closed
  with the v0.8.13 / v0.8.14 supersession trail.

### Tests

- 449 lib + 45 integration + 11 AWS SigV4 canonical vectors +
  2 server-side bolero fuzz targets + 1 chaos scaffold = total
  test target count climbs from 519 to ~540, all green under
  `RUSTFLAGS="-D warnings"`; `cargo clippy --workspace
  --all-targets` clean; `cargo fmt --all --check` clean.

### Notes

- v0.8.18 is the **production-readiness floor**. Combined with
  v0.8.17's audit closeout, this is the version a Reddit / Hacker
  News launch reviewer would say "yes, this is a production
  project" about. Threat model + runbook + AWS test vectors +
  fuzz coverage on both the codec and server layers + coverage
  / bench CI are the items reviewers look for first.
- Roadmap items deferred from this release: criterion regression-
  tracking benches (needs baseline storage like
  `benchmark-action/github-action-benchmark`), full chaos
  scenarios (5+ tests against backend-method-level fault
  injection), supply-chain hardening (sigstore release signing,
  reproducible builds, SBOM badge).

## [0.8.17] — 2026-06-07

Third-round audit closeout. The v0.8.16 follow-up review caught
5 residual items (2 MED + 3 LOW): F-5 was gate-conditional, F-13
missed 8 adjacent endpoints, F-12 was dead code, and pre-v0.8.15
user data + v0.8.15 orphan sidecars needed operator hatches.
All five closed. No CRIT / HIGH left.

### Fixed

- **#160 G-1** — F-5 presigned-URL 501 is now unconditional. The
  v0.8.16 check ran AFTER `let gate = gate?;`, so deployments
  without `--sigv4a-credentials` had `?X-Amz-Algorithm=AWS4-ECDSA-
  P256-SHA256` URLs silently fall through to the SigV4 path
  (which doesn't understand SigV4a query auth either). The
  presigned-detect call now runs *before* the gate guard.
- **#161 G-2** — reserved-name guard extended to 8 adjacent
  per-object endpoints: `get_object_acl`, `put_object_acl`,
  `get_object_attributes`, `get_object_tagging`,
  `put_object_tagging`, `delete_object_tagging`, `restore_object`,
  and `upload_part_copy` (both source + destination sides).
  The v0.8.16 F-13 fix only covered GET / HEAD / DELETE. New
  shared helper `S4Service::check_not_reserved_key(...)` +
  `ReservedKeyMode` enum so every site uses the same code; the
  three pre-existing F-13 sites + the M-1 PUT / Copy /
  CreateMultipart sites refactor through the same helper.
- **#162 G-3** — `post_magic_entropy_high` short-sample guard is
  now reachable. The v0.8.16 F-12 check inside the helper
  defaulted to `false` for `<= 48`-byte samples but the upstream
  `MIN_SAMPLE_BYTES = 128` short-circuit in `pick_from_sample`
  filtered every such sample before it could reach F-12. The
  magic-byte arm now runs *above* the MIN_SAMPLE_BYTES gate, so a
  40-byte `BZh:loglog:` user log actually hits the post-magic
  entropy check and gets routed to the default codec (compressed)
  rather than passed through uncompressed. Closes the v0.8.15 M-7
  motivation that v0.8.16 F-12 thought it had closed.

### Added

- **#163 G-4** — `--allow-legacy-reserved-key-reads` CLI flag. A
  migration escape hatch for operators upgrading from
  pre-v0.8.15 deployments that may carry legitimate user-owned
  objects whose key ends in `.s4index`. When set, the
  reserved-name guard does NOT block GET / HEAD / DELETE on
  `.s4index` keys; writes stay blocked regardless of the flag.
  Default `false` matches v0.8.16 behaviour. Boot-time info-log
  is loud when the flag is on so the operator notices the
  migration window is open.
- **#164 G-5** — `docs/orphan-sidecar-recovery.md`: operator
  recipe for sweeping the orphan `<key>.s4index` artifacts that
  v0.8.15 H-g left on versioning-Enabled buckets. v0.8.16 #151
  F-7 stopped emitting new orphans by skipping the sidecar block
  on versioned multipart Complete; this recipe handles the
  one-time cleanup of pre-F-7 leftovers. A future release may
  ship a `s4 admin sweep-orphan-sidecars` subcommand that
  automates the same loop.

### Tests

- 438 lib + 45 integration tests green under `RUSTFLAGS="-D warnings"`;
  `cargo clippy --workspace --all-targets` clean; `cargo fmt
  --all --check` clean.

### Notes

- v0.8.17 is the public-launch target. Three full multi-agent
  audit cycles have closed every CRIT / HIGH / MED finding from
  the pre-release review. Remaining LOW items are tracked as
  roadmap rather than launch-blocking.
- `--allow-legacy-reserved-key-reads` is the **only** new
  operator-visible knob since v0.8.16. The cumulative audit
  surface area still totals two opt-ins
  (`--trust-x-forwarded-for` since v0.8.11,
  `--prefer-columnar-gpu` since v0.8.13) plus this v0.8.17
  migration hatch.

## [0.8.16] — 2026-06-06

Second-round audit closeout. The v0.8.15 HIGH + MED sweep landed
the **shape** of every fix, but a follow-up Codex CLI + Claude
Code review pass found 15 spots where the fix was incomplete,
introduced a regression, or missed an adjacent code path. This
release closes those. Reddit launch target stays v0.8.16+.

### Fixed

- **#145 F-1** — `cpu_zstd` / `cpu_gzip` bomb-detection probe was
  dead code. `Read::take(limit)` returns `Ok(0)` for every
  subsequent `read()` once its budget is exhausted, regardless of
  the inner reader's state — the v0.8.15 #144 probe through the
  consumed `Take` wrapper could never report "more bytes
  available". Drop the wrapper first, then probe via the inner
  decoder. Same fix applied to both the free-fn helpers AND the
  `impl Codec` async path (which is what server-side multipart
  GET actually invokes — the v0.8.15 fix never reached it).
- **#146 F-2** — `decode_index` now verifies inter-entry
  monotonicity. v0.8.15 H-a closed the per-entry `offset+size`
  overflow but a forged sidecar with `[ooff=100,...],[ooff=0,...]`
  still defeated `binary_search_by`, and `start - entries[first_idx].
  original_offset` underflowed `u64`. New `NonMonotonicEntries`
  variant + a `windows(2)` walk.
- **#147 F-3** — `build_index_from_body` had three more
  `as usize` / plain-`+` hazards left over from v0.8.15 H-b
  (`pad_len`, `compressed_size`, cumulative `original_off`).
  `try_from` / `checked_add` everywhere; typed `PayloadTooLarge`
  on overflow.
- **#148 F-4** — SigV4a `x-amz-content-sha256` header is now
  *required* (not just "if present must be signed"), and the
  canonical-request builder rejects `SignedHeaders=` entries
  whose header is absent from the request. The v0.8.15 H-e fix
  let an attacker drop the header entirely → canonical falls
  back to `UNSIGNED-PAYLOAD`. New `MissingContentSha256` /
  `SignedHeaderMissing` typed variants.
- **#149 F-5** — SigV4a presigned URL form
  (`?X-Amz-Algorithm=AWS4-ECDSA-P256-SHA256`) is now explicitly
  rejected with 501 NotImplemented. The pre-F-5 gate only
  recognised the `Authorization` header form; presigned URLs
  silently fell through to the SigV4 path which also doesn't
  understand SigV4a query auth — effectively unsigned.
- **#150 F-6** — `canonical_query_string` / `canonical_uri_path`
  switched to byte-level encoding. The v0.8.15 #132 helpers ran
  `decode_utf8_lossy()`, which replaced any non-UTF8 percent-
  encoded byte (e.g. `%FF`) with `U+FFFD` (`%EF%BF%BD` after
  re-encode), mismatching every signer that operates on raw
  bytes. New `percent_decode_bytes` / `aws_canonical_encode_bytes`
  pair.
- **#151 F-7** — multipart Complete skips sidecar build for
  versioning-Enabled buckets. The v0.8.15 H-g HEAD/stamp ran
  *before* the shadow-key re-PUT, so the sidecar was bound to a
  key that was about to be deleted — Range GET fall-back was
  silently always-on, and the `<key>.s4index` was leaked. Skip
  for versioned multipart; a follow-up issue tracks writing the
  sidecar under the shadow key with the shadow's ETag.
- **#152 F-8** — `copy_object` strips client `s4-*` metadata
  unconditionally (moved outside the `if let Ok(head)` block).
  The v0.8.15 M-2 fix's strip only ran when the backend HEAD
  succeeded; on HEAD failure or `metadata=None` the client's
  `S4-CODEC=...` injection survived.
- **#153 F-9** — CORS validation now also runs on
  `CorsManager::from_json` (snapshot restore) and the runtime
  `rule_matches_method` no longer honours `pat == "*"`. The
  v0.8.15 #139 fix only gated `PutBucketCors`; a pre-#139
  snapshot file with `AllowedMethods: ["*"]` survived restore
  and the legacy matcher kept honouring it.
- **#154 F-10** — streaming PUT over-length is now a per-chunk
  mid-flight check (inside the read loop), not a post-flight
  check at end-of-stream. v0.8.15 M-4 let a client ship 100 GiB
  through the compress + frame pipeline before rejecting with
  `RequestBodyLengthMismatch`; F-10 short-circuits the moment
  the cumulative read exceeds the declared length.
- **#155 F-11** — `parse_iso8601` now applies per-month day
  caps (+ leap-year for February). The v0.8.15 H-f bounds
  accepted any `day ∈ [1, 31]`, so `2026-02-31` silently
  normalised to `2026-03-03` through the civil-from-date
  arithmetic.
- **#156 F-12** — `SamplingDispatcher`'s `post_magic_entropy_high`
  defaults to `false` (=  "don't trust the magic alone") on
  short samples (≤ 48 bytes). The v0.8.15 M-7 fix returned
  `true` for short samples, defeating the original motivation —
  a 40-byte `BZh:loglog:` user log file still passthrough'd
  even after M-7.
- **#157 F-13** — reserved-name guard now also fires on GET /
  HEAD / DELETE. The v0.8.15 #137 fix only blocked PUT / Copy /
  CreateMultipart, so a curious client could
  `GetObject(<key>.s4index)` and read the raw sidecar (frame
  layout, source ETag) — information disclosure. DELETE on a
  sidecar key would have orphaned the sidecar.
- **#158 F-14** — `apply_default_on_put` re-arms expired
  retention. Pre-F-14, a key whose `retain_until` had elapsed
  but whose state record still lived in the manager silently
  blocked re-arming on the next PUT — AWS S3 spec is that each
  PUT under bucket-default re-arms the clock. `retain_until <=
  now` no longer counts as "active retention".
- **#159 F-15** — `INDEX_HEADER_BYTES = 40` constant was a typo
  (v2 fixed header is actually 44 bytes). Now `#[deprecated]`
  with the value corrected to `HEADER_FIXED_V2`, with
  `HEADER_FIXED_V1` / `HEADER_FIXED_V2` exposed as the
  successor public constants.

### Tests

- 438 lib + 45 integration tests green under `RUSTFLAGS="-D warnings"`;
  `cargo clippy --workspace --all-targets` clean; `cargo fmt
  --all --check` clean.

### Notes

- v0.8.16 is **the** version for the Reddit launch. Two
  full multi-agent audit cycles (v0.8.11-15 + v0.8.16) have
  closed every CRIT / HIGH / MED finding from the pre-release
  review. Remaining LOW items are tracked as roadmap rather
  than launch-blocking.

## [0.8.15] — 2026-06-06

Post-launch security audit closeout. Picks up the HIGH (8) + MED (10)
findings that were marked as "follow-up" in the v0.8.11–v0.8.14
release notes (Codex CLI + Claude Code review of the wire format,
codec layer, multipart pipeline, IAM / SigV4a stack, and operational
surfaces). No CRIT remained. WASM client + multipart sidecar
correctness + AWS-canonical SigV4 interop were the load-bearing
gaps; this release closes them.

### Fixed

- **#130 H-a** — `FrameIndexEntry::original_end()` /
  `compressed_end()` use `saturating_add` and `decode_index` rejects
  `offset+size` overflow per entry (`index.rs:99 / 391`). A forged
  sidecar entry with `original_offset = u64::MAX-10` no longer wraps
  the range planner.
- **#131 H-b / H-c** — 32-bit `usize` casts in the WASM decoder
  hardened. `multipart.rs:135` uses `usize::try_from` on
  `compressed_size` / pad length (new typed `PayloadTooLarge`
  error). `index.rs:288 / 321` adds `MAX_FRAMES = 16M` and
  `MAX_ETAG_BYTES = 4 KiB` upper bounds so a forged sidecar can't
  trick `s4-codec-wasm` into a truncated payload read.
- **#132 H-d** — `routing.rs::canonical_query_string` now decodes
  each key/value, re-encodes per the AWS canonical RFC 3986
  unreserved set, then sorts on the encoded form. Likewise
  `canonical_uri_path` does AWS-canonical path encoding (slashes
  literal). Real AWS SDK / aws-crt-cpp signatures now interop end-
  to-end (in v0.8.12 #126 we fixed the string-to-sign shape, but
  the canonical request preceding it still mismatched; #132 closes
  the loop).
- **#133 H-e** — `sigv4a.rs::verify_request` requires `host` in
  `SignedHeaders=` and, when `x-amz-content-sha256` is present in
  the headers, requires that name be in `SignedHeaders=` too. Two
  new typed errors: `HostNotSigned`, `ContentSha256NotSigned`. AWS
  S3 enforces both; closes the MITM `Host`-rewrite vector.
- **#134 H-f** — `policy.rs::parse_iso8601` clamps year ∈
  `[1970, 9999]` + month / day / time-of-day bounds *before* the
  civil-from-date multiply. Policy `Condition: DateLessThan
  ["9999999999...Z"]` no longer wraps the i64 product into a
  silently-flipped comparison.
- **#135 H-g** — multipart Complete now HEADs the freshly-completed
  object and stamps `source_etag` / `source_compressed_size` on the
  sidecar (`service.rs:4785`). Matches the single-PUT path. Without
  the binding, a subsequent backend-side mutation (lifecycle move,
  out-of-band CopyObject) wouldn't trip the stale-sidecar check on
  the next Range GET.
- **#136 H-h** — `decompress_multipart` enforces an aggregate
  output cap of `--max-body-bytes` (default 5 GiB) across all
  frames, in addition to the existing per-frame cap. Pre-flight
  on `header.original_size` plus post-decode `produced` accounting.
  A forged multi-frame body can no longer pin tens of GiB of
  plaintext in `BytesMut::extend_from_slice`.
- **#137 M-1** — reserved-name guard on PUT / Copy / Create
  multipart. Keys ending in `.s4index` return `InvalidObjectName`
  at the listener edge. Pairs with the new
  `s4_codec::index::SIDECAR_SUFFIX` constant + the
  `is_reserved_sidecar_key` helper as the single source of truth.
- **#138 M-2** — `copy_object` with
  `MetadataDirective: REPLACE` strips every `s4-*` key from the
  client-supplied metadata before re-populating from the source
  HEAD. Pre-M-2 `or_insert_with` preferred the client's value,
  letting a malicious client inject e.g.
  `s4-original-size=5368709120` for downstream misalloc / silent
  data corruption.
- **#139 M-3** — `PutBucketCors` rejects `AllowedMethods` outside
  the canonical `{GET, PUT, POST, DELETE, HEAD}` set (including
  the `*` wildcard) with `InvalidArgument`. New
  `cors::CorsManager::validate` is the listener-side check;
  matches AWS S3 behaviour.
- **#140 M-4** — streaming PUT path adds an over-length guard via
  the new `CodecError::OverlengthStream { expected, got }` variant.
  A client sending `Content-Length: 1` followed by 1 GiB of body
  now gets `RequestBodyLengthMismatch` (400) instead of silent
  storage. Mirrors AWS S3 wire behaviour.
- **#141 M-5** — `ObjectLockManager::apply_default_on_put` no
  longer auto-applies bucket-default retention onto a key whose
  only existing state is `legal_hold_on = true`. Pre-M-5 a
  legal-hold-only key would silently pick up the Governance clock
  on the next overwrite PUT.
- **#142 M-6 / M-7** — `SamplingDispatcher` now requires the
  bytes *after* a magic-byte hit to also show high entropy before
  routing to `Passthrough`. User logs that happen to start with
  `BZh` (or any other 2–3-byte magic by coincidence) keep getting
  compressed. Adversarial-bypass limits (low-entropy prefix on
  random body) are documented as a known sampling caveat; the
  multi-window variant is a listener-side follow-up.
- **#143 M-8** — `pad_to_minimum` doc / contract pinned. Maximum
  overshoot is `PADDING_HEADER_BYTES - 1 = 11` bytes; doc no
  longer claims a uniform 12-byte "ε". The `reserve(0)` no-op is
  also gated behind `payload_len > 0`.
- **#144 M-9** — `cpu_zstd` / `cpu_gzip` bomb-detection error
  messages probe one byte past the truncation cap so the log
  distinguishes "decoder happened to land in
  `(orig, orig+1024]`" from "actual bomb, more bytes available".
  Operators on a triage page get an actionable signal instead of
  a misleading byte count.

### Changed

- **Inter-crate `version` pins** (`crates/s4-server/Cargo.toml`,
  `crates/s4-codec-py/Cargo.toml`) widened from `0.8.10` to `0.8`
  so end-users pulling `cargo install s4-server` always resolve
  to the latest published `s4-codec` / `s4-config` for the
  current minor — closes the "pinned to 0.8.10 publishes" hazard
  that surfaced during the v0.8.14 crates.io rollout.

### Tests

- 438 lib + 45 integration tests green under
  `RUSTFLAGS="-D warnings"`; `cargo fmt --all --check` clean;
  `cargo clippy --workspace --all-targets` clean.

### Notes

- v0.8.15 is **the** version to target for the public Reddit
  launch. Every CRIT + HIGH + MED finding from the multi-agent
  pre-release review is now closed. Remaining LOW items
  (`shannon_entropy` u32 saturation only-at-`4 GiB`-sample,
  `INDEX_HEADER_BYTES` naming cosmetics, `pad_to_minimum`
  `reserve` micro-optimisation, SigV4a skew at extreme tolerances)
  are tracked in a `LOW.md` follow-up and don't block correctness
  or AWS-compat.

## [0.8.14] — 2026-06-06

Hotfix on top of v0.8.13. The v0.8.13 #127 (MED-B) "force buffered
PUT path when a whole-body checksum is supplied" attempt regressed
the MinIO E2E job — modern AWS SDKs auto-add an
`x-amz-checksum-crc32` trailer by default, which made every SDK
PUT lose the streaming-framed code path and therefore lose its
sidecar. Range GET fast-path and `upload_part_copy` over an
S4-framed source both depend on the sidecar being there.

### Fixed

- **#129 — streaming PUT path no longer downgraded to buffered on
  client-supplied checksums (`service.rs:2396`).** Drops the
  `!client_supplied_checksum` term from `use_framed`. Streaming
  PUTs are framed again and the sidecar is produced as before.
  This re-opens the v0.8.11 #122 fail-open hole for the streaming
  case only — the buffered PUT branch and `UploadPart` continue
  to verify. True streaming verify (tee-into-hasher on the
  chained input + final digest check at end-of-stream) is the
  tracked follow-up. Tests:
  `range_get_falls_back_to_full_when_sidecar_etag_stale`,
  `upload_part_copy_propagates_source_version_id`.

### Notes

- v0.8.14 is `v0.8.13` minus the use_framed downgrade. The MED-B
  helper itself (`verify_client_body_checksums` with all six AWS
  algorithms from MED-C) remains active on every buffered PUT and
  `UploadPart` request — those paths are bit-for-bit unchanged.
- Operators who want the buffered + verify behaviour on every PUT
  can set `--dispatcher always` with `--codec passthrough`; the
  sampling dispatcher continues to make the codec choice per-PUT.

## [0.8.13] — 2026-06-06

Pre-release **MED sweep** — 4 MED findings from the same Codex CLI
+ Claude Code review. Each one was previously flagged as
follow-up scope in the v0.8.11 / v0.8.12 cut notes; shipping them
now keeps the launch posture self-consistent. No CRIT / HIGH
remaining from the review.

### Added

- **#125 MED-impl — Bitcomp auto-routing for columnar-integer
  payloads (`dispatcher.rs:167`).** The README claim
  "integer/columnar → Bitcomp" was previously honoured only via
  explicit `--codec nvcomp-bitcomp`. The sampling dispatcher now
  carries a per-stride-position byte-histogram detector
  (`looks_columnar_integer`) that flags a sample as a u32 / u64 LE
  integer column when one stride's max-vs-min byte entropy gap
  exceeds 4.0 bits (the signature of bounded ints — high entropy
  on the low byte, ≈ 0 entropy on the high byte). New
  `--prefer-columnar-gpu` CLI flag opts a deployment in; off by
  default so v0.8.12-and-earlier deployments are bit-for-bit
  unchanged. Tests cover postings / timestamps / text / random /
  size-threshold / no-GPU branches.
- **#128 MED-C — full AWS checksum coverage on the buffered PUT +
  UploadPart path (`service.rs:132`).** The v0.8.11 #122 HIGH-12
  fix verified `Content-MD5` / `x-amz-checksum-crc32c` /
  `x-amz-checksum-sha256`; this release extends
  `verify_client_body_checksums` to also cover `x-amz-checksum-crc32`
  (IEEE 802.3, via `crc32fast`), `x-amz-checksum-sha1` (new
  `sha1 = "0.10"` dep), and `x-amz-checksum-crc64nvme` (small
  inline table-driven implementation). Mismatch → `BadDigest`.

### Fixed

- **#126 MED-A — SigV4a now verifies the AWS-spec string-to-sign
  instead of the raw canonical request (`sigv4a.rs:441`).** The
  previous code passed `canonical_request_bytes` straight to the
  p256 verifier — fine for S4's own `SigV4aGate` (signer and
  verifier both used the canonical bytes) but rejected real AWS
  SDK / aws-crt-cpp signatures, which hash the canonical request
  and sign `"AWS4-ECDSA-P256-SHA256" || x-amz-date ||
  credential_scope || hex(sha256(canonical))`. The verifier now
  builds the spec-correct string-to-sign and the existing
  freshness / scope / region / signed-header gates are unchanged.
  Test fixtures (`build_signed_request`, routing-layer fixture,
  feature_e2e fixture) updated to sign the string-to-sign.
- **#127 MED-B — streaming-framed PUT now falls back to the
  buffered path when the client supplied a whole-body checksum
  (`service.rs:2287`).** The streaming pipeline consumes the body
  chunk-by-chunk and cannot produce a whole-body digest without
  buffering. Rather than silently dropping the checksum (the
  v0.8.11 fail-open hole) or returning an opaque error, we
  redirect to the existing buffered branch, which runs
  `verify_client_body_checksums` and produces `BadDigest` on
  mismatch. TTFB cost is paid only for PUTs that actually ship a
  checksum header; non-checksummed PUTs keep the streaming
  benefit. True streaming verify (tee-into-hasher on the chained
  stream) is tracked as a follow-up — the buffered fallback is
  the correctness floor.

### Tests

- 438 lib + 45 integration tests green; `cargo fmt --all --check`
  clean. New tests added under `dispatcher::tests` for the
  columnar branch.

### Notes

- v0.8.13 closes the full CRIT + HIGH + MED set from the Codex CLI
  + Claude Code pre-release review. The remaining items
  (true-streaming PUT checksum verify, encryption-aware sidecar,
  multi-window sampling) are scoped as roadmap improvements rather
  than launch-blocking findings.

## [0.8.12] — 2026-06-06

Pre-release **HIGH sweep** — 9 HIGH findings from the same Codex CLI
+ Claude Code review pass that produced the v0.8.11 CRIT cut. Every
HIGH widens the gap between what the README claims and what the
gateway actually enforces (Object Lock, IAM, integrity); shipping
them before the launch keeps the public posture honest.

### Fixed

- **#116 HIGH-6 — multipart Complete now re-verifies Object Lock on
  the target key (`service.rs:4306`).** The single-PUT path consults
  the lock manager at L2007; Complete used to skip the check, so an
  attacker with `s3:PutObject` could `CreateMultipartUpload` against
  a `legal_hold=on` / under-retention key and overwrite it at Complete.
  CompleteMultipartUpload doesn't carry the bypass header on the
  wire — operators who need to break Governance call
  `PutObjectRetention` first.
- **#117 HIGH-7 — `x-amz-bypass-governance-retention` now requires
  the matching IAM permission (`service.rs:3411 / 5227`).** The
  bypass flag used to flow straight into `state.can_delete(...)`
  regardless of policy. Now the gateway runs
  `enforce_policy("s3:BypassGovernanceRetention", ...)` before
  honouring the header; an unprivileged caller's bypass flag is
  silently downgraded to `false` and the lock keeps blocking.
- **#118 HIGH-8 — Object Lock administrative APIs now run
  `enforce_policy` with the matching action verbs (`service.rs:5128 /
  5169 / 5210 / 5250 / 5293`).** `put_object_legal_hold`,
  `put_object_retention`, `put_object_lock_configuration`,
  `get_object_legal_hold`, `get_object_retention`,
  `get_object_lock_configuration` were previously ungated — a bucket
  policy denying `s3:PutObjectLegalHold` could be bypassed by
  hitting the API directly.
- **#119 HIGH-9 — multipart API lifecycle now runs the same
  `s3:PutObject` gate as single-PUT (`service.rs:3941 / 4097 / 4298 /
  4818 / 4969`).** `create_multipart_upload`, `upload_part`,
  `complete_multipart_upload`, `abort_multipart_upload`,
  `upload_part_copy` were all previously ungated; a policy that
  denied `s3:PutObject` was bypassable by switching the client to
  the multipart wire path. README L693 ("every PUT / GET / DELETE
  ... is evaluated") was previously aspirational; it is now correct.
- **#120 HIGH-10 — sidecars are suppressed when the on-disk body
  will be SSE-encrypted (`service.rs:2308 / 4436`).** The sidecar
  describes offsets into the pre-encrypt `compressed` body, but the
  bytes the backend stores under SSE-S4 / SSE-C / SSE-KMS are
  *post-encrypt* (different length + layout). A Range GET would
  slice the ciphertext at the stale offsets and 500. Encrypted-object
  Range GET now buffers the full body, decrypts, and parses frames
  — partial-fetch perf is traded for correctness. An encryption-
  aware sidecar format is tracked as a follow-up.
- **#121 HIGH-11 — rate-limit pool is bounded (`rate_limit.rs:55 +
  73`).** The per-`(rule, principal, bucket)` `DashMap` was unbounded;
  a request stream cycling fake access-key-ids could grow the pool
  by millions of entries until the gateway OOM'd. New
  `DEFAULT_MAX_ACTIVE_LIMITERS = 16384` cap; overflowing keys fall
  onto a per-rule shared limiter (still rate-limited, just share one
  bucket). `active_limiter_count()` accessor surfaces the live size
  for the Prometheus gauge.
- **#122 HIGH-12 — client-supplied integrity checksums are now
  verified against the received body (`service.rs:132 / 2390 /
  4319`).** `Content-MD5`, `x-amz-checksum-crc32c`, and
  `x-amz-checksum-sha256` are computed over the received body
  before the gateway strips the header on the way to the backend.
  Mismatches surface as `BadDigest` (HTTP 400), matching AWS. The
  remaining S3 checksum algorithms (CRC32 non-Castagnoli, SHA-1,
  CRC64-NVME) and the streaming-framed PUT path are tracked as
  follow-ups — covered in this release: buffered-path PUT,
  `UploadPart`.
- **#123 HIGH-13 — SigV4a Authorization parser now rejects
  duplicate `Credential=` / `SignedHeaders=` / `Signature=` fields
  (`sigv4a.rs:226`).** The previous loop overwrote on each match;
  an attacker could send `Credential=AKIAVICTIM,Credential=AKIAATTACKER,
  Signature=<valid-for-attacker>` and the verifier would pick the
  attacker while a sidecar parser scanning left-to-right would see
  the victim. Auth-confusion vector closed; duplicates now surface
  as `BadSignature` at parse time.
- **#124 HIGH-14 — `decode_index` no longer pre-allocates the full
  attacker-claimed entry table (`index.rs:312`).** Mirrors the #89
  hardening pattern: the initial `Vec::with_capacity` clamps to
  4096 entries (128 KiB at 32 B/entry) and the `push` loop grows
  the vector under the existing `expected_remaining == input.len()`
  bound. A 3.2 GiB forged sidecar can no longer drive a 3.2 GiB
  `Vec` allocation. Closes the obvious "same class as #89, applied
  to sidecar" finding a reviewer would otherwise paste next to the
  README's fuzz claim.

### Tests

- 438 lib + 45 integration tests green; `cargo fmt --all --check`
  clean.

### Notes

- v0.8.12 is a strict superset of v0.8.11; the same `--trust-x-forwarded-for`
  CLI flag still controls the v0.8.11 CRIT-4 default behaviour.
- This release closes the **complete CRIT + HIGH set** from the
  pre-release review. Remaining MED / LOW items (12 magic-byte rule
  count is correct as of v0.8.11, dispatcher `Bitcomp` auto-promote,
  multi-window sampling, etc.) are tracked individually and don't
  block the public launch.

## [0.8.11] — 2026-06-06

Pre-release **security review sweep** — 5 CRIT findings from a
combined Codex CLI + Claude Code review pass, fixed and shipped
before the public launch. Every CRIT is a real auth / data-integrity
hole an attacker (or a confused operator) could trigger via the S3
wire protocol; the patch is one-commit because the findings landed
together and share build / CHANGELOG churn.

### Fixed

- **#111 CRIT-1 — chunked SSE GET no longer returns un-decompressed
  bytes (`service.rs:2936`).** When `--sse-s4-key` + `--sse-chunk-size > 0`
  were both configured, the `S4E5` / `S4E6` GET path took an early
  return that wired the decrypt stream straight into the HTTP body,
  skipping the codec decompress / frame parser stages. Clients
  received S4F2-framed / zstd-compressed bytes instead of plaintext.
  Fix: the streaming early-return is now gated on
  `codec == Passthrough && !needs_frame_parse`; everything else
  falls through to the buffered decrypt path
  (`decrypt_chunked_buffered_default`), which feeds the existing
  decompress pipeline. Streaming TTFB benefit preserved for the
  passthrough case it was designed for.
- **#112 CRIT-2 — multipart SSE replication no longer leaks plaintext
  to the destination bucket (`service.rs:4247`).** The replication
  dispatcher snapshot was taken before the SSE re-encrypt branch, so
  destinations received the assembled-but-unencrypted framed body
  even when SSE-S4 / SSE-C / SSE-KMS was active. Destination GETs
  then failed to decrypt — or, worse, succeeded in handing plaintext
  to a downstream consumer that had been promised at-rest encryption.
  Fix: refresh `replication_body` with the post-encrypt `new_body`
  inside the re-PUT branch so destinations always see the same
  on-disk shape the source does.
- **#113 CRIT-3 — `DeleteObjects` no longer bypasses Object Lock /
  bucket policy / versioning / sidecar cleanup (`service.rs:3588`).**
  Batch delete used to MFA-check the request and then call
  `self.backend.delete_objects(req)` straight through, which meant
  a key under `legal_hold = on` (or `Retain: Governance`) could be
  removed by listing it inside a DeleteObjects XML — directly
  contradicting the README's compliance posture. Fix: dispatch every
  `ObjectIdentifier` through the gated per-object `delete_object`
  handler, accumulate `DeletedObject` / `Error` lists, and respect
  `Delete.quiet`. Failures surface as per-key entries in `Errors`
  (S3 spec — batch never aborts on a single failure).
- **#114 CRIT-4 — `X-Forwarded-For` is no longer trusted from any
  client by default (`service.rs:1241`).** The `aws:SourceIp` Condition
  key and the access-log `remote_ip` field both used to consume the
  leftmost token of a client-supplied header. A public-internet
  request could spoof `curl -H 'X-Forwarded-For: 10.0.0.1'` and
  satisfy any IP-allowlist Allow rule. Fix: the header is ignored by
  default; operators behind a trusted reverse proxy opt in with
  `--trust-x-forwarded-for` (new CLI flag) and accept responsibility
  for the proxy stripping client-supplied values. Boot log explicitly
  states which mode is active. A future release will validate the
  forwarded address against a `--trusted-proxies` CIDR list using
  the real TCP peer address; this opt-in flag closes the immediate
  auth-bypass without that plumbing.
- **#115 CRIT-5 — policy parser no longer silently ignores
  `NotAction` / `NotResource` / `NotPrincipal` and other unsupported
  AWS keywords (`policy.rs:142`).** Without `#[serde(deny_unknown_fields)]`
  on `StatementJson` / `PolicyJson`, a policy author writing
  `{"NotResource": "secret/*"}` was silently parsed as "no Resource
  restriction" — the rule then matched every object, including
  `secret/`. Fail-open is the worst kind of policy bug; this turns
  it into a parse error at config-load time so the operator sees
  the misconfiguration immediately. The top-level `Id` field (AWS
  canonical) is now explicitly accepted-and-ignored.

### Changed

- **README "14 magic-byte rules" → 12** (`README.md:373`,
  `SOCIAL_POSTS.md:90 / :296`). The `looks_already_compressed` matcher
  has 12 rules (gzip / zstd / PNG / JPEG / PDF / ZIP / 7z / xz /
  bzip2 / ftyp / EBML / WEBP), not 14. The honest count goes into
  the launch material.
- **README zstd decompression bomb hardening wording**
  (`README.md:703`) clarified. The guard caps the decode at
  `manifest.original_size + 1024`, not "regardless of an
  attacker-controlled manifest claim" — a 5 GiB manifest claim is
  honored up to 5 GiB, so operators need an additional per-request
  / per-frame memory ceiling at the listener edge for adversarial
  uploads.

### Tests

- New defaults exercised: `tests/roundtrip.rs::policy_iam_condition_ip_address_denies_outside_cidr`
  now opts in to `with_trust_x_forwarded_for(true)` since the test
  models a trusted-proxy deployment.
- 438 lib + 45 integration tests green; `cargo fmt --all --check`
  clean.

### Notes

- This is a **behavioural breaking change** for operators relying on
  the implicit `X-Forwarded-For` trust. Set `--trust-x-forwarded-for`
  to restore the prior behaviour when the gateway is behind a
  trusted reverse proxy that scrubs client-supplied values.
  Gateways listening directly on the public internet should leave
  the flag off and move IP gating to the proxy.

## [0.8.10] — 2026-05-20

Pre-launch hardening **Phase 3** (#111 tracker) — docs補強 sweep
closing the remaining 8 audit findings (#100 #101 #102 #103 #104 #105
#106 #107 #108). No code changes; only README + new
`docs/THIRD_PARTY_LICENSES.html` + new `about.toml`.

This completes the full pre-launch audit cycle: **20 / 20 findings
resolved**. Tracker #111 closed.

### Added

- **#100 Security & threat model section** — explicit boundaries for
  authentication scope (S4 verifies SigV4 / SigV4a on incoming; uses
  its own credentials to backend, no client identity delegation), TLS
  termination (S4 owns its listener; second hop to backend uses SDK's
  rustls + system root CA), bucket policy enforcement (both layers
  must permit), hyper body-size / connection / read-timeout limits
  (from v0.8.5 #84), single-tenant-by-design isolation model, and
  explicit non-goals (not an IDS/WAF, no body logging, no IAM proxy).
- **#101 SDK compatibility matrix** — 11-row table: aws-cli /
  boto3 / aws-sdk-rust (✅ Tested), aws-sdk-go-v2 / aws-sdk-java-v2 /
  MinIO mc / rclone (✅ Should work; wire-level shapes covered),
  s3cmd (⚠️ no SigV2 fallback), presigned URLs / conditional
  GET-PUT / Content-MD5 / x-amz-content-sha256 (✅ Tested),
  Content-Encoding: gzip double-encode caveat (⚠️ documented).
  Endpoint URL style explainer (both virtual-hosted and path-style
  accepted).
- **#104 Third-party license disclosure** — new `about.toml` config
  for cargo-about + auto-generated `docs/THIRD_PARTY_LICENSES.html`
  (617 KB, ~350 transitive crates, all permissive: Apache-2.0 / MIT /
  BSD-{2,3}-Clause / ISC / Zlib / Unicode / 0BSD / MPL-2.0 /
  OpenSSL / CDLA-Permissive-2.0). Linked from the License section of
  README. License section also got nvCOMP redistribution clarification
  (BYO; NVIDIA SLA terms apply).
- **#105 "When NOT to use S4" section** — already-compressed payloads
  (passthrough cost), small objects (< 16 KiB break-even), metadata-
  ops dominant workloads, ultra-low-latency tail SLOs, single-region
  cold-storage-only (Glacier prices already low enough), strict
  regulatory environments without third-party audit on file, and
  the restated "do not use as the only copy of irreplaceable data"
  pre-1.0 guidance.
- **#106 Durability / corruption recovery section** — explicit write
  protocol (PUT main → PUT sidecar → CompleteMultipart on multipart;
  main object PUT is the commit point), 6-row failure-mode table
  (client disconnect mid-PUT / sidecar PUT fail / multipart Complete
  fail / corrupted body / sidecar divergence / sidecar missing) with
  recovery actions per row, CRC scope documentation (what it catches
  + what it doesn't), and repair tool status (v0.9 roadmap; manual
  recovery path for divergence cases until then).

### Changed

- **#107 TTFB number sourcing** — "TTFB ms-class" qualified with
  measurement conditions (RTX 4070 Ti SUPER + Ryzen 9 9950X,
  single-pass 256 MiB compressible input, codec `cpu-zstd-3`, single
  concurrent request, S4 colocated with backend, excluding TLS
  handshake + SigV4 verify). Headline cell in How it Compares table
  also links to the conditions block.
- **#108 Helm chart "image not yet on Docker Hub" restructuring** —
  Helm section now leads with "Build the image yourself first" and
  spells out the `docker build → docker push → helm install --set
  image.repository` flow, replacing the prior trailing
  "the image is not yet on Docker Hub" caveat that read as
  unfinished work.
- **#102 #103 competitor diff** (fully closed; Phase 2 was partial) —
  the SDK compat matrix + threat model section together complete
  the "narrow the framing" guidance (we are a transparent-compression
  proxy in front of an existing S3 backend, not a storage system
  competing with MinIO / Garage / SeaweedFS). License framing pinned
  to per-project upstream LICENSE links + footnote on terms changing
  between releases.

### Test posture

622 workspace tests pass (unchanged) / 52 ignored. `cargo audit` clean
(same 4 documented ignores as v0.8.8 / v0.8.9).

### Tracker #111

**CLOSED.** All 20 pre-launch audit findings (5 Phase 1 + 7 Phase 2 +
8 Phase 3) resolved across v0.8.8 → v0.8.10.

## [0.8.9] — 2026-05-20

Pre-launch hardening **Phase 2** — README claim accuracy sweep (#111 tracker).
Closes 7 audit findings from the claude + codex cross-review.
No code changes. Crates republished only to refresh crates.io README.

### Documentation

- **#93 (CRITICAL)** "No lock-in: read your bucket directly with aws-cli"
  rewritten — the compressed objects + S4IX sidecars ARE S3-native (any
  S3 client can read them), but the original payload requires
  `s4-codec` / `s4-codec-py` / `s4-codec-wasm` to decompress. The
  Apache-2.0 decoder tools are the actual anti-lock-in story; the
  earlier line implied stock `aws-cli` returned original bytes, which
  contradicted the rest of the README.
- **#94 (CRITICAL)** "S3 API compatibility: ✅ Full" replaced with an
  explicit matrix listing 25+ surfaces (PUT / GET / multipart / HEAD /
  Range / Conditional / ACL / versioning / object-lock / lifecycle /
  notifications / replication / policy / tagging / CORS / inventory /
  MFA delete / SSE-S3 / SSE-KMS / SSE-C / Select / presigned URLs /
  SigV4 + SigV4a / storage class transitions). NotImplemented surfaces
  (RequestPayment / Accelerate / Logging / cross-region S4-chain
  replication) marked "—".
- **#95 (HIGH)** "Cuts your AWS S3 bill 50–80%" rephrased as "Reduces
  S3 **storage bytes** 50–80% for compressible payloads. Total bill
  impact depends on workload mix — request cost / egress / GPU compute
  unchanged." Headline 99%-saved example kept but qualified.
- **#96 (HIGH)** Bench table got a "Codec verdict" column making the
  GPU vs CPU narrative explicit (CPU wins on text/log, GPU wins on
  integer/columnar) so readers don't misread "cpu-zstd-3 best
  throughput on nginx logs" as undermining the GPU pitch.
- **#97 (HIGH)** "transparently compresses every object with GPU
  codecs" reworded to acknowledge per-payload dispatcher routing
  (text → CPU, columnar → GPU, already-compressed → passthrough, no
  GPU → CPU end-to-end) with the Prometheus counter +
  `codec_chosen` access-log field cited for observability.
- **#99 (HIGH)** Range GET claim qualified — works out-of-box for
  arrow-rs / datafusion / duckdb suffix-range Parquet footer reads;
  parallel range reads across overlapping frame extents do extra
  decode work and are not yet optimised. Tracked by the matrix entry
  + a note explicitly calling out the parquet/ORC cross-validation
  harness as roadmap.
- **#109 (LOW)** Added explicit **"Status: alpha / early-access"**
  callout at the top of Project Status, with "looking for design-
  partner users", a pre-1.0 wire-format-may-break disclaimer, and
  "do not use S4 as the only copy of irreplaceable data; pair with
  backend-native replication / versioning until v1.0 + first public
  production deployment is documented" guidance. Replaces the
  prior "Production-ready" assertion that wasn't backed by any
  public production user yet.

### Competitor diff polish (partial #102 #103)

- Comparison table got per-project upstream links (MinIO / Garage)
  + "stance" row that frames S4 as a transparent-compression proxy
  rather than a storage-system replacement. License cells now link
  to upstream LICENSE files; in-table license framing softened to
  "upstream LICENSE: AGPLv3 (+ commercial)" with a footnote
  clarifying these can change between releases.

### Test posture

622 workspace tests pass (unchanged from v0.8.8) / 52 ignored.
`cargo audit` clean.

## [0.8.8] — 2026-05-20

Pre-launch hardening **Phase 1** (issue tracker #111). claude + codex
cross-review of v0.8.7 surfaced 20 findings ahead of HN / Reddit-scale
distribution; this release ships the security / CI / quickstart subset
(5 issues closed). README claim tone-down + docs補強 (Phase 2 / 3) are
queued in #93–#109 and ship in a follow-up.

### Fixed

- **#90 pyo3 0.22.6 buffer-overflow CVE (RUSTSEC-2025-0020)** — bumped
  `s4-codec-py` from pyo3 0.22 to 0.24. API migration was a 13-call
  rename (`get_type_bound` → `get_type`, `PyBytes::new_bound` →
  `PyBytes::new`); pyo3 0.21 had already introduced the `Bound<'py>`
  API the binding was using, so the upgrade was a deprecation-warning
  cleanup rather than a structural rewrite. Existing pytest suite
  (v0.8.5 #85) green post-upgrade.
- **#92 metrics 0.24.5 yanked** — bumped to 0.24.6 via `cargo update`.
  `rustls-acme 0.15.1 → 0.15.2` swept up the same way.
- **#92 astral-tokio-tar 4 dev-only CVEs (RUSTSEC-2026-0066/0112/0113/
  0145)** — bumped `testcontainers-modules` 0.14 → 0.15 (pulls
  `astral-tokio-tar 0.6.x` transitively). dev-dependency only, not in
  shipped binary, but cleans up `cargo audit` output. Existing E2E
  test suite green post-upgrade.
- **#98 README cargo install caveats** — added explicit Rust 1.92+
  requirement, GPU codec opt-in (`--features nvcomp-gpu` + CUDA
  toolchain + `NVCOMP_HOME`), and binary name (`s4`, not `s4-server`)
  callouts under the Install via cargo block.
- **#110 README Docker quickstart** — fixed the broken `aws cp s3://
  demo/big.log -.compressed` line (unnatural destination name + no
  `big.log` source generation step). Now generates a 135 MiB sample
  via `head -c 100M /dev/urandom | base64 > big.log`, copies to
  `./big.log.roundtrip` and `./big.log.compressed`, and shows
  expected lossless roundtrip + size delta.

### Added (CI)

- **`security-audit` job** — `cargo audit --locked --version 0.21`
  runs on every push and PR, gates `notify-on-failure` (so a fresh
  RUSTSEC advisory blocks merge). Three `--ignore` flags for the
  rustls-webpki CVEs (#91, upstream-blocked) and one for the
  unmaintained `rustls-pemfile` caretaker advisory; each ignore
  carries an inline comment pointing at the tracker issue so the
  reason is auditable from the workflow itself.

### Known issue (deferred to upstream)

- **#91 rustls-webpki 0.101.7 three CVEs via aws-sdk-* TLS stack** —
  RUSTSEC-2026-0098/0099/0104. The dep chain `aws-sdk-* →
  aws-smithy-http-client → hyper-rustls 0.24 → rustls 0.21 →
  rustls-webpki 0.101` is pinned by the upstream AWS SDK; resolving
  the CVE requires rustls 0.23+ (webpki 0.103+), which is API-
  incompatible with rustls 0.21. Three options remain on the table:
  (a) wait for `aws-sdk-rust` to migrate (tracking in the AWS SDK
  GitHub repo), (b) fork `aws-smithy-http-client` (large maintenance
  surface), (c) switch the AWS SDK to `native-tls` (changes deployment
  cert-handling story). v0.8.8 ships with the audit ignore + the
  trade-off documented in #91; not exploitable without an
  attacker-controlled CA chain that abuses the specific URI / wildcard
  / CRL parser paths the CVEs cover.

### Test posture

622 workspace tests pass (unchanged from v0.8.7) / 52 ignored. New
`security-audit` CI job green with documented `--ignore` set.

## [0.8.7] — 2026-05-14

Codex (gpt-5.5) third-party review of v0.8.6 caught **3 findings** —
1 HIGH (nvCOMP still had the same alloc-before-validate shape #89
fixed in the CPU codecs) + 2 LOW (regression test gap on the WASM
`*_blocking` path). Same-day fix-and-ship pattern.

### Fixed

- **HIGH (Codex review): nvcomp.rs alloc-before-validate residual** —
  all three `Vec::with_capacity(expected_orig_size)` sites in
  `NvcompZstdCodec::decompress` / `NvcompBitcompCodec::decompress` /
  `NvcompGDeflateCodec::decompress` now route through
  `expected_orig_size.min(DECOMPRESS_BOOTSTRAP_CAPACITY)` (the same
  1 MiB cap v0.8.6 #89 applied to CPU codecs). Verified safe via the
  `ferro_compress::Codec::decompress` impl: nvCOMP HLIF
  (`nvcomp_hlif.rs:703`) calls `output.resize(decomp_bytes, 0)` to
  size the buffer itself based on the parsed compressed-frame header,
  so the call-site `with_capacity` is a sizing hint that doesn't
  constrain the final length.
- **LOW × 2 (Codex review): `decompress_blocking` regression coverage**
  — v0.8.6 added `issue_89_*` regression tests only on the async
  `Codec::decompress` path. WASM clients (`s4-codec-wasm`) hit the
  sync `decompress_blocking` path because the browser has no tokio
  runtime, so the async-path coverage left the WASM surface untested
  for the same alloc-before-validate shape. Added blocking variants
  per codec: `issue_89_blocking_rejects_manifest_over_5gib` +
  `issue_89_blocking_bootstrap_cap_keeps_4gib_claim_alloc_safe`.

### Process note

`codex review --commit <SHA>` doesn't accept a custom prompt
(`error: '--commit' cannot be used with '[PROMPT]'`). Workaround:
`git show <SHA> -- <code-files> | codex exec "<prompt>"`. This both
focuses the review on real code (excluding the 326 binary corpus
files from the fuzz farm commit) and lets the prompt steer
severity-tagged output. ~26k input tokens for a 3-finding response.

### Test posture

622 workspace tests pass (was 618, +4 blocking regressions) / 52
ignored. CI green.

## [0.8.6] — 2026-05-14

Continuous fuzz farm caught **#89** — `CpuZstd::decompress` would
`Vec::with_capacity(manifest.original_size as usize)` _before_ checking
the manifest, so a forged manifest with `original_size = u32::MAX`
drove a 4 GiB pre-allocation and OOM'd the process inside the
libfuzzer 2 GiB RSS cap (within seconds of the farm starting).
Identical shape exists in `CpuGzip::decompress` and both
`*_blocking` siblings (used by the WASM crate).

This is the same class of bug as v0.8.5 #83 (`nvcomp.rs` H-3): the
GPU codecs were already guarded, the CPU codecs were not. The audit
rounds missed it because `nvcomp.rs` had its own private
`MAX_DECOMPRESSED_BYTES` + `validate_decompress_manifest` helper
under `#[cfg(any(feature = "nvcomp-gpu", test))]`, so a global grep
for the helper name only ever surfaced the GPU sites.

### Fixed

- **#89 alloc-before-validate in CpuZstd / CpuGzip** —
  - **Promoted `MAX_DECOMPRESSED_BYTES` (5 GiB, AWS S3 single-PUT max)
    + `validate_decompress_manifest` from `s4_codec::nvcomp::*` to
    crate root `s4_codec::*`** so CPU codecs share the exact same
    pre-allocation guard. Re-exported under the historical names
    inside `nvcomp.rs` so any downstream that imported
    `s4_codec::nvcomp::MAX_DECOMPRESSED_BYTES` keeps compiling.
  - **New `DECOMPRESS_BOOTSTRAP_CAPACITY = 1 MiB` constant** caps the
    *initial* `Vec::with_capacity` even when the manifest claims a
    legitimate-but-large output (≤5 GiB, e.g. 4 GiB). Without this
    the validate guard alone wasn't sufficient — `with_capacity(4 GiB)`
    still drove address-space pressure / RSS-OOM under the libfuzzer
    cap. `read_to_end` (already bounded by the existing decompression-
    bomb `take(limit)`) grows the buffer as actual bytes arrive.
  - Both async and `_blocking` paths in `cpu_zstd.rs` + `cpu_gzip.rs`
    now route through `validate_decompress_manifest` and the
    bootstrap-capped `with_capacity`.
  - 4 new regression tests pin both shapes:
    `issue_89_rejects_manifest_over_5gib` (ceiling reject) +
    `issue_89_bootstrap_cap_keeps_4gib_claim_alloc_safe` (sub-ceiling
    forged claim handled cleanly), per codec.
  - `Passthrough::decompress` already does no `original_size`-based
    allocation (it just verifies CRC against the input bytes), so
    nothing to change there.

### Continuous fuzz farm (out-of-tree, /home/y1/fuzz-runner/)

- Re-enabled `s4-fuzz@s4-codec-cpu_zstd_decompress_bolero.service`
  after #89 fix landed; the same OOM input no longer trips the 2 GiB
  libfuzzer cap.

### Test posture

618 workspace tests pass (was 614) / 52 ignored. CI green.

## [0.8.5] — 2026-05-14

Deep-audit 第三弾 sweep: **3 CRITICAL + 9 HIGH + 5 MEDIUM + 1 LOW**
findings closed across **7 issues** (#81–#87). Fresh audit angles
(HTTP wire / GPU codec safety / WASM+Python bindings / background
task lifecycle) found 18 new issues missing from the first two
audit rounds. Same posture: deploy if you run S4 in production.

### Fixed (operational resilience)

- **SIGTERM handler + background task cancellation + dispatcher
  panic supervision** (#81 / C-1 + H-7) — Kubernetes pod stop
  (SIGTERM) used to bypass graceful shutdown completely; only SIGINT
  was wired. Pods waited the full grace period and then SIGKILL,
  losing every in-flight upload mid-write. Now `SignalKind::terminate`
  feeds the same shutdown notify as Ctrl-C; six background tasks
  (lifecycle / inventory / multipart-sweep / replication-status-sweep
  / access-log flusher / TLS reload) listen for the notify and exit
  cleanly. Per-PUT replication + per-event notification dispatcher
  spawns now wrap their futures in `futures::FutureExt::catch_unwind`
  so a single panic doesn't silently kill the whole feature; new
  metric `s4_dispatcher_panics_total{kind}`.

### Fixed (binding correctness)

- **Binding version inheritance + WASM panic hook** (#82 / C-2 + C-3)
  — both `s4-codec-py` and `s4-codec-wasm` were `version = "0.1.0"`
  hardcoded while the workspace was already at v0.8.x; PyPI / npm
  publishes were going to ship a misleadingly-pre-release bundle.
  Both now inherit `version.workspace = true`. `s4-codec-wasm` also
  installs `console_error_panic_hook` automatically via
  `#[wasm_bindgen(start)]` so a codec panic surfaces as a
  `console.error` instead of poisoning the WASM linear memory and
  silently killing the JS context.

### Fixed (data integrity)

- **GPU codec safety: u32 offsets / memory budget / nvCOMP manifest
  validate** (#83 / H-1 + H-2 + H-3) — GPU `select_csv` accepted CSV
  bodies up to 12 GiB but stored absolute byte offsets in `u32`,
  silently truncating any column past the 4 GiB boundary; the kernel
  then dereferenced `csv[start + i]` against unrelated bytes,
  producing wrong WHERE-filter results. Now capped at `u32::MAX`
  (4 GiB) with `GpuSelectError::BodyTooLarge`. The memory budget
  check also now accounts for row-index allocations + host clones,
  so small-row × billions-of-rows inputs fall back to CPU instead of
  OOM-ing. nvCOMP decompress validates `manifest.original_size` /
  `compressed_size` against a 5 GiB ceiling (AWS S3 single-PUT max)
  before allocating, and uses `usize::try_from` instead of `as` to
  catch `u64`-truncation on 32-bit targets.

### Fixed (security)

- **HTTP wire hardening** (#84 / H-4 + H-5 + H-6) — three issues:
  - **Duplicate signed-header reject (SigV4a)**: a client sending
    two `x-amz-date` headers used to make the signature canonical
    bytes and the downstream input parser see different values
    (auth confusion). Now `routing.rs` rejects with
    `SigV4aError::DuplicateSignedHeader`.
  - **Slowloris guard**: per-connection `tokio::time::timeout`
    (default 30s, `--read-timeout-seconds`) and connection cap via
    `Arc<Semaphore>` (default 1024, `--max-concurrent-connections`).
  - **Hyper limits explicit**: `--max-header-bytes` (default 64 KiB),
    `--http2` (default off — S3 API is HTTP/1.1 in practice and h2
    has its own DoS surface).

### Fixed (correctness + ergonomics)

- **Python pytest suite + per-CodecError exception classes** (#85 /
  H-8 + M-5) — `s4-codec-py` had zero test files; the wheel was
  unverified at runtime. New `tests/test_binding.py` with 10 pytest
  functions (zstd / gzip round-trip, gzip stdlib decode compat,
  GIL release threading, tampered-payload error, version regression
  guard). `codec_err_to_py` rewritten as exhaustive match — each
  `CodecError` variant maps to a typed Python exception
  (`S4CrcMismatchError`, `S4SizeMismatchError`, `S4BackendError`,
  `S4IoError`, etc.) so callers can `except S4CrcMismatchError`
  programmatically.
- **Lifecycle MEDIUM** (#86 / M-1 + M-2 + M-3) —
  - **Flusher Notify**: access-log flusher honors the cancellation
    Notify so it drains pending entries on shutdown instead of being
    SIGKILL-aborted mid-write.
  - **Replication semaphore**: bounded `tokio::sync::Semaphore`
    (default 1024, `--replication-max-concurrent`) caps in-flight
    dispatcher tasks. Fixes OOM under high-volume + slow-destination
    workloads.
  - **SIGUSR1 snapshot dump-back**: long-promised hook finally
    landed. `kill -USR1 <pid>` walks all 9 manager `to_json`
    snapshots and atomic-writes them (tmp + rename) to the
    configured `--*-state-file` paths; new metric
    `s4_sigusr1_dump_total{manager,result}`.

### Documentation (#87)

- `s4-codec-wasm/README.md`: documented the 256 MB browser heap
  limit + `Uint8Array.subarray` per-frame workaround; clarified the
  browser-safe codec subset (Passthrough / CpuZstd / CpuGzip only).
- `s4-codec-py/README.md`: added Threading / GIL section (compress
  releases the GIL; safe for asyncio via `asyncio.to_thread`),
  Supported codecs table (CPU default vs `--features nvcomp-gpu`
  opt-in), Publishing status (manual maturin + twine until CI
  automates).

### Test posture

614 workspace tests pass / 52 ignored (Docker-gated). Was 593/52 in
v0.8.4. CI green.

## [0.8.4] — 2026-05-14

Deep-audit 第二弾 sweep: **2 CRITICAL + 8 HIGH + 6 MEDIUM + 1 LOW**
findings closed across **10 issues** (#71–#80) plus an operational fix
for the CI gate that was silently red since v0.7.1. Same posture as
v0.8.2: deploy if you use replication, multipart × SSE, audit logs as
compliance evidence, IAM-style bucket policies, SigV4a, or any of the
manager state-file flags.

### Fixed (data integrity / silent corruption)

- **Multipart Complete: backend body-fetch error must propagate**
  (#71 / C-1) — the SSE re-encrypt branch silently used
  `assembled_body = None` when `backend.get_object` failed, producing
  a 200 OK with **plaintext bytes** persisted on the backend (same
  class as v0.8 BUG-5, different code path). `NoSuchKey` is the only
  failure now treated as benign; everything else returns InternalError
  so the client retries. Abort cleanup order also reversed (audit H-7).
- **upload_part_copy propagates source `version_id`** (#74 / H-3) —
  was silently fetching the latest version when the client requested
  a specific historical version-id (silent wrong content).
- **Streaming GET CRC verify + Range GET sidecar etag binding**
  (#73 / H-1 + H-2 + M2) — CpuZstd streaming GET path now ends with
  a CRC check against the manifest. Range GET sidecars bumped to v2
  with `source_etag` + `source_compressed_size`; mismatch with the
  current object's HEAD falls back to a full GET. Streaming compress
  requires the body to match `Content-Length` and returns 400
  IncompleteBody on truncation.

### Fixed (security)

- **Policy: object/bucket ARN scoping + Principal validation** (#75 /
  H-4 + H-5) — bucket-level ARNs no longer authorise object actions
  (privilege escalation closed). `PrincipalSet::Wildcard` accepts only
  literal `"*"`; unsupported principal types (Service / Federated /
  CanonicalUser), empty AWS lists, and malformed shapes are rejected
  at parse time instead of silently widening to anonymous-everyone.
- **SigV4a: enforce `x-amz-date` freshness + scope shape** (#76 / H-6)
  — captured SigV4a requests can no longer be replayed indefinitely.
  `--sigv4a-skew-tolerance-seconds` (default 900) gates the timestamp;
  `RequestTimeTooSkewed` (403) on out-of-window requests.

### Fixed (operational resilience)

- **Snapshot boot fault isolation** (#72 / C-2) — single corrupted
  state file used to kill the boot. New `state_loader::load_or_fresh`
  centralises the load path: read / parse failure logs WARN, bumps
  `s4_state_file_load_failures_total{manager,reason}`, and returns a
  fresh manager. Operator's snapshot file is left in place on disk for
  inspection. Applied to all 9 state-file flags.
- **RwLock / Mutex poison recovery in 10 managers** (#77 / H-8) — 75
  `.expect("poisoned")` call sites swept into the new
  `lock_recovery::{recover_read,recover_write,recover_mutex}` helpers.
  A panic inside a write-guarded section no longer crashes the next
  `to_json` (e.g., from a SIGUSR1 dump-back hook). New metric
  `s4_lock_poison_recovery_total{lock,kind}` exposes recovery rate.
- **Lifecycle pagination guards** (#78 / M3) — `is_truncated=true` with
  `next_continuation_token=None` (malformed backend response) used to
  loop forever. Both the object-walk and the multipart-uploads walk
  break + WARN now.
- **ACME renewal poll timeout** (#80 / L1) — 60s `tokio::time::timeout`
  on `state.next()` so a hung Let's Encrypt API doesn't kill the
  renewal task silently. New metric label
  `s4_acme_renewal_total{result="timeout"}`.

### Fixed (correctness)

- **Tagging header validation per AWS S3 spec** (#79 / M5) — empty
  key, duplicate keys, key > 128 bytes, value > 256 bytes, and > 10
  tags per object now return `InvalidArgument` instead of being
  silently accepted (or last-wins on duplicates).

### CI

- **CI workflow opens an issue on `main` push failure** — operators
  get an email via the issue notification path so a fmt drift /
  clippy regression / test failure on `main` is loud instead of
  silent. The v0.7.1–v0.8.3 series silently ran with red CI for 20+
  pushes (the release process didn't gate on `cargo fmt --check`);
  fixed by adding the gate to the release routine and the workflow
  itself.
- **Workspace-wide `cargo fmt --all`** sweep applied. CI's
  `cargo fmt --check` gate passes again.

## [0.8.3] — 2026-05-14

Operational hardening + audit MEDIUM-class sweep. Six issues
(#65–#70) closing the remaining v0.8.2-deferred audit findings:
1 CRITICAL (lifecycle ↔ object-lock interaction not enforced),
2 HIGH (replication status leak, inventory KMS mis-classification),
2 MEDIUM (lock state on replicas, lifecycle multipart-abort), 1 doc
sweep.

### Fixed

- **Lifecycle scanner consults Object Lock** (#65 / audit C-2) —
  scanner now HEAD-checks each object's lock state via
  `ObjectLockManager::get(bucket, key)` before delete / metadata-
  rewrite. Locked objects increment `ScanReport.skipped_locked` and
  bump `s4_lifecycle_actions_total{action="skipped_locked"}` so a
  Compliance-locked object that "should" expire is now visible in
  the metric stream instead of failing silently at the backend.
- **Replication: status HashMap growth bounded** (#66 / audit H-5) —
  `ReplicationStatusEntry` gains a `recorded_at: DateTime<Utc>`
  field; new `sweep_stale(now, max_age)` drops terminal-state
  entries (Completed / Failed) older than the threshold. Pending
  entries are never swept (still in-flight). New CLI flag
  `--replication-status-ttl-hours <N>` (default 168 = 7 days, long
  enough for an on-call rotation to investigate failures). Hourly
  background sweep + `s4_replication_status_swept_total` counter.
  Snapshot back-compat via `serde(default)` for the new field.
- **Inventory: SSE-KMS encryption_status classification** (#67 /
  audit H-7) — `encryption_status_from_head` now checks
  `server_side_encryption == "aws:kms"` BEFORE `ssekms_key_id` (the
  HEAD response carries the former, not the latter). SSE-KMS
  objects in the daily inventory CSV are now correctly labelled
  `SSE-KMS` instead of being misclassified as `SSE-S4`.
- **Object Lock state propagated to replicated objects** (#68 /
  audit M-1) — `replicate_object` now carries an
  `Option<ObjectLockState>` parameter; `spawn_replication_if_matched`
  captures the source's lock state and the destination PUT replays
  the WORM mode / retain-until-date / legal-hold via headers. When
  the destination has no `ObjectLockManager`, log WARN once per
  (src, dst) pair + bump `s4_replication_lock_propagation_skipped_total`.
  Closes the "WORM at source, deletable at destination" gap.
- **Lifecycle AbortIncompleteMultipartUpload — actually fires** (#69
  / audit M-2) — `LifecycleAction::AbortMultipartUpload { upload_id }`
  added; new `evaluate_in_flight_multipart` evaluator branch; scanner
  walks `list_multipart_uploads` per bucket and aborts uploads past
  the configured age. Successful abort drops the entry from
  `MultipartStateStore` (immediate `Zeroizing` wipe of any SSE-C key
  bytes still held).

### Documentation (#70)

- `--lifecycle-state-file` docstring rewritten — was still claiming
  "actual ... invocation deferred to v0.7+" even though v0.7 #45 +
  v0.8.3 #65 / #69 shipped the full scanner. Now cites the real
  post-v0.8.3 status (walk + execute + lock-skip + multipart abort,
  with NoncurrentVersionExpiration as the only remaining deferred
  rule shape).
- `notifications::EventType::ObjectRemovedDeleteMarker` doc clarified
  — fires for both Enabled and Suspended versioning state; Suspended
  also physically deletes the prior null version (consumers cannot
  tell from the event type alone).
- README S3 Select GPU caveat (audit M-4): no change — README has no
  S3 Select section, so there's no false claim to caveat.

### Test posture (post-v0.8.3)

- Workspace tests: 537 pass / 47 ignored (Docker-gated). Was 523/43
  in v0.8.2.
- All four 2026-05-14 audit-track findings (Codex CLI crypto + Codex
  multipart-concurrency + cross-feature interaction + docs drift)
  closed across v0.8.2 + v0.8.3. Remaining open INFO-class findings
  carry no security or correctness risk.

## [0.8.2] — 2026-05-14

Security hotfix from a deep four-track audit (Codex CLI on
crypto + concurrency, internal cross-feature interaction matrix,
docs-vs-implementation drift). **Three CRITICAL data-integrity /
silent-corruption fixes plus four HIGH crypto / DoS / leak fixes.**
Any production deployment that uses replication, multipart × SSE-C,
or audit-log-based compliance evidence should upgrade.

### Fixed (data integrity)

- **Replication: generation token + shadow-key destination** (#61) —
  v0.6 #40 stamped status by `(source_bucket, source_key)` only with
  no per-PUT generation. Two PUTs to the same key spawned concurrent
  replication tasks; an older retry could clobber the destination
  with stale bytes after the newer one finished. Source PUT also
  routed via the shadow key on Enabled-versioning buckets but the
  destination wrote under the logical key — destination version
  chains lost the new version. Fix: monotonic `AtomicU64` generation
  per PUT, CAS-style status update, shadow-key destination when source
  is versioned. Snapshot back-compat via `serde(untagged)`.
- **Multipart: SSE-C key consistency on `UploadPart`** (#62 / H-1) —
  v0.8.0 BUG-10 stripped SSE-C headers to stop backend forwarding,
  but never checked the part's key against the
  `CreateMultipartUpload` context's key. A client could send
  part 1 with key-A and part 2 with key-B; both accepted, plaintext
  silently corrupted on GET. Now `parse_customer_key_headers` runs
  on every part and the resulting MD5 is compared to the Create
  context's MD5; mismatch / omission / partial header set all
  return `400 InvalidArgument`.

### Fixed (security / DoS / leak)

- **Audit log: terminal HMAC marker + cross-file authentication**
  (#63 / H-2 + H-3) — v0.5 #31 emitted a hash-chained HMAC per line
  but had no end-of-file marker; an attacker could truncate the
  newest entries without `verify-audit-log` flagging a break. The
  `# prev_file_tail=` cross-file hint was also trusted from the file
  itself, enabling splice / replay. Now: every batch file ends with
  `# eof_hmac=<hex>` (HMAC of the chain state at file close); the
  `Drop` impl flushes a marker on graceful shutdown.
  `verify-audit-log` gains `--require-eof-hmac` (strict mode) and
  `--expected-prev-tail <hex>` (operator-supplied authenticated
  tail). `VerifyReport` adds `unsigned_eof` and `unsigned_prev_tail`
  flags so tooling can flag pre-v0.8.2 logs without failing them.
- **Chunked SSE: pre-validate chunk_size × chunk_count before
  alloc** (#64 / H-4) — `decrypt_chunked_buffered` allocated
  `chunk_size * chunk_count` before validating the body actually
  contained that much ciphertext. A malicious / corrupted S4E5/S4E6
  header could trigger huge allocation or u64 overflow / panic
  before authentication failure was reached. Now uses
  `checked_mul`, caps at a caller-supplied `max_body_bytes` (default
  5 GiB), and rejects with the new `SseError::ChunkFrameTooLarge` /
  `ChunkFrameTruncated` variants. Service.rs API unchanged via
  `decrypt_chunked_buffered_default` wrapper. Includes a fuzz
  regression test (100k random bodies × 5 cap variants — no panic).
- **Multipart: abandoned-upload TTL + SSE-C key zeroize** (#62 /
  H-6) — `MultipartStateStore::by_upload_id` had no TTL or sweep, and
  the SSE-C key bytes were stored as bare `[u8; 32]`. Clients that
  initiated multipart but never completed/aborted left raw 32-byte
  customer keys in process memory indefinitely, leaking on core
  dump / swap-out. Now: `Zeroizing<[u8; 32]>` for the SSE-C key (auto
  -wipes on `remove()` / `sweep_stale()` / process exit); new
  `--multipart-abandoned-ttl-hours` flag (default 24, AWS S3 spec
  value); hourly `tokio::time::interval` sweep task; new metric
  `s4_multipart_abandoned_uploads_total`.

### Notes

- **From v0.8.0 / v0.8.1**: any deployment that uses SSE-C with
  multipart, or relies on replication for cross-bucket DR, should
  upgrade. The audit-log fixes are recommended for compliance
  deployments that treat the log as evidence.
- **Asymmetric versioning** (source Enabled, destination Suspended)
  for replication is documented out-of-scope and emits warnings.

## [0.8.1] — 2026-05-13

Security hotfix surfaced by a post-v0.8.0 audit of v0.4–v0.8 quality
gaps. **Includes a cryptographic security regression fix** (#57) — any
deployment that uses streaming SSE under high PUT volume per key
should upgrade.

### Fixed (security)

- **S4E5 chunked-SSE nonce salt widened 4 B → 8 B (`S4E6` frame)** (#57) —
  v0.8.0 #52's S4E5 frame used a 4-byte per-PUT salt. AES-GCM nonce
  uniqueness is the foundation of authentication; with a 32-bit salt
  the birthday collision is at ~77 k PUTs per key (50 % probability).
  Two AES-GCM messages under the same `(key, nonce)` **leaks the
  authentication key + plaintext XOR** — a categorical IND-CPA /
  IND-CCA break. The new `S4E6` frame uses an 8-byte salt (50 % at
  ~5.06 × 10⁹ PUTs per key — **~65,000× headroom**). Existing S4E5
  objects keep decrypting via back-compat read; new PUTs emit S4E6.
  `chunk_count` now caps at 24 bits = 16,777,215 (×1 MiB chunk_size =
  16 PiB per object — three orders over S3's 5 GiB cap).
- **SSE-KMS DEK plaintext zeroized on drop** (#58) — defense in depth.
  `KmsBackend::generate_dek` / `decrypt_dek` return
  `Zeroizing<Vec<u8>>`; `service.rs` PUT / GET / multipart Complete
  branches hold the stack `[u8; 32]` in `Zeroizing<[u8; 32]>`. Process
  memory dump / swap-out / core dump can no longer leak a previously-
  used DEK after the PUT / GET that used it returns.
- **Multipart Complete atomic per (bucket, key)** (#59) — v0.8.0
  BUG-5 fix routed Complete through "GET assembled body → encrypt →
  PUT back". Two concurrent Completes on the same key (different
  upload-ids) raced: client B could read client A's plaintext between
  A's GET and A's PUT. New per-(bucket, key) `tokio::Mutex` in a
  `DashMap` shard serialises the critical section; lock entries are
  pruned lazily when their `Arc::strong_count` drops to 1.

### CI

- **AwsKms feature: real KMS roundtrip workflow** (#60) — new
  `.github/workflows/aws-kms-e2e.yml` (env-var-gated, no-op-on-missing
  per the v0.7.1 fix pattern) runs `aws_kms_roundtrip` and
  `aws_kms_unwrap_unknown_arn_fails` against a real AWS KMS key on
  schedule + workflow_dispatch + `aws-kms-e2e` PR label. Closes the
  "feature compiled but never validated end-to-end" gap from v0.6 #28.

### Notes

- **Migration from v0.8.0 S4E5 objects**: not required at install
  time. Operators near the 65 k birthday limit on a single key should
  rotate the keyring slot (keep the old key in
  `--sse-s4-key-rotated`) and let lifecycle / replication re-emit
  affected objects as S4E6.
- **SSE-C chunked variant**: still buffered (S4E3 only). Chunked
  SSE-C / SSE-KMS variants deferred to a future release.

## [0.8.0] — 2026-05-13

Performance / GPU pipeline doubling-down — circle back to the original
differentiation. Seven v0.8 milestone issues delivered (#50–#56) plus
**six wire-bug fixes** in the multipart × SSE / versioning / object-lock
/ tagging / replication interactions surfaced by the new E2E suite.

### Wire-level bug fixes (BUG-5..10, surfaced by #54 multipart E2E)

- **BUG-5 (CRITICAL — silent plaintext leak)**: `upload_part` had no
  SSE branch. Multipart × SSE-S4/SSE-C/SSE-KMS used to **store
  plaintext on the backend** even when the gateway was configured for
  encryption. Fixed by routing multipart through a per-upload
  `MultipartUploadContext` (new `multipart_state.rs` module): SSE
  config from `CreateMultipartUpload` is held and applied during
  `CompleteMultipartUpload` (whole-body re-encrypt, single decrypt
  on GET — same on-disk shape as v0.7 SSE).
- **BUG-6**: `complete_multipart_upload` didn't mint a version-id on
  versioned buckets (multipart bypassed the chain). Fixed by
  replicating the `pending_version` branch from `put_object`.
- **BUG-7 (CRITICAL — compliance)**: `complete_multipart_upload`
  didn't call `ObjectLockManager::apply_default_on_put`. **Multipart
  uploads bypassed WORM** — DELETE succeeded on Compliance buckets.
- **BUG-8**: `complete_multipart_upload` didn't call
  `spawn_replication_if_matched` — multipart uploads to a replication
  source never reached the destination.
- **BUG-9**: `create_multipart_upload`'s `Tagging` field was dropped
  on the floor — `TagManager` was never populated, GetObjectTagging
  returned empty.
- **BUG-10**: `create_multipart_upload` forwarded SSE-C / SSE-KMS
  request headers to the backend (same class as v0.7 #48 BUG-2/3,
  unfixed for the multipart entry point). MinIO rejected with
  "HTTPS required" / "KMS not configured". Fixed by `take()`-ing the
  SSE input fields off `req.input` before backend dispatch.

### Added

- **GPU column scan for S3 Select** (#51) — `select_gpu(...)` stub
  from v0.6 #41 replaced with an actual cudarc-backed kernel.
  NVRTC-compiled CUDA C source with two kernels:
  `column_compare_bytes` (Eq / NotEq / LikePrefix) and
  `column_compare_i64` (GT / LT, on-device i64 parse). Bench: 100M-row
  CSV, `WHERE country='Japan'`, GPU 0.94 GiB/s vs CPU 0.56 GiB/s
  (1.60× — the 5× target needs a parallel-scan row-indexing pass on
  GPU as well, deferred).
- **Chunked SSE wire frame S4E5 for streaming GET** (#52) — new
  20-byte header + per-chunk 16-byte AES-GCM tag, default 1 MiB
  chunks. GET emits decrypted chunks via `tokio::Stream`; client sees
  first byte after one chunk's verify (vs full-body buffer
  previously). `--sse-chunk-size 0` keeps the legacy buffered S4E2
  path. SSE-S4 keyring path only — SSE-C / SSE-KMS chunked variants
  deferred. Salt is 4 bytes per PUT (~65k birthday limit per key —
  follow-up tracks 8-byte widening).
- **AES-NI hardware-accelerated SSE measurement** (#50) — runtime
  detect via `is_x86_feature_detected!("aes")` + `pclmulqdq`; new
  metric `s4_sse_aes_backend{kind}` gauge; new bench example
  `bench_sse_throughput`. Measured throughput on Ryzen 9 9950X:
  AES-NI **1661 MB/s** (1 MiB body) vs software fallback **194 MB/s
  (8.7× speedup)**. README "Performance" section updated with the
  full 3-size × 2-op table.
- **GPU pipeline Prometheus metrics** (#55) —
  `s4_gpu_compress_seconds` / `s4_gpu_decompress_seconds` histograms,
  `s4_gpu_throughput_bytes_per_sec` gauge,
  `s4_gpu_in_flight` / `s4_gpu_oom_total`. Surfaced via a new
  callback-style `CodecRegistry::compress_with_telemetry` API that
  keeps the s4-codec crate slim (no `metrics` dep). New
  `docs/observability.md` documents the metric set + a 4-panel
  Grafana layout.
- **GPU auto-detect at boot** (#56) — `nvcomp-gpu` feature build
  + a CUDA device at runtime → sampling dispatcher prefers
  `nvcomp-zstd` over `cpu-zstd` for objects ≥ `--gpu-min-bytes`
  (default 1 MiB; below this the PCIe upload + kernel launch
  overhead exceeds CPU compress time). New trait method
  `CodecDispatcher::pick_with_size_hint(sample, total_size)` —
  default impl delegates to the existing `pick(sample)` so all
  downstream impls keep working.
- **Multipart × SSE / versioning / object-lock / tagging /
  replication E2E** (#54) — new `feature_e2e.rs` Docker-gated tests
  (9 multipart × feature scenarios). All pass after the BUG-5..10
  fixes.

### Changed

- Workspace bumped to 0.8.0.
- `S4Service` now holds an always-on `multipart_state:
  Arc<MultipartStateStore>` for per-upload-id SSE / Tagging / Object
  Lock context.
- `CodecRegistry` exposes `compress_with_telemetry` /
  `decompress_with_telemetry` (additive, non-breaking).

### Performance

- nvCOMP bench refresh on RTX 4070 Ti SUPER + nvCOMP 5.2.0.10 (#53):
  v0.3 → v0.8 same-code gains of +20–36% across codecs (driver +
  hardware only). Headline: **nvcomp-bitcomp gives 11.93× ratio on
  sorted u32 posting lists** (vs cpu-zstd 1.48×). nvcomp-zstd is
  3.3–4.5× faster than cpu-zstd on numeric columns. nvcomp wins on
  every workload's decompress path; cpu-zstd-3 still wins on
  text-heavy compress because the Rust zstd library is highly tuned
  and PCIe round-trip dominates small bodies.

### Notes

- **Single-encrypt over assembled multipart body**: `CompleteMultipartUpload`
  GETs the assembled bytes from the backend, encrypts once, and PUTs
  back. This costs an extra round-trip per Complete but keeps the
  GET decrypt path identical to single-PUT. Per-part encrypt with a
  multi-segment decrypt walker is a follow-up.
- **GPU select 1.60× speedup, not 5×**: the host-side memchr row
  indexing is the shared bottleneck. A future Wave moves indexing
  onto the GPU via parallel scan to unlock the remaining headroom.

## [0.7.1] — 2026-05-13

Operator-UX patch release surfaced by a dogfood walkthrough against
MinIO + aws-cli. No new features, no API changes.

### Fixed

- All nine `--*-state-file` flags (versioning / object-lock /
  mfa-delete / cors / inventory / notifications / tagging /
  replication / lifecycle) now accept an empty file as "start fresh,
  use this path for future snapshot dumps". Previously, `touch
  /tmp/foo.json && --versioning-state-file /tmp/foo.json` failed at
  boot with `EOF while parsing` because `from_json("")` rejected
  empty input — operators had to hand-write a non-trivial empty
  snapshot JSON before the manager would attach. The empty-file
  branch is centralised in a new `read_state_file_or_fresh(path)`
  helper that covers all three "start fresh" cases (empty path,
  missing file, empty / whitespace-only file content).

### Documentation

- `--*-state-file` docstrings updated to drop the misleading "pass
  `--flag ""` (empty path)" hint that never worked under clap's
  value-required parsing. The accurate workflow is `--flag
  /tmp/whatever.json` (file may be missing or empty).

## [0.7.0] — 2026-05-13

E2E hardening sprint — six v0.7 milestone issues delivered (#44–#49).
**No new features.** Theme: finish the half-built v0.6 features
(CORS / Lifecycle / Inventory / SigV4a) and validate v0.4–v0.6 against
a real MinIO backend through aws-sdk-s3. The MinIO E2E suite (#48)
surfaced **four production wire-level bugs** in SSE that
`MemoryBackend` mocks couldn't catch — all fixed in this release.

### Wire-level bug fixes (surfaced by #48 E2E)

- **BUG-1**: `service::put_object` stamped `content_length` from the
  post-compression bytes, but the SSE encryption branch then made the
  body longer (frame header + nonce + tag). Hyper rejected every SSE
  PUT to a real S3 backend with `StreamLengthMismatch`. Fixed by
  re-stamping `content_length` from `body_to_send.len()` after the
  encryption branch decides the final bytes.
- **BUG-2 / 3**: SSE-C / SSE-KMS request headers were forwarded to
  the upstream backend (MemoryBackend ignored them). Real backends
  rejected: MinIO requires HTTPS for SSE-C and refuses SSE-KMS without
  KMS configured. Fixed by `take()`-ing the SSE input fields off
  `req.input` before backend dispatch — S4 owns the
  encrypt-then-store contract.
- **BUG-4**: HEAD didn't echo `x-amz-server-side-encryption` +
  `x-amz-server-side-encryption-aws-kms-key-id` for SSE-KMS / SSE-C
  objects. HEAD has no body so it can't peek the frame magic. Fixed
  by stamping `s4-sse-type` / `s4-sse-kms-key-id` / `s4-sse-c-key-md5`
  metadata at PUT time and reading them on HEAD. SSE-S4 stays
  deliberately unstamped — server-driven transparent encryption
  shouldn't masquerade as SSE-S3.

### Finished

- **CORS OPTIONS preflight wired through hyper listener** (#44) —
  v0.6 #38 landed `S4Service::handle_preflight` but no listener
  routed OPTIONS to it. The `routing.rs` interceptor now intercepts
  before the s3s pipeline; matched preflights return 200 with
  Allow-Origin/Methods/Headers, mismatches return 403, no-CORS
  buckets fall through to s3s.
- **Lifecycle scanner: actual list_objects + execute** (#45) — v0.6
  #37 landed the evaluator + handlers but the scheduler was
  skeleton-only. `lifecycle::run_scan_once(&Arc<S4Service>)` walks
  every bucket with a config, lists each object via
  `list_objects_v2`, executes Expire (delete) / Transition
  (copy_object with new storage class) — Object-Lock-protected
  objects skipped (lock wins). Foundational `SharedService<B>` Arc
  wrapper added so background scanners can call into the service.
- **Inventory scanner: actual bucket walk + CSV emit** (#46) — v0.6
  #36 landed manager + handlers + render helpers but the scheduler
  only marked runs. Now walks buckets via
  `list_objects_v2`, builds `InventoryRow` per object (including
  encryption-status from `s4-encrypted` metadata), writes CSV +
  manifest.json to the configured destination prefix.
- **SigV4a verify gate wired into request flow** (#47) — v0.5 #33
  landed credential store + verifier but no middleware. The
  `routing.rs` middleware now detects
  `Authorization: AWS4-ECDSA-P256-SHA256`, builds canonical-request
  bytes (using the `x-amz-content-sha256` header for the payload
  hash to keep the body stream borrow non-destructive), calls the
  verifier, and returns 403 `SignatureDoesNotMatch` on failure.

### Quality

- **MinIO E2E smoke for v0.4–v0.6 features** (#48) — new
  `tests/feature_e2e.rs` Docker-gated suite, **12 tests** through
  real `aws-sdk-s3` against MinIO covering: SSE-S4 / SSE-C /
  SSE-KMS round-trip, versioning, Object Lock (Compliance +
  Governance + bypass), tagging, replication, CORS preflight, MFA
  Delete, plus the lifecycle / inventory / SigV4a scanners from
  #45/#46/#47. All pass after the four BUG fixes.
- **URL parse hardening** (#49) — five `format!("/{bucket}/{key}").parse::<Uri>().unwrap()`
  call sites in `service.rs` (sidecar key paths) replaced with a
  `safe_object_uri(bucket, key)` helper that percent-encodes via the
  `percent-encoding` crate and returns `S3Error(InvalidObjectName)`
  on failure. New fuzz test runs every byte 0x00–0xFF and 11
  adversarial Unicode codepoints (RTL, NULL, BOM, ZWS, line/paragraph
  separators, U+10FFFF, etc.) through put/get/head/delete handlers
  asserting no panic.

### Changed

- Workspace bumped to 0.7.0.
- `S4Service` now wrapped in `SharedService<B>(Arc<S4Service<B>>)`
  newtype with delegating `impl S3` for all 99 trait methods, so
  background scanners (lifecycle / inventory) can call into the
  service via an `Arc` clone.

### Test posture (post-v0.7)

| | passed | ignored (Docker / AWS-creds) |
|---|---|---|
| Workspace | 461 | 28 |
| MinIO E2E (with Docker) | 12 (feature_e2e) + 3 (minio_e2e) + 5 (http_e2e) + 4 (multipart_e2e) | 0 |
| AWS-creds E2E | 0 | 3 |

## [0.6.0] — 2026-05-13

Eight v0.6 milestone issues delivered (#35–#42). Theme: **ecosystem
completion — drop-in S3 replacement**. With v0.4 (operations) and v0.5
(security) shipped, v0.6 closes the remaining S3 API surface gaps so
existing AWS SDK / s3 client code lights up against an S4 endpoint
without per-feature workarounds.

### Added

- **Bucket Notifications** (#35) — `PutBucketNotificationConfiguration`
  + a fire-and-forget dispatcher. Webhook destinations always available;
  SQS / SNS gated behind `--features aws-events`. AWS-shaped event JSON
  payloads. New CLI `--notifications-state-file <PATH>`. Drop counter
  `s4_notifications_dropped_total{dest}` after the 3-attempt retry
  budget.
- **Inventory CSV daily reports** (#36) — `InventoryManager` per-bucket
  configs + AWS schema-compatible CSV output + manifest.json. Background
  scheduler (`--inventory-scan-interval-hours`, default 1) marks runs;
  test-driven `run_once_for_test` end-to-end emission path. New CLI
  `--inventory-state-file <PATH>`.
- **Lifecycle execution scaffolding** (#37) — `LifecycleManager`
  evaluates Expiration / Transition / NoncurrentVersionExpiration rules
  against (key, age, size, tags). `PutBucketLifecycleConfiguration`
  handlers replaced with manager-aware impls. Background scanner
  skeleton via `--lifecycle-scan-interval-hours` (default 24); actual
  bucket-walk + delete invocation deferred to v0.7. New metric
  `s4_lifecycle_actions_total{bucket,action}`.
- **CORS bucket configuration + preflight match** (#38) — `CorsManager`
  + `PutBucketCors` / `GetBucketCors` / `DeleteBucketCors` handlers +
  `S4Service::handle_preflight(bucket, origin, method, headers)` for
  the listener-side OPTIONS interceptor (full routing wire-up follow-up).
  S3-spec declaration-order first-match-wins evaluation. New CLI
  `--cors-state-file <PATH>`.
- **Object + Bucket Tagging + IAM tag conditions** (#39) — `TagManager`
  per-(bucket, key) and per-bucket tag stores. `Put/Get/DeleteObjectTagging`
  + `Put/Get/DeleteBucketTagging` handlers. PUT honours the
  `x-amz-tagging` header (URL-encoded query). `policy.rs` extended with
  `s3:ExistingObjectTag/<key>` and `s3:RequestObjectTag/<key>` Condition
  keys (fail-closed when tag is absent). New CLI `--tagging-state-file <PATH>`.
- **Cross-bucket Replication** (#40) — `ReplicationManager` per-bucket
  rules with prefix / tag filter and priority. PUT to source bucket
  triggers async tokio::spawn copy to destination bucket; status
  surfaced via `x-amz-replication-status` metadata on HEAD/GET.
  Highest-priority enabled rule wins (S3 spec). 3-attempt retry budget;
  failures bump `s4_replication_dropped_total{bucket}`. New CLI
  `--replication-state-file <PATH>`.
- **S3 Select** (#41) — `SelectObjectContent` handler with a
  sqlparser-backed SQL subset (SELECT cols / `_N` / `*`, WHERE with
  `=`/`<>`/`<`/`>`/`<=`/`>=`/`LIKE`/`AND`/`OR`/`NOT`/`IS NULL`, string
  / int / float / bool / NULL literals, numeric promotion). CSV + JSON
  Lines input; AWS event-stream output (Records / Stats / End frames
  with proper CRC framing). GPU stub (`select_gpu`) wired but inactive
  for v0.7+ acceleration.
- **MFA Delete** (#42) — `MfaDeleteManager` per-bucket MFA Delete
  state + RFC 6238 TOTP verification on DELETE / DELETE-version /
  PutBucketVersioning(MfaDelete=Enabled). Single shared secret with
  per-bucket override; ±1 30-second clock skew tolerance. New CLIs
  `--mfa-delete-state-file <PATH>` and `--mfa-default-secret-file <PATH>`.

### Changed

- Workspace bumped to 0.6.0.
- `S4Service::backend` is now `Arc<B>` (was `B`) so the replication
  dispatcher can spawn destination-bucket PUTs through the same backend.

### Notes

- **Background scanners**: lifecycle (#37) and inventory (#36) ship the
  evaluator + manager + handler set, but the actual periodic
  `list_objects_v2` walk is skeleton-only. Deferred so the per-bucket
  iteration shape (Arc back-ref into `S4Service`) can be designed once
  rather than per-feature.
- **Single-instance scope**: replication (#40), notifications (#35),
  inventory (#36), lifecycle (#37), tagging (#39), CORS (#38), MFA
  Delete (#42) all hold state in-memory per gateway. JSON snapshot
  load/dump APIs are in place; SIGUSR1 dump-back hooks are deferred to
  v0.7+. Multi-instance coordination (true cross-region replication,
  shared secret distribution) is out of scope for v0.6.
- **S3 Select scope cuts**: Parquet / ORC input rejected; aggregates /
  GROUP BY / JOIN / ORDER BY / LIMIT / DISTINCT / function calls return
  `UnsupportedFeature`.

## [0.5.0] — 2026-05-13

Eight v0.5 milestone issues delivered (#27–#34). Theme: **regulated-
industry posture**. Three new on-disk encryption frames (`S4E2` / `S4E3`
/ `S4E4`), full Object Lock (WORM) enforcement, tamper-evident audit
log chain, in-server versioning state machine, SigV4a signature
verification, and a single `--compliance-mode strict` flag that
bundles them into a regulated-industry deploy.

### Added

- **SSE-C — customer-provided keys** (#27) — `x-amz-server-side-encryption-customer-{algorithm,key,key-MD5}`
  request headers. Server encrypts on PUT and decrypts on GET without
  ever persisting the key (only an MD5 fingerprint as metadata, kept
  in the AAD of the new `S4E3` frame so substitution is detected).
- **SSE-KMS envelope encryption** (#28) — `--kms-local-dir <DIR>` opens
  a file-based KEK store (`LocalKms`); `--features aws-kms` adds an
  `AwsKms` backend that calls `GenerateDataKey` / `Decrypt`. Per-object
  DEKs ride in the new `S4E4` frame (key-id + wrapped-DEK in AAD).
  `--kms-default-key-id` mirrors AWS's bucket-default key behaviour.
- **SSE-S4 key rotation** (#29) — new `S4E2` frame stamps a 2-byte
  key-id; `--sse-s4-key-rotated id=N,key=PATH` (repeatable) keeps
  retired keys around for decryption while the active key (`--sse-s4-key`,
  always id=1) handles new writes. v0.4 `S4E1` bodies decrypt unchanged
  via the keyring's tries-each-key fallback.
- **Object Lock (WORM) enforcement** (#30) — `--object-lock-state-file <PATH>`
  attaches an in-memory `ObjectLockManager`. DELETE and overwrite-PUT
  for objects under retention or legal hold are rejected with 403
  AccessDenied. Modes: `GOVERNANCE` (override-able with
  `x-amz-bypass-governance-retention`), `COMPLIANCE` (no override
  until `retain-until` expires). Bucket defaults auto-apply on PUT.
- **Audit log HMAC chain** (#31) — `--audit-log-hmac-key <SPEC>` appends
  a hash-linked HMAC-SHA256 to every access-log line. New
  `s4 verify-audit-log <FILE> --hmac-key <SPEC>` subcommand walks the
  chain and reports the first break. Cross-file rotation links via
  `# prev_file_tail=<hex>` headers so verification spans hourly files.
- **Compliance mode bundle** (#32) — `--compliance-mode strict` refuses
  to start unless TLS, audit-signed access log, SSE, and Object Lock
  are all configured; forces TLS down to 1.3-only; and at runtime
  rejects every PUT that doesn't declare SSE. Sets the gauge
  `s4_compliance_mode_active{mode="strict"}` to 1 for fleet-wide
  alerting.
- **SigV4a signature verification** (#33) — `--sigv4a-credentials <DIR>`
  loads ECDSA P-256 PEM public keys (one `<access_key_id>.pem` per
  caller). Detects `AWS4-ECDSA-P256-SHA256` authorization, verifies
  the canonical request bytes, and checks `X-Amz-Region-Set` membership.
  SigV4 (the existing HMAC-based path) keeps working unchanged.
- **First-class versioning state machine** (#34) —
  `--versioning-state-file <PATH>` attaches an in-memory
  `VersioningManager`. PUTs to Enabled buckets generate UUIDv4
  version-ids and persist bytes under shadow keys
  (`<key>.__s4ver__/<vid>`); GETs route on `?versionId=` query;
  DELETE without version-id creates a delete marker; explicit
  version-id DELETE removes a single revision. ListObjectVersions
  paginates the chain with `IsLatest` flags.

### Changed

- Workspace bumped to 0.5.0.
- `S4Service::with_sse_key` (v0.4) now wraps the key in a 1-slot
  keyring internally so v0.4 deployments transparently ride the new
  S4E2 frame on writes (S4E1 reads still work via the keyring's
  fallback).

### Notes

- **SSE-KMS scope cuts** (deferred): per-object DEK zeroize, request-
  scoped DEK reuse for ≥1200 ops/sec/region (above the AWS KMS rate
  limit), PKCS#11 / HSM backends.
- **SigV4a integration**: the credential store loads at boot, but
  full middleware glue (canonical-request bytes from the s3s framework
  into the verifier) is the follow-up to v0.5 #33. Operators flagging
  this on get a startup-time validation of the credential dir today.
- **Versioning / Object Lock persistence**: in-memory only; JSON
  snapshot is operator-managed via `to_json` / `from_json` (not yet
  wired to a SIGUSR1 dump-back hook). Multi-instance replication is
  deferred to a future milestone.

## [0.4.0] — 2026-05-13

Twelve v0.4 milestone issues delivered (#15–#26). Theme: **production
operations, language reach, security, ecosystem**. The gateway now has
rate limiting, S3-style access logs, server-managed at-rest encryption
and adaptive streaming chunk sizing, plus a CPU-gzip codec that yields
plain `gunzip`-decodable bytes. Reach extends with PyO3 in-process
binding, a wasm32 browser-side decoder, and a Helm chart for K8s
deployments. Operations get a one-command AWS-E2E nightly bootstrap
and storage-class transition guidance.

### Added

- **AWS-E2E one-command bootstrap** (#15) — `scripts/bootstrap-aws-e2e.sh`
  drives `terraform apply` (S3 bucket + IAM role) and pushes the
  outputs into `gh variable set` so the nightly E2E workflow can run
  unattended. No new AWS resources are created if the workflow is
  never enabled — operator opt-in.
- **Adaptive S4F2 chunk sizing** (#16) — `pick_chunk_size(content_length)`
  picks 1 MiB / 4 MiB / 16 MiB tiers (≤1 MiB / ≤100 MiB / >100 MiB).
  Small objects no longer pay the multi-frame overhead; large objects
  parallelise more under streaming pipelining (#12).
- **`list_object_versions` filters S4 sidecars** (#17) — `.s4ix`
  sidecars never surface to clients, matching the existing
  `list_objects_v2` behaviour.
- **Wire-overhead micro-bench for v0.2 framed single-PUT** (#18) —
  `bench_framed_overhead.rs` measures the 28-byte S4F2 header cost:
  23.14 % on 1 KiB, 0.23 % on 100 KiB, 0.03 % on 1 MiB. Documents the
  small-object regime where the overhead is non-trivial and points
  callers at adaptive sizing (#16).
- **Token-bucket rate limiting** (#19) — `--rate-limit` accepts a
  glob-matched ruleset (e.g. `principal=alice,bucket=hot,rps=200,burst=400`)
  with first-match-wins evaluation and per-(rule, principal, bucket)
  limiter cells via `governor` + `dashmap`. New
  `s4_rate_limit_throttled_total{rule,bucket}` Prometheus counter.
- **S3-style access log emission** (#20) — `--access-log local:dir/`
  writes hourly-rotated S3 server-access-log lines (one per
  PUT/GET/HEAD/DELETE) covering remote IP, principal, bucket+key,
  status, bytes, and request-id. `s3://` destinations are explicitly
  rejected with a clear error (deferred to follow-up).
- **SSE-S4 server-managed AES-256-GCM at rest** (#21) — `--sse-s4-key`
  takes a 32-byte key (raw / hex / base64). PUT compresses → encrypts
  with the new `S4E1` wire frame (4-byte magic + 1-byte algo + 12-byte
  nonce + 16-byte tag + ciphertext); GET reverses it. Tampered
  ciphertext fails AES-GCM auth and surfaces as InternalError;
  encrypted GET against a no-key gateway surfaces as InvalidRequest.
  Scope cuts (deferred): SSE-C, per-object KMS, key rotation.
- **Storage-class transition guidance** (#22) —
  `docs/storage-class-transitions.md` documents the sidecar coupling
  rule (object + `.s4ix` must transition together) and gives a Terraform
  lifecycle example matching both prefixes.
- **Python binding via PyO3** (#23) — new `crates/s4-codec-py/`
  exposes `CpuZstd`, `CpuGzip`, and `gpu_available()` to Python
  through `abi3-py39`. `maturin build` ships a single wheel that
  works across CPython 3.9+. Publish flow documented in the crate
  README (PyPI credentials not in CI yet).
- **Browser-side WASM decoder** (#24) — new `crates/s4-codec-wasm/`
  builds to `wasm32-unknown-unknown` and exposes
  `decompressFramed(bytes)` / `decompressSingle(bytes, codec_id)` /
  `supportedCodecs()` via wasm-bindgen. Lets a static-site frontend
  decode S4F2 objects served straight from S3 with no gateway hop.
  npm publish flow documented (npm token not in CI yet).
- **Helm chart MVP** (#25) — `charts/s4/` (Chart.yaml + values.yaml +
  templates/) deploys S4 to Kubernetes with configurable upstream
  endpoint, replicas, ServiceMonitor for Prometheus, and an optional
  `Secret` for the SSE-S4 key. ArtifactHub publish flow documented.
- **`cpu-gzip` codec wire-compatible with stock `gunzip`** (#26) —
  new `CodecKind::CpuGzip` (id `7`) using `flate2` produces plain
  RFC 1952 gzip framing so a downstream consumer can `gunzip < object`
  without S4 in the loop. Decompression-bomb hardening caps output at
  100× input or 4 GiB, whichever is smaller.

### Changed

- Workspace gains `crates/s4-codec-py` and `crates/s4-codec-wasm`
  members.

### Notes

- **PyPI / npm / ArtifactHub publish are out-of-band** for v0.4 — the
  artifacts are buildable from the tagged tree and the publish flows
  are documented per crate, but no credentials live in CI yet.
- **AWS-E2E nightly remains opt-in.** The bootstrap script is a tool;
  no resources get created until an operator runs it on their own
  account.

## [0.3.0] — 2026-05-12

Five v0.3 milestone issues delivered (#10 #11 #12 #13 #14). Theme:
operational polish for the v0.2 surface — TLS cert hot-reload + ACME
auto-cert eliminate the "manage your own PEM" pain, IAM Conditions
unlock real-world AWS bucket policies, GPU streaming pipelining
gives 2.55× CPU compress + 1.4× GPU compress speedup, and the new
benches/comparison/ stack lets anyone reproduce the head-to-head
numbers vs Garage / MinIO.

### Added

- **TLS cert hot-reload on SIGHUP** (#10) — `kill -HUP <pid>` swaps
  the cert/key without dropping in-flight connections; bad reloads
  log WARN and keep the previous config so a deploy mistake never
  causes a listener outage. New `s4_tls_cert_reload_total{result}`
  Prometheus counter.
- **ACME / Let's Encrypt auto-cert** (#11) — `--acme <domain>` plus
  `--acme-contact` / `--acme-cache-dir` / `--acme-staging`. Uses
  TLS-ALPN-01 challenge handled inline on the listening port (no
  separate port-80 listener). Background renewal at the standard
  ~60-day interval; new `s4_acme_renewal_total{result}` +
  `s4_acme_cert_expiry_seconds` metrics.
- **GPU streaming pipelining** (#12) — `streaming_compress_to_frames`
  keeps `DEFAULT_S4F2_INFLIGHT = 3` chunks in flight via
  `futures::stream::FuturesOrdered`, ordering preserved. Bench
  (`bench_pipeline.rs` example): cpu-zstd 0.56 → 1.43 GB/s (**2.55×**),
  nvcomp-zstd 0.56 → 0.78 GB/s (**1.4×**). Memory peak still bounded
  (3 × chunk_size = 12 MiB input buffering vs sequential 4 MiB).
- **IAM Condition support in bucket policy** (#13) —
  `IpAddress` / `NotIpAddress` (CIDR), `StringEquals` / `StringLike` /
  `StringNotEquals` / `StringNotLike`, `DateGreaterThan` /
  `DateLessThan`, `Bool`. Well-known context keys: `aws:SourceIp`
  (from `X-Forwarded-For`), `aws:UserAgent`, `aws:CurrentTime`,
  `aws:SecureTransport`. New `Policy::evaluate_with(...)` API
  taking a `RequestContext`; old `Policy::evaluate(...)` kept for
  back-compat.
- **Compression-ratio comparison bench scaffold** (#14) — new
  `benches/comparison/` with `docker-compose.yml` + `garage.toml` +
  `run.sh` driver. Brings up Garage (zstd L6) + MinIO (server-side
  text compression) + S4 (cpu-zstd) + S4 (nvcomp-zstd) and writes
  `bench-result.csv` with (workload, system, ratio, put/get secs).
  Three workloads (nginx-log, parquet-like, random-bytes); Silesia
  / real Parquet / peak RSS deferred to follow-up issues.
- **Bench example** `bench_codecs.rs` extended with three typed-
  numeric workloads (postings u32, timestamps i64, doc_values i64)
  + `BitcompDataType` made public so callers can target the right
  column shape — full 22-row codec × dataset × ratio table now in
  the README.
- **README cost-savings self-diagnostic** — five-row table from
  $500/mo to $50,000/mo S3 bill plus an honest "if your bill is
  under $1,000/mo, don't bother" note as a counterweight to the
  headline 50–80% pitch.

### Fixed

- `MutexGuard` held across `await` (clippy `await_holding_lock`) in
  two roundtrip tests after the v0.2 #4 framed-format refactor.
- `aws_e2e` cred detection now also accepts a present
  `~/.aws/credentials` or `~/.aws/config` (the `aws configure` /
  `aws sso login` happy path), not just env vars.
- `aws_s3_multipart_roundtrip_compresses_and_unframes` assertion
  now honours the S3 multipart 5 MiB-per-part minimum (was claiming
  >10× compression for what's mathematically capped at 2× on a
  2-part upload).

## [0.2.0] — 2026-05-12

Eight v0.2 milestone issues delivered (#1, #2, #3, #4, #5, #6, #7, #9 —
#8 DietGPU explicitly closed as out-of-scope after honest cost/value
re-assessment). Real-GPU validation done on the dev box (RTX 4070 Ti
SUPER + nvCOMP 5.x) so no "deferred to EC2" caveats remain on shipped
codec features.

### Added — Performance / scale

- **GPU streaming compress** (#1) — per-chunk pipelined `nvcomp-zstd`
  via the unified `streaming_compress_to_frames` path. Bound host-RAM
  peak to `chunk_size + compressed_size` (a 10 GB highly-compressible
  upload now peaks at ~210 MB of host RAM instead of buffering the
  full input).
- **Single-PUT framed format** (#4) — every compressed PUT now goes
  through the same S4F2 multi-frame format multipart uploads use, with
  an optional `<key>.s4index` sidecar for objects that produce more
  than one frame. Range GET on single-PUT objects gets the same
  partial-fetch optimisation multipart already had.
- **Multipart final-part padding trim** (#5) — heuristic-based padding
  skip for likely-final parts (parts with raw user-bytes < 5 MiB).
  Saves up to ~5 MiB per object on highly compressible workloads where
  the final part shrinks far below 5 MiB after compression.

### Added — S3 API completeness

- **Byte-range aware `upload_part_copy`** (#6) — when the source object
  is S4-framed, the user-visible byte range is what gets copied
  (decompressed and re-framed), not raw compressed bytes. Falls back
  to the original passthrough for non-framed sources (cheaper).
- **HTTPS / TLS termination** (#2) — native rustls + ring termination
  via `--tls-cert` / `--tls-key`. ALPN advertises `h2` then
  `http/1.1`, so HTTP/2 is negotiated automatically with capable
  clients. Removes the requirement to front S4 with a reverse proxy
  for HTTPS.

### Added — Production hardening

- **Bucket policy enforcement** (#7) — optional `--policy` flag accepts
  AWS-style bucket policy JSON, evaluated on every PUT/GET/DELETE/List/
  Copy/UploadPartCopy with explicit Deny > explicit Allow > implicit
  Deny. Subset: `Effect`, `Action` (`s3:*` / `s3:GetObject` etc.),
  `Resource` with glob, `Principal` (SigV4 access-key match).
  `s4_policy_denials_total{action,bucket}` Prometheus counter.
- **AWS S3 (real) integration tests in CI** (#3) — Terraform module
  in `infra/aws-e2e/` (test bucket + GitHub OIDC + least-privilege
  IAM role), `.github/workflows/aws-e2e.yml` (nightly + on-demand +
  PR-label-triggered), `tests/aws_e2e.rs` with 3 tests covering
  single-PUT, multipart, and Range GET against real AWS S3. User
  needs to `terraform apply` once and configure 3 GitHub Actions
  variables to activate the workflow.

### Added — Codec ecosystem

- **`nvcomp-gdeflate` codec** (#9) — DEFLATE-family GPU codec via
  nvCOMP's batched GDeflate API. New `CodecKind::NvcompGDeflate`
  (wire id=6, append-only — preserves the existing 0..=5 enum
  stability). Enabled when the `nvcomp-gpu` feature is on and a
  CUDA-capable GPU is detected at runtime.

### Fixed

- `streaming_compress_nvcomp_zstd` was wrongly assuming nvCOMP batched
  output forms a stock zstd stream; in reality nvCOMP wraps each call
  in an internal FCG1 header. The function is removed and all GPU PUTs
  now route through the v0.2 #4 unified S4F2 path which is the actual
  wire format produced. Local-GPU validation surfaced the bug; the
  earlier "deferred to EC2" framing had hidden it.
- `Algo::GDeflate` was missing from `NvcompCodec::with_chunk_size`'s
  algorithm whitelist and from the FCG1 algo_tag dispatch (decompress
  failed with "unknown algo tag: 255").

### Closed without implementation

- **#8 DietGPU codec** — closed without implementation. Implementation
  cost is ~3-4 hours focused work (vendor source + CMake build.rs +
  C++ shim + FFI + GPU validation), and the headline "license clean"
  value is partial since CUDA runtime itself remains NVIDIA proprietary.
  DietGPU upstream is also sparsely maintained (last meaningful activity
  2022-2023). See the issue for the full rationale; reopen if a concrete
  user need surfaces.

## [0.1.0] — 2026-05-12

First public release. Published to crates.io as `s4-server` (binary `s4`),
`s4-codec`, and `s4-config`. Apache-2.0.

### Added (since pre-release)
- ferro-compress source physically integrated into `s4-codec` so
  `cargo install s4-server --features nvcomp-gpu` works without an upstream
  crates.io release of ferro-compress.
- Per-crate metadata (description, keywords, categories, README/LICENSE/NOTICE
  symlinks) so the crates render properly on crates.io and docs.rs.
- Public Docker images (`Dockerfile` CPU + `Dockerfile.gpu`) and
  `docker-compose.yml` / `docker-compose.gpu.yml` quick-start.
- Bilingual README (English + Japanese), CONTRIBUTING, SECURITY,
  CODE_OF_CONDUCT, Issue + PR templates.

### Phase 2.1 (2026-05-12)

#### Added
- **Sidecar frame index** (`<key>.s4index`) for efficient Range GET. Multipart
  objects get an index sidecar at `complete_multipart_upload`; Range requests
  fetch only the needed compressed bytes from the backend.
- **OpenTelemetry traces** via OTLP gRPC exporter (`--otlp-endpoint`).
- **Fuzz infrastructure** (3 layers):
  - 38 proptest properties across `fuzz_parsers.rs`, `fuzz_server.rs`,
    `fuzz_advanced.rs` (mutational + multi-frame sequence + differential)
  - 7 bolero coverage-guided fuzz targets (works with libfuzzer on nightly)
  - 3 fuzz canary tests (proves CI fuzz is alive)
- **CI workflows**: `.github/workflows/ci.yml` (push/PR + 10K-cases proptest
  stress) and `.github/workflows/fuzz-nightly.yml` (1M-cases proptest +
  libfuzzer 30min × 5 targets, auto-opens GitHub issue on failure)
- **Soak harness** (`scripts/soak/run.sh`) — 24h+ sustained PUT/GET load with
  RSS / FD / connection leak detection.
- **Hardened**: `cpu_zstd::decompress` against decompression-bomb manifests
  (`Decoder + take(limit)`).

#### Fixed
- `FrameIter` infinite-loop on 1-byte input (caught by proptest fuzz).

### Phase 2.0 (2026-05-12)

#### Added
- **Range GET** on S4-compressed objects (`bytes=N-M` / `bytes=-N` /
  `bytes=N-`), full read + slice fallback (sidecar optimization in 2.1).
- **Per-frame codec dispatch** in multipart (frame format bumped to `S4F2`,
  28-byte header, codec_id u32 LE).
- **`copy_object` S4-aware**: source `s4-*` metadata force-preserved across
  `MetadataDirective::REPLACE` (silent corruption fix).
- **`/metrics` Prometheus endpoint** with `s4_requests_total`,
  `s4_bytes_in_total`, `s4_bytes_out_total`, `s4_request_latency_seconds`.

### Phase 1 (2026-05-12)

#### Added
- Initial release of S4: GPU-accelerated S3-compatible storage gateway.
- s3s 0.13 framework + `s3s_aws::Proxy` AWS S3 backend forwarding.
- `s4-codec` crate with `Codec` trait + `CodecRegistry` + `CodecDispatcher`
  (`AlwaysDispatcher`, `SamplingDispatcher` with entropy + 14 magic-byte
  detection).
- Codec backends: `Passthrough`, `CpuZstd` (full streaming), `NvcompZstd`
  (GPU, gated by `nvcomp-gpu` feature), `NvcompBitcomp` (GPU, integer columns).
- Multipart per-part compression with `S4F1` (later `S4F2`) frame format and
  `S4P1` padding (S3 5 MiB minimum).
- Streaming I/O: `cpu_zstd_decompress_stream` for GET, `streaming_compress_cpu_zstd`
  for PUT.
- Wire-compatibility fixes: content-length, checksum, etag rewriting on
  PUT/GET to prevent SDK errors.
- 45+ Phase 2 S3 op delegations (ACL, Tagging, Lifecycle, Versioning, etc.).
- `/health` + `/ready` HTTP endpoints (ALB / k8s-friendly).
- Structured JSON logging (`--log-format json`) with per-request metrics.
- Vendored `ferro-compress` (Apache-2.0/MIT) for nvCOMP Rust binding.
- E2E tests against MinIO via testcontainers (CPU + GPU variants).
- HTTP-level E2E tests with real `aws-sdk-s3` client.
