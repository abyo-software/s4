//! End-to-end roundtrip integration test。
//!
//! 純 in-memory な S3 backend mock (`MemoryBackend`) を用意し、
//! `S4Service<MemoryBackend, _>` 経由で put → get がバイト一致することを検証する。
//! HTTP layer / aws-sdk-s3 を経由せず、`S3` trait のみで結線するので、外部依存ゼロで
//! CI で常時走らせられる。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use s3s::dto::*;
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::AlwaysDispatcher;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::blob::{bytes_to_blob, collect_blob};

fn make_registry(default: CodecKind) -> Arc<CodecRegistry> {
    Arc::new(
        CodecRegistry::new(default)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    )
}

fn make_dispatcher(kind: CodecKind) -> Arc<AlwaysDispatcher> {
    Arc::new(AlwaysDispatcher(kind))
}

/// In-memory な (bucket, key) → (body, metadata) ストア。
/// 実装するのは S4 が呼ぶ最小集合: `put_object`, `get_object`, `head_object`。
/// それ以外は trait default (NotImplemented) のまま。
///
/// `inner` を `Arc<Mutex<...>>` で持つことで test が S4Service を消費せずに
/// backend storage を覗ける (single-PUT framed body / sidecar 検証で使用)。
struct MemoryBackend {
    inner: Arc<Mutex<HashMap<(String, String), StoredObject>>>,
}

#[derive(Clone)]
struct StoredObject {
    body: Bytes,
    metadata: Option<Metadata>,
    content_type: Option<ContentType>,
}

impl MemoryBackend {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn shared(&self) -> Arc<Mutex<HashMap<(String, String), StoredObject>>> {
        Arc::clone(&self.inner)
    }
}

#[async_trait::async_trait]
impl S3 for MemoryBackend {
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let body = match req.input.body.take() {
            Some(blob) => collect_blob(blob, 100 * 1024 * 1024).await.map_err(|e| {
                S3Error::with_message(S3ErrorCode::InternalError, format!("collect: {e}"))
            })?,
            None => Bytes::new(),
        };
        let stored = StoredObject {
            body,
            metadata: req.input.metadata.clone(),
            content_type: req.input.content_type.clone(),
        };
        self.inner
            .lock()
            .unwrap()
            .insert((req.input.bucket.clone(), req.input.key.clone()), stored);
        Ok(S3Response::new(PutObjectOutput::default()))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let stored = {
            let lock = self.inner.lock().unwrap();
            lock.get(&key).cloned()
        };
        let stored = stored.ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        let len = stored.body.len() as i64;
        let out = GetObjectOutput {
            body: Some(bytes_to_blob(stored.body)),
            content_length: Some(len),
            metadata: stored.metadata,
            content_type: stored.content_type,
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
            content_length: Some(stored.body.len() as i64),
            metadata: stored.metadata.clone(),
            content_type: stored.content_type.clone(),
            ..Default::default()
        };
        Ok(S3Response::new(out))
    }
}

