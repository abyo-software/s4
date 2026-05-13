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

    /// v0.5 #34: DELETE the backend bytes (idempotent — missing key is OK,
    /// matching real S3 behaviour). The S4Service-level versioning logic
    /// owns the chain semantics; this just clears the underlying byte.
    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        self.inner.lock().unwrap().remove(&key);
        Ok(S3Response::new(DeleteObjectOutput::default()))
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

/// v0.4 #21: SSE-S4 (AES-256-GCM) compress→encrypt→PUT round-trip
/// matches GET→decrypt→decompress. Backend bytes start with the S4E1
/// magic; flipping any byte after the header makes GET fail with a
/// hard auth-tag error (ciphertext tampering or wrong key).
#[tokio::test]
async fn sse_s4_roundtrip_and_tamper_detection() {
    use s4_server::sse::SseKey;

    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let key = Arc::new(SseKey::from_bytes(&[42u8; 32]).unwrap());
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_sse_key(Arc::clone(&key));

    let payload =
        Bytes::from(b"top secret payload that needs to be encrypted on the wire".repeat(50));
    s4.put_object(put_request("bucket", "encrypted", payload.clone()))
        .await
        .expect("put");

    // Snapshot the on-disk body and metadata before any GET so we can
    // (a) confirm encryption shape and (b) tamper for the second test.
    let (stored_body, stored_meta) = {
        let inner = backend_view.lock().unwrap();
        let s = inner
            .get(&("bucket".into(), "encrypted".into()))
            .unwrap()
            .clone();
        (s.body, s.metadata)
    };
    // v0.5 #29: `with_sse_key` now wraps the supplied key in a 1-slot
    // keyring (id=1 active) and writes the S4E2 frame so single-key
    // operators automatically get the rotation-ready format. The
    // gateway's GET path still decrypts S4E1 bodies via the keyring's
    // legacy fallback (covered by `legacy_s4e1_object_decrypts_under_v05_keyring`).
    assert_eq!(
        &stored_body[..4],
        b"S4E2",
        "encrypted body must start with S4E2 magic"
    );
    assert_eq!(
        u16::from_be_bytes([stored_body[5], stored_body[6]]),
        1,
        "with_sse_key uses id=1 as the active slot"
    );
    let meta = stored_meta.as_ref().expect("must have metadata");
    assert_eq!(
        meta.get("s4-encrypted").map(String::as_str),
        Some("aes-256-gcm")
    );

    // Round-trip with the same gateway → original bytes.
    let resp = s4
        .get_object(get_request("bucket", "encrypted"))
        .await
        .expect("get");
    assert_eq!(read_back(resp).await, payload);

    // Tamper: flip a byte in the ciphertext, then GET fails with
    // InternalError (we surface the AES-GCM auth-tag failure as such).
    {
        let mut inner = backend_view.lock().unwrap();
        let stored = inner
            .get_mut(&("bucket".into(), "encrypted".into()))
            .unwrap();
        let mut bytes = stored.body.to_vec();
        let idx = bytes.len() - 1;
        bytes[idx] ^= 0xff;
        stored.body = Bytes::from(bytes);
    }
    let err = s4
        .get_object(get_request("bucket", "encrypted"))
        .await
        .expect_err("tampered ciphertext must fail");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("SSE-S4 decrypt failed"),
        "expected SSE-S4 decrypt error, got {dbg}"
    );
}

/// v0.4 #21: an object marked `s4-encrypted` but the gateway has no key
/// configured returns InvalidRequest (= operator misconfig).
#[tokio::test]
async fn sse_s4_get_without_key_errors() {
    use s4_server::sse::SseKey;

    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let key = Arc::new(SseKey::from_bytes(&[42u8; 32]).unwrap());
    // PUT with a key
    let s4_with_key = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_sse_key(Arc::clone(&key));
    s4_with_key
        .put_object(put_request("bucket", "k", Bytes::from_static(b"data")))
        .await
        .expect("put");
    let backend = s4_with_key.into_backend();

    // GET via a fresh service that has NO key configured
    let s4_no_key = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );
    let err = s4_no_key
        .get_object(get_request("bucket", "k"))
        .await
        .expect_err("must require key");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("no --sse-s4-key"),
        "expected key-missing error, got {dbg}"
    );
    let _ = backend_view;
}

/// v0.5 #29: end-to-end key rotation. A 2-key keyring (id=2 active,
/// id=1 retired) must (a) write S4E2 frames stamped with id=2,
/// (b) round-trip those bodies, and (c) after a second rotation
/// (id=3 active, ring also has 1 and 2), still decrypt the older
/// objects under their original ids.
#[tokio::test]
async fn sse_s4_keyring_rotation_e2e() {
    use s4_server::sse::{SseKey, SseKeyring};

    let backend = MemoryBackend::new();
    let backend_view = backend.shared();

    let k1 = Arc::new(SseKey::from_bytes(&[1u8; 32]).unwrap());
    let k2 = Arc::new(SseKey::from_bytes(&[2u8; 32]).unwrap());
    let k3 = Arc::new(SseKey::from_bytes(&[3u8; 32]).unwrap());

    // ----- Wave 1: keyring active=2, retired=1.
    let mut ring_v1 = SseKeyring::new(2, Arc::clone(&k2));
    ring_v1.add(1, Arc::clone(&k1));
    let s4_v1 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_sse_keyring(Arc::new(ring_v1));

    let payload_a =
        Bytes::from(b"object encrypted under active=2 (rotation wave 1)".repeat(20));
    s4_v1
        .put_object(put_request("bucket", "obj_a", payload_a.clone()))
        .await
        .expect("put obj_a");

    // Backend snapshot: must be S4E2 with key_id=2 (BE).
    {
        let inner = backend_view.lock().unwrap();
        let stored = inner
            .get(&("bucket".into(), "obj_a".into()))
            .expect("obj_a stored");
        assert_eq!(&stored.body[..4], b"S4E2", "active=2 must write S4E2");
        let id = u16::from_be_bytes([stored.body[5], stored.body[6]]);
        assert_eq!(id, 2, "object stamped with active key_id");
        let meta = stored
            .metadata
            .as_ref()
            .expect("metadata must include s4-encrypted");
        assert_eq!(
            meta.get("s4-encrypted").map(String::as_str),
            Some("aes-256-gcm"),
            "metadata flag stays the same regardless of S4E1 vs S4E2"
        );
    }

    // GET round-trip ok with the same keyring.
    let resp_a = s4_v1
        .get_object(get_request("bucket", "obj_a"))
        .await
        .expect("get obj_a v1");
    assert_eq!(read_back(resp_a).await, payload_a);

    // ----- Wave 2: rotate to active=3, retire 1 and 2.
    let backend = s4_v1.into_backend();
    let mut ring_v2 = SseKeyring::new(3, Arc::clone(&k3));
    ring_v2.add(1, Arc::clone(&k1));
    ring_v2.add(2, Arc::clone(&k2));
    let s4_v2 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_sse_keyring(Arc::new(ring_v2));

    // Old obj_a (key_id=2) must still decrypt.
    let resp_a = s4_v2
        .get_object(get_request("bucket", "obj_a"))
        .await
        .expect("get obj_a v2 (rotated)");
    assert_eq!(
        read_back(resp_a).await,
        payload_a,
        "old key_id=2 object must still decrypt under v2 keyring"
    );

    // New PUT must be encrypted under active=3.
    let payload_c = Bytes::from(b"object encrypted under active=3 (rotation wave 2)".repeat(20));
    s4_v2
        .put_object(put_request("bucket", "obj_c", payload_c.clone()))
        .await
        .expect("put obj_c");
    {
        let inner = backend_view.lock().unwrap();
        let stored = inner
            .get(&("bucket".into(), "obj_c".into()))
            .expect("obj_c stored");
        assert_eq!(&stored.body[..4], b"S4E2");
        let id = u16::from_be_bytes([stored.body[5], stored.body[6]]);
        assert_eq!(id, 3, "rotated active=3 must stamp new objects with 3");
    }
    let resp_c = s4_v2
        .get_object(get_request("bucket", "obj_c"))
        .await
        .expect("get obj_c v2");
    assert_eq!(read_back(resp_c).await, payload_c);

    // Mint a third object under wave 2 but inject id=1 by writing it
    // through a single-key keyring (active=1) so we can confirm the
    // wave-2 ring decrypts arbitrary retired-id S4E2 bodies.
    let backend = s4_v2.into_backend();
    let ring_id1_only = SseKeyring::new(1, Arc::clone(&k1));
    let s4_id1 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_sse_keyring(Arc::new(ring_id1_only));
    let payload_b = Bytes::from(b"object encrypted under id=1 (legacy retired)".repeat(20));
    s4_id1
        .put_object(put_request("bucket", "obj_b", payload_b.clone()))
        .await
        .expect("put obj_b under id=1");
    {
        let inner = backend_view.lock().unwrap();
        let stored = inner
            .get(&("bucket".into(), "obj_b".into()))
            .expect("obj_b stored");
        let id = u16::from_be_bytes([stored.body[5], stored.body[6]]);
        assert_eq!(id, 1, "obj_b must be stamped with id=1");
    }
    let backend = s4_id1.into_backend();
    // Now route GET through the wave-2 keyring (active=3, also has 1,2).
    let mut ring_v2_again = SseKeyring::new(3, Arc::clone(&k3));
    ring_v2_again.add(1, Arc::clone(&k1));
    ring_v2_again.add(2, Arc::clone(&k2));
    let s4_v2b = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_sse_keyring(Arc::new(ring_v2_again));
    let resp_b = s4_v2b
        .get_object(get_request("bucket", "obj_b"))
        .await
        .expect("get obj_b under wave-2 ring");
    assert_eq!(
        read_back(resp_b).await,
        payload_b,
        "id=1 object must decrypt via wave-2 keyring"
    );
}

