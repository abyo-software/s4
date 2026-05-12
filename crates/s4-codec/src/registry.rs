//! 複数 Codec を保持し、`CodecKind` ベースで dispatch するレジストリ。
//!
//! S4Service は単一 codec ではなく `Arc<CodecRegistry>` を持つことで、
//!
//! - PUT 時: dispatcher が選んだ `CodecKind` で `compress` を呼ぶ
//! - GET 時: object metadata から復元した manifest.codec で `decompress` を呼ぶ
//!
//! を可能にする。これによりひとつの S4 インスタンスが複数 codec の混在オブジェクトを
//! 透過的に扱えるようになり、Phase 1 で抱えていた「codec mismatch エラー」を解消する。

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;

use crate::{ChunkManifest, Codec, CodecError, CodecKind};

/// codec dispatch レジストリ。`Arc` 越しに S4Service / 複数タスクから共有する想定。
pub struct CodecRegistry {
    codecs: HashMap<CodecKind, Arc<dyn Codec>>,
    default: CodecKind,
}

impl CodecRegistry {
    /// `default` で指定した codec が PUT 時の codec として使われる
    /// (dispatcher が別 kind を選んだ場合は、その kind が登録されていれば優先)。
    pub fn new(default: CodecKind) -> Self {
        Self {
            codecs: HashMap::new(),
            default,
        }
    }

    /// codec を登録。同じ kind を 2 度登録すると後勝ち。
    pub fn register(&mut self, codec: Arc<dyn Codec>) -> &mut Self {
        self.codecs.insert(codec.kind(), codec);
        self
    }

    /// `register` の chain 用 builder
    #[must_use]
    pub fn with(mut self, codec: Arc<dyn Codec>) -> Self {
        self.register(codec);
        self
    }

    /// 登録済 kind 一覧
    pub fn kinds(&self) -> impl Iterator<Item = CodecKind> + '_ {
        self.codecs.keys().copied()
    }

    /// default kind
    pub fn default_kind(&self) -> CodecKind {
        self.default
    }

    fn lookup(&self, kind: CodecKind) -> Result<&Arc<dyn Codec>, CodecError> {
        self.codecs
            .get(&kind)
            .ok_or(CodecError::UnregisteredCodec(kind))
    }

    /// 指定 kind の codec で compress
    pub async fn compress(
        &self,
        input: Bytes,
        kind: CodecKind,
    ) -> Result<(Bytes, ChunkManifest), CodecError> {
        let codec = self.lookup(kind)?;
        codec.compress(input).await
    }

    /// manifest が指す codec で decompress (本命の dispatch path)
    pub async fn decompress(
        &self,
        input: Bytes,
        manifest: &ChunkManifest,
    ) -> Result<Bytes, CodecError> {
        let codec = self.lookup(manifest.codec)?;
        codec.decompress(input, manifest).await
    }
}

impl std::fmt::Debug for CodecRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut kinds: Vec<&CodecKind> = self.codecs.keys().collect();
        kinds.sort_unstable_by_key(|k| k.as_str());
        f.debug_struct("CodecRegistry")
            .field("default", &self.default)
            .field("registered", &kinds)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_zstd::CpuZstd;
    use crate::passthrough::Passthrough;

    fn registry() -> CodecRegistry {
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default()))
    }

    #[tokio::test]
    async fn dispatches_compress_by_kind() {
        let r = registry();
        let input = Bytes::from(vec![b'a'; 1024]);

        let (compressed_pt, manifest_pt) = r
            .compress(input.clone(), CodecKind::Passthrough)
            .await
            .unwrap();
        assert_eq!(manifest_pt.codec, CodecKind::Passthrough);
        assert_eq!(compressed_pt.len(), input.len());

        let (compressed_zstd, manifest_zstd) =
            r.compress(input.clone(), CodecKind::CpuZstd).await.unwrap();
        assert_eq!(manifest_zstd.codec, CodecKind::CpuZstd);
        assert!(compressed_zstd.len() < input.len() / 5);
    }

    #[tokio::test]
    async fn dispatches_decompress_by_manifest() {
        let r = registry();
        let input = Bytes::from(vec![b'a'; 1024]);
        let (compressed, manifest) = r.compress(input.clone(), CodecKind::CpuZstd).await.unwrap();
        let decompressed = r.decompress(compressed, &manifest).await.unwrap();
        assert_eq!(decompressed, input);
    }

    #[tokio::test]
    async fn unregistered_codec_yields_error() {
        let r = registry();
        let bogus_manifest = ChunkManifest {
            codec: CodecKind::NvcompBitcomp,
            original_size: 10,
            compressed_size: 10,
            crc32c: 0,
        };
        let err = r
            .decompress(Bytes::from_static(b"0123456789"), &bogus_manifest)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CodecError::UnregisteredCodec(CodecKind::NvcompBitcomp)
        ));
    }
}