fn put_request(bucket: &str, key: &str, body: Bytes) -> S3Request<PutObjectInput> {
    let input = PutObjectInput {
        bucket: bucket.into(),
        key: key.into(),
        body: Some(bytes_to_blob(body)),
        ..Default::default()
    };
    S3Request {
        input,
        method: http::Method::PUT,
        uri: format!("/{bucket}/{key}").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn get_request(bucket: &str, key: &str) -> S3Request<GetObjectInput> {
    let input = GetObjectInput {
        bucket: bucket.into(),
        key: key.into(),
        ..Default::default()
    };
    S3Request {
        input,
        method: http::Method::GET,
        uri: format!("/{bucket}/{key}").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

async fn read_back(resp: S3Response<GetObjectOutput>) -> Bytes {
    collect_blob(resp.output.body.expect("body"), 100 * 1024 * 1024)
        .await
        .expect("collect")
}

#[tokio::test]
async fn cpu_zstd_roundtrip_through_s4service() {
    let backend = MemoryBackend::new();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );

    let payload = Bytes::from(vec![b'x'; 100_000]); // highly compressible
    s4.put_object(put_request("bucket", "key1", payload.clone()))
        .await
        .expect("put");

    let resp = s4
        .get_object(get_request("bucket", "key1"))
        .await
        .expect("get");
    let got = read_back(resp).await;
    assert_eq!(got, payload, "roundtrip body must match");
}

#[tokio::test]
async fn passthrough_roundtrip_through_s4service() {
    let backend = MemoryBackend::new();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::Passthrough),
        make_dispatcher(CodecKind::Passthrough),
    );

    let payload = Bytes::from_static(b"hello squished s3");
    s4.put_object(put_request("bucket", "key2", payload.clone()))
        .await
        .expect("put");

    let resp = s4
        .get_object(get_request("bucket", "key2"))
        .await
        .expect("get");
    let got = read_back(resp).await;
    assert_eq!(got, payload);
}

#[tokio::test]
async fn cpu_zstd_actually_compresses_in_backend_storage() {
    // 1 MB of repeated bytes — zstd should reduce to <10 KB。
    // 検証は S4Service の HEAD で `s4-compressed-size` metadata を読み、
    // 圧縮率が想定通り出ていることを確認する。
    let backend = MemoryBackend::new();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );

    let payload = Bytes::from(vec![b'x'; 1024 * 1024]);
    s4.put_object(put_request("bucket", "compressible", payload.clone()))
        .await
        .expect("put");

    // HEAD で metadata を取り出し、compressed_size が小さくなっていることを確認
    let head = s4
        .head_object(S3Request {
            input: HeadObjectInput {
                bucket: "bucket".into(),
                key: "compressible".into(),
                ..Default::default()
            },
            method: http::Method::HEAD,
            uri: "/bucket/compressible".parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        })
        .await
        .expect("head");
    let meta = head.output.metadata.expect("metadata must be set by S4");
    let original = meta.get("s4-original-size").expect("original-size meta");
    let compressed = meta
        .get("s4-compressed-size")
        .expect("compressed-size meta");
    let original_n: u64 = original.parse().unwrap();
    let compressed_n: u64 = compressed.parse().unwrap();
    assert_eq!(original_n, payload.len() as u64);
    assert!(
        compressed_n < original_n / 100,
        "expected zstd to compress 1 MB of x bytes to <10 KB, got {compressed_n} bytes"
    );
}

#[tokio::test]
async fn get_object_without_s4_metadata_passes_through() {
    // S4 が書いていないオブジェクトを bucket に直接置く → S4 経由 GET でそのまま返るべき
    let backend = MemoryBackend::new();
    let raw = Bytes::from_static(b"this object was put without S4 in the path");
    backend.inner.lock().unwrap().insert(
        ("bucket".into(), "raw".into()),
        StoredObject {
            body: raw.clone(),
            metadata: None,
            content_type: None,
        },
    );

    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );
    let resp = s4
        .get_object(get_request("bucket", "raw"))
        .await
        .expect("get");
    let got = read_back(resp).await;
    assert_eq!(
        got, raw,
        "raw object lacking s4 metadata must pass through unchanged"
    );
}

/// v0.2 #4: large single-PUT object becomes multi-frame S4F2 + sidecar.
///
/// Validates:
/// - Body bytes start with the S4F2 magic (= framed format)
/// - `s4-framed` metadata flag is set
/// - `<key>.s4index` sidecar is written (for multi-frame objects)
/// - Roundtrip GET reconstructs the original bytes exactly
#[tokio::test]
async fn single_put_above_chunk_size_is_framed_with_sidecar() {
    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );

    // > DEFAULT_S4F2_CHUNK_SIZE (4 MiB) → must produce >= 2 frames
    let payload = Bytes::from(vec![b'z'; 5 * 1024 * 1024]);
    s4.put_object(put_request("bucket", "framed_object", payload.clone()))
        .await
        .expect("put");

    // peek the backend to verify framed format. Clone what we need out of
    // the lock then drop it before the next await (clippy:
    // mutex_held_across_await).
    let (stored_body, stored_meta, has_sidecar) = {
        let inner = backend_view.lock().unwrap();
        let stored = inner
            .get(&("bucket".into(), "framed_object".into()))
            .unwrap();
        (
            stored.body.clone(),
            stored.metadata.clone(),
            inner.contains_key(&("bucket".into(), "framed_object.s4index".into())),
        )
    };
    assert_eq!(
        &stored_body[0..4],
        b"S4F2",
        "framed body must start with S4F2 magic"
    );
    let meta = stored_meta.as_ref().expect("must have s4 metadata");
    assert_eq!(
        meta.get("s4-framed").map(String::as_str),
        Some("true"),
        "s4-framed flag must be set on framed objects"
    );
    assert!(
        has_sidecar,
        "sidecar object must be written for multi-frame body"
    );

    let resp = s4
        .get_object(get_request("bucket", "framed_object"))
        .await
        .expect("get");
    let got = read_back(resp).await;
    assert_eq!(
        got, payload,
        "framed roundtrip must reconstruct original bytes"
    );
}

