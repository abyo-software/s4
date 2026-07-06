//! Durable multipart part-state records (`.s4mpu/` reserved prefix).
//!
//! ## Why
//!
//! The client-transparent multipart composite ETag
//! (`MD5(concat(original-part-MD5s))-N`, s3-compat fix #4) needs every
//! part's ORIGINAL-payload MD5 at `CompleteMultipartUpload` time. Those
//! MD5s are recorded by `UploadPart` into the in-memory
//! [`crate::multipart_state::MultipartStateStore`] — which does not
//! survive a gateway restart and is not shared between the `>=2`
//! stateless gateways of the recommended HA topology. Pre-durable-state,
//! a restart mid-upload (or parts landing on different instances) meant
//! Complete still succeeded (ListParts reverse-map) but the object kept
//! the backend composite ETag with no logical stamp.
//!
//! ## What
//!
//! On each successful `UploadPart` / `UploadPartCopy` (under the default
//! client-transparent ETag mode) the gateway additionally persists that
//! part's `(original_md5, backend_etag)` pair as ONE SMALL JSON object
//! in the **backend bucket** under the reserved prefix:
//!
//! ```text
//! .s4mpu/<hex(upload_id)>/<part_number>
//! ```
//!
//! One object per part means two gateways never read-modify-write a
//! shared manifest — a re-PUT of the same part number simply overwrites
//! its record (last-writer-wins, matching S3 part-overwrite semantics).
//! The `upload_id` is lowercase-hex-encoded because backends mint opaque
//! ids that may contain `/` or other key-structure-ambiguous characters.
//!
//! `CompleteMultipartUpload` merges these records under the in-memory
//! map (**in-memory wins** for parts present in both) so any gateway —
//! or a restarted one — can compute the full composite and keep strict
//! part-ETag validation. Complete/Abort best-effort delete the upload's
//! records; `s4 maintain` (`action = "mpu-state-gc"`) garbage-collects
//! records whose upload no longer exists on the backend.
//!
//! ## What is (and is NOT) in a record
//!
//! Only the two per-part ETag halves: `MD5(original part bytes)` and the
//! backend's (compressed-part) ETag — both are content fingerprints the
//! wire already exposes, not secrets. The per-upload SSE recipe
//! (`MultipartSseMode`, including the raw SSE-C key) is deliberately
//! NEVER persisted: it stays in-memory-only with its existing
//! `Zeroizing` lifetime, so durable records add no key-material-at-rest
//! surface. (Consequence: an SSE multipart upload still needs its
//! Create/Complete pair on the same live gateway for the encrypt
//! post-processing — see `docs/compatibility.md`.)

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::multipart_state::PartEtags;

/// Reserved backend-key prefix for durable multipart part-state
/// records. Same namespace family as `.s4dict/` (shared dictionaries)
/// and `.__s4ver__/` (versioning shadows): hidden from listings,
/// blocked for client writes, skipped by the offline tools.
pub const MPU_STATE_PREFIX: &str = ".s4mpu/";

/// `true` for keys inside the durable multipart-state namespace.
#[must_use]
pub fn is_mpu_state_key(key: &str) -> bool {
    key.starts_with(MPU_STATE_PREFIX)
}

/// Lowercase-hex of `upload_id`'s UTF-8 bytes. Backends mint opaque
/// upload ids that may contain `/`, `+`, whitespace, … — hex keeps the
/// record key's `<uploadId>/<partNumber>` structure unambiguous and
/// reversible.
fn hex_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Inverse of [`hex_encode`]. `None` unless the input is an even-length
/// lowercase-hex string decoding to valid UTF-8.
fn hex_decode(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    for pair in s.as_bytes().chunks_exact(2) {
        let hi = char::from(pair[0]).to_digit(16)?;
        let lo = char::from(pair[1]).to_digit(16)?;
        // Reject uppercase so encode→decode→encode is the identity
        // (record keys are always minted lowercase).
        if pair[0].is_ascii_uppercase() || pair[1].is_ascii_uppercase() {
            return None;
        }
        bytes.push(((hi << 4) | lo) as u8);
    }
    String::from_utf8(bytes).ok()
}

/// `.s4mpu/<hex(upload_id)>/` — the listing prefix that covers every
/// record of one upload (used by Complete/Abort cleanup and the
/// `mpu-state-gc` maintain action).
#[must_use]
pub fn upload_prefix(upload_id: &str) -> String {
    format!("{MPU_STATE_PREFIX}{}/", hex_encode(upload_id))
}

/// `.s4mpu/<hex(upload_id)>/<part_number>` — the backend key of one
/// part's durable record.
#[must_use]
pub fn record_key(upload_id: &str, part_number: i32) -> String {
    format!("{MPU_STATE_PREFIX}{}/{part_number}", hex_encode(upload_id))
}

