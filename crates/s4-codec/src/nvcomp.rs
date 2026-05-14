//! nvCOMP (NVIDIA proprietary) backend ラッパー。
//!
//! ## 設計方針 (2026-05-12 確定)
//!
//! - **integrated ferro-compress 経由**: nvCOMP の Rust binding を s4-codec の内部
//!   module `crate::ferro_compress` (Apache-2.0 OR MIT) として物理統合済。本 module は
//!   それを async な [`crate::Codec`] trait に bridge する薄い adapter。
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

#[cfg(any(feature = "nvcomp-gpu", test))]
use crate::{ChunkManifest, CodecError};

/// v0.8.5 #83 H-3: maximum decompressed payload size honoured at
/// decompress entry. Manifests claiming a larger `original_size` are
/// rejected pre-allocation as forged / corrupted, so a malicious
/// manifest cannot drive `Vec::with_capacity(huge)` into an OOM
/// (memory-DoS) before the CRC check ever runs.
///
/// Rationale for 5 GiB: matches AWS S3's documented single-PUT object
/// ceiling (`PUT Object` is capped at 5 GiB; bigger payloads must use
/// multipart upload, which is split into ≤5 GiB parts). Real S4
/// chunks are bounded by the same ceiling end-to-end, so a manifest
/// whose `original_size` exceeds it cannot have come from a
/// well-formed S4 PUT.
#[cfg(any(feature = "nvcomp-gpu", test))]
pub const MAX_DECOMPRESSED_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// v0.8.5 #83 H-3 helper: shared pre-allocation manifest validator
/// invoked by every nvCOMP decompress path (Zstd / Bitcomp /
/// GDeflate). Centralising the check keeps the three decompress sites
/// (and any future nvCOMP codec) using identical limits and error
/// shapes, so one missed update can't reintroduce the alloc-before-
/// validate bug. Returns the `usize`-narrowed `original_size` ready
/// for `Vec::with_capacity`, or a typed `CodecError` the caller
/// propagates verbatim.
#[cfg(any(feature = "nvcomp-gpu", test))]
pub(crate) fn validate_decompress_manifest(
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
    // `u64 → usize` is lossy on 32-bit targets; reject explicitly so
    // a 3 GiB manifest doesn't truncate to ~0 bytes on wasm32 / armv7
    // and silently under-allocate the destination buffer.
    usize::try_from(manifest.original_size).map_err(|_| CodecError::ManifestSizeExceedsLimit {
        requested: manifest.original_size,
        limit: usize::MAX as u64,
    })
}

#[cfg(feature = "nvcomp-gpu")]
mod imp {
    use std::sync::Arc;

    use crate::ferro_compress::{Algo, BitcompDataType, Codec as FerroCodec, NvcompCodec};
    use bytes::Bytes;

    use crate::{ChunkManifest, Codec, CodecError, CodecKind};
    use super::validate_decompress_manifest;

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
            // v0.8.5 #83 H-3: validate manifest BEFORE allocating the
            // destination buffer. A forged manifest with a huge
            // `original_size` would otherwise drive `Vec::with_capacity`
            // straight into an OOM (memory-DoS) before the CRC check
            // ever runs, and a `u64 as usize` truncation on a 32-bit
            // target would silently under-allocate.
            let expected_crc = manifest.crc32c;
            let expected_orig_size = validate_decompress_manifest(manifest, input.len())?;
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
            // v0.8.5 #83 H-3: see NvcompZstdCodec::decompress.
            let expected_crc = manifest.crc32c;
            let expected_orig_size = validate_decompress_manifest(manifest, input.len())?;
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

    /// nvCOMP GDeflate を S4 の `Codec` trait に bridge (v0.2 #9)。
    /// DEFLATE-family GPU codec。汎用 binary、log、JSON 等に zstd と並ぶ
    /// 候補。zstd よりは圧縮率劣るが、algorithm-level format が DEFLATE
    /// 互換なので将来 wrapper を被せれば stock gunzip でも復号可能 (Phase 2)。
    pub struct NvcompGDeflateCodec {
        inner: Arc<NvcompCodec>,
    }

    impl NvcompGDeflateCodec {
        pub fn new() -> Result<Self, CodecError> {
            let inner = NvcompCodec::new(Algo::GDeflate)
                .map_err(|e| CodecError::Backend(anyhow::anyhow!("nvcomp gdeflate init: {e}")))?;
            Ok(Self {
                inner: Arc::new(inner),
            })
        }
    }

    #[async_trait::async_trait]
    impl Codec for NvcompGDeflateCodec {
        fn kind(&self) -> CodecKind {
            CodecKind::NvcompGDeflate
        }

