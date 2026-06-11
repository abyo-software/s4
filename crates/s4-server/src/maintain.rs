//! v1.2: `s4 maintain` — policy-driven bucket maintenance.
//!
//! `s4 migrate` and `s4 recompact` are one-bucket, one-action CLI
//! invocations; in practice operators chain several of them in cron
//! ("frame the new uploads, bake last month's logs at 19, push cold
//! prefixes to GLACIER_IR"). `s4 maintain` lifts that into a single
//! declarative TOML policy file:
//!
//! ```toml
//! [[rule]]
//! name = "compress-new-logs"       # required, unique
//! bucket = "prod-logs"             # required
//! prefix = "app/"                  # optional
//! action = "migrate"               # migrate | recompact | transition
//! older-than = "7d"                # optional, all actions
//!
//! [[rule]]
//! name = "bake-cold-logs"
//! bucket = "prod-logs"
//! action = "recompact"
//! older-than = "30d"
//! target-zstd-level = 19           # action params mirror the CLI flags
//!
//! [[rule]]
//! name = "archive-old-logs"
//! bucket = "prod-logs"
//! action = "transition"
//! older-than = "90d"
//! storage-class = "GLACIER_IR"     # required for transition
//! ```
//!
//! Rules run **sequentially, top to bottom**, against one backend
//! endpoint. Like every offline tool in this crate the run is
//! **dry-run by default** (`--execute` writes), and `--interval 24h`
//! turns the one-shot run into a resident loop (run → sleep → re-run)
//! that exits gracefully on SIGTERM / SIGINT, finishing the rule in
//! flight first.
//!
//! ## Actions
//!
//! - `migrate` / `recompact` call the **same library entry points** the
//!   stand-alone subcommands use ([`crate::migrate::run_migrate`] /
//!   [`crate::recompact::run_recompact`]) — identical selection,
//!   verification, sidecar and skip-taxonomy behaviour. `older-than`
//!   on a migrate rule applies the same conservative `LastModified`
//!   gate `recompact --older-than` uses (unknown age = too recent).
//! - `transition` is new: it changes the **storage class** of cold
//!   objects via a same-bucket same-key server-side `CopyObject`
//!   (`x-amz-storage-class: <target>`; see [`copy_to_class`] for the
//!   metadata-directive details), the
//!   programmatic twin of the lifecycle configuration documented in
//!   `docs/storage-class-transitions.md`. The S4-specific value over a
//!   plain lifecycle rule: the `<key>.s4index` **sidecar always
//!   accompanies its main object** into the same class (and is
//!   realigned when a previous run left it behind), so the pair never
//!   splits the way a size- or suffix-filtered lifecycle rule can.
//!   Sidecars are never transitioned on their own — an orphan sidecar
//!   is `s4 sweep-orphan-sidecars` territory, not ours.
//!
//! ## Honesty constraints
//!
//! - Dry-run evaluates each rule against the bucket's **current**
//!   state. Effects of earlier rules in the same policy are not
//!   simulated — e.g. a transition rule's dry-run cannot count the
//!   sidecars a preceding migrate rule *would* create.
//! - `transition` cannot move objects **out of** `GLACIER` /
//!   `DEEP_ARCHIVE` without a prior restore (the server-side copy
//!   fails with `InvalidObjectState`; the object is left as-is and
//!   counted as failed).
//! - The copy preserves user metadata, Content-* attributes, Expires,
//!   WebsiteRedirectLocation and tags, but **not backend-managed SSE**
//!   (the destination is re-encrypted under the bucket's default
//!   encryption — a per-object SSE-KMS key choice is lost, SSE-C
//!   sources fail the copy), and a **multipart original's composite
//!   full-object checksum and ETag change** (the single-op copy is
//!   single-part; the backend recomputes); the ETag change invalidates
//!   the sidecar's ETag binding until the next gateway write, so Range
//!   GETs fall back to full reads (perf-only, never a correctness
//!   loss).
//! - Every copy is pinned with `x-amz-copy-source-if-match` to the
//!   generation a fresh `HeadObject` described, so a concurrent
//!   overwrite can never get stale metadata stamped onto its new bytes
//!   — the backend answers 412 and the object is counted as
//!   `skipped_etag_raced`.
//! - Versioned buckets double-bill on every rewrite/copy, exactly like
//!   `migrate` / `recompact` — the per-rule reports warn.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aws_sdk_s3::Client;
use s4_codec::CodecKind;
use s4_codec::index::{SIDECAR_SUFFIX, sidecar_key};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::migrate::{MigrateParams, MigrateReport};
use crate::recompact::{RecompactParams, RecompactReport, parse_duration_suffix};

/// Single-operation `CopyObject` size ceiling (5 GiB). Larger objects
/// would need a multipart copy, which the transition action does not
/// implement — they are skipped (`too-large`) and noted.
pub const MAX_COPY_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// `now - older_than` as epoch seconds, the cutoff
/// [`crate::recompact::is_too_recent`] compares `LastModified` against.
/// `None` cutoff = no age filter. Saturating on the (absurd) edges so a
/// clock before the epoch or a u64::MAX duration cannot panic.
/// Shared by the maintain rules and `migrate`'s `older-than` gate
/// ([`crate::migrate::run_migrate_with_cutoff`]); same arithmetic as
/// `run_recompact`'s inline computation.
pub(crate) fn cutoff_epoch_secs(older_than: Option<Duration>) -> Option<i64> {
    older_than.map(|older| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let now_secs = i64::try_from(now_secs).unwrap_or(i64::MAX);
        let older_secs = i64::try_from(older.as_secs()).unwrap_or(i64::MAX);
        now_secs.saturating_sub(older_secs)
    })
}

/// v1.2 stability: `#[non_exhaustive]` — new maintain-time failure
/// modes may be added in minor releases. Downstream callers must
/// include a `_ =>` arm when matching on this enum.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MaintainError {
    #[error("cannot read policy file {path}: {cause}")]
    PolicyRead { path: String, cause: String },
    #[error("policy file is not valid TOML: {cause}")]
    PolicyParse { cause: String },
    /// Every rule-validation problem found in one pass — the operator
    /// fixes the whole file once instead of replaying error-by-error.
    #[error("invalid maintain policy ({} error(s)):\n  - {}", errors.len(), errors.join("\n  - "))]
    PolicyInvalid { errors: Vec<String> },
    #[error("S3 backend error on {op} {bucket}/{key}: {cause}")]
    Backend {
        op: &'static str,
        bucket: String,
        // Empty for bucket-level ops (ListObjectsV2 / GetBucketVersioning).
        key: String,
        // Named `cause` (not `source`) — same convention as
        // `migrate::MigrateError` / `recompact::RecompactError`.
        cause: String,
    },
}

// ---------------------------------------------------------------------------
// Policy file
// ---------------------------------------------------------------------------

/// Raw deserialization target for the policy TOML. Every field is
/// optional so one `toml::from_str` pass succeeds and the validator can
/// report **all** problems (missing names, unknown actions, duplicate
/// rules, …) in a single [`MaintainError::PolicyInvalid`]. Unknown keys
/// are still rejected at parse time (`deny_unknown_fields`), where the
/// TOML error span points at the offending line.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RawRule {
    name: Option<String>,
    bucket: Option<String>,
    prefix: Option<String>,
    action: Option<String>,
    older_than: Option<String>,
    // migrate + recompact params (CLI-flag names and defaults).
    no_tags: Option<bool>,
    concurrency: Option<usize>,
    max_objects: Option<usize>,
    max_body_bytes: Option<u64>,
    // recompact-only params.
    target_zstd_level: Option<i32>,
    min_gain_percent: Option<f64>,
    assume_unstamped_framed: Option<bool>,
    // transition-only param.
    storage_class: Option<String>,
}

