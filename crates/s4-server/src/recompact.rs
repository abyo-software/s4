//! v1.1: `s4 recompact` — rewrite cpu-zstd framed objects at a higher
//! zstd level (the S3 take on LSM compaction).
//!
//! The gateway's PUT path favours latency: bodies are framed with
//! `cpu-zstd` at the server's `--zstd-level` (default 3). This tool
//! walks a bucket (or prefix) during a quiet window and "bakes" those
//! frames at a higher level (`--target-zstd-level`, default 19),
//! shrinking the backend bill without touching the read path — the
//! frames stay self-describing cpu-zstd, so any gateway build reads
//! them unchanged. Per object:
//!
//! 1. A 4-byte `Range` GET probes the `S4F2` / `S4P1` frame magic and
//!    the `s4-codec` / `s4-zstd-level` metadata. Only **S4-framed,
//!    cpu-zstd** objects qualify — the exact inverse of `s4 migrate`'s
//!    selection. Plain objects are skipped (`not-s4`, with a "run
//!    `s4 migrate` first" hint); `passthrough` / `cpu-gzip` /
//!    `nvcomp-*` / `cpu-zstd-dict` stamps are skipped
//!    (`unsupported-codec` — this tool is cpu-zstd → cpu-zstd only).
//!    Framed objects **without** the `s4-codec` metadata stamp are
//!    skipped too (`unstamped-framed`): the gateway and `s4 migrate`
//!    always stamp, so an unstamped S4F2 prefix means something else
//!    wrote the bytes (or stripped the metadata) and silently
//!    "promoting" it could mangle a foreign format. Pass
//!    `--assume-unstamped-framed` to opt in to the pre-v1.0.1
//!    behaviour of treating them as gateway frames.
//! 2. An object already stamped `s4-zstd-level >= target` is skipped
//!    (`already-compacted`) — the idempotency core: a re-run resumes
//!    automatically with no checkpoint file.
//! 3. The full body is fetched and the existing frames are decoded
//!    in-process with the same `FrameIter` + registry path the
//!    gateway's GET uses — recovering the original bytes doubles as an
//!    integrity check on the stored frames.
//! 4. The original bytes are re-framed via the **same**
//!    [`streaming_compress_to_frames`] + [`pick_chunk_size`] pair the
//!    gateway's PUT path calls, at the target level. The rewrite only
//!    proceeds when the new frames are at least `--min-gain-percent`
//!    (default 3%) smaller than the **currently stored** bytes —
//!    otherwise the object is left alone (`insufficient-gain`), which
//!    also keeps a `s4-zstd-level`-less re-run (e.g. after a
//!    CopyObject dropped the stamp) from churning.
//! 5. The new frames are decompressed back and byte-compared against
//!    the decoded original (**no verify, no write** — same policy as
//!    migrate), a `HEAD` re-checks the source ETag immediately before
//!    the PUT (`etag-raced` on mismatch; narrows but does NOT close
//!    the race — S3 has no compare-and-swap), and the object is
//!    re-PUT with its user metadata preserved, the `s4-*` manifest
//!    keys re-stamped for the new frames, and `s4-zstd-level` set to
//!    the target.
//! 6. Multi-frame results get a fresh `<key>.s4index` sidecar (ETag +
//!    size binding from the PUT response); when the new body is
//!    single-frame, a stale sidecar left by the previous shape is
//!    deleted (best-effort HEAD-then-DELETE).
//!
//! ## Dry-run by default
//!
//! Like `migrate` / `sweep-orphan-sidecars`, the default mode only
//! reports what *would* be rewritten (it still GETs + decodes +
//! recompresses + verifies, so the projected sizes are measured, not
//! estimated). Pass `--execute` to write.
//!
//! ## `--older-than`
//!
//! `--older-than 30d` (or `12h`, `45m`, `90s`) restricts the rewrite
//! to objects whose backend `LastModified` is at least that old —
//! the "recompact what has gone cold" knob for nightly runs. Objects
//! newer than the cutoff (or with no `LastModified` at all, which is
//! treated conservatively as new) are skipped (`too-recent`).
//!
//! ## Honesty constraints
//!
//! - **cpu-zstd → cpu-zstd only.** GPU-written (`nvcomp-*`), gzip,
//!   dictionary (`cpu-zstd-dict`) and passthrough objects are skipped,
//!   not converted.
//! - **SSE deployments are out of scope** (the CLI refuses the SSE
//!   flags before this module runs); encrypted bodies never carry the
//!   frame magic, so they classify as `not-s4` defensively anyway.
//! - **Versioned buckets work but double-bill**: the overwrite PUT
//!   leaves the previous version in place. The report warns when
//!   `GetBucketVersioning` says `Enabled`.
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
//!   rely on per-object ACLs or Object Lock must not be recompacted
//!   with this tool; the report repeats this in `notes`.
//! - **Internal keys are never touched**: `.s4index` sidecars,
//!   `.s4dict/` shared dictionaries and `.__s4ver__/` versioning
//!   shadow keys are excluded from the listing — rewriting a shadow
//!   key would break the version-restore path.
//! - **Multipart-written objects are rewritten as single-PUT framed
//!   objects** (padding frames dropped, `s4-multipart` flag dropped) —
//!   byte-identical on GET, but the multipart ETag shape is lost (any
//!   overwrite PUT changes the ETag regardless).

use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::Client;
use bytes::{Bytes, BytesMut};
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::index::{build_index_from_body, encode_index, sidecar_key};
use s4_codec::multipart::FrameIter;
use s4_codec::passthrough::Passthrough;
use s4_codec::{ChunkManifest, CodecKind, CodecRegistry};
use serde::Serialize;
use thiserror::Error;

use crate::migrate::{bytes_blob, human_bytes, is_s4_frame_prefix, normalize_etag};
use crate::service::{
    META_CODEC, META_COMPRESSED_SIZE, META_CRC32C, META_FRAMED, META_ORIGINAL_SIZE, META_ZSTD_LEVEL,
};
use crate::streaming::{pick_chunk_size, streaming_compress_to_frames};

/// Default `--concurrency`: 4 objects in flight, same reasoning as
/// migrate (each in-flight object holds its stored body, decoded
/// original and re-framed output in RAM).
pub const DEFAULT_RECOMPACT_CONCURRENCY: usize = 4;

/// Default `--target-zstd-level`. 19 is zstd's "high compression"
/// sweet spot — beyond it the CPU cost climbs steeply for marginal
/// ratio. Compression level is encode-side only: any gateway build
/// decompresses level-19 frames exactly like level-3 frames.
pub const DEFAULT_TARGET_ZSTD_LEVEL: i32 = 19;

/// Default `--min-gain-percent`: rewrite only when the re-framed bytes
/// are at least this much smaller than the currently stored bytes.
pub const DEFAULT_MIN_GAIN_PERCENT: f64 = 3.0;

