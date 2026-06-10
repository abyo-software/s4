//! S4 圧縮 codec layer。バックエンドを差し替え可能にする中立 trait を提供する。
//!
//! ## 採用 backend (2026-05 検討)
//!
//! - **nvCOMP** (NVIDIA proprietary、要 license 確認): Bitcomp / gANS / zstd-GPU
//! - **DietGPU** (Meta, MIT): ANS-only、license clean な fallback
//! - **CPU zstd**: GPU 無し環境向け究極の fallback / test bed

use std::str::FromStr;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod cpu_gzip;
pub mod cpu_zstd;
pub mod cpu_zstd_dict;
pub mod dietgpu;
pub mod dispatcher;
#[cfg(feature = "nvcomp-gpu")]
mod ferro_compress;
/// v0.8 #51: GPU-accelerated CSV column scan for the S3 Select WHERE
/// evaluator. Feature-gated on `nvcomp-gpu` so the default build doesn't
/// need cudarc / a CUDA driver. The s4-server `select` module calls into
/// this only when the parsed query shape matches the supported subset
/// (single-column equality / inequality / GT / LT / LIKE-prefix), and
/// otherwise falls back to the existing CPU evaluator transparently.
#[cfg(feature = "nvcomp-gpu")]
pub mod gpu_select;
pub mod index;
pub mod multipart;
pub mod nvcomp;
pub mod passthrough;
pub mod registry;

pub use registry::CodecRegistry;

/// 圧縮 codec の種類 (manifest に記録、後段の decompress で codec を確定するために使う)
///
/// v1.0 stability: `#[non_exhaustive]` — future codecs (e.g. LZ4, Brotli)
/// may be added in minor releases without bumping major. Downstream
/// callers must include a `_ =>` arm when matching on this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CodecKind {
    Passthrough,
    NvcompBitcomp,
    NvcompGans,
    NvcompZstd,
    DietGpuAns,
    CpuZstd,
    /// nvCOMP GDeflate (v0.2 #9). DEFLATE-family GPU codec; output bytes are
    /// NOT gzip-compatible at the wire level (different framing) but the
    /// algorithm-level format aligns with stock DEFLATE/zlib decoders given
    /// the right wrapper.
    NvcompGDeflate,
    /// CPU gzip via `flate2` (v0.4 #26). Produces RFC 1952 gzip output that
    /// any standard `gunzip`-aware client can decode without knowing about
    /// S4. Pair with the `Content-Encoding: gzip` header to serve to a
    /// browser / curl that's never heard of S4.
    CpuGzip,
    /// CPU zstd with a shared trained dictionary (v1.1 `--zstd-dict`).
    /// Same wire bytes as stock `zstd -D <dictfile>` — a plain zstd frame
    /// that references an external dictionary. The dictionary itself is
    /// NOT in the frame; the s4-server PUT path records which dictionary
    /// was used in the `s4-dict-id` object-metadata key, and the GET path
    /// resolves it back. Readers older than this variant fail with the
    /// existing unknown-codec-id error (additive wire change: new id, no
    /// layout change).
    CpuZstdDict,
}

impl CodecKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::NvcompBitcomp => "nvcomp-bitcomp",
            Self::NvcompGans => "nvcomp-gans",
            Self::NvcompZstd => "nvcomp-zstd",
            Self::DietGpuAns => "dietgpu-ans",
            Self::CpuZstd => "cpu-zstd",
            Self::NvcompGDeflate => "nvcomp-gdeflate",
            Self::CpuGzip => "cpu-gzip",
            Self::CpuZstdDict => "cpu-zstd-dict",
        }
    }

    /// 安定 numeric ID。`s4-codec/multipart.rs` の frame header に書き込む際に使う。
    /// ⚠️ **この値は wire format の一部** — 既存値の変更禁止 (新 codec は新 ID を割当)。
    pub fn id(self) -> u32 {
        match self {
            Self::Passthrough => 0,
            Self::CpuZstd => 1,
            Self::NvcompZstd => 2,
            Self::NvcompBitcomp => 3,
            Self::NvcompGans => 4,
            Self::DietGpuAns => 5,
            Self::NvcompGDeflate => 6,
            Self::CpuGzip => 7,
            Self::CpuZstdDict => 8,
        }
    }

    pub fn from_id(id: u32) -> Option<Self> {
        Some(match id {
            0 => Self::Passthrough,
            1 => Self::CpuZstd,
            2 => Self::NvcompZstd,
            3 => Self::NvcompBitcomp,
            4 => Self::NvcompGans,
            5 => Self::DietGpuAns,
            6 => Self::NvcompGDeflate,
            7 => Self::CpuGzip,
            8 => Self::CpuZstdDict,
            _ => return None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown codec kind: {0}")]
pub struct ParseCodecKindError(String);

impl FromStr for CodecKind {
    type Err = ParseCodecKindError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "passthrough" => Self::Passthrough,
            "nvcomp-bitcomp" => Self::NvcompBitcomp,
            "nvcomp-gans" => Self::NvcompGans,
            "nvcomp-zstd" => Self::NvcompZstd,
            "dietgpu-ans" => Self::DietGpuAns,
            "cpu-zstd" => Self::CpuZstd,
            "nvcomp-gdeflate" => Self::NvcompGDeflate,
            "cpu-gzip" => Self::CpuGzip,
            "cpu-zstd-dict" => Self::CpuZstdDict,
            other => return Err(ParseCodecKindError(other.into())),
        })
    }
}