/// One validated maintenance rule. Produced by [`parse_policy`] /
/// [`load_policy`] only — `#[non_exhaustive]` keeps the door open for
/// new optional keys in minor releases without breaking downstream
/// struct literals (the lesson `MigrateParams` taught us).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MaintainRule {
    /// Unique rule name (the report and the resident-mode logs key on it).
    pub name: String,
    pub bucket: String,
    pub prefix: Option<String>,
    /// Common age gate: only objects whose backend `LastModified` is at
    /// least this old are acted on (conservative: unknown age = too
    /// recent, same as `recompact --older-than`).
    pub older_than: Option<Duration>,
    pub action: RuleAction,
}

/// The action a rule performs. `#[non_exhaustive]`: new actions may be
/// added in minor releases.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RuleAction {
    Migrate(MigrateRule),
    Recompact(RecompactRule),
    Transition(TransitionRule),
}

impl RuleAction {
    /// The `action = "…"` string this variant was parsed from.
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleAction::Migrate(_) => "migrate",
            RuleAction::Recompact(_) => "recompact",
            RuleAction::Transition(_) => "transition",
        }
    }
}

/// `action = "migrate"` parameters — same names and defaults as the
/// `s4 migrate` CLI flags.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MigrateRule {
    pub no_tags: bool,
    pub concurrency: usize,
    pub max_objects: Option<usize>,
    pub max_body_bytes: u64,
}

/// `action = "recompact"` parameters — same names and defaults as the
/// `s4 recompact` CLI flags.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RecompactRule {
    pub target_zstd_level: i32,
    pub min_gain_percent: f64,
    pub assume_unstamped_framed: bool,
    pub no_tags: bool,
    pub concurrency: usize,
    pub max_objects: Option<usize>,
    pub max_body_bytes: u64,
}

/// `action = "transition"` parameters.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TransitionRule {
    /// Target storage class, canonical S3 form (`"GLACIER_IR"`,
    /// `"STANDARD_IA"`, …) — validated against the SDK's known set at
    /// policy-load time.
    pub storage_class: String,
}

/// A validated maintenance policy: the `[[rule]]` array in file order
/// (rules execute top to bottom).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MaintainPolicy {
    pub rules: Vec<MaintainRule>,
}

/// Read + parse + validate a policy file. All rule-validation problems
/// are collected into one [`MaintainError::PolicyInvalid`].
pub fn load_policy(path: &std::path::Path) -> Result<MaintainPolicy, MaintainError> {
    let text = std::fs::read_to_string(path).map_err(|e| MaintainError::PolicyRead {
        path: path.display().to_string(),
        cause: e.to_string(),
    })?;
    parse_policy(&text)
}

/// Parse + validate a policy from TOML text. Split from [`load_policy`]
/// so the validation matrix is unit-testable without a filesystem.
pub fn parse_policy(text: &str) -> Result<MaintainPolicy, MaintainError> {
    let raw: RawPolicy = toml::from_str(text).map_err(|e| MaintainError::PolicyParse {
        cause: e.to_string(),
    })?;

    let mut errors: Vec<String> = Vec::new();
    if raw.rule.is_empty() {
        errors.push("policy contains no [[rule]] entries".to_owned());
    }

    let mut seen_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut rules: Vec<MaintainRule> = Vec::new();

    for (idx, r) in raw.rule.iter().enumerate() {
        // Human-readable rule label for the error messages: the name
        // when present, the 1-based [[rule]] position otherwise.
        let label = match r.name.as_deref() {
            Some(n) if !n.is_empty() => format!("rule {n:?}"),
            _ => format!("rule #{}", idx + 1),
        };

        let name = match r.name.as_deref() {
            Some(n) if !n.is_empty() => {
                if !seen_names.insert(n.to_owned()) {
                    errors.push(format!("{label}: duplicate rule name {n:?}"));
                }
                n.to_owned()
            }
            Some(_) => {
                errors.push(format!("{label}: `name` must not be empty"));
                String::new()
            }
            None => {
                errors.push(format!("{label}: missing required key `name`"));
                String::new()
            }
        };

        let bucket = match r.bucket.as_deref() {
            Some(b) if !b.is_empty() => b.to_owned(),
            Some(_) => {
                errors.push(format!("{label}: `bucket` must not be empty"));
                String::new()
            }
            None => {
                errors.push(format!("{label}: missing required key `bucket`"));
                String::new()
            }
        };

        let older_than = match r.older_than.as_deref() {
            None => None,
            Some(s) => match parse_duration_suffix(s) {
                Ok(d) => Some(d),
                Err(e) => {
                    errors.push(format!("{label}: `older-than`: {e}"));
                    None
                }
            },
        };

        // Which optional keys are set but make no sense for the chosen
        // action? Reported instead of silently ignored — a recompact
        // knob on a migrate rule is almost certainly a copy-paste slip.
        let set_keys: Vec<&'static str> = [
            ("no-tags", r.no_tags.is_some()),
            ("concurrency", r.concurrency.is_some()),
            ("max-objects", r.max_objects.is_some()),
            ("max-body-bytes", r.max_body_bytes.is_some()),
            ("target-zstd-level", r.target_zstd_level.is_some()),
            ("min-gain-percent", r.min_gain_percent.is_some()),
            (
                "assume-unstamped-framed",
                r.assume_unstamped_framed.is_some(),
            ),
            ("storage-class", r.storage_class.is_some()),
        ]
        .into_iter()
        .filter_map(|(k, set)| set.then_some(k))
        .collect();
        let mut reject_inapplicable = |action: &str, allowed: &[&str]| {
            for k in &set_keys {
                if !allowed.contains(k) {
                    errors.push(format!(
                        "{label}: key `{k}` does not apply to action = \"{action}\""
                    ));
                }
            }
        };

        let action = match r.action.as_deref() {
            Some("migrate") => {
                reject_inapplicable(
                    "migrate",
                    &["no-tags", "concurrency", "max-objects", "max-body-bytes"],
                );
                Some(RuleAction::Migrate(MigrateRule {
                    no_tags: r.no_tags.unwrap_or(false),
                    concurrency: r
                        .concurrency
                        .unwrap_or(crate::migrate::DEFAULT_MIGRATE_CONCURRENCY),
                    max_objects: r.max_objects,
                    max_body_bytes: r
                        .max_body_bytes
                        .unwrap_or(crate::repair::DEFAULT_REPAIR_BODY_BYTES_CAP),
                }))
            }
            Some("recompact") => {
                reject_inapplicable(
                    "recompact",
                    &[
                        "no-tags",
                        "concurrency",
                        "max-objects",
                        "max-body-bytes",
                        "target-zstd-level",
                        "min-gain-percent",
                        "assume-unstamped-framed",
                    ],
                );
                let target_zstd_level = r
                    .target_zstd_level
                    .unwrap_or(crate::recompact::DEFAULT_TARGET_ZSTD_LEVEL);
                if !(1..=22).contains(&target_zstd_level) {
                    errors.push(format!(
                        "{label}: `target-zstd-level` must be in 1..=22, got {target_zstd_level}"
                    ));
                }
                Some(RuleAction::Recompact(RecompactRule {
                    target_zstd_level,
                    min_gain_percent: r
                        .min_gain_percent
                        .unwrap_or(crate::recompact::DEFAULT_MIN_GAIN_PERCENT),
                    assume_unstamped_framed: r.assume_unstamped_framed.unwrap_or(false),
                    no_tags: r.no_tags.unwrap_or(false),
                    concurrency: r
                        .concurrency
                        .unwrap_or(crate::recompact::DEFAULT_RECOMPACT_CONCURRENCY),
                    max_objects: r.max_objects,
                    max_body_bytes: r
                        .max_body_bytes
                        .unwrap_or(crate::repair::DEFAULT_REPAIR_BODY_BYTES_CAP),
                }))
            }
            Some("transition") => {
                reject_inapplicable("transition", &["storage-class"]);
                let valid = aws_sdk_s3::types::StorageClass::values();
                match r.storage_class.as_deref() {
                    Some(sc) if valid.contains(&sc) => {
                        Some(RuleAction::Transition(TransitionRule {
                            storage_class: sc.to_owned(),
                        }))
                    }
                    Some(sc) => {
                        errors.push(format!(
                            "{label}: unknown storage-class {sc:?} (valid: {})",
                            valid.join(", ")
                        ));
                        None
                    }
                    None => {
                        errors.push(format!(
                            "{label}: action = \"transition\" requires `storage-class`"
                        ));
                        None
                    }
                }
            }
            Some(other) => {
                errors.push(format!(
                    "{label}: unknown action {other:?} (valid: migrate, recompact, transition)"
                ));
                None
            }
            None => {
                errors.push(format!("{label}: missing required key `action`"));
                None
            }
        };

        if let Some(action) = action
            && !name.is_empty()
            && !bucket.is_empty()
        {
            rules.push(MaintainRule {
                name,
                bucket,
                prefix: r.prefix.clone(),
                older_than,
                action,
            });
        }
    }

    if errors.is_empty() {
        Ok(MaintainPolicy { rules })
    } else {
        Err(MaintainError::PolicyInvalid { errors })
    }
}

