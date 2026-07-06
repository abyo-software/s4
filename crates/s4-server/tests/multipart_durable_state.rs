//! Durable multipart part-state regression tests (`.s4mpu/` records,
//! `crate::mpu_durable`).
//!
//! Pre-durable-state, the client-transparent multipart composite ETag
//! (`MD5(concat(original-part-MD5s))-N`) was computed from per-upload
//! state held ONLY in process memory: a gateway restart mid-upload, or
//! a multi-gateway deployment where parts landed on different
//! instances, completed successfully but left the object with the
//! backend composite ETag and no logical stamp. These tests drive the
//! durable-record path end to end against an in-memory multipart
//! backend (same harness family as `tests/multipart_audit_71.rs` — no
//! Docker, runs on every plain `cargo test`):
//!
//! - restart simulation: parts uploaded through one `S4Service`
//!   instance, Complete issued through a FRESH instance sharing only
//!   the backend — the composite must still be exact and stamped.
//! - two-instance simulation: parts split across two live instances,
//!   either one completes with the full composite; strict part-ETag
//!   validation holds for parts the completing instance never saw.
//! - flag-off (`--no-durable-multipart-state`): bit-for-bit the
//!   pre-durable behaviour (ListParts fallback, no stamp, no records).
//! - cleanup: Complete/Abort delete exactly their own upload's records.
//! - transparency: `.s4mpu/` keys are hidden from ListObjectsV2 and
//!   blocked for client writes.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use md5::{Digest as _, Md5};
use s3s::dto::*;
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::AlwaysDispatcher;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::blob::{bytes_to_blob, collect_blob};
use s4_server::mpu_durable;

const MAX_BODY: usize = 256 * 1024 * 1024;

fn make_registry() -> Arc<CodecRegistry> {
    Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    )
}

