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
//!
//! ## Operations (v1.3 dict ops)
//!
//! - **Per-prefix metrics**: the PUT-side dict branch records
//!   `s4_dict_put_total{prefix,outcome}` and
//!   `s4_dict_put_bytes_total{prefix,kind}` (both compression results are
//!   measured per PUT anyway, so the loss side costs nothing extra);
//!   `s4 dict-status --metrics-url <URL>` scrapes them and flags
//!   stale-looking dictionaries (win rate below `--warn-win-rate`).
//! - **Restart-less rotation** (`--zstd-dict-map <FILE>` + `SIGHUP`):
//!   a TOML `[mappings]` table carries the same prefix→dict-id mappings
//!   as repeated `--zstd-dict` flags; on `SIGHUP` the file is re-read,
//!   new dictionaries are fetched + verified, and the gateway's
//!   [`SharedDictStore`] is swapped atomically. A failed reload keeps
//!   the current store (fail-safe — never a half-applied swap).
//! - **Multipart is out of scope by design**: multipart parts only ever
//!   take the streaming per-part frame path (the dict store is consulted
//!   exclusively by single-object PUT), and S3's 5 MiB minimum part size
//!   ([`s4_codec::multipart::S3_MULTIPART_MIN_PART_BYTES`]) sits far
//!   above the small-object ceiling (`--zstd-dict-max-bytes`, default
//!   1 MiB) the feature targets — the two size ranges never intersect.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aws_sdk_s3::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Reserved in-bucket prefix where trained dictionaries live.
pub const DICT_KEY_PREFIX: &str = ".s4dict/";

/// Hex chars of the SHA-256 prefix used as the dictionary id.
pub const DICT_ID_HEX_LEN: usize = 16;

/// LRU capacity of the lazy GET-side dictionary cache. Trained
/// dictionaries default to ≤ ~110 KiB ([`DEFAULT_MAX_DICT_BYTES`]), so 16
/// slots hold ~2 MiB in practice; the lazy fetch additionally hard-caps a
/// single `.s4dict/` object at [`DICT_FETCH_MAX_BYTES`] (1 MiB), bounding
/// the cache at 16 MiB even against oversized backend blobs.
pub const DICT_CACHE_CAPACITY: usize = 16;

/// **Single source of truth for the dictionary size contract** (1 MiB).
///
/// Every component that handles dictionary bytes enforces this same cap,
/// so a dictionary that exists at all is readable everywhere:
///
/// - `train-dict` refuses `--max-dict-bytes` above the cap
///   ([`TrainDictError::MaxDictBytesOverCap`]) — an over-cap dictionary
///   could be written but never read back by a flag-less gateway;
/// - boot preload ([`DictStore::new`], `--zstd-dict`) refuses an over-cap
///   `.s4dict/<id>` object with a typed boot error;
/// - the GET-side lazy fetch (`resolve_dict`) hard-caps the `.s4dict/`
///   body it will collect, bounding the worst-case lazy-LRU footprint at
///   `16 slots x 1 MiB = 16 MiB` even against hostile oversized blobs.
///
/// Trained dictionaries default to ≤ ~110 KiB ([`DEFAULT_MAX_DICT_BYTES`]);
/// 1 MiB leaves ~9x headroom for operators who train larger.
pub const DICT_FETCH_MAX_BYTES: usize = 1024 * 1024;

/// Backend metadata key carrying the **full** SHA-256 hex (64 chars) of a
/// `.s4dict/<id>` object's bytes. Stamped by `train-dict` (and by the
/// gateway's cross-bucket CopyObject dict propagation) so the GET-side
/// lazy fetch can verify the full digest instead of only the 16-hex
/// (64-bit) id prefix. Absent on dictionaries trained before v1.0.1 —
/// the fetch path then falls back to the prefix check (back-compat).
pub const DICT_SHA256_META_KEY: &str = "s4-dict-sha256";

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

