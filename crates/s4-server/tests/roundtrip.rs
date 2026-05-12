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
use s4_codec::passthrough::Passthrough;
use s4_server::S4Service;
use s4_server::blob::{bytes_to_blob, collect_blob};

/// In-memory な (bucket, key) → (body, metadata) ストア。
/// 実装するのは S4 が呼ぶ最小集合: `put_object`, `get_object`, `head_object`。
/// それ以外は trait default (NotImplemented) のまま。
struct MemoryBackend {
    inner: Mutex<HashMap<(String, String), StoredObject>>,
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
            inner: Mutex::new(HashMap::new()),
        }
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
    let s4 = S4Service::new(backend, Arc::new(CpuZstd::default()));

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
    let s4 = S4Service::new(backend, Arc::new(Passthrough));

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
    let s4 = S4Service::new(backend, Arc::new(CpuZstd::default()));

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

    let s4 = S4Service::new(backend, Arc::new(CpuZstd::default()));
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
