//! CPU zstd with a shared **trained dictionary** (v1.1 `--zstd-dict`).
//!
//! Small objects (single-digit-KiB JSON events, per-line log PUTs, …) don't
//! compress with plain zstd — the zstd window never sees enough redundancy
//! within one object. A dictionary trained on a sample of similar objects
//! moves that shared redundancy out of band: each object then compresses
//! against the dictionary, typically 2-5× better than dict-less zstd on
//! homogeneous small payloads.
//!
//! ## Lock-in posture
//!
//! The dictionary is a **stock zstd dictionary** (`ZDICT_trainFromBuffer`
//! output via `zstd::dict::from_samples`) and the compressed payload is a
//! **stock zstd frame** that references it. No S4-private container:
//!
//! ```text
//! zstd -D <dictfile> -d payload.zst        # CLI decode, no S4 involved
//! zstandard.ZstdDecompressor(dict_data=…)  # python, ditto
//! ```
//!
//! (The `external_zstd_stream_api_decodes_dict_frame` test below pins the
//! plain-`zstd`-crate decode; the minio E2E in s4-server additionally pins
//! the `zstd` CLI when present on the host.)
//!
//! ## Integrity rules
//!
//! Same as [`crate::cpu_zstd`]: pre-allocation manifest validation
//! ([`crate::validate_decompress_manifest`]), decompression-bomb cap at
//! `original_size + 1024`, bootstrap-capped initial alloc, post-decode
//! size + crc32c verify. Decoding with the wrong dictionary surfaces as a
//! typed `Err` (zstd refuses the frame or the CRC catches it) — never a
//! panic.

use std::sync::Arc;

use bytes::Bytes;

use crate::{
    ChunkManifest, Codec, CodecError, CodecKind, DECOMPRESS_BOOTSTRAP_CAPACITY,
    validate_decompress_manifest,
};

/// CPU zstd codec bound to one trained dictionary. `level` is clamped to
/// 1..=22 like [`crate::cpu_zstd::CpuZstd`].
///
/// The dictionary is held as `Arc<[u8]>` so one in-memory copy serves the
/// gateway-wide compress path, the per-GET decompress path, and the
/// `spawn_blocking` closures without cloning the (up to ~110 KiB) bytes.
#[derive(Debug, Clone)]
pub struct CpuZstdDict {
    dict: Arc<[u8]>,
    level: i32,
}

impl CpuZstdDict {
    pub const DEFAULT_LEVEL: i32 = crate::cpu_zstd::CpuZstd::DEFAULT_LEVEL;

    /// Build a dict-bound codec. Rejects an empty dictionary — zstd would
    /// accept it and silently degrade to dict-less compression, which
    /// would then stamp a misleading `cpu-zstd-dict` codec label on wire.
    pub fn new(dict: Arc<[u8]>, level: i32) -> Result<Self, CodecError> {
        if dict.is_empty() {
            return Err(CodecError::Backend(anyhow::anyhow!(
                "zstd dictionary is empty (0 bytes) — refusing to build cpu-zstd-dict codec"
            )));
        }
        Ok(Self {
            dict,
            level: level.clamp(1, 22),
        })
    }

    /// Borrow the dictionary bytes (used by callers that need the id /
    /// fingerprint of the dictionary this codec is bound to).
    pub fn dict(&self) -> &Arc<[u8]> {
        &self.dict
    }
}

/// Train a zstd dictionary from individual samples
/// (`zstd::dict::from_samples` / `ZDICT_trainFromBuffer`). `max_dict_bytes`
/// caps the output size; zstd upstream recommends ~110 KiB (112640) for
/// general workloads. Errors (e.g. "Src size is incorrect" when the sample
/// set is too small / too uniform for ZDICT) surface as `CodecError::Io`.
pub fn train_from_samples<S: AsRef<[u8]>>(
    samples: &[S],
    max_dict_bytes: usize,
) -> Result<Vec<u8>, CodecError> {
    let dict = zstd::dict::from_samples(samples, max_dict_bytes).map_err(CodecError::Io)?;
    if dict.is_empty() {
        return Err(CodecError::Backend(anyhow::anyhow!(
            "zstd dictionary training produced 0 bytes (samples too small / too uniform?)"
        )));
    }
    Ok(dict)
}

