//! v0.8.4 #71 audit-finding regression tests.
//!
//! Two findings, both confined to `service::complete_multipart_upload` /
//! `service::abort_multipart_upload`:
//!
//! - **C-1 (CRITICAL — silent SSE plaintext leak):** the assembled-body
//!   GET issued from `complete_multipart_upload` used to swallow any
//!   backend `Err(_) => None`, which silently skipped the SSE re-encrypt
//!   branch and left the multipart object on the backend as plaintext on
//!   SSE-S4 / SSE-C / SSE-KMS configured buckets.
//!
//! - **H-7 (HIGH — abort cleanup order):** the in-process per-upload
//!   state (which holds the SSE-C key bytes inside `Zeroizing`) used to
//!   be cleared *before* the backend abort RPC. A transient backend
//!   failure on Abort would therefore destroy the local state while
//!   leaving the parts on the backend, breaking client retry.
//!
//! Both tests use a fault-injecting in-memory backend so we can drive
//! the failure modes deterministically without the Docker MinIO setup.
//! See `tests/feature_e2e.rs` for the existing Docker-gated multipart
//! coverage; the additions here are explicitly *not* Docker-gated so
//! they run on every plain `cargo test --workspace`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

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

const MAX_BODY: usize = 256 * 1024 * 1024;

fn make_registry() -> Arc<CodecRegistry> {
    Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    )
}

fn make_dispatcher() -> Arc<AlwaysDispatcher> {
    Arc::new(AlwaysDispatcher(CodecKind::CpuZstd))
}

// =========================================================================
// In-memory backend with multipart support + opt-in fault injection.
//
// Storage shape (all behind a single `Arc<Mutex<InnerState>>` so tests
// can both arm faults and inspect post-state through the shared handle
// they grab before constructing the `S4Service` — `S4Service::new`
// wraps the backend in its own private `Arc<B>`, so the only way to
// retain a side-channel handle is to share the inner state explicitly):
//
//   - `objects`: bucket/key → (body, metadata, content_type)
//   - `mpu_parts`: upload_id → ordered map<part_number, body>
//   - `mpu_objects`: upload_id → (bucket, key) so Complete knows where
//     to materialise the assembled body
//   - `fail_get_object_remaining`: counter — when `> 0`, the next N
//     `get_object` calls fail with `fail_get_object_code`.
//   - `fail_abort_multipart_remaining`: counter — when `> 0`, the next
//     N `abort_multipart_upload` calls fail with `InternalError`.
// =========================================================================

#[derive(Clone)]
struct StoredObject {
    body: Bytes,
    metadata: Option<Metadata>,
    content_type: Option<ContentType>,
}

struct InnerState {
    objects: HashMap<(String, String), StoredObject>,
    mpu_parts: HashMap<String, std::collections::BTreeMap<i32, Bytes>>,
    mpu_objects: HashMap<String, (String, String)>,
    next_upload_id: u64,
    fail_get_object_remaining: u32,
    fail_get_object_code: S3ErrorCode,
    fail_abort_multipart_remaining: u32,
}

impl InnerState {
    fn new() -> Self {
        Self {
            objects: HashMap::new(),
            mpu_parts: HashMap::new(),
            mpu_objects: HashMap::new(),
            next_upload_id: 0,
            fail_get_object_remaining: 0,
            fail_get_object_code: S3ErrorCode::InternalError,
            fail_abort_multipart_remaining: 0,
        }
    }
}

struct FaultInjectMemBackend {
    state: Arc<Mutex<InnerState>>,
}

impl FaultInjectMemBackend {
    fn from_shared(state: Arc<Mutex<InnerState>>) -> Self {
        Self { state }
    }
}

fn arm_get_object_failure(state: &Mutex<InnerState>, count: u32, code: S3ErrorCode) {
    let mut s = state.lock().unwrap();
    s.fail_get_object_remaining = count;
    s.fail_get_object_code = code;
}

fn arm_abort_multipart_failure(state: &Mutex<InnerState>, count: u32) {
    state.lock().unwrap().fail_abort_multipart_remaining = count;
}

