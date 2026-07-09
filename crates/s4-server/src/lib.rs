//! S4 server crate — `S4Service` (圧縮 hook 付き S3 trait 実装) と関連 helper を提供。

pub mod access_log;
pub mod acme;
pub mod audit_log;
pub mod blob;
pub mod cors;
pub mod dict;
pub mod estimate;
pub(crate) mod frame_stream;
pub mod gpu_batch;
pub mod inventory;
pub mod kms;
pub mod ledger;
pub mod lifecycle;
pub mod lock_recovery;
pub mod maintain;
pub mod marketplace;
pub mod metrics;
pub mod mfa;
pub mod migrate;
pub mod mpu_durable;
pub mod multipart_state;
pub mod notifications;
pub mod object_lock;
#[cfg(feature = "parquet-recompact")]
pub mod parquet_recompact;
pub mod policy;
pub mod rate_limit;
pub mod recompact;
pub mod repair;
pub mod replication;
pub mod routing;
pub mod select;
// FREEZE-CANDIDATE: `s4_server::service` — `SigV4aGate`, `resolve_range`,
// `S4Service` (also re-exported at crate root) are consumed by the `s4`
// binary (`src/main.rs`) and integration tests as a separate compilation
// unit, so the module cannot be `pub(crate)`. Listed in the README v1.0
// public-surface freeze table to make the "may change in any minor release"
// claim accurate. Cluster B owns the README entry.
pub mod service;
pub mod service_arc;
pub mod sigv4a;
// FREEZE-CANDIDATE: `s4_server::sse` — `SseKey`, `SseKeyring`,
// `SharedSseKeyring`, `compute_key_md5`, `SSE_C_ALGORITHM`, `encrypt`,
// `decrypt`, `encrypt_v2`, `parse_s4e6_header`, `peek_magic`,
// `S4E5_HEADER_BYTES`, `S4E6_HEADER_BYTES`, `SSE_MAGIC_V5`,
// `ALGO_AES_256_GCM` are consumed by the `s4` binary (`src/main.rs`),
// examples (`examples/bench_sse_throughput.rs`) and integration tests
// (`tests/{feature_e2e,roundtrip,chaos,multipart_audit_71,sidecar_repair_via_minio}.rs`).
// Cannot be `pub(crate)`. Listed in the README v1.0 public-surface freeze
// table; Cluster B owns the README entry.
pub mod sse;
pub mod state_loader;
// FREEZE-CANDIDATE: `s4_server::streaming` — `DEFAULT_S4F2_CHUNK_SIZE`,
// `streaming_compress_to_frames`, `streaming_compress_to_frames_with` are
// consumed by benchmarking examples (`examples/bench_pipeline.rs`,
// `examples/bench_framed_overhead.rs`) and integration tests
// (`tests/gpu_streaming.rs`). External surface is small but the
// async-streaming signatures (`StreamingBlob`, `CodecKind`) make a
// crate-root shim noisy; freezing the three items in place is cleaner.
// Cluster B owns the README entry.
pub mod streaming;
pub mod streaming_checksum;
pub mod tagging;
pub mod tls;
pub mod versioning;

pub use s4_codec as codec;
pub use s4_config as config;
pub use service::S4Service;
