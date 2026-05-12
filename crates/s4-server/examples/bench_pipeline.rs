//! Compares sequential vs pipelined `streaming_compress_to_frames` (v0.3 #12).
//!
//! Run:
//!   NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
//!     cargo run --release --example bench_pipeline \
//!       -p s4-server --features nvcomp-gpu

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures::stream;
use s3s::dto::StreamingBlob;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::streaming::{DEFAULT_S4F2_CHUNK_SIZE, streaming_compress_to_frames_with};

fn make_blob(data: Bytes, chunk: usize) -> StreamingBlob {
    let chunks: Vec<_> = data
        .chunks(chunk)
        .map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c)))
        .collect();
    StreamingBlob::wrap(stream::iter(chunks))
}

fn build_registry(default: CodecKind) -> Arc<CodecRegistry> {
    #[allow(unused_mut)]
    let mut r = CodecRegistry::new(default)
        .with(Arc::new(Passthrough))
        .with(Arc::new(CpuZstd::default()));
    #[cfg(feature = "nvcomp-gpu")]
    {
        use s4_codec::nvcomp::{NvcompGDeflateCodec, NvcompZstdCodec, is_gpu_available};
        if is_gpu_available() {
            if let Ok(c) = NvcompZstdCodec::new() {
                r = r.with(Arc::new(c));
            }
            if let Ok(c) = NvcompGDeflateCodec::new() {
                r = r.with(Arc::new(c));
            }
        }
    }
    Arc::new(r)
}

fn synth(size: usize) -> Bytes {
    // Highly compressible mix: alternating runs of 'x' and a counter.
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

async fn measure(
    codec: CodecKind,
    registry: Arc<CodecRegistry>,
    data: Bytes,
    chunk_size: usize,
    inflight: usize,
) -> (f64, u64) {
    let t0 = Instant::now();
    let blob = make_blob(data.clone(), 64 * 1024);
    let (out, _) =
        streaming_compress_to_frames_with(blob, Arc::clone(&registry), codec, chunk_size, inflight)
            .await
            .unwrap();
    let secs = t0.elapsed().as_secs_f64();
    (secs, out.len() as u64)
}

#[tokio::main]
async fn main() {
    println!("# v0.3 #12 — streaming_compress_to_frames pipelining bench\n");

    let total_size = 1024 * 1024 * 1024; // 1 GiB
    println!("Input: 1 GiB highly-compressible synthetic mix");
    println!(
        "Chunk size: {} MiB",
        DEFAULT_S4F2_CHUNK_SIZE / (1024 * 1024)
    );
    println!();
    println!("| Codec | inflight | Time | Throughput | Compressed | Ratio |");
    println!("|---|---:|---:|---:|---:|---:|");

    let data = synth(total_size);

    let codecs = [
        ("cpu-zstd", CodecKind::CpuZstd),
        #[cfg(feature = "nvcomp-gpu")]
        ("nvcomp-zstd", CodecKind::NvcompZstd),
    ];
    let inflights = [1usize, 3, 6];

    for (label, codec) in codecs {
        let registry = build_registry(codec);
        for inflight in inflights {
            let (secs, comp_size) = measure(
                codec,
                Arc::clone(&registry),
                data.clone(),
                DEFAULT_S4F2_CHUNK_SIZE,
                inflight,
            )
            .await;
            let gbps = (total_size as f64) / secs / 1e9;
            let ratio = total_size as f64 / comp_size as f64;
            println!(
                "| {label} | {inflight} | {secs:.2}s | {gbps:.2} GB/s | {comp_mib} MiB | {ratio:.2}× |",
                comp_mib = comp_size / (1024 * 1024),
            );
        }
    }

    println!();
    println!("inflight=1 = sequential baseline; inflight=3 = the new default;");
    println!("inflight=6 = stress test (more memory, less speedup unless reader-bound).");
}
