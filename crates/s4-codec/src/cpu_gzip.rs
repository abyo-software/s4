//! RFC 1952 gzip codec via `flate2` (v0.4 #26).
//!
//! Why CPU and not GPU: nvCOMP's GDeflate produces a multi-stream
//! parallel-decode-friendly format that is **not** a single valid DEFLATE
//! stream — wrapping it with a gzip header doesn't make stock `gunzip`
//! decode it. To deliver the actual user-facing value of issue #26 (= "an
//! S3 object S4 stored that any browser / curl / standard library can
//! decompress without knowing about S4"), the codec has to emit a real
//! gzip stream. CPU `flate2` is the right tool.
//!
//! Trade-off: no GPU acceleration on this codec. For wire-compat against
//! gunzip-aware clients use `cpu-gzip`; for raw GPU throughput where
//! everyone speaks S4 use `nvcomp-zstd` / `nvcomp-bitcomp`.
//!
//! Default compression level is 6 — `flate2`'s default and the same level
//! `gzip(1)` uses out of the box. Range 0..=9 (= flate2::Compression range).

use std::io::{Read, Write};

use bytes::Bytes;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::{
    ChunkManifest, Codec, CodecError, CodecKind, DECOMPRESS_BOOTSTRAP_CAPACITY,
    validate_decompress_manifest,
};

/// CPU gzip codec (RFC 1952). `level` clamped to 0..=9.
#[derive(Debug, Clone)]
pub struct CpuGzip {
    level: u32,
}

impl CpuGzip {
    pub const DEFAULT_LEVEL: u32 = 6;

    pub fn new(level: u32) -> Self {
        Self {
            level: level.min(9),
        }
    }
}

impl Default for CpuGzip {
    fn default() -> Self {
        Self::new(Self::DEFAULT_LEVEL)
    }
}

