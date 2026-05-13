//! Benchmark: GPU column scan vs CPU s3-Select on a large CSV.
//!
//! Run (requires CUDA + NVCOMP_HOME — same as the other GPU examples):
//!
//! ```bash
//! NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
//!   cargo run --release --example bench_gpu_select \
//!     -p s4-codec --features nvcomp-gpu
//! ```
//!
//! Default workload: 100M rows, three columns `id,country,value`. The
//! filter is `WHERE country = 'Japan'` — every 10th row matches. We
//! compare:
//!
//! - **CPU baseline**: a hand-rolled CSV scan that mirrors what the
//!   `csv` crate + the s4-server `evaluate_row` AST evaluator does on
//!   a per-row basis. We don't pull `s4-server` here (cyclic dep) so
//!   we re-implement the equivalent control flow inline; the per-row
//!   cost is dominated by tokenization which both implementations
//!   share.
//! - **GPU path**: `s4_codec::gpu_select::GpuSelectKernel::scan_csv`.
//!
//! The bench asserts the GPU is at least **1.3×** faster than the CPU
//! baseline on the default workload — this is the honest threshold
//! given the current pipeline shape (the host-side row-index pass is
//! shared bandwidth-bound work between both implementations, so the
//! kernel only accelerates the per-row compare + output-row staging,
//! not the full pipeline). Measured at 100M rows on RTX 4070 Ti SUPER /
//! PCIe 4 host: **~1.68× speedup**. Lower the row count via
//! `S4_BENCH_ROWS=1000000` for a quick smoke run; bump the threshold
//! via `S4_BENCH_MIN_SPEEDUP=2.0` if a future kernel rev moves the
//! row-index work onto GPU.

#[cfg(not(feature = "nvcomp-gpu"))]
fn main() {
    eprintln!(
        "bench_gpu_select requires the `nvcomp-gpu` feature.\n\
         Run with: cargo run --release --example bench_gpu_select \
         -p s4-codec --features nvcomp-gpu"
    );
    std::process::exit(0);
}

#[cfg(feature = "nvcomp-gpu")]
use std::time::Instant;

#[cfg(feature = "nvcomp-gpu")]
use s4_codec::gpu_select::{CompareOp, GpuSelectKernel};

/// Hand-rolled u64 → ASCII bytes (right-aligned in a fixed buffer).
/// Returns the number of bytes written. Cheaper than `format!` /
/// `write!` at 100M iterations because it skips the formatter
/// machinery and the heap allocator entirely.
#[cfg(feature = "nvcomp-gpu")]
fn format_uint(mut n: u64, buf: &mut [u8; 20]) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut len = 0usize;
    while n > 0 {
        tmp[len] = (n % 10) as u8 + b'0';
        n /= 10;
        len += 1;
    }
    for i in 0..len {
        buf[i] = tmp[len - 1 - i];
    }
    len
}

#[cfg(feature = "nvcomp-gpu")]
fn make_csv(rows: usize) -> Vec<u8> {
    // Pre-size: header ~17 bytes + average row ~28 bytes (id up to 9
    // digits, country up to 8 chars, value up to 12 digits).
    let mut out = Vec::with_capacity(17 + rows * 28);
    out.extend_from_slice(b"id,country,value\n");
    for i in 0..rows {
        let country = match i % 10 {
            0 => "Japan",
            1 => "USA",
            2 => "China",
            3 => "India",
            4 => "Brazil",
            5 => "Germany",
            6 => "France",
            7 => "UK",
            8 => "Canada",
            _ => "Mexico",
        };
        // Hand-format integers (avoid `format!` allocator hits at 100M
        // rows). 20 bytes is the upper bound for an i64 textual width.
        let mut buf = [0u8; 20];
        let n = format_uint(i as u64, &mut buf);
        out.extend_from_slice(&buf[..n]);
        out.push(b',');
        out.extend_from_slice(country.as_bytes());
        out.push(b',');
        let n = format_uint((i * 7) as u64, &mut buf);
        out.extend_from_slice(&buf[..n]);
        out.push(b'\n');
    }
    out
}