/// Sync compress against `dict`. Mirrors
/// [`crate::cpu_zstd::compress_blocking`] — provided so offline tooling /
/// tests can produce byte-identical frames without a tokio runtime.
pub fn compress_blocking(
    input: &[u8],
    dict: &[u8],
    level: i32,
) -> Result<(Vec<u8>, ChunkManifest), CodecError> {
    let level = level.clamp(1, 22);
    let original_size = input.len() as u64;
    let original_crc = crc32c::crc32c(input);
    let compressed = encode_all_with_dict(input, dict, level).map_err(CodecError::Io)?;
    let manifest = ChunkManifest {
        codec: CodecKind::CpuZstdDict,
        original_size,
        compressed_size: compressed.len() as u64,
        crc32c: original_crc,
    };
    Ok((compressed, manifest))
}

/// Sync decompress against `dict`, with the exact same manifest /
/// bomb-cap / size / crc checks as the async trait path. Mirrors
/// [`crate::cpu_zstd::decompress_blocking`].
pub fn decompress_blocking(
    input: &[u8],
    dict: &[u8],
    manifest: &ChunkManifest,
) -> Result<Vec<u8>, CodecError> {
    if manifest.codec != CodecKind::CpuZstdDict {
        return Err(CodecError::CodecMismatch {
            expected: CodecKind::CpuZstdDict,
            got: manifest.codec,
        });
    }
    let allocated_orig_size = validate_decompress_manifest(manifest, input.len())?;
    decode_capped_with_dict(input, dict, manifest.original_size, allocated_orig_size)
        .map_err(CodecError::Io)
        .and_then(|buf| verify_decoded(buf, manifest))
}

/// One zstd stream encode pass against a dictionary. `Vec<u8>`-writer
/// variant of `zstd::stream::encode_all` (which has no dict parameter).
fn encode_all_with_dict(input: &[u8], dict: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let mut encoder = zstd::stream::write::Encoder::with_dictionary(Vec::new(), level, dict)?;
    encoder.write_all(input)?;
    encoder.finish()
}

/// Decode with the bomb cap (`original_size + 1024`) and the bootstrap-
/// capped initial alloc — same shape as `cpu_zstd`'s decode closure, with
/// `Decoder::with_dictionary` swapped in. Returns the raw decoded bytes;
/// caller runs `verify_decoded` for the size / crc checks.
fn decode_capped_with_dict(
    input: &[u8],
    dict: &[u8],
    expected_orig_size: u64,
    allocated_orig_size: usize,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let limit = expected_orig_size.saturating_add(1024);
    // `&[u8]` is already `BufRead`, so no extra `BufReader` wrapper needed.
    let mut decoder = zstd::stream::read::Decoder::with_dictionary(input, dict)?;
    let mut buf = Vec::with_capacity(allocated_orig_size.min(DECOMPRESS_BOOTSTRAP_CAPACITY));
    {
        let mut limited = (&mut decoder).take(limit);
        limited.read_to_end(&mut buf)?;
    }
    // v0.8.16 F-1 pattern: probe via the underlying decoder (the consumed
    // `Take` would always report 0) to tell a capped bomb from an exact-
    // overshoot decode.
    if buf.len() as u64 > expected_orig_size {
        let mut peek = [0u8; 1];
        let more_available = decoder.read(&mut peek).map(|n| n > 0).unwrap_or(false);
        return Err(std::io::Error::other(format!(
            "zstd-dict decompression bomb detected: produced at least {} bytes \
             (truncated at cap = manifest.original_size + 1024 = {}); manifest claimed {}{}",
            buf.len(),
            limit,
            expected_orig_size,
            if more_available {
                "; decoder had more bytes available beyond the cap"
            } else {
                ""
            },
        )));
    }
    Ok(buf)
}

