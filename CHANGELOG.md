# Changelog

All notable changes to S4 will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
