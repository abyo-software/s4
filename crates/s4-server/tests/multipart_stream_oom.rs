//! #148 regression: multipart GET / CompleteMultipartUpload must not
//! materialize the whole object in memory.
//!
//! Live repro (2026-07-08 Metered Savings E2E, EKS chart defaults, 2Gi
//! limit): a single `aws s3 cp` of a 2 GiB / 32-part object OOM-killed
//! the gateway (exit 137) because the GET path `collect_blob`s the full
//! compressed body and `decompress_multipart` accumulates every
//! decompressed frame into one `BytesMut`. The only bound is
//! `--max-body-bytes` (default 5 GiB) — far above container limits.
//! The same full-object buffering lives inside Complete (the
//! post-Complete assembled-body fetch that builds the `.s4index`).
//!
//! These tests pin the fix by inverting the failure: a gateway whose
//! `max_body_bytes` is SMALLER than the assembled object must still
//! serve full GETs, Range GETs, and Complete — possible only if those
//! paths stream frame-by-frame instead of collecting the body. (An RSS
//! assertion would be flaky; the cap is the deterministic proxy.)
//!
//! Harness: same in-memory multipart backend family as
//! `tests/multipart_durable_state.rs` — no Docker, runs on plain
//! `cargo test`.

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

/// Test-side collect cap (generous — only bounds the TEST's own reads).
const TEST_COLLECT_MAX: usize = 256 * 1024 * 1024;
/// The gateway cap under test: smaller than the assembled object AND its
/// original size, larger than any single part / frame.
const SMALL_CAP: usize = 8 * 1024 * 1024;

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
// In-memory multipart backend (same shape as multipart_durable_state.rs).
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
    /// Ranged GETs served (lets tests assert the sidecar partial-fetch
    /// path was actually taken vs the full-body fallback).
    ranged_gets: u64,
}

struct MemBackend {
    state: Arc<Mutex<InnerState>>,
}

impl MemBackend {
    fn from_shared(state: Arc<Mutex<InnerState>>) -> Self {
        Self { state }
    }
}

/// Slice a stored body according to the request's Range (the real
/// backend behaviour the sidecar partial-fetch path depends on).
fn apply_range(body: &Bytes, range: &Range) -> Bytes {
    match range {
        Range::Int { first, last } => {
            let start = *first as usize;
            let end = last
                .map(|l| (l as usize + 1).min(body.len()))
                .unwrap_or(body.len());
            body.slice(start..end)
        }
        Range::Suffix { length } => {
            let start = body.len().saturating_sub(*length as usize);
            body.slice(start..)
        }
    }
}

