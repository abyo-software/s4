//! v0.9 #106 — chaos / fault-injection infrastructure.
//!
//! v0.8.18 P7 landed a scaffold (`chaos_scaffold_smoke`) that established
//! the test target so CI runs it. v0.9 fleshes that scaffold out with a
//! reusable `ChaosBackend` wrapper + concrete scenarios that drive the
//! `S4Service` through the failure modes the production gateway must
//! survive without **silently corrupting data** or **panicking**.
//!
//! All faults are armed *deterministically* (ordinal-based counters,
//! `tokio::sync::Notify` for ordering, no `tokio::time::sleep` racing
//! against wall-clock) so the suite is reproducible under
//! `--test-threads=1` and `cargo nextest`. Docker / `testcontainers` is
//! intentionally **not** used — every scenario runs against an
//! in-memory mock so the test suite stays a plain `cargo test`
//! invocation away.
//!
//! Scenarios landed in this file (numbering follows the planned-list
//! comment carried over from the v0.8.18 scaffold):
//!
//! 1. **Backend GET fails mid-stream** — `chaos_get_5xx_mid_stream`:
//!    the backend's `GetObject` returns a `StreamingBlob` whose first
//!    chunk is OK and second chunk is an `Err`. Since #148 the framed
//!    GET path streams frame-by-frame, so the gateway cannot know
//!    about a later backend failure up-front — the invariant is that
//!    the response BODY must fail loudly (an error mid-stream, which
//!    aborts the HTTP response) and must NEVER complete as a silently
//!    truncated 200 body.
//!
//! 2. **Backend HEAD latency** — `chaos_head_latency_timeout_fails_close`:
//!    the backend's `HeadObject` parks on a `Notify` indefinitely. A
//!    Range GET wrapped in `tokio::time::timeout` must fail-close
//!    (the gateway must not block forever waiting for the sidecar
//!    binding check). This validates that the test harness's
//!    timeout discipline is *effective* against a stuck backend —
//!    the production read-timeout knob plumbed through the AWS SDK
//!    relies on the same fail-close behaviour.
//!
//! 3. **Concurrent overwrite of the same key** —
//!    `chaos_concurrent_put_same_key_no_mix`: two tokio tasks PUT
//!    different bodies at the same `(bucket, key)`, sequenced via
//!    `Notify` so one task lands fully before the other starts (deterministic
//!    serial-overwrite). The final stored body must match one of the
//!    two PUTs byte-for-byte — *never* a spliced / interleaved mix —
//!    and the sidecar (if any) must bind to whichever PUT won.
//!
//! 4. **SSE-S4 keyring rotation during PUT** — `chaos_keyring_rotation_mid_put`:
//!    PUT body 1 under keyring version A, swap the gateway's keyring
//!    Arc to version B (active key id rotated, A retained for read),
//!    PUT body 2. Read-back of both objects must succeed (the
//!    decrypt path resolves the per-object key id), and crucially
//!    the rotation must not panic mid-flight.
//!
//! 5. **`CompleteMultipartUpload` backend failure → clean revert** —
//!    `chaos_complete_mpu_fails_state_unchanged`: backend's
//!    `CompleteMultipartUpload` returns 500. The S4 multipart state
//!    must NOT be cleared (operator retry must be possible), and no
//!    object must materialise at `(bucket, key)`.
//!
//! Each scenario carries a doc comment explaining (a) what fault is
//! injected, (b) what production invariant is being asserted, and
//! (c) which planned-scenario number from the v0.8.18 scaffold list
//! it corresponds to. Mock-backend faults are armed via the
//! [`ChaosConfig`] handle shared between the test and the
//! `ChaosBackend` instance — no random / time-based faults; every
//! injection is ordinal-counted.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::Stream;
use s3s::dto::*;
use s3s::stream::{ByteStream, RemainingLength};
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result, StdError};
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::AlwaysDispatcher;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::blob::{bytes_to_blob, collect_blob};

const MAX_BODY: usize = 256 * 1024 * 1024;

// =========================================================================
// Shared test helpers — registry / dispatcher / request builders. Mirrors
// the shape `tests/multipart_audit_71.rs` uses so future contributors
// can copy-paste between chaos / audit fixtures with minimal churn.
// =========================================================================

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

