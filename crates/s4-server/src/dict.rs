//! v1.1 `--zstd-dict`: shared-dictionary zstd for small objects.
//!
//! Plain zstd buys ~nothing on single-digit-KiB objects (JSON events,
//! per-line log PUTs) — the window never sees cross-object redundancy.
//! This module wires `s4_codec::cpu_zstd_dict` into the gateway:
//!
//! - **Training** ([`run_train_dict`], CLI `s4 train-dict <bucket>/<prefix>
//!   --endpoint-url <BACKEND>`): samples small objects under a prefix
//!   straight from the backend (same backend-direct posture as `s4
//!   migrate`), trains a stock zstd dictionary, and PUTs it to the
//!   in-bucket `.s4dict/<dict-id>` object.
//! - **Naming**: `<dict-id>` = first 16 hex chars of the SHA-256 of the
//!   dictionary bytes — content-addressed, so a dict object is immutable
//!   by construction (re-PUT of the same id must carry identical bytes;
//!   anything else is refused at train time and fails the fingerprint
//!   check at fetch time).
//! - **Gateway config** ([`parse_zstd_dict_flag`] / [`DictStore`]): each
//!   `--zstd-dict '<bucket>/<key-prefix>=<dict-id>'` maps a key prefix to
//!   a dictionary; the dict bytes are fetched from the backend at boot
//!   (missing dict = boot error). PUTs whose key longest-prefix-matches
//!   and whose size fits `--zstd-dict-max-bytes` compress with the dict
//!   **iff it actually wins** over dict-less cpu-zstd (both are computed —
//!   acceptable because the path is capped to small bodies).
//! - **GET resilience** ([`DictCache`]): a gateway that no longer carries
//!   the `--zstd-dict` flag (or never did) can still read dict objects:
//!   the GET path lazy-fetches `.s4dict/<id>` from the object's bucket,
//!   verifies the fingerprint, and caches it in a small LRU.
//!
//! `.s4dict/` keys are hidden from listings (same treatment as
//! `.s4index` sidecars / `.__s4ver__/` shadow versions).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aws_sdk_s3::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Reserved in-bucket prefix where trained dictionaries live.
pub const DICT_KEY_PREFIX: &str = ".s4dict/";

/// Hex chars of the SHA-256 prefix used as the dictionary id.
pub const DICT_ID_HEX_LEN: usize = 16;

/// LRU capacity of the lazy GET-side dictionary cache. Dictionaries are
/// ≤ ~110 KiB each, so 16 slots bound the cache at ~2 MiB.
pub const DICT_CACHE_CAPACITY: usize = 16;

/// Default `--zstd-dict-max-bytes`: only bodies up to this size take the
/// compress-both-ways dict path (1 MiB).
pub const DEFAULT_DICT_MAX_OBJECT_BYTES: usize = 1024 * 1024;

/// Default `train-dict --max-dict-bytes` (110 KiB, zstd upstream's
/// recommended dictionary size).
pub const DEFAULT_MAX_DICT_BYTES: usize = 112_640;

/// Default `train-dict --max-samples`.
pub const DEFAULT_TRAIN_MAX_SAMPLES: usize = 1000;

/// Default `train-dict --min-samples` — below this, training is refused
/// (ZDICT output from a handful of samples is noise).
pub const DEFAULT_TRAIN_MIN_SAMPLES: usize = 8;

/// Default `train-dict --sample-max-bytes` — objects larger than this are
/// skipped during sampling (the feature targets small objects; big bodies
/// also dominate ZDICT training unhelpfully). 64 KiB.
pub const DEFAULT_TRAIN_SAMPLE_MAX_BYTES: u64 = 64 * 1024;