#[async_trait::async_trait]
impl S3 for MemBackend {
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let body = match req.input.body.take() {
            Some(blob) => collect_blob(blob, TEST_COLLECT_MAX).await.map_err(|e| {
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
        let etag = md5_hex(&stored.body);
        let body = match req.input.range.as_ref() {
            Some(r) => {
                self.state.lock().unwrap().ranged_gets += 1;
                apply_range(&stored.body, r)
            }
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
            Some(blob) => collect_blob(blob, TEST_COLLECT_MAX).await.map_err(|e| {
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
// Harness: service construction + request builders.
// =========================================================================

fn make_service(state: &Arc<Mutex<InnerState>>, max_body: usize) -> S4Service<MemBackend> {
    S4Service::new(
        MemBackend::from_shared(Arc::clone(state)),
        make_registry(),
        Arc::new(AlwaysDispatcher(CodecKind::CpuZstd)),
    )
    .with_max_body_bytes(max_body)
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

/// Three 6 MiB compressible non-final parts (padded to the 5 MiB floor
/// each once compressed) plus a small final part. Assembled backend
/// object ≈ 15 MiB, original 18 MiB + 64 KiB — both above `SMALL_CAP`,
/// while every individual part stays below it.
fn oversized_parts() -> Vec<Bytes> {
    vec![
        Bytes::from(vec![b'a'; 6 * 1024 * 1024]),
        Bytes::from(vec![b'b'; 6 * 1024 * 1024]),
        Bytes::from(vec![b'c'; 6 * 1024 * 1024]),
        Bytes::from(vec![b'd'; 64 * 1024]),
    ]
}

async fn upload_all(
    svc: &S4Service<MemBackend>,
    bucket: &str,
    key: &str,
    parts: &[Bytes],
) -> (String, Vec<String>) {
    let create = svc
        .create_multipart_upload(create_mpu_req(bucket, key))
        .await
        .expect("create");
    let upload_id = create.output.upload_id.expect("upload id");
    let mut etags = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let pn = (i + 1) as i32;
        let up = svc
            .upload_part(upload_part_req(bucket, key, &upload_id, pn, part.clone()))
            .await
            .unwrap_or_else(|e| panic!("part {pn}: {e:?}"));
        etags.push(up.output.e_tag.expect("part etag").into_value());
    }
    (upload_id, etags)
}

async fn complete_all(
    svc: &S4Service<MemBackend>,
    bucket: &str,
    key: &str,
    upload_id: &str,
    etags: &[String],
) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
    let manifest: Vec<(i32, &str)> = etags
        .iter()
        .enumerate()
        .map(|(i, e)| ((i + 1) as i32, e.as_str()))
        .collect();
    svc.complete_multipart_upload(complete_mpu_req(bucket, key, upload_id, manifest))
        .await
}

fn concat_parts(parts: &[Bytes]) -> Vec<u8> {
    let mut all = Vec::new();
    for p in parts {
        all.extend_from_slice(p);
    }
    all
}

fn sidecar_present(state: &Arc<Mutex<InnerState>>, bucket: &str, key: &str) -> bool {
    state
        .lock()
        .unwrap()
        .objects
        .contains_key(&(bucket.to_owned(), format!("{key}.s4index")))
}

// =========================================================================
// Tests
// =========================================================================

/// #148 core: a full GET of a multipart object larger than
/// `max_body_bytes` must stream (frame-by-frame) instead of collecting —
/// pre-fix this fails with the collect cap error (the in-memory proxy
/// for the live OOM).
#[tokio::test]
async fn multipart_get_streams_beyond_max_body_bytes() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let parts = oversized_parts();
    let want = concat_parts(&parts);

    // Upload + Complete through a BIG-cap instance (Complete's own
    // de-buffering is pinned separately below).
    let svc_big = make_service(&state, TEST_COLLECT_MAX);
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_ref()).collect();
    let expected_etag = expected_composite(&refs);
    let (upload_id, etags) = upload_all(&svc_big, "b", "oom/get.bin", &parts).await;
    complete_all(&svc_big, "b", "oom/get.bin", &upload_id, &etags)
        .await
        .expect("complete via big-cap instance");

    // GET through a SMALL-cap instance: must stream, not collect.
    let svc_small = make_service(&state, SMALL_CAP);
    let got = svc_small
        .get_object(get_req("b", "oom/get.bin"))
        .await
        .expect("multipart GET must succeed with max_body_bytes below the object size");
    assert_eq!(
        got.output.content_length,
        Some(want.len() as i64),
        "streamed GET must still declare the exact original length"
    );
    assert_eq!(
        got.output.e_tag.expect("logical etag").into_value(),
        expected_etag,
        "streamed GET must echo the stamped composite"
    );
    let body = collect_blob(got.output.body.expect("body"), TEST_COLLECT_MAX)
        .await
        .expect("collect");
    assert_eq!(body.len(), want.len(), "roundtrip length");
    assert_eq!(md5_hex(&body), md5_hex(&want), "roundtrip bytes");
}

/// #148 Complete-side: Complete of an upload whose ASSEMBLED object
/// exceeds `max_body_bytes` must still stamp the composite, write the
/// `.s4index` sidecar, and leave HEAD reporting the original length —
/// pre-fix the assembled-body fetch fails the cap and all
/// post-processing is silently skipped.
#[tokio::test]
async fn multipart_complete_succeeds_beyond_max_body_bytes() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let parts = oversized_parts();
    let want = concat_parts(&parts);
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_ref()).collect();
    let expected_etag = expected_composite(&refs);

    let svc = make_service(&state, SMALL_CAP);
    let (upload_id, etags) = upload_all(&svc, "b", "oom/complete.bin", &parts).await;
    let resp = complete_all(&svc, "b", "oom/complete.bin", &upload_id, &etags)
        .await
        .expect("complete via small-cap instance");
    assert_eq!(
        resp.output.e_tag.expect("composite").into_value(),
        expected_etag,
        "Complete must stamp + return the composite without buffering the body"
    );
    assert!(
        sidecar_present(&state, "b", "oom/complete.bin"),
        ".s4index sidecar must be written without buffering the body"
    );
    let head = svc
        .head_object(head_req("b", "oom/complete.bin"))
        .await
        .expect("head");
    assert_eq!(
        head.output.content_length,
        Some(want.len() as i64),
        "HEAD must report the original size (s4-original-size stamp)"
    );

    // The scan-built sidecar must be byte-accurate: a Range GET spanning
    // the part-1/part-2 frame boundary goes through the partial-fetch
    // path and must return exactly the right bytes.
    let boundary = 6 * 1024 * 1024;
    let mut get = get_req("b", "oom/complete.bin");
    get.input.range = Some(Range::Int {
        first: (boundary - 50) as u64,
        last: Some((boundary + 49) as u64),
    });
    let got = svc.get_object(get).await.expect("range GET via sidecar");
    let body = collect_blob(got.output.body.expect("body"), TEST_COLLECT_MAX)
        .await
        .expect("collect");
    let mut expected_slice = vec![b'a'; 50];
    expected_slice.extend_from_slice(&[b'b'; 50]);
    assert_eq!(body.as_ref(), expected_slice.as_slice());
}

/// #148 sidecar-fast-path side: a Range GET that goes through the
/// `.s4index` partial-fetch path but whose COVERING FRAMES exceed
/// `max_body_bytes` must stream them (pre-fix `partial_range_get`
/// collected the whole covering span and decompressed it into one
/// buffer before slicing — a widest-possible Range re-created the OOM
/// the plain GET had).
#[tokio::test]
async fn multipart_sidecar_range_get_streams_beyond_cap() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let parts = oversized_parts();
    let want = concat_parts(&parts);
    let total = want.len();

    // Complete via a big-cap instance: writes the sidecar.
    let svc_big = make_service(&state, TEST_COLLECT_MAX);
    let (upload_id, etags) = upload_all(&svc_big, "b", "oom/sidecar-range.bin", &parts).await;
    complete_all(&svc_big, "b", "oom/sidecar-range.bin", &upload_id, &etags)
        .await
        .expect("complete");
    assert!(
        sidecar_present(&state, "b", "oom/sidecar-range.bin"),
        "precondition: the sidecar fast path must be armed"
    );

    // Range covering all four parts (minus 100 bytes at each edge):
    // the covering frames span ~15 MiB on the backend — well past the
    // 8 MiB cap the small instance runs with.
    let svc_small = make_service(&state, SMALL_CAP);
    let mut get = get_req("b", "oom/sidecar-range.bin");
    get.input.range = Some(Range::Int {
        first: 100,
        last: Some((total - 101) as u64),
    });
    let got = svc_small
        .get_object(get)
        .await
        .expect("sidecar-path Range GET must stream past max_body_bytes");
    assert!(
        state.lock().unwrap().ranged_gets > 0,
        "the request must have gone through the sidecar partial-fetch path \
         (a ranged backend GET), not the full-body fallback"
    );
    assert_eq!(
        got.output.content_range.as_deref(),
        Some(format!("bytes 100-{}/{}", total - 101, total).as_str())
    );
    let body = collect_blob(got.output.body.expect("body"), TEST_COLLECT_MAX)
        .await
        .expect("collect");
    assert_eq!(body.len(), total - 200, "slice length");
    assert_eq!(
        md5_hex(&body),
        md5_hex(&want[100..total - 100]),
        "slice bytes"
    );
}

/// #148 Range-side: a Range GET of a multipart object with NO usable
/// sidecar (deleted / never written — e.g. a pre-fix phantom) must not
/// materialize the whole object to slice it. Post-fix it streams,
/// skipping non-covering frames.
#[tokio::test]
async fn multipart_range_get_streams_without_sidecar_beyond_cap() {
    let state = Arc::new(Mutex::new(InnerState::default()));
    let parts = oversized_parts();
    let total: usize = parts.iter().map(|p| p.len()).sum();

    let svc_big = make_service(&state, TEST_COLLECT_MAX);
    let (upload_id, etags) = upload_all(&svc_big, "b", "oom/range.bin", &parts).await;
    complete_all(&svc_big, "b", "oom/range.bin", &upload_id, &etags)
        .await
        .expect("complete");

    // Simulate the sidecar-less state (legacy object / interrupted
    // Complete): drop the sidecar the Complete just wrote.
    state
        .lock()
        .unwrap()
        .objects
        .remove(&("b".to_owned(), "oom/range.bin.s4index".to_owned()));

    let svc_small = make_service(&state, SMALL_CAP);
    // Last 100 bytes live in the final ('d') part.
    let mut get = get_req("b", "oom/range.bin");
    get.input.range = Some(Range::Int {
        first: (total - 100) as u64,
        last: Some((total - 1) as u64),
    });
    let got = svc_small
        .get_object(get)
        .await
        .expect("sidecar-less Range GET must stream under the cap");
    assert_eq!(
        got.output.content_range.as_deref(),
        Some(format!("bytes {}-{}/{}", total - 100, total - 1, total).as_str()),
        "ContentRange must be in the logical domain"
    );
    let body = collect_blob(got.output.body.expect("body"), TEST_COLLECT_MAX)
        .await
        .expect("collect");
    assert_eq!(body.as_ref(), vec![b'd'; 100].as_slice());
}