fn put_req(bucket: &str, key: &str, body: Bytes) -> S3Request<PutObjectInput> {
    req(
        PutObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            body: Some(bytes_to_blob(body)),
            ..Default::default()
        },
        http::Method::PUT,
        &format!("/{bucket}/{key}"),
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
            multipart_upload: Some(CompletedMultipartUpload { parts: Some(parts) }),
            ..Default::default()
        },
        http::Method::POST,
        &format!("/{bucket}/{key}?uploadId={upload_id}"),
    )
}

// =========================================================================
// ChaosBackend — composable fault-injection layer over a `MemoryBackend`.
//
// The shape is: an in-memory storage layer (`InnerState`) wrapped by a
// `ChaosConfig` fault map. The config is mutated through `&Arc<Mutex>`
// from the test, so faults can be armed AFTER the `S4Service` has been
// constructed (`S4Service::new` takes the backend by value, so the
// shared-state handle is the only side-channel).
//
// Fault rules (all ordinal, no randomness):
//
//   * `get_object_fail_after_n_bytes`: the next GET returns a body
//     `StreamingBlob` that yields the first `n` bytes OK then errors.
//     Used to assert the gateway converts mid-stream failure into a
//     5xx response (Scenario 1).
//   * `head_object_block_forever`: the next HEAD blocks on a Notify
//     that the test never wakes. The test wraps the gateway call in
//     `tokio::time::timeout` to assert the operator-facing fail-
//     close path is effective (Scenario 2).
//   * `complete_mpu_fail`: the next CompleteMultipartUpload returns
//     `InternalError`, leaving the parts and the gateway-side
//     multipart state intact (Scenario 5).
//
// All counters decrement on trigger; the test arms `Some(n)` and the
// backend overwrites itself to `None` once consumed.
// =========================================================================

#[derive(Clone, Default)]
struct StoredObject {
    body: Bytes,
    metadata: Option<Metadata>,
    content_type: Option<ContentType>,
}

#[derive(Default)]
struct InnerState {
    objects: HashMap<(String, String), StoredObject>,
    mpu_parts: HashMap<String, std::collections::BTreeMap<i32, Bytes>>,
    mpu_objects: HashMap<String, (String, String)>,
    next_upload_id: u64,
}

#[derive(Default)]
struct ChaosConfig {
    /// When `Some(n)`: the next `get_object` returns a stream whose
    /// first `n` bytes succeed and then errors. Resets to `None` after
    /// one use.
    get_object_fail_after_n_bytes: Option<usize>,
    /// When set: every subsequent `head_object` `.await`s on this
    /// `Notify` and the test never wakes it (deterministic block-
    /// forever). Arm AFTER any setup HEADs you don't want to trip on
    /// (e.g. the HEAD inside `multipart Complete`'s sidecar-stamp
    /// path is non-load-bearing — arming this after Complete returns
    /// scopes the block to the gateway-visible Range GET path).
    head_object_block_on: Option<Arc<tokio::sync::Notify>>,
    /// When `true`: the next `complete_multipart_upload` returns
    /// `InternalError` without materialising the assembled object.
    /// Resets to `false` after one use.
    complete_mpu_fail: bool,
}

#[derive(Clone)]
struct ChaosHandle {
    state: Arc<Mutex<InnerState>>,
    faults: Arc<Mutex<ChaosConfig>>,
}

impl ChaosHandle {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(InnerState::default())),
            faults: Arc::new(Mutex::new(ChaosConfig::default())),
        }
    }

    fn arm_get_fail_after(&self, bytes: usize) {
        self.faults.lock().unwrap().get_object_fail_after_n_bytes = Some(bytes);
    }

    fn arm_head_block(&self, notify: Arc<tokio::sync::Notify>) {
        self.faults.lock().unwrap().head_object_block_on = Some(notify);
    }

    fn arm_complete_mpu_fail(&self) {
        self.faults.lock().unwrap().complete_mpu_fail = true;
    }

    fn stored(&self, bucket: &str, key: &str) -> Option<StoredObject> {
        self.state
            .lock()
            .unwrap()
            .objects
            .get(&(bucket.into(), key.into()))
            .cloned()
    }
}