/// v0.5 #29 back-compat: an S4E1-framed object (written by v0.4) must
/// decrypt unchanged through a v0.5 gateway whose keyring has the
/// original key as the active id=1 slot. Exercises the keyring's S4E1
/// fallback branch via the GET path end-to-end.
#[tokio::test]
async fn legacy_s4e1_object_decrypts_under_v05_keyring() {
    use s4_server::sse::{SseKey, SseKeyring};

    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let key = Arc::new(SseKey::from_bytes(&[42u8; 32]).unwrap());

    // Hand-craft an S4E1 body (the v0.4 wire format) by going through
    // the low-level `sse::encrypt` and writing it directly into the
    // backend with the s4-encrypted metadata flag plus a real codec
    // manifest so the GET path actually exercises the SSE branch.
    let plaintext = Bytes::from(b"v0.4 vintage encrypted object".repeat(20));
    let raw_compressed = zstd::stream::encode_all(plaintext.as_ref(), 3).unwrap();
    let s4e1_body = s4_server::sse::encrypt(&key, &raw_compressed);
    assert_eq!(&s4e1_body[..4], b"S4E1", "preflight: must be the v1 frame");

    let mut meta = Metadata::default();
    meta.insert("s4-codec".into(), "cpu-zstd".into());
    meta.insert("s4-original-size".into(), plaintext.len().to_string());
    meta.insert(
        "s4-compressed-size".into(),
        raw_compressed.len().to_string(),
    );
    meta.insert(
        "s4-crc32c".into(),
        crc32c::crc32c(&plaintext).to_string(),
    );
    meta.insert("s4-encrypted".into(), "aes-256-gcm".into());
    backend_view.lock().unwrap().insert(
        ("bucket".into(), "legacy_enc".into()),
        StoredObject {
            body: s4e1_body,
            metadata: Some(meta),
            content_type: None,
        },
    );

    // v0.5 gateway with a 1-slot keyring (id=1 active = the original key).
    let ring = SseKeyring::new(1, Arc::clone(&key));
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_sse_keyring(Arc::new(ring));
    let resp = s4
        .get_object(get_request("bucket", "legacy_enc"))
        .await
        .expect("get legacy S4E1 via v0.5 keyring");
    assert_eq!(read_back(resp).await, plaintext);
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

// =========================================================================
// v0.5 #34: Versioning state machine integration tests.
//
// All five tests share the same shape:
//   1. Build an S4Service with `with_versioning(...)` attached.
//   2. Drive PUT/GET/DELETE/list_object_versions through it.
//   3. Inspect S3-level outputs (response.version_id, list_object_versions
//      Versions / DeleteMarkers arrays, etc) for the expected wire-shape.
//
// We use cpu-zstd as the codec so the buckets exercise the full
// compress + framed PUT path, but the assertions are about versioning
// semantics only (compression correctness is covered by the upstream
// roundtrip tests).
// =========================================================================

fn make_versioned_s4(
    codec: CodecKind,
) -> (
    S4Service<MemoryBackend>,
    Arc<s4_server::versioning::VersioningManager>,
) {
    let backend = MemoryBackend::new();
    let mgr = Arc::new(s4_server::versioning::VersioningManager::new());
    let s4 = S4Service::new(backend, make_registry(codec), make_dispatcher(codec))
        .with_versioning(Arc::clone(&mgr));
    (s4, mgr)
}

async fn enable_versioning_on(s4: &S4Service<MemoryBackend>, bucket: &str) {
    let inp = PutBucketVersioningInput::builder()
        .bucket(bucket.to_owned())
        .versioning_configuration(VersioningConfiguration {
            status: Some(BucketVersioningStatus::from_static(
                BucketVersioningStatus::ENABLED,
            )),
            ..Default::default()
        })
        .build()
        .expect("build PutBucketVersioningInput");
    let req = S3Request {
        input: inp,
        method: http::Method::PUT,
        uri: format!("/{bucket}?versioning").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    s4.put_bucket_versioning(req)
        .await
        .expect("enable versioning");
}

fn get_request_with_version(bucket: &str, key: &str, version_id: &str) -> S3Request<GetObjectInput> {
    let mut r = get_request(bucket, key);
    r.input.version_id = Some(version_id.to_owned());
    r
}

fn delete_request(bucket: &str, key: &str) -> S3Request<DeleteObjectInput> {
    let input = DeleteObjectInput {
        bucket: bucket.into(),
        key: key.into(),
        ..Default::default()
    };
    S3Request {
        input,
        method: http::Method::DELETE,
        uri: format!("/{bucket}/{key}").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn delete_request_with_version(
    bucket: &str,
    key: &str,
    version_id: &str,
) -> S3Request<DeleteObjectInput> {
    let mut r = delete_request(bucket, key);
    r.input.version_id = Some(version_id.to_owned());
    r
}

fn list_versions_request(bucket: &str) -> S3Request<ListObjectVersionsInput> {
    let input = ListObjectVersionsInput {
        bucket: bucket.into(),
        ..Default::default()
    };
    S3Request {
        input,
        method: http::Method::GET,
        uri: format!("/{bucket}?versions").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

#[tokio::test]
async fn versioning_enable_then_put_creates_new_version() {
    let (s4, mgr) = make_versioned_s4(CodecKind::CpuZstd);
    enable_versioning_on(&s4, "bucket").await;
    assert_eq!(
        mgr.state("bucket"),
        s4_server::versioning::VersioningState::Enabled
    );

    let payload1 = Bytes::from_static(b"hello version 1");
    let resp1 = s4
        .put_object(put_request("bucket", "key", payload1.clone()))
        .await
        .expect("put v1");
    let vid1 = resp1
        .output
        .version_id
        .as_ref()
        .expect("Enabled bucket PUT must surface x-amz-version-id")
        .clone();
    assert_ne!(vid1, "null");
    assert_eq!(vid1.len(), 32, "UUIDv4 simple form is 32 hex chars");

    let payload2 = Bytes::from_static(b"hello version 2 - longer");
    let resp2 = s4
        .put_object(put_request("bucket", "key", payload2.clone()))
        .await
        .expect("put v2");
    let vid2 = resp2.output.version_id.as_ref().expect("vid2").clone();
    assert_ne!(vid1, vid2, "each PUT must mint a fresh version-id");

    // Latest GET (no versionId) returns v2.
    let got_latest = s4
        .get_object(get_request("bucket", "key"))
        .await
        .expect("get latest");
    assert_eq!(got_latest.output.version_id.as_deref(), Some(vid2.as_str()));
    assert_eq!(read_back(got_latest).await, payload2);

    // The chain has two entries.
    let page = mgr.list_versions("bucket", None, None, None, 100);
    assert_eq!(page.versions.len(), 2);
}

#[tokio::test]
async fn versioning_get_with_version_id_returns_specific_version() {
    let (s4, _mgr) = make_versioned_s4(CodecKind::CpuZstd);
    enable_versioning_on(&s4, "bucket").await;

    let v1_payload = Bytes::from_static(b"v1 body");
    let resp1 = s4
        .put_object(put_request("bucket", "obj", v1_payload.clone()))
        .await
        .expect("put v1");
    let vid1 = resp1.output.version_id.expect("vid1");

    let v2_payload = Bytes::from_static(b"v2 body - different");
    let resp2 = s4
        .put_object(put_request("bucket", "obj", v2_payload.clone()))
        .await
        .expect("put v2");
    let vid2 = resp2.output.version_id.expect("vid2");

    // GET with the older vid returns v1's bytes (the new shadow-key
    // routing is what makes this work — without it, both PUTs would
    // overwrite the same backend key and v1 bytes would be lost).
    let got_v1 = s4
        .get_object(get_request_with_version("bucket", "obj", &vid1))
        .await
        .expect("get v1");
    assert_eq!(got_v1.output.version_id.as_deref(), Some(vid1.as_str()));
    assert_eq!(read_back(got_v1).await, v1_payload);

    // GET with the newer vid returns v2.
    let got_v2 = s4
        .get_object(get_request_with_version("bucket", "obj", &vid2))
        .await
        .expect("get v2");
    assert_eq!(read_back(got_v2).await, v2_payload);

    // GET with an unknown vid returns NoSuchVersion.
    let err = s4
        .get_object(get_request_with_version(
            "bucket",
            "obj",
            "deadbeefdeadbeefdeadbeefdeadbeef",
        ))
        .await
        .expect_err("unknown version must 404");
    assert!(
        format!("{err:?}").contains("NoSuchVersion"),
        "expected NoSuchVersion, got {err:?}"
    );
}

#[tokio::test]
async fn versioning_delete_without_version_id_creates_delete_marker_and_get_returns_404() {
    let (s4, mgr) = make_versioned_s4(CodecKind::CpuZstd);
    enable_versioning_on(&s4, "bucket").await;

    let payload = Bytes::from_static(b"about to be tombstoned");
    let put_resp = s4
        .put_object(put_request("bucket", "doomed", payload.clone()))
        .await
        .expect("put");
    let real_vid = put_resp.output.version_id.expect("real vid");

    // DELETE without version-id → push a delete marker.
    let del_resp = s4
        .delete_object(delete_request("bucket", "doomed"))
        .await
        .expect("delete creates marker");
    assert_eq!(del_resp.output.delete_marker, Some(true));
    let marker_vid = del_resp.output.version_id.expect("marker vid");
    assert_ne!(marker_vid, real_vid);

    // GET without version-id now returns NoSuchKey (delete marker is
    // the latest entry in the chain).
    let err = s4
        .get_object(get_request("bucket", "doomed"))
        .await
        .expect_err("delete-marker latest must 404");
    assert!(
        format!("{err:?}").contains("NoSuchKey"),
        "expected NoSuchKey, got {err:?}"
    );

    // The real version is still reachable by explicit version-id —
    // the delete marker is a tombstone, not a destructive delete.
    let undeleted = s4
        .get_object(get_request_with_version("bucket", "doomed", &real_vid))
        .await
        .expect("real version still reachable by vid");
    assert_eq!(read_back(undeleted).await, payload);

    // Index sanity: chain has 1 real + 1 marker.
    let page = mgr.list_versions("bucket", None, None, None, 100);
    assert_eq!(page.versions.len(), 1);
    assert_eq!(page.delete_markers.len(), 1);
    assert!(page.delete_markers[0].is_latest);
}

#[tokio::test]
async fn versioning_delete_with_version_id_removes_specific_version_and_other_versions_remain() {
    let (s4, mgr) = make_versioned_s4(CodecKind::CpuZstd);
    enable_versioning_on(&s4, "bucket").await;

    let p1 = Bytes::from_static(b"version one");
    let r1 = s4
        .put_object(put_request("bucket", "k", p1.clone()))
        .await
        .expect("put 1");
    let v1 = r1.output.version_id.expect("v1");

    let p2 = Bytes::from_static(b"version two - bigger payload");
    let r2 = s4
        .put_object(put_request("bucket", "k", p2.clone()))
        .await
        .expect("put 2");
    let v2 = r2.output.version_id.expect("v2");

    let p3 = Bytes::from_static(b"version three - biggest payload of them all");
    let r3 = s4
        .put_object(put_request("bucket", "k", p3.clone()))
        .await
        .expect("put 3");
    let v3 = r3.output.version_id.expect("v3");

    // Delete the *middle* version specifically — other versions stay.
    let del = s4
        .delete_object(delete_request_with_version("bucket", "k", &v2))
        .await
        .expect("specific-version delete");
    assert_eq!(del.output.version_id.as_deref(), Some(v2.as_str()));
    assert_ne!(del.output.delete_marker, Some(true));

    // v1 + v3 are still reachable by explicit vid; v2 is gone.
    let got_v1 = s4
        .get_object(get_request_with_version("bucket", "k", &v1))
        .await
        .expect("v1 must remain");
    assert_eq!(read_back(got_v1).await, p1);

    let got_v3 = s4
        .get_object(get_request_with_version("bucket", "k", &v3))
        .await
        .expect("v3 must remain");
    assert_eq!(read_back(got_v3).await, p3);

    let err = s4
        .get_object(get_request_with_version("bucket", "k", &v2))
        .await
        .expect_err("v2 must be gone");
    assert!(
        format!("{err:?}").contains("NoSuchVersion"),
        "expected NoSuchVersion, got {err:?}"
    );

    // Latest (= v3) is still v3, GET without vid returns p3.
    let latest = s4
        .get_object(get_request("bucket", "k"))
        .await
        .expect("latest");
    assert_eq!(latest.output.version_id.as_deref(), Some(v3.as_str()));
    assert_eq!(read_back(latest).await, p3);

    // Chain has 2 entries left.
    let page = mgr.list_versions("bucket", None, None, None, 100);
    assert_eq!(page.versions.len(), 2);
    let vids: Vec<&str> = page.versions.iter().map(|e| e.version_id.as_str()).collect();
    assert!(vids.contains(&v1.as_str()));
    assert!(vids.contains(&v3.as_str()));
    assert!(!vids.contains(&v2.as_str()));
}

#[tokio::test]
async fn versioning_list_versions_returns_chronological_history_with_is_latest_flag() {
    let (s4, _mgr) = make_versioned_s4(CodecKind::CpuZstd);
    enable_versioning_on(&s4, "bucket").await;

    // Three versions of the same key + one of a different key + a
    // delete marker on the second key.
    let _ = s4
        .put_object(put_request("bucket", "alpha", Bytes::from_static(b"a1")))
        .await
        .expect("a1");
    let _ = s4
        .put_object(put_request("bucket", "alpha", Bytes::from_static(b"a2")))
        .await
        .expect("a2");
    let r3 = s4
        .put_object(put_request("bucket", "alpha", Bytes::from_static(b"a3")))
        .await
        .expect("a3");
    let v3_alpha = r3.output.version_id.expect("a3 vid");

    let _ = s4
        .put_object(put_request("bucket", "beta", Bytes::from_static(b"b1")))
        .await
        .expect("b1");
    let _ = s4
        .delete_object(delete_request("bucket", "beta"))
        .await
        .expect("delete beta");

    // Drive list_object_versions through the S4Service handler (full
    // wire-shape, as a real S3 client would see it).
    let resp = s4
        .list_object_versions(list_versions_request("bucket"))
        .await
        .expect("list versions");

    // 3 alpha versions + 1 real beta version.
    let versions = resp.output.versions.expect("Versions array");
    assert_eq!(versions.len(), 4);
    // Ordering: key asc → "alpha" first, then "beta". Inside "alpha"
    // newest first.
    let alpha_versions: Vec<&ObjectVersion> = versions
        .iter()
        .filter(|v| v.key.as_deref() == Some("alpha"))
        .collect();
    assert_eq!(alpha_versions.len(), 3);
    assert_eq!(
        alpha_versions[0].version_id.as_deref(),
        Some(v3_alpha.as_str()),
        "newest alpha must come first"
    );
    assert_eq!(alpha_versions[0].is_latest, Some(true));
    assert_eq!(alpha_versions[1].is_latest, Some(false));
    assert_eq!(alpha_versions[2].is_latest, Some(false));

    // 1 delete marker for beta — and it must be is_latest=true (the
    // delete marker is the current state of beta).
    let markers = resp.output.delete_markers.expect("DeleteMarkers array");
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].key.as_deref(), Some("beta"));
    assert_eq!(markers[0].is_latest, Some(true));

    // beta's prior real version is still in the Versions array but
    // is_latest=false (since the delete marker is now latest).
    let beta_real: Vec<&ObjectVersion> = versions
        .iter()
        .filter(|v| v.key.as_deref() == Some("beta"))
        .collect();
    assert_eq!(beta_real.len(), 1);
    assert_eq!(beta_real[0].is_latest, Some(false));

    // Bucket name + paging metadata round-trip.
    assert_eq!(resp.output.name.as_deref(), Some("bucket"));
    assert_eq!(resp.output.is_truncated, Some(false));
}


// ------------------------------------------------------------------
// v0.5 #27: SSE-C (customer-provided keys) end-to-end.
// ------------------------------------------------------------------

fn put_request_sse_c(
    bucket: &str,
    key: &str,
    body: Bytes,
    cust_key: &[u8; 32],
) -> S3Request<PutObjectInput> {
    use base64::Engine as _;
    let mut req = put_request(bucket, key, body);
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(cust_key);
    let md5 = s4_server::sse::compute_key_md5(cust_key);
    let md5_b64 = base64::engine::general_purpose::STANDARD.encode(md5);
    req.input.sse_customer_algorithm = Some(s4_server::sse::SSE_C_ALGORITHM.into());
    req.input.sse_customer_key = Some(key_b64);
    req.input.sse_customer_key_md5 = Some(md5_b64);
    req
}

fn get_request_sse_c(bucket: &str, key: &str, cust_key: &[u8; 32]) -> S3Request<GetObjectInput> {
    use base64::Engine as _;
    let mut req = get_request(bucket, key);
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(cust_key);
    let md5 = s4_server::sse::compute_key_md5(cust_key);
    let md5_b64 = base64::engine::general_purpose::STANDARD.encode(md5);
    req.input.sse_customer_algorithm = Some(s4_server::sse::SSE_C_ALGORITHM.into());
    req.input.sse_customer_key = Some(key_b64);
    req.input.sse_customer_key_md5 = Some(md5_b64);
    req
}

#[tokio::test]
async fn sse_c_roundtrip_and_wrong_key_fails() {
    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );
    let cust_key = [0xa5u8; 32];
    let payload = Bytes::from(b"customer-provided-key payload that the server never sees a copy of".repeat(40));

    let put_resp = s4
        .put_object(put_request_sse_c("bucket", "scc", payload.clone(), &cust_key))
        .await
        .expect("put");
    // Echo: algorithm + key MD5 should be on the response (algorithm
    // alone is enough to prove SSE-C was honored).
    assert_eq!(
        put_resp.output.sse_customer_algorithm.as_deref(),
        Some(s4_server::sse::SSE_C_ALGORITHM)
    );
    assert!(put_resp.output.sse_customer_key_md5.is_some());

    // Backend storage starts with the S4E3 magic — the key is not
    // persisted, only the per-object MD5 fingerprint via the AAD.
    {
        let inner = backend_view.lock().unwrap();
        let stored = inner.get(&("bucket".into(), "scc".into())).unwrap();
        assert_eq!(&stored.body[..4], b"S4E3", "SSE-C body must start with S4E3 magic");
    }

    // Round-trip with the same key → original bytes.
    let resp = s4
        .get_object(get_request_sse_c("bucket", "scc", &cust_key))
        .await
        .expect("get");
    assert_eq!(read_back(resp).await, payload);

    // Wrong key → AccessDenied (matches AWS).
    let wrong_key = [0xb6u8; 32];
    let err = s4
        .get_object(get_request_sse_c("bucket", "scc", &wrong_key))
        .await
        .expect_err("wrong key must fail");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("AccessDenied"),
        "expected AccessDenied for wrong SSE-C key, got {dbg}"
    );

    // GET without SSE-C headers on an SSE-C object → InvalidRequest.
    let err = s4
        .get_object(get_request("bucket", "scc"))
        .await
        .expect_err("no key must fail");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("InvalidRequest") || dbg.contains("supply x-amz-server-side-encryption-customer"),
        "expected SSE-C-required error, got {dbg}"
    );
}

// ------------------------------------------------------------------
// v0.5 #28: SSE-KMS (envelope-encrypted DEK) end-to-end with LocalKms.
// ------------------------------------------------------------------

#[tokio::test]
async fn sse_kms_roundtrip_with_local_kms() {
    use std::collections::HashMap;
    use std::sync::Arc;

    let kek = [0x33u8; 32];
    let mut keks = HashMap::new();
    keks.insert("alpha".to_string(), kek);
    let kms = Arc::new(s4_server::kms::LocalKms::from_keks(
        std::env::temp_dir(),
        keks,
    )) as Arc<dyn s4_server::kms::KmsBackend>;

    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_kms_backend(kms, Some("alpha".into()));

    let payload = Bytes::from(b"kms envelope-encrypted payload".repeat(80));
    let mut put_req = put_request("bucket", "kms-obj", payload.clone());
    put_req.input.server_side_encryption =
        Some(ServerSideEncryption::from_static(ServerSideEncryption::AWS_KMS));
    put_req.input.ssekms_key_id = Some("alpha".into());
    let put_resp = s4.put_object(put_req).await.expect("put");
    // Echo: SSE + KMS key id in response.
    assert_eq!(
        put_resp.output.server_side_encryption.as_ref().map(|s| s.as_str().to_string()),
        Some(ServerSideEncryption::AWS_KMS.to_string())
    );
    assert_eq!(put_resp.output.ssekms_key_id.as_deref(), Some("alpha"));

    // Storage starts with S4E4 magic — DEK is wrapped, KEK never on disk.
    {
        let inner = backend_view.lock().unwrap();
        let stored = inner.get(&("bucket".into(), "kms-obj".into())).unwrap();
        assert_eq!(&stored.body[..4], b"S4E4", "SSE-KMS body must start with S4E4 magic");
    }

    // Round-trip via the same gateway.
    let resp = s4
        .get_object(get_request("bucket", "kms-obj"))
        .await
        .expect("get");
    assert_eq!(read_back(resp).await, payload);

    // GET via a fresh gateway with NO KMS configured → InvalidRequest.
    // Build the no-KMS gateway over a backend that shares the same
    // storage map as the original so the SSE-KMS object is visible.
    let backend2 = MemoryBackend {
        inner: Arc::clone(&backend_view),
    };
    let s4_no_kms = S4Service::new(
        backend2,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );
    let err = s4_no_kms
        .get_object(get_request("bucket", "kms-obj"))
        .await
        .expect_err("no kms must fail");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("InvalidRequest") || dbg.contains("KMS"),
        "expected KMS-required error, got {dbg}"
    );
}