/// `<dict-id>` for a dictionary's raw bytes: first 16 hex chars of its
/// SHA-256 (lowercase).
pub fn dict_id_of(dict_bytes: &[u8]) -> String {
    let digest = Sha256::digest(dict_bytes);
    let mut s = String::with_capacity(DICT_ID_HEX_LEN);
    for byte in digest.iter().take(DICT_ID_HEX_LEN / 2) {
        use std::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Validate a dict-id: exactly 16 lowercase hex chars. Anything else is
/// refused at flag-parse time AND at GET time (the id is spliced into a
/// backend key, so this also keeps `s4-dict-id` metadata from smuggling
/// path segments like `../`).
pub fn is_valid_dict_id(id: &str) -> bool {
    id.len() == DICT_ID_HEX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Backend key of a dictionary object.
pub fn dict_object_key(dict_id: &str) -> String {
    format!("{DICT_KEY_PREFIX}{dict_id}")
}

/// `true` for keys under the reserved `.s4dict/` prefix (hidden from
/// listings).
pub fn is_dict_key(key: &str) -> bool {
    key.starts_with(DICT_KEY_PREFIX)
}

/// One parsed `--zstd-dict <bucket>/<key-prefix>=<dict-id>` entry.
/// `prefix` keeps the combined `bucket/key-prefix` form used for
/// longest-prefix matching; `bucket` is split out for the boot-time fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictConfigEntry {
    /// Combined `<bucket>/<key-prefix>` (the key-prefix may be empty =
    /// whole bucket).
    pub prefix: String,
    /// Bucket parsed off the front of `prefix`.
    pub bucket: String,
    pub dict_id: String,
}

/// Parse one `--zstd-dict` value. The dict-id is 16 hex chars after the
/// **last** `=` (S3 keys may legally contain `=`).
pub fn parse_zstd_dict_flag(spec: &str) -> Result<DictConfigEntry, String> {
    let (prefix, dict_id) = spec
        .rsplit_once('=')
        .ok_or_else(|| format!("expected '<bucket>/<key-prefix>=<dict-id>', got {spec:?}"))?;
    if !is_valid_dict_id(dict_id) {
        return Err(format!(
            "dict-id must be exactly {DICT_ID_HEX_LEN} lowercase hex chars, got {dict_id:?}"
        ));
    }
    let (bucket, _key_prefix) = prefix
        .split_once('/')
        .ok_or_else(|| format!("prefix must be '<bucket>/<key-prefix>', got {prefix:?}"))?;
    if bucket.is_empty() {
        return Err(format!("empty bucket in {spec:?}"));
    }
    Ok(DictConfigEntry {
        prefix: prefix.to_owned(),
        bucket: bucket.to_owned(),
        dict_id: dict_id.to_owned(),
    })
}

/// Boot-time dictionary store: configured prefix→dict mappings plus the
/// preloaded dictionary bytes. Built in `main.rs` from the `--zstd-dict`
/// flags (every dict GETted from the backend; a missing dict is a boot
/// error) and handed to `S4Service` via `with_zstd_dicts`.
#[derive(Debug)]
pub struct DictStore {
    /// `(combined-prefix, dict-id)` sorted by prefix length descending so
    /// the first match is the longest match.
    entries: Vec<(String, String)>,
    /// dict-id → dictionary bytes (fingerprint-verified at load).
    dicts: HashMap<String, Arc<[u8]>>,
    /// PUT-side body-size ceiling for the dict path (`--zstd-dict-max-bytes`).
    max_object_bytes: usize,
    /// zstd level used for the dict compressor (server `--zstd-level`).
    level: i32,
}

impl DictStore {
    /// Assemble a store from parsed entries + already-fetched dict bytes.
    /// Verifies every dictionary's fingerprint against its configured id
    /// and rejects duplicates-with-different-ids for the same prefix.
    pub fn new(
        entries: Vec<DictConfigEntry>,
        dict_bytes: HashMap<String, Vec<u8>>,
        max_object_bytes: usize,
        level: i32,
    ) -> Result<Self, String> {
        let mut dicts: HashMap<String, Arc<[u8]>> = HashMap::new();
        for (id, bytes) in dict_bytes {
            let actual = dict_id_of(&bytes);
            if actual != id {
                return Err(format!(
                    "dictionary fingerprint mismatch for id {id}: backend object hashes to \
                     {actual} (corrupted / tampered `.s4dict/{id}`?)"
                ));
            }
            if bytes.is_empty() {
                return Err(format!("dictionary {id} is empty"));
            }
            dicts.insert(id, Arc::from(bytes.into_boxed_slice()));
        }
        let mut prefixes: Vec<(String, String)> = Vec::with_capacity(entries.len());
        for e in entries {
            if !dicts.contains_key(&e.dict_id) {
                return Err(format!(
                    "no dictionary bytes loaded for id {} (prefix {:?})",
                    e.dict_id, e.prefix
                ));
            }
            if let Some((_, existing)) = prefixes.iter().find(|(p, _)| *p == e.prefix) {
                if *existing != e.dict_id {
                    return Err(format!(
                        "prefix {:?} configured twice with different dict-ids ({existing} vs {})",
                        e.prefix, e.dict_id
                    ));
                }
                continue;
            }
            prefixes.push((e.prefix, e.dict_id));
        }
        // Longest prefix first → first match wins below.
        prefixes.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(Self {
            entries: prefixes,
            dicts,
            max_object_bytes,
            level,
        })
    }

    /// Longest-prefix match of `<bucket>/<key>` against the configured
    /// prefixes. Returns `(dict_id, dict_bytes)` on hit.
    pub fn lookup(&self, bucket: &str, key: &str) -> Option<(String, Arc<[u8]>)> {
        // Avoid allocating the combined string per PUT: match the bucket
        // part and the key part separately against each entry.
        for (prefix, dict_id) in &self.entries {
            let Some(rest) = prefix.strip_prefix(bucket) else {
                continue;
            };
            let Some(key_prefix) = rest.strip_prefix('/') else {
                continue; // bucket name was only a prefix of the entry's bucket
            };
            if key.starts_with(key_prefix) {
                let dict = self.dicts.get(dict_id)?;
                return Some((dict_id.clone(), Arc::clone(dict)));
            }
        }
        None
    }

    /// Preloaded dictionary bytes by id (GET-side fast path before any
    /// LRU / backend fetch).
    pub fn get_preloaded(&self, dict_id: &str) -> Option<Arc<[u8]>> {
        self.dicts.get(dict_id).map(Arc::clone)
    }

    pub fn max_object_bytes(&self) -> usize {
        self.max_object_bytes
    }

    pub fn level(&self) -> i32 {
        self.level
    }

    /// Configured `(prefix, dict_id)` pairs (boot logging).
    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }
}

/// PUT-side decision: take the dict result only when it is strictly
/// smaller than dict-less cpu-zstd. Ties go to plain zstd — equal size
/// with one fewer moving part (no dictionary needed at read time).
pub fn dict_wins(dict_compressed_len: usize, plain_compressed_len: usize) -> bool {
    dict_compressed_len < plain_compressed_len
}

/// GET-side lazy dictionary LRU. Always attached to the service (even
/// with no `--zstd-dict` flags) so objects carrying `s4-dict-id` stay
/// readable after the operator drops the flag: on miss the service
/// fetches `.s4dict/<id>` from the object's bucket, verifies the
/// fingerprint, and inserts here. Keyed by dict-id only — ids are
/// content-addressed, so equal id ⇒ equal bytes regardless of bucket.
#[derive(Debug, Default)]
pub struct DictCache {
    inner: Mutex<DictCacheInner>,
}

#[derive(Debug, Default)]
struct DictCacheInner {
    map: HashMap<String, Arc<[u8]>>,
    /// Recency queue, most-recent at the back.
    order: Vec<String>,
}

impl DictCache {
    pub fn get(&self, dict_id: &str) -> Option<Arc<[u8]>> {
        let mut inner = self.inner.lock().ok()?;
        let hit = inner.map.get(dict_id).map(Arc::clone)?;
        if let Some(pos) = inner.order.iter().position(|k| k == dict_id) {
            let k = inner.order.remove(pos);
            inner.order.push(k);
        }
        Some(hit)
    }

    pub fn insert(&self, dict_id: String, dict: Arc<[u8]>) {
        let Ok(mut inner) = self.inner.lock() else {
            return; // poisoned lock: degrade to cache-off (refetch next GET)
        };
        if inner.map.contains_key(&dict_id) {
            if let Some(pos) = inner.order.iter().position(|k| *k == dict_id) {
                let k = inner.order.remove(pos);
                inner.order.push(k);
            }
            return;
        }
        while inner.map.len() >= DICT_CACHE_CAPACITY {
            if inner.order.is_empty() {
                break; // defensive: map/order out of sync
            }
            let evicted = inner.order.remove(0);
            inner.map.remove(&evicted);
        }
        inner.map.insert(dict_id.clone(), dict);
        inner.order.push(dict_id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|i| i.map.len()).unwrap_or(0)
    }
}

// ===========================================================================
// `s4 train-dict` — backend-direct training tool (same posture as migrate)
// ===========================================================================

/// v1.1 stability: `#[non_exhaustive]` — new training failure modes may be
/// added in minor releases.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TrainDictError {
    #[error("S3 backend error on {op} {bucket}/{key}: {cause}")]
    Backend {
        op: &'static str,
        bucket: String,
        key: String,
        cause: String,
    },
    #[error(
        "not enough samples under {bucket}/{prefix}: found {found} usable objects, \
         need at least {min} (small objects ≤ sample-max-bytes, not already S4-compressed)"
    )]
    NotEnoughSamples {
        bucket: String,
        prefix: String,
        found: usize,
        min: usize,
    },
    #[error("zstd dictionary training failed: {0}")]
    Training(String),
    #[error(
        "`.s4dict/{dict_id}` already exists with DIFFERENT bytes — dictionary objects are \
         immutable; this should be impossible for a content-addressed id (backend corruption?)"
    )]
    DictObjectConflict { dict_id: String },
}

