//! v1.1: `s4 estimate` — read-only pre-deployment savings simulator.
//!
//! Answers "how many GB / dollars would S4 save on this existing bucket?"
//! **before** the gateway is deployed. The tool:
//!
//! 1. Lists the bucket (or a prefix) via `ListObjectsV2`, excluding
//!    S4-internal keys (`.s4index` sidecars, `.s4dict/` shared
//!    dictionaries, `.__s4ver__/` versioning shadow keys — counting
//!    those would skew the projection), capped at `max_list_keys`.
//! 2. Stratifies objects by file extension (`.log`, `.json`, … or
//!    `"(none)"`), then draws up to `samples_per_stratum` objects per
//!    stratum, **size-weighted without replacement**, from a seeded
//!    `StdRng` so two runs with the same seed sample the same objects.
//! 3. GETs each sample (a `Range: bytes=0-N` prefix when the object is
//!    larger than `max_sample_bytes`), runs the **same**
//!    [`SamplingDispatcher`] decision the server would run at PUT time,
//!    actually compresses the bytes with the chosen CPU codec, and
//!    measures the ratio.
//! 4. Extrapolates per stratum: `sample-size-weighted mean ratio ×
//!    stratum total bytes`, then sums across strata and converts to
//!    $/month at `price_per_gb_month`.
//!
//! ## Honesty constraints (read before editing the output text)
//!
//! - **GPU codecs are never executed here.** When the dispatcher's pick
//!   is an `nvcomp-*` / `dietgpu-*` kind (because the operator passed
//!   `--prefer-columnar-gpu` or the build actually has a GPU), the
//!   measurement falls back to `cpu-zstd` as a **proxy** and the report
//!   carries an explicit note saying so. Bitcomp on integer columns
//!   typically compresses *better* than the cpu-zstd proxy, so the
//!   proxy is conservative there — but we never print a number we did
//!   not measure.
//! - The estimate covers **storage bytes only**. Request, egress and
//!   (on GPU deployments) compute costs are unchanged by S4; the report
//!   says so in a fixed note.
//! - **Frame overhead**: each measured compressed size includes exactly
//!   one S4F2 frame header ([`FRAME_HEADER_BYTES`] = 28 bytes). The
//!   real server splits large bodies into multiple frames
//!   (`DEFAULT_S4F2_CHUNK_SIZE` = 4 MiB per frame), adding 28 bytes per
//!   extra frame — ≤ ~7×10⁻⁶ of the body at that frame size, so the
//!   single-frame simplification is deliberately ignored here (and
//!   disclosed in this comment rather than silently).
//! - **Prefix sampling**: a `Range` GET of the first `max_sample_bytes`
//!   measures the head of the object, which can compress differently
//!   from the tail (e.g. a sorted column store). The report carries a
//!   fixed note about this.
//! - **Single-stream measurement bias**: each sample is compressed as
//!   one continuous zstd stream, but the real server resets the
//!   stream at every 4 MiB chunk boundary
//!   (`DEFAULT_S4F2_CHUNK_SIZE`), losing cross-chunk history. Ratios
//!   measured on up-to-8-MiB single-stream samples are therefore
//!   slightly **optimistic** versus what the gateway will store. The
//!   report carries a fixed note about this too.
//! - **Sample races**: an object deleted between the listing and its
//!   sample GET (404 / NoSuchKey) is skipped — like the empty-body
//!   race — and counted in a report note instead of aborting the run.
//! - **Already-S4 objects are excluded from the measurement**: on a
//!   bucket the gateway has already been writing to, sampled bodies
//!   that carry the `s4-codec` / `s4-encrypted` metadata stamp or
//!   start with the `S4F2`/`S4P1` frame magic or an `S4E*` SSE magic
//!   are framed/encrypted (≈ incompressible) bytes — measuring them
//!   would drag the stratum ratio toward 1.0 and produce a garbage
//!   estimate. They are counted per stratum (`already_s4`) and in a
//!   report note instead. Their **listed bytes still count toward the
//!   totals and projections** (we only know already-S4 status for the
//!   sampled subset), which the note discloses.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use aws_sdk_s3::Client;
use bytes::Bytes;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use s4_codec::cpu_gzip::CpuGzip;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::{AlwaysDispatcher, SamplingDispatcher};
use s4_codec::multipart::FRAME_HEADER_BYTES;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecDispatcher, CodecKind, CodecRegistry};
use serde::Serialize;
use thiserror::Error;

/// Default `--max-list-keys`: stop listing after this many (non-sidecar)
/// objects so a 500M-key bucket doesn't turn a pre-sales estimate into an
/// hour-long LIST bill. The report flags the truncation explicitly.
pub const DEFAULT_MAX_LIST_KEYS: usize = 100_000;
/// Default `--samples-per-stratum`: 8 GETs per extension stratum is enough
/// to stabilise the weighted-mean ratio on homogeneous strata (logs,
/// JSON dumps) while keeping the total GET count small.
pub const DEFAULT_SAMPLES_PER_STRATUM: usize = 8;
/// Default `--max-sample-bytes`: 8 MiB. Objects larger than this are
/// measured on a `Range: bytes=0-…` prefix GET.
pub const DEFAULT_MAX_SAMPLE_BYTES: u64 = 8 * 1024 * 1024;
/// Default `--seed` for the deterministic size-weighted sampler.
pub const DEFAULT_SEED: u64 = 42;
/// Default `--price-per-gb-month`: AWS S3 Standard us-east-1 first-50TB
/// tier ($0.023/GB-month). The dollar figures scale linearly — operators
/// on other tiers / providers pass their own price.
pub const DEFAULT_PRICE_PER_GB_MONTH: f64 = 0.023;

/// Dispatcher decision sample size. MUST stay in sync with the server's
/// PUT path (`service.rs` private `SAMPLE_BYTES = 4096`) so the estimate
/// picks the same codec the gateway would pick at runtime.
const DISPATCH_SAMPLE_BYTES: usize = 4096;

/// Metadata key the gateway stamps on SSE-S4 encrypted objects — the
/// literal `"s4-encrypted"` written by `service.rs` (which has no
/// exported constant for it; keep in sync).
const META_ENCRYPTED: &str = "s4-encrypted";