        async fn compress(&self, input: Bytes) -> Result<(Bytes, ChunkManifest), CodecError> {
            let original_size = input.len() as u64;
            let original_crc = crc32c::crc32c(&input);
            let codec = Arc::clone(&self.inner);
            let compressed = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CodecError> {
                let mut out = Vec::with_capacity(codec.max_compressed_len(input.len()));
                codec.compress(input.as_ref(), &mut out).map_err(|e| {
                    CodecError::Backend(anyhow::anyhow!("nvcomp gdeflate compress: {e}"))
                })?;
                Ok(out)
            })
            .await??;
            let manifest = ChunkManifest {
                codec: CodecKind::NvcompGDeflate,
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
            if manifest.codec != CodecKind::NvcompGDeflate {
                return Err(CodecError::CodecMismatch {
                    expected: CodecKind::NvcompGDeflate,
                    got: manifest.codec,
                });
            }
            // v0.8.5 #83 H-3: see NvcompZstdCodec::decompress.
            let expected_crc = manifest.crc32c;
            let expected_orig_size = validate_decompress_manifest(manifest, input.len())?;
            let codec = Arc::clone(&self.inner);
            let decompressed =
                tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CodecError> {
                    let mut out = Vec::with_capacity(expected_orig_size);
                    codec.decompress(input.as_ref(), &mut out).map_err(|e| {
                        CodecError::Backend(anyhow::anyhow!("nvcomp gdeflate decompress: {e}"))
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
pub use imp::{NvcompBitcompCodec, NvcompGDeflateCodec, NvcompZstdCodec, is_gpu_available};

/// nvCOMP Bitcomp typed-column hint. Selects the bit-packing layout the
/// codec uses internally — the right choice can swing compression ratio
/// from ~1.2× (`Char`, treating numeric data as opaque bytes) to >3.5×
/// (`Uint32` on a sorted u32 posting list). Exposed publicly so callers
/// can target their column shape without going through `default_general`.
#[cfg(feature = "nvcomp-gpu")]
pub use crate::ferro_compress::BitcompDataType;

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

/// v0.8.5 #83 H-3 unit tests for the manifest pre-allocation validator.
/// These run without `nvcomp-gpu` because they exercise the
/// pure-host validation path — no CUDA runtime, no large allocations.
/// Keeps the safety guard exercised on every `cargo test -p s4-codec`
/// invocation, even on machines that can't build the full nvCOMP backend.
#[cfg(test)]
mod manifest_validate_tests {
    use super::{MAX_DECOMPRESSED_BYTES, validate_decompress_manifest};
    use crate::{ChunkManifest, CodecError, CodecKind};

    fn manifest(original: u64, compressed: u64) -> ChunkManifest {
        ChunkManifest {
            codec: CodecKind::NvcompZstd,
            original_size: original,
            compressed_size: compressed,
            crc32c: 0,
        }
    }

    #[test]
    fn decompress_rejects_manifest_original_size_over_limit() {
        // Forged manifest claiming 6 GiB of decompressed output —
        // would otherwise have allocated `Vec::with_capacity(6 GiB)`
        // and tripped an OOM before any CRC check.
        let m = manifest(MAX_DECOMPRESSED_BYTES + 1, 1024);
        let err = validate_decompress_manifest(&m, 1024).unwrap_err();
        match err {
            CodecError::ManifestSizeExceedsLimit { requested, limit } => {
                assert_eq!(requested, MAX_DECOMPRESSED_BYTES + 1);
                assert_eq!(limit, MAX_DECOMPRESSED_BYTES);
            }
            other => panic!("expected ManifestSizeExceedsLimit, got {other:?}"),
        }
    }

    #[test]
    fn decompress_rejects_manifest_compressed_size_mismatch() {
        // Forged manifest whose compressed_size disagrees with the
        // actual payload length — fails fast pre-allocation so a
        // truncated / padded payload cannot drive a sized read.
        let m = manifest(1024, 2048);
        let err = validate_decompress_manifest(&m, 1024).unwrap_err();
        match err {
            CodecError::ManifestSizeMismatch {
                manifest: m_size,
                actual,
            } => {
                assert_eq!(m_size, 2048);
                assert_eq!(actual, 1024);
            }
            other => panic!("expected ManifestSizeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decompress_validates_well_formed_manifest() {
        // Sanity: a manifest whose original_size is at the ceiling
        // and whose compressed_size matches is accepted.
        let m = manifest(MAX_DECOMPRESSED_BYTES, 1024);
        let n = validate_decompress_manifest(&m, 1024)
            .expect("well-formed manifest at the ceiling must pass");
        assert_eq!(n as u64, MAX_DECOMPRESSED_BYTES);
    }

    /// v0.8.5 #83 H-3: on a 32-bit target a `u64 → usize` cast can
    /// truncate a multi-GiB manifest down to a few hundred MiB,
    /// silently under-allocating the destination buffer. The explicit
    /// `usize::try_from` arm in `validate_decompress_manifest` closes
    /// that bug. We can't flip the host pointer width, but we can
    /// assert the limit-check arm catches anything the try_from would
    /// have rejected on either pointer width — on 32-bit
    /// `MAX_DECOMPRESSED_BYTES` already exceeds `usize::MAX`, so the
    /// limit arm catches it first; on 64-bit the limit guards the
    /// same value-space below `usize::MAX`. Either way the
    /// alloc-before-validate / silent-truncation bug class stays
    /// closed.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn decompress_rejects_u64_to_usize_overflow_on_32bit_targets() {
        // 5 GiB > u32::MAX (~4 GiB), so the limit check fires first.
        let m = manifest(MAX_DECOMPRESSED_BYTES, 1024);
        let err = validate_decompress_manifest(&m, 1024).unwrap_err();
        assert!(matches!(err, CodecError::ManifestSizeExceedsLimit { .. }));
    }

    /// 64-bit dual: validate that the entire safe range narrows
    /// cleanly under `usize::try_from`. The 32-bit gating arm above
    /// carries the forge-the-truncation contract on platforms where
    /// it matters; on 64-bit the conversion can't fail for any value
    /// within the accepted range.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn decompress_rejects_u64_to_usize_overflow_on_32bit_targets() {
        let m = manifest(MAX_DECOMPRESSED_BYTES, 16);
        let n = validate_decompress_manifest(&m, 16).expect("limit value narrows on 64-bit");
        assert_eq!(n as u64, MAX_DECOMPRESSED_BYTES);
    }
}