/// Post-decode size + crc verification shared by the sync and async paths.
fn verify_decoded(buf: Vec<u8>, manifest: &ChunkManifest) -> Result<Vec<u8>, CodecError> {
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

#[async_trait::async_trait]
impl Codec for CpuZstdDict {
    fn kind(&self) -> CodecKind {
        CodecKind::CpuZstdDict
    }

    async fn compress(&self, input: Bytes) -> Result<(Bytes, ChunkManifest), CodecError> {
        let level = self.level;
        let dict = Arc::clone(&self.dict);
        let original_size = input.len() as u64;
        let original_crc = crc32c::crc32c(&input);

        let compressed = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            encode_all_with_dict(input.as_ref(), &dict, level)
        })
        .await??;

        let manifest = ChunkManifest {
            codec: CodecKind::CpuZstdDict,
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
        if manifest.codec != CodecKind::CpuZstdDict {
            return Err(CodecError::CodecMismatch {
                expected: CodecKind::CpuZstdDict,
                got: manifest.codec,
            });
        }
        let allocated_orig_size = validate_decompress_manifest(manifest, input.len())?;
        let expected_orig_size = manifest.original_size;
        let dict = Arc::clone(&self.dict);

        let decoded = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            decode_capped_with_dict(
                input.as_ref(),
                &dict,
                expected_orig_size,
                allocated_orig_size,
            )
        })
        .await??;

        verify_decoded(decoded, manifest).map(Bytes::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Train a small dictionary from synthetic JSON-event-shaped samples.
    /// ZDICT needs a reasonable number of samples (and total bytes) or it
    /// errors out, so generate ~200 varied-but-similar records.
    fn sample_corpus() -> Vec<Vec<u8>> {
        (0..200u32)
            .map(|i| {
                format!(
                    "{{\"timestamp\":\"2026-06-10T12:{:02}:{:02}Z\",\"level\":\"info\",\
                     \"service\":\"checkout-api\",\"event\":\"order_created\",\
                     \"order_id\":\"ord_{:08}\",\"customer_id\":\"cus_{:08}\",\
                     \"amount_cents\":{},\"currency\":\"USD\",\"items\":{}}}",
                    i % 60,
                    (i * 7) % 60,
                    i,
                    i * 31,
                    100 + i * 13,
                    i % 9
                )
                .into_bytes()
            })
            .collect()
    }

    fn trained_dict() -> Arc<[u8]> {
        let corpus = sample_corpus();
        let dict = train_from_samples(&corpus, 16 * 1024).expect("train");
        Arc::from(dict.into_boxed_slice())
    }

    fn fresh_sample() -> Bytes {
        Bytes::from_static(
            b"{\"timestamp\":\"2026-06-10T13:01:02Z\",\"level\":\"info\",\
              \"service\":\"checkout-api\",\"event\":\"order_created\",\
              \"order_id\":\"ord_99999999\",\"customer_id\":\"cus_12121212\",\
              \"amount_cents\":4200,\"currency\":\"USD\",\"items\":3}",
        )
    }

    #[tokio::test]
    async fn dict_roundtrip() {
        let codec = CpuZstdDict::new(trained_dict(), CpuZstdDict::DEFAULT_LEVEL).expect("codec");
        let input = fresh_sample();
        let (compressed, manifest) = codec.compress(input.clone()).await.expect("compress");
        assert_eq!(manifest.codec, CodecKind::CpuZstdDict);
        assert_eq!(manifest.original_size, input.len() as u64);
        assert_eq!(manifest.compressed_size, compressed.len() as u64);
        let decompressed = codec.decompress(compressed, &manifest).await.expect("dec");
        assert_eq!(decompressed, input);
    }

    /// The whole point of the feature: on a small object similar to the
    /// training corpus, the dict frame must be smaller than dict-less zstd.
    #[tokio::test]
    async fn dict_beats_plain_zstd_on_small_similar_payload() {
        let codec = CpuZstdDict::new(trained_dict(), CpuZstdDict::DEFAULT_LEVEL).expect("codec");
        let input = fresh_sample();
        let (with_dict, _) = codec.compress(input.clone()).await.expect("compress");
        let (without_dict, _) =
            crate::cpu_zstd::compress_blocking(&input, CpuZstdDict::DEFAULT_LEVEL).expect("plain");
        assert!(
            with_dict.len() < without_dict.len(),
            "dict frame ({}) must beat plain zstd ({}) on a corpus-like sample",
            with_dict.len(),
            without_dict.len()
        );
    }

    /// Wrong dictionary at decode time → typed Err (Io or CrcMismatch),
    /// never a panic, never silently-wrong bytes.
    #[tokio::test]
    async fn wrong_dict_decode_fails_with_err_not_panic() {
        let right = CpuZstdDict::new(trained_dict(), CpuZstdDict::DEFAULT_LEVEL).expect("codec");
        let input = fresh_sample();
        let (compressed, manifest) = right.compress(input.clone()).await.expect("compress");

        // Train a *different* dictionary from a different corpus.
        let other_corpus: Vec<Vec<u8>> = (0..200u32)
            .map(|i| format!("metric host=web-{i:04} cpu={} mem={}\n", i % 100, i * 3).into_bytes())
            .collect();
        let other_dict: Arc<[u8]> = Arc::from(
            train_from_samples(&other_corpus, 16 * 1024)
                .expect("train other")
                .into_boxed_slice(),
        );
        let wrong = CpuZstdDict::new(other_dict, CpuZstdDict::DEFAULT_LEVEL).expect("codec");
        let err = wrong
            .decompress(compressed, &manifest)
            .await
            .expect_err("wrong dict must fail");
        assert!(
            matches!(
                err,
                CodecError::Io(_)
                    | CodecError::CrcMismatch { .. }
                    | CodecError::SizeMismatch { .. }
            ),
            "expected Io/CrcMismatch/SizeMismatch, got {err:?}"
        );
    }

    #[test]
    fn empty_dict_rejected() {
        let empty: Arc<[u8]> = Arc::from(Vec::new().into_boxed_slice());
        let err = CpuZstdDict::new(empty, 3).expect_err("empty dict must be rejected");
        assert!(matches!(err, CodecError::Backend(_)));
    }

    #[tokio::test]
    async fn rejects_codec_mismatch_manifest() {
        let codec = CpuZstdDict::new(trained_dict(), 3).expect("codec");
        let manifest = ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size: 10,
            compressed_size: 10,
            crc32c: 0,
        };
        let err = codec
            .decompress(Bytes::from_static(b"0123456789"), &manifest)
            .await
            .expect_err("mismatch");
        assert!(matches!(err, CodecError::CodecMismatch { .. }));
    }

    /// Forged manifest above the 5 GiB ceiling → pre-allocation reject,
    /// same contract as cpu_zstd issue #89.
    #[tokio::test]
    async fn rejects_manifest_over_5gib() {
        let codec = CpuZstdDict::new(trained_dict(), 3).expect("codec");
        let body = Bytes::from_static(&[0x00, 0xd1, 0xd1, 0xd1, 0xd1, 0xd1]);
        let manifest = ChunkManifest {
            codec: CodecKind::CpuZstdDict,
            original_size: crate::MAX_DECOMPRESSED_BYTES + 1,
            compressed_size: body.len() as u64,
            crc32c: 0,
        };
        let err = codec.decompress(body, &manifest).await.expect_err("err");
        assert!(matches!(err, CodecError::ManifestSizeExceedsLimit { .. }));
    }

    /// The frame written by this codec is a stock zstd frame: the plain
    /// `zstd` crate streaming API (no S4 types at all) decodes it given
    /// the same dictionary bytes. This is the in-process proof behind the
    /// README "decode without the gateway" recipe; the minio E2E
    /// additionally pins the `zstd` CLI when available.
    #[test]
    fn external_zstd_stream_api_decodes_dict_frame() {
        use std::io::Read;
        let dict = trained_dict();
        let input = fresh_sample();
        let (frame, _manifest) =
            compress_blocking(&input, &dict, CpuZstdDict::DEFAULT_LEVEL).expect("compress");
        let mut decoder =
            zstd::stream::read::Decoder::with_dictionary(frame.as_slice(), &dict).expect("decoder");
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("decode");
        assert_eq!(out, input);
    }

    /// Sync helpers mirror the async path byte-for-byte.
    #[test]
    fn blocking_roundtrip() {
        let dict = trained_dict();
        let input = fresh_sample();
        let (compressed, manifest) =
            compress_blocking(&input, &dict, CpuZstdDict::DEFAULT_LEVEL).expect("compress");
        let decompressed = decompress_blocking(&compressed, &dict, &manifest).expect("dec");
        assert_eq!(decompressed, input);
    }

    #[test]
    fn train_from_samples_rejects_tiny_corpus_with_err() {
        // 2 near-empty samples — ZDICT errors out; must be Err, not panic.
        let samples: Vec<Vec<u8>> = vec![b"a".to_vec(), b"b".to_vec()];
        assert!(train_from_samples(&samples, 16 * 1024).is_err());
    }
}
