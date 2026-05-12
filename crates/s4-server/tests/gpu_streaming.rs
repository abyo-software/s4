//! GPU streaming compress acceptance tests for issue #1.
//!
//! Run with: `cargo test --features nvcomp-gpu -p s4-server -- --ignored gpu_streaming`
//!
//! Skipped without the `nvcomp-gpu` feature. Each test is `#[ignore]` because
//! it requires a CUDA-capable GPU + `NVCOMP_HOME` at build time.

#![cfg(feature = "nvcomp-gpu")]

use bytes::Bytes;
use futures::stream;
use s3s::dto::StreamingBlob;
use s4_codec::CodecKind;
use s4_server::streaming::{
    DEFAULT_NVCOMP_STREAM_CHUNK_SIZE, cpu_zstd_decompress_stream, streaming_compress_nvcomp_zstd,
};

fn make_blob_chunked(data: Bytes, chunk_size: usize) -> StreamingBlob {
    let chunks: Vec<_> = data
        .chunks(chunk_size)
        .map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c)))
        .collect();
    StreamingBlob::wrap(stream::iter(chunks))
}

async fn collect(mut blob: StreamingBlob) -> Bytes {
    use bytes::BytesMut;
    use futures::StreamExt;
    let mut buf = BytesMut::new();
    while let Some(chunk) = blob.next().await {
        buf.extend_from_slice(&chunk.unwrap());
    }
    buf.freeze()
}

fn synthetic_input(size: usize) -> Bytes {
    // Mix of repeated bytes (highly compressible) + a counter (medium compressible)
    // so we exercise both extremes within one input.
    let mut buf = Vec::with_capacity(size);
    let mut counter: u32 = 0;
    while buf.len() < size {
        if (buf.len() / 1024) % 2 == 0 {
            // 1 KiB block of repeated 'x' (~1000:1 compression)
            let take = (size - buf.len()).min(1024);
            buf.extend(std::iter::repeat(b'x').take(take));
        } else {
            // 1 KiB block of u32 counter (~5:1 compression)
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

async fn assert_roundtrip(size: usize) {
    let original = synthetic_input(size);
    // Feed in 64 KiB chunks (simulating an HTTP body stream).
    let blob = make_blob_chunked(original.clone(), 64 * 1024);

    let (compressed, manifest) =
        streaming_compress_nvcomp_zstd(blob, DEFAULT_NVCOMP_STREAM_CHUNK_SIZE)
            .await
            .expect("streaming_compress_nvcomp_zstd failed");

    assert_eq!(manifest.codec, CodecKind::NvcompZstd);
    assert_eq!(manifest.original_size, original.len() as u64);
    assert_eq!(manifest.compressed_size, compressed.len() as u64);
    assert_eq!(manifest.crc32c, crc32c::crc32c(&original));
    assert!(
        compressed.len() < original.len(),
        "expected compressed ({}) < original ({})",
        compressed.len(),
        original.len()
    );

    // Decompress via the existing CPU zstd streaming decoder — proves the
    // wire-format property that concatenated nvCOMP zstd outputs form a
    // single valid zstd stream.
    let blob = StreamingBlob::wrap(stream::once(
        async move { Ok::<_, std::io::Error>(compressed) },
    ));
    let out = collect(cpu_zstd_decompress_stream(blob)).await;
    assert_eq!(out, original);
}

#[tokio::test]
#[ignore = "requires CUDA-capable GPU + NVCOMP_HOME at build time"]
async fn streaming_compress_nvcomp_zstd_roundtrip_1mib() {
    assert_roundtrip(1024 * 1024).await;
}

#[tokio::test]
#[ignore = "requires CUDA-capable GPU + NVCOMP_HOME at build time"]
async fn streaming_compress_nvcomp_zstd_roundtrip_100mib() {
    assert_roundtrip(100 * 1024 * 1024).await;
}

#[tokio::test]
#[ignore = "requires CUDA-capable GPU + NVCOMP_HOME at build time, ~5+ GB GPU memory"]
async fn streaming_compress_nvcomp_zstd_roundtrip_1gib() {
    assert_roundtrip(1024 * 1024 * 1024).await;
}
