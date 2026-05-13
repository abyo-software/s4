//! Object + Bucket tagging (v0.6 #39).
//!
//! S3 attaches a `TagSet` (max 10 key/value pairs, key ≤ 128 bytes,
//! value ≤ 256 bytes — AWS S3 spec) to each object, and another (also
//! max 10) to each bucket. Tags are surfaced to the IAM policy
//! evaluator via two condition keys:
//!
//! - `s3:ExistingObjectTag/<key>` — the existing tag attached to the
//!   object the request is targeting (resolved via [`TagManager`] at
//!   policy evaluation time).
//! - `s3:RequestObjectTag/<key>` — a tag the caller is supplying as
//!   part of *this* request, either via the `x-amz-tagging` URL-encoded
//!   header on `PutObject`, or via the `Tagging` body field on
//!   `PutObjectTagging`.
//!
//! ## scope (v0.6 #39)
//!
//! - **In-memory only** with optional JSON snapshot for restart-
//!   recoverable state — same shape as `versioning.rs` /
//!   `object_lock.rs`'s `--versioning-state-file` /
//!   `--object-lock-state-file`.
//! - **Per-(bucket, key) granularity**, no version-id-aware tag
//!   attachment (matches the v0.5 #30 object-lock decision; AWS-style
//!   per-version tags can be layered on top later).
//! - **No charge / accounting** model — tags are stored, served, and
//!   evaluated; cost-allocation reports are out of scope.
//! - **No tag-key character validation** beyond the AWS length limits.
//!   The wider AWS rule set (allowed character class, no `aws:` prefix
//!   for user tags, etc.) is deferred — operators get the spec as it
//!   relates to gating but can store any UTF-8 they like.
//!
//! ## scope-out (DO NOT touch — handled by sibling agents)
//!
//! - notification dispatch (#35), lifecycle expiration (#37)
//! - ACL / replication / website / logging
//! - per-version tag attachment

use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// AWS S3 max number of tags per object / bucket.
pub const MAX_TAGS_PER_OBJECT: usize = 10;
/// AWS S3 max length (in bytes) of a tag key.
pub const MAX_TAG_KEY_BYTES: usize = 128;
/// AWS S3 max length (in bytes) of a tag value.
pub const MAX_TAG_VALUE_BYTES: usize = 256;

/// An ordered tag set. Insertion order is preserved (mirrors the AWS
/// XML wire format, which is order-significant for the response). For
/// duplicates on the same key, the *last* pair wins on lookup, matching
/// AWS S3 behaviour for `x-amz-tagging`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSet(pub Vec<(String, String)>);

impl TagSet {
    /// Empty tag set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a tag set from `(key, value)` pairs, validating the
    /// AWS S3 limits (max 10, key ≤ 128 B, value ≤ 256 B). Duplicate
    /// keys are retained in insertion order; lookup picks the last one.
    pub fn from_pairs(pairs: Vec<(String, String)>) -> Result<Self, TagError> {
        let s = Self(pairs);
        s.validate()?;
        Ok(s)
    }

    /// Look up the value for `key`. Last-wins on duplicates.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Iterate the pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &(String, String)> {
        self.0.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Enforce the AWS S3 size limits (count ≤ 10, key ≤ 128 B,
    /// value ≤ 256 B). Called by [`Self::from_pairs`]; can also be
    /// called directly when constructing a `TagSet` from external
    /// input that wasn't validated yet.
    pub fn validate(&self) -> Result<(), TagError> {
        if self.0.len() > MAX_TAGS_PER_OBJECT {
            return Err(TagError::TooMany { got: self.0.len() });
        }
        for (k, v) in &self.0 {
            if k.len() > MAX_TAG_KEY_BYTES {
                return Err(TagError::KeyTooLong { len: k.len() });
            }
            if v.len() > MAX_TAG_VALUE_BYTES {
                return Err(TagError::ValueTooLong { len: v.len() });
            }
        }
        Ok(())
    }
}

/// Error class for tag-set construction / parse.
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("too many tags: {got} (max {})", MAX_TAGS_PER_OBJECT)]
    TooMany { got: usize },
    #[error("tag key too long: {len} bytes (max {})", MAX_TAG_KEY_BYTES)]
    KeyTooLong { len: usize },
    #[error("tag value too long: {len} bytes (max {})", MAX_TAG_VALUE_BYTES)]
    ValueTooLong { len: usize },
    #[error("invalid tag header (URL-encoded): {0}")]
    InvalidHeader(String),
}

