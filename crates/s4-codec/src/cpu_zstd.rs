//! CPU zstd backend — GPU 非搭載環境向け究極の fallback、および test bed。
//!
//! - `zstd` crate (`zstd-safe` + `zstd-sys`、Apache-2.0 OR MIT) を使った直球実装
//! - 圧縮処理は CPU 重量級なので `tokio::task::spawn_blocking` で別スレッドへ逃がす
//! - production では nvCOMP より遅いが、機能 / wire 互換 test の常設レーンとして必須

use bytes::Bytes;

use crate::{
    ChunkManifest, Codec, CodecError, CodecKind, DECOMPRESS_BOOTSTRAP_CAPACITY,
    validate_decompress_manifest,
};

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

/// Sync, runtime-free decompress used by `s4-codec-wasm` (browser / WASM has
/// no tokio runtime and no `spawn_blocking`). Same checks as the trait
/// implementation: codec/size match, decompression-bomb cap at
/// `manifest.original_size + 1024`, crc32c verify after.
///
/// Kept in this module (and not duplicated in the wasm crate) so the bomb
/// limit + size + crc rules stay defined exactly once.
pub fn decompress_blocking(input: &[u8], manifest: &ChunkManifest) -> Result<Vec<u8>, CodecError> {
    if manifest.codec != CodecKind::CpuZstd {
        return Err(CodecError::CodecMismatch {
            expected: CodecKind::CpuZstd,
            got: manifest.codec,
        });
    }
    // v0.8.6 #89: pre-allocation guard. Reject `original_size > 5 GiB` and
    // confirm `compressed_size` matches the actual payload length BEFORE
    // the `Vec::with_capacity` below.
    let allocated_orig_size = validate_decompress_manifest(manifest, input.len())?;
    use std::io::Read;
    let limit = manifest.original_size.saturating_add(1024);
    let mut decoder = zstd::stream::Decoder::new(input).map_err(CodecError::Io)?;
    // v0.8.6 #89: cap the *initial* alloc at 1 MiB even if the manifest
    // claims a much larger output. A forged `original_size = 4 GiB` no
    // longer drives 4 GiB of address space at `with_capacity` time;
    // `read_to_end` (already bounded by `take(limit)` above) grows the
    // buffer naturally as actual decoded bytes arrive.
    let mut buf = Vec::with_capacity(allocated_orig_size.min(DECOMPRESS_BOOTSTRAP_CAPACITY));
    (&mut decoder)
        .take(limit)
        .read_to_end(&mut buf)
        .map_err(CodecError::Io)?;
    if (buf.len() as u64) > manifest.original_size {
        return Err(CodecError::Io(std::io::Error::other(format!(
            "zstd decompression bomb detected: produced {} bytes, manifest claimed {}",
            buf.len(),
            manifest.original_size
        ))));
    }
    if buf.len() as u64 != manifest.original_size {
        return Err(CodecError::SizeMismatch {
            expected: manifest.original_size,
            got: buf.len() as u64,
        });
    }
    let actual_crc = crc32c::crc32c(&buf);
    if actual_crc != manifest.crc32c {
        return Err(CodecError::CrcMismatch {
            expected: manifest.crc32c,
            got: actual_crc,
        });
    }
    Ok(buf)
}

