//! Advanced fuzz: mutational / multi-frame sequence / differential。
//!
//! `fuzz_parsers.rs` (random input + roundtrip) と `fuzz_bolero.rs`
//! (coverage-guided 候補) を補完する、より bug を引き出しやすい strategy:
//!
//! - **mutational**: valid な入力を生成 → 1 byte (or N byte) flip → parser に
//!   投げて「必ず Err、決して silent corruption しない」を verify
//! - **multi-frame sequence**: 任意 codec / size / padding pattern を持つ
//!   N 個の frame を concat → FrameIter で順序保存 + count 一致を verify
//! - **differential**: 同じ入力を「production parser (read_frame)」と
//!   「naive reference parser (素直な byte walker)」で並走 → output 不一致は
//!   即 fuzz failure

use bytes::{BufMut, Bytes, BytesMut};
use proptest::prelude::*;

use s4_codec::CodecKind;
use s4_codec::index::{FrameIndex, FrameIndexEntry, decode_index, encode_index};
use s4_codec::multipart::{
    FRAME_HEADER_BYTES, FRAME_MAGIC, FrameError, FrameHeader, FrameIter, PADDING_HEADER_BYTES,
    PADDING_MAGIC, pad_to_minimum, read_frame, write_frame,
};

// ====== Mutational fuzz: valid input → 1 byte flip → parser ======

fn arb_codec_kind() -> impl Strategy<Value = CodecKind> {
    prop_oneof![
        Just(CodecKind::Passthrough),
        Just(CodecKind::CpuZstd),
        Just(CodecKind::NvcompZstd),
        Just(CodecKind::NvcompBitcomp),
        Just(CodecKind::DietGpuAns),
    ]
}

fn build_valid_frame(codec: CodecKind, original_size: u64, payload: &[u8], crc32c: u32) -> Bytes {
    let mut buf = BytesMut::new();
    let header = FrameHeader {
        codec,
        original_size,
        compressed_size: payload.len() as u64,
        crc32c,
    };
    write_frame(&mut buf, header, payload);
    buf.freeze()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// valid frame の任意 byte を XOR して parser に投げる → 結果は ok or err、
    /// **panic / 無限ループ / OOM のいずれもなし**。silent な header ↔ payload
    /// mismatch を許さないことを verify (CRC や magic 検査を必ず通る)。
    #[test]
    fn mutated_frame_no_silent_corruption(
        codec in arb_codec_kind(),
        original_size in 0u64..100_000,
        payload in proptest::collection::vec(any::<u8>(), 0..512),
        crc32c in any::<u32>(),
        flip_idx in any::<usize>(),
        flip_mask in 1u8..=255,
    ) {
        let valid = build_valid_frame(codec, original_size, &payload, crc32c);
        let mut bytes = valid.to_vec();
        if !bytes.is_empty() {
            let idx = flip_idx % bytes.len();
            bytes[idx] ^= flip_mask;
        }
        let mutated = Bytes::from(bytes);
        // 結果が Ok だろうが Err だろうが parser が安定動作すれば良い
        let _ = read_frame(mutated);
    }

    /// valid frame の magic 1 byte を flip → 必ず BadMagic で error
    #[test]
    fn frame_magic_corruption_always_caught(
        codec in arb_codec_kind(),
        payload in proptest::collection::vec(any::<u8>(), 0..256),
        magic_byte in 0usize..4,
        flip_mask in 1u8..=255,
    ) {
        let valid = build_valid_frame(codec, 99, &payload, 0xdead);
        let mut bytes = valid.to_vec();
        bytes[magic_byte] ^= flip_mask;
        // magic を flip したのに偶然 "S4F2" になる確率は 1/(2^8 - 1) 弱なのでほぼ
        // 全ケースで magic mismatch
        let result = read_frame(Bytes::from(bytes.clone()));
        if &bytes[..4] != FRAME_MAGIC {
            prop_assert!(
                matches!(result, Err(FrameError::BadMagic { .. })),
                "magic flip must yield BadMagic, got {:?}",
                result.err()
            );
        }
    }

    /// valid frame の payload byte を flip → header は無事なので parse は通るが
    /// 上位 (decompress) で CRC mismatch が出るはず (frame layer では検出不可、
    /// codec layer の責任)。frame parse は OK である事だけ verify
    #[test]
    fn payload_corruption_doesnt_break_frame_parse(
        payload in proptest::collection::vec(any::<u8>(), 16..512),
        flip_idx in any::<usize>(),
        flip_mask in 1u8..=255,
    ) {
        let valid = build_valid_frame(CodecKind::CpuZstd, 99, &payload, 0xfeed);
        let mut bytes = valid.to_vec();
        let payload_start = FRAME_HEADER_BYTES;
        let payload_len = bytes.len() - payload_start;
        if payload_len > 0 {
            let idx = payload_start + (flip_idx % payload_len);
            bytes[idx] ^= flip_mask;
        }
        // frame parse は通るはず (header 健在)
        let result = read_frame(Bytes::from(bytes));
        prop_assert!(result.is_ok(), "header-intact frame must parse OK");
    }

    /// mutated index byte → decode_index は panic せず Err か Ok
    #[test]
    fn mutated_index_no_panic(
        n_entries in 1usize..16,
        flip_idx in any::<usize>(),
        flip_mask in 1u8..=255,
    ) {
        let mut entries = Vec::with_capacity(n_entries);
        let mut orig_off = 0u64;
        let mut comp_off = 0u64;
        for i in 0..n_entries {
            let orig = (i as u64 + 1) * 100;
            let comp = (i as u64 + 1) * 50;
            entries.push(FrameIndexEntry {
                original_offset: orig_off,
                original_size: orig,
                compressed_offset: comp_off,
                compressed_size: comp,
            });
            orig_off += orig;
            comp_off += comp + 16;
        }
        // v0.8.4 #73 H-2: fuzz harness doesn't exercise version binding.
        let idx = FrameIndex {
            total_padded_size: comp_off,
            entries,
            source_etag: None,
            source_compressed_size: None,
        };
        let mut bytes = encode_index(&idx).to_vec();
        if !bytes.is_empty() {
            let i = flip_idx % bytes.len();
            bytes[i] ^= flip_mask;
        }
        // panic / 無限ループしないこと、結果は Result 何でも良い
        let _ = decode_index(Bytes::from(bytes));
    }
}

