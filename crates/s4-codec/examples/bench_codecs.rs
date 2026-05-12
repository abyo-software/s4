//! Quick local benchmark harness — drives the s4-codec roundtrip for
//! CPU zstd / nvCOMP zstd / nvCOMP GDeflate across three synthetic
//! workloads and prints a markdown table of ratio + GB/s. Used to
//! generate the numbers in README.md "Benchmarks" section.
//!
//! Run:
//!   NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
//!     cargo run --release --example bench_codecs \
//!       -p s4-codec --features nvcomp-gpu

use std::time::Instant;

use bytes::Bytes;
use s4_codec::Codec as CodecAsync;
use s4_codec::cpu_zstd::CpuZstd;

/// Returns (uncompressed_size, compressed_size, compress_secs, decompress_secs).
async fn measure_codec<C: CodecAsync + ?Sized>(codec: &C, data: Bytes) -> (u64, u64, f64, f64) {
    let original_size = data.len() as u64;
    let t0 = Instant::now();
    let (compressed, manifest) = codec.compress(data.clone()).await.unwrap();
    let comp_secs = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let decompressed = codec
        .decompress(compressed.clone(), &manifest)
        .await
        .unwrap();
    let dec_secs = t1.elapsed().as_secs_f64();
    assert_eq!(decompressed, data, "codec roundtrip must match");

    (original_size, compressed.len() as u64, comp_secs, dec_secs)
}

/// Workload generators
mod workload {
    use bytes::Bytes;

    /// Realistic-ish nginx access log line repeated to fill the size.
    pub fn nginx_log(size: usize) -> Bytes {
        let line = b"203.0.113.42 - - [12/May/2026:10:30:45 +0000] \"GET /api/v1/users/123 HTTP/1.1\" 200 4521 \"https://example.com/dashboard\" \"Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36\"\n";
        let mut buf = Vec::with_capacity(size);
        let mut counter: u64 = 0;
        while buf.len() < size {
            // sprinkle a unique counter so it's not trivially compressible to ~1 byte
            buf.extend_from_slice(line);
            buf.extend_from_slice(format!(" id={counter:016x}\n").as_bytes());
            counter += 1;
        }
        buf.truncate(size);
        Bytes::from(buf)
    }

    /// Parquet-like: 4 KiB blocks alternating "small u32 values" (highly
    /// compressible numeric column) with text metadata (modest compressibility).
    pub fn parquet_like(size: usize) -> Bytes {
        let mut buf = Vec::with_capacity(size);
        let mut counter: u32 = 0;
        while buf.len() < size {
            // 4 KiB of u32 counter (numeric column, ~5x compression)
            for _ in 0..1024 {
                buf.extend_from_slice(&counter.to_le_bytes());
                counter = counter.wrapping_add(1);
            }
            // 1 KiB of metadata text
            for _ in 0..32 {
                buf.extend_from_slice(b"col=user_id,type=u32,encoding=plain\n");
            }
        }
        buf.truncate(size);
        Bytes::from(buf)
    }

    /// Already-compressed proxy: pseudo-random bytes (high entropy).
    /// Stand-in for jpeg/zip/tar.gz uploads — nothing more should compress.
    pub fn already_compressed(size: usize) -> Bytes {
        let mut buf = Vec::with_capacity(size);
        let mut state: u64 = 0xdead_beef_cafe_babe;
        while buf.len() < size {
            // xorshift64 — fast, deterministic, statistically random
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            buf.extend_from_slice(&state.to_le_bytes());
        }
        buf.truncate(size);
        Bytes::from(buf)
    }
}

fn fmt_gbps(bytes: u64, secs: f64) -> String {
    if secs <= 0.0 {
        return "n/a".into();
    }
    let gbps = (bytes as f64) / secs / 1e9;
    format!("{gbps:.2} GB/s")
}

fn fmt_ratio(orig: u64, comp: u64) -> String {
    if comp == 0 {
        return "∞".into();
    }
    let r = orig as f64 / comp as f64;
    format!("{r:.2}×")
}

#[tokio::main]
async fn main() {
    println!("# S4 codec benchmark\n");
    println!("Hardware: {}", host_label());
    println!();
    println!("| Workload | Codec | Original | Compressed | Ratio | Compress | Decompress |");
    println!("|---|---|---:|---:|---:|---:|---:|");

    let workloads: Vec<(&str, Bytes)> = vec![
        (
            "nginx access log (256 MiB)",
            workload::nginx_log(256 * 1024 * 1024),
        ),
        (
            "parquet-like (256 MiB)",
            workload::parquet_like(256 * 1024 * 1024),
        ),
        (
            "already-compressed (64 MiB)",
            workload::already_compressed(64 * 1024 * 1024),
        ),
    ];

    fn print_row(label: &str, codec: &str, orig: u64, comp: u64, c: f64, d: f64) {
        // Standard convention: throughput is reported in **uncompressed bytes
        // per second** for both compress (input rate) and decompress (output
        // rate). That matches the way nvCOMP, lz4, and zstd publish numbers.
        println!(
            "| {label} | {codec} | {orig_mib} MiB | {comp_mib} MiB | {ratio} | {c_gbps} | {d_gbps} |",
            orig_mib = orig / (1024 * 1024),
            comp_mib = comp / (1024 * 1024),
            ratio = fmt_ratio(orig, comp),
            c_gbps = fmt_gbps(orig, c),
            d_gbps = fmt_gbps(orig, d),
        );
    }

    for (label, data) in workloads {
        // CPU zstd level 3 (the s4-server default)
        let cpu = CpuZstd::default();
        let (orig, comp, c, d) = measure_codec(&cpu, data.clone()).await;
        print_row(label, "cpu-zstd-3", orig, comp, c, d);

        #[cfg(feature = "nvcomp-gpu")]
        {
            use s4_codec::nvcomp::{NvcompGDeflateCodec, NvcompZstdCodec, is_gpu_available};
            if is_gpu_available() {
                if let Ok(codec) = NvcompZstdCodec::new() {
                    let (o, cz, t_c, t_d) = measure_codec(&codec, data.clone()).await;
                    print_row(label, "nvcomp-zstd", o, cz, t_c, t_d);
                }
                if let Ok(codec) = NvcompGDeflateCodec::new() {
                    let (o, cz, t_c, t_d) = measure_codec(&codec, data.clone()).await;
                    print_row(label, "nvcomp-gdeflate", o, cz, t_c, t_d);
                }
            } else {
                println!("(GPU not detected at runtime — skipping nvcomp rows for this workload)");
            }
        }
        #[cfg(not(feature = "nvcomp-gpu"))]
        let _ = label;
    }
    println!();
    println!("Single-pass measurement, single replicate. Numbers are indicative,");
    println!("not paper-grade — for reproducible head-to-head benches see issue #14.");
}

fn host_label() -> String {
    let cpu = std::process::Command::new("sh")
        .arg("-c")
        .arg("grep -m1 'model name' /proc/cpuinfo | sed 's/model name\\s*: //'")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "(unknown CPU)".into());
    let gpu = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no GPU)".into());
    format!("CPU `{cpu}`, GPU `{gpu}`")
}