/// Sync compress sibling of `decompress_blocking`. Provided for symmetry — the
/// browser side rarely compresses (it's read-only), but having both halves
/// keeps the API explainable and useful for offline tooling.
pub fn compress_blocking(input: &[u8], level: i32) -> Result<(Vec<u8>, ChunkManifest), CodecError> {
    let level = level.clamp(1, 22);
    let original_size = input.len() as u64;
    let original_crc = crc32c::crc32c(input);
    let compressed = zstd::stream::encode_all(input, level).map_err(CodecError::Io)?;
    Ok((
        compressed.clone(),
        ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size,
            compressed_size: compressed.len() as u64,
            crc32c: original_crc,
        },
    ))
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
        // v0.8.6 #89: pre-allocation guard — reject `original_size > 5 GiB`
        // and confirm `compressed_size` matches the actual payload length
        // BEFORE the `Vec::with_capacity` inside spawn_blocking. The fuzz
        // farm (cpu_zstd_decompress_bolero, issue #89) hit OOM within
        // seconds because a manifest could claim `original_size = u32::MAX`
        // and drive `Vec::with_capacity(4 GiB)` before any size check.
        let allocated_orig_size = validate_decompress_manifest(manifest, input.len())?;

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
            // v0.8.6 #89: bootstrap-capped initial alloc — see lib.rs
            // `DECOMPRESS_BOOTSTRAP_CAPACITY` doc.
            let mut buf =
                Vec::with_capacity(allocated_orig_size.min(DECOMPRESS_BOOTSTRAP_CAPACITY));
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

    /// v0.8.6 #89 regression #1 — manifest claims `original_size > 5 GiB`
    /// (above `MAX_DECOMPRESSED_BYTES`); `validate_decompress_manifest`
    /// must reject it pre-allocation.
    #[tokio::test]
    async fn issue_89_rejects_manifest_over_5gib() {
        let codec = CpuZstd::default();
        let body = Bytes::from_static(&[0x00, 0xd1, 0xd1, 0xd1, 0xd1, 0xd1]);
        let manifest = ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size: crate::MAX_DECOMPRESSED_BYTES + 1,
            compressed_size: body.len() as u64,
            crc32c: 0,
        };
        let err = codec.decompress(body, &manifest).await.unwrap_err();
        match err {
            CodecError::ManifestSizeExceedsLimit { requested, limit } => {
                assert_eq!(requested, crate::MAX_DECOMPRESSED_BYTES + 1);
                assert_eq!(limit, crate::MAX_DECOMPRESSED_BYTES);
            }
            other => panic!("expected ManifestSizeExceedsLimit, got {other:?}"),
        }
    }

    /// v0.8.6 #89 regression #2 — replays the exact OOM-triggering shape
    /// the continuous fuzz farm landed in `cpu_zstd_decompress_bolero`'s
    /// crashes/ dir within seconds. The libfuzzer artifact had body
    /// `0x00 0xd1×5` with a `(Vec<u8>, u32)` bolero generator producing a
    /// `claimed_orig` close to `u32::MAX` — under the 5 GiB validate
    /// ceiling, so `validate_decompress_manifest` accepts it. The
    /// `DECOMPRESS_BOOTSTRAP_CAPACITY` cap on the initial `with_capacity`
    /// is what now keeps the actual alloc at 1 MiB instead of 4 GiB; the
    /// decode then fails fast with `Io(UnexpectedEof)` because the body
    /// is not a valid zstd frame, well before any RSS pressure.
    #[tokio::test]
    async fn issue_89_bootstrap_cap_keeps_4gib_claim_alloc_safe() {
        let codec = CpuZstd::default();
        let body = Bytes::from_static(&[0x00, 0xd1, 0xd1, 0xd1, 0xd1, 0xd1]);
        let manifest = ChunkManifest {
            codec: CodecKind::CpuZstd,
            // u32::MAX = 4 GiB, below the 5 GiB validate ceiling, so the
            // pre-alloc guard passes and we exercise the bootstrap-cap
            // alloc path directly.
            original_size: u32::MAX as u64,
            compressed_size: body.len() as u64,
            crc32c: 0,
        };
        let err = codec.decompress(body, &manifest).await.unwrap_err();
        // Either Io (zstd refuses to decode the garbage body) or
        // SizeMismatch (decoded a zero-byte plaintext, manifest claims
        // 4 GiB) — both prove the call returned cleanly without OOM.
        assert!(
            matches!(err, CodecError::Io(_) | CodecError::SizeMismatch { .. }),
            "expected Io or SizeMismatch, got {err:?}"
        );
    }

    /// `decompress_blocking` (used by `s4-codec-wasm`) round-trips through
    /// `compress_blocking` with the same checks the async path applies.
    #[test]
    fn blocking_roundtrip() {
        let input = b"hello squished s3 hello squished s3 hello squished s3";
        let (compressed, manifest) = compress_blocking(input, CpuZstd::DEFAULT_LEVEL).unwrap();
        assert_eq!(manifest.codec, CodecKind::CpuZstd);
        let decompressed = decompress_blocking(&compressed, &manifest).unwrap();
        assert_eq!(decompressed, input);
    }
}