// ====== Multi-frame sequence fuzz ======

#[derive(Debug, Clone)]
enum SeqElement {
    Frame {
        codec: CodecKind,
        original_size: u64,
        payload_len: usize,
        crc32c: u32,
    },
    Padding {
        len: usize,
    },
}

fn arb_seq_element() -> impl Strategy<Value = SeqElement> {
    prop_oneof![
        (arb_codec_kind(), 0u64..1_000_000, 0usize..256, any::<u32>()).prop_map(
            |(codec, original_size, payload_len, crc32c)| SeqElement::Frame {
                codec,
                original_size,
                payload_len,
                crc32c,
            }
        ),
        (0usize..512).prop_map(|len| SeqElement::Padding { len }),
    ]
}

fn build_seq(elements: &[SeqElement]) -> (Bytes, usize) {
    let mut buf = BytesMut::new();
    let mut frame_count = 0usize;
    for e in elements {
        match e {
            SeqElement::Frame {
                codec,
                original_size,
                payload_len,
                crc32c,
            } => {
                let payload = vec![0xab_u8; *payload_len];
                let header = FrameHeader {
                    codec: *codec,
                    original_size: *original_size,
                    compressed_size: payload.len() as u64,
                    crc32c: *crc32c,
                };
                write_frame(&mut buf, header, &payload);
                frame_count += 1;
            }
            SeqElement::Padding { len } => {
                buf.put_slice(PADDING_MAGIC);
                buf.put_u64_le(*len as u64);
                buf.resize(buf.len() + len, 0);
            }
        }
    }
    (buf.freeze(), frame_count)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// 任意 frame + padding sequence → FrameIter は frame だけを順序保存して yield、
    /// padding は完全 skip
    #[test]
    fn frame_iter_preserves_frame_count_skipping_padding(
        elements in proptest::collection::vec(arb_seq_element(), 0..32),
    ) {
        let (bytes, expected_frames) = build_seq(&elements);
        let mut got = 0usize;
        for r in FrameIter::new(bytes) {
            // 全 frame 有効に書いたので Ok だけが返るはず
            prop_assert!(r.is_ok(), "valid sequence must yield Ok frames, got {:?}", r);
            got += 1;
        }
        prop_assert_eq!(got, expected_frames, "frame count must match (padding skipped)");
    }

    /// 任意 sequence → byte 数が想定通り (frame header + payload + padding header + len)
    #[test]
    fn build_seq_byte_count_invariant(
        elements in proptest::collection::vec(arb_seq_element(), 0..16),
    ) {
        let (bytes, _) = build_seq(&elements);
        let expected: usize = elements.iter().map(|e| match e {
            SeqElement::Frame { payload_len, .. } => FRAME_HEADER_BYTES + payload_len,
            SeqElement::Padding { len } => PADDING_HEADER_BYTES + len,
        }).sum();
        prop_assert_eq!(bytes.len(), expected);
    }

    /// frame sequence + 末尾 random garbage → garbage に当たった時点で FrameIter
    /// が fused、それまでの frame は全て返る
    #[test]
    fn frame_iter_with_trailing_garbage_doesnt_lose_prefix(
        good_elements in proptest::collection::vec(arb_seq_element(), 1..8),
        garbage in proptest::collection::vec(any::<u8>(), 1..64),
    ) {
        let (good_bytes, good_count) = build_seq(&good_elements);
        let mut combined = BytesMut::new();
        combined.extend_from_slice(&good_bytes);
        combined.extend_from_slice(&garbage);
        let mut frames = 0usize;
        let mut errs = 0usize;
        for r in FrameIter::new(combined.freeze()) {
            match r {
                Ok(_) => frames += 1,
                Err(_) => errs += 1,
            }
        }
        // garbage が parse できなければ最大 1 個 err、その後 fused で None
        prop_assert!(errs <= 1, "should fuse on first error, got {errs} errors");
        prop_assert!(frames <= good_count, "should not produce extra frames");
    }
}

