//! #150 regression: an interrupted CompleteMultipartUpload must be
//! idempotently retryable — no index-less phantom left at the client key
//! with the retry answering `NoSuchUpload`.
//!
//! Live repro (2026-07-08 Metered Savings E2E): the gateway's Complete
//! runs backend-Complete → drop in-memory + durable `.s4mpu/` state →
//! full-body GET → logical-ETag stamp → `.s4index` PUT → response. A
//! connection kill inside that window (easy under the 30 s
//! whole-connection cap, #149) left the base object committed but
//! unindexed, with all salvage state already destroyed — the client
//! retry re-ran backend Complete against a consumed upload-id and got
//! `NoSuchUpload`. 4 of 6 interrupted 2 GiB uploads left 160 MiB
//! padding phantoms at the client keys.
//!
//! The fault injection here reproduces the kill deterministically: the
//! backend hangs the first post-Complete GET of the main object, the
//! test cancels the Complete future (exactly what hyper does when the
//! connection dies), then retries through a FRESH gateway instance.
//!
//! Harness: in-memory multipart backend, same family as
//! `tests/multipart_durable_state.rs`. No Docker.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
// In-memory multipart backend with a "hang the main-object GET" fault:
// arming `hang_main_get` makes any GET of a non-internal key (not
// `.s4mpu/`, not `.s4index`) pend forever — the caller then cancels the
// in-flight Complete future, modelling the connection death mid-handler.
// =========================================================================

#[derive(Clone)]
struct StoredObject {
    body: Bytes,
    metadata: Option<Metadata>,
    content_type: Option<ContentType>,
}

#[derive(Default)]
struct InnerState {
    objects: BTreeMap<(String, String), StoredObject>,
    mpu_parts: HashMap<String, BTreeMap<i32, Bytes>>,
    mpu_meta: HashMap<String, (String, String, Option<Metadata>)>,
    next_upload_id: u64,
}

struct FaultBackend {
    state: Arc<Mutex<InnerState>>,
    hang_main_get: Arc<AtomicBool>,
    /// QA delta-round: error every RANGED main-object GET — the
    /// frame-hop scan's transport-failure case. Distinct from
    /// `hang_main_get` so the transient-failure semantics (recovery
    /// must fail closed with state intact, then succeed once the
    /// backend heals) are testable deterministically.
    fail_ranged_get: Arc<AtomicBool>,
}