/// Bytes per GB for the $/month conversion. AWS bills storage in
/// binary gigabytes (GiB) despite the "GB" label, so we use 2³⁰.
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// v1.1 stability: `#[non_exhaustive]` — new estimate-time failure modes
/// may be added in minor releases. Downstream callers must include a
/// `_ =>` arm when matching on this enum.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EstimateError {
    #[error("S3 backend error on {op} {bucket}/{key}: {cause}")]
    Backend {
        op: &'static str,
        bucket: String,
        // Empty for bucket-level ops (ListObjectsV2).
        key: String,
        // Named `cause` (not `source`) so thiserror doesn't auto-treat it
        // as a `#[source]` chain field — the upstream SDK error is already
        // stringified into `cause`. Same convention as `repair::RepairError`.
        cause: String,
    },
    #[error("codec error while measuring {bucket}/{key}: {cause}")]
    Codec {
        bucket: String,
        key: String,
        cause: String,
    },
    #[error("invalid estimate target {target:?}: {reason}")]
    InvalidTarget { target: String, reason: String },
}

/// Knobs for one estimate run. Mirrors the server's codec-selection
/// surface (`--codec` / `--dispatcher` / `--zstd-level` /
/// `--gpu-min-bytes` / `--prefer-columnar-gpu`) plus the estimate-only
/// sampling flags, so the simulated pick matches what the deployed
/// gateway would choose.
#[derive(Debug, Clone)]
pub struct EstimateParams {
    /// Restrict the listing to keys under this prefix.
    pub prefix: Option<String>,
    /// Stop listing after this many non-sidecar objects (truncation is
    /// flagged in the report).
    pub max_list_keys: usize,
    /// Objects sampled (GET + compressed) per extension stratum.
    /// Values below 1 are clamped to 1.
    pub samples_per_stratum: usize,
    /// Per-sample byte cap; larger objects are measured on a Range-GET
    /// prefix of this many bytes. Values below 1 are clamped to 1.
    pub max_sample_bytes: u64,
    /// RNG seed for the deterministic size-weighted sampler.
    pub seed: u64,
    /// $/GB-month used for the before/after cost lines.
    pub price_per_gb_month: f64,
    /// Server `--codec` (the dispatcher's default pick).
    pub default_codec: CodecKind,
    /// Server `--zstd-level` (level used for cpu-zstd measurement,
    /// including the GPU-proxy path).
    pub zstd_level: i32,
    /// Server `--dispatcher`: `true` = sampling (entropy + magic-byte
    /// auto-selection), `false` = always (fixed `default_codec`).
    pub use_sampling_dispatcher: bool,
    /// Server `--gpu-min-bytes`.
    pub gpu_min_bytes: usize,
    /// Server `--prefer-columnar-gpu`.
    pub prefer_columnar_gpu: bool,
    /// When `true`, the simulated dispatcher behaves as if a GPU were
    /// present at runtime (picks may be `nvcomp-*`); the measurement
    /// still runs on CPU (`cpu-zstd` proxy) and the report notes it.
    /// Callers set this from the real GPU probe OR from explicit
    /// operator intent (`--prefer-columnar-gpu` on a CPU build = "I am
    /// planning a GPU deployment").
    pub simulate_gpu: bool,
    /// Whether this build/host actually has a usable GPU (for honest
    /// wording in the proxy note). Independent from `simulate_gpu`.
    pub gpu_present: bool,
}

/// Per-runtime-codec pick count inside one stratum.
#[derive(Debug, Clone, Serialize)]
pub struct CodecPicks {
    /// Codec the **deployed server** would choose (`CodecKind::as_str()`
    /// name, e.g. `"nvcomp-bitcomp"`).
    pub codec: String,
    /// Number of sampled objects that picked this codec.
    pub picks: u64,
    /// Codec the ratio was actually **measured** with. Differs from
    /// `codec` only on the GPU-proxy path (`"cpu-zstd"`).
    pub measured_with: String,
}

/// Aggregated result for one extension stratum.
#[derive(Debug, Clone, Serialize)]
pub struct StratumReport {
    /// `".log"`, `".json"`, … or `"(none)"` for extension-less keys.
    pub stratum: String,
    /// Objects listed in this stratum.
    pub objects: u64,
    /// Total listed bytes in this stratum.
    pub bytes: u64,
    /// Objects actually sampled (GET + compressed).
    pub sampled: u64,
    /// Drawn samples excluded because the object is already S4-managed
    /// (framed or encrypted by the gateway / `s4 migrate`) — already
    /// compressed bytes that must not contaminate `ratio`. See the
    /// `already S4-managed` report note.
    pub already_s4: u64,
    /// Bytes actually fetched and compressed across the samples
    /// (post Range-GET cap).
    pub sampled_bytes_read: u64,
    /// Runtime-codec breakdown of the samples.
    pub codecs: Vec<CodecPicks>,
    /// Sample-size-weighted mean of `framed_compressed / bytes_read`,
    /// including one 28-byte S4F2 frame header per object. `1.0` when
    /// nothing was sampled (e.g. all-zero-byte stratum).
    pub ratio: f64,
    /// `ratio × bytes`, rounded.
    pub projected_bytes: u64,
}

/// Full result of one estimate run. Serializes to the `--format json`
/// output verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct EstimateReport {
    pub bucket: String,
    pub prefix: Option<String>,
    /// Objects listed (after `.s4index` exclusion, before sampling).
    pub total_objects: u64,
    pub total_bytes: u64,
    /// `true` when listing stopped at `max_list_keys` with more keys
    /// remaining — totals then cover only the listed subset.
    pub listing_truncated: bool,
    pub max_list_keys: usize,
    pub sampled_objects: u64,
    /// Bytes actually fetched + compressed (post Range-GET cap).
    pub sampled_bytes_read: u64,
    /// Σ full sizes of the sampled objects ÷ `total_bytes` — the
    /// bytes-basis coverage of the extrapolation.
    pub sampled_fraction_of_total_bytes: f64,
    pub strata: Vec<StratumReport>,
    /// `projected_total_bytes / total_bytes` (1.0 on an empty bucket).
    pub overall_ratio: f64,
    pub projected_total_bytes: u64,
    pub price_per_gb_month: f64,
    pub current_monthly_cost_usd: f64,
    pub projected_monthly_cost_usd: f64,
    pub seed: u64,
    /// Fixed honesty notes + run-specific caveats (truncation,
    /// GPU-proxy measurements). Always read these before quoting the
    /// numbers anywhere.
    pub notes: Vec<String>,
}

