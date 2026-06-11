# S4 — Squished S3

[![CI](https://github.com/abyo-software/s4/actions/workflows/ci.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/ci.yml)
[![Nightly Fuzz](https://github.com/abyo-software/s4/actions/workflows/fuzz-nightly.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/fuzz-nightly.yml)
[![AWS E2E](https://github.com/abyo-software/s4/actions/workflows/aws-e2e.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/aws-e2e.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org)

> **Drop-in S3-compatible storage gateway with GPU-accelerated transparent compression.**
> Reduces S3 **storage bytes** 50–80% for compressible payloads (logs, JSON,
> Parquet/ORC) without changing application code. Total bill impact depends on
> workload mix — request cost / egress / GPU compute are unchanged.

**Headline numbers** (RTX 4070 Ti SUPER + Ryzen 9 9950X, single-pass roundtrip
through `s4-codec`, last benchmarked 2026-05-13 on nvCOMP 5.2.0.10 / CUDA
13.2 driver 595.58.03; full table + reproduction recipe below):

| Workload | Best ratio | Best compress throughput | Codec verdict |
|---|---:|---:|---|
| nginx access log (256 MiB)   | **155×** (cpu-zstd-3) | 3.7 GB/s (cpu-zstd-3) | CPU wins — text deduplicates well at low CPU cost |
| Parquet-like mixed (256 MiB) | **2.09×** (nvcomp-bitcomp) | 1.5 GB/s (nvcomp-bitcomp) | GPU wins on Bitcomp for integer/columnar layouts |
| Postings (u32, 64 MiB)       | **11.9×** (nvcomp-bitcomp) | 1.6 GB/s (nvcomp-bitcomp) | GPU wins decisively on monotonic integer columns |
| Already-compressed (64 MiB)  | 1.00× (passthrough)  | 2.2 GB/s (passthrough)| Dispatcher detects + skips — no codec cost |

**Codec selection is not always GPU** (#96 #97). The dispatcher samples
entropy + magic bytes and routes per object:

- text / log → `cpu-zstd-3` (often beats GPU codecs both on ratio AND
  throughput at the input size where everything fits in L3)
- columnar integers (Parquet / postings / time-series) →
  `nvcomp-bitcomp` (GPU's strength on integer/columnar layouts).
  Two modes:
  - explicit: `--codec nvcomp-bitcomp` always picks Bitcomp regardless
    of sample content
  - automatic: `--prefer-columnar-gpu` (opt-in) lets the sampling
    dispatcher detect a u32 / u64 LE integer column via per-stride
    byte-position entropy and route to Bitcomp once the body is
    `>= --gpu-min-bytes`. Default is off so v0.8.11-or-earlier
    deployments are bit-for-bit unchanged
- already-compressed (mp4 / jpeg / parquet-with-zstd-block-codec / `.gz`
  detected by magic byte) → `passthrough` (no harm done)
- non-GPU build OR no GPU at runtime → CPU codecs end-to-end

Observe per-codec request distribution via PromQL
`sum by (codec) (rate(s4_requests_total[5m]))` (the `codec`
label on `s4_requests_total` carries the chosen codec name)
Prometheus counter, or per-PUT in the structured JSON access log
(`{"codec_chosen":"..."}`). GPU is a multiplier on the *integer/columnar*
side of mixed workloads, not a blanket "compress with GPU" claim.

Translated to AWS S3 Standard at $0.023/GB/month: **1 TiB of nginx log
data → ~6.6 GiB stored → $0.15/month vs $23.55/month uncompressed (99%
storage savings, single-pass)**. Mixed-content Parquet workloads see ~50%
storage savings.

**What this number does and doesn't cover** (#95): storage-bytes only.
PUT/GET request cost is unchanged (1 PUT in = 1 PUT out, plus a small
`.s4index` sidecar PUT for indexed range-read). Egress is unchanged
(GET serves the decompressed payload). GPU compute is a separate cost
(c. EC2 g4dn / g5 hourly) — pays for itself on TB-scale, not GB-scale,
ingest. See [Cost savings — does S4 make sense for your bill?](#cost-savings-does-s4-make-sense-for-your-bill) below for the
break-even maths.

---

## What is S4?

S4 (**Squished S3**) is an S3-compatible storage gateway written in Rust that
sits between your applications (boto3 / aws-sdk / aws-cli / Spark / Trino /
DuckDB / anything S3) and your real S3 bucket — and **transparently compresses
each object** with a codec the dispatcher picks per-payload: GPU
(NVIDIA nvCOMP zstd / Bitcomp / GDeflate) for integer/columnar data, CPU
zstd / gzip for text/log, passthrough (no codec cost) for already-compressed
inputs. See [the codec verdict table](#headline-numbers) above for the routing rules.

```
                        endpoint: s4.example.com
   your application ──────────────────────────▶  S4 (this project)
   (boto3, Spark,                                       │
    Trino, ...)                                         ▼
                                            (compress with GPU)
                                                        │
                                                        ▼
                                                 AWS S3 (real bucket)
```

- **No app changes**: same S3 wire protocol, same SigV4 auth, same SDK calls
- **Transparent**: PUT compresses, GET decompresses; clients see the original bytes
- **Open format, no lock-in**: stop the gateway and the **compressed
  objects + S4IX sidecars remain S3-native** — readable by stock `aws-cli`
  / boto3 / any S3 client. The **original payload** then requires
  `s4-codec` (CLI tool), `s4-codec-py` (pip), or `s4-codec-wasm` (browser)
  to decompress — all Apache-2.0, ~1k LOC of pure decode, no gateway runtime
  needed. The wire format (S4F2 frame + S4IX sidecar) is documented in
  the source: [`crates/s4-codec/src/multipart.rs`](crates/s4-codec/src/multipart.rs) (frame layout) and
  [`crates/s4-codec/src/index.rs`](crates/s4-codec/src/index.rs) (sidecar layout)

## Why S4?

| Problem | Solution |
|---|---|
| Your S3 bill grows linearly with data, but most data is ≥3× compressible | S4 compresses on the way in, charging you only for the squished bytes |
| Your apps don't compress data themselves (and you don't want to change them) | S4 is a wire-compatible drop-in — just change `--endpoint-url` |
| Existing object-storage compressors (MinIO S2, Garage zstd) are CPU-only | S4 supports nvCOMP **GPU** codecs — Bitcomp gives 3.6–7.5× on integer columns |
| Analytics workloads need byte-range reads | S4 supports `Range` GET via sidecar frame index (parquet/ORC reader compatible) |

## Stability — v1.0 guarantees

S4 ships under the [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
contract as of **v1.0.0**. That means the items below are stable for the
v1.x line — any incompatible change to them ships under a **v2.0.0**
release with migration guidance, not a v1.x patch.

### What's stable (= v2.0 if broken)

| Surface | Frozen at v1.0 |
|---|---|
| **Wire formats** on the backend | `S4F2` framed body + `S4P1` padding (multipart + single-PUT framed objects); `S4IX` v1 / v2 / v3 sidecar layouts; `S4E1` / `S4E2` / `S4E3` / `S4E4` / `S4E5` / `S4E6` SSE envelopes. A v1.x reader can read any byte stream another v1.x server has written, in either direction. **Cross-major back-compat caveats:** (a) v0.8.x readers handle `S4IX` v1 / v2 but return `UnsupportedVersion(3)` on v3 sidecars (introduced in v0.9 #106 for SSE-S4 chunked / `S4E6` partial-fetch); deployments without an SSE-S4 keyring configured (= `--sse-s4-key*` flags unset) never emit v3 sidecars and are bidirectionally compatible with v0.8.x. The default `--sse-chunk-size` is 1 MiB and IS active whenever SSE-S4 is enabled, so SSE-S4 deployments DO emit v3 by default. (b) `S4E6` was introduced in v0.8.1 (commit `a7333f2`), so any v0.8.1+ reader recognizes it; only the v0.8.0 hot-fix line lacks `S4E6` support and would refuse SSE-S4 chunked objects. (c) v0.8.x server binaries can still read all v1.0-written framed bodies + v1/v2 sidecars + S4E1–S4E5 envelopes — the only cross-major refusals are the two listed above. |
| **`s4` binary subcommands** (CLI surface) | `verify-sidecar`, `repair-sidecar`, `sweep-orphan-sidecars`, `verify-audit-log`, plus the long-running server's documented `--<flag>` set. New flags are additive (default off). |
| **`s4_server::repair::*` public API** | `verify_sidecar`, `repair_sidecar` (and the `_with_keyring` variant), `sweep_orphan_sidecars`. Types: `RepairError`, `SidecarStatus`, `RepairReport`, `OrphanReason`, `OrphanReport`, `SweepReport`, `VerifyReport`, `DeletePolicy`, `RepairSseBinding`. All public enums in this module are `#[non_exhaustive]` — adding a new variant in a minor release is **not** breaking (downstream `match` must use a catch-all arm). Public structs (`RepairReport`, `OrphanReport`, `SweepReport`, `VerifyReport`, `RepairSseBinding`) are NOT `#[non_exhaustive]`; their public field set is frozen as-is, additions to those structs are v2.0 territory. Library consumers can pin `s4-server = "1"` and rebuild against any v1.x without code changes. |
| **`s4_server::service::S4Service` shape** | The `S4Service<B>` struct itself, its `S4Service::new(backend, registry, dispatcher)` constructor (signature: `pub fn new(backend: B, registry: Arc<CodecRegistry>, dispatcher: Arc<dyn CodecDispatcher>) -> Self`), and its builder API are frozen. The builder API is the long-form `S4Service::new(...).with_<knob>(value)...` chain — every `pub fn with_*` currently visible on `S4Service` (e.g. `with_sse_key`, `with_sse_keyring`, `with_sse_chunk_size`, `with_secure_transport`, `with_trust_x_forwarded_for`, `with_max_body_bytes`, `with_sigv4a_gate`, `with_kms_backend`, `with_replication`, `with_replication_max_concurrent`, `with_versioning`, `with_object_lock`, `with_mfa_delete`, `with_cors`, `with_lifecycle`, `with_inventory`, `with_notifications`, `with_tagging`, `with_policy`, `with_access_log`, `with_rate_limits`, `with_compliance_strict`, `with_allow_legacy_reserved_key_reads`) is locked to its current `fn(self, …) -> Self` signature; renames or signature changes ship under v2.0. **Adding** a new `with_<knob>` builder is additive (ships in a minor). The `SharedService` newtype at `s4_server::service_arc::SharedService` (the externally-supported "wrap an `S4Service` for clone-able shared use" path), `SigV4aGate` + `SigV4aGateError`, `resolve_range`, the `DEFAULT_MAX_BODY_BYTES` + `DEFAULT_REPLICATION_MAX_CONCURRENT` constants, and the wrapping pattern (`Arc<S4Service>` is the supported handle shape) are frozen. Implementation internals behind `S4Service` (request routing, multipart state, etc.) remain refactorable as long as the listed surface stays bit-equivalent at the call site. **Important caveat on builder parameter types**: 13 of the listed `with_*` builders take `Arc<ManagerType>` parameters whose `ManagerType` lives in an unfrozen module (e.g. `with_tagging(Arc<tagging::TagManager>)`, `with_inventory(Arc<inventory::InventoryManager>)`, `with_replication(Arc<replication::ReplicationManager>)`, …). The **builder signature** is frozen — calling `with_tagging` with an `Arc<TagManager>` is contract-stable; but the **`TagManager` constructor / fields / behavior** are NOT frozen because `s4_server::tagging` is not in the freeze list (see §"Modules NOT in the freeze list" below). Library consumers who construct + inject these managers should pin a precise `=1.x.y` and treat the manager module surface as a manual-integration step across minors. |
| **`s4_server::sse` public surface** | Types: `SseKey`, `SseKeyring`, `SharedSseKey` (= `Arc<SseKey>`, parameter type of `S4Service::with_sse_key`), `SharedSseKeyring` (= `Arc<SseKeyring>`), `SseError`, `SseSource<'a>`, `S4E6Header<'a>` (return type of `parse_s4e6_header`). Functions: `compute_key_md5`, `encrypt`, `decrypt`, `encrypt_v2`, `parse_s4e6_header`, `peek_magic`. Constants: `SSE_C_ALGORITHM`, `ALGO_AES_256_GCM`, `SSE_MAGIC_V5`, `S4E5_HEADER_BYTES`, `S4E6_HEADER_BYTES`. New SSE envelopes (e.g. provisional `S4E7` chunked-KMS) ship as **additive** symbols and do not break the v1.x contract. |
| **`s4_server::streaming` public surface** | `DEFAULT_S4F2_CHUNK_SIZE` constant, `streaming_compress_to_frames` + `streaming_compress_to_frames_with` functions. These functions accept `s3s::dto::StreamingBlob` parameters; that type is governed by the `s3s 0.13` row (see "HTTP API surface" below). |
| **`s4-codec` codec trait + format constants** | `Codec` trait shape, `CodecKind` enum (all `#[non_exhaustive]`), `CodecError`, `IndexError`, `FrameError`, `GpuSelectError`, `CompareOp`. Constants: `index::{SIDECAR_SUFFIX, MAX_FRAMES, MAX_ETAG_BYTES, ENTRY_BYTES, HEADER_FIXED_V1, HEADER_FIXED_V2, INDEX_VERSION, INDEX_VERSION_V1, INDEX_VERSION_V2}`. Items: `index::{FrameIndex, encode_index, decode_index, FrameIndexEntry, SseChunkBinding, RangePlan, EncryptedRangePlan}` (`FrameIndex` and the latter four are all `pub struct`s; their public field sets + inherent method signatures are frozen at v1.0; field additions / removals / renames are v2.0 territory, same rule as the public structs in `s4_server::repair`), `multipart::FrameHeader` layout. Python (`s4-codec-py`) and WASM (`s4-codec-wasm`) bindings are versioned in lockstep with `s4-codec`; their binding-specific public APIs are frozen. The Python module `s4_codec` exports exactly these names: classes `CpuZstd` + `CpuGzip` (Python-side names per the `#[pyclass(name = "…")]` attributes; the underlying Rust types are `PyCpuZstd` / `PyCpuGzip`), function `gpu_available()`, attribute `__version__`, and the exception classes `S4Error`, `S4CrcMismatchError`, `S4SizeMismatchError`, `S4CodecMismatchError`, `S4UnregisteredCodecError`, `S4ManifestSizeExceedsLimitError`, `S4ManifestSizeMismatchError`, `S4BackendError`, `S4IoError` (full hierarchy in `crates/s4-codec-py/src/lib.rs:52-60`). The WASM module exports exactly these names: `decompressFramed`, `decompressSingle`, `supportedCodecs`, `supportedFrameMagic`. The bindings do NOT re-export the full Rust surface — only the names listed above are part of the v1.0 contract for each binding. |
| **`s4-config`** | The `CompressionMode` enum + `BackendConfig` struct field set + `S4Config` struct field set are frozen (the same `pub use s4_config as config` re-export inside `s4-server` makes these reachable through `s4_server::config::*`). The `S4Config::from_toml` stub is **NOT frozen** — it currently returns `bail!("toml loading not implemented yet")` and the eventual real implementation may change its error / return shape in any v1.x minor. |
| **HTTP API surface** | S3 wire compatibility — the [`s3s 0.13`](https://crates.io/crates/s3s/0.13.0) trait set S4 implements. PUT / GET / Range GET / multipart / SigV4 / SigV4a / `x-amz-checksum-*` / `x-amz-server-side-encryption-*` headers all preserved. **`s3s` is itself pre-1.0**; our v1.x contract is that we will continue to track the `s3s 0.13` trait surface that S4 currently implements, accepting backward-compatible additions in `s3s` minors. A `s3s` major bump (0.14, 1.0) that breaks our trait impls would itself trigger a v2.0 of S4 with a clear migration in `docs/migration/`. |
| **Container image tags + Helm chart `values.yaml` keys** | `ghcr.io/abyo-software/s4:<major>.<minor>.<patch>` + `:<major>.<minor>` + `:latest` floating tag rules; GPU build sibling tags `:<major>.<minor>.<patch>-gpu`. The complete top-level `values.yaml` key set is frozen: `replicas`, `image.{repository, tag, pullPolicy, pullSecrets}`, `nameOverride`, `fullnameOverride`, `serviceAccount.{create, annotations, name}`, `backend.{endpointUrl, region}`, `codec`, `zstdLevel`, `dispatcher`, `logFormat`, `otlpEndpoint`, `gpu.{enabled, count, nodeSelector, runtimeClassName}`, `tls.{enabled, cert, key, existingSecret, certKey, keyKey}`, `policy.{json, existingConfigMap}`, `service.{type, port, annotations}`, `ingress.{enabled, className, annotations, hosts, tls}`, `resources.{requests, limits}`, `podAnnotations`, `podLabels`, `podSecurityContext`, `securityContext`, `nodeSelector`, `tolerations`, `affinity`, `extraEnv`, `extraVolumes`, `extraVolumeMounts`, `probes.{liveness, readiness}`. **Default values** may shift in a minor release (e.g. a probe tuning change to reduce flake); the **key shape** (key names + structure) is v2.0 territory. |

### How to read the freeze table — scope of "frozen"

The freeze table above lists items **by name**. The v1.0 contract is exactly the named items, no more and no less:

- **Items named in the table are frozen.** Their signatures, field sets (for structs), and variant sets (for `#[non_exhaustive]` enums modulo additive variants) are stable across all v1.x releases.
- **Other `pub` items in the same frozen modules are NOT part of the v1.0 contract.** Each frozen module (`s4_server::repair`, `s4_server::sse`, `s4_server::service`, `s4_server::streaming`, `s4_server::service_arc`, `s4_codec::index`, `s4_codec::multipart`, etc.) carries other `pub` symbols (helper functions, internal constants, intermediate types) that exist because Rust visibility allows internal callers + integration tests to reach them. Examples currently present but NOT frozen: `s4_server::repair::parse_bucket_key`, the `DEFAULT_REPAIR_BODY_BYTES_CAP` / `MAX_SIDECAR_BODY_BYTES` / `SSE_S4_REPAIR_MAX_OVERHEAD_BYTES` / `SSE_S4_REPAIR_MAX_CHUNK_SLACK_BYTES` constants in `repair`; the `SSE_MAGIC_V1`…`V6` constants, `CustomerKeyMaterial`, `parse_customer_key_headers`, `encrypt_with_source`, `S4E4Header`, `parse_s4e4_header`, `decrypt_with_kms`, and the various chunked-SSE helpers in `s4_server::sse`; `INDEX_MAGIC`, `SSE_BLOCK_V3`, `INDEX_HEADER_BYTES`, `build_index_from_body`, `sidecar_key`, `is_reserved_sidecar_key`, `FRAME_MAGIC`, `PADDING_MAGIC`, `FRAME_HEADER_BYTES` in `s4_codec`. (This is a representative list, not exhaustive.)
- **If you depend on an unlisted item**, pin a precise `=1.x.y` (not `^1`) and treat each minor bump as a manual integration step. If you'd like an item promoted to the named freeze list, please file an issue with the use case.

Why this scope shape? An exhaustive "freeze every `pub` item" contract would over-promise on transitive internal-helper churn that the binary + tests need to be able to evolve. A "freeze nothing" contract would under-promise on the items library consumers actually integrate against. Naming the items keeps the contract explicit on both ends.

### Modules NOT in the freeze list

`s4-server` ships 34 `pub mod` declarations from `crates/s4-server/src/lib.rs` so the `s4` binary (which is a separate crate) + the integration tests + the example binaries can reach the surface they need. Five modules contribute frozen items above: `repair`, `service`, `sse`, `streaming`, and `service_arc` (the last contributes only `SharedService`; the rest of `service_arc`'s contents are not frozen).

Library consumers MAY `use s4_server::<other_module>::*;` — Rust visibility allows it — but those imports are **not frozen** and may break in any v1.x minor release without notice. The other 30 modules (`access_log`, `acme`, `audit_log`, `blob`, `cors`, `dict`, `estimate`, `inventory`, `kms`, `ledger`, `lifecycle`, `lock_recovery`, `metrics`, `mfa`, `migrate`, `multipart_state`, `notifications`, `object_lock`, `policy`, `rate_limit`, `recompact`, `replication`, `routing`, `select`, `sigv4a`, `state_loader`, `streaming_checksum`, `tagging`, `tls`, `versioning`) exist as `pub mod` for binary-and-tests' needs, not as a published surface.

If you depend on one of these unfrozen modules, pin a precise `=1.x.y` (rather than `^1`) and treat any minor bump as a manual integration step. If you would like an item promoted to the frozen surface, please file an issue with the use case.

### Backend compatibility matrix (CI-verified surface)

[`compat-matrix.yml`](.github/workflows/compat-matrix.yml) runs a 1 PUT + 1
GET + sidecar HEAD round-trip per backend through a live s4-server, on a
weekly schedule and via `workflow_dispatch`. CI-verified backends as of
v1.0:

| Backend | Tier | CI status on this upstream repo |
|---|---|---|
| MinIO | docker | ✓ gating (positive round-trip evidence on every CI run; the only docker-tier backend with a gated round-trip on this repo) |
| AWS S3 | real cloud | ⚠ opt-in. Gates ONLY when `AWS_E2E_BUCKET` + `AWS_E2E_ROLE_ARN` + `AWS_E2E_REGION` are configured on the workflow. This upstream repo has them **NOT configured**, so `aws-e2e.yml` job exits as "skipping" in seconds. A fork that sets them gets a real gate. |
| Backblaze B2 | real cloud | ⚠ opt-in. Gates ONLY when `vars.B2_BUCKET` / `B2_ENDPOINT` / `B2_REGION` + `secrets.B2_KEY_ID` / `B2_APPLICATION_KEY` are configured. Not currently configured on this upstream repo. |
| Cloudflare R2 | real cloud | ⚠ opt-in. Same shape as B2 (`R2_*` vars + `R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY` secrets). Not currently configured. |
| Wasabi | real cloud | ⚠ opt-in. Same shape as B2 (`WASABI_*` vars + `WASABI_ACCESS_KEY_ID` / `WASABI_SECRET_ACCESS_KEY` secrets). Not currently configured. |
| Garage | docker | ⚠ claimed but not currently CI-verified — `dxflrs/garage:v1.1.0` rejects `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` from current `aws-sdk-rust` (worked in v0.x against older garage); the round-trip step is `continue-on-error` until either s4-server pins `UNSIGNED-PAYLOAD` on the relay path or garage v1.2+ ships chunked-signed support. The provisioning steps DO gate (admin-API surface is verified). |
| Ceph RGW | docker, best-effort | ⚠ claimed but not currently CI-verified — `quay.io/ceph/demo:latest-quincy` is unmaintained upstream and drifts on the streaming checksum wire shape (`XAmzContentSHA256Mismatch`). Both the start step AND the round-trip step are `continue-on-error` (the demo image's startup failure has historically been the noisier of the two). Real Ceph clusters operated by users should work because Ceph RGW production releases track the AWS wire spec; we need a maintained demo image or an operator-CI hook to re-introduce gating coverage. |

The compat-matrix job's start-step gates for every docker-tier backend **except Ceph RGW** (whose container image is unmaintained upstream — start failure surfaces as a warning, not a workflow failure). The round-trip step is `continue-on-error` for Garage and Ceph RGW only. The status here is the source of truth — if a backend isn't in this table as `✓ gating`, treat the README's other compat claims as "should work given S3 wire compatibility, not asserted by CI on this repo." The real-cloud rows above are honest about the opt-in nature: a fork that wires up the secrets gets immediate weekly verification; this upstream repo intentionally doesn't carry shared cloud credentials.

### What's not promised (operator-tunable / explicitly opt-in)

- Compression **ratios** + **throughput numbers**: these are workload-dependent and benchmark conditions are published, not promised SLAs.
- Default values for `--max-body-bytes`, `--sse-chunk-size`, `--gpu-min-bytes`, and similar runtime tunables: defaults may shift in a minor release if a clear correctness / safety reason warrants it (the v0.9 #106-32bit fix that clamped to `isize::MAX` on 32-bit is an example of a default the SemVer-stable contract did not protect).
- Implementation details inside frozen modules (private functions, struct field reordering, internal trait impls): the v1.0 freeze pins the *items listed above*, not "every line in `service.rs`". Re-arranging request-routing internals is fine in a minor.
- Backend behavior beyond S3-wire-spec compliance (e.g. how a specific backend handles a particular SigV4 edge case): we test the documented backends (see §"Backend compatibility matrix"), but breakage caused by a backend-side change is not a v2.0 trigger on our end.
- Experimental flags marked `--allow-legacy-*` or surfaced as `unstable` in `--help`: explicitly opt-in to behavior that may change.
- Cross-region replication and the `replication.*` config surface: shipped as **experimental scaffolding** in v0.6 with the wire path stubbed in but no production-grade reconciliation. Excluded from v1.0 freeze; promotion to first-class (with Jepsen-class consistency tests) is on the v1.x roadmap below.
- Security advisories accepted as risk-with-mitigation: see [`docs/security/cargo-audit-ignores.md`](docs/security/cargo-audit-ignores.md) for the 4 currently-ignored RUSTSEC advisories, each with rationale, mitigation, and upstream-tracking links. The ignore list is part of CI (`cargo audit` is a merge-block); changes to the list are visible in the diff.

### v1.x roadmap candidates (= shipping under v1.x without breaking the contract above)

- Chunked SSE-KMS envelope (provisional `S4E7` magic) + chunked SSE-C (`S4E8`) → Range GET partial-fetch fast-path for SSE-KMS / SSE-C, parallel to the v0.9 #106 work that enabled it for SSE-S4 chunked (`S4E6`).
- `S4F3` streaming frame format → enables streaming PUT checksum verify for multipart `upload_part` (= closes the codec-API constraint documented in [`docs/security/streaming-checksum-coverage.md`](docs/security/streaming-checksum-coverage.md)).
- 32-bit `s4-server` runtime promotion from advisory to required CI smoke (currently advisory per v0.11 #A4).
- Per-action SHA pinning on the GHA workflows (supply chain hardening; v0.11 #A5 ended at the floating-major tag policy).
- Cross-region replication promoted from experimental scaffolding to production-grade, with Jepsen-style consistency tests.
- Re-introducing Garage + Ceph as `✓ gating` in the backend compat matrix once the upstream signature-interop drifts are resolved.
- Additional codec backends (Snappy, LZ4 if user demand emerges).

### Stability policy in practice

- **Adding** a new codec / SSE envelope / sidecar version / CLI subcommand / lib function is **additive** = ships in a minor release. The v0.9 `verify-sidecar` subcommand + the v3 sidecar variant + the `S4E6` chunked envelope are examples of minor-release additions.
- **Changing** the wire format of an existing magic (e.g. shrinking `S4F2`'s header) is **breaking** = ships in a major release.
- **Removing** a CLI subcommand or a pub function is **breaking** = ships in a major release after a deprecation cycle.
- **Default value drifts** for runtime tunables — not breaking per the carve-out above, but always called out in CHANGELOG `### Changed`.

The audit trail for what counts as breaking lives in [`CHANGELOG.md`](CHANGELOG.md) per the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format S4 uses end-to-end. Migration recipes for any future v2.0 will live in `docs/migration/<from>-to-<to>.md`; no such file exists today because no breaking change is on the v1.x roadmap.

### v0.x → v1.0 source compatibility note

The v1.0 cut adds `#[non_exhaustive]` to every public enum we consider growable as a forward-compat measure. The annotation is a Rust-source-level forward-compat tool — it works the same way whether the containing module is frozen by name in the table above or not. (AWS-spec-bound enums — e.g. `IncludedVersions`, `LifecycleStatus`, `LockMode`, `Effect`, `ReplicationStatus`, `VersioningState` — are intentionally NOT annotated because their wire-side values are pinned by the AWS S3 spec and we do not expect to grow them; if AWS ever does, that becomes a SemVer-major event for us anyway.) The complete list of annotated enums (34 total across s4-codec + s4-server + s4-config): `CodecKind`, `CodecError`, `IndexError`, `FrameError`, `CompareOp`, `GpuSelectError` (s4-codec); `RepairError`, `SidecarStatus`, `OrphanReason`, `DeletePolicy`, `SseError`, `SseSource<'a>`, `BlobError`, `KmsError`, `SigV4aError`, `SelectError`, `SelectInputFormat`, `SelectOutputFormat`, `CorsValidationError`, `AuditKeyError`, `VerifyError`, `TagError`, `MfaError`, `InventoryFormat`, `RunError`, `EventType`, `Destination`, `MultipartSseMode`, `LifecycleAction`, `PrincipalSet`, `ResourceArn`, `PolicyParseError`, `SigV4aGateError` (s4-server); `CompressionMode` (s4-config). Full diff in commits `ce30dde` + `db06912` + the round-6 wave that closed the s4-config gap. This is a SemVer-compatible change *for the wire format* and *for additive runtime behavior*, but it IS a **source-level break** for downstream code that did exhaustive `match` against these enums without a `_ =>` catch-all arm. The fix on the consumer side is mechanical: add a `_ =>` arm to every affected `match`. We make this an explicit caveat rather than the usual additive-minor treatment because it lands at the v0.x→v1.0 cut; we do not consider this a v2.0 trigger because the alternative (shipping these enums without `#[non_exhaustive]` and locking ourselves into never adding a variant within v1.x) would be the worse contract. From v1.0 onward, adding a variant to any of these enums is purely additive (the non_exhaustive attribute is the contract that makes it so).

## Quick Start

### Install via cargo (Rust devs)

```bash
cargo install s4-server                                  # CPU build
s4 --endpoint-url https://s3.us-east-1.amazonaws.com     # binary is `s4`, not `s4-server`
```

**Caveats** (v0.8.8, #98):
- Requires Rust 1.92+ (`rustup update stable` first).
- The default `cargo install` builds **CPU codecs only**. GPU codecs
  (`nvcomp-zstd` / `Bitcomp` / `GDeflate`) require `cargo install s4-server
  --features nvcomp-gpu`, which needs the CUDA toolchain and `NVCOMP_HOME`
  pointing at an extracted nvCOMP SDK at build time. Without these the build
  fails at link time with an `nvcomp` lib not found error.
- The installed binary is `s4` (not `s4-server`); check with `which s4`.

### 60-second local trial (Docker, CPU-only)

```bash
git clone https://github.com/abyo-software/s4 && cd s4
docker compose up -d                    # MinIO + S4 server on localhost:8014

# Generate a sample object so the cp lines have something to upload.
head -c 100M /dev/urandom | base64 > big.log    # ~135 MiB of text, compresses well

# Use any S3 client. Below uses aws-cli; replace endpoint with anything.
aws --endpoint-url http://localhost:8014 s3 mb s3://demo
aws --endpoint-url http://localhost:8014 s3 cp big.log s3://demo/big.log
aws --endpoint-url http://localhost:8014 s3 cp s3://demo/big.log ./big.log.roundtrip

# Inspect the compressed object directly on MinIO (different endpoint, bypasses S4).
aws --endpoint-url http://localhost:9000 s3 cp s3://demo/big.log ./big.log.compressed
ls -la big.log big.log.compressed big.log.roundtrip
# Expected: big.log == big.log.roundtrip (lossless), big.log.compressed is much smaller.
```

### Try with GPU compression (NVIDIA nvCOMP)

```bash
# Requires NVIDIA Container Toolkit + a CUDA-capable GPU
docker compose -f docker-compose.gpu.yml up -d
aws --endpoint-url http://localhost:8014 s3 cp parquet-file.parq s3://demo/
```

See [docker-compose.gpu.yml](docker-compose.gpu.yml) for details.

### GPU small-PUT batching (`--gpu-batch-small-puts`, opt-in)

Per-object GPU compression below `--gpu-min-bytes` (default 1 MiB) loses to
CPU because each call pays a fixed kernel-launch + PCIe round-trip.
`--gpu-batch-small-puts` (v1.1, off by default; requires the `nvcomp-gpu`
build + a CUDA GPU at boot, refuses to start otherwise) coalesces
**concurrent** small PUTs into a single `nvcompBatchedZstd` kernel launch:

```bash
s4 --endpoint-url ... --gpu-batch-small-puts \
   --gpu-batch-max-items 32 \      # flush at 32 pending bodies (default)
   --gpu-batch-window-ms 4 \       # ...or after 4 ms, whichever first (default)
   --gpu-batch-floor-bytes 4096    # bodies below 4 KiB stay on cpu-zstd (default)
```

Eligibility: dispatcher picked `cpu-zstd`, no `--zstd-dict` match, declared
`Content-Length` in `[--gpu-batch-floor-bytes, --gpu-min-bytes)`. Stored
objects are **standard `nvcomp-zstd` bodies — wire-format identical to the
per-object GPU path**; the GET path has zero batch awareness. Any decline
(queue full, GPU error, batched output not smaller than the input) falls
back to the unchanged cpu-zstd path; watch the split via the
`s4_gpu_batch_total{result="batched"|"fallback"}` counter.

Trade-offs, measured on 1000 × 8 KiB log-like objects (RTX 4070 Ti SUPER +
Ryzen 9 9950X, nvCOMP 5.2.0.10, `cargo bench -p s4-codec --features
nvcomp-gpu --bench gpu_small_batch`, 2026-06-11):

| Path | Wall time | Objects/s | Total compressed |
|---|---:|---:|---:|
| cpu-zstd-3, sequential | 15.7–19.5 ms | ~52–64k | 735,396 B (11.14×) |
| nvcomp-zstd per-object | 702–707 ms | ~1.4k | 665,375 B (12.31×) |
| nvcomp-zstd **batched (32/launch)** | 29.7–29.9 ms | ~33.5k | 665,375 B (12.31×) |

Honest read: batching makes small-object GPU compression **~24× faster
than per-object GPU** and yields ~10% smaller output than cpu-zstd-3, but
a single CPU core still finishes this 8 KiB workload ~1.5–1.9× sooner in
wall time on this hardware. Enable the flag when (a) ingest CPU is the
bottleneck and you want to offload small-object compression to an
otherwise-idle GPU, or (b) the extra compression ratio matters at fleet
scale; skip it if raw single-node PUT latency is what you optimise — each
batched PUT also waits up to `--gpu-batch-window-ms` for its batch to fill.

### Kubernetes (Helm)

Official container images are published to GitHub Container Registry on every
`v*.*.*` release tag — `ghcr.io/abyo-software/s4:<version>` (CPU, multi-arch
amd64 + arm64) and `ghcr.io/abyo-software/s4:<version>-gpu` (nvCOMP GPU build,
amd64). The package is public; no `imagePullSecrets` needed.

```bash
helm install s4 ./charts/s4 \
  --set image.tag=1.0.0 \
  --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com \
  --set backend.region=us-east-1
kubectl port-forward svc/s4 8014:8014
```

(Use `image.tag=1` for the floating major-line tag that auto-rolls forward
across v1.x minors; the per-version, per-minor, and floating-major tag
rules are defined in §Stability.)

For the GPU image, override `image.tag` with the `-gpu` suffix and turn on
GPU scheduling:

```bash
helm install s4 ./charts/s4 \
  --set image.tag=1.0.0-gpu \
  --set codec=nvcomp-zstd \
  --set gpu.enabled=true \
  --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com
```

The chart in [`charts/s4/`](charts/s4/) ships a stateless Deployment + Service
(ClusterIP, port 8014), optional GPU node selector (`gpu.enabled=true` for
nvCOMP), inline or cert-manager TLS, and bucket-policy ConfigMap. See
[charts/s4/README.md](charts/s4/README.md) for the full values table and
[.github/workflows/docker.yml](.github/workflows/docker.yml) for the image
build / publish pipeline.

### Verifying the image / chart locally

The published image + chart pair is exercised in CI on every push that
touches the distribution surface
([.github/workflows/docker-smoke.yml](.github/workflows/docker-smoke.yml) —
v0.10 wave-2 #B2): `helm lint` + `helm template` against `charts/s4`
with a placeholder backend URL (catches values-schema / template
regressions), `docker compose config` against both compose files
(catches reference / image-tag drift), and `docker pull` +
`s4 --help` / `s4 --version` against the latest published ghcr.io tag
(tolerates the not-yet-published case via `continue-on-error`).
Operators can reproduce the same checks locally before deploying:

```bash
# Helm chart sanity (with placeholder so backend.endpointUrl is satisfied)
helm lint ./charts/s4 --set backend.endpointUrl=https://s3.example.com
helm template s4 ./charts/s4 --set backend.endpointUrl=https://s3.example.com \
  | kubectl apply --dry-run=client -f -

# Compose file syntax + image-ref validation
docker compose -f docker-compose.yml config > /dev/null
docker compose -f docker-compose.gpu.yml config > /dev/null

# Image smoke (run this after a release lands on ghcr.io)
docker pull ghcr.io/abyo-software/s4:1.1.0
docker run --rm ghcr.io/abyo-software/s4:1.1.0 --help
docker run --rm ghcr.io/abyo-software/s4:1.1.0 --version
```

### Python (pip)

For ML / ETL pipelines that just want the codec without the gateway:

```python
from s4_codec import CpuZstd, CpuGzip, gpu_available
codec = CpuZstd(level=3)
compressed, original_size, crc = codec.compress(data_bytes)
roundtrip = codec.decompress(compressed, original_size, crc)
```

PyO3 bindings live in [`crates/s4-codec-py/`](crates/s4-codec-py/) — build
with `maturin build --release` (and `--features nvcomp-gpu` for GPU).

### Browser (WASM)

For frontend apps that read S4-compressed objects directly from S3 over a
presigned URL, no S4 server in the read path:

```bash
rustup target add wasm32-unknown-unknown
wasm-pack build --release --target web crates/s4-codec-wasm  # → pkg/
```

The bundle exports `decompressFramed` / `decompressSingle` for the CPU
codec subset (`passthrough`, `cpu-zstd`, `cpu-gzip`). See
[`crates/s4-codec-wasm/README.md`](crates/s4-codec-wasm/README.md) for
the API and a 10-line example.

### Python dataframes (s4fs / fsspec)

For pandas / pyarrow / DuckDB / Polars reading S4 objects **straight off the
backend** — no gateway in the read path. Range reads use the `.s4index`
sidecar to fetch only the overlapping frames; non-S4 objects pass through
byte-for-byte. Read-only by design (writes go through the gateway); GPU
(`nvcomp-*`) frames and SSE-encrypted objects raise `NotImplementedError`
rather than decode wrong (SSE detection is triple-layered: `s4-encrypted`
metadata, sidecar SSE binding, and `S4E1`–`S4E6` magic-byte sniff).

```python
import pandas as pd
opts = {"target_options": {"endpoint_url": "http://backend:9000"}}
df = pd.read_parquet("s4://bucket/data.parquet", storage_options=opts)
```

See [`python/s4fs/README.md`](python/s4fs/README.md) for pyarrow / DuckDB
examples and the supported-codec matrix.

### Build from source

```bash
cargo build --release --workspace                       # CPU-only
NVCOMP_HOME=/path/to/nvcomp cargo build --release --workspace --features s4-server/nvcomp-gpu

target/release/s4 --endpoint-url https://s3.us-east-1.amazonaws.com \
    --host 0.0.0.0 --port 8014 --codec cpu-zstd --log-format json
```

### Supported targets

| Crate                          | 64-bit Linux (`x86_64` / `aarch64`) | 32-bit Linux (`i686`) | Browser (`wasm32-unknown-unknown`) |
|--------------------------------|:-----------------------------------:|:---------------------:|:----------------------------------:|
| `s4-codec` (library)           | ✅ tier 1                           | ✅ compiles + tests   | ✅ via `s4-codec-wasm`             |
| `s4-codec-wasm` (browser)      | n/a                                 | n/a                   | ✅ tier 1                          |
| `s4-config`                    | ✅ tier 1                           | ✅                    | ✅                                 |
| `s4-server` (gateway binary)   | ✅ tier 1                           | ✅ compiles + `--help` / `--version` + advisory PUT/GET round-trip (CI) | ❌ not applicable           |
| `nvcomp-gpu` feature (any crate above) | ✅ x86_64 only (NVIDIA driver) | ❌ (no 32-bit nvCOMP) | ❌                            |

Runtime-tested platform is **`x86_64-unknown-linux-gnu`** and
**`aarch64-unknown-linux-gnu`** (CI matrix). The 32-bit `i686-unknown-linux-gnu`
target builds clean for `s4-codec` / `s4-config` / `s4-server` as of
v0.9 #106 (default-bytes constants are now `target_pointer_width` cfg-gated
so the 5 GiB AWS S3 single-PUT ceiling no longer const-overflows `usize` on
32-bit). v0.10 wave-2 #A4 adds a per-push CI job that (a) executes the
`s4-codec` + `s4-config` test suites under `--target i686-unknown-linux-gnu`
and (b) builds the `s4` binary itself for i686 + invokes
`s4 --help` / `s4 --version` as a runtime smoke. v0.11 #A4 extends the
same job with an **end-to-end PUT/GET round-trip** — the i686 `s4` binary
runs in front of a stock MinIO container and the AWS CLI puts then gets
a small object back through it, byte-equality-checked. The round-trip
step lands in CI as **advisory (`continue-on-error: true`)** so a
first-time 32-bit runtime bug surfaces in the job log without turning
the badge red while a fix lands in a follow-up v0.11.x commit; promotion
to a required gate happens once a stretch of green main pushes is
observed. Operators running on i686 should still treat
`--max-body-bytes` carefully (auto-clamps to `isize::MAX as usize`
≈ 2 GiB on 32-bit — Rust caps any single `Vec` / `Bytes` allocation
at `isize::MAX`, so a higher gateway guard would let oversized requests
panic inside the SSE buffered-decrypt pre-alloc path).

The `wasm32-unknown-unknown` target is the public release channel for the
browser decoder (`s4-codec-wasm`); the criterion regression-tracking suite
and `cargo check --target wasm32-unknown-unknown` keep it green on every CI
push to `main`.

## How it Compares

| Feature | S4 | [MinIO](https://github.com/minio/minio) | [Garage](https://git.deuxfleurs.fr/Deuxfleurs/garage) | Wasabi / B2 | AWS S3 |
|---|---|---|---|---|---|
| Stance | Transparent-compression proxy in front of an existing S3 backend | Standalone S3-compatible storage system | Standalone S3-compatible storage system | Hosted S3-compatible storage | The reference |
| S3 API compatibility | See [matrix below](#s3-api-compatibility-matrix) | Comprehensive | Subset | Comprehensive | Native |
| **GPU compression** | ✅ nvCOMP zstd / Bitcomp / GDeflate | ❌ | ❌ | ❌ | ❌ |
| **CPU compression** | ✅ zstd 1–22 / gzip | ⚠️ S2 only (legacy) | ✅ zstd 1–22 | ❌ | ❌ |
| **Auto codec selection** | ✅ entropy + magic-byte sampling | ❌ | ❌ | — | — |
| **Range GET on compressed** | ✅ via S4IX sidecar (see [matrix](#s3-api-compatibility-matrix) for the range modes supported) | n/a | n/a | ✅ | ✅ |
| **Streaming I/O** | ✅ chunked PUT / GET; GPU per-chunk pipelined ([conditions](#streaming-io)) | ✅ | ✅ | ✅ | ✅ |
| **Native HTTPS / TLS** | ✅ rustls + ring, ALPN h2 | ⚠️ via reverse proxy | ⚠️ via reverse proxy | ✅ | ✅ |
| **Bucket-policy enforcement at gateway** | ✅ AWS-style JSON, Allow / Deny | n/a | n/a | ✅ | ✅ |
| **Acts as gateway to existing S3** | ✅ (the whole point) | ❌ (gateway mode removed upstream) | ❌ | ❌ | n/a |
| **License** | Apache-2.0 | upstream LICENSE: AGPLv3 (+ commercial) | upstream LICENSE: AGPLv3 | proprietary | proprietary |

*(MinIO / Garage license cells link to upstream LICENSE files; project licenses
 can change between releases. Do not treat as legal advice. See #103.)*

### S3 API compatibility matrix

S4 implements the parts of the S3 API needed to act as a transparent
compression proxy in front of an existing bucket. **It is not a complete
S3 implementation** — operations marked "—" return `NotImplemented` and
should not be called against an S4 endpoint. PRs welcome on the matrix
rows you need.

| Surface | Status | Notes |
|---|---|---|
| PUT / GET object | ✅ Full | single-PUT + range-GET (see below) |
| Multipart upload (create / part / complete / abort) | ✅ Full | with per-part framing + final-part padding trim |
| HEAD object | ✅ Full | returns post-compression `Content-Length` (matches what S3 returns; original size in `x-amz-meta-s4-original-size`) |
| Range GET | ✅ S3 spec | `bytes=N-M`, `bytes=-N` (suffix), `bytes=N-` (open-ended); range maps through S4IX sidecar to compressed byte offsets |
| Conditional GET / PUT (`If-Match` / `If-None-Match` / `If-Modified-Since`) | ✅ Full | |
| PutObjectAcl / GetObjectAcl | ✅ canned ACLs only | `private` / `public-read` / `public-read-write` / `authenticated-read` / `aws-exec-read` / `bucket-owner-read` / `bucket-owner-full-control` |
| Bucket versioning | ✅ Full | per-version UUIDv4 ID, delete-marker semantics |
| Object lock (Governance / Compliance) | ✅ Full | per-object retention + legal-hold |
| Bucket lifecycle (`LifecycleConfiguration`) | ✅ Full | Expiration / NoncurrentVersionExpiration / AbortIncompleteMultipartUpload |
| Bucket notifications (Webhook / SQS / SNS) | ✅ Full | SQS/SNS gated behind `aws-events` feature |
| Bucket replication | ⚠ experimental | rule-based, per-PUT dispatcher; ships as **experimental scaffolding** (wire path + config surface only). **Excluded from the v1.0 freeze** — promotion to production-grade is on the v1.x roadmap. |
| Bucket policy | ✅ AWS-style JSON | Allow / Deny, IAM Conditions subset (see #100) |
| Tagging (object / bucket) | ✅ Full | |
| CORS configuration | ✅ Full | |
| Inventory | ✅ Full | CSV / Parquet output |
| MFA Delete | ✅ Full | RFC 6238 TOTP |
| SSE-S3 (server-side, S4-managed keys) | ✅ Full | AES-256-GCM (S4E1/S4E2 wire) |
| SSE-KMS (envelope encryption) | ✅ Full | LocalKms (file-backed KEKs) default; AWS KMS gated behind `aws-kms` feature |
| SSE-C (customer-provided key) | ✅ Full | (S4E3 wire) |
| S3 Select | ✅ subset | CSV input, single-column equality / inequality / GT / LT / LIKE-prefix; falls back to CPU eval where unsupported |
| Presigned URLs | ✅ Full | both PUT and GET |
| SigV4 / SigV4a auth | ✅ Full | SigV4a requires `--sigv4a-credentials <DIR>` |
| Storage class transitions (Standard ↔ IA ↔ Glacier) | ✅ tagging-driven | see [docs/storage-class-transitions.md](docs/storage-class-transitions.md) |
| Cross-region replication via S4 chain | — | use AWS S3 native CRR on the backend |
| RequestPayment / Accelerate / Logging configuration | — | not implemented; report a 501 |

**Range GET caveat** (#99): the S4IX sidecar gives a per-frame index, so
range maps to a contiguous read of the covering frames and a decode that's
sliced at the boundaries the caller asked for. Parquet/ORC readers
(arrow-rs, datafusion, duckdb's parquet reader) that issue suffix-range
GET against the footer work out of the box. Parallel range reads against
overlapping frame extents do extra decode work and are not yet optimized;
see #99 for the parquet/ORC reader cross-validation harness on the
roadmap.

### SDK compatibility matrix

Test status per major S3 client. "Tested" means a green E2E run in CI or
documented manual verification; "Should work" means the wire shape is
satisfied but no explicit test covers it yet; "Known issue" links to the
relevant issue.

| Client | Status | Notes |
|---|---|---|
| `aws-cli` (v2.x) | ✅ Tested | path-style + virtual-hosted URLs, presigned URLs, multipart, range GET |
| `boto3` (Python) | ✅ Tested | via `s4-codec-py` integration tests + `tests/test_binding.py` |
| `aws-sdk-rust` (v1.x) | ✅ Tested | the gateway is built on it; trait-level coverage in `tests/feature_e2e.rs` |
| `aws-sdk-go-v2` | ✅ Should work | wire-level shapes shared with aws-sdk-rust; no explicit smoke test yet |
| `aws-sdk-java-v2` | ✅ Should work | same as Go v2 caveat |
| `MinIO mc` | ✅ Should work | path-style + virtual-hosted both fine; one-off `mc cp` validated manually |
| `rclone` (s3 backend) | ✅ Should work | multipart chunk size driven by client; large objects respect S4 frame budget |
| `s3cmd` | ⚠️ Should work | older client; SigV2 fallback NOT supported (S4 is SigV4 + SigV4a only) |
| Presigned URLs (SigV4) | ✅ Tested | both PUT and GET; query-string signing path covered |
| Conditional GET / PUT | ✅ Tested | `If-Match` / `If-None-Match` / `If-Modified-Since` / `If-Unmodified-Since` |
| `Content-MD5` / `x-amz-content-sha256` | ✅ Tested | both unsigned (`UNSIGNED-PAYLOAD`) and SHA256-hashed payloads |
| `Content-Encoding: gzip` interplay | ⚠️ See note | S4 may double-encode if the client sends `Content-Encoding: gzip` AND S4 also picks `cpu-gzip` — use `--codec cpu-zstd` or set client `Content-Encoding: identity` |

**Endpoint URL style** (#101): S4 accepts both **virtual-hosted-style**
(`https://my-bucket.s4.example.com/key`) and **path-style**
(`https://s4.example.com/my-bucket/key`); the backend ` aws-sdk-s3 `
client uses whatever the operator's `--endpoint-url` configuration
specifies. If your client is fussy about this, set `--path-style` on
the s4 server side or `--force-path-style` on the AWS SDK side.

### Backend compatibility matrix

S4 is a transparent compression proxy in front of an S3-compatible
backend. Each row below is the **verification posture** S4 holds for
that backend — what CI actually exercises, not "should work" claims.
v0.11 #A7 added the weekly
[`compat-matrix.yml`](.github/workflows/compat-matrix.yml) workflow
that drives the docker-tier verifications (and the real-cloud rows
when operators provide credentials).

| Backend | Verification | Notes |
|---|---|---|
| [AWS S3](https://aws.amazon.com/s3/) | ⚠️ Opt-in nightly CI ([`aws-e2e.yml`](.github/workflows/aws-e2e.yml); gates only when `AWS_E2E_*` secrets are configured on the fork; this upstream repo has them unset) | real bucket, OIDC-assumed IAM role; the reference implementation when a fork wires the secrets |
| [MinIO](https://github.com/minio/minio) | ✅ Verified via per-PR CI (`http_e2e` / `multipart_e2e` testcontainers) + weekly compat-matrix | `quay.io/minio/minio:latest` |
| [Garage](https://git.deuxfleurs.fr/Deuxfleurs/garage) | ⚠️ Provisioning verified weekly via compat-matrix CI (docker `dxflrs/garage:v1.1.0`); round-trip is `continue-on-error` due to `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` signature drift between `aws-sdk-rust` and garage v1.1.0 | single-node `replication_mode = "none"`, CLI-provisioned bucket + key. See §Stability compat-matrix row for the full caveat. |
| [Ceph RGW](https://docs.ceph.com/en/latest/radosgw/) | ⚠️ Best-effort weekly compat-matrix CI (`quay.io/ceph/demo:latest-quincy`) | the upstream `ceph/demo` image is no longer actively maintained; **both** the start step and the round-trip step are gated `continue-on-error` so pull / startup / wire-shape drift failures surface as warnings rather than blocking the matrix |
| [Backblaze B2](https://www.backblaze.com/b2/cloud-storage.html) | 🔧 Configurable in operator CI (real backend; requires `vars.B2_BUCKET` / `B2_ENDPOINT` / `B2_REGION` + `secrets.B2_KEY_ID` / `B2_APPLICATION_KEY`) | weekly when configured, silent skip otherwise |
| [Cloudflare R2](https://www.cloudflare.com/products/r2/) | 🔧 Configurable in operator CI (real backend; requires `vars.R2_BUCKET` / `R2_ENDPOINT` / `R2_REGION` + `secrets.R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY`) | weekly when configured, silent skip otherwise |
| [Wasabi](https://wasabi.com/) | 🔧 Configurable in operator CI (real backend; requires `vars.WASABI_BUCKET` / `WASABI_ENDPOINT` / `WASABI_REGION` + `secrets.WASABI_ACCESS_KEY_ID` / `WASABI_SECRET_ACCESS_KEY`) | weekly when configured, silent skip otherwise |

Each compat-matrix job runs a 1 PUT + 1 GET + sidecar HEAD against
the live backend through an `s4 --codec cpu-zstd --dispatcher always`
server — sidecar HEAD on the backend asserts the second backend round-
trip (sidecar PUT) lands the way s4 expects, which is where most
S3-API-shape divergences would surface (PutObject without
`Content-MD5`, aws-chunked encoding, etc.).

## Security & threat model

S4 is a TLS-terminating S3-compatible proxy. The boundaries you should
think about:

- **Authentication scope**: S4 verifies SigV4 / SigV4a on incoming
  requests using credentials operators configure (`--credentials FILE`
  or `--sigv4a-credentials DIR`). The S4 server then turns around and
  speaks to the backend bucket using **its own** AWS credentials
  (`AWS_ACCESS_KEY_ID` etc. from the standard SDK chain). Client
  identity is **not** delegated to the backend; the backend sees S4 as
  one principal regardless of which incoming client made the request.
  If you need per-client backend identity, run one S4 instance per
  client and use distinct backend credentials.
- **TLS termination**: S4 terminates TLS at its own listener
  (`--tls-cert` / `--tls-key`, or ACME via `--acme`). The connection
  to the backend uses the SDK's own TLS (rustls with the system root
  CA store). If your security model requires end-to-end TLS without
  intermediate decryption, S4 is the wrong shape — use a different
  proxy or run S4 colocated with the backend so the second TLS hop
  doesn't leave the same host.
- **Bucket policy enforcement at the S4 layer**: when `--bucket-policy
  FILE` is set, S4 evaluates AWS-style JSON Allow / Deny rules
  **before** forwarding to the backend. The backend's own bucket
  policy still applies on top. Two policies in series; both must
  permit. We do **not** parse every IAM Condition operator — see
  [`crates/s4-server/src/policy.rs`](crates/s4-server/src/policy.rs)
  for the supported subset.
- **Body-size limits / request smuggling**: hyper limits enforced
  (`--max-header-bytes`, default 64 KiB; `--max-concurrent-connections`,
  default 1024; `--read-timeout-seconds`, default 30s — see v0.8.5
  #84). HTTP/2 is **off by default** (`--http2` to opt in); the S3 API
  is HTTP/1.1 in practice and h2 adds DoS surface (stream-multiplexing
  abuse) that doesn't pay off for our workload.
- **Tenant isolation**: S4 is **single-tenant by design** — one S4
  instance per security boundary. We do not enforce cross-bucket
  isolation at the S4 layer beyond what the backend's IAM enforces.
  Multi-tenant deployments should run one S4 instance per tenant with
  separate backend credentials.
- **Non-goals**: S4 is not an IDS / WAF, does not log request bodies
  (only headers + length), does not implement S3's `ObjectACL`
  Grant-by-CanonicalUser semantics beyond canned ACLs, does not
  proxy IAM API calls.

For incident reporting see [SECURITY.md](SECURITY.md).

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                          S4 server                               │
│  ┌──────────────────┐  ┌─────────────────┐  ┌────────────────┐   │
│  │ s3s framework    │→ │ S4Service       │→ │ s3s_aws::Proxy │ → │ → backend (AWS S3 / MinIO)
│  │ (HTTP + SigV4)   │  │ (compress hook) │  │ (aws-sdk-s3)   │   │
│  └──────────────────┘  └────────┬────────┘  └────────────────┘   │
│                                 ▼                                │
│  ┌─────────────────────────────────────────────────────────┐     │
│  │ s4-codec::CodecRegistry  (multi-codec dispatch by id)   │     │
│  │   ├─ Passthrough          (no compression)              │     │
│  │   ├─ CpuZstd              (zstd-rs, streaming)          │     │
│  │   ├─ NvcompZstd           (nvCOMP, GPU, per-chunk)      │     │
│  │   ├─ NvcompBitcomp        (nvCOMP, integer columns)     │     │
│  │   └─ NvcompGDeflate       (nvCOMP, DEFLATE-family GPU)  │     │
│  └─────────────────────────────────────────────────────────┘     │
│  ┌─────────────────────────────────────────────────────────┐     │
│  │ s4-codec::CodecDispatcher                               │     │
│  │   ├─ AlwaysDispatcher                                   │     │
│  │   └─ SamplingDispatcher  (entropy + 12 magic bytes)     │     │
│  └─────────────────────────────────────────────────────────┘     │
└──────────────────────────────────────────────────────────────────┘
        ▲              ▲              ▲                ▲
        │              │              │                │
   /health         /ready         /metrics         OTLP traces
   (probe)        (probe)       (Prometheus)       (Jaeger / X-Ray)
```

## Benchmarks

Single-pass roundtrip through `s4-codec`. Hardware: RTX 4070 Ti SUPER 16 GB
+ nvCOMP 5.2.0.10 + CUDA 13.2 driver 595.58.03 + Ryzen 9 9950X. Throughput
is reported as **uncompressed bytes per second** (the convention nvCOMP /
lz4 / zstd publish). Last benchmarked 2026-05-13 (v0.8 #53,
`crates/s4-codec/examples/bench_codecs.rs`).

![v0.8 perf chart](docs/perf-v0.8.png)

| Workload | Codec | Original | Compressed | Ratio | Compress | Decompress |
|---|---|---:|---:|---:|---:|---:|
| nginx access log (256 MiB) | cpu-zstd-3 | 256 MiB | 1 MiB | **155.01×** | 3.71 GB/s | 3.27 GB/s |
| nginx access log (256 MiB) | nvcomp-zstd | 256 MiB | 2 MiB | 95.60× | 1.70 GB/s | 2.86 GB/s |
| nginx access log (256 MiB) | nvcomp-gdeflate | 256 MiB | 169 MiB | 1.51× | 1.07 GB/s | 2.51 GB/s |
| Parquet-like mixed (256 MiB) | cpu-zstd-3 | 256 MiB | 133 MiB | 1.92× | 0.75 GB/s | 1.89 GB/s |
| Parquet-like mixed (256 MiB) | nvcomp-zstd | 256 MiB | 131 MiB | 1.94× | 1.44 GB/s | 2.62 GB/s |
| Parquet-like mixed (256 MiB) | nvcomp-gdeflate | 256 MiB | 183 MiB | 1.40× | 1.05 GB/s | 2.62 GB/s |
| Parquet-like mixed (256 MiB) | nvcomp-bitcomp | 256 MiB | 122 MiB | **2.09×** | 1.49 GB/s | 1.44 GB/s |
| Postings (u32, 64 MiB) | cpu-zstd-3 | 64 MiB | 43 MiB | 1.48× | 1.22 GB/s | 1.65 GB/s |
| Postings (u32, 64 MiB) | nvcomp-zstd | 64 MiB | 42 MiB | 1.52× | 1.29 GB/s | 2.52 GB/s |
| Postings (u32, 64 MiB) | nvcomp-gdeflate | 64 MiB | 42 MiB | 1.51× | 1.06 GB/s | 2.44 GB/s |
| Postings (u32, 64 MiB) | nvcomp-bitcomp | 64 MiB | 5 MiB | **11.93×** | 1.61 GB/s | 1.50 GB/s |
| Timestamps (i64, 64 MiB) | cpu-zstd-3 | 64 MiB | 24 MiB | 2.63× | 0.35 GB/s | 0.92 GB/s |
| Timestamps (i64, 64 MiB) | nvcomp-zstd | 64 MiB | 24 MiB | 2.61× | 1.14 GB/s | 2.70 GB/s |
| Timestamps (i64, 64 MiB) | nvcomp-gdeflate | 64 MiB | 48 MiB | 1.32× | 0.89 GB/s | 2.26 GB/s |
| Timestamps (i64, 64 MiB) | nvcomp-bitcomp | 64 MiB | 21 MiB | **2.95×** | 1.45 GB/s | 1.39 GB/s |
| doc_values (i64, 64 MiB) | cpu-zstd-3 | 64 MiB | 44 MiB | 1.45× | 0.26 GB/s | 1.01 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-zstd | 64 MiB | 34 MiB | **1.86×** | 1.04 GB/s | 2.59 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-gdeflate | 64 MiB | 48 MiB | 1.33× | 0.96 GB/s | 2.54 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-bitcomp | 64 MiB | 37 MiB | 1.72× | 1.41 GB/s | 1.48 GB/s |
| Already-compressed (64 MiB) | cpu-zstd-3 | 64 MiB | 64 MiB | 1.00× | 2.23 GB/s | 3.15 GB/s |
| Already-compressed (64 MiB) | nvcomp-zstd | 64 MiB | 64 MiB | 1.00× | 0.83 GB/s | 2.37 GB/s |
| Already-compressed (64 MiB) | nvcomp-gdeflate | 64 MiB | 64 MiB | 1.00× | 0.92 GB/s | 2.39 GB/s |

**v0.3 → v0.8 throughput delta** (compress GB/s on the same hardware,
nvCOMP 5.0.x → 5.2.0.10, no source-code changes — pure runtime / driver gains):

| Workload | Codec | v0.3 (2026-04) | v0.8 (2026-05-13) | Delta |
|---|---|---:|---:|---:|
| nginx (256 MiB) | cpu-zstd-3 | 2.72 GB/s | **3.71 GB/s** | +36% |
| nginx (256 MiB) | nvcomp-zstd | 1.27 GB/s | **1.70 GB/s** | +34% |
| parquet (256 MiB) | nvcomp-zstd | 1.06 GB/s | **1.44 GB/s** | +36% |
| parquet (256 MiB) | nvcomp-bitcomp | 1.20 GB/s | **1.49 GB/s** | +24% |
| timestamps (64 MiB) | nvcomp-zstd | 0.95 GB/s | **1.14 GB/s** | +20% |
| timestamps (64 MiB) | nvcomp-bitcomp | 1.20 GB/s | **1.45 GB/s** | +21% |
| doc_values (64 MiB) | nvcomp-zstd | 0.80 GB/s | **1.04 GB/s** | +30% |

**Reading the table:**

- **`cpu-zstd-3`** dominates on text — 155× on nginx logs is hard to beat.
- **`nvcomp-bitcomp`** is the killer for typed numeric columns: 11.93× on
  sorted u32 posting lists (vs ~1.5× for everything else), 2.95× on
  monotonic i64 timestamps. The `data_type` hint is critical (`Char` on
  numeric data degrades to ~1.2×); see [`s4_codec::nvcomp::BitcompDataType`]
  for the typed constructors.
- **`nvcomp-zstd`** is competitive on Parquet-like / mixed workloads and
  frees the CPU for serving requests in parallel.
- **`nvcomp-gdeflate`** sits between zstd and "no compression" — useful
  when you need DEFLATE-format wire compat (in v0.3 the
  [`gunzip`-compatible wrapper](https://github.com/abyo-software/s4/issues/26)
  will make this codec serve `Content-Encoding: gzip` to any HTTP client).
- **Already-compressed inputs** are correctly bypassed at ratio 1.0× by every
  codec — S4 never makes a file *bigger*.

**Throughput note**: nvCOMP runs through the FCG1-framed batched API at
the default 64 KiB chunk size, so per-call overhead dominates the 64 MiB
input cases. Production deployments using larger chunks via
`streaming_compress_to_frames` (v0.2 #1) push GPU compress >5 GB/s on
highly compressible inputs. The full head-to-head bench vs MinIO S2 /
Garage zstd is tracked in
[issue #14](https://github.com/abyo-software/s4/issues/14); the latest CSV
captured on 2026-05-13 lives at
[`benches/comparison/result-2026-05-13.csv`](benches/comparison/result-2026-05-13.csv)
(MinIO + s4-cpu only; Garage's auto-issued keys and the s4-gpu image
require manual setup outside the driver script).

**Multipart streaming note** (v0.2 #1, surfaced again by the v0.8 #53
comparison run): per-part S4F2 framing (4 MiB chunks) means a 64 MiB
nginx-log multipart upload reports ~1.6× ratio at the storage layer
instead of the 155× single-pass ratio above — each chunk is too small
for zstd's longest-match window to amortize across the whole object.
Ratio scales back to single-pass numbers once `cargo install` users
configure larger multipart chunk sizes via the AWS SDK
`multipart_chunksize` knob (S4 itself stays at the 4 MiB default for
Range-GET granularity). The CSV captures end-to-end PUT/GET wall-clock
including framing overhead.

### Performance regression tracking (criterion + GitHub Pages)

The single-pass numbers above are captured manually on the maintainer's
workstation; for **per-commit regression detection** S4 also runs a
criterion bench suite on every push to `main`
([`.github/workflows/bench.yml`](.github/workflows/bench.yml)), stores
the timing history in the `gh-pages` branch via
[`benchmark-action/github-action-benchmark`](https://github.com/benchmark-action/github-action-benchmark),
and comments on a commit when any tracked target gets ≥ 1.1× slower
than its previous best. The targets cover the CPU hot paths every
default-build deployment runs through:

- `crates/s4-codec/benches/codec_roundtrip.rs` — `cpu-zstd` (levels
  1 / 3 / 22) / `cpu-gzip` / `passthrough` compress + decompress at
  1 KiB / 1 MiB / 16 MiB.
- `crates/s4-codec/benches/frame_codec.rs` — `write_frame` and the
  `FrameIter` walker, with the padding-skip branch exercised.
- `crates/s4-codec/benches/index_codec.rs` — S4IX sidecar
  `encode_index` / `decode_index` / `lookup_range` across 128 /
  1024 / 4096 frame counts.

GPU codecs (`nvcomp-*`) are intentionally not in the regression suite
because GitHub-hosted runners have no CUDA-capable GPU; the manual
table above remains the canonical source for those numbers.

The rendered trend chart lives at
`https://abyo-software.github.io/s4/dev/bench/` after the first
successful CI run on `main` initialises the `gh-pages` branch.

### SSE throughput (AES-NI vs software fallback)

S4's server-side encryption (`--sse-s4-key`) goes through the `aes-gcm`
crate, which selects the AES-NI hardware path automatically on x86_64
hosts where the `aes` + `pclmulqdq` CPU features are present. v0.8 #50
adds (a) a boot log line confirming which backend is live, (b) a
`s4_sse_aes_backend{kind="aes-ni"|"neon"|"software"}` Prometheus gauge
stamped at startup, and (c) the `bench_sse_throughput` example below
that measures the resulting encrypt / decrypt throughput.

Numbers below are from the same Ryzen 9 9950X host as the codec table.
Reproduce with `cargo run --release -p s4-server --example
bench_sse_throughput` (AES-NI is the default; force the software
backend with `RUSTFLAGS="--cfg aes_force_soft --cfg
polyval_force_soft"` and a clean target dir).

| Body size | AES-NI Encrypt | AES-NI Decrypt | Software Encrypt | Software Decrypt |
|-----------|---------------:|---------------:|-----------------:|-----------------:|
| 64 KiB    | 1661 MB/s      | 1692 MB/s      | 194 MB/s         | 194 MB/s         |
| 1 MiB     | 1709 MB/s      | 1718 MB/s      | 195 MB/s         | 195 MB/s         |
| 100 MiB   | 956 MB/s       | 925 MB/s       | 181 MB/s         | 180 MB/s         |

AES-NI delivers ~8.7× throughput on 64 KiB / 1 MiB bodies (the regime
that dominates real S3 object traffic). The 100 MiB row's narrower
gap (~5.2×) is the buffer allocator + page-fault floor — `aes-gcm`
uses a single contiguous `Vec` for the ciphertext, so 100 MiB cases
charge a `mmap` per iteration that's not on the AES path. Operators
running on hosts without AES-NI (very old / virtualized x86 or
non-x86 hardware) should expect ~190 MB/s encrypt / decrypt as the
sustained ceiling for SSE-S4 — still ahead of the network for most
deployments, but worth knowing when sizing CPU headroom.

**Detecting which backend is live**: the boot log emits
`S4 AES-NI feature detection ... aes_ni_available=true` (or `false`),
and `curl -s localhost:9100/metrics | grep s4_sse_aes_backend` shows
the gauge with the active `kind` label.

**Reproducing locally** (requires CUDA + nvCOMP):

```bash
NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
  cargo run --release --example bench_codecs \
    -p s4-codec --features nvcomp-gpu

# Streaming pipeline bench (1 GiB highly-compressible, in-flight chunks):
NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
  cargo run --release --example bench_pipeline \
    -p s4-server --features nvcomp-gpu

# Comparison vs MinIO / Garage (Docker required):
docker compose -f benches/comparison/docker-compose.yml up -d
AWS_REQUEST_CHECKSUM_CALCULATION=when_required \
AWS_RESPONSE_CHECKSUM_VALIDATION=when_required \
  ./benches/comparison/run.sh benches/comparison/result-$(date +%F).csv
```

## Cost savings — does S4 make sense for your bill?

S4 is **not** worth deploying for everyone. The economics depend on (a)
your AWS S3 bill, (b) how compressible your data is, (c) the cost of the
EC2 GPU instance running S4. Here's an honest table to self-diagnose:

| Your monthly S3 bill | Likely savings (50–80%) | EC2 GPU cost | Net savings | Verdict |
|---:|---:|---:|---:|---|
| $500   | $250 – $400     | ~$730/mo (g6.xlarge)    | **−$330 to −$480**    | ❌ NOT worth it |
| $1,000 | $500 – $800     | ~$730/mo                | **−$230 to +$70**     | ⚠️ Breakeven; only if you'd use the GPU for other work too |
| $3,000 | $1,500 – $2,400 | ~$730/mo                | **+$770 to +$1,670**  | ✅ Real savings |
| $10,000 | $5,000 – $8,000 | ~$1,860/mo (g6e.xlarge) | **+$3,140 to +$6,140** | ✅✅ Strong ROI |
| $50,000 | $25,000 – $40,000 | ~$1,860/mo            | **+$23,140 to +$38,140** | ✅✅✅ Material savings |

**Notes:**
- "Likely savings 50–80%" is the typical range for log-heavy workloads
  (`cpu-zstd-3` 155×) and Parquet (`nvcomp-zstd` ~2× plus better Range GET
  efficiency). For pure-numeric column-store data with `nvcomp-bitcomp` on
  sorted posting lists, the ratio swings to **>10×** — savings closer to
  90%+.
- EC2 prices are us-east-1 on-demand, May 2026. Spot instances cut these by
  ~70%, breakeven at ~$300/mo S3 bill instead of $1,000.
- S4 itself is open source (Apache-2.0) — the only cost is the EC2 instance
  and your time.
- **If your monthly S3 bill is under $1,000 and you're not already running
  GPUs for other work, don't bother.** Use S4's `cpu-zstd` codec on a small
  CPU instance, or front your bucket with nginx + gzip — both will give
  most of the savings without GPU hardware.

## When NOT to use S4

Honest list of workloads where S4 doesn't pay off:

- **Already-compressed payloads** (mp4, jpeg, gzip-of-anything, parquet
  with column-level codec already on, lz4 / zstd-prepacked archives) —
  S4's dispatcher detects + routes to `passthrough` so there's no harm
  done, but you're paying for the round-trip without getting savings.
- **Small objects** (< 16 KiB) — the S4F2 frame header (28 bytes) +
  S4IX sidecar (32–96 bytes per object) eats the compression ratio
  before you start. Break-even is workload-dependent; rule of thumb
  is **objects > 1 MiB** make the math comfortable, < 16 KiB make it
  negative. The dispatcher does not yet skip-compress small objects
  automatically (#105 follow-up).
- **Metadata-ops dominant workloads** — heavy `ListObjects` / `HeadObject`
  / `CopyObject` against millions of small keys add S4 hop latency
  without touching the codec. S4 is on-path for those, so you pay
  the second TLS hop + s3s framework overhead.
- **Ultra-low-latency tail SLOs** (sub-10ms p99 GET) — S4's streaming
  GET adds decoder warm-up + S4IX sidecar fetch (one extra round-trip
  for the index when not cached). Fine for analytics / archival /
  bulk; not fine for an OLTP-style hot read path.
- **Single-region cold-storage-only** (everything goes straight to
  Glacier) — Glacier already prices low enough that the storage
  savings rarely pay for the compute / operational cost of running S4.
- **Strict regulatory environments without third-party audit on file** —
  v1.0 freezes the wire + API surface, but S4 has no SOC2 / ISO27001 /
  FedRAMP audit trail yet. If your compliance team's bar is "must have
  third-party audit on file", S4 isn't there.
- **As the only copy of irreplaceable data, before a production
  reference is on file** — until at least one public production
  deployment reference lands (we're collecting them under issue label
  `production-reference`), pair S4 with backend-native versioning +
  replication. The v1.0 freeze is a contract on surface stability,
  not a substitute for the operational track record that a reference
  deployment provides.

## Durability, corruption recovery, and the repair tool

### Write protocol
A PUT goes through three S3 calls behind one client-visible request:

1. **PUT `<key>`** — the compressed S4F2-framed body (atomic single-PUT
   for objects under the multipart threshold; otherwise an S3 multipart
   upload with per-part frames).
2. **PUT `<key>.s4index`** — the S4IX sidecar with per-frame offset +
   original-size + crc32c entries.
3. (multipart only) **CompleteMultipartUpload** — finalises the main
   object atomically; the sidecar is written after this completes.

The main object PUT is the **commit point**; the sidecar exists to
optimise Range GET and is treated as recoverable / rebuildable from the
main object (next section).

### Failure modes and what each one looks like

| Failure | Visible symptom | Recovery |
|---|---|---|
| Client disconnects mid-PUT | Backend returns `IncompleteBody` or 5xx, S4 maps to `TruncatedStream` (v0.8.4 #73). Main object NOT created; sidecar NOT created. No partial state. | None needed — retry the PUT |
| Main object PUT succeeds, sidecar PUT fails | GETs work (full object decode, no range optimisation); Range GETs fall back to "read whole object, decode, slice". | `s4 repair-sidecar <bucket>/<key> --endpoint-url <BACKEND>` rebuilds the sidecar by re-scanning frames in the main object |
| Multipart UploadPart succeeds, CompleteMultipartUpload fails | Backend cleans up uncommitted parts on lifecycle-driven `AbortIncompleteMultipartUpload` (S3 default 7 days, or operator policy). | Retry the upload; orphan parts charged but auto-deleted |
| S3 returns a corrupted object body (rare, but happens on hardware faults) | Per-frame `crc32c` mismatch on decode → `CodecError::CrcMismatch` → S4 returns 500 to client with diagnostic. | None within S4 — fix at the backend storage layer; S4 won't return corrupted bytes |
| Sidecar diverges from main object (manual `aws-cli` edit, etc.) | First Range GET that hits the diverged region returns 500 with `IndexFrameMismatch`. | `s4 verify-sidecar <bucket>/<key> --endpoint-url <BACKEND>` flags it; `s4 repair-sidecar` rebuilds |
| Backend object exists, sidecar missing entirely | GETs work; Range GETs degrade to fallback path. | `s4 repair-sidecar <bucket>/<key> --endpoint-url <BACKEND>` |
| Bucket has accumulated orphan `.s4index` from the v0.8.15 H-g window | Storage bill grows but reads still work (orphans never reach the GET path). | `s4 sweep-orphan-sidecars <bucket> --endpoint-url <BACKEND> --delete` (run without `--delete` first to inspect). See `docs/orphan-sidecar-recovery.md`. |

### CRC scope

`crc32c` is computed over the **decompressed original payload** of each
frame and stored in both the frame header and the sidecar entry. This
catches:
- Mid-flight corruption at the backend storage layer
- Codec backend bugs that decode to subtly wrong bytes
- Forged manifest attacks where the attacker replaces the compressed body

It does **not** catch:
- A correctly-encoded malicious payload from a tampered backend (the
  CRC verifies the bytes match what was encoded, not that what was
  encoded was the originally-PUT bytes) — that's what S4's SigV4 auth
  on the PUT side covers
- Lost frames from a truncated multipart that nonetheless committed
  (the per-part Complete API itself is the integrity check there)

### Repair tool status

v0.9 #106 shipped three sidecar-maintenance subcommands on the `s4`
binary. All three point at the **backend** (not the S4 gateway) — the
gateway hides `.s4index` from listings and decompresses bodies on GET,
both of which break this tooling:

```bash
# Read-only check. Exits 0 on Ok / LegacyV1 / MissingHarmless
# (single-frame object, no sidecar by design) / MissingUnknown (body
# exceeds the deep-scan cap, can't classify); exits 1 on
# MissingDivergent / StaleEtag / StaleSize / DecodeError /
# EncryptedSidecarUnsupported (SSE-S4 chunked, see follow-up below).
s4 verify-sidecar bucket/key --endpoint-url https://s3.example.com

# Re-scan the main object and overwrite the sidecar. Default body cap
# is 5 GiB (matches --max-body-bytes); pass --max-body-bytes to raise.
# Does NOT yet support SSE-S4 chunked encrypted objects from the CLI
# (operator needs the SSE keyring; v0.10 roadmap is to plumb
# `--sse-s4-key <path>` through). Until then, re-PUT the object via
# the v0.9+ gateway to regenerate the v3 sidecar.
s4 repair-sidecar bucket/key --endpoint-url https://s3.example.com

# Find dangling `.s4index` whose pair is missing or stale. Dry-run by
# default; --delete actually removes them. The default --delete only
# removes pair-bound orphans (PairedMissing / PairedEtagMismatch /
# PairedSizeMismatch); SidecarUndecodable entries stay until you
# escalate with --delete-undecodable (guards against deleting legacy
# reserved-name user data under --allow-legacy-reserved-key-reads).
s4 sweep-orphan-sidecars bucket --endpoint-url https://s3.example.com [--delete] [--delete-undecodable]
```

The manual fallback (DELETE the sidecar — Range GET drops to the
full-read path) still works for one-offs without the CLI handy. See
`docs/orphan-sidecar-recovery.md` for the v0.8.15 H-g cleanup recipe
using `s4 sweep-orphan-sidecars`.

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
   [`contrib/grafana/s4-savings-dashboard.json`](contrib/grafana/s4-savings-dashboard.json)
   (see [docs/observability.md](docs/observability.md) for the import
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
     - DELETE / overwrite subtraction uses a best-effort HEAD probe of the removed object — a raced probe leaves the counters slightly stale rather than failing the request
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
  write-shaped request** (plus a sidecar HEAD where relevant) — this
  extra backend traffic exists *only* when the flag is set. Objects
  without S4 metadata (written before S4, or backend-direct) subtract
  as `original = stored = size`, i.e. they contribute zero claimed
  savings.
- State-file durability matches the other `--*-state-file` managers
  plus an event-driven flush (atomic tmp+rename on every mutation;
  SIGUSR1 re-dumps it too) — a crash loses at most the in-flight
  event.

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
lifecycle configuration in `docs/storage-class-transitions.md`, with
one S4-specific guarantee a generic lifecycle filter cannot give you:
**the `<key>.s4index` sidecar always accompanies its main object into
the same class** (and a sidecar that drifted in an earlier interrupted
run is realigned), so the pair never splits the way a size- or
suffix-filtered lifecycle rule can. Sidecars are never transitioned on
their own. Skip taxonomy follows the house style:
`already-target-class` (the idempotency core), `too-recent`,
`etag-raced` (pre-copy HEAD guard), `too-large` (single `CopyObject`
caps at 5 GiB).

Example run against MinIO (the `maintain_minio` e2e seed shape: two
plain text logs under `app/`, one zstd-3-framed log under `archive/`;
output verbatim, per-rule note blocks elided for space):

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

### Shared zstd dictionaries for small objects (`s4 train-dict` + `--zstd-dict`)

Single-digit-KiB objects (JSON events, per-line log PUTs, small API
payloads) barely compress with plain zstd — the window never sees
redundancy *across* objects. A **shared dictionary** trained on a sample
of similar objects moves that redundancy out of band; each object then
compresses against the dictionary. Three steps:

```bash
# 1. Train from existing small objects (backend-direct tool, like migrate).
#    Writes the dictionary to `.s4dict/<dict-id>` inside the bucket and
#    prints the gateway flag.
s4 train-dict mybucket/events/ --endpoint-url https://s3.example.com
#   → --zstd-dict 'mybucket/events/=0123456789abcdef'

# 2. Start the gateway with the printed mapping (repeatable per prefix).
s4 --endpoint-url https://s3.example.com \
   --zstd-dict 'mybucket/events/=0123456789abcdef'

# 3. Confirm the effect: codec label `cpu-zstd-dict` in the access log /
#    `s4_requests_total{codec="cpu-zstd-dict"}`, and backend object sizes.
```

Measured effect (minio E2E `dict_minio.rs`, 100 × ~300-byte JSON events
of identical schema): **8 903 bytes stored with the dictionary vs
21 923 bytes with plain cpu-zstd — 2.46× smaller (40 % of the dict-less
size)**. The win scales with how homogeneous the objects are; on
heterogeneous prefixes the dictionary won't beat plain zstd, and the
gateway then **falls back to plain cpu-zstd automatically** (both are
compressed and compared per PUT — affordable because the path is capped
at `--zstd-dict-max-bytes`, default 1 MiB).

Mechanics and operational notes:

- **When the dict path applies**: dispatcher picked `cpu-zstd` + key
  longest-prefix-matches a configured `<bucket>/<prefix>` + declared
  `Content-Length` ≤ `--zstd-dict-max-bytes`. Everything else — and
  *every* PUT when no `--zstd-dict` flag is set — is bit-for-bit
  unchanged. Multipart uploads and chunked uploads without a
  Content-Length never take the dict path.
- **Wire format is additive**: the object is a normal single-frame S4F2
  body whose frame carries the new codec id 8 (`cpu-zstd-dict`); the
  dictionary id travels in the `s4-dict-id` object-metadata key. The
  S4F2 layout itself is unchanged.
- **Pre-v1.1 readers** (older gateway / `s4-codec` builds) fail a GET of
  a dict-compressed object with the existing *unknown codec id* error —
  a clean, typed failure, not silent corruption. Roll gateways forward
  before enabling the flag if you run mixed fleets.
- **Dropping the flag doesn't strand data**: a gateway booted without
  `--zstd-dict` lazily fetches `.s4dict/<id>` from the object's bucket
  on first GET (fingerprint-verified, small LRU cache; failures surface
  as 5xx + `s4_dict_fetch_total{result="err"}`).
- **`.s4dict/<dict-id>`** is hidden from gateway listings, named by the
  SHA-256 prefix of its bytes (content-addressed, immutable; re-training
  the same corpus is idempotent).
- **No lock-in**: the stored payload is a **stock zstd frame** and the
  dictionary object is **raw zstd dictionary bytes**. Decode without any
  S4 software (the E2E pins this recipe against the real `zstd` CLI):

  ```bash
  # strip the 28-byte S4F2 frame header, then:
  aws s3 cp s3://mybucket/.s4dict/0123456789abcdef dict.bin
  zstd -D dict.bin -d payload.zst -o original.json
  # python: zstandard.ZstdDecompressor(dict_data=ZstdCompressionDict(dict.bin))
  ```

- **Dictionaries are bucket-local.** GET resolves `.s4dict/<id>` from
  the *object's own bucket*. Cross-bucket CopyObject through the
  gateway propagates the dictionary to the destination bucket
  automatically (content-addressed, idempotent); **cross-region
  replication (experimental) does not** — place the dictionary in the
  replica bucket yourself or its dict-compressed replicas fail GET
  with a typed 5xx. `.s4dict/` keys are write-protected through the
  gateway (`InvalidObjectName` on PUT/DELETE, reads allowed); manage
  dictionaries with `s4 train-dict` against the backend. `train-dict`
  also stamps the full digest as `s4-dict-sha256` metadata, which the
  lazy-fetch path verifies when present (pre-existing dictionaries
  without the stamp fall back to the 16-hex prefix check). Dictionary
  size is one 1 MiB contract enforced at all three surfaces:
  `train-dict --max-dict-bytes` rejects above-cap requests, boot-time
  `--zstd-dict` preload refuses an above-cap dictionary, and the
  flag-less lazy fetch refuses it too — so a dictionary that works with
  the flag can never become unreadable without it.
- **Reserved metadata namespace**: the gateway strips client-supplied
  `x-amz-meta-s4-*` keys on PUT — they are S4's manifest namespace and
  forging them (e.g. a stray `s4-dict-id`) must not change GET behavior.
- **Scope-outs (follow-ups)**: `s4-codec-wasm` doesn't decode
  `cpu-zstd-dict` natively yet (`s4-codec-py` does, via the
  `CpuZstdDict` binding — s4fs uses it). Multipart uploads are out of
  scope **by design**, not as a follow-up: parts never consult the
  dictionary store, and S3's 5 MiB minimum part size sits far above the
  small-object ceiling (`--zstd-dict-max-bytes`, default 1 MiB) the
  feature targets — the two size ranges never intersect. Re-training
  for schema drift no longer needs a restart — see the next section.

#### Operating dictionaries (`s4 dict-status` + `--zstd-dict-map` + SIGHUP)

Day-2 operations for the feature above: drift monitoring and
restart-less rotation.

- **Per-prefix health metrics**: the dict PUT branch exports
  `s4_dict_put_total{prefix,outcome="win"|"loss"}` and
  `s4_dict_put_bytes_total{prefix,kind="original"|"dict"|"plain"}` —
  both compression results are measured per PUT anyway, so the byte
  counters are exact whether the dictionary won or lost. Cardinality is
  bounded by the configured prefix count; without dict configuration
  the series are never registered. The gateway also self-monitors: when
  a prefix's rolling win rate over its last 100 dict-path PUTs drops
  below 0.5, it WARNs (at most once per prefix per hour) that the
  dictionary looks stale. SIGHUP map reloads are counted as
  `s4_dict_reload_total{result="ok"|"err"}`.
- **`s4 dict-status --metrics-url <URL>`** scrapes `/metrics` and
  reports per-prefix win rate / effective compression ratio / lazy
  fetch errors; any prefix below `--warn-win-rate` (default 0.5) gets a
  warning and the command exits 1, so a cron job catches drift
  unattended (`--format json` for machines). Measured output (minio E2E
  `dict_ops_minio.rs`: 30 matching JSON PUTs under `events/`, then
  random bodies under a deliberately mismatched `rand/` mapping):

  ```console
  $ s4 dict-status --metrics-url http://127.0.0.1:8014/metrics
  PREFIX                                      WIN   LOSS  WIN-RATE   ORIGINAL-BYTES     DICT-BYTES  DICT-RATIO
  dictops/events/                              30      0    100.0%             7440           1689       22.7%
  dictops/rand/                                 0     16      0.0%             6400           6608      103.2%  STALE
  lazy dict fetches: ok=0 err=0
  WARN prefix "dictops/rand/": win rate 0.00 over 16 dict-path PUT(s) is below 0.50 — dictionary may be stale; consider retraining (s4 train-dict)
  $ echo $?
  1
  ```

- **Restart-less rotation** (`--zstd-dict-map <FILE>` + SIGHUP): the
  TOML file is the reloadable twin of repeated `--zstd-dict` flags —
  same validation, same boot-time fetch + fingerprint verification,
  same 1 MiB dictionary cap (a prefix configured in both places is a
  boot error):

  ```toml
  # dict-map.toml
  [mappings]
  "mybucket/events/" = "0123456789abcdef"
  ```

  ```bash
  s4 --endpoint-url https://s3.example.com --zstd-dict-map dict-map.toml
  # rotate without a restart:
  s4 train-dict mybucket/events/ --endpoint-url https://s3.example.com  # → new dict-id
  $EDITOR dict-map.toml                                                 # point the prefix at it
  kill -HUP <pid>             # fetch + verify + atomic store swap
  ```

  A failed reload (unreadable file, bad TOML, missing `.s4dict/`
  object, fingerprint mismatch) keeps the **current** mappings live —
  ERROR log + `s4_dict_reload_total{result="err"}`, never a
  half-applied swap. In-flight requests finish on the generation they
  started with. Without `--zstd-dict-map`, SIGHUP does not touch
  dictionary configuration (the TLS cert reload on SIGHUP is
  independent and unchanged).

## Production Features

### Streaming I/O

**Measurement conditions for the numbers below** (#107): RTX 4070 Ti
SUPER + Ryzen 9 9950X, single-pass 256 MiB compressible input, codec
`cpu-zstd-3` (or as noted), single concurrent request, S4 colocated
with backend (no network RTT to amortise). TTFB excludes TLS handshake
+ SigV4 verification (those add 5–15 ms once per connection).

- **Streaming GET** for non-multipart `cpu-zstd` / `passthrough` objects:
  TTFB **8–20 ms** under the conditions above, memory ≈ zstd window
  (8 MiB at level 3) + 64 KiB buffer
- **Streaming PUT** for the same codecs: input never fully buffered, peak memory
  ≈ compressed size (5 GB → ~50 MB at 100× ratio). Client-supplied whole-body
  checksums (`Content-MD5`, `x-amz-checksum-{crc32, crc32c, sha1, sha256, crc64nvme}`)
  are verified **in-stream** via a tee-into-hasher wrapper (v0.9 #106): mismatched
  bytes surface as `400 BadDigest` without buffering the body. GPU codecs and
  multipart `UploadPart` keep the buffered per-body / per-part verify path
  (the bytes are already in memory there for framing / padding) —
  see [`docs/security/streaming-checksum-coverage.md`](docs/security/streaming-checksum-coverage.md)
  for the full coverage matrix and the codec-API constraint that makes
  this a fundamental property of those branches, not deferred plumbing
- **GPU streaming compress** (v0.2): nvCOMP `zstd` / `gdeflate` PUTs run a
  per-chunk pipeline so a 10 GB highly-compressible upload peaks at ~210 MB
  host RAM instead of buffering the full input
- **Single-PUT framed format unification** (v0.2): every compressed PUT now
  uses the same `S4F2` multi-frame format multipart uploads use, with an
  optional `<key>.s4index` sidecar. Range GET partial-fetch optimisation
  applies to single-PUT objects too, not just multipart
- **Multipart per-part compression**: each part compressed and frame-encoded
  (`S4F2` magic), per-frame codec dispatch (mixed codecs in one object)
- **Multipart final-part padding trim** (v0.2): the final part of a multipart
  with a tiny highly-compressible tail skips `S4P1` padding (saves up to
  ~5 MiB per object on highly compressible workloads)
- **Range GET via sidecar `<key>.s4index`**: only the needed compressed bytes
  are fetched from backend, decoded, and sliced. Falls back to full read when
  sidecar is absent
- **Encryption-aware Range GET fast-path** (v0.9 #106): SSE-S4 chunked
  (`--sse-chunk-size > 0`, S4E6 frame) Range GETs now partial-fetch just
  the enclosing S4E6 chunks from backend instead of pulling the full
  encrypted body. The v3 `<key>.s4index` sidecar carries the per-PUT salt +
  chunk geometry so the GET path can compute the encrypted byte range
  without re-fetching the header. SSE-KMS / SSE-C / SSE-S4 buffered
  (`--sse-chunk-size 0`) keep the v0.8.12 #120 buffered fallback (= full
  decrypt → frame-parse → slice); covering them needs separate plumbing
  (KMS DEK envelope shape, customer-key per-request material) and is on
  the v0.10+ roadmap
- **Byte-range aware `upload_part_copy`** (v0.2): when the source is S4-framed,
  the user-visible byte range is what gets copied (decompressed and re-framed),
  not raw compressed bytes

### Server-side encryption — Range GET fast-path matrix

S4 supports four SSE modes (table below). The **Range GET fast-path**
introduced in v0.9 #106 partial-fetches only the enclosing encrypted
chunks for a given byte range instead of pulling the full body — but it
only works for **SSE-S4 chunked** (`--sse-chunk-size > 0`, `S4E6` wire
envelope). The other three modes fall back to the v0.8.12 #120 buffered
path (full decrypt → frame-parse → slice).

| SSE mode | CLI flag | Wire envelope | Range GET fast-path? |
|---|---|---|---|
| SSE-S4 chunked (default since v0.8 #52) | `--sse-s4-key <path>` + `--sse-chunk-size 1048576` (default) | `S4E6` | ✅ partial-fetch via v3 sidecar |
| SSE-S4 buffered (back-compat) | `--sse-s4-key <path>` + `--sse-chunk-size 0` | `S4E2` | ❌ buffered fallback |
| SSE-C (customer-provided key) | per-request `x-amz-server-side-encryption-customer-*` headers | `S4E3` | ❌ buffered fallback |
| SSE-KMS (envelope, per-object DEK) | `--kms-local-dir <dir>` (or `--features aws-kms`) | `S4E4` | ❌ buffered fallback |
| Multipart with any SSE | (any of the above on a multipart PUT) | per-part `S4Ex` | ❌ no sidecar emitted (v0.8.16 #151) |

**Why only chunked SSE-S4?** Non-chunked envelopes (`S4E2` / `S4E3` /
`S4E4`) wrap the entire body under one AES-256-GCM authentication tag.
AEAD decrypt is only defined over the full ciphertext + AAD + tag
quadruple — there is no "verify just the prefix" mode — so partial
plaintext cannot be exposed without fetching and tag-verifying the
whole body. This is the AEAD security contract, not an optimization
deferment. The `S4E6` chunked envelope (v0.8 #52, refined in
v0.8.1 #57) explicitly slices the plaintext into fixed-size chunks
and emits one tag per chunk with a nonce derived from a per-PUT
salt + chunk index, which is what makes chunk-aligned partial
decrypt well-defined. Full per-mode walkthrough lives in
[`docs/security/sse-partial-fetch-constraint.md`](docs/security/sse-partial-fetch-constraint.md).

**Operator recommendation**: for Range-GET-heavy workloads on large
objects (parquet / ORC footer reads, video segment seeks, log-line
slice reads) where SSE is required, scope your data to **SSE-S4
chunked** to keep the fast-path. The 1 MiB default chunk size
matches the typical parquet row-group read pattern; smaller chunks
give finer-grained partial fetch at higher tag overhead, larger
chunks reduce on-disk tag bytes but do more wasted decrypt per Range
GET.

```bash
s4-server \
  --sse-s4-key /etc/s4/sse.key \
  --sse-chunk-size 1048576 \
  ...
```

If SSE-KMS or SSE-C is required by your key-management posture,
either accept the buffered Range GET cost or restructure the data
into smaller objects so the buffered fetch is bounded. Chunked-KMS
(provisional `S4E7`) and chunked-SSE-C (provisional `S4E8`)
envelopes are v0.11+ roadmap candidates, not promised features.

### Observability
- **`/health`** — liveness probe, always 200 OK
- **`/ready`** — readiness probe, runs `ListBuckets` against the backend
- **`/metrics`** — Prometheus text format
  (`s4_requests_total{op,codec,result}`, `s4_bytes_in_total`, `s4_bytes_out_total`,
  `s4_request_latency_seconds`, `s4_policy_denials_total{action,bucket}`)
- **Structured JSON logs** (`--log-format json`) with per-request fields:
  `op`, `bucket`, `key`, `codec`, `bytes_in`, `bytes_out`, `ratio`, `latency_ms`, `ok`
- **OpenTelemetry traces** (`--otlp-endpoint http://collector:4317`) — each
  PUT/GET emitted as `s4.put_object` / `s4.get_object` span with semantic
  attributes; export to Jaeger / Tempo / Grafana / AWS X-Ray.

### Security
- **Native HTTPS / TLS** (v0.2) — `--tls-cert` / `--tls-key` for direct
  termination via `tokio-rustls + ring`, ALPN advertises `h2` then
  `http/1.1`. No reverse-proxy required for HTTPS deployments.
- **Bucket policy enforcement at the gateway** (v0.2) — `--policy <path>`
  accepts an AWS-style bucket policy JSON; every PUT / GET / DELETE / List /
  Copy / UploadPartCopy is evaluated with explicit Deny > explicit Allow >
  implicit Deny semantics (matches AWS). Subset: `Effect`, `Action` (e.g.
  `s3:GetObject` / `s3:*`), `Resource` with glob, `Principal` (SigV4
  access-key match). Denials are bumped on
  `s4_policy_denials_total{action,bucket}`.

### Data Integrity
- **CRC32C** stored per-object (single PUT) or per-frame (multipart), verified on GET
- **`copy_object` S4-aware**: source's `s4-*` metadata is preserved across
  `MetadataDirective: REPLACE` (prevents silent corruption of the destination)
- **Zstd decompression bomb hardening**: `Decoder + take(manifest.original_size + 1024)`
  caps the decode at the manifest's declared size (+ a small overshoot margin) so a
  zero-size manifest paired with a high-ratio frame surfaces as a typed `Io("bomb
  detected")` instead of unbounded RAM growth. The cap is still bound by the
  manifest claim itself — a 5 GiB manifest is honored up to 5 GiB, so operators
  must additionally enforce a per-request memory ceiling at the listener
  (`--max-body-bytes` / a future per-frame cap) for adversarial uploads

### Storage class transitions
- Each compressed object is stored as `<key>` + `<key>.s4index` sidecar.
  S3 lifecycle rules must move both files together — a split pair breaks
  Range GET (sidecar in IA + main in Glacier ⇒ `InvalidObjectState`).
- Recommended: `"Filter": {}` (whole bucket) or a `Filter.Prefix` rule
  that covers both `foo/...` and `foo/....s4index`. Avoid size- or
  suffix-scoped filters that catch one but not the other.
- See [docs/storage-class-transitions.md](docs/storage-class-transitions.md)
  for two example lifecycle JSONs (IA-after-30d and prefix→Glacier-after-60d),
  the anti-pattern walkthrough, and a `head-object` drift-audit recipe.
- v1.2: a `transition` rule in an `s4 maintain` policy automates the
  same change from the S4 side, with the sidecar guaranteed to
  accompany its main object — see "Policy-driven maintenance" above.

### S3 API coverage (45+ ops)
- Compression hook: `put_object`, `get_object`, `upload_part`
- Range GET: full S3 spec (`bytes=N-M`, `bytes=-N`, `bytes=N-`)
- Multipart: `create_multipart_upload`, `upload_part`, `complete_multipart_upload`, `abort_multipart_upload`, `list_parts`, `list_multipart_uploads`
- Phase 2 delegations (passthrough): ACL, Tagging, Lifecycle, Versioning, Replication, CORS, Encryption, Logging, Notification, Website, Object Lock, Public Access Block, ...
- Hidden: `*.s4index` sidecars are filtered from `list_objects[_v2]` responses

## Testing & Validation

| Tier | What runs | Where | Pass count |
|---|---|---|---|
| **Unit + integration** | parsers, registry, blob helpers, S3 trait, policy, TLS | every push (CI) | 70+ |
| **Chaos / fault-injection** | mid-stream GET error, HEAD timeout fail-close, concurrent overwrite, SSE keyring rotation, MPU complete failure (deterministic, in-memory) | every push (CI) | 6 |
| **proptest fuzz** | 39 properties × 256–10K cases (push), × 1M (nightly) | every push + nightly | 39 |
| **bolero coverage-guided** | 7 targets, libfuzzer engine | nightly (matrix, 30 min × 5) | 7 |
| **fuzz canary** | proves fuzz framework is alive | every push | 3 |
| **Docker MinIO E2E** | full HTTP wire + SigV4 against real MinIO + multipart + upload_part_copy | every push (CI) | 8 |
| **In-process TLS E2E** | rcgen self-signed cert + tokio-rustls + reqwest h2/h11 | every push | 2 |
| **GPU codec E2E** | real CUDA, nvCOMP zstd / Bitcomp / GDeflate, streaming + bytes API | manual (`--features nvcomp-gpu`) | 5 |
| **Real AWS S3 E2E** | OIDC role + actual S3, single-PUT / multipart / Range GET | nightly (`aws-e2e.yml`, opt-in) | 3 |
| **Soak / load** | 24h sustained load, RSS / FD / connection leak detection | manual (`scripts/soak/run.sh`) | continuous |

**125 default tests + 15 ignored (Docker / GPU / AWS env required) = 140 tests**,
plus PROPTEST_CASES=10000 stress run on every push (~73 sec, 380K fuzz cases),
1M cases × 38 properties nightly (~6 h, 38M+ fuzz cases).

Two real bugs already caught by fuzz infrastructure:
1. `FrameIter` infinite-loop on 1-byte input (DoS) — fixed with `fused: bool`
2. `cpu_zstd::decompress` could OOM on attacker-controlled manifest claim —
   fixed with `Decoder + take(limit)`

```bash
cargo test --workspace                   # default
cargo test --workspace -- --ignored --test-threads=1   # E2E (Docker required)
PROPTEST_CASES=100000 cargo test --workspace --release --test fuzz_parsers --test fuzz_server --test fuzz_advanced
NVCOMP_HOME=... cargo test --workspace --features s4-server/nvcomp-gpu -- --ignored
./scripts/soak/run.sh                    # 24 h soak (Marketplace pre-release)
```

## Configuration

| CLI flag | Default | Description |
|---|---|---|
| `--endpoint-url` | (required) | Backend S3 endpoint (e.g. `https://s3.us-east-1.amazonaws.com`) |
| `--host` | `127.0.0.1` | Bind host |
| `--port` | `8014` | Bind port |
| `--domain` | (none) | Virtual-hosted-style requests domain |
| `--codec` | `cpu-zstd` | Default codec: `passthrough`, `cpu-zstd`, `nvcomp-zstd`, `nvcomp-bitcomp` |
| `--zstd-level` | `3` | CPU zstd compression level (1–22) |
| `--dispatcher` | `sampling` | `always` (use `--codec`) or `sampling` (entropy + magic byte) |
| `--log-format` | `pretty` | `pretty` (terminal) or `json` (CloudWatch / fluent-bit) |
| `--otlp-endpoint` | (none) | OpenTelemetry OTLP gRPC endpoint |
| `--service-name` | `s4` | OTel resource `service.name` |
| `--tls-cert` | (none) | TLS server certificate (PEM). Together with `--tls-key`, terminates HTTPS on the listener. Hot-reload via `SIGHUP` (v0.3) |
| `--tls-key` | (none) | TLS server private key (PEM, PKCS#8 or RSA) |
| `--acme` | (none) | Comma-separated domains for ACME (Let's Encrypt) auto-cert via TLS-ALPN-01. Mutually exclusive with `--tls-cert` (v0.3) |
| `--acme-contact` | (none) | Contact email for ACME account (required when `--acme` is set) |
| `--acme-cache-dir` | `~/.s4/acme/` | Cert + account cache directory (so restarts don't trigger fresh enrollments and exhaust LE rate limits) |
| `--acme-staging` | (off) | Use the LE staging directory (no rate limits; cert is not browser-trusted). Recommended for first-run |
| `--policy` | (none) | AWS-style bucket policy JSON. When set, every PUT/GET/DELETE/List request is evaluated before backend dispatch |

AWS credentials are read from the standard AWS chain (`AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY` / `AWS_PROFILE` / IAM role on EC2).

### HTTPS

S4 can terminate TLS itself — no fronting reverse proxy required:

```bash
s4 --endpoint-url https://s3.us-east-1.amazonaws.com \
   --host 0.0.0.0 --port 8443 \
   --tls-cert /etc/ssl/s4.crt --tls-key /etc/ssl/s4.key
aws --endpoint-url https://localhost:8443 s3 ls
```

Backed by `tokio-rustls` + `ring`. ALPN advertises `h2` then `http/1.1`, so
HTTP/2 is negotiated automatically with capable clients. Without these
flags, S4 serves plain HTTP (the default).

**Cert hot-reload (v0.3)**: rotate `--tls-cert` / `--tls-key` files on disk
and `kill -HUP <pid>` to swap the active cert without dropping any
in-flight connections. Re-read failures keep the previous cert in effect
so a bad deploy never causes a listener outage.

**ACME / Let's Encrypt (v0.3)**: for public deployments, fetch and renew
certs automatically with `--acme`:

```bash
s4 --endpoint-url https://s3.us-east-1.amazonaws.com \
   --host 0.0.0.0 --port 443 \
   --acme s4.example.com,api.example.com \
   --acme-contact ops@example.com \
   --acme-staging   # remove for production after first-run validation
```

Uses TLS-ALPN-01 challenge handled inline on the listening port — no
separate port-80 listener required. Background renewal at the standard
~60-day interval; `s4_acme_renewal_total{result}` Prometheus counter
+ `s4_acme_cert_expiry_seconds` gauge for monitoring.

### Bucket policy enforcement

Pass an AWS-style bucket policy JSON to `--policy` to gate requests at the
gateway:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {"Sid": "ReadOnly",  "Effect": "Allow", "Action": ["s3:GetObject", "s3:ListBucket"],
     "Resource": ["arn:aws:s3:::my-bucket", "arn:aws:s3:::my-bucket/*"]},
    {"Sid": "DenyDelete", "Effect": "Deny",  "Action": "s3:DeleteObject",
     "Resource": "arn:aws:s3:::my-bucket/*"}
  ]
}
```

Supported subset (v0.3):

- **`Effect` / `Action` / `Resource` / `Principal`** (v0.2 baseline): SigV4
  access-key match for Principal, glob matching for Resource.
- **`Condition` clauses** (v0.3 #13): `IpAddress` / `NotIpAddress` (CIDR),
  `StringEquals` / `StringNotEquals` / `StringLike` / `StringNotLike`,
  `DateGreaterThan` / `DateLessThan`, `Bool`. Supports the well-known
  AWS context keys `aws:SourceIp` (taken from the `X-Forwarded-For`
  header — set this at your reverse proxy / load balancer),
  `aws:UserAgent`, `aws:CurrentTime`, `aws:SecureTransport` (true when
  the listener is `--tls-cert` or `--acme`).

Decision order is the standard AWS one: **explicit Deny > explicit Allow >
implicit Deny**. Conditions are AND-combined within a Statement. Denials
are exposed as the `s4_policy_denials_total{action,bucket}` Prometheus
counter.

For STS / AssumeRole chains and cross-account delegation (still out of
scope), front S4 with an IAM-aware proxy and use this flag for the
in-gateway last-mile checks.

## On-the-wire Format

S4 stores data as either:

### Single PUT (framed, `S4F2` magic, since v0.2 #4)
S3 metadata holds the manifest:

```
x-amz-meta-s4-codec:           passthrough | cpu-zstd | nvcomp-zstd | ...
x-amz-meta-s4-original-size:   <decoded bytes>
x-amz-meta-s4-compressed-size: <stored bytes, includes S4F2 framing>
x-amz-meta-s4-crc32c:          <CRC32C of original bytes>
```

Since v0.2 #4 the body is the same `S4F2` framed format multipart uploads
use (one frame per `DEFAULT_S4F2_CHUNK_SIZE` = 4 MiB chunk). Small objects
(< 4 MiB) produce a single S4F2 frame and pay a constant **+28 byte** wire
overhead vs the raw compressed bytes — see footnote [^wire-overhead].

### Multipart (framed, `S4F2` magic, per-part compression)

```
x-amz-meta-s4-multipart: true
x-amz-meta-s4-codec:     <default codec for the object>
```

Object body is a sequence of:

```
┌──────────── 28-byte frame header ────────────┐
│ "S4F2" │ codec_id u32 │ orig u64 │ comp u64  │ crc32c u32 │  payload (comp bytes)
└────────────────────────────────────────────────┘

(optional) ┌──── padding ────┐
           │ "S4P1" │ len u64 │ <len zero bytes>
           └─────────────────┘
```

A sidecar object `<key>.s4index` (binary, `S4IX` magic) maps decompressed
byte ranges to compressed byte offsets — used by Range GET to fetch only the
needed bytes from S3.

[^wire-overhead]: Per v0.4 #18 micro-bench
    (`crates/s4-server/examples/bench_framed_overhead.rs`, cpu-zstd codec,
    partially-compressible synthetic input, single-frame payloads):

    | size | raw_compressed | framed | overhead_bytes | overhead_pct |
    |---|---:|---:|---:|---:|
    | 1 KiB | 121 B | 149 B | +28 B | 23.14% |
    | 100 KiB | 12 040 B | 12 068 B | +28 B | 0.23% |
    | 1 MiB | 102 811 B | 102 839 B | +28 B | 0.03% |

    Overhead is a flat 28 bytes (= `FRAME_HEADER_BYTES`: `"S4F2"` magic u32 +
    codec_id u32 + original_size u64 + compressed_size u64 + crc32c u32) per
    single-frame object, independent of payload size; the percentage shrinks
    quickly as objects grow. Reproduce with
    `cargo run --release --example bench_framed_overhead -p s4-server`.

## Project Status

> **Status: v1.0 — stable surface, no public production deployment
> reference yet.** v1.0 is the SemVer-stable freeze of the wire formats,
> library API surface, CLI subcommands, `s3s 0.13` HTTP trait set, and
> Helm `values.yaml` key shape enumerated in the §"Stability" section
> above. It is *not* a marketing claim that "S4 has been battle-tested
> at every Fortune 500." The freeze means downstream consumers can pin
> `s4-server = "1"` (or `s4-codec = "1"`, or `s4-config = "1"` in a
> `Cargo.toml`; or `ghcr.io/abyo-software/s4:1` for the container) and
> rely on the surface not changing
> under them; first public production deployment references are still
> being collected. If you're putting S4 into a TB-scale workload, please
> file an issue tagged `production-reference` so we can list your
> deployment alongside the audit + fuzz evidence below.

- **Release line:** [CHANGELOG.md](CHANGELOG.md) has the full
  per-version history; the GitHub Releases page has the cut-points.
  Cumulative scope through v1.0 is **714+ workspace tests + 14+
  production milestones** covering S3-compatible PUT / GET / multipart
  / Select / SSE-S3 / SSE-KMS / SSE-C / IAM Conditions / bucket
  policy / versioning / object-lock / lifecycle / inventory /
  notifications (Webhook / SQS / SNS) / CORS / tagging / MFA delete /
  SigV4 + SigV4a, plus Python (`s4-codec-py`) and browser
  (`s4-codec-wasm`) bindings, all on crates.io as the
  [`s4-server`](https://crates.io/crates/s4-server) /
  [`s4-codec`](https://crates.io/crates/s4-codec) /
  [`s4-config`](https://crates.io/crates/s4-config) trio. **Cross-region
  replication** ships as experimental scaffolding (config surface + wire
  stub) and is intentionally **excluded from the v1.0 freeze** — promotion
  to production-grade is on the v1.x roadmap.
- **Audit history:** three rounds of deep audit (`第一弾` / `第二弾` /
  `第三弾`) closed in v0.8.2 → v0.8.5; pre-launch audit (claude + codex
  cross-review, tracker #111) in v0.8.7 → v0.8.8; integrated audit
  rounds R1–R6 across v0.9 / v0.10 / v0.11 cuts; v1.0 readiness audit
  (Opus + Codex adversarial review) drove 13 surfaced findings to
  closure — including the v1.0 stability section in this README, the
  `#[non_exhaustive]` annotations on every public enum, gating
  test-only helpers out of the public API contract, and qualifying the
  backend compatibility matrix above. Findings spanned CRITICAL
  pre-auth state-machine bugs, HTTP wire hardening, GPU codec safety,
  binding correctness, background-task lifecycle, README claim
  accuracy, and v1.0 freeze surface completeness. CVE clean
  (`cargo audit`, see CI `security-audit` job); 4 advisories accepted
  as risk-with-mitigation per
  [`docs/security/cargo-audit-ignores.md`](docs/security/cargo-audit-ignores.md).
- **Continuous fuzz farm** (v0.8.6) — 5 bolero targets running 24/7
  under a `systemd-user` slice budgeted at 8 cores / 30 GiB (1/4 of the
  build host). Coverage compounds across `Restart=always` wakeups; any
  crash auto-files a GitHub issue (label `fuzz-crash`, deduped by SHA1
  of the input). First catch: **#89** (CpuZstd / CpuGzip
  alloc-before-validate) found within seconds, fixed and shipped
  same-day in v0.8.6.
- **Real-GPU validation** done on RTX 4070 Ti SUPER + nvCOMP 5.x:
  streaming zstd 1 GiB roundtrip + GDeflate roundtrip both green; OMB
  bench runs on EC2 c7gd.8xlarge (latest v0.8 perf chart at
  `docs/perf-v0.8.png`).
- **Suitable for** log archival, data lake / parquet/ORC analytics,
  drop-in transparent-compression proxy in front of any S3-compatible
  backend. The v1.0 surface freeze means you can integrate against a
  stable contract; the "no public production reference yet" caveat
  means we still recommend pairing with backend-native replication /
  versioning for irreplaceable data until at least one production
  reference is published.
- **Roadmap is driven by audit findings + continuous fuzz** rather than
  feature checklists; file issues at
  https://github.com/abyo-software/s4/issues to influence it.

## Contributing

Pull requests are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for the
development setup, coding conventions, and the test/fuzz/soak protocol.

By contributing, you agree your contributions will be licensed under
Apache-2.0 (no separate CLA required).

## Security

Found a vulnerability? Please **do not open a public issue**. Instead, follow
[SECURITY.md](SECURITY.md) for coordinated disclosure.

## License

Licensed under the **Apache License, Version 2.0** ([LICENSE](LICENSE)).
See [NOTICE](NOTICE) for third-party attributions including the vendored
`ferro-compress` (Apache-2.0 OR MIT) and the optional NVIDIA nvCOMP SDK
(proprietary, BYO).

Full third-party license disclosure (auto-generated from `cargo about`
across the three target triples we ship to —
`x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu` /
`wasm32-unknown-unknown`) lives at
[`docs/THIRD_PARTY_LICENSES.html`](docs/THIRD_PARTY_LICENSES.html).
~350 transitive crates, all permissive
(Apache-2.0 / MIT / BSD-{2,3}-Clause / ISC / Zlib / Unicode / 0BSD /
MPL-2.0 / OpenSSL / CDLA-Permissive-2.0). Regenerate with
`cargo about generate about.hbs --output-file docs/THIRD_PARTY_LICENSES.html`
(requires `cargo install cargo-about --features cli`).

**The optional `nvcomp-gpu` feature** pulls the proprietary NVIDIA
nvCOMP SDK at build time. nvCOMP is **not bundled** with S4 distributions;
operators set `NVCOMP_HOME` to a locally extracted SDK from the
[NVIDIA Developer Zone](https://developer.nvidia.com/nvcomp-download).
nvCOMP redistribution is subject to NVIDIA's SLA — confirm with NVIDIA
in writing before bundling into a downstream AMI / container image.

`"S4"` and `"Squished S3"` are unregistered trademarks of abyo software 合同会社.
`"Amazon S3"` and `"AWS"` are trademarks of Amazon.com, Inc. S4 is not
affiliated with, endorsed by, or sponsored by Amazon.

## Authors

- abyo software 合同会社 — sponsoring organization, commercial AMI distribution
- masumi-ryugo — original author / maintainer