/// Parse a record key back into `(upload_id, part_number)`. `None` for
/// anything that is not a well-formed record key (the `mpu-state-gc`
/// maintain action reports those as `skipped-unparseable` instead of
/// deleting blindly).
#[must_use]
pub fn parse_record_key(key: &str) -> Option<(String, i32)> {
    let rest = key.strip_prefix(MPU_STATE_PREFIX)?;
    let (hex_id, part) = rest.split_once('/')?;
    let upload_id = hex_decode(hex_id)?;
    if upload_id.is_empty() {
        return None;
    }
    // Reject `01`-style zero-padded / signed part segments so one
    // logical part number has exactly one canonical key.
    if part.len() > 1 && part.starts_with('0') {
        return None;
    }
    let part_number: i32 = part.parse().ok()?;
    if !(1..=10_000).contains(&part_number) {
        return None;
    }
    Some((upload_id, part_number))
}

/// One part's durable state, JSON-serialized as the record object body.
///
/// v1: `{"v":1,"upload_id":…,"part_number":…,"original_md5":…,
/// "backend_etag":…,"key":…}`. `upload_id` / `part_number` are
/// intentionally duplicated from the record key so a reader can verify
/// the body belongs to the key it was fetched under (defense against
/// out-of-band copies / renames of record objects). `key` (the logical
/// object key) is informational for operators inspecting the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurablePartRecord {
    /// Format version. Readers reject anything but `1` (fail closed to
    /// the pre-durable fallback rather than mis-parse a future format).
    pub v: u32,
    pub upload_id: String,
    pub part_number: i32,
    /// `MD5(original part bytes)` as 32 lowercase hex chars — the value
    /// `UploadPart` advertised to the client.
    pub original_md5: String,
    /// The backend-issued (compressed-part) ETag, unquoted strong form.
    pub backend_etag: String,
    /// Logical object key the upload targets (informational).
    pub key: String,
}

impl DurablePartRecord {
    /// Current on-backend format version.
    pub const VERSION: u32 = 1;

    /// JSON-encode for the record PUT body.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        // A struct of strings/ints cannot fail to serialize; fall back
        // to an empty body (decode rejects it) rather than panic.
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Decode + validate a record fetched from
    /// [`record_key`]`(upload_id, part_number)`. Returns `None` (caller
    /// falls back to ListParts recovery) when the body is not valid
    /// JSON, is a different format version, or does not match the key
    /// it was fetched under.
    #[must_use]
    pub fn decode(bytes: &[u8], upload_id: &str, part_number: i32) -> Option<Self> {
        let rec: Self = serde_json::from_slice(bytes).ok()?;
        if rec.v != Self::VERSION
            || rec.upload_id != upload_id
            || rec.part_number != part_number
            || !is_md5_hex(&rec.original_md5)
            || rec.backend_etag.is_empty()
        {
            return None;
        }
        Some(rec)
    }
}

