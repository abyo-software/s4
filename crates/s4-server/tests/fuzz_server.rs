//! Property-based fuzz of s4-server helpers (proptest)。
//!
//! `s4-codec/tests/fuzz_parsers.rs` が低層 (frame/index parser) を担当、
//! こちらは server 層の `resolve_range`、`collect_blob`、`SamplingDispatcher`
//! 等の不変式を検証する。
//!
//! `cargo test -p s4-server --test fuzz_server` で実行。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::stream;
use proptest::prelude::*;
use s3s::dto::*;
use s3s::dto::{Range, StreamingBlob};
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use s4_codec::CodecKind;
use s4_codec::dispatcher::{AlwaysDispatcher, CodecDispatcher, SamplingDispatcher};
use s4_codec::{CodecRegistry, passthrough::Passthrough};
use s4_server::S4Service;
use s4_server::blob::{bytes_to_blob, collect_blob, peek_sample};
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

// ====== v0.7 #49: URL-parse hardening — keys with arbitrary bytes ======

/// Minimal in-memory S3 backend used only by the v0.7 #49 byte-coverage
/// fuzz; mirrors the `roundtrip.rs` MemoryBackend but trims it to the
/// 4 ops the fuzz exercises (PUT / GET / HEAD / DELETE) so this test
/// file stays self-contained.
struct FuzzBackend {
    inner: Arc<Mutex<HashMap<(String, String), Bytes>>>,
}

impl FuzzBackend {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl S3 for FuzzBackend {
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let body = match req.input.body.take() {
            Some(blob) => collect_blob(blob, 1024 * 1024).await.map_err(|e| {
                S3Error::with_message(S3ErrorCode::InternalError, format!("collect: {e}"))
            })?,
            None => Bytes::new(),
        };
        self.inner
            .lock()
            .unwrap()
            .insert((req.input.bucket.clone(), req.input.key.clone()), body);
        Ok(S3Response::new(PutObjectOutput::default()))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let bytes = {
            let lock = self.inner.lock().unwrap();
            lock.get(&key).cloned()
        };
        let bytes = bytes.ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        let len = bytes.len() as i64;
        let out = GetObjectOutput {
            body: Some(bytes_to_blob(bytes)),
            content_length: Some(len),
            ..Default::default()
        };
        Ok(S3Response::new(out))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let lock = self.inner.lock().unwrap();
        let stored = lock
            .get(&key)
            .ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        let out = HeadObjectOutput {
            content_length: Some(stored.len() as i64),
            ..Default::default()
        };
        Ok(S3Response::new(out))
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        self.inner.lock().unwrap().remove(&key);
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }
}

fn make_fuzz_service() -> S4Service<FuzzBackend> {
    let registry = Arc::new(CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)));
    let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::Passthrough));
    S4Service::new(FuzzBackend::new(), registry, dispatcher)
}

/// Helper: synthesise a request with the literal "/" URI (the only
/// always-safe Uri value); the per-handler logic uses
/// `req.input.bucket / .key` for routing, so the request URI is
/// metadata only — fuzz inputs can therefore vary the key freely
/// without poisoning the test harness itself.
fn put_req(bucket: &str, key: &str) -> S3Request<PutObjectInput> {
    S3Request {
        input: PutObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            body: Some(bytes_to_blob(Bytes::from_static(b"x"))),
            ..Default::default()
        },
        method: http::Method::PUT,
        uri: "/".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn get_req(bucket: &str, key: &str) -> S3Request<GetObjectInput> {
    S3Request {
        input: GetObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            ..Default::default()
        },
        method: http::Method::GET,
        uri: "/".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn head_req(bucket: &str, key: &str) -> S3Request<HeadObjectInput> {
    S3Request {
        input: HeadObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            ..Default::default()
        },
        method: http::Method::HEAD,
        uri: "/".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn delete_req(bucket: &str, key: &str) -> S3Request<DeleteObjectInput> {
    S3Request {
        input: DeleteObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            ..Default::default()
        },
        method: http::Method::DELETE,
        uri: "/".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

/// v0.7 #49: every byte 0x00..=0xFF (as a 1-byte UTF-8-lossy key) plus
/// a battery of awkward Unicode scalars must be PUT / GET / HEAD /
/// DELETE-able through `S4Service` without panicking. Whether the
/// handler returns Ok or a typed `S3Error` is irrelevant — the
/// previous `format!(...).parse::<Uri>().unwrap()` call sites would
/// crash the worker on, e.g. raw `\n` or `\0`; this test is the
/// regression guard for that crash.
#[test]
fn fuzz_keys_with_every_byte() {
    let rt = rt();
    let s4 = make_fuzz_service();
    let bucket = "test-bucket";

    // Build the corpus: 256 single-byte keys + selected adversarial
    // Unicode scalars that historically broke URI parsers.
    let mut keys: Vec<String> = (0u8..=255)
        .map(|b| String::from_utf8_lossy(&[b]).into_owned())
        .collect();
    // Common DoS / homoglyph / hidden-bidi inputs.
    keys.push("\u{202E}".into()); // RTL override
    keys.push("\u{0000}".into()); // NULL via Unicode escape (= byte 0x00, dup ok)
    keys.push("\u{FEFF}".into()); // BOM / zero-width no-break space
    keys.push("\u{200B}".into()); // zero-width space
    keys.push("\u{200E}".into()); // LTR mark
    keys.push("\u{2028}".into()); // line separator
    keys.push("\u{2029}".into()); // paragraph separator
    // Surrogate code points are *not* valid in `&str` (Rust enforces
    // well-formed UTF-8), so we cover the unpaired-surrogate concern
    // by feeding the bytes that *would* form one (already in the
    // 0x00..=0xFF sweep above as raw lone bytes — `from_utf8_lossy`
    // turns them into U+FFFD, which is itself a worthwhile input).
    // High-plane / non-BMP scalars:
    keys.push("\u{1F4A9}".into()); // U+1F4A9 PILE OF POO (4-byte UTF-8)
    keys.push("\u{10FFFF}".into()); // last legal scalar
    // Long mixed-script + control sandwich:
    keys.push("a/\u{0000}b\u{202E}c/\u{FEFF}日本".into());

    for key in &keys {
        // PUT must not panic.
        let _ = rt.block_on(s4.put_object(put_req(bucket, key)));
        // GET must not panic (may return NoSuchKey for keys whose PUT
        // failed; either outcome is acceptable for this regression).
        let _ = rt.block_on(s4.get_object(get_req(bucket, key)));
        // HEAD must not panic.
        let _ = rt.block_on(s4.head_object(head_req(bucket, key)));
        // DELETE must not panic; this is the path that exercises the
        // sidecar-DELETE re-entry (line ~2818 pre-fix), which used to
        // unwrap on `format!("/{bucket}/{}", sidecar_key(&key))`.
        let _ = rt.block_on(s4.delete_object(delete_req(bucket, key)));
    }
}