// =========================================================================
// v0.5 #30: Object Lock (WORM) enforcement integration tests.
//
// All four E2E tests build an `S4Service` with `with_object_lock(...)`
// attached, drive PUT / DELETE / put_object_retention /
// put_object_legal_hold / put_object_lock_configuration through the
// public S3 trait, and assert that the lock manager refuses the
// operation with HTTP 403 `AccessDenied` while a lock is in effect (or
// permits it once the lock has expired or been explicitly bypassed).
// =========================================================================

fn make_object_locked_s4(
    codec: CodecKind,
) -> (
    S4Service<MemoryBackend>,
    Arc<s4_server::object_lock::ObjectLockManager>,
) {
    let backend = MemoryBackend::new();
    let mgr = Arc::new(s4_server::object_lock::ObjectLockManager::new());
    let s4 = S4Service::new(backend, make_registry(codec), make_dispatcher(codec))
        .with_object_lock(Arc::clone(&mgr));
    (s4, mgr)
}

fn delete_request_with_bypass(
    bucket: &str,
    key: &str,
    bypass: bool,
) -> S3Request<DeleteObjectInput> {
    let input = DeleteObjectInput {
        bucket: bucket.into(),
        key: key.into(),
        bypass_governance_retention: Some(bypass),
        ..Default::default()
    };
    S3Request {
        input,
        method: http::Method::DELETE,
        uri: format!("/{bucket}/{key}").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

#[tokio::test]
async fn object_lock_compliance_mode_blocks_delete_until_expiry() {
    use chrono::{Duration as ChronoDuration, Utc};
    use s4_server::object_lock::{LockMode, ObjectLockState};

    let (s4, mgr) = make_object_locked_s4(CodecKind::CpuZstd);
    let payload = Bytes::from_static(b"locked-compliance");
    s4.put_object(put_request("bucket", "obj", payload.clone()))
        .await
        .expect("put");
    // Arm a Compliance-mode retention 1 day in the future.
    mgr.set(
        "bucket",
        "obj",
        ObjectLockState {
            mode: Some(LockMode::Compliance),
            retain_until: Some(Utc::now() + ChronoDuration::days(1)),
            legal_hold_on: false,
        },
    );

    // DELETE without bypass → 403 AccessDenied.
    let err = s4
        .delete_object(delete_request("bucket", "obj"))
        .await
        .expect_err("Compliance must block delete");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);

    // DELETE WITH bypass=true → still 403 (Compliance is never bypassable).
    let err = s4
        .delete_object(delete_request_with_bypass("bucket", "obj", true))
        .await
        .expect_err("Compliance must reject bypass header");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);

    // Once retention has expired, DELETE succeeds.
    mgr.set(
        "bucket",
        "obj",
        ObjectLockState {
            mode: Some(LockMode::Compliance),
            retain_until: Some(Utc::now() - ChronoDuration::seconds(1)),
            legal_hold_on: false,
        },
    );
    s4.delete_object(delete_request("bucket", "obj"))
        .await
        .expect("expired Compliance lock must permit delete");
    // After delete the per-object lock state is cleared so the freed key
    // can be re-armed by a future PUT under the bucket default.
    assert!(mgr.get("bucket", "obj").is_none());
}

