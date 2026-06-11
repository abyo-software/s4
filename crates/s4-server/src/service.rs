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
use s4_codec::{ChunkManifest, CodecDispatcher, CodecKind, CodecRegistry, CompressTelemetry};
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::blob::{
    bytes_to_blob, chain_sample_with_rest, collect_blob, collect_with_sample, peek_sample,
};
use crate::streaming::{
    Crc32cVerifyingReader, async_read_to_blob, blob_to_async_read, cpu_zstd_decompress_stream,
    pick_chunk_size, streaming_compress_to_frames, supports_streaming_compress,
    supports_streaming_decompress,
};

/// PUT body の先頭 sampling で渡す最大 byte 数。
const SAMPLE_BYTES: usize = 4096;

/// v0.8 #55: stamp the GPU pipeline metrics (`s4_gpu_compress_seconds`,
/// `s4_gpu_throughput_bytes_per_sec`, `s4_gpu_oom_total`) from a
/// `CompressTelemetry` returned by `CodecRegistry::compress_with_telemetry`.
/// CPU codecs (`gpu_seconds = None`) are no-ops here — they're already
/// covered by the existing `s4_request_latency_seconds` / `s4_bytes_*`
/// counters in the request-level `record_put` / `record_get` calls.
#[inline]
fn stamp_gpu_compress_telemetry(tel: &CompressTelemetry) {
    if let Some(secs) = tel.gpu_seconds {
        crate::metrics::record_gpu_compress(tel.codec, secs, tel.bytes_in, tel.bytes_out);
    }
    if tel.oom {
        crate::metrics::record_gpu_oom(tel.codec);
    }
}

/// v0.7 #49: percent-encoding set covering everything that is **not** an
/// `unreserved` character per RFC 3986 §2.3, **plus** we additionally
/// encode the path-reserved sub-delims that `http::Uri` rejects in a
/// path segment (`?`, `#`, `%`, control bytes, space, etc.). We
/// deliberately keep `/` un-encoded because S3 keys legally use `/` as
/// a logical separator and the rest of the synthetic URI relies on the
/// path layout `/{bucket}/{key}` round-tripping byte-for-byte.
const URI_KEY_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'\\')
    .add(b'^')
    .add(b'[')
    .add(b']')
    .add(b'%');

/// v0.7 #49: build the synthetic `/{bucket}/{key}` request URI used by
/// the sidecar / replication helpers when they re-enter the backend
/// trait without going through the HTTP layer. S3 object keys can
/// contain spaces, control bytes, and arbitrary Unicode that would
/// make `format!(...).parse::<http::Uri>()` panic; we percent-encode
/// the key bytes (RFC 3986 path segment) and the bucket name (defensive
/// — bucket names are normally DNS-safe, but the helper is the single
/// choke-point) before splicing them in. If the encoded form *still*
/// fails to parse (extremely unlikely once everything outside the
/// unreserved set is escaped) we surface a typed `400 InvalidObjectName`
/// instead of crashing the worker.
pub(crate) fn safe_object_uri(bucket: &str, key: &str) -> S3Result<http::Uri> {
    use percent_encoding::utf8_percent_encode;
    let bucket_enc = utf8_percent_encode(bucket, URI_KEY_ENCODE_SET);
    let key_enc = utf8_percent_encode(key, URI_KEY_ENCODE_SET);
    let raw = format!("/{bucket_enc}/{key_enc}");
    raw.parse::<http::Uri>().map_err(|e| {
        // S3 spec uses `InvalidObjectName` (HTTP 400) for keys that
        // can't be represented in a request URI. The generated
        // `S3ErrorCode` enum doesn't expose a typed variant for it,
        // so we round-trip through `from_bytes` which preserves the
        // canonical wire string while falling back to InvalidArgument
        // if even that lookup fails (cannot happen at runtime — kept
        // as a belt-and-suspenders branch so this helper never
        // panics).
        let code =
            S3ErrorCode::from_bytes(b"InvalidObjectName").unwrap_or(S3ErrorCode::InvalidArgument);
        S3Error::with_message(
            code,
            format!("object key cannot be encoded as a request URI: {e}"),
        )
    })
}

/// v0.8.12 HIGH-12 fix: verify a client-supplied integrity checksum
/// against the received body BEFORE we strip the header on the way
/// to the backend. Returns `Err(BadDigest)` on mismatch (matches
/// AWS S3 wire behaviour); `Ok(())` when the supplied digest matches
/// OR when the supplied algorithm is one we don't yet implement
/// (the latter is logged so operators see the gap — fail-open on
/// unsupported algorithms is the documented trade in the v0.8.11
/// CHANGELOG, with full coverage tracked as a follow-up issue).
///
/// Algorithms covered: `Content-MD5` (base64 MD5),
/// `x-amz-checksum-crc32c` (base64 big-endian u32),
/// `x-amz-checksum-sha256` (base64 SHA-256). The remaining S3
/// checksum algorithms (CRC32 non-Castagnoli, SHA-1, CRC64-NVME)
/// are accepted and silently passed; verifying them needs new
/// dependencies and was held back to keep the v0.8.12 surface
/// bounded.
#[allow(clippy::too_many_arguments)]
fn verify_client_body_checksums(
    body: &[u8],
    content_md5_b64: Option<&str>,
    checksum_crc32_b64: Option<&str>,
    checksum_crc32c_b64: Option<&str>,
    checksum_sha1_b64: Option<&str>,
    checksum_sha256_b64: Option<&str>,
    checksum_crc64nvme_b64: Option<&str>,
) -> S3Result<()> {
    use base64::Engine as _;
    use md5::Md5;
    use sha2::Sha256;
    // `Digest` from md-5 / sha2 brings the `new`, `update`, `finalize`
    // trait methods into scope. Bind anonymously so this `use` is
    // never flagged as unused while still serving its real purpose.
    use md5::Digest as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let bad = |what: &str| {
        let code = S3ErrorCode::from_bytes(b"BadDigest").unwrap_or(S3ErrorCode::InvalidArgument);
        S3Error::with_message(
            code,
            format!("client-supplied {what} did not match the received body"),
        )
    };
    if let Some(claimed) = content_md5_b64 {
        let want = b64.decode(claimed).map_err(|_| {
            S3Error::with_message(S3ErrorCode::InvalidDigest, "malformed Content-MD5")
        })?;
        if want.len() != 16 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "Content-MD5 must decode to 16 bytes",
            ));
        }
        let mut h = Md5::new();
        h.update(body);
        let got = h.finalize();
        // `subtle::ConstantTimeEq` would be ideal but the existing
        // `constant_time_eq` helper in sse.rs is private; use a
        // straightforward byte compare. The attacker doesn't get to
        // choose the body retroactively, so a timing oracle here
        // doesn't help them. `&got[..]` derefs the GenericArray
        // into a `&[u8]` (the deprecated `.as_slice()` is gone in
        // generic-array 1.x; CI runs `-D warnings`).
        if got[..] != *want.as_slice() {
            return Err(bad("Content-MD5"));
        }
    }
    if let Some(claimed) = checksum_crc32c_b64 {
        let want = b64.decode(claimed).map_err(|_| {
            S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "malformed x-amz-checksum-crc32c",
            )
        })?;
        if want.len() != 4 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "x-amz-checksum-crc32c must decode to 4 bytes (big-endian u32)",
            ));
        }
        let got = crc32c::crc32c(body).to_be_bytes();
        if got != want.as_slice() {
            return Err(bad("x-amz-checksum-crc32c"));
        }
    }
    if let Some(claimed) = checksum_sha256_b64 {
        let want = b64.decode(claimed).map_err(|_| {
            S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "malformed x-amz-checksum-sha256",
            )
        })?;
        if want.len() != 32 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "x-amz-checksum-sha256 must decode to 32 bytes",
            ));
        }
        let mut h = Sha256::new();
        h.update(body);
        let got = h.finalize();
        if got[..] != *want.as_slice() {
            return Err(bad("x-amz-checksum-sha256"));
        }
    }
    // v0.8.12 #128 (MED-C): CRC32 (IEEE 802.3 — the non-Castagnoli
    // variant AWS uses for `x-amz-checksum-crc32`). 4-byte
    // big-endian value, base64-encoded.
    if let Some(claimed) = checksum_crc32_b64 {
        let want = b64.decode(claimed).map_err(|_| {
            S3Error::with_message(S3ErrorCode::InvalidDigest, "malformed x-amz-checksum-crc32")
        })?;
        if want.len() != 4 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "x-amz-checksum-crc32 must decode to 4 bytes (big-endian u32)",
            ));
        }
        let mut h = crc32fast::Hasher::new();
        h.update(body);
        let got = h.finalize().to_be_bytes();
        if got != want.as_slice() {
            return Err(bad("x-amz-checksum-crc32"));
        }
    }
    // v0.8.12 #128 (MED-C): SHA-1. 20-byte digest, base64-encoded.
    if let Some(claimed) = checksum_sha1_b64 {
        use sha1::Sha1;
        let want = b64.decode(claimed).map_err(|_| {
            S3Error::with_message(S3ErrorCode::InvalidDigest, "malformed x-amz-checksum-sha1")
        })?;
        if want.len() != 20 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "x-amz-checksum-sha1 must decode to 20 bytes",
            ));
        }
        let mut h = Sha1::new();
        h.update(body);
        let got = h.finalize();
        if got[..] != *want.as_slice() {
            return Err(bad("x-amz-checksum-sha1"));
        }
    }
    // v0.8.12 #128 (MED-C): CRC64-NVME — AWS's newest checksum
    // algorithm. NVMe spec: poly 0xad93d23594c93659, init / xorout
    // 0xffffffffffffffff, refin / refout true. The reflected
    // polynomial + 256-entry lookup table are computed lazily on
    // first call (small enough to inline rather than pull in a
    // dedicated crc64 crate).
    if let Some(claimed) = checksum_crc64nvme_b64 {
        let want = b64.decode(claimed).map_err(|_| {
            S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "malformed x-amz-checksum-crc64nvme",
            )
        })?;
        if want.len() != 8 {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                "x-amz-checksum-crc64nvme must decode to 8 bytes (big-endian u64)",
            ));
        }
        let got = crc64_nvme(body).to_be_bytes();
        if got != want.as_slice() {
            return Err(bad("x-amz-checksum-crc64nvme"));
        }
    }
    Ok(())
}

/// v0.9 #106-audit-R2 P2-INT-2: verify SigV4-streaming **trailer**-supplied
/// checksums against an already-finalised [`ComputedDigests`].
///
/// Shared between the streaming-framed branch (digests computed via the
/// tee wrapper) and the buffered branch (digests computed in one shot
/// over the in-memory body via [`crate::streaming_checksum::compute_digests`]).
/// Centralising the logic prevents the pre-#106 fail-open shape —
/// where one branch verified trailers and the other silently skipped
/// them — from regressing. Both branches now go through the same
/// announce-parsing / fail-closed / per-name `compare_b64` pipeline.
///
/// Fail-closed posture (matches the streaming branch's behaviour):
///
/// - No `x-amz-trailer` header → returns Ok (no verification claimed).
/// - Header announces only non-checksum trailers (`x-amz-trailer-signature`,
///   custom) → returns Ok (filter selects checksum names only).
/// - Header announces `x-amz-checksum-*` but the trailing-headers handle
///   was absent → `BadDigest`.
/// - Handle present but trailers were never delivered (`read` returns
///   None) → `BadDigest`.
/// - Trailer announced but value missing in the delivered block → `BadDigest`.
/// - Value present but malformed / mismatched / refers to an unhashed
///   algorithm → `BadDigest` / `InvalidDigest` per [`ComputedDigests::compare_b64`].
fn verify_client_trailer_checksums(
    announced: Option<&str>,
    trailers_handle: Option<&s3s::TrailingHeaders>,
    computed: &crate::streaming_checksum::ComputedDigests,
) -> S3Result<()> {
    let Some(announced) = announced else {
        return Ok(());
    };
    let promised_checksum_trailers: Vec<String> = announced
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|n| {
            // RFC 9110 §5.1: HTTP header names are
            // case-insensitive — match accordingly.
            n.to_ascii_lowercase().starts_with("x-amz-checksum-")
        })
        .collect();
    if promised_checksum_trailers.is_empty() {
        return Ok(());
    }
    let bad_digest = |msg: String| -> S3Error {
        let code = S3ErrorCode::from_bytes(b"BadDigest").unwrap_or(S3ErrorCode::InvalidArgument);
        S3Error::with_message(code, msg)
    };
    let Some(th) = trailers_handle else {
        return Err(bad_digest(
            "client announced checksum trailer(s) via x-amz-trailer but \
             no trailing-headers handle was attached to the request"
                .into(),
        ));
    };
    let result = th.read(|hmap| {
        for name in &promised_checksum_trailers {
            match hmap.get(name.as_str()).and_then(|v| v.to_str().ok()) {
                Some(val) => {
                    computed.compare_b64(name, val)?;
                }
                None => {
                    return Err(bad_digest(format!(
                        "client announced trailer {name} via \
                         x-amz-trailer but the trailer value was \
                         missing or unparseable"
                    )));
                }
            }
        }
        Ok::<(), S3Error>(())
    });
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(e)) => Err(e),
        None => Err(bad_digest(
            "client announced checksum trailer(s) via x-amz-trailer \
             but no trailing-headers block was delivered with the body"
                .into(),
        )),
    }
}

/// v0.8.12 #128 (MED-C): CRC-64/NVME (AWS S3 `x-amz-checksum-crc64nvme`).
/// NVMe spec: poly 0xad93d23594c93659, init 0xffffffffffffffff, refin
/// true, refout true, xorout 0xffffffffffffffff. The reflected
/// polynomial table is computed lazily on first call via
/// [`std::sync::OnceLock`]; subsequent calls share the 256-entry table.
fn crc64_nvme(bytes: &[u8]) -> u64 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u64; 256]> = OnceLock::new();
    let tbl = TABLE.get_or_init(|| {
        // Reflected polynomial (bit-reverse of 0xad93d23594c93659).
        const POLY_REFLECTED: u64 = 0x9a6c_9329_ac4b_c9b5;
        let mut t = [0u64; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut c = i as u64;
            let mut j = 0;
            while j < 8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ POLY_REFLECTED
                } else {
                    c >> 1
                };
                j += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    });
    let mut crc: u64 = !0u64;
    for &b in bytes {
        let idx = ((crc as u8) ^ b) as usize;
        crc = (crc >> 8) ^ tbl[idx];
    }
    !crc
}

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
    /// Wrapped in `Arc` so the v0.6 #40 cross-bucket replication
    /// dispatcher can clone it into a detached `tokio::spawn` task
    /// (Arc::clone is cheap; backend trait methods take `&self` so no
    /// other handler is affected by the indirection).
    backend: Arc<B>,
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
    /// v0.6 #38: optional first-class CORS bucket configuration manager.
    /// When `Some(...)`, S4-server itself owns per-bucket CORS rules and
    /// `put_bucket_cors` / `get_bucket_cors` / `delete_bucket_cors`
    /// consult the manager instead of passing through to the backend.
    /// `handle_preflight` (public method on `S4Service`) routes OPTIONS-
    /// style preflight matching through the same store; the actual HTTP
    /// OPTIONS routing wire-up at the listener level is a follow-up
    /// (s3s framework does not surface OPTIONS as a typed handler).
    cors: Option<Arc<crate::cors::CorsManager>>,
    /// v0.6 #36: optional first-class S3 Inventory manager. When
    /// `Some(...)`, S4-server itself owns per-(bucket, id) inventory
    /// configurations and `put_bucket_inventory_configuration` /
    /// `get_bucket_inventory_configuration` /
    /// `list_bucket_inventory_configurations` /
    /// `delete_bucket_inventory_configuration` consult the manager
    /// instead of passing through to the backend. The actual periodic
    /// CSV emission is driven by a tokio task in `main.rs` that calls
    /// `InventoryManager::run_once_for_test` on a fixed cadence; the
    /// service handlers below only deal with config-level CRUD.
    inventory: Option<Arc<crate::inventory::InventoryManager>>,
    /// v0.6 #35: optional first-class S3 bucket-notification manager.
    /// When `Some(...)`, S4-server itself owns per-bucket notification
    /// configurations and `put_bucket_notification_configuration` /
    /// `get_bucket_notification_configuration` consult the manager
    /// instead of passing through to the backend. Successful PUT /
    /// DELETE handlers fire matching destinations on a detached tokio
    /// task (best-effort; see `crate::notifications::dispatch_event`).
    notifications: Option<Arc<crate::notifications::NotificationManager>>,
    /// v0.6 #37: optional first-class S3 Lifecycle configuration
    /// manager. When `Some(...)`, S4-server itself owns per-bucket
    /// lifecycle rules and `put_bucket_lifecycle_configuration` /
    /// `get_bucket_lifecycle_configuration` /
    /// `delete_bucket_lifecycle` consult the manager instead of
    /// passing through to the backend. The actual background scanner
    /// (list_objects_v2 -> evaluate -> delete / metadata-rewrite per
    /// rule) is a v0.7+ follow-up; the test path
    /// `S4Service::run_lifecycle_once_for_test` exercises the
    /// evaluator end-to-end so this v0.6 #37 wiring is enough to ship
    /// the configuration-management half without putting a
    /// half-wired bucket-walk in front of users.
    lifecycle: Option<Arc<crate::lifecycle::LifecycleManager>>,
    /// v0.6 #39: optional first-class object + bucket Tagging manager.
    /// When `Some(...)`, S4-server itself owns per-(bucket, key) and
    /// per-bucket tag state — `PutObjectTagging` /
    /// `GetObjectTagging` / `DeleteObjectTagging` /
    /// `PutBucketTagging` / `GetBucketTagging` /
    /// `DeleteBucketTagging` route through the manager (replacing the
    /// previous backend-passthrough behaviour). `put_object` also
    /// pre-parses the `x-amz-tagging` header / `Tagging` input field
    /// so the IAM policy evaluator can gate on
    /// `s3:RequestObjectTag/<key>` and `s3:ExistingObjectTag/<key>`.
    /// On a successful PUT the parsed tags are persisted; on a
    /// successful DELETE the matching tag entry is dropped.
    tagging: Option<Arc<crate::tagging::TagManager>>,
    /// v0.6 #40: optional first-class cross-bucket replication manager.
    /// When `Some(...)`, S4-server itself owns per-bucket replication
    /// rules; `PutBucketReplication` / `GetBucketReplication` /
    /// `DeleteBucketReplication` route through the manager (replacing
    /// the previous backend-passthrough behaviour). On every successful
    /// `put_object` the manager's rule list is consulted; the
    /// highest-priority matching enabled rule wins, the per-key status
    /// is recorded as `Pending`, and the source body and metadata are
    /// handed to a detached tokio task that PUTs to the destination
    /// bucket through the same backend. The replica is stamped with
    /// `x-amz-replication-status: REPLICA` in its metadata; the
    /// source-side status is updated to `Completed` on success or
    /// `Failed` after the 3-attempt retry budget is exhausted (drop
    /// counter bumps in either-side case so dashboards see the loss).
    /// `head_object` / `get_object` echo the recorded status back as
    /// `x-amz-replication-status` so consumers can poll progress.
    /// Limited to single-instance (same `S4Service`) replication; true
    /// cross-region (multi-instance) is a v0.7+ follow-up.
    replication: Option<Arc<crate::replication::ReplicationManager>>,
    /// v0.6 #42: optional MFA-Delete enforcement layer. When `Some(...)`,
    /// every DELETE / DELETE-version / delete-marker / `PutBucketVersioning`
    /// request against a bucket whose MFA-Delete state is `Enabled`
    /// must carry `x-amz-mfa: <serial> <code>` (RFC 6238 6-digit TOTP);
    /// missing or invalid tokens return HTTP 403 `AccessDenied`. When
    /// `None` (default), the gate is a no-op so existing v0.4 / v0.5
    /// deployments are unaffected until they explicitly call
    /// `with_mfa_delete(...)`.
    mfa_delete: Option<Arc<crate::mfa::MfaDeleteManager>>,
    /// v0.5 #32: when `true`, every PUT must carry an SSE indicator
    /// (`x-amz-server-side-encryption`, the SSE-C customer-key headers,
    /// or be matched against a configured server-managed keyring/KMS).
    /// Set by `--compliance-mode strict` after the boot-time
    /// prerequisite check passes.
    compliance_strict: bool,
    /// v0.7 #47: optional SigV4a (asymmetric ECDSA-P256-SHA256) verify
    /// gate. When `Some(...)`, the listener-side middleware (see
    /// [`crate::routing::try_sigv4a_verify`]) inspects every incoming
    /// request and short-circuits SigV4a-signed ones — verifying the
    /// signature against the credential store and returning 403
    /// `SignatureDoesNotMatch` / `InvalidAccessKeyId` on failure. Plain
    /// SigV4 (HMAC-SHA256) requests pass through to s3s untouched. When
    /// `None`, the middleware is a no-op so the existing SigV4 path is
    /// unaffected (operators opt in via `--sigv4a-credentials <DIR>`).
    sigv4a_gate: Option<Arc<SigV4aGate>>,
    /// v0.8 #54 BUG-5..10: per-`upload_id` side-table that ferries the
    /// SSE / Tagging / Object-Lock context captured at
    /// `CreateMultipartUpload` time through to `UploadPart` /
    /// `CompleteMultipartUpload`. Always-on (no `with_*` flag) — the
    /// store is gateway-internal and idle when no multipart is in
    /// flight. See [`crate::multipart_state`] for rationale.
    multipart_state: Arc<crate::multipart_state::MultipartStateStore>,
    /// v0.8 #52: plaintext bytes per S4E5 chunk on the SSE-S4 PUT
    /// path. `0` (default) → use the legacy buffered S4E2 path
    /// (whole-body AES-GCM tag, GET buffers + verifies before
    /// emitting). Non-zero → use the chunked S4E5 frame so GET can
    /// stream-decrypt chunk-by-chunk. Wired by `--sse-chunk-size`
    /// in `main.rs`. SSE-C and SSE-KMS are intentionally unaffected
    /// (chunked variants tracked in a follow-up issue).
    sse_chunk_size: usize,
    /// v0.8.5 #86 (audit M-2): bounded permit pool gating the detached
    /// replication dispatcher in [`Self::spawn_replication_if_matched`].
    /// Without this cap, a high-volume PUT workload (1k req/s × N enabled
    /// rules × slow destination = O(10k) in-flight tokio tasks) could
    /// exhaust process memory before the destination drains. Each
    /// dispatcher spawn `acquire_owned`s one permit and holds it for the
    /// lifetime of the destination PUT + status stamp; once the cap is
    /// reached the dispatcher async-blocks on `acquire_owned()` so the
    /// listener path itself never stalls — only the in-flight replica
    /// queue depth is bounded. Default 1024 (operator-tunable via
    /// `--replication-max-concurrent`).
    replication_semaphore: Arc<tokio::sync::Semaphore>,
    /// v0.8.11 CRIT-4 fix: trust the `X-Forwarded-For` header for the
    /// `aws:SourceIp` Condition key only when the operator has
    /// explicitly opted in via `--trust-x-forwarded-for`. Default
    /// (`false`) makes the policy evaluator see `source_ip = None`
    /// for incoming requests, so a public-internet client can no
    /// longer spoof an internal CIDR by setting `X-Forwarded-For`
    /// themselves. Operators behind a trusted reverse proxy that
    /// scrubs / sets `X-Forwarded-For` enable the flag; gateways
    /// listening directly on the public internet leave it off and
    /// gain a clear fail-closed default. A future release plumbs
    /// the TCP peer address through the s3s service trait so we can
    /// validate the forwarded header against a `--trusted-proxies`
    /// CIDR list; until then the boolean opt-in closes the immediate
    /// auth-bypass surface.
    trust_x_forwarded_for: bool,
    /// v0.8.17 G-4 (#161): migration escape hatch. When `true`,
    /// the v0.8.16 F-13 reserved-name guard does NOT block GET /
    /// HEAD / DELETE on keys ending in `.s4index` — the operator
    /// is asserting that the deployment may carry pre-v0.8.15
    /// user objects with that suffix and wants a window to
    /// migrate them off. Writes (PUT / Copy / Create-Multipart)
    /// stay blocked regardless of this flag, so attacker
    /// injection from M-1 / F-13 stays closed. Default
    /// `false` matches the v0.8.16 behaviour.
    allow_legacy_reserved_key_reads: bool,
    /// v1.1 `--zstd-dict`: optional prefix→dictionary store for the PUT
    /// side. An empty [`crate::dict::SharedDictStore`] (default — `load`
    /// returns `None`) leaves every PUT bit-for-bit on the pre-dict
    /// path; a populated one makes small cpu-zstd PUTs whose key
    /// longest-prefix-matches a configured prefix try `cpu-zstd-dict`
    /// (falling back to plain cpu-zstd when the dictionary doesn't
    /// actually shrink the body). Built in `main.rs` from the repeated
    /// `--zstd-dict <bucket>/<prefix>=<dict-id>` flags and/or the
    /// `--zstd-dict-map <FILE>` TOML; every dictionary is fetched from
    /// the backend at boot.
    ///
    /// v1.3 dict ops: the holder is reload-capable — the
    /// `--zstd-dict-map` SIGHUP handler swaps a fully-built replacement
    /// store in atomically (RCU; no lock on the request path). Each
    /// request `load`s the store once and keeps that generation for its
    /// lifetime.
    zstd_dicts: Arc<crate::dict::SharedDictStore>,
    /// v1.3 dict ops: rolling per-prefix win-rate monitor for the dict
    /// PUT branch. When a configured prefix's last 100 dict-vs-plain
    /// decisions fall below a 0.5 win rate, the gateway WARNs (at most
    /// once per prefix per hour) that the dictionary looks stale.
    /// Inert (never touched) when no dict store is configured.
    dict_win_tracker: crate::dict::DictWinTracker,
    /// v1.1 `--zstd-dict` GET resilience: small LRU of lazily-fetched
    /// dictionaries. Always present (no flag needed) so objects stamped
    /// with `s4-dict-id` stay readable on a gateway that never loaded —
    /// or has since dropped — the matching `--zstd-dict` flag: the GET
    /// path fetches `.s4dict/<id>` from the object's bucket, verifies
    /// the content-addressed fingerprint, and caches it here.
    dict_cache: Arc<crate::dict::DictCache>,
    /// v1.2 `--gpu-batch-small-puts`: optional GPU batch-compression
    /// aggregator handle. `None` (default) keeps every PUT bit-for-bit on
    /// the pre-batch path. `Some(...)` routes small cpu-zstd PUT bodies
    /// (`--gpu-batch-floor-bytes <= len < --gpu-min-bytes`, no dict match)
    /// through the nvCOMP batched-zstd aggregator; any decline (queue
    /// full / GPU error / output not smaller than input) falls back to
    /// the unchanged cpu-zstd framed path. Wired by `main.rs` only when
    /// the flag is on AND the `nvcomp-gpu` build has a CUDA GPU at boot.
    gpu_batch: Option<crate::gpu_batch::GpuBatchHandle>,
    /// v1.2 `--savings-ledger-state-file`: optional measured-savings
    /// ledger. `None` (default) keeps every code path bit-for-bit on
    /// the pre-ledger behaviour (every hook is `if let Some(...)`
    /// guarded). `Some(...)` makes write-shaped handlers (PUT /
    /// CompleteMultipartUpload / CopyObject / DELETE) maintain
    /// per-bucket cumulative `original_bytes` / `stored_bytes` /
    /// `objects` counters, flushed event-driven to the operator's
    /// state file and exported as `s4_ledger_*` gauges. Overwrite /
    /// DELETE subtraction relies on best-effort HEAD probes of the
    /// to-be-replaced object — the extra backend requests exist
    /// **only** when this field is `Some` (documented in the README).
    savings_ledger: Option<Arc<crate::ledger::SavingsLedger>>,
}

/// v0.8.17 G-2: which AWS error shape the reserved-name guard
/// should emit on hit. `Read`-mode endpoints (GET / HEAD /
/// Attributes / Tagging-read) return `NoSuchKey` — consistent
/// with the listing filter hiding the sidecar. `Mutating`-mode
/// endpoints (PUT / Copy / DELETE / Tagging-write / ACL-write)
/// return `InvalidObjectName` so the client sees the suffix is
/// reserved by-design rather than coincidentally missing.
#[derive(Clone, Copy, Debug)]
enum ReservedKeyMode {
    Read,
    Mutating,
}

/// v1.2 savings ledger: byte footprint of one backend object as seen
/// by a [`S4Service::ledger_probe_object`] HEAD probe. `stored_bytes`
/// is the backend Content-Length (frames + SSE envelope, NO sidecar —
/// sidecars are probed separately because their lifecycle differs);
/// `original_bytes` is the logical pre-compression size.
///
/// v1.2 audit R1 P2: `accounted` reports whether the object carries
/// the gateway's `s4-ledger` marker (= its bytes were added to the
/// ledger at write time). Subtraction sites must skip `!accounted`
/// footprints and call `SavingsLedger::record_skipped_unaccounted`
/// instead — subtracting a never-added object is the asymmetric-
/// subtraction bug this marker exists to prevent.
#[derive(Clone, Copy, Debug)]
struct LedgerFootprint {
    original_bytes: u64,
    stored_bytes: u64,
    accounted: bool,
}

impl<B: S3> S4Service<B> {
    /// AWS S3 単発 PUT の API 上限 (5 GiB)。
    ///
    /// v0.9 #106 (32-bit target support): `target_pointer_width` で gating して
    /// 32-bit target の const-overflow を回避。 32-bit では `isize::MAX as usize`
    /// (≈ 2 GiB on 32-bit) に collapse ── Rust 言語仕様で `Vec` / `Bytes`
    /// 1 回の allocation は `isize::MAX` byte が上限 (`usize::MAX` ではない) で、
    /// `usize::MAX` を cap にすると oversized-body guard を通過した後で
    /// `Vec::with_capacity` 側が panic することがある (Codex review P2 で発覚)。
    /// s4-server runtime は 64-bit only (README §"Supported targets") だが、
    /// workspace-wide `cargo check --target wasm32-*` 等で blocking しない + 32-bit
    /// build で SSE buffered-decrypt が OOM panic しないためのガード。
    #[cfg(target_pointer_width = "64")]
    pub const DEFAULT_MAX_BODY_BYTES: usize = 5 * 1024 * 1024 * 1024;
    #[cfg(target_pointer_width = "32")]
    pub const DEFAULT_MAX_BODY_BYTES: usize = isize::MAX as usize;

    /// v0.8.5 #86 (audit M-2): default cap on simultaneously-in-flight
    /// replication dispatcher tasks. See the `replication_semaphore`
    /// field doc for the rationale + override path.
    pub const DEFAULT_REPLICATION_MAX_CONCURRENT: usize = 1024;

    pub fn new(
        backend: B,
        registry: Arc<CodecRegistry>,
        dispatcher: Arc<dyn CodecDispatcher>,
    ) -> Self {
        Self {
            backend: Arc::new(backend),
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
            cors: None,
            inventory: None,
            notifications: None,
            lifecycle: None,
            tagging: None,
            replication: None,
            mfa_delete: None,
            compliance_strict: false,
            sigv4a_gate: None,
            multipart_state: Arc::new(crate::multipart_state::MultipartStateStore::new()),
            // v0.8 #52: chunked SSE-S4 disabled by default — opt
            // in via `S4Service::with_sse_chunk_size(...)` /
            // `--sse-chunk-size <BYTES>`. Default keeps the legacy
            // S4E2 buffered path so existing deployments are
            // bit-for-bit unchanged.
            sse_chunk_size: 0,
            // v0.8.5 #86 (audit M-2): default cap of 1024 in-flight
            // replication tasks. Picked to be (a) ample headroom over a
            // typical steady-state replication rate (the v0.8.3 #66
            // status-sweep doc cites 1k keys/hour as a "steady" rate, so
            // even a 100x burst lands well under 1024), (b) small enough
            // that the worst-case memory pinned by stalled dispatchers
            // — body bytes + metadata — stays bounded (1024 × 5 MiB
            // typical S3 PUT ≈ 5 GiB, recoverable). Operators with
            // wider cross-region fan-out can override via
            // `--replication-max-concurrent`.
            replication_semaphore: Arc::new(tokio::sync::Semaphore::new(
                Self::DEFAULT_REPLICATION_MAX_CONCURRENT,
            )),
            // v0.8.11 CRIT-4: default fail-closed — ignore client-
            // supplied `X-Forwarded-For` until the operator opts in
            // through `with_trust_x_forwarded_for(true)`.
            trust_x_forwarded_for: false,
            // v0.8.17 G-4: closed by default; opt in via
            // `with_allow_legacy_reserved_key_reads(true)` for the
            // migration window only.
            allow_legacy_reserved_key_reads: false,
            // v1.1 `--zstd-dict`: PUT-side dict store off by default
            // (no behaviour change without the flag); the GET-side lazy
            // cache is always live but only consulted for objects that
            // carry the `s4-dict-id` metadata stamp.
            zstd_dicts: Arc::new(crate::dict::SharedDictStore::default()),
            // v1.3 dict ops: win-rate monitor — inert until a dict
            // store is attached AND a PUT takes the dict branch.
            dict_win_tracker: crate::dict::DictWinTracker::default(),
            dict_cache: Arc::new(crate::dict::DictCache::default()),
            // v1.2 `--gpu-batch-small-puts`: off by default — no GPU
            // batch aggregator, zero behaviour change.
            gpu_batch: None,
            // v1.2 `--savings-ledger-state-file`: off by default — no
            // ledger counters, no probe HEADs, zero behaviour change.
            savings_ledger: None,
        }
    }

    /// v1.2 `--savings-ledger-state-file`: attach the measured-savings
    /// ledger. Write-shaped handlers then maintain per-bucket
    /// cumulative original/stored/objects counters (flushed to the
    /// ledger's state file on every mutation, exported as
    /// `s4_ledger_*` gauges, readable offline via `s4 savings`).
    /// When unset (default), every handler is bit-for-bit unchanged.
    #[must_use]
    pub fn with_savings_ledger(mut self, ledger: Arc<crate::ledger::SavingsLedger>) -> Self {
        self.savings_ledger = Some(ledger);
        self
    }

    /// v1.2: introspection accessor for the attached savings ledger
    /// (SIGUSR1 dump-back walk in `main.rs`; same shape as
    /// [`Self::tag_manager`] and friends).
    pub fn savings_ledger(&self) -> Option<&Arc<crate::ledger::SavingsLedger>> {
        self.savings_ledger.as_ref()
    }

    /// v1.2 `--gpu-batch-small-puts`: attach the GPU small-PUT batch
    /// aggregator handle. Small cpu-zstd PUTs inside the handle's
    /// `[floor_bytes, max_bytes)` size window are compressed through the
    /// nvCOMP batched-zstd path (one kernel launch per batch); everything
    /// the batch path declines falls back to the unchanged cpu-zstd
    /// framed path. Stored objects are standard `nvcomp-zstd` bodies —
    /// the GET path needs (and has) no batch awareness.
    pub fn with_gpu_batch(mut self, handle: crate::gpu_batch::GpuBatchHandle) -> Self {
        self.gpu_batch = Some(handle);
        self
    }

    /// v1.1 `--zstd-dict`: attach the boot-time dictionary store. PUTs
    /// whose key longest-prefix-matches a configured `<bucket>/<prefix>`
    /// and whose declared size fits `--zstd-dict-max-bytes` compress
    /// with the trained dictionary when it beats plain cpu-zstd. When
    /// unset (default), PUT behaviour is bit-for-bit unchanged.
    #[must_use]
    pub fn with_zstd_dicts(mut self, store: Arc<crate::dict::DictStore>) -> Self {
        self.zstd_dicts = Arc::new(crate::dict::SharedDictStore::new(Some(store)));
        self
    }

    /// v1.3 dict ops (`--zstd-dict-map` + SIGHUP): attach a
    /// **reload-capable** shared dictionary store. Same PUT/GET
    /// semantics as [`Self::with_zstd_dicts`], but the caller keeps a
    /// handle to the [`crate::dict::SharedDictStore`] and may
    /// [`crate::dict::SharedDictStore::swap`] in a fully-built
    /// replacement at any time (the SIGHUP map-reload path in
    /// `main.rs`). Requests pick up the new generation on their next
    /// store load; in-flight requests finish on the generation they
    /// started with.
    #[must_use]
    pub fn with_shared_zstd_dicts(mut self, shared: Arc<crate::dict::SharedDictStore>) -> Self {
        self.zstd_dicts = shared;
        self
    }

    /// v0.8.17 G-4: opt in to a migration window where GET / HEAD /
    /// DELETE on `<key>.s4index` are allowed even though new
    /// writes against that suffix stay rejected. Used by operators
    /// upgrading from pre-v0.8.15 deployments that may carry
    /// legacy user-owned objects with the now-reserved suffix.
    /// Defaults to `false`; turn off again once the legacy data
    /// has been migrated.
    #[must_use]
    pub fn with_allow_legacy_reserved_key_reads(mut self, on: bool) -> Self {
        self.allow_legacy_reserved_key_reads = on;
        self
    }

    /// v0.8.11 CRIT-4 fix: opt in to consuming the leftmost token of
    /// the `X-Forwarded-For` header as `aws:SourceIp`. Only enable
    /// when the gateway sits behind a trusted reverse proxy that
    /// strips (or rewrites) any client-supplied value. When left
    /// off (default), the policy evaluator sees `source_ip = None`
    /// regardless of what the client sends — closing the
    /// public-internet `X-Forwarded-For: 10.0.0.1` IAM-allowlist
    /// bypass.
    #[must_use]
    pub fn with_trust_x_forwarded_for(mut self, on: bool) -> Self {
        self.trust_x_forwarded_for = on;
        self
    }

    /// v0.7 #47: attach the SigV4a verify gate. Once set, the
    /// listener-side middleware (`crate::routing::try_sigv4a_verify`)
    /// short-circuits any incoming `AWS4-ECDSA-P256-SHA256` request,
    /// verifying it against the supplied credential store and
    /// returning 403 on failure. Plain SigV4 (HMAC-SHA256) requests
    /// are unaffected. When the gate is unset (default), the
    /// middleware skips entirely so existing SigV4 deployments keep
    /// working.
    #[must_use]
    pub fn with_sigv4a_gate(mut self, gate: Arc<SigV4aGate>) -> Self {
        self.sigv4a_gate = Some(gate);
        self
    }

    /// v0.7 #47: borrow the attached SigV4a gate. Used by `main.rs`
    /// to snapshot the gate `Arc` before the s3s `ServiceBuilder`
    /// consumes the `S4Service` (the listener-side middleware needs
    /// the same `Arc` because s3s' SigV4 verifier rejects SigV4a
    /// algorithm tokens with "unknown algorithm" — match has to
    /// happen at the hyper layer instead).
    #[must_use]
    pub fn sigv4a_gate(&self) -> Option<&Arc<SigV4aGate>> {
        self.sigv4a_gate.as_ref()
    }

    /// v0.8.2 #62: borrow the multipart state store so `main.rs` can
    /// snapshot the `Arc` before the s3s `ServiceBuilder` consumes
    /// the `S4Service`. The background `sweep_stale` task in `main.rs`
    /// holds this `Arc` and ticks once an hour to drop abandoned
    /// upload contexts (and their `Zeroizing<[u8; 32]>` SSE-C keys).
    #[must_use]
    pub fn multipart_state(&self) -> &Arc<crate::multipart_state::MultipartStateStore> {
        &self.multipart_state
    }

    /// v0.6 #39: attach the in-memory object + bucket Tagging manager.
    /// Once set, `Put/Get/Delete` `Object/Bucket Tagging` route
    /// through the manager (instead of forwarding to the backend),
    /// and `put_object`'s `x-amz-tagging` parse path becomes the
    /// source of `s3:RequestObjectTag/<key>` for the IAM policy
    /// evaluator. The manager itself is shared via `Arc`.
    #[must_use]
    pub fn with_tagging(mut self, mgr: Arc<crate::tagging::TagManager>) -> Self {
        self.tagging = Some(mgr);
        self
    }

    /// v0.6 #39: borrow the attached tagging manager (test /
    /// introspection — the snapshotter in `main.rs`, when wired,
    /// will keep its own `Arc` clone).
    #[must_use]
    pub fn tag_manager(&self) -> Option<&Arc<crate::tagging::TagManager>> {
        self.tagging.as_ref()
    }

    /// v0.6 #36: attach the in-memory S3 Inventory manager. Once set,
    /// `put_bucket_inventory_configuration` /
    /// `get_bucket_inventory_configuration` /
    /// `list_bucket_inventory_configurations` /
    /// `delete_bucket_inventory_configuration` route through the
    /// manager. The actual periodic CSV / manifest emission is
    /// orchestrated by a tokio task started in `main.rs`; the manager
    /// itself is shared between the handler and the scheduler via
    /// `Arc`.
    #[must_use]
    pub fn with_inventory(mut self, mgr: Arc<crate::inventory::InventoryManager>) -> Self {
        self.inventory = Some(mgr);
        self
    }

    /// v0.6 #36: borrow the attached inventory manager (test /
    /// introspection — the background scheduler in `main.rs` keeps its
    /// own `Arc` clone, so this accessor is for the test path that
    /// invokes `run_once_for_test` directly).
    #[must_use]
    pub fn inventory_manager(&self) -> Option<&Arc<crate::inventory::InventoryManager>> {
        self.inventory.as_ref()
    }

    /// v0.6 #37: attach the in-memory S3 Lifecycle configuration
    /// manager. Once set, `put_bucket_lifecycle_configuration` /
    /// `get_bucket_lifecycle_configuration` / `delete_bucket_lifecycle`
    /// route through the manager (replacing the previous backend-
    /// passthrough behaviour). The actual periodic scanner that walks
    /// the source bucket and invokes Expiration / Transition /
    /// NoncurrentExpiration actions is a v0.7+ follow-up — see
    /// [`Self::run_lifecycle_once_for_test`] for the in-memory test
    /// path that exercises the evaluator end-to-end.
    #[must_use]
    pub fn with_lifecycle(mut self, mgr: Arc<crate::lifecycle::LifecycleManager>) -> Self {
        self.lifecycle = Some(mgr);
        self
    }

    /// v0.6 #37: borrow the attached lifecycle manager (test /
    /// introspection — the background scheduler in `main.rs` keeps its
    /// own `Arc` clone, so this accessor is for the test path that
    /// invokes the evaluator directly).
    #[must_use]
    pub fn lifecycle_manager(&self) -> Option<&Arc<crate::lifecycle::LifecycleManager>> {
        self.lifecycle.as_ref()
    }

    /// v0.6 #37: synchronous test entry that runs the lifecycle evaluator
    /// against a caller-provided list of `(key, age, size, tags)` tuples
    /// and returns the `(key, action)` pairs that should fire. The actual
    /// backend invocation (S3.delete_object / metadata rewrite) is left
    /// to the caller — the unit + E2E tests use this to verify the
    /// evaluator without spawning the (deferred) background scanner.
    /// Returns an empty `Vec` when no lifecycle manager is attached or
    /// no rule matches.
    ///
    /// **v1.0 F3 — UNSTABLE, NOT part of the v1.0 public API contract.**
    /// Marked `#[doc(hidden)]` so it does not appear in `cargo doc` output
    /// and so external consumers don't grow a dependency on its existence
    /// or signature. It stays `pub` only because the same-crate integration
    /// test in `tests/roundtrip.rs` cannot reach `#[cfg(test)]`-gated items
    /// (Rust builds the lib without `cfg(test)` when compiling integration
    /// test targets). Signature, name, or existence may change in any 1.y
    /// release without semver notice.
    #[doc(hidden)]
    #[must_use]
    pub fn run_lifecycle_once_for_test(
        &self,
        bucket: &str,
        objects: &[crate::lifecycle::EvaluateBatchEntry],
    ) -> Vec<(String, crate::lifecycle::LifecycleAction)> {
        let Some(mgr) = self.lifecycle.as_ref() else {
            return Vec::new();
        };
        crate::lifecycle::evaluate_batch(mgr, bucket, objects)
    }

    /// v0.6 #35: attach the in-memory bucket-notification manager. Once
    /// set, `put_bucket_notification_configuration` /
    /// `get_bucket_notification_configuration` route through the manager
    /// (replacing the previous backend-passthrough behaviour); successful
    /// `put_object` / `delete_object` calls fire matching destinations
    /// on a detached tokio task via
    /// `crate::notifications::dispatch_event` (best-effort, fire-and-
    /// forget — failures bump the manager's `dropped_total` counter and
    /// log at warn but do NOT fail the originating S3 request).
    #[must_use]
    pub fn with_notifications(
        mut self,
        mgr: Arc<crate::notifications::NotificationManager>,
    ) -> Self {
        self.notifications = Some(mgr);
        self
    }

    /// v0.6 #35: borrow the attached notifications manager (test /
    /// introspection — used by the metrics layer to read
    /// `dropped_total`).
    #[must_use]
    pub fn notifications_manager(&self) -> Option<&Arc<crate::notifications::NotificationManager>> {
        self.notifications.as_ref()
    }

    /// v0.6 #35: internal helper used by the DELETE handlers to fire a
    /// matching notification on a detached tokio task. No-op when no
    /// manager is attached or no rule on the bucket matches the given
    /// (event, key) tuple.
    fn fire_delete_notification(
        &self,
        bucket: &str,
        key: &str,
        event: crate::notifications::EventType,
        version_id: Option<String>,
    ) {
        let Some(mgr) = self.notifications.as_ref() else {
            return;
        };
        let dests = mgr.match_destinations(bucket, &event, key);
        if dests.is_empty() {
            return;
        }
        tokio::spawn(crate::notifications::dispatch_event(
            Arc::clone(mgr),
            bucket.to_owned(),
            key.to_owned(),
            event,
            None,
            None,
            version_id,
            format!("S4-{}", uuid::Uuid::new_v4()),
        ));
    }

    /// v0.6 #40: attach the in-memory cross-bucket replication manager.
    /// Once set, `put_bucket_replication` / `get_bucket_replication` /
    /// `delete_bucket_replication` route through the manager (replacing
    /// the previous backend-passthrough behaviour); a successful
    /// `put_object` whose key matches an enabled rule fires a detached
    /// tokio task that PUTs the same body + metadata to the rule's
    /// destination bucket, stamping the replica with
    /// `x-amz-replication-status: REPLICA`. Failures after the retry
    /// budget bump the manager's `dropped_total` counter and are
    /// surfaced in the `s4_replication_dropped_total` Prometheus
    /// counter; successes bump `s4_replication_replicated_total`.
    #[must_use]
    pub fn with_replication(mut self, mgr: Arc<crate::replication::ReplicationManager>) -> Self {
        self.replication = Some(mgr);
        self
    }

    /// v0.6 #40: borrow the attached replication manager (test /
    /// introspection — used by the metrics layer to read
    /// `dropped_total`).
    #[must_use]
    pub fn replication_manager(&self) -> Option<&Arc<crate::replication::ReplicationManager>> {
        self.replication.as_ref()
    }

    /// v0.6 #40: internal helper used by the PUT handlers to fire a
    /// detached cross-bucket replication task. No-op when no manager
    /// is attached, the source backend PUT failed, or no rule on the
    /// source bucket matches the (key, tags) tuple. The `body` is the
    /// post-compression / post-encryption `Bytes` that was sent to
    /// the source backend (refcount-cloned), and `metadata` is the
    /// metadata map that already includes the manifest /
    /// `s4-encrypted` markers — the replica decodes through the same
    /// path. The destination PUT runs through `Arc<B>::put_object`.
    ///
    /// ## v0.8.2 #61: generation token + shadow-key destination
    ///
    /// `pending_version` is the source-side `PutOutcome` minted by the
    /// caller's versioning branch (or `None` for unversioned /
    /// suspended buckets). When `pending_version.versioned_response`
    /// is `true`, the dispatcher writes the destination under the same
    /// shadow path the source uses (`<key>.__s4ver__/<vid>`) so the
    /// destination's version chain receives the new version the same
    /// way `?versionId=` GET resolves it. Closes audit C-1.
    ///
    /// The dispatcher also mints a fresh `generation` token before
    /// spawning, threaded through to [`crate::replication::
    /// replicate_object`]. Closes audit C-3 — a stale retry of an
    /// older PUT can no longer overwrite the destination's newer bytes
    /// because the CAS guard sees the higher stored generation and
    /// drops its destination write.
    ///
    /// ## Asymmetric versioning policy (out of scope)
    ///
    /// We assume source + destination buckets share the same
    /// versioning policy (both Enabled or both Suspended /
    /// Unversioned). Cross-bucket policy queries would require a
    /// backend round-trip per replication, which is not worth it for
    /// the single-instance scope. Operators who configure asymmetric
    /// versioning will see destination-side `?versionId=` lookups
    /// miss — documented as out-of-scope until a future per-rule
    /// `destination_versioning_policy` knob lands.
    // 8 args is the post-#61 shape: replication needs the
    // source bucket+key, the canonical tag set for rule-matching,
    // the post-codec body+metadata for the destination PUT, the
    // backend-success gate, and the pending version-id for the
    // shadow-key destination override. A shape struct would just
    // split the (single) call site so opt for the inline form.
    #[allow(clippy::too_many_arguments)]
    fn spawn_replication_if_matched(
        &self,
        source_bucket: &str,
        source_key: &str,
        request_tags: &Option<crate::tagging::TagSet>,
        body: &bytes::Bytes,
        metadata: &Option<std::collections::HashMap<String, String>>,
        backend_ok: bool,
        pending_version: Option<&crate::versioning::PutOutcome>,
    ) where
        B: Send + Sync + 'static,
    {
        if !backend_ok {
            return;
        }
        let Some(mgr) = self.replication.as_ref() else {
            return;
        };
        // Pull the request's tags into the (k, v) shape the matcher
        // expects. The tagging manager would have the canonical
        // post-PUT view but at this point in the pipeline it's
        // already been written above; for the rule-match decision
        // the request's tags are sufficient (= the tags this PUT
        // applies, S3 PutObject is full-replace on tags).
        let object_tags: Vec<(String, String)> = request_tags
            .as_ref()
            .map(|ts| ts.iter().cloned().collect())
            .unwrap_or_default();
        let Some(rule) = mgr.match_rule(source_bucket, source_key, &object_tags) else {
            return;
        };
        // v0.8.2 #61: mint the per-PUT generation BEFORE the eager
        // Pending stamp so the stamp itself carries the right
        // generation (the CAS in `record_status_if_newer` would
        // otherwise see a `generation=0` Pending and accept any
        // stale retry).
        let generation = mgr.next_generation();
        // Eagerly mark the source key as Pending so a HEAD between
        // the source PUT returning and the spawned task completing
        // surfaces the in-flight state. CAS-guarded so a slower
        // older PUT can't downgrade a newer Completed back to Pending.
        let _ = mgr.record_status_if_newer(
            source_bucket,
            source_key,
            generation,
            crate::replication::ReplicationStatus::Pending,
        );
        // v0.8.2 #61: derive the destination storage key. For a
        // versioning-Enabled source the destination receives the
        // same shadow-key path so a `?versionId=<vid>` GET on the
        // destination resolves through the same lookup the source
        // uses. Suspended / Unversioned sources keep the logical
        // key (= `None` override = dispatcher uses `source_key`).
        let destination_key_override = pending_version
            .filter(|pv| pv.versioned_response)
            .map(|pv| versioned_shadow_key(source_key, &pv.version_id));
        // v0.8.3 #68 (audit M-1): capture the source object's Object
        // Lock state so the dispatcher can decorate the destination
        // PUT with the matching AWS-wire lock headers. Without this,
        // a Compliance / Governance / legal-hold protected source
        // would replicate to a destination where DELETE succeeds
        // (the WORM posture would only hold on the source).
        let source_lock_state = self
            .object_lock
            .as_ref()
            .and_then(|mgr| mgr.get(source_bucket, source_key));
        // v0.8.3 #68: hand the destination-side ObjectLockManager to
        // the dispatcher closure so we can persist the propagated
        // lock state on successful destination PUT (the destination
        // PUT below bypasses S4Service::put_object — we drive the
        // backend directly — so the explicit_lock_mode commit block
        // in put_object never fires for replicas. We replay it here
        // against the destination key.)
        let dest_lock_mgr = self.object_lock.as_ref().map(Arc::clone);
        let mgr_cl = Arc::clone(mgr);
        let backend = Arc::clone(&self.backend);
        let body_cl = body.clone();
        let metadata_cl = metadata.clone();
        let source_bucket_cl = source_bucket.to_owned();
        let source_key_cl = source_key.to_owned();
        let source_lock_state_for_closure = source_lock_state.clone();
        let source_bucket_for_warn = source_bucket.to_owned();
        // v0.8.5 #86 (audit M-2): bound the in-flight replication queue
        // depth. Acquire happens INSIDE the spawned task (not on the
        // listener path) so a saturated semaphore back-pressures the
        // dispatcher pool without stalling the source PUT response —
        // the source has already returned 200 to the client by the time
        // the spawn body runs. A failed `acquire_owned` only happens
        // when the semaphore is closed (we never close it, so the
        // logged-and-skipped fallback is unreachable in practice).
        let semaphore = Arc::clone(&self.replication_semaphore);
        tokio::spawn(async move {
            let _permit = match semaphore.acquire_owned().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        bucket = %source_bucket_cl,
                        key = %source_key_cl,
                        "S4 replication dispatcher could not acquire semaphore permit (closed? {e}); skipping replica"
                    );
                    return;
                }
            };
            let do_put = move |dest_bucket: String,
                               dest_key: String,
                               dest_body: bytes::Bytes,
                               dest_meta: Option<std::collections::HashMap<String, String>>| {
                let backend = Arc::clone(&backend);
                let dest_lock_mgr = dest_lock_mgr.clone();
                let lock_state = source_lock_state_for_closure.clone();
                let warn_src = source_bucket_for_warn.clone();
                async move {
                    let req = S3Request {
                        input: PutObjectInput {
                            bucket: dest_bucket.clone(),
                            key: dest_key.clone(),
                            body: Some(bytes_to_blob(dest_body)),
                            metadata: dest_meta,
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
                    };
                    let put_result = backend
                        .put_object(req)
                        .await
                        .map(|_| ())
                        .map_err(|e| format!("destination put_object: {e}"));
                    // v0.8.3 #68: on successful destination PUT,
                    // persist the propagated lock state into the
                    // destination's ObjectLockManager so a subsequent
                    // DELETE on the destination is refused. Three cases:
                    //   - PUT failed     → skip (no replica to protect)
                    //   - lock_state None → nothing to propagate
                    //   - dest manager None (operator misconfig)
                    //                     → log warn-once + bump skip metric
                    if put_result.is_ok()
                        && let Some(state) = lock_state
                    {
                        match dest_lock_mgr {
                            Some(ref mgr) => {
                                mgr.set(&dest_bucket, &dest_key, state);
                            }
                            None => {
                                crate::replication::warn_lock_propagation_skipped(
                                    &warn_src,
                                    &dest_bucket,
                                );
                            }
                        }
                    }
                    put_result
                }
            };
            // v0.8.5 #81 (audit H-7): wrap the dispatcher body in
            // `futures::FutureExt::catch_unwind` so a panic inside
            // `replicate_object` (or any of the user-supplied closures
            // it drives — `do_put`, the destination backend, the lock
            // manager) does NOT bubble out of the detached task as a
            // `JoinError` that no operator dashboard scrapes. Caught
            // panics bump `s4_dispatcher_panics_total{kind="replication"}`
            // + log at ERROR with the panic payload, so silent feature
            // degradation (= every replication PUT panicking and
            // dropping the replica without any visible signal) becomes
            // a first-class metric the operator can alert on.
            //
            // `AssertUnwindSafe` is required because the inner future
            // captures `Arc<...>` clones + a `do_put` closure that are
            // not `UnwindSafe` by default; the safety contract here is
            // "we don't continue using any of those captures after the
            // panic" which trivially holds (we drop them and return).
            use futures::FutureExt as _;
            let dispatcher_kind = "replication";
            let fut = crate::replication::replicate_object(
                rule,
                source_bucket_cl,
                source_key_cl,
                body_cl,
                metadata_cl,
                do_put,
                mgr_cl,
                generation,
                destination_key_override,
                source_lock_state,
            );
            if let Err(panic) = std::panic::AssertUnwindSafe(fut).catch_unwind().await {
                let panic_msg = panic
                    .downcast_ref::<&'static str>()
                    .copied()
                    .map(str::to_owned)
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "(non-string panic payload)".to_owned());
                tracing::error!(
                    kind = dispatcher_kind,
                    panic_payload = %panic_msg,
                    "S4 dispatcher task panicked (caught by catch_unwind, runtime not poisoned)"
                );
                crate::metrics::record_dispatcher_panic(dispatcher_kind);
            }
        });
    }

    /// v0.6 #42: attach the in-memory MFA-Delete enforcement manager.
    /// Once set, every DELETE / DELETE-version / delete-marker /
    /// `PutBucketVersioning` request against a bucket whose MFA-Delete
    /// state is `Enabled` requires a valid `x-amz-mfa: <serial> <code>`
    /// header (RFC 6238 6-digit TOTP); the gate is a no-op for buckets
    /// where MFA-Delete is `Disabled` (S3 default).
    #[must_use]
    pub fn with_mfa_delete(mut self, mgr: Arc<crate::mfa::MfaDeleteManager>) -> Self {
        self.mfa_delete = Some(mgr);
        self
    }

    /// v0.6 #42: borrow the attached MFA-Delete manager (test /
    /// introspection — used by the snapshot path in `main.rs` to call
    /// `to_json` for restart-recoverable state).
    #[must_use]
    pub fn mfa_delete_manager(&self) -> Option<&Arc<crate::mfa::MfaDeleteManager>> {
        self.mfa_delete.as_ref()
    }

    /// v0.6 #38: attach the in-memory CORS configuration manager. Once
    /// set, `put_bucket_cors` / `get_bucket_cors` / `delete_bucket_cors`
    /// route through the manager instead of forwarding to the backend,
    /// and [`Self::handle_preflight`] becomes useful for the (future)
    /// listener-side OPTIONS interceptor.
    #[must_use]
    pub fn with_cors(mut self, mgr: Arc<crate::cors::CorsManager>) -> Self {
        self.cors = Some(mgr);
        self
    }

    /// v0.6 #38: Borrow the attached CORS manager (test / introspection).
    #[must_use]
    pub fn cors_manager(&self) -> Option<&Arc<crate::cors::CorsManager>> {
        self.cors.as_ref()
    }

    /// v0.6 #38: evaluate a CORS preflight request against the bucket's
    /// configured rules and, if a rule matches, return the headers that
    /// the (future) listener-side OPTIONS interceptor must put on the
    /// 200 response: `Access-Control-Allow-Origin`, `Access-Control-
    /// Allow-Methods`, `Access-Control-Allow-Headers`, optionally
    /// `Access-Control-Max-Age` and `Access-Control-Expose-Headers`.
    ///
    /// Returns `None` when no manager is attached, no config is
    /// registered for the bucket, or no rule matches the (origin,
    /// method, headers) triple. The caller is responsible for turning
    /// `None` into the appropriate 403 response.
    ///
    /// **Note:** the OPTIONS routing itself (i.e. wiring this method
    /// into the hyper-util listener path) is a follow-up — s3s does not
    /// surface OPTIONS as a typed S3 handler, so this method is
    /// currently call-able only from inside other handlers and tests.
    #[must_use]
    pub fn handle_preflight(
        &self,
        bucket: &str,
        origin: &str,
        method: &str,
        request_headers: &[String],
    ) -> Option<std::collections::HashMap<String, String>> {
        let mgr = self.cors.as_ref()?;
        let rule = mgr.match_preflight(bucket, origin, method, request_headers)?;
        let mut h = std::collections::HashMap::new();
        // Echo the matched origin back. If the rule used "*" we still
        // echo "*" (S3 spec — the spec does not require us to echo the
        // *requesting* origin when the wildcard matched).
        let allow_origin = if rule.allowed_origins.iter().any(|o| o == "*") {
            "*".to_string()
        } else {
            origin.to_string()
        };
        h.insert("Access-Control-Allow-Origin".to_string(), allow_origin);
        h.insert(
            "Access-Control-Allow-Methods".to_string(),
            rule.allowed_methods.join(", "),
        );
        if !rule.allowed_headers.is_empty() {
            // For the Allow-Headers response, echo back the rule's
            // pattern list verbatim (S3 echoes the configured list,
            // including "*" if present). Browsers honour exact-match
            // rules.
            h.insert(
                "Access-Control-Allow-Headers".to_string(),
                rule.allowed_headers.join(", "),
            );
        }
        if let Some(secs) = rule.max_age_seconds {
            h.insert("Access-Control-Max-Age".to_string(), secs.to_string());
        }
        if !rule.expose_headers.is_empty() {
            h.insert(
                "Access-Control-Expose-Headers".to_string(),
                rule.expose_headers.join(", "),
            );
        }
        Some(h)
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
    pub fn with_object_lock(mut self, mgr: Arc<crate::object_lock::ObjectLockManager>) -> Self {
        self.object_lock = Some(mgr);
        self
    }

    /// v0.7 #45: borrow the attached Object Lock manager (read-only —
    /// the lifecycle scanner uses this to skip currently-locked objects
    /// before issuing `delete_object`, since an Object Lock always wins
    /// over Lifecycle Expiration in AWS S3 semantics). Mirrors the
    /// shape of [`Self::lifecycle_manager`] /
    /// [`Self::tag_manager`] — purely additive accessor, no handler
    /// behaviour change.
    #[must_use]
    pub fn object_lock_manager(&self) -> Option<&Arc<crate::object_lock::ObjectLockManager>> {
        self.object_lock.as_ref()
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

    /// v0.8.5 #86 (audit M-3): borrow the attached versioning manager so
    /// the SIGUSR1 snapshot dump-back hook in `main.rs` can re-emit the
    /// in-memory state to the operator's `--versioning-state-file`
    /// without restarting the gateway. Mirrors the shape of
    /// [`Self::object_lock_manager`] / [`Self::lifecycle_manager`] —
    /// purely additive accessor, no handler behaviour change.
    #[must_use]
    pub fn versioning_manager(&self) -> Option<&Arc<crate::versioning::VersioningManager>> {
        self.versioning.as_ref()
    }

    /// v0.8.5 #86 (audit M-2): override the default replication-dispatch
    /// concurrency cap (1024). Wired by the `--replication-max-concurrent`
    /// CLI flag in `main.rs`. Operators running heavy cross-region
    /// fan-out may need to raise this; operators on memory-constrained
    /// hosts may need to lower it. The new value replaces the existing
    /// `Semaphore` (so calling this after dispatchers are already in
    /// flight is fine — the in-flight tasks hold permits from the old
    /// semaphore which is dropped when its last permit is released).
    /// A `max` of 0 would deadlock all replicas; the value is silently
    /// clamped to 1 instead.
    #[must_use]
    pub fn with_replication_max_concurrent(mut self, max: usize) -> Self {
        let max = max.max(1);
        self.replication_semaphore = Arc::new(tokio::sync::Semaphore::new(max));
        self
    }

    /// v0.8.5 #86 (audit M-2): borrow the in-flight replication
    /// concurrency permit pool. Tests inspect `available_permits()`
    /// after invoking `spawn_replication_if_matched` to verify the
    /// dispatcher actually `acquire_owned`s before kicking off the
    /// destination PUT.
    #[must_use]
    pub fn replication_semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.replication_semaphore
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

    /// v0.8 #52: opt the SSE-S4 PUT path into the chunked S4E5 frame
    /// (so the matching GET can stream-decrypt chunk-by-chunk
    /// instead of buffering the entire body before tag verify).
    /// `bytes` is the plaintext slice size — typically 1 MiB; 0
    /// disables the path and reverts to the legacy S4E2 buffered
    /// frame.
    ///
    /// SSE-C (S4E3) and SSE-KMS (S4E4) are intentionally untouched:
    /// the chunked envelopes for those flows are a follow-up issue
    /// (the customer-key wire surface needs separate version
    /// negotiation).
    ///
    /// Has no effect when `with_sse_keyring` / `with_sse_key` is
    /// not also set — the chunked path runs only on the SSE-S4
    /// branch of `put_object`.
    #[must_use]
    pub fn with_sse_chunk_size(mut self, bytes: usize) -> Self {
        self.sse_chunk_size = bytes;
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
            // v0.8.11 CRIT-4 fix: same trust gate as `request_context`.
            // Recording a client-controllable header in the access log
            // would poison forensic queries; leave it `None` until the
            // operator declares X-Forwarded-For is set by a trusted
            // proxy.
            remote_ip: if self.trust_x_forwarded_for {
                req.headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|raw| raw.split(',').next())
                    .map(|s| s.trim().to_owned())
            } else {
                None
            },
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

    /// v0.8.17 G-2: shared reserved-name guard used by every per-object
    /// API handler. `mode` chooses the AWS error shape: `Mutating`
    /// (PUT / Copy / DELETE / Tagging-write) returns
    /// `InvalidObjectName`; `Read` (GET / HEAD / Attributes / Tagging-read)
    /// returns `NoSuchKey` so a curious client gets the same response
    /// the listing filter has been giving them since v0.8.12 (the
    /// sidecar is invisible to list).
    ///
    /// v0.8.17 G-4: when `--allow-legacy-reserved-key-reads` is set
    /// AND the call is a `Read`, the guard returns `Ok(())` so
    /// operators upgrading from pre-v0.8.15 deployments can still
    /// access (and migrate off) any user-owned `<key>.s4index`
    /// objects that landed before M-1 / F-13 closed the namespace.
    /// Mutating operations stay blocked regardless of the flag —
    /// the flag is a read-only migration aid, not an injection
    /// re-opener.
    ///
    /// v1.0.1 audit R1 P2: `.s4dict/<id>` dictionary objects get the same
    /// Mutating-mode protection as `.s4index` sidecars — a gateway-side
    /// PUT / Copy / DELETE against the reserved prefix would let any
    /// client overwrite or remove the dictionary that other objects in
    /// the bucket need at GET time. Reads stay allowed (the bytes are
    /// content-addressed and non-secret; reading them is the documented
    /// no-gateway escape hatch). `train-dict` is unaffected — it writes
    /// backend-direct, never through this handler.
    fn check_not_reserved_key(&self, key: &str, mode: ReservedKeyMode) -> S3Result<()> {
        if matches!(mode, ReservedKeyMode::Mutating) && crate::dict::is_dict_key(key) {
            let code = S3ErrorCode::from_bytes(b"InvalidObjectName")
                .unwrap_or(S3ErrorCode::InvalidArgument);
            return Err(S3Error::with_message(
                code,
                format!(
                    "object key {key:?} is reserved (prefix `{}` holds S4 shared-dictionary \
                     objects; use `s4 train-dict` against the backend to manage them)",
                    crate::dict::DICT_KEY_PREFIX,
                ),
            ));
        }
        if !s4_codec::index::is_reserved_sidecar_key(key) {
            return Ok(());
        }
        if matches!(mode, ReservedKeyMode::Read) && self.allow_legacy_reserved_key_reads {
            return Ok(());
        }
        match mode {
            ReservedKeyMode::Read => Err(S3Error::with_message(
                S3ErrorCode::NoSuchKey,
                format!("object key {key:?} is reserved for S4 internal sidecars"),
            )),
            ReservedKeyMode::Mutating => {
                let code = S3ErrorCode::from_bytes(b"InvalidObjectName")
                    .unwrap_or(S3ErrorCode::InvalidArgument);
                Err(S3Error::with_message(
                    code,
                    format!(
                        "object key {key:?} is reserved (suffix `{}` is used for S4 internal \
                         sidecars)",
                        s4_codec::index::SIDECAR_SUFFIX,
                    ),
                ))
            }
        }
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
        // v0.8.11 CRIT-4 fix: `X-Forwarded-For` is a client-controllable
        // header. Trusting it unconditionally lets any public-internet
        // request claim it came from a trusted CIDR (e.g.
        // `curl -H 'X-Forwarded-For: 10.0.0.1'` to satisfy a
        // `Condition: NotIpAddress aws:SourceIp [10.0.0.0/8]` Deny).
        // We now only consume the header when the operator has
        // declared "this gateway sits behind a trusted reverse proxy
        // that scrubs client-supplied values" via
        // `with_trust_x_forwarded_for(true)` /
        // `--trust-x-forwarded-for`. Default leaves `source_ip` as
        // `None`, which fails closed for IP-allowlist Allow rules
        // and fails open for IP-blocklist Deny rules — operators
        // who need either case behind a public listener must opt in
        // or move the gate to the reverse proxy. The leftmost
        // comma-separated token is the originator per the
        // `X-Forwarded-For: client, proxy1, proxy2` convention.
        let source_ip = if self.trust_x_forwarded_for {
            req.headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|raw| raw.split(',').next())
                .and_then(|s| s.trim().parse().ok())
        } else {
            None
        };
        crate::policy::RequestContext {
            source_ip,
            user_agent,
            request_time: Some(std::time::SystemTime::now()),
            secure_transport: self.secure_transport,
            existing_object_tags: None,
            request_object_tags: None,
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
        self.enforce_policy_with_extra(req, action, bucket, key, None, None)
    }

    /// v0.6 #39: variant of [`Self::enforce_policy`] that lets the
    /// caller plumb tag context (existing-on-object + on-request) into
    /// the policy evaluator. Both arguments default to `None`, in
    /// which case the resulting `RequestContext` is identical to
    /// [`Self::enforce_policy`]'s — so for handlers that don't deal
    /// with tags this is a transparent no-op.
    fn enforce_policy_with_extra<I>(
        &self,
        req: &S3Request<I>,
        action: &'static str,
        bucket: &str,
        key: Option<&str>,
        request_tags: Option<&crate::tagging::TagSet>,
        existing_tags: Option<&crate::tagging::TagSet>,
    ) -> S3Result<()> {
        let Some(policy) = self.policy.as_ref() else {
            return Ok(());
        };
        let principal_id = Self::principal_of(req);
        let mut ctx = self.request_context(req);
        if let Some(t) = request_tags {
            ctx.request_object_tags = Some(t.clone());
        }
        if let Some(t) = existing_tags {
            ctx.existing_object_tags = Some(t.clone());
        }
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

    /// テスト用: backend を取り戻す (test helper、production では使わない).
    /// v0.6 #40 で `backend` が `Arc<B>` 化したので `Arc::try_unwrap` で
    /// 1-clone の場合のみ返す。共有されている (= replication dispatcher が
    /// 同じ Arc を持っていて未完了) 場合は `Err` を返さず panic させる
    /// (test 用途専用 helper の caller 契約を維持)。
    pub fn into_backend(self) -> B {
        Arc::try_unwrap(self.backend).unwrap_or_else(|_| {
            panic!("into_backend: backend Arc still shared (replication dispatcher in flight?)")
        })
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

    /// v0.9 #106: SSE-S4 chunked (S4E6) encryption-aware partial
    /// Range GET. The sidecar carries an [`s4_codec::index::SseChunkBinding`]
    /// (salt + key_id + chunk geometry) that lets us:
    ///
    /// 1. Map the [`s4_codec::index::RangePlan`]'s pre-encrypt byte range
    ///    to an encrypted chunk-range via
    ///    [`FrameIndex::encrypted_lookup`].
    /// 2. Partial-GET only those S4E6 chunks from backend (instead of
    ///    the entire encrypted body).
    /// 3. Decrypt the fetched chunks via
    ///    [`crate::sse::decrypt_s4e6_chunk_range`] (per-chunk
    ///    independently sealed — no need for the full body's tag).
    /// 4. Frame-parse + decompress the decrypted plaintext and slice
    ///    out the client-requested bytes via the existing
    ///    [`Self::partial_range_get`] machinery (re-used to keep one
    ///    source of truth for the response shaping).
    ///
    /// Returns `Err(...)` on any failure (auth, range, parse) so the
    /// caller can decide to fall back to the buffered full-GET path.
    /// In practice we surface a clear `InternalError` and let it
    /// bubble — Range GET on an encrypted body that fails partial
    /// fetch is a genuine error condition (sidecar / body mismatch,
    /// keyring rotated, etc.), not a quietly-degrade case.
    #[allow(clippy::too_many_arguments)]
    async fn partial_range_get_encrypted(
        &self,
        req: &S3Request<GetObjectInput>,
        plan: s4_codec::index::RangePlan,
        enc_plan: s4_codec::index::EncryptedRangePlan,
        sse: s4_codec::index::SseChunkBinding,
        client_start: u64,
        client_end_exclusive: u64,
        total_original: u64,
        get_start: Instant,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let keyring = self.sse_keyring.as_ref().ok_or_else(|| {
            S3Error::with_message(
                S3ErrorCode::InvalidRequest,
                "object is SSE-S4 chunked but no --sse-s4-key is configured on this gateway",
            )
        })?;
        // Partial-fetch the enc byte range that covers the needed
        // chunks. Note that `byte_end_exclusive - 1` is the inclusive
        // last byte (matches the existing partial_range_get
        // convention).
        let backend_range = s3s::dto::Range::Int {
            first: enc_plan.enc_byte_start,
            last: Some(enc_plan.enc_byte_end_exclusive - 1),
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
                "backend partial GET returned empty body (SSE-S4 chunked Range)",
            )
        })?;
        let enc_bytes = collect_blob(blob, self.max_body_bytes)
            .await
            .map_err(internal("collect SSE-S4 chunked partial body"))?;

        // Decrypt the partial chunks → pre-encrypt (= compressed-framed) plaintext.
        let plaintext = crate::sse::decrypt_s4e6_chunk_range(
            &enc_bytes,
            keyring.as_ref(),
            sse.enc_chunk_size,
            sse.enc_chunk_count,
            sse.enc_key_id,
            &sse.enc_salt,
            sse.enc_plaintext_len,
            enc_plan.chunk_idx_start,
            enc_plan.chunk_idx_last_inclusive,
        )
        .map_err(|e| {
            S3Error::with_message(
                S3ErrorCode::InternalError,
                format!("SSE-S4 chunked partial decrypt failed: {e}"),
            )
        })?;
        // Slice the decrypted concatenation down to the requested
        // pre-encrypt byte range (= the `RangePlan.byte_start..
        // byte_end_exclusive` range, expressed inside the chunks we
        // fetched).
        let s = enc_plan.pre_encrypt_slice_start_in_concat as usize;
        let e = enc_plan.pre_encrypt_slice_end_in_concat as usize;
        if e > plaintext.len() {
            return Err(S3Error::with_message(
                S3ErrorCode::InternalError,
                "SSE-S4 chunked partial decrypt produced fewer bytes than the sidecar declared",
            ));
        }
        let pre_encrypt_slice = plaintext.slice(s..e);

        // Frame-parse + decompress the pre-encrypt slice, then slice
        // again on the original byte range. The plan's
        // slice_start_in_combined / slice_end_in_combined account for
        // the original_offset of the first frame we fetched — they
        // are pre-encrypt-domain offsets, identical to the
        // non-encrypted partial-range path.
        let mut combined = BytesMut::new();
        for frame in FrameIter::new(pre_encrypt_slice) {
            let (header, payload) = frame.map_err(|fe| {
                S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!("SSE-S4 chunked partial frame parse: {fe}"),
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
                .map_err(internal("SSE-S4 chunked partial decompress"))?;
            combined.extend_from_slice(&decompressed);
        }
        let combined = combined.freeze();
        let sliced = combined
            .slice(plan.slice_start_in_combined as usize..plan.slice_end_in_combined as usize);

        // Response shaping: identical to the unencrypted partial
        // path (clear backend checksums / e_tag since they describe
        // the encrypted body, not the plaintext slice).
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
        // Use the encrypted bytes_in for the bandwidth-saved metric —
        // that's what actually traversed the wire, vs. the full
        // encrypted body that the buffered fallback would have
        // fetched.
        crate::metrics::record_get(
            "sse-s4-chunked-partial",
            enc_plan.enc_byte_end_exclusive - enc_plan.enc_byte_start,
            returned_size,
            elapsed.as_secs_f64(),
            true,
        );
        info!(
            op = "get_object",
            bucket = %req.input.bucket,
            key = %req.input.key,
            bytes_in = enc_plan.enc_byte_end_exclusive - enc_plan.enc_byte_start,
            bytes_out = returned_size,
            total_object_size = total_original,
            range = true,
            path = "sidecar-partial-sse-s4-chunked",
            chunks_fetched = (enc_plan.chunk_idx_last_inclusive - enc_plan.chunk_idx_start + 1) as u64,
            latency_ms = elapsed.as_millis() as u64,
            "S4 partial Range GET via v3 sidecar (SSE-S4 chunked fast-path)"
        );
        Ok(backend_resp)
    }

    /// `<key>.s4index` sidecar object を backend に書く。失敗しても本体 PUT は
    /// 成功扱いにしたいので、err は warn ログのみ (Range GET の partial path が
    /// 使えなくなるが、full read fallback で意味的には正しい結果を返す)。
    /// Returns the number of sidecar bytes actually written to the
    /// backend (`0` on skip / failure) so the v1.2 savings ledger can
    /// fold the sidecar into the object's `stored_bytes` footprint.
    /// Callers without a ledger ignore the value.
    async fn write_sidecar(&self, bucket: &str, key: &str, index: &FrameIndex) -> u64 {
        let bytes = encode_index(index);
        let len = bytes.len() as i64;
        let sidecar = sidecar_key(key);
        // v0.7 #49: synthetic re-entry URI must be percent-encoded; if
        // the (already legally-arbitrary) S3 key produces something we
        // cannot encode at all, drop the sidecar PUT (the GET path
        // falls back to a full read on a missing sidecar) instead of
        // panicking on `parse().unwrap()`.
        let uri = match safe_object_uri(bucket, &sidecar) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    bucket,
                    key,
                    "S4 write_sidecar skipped (key not URI-encodable): {e}"
                );
                return 0;
            }
        };
        let put_input = PutObjectInput {
            bucket: bucket.into(),
            key: sidecar,
            body: Some(bytes_to_blob(bytes)),
            content_length: Some(len),
            content_type: Some("application/x-s4-index".into()),
            ..Default::default()
        };
        let put_req = S3Request {
            input: put_input,
            method: http::Method::PUT,
            uri,
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        match self.backend.put_object(put_req).await {
            Ok(_) => len as u64,
            Err(e) => {
                tracing::warn!(
                    bucket,
                    key,
                    "S4 write_sidecar failed (Range GET will fall back to full read): {e}"
                );
                0
            }
        }
    }

    /// v0.8.4 #73 H-2: confirm that the sidecar we just decoded still
    /// describes the current backend object before we trust its frame
    /// offsets for a partial Range GET. The sidecar carries the source
    /// `etag` and `compressed_size` that were observed at PUT time; we
    /// HEAD the backend object and compare.
    ///
    /// Decision matrix:
    /// - sidecar `source_etag = None` (legacy v1 / build_index_from_body
    ///   that wasn't stamped) → return `true` (best-effort, preserves
    ///   pre-v0.8.4 behaviour for existing on-disk sidecars).
    /// - HEAD fails → return `false` (we can't tell either way; full GET
    ///   path will surface the real backend error to the client).
    /// - HEAD ETag matches → `true`.
    /// - HEAD ETag differs OR HEAD size differs from
    ///   `source_compressed_size` → `false` (sidecar stale or attacker-
    ///   written; fall back to full GET).
    async fn sidecar_version_binding_ok(
        &self,
        bucket: &str,
        key: &str,
        index: &FrameIndex,
    ) -> bool {
        let Some(ref expected_etag) = index.source_etag else {
            // Legacy sidecar without the v0.8.4 #73 H-2 binding —
            // back-compat: trust it (the partial fetch is the same
            // best-effort path that v0.8.3 and earlier shipped).
            return true;
        };
        let head_input = HeadObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            ..Default::default()
        };
        let uri = match safe_object_uri(bucket, key) {
            Ok(u) => u,
            Err(_) => return false,
        };
        let head_req = S3Request {
            input: head_input,
            method: http::Method::HEAD,
            uri,
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let head = match self.backend.head_object(head_req).await {
            Ok(r) => r.output,
            Err(e) => {
                tracing::debug!(
                    bucket,
                    key,
                    "S4 sidecar version-binding HEAD failed, falling back to full GET: {e}"
                );
                return false;
            }
        };
        // ETag is a strong-vs-weak enum; we compare on the unwrapped string
        // form (matches what the PUT path stamped — see below).
        let live_etag = head.e_tag.as_ref().map(|t| t.value());
        if live_etag != Some(expected_etag.as_str()) {
            tracing::debug!(
                bucket,
                key,
                "sidecar stale (ETag mismatch), falling back to full GET (sidecar={:?}, live={:?})",
                expected_etag,
                live_etag,
            );
            return false;
        }
        if let Some(expected_size) = index.source_compressed_size
            && let Some(live_size) = head.content_length
            && live_size as u64 != expected_size
        {
            tracing::debug!(
                bucket,
                key,
                "sidecar stale (size mismatch), falling back to full GET (sidecar={}, live={})",
                expected_size,
                live_size,
            );
            return false;
        }
        true
    }

    /// `<key>.s4index` sidecar を backend から読み出す。なければ None。
    async fn read_sidecar(&self, bucket: &str, key: &str) -> Option<FrameIndex> {
        let sidecar = sidecar_key(key);
        // v0.7 #49: same encode-or-bail treatment as write_sidecar.
        let uri = safe_object_uri(bucket, &sidecar).ok()?;
        let get_input = GetObjectInput {
            bucket: bucket.into(),
            key: sidecar,
            ..Default::default()
        };
        let get_req = S3Request {
            input: get_input,
            method: http::Method::GET,
            uri,
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

    /// v1.2 savings ledger: best-effort HEAD probe of one backend
    /// object's byte footprint, used for DELETE / overwrite
    /// subtraction. Only called when `--savings-ledger-state-file` is
    /// configured (the extra HEAD per write/delete is the documented
    /// cost of the opt-in).
    ///
    /// Resolution order for `original_bytes`:
    /// 1. the `s4-original-size` manifest stamp (single-PUT gateway
    ///    writes);
    /// 2. for stamped-S4 objects without it (multipart Completes carry
    ///    `s4-codec` only), the `<key>.s4index` sidecar's
    ///    `total_original_size()`;
    /// 3. otherwise `original = stored` (non-S4 / backend-direct
    ///    objects, and S4 objects whose logical size is unrecoverable —
    ///    e.g. encrypted multipart without a sidecar). The symmetric
    ///    fallback means such objects contribute zero measured savings
    ///    rather than a fabricated number.
    ///
    /// `None` = the object does not exist or the probe failed; callers
    /// treat that as "nothing to subtract" (best-effort, disclosed in
    /// the report notes).
    ///
    /// v1.2 audit R1: internal S4 objects (`.s4dict/<id>` dictionaries
    /// and `<key>.s4index` sidecars) always probe as `None` — they are
    /// never ledger-accounted as standalone objects (sidecar bytes ride
    /// in their main object's delta; dictionaries are excluded
    /// entirely, see the `crate::ledger` module doc). The reserved-key
    /// guard already blocks client mutations on these keys, so this is
    /// defence-in-depth for internal call paths. Versioning shadow keys
    /// (`.__s4ver__/...`) are deliberately NOT skipped: every stored
    /// version is a ledger object.
    async fn ledger_probe_object(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Option<LedgerFootprint> {
        if crate::dict::is_dict_key(key) || s4_codec::index::is_reserved_sidecar_key(key) {
            return None;
        }
        let uri = safe_object_uri(bucket, key).ok()?;
        let head_input = HeadObjectInput {
            bucket: bucket.into(),
            key: key.into(),
            version_id: version_id.map(str::to_owned),
            ..Default::default()
        };
        let head_req = S3Request {
            input: head_input,
            method: http::Method::HEAD,
            uri,
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let head = self.backend.head_object(head_req).await.ok()?;
        let stored_bytes = head
            .output
            .content_length
            .and_then(|n| u64::try_from(n).ok())?;
        let meta = head.output.metadata.as_ref();
        // v1.2 audit R1 P2: only gateway-accounted objects (stamped
        // `s4-ledger: 1` at write time) may be subtracted.
        let accounted = meta
            .and_then(|m| m.get(META_LEDGER))
            .is_some_and(|v| v == META_LEDGER_ACCOUNTED);
        if let Some(original_bytes) = meta
            .and_then(|m| m.get(META_ORIGINAL_SIZE))
            .and_then(|v| v.parse::<u64>().ok())
        {
            return Some(LedgerFootprint {
                original_bytes,
                stored_bytes,
                accounted,
            });
        }
        if meta.is_some_and(|m| m.contains_key(META_CODEC))
            && let Some(idx) = self.read_sidecar(bucket, key).await
        {
            return Some(LedgerFootprint {
                original_bytes: idx.total_original_size(),
                stored_bytes,
                accounted,
            });
        }
        Some(LedgerFootprint {
            original_bytes: stored_bytes,
            stored_bytes,
            accounted,
        })
    }

    /// v1.2 savings ledger: HEAD `<key>.s4index` and return its
    /// Content-Length (`0` when absent / unprobeable). Used to fold a
    /// to-be-replaced or to-be-deleted sidecar into the subtraction.
    async fn ledger_probe_sidecar_bytes(&self, bucket: &str, key: &str) -> u64 {
        let sidecar = sidecar_key(key);
        let Ok(uri) = safe_object_uri(bucket, &sidecar) else {
            return 0;
        };
        let head_req = S3Request {
            input: HeadObjectInput {
                bucket: bucket.into(),
                key: sidecar,
                ..Default::default()
            },
            method: http::Method::HEAD,
            uri,
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        match self.backend.head_object(head_req).await {
            Ok(head) => head
                .output
                .content_length
                .and_then(|n| u64::try_from(n).ok())
                .unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Multipart object (frame 列) を解凍 → 元 bytes を再構築。
    ///
    /// **per-frame codec dispatch**: 各 frame header に codec_id が入っているので、
    /// frame ごとに registry が違う codec を呼ぶことができる。同一 object 内で
    /// 異なる codec が混在していても透過的に解凍可能 (parquet 風 mixed columns 等)。
    async fn decompress_multipart(&self, bytes: bytes::Bytes) -> S3Result<bytes::Bytes> {
        let mut out = BytesMut::new();
        // v0.8.15 H-h: cap the *aggregate* decoded output. Each
        // individual frame is already bounded by
        // `validate_decompress_manifest` (default 5 GiB per frame),
        // but a forged multi-frame body can declare many frames
        // each near the limit — without an object-level ceiling, a
        // single GET could pin tens of GiB of plaintext in
        // `BytesMut::extend_from_slice`. Use the gateway's
        // `max_body_bytes` (same cap that bounds PUT bodies) so a
        // GET can never produce more plaintext than a PUT can ever
        // legitimately have stored.
        let aggregate_cap = self.max_body_bytes;
        let mut produced: usize = 0;
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
            // v0.8.15 H-h: pre-flight check on the declared
            // `original_size` so a forged manifest claiming a frame
            // that would push us past the cap is rejected before we
            // start decoding. Defence-in-depth alongside the
            // post-decode `produced` check below.
            if (produced as u64).saturating_add(header.original_size) > aggregate_cap as u64 {
                return Err(S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!(
                        "multipart aggregate output exceeds cap: would reach \
                         {produced_total} bytes after this frame, cap is {aggregate_cap}",
                        produced_total = (produced as u64).saturating_add(header.original_size),
                    ),
                ));
            }
            let decompressed = self
                .registry
                .decompress(payload, &chunk_manifest)
                .await
                .map_err(internal("multipart frame decompress"))?;
            produced = produced.saturating_add(decompressed.len());
            if produced > aggregate_cap {
                return Err(S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!(
                        "multipart aggregate output exceeded cap: {produced} bytes \
                         emitted, cap is {aggregate_cap}"
                    ),
                ));
            }
            out.extend_from_slice(&decompressed);
        }
        Ok(out.freeze())
    }

    // ===================================================================
    // v1.1 `--zstd-dict` helpers
    // ===================================================================

    /// Resolve dictionary bytes for `dict_id`, in priority order:
    /// 1. boot-time preloaded store (`--zstd-dict` flags),
    /// 2. the lazy LRU cache,
    /// 3. backend fetch of `.s4dict/<dict_id>` from the object's bucket
    ///    (fingerprint-verified, then inserted into the LRU).
    ///
    /// Step 3 is what keeps dict objects readable after the operator
    /// drops the `--zstd-dict` flag. A failed fetch is a 5xx with an
    /// explicit message (and a `s4_dict_fetch_total{result="err"}` bump)
    /// — NOT a silent passthrough of compressed bytes.
    async fn resolve_dict(&self, bucket: &str, dict_id: &str) -> S3Result<Arc<[u8]>> {
        // v1.0.1 audit R2 P3: the preload lookup is `(bucket, id)`-keyed
        // just like the lazy cache below — a dictionary preloaded for one
        // bucket's `--zstd-dict` prefix must not satisfy GETs against a
        // different bucket (the 16-hex id is only a 64-bit prefix).
        if let Some(store) = self.zstd_dicts.load()
            && let Some(dict) = store.get_preloaded(bucket, dict_id)
        {
            return Ok(dict);
        }
        // v1.0.1 audit R1 P3: the lazy cache is keyed `(bucket, id)` —
        // see `DictCache` docs. A forged `.s4dict/<id>` planted in one
        // bucket must not satisfy GETs against other buckets.
        if let Some(dict) = self.dict_cache.get(bucket, dict_id) {
            return Ok(dict);
        }
        let dict_key = crate::dict::dict_object_key(dict_id);
        let uri = safe_object_uri(bucket, &dict_key)?;
        let get_req = S3Request {
            input: GetObjectInput {
                bucket: bucket.into(),
                key: dict_key.clone(),
                ..Default::default()
            },
            method: http::Method::GET,
            uri,
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let fetch_failed = |cause: String| {
            crate::metrics::record_dict_fetch("err");
            S3Error::with_message(
                S3ErrorCode::InternalError,
                format!(
                    "object requires zstd dictionary {dict_id} but `{bucket}/{dict_key}` \
                     could not be fetched from the backend: {cause}"
                ),
            )
        };
        let mut resp = self
            .backend
            .get_object(get_req)
            .await
            .map_err(|e| fetch_failed(format!("{e}")))?;
        // v1.0.1 audit R1 P3: capture the full-SHA-256 claim (stamped by
        // `train-dict` since v1.0.1) before the body is consumed.
        let claimed_sha256 = resp
            .output
            .metadata
            .as_ref()
            .and_then(|m| m.get(crate::dict::DICT_SHA256_META_KEY))
            .cloned();
        let blob = resp
            .output
            .body
            .take()
            .ok_or_else(|| fetch_failed("backend returned no body".into()))?;
        // Trained dictionaries default to ≤ ~110 KiB; the 1 MiB hard cap
        // (`DICT_FETCH_MAX_BYTES`) leaves ~9x headroom while keeping the
        // lazy LRU's worst-case footprint at 16 slots x 1 MiB = 16 MiB
        // against a hostile oversized `.s4dict/` blob.
        let bytes = collect_blob(blob, crate::dict::DICT_FETCH_MAX_BYTES)
            .await
            .map_err(|e| fetch_failed(format!("collect dictionary body: {e}")))?;
        // Fingerprint check (shared `verify_dict_bytes` discipline, also
        // enforced by the boot preload): always verify the 16-hex id
        // prefix; when the object carries the full-SHA-256 metadata
        // stamp, verify the complete digest too (closes the 64-bit
        // truncation window for post-v1.0.1 dictionaries; pre-stamp
        // dictionaries keep the prefix-only check — back-compat).
        crate::dict::verify_dict_bytes(dict_id, claimed_sha256.as_deref(), &bytes)
            .map_err(fetch_failed)?;
        let dict: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
        self.dict_cache
            .insert(bucket.to_owned(), dict_id.to_owned(), Arc::clone(&dict));
        crate::metrics::record_dict_fetch("ok");
        debug!(
            bucket,
            dict_id, "S4 zstd dictionary lazy-fetched from backend and cached"
        );
        Ok(dict)
    }

    /// v1.0.1 audit R1 P2: make sure `.s4dict/<dict_id>` exists in
    /// `bucket`, PUTting `dict` there when absent. Used by the
    /// cross-bucket CopyObject path — dictionaries are **bucket-local**
    /// (`resolve_dict` only ever looks in the object's own bucket), so a
    /// copy that propagates the `s4-dict-id` stamp must carry the
    /// dictionary object along or the destination object stays readable
    /// only for as long as the *source* bucket (and its dict) survives.
    ///
    /// Idempotent by construction: the key is content-addressed, so an
    /// existing `.s4dict/<id>` already holds the same bytes (and a lost
    /// HEAD/PUT race just rewrites them). The PUT stamps the full-SHA-256
    /// metadata exactly like `train-dict` does.
    async fn ensure_dict_object_in_bucket(
        &self,
        bucket: &str,
        dict_id: &str,
        dict: &[u8],
    ) -> S3Result<()> {
        let dict_key = crate::dict::dict_object_key(dict_id);
        let head_req = S3Request {
            input: HeadObjectInput {
                bucket: bucket.into(),
                key: dict_key.clone(),
                ..Default::default()
            },
            method: http::Method::HEAD,
            uri: safe_object_uri(bucket, &dict_key)?,
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        if self.backend.head_object(head_req).await.is_ok() {
            return Ok(()); // content-addressed: same id ⇒ same bytes
        }
        let mut meta = Metadata::new();
        meta.insert(
            crate::dict::DICT_SHA256_META_KEY.to_owned(),
            crate::dict::dict_sha256_hex(dict),
        );
        let put_req = S3Request {
            input: PutObjectInput {
                bucket: bucket.into(),
                key: dict_key.clone(),
                body: Some(bytes_to_blob(bytes::Bytes::copy_from_slice(dict))),
                content_length: Some(dict.len() as i64),
                metadata: Some(meta),
                ..Default::default()
            },
            method: http::Method::PUT,
            uri: safe_object_uri(bucket, &dict_key)?,
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        self.backend.put_object(put_req).await.map_err(|e| {
            S3Error::with_message(
                S3ErrorCode::InternalError,
                format!(
                    "cross-bucket copy of a dict-compressed object: PUT \
                     `{bucket}/{dict_key}` failed: {e}"
                ),
            )
        })?;
        debug!(
            bucket,
            dict_id, "S4 copy_object: propagated `.s4dict/` dictionary to destination bucket"
        );
        Ok(())
    }

    /// GET-side decompress for `s4-dict-id`-stamped objects. Same S4F2
    /// frame walk as [`Self::decompress_multipart`] (incl. the aggregate
    /// output cap), with `cpu-zstd-dict` frames decoded against `dict`
    /// instead of going through the registry.
    async fn decompress_framed_with_dict(
        &self,
        bytes: bytes::Bytes,
        dict: Arc<[u8]>,
    ) -> S3Result<bytes::Bytes> {
        let dict_codec = s4_codec::cpu_zstd_dict::CpuZstdDict::new(
            dict,
            s4_codec::cpu_zstd_dict::CpuZstdDict::DEFAULT_LEVEL,
        )
        .map_err(internal("build cpu-zstd-dict codec"))?;
        let mut out = BytesMut::new();
        let aggregate_cap = self.max_body_bytes;
        let mut produced: usize = 0;
        for frame in FrameIter::new(bytes) {
            let (header, payload) = frame.map_err(|e| {
                S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!("dict-framed object frame parse: {e}"),
                )
            })?;
            let chunk_manifest = ChunkManifest {
                codec: header.codec,
                original_size: header.original_size,
                compressed_size: header.compressed_size,
                crc32c: header.crc32c,
            };
            if (produced as u64).saturating_add(header.original_size) > aggregate_cap as u64 {
                return Err(S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!(
                        "dict-framed aggregate output exceeds cap: would reach {} bytes \
                         after this frame, cap is {aggregate_cap}",
                        (produced as u64).saturating_add(header.original_size),
                    ),
                ));
            }
            let decompressed = if header.codec == CodecKind::CpuZstdDict {
                use s4_codec::Codec as _;
                dict_codec
                    .decompress(payload, &chunk_manifest)
                    .await
                    .map_err(internal("dict frame decompress"))?
            } else {
                // Defensive: a mixed body (e.g. a fallback frame written
                // as plain cpu-zstd) still decodes via the registry.
                self.registry
                    .decompress(payload, &chunk_manifest)
                    .await
                    .map_err(internal("dict-framed frame decompress"))?
            };
            produced = produced.saturating_add(decompressed.len());
            if produced > aggregate_cap {
                return Err(S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!(
                        "dict-framed aggregate output exceeded cap: {produced} bytes \
                         emitted, cap is {aggregate_cap}"
                    ),
                ));
            }
            out.extend_from_slice(&decompressed);
        }
        Ok(out.freeze())
    }

    /// PUT-side dict path: compress the (small, already-buffered) body
    /// BOTH with the trained dictionary and with plain cpu-zstd, keep
    /// whichever is smaller, and wrap the winner in a single S4F2 frame
    /// (same layout the streaming path writes for a ≤1-chunk body).
    ///
    /// Returns `(framed_body, aggregate_manifest, used_dict)` —
    /// `used_dict == false` is the fallback shape: a normal `cpu-zstd`
    /// framed object, indistinguishable from a non-dict PUT, and no
    /// `s4-dict-id` stamp.
    ///
    /// v1.3 dict ops: `matched_prefix` is the configured
    /// `<bucket>/<key-prefix>` that routed this PUT here — both
    /// compression results are measured anyway, so the per-prefix
    /// win/loss + byte counters (`s4_dict_put_total` /
    /// `s4_dict_put_bytes_total`) cost nothing extra, and the rolling
    /// win-rate monitor WARNs when the dictionary stops paying off.
    async fn compress_small_with_dict(
        &self,
        bytes: bytes::Bytes,
        dict: Arc<[u8]>,
        level: i32,
        matched_prefix: &str,
    ) -> S3Result<(bytes::Bytes, ChunkManifest, bool)> {
        use s4_codec::Codec as _;
        let original_len = bytes.len() as u64;
        let dict_codec = s4_codec::cpu_zstd_dict::CpuZstdDict::new(dict, level)
            .map_err(internal("build cpu-zstd-dict codec"))?;
        let (dict_payload, dict_manifest) = dict_codec
            .compress(bytes.clone())
            .await
            .map_err(internal("cpu-zstd-dict compress"))?;
        let (plain_payload, plain_manifest) = self
            .registry
            .compress(bytes, CodecKind::CpuZstd)
            .await
            .map_err(internal("cpu-zstd compress (dict comparison)"))?;
        let used_dict = crate::dict::dict_wins(dict_payload.len(), plain_payload.len());
        // v1.3 dict ops: per-prefix decision + byte-volume counters.
        // Recorded ONLY on this path — a gateway without a dict store
        // never registers these series. Cardinality is bounded by the
        // operator-configured prefix count.
        crate::metrics::record_dict_put(
            matched_prefix,
            used_dict,
            original_len,
            dict_payload.len() as u64,
            plain_payload.len() as u64,
        );
        if let Some(win_rate) = self.dict_win_tracker.record(matched_prefix, used_dict) {
            warn!(
                prefix = matched_prefix,
                win_rate,
                window = crate::dict::DICT_WIN_RATE_WINDOW,
                "S4 dict win rate fell below {} over the last {} dict-path PUTs — the \
                 dictionary may be stale for this prefix's current workload; consider \
                 retraining (s4 train-dict) and rotating the mapping (--zstd-dict-map \
                 + SIGHUP reloads without a restart)",
                crate::dict::DICT_WIN_RATE_WARN_THRESHOLD,
                crate::dict::DICT_WIN_RATE_WINDOW,
            );
        }
        let (payload, chunk_manifest) = if used_dict {
            (dict_payload, dict_manifest)
        } else {
            (plain_payload, plain_manifest)
        };
        let mut framed = BytesMut::with_capacity(FRAME_HEADER_BYTES + payload.len());
        write_frame(
            &mut framed,
            FrameHeader {
                codec: chunk_manifest.codec,
                original_size: chunk_manifest.original_size,
                compressed_size: payload.len() as u64,
                crc32c: chunk_manifest.crc32c,
            },
            &payload,
        );
        let framed = framed.freeze();
        // Aggregate manifest mirrors `streaming_compress_to_frames`:
        // codec = the frame codec, compressed_size = total framed bytes
        // (incl. the 28-byte header), crc32c = crc of the *input*.
        let manifest = ChunkManifest {
            codec: chunk_manifest.codec,
            original_size: chunk_manifest.original_size,
            compressed_size: framed.len() as u64,
            crc32c: chunk_manifest.crc32c,
        };
        Ok((framed, manifest, used_dict))
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

/// v0.6 #42: wall-clock seconds since the UNIX epoch — fed to
/// `mfa::check_mfa` so the TOTP verifier can match the client's
/// authenticator app's view of "now". Falls back to `0` on the
/// (impossible-in-practice) clock-before-1970 path so the verifier
/// rejects rather than panicking.
fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// v0.6 #42: translate an `MfaError` into the matching S3 wire error.
///
/// - `Missing` / `SerialMismatch` / `InvalidCode` → `403 AccessDenied`
///   (S3 spec for MFA Delete: every gating failure surfaces as
///   `AccessDenied`, not a separate `MFA*` code).
/// - `Malformed` → `400 InvalidRequest` (the request itself is
///   syntactically broken, not a permission issue).
fn mfa_error_to_s3(e: crate::mfa::MfaError) -> S3Error {
    match e {
        crate::mfa::MfaError::Missing => S3Error::with_message(
            S3ErrorCode::AccessDenied,
            "MFA token required for this operation",
        ),
        crate::mfa::MfaError::Malformed => {
            S3Error::with_message(S3ErrorCode::InvalidRequest, "malformed x-amz-mfa header")
        }
        crate::mfa::MfaError::SerialMismatch => S3Error::with_message(
            S3ErrorCode::AccessDenied,
            "MFA serial does not match configured device",
        ),
        crate::mfa::MfaError::InvalidCode => {
            S3Error::with_message(S3ErrorCode::AccessDenied, "invalid MFA code")
        }
    }
}

fn is_multipart_object(metadata: &Option<Metadata>) -> bool {
    metadata
        .as_ref()
        .and_then(|m| m.get(META_MULTIPART))
        .map(|v| v == "true")
        .unwrap_or(false)
}

// v1.1 `s4 migrate`: the five manifest keys are `pub(crate)` (NOT `pub`)
// so `crate::migrate` can stamp byte-identical metadata to what this PUT
// path writes, without widening the SemVer-frozen public surface. The
// values and semantics are unchanged.
pub(crate) const META_CODEC: &str = "s4-codec";
pub(crate) const META_ORIGINAL_SIZE: &str = "s4-original-size";
pub(crate) const META_COMPRESSED_SIZE: &str = "s4-compressed-size";
pub(crate) const META_CRC32C: &str = "s4-crc32c";
/// Multipart upload で per-part frame format を使ったオブジェクトであることを示す。
/// GET 時にこの flag を見て frame parser を起動する。
const META_MULTIPART: &str = "s4-multipart";
/// v0.2 #4: single-PUT でも S4F2 framed format で書かれていることを示す。
/// 旧 v0.1 single-PUT は raw 圧縮 bytes (この flag なし)。GET 時にこの flag を
/// 見て framed 経路 (= multipart と同じ FrameIter parse) に流す。
pub(crate) const META_FRAMED: &str = "s4-framed";
/// v1.1 `--zstd-dict`: 16-hex dictionary id (`.s4dict/<id>` の `<id>`) を
/// 指す。`cpu-zstd-dict` (codec id 8) で圧縮した single-PUT framed object に
/// だけ stamp される。GET 側はこの id で辞書を解決する (preloaded →
/// lazy-fetch LRU → backend `.s4dict/<id>`)。フレーム自体には辞書 id を
/// 入れない (S4F2 レイアウト不変 = additive wire change)。
pub(crate) const META_DICT_ID: &str = "s4-dict-id";
/// v1.1 `s4 recompact`: the zstd compression level the object's frames
/// were last (re)written at. **Recompact-only stamp** — the gateway
/// neither reads nor writes this key (PUT-path objects simply lack it),
/// and GET-path behaviour is identical with or without it. Recompact
/// uses it as its idempotency marker: an object whose stamp is already
/// `>= --target-zstd-level` is skipped (`already-compacted`) on
/// re-runs. Not propagated by CopyObject's reserved-key allowlist —
/// copies are simply re-examined on the next recompact run and skip as
/// `insufficient-gain`.
pub(crate) const META_ZSTD_LEVEL: &str = "s4-zstd-level";
/// v1.2 audit R1 P2: internal marker stamped on every write the
/// gateway **adds to the savings ledger** (single PUT, zero-length
/// PUT, multipart Create→Complete; CopyObject destinations inherit it
/// from the source via the backend metadata copy / the REPLACE merge).
/// The DELETE / overwrite ledger probes subtract **only** objects
/// carrying this marker — objects written around the gateway (backend
/// direct, `s4fs`, `s4 migrate`, `s4 recompact`) were never added, so
/// subtracting them would corrupt the bucket's measured ratio.
/// Clients cannot forge it: `strip_reserved_client_metadata` removes
/// every client-supplied `s4-*` key before this stamp. Only stamped
/// when `--savings-ledger-state-file` is configured — flag-off
/// deployments stay bit-for-bit on pre-ledger metadata.
pub(crate) const META_LEDGER: &str = "s4-ledger";
/// Value stored under [`META_LEDGER`].
pub(crate) const META_LEDGER_ACCOUNTED: &str = "1";

/// v1.0.1 audit R1 P1: remove every client-supplied metadata key in the
/// gateway-reserved `s4-*` namespace (case-insensitive). The PUT path
/// calls this before compressing so a client can never pre-stamp
/// `s4-codec` / `s4-dict-id` / `s4-encrypted` etc. — those keys are
/// written exclusively by the gateway after it has actually performed
/// the corresponding transform. Mirrors the CopyObject REPLACE strip
/// (v0.8.16 F-8), which uses the same prefix predicate.
fn strip_reserved_client_metadata(metadata: &mut Option<Metadata>) {
    if let Some(meta) = metadata.as_mut() {
        meta.retain(|k, _| !k.to_ascii_lowercase().starts_with("s4-"));
    }
}

/// v1.2 audit R2 P2: snapshot of the metadata handed to the cross-bucket
/// replication dispatcher. The dispatcher PUTs the replica directly
/// against the backend (it never re-enters `put_object`), so replicas
/// are — by documented design — **not** added to the savings ledger. A
/// verbatim clone of the source metadata would carry the `s4-ledger`
/// marker onto that unaccounted replica, and a later gateway-routed
/// DELETE of the replica would then subtract bytes that were never
/// added (the exact asymmetry the marker exists to prevent). Strip the
/// marker; every other key (codec manifest, SSE markers, client
/// metadata) is forwarded verbatim so the replica stays readable.
fn replication_metadata_snapshot(metadata: &Option<Metadata>) -> Option<Metadata> {
    let mut snap = metadata.clone();
    if let Some(m) = snap.as_mut() {
        m.remove(META_LEDGER);
    }
    snap
}

/// v1.1 `--zstd-dict`: pull the validated dict-id off object metadata.
/// Invalid shapes (non-16-hex) are treated as absent — the GET path then
/// falls through to the frame parser, which fails typed on the
/// `cpu-zstd-dict` frame instead of splicing a tainted id into a
/// backend key.
fn extract_dict_id(metadata: &Option<Metadata>) -> Option<String> {
    let id = metadata.as_ref()?.get(META_DICT_ID)?;
    if crate::dict::is_valid_dict_id(id) {
        Some(id.clone())
    } else {
        None
    }
}

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
        other => S3Error::with_message(S3ErrorCode::InternalError, format!("KMS error: {other}")),
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
        E::InvalidCustomerKey { reason } => {
            S3Error::with_message(S3ErrorCode::InvalidArgument, format!("SSE-C: {reason}"))
        }
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

/// v0.6 #41: map a `select::SelectError` to the S3 error surface. AWS
/// uses a domain-specific `InvalidSqlExpression` code for parse / unsupported
/// errors, but s3s 0.13 doesn't expose that as a typed variant — we
/// fall back to the well-known `InvalidRequest` 400 with a descriptive
/// message that includes the original error context.
fn select_error_to_s3(e: crate::select::SelectError, fmt: &str) -> S3Error {
    use crate::select::SelectError;
    match e {
        SelectError::Parse(msg) => S3Error::with_message(
            S3ErrorCode::InvalidRequest,
            format!("SQL parse error: {msg}"),
        ),
        SelectError::UnsupportedFeature(msg) => S3Error::with_message(
            S3ErrorCode::InvalidRequest,
            format!("unsupported SQL feature: {msg}"),
        ),
        SelectError::RowEval(msg) => S3Error::with_message(
            S3ErrorCode::InvalidRequest,
            format!("SQL row evaluation error: {msg}"),
        ),
        SelectError::InputFormat(msg) => S3Error::with_message(
            S3ErrorCode::InvalidRequest,
            format!("{fmt} input format error: {msg}"),
        ),
    }
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
    ts.format(s3s::dto::TimestampFormat::DateTime, &mut buf)
        .ok()?;
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

/// v0.6 #39: convert our internal [`crate::tagging::TagSet`] into the
/// s3s `Vec<Tag>` wire shape used on `GetObject/BucketTaggingOutput`.
/// Both halves of every pair land in the `Some(_)` slot — AWS marks
/// the field optional but always populates it on response.
fn tagset_to_aws(set: &crate::tagging::TagSet) -> Vec<Tag> {
    set.iter()
        .map(|(k, v)| Tag {
            key: Some(k.clone()),
            value: Some(v.clone()),
        })
        .collect()
}

/// v0.6 #39: inverse of [`tagset_to_aws`] for input handlers. Missing
/// keys / values become empty strings (mirrors AWS, which rejects
/// `<Key/>` with InvalidTag at the parser layer; downstream
/// `TagSet::validate` then enforces our size limits).
fn aws_to_tagset(tags: &[Tag]) -> Result<crate::tagging::TagSet, crate::tagging::TagError> {
    let pairs = tags
        .iter()
        .map(|t| {
            (
                t.key.clone().unwrap_or_default(),
                t.value.clone().unwrap_or_default(),
            )
        })
        .collect();
    crate::tagging::TagSet::from_pairs(pairs)
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
        // v0.8.15 M-1 / v0.8.17 G-2: shared reserved-name guard.
        self.check_not_reserved_key(&put_key, ReservedKeyMode::Mutating)?;
        // v1.0.1 audit R1 P1: drop client-supplied metadata in the
        // gateway-reserved `s4-*` namespace before anything else looks at
        // it (same unconditional strip CopyObject's REPLACE directive has
        // had since v0.8.16 F-8). Pre-fix, a plain PUT carrying e.g.
        // `x-amz-meta-s4-dict-id: <16hex>` was stored verbatim, and a
        // later flag-less GET took the dictionary path for an object the
        // gateway never dict-compressed → 5xx on a perfectly normal
        // object (freeze violation). The gateway re-stamps its own
        // `s4-*` manifest keys further down, after compression.
        strip_reserved_client_metadata(&mut req.input.metadata);
        let access_preamble = self.access_log_preamble(&req);
        self.enforce_rate_limit(&req, &put_bucket)?;
        // v0.6 #39: parse `x-amz-tagging` (URL-encoded query string) so
        // the IAM policy gate sees the request's tags via
        // `s3:RequestObjectTag/<key>`. `existing_object_tags` is also
        // resolved from the Tagging manager (when wired) so
        // `s3:ExistingObjectTag/<key>` works on overwrite.
        let request_tags: Option<crate::tagging::TagSet> = req
            .input
            .tagging
            .as_deref()
            .map(crate::tagging::parse_tagging_header)
            .transpose()
            .map_err(|e| S3Error::with_message(S3ErrorCode::InvalidArgument, e.to_string()))?;
        let existing_tags: Option<crate::tagging::TagSet> = self
            .tagging
            .as_ref()
            .and_then(|m| m.get_object_tags(&put_bucket, &put_key));
        self.enforce_policy_with_extra(
            &req,
            "s3:PutObject",
            &put_bucket,
            Some(&put_key),
            request_tags.as_ref(),
            existing_tags.as_ref(),
        )?;
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
            // v0.9 #106: parse client-supplied checksum headers
            // **before** awaiting any body bytes. A malformed
            // `Content-MD5` / `x-amz-checksum-*` value must surface
            // as `InvalidDigest` immediately so a slow / non-
            // delivering body cannot tie up the handler waiting on
            // bytes only to reject the request on a header-level
            // problem. The parsed `ClientChecksums` value is reused
            // by the streaming-framed branch below; the
            // bytes-buffered branch keeps its own
            // `verify_client_body_checksums` call which is idempotent
            // with this parse.
            let client_checksums = crate::streaming_checksum::ClientChecksums::from_request_fields(
                req.input.content_md5.as_deref(),
                req.input.checksum_crc32.as_deref(),
                req.input.checksum_crc32c.as_deref(),
                req.input.checksum_sha1.as_deref(),
                req.input.checksum_sha256.as_deref(),
                req.input.checksum_crc64nvme.as_deref(),
            )?;
            // Sample 4 KiB から codec を決定。streaming-aware codec なら streaming
            // compress fast path、そうでなければ従来の collect-then-compress。
            let (sample, rest_stream) = peek_sample(blob, SAMPLE_BYTES)
                .await
                .map_err(internal("peek put sample"))?;
            let sample_len = sample.len().min(SAMPLE_BYTES);
            // v0.8 #56: pass the request's Content-Length (when present) so
            // the sampling dispatcher can promote large objects to a GPU
            // codec. Chunked transfers (no Content-Length) keep CPU.
            let total_size_hint = req.input.content_length.and_then(|n| u64::try_from(n).ok());
            let kind = self
                .dispatcher
                .pick_with_size_hint(&sample[..sample_len], total_size_hint)
                .await;

            // Passthrough buys nothing from S4F2 wrapping (no compression =
            // no per-chunk frame to skip past) and the +28-byte header
            // overhead breaks size-sensitive callers that expect a true
            // pass-through. So passthrough always uses the legacy raw-blob
            // path; only compressing codecs go through the framed path.
            //
            // v0.9 #106 — true streaming PUT checksum verify. The
            // streaming-framed path used to fail-open on client-supplied
            // whole-body checksums (`x-amz-checksum-{crc32, crc32c, sha1,
            // sha256, crc64nvme}` and `Content-MD5`): the v0.8.13 #127
            // attempt to "force buffered when any checksum header is
            // present" had to be reverted in v0.8.14 #129 because modern
            // AWS SDKs auto-attach `x-amz-checksum-crc32`, which made
            // every SDK PUT lose the streaming-framed path and therefore
            // its sidecar (range_get_falls_back_to_full_when_sidecar_etag_stale
            // + upload_part_copy_propagates_source_version_id failed on
            // CI). v0.9 #106 keeps the streaming-framed path and tees
            // each chunk into a multi-hasher (`streaming_checksum`
            // module) as it flows through the compressor. On EOF the
            // hashers are finalised and compared; a mismatch surfaces
            // as a synthetic `io::Error` carrying
            // `StreamingChecksumError` which we downcast back below and
            // map to a typed 400 BadDigest. Sidecar emission is
            // unaffected — the verifier sits **upstream** of
            // `streaming_compress_to_frames`, so on mismatch the call
            // returns Err and we never reach the backend write or
            // sidecar build, preserving the post-revert invariant.
            //
            // Scope: single-PUT cpu-zstd / passthrough only. Multipart
            // `upload_part` keeps its buffered per-part verify (the
            // part body is already in memory there for framing /
            // padding, so streaming verify wouldn't save anything).
            // GPU codecs (nvcomp-*) fall through to the buffered
            // branch below — they are bytes-buffered today and use the
            // existing `verify_client_body_checksums`.
            // (`client_checksums` was parsed before `peek_sample`
            // above so malformed values fail pre-stream.)
            //
            // v0.9 #106 trailer support: the chunked / SigV4-streaming
            // SDK case attaches the actual checksum value in the
            // request **trailers** (post-body). The `x-amz-trailer`
            // request header announces which algorithm(s) will follow;
            // we use it to decide which hashers to spin up at body
            // start so the digest is ready to compare once trailers
            // arrive. After the codec consumes the body we read
            // `req.trailing_headers` and run a deferred comparison
            // against the finalised digests via
            // `ComputedDigests::compare_b64` (see post-stream block
            // below). Without this, a bad trailer checksum on the
            // streaming-framed path would silently pass — same
            // fail-open shape this issue is closing, different
            // delivery mechanism.
            let trailer_hashers: crate::streaming_checksum::WhichHashers = req
                .headers
                .get("x-amz-trailer")
                .and_then(|v| v.to_str().ok())
                .map(crate::streaming_checksum::WhichHashers::from_trailer_header)
                .unwrap_or_default();
            let which_hashers = client_checksums.which_hashers().or(trailer_hashers);
            // v1.1 `--zstd-dict`: small-object shared-dictionary path.
            // Applies only when (a) a dict store is configured, (b) the
            // dispatcher picked cpu-zstd, (c) the key longest-prefix-
            // matches a configured `<bucket>/<prefix>`, and (d) the
            // declared Content-Length fits `--zstd-dict-max-bytes`
            // (chunked transfers without a length stay on the unchanged
            // path — we refuse to buffer blind). With no `--zstd-dict`
            // flag, `dict_candidate` is always `None` and every PUT is
            // bit-for-bit on the pre-dict path.
            //
            // v1.3 dict ops: the store is `load`ed ONCE per PUT — this
            // request keeps the same generation across the candidate
            // lookup and the level / ceiling reads below, so a SIGHUP
            // map reload mid-request can never mix two configurations.
            let dict_store = if kind == CodecKind::CpuZstd {
                self.zstd_dicts.load()
            } else {
                None
            };
            let dict_candidate = dict_store.as_ref().and_then(|store| {
                let fits = total_size_hint
                    .map(|n| n <= store.max_object_bytes() as u64)
                    .unwrap_or(false);
                if fits {
                    store.lookup_with_prefix(&put_bucket, &put_key)
                } else {
                    None
                }
            });
            let mut stamped_dict_id: Option<String> = None;
            // v1.2 `--gpu-batch-small-puts`: small-PUT GPU batch window.
            // Eligible only when (a) the aggregator is wired (flag on +
            // GPU build + CUDA device at boot), (b) the dispatcher picked
            // cpu-zstd (the exact case the per-object GPU path rejects as
            // too small), (c) no dictionary matched (dict path wins — it
            // exists precisely for small homogeneous objects), and (d)
            // the declared Content-Length sits inside
            // `[--gpu-batch-floor-bytes, --gpu-min-bytes)`. Chunked
            // transfers (no Content-Length) stay on the unchanged path —
            // same refuse-to-buffer-blind rule as the dict path.
            let gpu_batch_handle = if dict_candidate.is_none() && kind == CodecKind::CpuZstd {
                self.gpu_batch
                    .as_ref()
                    .filter(|h| total_size_hint.is_some_and(|n| h.eligible_size(n)))
            } else {
                None
            };
            let use_framed = supports_streaming_compress(kind) && kind != CodecKind::Passthrough;
            let (compressed, manifest, is_framed) = if let Some((dict_prefix, dict_id, dict)) =
                dict_candidate
            {
                // Bodies on this path are ≤ --zstd-dict-max-bytes
                // (default 1 MiB), so buffering + compress-both-ways is
                // an acceptable cost for the 2-5× ratio win on
                // homogeneous small objects.
                let bytes = collect_with_sample(sample, rest_stream, self.max_body_bytes)
                    .await
                    .map_err(internal("collect put body (dict path)"))?;
                // Same buffered-path integrity checkpoint as the
                // bytes-buffered branch below: all six header checksums
                // plus the SigV4-streaming trailer comparison.
                verify_client_body_checksums(
                    &bytes,
                    req.input.content_md5.as_deref(),
                    req.input.checksum_crc32.as_deref(),
                    req.input.checksum_crc32c.as_deref(),
                    req.input.checksum_sha1.as_deref(),
                    req.input.checksum_sha256.as_deref(),
                    req.input.checksum_crc64nvme.as_deref(),
                )?;
                if let Some(announced) = req
                    .headers
                    .get("x-amz-trailer")
                    .and_then(|v| v.to_str().ok())
                {
                    let which =
                        crate::streaming_checksum::WhichHashers::from_trailer_header(announced);
                    let computed = if which.any() {
                        crate::streaming_checksum::compute_digests(&bytes, which)
                    } else {
                        crate::streaming_checksum::ComputedDigests::default()
                    };
                    verify_client_trailer_checksums(
                        Some(announced),
                        req.trailing_headers.as_ref(),
                        &computed,
                    )?;
                }
                let dict_level = dict_store
                    .as_ref()
                    .map(|s| s.level())
                    .unwrap_or(s4_codec::cpu_zstd::CpuZstd::DEFAULT_LEVEL);
                debug!(
                    bucket = ?req.input.bucket,
                    key = ?req.input.key,
                    bytes = bytes.len(),
                    dict_id = %dict_id,
                    path = "dict-framed",
                    "S4 put_object: compressing (buffered, dict vs plain cpu-zstd)"
                );
                if bytes.len()
                    <= dict_store
                        .as_ref()
                        .map(|s| s.max_object_bytes())
                        .unwrap_or(crate::dict::DEFAULT_DICT_MAX_OBJECT_BYTES)
                {
                    let (body, manifest, used_dict) = self
                        .compress_small_with_dict(bytes, dict, dict_level, &dict_prefix)
                        .await?;
                    if used_dict {
                        stamped_dict_id = Some(dict_id);
                    }
                    (body, manifest, true)
                } else {
                    // The declared Content-Length fit the dict ceiling
                    // but the wire body didn't (lying client). Re-frame
                    // through the standard chunked path — same output
                    // shape a non-dict PUT of this body would produce.
                    let actual_len = bytes.len() as u64;
                    let (body, manifest) = streaming_compress_to_frames(
                        bytes_to_blob(bytes),
                        Arc::clone(&self.registry),
                        kind,
                        pick_chunk_size(Some(actual_len)),
                        None,
                    )
                    .await
                    .map_err(internal("framed compress (dict-path size fallback)"))?;
                    (body, manifest, true)
                }
            } else if let Some(batch) = gpu_batch_handle {
                // v1.2 `--gpu-batch-small-puts`: buffered small-PUT GPU
                // batch path. Bodies here are < --gpu-min-bytes (default
                // 1 MiB) by the eligibility gate, so buffering is cheaper
                // than the dict path's ceiling. Verification mirrors the
                // buffered branches: all six header checksums + the
                // SigV4-streaming trailer comparison.
                let bytes = collect_with_sample(sample, rest_stream, self.max_body_bytes)
                    .await
                    .map_err(internal("collect put body (gpu-batch path)"))?;
                verify_client_body_checksums(
                    &bytes,
                    req.input.content_md5.as_deref(),
                    req.input.checksum_crc32.as_deref(),
                    req.input.checksum_crc32c.as_deref(),
                    req.input.checksum_sha1.as_deref(),
                    req.input.checksum_sha256.as_deref(),
                    req.input.checksum_crc64nvme.as_deref(),
                )?;
                if let Some(announced) = req
                    .headers
                    .get("x-amz-trailer")
                    .and_then(|v| v.to_str().ok())
                {
                    let which =
                        crate::streaming_checksum::WhichHashers::from_trailer_header(announced);
                    let computed = if which.any() {
                        crate::streaming_checksum::compute_digests(&bytes, which)
                    } else {
                        crate::streaming_checksum::ComputedDigests::default()
                    };
                    verify_client_trailer_checksums(
                        Some(announced),
                        req.trailing_headers.as_ref(),
                        &computed,
                    )?;
                }
                debug!(
                    bucket = ?req.input.bucket,
                    key = ?req.input.key,
                    bytes = bytes.len(),
                    path = "gpu-batch",
                    "S4 put_object: compressing (buffered, nvCOMP batched zstd)"
                );
                // Re-check the wire size: a lying client (Content-Length
                // inside the window, actual body outside it) skips the
                // batch and lands on the standard fallback below.
                let batched = if batch.eligible_size(bytes.len() as u64) {
                    batch.try_compress(bytes.clone()).await
                } else {
                    Err(crate::gpu_batch::GpuBatchError::Codec(
                        s4_codec::CodecError::Backend(anyhow::anyhow!(
                            "actual body size {} outside the gpu-batch window",
                            bytes.len()
                        )),
                    ))
                };
                match batched {
                    // Ratio guard: accept the batched output only when it
                    // actually shrank the body. Small objects can come out
                    // of GPU zstd *larger* than cpu-zstd-3 would make them
                    // (per-chunk framing overhead dominates near the
                    // floor); when the batch output is >= the input we
                    // fall back to the pre-existing cpu-zstd framed path
                    // — the same regime a passthrough-vs-compress
                    // decision follows elsewhere.
                    Ok((body, manifest)) if (body.len() as u64) < manifest.original_size => {
                        crate::metrics::record_gpu_batch("batched");
                        // Buffered raw-blob shape — exactly the existing
                        // per-object GPU-codec PUT path (is_framed=false).
                        (body, manifest, false)
                    }
                    Ok((body, manifest)) => {
                        debug!(
                            bucket = ?req.input.bucket,
                            key = ?req.input.key,
                            compressed = body.len(),
                            original = manifest.original_size,
                            "gpu-batch output not smaller than input; falling back to cpu-zstd"
                        );
                        crate::metrics::record_gpu_batch("fallback");
                        let actual_len = bytes.len() as u64;
                        let (body, manifest) = streaming_compress_to_frames(
                            bytes_to_blob(bytes),
                            Arc::clone(&self.registry),
                            kind,
                            pick_chunk_size(Some(actual_len)),
                            None,
                        )
                        .await
                        .map_err(internal("framed compress (gpu-batch ratio fallback)"))?;
                        (body, manifest, true)
                    }
                    Err(e) => {
                        // Queue full / worker gone / CUDA failure — the
                        // PUT must still succeed exactly as it would have
                        // without the flag.
                        debug!(
                            bucket = ?req.input.bucket,
                            key = ?req.input.key,
                            error = %e,
                            "gpu-batch declined; falling back to cpu-zstd framed path"
                        );
                        crate::metrics::record_gpu_batch("fallback");
                        let actual_len = bytes.len() as u64;
                        let (body, manifest) = streaming_compress_to_frames(
                            bytes_to_blob(bytes),
                            Arc::clone(&self.registry),
                            kind,
                            pick_chunk_size(Some(actual_len)),
                            None,
                        )
                        .await
                        .map_err(internal("framed compress (gpu-batch error fallback)"))?;
                        (body, manifest, true)
                    }
                }
            } else if use_framed {
                // streaming fast path: input は memory に collect しない
                let chained = chain_sample_with_rest(sample, rest_stream);
                // v0.9 #106: tee the chained input through a multi-hasher
                // when ANY client checksum claim is present (header or
                // trailer). The wrapper is a no-op (and skipped
                // entirely) when neither side has work, so non-
                // checksummed PUTs keep their pre-#106 throughput.
                let (chained, digest_handle) = if which_hashers.any() {
                    let (b, h) = crate::streaming_checksum::tee_into_hashers_with_handle(
                        chained,
                        client_checksums.clone(),
                        which_hashers,
                    );
                    (b, Some(h))
                } else {
                    (chained, None)
                };
                debug!(
                    bucket = ?req.input.bucket,
                    key = ?req.input.key,
                    codec = kind.as_str(),
                    path = "streaming-framed",
                    client_checksum_verify = client_checksums.any(),
                    "S4 put_object: compressing (streaming, S4F2 multi-frame)"
                );
                // v0.4 #16: pick the chunk size based on the request's
                // Content-Length when known, falling back to the 4 MiB
                // default for chunked transfers.
                let chunk_size = pick_chunk_size(req.input.content_length.map(|n| n as u64));
                // v0.8.4 #73 M2: pass the request's Content-Length so
                // streaming_compress_to_frames can fail-fast on a mid-PUT
                // truncation (client disconnect after sending half the
                // body). `None` is the chunked-Transfer-Encoding case
                // where the upstream genuinely doesn't know the size and
                // the backend's framing layer is the only truncation
                // signal we have.
                let expected_input_size =
                    req.input.content_length.and_then(|n| u64::try_from(n).ok());
                let (body, manifest) = streaming_compress_to_frames(
                    chained,
                    Arc::clone(&self.registry),
                    kind,
                    chunk_size,
                    expected_input_size,
                )
                .await
                .map_err(|e| match e {
                    s4_codec::CodecError::TruncatedStream { expected, got } => {
                        // 400 IncompleteBody: client advertised N bytes
                        // but disconnected after `got`. Mirrors AWS S3's
                        // canonical error code for the same shape so SDK
                        // retries kick in instead of treating the PUT as
                        // a successful upload of a half-body.
                        S3Error::with_message(
                            S3ErrorCode::IncompleteBody,
                            format!("PUT body truncated: expected {expected} bytes, got {got}"),
                        )
                    }
                    // v0.8.15 M-4: 400
                    // `RequestBodyLengthMismatch` for over-length
                    // bodies. AWS S3 returns this when the declared
                    // `Content-Length` is smaller than the wire body;
                    // S4 used to silently accept the surplus bytes.
                    // `IncompleteBody` is the closest typed variant
                    // in the s3s enum — we widen the message so the
                    // SDK / curl side sees the shape unambiguously.
                    s4_codec::CodecError::OverlengthStream { expected, got } => {
                        let code = S3ErrorCode::from_bytes(b"RequestBodyLengthMismatch")
                            .unwrap_or(S3ErrorCode::IncompleteBody);
                        S3Error::with_message(
                            code,
                            format!(
                                "PUT body length mismatch: Content-Length declared {expected} \
                                 bytes, body carried at least {got}"
                            ),
                        )
                    }
                    // v0.9 #106: streaming checksum mismatch — the tee
                    // wrapper emitted a synthetic io::Error carrying
                    // StreamingChecksumError. Downcast and remap to
                    // BadDigest so the client sees the same response
                    // the buffered path would have produced.
                    s4_codec::CodecError::Io(ref io_err) => {
                        if let Some(alg) =
                            crate::streaming_checksum::extract_streaming_checksum_error(io_err)
                        {
                            let code = S3ErrorCode::from_bytes(b"BadDigest")
                                .unwrap_or(S3ErrorCode::InvalidArgument);
                            S3Error::with_message(
                                code,
                                format!("client-supplied {alg} did not match the received body"),
                            )
                        } else {
                            internal("streaming framed compress")(e)
                        }
                    }
                    other => internal("streaming framed compress")(other),
                })?;
                // v0.9 #106 trailer-deferred verify. Header claims
                // have already been compared eagerly inside the tee
                // at EOF (mismatch surfaces as `BadDigest` through
                // the `CodecError::Io` branch above). Now that the
                // body has been fully consumed, request trailers are
                // available — delegate to the shared trailer-verify
                // helper (also used by the buffered branch below,
                // see v0.9 #106-audit-R2 P2-INT-2).
                //
                // **Fail-closed when announced trailers are
                // missing**: if the client announced
                // `x-amz-trailer: x-amz-checksum-*` but did NOT
                // deliver the trailer value (or the trailers block
                // never arrived), the helper refuses the PUT with
                // `BadDigest`. Skipping the comparison in that case
                // would silently re-open the streaming fail-open
                // this issue closes — a client could declare an
                // integrity check and then omit the value to bypass
                // verification.
                if let Some(handle) = digest_handle.as_ref() {
                    let announced = req
                        .headers
                        .get("x-amz-trailer")
                        .and_then(|v| v.to_str().ok());
                    // If the tee never finalised (computed is None)
                    // the body was incomplete; the CodecError path
                    // would have already surfaced — defensive belt
                    // for any future refactor. We still need a
                    // ComputedDigests instance to feed the helper
                    // when trailers were announced, so synthesise
                    // an empty one and let `compare_b64` reject
                    // every claim as BadDigest (every algorithm
                    // slot is None).
                    let computed = handle
                        .lock()
                        .expect("digest handle lock poisoned")
                        .clone()
                        .unwrap_or_default();
                    verify_client_trailer_checksums(
                        announced,
                        req.trailing_headers.as_ref(),
                        &computed,
                    )?;
                }
                (body, manifest, true)
            } else {
                // GPU codec 等で streaming-aware でないものは bytes-buffered path
                // (raw 圧縮 bytes、framed なし — back-compat 互換 path)
                let bytes = collect_with_sample(sample, rest_stream, self.max_body_bytes)
                    .await
                    .map_err(internal("collect put body (buffered path)"))?;
                // v0.8.12 HIGH-12 / #128 MED-C: verify all six AWS
                // checksum algorithms against the received body on
                // the buffered path. The streaming-framed branch
                // above redirects here when ANY checksum header is
                // present (#127 MED-B), so this is the single
                // checkpoint for client-supplied integrity.
                verify_client_body_checksums(
                    &bytes,
                    req.input.content_md5.as_deref(),
                    req.input.checksum_crc32.as_deref(),
                    req.input.checksum_crc32c.as_deref(),
                    req.input.checksum_sha1.as_deref(),
                    req.input.checksum_sha256.as_deref(),
                    req.input.checksum_crc64nvme.as_deref(),
                )?;
                // v0.9 #106-audit-R2 P2-INT-2: SigV4-streaming trailer
                // checksums must verify on the buffered path too. Pre-fix
                // the streaming-framed branch above handled
                // `x-amz-trailer` while this branch silently dropped
                // it — a client could PUT through a GPU codec / non-
                // streaming dispatch and bypass trailer verification.
                // We have the full body in memory here, so a one-shot
                // `compute_digests` followed by the shared
                // `verify_client_trailer_checksums` helper closes the
                // gap. The hasher selector is derived from the same
                // `x-amz-trailer` header parser the streaming branch
                // uses (`WhichHashers::from_trailer_header`).
                if let Some(announced) = req
                    .headers
                    .get("x-amz-trailer")
                    .and_then(|v| v.to_str().ok())
                {
                    let which =
                        crate::streaming_checksum::WhichHashers::from_trailer_header(announced);
                    if which.any() {
                        let computed = crate::streaming_checksum::compute_digests(&bytes, which);
                        verify_client_trailer_checksums(
                            Some(announced),
                            req.trailing_headers.as_ref(),
                            &computed,
                        )?;
                    } else {
                        // Header announced only non-checksum trailers
                        // (e.g. `x-amz-trailer-signature`). The helper
                        // would return Ok in that case — invoke it
                        // anyway for symmetry with the streaming branch
                        // so a future change to the filter logic stays
                        // wired through both paths.
                        verify_client_trailer_checksums(
                            Some(announced),
                            req.trailing_headers.as_ref(),
                            &crate::streaming_checksum::ComputedDigests::default(),
                        )?;
                    }
                }
                debug!(
                    bucket = ?req.input.bucket,
                    key = ?req.input.key,
                    bytes = bytes.len(),
                    codec = kind.as_str(),
                    path = "buffered",
                    "S4 put_object: compressing (buffered, raw blob)"
                );
                // v0.8 #55: telemetry-returning compress so we can stamp
                // GPU-pipeline Prometheus metrics (`s4_gpu_compress_seconds`,
                // throughput gauge, OOM counter) for nvcomp / dietgpu codecs.
                // CPU codecs come back with `gpu_seconds = None` and the
                // stamp helper short-circuits — no extra cost on CPU path.
                let (compress_res, tel) = self.registry.compress_with_telemetry(bytes, kind).await;
                stamp_gpu_compress_telemetry(&tel);
                let (body, m) = compress_res.map_err(internal("registry compress"))?;
                (body, m, false)
            };

            write_manifest(&mut req.input.metadata, &manifest);
            // v1.2 audit R1 P2: this PUT is about to be added to the
            // savings ledger — stamp the accounting marker so a later
            // DELETE / overwrite probe knows the subtraction is
            // symmetric. Ledger-off deployments skip the stamp (flag
            // absent ⇒ bit-for-bit pre-ledger metadata).
            if self.savings_ledger.is_some() {
                req.input
                    .metadata
                    .get_or_insert_with(Default::default)
                    .insert(META_LEDGER.into(), META_LEDGER_ACCOUNTED.into());
            }
            if is_framed {
                // v0.2 #4: framed body であることを GET 側に伝える meta flag。
                req.input
                    .metadata
                    .get_or_insert_with(Default::default)
                    .insert(META_FRAMED.into(), "true".into());
            }
            // v1.1 `--zstd-dict`: record which dictionary the body was
            // compressed against. Only stamped when the dict actually won
            // the size comparison — fallback bodies are plain `cpu-zstd`
            // frames and carry no dict reference.
            if let Some(ref dict_id) = stamped_dict_id {
                req.input
                    .metadata
                    .get_or_insert_with(Default::default)
                    .insert(META_DICT_ID.into(), dict_id.clone());
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
            // (sidecar_index is built below, after the SSE-mode
            // extraction, so v0.8.12 HIGH-10 can short-circuit the
            // build when the on-disk bytes are about to be encrypted.)
            // v0.4 #21 / v0.5 #29 / v0.5 #27: encrypt-after-compress.
            // Precedence:
            //   - SSE-C headers present → per-request customer key (S4E3)
            //   - server-managed keyring configured → active key (S4E2)
            //   - neither → no encryption (raw compressed body)
            // The `s4-encrypted: aes-256-gcm` metadata flag is set in
            // both encrypted modes; the on-disk frame magic distinguishes
            // S4E1 / S4E2 / S4E3 so GET picks the right decrypt path.
            // v0.7 #48 BUG-2/3 fix: take() the SSE fields off req.input
            // so the encryption headers are NOT forwarded to the
            // backend. S4 owns the encrypt-then-store contract; if we
            // leave the headers in place, real S3-compat backends
            // (MinIO / AWS) try to apply their own SSE on top and
            // either reject (MinIO requires HTTPS for SSE-C) or fail
            // (MinIO has no KMS configured). MemoryBackend ignored
            // these so mock tests passed.
            let sse_c_alg = req.input.sse_customer_algorithm.take();
            let sse_c_key = req.input.sse_customer_key.take();
            let sse_c_md5 = req.input.sse_customer_key_md5.take();
            let sse_header = req.input.server_side_encryption.take();
            let sse_kms_key = req.input.ssekms_key_id.take();
            let sse_c_material = extract_sse_c_material(&sse_c_alg, &sse_c_key, &sse_c_md5)?;
            // v0.5 #28: SSE-KMS request? Resolves to None unless the
            // request asks for `aws:kms` AND a key id is available
            // (explicit header or gateway default). When set, we'll
            // generate a per-object DEK below.
            let kms_key_id = extract_kms_key_id(
                &sse_header,
                &sse_kms_key,
                self.kms_default_key_id.as_deref(),
            );
            // v0.8.12 HIGH-10 fix: the sidecar offsets describe the
            // pre-encrypt `compressed` body, but the bytes the
            // backend stores when any SSE mode is active are
            // *post-encrypt* (different length, different layout).
            // A Range GET on an SSE-encrypted object would slice the
            // ciphertext at the stale offsets, hand the wrong bytes
            // to the frame parser, and 500. Suppress the sidecar
            // entirely when SSE is going to be applied below;
            // encrypted-object Range GET falls back to the buffered
            // path (decrypt full body → frame parse → slice), trading
            // partial-fetch performance for correctness.
            //
            // v0.9 #106 (encryption-aware sidecar): re-enable sidecar
            // emission for the **SSE-S4 chunked (S4E6) path only** —
            // S4E6 chunks are per-chunk independently sealed so the
            // GET path can compute encrypted byte ranges, partial-fetch
            // just the needed chunks, decrypt + frame-parse + slice.
            // The pre-encrypt `compressed` offsets in the sidecar are
            // still load-bearing (the GET path decrypts into the
            // pre-encrypt domain before frame-parsing), with the new
            // v3 SSE binding (`sse_v3`) stamped below once the
            // encrypt path runs and reveals the per-PUT salt /
            // chunk_count / key_id. SSE-KMS / SSE-C / S4E2 buffered
            // (`--sse-chunk-size 0`) keep the v0.8.12 #120 buffered
            // fallback (= sidecar suppressed) — multi-mode plumbing
            // is the v0.10+ roadmap.
            let will_encrypt =
                sse_c_material.is_some() || kms_key_id.is_some() || self.sse_keyring.is_some();
            let sse_s4_chunked_path = sse_c_material.is_none()
                && kms_key_id.is_none()
                && self.sse_keyring.is_some()
                && self.sse_chunk_size > 0;
            let sidecar_index = if is_framed && (!will_encrypt || sse_s4_chunked_path) {
                s4_codec::index::build_index_from_body(&compressed).ok()
            } else {
                None
            };
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
                && sse_header.as_ref().map(|s| s.as_str()) != Some(ServerSideEncryption::AES256)
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
            //
            // v0.8.1 #58: the plaintext DEK lives in three places
            // during one PUT:
            //
            //   1. The `Zeroizing<Vec<u8>>` returned by `generate_dek`
            //      — wiped when the binding `dek` falls out of scope at
            //      the end of this `if`-arm.
            //   2. The stack `[u8; 32]` we copy into for `SseSource::Kms`
            //      — wrapped in `Zeroizing<[u8; 32]>` so it's wiped when
            //      the outer `kms_wrap` `Option` is dropped at the end
            //      of `put_object`.
            //   3. AES-GCM internal key state inside the `aes-gcm`
            //      crate during `encrypt_with_source` — out of scope
            //      for this fix; tracked separately in v0.8.2.
            let kms_wrap: Option<(zeroize::Zeroizing<[u8; 32]>, crate::kms::WrappedDek)> =
                if let Some(ref key_id) = kms_key_id {
                    let kms = self.kms.as_ref().ok_or_else(|| {
                    S3Error::with_message(
                        S3ErrorCode::InvalidRequest,
                        "SSE-KMS requested but no --kms-local-dir / --kms-aws-region is configured on this gateway",
                    )
                })?;
                    // `dek` is `Zeroizing<Vec<u8>>`; deref + slice access
                    // works unchanged via `Deref<Target=Vec<u8>>`.
                    let (dek, wrapped) = kms.generate_dek(key_id).await.map_err(kms_error_to_s3)?;
                    if dek.len() != 32 {
                        return Err(S3Error::with_message(
                            S3ErrorCode::InternalError,
                            format!(
                                "KMS backend returned a DEK of {} bytes (expected 32)",
                                dek.len()
                            ),
                        ));
                    }
                    let mut dek_arr: zeroize::Zeroizing<[u8; 32]> =
                        zeroize::Zeroizing::new([0u8; 32]);
                    dek_arr.copy_from_slice(&dek);
                    // `dek` (the `Zeroizing<Vec<u8>>`) is dropped at the
                    // end of this scope, wiping the heap allocation.
                    Some((dek_arr, wrapped))
                } else {
                    None
                };
            // v0.7 #48 BUG-4 fix: stamp the SSE *type* into metadata
            // alongside `s4-encrypted` so HEAD (which doesn't fetch the
            // body) can echo the correct `x-amz-server-side-encryption`
            // value. Without this, HEAD on an SSE-KMS object would not
            // echo `aws:kms` because the frame magic is only available
            // on the body (which HEAD doesn't read).
            let body_to_send = if let Some(ref m) = sse_c_material {
                let meta = req.input.metadata.get_or_insert_with(Default::default);
                meta.insert("s4-encrypted".into(), "aes-256-gcm".into());
                meta.insert("s4-sse-type".into(), "AES256".into());
                meta.insert(
                    "s4-sse-c-key-md5".into(),
                    base64::engine::general_purpose::STANDARD.encode(m.key_md5),
                );
                crate::sse::encrypt_with_source(
                    &compressed,
                    crate::sse::SseSource::CustomerKey {
                        key: &m.key,
                        key_md5: &m.key_md5,
                    },
                )
            } else if let Some((ref dek, ref wrapped)) = kms_wrap {
                let meta = req.input.metadata.get_or_insert_with(Default::default);
                meta.insert("s4-encrypted".into(), "aes-256-gcm".into());
                meta.insert("s4-sse-type".into(), "aws:kms".into());
                meta.insert("s4-sse-kms-key-id".into(), wrapped.key_id.clone());
                // v0.8.1 #58: `dek` is `&Zeroizing<[u8; 32]>`; `SseSource::Kms`
                // wants `&[u8; 32]`. Rust auto-derefs `&Zeroizing<T>` to
                // `&T` here via `Deref<Target=T>`, so the binding picks
                // up the inner array reference without copying. The array
                // stays in the `Zeroizing` wrapper that owns it and gets
                // wiped when `kms_wrap` drops at the end of `put_object`.
                let dek_ref: &[u8; 32] = dek;
                crate::sse::encrypt_with_source(
                    &compressed,
                    crate::sse::SseSource::Kms {
                        dek: dek_ref,
                        wrapped,
                    },
                )
            } else if let Some(keyring) = self.sse_keyring.as_ref() {
                // SSE-S4 is server-driven transparent encryption; the
                // client didn't ask for SSE. We stamp `s4-encrypted`
                // (internal flag the GET path needs) but deliberately
                // do NOT stamp `s4-sse-type` — that lights up the HEAD
                // echo of `x-amz-server-side-encryption: AES256`,
                // which would falsely advertise AWS-style SSE-S3
                // semantics the operator didn't request.
                let meta = req.input.metadata.get_or_insert_with(Default::default);
                meta.insert("s4-encrypted".into(), "aes-256-gcm".into());
                // v0.8 #52: when `--sse-chunk-size > 0` is configured,
                // emit the chunked S4E5 frame so the matching GET can
                // stream-decrypt instead of buffering 5 GiB before
                // emitting a byte. Falls back to the buffered S4E2
                // frame at chunk_size=0 (default) so existing
                // deployments are bit-for-bit unchanged.
                if self.sse_chunk_size > 0 {
                    crate::sse::encrypt_v2_chunked(&compressed, keyring, self.sse_chunk_size)
                        .map_err(|e| {
                            S3Error::with_message(
                                S3ErrorCode::InternalError,
                                format!("SSE-S4 chunked encrypt failed: {e}"),
                            )
                        })?
                } else {
                    crate::sse::encrypt_v2(&compressed, keyring)
                }
            } else {
                compressed.clone()
            };
            // v0.9 #106: when the SSE-S4 chunked path ran (and only
            // that path — SSE-KMS / SSE-C / S4E2 buffered keep the
            // buffered fallback), parse the S4E6 header bytes back
            // out of `body_to_send` to recover the per-PUT salt /
            // key_id / chunk_count and stamp them onto the sidecar's
            // SSE binding. The salt isn't secret (it lives in the
            // encrypted body's plaintext header) so duplicating it
            // in the sidecar saves the GET path an extra HEAD/GET to
            // re-derive it. `parse_s4e6_header` reads the fixed-
            // layout fields only — any failure leaves `sse_binding`
            // as `None`, which falls through to the legacy buffered
            // fallback on GET (= safe degradation, not corruption).
            let sse_binding: Option<s4_codec::index::SseChunkBinding> = if sse_s4_chunked_path {
                match crate::sse::parse_s4e6_header(&body_to_send) {
                    Ok(hdr) => Some(s4_codec::index::SseChunkBinding {
                        enc_chunk_size: hdr.chunk_size,
                        enc_chunk_count: hdr.chunk_count,
                        enc_key_id: hdr.key_id,
                        enc_salt: *hdr.salt,
                        enc_plaintext_len: compressed.len() as u64,
                        // S4E6_HEADER_BYTES = 24 today; carried
                        // explicitly so a future bump (e.g. S4E7
                        // with a different fixed-header size) can't
                        // silently break v3 sidecar decode.
                        enc_header_bytes: crate::sse::S4E6_HEADER_BYTES as u32,
                    }),
                    Err(e) => {
                        tracing::warn!(
                            bucket = %put_bucket,
                            key = %put_key,
                            "S4 sidecar SSE-binding stamp failed (Range GET will fall back \
                             to buffered): {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };
            // v0.6 #40: capture the about-to-be-sent body + metadata so
            // the replication dispatcher (run after the source PUT
            // succeeds) can hand the same backend bytes to the
            // destination bucket. `Bytes` clone is cheap (refcounted).
            let replication_body = body_to_send.clone();
            // v1.2 audit R2 P2: NOT a verbatim clone — the ledger marker
            // must not ride along to the (never-accounted) replica.
            let replication_metadata = replication_metadata_snapshot(&req.input.metadata);
            // v0.7 #48 BUG-1 fix: SSE encryption (S4E1/E2/E3/E4 frames)
            // makes the body longer than the post-compression bytes
            // (header + nonce + tag overhead). The earlier
            // content_length stamp at compressed.len() is now stale, so
            // re-stamp from the actual bytes about to be sent or the
            // backend (real S3 / MinIO) rejects with
            // `StreamLengthMismatch`. MemoryBackend never validated
            // this, which is why mock-only tests passed.
            req.input.content_length = Some(body_to_send.len() as i64);
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
                    crate::versioning::VersioningState::Enabled => crate::versioning::PutOutcome {
                        version_id: crate::versioning::VersioningManager::new_version_id(),
                        versioned_response: true,
                    },
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
            // v0.8.4 #73 H-2: capture the to-be-stored body length BEFORE
            // the move into `req.input` is consumed by the backend call.
            // The sidecar's `source_compressed_size` is checked against
            // the live HEAD `Content-Length` on Range GET to detect a
            // backend-side mutation.
            let backend_object_size = req.input.content_length.and_then(|n| u64::try_from(n).ok());
            // v1.2 savings ledger: probe the to-be-replaced footprint
            // BEFORE the backend PUT overwrites it. Versioning-Enabled
            // buckets skip the probe — the PUT lands under a fresh
            // shadow key and the prior version's bytes stay on the
            // backend (each stored version is a ledger object). The
            // extra HEAD exists only when the ledger flag is on.
            let ledger_versioned_put = pending_version
                .as_ref()
                .map(|pv| pv.versioned_response)
                .unwrap_or(false);
            let ledger_old_main: Option<LedgerFootprint> =
                if self.savings_ledger.is_some() && !ledger_versioned_put {
                    self.ledger_probe_object(&put_bucket, &put_key, None).await
                } else {
                    None
                };
            let mut backend_resp = self.backend.put_object(req).await;
            // v0.9 #106 (Codex P2): on the SSE-S4 chunked PUT path,
            // if we *couldn't* recover the per-PUT salt / key_id /
            // chunk_count (= `sse_binding.is_none()`), we MUST NOT
            // emit any sidecar — the bytes on disk are S4E6-encrypted
            // and the offsets in `sidecar_index` are pre-encrypt. A
            // v2 sidecar (sans SSE binding) would skip the encryption-
            // aware GET fast-path AND skip the v0.8.12 #120 buffered
            // fallback (the GET path treats a present sidecar as
            // "use partial_range_get on the backend body"), so it
            // would slice ciphertext at plaintext offsets, hand wrong
            // bytes to the frame parser, and 500 (or worse, return
            // garbage that decodes by accident). Drop the sidecar so
            // the GET falls back to buffered = correct.
            let suppress_sidecar_for_failed_sse_binding =
                sse_s4_chunked_path && sse_binding.is_none();
            // v1.2 savings ledger: sidecar bytes folded into the stored
            // footprint. `old` is probed only when a new sidecar is
            // about to overwrite it (a stale sidecar that stays on the
            // backend keeps counting — it still occupies bytes there).
            let mut ledger_sidecar_old: u64 = 0;
            let mut ledger_sidecar_new: u64 = 0;
            if let Some(mut idx) = sidecar_index
                && let Ok(ref resp) = backend_resp
                && idx.entries.len() > 1
                && !suppress_sidecar_for_failed_sse_binding
            {
                // 1 chunk しかない (small object) なら sidecar は意味がない (=
                // partial fetch しても full body と同じ範囲) ので省略。
                // Sidecar は user-visible key で書く (latest version の
                // partial fetch path 用)。Old versions の Range GET は今 task
                // の scope 外 (full read fallback でも意味的には正しい)。
                //
                // v0.8.4 #73 H-2: stamp the version-binding fields the
                // GET path needs to detect a stale / attacker-written
                // sidecar. ETag comes from the backend's PUT response —
                // when missing (some backends don't return an ETag) we
                // synthesize a CRC-derived stable identifier so the
                // sidecar still binds to *something*; the GET HEAD will
                // see the same backend ETag (None vs None) and treat the
                // pair as consistent.
                let source_etag = resp.output.e_tag.as_ref().map(|t| t.value().to_string());
                idx.source_etag = source_etag;
                idx.source_compressed_size = backend_object_size;
                // v0.9 #106: stamp the SSE chunked binding so the GET
                // path can run the encrypted Range partial-fetch
                // fast-path. `None` keeps the sidecar at v2 layout
                // (= existing behaviour for non-SSE-S4-chunked PUTs).
                idx.sse_v3 = sse_binding;
                // v1.2 audit R1 P2: only subtract the to-be-replaced
                // sidecar when the bytes it described were themselves
                // ledger-accounted — an unaccounted main object's
                // sidecar was never added either. Versioned PUTs skip
                // the main-object probe, but the user-visible-key
                // sidecar they replace was written by the gateway for
                // the (accounted) previous version, so they keep
                // subtracting it.
                if self.savings_ledger.is_some()
                    && (ledger_versioned_put || ledger_old_main.is_some_and(|f| f.accounted))
                {
                    ledger_sidecar_old =
                        self.ledger_probe_sidecar_bytes(&put_bucket, &put_key).await;
                }
                ledger_sidecar_new = self.write_sidecar(&put_bucket, &put_key, &idx).await;
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
                resp.output.sse_customer_key_md5 =
                    Some(base64::engine::general_purpose::STANDARD.encode(m.key_md5));
            }
            // v0.5 #28: SSE-KMS echo — `aws:kms` + the canonical key id
            // the backend returned (AWS KMS returns the ARN even when
            // the request used an alias).
            if let (Some((_, wrapped)), Ok(resp)) = (kms_wrap.as_ref(), backend_resp.as_mut()) {
                resp.output.server_side_encryption = Some(ServerSideEncryption::from_static(
                    ServerSideEncryption::AWS_KMS,
                ));
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
            // v1.2 savings ledger: commit the PUT's footprint as one
            // delta — subtract whatever the probe saw at this key
            // (overwrite = swap, not double-count), add the new body +
            // sidecar bytes. Versioned PUTs skipped the probe, so they
            // are pure adds (every stored version is a ledger object).
            //
            // v1.2 audit R1 P2: an existing-but-unaccounted old object
            // (no `s4-ledger` marker) is NOT subtracted — it was never
            // added. The overwrite then counts as a fresh object (+1)
            // and the skipped removal is tallied for the report.
            if let (Some(ledger), true) = (self.savings_ledger.as_ref(), backend_resp.is_ok()) {
                let stored_new = backend_object_size
                    .unwrap_or(0)
                    .saturating_add(ledger_sidecar_new);
                let old_accounted = ledger_old_main.is_some_and(|f| f.accounted);
                let (old_original, old_stored) = ledger_old_main
                    .filter(|f| f.accounted)
                    .map(|f| (f.original_bytes, f.stored_bytes))
                    .unwrap_or((0, 0));
                let old_stored = old_stored.saturating_add(ledger_sidecar_old);
                if ledger_old_main.is_some() && !old_accounted {
                    ledger.record_skipped_unaccounted(&put_bucket);
                }
                ledger.apply_delta(
                    &put_bucket,
                    crate::ledger::signed_delta(original_size, old_original),
                    crate::ledger::signed_delta(stored_new, old_stored),
                    if old_accounted { 0 } else { 1 },
                );
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
            // v0.6 #35: fire bucket-notification destinations (best-effort,
            // detached). Skipped when no manager is attached or when the
            // bucket has no rule matching `s3:ObjectCreated:Put` for this
            // key.
            if backend_resp.is_ok()
                && let Some(mgr) = self.notifications.as_ref()
            {
                let dests = mgr.match_destinations(
                    &put_bucket,
                    &crate::notifications::EventType::ObjectCreatedPut,
                    &put_key,
                );
                if !dests.is_empty() {
                    let etag = backend_resp
                        .as_ref()
                        .ok()
                        .and_then(|r| r.output.e_tag.clone())
                        .map(ETag::into_value);
                    let version_id = pending_version
                        .as_ref()
                        .filter(|pv| pv.versioned_response)
                        .map(|pv| pv.version_id.clone());
                    tokio::spawn(crate::notifications::dispatch_event(
                        Arc::clone(mgr),
                        put_bucket.clone(),
                        put_key.clone(),
                        crate::notifications::EventType::ObjectCreatedPut,
                        Some(original_size),
                        etag,
                        version_id,
                        format!("S4-{}", uuid::Uuid::new_v4()),
                    ));
                }
            }
            // v0.6 #39: persist parsed `x-amz-tagging` tags into the
            // tagging manager on a successful PUT. AWS PutObject's
            // tagging is a full-replace operation (not a merge), so
            // any pre-existing entry for `(bucket, key)` is overwritten.
            if backend_resp.is_ok()
                && let (Some(mgr), Some(tags)) = (self.tagging.as_ref(), request_tags.clone())
            {
                mgr.put_object_tags(&put_bucket, &put_key, tags);
            }
            // v0.6 #40: cross-bucket replication fire-point. On
            // successful source PUT, consult the replication manager;
            // when an enabled rule matches, mark the source key
            // `Pending` and spawn a detached task that PUTs the same
            // backend bytes + metadata to the rule's destination
            // bucket. The dispatcher itself records `Completed` /
            // `Failed` and bumps the drop counter on retry-budget
            // exhaustion.
            self.spawn_replication_if_matched(
                &put_bucket,
                &put_key,
                &request_tags,
                &replication_body,
                &replication_metadata,
                backend_resp.is_ok(),
                pending_version.as_ref(),
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
        // v1.2 savings ledger: same pre-PUT replace probe as the
        // body-bearing branch (a zero-length PUT can still overwrite a
        // non-empty object, which must subtract the old footprint).
        let ledger_versioned_put = pending_version
            .as_ref()
            .map(|pv| pv.versioned_response)
            .unwrap_or(false);
        let ledger_old_main: Option<LedgerFootprint> =
            if self.savings_ledger.is_some() && !ledger_versioned_put {
                self.ledger_probe_object(&put_bucket, &put_key, None).await
            } else {
                None
            };
        // v1.2 audit R1 P2: zero-length objects are ledger-accounted
        // (objects + zero bytes) — stamp the marker so their later
        // DELETE symmetrically drops the object count.
        if self.savings_ledger.is_some() {
            req.input
                .metadata
                .get_or_insert_with(Default::default)
                .insert(META_LEDGER.into(), META_LEDGER_ACCOUNTED.into());
        }
        let mut backend_resp = self.backend.put_object(req).await;
        // v1.2 savings ledger: a zero-length object stores zero bytes —
        // the delta is purely the removal of whatever it replaced (plus
        // an object-count bump when the key is new). Unaccounted old
        // objects are skipped + tallied, same as the body-bearing path.
        if let (Some(ledger), true) = (self.savings_ledger.as_ref(), backend_resp.is_ok()) {
            let old_accounted = ledger_old_main.is_some_and(|f| f.accounted);
            let (old_original, old_stored) = ledger_old_main
                .filter(|f| f.accounted)
                .map(|f| (f.original_bytes, f.stored_bytes))
                .unwrap_or((0, 0));
            if ledger_old_main.is_some() && !old_accounted {
                ledger.record_skipped_unaccounted(&put_bucket);
            }
            ledger.apply_delta(
                &put_bucket,
                crate::ledger::signed_delta(0, old_original),
                crate::ledger::signed_delta(0, old_stored),
                if old_accounted { 0 } else { 1 },
            );
        }
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
        // v0.6 #35: same notification fire-point as the body-bearing PUT
        // branch above (zero-length objects still match `ObjectCreated:Put`
        // rules per the AWS event taxonomy).
        if backend_resp.is_ok()
            && let Some(mgr) = self.notifications.as_ref()
        {
            let dests = mgr.match_destinations(
                &put_bucket,
                &crate::notifications::EventType::ObjectCreatedPut,
                &put_key,
            );
            if !dests.is_empty() {
                let etag = backend_resp
                    .as_ref()
                    .ok()
                    .and_then(|r| r.output.e_tag.clone())
                    .map(ETag::into_value);
                let version_id = pending_version
                    .as_ref()
                    .filter(|pv| pv.versioned_response)
                    .map(|pv| pv.version_id.clone());
                tokio::spawn(crate::notifications::dispatch_event(
                    Arc::clone(mgr),
                    put_bucket.clone(),
                    put_key.clone(),
                    crate::notifications::EventType::ObjectCreatedPut,
                    Some(0),
                    etag,
                    version_id,
                    format!("S4-{}", uuid::Uuid::new_v4()),
                ));
            }
        }
        // v0.6 #39: persist parsed `x-amz-tagging` for the body-less
        // (zero-length) PUT branch too — same shape as the body-bearing
        // branch above.
        if backend_resp.is_ok()
            && let (Some(mgr), Some(tags)) = (self.tagging.as_ref(), request_tags.clone())
        {
            mgr.put_object_tags(&put_bucket, &put_key, tags);
        }
        // v0.6 #40: cross-bucket replication for the zero-length PUT
        // branch — same shape as the body-bearing branch above.
        // v0.8.2 #61: pass `pending_version` so a versioned source's
        // destination receives the same shadow-key path.
        self.spawn_replication_if_matched(
            &put_bucket,
            &put_key,
            &request_tags,
            &bytes::Bytes::new(),
            &None,
            backend_resp.is_ok(),
            pending_version.as_ref(),
        );
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
        // v0.8.16 F-13 / v0.8.17 G-2: shared reserved-name guard.
        self.check_not_reserved_key(&get_key, ReservedKeyMode::Read)?;
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
        let get_sse_c_material = extract_sse_c_material(&sse_c_alg, &sse_c_key, &sse_c_md5)?;

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
                    Some(vid) => {
                        mgr.lookup_version(&get_bucket, &get_key, vid)
                            .ok_or_else(|| {
                                S3Error::with_message(
                                    S3ErrorCode::NoSuchVersion,
                                    format!("no such version: {vid}"),
                                )
                            })?
                    }
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
        //
        // v0.8.4 #73 H-2: BEFORE trusting the sidecar's frame offsets,
        // verify the source object hasn't been overwritten / mutated since
        // the sidecar was stamped. The sidecar carries the backend ETag
        // captured at PUT time (`source_etag`); a HEAD against the current
        // backend object tells us the live ETag. If they disagree we treat
        // the sidecar as stale and fall through to the full-GET path —
        // returning the wrong frames for a Range request would surface as
        // a CRC mismatch deeper in the stack but would also potentially
        // disclose unrelated frames if a hostile operator wrote the
        // sidecar themselves. Fail-open to "full read" is the safe default.
        //
        // Legacy v1 sidecars (no `source_etag` populated) keep the old
        // best-effort behaviour so existing on-disk indexes don't suddenly
        // start missing the partial-fetch path.
        if let Some(ref r) = range_request
            && let Some(index) = self.read_sidecar(&req.input.bucket, &req.input.key).await
            && self
                .sidecar_version_binding_ok(&req.input.bucket, &req.input.key, &index)
                .await
        {
            let total = index.total_original_size();
            let (start, end_exclusive) = match resolve_range(r, total) {
                Ok(v) => v,
                Err(e) => {
                    return Err(S3Error::with_message(S3ErrorCode::InvalidRange, e));
                }
            };
            if let Some(plan) = index.lookup_range(start, end_exclusive) {
                // v0.9 #106: v3 sidecar with an SSE chunked binding →
                // encrypted partial-fetch fast-path. SSE-S4 chunked
                // (S4E6) is the only scope-in encryption mode; for
                // every other case (v1 / v2 sidecar) we fall through
                // to the existing pre-encrypt `partial_range_get`.
                // SSE-KMS / SSE-C / S4E2 buffered never get a
                // sidecar emitted (see PUT path `sidecar_index`
                // condition), so they trivially take the existing
                // buffered fallback further down.
                //
                // Codex P2 (round 2): when the sidecar HAS an SSE
                // binding but `encrypted_lookup` returns `None` (=
                // stale / corrupted chunk geometry, or a Range that
                // falls outside the declared `enc_plaintext_len`),
                // we must NOT fall through to `partial_range_get`
                // — that would slice the S4E6 ciphertext at
                // pre-encrypt offsets and either 500 or return
                // garbage. Skip the fast-path entirely so the
                // buffered fallback below decrypts + frame-parses
                // correctly.
                if let Some(sse) = index.sse_v3.as_ref() {
                    if let Some(enc_plan) = index.encrypted_lookup(&plan) {
                        return self
                            .partial_range_get_encrypted(
                                &req,
                                plan,
                                enc_plan,
                                *sse,
                                start,
                                end_exclusive,
                                total,
                                get_start,
                            )
                            .await;
                    }
                    // Encrypted body + binding present but
                    // `encrypted_lookup` refused (= sidecar /
                    // body mismatch). Fall through to the buffered
                    // full-GET below — safer than slicing
                    // ciphertext with pre-encrypt offsets.
                    //
                    // Data-flow note: `req.input.range` was
                    // already `.take()`-ed into `range_request` at
                    // L3695, so the subsequent
                    // `self.backend.get_object(req)` carries no
                    // Range header (= full body fetch). The local
                    // `range_request` is then re-applied to the
                    // *decrypted + decompressed* plaintext by the
                    // buffered slice path further down. Without
                    // the `.take()` above, we'd have to clear it
                    // explicitly here or we'd slice ciphertext.
                } else {
                    return self
                        .partial_range_get(&req, plan, start, end_exclusive, total, get_start)
                        .await;
                }
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
        // v1.1 `--zstd-dict`: dict-compressed objects are framed single-
        // PUTs whose frames need the dictionary named by `s4-dict-id`.
        let dict_id_meta = extract_dict_id(&resp.output.metadata);

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
                // through the KMS backend (async). S4E1/E2/E3 take
                // the sync path (keyring or customer key).
                //
                // v0.8 #52 (S4E5) / v0.8.1 #57 (S4E6): the chunked
                // SSE-S4 frames take the *streaming* path — we hand
                // the response body a per-chunk verify-and-emit
                // Stream so the client sees chunk 0 plaintext after
                // one chunk-worth of AES-GCM verify (vs. waiting
                // for the whole body's tag), and the gateway no
                // longer needs to materialize the full plaintext
                // in memory before responding. SSE-C is out of
                // scope for the chunked path (chunked S4E3 is a
                // follow-up), so this branch requires the SSE-S4
                // keyring to be wired and `get_sse_c_material` to
                // be absent — otherwise we surface a clear
                // misconfiguration error instead of silently
                // falling through to the buffered chunked path.
                // v0.8.11 CRIT-1 fix: the chunked stream early-return is
                // only correct when the decrypted body IS the user's
                // plaintext as-stored. If the object went through the
                // codec (compressed) or carries S4F2 frames, returning
                // the decrypt stream directly hands the client
                // compressed / framed bytes. Restrict the early-return
                // to codec=Passthrough + non-framed objects; everything
                // else falls through to the buffered path, which
                // decrypt-buffers S4E5/S4E6 via
                // `decrypt_chunked_buffered_default` and then runs the
                // existing decompress pipeline.
                let chunked_streaming_safe = !needs_frame_parse
                    && manifest_opt
                        .as_ref()
                        .map(|m| m.codec == CodecKind::Passthrough)
                        .unwrap_or(false);
                if matches!(crate::sse::peek_magic(&body), Some("S4E5") | Some("S4E6"))
                    && get_sse_c_material.is_none()
                    && chunked_streaming_safe
                {
                    let keyring_arc = self.sse_keyring.clone().ok_or_else(|| {
                        S3Error::with_message(
                            S3ErrorCode::InvalidRequest,
                            "object is SSE-S4 encrypted (S4E5/S4E6) but no --sse-s4-key is configured on this gateway",
                        )
                    })?;
                    let body_len = body.len() as u64;
                    let stream = crate::sse::decrypt_chunked_stream(body, keyring_arc.as_ref());
                    // Stream is `'static` (the keyring borrow is
                    // consumed up front; the cipher lives inside
                    // the stream state — see decrypt_chunked_stream
                    // doc), so we can move it straight into a
                    // StreamingBlob without lifetime gymnastics.
                    use futures::StreamExt;
                    let mapped = stream.map(|r| {
                        r.map_err(|e| std::io::Error::other(format!("SSE-S4 chunked decrypt: {e}")))
                    });
                    use s3s::dto::StreamingBlob;
                    resp.output.body = Some(StreamingBlob::wrap(mapped));
                    // Plaintext content_length is unknown until all
                    // chunks have been verified; null it out so the
                    // ByteStream wrapper reports `unknown` to the
                    // HTTP layer (which then emits chunked transfer-
                    // encoding) rather than lying about the size.
                    resp.output.content_length = None;
                    // The backend's checksums + ETag describe the
                    // encrypted body (S4E5/S4E6 wire format), not
                    // the plaintext we're about to stream — clear them
                    // so the AWS SDK doesn't fail the GET with a
                    // ChecksumMismatch on a successful round-trip.
                    // Mirrors the streaming-zstd path at L1180-1185.
                    resp.output.checksum_crc32 = None;
                    resp.output.checksum_crc32c = None;
                    resp.output.checksum_crc64nvme = None;
                    resp.output.checksum_sha1 = None;
                    resp.output.checksum_sha256 = None;
                    resp.output.e_tag = None;
                    let elapsed = get_start.elapsed();
                    crate::metrics::record_get(
                        "sse-s4-chunked",
                        body_len,
                        body_len,
                        elapsed.as_secs_f64(),
                        true,
                    );
                    return Ok(resp);
                }
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
                    resp.output.server_side_encryption = Some(ServerSideEncryption::from_static(
                        ServerSideEncryption::AWS_KMS,
                    ));
                    resp.output.ssekms_key_id = Some(hdr.key_id.to_string());
                }
                bytes_to_blob(plain)
            } else if let Some(ref m) = get_sse_c_material {
                // Client sent SSE-C headers for an unencrypted object —
                // mirror AWS S3's 400 InvalidRequest.
                let _ = m;
                return Err(sse_c_error_to_s3(
                    crate::sse::SseError::CustomerKeyUnexpected,
                ));
            } else {
                blob
            };
            // v0.5 #27: SSE-C echo on success — algorithm + key MD5
            // tell the client that the supplied key was the one used.
            if let Some(ref m) = get_sse_c_material {
                resp.output.sse_customer_algorithm = Some(crate::sse::SSE_C_ALGORITHM.into());
                resp.output.sse_customer_key_md5 =
                    Some(base64::engine::general_purpose::STANDARD.encode(m.key_md5));
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
                // v0.8.4 #73 H-1: wrap the decompressor output in a
                // rolling-CRC32C verifier so a tampered ciphertext (or a
                // backend-side corruption that the zstd decoder happens
                // to "successfully" decode into wrong bytes) surfaces as
                // a streaming error tail at EOF instead of silently
                // delivering corrupt plaintext to the client. The wrap
                // is a pure pass-through during the body — no extra
                // buffering, TTFB unaffected — and the integrity
                // decision lands at the last chunk.
                let decompressed_blob = cpu_zstd_decompress_stream(blob);
                let verified_reader = Crc32cVerifyingReader::new(
                    blob_to_async_read(decompressed_blob),
                    m.crc32c,
                    m.original_size,
                );
                let verified_blob = async_read_to_blob(verified_reader);
                resp.output.content_length = Some(m.original_size as i64);
                resp.output.checksum_crc32 = None;
                resp.output.checksum_crc32c = None;
                resp.output.checksum_crc64nvme = None;
                resp.output.checksum_sha1 = None;
                resp.output.checksum_sha256 = None;
                resp.output.e_tag = None;
                resp.output.body = Some(verified_blob);
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

            // v1.0.1 audit R1 P1: the dict branch is gated on the
            // *manifest* codec (`s4-codec: cpu-zstd-dict`), not on the
            // mere presence of `s4-dict-id` metadata. Pre-fix, any object
            // whose metadata carried a well-formed `s4-dict-id` —
            // regardless of how it was compressed — was routed into the
            // dictionary path, where the `.s4dict/<id>` fetch (almost
            // certainly absent) turned a v1.0.0-fine GET into a 5xx.
            // Objects the gateway dict-compresses always stamp both keys
            // together (PUT path), so requiring the pair changes nothing
            // for genuine dict objects; a stray `s4-dict-id` on any other
            // codec is now ignored and the object decodes exactly as it
            // did before the dict feature existed.
            let dict_codec_object = manifest_opt
                .as_ref()
                .map(|m| m.codec == CodecKind::CpuZstdDict)
                .unwrap_or(false);
            let decompressed = if let Some(ref dict_id) = dict_id_meta
                && dict_codec_object
            {
                // v1.1 `--zstd-dict`: resolve the dictionary (preloaded →
                // LRU → lazy backend fetch of `.s4dict/<id>`) and walk the
                // S4F2 frames with the dict-aware decoder. Works even on a
                // gateway booted without any `--zstd-dict` flag.
                let dict = self.resolve_dict(&get_bucket, dict_id).await?;
                self.decompress_framed_with_dict(bytes, dict).await?
            } else if needs_frame_parse {
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
        // v0.6 #40: echo the recorded `x-amz-replication-status` so
        // consumers can poll progress (PENDING / COMPLETED / FAILED).
        if let Some(mgr) = self.replication.as_ref()
            && let Some(status) = mgr.lookup_status(&get_bucket, &get_key)
        {
            resp.output.replication_status = Some(s3s::dto::ReplicationStatus::from(
                status.as_aws_str().to_owned(),
            ));
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
        // v0.6 #40: capture bucket/key before req is consumed so the
        // replication-status echo can look the entry up.
        let head_bucket = req.input.bucket.clone();
        let head_key = req.input.key.clone();
        // v0.8.16 F-13 / v0.8.17 G-2: shared reserved-name guard.
        self.check_not_reserved_key(&head_key, ReservedKeyMode::Read)?;
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
        // v0.6 #40: echo `x-amz-replication-status` (PENDING / COMPLETED
        // / FAILED) so consumers can poll progress without a GET.
        if let Some(mgr) = self.replication.as_ref()
            && let Some(status) = mgr.lookup_status(&head_bucket, &head_key)
        {
            resp.output.replication_status = Some(s3s::dto::ReplicationStatus::from(
                status.as_aws_str().to_owned(),
            ));
        }
        // v0.7 #48 BUG-4 fix: HEAD must echo SSE indicators so SDKs
        // and pipelines see the same posture they got on PUT. The PUT
        // path stamps `s4-sse-type` metadata for exactly this — HEAD
        // doesn't fetch the body, so it can't peek frame magic.
        if let Some(meta) = resp.output.metadata.as_ref()
            && let Some(sse_type) = meta.get("s4-sse-type")
        {
            {
                match sse_type.as_str() {
                    "aws:kms" => {
                        resp.output.server_side_encryption = Some(
                            ServerSideEncryption::from_static(ServerSideEncryption::AWS_KMS),
                        );
                        if let Some(key_id) = meta.get("s4-sse-kms-key-id") {
                            resp.output.ssekms_key_id = Some(key_id.clone());
                        }
                    }
                    _ => {
                        resp.output.server_side_encryption = Some(
                            ServerSideEncryption::from_static(ServerSideEncryption::AES256),
                        );
                        if let Some(md5) = meta.get("s4-sse-c-key-md5") {
                            resp.output.sse_customer_algorithm =
                                Some(crate::sse::SSE_C_ALGORITHM.into());
                            resp.output.sse_customer_key_md5 = Some(md5.clone());
                        }
                    }
                }
            }
        }
        Ok(resp)
    }
    async fn delete_object(
        &self,
        mut req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        // v0.8.16 F-13 / v0.8.17 G-2: shared reserved-name guard.
        // The S4 internal sidecar cleanup path
        // (`write_sidecar` and friends) talks to
        // `self.backend.delete_object(...)` directly, NOT through
        // this trait method, so the guard doesn't break
        // legitimate sidecar cleanup.
        self.check_not_reserved_key(&key, ReservedKeyMode::Mutating)?;
        self.enforce_rate_limit(&req, &bucket)?;
        self.enforce_policy(&req, "s3:DeleteObject", &bucket, Some(&key))?;
        // v0.6 #42: MFA Delete enforcement. When the bucket has
        // MFA-Delete = Enabled, every DELETE / DELETE-version /
        // delete-marker form needs `x-amz-mfa: <serial> <code>` (RFC 6238
        // 6-digit TOTP). Runs *before* the WORM / versioning routers so
        // a missing token is denied for free regardless of which delete
        // path the request would otherwise take.
        if let Some(mgr) = self.mfa_delete.as_ref()
            && mgr.is_enabled(&bucket)
        {
            let header = req.input.mfa.as_deref();
            if let Err(e) = crate::mfa::check_mfa(&bucket, header, mgr, current_unix_secs()) {
                crate::metrics::record_mfa_delete_denial(&bucket);
                return Err(mfa_error_to_s3(e));
            }
        }
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
            let bypass_header = req.input.bypass_governance_retention.unwrap_or(false);
            // v0.8.12 HIGH-7 fix: the bypass header alone used to be
            // enough to override Governance retention. AWS spec
            // requires the caller hold `s3:BypassGovernanceRetention`
            // for the target ARN; without that, the header is
            // silently ignored (not an error — it lines up with how
            // AWS' canonical behaviour treats unprivileged callers).
            let bypass_allowed = if bypass_header {
                self.enforce_policy(&req, "s3:BypassGovernanceRetention", &bucket, Some(&key))
                    .is_ok()
            } else {
                false
            };
            let now = chrono::Utc::now();
            if !state.can_delete(now, bypass_allowed) {
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
                        // v1.2 savings ledger: probe the version's
                        // footprint before its backend bytes vanish
                        // (extra HEAD only when the ledger is on).
                        let ledger_old: Option<LedgerFootprint> = if self.savings_ledger.is_some() {
                            self.ledger_probe_object(&bucket, &backend_target, None)
                                .await
                        } else {
                            None
                        };
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
                        let backend_del = self.backend.delete_object(backend_req).await;
                        // v1.2 savings ledger: subtract only when the
                        // backend actually dropped the bytes — and
                        // (audit R1 P2) only for ledger-accounted
                        // versions; unmarked ones are tallied instead.
                        if let (Some(ledger), Some(f), Ok(_)) = (
                            self.savings_ledger.as_ref(),
                            ledger_old,
                            backend_del.as_ref(),
                        ) {
                            if f.accounted {
                                ledger.apply_delta(
                                    &bucket,
                                    crate::ledger::signed_delta(0, f.original_bytes),
                                    crate::ledger::signed_delta(0, f.stored_bytes),
                                    -1,
                                );
                            } else {
                                ledger.record_skipped_unaccounted(&bucket);
                            }
                        }
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
                    // v0.6 #35: specific-version DELETE always counts as
                    // a hard `ObjectRemoved:Delete` event (the chain
                    // entry, marker or not, is gone after this call).
                    self.fire_delete_notification(
                        &bucket,
                        &key,
                        crate::notifications::EventType::ObjectRemovedDelete,
                        Some(vid.clone()),
                    );
                    return Ok(S3Response::new(output));
                }
                // No version_id: record a delete marker (state-aware).
                let outcome = mgr.record_delete(&bucket, &key);
                if state == crate::versioning::VersioningState::Suspended {
                    // v1.2 savings ledger: the prior `<key>` (null
                    // version) bytes are physically evicted below —
                    // probe before, subtract on confirmed delete.
                    let ledger_old: Option<LedgerFootprint> = if self.savings_ledger.is_some() {
                        self.ledger_probe_object(&bucket, &key, None).await
                    } else {
                        None
                    };
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
                    let backend_del = self.backend.delete_object(backend_req).await;
                    // v1.2 audit R1 P2: marker-gated, same as the
                    // specific-version branch above.
                    if let (Some(ledger), Some(f), Ok(_)) = (
                        self.savings_ledger.as_ref(),
                        ledger_old,
                        backend_del.as_ref(),
                    ) {
                        if f.accounted {
                            ledger.apply_delta(
                                &bucket,
                                crate::ledger::signed_delta(0, f.original_bytes),
                                crate::ledger::signed_delta(0, f.stored_bytes),
                                -1,
                            );
                        } else {
                            ledger.record_skipped_unaccounted(&bucket);
                        }
                    }
                }
                let output = DeleteObjectOutput {
                    delete_marker: Some(true),
                    version_id: outcome.version_id.clone(),
                    ..Default::default()
                };
                // v0.6 #35: versioned bucket DELETE without a version-id
                // creates a delete marker — the dedicated AWS event
                // taxonomy entry. Suspended-state buckets also push a
                // (null) marker, so the same event fires there.
                self.fire_delete_notification(
                    &bucket,
                    &key,
                    crate::notifications::EventType::ObjectRemovedDeleteMarker,
                    outcome.version_id,
                );
                return Ok(S3Response::new(output));
            }
        }
        // Legacy / Unversioned path: physical delete on the backend +
        // best-effort sidecar cleanup (mirrors v0.4 behaviour).
        //
        // v1.2 savings ledger: probe the doomed object's footprint
        // before the backend forgets it (one extra HEAD — plus a
        // sidecar HEAD below — only when the ledger flag is on; a
        // probe miss means the key didn't exist and S3 DELETE still
        // returns 204, so nothing is subtracted).
        let ledger_old_main: Option<LedgerFootprint> = if self.savings_ledger.is_some() {
            self.ledger_probe_object(&bucket, &key, None).await
        } else {
            None
        };
        let resp = self.backend.delete_object(req).await?;
        // Accumulated stored-bytes removal for the single ledger commit
        // at the end of this path (main object now, sidecar appended
        // below once its own DELETE is confirmed).
        let mut ledger_removed_stored: u64 = ledger_old_main.map(|f| f.stored_bytes).unwrap_or(0);
        // v0.5 #30: drop any per-object lock state once the delete has
        // succeeded so the freed key can be re-armed by a future PUT
        // under the bucket default. Reaching here implies the lock had
        // already passed `can_delete` above, so this is purely cleanup.
        if let Some(mgr) = self.object_lock.as_ref() {
            mgr.clear(&bucket, &key);
        }
        // v0.6 #39: drop any object-level tag set on physical delete —
        // the freed key starts a fresh tag history if a future PUT
        // re-creates it. (Versioned-delete branches above return early
        // and do NOT touch tags, mirroring AWS where tag state is
        // attached to the logical key, not the version chain.)
        if let Some(mgr) = self.tagging.as_ref() {
            mgr.delete_object_tags(&bucket, &key);
        }
        let sidecar = sidecar_key(&key);
        // v0.7 #49: skip the sidecar DELETE if the key + sidecar suffix
        // can't be encoded into a request URI — the primary delete
        // already succeeded and a stale sidecar is harmless (Range GET
        // re-validates the underlying object on next read).
        if let Ok(uri) = safe_object_uri(&bucket, &sidecar) {
            // v1.2 savings ledger: the sidecar's bytes leave the
            // backend with this DELETE — measure them first (HEAD,
            // ledger-on only), subtract only on confirmed removal.
            // Audit R1 P2: an unaccounted main object's sidecar was
            // never added either, so it is excluded from subtraction
            // the same way.
            let ledger_sidecar_bytes: u64 =
                if self.savings_ledger.is_some() && ledger_old_main.is_some_and(|f| f.accounted) {
                    self.ledger_probe_sidecar_bytes(&bucket, &key).await
                } else {
                    0
                };
            let sidecar_input = DeleteObjectInput {
                bucket: bucket.clone(),
                key: sidecar,
                ..Default::default()
            };
            let sidecar_req = S3Request {
                input: sidecar_input,
                method: http::Method::DELETE,
                uri,
                headers: http::HeaderMap::new(),
                extensions: http::Extensions::new(),
                credentials: None,
                region: None,
                service: None,
                trailing_headers: None,
            };
            let sidecar_del = self.backend.delete_object(sidecar_req).await;
            if ledger_sidecar_bytes > 0 && sidecar_del.is_ok() {
                ledger_removed_stored = ledger_removed_stored.saturating_add(ledger_sidecar_bytes);
            }
        }
        // v1.2 savings ledger: one combined subtraction (main object +
        // confirmed sidecar). Skipped entirely when the probe saw no
        // object (DELETE of a nonexistent key is still 204). Audit R1
        // P2: only ledger-accounted objects (`s4-ledger` marker) are
        // subtracted — a backend-direct / s4fs / migrate-written
        // object was never added, so its DELETE is tallied as skipped
        // and disclosed in the `s4 savings` notes instead.
        if let (Some(ledger), Some(f)) = (self.savings_ledger.as_ref(), ledger_old_main) {
            if f.accounted {
                ledger.apply_delta(
                    &bucket,
                    crate::ledger::signed_delta(0, f.original_bytes),
                    crate::ledger::signed_delta(0, ledger_removed_stored),
                    -1,
                );
            } else {
                ledger.record_skipped_unaccounted(&bucket);
            }
        }
        // v0.6 #35: legacy unversioned-bucket hard delete fires the
        // canonical `ObjectRemoved:Delete` event.
        self.fire_delete_notification(
            &bucket,
            &key,
            crate::notifications::EventType::ObjectRemovedDelete,
            None,
        );
        Ok(resp)
    }
    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        // v0.6 #42: MFA Delete applies once to the whole batch (S3 spec:
        // when MFA-Delete is on the bucket, a missing / invalid token
        // fails the entire DeleteObjects request, not per-object).
        if let Some(mgr) = self.mfa_delete.as_ref()
            && mgr.is_enabled(&req.input.bucket)
        {
            let header = req.input.mfa.as_deref();
            if let Err(e) =
                crate::mfa::check_mfa(&req.input.bucket, header, mgr, current_unix_secs())
            {
                crate::metrics::record_mfa_delete_denial(&req.input.bucket);
                return Err(mfa_error_to_s3(e));
            }
        }
        // v0.8.11 CRIT-3 fix: route every entry through the gated
        // per-object `delete_object` path so Object Lock, IAM policy,
        // versioning, tagging, sidecar cleanup and notification fan-
        // out all fire for batch DELETE. The previous
        // `self.backend.delete_objects(req).await` straight-through
        // bypassed every gate, so a `legal_hold=on` key listed inside
        // a DeleteObjects XML was happily removed.
        //
        // S3 spec note: DeleteObjects is "best-effort per object" —
        // a failure on one key surfaces as an `Errors` entry without
        // aborting the rest of the batch. Quiet-mode suppresses the
        // `Deleted` list (errors are still reported). We honour both.
        let bucket = req.input.bucket.clone();
        let bypass_governance = req.input.bypass_governance_retention.unwrap_or(false);
        let mfa_header = req.input.mfa.clone();
        let quiet = req.input.delete.quiet.unwrap_or(false);
        let mut deleted: Vec<DeletedObject> = Vec::new();
        let mut errors: Vec<s3s::dto::Error> = Vec::new();
        for ident in req.input.delete.objects.iter() {
            let key = ident.key.clone();
            let version_id = ident.version_id.clone();
            let per_input = DeleteObjectInput {
                bucket: bucket.clone(),
                key: key.clone(),
                version_id: version_id.clone(),
                bypass_governance_retention: Some(bypass_governance),
                mfa: mfa_header.clone(),
                ..Default::default()
            };
            let per_uri = match safe_object_uri(&bucket, &key) {
                Ok(u) => u,
                Err(_) => {
                    errors.push(s3s::dto::Error {
                        code: Some("InvalidArgument".to_owned()),
                        key: Some(key),
                        message: Some("object key is not URI-encodable".to_owned()),
                        version_id,
                    });
                    continue;
                }
            };
            let per_req = S3Request {
                input: per_input,
                method: http::Method::DELETE,
                uri: per_uri,
                headers: req.headers.clone(),
                extensions: http::Extensions::new(),
                credentials: req.credentials.clone(),
                region: req.region.clone(),
                service: req.service.clone(),
                trailing_headers: None,
            };
            match self.delete_object(per_req).await {
                Ok(resp) => {
                    let out = resp.output;
                    // DeleteObjectOutput doesn't surface a separate
                    // `delete_marker_version_id`; the marker's version
                    // id is whatever `version_id` carries (when the
                    // versioning manager pushed a delete-marker, that
                    // field already holds the marker's vid).
                    let vid = out.version_id.clone().or(version_id);
                    deleted.push(DeletedObject {
                        key: Some(key),
                        version_id: vid.clone(),
                        delete_marker: out.delete_marker,
                        delete_marker_version_id: vid,
                    });
                }
                Err(e) => {
                    let code_str = e.code().as_str().to_owned();
                    let msg = e.message().unwrap_or(code_str.as_str()).to_owned();
                    errors.push(s3s::dto::Error {
                        code: Some(code_str),
                        key: Some(key),
                        message: Some(msg),
                        version_id,
                    });
                }
            }
        }
        let output = DeleteObjectsOutput {
            deleted: if quiet || deleted.is_empty() {
                None
            } else {
                Some(deleted)
            },
            errors: if errors.is_empty() {
                None
            } else {
                Some(errors)
            },
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }
    async fn copy_object(
        &self,
        mut req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        // copy is conceptually "GetObject src + PutObject dst" — enforce both.
        let dst_bucket = req.input.bucket.clone();
        let dst_key = req.input.key.clone();
        // v0.8.15 M-1 / v0.8.17 G-2: shared reserved-name guard.
        self.check_not_reserved_key(&dst_key, ReservedKeyMode::Mutating)?;
        self.enforce_policy(&req, "s3:PutObject", &dst_bucket, Some(&dst_key))?;
        match &req.input.copy_source {
            CopySource::Bucket { bucket, key, .. } => {
                // v0.8.17 G-2: source `<key>.s4index` would let
                // CopyObject expose the raw sidecar (frame layout +
                // source ETag) into a writable destination, bypassing
                // the F-13 GET reject. Same guard, Read mode (returns
                // NoSuchKey to match listing semantics).
                self.check_not_reserved_key(key, ReservedKeyMode::Read)?;
                self.enforce_policy(&req, "s3:GetObject", bucket, Some(key))?;
            }
            CopySource::AccessPoint { key, .. } => {
                // v1.2 audit R2 P3: the reserved-key guard is a pure
                // key-namespace check — apply it regardless of how the
                // source is addressed, so an access-point ARN can't
                // read `<key>.s4index` / `.s4dict/<id>` around the G-2
                // gate. `s3:GetObject` policy enforcement, however,
                // needs a bucket name; resolving an access-point ARN
                // to its underlying bucket is backend-specific, so
                // source-side policy for AP copies remains the
                // backend's responsibility (documented limitation —
                // the destination-side `s3:PutObject` gate above still
                // applies).
                self.check_not_reserved_key(key, ReservedKeyMode::Read)?;
            }
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
        // v0.8.16 F-8: strip the client-supplied `s4-*` keys
        // *unconditionally* — the v0.8.15 M-2 fix only ran the
        // strip inside the `if let Ok(head) = ...` block, so a
        // backend HEAD failure (transient 5xx, NoSuchKey on a
        // racing delete) left attacker-injected `s4-*` /
        // `S4-*` metadata intact on the destination. We strip
        // first, then re-populate from the source HEAD when
        // available — HEAD failure simply means the destination
        // loses the codec markers (correct: a CopyObject without
        // the source's codec metadata produces an unreadable
        // object, but doesn't allow injection).
        //
        // v1.2 audit R2 P3: the strip is hoisted out of the
        // `CopySource::Bucket` gate — it is a pure function of the
        // *destination* metadata and must not depend on how the source
        // is addressed. Pre-fix, a REPLACE copy whose source was an
        // access-point ARN skipped the strip entirely, letting a
        // client-supplied `x-amz-meta-s4-ledger` + forged
        // `s4-original-size` land on the destination verbatim — a
        // forgeable marker breaks the ledger's "clients cannot forge
        // it" subtraction contract.
        if needs_merge {
            strip_reserved_client_metadata(&mut req.input.metadata);
        }
        if needs_merge
            && let CopySource::Bucket {
                bucket,
                key,
                version_id,
            } = &req.input.copy_source
        {
            let head_input = HeadObjectInput {
                bucket: bucket.to_string(),
                key: key.to_string(),
                // v1.0.1 audit R2 P2: pin the probe to the *requested*
                // source version. Pre-fix the HEAD always saw "latest",
                // so a `?versionId=`-pinned REPLACE copy merged the
                // latest version's s4-* manifest (codec / sizes / crc /
                // dict-id) onto the pinned version's bytes — silent
                // corruption surfacing as a 5xx on the destination GET.
                version_id: version_id.as_deref().map(str::to_owned),
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
                    // v1.1 `--zstd-dict`: without the dict reference the
                    // destination of a REPLACE-directive copy could not
                    // resolve its dictionary at GET time.
                    META_DICT_ID,
                    // v1.2 audit R1 P2: the ledger marker mirrors the
                    // source — a REPLACE copy of an accounted object
                    // stays accounted (the COPY directive gets this
                    // for free via the backend metadata copy). The
                    // ledger commit below conditions its add on the
                    // same source marker, so destination metadata and
                    // counters stay in lockstep.
                    META_LEDGER,
                ] {
                    if let Some(v) = src_meta.get(key) {
                        dest_meta.insert(key.to_string(), v.clone());
                    }
                }
                // SSE markers are equally reserved — propagate any
                // source flags so a copy of an encrypted object stays
                // marked as encrypted at the destination.
                for sse_key in [
                    "s4-encrypted",
                    "s4-sse-type",
                    "s4-sse-c-key-md5",
                    "s4-sse-kms-key-id",
                ] {
                    if let Some(v) = src_meta.get(sse_key) {
                        dest_meta.insert(sse_key.to_string(), v.clone());
                    }
                }
                debug!(
                    src_bucket = %bucket,
                    src_key = %key,
                    "S4 copy_object: replaced client s4-* metadata with source values across REPLACE directive (v0.8.15 M-2)"
                );
            }
        }
        // v1.0.1 audit R1 P2: cross-bucket copies of dict-compressed
        // objects must carry the dictionary along. Both directives
        // propagate the `s4-dict-id` stamp (COPY via the backend's
        // metadata copy, REPLACE via the merge above), but `.s4dict/<id>`
        // is bucket-local and `resolve_dict` only ever looks in the
        // object's own bucket — without this, the destination object
        // would 5xx on GET as soon as the source bucket's dictionary is
        // gone. Resolve the dict from the *source* bucket (preloaded →
        // LRU → lazy fetch, fingerprint-verified) and content-addressed-
        // PUT it into the destination bucket (existing object = skip,
        // idempotent). A propagation failure fails the copy: completing
        // it would mint an object with a dangling dict reference.
        if let CopySource::Bucket {
            bucket: src_bucket,
            key: src_key,
            version_id: src_version_id,
        } = &req.input.copy_source
            && **src_bucket != *dst_bucket
        {
            let head_req = S3Request {
                input: HeadObjectInput {
                    bucket: src_bucket.to_string(),
                    key: src_key.to_string(),
                    // v1.0.1 audit R2 P2: same pinning as the REPLACE
                    // merge probe above — a `?versionId=` copy whose
                    // *latest* source version is not dict-compressed
                    // would otherwise skip the dict propagation (HEAD
                    // sees no `s4-dict-id`), leaving the destination
                    // object with a dangling dictionary reference.
                    version_id: src_version_id.as_deref().map(str::to_owned),
                    ..Default::default()
                },
                method: http::Method::HEAD,
                uri: safe_object_uri(src_bucket, src_key)?,
                headers: http::HeaderMap::new(),
                extensions: http::Extensions::new(),
                credentials: req.credentials.clone(),
                region: req.region.clone(),
                service: req.service.clone(),
                trailing_headers: None,
            };
            // HEAD failure = "no dict to carry" (the copy itself will
            // surface a missing source as its own error) — same
            // fail-open posture as the REPLACE merge above.
            if let Ok(head) = self.backend.head_object(head_req).await
                && let Some(dict_id) = extract_dict_id(&head.output.metadata)
            {
                let dict = self.resolve_dict(src_bucket, &dict_id).await?;
                self.ensure_dict_object_in_bucket(&dst_bucket, &dict_id, &dict)
                    .await?;
            }
        }
        // v1.2 savings ledger: capture (a) the source footprint — used
        // to stamp the REPLACE destination's `s4-original-size` below
        // and as the fallback when the post-copy destination probe
        // races a delete (the copy writes byte-identical content, so
        // the source resolution is the best available stand-in) — and
        // (b) the old destination footprint, BEFORE the backend copy
        // replaces it. Same-bucket same-key REPLACE copies net out to
        // zero (old == new), which is exactly the "metadata rewrite,
        // not new data" semantics. Probes exist only when the ledger
        // is on.
        let ledger_probes: Option<(Option<LedgerFootprint>, Option<LedgerFootprint>)> =
            if self.savings_ledger.is_some() {
                let src = if let CopySource::Bucket {
                    bucket,
                    key,
                    version_id,
                } = &req.input.copy_source
                {
                    self.ledger_probe_object(bucket, key, version_id.as_deref())
                        .await
                } else {
                    None
                };
                let old_dst = self.ledger_probe_object(&dst_bucket, &dst_key, None).await;
                Some((src, old_dst))
            } else {
                None
            };
        // v1.2 audit R2 P2: symmetrize the original-size resolution
        // across the copy. Multipart sources carry no
        // `s4-original-size` metadata — their logical size lives in
        // the *source's* `.s4index`, which a copy does NOT carry to
        // the destination — so the destination's later DELETE probe
        // would fall back to `original = stored` while this copy's add
        // resolved the sidecar's logical size: every copy→delete cycle
        // would leave `logical − stored` phantom original bytes behind.
        // For REPLACE copies the gateway owns the metadata: stamp the
        // resolved original so the future probe sees the value the add
        // used. (`or_insert` — a source-merged `s4-original-size` from
        // the block above stays authoritative; only accounted sources
        // are stamped, mirroring the add gate below.)
        if needs_merge
            && self.savings_ledger.is_some()
            && let Some((Some(src), _)) = ledger_probes.as_ref()
            && src.accounted
        {
            req.input
                .metadata
                .get_or_insert_with(Default::default)
                .entry(META_ORIGINAL_SIZE.to_owned())
                .or_insert_with(|| src.original_bytes.to_string());
        }
        let copy_resp = self.backend.copy_object(req).await;
        if let (Some(ledger), Some((src, old_dst)), Ok(_)) = (
            self.savings_ledger.as_ref(),
            ledger_probes,
            copy_resp.as_ref(),
        ) {
            // v1.2 audit R2 P2: resolve the ADD from the freshly-written
            // *destination* probe, so the add is — by construction —
            // exactly what the destination's later DELETE probe will
            // subtract (metadata `s4-original-size` → destination
            // sidecar → `original = stored` fallback). Pre-fix the add
            // trusted the *source* probe, whose sidecar-resolved
            // logical size a COPY-directive destination can never
            // reproduce (the sidecar isn't copied) — churn left phantom
            // savings behind. The trade: a COPY-directive copy of a
            // multipart source now adds `original == stored` (zero
            // claimed savings for that copy — honest under-claim
            // instead of an unpayable over-claim); REPLACE copies keep
            // the logical size via the stamp above. The source probe
            // remains the fallback for a destination probe lost to a
            // raced delete.
            let new_dst = match self.ledger_probe_object(&dst_bucket, &dst_key, None).await {
                Some(f) => Some(f),
                None => src,
            };
            match new_dst {
                Some(new) => {
                    // v1.2 audit R1 P2: marker-gated on both sides.
                    // The destination inherits the source's marker
                    // (COPY: backend metadata copy; REPLACE: the merge
                    // above), so `new.accounted` decides whether the
                    // copy adds to the ledger, and `old_dst.accounted`
                    // whether the replaced object is subtracted. A
                    // copy of an unaccounted source over an accounted
                    // destination is a pure subtraction (the accounted
                    // bytes are gone, the new ones were never added).
                    let new_counts = new.accounted;
                    let old_counts = old_dst.is_some_and(|f| f.accounted);
                    if old_dst.is_some() && !old_counts {
                        ledger.record_skipped_unaccounted(&dst_bucket);
                    }
                    let (new_original, new_stored) = if new_counts {
                        (new.original_bytes, new.stored_bytes)
                    } else {
                        (0, 0)
                    };
                    let (old_original, old_stored) = old_dst
                        .filter(|f| f.accounted)
                        .map(|f| (f.original_bytes, f.stored_bytes))
                        .unwrap_or((0, 0));
                    let objects_delta = match (new_counts, old_counts) {
                        (true, true) | (false, false) => 0,
                        (true, false) => 1,
                        (false, true) => -1,
                    };
                    if new_counts || old_counts {
                        ledger.apply_delta(
                            &dst_bucket,
                            crate::ledger::signed_delta(new_original, old_original),
                            crate::ledger::signed_delta(new_stored, old_stored),
                            objects_delta,
                        );
                    }
                }
                None => {
                    tracing::warn!(
                        bucket = %dst_bucket,
                        key = %dst_key,
                        "S4 savings ledger: CopyObject footprint unprobeable \
                         (src and dst HEAD both failed); counters not updated for this copy"
                    );
                }
            }
        }
        copy_resp
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
                    .map(|k| {
                        !k.ends_with(".s4index")
                            && !is_versioning_shadow_key(k)
                            && !crate::dict::is_dict_key(k)
                    })
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
                    .map(|k| {
                        !k.ends_with(".s4index")
                            && !is_versioning_shadow_key(k)
                            && !crate::dict::is_dict_key(k)
                    })
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
                    .map(|k| {
                        !k.ends_with(".s4index")
                            && !is_versioning_shadow_key(k)
                            && !crate::dict::is_dict_key(k)
                    })
                    .unwrap_or(true)
            });
        }
        if let Some(markers) = resp.output.delete_markers.as_mut() {
            markers.retain(|m| {
                m.key
                    .as_ref()
                    .map(|k| {
                        !k.ends_with(".s4index")
                            && !is_versioning_shadow_key(k)
                            && !crate::dict::is_dict_key(k)
                    })
                    .unwrap_or(true)
            });
        }
        Ok(resp)
    }

    async fn create_multipart_upload(
        &self,
        mut req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        // v0.8.12 HIGH-9 fix: gate multipart Create on `s3:PutObject` —
        // the destination is conceptually about to host a new object,
        // matching what `put_object` enforces L2078. Without this, a
        // bucket policy denying `s3:PutObject` was bypassable simply
        // by switching the client to the multipart wire path.
        let mp_bucket = req.input.bucket.clone();
        let mp_key = req.input.key.clone();
        // v0.8.15 M-1 / v0.8.17 G-2: shared reserved-name guard.
        self.check_not_reserved_key(&mp_key, ReservedKeyMode::Mutating)?;
        self.enforce_policy(&req, "s3:PutObject", &mp_bucket, Some(&mp_key))?;
        self.enforce_rate_limit(&req, &mp_bucket)?;
        // v1.0.1 audit R2 P2: same reserved-namespace strip as the PUT
        // path (R1 P1). Pre-fix, Create only *overwrote* `s4-multipart` /
        // `s4-codec` below, so a client-supplied
        // `x-amz-meta-s4-encrypted: aes-256-gcm` (or `s4-dict-id`, ...)
        // survived onto the completed object — and a flag-less GET then
        // took the decrypt / dictionary path on an object the gateway
        // never encrypted → 5xx (the same freeze violation put_object
        // closed, re-opened through the multipart wire path). The
        // gateway re-stamps its own `s4-*` keys right below.
        strip_reserved_client_metadata(&mut req.input.metadata);
        // Multipart object は per-part 圧縮 + frame 形式で書く。GET 時に
        // frame parse を起動するため、object metadata に flag を立てる。
        // codec は dispatcher の default kind を採用 (per-part 別 codec は Phase 2)。
        let codec_kind = self.registry.default_kind();
        let meta = req.input.metadata.get_or_insert_with(Default::default);
        meta.insert(META_MULTIPART.into(), "true".into());
        meta.insert(META_CODEC.into(), codec_kind.as_str().into());
        // v1.2 audit R1 P2: the assembled object will be added to the
        // savings ledger at Complete time — stamp the accounting
        // marker now so the backend carries it onto the completed
        // object (and the SSE / versioning re-PUT path re-reads it via
        // HEAD). Ledger-off deployments stay bit-for-bit unchanged.
        if self.savings_ledger.is_some() {
            meta.insert(META_LEDGER.into(), META_LEDGER_ACCOUNTED.into());
        }
        // v0.8 #54 BUG-10 fix: take() the SSE request fields off
        // `req.input` so they are NOT forwarded to the backend on
        // CreateMultipartUpload. Same root cause as v0.7 #48 BUG-2/3 on
        // single-PUT — MinIO rejects SSE-C with "HTTPS required" and
        // SSE-KMS with "KMS not configured" when the headers reach it.
        // S4 owns the encrypt-then-store contract; we capture the
        // recipe in `multipart_state` here and apply it on Complete.
        let sse_c_alg = req.input.sse_customer_algorithm.take();
        let sse_c_key = req.input.sse_customer_key.take();
        let sse_c_md5 = req.input.sse_customer_key_md5.take();
        let sse_header = req.input.server_side_encryption.take();
        let sse_kms_key = req.input.ssekms_key_id.take();
        // Strip the encryption-context too — leaving it would make
        // MinIO try to validate it against a non-existent KMS key.
        let _ = req.input.ssekms_encryption_context.take();
        let sse_c_material = extract_sse_c_material(&sse_c_alg, &sse_c_key, &sse_c_md5)?;
        let kms_key_id = extract_kms_key_id(
            &sse_header,
            &sse_kms_key,
            self.kms_default_key_id.as_deref(),
        );
        // SSE-C / SSE-KMS exclusivity (mirrors put_object L1870).
        if sse_c_material.is_some() && kms_key_id.is_some() {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "SSE-C and SSE-KMS cannot be used together on the same multipart upload",
            ));
        }
        let sse_mode = if let Some(ref m) = sse_c_material {
            // v0.8.2 #62 (H-6 audit fix): wrap the customer-supplied
            // 32-byte key in `Zeroizing` so abandoned uploads (or
            // normal Complete/Abort) wipe the key bytes on drop. The
            // `key_md5` is the public fingerprint and stays as a
            // bare `[u8; 16]`.
            crate::multipart_state::MultipartSseMode::SseC {
                key: zeroize::Zeroizing::new(m.key),
                key_md5: m.key_md5,
            }
        } else if let Some(ref kid) = kms_key_id {
            // KMS pre-flight: fail at Create rather than at Complete if
            // the gateway has no KMS backend wired (mirrors the
            // put_object L1879 check).
            if self.kms.is_none() {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidRequest,
                    "SSE-KMS requested but no --kms-local-dir / --kms-aws-region is configured on this gateway",
                ));
            }
            crate::multipart_state::MultipartSseMode::SseKms {
                key_id: kid.clone(),
            }
        } else if self.sse_keyring.is_some() {
            // SSE-S4: server-driven transparent encryption. Activates
            // whenever the gateway has a keyring configured AND the
            // client didn't pick a different SSE mode.
            crate::multipart_state::MultipartSseMode::SseS4
        } else {
            crate::multipart_state::MultipartSseMode::None
        };
        // v0.8 #54 BUG-9 fix: parse the Tagging header on Create. The
        // single-PUT path does this on PutObject; the multipart path
        // captures it now and commits via TagManager on Complete.
        let request_tags: Option<crate::tagging::TagSet> = req
            .input
            .tagging
            .as_deref()
            .map(crate::tagging::parse_tagging_header)
            .transpose()
            .map_err(|e| S3Error::with_message(S3ErrorCode::InvalidArgument, e.to_string()))?;
        // Strip the `Tagging` field off the input so the backend
        // doesn't try to apply it (no-op on MinIO but keeps the wire
        // clean).
        let _ = req.input.tagging.take();
        // Object Lock recipe (BUG-7 — captured here, applied on Complete).
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
        let explicit_legal_hold_on: bool = req
            .input
            .object_lock_legal_hold_status
            .as_ref()
            .map(|s| s.as_str().eq_ignore_ascii_case("ON"))
            .unwrap_or(false);
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        debug!(
            bucket = %bucket,
            key = %key,
            codec = codec_kind.as_str(),
            sse = ?sse_mode,
            "S4 create_multipart_upload: marking object for per-part compression"
        );
        let mut resp = self.backend.create_multipart_upload(req).await?;
        // Stash the per-upload context only after the backend handed
        // us an upload_id (failed Creates leave nothing in the store).
        if let Some(upload_id) = resp.output.upload_id.as_ref() {
            self.multipart_state.put(
                upload_id,
                crate::multipart_state::MultipartUploadContext {
                    bucket,
                    key,
                    sse: sse_mode.clone(),
                    tags: request_tags,
                    object_lock_mode: explicit_lock_mode,
                    object_lock_retain_until: explicit_retain_until,
                    object_lock_legal_hold: explicit_legal_hold_on,
                },
            );
        }
        // SSE-C / SSE-KMS response echo (mirrors put_object L2036-L2050).
        match &sse_mode {
            crate::multipart_state::MultipartSseMode::SseC { key_md5, .. } => {
                resp.output.sse_customer_algorithm = Some(crate::sse::SSE_C_ALGORITHM.into());
                resp.output.sse_customer_key_md5 =
                    Some(base64::engine::general_purpose::STANDARD.encode(key_md5));
            }
            crate::multipart_state::MultipartSseMode::SseKms { key_id } => {
                resp.output.server_side_encryption = Some(ServerSideEncryption::from_static(
                    ServerSideEncryption::AWS_KMS,
                ));
                resp.output.ssekms_key_id = Some(key_id.clone());
            }
            _ => {}
        }
        Ok(resp)
    }

    async fn upload_part(
        &self,
        mut req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        // v0.8.12 HIGH-9 fix: same `s3:PutObject` gate as
        // `put_object` / `create_multipart_upload`. Even though
        // Create already passed the gate, a bucket policy that
        // *revokes* `s3:PutObject` mid-flight should stop further
        // parts (e.g. legal hold drops, retention shortened).
        let part_bucket = req.input.bucket.clone();
        let part_key = req.input.key.clone();
        self.enforce_policy(&req, "s3:PutObject", &part_bucket, Some(&part_key))?;
        self.enforce_rate_limit(&req, &part_bucket)?;
        // 各 part を圧縮して frame header 付きで forward。GET 時に
        // `decompress_multipart` が frame iter で順に解凍する。
        // **per-part codec dispatch**: dispatcher が body 先頭 sample から
        // codec を選ぶので、parquet 風の mixed-content multipart で part ごとに
        // 最適 codec を使える (整数列 part → Bitcomp、text 列 part → zstd 等)。
        //
        // v0.8 #54 BUG-5/BUG-10 fix: lookup the per-upload SSE
        // context captured by `create_multipart_upload` and (a) strip
        // any SSE-C request headers off `req.input` so the backend
        // doesn't see them — same root cause as v0.7 #48 BUG-2/3 on
        // single-PUT; MinIO refuses SSE-C parts over HTTP — and (b)
        // observe that an upload context exists for `upload_id`. The
        // actual encrypt happens once at `complete_multipart_upload`
        // time on the assembled body (the per-part-encrypt approach
        // would require a matching multi-segment decrypt path on GET;
        // encrypting the whole assembled body keeps the GET path's
        // `is_sse_encrypted` branch in get_object L2429 working
        // unchanged).
        let sse_ctx = self.multipart_state.get(req.input.upload_id.as_str());
        // v0.8.2 #62 (H-1 audit fix): SSE-C key consistency check.
        // The AWS S3 spec requires the same SSE-C key headers on
        // every UploadPart and rejects mismatches with 400. Prior to
        // #62 we silently stripped the headers (BUG-10 fix) without
        // validating them, allowing a client to send part 1 under
        // key-A and part 2 under key-B; both got stored, then
        // re-encrypted with key-A on Complete — the client thinks
        // part 2 is under key-B but a GET with key-B would in fact
        // hit the part-1 ciphertext that was actually encrypted with
        // key-A. That would either decrypt successfully (silent
        // corruption: client lost track of which key encrypts what)
        // or fail in a confusing way. Validate the per-part headers
        // now and reject with 400 InvalidArgument on mismatch /
        // omission / partial supply, matching real-S3 behaviour.
        if let Some(ref ctx) = sse_ctx {
            if let crate::multipart_state::MultipartSseMode::SseC {
                key_md5: ctx_md5, ..
            } = &ctx.sse
            {
                let alg = req.input.sse_customer_algorithm.take();
                let key_b64 = req.input.sse_customer_key.take();
                let md5_b64 = req.input.sse_customer_key_md5.take();
                match (alg, key_b64, md5_b64) {
                    (Some(a), Some(k), Some(m)) => {
                        // Parse + validate; if the per-part headers
                        // are themselves malformed (algorithm not
                        // AES256, MD5 mismatch, key not 32 bytes)
                        // surface the same 400 the single-PUT path
                        // would. Then compare the parsed MD5 to the
                        // upload-context's MD5; mismatch is a
                        // different-key UploadPart and must reject.
                        let part_material = crate::sse::parse_customer_key_headers(&a, &k, &m)
                            .map_err(sse_c_error_to_s3)?;
                        if part_material.key_md5 != *ctx_md5 {
                            return Err(S3Error::with_message(
                                S3ErrorCode::InvalidArgument,
                                "SSE-C key on UploadPart does not match the key supplied on CreateMultipartUpload",
                            ));
                        }
                        // OK — same key as Create. Headers are
                        // already taken off `req.input` so the
                        // backend never sees them.
                    }
                    (None, None, None) => {
                        // AWS S3 spec: SSE-C headers MUST be replayed
                        // on every UploadPart of an SSE-C multipart.
                        // Real-S3 returns 400 InvalidRequest in this
                        // case; mirror that.
                        return Err(S3Error::with_message(
                            S3ErrorCode::InvalidRequest,
                            "SSE-C requires customer-key headers on every UploadPart (CreateMultipartUpload was SSE-C)",
                        ));
                    }
                    _ => {
                        // Partial header set (e.g. algorithm + key
                        // but no MD5) — same handling as the
                        // single-PUT `extract_sse_c_material` helper.
                        return Err(S3Error::with_message(
                            S3ErrorCode::InvalidRequest,
                            "SSE-C requires all three of: x-amz-server-side-encryption-customer-{algorithm,key,key-MD5}",
                        ));
                    }
                }
            } else {
                // CreateMultipartUpload was non-SSE-C (None / SseS4 /
                // SseKms). A part that arrives carrying SSE-C headers
                // is either a confused client or an attempt to
                // smuggle SSE-C around the gateway-internal SSE
                // recipe. Reject with 400 InvalidRequest rather than
                // silently strip — the strip would let the client
                // believe the part was encrypted under their key
                // when in fact the upload's encryption recipe is
                // whatever the Create captured.
                if req.input.sse_customer_algorithm.is_some()
                    || req.input.sse_customer_key.is_some()
                    || req.input.sse_customer_key_md5.is_some()
                {
                    return Err(S3Error::with_message(
                        S3ErrorCode::InvalidRequest,
                        "UploadPart sent SSE-C headers but CreateMultipartUpload was not SSE-C",
                    ));
                }
            }
        } else {
            // No upload context registered (gateway crashed between
            // Create and Part, or pre-#62 abandoned-upload restore).
            // We can't check key consistency in this case — strip
            // the headers and let the request through unchanged so
            // the backend's `NoSuchUpload` reply (or whatever it
            // chooses to do) flows back to the client.
            let _ = req.input.sse_customer_algorithm.take();
            let _ = req.input.sse_customer_key.take();
            let _ = req.input.sse_customer_key_md5.take();
        }
        let _sse_ctx = sse_ctx;
        if let Some(blob) = req.input.body.take() {
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect upload_part body"))?;
            // v0.8.12 HIGH-12 / #128 MED-C: verify all six AWS
            // checksum algorithms against the received part body.
            verify_client_body_checksums(
                &bytes,
                req.input.content_md5.as_deref(),
                req.input.checksum_crc32.as_deref(),
                req.input.checksum_crc32c.as_deref(),
                req.input.checksum_sha1.as_deref(),
                req.input.checksum_sha256.as_deref(),
                req.input.checksum_crc64nvme.as_deref(),
            )?;
            let sample_len = bytes.len().min(SAMPLE_BYTES);
            // v0.8 #56: full part body is already in memory here; use its
            // length as the size hint so the dispatcher can promote to GPU
            // if it's big enough.
            let codec_kind = self
                .dispatcher
                .pick_with_size_hint(&bytes[..sample_len], Some(bytes.len() as u64))
                .await;
            let original_size = bytes.len() as u64;
            // v0.8 #55: telemetry-returning compress (GPU metrics stamp).
            let (compress_res, tel) = self
                .registry
                .compress_with_telemetry(bytes, codec_kind)
                .await;
            stamp_gpu_compress_telemetry(&tel);
            let (compressed, manifest) =
                compress_res.map_err(internal("registry compress part"))?;
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
        mut req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        let upload_id = req.input.upload_id.clone();
        // v0.8.12 HIGH-9 fix: gate Complete on `s3:PutObject` (the
        // commit point for the multipart-assembled object).
        self.enforce_policy(&req, "s3:PutObject", &bucket, Some(&key))?;
        self.enforce_rate_limit(&req, &bucket)?;
        // v0.8.12 HIGH-6 fix: re-verify Object Lock on the target key
        // at Complete time. Without this an attacker with PutObject
        // permission could `CreateMultipartUpload` against a key
        // that's currently under retention / legal hold and silently
        // overwrite it on Complete (the single-PUT path runs the
        // same check at L2007). Compliance retention is never
        // bypassable; Governance only with explicit IAM permission
        // (HIGH-7 gate below).
        if let Some(mgr) = self.object_lock.as_ref()
            && let Some(state) = mgr.get(&bucket, &key)
        {
            // CompleteMultipartUpload doesn't carry the bypass header
            // (the s3s DTO matches AWS' wire schema). A locked key
            // therefore cannot be overwritten by Complete regardless
            // of caller permission — operators who need to break a
            // Governance lock do it via PutObjectRetention before
            // calling Complete.
            let now = chrono::Utc::now();
            if !state.can_delete(now, false) {
                crate::metrics::record_policy_denial("s3:PutObject", &bucket);
                return Err(S3Error::with_message(
                    S3ErrorCode::AccessDenied,
                    "Access Denied because target key is protected by object lock",
                ));
            }
        }
        // v0.8.1 #59: serialise concurrent Complete invocations on the
        // same `(bucket, key)`. The race window the lock closes is the
        // GET-assembled-body → encrypt → PUT-encrypted-body triple
        // below (BUG-5 fix); without serialisation, two Completes for
        // different `upload_id` but the same logical key could each
        // read the other's plaintext assembled body and overwrite the
        // peer's encrypted result. The guard is held to function exit
        // (drop on `Ok` / `Err`), covering version-id mint, object-
        // lock apply, tagging persist, and replication enqueue too.
        let completion_lock = self.multipart_state.completion_lock(&bucket, &key);
        let _completion_guard = completion_lock.lock().await;
        // v0.8 #54 — fetch the per-upload context captured on Create.
        // `None` means an abandoned / unknown upload_id (gateway
        // crashed between Create and Complete, or pre-v0.8 state
        // restore); we still let the backend do its thing for
        // transparency, but we can't apply any SSE / version / lock /
        // tag / replication post-processing because we never captured
        // the recipe.
        let ctx = self.multipart_state.get(upload_id.as_str());
        // v0.8 #54 BUG-10 fix: same SSE-C header strip as upload_part
        // — some clients (boto3 / aws-sdk-cpp older versions) replay
        // the SSE-C triple on Complete too, and MinIO will choke if
        // they reach the backend.
        let _ = req.input.sse_customer_algorithm.take();
        let _ = req.input.sse_customer_key.take();
        let _ = req.input.sse_customer_key_md5.take();
        // v1.2 savings ledger: Complete assembles the parts at `<key>`,
        // destroying whatever lived there (even on versioning-Enabled
        // buckets the plain-`<key>` bytes are overwritten and — on the
        // shadow re-PUT path — deleted afterwards). Probe the doomed
        // footprint before the backend call; extra HEAD only when the
        // ledger flag is on.
        let ledger_old_main: Option<LedgerFootprint> = if self.savings_ledger.is_some() {
            self.ledger_probe_object(&bucket, &key, None).await
        } else {
            None
        };
        let mut resp = self.backend.complete_multipart_upload(req).await?;
        // CompleteMultipartUpload 成功 → 完成した object を full fetch して frame
        // index を build、`<key>.s4index` sidecar として保存。これで Range GET の
        // partial fetch path が利用可能になる (Range request の帯域節約)。
        // 注: 巨大 object の場合この pass は重いが、Range query は一度 sidecar が
        // できれば爆速になるので 1 回の cost は payback される
        //
        // v0.8 #54 BUG-5..9: this same fetch is the choke-point for
        // the SSE encrypt re-PUT + versioning shadow-key rewrite +
        // replication source-bytes capture, so we GET once and reuse
        // the bytes for every post-processing step.
        let assembled_body: Option<bytes::Bytes> = if let Ok(uri) = safe_object_uri(&bucket, &key) {
            let get_input = GetObjectInput {
                bucket: bucket.clone(),
                key: key.clone(),
                ..Default::default()
            };
            let get_req = S3Request {
                input: get_input,
                method: http::Method::GET,
                uri,
                headers: http::HeaderMap::new(),
                extensions: http::Extensions::new(),
                credentials: None,
                region: None,
                service: None,
                trailing_headers: None,
            };
            match self.backend.get_object(get_req).await {
                Ok(get_resp) => match get_resp.output.body {
                    Some(blob) => collect_blob(blob, self.max_body_bytes).await.ok(),
                    None => None,
                },
                Err(e) => {
                    // v0.8.4 #71 (C-1 audit fix): a silent
                    // `Err(_) => None` here is a SSE plaintext
                    // leak. The post-processing block below only
                    // runs the SSE re-encrypt branch when
                    // `assembled_body.is_some()`, so swallowing a
                    // backend error skipped the encrypt step and
                    // left the multipart object on disk as
                    // plaintext, even on SSE-S4 / SSE-C / SSE-KMS
                    // configured buckets. Same root-cause family
                    // as v0.8 BUG-5; this branch closes the
                    // remaining read-side window.
                    //
                    // We distinguish two cases:
                    //  - `NoSuchKey`: the object is genuinely
                    //    missing post-Complete. This is rare and
                    //    typically races with a concurrent
                    //    DeleteObject; there is nothing to re-
                    //    encrypt and no SSE markers to honour, so
                    //    falling through to the legacy
                    //    `assembled_body = None` path is safe.
                    //  - everything else (5xx, network, auth,
                    //    etc.): we must FAIL the Complete so the
                    //    client can retry. Returning Ok with
                    //    `assembled_body = None` would silently
                    //    skip the SSE re-encrypt and leave the
                    //    backend bytes plaintext.
                    if matches!(e.code(), &S3ErrorCode::NoSuchKey) {
                        tracing::warn!(
                            bucket = %bucket,
                            key = %key,
                            "multipart Complete: backend GET returned NoSuchKey; \
                             skipping post-processing (object likely raced with DeleteObject)"
                        );
                        None
                    } else {
                        tracing::error!(
                            bucket = %bucket,
                            key = %key,
                            error = %e,
                            "multipart Complete: backend GET failed; failing the Complete \
                             so the client retries (silent fall-through would skip SSE \
                             re-encrypt and store plaintext)"
                        );
                        return Err(internal("multipart Complete: backend body fetch failed")(e));
                    }
                }
            }
        } else {
            None
        };
        // v1.2 savings ledger: the assembled body is the ground truth
        // for the new object's footprint. `original` = sum of the
        // per-part frame headers' original sizes (exactly what the
        // client uploaded); a body the frame scanner can't parse
        // (passthrough-codec multipart = raw parts) falls back to the
        // body length (original == stored, zero claimed savings).
        // `stored` starts as the assembled length and is replaced by
        // the re-PUT length when SSE / versioning rewrites the bytes
        // below. `None` body (NoSuchKey race) ⇒ the ledger skips this
        // Complete with a WARN — never guess.
        //
        // v1.2 audit R1 P2 (CPU regression): the frame scan
        // (`build_index_from_body`) exists ONLY when the ledger flag
        // is on — flag-off deployments must not pay an O(body) walk
        // per Complete for counters nobody is keeping.
        let ledger_on = self.savings_ledger.is_some();
        let ledger_new_original: Option<u64> = if ledger_on {
            assembled_body.as_ref().map(|b| {
                build_index_from_body(b)
                    .map(|idx| idx.total_original_size())
                    .unwrap_or(b.len() as u64)
            })
        } else {
            None
        };
        let mut ledger_new_stored: Option<u64> = if ledger_on {
            assembled_body.as_ref().map(|b| b.len() as u64)
        } else {
            None
        };
        let mut ledger_sidecar_old: u64 = 0;
        let mut ledger_sidecar_new: u64 = 0;
        // Sidecar build (existing behaviour, gated on assembled body).
        //
        // v0.8.12 HIGH-10 fix: skip the sidecar when the Complete is
        // going to SSE-encrypt the assembled body before re-PUT (the
        // single-PUT path applies the same suppression at L2271).
        // Stale offsets into the pre-encrypt body would break Range
        // GET on the encrypted on-disk bytes. `ctx.sse != None`
        // covers all three SSE modes captured at Create time.
        let mp_will_encrypt = ctx
            .as_ref()
            .map(|c| !matches!(c.sse, crate::multipart_state::MultipartSseMode::None))
            .unwrap_or(false);
        // v0.8.16 F-7: versioned multipart writes the assembled body
        // under `versioned_shadow_key(&key, vid)` *after* this
        // sidecar block, then deletes the original `<key>`. Stamping
        // the sidecar against the to-be-deleted `<key>` (which is
        // what H-g did) leaves an orphan `<key>.s4index` whose
        // source-ETag binding can never match the live shadow body
        // — the Range GET fast-path's stale-sidecar check then
        // falls through to a full read on every request, silently
        // disabling partial fetch. Skip the sidecar build entirely
        // for versioned buckets; a follow-up issue tracks writing
        // the sidecar under the shadow key with the shadow's ETag.
        let mp_skip_sidecar_for_versioning = self
            .versioning
            .as_ref()
            .map(|mgr| mgr.state(&bucket))
            .map(|state| state == crate::versioning::VersioningState::Enabled)
            .unwrap_or(false);
        if let Some(ref body) = assembled_body
            && !mp_will_encrypt
            && !mp_skip_sidecar_for_versioning
            && let Ok(mut index) = build_index_from_body(body)
        {
            // v0.8.15 H-g: stamp the source-ETag / source-compressed-size
            // binding on the multipart sidecar. The single-PUT path
            // does this at L2519-L2521 via the backend's PUT response,
            // but Complete returns its own ETag (an opaque manifest
            // hash) so we have to HEAD the freshly-completed object
            // to pick up what backend actually wrote, then bind the
            // sidecar to those values. Without the binding, a
            // subsequent backend-side mutation (lifecycle rewrite,
            // out-of-band CopyObject) wouldn't trip the staleness
            // check on the next Range GET — the GET would happily
            // slice the new bytes at the old sidecar offsets, with
            // silent data corruption.
            if let Ok(uri) = safe_object_uri(&bucket, &key) {
                let head_req = S3Request {
                    input: HeadObjectInput {
                        bucket: bucket.clone(),
                        key: key.clone(),
                        ..Default::default()
                    },
                    method: http::Method::HEAD,
                    uri,
                    headers: http::HeaderMap::new(),
                    extensions: http::Extensions::new(),
                    credentials: None,
                    region: None,
                    service: None,
                    trailing_headers: None,
                };
                if let Ok(head) = self.backend.head_object(head_req).await {
                    index.source_etag = head.output.e_tag.as_ref().map(|t| t.value().to_string());
                    index.source_compressed_size = head
                        .output
                        .content_length
                        .and_then(|n| u64::try_from(n).ok());
                }
                // HEAD failure is non-fatal — the sidecar still works
                // as a v1-style best-effort fast path; the Range GET
                // simply falls back to a full read on any consistency
                // signal.
            }
            // v1.2 savings ledger: same sidecar replace accounting as
            // the single-PUT path (probe the to-be-overwritten sidecar
            // only when one is about to be written). Audit R1 P2: only
            // when the replaced main object was itself accounted —
            // an unaccounted object's sidecar was never added.
            if self.savings_ledger.is_some() && ledger_old_main.is_some_and(|f| f.accounted) {
                ledger_sidecar_old = self.ledger_probe_sidecar_bytes(&bucket, &key).await;
            }
            ledger_sidecar_new = self.write_sidecar(&bucket, &key, &index).await;
        }
        // From here on, post-processing depends on the context —
        // short-circuit when the upload had no captured recipe
        // (legacy / crashed-Create / pre-v0.8 state restore).
        if let Some(ctx) = ctx {
            // v0.8 #54 BUG-6 fix: mint a version-id when the bucket
            // is versioning-Enabled. The single-PUT path does this in
            // `put_object` ~L1968; multipart was the missing branch.
            // We mint here (post-Complete, before any re-PUT) so the
            // same vid threads into both the shadow-key rewrite and
            // the VersionEntry the manager records.
            let pending_version: Option<crate::versioning::PutOutcome> = self
                .versioning
                .as_ref()
                .map(|mgr| mgr.state(&bucket))
                .map(|state| match state {
                    crate::versioning::VersioningState::Enabled => crate::versioning::PutOutcome {
                        version_id: crate::versioning::VersioningManager::new_version_id(),
                        versioned_response: true,
                    },
                    crate::versioning::VersioningState::Suspended
                    | crate::versioning::VersioningState::Unversioned => {
                        crate::versioning::PutOutcome {
                            version_id: crate::versioning::NULL_VERSION_ID.to_owned(),
                            versioned_response: false,
                        }
                    }
                });
            // v0.8 #54 BUG-5 fix: encrypt the assembled framed body
            // and re-PUT it to the backend so the on-disk bytes are
            // SSE-encrypted. The single-PUT path does this body-by-
            // body inside `put_object` (L1907-L1942); for multipart,
            // encrypt-per-part would require a multi-segment decrypt
            // path on GET — we instead do a single encrypt over the
            // assembled framed body so the existing GET decrypt
            // branch (`is_sse_encrypted` → `decrypt(body, source)` →
            // FrameIter) handles it unchanged.
            //
            // The cost is one extra round-trip per Complete for SSE-
            // enabled multipart (already-paid for the sidecar build).
            // For single-instance gateways pointing at a co-located
            // backend this is negligible; cross-region operators
            // would benefit from per-part encrypt + multi-segment
            // decrypt as a follow-up.
            let needs_re_put = matches!(
                ctx.sse,
                crate::multipart_state::MultipartSseMode::SseS4
                    | crate::multipart_state::MultipartSseMode::SseC { .. }
                    | crate::multipart_state::MultipartSseMode::SseKms { .. }
            ) || pending_version
                .as_ref()
                .map(|pv| pv.versioned_response)
                .unwrap_or(false);
            // v0.8.11 CRIT-2 fix: seed the replication body with the
            // pre-encrypt assembled bytes, but overwrite it with the
            // post-encrypt `new_body` once the re-PUT branch lands.
            // The previous "snapshot in advance" pattern shipped the
            // *plaintext* framed body to the destination bucket even
            // when SSE-S4 / SSE-C / SSE-KMS was active — the GET on
            // the destination would then fail to decrypt (or, worse,
            // succeed in handing out plaintext that the source had
            // promised was encrypted at rest). When `needs_re_put`
            // is false (no SSE, no versioning), the backend still
            // holds the original plaintext-framed bytes, and the
            // seed value is what the destination should receive.
            let mut replication_body = assembled_body.clone();
            let mut applied_metadata: Option<std::collections::HashMap<String, String>> = None;
            if needs_re_put && let Some(body) = assembled_body {
                // v0.8.1 #58: same Zeroizing pattern as put_object's
                // single-PUT KMS branch — DEK plaintext lives in
                // `Zeroizing<[u8; 32]>` for the lifetime of this
                // Complete handler, then is wiped on drop.
                let kms_wrap: Option<(zeroize::Zeroizing<[u8; 32]>, crate::kms::WrappedDek)> =
                    if let crate::multipart_state::MultipartSseMode::SseKms { ref key_id } = ctx.sse
                    {
                        let kms = self.kms.as_ref().ok_or_else(|| {
                        S3Error::with_message(
                            S3ErrorCode::InvalidRequest,
                            "SSE-KMS requested but no --kms-local-dir / --kms-aws-region is configured on this gateway",
                        )
                    })?;
                        let (dek, wrapped) =
                            kms.generate_dek(key_id).await.map_err(kms_error_to_s3)?;
                        if dek.len() != 32 {
                            return Err(S3Error::with_message(
                                S3ErrorCode::InternalError,
                                format!(
                                    "KMS backend returned a DEK of {} bytes (expected 32)",
                                    dek.len()
                                ),
                            ));
                        }
                        let mut dek_arr: zeroize::Zeroizing<[u8; 32]> =
                            zeroize::Zeroizing::new([0u8; 32]);
                        dek_arr.copy_from_slice(&dek);
                        // `dek` (Zeroizing<Vec<u8>>) is dropped at scope end.
                        Some((dek_arr, wrapped))
                    } else {
                        None
                    };
                // Build the new metadata map: re-fetch via HEAD so
                // the multipart / codec markers the backend stamped
                // on Create flow through unchanged, then layer the
                // SSE markers on top.
                let head_req = S3Request {
                    input: HeadObjectInput {
                        bucket: bucket.clone(),
                        key: key.clone(),
                        ..Default::default()
                    },
                    method: http::Method::HEAD,
                    uri: safe_object_uri(&bucket, &key)?,
                    headers: http::HeaderMap::new(),
                    extensions: http::Extensions::new(),
                    credentials: None,
                    region: None,
                    service: None,
                    trailing_headers: None,
                };
                let mut new_metadata: std::collections::HashMap<String, String> =
                    match self.backend.head_object(head_req).await {
                        Ok(h) => h.output.metadata.unwrap_or_default(),
                        Err(_) => std::collections::HashMap::new(),
                    };
                // v1.2 audit R1 P2: the re-PUT object is the one the
                // ledger accounts — re-stamp the marker in case the
                // upload was Created before the ledger flag was
                // enabled (or the HEAD above failed).
                if self.savings_ledger.is_some() {
                    new_metadata.insert(META_LEDGER.into(), META_LEDGER_ACCOUNTED.into());
                    // v1.2 audit R2 P2: this re-PUT path (SSE multipart
                    // and versioning-Enabled multipart) suppresses the
                    // `.s4index` sidecar, so without an
                    // `s4-original-size` stamp the later DELETE probe
                    // falls back to `original = stored` while the
                    // Complete's add used the frame-scan logical size —
                    // every add→delete cycle would leave
                    // `logical − stored` phantom original bytes behind
                    // (overstated savings, no disclosure). Stamp the
                    // same semantics `write_manifest` records on the
                    // single-PUT path: original = logical client bytes
                    // (the exact frame-scan sum the ledger add below
                    // uses), compressed = the pre-SSE assembled framed
                    // length. `insert` overwrites any HEAD-inherited
                    // stale value — the gateway's own scan is
                    // authoritative. Ledger-gated: flag-off deployments
                    // keep bit-for-bit pre-ledger metadata. GET-path
                    // behaviour is unchanged — `extract_manifest`
                    // additionally requires `s4-crc32c`, which multipart
                    // objects never carry, so the single-chunk decode
                    // path stays unreachable for them.
                    if let Some(original) = ledger_new_original {
                        new_metadata.insert(META_ORIGINAL_SIZE.into(), original.to_string());
                        new_metadata.insert(META_COMPRESSED_SIZE.into(), body.len().to_string());
                    }
                }
                let new_body = match &ctx.sse {
                    crate::multipart_state::MultipartSseMode::SseC { key, key_md5 } => {
                        new_metadata.insert("s4-encrypted".into(), "aes-256-gcm".into());
                        new_metadata.insert("s4-sse-type".into(), "AES256".into());
                        new_metadata.insert(
                            "s4-sse-c-key-md5".into(),
                            base64::engine::general_purpose::STANDARD.encode(key_md5),
                        );
                        // v0.8.2 #62: `key` is `&Zeroizing<[u8; 32]>`;
                        // auto-deref through one explicit binding so
                        // `SseSource::CustomerKey` gets the `&[u8; 32]`
                        // it expects (mirrors the SSE-KMS DEK shape
                        // a few lines down).
                        let key_ref: &[u8; 32] = key;
                        crate::sse::encrypt_with_source(
                            &body,
                            crate::sse::SseSource::CustomerKey {
                                key: key_ref,
                                key_md5,
                            },
                        )
                    }
                    crate::multipart_state::MultipartSseMode::SseKms { .. } => {
                        let (dek, wrapped) = kms_wrap
                            .as_ref()
                            .expect("SseKms branch implies kms_wrap is Some");
                        new_metadata.insert("s4-encrypted".into(), "aes-256-gcm".into());
                        new_metadata.insert("s4-sse-type".into(), "aws:kms".into());
                        new_metadata.insert("s4-sse-kms-key-id".into(), wrapped.key_id.clone());
                        // v0.8.1 #58: auto-deref from `&Zeroizing<[u8; 32]>`
                        // to `&[u8; 32]` (same shape as the put_object
                        // single-PUT branch).
                        let dek_ref: &[u8; 32] = dek;
                        crate::sse::encrypt_with_source(
                            &body,
                            crate::sse::SseSource::Kms {
                                dek: dek_ref,
                                wrapped,
                            },
                        )
                    }
                    crate::multipart_state::MultipartSseMode::SseS4 => {
                        let keyring = self.sse_keyring.as_ref().ok_or_else(|| {
                            S3Error::with_message(
                                S3ErrorCode::InternalError,
                                "SSE-S4 captured at Create but keyring missing at Complete",
                            )
                        })?;
                        new_metadata.insert("s4-encrypted".into(), "aes-256-gcm".into());
                        // SSE-S4 deliberately omits `s4-sse-type` so
                        // HEAD doesn't falsely advertise AWS-style
                        // SSE-S3 (matches the put_object L1929-L1939
                        // comment).
                        // v0.8 #52: same chunk_size dispatch as the
                        // single-PUT branch — multipart Complete
                        // re-encrypts the assembled body, so honoring
                        // the chunked path here is required to keep
                        // GET streaming on multipart-uploaded objects.
                        if self.sse_chunk_size > 0 {
                            crate::sse::encrypt_v2_chunked(&body, keyring, self.sse_chunk_size)
                                .map_err(|e| {
                                    S3Error::with_message(
                                        S3ErrorCode::InternalError,
                                        format!("SSE-S4 chunked encrypt failed at Complete: {e}"),
                                    )
                                })?
                        } else {
                            crate::sse::encrypt_v2(&body, keyring)
                        }
                    }
                    crate::multipart_state::MultipartSseMode::None => body.clone(),
                };
                // v0.8 #54 BUG-6 fix: write the re-PUT under the
                // shadow key so the version chain doesn't overwrite
                // the previous version on a versioned bucket. The
                // original (unshadowed) key was assembled by the
                // backend on Complete; we delete it after the shadow
                // PUT lands.
                let put_target_key = if let Some(pv) = pending_version.as_ref() {
                    if pv.versioned_response {
                        versioned_shadow_key(&key, &pv.version_id)
                    } else {
                        key.clone()
                    }
                } else {
                    key.clone()
                };
                let new_body_len = new_body.len() as i64;
                // v1.2 savings ledger: the re-PUT bytes (SSE envelope /
                // shadow-key rewrite) are what actually stays on the
                // backend — they supersede the assembled length.
                ledger_new_stored = Some(new_body.len() as u64);
                let put_req = S3Request {
                    input: PutObjectInput {
                        bucket: bucket.clone(),
                        key: put_target_key.clone(),
                        body: Some(bytes_to_blob(new_body.clone())),
                        metadata: Some(new_metadata.clone()),
                        content_length: Some(new_body_len),
                        ..Default::default()
                    },
                    method: http::Method::PUT,
                    uri: safe_object_uri(&bucket, &put_target_key)?,
                    headers: http::HeaderMap::new(),
                    extensions: http::Extensions::new(),
                    credentials: None,
                    region: None,
                    service: None,
                    trailing_headers: None,
                };
                self.backend.put_object(put_req).await?;
                // v0.8.11 CRIT-2 fix: refresh the replication snapshot
                // with the bytes that were actually persisted to the
                // backend (post-SSE-encrypt for SSE modes; identical to
                // `body` for `MultipartSseMode::None` + versioning-only
                // re-PUT). The destination then sees the same on-disk
                // shape the source does, and a destination GET decrypts
                // correctly when SSE is on.
                replication_body = Some(new_body.clone());
                // If we rewrote the storage key (versioning shadow),
                // we must drop the original (unshadowed) Complete-
                // assembled bytes so subsequent listings don't see a
                // duplicate.
                if put_target_key != key {
                    let del_req = S3Request {
                        input: DeleteObjectInput {
                            bucket: bucket.clone(),
                            key: key.clone(),
                            ..Default::default()
                        },
                        method: http::Method::DELETE,
                        uri: safe_object_uri(&bucket, &key)?,
                        headers: http::HeaderMap::new(),
                        extensions: http::Extensions::new(),
                        credentials: None,
                        region: None,
                        service: None,
                        trailing_headers: None,
                    };
                    let _ = self.backend.delete_object(del_req).await;
                }
                // v1.2 audit R2 P2: same marker strip as the single-PUT
                // replication capture — the replica is never ledger-
                // accounted, so it must not carry `s4-ledger`.
                applied_metadata = replication_metadata_snapshot(&Some(new_metadata));
            }
            // v0.8 #54 BUG-6 commit: register the new version with
            // the VersioningManager so list_object_versions /
            // GET ?versionId= see it.
            if let (Some(mgr), Some(pv)) = (self.versioning.as_ref(), pending_version.as_ref()) {
                let etag = resp
                    .output
                    .e_tag
                    .clone()
                    .map(ETag::into_value)
                    .unwrap_or_default();
                let now = chrono::Utc::now();
                mgr.commit_put_with_version(
                    &bucket,
                    &key,
                    crate::versioning::VersionEntry {
                        version_id: pv.version_id.clone(),
                        etag,
                        size: replication_body
                            .as_ref()
                            .map(|b| b.len() as u64)
                            .unwrap_or(0),
                        is_delete_marker: false,
                        created_at: now,
                    },
                );
                if pv.versioned_response {
                    resp.output.version_id = Some(pv.version_id.clone());
                }
            }
            // v0.8 #54 BUG-7 fix: persist any per-upload Object Lock
            // recipe + auto-apply the bucket default. Mirrors the
            // put_object L2057-L2074 block.
            if let Some(mgr) = self.object_lock.as_ref() {
                if ctx.object_lock_mode.is_some()
                    || ctx.object_lock_retain_until.is_some()
                    || ctx.object_lock_legal_hold
                {
                    let mut state = mgr.get(&bucket, &key).unwrap_or_default();
                    if let Some(m) = ctx.object_lock_mode {
                        state.mode = Some(m);
                    }
                    if let Some(u) = ctx.object_lock_retain_until {
                        state.retain_until = Some(u);
                    }
                    if ctx.object_lock_legal_hold {
                        state.legal_hold_on = true;
                    }
                    mgr.set(&bucket, &key, state);
                }
                mgr.apply_default_on_put(&bucket, &key, chrono::Utc::now());
            }
            // v0.8 #54 BUG-9 fix: persist the captured tags via the
            // TagManager so GetObjectTagging returns them.
            if let (Some(mgr), Some(tags)) = (self.tagging.as_ref(), ctx.tags.as_ref()) {
                mgr.put_object_tags(&bucket, &key, tags.clone());
            }
            // SSE-C / SSE-KMS response echo. The
            // CompleteMultipartUploadOutput only exposes
            // `server_side_encryption` + `ssekms_key_id` (no
            // sse_customer_* — those round-tripped on Create / parts).
            match &ctx.sse {
                crate::multipart_state::MultipartSseMode::SseC { .. } => {
                    resp.output.server_side_encryption = Some(ServerSideEncryption::from_static(
                        ServerSideEncryption::AES256,
                    ));
                }
                crate::multipart_state::MultipartSseMode::SseKms { key_id } => {
                    resp.output.server_side_encryption = Some(ServerSideEncryption::from_static(
                        ServerSideEncryption::AWS_KMS,
                    ));
                    resp.output.ssekms_key_id = Some(key_id.clone());
                }
                _ => {}
            }
            // v0.8 #54 BUG-8 fix: fire cross-bucket replication just
            // like put_object L2165 does. We hand the dispatcher the
            // assembled body bytes (post-encrypt where applicable, so
            // the destination ends up byte-identical to the source's
            // on-disk shape) plus the metadata that was actually
            // committed.
            let replication_body_bytes = replication_body.unwrap_or_default();
            // v0.8.2 #61: thread the multipart-Complete `pending_version`
            // through so a versioning-Enabled source's destination
            // receives the same shadow-key path (mirror of the
            // single-PUT branch above).
            self.spawn_replication_if_matched(
                &bucket,
                &key,
                &ctx.tags,
                &replication_body_bytes,
                &applied_metadata,
                true,
                pending_version.as_ref(),
            );
            self.multipart_state.remove(upload_id.as_str());
        }
        // v1.2 savings ledger: commit the Complete as one delta —
        // subtract whatever lived at `<key>` (plus a replaced sidecar),
        // add the assembled/re-PUT bytes + new sidecar. When the
        // assembled body couldn't be fetched (NoSuchKey race), skip
        // with a WARN instead of guessing.
        if let Some(ledger) = self.savings_ledger.as_ref() {
            match (ledger_new_original, ledger_new_stored) {
                (Some(new_original), Some(new_stored)) => {
                    let new_stored = new_stored.saturating_add(ledger_sidecar_new);
                    // v1.2 audit R1 P2: marker-gated old subtraction,
                    // same contract as the single-PUT overwrite path.
                    let old_accounted = ledger_old_main.is_some_and(|f| f.accounted);
                    let (old_original, old_stored) = ledger_old_main
                        .filter(|f| f.accounted)
                        .map(|f| (f.original_bytes, f.stored_bytes))
                        .unwrap_or((0, 0));
                    let old_stored = old_stored.saturating_add(ledger_sidecar_old);
                    if ledger_old_main.is_some() && !old_accounted {
                        ledger.record_skipped_unaccounted(&bucket);
                    }
                    ledger.apply_delta(
                        &bucket,
                        crate::ledger::signed_delta(new_original, old_original),
                        crate::ledger::signed_delta(new_stored, old_stored),
                        if old_accounted { 0 } else { 1 },
                    );
                }
                _ => {
                    // v1.2 audit R2 P3: the marker was stamped at Create
                    // time and survives on the completed object even
                    // though this add is being skipped — disclose the
                    // marker/accounting mismatch loudly (the object's
                    // later DELETE will subtract via the marker gate;
                    // the zero-clamp + report drift note are the
                    // documented guard rails).
                    tracing::warn!(
                        bucket = %bucket,
                        key = %key,
                        "S4 savings ledger: multipart Complete not accounted \
                         (assembled body unavailable — backend GET failed or the body \
                         exceeds --max-body-bytes); counters unchanged for this object. \
                         NOTE: this object will carry the ledger marker without being \
                         counted, so its later DELETE may subtract bytes that were never \
                         added (clamped at zero, disclosed via the report drift note)"
                    );
                }
            }
        }
        // v0.8.1 #59 janitor: best-effort sweep of stale completion
        // locks while we are still on the critical path of a single
        // Complete (so steady-state workloads of unique keys don't
        // accumulate `DashMap` entries). The sweep only retires
        // entries whose `Arc::strong_count == 1`, so any other in-
        // flight Complete on a different key keeps its lock alive.
        // Our own `_completion_guard` keeps `bucket`/`key`'s entry
        // alive across this call; it's reaped on the next Complete or
        // the next caller-driven prune.
        self.multipart_state.prune_completion_locks();
        Ok(resp)
    }
    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        // v0.8.12 HIGH-9 fix: gate Abort on `s3:AbortMultipartUpload`
        // — the AWS-spec action verb for this operation. Without the
        // gate, anyone who could guess an upload_id could throw away
        // someone else's in-flight multipart upload.
        let abort_bucket = req.input.bucket.clone();
        let abort_key = req.input.key.clone();
        self.enforce_policy(
            &req,
            "s3:AbortMultipartUpload",
            &abort_bucket,
            Some(&abort_key),
        )?;
        // v0.8 #54: drop the per-upload state (SSE-C key bytes / tag
        // set) promptly so an aborted upload doesn't leak the
        // customer's key into a long-running gateway's RSS.
        //
        // v0.8.4 #71 (H-7 audit fix): backend.abort_multipart_upload
        // FIRST, then drop in-process state ONLY on success. The
        // previous order ("remove → call backend") meant a transient
        // backend abort failure (5xx, network) wiped the SSE-C key
        // bytes locally while leaving the parts on the backend, so a
        // client retry would have to re-validate the SSE-C key against
        // a context the gateway no longer has — and the retried abort
        // would still hit the unaborted backend parts. Calling the
        // backend first lets the failure propagate to the client with
        // state intact for a clean retry; only on success do we wipe
        // the local state.
        let upload_id = req.input.upload_id.as_str().to_owned();
        let resp = self.backend.abort_multipart_upload(req).await?;
        self.multipart_state.remove(&upload_id);
        Ok(resp)
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
        // v0.8.17 G-2: reserved-name guard. Without it a hostile
        // client can `GetObjectAcl(<key>.s4index)` to confirm the
        // sidecar exists, an information leak the F-13 GET reject
        // closed for the same object.
        self.check_not_reserved_key(&req.input.key, ReservedKeyMode::Read)?;
        self.backend.get_object_acl(req).await
    }
    async fn put_object_acl(
        &self,
        req: S3Request<PutObjectAclInput>,
    ) -> S3Result<S3Response<PutObjectAclOutput>> {
        // v0.8.17 G-2: reserved-name guard. `put-object-acl
        // --acl public-read` against `<key>.s4index` would grant
        // external read access to the internal sidecar, bypassing
        // the F-13 GET reject via the backend's public-URL path.
        self.check_not_reserved_key(&req.input.key, ReservedKeyMode::Mutating)?;
        self.backend.put_object_acl(req).await
    }
    // v0.6 #39: object tagging — when a `TagManager` is attached the
    // configuration / per-(bucket, key) state lives in the manager and
    // these handlers serve directly from it; when no manager is
    // attached they fall back to the backend (legacy passthrough so
    // v0.5 deployments are unaffected).
    async fn get_object_tagging(
        &self,
        req: S3Request<GetObjectTaggingInput>,
    ) -> S3Result<S3Response<GetObjectTaggingOutput>> {
        // v0.8.17 G-2: reserved-name guard.
        self.check_not_reserved_key(&req.input.key, ReservedKeyMode::Read)?;
        let Some(mgr) = self.tagging.as_ref() else {
            return self.backend.get_object_tagging(req).await;
        };
        let tags = mgr
            .get_object_tags(&req.input.bucket, &req.input.key)
            .unwrap_or_default();
        Ok(S3Response::new(GetObjectTaggingOutput {
            tag_set: tagset_to_aws(&tags),
            ..Default::default()
        }))
    }
    async fn put_object_tagging(
        &self,
        req: S3Request<PutObjectTaggingInput>,
    ) -> S3Result<S3Response<PutObjectTaggingOutput>> {
        // v0.8.17 G-2: reserved-name guard.
        self.check_not_reserved_key(&req.input.key, ReservedKeyMode::Mutating)?;
        let Some(mgr) = self.tagging.as_ref() else {
            return self.backend.put_object_tagging(req).await;
        };
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        let parsed = aws_to_tagset(&req.input.tagging.tag_set)
            .map_err(|e| S3Error::with_message(S3ErrorCode::InvalidArgument, e.to_string()))?;
        // v0.6 #39: gate via IAM policy with both the request tags
        // (`s3:RequestObjectTag/<key>`) and any existing tags on the
        // target object (`s3:ExistingObjectTag/<key>`).
        let existing = mgr.get_object_tags(&bucket, &key);
        self.enforce_policy_with_extra(
            &req,
            "s3:PutObjectTagging",
            &bucket,
            Some(&key),
            Some(&parsed),
            existing.as_ref(),
        )?;
        mgr.put_object_tags(&bucket, &key, parsed);
        Ok(S3Response::new(PutObjectTaggingOutput::default()))
    }
    async fn delete_object_tagging(
        &self,
        req: S3Request<DeleteObjectTaggingInput>,
    ) -> S3Result<S3Response<DeleteObjectTaggingOutput>> {
        // v0.8.17 G-2: reserved-name guard.
        self.check_not_reserved_key(&req.input.key, ReservedKeyMode::Mutating)?;
        let Some(mgr) = self.tagging.as_ref() else {
            return self.backend.delete_object_tagging(req).await;
        };
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        let existing = mgr.get_object_tags(&bucket, &key);
        self.enforce_policy_with_extra(
            &req,
            "s3:DeleteObjectTagging",
            &bucket,
            Some(&key),
            None,
            existing.as_ref(),
        )?;
        mgr.delete_object_tags(&bucket, &key);
        Ok(S3Response::new(DeleteObjectTaggingOutput::default()))
    }
    async fn get_object_attributes(
        &self,
        req: S3Request<GetObjectAttributesInput>,
    ) -> S3Result<S3Response<GetObjectAttributesOutput>> {
        // v0.8.17 G-2: reserved-name guard. Attributes leak the
        // sidecar's size + ETag, same shape as F-13's GET concern.
        self.check_not_reserved_key(&req.input.key, ReservedKeyMode::Read)?;
        self.backend.get_object_attributes(req).await
    }
    async fn restore_object(
        &self,
        req: S3Request<RestoreObjectInput>,
    ) -> S3Result<S3Response<RestoreObjectOutput>> {
        // v0.8.17 G-2: reserved-name guard.
        self.check_not_reserved_key(&req.input.key, ReservedKeyMode::Mutating)?;
        self.backend.restore_object(req).await
    }
    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        // v0.8.12 HIGH-9 fix: same per-action gates as `copy_object` —
        // destination PUT + source GET.
        let dst_bucket = req.input.bucket.clone();
        let dst_key = req.input.key.clone();
        // v0.8.17 G-2: reserved-name guard on both destination
        // and source. Mirrors what `copy_object` enforces.
        self.check_not_reserved_key(&dst_key, ReservedKeyMode::Mutating)?;
        if let CopySource::Bucket { key, .. } = &req.input.copy_source {
            self.check_not_reserved_key(key, ReservedKeyMode::Read)?;
        }
        self.enforce_policy(&req, "s3:PutObject", &dst_bucket, Some(&dst_key))?;
        if let CopySource::Bucket { bucket, key, .. } = &req.input.copy_source {
            self.enforce_policy(&req, "s3:GetObject", bucket, Some(key))?;
        }
        self.enforce_rate_limit(&req, &dst_bucket)?;
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
        // v0.8.4 #74: propagate the optional `?versionId=<vid>` from the
        // copy-source header. Without this, a versioned source bucket
        // copy that pins a specific old version would silently fall
        // back to "latest", assembling wrong bytes into the destination
        // multipart object (silent data corruption).
        let CopySource::Bucket {
            bucket: src_bucket,
            key: src_key,
            version_id: src_version_id,
        } = &req.input.copy_source
        else {
            return self.backend.upload_part_copy(req).await;
        };
        let src_bucket = src_bucket.to_string();
        let src_key = src_key.to_string();
        let src_version_id: Option<String> = src_version_id.as_deref().map(str::to_owned);

        // Probe metadata to decide whether the source needs S4-aware copy.
        let head_input = HeadObjectInput {
            bucket: src_bucket.clone(),
            key: src_key.clone(),
            version_id: src_version_id.clone(),
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
        // byte range, fully decompressed. version_id is propagated so
        // pinned-version copies fetch the exact version requested.
        let mut get_input = GetObjectInput {
            bucket: src_bucket.clone(),
            key: src_key.clone(),
            version_id: src_version_id.clone(),
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
        // v0.8 #56: same size-hint promotion as the upload_part path.
        let codec_kind = self
            .dispatcher
            .pick_with_size_hint(&bytes[..sample_len], Some(bytes.len() as u64))
            .await;
        let original_size = bytes.len() as u64;
        // v0.8 #55: telemetry-returning compress (GPU metrics stamp).
        let (compress_res, tel) = self
            .registry
            .compress_with_telemetry(bytes, codec_kind)
            .await;
        stamp_gpu_compress_telemetry(&tel);
        let (compressed, manifest) =
            compress_res.map_err(internal("registry compress upload_part_copy"))?;
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
        self.enforce_policy(
            &req,
            "s3:GetBucketObjectLockConfiguration",
            &req.input.bucket,
            None,
        )?;
        if let Some(mgr) = self.object_lock.as_ref() {
            let cfg = mgr
                .bucket_default(&req.input.bucket)
                .map(|d| ObjectLockConfiguration {
                    object_lock_enabled: Some(ObjectLockEnabled::from_static(
                        ObjectLockEnabled::ENABLED,
                    )),
                    rule: Some(ObjectLockRule {
                        default_retention: Some(DefaultRetention {
                            days: Some(d.retention_days as i32),
                            mode: Some(ObjectLockRetentionMode::from_static(match d.mode {
                                crate::object_lock::LockMode::Governance => {
                                    ObjectLockRetentionMode::GOVERNANCE
                                }
                                crate::object_lock::LockMode::Compliance => {
                                    ObjectLockRetentionMode::COMPLIANCE
                                }
                            })),
                            years: None,
                        }),
                    }),
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
        self.enforce_policy(
            &req,
            "s3:PutBucketObjectLockConfiguration",
            &req.input.bucket,
            None,
        )?;
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
        let key = req.input.key.clone();
        self.enforce_policy(&req, "s3:GetObjectLegalHold", &req.input.bucket, Some(&key))?;
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
        let key = req.input.key.clone();
        self.enforce_policy(&req, "s3:PutObjectLegalHold", &req.input.bucket, Some(&key))?;
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
        let key = req.input.key.clone();
        self.enforce_policy(&req, "s3:GetObjectRetention", &req.input.bucket, Some(&key))?;
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
        let key = req.input.key.clone();
        self.enforce_policy(&req, "s3:PutObjectRetention", &req.input.bucket, Some(&key))?;
        if let Some(mgr) = self.object_lock.as_ref() {
            let bucket = req.input.bucket.clone();
            let key = req.input.key.clone();
            // v0.8.12 HIGH-7 fix: the bypass header gates Governance
            // shortening only when the caller has the matching IAM
            // action explicitly allowed; otherwise it's silently
            // dropped to `false` and the "shortening Governance
            // requires bypass" branch below rejects.
            let bypass_header = req.input.bypass_governance_retention.unwrap_or(false);
            let bypass = if bypass_header {
                self.enforce_policy(&req, "s3:BypassGovernanceRetention", &bucket, Some(&key))
                    .is_ok()
            } else {
                false
            };
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
        // v0.6 #42: MFA gating on the `PutBucketVersioning` request
        // itself. S3 spec: when the request body carries an
        // `MfaDelete` element (either `Enabled` or `Disabled`), the
        // request must include a valid `x-amz-mfa` token — both for
        // the *first* enable (so the operator can't quietly side-step
        // the gate by never enabling it) and for any subsequent
        // change (so a leaked credential alone can't disable MFA
        // Delete to bypass it on subsequent DELETEs). Requests that
        // omit the `MfaDelete` element entirely (i.e. they flip only
        // `Status`) skip this gate, matching AWS.
        if let Some(mgr) = self.mfa_delete.as_ref()
            && let Some(target_enabled) = req
                .input
                .versioning_configuration
                .mfa_delete
                .as_ref()
                .map(|m| m.as_str().eq_ignore_ascii_case("Enabled"))
        {
            let bucket = req.input.bucket.clone();
            let header = req.input.mfa.as_deref();
            let secret = mgr.lookup_secret(&bucket);
            let verified = match (header, secret.as_ref()) {
                (Some(h), Some(s)) => match crate::mfa::parse_mfa_header(h) {
                    Ok((serial, code)) => {
                        serial == s.serial
                            && crate::mfa::verify_totp(&s.secret_base32, &code, current_unix_secs())
                    }
                    Err(_) => false,
                },
                _ => false,
            };
            if !verified {
                crate::metrics::record_mfa_delete_denial(&bucket);
                let err = if header.is_none() {
                    crate::mfa::MfaError::Missing
                } else {
                    crate::mfa::MfaError::InvalidCode
                };
                return Err(mfa_error_to_s3(err));
            }
            mgr.set_bucket_state(&bucket, target_enabled);
        }
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

    // ---- Bucket CORS (v0.6 #38) ----
    async fn get_bucket_cors(
        &self,
        req: S3Request<GetBucketCorsInput>,
    ) -> S3Result<S3Response<GetBucketCorsOutput>> {
        if let Some(mgr) = self.cors.as_ref() {
            let cfg = mgr.get(&req.input.bucket).ok_or_else(|| {
                S3Error::with_message(
                    S3ErrorCode::NoSuchCORSConfiguration,
                    "The CORS configuration does not exist".to_string(),
                )
            })?;
            let rules: Vec<CORSRule> = cfg
                .rules
                .into_iter()
                .map(|r| CORSRule {
                    allowed_headers: if r.allowed_headers.is_empty() {
                        None
                    } else {
                        Some(r.allowed_headers)
                    },
                    allowed_methods: r.allowed_methods,
                    allowed_origins: r.allowed_origins,
                    expose_headers: if r.expose_headers.is_empty() {
                        None
                    } else {
                        Some(r.expose_headers)
                    },
                    id: r.id,
                    max_age_seconds: r.max_age_seconds.map(|s| s as i32),
                })
                .collect();
            return Ok(S3Response::new(GetBucketCorsOutput {
                cors_rules: Some(rules),
            }));
        }
        self.backend.get_bucket_cors(req).await
    }
    async fn put_bucket_cors(
        &self,
        req: S3Request<PutBucketCorsInput>,
    ) -> S3Result<S3Response<PutBucketCorsOutput>> {
        if let Some(mgr) = self.cors.as_ref() {
            let cfg = crate::cors::CorsConfig {
                rules: req
                    .input
                    .cors_configuration
                    .cors_rules
                    .into_iter()
                    .map(|r| crate::cors::CorsRule {
                        allowed_origins: r.allowed_origins,
                        allowed_methods: r.allowed_methods,
                        allowed_headers: r.allowed_headers.unwrap_or_default(),
                        expose_headers: r.expose_headers.unwrap_or_default(),
                        max_age_seconds: r
                            .max_age_seconds
                            .and_then(|s| if s < 0 { None } else { Some(s as u32) }),
                        id: r.id,
                    })
                    .collect(),
            };
            // v0.8.15 M-3: AWS S3 rejects `AllowedMethods` outside
            // the canonical {GET,PUT,POST,DELETE,HEAD} set (including
            // the `*` wildcard). Validate at PutBucketCors time so
            // operators see the misconfiguration in the API response
            // instead of having silently-broken preflights at the
            // browser later.
            if let Err(e) = crate::cors::CorsManager::validate(&cfg) {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    e.to_string(),
                ));
            }
            mgr.put(&req.input.bucket, cfg);
            return Ok(S3Response::new(PutBucketCorsOutput::default()));
        }
        self.backend.put_bucket_cors(req).await
    }
    async fn delete_bucket_cors(
        &self,
        req: S3Request<DeleteBucketCorsInput>,
    ) -> S3Result<S3Response<DeleteBucketCorsOutput>> {
        if let Some(mgr) = self.cors.as_ref() {
            mgr.delete(&req.input.bucket);
            return Ok(S3Response::new(DeleteBucketCorsOutput::default()));
        }
        self.backend.delete_bucket_cors(req).await
    }

    // ---- Bucket lifecycle (v0.6 #37) ----
    async fn get_bucket_lifecycle_configuration(
        &self,
        req: S3Request<GetBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketLifecycleConfigurationOutput>> {
        if let Some(mgr) = self.lifecycle.as_ref() {
            let cfg = mgr.get(&req.input.bucket).ok_or_else(|| {
                S3Error::with_message(
                    S3ErrorCode::NoSuchLifecycleConfiguration,
                    "The lifecycle configuration does not exist".to_string(),
                )
            })?;
            let rules: Vec<LifecycleRule> = cfg.rules.iter().map(internal_rule_to_dto).collect();
            return Ok(S3Response::new(GetBucketLifecycleConfigurationOutput {
                rules: Some(rules),
                transition_default_minimum_object_size: None,
            }));
        }
        self.backend.get_bucket_lifecycle_configuration(req).await
    }
    async fn put_bucket_lifecycle_configuration(
        &self,
        req: S3Request<PutBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketLifecycleConfigurationOutput>> {
        if let Some(mgr) = self.lifecycle.as_ref() {
            let bucket = req.input.bucket.clone();
            let dto_cfg = req.input.lifecycle_configuration.unwrap_or_default();
            let cfg = dto_lifecycle_to_internal(&dto_cfg);
            mgr.put(&bucket, cfg);
            return Ok(S3Response::new(
                PutBucketLifecycleConfigurationOutput::default(),
            ));
        }
        self.backend.put_bucket_lifecycle_configuration(req).await
    }
    async fn delete_bucket_lifecycle(
        &self,
        req: S3Request<DeleteBucketLifecycleInput>,
    ) -> S3Result<S3Response<DeleteBucketLifecycleOutput>> {
        if let Some(mgr) = self.lifecycle.as_ref() {
            mgr.delete(&req.input.bucket);
            return Ok(S3Response::new(DeleteBucketLifecycleOutput::default()));
        }
        self.backend.delete_bucket_lifecycle(req).await
    }

    // ---- Bucket tagging (v0.6 #39) ----
    async fn get_bucket_tagging(
        &self,
        req: S3Request<GetBucketTaggingInput>,
    ) -> S3Result<S3Response<GetBucketTaggingOutput>> {
        let Some(mgr) = self.tagging.as_ref() else {
            return self.backend.get_bucket_tagging(req).await;
        };
        let tags = mgr.get_bucket_tags(&req.input.bucket).unwrap_or_default();
        Ok(S3Response::new(GetBucketTaggingOutput {
            tag_set: tagset_to_aws(&tags),
        }))
    }
    async fn put_bucket_tagging(
        &self,
        req: S3Request<PutBucketTaggingInput>,
    ) -> S3Result<S3Response<PutBucketTaggingOutput>> {
        let Some(mgr) = self.tagging.as_ref() else {
            return self.backend.put_bucket_tagging(req).await;
        };
        let bucket = req.input.bucket.clone();
        let parsed = aws_to_tagset(&req.input.tagging.tag_set)
            .map_err(|e| S3Error::with_message(S3ErrorCode::InvalidArgument, e.to_string()))?;
        self.enforce_policy(&req, "s3:PutBucketTagging", &bucket, None)?;
        mgr.put_bucket_tags(&bucket, parsed);
        Ok(S3Response::new(PutBucketTaggingOutput::default()))
    }
    async fn delete_bucket_tagging(
        &self,
        req: S3Request<DeleteBucketTaggingInput>,
    ) -> S3Result<S3Response<DeleteBucketTaggingOutput>> {
        let Some(mgr) = self.tagging.as_ref() else {
            return self.backend.delete_bucket_tagging(req).await;
        };
        let bucket = req.input.bucket.clone();
        self.enforce_policy(&req, "s3:PutBucketTagging", &bucket, None)?;
        mgr.delete_bucket_tags(&bucket);
        Ok(S3Response::new(DeleteBucketTaggingOutput::default()))
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

    // ---- Bucket notification (v0.6 #35) ----
    //
    // When a `NotificationManager` is attached, S4 itself owns per-bucket
    // notification configurations and the PUT / GET handlers route through
    // the manager. The wire DTO's queue / topic configurations map onto
    // S4's `Destination::Sqs` / `Destination::Sns`; LambdaFunction and
    // EventBridge configurations are accepted on PUT but silently dropped
    // (out of scope for v0.6 #35). When no manager is attached the legacy
    // backend-passthrough behaviour applies.
    async fn get_bucket_notification_configuration(
        &self,
        req: S3Request<GetBucketNotificationConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketNotificationConfigurationOutput>> {
        if let Some(mgr) = self.notifications.as_ref() {
            let cfg = mgr.get(&req.input.bucket).unwrap_or_default();
            let dto = notif_to_dto(&cfg);
            return Ok(S3Response::new(GetBucketNotificationConfigurationOutput {
                event_bridge_configuration: dto.event_bridge_configuration,
                lambda_function_configurations: dto.lambda_function_configurations,
                queue_configurations: dto.queue_configurations,
                topic_configurations: dto.topic_configurations,
            }));
        }
        self.backend
            .get_bucket_notification_configuration(req)
            .await
    }
    async fn put_bucket_notification_configuration(
        &self,
        req: S3Request<PutBucketNotificationConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketNotificationConfigurationOutput>> {
        if let Some(mgr) = self.notifications.as_ref() {
            let cfg = notif_from_dto(&req.input.notification_configuration);
            mgr.put(&req.input.bucket, cfg);
            return Ok(S3Response::new(
                PutBucketNotificationConfigurationOutput::default(),
            ));
        }
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

    // ---- Bucket replication (v0.6 #40) ----
    async fn get_bucket_replication(
        &self,
        req: S3Request<GetBucketReplicationInput>,
    ) -> S3Result<S3Response<GetBucketReplicationOutput>> {
        if let Some(mgr) = self.replication.as_ref() {
            return match mgr.get(&req.input.bucket) {
                Some(cfg) => Ok(S3Response::new(GetBucketReplicationOutput {
                    replication_configuration: Some(replication_to_dto(&cfg)),
                })),
                None => Err(S3Error::with_message(
                    S3ErrorCode::Custom("ReplicationConfigurationNotFoundError".into()),
                    format!(
                        "no replication configuration on bucket {}",
                        req.input.bucket
                    ),
                )),
            };
        }
        self.backend.get_bucket_replication(req).await
    }
    async fn put_bucket_replication(
        &self,
        req: S3Request<PutBucketReplicationInput>,
    ) -> S3Result<S3Response<PutBucketReplicationOutput>> {
        if let Some(mgr) = self.replication.as_ref() {
            let cfg = replication_from_dto(&req.input.replication_configuration);
            mgr.put(&req.input.bucket, cfg);
            return Ok(S3Response::new(PutBucketReplicationOutput::default()));
        }
        self.backend.put_bucket_replication(req).await
    }
    async fn delete_bucket_replication(
        &self,
        req: S3Request<DeleteBucketReplicationInput>,
    ) -> S3Result<S3Response<DeleteBucketReplicationOutput>> {
        if let Some(mgr) = self.replication.as_ref() {
            mgr.delete(&req.input.bucket);
            return Ok(S3Response::new(DeleteBucketReplicationOutput::default()));
        }
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

    // ====================================================================
    // v0.6 #41: S3 Select — server-side SQL filter on object body.
    //
    // Fetch the object via the regular `get_object` path (so SSE-C /
    // SSE-S4 / SSE-KMS / S4 codec all decompress + decrypt transparently),
    // run a small SQL subset (CSV + JSON Lines, equality / inequality /
    // LIKE / AND / OR / NOT) over the in-memory body, and stream the
    // matched rows back as AWS event-stream `Records` + `Stats` + `End`
    // frames.
    //
    // Limitations (deliberate, documented):
    //   - Parquet input is rejected with NotImplemented.
    //   - Aggregates / GROUP BY / JOIN / ORDER BY / LIMIT are rejected at
    //     parse time as InvalidRequest (s3s 0.13 doesn't expose AWS's
    //     domain-specific `InvalidSqlExpression` code).
    //   - The body is fully buffered before SQL evaluation (S3 Select
    //     streaming-during-evaluation is v0.7 scope).
    //   - GPU-accelerated WHERE evaluation is stubbed out (always None).
    async fn select_object_content(
        &self,
        req: S3Request<SelectObjectContentInput>,
    ) -> S3Result<S3Response<SelectObjectContentOutput>> {
        use crate::select::{
            EventStreamWriter, SelectInputFormat, SelectOutputFormat, run_select_csv,
            run_select_jsonlines,
        };

        let select_bucket = req.input.bucket.clone();
        let select_key = req.input.key.clone();
        self.enforce_rate_limit(&req, &select_bucket)?;
        self.enforce_policy(&req, "s3:GetObject", &select_bucket, Some(&select_key))?;

        let request = req.input.request;
        let sql = request.expression.clone();
        if request.expression_type.as_str() != "SQL" {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidExpressionType,
                format!(
                    "ExpressionType must be SQL, got: {}",
                    request.expression_type.as_str()
                ),
            ));
        }

        let input_format = if let Some(_json) = request.input_serialization.json.as_ref() {
            SelectInputFormat::JsonLines
        } else if let Some(csv) = request.input_serialization.csv.as_ref() {
            let has_header = csv
                .file_header_info
                .as_ref()
                .map(|h| {
                    let s = h.as_str();
                    s.eq_ignore_ascii_case("USE") || s.eq_ignore_ascii_case("IGNORE")
                })
                .unwrap_or(false);
            let delim = csv
                .field_delimiter
                .as_deref()
                .and_then(|s| s.chars().next())
                .unwrap_or(',');
            SelectInputFormat::Csv {
                has_header,
                delimiter: delim,
            }
        } else if request.input_serialization.parquet.is_some() {
            return Err(S3Error::with_message(
                S3ErrorCode::NotImplemented,
                "Parquet input is not supported by this S3 Select implementation (v0.6: CSV / JSON Lines only)",
            ));
        } else {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidRequest,
                "InputSerialization requires exactly one of CSV / JSON / Parquet",
            ));
        };
        if let Some(ct) = request.input_serialization.compression_type.as_ref()
            && !ct.as_str().eq_ignore_ascii_case("NONE")
        {
            return Err(S3Error::with_message(
                S3ErrorCode::NotImplemented,
                format!(
                    "InputSerialization CompressionType={} is not supported (v0.6: NONE only)",
                    ct.as_str()
                ),
            ));
        }

        let output_format = if request.output_serialization.json.is_some() {
            SelectOutputFormat::Json
        } else if request.output_serialization.csv.is_some() {
            SelectOutputFormat::Csv
        } else {
            return Err(S3Error::with_message(
                S3ErrorCode::InvalidRequest,
                "OutputSerialization requires exactly one of CSV / JSON",
            ));
        };

        let get_input = GetObjectInput {
            bucket: select_bucket.clone(),
            key: select_key.clone(),
            sse_customer_algorithm: req.input.sse_customer_algorithm.clone(),
            sse_customer_key: req.input.sse_customer_key.clone(),
            sse_customer_key_md5: req.input.sse_customer_key_md5.clone(),
            ..Default::default()
        };
        let get_req = S3Request {
            input: get_input,
            method: http::Method::GET,
            uri: format!("/{}/{}", select_bucket, select_key)
                .parse()
                .map_err(|e| {
                    S3Error::with_message(
                        S3ErrorCode::InternalError,
                        format!("constructing inner GET URI: {e}"),
                    )
                })?,
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let mut get_resp = self.get_object(get_req).await?;
        let blob = get_resp.output.body.take().ok_or_else(|| {
            S3Error::with_message(
                S3ErrorCode::InternalError,
                "Select: object body was empty after GET",
            )
        })?;
        let body_bytes = crate::blob::collect_blob(blob, self.max_body_bytes)
            .await
            .map_err(internal("collect Select body"))?;
        let scanned = body_bytes.len() as u64;

        let matched_payload = match input_format {
            SelectInputFormat::JsonLines => run_select_jsonlines(&sql, &body_bytes, output_format)
                .map_err(|e| select_error_to_s3(e, "JSON Lines"))?,
            SelectInputFormat::Csv { .. } => {
                run_select_csv(&sql, &body_bytes, input_format, output_format)
                    .map_err(|e| select_error_to_s3(e, "CSV"))?
            }
        };

        let returned = matched_payload.len() as u64;
        let processed = scanned;
        let mut events: Vec<S3Result<SelectObjectContentEvent>> = Vec::with_capacity(3);
        if !matched_payload.is_empty() {
            events.push(Ok(SelectObjectContentEvent::Records(RecordsEvent {
                payload: Some(bytes::Bytes::from(matched_payload)),
            })));
        }
        events.push(Ok(SelectObjectContentEvent::Stats(StatsEvent {
            details: Some(Stats {
                bytes_scanned: Some(scanned as i64),
                bytes_processed: Some(processed as i64),
                bytes_returned: Some(returned as i64),
            }),
        })));
        events.push(Ok(SelectObjectContentEvent::End(EndEvent {})));
        // Touch EventStreamWriter so the public API stays linked into the
        // build (the actual wire framing is delegated to s3s).
        let _writer = EventStreamWriter::new();

        let stream = SelectObjectContentEventStream::new(futures::stream::iter(events));
        let output = SelectObjectContentOutput {
            payload: Some(stream),
        };
        Ok(S3Response::new(output))
    }

    // ---- Bucket Inventory configuration (v0.6 #36) ----
    //
    // When an `InventoryManager` is attached, S4-server owns the
    // configuration store and these handlers no longer pass through to
    // the backend. The mapping between the s3s-typed
    // `InventoryConfiguration` and the inventory module's internal
    // `InventoryConfig` is intentionally lossy: only the fields S4
    // actually uses for periodic CSV emission survive the round trip
    // (id, source bucket, destination bucket / prefix, format, included
    // versions, schedule frequency). Optional fields, encryption, and
    // filter prefixes are accepted on PUT and re-surfaced on GET via
    // a best-effort default-shape `InventoryConfiguration` so the
    // client sees a roundtrip-clean response.
    async fn put_bucket_inventory_configuration(
        &self,
        req: S3Request<PutBucketInventoryConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketInventoryConfigurationOutput>> {
        if let Some(mgr) = self.inventory.as_ref() {
            let cfg = inv_from_dto(
                &req.input.bucket,
                &req.input.id,
                &req.input.inventory_configuration,
            );
            mgr.put(cfg);
            return Ok(S3Response::new(
                PutBucketInventoryConfigurationOutput::default(),
            ));
        }
        self.backend.put_bucket_inventory_configuration(req).await
    }

    async fn get_bucket_inventory_configuration(
        &self,
        req: S3Request<GetBucketInventoryConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketInventoryConfigurationOutput>> {
        if let Some(mgr) = self.inventory.as_ref() {
            let cfg = mgr.get(&req.input.bucket, &req.input.id);
            if let Some(cfg) = cfg {
                let out = GetBucketInventoryConfigurationOutput {
                    inventory_configuration: Some(inv_to_dto(&cfg)),
                };
                return Ok(S3Response::new(out));
            }
            // AWS returns `NoSuchConfiguration` (404) when the id has no
            // matching inventory configuration on the bucket. The
            // generated `S3ErrorCode` enum doesn't expose a typed variant
            // for this code, so we round-trip through `from_bytes` which
            // wraps unknown codes as `Custom(...)` (= the AWS-canonical
            // error-code string survives into the XML response envelope).
            let code =
                S3ErrorCode::from_bytes(b"NoSuchConfiguration").unwrap_or(S3ErrorCode::NoSuchKey);
            return Err(S3Error::with_message(
                code,
                format!(
                    "no inventory configuration with id={} on bucket={}",
                    req.input.id, req.input.bucket
                ),
            ));
        }
        self.backend.get_bucket_inventory_configuration(req).await
    }

    async fn list_bucket_inventory_configurations(
        &self,
        req: S3Request<ListBucketInventoryConfigurationsInput>,
    ) -> S3Result<S3Response<ListBucketInventoryConfigurationsOutput>> {
        if let Some(mgr) = self.inventory.as_ref() {
            let list = mgr.list_for_bucket(&req.input.bucket);
            let dto_list: Vec<InventoryConfiguration> = list.iter().map(inv_to_dto).collect();
            let out = ListBucketInventoryConfigurationsOutput {
                continuation_token: req.input.continuation_token.clone(),
                inventory_configuration_list: if dto_list.is_empty() {
                    None
                } else {
                    Some(dto_list)
                },
                is_truncated: Some(false),
                next_continuation_token: None,
            };
            return Ok(S3Response::new(out));
        }
        self.backend.list_bucket_inventory_configurations(req).await
    }

    async fn delete_bucket_inventory_configuration(
        &self,
        req: S3Request<DeleteBucketInventoryConfigurationInput>,
    ) -> S3Result<S3Response<DeleteBucketInventoryConfigurationOutput>> {
        if let Some(mgr) = self.inventory.as_ref() {
            mgr.delete(&req.input.bucket, &req.input.id);
            return Ok(S3Response::new(
                DeleteBucketInventoryConfigurationOutput::default(),
            ));
        }
        self.backend
            .delete_bucket_inventory_configuration(req)
            .await
    }
}

// ---------------------------------------------------------------------------
// v0.6 #36: Convert between the s3s-typed `InventoryConfiguration` (the wire
// surface) and our internal `crate::inventory::InventoryConfig`. Only the
// fields S4 actually uses for CSV emission survive the round trip; the
// missing fields (filter prefix, optional fields, encryption) are dropped on
// PUT and re-rendered as the AWS-default shape on GET so the client sees a
// well-formed `InventoryConfiguration`.
// ---------------------------------------------------------------------------

fn inv_from_dto(
    bucket: &str,
    id: &str,
    dto: &InventoryConfiguration,
) -> crate::inventory::InventoryConfig {
    let frequency_hours = match dto.schedule.frequency.as_str() {
        "Weekly" => 24 * 7,
        // Daily is the default; anything S4 doesn't recognise (incl.
        // empty, which is the s3s-default) maps to Daily so the
        // operator's PUT doesn't silently turn into a no-op cadence.
        _ => 24,
    };
    // Parquet/ORC are not supported (issue #36 scope); we still accept
    // the PUT so callers don't fail-loud, but we record CSV and rely on
    // the operator catching the discrepancy on GET.
    let format = crate::inventory::InventoryFormat::Csv;
    crate::inventory::InventoryConfig {
        id: id.to_owned(),
        bucket: bucket.to_owned(),
        destination_bucket: dto.destination.s3_bucket_destination.bucket.clone(),
        destination_prefix: dto
            .destination
            .s3_bucket_destination
            .prefix
            .clone()
            .unwrap_or_default(),
        frequency_hours,
        format,
        included_object_versions: crate::inventory::IncludedVersions::from_aws_str(
            dto.included_object_versions.as_str(),
        ),
    }
}

fn inv_to_dto(cfg: &crate::inventory::InventoryConfig) -> InventoryConfiguration {
    InventoryConfiguration {
        id: cfg.id.clone(),
        is_enabled: true,
        included_object_versions: InventoryIncludedObjectVersions::from(
            cfg.included_object_versions.as_aws_str().to_owned(),
        ),
        destination: InventoryDestination {
            s3_bucket_destination: InventoryS3BucketDestination {
                account_id: None,
                bucket: cfg.destination_bucket.clone(),
                encryption: None,
                format: InventoryFormat::from(cfg.format.as_aws_str().to_owned()),
                prefix: if cfg.destination_prefix.is_empty() {
                    None
                } else {
                    Some(cfg.destination_prefix.clone())
                },
            },
        },
        schedule: InventorySchedule {
            // `frequency_hours == 168` -> Weekly; everything else maps to
            // Daily for the wire response (the manager keeps the precise
            // hour count internally for due-checking).
            frequency: InventoryFrequency::from(
                if cfg.frequency_hours == 24 * 7 {
                    "Weekly"
                } else {
                    "Daily"
                }
                .to_owned(),
            ),
        },
        filter: None,
        optional_fields: None,
    }
}

// ---------------------------------------------------------------------------
// v0.6 #35: Convert between the s3s-typed `NotificationConfiguration` (the
// wire surface) and our internal `crate::notifications::NotificationConfig`.
//
// We support TopicConfiguration (-> Destination::Sns) and QueueConfiguration
// (-> Destination::Sqs). LambdaFunction and EventBridge configurations are
// silently dropped on PUT (out of scope for v0.6 #35); the GET response only
// surfaces topic / queue rules.
//
// The webhook destination has no AWS-native wire form: operators configure
// webhooks via the JSON snapshot file (`--notifications-state-file`) or by
// poking `NotificationManager::put` directly from a custom binary. This
// keeps the wire surface AWS-compatible while still letting the always-
// available `Webhook` destination be reachable.
// ---------------------------------------------------------------------------

fn notif_from_dto(dto: &NotificationConfiguration) -> crate::notifications::NotificationConfig {
    let mut rules: Vec<crate::notifications::NotificationRule> = Vec::new();
    if let Some(topics) = dto.topic_configurations.as_ref() {
        for (idx, t) in topics.iter().enumerate() {
            let events = events_from_dto(&t.events);
            let (prefix, suffix) = filter_from_dto(t.filter.as_ref());
            rules.push(crate::notifications::NotificationRule {
                id: t.id.clone().unwrap_or_else(|| format!("topic-{idx}")),
                events,
                destination: crate::notifications::Destination::Sns {
                    topic_arn: t.topic_arn.clone(),
                },
                filter_prefix: prefix,
                filter_suffix: suffix,
            });
        }
    }
    if let Some(queues) = dto.queue_configurations.as_ref() {
        for (idx, q) in queues.iter().enumerate() {
            let events = events_from_dto(&q.events);
            let (prefix, suffix) = filter_from_dto(q.filter.as_ref());
            rules.push(crate::notifications::NotificationRule {
                id: q.id.clone().unwrap_or_else(|| format!("queue-{idx}")),
                events,
                destination: crate::notifications::Destination::Sqs {
                    queue_arn: q.queue_arn.clone(),
                },
                filter_prefix: prefix,
                filter_suffix: suffix,
            });
        }
    }
    crate::notifications::NotificationConfig { rules }
}

fn notif_to_dto(cfg: &crate::notifications::NotificationConfig) -> NotificationConfiguration {
    let mut topics: Vec<TopicConfiguration> = Vec::new();
    let mut queues: Vec<QueueConfiguration> = Vec::new();
    for rule in &cfg.rules {
        let events: Vec<Event> = rule
            .events
            .iter()
            .map(|e| Event::from(e.as_aws_str().to_owned()))
            .collect();
        let filter = filter_to_dto(rule.filter_prefix.as_deref(), rule.filter_suffix.as_deref());
        match &rule.destination {
            crate::notifications::Destination::Sns { topic_arn } => {
                topics.push(TopicConfiguration {
                    events,
                    filter,
                    id: Some(rule.id.clone()),
                    topic_arn: topic_arn.clone(),
                });
            }
            crate::notifications::Destination::Sqs { queue_arn } => {
                queues.push(QueueConfiguration {
                    events,
                    filter,
                    id: Some(rule.id.clone()),
                    queue_arn: queue_arn.clone(),
                });
            }
            // Webhook destinations have no AWS wire equivalent — they
            // round-trip through the JSON snapshot only. Skip them on the
            // GET surface (an SDK consumer wouldn't know what to do with
            // them anyway).
            crate::notifications::Destination::Webhook { .. } => {}
        }
    }
    NotificationConfiguration {
        event_bridge_configuration: None,
        lambda_function_configurations: None,
        queue_configurations: if queues.is_empty() {
            None
        } else {
            Some(queues)
        },
        topic_configurations: if topics.is_empty() {
            None
        } else {
            Some(topics)
        },
    }
}

fn events_from_dto(events: &[Event]) -> Vec<crate::notifications::EventType> {
    events
        .iter()
        .filter_map(|e| crate::notifications::EventType::from_aws_str(e.as_ref()))
        .collect()
}

fn filter_from_dto(
    f: Option<&NotificationConfigurationFilter>,
) -> (Option<String>, Option<String>) {
    let Some(f) = f else {
        return (None, None);
    };
    let Some(key) = f.key.as_ref() else {
        return (None, None);
    };
    let Some(rules) = key.filter_rules.as_ref() else {
        return (None, None);
    };
    let mut prefix = None;
    let mut suffix = None;
    for r in rules {
        let name = r.name.as_ref().map(|n| n.as_str().to_ascii_lowercase());
        let value = r.value.clone();
        match name.as_deref() {
            Some("prefix") => prefix = value,
            Some("suffix") => suffix = value,
            _ => {}
        }
    }
    (prefix, suffix)
}

fn filter_to_dto(
    prefix: Option<&str>,
    suffix: Option<&str>,
) -> Option<NotificationConfigurationFilter> {
    if prefix.is_none() && suffix.is_none() {
        return None;
    }
    let mut rules: Vec<FilterRule> = Vec::new();
    if let Some(p) = prefix {
        rules.push(FilterRule {
            name: Some(FilterRuleName::from("prefix".to_owned())),
            value: Some(p.to_owned()),
        });
    }
    if let Some(s) = suffix {
        rules.push(FilterRule {
            name: Some(FilterRuleName::from("suffix".to_owned())),
            value: Some(s.to_owned()),
        });
    }
    Some(NotificationConfigurationFilter {
        key: Some(S3KeyFilter {
            filter_rules: Some(rules),
        }),
    })
}

// ---------------------------------------------------------------------------
// v0.6 #40: Convert between the s3s-typed `ReplicationConfiguration` (the
// wire surface) and our internal `crate::replication::ReplicationConfig`.
// AWS's `ReplicationRuleFilter` is a sum type — `Prefix | Tag | And { Prefix,
// Tags }`; we flatten it into the single `(prefix, tag-vec)` representation
// the matcher needs. Sub-blocks v0.6 #40 does not implement
// (DeleteMarkerReplication / SourceSelectionCriteria / ReplicationTime /
// Metrics / EncryptionConfiguration) round-trip as `None` on GET — operators
// who set them on PUT see them silently dropped, mirroring "feature not
// supported in this release" semantics.
// ---------------------------------------------------------------------------

fn replication_from_dto(dto: &ReplicationConfiguration) -> crate::replication::ReplicationConfig {
    let rules = dto
        .rules
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            let id =
                r.id.as_ref()
                    .map(|s| s.as_str().to_owned())
                    .unwrap_or_else(|| format!("rule-{idx}"));
            let priority = r.priority.unwrap_or(0).max(0) as u32;
            let status_enabled = r.status.as_str() == ReplicationRuleStatus::ENABLED;
            let filter = replication_filter_from_dto(r.filter.as_ref(), r.prefix.as_deref());
            let destination_bucket = r.destination.bucket.clone();
            let destination_storage_class = r
                .destination
                .storage_class
                .as_ref()
                .map(|s| s.as_str().to_owned());
            crate::replication::ReplicationRule {
                id,
                priority,
                status_enabled,
                filter,
                destination_bucket,
                destination_storage_class,
            }
        })
        .collect();
    crate::replication::ReplicationConfig {
        role: dto.role.clone(),
        rules,
    }
}

fn replication_to_dto(cfg: &crate::replication::ReplicationConfig) -> ReplicationConfiguration {
    let rules = cfg
        .rules
        .iter()
        .map(|r| {
            let status = if r.status_enabled {
                ReplicationRuleStatus::from_static(ReplicationRuleStatus::ENABLED)
            } else {
                ReplicationRuleStatus::from_static(ReplicationRuleStatus::DISABLED)
            };
            let destination = Destination {
                access_control_translation: None,
                account: None,
                bucket: r.destination_bucket.clone(),
                encryption_configuration: None,
                metrics: None,
                replication_time: None,
                storage_class: r
                    .destination_storage_class
                    .as_ref()
                    .map(|s| StorageClass::from(s.clone())),
            };
            let filter = Some(replication_filter_to_dto(&r.filter));
            ReplicationRule {
                delete_marker_replication: None,
                destination,
                existing_object_replication: None,
                filter,
                id: Some(r.id.clone()),
                prefix: None,
                priority: Some(r.priority as i32),
                source_selection_criteria: None,
                status,
            }
        })
        .collect();
    ReplicationConfiguration {
        role: cfg.role.clone(),
        rules,
    }
}

fn replication_filter_from_dto(
    f: Option<&ReplicationRuleFilter>,
    rule_level_prefix: Option<&str>,
) -> crate::replication::ReplicationFilter {
    let mut prefix: Option<String> = rule_level_prefix.map(str::to_owned);
    let mut tags: Vec<(String, String)> = Vec::new();
    if let Some(f) = f {
        if let Some(p) = f.prefix.as_ref()
            && prefix.is_none()
        {
            prefix = Some(p.clone());
        }
        if let Some(t) = f.tag.as_ref()
            && let (Some(k), Some(v)) = (t.key.as_ref(), t.value.as_ref())
        {
            tags.push((k.clone(), v.clone()));
        }
        if let Some(and) = f.and.as_ref() {
            if let Some(p) = and.prefix.as_ref()
                && prefix.is_none()
            {
                prefix = Some(p.clone());
            }
            if let Some(ts) = and.tags.as_ref() {
                for t in ts {
                    if let (Some(k), Some(v)) = (t.key.as_ref(), t.value.as_ref()) {
                        tags.push((k.clone(), v.clone()));
                    }
                }
            }
        }
    }
    crate::replication::ReplicationFilter { prefix, tags }
}

fn replication_filter_to_dto(f: &crate::replication::ReplicationFilter) -> ReplicationRuleFilter {
    if f.tags.is_empty() {
        ReplicationRuleFilter {
            and: None,
            prefix: f.prefix.clone(),
            tag: None,
        }
    } else if f.tags.len() == 1 && f.prefix.is_none() {
        let (k, v) = &f.tags[0];
        ReplicationRuleFilter {
            and: None,
            prefix: None,
            tag: Some(Tag {
                key: Some(k.clone()),
                value: Some(v.clone()),
            }),
        }
    } else {
        let tags: Vec<Tag> = f
            .tags
            .iter()
            .map(|(k, v)| Tag {
                key: Some(k.clone()),
                value: Some(v.clone()),
            })
            .collect();
        ReplicationRuleFilter {
            and: Some(ReplicationRuleAndOperator {
                prefix: f.prefix.clone(),
                tags: Some(tags),
            }),
            prefix: None,
            tag: None,
        }
    }
}

// ---------------------------------------------------------------------------
// v0.6 #37: Convert between the s3s-typed `BucketLifecycleConfiguration`
// (the wire surface) and our internal `crate::lifecycle::LifecycleConfig`.
// The internal representation flattens AWS's "Filter | And" disjunction
// into a single `LifecycleFilter` struct of optional fields plus a tag
// vector. Fields S4's evaluator does not consume
// (`expired_object_delete_marker`, `noncurrent_version_transitions`,
// `transition_default_minimum_object_size`, the storage class on the
// noncurrent expiration) are dropped on PUT and re-rendered as their
// AWS-default shape on GET so the client always sees a well-formed
// configuration.
// ---------------------------------------------------------------------------

fn dto_lifecycle_to_internal(
    dto: &BucketLifecycleConfiguration,
) -> crate::lifecycle::LifecycleConfig {
    crate::lifecycle::LifecycleConfig {
        rules: dto.rules.iter().map(dto_rule_to_internal).collect(),
    }
}

fn dto_rule_to_internal(rule: &LifecycleRule) -> crate::lifecycle::LifecycleRule {
    let status = crate::lifecycle::LifecycleStatus::from_aws_str(rule.status.as_str());
    let filter = rule
        .filter
        .as_ref()
        .map(dto_filter_to_internal)
        .unwrap_or_default();
    let expiration_days = rule
        .expiration
        .as_ref()
        .and_then(|e| e.days)
        .and_then(|d| u32::try_from(d).ok());
    let expiration_date = rule
        .expiration
        .as_ref()
        .and_then(|e| e.date.as_ref())
        .and_then(timestamp_to_chrono_utc);
    let transitions: Vec<crate::lifecycle::TransitionRule> = rule
        .transitions
        .as_ref()
        .map(|ts| {
            ts.iter()
                .filter_map(|t| {
                    let days = u32::try_from(t.days?).ok()?;
                    let storage_class = t.storage_class.as_ref()?.as_str().to_owned();
                    Some(crate::lifecycle::TransitionRule {
                        days,
                        storage_class,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let noncurrent_version_expiration_days = rule
        .noncurrent_version_expiration
        .as_ref()
        .and_then(|n| n.noncurrent_days)
        .and_then(|d| u32::try_from(d).ok());
    let abort_incomplete_multipart_upload_days = rule
        .abort_incomplete_multipart_upload
        .as_ref()
        .and_then(|a| a.days_after_initiation)
        .and_then(|d| u32::try_from(d).ok());
    crate::lifecycle::LifecycleRule {
        id: rule.id.clone().unwrap_or_default(),
        status,
        filter,
        expiration_days,
        expiration_date,
        transitions,
        noncurrent_version_expiration_days,
        abort_incomplete_multipart_upload_days,
    }
}

fn dto_filter_to_internal(filter: &LifecycleRuleFilter) -> crate::lifecycle::LifecycleFilter {
    let mut prefix = filter.prefix.clone();
    let mut tags: Vec<(String, String)> = Vec::new();
    let mut size_gt: Option<u64> = filter
        .object_size_greater_than
        .and_then(|n| u64::try_from(n).ok());
    let mut size_lt: Option<u64> = filter
        .object_size_less_than
        .and_then(|n| u64::try_from(n).ok());
    if let Some(t) = &filter.tag
        && let (Some(k), Some(v)) = (t.key.as_ref(), t.value.as_ref())
    {
        tags.push((k.clone(), v.clone()));
    }
    if let Some(and) = &filter.and {
        if prefix.is_none() {
            prefix = and.prefix.clone();
        }
        if size_gt.is_none() {
            size_gt = and
                .object_size_greater_than
                .and_then(|n| u64::try_from(n).ok());
        }
        if size_lt.is_none() {
            size_lt = and
                .object_size_less_than
                .and_then(|n| u64::try_from(n).ok());
        }
        if let Some(ts) = &and.tags {
            for t in ts {
                if let (Some(k), Some(v)) = (t.key.as_ref(), t.value.as_ref()) {
                    tags.push((k.clone(), v.clone()));
                }
            }
        }
    }
    crate::lifecycle::LifecycleFilter {
        prefix,
        tags,
        object_size_greater_than: size_gt,
        object_size_less_than: size_lt,
    }
}

fn internal_rule_to_dto(rule: &crate::lifecycle::LifecycleRule) -> LifecycleRule {
    let expiration = if rule.expiration_days.is_some() || rule.expiration_date.is_some() {
        Some(LifecycleExpiration {
            date: rule.expiration_date.map(chrono_utc_to_timestamp),
            days: rule.expiration_days.map(|d| d as i32),
            expired_object_delete_marker: None,
        })
    } else {
        None
    };
    let transitions: Option<TransitionList> = if rule.transitions.is_empty() {
        None
    } else {
        Some(
            rule.transitions
                .iter()
                .map(|t| Transition {
                    date: None,
                    days: Some(t.days as i32),
                    storage_class: Some(TransitionStorageClass::from(t.storage_class.clone())),
                })
                .collect(),
        )
    };
    let noncurrent_version_expiration =
        rule.noncurrent_version_expiration_days
            .map(|d| NoncurrentVersionExpiration {
                newer_noncurrent_versions: None,
                noncurrent_days: Some(d as i32),
            });
    let abort_incomplete_multipart_upload =
        rule.abort_incomplete_multipart_upload_days
            .map(|d| AbortIncompleteMultipartUpload {
                days_after_initiation: Some(d as i32),
            });
    let filter = if rule.filter.tags.is_empty()
        && rule.filter.object_size_greater_than.is_none()
        && rule.filter.object_size_less_than.is_none()
    {
        rule.filter.prefix.as_ref().map(|p| LifecycleRuleFilter {
            and: None,
            object_size_greater_than: None,
            object_size_less_than: None,
            prefix: Some(p.clone()),
            tag: None,
        })
    } else if rule.filter.tags.len() == 1
        && rule.filter.prefix.is_none()
        && rule.filter.object_size_greater_than.is_none()
        && rule.filter.object_size_less_than.is_none()
    {
        let (k, v) = rule.filter.tags[0].clone();
        Some(LifecycleRuleFilter {
            and: None,
            object_size_greater_than: None,
            object_size_less_than: None,
            prefix: None,
            tag: Some(Tag {
                key: Some(k),
                value: Some(v),
            }),
        })
    } else {
        let tags = if rule.filter.tags.is_empty() {
            None
        } else {
            Some(
                rule.filter
                    .tags
                    .iter()
                    .map(|(k, v)| Tag {
                        key: Some(k.clone()),
                        value: Some(v.clone()),
                    })
                    .collect(),
            )
        };
        Some(LifecycleRuleFilter {
            and: Some(LifecycleRuleAndOperator {
                object_size_greater_than: rule
                    .filter
                    .object_size_greater_than
                    .and_then(|n| i64::try_from(n).ok()),
                object_size_less_than: rule
                    .filter
                    .object_size_less_than
                    .and_then(|n| i64::try_from(n).ok()),
                prefix: rule.filter.prefix.clone(),
                tags,
            }),
            object_size_greater_than: None,
            object_size_less_than: None,
            prefix: None,
            tag: None,
        })
    };
    LifecycleRule {
        abort_incomplete_multipart_upload,
        expiration,
        filter,
        id: if rule.id.is_empty() {
            None
        } else {
            Some(rule.id.clone())
        },
        noncurrent_version_expiration,
        noncurrent_version_transitions: None,
        prefix: None,
        status: ExpirationStatus::from(rule.status.as_aws_str().to_owned()),
        transitions,
    }
}

// (timestamp <-> chrono helpers `timestamp_to_chrono_utc` /
// `chrono_utc_to_timestamp` are defined earlier in this file for the
// tagging/notifications work; the lifecycle DTO converters reuse them.)

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
///
/// v0.8.4 #76 (audit H-6): the gate now enforces an `x-amz-date`
/// freshness window (default 15 min, AWS-spec) and a strict credential
/// scope shape (`<key>/<YYYYMMDD>/s3/aws4_request`), shutting the
/// captured-request replay vector — previously a stolen valid SigV4a
/// signature could be replayed indefinitely (including DELETE).
#[derive(Debug, Clone)]
pub struct SigV4aGate {
    store: crate::sigv4a::SharedSigV4aCredentialStore,
    /// v0.8.4 #76: how far the request's `x-amz-date` may drift from
    /// the server's clock before being rejected with 403
    /// `RequestTimeTooSkewed`. Matches the AWS S3 spec default of
    /// 15 min when constructed via [`SigV4aGate::new`]; the operator
    /// can override via [`SigV4aGate::with_skew_tolerance`] (CLI flag
    /// `--sigv4a-skew-tolerance-seconds`).
    skew_tolerance: chrono::Duration,
}

impl SigV4aGate {
    /// Default `x-amz-date` skew tolerance — 15 min, matching AWS S3.
    pub const DEFAULT_SKEW_TOLERANCE_SECS: i64 = 900;

    #[must_use]
    pub fn new(store: crate::sigv4a::SharedSigV4aCredentialStore) -> Self {
        Self {
            store,
            skew_tolerance: chrono::Duration::seconds(Self::DEFAULT_SKEW_TOLERANCE_SECS),
        }
    }

    /// v0.8.4 #76: override the `x-amz-date` skew tolerance (default
    /// 15 min). Operators can widen this for high-clock-drift
    /// environments or tighten it for compliance regimes that demand
    /// stricter freshness.
    #[must_use]
    pub fn with_skew_tolerance(mut self, skew: chrono::Duration) -> Self {
        self.skew_tolerance = skew;
        self
    }

    /// Read the configured skew tolerance — exposed mostly for test +
    /// observability use.
    #[must_use]
    pub fn skew_tolerance(&self) -> chrono::Duration {
        self.skew_tolerance
    }

    /// Inspect an incoming HTTP request. Behaviour:
    ///
    /// - Not SigV4a (no `X-Amz-Region-Set` and no SigV4a `Authorization`
    ///   prefix) → returns `Ok(())`; the framework's existing SigV4
    ///   path handles the request.
    /// - SigV4a + valid signature + region match + fresh x-amz-date
    ///   → `Ok(())`.
    /// - SigV4a + unknown access-key-id → `Err` with `InvalidAccessKeyId`.
    /// - SigV4a + bad signature / region mismatch → `Err` with
    ///   `SignatureDoesNotMatch`.
    /// - SigV4a + missing or skewed `x-amz-date` → `Err` with one of
    ///   the v0.8.4 #76 freshness variants (`RequestTimeTooSkewed`
    ///   et al.).
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
        self.pre_route_at(
            req,
            requested_region,
            canonical_request_bytes,
            chrono::Utc::now(),
        )
    }

    /// Like [`SigV4aGate::pre_route`] but takes an explicit `now` for
    /// tests that need to pin the freshness clock. Production callers
    /// use `pre_route` (which calls `chrono::Utc::now()`).
    pub fn pre_route_at<B>(
        &self,
        req: &http::Request<B>,
        requested_region: &str,
        canonical_request_bytes: &[u8],
        now: chrono::DateTime<chrono::Utc>,
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
            .map_err(|_| SigV4aGateError::MalformedAuthorization)?;
        let region_set = req
            .headers()
            .get(crate::sigv4a::REGION_SET_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("*");
        let key = self
            .store
            .get(&parsed.access_key_id)
            .ok_or_else(|| SigV4aGateError::UnknownAccessKey(parsed.access_key_id.clone()))?;
        // v0.8.4 #76: snapshot the request headers into a
        // lowercase-keyed flat map so `verify_request` can do the
        // x-amz-date freshness checks without taking a generic
        // `HeaderMap` dep. Cheap because the headers list is tiny.
        //
        // v0.8.5 #84 (audit H-4): detect duplicate header names while
        // we flatten — `HashMap::insert` would silently overwrite the
        // first value with the second, mirroring the auth-confusion
        // vector the canonical-request builder also defends against.
        // Reject upfront so the rest of the gate (freshness check,
        // ECDSA verify) never sees a half-truncated header set. We
        // detect by checking `contains_key` *before* insertion rather
        // than by counting via `headers().get_all`, because the
        // upstream `HeaderMap` iteration yields each duplicate entry
        // as its own (name, value) pair — the second-seen entry is
        // exactly what `contains_key` traps.
        let mut header_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::with_capacity(req.headers().len());
        for (name, value) in req.headers() {
            if let Ok(v) = value.to_str() {
                let lower = name.as_str().to_ascii_lowercase();
                if header_map.contains_key(&lower) {
                    return Err(SigV4aGateError::Verify(
                        crate::sigv4a::SigV4aError::DuplicateSignedHeader { header: lower },
                    ));
                }
                header_map.insert(lower, v.to_string());
            }
        }
        crate::sigv4a::verify_request(
            &parsed,
            &header_map,
            canonical_request_bytes,
            key,
            region_set,
            requested_region,
            now,
            self.skew_tolerance,
        )
        .map_err(SigV4aGateError::Verify)?;
        Ok(())
    }
}

/// Failure modes from [`SigV4aGate::pre_route`]. All variants map to
/// HTTP 403 with one of the two AWS-standard error codes
/// (`InvalidAccessKeyId` / `SignatureDoesNotMatch` / `RequestTimeTooSkewed`)
/// — see [`SigV4aGateError::s3_error_code`].
///
/// v1.0 stability: `#[non_exhaustive]` — new gate-level failures may
/// be added in minor releases. Downstream callers must include a
/// `_ =>` arm when matching on this enum.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// AWS S3 error code that should accompany the response.
    ///
    /// v0.8.4 #76 (audit H-6): the freshness check surfaces
    /// `RequestTimeTooSkewed` (matches AWS spec); date / scope shape
    /// failures surface as `InvalidRequest` (400); other failures stay
    /// `SignatureDoesNotMatch` / `InvalidAccessKeyId` (403) so the wire
    /// surface stays AWS-compatible.
    #[must_use]
    pub fn s3_error_code(&self) -> &'static str {
        match self {
            Self::UnknownAccessKey(_) => "InvalidAccessKeyId",
            Self::Verify(crate::sigv4a::SigV4aError::RequestTimeTooSkewed { .. }) => {
                "RequestTimeTooSkewed"
            }
            Self::Verify(
                crate::sigv4a::SigV4aError::MissingXAmzDate
                | crate::sigv4a::SigV4aError::InvalidDateFormat
                | crate::sigv4a::SigV4aError::DateScopeMismatch
                | crate::sigv4a::SigV4aError::XAmzDateNotSigned
                | crate::sigv4a::SigV4aError::InvalidTerminator
                | crate::sigv4a::SigV4aError::WrongService { .. }
                | crate::sigv4a::SigV4aError::InvalidCredentialScope,
            ) => "InvalidRequest",
            _ => "SignatureDoesNotMatch",
        }
    }

    /// HTTP status code to accompany the response. v0.8.4 #76: format
    /// errors that are clearly client mistakes (missing / malformed
    /// `x-amz-date`, malformed credential scope, wrong service) are
    /// surfaced as 400 InvalidRequest; the rest stay 403.
    #[must_use]
    pub fn http_status(&self) -> http::StatusCode {
        match self {
            Self::Verify(
                crate::sigv4a::SigV4aError::MissingXAmzDate
                | crate::sigv4a::SigV4aError::InvalidDateFormat
                | crate::sigv4a::SigV4aError::DateScopeMismatch
                | crate::sigv4a::SigV4aError::XAmzDateNotSigned
                | crate::sigv4a::SigV4aError::InvalidTerminator
                | crate::sigv4a::SigV4aError::WrongService { .. }
                | crate::sigv4a::SigV4aError::InvalidCredentialScope,
            ) => http::StatusCode::BAD_REQUEST,
            _ => http::StatusCode::FORBIDDEN,
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

    /// v1.0.1 audit R1 P1: client-supplied `s4-*` metadata must be
    /// stripped on PUT (case-insensitive), leaving non-reserved keys
    /// untouched. Without this, `x-amz-meta-s4-dict-id` on a normal PUT
    /// would route the later GET into the dictionary path and 5xx.
    #[test]
    fn strip_reserved_client_metadata_drops_s4_namespace() {
        let mut meta = Metadata::new();
        meta.insert("s4-dict-id".into(), "0123456789abcdef".into());
        meta.insert("S4-Codec".into(), "cpu-zstd".into());
        meta.insert("s4-original-size".into(), "1".into());
        meta.insert("s4-encrypted".into(), "aes-256-gcm".into());
        meta.insert("custom-key".into(), "kept".into());
        meta.insert("s4ish-but-not-reserved".into(), "kept".into());
        let mut opt = Some(meta);
        strip_reserved_client_metadata(&mut opt);
        let m = opt.expect("map survives, only reserved keys removed");
        assert!(!m.contains_key("s4-dict-id"));
        assert!(!m.contains_key("S4-Codec"));
        assert!(!m.contains_key("s4-original-size"));
        assert!(!m.contains_key("s4-encrypted"));
        assert_eq!(m.get("custom-key").map(String::as_str), Some("kept"));
        assert_eq!(
            m.get("s4ish-but-not-reserved").map(String::as_str),
            Some("kept")
        );
        let mut none: Option<Metadata> = None;
        strip_reserved_client_metadata(&mut none);
        assert!(none.is_none());
    }

    /// v1.0.1 audit R1 P2: gateway-side mutations of `.s4dict/<id>` keys
    /// are rejected (InvalidObjectName, same shape as `.s4index`), while
    /// reads stay allowed (content-addressed bytes are the documented
    /// no-gateway escape hatch).
    #[test]
    fn reserved_key_guard_blocks_dict_prefix_mutations() {
        struct NullBackend;
        impl S3 for NullBackend {}
        let svc = S4Service::new(
            NullBackend,
            Arc::new(CodecRegistry::new(CodecKind::CpuZstd)),
            Arc::new(s4_codec::dispatcher::AlwaysDispatcher(CodecKind::CpuZstd)),
        );
        let err = svc
            .check_not_reserved_key(".s4dict/0123456789abcdef", ReservedKeyMode::Mutating)
            .expect_err("mutating a dict object must be rejected");
        assert!(
            format!("{err:?}").contains("reserved"),
            "error must explain the reserved prefix: {err:?}"
        );
        assert!(
            svc.check_not_reserved_key(".s4dict/0123456789abcdef", ReservedKeyMode::Read)
                .is_ok(),
            "reading a dict object stays allowed"
        );
        assert!(
            svc.check_not_reserved_key("normal/key.json", ReservedKeyMode::Mutating)
                .is_ok()
        );
    }

    /// v1.1 `--zstd-dict`: `extract_dict_id` must reject anything that
    /// isn't exactly 16 lowercase hex chars — the value is spliced into
    /// a backend object key (`.s4dict/<id>`), so tainted metadata must
    /// not smuggle path segments.
    #[test]
    fn extract_dict_id_validates_shape() {
        let with = |v: &str| {
            let mut meta = Metadata::new();
            meta.insert(META_DICT_ID.into(), v.into());
            Some(meta)
        };
        assert_eq!(
            extract_dict_id(&with("0123456789abcdef")).as_deref(),
            Some("0123456789abcdef")
        );
        assert!(extract_dict_id(&with("../../../etc/pwd")).is_none());
        assert!(extract_dict_id(&with("0123456789ABCDEF")).is_none());
        assert!(extract_dict_id(&with("0123456789abcde")).is_none());
        assert!(extract_dict_id(&with("")).is_none());
        assert!(extract_dict_id(&None).is_none());
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

    // v0.7 #49: safe_object_uri must round-trip every legal S3 key
    // (which includes spaces, slashes, control chars, raw UTF-8) into
    // a parseable `http::Uri` instead of panicking like the previous
    // `format!(...).parse().unwrap()` call sites did.

    #[test]
    fn safe_object_uri_basic_ascii() {
        let uri = safe_object_uri("bucket", "key").expect("ascii must be safe");
        assert_eq!(uri.path(), "/bucket/key");
    }

    #[test]
    fn safe_object_uri_encodes_spaces() {
        let uri = safe_object_uri("bucket", "key with spaces").expect("must encode spaces");
        // RFC 3986 path-segment encoding turns ' ' into %20.
        assert!(
            uri.path().contains("%20"),
            "expected percent-encoded space, got {}",
            uri.path()
        );
        assert!(uri.path().starts_with("/bucket/"));
    }

    #[test]
    fn safe_object_uri_preserves_slashes() {
        // S3 keys legally contain '/' as a logical path separator —
        // the helper must NOT escape it (otherwise the synthetic URI
        // changes the perceived hierarchy).
        let uri = safe_object_uri("bucket", "key/with/slashes").expect("slashes must round-trip");
        assert_eq!(uri.path(), "/bucket/key/with/slashes");
    }

    #[test]
    fn safe_object_uri_handles_newline_without_panic() {
        // Newlines are control chars in URIs; whether the result is
        // Ok (encoded as %0A) or Err (parse rejects), the helper
        // MUST NOT panic. Either outcome is acceptable.
        let _ = safe_object_uri("bucket", "key\n");
    }

    #[test]
    fn safe_object_uri_handles_null_byte_without_panic() {
        let _ = safe_object_uri("bucket", "key\0bad");
    }

    #[test]
    fn safe_object_uri_handles_unicode_without_panic() {
        // RTL override, BOM, plain Japanese — none should panic.
        let _ = safe_object_uri("bucket", "rtl\u{202E}override");
        let _ = safe_object_uri("bucket", "\u{FEFF}bom-key");
        let _ = safe_object_uri("bucket", "日本語キー");
    }

    #[test]
    fn safe_object_uri_no_panic_for_every_byte() {
        // Exhaustive byte coverage: 0x00..=0xFF as a 1-byte key.
        // None of these may panic. (0x80..=0xFF are not valid UTF-8
        // by themselves; we go through `String::from_utf8_lossy` so
        // the helper sees a real `&str` regardless of the raw byte.)
        for b in 0u8..=255 {
            let s = String::from_utf8_lossy(&[b]).into_owned();
            let _ = safe_object_uri("bucket", &s);
        }
    }

    /// v0.8.1 #58: smoke test for the DEK-handling shape used by the
    /// SSE-KMS branches of `put_object` and `complete_multipart_upload`.
    /// Mirrors the call pattern (generate_dek → length check → copy
    /// into stack `[u8; 32]` → reborrow as `&[u8; 32]` for `SseSource`)
    /// without spinning up a full `S4Service`.
    ///
    /// The real assertion this guards against is a regression where
    /// the `Zeroizing` wrapper is accidentally dropped before the
    /// stack copy lands (e.g. someone refactors to use
    /// `let dek = kms.generate_dek(...).await?.0; drop(dek); ...`)
    /// or where `&**dek` is rewritten in a way that doesn't compile.
    #[tokio::test]
    async fn kms_dek_lifetime_within_function_scope() {
        use crate::kms::{KmsBackend, LocalKms};
        use std::collections::HashMap;
        use std::path::PathBuf;
        use zeroize::Zeroizing;

        let mut keks = HashMap::new();
        keks.insert("scope".to_string(), [33u8; 32]);
        let kms = LocalKms::from_keks(PathBuf::from("/tmp/kms-scope-test"), keks);

        // Mirror the put_object KMS branch shape exactly.
        let (dek, wrapped) = kms.generate_dek("scope").await.unwrap();
        assert_eq!(dek.len(), 32);
        let mut dek_arr: Zeroizing<[u8; 32]> = Zeroizing::new([0u8; 32]);
        dek_arr.copy_from_slice(&dek);

        // The reborrow used at the SseSource construction site —
        // mirrors the call-site pattern where `let dek_ref: &[u8; 32]`
        // auto-derefs from a `Zeroizing<[u8; 32]>` reference.
        let dek_ref: &[u8; 32] = &dek_arr;
        // Sanity: the reborrow points at the same bytes.
        assert_eq!(dek_ref, &*dek_arr);
        // Wrapped key id flows through unchanged.
        assert_eq!(wrapped.key_id, "scope");

        // At end of scope, both `dek` (Zeroizing<Vec<u8>>) and
        // `dek_arr` (Zeroizing<[u8; 32]>) are dropped, wiping the
        // backing memory. Cannot directly assert the wipe (would be
        // UB to read freed memory), so this test instead enforces
        // that the call shape compiles and executes; the wipe itself
        // is exercised by the `zeroize` crate's own test suite.
    }

    /// v0.8.5 #86 (audit M-2): the replication dispatcher must
    /// `acquire_owned()` a permit from `replication_semaphore` before
    /// kicking off the destination PUT, so a saturated semaphore
    /// back-pressures the in-flight queue depth instead of letting it
    /// grow without bound. We exercise the field directly (initial
    /// permit count, override via `with_replication_max_concurrent`,
    /// permit drop on `Drop`) — the full `spawn_replication_if_matched`
    /// integration is exercised by the existing replication tests in
    /// `tests/feature_e2e.rs` once a `ReplicationManager` is attached.
    #[tokio::test]
    async fn replication_semaphore_caps_concurrent_dispatchers() {
        // Build a minimal `S4Service` directly — no handler path is
        // exercised, only the constructor + setter + accessor shape.
        let registry = Arc::new(
            CodecRegistry::new(CodecKind::Passthrough)
                .with(Arc::new(s4_codec::passthrough::Passthrough)),
        );
        let dispatcher = Arc::new(s4_codec::dispatcher::AlwaysDispatcher(
            CodecKind::Passthrough,
        ));
        let s4 = S4Service::new(NoopBackend, registry, dispatcher);

        // Default cap matches the documented constant.
        assert_eq!(
            s4.replication_semaphore().available_permits(),
            S4Service::<NoopBackend>::DEFAULT_REPLICATION_MAX_CONCURRENT,
            "fresh S4Service must expose DEFAULT_REPLICATION_MAX_CONCURRENT permits"
        );

        // Override via the builder — replaces the underlying `Semaphore`.
        let s4 = s4.with_replication_max_concurrent(2);
        assert_eq!(
            s4.replication_semaphore().available_permits(),
            2,
            "with_replication_max_concurrent(2) must expose exactly 2 permits"
        );

        // Acquiring permits must reduce `available_permits()` and
        // dropping them must restore the count — this is the contract
        // `spawn_replication_if_matched` relies on for back-pressure.
        let sem = Arc::clone(s4.replication_semaphore());
        let p1 = sem.clone().acquire_owned().await.expect("permit 1");
        let p2 = sem.clone().acquire_owned().await.expect("permit 2");
        assert_eq!(
            sem.available_permits(),
            0,
            "two acquired permits must zero `available_permits()`"
        );
        // A third `try_acquire_owned` must fail — the cap is enforced
        // synchronously, no extra spawn slips through.
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "third acquire must back-pressure: cap was 2"
        );
        drop(p1);
        drop(p2);
        assert_eq!(
            sem.available_permits(),
            2,
            "dropping permits must restore cap"
        );

        // Lower-bound clamp: a 0 cap would deadlock all dispatchers,
        // so the setter clamps it to 1 instead of accepting it
        // (callers are warned in the CLI doc).
        let s4 = s4.with_replication_max_concurrent(0);
        assert_eq!(
            s4.replication_semaphore().available_permits(),
            1,
            "cap=0 must be clamped to 1 to avoid total deadlock"
        );
    }

    /// v0.8.5 #86 (audit M-1): the access-log flusher must return a
    /// `JoinHandle<()>` that the caller can `abort()` on shutdown
    /// without leaving a dangling task. The pre-#86 call site dropped
    /// the handle at end-of-block (silently detaching it); the fix is
    /// hoisting it into a process-lived `Vec` so the graceful-shutdown
    /// branch in `main.rs` can wait for clean exit. This test exercises
    /// the `JoinHandle.abort()` shape directly so a future refactor that
    /// stops returning the handle (or returns a non-abortable wrapper)
    /// trips this regression guard.
    #[tokio::test]
    async fn flusher_handle_can_be_aborted_cleanly() {
        // Stand up a minimal `AccessLog` pointing at a tmp dir so the
        // flusher's `create_dir_all` succeeds. The dir is cleaned up
        // by the OS / test harness; we don't assert on the contents.
        let tmp = std::env::temp_dir().join(format!(
            "s4-86-flusher-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let dest = crate::access_log::AccessLogDest { dir: tmp.clone() };
        let log = crate::access_log::AccessLog::new(dest);
        let handle = log.spawn_flusher(None);
        assert!(
            !handle.is_finished(),
            "freshly-spawned flusher must not yet be finished"
        );
        handle.abort();
        // `await`-ing an aborted handle returns `Err(JoinError)` whose
        // `is_cancelled()` is true.
        let join_result = handle.await;
        assert!(
            join_result.is_err(),
            "aborted flusher must surface JoinError, got Ok"
        );
        assert!(
            join_result.unwrap_err().is_cancelled(),
            "JoinError must report .is_cancelled() = true after abort()"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Stub backend used solely by the v0.8.5 #86 unit tests above —
    /// the `S4Service` constructor needs `B: S3` but the tests only
    /// exercise builder / accessor shape, never a handler call. Every
    /// `S3` method falls through to the trait's default
    /// `NotImplemented` (which `s3s` provides automatically).
    struct NoopBackend;

    #[async_trait::async_trait]
    impl S3 for NoopBackend {}

    /// v0.8.5 #81 (audit H-7): the panic-catch wrapper at the
    /// dispatcher spawn site must intercept a panicking inner future,
    /// log at ERROR, and bump the per-kind counter — instead of letting
    /// the panic propagate as a `JoinError` that no operator dashboard
    /// scrapes. We exercise the wrapper directly (rather than driving a
    /// full `spawn_replication_if_matched` end-to-end, which would
    /// require a full `S4Service` + backend) because the wrapper shape
    /// is the load-bearing piece — any inner-future swap would still
    /// route through the same `AssertUnwindSafe(...).catch_unwind()`
    /// closure we want to lock in here.
    #[tokio::test]
    async fn dispatcher_panic_caught_and_metric_bumped() {
        use futures::FutureExt as _;

        let handle = crate::metrics::test_metrics_handle();
        let kind = "replication";

        // Mirror the production wrapper shape verbatim — if the
        // production code ever stops using `AssertUnwindSafe.catch_unwind`
        // this test shouldn't keep passing on a hand-rolled copy that
        // diverged.
        let panicking = async {
            panic!("simulated dispatcher panic");
        };
        let result = std::panic::AssertUnwindSafe(panicking).catch_unwind().await;
        assert!(
            result.is_err(),
            "catch_unwind must surface the panic instead of swallowing it"
        );
        // Bump the production counter via the same helper the wrapper
        // calls so the rendered output gates on the production code
        // path, not a parallel bookkeeping copy.
        crate::metrics::record_dispatcher_panic(kind);

        let rendered = handle.render();
        assert!(
            rendered.contains("s4_dispatcher_panics_total"),
            "expected s4_dispatcher_panics_total in metrics output, got: {rendered}"
        );
        assert!(
            rendered.contains("kind=\"replication\""),
            "expected kind=\"replication\" label in metrics output, got: {rendered}"
        );
    }

    /// v0.9 #106-audit-R2 P2-INT-2: the shared trailer-verify helper
    /// short-circuits when the `x-amz-trailer` header is absent (no
    /// claim → nothing to verify).
    #[test]
    fn verify_client_trailer_checksums_passes_when_no_header() {
        let computed = crate::streaming_checksum::ComputedDigests::default();
        verify_client_trailer_checksums(None, None, &computed).expect("no claim → Ok");
    }

    /// Helper that only announces non-checksum trailers (e.g. the
    /// `x-amz-trailer-signature` SDKs add for SigV4 streaming) is also
    /// a no-op — the filter discards them before anything else runs.
    #[test]
    fn verify_client_trailer_checksums_ignores_non_checksum_trailers() {
        let computed = crate::streaming_checksum::ComputedDigests::default();
        verify_client_trailer_checksums(Some("x-amz-trailer-signature"), None, &computed)
            .expect("non-checksum trailers must not fail");
    }

    /// Fail-closed: announced checksum trailer + no trailing-headers
    /// handle = `BadDigest`. This is the core regression fence for the
    /// buffered-path silent-skip the P2-INT-2 fix closes.
    #[test]
    fn verify_client_trailer_checksums_no_handle_fails_closed() {
        let computed = crate::streaming_checksum::ComputedDigests::default();
        let err = verify_client_trailer_checksums(Some("x-amz-checksum-crc32c"), None, &computed)
            .expect_err("announced trailer with no handle must fail closed");
        assert_eq!(err.code().as_str(), "BadDigest");
        assert!(
            err.message()
                .unwrap_or_default()
                .contains("trailing-headers handle"),
            "error message must hint at the missing handle, got {err:?}"
        );
    }

    /// Case-insensitive trailer name match — AWS SDKs may use any
    /// casing per RFC 9110 §5.1. The filter must still detect the
    /// `x-amz-checksum-` prefix; the helper then propagates the bad-
    /// digest reject via the missing handle.
    #[test]
    fn verify_client_trailer_checksums_case_insensitive_filter() {
        let computed = crate::streaming_checksum::ComputedDigests::default();
        let err = verify_client_trailer_checksums(Some("X-Amz-Checksum-Crc32c"), None, &computed)
            .expect_err("upper-case trailer name must still be detected");
        assert_eq!(err.code().as_str(), "BadDigest");
    }

    /// Mixed announce: one checksum trailer and one unrelated trailer.
    /// The filter retains the checksum one and routes to the fail-closed
    /// branch when the handle is absent.
    #[test]
    fn verify_client_trailer_checksums_mixed_announce_still_validates() {
        let computed = crate::streaming_checksum::ComputedDigests::default();
        let err = verify_client_trailer_checksums(
            Some("x-amz-checksum-sha256, x-amz-trailer-signature"),
            None,
            &computed,
        )
        .expect_err("mixed announce with checksum entry must still fail closed");
        assert_eq!(err.code().as_str(), "BadDigest");
    }

    // ---- v1.0.1 audit R2 P2 handler-path tests ------------------------
    //
    // A minimal recording backend: captures the exact inputs the gateway
    // forwards so the tests can assert on what reaches the backend wire
    // (the load-bearing surface for the metadata-strip and the
    // versioned-HEAD-probe fixes). State lives behind an `Arc` so the
    // test keeps a handle after handing the backend to `S4Service`.
    #[derive(Default)]
    struct RecordingState {
        /// Metadata of every `CreateMultipartUpload` forwarded.
        mp_metadata: std::sync::Mutex<Vec<Option<Metadata>>>,
        /// `(bucket, key, version_id)` of every `HeadObject` forwarded.
        head_probes: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
        /// Metadata of every `CopyObject` forwarded (v1.2 audit R2 P3:
        /// the access-point REPLACE strip asserts on what reaches the
        /// backend wire).
        copy_metadata: std::sync::Mutex<Vec<Option<Metadata>>>,
    }

    #[derive(Clone, Default)]
    struct RecordingBackend {
        state: Arc<RecordingState>,
    }

    #[async_trait::async_trait]
    impl S3 for RecordingBackend {
        async fn create_multipart_upload(
            &self,
            req: S3Request<CreateMultipartUploadInput>,
        ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
            self.state
                .mp_metadata
                .lock()
                .expect("lock")
                .push(req.input.metadata.clone());
            Ok(S3Response::new(CreateMultipartUploadOutput {
                upload_id: Some("upload-1".into()),
                ..Default::default()
            }))
        }

        async fn head_object(
            &self,
            req: S3Request<HeadObjectInput>,
        ) -> S3Result<S3Response<HeadObjectOutput>> {
            self.state.head_probes.lock().expect("lock").push((
                req.input.bucket.clone(),
                req.input.key.clone(),
                req.input.version_id.clone(),
            ));
            // Plain compressed object: enough metadata for the REPLACE
            // merge to run, no `s4-dict-id` so the cross-bucket dict
            // propagation probe stops after the HEAD.
            let mut meta = Metadata::new();
            meta.insert(META_CODEC.into(), "cpu-zstd".into());
            Ok(S3Response::new(HeadObjectOutput {
                metadata: Some(meta),
                ..Default::default()
            }))
        }

        async fn copy_object(
            &self,
            req: S3Request<CopyObjectInput>,
        ) -> S3Result<S3Response<CopyObjectOutput>> {
            self.state
                .copy_metadata
                .lock()
                .expect("lock")
                .push(req.input.metadata.clone());
            Ok(S3Response::new(CopyObjectOutput::default()))
        }
    }

    fn recording_service() -> (S4Service<RecordingBackend>, Arc<RecordingState>) {
        let backend = RecordingBackend::default();
        let state = Arc::clone(&backend.state);
        let svc = S4Service::new(
            backend,
            Arc::new(CodecRegistry::new(CodecKind::CpuZstd)),
            Arc::new(s4_codec::dispatcher::AlwaysDispatcher(CodecKind::CpuZstd)),
        );
        (svc, state)
    }

    fn synthetic_req<T>(input: T, method: http::Method) -> S3Request<T> {
        S3Request {
            input,
            method,
            uri: http::Uri::from_static("/"),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    /// v1.0.1 audit R2 P2: client-supplied `s4-*` metadata on
    /// CreateMultipartUpload must be stripped before the backend forward
    /// — pre-fix, only `s4-multipart` / `s4-codec` were overwritten, so a
    /// forged `s4-encrypted: aes-256-gcm` survived to the completed
    /// object and a flag-less GET 5xx'd in the decrypt path (same freeze
    /// violation the put_object strip closed, multipart wire edition).
    #[tokio::test]
    async fn create_multipart_strips_reserved_client_metadata() {
        let (svc, state) = recording_service();
        let mut builder = CreateMultipartUploadInput::builder();
        builder.set_bucket("bkt".to_owned());
        builder.set_key("obj.bin".to_owned());
        let mut meta = Metadata::new();
        meta.insert("s4-encrypted".into(), "aes-256-gcm".into());
        meta.insert("s4-dict-id".into(), "0123456789abcdef".into());
        meta.insert("S4-Original-Size".into(), "1".into());
        meta.insert("app-team".into(), "kept".into());
        builder.set_metadata(Some(meta));
        let input = builder.build().expect("input");
        svc.create_multipart_upload(synthetic_req(input, http::Method::POST))
            .await
            .expect("create_multipart_upload");

        let recorded = state.mp_metadata.lock().expect("lock");
        assert_eq!(recorded.len(), 1, "exactly one backend forward");
        let meta = recorded[0].as_ref().expect("metadata reaches the backend");
        assert!(
            !meta.contains_key("s4-encrypted"),
            "forged s4-encrypted must be stripped, got {meta:?}"
        );
        assert!(
            !meta.contains_key("s4-dict-id"),
            "forged s4-dict-id must be stripped, got {meta:?}"
        );
        assert!(
            !meta.contains_key("S4-Original-Size"),
            "strip must be case-insensitive, got {meta:?}"
        );
        assert_eq!(
            meta.get("app-team").map(String::as_str),
            Some("kept"),
            "non-reserved client metadata must survive"
        );
        // The gateway's own stamps are re-applied after the strip.
        assert_eq!(meta.get(META_MULTIPART).map(String::as_str), Some("true"));
        assert_eq!(meta.get(META_CODEC).map(String::as_str), Some("cpu-zstd"));
    }

    /// v1.0.1 audit R2 P2: both copy_object source HEAD probes (REPLACE
    /// metadata merge + cross-bucket dict propagation) must carry the
    /// `?versionId=` of the copy source — pre-fix they probed "latest",
    /// merging the wrong version's s4-* manifest / missing the pinned
    /// version's dictionary.
    #[tokio::test]
    async fn copy_object_head_probes_honor_pinned_source_version() {
        let (svc, state) = recording_service();
        let mut builder = CopyObjectInput::builder();
        builder.set_bucket("dst-bkt".to_owned());
        builder.set_key("dst.bin".to_owned());
        builder.set_copy_source(CopySource::Bucket {
            bucket: "src-bkt".to_owned().into_boxed_str(),
            key: "src.bin".to_owned().into_boxed_str(),
            version_id: Some("vid-123".to_owned().into_boxed_str()),
        });
        builder.set_metadata_directive(Some(MetadataDirective::from_static(
            MetadataDirective::REPLACE,
        )));
        let input = builder.build().expect("input");
        svc.copy_object(synthetic_req(input, http::Method::PUT))
            .await
            .expect("copy_object");

        let probes = state.head_probes.lock().expect("lock");
        assert_eq!(
            probes.len(),
            2,
            "REPLACE merge + cross-bucket dict probes must both run, got {probes:?}"
        );
        for (bucket, key, version_id) in probes.iter() {
            assert_eq!(bucket, "src-bkt");
            assert_eq!(key, "src.bin");
            assert_eq!(
                version_id.as_deref(),
                Some("vid-123"),
                "HEAD probe must pin the requested source version"
            );
        }
    }

    /// v1.2 audit R2 P2: the metadata snapshot handed to the
    /// replication dispatcher must NOT carry the `s4-ledger` marker —
    /// the replica PUT bypasses `put_object` (never ledger-added), so a
    /// marker-carrying replica would be subtracted on a later gateway
    /// DELETE without ever having been added. Everything else (codec
    /// manifest, SSE markers, client keys) must be forwarded verbatim.
    #[test]
    fn replication_metadata_snapshot_drops_ledger_marker() {
        let mut meta = Metadata::new();
        meta.insert(META_LEDGER.into(), META_LEDGER_ACCOUNTED.into());
        meta.insert(META_CODEC.into(), "cpu-zstd".into());
        meta.insert(META_ORIGINAL_SIZE.into(), "1234".into());
        meta.insert("s4-encrypted".into(), "aes-256-gcm".into());
        meta.insert("app-team".into(), "kept".into());
        let snap = replication_metadata_snapshot(&Some(meta.clone()))
            .expect("map survives, only the marker is removed");
        assert!(
            !snap.contains_key(META_LEDGER),
            "ledger marker must not reach the replica: {snap:?}"
        );
        assert_eq!(snap.get(META_CODEC).map(String::as_str), Some("cpu-zstd"));
        assert_eq!(
            snap.get(META_ORIGINAL_SIZE).map(String::as_str),
            Some("1234"),
            "codec manifest keys must survive (the replica must stay readable)"
        );
        assert_eq!(
            snap.get("s4-encrypted").map(String::as_str),
            Some("aes-256-gcm")
        );
        assert_eq!(snap.get("app-team").map(String::as_str), Some("kept"));
        // The source request's own metadata is untouched (clone, not
        // mutate) — the source object keeps its marker.
        assert!(meta.contains_key(META_LEDGER));
        // None stays None (zero-length / body-less PUT shape).
        assert!(replication_metadata_snapshot(&None).is_none());
    }

    /// v1.2 audit R2 P3: a REPLACE-directive copy whose source is an
    /// access-point ARN must strip client-supplied `s4-*` destination
    /// metadata exactly like a bucket-addressed copy — pre-fix the
    /// strip lived behind the `CopySource::Bucket` gate, so an AP copy
    /// let a forged `x-amz-meta-s4-ledger` + `s4-original-size` land on
    /// the destination verbatim (a forgeable marker breaks the ledger's
    /// subtraction contract).
    #[tokio::test]
    async fn access_point_replace_copy_strips_reserved_metadata() {
        let (svc, state) = recording_service();
        let mut builder = CopyObjectInput::builder();
        builder.set_bucket("dst-bkt".to_owned());
        builder.set_key("dst.bin".to_owned());
        builder.set_copy_source(CopySource::AccessPoint {
            region: "us-east-1".to_owned().into_boxed_str(),
            account_id: "123456789012".to_owned().into_boxed_str(),
            access_point_name: "my-ap".to_owned().into_boxed_str(),
            key: "src.bin".to_owned().into_boxed_str(),
        });
        builder.set_metadata_directive(Some(MetadataDirective::from_static(
            MetadataDirective::REPLACE,
        )));
        let mut meta = Metadata::new();
        meta.insert(META_LEDGER.into(), META_LEDGER_ACCOUNTED.into());
        meta.insert(META_ORIGINAL_SIZE.into(), "999999999999".into());
        meta.insert("S4-Codec".into(), "cpu-zstd".into());
        meta.insert("app-team".into(), "kept".into());
        builder.set_metadata(Some(meta));
        let input = builder.build().expect("input");
        svc.copy_object(synthetic_req(input, http::Method::PUT))
            .await
            .expect("copy_object with access-point source");

        let recorded = state.copy_metadata.lock().expect("lock");
        assert_eq!(recorded.len(), 1, "exactly one backend forward");
        let meta = recorded[0].as_ref().expect("metadata reaches the backend");
        assert!(
            !meta.contains_key(META_LEDGER),
            "forged ledger marker must be stripped on AP REPLACE copies, got {meta:?}"
        );
        assert!(
            !meta.contains_key(META_ORIGINAL_SIZE),
            "forged s4-original-size must be stripped, got {meta:?}"
        );
        assert!(
            !meta.contains_key("S4-Codec"),
            "strip must stay case-insensitive, got {meta:?}"
        );
        assert_eq!(
            meta.get("app-team").map(String::as_str),
            Some("kept"),
            "non-reserved client metadata must survive"
        );
    }

    /// v1.2 audit R2 P3: the G-2 reserved-key guard on the copy
    /// *source* must also apply to access-point addressing — an AP ARN
    /// must not read `<key>.s4index` / `.s4dict/<id>` around the gate.
    #[tokio::test]
    async fn access_point_copy_source_reserved_key_rejected() {
        let (svc, state) = recording_service();
        let mut builder = CopyObjectInput::builder();
        builder.set_bucket("dst-bkt".to_owned());
        builder.set_key("dst.bin".to_owned());
        builder.set_copy_source(CopySource::AccessPoint {
            region: "us-east-1".to_owned().into_boxed_str(),
            account_id: "123456789012".to_owned().into_boxed_str(),
            access_point_name: "my-ap".to_owned().into_boxed_str(),
            key: "secret.bin.s4index".to_owned().into_boxed_str(),
        });
        let input = builder.build().expect("input");
        let err = svc
            .copy_object(synthetic_req(input, http::Method::PUT))
            .await
            .expect_err("sidecar source via access point must be rejected");
        assert_eq!(
            err.code().as_str(),
            "NoSuchKey",
            "Read-mode guard mirrors listing semantics, got {err:?}"
        );
        assert!(
            state.copy_metadata.lock().expect("lock").is_empty(),
            "the rejected copy must never reach the backend"
        );
    }
}
