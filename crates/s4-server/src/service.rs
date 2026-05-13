//! `s3s::S3` 実装 — `s3s_aws::Proxy` への delegation を default にしつつ、
//! `put_object` / `get_object` 経路で `s4_codec::CodecRegistry` を呼ぶ。
//!
//! ## カバー範囲 (Phase 1 月 2)
//!
//! - 圧縮 hook あり: `put_object`, `get_object`
//! - 純 delegation (圧縮なし): `head_bucket`, `list_buckets`, `create_bucket`, `delete_bucket`,
//!   `head_object`, `delete_object`, `delete_objects`, `copy_object`, `list_objects`,
//!   `list_objects_v2`, `create_multipart_upload`, `upload_part`,
//!   `complete_multipart_upload`, `abort_multipart_upload`, `list_multipart_uploads`,
//!   `list_parts`
//! - 未対応 (デフォルトで NotImplemented): その他 80+ ops (Tagging / ACL / Lifecycle 等は Phase 2)
//!
//! ## アーキテクチャ
//!
//! - `S4Service<B>` は backend (B: S3) と `Arc<CodecRegistry>` と `Arc<dyn CodecDispatcher>`
//!   を保持する。`CodecRegistry` 経由で複数 codec を抱えられるので、ひとつの S4 インスタンスが
//!   複数 codec で書かれた object を透過的に GET できる
//! - PUT: dispatcher が body の先頭 sample から codec を選び、registry で compress、
//!   manifest を S3 metadata に書いて backend に forward
//! - GET: backend から取得 → metadata から manifest を復元 → registry.decompress で
//!   manifest 指定の codec で解凍 → 元の bytes を return
//!
//! ## 既知の制限事項
//!
//! - **Multipart Upload は per-part 圧縮が未実装**: 現状は upload_part を素通し。
//!   Phase 1 月 2 後半で per-part compress + complete_multipart_upload で manifest 集約。
//! - **PUT body は memory に collect**: max_body_bytes 上限あり (default 5 GiB = S3 単発 PUT 上限)。
//!   Streaming-aware 圧縮は Phase 2。

use std::sync::Arc;

use base64::Engine as _;
use bytes::BytesMut;
use s3s::dto::*;
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use s4_codec::index::{FrameIndex, build_index_from_body, decode_index, encode_index, sidecar_key};
use s4_codec::multipart::{
    FRAME_HEADER_BYTES, FrameHeader, FrameIter, S3_MULTIPART_MIN_PART_BYTES, pad_to_minimum,
    write_frame,
};
use s4_codec::{ChunkManifest, CodecDispatcher, CodecKind, CodecRegistry};
use std::time::Instant;
use tracing::{debug, info};

use crate::blob::{
    bytes_to_blob, chain_sample_with_rest, collect_blob, collect_with_sample, peek_sample,
};
use crate::streaming::{
    cpu_zstd_decompress_stream, pick_chunk_size, streaming_compress_to_frames,
    supports_streaming_compress, supports_streaming_decompress,
};

/// PUT body の先頭 sampling で渡す最大 byte 数。
const SAMPLE_BYTES: usize = 4096;

/// v0.4 #20: captured at the start of a handler, before the request is
/// consumed by the backend call, so the matching `record_access` at
/// end-of-request can fill in the structured access log entry.
struct AccessLogPreamble {
    remote_ip: Option<String>,
    requester: Option<String>,
    request_uri: String,
    user_agent: Option<String>,
}

pub struct S4Service<B: S3> {
    backend: B,
    registry: Arc<CodecRegistry>,
    dispatcher: Arc<dyn CodecDispatcher>,
    max_body_bytes: usize,
    policy: Option<crate::policy::SharedPolicy>,
    /// v0.3 #13: surfaced as the `aws:SecureTransport` Condition key. Set
    /// to `true` when the listener is wrapped in TLS (or ACME), so policies
    /// gating "deny if not over TLS" can do their job. Defaults to `false`
    /// (HTTP); set via [`S4Service::with_secure_transport`] at boot.
    secure_transport: bool,
    /// v0.4 #19: optional per-(principal, bucket) token-bucket limiter.
    rate_limits: Option<crate::rate_limit::SharedRateLimits>,
    /// v0.4 #20: optional S3-style access log emitter.
    access_log: Option<crate::access_log::SharedAccessLog>,
    /// v0.4 #21 / v0.5 #29: optional server-side encryption keyring
    /// (AES-256-GCM). When set, every PUT body gets wrapped in S4E2
    /// (with the keyring's active key id) after the compress + framing
    /// steps; every GET that sniffs as S4E1/S4E2 is decrypted before
    /// frame parsing. A `with_sse_key(...)` call wraps the supplied
    /// key in a 1-slot keyring so single-key (v0.4) operators get the
    /// same behaviour they had before, just on the v2 frame.
    sse_keyring: Option<crate::sse::SharedSseKeyring>,
    /// v0.5 #34: optional first-class versioning state machine. When
    /// `Some(...)`, S4-server itself owns the per-bucket versioning
    /// state + per-(bucket, key) version chain; PUT / GET / DELETE /
    /// list_object_versions / get_bucket_versioning /
    /// put_bucket_versioning handlers consult the manager instead of
    /// passing through. When `None` (default), the legacy
    /// backend-passthrough behaviour applies so existing v0.4
    /// deployments are unaffected until they explicitly call
    /// `with_versioning(...)`.
    versioning: Option<Arc<crate::versioning::VersioningManager>>,
    /// v0.5 #28: optional SSE-KMS envelope-encryption backend. When
    /// `Some(...)`, PUTs carrying `x-amz-server-side-encryption: aws:kms`
    /// generate a fresh DEK via the backend, encrypt the body with it
    /// (S4E4 frame), and persist only the wrapped DEK. GETs sniffing as
    /// S4E4 unwrap the DEK through the same backend before decrypt.
    /// `kms_default_key_id` is used when the request omits an explicit
    /// `x-amz-server-side-encryption-aws-kms-key-id` (mirrors AWS S3
    /// bucket-default behaviour).
    kms: Option<Arc<dyn crate::kms::KmsBackend>>,
    kms_default_key_id: Option<String>,
    /// v0.5 #30: optional Object Lock (WORM) enforcement layer. When
    /// `Some(...)`, `delete_object` and overwrite-style `put_object`
    /// consult the manager and refuse the operation with HTTP 403
    /// `AccessDenied` while the object is locked (Compliance until
    /// expiry, Governance unless the bypass header is set, or any time
    /// a legal hold is on). PUT also auto-applies the bucket-default
    /// retention to brand-new objects when configured. When `None`
    /// (default), the legacy backend-passthrough behaviour applies, so
    /// existing v0.4 deployments are unaffected until they explicitly
    /// call `with_object_lock(...)`.
    object_lock: Option<Arc<crate::object_lock::ObjectLockManager>>,
    /// v0.5 #32: when `true`, every PUT must carry an SSE indicator
    /// (`x-amz-server-side-encryption`, the SSE-C customer-key headers,
    /// or be matched against a configured server-managed keyring/KMS).
    /// Set by `--compliance-mode strict` after the boot-time
    /// prerequisite check passes.
    compliance_strict: bool,
}

impl<B: S3> S4Service<B> {
    /// AWS S3 単発 PUT の API 上限 (5 GiB)
    pub const DEFAULT_MAX_BODY_BYTES: usize = 5 * 1024 * 1024 * 1024;

    pub fn new(
        backend: B,
        registry: Arc<CodecRegistry>,
        dispatcher: Arc<dyn CodecDispatcher>,
    ) -> Self {
        Self {
            backend,
            registry,
            dispatcher,
            max_body_bytes: Self::DEFAULT_MAX_BODY_BYTES,
            policy: None,
            secure_transport: false,
            rate_limits: None,
            access_log: None,
            sse_keyring: None,
            versioning: None,
            kms: None,
            kms_default_key_id: None,
            object_lock: None,
            compliance_strict: false,
        }
    }

    /// v0.5 #32: enable strict compliance mode. Every PUT must carry an
    /// SSE indicator (server-side encryption header or SSE-C customer
    /// key); requests without one are rejected with 400 InvalidRequest.
    /// Boot-time prerequisite checking lives in the binary
    /// (`validate_compliance_mode`) so this flag is purely the runtime
    /// switch.
    #[must_use]
    pub fn with_compliance_strict(mut self, on: bool) -> Self {
        self.compliance_strict = on;
        self
    }

    /// v0.5 #30: attach the in-memory Object Lock (WORM) enforcement
    /// manager. Once set, `delete_object` and overwrite-path
    /// `put_object` refuse operations on locked keys with HTTP 403
    /// `AccessDenied`; new PUTs to a bucket with a default retention
    /// policy auto-create per-object lock state.
    #[must_use]
    pub fn with_object_lock(
        mut self,
        mgr: Arc<crate::object_lock::ObjectLockManager>,
    ) -> Self {
        self.object_lock = Some(mgr);
        self
    }

    /// v0.5 #28: attach an SSE-KMS backend. `default_key_id` is used
    /// when a PUT requests SSE-KMS without naming a specific KMS key
    /// (operators set this to mirror AWS S3's bucket-default key).
    #[must_use]
    pub fn with_kms_backend(
        mut self,
        kms: Arc<dyn crate::kms::KmsBackend>,
        default_key_id: Option<String>,
    ) -> Self {
        self.kms = Some(kms);
        self.kms_default_key_id = default_key_id;
        self
    }

    /// v0.5 #34: attach the first-class versioning state machine. Once
    /// set, this `S4Service` owns the per-bucket versioning state +
    /// per-(bucket, key) version chain; `put_object` / `get_object` /
    /// `delete_object` / `list_object_versions` /
    /// `get_bucket_versioning` / `put_bucket_versioning` consult the
    /// manager instead of passing through to the backend. The backend
    /// is still used as the byte store: Suspended / Unversioned buckets
    /// keep using `<key>` directly (legacy), Enabled buckets redirect
    /// each version's bytes to a shadow key
    /// (`<key>.__s4ver__/<version-id>`) so older versions survive newer
    /// PUTs to the same logical key.
    #[must_use]
    pub fn with_versioning(mut self, mgr: Arc<crate::versioning::VersioningManager>) -> Self {
        self.versioning = Some(mgr);
        self
    }

    /// v0.4 #21 (kept for back-compat): attach a single SSE-S4 key.
    /// Internally wraps it in a 1-slot keyring with id=1 active, so
    /// new objects ride the v0.5 S4E2 frame while previously-written
    /// S4E1 bytes (this same key) still decrypt via the keyring's S4E1
    /// fallback path. Operators wanting true rotation should call
    /// [`Self::with_sse_keyring`] instead.
    #[must_use]
    pub fn with_sse_key(mut self, key: crate::sse::SharedSseKey) -> Self {
        let keyring = crate::sse::SseKeyring::new(1, key);
        self.sse_keyring = Some(std::sync::Arc::new(keyring));
        self
    }

    /// v0.5 #29: attach a multi-key SSE-S4 keyring. PUT encrypts under
    /// the active key (S4E2 frame stamped with that key's id); GET
    /// dispatches on the body's magic — S4E1 falls back to trying every
    /// key in the ring (active first) so v0.4 objects survive a
    /// migration; S4E2 looks up the explicit key_id from the header.
    #[must_use]
    pub fn with_sse_keyring(mut self, keyring: crate::sse::SharedSseKeyring) -> Self {
        self.sse_keyring = Some(keyring);
        self
    }

    /// v0.4 #20: attach an S3-style access-log emitter. Each completed
    /// PUT / GET / DELETE / List handler emits one entry into the
    /// emitter's buffer; a background flusher (started separately, see
    /// [`crate::access_log::AccessLog::spawn_flusher`]) writes hourly
    /// rotated `.log` files into the configured directory.
    #[must_use]
    pub fn with_access_log(mut self, log: crate::access_log::SharedAccessLog) -> Self {
        self.access_log = Some(log);
        self
    }