#[tokio::test]
async fn object_lock_governance_mode_allows_delete_with_bypass_header() {
    use chrono::{Duration as ChronoDuration, Utc};
    use s4_server::object_lock::{LockMode, ObjectLockState};

    let (s4, mgr) = make_object_locked_s4(CodecKind::CpuZstd);
    let payload = Bytes::from_static(b"locked-governance");
    s4.put_object(put_request("bucket", "gov", payload.clone()))
        .await
        .expect("put");
    mgr.set(
        "bucket",
        "gov",
        ObjectLockState {
            mode: Some(LockMode::Governance),
            retain_until: Some(Utc::now() + ChronoDuration::days(7)),
            legal_hold_on: false,
        },
    );

    // Without bypass header → AccessDenied.
    let err = s4
        .delete_object(delete_request("bucket", "gov"))
        .await
        .expect_err("Governance without bypass must be denied");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);

    // With bypass=true → permitted.
    s4.delete_object(delete_request_with_bypass("bucket", "gov", true))
        .await
        .expect("Governance + bypass must permit delete");
    // Re-arm via per-object retention is cleared after the permitted
    // delete (so a subsequent PUT can pick up bucket-default retention).
    assert!(mgr.get("bucket", "gov").is_none());
}

#[tokio::test]
async fn object_lock_legal_hold_blocks_delete_independent_of_retention() {
    use s4_server::object_lock::ObjectLockState;

    let (s4, mgr) = make_object_locked_s4(CodecKind::CpuZstd);
    let payload = Bytes::from_static(b"legal-hold-only");
    s4.put_object(put_request("bucket", "lh", payload.clone()))
        .await
        .expect("put");
    // No retention at all — pure legal hold.
    mgr.set(
        "bucket",
        "lh",
        ObjectLockState {
            mode: None,
            retain_until: None,
            legal_hold_on: true,
        },
    );

    // Bypass header is irrelevant; legal hold cannot be bypassed.
    let err = s4
        .delete_object(delete_request_with_bypass("bucket", "lh", true))
        .await
        .expect_err("legal hold must block delete even with bypass");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);

    // Lifting the legal hold via the public API (PutObjectLegalHold)
    // unblocks delete.
    let put_lh = PutObjectLegalHoldInput {
        bucket: "bucket".into(),
        key: "lh".into(),
        legal_hold: Some(ObjectLockLegalHold {
            status: Some(ObjectLockLegalHoldStatus::from_static(
                ObjectLockLegalHoldStatus::OFF,
            )),
        }),
        ..Default::default()
    };
    let lh_req = S3Request {
        input: put_lh,
        method: http::Method::PUT,
        uri: "/bucket/lh?legal-hold".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    s4.put_object_legal_hold(lh_req)
        .await
        .expect("toggle legal hold off");
    // Confirm GET reflects OFF.
    let get_lh = GetObjectLegalHoldInput {
        bucket: "bucket".into(),
        key: "lh".into(),
        ..Default::default()
    };
    let get_req = S3Request {
        input: get_lh,
        method: http::Method::GET,
        uri: "/bucket/lh?legal-hold".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    let resp = s4
        .get_object_legal_hold(get_req)
        .await
        .expect("get legal hold");
    let status = resp
        .output
        .legal_hold
        .as_ref()
        .and_then(|h| h.status.as_ref())
        .map(|s| s.as_str().to_owned())
        .unwrap_or_default();
    assert_eq!(status, "OFF");
    // Now delete succeeds.
    s4.delete_object(delete_request("bucket", "lh"))
        .await
        .expect("delete must succeed once legal hold is off");
}

#[tokio::test]
async fn object_lock_bucket_default_auto_applies_on_put() {
    use s4_server::object_lock::{BucketObjectLockDefault, LockMode};

    let (s4, mgr) = make_object_locked_s4(CodecKind::CpuZstd);
    // Install a 30-day Governance default via the public S3 API
    // (PutObjectLockConfiguration).
    let put_cfg_input = PutObjectLockConfigurationInput {
        bucket: "bucket".into(),
        object_lock_configuration: Some(ObjectLockConfiguration {
            object_lock_enabled: Some(ObjectLockEnabled::from_static(
                ObjectLockEnabled::ENABLED,
            )),
            rule: Some(ObjectLockRule {
                default_retention: Some(DefaultRetention {
                    days: Some(30),
                    mode: Some(ObjectLockRetentionMode::from_static(
                        ObjectLockRetentionMode::GOVERNANCE,
                    )),
                    years: None,
                }),
            }),
        }),
        ..Default::default()
    };
    let put_cfg_req = S3Request {
        input: put_cfg_input,
        method: http::Method::PUT,
        uri: "/bucket?object-lock".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    s4.put_object_lock_configuration(put_cfg_req)
        .await
        .expect("put bucket lock config");
    // Sanity: the manager now has a default.
    let default = mgr.bucket_default("bucket").expect("default present");
    assert_eq!(default.mode, LockMode::Governance);
    assert_eq!(default.retention_days, 30);
    assert_eq!(
        default,
        BucketObjectLockDefault {
            mode: LockMode::Governance,
            retention_days: 30,
        }
    );

    // Read-back via the public S3 API also surfaces it.
    let get_cfg_req = S3Request {
        input: GetObjectLockConfigurationInput {
            bucket: "bucket".into(),
            ..Default::default()
        },
        method: http::Method::GET,
        uri: "/bucket?object-lock".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    let cfg_resp = s4
        .get_object_lock_configuration(get_cfg_req)
        .await
        .expect("get bucket lock config");
    let days = cfg_resp
        .output
        .object_lock_configuration
        .as_ref()
        .and_then(|c| c.rule.as_ref())
        .and_then(|r| r.default_retention.as_ref())
        .and_then(|d| d.days);
    assert_eq!(days, Some(30));

    // PUT a new object — bucket-default retention auto-applies.
    let payload = Bytes::from_static(b"auto-locked");
    s4.put_object(put_request("bucket", "auto", payload.clone()))
        .await
        .expect("put");
    let state = mgr.get("bucket", "auto").expect("auto-applied state");
    assert_eq!(state.mode, Some(LockMode::Governance));
    let until = state.retain_until.expect("retain_until set");
    let now = chrono::Utc::now();
    let secs = (until - now).num_seconds();
    // 30 days ± 5 seconds slack.
    assert!(
        (30 * 86400 - 5..=30 * 86400 + 5).contains(&secs),
        "retain_until off: {secs} seconds from now (expected ~30 days)"
    );

    // DELETE without bypass → AccessDenied (Governance applies).
    let err = s4
        .delete_object(delete_request("bucket", "auto"))
        .await
        .expect_err("auto-applied Governance must block delete");
    assert_eq!(err.code(), &S3ErrorCode::AccessDenied);

    // DELETE with bypass=true → permitted.
    s4.delete_object(delete_request_with_bypass("bucket", "auto", true))
        .await
        .expect("bypass permits delete");
}

// ------------------------------------------------------------------
// v0.5 #32: compliance-mode strict — every PUT must declare SSE.
// ------------------------------------------------------------------

#[tokio::test]
async fn compliance_strict_rejects_put_without_sse() {
    let backend = MemoryBackend::new();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_compliance_strict(true);
    // PUT with no SSE header at all → 400 InvalidRequest.
    let err = s4
        .put_object(put_request("bucket", "k", Bytes::from_static(b"plaintext")))
        .await
        .expect_err("compliance-strict must reject plaintext PUT");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("InvalidRequest") || dbg.contains("compliance-mode strict"),
        "expected compliance-mode reject, got {dbg}"
    );
}

#[tokio::test]
async fn compliance_strict_accepts_put_with_keyring_configured() {
    use std::sync::Arc;
    let backend = MemoryBackend::new();
    let key = Arc::new(s4_server::sse::SseKey::from_bytes(&[0x77u8; 32]).unwrap());
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    )
    .with_sse_key(key)
    .with_compliance_strict(true);
    // Keyring configured → server-side SSE-S4 is implicit, plain PUT
    // is OK because the gateway will encrypt regardless.
    let _ = s4
        .put_object(put_request("bucket", "k", Bytes::from_static(b"plaintext")))
        .await
        .expect("strict PUT must succeed when SSE-S4 keyring is configured");
}


// ---- v0.6 #38: CORS bucket configuration + preflight ----

fn put_bucket_cors_request(
    bucket: &str,
    rules: Vec<CORSRule>,
) -> S3Request<PutBucketCorsInput> {
    let input = PutBucketCorsInput {
        bucket: bucket.into(),
        cors_configuration: CORSConfiguration { cors_rules: rules },
        checksum_algorithm: None,
        content_md5: None,
        expected_bucket_owner: None,
    };
    S3Request {
        input,
        method: http::Method::PUT,
        uri: format!("/{bucket}?cors").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn get_bucket_cors_request(bucket: &str) -> S3Request<GetBucketCorsInput> {
    let input = GetBucketCorsInput {
        bucket: bucket.into(),
        expected_bucket_owner: None,
    };
    S3Request {
        input,
        method: http::Method::GET,
        uri: format!("/{bucket}?cors").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

#[tokio::test]
async fn cors_put_get_round_trip() {
    use std::sync::Arc;
    let backend = MemoryBackend::new();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::Passthrough),
        make_dispatcher(CodecKind::Passthrough),
    )
    .with_cors(Arc::new(s4_server::cors::CorsManager::new()));

    let rules = vec![
        CORSRule {
            allowed_origins: vec!["https://app.example.com".into()],
            allowed_methods: vec!["GET".into(), "PUT".into()],
            allowed_headers: Some(vec!["Content-Type".into(), "X-Amz-Date".into()]),
            expose_headers: Some(vec!["ETag".into()]),
            id: Some("rule-1".into()),
            max_age_seconds: Some(3600),
        },
        CORSRule {
            allowed_origins: vec!["*".into()],
            allowed_methods: vec!["GET".into()],
            allowed_headers: None,
            expose_headers: None,
            id: None,
            max_age_seconds: None,
        },
    ];

    s4.put_bucket_cors(put_bucket_cors_request("b", rules.clone()))
        .await
        .expect("PutBucketCors");

    let resp = s4
        .get_bucket_cors(get_bucket_cors_request("b"))
        .await
        .expect("GetBucketCors");
    let got = resp.output.cors_rules.expect("rules present");
    assert_eq!(got.len(), 2, "two rules round-trip");

    // Rule 1 — explicit headers / expose / id / max-age preserved.
    assert_eq!(
        got[0].allowed_origins,
        vec!["https://app.example.com".to_string()]
    );
    assert_eq!(
        got[0].allowed_methods,
        vec!["GET".to_string(), "PUT".to_string()]
    );
    assert_eq!(
        got[0].allowed_headers.as_ref().expect("allowed_headers"),
        &vec!["Content-Type".to_string(), "X-Amz-Date".to_string()]
    );
    assert_eq!(
        got[0].expose_headers.as_ref().expect("expose_headers"),
        &vec!["ETag".to_string()]
    );
    assert_eq!(got[0].id.as_deref(), Some("rule-1"));
    assert_eq!(got[0].max_age_seconds, Some(3600));

    // Rule 2 — empty optional fields collapse back to None.
    assert_eq!(got[1].allowed_origins, vec!["*".to_string()]);
    assert_eq!(got[1].allowed_methods, vec!["GET".to_string()]);
    assert!(got[1].allowed_headers.is_none());
    assert!(got[1].expose_headers.is_none());
    assert_eq!(got[1].max_age_seconds, None);

    // Sanity: GET on bucket with no config → NoSuchCORSConfiguration 404.
    let err = s4
        .get_bucket_cors(get_bucket_cors_request("ghost"))
        .await
        .expect_err("missing config must error");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("NoSuchCORSConfiguration"),
        "expected NoSuchCORSConfiguration, got {dbg}"
    );
}

#[tokio::test]
async fn cors_preflight_match_returns_correct_headers() {
    use std::sync::Arc;
    let backend = MemoryBackend::new();
    let cors = Arc::new(s4_server::cors::CorsManager::new());
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::Passthrough),
        make_dispatcher(CodecKind::Passthrough),
    )
    .with_cors(Arc::clone(&cors));

    // Seed a rule via PutBucketCors so we exercise the same wire path
    // an SDK client would use.
    s4.put_bucket_cors(put_bucket_cors_request(
        "b",
        vec![CORSRule {
            allowed_origins: vec!["https://app.example.com".into()],
            allowed_methods: vec!["GET".into(), "PUT".into(), "POST".into()],
            allowed_headers: Some(vec!["Content-Type".into(), "X-Amz-Date".into()]),
            expose_headers: Some(vec!["ETag".into(), "X-Amz-Request-Id".into()]),
            id: Some("explicit-origin".into()),
            max_age_seconds: Some(600),
        }],
    ))
    .await
    .expect("PutBucketCors");

    // Matching preflight — mixed-case header to confirm CI matching.
    let headers = s4
        .handle_preflight(
            "b",
            "https://app.example.com",
            "PUT",
            &["content-type".to_owned()],
        )
        .expect("preflight should match");
    assert_eq!(
        headers.get("Access-Control-Allow-Origin").map(String::as_str),
        Some("https://app.example.com"),
        "explicit origin echoed back verbatim"
    );
    assert_eq!(
        headers.get("Access-Control-Allow-Methods").map(String::as_str),
        Some("GET, PUT, POST")
    );
    assert_eq!(
        headers.get("Access-Control-Allow-Headers").map(String::as_str),
        Some("Content-Type, X-Amz-Date")
    );
    assert_eq!(
        headers.get("Access-Control-Max-Age").map(String::as_str),
        Some("600")
    );
    assert_eq!(
        headers
            .get("Access-Control-Expose-Headers")
            .map(String::as_str),
        Some("ETag, X-Amz-Request-Id")
    );

    // Non-matching method must return None (caller turns into 403).
    assert!(
        s4.handle_preflight(
            "b",
            "https://app.example.com",
            "DELETE",
            &["content-type".to_owned()],
        )
        .is_none(),
        "method outside rule must miss"
    );

    // Wildcard origin — replace config and confirm "*" echoes back as "*".
    s4.put_bucket_cors(put_bucket_cors_request(
        "b",
        vec![CORSRule {
            allowed_origins: vec!["*".into()],
            allowed_methods: vec!["GET".into()],
            allowed_headers: Some(vec!["*".into()]),
            expose_headers: None,
            id: None,
            max_age_seconds: Some(60),
        }],
    ))
    .await
    .expect("PutBucketCors (wildcard)");
    let headers = s4
        .handle_preflight("b", "https://anything", "GET", &["X-Custom".to_owned()])
        .expect("wildcard preflight");
    assert_eq!(
        headers.get("Access-Control-Allow-Origin").map(String::as_str),
        Some("*"),
        "wildcard rule echoes Access-Control-Allow-Origin: *"
    );

    // Sanity: handle_preflight without a manager returns None.
    let backend2 = MemoryBackend::new();
    let plain = S4Service::new(
        backend2,
        make_registry(CodecKind::Passthrough),
        make_dispatcher(CodecKind::Passthrough),
    );
    assert!(
        plain
            .handle_preflight("b", "https://x", "GET", &[])
            .is_none(),
        "no manager attached → preflight is a no-op"
    );
}

