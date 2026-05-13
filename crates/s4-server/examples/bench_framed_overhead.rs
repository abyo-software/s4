//! Wire-overhead micro-bench for the v0.2 #4 framed single-PUT format
//! (issue v0.4 #18).
//!
//! v0.2 #4 made every S4 single-PUT object go through `streaming_compress_to_frames`,
//! which wraps the codec output in S4F2 frames. Each frame carries a 28-byte header
//! (see `s4_codec::multipart::FRAME_HEADER_BYTES`). For small objects (< chunk_size,
//! i.e. single-frame payloads) the framed output is exactly
//! `raw_compressed_bytes + 28` regardless of the input size — this bench confirms
//! that hard number on three realistic small-object sizes.
//!
//! Run:
//!   cargo run --release --example bench_framed_overhead -p s4-server
//!
//! Output is a markdown table that gets pasted as a footnote into the README's
//! "On-the-wire Format" section.

use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use s3s::dto::StreamingBlob;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::passthrough::Passthrough;
use s4_codec::{Codec, CodecKind, CodecRegistry};
use s4_server::streaming::{DEFAULT_S4F2_CHUNK_SIZE, streaming_compress_to_frames};

fn make_blob(data: Bytes) -> StreamingBlob {
    // One-shot stream — small-object single-PUT shape.
    let chunks = vec![Ok::<_, std::io::Error>(data)];
    StreamingBlob::wrap(stream::iter(chunks))
}

fn build_registry() -> Arc<CodecRegistry> {
    let r = CodecRegistry::new(CodecKind::CpuZstd)
        .with(Arc::new(Passthrough))
        .with(Arc::new(CpuZstd::default()));
    Arc::new(r)
}

/// Realistic small-object payload — partially compressible (mix of repeated
/// runs and a counter), so the absolute compressed sizes are meaningful and
/// not dominated by zstd literally outputting "1024 zeros" in a few bytes.
fn synth(size: usize) -> Bytes {
    let mut buf = Vec::with_capacity(size);
    let mut counter: u64 = 0;
    while buf.len() < size {
        let run = (size - buf.len()).min(4096);
        buf.extend(std::iter::repeat_n(b'x', run / 2));
        for _ in 0..(run / 16) {
            buf.extend_from_slice(&counter.to_le_bytes());
            counter += 1;
        }
    }
    buf.truncate(size);
    Bytes::from(buf)
}

#[tokio::main]
async fn main() {
    println!("# v0.4 #18 — framed single-PUT wire-overhead micro-bench\n");
    println!("Compares S4F2 framed output (the v0.2 #4 unified single-PUT path)");
    println!("against the raw cpu-zstd compressed bytes on three small-object sizes.");
    println!(
        "Each size produces a single S4F2 frame (chunk_size = {} MiB),",
        DEFAULT_S4F2_CHUNK_SIZE / (1024 * 1024)
    );
    println!("so the framed output is exactly raw_compressed + 28 bytes header.\n");

    let registry = build_registry();
    let raw_codec = CpuZstd::default();

    let sizes = [
        ("1 KiB", 1024usize),
        ("100 KiB", 100 * 1024),
        ("1 MiB", 1024 * 1024),
    ];

    println!("| size | raw_compressed | framed | overhead_bytes | overhead_pct |");
    println!("|---|---:|---:|---:|---:|");

    for (label, size) in sizes {
        let data = synth(size);

        // Raw cpu-zstd compressed bytes (no S4F2 wrapping) — the legacy
        // single-PUT body shape, used here as the apples-to-apples baseline.
        let (raw_compressed, _raw_manifest) = raw_codec
            .compress(data.clone())
            .await
            .expect("raw compress");
        let raw_len = raw_compressed.len();

        // S4F2 framed single-PUT output (the v0.2 #4 path).
        let blob = make_blob(data.clone());
        // v0.8.4 #73 M2: bench harness controls the input stream end-to-end
        // (it isn't a truncating client) so pass `None` for `expected_size`.
        let (framed, _manifest) = streaming_compress_to_frames(
            blob,
            Arc::clone(&registry),
            CodecKind::CpuZstd,
            DEFAULT_S4F2_CHUNK_SIZE,
            None,
        )
        .await
        .expect("framed compress");
        let framed_len = framed.len();

        let overhead = framed_len as i64 - raw_len as i64;
        let overhead_pct = (overhead as f64 / raw_len as f64) * 100.0;

        println!("| {label} | {raw_len} B | {framed_len} B | +{overhead} B | {overhead_pct:.2}% |");
    }

    println!();
    println!("Expected overhead per object: 28 bytes = `FRAME_HEADER_BYTES`");
    println!("(`\"S4F2\"` u32 magic + codec_id u32 + original_size u64 + ");
    println!("compressed_size u64 + crc32c u32). Single-frame payloads pay it once.");
}