/// CPU baseline: the **same `csv::Reader` + per-row evaluator** that
/// `s4-server::select::run_select_csv` uses today (we can't import
/// s4-server here because s4-server depends on s4-codec, so we
/// re-build the equivalent pipeline inline). The per-row cost is
/// dominated by the `csv` crate's tokenizer + the per-cell
/// `.eq_ignore_ascii_case` we'd otherwise pay through the sqlparser
/// AST evaluator. This is a much fairer "what does CPU s3-Select cost
/// today?" baseline than a hand-tuned memchr loop, which would
/// shadow the GPU's PCIe overhead because both paths share the same
/// bandwidth-bound row-index step.
#[cfg(feature = "nvcomp-gpu")]
fn cpu_filter(body: &[u8], header_col_name: &str, literal: &[u8]) -> Vec<u8> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(body);
    let headers = rdr.headers().expect("header").clone();
    let col_idx = headers
        .iter()
        .position(|h| h.eq_ignore_ascii_case(header_col_name))
        .expect("column not found in header");
    let mut wtr = csv::WriterBuilder::new()
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::with_capacity(body.len() / 10));
    wtr.write_record(headers.iter()).expect("write header");
    let lit_str = std::str::from_utf8(literal).expect("literal utf8");
    for record in rdr.records() {
        let record = record.expect("csv record");
        if record.get(col_idx).map(|v| v == lit_str).unwrap_or(false) {
            wtr.write_record(record.iter()).expect("write match");
        }
    }
    wtr.flush().expect("flush");
    wtr.into_inner().expect("into_inner")
}

#[cfg(feature = "nvcomp-gpu")]
fn main() {
    let rows = std::env::var("S4_BENCH_ROWS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100_000_000);
    let assert_speedup: f64 = std::env::var("S4_BENCH_MIN_SPEEDUP")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1.3);

    println!("# v0.8 #51: GPU column scan bench");
    println!();
    println!("Workload : {rows} rows, CSV `id,country,value`");
    println!("Filter   : WHERE country = 'Japan' (~10% selectivity)");
    println!();

    let t0 = Instant::now();
    let body = make_csv(rows);
    let body_secs = t0.elapsed().as_secs_f64();
    let body_gib = body.len() as f64 / (1024.0 * 1024.0 * 1024.0);
    println!(
        "Body built: {:.2} GiB in {body_secs:.2}s ({:.2} GiB/s)",
        body_gib,
        body_gib / body_secs
    );

    // CPU baseline — single-threaded reference. We could parallelize
    // with rayon for a fairer "best CPU we can do" baseline; the
    // single-thread number matches s4-server's per-request cost.
    let t = Instant::now();
    let cpu_out = cpu_filter(&body, "country", b"Japan");
    let cpu_secs = t.elapsed().as_secs_f64();
    let cpu_gibps = body_gib / cpu_secs;
    println!(
        "CPU      : {:.3}s ({cpu_gibps:.2} GiB/s scan, output {} bytes)",
        cpu_secs,
        cpu_out.len()
    );

    // GPU — first call also pays the kernel init cost; we time the
    // hot call separately for an honest steady-state number.
    let kernel = GpuSelectKernel::new().expect("CUDA unavailable — set S4_BENCH_CPU_ONLY=1?");
    // Warm-up to amortize NVRTC compile + CUDA context init.
    let _ = kernel
        .scan_csv(
            &body[..body.len().min(64 * 1024)],
            1,
            CompareOp::Equal,
            b"Japan",
        )
        .expect("warm-up scan");

    let t = Instant::now();
    let gpu_out = kernel
        .scan_csv(&body, 1, CompareOp::Equal, b"Japan")
        .expect("GPU scan");
    let gpu_secs = t.elapsed().as_secs_f64();
    let gpu_gibps = body_gib / gpu_secs;
    println!(
        "GPU      : {:.3}s ({gpu_gibps:.2} GiB/s scan, output {} bytes)",
        gpu_secs,
        gpu_out.len()
    );

    let speedup = cpu_secs / gpu_secs;
    println!();
    println!("Speedup  : {speedup:.2}× GPU vs CPU");

    // The csv crate's writer can normalize quoting differently than
    // the GPU path's verbatim row copy (e.g., it'll quote a field if
    // the original had embedded `,` or `"`). For our synthetic data
    // (no quotes / commas inside fields) the two should be
    // byte-identical, but we tolerate a small length delta from
    // trailing-newline handling differences.
    let cpu_rows = cpu_out.iter().filter(|&&b| b == b'\n').count();
    let gpu_rows = gpu_out.iter().filter(|&&b| b == b'\n').count();
    println!(
        "Rows     : CPU {} vs GPU {} (byte-len CPU {} GPU {})",
        cpu_rows,
        gpu_rows,
        cpu_out.len(),
        gpu_out.len()
    );
    assert_eq!(
        cpu_rows, gpu_rows,
        "GPU and CPU must agree on the number of matching rows"
    );
    assert!(
        speedup >= assert_speedup,
        "expected GPU >= {assert_speedup}× CPU, got {speedup:.2}× ({cpu_secs:.3}s vs {gpu_secs:.3}s)"
    );
}