// ---------------------------------------------------------------------------
// transition action
// ---------------------------------------------------------------------------

/// One per-object hard failure during a transition rule.
#[derive(Debug, Clone, Serialize)]
pub struct TransitionFailure {
    pub key: String,
    pub op: String,
    pub cause: String,
}

/// Full result of one transition rule. Serializes into the maintain
/// `--format json` output verbatim.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct TransitionReport {
    pub bucket: String,
    pub prefix: Option<String>,
    /// `true` = nothing was copied (default mode).
    pub dry_run: bool,
    /// Target storage class, canonical S3 form.
    pub storage_class: String,
    /// `older-than` cutoff in seconds (`null` = no age filter).
    pub older_than_secs: Option<u64>,
    /// Main objects listed (`.s4index` sidecars are companions, not
    /// work items; `.s4dict/` and `.__s4ver__/` keys are excluded).
    pub total_objects: u64,
    pub total_bytes: u64,
    /// Main objects whose storage class changed (dry-run: would change).
    pub transitioned: u64,
    /// `<key>.s4index` sidecars moved alongside their main object —
    /// includes realignments of sidecars a previous partial run left
    /// behind. Never moved on their own.
    pub transitioned_sidecars: u64,
    pub skipped_already_target_class: u64,
    pub skipped_too_recent: u64,
    /// Overwritten after the listing snapshot (pre-copy guard HEAD
    /// mismatch) **or** between the guard HEAD and the copy itself
    /// (`x-amz-copy-source-if-match` answered 412) — either way the
    /// object became hot again and is left alone; the next run sees it
    /// fresh.
    pub skipped_etag_raced: u64,
    /// Over [`MAX_COPY_OBJECT_BYTES`] — a single server-side CopyObject
    /// cannot move them.
    pub skipped_too_large: u64,
    pub failed: u64,
    /// Per-key failure details, sorted by key.
    pub failures: Vec<TransitionFailure>,
    /// `GetBucketVersioning` said `Enabled` (double-billing warning in
    /// `notes`). `false` covers Suspended / never-enabled / unknown.
    pub versioning_enabled: bool,
    /// Fixed honesty notes + run-specific caveats.
    pub notes: Vec<String>,
}

/// Why one main object was left untouched by a transition rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransitionSkip {
    TooRecent,
    AlreadyTargetClass,
    TooLarge,
}

/// `ListObjectsV2` / `HeadObject` omit the storage class for STANDARD
/// objects on some backends — normalize the absent form.
fn effective_class(class: Option<&str>) -> &str {
    match class {
        None | Some("") => "STANDARD",
        Some(c) => c,
    }
}

/// Pure selection gate, run per main object before any network call.
/// Order mirrors `recompact_one`: the age gate first (no work spent on
/// hot objects), then the idempotency class check, then the copy-size
/// ceiling.
pub(crate) fn classify_transition(
    current_class: Option<&str>,
    target_class: &str,
    last_modified_epoch_secs: Option<i64>,
    cutoff_epoch_secs: Option<i64>,
    size: u64,
) -> Option<TransitionSkip> {
    if crate::recompact::is_too_recent(last_modified_epoch_secs, cutoff_epoch_secs) {
        return Some(TransitionSkip::TooRecent);
    }
    if effective_class(current_class) == target_class {
        return Some(TransitionSkip::AlreadyTargetClass);
    }
    if size > MAX_COPY_OBJECT_BYTES {
        return Some(TransitionSkip::TooLarge);
    }
    None
}

/// `x-amz-copy-source` value for a same-bucket same-key copy. The key
/// half is percent-encoded with the same conservative set
/// `migrate::encode_tagging` uses, minus `/` (path separators must
/// survive verbatim).
fn copy_source(bucket: &str, key: &str) -> String {
    const ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~')
        .remove(b'/');
    format!(
        "{bucket}/{}",
        percent_encoding::utf8_percent_encode(key, ENCODE_SET)
    )
}

/// One listed object (main or sidecar) with the fields the transition
/// selection needs. `ListObjectsV2` carries all of them, so selection
/// costs no per-object request.
#[derive(Debug, Clone)]
struct TransitionObject {
    key: String,
    size: u64,
    last_modified_epoch_secs: Option<i64>,
    storage_class: Option<String>,
    etag: Option<String>,
}

struct TransitionInventory {
    mains: Vec<TransitionObject>,
    /// `<key>.s4index` → its listing entry, for the companion lookup.
    sidecars: BTreeMap<String, TransitionObject>,
}

