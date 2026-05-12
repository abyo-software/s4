//! nvCOMP (NVIDIA proprietary) backend ラッパー。
//!
//! ## 設計方針 (2026-05-12 確定)
//!
//! - **vendored ferro-compress 経由**: `crates/s4-codec/vendor/ferro-compress` (Apache-2.0
//!   OR MIT) に nvCOMP の Rust binding を vendoring 済。本 module はそれを async な
//!   [`crate::Codec`] trait に bridge する薄い adapter。
//! - **feature gate**: `nvcomp-gpu` feature を opt-in にすることで、CUDA toolchain と
//!   NVCOMP_HOME が無い環境でも default build (cargo check / test) が green に保たれる。
//! - **配布形態**: nvCOMP redist は NVIDIA SLA 制約あり。Phase 1 は **BYO 方式**
//!   (顧客が NGC からダウンロード) を default、AMI 同梱は NVIDIA 書面確認後に判断。
//!
//! ## 提供 codec
//!
//! - [`NvcompZstdCodec`]: nvCOMP zstd-GPU。汎用 text / log。
//! - [`NvcompBitcompCodec`]: nvCOMP Bitcomp。整数列 (Parquet 数値列、time-series)。
//!
//! ## ビルド方法
//!
//! ```bash
//! export NVCOMP_HOME=/path/to/nvcomp-linux-x86_64-5.x.x.x_cuda12-archive
//! cargo build --features nvcomp-gpu
//! cargo test --features nvcomp-gpu -- --ignored  # GPU 必須テスト
//! ```

#[cfg(feature = "nvcomp-gpu")]
mod imp {
    use std::sync::Arc;

    use bytes::Bytes;
    use ferro_compress_vendored::{Algo, BitcompDataType, Codec as FerroCodec, NvcompCodec};

    use crate::{ChunkManifest, Codec, CodecError, CodecKind};

    /// nvCOMP zstd-GPU を S4 の `Codec` trait に bridge。
    pub struct NvcompZstdCodec {
        inner: Arc<NvcompCodec>,
    }

    impl NvcompZstdCodec {
        pub fn new() -> Result<Self, CodecError> {
            let inner = NvcompCodec::new(Algo::Zstd)
                .map_err(|e| CodecError::Backend(anyhow::anyhow!("nvcomp zstd init: {e}")))?;
            Ok(Self {
                inner: Arc::new(inner),
            })
        }
    }

    #[async_trait::async_trait]
    impl Codec for NvcompZstdCodec {
        fn kind(&self) -> CodecKind {
            CodecKind::NvcompZstd
        }

        async fn compress(&self, input: Bytes) -> Result<(Bytes, ChunkManifest), CodecError> {
            let original_size = input.len() as u64;
            let original_crc = crc32c::crc32c(&input);
            let codec = Arc::clone(&self.inner);
            let compressed = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CodecError> {
                let mut out = Vec::with_capacity(codec.max_compressed_len(input.len()));
                codec.compress(input.as_ref(), &mut out).map_err(|e| {
                    CodecError::Backend(anyhow::anyhow!("nvcomp zstd compress: {e}"))
                })?;
                Ok(out)
            })
            .await??;
            let manifest = ChunkManifest {
                codec: CodecKind::NvcompZstd,
                original_size,
                compressed_size: compressed.len() as u64,
                crc32c: original_crc,
            };
            Ok((Bytes::from(compressed), manifest))
        }

