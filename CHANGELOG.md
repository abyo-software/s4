# Changelog

All notable changes to S4 will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

S4 is pre-1.0; all 0.0.x changes are tracked here. The first tagged release
will be `0.1.0` once the public API stabilizes.

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
