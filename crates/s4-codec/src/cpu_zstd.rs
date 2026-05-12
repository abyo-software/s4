//! CPU zstd backend — GPU 非搭載環境向け究極の fallback、および test bed。
//!
//! - `zstd` crate (`zstd-safe` + `zstd-sys`、Apache-2.0 OR MIT) を使った直球実装
//! - 圧縮処理は CPU 重量級なので `tokio::task::spawn_blocking` で別スレッドへ逃がす
//! - production では nvCOMP より遅いが、機能 / wire 互換 test の常設レーンとして必須

use bytes::Bytes;

use crate::{ChunkManifest, Codec, CodecError, CodecKind};

/// CPU zstd codec。`level` は 1..=22 (zstd-22 は最大圧縮率、時間は長い)。
///
/// S4 default は `3` (zstd の通常 default、速度と圧縮率のバランス)。
#[derive(Debug, Clone)]
pub struct CpuZstd {
    level: i32,
}

impl CpuZstd {
    pub const DEFAULT_LEVEL: i32 = 3;

    pub fn new(level: i32) -> Self {
        Self {
            level: level.clamp(1, 22),
        }
    }
}

impl Default for CpuZstd {
    fn default() -> Self {
        Self::new(Self::DEFAULT_LEVEL)
    }
}

#[async_trait::async_trait]
impl Codec for CpuZstd {
    fn kind(&self) -> CodecKind {
        CodecKind::CpuZstd
    }

    async fn compress(&self, input: Bytes) -> Result<(Bytes, ChunkManifest), CodecError> {
        let level = self.level;
        let original_size = input.len() as u64;
        let original_crc = crc32c::crc32c(&input);

        let compressed = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            zstd::stream::encode_all(input.as_ref(), level)
        })
        .await??;

        let compressed_size = compressed.len() as u64;
        let manifest = ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size,
            compressed_size,
            crc32c: original_crc,
        };
        Ok((Bytes::from(compressed), manifest))
    }

    async fn decompress(
        &self,
        input: Bytes,
        manifest: &ChunkManifest,
    ) -> Result<Bytes, CodecError> {
        if manifest.codec != CodecKind::CpuZstd {
            return Err(CodecError::CodecMismatch {
                expected: CodecKind::CpuZstd,
                got: manifest.codec,
            });
        }
        if input.len() as u64 != manifest.compressed_size {
            return Err(CodecError::SizeMismatch {
                expected: manifest.compressed_size,
                got: input.len() as u64,
            });
        }

        let expected_crc = manifest.crc32c;
        let expected_orig_size = manifest.original_size;

        // **Zstd decompression bomb hardening**: 信頼できない入力 (改ざんされた
        // sidecar / S3 上で bit flip / 攻撃者操作) で `decode_all` が無制限に
        // 出力を伸ばすと OOM するので、`expected_orig_size + small margin` で
        // 上限を hard-cap する。Decoder + Read::take パターンで実装。
        let decompressed = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            // 1 KiB margin: zstd の internal buffer flush で多少 overshoot しても
            // 検出できる余地を残す。expected_orig_size + margin を超えたら
            // bomb 認定して error にする
            let limit = expected_orig_size.saturating_add(1024);
            let mut decoder = zstd::stream::Decoder::new(input.as_ref())?;
            let mut buf = Vec::with_capacity(expected_orig_size as usize);
            (&mut decoder).take(limit).read_to_end(&mut buf)?;
            // limit 以上を消費したかチェック (= bomb)
            if (buf.len() as u64) > expected_orig_size {
                return Err(std::io::Error::other(format!(
                    "zstd decompression bomb detected: produced {} bytes, manifest claimed {}",
                    buf.len(),
                    expected_orig_size
                )));
            }
            Ok(buf)
        })
        .await??;

        if decompressed.len() as u64 != expected_orig_size {
            return Err(CodecError::SizeMismatch {
                expected: expected_orig_size,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_small() {
        let codec = CpuZstd::default();
        let input = Bytes::from_static(b"hello squished s3 hello squished s3 hello squished s3");
        let (compressed, manifest) = codec.compress(input.clone()).await.unwrap();
        // small string compresses small but not necessarily smaller
        assert_eq!(manifest.codec, CodecKind::CpuZstd);
        assert_eq!(manifest.original_size, input.len() as u64);
        let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
        assert_eq!(decompressed, input);
    }

    #[tokio::test]
    async fn roundtrip_compressible() {
        let codec = CpuZstd::default();
        // highly compressible payload (1 MB of repeated pattern)
        let input = Bytes::from(vec![b'x'; 1024 * 1024]);
        let (compressed, manifest) = codec.compress(input.clone()).await.unwrap();
        assert!(
            compressed.len() < input.len() / 100,
            "expected zstd to compress 1 MiB of x bytes very well, got {} bytes",
            compressed.len()
        );
        let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
        assert_eq!(decompressed, input);
    }

    #[tokio::test]
    async fn detects_corrupted_compressed_payload() {
        let codec = CpuZstd::default();
        let input = Bytes::from(vec![b'x'; 1024]);
        let (mut compressed, manifest) = codec.compress(input).await.unwrap();
        // flip a byte mid-payload
        let mut buf = compressed.to_vec();
        if buf.len() > 8 {
            buf[5] ^= 0xff;
        }
        compressed = Bytes::from(buf);
        let err = codec.decompress(compressed, &manifest).await.unwrap_err();
        // either zstd refuses to decode (Io) or crc check catches it (CrcMismatch)
        assert!(matches!(
            err,
            CodecError::Io(_) | CodecError::CrcMismatch { .. } | CodecError::SizeMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn rejects_codec_mismatch() {
        let codec = CpuZstd::default();
        let manifest = ChunkManifest {
            codec: CodecKind::Passthrough,
            original_size: 10,
            compressed_size: 10,
            crc32c: 0,
        };
        let err = codec
            .decompress(Bytes::from_static(b"0123456789"), &manifest)
            .await
            .unwrap_err();
        assert!(matches!(err, CodecError::CodecMismatch { .. }));
    }
}