/// Knobs for one training run.
#[derive(Debug, Clone)]
pub struct TrainDictParams {
    /// Key prefix under the bucket to sample (may be empty = whole bucket).
    pub prefix: String,
    /// Stop after sampling this many objects.
    pub max_samples: usize,
    /// Dictionary output size cap.
    pub max_dict_bytes: usize,
    /// Refuse to train below this many usable samples.
    pub min_samples: usize,
    /// Skip objects larger than this many bytes.
    pub sample_max_bytes: u64,
    /// zstd level recorded in the report (training itself is level-free).
    pub zstd_level: i32,
}

/// Result of one training run; `--format`-less, rendered by `main.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct TrainDictReport {
    pub bucket: String,
    pub prefix: String,
    pub sampled_objects: usize,
    pub sampled_bytes: u64,
    pub skipped_too_large: usize,
    pub skipped_already_s4: usize,
    pub dict_id: String,
    pub dict_bytes: usize,
    /// `true` when `.s4dict/<id>` already existed with identical bytes
    /// (idempotent re-train).
    pub dict_already_existed: bool,
    /// The exact gateway flag to copy-paste.
    pub gateway_flag: String,
}

/// S4F2 / S4P1 magic probe (same constants the gateway writes). Objects
/// already in S4 format are compressed bytes — training on them is
/// counterproductive, so they're skipped and counted.
fn looks_like_s4_body(head: &[u8]) -> bool {
    head.len() >= 4
        && (head[..4] == *s4_codec::multipart::FRAME_MAGIC
            || head[..4] == *s4_codec::multipart::PADDING_MAGIC)
}