    /// Capture the per-request access-log preamble before the request is
    /// consumed by the backend call. Returns `None` if no access logger
    /// is configured (cheap early-out so the handler doesn't pay the
    /// header-clone cost when access logging is off).
    fn access_log_preamble<I>(&self, req: &S3Request<I>) -> Option<AccessLogPreamble> {
        self.access_log.as_ref()?;
        Some(AccessLogPreamble {
            remote_ip: req
                .headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|raw| raw.split(',').next())
                .map(|s| s.trim().to_owned()),
            requester: Self::principal_of(req).map(str::to_owned),
            request_uri: format!("{} {}", req.method, req.uri.path()),
            user_agent: req
                .headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        })
    }

    /// Internal — called by handlers at end-of-request with a captured
    /// preamble. Best-effort: swallows the await fast (clones Arc +
    /// pushes), no error propagation back to the request path.
    #[allow(clippy::too_many_arguments)]
    async fn record_access(
        &self,
        preamble: Option<AccessLogPreamble>,
        operation: &'static str,
        bucket: &str,
        key: Option<&str>,
        http_status: u16,
        bytes_sent: u64,
        object_size: u64,
        total_time_ms: u64,
        error_code: Option<&str>,
    ) {
        let (Some(log), Some(p)) = (self.access_log.as_ref(), preamble) else {
            return;
        };
        log.record(crate::access_log::AccessLogEntry {
            time: std::time::SystemTime::now(),
            bucket: bucket.to_owned(),
            remote_ip: p.remote_ip,
            requester: p.requester,
            operation,
            key: key.map(str::to_owned),
            request_uri: p.request_uri,
            http_status,
            error_code: error_code.map(str::to_owned),
            bytes_sent,
            object_size,
            total_time_ms,
            user_agent: p.user_agent,
        })
        .await;
    }

    /// v0.4 #19: attach a per-(principal, bucket) token-bucket rate limiter.
    /// When set, every PUT / GET / DELETE / List / Copy / multipart op is
    /// throttle-checked before the policy gate; throttled requests return
    /// `S3ErrorCode::SlowDown` (HTTP 503) and bump
    /// `s4_rate_limit_throttled_total{principal,bucket}`.
    #[must_use]
    pub fn with_rate_limits(mut self, rl: crate::rate_limit::SharedRateLimits) -> Self {
        self.rate_limits = Some(rl);
        self
    }

    /// Helper used by request handlers to apply the rate limit. Returns
    /// `Ok(())` when allowed (or no rate limiter is configured), or a
    /// `SlowDown` S3Error otherwise.
    fn enforce_rate_limit<I>(&self, req: &S3Request<I>, bucket: &str) -> S3Result<()> {
        let Some(rl) = self.rate_limits.as_ref() else {
            return Ok(());
        };
        let principal_id = Self::principal_of(req);
        if !rl.check(principal_id, bucket) {
            crate::metrics::record_rate_limit_throttle(principal_id.unwrap_or("-"), bucket);
            return Err(S3Error::with_message(
                S3ErrorCode::SlowDown,
                format!("rate-limited: bucket={bucket}"),
            ));
        }
        Ok(())
    }

    /// Tell the policy evaluator that the listener is reached over TLS
    /// (or ACME). When `true`, the `aws:SecureTransport` Condition key
    /// resolves to `true`. Defaults to `false`.
    #[must_use]
    pub fn with_secure_transport(mut self, on: bool) -> Self {
        self.secure_transport = on;
        self
    }

    #[must_use]
    pub fn with_max_body_bytes(mut self, n: usize) -> Self {
        self.max_body_bytes = n;
        self
    }

    /// Attach an optional bucket policy (v0.2 #7). When `Some(...)`, every
    /// PUT / GET / DELETE / List handler runs `policy.evaluate(...)` before
    /// delegating to the backend; failures return `S3ErrorCode::AccessDenied`.
    /// When `None` (the default), no policy enforcement happens.
    #[must_use]
    pub fn with_policy(mut self, policy: crate::policy::SharedPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Pull the SigV4 access key id off the request's credentials, if any.
    /// Used as the `principal_id` for policy evaluation.
    fn principal_of<I>(req: &S3Request<I>) -> Option<&str> {
        req.credentials.as_ref().map(|c| c.access_key.as_str())
    }

    /// v0.3 #13: build the per-request policy context from the incoming
    /// `S3Request`. Pulls `aws:UserAgent` from the User-Agent header,
    /// `aws:SourceIp` from the standard `X-Forwarded-For` header (most
    /// production deployments are behind an LB / reverse proxy that sets
    /// this), `aws:CurrentTime` from the system clock, and
    /// `aws:SecureTransport` from the per-listener TLS flag.
    fn request_context<I>(&self, req: &S3Request<I>) -> crate::policy::RequestContext {
        let user_agent = req
            .headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // X-Forwarded-For is `client, proxy1, proxy2`; the leftmost entry
        // is the original client. Trim and parse leniently.
        let source_ip = req
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|raw| raw.split(',').next())
            .and_then(|s| s.trim().parse().ok());
        crate::policy::RequestContext {
            source_ip,
            user_agent,
            request_time: Some(std::time::SystemTime::now()),
            secure_transport: self.secure_transport,
            extra: Default::default(),
        }
    }

    /// Helper used by request handlers to enforce the optional policy.
    /// Returns `Ok(())` when allowed (or no policy is configured), or an
    /// `AccessDenied` S3Error otherwise. Bumps the policy denial Prometheus
    /// counter on deny.
    fn enforce_policy<I>(
        &self,
        req: &S3Request<I>,
        action: &'static str,
        bucket: &str,
        key: Option<&str>,
    ) -> S3Result<()> {
        let Some(policy) = self.policy.as_ref() else {
            return Ok(());
        };
        let principal_id = Self::principal_of(req);
        let ctx = self.request_context(req);
        let decision = policy.evaluate_with(action, bucket, key, principal_id, &ctx);
        if decision.allow {
            Ok(())
        } else {
            crate::metrics::record_policy_denial(action, bucket);
            tracing::info!(
                action,
                bucket,
                key = ?key,
                principal = ?principal_id,
                source_ip = ?ctx.source_ip,
                user_agent = ?ctx.user_agent,
                secure_transport = ctx.secure_transport,
                matched_sid = ?decision.matched_sid,
                effect = ?decision.matched_effect,
                "S4 policy denied request"
            );
            Err(S3Error::with_message(
                S3ErrorCode::AccessDenied,
                format!("denied by S4 policy: {action} on bucket={bucket}"),
            ))
        }
    }

    /// テスト用: backend を取り戻す (test helper、production では使わない)
    pub fn into_backend(self) -> B {
        self.backend
    }

    /// 必要 frame だけを backend に Range GET し、frame parse + decompress + slice
    /// した結果を返す sidecar fast path。Range request の **帯域節約版**。
    async fn partial_range_get(
        &self,
        req: &S3Request<GetObjectInput>,
        plan: s4_codec::index::RangePlan,
        client_start: u64,
        client_end_exclusive: u64,
        total_original: u64,
        get_start: Instant,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        // 必要 byte 範囲だけを backend に partial GET
        let backend_range = s3s::dto::Range::Int {
            first: plan.byte_start,
            last: Some(plan.byte_end_exclusive - 1),
        };
        let backend_input = GetObjectInput {
            bucket: req.input.bucket.clone(),
            key: req.input.key.clone(),
            range: Some(backend_range),
            ..Default::default()
        };
        let backend_req = S3Request {
            input: backend_input,
            method: req.method.clone(),
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let mut backend_resp = self.backend.get_object(backend_req).await?;
        let blob = backend_resp.output.body.take().ok_or_else(|| {
            S3Error::with_message(
                S3ErrorCode::InternalError,
                "backend partial GET returned empty body",
            )
        })?;
        let bytes = collect_blob(blob, self.max_body_bytes)
            .await
            .map_err(internal("collect partial body"))?;

        // frame parse + decompress
        let mut combined = BytesMut::new();
        for frame in FrameIter::new(bytes) {
            let (header, payload) = frame.map_err(|e| {
                S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!("partial-range frame parse: {e}"),
                )
            })?;
            let chunk_manifest = ChunkManifest {
                codec: header.codec,
                original_size: header.original_size,
                compressed_size: header.compressed_size,
                crc32c: header.crc32c,
            };
            let decompressed = self
                .registry
                .decompress(payload, &chunk_manifest)
                .await
                .map_err(internal("partial-range decompress"))?;
            combined.extend_from_slice(&decompressed);
        }
        let combined = combined.freeze();
        let sliced = combined
            .slice(plan.slice_start_in_combined as usize..plan.slice_end_in_combined as usize);

        // response 組立て
        let returned_size = sliced.len() as u64;
        backend_resp.output.content_length = Some(returned_size as i64);
        backend_resp.output.content_range = Some(format!(
            "bytes {client_start}-{}/{total_original}",
            client_end_exclusive - 1
        ));
        backend_resp.output.checksum_crc32 = None;
        backend_resp.output.checksum_crc32c = None;
        backend_resp.output.checksum_crc64nvme = None;
        backend_resp.output.checksum_sha1 = None;
        backend_resp.output.checksum_sha256 = None;
        backend_resp.output.e_tag = None;
        backend_resp.output.body = Some(bytes_to_blob(sliced));
        backend_resp.status = Some(http::StatusCode::PARTIAL_CONTENT);

        let elapsed = get_start.elapsed();
        crate::metrics::record_get(
            "partial",
            plan.byte_end_exclusive - plan.byte_start,
            returned_size,
            elapsed.as_secs_f64(),
            true,
        );
        info!(
            op = "get_object",
            bucket = %req.input.bucket,
            key = %req.input.key,
            bytes_in = plan.byte_end_exclusive - plan.byte_start,
            bytes_out = returned_size,
            total_object_size = total_original,
            range = true,
            path = "sidecar-partial",
            latency_ms = elapsed.as_millis() as u64,
            "S4 partial Range GET via sidecar index"
        );
        Ok(backend_resp)
    }

    /// `<key>.s4index` sidecar object を backend に書く。失敗しても本体 PUT は
    /// 成功扱いにしたいので、err は warn ログのみ (Range GET の partial path が
    /// 使えなくなるが、full read fallback で意味的には正しい結果を返す)。
    async fn write_sidecar(&self, bucket: &str, key: &str, index: &FrameIndex) {
        let bytes = encode_index(index);
        let len = bytes.len() as i64;
        let put_input = PutObjectInput {
            bucket: bucket.into(),
            key: sidecar_key(key),
            body: Some(bytes_to_blob(bytes)),
            content_length: Some(len),
            content_type: Some("application/x-s4-index".into()),
            ..Default::default()
        };
        let put_req = S3Request {
            input: put_input,
            method: http::Method::PUT,
            uri: format!("/{bucket}/{}", sidecar_key(key)).parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        if let Err(e) = self.backend.put_object(put_req).await {
            tracing::warn!(
                bucket,
                key,
                "S4 write_sidecar failed (Range GET will fall back to full read): {e}"
            );
        }
    }

    /// `<key>.s4index` sidecar を backend から読み出す。なければ None。
    async fn read_sidecar(&self, bucket: &str, key: &str) -> Option<FrameIndex> {
        let get_input = GetObjectInput {
            bucket: bucket.into(),
            key: sidecar_key(key),
            ..Default::default()
        };
        let get_req = S3Request {
            input: get_input,
            method: http::Method::GET,
            uri: format!("/{bucket}/{}", sidecar_key(key)).parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let resp = self.backend.get_object(get_req).await.ok()?;
        let blob = resp.output.body?;
        let bytes = collect_blob(blob, 64 * 1024 * 1024).await.ok()?;
        decode_index(bytes).ok()
    }

    /// Multipart object (frame 列) を解凍 → 元 bytes を再構築。
    ///
    /// **per-frame codec dispatch**: 各 frame header に codec_id が入っているので、
    /// frame ごとに registry が違う codec を呼ぶことができる。同一 object 内で
    /// 異なる codec が混在していても透過的に解凍可能 (parquet 風 mixed columns 等)。
    async fn decompress_multipart(&self, bytes: bytes::Bytes) -> S3Result<bytes::Bytes> {
        let mut out = BytesMut::new();
        for frame in FrameIter::new(bytes) {
            let (header, payload) = frame.map_err(|e| {
                S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!("multipart frame parse: {e}"),
                )
            })?;
            let chunk_manifest = ChunkManifest {
                codec: header.codec,
                original_size: header.original_size,
                compressed_size: header.compressed_size,
                crc32c: header.crc32c,
            };
            let decompressed = self
                .registry
                .decompress(payload, &chunk_manifest)
                .await
                .map_err(internal("multipart frame decompress"))?;
            out.extend_from_slice(&decompressed);
        }
        Ok(out.freeze())
    }
}

/// Parse a CopySourceRange header value (`bytes=N-M`, `bytes=N-`, `bytes=-N`)
/// into the s3s::dto::Range used by the GetObject path. The S3 spec only
/// allows `bytes=N-M` for upload_part_copy (no suffix or open-ended), so
/// reject the other variants for parity with AWS.
fn parse_copy_source_range(s: &str) -> Result<s3s::dto::Range, String> {
    let rest = s
        .strip_prefix("bytes=")
        .ok_or_else(|| format!("CopySourceRange must start with 'bytes=', got {s:?}"))?;
    let (a, b) = rest
        .split_once('-')
        .ok_or_else(|| format!("CopySourceRange must be 'bytes=N-M', got {s:?}"))?;
    let first: u64 = a
        .parse()
        .map_err(|_| format!("CopySourceRange first byte not a number: {a:?}"))?;
    let last: u64 = b
        .parse()
        .map_err(|_| format!("CopySourceRange last byte not a number: {b:?}"))?;
    if last < first {
        return Err(format!("CopySourceRange last < first: {s:?}"));
    }
    Ok(s3s::dto::Range::Int {
        first,
        last: Some(last),
    })
}

/// v0.5 #34: synthesize the backend storage key for a given
/// (logical key, version-id) pair on an Enabled-versioning bucket.
///
/// Uses the `__s4ver__/` infix because:
/// - it's not a substring of `.s4index` / `.s4ver` natural keys (no false-positive
///   listing filter collisions)
/// - directory-style separator keeps S3 console "browse by prefix" UX intact
///   (versions roll up under one virtual folder per object)
/// - human-readable on debug logs / `aws s3 ls`
///
/// `list_objects` / `list_objects_v2` / `list_object_versions` MUST filter
/// keys containing `.__s4ver__/` from results so customers don't see internal
/// shadow objects.
pub fn versioned_shadow_key(key: &str, version_id: &str) -> String {
    format!("{key}.__s4ver__/{version_id}")
}

/// Test for the marker substring used by [`versioned_shadow_key`]. Cheap str
/// scan; both list_objects filter and the GET passthrough check use this.
fn is_versioning_shadow_key(key: &str) -> bool {
    key.contains(".__s4ver__/")
}

fn is_multipart_object(metadata: &Option<Metadata>) -> bool {
    metadata
        .as_ref()
        .and_then(|m| m.get(META_MULTIPART))
        .map(|v| v == "true")
        .unwrap_or(false)
}

const META_CODEC: &str = "s4-codec";
const META_ORIGINAL_SIZE: &str = "s4-original-size";
const META_COMPRESSED_SIZE: &str = "s4-compressed-size";
const META_CRC32C: &str = "s4-crc32c";
/// Multipart upload で per-part frame format を使ったオブジェクトであることを示す。
/// GET 時にこの flag を見て frame parser を起動する。
const META_MULTIPART: &str = "s4-multipart";
/// v0.2 #4: single-PUT でも S4F2 framed format で書かれていることを示す。
/// 旧 v0.1 single-PUT は raw 圧縮 bytes (この flag なし)。GET 時にこの flag を
/// 見て framed 経路 (= multipart と同じ FrameIter parse) に流す。
const META_FRAMED: &str = "s4-framed";

fn is_framed_v2_object(metadata: &Option<Metadata>) -> bool {
    metadata
        .as_ref()
        .and_then(|m| m.get(META_FRAMED))
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// v0.4 #21: detect SSE-S4 by the metadata flag we set on PUT.
fn is_sse_encrypted(metadata: &Option<Metadata>) -> bool {
    metadata
        .as_ref()
        .and_then(|m| m.get("s4-encrypted"))
        .map(|v| v == "aes-256-gcm")
        .unwrap_or(false)
}

/// v0.5 #27: pull the three SSE-C headers off an input struct. The S3
/// contract is "all three or none" — partial sets are a 400.
///
/// Returns `Ok(None)` when no SSE-C headers were sent (server-managed or
/// no encryption), `Ok(Some(material))` on validated client key, and
/// `Err` for malformed or partial inputs.
fn extract_sse_c_material(
    algorithm: &Option<String>,
    key: &Option<String>,
    md5: &Option<String>,
) -> S3Result<Option<crate::sse::CustomerKeyMaterial>> {
    match (algorithm, key, md5) {
        (None, None, None) => Ok(None),
        (Some(a), Some(k), Some(m)) => crate::sse::parse_customer_key_headers(a, k, m)
            .map(Some)
            .map_err(sse_c_error_to_s3),
        _ => Err(S3Error::with_message(
            S3ErrorCode::InvalidRequest,
            "SSE-C requires all three of: x-amz-server-side-encryption-customer-{algorithm,key,key-MD5}",
        )),
    }
}

/// v0.5 #28: detect SSE-KMS request — `x-amz-server-side-encryption: aws:kms`.
/// Returns the key-id to wrap under, falling back to the gateway default.
fn extract_kms_key_id(
    sse: &Option<ServerSideEncryption>,
    sse_kms_key_id: &Option<String>,
    gateway_default: Option<&str>,
) -> Option<String> {
    let asks_for_kms = sse
        .as_ref()
        .map(|s| s.as_str() == ServerSideEncryption::AWS_KMS)
        .unwrap_or(false);
    if !asks_for_kms {
        return None;
    }
    sse_kms_key_id
        .clone()
        .or_else(|| gateway_default.map(str::to_owned))
}

/// v0.5 #28: map kms module errors to AWS-shaped S3 error codes.
/// `KeyNotFound` is operator misconfig (400); `BackendUnavailable` is a
/// transient KMS outage (503). Other variants are 500 InternalError.
fn kms_error_to_s3(e: crate::kms::KmsError) -> S3Error {
    use crate::kms::KmsError as K;
    match e {
        K::KeyNotFound { key_id } => S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!("KMS key not found: {key_id}"),
        ),
        K::BackendUnavailable { message } => S3Error::with_message(
            S3ErrorCode::ServiceUnavailable,
            format!("KMS backend unavailable: {message}"),
        ),
        other => S3Error::with_message(
            S3ErrorCode::InternalError,
            format!("KMS error: {other}"),
        ),
    }
}