impl FaultBackend {
    fn new(state: Arc<Mutex<InnerState>>, hang_main_get: Arc<AtomicBool>) -> Self {
        Self {
            state,
            hang_main_get,
            fail_ranged_get: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with_fail_ranged(mut self, flag: Arc<AtomicBool>) -> Self {
        self.fail_ranged_get = flag;
        self
    }
}

fn is_internal_key(key: &str) -> bool {
    mpu_durable::is_mpu_state_key(key) || key.ends_with(".s4index")
}

/// Slice a stored body according to the request's Range — the frame-hop
/// Complete scan depends on real ranged-GET semantics.
fn apply_range(body: &Bytes, range: &Range) -> Bytes {
    match range {
        Range::Int { first, last } => {
            let start = *first as usize;
            let end = last
                .map(|l| (l as usize + 1).min(body.len()))
                .unwrap_or(body.len());
            body.slice(start.min(body.len())..end.max(start.min(body.len())))
        }
        Range::Suffix { length } => {
            let start = body.len().saturating_sub(*length as usize);
            body.slice(start..)
        }
    }
}

#[async_trait::async_trait]
impl S3 for FaultBackend {
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
        if self.hang_main_get.load(Ordering::SeqCst) && !is_internal_key(&req.input.key) {
            // The connection died mid-transfer: the handler future is
            // about to be cancelled by the test.
            std::future::pending::<()>().await;
        }
        if self.fail_ranged_get.load(Ordering::SeqCst)
            && !is_internal_key(&req.input.key)
            && req.input.range.is_some()
        {
            return Err(S3Error::with_message(
                S3ErrorCode::InternalError,
                "injected transient ranged-GET failure",
            ));
        }
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let stored = {
            let st = self.state.lock().unwrap();
            st.objects.get(&key).cloned()
        };
        let stored = stored.ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        let etag = md5_hex(&stored.body);
        let body = match req.input.range.as_ref() {
            Some(r) => apply_range(&stored.body, r),
            None => stored.body.clone(),
        };
        let len = body.len() as i64;
        Ok(S3Response::new(GetObjectOutput {
            body: Some(bytes_to_blob(body)),
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
            let (bucket, key, _meta) = st
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
// Harness
// =========================================================================

fn make_service(state: &Arc<Mutex<InnerState>>, hang: &Arc<AtomicBool>) -> S4Service<FaultBackend> {
    S4Service::new(
        FaultBackend::new(Arc::clone(state), Arc::clone(hang)),
        make_registry(),
        Arc::new(AlwaysDispatcher(CodecKind::CpuZstd)),
    )
}

fn make_service_with_ranged_fail(
    state: &Arc<Mutex<InnerState>>,
    hang: &Arc<AtomicBool>,
    fail_ranged: &Arc<AtomicBool>,
) -> S4Service<FaultBackend> {
    S4Service::new(
        FaultBackend::new(Arc::clone(state), Arc::clone(hang))
            .with_fail_ranged(Arc::clone(fail_ranged)),
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

fn part_bodies() -> (Bytes, Bytes) {
    let p1 = Bytes::from(vec![b'a'; 5 * 1024 * 1024 + 137]);
    let p2 = Bytes::from(vec![b'b'; 64 * 1024]);
    (p1, p2)
}

fn sidecar_present(state: &Arc<Mutex<InnerState>>, bucket: &str, key: &str) -> bool {
    state
        .lock()
        .unwrap()
        .objects
        .contains_key(&(bucket.to_owned(), format!("{key}.s4index")))
}

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

/// Drive an upload to the point where the FIRST Complete is interrupted
/// mid-handler (cancelled after the backend Complete committed, before
/// the gateway's post-processing finished). Returns everything a retry
/// needs.
async fn interrupted_complete(
    state: &Arc<Mutex<InnerState>>,
    hang: &Arc<AtomicBool>,
    bucket: &str,
    key: &str,
) -> (String, String, String) {
    let (p1, p2) = part_bodies();
    let svc_a = make_service(state, hang);
    let create = svc_a
        .create_multipart_upload(create_mpu_req(bucket, key))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.expect("upload id");
    let etag1 = svc_a
        .upload_part(upload_part_req(bucket, key, &upload_id, 1, p1))
        .await
        .expect("part 1")
        .output
        .e_tag
        .expect("etag1")
        .into_value();
    let etag2 = svc_a
        .upload_part(upload_part_req(bucket, key, &upload_id, 2, p2))
        .await
        .expect("part 2")
        .output
        .e_tag
        .expect("etag2")
        .into_value();

    // Arm the fault: the post-Complete fetch of the main object hangs,
    // and the test cancels the whole handler — the connection died.
    hang.store(true, Ordering::SeqCst);
    let interrupted = tokio::time::timeout(
        Duration::from_millis(500),
        svc_a.complete_multipart_upload(complete_mpu_req(
            bucket,
            key,
            &upload_id,
            vec![(1, etag1.as_str()), (2, etag2.as_str())],
        )),
    )
    .await;
    assert!(
        interrupted.is_err(),
        "the fault must interrupt the first Complete mid-handler"
    );
    hang.store(false, Ordering::SeqCst);

    // The backend Complete committed: the base object exists…
    assert!(
        state
            .lock()
            .unwrap()
            .objects
            .contains_key(&(bucket.to_owned(), key.to_owned())),
        "backend Complete must have committed the base object"
    );
    // …but the gateway never got to the sidecar — the phantom state.
    assert!(
        !sidecar_present(state, bucket, key),
        "interrupted Complete must not have written the sidecar yet"
    );

    (upload_id, etag1, etag2)
}

// =========================================================================
// Tests
// =========================================================================

/// #150 core: the client's retried Complete (fresh gateway instance —
/// crash/LB case) must succeed idempotently: composite ETag returned,
/// `.s4index` written, stamp present, bytes intact, durable state
/// reaped. Pre-fix: the retry gets `NoSuchUpload` and the phantom stays.
#[tokio::test]
async fn interrupted_complete_retry_succeeds_idempotently() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let hang = Arc::new(AtomicBool::new(false));
    let (p1, p2) = part_bodies();
    let expected = expected_composite(&[&p1, &p2]);

    let (upload_id, etag1, etag2) = interrupted_complete(&state, &hang, "b", "idem/obj.bin").await;

    // Retry through a FRESH instance (the crashed gateway is gone).
    let svc_b = make_service(&state, &hang);
    let resp = svc_b
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "idem/obj.bin",
            &upload_id,
            vec![(1, etag1.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect("retried Complete for an already-committed upload must succeed");
    assert_eq!(
        resp.output.e_tag.expect("composite").into_value(),
        expected,
        "retry must return the client-transparent composite"
    );

    // The object is fully coherent: sidecar + stamp + bytes.
    assert!(
        sidecar_present(&state, "b", "idem/obj.bin"),
        "retry must (re)write the .s4index sidecar"
    );
    let head = svc_b
        .head_object(head_req("b", "idem/obj.bin"))
        .await
        .expect("head");
    assert_eq!(
        head.output.e_tag.expect("head etag").into_value(),
        expected,
        "HEAD must echo the stamped composite after the retry"
    );
    assert_eq!(
        head.output.content_length,
        Some((p1.len() + p2.len()) as i64),
        "HEAD must report the original size after the retry"
    );
    let got = svc_b
        .get_object(get_req("b", "idem/obj.bin"))
        .await
        .expect("get");
    let body = collect_blob(got.output.body.expect("body"), MAX_BODY)
        .await
        .expect("collect");
    let mut want = p1.to_vec();
    want.extend_from_slice(&p2);
    assert_eq!(md5_hex(&body), md5_hex(&want), "bytes must round-trip");

    // All durable state (part records + completion marker) reaped.
    assert!(
        mpu_record_keys(&state).is_empty(),
        "successful retry must reap all .s4mpu/ state: {:?}",
        mpu_record_keys(&state)
    );
}

/// Guard: the idempotent-recovery path must NOT blindly bless any retry
/// — a retry whose manifest doesn't match what was committed (tampered
/// part ETag) must still be rejected.
#[tokio::test]
async fn interrupted_complete_retry_with_wrong_manifest_rejected() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let hang = Arc::new(AtomicBool::new(false));

    let (upload_id, _etag1, etag2) =
        interrupted_complete(&state, &hang, "b", "wrong/obj.bin").await;

    let svc_b = make_service(&state, &hang);
    let wrong = "f".repeat(32);
    let err = svc_b
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "wrong/obj.bin",
            &upload_id,
            vec![(1, wrong.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect_err("a retry with a tampered manifest must not be blessed");
    assert!(
        matches!(
            *err.code(),
            S3ErrorCode::NoSuchUpload | S3ErrorCode::InvalidPart
        ),
        "err: {err:?}"
    );
}

/// Guard: after a FULLY completed upload (post-processing + cleanup all
/// done), a duplicate Complete for the consumed upload-id keeps
/// answering `NoSuchUpload` — recovery must not fabricate success once
/// the durable state is gone.
#[tokio::test]
async fn duplicate_complete_after_full_success_still_no_such_upload() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let hang = Arc::new(AtomicBool::new(false));
    let (p1, p2) = part_bodies();

    let svc = make_service(&state, &hang);
    let create = svc
        .create_multipart_upload(create_mpu_req("b", "dup/obj.bin"))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.expect("upload id");
    let etag1 = svc
        .upload_part(upload_part_req("b", "dup/obj.bin", &upload_id, 1, p1))
        .await
        .expect("part 1")
        .output
        .e_tag
        .expect("etag1")
        .into_value();
    let etag2 = svc
        .upload_part(upload_part_req("b", "dup/obj.bin", &upload_id, 2, p2))
        .await
        .expect("part 2")
        .output
        .e_tag
        .expect("etag2")
        .into_value();
    let manifest = vec![(1, etag1.as_str()), (2, etag2.as_str())];
    svc.complete_multipart_upload(complete_mpu_req(
        "b",
        "dup/obj.bin",
        &upload_id,
        manifest.clone(),
    ))
    .await
    .expect("first Complete");
    assert!(
        mpu_record_keys(&state).is_empty(),
        "state fully reaped after the successful Complete"
    );

    let err = svc
        .complete_multipart_upload(complete_mpu_req("b", "dup/obj.bin", &upload_id, manifest))
        .await
        .expect_err("duplicate Complete after full success stays an error");
    assert_eq!(*err.code(), S3ErrorCode::NoSuchUpload, "err: {err:?}");
}

/// QA delta-round (frame-hop scan): a TRANSIENT ranged-GET failure
/// during recovery must fail the retry with all durable state intact —
/// finalizing would reap the completion record and stamp the stored
/// (compressed) length as the original size. Once the backend heals,
/// the next retry recovers fully.
#[tokio::test]
async fn recovery_survives_transient_ranged_get_failure() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let hang = Arc::new(AtomicBool::new(false));
    let fail_ranged = Arc::new(AtomicBool::new(false));
    let (p1, p2) = part_bodies();
    let expected = expected_composite(&[&p1, &p2]);

    let (upload_id, etag1, etag2) = interrupted_complete(&state, &hang, "b", "flaky/obj.bin").await;

    // Retry through an instance whose ranged GETs fail (transient
    // backend trouble): the recovery must ERROR, not fabricate success.
    let svc_flaky = make_service_with_ranged_fail(&state, &hang, &fail_ranged);
    fail_ranged.store(true, Ordering::SeqCst);
    let err = svc_flaky
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "flaky/obj.bin",
            &upload_id,
            vec![(1, etag1.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect_err("recovery with failing hop reads must not fabricate success");
    assert_eq!(*err.code(), S3ErrorCode::InternalError, "err: {err:?}");
    // State intact: completion record + part records survive, no
    // sidecar, no premature stamp.
    let keys = mpu_record_keys(&state);
    assert_eq!(
        keys.len(),
        3,
        "durable state must survive the failed recovery: {keys:?}"
    );
    assert!(
        !sidecar_present(&state, "b", "flaky/obj.bin"),
        "no sidecar may be written by a failed recovery"
    );

    // Backend heals → the next retry recovers fully.
    fail_ranged.store(false, Ordering::SeqCst);
    let resp = svc_flaky
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "flaky/obj.bin",
            &upload_id,
            vec![(1, etag1.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect("retry after the backend heals must recover");
    assert_eq!(resp.output.e_tag.expect("composite").into_value(), expected);
    assert!(sidecar_present(&state, "b", "flaky/obj.bin"));
    assert!(mpu_record_keys(&state).is_empty(), "state reaped");
}

/// QA round-6 Medium: when NO completion record can exist
/// (`--no-durable-multipart-state`), a transient hop-scan failure must
/// NOT fail the Complete — the retry could never be answered (backend
/// NoSuchUpload, recovery disabled), stranding the client against a
/// committed upload. The Complete degrades: 200 with post-processing
/// skipped (no stamp, no sidecar).
#[tokio::test]
async fn no_durable_state_scan_failure_degrades_instead_of_stranding() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let hang = Arc::new(AtomicBool::new(false));
    let fail_ranged = Arc::new(AtomicBool::new(true));
    let (p1, p2) = part_bodies();

    let svc = make_service_with_ranged_fail(&state, &hang, &fail_ranged)
        .with_durable_multipart_state(false);
    let create = svc
        .create_multipart_upload(create_mpu_req("b", "degrade/obj.bin"))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.expect("upload id");
    let etag1 = svc
        .upload_part(upload_part_req("b", "degrade/obj.bin", &upload_id, 1, p1))
        .await
        .expect("part 1")
        .output
        .e_tag
        .expect("etag1")
        .into_value();
    let etag2 = svc
        .upload_part(upload_part_req("b", "degrade/obj.bin", &upload_id, 2, p2))
        .await
        .expect("part 2")
        .output
        .e_tag
        .expect("etag2")
        .into_value();

    let resp = svc
        .complete_multipart_upload(complete_mpu_req(
            "b",
            "degrade/obj.bin",
            &upload_id,
            vec![(1, etag1.as_str()), (2, etag2.as_str())],
        ))
        .await
        .expect("scan failure without a completion record must degrade, not error");
    // QA round-7: a Failed scan may BE the If-Match concurrent-
    // overwrite signal, so the degrade path must not mutate the key at
    // all — including the composite self-copy stamp (it re-HEADs
    // whatever lives at the key NOW and would write the old upload's
    // composite onto a new generation). Unstamped multipart presents
    // no ETag (the documented >5 GiB shape).
    assert!(
        resp.output.e_tag.is_none(),
        "degraded Complete must not stamp/echo a composite"
    );
    assert!(
        !sidecar_present(&state, "b", "degrade/obj.bin"),
        "degraded Complete must not write a sidecar"
    );
    // The committed object is intact and readable once the backend
    // heals (GET streams; frame parse happens client-side of the scan).
    fail_ranged.store(false, Ordering::SeqCst);
    let got = svc
        .get_object(get_req("b", "degrade/obj.bin"))
        .await
        .expect("committed object must be readable");
    let body = collect_blob(got.output.body.expect("body"), MAX_BODY)
        .await
        .expect("collect");
    assert_eq!(body.len(), 5 * 1024 * 1024 + 137 + 64 * 1024);
}
