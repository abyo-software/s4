//! v1.1: `s4 migrate` — bulk retro-compression of pre-existing objects.
//!
//! Walks a bucket (or prefix) that predates the S4 gateway and rewrites
//! each plain object into the same S4F2 framed format the gateway's PUT
//! path produces, so later GETs through the gateway decompress them
//! transparently and the backend bill shrinks. Per object:
//!
//! 1. A 4-byte `Range` GET probes for the `S4F2` / `S4P1` frame magic
//!    and for the `s4-codec` metadata key — anything the gateway (or a
//!    previous migrate run) already wrote is skipped, which is also why
//!    a re-run resumes automatically with no checkpoint file.
//! 2. The full body is fetched (metadata + Content-Type captured), the
//!    **same** [`SamplingDispatcher`] decision the gateway runs at PUT
//!    time picks a codec, and the body is framed via the **same**
//!    [`streaming_compress_to_frames`] + [`pick_chunk_size`] pair the
//!    gateway's streaming PUT path calls.
//! 3. The framed bytes are decompressed back in-process and compared
//!    byte-for-byte against the original. **No verify, no write** —
//!    this tool overwrites user data, so the roundtrip check has no
//!    off switch.
//! 4. A `HEAD` re-checks the source ETag immediately before the PUT;
//!    a mismatch (concurrent writer) skips the object. This narrows
//!    but does NOT close the race window — S3 has no compare-and-swap,
//!    so a writer landing between the HEAD and the PUT is still lost.
//! 5. Multi-frame results get the same `<key>.s4index` sidecar the
//!    gateway writes (ETag + size binding stamped from the PUT
//!    response), so Range GETs keep the partial-fetch fast path.
//!    Single-frame results delete any stale multi-frame sidecar a
//!    previous shape of the object left behind (HEAD-then-DELETE,
//!    same policy as `s4 recompact`).
//!
//! ## Dry-run by default
//!
//! Like `sweep-orphan-sidecars`, the default mode only reports what
//! *would* be migrated (it still GETs + compresses + verifies, so the
//! projected sizes are measured, not estimated). Pass `--execute` to
//! write.
//!
//! ## Honesty constraints
//!
//! - **GPU codecs are never executed here.** When the dispatcher's pick
//!   is a GPU (`nvcomp-*` / `dietgpu-*`) or non-streaming CPU kind
//!   (`cpu-gzip`), migrate **really falls back to `cpu-zstd`** at the
//!   server's `--zstd-level` — the same direction a non-GPU gateway
//!   build takes — and the report's codec breakdown shows
//!   `picked != wrote_with` plus an explicit note. Frames are
//!   self-describing, so a GPU gateway reads the cpu-zstd frames fine.
//! - **SSE deployments are out of scope** (the CLI refuses the SSE
//!   flags before this module runs): encrypted bodies need the
//!   gateway's keyring plumbing — route writes through a running
//!   gateway instead.
//! - **Versioned buckets work but double-bill**: the overwrite PUT
//!   leaves the old (uncompressed) version in place. The report warns
//!   when `GetBucketVersioning` says `Enabled`.
//! - **Storage class and object tags are carried over** on the rewrite
//!   PUT (the GET's storage class is re-sent unless it is
//!   absent/STANDARD; tags come from `GetObjectTagging`). When the
//!   tagging read fails with an AccessDenied / NotImplemented /
//!   NotSupported-class error (credential without `s3:GetObjectTagging`,
//!   backend without tagging), the object is **skipped**
//!   (`tags-unreadable`) instead of rewritten-with-tags-stripped or
//!   hard-failed; pass `--no-tags` to opt out of the tagging read and
//!   rewrite anyway (tags are then NOT preserved). **ACLs and
//!   Object Lock retention/legal-hold are NOT carried over** — the
//!   overwrite PUT resets them to the bucket defaults. Buckets that
//!   rely on per-object ACLs or Object Lock must not be migrated with
//!   this tool; the report repeats this in `notes`.
//! - **Internal keys are never touched**: `.s4index` sidecars,
//!   `.s4dict/` shared dictionaries and `.__s4ver__/` versioning
//!   shadow keys are excluded from the listing — re-compressing any of
//!   them would corrupt dictionary GETs or version restores.

use std::collections::BTreeMap;
use std::sync::Arc;

use aws_sdk_s3::Client;
use bytes::{Bytes, BytesMut};
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::{AlwaysDispatcher, SamplingDispatcher};
use s4_codec::index::{SIDECAR_SUFFIX, build_index_from_body, encode_index, sidecar_key};
use s4_codec::multipart::{FRAME_MAGIC, FrameIter, PADDING_MAGIC};
use s4_codec::passthrough::Passthrough;
use s4_codec::{ChunkManifest, CodecDispatcher, CodecKind, CodecRegistry};
use serde::Serialize;
use thiserror::Error;

use crate::service::{
    META_CODEC, META_COMPRESSED_SIZE, META_CRC32C, META_FRAMED, META_ORIGINAL_SIZE,
};
use crate::streaming::{pick_chunk_size, streaming_compress_to_frames};

/// Default `--concurrency`: 4 objects in flight. Each in-flight object
/// holds its body + framed output in RAM, so the worst-case peak is
/// `concurrency × 2 × max_body_bytes` — keep this low by default.
pub const DEFAULT_MIGRATE_CONCURRENCY: usize = 4;

/// Dispatcher decision sample size. MUST stay in sync with the server's
/// PUT path (`service.rs` private `SAMPLE_BYTES = 4096`) and with
/// `estimate.rs`, so migrate picks the same codec the gateway would
/// pick at runtime.
const DISPATCH_SAMPLE_BYTES: usize = 4096;

/// Bytes fetched by the already-S4 magic probe (`Range: bytes=0-3`).
const MAGIC_PROBE_BYTES: usize = 4;

/// v1.1 stability: `#[non_exhaustive]` — new migrate-time failure modes
/// may be added in minor releases. Downstream callers must include a
/// `_ =>` arm when matching on this enum.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MigrateError {
    #[error("S3 backend error on {op} {bucket}/{key}: {cause}")]
    Backend {
        op: &'static str,
        bucket: String,
        // Empty for bucket-level ops (ListObjectsV2 / GetBucketVersioning).
        key: String,
        // Named `cause` (not `source`) so thiserror doesn't auto-treat it
        // as a `#[source]` chain field — same convention as
        // `repair::RepairError` and `estimate::EstimateError`.
        cause: String,
    },
    #[error("invalid migrate target {target:?}: {reason}")]
    InvalidTarget { target: String, reason: String },
}

/// Why one object was left untouched. `#[non_exhaustive]`: new skip
/// classes may be added in minor releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SkipReason {
    /// Body already starts with `S4F2` / `S4P1`, or carries the
    /// `s4-codec` metadata stamp (gateway- or migrate-written).
    AlreadyS4,
    /// Dispatcher picked passthrough, the framed output was not
    /// smaller than the input, or the object is empty.
    NotCompressible,
    /// Listed (or fetched) size exceeds `max_body_bytes`.
    TooLarge,
    /// The ETag changed between the GET and the pre-PUT HEAD —
    /// a concurrent writer won; nothing was overwritten.
    EtagRaced,
    /// Deprecated since v1.0.1: a roundtrip-verify failure on bytes we
    /// just framed ourselves is a bug, so it is now reported as a hard
    /// failure (`failures[].op == "verify"`, exit 1) — same policy as
    /// `recompact`. The variant is kept (the enum is `#[non_exhaustive]`
    /// and the `skipped_verify_failed` JSON field stays, always `0`) so
    /// downstream JSON consumers keep parsing.
    VerifyFailed,
    /// `GetObjectTagging` failed with an AccessDenied / NotImplemented /
    /// NotSupported-class error: the object may carry tags we cannot
    /// read, so rewriting it would silently strip them. Grant
    /// `s3:GetObjectTagging` or pass `--no-tags` to rewrite without
    /// preserving tags. Other tagging errors (transient faults) stay
    /// hard failures.
    TagsUnreadable,
}

