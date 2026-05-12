//! Property-based fuzz of s4-server helpers (proptest)。
//!
//! `s4-codec/tests/fuzz_parsers.rs` が低層 (frame/index parser) を担当、
//! こちらは server 層の `resolve_range`、`collect_blob`、`SamplingDispatcher`
//! 等の不変式を検証する。
//!
//! `cargo test -p s4-server --test fuzz_server` で実行。

use bytes::Bytes;
use futures::stream;
use proptest::prelude::*;
use s3s::dto::{Range, StreamingBlob};
use s4_codec::CodecKind;
use s4_codec::dispatcher::{AlwaysDispatcher, CodecDispatcher, SamplingDispatcher};
use s4_server::blob::{collect_blob, peek_sample};
use s4_server::service::resolve_range;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn make_blob_from_chunks(chunks: Vec<Bytes>) -> StreamingBlob {
    let s = stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
    StreamingBlob::wrap(s)
}

// ====== resolve_range: overflow / 境界 ======

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// resolve_range with arbitrary Range::Int + total → 必ず Result、panic / overflow しない
    #[test]
    fn resolve_range_int_no_panic(
        first in any::<u64>(),
        last_opt in proptest::option::of(any::<u64>()),
        total in any::<u64>(),
    ) {
        let r = Range::Int { first, last: last_opt };
        let _ = resolve_range(&r, total);
    }

    /// resolve_range with arbitrary Range::Suffix → panic / underflow しない
    #[test]
    fn resolve_range_suffix_no_panic(
        length in any::<u64>(),
        total in any::<u64>(),
    ) {
        let r = Range::Suffix { length };
        let _ = resolve_range(&r, total);
    }

    /// 成功 case: 返した (start, end) は 必ず start < end <= total
    #[test]
    fn resolve_range_int_invariant(
        total in 1u64..1_000_000,
        first in 0u64..1_000_000,
        last in 0u64..1_000_000,
    ) {
        let r = Range::Int { first, last: Some(last) };
        if let Ok((start, end)) = resolve_range(&r, total) {
            prop_assert!(start <= end);
            prop_assert!(end <= total);
            prop_assert!(start < total);
        }
    }

    /// Suffix 成功 case: end 必ず total、start = total - min(length, total)
    #[test]
    fn resolve_range_suffix_invariant(
        total in 1u64..1_000_000,
        length in 0u64..2_000_000,
    ) {
        let r = Range::Suffix { length };
        if let Ok((start, end)) = resolve_range(&r, total) {
            prop_assert_eq!(end, total);
            prop_assert!(start <= end);
            prop_assert!(end - start <= length || length == 0);
        }
    }

    /// total = 0 → 必ず Err (空 object に Range は意味なし)
    #[test]
    fn resolve_range_zero_total_always_err(
        first in any::<u64>(),
        length in any::<u64>(),
    ) {
        let int_r = Range::Int { first, last: None };
        prop_assert!(resolve_range(&int_r, 0).is_err());
        let suf_r = Range::Suffix { length };
        prop_assert!(resolve_range(&suf_r, 0).is_err());
    }
}

// ====== collect_blob: arbitrary chunk shape ======

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// arbitrary な chunk 列を collect_blob すると順序保ったまま結合される
    #[test]
    fn collect_blob_concatenates_chunks_in_order(
        chunks in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..256),
            0..16,
        ),
    ) {
        let chunk_bytes: Vec<Bytes> = chunks.iter().map(|c| Bytes::from(c.clone())).collect();
        let expected: Vec<u8> = chunks.into_iter().flatten().collect();
        let blob = make_blob_from_chunks(chunk_bytes);
        let rt = rt();
        let result = rt.block_on(async { collect_blob(blob, 64 * 1024).await });
        match result {
            Ok(bytes) => prop_assert_eq!(bytes.as_ref(), expected.as_slice()),
            Err(_) if expected.len() > 64 * 1024 => {} // oversized は OK
            Err(e) => prop_assert!(false, "unexpected error: {e}"),
        }
    }

    /// max_bytes を超えた入力は **必ず** Oversized で拒否 (DoS 防御)
    #[test]
    fn collect_blob_oversized_always_rejects(
        max_bytes in 1usize..1024,
        excess in 1usize..1024,
    ) {
        let total = max_bytes + excess;
        let blob = make_blob_from_chunks(vec![Bytes::from(vec![0u8; total])]);
        let rt = rt();
        let result = rt.block_on(async { collect_blob(blob, max_bytes).await });
        prop_assert!(result.is_err(), "blob of {total} bytes must be rejected with max={max_bytes}");
    }

    /// peek_sample(blob, N) で取得した sample.len() <= N、rest は残り全部
    #[test]
    fn peek_sample_split_invariant(
        body in proptest::collection::vec(any::<u8>(), 0..2048),
        peek_n in 1usize..512,
    ) {
        let total_len = body.len();
        let blob = make_blob_from_chunks(vec![Bytes::from(body.clone())]);
        let rt = rt();
        let (sample, rest) = rt.block_on(async {
            peek_sample(blob, peek_n).await.unwrap()
        });
        prop_assert!(sample.len() <= peek_n);
        prop_assert!(sample.len() <= total_len);
        let rest_collected = rt.block_on(async { collect_blob(rest, 64 * 1024).await.unwrap() });
        prop_assert_eq!(sample.len() + rest_collected.len(), total_len);
        // sample + rest を結合すると元 body と一致
        let mut combined = sample.to_vec();
        combined.extend_from_slice(&rest_collected);
        prop_assert_eq!(combined, body);
    }
}

// ====== Dispatcher invariants ======

fn arb_codec_kind() -> impl Strategy<Value = CodecKind> {
    prop_oneof![
        Just(CodecKind::Passthrough),
        Just(CodecKind::CpuZstd),
        Just(CodecKind::NvcompZstd),
        Just(CodecKind::NvcompBitcomp),
        Just(CodecKind::DietGpuAns),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// SamplingDispatcher は random sample に対し常に有効な CodecKind を返す
    #[test]
    fn sampling_dispatcher_pick_returns_known_kind(
        sample in proptest::collection::vec(any::<u8>(), 0..8192),
        default in arb_codec_kind(),
    ) {
        let d = SamplingDispatcher::new(default);
        let rt = rt();
        let kind = rt.block_on(d.pick(&sample));
        // CodecKind::id() は閉じた enum なので必ず < 6
        prop_assert!(kind.id() < 6);
    }

    /// AlwaysDispatcher は常に設定した kind を返す (任意 input 不変)
    #[test]
    fn always_dispatcher_returns_configured(
        sample in proptest::collection::vec(any::<u8>(), 0..1024),
        kind in arb_codec_kind(),
    ) {
        let d = AlwaysDispatcher(kind);
        let rt = rt();
        let got = rt.block_on(d.pick(&sample));
        prop_assert_eq!(got, kind);
    }
}