        async fn decompress(
            &self,
            input: Bytes,
            manifest: &ChunkManifest,
        ) -> Result<Bytes, CodecError> {
            if manifest.codec != CodecKind::NvcompZstd {
                return Err(CodecError::CodecMismatch {
                    expected: CodecKind::NvcompZstd,
                    got: manifest.codec,
                });
            }
            let expected_crc = manifest.crc32c;
            let expected_orig_size = manifest.original_size as usize;
            let codec = Arc::clone(&self.inner);
            let decompressed =
                tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CodecError> {
                    let mut out = Vec::with_capacity(expected_orig_size);
                    codec.decompress(input.as_ref(), &mut out).map_err(|e| {
                        CodecError::Backend(anyhow::anyhow!("nvcomp zstd decompress: {e}"))
                    })?;
                    Ok(out)
                })
                .await??;
            if decompressed.len() != expected_orig_size {
                return Err(CodecError::SizeMismatch {
                    expected: manifest.original_size,
                    got: decompressed.len() as u64,
                });
            }
            let actual_crc = crc32c::crc32c(&decompressed);
            if actual_crc != expected_crc {
                return Err(CodecError::CrcMismatch {
                    expected: expected_crc,
                    got: actual_crc,
                });
            }
            Ok(Bytes::from(decompressed))
        }
    }

    /// nvCOMP Bitcomp を S4 の `Codec` trait に bridge。整数列に最適化。
    pub struct NvcompBitcompCodec {
        inner: Arc<NvcompCodec>,
    }

    impl NvcompBitcompCodec {
        /// `data_type` で nvCOMP の bit-packing / delta layout が決まる。
        /// 整数列 / float 列で適切に使い分ける必要がある (Char 汎用は圧縮率が落ちる)。
        pub fn new(data_type: BitcompDataType) -> Result<Self, CodecError> {
            let inner = NvcompCodec::new(Algo::Bitcomp { data_type })
                .map_err(|e| CodecError::Backend(anyhow::anyhow!("nvcomp bitcomp init: {e}")))?;
            Ok(Self {
                inner: Arc::new(inner),
            })
        }

        /// デフォルト: data_type=Char (バイト列汎用)
        pub fn default_general() -> Result<Self, CodecError> {
            Self::new(BitcompDataType::Char)
        }
    }

    #[async_trait::async_trait]
    impl Codec for NvcompBitcompCodec {
        fn kind(&self) -> CodecKind {
            CodecKind::NvcompBitcomp
        }

        async fn compress(&self, input: Bytes) -> Result<(Bytes, ChunkManifest), CodecError> {
            let original_size = input.len() as u64;
            let original_crc = crc32c::crc32c(&input);
            let codec = Arc::clone(&self.inner);
            let compressed = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CodecError> {
                let mut out = Vec::with_capacity(codec.max_compressed_len(input.len()));
                codec.compress(input.as_ref(), &mut out).map_err(|e| {
                    CodecError::Backend(anyhow::anyhow!("nvcomp bitcomp compress: {e}"))
                })?;
                Ok(out)
            })
            .await??;
            let manifest = ChunkManifest {
                codec: CodecKind::NvcompBitcomp,
                original_size,
                compressed_size: compressed.len() as u64,
                crc32c: original_crc,
            };
            Ok((Bytes::from(compressed), manifest))
        }

        async fn decompress(
            &self,
            input: Bytes,
            manifest: &ChunkManifest,
        ) -> Result<Bytes, CodecError> {
            if manifest.codec != CodecKind::NvcompBitcomp {
                return Err(CodecError::CodecMismatch {
                    expected: CodecKind::NvcompBitcomp,
                    got: manifest.codec,
                });
            }
            let expected_crc = manifest.crc32c;
            let expected_orig_size = manifest.original_size as usize;
            let codec = Arc::clone(&self.inner);
            let decompressed =
                tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CodecError> {
                    let mut out = Vec::with_capacity(expected_orig_size);
                    codec.decompress(input.as_ref(), &mut out).map_err(|e| {
                        CodecError::Backend(anyhow::anyhow!("nvcomp bitcomp decompress: {e}"))
                    })?;
                    Ok(out)
                })
                .await??;
            if decompressed.len() != expected_orig_size {
                return Err(CodecError::SizeMismatch {
                    expected: manifest.original_size,
                    got: decompressed.len() as u64,
                });
            }
            let actual_crc = crc32c::crc32c(&decompressed);
            if actual_crc != expected_crc {
                return Err(CodecError::CrcMismatch {
                    expected: expected_crc,
                    got: actual_crc,
                });
            }
            Ok(Bytes::from(decompressed))
        }
    }

    /// CUDA-capable な GPU が runtime に存在するか
    pub fn is_gpu_available() -> bool {
        NvcompCodec::is_available()
    }
}

#[cfg(feature = "nvcomp-gpu")]
pub use imp::{NvcompBitcompCodec, NvcompZstdCodec, is_gpu_available};

#[cfg(not(feature = "nvcomp-gpu"))]
pub fn is_gpu_available() -> bool {
    false
}

#[cfg(all(test, feature = "nvcomp-gpu"))]
mod tests {
    use super::*;
    use crate::Codec;
    use bytes::Bytes;

    #[tokio::test]
    #[ignore = "requires CUDA-capable GPU + NVCOMP_HOME at build time"]
    async fn nvcomp_zstd_roundtrip() {
        if !is_gpu_available() {
            eprintln!("skipping: no CUDA GPU detected at runtime");
            return;
        }
        let codec = NvcompZstdCodec::new().expect("init");
        let input = Bytes::from(vec![b'a'; 100_000]);
        let (compressed, manifest) = codec.compress(input.clone()).await.expect("compress");
        assert!(compressed.len() < input.len() / 10);
        let decompressed = codec
            .decompress(compressed, &manifest)
            .await
            .expect("decompress");
        assert_eq!(decompressed, input);
    }

    #[tokio::test]
    #[ignore = "requires CUDA-capable GPU + NVCOMP_HOME at build time"]
    async fn nvcomp_bitcomp_roundtrip_on_integer_column() {
        if !is_gpu_available() {
            eprintln!("skipping: no CUDA GPU detected at runtime");
            return;
        }
        let codec = NvcompBitcompCodec::default_general().expect("init");
        // Parquet 風の単調増加 i64 列 (8 KB 分 = 1024 elements)
        let mut payload: Vec<u8> = Vec::with_capacity(8192);
        for i in 0i64..1024 {
            payload.extend_from_slice(&i.to_le_bytes());
        }
        let input = Bytes::from(payload);
        let (compressed, manifest) = codec.compress(input.clone()).await.expect("compress");
        // Bitcomp は単調整数で 3.6-7.5x 圧縮を期待 (Phase 0 実測値)
        assert!(
            compressed.len() < input.len() / 2,
            "bitcomp on monotone i64 should compress >2x, got {} -> {}",
            input.len(),
            compressed.len()
        );
        let decompressed = codec
            .decompress(compressed, &manifest)
            .await
            .expect("decompress");
        assert_eq!(decompressed, input);
    }
}
