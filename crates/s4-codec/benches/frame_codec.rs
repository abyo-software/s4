//! v0.9 — criterion regression-tracking benches for the multipart
//! frame writer (`write_frame`) and reader (`FrameIter`). These are
//! the hot paths every multipart GET runs through; regressing them
//! costs every read of a multipart object.
//!
//! Bench shape: a synthetic object of N data frames (with realistic
//! payload sizes) gets serialized once, then `FrameIter` walks the
//! whole buffer per sample. Two frame counts (16 / 256) probe both
//! "few large parts" and "many medium parts" layouts; 4 KiB padding
//! between every pair of data frames exercises the padding-skip
//! branch of the iterator.

use bytes::{Bytes, BytesMut};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use s4_codec::CodecKind;
use s4_codec::multipart::{
    FRAME_HEADER_BYTES, FrameHeader, FrameIter, pad_to_minimum, write_frame,
};

/// Build a multipart buffer of `n_frames` data frames, each carrying a
/// `payload_bytes`-long payload, with a small padding frame between
/// every pair (so the iterator's `S4P1` skip path is exercised). The
/// payload is the same deterministic xorshift stream as
/// `codec_roundtrip.rs` so the criterion JSON is comparable.
fn build_multipart_buffer(n_frames: usize, payload_bytes: usize) -> Bytes {
    let mut buf = BytesMut::new();
    let mut state: u64 = 0x1234_5678_9abc_def0;
    for i in 0..n_frames {
        let mut payload = Vec::with_capacity(payload_bytes);
        while payload.len() < payload_bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            payload.extend_from_slice(&state.to_le_bytes());
        }
        payload.truncate(payload_bytes);
        let header = FrameHeader {
            codec: CodecKind::CpuZstd,
            original_size: payload_bytes as u64 * 2, // arbitrary > compressed
            compressed_size: payload_bytes as u64,
            crc32c: 0xdead_beef ^ (i as u32),
        };
        write_frame(&mut buf, header, &payload);
        // Padding between frames to keep the iterator exercising the
        // S4P1 skip branch. 1 KiB padded shape — big enough to not be
        // a degenerate header-only frame, small enough to not dominate
        // the buffer size.
        let target = buf.len() + 1024;
        pad_to_minimum(&mut buf, target);
    }
    buf.freeze()
}

fn bench_write_frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_frame");
    // Two payload sizes — 4 KiB (small per-frame header overhead
    // dominates) and 256 KiB (the memcpy of the payload dominates).
    for payload_bytes in [4 * 1024usize, 256 * 1024] {
        let payload = {
            let mut state: u64 = 0xfeed_face_dead_beef;
            let mut v = Vec::with_capacity(payload_bytes);
            while v.len() < payload_bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                v.extend_from_slice(&state.to_le_bytes());
            }
            v.truncate(payload_bytes);
            v
        };
        let header = FrameHeader {
            codec: CodecKind::CpuZstd,
            original_size: payload_bytes as u64 * 2,
            compressed_size: payload_bytes as u64,
            crc32c: 0xcafe_d00d,
        };
        group.throughput(Throughput::Bytes(
            (FRAME_HEADER_BYTES + payload_bytes) as u64,
        ));
        group.bench_with_input(
            BenchmarkId::new("single", format!("{}KiB", payload_bytes / 1024)),
            &payload,
            |b, p| {
                b.iter(|| {
                    let mut buf = BytesMut::with_capacity(FRAME_HEADER_BYTES + p.len());
                    write_frame(&mut buf, header, p);
                    std::hint::black_box(buf);
                });
            },
        );
    }
    group.finish();
}

fn bench_frame_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_iter");
    // (label, n_frames, per-frame payload). Total bench buffer:
    //   16 frames × 64 KiB ≈ 1 MiB; 256 frames × 4 KiB ≈ 1 MiB.
    // Throughput is reported in total buffer bytes so the criterion
    // numbers map to "MiB of multipart object parsed per second".
    let shapes: &[(&str, usize, usize)] =
        &[("16f_64KiB", 16, 64 * 1024), ("256f_4KiB", 256, 4 * 1024)];
    for (label, n_frames, payload) in shapes {
        let buf = build_multipart_buffer(*n_frames, *payload);
        group.throughput(Throughput::Bytes(buf.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &buf, |b, buf| {
            b.iter(|| {
                let mut count = 0usize;
                for frame in FrameIter::new(buf.clone()) {
                    let (hdr, payload) = frame.unwrap();
                    std::hint::black_box(&hdr);
                    std::hint::black_box(&payload);
                    count += 1;
                }
                std::hint::black_box(count);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_write_frame, bench_frame_iter);
criterion_main!(benches);
