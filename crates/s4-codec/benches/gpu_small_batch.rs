//! v1.2 `--gpu-batch-small-puts` throughput evidence bench.
//!
//! Compares three ways of compressing 1000 × 8 KiB small objects:
//!
//!   (a) cpu-zstd (level 3) sequential — the flag-off PUT path today
//!   (b) nvcomp-zstd per-object sequential — one kernel launch per object
//!       (why `--gpu-min-bytes` exists)
//!   (c) nvcomp-zstd batched, 32 objects per `compress_batch` call — one
//!       kernel launch per 32 objects (the v1.2 batch aggregator path)
//!
//! Reports wall time + total compressed size for each. Requires a
//! CUDA-capable GPU + `--features nvcomp-gpu` build; exits gracefully
//! otherwise so `cargo bench` on CPU-only checkouts stays green.
//!
//! Run:
//!   NVCOMP_HOME=... cargo bench -p s4-codec --features nvcomp-gpu \
//!     --bench gpu_small_batch
//!
//! `harness = false`: this is a wall-clock A/B/C evidence harness, not a
//! criterion regression tracker (the GPU gate + one-shot batch shape
//! doesn't fit criterion's sampling model).

#[cfg(not(feature = "nvcomp-gpu"))]
fn main() {
    eprintln!("gpu_small_batch bench requires --features nvcomp-gpu; skipping");
}

#[cfg(feature = "nvcomp-gpu")]
fn main() {
    imp::run();
}

#[cfg(feature = "nvcomp-gpu")]
mod imp {
    use std::time::Instant;

    use bytes::Bytes;
    use s4_codec::Codec;
    use s4_codec::cpu_zstd::CpuZstd;
    use s4_codec::nvcomp::{NvcompZstdCodec, is_gpu_available};
    use s4_codec::nvcomp_batched::NvcompZstdBatchEncoder;

    const NUM_OBJECTS: usize = 1000;
    const OBJECT_BYTES: usize = 8 * 1024;
    const BATCH_SIZE: usize = 32;

    /// Synthetic log-like 8 KiB body: repeated structure with per-line
    /// variability (realistic zstd ratio, not a degenerate all-'x' run).
    fn make_object(i: usize) -> Bytes {
        let mut v = Vec::with_capacity(OBJECT_BYTES + 128);
        let mut line_no = 0usize;
        while v.len() < OBJECT_BYTES {
            v.extend_from_slice(
                format!(
                    "2026-06-10T12:{:02}:{:02}Z INFO svc=ingest obj={i:05} line={line_no:06} \
                     status=200 latency_ms={} bytes={}\n",
                    (line_no / 60) % 60,
                    line_no % 60,
                    (i * 31 + line_no * 7) % 250,
                    (i * 13 + line_no * 101) % 65536,
                )
                .as_bytes(),
            );
            line_no += 1;
        }
        v.truncate(OBJECT_BYTES);
        Bytes::from(v)
    }

    pub fn run() {
        if !is_gpu_available() {
            eprintln!("gpu_small_batch bench: no CUDA GPU detected; skipping");
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");

        let objects: Vec<Bytes> = (0..NUM_OBJECTS).map(make_object).collect();
        let total_in: usize = objects.iter().map(|o| o.len()).sum();

        let cpu = CpuZstd::default(); // level 3 — same as the PUT path default
        let gpu = NvcompZstdCodec::new().expect("nvcomp init");
        let batch = NvcompZstdBatchEncoder::new().expect("batch encoder init");

        // Warmup: GPU context + buffer pools + zstd contexts out of the
        // measured region.
        rt.block_on(async {
            let _ = cpu.compress(objects[0].clone()).await.expect("cpu warmup");
            let _ = gpu.compress(objects[0].clone()).await.expect("gpu warmup");
        });
        let _ = batch
            .compress_batch(&objects[..BATCH_SIZE])
            .expect("batch warmup");

        // (a) cpu-zstd-3 sequential
        let t = Instant::now();
        let mut cpu_out = 0usize;
        rt.block_on(async {
            for o in &objects {
                let (c, _m) = cpu.compress(o.clone()).await.expect("cpu compress");
                cpu_out += c.len();
            }
        });
        let cpu_wall = t.elapsed();

        // (b) nvcomp-zstd per-object sequential
        let t = Instant::now();
        let mut gpu_seq_out = 0usize;
        rt.block_on(async {
            for o in &objects {
                let (c, _m) = gpu.compress(o.clone()).await.expect("gpu compress");
                gpu_seq_out += c.len();
            }
        });
        let gpu_seq_wall = t.elapsed();

        // (c) nvcomp-zstd batched, 32 per launch
        let t = Instant::now();
        let mut gpu_batch_out = 0usize;
        for chunk in objects.chunks(BATCH_SIZE) {
            let results = batch.compress_batch(chunk).expect("batch compress");
            for r in results {
                let (c, _m) = r.expect("batch item");
                gpu_batch_out += c.len();
            }
        }
        let gpu_batch_wall = t.elapsed();

        let report = |name: &str, wall: std::time::Duration, out: usize| {
            let secs = wall.as_secs_f64();
            println!(
                "{name:<28} wall={:>8.1} ms  objs/s={:>9.0}  in-throughput={:>7.1} MB/s  \
                 total-compressed={out} B (ratio {:.2}x)",
                secs * 1e3,
                NUM_OBJECTS as f64 / secs,
                total_in as f64 / secs / 1e6,
                total_in as f64 / out as f64,
            );
        };
        println!(
            "gpu_small_batch: {NUM_OBJECTS} objects x {OBJECT_BYTES} B (total {total_in} B), \
             batch={BATCH_SIZE}"
        );
        report("(a) cpu-zstd-3 sequential", cpu_wall, cpu_out);
        report("(b) nvcomp-zstd per-object", gpu_seq_wall, gpu_seq_out);
        report("(c) nvcomp-zstd batched", gpu_batch_wall, gpu_batch_out);
    }
}