// =====================================================================
// v0.6 #41: S3 Select end-to-end — PUT a 100-row CSV via S4 (so it
// passes through the codec / framing path), then SelectObjectContent
// with a WHERE filter and confirm the matched-rows-only payload comes
// back inside the AWS event-stream `Records` frame (followed by Stats +
// End sentinel events).
// =====================================================================
#[tokio::test]
async fn s3_select_csv_filter_e2e() {
    use futures::StreamExt as _;

    let backend = MemoryBackend::new();
    let s4 = S4Service::new(
        backend,
        make_registry(CodecKind::CpuZstd),
        make_dispatcher(CodecKind::CpuZstd),
    );

    let mut csv_body = String::from("name,age\n");
    for i in 0..100 {
        csv_body.push_str(&format!("user{i},{i}\n"));
    }
    s4.put_object(put_request(
        "selbucket",
        "people.csv",
        Bytes::from(csv_body.into_bytes()),
    ))
    .await
    .expect("PUT 100-row CSV");

    let select_input = SelectObjectContentInput {
        bucket: "selbucket".into(),
        key: "people.csv".into(),
        expected_bucket_owner: None,
        sse_customer_algorithm: None,
        sse_customer_key: None,
        sse_customer_key_md5: None,
        request: SelectObjectContentRequest {
            expression: "SELECT name, age FROM s3object WHERE age > 90".into(),
            expression_type: ExpressionType::from_static(ExpressionType::SQL),
            input_serialization: InputSerialization {
                csv: Some(CSVInput {
                    file_header_info: Some(FileHeaderInfo::from_static(FileHeaderInfo::USE)),
                    field_delimiter: Some(",".into()),
                    ..Default::default()
                }),
                compression_type: None,
                json: None,
                parquet: None,
            },
            output_serialization: OutputSerialization {
                csv: Some(CSVOutput::default()),
                json: None,
            },
            request_progress: None,
            scan_range: None,
        },
    };
    let req = S3Request {
        input: select_input,
        method: http::Method::POST,
        uri: "/selbucket/people.csv?select&select-type=2".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    let resp = s4
        .select_object_content(req)
        .await
        .expect("SelectObjectContent should succeed");

    let mut stream = resp.output.payload.expect("payload event stream");
    let mut records_payload: Vec<u8> = Vec::new();
    let mut saw_end = false;
    let mut saw_stats = false;
    while let Some(item) = stream.next().await {
        let ev = item.expect("event-stream items must not be Err");
        match ev {
            SelectObjectContentEvent::Records(r) => {
                if let Some(p) = r.payload {
                    records_payload.extend_from_slice(&p);
                }
            }
            SelectObjectContentEvent::Stats(s) => {
                saw_stats = true;
                let stats = s.details.expect("Stats.details");
                assert!(stats.bytes_scanned.unwrap_or(0) > 100);
                assert!(stats.bytes_returned.unwrap_or(0) > 0);
            }
            SelectObjectContentEvent::End(_) => {
                saw_end = true;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(saw_stats, "Stats event missing from stream");
    assert!(saw_end, "End sentinel missing from stream");

    let payload_str = std::str::from_utf8(&records_payload)
        .expect("Records payload must be UTF-8 CSV");
    let rows: Vec<&str> = payload_str
        .split("\r\n")
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        rows.len(),
        9,
        "WHERE age > 90 expects rows 91..=99 = 9 rows, got: {rows:?}"
    );
    assert!(rows.iter().any(|r| r.starts_with("user91,91")));
    assert!(rows.iter().any(|r| r.starts_with("user99,99")));
    assert!(!rows.iter().any(|r| r.starts_with("user90,90")));
    assert!(!rows.iter().any(|r| r.starts_with("user50,50")));

    assert!(
        s4_server::select::select_gpu(
            "SELECT * FROM s3object",
            b"",
            &s4_server::select::SelectInputFormat::Csv {
                has_header: true,
                delimiter: ',',
            },
        )
        .is_none(),
        "GPU select stub must return None for v0.6"
    );
}

// =========================================================================
// v0.6 #36: S3 Inventory configuration + CSV emission.
// =========================================================================

type InventoryHarness = (
    S4Service<MemoryBackend>,
    Arc<s4_server::inventory::InventoryManager>,
    Arc<Mutex<HashMap<(String, String), StoredObject>>>,
);

fn make_inventory_s4(codec: CodecKind) -> InventoryHarness {
    let backend = MemoryBackend::new();
    let backend_view = backend.shared();
    let mgr = Arc::new(s4_server::inventory::InventoryManager::new());
    let s4 = S4Service::new(backend, make_registry(codec), make_dispatcher(codec))
        .with_inventory(Arc::clone(&mgr));
    (s4, mgr, backend_view)
}

fn put_bucket_inventory_request(
    bucket: &str,
    id: &str,
    cfg: InventoryConfiguration,
) -> S3Request<PutBucketInventoryConfigurationInput> {
    let input = PutBucketInventoryConfigurationInput {
        bucket: bucket.into(),
        id: id.into(),
        inventory_configuration: cfg,
        expected_bucket_owner: None,
    };
    S3Request {
        input,
        method: http::Method::PUT,
        uri: format!("/{bucket}?inventory&id={id}").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn aws_inventory_config(id: &str, dst_bucket: &str, dst_prefix: &str) -> InventoryConfiguration {
    InventoryConfiguration {
        id: id.to_owned(),
        is_enabled: true,
        included_object_versions: InventoryIncludedObjectVersions::from_static(
            InventoryIncludedObjectVersions::CURRENT,
        ),
        destination: InventoryDestination {
            s3_bucket_destination: InventoryS3BucketDestination {
                account_id: None,
                bucket: dst_bucket.into(),
                encryption: None,
                format: InventoryFormat::from_static(InventoryFormat::CSV),
                prefix: Some(dst_prefix.into()),
            },
        },
        schedule: InventorySchedule {
            frequency: InventoryFrequency::from_static(InventoryFrequency::DAILY),
        },
        filter: None,
        optional_fields: None,
    }
}

#[tokio::test]
async fn inventory_put_get_round_trip() {
    let (s4, _mgr, _view) = make_inventory_s4(CodecKind::Passthrough);
    let cfg = aws_inventory_config("daily-report", "audit-dst", "inventories");
    s4.put_bucket_inventory_configuration(put_bucket_inventory_request(
        "src-bucket",
        "daily-report",
        cfg.clone(),
    ))
    .await
    .expect("PutBucketInventoryConfiguration");

    let get_req = S3Request {
        input: GetBucketInventoryConfigurationInput {
            bucket: "src-bucket".into(),
            id: "daily-report".into(),
            expected_bucket_owner: None,
        },
        method: http::Method::GET,
        uri: "/src-bucket?inventory&id=daily-report".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    let resp = s4
        .get_bucket_inventory_configuration(get_req)
        .await
        .expect("GetBucketInventoryConfiguration");
    let got = resp
        .output
        .inventory_configuration
        .expect("config must round-trip");
    assert_eq!(got.id, "daily-report");
    assert_eq!(got.destination.s3_bucket_destination.bucket, "audit-dst");
    assert_eq!(
        got.destination.s3_bucket_destination.prefix.as_deref(),
        Some("inventories")
    );
    assert_eq!(got.schedule.frequency.as_str(), "Daily");
    assert_eq!(got.included_object_versions.as_str(), "Current");

    let list_req = S3Request {
        input: ListBucketInventoryConfigurationsInput {
            bucket: "src-bucket".into(),
            continuation_token: None,
            expected_bucket_owner: None,
        },
        method: http::Method::GET,
        uri: "/src-bucket?inventory".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    let list_resp = s4
        .list_bucket_inventory_configurations(list_req)
        .await
        .expect("ListBucketInventoryConfigurations");
    let entries = list_resp
        .output
        .inventory_configuration_list
        .expect("list must contain entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "daily-report");

    let del_req = S3Request {
        input: DeleteBucketInventoryConfigurationInput {
            bucket: "src-bucket".into(),
            id: "daily-report".into(),
            expected_bucket_owner: None,
        },
        method: http::Method::DELETE,
        uri: "/src-bucket?inventory&id=daily-report".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    s4.delete_bucket_inventory_configuration(del_req)
        .await
        .expect("DeleteBucketInventoryConfiguration");

    let get_again = S3Request {
        input: GetBucketInventoryConfigurationInput {
            bucket: "src-bucket".into(),
            id: "daily-report".into(),
            expected_bucket_owner: None,
        },
        method: http::Method::GET,
        uri: "/src-bucket?inventory&id=daily-report".parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    };
    let err = s4
        .get_bucket_inventory_configuration(get_again)
        .await
        .expect_err("deleted config must yield NoSuchConfiguration");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("NoSuchConfiguration"),
        "expected NoSuchConfiguration after delete, got {dbg}"
    );
}

#[tokio::test]
async fn inventory_csv_emission_writes_to_destination_prefix() {
    let (s4, mgr, backend_view) = make_inventory_s4(CodecKind::Passthrough);
    let dst_prefix = "inventories";
    let cfg = aws_inventory_config("d1", "audit-dst", dst_prefix);
    s4.put_bucket_inventory_configuration(put_bucket_inventory_request(
        "src", "d1", cfg,
    ))
    .await
    .expect("Put inventory config");

    for (k, body) in [
        ("alpha.txt", &b"AAA"[..]),
        ("nested/beta.bin", &b"BB"[..]),
        ("z.txt", &b"Z"[..]),
    ] {
        s4.put_object(put_request("src", k, Bytes::copy_from_slice(body)))
            .await
            .expect("put source object");
    }

    let rows: Vec<s4_server::inventory::InventoryRow> = {
        let view = backend_view.lock().unwrap();
        let mut vs: Vec<_> = view
            .iter()
            .filter_map(|((b, k), o)| {
                if b == "src" {
                    Some(s4_server::inventory::InventoryRow {
                        bucket: b.clone(),
                        key: k.clone(),
                        version_id: None,
                        is_latest: true,
                        is_delete_marker: false,
                        size: o.body.len() as u64,
                        last_modified: chrono::Utc::now(),
                        etag: format!("\"etag-of-{k}\""),
                        storage_class: "STANDARD".into(),
                        encryption_status: "NOT-SSE".into(),
                    })
                } else {
                    None
                }
            })
            .collect();
        vs.sort_by(|a, b| a.key.cmp(&b.key));
        vs
    };
    assert_eq!(rows.len(), 3);

    let now = chrono::Utc::now();
    let cfg_internal = s4_server::inventory::InventoryConfig {
        id: "d1".into(),
        bucket: "src".into(),
        destination_bucket: "audit-dst".into(),
        destination_prefix: dst_prefix.into(),
        frequency_hours: 24,
        format: s4_server::inventory::InventoryFormat::Csv,
        included_object_versions: s4_server::inventory::IncludedVersions::Current,
    };
    let csv_bytes = s4_server::inventory::render_csv(rows.into_iter());
    let csv_key = s4_server::inventory::csv_destination_key(&cfg_internal, now);
    let manifest_key = s4_server::inventory::manifest_destination_key(&cfg_internal, now);
    let manifest_body = s4_server::inventory::render_manifest_json(
        &cfg_internal,
        std::slice::from_ref(&csv_key),
        &["dummy-md5".to_owned()],
        now,
    )
    .into_bytes();
    s4.put_object(put_request(
        "audit-dst",
        &csv_key,
        Bytes::from(csv_bytes.clone()),
    ))
    .await
    .expect("PUT inventory CSV");
    s4.put_object(put_request(
        "audit-dst",
        &manifest_key,
        Bytes::from(manifest_body.clone()),
    ))
    .await
    .expect("PUT inventory manifest");
    mgr.mark_run("src", "d1", now);

    let got = s4
        .get_object(get_request("audit-dst", &csv_key))
        .await
        .expect("GET emitted inventory CSV");
    let got_body = read_back(got).await;
    let csv_text = std::str::from_utf8(&got_body).expect("utf8 csv");
    let mut lines = csv_text.lines();
    assert_eq!(
        lines.next().unwrap(),
        "Bucket,Key,VersionId,IsLatest,IsDeleteMarker,Size,LastModifiedDate,ETag,StorageClass,EncryptionStatus"
    );
    let body_lines: Vec<&str> = lines.collect();
    assert_eq!(body_lines.len(), 3, "one row per source object");
    assert!(body_lines.iter().any(|l| l.contains("\"alpha.txt\"")));
    assert!(body_lines.iter().any(|l| l.contains("\"nested/beta.bin\"")));
    assert!(body_lines.iter().any(|l| l.contains("\"z.txt\"")));

    let got_mf = s4
        .get_object(get_request("audit-dst", &manifest_key))
        .await
        .expect("GET emitted manifest.json");
    let mf_body = read_back(got_mf).await;
    let parsed: serde_json::Value = serde_json::from_slice(&mf_body).expect("manifest json");
    assert_eq!(parsed["sourceBucket"], "src");
    assert_eq!(parsed["destinationBucket"], "audit-dst");

    assert!(!mgr.due("src", "d1", now + chrono::Duration::hours(1)));
    assert!(mgr.due("src", "d1", now + chrono::Duration::hours(25)));

    assert!(csv_key.starts_with("inventories/src/d1/data/"));
    assert!(manifest_key.starts_with("inventories/src/d1/"));
    assert!(manifest_key.ends_with("manifest.json"));
}