/// Knobs for one migrate run. The codec-selection half mirrors the
/// server's flags (`--codec` / `--dispatcher` / `--zstd-level` /
/// `--gpu-min-bytes` / `--prefer-columnar-gpu`) so the pick matches
/// what the deployed gateway would choose at PUT time.
#[derive(Debug, Clone)]
pub struct MigrateParams {
    /// Restrict the listing to keys under this prefix.
    pub prefix: Option<String>,
    /// `false` (default) = dry-run: GET + compress + verify but never
    /// PUT. `true` = actually rewrite objects (and sidecars).
    pub execute: bool,
    /// Objects processed in parallel. Values below 1 are clamped to 1.
    pub concurrency: usize,
    /// Stop listing after this many non-sidecar objects (`None` = no
    /// cap). Objects beyond the cap are not touched and the report
    /// flags the truncation.
    pub max_objects: Option<usize>,
    /// Per-object body cap; larger objects are skipped (`TooLarge`).
    pub max_body_bytes: u64,
    /// Server `--codec` (the dispatcher's default pick).
    pub default_codec: CodecKind,
    /// Server `--zstd-level` (used for the actual cpu-zstd writes,
    /// including the GPU/gzip fallback path).
    pub zstd_level: i32,
    /// Server `--dispatcher`: `true` = sampling, `false` = always.
    pub use_sampling_dispatcher: bool,
    /// Server `--gpu-min-bytes`.
    pub gpu_min_bytes: usize,
    /// Server `--prefer-columnar-gpu`.
    pub prefer_columnar_gpu: bool,
    /// Real GPU probe result for this build/host. Only used to make the
    /// dispatcher's *pick* match a GPU gateway; the bytes written are
    /// always CPU (`cpu-zstd`) regardless.
    pub gpu_present: bool,
    /// `--no-tags`: skip the `GetObjectTagging` read entirely and
    /// rewrite without carrying tags over. Explicit opt-out for
    /// credentials/backends where tagging reads are denied or
    /// unimplemented (objects otherwise skip as `tags-unreadable`).
    /// WARNING: any existing object tags are NOT preserved on
    /// rewritten objects.
    pub no_tags: bool,
}

/// One `picked → wrote_with` codec pairing and how many migrated
/// objects took it. `picked != wrote_with` marks the real-fallback
/// path (GPU / gzip picks written as cpu-zstd).
#[derive(Debug, Clone, Serialize)]
pub struct CodecMigrateCount {
    /// Codec the deployed gateway's dispatcher would choose.
    pub picked: String,
    /// Codec the frames were actually written with.
    pub wrote_with: String,
    pub objects: u64,
}

/// One per-object hard failure (the object was left as-is unless the
/// `op` says otherwise — see the sidecar note in [`run_migrate`]).
#[derive(Debug, Clone, Serialize)]
pub struct MigrateFailure {
    pub key: String,
    pub op: String,
    pub cause: String,
}

/// Full result of one migrate run. Serializes to the `--format json`
/// output verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct MigrateReport {
    pub bucket: String,
    pub prefix: Option<String>,
    /// `true` = nothing was written (default mode).
    pub dry_run: bool,
    /// Objects listed (after `.s4index` / `.s4dict/` / `.__s4ver__/`
    /// exclusion, before any skip).
    pub total_objects: u64,
    pub total_bytes: u64,
    /// `true` when listing stopped at `max_objects` with more keys
    /// remaining — those keys were not examined at all.
    pub listing_truncated: bool,
    pub max_objects: Option<usize>,
    /// Objects migrated (dry-run: objects that *would* be migrated;
    /// the sizes below are measured on the real compressed output
    /// either way).
    pub migrated: u64,
    pub migrated_bytes_before: u64,
    pub migrated_bytes_after: u64,
    pub skipped_already_s4: u64,
    pub skipped_not_compressible: u64,
    pub skipped_too_large: u64,
    pub skipped_etag_raced: u64,
    pub skipped_verify_failed: u64,
    /// Objects skipped because `GetObjectTagging` failed with an
    /// AccessDenied / NotImplemented / NotSupported-class error (see
    /// [`SkipReason::TagsUnreadable`]). Always `0` with `--no-tags`.
    pub skipped_tags_unreadable: u64,
    /// `true` when the run was made with `--no-tags` (tags neither read
    /// nor carried over).
    pub no_tags: bool,
    pub failed: u64,
    /// Per-key failure details, sorted by key.
    pub failures: Vec<MigrateFailure>,
    /// Codec breakdown of the migrated set.
    pub codecs: Vec<CodecMigrateCount>,
    /// `GetBucketVersioning` said `Enabled` (double-billing warning in
    /// `notes`). `false` covers Suspended / never-enabled / unknown.
    pub versioning_enabled: bool,
    /// Fixed honesty notes + run-specific caveats. Always read these
    /// before quoting the numbers anywhere.
    pub notes: Vec<String>,
}

/// `true` when the prefix bytes carry an S4 frame magic (`S4F2` data
/// frame or `S4P1` padding frame) — i.e. the object is already in S4
/// format and must not be re-compressed.
pub fn is_s4_frame_prefix(prefix: &[u8]) -> bool {
    prefix.len() >= 4
        && (&prefix[..4] == FRAME_MAGIC.as_slice() || &prefix[..4] == PADDING_MAGIC.as_slice())
}

/// Map the dispatcher's runtime pick to the codec migrate actually
/// writes with. Migrate is CPU-only and only emits streaming-framable
/// codecs, so anything that is not `cpu-zstd` (GPU kinds, `cpu-gzip`,
/// any future kind — the enum is `#[non_exhaustive]`) **really falls
/// back** to `cpu-zstd`. `Passthrough` is returned unchanged because
/// the caller skips those objects before writing.
pub fn write_kind(pick: CodecKind) -> CodecKind {
    match pick {
        CodecKind::Passthrough | CodecKind::CpuZstd => pick,
        _ => CodecKind::CpuZstd,
    }
}

/// Pure size gate run before any network call: empty objects have
/// nothing to compress, over-cap objects are skipped (we'd have to
/// buffer them whole for the verify step).
fn size_precheck(size: u64, max_body_bytes: u64) -> Option<SkipReason> {
    if size == 0 {
        Some(SkipReason::NotCompressible)
    } else if size > max_body_bytes {
        Some(SkipReason::TooLarge)
    } else {
        None
    }
}

/// Strip surrounding quotes from a wire ETag — the sidecar stores the
/// normalized form (same canonical form `repair::normalize_etag` and
/// the gateway's `write_sidecar` use). `pub(crate)` for `recompact`,
/// which needs the identical canonical form; behaviour unchanged.
pub(crate) fn normalize_etag(s: &str) -> String {
    s.trim_matches('"').to_owned()
}

/// `true` for `.__s4ver__/` versioning shadow keys — the backend-side
/// storage the gateway uses for non-current versions on
/// versioning-Enabled buckets (`service::versioned_shadow_key`). The
/// offline tools must exclude them exactly like the gateway's listing
/// filter does: rewriting a shadow key would break the version-restore
/// path, and counting one would skew the estimate. `pub(crate)` for
/// `estimate` / `recompact`, which apply the same exclusion.
///
/// Known over-exclusion: this is a plain substring check, so a
/// *customer* object whose key merely contains the literal
/// `.__s4ver__/` anywhere is excluded from the offline tools too (it
/// is never examined, migrated, recompacted, or counted). This is the
/// same blind spot as the gateway's own listing filter — the
/// `.__s4ver__/` namespace collision is a known, documented
/// limitation, not specific to these tools.
pub(crate) fn is_versioning_shadow_key(key: &str) -> bool {
    key.contains(".__s4ver__/")
}

/// `true` for keys the offline tools (estimate / migrate / recompact)
/// must never list as work items: `.s4index` sidecars, `.s4dict/`
/// shared dictionaries, `.__s4ver__/` versioning shadow keys, and
/// `.s4mpu/` durable multipart part-state records. `pub(crate)` for
/// `estimate` / `recompact`.
pub(crate) fn is_internal_key(key: &str) -> bool {
    key.ends_with(SIDECAR_SUFFIX)
        || crate::dict::is_dict_key(key)
        || is_versioning_shadow_key(key)
        || crate::mpu_durable::is_mpu_state_key(key)
}