/// Paginate `ListObjectsV2`. Unlike `migrate::list_inventory` the
/// sidecars are **kept** (in their own map — they transition with
/// their main object); `.s4dict/` dictionaries and `.__s4ver__/`
/// version shadows stay excluded entirely (changing a dictionary's or
/// shadow's class behind the gateway's back has no upside and Glacier
/// would break dictionary GETs outright).
async fn list_transition_inventory(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
) -> Result<TransitionInventory, MaintainError> {
    let mut mains: Vec<TransitionObject> = Vec::new();
    let mut sidecars: BTreeMap<String, TransitionObject> = BTreeMap::new();
    let mut continuation: Option<String> = None;
    loop {
        let mut req = client.list_objects_v2().bucket(bucket);
        if let Some(p) = prefix {
            req = req.prefix(p);
        }
        if let Some(c) = continuation.as_ref() {
            req = req.continuation_token(c);
        }
        let resp = req.send().await.map_err(|e| MaintainError::Backend {
            op: "ListObjectsV2",
            bucket: bucket.into(),
            key: String::new(),
            cause: format!("{e}"),
        })?;
        for obj in resp.contents() {
            let Some(k) = obj.key() else { continue };
            if crate::dict::is_dict_key(k) || crate::migrate::is_versioning_shadow_key(k) {
                continue;
            }
            let item = TransitionObject {
                key: k.to_owned(),
                size: obj.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0),
                last_modified_epoch_secs: obj.last_modified().map(|t| t.secs()),
                storage_class: obj.storage_class().map(|c| c.as_str().to_owned()),
                etag: obj.e_tag().map(crate::migrate::normalize_etag),
            };
            if k.ends_with(SIDECAR_SUFFIX) {
                sidecars.insert(k.to_owned(), item);
            } else {
                mains.push(item);
            }
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
    Ok(TransitionInventory { mains, sidecars })
}

/// How one [`copy_to_class`] attempt failed.
#[derive(Debug)]
enum CopyClassError {
    /// The backend rejected the copy with `412 PreconditionFailed`: the
    /// object was overwritten between our attribute `HeadObject` and the
    /// `CopyObject` (v1.2 audit R1 P2). The new bytes are hot again and
    /// carry their own correct metadata — leaving them alone is the
    /// right outcome, so callers count this as `skipped_etag_raced`,
    /// not as a failure.
    Raced,
    /// Anything else — surfaced in the report's `failures`.
    Other(String),
}

/// `true` when a `CopyObject` error is the backend telling us the
/// `x-amz-copy-source-if-match` precondition failed (HTTP 412 /
/// error code `PreconditionFailed`). Same belt-and-braces shape as
/// `repair.rs`'s if-match handling: the modeled error code when the
/// backend sends one, the raw HTTP status otherwise, and a message
/// substring as the last resort for proxies that mangle both.
fn copy_error_is_precondition(code: Option<&str>, status: Option<u16>, msg: &str) -> bool {
    code == Some("PreconditionFailed") || status == Some(412) || msg.contains("PreconditionFailed")
}

/// Build the storage-class `CopyObject` request from a fresh
/// `HeadObject` snapshot. Split from [`copy_to_class`] so a unit test
/// can assert the request shape (notably that
/// `x-amz-copy-source-if-match` is pinned to the HEAD's ETag) without a
/// backend.
fn build_class_copy(
    client: &Client,
    bucket: &str,
    key: &str,
    target_class: &str,
    head: &aws_sdk_s3::operation::head_object::HeadObjectOutput,
) -> aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder {
    let mut req = client
        .copy_object()
        .bucket(bucket)
        .key(key)
        .copy_source(copy_source(bucket, key))
        .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace)
        .set_metadata(head.metadata().cloned())
        .set_content_type(head.content_type().map(str::to_owned))
        .set_cache_control(head.cache_control().map(str::to_owned))
        .set_content_disposition(head.content_disposition().map(str::to_owned))
        .set_content_encoding(head.content_encoding().map(str::to_owned))
        .set_content_language(head.content_language().map(str::to_owned))
        .set_website_redirect_location(head.website_redirect_location().map(str::to_owned))
        .storage_class(aws_sdk_s3::types::StorageClass::from(target_class));
    // `HeadObjectOutput::expires` is deprecated in favour of the raw
    // `expires_string`, but `CopyObjectInput` only accepts the parsed
    // `DateTime` — the parsed accessor is the only loss-free way to
    // round-trip the attribute.
    #[allow(deprecated)]
    {
        req = req.set_expires(head.expires().copied());
    }
    // v1.2 audit R1 P2: pin the copy to the exact bytes the HEAD above
    // described. Without it, a PUT racing between the HEAD and the
    // REPLACE-directive copy would get the *old* metadata stamped onto
    // the *new* bytes (e.g. an `s4-codec` stamp on a plaintext body —
    // unreadable through the gateway). A HEAD without an ETag (no known
    // backend does this) degrades to the unguarded pre-v1.2 behaviour.
    if let Some(etag) = head.e_tag() {
        req = req.copy_source_if_match(etag);
    }
    req
}

/// Same-bucket same-key server-side copy that only changes the storage
/// class. The body never travels through this process.
///
/// Directive choice: `MetadataDirective: REPLACE`, re-sending the user
/// metadata (including the `s4-*` manifest stamps) and the
/// Content-Type / Cache-Control / Content-* / Expires /
/// WebsiteRedirectLocation attributes captured from a fresh
/// `HeadObject` — byte-identical to what `COPY` would keep. `COPY`
/// itself is not usable here: AWS accepts a same-key copy when the
/// storage class changes, but MinIO (and kin) reject any same-key copy
/// whose metadata directive is not `REPLACE`. The tagging directive is
/// left at its default (`COPY`), so object tags ride along server-side
/// either way.
///
/// What the REPLACE copy does **not** preserve:
///
/// - **Backend-managed SSE**: no encryption headers are re-sent, so the
///   destination is written under the **bucket's default** encryption
///   config. A source encrypted with a per-object SSE-KMS key loses
///   that key choice (re-encrypted with the bucket default); an SSE-C
///   source fails the copy outright (we do not hold the key).
/// - **Full-object checksums of multipart originals**: the single-op
///   copy produces a single-part destination, so a multipart original's
///   composite (checksum-of-parts) value is replaced by a backend-
///   computed full-object value — consumers pinning the old composite
///   value (or the old multipart ETag) will mismatch.
///
/// The copy itself is pinned to the HEADed generation via
/// `x-amz-copy-source-if-match` (see [`build_class_copy`]); a 412 maps
/// to [`CopyClassError::Raced`].
async fn copy_to_class(
    client: &Client,
    bucket: &str,
    key: &str,
    target_class: &str,
) -> Result<(), CopyClassError> {
    let head = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| CopyClassError::Other(format!("HeadObject(pre-copy attributes): {e}")))?;
    build_class_copy(client, bucket, key, target_class, &head)
        .send()
        .await
        .map(|_| ())
        .map_err(|e| {
            use aws_sdk_s3::error::ProvideErrorMetadata as _;
            let code = e.code().map(str::to_owned);
            let status = e.raw_response().map(|r| r.status().as_u16());
            let msg = format!("{e}");
            if copy_error_is_precondition(code.as_deref(), status, &msg) {
                CopyClassError::Raced
            } else {
                CopyClassError::Other(match code {
                    Some(c) => format!("{c}: {msg}"),
                    None => msg,
                })
            }
        })
}