/// 32 lowercase-hex chars (the `md5_hex` output shape the composite
/// decoder requires).
fn is_md5_hex(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Merge durable records under the in-memory map: **in-memory wins**
/// for any part number present in both (the local gateway's own record
/// is at least as fresh as what it persisted). Returns `None` only when
/// both sides are empty, preserving the `Option` shape
/// `complete_multipart_upload` branches on.
#[must_use]
pub fn merge_parts(
    in_memory: Option<HashMap<i32, PartEtags>>,
    durable: HashMap<i32, PartEtags>,
) -> Option<HashMap<i32, PartEtags>> {
    if durable.is_empty() {
        return in_memory;
    }
    let mut merged = in_memory.unwrap_or_default();
    for (pn, pe) in durable {
        merged.entry(pn).or_insert(pe);
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(upload_id: &str, part_number: i32) -> DurablePartRecord {
        DurablePartRecord {
            v: DurablePartRecord::VERSION,
            upload_id: upload_id.to_owned(),
            part_number,
            original_md5: "0123456789abcdef0123456789abcdef".to_owned(),
            backend_etag: "be-etag".to_owned(),
            key: "some/object.bin".to_owned(),
        }
    }

    /// encode → decode round-trips every field.
    #[test]
    fn record_encode_decode_round_trip() {
        let r = rec("upload-1", 7);
        let bytes = r.encode();
        let back =
            DurablePartRecord::decode(&bytes, "upload-1", 7).expect("round-trip must decode");
        assert_eq!(back, r);
    }

    /// decode fails closed on: garbage bytes, wrong format version,
    /// key/body mismatch (upload id or part number), malformed MD5,
    /// empty backend ETag.
    #[test]
    fn record_decode_rejects_invalid() {
        let good = rec("u", 1);
        assert!(DurablePartRecord::decode(b"not json", "u", 1).is_none());
        assert!(DurablePartRecord::decode(b"", "u", 1).is_none());

        let mut wrong_v = good.clone();
        wrong_v.v = 2;
        assert!(
            DurablePartRecord::decode(&wrong_v.encode(), "u", 1).is_none(),
            "future format version must be rejected (fail closed)"
        );

        assert!(
            DurablePartRecord::decode(&good.encode(), "other-upload", 1).is_none(),
            "upload-id mismatch vs the key it was fetched under must reject"
        );
        assert!(
            DurablePartRecord::decode(&good.encode(), "u", 2).is_none(),
            "part-number mismatch must reject"
        );

        let mut bad_md5 = good.clone();
        bad_md5.original_md5 = "XYZ".into();
        assert!(DurablePartRecord::decode(&bad_md5.encode(), "u", 1).is_none());
        let mut upper_md5 = good.clone();
        upper_md5.original_md5 = "0123456789ABCDEF0123456789ABCDEF".into();
        assert!(
            DurablePartRecord::decode(&upper_md5.encode(), "u", 1).is_none(),
            "uppercase hex is not the md5_hex shape the composite builder emits"
        );

        let mut no_etag = good;
        no_etag.backend_etag = String::new();
        assert!(DurablePartRecord::decode(&no_etag.encode(), "u", 1).is_none());
    }

    /// Record keys hex-encode the upload id, so ids containing `/`,
    /// `+`, spaces, or non-ASCII round-trip through
    /// `record_key`/`parse_record_key` unambiguously.
    #[test]
    fn record_key_round_trips_hostile_upload_ids() {
        for id in [
            "plain-id",
            "with/slash/es",
            "base64+ish/id==",
            "spaces and\ttabs",
            "日本語アップロード",
            "2~x7abc.def_ghi-jkl",
        ] {
            for pn in [1, 42, 10_000] {
                let key = record_key(id, pn);
                assert!(is_mpu_state_key(&key));
                assert!(
                    !key[MPU_STATE_PREFIX.len()..]
                        .split('/')
                        .next()
                        .unwrap_or_default()
                        .contains('/'),
                    "hex segment must not contain a separator"
                );
                let (back_id, back_pn) =
                    parse_record_key(&key).expect("minted key must parse back");
                assert_eq!(back_id, id);
                assert_eq!(back_pn, pn);
                assert!(
                    key.starts_with(&upload_prefix(id)),
                    "record key must live under its upload's cleanup prefix"
                );
            }
        }
    }

    /// Non-record keys and malformed variants parse to `None`.
    #[test]
    fn parse_record_key_rejects_malformed() {
        for k in [
            "user/object.bin",            // not in the namespace
            ".s4mpu/",                    // no upload segment
            ".s4mpu/deadbeef",            // no part segment
            ".s4mpu/deadbeef/",           // empty part segment
            ".s4mpu/deadbeef/notanumber", // non-numeric part
            ".s4mpu/deadbeef/0",          // part 0 out of range
            ".s4mpu/deadbeef/10001",      // > 10000 out of range
            ".s4mpu/deadbeef/-1",         // negative
            ".s4mpu/deadbeef/007",        // zero-padded (non-canonical)
            ".s4mpu/abc/1",               // odd-length hex
            ".s4mpu/zzzz/1",              // non-hex chars
            ".s4mpu/DEADBEEF/1",          // uppercase hex (non-canonical)
            ".s4mpu//1",                  // empty upload id
            ".s4mpu/deadbeef/1/trailing", // extra path segment
            ".s4mpu/fffefdfc/1",          // invalid UTF-8 after decode
        ] {
            assert!(parse_record_key(k).is_none(), "must reject {k:?}");
        }
    }

    /// Merge semantics: in-memory wins on collisions, durable fills the
    /// gaps, `None`+empty stays `None`.
    #[test]
    fn merge_parts_in_memory_wins() {
        let mem: HashMap<i32, PartEtags> = HashMap::from([(
            1,
            PartEtags {
                original_md5: "a".repeat(32),
                backend_etag: "mem-1".into(),
            },
        )]);
        let durable: HashMap<i32, PartEtags> = HashMap::from([
            (
                1,
                PartEtags {
                    original_md5: "b".repeat(32),
                    backend_etag: "dur-1".into(),
                },
            ),
            (
                2,
                PartEtags {
                    original_md5: "c".repeat(32),
                    backend_etag: "dur-2".into(),
                },
            ),
        ]);
        let merged = merge_parts(Some(mem), durable).expect("non-empty merge");
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged.get(&1).expect("part 1").backend_etag,
            "mem-1",
            "in-memory record must win over the durable one"
        );
        assert_eq!(merged.get(&2).expect("part 2").backend_etag, "dur-2");

        // Durable-only (restart case): durable fills everything.
        let durable_only: HashMap<i32, PartEtags> = HashMap::from([(
            3,
            PartEtags {
                original_md5: "d".repeat(32),
                backend_etag: "dur-3".into(),
            },
        )]);
        let merged = merge_parts(None, durable_only).expect("durable-only merge");
        assert_eq!(merged.len(), 1);

        // Both empty ⇒ None (preserves the Option shape Complete
        // branches on).
        assert!(merge_parts(None, HashMap::new()).is_none());
        let empty_mem: Option<HashMap<i32, PartEtags>> = Some(HashMap::new());
        assert_eq!(
            merge_parts(empty_mem, HashMap::new()).map(|m| m.len()),
            Some(0),
            "Some(empty) + empty durable keeps the Some(empty) shape unchanged"
        );
    }
}