#[derive(Debug, Clone)]
struct ObjectJob {
    key: String,
    size: u64,
    /// Backend `LastModified` as epoch seconds (`None` when the listing
    /// omitted it). Only consulted by the v1.2 `s4 maintain` path
    /// ([`run_migrate_with_cutoff`]); plain `s4 migrate` ignores it.
    last_modified_epoch_secs: Option<i64>,
}

struct Inventory {
    objects: Vec<ObjectJob>,
    truncated: bool,
}

/// Paginate `ListObjectsV2`, skipping internal keys (`.s4index`
/// sidecars, `.s4dict/` dictionaries, `.__s4ver__/` version shadows),
/// stopping at `max_objects` collected keys. Same pagination shape as
/// `estimate::list_inventory` / `repair::sweep_orphan_sidecars`.
async fn list_inventory(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    max_objects: Option<usize>,
) -> Result<Inventory, MigrateError> {
    let mut objects: Vec<ObjectJob> = Vec::new();
    let mut truncated = false;
    let mut continuation: Option<String> = None;
    'pages: loop {
        let mut req = client.list_objects_v2().bucket(bucket);
        if let Some(p) = prefix {
            req = req.prefix(p);
        }
        if let Some(c) = continuation.as_ref() {
            req = req.continuation_token(c);
        }
        let resp = req.send().await.map_err(|e| MigrateError::Backend {
            op: "ListObjectsV2",
            bucket: bucket.into(),
            key: String::new(),
            cause: format!("{e}"),
        })?;
        for obj in resp.contents() {
            let Some(k) = obj.key() else { continue };
            if is_internal_key(k) {
                continue;
            }
            if let Some(cap) = max_objects
                && objects.len() >= cap
            {
                truncated = true;
                break 'pages;
            }
            let size = obj.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0);
            objects.push(ObjectJob {
                key: k.to_owned(),
                size,
                last_modified_epoch_secs: obj.last_modified().map(|t| t.secs()),
            });
        }
        if resp.is_truncated().unwrap_or(false) {
            continuation = resp.next_continuation_token().map(str::to_owned);
            if continuation.is_none() {
                // Defensive: truncated response without a continuation
                // token is a backend bug; bail rather than infinite-loop.
                break;
            }
        } else {
            break;
        }
    }
    Ok(Inventory { objects, truncated })
}

/// URL-encode one `key=value` tag set as the `x-amz-tagging` PUT header
/// expects (`k1=v1&k2=v2`, both halves percent-encoded). Uses the same
/// AWS canonical set the SigV4 path uses (everything but unreserved
/// characters), which is a superset of what the header needs — safe,
/// never under-encoded. `pub(crate)` for `recompact`.
pub(crate) fn encode_tagging(tags: &[(String, String)]) -> String {
    const ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    tags.iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                percent_encoding::utf8_percent_encode(k, ENCODE_SET),
                percent_encoding::utf8_percent_encode(v, ENCODE_SET),
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// How a failed `GetObjectTagging` call is handled. `pub(crate)` for
/// `recompact` (same skip-vs-fail policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagErrorClass {
    /// The backend says the object simply has no tag set (`NoSuchTagSet`
    /// — AWS returns an empty set instead, but some backends 404 with
    /// this code). Safe to treat as "no tags": nothing can be lost.
    NoTags,
    /// AccessDenied / NotImplemented / NotSupported class: the object
    /// may carry tags we are not allowed (or able) to read. The caller
    /// skips the object (`tags-unreadable`) instead of stripping tags
    /// or hard-failing the whole class.
    Unreadable,
    /// Anything else (throttling, 5xx, transport faults): hard failure,
    /// same as before — a transient error must not be mistaken for
    /// "this backend has no tagging".
    Other,
}

/// Classify a failed `GetObjectTagging` by S3 error code (preferred)
/// or, when the response carried no modeled code, by raw HTTP status.
/// Split out of [`fetch_tags`] so the classification is unit-testable
/// without a network. `pub(crate)` for `recompact`.
pub(crate) fn classify_tagging_error(
    code: Option<&str>,
    http_status: Option<u16>,
) -> TagErrorClass {
    match code {
        Some("NoSuchTagSet") => TagErrorClass::NoTags,
        Some(
            "AccessDenied"
            | "NotImplemented"
            | "NotSupported"
            | "MethodNotAllowed"
            | "OperationNotSupported",
        ) => TagErrorClass::Unreadable,
        // Any other modeled code (NoSuchKey, SlowDown, InternalError, …)
        // is a real per-object failure.
        Some(_) => TagErrorClass::Other,
        // Code-less responses: classify by status. 403 = denied,
        // 405/501 = tagging not implemented on this backend. A raw 404
        // is deliberately NOT NoTags — without a code it may be
        // NoSuchKey (concurrently deleted object) and rewriting would
        // resurrect it.
        None => match http_status {
            Some(403 | 405 | 501) => TagErrorClass::Unreadable,
            _ => TagErrorClass::Other,
        },
    }
}

/// A failed (and not `NoSuchTagSet`) `GetObjectTagging`, pre-classified
/// for the skip-vs-fail decision. `pub(crate)` for `recompact`.
#[derive(Debug, Clone)]
pub(crate) struct TagFetchError {
    /// `true` = [`TagErrorClass::Unreadable`] (skip as
    /// `tags-unreadable`); `false` = hard failure.
    pub unreadable: bool,
    pub cause: String,
}

