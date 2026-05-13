//! GPU compress/decompress acceptance tests for v0.2 #1, #9 (and later #8).
//!
//! Run with:
//!   NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
//!     cargo test --release -p s4-server --features nvcomp-gpu --test gpu_streaming \
//!       -- --ignored --test-threads=1
//!
//! Each test is `#[ignore]` because it needs CUDA-capable GPU + NVCOMP_HOME at
//! build time + the nvCOMP shared library at runtime.
//!
//! These tests exercise the v0.2 #4 unified path: per-chunk compress with the
//! GPU codec, S4F2 framing per chunk, then per-frame decompress on the read
//! side (same logic the real S4Service::decompress_multipart uses). This is
//! the wire format that lets a 10 GB highly compressible upload stay under
//! ~210 MB host RAM peak instead of buffering the full 10 GB.

#![cfg(feature = "nvcomp-gpu")]

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::stream;
use s3s::dto::StreamingBlob;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::multipart::FrameIter;
use s4_codec::nvcomp::NvcompGDeflateCodec;
use s4_codec::passthrough::Passthrough;
use s4_codec::{ChunkManifest, CodecKind, CodecRegistry};
use s4_server::streaming::{DEFAULT_S4F2_CHUNK_SIZE, streaming_compress_to_frames};

fn synthetic_input(size: usize) -> Bytes {
    let mut buf = Vec::with_capacity(size);
    let mut counter: u32 = 0;
    while buf.len() < size {
        if (buf.len() / 1024) % 2 == 0 {
            // 1 KiB block of repeated 'x' (~1000:1 compression)
            let take = (size - buf.len()).min(1024);
            buf.extend(std::iter::repeat(b'x').take(take));
        } else {
            let mut left = (size - buf.len()).min(1024);
            while left >= 4 {
                buf.extend_from_slice(&counter.to_le_bytes());
                counter += 1;
                left -= 4;
            }
        }
    }
    Bytes::from(buf)
}

fn make_blob_chunked(data: Bytes, chunk_size: usize) -> StreamingBlob {
    let chunks: Vec<_> = data
        .chunks(chunk_size)
        .map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c)))
        .collect();
    StreamingBlob::wrap(stream::iter(chunks))
}

/// Build a registry that knows about the codec we'll exercise + Passthrough +
/// CpuZstd for completeness. nvCOMP backends register optionally.
fn build_registry(default: CodecKind) -> Arc<CodecRegistry> {
    let mut r = CodecRegistry::new(default)
        .with(Arc::new(Passthrough))
        .with(Arc::new(CpuZstd::default()));
    use s4_codec::nvcomp::{NvcompBitcompCodec, NvcompZstdCodec, is_gpu_available};
    if is_gpu_available() {
        if let Ok(c) = NvcompZstdCodec::new() {
            r = r.with(Arc::new(c));
        }
        if let Ok(c) = NvcompBitcompCodec::default_general() {
            r = r.with(Arc::new(c));
        }
        if let Ok(c) = NvcompGDeflateCodec::new() {
            r = r.with(Arc::new(c));
        }
    }
    Arc::new(r)
}

/// Decompress an S4F2 multi-frame body using the registry — mirrors the logic
/// in S4Service::decompress_multipart so the codec layer can be tested in
/// isolation.
async fn decompress_frames(framed: Bytes, registry: &CodecRegistry) -> Bytes {
    let mut out = BytesMut::new();
    for frame in FrameIter::new(framed) {
        let (header, payload) = frame.expect("frame parse");
        let manifest = ChunkManifest {
            codec: header.codec,
            original_size: header.original_size,
            compressed_size: header.compressed_size,
            crc32c: header.crc32c,
        };
        let bytes = registry
            .decompress(payload, &manifest)
            .await
            .expect("decompress");
        out.extend_from_slice(&bytes);
    }
    out.freeze()
}

/// Verifies the v0.2 #1 acceptance: per-chunk pipelined GPU compression
/// produces an S4F2 multi-frame body that round-trips byte-equal.
async fn assert_roundtrip(codec: CodecKind, size: usize) {
    let original = synthetic_input(size);
    let registry = build_registry(codec);

    let blob = make_blob_chunked(original.clone(), 64 * 1024);
    // v0.8.4 #73 M2: pass `Some(original.len())` so the truncation guard
    // would catch a regression where the synthetic input stream returned
    // EOF early (the rest of the test still asserts byte-equal round-trip).
    let (framed, manifest) = streaming_compress_to_frames(
        blob,
        Arc::clone(&registry),
        codec,
        DEFAULT_S4F2_CHUNK_SIZE,
        Some(original.len() as u64),
    )
    .await
    .expect("streaming_compress_to_frames");

    assert_eq!(manifest.codec, codec);
    assert_eq!(manifest.original_size, original.len() as u64);
    assert_eq!(manifest.compressed_size, framed.len() as u64);
    assert_eq!(manifest.crc32c, crc32c::crc32c(&original));
    assert!(
        framed.len() < original.len(),
        "expected framed compressed ({}) < original ({})",
        framed.len(),
        original.len()
    );

    let decompressed = decompress_frames(framed, &registry).await;
    assert_eq!(decompressed, original);
}

#[tokio::test]
#[ignore = "requires CUDA-capable GPU + NVCOMP_HOME"]
async fn streaming_compress_nvcomp_zstd_roundtrip_1mib() {
    assert_roundtrip(CodecKind::NvcompZstd, 1024 * 1024).await;
}

#[tokio::test]
#[ignore = "requires CUDA-capable GPU + NVCOMP_HOME"]
async fn streaming_compress_nvcomp_zstd_roundtrip_100mib() {
    assert_roundtrip(CodecKind::NvcompZstd, 100 * 1024 * 1024).await;
}

#[tokio::test]
#[ignore = "requires CUDA-capable GPU + NVCOMP_HOME, ~5+ GB GPU memory"]
async fn streaming_compress_nvcomp_zstd_roundtrip_1gib() {
    assert_roundtrip(CodecKind::NvcompZstd, 1024 * 1024 * 1024).await;
}

/// v0.2 #9: nvCOMP GDeflate roundtrip — same per-chunk pipeline as zstd.
#[tokio::test]
#[ignore = "requires CUDA-capable GPU + NVCOMP_HOME"]
async fn streaming_compress_nvcomp_gdeflate_roundtrip_1mib() {
    assert_roundtrip(CodecKind::NvcompGDeflate, 1024 * 1024).await;
}

/// v0.2 #9: bytes-API roundtrip — verifies the FFI bindings compile + link
/// + the Algo::GDeflate -> FCG1 algo_tag wiring works end-to-end.
#[tokio::test]
#[ignore = "requires CUDA-capable GPU + NVCOMP_HOME"]
async fn nvcomp_gdeflate_codec_roundtrip() {
    use s4_codec::Codec;

    let codec = NvcompGDeflateCodec::new().expect("nvcomp gdeflate init");
    let original = synthetic_input(1024 * 1024);
    let (compressed, manifest) = codec
        .compress(original.clone())
        .await
        .expect("gdeflate compress");
    assert_eq!(manifest.codec, CodecKind::NvcompGDeflate);
    assert_eq!(manifest.original_size, original.len() as u64);
    assert!(
        compressed.len() < original.len(),
        "gdeflate should reduce size, got {} -> {}",
        original.len(),
        compressed.len()
    );
    let decompressed = codec
        .decompress(compressed, &manifest)
        .await
        .expect("gdeflate decompress");
    assert_eq!(decompressed, original);
}