/// v0.2 #4: small single-PUT object is still framed but no sidecar
/// (single frame = sidecar offers no benefit).
#[tokio::test]
async fn single_put_small_object_is_framed_without_sidecar() {
    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );

    let payload = Bytes::from(b"small payload, well under the chunk size".to_vec());
    s4.put_object(put_request("bucket", "tiny", payload.clone()))
        .await
        .expect("put");

    let has_sidecar = {
        let inner = backend_view.lock().unwrap();
        inner.contains_key(&("bucket".into(), "tiny.s4index".into()))
    };
    assert!(
        !has_sidecar,
        "no sidecar should be written for single-frame objects"
    );

    let resp = s4
        .get_object(get_request("bucket", "tiny"))
        .await
        .expect("get");
    assert_eq!(read_back(resp).await, payload);
}

/// v0.2 #4: Range GET on a framed object follows the sidecar partial-fetch
/// path. With per-chunk frames + sidecar, the GET handler should serve the
/// requested range correctly (byte-equal to the original slice).
#[tokio::test]
async fn single_put_framed_range_get_via_sidecar() {
    let backend = MemoryBackend::new();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );

    // Multi-chunk payload: 6 MiB → 2 frames at 4 MiB chunk size
    let payload: Bytes = (0u32..(6 * 1024 * 1024 / 4))
        .flat_map(|n| n.to_le_bytes())
        .collect::<Vec<u8>>()
        .into();
    s4.put_object(put_request("bucket", "rangeable", payload.clone()))
        .await
        .expect("put");

    // Range GET: bytes 1_500_000-1_500_999 (1000 bytes inside the first frame)
    let mut req = get_request("bucket", "rangeable");
    req.input.range = Some(s3s::dto::Range::Int {
        first: 1_500_000,
        last: Some(1_500_999),
    });
    let resp = s4.get_object(req).await.expect("range get");
    let got = read_back(resp).await;
    assert_eq!(got.len(), 1000, "should return exactly the requested range");
    assert_eq!(
        got,
        payload.slice(1_500_000..1_501_000),
        "ranged bytes must match original slice"
    );

    // Range GET that crosses a frame boundary
    let mut req = get_request("bucket", "rangeable");
    req.input.range = Some(s3s::dto::Range::Int {
        first: 4_000_000,
        last: Some(4_500_000),
    });
    let resp = s4.get_object(req).await.expect("cross-frame range get");
    let got = read_back(resp).await;
    assert_eq!(got.len(), 4_500_001 - 4_000_000);
    assert_eq!(got, payload.slice(4_000_000..4_500_001));
}

/// v0.2 #7: a Deny on s3:DeleteObject blocks delete from reaching the
/// backend, while other actions still pass.
#[tokio::test]
async fn policy_denies_delete_but_allows_get_and_put() {
    use s4_server::policy::Policy;

    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let policy = Policy::from_json_str(
        r#"{
            "Version": "2012-10-17",
            "Statement": [
              {"Sid": "AllowAll", "Effect": "Allow", "Action": "s3:*",
               "Resource": "arn:aws:s3:::bucket/*"},
              {"Sid": "DenyDelete", "Effect": "Deny", "Action": "s3:DeleteObject",
               "Resource": "arn:aws:s3:::bucket/*"}
            ]
        }"#,
    )
    .expect("policy parse");
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_policy(Arc::new(policy));

    // PUT allowed
    let payload = Bytes::from_static(b"data");
    s4.put_object(put_request("bucket", "k", payload.clone()))
        .await
        .expect("put should be allowed by policy");
    assert!(
        backend_view
            .lock()
            .unwrap()
            .contains_key(&("bucket".into(), "k".into()))
    );

    // GET allowed
    let resp = s4
        .get_object(get_request("bucket", "k"))
        .await
        .expect("get should be allowed");
    assert_eq!(read_back(resp).await, payload);

    // DELETE denied — should return AccessDenied without reaching backend
    let del_input = DeleteObjectInput {
        bucket: "bucket".into(),
        key: "k".into(),
        ..Default::default()
    };
    let del_req = S3Request {
        input: del_input,
        method: http::Method::DELETE,
        uri: "/bucket/k".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    let err = s4.delete_object(del_req).await.expect_err("should deny");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("AccessDenied"),
        "expected AccessDenied, got {dbg}"
    );
    // Object must still be in backend (delete didn't reach it)
    assert!(
        backend_view
            .lock()
            .unwrap()
            .contains_key(&("bucket".into(), "k".into()))
    );
}