/// `GetObjectTagging` → the object's tag set as `(key, value)` pairs.
/// Carried over verbatim on the rewrite PUT so retro-compression does
/// not silently strip lifecycle/billing tags. A `NoSuchTagSet` error
/// is folded into `Ok(vec![])` (some backends 404 instead of returning
/// an empty set); other failures come back classified — see
/// [`TagErrorClass`]. `pub(crate)` for `recompact` (same carry-over).
pub(crate) async fn fetch_tags(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<Vec<(String, String)>, TagFetchError> {
    match client
        .get_object_tagging()
        .bucket(bucket)
        .key(key)
        .send()
        .await
    {
        Ok(resp) => Ok(resp
            .tag_set()
            .iter()
            .map(|t| (t.key().to_owned(), t.value().to_owned()))
            .collect()),
        Err(e) => {
            use aws_sdk_s3::error::ProvideErrorMetadata as _;
            let code = e.code().map(str::to_owned);
            let status = e.raw_response().map(|r| r.status().as_u16());
            match classify_tagging_error(code.as_deref(), status) {
                TagErrorClass::NoTags => Ok(Vec::new()),
                class => Err(TagFetchError {
                    unreadable: class == TagErrorClass::Unreadable,
                    cause: match code {
                        Some(c) => format!("{c}: {e}"),
                        None => format!("{e}"),
                    },
                }),
            }
        }
    }
}

/// `GetBucketVersioning` → `Ok(true)` iff the bucket reports `Enabled`.
/// Errors are surfaced (the caller downgrades them to a report note —
/// the warning is best-effort, the migration itself is not affected).
/// `pub(crate)` for `recompact` (same best-effort probe, same warning);
/// behaviour unchanged.
pub(crate) async fn versioning_enabled(
    client: &Client,
    bucket: &str,
) -> Result<bool, MigrateError> {
    let resp = client
        .get_bucket_versioning()
        .bucket(bucket)
        .send()
        .await
        .map_err(|e| MigrateError::Backend {
            op: "GetBucketVersioning",
            bucket: bucket.into(),
            key: String::new(),
            cause: format!("{e}"),
        })?;
    Ok(matches!(
        resp.status(),
        Some(aws_sdk_s3::types::BucketVersioningStatus::Enabled)
    ))
}

/// Build the same dispatcher the server would build from the same flags
/// (`main.rs::build_dispatcher`). `prefer_gpu` comes from the real GPU
/// probe — on CPU hosts the dispatcher never picks a GPU kind, matching
/// the non-GPU gateway build this migration is feeding.
fn build_migrate_dispatcher(params: &MigrateParams) -> Arc<dyn CodecDispatcher> {
    if params.use_sampling_dispatcher {
        Arc::new(
            SamplingDispatcher::new(params.default_codec)
                .with_gpu_preference(params.gpu_present, params.gpu_min_bytes)
                .with_columnar_gpu_preference(params.gpu_present && params.prefer_columnar_gpu),
        )
    } else {
        Arc::new(AlwaysDispatcher(params.default_codec))
    }
}

/// CPU-only write registry: `cpu-zstd` (at the server's `--zstd-level`)
/// plus `passthrough`. GPU codecs are intentionally absent — see the
/// module docs.
fn build_write_registry(params: &MigrateParams) -> CodecRegistry {
    CodecRegistry::new(CodecKind::CpuZstd)
        .with(Arc::new(Passthrough))
        .with(Arc::new(CpuZstd::new(params.zstd_level)))
}

/// Wrap an in-memory body as the `StreamingBlob` input
/// [`streaming_compress_to_frames`] expects. `pub(crate)` for
/// `recompact`; behaviour unchanged.
pub(crate) fn bytes_blob(body: Bytes) -> s3s::dto::StreamingBlob {
    s3s::dto::StreamingBlob::wrap(futures::stream::once(async move {
        Ok::<_, std::io::Error>(body)
    }))
}

/// Mandatory pre-write roundtrip: parse the framed bytes with the same
/// `FrameIter` the gateway's GET path uses, decompress each frame, and
/// compare the concatenation byte-for-byte against the original body.
/// Any parse / decompress error or byte difference returns `false`.
/// `pub(crate)` for `recompact`, which runs the same mandatory pre-write
/// check on its re-framed bytes; behaviour unchanged.
pub(crate) async fn verify_roundtrip(
    registry: &Arc<CodecRegistry>,
    framed: Bytes,
    original: &[u8],
) -> bool {
    let mut out = BytesMut::with_capacity(original.len());
    for frame in FrameIter::new(framed) {
        let Ok((header, payload)) = frame else {
            return false;
        };
        let manifest = ChunkManifest {
            codec: header.codec,
            original_size: header.original_size,
            compressed_size: header.compressed_size,
            crc32c: header.crc32c,
        };
        let Ok(decompressed) = registry.decompress(payload, &manifest).await else {
            return false;
        };
        // Bail before over-accumulating on a forged / corrupt frame set.
        if out.len() + decompressed.len() > original.len() {
            return false;
        }
        out.extend_from_slice(&decompressed);
    }
    out.as_ref() == original
}

/// Per-object outcome, folded into the run report.
#[derive(Debug)]
enum ObjectOutcome {
    Migrated {
        bytes_before: u64,
        bytes_after: u64,
        picked: CodecKind,
        wrote_with: CodecKind,
    },
    Skipped(SkipReason),
    Failed {
        op: &'static str,
        cause: String,
    },
}

/// Run the full per-object pipeline. Every early return is one of the
/// report buckets; this function never aborts the whole run (listing
/// failures do — see [`run_migrate`]).
async fn migrate_one(
    client: &Client,
    bucket: &str,
    job: &ObjectJob,
    dispatcher: &Arc<dyn CodecDispatcher>,
    registry: &Arc<CodecRegistry>,
    params: &MigrateParams,
) -> ObjectOutcome {
    if let Some(reason) = size_precheck(job.size, params.max_body_bytes) {
        return ObjectOutcome::Skipped(reason);
    }

    // Cheap already-S4 probe: 4 bytes of body + the metadata headers
    // ride along on the same response. Metadata first — it covers every
    // gateway-written shape (framed, legacy raw-zstd, passthrough,
    // gzip-buffered, SSE), where the magic alone only covers framed.
    let probe = match client
        .get_object()
        .bucket(bucket)
        .key(&job.key)
        .range(format!("bytes=0-{}", MAGIC_PROBE_BYTES - 1))
        .send()
        .await
    {
        Ok(p) => p,
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "GetObject(probe)",
                cause: format!("{e}"),
            };
        }
    };
    if probe
        .metadata()
        .map(|m| m.contains_key(META_CODEC))
        .unwrap_or(false)
    {
        return ObjectOutcome::Skipped(SkipReason::AlreadyS4);
    }
    let head_bytes = match probe.body.collect().await {
        Ok(b) => b.into_bytes(),
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "GetObject(probe body)",
                cause: format!("{e}"),
            };
        }
    };
    if is_s4_frame_prefix(&head_bytes) {
        return ObjectOutcome::Skipped(SkipReason::AlreadyS4);
    }

    // Full fetch — keep the response headers we must carry over.
    let resp = match client
        .get_object()
        .bucket(bucket)
        .key(&job.key)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "GetObject",
                cause: format!("{e}"),
            };
        }
    };
    // Size gate BEFORE buffering: when the response declares its
    // Content-Length, an over-cap body is skipped without reading it
    // (the listing snapshot may have raced a grow). Unknown length
    // falls through to the post-collect check below, as before.
    if let Some(len) = resp.content_length().and_then(|l| u64::try_from(l).ok())
        && len > params.max_body_bytes
    {
        return ObjectOutcome::Skipped(SkipReason::TooLarge);
    }
    let source_etag = resp.e_tag().map(normalize_etag);
    let content_type = resp.content_type().map(str::to_owned);
    let cache_control = resp.cache_control().map(str::to_owned);
    let content_disposition = resp.content_disposition().map(str::to_owned);
    let content_encoding = resp.content_encoding().map(str::to_owned);
    let content_language = resp.content_language().map(str::to_owned);
    // Carry the storage class over (None / STANDARD are left unset so
    // the PUT takes the bucket default, same as the original PUT did).
    let storage_class = resp
        .storage_class()
        .filter(|sc| **sc != aws_sdk_s3::types::StorageClass::Standard)
        .cloned();
    let mut metadata = resp.metadata().cloned().unwrap_or_default();
    let body = match resp.body.collect().await {
        Ok(b) => b.into_bytes(),
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "GetObject(body)",
                cause: format!("{e}"),
            };
        }
    };
    // Re-run the gates on the *fetched* body — the listing snapshot may
    // have raced an overwrite (different size, or now-S4 bytes).
    if let Some(reason) = size_precheck(body.len() as u64, params.max_body_bytes) {
        return ObjectOutcome::Skipped(reason);
    }
    if is_s4_frame_prefix(&body) {
        return ObjectOutcome::Skipped(SkipReason::AlreadyS4);
    }

    // Same dispatch the gateway PUT path runs: 4 KiB prefix sample +
    // total size hint.
    let sample_len = body.len().min(DISPATCH_SAMPLE_BYTES);
    let picked = dispatcher
        .pick_with_size_hint(&body[..sample_len], Some(body.len() as u64))
        .await;
    if picked == CodecKind::Passthrough {
        return ObjectOutcome::Skipped(SkipReason::NotCompressible);
    }
    let wrote_with = write_kind(picked);

    // Same framing call + chunk-size policy as the gateway's
    // streaming-framed PUT branch.
    let chunk_size = pick_chunk_size(Some(body.len() as u64));
    let (framed, manifest) = match streaming_compress_to_frames(
        bytes_blob(body.clone()),
        Arc::clone(registry),
        wrote_with,
        chunk_size,
        Some(body.len() as u64),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "compress",
                cause: format!("{e}"),
            };
        }
    };
    // No gain (frame headers ate the savings) → leave the object alone.
    if framed.len() >= body.len() {
        return ObjectOutcome::Skipped(SkipReason::NotCompressible);
    }

    // Mandatory roundtrip verify — runs in dry-run too, so the dry-run
    // counts are exactly what `--execute` would write. A failure here
    // means we mis-framed bytes we just produced: that is a bug,
    // surfaced loudly as a hard failure (exit 1), not a skip — same
    // policy as `recompact`.
    if !verify_roundtrip(registry, framed.clone(), &body).await {
        return ObjectOutcome::Failed {
            op: "verify",
            cause: "roundtrip verify failed on freshly framed bytes (bug — nothing written)".into(),
        };
    }

    let outcome = ObjectOutcome::Migrated {
        bytes_before: body.len() as u64,
        bytes_after: framed.len() as u64,
        picked,
        wrote_with,
    };
    if !params.execute {
        return outcome;
    }

    // Conflict guard: re-HEAD and compare the ETag captured at GET time.
    // NOT a full race fix (no compare-and-swap in S3) — see module docs.
    let head = match client
        .head_object()
        .bucket(bucket)
        .key(&job.key)
        .send()
        .await
    {
        Ok(h) => h,
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "HeadObject(pre-put)",
                cause: format!("{e}"),
            };
        }
    };
    if head.e_tag().map(normalize_etag) != source_etag {
        return ObjectOutcome::Skipped(SkipReason::EtagRaced);
    }

    // Carry object tags over (unless `--no-tags` opted out). An
    // AccessDenied / NotImplemented-class tagging-read failure skips
    // the object (`tags-unreadable`) — rewriting would silently strip
    // tags we cannot see; any other tagging failure stays a hard
    // failure. Nothing has been written yet at this point.
    let tags = if params.no_tags {
        Vec::new()
    } else {
        match fetch_tags(client, bucket, &job.key).await {
            Ok(t) => t,
            Err(e) if e.unreadable => {
                return ObjectOutcome::Skipped(SkipReason::TagsUnreadable);
            }
            Err(e) => {
                return ObjectOutcome::Failed {
                    op: "GetObjectTagging",
                    cause: e.cause,
                };
            }
        }
    };

    // Stamp the exact metadata contract the gateway PUT path writes
    // (`write_manifest` + the framed flag), preserving the user's own
    // metadata. Pre-existing `s4-*` keys are dropped — they could only
    // be stale leftovers and would corrupt the GET path's manifest.
    metadata.retain(|k, _| !k.starts_with("s4-"));
    metadata.insert(META_CODEC.into(), manifest.codec.as_str().into());
    metadata.insert(
        META_ORIGINAL_SIZE.into(),
        manifest.original_size.to_string(),
    );
    metadata.insert(
        META_COMPRESSED_SIZE.into(),
        manifest.compressed_size.to_string(),
    );
    metadata.insert(META_CRC32C.into(), manifest.crc32c.to_string());
    metadata.insert(META_FRAMED.into(), "true".into());

    let put_resp = match client
        .put_object()
        .bucket(bucket)
        .key(&job.key)
        .body(aws_sdk_s3::primitives::ByteStream::from(framed.clone()))
        .set_metadata(Some(metadata))
        .set_content_type(content_type)
        .set_cache_control(cache_control)
        .set_content_disposition(content_disposition)
        .set_content_encoding(content_encoding)
        .set_content_language(content_language)
        .set_storage_class(storage_class)
        .set_tagging(if tags.is_empty() {
            None
        } else {
            Some(encode_tagging(&tags))
        })
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "PutObject",
                cause: format!("{e}"),
            };
        }
    };

    // Sidecar, same policy as the gateway: multi-frame bodies only,
    // with the ETag + size version binding stamped from the PUT
    // response. A sidecar PUT failure is loud (the object itself is
    // migrated and readable — GETs fall back to a full read — but the
    // run exits non-zero so the operator notices; a re-run will skip
    // the object as already-S4, so the fix is `s4 repair-sidecar`).
    match build_index_from_body(&framed) {
        Ok(mut idx) if idx.entries.len() > 1 => {
            idx.source_etag = put_resp.e_tag().map(normalize_etag);
            idx.source_compressed_size = Some(framed.len() as u64);
            let sidecar_bytes = encode_index(&idx);
            if let Err(e) = client
                .put_object()
                .bucket(bucket)
                .key(sidecar_key(&job.key))
                .body(aws_sdk_s3::primitives::ByteStream::from(sidecar_bytes))
                .send()
                .await
            {
                return ObjectOutcome::Failed {
                    op: "PutObject(sidecar)",
                    cause: format!(
                        "object migrated but sidecar write failed (Range GETs fall back \
                         to full reads; run `s4 repair-sidecar {bucket}/{key}`): {e}",
                        key = job.key
                    ),
                };
            }
        }
        Ok(_) => {
            // Single frame: no sidecar by design (gateway parity). Remove
            // a stale multi-frame one if the object's previous shape had
            // left it behind (e.g. migrated multi-frame, overwritten by a
            // direct plain PUT, now re-migrated single-frame). The
            // existence HEAD keeps versioned buckets from accumulating
            // delete markers for never-existing sidecar keys. NOTE on the
            // concurrent-writer race: a writer landing between our PUT
            // and this DELETE could have written a fresh sidecar that we
            // now remove — that is perf-only (the gateway falls back to a
            // full-read on a missing sidecar), never a correctness loss.
            let sidecar = sidecar_key(&job.key);
            let exists = client
                .head_object()
                .bucket(bucket)
                .key(&sidecar)
                .send()
                .await
                .is_ok();
            if exists
                && let Err(e) = client
                    .delete_object()
                    .bucket(bucket)
                    .key(&sidecar)
                    .send()
                    .await
            {
                return ObjectOutcome::Failed {
                    op: "DeleteObject(sidecar)",
                    cause: format!(
                        "object migrated but its now-stale sidecar could not be deleted \
                         (the gateway ignores it via the ETag binding; clean up with \
                         `s4 sweep-orphan-sidecars`): {e}"
                    ),
                };
            }
        }
        Err(e) => {
            // We just framed these bytes ourselves; an unparseable body
            // here is a bug, not an operator condition. Surface loudly.
            return ObjectOutcome::Failed {
                op: "build_index_from_body",
                cause: format!("{e}"),
            };
        }
    }

    outcome
}