/// v0.5 #27: map sse module errors to AWS-shaped S3 error codes.
/// `WrongCustomerKey` → 403 AccessDenied (matches AWS behaviour);
/// `InvalidCustomerKey` / algorithm / required / unexpected → 400.
fn sse_c_error_to_s3(e: crate::sse::SseError) -> S3Error {
    use crate::sse::SseError as E;
    match e {
        E::WrongCustomerKey => S3Error::with_message(
            S3ErrorCode::AccessDenied,
            "SSE-C key does not match the key used at PUT time",
        ),
        E::InvalidCustomerKey { reason } => S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!("SSE-C: {reason}"),
        ),
        E::CustomerKeyAlgorithmUnsupported { algo } => S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!("SSE-C unsupported algorithm: {algo:?} (only AES256 is allowed)"),
        ),
        E::CustomerKeyRequired => S3Error::with_message(
            S3ErrorCode::InvalidRequest,
            "object is SSE-C encrypted; supply x-amz-server-side-encryption-customer-* headers",
        ),
        E::CustomerKeyUnexpected => S3Error::with_message(
            S3ErrorCode::InvalidRequest,
            "object is not SSE-C encrypted; do not send x-amz-server-side-encryption-customer-* headers",
        ),
        other => S3Error::with_message(S3ErrorCode::InternalError, format!("SSE error: {other}")),
    }
}

fn extract_manifest(metadata: &Option<Metadata>) -> Option<ChunkManifest> {
    let m = metadata.as_ref()?;
    let codec = m
        .get(META_CODEC)
        .and_then(|s| s.parse::<CodecKind>().ok())?;
    let original_size = m.get(META_ORIGINAL_SIZE)?.parse().ok()?;
    let compressed_size = m.get(META_COMPRESSED_SIZE)?.parse().ok()?;
    let crc32c = m.get(META_CRC32C)?.parse().ok()?;
    Some(ChunkManifest {
        codec,
        original_size,
        compressed_size,
        crc32c,
    })
}

fn write_manifest(metadata: &mut Option<Metadata>, manifest: &ChunkManifest) {
    let meta = metadata.get_or_insert_with(Default::default);
    meta.insert(META_CODEC.into(), manifest.codec.as_str().into());
    meta.insert(
        META_ORIGINAL_SIZE.into(),
        manifest.original_size.to_string(),
    );
    meta.insert(
        META_COMPRESSED_SIZE.into(),
        manifest.compressed_size.to_string(),
    );
    meta.insert(META_CRC32C.into(), manifest.crc32c.to_string());
}

fn internal<E: std::fmt::Display>(prefix: &'static str) -> impl FnOnce(E) -> S3Error {
    move |e| S3Error::with_message(S3ErrorCode::InternalError, format!("{prefix}: {e}"))
}