/// Parse `<bucket>` or `<bucket>/<prefix>` (slashes after the first
/// belong to the prefix). An empty prefix after a trailing `/` is
/// treated as "no prefix".
pub fn parse_bucket_prefix(target: &str) -> Result<(String, Option<String>), EstimateError> {
    let (bucket, prefix) = match target.split_once('/') {
        Some((b, p)) => (
            b,
            if p.is_empty() {
                None
            } else {
                Some(p.to_owned())
            },
        ),
        None => (target, None),
    };
    if bucket.is_empty() {
        return Err(EstimateError::InvalidTarget {
            target: target.to_owned(),
            reason: "bucket name is empty".into(),
        });
    }
    Ok((bucket.to_owned(), prefix))
}

/// Extension stratum for a key: lowercased extension of the final path
/// segment **with the leading dot** (`".log"`), or `"(none)"` when the
/// segment has no extension (including dotfiles like `.gitignore`,
/// matching `std::path::Path::extension` semantics).
pub fn stratum_for_key(key: &str) -> String {
    let file_name = key.rsplit('/').next().unwrap_or(key);
    match file_name.rfind('.') {
        // Dot at position 0 = dotfile without extension; dot at the very
        // end = trailing-dot name, also "(none)".
        Some(0) | None => "(none)".to_owned(),
        Some(i) if i + 1 == file_name.len() => "(none)".to_owned(),
        Some(i) => format!(".{}", file_name[i + 1..].to_lowercase()),
    }
}

/// Size-weighted sampling **without replacement**: draw up to `k`
/// distinct indices from `sizes`, each draw proportional to the
/// remaining objects' sizes. Zero-byte objects are never drawn (there
/// is nothing to compress). Deterministic for a given RNG state; the
/// returned indices are sorted for stable downstream iteration.
fn weighted_sample_indices(sizes: &[u64], k: usize, rng: &mut StdRng) -> Vec<usize> {
    let mut remaining: Vec<usize> = (0..sizes.len()).filter(|&i| sizes[i] > 0).collect();
    let mut picked = Vec::with_capacity(k.min(remaining.len()));
    while picked.len() < k && !remaining.is_empty() {
        let total: u128 = remaining.iter().map(|&i| u128::from(sizes[i])).sum();
        // `remaining` only holds size>0 indices, so total >= 1.
        let mut t = rng.gen_range(0..total);
        let mut chosen_pos = remaining.len() - 1;
        for (pos, &i) in remaining.iter().enumerate() {
            let w = u128::from(sizes[i]);
            if t < w {
                chosen_pos = pos;
                break;
            }
            t -= w;
        }
        picked.push(remaining.swap_remove(chosen_pos));
    }
    picked.sort_unstable();
    picked
}

/// One measured sample: `(full object size, measured ratio)`.
/// `ratio = (compressed + FRAME_HEADER_BYTES) / bytes_read`.
type WeightedRatio = (u64, f64);

/// Sample-size-weighted mean ratio. Weights are **full object sizes**
/// (not bytes read), so a 5 GiB object sampled via an 8 MiB prefix
/// still dominates the stratum mean the way it dominates the bill.
/// Returns 1.0 (no change) for an empty sample set.
fn weighted_mean_ratio(samples: &[WeightedRatio]) -> f64 {
    let total_w: f64 = samples.iter().map(|&(w, _)| w as f64).sum();
    if total_w <= 0.0 {
        return 1.0;
    }
    let acc: f64 = samples.iter().map(|&(w, r)| (w as f64) * r).sum();
    acc / total_w
}

/// Map the dispatcher's runtime pick to the codec we can actually run
/// here (CPU-only). Returns `(measurement kind, is_gpu_proxy)`.
/// `nvcomp-*` / `dietgpu-*` (and any future non-CPU kind — the enum is
/// `#[non_exhaustive]`) fall back to `cpu-zstd` as a proxy.
fn measurement_kind(pick: CodecKind) -> (CodecKind, bool) {
    match pick {
        CodecKind::Passthrough | CodecKind::CpuZstd | CodecKind::CpuGzip => (pick, false),
        _ => (CodecKind::CpuZstd, true),
    }
}

/// `true` when the sample GET's metadata marks the object as already
/// S4-managed: the `s4-codec` manifest stamp (framed / legacy raw-zstd
/// / passthrough gateway writes) or the `s4-encrypted` SSE stamp.
/// Same metadata-first policy as `migrate`'s already-S4 probe.
fn is_already_s4_metadata(metadata: Option<&HashMap<String, String>>) -> bool {
    metadata.is_some_and(|m| {
        m.contains_key(crate::service::META_CODEC) || m.contains_key(META_ENCRYPTED)
    })
}

/// `true` when the sampled body bytes themselves are S4-managed output:
/// an `S4F2`/`S4P1` frame stream or an `S4E1`..`S4E6` SSE-S4 ciphertext
/// ([`crate::sse::looks_encrypted`] — length-gated at the 36-byte
/// minimum header, so a < 36-byte sample can only be caught via the
/// metadata stamp). Covers objects whose metadata was stripped (e.g. a
/// backend-side copy).
///
/// Audit R3 P3: the 4-byte magic alone is not enough — a legitimate
/// customer object could start with the same bytes and would silently
/// drop out of the measurement. For `S4F2` we additionally require the
/// fixed header to parse structurally (known codec id, payload length
/// that fits inside the object); for `S4P1` the padding length must fit.
fn is_already_s4_body(body_prefix: &[u8], object_size: u64) -> bool {
    use s4_codec::multipart::{
        FRAME_HEADER_BYTES, FRAME_MAGIC, PADDING_HEADER_BYTES, PADDING_MAGIC,
    };
    if body_prefix.starts_with(FRAME_MAGIC) {
        if body_prefix.len() < FRAME_HEADER_BYTES {
            return false; // shorter than one header can't be a frame stream
        }
        let (Ok(codec_bytes), Ok(size_bytes)) = (
            <[u8; 4]>::try_from(&body_prefix[4..8]),
            <[u8; 8]>::try_from(&body_prefix[16..24]),
        ) else {
            return false; // unreachable given the length check above
        };
        let codec_id = u32::from_le_bytes(codec_bytes);
        let compressed_size = u64::from_le_bytes(size_bytes);
        return s4_codec::CodecKind::from_id(codec_id).is_some()
            && compressed_size.saturating_add(FRAME_HEADER_BYTES as u64) <= object_size;
    }
    if body_prefix.starts_with(PADDING_MAGIC) {
        if body_prefix.len() < PADDING_HEADER_BYTES {
            return false;
        }
        let Ok(pad_bytes) = <[u8; 8]>::try_from(&body_prefix[4..12]) else {
            return false; // unreachable given the length check above
        };
        let pad_len = u64::from_le_bytes(pad_bytes);
        return pad_len.saturating_add(PADDING_HEADER_BYTES as u64) <= object_size;
    }
    crate::sse::looks_encrypted(body_prefix)
}