/// List + GET small objects under the prefix, train, PUT `.s4dict/<id>`.
pub async fn run_train_dict(
    client: &Client,
    bucket: &str,
    params: &TrainDictParams,
) -> Result<TrainDictReport, TrainDictError> {
    let backend_err = |op: &'static str, key: &str| {
        let bucket = bucket.to_owned();
        let key = key.to_owned();
        move |e: String| TrainDictError::Backend {
            op,
            bucket,
            key,
            cause: e,
        }
    };

    // -- 1. list candidate keys ------------------------------------------
    let mut candidates: Vec<(String, u64)> = Vec::new();
    let mut skipped_too_large = 0usize;
    let mut continuation: Option<String> = None;
    'pages: loop {
        let mut req = client.list_objects_v2().bucket(bucket);
        if !params.prefix.is_empty() {
            req = req.prefix(&params.prefix);
        }
        if let Some(c) = continuation.as_ref() {
            req = req.continuation_token(c);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| backend_err("ListObjectsV2", "")(format!("{e}")))?;
        for obj in resp.contents() {
            let Some(k) = obj.key() else { continue };
            if k.ends_with(s4_codec::index::SIDECAR_SUFFIX)
                || is_dict_key(k)
                || k.contains(".__s4ver__/")
            {
                continue;
            }
            let size = obj.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0);
            if size == 0 {
                continue;
            }
            if size > params.sample_max_bytes {
                skipped_too_large += 1;
                continue;
            }
            candidates.push((k.to_owned(), size));
            if candidates.len() >= params.max_samples {
                break 'pages;
            }
        }
        match resp.is_truncated().unwrap_or(false) {
            true => {
                continuation = resp.next_continuation_token().map(str::to_owned);
                if continuation.is_none() {
                    break;
                }
            }
            false => break,
        }
    }

    // -- 2. GET each candidate (full body) -------------------------------
    let mut samples: Vec<Vec<u8>> = Vec::with_capacity(candidates.len());
    let mut sampled_bytes = 0u64;
    let mut skipped_already_s4 = 0usize;
    for (key, _size) in &candidates {
        let resp = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| backend_err("GetObject", key)(format!("{e}")))?;
        // Objects the gateway already compressed carry `s4-codec`
        // metadata and/or the S4F2 magic — exclude either signal.
        let already_s4_meta = resp
            .metadata()
            .map(|m| m.contains_key("s4-codec"))
            .unwrap_or(false);
        let body = resp
            .body
            .collect()
            .await
            .map_err(|e| backend_err("GetObject(body)", key)(format!("{e}")))?
            .into_bytes();
        if already_s4_meta || looks_like_s4_body(&body) {
            skipped_already_s4 += 1;
            continue;
        }
        if body.is_empty() {
            continue;
        }
        sampled_bytes += body.len() as u64;
        samples.push(body.to_vec());
    }

    if samples.len() < params.min_samples {
        return Err(TrainDictError::NotEnoughSamples {
            bucket: bucket.to_owned(),
            prefix: params.prefix.clone(),
            found: samples.len(),
            min: params.min_samples,
        });
    }

    // -- 3. train ---------------------------------------------------------
    let dict = s4_codec::cpu_zstd_dict::train_from_samples(&samples, params.max_dict_bytes)
        .map_err(|e| TrainDictError::Training(e.to_string()))?;
    let dict_id = dict_id_of(&dict);
    let dict_key = dict_object_key(&dict_id);

    // -- 4. PUT `.s4dict/<id>` (idempotent; immutable on conflict) --------
    let existing = client
        .get_object()
        .bucket(bucket)
        .key(&dict_key)
        .send()
        .await;
    let dict_already_existed = match existing {
        Ok(resp) => {
            let body = resp
                .body
                .collect()
                .await
                .map_err(|e| backend_err("GetObject(body)", &dict_key)(format!("{e}")))?
                .into_bytes();
            if body.as_ref() != dict.as_slice() {
                return Err(TrainDictError::DictObjectConflict { dict_id });
            }
            true
        }
        Err(_) => false, // treat any GET failure as "absent"; the PUT below surfaces real errors
    };
    if !dict_already_existed {
        client
            .put_object()
            .bucket(bucket)
            .key(&dict_key)
            .content_type("application/x-zstd-dictionary")
            .body(aws_sdk_s3::primitives::ByteStream::from(dict.clone()))
            .send()
            .await
            .map_err(|e| backend_err("PutObject", &dict_key)(format!("{e}")))?;
    }

    let gateway_flag = format!("--zstd-dict '{}/{}={}'", bucket, params.prefix, dict_id);
    Ok(TrainDictReport {
        bucket: bucket.to_owned(),
        prefix: params.prefix.clone(),
        sampled_objects: samples.len(),
        sampled_bytes,
        skipped_too_large,
        skipped_already_s4,
        dict_id,
        dict_bytes: dict.len(),
        dict_already_existed,
        gateway_flag,
    })
}