/// Run one transition rule. Writes nothing unless `execute`. Listing /
/// versioning-probe failures abort the rule; per-object failures are
/// counted in the report.
///
/// Objects are processed **sequentially**: each transition is one
/// metadata-only server-side copy (no body transfer), so the win from
/// parallelism is small and the sequential order keeps the
/// main-then-sidecar pairing trivially race-free within the run.
pub(crate) async fn run_transition(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    rule: &TransitionRule,
    older_than: Option<Duration>,
    execute: bool,
) -> Result<TransitionReport, MaintainError> {
    let target = rule.storage_class.as_str();
    let cutoff = cutoff_epoch_secs(older_than);
    let inv = list_transition_inventory(client, bucket, prefix).await?;
    // Best-effort versioning probe — downgrade failures to a note, same
    // as migrate / recompact.
    let (versioning, versioning_note) =
        match crate::migrate::versioning_enabled(client, bucket).await {
            Ok(v) => (v, None),
            Err(e) => (false, Some(format!("{e}"))),
        };

    let mut report = TransitionReport {
        bucket: bucket.to_owned(),
        prefix: prefix.map(str::to_owned),
        dry_run: !execute,
        storage_class: target.to_owned(),
        older_than_secs: older_than.map(|d| d.as_secs()),
        total_objects: inv.mains.len() as u64,
        total_bytes: inv.mains.iter().map(|o| o.size).sum(),
        transitioned: 0,
        transitioned_sidecars: 0,
        skipped_already_target_class: 0,
        skipped_too_recent: 0,
        skipped_etag_raced: 0,
        skipped_too_large: 0,
        failed: 0,
        failures: Vec::new(),
        versioning_enabled: versioning,
        notes: Vec::new(),
    };

    for main in &inv.mains {
        // The companion sidecar, if it exists and is not at the target
        // class yet. Looked up from the same listing snapshot.
        let lagging_sidecar = inv
            .sidecars
            .get(&sidecar_key(&main.key))
            .filter(|sc| effective_class(sc.storage_class.as_deref()) != target);

        match classify_transition(
            main.storage_class.as_deref(),
            target,
            main.last_modified_epoch_secs,
            cutoff,
            main.size,
        ) {
            Some(TransitionSkip::TooRecent) => {
                report.skipped_too_recent += 1;
                continue;
            }
            Some(TransitionSkip::AlreadyTargetClass) => {
                report.skipped_already_target_class += 1;
                // Realign a sidecar a previous partial run (or a drifted
                // lifecycle rule) left behind — still "accompanying its
                // main", whose settled class is the target.
                if let Some(sc) = lagging_sidecar {
                    if execute {
                        match copy_to_class(client, bucket, &sc.key, target).await {
                            Ok(()) => report.transitioned_sidecars += 1,
                            // Sidecar overwritten mid-copy ⇒ a gateway
                            // write is in flight; its PUT settles the
                            // pair's classes, ours would fight it.
                            Err(CopyClassError::Raced) => report.skipped_etag_raced += 1,
                            Err(CopyClassError::Other(cause)) => {
                                report.failed += 1;
                                report.failures.push(TransitionFailure {
                                    key: sc.key.clone(),
                                    op: "CopyObject(sidecar)".to_owned(),
                                    cause: format!(
                                        "sidecar realign to {target} failed (its main object \
                                         is already {target}; re-run to retry): {cause}"
                                    ),
                                });
                            }
                        }
                    } else {
                        report.transitioned_sidecars += 1;
                    }
                }
                continue;
            }
            Some(TransitionSkip::TooLarge) => {
                report.skipped_too_large += 1;
                continue;
            }
            None => {}
        }

        if !execute {
            report.transitioned += 1;
            if lagging_sidecar.is_some() {
                report.transitioned_sidecars += 1;
            }
            continue;
        }

        // Conflict guard: re-HEAD and compare against the listing ETag.
        // The server-side copy always moves the *current* bytes, so the
        // guard is not about staleness — it keeps us from transitioning
        // an object that was overwritten (= became hot again) after the
        // listing snapshot, which would defeat the `older-than` intent.
        let head = match client
            .head_object()
            .bucket(bucket)
            .key(&main.key)
            .send()
            .await
        {
            Ok(h) => h,
            Err(e) => {
                report.failed += 1;
                report.failures.push(TransitionFailure {
                    key: main.key.clone(),
                    op: "HeadObject(pre-copy)".to_owned(),
                    cause: format!("{e}"),
                });
                continue;
            }
        };
        if let Some(listed_etag) = &main.etag
            && head.e_tag().map(crate::migrate::normalize_etag).as_ref() != Some(listed_etag)
        {
            report.skipped_etag_raced += 1;
            continue;
        }
        // Defensive idempotency re-check on the fresh HEAD: another
        // maintain run (or a lifecycle rule) may have moved the object
        // between the listing and now.
        if effective_class(head.storage_class().map(|c| c.as_str())) == target {
            report.skipped_already_target_class += 1;
            continue;
        }

        match copy_to_class(client, bucket, &main.key, target).await {
            Ok(()) => report.transitioned += 1,
            // 412 from the if-match pin: overwritten between our HEAD
            // and the copy — the object became hot again, exactly the
            // population the guard HEAD above already skips. Leave the
            // sidecar alone too: the in-flight gateway write settles
            // the pair.
            Err(CopyClassError::Raced) => {
                report.skipped_etag_raced += 1;
                continue;
            }
            Err(CopyClassError::Other(cause)) => {
                report.failed += 1;
                report.failures.push(TransitionFailure {
                    key: main.key.clone(),
                    op: "CopyObject".to_owned(),
                    cause,
                });
                continue;
            }
        }
        if let Some(sc) = lagging_sidecar {
            match copy_to_class(client, bucket, &sc.key, target).await {
                Ok(()) => report.transitioned_sidecars += 1,
                Err(CopyClassError::Raced) => report.skipped_etag_raced += 1,
                Err(CopyClassError::Other(cause)) => {
                    report.failed += 1;
                    report.failures.push(TransitionFailure {
                        key: sc.key.clone(),
                        op: "CopyObject(sidecar)".to_owned(),
                        cause: format!(
                            "main object transitioned to {target} but its sidecar copy \
                             failed — the pair's storage classes have drifted (Range GETs \
                             may degrade, see docs/storage-class-transitions.md); re-run \
                             to realign: {cause}"
                        ),
                    });
                }
            }
        }
    }

    report.failures.sort_by(|a, b| a.key.cmp(&b.key));

    if report.total_objects == 0 {
        report.notes.push("no objects found".into());
    }
    if report.dry_run {
        report.notes.push(
            "dry-run: nothing was copied; counts are selected from the live listing — pass \
             --execute to transition"
                .into(),
        );
    }
    report.notes.push(
        "each transition is a same-key server-side CopyObject (MetadataDirective REPLACE \
         re-sending the HEAD-captured user metadata + Content-* / Expires / \
         WebsiteRedirectLocation attributes — required by backends that reject same-key \
         COPY-directive copies), pinned with x-amz-copy-source-if-match to the HEADed \
         generation (a racing overwrite is counted as etag-raced, never stamped over): \
         metadata, tags and Content-Type ride along, but no encryption headers are \
         re-sent — a backend-SSE original is re-encrypted under the bucket's default \
         encryption (a per-object SSE-KMS key choice is lost; SSE-C originals fail the \
         copy), and a multipart original becomes single-part, so its composite \
         full-object checksum is replaced by a backend-computed one and its ETag changes \
         — the ETag change invalidates the sidecar's ETag binding until the next gateway \
         write, so Range GETs fall back to full reads (perf-only)"
            .into(),
    );
    report.notes.push(
        "sidecars accompany their main object: <key>.s4index moves to the same class in \
         the same run (and is realigned when a previous run left it behind); sidecars are \
         never transitioned on their own — orphans are `s4 sweep-orphan-sidecars` \
         territory"
            .into(),
    );
    report.notes.push(
        "objects already in GLACIER / DEEP_ARCHIVE cannot be copied without a prior \
         restore — such transitions fail with InvalidObjectState and the object is left \
         as-is"
            .into(),
    );
    if report.skipped_too_large > 0 {
        report.notes.push(format!(
            "{} object(s) skipped as too-large — a single server-side CopyObject moves at \
             most {} bytes; transition them with a multipart copy or a lifecycle rule",
            report.skipped_too_large, MAX_COPY_OBJECT_BYTES,
        ));
    }
    if report.versioning_enabled {
        report.notes.push(
            "WARNING: bucket versioning is Enabled — each transition copy leaves the \
             previous version in place, so storage is double-billed until old versions \
             are lifecycle-expired"
                .into(),
        );
    }
    if let Some(cause) = versioning_note {
        report.notes.push(format!(
            "could not determine bucket versioning state ({cause}); if versioning is \
             Enabled, transitioned objects double-bill until old versions expire"
        ));
    }
    if report.failed > 0 {
        report.notes.push(format!(
            "{} object(s) failed — see `failures`; re-running resumes automatically \
             (objects already at the target class are skipped)",
            report.failed,
        ));
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// maintain engine
// ---------------------------------------------------------------------------

/// Knobs shared by every rule of one maintain run. The codec-selection
/// half mirrors the server flags exactly like [`MigrateParams`] does,
/// so migrate rules pick the same codec the deployed gateway would.
#[derive(Debug, Clone)]
pub struct MaintainParams {
    /// `false` (default) = dry-run for **every** rule; `true` = write.
    pub execute: bool,
    /// Server `--codec`.
    pub default_codec: CodecKind,
    /// Server `--zstd-level`.
    pub zstd_level: i32,
    /// Server `--dispatcher`: `true` = sampling, `false` = always.
    pub use_sampling_dispatcher: bool,
    /// Server `--gpu-min-bytes`.
    pub gpu_min_bytes: usize,
    /// Server `--prefer-columnar-gpu`.
    pub prefer_columnar_gpu: bool,
    /// Real GPU probe result for this build/host (see [`MigrateParams`]).
    pub gpu_present: bool,
}

/// How one rule ended. `#[non_exhaustive]`: new actions (and therefore
/// new outcome shapes) may be added in minor releases.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RuleOutcome {
    Migrate {
        report: MigrateReport,
        /// Listed objects the rule's `older-than` cutoff excluded
        /// before examination. Rides next to the report because
        /// [`MigrateReport`] predates the age filter and is a frozen
        /// plain struct — see `migrate::run_migrate_with_cutoff`.
        skipped_too_recent: u64,
    },
    Recompact {
        report: RecompactReport,
    },
    Transition {
        report: TransitionReport,
    },
    /// The rule aborted before producing a report (listing failure,
    /// backend unreachable, …). Later rules still run.
    Error {
        message: String,
    },
}

impl RuleOutcome {
    /// `true` when the rule should flip the run's exit code: a
    /// rule-level abort, or any per-object hard failure in its report.
    pub fn failed(&self) -> bool {
        match self {
            RuleOutcome::Migrate { report, .. } => report.failed > 0,
            RuleOutcome::Recompact { report } => report.failed > 0,
            RuleOutcome::Transition { report } => report.failed > 0,
            RuleOutcome::Error { .. } => true,
        }
    }
}

/// One executed rule's slot in the run report.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct RuleReport {
    pub name: String,
    pub bucket: String,
    pub prefix: Option<String>,
    /// `"migrate"` / `"recompact"` / `"transition"`.
    pub action: String,
    pub older_than_secs: Option<u64>,
    pub outcome: RuleOutcome,
}

