//! Property-based fuzz of parser surfaces (proptest)。
//!
//! ## 目的
//!
//! - 悪意ある / 偶然の bit-flipped 入力に対し parser が **panic / 無限ループ
//!   / OOM** しないこと
//! - serialize → deserialize roundtrip の不変式
//! - lookup_range が返す `RangePlan` の slice index が必ず in-bounds
//!
//! production data 入力は信頼できないので (S3 上の sidecar が改ざんされたケース、
//! S3 wire 上で bit が flip したケース等)、parser 側で defensive に Result を
//! 返すことが必須。
//!
//! `cargo test --test fuzz_parsers` で実行。各 property は default 256 回 / target。

use proptest::prelude::*;

use s4_codec::CodecKind;
use s4_codec::index::{FrameIndex, FrameIndexEntry, decode_index, encode_index};
use s4_codec::multipart::{
    FRAME_HEADER_BYTES, FrameHeader, FrameIter, S3_MULTIPART_MIN_PART_BYTES, pad_to_minimum,
    read_frame, write_frame,
};

use bytes::{Bytes, BytesMut};

// ====== read_frame: 任意 bytes に対して panic せず Result を返す ======
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512, .. ProptestConfig::default()
    })]

    #[test]
    fn read_frame_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = read_frame(Bytes::from(bytes));
    }

    #[test]
    fn frame_iter_terminates_on_random_input(
        bytes in proptest::collection::vec(any::<u8>(), 0..4096)
    ) {
        let mut count = 0usize;
        for r in FrameIter::new(Bytes::from(bytes)) {
            // err でも ok でも構わない、ただし無限に出続けない
            let _ = r;
            count += 1;
            // 安全弁: 1 frame 24 byte + 8 padding header = min 12 byte/iter なので
            // 入力 4096 byte なら 4096/12 ≈ 341 が上限
            prop_assert!(count < 1024);
        }
    }

    #[test]
    fn decode_index_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..16384)) {
        let _ = decode_index(Bytes::from(bytes));
    }
}

// ====== roundtrip 不変式 ======

fn arb_codec_kind() -> impl Strategy<Value = CodecKind> {
    prop_oneof![
        Just(CodecKind::Passthrough),
        Just(CodecKind::CpuZstd),
        Just(CodecKind::NvcompZstd),
        Just(CodecKind::NvcompBitcomp),
        Just(CodecKind::NvcompGans),
        Just(CodecKind::DietGpuAns),
    ]
}

#[allow(dead_code)]
fn arb_frame_header(payload_len: usize) -> impl Strategy<Value = FrameHeader> {
    (arb_codec_kind(), 0u64..1_000_000u64, any::<u32>()).prop_map(
        move |(codec, original_size, crc32c)| FrameHeader {
            codec,
            original_size,
            compressed_size: payload_len as u64,
            crc32c,
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256, .. ProptestConfig::default()
    })]

    #[test]
    fn frame_write_then_read_roundtrips(
        payload in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let payload_bytes = Bytes::from(payload);
        let header = FrameHeader {
            codec: CodecKind::CpuZstd,
            original_size: 12345,
            compressed_size: payload_bytes.len() as u64,
            crc32c: 0xfeed_face,
        };
        let mut buf = BytesMut::new();
        write_frame(&mut buf, header, &payload_bytes);
        prop_assert_eq!(buf.len(), FRAME_HEADER_BYTES + payload_bytes.len());
        let (got_header, got_payload, rest) = read_frame(buf.freeze())
            .map_err(|e| TestCaseError::fail(format!("read_frame failed: {e}")))?;
        prop_assert_eq!(got_header, header);
        prop_assert_eq!(got_payload, payload_bytes);
        prop_assert!(rest.is_empty());
    }

    #[test]
    fn frame_header_codec_id_roundtrip(
        codec in arb_codec_kind(),
        payload_len in 0usize..1024,
    ) {
        let payload = vec![0u8; payload_len];
        let header = FrameHeader {
            codec,
            original_size: 99,
            compressed_size: payload_len as u64,
            crc32c: 0,
        };
        let mut buf = BytesMut::new();
        write_frame(&mut buf, header, &payload);
        let (got, _, _) = read_frame(buf.freeze())
            .map_err(|e| TestCaseError::fail(format!("read_frame: {e}")))?;
        prop_assert_eq!(got.codec, codec);
    }

    #[test]
    fn pad_to_minimum_invariant(
        initial_len in 0usize..10_000,
        target in 0usize..1_000_000,
    ) {
        let mut buf = BytesMut::new();
        buf.resize(initial_len, 0xab);
        pad_to_minimum(&mut buf, target);
        prop_assert!(buf.len() >= target.min(initial_len.max(target)));
        if initial_len < target {
            prop_assert!(buf.len() >= target);
            // overshoot は最大で padding header の 12 byte 程度
            prop_assert!(buf.len() < target + 64);
        } else {
            prop_assert_eq!(buf.len(), initial_len);
        }
    }

    #[test]
    fn pad_to_5mib_always_meets_s3_minimum(
        initial_len in 0usize..(S3_MULTIPART_MIN_PART_BYTES + 1024),
    ) {
        let mut buf = BytesMut::new();
        buf.resize(initial_len, 0);
        pad_to_minimum(&mut buf, S3_MULTIPART_MIN_PART_BYTES);
        prop_assert!(buf.len() >= S3_MULTIPART_MIN_PART_BYTES);
    }
}

