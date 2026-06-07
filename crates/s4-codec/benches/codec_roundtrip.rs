//! v0.9 — criterion regression-tracking benches for the CPU codecs that
//! ship in the default build (no `nvcomp-gpu` feature). Three codecs
//! (`cpu-zstd`, `cpu-gzip`, `passthrough`) × three input sizes
//! (1 KiB / 1 MiB / 16 MiB) × two halves (compress / decompress) gives
//! ~18 bench points, well under the 30-target CI budget.
//!
//! The `cpu-zstd` codec also gets a level sweep (1 / 3 / 22) on the
//! 1 MiB workload so a regression in the `zstd-safe` crate's mid-range
//! path is loud at any compression setting.
//!
//! Throughput is reported in **uncompressed bytes per second** — the
//! convention nvCOMP / lz4 / zstd publish and what the existing
//! `README.md` Benchmarks table uses. This keeps the criterion JSON
//! comparable to the ad-hoc `examples/bench_codecs.rs` numbers.
//!
//! GPU codecs (`nvcomp-*`, `dietgpu-*`) are deliberately out of scope —
//! GitHub-hosted runners don't have a CUDA-capable GPU, so feature-gating
//! them in would either skip the bench or fail CI.

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use s4_codec::Codec;
use s4_codec::cpu_gzip::CpuGzip;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::passthrough::Passthrough;

/// Sizes deliberately bracketed to cover (a) per-call overhead at 1 KiB,
/// (b) the typical S4 chunk at 1 MiB, (c) the larger end of a single
/// part at 16 MiB. Anything bigger than 16 MiB pushes the CI bench step
/// over the soft 2-minute budget we want to keep under
/// `benchmark-action/github-action-benchmark`.
const SIZES: &[(&str, usize)] = &[("1KiB", 1 << 10), ("1MiB", 1 << 20), ("16MiB", 16 << 20)];

/// Mildly-compressible synthetic input. `xorshift64` over a 256-entry
/// table keeps the entropy roughly constant across sizes so the
/// criterion timing isn't dominated by the input generator's behaviour.
/// Using a constant seed makes the bench input deterministic across runs
/// — required for the `benchmark-action` regression diff to be
/// meaningful.
fn synthetic_input(size: usize) -> Bytes {
    let mut state: u64 = 0xdead_beef_cafe_babe;
    let mut buf = Vec::with_capacity(size);
    // Mix repeating tokens (good for entropy coders) with the xorshift
    // stream (poison-pill for trivial RLE). The 50/50 split lands
    // around the 2-3× ratio range that real-world S4 traffic sits in.
    let tokens: &[&[u8]] = &[
        b"GET /api/v1/users/12345 HTTP/1.1\r\n",
        b"\"timestamp\":\"2026-06-07T12:34:56Z\",",
        b"user_id=98765&session=abcd",
    ];
    let mut i: usize = 0;
    while buf.len() < size {
        if i.is_multiple_of(2) {
            buf.extend_from_slice(tokens[i % tokens.len()]);
        } else {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            buf.extend_from_slice(&state.to_le_bytes());
        }
        i += 1;
    }
    buf.truncate(size);
    Bytes::from(buf)
}

/// `criterion::async_executor` is a heavy dep; the codecs run their
/// `spawn_blocking` payload synchronously when called from a current-thread
/// runtime, so the bench drives them through a per-iteration `block_on`
/// instead of pulling in `tokio-test` or `futures::executor::block_on`'s
/// extra workspace dep. The runtime build is the slim default
/// (`rt` + `rt-multi-thread`) already in the crate's dev-deps, so this
/// matches the existing test harness.
fn block_on<F: std::future::Future>(rt: &tokio::runtime::Runtime, fut: F) -> F::Output {
    rt.block_on(fut)
}

