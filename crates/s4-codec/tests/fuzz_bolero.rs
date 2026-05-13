//! Bolero fuzz targets — coverage-guided fuzzing on stable Rust。
//!
//! ## なぜ proptest と別個に bolero を持つか
//!
//! - **proptest**: random / structural property generator。stable で軽量。
//!   `tests/fuzz_parsers.rs` が担当。
//! - **bolero**: 同じ `check!` API で複数 fuzz engine (libfuzzer / honggfuzz /
//!   AFL / Kani / random) に dispatch できる。**coverage-guided** で新 branch
//!   を狙う fuzzer に switch 可能 (要 nightly Rust + engine 別 install)。
//!
//! ## 実行方法
//!
//! ```bash
//! # 1. CI / dev で軽く回す (random engine、cargo test と同じ感覚)
//! cargo test --test fuzz_bolero
//!
//! # 2. 本格 coverage-guided fuzz (24h 等、要 nightly Rust)
//! cargo install cargo-bolero
//! cargo bolero test --engine libfuzzer frame_parser_bolero -- -max_total_time=86400
//!
//! # 3. crash artifact を replay
//! cargo bolero test --engine libfuzzer frame_parser_bolero -- corpus/<crash-input>
//! ```
//!
//! corpus は `crates/s4-codec/tests/__fuzz__/<target>/corpus/` 以下に蓄積される。

use bytes::{Bytes, BytesMut};
use s4_codec::ChunkManifest;
use s4_codec::Codec;
use s4_codec::CodecKind;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::index::{FrameIndex, FrameIndexEntry, decode_index, encode_index};
use s4_codec::multipart::{FrameIter, read_frame};
use s4_codec::passthrough::Passthrough;

#[test]
fn frame_parser_bolero() {
    bolero::check!()
        .with_type::<Vec<u8>>()
        .for_each(|input: &Vec<u8>| {
            // 任意 input → panic / 無限ループ なし
            let _ = read_frame(Bytes::from(input.clone()));
        });
}

#[test]
fn frame_iter_bolero() {
    bolero::check!()
        .with_type::<Vec<u8>>()
        .for_each(|input: &Vec<u8>| {
            // FrameIter は必ず terminate (Result が err になっても fused で None になる)
            let mut count = 0usize;
            for r in FrameIter::new(Bytes::from(input.clone())) {
                let _ = r;
                count += 1;
                assert!(
                    count < 10_000,
                    "FrameIter must terminate even on adversarial input"
                );
            }
        });
}

#[test]
fn index_decoder_bolero() {
    bolero::check!()
        .with_type::<Vec<u8>>()
        .for_each(|input: &Vec<u8>| {
            // decode_index は任意 byte 列に対し panic せず Result
            let _ = decode_index(Bytes::from(input.clone()));
        });
}

#[test]
fn cpu_zstd_decompress_bolero() {
    let codec = CpuZstd::default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    bolero::check!().with_type::<(Vec<u8>, u32)>().for_each(
        |(input, claimed_orig): &(Vec<u8>, u32)| {
            // claimed_orig を u64 に拡張 (大きい値も試す)
            let manifest = ChunkManifest {
                codec: CodecKind::CpuZstd,
                original_size: *claimed_orig as u64,
                compressed_size: input.len() as u64,
                crc32c: 0,
            };
            let bytes = Bytes::from(input.clone());
            // 任意 input + 任意 claimed_orig → panic / OOM なし
            let _ = rt.block_on(async { codec.decompress(bytes, &manifest).await });
        },
    );
}

#[test]
fn frame_roundtrip_bolero() {
    use bolero::generator::*;
    bolero::check!()
        .with_generator((
            // codec_id 0..=5 (有効な variant)
            (0u32..6),
            // original_size
            (0u64..100_000),
            // payload (実際の長さ = compressed_size)
            produce_with::<Vec<u8>>().len(0..2048),
            // crc32c
            produce::<u32>(),
        ))
        .for_each(
            |(codec_id, orig, payload, crc): &(u32, u64, Vec<u8>, u32)| {
                let codec = CodecKind::from_id(*codec_id).unwrap();
                let header = s4_codec::multipart::FrameHeader {
                    codec,
                    original_size: *orig,
                    compressed_size: payload.len() as u64,
                    crc32c: *crc,
                };
                let mut buf = BytesMut::new();
                s4_codec::multipart::write_frame(&mut buf, header, payload);
                let (got_header, got_payload, rest) =
                    read_frame(buf.freeze()).expect("write_frame output must roundtrip");
                assert_eq!(got_header, header);
                assert_eq!(got_payload.as_ref(), payload.as_slice());
                assert!(rest.is_empty());
            },
        );
}

#[test]
fn passthrough_roundtrip_bolero() {
    let codec = Passthrough;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    bolero::check!()
        .with_type::<Vec<u8>>()
        .for_each(|input: &Vec<u8>| {
            let bytes = Bytes::from(input.clone());
            rt.block_on(async {
                let (compressed, manifest) = codec.compress(bytes.clone()).await.unwrap();
                let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
                assert_eq!(
                    decompressed, bytes,
                    "Passthrough roundtrip must be lossless"
                );
            });
        });
}

#[test]
fn index_roundtrip_bolero() {
    use bolero::generator::*;
    bolero::check!()
        .with_generator(produce_with::<Vec<(u64, u64, u64)>>().len(0..32))
        .for_each(|raw: &Vec<(u64, u64, u64)>| {
            let mut entries = Vec::with_capacity(raw.len());
            let mut orig_off = 0u64;
            let mut comp_off = 0u64;
            for &(orig, comp, gap) in raw {
                let orig = (orig % 1024) + 1;
                let comp = (comp % 1024) + 1;
                let gap = gap % 64;
                entries.push(FrameIndexEntry {
                    original_offset: orig_off,
                    original_size: orig,
                    compressed_offset: comp_off,
                    compressed_size: comp,
                });
                orig_off = orig_off.saturating_add(orig);
                comp_off = comp_off.saturating_add(comp).saturating_add(gap);
            }
            let idx = FrameIndex {
                total_padded_size: comp_off,
                entries,
                // v0.8.4 #73 H-2: bolero harness doesn't fuzz the version
                // binding fields; default-construct so the roundtrip
                // assertion below covers the (entries, padded_size) shape.
                source_etag: None,
                source_compressed_size: None,
            };
            let bytes = encode_index(&idx);
            let decoded = decode_index(bytes).expect("encoded index must decode");
            assert_eq!(decoded, idx, "FrameIndex roundtrip");
        });
}