/// `<bucket>` or `<bucket>/<prefix>` splitter for the train-dict CLI
/// (same rule as `estimate` / `migrate`: slashes after the first belong
/// to the prefix).
pub fn parse_bucket_prefix(target: &str) -> Result<(String, String), String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("empty target (expected <bucket> or <bucket>/<prefix>)".into());
    }
    match target.split_once('/') {
        None => Ok((target.to_owned(), String::new())),
        Some((bucket, prefix)) => {
            if bucket.is_empty() {
                return Err(format!("empty bucket in {target:?}"));
            }
            Ok((bucket.to_owned(), prefix.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_dict(seed: u8) -> Vec<u8> {
        (0..64u8)
            .map(|i| i.wrapping_mul(7).wrapping_add(seed))
            .collect()
    }

    fn store_with(entries: &[(&str, Vec<u8>)]) -> DictStore {
        // entries: (prefix, dict bytes)
        let mut cfg = Vec::new();
        let mut dicts = HashMap::new();
        for (prefix, bytes) in entries {
            let id = dict_id_of(bytes);
            cfg.push(parse_zstd_dict_flag(&format!("{prefix}={id}")).expect("parse"));
            dicts.insert(id, bytes.clone());
        }
        DictStore::new(cfg, dicts, DEFAULT_DICT_MAX_OBJECT_BYTES, 3).expect("store")
    }

    #[test]
    fn dict_id_is_16_lower_hex() {
        let id = dict_id_of(b"hello dictionary");
        assert_eq!(id.len(), 16);
        assert!(is_valid_dict_id(&id), "{id}");
    }

    #[test]
    fn dict_id_validation_rejects_bad_shapes() {
        assert!(!is_valid_dict_id(""));
        assert!(!is_valid_dict_id("0123456789abcde")); // 15 chars
        assert!(!is_valid_dict_id("0123456789abcdef0")); // 17 chars
        assert!(!is_valid_dict_id("0123456789ABCDEF")); // uppercase
        assert!(!is_valid_dict_id("0123456789abcdeg")); // non-hex
        assert!(!is_valid_dict_id("../../../../etc/p")); // traversal shape
        assert!(is_valid_dict_id("0123456789abcdef"));
    }

    #[test]
    fn flag_parse_roundtrip() {
        let e = parse_zstd_dict_flag("mybucket/logs/app1/=0123456789abcdef").expect("parse");
        assert_eq!(e.bucket, "mybucket");
        assert_eq!(e.prefix, "mybucket/logs/app1/");
        assert_eq!(e.dict_id, "0123456789abcdef");
        // key prefixes may contain '=' — split happens at the LAST '='.
        let e2 = parse_zstd_dict_flag("b/k=v/=0123456789abcdef").expect("parse");
        assert_eq!(e2.prefix, "b/k=v/");
    }

    #[test]
    fn flag_parse_rejects_bad_inputs() {
        assert!(parse_zstd_dict_flag("no-equals-here").is_err());
        assert!(parse_zstd_dict_flag("bucket/p=BADID").is_err());
        assert!(parse_zstd_dict_flag("nobucketslash=0123456789abcdef").is_err());
        assert!(parse_zstd_dict_flag("/empty-bucket=0123456789abcdef").is_err());
    }

    #[test]
    fn lookup_longest_prefix_wins() {
        let d_short = fake_dict(1);
        let d_long = fake_dict(2);
        let store = store_with(&[
            ("bkt/logs/", d_short.clone()),
            ("bkt/logs/app1/", d_long.clone()),
        ]);
        let (id, _) = store
            .lookup("bkt", "logs/app1/2026-06-10.json")
            .expect("hit");
        assert_eq!(id, dict_id_of(&d_long), "longest prefix must win");
        let (id2, _) = store.lookup("bkt", "logs/app2/x.json").expect("hit");
        assert_eq!(id2, dict_id_of(&d_short));
        assert!(store.lookup("bkt", "images/cat.png").is_none());
        assert!(
            store.lookup("other-bucket", "logs/app1/x.json").is_none(),
            "bucket must participate in the match"
        );
        assert!(
            store.lookup("bk", "t/logs/app1/x.json").is_none(),
            "bucket name must match exactly, not as a substring"
        );
    }

    #[test]
    fn store_rejects_fingerprint_mismatch() {
        let bytes = fake_dict(3);
        let wrong_id = "0123456789abcdef".to_owned();
        let cfg = vec![parse_zstd_dict_flag(&format!("b/p={wrong_id}")).expect("parse")];
        let mut dicts = HashMap::new();
        dicts.insert(wrong_id, bytes);
        let err =
            DictStore::new(cfg, dicts, DEFAULT_DICT_MAX_OBJECT_BYTES, 3).expect_err("must reject");
        assert!(err.contains("fingerprint mismatch"), "{err}");
    }

    #[test]
    fn dict_wins_requires_strict_improvement() {
        assert!(dict_wins(99, 100));
        assert!(!dict_wins(100, 100), "tie goes to plain zstd");
        assert!(!dict_wins(101, 100));
    }

    #[test]
    fn cache_lru_evicts_oldest() {
        let cache = DictCache::default();
        let dict: Arc<[u8]> = Arc::from(fake_dict(0).into_boxed_slice());
        for i in 0..(DICT_CACHE_CAPACITY + 4) {
            cache.insert(format!("{i:016x}"), Arc::clone(&dict));
        }
        assert_eq!(cache.len(), DICT_CACHE_CAPACITY);
        assert!(
            cache.get(&format!("{:016x}", 0)).is_none(),
            "oldest evicted"
        );
        assert!(
            cache
                .get(&format!("{:016x}", DICT_CACHE_CAPACITY + 3))
                .is_some()
        );
        // Touching an entry protects it from the next eviction.
        let survivor = format!("{:016x}", 4);
        assert!(cache.get(&survivor).is_some());
        cache.insert(format!("{:016x}", 999), Arc::clone(&dict));
        assert!(cache.get(&survivor).is_some(), "recently-used must survive");
    }

    #[test]
    fn dict_key_helpers() {
        assert_eq!(
            dict_object_key("0123456789abcdef"),
            ".s4dict/0123456789abcdef"
        );
        assert!(is_dict_key(".s4dict/0123456789abcdef"));
        assert!(
            !is_dict_key("data/.s4dict/x"),
            "only bucket-root .s4dict/ is reserved"
        );
        assert!(!is_dict_key("regular/key.json"));
    }

    #[test]
    fn parse_bucket_prefix_shapes() {
        assert_eq!(
            parse_bucket_prefix("bkt").expect("ok"),
            ("bkt".into(), String::new())
        );
        assert_eq!(
            parse_bucket_prefix("bkt/logs/app1/").expect("ok"),
            ("bkt".into(), "logs/app1/".into())
        );
        assert!(parse_bucket_prefix("").is_err());
        assert!(parse_bucket_prefix("/x").is_err());
    }
}