#[async_trait::async_trait]
impl S3 for FaultInjectMemBackend {
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
        Ok(S3Response::new(PutObjectOutput::default()))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        // Fault injection (decrement-on-trigger) — must run before the
        // real lookup so the test exercises the failure branch even
        // when the object is genuinely present.
        let injected: Option<S3ErrorCode> = {
            let mut st = self.state.lock().unwrap();
            if st.fail_get_object_remaining > 0 {
                st.fail_get_object_remaining -= 1;
                Some(st.fail_get_object_code.clone())
            } else {
                None
            }
        };
        if let Some(code) = injected {
            return Err(S3Error::with_message(
                code,
                "fault-injected get_object failure",
            ));
        }
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let stored = {
            let st = self.state.lock().unwrap();
            st.objects.get(&key).cloned()
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
        let st = self.state.lock().unwrap();
        let stored = st
            .objects
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

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        self.state.lock().unwrap().objects.remove(&key);
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let mut st = self.state.lock().unwrap();
        st.next_upload_id += 1;
        let upload_id = format!("mpu-{}", st.next_upload_id);
        st.mpu_parts.insert(upload_id.clone(), Default::default());
        st.mpu_objects.insert(
            upload_id.clone(),
            (req.input.bucket.clone(), req.input.key.clone()),
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
        let upload_id = req.input.upload_id.clone();
        let part_number = req.input.part_number;
        {
            let mut st = self.state.lock().unwrap();
            let parts = st.mpu_parts.get_mut(&upload_id).ok_or_else(|| {
                S3Error::with_message(S3ErrorCode::NoSuchUpload, "no such upload")
            })?;
            parts.insert(part_number, body);
        }
        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(ETag::Strong(format!("etag-{part_number}"))),
            ..Default::default()
        }))
    }

    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let upload_id = req.input.upload_id.clone();
        let (bucket, key, assembled) = {
            let mut st = self.state.lock().unwrap();
            let parts = st.mpu_parts.remove(&upload_id).ok_or_else(|| {
                S3Error::with_message(S3ErrorCode::NoSuchUpload, "no such upload")
            })?;
            let (bucket, key) =
                st.mpu_objects.remove(&upload_id).ok_or_else(|| {
                    S3Error::with_message(S3ErrorCode::NoSuchUpload, "no upload context")
                })?;
            // Concat parts in part-number order (BTreeMap iter is sorted).
            let mut buf = Vec::new();
            for (_, body) in parts {
                buf.extend_from_slice(&body);
            }
            (bucket, key, Bytes::from(buf))
        };
        // Materialise the object so subsequent get_object / head_object
        // see it. (Real S3 / MinIO does the same — Complete is what
        // makes the assembled bytes visible at `(bucket, key)`.)
        let stored = StoredObject {
            body: assembled,
            metadata: None,
            content_type: None,
        };
        self.state
            .lock()
            .unwrap()
            .objects
            .insert((bucket.clone(), key.clone()), stored);
        Ok(S3Response::new(CompleteMultipartUploadOutput {
            bucket: Some(bucket),
            key: Some(key),
            e_tag: Some(ETag::Strong("complete-etag".into())),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let injected = {
            let mut st = self.state.lock().unwrap();
            if st.fail_abort_multipart_remaining > 0 {
                st.fail_abort_multipart_remaining -= 1;
                true
            } else {
                false
            }
        };
        if injected {
            return Err(S3Error::with_message(
                S3ErrorCode::InternalError,
                "fault-injected abort_multipart_upload failure",
            ));
        }
        let mut st = self.state.lock().unwrap();
        st.mpu_parts.remove(req.input.upload_id.as_str());
        st.mpu_objects.remove(req.input.upload_id.as_str());
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }
}

// =========================================================================
// Request builders.
// =========================================================================

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
        &format!("/{bucket}/{key}?uploadId={upload_id}&partNumber={part_number}"),
    )
}

fn complete_mpu_req(
    bucket: &str,
    key: &str,
    upload_id: &str,
    parts: Vec<CompletedPart>,
) -> S3Request<CompleteMultipartUploadInput> {
    req(
        CompleteMultipartUploadInput {
            bucket: bucket.into(),
            key: key.into(),
            upload_id: upload_id.into(),
            multipart_upload: Some(CompletedMultipartUpload {
                parts: Some(parts),
            }),
            ..Default::default()
        },
        http::Method::POST,
        &format!("/{bucket}/{key}?uploadId={upload_id}"),
    )
}