/// Fold one per-object outcome into the report accumulators.
fn fold_outcome(
    report: &mut MigrateReport,
    codec_counts: &mut BTreeMap<(String, String), u64>,
    key: String,
    outcome: ObjectOutcome,
) {
    match outcome {
        ObjectOutcome::Migrated {
            bytes_before,
            bytes_after,
            picked,
            wrote_with,
        } => {
            report.migrated += 1;
            report.migrated_bytes_before += bytes_before;
            report.migrated_bytes_after += bytes_after;
            *codec_counts
                .entry((picked.as_str().to_owned(), wrote_with.as_str().to_owned()))
                .or_insert(0) += 1;
        }
        ObjectOutcome::Skipped(reason) => match reason {
            SkipReason::AlreadyS4 => report.skipped_already_s4 += 1,
            SkipReason::NotCompressible => report.skipped_not_compressible += 1,
            SkipReason::TooLarge => report.skipped_too_large += 1,
            SkipReason::EtagRaced => report.skipped_etag_raced += 1,
            SkipReason::VerifyFailed => report.skipped_verify_failed += 1,
            SkipReason::TagsUnreadable => report.skipped_tags_unreadable += 1,
        },
        ObjectOutcome::Failed { op, cause } => {
            report.failed += 1;
            report.failures.push(MigrateFailure {
                key,
                op: op.to_owned(),
                cause,
            });
        }
    }
}

/// Run the full migration against `bucket`. Writes nothing unless
/// `params.execute` is set. Listing / versioning-probe failures abort
/// the run; per-object failures are counted in the report (callers map
/// `report.failed > 0` to a non-zero exit).
pub async fn run_migrate(
    client: &Client,
    bucket: &str,
    params: &MigrateParams,
) -> Result<MigrateReport, MigrateError> {
    run_migrate_with_cutoff(client, bucket, params, None)
        .await
        .map(|(report, _)| report)
}