/// JSON snapshot wrapper. Tuple keys can't roundtrip through
/// `HashMap` JSON, so the object map is flattened to a `Vec`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TagSnapshot {
    objects: Vec<((String, String), TagSet)>,
    buckets: HashMap<String, TagSet>,
}

/// Owns the per-(bucket, key) and per-bucket tag state. All operations
/// take the inner `RwLock`; cloning a manager is intentionally not
/// supported — share via `Arc<TagManager>`.
#[derive(Debug, Default)]
pub struct TagManager {
    /// `(bucket, key) → tags`
    objects: RwLock<HashMap<(String, String), TagSet>>,
    /// `bucket → tags`
    buckets: RwLock<HashMap<String, TagSet>>,
}

impl TagManager {
    /// Empty manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace (or create) the object-level tag set. AWS PutObjectTagging
    /// is a full-replace operation (no merge), so we mirror that.
    pub fn put_object_tags(&self, bucket: &str, key: &str, tags: TagSet) {
        self.objects
            .write()
            .expect("tagging objects RwLock poisoned")
            .insert((bucket.to_owned(), key.to_owned()), tags);
    }

    /// Borrow-clone the object-level tag set. `None` when no tags have
    /// been set for `(bucket, key)`.
    #[must_use]
    pub fn get_object_tags(&self, bucket: &str, key: &str) -> Option<TagSet> {
        self.objects
            .read()
            .expect("tagging objects RwLock poisoned")
            .get(&(bucket.to_owned(), key.to_owned()))
            .cloned()
    }

    /// Drop the object-level tag set for `(bucket, key)` (idempotent —
    /// missing entry is a no-op, matching AWS DeleteObjectTagging).
    pub fn delete_object_tags(&self, bucket: &str, key: &str) {
        self.objects
            .write()
            .expect("tagging objects RwLock poisoned")
            .remove(&(bucket.to_owned(), key.to_owned()));
    }

    /// Replace (or create) the bucket-level tag set.
    pub fn put_bucket_tags(&self, bucket: &str, tags: TagSet) {
        self.buckets
            .write()
            .expect("tagging buckets RwLock poisoned")
            .insert(bucket.to_owned(), tags);
    }

    /// Borrow-clone the bucket-level tag set.
    #[must_use]
    pub fn get_bucket_tags(&self, bucket: &str) -> Option<TagSet> {
        self.buckets
            .read()
            .expect("tagging buckets RwLock poisoned")
            .get(bucket)
            .cloned()
    }

    /// Drop the bucket-level tag set (idempotent).
    pub fn delete_bucket_tags(&self, bucket: &str) {
        self.buckets
            .write()
            .expect("tagging buckets RwLock poisoned")
            .remove(bucket);
    }

    /// JSON snapshot for restart-recoverable state. Pair with
    /// [`Self::from_json`].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let objects: Vec<((String, String), TagSet)> = self
            .objects
            .read()
            .expect("tagging objects RwLock poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let buckets = self
            .buckets
            .read()
            .expect("tagging buckets RwLock poisoned")
            .clone();
        let snap = TagSnapshot { objects, buckets };
        serde_json::to_string(&snap)
    }

    /// Restore from a JSON snapshot produced by [`Self::to_json`].
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: TagSnapshot = serde_json::from_str(s)?;
        let mut objects = HashMap::with_capacity(snap.objects.len());
        for (k, v) in snap.objects {
            objects.insert(k, v);
        }
        Ok(Self {
            objects: RwLock::new(objects),
            buckets: RwLock::new(snap.buckets),
        })
    }
}

/// Parse the AWS S3 `x-amz-tagging` request header. The wire format is
/// a URL-encoded query string (`Project=Phoenix&Env=prod`); each pair
/// is `key=value` with both halves percent-decoded. An empty header
/// resolves to an empty `TagSet`. Keys without `=` are treated as
/// `(key, "")` (matches `serde_urlencoded` / browser form-encode).
///
/// The parsed result is validated against the AWS S3 size limits.
pub fn parse_tagging_header(header: &str) -> Result<TagSet, TagError> {
    let trimmed = header.trim();
    if trimmed.is_empty() {
        return Ok(TagSet::new());
    }
    let mut pairs = Vec::new();
    for part in trimmed.split('&') {
        if part.is_empty() {
            continue;
        }
        let (raw_k, raw_v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        let k = url_decode(raw_k)
            .map_err(|e| TagError::InvalidHeader(format!("key {raw_k:?}: {e}")))?;
        let v = url_decode(raw_v)
            .map_err(|e| TagError::InvalidHeader(format!("value {raw_v:?}: {e}")))?;
        pairs.push((k, v));
    }
    TagSet::from_pairs(pairs)
}

/// Render a tag set as an AWS S3 `x-amz-tagging` URL-encoded string,
/// suitable for the response echo header. Insertion order is
/// preserved.
#[must_use]
pub fn render_tagging_header(tags: &TagSet) -> String {
    let mut out = String::new();
    for (i, (k, v)) in tags.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        url_encode_to(&mut out, k);
        out.push('=');
        url_encode_to(&mut out, v);
    }
    out
}