fn abort_mpu_req(
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> S3Request<AbortMultipartUploadInput> {
    req(
        AbortMultipartUploadInput {
            bucket: bucket.into(),
            key: key.into(),
            upload_id: upload_id.into(),
            ..Default::default()
        },
        http::Method::DELETE,
        &format!("/{bucket}/{key}?uploadId={upload_id}"),
    )
}

// =========================================================================
// C-1 — silent SSE plaintext leak: backend GET failure during multipart
// Complete must FAIL the Complete instead of silently dropping the SSE
// re-encrypt branch.
// =========================================================================

#[tokio::test]
async fn multipart_complete_returns_5xx_when_backend_get_fails() {
    use s4_server::sse::SseKey;

    // Share state up-front so the test can both arm fault injection
    // and inspect the post-state (S4Service::new wraps the backend in
    // its own private Arc<B>; the shared `state` is the only handle
    // we keep on the side).
    let shared_state = Arc::new(Mutex::new(InnerState::new()));
    let backend = FaultInjectMemBackend::from_shared(Arc::clone(&shared_state));

    // SSE-S4 enabled — without the C-1 fix, a get_object failure
    // would skip the encrypt-and-PUT branch and the backend bytes
    // would remain plaintext.
    let key = Arc::new(SseKey::from_bytes(&[7u8; 32]).unwrap());
    let s4 = S4Service::new(backend, make_registry(), make_dispatcher())
        .with_sse_key(Arc::clone(&key));

    let bucket = "sse-bucket";
    let object_key = "leak-canary";

    // 1) Create + upload 2 parts.
    let create = s4
        .create_multipart_upload(create_mpu_req(bucket, object_key))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.clone().expect("upload_id");

    let part1 = Bytes::from_static(b"PART-1-PLAINTEXT-CANARY-XXXXXXXX");
    let part2 = Bytes::from_static(b"PART-2-PLAINTEXT-CANARY-YYYYYYYY");
    let r1 = s4
        .upload_part(upload_part_req(
            bucket,
            object_key,
            &upload_id,
            1,
            part1.clone(),
        ))
        .await
        .expect("part 1");
    let r2 = s4
        .upload_part(upload_part_req(
            bucket,
            object_key,
            &upload_id,
            2,
            part2.clone(),
        ))
        .await
        .expect("part 2");

    let parts = vec![
        CompletedPart {
            e_tag: r1.output.e_tag.clone(),
            part_number: Some(1),
            ..Default::default()
        },
        CompletedPart {
            e_tag: r2.output.e_tag.clone(),
            part_number: Some(2),
            ..Default::default()
        },
    ];

    // 2) Arm the fault: the next get_object (which is the assembled-body
    //    fetch inside complete_multipart_upload) returns a non-NoSuchKey
    //    5xx-style code. The C-1 fix must propagate this as an
    //    InternalError, not silently fall through.
    arm_get_object_failure(&shared_state, 1, S3ErrorCode::InternalError);

    let err = s4
        .complete_multipart_upload(complete_mpu_req(
            bucket,
            object_key,
            &upload_id,
            parts.clone(),
        ))
        .await
        .expect_err("Complete must fail when backend GET fails");
    let code = format!("{:?}", err.code());
    assert!(
        code.contains("InternalError"),
        "expected InternalError surface, got {code}"
    );

    // 3) Diagnostic: the in-memory backend's complete_multipart_upload
    //    mock did materialise the assembled bytes (as real S3 / MinIO
    //    does), so an object exists at `(bucket, key)`. The C-1 fix's
    //    contract is that the gateway returns 5xx so the client treats
    //    Complete as failed — this assertion above is the load-bearing
    //    one. We additionally log the body shape for clarity (a future
    //    regression that "swallows the error AND leaves plaintext in
    //    place" would still be caught by the 5xx assertion above
    //    because the gateway would instead silently return 200).
    let inner = shared_state.lock().unwrap();
    if let Some(stored) = inner
        .objects
        .get(&(bucket.to_string(), object_key.to_string()))
    {
        let mut both_canaries = Vec::new();
        both_canaries.extend_from_slice(&part1);
        both_canaries.extend_from_slice(&part2);
        let is_plaintext_canary = stored.body.as_ref() == both_canaries.as_slice();
        eprintln!(
            "post-fault backend body present (len={}, plaintext_canary={})",
            stored.body.len(),
            is_plaintext_canary
        );
    }
}

/// Companion test: a `NoSuchKey` from the backend GET is treated as the
/// safe race-condition path (object truly missing post-Complete), not
/// as a failure. The fix must let the upload Complete proceed (no
/// SSE re-encrypt is possible because there is nothing to encrypt).
#[tokio::test]
async fn multipart_complete_tolerates_backend_get_no_such_key() {
    use s4_server::sse::SseKey;

    let shared_state = Arc::new(Mutex::new(InnerState::new()));
    let backend = FaultInjectMemBackend::from_shared(Arc::clone(&shared_state));
    let key = Arc::new(SseKey::from_bytes(&[7u8; 32]).unwrap());
    let s4 = S4Service::new(backend, make_registry(), make_dispatcher())
        .with_sse_key(Arc::clone(&key));

    let bucket = "sse-bucket";
    let object_key = "raced-with-delete";
    let create = s4
        .create_multipart_upload(create_mpu_req(bucket, object_key))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.expect("upload_id");

    let body = Bytes::from_static(b"body");
    let r1 = s4
        .upload_part(upload_part_req(
            bucket, object_key, &upload_id, 1, body,
        ))
        .await
        .expect("part 1");

    arm_get_object_failure(&shared_state, 1, S3ErrorCode::NoSuchKey);

    let parts = vec![CompletedPart {
        e_tag: r1.output.e_tag.clone(),
        part_number: Some(1),
        ..Default::default()
    }];
    // NoSuchKey = race condition; Complete should still succeed (we
    // can't re-encrypt what isn't there, and there is nothing the
    // gateway stamped as durable that needs SSE marker reconciliation).
    let _ = s4
        .complete_multipart_upload(complete_mpu_req(
            bucket,
            object_key,
            &upload_id,
            parts,
        ))
        .await
        .expect("NoSuchKey on assembled-body fetch is safe to skip");
}

// =========================================================================
// H-7 — abort cleanup order: a failed backend abort must NOT wipe the
// in-process per-upload state, so the client retry can reuse the SSE-C
// key context.
// =========================================================================

#[tokio::test]
async fn abort_multipart_failure_keeps_state_for_retry() {
    let shared_state = Arc::new(Mutex::new(InnerState::new()));
    let backend = FaultInjectMemBackend::from_shared(Arc::clone(&shared_state));
    let s4 = S4Service::new(backend, make_registry(), make_dispatcher());

    let bucket = "abort-bucket";
    let object_key = "to-be-aborted";

    // Establish per-upload state.
    let create = s4
        .create_multipart_upload(create_mpu_req(bucket, object_key))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.expect("upload_id");

    let body = Bytes::from_static(b"part-1");
    let _ = s4
        .upload_part(upload_part_req(
            bucket, object_key, &upload_id, 1, body,
        ))
        .await
        .expect("part 1");

    // Sanity: in-process state exists.
    let mp_state = s4.multipart_state();
    assert!(
        mp_state.get(&upload_id).is_some(),
        "state must be present after Create"
    );

    // Arm one Abort failure. The H-7 fix calls backend.abort FIRST
    // and only removes local state on success — so a backend failure
    // must leave the in-process state intact for retry.
    arm_abort_multipart_failure(&shared_state, 1);

    let err = s4
        .abort_multipart_upload(abort_mpu_req(bucket, object_key, &upload_id))
        .await
        .expect_err("first Abort must fail (fault-injected)");
    let code = format!("{:?}", err.code());
    assert!(
        code.contains("InternalError"),
        "expected fault-injected InternalError, got {code}"
    );
    assert!(
        mp_state.get(&upload_id).is_some(),
        "state must STILL be present after a failed Abort (H-7 fix)"
    );

    // Retry — fault is now disarmed (counter exhausted), backend Abort
    // succeeds, and only NOW does the in-process state get cleared.
    s4.abort_multipart_upload(abort_mpu_req(bucket, object_key, &upload_id))
        .await
        .expect("retry Abort succeeds");
    assert!(
        mp_state.get(&upload_id).is_none(),
        "state must be cleared on a successful Abort"
    );
}