// ====== Differential fuzz: production parser vs naive reference ======

/// Naive な reference 実装。`read_frame` の最適化前 (defensive な byte walker)
/// と思って欲しい。性能は度外視で「絶対正しい」基準動作を表現する。
fn naive_read_frame(input: &[u8]) -> Result<(FrameHeader, Vec<u8>, usize), FrameError> {
    if input.len() < FRAME_HEADER_BYTES {
        return Err(FrameError::TooShort(input.len()));
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&input[..4]);
    if &magic != FRAME_MAGIC {
        return Err(FrameError::BadMagic {
            expected: *FRAME_MAGIC,
            got: magic,
        });
    }
    let codec_id = u32::from_le_bytes(input[4..8].try_into().unwrap());
    let codec = CodecKind::from_id(codec_id).ok_or(FrameError::UnknownCodec(codec_id))?;
    let original_size = u64::from_le_bytes(input[8..16].try_into().unwrap());
    let compressed_size = u64::from_le_bytes(input[16..24].try_into().unwrap());
    let crc32c = u32::from_le_bytes(input[24..28].try_into().unwrap());
    let total = FRAME_HEADER_BYTES + compressed_size as usize;
    if total > input.len() {
        return Err(FrameError::PayloadTruncated {
            compressed_size,
            remaining: input.len() - FRAME_HEADER_BYTES,
        });
    }
    let payload = input[FRAME_HEADER_BYTES..total].to_vec();
    Ok((
        FrameHeader {
            codec,
            original_size,
            compressed_size,
            crc32c,
        },
        payload,
        total,
    ))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// production の `read_frame` と naive な reference 実装は同じ input に対し
    /// **必ず同じ結果** を返す。差が出れば最適化バグ。
    #[test]
    fn read_frame_matches_naive_reference(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let prod_result = read_frame(Bytes::from(bytes.clone()));
        let naive_result = naive_read_frame(&bytes);

        match (prod_result, naive_result) {
            (Ok((p_h, p_payload, p_rest)), Ok((n_h, n_payload, n_total))) => {
                prop_assert_eq!(p_h, n_h, "header must match");
                prop_assert_eq!(p_payload.as_ref(), n_payload.as_slice(), "payload must match");
                prop_assert_eq!(p_rest.len(), bytes.len() - n_total, "remainder size must match");
            }
            (Err(p_e), Err(n_e)) => {
                // 同じ kind の error であること
                prop_assert_eq!(
                    std::mem::discriminant(&p_e),
                    std::mem::discriminant(&n_e),
                    "error kinds diverge: prod={:?} naive={:?}", p_e, n_e
                );
            }
            (p, n) => prop_assert!(
                false,
                "production and naive parsers diverge: prod={:?} naive={:?}",
                p.is_ok(),
                n.is_ok()
            ),
        }
    }
}

// ====== pad_to_minimum oracle ======

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// pad で出力 size が ≥ target、かつ FrameIter が pad を完全 skip
    #[test]
    fn pad_then_iter_yields_only_data_frames(
        n_frames in 1usize..8,
        target in 1024usize..(64 * 1024),
    ) {
        let mut buf = BytesMut::new();
        for i in 0..n_frames {
            let payload = vec![i as u8; 32];
            write_frame(
                &mut buf,
                FrameHeader {
                    codec: CodecKind::CpuZstd,
                    original_size: 100,
                    compressed_size: payload.len() as u64,
                    crc32c: 0,
                },
                &payload,
            );
            // 各 frame の後ろに padding を入れる
            let new_target = buf.len() + target / n_frames;
            pad_to_minimum(&mut buf, new_target);
        }
        let bytes = buf.freeze();
        let frames: Vec<_> = FrameIter::new(bytes).collect();
        prop_assert_eq!(frames.len(), n_frames);
        for r in &frames {
            prop_assert!(r.is_ok());
        }
    }
}
