//! v0.9 — criterion regression-tracking benches for the S4IX sidecar
//! codec (`encode_index` / `decode_index`). Range GETs against
//! multipart objects depend on the sidecar parse staying fast; a
//! regression that doubles parse time directly doubles the cold-start
//! latency of every range request.
//!
//! Bench shape: synthesize a `FrameIndex` of N frames (128 / 1024
//! / 4096 — same order of magnitude as production sidecars; 4 MiB
//! chunks × ~16 GiB object hits ~4 K frames), encode once, then run
//! `encode_index` / `decode_index` in tight loops. `lookup_range` is
//! also benched on the 1024-frame index because it's the hot path on
//! every range request.

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use s4_codec::index::{FrameIndex, FrameIndexEntry, decode_index, encode_index};

/// Build a `FrameIndex` of `n_frames` evenly-sized 4 MiB chunks with a
/// small etag tail, matching the v0.8.4 sidecar shape the production
/// PUT path emits.
fn build_index(n_frames: usize) -> FrameIndex {
    const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
    const COMPRESSED_PER_CHUNK: u64 = 2 * 1024 * 1024; // ~2× ratio
    let mut entries = Vec::with_capacity(n_frames);
    let mut orig_off = 0u64;
    let mut comp_off = 0u64;
    for _ in 0..n_frames {
        entries.push(FrameIndexEntry {
            original_offset: orig_off,
            original_size: CHUNK_SIZE,
            compressed_offset: comp_off,
            compressed_size: COMPRESSED_PER_CHUNK,
        });
        orig_off += CHUNK_SIZE;
        comp_off += COMPRESSED_PER_CHUNK;
    }
    FrameIndex {
        total_padded_size: comp_off,
        entries,
        // Realistic AWS S3 multipart ETag shape (hex + part-count
        // suffix). The encoder writes this as variable-length bytes
        // after the v2 fixed header, so including it keeps the
        // serialized size honest.
        source_etag: Some("\"d41d8cd98f00b204e9800998ecf8427e-32\"".to_owned()),
        source_compressed_size: Some(comp_off),
    }
}

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode_index");
    for &n_frames in &[128usize, 1024, 4096] {
        let idx = build_index(n_frames);
        // Encoded bytes are 44 (v2 header) + etag + n×32. Throughput
        // reported as encoded bytes so the criterion plot reads as
        // "MiB of sidecar serialized per second".
        let encoded_len = encode_index(&idx).len();
        group.throughput(Throughput::Bytes(encoded_len as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_frames}f")),
            &idx,
            |b, idx| {
                b.iter(|| {
                    let out = encode_index(idx);
                    std::hint::black_box(out);
                });
            },
        );
    }
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_index");
    for &n_frames in &[128usize, 1024, 4096] {
        let idx = build_index(n_frames);
        let bytes: Bytes = encode_index(&idx);
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_frames}f")),
            &bytes,
            |b, bytes| {
                b.iter(|| {
                    let decoded = decode_index(bytes.clone()).unwrap();
                    std::hint::black_box(decoded);
                });
            },
        );
    }
    group.finish();
}

fn bench_lookup_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("lookup_range_1024f");
    let idx = build_index(1024);
    let total = idx.total_original_size();
    // Three range shapes that span the binary-search workload:
    //   small_head: 1 KiB starting at offset 0 (single frame).
    //   mid: 16 MiB centred at the midpoint (~4 frames).
    //   span: 256 MiB across the whole object (~64 frames).
    let cases: &[(&str, u64, u64)] = &[
        ("small_head", 0, 1024),
        ("mid_16MiB", total / 2, total / 2 + 16 * 1024 * 1024),
        (
            "span_256MiB",
            total / 4,
            (total / 4 + 256 * 1024 * 1024).min(total),
        ),
    ];
    for (label, start, end) in cases {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                let plan = idx.lookup_range(*start, *end);
                std::hint::black_box(plan);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode, bench_lookup_range);
criterion_main!(benches);