/// v0.3 #13: a policy with an IpAddress Condition denies a request whose
/// X-Forwarded-For header puts it outside the trusted CIDR. Validates the
/// full hot path: header → request_context → policy.evaluate_with → deny.
#[tokio::test]
async fn policy_iam_condition_ip_address_denies_outside_cidr() {
    use s4_server::policy::Policy;

    let backend = MemoryBackend::new();
    let policy = Policy::from_json_str(
        r#"{
            "Version": "2012-10-17",
            "Statement": [
              {"Sid": "AllowFromCorpVpn",
               "Effect": "Allow",
               "Action": "s3:GetObject",
               "Resource": "arn:aws:s3:::bucket/*",
               "Condition": {"IpAddress": {"aws:SourceIp": ["10.0.0.0/8"]}}}
            ]
        }"#,
    )
    .expect("policy parse");
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_policy(Arc::new(policy));

    // Manually craft a GET with X-Forwarded-For populated. Use the
    // existing helper but override headers.
    let make_get = |xff: Option<&str>| {
        let mut req = get_request("bucket", "k");
        if let Some(ip) = xff {
            req.headers.insert("x-forwarded-for", ip.parse().unwrap());
        }
        req
    };

    // PUT first so the object exists (PUT itself is denied because no
    // s3:PutObject in the Allow list — implicit deny — so we go around
    // S4 and inject the object directly via the in-memory backend).
    let payload = Bytes::from_static(b"data");
    let raw_compressed = zstd::stream::encode_all(payload.as_ref(), 3).unwrap();
    let mut meta = Metadata::default();
    meta.insert("s4-codec".into(), "cpu-zstd".into());
    meta.insert("s4-original-size".into(), payload.len().to_string());
    meta.insert(
        "s4-compressed-size".into(),
        raw_compressed.len().to_string(),
    );
    meta.insert("s4-crc32c".into(), crc32c::crc32c(&payload).to_string());
    {
        let backend_ref = s4.into_backend();
        backend_ref.inner.lock().unwrap().insert(
            ("bucket".into(), "k".into()),
            StoredObject {
                body: Bytes::from(raw_compressed),
                metadata: Some(meta),
                content_type: None,
            },
        );

        // Re-wrap the backend in S4Service for the GET tests.
        let policy = Policy::from_json_str(
            r#"{"Statement": [{"Effect": "Allow", "Action": "s3:GetObject",
                  "Resource": "arn:aws:s3:::bucket/*",
                  "Condition": {"IpAddress": {"aws:SourceIp": ["10.0.0.0/8"]}}}]}"#,
        )
        .unwrap();
        let s4 = S4Service::new(
            backend_ref,
            make_registry(CodecKind::CpuZstd),
            make_dispatcher(CodecKind::CpuZstd),
        )
        .with_policy(Arc::new(policy));

        // Inside the trusted CIDR → allow.
        let resp = s4
            .get_object(make_get(Some("10.5.6.7")))
            .await
            .expect("ip in cidr should allow");
        assert_eq!(read_back(resp).await, payload);

        // Outside the trusted CIDR → AccessDenied.
        let err = s4
            .get_object(make_get(Some("203.0.113.1")))
            .await
            .expect_err("ip outside cidr should deny");
        let dbg = format!("{err:?}");
        assert!(dbg.contains("AccessDenied"), "got {dbg}");

        // No X-Forwarded-For at all → context has no source_ip → IpAddress
        // condition fails → statement skipped → implicit deny.
        let err = s4
            .get_object(make_get(None))
            .await
            .expect_err("missing source ip should deny via implicit deny");
        let dbg = format!("{err:?}");
        assert!(dbg.contains("AccessDenied"), "got {dbg}");
    }
}