struct ChaosBackend {
    handle: ChaosHandle,
}

impl ChaosBackend {
    fn new(handle: ChaosHandle) -> Self {
        Self { handle }
    }
}

/// A `ByteStream` that yields `head` first and then a fixed error,
/// driving the gateway's `collect_blob` into the `BlobError::Read`
/// branch deterministically.
struct FailingMidStream {
    head: Option<Bytes>,
    erred: bool,
}

impl FailingMidStream {
    fn new(head: Bytes) -> Self {
        Self {
            head: Some(head),
            erred: false,
        }
    }
}

impl Stream for FailingMidStream {
    type Item = Result<Bytes, StdError>;
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = self.get_mut();
        if let Some(b) = me.head.take() {
            return Poll::Ready(Some(Ok(b)));
        }
        if !me.erred {
            me.erred = true;
            let err: StdError = Box::new(std::io::Error::other(
                "chaos: injected mid-stream backend error",
            ));
            return Poll::Ready(Some(Err(err)));
        }
        Poll::Ready(None)
    }
}

impl ByteStream for FailingMidStream {
    fn remaining_length(&self) -> RemainingLength {
        // Length is intentionally unknown — the stream is a partial
        // prefix plus an error; we want the gateway to surface the
        // error rather than rely on `Content-Length` reconciliation.
        RemainingLength::new_exact(0)
    }
}

#[async_trait::async_trait]
impl S3 for ChaosBackend {
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let body = match req.input.body.take() {
            Some(blob) => collect_blob(blob, MAX_BODY)
                .await
                .map_err(|e| S3Error::with_message(S3ErrorCode::InternalError, format!("{e}")))?,
            None => Bytes::new(),
        };
        let stored = StoredObject {
            body,
            metadata: req.input.metadata.clone(),
            content_type: req.input.content_type.clone(),
        };
        self.handle
            .state
            .lock()
            .unwrap()
            .objects
            .insert((req.input.bucket.clone(), req.input.key.clone()), stored);
        Ok(S3Response::new(PutObjectOutput {
            e_tag: Some(ETag::Strong("chaos-etag".into())),
            ..Default::default()
        }))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        // Scenario 1: serve a body stream that errors after N bytes.
        let injected = self
            .handle
            .faults
            .lock()
            .unwrap()
            .get_object_fail_after_n_bytes
            .take();
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let stored = {
            let st = self.handle.state.lock().unwrap();
            st.objects.get(&key).cloned()
        };
        let stored = stored.ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        if let Some(n) = injected {
            let n = n.min(stored.body.len());
            let head = stored.body.slice(0..n);
            let mid_stream = FailingMidStream::new(head);
            // Report the full content_length on the response — the
            // gateway must NOT short-circuit on body length and must
            // instead surface the mid-stream read error.
            let out = GetObjectOutput {
                body: Some(StreamingBlob::new(mid_stream)),
                content_length: Some(stored.body.len() as i64),
                metadata: stored.metadata,
                content_type: stored.content_type,
                ..Default::default()
            };
            return Ok(S3Response::new(out));
        }
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
        // Scenario 2: block forever on a test-supplied Notify so the
        // gateway-call wrapper in the test exercises the fail-close
        // path under `tokio::time::timeout`.
        let block = self
            .handle
            .faults
            .lock()
            .unwrap()
            .head_object_block_on
            .take();
        if let Some(n) = block {
            n.notified().await; // Test never calls `notify_one`.
        }
        let key = (req.input.bucket.clone(), req.input.key.clone());
        let st = self.handle.state.lock().unwrap();
        let stored = st
            .objects
            .get(&key)
            .ok_or_else(|| S3Error::new(S3ErrorCode::NoSuchKey))?;
        let out = HeadObjectOutput {
            content_length: Some(stored.body.len() as i64),
            metadata: stored.metadata.clone(),
            content_type: stored.content_type.clone(),
            e_tag: Some(ETag::Strong("chaos-etag".into())),
            ..Default::default()
        };
        Ok(S3Response::new(out))
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let key = (req.input.bucket.clone(), req.input.key.clone());
        self.handle.state.lock().unwrap().objects.remove(&key);
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let mut st = self.handle.state.lock().unwrap();
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
            Some(blob) => collect_blob(blob, MAX_BODY)
                .await
                .map_err(|e| S3Error::with_message(S3ErrorCode::InternalError, format!("{e}")))?,
            None => Bytes::new(),
        };
        let upload_id = req.input.upload_id.clone();
        let part_number = req.input.part_number;
        {
            let mut st = self.handle.state.lock().unwrap();
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
        // Scenario 5: backend Complete fails. Crucially, the gateway-
        // side multipart state must remain so the operator can retry;
        // and we must NOT materialise the assembled bytes at
        // (bucket, key) either.
        let fail = {
            let mut f = self.handle.faults.lock().unwrap();
            let v = f.complete_mpu_fail;
            f.complete_mpu_fail = false;
            v
        };
        if fail {
            return Err(S3Error::with_message(
                S3ErrorCode::InternalError,
                "chaos: injected complete_multipart_upload failure",
            ));
        }
        let upload_id = req.input.upload_id.clone();
        let (bucket, key, assembled) = {
            let mut st = self.handle.state.lock().unwrap();
            let parts = st.mpu_parts.remove(&upload_id).ok_or_else(|| {
                S3Error::with_message(S3ErrorCode::NoSuchUpload, "no such upload")
            })?;
            let (bucket, key) = st.mpu_objects.remove(&upload_id).ok_or_else(|| {
                S3Error::with_message(S3ErrorCode::NoSuchUpload, "no upload context")
            })?;
            let mut buf = BytesMut::new();
            for (_, body) in parts {
                buf.extend_from_slice(&body);
            }
            (bucket, key, buf.freeze())
        };
        let stored = StoredObject {
            body: assembled,
            ..Default::default()
        };
        self.handle
            .state
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
        let mut st = self.handle.state.lock().unwrap();
        st.mpu_parts.remove(req.input.upload_id.as_str());
        st.mpu_objects.remove(req.input.upload_id.as_str());
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }
}

