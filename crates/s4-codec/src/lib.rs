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

pub mod cpu_zstd;
pub mod dietgpu;
pub mod dispatcher;
pub mod multipart;
pub mod nvcomp;
pub mod passthrough;
pub mod registry;

pub use registry::CodecRegistry;

/// 圧縮 codec の種類 (manifest に記録、後段の decompress で codec を確定するために使う)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodecKind {
    Passthrough,
    NvcompBitcomp,
    NvcompGans,
    NvcompZstd,
    DietGpuAns,
    CpuZstd,
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
        }
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

/// codec 操作のエラー型。`anyhow::Error` ではなく専用型にすることで、上位 (S4Service) が
/// HTTP エラーコードを意味的に出し分けやすくする。
#[derive(Debug, Error)]
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