/// 圧縮済 chunk のメタ情報。S3 オブジェクトの metadata に格納される。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkManifest {
    pub codec: CodecKind,
    pub original_size: u64,
    pub compressed_size: u64,
    pub crc32c: u32,
}

/// v0.8 #55: per-op telemetry returned by `CodecRegistry::compress_with_telemetry`
/// / `decompress_with_telemetry`. Lets the s4-server caller stamp Prometheus
/// metrics (`s4_gpu_compress_seconds`, `s4_gpu_throughput_bytes_per_sec`,
/// `s4_gpu_oom_total`) without s4-codec needing a `metrics` dep itself —
/// callback pattern keeps the codec dep tree slim.
///
/// Fields:
/// - `codec`: stable codec kind name (`CodecKind::as_str()` —
///   `"cpu-zstd"` / `"nvcomp-zstd"` / etc).
/// - `bytes_in`: input length to the operation. For compress this is the
///   uncompressed input; for decompress this is the compressed input.
/// - `bytes_out`: output length. For compress = compressed; for decompress
///   = decompressed.
/// - `gpu_seconds`: `Some(elapsed_secs)` for GPU-backed codecs (Nvcomp*),
///   `None` for CPU codecs (CpuZstd / Passthrough / CpuGzip). Callers
///   skip the GPU metric stamp when this is `None`.
/// - `oom`: `true` iff the operation failed with an OOM-classified error.
///   The associated `Result` is still `Err(...)`; this flag exists so the
///   stamp helper can tell OOM apart from generic backend errors without
///   introspecting the `CodecError` chain at the call site.
#[derive(Debug, Clone, Copy)]
pub struct CompressTelemetry {
    pub codec: &'static str,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub gpu_seconds: Option<f64>,
    pub oom: bool,
}

impl CompressTelemetry {
    /// CPU-codec convenience constructor — `gpu_seconds = None`,
    /// `oom = false`. Used by passthrough / cpu-zstd / cpu-gzip path.
    pub fn cpu(codec: &'static str, bytes_in: u64, bytes_out: u64) -> Self {
        Self {
            codec,
            bytes_in,
            bytes_out,
            gpu_seconds: None,
            oom: false,
        }
    }

    /// GPU-codec convenience constructor — populates `gpu_seconds`
    /// from the measured wall-clock duration of the inner compress /
    /// decompress call.
    pub fn gpu(codec: &'static str, bytes_in: u64, bytes_out: u64, seconds: f64) -> Self {
        Self {
            codec,
            bytes_in,
            bytes_out,
            gpu_seconds: Some(seconds),
            oom: false,
        }
    }

    /// Mark this telemetry as the OOM-failure shape — paired with
    /// `Err(CodecError::Backend(...))`. Callers stamp
    /// `s4_gpu_oom_total{codec=...}` when this is `true`.
    pub fn with_oom(mut self) -> Self {
        self.oom = true;
        self
    }
}

