//! S4 圧縮 codec layer。バックエンドを差し替え可能にする中立 trait を提供する。
//!
//! ## 採用 backend (2026-05 検討)
//!
//! - **nvCOMP** (NVIDIA proprietary、要 license 確認): Bitcomp / gANS / zstd-GPU
//! - **DietGPU** (Meta, MIT): ANS-only、license clean な fallback
//! - **CPU zstd**: GPU 無し環境向け究極の fallback

use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub mod cpu_zstd;
pub mod dietgpu;
pub mod nvcomp;
pub mod passthrough;

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

/// 圧縮済 chunk のメタ情報。S3 オブジェクトの metadata に格納される。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkManifest {
    pub codec: CodecKind,
    pub original_size: u64,
    pub compressed_size: u64,
    pub crc32c: u32,
}

/// pluggable な圧縮 backend trait。
///
/// すべて async — GPU codec は CUDA stream に await できる。
#[async_trait::async_trait]
pub trait Codec: Send + Sync {
    /// この実装が提供する codec の種類
    fn kind(&self) -> CodecKind;

    /// 圧縮: 入力 bytes → 圧縮済 bytes + manifest
    async fn compress(&self, input: Bytes) -> anyhow::Result<(Bytes, ChunkManifest)>;

    /// 解凍: 圧縮済 bytes + manifest → 元の bytes
    async fn decompress(&self, input: Bytes, manifest: &ChunkManifest) -> anyhow::Result<Bytes>;
}

/// データ種別から最適 codec を選ぶ dispatcher (Phase 1 後半で実装)。
///
/// 入力先頭の sampling で integer 主体 / text 主体 / binary 既圧縮を判定する。
#[async_trait::async_trait]
pub trait CodecDispatcher: Send + Sync {
    async fn pick(&self, sample: &[u8]) -> CodecKind;
}