/// Minimal `application/x-www-form-urlencoded` decoder: turns `+` into
/// space (RFC 3986 form variant — AWS S3 accepts both `%20` and `+`)
/// and resolves `%xx` escapes to their byte value. Returns an error
/// when a `%` is not followed by two hex digits, or when the resulting
/// bytes are not valid UTF-8.
fn url_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(format!("truncated %-escape at byte {i}"));
                }
                let hi = hex_digit(bytes[i + 1])
                    .ok_or_else(|| format!("non-hex byte after % at {}", i + 1))?;
                let lo = hex_digit(bytes[i + 2])
                    .ok_or_else(|| format!("non-hex byte after % at {}", i + 2))?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|e| format!("invalid UTF-8: {e}"))
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

/// Append `s` to `out`, percent-encoding everything that isn't an
/// unreserved RFC 3986 character (`A-Za-z0-9-_.~`). Conservative —
/// AWS accepts a wider class but never *requires* it, so we keep the
/// output portable.
fn url_encode_to(out: &mut String, s: &str) {
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || b == b'-'
            || b == b'_'
            || b == b'.'
            || b == b'~';
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[((b >> 4) & 0x0F) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_pairs_too_many_rejected() {
        let pairs: Vec<(String, String)> = (0..11)
            .map(|i| (format!("k{i}"), format!("v{i}")))
            .collect();
        let err = TagSet::from_pairs(pairs).expect_err("must reject 11 pairs");
        assert!(matches!(err, TagError::TooMany { got: 11 }));
    }

    #[test]
    fn from_pairs_long_key_rejected() {
        let pairs = vec![("k".repeat(129), "v".into())];
        let err = TagSet::from_pairs(pairs).expect_err("must reject 129-byte key");
        assert!(matches!(err, TagError::KeyTooLong { len: 129 }));
    }

    #[test]
    fn from_pairs_long_value_rejected() {
        let pairs = vec![("k".into(), "v".repeat(257))];
        let err = TagSet::from_pairs(pairs).expect_err("must reject 257-byte value");
        assert!(matches!(err, TagError::ValueTooLong { len: 257 }));
    }

    #[test]
    fn from_pairs_at_limits_accepted() {
        // Exactly 10 tags, key = 128 bytes, value = 256 bytes — at the
        // boundary, must be accepted.
        let pairs: Vec<(String, String)> = (0..10)
            .map(|i| {
                let k = format!("k{i}");
                let v = format!("v{i}");
                let k = format!("{k:k<128}");
                let v = format!("{v:v<256}");
                (k, v)
            })
            .collect();
        // Sanity: lengths are at the limit.
        for (k, v) in &pairs {
            assert_eq!(k.len(), 128);
            assert_eq!(v.len(), 256);
        }
        let s = TagSet::from_pairs(pairs).expect("at-limit pairs must pass");
        assert_eq!(s.len(), 10);
    }

    #[test]
    fn parse_tagging_header_basic() {
        let s = parse_tagging_header("K1=V1&K2=V2").expect("parse");
        assert_eq!(s.len(), 2);
        assert_eq!(s.get("K1"), Some("V1"));
        assert_eq!(s.get("K2"), Some("V2"));
    }

    #[test]
    fn parse_tagging_header_url_encoded_values() {
        // `%20` (space), `%2F` (slash), and `+` (form-style space).
        let s = parse_tagging_header("Path=foo%2Fbar&Greet=hello%20world&Plus=a+b")
            .expect("parse");
        assert_eq!(s.get("Path"), Some("foo/bar"));
        assert_eq!(s.get("Greet"), Some("hello world"));
        assert_eq!(s.get("Plus"), Some("a b"));
    }

    #[test]
    fn parse_tagging_header_empty_value() {
        let s = parse_tagging_header("Bare").expect("parse");
        assert_eq!(s.get("Bare"), Some(""));
        let s2 = parse_tagging_header("K=").expect("parse");
        assert_eq!(s2.get("K"), Some(""));
    }

    #[test]
    fn parse_tagging_header_empty_returns_empty_set() {
        let s = parse_tagging_header("").expect("parse");
        assert!(s.is_empty());
        let s2 = parse_tagging_header("   ").expect("parse");
        assert!(s2.is_empty());
    }

    #[test]
    fn parse_tagging_header_truncated_escape_rejected() {
        let err = parse_tagging_header("K=%2").expect_err("truncated");
        assert!(matches!(err, TagError::InvalidHeader(_)));
    }

    #[test]
    fn render_tagging_header_round_trip() {
        let original = TagSet::from_pairs(vec![
            ("Project".into(), "Phoenix".into()),
            ("Env".into(), "prod with space".into()),
            ("Path".into(), "data/2026".into()),
        ])
        .expect("ts");
        let rendered = render_tagging_header(&original);
        let parsed = parse_tagging_header(&rendered).expect("parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn manager_object_put_get_delete() {
        let m = TagManager::new();
        let tags =
            TagSet::from_pairs(vec![("Owner".into(), "alice".into())]).expect("ts");
        m.put_object_tags("b", "k", tags.clone());
        assert_eq!(m.get_object_tags("b", "k"), Some(tags));
        m.delete_object_tags("b", "k");
        assert!(m.get_object_tags("b", "k").is_none());
        // Idempotent re-delete.
        m.delete_object_tags("b", "k");
    }

    #[test]
    fn manager_bucket_put_get_delete() {
        let m = TagManager::new();
        let tags =
            TagSet::from_pairs(vec![("CostCenter".into(), "42".into())]).expect("ts");
        m.put_bucket_tags("b", tags.clone());
        assert_eq!(m.get_bucket_tags("b"), Some(tags));
        m.delete_bucket_tags("b");
        assert!(m.get_bucket_tags("b").is_none());
    }

    #[test]
    fn manager_object_and_bucket_independent() {
        // Setting an object tag must not pollute the bucket-level map
        // (and vice versa). Regression guard for an early-prototype
        // bug where both maps were keyed by `bucket` only.
        let m = TagManager::new();
        m.put_object_tags(
            "b",
            "k",
            TagSet::from_pairs(vec![("o".into(), "1".into())]).unwrap(),
        );
        m.put_bucket_tags("b", TagSet::from_pairs(vec![("b".into(), "2".into())]).unwrap());
        assert_eq!(m.get_object_tags("b", "k").unwrap().get("o"), Some("1"));
        assert!(m.get_object_tags("b", "k").unwrap().get("b").is_none());
        assert_eq!(m.get_bucket_tags("b").unwrap().get("b"), Some("2"));
        assert!(m.get_bucket_tags("b").unwrap().get("o").is_none());
    }

    #[test]
    fn manager_json_snapshot_round_trip() {
        let m = TagManager::new();
        m.put_object_tags(
            "b1",
            "k1",
            TagSet::from_pairs(vec![("Project".into(), "Phoenix".into())]).unwrap(),
        );
        m.put_object_tags(
            "b2",
            "k2",
            TagSet::from_pairs(vec![("Env".into(), "prod".into())]).unwrap(),
        );
        m.put_bucket_tags(
            "b1",
            TagSet::from_pairs(vec![("CostCenter".into(), "42".into())]).unwrap(),
        );
        let json = m.to_json().expect("to_json");
        let m2 = TagManager::from_json(&json).expect("from_json");
        assert_eq!(
            m2.get_object_tags("b1", "k1").unwrap().get("Project"),
            Some("Phoenix")
        );
        assert_eq!(
            m2.get_object_tags("b2", "k2").unwrap().get("Env"),
            Some("prod")
        );
        assert_eq!(
            m2.get_bucket_tags("b1").unwrap().get("CostCenter"),
            Some("42")
        );
    }

    #[test]
    fn tag_set_get_last_wins_on_duplicate_keys() {
        // AWS x-amz-tagging "K=A&K=B" → look-up returns "B".
        let s = parse_tagging_header("K=A&K=B").expect("parse");
        assert_eq!(s.get("K"), Some("B"));
    }
}