/// Full result of one maintain cycle. Serializes to the `--format json`
/// output verbatim.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct MaintainReport {
    /// `true` = no rule wrote anything (default mode).
    pub dry_run: bool,
    /// Rules in the policy.
    pub rules_total: u64,
    /// Rules actually executed this cycle (`< rules_total` only when a
    /// shutdown request interrupted the cycle).
    pub rules_run: u64,
    /// Executed rules that aborted or had per-object failures.
    pub rules_failed: u64,
    /// A shutdown request (SIGTERM / SIGINT) arrived mid-cycle: the
    /// in-flight rule was finished, the remaining ones were not run.
    pub interrupted: bool,
    /// Per-rule results, in execution (= policy file) order.
    pub rules: Vec<RuleReport>,
    pub notes: Vec<String>,
}

/// Execute every rule of `policy` sequentially against one backend.
/// Never aborts the run on a rule failure — the failed rule is recorded
/// ([`RuleOutcome::Error`]) and the next rule runs, so one unreachable
/// bucket cannot starve the rest of a nightly cycle. Callers map
/// `rules_failed > 0` to a non-zero exit (one-shot mode) or an error
/// log (resident mode).
///
/// `shutdown` (optional) is checked **between** rules: when it reads
/// `true` the cycle stops without starting the next rule — the graceful
/// half of the `--interval` SIGTERM contract. The flag is never checked
/// mid-rule; an in-flight rule always completes.
pub async fn run_maintain(
    client: &Client,
    policy: &MaintainPolicy,
    params: &MaintainParams,
    shutdown: Option<&AtomicBool>,
) -> MaintainReport {
    let mut report = MaintainReport {
        dry_run: !params.execute,
        rules_total: policy.rules.len() as u64,
        rules_run: 0,
        rules_failed: 0,
        interrupted: false,
        rules: Vec::with_capacity(policy.rules.len()),
        notes: Vec::new(),
    };

    for rule in &policy.rules {
        if let Some(flag) = shutdown
            && flag.load(Ordering::SeqCst)
        {
            report.interrupted = true;
            break;
        }
        let outcome = match &rule.action {
            RuleAction::Migrate(m) => {
                let mp = MigrateParams {
                    prefix: rule.prefix.clone(),
                    execute: params.execute,
                    concurrency: m.concurrency,
                    max_objects: m.max_objects,
                    max_body_bytes: m.max_body_bytes,
                    default_codec: params.default_codec,
                    zstd_level: params.zstd_level,
                    use_sampling_dispatcher: params.use_sampling_dispatcher,
                    gpu_min_bytes: params.gpu_min_bytes,
                    prefer_columnar_gpu: params.prefer_columnar_gpu,
                    gpu_present: params.gpu_present,
                    no_tags: m.no_tags,
                };
                match crate::migrate::run_migrate_with_cutoff(
                    client,
                    &rule.bucket,
                    &mp,
                    rule.older_than,
                )
                .await
                {
                    Ok((report, skipped_too_recent)) => RuleOutcome::Migrate {
                        report,
                        skipped_too_recent,
                    },
                    Err(e) => RuleOutcome::Error {
                        message: e.to_string(),
                    },
                }
            }
            RuleAction::Recompact(rc) => {
                let rp = RecompactParams {
                    prefix: rule.prefix.clone(),
                    execute: params.execute,
                    concurrency: rc.concurrency,
                    max_objects: rc.max_objects,
                    max_body_bytes: rc.max_body_bytes,
                    target_zstd_level: rc.target_zstd_level,
                    min_gain_percent: rc.min_gain_percent,
                    older_than: rule.older_than,
                    assume_unstamped_framed: rc.assume_unstamped_framed,
                    no_tags: rc.no_tags,
                };
                match crate::recompact::run_recompact(client, &rule.bucket, &rp).await {
                    Ok(report) => RuleOutcome::Recompact { report },
                    Err(e) => RuleOutcome::Error {
                        message: e.to_string(),
                    },
                }
            }
            RuleAction::Transition(t) => {
                match run_transition(
                    client,
                    &rule.bucket,
                    rule.prefix.as_deref(),
                    t,
                    rule.older_than,
                    params.execute,
                )
                .await
                {
                    Ok(report) => RuleOutcome::Transition { report },
                    Err(e) => RuleOutcome::Error {
                        message: e.to_string(),
                    },
                }
            }
        };
        report.rules_run += 1;
        if outcome.failed() {
            report.rules_failed += 1;
        }
        report.rules.push(RuleReport {
            name: rule.name.clone(),
            bucket: rule.bucket.clone(),
            prefix: rule.prefix.clone(),
            action: rule.action.as_str().to_owned(),
            older_than_secs: rule.older_than.map(|d| d.as_secs()),
            outcome,
        });
    }

    if report.dry_run {
        report
            .notes
            .push("dry-run: no rule wrote anything — pass --execute to apply the policy".into());
    }
    report.notes.push(
        "rules run sequentially against the bucket's current state; a dry-run cannot \
         simulate the effects of earlier rules in the same policy (e.g. a transition \
         rule's dry-run does not see the sidecars a preceding migrate rule would create)"
            .into(),
    );
    if report.interrupted {
        let not_run: Vec<&str> = policy
            .rules
            .iter()
            .skip(report.rules_run as usize)
            .map(|r| r.name.as_str())
            .collect();
        report.notes.push(format!(
            "shutdown requested mid-cycle: the in-flight rule was finished, {} rule(s) \
             were not run ({})",
            not_run.len(),
            not_run.join(", "),
        ));
    }
    if report.rules_failed > 0 {
        report.notes.push(format!(
            "{} rule(s) failed — see the per-rule reports; re-running resumes \
             automatically (all three actions are idempotent)",
            report.rules_failed,
        ));
    }
    report
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// Render one transition report for `--format table`, same shape family
/// as `migrate::render_human` / `recompact::render_human`.
pub fn render_transition_human(report: &TransitionReport) -> String {
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
    let _ = writeln!(out, "S4 transition {target} — {mode}");
    let older = match report.older_than_secs {
        Some(secs) => format!(
            "   older-than: {}",
            crate::recompact::human_duration_secs(secs)
        ),
        None => String::new(),
    };
    let _ = writeln!(
        out,
        "  target storage class: {}{}",
        report.storage_class, older,
    );
    let _ = writeln!(
        out,
        "  objects: {}   total: {} ({} bytes)",
        report.total_objects,
        crate::migrate::human_bytes(report.total_bytes),
        report.total_bytes,
    );
    let verb = if report.dry_run {
        "would transition"
    } else {
        "transitioned"
    };
    let _ = writeln!(
        out,
        "  {verb}: {} object(s) + {} sidecar(s)",
        report.transitioned, report.transitioned_sidecars,
    );
    let _ = writeln!(
        out,
        "  skipped: {} already-target-class, {} too-recent, {} etag-raced, {} too-large",
        report.skipped_already_target_class,
        report.skipped_too_recent,
        report.skipped_etag_raced,
        report.skipped_too_large,
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

/// Render the full maintain run for `--format table`: a one-line run
/// header, one titled section per rule (embedding the action's own
/// human renderer), and the run-level notes.
pub fn render_human(report: &MaintainReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mode = if report.dry_run {
        "dry-run (pass --execute to write)"
    } else {
        "execute"
    };
    let _ = writeln!(out, "S4 maintain — {mode}");
    let _ = writeln!(
        out,
        "  rules: {} ({} run, {} failed)",
        report.rules_total, report.rules_run, report.rules_failed,
    );
    for rule in &report.rules {
        let target = match &rule.prefix {
            Some(p) => format!("{}/{}", rule.bucket, p),
            None => rule.bucket.clone(),
        };
        let older = match rule.older_than_secs {
            Some(secs) => format!(
                ", older-than {}",
                crate::recompact::human_duration_secs(secs)
            ),
            None => String::new(),
        };
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "=== rule {:?} — {} {}{} ===",
            rule.name, rule.action, target, older,
        );
        match &rule.outcome {
            RuleOutcome::Migrate {
                report,
                skipped_too_recent,
            } => {
                out.push_str(&crate::migrate::render_human(report));
                if *skipped_too_recent > 0 {
                    let _ = writeln!(
                        out,
                        "  (older-than cutoff excluded {skipped_too_recent} listed \
                         object(s) before examination)"
                    );
                }
            }
            RuleOutcome::Recompact { report } => {
                out.push_str(&crate::recompact::render_human(report));
            }
            RuleOutcome::Transition { report } => {
                out.push_str(&render_transition_human(report));
            }
            RuleOutcome::Error { message } => {
                let _ = writeln!(out, "  ERROR: {message}");
            }
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

    const VALID_POLICY: &str = r#"
[[rule]]
name = "compress-new-logs"
bucket = "prod-logs"
prefix = "app/"
action = "migrate"
older-than = "7d"

[[rule]]
name = "bake-cold-logs"
bucket = "prod-logs"
action = "recompact"
older-than = "30d"
target-zstd-level = 19
min-gain-percent = 5.0

[[rule]]
name = "archive-old-logs"
bucket = "prod-logs"
prefix = "app/2025/"
action = "transition"
older-than = "90d"
storage-class = "GLACIER_IR"
"#;

    fn invalid_errors(text: &str) -> Vec<String> {
        match parse_policy(text) {
            Err(MaintainError::PolicyInvalid { errors }) => errors,
            other => panic!("expected PolicyInvalid, got {other:?}"),
        }
    }

    #[test]
    fn parse_valid_policy_preserves_order_and_defaults() {
        let policy = parse_policy(VALID_POLICY).expect("valid policy");
        assert_eq!(policy.rules.len(), 3);
        // File order is execution order.
        assert_eq!(policy.rules[0].name, "compress-new-logs");
        assert_eq!(policy.rules[1].name, "bake-cold-logs");
        assert_eq!(policy.rules[2].name, "archive-old-logs");

        let m = &policy.rules[0];
        assert_eq!(m.bucket, "prod-logs");
        assert_eq!(m.prefix.as_deref(), Some("app/"));
        assert_eq!(m.older_than, Some(Duration::from_secs(7 * 86_400)));
        match &m.action {
            RuleAction::Migrate(mr) => {
                // CLI defaults applied.
                assert!(!mr.no_tags);
                assert_eq!(mr.concurrency, crate::migrate::DEFAULT_MIGRATE_CONCURRENCY);
                assert_eq!(mr.max_objects, None);
                assert_eq!(
                    mr.max_body_bytes,
                    crate::repair::DEFAULT_REPAIR_BODY_BYTES_CAP
                );
            }
            other => panic!("expected migrate, got {other:?}"),
        }

        match &policy.rules[1].action {
            RuleAction::Recompact(rc) => {
                assert_eq!(rc.target_zstd_level, 19);
                assert!((rc.min_gain_percent - 5.0).abs() < f64::EPSILON);
                assert!(!rc.assume_unstamped_framed);
                assert_eq!(
                    rc.concurrency,
                    crate::recompact::DEFAULT_RECOMPACT_CONCURRENCY
                );
            }
            other => panic!("expected recompact, got {other:?}"),
        }

        match &policy.rules[2].action {
            RuleAction::Transition(t) => assert_eq!(t.storage_class, "GLACIER_IR"),
            other => panic!("expected transition, got {other:?}"),
        }
    }

    #[test]
    fn unknown_key_rejected_at_parse_time() {
        let text = r#"
[[rule]]
name = "x"
bucket = "b"
action = "migrate"
not-a-real-key = true
"#;
        match parse_policy(text) {
            Err(MaintainError::PolicyParse { cause }) => {
                assert!(cause.contains("not-a-real-key"), "cause: {cause}");
            }
            other => panic!("expected PolicyParse, got {other:?}"),
        }
    }

    #[test]
    fn unknown_action_rejected() {
        let errors = invalid_errors(
            r#"
[[rule]]
name = "x"
bucket = "b"
action = "defragment"
"#,
        );
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("unknown action \"defragment\""),
            "{errors:?}"
        );
    }

    #[test]
    fn duplicate_rule_names_rejected() {
        let errors = invalid_errors(
            r#"
[[rule]]
name = "same"
bucket = "b"
action = "migrate"

[[rule]]
name = "same"
bucket = "b"
action = "recompact"
"#,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("duplicate rule name"), "{errors:?}");
    }

    #[test]
    fn invalid_duration_rejected() {
        let errors = invalid_errors(
            r#"
[[rule]]
name = "x"
bucket = "b"
action = "migrate"
older-than = "1h30m"
"#,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("older-than"), "{errors:?}");
    }

    #[test]
    fn invalid_storage_class_rejected() {
        let errors = invalid_errors(
            r#"
[[rule]]
name = "x"
bucket = "b"
action = "transition"
storage-class = "glacier_ir"
"#,
        );
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("unknown storage-class \"glacier_ir\""),
            "{errors:?}"
        );
        // The message lists the canonical values so the operator can fix
        // the casing without leaving the terminal.
        assert!(errors[0].contains("GLACIER_IR"), "{errors:?}");
    }

    #[test]
    fn transition_requires_storage_class() {
        let errors = invalid_errors(
            r#"
[[rule]]
name = "x"
bucket = "b"
action = "transition"
"#,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("requires `storage-class`"), "{errors:?}");
    }

    #[test]
    fn inapplicable_keys_rejected_per_action() {
        // recompact knob + transition knob on a migrate rule.
        let errors = invalid_errors(
            r#"
[[rule]]
name = "x"
bucket = "b"
action = "migrate"
target-zstd-level = 19
storage-class = "GLACIER"
"#,
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("`target-zstd-level`")));
        assert!(errors.iter().any(|e| e.contains("`storage-class`")));
    }

    #[test]
    fn all_validation_errors_reported_in_one_pass() {
        let errors = invalid_errors(
            r#"
[[rule]]
bucket = "b"
action = "shrink"

[[rule]]
name = "y"
action = "transition"
older-than = "soon"
"#,
        );
        // rule #1: missing name + unknown action;
        // rule "y": missing bucket + bad duration + missing storage-class.
        assert_eq!(errors.len(), 5, "{errors:?}");
        let display = MaintainError::PolicyInvalid { errors }.to_string();
        assert!(display.contains("5 error(s)"), "{display}");
    }

    #[test]
    fn empty_policy_rejected() {
        let errors = invalid_errors("");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("no [[rule]]"), "{errors:?}");
    }

    #[test]
    fn transition_classification() {
        // Age gate first: a recent object is too-recent even when its
        // class already matches.
        assert_eq!(
            classify_transition(
                Some("GLACIER_IR"),
                "GLACIER_IR",
                Some(2_000),
                Some(1_000),
                10
            ),
            Some(TransitionSkip::TooRecent)
        );
        // Unknown LastModified under an active cutoff = too recent
        // (same conservative rule as recompact).
        assert_eq!(
            classify_transition(Some("STANDARD"), "GLACIER_IR", None, Some(1_000), 10),
            Some(TransitionSkip::TooRecent)
        );
        // Idempotency: already at the target class.
        assert_eq!(
            classify_transition(Some("GLACIER_IR"), "GLACIER_IR", Some(500), Some(1_000), 10),
            Some(TransitionSkip::AlreadyTargetClass)
        );
        // Absent class == STANDARD.
        assert_eq!(
            classify_transition(None, "STANDARD", None, None, 10),
            Some(TransitionSkip::AlreadyTargetClass)
        );
        // CopyObject single-op ceiling.
        assert_eq!(
            classify_transition(None, "GLACIER_IR", None, None, MAX_COPY_OBJECT_BYTES + 1),
            Some(TransitionSkip::TooLarge)
        );
        // Candidate: old enough, different class, copyable size.
        assert_eq!(
            classify_transition(Some("STANDARD"), "GLACIER_IR", Some(500), Some(1_000), 10),
            None
        );
        assert_eq!(
            classify_transition(None, "GLACIER_IR", None, None, 10),
            None
        );
    }

    #[test]
    fn cutoff_arithmetic() {
        assert_eq!(cutoff_epoch_secs(None), None);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs() as i64;
        let cutoff = cutoff_epoch_secs(Some(Duration::from_secs(3600))).expect("some");
        // now - 1h, within a few seconds of slop.
        assert!(
            (cutoff - (now - 3600)).abs() < 5,
            "cutoff {cutoff} now {now}"
        );
        // Absurd durations saturate instead of panicking: u64::MAX
        // seconds clamps to i64::MAX, so the cutoff bottoms out at
        // i64::MIN-ish via saturating_sub — everything is "old enough".
        let saturated =
            cutoff_epoch_secs(Some(Duration::from_secs(u64::MAX))).expect("some cutoff");
        assert!(saturated <= 0, "saturated cutoff {saturated}");
    }

    #[test]
    fn copy_source_percent_encodes_but_keeps_slashes() {
        assert_eq!(copy_source("b", "plain/a.log"), "b/plain/a.log");
        assert_eq!(
            copy_source("b", "dir with space/k+v.log"),
            "b/dir%20with%20space/k%2Bv.log"
        );
    }

    #[test]
    fn effective_class_normalizes_absent_to_standard() {
        assert_eq!(effective_class(None), "STANDARD");
        assert_eq!(effective_class(Some("")), "STANDARD");
        assert_eq!(effective_class(Some("GLACIER")), "GLACIER");
    }

    /// Offline client good enough to *build* requests (nothing is sent).
    fn offline_client() -> Client {
        Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .build(),
        )
    }

    /// v1.2 audit R1 P2 + P3: the storage-class copy must pin the
    /// source generation with `x-amz-copy-source-if-match` and re-send
    /// every HEAD-surfaced attribute (now including Expires and
    /// WebsiteRedirectLocation) under the REPLACE directive.
    #[test]
    fn class_copy_pins_etag_and_resends_attributes() {
        let client = offline_client();
        #[allow(deprecated)] // expires: only parsed-DateTime round-trips into CopyObjectInput
        let head = aws_sdk_s3::operation::head_object::HeadObjectOutput::builder()
            .e_tag("\"d41d8cd98f00b204e9800998ecf8427e\"")
            .content_type("text/plain")
            .cache_control("max-age=60")
            .content_disposition("attachment")
            .content_encoding("gzip")
            .content_language("en")
            .website_redirect_location("/elsewhere")
            .expires(aws_sdk_s3::primitives::DateTime::from_secs(1_700_000_000))
            .metadata("s4-codec", "cpu-zstd")
            .build();
        let req = build_class_copy(&client, "bkt", "logs/a.log", "GLACIER_IR", &head);
        let input = req.as_input();
        assert_eq!(
            input.get_copy_source_if_match().as_deref(),
            Some("\"d41d8cd98f00b204e9800998ecf8427e\""),
            "copy must be pinned to the HEADed ETag"
        );
        assert_eq!(input.get_bucket().as_deref(), Some("bkt"));
        assert_eq!(input.get_key().as_deref(), Some("logs/a.log"));
        assert_eq!(input.get_copy_source().as_deref(), Some("bkt/logs/a.log"));
        assert_eq!(
            input.get_metadata_directive(),
            &Some(aws_sdk_s3::types::MetadataDirective::Replace)
        );
        assert_eq!(input.get_content_type().as_deref(), Some("text/plain"));
        assert_eq!(input.get_cache_control().as_deref(), Some("max-age=60"));
        assert_eq!(
            input.get_content_disposition().as_deref(),
            Some("attachment")
        );
        assert_eq!(input.get_content_encoding().as_deref(), Some("gzip"));
        assert_eq!(input.get_content_language().as_deref(), Some("en"));
        assert_eq!(
            input.get_website_redirect_location().as_deref(),
            Some("/elsewhere"),
            "WebsiteRedirectLocation must survive the REPLACE copy"
        );
        #[allow(deprecated)]
        let expires = *input.get_expires();
        assert_eq!(
            expires,
            Some(aws_sdk_s3::primitives::DateTime::from_secs(1_700_000_000)),
            "Expires must survive the REPLACE copy"
        );
        assert_eq!(
            input
                .get_metadata()
                .as_ref()
                .and_then(|m| m.get("s4-codec"))
                .map(String::as_str),
            Some("cpu-zstd"),
            "user metadata (s4-* stamps) must ride along"
        );
        assert_eq!(
            input.get_storage_class(),
            &Some(aws_sdk_s3::types::StorageClass::GlacierIr)
        );
    }

    /// A HEAD without an ETag (no known backend) degrades to the
    /// unguarded copy instead of sending an empty if-match.
    #[test]
    fn class_copy_without_etag_sends_no_if_match() {
        let client = offline_client();
        let head = aws_sdk_s3::operation::head_object::HeadObjectOutput::builder().build();
        let req = build_class_copy(&client, "bkt", "k", "STANDARD_IA", &head);
        assert_eq!(req.as_input().get_copy_source_if_match(), &None);
    }

    #[test]
    fn precondition_classifier_matches_all_three_shapes() {
        // Modeled error code (AWS, MinIO both send it).
        assert!(copy_error_is_precondition(
            Some("PreconditionFailed"),
            None,
            ""
        ));
        // Raw HTTP status only.
        assert!(copy_error_is_precondition(None, Some(412), "opaque"));
        // Message substring as last resort.
        assert!(copy_error_is_precondition(
            None,
            None,
            "service error: PreconditionFailed: At least one of the pre-conditions you \
             specified did not hold"
        ));
        // Everything else stays a hard failure.
        assert!(!copy_error_is_precondition(
            Some("NoSuchKey"),
            Some(404),
            "NoSuchKey"
        ));
        assert!(!copy_error_is_precondition(None, None, "timeout"));
    }
}