/// v0.8 #55: heuristic OOM classifier. nvCOMP / cudarc surface OOM as a
/// `CodecError::Backend(anyhow!("...out of memory..."))` (the underlying
/// CUDA driver returns `CUDA_ERROR_OUT_OF_MEMORY` which `cudarc` /
/// nvCOMP stringify); we substring-match for the well-known fragments
/// so the metric stamp doesn't need to thread a typed error variant
/// through the FFI boundary. Returns `true` only on a high-confidence
/// match; non-OOM backend errors (CRC mismatch, IO error, etc.) yield
/// `false` and are stamped as plain `s4_requests_total{result="err"}`
/// without bumping the OOM counter.
pub fn looks_like_oom(err: &CodecError) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("out of memory")
        || s.contains("cudaerrormemoryallocation")
        || s.contains("cuda_error_out_of_memory")
}

/// codec 操作のエラー型。`anyhow::Error` ではなく専用型にすることで、上位 (S4Service) が
/// HTTP エラーコードを意味的に出し分けやすくする。
///
/// v1.0 stability: `#[non_exhaustive]` — new error variants (e.g. for
/// future codecs or validation guards) may be added in minor releases.
/// Downstream callers must include a `_ =>` arm when matching.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodecError {
    #[error("codec mismatch: expected {expected:?}, got {got:?}")]
    CodecMismatch { expected: CodecKind, got: CodecKind },

    #[error("crc32c mismatch (chunk corruption?): expected {expected:#010x}, got {got:#010x}")]
    CrcMismatch { expected: u32, got: u32 },

    #[error("compressed size mismatch: manifest says {expected} bytes, payload is {got} bytes")]
    SizeMismatch { expected: u64, got: u64 },

    #[error("compression backend error: {0}")]
    Backend(#[from] anyhow::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("blocking-task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("codec {0:?} is not registered in this CodecRegistry")]
    UnregisteredCodec(CodecKind),

    /// v0.8.4 #73 M2: streaming compress consumed fewer input bytes than the
    /// caller advertised (typically a client disconnect mid-PUT). Surfaced
    /// from `streaming::streaming_compress_to_frames` when its
    /// `expected_size = Some(n)` argument is supplied; the s4-server PUT
    /// handler maps this to a 400 BadRequest so the client cannot rely on
    /// silent success of a half-uploaded body.
    #[error("streaming compress truncated: expected {expected} input bytes, got {got}")]
    TruncatedStream { expected: u64, got: u64 },

    /// v0.8.15 M-4: the client advertised a `Content-Length` of `expected`
    /// bytes but kept feeding the gateway data past that point. AWS S3
    /// returns `IncompleteBody` / `RequestBodyLengthMismatch` for the
    /// same shape (under-length is `TruncatedStream`, over-length is
    /// this variant). The s4-server PUT handler maps both to 400 so a
    /// client retry can succeed instead of silently storing the
    /// truncated-at-the-listener body.
    #[error("streaming compress over-length: expected {expected} input bytes, got at least {got}")]
    OverlengthStream { expected: u64, got: u64 },

    /// v0.8.5 #83 H-3: nvCOMP decompress refused to honour a manifest whose
    /// `original_size` exceeds the safety ceiling (default 5 GiB — AWS S3
    /// single-PUT max). Without this gate, a forged or corrupted manifest
    /// can drive a `Vec::with_capacity(huge)` and trip an OOM before the
    /// CRC check ever runs. Distinct from `SizeMismatch` because here the
    /// manifest itself is rejected pre-allocation rather than a
    /// post-decompress length comparison.
    #[error(
        "manifest original_size {requested} exceeds safety limit {limit} \
         (forged / corrupted manifest?)"
    )]
    ManifestSizeExceedsLimit { requested: u64, limit: u64 },

    /// v0.8.5 #83 H-3: nvCOMP decompress saw a manifest whose
    /// `compressed_size` field disagrees with the actual input payload
    /// length. Surfaced before allocation so a forged header can't drive
    /// a sized read against truncated or padded input. Distinct from
    /// `SizeMismatch` (which is the post-decompress original-size check):
    /// this is a pre-flight check on the *compressed* side.
    #[error("manifest compressed_size {manifest} does not match payload length {actual}")]
    ManifestSizeMismatch { manifest: u64, actual: u64 },
}

