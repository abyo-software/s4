# Changelog

All notable changes to S4 will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