/// Bytes fetched by the frame-magic probe (`Range: bytes=0-3`).
const MAGIC_PROBE_BYTES: usize = 4;

/// v1.1 stability: `#[non_exhaustive]` — new recompact-time failure
/// modes may be added in minor releases.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RecompactError {
    #[error("S3 backend error on {op} {bucket}/{key}: {cause}")]
    Backend {
        op: &'static str,
        bucket: String,
        // Empty for bucket-level ops (ListObjectsV2 / GetBucketVersioning).
        key: String,
        // Named `cause` (not `source`) — same convention as
        // `migrate::MigrateError`.
        cause: String,
    },
    #[error("invalid recompact target {target:?}: {reason}")]
    InvalidTarget { target: String, reason: String },
}

/// Why one object was left untouched. `#[non_exhaustive]`: new skip
/// classes may be added in minor releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SkipReason {
    /// Not an S4-framed object (no `S4F2` / `S4P1` magic) — run
    /// `s4 migrate` first to frame it, then recompact.
    NotS4,
    /// `s4-zstd-level` metadata is already `>= --target-zstd-level`.
    AlreadyCompacted,
    /// `s4-codec` metadata (or a frame header) names a codec other
    /// than `cpu-zstd` — passthrough / gzip / nvcomp-* / cpu-zstd-dict
    /// are out of scope for this tool.
    UnsupportedCodec,
    /// Body carries the `S4F2` / `S4P1` magic but no `s4-codec`
    /// metadata stamp — a backend-written framed object without
    /// gateway metadata. Skipped by default (the bytes may be a
    /// foreign format that merely shares the prefix); pass
    /// `--assume-unstamped-framed` to recompact it anyway.
    UnstampedFramed,
    /// Re-framing at the target level did not shrink the stored bytes
    /// by at least `--min-gain-percent`.
    InsufficientGain,
    /// Stored (or decoded-original) size exceeds `max_body_bytes`.
    TooLarge,
    /// The ETag changed between the GET and the pre-PUT HEAD —
    /// a concurrent writer won; nothing was overwritten.
    EtagRaced,
    /// `LastModified` is newer than the `--older-than` cutoff (or
    /// unknown, which is treated conservatively as new).
    TooRecent,
    /// `GetObjectTagging` failed with an AccessDenied / NotImplemented /
    /// NotSupported-class error: the object may carry tags we cannot
    /// read, so rewriting it would silently strip them. Grant
    /// `s3:GetObjectTagging` or pass `--no-tags` to rewrite without
    /// preserving tags. Other tagging errors (transient faults) stay
    /// hard failures. Same policy as `migrate`.
    TagsUnreadable,
}

/// Knobs for one recompact run.
#[derive(Debug, Clone)]
pub struct RecompactParams {
    /// Restrict the listing to keys under this prefix.
    pub prefix: Option<String>,
    /// `false` (default) = dry-run: GET + decode + recompress + verify
    /// but never PUT. `true` = actually rewrite objects (and sidecars).
    pub execute: bool,
    /// Objects processed in parallel. Values below 1 are clamped to 1.
    pub concurrency: usize,
    /// Stop listing after this many non-sidecar objects (`None` = no
    /// cap). Objects beyond the cap are not touched and the report
    /// flags the truncation.
    pub max_objects: Option<usize>,
    /// Per-object cap on both the stored body and the decoded original;
    /// larger objects are skipped (`TooLarge`).
    pub max_body_bytes: u64,
    /// zstd level the frames are rewritten at (clamped to 1..=22 by the
    /// codec). Also the `already-compacted` comparison threshold.
    pub target_zstd_level: i32,
    /// Minimum shrink (percent of the currently stored bytes) required
    /// before the rewrite happens.
    pub min_gain_percent: f64,
    /// Only rewrite objects whose `LastModified` is at least this old.
    /// `None` = no age filter.
    pub older_than: Option<Duration>,
    /// Treat S4F2/S4P1-framed objects with **no** `s4-codec` metadata
    /// stamp as gateway frames and recompact them (`false` = skip them
    /// as `unstamped-framed`, the safe default — see [`SkipReason`]).
    pub assume_unstamped_framed: bool,
    /// `--no-tags`: skip the `GetObjectTagging` read entirely and
    /// rewrite without carrying tags over. Explicit opt-out for
    /// credentials/backends where tagging reads are denied or
    /// unimplemented (objects otherwise skip as `tags-unreadable`).
    /// WARNING: any existing object tags are NOT preserved on
    /// rewritten objects. Same policy as `migrate`.
    pub no_tags: bool,
}

/// One per-object hard failure (the object was left as-is unless the
/// `op` says otherwise — see the sidecar notes in [`run_recompact`]).
#[derive(Debug, Clone, Serialize)]
pub struct RecompactFailure {
    pub key: String,
    pub op: String,
    pub cause: String,
}

/// Full result of one recompact run. Serializes to the `--format json`
/// output verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct RecompactReport {
    pub bucket: String,
    pub prefix: Option<String>,
    /// `true` = nothing was written (default mode).
    pub dry_run: bool,
    pub target_zstd_level: i32,
    pub min_gain_percent: f64,
    /// `--older-than` cutoff in seconds (`null` = no age filter).
    pub older_than_secs: Option<u64>,
    /// Objects listed (after `.s4index` / `.s4dict/` / `.__s4ver__/`
    /// exclusion, before any skip).
    pub total_objects: u64,
    pub total_bytes: u64,
    /// `true` when listing stopped at `max_objects` with more keys
    /// remaining — those keys were not examined at all.
    pub listing_truncated: bool,
    pub max_objects: Option<usize>,
    /// Objects rewritten (dry-run: objects that *would* be rewritten;
    /// the sizes below are measured on the real re-framed output
    /// either way).
    pub recompacted: u64,
    /// Stored (already-compressed) bytes of the rewritten set, before.
    pub recompacted_bytes_before: u64,
    /// Stored bytes of the rewritten set, after.
    pub recompacted_bytes_after: u64,
    pub skipped_not_s4: u64,
    pub skipped_already_compacted: u64,
    pub skipped_unsupported_codec: u64,
    /// Framed body without the gateway's `s4-codec` stamp, skipped
    /// because `--assume-unstamped-framed` was not passed.
    pub skipped_unstamped_framed: u64,
    pub skipped_insufficient_gain: u64,
    pub skipped_too_large: u64,
    pub skipped_etag_raced: u64,
    pub skipped_too_recent: u64,
    /// Objects skipped because `GetObjectTagging` failed with an
    /// AccessDenied / NotImplemented / NotSupported-class error (see
    /// [`SkipReason::TagsUnreadable`]). Always `0` with `--no-tags`.
    pub skipped_tags_unreadable: u64,
    /// `true` when the run was made with `--no-tags` (tags neither read
    /// nor carried over).
    pub no_tags: bool,
    pub failed: u64,
    /// Per-key failure details, sorted by key.
    pub failures: Vec<RecompactFailure>,
    /// `GetBucketVersioning` said `Enabled` (double-billing warning in
    /// `notes`). `false` covers Suspended / never-enabled / unknown.
    pub versioning_enabled: bool,
    /// Fixed honesty notes + run-specific caveats. Always read these
    /// before quoting the numbers anywhere.
    pub notes: Vec<String>,
}