fn md5_hex(bytes: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(bytes);
    let digest: [u8; 16] = h.finalize().into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The AWS composite ETag over the ORIGINAL part payloads, in manifest
/// (ascending part-number) order.
fn expected_composite(parts: &[&[u8]]) -> String {
    let mut concat: Vec<u8> = Vec::new();
    for p in parts {
        let mut h = Md5::new();
        h.update(p);
        let digest: [u8; 16] = h.finalize().into();
        concat.extend_from_slice(&digest);
    }
    format!("{}-{}", md5_hex(&concat), parts.len())
}

// =========================================================================
// Shared-state in-memory backend with multipart + listing + copy support.
// Two `S4Service` instances built over clones of the same
// `Arc<Mutex<InnerState>>` model two gateways in front of one backend
// (or one gateway before/after a restart).
// =========================================================================

#[derive(Clone)]
struct StoredObject {
    body: Bytes,
    metadata: Option<Metadata>,
    content_type: Option<ContentType>,
}

#[derive(Default)]
struct InnerState {
    /// `(bucket, key)` → object; `BTreeMap` so listings come out in key
    /// order like a real backend.
    objects: BTreeMap<(String, String), StoredObject>,
    mpu_parts: HashMap<String, BTreeMap<i32, Bytes>>,
    /// upload_id → (bucket, key, Create-time metadata) — the metadata is
    /// stamped onto the assembled object like MinIO/AWS do.
    mpu_meta: HashMap<String, (String, String, Option<Metadata>)>,
    next_upload_id: u64,
}

struct MemBackend {
    state: Arc<Mutex<InnerState>>,
}

impl MemBackend {
    fn from_shared(state: Arc<Mutex<InnerState>>) -> Self {
        Self { state }
    }
}

/// Keys currently stored under the reserved `.s4mpu/` prefix.
fn mpu_record_keys(state: &Mutex<InnerState>) -> Vec<String> {
    state
        .lock()
        .unwrap()
        .objects
        .keys()
        .filter(|(_, k)| mpu_durable::is_mpu_state_key(k))
        .map(|(_, k)| k.clone())
        .collect()
}

#[async_trait::async_trait]
impl S3 for MemBackend {
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let body = match req.input.body.take() {
            Some(blob) => collect_blob(blob, MAX_BODY).await.map_err(|e| {
                S3Error::with_message(S3ErrorCode::InternalError, format!("collect: {e}"))
            })?,
            None => Bytes::new(),
        };
        let etag = md5_hex(&body);
        let stored = StoredObject {
            body,
            metadata: req.input.metadata.clone(),
            content_type: req.input.content_type.clone(),
        };
        self.state
            .lock()
            .unwrap()
            .objects
            .insert((req.input.bucket.clone(), req.input.key.clone()), stored);
        Ok(S3Response::new(PutObjectOutput {
            e_tag: Some(ETag::Strong(etag)),
            ..Default::default()
        }))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let stored = {
            let st = self.state.lock().unwrap();
            st.objects.get(&key).cloned()
        };
        let stored = stored.ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        let len = stored.body.len() as i64;
        let etag = md5_hex(&stored.body);
        Ok(S3Response::new(GetObjectOutput {
            body: Some(bytes_to_blob(stored.body)),
            content_length: Some(len),
            metadata: stored.metadata,
            content_type: stored.content_type,
            e_tag: Some(ETag::Strong(etag)),
            ..Default::default()
        }))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let st = self.state.lock().unwrap();
        let stored = st
            .objects
            .get(&key)
            .ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        Ok(S3Response::new(HeadObjectOutput {
            content_length: Some(stored.body.len() as i64),
            metadata: stored.metadata.clone(),
            content_type: stored.content_type.clone(),
            e_tag: Some(ETag::Strong(md5_hex(&stored.body))),
            ..Default::default()
        }))
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        self.state.lock().unwrap().objects.remove(&key);
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }

    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let bucket = req.input.bucket.clone();
        let prefix = req.input.prefix.clone().unwrap_or_default();
        let st = self.state.lock().unwrap();
        let contents: Vec<Object> = st
            .objects
            .iter()
            .filter(|((b, k), _)| *b == bucket && k.starts_with(&prefix))
            .map(|((_, k), o)| Object {
                key: Some(k.clone()),
                size: Some(o.body.len() as i64),
                e_tag: Some(ETag::Strong(md5_hex(&o.body))),
                ..Default::default()
            })
            .collect();
        let key_count = contents.len() as i32;
        Ok(S3Response::new(ListObjectsV2Output {
            contents: Some(contents),
            key_count: Some(key_count),
            is_truncated: Some(false),
            ..Default::default()
        }))
    }

    /// Self-copy metadata stamp support (`MetadataDirective=REPLACE`):
    /// keeps the stored bytes, replaces the metadata with the request's.
    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        let CopySource::Bucket {
            bucket: src_bucket,
            key: src_key,
            ..
        } = &req.input.copy_source
        else {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "access-point copy source unsupported",
            ));
        };
        let src = (src_bucket.to_string(), src_key.to_string());
        let mut st = self.state.lock().unwrap();
        let mut obj = st
            .objects
            .get(&src)
            .cloned()
            .ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        if req
            .input
            .metadata_directive
            .as_ref()
            .map(|d| d.as_str() == MetadataDirective::REPLACE)
            .unwrap_or(false)
        {
            obj.metadata = req.input.metadata.clone();
            if req.input.content_type.is_some() {
                obj.content_type = req.input.content_type.clone();
            }
        }
        st.objects
            .insert((req.input.bucket.clone(), req.input.key.clone()), obj);
        Ok(S3Response::new(CopyObjectOutput::default()))
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let mut st = self.state.lock().unwrap();
        st.next_upload_id += 1;
        // Hostile-shaped id (slash + plus) to prove the hex record-key
        // encoding holds up against real backends' opaque ids.
        let upload_id = format!("mpu/{}+id", st.next_upload_id);
        st.mpu_parts.insert(upload_id.clone(), BTreeMap::new());
        st.mpu_meta.insert(
            upload_id.clone(),
            (
                req.input.bucket.clone(),
                req.input.key.clone(),
                req.input.metadata.clone(),
            ),
        );
        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(req.input.bucket.clone()),
            key: Some(req.input.key.clone()),
            upload_id: Some(upload_id),
            ..Default::default()
        }))
    }

    async fn upload_part(
        &self,
        mut req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let body = match req.input.body.take() {
            Some(blob) => collect_blob(blob, MAX_BODY).await.map_err(|e| {
                S3Error::with_message(S3ErrorCode::InternalError, format!("collect: {e}"))
            })?,
            None => Bytes::new(),
        };
        let etag = md5_hex(&body);
        let mut st = self.state.lock().unwrap();
        let parts = st
            .mpu_parts
            .get_mut(req.input.upload_id.as_str())
            .ok_or_else(|| S3Error::with_message(S3ErrorCode::NoSuchUpload, "no such upload"))?;
        parts.insert(req.input.part_number, body);
        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(ETag::Strong(etag)),
            ..Default::default()
        }))
    }

    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        let st = self.state.lock().unwrap();
        let parts = st
            .mpu_parts
            .get(req.input.upload_id.as_str())
            .ok_or_else(|| S3Error::with_message(S3ErrorCode::NoSuchUpload, "no such upload"))?;
        let listed: Vec<Part> = parts
            .iter()
            .map(|(pn, body)| Part {
                part_number: Some(*pn),
                e_tag: Some(ETag::Strong(md5_hex(body))),
                size: Some(body.len() as i64),
                ..Default::default()
            })
            .collect();
        Ok(S3Response::new(ListPartsOutput {
            parts: Some(listed),
            is_truncated: Some(false),
            ..Default::default()
        }))
    }

    /// Complete with real part-ETag validation: a manifest entry whose
    /// ETag does not match the STORED (framed) part bytes is rejected
    /// with `InvalidPart`, exactly what MinIO/AWS do — this is what
    /// makes the reverse-map assertions in the tests meaningful. State
    /// is consumed only on success so a corrected retry works.
    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let upload_id = req.input.upload_id.clone();
        let manifest: Vec<CompletedPart> = req
            .input
            .multipart_upload
            .as_ref()
            .and_then(|mp| mp.parts.clone())
            .unwrap_or_default();
        let (bucket, key, assembled, composite) = {
            let st = self.state.lock().unwrap();
            let parts = st.mpu_parts.get(upload_id.as_str()).ok_or_else(|| {
                S3Error::with_message(S3ErrorCode::NoSuchUpload, "no such upload")
            })?;
            let (bucket, key, meta) = st
                .mpu_meta
                .get(upload_id.as_str())
                .cloned()
                .ok_or_else(|| S3Error::with_message(S3ErrorCode::NoSuchUpload, "no ctx"))?;
            let mut buf: Vec<u8> = Vec::new();
            let mut md5_concat: Vec<u8> = Vec::new();
            for cp in &manifest {
                let pn = cp.part_number.ok_or_else(|| {
                    S3Error::with_message(S3ErrorCode::InvalidPart, "part without number")
                })?;
                let body = parts.get(&pn).ok_or_else(|| {
                    S3Error::with_message(S3ErrorCode::InvalidPart, format!("no part {pn}"))
                })?;
                let stored_etag = md5_hex(body);
                if let Some(submitted) = cp.e_tag.as_ref()
                    && submitted.as_strong() != Some(stored_etag.as_str())
                {
                    return Err(S3Error::with_message(
                        S3ErrorCode::InvalidPart,
                        format!("part {pn} etag mismatch"),
                    ));
                }
                buf.extend_from_slice(body);
                let mut h = Md5::new();
                h.update(body);
                let digest: [u8; 16] = h.finalize().into();
                md5_concat.extend_from_slice(&digest);
            }
            let composite = format!("{}-{}", md5_hex(&md5_concat), manifest.len());
            let _ = meta.clone();
            (bucket, key, Bytes::from(buf), composite)
        };
        let mut st = self.state.lock().unwrap();
        let meta = st
            .mpu_meta
            .remove(upload_id.as_str())
            .and_then(|(_, _, m)| m);
        st.mpu_parts.remove(upload_id.as_str());
        st.objects.insert(
            (bucket.clone(), key.clone()),
            StoredObject {
                body: assembled,
                metadata: meta,
                content_type: None,
            },
        );
        Ok(S3Response::new(CompleteMultipartUploadOutput {
            bucket: Some(bucket),
            key: Some(key),
            e_tag: Some(ETag::Strong(composite)),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let mut st = self.state.lock().unwrap();
        st.mpu_parts.remove(req.input.upload_id.as_str());
        st.mpu_meta.remove(req.input.upload_id.as_str());
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }
}

// =========================================================================
// Harness: service construction + request builders.
// =========================================================================

/// One "gateway instance" over the shared backend state. A fresh call
/// with the same `state` models a restart (new empty in-memory
/// multipart side-table) or a second instance behind the LB.
fn make_service(state: &Arc<Mutex<InnerState>>) -> S4Service<MemBackend> {
    S4Service::new(
        MemBackend::from_shared(Arc::clone(state)),
        make_registry(),
        Arc::new(AlwaysDispatcher(CodecKind::CpuZstd)),
    )
}

fn req<I>(input: I, method: http::Method, uri: &str) -> S3Request<I> {
    S3Request {
        input,
        method,
        uri: uri.parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn create_mpu_req(bucket: &str, key: &str) -> S3Request<CreateMultipartUploadInput> {
    req(
        CreateMultipartUploadInput {
            bucket: bucket.into(),
            key: key.into(),
            ..Default::default()
        },
        http::Method::POST,
        &format!("/{bucket}/{key}?uploads"),
    )
}

fn upload_part_req(
    bucket: &str,
    key: &str,
    upload_id: &str,
    part_number: i32,
    body: Bytes,
) -> S3Request<UploadPartInput> {
    req(
        UploadPartInput {
            bucket: bucket.into(),
            key: key.into(),
            upload_id: upload_id.into(),
            part_number,
            body: Some(bytes_to_blob(body)),
            ..Default::default()
        },
        http::Method::PUT,
        &format!("/{bucket}/{key}?uploadId=up&partNumber={part_number}"),
    )
}

fn complete_mpu_req(
    bucket: &str,
    key: &str,
    upload_id: &str,
    parts: Vec<(i32, &str)>,
) -> S3Request<CompleteMultipartUploadInput> {
    let parts = parts
        .into_iter()
        .map(|(pn, etag)| CompletedPart {
            part_number: Some(pn),
            e_tag: Some(ETag::Strong(etag.to_owned())),
            ..Default::default()
        })
        .collect();
    req(
        CompleteMultipartUploadInput {
            bucket: bucket.into(),
            key: key.into(),
            upload_id: upload_id.into(),
            multipart_upload: Some(CompletedMultipartUpload { parts: Some(parts) }),
            ..Default::default()
        },
        http::Method::POST,
        &format!("/{bucket}/{key}?uploadId=up"),
    )
}

fn abort_mpu_req(bucket: &str, key: &str, upload_id: &str) -> S3Request<AbortMultipartUploadInput> {
    req(
        AbortMultipartUploadInput {
            bucket: bucket.into(),
            key: key.into(),
            upload_id: upload_id.into(),
            ..Default::default()
        },
        http::Method::DELETE,
        &format!("/{bucket}/{key}?uploadId=up"),
    )
}

fn get_req(bucket: &str, key: &str) -> S3Request<GetObjectInput> {
    req(
        GetObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            ..Default::default()
        },
        http::Method::GET,
        &format!("/{bucket}/{key}"),
    )
}

fn head_req(bucket: &str, key: &str) -> S3Request<HeadObjectInput> {
    req(
        HeadObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            ..Default::default()
        },
        http::Method::HEAD,
        &format!("/{bucket}/{key}"),
    )
}

/// Two compressible parts: a >5 MiB non-final part and a small final
/// one (the shapes the gateway's padding heuristic distinguishes).
fn part_bodies() -> (Bytes, Bytes) {
    let p1 = Bytes::from(vec![b'a'; 5 * 1024 * 1024 + 137]);
    let p2 = Bytes::from(vec![b'b'; 64 * 1024]);
    (p1, p2)
}

/// Create + upload both parts through `svc`, returning
/// `(upload_id, advertised part-1 ETag, advertised part-2 ETag)`.
async fn create_and_upload_two_parts(
    svc: &S4Service<MemBackend>,
    bucket: &str,
    key: &str,
    p1: Bytes,
    p2: Bytes,
) -> (String, String, String) {
    let create = svc
        .create_multipart_upload(create_mpu_req(bucket, key))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.expect("upload id");
    let up1 = svc
        .upload_part(upload_part_req(bucket, key, &upload_id, 1, p1))
        .await
        .expect("part 1");
    let up2 = svc
        .upload_part(upload_part_req(bucket, key, &upload_id, 2, p2))
        .await
        .expect("part 2");
    let etag1 = up1.output.e_tag.expect("part 1 etag").into_value();
    let etag2 = up2.output.e_tag.expect("part 2 etag").into_value();
    (upload_id, etag1, etag2)
}

// =========================================================================
// Tests
// =========================================================================

/// Restart simulation: parts go through instance A; a FRESH instance B
/// (empty in-memory side-table, same backend) completes. Durable
/// records must carry the original-part MD5s across, so B stamps the
/// exact client-transparent composite and HEAD/GET echo it.
#[tokio::test]
async fn restart_complete_produces_exact_composite() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let (p1, p2) = part_bodies();
    let expected = expected_composite(&[&p1, &p2]);

    let svc_a = make_service(&state);
    let (upload_id, etag1, etag2) =
        create_and_upload_two_parts(&svc_a, "b", "restart/obj.bin", p1.clone(), p2.clone()).await;

    // The advertised part ETags are the ORIGINAL-payload MD5s.
    assert_eq!(etag1, md5_hex(&p1));
    assert_eq!(etag2, md5_hex(&p2));

    // One durable record per part, decodable and carrying the same pair
    // the in-memory side-table holds.
    let rec_keys = mpu_record_keys(&state);
    assert_eq!(rec_keys.len(), 2, "one record per part: {rec_keys:?}");
    let rec1_key = mpu_durable::record_key(&upload_id, 1);
    let rec1_bytes = state
        .lock()
        .unwrap()
        .objects
        .get(&("b".to_owned(), rec1_key.clone()))
        .expect("part-1 record object")
        .body
        .clone();
    let rec1 = mpu_durable::DurablePartRecord::decode(&rec1_bytes, &upload_id, 1)
        .expect("record must decode against its key");
    assert_eq!(rec1.original_md5, md5_hex(&p1));
    assert_eq!(rec1.key, "restart/obj.bin");
    // The persisted backend ETag matches the stored framed part bytes.
    let framed1 = state
        .lock()
        .unwrap()
        .mpu_parts
        .get(&upload_id)
        .and_then(|m| m.get(&1).cloned())
        .expect("framed part 1 on backend");
    assert_eq!(rec1.backend_etag, md5_hex(&framed1));

    // "Restart": a brand-new service instance over the same backend.
    let svc_b = make_service(&state);
    let resp = svc_b
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "restart/obj.bin",
            &upload_id,
            vec![(1, etag1.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect("complete on restarted instance");
    assert_eq!(
        resp.output.e_tag.expect("composite etag").into_value(),
        expected,
        "Complete must return the exact client-transparent composite"
    );

    // HEAD + GET echo the stamped composite; GET round-trips the bytes.
    let head = svc_b
        .head_object(head_req("b", "restart/obj.bin"))
        .await
        .expect("head");
    assert_eq!(
        head.output.e_tag.expect("head etag").into_value(),
        expected,
        "HEAD must echo the stamped composite"
    );
    let get = svc_b
        .get_object(get_req("b", "restart/obj.bin"))
        .await
        .expect("get");
    assert_eq!(
        get.output.e_tag.expect("get etag").into_value(),
        expected,
        "GET must echo the stamped composite"
    );
    let body = collect_blob(get.output.body.expect("body"), MAX_BODY)
        .await
        .expect("collect");
    let mut want = p1.to_vec();
    want.extend_from_slice(&p2);
    assert_eq!(body.as_ref(), want.as_slice(), "GET body must round-trip");

    // Success cleanup: no records left behind.
    assert!(
        mpu_record_keys(&state).is_empty(),
        "Complete must best-effort delete the upload's durable records"
    );
}

/// Two-instance simulation: part 1 through instance A, part 2 through
/// instance B; B completes. B's in-memory map has only part 2 — part 1
/// comes from A's durable record. Strict validation must hold for the
/// merged part too: a wrong client ETag for part 1 is InvalidPart, the
/// correct one completes with the full composite.
#[tokio::test]
async fn two_instances_split_parts_complete_with_validation() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let (p1, p2) = part_bodies();
    let expected = expected_composite(&[&p1, &p2]);

    let svc_a = make_service(&state);
    let svc_b = make_service(&state);

    let create = svc_a
        .create_multipart_upload(create_mpu_req("b", "split/obj.bin"))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.expect("upload id");
    let etag1 = svc_a
        .upload_part(upload_part_req(
            "b",
            "split/obj.bin",
            &upload_id,
            1,
            p1.clone(),
        ))
        .await
        .expect("part 1 via A")
        .output
        .e_tag
        .expect("etag 1")
        .into_value();
    let etag2 = svc_b
        .upload_part(upload_part_req(
            "b",
            "split/obj.bin",
            &upload_id,
            2,
            p2.clone(),
        ))
        .await
        .expect("part 2 via B")
        .output
        .e_tag
        .expect("etag 2")
        .into_value();

    // Wrong ETag for the part B never saw (part 1) → the durable record
    // is what enforces the mismatch → backend InvalidPart.
    let wrong = "0".repeat(32);
    let err = svc_b
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "split/obj.bin",
            &upload_id,
            vec![(1, wrong.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect_err("wrong part-1 etag must fail");
    assert_eq!(*err.code(), S3ErrorCode::InvalidPart, "err: {err:?}");
    // The failed Complete must keep the durable records for the retry.
    assert_eq!(
        mpu_record_keys(&state).len(),
        2,
        "failed Complete must not reap the records"
    );

    // Correct manifest completes on B with the full composite.
    let resp = svc_b
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "split/obj.bin",
            &upload_id,
            vec![(1, etag1.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect("complete on instance B");
    assert_eq!(resp.output.e_tag.expect("composite").into_value(), expected);
    let head = svc_b
        .head_object(head_req("b", "split/obj.bin"))
        .await
        .expect("head");
    assert_eq!(head.output.e_tag.expect("head etag").into_value(), expected);
    assert!(mpu_record_keys(&state).is_empty(), "records reaped");
}

/// Flag-off: with `with_durable_multipart_state(false)` (the
/// `--no-durable-multipart-state` CLI flag) the behaviour is exactly
/// pre-durable — no records written, restart-Complete succeeds via the
/// ListParts reverse-map, but the object keeps no logical stamp (the
/// Complete response and HEAD both present no ETag for the unstamped
/// multipart object).
#[tokio::test]
async fn flag_off_falls_back_to_listparts_without_stamp() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let (p1, p2) = part_bodies();

    let svc_a = make_service(&state).with_durable_multipart_state(false);
    let (upload_id, etag1, etag2) =
        create_and_upload_two_parts(&svc_a, "b", "off/obj.bin", p1.clone(), p2.clone()).await;
    assert!(
        mpu_record_keys(&state).is_empty(),
        "flag off must write no .s4mpu/ records"
    );

    // Restarted instance, flag off: Complete succeeds (ListParts maps
    // the manifest by part number) but nothing is stamped.
    let svc_b = make_service(&state).with_durable_multipart_state(false);
    let resp = svc_b
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "off/obj.bin",
            &upload_id,
            vec![(1, etag1.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect("complete must still succeed via ListParts fallback");
    assert!(
        resp.output.e_tag.is_none(),
        "unstamped multipart Complete must present no ETag"
    );
    let head = svc_b
        .head_object(head_req("b", "off/obj.bin"))
        .await
        .expect("head");
    assert!(
        head.output.e_tag.is_none(),
        "HEAD of an unstamped multipart object presents no ETag"
    );
    // Data path is unaffected: the bytes round-trip; GET (like Complete
    // and HEAD) presents no ETag for the unstamped multipart object.
    let get = svc_b
        .get_object(get_req("b", "off/obj.bin"))
        .await
        .expect("get");
    assert!(
        get.output.e_tag.is_none(),
        "GET of an unstamped multipart object presents no ETag"
    );
    let body = collect_blob(get.output.body.expect("body"), MAX_BODY)
        .await
        .expect("collect");
    let mut want = p1.to_vec();
    want.extend_from_slice(&p2);
    assert_eq!(body.as_ref(), want.as_slice());
}

/// Same-instance Complete (in-memory state fully present) also reaps
/// the records, and only the completed upload's records — a concurrent
/// upload's records survive.
#[tokio::test]
async fn complete_reaps_only_its_own_records() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let (p1, p2) = part_bodies();
    let svc = make_service(&state);

    let (upload_a, a1, a2) =
        create_and_upload_two_parts(&svc, "b", "gc/a.bin", p1.clone(), p2.clone()).await;
    let (upload_b, _b1, _b2) =
        create_and_upload_two_parts(&svc, "b", "gc/b.bin", p1.clone(), p2.clone()).await;
    assert_eq!(mpu_record_keys(&state).len(), 4);

    svc.complete_multipart_upload(complete_mpu_req(
        "b",
        "gc/a.bin",
        &upload_a,
        vec![(1, a1.as_str()), (2, a2.as_str())],
    ))
    .await
    .expect("complete upload A");

    let left = mpu_record_keys(&state);
    assert_eq!(left.len(), 2, "only upload B's records remain: {left:?}");
    assert!(
        left.iter()
            .all(|k| k.starts_with(&mpu_durable::upload_prefix(&upload_b))),
        "survivors all belong to upload B: {left:?}"
    );
}

/// Abort best-effort deletes the aborted upload's records.
#[tokio::test]
async fn abort_reaps_durable_records() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let (p1, p2) = part_bodies();
    let svc = make_service(&state);
    let (upload_id, _e1, _e2) =
        create_and_upload_two_parts(&svc, "b", "abort/obj.bin", p1, p2).await;
    assert_eq!(mpu_record_keys(&state).len(), 2);

    svc.abort_multipart_upload(abort_mpu_req("b", "abort/obj.bin", &upload_id))
        .await
        .expect("abort");
    assert!(
        mpu_record_keys(&state).is_empty(),
        "abort must reap the upload's durable records"
    );
}

/// Transparency: `.s4mpu/` records are hidden from ListObjectsV2 and
/// client writes into the namespace are rejected.
#[tokio::test]
async fn mpu_records_hidden_from_listing_and_write_blocked() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let (p1, p2) = part_bodies();
    let svc = make_service(&state);
    let _ = create_and_upload_two_parts(&svc, "b", "vis/obj.bin", p1, p2).await;
    assert_eq!(mpu_record_keys(&state).len(), 2, "records exist on backend");

    let list = svc
        .list_objects_v2(req(
            ListObjectsV2Input {
                bucket: "b".into(),
                ..Default::default()
            },
            http::Method::GET,
            "/b?list-type=2",
        ))
        .await
        .expect("list");
    let keys: Vec<String> = list
        .output
        .contents
        .unwrap_or_default()
        .into_iter()
        .filter_map(|o| o.key)
        .collect();
    assert!(
        keys.iter().all(|k| !mpu_durable::is_mpu_state_key(k)),
        "listing must hide .s4mpu/ records: {keys:?}"
    );

    // Client PUT into the reserved namespace is rejected.
    let err = svc
        .put_object(req(
            PutObjectInput {
                bucket: "b".into(),
                key: ".s4mpu/deadbeef/1".into(),
                body: Some(bytes_to_blob(Bytes::from_static(b"forged"))),
                ..Default::default()
            },
            http::Method::PUT,
            "/b/.s4mpu/deadbeef/1",
        ))
        .await
        .expect_err("client write into .s4mpu/ must be rejected");
    assert!(
        err.message().unwrap_or_default().contains("reserved"),
        "err: {err:?}"
    );
}

/// Stale-state guard (2026-07-06 review finding): a durable record whose
/// `backend_etag` no longer matches the authoritative ListParts ETag for
/// that part (a delayed `UploadPart`'s state write landing AFTER a
/// re-upload of the same part number) must be IGNORED — completing with
/// it would stamp a composite for bytes the backend no longer holds.
/// The part degrades to the unrecorded fallback: Complete succeeds
/// (reverse-mapped to the ListParts ETag) but nothing is stamped, and
/// GET returns the real bytes with no logical ETag.
#[tokio::test]
async fn stale_durable_record_is_ignored_not_stamped() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let (p1, p2) = part_bodies();

    let svc_a = make_service(&state);
    let (upload_id, _etag1, etag2) =
        create_and_upload_two_parts(&svc_a, "b", "stale/obj.bin", p1.clone(), p2.clone()).await;

    // Simulate the delayed writer: overwrite part 1's durable record with
    // state from an EARLIER upload attempt — an original_md5 of different
    // bytes and a backend_etag that no longer matches what ListParts
    // reports for the part actually stored.
    let stale_md5 = md5_hex(b"the earlier, superseded part-1 bytes");
    let stale = mpu_durable::DurablePartRecord {
        v: mpu_durable::DurablePartRecord::VERSION,
        upload_id: upload_id.clone(),
        part_number: 1,
        original_md5: stale_md5.clone(),
        backend_etag: "d41d8cd98f00b204e9800998ecf8427e".to_owned(), // not the stored part
        key: "stale/obj.bin".to_owned(),
    };
    state.lock().unwrap().objects.insert(
        ("b".to_owned(), mpu_durable::record_key(&upload_id, 1)),
        StoredObject {
            body: Bytes::from(stale.encode()),
            metadata: None,
            content_type: None,
        },
    );

    // A FRESH instance (no in-memory state) completes with a manifest
    // citing the STALE part-1 ETag — exactly what the delayed client
    // would submit.
    let svc_b = make_service(&state);
    let complete = svc_b
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "stale/obj.bin",
            &upload_id,
            vec![(1, stale_md5.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect("Complete must still succeed via the unrecorded fallback");
    assert!(
        complete.output.e_tag.is_none(),
        "a composite computed from a stale record must NOT be stamped; got {:?}",
        complete.output.e_tag
    );

    // HEAD reflects the unstamped state, and GET returns the REAL bytes.
    let head = svc_b
        .head_object(head_req("b", "stale/obj.bin"))
        .await
        .expect("head");
    assert!(
        head.output.e_tag.is_none(),
        "unstamped multipart object must present no ETag; got {:?}",
        head.output.e_tag
    );
    let got = svc_b
        .get_object(get_req("b", "stale/obj.bin"))
        .await
        .expect("get");
    let body = collect_blob(got.output.body.expect("body"), MAX_BODY)
        .await
        .expect("collect");
    let mut expected_bytes = p1.to_vec();
    expected_bytes.extend_from_slice(&p2);
    assert_eq!(
        md5_hex(&body),
        md5_hex(&expected_bytes),
        "GET must return the bytes the backend actually holds"
    );
}

/// #144: HEAD of a multipart object must present the ORIGINAL
/// (decompressed) ContentLength, not the stored compressed size —
/// GET and Range GET already report original sizes, and HEAD was the
/// odd one out (validated live on R2 + MinIO, 2026-07-06).
#[tokio::test]
async fn multipart_head_reports_original_content_length() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let (p1, p2) = part_bodies();
    let original_total = (p1.len() + p2.len()) as i64;

    let svc = make_service(&state);
    let (upload_id, etag1, etag2) =
        create_and_upload_two_parts(&svc, "b", "headlen/obj.bin", p1, p2).await;
    svc.complete_multipart_upload(complete_mpu_req(
        "b",
        "headlen/obj.bin",
        &upload_id,
        vec![(1, etag1.as_str()), (2, etag2.as_str())],
    ))
    .await
    .expect("complete");

    let head = svc
        .head_object(head_req("b", "headlen/obj.bin"))
        .await
        .expect("head");
    assert_eq!(
        head.output.content_length,
        Some(original_total),
        "HEAD must report the original size (stored compressed size leaked?)"
    );

    // Pre-v1.5 objects have no `s4-original-size` stamp — strip it from the
    // backend metadata and HEAD again: the sidecar fallback must still
    // resolve the original size.
    {
        let mut st = state.lock().unwrap();
        let obj = st
            .objects
            .get_mut(&("b".to_owned(), "headlen/obj.bin".to_owned()))
            .expect("object on backend");
        let md = obj.metadata.as_mut().expect("stamped metadata");
        md.remove("s4-original-size")
            .expect("stamp must have been written by Complete");
    }
    let head = svc
        .head_object(head_req("b", "headlen/obj.bin"))
        .await
        .expect("head (fallback)");
    assert_eq!(
        head.output.content_length,
        Some(original_total),
        "sidecar fallback must resolve the original size for unstamped multipart objects"
    );
}