/// v0.8.6 #89: maximum decompressed payload size honoured at decompress
/// entry by every codec. Manifests claiming a larger `original_size` are
/// rejected pre-allocation as forged / corrupted, so a malicious manifest
/// cannot drive `Vec::with_capacity(huge)` into an OOM (memory-DoS)
/// before the CRC check ever runs.
///
/// Was `nvcomp::MAX_DECOMPRESSED_BYTES` (v0.8.5 #83), promoted to
/// `s4_codec::MAX_DECOMPRESSED_BYTES` so CPU codecs (CpuZstd / CpuGzip)
/// share the exact same ceiling — the continuous fuzz farm hit OOM in
/// `cpu_zstd_decompress_bolero` (issue #89) within minutes because the
/// CPU codecs were doing `Vec::with_capacity(manifest.original_size)`
/// before this guard had been promoted out of the GPU-only module.
///
/// Rationale for 5 GiB: matches AWS S3's documented single-PUT object
/// ceiling (`PUT Object` is capped at 5 GiB; bigger payloads must use
/// multipart upload, which is split into ≤5 GiB parts). Real S4 chunks
/// are bounded by the same ceiling end-to-end, so a manifest whose
/// `original_size` exceeds it cannot have come from a well-formed S4 PUT.
pub const MAX_DECOMPRESSED_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// v0.8.6 #89: bootstrap capacity for the decompressed-output `Vec` so
/// the `Vec::with_capacity(original_size)` pre-allocation can no longer
/// be driven into RSS-OOM by a forged manifest. Small enough (1 MiB)
/// that even an attacker claiming `original_size = u32::MAX` only
/// reserves 1 MiB up front; `read_to_end` grows the buffer as actual
/// decompressed bytes arrive (capped at `manifest.original_size + 1024`
/// by the existing decompression-bomb guard).
///
/// Why not `Vec::new()` (= 0 capacity)? `read_to_end` would grow the
/// buffer via doubling, producing ~20 reallocations + memcpys for a
/// typical 1 MiB chunk. 1 MiB pre-alloc skips those for the common
/// small-chunk case while keeping the worst-case adversarial alloc
/// flat at 1 MiB.
pub const DECOMPRESS_BOOTSTRAP_CAPACITY: usize = 1 << 20; // 1 MiB

/// v0.8.6 #89: shared pre-allocation manifest validator invoked by every
/// decompress path (CpuZstd / CpuGzip / nvCOMP Zstd / Bitcomp /
/// GDeflate). Centralising the check keeps every decompress site using
/// identical limits and error shapes, so one missed update can't
/// reintroduce the alloc-before-validate bug. Returns the
/// `usize`-narrowed `original_size` ready for `Vec::with_capacity`, or a
/// typed `CodecError` the caller propagates verbatim.
///
/// Was `nvcomp::validate_decompress_manifest` (v0.8.5 #83). Promoted
/// out of the `#[cfg(any(feature = "nvcomp-gpu", test))]` gate so CPU
/// codecs can call it unconditionally.
pub fn validate_decompress_manifest(
    manifest: &ChunkManifest,
    actual_compressed_len: usize,
) -> Result<usize, CodecError> {
    if manifest.original_size > MAX_DECOMPRESSED_BYTES {
        return Err(CodecError::ManifestSizeExceedsLimit {
            requested: manifest.original_size,
            limit: MAX_DECOMPRESSED_BYTES,
        });
    }
    if manifest.compressed_size != actual_compressed_len as u64 {
        return Err(CodecError::ManifestSizeMismatch {
            manifest: manifest.compressed_size,
            actual: actual_compressed_len as u64,
        });
    }
    usize::try_from(manifest.original_size).map_err(|_| CodecError::ManifestSizeExceedsLimit {
        requested: manifest.original_size,
        limit: usize::MAX as u64,
    })
}

/// pluggable な圧縮 backend trait。
///
/// すべて async — GPU codec は CUDA stream に await でき、CPU codec は
/// `spawn_blocking` で別スレッドへ逃がす。
#[async_trait::async_trait]
pub trait Codec: Send + Sync {
    /// この実装が提供する codec の種類
    fn kind(&self) -> CodecKind;

    /// 圧縮: 入力 bytes → 圧縮済 bytes + manifest
    async fn compress(&self, input: Bytes) -> Result<(Bytes, ChunkManifest), CodecError>;

    /// 解凍: 圧縮済 bytes + manifest → 元の bytes
    async fn decompress(&self, input: Bytes, manifest: &ChunkManifest)
    -> Result<Bytes, CodecError>;
}

pub use dispatcher::CodecDispatcher;