/// Parse a `--older-than` duration of the shape `<integer><s|m|h|d>`
/// (e.g. `30d`, `12h`, `45m`, `90s`). Lowercase units only; no
/// fractions, no composite forms (`1h30m` is rejected). The workspace
/// carries no humantime-style dependency, so this stays deliberately
/// minimal.
pub fn parse_duration_suffix(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let Some(unit) = s.chars().last() else {
        return Err("empty duration (expected e.g. `30d`, `12h`, `45m`, `90s`)".into());
    };
    let mult: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => {
            return Err(format!(
                "invalid duration {s:?}: must end in `s`, `m`, `h` or `d` (e.g. `30d`)"
            ));
        }
    };
    let num = &s[..s.len() - 1];
    if num.is_empty() {
        return Err(format!("invalid duration {s:?}: missing the number part"));
    }
    let n: u64 = num
        .parse()
        .map_err(|e| format!("invalid duration {s:?}: {e}"))?;
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| format!("invalid duration {s:?}: overflows u64 seconds"))?;
    Ok(Duration::from_secs(secs))
}

/// Render whole seconds back as the shortest `<n><unit>` form, for the
/// table output (`2592000` → `30d`). `pub(crate)` for `maintain`'s
/// table renderer; behaviour unchanged.
pub(crate) fn human_duration_secs(secs: u64) -> String {
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// `--older-than` gate. `cutoff_epoch_secs = now - older_than`; an
/// object qualifies only when its `LastModified` is at or before the
/// cutoff. No cutoff (`None`) admits everything; an unknown
/// `LastModified` under an active cutoff is conservatively `TooRecent`
/// (we cannot prove the object is old enough to rewrite). `pub(crate)`
/// for `migrate` (the `s4 maintain` `older-than` gate) and `maintain`'s
/// transition action — same semantics everywhere; behaviour unchanged.
pub(crate) fn is_too_recent(
    last_modified_epoch_secs: Option<i64>,
    cutoff_epoch_secs: Option<i64>,
) -> bool {
    match cutoff_epoch_secs {
        None => false,
        Some(cutoff) => match last_modified_epoch_secs {
            None => true,
            Some(t) => t > cutoff,
        },
    }
}

/// `true` when the `s4-zstd-level` stamp already meets the target.
/// Absent or unparseable stamps (gateway-written objects never carry
/// one) count as "not yet compacted".
fn is_already_compacted(meta_level: Option<&str>, target_level: i32) -> bool {
    meta_level
        .and_then(|v| v.parse::<i32>().ok())
        .is_some_and(|lvl| lvl >= target_level)
}

/// Classify one object from its probe response (metadata + first 4
/// body bytes). `None` = candidate for recompaction. Order matters:
/// the codec stamp is checked **before** the frame magic so a
/// gateway-written passthrough object (raw body, `s4-codec:
/// passthrough`) classifies as `UnsupportedCodec`, not `NotS4`.
/// A framed body with **no** codec stamp at all is `UnstampedFramed`
/// unless `assume_unstamped_framed` opts in — the gateway and migrate
/// always stamp, so unstamped S4F2 bytes were written by something
/// else and must not be silently "promoted".
fn classify_probe(
    meta_codec: Option<&str>,
    meta_level: Option<&str>,
    body_prefix: &[u8],
    target_level: i32,
    assume_unstamped_framed: bool,
) -> Option<SkipReason> {
    if let Some(codec) = meta_codec
        && codec != CodecKind::CpuZstd.as_str()
    {
        return Some(SkipReason::UnsupportedCodec);
    }
    if !is_s4_frame_prefix(body_prefix) {
        // Covers plain pre-gateway objects AND legacy v0.1 raw-zstd
        // bodies (cpu-zstd stamp, no framing) — recompact only handles
        // framed objects.
        return Some(SkipReason::NotS4);
    }
    if meta_codec.is_none() && !assume_unstamped_framed {
        return Some(SkipReason::UnstampedFramed);
    }
    if is_already_compacted(meta_level, target_level) {
        return Some(SkipReason::AlreadyCompacted);
    }
    None
}

/// Gain gate: rewrite only when `after` is strictly smaller than
/// `before` AND the shrink is at least `min_gain_percent` percent of
/// `before`. A non-positive `min_gain_percent` therefore still
/// requires *some* shrink — recompact never rewrites size-neutral or
/// size-increasing results.
fn meets_min_gain(before: u64, after: u64, min_gain_percent: f64) -> bool {
    if before == 0 || after >= before {
        return false;
    }
    let gain_percent = ((before - after) as f64) * 100.0 / (before as f64);
    gain_percent >= min_gain_percent
}

#[derive(Debug, Clone)]
struct ObjectJob {
    key: String,
    size: u64,
    /// Backend `LastModified` as epoch seconds (`None` when the
    /// listing omitted it).
    last_modified_epoch_secs: Option<i64>,
}

struct Inventory {
    objects: Vec<ObjectJob>,
    truncated: bool,
}

/// Paginate `ListObjectsV2`, skipping internal keys (`.s4index`
/// sidecars, `.s4dict/` dictionaries, `.__s4ver__/` version shadows),
/// stopping at `max_objects` collected keys. Same pagination shape as
/// `migrate::list_inventory`, plus the `LastModified` capture the
/// `--older-than` filter needs.
async fn list_inventory(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    max_objects: Option<usize>,
) -> Result<Inventory, RecompactError> {
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
        let resp = req.send().await.map_err(|e| RecompactError::Backend {
            op: "ListObjectsV2",
            bucket: bucket.into(),
            key: String::new(),
            cause: format!("{e}"),
        })?;
        for obj in resp.contents() {
            let Some(k) = obj.key() else { continue };
            if crate::migrate::is_internal_key(k) {
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

/// Decode-existing-frames result.
enum DecodeOutcome {
    /// Original bytes recovered.
    Decoded(Bytes),
    /// A frame header named a codec other than cpu-zstd.
    UnsupportedFrameCodec,
    /// Decoded output would exceed `max_body_bytes`.
    ExceedsCap,
    /// Frame parse / decompress / CRC failure — stored frames are
    /// corrupt or truncated; surfaced as a hard failure.
    Corrupt(String),
}

/// Decode an S4-framed body back to the original bytes with the same
/// `FrameIter` walk the gateway's GET path uses (padding frames are
/// skipped by the iterator). Defensive per-frame codec check: only
/// cpu-zstd frames are in scope here, anything else (cpu-zstd-dict
/// frames missing their metadata stamp, GPU frames, …) bails as
/// `UnsupportedFrameCodec` rather than failing inside the registry.
async fn decode_frames(
    registry: &Arc<CodecRegistry>,
    framed: Bytes,
    max_body_bytes: u64,
) -> DecodeOutcome {
    let mut out = BytesMut::new();
    for frame in FrameIter::new(framed) {
        let (header, payload) = match frame {
            Ok(v) => v,
            Err(e) => return DecodeOutcome::Corrupt(format!("frame parse: {e}")),
        };
        if header.codec != CodecKind::CpuZstd {
            return DecodeOutcome::UnsupportedFrameCodec;
        }
        let manifest = ChunkManifest {
            codec: header.codec,
            original_size: header.original_size,
            compressed_size: header.compressed_size,
            crc32c: header.crc32c,
        };
        let decompressed = match registry.decompress(payload, &manifest).await {
            Ok(d) => d,
            Err(e) => return DecodeOutcome::Corrupt(format!("frame decompress: {e}")),
        };
        if (out.len() + decompressed.len()) as u64 > max_body_bytes {
            return DecodeOutcome::ExceedsCap;
        }
        out.extend_from_slice(&decompressed);
    }
    DecodeOutcome::Decoded(out.freeze())
}

/// Decode + recompress registry: `cpu-zstd` at the target level (the
/// level only affects the encode side — decode of the existing frames
/// is level-agnostic) plus `passthrough` for registry completeness.
fn build_registry(params: &RecompactParams) -> CodecRegistry {
    CodecRegistry::new(CodecKind::CpuZstd)
        .with(Arc::new(Passthrough))
        .with(Arc::new(CpuZstd::new(params.target_zstd_level)))
}

/// Per-object outcome, folded into the run report.
#[derive(Debug)]
enum ObjectOutcome {
    Recompacted { bytes_before: u64, bytes_after: u64 },
    Skipped(SkipReason),
    Failed { op: &'static str, cause: String },
}

/// Run the full per-object pipeline. Every early return is one of the
/// report buckets; this function never aborts the whole run (listing
/// failures do — see [`run_recompact`]).
async fn recompact_one(
    client: &Client,
    bucket: &str,
    job: &ObjectJob,
    registry: &Arc<CodecRegistry>,
    params: &RecompactParams,
    cutoff_epoch_secs: Option<i64>,
) -> ObjectOutcome {
    // Age gate first — no network spent on objects still hot.
    if is_too_recent(job.last_modified_epoch_secs, cutoff_epoch_secs) {
        return ObjectOutcome::Skipped(SkipReason::TooRecent);
    }
    if job.size == 0 {
        // A zero-byte object cannot carry the frame magic.
        return ObjectOutcome::Skipped(SkipReason::NotS4);
    }
    if job.size > params.max_body_bytes {
        return ObjectOutcome::Skipped(SkipReason::TooLarge);
    }

    // Cheap classification probe: 4 bytes of body + the metadata
    // headers ride along on the same response.
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
    let (probe_codec, probe_level) = {
        let m = probe.metadata();
        (
            m.and_then(|m| m.get(META_CODEC)).cloned(),
            m.and_then(|m| m.get(META_ZSTD_LEVEL)).cloned(),
        )
    };
    let head_bytes = match probe.body.collect().await {
        Ok(b) => b.into_bytes(),
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "GetObject(probe body)",
                cause: format!("{e}"),
            };
        }
    };
    if let Some(reason) = classify_probe(
        probe_codec.as_deref(),
        probe_level.as_deref(),
        &head_bytes,
        params.target_zstd_level,
        params.assume_unstamped_framed,
    ) {
        return ObjectOutcome::Skipped(reason);
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
    // Content-Length, an over-cap stored body is skipped without
    // reading it (the listing snapshot may have raced a grow). Unknown
    // length falls through to the post-collect check below, as before.
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
    let stored = match resp.body.collect().await {
        Ok(b) => b.into_bytes(),
        Err(e) => {
            return ObjectOutcome::Failed {
                op: "GetObject(body)",
                cause: format!("{e}"),
            };
        }
    };
    // Re-run the gates on the *fetched* object — the listing snapshot
    // (and the probe) may have raced an overwrite.
    if stored.is_empty() {
        return ObjectOutcome::Skipped(SkipReason::NotS4);
    }
    if stored.len() as u64 > params.max_body_bytes {
        return ObjectOutcome::Skipped(SkipReason::TooLarge);
    }
    if let Some(reason) = classify_probe(
        metadata.get(META_CODEC).map(String::as_str),
        metadata.get(META_ZSTD_LEVEL).map(String::as_str),
        &stored,
        params.target_zstd_level,
        params.assume_unstamped_framed,
    ) {
        return ObjectOutcome::Skipped(reason);
    }

    // Decode the existing frames — this both recovers the original
    // bytes and validates the stored object end to end (same FrameIter
    // + registry path the gateway GET uses).
    let original = match decode_frames(registry, stored.clone(), params.max_body_bytes).await {
        DecodeOutcome::Decoded(b) => b,
        DecodeOutcome::UnsupportedFrameCodec => {
            return ObjectOutcome::Skipped(SkipReason::UnsupportedCodec);
        }
        DecodeOutcome::ExceedsCap => return ObjectOutcome::Skipped(SkipReason::TooLarge),
        DecodeOutcome::Corrupt(cause) => {
            return ObjectOutcome::Failed {
                op: "decode-frames",
                cause: format!(
                    "stored frames failed to decode (object left untouched; run \
                     `s4 verify-sidecar {bucket}/{key}` and check backend integrity): {cause}",
                    key = job.key
                ),
            };
        }
    };
    if original.is_empty() {
        // Padding-only / zero-data frame stream: nothing to recompress.
        return ObjectOutcome::Skipped(SkipReason::InsufficientGain);
    }

    // Same framing call + chunk-size policy as the gateway's streaming
    // PUT path (and as `s4 migrate`), at the target level.
    let chunk_size = pick_chunk_size(Some(original.len() as u64));
    let (framed, manifest) = match streaming_compress_to_frames(
        bytes_blob(original.clone()),
        Arc::clone(registry),
        CodecKind::CpuZstd,
        chunk_size,
        Some(original.len() as u64),
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

    // Gain gate against the bytes *currently stored* — that is what the
    // backend bills for.
    if !meets_min_gain(
        stored.len() as u64,
        framed.len() as u64,
        params.min_gain_percent,
    ) {
        return ObjectOutcome::Skipped(SkipReason::InsufficientGain);
    }

    // Mandatory roundtrip verify on the new frames — runs in dry-run
    // too, so the dry-run counts are exactly what `--execute` would
    // write. A failure here means we mis-framed bytes we just produced:
    // that is a bug, surfaced loudly as a failure (not a skip).
    if !crate::migrate::verify_roundtrip(registry, framed.clone(), &original).await {
        return ObjectOutcome::Failed {
            op: "verify",
            cause: "roundtrip verify failed on freshly re-framed bytes (bug — nothing written)"
                .into(),
        };
    }

    let outcome = ObjectOutcome::Recompacted {
        bytes_before: stored.len() as u64,
        bytes_after: framed.len() as u64,
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

    // Carry object tags over (same policy as migrate, unless
    // `--no-tags` opted out). An AccessDenied / NotImplemented-class
    // tagging-read failure skips the object (`tags-unreadable`) —
    // rewriting would silently strip tags we cannot see; any other
    // tagging failure stays a hard failure. Nothing written yet.
    let tags = if params.no_tags {
        Vec::new()
    } else {
        match crate::migrate::fetch_tags(client, bucket, &job.key).await {
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

    // Preserve the user's metadata, re-stamp the manifest keys for the
    // new frames, and add the recompact level stamp. Stale `s4-*`
    // leftovers (including `s4-multipart` — the rewrite is a single
    // coherent frame stream now — and any dropped sidecar/dict hints)
    // are removed, exactly like migrate.
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
    metadata.insert(META_ZSTD_LEVEL.into(), params.target_zstd_level.to_string());

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
            Some(crate::migrate::encode_tagging(&tags))
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

    // Sidecar, same policy as the gateway / migrate: multi-frame bodies
    // get a fresh sidecar with the new ETag + size binding. Single-frame
    // bodies get none — and any stale sidecar from the object's previous
    // shape is removed (the gateway would ignore it via the ETag binding,
    // but leaving it would trip `sweep-orphan-sidecars`).
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
                        "object recompacted but sidecar write failed (Range GETs fall back \
                         to full reads; run `s4 repair-sidecar {bucket}/{key}`): {e}",
                        key = job.key
                    ),
                };
            }
        }
        Ok(_) => {
            // Single frame: no sidecar by design (gateway parity). Remove
            // a stale one if the previous shape had left it behind. The
            // existence HEAD keeps versioned buckets from accumulating
            // delete markers for never-existing sidecar keys.
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
                        "object recompacted but its now-stale sidecar could not be deleted \
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
fn fold_outcome(report: &mut RecompactReport, key: String, outcome: ObjectOutcome) {
    match outcome {
        ObjectOutcome::Recompacted {
            bytes_before,
            bytes_after,
        } => {
            report.recompacted += 1;
            report.recompacted_bytes_before += bytes_before;
            report.recompacted_bytes_after += bytes_after;
        }
        ObjectOutcome::Skipped(reason) => match reason {
            SkipReason::NotS4 => report.skipped_not_s4 += 1,
            SkipReason::AlreadyCompacted => report.skipped_already_compacted += 1,
            SkipReason::UnsupportedCodec => report.skipped_unsupported_codec += 1,
            SkipReason::UnstampedFramed => report.skipped_unstamped_framed += 1,
            SkipReason::InsufficientGain => report.skipped_insufficient_gain += 1,
            SkipReason::TooLarge => report.skipped_too_large += 1,
            SkipReason::EtagRaced => report.skipped_etag_raced += 1,
            SkipReason::TooRecent => report.skipped_too_recent += 1,
            SkipReason::TagsUnreadable => report.skipped_tags_unreadable += 1,
        },
        ObjectOutcome::Failed { op, cause } => {
            report.failed += 1;
            report.failures.push(RecompactFailure {
                key,
                op: op.to_owned(),
                cause,
            });
        }
    }
}

/// Run the full recompaction against `bucket`. Writes nothing unless
/// `params.execute` is set. Listing failures abort the run; per-object
/// failures are counted in the report (callers map `report.failed > 0`
/// to a non-zero exit).
pub async fn run_recompact(
    client: &Client,
    bucket: &str,
    params: &RecompactParams,
) -> Result<RecompactReport, RecompactError> {
    let concurrency = params.concurrency.max(1);

    let cutoff_epoch_secs: Option<i64> = params.older_than.map(|older| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let now_secs = i64::try_from(now_secs).unwrap_or(i64::MAX);
        let older_secs = i64::try_from(older.as_secs()).unwrap_or(i64::MAX);
        now_secs.saturating_sub(older_secs)
    });

    let inventory =
        list_inventory(client, bucket, params.prefix.as_deref(), params.max_objects).await?;
    // Best-effort versioning probe — downgrade failures to a note so an
    // ACL-restricted operator can still recompact.
    let (versioning, versioning_note) =
        match crate::migrate::versioning_enabled(client, bucket).await {
            Ok(v) => (v, None),
            Err(e) => (false, Some(format!("{e}"))),
        };

    let registry = Arc::new(build_registry(params));

    use futures::StreamExt as _;
    let results: Vec<(String, ObjectOutcome)> = futures::stream::iter(inventory.objects.iter())
        .map(|job| {
            let registry = &registry;
            async move {
                let outcome =
                    recompact_one(client, bucket, job, registry, params, cutoff_epoch_secs).await;
                (job.key.clone(), outcome)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let total_objects = inventory.objects.len() as u64;
    let total_bytes: u64 = inventory.objects.iter().map(|o| o.size).sum();
    let mut report = RecompactReport {
        bucket: bucket.to_owned(),
        prefix: params.prefix.clone(),
        dry_run: !params.execute,
        target_zstd_level: params.target_zstd_level,
        min_gain_percent: params.min_gain_percent,
        older_than_secs: params.older_than.map(|d| d.as_secs()),
        total_objects,
        total_bytes,
        listing_truncated: inventory.truncated,
        max_objects: params.max_objects,
        recompacted: 0,
        recompacted_bytes_before: 0,
        recompacted_bytes_after: 0,
        skipped_not_s4: 0,
        skipped_already_compacted: 0,
        skipped_unsupported_codec: 0,
        skipped_unstamped_framed: 0,
        skipped_insufficient_gain: 0,
        skipped_too_large: 0,
        skipped_etag_raced: 0,
        skipped_too_recent: 0,
        skipped_tags_unreadable: 0,
        no_tags: params.no_tags,
        failed: 0,
        failures: Vec::new(),
        versioning_enabled: versioning,
        notes: Vec::new(),
    };
    for (key, outcome) in results {
        fold_outcome(&mut report, key, outcome);
    }
    // `buffer_unordered` completion order is nondeterministic — sort for
    // stable output.
    report.failures.sort_by(|a, b| a.key.cmp(&b.key));

    if total_objects == 0 {
        report.notes.push("no objects found".into());
    }
    if report.dry_run {
        report.notes.push(
            "dry-run: nothing was written; counts and sizes are measured on the real \
             re-framed output — pass --execute to recompact"
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
             bucket defaults, so do not recompact buckets relying on per-object ACLs or \
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
            "WARNING: bucket versioning is Enabled — each recompacted object leaves its \
             previous version in place, so storage is double-billed until old versions \
             are lifecycle-expired"
                .into(),
        );
    }
    if let Some(cause) = versioning_note {
        report.notes.push(format!(
            "could not determine bucket versioning state ({cause}); if versioning is \
             Enabled, recompacted objects double-bill until old versions expire"
        ));
    }
    if report.listing_truncated {
        report.notes.push(format!(
            "listing truncated at --max-objects={}: keys beyond the first {} were not \
             examined (re-run to continue — already-compacted objects are skipped)",
            params
                .max_objects
                .map(|n| n.to_string())
                .unwrap_or_default(),
            total_objects,
        ));
    }
    if report.skipped_not_s4 > 0 {
        report.notes.push(format!(
            "{} object(s) skipped as not-s4 — they are not S4-framed; run `s4 migrate` \
             first to frame them, then recompact",
            report.skipped_not_s4,
        ));
    }
    if report.skipped_unstamped_framed > 0 {
        report.notes.push(format!(
            "{} object(s) skipped as unstamped-framed — S4F2/S4P1-framed bytes without \
             gateway metadata (backend-written?); pass --assume-unstamped-framed to \
             recompact them anyway",
            report.skipped_unstamped_framed,
        ));
    }
    if report.failed > 0 {
        report.notes.push(format!(
            "{} object(s) failed — see `failures`; re-running resumes automatically \
             (already-compacted objects are skipped)",
            report.failed,
        ));
    }

    Ok(report)
}

/// Render the default human-readable summary for `--format table`.
pub fn render_human(report: &RecompactReport) -> String {
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
    let _ = writeln!(out, "S4 recompact {target} — {mode}");
    let older = match report.older_than_secs {
        Some(secs) => format!("   older-than: {}", human_duration_secs(secs)),
        None => String::new(),
    };
    let _ = writeln!(
        out,
        "  target zstd level: {}   min gain: {}%{}",
        report.target_zstd_level, report.min_gain_percent, older,
    );
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
        "would recompact"
    } else {
        "recompacted"
    };
    let saved = report
        .recompacted_bytes_before
        .saturating_sub(report.recompacted_bytes_after);
    let _ = writeln!(
        out,
        "  {verb}: {} object(s), {} -> {} (saves {})",
        report.recompacted,
        human_bytes(report.recompacted_bytes_before),
        human_bytes(report.recompacted_bytes_after),
        human_bytes(saved),
    );
    let _ = writeln!(
        out,
        "  skipped: {} not-s4, {} already-compacted, {} unsupported-codec, \
         {} unstamped-framed, {} insufficient-gain, {} too-large, {} etag-raced, \
         {} too-recent, {} tags-unreadable",
        report.skipped_not_s4,
        report.skipped_already_compacted,
        report.skipped_unsupported_codec,
        report.skipped_unstamped_framed,
        report.skipped_insufficient_gain,
        report.skipped_too_large,
        report.skipped_etag_raced,
        report.skipped_too_recent,
        report.skipped_tags_unreadable,
    );
    let _ = writeln!(out, "  failed: {}", report.failed);
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
    fn duration_parser_accepts_simple_forms() {
        assert_eq!(
            parse_duration_suffix("30d").expect("30d"),
            Duration::from_secs(30 * 86_400)
        );
        assert_eq!(
            parse_duration_suffix("12h").expect("12h"),
            Duration::from_secs(12 * 3600)
        );
        assert_eq!(
            parse_duration_suffix("45m").expect("45m"),
            Duration::from_secs(45 * 60)
        );
        assert_eq!(
            parse_duration_suffix("90s").expect("90s"),
            Duration::from_secs(90)
        );
        assert_eq!(
            parse_duration_suffix(" 7d ".trim()).expect("trimmed"),
            Duration::from_secs(7 * 86_400)
        );
        assert_eq!(
            parse_duration_suffix("0d").expect("0d"),
            Duration::from_secs(0)
        );
    }

    #[test]
    fn duration_parser_rejects_bad_forms() {
        for bad in ["", "d", "10", "10x", "1.5h", "-3d", "1h30m", "10D", "h10"] {
            assert!(
                parse_duration_suffix(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        // u64-seconds overflow.
        assert!(parse_duration_suffix("999999999999999999999d").is_err());
        assert!(parse_duration_suffix(&format!("{}d", u64::MAX)).is_err());
    }

    #[test]
    fn human_duration_shortest_unit() {
        assert_eq!(human_duration_secs(30 * 86_400), "30d");
        assert_eq!(human_duration_secs(12 * 3600), "12h");
        assert_eq!(human_duration_secs(45 * 60), "45m");
        assert_eq!(human_duration_secs(90), "90s");
        assert_eq!(human_duration_secs(0), "0s");
        // 25h is not a whole number of days → falls to hours.
        assert_eq!(human_duration_secs(25 * 3600), "25h");
    }

    #[test]
    fn too_recent_gate() {
        // No cutoff: everything qualifies.
        assert!(!is_too_recent(None, None));
        assert!(!is_too_recent(Some(1_000), None));
        // Cutoff active: unknown LastModified is conservatively recent.
        assert!(is_too_recent(None, Some(1_000)));
        // At or before the cutoff = old enough.
        assert!(!is_too_recent(Some(1_000), Some(1_000)));
        assert!(!is_too_recent(Some(999), Some(1_000)));
        assert!(is_too_recent(Some(1_001), Some(1_000)));
    }

    #[test]
    fn already_compacted_stamp() {
        assert!(is_already_compacted(Some("19"), 19));
        assert!(is_already_compacted(Some("22"), 19));
        assert!(!is_already_compacted(Some("3"), 19));
        assert!(!is_already_compacted(None, 19));
        // Unparseable stamps count as "not compacted" (re-stamped on
        // the next successful rewrite).
        assert!(!is_already_compacted(Some("not-a-number"), 19));
        assert!(!is_already_compacted(Some(""), 19));
    }

    #[test]
    fn probe_classification_order() {
        let s4 = b"S4F2rest".as_slice();
        let raw = b"plain te".as_slice();
        // Candidate: cpu-zstd framed, no level stamp.
        assert_eq!(classify_probe(Some("cpu-zstd"), None, s4, 19, false), None);
        // Framed but no metadata at all: NOT a candidate by default —
        // the gateway and migrate always stamp, so unstamped S4F2 bytes
        // were written by something else and must not be promoted.
        assert_eq!(
            classify_probe(None, None, s4, 19, false),
            Some(SkipReason::UnstampedFramed)
        );
        // ... unless the operator opts in with --assume-unstamped-framed.
        assert_eq!(classify_probe(None, None, s4, 19, true), None);
        // Candidate: stamped below target.
        assert_eq!(
            classify_probe(Some("cpu-zstd"), Some("3"), s4, 19, false),
            None
        );
        // Already at/above target.
        assert_eq!(
            classify_probe(Some("cpu-zstd"), Some("19"), s4, 19, false),
            Some(SkipReason::AlreadyCompacted)
        );
        // Codec gate fires BEFORE the magic gate: a gateway passthrough
        // object (raw body + stamp) is unsupported-codec, not not-s4.
        assert_eq!(
            classify_probe(Some("passthrough"), None, raw, 19, false),
            Some(SkipReason::UnsupportedCodec)
        );
        for codec in ["cpu-gzip", "nvcomp-zstd", "nvcomp-bitcomp", "cpu-zstd-dict"] {
            assert_eq!(
                classify_probe(Some(codec), None, s4, 19, false),
                Some(SkipReason::UnsupportedCodec),
                "{codec} must be unsupported"
            );
        }
        // Plain object: not-s4 (with or without the opt-in flag — the
        // flag only widens the framed case, never the unframed one).
        assert_eq!(
            classify_probe(None, None, raw, 19, false),
            Some(SkipReason::NotS4)
        );
        assert_eq!(
            classify_probe(None, None, raw, 19, true),
            Some(SkipReason::NotS4)
        );
        // Legacy raw-zstd (cpu-zstd stamp, zstd magic, no framing).
        assert_eq!(
            classify_probe(Some("cpu-zstd"), None, &[0x28, 0xb5, 0x2f, 0xfd], 19, false),
            Some(SkipReason::NotS4)
        );
        // Padding magic counts as framed.
        assert_eq!(
            classify_probe(Some("cpu-zstd"), None, b"S4P1\0\0\0\0", 19, false),
            None
        );
        // Unstamped padding magic is gated like unstamped data frames.
        assert_eq!(
            classify_probe(None, None, b"S4P1\0\0\0\0", 19, false),
            Some(SkipReason::UnstampedFramed)
        );
    }

    #[test]
    fn min_gain_gate() {
        // 100 → 97 is exactly 3.0%.
        assert!(meets_min_gain(100, 97, 3.0));
        // 100 → 98 is 2.0% < 3.0%.
        assert!(!meets_min_gain(100, 98, 3.0));
        assert!(meets_min_gain(100, 50, 3.0));
        // Equal or growing output never rewrites, even at min-gain 0.
        assert!(!meets_min_gain(100, 100, 0.0));
        assert!(!meets_min_gain(100, 101, 0.0));
        assert!(meets_min_gain(100, 99, 0.0));
        // Degenerate inputs.
        assert!(!meets_min_gain(0, 0, 3.0));
        assert!(!meets_min_gain(0, 10, 3.0));
    }

    fn empty_report() -> RecompactReport {
        RecompactReport {
            bucket: "b".into(),
            prefix: None,
            dry_run: true,
            target_zstd_level: DEFAULT_TARGET_ZSTD_LEVEL,
            min_gain_percent: DEFAULT_MIN_GAIN_PERCENT,
            older_than_secs: None,
            total_objects: 0,
            total_bytes: 0,
            listing_truncated: false,
            max_objects: None,
            recompacted: 0,
            recompacted_bytes_before: 0,
            recompacted_bytes_after: 0,
            skipped_not_s4: 0,
            skipped_already_compacted: 0,
            skipped_unsupported_codec: 0,
            skipped_unstamped_framed: 0,
            skipped_insufficient_gain: 0,
            skipped_too_large: 0,
            skipped_etag_raced: 0,
            skipped_too_recent: 0,
            skipped_tags_unreadable: 0,
            no_tags: false,
            failed: 0,
            failures: Vec::new(),
            versioning_enabled: false,
            notes: Vec::new(),
        }
    }

    #[test]
    fn fold_outcome_aggregates_every_bucket() {
        let mut report = empty_report();
        fold_outcome(
            &mut report,
            "a".into(),
            ObjectOutcome::Recompacted {
                bytes_before: 1000,
                bytes_after: 800,
            },
        );
        fold_outcome(
            &mut report,
            "b".into(),
            ObjectOutcome::Recompacted {
                bytes_before: 500,
                bytes_after: 400,
            },
        );
        for (key, reason) in [
            ("c", SkipReason::NotS4),
            ("d", SkipReason::AlreadyCompacted),
            ("e", SkipReason::UnsupportedCodec),
            ("e2", SkipReason::UnstampedFramed),
            ("f", SkipReason::InsufficientGain),
            ("g", SkipReason::TooLarge),
            ("h", SkipReason::EtagRaced),
            ("i", SkipReason::TooRecent),
            ("i2", SkipReason::TagsUnreadable),
        ] {
            fold_outcome(&mut report, key.into(), ObjectOutcome::Skipped(reason));
        }
        fold_outcome(
            &mut report,
            "j".into(),
            ObjectOutcome::Failed {
                op: "PutObject",
                cause: "boom".into(),
            },
        );

        assert_eq!(report.recompacted, 2);
        assert_eq!(report.recompacted_bytes_before, 1500);
        assert_eq!(report.recompacted_bytes_after, 1200);
        assert_eq!(report.skipped_not_s4, 1);
        assert_eq!(report.skipped_already_compacted, 1);
        assert_eq!(report.skipped_unsupported_codec, 1);
        assert_eq!(report.skipped_unstamped_framed, 1);
        assert_eq!(report.skipped_insufficient_gain, 1);
        assert_eq!(report.skipped_too_large, 1);
        assert_eq!(report.skipped_etag_raced, 1);
        assert_eq!(report.skipped_too_recent, 1);
        assert_eq!(report.skipped_tags_unreadable, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.failures[0].key, "j");
        assert_eq!(report.failures[0].op, "PutObject");
    }

    fn dummy_report() -> RecompactReport {
        let mut r = empty_report();
        r.prefix = Some("logs/".into());
        r.dry_run = false;
        r.older_than_secs = Some(30 * 86_400);
        r.total_objects = 5;
        r.total_bytes = 5000;
        r.recompacted = 2;
        r.recompacted_bytes_before = 3000;
        r.recompacted_bytes_after = 2400;
        r.skipped_not_s4 = 1;
        r.skipped_unsupported_codec = 1;
        r.skipped_too_recent = 1;
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
        assert_eq!(v["target_zstd_level"], 19);
        assert_eq!(v["min_gain_percent"], 3.0);
        assert_eq!(v["older_than_secs"], 2_592_000);
        assert_eq!(v["total_objects"], 5);
        assert_eq!(v["total_bytes"], 5000);
        assert_eq!(v["listing_truncated"], false);
        assert_eq!(v["max_objects"], serde_json::Value::Null);
        assert_eq!(v["recompacted"], 2);
        assert_eq!(v["recompacted_bytes_before"], 3000);
        assert_eq!(v["recompacted_bytes_after"], 2400);
        assert_eq!(v["skipped_not_s4"], 1);
        assert_eq!(v["skipped_already_compacted"], 0);
        assert_eq!(v["skipped_unsupported_codec"], 1);
        assert_eq!(v["skipped_unstamped_framed"], 0);
        assert_eq!(v["skipped_insufficient_gain"], 0);
        assert_eq!(v["skipped_too_large"], 0);
        assert_eq!(v["skipped_etag_raced"], 0);
        assert_eq!(v["skipped_too_recent"], 1);
        assert_eq!(v["skipped_tags_unreadable"], 0);
        assert_eq!(v["no_tags"], false);
        assert_eq!(v["failed"], 0);
        assert_eq!(v["versioning_enabled"], true);
        assert!(v["notes"].as_array().is_some_and(|a| !a.is_empty()));
        // Skip-reason serde casing (kebab-case like migrate's).
        assert_eq!(
            serde_json::to_value(SkipReason::NotS4).expect("skip reason"),
            "not-s4"
        );
        assert_eq!(
            serde_json::to_value(SkipReason::AlreadyCompacted).expect("skip reason"),
            "already-compacted"
        );
        assert_eq!(
            serde_json::to_value(SkipReason::UnsupportedCodec).expect("skip reason"),
            "unsupported-codec"
        );
        assert_eq!(
            serde_json::to_value(SkipReason::UnstampedFramed).expect("skip reason"),
            "unstamped-framed"
        );
        assert_eq!(
            serde_json::to_value(SkipReason::InsufficientGain).expect("skip reason"),
            "insufficient-gain"
        );
        assert_eq!(
            serde_json::to_value(SkipReason::TooRecent).expect("skip reason"),
            "too-recent"
        );
        assert_eq!(
            serde_json::to_value(SkipReason::TagsUnreadable).expect("skip reason"),
            "tags-unreadable"
        );
    }

    #[test]
    fn render_human_mentions_key_figures() {
        let txt = render_human(&dummy_report());
        assert!(txt.contains("S4 recompact b/logs/"));
        assert!(txt.contains("execute"));
        assert!(txt.contains("target zstd level: 19"));
        assert!(txt.contains("min gain: 3%"));
        assert!(txt.contains("older-than: 30d"));
        assert!(txt.contains("recompacted: 2 object(s)"));
        assert!(txt.contains("not-s4"));
        assert!(txt.contains("too-recent"));
        assert!(txt.contains("Notes:"));
        assert!(txt.contains("versioning is Enabled"));

        let mut dry = dummy_report();
        dry.dry_run = true;
        dry.older_than_secs = None;
        let txt = render_human(&dry);
        assert!(txt.contains("dry-run (pass --execute to write)"));
        assert!(txt.contains("would recompact: 2 object(s)"));
        assert!(!txt.contains("older-than:"));
    }

    /// Frame real bytes at level 3 (the gateway's latency-first write),
    /// decode them back through `decode_frames`, re-frame at level 19,
    /// and confirm (a) the decode recovers the original byte-for-byte,
    /// (b) the level-19 output is smaller and survives the mandatory
    /// roundtrip verify, (c) tampered frames surface as `Corrupt`, and
    /// (d) a non-cpu-zstd frame stream bails as unsupported.
    #[tokio::test]
    async fn decode_then_recompress_pipeline() {
        // Varied (not purely repetitive) text so the level-19 advantage
        // is real, mirroring the e2e seed data.
        let mut text = String::new();
        for i in 0..60_000u64 {
            use std::fmt::Write as _;
            let _ = writeln!(
                text,
                "level=info req={i:08} user=u{} path=/api/v1/items/{} status={} latency_ms={}",
                i % 997,
                (i * 7) % 10_000,
                if i % 17 == 0 { 404 } else { 200 },
                i % 250,
            );
        }
        let original = Bytes::from(text.into_bytes());
        assert!(original.len() > 4 * 1024 * 1024, "must be multi-frame");

        let low = Arc::new(
            CodecRegistry::new(CodecKind::CpuZstd)
                .with(Arc::new(Passthrough))
                .with(Arc::new(CpuZstd::new(3))),
        );
        let chunk = pick_chunk_size(Some(original.len() as u64));
        let (level3, _) = streaming_compress_to_frames(
            bytes_blob(original.clone()),
            Arc::clone(&low),
            CodecKind::CpuZstd,
            chunk,
            Some(original.len() as u64),
        )
        .await
        .expect("level-3 frame");

        let params = RecompactParams {
            prefix: None,
            execute: false,
            concurrency: DEFAULT_RECOMPACT_CONCURRENCY,
            max_objects: None,
            max_body_bytes: u64::MAX,
            target_zstd_level: DEFAULT_TARGET_ZSTD_LEVEL,
            min_gain_percent: DEFAULT_MIN_GAIN_PERCENT,
            older_than: None,
            assume_unstamped_framed: false,
            no_tags: false,
        };
        let registry = Arc::new(build_registry(&params));

        // (a) decode recovers the original.
        let decoded = match decode_frames(&registry, level3.clone(), u64::MAX).await {
            DecodeOutcome::Decoded(b) => b,
            other => panic!(
                "decode must succeed, got {:?}",
                std::mem::discriminant(&other)
            ),
        };
        assert_eq!(decoded, original);

        // (b) level-19 re-frame shrinks and verifies.
        let (level19, manifest) = streaming_compress_to_frames(
            bytes_blob(decoded.clone()),
            Arc::clone(&registry),
            CodecKind::CpuZstd,
            pick_chunk_size(Some(decoded.len() as u64)),
            Some(decoded.len() as u64),
        )
        .await
        .expect("level-19 frame");
        assert_eq!(manifest.original_size, original.len() as u64);
        assert!(
            meets_min_gain(level3.len() as u64, level19.len() as u64, 3.0),
            "level 19 must shrink level-3 frames by >= 3% on this corpus \
             ({} -> {})",
            level3.len(),
            level19.len(),
        );
        assert!(crate::migrate::verify_roundtrip(&registry, level19.clone(), &decoded).await);

        // Decoded-size cap enforcement.
        assert!(matches!(
            decode_frames(&registry, level3.clone(), 1024).await,
            DecodeOutcome::ExceedsCap
        ));

        // (c) tampered payload byte → Corrupt.
        let mut tampered = level3.to_vec();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0xff;
        assert!(matches!(
            decode_frames(&registry, Bytes::from(tampered), u64::MAX).await,
            DecodeOutcome::Corrupt(_)
        ));

        // (d) a passthrough-codec frame stream is out of scope.
        let mut pt_framed = BytesMut::new();
        s4_codec::multipart::write_frame(
            &mut pt_framed,
            s4_codec::multipart::FrameHeader {
                codec: CodecKind::Passthrough,
                original_size: 5,
                compressed_size: 5,
                crc32c: crc32c::crc32c(b"hello"),
            },
            b"hello",
        );
        assert!(matches!(
            decode_frames(&registry, pt_framed.freeze(), u64::MAX).await,
            DecodeOutcome::UnsupportedFrameCodec
        ));
    }
}