/// v1.2 (`s4 maintain`): [`run_migrate`] plus an optional `older_than`
/// age cutoff — only objects whose backend `LastModified` is at least
/// that old are examined (same conservative gate as `s4 recompact
/// --older-than`: unknown `LastModified` under an active cutoff counts
/// as too recent). Returns the report and the number of listed objects
/// the cutoff excluded.
///
/// The too-recent count rides **next to** the report, not inside it:
/// [`MigrateParams`] / [`MigrateReport`] predate the age filter and are
/// plain (non-`non_exhaustive`) pub structs, so adding a field would
/// break downstream struct literals — the v1.0 freeze forbids that.
/// Excluded objects still count in `total_objects` / `total_bytes`
/// (they were listed) and a note flags the exclusion. `pub(crate)`:
/// the public surface for this knob is the `older-than` key of an
/// `s4 maintain` migrate rule, not a new library entry point.
pub(crate) async fn run_migrate_with_cutoff(
    client: &Client,
    bucket: &str,
    params: &MigrateParams,
    older_than: Option<std::time::Duration>,
) -> Result<(MigrateReport, u64), MigrateError> {
    let concurrency = params.concurrency.max(1);
    let cutoff_epoch_secs = crate::maintain::cutoff_epoch_secs(older_than);

    let inventory =
        list_inventory(client, bucket, params.prefix.as_deref(), params.max_objects).await?;
    // Best-effort versioning probe — downgrade failures to a note so an
    // ACL-restricted operator can still migrate.
    let (versioning, versioning_note) = match versioning_enabled(client, bucket).await {
        Ok(v) => (v, None),
        Err(e) => (false, Some(format!("{e}"))),
    };

    let dispatcher = build_migrate_dispatcher(params);
    let registry = Arc::new(build_write_registry(params));

    // Age gate BEFORE any per-object network call — same placement as
    // `recompact_one`'s gate. With no cutoff (the plain `s4 migrate`
    // path) every listed object is eligible, exactly as before.
    let (eligible, too_recent): (Vec<&ObjectJob>, Vec<&ObjectJob>) =
        inventory.objects.iter().partition(|job| {
            !crate::recompact::is_too_recent(job.last_modified_epoch_secs, cutoff_epoch_secs)
        });
    let skipped_too_recent = too_recent.len() as u64;

    use futures::StreamExt as _;
    let results: Vec<(String, ObjectOutcome)> = futures::stream::iter(eligible)
        .map(|job| {
            let dispatcher = &dispatcher;
            let registry = &registry;
            async move {
                let outcome = migrate_one(client, bucket, job, dispatcher, registry, params).await;
                (job.key.clone(), outcome)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let total_objects = inventory.objects.len() as u64;
    let total_bytes: u64 = inventory.objects.iter().map(|o| o.size).sum();
    let mut report = MigrateReport {
        bucket: bucket.to_owned(),
        prefix: params.prefix.clone(),
        dry_run: !params.execute,
        total_objects,
        total_bytes,
        listing_truncated: inventory.truncated,
        max_objects: params.max_objects,
        migrated: 0,
        migrated_bytes_before: 0,
        migrated_bytes_after: 0,
        skipped_already_s4: 0,
        skipped_not_compressible: 0,
        skipped_too_large: 0,
        skipped_etag_raced: 0,
        skipped_verify_failed: 0,
        skipped_tags_unreadable: 0,
        no_tags: params.no_tags,
        failed: 0,
        failures: Vec::new(),
        codecs: Vec::new(),
        versioning_enabled: versioning,
        notes: Vec::new(),
    };
    let mut codec_counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    for (key, outcome) in results {
        fold_outcome(&mut report, &mut codec_counts, key, outcome);
    }
    // `buffer_unordered` completion order is nondeterministic — sort for
    // stable output.
    report.failures.sort_by(|a, b| a.key.cmp(&b.key));
    report.codecs = codec_counts
        .into_iter()
        .map(|((picked, wrote_with), objects)| CodecMigrateCount {
            picked,
            wrote_with,
            objects,
        })
        .collect();

    if total_objects == 0 {
        report.notes.push("no objects found".into());
    }
    if skipped_too_recent > 0 {
        report.notes.push(format!(
            "{skipped_too_recent} listed object(s) were newer than the older-than cutoff and \
             not examined (counted in total_objects but in no skip bucket — the `s4 maintain` \
             rule report carries the too-recent count)"
        ));
    }
    if report.dry_run {
        report.notes.push(
            "dry-run: nothing was written; counts and sizes are measured on the real \
             compressed output — pass --execute to migrate"
                .into(),
        );
    }
    report.notes.push(
        "conflict safety: the source ETag is re-checked via HEAD immediately before each \
         overwrite, but S3 has no compare-and-swap — a writer landing between the HEAD \
         and the PUT is silently overwritten"
            .into(),
    );
    if params.no_tags {
        report.notes.push(
            "--no-tags: GetObjectTagging was not called and object tags are NOT carried \
             over — any existing tags are absent from rewritten objects. Storage class is \
             still carried over; ACLs and Object Lock retention/legal-hold are NOT — the \
             overwrite PUT resets them to bucket defaults"
                .into(),
        );
    } else {
        report.notes.push(
            "storage class and object tags are carried over on each rewrite; ACLs and \
             Object Lock retention/legal-hold are NOT — the overwrite PUT resets them to \
             bucket defaults, so do not migrate buckets relying on per-object ACLs or \
             Object Lock"
                .into(),
        );
    }
    if report.skipped_tags_unreadable > 0 {
        report.notes.push(format!(
            "{} object(s) skipped as tags-unreadable — GetObjectTagging failed with an \
             AccessDenied/NotImplemented/NotSupported-class error, so the rewrite was \
             withheld rather than silently dropping tags; grant s3:GetObjectTagging or \
             pass --no-tags to rewrite without preserving tags",
            report.skipped_tags_unreadable,
        ));
    }
    if report.versioning_enabled {
        report.notes.push(
            "WARNING: bucket versioning is Enabled — each migrated object leaves its \
             previous (uncompressed) version in place, so storage is double-billed until \
             old versions are lifecycle-expired"
                .into(),
        );
    }
    if let Some(cause) = versioning_note {
        report.notes.push(format!(
            "could not determine bucket versioning state ({cause}); if versioning is \
             Enabled, migrated objects double-bill until old versions expire"
        ));
    }
    if report.listing_truncated {
        report.notes.push(format!(
            "listing truncated at --max-objects={}: keys beyond the first {} were not \
             examined (re-run to continue — already-migrated objects are skipped)",
            params
                .max_objects
                .map(|n| n.to_string())
                .unwrap_or_default(),
            total_objects,
        ));
    }
    for c in &report.codecs {
        if c.picked != c.wrote_with {
            report.notes.push(format!(
                "{} would be chosen by the gateway for {} object(s); migrate wrote {} \
                 (level {}) instead — migrate is CPU-only and the frames are \
                 self-describing, so a GPU gateway reads them unchanged",
                c.picked, c.objects, c.wrote_with, params.zstd_level,
            ));
        }
    }
    if report.failed > 0 {
        report.notes.push(format!(
            "{} object(s) failed — see `failures`; re-running resumes automatically \
             (already-migrated objects are skipped)",
            report.failed,
        ));
    }

    Ok((report, skipped_too_recent))
}

/// Format a byte count as a short human string (binary units).
/// `pub(crate)` for `recompact`'s table renderer; behaviour unchanged.
pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Render the default human-readable summary for `--format table`.
pub fn render_human(report: &MigrateReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let target = match &report.prefix {
        Some(p) => format!("{}/{}", report.bucket, p),
        None => report.bucket.clone(),
    };
    let mode = if report.dry_run {
        "dry-run (pass --execute to write)"
    } else {
        "execute"
    };
    let _ = writeln!(out, "S4 migrate {target} — {mode}");
    let _ = writeln!(
        out,
        "  objects: {}{}   total: {} ({} bytes)",
        report.total_objects,
        if report.listing_truncated {
            " (listing truncated)".to_owned()
        } else {
            String::new()
        },
        human_bytes(report.total_bytes),
        report.total_bytes,
    );
    let verb = if report.dry_run {
        "would migrate"
    } else {
        "migrated"
    };
    let saved = report
        .migrated_bytes_before
        .saturating_sub(report.migrated_bytes_after);
    let _ = writeln!(
        out,
        "  {verb}: {} object(s), {} -> {} (saves {})",
        report.migrated,
        human_bytes(report.migrated_bytes_before),
        human_bytes(report.migrated_bytes_after),
        human_bytes(saved),
    );
    let _ = writeln!(
        out,
        "  skipped: {} already-s4, {} not-compressible, {} too-large, {} etag-raced, \
         {} verify-failed, {} tags-unreadable",
        report.skipped_already_s4,
        report.skipped_not_compressible,
        report.skipped_too_large,
        report.skipped_etag_raced,
        report.skipped_verify_failed,
        report.skipped_tags_unreadable,
    );
    let _ = writeln!(out, "  failed: {}", report.failed);
    if !report.codecs.is_empty() {
        let codecs = report
            .codecs
            .iter()
            .map(|c| {
                if c.picked == c.wrote_with {
                    format!("{}\u{d7}{}", c.wrote_with, c.objects)
                } else {
                    format!("{}\u{d7}{} (picked {})", c.wrote_with, c.objects, c.picked)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  codecs: {codecs}");
    }
    if !report.failures.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Failures:");
        for f in &report.failures {
            let _ = writeln!(out, "  - {} [{}]: {}", f.key, f.op, f.cause);
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Notes:");
    for n in &report.notes {
        let _ = writeln!(out, "  - {n}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s4_magic_prefix_detection() {
        assert!(is_s4_frame_prefix(b"S4F2rest-of-frame"));
        assert!(is_s4_frame_prefix(b"S4P1\0\0\0\0"));
        assert!(is_s4_frame_prefix(b"S4F2")); // exactly 4 bytes
        assert!(!is_s4_frame_prefix(b"S4F")); // too short
        assert!(!is_s4_frame_prefix(b""));
        assert!(!is_s4_frame_prefix(b"S4IXindex-not-frame"));
        assert!(!is_s4_frame_prefix(b"plain text body"));
        // zstd magic (legacy raw-zstd gateway objects are caught by the
        // metadata probe, not the frame magic).
        assert!(!is_s4_frame_prefix(&[0x28, 0xb5, 0x2f, 0xfd]));
    }

    #[test]
    fn write_kind_falls_back_to_cpu_zstd() {
        assert_eq!(write_kind(CodecKind::CpuZstd), CodecKind::CpuZstd);
        assert_eq!(write_kind(CodecKind::Passthrough), CodecKind::Passthrough);
        // Non-streaming CPU codec and every GPU kind really fall back.
        assert_eq!(write_kind(CodecKind::CpuGzip), CodecKind::CpuZstd);
        assert_eq!(write_kind(CodecKind::NvcompZstd), CodecKind::CpuZstd);
        assert_eq!(write_kind(CodecKind::NvcompBitcomp), CodecKind::CpuZstd);
        assert_eq!(write_kind(CodecKind::DietGpuAns), CodecKind::CpuZstd);
    }

    #[test]
    fn size_precheck_gates() {
        assert_eq!(size_precheck(0, 100), Some(SkipReason::NotCompressible));
        assert_eq!(size_precheck(1, 100), None);
        assert_eq!(size_precheck(100, 100), None);
        assert_eq!(size_precheck(101, 100), Some(SkipReason::TooLarge));
    }

    #[test]
    fn etag_normalization() {
        assert_eq!(normalize_etag("\"abc-1\""), "abc-1");
        assert_eq!(normalize_etag("abc-1"), "abc-1");
    }

    #[test]
    fn internal_keys_are_excluded_from_listings() {
        // Sidecars.
        assert!(is_internal_key("data/file.bin.s4index"));
        // Shared dictionaries (bucket-root `.s4dict/<id>` only).
        assert!(is_internal_key(".s4dict/0123456789abcdef"));
        assert!(
            !is_internal_key("data/.s4dict/x"),
            "dict prefix is root-anchored"
        );
        // Versioning shadow keys, at any depth.
        assert!(is_internal_key("file.txt.__s4ver__/v123"));
        assert!(is_internal_key("a/b/file.txt.__s4ver__/919b51b1-x"));
        assert!(is_versioning_shadow_key("k.__s4ver__/v1"));
        assert!(
            !is_versioning_shadow_key("k.__s4ver__"),
            "needs the directory slash"
        );
        // Regular customer keys pass.
        assert!(!is_internal_key("logs/app.log"));
        assert!(!is_internal_key("weird.__s4ver_not_quite/x"));
    }

    #[test]
    fn tagging_header_encoding() {
        assert_eq!(encode_tagging(&[]), "");
        assert_eq!(encode_tagging(&[("env".into(), "prod".into())]), "env=prod");
        assert_eq!(
            encode_tagging(&[
                ("env".into(), "prod".into()),
                ("team".into(), "s4 core".into()),
            ]),
            "env=prod&team=s4%20core"
        );
        // Reserved characters in both halves are percent-encoded.
        assert_eq!(
            encode_tagging(&[("k&=".into(), "v+%/ü".into())]),
            "k%26%3D=v%2B%25%2F%C3%BC"
        );
    }

    fn empty_report() -> MigrateReport {
        MigrateReport {
            bucket: "b".into(),
            prefix: None,
            dry_run: true,
            total_objects: 0,
            total_bytes: 0,
            listing_truncated: false,
            max_objects: None,
            migrated: 0,
            migrated_bytes_before: 0,
            migrated_bytes_after: 0,
            skipped_already_s4: 0,
            skipped_not_compressible: 0,
            skipped_too_large: 0,
            skipped_etag_raced: 0,
            skipped_verify_failed: 0,
            skipped_tags_unreadable: 0,
            no_tags: false,
            failed: 0,
            failures: Vec::new(),
            codecs: Vec::new(),
            versioning_enabled: false,
            notes: Vec::new(),
        }
    }

    #[test]
    fn fold_outcome_aggregates_every_bucket() {
        let mut report = empty_report();
        let mut counts = BTreeMap::new();
        fold_outcome(
            &mut report,
            &mut counts,
            "a".into(),
            ObjectOutcome::Migrated {
                bytes_before: 1000,
                bytes_after: 100,
                picked: CodecKind::CpuZstd,
                wrote_with: CodecKind::CpuZstd,
            },
        );
        fold_outcome(
            &mut report,
            &mut counts,
            "b".into(),
            ObjectOutcome::Migrated {
                bytes_before: 500,
                bytes_after: 50,
                picked: CodecKind::NvcompZstd,
                wrote_with: CodecKind::CpuZstd,
            },
        );
        for (key, reason) in [
            ("c", SkipReason::AlreadyS4),
            ("d", SkipReason::NotCompressible),
            ("e", SkipReason::TooLarge),
            ("f", SkipReason::EtagRaced),
            ("g", SkipReason::VerifyFailed),
            ("g2", SkipReason::TagsUnreadable),
        ] {
            fold_outcome(
                &mut report,
                &mut counts,
                key.into(),
                ObjectOutcome::Skipped(reason),
            );
        }
        fold_outcome(
            &mut report,
            &mut counts,
            "h".into(),
            ObjectOutcome::Failed {
                op: "PutObject",
                cause: "boom".into(),
            },
        );

        assert_eq!(report.migrated, 2);
        assert_eq!(report.migrated_bytes_before, 1500);
        assert_eq!(report.migrated_bytes_after, 150);
        assert_eq!(report.skipped_already_s4, 1);
        assert_eq!(report.skipped_not_compressible, 1);
        assert_eq!(report.skipped_too_large, 1);
        assert_eq!(report.skipped_etag_raced, 1);
        assert_eq!(report.skipped_verify_failed, 1);
        assert_eq!(report.skipped_tags_unreadable, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.failures[0].key, "h");
        assert_eq!(report.failures[0].op, "PutObject");
        // Two distinct (picked, wrote_with) pairs.
        assert_eq!(counts.len(), 2);
        assert_eq!(counts[&("cpu-zstd".to_owned(), "cpu-zstd".to_owned())], 1);
        assert_eq!(
            counts[&("nvcomp-zstd".to_owned(), "cpu-zstd".to_owned())],
            1
        );
    }

    fn dummy_report() -> MigrateReport {
        let mut r = empty_report();
        r.prefix = Some("logs/".into());
        r.dry_run = false;
        r.total_objects = 4;
        r.total_bytes = 4000;
        r.migrated = 2;
        r.migrated_bytes_before = 3000;
        r.migrated_bytes_after = 300;
        r.skipped_already_s4 = 1;
        r.skipped_not_compressible = 1;
        r.codecs = vec![CodecMigrateCount {
            picked: "nvcomp-zstd".into(),
            wrote_with: "cpu-zstd".into(),
            objects: 2,
        }];
        r.versioning_enabled = true;
        r.notes = vec!["WARNING: bucket versioning is Enabled".into()];
        r
    }

    #[test]
    fn report_json_shape() {
        let v = serde_json::to_value(dummy_report()).expect("serialize");
        assert_eq!(v["bucket"], "b");
        assert_eq!(v["prefix"], "logs/");
        assert_eq!(v["dry_run"], false);
        assert_eq!(v["total_objects"], 4);
        assert_eq!(v["total_bytes"], 4000);
        assert_eq!(v["listing_truncated"], false);
        assert_eq!(v["max_objects"], serde_json::Value::Null);
        assert_eq!(v["migrated"], 2);
        assert_eq!(v["migrated_bytes_before"], 3000);
        assert_eq!(v["migrated_bytes_after"], 300);
        assert_eq!(v["skipped_already_s4"], 1);
        assert_eq!(v["skipped_not_compressible"], 1);
        assert_eq!(v["skipped_too_large"], 0);
        assert_eq!(v["skipped_etag_raced"], 0);
        assert_eq!(v["skipped_verify_failed"], 0);
        assert_eq!(v["skipped_tags_unreadable"], 0);
        assert_eq!(v["no_tags"], false);
        assert_eq!(v["failed"], 0);
        assert_eq!(v["codecs"][0]["picked"], "nvcomp-zstd");
        assert_eq!(v["codecs"][0]["wrote_with"], "cpu-zstd");
        assert_eq!(v["codecs"][0]["objects"], 2);
        assert_eq!(v["versioning_enabled"], true);
        assert!(v["notes"].as_array().is_some_and(|a| !a.is_empty()));
        // Skip-reason serde casing (kebab-case like CodecKind).
        assert_eq!(
            serde_json::to_value(SkipReason::AlreadyS4).expect("skip reason"),
            "already-s4"
        );
        assert_eq!(
            serde_json::to_value(SkipReason::VerifyFailed).expect("skip reason"),
            "verify-failed"
        );
        assert_eq!(
            serde_json::to_value(SkipReason::TagsUnreadable).expect("skip reason"),
            "tags-unreadable"
        );
    }

    /// Skip-vs-fail classification for GetObjectTagging errors — the
    /// core of the tags-unreadable fix: permission / not-implemented
    /// errors must NOT hard-fail the whole migration (regression: a
    /// credential without s3:GetObjectTagging used to fail every
    /// object), while transient errors must NOT be mistaken for
    /// "backend has no tagging".
    #[test]
    fn tagging_error_classification() {
        // Backend says "no tag set on this object" → safe empty set.
        assert_eq!(
            classify_tagging_error(Some("NoSuchTagSet"), Some(404)),
            TagErrorClass::NoTags
        );
        // Permission / unimplemented codes → skip as tags-unreadable.
        for code in [
            "AccessDenied",
            "NotImplemented",
            "NotSupported",
            "MethodNotAllowed",
            "OperationNotSupported",
        ] {
            assert_eq!(
                classify_tagging_error(Some(code), None),
                TagErrorClass::Unreadable,
                "{code} must classify as unreadable"
            );
        }
        // Code takes precedence over status.
        assert_eq!(
            classify_tagging_error(Some("AccessDenied"), Some(403)),
            TagErrorClass::Unreadable
        );
        // Other modeled codes stay hard failures.
        for code in ["NoSuchKey", "SlowDown", "InternalError", "NoSuchBucket"] {
            assert_eq!(
                classify_tagging_error(Some(code), Some(500)),
                TagErrorClass::Other,
                "{code} must stay a hard failure"
            );
        }
        // Code-less responses: 403 / 405 / 501 are unreadable-class.
        assert_eq!(
            classify_tagging_error(None, Some(403)),
            TagErrorClass::Unreadable
        );
        assert_eq!(
            classify_tagging_error(None, Some(405)),
            TagErrorClass::Unreadable
        );
        assert_eq!(
            classify_tagging_error(None, Some(501)),
            TagErrorClass::Unreadable
        );
        // A raw code-less 404 may be NoSuchKey (deleted object) — must
        // NOT be folded into NoTags, and must NOT be skipped.
        assert_eq!(
            classify_tagging_error(None, Some(404)),
            TagErrorClass::Other
        );
        // Transport-level failures (no response at all) stay hard.
        assert_eq!(classify_tagging_error(None, None), TagErrorClass::Other);
        assert_eq!(
            classify_tagging_error(None, Some(500)),
            TagErrorClass::Other
        );
    }

    #[test]
    fn render_human_mentions_key_figures() {
        let txt = render_human(&dummy_report());
        assert!(txt.contains("S4 migrate b/logs/"));
        assert!(txt.contains("execute"));
        assert!(txt.contains("migrated: 2 object(s)"));
        assert!(txt.contains("already-s4"));
        assert!(txt.contains("cpu-zstd\u{d7}2 (picked nvcomp-zstd)"));
        assert!(txt.contains("Notes:"));
        assert!(txt.contains("versioning is Enabled"));

        let mut dry = dummy_report();
        dry.dry_run = true;
        let txt = render_human(&dry);
        assert!(txt.contains("dry-run (pass --execute to write)"));
        assert!(txt.contains("would migrate: 2 object(s)"));
    }

    /// The mandatory pre-write check: compress real bytes through the
    /// same framing call migrate uses, then confirm the verifier (a)
    /// accepts the genuine output and (b) rejects a tampered copy and a
    /// wrong original.
    #[tokio::test]
    async fn verify_roundtrip_accepts_genuine_and_rejects_tampered() {
        let params = MigrateParams {
            prefix: None,
            execute: false,
            concurrency: DEFAULT_MIGRATE_CONCURRENCY,
            max_objects: None,
            max_body_bytes: u64::MAX,
            default_codec: CodecKind::CpuZstd,
            zstd_level: CpuZstd::DEFAULT_LEVEL,
            use_sampling_dispatcher: true,
            gpu_min_bytes: SamplingDispatcher::DEFAULT_GPU_MIN_BYTES,
            prefer_columnar_gpu: false,
            gpu_present: false,
            no_tags: false,
        };
        let registry = Arc::new(build_write_registry(&params));
        // > 1 MiB so pick_chunk_size lands on 4 MiB... actually 2 MiB
        // body → 4 MiB chunks → single frame; use a 5 MiB body for a
        // genuine multi-frame layout.
        let original = Bytes::from(
            b"migrate verify line: status=200 path=/api/items\n"
                .repeat(110_000)
                .to_vec(),
        );
        assert!(original.len() > 4 * 1024 * 1024);
        let chunk_size = pick_chunk_size(Some(original.len() as u64));
        let (framed, manifest) = streaming_compress_to_frames(
            bytes_blob(original.clone()),
            Arc::clone(&registry),
            CodecKind::CpuZstd,
            chunk_size,
            Some(original.len() as u64),
        )
        .await
        .expect("compress");
        assert_eq!(manifest.original_size, original.len() as u64);
        assert!(is_s4_frame_prefix(&framed), "output must carry S4F2 magic");
        let idx = build_index_from_body(&framed).expect("index");
        assert!(idx.entries.len() > 1, "5 MiB body must be multi-frame");

        assert!(verify_roundtrip(&registry, framed.clone(), &original).await);

        // Tamper one payload byte past the first frame header — the
        // per-frame CRC / zstd integrity must fail the verify.
        let mut tampered = framed.to_vec();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0xff;
        assert!(!verify_roundtrip(&registry, Bytes::from(tampered), &original).await);

        // Wrong original (truncated) must fail the byte compare.
        assert!(!verify_roundtrip(&registry, framed, &original[..original.len() - 1]).await);
    }
}
