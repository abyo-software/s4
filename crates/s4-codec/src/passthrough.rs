//! 無圧縮 codec — テストおよび圧縮無効化フラグ用

use bytes::Bytes;

use crate::{ChunkManifest, Codec, CodecKind};

pub struct Passthrough;

#[async_trait::async_trait]
impl Codec for Passthrough {
    fn kind(&self) -> CodecKind {
        CodecKind::Passthrough
    }

    async fn compress(&self, input: Bytes) -> anyhow::Result<(Bytes, ChunkManifest)> {
        let len = input.len() as u64;
        let crc = crc32c(&input);
        let manifest = ChunkManifest {
            codec: CodecKind::Passthrough,
            original_size: len,
            compressed_size: len,
            crc32c: crc,
        };
        Ok((input, manifest))
    }

    async fn decompress(&self, input: Bytes, manifest: &ChunkManifest) -> anyhow::Result<Bytes> {
        anyhow::ensure!(
            manifest.codec == CodecKind::Passthrough,
            "manifest codec mismatch: expected Passthrough, got {:?}",
            manifest.codec
        );
        let crc = crc32c(&input);
        anyhow::ensure!(
            crc == manifest.crc32c,
            "crc32c mismatch on passthrough decompress (chunk corruption?): expected {}, got {}",
            manifest.crc32c,
            crc,
        );
        Ok(input)
    }
}

fn crc32c(_data: &[u8]) -> u32 {
    // TODO Phase 1: 実 crc32c (crc32fast or sse4.2 instruction) を入れる。
    // 現状はスタブで 0 固定 — manifest 検証ロジックの shape のみ担保。
    0
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
        let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
        assert_eq!(decompressed, input);
    }
}
