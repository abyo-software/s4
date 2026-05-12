//! 無圧縮 codec — テストおよび圧縮無効化フラグ用。
//!
//! crc32c は実 (SSE 4.2 / ARM CRC instruction を `crc32c` crate が利用) を入れているので
//! manifest の改ざん / S3 上の bit rot は確実に検出できる。

use bytes::Bytes;

use crate::{ChunkManifest, Codec, CodecError, CodecKind};

pub struct Passthrough;

#[async_trait::async_trait]
impl Codec for Passthrough {
    fn kind(&self) -> CodecKind {
        CodecKind::Passthrough
    }

    async fn compress(&self, input: Bytes) -> Result<(Bytes, ChunkManifest), CodecError> {
        let len = input.len() as u64;
        let crc = crc32c::crc32c(&input);
        Ok((
            input,
            ChunkManifest {
                codec: CodecKind::Passthrough,
                original_size: len,
                compressed_size: len,
                crc32c: crc,
            },
        ))
    }

    async fn decompress(
        &self,
        input: Bytes,
        manifest: &ChunkManifest,
    ) -> Result<Bytes, CodecError> {
        if manifest.codec != CodecKind::Passthrough {
            return Err(CodecError::CodecMismatch {
                expected: CodecKind::Passthrough,
                got: manifest.codec,
            });
        }
        let crc = crc32c::crc32c(&input);
        if crc != manifest.crc32c {
            return Err(CodecError::CrcMismatch {
                expected: manifest.crc32c,
                got: crc,
            });
        }
        if input.len() as u64 != manifest.compressed_size {
            return Err(CodecError::SizeMismatch {
                expected: manifest.compressed_size,
                got: input.len() as u64,
            });
        }
        Ok(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_preserves_bytes() {
        let codec = Passthrough;
        let input = Bytes::from_static(b"hello squished s3");
        let (compressed, manifest) = codec.compress(input.clone()).await.unwrap();
        assert_eq!(compressed, input);
        assert_eq!(manifest.codec, CodecKind::Passthrough);
        assert_eq!(manifest.original_size, input.len() as u64);
        assert_eq!(manifest.compressed_size, input.len() as u64);
        assert_ne!(manifest.crc32c, 0, "crc32c must not be the stub zero");
        let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
        assert_eq!(decompressed, input);
    }

    #[tokio::test]
    async fn detects_corrupted_payload() {
        let codec = Passthrough;
        let original = Bytes::from_static(b"hello squished s3");
        let (_, manifest) = codec.compress(original.clone()).await.unwrap();
        let corrupted = Bytes::from_static(b"hello SQUISHED s3");
        let err = codec.decompress(corrupted, &manifest).await.unwrap_err();
        assert!(matches!(err, CodecError::CrcMismatch { .. }));
    }

    #[tokio::test]
    async fn rejects_codec_mismatch() {
        let codec = Passthrough;
        let original = Bytes::from_static(b"hello");
        let (_, mut manifest) = codec.compress(original.clone()).await.unwrap();
        manifest.codec = CodecKind::CpuZstd;
        let err = codec.decompress(original, &manifest).await.unwrap_err();
        assert!(matches!(err, CodecError::CodecMismatch { .. }));
    }
}