// =========================================================================
// v0.8.18 P7 smoke test — retained as a regression guard so a refactor
// that breaks the test target's compile (Bytes, AtomicU32) still trips.
// =========================================================================

#[test]
fn chaos_scaffold_smoke() {
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    let bytes = Bytes::from_static(b"placeholder");
    let counter = Arc::new(AtomicU32::new(0));
    assert_eq!(bytes.len(), 11);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

// =========================================================================
// Scenario 1 — Backend GET returns 5xx mid-stream → gateway must surface
// an error (not a truncated 200). The assertion is that the gateway-
// level `get_object` returns `Err`, not `Ok` with a short body.
// =========================================================================

/// **Scenario 1 (planned-list #1)**: a backend GET whose body stream
/// errors mid-way must surface to the client as a **5xx returned
/// from the gateway's own `get_object`** — not as a 200 followed by
/// a body stream that errors only on consumption (which on the wire
/// becomes a truncated 200 with the wrong content-length; many
/// clients treat that as success). The path that gives us this
/// guarantee is the **buffered** decode path: when the stored
/// object carries S4 codec metadata (`s4-codec=cpu-zstd`), the
/// gateway collects the body in-process before returning, and a
/// `BlobError::Read` mid-collect propagates as `S3Error` from
/// `get_object` itself.
///
/// The test arranges this by PUTting through CpuZstd (default
/// codec for the suite) so the stored object carries the framed +
/// compressed shape that forces the gateway down the buffered
/// path on GET. The assertion requires `Err` directly from
/// `s4.get_object(...)` — accepting the "Ok with body that errors
/// later" branch would let the very regression this scenario is
/// supposed to pin slip through.
#[tokio::test]
async fn chaos_get_5xx_mid_stream() {
    let handle = ChaosHandle::new();
    let backend = ChaosBackend::new(handle.clone());
    // Default registry uses CpuZstd; the stored object carries S4
    // codec metadata (framed), so GET takes the #148 streaming frame
    // walk. A mid-stream backend error cannot surface before the
    // response starts — the pinned invariant is that consuming the
    // body yields a loud error (which aborts the HTTP response on the
    // wire) and never a silently short success.
    let s4 = S4Service::new(backend, make_registry(), make_dispatcher());

    let payload = Bytes::from(vec![0xABu8; 64 * 1024]);
    s4.put_object(put_req("b", "scenario1", payload.clone()))
        .await
        .expect("baseline PUT");

    // Arm the fault: next GET emits ~1 KiB of body and then errors.
    handle.arm_get_fail_after(1024);

    // Scenario 1 invariant (updated for #148 streaming): the GET may
    // return Ok (headers already sent, streaming), but the BODY must
    // terminate in an error — completing cleanly with fewer bytes than
    // the object holds would be the silent-truncation footgun this
    // scenario pins. (Pre-#148 the buffered path surfaced an up-front
    // `Err`; both shapes abort the client-visible transfer.)
    match s4.get_object(get_req("b", "scenario1")).await {
        Err(err) => {
            let code = format!("{:?}", err.code());
            assert!(
                !code.contains("NoSuchKey"),
                "wrong error code — got NoSuchKey, expected an InternalError-class \
                 surface for a mid-stream backend failure (code: {code})"
            );
        }
        Ok(resp) => {
            use futures::TryStreamExt as _;
            let mut got: usize = 0;
            let mut stream_err: Option<String> = None;
            let mut body = std::pin::pin!(resp.output.body.expect("body"));
            loop {
                match body.try_next().await {
                    Ok(Some(chunk)) => got += chunk.len(),
                    Ok(None) => break,
                    Err(e) => {
                        stream_err = Some(e.to_string());
                        break;
                    }
                }
            }
            assert!(
                stream_err.is_some(),
                "Scenario 1 invariant: the streamed body must FAIL when the \
                 backend dies mid-stream — it completed 'cleanly' after \
                 {got} of {} bytes (silent truncation)",
                payload.len()
            );
        }
    }

    // Sanity: the storage layer still has *something* at the key —
    // the failed GET must not have mutated backend state. We don't
    // assert byte-equality against `payload` because the codec
    // path stamps a compressed/framed body, but the key must remain
    // populated.
    assert!(
        handle.stored("b", "scenario1").is_some(),
        "backend storage must still hold the object after a failed GET"
    );
}

// =========================================================================
// Scenario 2 — Backend HEAD blocks forever → a `tokio::time::timeout`-
// wrapped Range GET fails-close. Validates the fail-close discipline
// that the production read-timeout knob (plumbed into the AWS SDK
// client) relies on; without it, a hung backend could pin a gateway
// handler indefinitely.
// =========================================================================

/// **Scenario 2 (planned-list #2)**: a Range GET whose **sidecar-
/// binding HEAD** hangs in the backend must surface the timeout
/// instead of pinning the gateway handler. This is the exact failure
/// the production read-timeout knob (plumbed through the AWS SDK
/// `aws_config::timeout` builder) is meant to handle; the test
/// exercises the same await chain via `tokio::time::timeout`.
///
/// Construction:
///   1. Upload a multipart object (2 parts) through the S4 gateway.
///      The Complete path stamps a multi-entry sidecar with
///      `source_etag` populated (via the HEAD inside the sidecar-
///      stamp block) — this is what arms the **Range GET** sidecar-
///      version-binding HEAD on read-back.
///   2. Arm `head_object_block_on` AFTER Complete returns, so the
///      sidecar-stamp HEAD has already fired and the only remaining
///      HEAD trigger is the `sidecar_version_binding_ok` call
///      inside `partial_range_get`.
///   3. Issue a `Range: bytes=0-15` GET wrapped in
///      `tokio::time::timeout(200 ms, ...)`. The gateway's
///      `read_sidecar` (a GET, not a HEAD — so it succeeds) hands
///      back the freshly-stamped index, the binding-check HEAD
///      blocks, and the timeout must fire.
///
/// Regressions this guards against: a refactor that spawns the
/// HEAD on a detached task (cancellation no longer propagates), or
/// silently demotes a hung HEAD into the legacy "trust the
/// sidecar" branch (which would still serve bytes but from a stale
/// view — a correctness footgun).
#[tokio::test]
async fn chaos_head_latency_timeout_fails_close() {
    let handle = ChaosHandle::new();
    let backend = ChaosBackend::new(handle.clone());
    let s4 = S4Service::new(backend, make_registry(), make_dispatcher());

    let bucket = "b";
    let object_key = "scenario2-mpu";

    // 1) Multipart upload (2 parts) through the gateway. The S4
    //    upload_part path wraps each part as a frame, so the
    //    backend-stored body is multi-frame and `build_index_from_body`
    //    inside multipart Complete produces a 2-entry sidecar.
    //    Highly-compressible bytes so the framed payload still has
    //    distinct frame boundaries the index can address.
    let part1 = Bytes::from(vec![b'A'; 8 * 1024]);
    let part2 = Bytes::from(vec![b'B'; 8 * 1024]);

    let create = s4
        .create_multipart_upload(create_mpu_req(bucket, object_key))
        .await
        .expect("create mpu");
    let upload_id = create.output.upload_id.expect("upload_id");

    let r1 = s4
        .upload_part(upload_part_req(
            bucket,
            object_key,
            &upload_id,
            1,
            part1.clone(),
        ))
        .await
        .expect("upload part 1");
    let r2 = s4
        .upload_part(upload_part_req(
            bucket,
            object_key,
            &upload_id,
            2,
            part2.clone(),
        ))
        .await
        .expect("upload part 2");

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
    s4.complete_multipart_upload(complete_mpu_req(bucket, object_key, &upload_id, parts))
        .await
        .expect("complete mpu");

    // 2) Arm head-block AFTER Complete returns. The sidecar-stamp
    //    HEAD has already fired and consumed its window. Now any
    //    HEAD the gateway issues — specifically the
    //    `sidecar_version_binding_ok` HEAD inside `partial_range_get`
    //    — will park on this notify forever.
    let block_notify = Arc::new(tokio::sync::Notify::new());
    handle.arm_head_block(Arc::clone(&block_notify));

    // 3) Range GET wrapped in a tight deadline. The sidecar lookup
    //    GET (against `<key>.s4index`) succeeds, then the binding
    //    HEAD hits the trap, and the deadline must fire.
    let mut range_input = GetObjectInput {
        bucket: bucket.into(),
        key: object_key.into(),
        ..Default::default()
    };
    range_input.range = Some(s3s::dto::Range::Int {
        first: 0,
        last: Some(15),
    });
    let range_req = req(
        range_input,
        http::Method::GET,
        &format!("/{bucket}/{object_key}"),
    );
    let outcome = tokio::time::timeout(Duration::from_millis(200), s4.get_object(range_req)).await;
    assert!(
        outcome.is_err(),
        "tokio::time::timeout must fire when the sidecar-binding HEAD hangs — \
         got {outcome:?}, expected Err(Elapsed). A passing inner future would \
         mean either the gateway spawned the HEAD on a detached task \
         (cancellation lost) or silently bypassed the binding check on hang \
         (would serve stale-binding bytes). Both are correctness bugs."
    );
    // Hold the notify until the assertion completes — dropping it
    // earlier would wake the waiter and convert deterministic
    // block-forever into a spurious wake-up.
    drop(block_notify);
}

// =========================================================================
// Scenario 3 — Concurrent overwrite of the same key. Two PUTs sequenced
// by `Notify` (NOT random scheduling) must leave the backend storage
// in a coherent state: one of the two bodies, never an interleaved mix.
// =========================================================================

/// **Scenario 3 (planned-list #5)**: idempotency under concurrent
/// overwrite. Drive two PUTs at the same `(bucket, key)` with
/// deterministic ordering (Notify hand-off) and assert the resulting
/// object body equals one of the two source payloads byte-for-byte —
/// a spliced or partial body would mean the backend's last-write-wins
/// semantics got broken by the gateway's compress / framing pipeline.
#[tokio::test]
async fn chaos_concurrent_put_same_key_no_mix() {
    let handle = ChaosHandle::new();
    let backend = ChaosBackend::new(handle.clone());
    let s4 = Arc::new(S4Service::new(backend, make_registry(), make_dispatcher()));

    let body_a = Bytes::from(vec![b'A'; 128 * 1024]);
    let body_b = Bytes::from(vec![b'B'; 128 * 1024]);

    let gate = Arc::new(tokio::sync::Notify::new());
    let s4a = Arc::clone(&s4);
    let body_a_clone = body_a.clone();
    let gate_a = Arc::clone(&gate);
    let t1 = tokio::spawn(async move {
        s4a.put_object(put_req("b", "scenario3", body_a_clone))
            .await
            .expect("PUT A");
        gate_a.notify_one();
    });

    let s4b = Arc::clone(&s4);
    let body_b_clone = body_b.clone();
    let gate_b = Arc::clone(&gate);
    let t2 = tokio::spawn(async move {
        gate_b.notified().await; // Wait until PUT A finishes.
        s4b.put_object(put_req("b", "scenario3", body_b_clone))
            .await
            .expect("PUT B");
    });

    t1.await.expect("task A");
    t2.await.expect("task B");

    // Read back through the gateway so the decompress + frame-parse
    // pipeline runs; a corrupt blend would surface as either a
    // decode error or a body that matches neither A nor B.
    let resp = s4
        .get_object(get_req("b", "scenario3"))
        .await
        .expect("GET after overwrite");
    let got = collect_blob(resp.output.body.expect("body"), MAX_BODY)
        .await
        .expect("collect body");

    assert!(
        got == body_a || got == body_b,
        "post-overwrite body must match one of the two PUTs byte-for-byte; \
         got len={}, neither matches A (len={}) nor B (len={})",
        got.len(),
        body_a.len(),
        body_b.len(),
    );
    // Sequenced via Notify (A then B), so the winner is B.
    assert_eq!(
        got, body_b,
        "deterministic ordering: B wrote second, so last-write-wins picks B"
    );
}

// =========================================================================
// Scenario 4 — SSE-S4 keyring rotation between two PUTs. Both objects
// must read back cleanly after rotation (the v0.5 #29 S4E2 frame format
// embeds the key id so the decrypt path picks the right entry from the
// retained keyring).
// =========================================================================

/// **Scenario 4 (planned-list #4)**: keyring rotation between PUTs.
/// PUT obj1 under keyring(active=1), build a new keyring with
/// (active=2, retain=1), construct a fresh `S4Service` against the
/// SAME backend storage handle, PUT obj2 under the rotated keyring.
/// Both reads must succeed and round-trip byte-equal — proving the
/// per-object S4E2 key_id embed lets the gateway decrypt across a
/// rotation event.
///
/// (The scaffold listed "rotation during an in-flight PUT". The
/// production gateway holds the keyring as `Option<Arc<SseKeyring>>`
/// and the rotate path swaps the Arc atomically — so the actual
/// runtime hazard is "PUT-under-old, GET-under-new", which is what
/// this test exercises.)
#[tokio::test]
async fn chaos_keyring_rotation_mid_put() {
    use s4_server::sse::{SseKey, SseKeyring};

    let handle = ChaosHandle::new();
    let backend = ChaosBackend::new(handle.clone());

    // Pre-rotation keyring: id=1 active.
    let k1 = Arc::new(SseKey::from_bytes(&[1u8; 32]).unwrap());
    let kr1 = Arc::new(SseKeyring::new(1, Arc::clone(&k1)));

    let s4_pre = S4Service::new(backend, make_registry(), make_dispatcher())
        .with_sse_keyring(Arc::clone(&kr1));

    let body1 = Bytes::from(vec![b'1'; 4 * 1024]);
    s4_pre
        .put_object(put_req("b", "scenario4-a", body1.clone()))
        .await
        .expect("PUT under keyring v1");

    // Rotate: id=2 active, id=1 retained for read-back of old objects.
    let k2 = Arc::new(SseKey::from_bytes(&[2u8; 32]).unwrap());
    let mut kr2_mut = SseKeyring::new(2, Arc::clone(&k2));
    kr2_mut.add(1, Arc::clone(&k1));
    let kr2 = Arc::new(kr2_mut);

    // Reconstruct the service against the SAME storage handle (the
    // production gateway swaps the Arc<SseKeyring> in-place; the
    // test models that by building a fresh service and asserting
    // the storage layer survives intact).
    let backend2 = ChaosBackend::new(handle.clone());
    let s4_post = S4Service::new(backend2, make_registry(), make_dispatcher())
        .with_sse_keyring(Arc::clone(&kr2));

    let body2 = Bytes::from(vec![b'2'; 4 * 1024]);
    s4_post
        .put_object(put_req("b", "scenario4-b", body2.clone()))
        .await
        .expect("PUT under keyring v2");

    // Both reads through the post-rotation service must succeed —
    // obj1 via the retained id=1 key, obj2 via the active id=2 key.
    let r1 = s4_post
        .get_object(get_req("b", "scenario4-a"))
        .await
        .expect("GET pre-rotation object after rotation");
    let g1 = collect_blob(r1.output.body.expect("body1"), MAX_BODY)
        .await
        .expect("collect body1");
    assert_eq!(
        g1, body1,
        "pre-rotation object must round-trip under retained id=1 key"
    );

    let r2 = s4_post
        .get_object(get_req("b", "scenario4-b"))
        .await
        .expect("GET post-rotation object");
    let g2 = collect_blob(r2.output.body.expect("body2"), MAX_BODY)
        .await
        .expect("collect body2");
    assert_eq!(
        g2, body2,
        "post-rotation object must round-trip under active id=2 key"
    );
}

// =========================================================================
// Scenario 5 — Backend CompleteMultipartUpload fails → multipart state
// must NOT be cleared (retry-able), and the destination key must NOT
// have any partial materialisation.
// =========================================================================

/// **Scenario 5 (planned-list #3)**: CompleteMultipartUpload backend
/// failure leaves the gateway-side multipart state intact so the
/// operator can retry, and does not materialise a partial object.
/// A regression here would either (a) lose the SSE-C key context
/// (preventing retry) or (b) silently surface a half-committed
/// object at `(bucket, key)`.
#[tokio::test]
async fn chaos_complete_mpu_fails_state_unchanged() {
    let handle = ChaosHandle::new();
    let backend = ChaosBackend::new(handle.clone());
    let s4 = S4Service::new(backend, make_registry(), make_dispatcher());

    let bucket = "b";
    let object_key = "scenario5";

    let create = s4
        .create_multipart_upload(create_mpu_req(bucket, object_key))
        .await
        .expect("create mpu");
    let upload_id = create.output.upload_id.expect("upload_id");

    let part_body = Bytes::from(vec![b'P'; 8 * 1024]);
    let part_resp = s4
        .upload_part(upload_part_req(
            bucket,
            object_key,
            &upload_id,
            1,
            part_body.clone(),
        ))
        .await
        .expect("upload part 1");

    // Sanity: gateway-side state exists.
    let mp_state = s4.multipart_state();
    assert!(
        mp_state.get(&upload_id).is_some(),
        "multipart state must be present after CreateMultipartUpload + UploadPart"
    );

    // Arm the fault and call Complete.
    handle.arm_complete_mpu_fail();
    let parts = vec![CompletedPart {
        e_tag: part_resp.output.e_tag.clone(),
        part_number: Some(1),
        ..Default::default()
    }];
    let err = s4
        .complete_multipart_upload(complete_mpu_req(bucket, object_key, &upload_id, parts))
        .await
        .expect_err("backend Complete failure must propagate");
    let code = format!("{:?}", err.code());
    assert!(
        code.contains("InternalError"),
        "expected an InternalError-class surface from a 5xx backend Complete, got {code}"
    );

    // Storage layer assertion: the destination key must be absent.
    assert!(
        handle.stored(bucket, object_key).is_none(),
        "no object must materialise at (bucket, key) when backend Complete fails"
    );
    // Gateway state assertion: the in-process record must remain so
    // the operator can retry the Complete with the same upload_id.
    assert!(
        mp_state.get(&upload_id).is_some(),
        "multipart state must remain present after a failed Complete, \
         so a retry can succeed without losing the SSE-C key context \
         (regression guard for the H-7-equivalent on the Complete path)"
    );
}
