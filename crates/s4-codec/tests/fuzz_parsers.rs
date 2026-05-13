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

use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::index::{FrameIndex, FrameIndexEntry, decode_index, encode_index};
use s4_codec::multipart::{
    FRAME_HEADER_BYTES, FrameHeader, FrameIter, PADDING_HEADER_BYTES, PADDING_MAGIC,
    S3_MULTIPART_MIN_PART_BYTES, pad_to_minimum, read_frame, write_frame,
};
use s4_codec::passthrough::Passthrough;
use s4_codec::{ChunkManifest, Codec, CodecKind};

use bytes::{BufMut, Bytes, BytesMut};

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
                // v0.8.4 #73 H-2: fuzz harness doesn't exercise the version-
                // binding fields; default-construct to None so the existing
                // adversarial coverage is unchanged.
                source_etag: None,
                source_compressed_size: None,
            }
        })
}

// ====== Adversarial frame: 巨大 compressed_size 主張 → OOM 防止 ======

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256, .. ProptestConfig::default()
    })]

    /// frame header に絶対あり得ない巨大 compressed_size を書き込んで read_frame
    /// に投げる → 必ず PayloadTruncated を返す (memory 確保しない)
    #[test]
    fn adversarial_huge_compressed_size_doesnt_oom(
        codec_id in 0u32..6,
        original_size in any::<u64>(),
        crc32c in any::<u32>(),
        actual_payload_len in 0usize..256,
        liar_factor in 1u64..u64::MAX,
    ) {
        let mut buf = BytesMut::with_capacity(FRAME_HEADER_BYTES + actual_payload_len);
        buf.extend_from_slice(b"S4F2");
        buf.put_u32_le(codec_id);
        buf.put_u64_le(original_size);
        // compressed_size を実際の payload_len より絶対大きく書く
        buf.put_u64_le(actual_payload_len as u64 + liar_factor);
        buf.put_u32_le(crc32c);
        buf.resize(FRAME_HEADER_BYTES + actual_payload_len, 0);
        let result = read_frame(buf.freeze());
        // 巨大 size 主張 + 短い payload → 必ず PayloadTruncated か UnknownCodec
        prop_assert!(result.is_err(), "huge compressed_size must fail, not allocate");
    }

    /// padding frame に巨大 length を書き込んで FrameIter に投げる → 必ず error
    #[test]
    fn adversarial_huge_padding_length_doesnt_oom(
        liar_factor in 1u64..u64::MAX,
        actual_pad_payload in 0usize..64,
    ) {
        let mut buf = BytesMut::with_capacity(PADDING_HEADER_BYTES + actual_pad_payload);
        buf.extend_from_slice(PADDING_MAGIC);
        buf.put_u64_le(actual_pad_payload as u64 + liar_factor);
        buf.resize(PADDING_HEADER_BYTES + actual_pad_payload, 0);
        let mut iter = FrameIter::new(buf.freeze());
        let r = iter.next();
        prop_assert!(matches!(r, Some(Err(_))), "huge padding length must fail safely");
        // fused: 次は None
        prop_assert!(iter.next().is_none(), "FrameIter must be fused after error");
    }
}

// ====== Codec roundtrip + decompress robustness ======

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64, .. ProptestConfig::default()
    })]

    /// CPU zstd: input bytes → compress → decompress = original bytes
    #[test]
    fn cpu_zstd_compress_decompress_roundtrips(
        payload in proptest::collection::vec(any::<u8>(), 0..16384),
        level in 1i32..=10,
    ) {
        let codec = CpuZstd::new(level);
        let input = Bytes::from(payload);
        let rt = rt();
        rt.block_on(async {
            let (compressed, manifest) = codec.compress(input.clone()).await.unwrap();
            let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
            assert_eq!(decompressed, input);
        });
    }

    /// Passthrough: input → compress → decompress = original (CRC 検証含む)
    #[test]
    fn passthrough_compress_decompress_roundtrips(
        payload in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let codec = Passthrough;
        let input = Bytes::from(payload);
        let rt = rt();
        rt.block_on(async {
            let (compressed, manifest) = codec.compress(input.clone()).await.unwrap();
            let decompressed = codec.decompress(compressed, &manifest).await.unwrap();
            assert_eq!(decompressed, input);
        });
    }

    /// adversarial: random bytes を CpuZstd.decompress に投げる → 必ず Err、決して panic / OOM しない
    #[test]
    fn cpu_zstd_decompress_random_input_no_panic(
        random_payload in proptest::collection::vec(any::<u8>(), 0..2048),
        manifest_orig_size in 0u64..1_000_000,
    ) {
        let codec = CpuZstd::default();
        let payload_len = random_payload.len() as u64;
        let manifest = ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size: manifest_orig_size,
            compressed_size: payload_len,
            crc32c: 0,
        };
        let rt = rt();
        rt.block_on(async {
            // 意図的に random input、ほぼすべて Err になる前提
            let _ = codec.decompress(Bytes::from(random_payload), &manifest).await;
            // 重要なのは panic しないこと、OOM しないこと (ここまで来れば OK)
        });
    }

    /// 🔥 Zstd bomb: 小さい payload が巨大 manifest を主張する → bounded memory で error
    #[test]
    fn cpu_zstd_bomb_caps_at_manifest_size(
        bomb_seed in 0u8..255,
        big_claim in 1_000u64..10_000_000_000u64,  // up to 10 GB claim
    ) {
        // 1 KB のデータを作って圧縮 (実際の decompressed size は 1 KB)
        let real_data = vec![bomb_seed; 1024];
        let compressed = zstd::stream::encode_all(real_data.as_slice(), 3).unwrap();
        // manifest を改ざん: original_size を巨大に主張 (= 攻撃者の sidecar 改ざん想定)
        let bomb_manifest = ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size: big_claim,  // 嘘
            compressed_size: compressed.len() as u64,
            crc32c: 0,
        };
        let codec = CpuZstd::default();
        let rt = rt();
        rt.block_on(async {
            // hardening 前: codec が big_claim 分のメモリを確保しようとして OOM
            // hardening 後: take(big_claim + 1024) で読むが、実際の decompressed は
            //               1 KB なので buf には 1 KB しか入らない、
            //               次のサイズチェックで SizeMismatch を返す
            let result = codec.decompress(Bytes::from(compressed), &bomb_manifest).await;
            assert!(result.is_err(), "decompress must error on size mismatch (claim too big)");
            // OOM しない = ここまで到達することが正解
        });
    }
}

// ====== CodecKind enum 完全性 ======

proptest! {
    #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

    /// 全 CodecKind variant の id ↔ from_id roundtrip (新 codec 追加時の漏れ検出)
    #[test]
    fn codec_kind_id_roundtrip(kind in arb_codec_kind()) {
        let id = kind.id();
        prop_assert_eq!(CodecKind::from_id(id), Some(kind));
    }

    /// 全 CodecKind variant の as_str ↔ parse roundtrip
    #[test]
    fn codec_kind_str_roundtrip(kind in arb_codec_kind()) {
        let s = kind.as_str();
        let parsed: CodecKind = s.parse().unwrap_or_else(|_| {
            panic!("CodecKind::{:?}.as_str() = {:?} but parse failed", kind, s)
        });
        prop_assert_eq!(parsed, kind);
    }

    /// 未知 id は None を返す (panic しない)
    #[test]
    fn codec_kind_unknown_id_returns_none(unknown_id in 100u32..u32::MAX) {
        prop_assert!(CodecKind::from_id(unknown_id).is_none());
    }
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