/// Sync, runtime-free decompress used by `s4-codec-wasm` (browser / WASM has
/// no tokio runtime). Same checks as the trait implementation: codec/size
/// match, decompression-bomb cap at `manifest.original_size + 1024`, crc32c
/// verify after.
pub fn decompress_blocking(input: &[u8], manifest: &ChunkManifest) -> Result<Vec<u8>, CodecError> {
    if manifest.codec != CodecKind::CpuGzip {
        return Err(CodecError::CodecMismatch {
            expected: CodecKind::CpuGzip,
            got: manifest.codec,
        });
    }
    // v0.8.6 #89: pre-allocation guard. Reject `original_size > 5 GiB` and
    // confirm `compressed_size` matches the actual payload length BEFORE
    // the `Vec::with_capacity` below — same shape applied to cpu_zstd.
    let allocated_orig_size = validate_decompress_manifest(manifest, input.len())?;
    let limit = manifest.original_size.saturating_add(1024);
    // v0.8.6 #89: bootstrap-capped initial alloc — see lib.rs
    // `DECOMPRESS_BOOTSTRAP_CAPACITY` doc.
    let mut buf = Vec::with_capacity(allocated_orig_size.min(DECOMPRESS_BOOTSTRAP_CAPACITY));
    let mut decoder = GzDecoder::new(input);
    {
        let mut limited = (&mut decoder).take(limit);
        limited.read_to_end(&mut buf).map_err(CodecError::Io)?;
        // v0.8.15 M-9: see cpu_zstd.rs for the rationale. Probe one
        // byte past `limit` to distinguish a truncated bomb from a
        // legitimate overshoot inside the 1024-byte zstd buffer
        // flush window the cap was tuned for.
        if buf.len() as u64 > manifest.original_size {
            let mut peek = [0u8; 1];
            let more_available = limited.read(&mut peek).map(|n| n > 0).unwrap_or(false);
            return Err(CodecError::Io(std::io::Error::other(format!(
                "gzip decompression bomb detected: produced at least {} bytes \
                 (truncated at cap = manifest.original_size + 1024 = {}); \
                 manifest claimed {}{}",
                buf.len(),
                limit,
                manifest.original_size,
                if more_available {
                    "; decoder had more bytes available beyond the cap"
                } else {
                    ""
                },
            ))));
        }
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

/// Sync compress sibling of `decompress_blocking`. Provided for symmetry.
pub fn compress_blocking(input: &[u8], level: u32) -> Result<(Vec<u8>, ChunkManifest), CodecError> {
    let level = level.min(9);
    let original_size = input.len() as u64;
    let original_crc = crc32c::crc32c(input);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(input).map_err(CodecError::Io)?;
    let compressed = encoder.finish().map_err(CodecError::Io)?;
    Ok((
        compressed.clone(),
        ChunkManifest {
            codec: CodecKind::CpuGzip,
            original_size,
            compressed_size: compressed.len() as u64,
            crc32c: original_crc,
        },
    ))
}

#[async_trait::async_trait]
impl Codec for CpuGzip {
    fn kind(&self) -> CodecKind {
        CodecKind::CpuGzip
    }

    async fn compress(&self, input: Bytes) -> Result<(Bytes, ChunkManifest), CodecError> {
        let level = self.level;
        let original_size = input.len() as u64;
        let original_crc = crc32c::crc32c(&input);

        let compressed = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level));
            encoder.write_all(input.as_ref())?;
            encoder.finish()
        })
        .await??;

        let manifest = ChunkManifest {
            codec: CodecKind::CpuGzip,
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
        if manifest.codec != CodecKind::CpuGzip {
            return Err(CodecError::CodecMismatch {
                expected: CodecKind::CpuGzip,
                got: manifest.codec,
            });
        }
        // v0.8.6 #89: pre-allocation guard — same shape as cpu_zstd. The
        // CpuZstd OOM (issue #89) was caught by the fuzz farm in seconds;
        // CpuGzip has the identical `Vec::with_capacity(original_size)`
        // shape and is just as vulnerable to a forged manifest.
        let allocated_orig_size = validate_decompress_manifest(manifest, input.len())?;

        let expected_crc = manifest.crc32c;
        let expected_orig_size = manifest.original_size;

        let decompressed = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            // Decompression-bomb hardening: cap at expected_orig_size + small
            // margin (same pattern cpu_zstd uses). A malicious sidecar could
            // claim a tiny original_size while the gzip footer says otherwise;
            // we trust the manifest and detect inflation past it as bomb.
            let limit = expected_orig_size.saturating_add(1024);
            // v0.8.6 #89: bootstrap-capped initial alloc — see lib.rs
            // `DECOMPRESS_BOOTSTRAP_CAPACITY` doc.
            let mut buf =
                Vec::with_capacity(allocated_orig_size.min(DECOMPRESS_BOOTSTRAP_CAPACITY));
            let mut decoder = GzDecoder::new(input.as_ref());
            (&mut decoder).take(limit).read_to_end(&mut buf)?;
            if (buf.len() as u64) > expected_orig_size {
                return Err(std::io::Error::other(format!(
                    "gzip decompression bomb detected: produced {} bytes, manifest claimed {}",
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
    use std::io::Read;

    #[tokio::test]
    async fn roundtrip_small() {
        let codec = CpuGzip::default();
        let input = Bytes::from_static(b"the quick brown fox jumps over the lazy dog ".as_slice());
        let (compressed, manifest) = codec.compress(input.clone()).await.unwrap();
        assert_eq!(manifest.codec, CodecKind::CpuGzip);
        assert_eq!(manifest.original_size, input.len() as u64);
        let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
        assert_eq!(decompressed, input);
    }

    #[tokio::test]
    async fn roundtrip_compressible() {
        let codec = CpuGzip::default();
        let input = Bytes::from(vec![b'x'; 1024 * 1024]);
        let (compressed, manifest) = codec.compress(input.clone()).await.unwrap();
        // 1 MiB of 'x' bytes should gzip to <1 KiB
        assert!(
            compressed.len() < 2048,
            "expected gzip to compress 1 MiB of x bytes well, got {} bytes",
            compressed.len()
        );
        let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
        assert_eq!(decompressed, input);
    }

    /// The whole point of this codec: stock `gunzip` (= flate2's
    /// GzDecoder, the same library every Linux distro ships) decodes
    /// the output. This is the wire-compat property the issue requires.
    #[tokio::test]
    async fn output_is_decodable_by_stock_gunzip() {
        let codec = CpuGzip::default();
        let input = Bytes::from(b"hello squished world\n".repeat(100));
        let (compressed, _manifest) = codec.compress(input.clone()).await.unwrap();

        // First two bytes must be the gzip magic (0x1f 0x8b) per RFC 1952.
        assert_eq!(
            &compressed[..2],
            &[0x1f, 0x8b],
            "must start with gzip magic"
        );

        // Decode with a fresh GzDecoder instance — different code path
        // from our decompress (which goes via the manifest); represents
        // what a downstream client / browser / curl would do.
        let mut buf = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_ref())
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, input.as_ref());
    }

    #[tokio::test]
    async fn rejects_codec_mismatch() {
        let codec = CpuGzip::default();
        let manifest = ChunkManifest {
            codec: CodecKind::CpuZstd,
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

    /// v0.8.6 #89 — CpuGzip has the identical
    /// `Vec::with_capacity(manifest.original_size)` shape as CpuZstd
    /// did. (1) reject manifests over the 5 GiB ceiling, (2) bootstrap-
    /// cap the initial alloc so a sub-5-GiB-but-still-huge claim
    /// (e.g. 4 GiB) doesn't drive the address space.
    #[tokio::test]
    async fn issue_89_rejects_manifest_over_5gib() {
        let codec = CpuGzip::default();
        let body = Bytes::from_static(&[0x1f, 0x8b]);
        let manifest = ChunkManifest {
            codec: CodecKind::CpuGzip,
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

    #[tokio::test]
    async fn issue_89_bootstrap_cap_keeps_4gib_claim_alloc_safe() {
        let codec = CpuGzip::default();
        let body = Bytes::from_static(&[0x1f, 0x8b]);
        let manifest = ChunkManifest {
            codec: CodecKind::CpuGzip,
            original_size: u32::MAX as u64,
            compressed_size: body.len() as u64,
            crc32c: 0,
        };
        let err = codec.decompress(body, &manifest).await.unwrap_err();
        assert!(
            matches!(err, CodecError::Io(_) | CodecError::SizeMismatch { .. }),
            "expected Io or SizeMismatch, got {err:?}"
        );
    }

    /// `decompress_blocking` (used by `s4-codec-wasm`) round-trips through
    /// `compress_blocking`. Output must still start with the gzip magic so
    /// stock browsers can decode it via `DecompressionStream("gzip")`.
    #[test]
    fn blocking_roundtrip_and_gzip_magic() {
        let input = b"hello squished world\n".repeat(100);
        let (compressed, manifest) = compress_blocking(&input, CpuGzip::DEFAULT_LEVEL).unwrap();
        assert_eq!(&compressed[..2], &[0x1f, 0x8b]);
        let decompressed = decompress_blocking(&compressed, &manifest).unwrap();
        assert_eq!(decompressed, input);
    }

    /// v0.8.7 (Codex review LOW) — blocking variants of the issue #89
    /// regression tests, mirroring the cpu_zstd additions.
    #[test]
    fn issue_89_blocking_rejects_manifest_over_5gib() {
        let body = &[0x1f, 0x8b];
        let manifest = ChunkManifest {
            codec: CodecKind::CpuGzip,
            original_size: crate::MAX_DECOMPRESSED_BYTES + 1,
            compressed_size: body.len() as u64,
            crc32c: 0,
        };
        let err = decompress_blocking(body, &manifest).unwrap_err();
        match err {
            CodecError::ManifestSizeExceedsLimit { requested, limit } => {
                assert_eq!(requested, crate::MAX_DECOMPRESSED_BYTES + 1);
                assert_eq!(limit, crate::MAX_DECOMPRESSED_BYTES);
            }
            other => panic!("expected ManifestSizeExceedsLimit, got {other:?}"),
        }
    }

    #[test]
    fn issue_89_blocking_bootstrap_cap_keeps_4gib_claim_alloc_safe() {
        let body = &[0x1f, 0x8b];
        let manifest = ChunkManifest {
            codec: CodecKind::CpuGzip,
            original_size: u32::MAX as u64,
            compressed_size: body.len() as u64,
            crc32c: 0,
        };
        let err = decompress_blocking(body, &manifest).unwrap_err();
        assert!(
            matches!(err, CodecError::Io(_) | CodecError::SizeMismatch { .. }),
            "expected Io or SizeMismatch, got {err:?}"
        );
    }
}