/// Full SHA-256 of a dictionary's raw bytes as 64 lowercase hex chars
/// (the value stored under [`DICT_SHA256_META_KEY`]).
pub fn dict_sha256_hex(dict_bytes: &[u8]) -> String {
    let digest = Sha256::digest(dict_bytes);
    let mut s = String::with_capacity(64);
    for byte in digest.iter() {
        use std::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// `<dict-id>` for a dictionary's raw bytes: first 16 hex chars of its
/// SHA-256 (lowercase).
pub fn dict_id_of(dict_bytes: &[u8]) -> String {
    let mut s = dict_sha256_hex(dict_bytes);
    s.truncate(DICT_ID_HEX_LEN);
    s
}

/// Shared fingerprint discipline for dictionary bytes — used by the boot
/// preload ([`DictStore::new`]) and the gateway's GET-side lazy fetch:
///
/// 1. the bytes' SHA-256 must start with the 16-hex content-addressed
///    `dict_id` (always checked);
/// 2. when the `.s4dict/<id>` object carries the full-SHA-256 metadata
///    stamp ([`DICT_SHA256_META_KEY`]), the complete digest must match
///    too (closes the 64-bit truncation window; dictionaries trained
///    before v1.0.1 lack the stamp and keep the prefix-only check).
pub fn verify_dict_bytes(
    dict_id: &str,
    claimed_sha256: Option<&str>,
    bytes: &[u8],
) -> Result<(), String> {
    let actual_sha256 = dict_sha256_hex(bytes);
    let actual_id = &actual_sha256[..DICT_ID_HEX_LEN];
    if actual_id != dict_id {
        return Err(format!(
            "fingerprint mismatch for id {dict_id}: bytes hash to {actual_id} \
             (corrupted / tampered `.s4dict/{dict_id}`?)"
        ));
    }
    if let Some(claimed) = claimed_sha256
        && claimed != actual_sha256
    {
        return Err(format!(
            "full SHA-256 mismatch for id {dict_id}: bytes hash to {actual_sha256} but \
             `{DICT_SHA256_META_KEY}` metadata claims {claimed} (corrupted / tampered \
             `.s4dict/{dict_id}`?)"
        ));
    }
    Ok(())
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
    entry_from_parts(prefix, dict_id)
}

/// Shared validation for one `<bucket>/<key-prefix>` → `<dict-id>` pair —
/// used by both the `--zstd-dict` flag parser and the `--zstd-dict-map`
/// TOML loader so the two configuration surfaces enforce identical rules.
fn entry_from_parts(prefix: &str, dict_id: &str) -> Result<DictConfigEntry, String> {
    if !is_valid_dict_id(dict_id) {
        return Err(format!(
            "dict-id must be exactly {DICT_ID_HEX_LEN} lowercase hex chars, got {dict_id:?}"
        ));
    }
    let (bucket, _key_prefix) = prefix
        .split_once('/')
        .ok_or_else(|| format!("prefix must be '<bucket>/<key-prefix>', got {prefix:?}"))?;
    if bucket.is_empty() {
        return Err(format!("empty bucket in {prefix:?}"));
    }
    Ok(DictConfigEntry {
        prefix: prefix.to_owned(),
        bucket: bucket.to_owned(),
        dict_id: dict_id.to_owned(),
    })
}

/// Serde shape of a `--zstd-dict-map <FILE>`:
///
/// ```toml
/// [mappings]
/// "mybucket/events/" = "0123456789abcdef"
/// "mybucket/logs/app1/" = "fedcba9876543210"
/// ```
///
/// Unknown top-level keys are rejected (a typo'd table name must not
/// silently configure nothing).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DictMapFile {
    /// `"<bucket>/<key-prefix>" = "<dict-id>"` pairs. `BTreeMap` for a
    /// deterministic entry order (error messages, boot logging). TOML
    /// itself rejects duplicate keys, so duplicate prefixes *within* the
    /// file fail at parse time.
    #[serde(default)]
    mappings: std::collections::BTreeMap<String, String>,
}

/// Parse the contents of a `--zstd-dict-map` TOML file into the same
/// [`DictConfigEntry`] list the repeated `--zstd-dict` flag produces.
/// Validation is identical to the flag parser (16-hex id, non-empty
/// bucket); duplicate prefixes are a TOML parse error by construction.
pub fn parse_zstd_dict_map(content: &str) -> Result<Vec<DictConfigEntry>, String> {
    let parsed: DictMapFile = toml::from_str(content).map_err(|e| format!("invalid TOML: {e}"))?;
    let mut out = Vec::with_capacity(parsed.mappings.len());
    for (prefix, dict_id) in &parsed.mappings {
        out.push(
            entry_from_parts(prefix, dict_id).map_err(|e| format!("mapping {prefix:?}: {e}"))?,
        );
    }
    Ok(out)
}

/// Merge `--zstd-dict` flag entries with `--zstd-dict-map` file entries.
/// A prefix configured through **both** sources is an error — even with
/// the same dict-id — because a SIGHUP reload would otherwise leave it
/// ambiguous which source owns the mapping going forward.
pub fn merge_dict_entries(
    flag_entries: Vec<DictConfigEntry>,
    map_entries: Vec<DictConfigEntry>,
) -> Result<Vec<DictConfigEntry>, String> {
    let mut merged = flag_entries;
    for e in map_entries {
        if merged.iter().any(|f| f.prefix == e.prefix) {
            return Err(format!(
                "prefix {:?} is configured in both --zstd-dict and --zstd-dict-map — \
                 keep it in exactly one place (the map file is the SIGHUP-reloadable \
                 source; prefer it for mappings that rotate)",
                e.prefix
            ));
        }
        merged.push(e);
    }
    Ok(merged)
}

/// v1.3 dict ops: reload-capable holder for the gateway's [`DictStore`].
///
/// The service reads the current generation with [`SharedDictStore::load`]
/// once per PUT / dict-GET; the `--zstd-dict-map` SIGHUP handler in
/// `main.rs` builds a **complete** replacement store off to the side and
/// installs it with [`SharedDictStore::swap`] (RCU via `arc-swap` — no
/// lock on the request path, in-flight requests keep the generation they
/// loaded). A failed reload never calls `swap`, so the gateway always
/// runs either the old config or the new one, never a half-applied mix.
#[derive(Debug, Default)]
pub struct SharedDictStore {
    inner: arc_swap::ArcSwapOption<DictStore>,
}

impl SharedDictStore {
    /// Wrap an initial store (`None` = dict feature off; every PUT stays
    /// on the pre-dict path until a `swap` installs one).
    pub fn new(store: Option<Arc<DictStore>>) -> Self {
        Self {
            inner: arc_swap::ArcSwapOption::new(store),
        }
    }

    /// Current store generation. Cheap (lock-free read) — called once
    /// per request that might touch the dict path.
    pub fn load(&self) -> Option<Arc<DictStore>> {
        self.inner.load_full()
    }

    /// Atomically install a new store generation (SIGHUP reload success
    /// path). Readers that already `load`ed keep the old `Arc` until
    /// they drop it.
    pub fn swap(&self, store: Arc<DictStore>) {
        self.inner.store(Some(store));
    }
}

/// Rolling-window length for the gateway-side per-prefix win-rate
/// monitor: the last 100 dict-path PUT decisions per prefix.
pub const DICT_WIN_RATE_WINDOW: usize = 100;

/// A full window whose win rate falls below this fraction triggers the
/// stale-dictionary WARN log (and the matching `s4 dict-status` default).
pub const DICT_WIN_RATE_WARN_THRESHOLD: f64 = 0.5;

/// Per-prefix WARN rate limit — at most one stale-dictionary log line
/// per prefix per hour, however many PUTs flow.
pub const DICT_WIN_RATE_WARN_INTERVAL: Duration = Duration::from_secs(3600);

/// v1.3 dict ops: gateway-side rolling win-rate monitor. The PUT-side
/// dict branch feeds every win/loss decision in; when a prefix's last
/// [`DICT_WIN_RATE_WINDOW`] decisions drop below
/// [`DICT_WIN_RATE_WARN_THRESHOLD`], [`DictWinTracker::record`] returns
/// the rate so the caller can emit a WARN (rate-limited per prefix by
/// [`DICT_WIN_RATE_WARN_INTERVAL`]). Judgement only starts on a **full**
/// window — a cold prefix's first few losses must not page anyone.
///
/// **Rotation caveat**: the window is NOT cleared on a SIGHUP dict-map
/// reload — the tracker is owned privately by the service while the
/// reload handler only swaps the [`SharedDictStore`], so the first
/// post-rotation WARN judgement can still include up to
/// [`DICT_WIN_RATE_WINDOW`] pre-rotation decisions. Worst case that is
/// one spurious WARN (rate-limited to one per prefix per hour) right
/// after a rotation that actually fixed the dictionary; the window
/// flushes itself after [`DICT_WIN_RATE_WINDOW`] new PUTs.
#[derive(Debug)]
pub struct DictWinTracker {
    window: usize,
    inner: Mutex<HashMap<String, PrefixWinWindow>>,
}

#[derive(Debug)]
struct PrefixWinWindow {
    /// Most-recent decision at the back.
    recent: VecDeque<bool>,
    /// Count of `true` entries in `recent` (kept in sync incrementally).
    wins: usize,
    last_warn: Option<Instant>,
}

impl Default for DictWinTracker {
    fn default() -> Self {
        Self::with_window(DICT_WIN_RATE_WINDOW)
    }
}

impl DictWinTracker {
    /// Window-size override for tests; production uses
    /// [`DICT_WIN_RATE_WINDOW`] via `Default`.
    pub fn with_window(window: usize) -> Self {
        Self {
            window: window.max(1),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Record one dict-vs-plain PUT decision for `prefix`. Returns
    /// `Some(win_rate)` exactly when the caller should WARN: the window
    /// is full, the rate is below [`DICT_WIN_RATE_WARN_THRESHOLD`], and
    /// the per-prefix rate limit has elapsed.
    pub fn record(&self, prefix: &str, win: bool) -> Option<f64> {
        self.record_at(prefix, win, Instant::now())
    }

    fn record_at(&self, prefix: &str, win: bool, now: Instant) -> Option<f64> {
        // Poisoned lock: degrade to monitoring-off (the dict path itself
        // is unaffected; same posture as DictCache).
        let mut inner = self.inner.lock().ok()?;
        let w = inner
            .entry(prefix.to_owned())
            .or_insert_with(|| PrefixWinWindow {
                recent: VecDeque::with_capacity(self.window),
                wins: 0,
                last_warn: None,
            });
        if w.recent.len() >= self.window
            && let Some(oldest) = w.recent.pop_front()
            && oldest
        {
            w.wins = w.wins.saturating_sub(1);
        }
        w.recent.push_back(win);
        if win {
            w.wins += 1;
        }
        if w.recent.len() < self.window {
            return None;
        }
        let rate = w.wins as f64 / w.recent.len() as f64;
        if rate >= DICT_WIN_RATE_WARN_THRESHOLD {
            return None;
        }
        let due = match w.last_warn {
            None => true,
            Some(t) => now.duration_since(t) >= DICT_WIN_RATE_WARN_INTERVAL,
        };
        if due {
            w.last_warn = Some(now);
            Some(rate)
        } else {
            None
        }
    }
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
    /// `(bucket, dict-id)` → dictionary bytes (fingerprint-verified at
    /// load). Bucket-scoped for the same containment reason as
    /// [`DictCache`] (v1.0.1 audit R2 P3): the id is only a 64-bit
    /// SHA-256 prefix, so an id-keyed map would let one bucket's entry
    /// satisfy preload lookups for *every* bucket on the gateway. The
    /// `--zstd-dict` flag shape (`<bucket>/<prefix>=<id>`) makes the
    /// bucket known at boot.
    dicts: HashMap<(String, String), Arc<[u8]>>,
    /// PUT-side body-size ceiling for the dict path (`--zstd-dict-max-bytes`).
    max_object_bytes: usize,
    /// zstd level used for the dict compressor (server `--zstd-level`).
    level: i32,
}

impl DictStore {
    /// Assemble a store from parsed entries + already-fetched dict bytes.
    /// Verifies every dictionary's fingerprint against its configured id
    /// (shared [`verify_dict_bytes`] discipline), enforces the
    /// [`DICT_FETCH_MAX_BYTES`] size contract (an over-cap dictionary
    /// would be writable at boot but unreadable by a flag-less gateway's
    /// lazy fetch — fail loudly at boot instead), and rejects
    /// duplicates-with-different-ids for the same prefix.
    pub fn new(
        entries: Vec<DictConfigEntry>,
        dict_bytes: HashMap<String, Vec<u8>>,
        max_object_bytes: usize,
        level: i32,
    ) -> Result<Self, String> {
        let mut verified: HashMap<String, Arc<[u8]>> = HashMap::new();
        for (id, bytes) in dict_bytes {
            // v1.0.1 audit R2 P3: same 1 MiB cap the lazy fetch enforces.
            // Accepting a bigger dictionary here would mint objects that
            // become unreadable the moment the operator drops the
            // `--zstd-dict` flag (lazy fetch refuses the oversized blob).
            if bytes.len() > DICT_FETCH_MAX_BYTES {
                return Err(format!(
                    "dictionary {id} is {} bytes, over the {DICT_FETCH_MAX_BYTES}-byte \
                     dictionary cap — a flag-less gateway's lazy `.s4dict/` fetch would \
                     refuse it; retrain with `--max-dict-bytes` ≤ {DICT_FETCH_MAX_BYTES}",
                    bytes.len()
                ));
            }
            verify_dict_bytes(&id, None, &bytes).map_err(|e| format!("dictionary {e}"))?;
            if bytes.is_empty() {
                return Err(format!("dictionary {id} is empty"));
            }
            verified.insert(id, Arc::from(bytes.into_boxed_slice()));
        }
        let mut dicts: HashMap<(String, String), Arc<[u8]>> = HashMap::new();
        let mut prefixes: Vec<(String, String)> = Vec::with_capacity(entries.len());
        for e in entries {
            let Some(bytes) = verified.get(&e.dict_id) else {
                return Err(format!(
                    "no dictionary bytes loaded for id {} (prefix {:?})",
                    e.dict_id, e.prefix
                ));
            };
            // v1.0.1 audit R2 P3: bucket-scoped key — see the field doc.
            dicts.insert((e.bucket.clone(), e.dict_id.clone()), Arc::clone(bytes));
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
        self.lookup_with_prefix(bucket, key)
            .map(|(_prefix, dict_id, dict)| (dict_id, dict))
    }

    /// [`Self::lookup`] that also returns the **matched configured
    /// prefix** (the combined `<bucket>/<key-prefix>` form) — the label
    /// the per-prefix dict metrics are keyed by, so a PUT's win/loss is
    /// attributed to the mapping that routed it (v1.3 dict ops).
    pub fn lookup_with_prefix(
        &self,
        bucket: &str,
        key: &str,
    ) -> Option<(String, String, Arc<[u8]>)> {
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
                // The entry's bucket is exactly the `bucket` argument
                // (it matched above), so the bucket-scoped key is
                // `(bucket, dict_id)`.
                let dict = self.dicts.get(&(bucket.to_owned(), dict_id.clone()))?;
                return Some((prefix.clone(), dict_id.clone(), Arc::clone(dict)));
            }
        }
        None
    }

    /// Preloaded dictionary bytes by `(bucket, id)` (GET-side fast path
    /// before any LRU / backend fetch). Bucket-scoped since v1.0.1 audit
    /// R2 P3 — a dictionary preloaded for one bucket's prefix must not
    /// satisfy GETs against a different bucket (the 16-hex id is only a
    /// 64-bit SHA-256 prefix; see the `dicts` field doc).
    pub fn get_preloaded(&self, bucket: &str, dict_id: &str) -> Option<Arc<[u8]>> {
        self.dicts
            .get(&(bucket.to_owned(), dict_id.to_owned()))
            .map(Arc::clone)
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
/// fingerprint, and inserts here.
///
/// Keyed by `(bucket, dict-id)` — NOT by id alone. Ids are
/// content-addressed, so for honestly-trained dictionaries equal id ⇒
/// equal bytes regardless of bucket; but the id is only a 64-bit SHA-256
/// prefix, and a single id-keyed cache would let a `.s4dict/<id>` object
/// planted in one bucket (a prefix-collision forgery the 16-hex check
/// alone cannot rule out) satisfy GETs against *every other* bucket on
/// the gateway. Bucket-scoping the key confines any such poisoning to
/// the bucket that already contains the hostile object.
#[derive(Debug, Default)]
pub struct DictCache {
    inner: Mutex<DictCacheInner>,
}

#[derive(Debug, Default)]
struct DictCacheInner {
    map: HashMap<(String, String), Arc<[u8]>>,
    /// Recency queue, most-recent at the back.
    order: Vec<(String, String)>,
}

impl DictCache {
    pub fn get(&self, bucket: &str, dict_id: &str) -> Option<Arc<[u8]>> {
        let mut inner = self.inner.lock().ok()?;
        let hit = inner
            .map
            .get(&(bucket.to_owned(), dict_id.to_owned()))
            .map(Arc::clone)?;
        if let Some(pos) = inner
            .order
            .iter()
            .position(|(b, k)| b == bucket && k == dict_id)
        {
            let k = inner.order.remove(pos);
            inner.order.push(k);
        }
        Some(hit)
    }

    pub fn insert(&self, bucket: String, dict_id: String, dict: Arc<[u8]>) {
        let Ok(mut inner) = self.inner.lock() else {
            return; // poisoned lock: degrade to cache-off (refetch next GET)
        };
        let cache_key = (bucket, dict_id);
        if inner.map.contains_key(&cache_key) {
            if let Some(pos) = inner.order.iter().position(|k| *k == cache_key) {
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
        inner.map.insert(cache_key.clone(), dict);
        inner.order.push(cache_key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|i| i.map.len()).unwrap_or(0)
    }
}

// ===========================================================================
// `s4 dict-status` — /metrics scrape → per-prefix dictionary health
// ===========================================================================

/// Default `s4 dict-status --warn-win-rate` (mirrors the gateway-side
/// [`DICT_WIN_RATE_WARN_THRESHOLD`]).
pub const DEFAULT_DICT_STATUS_WARN_WIN_RATE: f64 = 0.5;

/// One parsed Prometheus text-format sample:
/// `(metric_name, labels, value)`.
pub type PromSample = (String, Vec<(String, String)>, f64);

/// Parse one Prometheus **text-format sample line** into
/// `(metric_name, labels, value)`. Comment (`# …`) and blank lines, and
/// anything that doesn't parse, return `None`.
///
/// Deliberately minimal — NOT a general OpenMetrics parser. It handles
/// exactly the line shape `metrics-exporter-prometheus` renders for the
/// counters `dict-status` consumes (`name{l1="v1",l2="v2"} value` /
/// `name value`), including the three escape sequences the text format
/// defines for label values (`\\`, `\"`, `\n`). Kept dependency-free on
/// purpose; unit-tested against a fixture captured from the real
/// recorder's `render()` output.
pub fn parse_prom_sample(line: &str) -> Option<PromSample> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] != b'{' && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let name = &line[..i];
    if name.is_empty() {
        return None;
    }
    let mut labels: Vec<(String, String)> = Vec::new();
    if i < bytes.len() && bytes[i] == b'{' {
        i += 1;
        loop {
            while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
                i += 1;
            }
            if i >= bytes.len() {
                return None; // unterminated label set
            }
            if bytes[i] == b'}' {
                i += 1;
                break;
            }
            let lname_start = i;
            while i < bytes.len() && bytes[i] != b'=' {
                i += 1;
            }
            if i >= bytes.len() {
                return None;
            }
            let lname = line[lname_start..i].trim().to_owned();
            i += 1; // consume '='
            if i >= bytes.len() || bytes[i] != b'"' {
                return None;
            }
            i += 1; // consume opening '"'
            let mut value = String::new();
            loop {
                if i >= bytes.len() {
                    return None; // unterminated label value
                }
                match bytes[i] {
                    b'\\' => {
                        i += 1;
                        // Read the escaped character as a whole char, not a
                        // byte: `\` followed by a multi-byte UTF-8 char (a
                        // malformed-but-possible `\é`) would otherwise leave
                        // `i` mid-codepoint and panic on the next
                        // `line[i..]` slice (v1.2 audit R1 P3). `\` is
                        // ASCII, so `i` is a char boundary here; an empty
                        // remainder (dangling trailing `\`) yields `None`.
                        let ch = line[i..].chars().next()?;
                        match ch {
                            // The three escapes the Prometheus text format
                            // defines for label values.
                            '\\' => value.push('\\'),
                            '"' => value.push('"'),
                            'n' => value.push('\n'),
                            // Anything else is not an escape — copy raw.
                            other => {
                                value.push('\\');
                                value.push(other);
                            }
                        }
                        i += ch.len_utf8();
                    }
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => {
                        // Advance by whole chars so multi-byte UTF-8 label
                        // values (S3 keys are arbitrary Unicode) survive.
                        let ch = line[i..].chars().next()?;
                        value.push(ch);
                        i += ch.len_utf8();
                    }
                }
            }
            labels.push((lname, value));
        }
    }
    let value: f64 = line[i..].split_ascii_whitespace().next()?.parse().ok()?;
    Some((name.to_owned(), labels, value))
}

fn label_value<'a>(labels: &'a [(String, String)], name: &str) -> Option<&'a str> {
    labels
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Per-prefix slice of an `s4 dict-status` report.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DictPrefixStatus {
    /// Configured `<bucket>/<key-prefix>` the metrics are labelled with.
    pub prefix: String,
    /// `s4_dict_put_total{outcome="win"}`.
    pub wins: u64,
    /// `s4_dict_put_total{outcome="loss"}`.
    pub losses: u64,
    /// `wins / (wins + losses)`; `0.0` when no decisions were recorded.
    pub win_rate: f64,
    /// `s4_dict_put_bytes_total{kind="original"}` — logical input bytes.
    pub original_bytes: u64,
    /// `s4_dict_put_bytes_total{kind="dict"}` — dict-compressed payload
    /// bytes (measured on every PUT, win or loss).
    pub dict_bytes: u64,
    /// `s4_dict_put_bytes_total{kind="plain"}` — plain cpu-zstd payload
    /// bytes for the same inputs (the per-PUT comparison baseline).
    pub plain_bytes: u64,
    /// Effective dict compression ratio: `dict_bytes / original_bytes`
    /// (lower is better); `0.0` when no bytes were recorded.
    pub dict_ratio: f64,
    /// `true` when this prefix tripped the `--warn-win-rate` threshold.
    pub stale: bool,
}

/// `s4 dict-status` report — built purely from one `/metrics` scrape.
#[derive(Debug, Clone, Serialize)]
pub struct DictStatusReport {
    /// Per-prefix health, sorted by prefix.
    pub prefixes: Vec<DictPrefixStatus>,
    /// `s4_dict_fetch_total{result="ok"}` — lazy GET-side fetches.
    pub dict_fetch_ok: u64,
    /// `s4_dict_fetch_total{result="err"}` — any non-zero value means
    /// dict-compressed objects failed GETs.
    pub dict_fetch_err: u64,
    /// The threshold the `stale` flags were judged against.
    pub warn_win_rate: f64,
    /// One human-readable line per stale prefix. Non-empty ⇒ the CLI
    /// exits 1 (cron-able).
    pub warnings: Vec<String>,
}

/// Build a [`DictStatusReport`] from raw Prometheus text. Lines other
/// than the three dict metric families are ignored. A prefix is flagged
/// stale when it has at least one recorded decision and its win rate is
/// strictly below `warn_win_rate`.
///
/// **Lifetime-counter caveat** (v1.2 audit R1 P3): the inputs are
/// monotonic Prometheus counters, so every number here is cumulative
/// **since gateway start** — not a rolling window like the gateway's
/// own [`DictWinTracker`] WARN log. Two consequences operators must
/// know:
///
/// - After rotating a dictionary (SIGHUP map reload), the historical
///   losses stay in the counters; a prefix flagged STALE keeps tripping
///   the threshold until enough post-rotation wins accumulate to pull
///   the lifetime rate back over `warn_win_rate` (or the gateway is
///   restarted, which resets the counters).
/// - A prefix **removed** from the map keeps its already-recorded
///   series in `/metrics` until the gateway restarts — it still shows
///   up (and can still read STALE) here even though no new PUT will
///   ever match it.
pub fn build_dict_status(metrics_text: &str, warn_win_rate: f64) -> DictStatusReport {
    let mut acc: std::collections::BTreeMap<String, DictPrefixStatus> =
        std::collections::BTreeMap::new();
    let mut dict_fetch_ok = 0u64;
    let mut dict_fetch_err = 0u64;
    for line in metrics_text.lines() {
        let Some((name, labels, value)) = parse_prom_sample(line) else {
            continue;
        };
        // Counters render as non-negative integers; clamp defensively.
        let v = if value.is_finite() && value > 0.0 {
            value as u64
        } else {
            0
        };
        match name.as_str() {
            n if n == crate::metrics::names::DICT_PUT_TOTAL => {
                let Some(prefix) = label_value(&labels, "prefix") else {
                    continue;
                };
                let e = acc.entry(prefix.to_owned()).or_default();
                e.prefix = prefix.to_owned();
                match label_value(&labels, "outcome") {
                    Some("win") => e.wins += v,
                    Some("loss") => e.losses += v,
                    _ => {}
                }
            }
            n if n == crate::metrics::names::DICT_PUT_BYTES_TOTAL => {
                let Some(prefix) = label_value(&labels, "prefix") else {
                    continue;
                };
                let e = acc.entry(prefix.to_owned()).or_default();
                e.prefix = prefix.to_owned();
                match label_value(&labels, "kind") {
                    Some("original") => e.original_bytes += v,
                    Some("dict") => e.dict_bytes += v,
                    Some("plain") => e.plain_bytes += v,
                    _ => {}
                }
            }
            n if n == crate::metrics::names::DICT_FETCH_TOTAL => {
                match label_value(&labels, "result") {
                    Some("ok") => dict_fetch_ok += v,
                    Some("err") => dict_fetch_err += v,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let mut warnings = Vec::new();
    let mut prefixes: Vec<DictPrefixStatus> = acc.into_values().collect();
    for p in &mut prefixes {
        let decisions = p.wins + p.losses;
        if decisions > 0 {
            p.win_rate = p.wins as f64 / decisions as f64;
        }
        if p.original_bytes > 0 {
            p.dict_ratio = p.dict_bytes as f64 / p.original_bytes as f64;
        }
        if decisions > 0 && p.win_rate < warn_win_rate {
            p.stale = true;
            warnings.push(format!(
                "prefix {:?}: win rate {:.2} over {decisions} dict-path PUT(s) is below \
                 {warn_win_rate:.2} — dictionary may be stale; consider retraining \
                 (s4 train-dict)",
                p.prefix, p.win_rate
            ));
        }
    }
    DictStatusReport {
        prefixes,
        dict_fetch_ok,
        dict_fetch_err,
        warn_win_rate,
        warnings,
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
        "--max-dict-bytes {requested} exceeds the {cap}-byte dictionary cap — a dictionary \
         this large could be trained and written, but every reader enforces the same cap \
         (gateway boot preload AND the flag-less gateway's lazy `.s4dict/` fetch), so the \
         objects it compresses would become unreadable; pass --max-dict-bytes ≤ {cap}"
    )]
    MaxDictBytesOverCap { requested: usize, cap: usize },
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
    /// Dictionary output size cap. Must be ≤ [`DICT_FETCH_MAX_BYTES`]
    /// (the gateway-wide dictionary size contract) — larger values are
    /// refused with [`TrainDictError::MaxDictBytesOverCap`] before any
    /// backend traffic.
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
    // v1.0.1 audit R2 P3: enforce the single dictionary size contract
    // ([`DICT_FETCH_MAX_BYTES`]) at the *writer* — pre-fix, train-dict
    // accepted any `--max-dict-bytes`, boot preload accepted any size,
    // but the lazy fetch refused > 1 MiB, so an over-cap dictionary
    // worked only until the operator dropped the `--zstd-dict` flag.
    if params.max_dict_bytes > DICT_FETCH_MAX_BYTES {
        return Err(TrainDictError::MaxDictBytesOverCap {
            requested: params.max_dict_bytes,
            cap: DICT_FETCH_MAX_BYTES,
        });
    }
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
            // Full SHA-256 alongside the 16-hex (64-bit) id in the key:
            // lets the gateway's lazy fetch verify the complete digest
            // instead of only the truncated prefix. Dictionaries trained
            // before this stamp existed simply lack the metadata and the
            // fetch path falls back to the prefix check.
            .metadata(DICT_SHA256_META_KEY, dict_sha256_hex(&dict))
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
            cache.insert("bkt".to_owned(), format!("{i:016x}"), Arc::clone(&dict));
        }
        assert_eq!(cache.len(), DICT_CACHE_CAPACITY);
        assert!(
            cache.get("bkt", &format!("{:016x}", 0)).is_none(),
            "oldest evicted"
        );
        assert!(
            cache
                .get("bkt", &format!("{:016x}", DICT_CACHE_CAPACITY + 3))
                .is_some()
        );
        // Touching an entry protects it from the next eviction.
        let survivor = format!("{:016x}", 4);
        assert!(cache.get("bkt", &survivor).is_some());
        cache.insert("bkt".to_owned(), format!("{:016x}", 999), Arc::clone(&dict));
        assert!(
            cache.get("bkt", &survivor).is_some(),
            "recently-used must survive"
        );
    }

    /// Cache poisoning containment: an entry inserted for bucket A must
    /// never satisfy a lookup for the same dict-id under bucket B — the
    /// 16-hex id is only a 64-bit prefix, so cross-bucket sharing would
    /// let one bucket's forged `.s4dict/<id>` serve every bucket.
    #[test]
    fn cache_is_bucket_scoped() {
        let cache = DictCache::default();
        let dict_a: Arc<[u8]> = Arc::from(fake_dict(1).into_boxed_slice());
        let dict_b: Arc<[u8]> = Arc::from(fake_dict(2).into_boxed_slice());
        let id = "0123456789abcdef";
        cache.insert("bucket-a".to_owned(), id.to_owned(), Arc::clone(&dict_a));
        assert!(
            cache.get("bucket-b", id).is_none(),
            "bucket B must not see bucket A's cached dictionary"
        );
        cache.insert("bucket-b".to_owned(), id.to_owned(), Arc::clone(&dict_b));
        assert_eq!(cache.get("bucket-a", id).as_deref(), Some(dict_a.as_ref()));
        assert_eq!(cache.get("bucket-b", id).as_deref(), Some(dict_b.as_ref()));
    }

    /// v1.0.1 audit R2 P3: the preload map is `(bucket, dict-id)`-keyed.
    /// A dictionary configured for bucket A must not satisfy a preload
    /// lookup against bucket B, even for the same 16-hex id — the id is
    /// only a 64-bit SHA-256 prefix, so cross-bucket sharing would let a
    /// prefix-collision forgery in one bucket serve every other bucket.
    /// (Equal-id-different-bytes can't be fabricated in a test, so we pin
    /// the key separation itself.)
    #[test]
    fn preload_is_bucket_scoped() {
        let dict_a = fake_dict(1);
        let dict_b = fake_dict(2);
        let id_a = dict_id_of(&dict_a);
        let id_b = dict_id_of(&dict_b);
        let store = store_with(&[
            ("bucket-a/logs/", dict_a.clone()),
            ("bucket-b/logs/", dict_b.clone()),
        ]);
        assert_eq!(
            store.get_preloaded("bucket-a", &id_a).as_deref(),
            Some(dict_a.as_slice())
        );
        assert_eq!(
            store.get_preloaded("bucket-b", &id_b).as_deref(),
            Some(dict_b.as_slice())
        );
        assert!(
            store.get_preloaded("bucket-b", &id_a).is_none(),
            "bucket B must not see bucket A's preloaded dictionary"
        );
        assert!(
            store.get_preloaded("bucket-a", &id_b).is_none(),
            "bucket A must not see bucket B's preloaded dictionary"
        );
        assert!(store.get_preloaded("bucket-c", &id_a).is_none());
    }

    /// v1.0.1 audit R2 P3: boot preload enforces the same 1 MiB cap the
    /// lazy GET-side fetch does — an over-cap dictionary accepted at boot
    /// would mint objects a flag-less gateway can never read.
    #[test]
    fn store_rejects_oversized_dictionary() {
        let bytes = vec![0xabu8; DICT_FETCH_MAX_BYTES + 1];
        let id = dict_id_of(&bytes);
        let cfg = vec![parse_zstd_dict_flag(&format!("b/p={id}")).expect("parse")];
        let mut dicts = HashMap::new();
        dicts.insert(id, bytes);
        let err =
            DictStore::new(cfg, dicts, DEFAULT_DICT_MAX_OBJECT_BYTES, 3).expect_err("must reject");
        assert!(err.contains("over the"), "{err}");
        assert!(err.contains("retrain"), "{err}");
        // Exactly-at-cap is fine (boundary).
        let bytes = vec![0xcdu8; DICT_FETCH_MAX_BYTES];
        let id = dict_id_of(&bytes);
        let cfg = vec![parse_zstd_dict_flag(&format!("b/p={id}")).expect("parse")];
        let mut dicts = HashMap::new();
        dicts.insert(id, bytes);
        DictStore::new(cfg, dicts, DEFAULT_DICT_MAX_OBJECT_BYTES, 3).expect("at-cap is accepted");
    }

    /// v1.0.1 audit R2 P3: `train-dict` refuses `--max-dict-bytes` over
    /// the cap with a typed error, before any backend traffic (the test
    /// client points at nothing routable — reaching the backend would
    /// fail differently).
    #[tokio::test]
    async fn train_dict_rejects_max_dict_bytes_over_cap() {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        let client = Client::from_conf(conf);
        let params = TrainDictParams {
            prefix: String::new(),
            max_samples: DEFAULT_TRAIN_MAX_SAMPLES,
            max_dict_bytes: DICT_FETCH_MAX_BYTES + 1,
            min_samples: DEFAULT_TRAIN_MIN_SAMPLES,
            sample_max_bytes: DEFAULT_TRAIN_SAMPLE_MAX_BYTES,
            zstd_level: 3,
        };
        let err = run_train_dict(&client, "bkt", &params)
            .await
            .expect_err("over-cap --max-dict-bytes must be refused");
        match err {
            TrainDictError::MaxDictBytesOverCap { requested, cap } => {
                assert_eq!(requested, DICT_FETCH_MAX_BYTES + 1);
                assert_eq!(cap, DICT_FETCH_MAX_BYTES);
            }
            other => panic!("expected MaxDictBytesOverCap, got {other:?}"),
        }
    }

    /// Shared fingerprint helper: prefix check always, full-SHA-256 check
    /// only when a claim is present (back-compat for pre-v1.0.1 dicts).
    #[test]
    fn verify_dict_bytes_discipline() {
        let bytes = fake_dict(7);
        let id = dict_id_of(&bytes);
        let full = dict_sha256_hex(&bytes);
        verify_dict_bytes(&id, None, &bytes).expect("prefix-only check passes");
        verify_dict_bytes(&id, Some(&full), &bytes).expect("full claim matches");
        let err = verify_dict_bytes("0123456789abcdef", None, &bytes)
            .expect_err("wrong id must be refused");
        assert!(err.contains("fingerprint mismatch"), "{err}");
        let wrong_claim = format!("{}{}", id, "0".repeat(64 - DICT_ID_HEX_LEN));
        let err = verify_dict_bytes(&id, Some(&wrong_claim), &bytes)
            .expect_err("prefix matches but full claim differs — must be refused");
        assert!(err.contains("full SHA-256 mismatch"), "{err}");
    }

    #[test]
    fn full_sha256_hex_is_64_chars_and_prefixes_dict_id() {
        let bytes = fake_dict(9);
        let full = dict_sha256_hex(&bytes);
        assert_eq!(full.len(), 64);
        assert!(full.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(&full[..DICT_ID_HEX_LEN], dict_id_of(&bytes));
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

    // =======================================================================
    // v1.3 dict ops
    // =======================================================================

    #[test]
    fn lookup_with_prefix_returns_matched_prefix() {
        let d_short = fake_dict(1);
        let d_long = fake_dict(2);
        let store = store_with(&[("bkt/logs/", d_short), ("bkt/logs/app1/", d_long.clone())]);
        let (prefix, id, _) = store
            .lookup_with_prefix("bkt", "logs/app1/x.json")
            .expect("hit");
        assert_eq!(prefix, "bkt/logs/app1/");
        assert_eq!(id, dict_id_of(&d_long));
        assert!(store.lookup_with_prefix("bkt", "images/x.png").is_none());
    }

    #[test]
    fn map_file_parses_and_validates_like_the_flag() {
        let entries = parse_zstd_dict_map(
            r#"
[mappings]
"bkt/logs/" = "0123456789abcdef"
"bkt/events/v2/" = "fedcba9876543210"
"#,
        )
        .expect("parse");
        assert_eq!(entries.len(), 2);
        // BTreeMap order: "bkt/events/v2/" sorts before "bkt/logs/".
        assert_eq!(entries[0].prefix, "bkt/events/v2/");
        assert_eq!(entries[0].bucket, "bkt");
        assert_eq!(entries[0].dict_id, "fedcba9876543210");
        assert_eq!(entries[1].prefix, "bkt/logs/");

        // Empty file / empty table → zero entries, not an error.
        assert!(parse_zstd_dict_map("").expect("empty ok").is_empty());
        assert!(
            parse_zstd_dict_map("[mappings]\n")
                .expect("empty table ok")
                .is_empty()
        );
    }

    #[test]
    fn map_file_rejects_bad_shapes() {
        // Invalid dict-id (same rule as the flag parser).
        let err = parse_zstd_dict_map("[mappings]\n\"bkt/logs/\" = \"BADID\"\n")
            .expect_err("bad id must be rejected");
        assert!(err.contains("lowercase hex"), "{err}");
        // Missing bucket separator.
        let err = parse_zstd_dict_map("[mappings]\n\"nobucket\" = \"0123456789abcdef\"\n")
            .expect_err("prefix without '/' must be rejected");
        assert!(err.contains("bucket"), "{err}");
        // Duplicate prefix = TOML duplicate key = parse error.
        let err = parse_zstd_dict_map(
            "[mappings]\n\"bkt/a/\" = \"0123456789abcdef\"\n\"bkt/a/\" = \"fedcba9876543210\"\n",
        )
        .expect_err("duplicate prefix must be rejected");
        assert!(err.contains("invalid TOML"), "{err}");
        // Typo'd table name must not silently configure nothing.
        let err = parse_zstd_dict_map("[mapping]\n\"bkt/a/\" = \"0123456789abcdef\"\n")
            .expect_err("unknown table must be rejected");
        assert!(err.contains("invalid TOML"), "{err}");
        // Not TOML at all.
        assert!(parse_zstd_dict_map("{json: true}").is_err());
    }

    #[test]
    fn merge_rejects_prefix_in_both_sources() {
        let flag = vec![parse_zstd_dict_flag("bkt/a/=0123456789abcdef").expect("parse")];
        let map = vec![parse_zstd_dict_flag("bkt/b/=0123456789abcdef").expect("parse")];
        let merged = merge_dict_entries(flag.clone(), map).expect("disjoint merge");
        assert_eq!(merged.len(), 2);

        // Same prefix in both — error even with the same id.
        let map_dup = vec![parse_zstd_dict_flag("bkt/a/=0123456789abcdef").expect("parse")];
        let err = merge_dict_entries(flag, map_dup).expect_err("dup prefix must be rejected");
        assert!(
            err.contains("both --zstd-dict and --zstd-dict-map"),
            "{err}"
        );
    }

    #[test]
    fn shared_store_swaps_atomically() {
        let shared = SharedDictStore::default();
        assert!(shared.load().is_none(), "default = dict feature off");
        let d1 = fake_dict(1);
        let store1 = Arc::new(store_with(&[("bkt/a/", d1.clone())]));
        let shared = SharedDictStore::new(Some(Arc::clone(&store1)));
        let gen1 = shared.load().expect("gen1");
        assert!(gen1.lookup("bkt", "a/x").is_some());
        // Swap in a store with a different prefix; old guard still works.
        let d2 = fake_dict(2);
        let store2 = Arc::new(store_with(&[("bkt/b/", d2)]));
        shared.swap(store2);
        let gen2 = shared.load().expect("gen2");
        assert!(gen2.lookup("bkt", "a/x").is_none());
        assert!(gen2.lookup("bkt", "b/x").is_some());
        assert!(
            gen1.lookup("bkt", "a/x").is_some(),
            "in-flight readers keep the generation they loaded"
        );
    }

    #[test]
    fn win_tracker_judges_only_full_windows() {
        let t = DictWinTracker::with_window(4);
        // 3 losses: window not full yet → never warns.
        assert!(t.record("p", false).is_none());
        assert!(t.record("p", false).is_none());
        assert!(t.record("p", false).is_none());
        // 4th decision fills the window at rate 0.0 → warn fires once.
        let rate = t.record("p", false).expect("full window below 0.5");
        assert!(rate.abs() < f64::EPSILON, "rate {rate}");
        // Still below threshold but inside the 1-hour rate limit → quiet.
        assert!(t.record("p", false).is_none());
        // A different prefix has its own window + rate limit.
        for _ in 0..3 {
            assert!(t.record("q", false).is_none());
        }
        assert!(t.record("q", false).is_some());
    }

    #[test]
    fn win_tracker_rolls_the_window_and_rate_limits() {
        let t = DictWinTracker::with_window(4);
        let start = Instant::now();
        // 4 wins: rate 1.0, no warn.
        for _ in 0..4 {
            assert!(t.record_at("p", true, start).is_none());
        }
        // 2 losses: rolling rate 2/4 = 0.5 — NOT below the threshold.
        assert!(t.record_at("p", false, start).is_none());
        assert!(t.record_at("p", false, start).is_none());
        // 3rd loss: 1/4 = 0.25 < 0.5 → warn.
        let rate = t.record_at("p", false, start).expect("warn at 0.25");
        assert!((rate - 0.25).abs() < f64::EPSILON);
        // Rate-limited within the hour even though still below threshold.
        assert!(
            t.record_at("p", false, start + Duration::from_secs(60))
                .is_none()
        );
        // After the interval elapses the warn fires again.
        assert!(
            t.record_at("p", false, start + DICT_WIN_RATE_WARN_INTERVAL)
                .is_some()
        );
        // Recovery: wins refill the window above the threshold → quiet.
        for i in 0..4 {
            assert!(
                t.record_at(
                    "p",
                    true,
                    start + DICT_WIN_RATE_WARN_INTERVAL + Duration::from_secs(1 + i)
                )
                .is_none()
            );
        }
    }

    #[test]
    fn prom_sample_parser_shapes() {
        // No labels.
        let (name, labels, v) = parse_prom_sample("s4_dict_fetch_total 3").expect("parse");
        assert_eq!(name, "s4_dict_fetch_total");
        assert!(labels.is_empty());
        assert!((v - 3.0).abs() < f64::EPSILON);
        // Labels, '/' and '=' inside values, trailing whitespace.
        let (name, labels, v) =
            parse_prom_sample("s4_dict_put_total{prefix=\"bkt/k=v/\",outcome=\"win\"} 41 ")
                .expect("parse");
        assert_eq!(name, "s4_dict_put_total");
        assert_eq!(
            labels,
            vec![
                ("prefix".to_owned(), "bkt/k=v/".to_owned()),
                ("outcome".to_owned(), "win".to_owned())
            ]
        );
        assert!((v - 41.0).abs() < f64::EPSILON);
        // Escapes in label values.
        let (_, labels, _) = parse_prom_sample(r#"m{p="a\"b\\c\nd"} 1"#).expect("escapes parse");
        assert_eq!(labels[0].1, "a\"b\\c\nd");
        // Backslash followed by a NON-escape ASCII char copies raw.
        let (_, labels, _) = parse_prom_sample(r#"m{p="a\tb"} 1"#).expect("raw escape parse");
        assert_eq!(labels[0].1, "a\\tb");
        // v1.2 audit R1 P3: backslash followed by a multi-byte UTF-8 char
        // must not panic on a char-boundary slice — copied raw instead.
        let (_, labels, _) = parse_prom_sample("m{p=\"a\\éb\"} 1").expect("multibyte escape");
        assert_eq!(labels[0].1, "a\\éb");
        let (_, labels, _) = parse_prom_sample("m{p=\"\\日本\"} 2").expect("cjk escape");
        assert_eq!(labels[0].1, "\\日本");
        // Multi-byte chars *not* behind a backslash still parse (regression
        // guard for the pre-existing chars()-based plain path).
        let (_, labels, _) = parse_prom_sample("m{p=\"ログ/日本語/\"} 7").expect("plain utf8");
        assert_eq!(labels[0].1, "ログ/日本語/");
        // Dangling trailing backslash inside an (unterminated) value → None,
        // same contract as before.
        assert!(parse_prom_sample("m{p=\"x\\").is_none());
        // Comments / blank / garbage → None.
        assert!(parse_prom_sample("# TYPE s4_dict_put_total counter").is_none());
        assert!(parse_prom_sample("").is_none());
        assert!(parse_prom_sample("name{unterminated=\"x").is_none());
        assert!(parse_prom_sample("name{} not-a-number").is_none());
        // Float values (histograms etc.) parse fine.
        let (_, _, v) = parse_prom_sample("m 0.0001").expect("float");
        assert!((v - 0.0001).abs() < 1e-12);
    }

    /// The parser MUST understand what the real recorder renders — fixture
    /// captured live: drive the shared in-process Prometheus recorder with
    /// `record_dict_put` / `record_dict_fetch`, then feed `render()` output
    /// straight into `build_dict_status`.
    #[test]
    fn dict_status_from_real_recorder_render() {
        let handle = crate::metrics::test_metrics_handle();
        // Healthy prefix: 3 wins, 1 loss (win rate 0.75).
        for _ in 0..3 {
            crate::metrics::record_dict_put("statbkt/events/", true, 300, 60, 200);
        }
        crate::metrics::record_dict_put("statbkt/events/", false, 300, 210, 200);
        // Stale prefix: 1 win, 4 losses (win rate 0.2).
        crate::metrics::record_dict_put("statbkt/blobs/", true, 100, 90, 95);
        for _ in 0..4 {
            crate::metrics::record_dict_put("statbkt/blobs/", false, 100, 99, 95);
        }
        crate::metrics::record_dict_fetch("ok");

        let rendered = handle.render();
        let report = build_dict_status(&rendered, DEFAULT_DICT_STATUS_WARN_WIN_RATE);
        let events = report
            .prefixes
            .iter()
            .find(|p| p.prefix == "statbkt/events/")
            .expect("events prefix parsed from real render output");
        assert_eq!(events.wins, 3);
        assert_eq!(events.losses, 1);
        assert!((events.win_rate - 0.75).abs() < f64::EPSILON);
        assert_eq!(events.original_bytes, 1200);
        assert_eq!(events.dict_bytes, 390);
        assert_eq!(events.plain_bytes, 800);
        assert!((events.dict_ratio - 390.0 / 1200.0).abs() < 1e-9);
        assert!(!events.stale);

        let blobs = report
            .prefixes
            .iter()
            .find(|p| p.prefix == "statbkt/blobs/")
            .expect("blobs prefix");
        assert_eq!(blobs.wins, 1);
        assert_eq!(blobs.losses, 4);
        assert!(blobs.stale, "win rate 0.2 must trip the 0.5 default");
        assert!(report.dict_fetch_ok >= 1);
        let warning = report
            .warnings
            .iter()
            .find(|w| w.contains("statbkt/blobs/"))
            .expect("stale prefix must produce a warning line");
        assert!(
            warning.contains("dictionary may be stale; consider retraining (s4 train-dict)"),
            "{warning}"
        );
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("statbkt/events/")),
            "healthy prefix must not warn: {:?}",
            report.warnings
        );
    }

    #[test]
    fn dict_status_empty_scrape_is_quiet() {
        let report = build_dict_status(
            "# HELP something_else\nsomething_else 5\n",
            DEFAULT_DICT_STATUS_WARN_WIN_RATE,
        );
        assert!(report.prefixes.is_empty());
        assert!(report.warnings.is_empty());
        assert_eq!(report.dict_fetch_ok, 0);
        assert_eq!(report.dict_fetch_err, 0);
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