#[derive(Debug, Clone)]
struct ListedObject {
    key: String,
    size: u64,
}

struct Inventory {
    objects: Vec<ListedObject>,
    truncated: bool,
}

/// Paginate `ListObjectsV2`, skipping internal keys (`.s4index`
/// sidecars, `.s4dict/` dictionaries, `.__s4ver__/` version shadows),
/// stopping at `max_list_keys` collected objects. Same pagination
/// shape as `repair::sweep_orphan_sidecars`.
async fn list_inventory(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    max_list_keys: usize,
) -> Result<Inventory, EstimateError> {
    let mut objects: Vec<ListedObject> = Vec::new();
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
        let resp = req.send().await.map_err(|e| EstimateError::Backend {
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
            if objects.len() >= max_list_keys {
                truncated = true;
                break 'pages;
            }
            let size = obj.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0);
            objects.push(ListedObject {
                key: k.to_owned(),
                size,
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

/// Build the same dispatcher the server would build from the same flags
/// (`main.rs::build_dispatcher`), except `prefer_gpu` comes from
/// `simulate_gpu` so a CPU-only estimate host can still predict the
/// pick a GPU deployment would make.
fn build_sim_dispatcher(params: &EstimateParams) -> Arc<dyn CodecDispatcher> {
    if params.use_sampling_dispatcher {
        Arc::new(
            SamplingDispatcher::new(params.default_codec)
                .with_gpu_preference(params.simulate_gpu, params.gpu_min_bytes)
                .with_columnar_gpu_preference(params.simulate_gpu && params.prefer_columnar_gpu),
        )
    } else {
        Arc::new(AlwaysDispatcher(params.default_codec))
    }
}

/// CPU-only measurement registry: `passthrough` + `cpu-zstd` (at the
/// server's `--zstd-level`) + `cpu-gzip`. GPU codecs are intentionally
/// absent — see the module docs.
fn build_measurement_registry(params: &EstimateParams) -> CodecRegistry {
    CodecRegistry::new(CodecKind::CpuZstd)
        .with(Arc::new(Passthrough))
        .with(Arc::new(CpuZstd::new(params.zstd_level)))
        .with(Arc::new(CpuGzip::default()))
}

struct SampleMeasurement {
    full_size: u64,
    bytes_read: u64,
    /// Compressed size + one S4F2 frame header.
    framed_bytes: u64,
    runtime_codec: CodecKind,
    gpu_proxy: bool,
}

/// One sample's fate. The two skip arms are benign mid-run races (the
/// bucket changed between the listing and the GET); neither aborts
/// the whole estimate.
enum SampleOutcome {
    Measured(SampleMeasurement),
    /// 404 / NoSuchKey: the listed object was deleted before its
    /// sample GET. Counted in a report note.
    Missing,
    /// Listed size said > 0 but the body came back empty (raced
    /// overwrite). Skipped rather than divide by zero.
    Empty,
    /// The object is already S4-managed (`s4-codec` / `s4-encrypted`
    /// metadata, or `S4F2`/`S4P1`/`S4E*` body magic): it is already
    /// compressed/encrypted gateway output, so measuring it would
    /// poison the stratum ratio. Counted per stratum and disclosed in
    /// a report note.
    AlreadyS4,
}

/// `true` when a sample GET failed because the object no longer exists
/// (deleted between the listing and the GET): the modeled `NoSuchKey`
/// service error, or any raw HTTP 404 (covers backends that 404
/// without the modeled code). Split out of [`measure_one`] so the
/// classification is unit-testable without a network.
fn is_missing_object_error(
    service_error: Option<&aws_sdk_s3::operation::get_object::GetObjectError>,
    http_status: Option<u16>,
) -> bool {
    service_error.is_some_and(|e| e.is_no_such_key()) || http_status == Some(404)
}

/// GET (optionally Range-limited) + dispatch + compress one sample.
async fn measure_one(
    client: &Client,
    bucket: &str,
    obj: &ListedObject,
    dispatcher: &Arc<dyn CodecDispatcher>,
    registry: &CodecRegistry,
    max_sample_bytes: u64,
) -> Result<SampleOutcome, EstimateError> {
    let mut req = client.get_object().bucket(bucket).key(&obj.key);
    if obj.size > max_sample_bytes {
        req = req.range(format!("bytes=0-{}", max_sample_bytes - 1));
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            // A listed object deleted mid-run is a benign race, not a
            // reason to abort the whole estimate.
            let status = e.raw_response().map(|r| r.status().as_u16());
            if is_missing_object_error(e.as_service_error(), status) {
                return Ok(SampleOutcome::Missing);
            }
            return Err(EstimateError::Backend {
                op: "GetObject",
                bucket: bucket.into(),
                key: obj.key.clone(),
                cause: format!("{e}"),
            });
        }
    };
    // Already-S4 gate, metadata first (covers every gateway-written
    // shape, including ones whose body magic a short Range GET would
    // miss) — see the module docs.
    if is_already_s4_metadata(resp.metadata()) {
        return Ok(SampleOutcome::AlreadyS4);
    }
    let body = resp
        .body
        .collect()
        .await
        .map_err(|e| EstimateError::Backend {
            op: "GetObject(body)",
            bucket: bucket.into(),
            key: obj.key.clone(),
            cause: format!("{e}"),
        })?
        .into_bytes();
    if body.is_empty() {
        return Ok(SampleOutcome::Empty);
    }
    // Body-magic gate: S4F2/S4P1 frames or S4E* ciphertext written by
    // the gateway but with the metadata stamp stripped (backend copy).
    // Structurally validated against the full object size (audit R3 P3).
    if is_already_s4_body(&body, obj.size) {
        return Ok(SampleOutcome::AlreadyS4);
    }
    let sample_len = body.len().min(DISPATCH_SAMPLE_BYTES);
    // Same call shape as the server PUT path: prefix sample + the full
    // object size as the size hint (the server knows Content-Length on
    // normal PUTs; LIST gives us the same number here).
    let pick = dispatcher
        .pick_with_size_hint(&body[..sample_len], Some(obj.size))
        .await;
    let (measure_with, gpu_proxy) = measurement_kind(pick);
    let (compressed, _manifest) = registry
        .compress(Bytes::copy_from_slice(&body), measure_with)
        .await
        .map_err(|e| EstimateError::Codec {
            bucket: bucket.into(),
            key: obj.key.clone(),
            cause: format!("{e}"),
        })?;
    Ok(SampleOutcome::Measured(SampleMeasurement {
        full_size: obj.size,
        bytes_read: body.len() as u64,
        framed_bytes: compressed.len() as u64 + FRAME_HEADER_BYTES as u64,
        runtime_codec: pick,
        gpu_proxy,
    }))
}

/// Run the full estimate against `bucket` (read-only: ListObjectsV2 +
/// GetObject only, never PUT/DELETE).
pub async fn run_estimate(
    client: &Client,
    bucket: &str,
    params: &EstimateParams,
) -> Result<EstimateReport, EstimateError> {
    let samples_per_stratum = params.samples_per_stratum.max(1);
    let max_sample_bytes = params.max_sample_bytes.max(1);

    let inventory = list_inventory(
        client,
        bucket,
        params.prefix.as_deref(),
        params.max_list_keys,
    )
    .await?;

    // Stratify. BTreeMap = deterministic stratum order, which together
    // with the single seeded RNG makes the whole sample set a pure
    // function of (listing order, seed).
    let mut strata: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, obj) in inventory.objects.iter().enumerate() {
        strata.entry(stratum_for_key(&obj.key)).or_default().push(i);
    }

    let dispatcher = build_sim_dispatcher(params);
    let registry = build_measurement_registry(params);
    let mut rng = StdRng::seed_from_u64(params.seed);

    let mut stratum_reports: Vec<StratumReport> = Vec::with_capacity(strata.len());
    let mut sampled_objects: u64 = 0;
    let mut sampled_bytes_read: u64 = 0;
    let mut sampled_full_bytes: u64 = 0;
    let mut samples_missing: u64 = 0;
    let mut samples_already_s4: u64 = 0;
    let mut proxied_codecs: BTreeMap<String, u64> = BTreeMap::new();

    for (stratum, indices) in &strata {
        let sizes: Vec<u64> = indices.iter().map(|&i| inventory.objects[i].size).collect();
        let stratum_bytes: u64 = sizes.iter().sum();
        let chosen = weighted_sample_indices(&sizes, samples_per_stratum, &mut rng);

        let mut measurements: Vec<WeightedRatio> = Vec::with_capacity(chosen.len());
        let mut codec_counts: BTreeMap<String, (u64, String)> = BTreeMap::new();
        let mut stratum_read: u64 = 0;
        let mut stratum_already_s4: u64 = 0;
        for &pos in &chosen {
            let obj = &inventory.objects[indices[pos]];
            let m = match measure_one(
                client,
                bucket,
                obj,
                &dispatcher,
                &registry,
                max_sample_bytes,
            )
            .await?
            {
                SampleOutcome::Measured(m) => m,
                SampleOutcome::Missing => {
                    samples_missing += 1;
                    continue;
                }
                SampleOutcome::Empty => continue,
                SampleOutcome::AlreadyS4 => {
                    samples_already_s4 += 1;
                    stratum_already_s4 += 1;
                    continue;
                }
            };
            sampled_objects += 1;
            sampled_bytes_read += m.bytes_read;
            sampled_full_bytes += m.full_size;
            stratum_read += m.bytes_read;
            let ratio = m.framed_bytes as f64 / m.bytes_read as f64;
            measurements.push((m.full_size, ratio));
            let (measured_kind, _) = measurement_kind(m.runtime_codec);
            let entry = codec_counts
                .entry(m.runtime_codec.as_str().to_owned())
                .or_insert((0, measured_kind.as_str().to_owned()));
            entry.0 += 1;
            if m.gpu_proxy {
                *proxied_codecs
                    .entry(m.runtime_codec.as_str().to_owned())
                    .or_insert(0) += 1;
            }
        }
        let ratio = weighted_mean_ratio(&measurements);
        let projected = (ratio * stratum_bytes as f64).round();
        let projected_bytes = if projected.is_finite() && projected >= 0.0 {
            projected as u64
        } else {
            stratum_bytes
        };
        stratum_reports.push(StratumReport {
            stratum: stratum.clone(),
            objects: indices.len() as u64,
            bytes: stratum_bytes,
            sampled: measurements.len() as u64,
            already_s4: stratum_already_s4,
            sampled_bytes_read: stratum_read,
            codecs: codec_counts
                .into_iter()
                .map(|(codec, (picks, measured_with))| CodecPicks {
                    codec,
                    picks,
                    measured_with,
                })
                .collect(),
            ratio,
            projected_bytes,
        });
    }

    let total_objects = inventory.objects.len() as u64;
    let total_bytes: u64 = inventory.objects.iter().map(|o| o.size).sum();
    let projected_total_bytes: u64 = stratum_reports.iter().map(|s| s.projected_bytes).sum();
    let overall_ratio = if total_bytes > 0 {
        projected_total_bytes as f64 / total_bytes as f64
    } else {
        1.0
    };
    let sampled_fraction = if total_bytes > 0 {
        sampled_full_bytes as f64 / total_bytes as f64
    } else {
        0.0
    };
    let current_cost = total_bytes as f64 / BYTES_PER_GB * params.price_per_gb_month;
    let projected_cost = projected_total_bytes as f64 / BYTES_PER_GB * params.price_per_gb_month;

    let mut notes: Vec<String> = Vec::new();
    if total_objects == 0 {
        notes.push("no objects found".into());
    }
    notes.push(
        "storage-bytes estimate only: request, egress, and (on GPU deployments) compute \
         costs are unchanged by S4"
            .into(),
    );
    notes.push(format!(
        "extrapolated from {} sampled object(s) covering {:.1}% of listed bytes \
         (size-weighted per-extension sampling, seed {})",
        sampled_objects,
        sampled_fraction * 100.0,
        params.seed,
    ));
    notes.push(
        "objects larger than --max-sample-bytes are measured on a Range GET of the first \
         bytes only; the whole-object ratio can differ from the prefix ratio"
            .into(),
    );
    notes.push(
        "samples are compressed as one continuous stream, but the running server resets \
         the zstd stream every 4 MiB chunk — measured ratios are slightly optimistic \
         versus what the gateway will store"
            .into(),
    );
    if samples_missing > 0 {
        notes.push(format!(
            "{samples_missing} sampled object(s) returned 404/NoSuchKey mid-run (deleted \
             between the listing and the sample GET) and were skipped"
        ));
    }
    if samples_already_s4 > 0 {
        notes.push(format!(
            "{samples_already_s4} sampled object(s) are already S4-managed (s4-codec/\
             s4-encrypted metadata or S4F2/S4P1/S4E* magic) and were excluded from the \
             measurement — objects already compressed/encrypted by S4 are not part of \
             this estimate's savings; their listed bytes still count toward the totals \
             and projections (see per-stratum `already_s4`)"
        ));
    }
    if inventory.truncated {
        notes.push(format!(
            "listing truncated at --max-list-keys={}: totals and projections cover only \
             the first {} listed objects, NOT the whole bucket",
            params.max_list_keys, total_objects,
        ));
    }
    for (codec, count) in &proxied_codecs {
        let host = if params.gpu_present {
            "this estimate run stays CPU-only by design"
        } else {
            "this host/build has no usable GPU"
        };
        notes.push(format!(
            "{codec} would be chosen at runtime for {count} sample(s); ratio shown is \
             cpu-zstd (level {}) proxy because {host} (typically conservative for \
             integer columns)",
            params.zstd_level,
        ));
    }

    Ok(EstimateReport {
        bucket: bucket.to_owned(),
        prefix: params.prefix.clone(),
        total_objects,
        total_bytes,
        listing_truncated: inventory.truncated,
        max_list_keys: params.max_list_keys,
        sampled_objects,
        sampled_bytes_read,
        sampled_fraction_of_total_bytes: sampled_fraction,
        strata: stratum_reports,
        overall_ratio,
        projected_total_bytes,
        price_per_gb_month: params.price_per_gb_month,
        current_monthly_cost_usd: current_cost,
        projected_monthly_cost_usd: projected_cost,
        seed: params.seed,
        notes,
    })
}

/// Format a byte count as a short human string (binary units).
fn human_bytes(n: u64) -> String {
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

/// Render the default human-readable table for `--format table`.
pub fn render_human(report: &EstimateReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let target = match &report.prefix {
        Some(p) => format!("{}/{}", report.bucket, p),
        None => report.bucket.clone(),
    };
    let _ = writeln!(out, "S4 storage estimate for {target}");
    let _ = writeln!(
        out,
        "  objects: {}{}   total: {} ({} bytes)",
        report.total_objects,
        if report.listing_truncated {
            format!(" (listing truncated at {})", report.max_list_keys)
        } else {
            String::new()
        },
        human_bytes(report.total_bytes),
        report.total_bytes,
    );
    let _ = writeln!(
        out,
        "  sampled: {} object(s), {} read ({:.1}% of listed bytes)",
        report.sampled_objects,
        human_bytes(report.sampled_bytes_read),
        report.sampled_fraction_of_total_bytes * 100.0,
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {:<12} {:>8} {:>12} {:>8} {:>7} {:>14}  codecs",
        "stratum", "objects", "bytes", "sampled", "ratio", "projected",
    );
    for s in &report.strata {
        let mut codecs = s
            .codecs
            .iter()
            .map(|c| {
                if c.codec == c.measured_with {
                    format!("{}\u{d7}{}", c.codec, c.picks)
                } else {
                    format!("{}\u{d7}{} ({} proxy)", c.codec, c.picks, c.measured_with)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        if s.already_s4 > 0 {
            if !codecs.is_empty() {
                codecs.push_str(", ");
            }
            codecs.push_str(&format!("already-s4\u{d7}{} (excluded)", s.already_s4));
        }
        let _ = writeln!(
            out,
            "  {:<12} {:>8} {:>12} {:>8} {:>7.3} {:>14}  {}",
            s.stratum,
            s.objects,
            human_bytes(s.bytes),
            s.sampled,
            s.ratio,
            human_bytes(s.projected_bytes),
            codecs,
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  projected total: {} ({} bytes, overall ratio {:.3})",
        human_bytes(report.projected_total_bytes),
        report.projected_total_bytes,
        report.overall_ratio,
    );
    let _ = writeln!(
        out,
        "  storage cost: ${:.2}/month now -> ${:.2}/month projected \
         (at ${}/GB-month, storage bytes only)",
        report.current_monthly_cost_usd,
        report.projected_monthly_cost_usd,
        report.price_per_gb_month,
    );
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
    fn stratum_extension_rules() {
        assert_eq!(stratum_for_key("a/b/app.log"), ".log");
        assert_eq!(stratum_for_key("UPPER.JSON"), ".json");
        assert_eq!(stratum_for_key("noext"), "(none)");
        assert_eq!(stratum_for_key("dir.tar/noext"), "(none)");
        assert_eq!(stratum_for_key("a/.gitignore"), "(none)");
        assert_eq!(stratum_for_key("trailing."), "(none)");
        assert_eq!(stratum_for_key("x.tar.gz"), ".gz");
    }

    #[test]
    fn parse_bucket_prefix_shapes() {
        let (b, p) = parse_bucket_prefix("bucket").expect("bare bucket");
        assert_eq!((b.as_str(), p), ("bucket", None));
        let (b, p) = parse_bucket_prefix("bucket/a/b/").expect("bucket+prefix");
        assert_eq!(b, "bucket");
        assert_eq!(p.as_deref(), Some("a/b/"));
        let (b, p) = parse_bucket_prefix("bucket/").expect("trailing slash");
        assert_eq!(b, "bucket");
        assert_eq!(p, None);
        assert!(parse_bucket_prefix("/key").is_err());
        assert!(parse_bucket_prefix("").is_err());
    }

    #[test]
    fn weighted_sampling_is_deterministic_and_skips_empty() {
        let sizes = [0u64, 100, 0, 50, 1_000_000, 3];
        let mut rng_a = StdRng::seed_from_u64(DEFAULT_SEED);
        let mut rng_b = StdRng::seed_from_u64(DEFAULT_SEED);
        let a = weighted_sample_indices(&sizes, 3, &mut rng_a);
        let b = weighted_sample_indices(&sizes, 3, &mut rng_b);
        assert_eq!(a, b, "same seed must sample the same indices");
        assert_eq!(a.len(), 3);
        for &i in &a {
            assert!(sizes[i] > 0, "zero-byte object {i} must never be sampled");
        }
        // Asking for more than available returns every non-empty index.
        let mut rng_c = StdRng::seed_from_u64(7);
        let all = weighted_sample_indices(&sizes, 100, &mut rng_c);
        assert_eq!(all, vec![1, 3, 4, 5]);
    }

    #[test]
    fn weighted_sampling_prefers_large_objects() {
        // One object holds ~99.99% of the bytes; a single draw must pick
        // it for (almost) any seed. Check a handful of seeds rather than
        // asserting on raw RNG internals.
        let sizes = [1u64, 1, 10_000_000, 1];
        for seed in 0..32u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let picked = weighted_sample_indices(&sizes, 1, &mut rng);
            assert_eq!(picked, vec![2], "seed {seed} picked {picked:?}");
        }
    }

    #[test]
    fn weighted_mean_ratio_math() {
        // Empty -> 1.0 (no change).
        assert!((weighted_mean_ratio(&[]) - 1.0).abs() < f64::EPSILON);
        // Single sample -> its own ratio.
        assert!((weighted_mean_ratio(&[(100, 0.25)]) - 0.25).abs() < 1e-12);
        // (90 bytes @ 0.1) + (10 bytes @ 0.9) -> 0.18.
        let r = weighted_mean_ratio(&[(90, 0.1), (10, 0.9)]);
        assert!((r - 0.18).abs() < 1e-12, "got {r}");
    }

    #[test]
    fn missing_object_error_classification() {
        use aws_sdk_s3::operation::get_object::GetObjectError;
        use aws_sdk_s3::types::error::NoSuchKey;

        // Modeled NoSuchKey → missing, regardless of the status hint.
        let no_such_key = GetObjectError::NoSuchKey(NoSuchKey::builder().build());
        assert!(is_missing_object_error(Some(&no_such_key), None));
        assert!(is_missing_object_error(Some(&no_such_key), Some(404)));
        // Raw 404 without a modeled code (e.g. some backends on ranged
        // GETs) → missing.
        assert!(is_missing_object_error(None, Some(404)));
        // Anything else aborts the run as a real backend error.
        let invalid_state = GetObjectError::InvalidObjectState(
            aws_sdk_s3::types::error::InvalidObjectState::builder().build(),
        );
        assert!(!is_missing_object_error(Some(&invalid_state), Some(403)));
        assert!(!is_missing_object_error(None, Some(500)));
        assert!(!is_missing_object_error(None, None));
    }

    /// Already-S4 sample classification — the core of the "gateway-run
    /// bucket produces garbage ratios" fix: framed / encrypted gateway
    /// output must be recognized via metadata OR body magic and
    /// excluded from the measurement.
    #[test]
    fn already_s4_sample_classification() {
        // Metadata stamps (any value counts; the key is the signal).
        let mut meta = HashMap::new();
        meta.insert("s4-codec".to_owned(), "cpu-zstd".to_owned());
        assert!(is_already_s4_metadata(Some(&meta)));
        let mut meta = HashMap::new();
        meta.insert("s4-encrypted".to_owned(), "aes-256-gcm".to_owned());
        assert!(is_already_s4_metadata(Some(&meta)));
        // User metadata without the stamps does not trigger.
        let mut meta = HashMap::new();
        meta.insert("owner".to_owned(), "alice".to_owned());
        assert!(!is_already_s4_metadata(Some(&meta)));
        assert!(!is_already_s4_metadata(None));

        // Frame magic — structurally validated (audit R3 P3): a real
        // frame written by the production writer classifies...
        let mut framed = bytes::BytesMut::new();
        s4_codec::multipart::write_frame(
            &mut framed,
            s4_codec::multipart::FrameHeader {
                codec: s4_codec::CodecKind::CpuZstd,
                original_size: 11,
                compressed_size: 7,
                crc32c: 0,
            },
            b"payload",
        );
        let framed = framed.freeze();
        assert!(is_already_s4_body(&framed, framed.len() as u64));
        // ...but customer bytes that merely start with the 4-byte magic
        // do not (unknown codec id / payload larger than the object).
        assert!(!is_already_s4_body(
            b"S4F2rest-of-frame-but-not-a-frame-header....",
            44
        ));
        // S4P1 padding: a plausible padding header classifies, a bogus
        // one (claimed pad larger than the object) does not.
        let mut pad = b"S4P1".to_vec();
        pad.extend_from_slice(&4u64.to_le_bytes());
        pad.extend_from_slice(&[0u8; 4]);
        assert!(is_already_s4_body(&pad, pad.len() as u64));
        let mut bogus_pad = b"S4P1".to_vec();
        bogus_pad.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(!is_already_s4_body(&bogus_pad, 12));
        // SSE magic — looks_encrypted is length-gated at the 36-byte
        // minimum header, so pad past it.
        for magic in [b"S4E1", b"S4E2", b"S4E3", b"S4E4", b"S4E5", b"S4E6"] {
            let mut body = magic.to_vec();
            body.extend_from_slice(&[0u8; 40]);
            assert!(
                is_already_s4_body(&body, body.len() as u64),
                "{} body must classify as already-s4",
                String::from_utf8_lossy(magic)
            );
        }
        // A too-short S4E* body is NOT caught by the magic (metadata
        // stamp covers gateway-written ones) — documented gate.
        assert!(!is_already_s4_body(b"S4E2 too short", 14));
        // Plain customer bytes pass through to measurement.
        assert!(!is_already_s4_body(b"plain text log line", 19));
        assert!(!is_already_s4_body(b"", 0));
        // zstd magic alone is not S4 management.
        assert!(!is_already_s4_body(&[0x28, 0xb5, 0x2f, 0xfd], 4));
    }

    #[test]
    fn measurement_kind_proxies_gpu_only() {
        assert_eq!(
            measurement_kind(CodecKind::CpuZstd),
            (CodecKind::CpuZstd, false)
        );
        assert_eq!(
            measurement_kind(CodecKind::Passthrough),
            (CodecKind::Passthrough, false)
        );
        assert_eq!(
            measurement_kind(CodecKind::CpuGzip),
            (CodecKind::CpuGzip, false)
        );
        assert_eq!(
            measurement_kind(CodecKind::NvcompBitcomp),
            (CodecKind::CpuZstd, true)
        );
        assert_eq!(
            measurement_kind(CodecKind::NvcompZstd),
            (CodecKind::CpuZstd, true)
        );
    }

    fn dummy_report() -> EstimateReport {
        EstimateReport {
            bucket: "b".into(),
            prefix: Some("logs/".into()),
            total_objects: 2,
            total_bytes: 1000,
            listing_truncated: false,
            max_list_keys: DEFAULT_MAX_LIST_KEYS,
            sampled_objects: 1,
            sampled_bytes_read: 600,
            sampled_fraction_of_total_bytes: 0.6,
            strata: vec![StratumReport {
                stratum: ".log".into(),
                objects: 2,
                bytes: 1000,
                sampled: 1,
                already_s4: 0,
                sampled_bytes_read: 600,
                codecs: vec![CodecPicks {
                    codec: "cpu-zstd".into(),
                    picks: 1,
                    measured_with: "cpu-zstd".into(),
                }],
                ratio: 0.5,
                projected_bytes: 500,
            }],
            overall_ratio: 0.5,
            projected_total_bytes: 500,
            price_per_gb_month: DEFAULT_PRICE_PER_GB_MONTH,
            current_monthly_cost_usd: 0.0,
            projected_monthly_cost_usd: 0.0,
            seed: DEFAULT_SEED,
            notes: vec!["storage-bytes estimate only".into()],
        }
    }

    #[test]
    fn report_json_shape() {
        let v = serde_json::to_value(dummy_report()).expect("serialize");
        assert_eq!(v["bucket"], "b");
        assert_eq!(v["prefix"], "logs/");
        assert_eq!(v["total_objects"], 2);
        assert_eq!(v["total_bytes"], 1000);
        assert_eq!(v["listing_truncated"], false);
        assert_eq!(v["sampled_objects"], 1);
        assert_eq!(v["sampled_fraction_of_total_bytes"], 0.6);
        assert_eq!(v["overall_ratio"], 0.5);
        assert_eq!(v["projected_total_bytes"], 500);
        assert_eq!(v["price_per_gb_month"], DEFAULT_PRICE_PER_GB_MONTH);
        assert_eq!(v["seed"], 42);
        let s = &v["strata"][0];
        assert_eq!(s["stratum"], ".log");
        assert_eq!(s["objects"], 2);
        assert_eq!(s["bytes"], 1000);
        assert_eq!(s["sampled"], 1);
        assert_eq!(s["already_s4"], 0);
        assert_eq!(s["ratio"], 0.5);
        assert_eq!(s["projected_bytes"], 500);
        assert_eq!(s["codecs"][0]["codec"], "cpu-zstd");
        assert_eq!(s["codecs"][0]["picks"], 1);
        assert_eq!(s["codecs"][0]["measured_with"], "cpu-zstd");
        assert!(v["notes"].as_array().is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn render_human_mentions_key_figures() {
        let txt = render_human(&dummy_report());
        assert!(txt.contains("S4 storage estimate for b/logs/"));
        assert!(txt.contains(".log"));
        assert!(txt.contains("Notes:"));
        assert!(txt.contains("storage-bytes estimate only"));
    }

    /// End-to-end of the *math* (no network): compress real bytes with
    /// the measurement registry and check the framed ratio matches what
    /// `run_estimate` would record for the same body.
    #[tokio::test]
    async fn framed_ratio_includes_frame_header() {
        let params = EstimateParams {
            prefix: None,
            max_list_keys: DEFAULT_MAX_LIST_KEYS,
            samples_per_stratum: DEFAULT_SAMPLES_PER_STRATUM,
            max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
            seed: DEFAULT_SEED,
            price_per_gb_month: DEFAULT_PRICE_PER_GB_MONTH,
            default_codec: CodecKind::CpuZstd,
            zstd_level: CpuZstd::DEFAULT_LEVEL,
            use_sampling_dispatcher: true,
            gpu_min_bytes: SamplingDispatcher::DEFAULT_GPU_MIN_BYTES,
            prefer_columnar_gpu: false,
            simulate_gpu: false,
            gpu_present: false,
        };
        let registry = build_measurement_registry(&params);
        let body = Bytes::from(vec![b'a'; 64 * 1024]);
        let (compressed, _) = registry
            .compress(body.clone(), CodecKind::CpuZstd)
            .await
            .expect("compress");
        let framed = compressed.len() as u64 + FRAME_HEADER_BYTES as u64;
        assert!(framed > compressed.len() as u64);
        let ratio = framed as f64 / body.len() as f64;
        assert!(
            ratio < 0.05,
            "64 KiB of 'a' must compress hard, got {ratio}"
        );
    }

    #[test]
    fn sim_dispatcher_predicts_gpu_pick_without_gpu() {
        // The honesty core: with simulate_gpu + columnar flags, the
        // dispatcher must predict nvcomp-bitcomp for a u32 LE integer
        // column even though this test host has no GPU — and
        // measurement_kind must proxy it to cpu-zstd.
        let params = EstimateParams {
            prefix: None,
            max_list_keys: DEFAULT_MAX_LIST_KEYS,
            samples_per_stratum: DEFAULT_SAMPLES_PER_STRATUM,
            max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
            seed: DEFAULT_SEED,
            price_per_gb_month: DEFAULT_PRICE_PER_GB_MONTH,
            default_codec: CodecKind::CpuZstd,
            zstd_level: CpuZstd::DEFAULT_LEVEL,
            use_sampling_dispatcher: true,
            gpu_min_bytes: SamplingDispatcher::DEFAULT_GPU_MIN_BYTES,
            prefer_columnar_gpu: true,
            simulate_gpu: true,
            gpu_present: false,
        };
        let dispatcher = build_sim_dispatcher(&params);
        // 4 KiB of u32 LE values < 2^16: low bytes vary, high bytes are
        // zero — the columnar-integer signature.
        let mut sample = Vec::with_capacity(4096);
        for i in 0u32..1024 {
            sample.extend_from_slice(&(i * 37 % 65_536).to_le_bytes());
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let pick = rt.block_on(dispatcher.pick_with_size_hint(&sample, Some(8 * 1024 * 1024)));
        assert_eq!(pick, CodecKind::NvcompBitcomp);
        assert_eq!(measurement_kind(pick), (CodecKind::CpuZstd, true));
    }
}