fn bench_compress(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio current-thread runtime");
    let mut group = c.benchmark_group("compress");
    for (label, size) in SIZES {
        let input = synthetic_input(*size);
        group.throughput(Throughput::Bytes(*size as u64));

        let zstd = CpuZstd::default();
        group.bench_with_input(BenchmarkId::new("cpu_zstd_lvl3", label), &input, |b, i| {
            b.iter(|| {
                let (compressed, _manifest) = block_on(&rt, zstd.compress(i.clone())).unwrap();
                std::hint::black_box(compressed);
            });
        });

        let gzip = CpuGzip::default();
        group.bench_with_input(BenchmarkId::new("cpu_gzip_lvl6", label), &input, |b, i| {
            b.iter(|| {
                let (compressed, _manifest) = block_on(&rt, gzip.compress(i.clone())).unwrap();
                std::hint::black_box(compressed);
            });
        });

        let passthrough = Passthrough;
        group.bench_with_input(BenchmarkId::new("passthrough", label), &input, |b, i| {
            b.iter(|| {
                let (compressed, _manifest) =
                    block_on(&rt, passthrough.compress(i.clone())).unwrap();
                std::hint::black_box(compressed);
            });
        });
    }
    group.finish();
}

fn bench_decompress(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio current-thread runtime");
    let mut group = c.benchmark_group("decompress");
    for (label, size) in SIZES {
        let input = synthetic_input(*size);
        group.throughput(Throughput::Bytes(*size as u64));

        // Pre-compute each codec's compressed payload + manifest once so
        // the timed loop measures decompress only, not the compress-half
        // setup. `criterion::Bencher::iter` re-runs the closure many times
        // per sample; cloning `Bytes` is O(1) (refcount).
        let zstd = CpuZstd::default();
        let (zstd_compressed, zstd_manifest) =
            block_on(&rt, zstd.compress(input.clone())).expect("cpu_zstd compress");
        group.bench_with_input(
            BenchmarkId::new("cpu_zstd_lvl3", label),
            &(zstd_compressed, zstd_manifest),
            |b, (c_bytes, manifest)| {
                b.iter(|| {
                    let out = block_on(&rt, zstd.decompress(c_bytes.clone(), manifest)).unwrap();
                    std::hint::black_box(out);
                });
            },
        );

        let gzip = CpuGzip::default();
        let (gzip_compressed, gzip_manifest) =
            block_on(&rt, gzip.compress(input.clone())).expect("cpu_gzip compress");
        group.bench_with_input(
            BenchmarkId::new("cpu_gzip_lvl6", label),
            &(gzip_compressed, gzip_manifest),
            |b, (c_bytes, manifest)| {
                b.iter(|| {
                    let out = block_on(&rt, gzip.decompress(c_bytes.clone(), manifest)).unwrap();
                    std::hint::black_box(out);
                });
            },
        );

        let passthrough = Passthrough;
        let (pt_compressed, pt_manifest) =
            block_on(&rt, passthrough.compress(input.clone())).expect("passthrough compress");
        group.bench_with_input(
            BenchmarkId::new("passthrough", label),
            &(pt_compressed, pt_manifest),
            |b, (c_bytes, manifest)| {
                b.iter(|| {
                    let out =
                        block_on(&rt, passthrough.decompress(c_bytes.clone(), manifest)).unwrap();
                    std::hint::black_box(out);
                });
            },
        );
    }
    group.finish();
}

/// Level sweep on 1 MiB only — covers the cheap (`lvl=1`), default
/// (`lvl=3`), and max (`lvl=22`) corners so a regression in any
/// `zstd-safe` cost-model branch is loud. Larger sizes at `lvl=22`
/// would push the bench past CI's 2-minute soft budget.
fn bench_zstd_levels(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio current-thread runtime");
    let mut group = c.benchmark_group("cpu_zstd_levels_1MiB");
    let input = synthetic_input(1 << 20);
    group.throughput(Throughput::Bytes((1 << 20) as u64));
    for level in [1i32, 3, 22] {
        let zstd = CpuZstd::new(level);
        group.bench_with_input(BenchmarkId::new("compress", level), &input, |b, i| {
            b.iter(|| {
                let (compressed, _manifest) = block_on(&rt, zstd.compress(i.clone())).unwrap();
                std::hint::black_box(compressed);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_compress, bench_decompress, bench_zstd_levels);
criterion_main!(benches);