/// v0.2 #7: a policy that allows nothing → implicit deny on every request.
#[tokio::test]
async fn policy_with_no_matching_statement_implicit_denies() {
    use s4_server::policy::Policy;

    let backend = MemoryBackend::new();
    // Policy allows ops on a different bucket → no statement matches "bucket"
    let policy = Policy::from_json_str(
        r#"{"Version": "2012-10-17", "Statement": [
            {"Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::other/*"}
        ]}"#,
    )
    .unwrap();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_policy(Arc::new(policy));

    let payload = Bytes::from_static(b"data");
    let err = s4
        .put_object(put_request("bucket", "k", payload))
        .await
        .expect_err("implicit deny");
    let dbg = format!("{err:?}");
    assert!(dbg.contains("AccessDenied"), "got {dbg}");
}

/// v0.2 #4 back-compat: a v0.1 object stored as raw compressed bytes (no
/// `s4-framed` flag) must still GET correctly through v0.2.
#[tokio::test]
async fn legacy_v01_raw_blob_still_decompresses_on_get() {
    let backend = MemoryBackend::new();
    // Manually inject a v0.1-style stored object: raw zstd-compressed body +
    // legacy manifest metadata, no `s4-framed` flag.
    let original = Bytes::from(b"legacy v0.1 payload that v0.2 must still read".to_vec());
    let raw_compressed = zstd::stream::encode_all(original.as_ref(), 3).unwrap();
    let mut meta = Metadata::default();
    meta.insert("s4-codec".into(), "cpu-zstd".into());
    meta.insert("s4-original-size".into(), original.len().to_string());
    meta.insert(
        "s4-compressed-size".into(),
        raw_compressed.len().to_string(),
    );
    meta.insert("s4-crc32c".into(), crc32c::crc32c(&original).to_string());
    backend.inner.lock().unwrap().insert(
        ("bucket".into(), "legacy_obj".into()),
        StoredObject {
            body: Bytes::from(raw_compressed),
            metadata: Some(meta),
            content_type: None,
        },
    );

    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );
    let resp = s4
        .get_object(get_request("bucket", "legacy_obj"))
        .await
        .expect("get");
    assert_eq!(
        read_back(resp).await,
        original,
        "legacy v0.1 raw blob must decompress correctly under v0.2"
    );
}

#[tokio::test]
async fn registry_dispatches_decompress_across_codecs() {
    // 同じ bucket に passthrough と cpu-zstd 両方で書いた object を、
    // **同じ S4 インスタンス** から GET して両方読めることを確認 (multi-codec dispatch)。
    let backend = MemoryBackend::new();

    // 1) passthrough で 1 個書く
    let s4_pt = S4Service::new(
        backend,
        make_registry(CodecKind::Passthrough),
        make_dispatcher(CodecKind::Passthrough),
    );
    let raw_payload = Bytes::from_static(b"unsquished bytes");
    s4_pt
        .put_object(put_request("bucket", "k_passthrough", raw_payload.clone()))
        .await
        .expect("put pt");

    // backend を取り出し、新しい S4 (cpu-zstd default + 全 codec 登録) で再 wrap
    let backend = s4_pt.into_backend();
    let s4_zstd = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );

    // 2) cpu-zstd で 1 個書く (こちらは新インスタンス経由)
    let zpayload = Bytes::from(vec![b'q'; 50_000]);
    s4_zstd
        .put_object(put_request("bucket", "k_zstd", zpayload.clone()))
        .await
        .expect("put zstd");

    // 3) 同じ s4_zstd で両方読めることを確認 (registry が manifest.codec で dispatch)
    let resp_pt = s4_zstd
        .get_object(get_request("bucket", "k_passthrough"))
        .await
        .expect("get pt");
    assert_eq!(read_back(resp_pt).await, raw_payload);
    let resp_zstd = s4_zstd
        .get_object(get_request("bucket", "k_zstd"))
        .await
        .expect("get zstd");
    assert_eq!(read_back(resp_zstd).await, zpayload);
}