// ====== FrameIndex roundtrip + lookup_range 不変式 ======

fn arb_frame_index(max_entries: usize) -> impl Strategy<Value = FrameIndex> {
    // 連続した monotone な entry 列を一度に生成 (single PUT で arbitrary な
    // (size, gap) 列を作ってから累積する)
    (
        1usize..=max_entries,
        proptest::collection::vec((1u64..1024, 1u64..1024, 0u64..16), 1..(max_entries + 1)),
    )
        .prop_map(|(n, raw)| {
            let mut entries = Vec::with_capacity(n.min(raw.len()));
            let mut orig_off = 0u64;
            let mut comp_off = 0u64;
            for &(orig_size, comp_size, gap) in raw.iter().take(n) {
                entries.push(FrameIndexEntry {
                    original_offset: orig_off,
                    original_size: orig_size,
                    compressed_offset: comp_off,
                    compressed_size: comp_size,
                });
                orig_off += orig_size;
                comp_off += comp_size + gap; // gap = padding 等
            }
            FrameIndex {
                total_padded_size: comp_off,
                entries,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128, .. ProptestConfig::default()
    })]

    #[test]
    fn index_encode_decode_roundtrip(idx in arb_frame_index(20)) {
        let bytes = encode_index(&idx);
        let decoded = decode_index(bytes)
            .map_err(|e| TestCaseError::fail(format!("decode: {e}")))?;
        prop_assert_eq!(decoded, idx);
    }

    #[test]
    fn lookup_range_returns_in_bounds_plan(idx in arb_frame_index(10)) {
        let total = idx.total_original_size();
        if total == 0 {
            return Ok(());
        }
        // 全範囲を試す
        let plan = match idx.lookup_range(0, total) {
            Some(p) => p,
            None => return Ok(()),
        };
        // S3 byte range が index 内に収まっている
        prop_assert!(plan.byte_start < idx.total_padded_size);
        prop_assert!(plan.byte_end_exclusive <= idx.total_padded_size);
        prop_assert!(plan.byte_start <= plan.byte_end_exclusive);
        // slice index が plan の対象 frames の解凍合計サイズ内
        let combined_size: u64 = idx.entries
            [plan.first_frame_idx..=plan.last_frame_idx_inclusive]
            .iter()
            .map(|e| e.original_size)
            .sum();
        prop_assert!(plan.slice_start_in_combined <= combined_size);
        prop_assert!(plan.slice_end_in_combined <= combined_size);
        prop_assert!(plan.slice_start_in_combined <= plan.slice_end_in_combined);
    }

    #[test]
    fn lookup_range_partial_request_in_bounds(
        idx in arb_frame_index(10),
        start_frac in 0u64..1000,
        len_frac in 1u64..1000,
    ) {
        let total = idx.total_original_size();
        if total == 0 {
            return Ok(());
        }
        let start = (start_frac * total) / 1000;
        let len = (len_frac * total) / 1000 + 1;
        let end = (start + len).min(total);
        if start >= end {
            return Ok(());
        }
        if let Some(plan) = idx.lookup_range(start, end) {
            // byte range 不変式
            prop_assert!(plan.byte_end_exclusive <= idx.total_padded_size);
            prop_assert!(plan.byte_start <= plan.byte_end_exclusive);
            // slice 不変式
            let combined_size: u64 = idx.entries
                [plan.first_frame_idx..=plan.last_frame_idx_inclusive]
                .iter()
                .map(|e| e.original_size)
                .sum();
            prop_assert!(plan.slice_end_in_combined <= combined_size);
        }
    }
}