/// v0.5 #30: parse the `x-amz-bypass-governance-retention` header into a
/// boolean flag. AWS S3 accepts `true` (case-insensitive); any other value
/// (including missing) is treated as `false`.
fn parse_bypass_governance_header(headers: &http::HeaderMap) -> bool {
    headers
        .get("x-amz-bypass-governance-retention")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Convert s3s `Timestamp` into a `chrono::DateTime<Utc>` by formatting it
/// as an RFC3339 string and re-parsing through `chrono`. The string format
/// avoids pulling the `time` crate (transitive dep of s3s, not declared by
/// s4-server) into our direct deps. Returns `None` if the format/parse fails
/// or the value is outside `chrono`'s supported range.
fn timestamp_to_chrono_utc(ts: &Timestamp) -> Option<chrono::DateTime<chrono::Utc>> {
    let mut buf = Vec::new();
    ts.format(s3s::dto::TimestampFormat::DateTime, &mut buf).ok()?;
    let s = std::str::from_utf8(&buf).ok()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Inverse of [`timestamp_to_chrono_utc`] — emit RFC3339 (the s3s
/// `DateTime` wire format) and re-parse via `Timestamp::parse`.
fn chrono_utc_to_timestamp(dt: chrono::DateTime<chrono::Utc>) -> Timestamp {
    // chrono's RFC3339 output format matches s3s' parser ("...Z" with
    // optional sub-second precision). Fall back to UNIX_EPOCH if anything
    // unexpected happens — we never produce malformed strings, so this
    // branch is unreachable in practice.
    let s = dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Timestamp::parse(s3s::dto::TimestampFormat::DateTime, &s).unwrap_or_default()
}

/// `Range` request を decompressed object サイズ `total` に適用して `(start, end_exclusive)`
/// を返す。`Range::Int { first, last }` は `bytes=first-last` (last は inclusive)、
/// `Range::Suffix { length }` は末尾 `length` byte。S3 仕様に準拠。
pub fn resolve_range(range: &s3s::dto::Range, total: u64) -> Result<(u64, u64), String> {
    if total == 0 {
        return Err("cannot range-get zero-length object".into());
    }
    match range {
        s3s::dto::Range::Int { first, last } => {
            let start = *first;
            let end_inclusive = match last {
                Some(l) => (*l).min(total - 1),
                None => total - 1,
            };
            if start > end_inclusive || start >= total {
                return Err(format!(
                    "range bytes={start}-{:?} out of object size {total}",
                    last
                ));
            }
            Ok((start, end_inclusive + 1))
        }
        s3s::dto::Range::Suffix { length } => {
            let len = (*length).min(total);
            Ok((total - len, total))
        }
    }
}

#[async_trait::async_trait]
impl<B: S3> S3 for S4Service<B> {
    // === 圧縮を挟む path (PUT) ===
    #[tracing::instrument(
        name = "s4.put_object",
        skip(self, req),
        fields(bucket = %req.input.bucket, key = %req.input.key, codec, bytes_in, bytes_out, latency_ms)
    )]
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let put_start = Instant::now();
        let put_bucket = req.input.bucket.clone();
        let put_key = req.input.key.clone();
        let access_preamble = self.access_log_preamble(&req);
        self.enforce_rate_limit(&req, &put_bucket)?;
        self.enforce_policy(&req, "s3:PutObject", &put_bucket, Some(&put_key))?;
        // v0.5 #30: an Object Lock-protected key cannot be overwritten by
        // a non-versioned PUT (Suspended / Unversioned bucket). Enabled
        // bucket PUTs are exempt because they materialise a fresh
        // version under a shadow key (`<key>.__s4ver__/<vid>`) — the
        // locked version's bytes are untouched. The check mirrors the
        // delete path (Compliance never bypassable, Governance via the
        // bypass header, legal hold never).
        if let Some(mgr) = self.object_lock.as_ref()
            && let Some(state) = mgr.get(&put_bucket, &put_key)
        {
            let bucket_versioned_enabled = self
                .versioning
                .as_ref()
                .map(|v| v.state(&put_bucket) == crate::versioning::VersioningState::Enabled)
                .unwrap_or(false);
            if !bucket_versioned_enabled {
                let bypass = parse_bypass_governance_header(&req.headers);
                let now = chrono::Utc::now();
                if !state.can_delete(now, bypass) {
                    crate::metrics::record_policy_denial("s3:PutObject", &put_bucket);
                    return Err(S3Error::with_message(
                        S3ErrorCode::AccessDenied,
                        "Access Denied because object protected by object lock",
                    ));
                }
            }
        }
        // v0.5 #30: per-PUT explicit retention / legal hold (S3
        // `x-amz-object-lock-mode`, `x-amz-object-lock-retain-until-date`,
        // `x-amz-object-lock-legal-hold`). Captured before the body
        // moves into the backend; persisted into the manager only on
        // backend success below.
        let explicit_lock_mode: Option<crate::object_lock::LockMode> = req
            .input
            .object_lock_mode
            .as_ref()
            .and_then(|m| crate::object_lock::LockMode::from_aws_str(m.as_str()));
        let explicit_retain_until: Option<chrono::DateTime<chrono::Utc>> = req
            .input
            .object_lock_retain_until_date
            .as_ref()
            .and_then(timestamp_to_chrono_utc);
        let explicit_legal_hold_on: Option<bool> = req
            .input
            .object_lock_legal_hold_status
            .as_ref()
            .map(|s| s.as_str().eq_ignore_ascii_case("ON"));
        if let Some(blob) = req.input.body.take() {
            // Sample 4 KiB から codec を決定。streaming-aware codec なら streaming
            // compress fast path、そうでなければ従来の collect-then-compress。
            let (sample, rest_stream) = peek_sample(blob, SAMPLE_BYTES)
                .await
                .map_err(internal("peek put sample"))?;
            let sample_len = sample.len().min(SAMPLE_BYTES);
            let kind = self.dispatcher.pick(&sample[..sample_len]).await;

            // Passthrough buys nothing from S4F2 wrapping (no compression =
            // no per-chunk frame to skip past) and the +28-byte header
            // overhead breaks size-sensitive callers that expect a true
            // pass-through. So passthrough always uses the legacy raw-blob
            // path; only compressing codecs go through the framed path.
            let use_framed = supports_streaming_compress(kind) && kind != CodecKind::Passthrough;
            let (compressed, manifest, is_framed) = if use_framed {
                // streaming fast path: input は memory に collect しない
                let chained = chain_sample_with_rest(sample, rest_stream);
                debug!(
                    bucket = ?req.input.bucket,
                    key = ?req.input.key,
                    codec = kind.as_str(),
                    path = "streaming-framed",
                    "S4 put_object: compressing (streaming, S4F2 multi-frame)"
                );
                // v0.4 #16: pick the chunk size based on the request's
                // Content-Length when known, falling back to the 4 MiB
                // default for chunked transfers.
                let chunk_size = pick_chunk_size(req.input.content_length.map(|n| n as u64));
                let (body, manifest) = streaming_compress_to_frames(
                    chained,
                    Arc::clone(&self.registry),
                    kind,
                    chunk_size,
                )
                .await
                .map_err(internal("streaming framed compress"))?;
                (body, manifest, true)
            } else {
                // GPU codec 等で streaming-aware でないものは bytes-buffered path
                // (raw 圧縮 bytes、framed なし — back-compat 互換 path)
                let bytes = collect_with_sample(sample, rest_stream, self.max_body_bytes)
                    .await
                    .map_err(internal("collect put body (buffered path)"))?;
                debug!(
                    bucket = ?req.input.bucket,
                    key = ?req.input.key,
                    bytes = bytes.len(),
                    codec = kind.as_str(),
                    path = "buffered",
                    "S4 put_object: compressing (buffered, raw blob)"
                );
                let (body, m) = self
                    .registry
                    .compress(bytes, kind)
                    .await
                    .map_err(internal("registry compress"))?;
                (body, m, false)
            };

            write_manifest(&mut req.input.metadata, &manifest);
            if is_framed {
                // v0.2 #4: framed body であることを GET 側に伝える meta flag。
                req.input
                    .metadata
                    .get_or_insert_with(Default::default)
                    .insert(META_FRAMED.into(), "true".into());
            }
            // 重要: content_length を圧縮後サイズで更新する。
            // これを忘れると下流 (aws-sdk-s3 → S3) が宣言サイズ分の bytes を
            // 待ち続けて RequestTimeout で失敗する (S3 仕様)。
            req.input.content_length = Some(compressed.len() as i64);
            // body を書き換えたので、客側が送ってきた original body 用の
            // checksum / MD5 ヘッダは無効化する (そのまま転送すると下流 S3 が
            // XAmzContentChecksumMismatch を返す)。S4 自身の整合性は
            // ChunkManifest.crc32c で担保している。
            req.input.checksum_algorithm = None;
            req.input.checksum_crc32 = None;
            req.input.checksum_crc32c = None;
            req.input.checksum_crc64nvme = None;
            req.input.checksum_sha1 = None;
            req.input.checksum_sha256 = None;
            req.input.content_md5 = None;
            let original_size = manifest.original_size;
            let compressed_size = manifest.compressed_size;
            let codec_label = manifest.codec.as_str();
            // framed body は GET 側で sidecar partial-fetch を効かせるため
            // build_index_from_body で sidecar を組み立てて backend に PUT する。
            let sidecar_index = if is_framed {
                s4_codec::index::build_index_from_body(&compressed).ok()
            } else {
                None
            };
            // v0.4 #21 / v0.5 #29 / v0.5 #27: encrypt-after-compress.
            // Precedence:
            //   - SSE-C headers present → per-request customer key (S4E3)
            //   - server-managed keyring configured → active key (S4E2)
            //   - neither → no encryption (raw compressed body)
            // The `s4-encrypted: aes-256-gcm` metadata flag is set in
            // both encrypted modes; the on-disk frame magic distinguishes
            // S4E1 / S4E2 / S4E3 so GET picks the right decrypt path.
            let sse_c_material = extract_sse_c_material(
                &req.input.sse_customer_algorithm,
                &req.input.sse_customer_key,
                &req.input.sse_customer_key_md5,
            )?;
            // v0.5 #28: SSE-KMS request? Resolves to None unless the
            // request asks for `aws:kms` AND a key id is available
            // (explicit header or gateway default). When set, we'll
            // generate a per-object DEK below.
            let kms_key_id = extract_kms_key_id(
                &req.input.server_side_encryption,
                &req.input.ssekms_key_id,
                self.kms_default_key_id.as_deref(),
            );
            // v0.5 #32: in compliance-strict mode, every PUT must
            // declare SSE — either client-supplied (SSE-C), KMS, or by
            // virtue of a server-side keyring being configured (which
            // applies SSE-S4 to every PUT automatically). Requests that
            // would otherwise land as plain compressed bytes are
            // rejected with 400 InvalidRequest.
            if self.compliance_strict
                && sse_c_material.is_none()
                && kms_key_id.is_none()
                && self.sse_keyring.is_none()
                && req
                    .input
                    .server_side_encryption
                    .as_ref()
                    .map(|s| s.as_str())
                    != Some(ServerSideEncryption::AES256)
            {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidRequest,
                    "compliance-mode strict: PUT must include x-amz-server-side-encryption \
                     (AES256 or aws:kms) or x-amz-server-side-encryption-customer-* headers",
                ));
            }
            // SSE-C and SSE-KMS are mutually exclusive on a single PUT
            // (AWS S3 returns 400 InvalidArgument). SSE-C wins by spec.
            if sse_c_material.is_some() && kms_key_id.is_some() {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    "SSE-C and SSE-KMS cannot be used together on the same PUT",
                ));
            }
            // KMS path needs to call generate_dek().await before the
            // body_to_send branch; capture the result here.
            let kms_wrap = if let Some(ref key_id) = kms_key_id {
                let kms = self.kms.as_ref().ok_or_else(|| {
                    S3Error::with_message(
                        S3ErrorCode::InvalidRequest,
                        "SSE-KMS requested but no --kms-local-dir / --kms-aws-region is configured on this gateway",
                    )
                })?;
                let (dek, wrapped) = kms
                    .generate_dek(key_id)
                    .await
                    .map_err(kms_error_to_s3)?;
                if dek.len() != 32 {
                    return Err(S3Error::with_message(
                        S3ErrorCode::InternalError,
                        format!("KMS backend returned a DEK of {} bytes (expected 32)", dek.len()),
                    ));
                }
                let mut dek_arr = [0u8; 32];
                dek_arr.copy_from_slice(&dek);
                Some((dek_arr, wrapped))
            } else {
                None
            };
            let body_to_send = if let Some(ref m) = sse_c_material {
                req.input
                    .metadata
                    .get_or_insert_with(Default::default)
                    .insert("s4-encrypted".into(), "aes-256-gcm".into());
                crate::sse::encrypt_with_source(
                    &compressed,
                    crate::sse::SseSource::CustomerKey {
                        key: &m.key,
                        key_md5: &m.key_md5,
                    },
                )
            } else if let Some((ref dek, ref wrapped)) = kms_wrap {
                req.input
                    .metadata
                    .get_or_insert_with(Default::default)
                    .insert("s4-encrypted".into(), "aes-256-gcm".into());
                crate::sse::encrypt_with_source(
                    &compressed,
                    crate::sse::SseSource::Kms { dek, wrapped },
                )
            } else if let Some(keyring) = self.sse_keyring.as_ref() {
                req.input
                    .metadata
                    .get_or_insert_with(Default::default)
                    .insert("s4-encrypted".into(), "aes-256-gcm".into());
                crate::sse::encrypt_v2(&compressed, keyring)
            } else {
                compressed.clone()
            };
            req.input.body = Some(bytes_to_blob(body_to_send));
            // v0.5 #34: pre-allocate a version-id when the bucket is
            // Enabled, then redirect the backend storage key to the
            // shadow path so older versions survive newer PUTs.
            // Suspended / Unversioned buckets keep using the plain
            // `<key>` (S3 spec: Suspended overwrites the same backend
            // object). Pre-allocation (instead of recording after PUT)
            // ensures the shadow key + the response's
            // `x-amz-version-id` use the same vid.
            let pending_version: Option<crate::versioning::PutOutcome> = self
                .versioning
                .as_ref()
                .map(|mgr| mgr.state(&put_bucket))
                .map(|state| match state {
                    crate::versioning::VersioningState::Enabled => {
                        crate::versioning::PutOutcome {
                            version_id: crate::versioning::VersioningManager::new_version_id(),
                            versioned_response: true,
                        }
                    }
                    crate::versioning::VersioningState::Suspended
                    | crate::versioning::VersioningState::Unversioned => {
                        crate::versioning::PutOutcome {
                            version_id: crate::versioning::NULL_VERSION_ID.to_owned(),
                            versioned_response: false,
                        }
                    }
                });
            if let Some(ref pv) = pending_version
                && pv.versioned_response
            {
                req.input.key = versioned_shadow_key(&put_key, &pv.version_id);
            }
            let mut backend_resp = self.backend.put_object(req).await;
            if let Some(idx) = sidecar_index
                && backend_resp.is_ok()
                && idx.entries.len() > 1
            {
                // 1 chunk しかない (small object) なら sidecar は意味がない (=
                // partial fetch しても full body と同じ範囲) ので省略。
                // Sidecar は user-visible key で書く (latest version の
                // partial fetch path 用)。Old versions の Range GET は今 task
                // の scope 外 (full read fallback でも意味的には正しい)。
                self.write_sidecar(&put_bucket, &put_key, &idx).await;
            }
            // v0.5 #34: commit the new version into the manager only on
            // backend success. Use the pre-allocated vid so the response
            // header and the chain entry agree.
            if let (Some(mgr), Some(pv), Ok(resp)) = (
                self.versioning.as_ref(),
                pending_version.as_ref(),
                backend_resp.as_mut(),
            ) {
                let etag = resp
                    .output
                    .e_tag
                    .clone()
                    .map(ETag::into_value)
                    .unwrap_or_else(|| format!("\"crc32c-{}\"", manifest.crc32c));
                let now = chrono::Utc::now();
                mgr.commit_put_with_version(
                    &put_bucket,
                    &put_key,
                    crate::versioning::VersionEntry {
                        version_id: pv.version_id.clone(),
                        etag,
                        size: original_size,
                        is_delete_marker: false,
                        created_at: now,
                    },
                );
                if pv.versioned_response {
                    resp.output.version_id = Some(pv.version_id.clone());
                }
            }
            // v0.5 #27: AWS S3 echoes the SSE-C headers back on success
            // so the client knows the server actually applied the
            // requested algorithm and which key fingerprint matched.
            if let (Some(m), Ok(resp)) = (sse_c_material.as_ref(), backend_resp.as_mut()) {
                resp.output.sse_customer_algorithm = Some(crate::sse::SSE_C_ALGORITHM.into());
                resp.output.sse_customer_key_md5 = Some(
                    base64::engine::general_purpose::STANDARD.encode(m.key_md5),
                );
            }
            // v0.5 #28: SSE-KMS echo — `aws:kms` + the canonical key id
            // the backend returned (AWS KMS returns the ARN even when
            // the request used an alias).
            if let (Some((_, wrapped)), Ok(resp)) =
                (kms_wrap.as_ref(), backend_resp.as_mut())
            {
                resp.output.server_side_encryption =
                    Some(ServerSideEncryption::from_static(ServerSideEncryption::AWS_KMS));
                resp.output.ssekms_key_id = Some(wrapped.key_id.clone());
            }
            // v0.5 #30: persist any per-PUT explicit retention / legal
            // hold the client supplied, then auto-apply the bucket
            // default (no-op when state is already populated). The
            // explicit fields take precedence — the bucket-default
            // helper bails out as soon as it sees any retention.
            if let (Some(mgr), Ok(_)) = (self.object_lock.as_ref(), backend_resp.as_ref()) {
                if explicit_lock_mode.is_some()
                    || explicit_retain_until.is_some()
                    || explicit_legal_hold_on.is_some()
                {
                    let mut state = mgr.get(&put_bucket, &put_key).unwrap_or_default();
                    if let Some(m) = explicit_lock_mode {
                        state.mode = Some(m);
                    }
                    if let Some(u) = explicit_retain_until {
                        state.retain_until = Some(u);
                    }
                    if let Some(lh) = explicit_legal_hold_on {
                        state.legal_hold_on = lh;
                    }
                    mgr.set(&put_bucket, &put_key, state);
                }
                mgr.apply_default_on_put(&put_bucket, &put_key, chrono::Utc::now());
            }
            let _ = (original_size, compressed_size); // mute unused warnings
            let elapsed = put_start.elapsed();
            crate::metrics::record_put(
                codec_label,
                original_size,
                compressed_size,
                elapsed.as_secs_f64(),
                backend_resp.is_ok(),
            );
            // v0.4 #20: structured access-log entry (best-effort).
            self.record_access(
                access_preamble,
                "REST.PUT.OBJECT",
                &put_bucket,
                Some(&put_key),
                if backend_resp.is_ok() { 200 } else { 500 },
                compressed_size,
                original_size,
                elapsed.as_millis() as u64,
                backend_resp.as_ref().err().map(|e| e.code().as_str()),
            )
            .await;
            info!(
                op = "put_object",
                bucket = %put_bucket,
                key = %put_key,
                codec = codec_label,
                bytes_in = original_size,
                bytes_out = compressed_size,
                ratio = format!(
                    "{:.3}",
                    if original_size == 0 { 1.0 } else { compressed_size as f64 / original_size as f64 }
                ),
                latency_ms = elapsed.as_millis() as u64,
                ok = backend_resp.is_ok(),
                "S4 put completed"
            );
            return backend_resp;
        }
        // Body-less PUT (rare: zero-length object). Mirror the body-full
        // versioning hooks so list_object_versions / GET-by-version still see
        // empty-body objects in the chain.
        let pending_version: Option<crate::versioning::PutOutcome> = self
            .versioning
            .as_ref()
            .map(|mgr| mgr.state(&put_bucket))
            .map(|state| match state {
                crate::versioning::VersioningState::Enabled => crate::versioning::PutOutcome {
                    version_id: crate::versioning::VersioningManager::new_version_id(),
                    versioned_response: true,
                },
                _ => crate::versioning::PutOutcome {
                    version_id: crate::versioning::NULL_VERSION_ID.to_owned(),
                    versioned_response: false,
                },
            });
        if let Some(ref pv) = pending_version
            && pv.versioned_response
        {
            req.input.key = versioned_shadow_key(&put_key, &pv.version_id);
        }
        let mut backend_resp = self.backend.put_object(req).await;
        if let (Some(mgr), Some(pv), Ok(resp)) = (
            self.versioning.as_ref(),
            pending_version.as_ref(),
            backend_resp.as_mut(),
        ) {
            let etag = resp
                .output
                .e_tag
                .clone()
                .map(ETag::into_value)
                .unwrap_or_default();
            let now = chrono::Utc::now();
            mgr.commit_put_with_version(
                &put_bucket,
                &put_key,
                crate::versioning::VersionEntry {
                    version_id: pv.version_id.clone(),
                    etag,
                    size: 0,
                    is_delete_marker: false,
                    created_at: now,
                },
            );
            if pv.versioned_response {
                resp.output.version_id = Some(pv.version_id.clone());
            }
        }
        // v0.5 #30: same explicit-then-default lock-state commit as the
        // body-bearing branch above, so a zero-length PUT also picks up
        // bucket-default retention.
        if let (Some(mgr), Ok(_)) = (self.object_lock.as_ref(), backend_resp.as_ref()) {
            if explicit_lock_mode.is_some()
                || explicit_retain_until.is_some()
                || explicit_legal_hold_on.is_some()
            {
                let mut state = mgr.get(&put_bucket, &put_key).unwrap_or_default();
                if let Some(m) = explicit_lock_mode {
                    state.mode = Some(m);
                }
                if let Some(u) = explicit_retain_until {
                    state.retain_until = Some(u);
                }
                if let Some(lh) = explicit_legal_hold_on {
                    state.legal_hold_on = lh;
                }
                mgr.set(&put_bucket, &put_key, state);
            }
            mgr.apply_default_on_put(&put_bucket, &put_key, chrono::Utc::now());
        }
        backend_resp
    }

    // === 圧縮を解く path (GET) ===
    #[tracing::instrument(
        name = "s4.get_object",
        skip(self, req),
        fields(bucket = %req.input.bucket, key = %req.input.key, codec, bytes_out, range, path)
    )]
    async fn get_object(
        &self,
        mut req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let get_start = Instant::now();
        let get_bucket = req.input.bucket.clone();
        let get_key = req.input.key.clone();
        self.enforce_rate_limit(&req, &get_bucket)?;
        self.enforce_policy(&req, "s3:GetObject", &get_bucket, Some(&get_key))?;
        // Range request の事前検出 (decompress 後 slice する path に使う)。
        let range_request = req.input.range.take();
        // v0.5 #27: pull SSE-C material from the input headers before
        // the request is moved into the backend. A header parse error
        // fails fast (no body fetch). The material is consumed below
        // when decrypting an S4E3-framed body; the SSE-C headers on
        // `req.input` are cleared so the backend doesn't see them.
        let sse_c_alg = req.input.sse_customer_algorithm.take();
        let sse_c_key = req.input.sse_customer_key.take();
        let sse_c_md5 = req.input.sse_customer_key_md5.take();
        let get_sse_c_material =
            extract_sse_c_material(&sse_c_alg, &sse_c_key, &sse_c_md5)?;

        // v0.5 #34: route the GET through the VersioningManager when
        // attached AND the bucket is in a versioning-aware state.
        // Resolves which version to fetch (explicit `?versionId=` query
        // param vs. chain latest), translates a delete-marker into 404
        // NoSuchKey, and rewrites the backend storage key to the shadow
        // path (`<key>.__s4ver__/<vid>`) for non-null Enabled-bucket
        // versions. `resolved_version_id` is stamped onto the response
        // so clients see a coherent `x-amz-version-id` header.
        //
        // When the bucket is Unversioned (or no manager attached), the
        // chain-resolution step is skipped and the request flows
        // through the existing single-key path unchanged.
        let resolved_version_id: Option<String> = match self.versioning.as_ref() {
            Some(mgr)
                if mgr.state(&get_bucket) != crate::versioning::VersioningState::Unversioned =>
            {
                let req_vid = req.input.version_id.take();
                let entry = match req_vid.as_deref() {
                    Some(vid) => mgr.lookup_version(&get_bucket, &get_key, vid).ok_or_else(
                        || S3Error::with_message(
                            S3ErrorCode::NoSuchVersion,
                            format!("no such version: {vid}"),
                        ),
                    )?,
                    None => mgr.lookup_latest(&get_bucket, &get_key).ok_or_else(|| {
                        S3Error::with_message(
                            S3ErrorCode::NoSuchKey,
                            format!("no such key: {get_key}"),
                        )
                    })?,
                };
                if entry.is_delete_marker {
                    // S3 spec: GET without versionId on a
                    // delete-marker latest → 404 NoSuchKey + the
                    // response carries `x-amz-delete-marker: true`.
                    // GET with explicit versionId pointing at a delete
                    // marker → 405 MethodNotAllowed; we surface
                    // NoSuchKey here for both since s3s collapses them
                    // into the same not-found error path.
                    return Err(S3Error::with_message(
                        S3ErrorCode::NoSuchKey,
                        format!("delete marker is the current version of {get_key}"),
                    ));
                }
                if entry.version_id != crate::versioning::NULL_VERSION_ID {
                    req.input.key = versioned_shadow_key(&get_key, &entry.version_id);
                }
                Some(entry.version_id)
            }
            _ => None,
        };

        // ====== Range GET の partial-fetch fast path (sidecar index 利用) ======
        // sidecar `<key>.s4index` が存在し、multipart-framed object であれば
        // 必要 frame だけを backend に Range GET し帯域節約する。
        if let Some(ref r) = range_request
            && let Some(index) = self.read_sidecar(&req.input.bucket, &req.input.key).await
        {
            let total = index.total_original_size();
            let (start, end_exclusive) = match resolve_range(r, total) {
                Ok(v) => v,
                Err(e) => {
                    return Err(S3Error::with_message(S3ErrorCode::InvalidRange, e));
                }
            };
            if let Some(plan) = index.lookup_range(start, end_exclusive) {
                return self
                    .partial_range_get(&req, plan, start, end_exclusive, total, get_start)
                    .await;
            }
        }
        let mut resp = self.backend.get_object(req).await?;
        // v0.5 #34: stamp the resolved version-id so the client sees a
        // coherent `x-amz-version-id` header (only for chains owned by
        // the manager — Unversioned buckets / no-manager paths never
        // set this).
        if let Some(ref vid) = resolved_version_id {
            resp.output.version_id = Some(vid.clone());
        }
        let is_multipart = is_multipart_object(&resp.output.metadata);
        let is_framed_v2 = is_framed_v2_object(&resp.output.metadata);
        // v0.2 #4: framed-v2 single-PUT は多 frame parse が必要なので
        // multipart と同じ path に流す。
        let needs_frame_parse = is_multipart || is_framed_v2;
        let manifest_opt = extract_manifest(&resp.output.metadata);

        if !needs_frame_parse && manifest_opt.is_none() {
            // S4 が書いていないオブジェクトは透過 (raw bucket pre-existing object 等)
            debug!("S4 get_object: object lacks s4-codec metadata, returning as-is");
            return Ok(resp);
        }

        if let Some(blob) = resp.output.body.take() {
            // v0.4 #21 / v0.5 #27: if the object was stored under SSE
            // (metadata flag `s4-encrypted: aes-256-gcm`), decrypt
            // before any frame parse / streaming decompress. Encrypted
            // bodies are opaque to the codec; this also forces the
            // buffered path because AES-GCM needs the full body for tag
            // verify. SSE-C uses the per-request customer key, SSE-S4
            // falls back to the configured keyring.
            let blob = if is_sse_encrypted(&resp.output.metadata) {
                let body = collect_blob(blob, self.max_body_bytes)
                    .await
                    .map_err(internal("collect SSE-encrypted body"))?;
                // v0.5 #28: peek the frame magic to route the right
                // decrypt path. S4E4 means SSE-KMS — unwrap the DEK
                // through the KMS backend (async). S4E1/E2/E3 take the
                // sync path (keyring or customer key).
                let plain = match crate::sse::peek_magic(&body) {
                    Some("S4E4") => {
                        let kms = self.kms.as_ref().ok_or_else(|| {
                            S3Error::with_message(
                                S3ErrorCode::InvalidRequest,
                                "object is SSE-KMS encrypted but no --kms-local-dir / --kms-aws-region is configured on this gateway",
                            )
                        })?;
                        let kms_ref: &dyn crate::kms::KmsBackend = kms.as_ref();
                        crate::sse::decrypt_with_kms(&body, kms_ref)
                            .await
                            .map_err(|e| match e {
                                crate::sse::SseError::KmsBackend(k) => kms_error_to_s3(k),
                                other => S3Error::with_message(
                                    S3ErrorCode::InternalError,
                                    format!("SSE-KMS decrypt failed: {other}"),
                                ),
                            })?
                    }
                    _ => {
                        if let Some(ref m) = get_sse_c_material {
                            crate::sse::decrypt(
                                &body,
                                crate::sse::SseSource::CustomerKey {
                                    key: &m.key,
                                    key_md5: &m.key_md5,
                                },
                            )
                            .map_err(sse_c_error_to_s3)?
                        } else {
                            let keyring = self.sse_keyring.as_ref().ok_or_else(|| {
                                S3Error::with_message(
                                    S3ErrorCode::InvalidRequest,
                                    "object is SSE-S4 encrypted but no --sse-s4-key is configured on this gateway",
                                )
                            })?;
                            crate::sse::decrypt(&body, keyring).map_err(|e| {
                                S3Error::with_message(
                                    S3ErrorCode::InternalError,
                                    format!("SSE-S4 decrypt failed: {e}"),
                                )
                            })?
                        }
                    }
                };
                // v0.5 #28: parse out the on-disk wrapped DEK's key id
                // so the GET response can echo `x-amz-server-side-encryption-aws-kms-key-id`.
                if matches!(crate::sse::peek_magic(&body), Some("S4E4"))
                    && let Ok(hdr) = crate::sse::parse_s4e4_header(&body)
                {
                    resp.output.server_side_encryption = Some(
                        ServerSideEncryption::from_static(ServerSideEncryption::AWS_KMS),
                    );
                    resp.output.ssekms_key_id = Some(hdr.key_id.to_string());
                }
                bytes_to_blob(plain)
            } else if let Some(ref m) = get_sse_c_material {
                // Client sent SSE-C headers for an unencrypted object —
                // mirror AWS S3's 400 InvalidRequest.
                let _ = m;
                return Err(sse_c_error_to_s3(crate::sse::SseError::CustomerKeyUnexpected));
            } else {
                blob
            };
            // v0.5 #27: SSE-C echo on success — algorithm + key MD5
            // tell the client that the supplied key was the one used.
            if let Some(ref m) = get_sse_c_material {
                resp.output.sse_customer_algorithm = Some(crate::sse::SSE_C_ALGORITHM.into());
                resp.output.sse_customer_key_md5 = Some(
                    base64::engine::general_purpose::STANDARD.encode(m.key_md5),
                );
            }
            // ====== Streaming fast path (CpuZstd, non-multipart, codec supports it) ======
            // 大規模 object (e.g. 5 GB) を memory に collect すると OOM するので、
            // codec が streaming-aware なら body を chunk-by-chunk で decompress して
            // 即座に client に流す。
            //
            // ただし Range request 時は streaming できない (slice するため total bytes
            // が必要) → buffered path に fall through。
            if range_request.is_none()
                && !needs_frame_parse
                && let Some(ref m) = manifest_opt
                && supports_streaming_decompress(m.codec)
                && m.codec == CodecKind::CpuZstd
            {
                let decompressed_blob = cpu_zstd_decompress_stream(blob);
                resp.output.content_length = Some(m.original_size as i64);
                resp.output.checksum_crc32 = None;
                resp.output.checksum_crc32c = None;
                resp.output.checksum_crc64nvme = None;
                resp.output.checksum_sha1 = None;
                resp.output.checksum_sha256 = None;
                resp.output.e_tag = None;
                resp.output.body = Some(decompressed_blob);
                let elapsed = get_start.elapsed();
                crate::metrics::record_get(
                    m.codec.as_str(),
                    m.compressed_size,
                    m.original_size,
                    elapsed.as_secs_f64(),
                    true,
                );
                info!(
                    op = "get_object",
                    bucket = %get_bucket,
                    key = %get_key,
                    codec = m.codec.as_str(),
                    bytes_in = m.compressed_size,
                    bytes_out = m.original_size,
                    path = "streaming",
                    setup_latency_ms = elapsed.as_millis() as u64,
                    "S4 get started (streaming)"
                );
                return Ok(resp);
            }
            // Passthrough: そのまま流す (Range なしの場合のみ streaming)
            if range_request.is_none()
                && !needs_frame_parse
                && let Some(ref m) = manifest_opt
                && m.codec == CodecKind::Passthrough
            {
                resp.output.content_length = Some(m.original_size as i64);
                resp.output.checksum_crc32 = None;
                resp.output.checksum_crc32c = None;
                resp.output.checksum_crc64nvme = None;
                resp.output.checksum_sha1 = None;
                resp.output.checksum_sha256 = None;
                resp.output.e_tag = None;
                resp.output.body = Some(blob);
                debug!("S4 get_object: passthrough streaming");
                return Ok(resp);
            }

            // ====== Buffered slow path (multipart frame parser, GPU codecs) ======
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect get body"))?;

            let decompressed = if needs_frame_parse {
                // multipart objects と framed-v2 single-PUT objects は同じ
                // S4F2 frame 列なので decompress_multipart で統一処理
                self.decompress_multipart(bytes).await?
            } else {
                let manifest = manifest_opt.as_ref().expect("non-multipart guarded above");
                self.registry
                    .decompress(bytes, manifest)
                    .await
                    .map_err(internal("registry decompress"))?
            };

            // Range request があれば slice。なければ full body を返す。
            let total_size = decompressed.len() as u64;
            let (final_bytes, status_override) = if let Some(r) = range_request.as_ref() {
                let (start, end) = resolve_range(r, total_size)
                    .map_err(|e| S3Error::with_message(S3ErrorCode::InvalidRange, e))?;
                let sliced = decompressed.slice(start as usize..end as usize);
                resp.output.content_range = Some(format!(
                    "bytes {start}-{}/{total_size}",
                    end.saturating_sub(1)
                ));
                (sliced, Some(http::StatusCode::PARTIAL_CONTENT))
            } else {
                (decompressed, None)
            };
            // 解凍後の真のサイズを返す (S3 client は content_length を信頼するので
            // 圧縮 size のままだと downstream が body を途中で切ってしまう)
            resp.output.content_length = Some(final_bytes.len() as i64);
            // 圧縮済 bytes の checksum を返すと AWS SDK 側で StreamingError
            // (ChecksumMismatch) になる。ETag も backend が返した「圧縮済 bytes の
            // MD5/checksum」なので意味的にズレる — クリアして S4 自身の crc32c
            // (manifest 内 / frame 内) で integrity を保証する設計にする。
            resp.output.checksum_crc32 = None;
            resp.output.checksum_crc32c = None;
            resp.output.checksum_crc64nvme = None;
            resp.output.checksum_sha1 = None;
            resp.output.checksum_sha256 = None;
            resp.output.e_tag = None;
            let returned_size = final_bytes.len() as u64;
            let codec_label = manifest_opt
                .as_ref()
                .map(|m| m.codec.as_str())
                .unwrap_or("multipart");
            resp.output.body = Some(bytes_to_blob(final_bytes));
            if let Some(status) = status_override {
                resp.status = Some(status);
            }
            let elapsed = get_start.elapsed();
            crate::metrics::record_get(codec_label, 0, returned_size, elapsed.as_secs_f64(), true);
            info!(
                op = "get_object",
                bucket = %get_bucket,
                key = %get_key,
                codec = codec_label,
                bytes_out = returned_size,
                total_object_size = total_size,
                range = range_request.is_some(),
                path = "buffered",
                latency_ms = elapsed.as_millis() as u64,
                "S4 get completed (buffered)"
            );
        }
        Ok(resp)
    }

    // === passthrough delegations ===
    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        self.backend.head_bucket(req).await
    }
    async fn list_buckets(
        &self,
        req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        self.backend.list_buckets(req).await
    }
    async fn create_bucket(
        &self,
        req: S3Request<CreateBucketInput>,
    ) -> S3Result<S3Response<CreateBucketOutput>> {
        self.backend.create_bucket(req).await
    }
    async fn delete_bucket(
        &self,
        req: S3Request<DeleteBucketInput>,
    ) -> S3Result<S3Response<DeleteBucketOutput>> {
        self.backend.delete_bucket(req).await
    }
    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let mut resp = self.backend.head_object(req).await?;
        if let Some(manifest) = extract_manifest(&resp.output.metadata) {
            // 客側には decompress 後の意味のある content_length / checksum を返す。
            // backend が返す圧縮済 bytes の checksum / e_tag は意味が違うため除去
            // (S4 は manifest 内の crc32c で integrity を担保する)。
            resp.output.content_length = Some(manifest.original_size as i64);
            resp.output.checksum_crc32 = None;
            resp.output.checksum_crc32c = None;
            resp.output.checksum_crc64nvme = None;
            resp.output.checksum_sha1 = None;
            resp.output.checksum_sha256 = None;
            resp.output.e_tag = None;
        }
        Ok(resp)
    }
    async fn delete_object(
        &self,
        mut req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        self.enforce_rate_limit(&req, &bucket)?;
        self.enforce_policy(&req, "s3:DeleteObject", &bucket, Some(&key))?;
        // v0.5 #30: refuse the delete while a WORM lock is in effect.
        // Compliance can never be bypassed; Governance can be overridden
        // via `x-amz-bypass-governance-retention: true`; legal hold
        // never. The check happens before the versioning router so a
        // locked object can't be soft-deleted (delete-marker push) on an
        // Enabled bucket either — S3 spec says lock applies to all
        // delete forms.
        if let Some(mgr) = self.object_lock.as_ref()
            && let Some(state) = mgr.get(&bucket, &key)
        {
            let bypass = req.input.bypass_governance_retention.unwrap_or(false);
            let now = chrono::Utc::now();
            if !state.can_delete(now, bypass) {
                crate::metrics::record_policy_denial("s3:DeleteObject", &bucket);
                return Err(S3Error::with_message(
                    S3ErrorCode::AccessDenied,
                    "Access Denied because object protected by object lock",
                ));
            }
        }
        // v0.5 #34: route DELETE through the VersioningManager when the
        // bucket is in a versioning-aware state.
        //
        // - Enabled bucket, no version_id → push a delete marker into
        //   the chain. NO backend object is touched (older versions
        //   stay reachable via specific-version GET).
        // - Enabled / Suspended bucket, with version_id → physical
        //   delete. Backend bytes at the shadow key (or `<key>` for
        //   `null`) are removed; chain entry is dropped. If the deleted
        //   entry was a delete marker, no backend bytes exist for it
        //   (record-only).
        // - Suspended bucket, no version_id → push a "null" delete
        //   marker (S3 spec); backend bytes at `<key>` are physically
        //   removed (same as legacy).
        // - Unversioned bucket → fall through to legacy passthrough.
        if let Some(mgr) = self.versioning.as_ref() {
            let state = mgr.state(&bucket);
            if state != crate::versioning::VersioningState::Unversioned {
                let req_vid = req.input.version_id.take();
                if let Some(vid) = req_vid {
                    // Specific-version DELETE: touch backend bytes only
                    // when the entry was a real version (not a delete
                    // marker, which has no backend bytes).
                    let outcome = mgr.record_delete_specific(&bucket, &key, &vid);
                    let backend_target = if vid == crate::versioning::NULL_VERSION_ID {
                        key.clone()
                    } else {
                        versioned_shadow_key(&key, &vid)
                    };
                    let was_real_version = outcome
                        .as_ref()
                        .map(|o| !o.is_delete_marker)
                        .unwrap_or(false);
                    if was_real_version {
                        // Best-effort backend cleanup; missing bytes
                        // are not an error (e.g. shadow key already
                        // GC'd).
                        let backend_input = DeleteObjectInput {
                            bucket: bucket.clone(),
                            key: backend_target,
                            ..Default::default()
                        };
                        let backend_req = S3Request {
                            input: backend_input,
                            method: http::Method::DELETE,
                            uri: req.uri.clone(),
                            headers: req.headers.clone(),
                            extensions: http::Extensions::new(),
                            credentials: req.credentials.clone(),
                            region: req.region.clone(),
                            service: req.service.clone(),
                            trailing_headers: None,
                        };
                        let _ = self.backend.delete_object(backend_req).await;
                    }
                    let mut output = DeleteObjectOutput {
                        version_id: Some(vid.clone()),
                        ..Default::default()
                    };
                    if let Some(o) = outcome.as_ref()
                        && o.is_delete_marker
                    {
                        output.delete_marker = Some(true);
                    }
                    return Ok(S3Response::new(output));
                }
                // No version_id: record a delete marker (state-aware).
                let outcome = mgr.record_delete(&bucket, &key);
                if state == crate::versioning::VersioningState::Suspended {
                    // Suspended buckets also evict the prior `<key>`
                    // bytes (the previous null version is gone too).
                    let backend_input = DeleteObjectInput {
                        bucket: bucket.clone(),
                        key: key.clone(),
                        ..Default::default()
                    };
                    let backend_req = S3Request {
                        input: backend_input,
                        method: http::Method::DELETE,
                        uri: req.uri.clone(),
                        headers: req.headers.clone(),
                        extensions: http::Extensions::new(),
                        credentials: req.credentials.clone(),
                        region: req.region.clone(),
                        service: req.service.clone(),
                        trailing_headers: None,
                    };
                    let _ = self.backend.delete_object(backend_req).await;
                }
                let output = DeleteObjectOutput {
                    delete_marker: Some(true),
                    version_id: outcome.version_id,
                    ..Default::default()
                };
                return Ok(S3Response::new(output));
            }
        }
        // Legacy / Unversioned path: physical delete on the backend +
        // best-effort sidecar cleanup (mirrors v0.4 behaviour).
        let resp = self.backend.delete_object(req).await?;
        // v0.5 #30: drop any per-object lock state once the delete has
        // succeeded so the freed key can be re-armed by a future PUT
        // under the bucket default. Reaching here implies the lock had
        // already passed `can_delete` above, so this is purely cleanup.
        if let Some(mgr) = self.object_lock.as_ref() {
            mgr.clear(&bucket, &key);
        }
        let sidecar_input = DeleteObjectInput {
            bucket: bucket.clone(),
            key: sidecar_key(&key),
            ..Default::default()
        };
        let sidecar_req = S3Request {
            input: sidecar_input,
            method: http::Method::DELETE,
            uri: format!("/{bucket}/{}", sidecar_key(&key)).parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let _ = self.backend.delete_object(sidecar_req).await;
        Ok(resp)
    }
    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        self.backend.delete_objects(req).await
    }
    async fn copy_object(
        &self,
        mut req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        // copy is conceptually "GetObject src + PutObject dst" — enforce both.
        let dst_bucket = req.input.bucket.clone();
        let dst_key = req.input.key.clone();
        self.enforce_policy(&req, "s3:PutObject", &dst_bucket, Some(&dst_key))?;
        if let CopySource::Bucket { bucket, key, .. } = &req.input.copy_source {
            self.enforce_policy(&req, "s3:GetObject", bucket, Some(key))?;
        }
        // S4-aware copy: source object に s4-* metadata がある場合、それを
        // destination に確実に preserve する。
        //
        // - MetadataDirective::COPY (default): backend が source metadata を
        //   そのまま copy するので S4 metadata も自動で渡る。介入不要
        // - MetadataDirective::REPLACE: 客が指定した metadata で source を
        //   上書き → s4-* metadata が消えると destination は decompress 不能に
        //   なる (silent corruption)。S4 が source metadata を HEAD で取得し、
        //   s4-* fields を input.metadata に強制 merge する
        let needs_merge = req
            .input
            .metadata_directive
            .as_ref()
            .map(|d| d.as_str() == MetadataDirective::REPLACE)
            .unwrap_or(false);
        if needs_merge && let CopySource::Bucket { bucket, key, .. } = &req.input.copy_source {
            let head_input = HeadObjectInput {
                bucket: bucket.to_string(),
                key: key.to_string(),
                ..Default::default()
            };
            let head_req = S3Request {
                input: head_input,
                method: req.method.clone(),
                uri: req.uri.clone(),
                headers: req.headers.clone(),
                extensions: http::Extensions::new(),
                credentials: req.credentials.clone(),
                region: req.region.clone(),
                service: req.service.clone(),
                trailing_headers: None,
            };
            if let Ok(head) = self.backend.head_object(head_req).await
                && let Some(src_meta) = head.output.metadata.as_ref()
            {
                let dest_meta = req.input.metadata.get_or_insert_with(Default::default);
                for key in [
                    META_CODEC,
                    META_ORIGINAL_SIZE,
                    META_COMPRESSED_SIZE,
                    META_CRC32C,
                    META_MULTIPART,
                    META_FRAMED,
                ] {
                    if let Some(v) = src_meta.get(key) {
                        // 客が同じ key を指定していたら preserve しない (= 上書き許可)
                        // していたら何もしない。指定していなければ insert
                        dest_meta
                            .entry(key.to_string())
                            .or_insert_with(|| v.clone());
                    }
                }
                debug!(
                    src_bucket = %bucket,
                    src_key = %key,
                    "S4 copy_object: preserved s4-* metadata across REPLACE directive"
                );
            }
        }
        self.backend.copy_object(req).await
    }
    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        self.enforce_rate_limit(&req, &req.input.bucket)?;
        self.enforce_policy(&req, "s3:ListBucket", &req.input.bucket, None)?;
        let mut resp = self.backend.list_objects(req).await?;
        // S4 内部 object (`*.s4index` sidecar、`.__s4ver__/` shadow versions
        // — v0.5 #34) を顧客から隠す。
        if let Some(contents) = resp.output.contents.as_mut() {
            contents.retain(|o| {
                o.key
                    .as_ref()
                    .map(|k| !k.ends_with(".s4index") && !is_versioning_shadow_key(k))
                    .unwrap_or(true)
            });
        }
        Ok(resp)
    }
    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        self.enforce_rate_limit(&req, &req.input.bucket)?;
        self.enforce_policy(&req, "s3:ListBucket", &req.input.bucket, None)?;
        let mut resp = self.backend.list_objects_v2(req).await?;
        if let Some(contents) = resp.output.contents.as_mut() {
            let before = contents.len();
            contents.retain(|o| {
                o.key
                    .as_ref()
                    .map(|k| !k.ends_with(".s4index") && !is_versioning_shadow_key(k))
                    .unwrap_or(true)
            });
            // key_count も補正 (S3 spec compliance)
            if let Some(kc) = resp.output.key_count.as_mut() {
                *kc -= (before - contents.len()) as i32;
            }
        }
        Ok(resp)
    }
    /// v0.4 #17: filter S4-internal sidecars from versioned listings.
    /// v0.5 #34: when a [`crate::versioning::VersioningManager`] is
    /// attached AND the bucket is in a versioning-aware state, build
    /// the `Versions` / `DeleteMarkers` arrays directly from the
    /// in-memory chain (paginated + ordered the S3 way: key asc,
    /// version newest-first inside each key). Otherwise fall back to
    /// passthrough + sidecar-filter (legacy v0.4 behaviour).
    async fn list_object_versions(
        &self,
        req: S3Request<ListObjectVersionsInput>,
    ) -> S3Result<S3Response<ListObjectVersionsOutput>> {
        self.enforce_rate_limit(&req, &req.input.bucket)?;
        self.enforce_policy(&req, "s3:ListBucket", &req.input.bucket, None)?;
        // v0.5 #34: VersioningManager-owned path.
        if let Some(mgr) = self.versioning.as_ref()
            && mgr.state(&req.input.bucket) != crate::versioning::VersioningState::Unversioned
        {
            let max_keys = req.input.max_keys.unwrap_or(1000) as usize;
            let page = mgr.list_versions(
                &req.input.bucket,
                req.input.prefix.as_deref(),
                req.input.key_marker.as_deref(),
                req.input.version_id_marker.as_deref(),
                max_keys,
            );
            let versions: Vec<ObjectVersion> = page
                .versions
                .into_iter()
                .map(|e| ObjectVersion {
                    key: Some(e.key),
                    version_id: Some(e.version_id),
                    is_latest: Some(e.is_latest),
                    e_tag: Some(ETag::Strong(e.etag)),
                    size: Some(e.size as i64),
                    last_modified: Some(std::time::SystemTime::from(e.last_modified).into()),
                    ..Default::default()
                })
                .collect();
            let delete_markers: Vec<DeleteMarkerEntry> = page
                .delete_markers
                .into_iter()
                .map(|e| DeleteMarkerEntry {
                    key: Some(e.key),
                    version_id: Some(e.version_id),
                    is_latest: Some(e.is_latest),
                    last_modified: Some(std::time::SystemTime::from(e.last_modified).into()),
                    ..Default::default()
                })
                .collect();
            let output = ListObjectVersionsOutput {
                name: Some(req.input.bucket.clone()),
                prefix: req.input.prefix.clone(),
                key_marker: req.input.key_marker.clone(),
                version_id_marker: req.input.version_id_marker.clone(),
                max_keys: req.input.max_keys,
                versions: if versions.is_empty() {
                    None
                } else {
                    Some(versions)
                },
                delete_markers: if delete_markers.is_empty() {
                    None
                } else {
                    Some(delete_markers)
                },
                is_truncated: Some(page.is_truncated),
                next_key_marker: page.next_key_marker,
                next_version_id_marker: page.next_version_id_marker,
                ..Default::default()
            };
            return Ok(S3Response::new(output));
        }
        // Legacy passthrough path (v0.4 #17 sidecar filter retained).
        let mut resp = self.backend.list_object_versions(req).await?;
        if let Some(versions) = resp.output.versions.as_mut() {
            versions.retain(|v| {
                v.key
                    .as_ref()
                    .map(|k| !k.ends_with(".s4index") && !is_versioning_shadow_key(k))
                    .unwrap_or(true)
            });
        }
        if let Some(markers) = resp.output.delete_markers.as_mut() {
            markers.retain(|m| {
                m.key
                    .as_ref()
                    .map(|k| !k.ends_with(".s4index") && !is_versioning_shadow_key(k))
                    .unwrap_or(true)
            });
        }
        Ok(resp)
    }

    async fn create_multipart_upload(
        &self,
        mut req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        // Multipart object は per-part 圧縮 + frame 形式で書く。GET 時に
        // frame parse を起動するため、object metadata に flag を立てる。
        // codec は dispatcher の default kind を採用 (per-part 別 codec は Phase 2)。
        let codec_kind = self.registry.default_kind();
        let meta = req.input.metadata.get_or_insert_with(Default::default);
        meta.insert(META_MULTIPART.into(), "true".into());
        meta.insert(META_CODEC.into(), codec_kind.as_str().into());
        debug!(
            bucket = ?req.input.bucket,
            key = ?req.input.key,
            codec = codec_kind.as_str(),
            "S4 create_multipart_upload: marking object for per-part compression"
        );
        self.backend.create_multipart_upload(req).await
    }

    async fn upload_part(
        &self,
        mut req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        // 各 part を圧縮して frame header 付きで forward。GET 時に
        // `decompress_multipart` が frame iter で順に解凍する。
        // **per-part codec dispatch**: dispatcher が body 先頭 sample から
        // codec を選ぶので、parquet 風の mixed-content multipart で part ごとに
        // 最適 codec を使える (整数列 part → Bitcomp、text 列 part → zstd 等)。
        if let Some(blob) = req.input.body.take() {
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect upload_part body"))?;
            let sample_len = bytes.len().min(SAMPLE_BYTES);
            let codec_kind = self.dispatcher.pick(&bytes[..sample_len]).await;
            let original_size = bytes.len() as u64;
            let (compressed, manifest) = self
                .registry
                .compress(bytes, codec_kind)
                .await
                .map_err(internal("registry compress part"))?;
            let header = FrameHeader {
                codec: codec_kind,
                original_size,
                compressed_size: compressed.len() as u64,
                crc32c: manifest.crc32c,
            };
            let mut framed = BytesMut::with_capacity(FRAME_HEADER_BYTES + compressed.len());
            write_frame(&mut framed, header, &compressed);
            // v0.2 #5: heuristic-based padding skip for likely-final parts.
            //
            // AWS SDK / aws-cli / boto3 always send the final (and only the
            // final) part below the configured part_size. So if the raw user
            // part is already smaller than S3's 5 MiB multipart minimum, this
            // is overwhelmingly likely to be the final part — and the final
            // part is exempt from S3's size constraint. Skipping padding here
            // saves up to ~5 MiB per object on highly compressible workloads.
            //
            // If a misbehaving client sends a tiny **non-final** part, S3
            // itself rejects with EntityTooSmall at CompleteMultipartUpload —
            // identical outcome to a vanilla S3 PUT, just earlier than
            // padding-then-complete would catch it.
            let likely_final = original_size < S3_MULTIPART_MIN_PART_BYTES as u64;
            if !likely_final {
                pad_to_minimum(&mut framed, S3_MULTIPART_MIN_PART_BYTES);
            }
            let framed_bytes = framed.freeze();
            let new_len = framed_bytes.len() as i64;
            // 同じ wire 互換問題が multipart にもある (content-length / checksum)
            req.input.content_length = Some(new_len);
            req.input.checksum_algorithm = None;
            req.input.checksum_crc32 = None;
            req.input.checksum_crc32c = None;
            req.input.checksum_crc64nvme = None;
            req.input.checksum_sha1 = None;
            req.input.checksum_sha256 = None;
            req.input.content_md5 = None;
            req.input.body = Some(bytes_to_blob(framed_bytes));
            debug!(
                part_number = ?req.input.part_number,
                upload_id = ?req.input.upload_id,
                original_size,
                framed_size = new_len,
                "S4 upload_part: framed compressed payload"
            );
        }
        self.backend.upload_part(req).await
    }
    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        let resp = self.backend.complete_multipart_upload(req).await?;
        // CompleteMultipartUpload 成功 → 完成した object を full fetch して frame
        // index を build、`<key>.s4index` sidecar として保存。これで Range GET の
        // partial fetch path が利用可能になる (Range request の帯域節約)。
        // 注: 巨大 object の場合この pass は重いが、Range query は一度 sidecar が
        // できれば爆速になるので 1 回の cost は payback される
        let bucket_clone = bucket.clone();
        let key_clone = key.clone();
        let get_input = GetObjectInput {
            bucket: bucket_clone.clone(),
            key: key_clone.clone(),
            ..Default::default()
        };
        let get_req = S3Request {
            input: get_input,
            method: http::Method::GET,
            uri: format!("/{bucket_clone}/{key_clone}").parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        if let Ok(get_resp) = self.backend.get_object(get_req).await
            && let Some(blob) = get_resp.output.body
            && let Ok(body) = collect_blob(blob, self.max_body_bytes).await
            && let Ok(index) = build_index_from_body(&body)
        {
            self.write_sidecar(&bucket, &key, &index).await;
        }
        Ok(resp)
    }
    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        self.backend.abort_multipart_upload(req).await
    }
    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        self.backend.list_multipart_uploads(req).await
    }
    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        self.backend.list_parts(req).await
    }

    // =========================================================================
    // Phase 2 — pure passthrough delegations。S4 はこれらに対して圧縮 hook を
    // 持たないので、backend (= AWS S3) の動作と完全に同一。
    //
    // 既知の制限事項:
    // - copy_object / upload_part_copy: source object が S4-compressed の場合、
    //   backend が bytes を copy するだけなので metadata (s4-codec etc) も一緒に
    //   coppied される (AWS S3 default = MetadataDirective COPY)。GET は manifest
    //   経由で正しく decompress できる。MetadataDirective REPLACE で上書き
    //   されると圧縮 metadata が消えて壊れる — 顧客側の運用で注意
    // - list_object_versions: versioning enabled bucket では各 version も S4
    //   metadata を維持する。古い version も S4 経由で正しく GET できる。
    // =========================================================================

    // ---- Object ACL / tagging / attributes ----
    async fn get_object_acl(
        &self,
        req: S3Request<GetObjectAclInput>,
    ) -> S3Result<S3Response<GetObjectAclOutput>> {
        self.backend.get_object_acl(req).await
    }
    async fn put_object_acl(
        &self,
        req: S3Request<PutObjectAclInput>,
    ) -> S3Result<S3Response<PutObjectAclOutput>> {
        self.backend.put_object_acl(req).await
    }
    async fn get_object_tagging(
        &self,
        req: S3Request<GetObjectTaggingInput>,
    ) -> S3Result<S3Response<GetObjectTaggingOutput>> {
        self.backend.get_object_tagging(req).await
    }
    async fn put_object_tagging(
        &self,
        req: S3Request<PutObjectTaggingInput>,
    ) -> S3Result<S3Response<PutObjectTaggingOutput>> {
        self.backend.put_object_tagging(req).await
    }
    async fn delete_object_tagging(
        &self,
        req: S3Request<DeleteObjectTaggingInput>,
    ) -> S3Result<S3Response<DeleteObjectTaggingOutput>> {
        self.backend.delete_object_tagging(req).await
    }
    async fn get_object_attributes(
        &self,
        req: S3Request<GetObjectAttributesInput>,
    ) -> S3Result<S3Response<GetObjectAttributesOutput>> {
        self.backend.get_object_attributes(req).await
    }
    async fn restore_object(
        &self,
        req: S3Request<RestoreObjectInput>,
    ) -> S3Result<S3Response<RestoreObjectOutput>> {
        self.backend.restore_object(req).await
    }
    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        // v0.2 #6: byte-range aware copy when the source is S4-framed.
        //
        // For a framed source (multipart upload OR single-PUT framed-v2),
        // a naive byte-range passthrough would copy compressed bytes that
        // don't align with S4 frame boundaries — silently corrupting the
        // result. Instead we GET the source through S4 (which handles
        // decompression + Range), re-compress + re-frame as a new part,
        // and forward as upload_part. For non-framed sources (S4-untouched
        // raw objects), passthrough is correct and we keep the original
        // (cheaper) code path.
        let CopySource::Bucket {
            bucket: src_bucket,
            key: src_key,
            ..
        } = &req.input.copy_source
        else {
            return self.backend.upload_part_copy(req).await;
        };
        let src_bucket = src_bucket.to_string();
        let src_key = src_key.to_string();

        // Probe metadata to decide whether the source needs S4-aware copy.
        let head_input = HeadObjectInput {
            bucket: src_bucket.clone(),
            key: src_key.clone(),
            ..Default::default()
        };
        let head_req = S3Request {
            input: head_input,
            method: http::Method::HEAD,
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let needs_s4_copy = match self.backend.head_object(head_req).await {
            Ok(h) => {
                is_multipart_object(&h.output.metadata) || is_framed_v2_object(&h.output.metadata)
            }
            Err(_) => false,
        };
        if !needs_s4_copy {
            return self.backend.upload_part_copy(req).await;
        }

        // Resolve the optional source byte range to pass to GET.
        let source_range = req
            .input
            .copy_source_range
            .as_ref()
            .map(|r| parse_copy_source_range(r))
            .transpose()
            .map_err(|e| S3Error::with_message(S3ErrorCode::InvalidRange, e))?;

        // GET source via S4 (handles decompression + sidecar partial fetch
        // when range is present). The result is the requested user-visible
        // byte range, fully decompressed.
        let mut get_input = GetObjectInput {
            bucket: src_bucket.clone(),
            key: src_key.clone(),
            ..Default::default()
        };
        get_input.range = source_range;
        let get_req = S3Request {
            input: get_input,
            method: http::Method::GET,
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let get_resp = self.get_object(get_req).await?;
        let blob = get_resp.output.body.ok_or_else(|| {
            S3Error::with_message(
                S3ErrorCode::InternalError,
                "upload_part_copy: empty body from source GET",
            )
        })?;
        let bytes = collect_blob(blob, self.max_body_bytes)
            .await
            .map_err(internal("collect upload_part_copy source body"))?;

        // Compress + frame as a fresh part (mirrors upload_part path).
        let sample_len = bytes.len().min(SAMPLE_BYTES);
        let codec_kind = self.dispatcher.pick(&bytes[..sample_len]).await;
        let original_size = bytes.len() as u64;
        let (compressed, manifest) = self
            .registry
            .compress(bytes, codec_kind)
            .await
            .map_err(internal("registry compress upload_part_copy"))?;
        let header = FrameHeader {
            codec: codec_kind,
            original_size,
            compressed_size: compressed.len() as u64,
            crc32c: manifest.crc32c,
        };
        let mut framed = BytesMut::with_capacity(FRAME_HEADER_BYTES + compressed.len());
        write_frame(&mut framed, header, &compressed);
        let likely_final = original_size < S3_MULTIPART_MIN_PART_BYTES as u64;
        if !likely_final {
            pad_to_minimum(&mut framed, S3_MULTIPART_MIN_PART_BYTES);
        }
        let framed_bytes = framed.freeze();
        let framed_len = framed_bytes.len() as i64;

        // Forward as upload_part to the destination multipart upload.
        let part_input = UploadPartInput {
            bucket: req.input.bucket.clone(),
            key: req.input.key.clone(),
            part_number: req.input.part_number,
            upload_id: req.input.upload_id.clone(),
            body: Some(bytes_to_blob(framed_bytes)),
            content_length: Some(framed_len),
            ..Default::default()
        };
        let part_req = S3Request {
            input: part_input,
            method: http::Method::PUT,
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let upload_resp = self.backend.upload_part(part_req).await?;

        let copy_output = UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: upload_resp.output.e_tag.clone(),
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(S3Response::new(copy_output))
    }

    // ---- Object lock / retention / legal hold (v0.5 #30) ----
    //
    // When an `ObjectLockManager` is attached the configuration / per-object
    // state lives in the manager and these handlers serve directly from it;
    // when no manager is attached they fall back to the backend (legacy
    // passthrough so v0.4 deployments are unaffected).
    async fn get_object_lock_configuration(
        &self,
        req: S3Request<GetObjectLockConfigurationInput>,
    ) -> S3Result<S3Response<GetObjectLockConfigurationOutput>> {
        if let Some(mgr) = self.object_lock.as_ref() {
            let cfg = mgr.bucket_default(&req.input.bucket).map(|d| {
                ObjectLockConfiguration {
                    object_lock_enabled: Some(ObjectLockEnabled::from_static(
                        ObjectLockEnabled::ENABLED,
                    )),
                    rule: Some(ObjectLockRule {
                        default_retention: Some(DefaultRetention {
                            days: Some(d.retention_days as i32),
                            mode: Some(ObjectLockRetentionMode::from_static(
                                match d.mode {
                                    crate::object_lock::LockMode::Governance => {
                                        ObjectLockRetentionMode::GOVERNANCE
                                    }
                                    crate::object_lock::LockMode::Compliance => {
                                        ObjectLockRetentionMode::COMPLIANCE
                                    }
                                },
                            )),
                            years: None,
                        }),
                    }),
                }
            });
            let output = GetObjectLockConfigurationOutput {
                object_lock_configuration: cfg,
            };
            return Ok(S3Response::new(output));
        }
        self.backend.get_object_lock_configuration(req).await
    }
    async fn put_object_lock_configuration(
        &self,
        req: S3Request<PutObjectLockConfigurationInput>,
    ) -> S3Result<S3Response<PutObjectLockConfigurationOutput>> {
        if let Some(mgr) = self.object_lock.as_ref() {
            let bucket = req.input.bucket.clone();
            if let Some(cfg) = req.input.object_lock_configuration.as_ref()
                && let Some(rule) = cfg.rule.as_ref()
                && let Some(d) = rule.default_retention.as_ref()
            {
                let mode = d
                    .mode
                    .as_ref()
                    .and_then(|m| crate::object_lock::LockMode::from_aws_str(m.as_str()))
                    .ok_or_else(|| {
                        S3Error::with_message(
                            S3ErrorCode::InvalidRequest,
                            "Object Lock default retention requires a valid Mode (GOVERNANCE | COMPLIANCE)",
                        )
                    })?;
                // S3 spec: exactly one of Days / Years (we accept Days
                // outright and convert Years → Days for storage; Years
                // is just a UX shorthand on the wire).
                let days: u32 = match (d.days, d.years) {
                    (Some(d), None) if d > 0 => d as u32,
                    (None, Some(y)) if y > 0 => (y as u32).saturating_mul(365),
                    _ => {
                        return Err(S3Error::with_message(
                            S3ErrorCode::InvalidRequest,
                            "Object Lock default retention requires exactly one of Days or Years (positive integer)",
                        ));
                    }
                };
                mgr.set_bucket_default(
                    &bucket,
                    crate::object_lock::BucketObjectLockDefault {
                        mode,
                        retention_days: days,
                    },
                );
            }
            return Ok(S3Response::new(PutObjectLockConfigurationOutput::default()));
        }
        self.backend.put_object_lock_configuration(req).await
    }
    async fn get_object_legal_hold(
        &self,
        req: S3Request<GetObjectLegalHoldInput>,
    ) -> S3Result<S3Response<GetObjectLegalHoldOutput>> {
        if let Some(mgr) = self.object_lock.as_ref() {
            let on = mgr
                .get(&req.input.bucket, &req.input.key)
                .map(|s| s.legal_hold_on)
                .unwrap_or(false);
            let status = ObjectLockLegalHoldStatus::from_static(if on {
                ObjectLockLegalHoldStatus::ON
            } else {
                ObjectLockLegalHoldStatus::OFF
            });
            let output = GetObjectLegalHoldOutput {
                legal_hold: Some(ObjectLockLegalHold {
                    status: Some(status),
                }),
            };
            return Ok(S3Response::new(output));
        }
        self.backend.get_object_legal_hold(req).await
    }
    async fn put_object_legal_hold(
        &self,
        req: S3Request<PutObjectLegalHoldInput>,
    ) -> S3Result<S3Response<PutObjectLegalHoldOutput>> {
        if let Some(mgr) = self.object_lock.as_ref() {
            let on = req
                .input
                .legal_hold
                .as_ref()
                .and_then(|h| h.status.as_ref())
                .map(|s| s.as_str().eq_ignore_ascii_case("ON"))
                .unwrap_or(false);
            mgr.set_legal_hold(&req.input.bucket, &req.input.key, on);
            return Ok(S3Response::new(PutObjectLegalHoldOutput::default()));
        }
        self.backend.put_object_legal_hold(req).await
    }
    async fn get_object_retention(
        &self,
        req: S3Request<GetObjectRetentionInput>,
    ) -> S3Result<S3Response<GetObjectRetentionOutput>> {
        if let Some(mgr) = self.object_lock.as_ref() {
            let retention = mgr
                .get(&req.input.bucket, &req.input.key)
                .filter(|s| s.mode.is_some() || s.retain_until.is_some())
                .map(|s| {
                    let mode = s.mode.map(|m| {
                        ObjectLockRetentionMode::from_static(match m {
                            crate::object_lock::LockMode::Governance => {
                                ObjectLockRetentionMode::GOVERNANCE
                            }
                            crate::object_lock::LockMode::Compliance => {
                                ObjectLockRetentionMode::COMPLIANCE
                            }
                        })
                    });
                    let until = s.retain_until.map(chrono_utc_to_timestamp);
                    ObjectLockRetention {
                        mode,
                        retain_until_date: until,
                    }
                });
            let output = GetObjectRetentionOutput { retention };
            return Ok(S3Response::new(output));
        }
        self.backend.get_object_retention(req).await
    }
    async fn put_object_retention(
        &self,
        req: S3Request<PutObjectRetentionInput>,
    ) -> S3Result<S3Response<PutObjectRetentionOutput>> {
        if let Some(mgr) = self.object_lock.as_ref() {
            let bucket = req.input.bucket.clone();
            let key = req.input.key.clone();
            let bypass = req.input.bypass_governance_retention.unwrap_or(false);
            let retention = req.input.retention.as_ref().ok_or_else(|| {
                S3Error::with_message(
                    S3ErrorCode::InvalidRequest,
                    "PutObjectRetention requires a Retention element",
                )
            })?;
            let new_mode = retention
                .mode
                .as_ref()
                .and_then(|m| crate::object_lock::LockMode::from_aws_str(m.as_str()));
            let new_until = retention
                .retain_until_date
                .as_ref()
                .map(timestamp_to_chrono_utc)
                .unwrap_or(None);
            let now = chrono::Utc::now();
            let existing = mgr.get(&bucket, &key).unwrap_or_default();
            // S3 immutability rules:
            //   - Compliance is one-way: once set, mode cannot move to
            //     Governance, and retain-until cannot be shortened.
            //   - Governance can be lengthened freely; shortened only
            //     with bypass=true.
            if let Some(existing_mode) = existing.mode
                && existing_mode == crate::object_lock::LockMode::Compliance
                && existing.is_locked(now)
            {
                if matches!(new_mode, Some(crate::object_lock::LockMode::Governance)) {
                    return Err(S3Error::with_message(
                        S3ErrorCode::AccessDenied,
                        "Cannot downgrade Compliance retention to Governance while lock is active",
                    ));
                }
                if let (Some(prev), Some(next)) = (existing.retain_until, new_until)
                    && next < prev
                {
                    return Err(S3Error::with_message(
                        S3ErrorCode::AccessDenied,
                        "Cannot shorten Compliance retention while lock is active",
                    ));
                }
            }
            if let Some(existing_mode) = existing.mode
                && existing_mode == crate::object_lock::LockMode::Governance
                && existing.is_locked(now)
                && !bypass
                && let (Some(prev), Some(next)) = (existing.retain_until, new_until)
                && next < prev
            {
                return Err(S3Error::with_message(
                    S3ErrorCode::AccessDenied,
                    "Shortening Governance retention requires x-amz-bypass-governance-retention: true",
                ));
            }
            let mut state = existing;
            if new_mode.is_some() {
                state.mode = new_mode;
            }
            if new_until.is_some() {
                state.retain_until = new_until;
            }
            mgr.set(&bucket, &key, state);
            return Ok(S3Response::new(PutObjectRetentionOutput::default()));
        }
        self.backend.put_object_retention(req).await
    }

    // ---- Versioning ----
    // list_object_versions is implemented above in the compression-hook
    // section so it filters S4-internal sidecars (v0.4 #17) AND, when a
    // VersioningManager is attached (v0.5 #34), serves chains directly
    // from the in-memory index.
    async fn get_bucket_versioning(
        &self,
        req: S3Request<GetBucketVersioningInput>,
    ) -> S3Result<S3Response<GetBucketVersioningOutput>> {
        // v0.5 #34: when a VersioningManager is attached, the bucket's
        // versioning state lives in the manager (= S4-server's
        // authoritative source). Pass-through hits the backend only
        // when no manager is configured (legacy v0.4 behaviour).
        if let Some(mgr) = self.versioning.as_ref() {
            let output = match mgr.state(&req.input.bucket).as_aws_status() {
                Some(s) => GetBucketVersioningOutput {
                    status: Some(BucketVersioningStatus::from(s.to_owned())),
                    ..Default::default()
                },
                None => GetBucketVersioningOutput::default(),
            };
            return Ok(S3Response::new(output));
        }
        self.backend.get_bucket_versioning(req).await
    }
    async fn put_bucket_versioning(
        &self,
        req: S3Request<PutBucketVersioningInput>,
    ) -> S3Result<S3Response<PutBucketVersioningOutput>> {
        // v0.5 #34: stash the new state in the manager, then forward to
        // the backend so any downstream that *also* tracks state
        // (e.g. a real S3 backend) stays in sync. Manager-attached but
        // backend rejection is treated as a soft-fail (state is still
        // owned by the manager).
        if let Some(mgr) = self.versioning.as_ref() {
            let new_state = match req
                .input
                .versioning_configuration
                .status
                .as_ref()
                .map(|s| s.as_str())
            {
                Some(s) if s.eq_ignore_ascii_case("Enabled") => {
                    crate::versioning::VersioningState::Enabled
                }
                Some(s) if s.eq_ignore_ascii_case("Suspended") => {
                    crate::versioning::VersioningState::Suspended
                }
                _ => crate::versioning::VersioningState::Unversioned,
            };
            mgr.set_state(&req.input.bucket, new_state);
            return Ok(S3Response::new(PutBucketVersioningOutput::default()));
        }
        self.backend.put_bucket_versioning(req).await
    }

    // ---- Bucket location ----
    async fn get_bucket_location(
        &self,
        req: S3Request<GetBucketLocationInput>,
    ) -> S3Result<S3Response<GetBucketLocationOutput>> {
        self.backend.get_bucket_location(req).await
    }

    // ---- Bucket policy ----
    async fn get_bucket_policy(
        &self,
        req: S3Request<GetBucketPolicyInput>,
    ) -> S3Result<S3Response<GetBucketPolicyOutput>> {
        self.backend.get_bucket_policy(req).await
    }
    async fn put_bucket_policy(
        &self,
        req: S3Request<PutBucketPolicyInput>,
    ) -> S3Result<S3Response<PutBucketPolicyOutput>> {
        self.backend.put_bucket_policy(req).await
    }
    async fn delete_bucket_policy(
        &self,
        req: S3Request<DeleteBucketPolicyInput>,
    ) -> S3Result<S3Response<DeleteBucketPolicyOutput>> {
        self.backend.delete_bucket_policy(req).await
    }
    async fn get_bucket_policy_status(
        &self,
        req: S3Request<GetBucketPolicyStatusInput>,
    ) -> S3Result<S3Response<GetBucketPolicyStatusOutput>> {
        self.backend.get_bucket_policy_status(req).await
    }

    // ---- Bucket ACL ----
    async fn get_bucket_acl(
        &self,
        req: S3Request<GetBucketAclInput>,
    ) -> S3Result<S3Response<GetBucketAclOutput>> {
        self.backend.get_bucket_acl(req).await
    }
    async fn put_bucket_acl(
        &self,
        req: S3Request<PutBucketAclInput>,
    ) -> S3Result<S3Response<PutBucketAclOutput>> {
        self.backend.put_bucket_acl(req).await
    }

    // ---- Bucket CORS ----
    async fn get_bucket_cors(
        &self,
        req: S3Request<GetBucketCorsInput>,
    ) -> S3Result<S3Response<GetBucketCorsOutput>> {
        self.backend.get_bucket_cors(req).await
    }
    async fn put_bucket_cors(
        &self,
        req: S3Request<PutBucketCorsInput>,
    ) -> S3Result<S3Response<PutBucketCorsOutput>> {
        self.backend.put_bucket_cors(req).await
    }
    async fn delete_bucket_cors(
        &self,
        req: S3Request<DeleteBucketCorsInput>,
    ) -> S3Result<S3Response<DeleteBucketCorsOutput>> {
        self.backend.delete_bucket_cors(req).await
    }

    // ---- Bucket lifecycle ----
    async fn get_bucket_lifecycle_configuration(
        &self,
        req: S3Request<GetBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketLifecycleConfigurationOutput>> {
        self.backend.get_bucket_lifecycle_configuration(req).await
    }
    async fn put_bucket_lifecycle_configuration(
        &self,
        req: S3Request<PutBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketLifecycleConfigurationOutput>> {
        self.backend.put_bucket_lifecycle_configuration(req).await
    }
    async fn delete_bucket_lifecycle(
        &self,
        req: S3Request<DeleteBucketLifecycleInput>,
    ) -> S3Result<S3Response<DeleteBucketLifecycleOutput>> {
        self.backend.delete_bucket_lifecycle(req).await
    }

    // ---- Bucket tagging ----
    async fn get_bucket_tagging(
        &self,
        req: S3Request<GetBucketTaggingInput>,
    ) -> S3Result<S3Response<GetBucketTaggingOutput>> {
        self.backend.get_bucket_tagging(req).await
    }
    async fn put_bucket_tagging(
        &self,
        req: S3Request<PutBucketTaggingInput>,
    ) -> S3Result<S3Response<PutBucketTaggingOutput>> {
        self.backend.put_bucket_tagging(req).await
    }
    async fn delete_bucket_tagging(
        &self,
        req: S3Request<DeleteBucketTaggingInput>,
    ) -> S3Result<S3Response<DeleteBucketTaggingOutput>> {
        self.backend.delete_bucket_tagging(req).await
    }

    // ---- Bucket encryption ----
    async fn get_bucket_encryption(
        &self,
        req: S3Request<GetBucketEncryptionInput>,
    ) -> S3Result<S3Response<GetBucketEncryptionOutput>> {
        self.backend.get_bucket_encryption(req).await
    }
    async fn put_bucket_encryption(
        &self,
        req: S3Request<PutBucketEncryptionInput>,
    ) -> S3Result<S3Response<PutBucketEncryptionOutput>> {
        self.backend.put_bucket_encryption(req).await
    }
    async fn delete_bucket_encryption(
        &self,
        req: S3Request<DeleteBucketEncryptionInput>,
    ) -> S3Result<S3Response<DeleteBucketEncryptionOutput>> {
        self.backend.delete_bucket_encryption(req).await
    }

    // ---- Bucket logging ----
    async fn get_bucket_logging(
        &self,
        req: S3Request<GetBucketLoggingInput>,
    ) -> S3Result<S3Response<GetBucketLoggingOutput>> {
        self.backend.get_bucket_logging(req).await
    }
    async fn put_bucket_logging(
        &self,
        req: S3Request<PutBucketLoggingInput>,
    ) -> S3Result<S3Response<PutBucketLoggingOutput>> {
        self.backend.put_bucket_logging(req).await
    }

    // ---- Bucket notification ----
    async fn get_bucket_notification_configuration(
        &self,
        req: S3Request<GetBucketNotificationConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketNotificationConfigurationOutput>> {
        self.backend
            .get_bucket_notification_configuration(req)
            .await
    }
    async fn put_bucket_notification_configuration(
        &self,
        req: S3Request<PutBucketNotificationConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketNotificationConfigurationOutput>> {
        self.backend
            .put_bucket_notification_configuration(req)
            .await
    }

    // ---- Bucket request payment ----
    async fn get_bucket_request_payment(
        &self,
        req: S3Request<GetBucketRequestPaymentInput>,
    ) -> S3Result<S3Response<GetBucketRequestPaymentOutput>> {
        self.backend.get_bucket_request_payment(req).await
    }
    async fn put_bucket_request_payment(
        &self,
        req: S3Request<PutBucketRequestPaymentInput>,
    ) -> S3Result<S3Response<PutBucketRequestPaymentOutput>> {
        self.backend.put_bucket_request_payment(req).await
    }

    // ---- Bucket website ----
    async fn get_bucket_website(
        &self,
        req: S3Request<GetBucketWebsiteInput>,
    ) -> S3Result<S3Response<GetBucketWebsiteOutput>> {
        self.backend.get_bucket_website(req).await
    }
    async fn put_bucket_website(
        &self,
        req: S3Request<PutBucketWebsiteInput>,
    ) -> S3Result<S3Response<PutBucketWebsiteOutput>> {
        self.backend.put_bucket_website(req).await
    }
    async fn delete_bucket_website(
        &self,
        req: S3Request<DeleteBucketWebsiteInput>,
    ) -> S3Result<S3Response<DeleteBucketWebsiteOutput>> {
        self.backend.delete_bucket_website(req).await
    }

    // ---- Bucket replication ----
    async fn get_bucket_replication(
        &self,
        req: S3Request<GetBucketReplicationInput>,
    ) -> S3Result<S3Response<GetBucketReplicationOutput>> {
        self.backend.get_bucket_replication(req).await
    }
    async fn put_bucket_replication(
        &self,
        req: S3Request<PutBucketReplicationInput>,
    ) -> S3Result<S3Response<PutBucketReplicationOutput>> {
        self.backend.put_bucket_replication(req).await
    }
    async fn delete_bucket_replication(
        &self,
        req: S3Request<DeleteBucketReplicationInput>,
    ) -> S3Result<S3Response<DeleteBucketReplicationOutput>> {
        self.backend.delete_bucket_replication(req).await
    }

    // ---- Bucket accelerate ----
    async fn get_bucket_accelerate_configuration(
        &self,
        req: S3Request<GetBucketAccelerateConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketAccelerateConfigurationOutput>> {
        self.backend.get_bucket_accelerate_configuration(req).await
    }
    async fn put_bucket_accelerate_configuration(
        &self,
        req: S3Request<PutBucketAccelerateConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketAccelerateConfigurationOutput>> {
        self.backend.put_bucket_accelerate_configuration(req).await
    }

    // ---- Bucket ownership controls ----
    async fn get_bucket_ownership_controls(
        &self,
        req: S3Request<GetBucketOwnershipControlsInput>,
    ) -> S3Result<S3Response<GetBucketOwnershipControlsOutput>> {
        self.backend.get_bucket_ownership_controls(req).await
    }
    async fn put_bucket_ownership_controls(
        &self,
        req: S3Request<PutBucketOwnershipControlsInput>,
    ) -> S3Result<S3Response<PutBucketOwnershipControlsOutput>> {
        self.backend.put_bucket_ownership_controls(req).await
    }
    async fn delete_bucket_ownership_controls(
        &self,
        req: S3Request<DeleteBucketOwnershipControlsInput>,
    ) -> S3Result<S3Response<DeleteBucketOwnershipControlsOutput>> {
        self.backend.delete_bucket_ownership_controls(req).await
    }

    // ---- Public access block ----
    async fn get_public_access_block(
        &self,
        req: S3Request<GetPublicAccessBlockInput>,
    ) -> S3Result<S3Response<GetPublicAccessBlockOutput>> {
        self.backend.get_public_access_block(req).await
    }
    async fn put_public_access_block(
        &self,
        req: S3Request<PutPublicAccessBlockInput>,
    ) -> S3Result<S3Response<PutPublicAccessBlockOutput>> {
        self.backend.put_public_access_block(req).await
    }
    async fn delete_public_access_block(
        &self,
        req: S3Request<DeletePublicAccessBlockInput>,
    ) -> S3Result<S3Response<DeletePublicAccessBlockOutput>> {
        self.backend.delete_public_access_block(req).await
    }
}

// ---------------------------------------------------------------------------
// v0.5 #33: SigV4a (asymmetric ECDSA-P256) integration hook.
//
// Kept as a self-contained block at the bottom of the file so it doesn't
// touch the existing `S4Service` struct, `new()`, or any of the per-op
// handlers above. The hook is wired in by the binary at server-build time
// as a hyper middleware layer (see `main.rs`), NOT inside `S4Service`.
//
// Lifecycle:
//   1. `SigV4aGate::new(store)` is constructed once at boot from the
//      operator-supplied credential directory.
//   2. For each incoming request, `SigV4aGate::pre_route(&req,
//      &requested_region, &canonical_request_bytes)` is invoked BEFORE
//      the request hits the S3 framework. If the request claims SigV4a
//      and verifies, control returns to the framework. Otherwise a 403
//      `SignatureDoesNotMatch` is produced.
//   3. Plain SigV4 (HMAC-SHA256) requests pass through untouched.
// ---------------------------------------------------------------------------

/// Gate that fronts the S3 service path with SigV4a verification (v0.5 #33).
///
/// Wraps a [`crate::sigv4a::SigV4aCredentialStore`] and exposes a single
/// `pre_route` entry point that returns `Ok(())` for both
/// "request is plain SigV4 — pass through" and "request is SigV4a and
/// verified", and an `Err(...)` containing a 403-equivalent diagnostic
/// otherwise. Cheap to clone (the inner store is `Arc`-backed).
#[derive(Debug, Clone)]
pub struct SigV4aGate {
    store: crate::sigv4a::SharedSigV4aCredentialStore,
}

impl SigV4aGate {
    #[must_use]
    pub fn new(store: crate::sigv4a::SharedSigV4aCredentialStore) -> Self {
        Self { store }
    }

    /// Inspect an incoming HTTP request. Behaviour:
    ///
    /// - Not SigV4a (no `X-Amz-Region-Set` and no SigV4a `Authorization`
    ///   prefix) → returns `Ok(())`; the framework's existing SigV4
    ///   path handles the request.
    /// - SigV4a + valid signature + region match → `Ok(())`.
    /// - SigV4a + unknown access-key-id → `Err` with `InvalidAccessKeyId`.
    /// - SigV4a + bad signature / region mismatch → `Err` with
    ///   `SignatureDoesNotMatch`.
    ///
    /// `canonical_request_bytes` is the SigV4a string-to-sign (or
    /// canonical-request bytes; the caller decides) that the framework
    /// has already produced for this request. Keeping it as a parameter
    /// instead of rebuilding it inside the hook avoids duplicating the
    /// canonicalisation logic.
    pub fn pre_route<B>(
        &self,
        req: &http::Request<B>,
        requested_region: &str,
        canonical_request_bytes: &[u8],
    ) -> Result<(), SigV4aGateError> {
        if !crate::sigv4a::detect(req) {
            return Ok(());
        }
        let auth_hdr = req
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(SigV4aGateError::MissingAuthorization)?;
        let parsed = crate::sigv4a::parse_authorization_header(auth_hdr)
            .ok_or(SigV4aGateError::MalformedAuthorization)?;
        let region_set = req
            .headers()
            .get(crate::sigv4a::REGION_SET_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("*");
        let key = self
            .store
            .get(&parsed.access_key_id)
            .ok_or_else(|| SigV4aGateError::UnknownAccessKey(parsed.access_key_id.clone()))?;
        crate::sigv4a::verify(
            &crate::sigv4a::CanonicalRequest::new(canonical_request_bytes),
            &parsed.signature_der,
            key,
            region_set,
            requested_region,
        )
        .map_err(SigV4aGateError::Verify)?;
        Ok(())
    }
}

/// Failure modes from [`SigV4aGate::pre_route`]. All variants map to
/// HTTP 403 with one of the two AWS-standard error codes
/// (`InvalidAccessKeyId` or `SignatureDoesNotMatch`).
#[derive(Debug, thiserror::Error)]
pub enum SigV4aGateError {
    #[error("missing Authorization header")]
    MissingAuthorization,
    #[error("malformed SigV4a Authorization header")]
    MalformedAuthorization,
    #[error("unknown SigV4a access-key-id: {0}")]
    UnknownAccessKey(String),
    #[error("SigV4a verification failed: {0}")]
    Verify(#[source] crate::sigv4a::SigV4aError),
}

impl SigV4aGateError {
    /// AWS S3 error code that should accompany a 403 response.
    #[must_use]
    pub fn s3_error_code(&self) -> &'static str {
        match self {
            Self::UnknownAccessKey(_) => "InvalidAccessKeyId",
            _ => "SignatureDoesNotMatch",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_via_metadata() {
        let original = ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size: 1234,
            compressed_size: 567,
            crc32c: 0xdead_beef,
        };
        let mut meta: Option<Metadata> = None;
        write_manifest(&mut meta, &original);
        let extracted = extract_manifest(&meta).expect("manifest must round-trip");
        assert_eq!(extracted.codec, original.codec);
        assert_eq!(extracted.original_size, original.original_size);
        assert_eq!(extracted.compressed_size, original.compressed_size);
        assert_eq!(extracted.crc32c, original.crc32c);
    }

    #[test]
    fn missing_metadata_yields_none() {
        let meta: Option<Metadata> = None;
        assert!(extract_manifest(&meta).is_none());
    }

    #[test]
    fn partial_metadata_yields_none() {
        let mut meta = Metadata::new();
        meta.insert(META_CODEC.into(), "cpu-zstd".into());
        let opt = Some(meta);
        assert!(extract_manifest(&opt).is_none());
    }

    #[test]
    fn parse_copy_source_range_basic() {
        let r = parse_copy_source_range("bytes=10-20").unwrap();
        match r {
            s3s::dto::Range::Int { first, last } => {
                assert_eq!(first, 10);
                assert_eq!(last, Some(20));
            }
            _ => panic!("expected Int range"),
        }
    }

    #[test]
    fn parse_copy_source_range_rejects_inverted() {
        let err = parse_copy_source_range("bytes=20-10").unwrap_err();
        assert!(err.contains("last < first"));
    }

    #[test]
    fn parse_copy_source_range_rejects_missing_prefix() {
        let err = parse_copy_source_range("10-20").unwrap_err();
        assert!(err.contains("must start with 'bytes='"));
    }

    #[test]
    fn parse_copy_source_range_rejects_open_ended() {
        // S3 upload_part_copy spec requires N-M (closed); suffix and
        // open-ended forms are not allowed for this header.
        assert!(parse_copy_source_range("bytes=10-").is_err());
        assert!(parse_copy_source_range("bytes=-10").is_err());
    }
}
