//! Tamper-evident audit-log HMAC chain (v0.5 #31).
//!
//! Extends the v0.4 #20 S3-style access log emitter with a
//! hash-linked HMAC-SHA256 column appended to every line. Each line's
//! HMAC is computed over the previous line's HMAC bytes concatenated
//! with the current line's text (excluding the HMAC field itself):
//!
//! ```text
//! hmac_n = HMAC-SHA256(key, hmac_{n-1} || line_n_without_hmac)
//! ```
//!
//! The genesis HMAC seed is `SHA256("S4-AUDIT-V1")` — a fixed,
//! publicly-known constant that anchors the chain at a deterministic
//! starting point so verifiers don't need to trust the producer about
//! "where the chain started".
//!
//! ## File rotation
//!
//! When the access-log flusher rolls over to a new file (hourly +
//! batch-counter), the new file starts with a comment line:
//!
//! ```text
//! # prev_file_tail=<hex-encoded last_hmac of the previous file>
//! ```
//!
//! The first real entry in the new file uses that tail as its
//! `prev_hmac`, so the chain extends across rotations. A verifier can
//! optionally walk multiple files in chronological order to confirm
//! the cross-file linkage.
//!
//! ## Wire format per entry
//!
//! ```text
//! <existing S3-style access-log line> <hex hmac (64 chars)>\n
//! ```
//!
//! A single trailing space then 64 lowercase hex chars. Existing
//! parsers that split on whitespace see one extra column.
//!
//! ## Key loader
//!
//! `AuditHmacKey::from_str("raw:32-byte-string")`,
//! `"hex:0123...64-char"`, or `"base64:..."` — same shape as
//! `SseKey::from_str` (see `sse.rs`). For very small ops setups, the
//! `raw:` prefix lets you stash the key directly in a CLI flag /
//! systemd unit env var; production should prefer `hex:` or `base64:`
//! delivered out-of-band.
//!
//! ## Verifier CLI
//!
//! `s4 verify-audit-log <FILE> --hmac-key <SPEC>` walks the file,
//! recomputes each line's expected HMAC, and reports the first chain
//! break (if any). Returns `VerifyReport { total_lines, ok_lines,
//! first_break }`. Comment lines (`# prev_file_tail=...`) are honoured
//! as the genesis-prev for the first real entry.
//!
//! ## Limitations (deliberate, v0.5 scope)
//!
//! - Single key, no key rotation — a follow-up issue tracks a key-id
//!   field per line.
//! - In-memory chain state only — if the process restarts mid-hour,
//!   the new flusher loads no state and writes a fresh genesis line at
//!   the top of the next batch file. Verifier handles this by treating
//!   missing `# prev_file_tail=` as "this batch is its own chain".
//! - Verifier only walks one file at a time; cross-file walk is the
//!   operator's responsibility (sort by name, feed one-by-one).

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The fixed genesis seed: `SHA256("S4-AUDIT-V1")`. Computed once at
/// startup; we keep it as a function (not a const) because Sha256 is
/// not const-fn yet.
pub const GENESIS_LABEL: &[u8] = b"S4-AUDIT-V1";

/// Hex-encoded HMAC field length in characters (SHA-256 → 32 bytes →
/// 64 hex chars).
pub const HMAC_HEX_LEN: usize = 64;

/// Comment prefix used to carry the previous file's last HMAC across a
/// rotation boundary.
pub const PREV_TAIL_COMMENT_PREFIX: &str = "# prev_file_tail=";

type HmacSha256 = Hmac<Sha256>;

/// Fixed-length HMAC-SHA256 key. Held inside an `Arc` for cheap
/// sharing across the access-log flusher and any verifier callers.
#[derive(Clone)]
pub struct AuditHmacKey(Arc<Vec<u8>>);

#[derive(Debug, Error)]
pub enum AuditKeyError {
    #[error(
        "audit-log HMAC key spec must start with `raw:`, `hex:`, or `base64:` (got: {0:?})"
    )]
    BadPrefix(String),
    #[error("audit-log HMAC key hex must be even-length and all-hex; got {0}")]
    BadHex(String),
    #[error("audit-log HMAC key base64 decode failed: {0}")]
    BadBase64(String),
    #[error("audit-log HMAC key must be at least 16 bytes after decode (got {0})")]
    TooShort(usize),
}

impl AuditHmacKey {
    /// Parse a key from a CLI-style spec. Three forms:
    ///
    /// - `raw:<utf8 bytes>` — the bytes after the prefix are the key
    ///   verbatim. Useful for tests and small ops; production should
    ///   prefer `hex:` or `base64:`.
    /// - `hex:<hex chars>` — even-length, all-hex.
    /// - `base64:<base64 chars>` — standard base64, padding optional.
    ///
    /// Minimum decoded length: 16 bytes (128 bits). HMAC-SHA256 itself
    /// permits any key length, but anything <16 bytes is operator
    /// error rather than a sound choice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl FromStr for AuditHmacKey {
    type Err = AuditKeyError;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let bytes = if let Some(s) = spec.strip_prefix("raw:") {
            s.as_bytes().to_vec()
        } else if let Some(s) = spec.strip_prefix("hex:") {
            if !s.len().is_multiple_of(2) || !s.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(AuditKeyError::BadHex(s.to_owned()));
            }
            let mut out = Vec::with_capacity(s.len() / 2);
            for i in (0..s.len()).step_by(2) {
                out.push(
                    u8::from_str_radix(&s[i..i + 2], 16)
                        .map_err(|_| AuditKeyError::BadHex(s.to_owned()))?,
                );
            }
            out
        } else if let Some(s) = spec.strip_prefix("base64:") {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s.as_bytes())
                .map_err(|e| AuditKeyError::BadBase64(e.to_string()))?
        } else {
            return Err(AuditKeyError::BadPrefix(spec.to_owned()));
        };
        if bytes.len() < 16 {
            return Err(AuditKeyError::TooShort(bytes.len()));
        }
        Ok(Self(Arc::new(bytes)))
    }
}

impl std::fmt::Debug for AuditHmacKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditHmacKey")
            .field("len", &self.0.len())
            .field("key", &"<redacted>")
            .finish()
    }
}

pub type SharedAuditHmacKey = Arc<AuditHmacKey>;

/// Compute the genesis seed: `SHA256("S4-AUDIT-V1")`. Used as the
/// `prev_hmac` for the very first line in a chain (when no previous
/// file's tail is available).
pub fn genesis_prev() -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(GENESIS_LABEL);
    let out = h.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    buf
}

/// Compute one chain step. Input: previous HMAC bytes + the line text
/// without its HMAC suffix (and without the trailing newline).
/// Output: 32-byte HMAC-SHA256.
pub fn chain_step(key: &AuditHmacKey, prev_hmac: &[u8], line_no_hmac: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes())
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(prev_hmac);
    mac.update(line_no_hmac);
    let out = mac.finalize().into_bytes();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    buf
}

/// Render `bytes` as lowercase hex (no separators).
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Decode a hex string back to bytes. `None` on any non-hex character
/// or odd length.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        out.push(u8::from_str_radix(&s[i..i + 2], 16).ok()?);
    }
    Some(out)
}

/// Result of `verify_audit_log`. `first_break` is `None` when the
/// chain is intact end-to-end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub total_lines: u64,
    pub ok_lines: u64,
    pub first_break: Option<VerifyBreak>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyBreak {
    /// 1-indexed line number within the file (counting all lines,
    /// including comment lines).
    pub line_no: u64,
    /// Hex-encoded HMAC the verifier computed.
    pub expected_hmac: String,
    /// Hex-encoded HMAC the verifier read off the line (or "<missing>"
    /// if the trailing column wasn't present at all).
    pub actual_hmac: String,
}

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("audit-log file {path:?}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("audit-log file {path:?}: prev_file_tail comment had non-hex value: {value:?}")]
    BadPrevTail {
        path: std::path::PathBuf,
        value: String,
    },
}

/// Walk an audit-log file, recomputing each line's HMAC and comparing
/// against the trailing column. Stops at the first break and reports
/// it (subsequent lines are NOT counted as `ok_lines` — they may all
/// be valid, just not chain-linked from where the break is).
///
/// Comment lines (lines starting with `#`) are honoured — specifically
/// `# prev_file_tail=<hex>` resets the running `prev_hmac` to that
/// value before the next non-comment line. Other comment lines are
/// counted but not chain-checked.
///
/// Empty / whitespace-only lines are skipped (counted but neither
/// chain-checked nor flagged).
pub fn verify_audit_log(path: &Path, key: &AuditHmacKey) -> Result<VerifyReport, VerifyError> {
    let raw = std::fs::read(path).map_err(|source| VerifyError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    verify_audit_bytes(path, &raw, key)
}

/// Same as `verify_audit_log` but takes the in-memory bytes directly.
/// Used by the unit tests; the file-path version delegates here after
/// reading.
pub fn verify_audit_bytes(
    path: &Path,
    bytes: &[u8],
    key: &AuditHmacKey,
) -> Result<VerifyReport, VerifyError> {
    let text = std::str::from_utf8(bytes).map_err(|e| VerifyError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })?;

    let mut prev_hmac: [u8; 32] = genesis_prev();
    let mut have_explicit_prev = false;
    let mut total: u64 = 0;
    let mut ok: u64 = 0;

    for (idx, raw_line) in text.split_inclusive('\n').enumerate() {
        total += 1;
        let line_no = (idx + 1) as u64;
        // Strip the trailing newline (and CR, defensively) for
        // chain-step input. We do NOT trim leading whitespace because
        // the access log format starts with `-` deliberately.
        let line = raw_line.trim_end_matches('\n').trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(PREV_TAIL_COMMENT_PREFIX) {
            let hex = rest.trim();
            let bytes = hex_decode(hex).ok_or_else(|| VerifyError::BadPrevTail {
                path: path.to_path_buf(),
                value: hex.to_owned(),
            })?;
            if bytes.len() != 32 {
                return Err(VerifyError::BadPrevTail {
                    path: path.to_path_buf(),
                    value: hex.to_owned(),
                });
            }
            prev_hmac.copy_from_slice(&bytes);
            have_explicit_prev = true;
            continue;
        }
        if line.starts_with('#') {
            // other comment — skip but count.
            continue;
        }
        // Split off the trailing HMAC column.
        let (line_no_hmac, actual_hex) = match split_hmac_suffix(line) {
            Some((body, hmac_hex)) => (body, hmac_hex),
            None => {
                return Ok(VerifyReport {
                    total_lines: total,
                    ok_lines: ok,
                    first_break: Some(VerifyBreak {
                        line_no,
                        expected_hmac: hex_encode(&chain_step(key, &prev_hmac, line.as_bytes())),
                        actual_hmac: "<missing>".to_owned(),
                    }),
                });
            }
        };
        let expected = chain_step(key, &prev_hmac, line_no_hmac.as_bytes());
        let expected_hex = hex_encode(&expected);
        if expected_hex == actual_hex {
            ok += 1;
            prev_hmac = expected;
            have_explicit_prev = true;
        } else {
            return Ok(VerifyReport {
                total_lines: total,
                ok_lines: ok,
                first_break: Some(VerifyBreak {
                    line_no,
                    expected_hmac: expected_hex,
                    actual_hmac: actual_hex.to_owned(),
                }),
            });
        }
    }
    let _ = have_explicit_prev; // reserved for future cross-file walk reporting
    Ok(VerifyReport {
        total_lines: total,
        ok_lines: ok,
        first_break: None,
    })
}

/// Split a chained line into `(body_without_hmac, hmac_hex)`. The
/// HMAC is the last whitespace-separated column and is exactly 64
/// lowercase hex characters. Returns `None` if the line doesn't end
/// with a valid hex column of the expected length.
fn split_hmac_suffix(line: &str) -> Option<(&str, &str)> {
    if line.len() <= HMAC_HEX_LEN + 1 {
        return None;
    }
    let cut = line.len() - HMAC_HEX_LEN;
    let body = &line[..cut];
    let hmac = &line[cut..];
    // body must end with a single space separator.
    if !body.ends_with(' ') {
        return None;
    }
    if hmac.len() != HMAC_HEX_LEN || !hmac.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // Drop the trailing space so the chain input matches the producer's
    // (which appends ` <hex>\n` to the underlying line).
    Some((&body[..body.len() - 1], hmac))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AuditHmacKey {
        AuditHmacKey::from_str("raw:0123456789abcdef0123456789abcdef").unwrap()
    }

    #[test]
    fn genesis_is_sha256_of_label() {
        let g = genesis_prev();
        // SHA-256("S4-AUDIT-V1") — recomputed independently to lock
        // the constant down. Any change to the label is a wire break.
        let mut h = Sha256::new();
        h.update(b"S4-AUDIT-V1");
        let want = h.finalize();
        assert_eq!(&g[..], &want[..]);
    }

    #[test]
    fn key_parsing_accepts_three_prefixes() {
        let r = AuditHmacKey::from_str("raw:0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(r.as_bytes().len(), 32);
        let h = AuditHmacKey::from_str(
            "hex:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        assert_eq!(h.as_bytes().len(), 32);
        // 32 zero bytes -> base64 "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        let b = AuditHmacKey::from_str("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .unwrap();
        assert_eq!(b.as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn key_parsing_rejects_short_keys() {
        let err = AuditHmacKey::from_str("raw:short").unwrap_err();
        assert!(matches!(err, AuditKeyError::TooShort(5)));
    }

    #[test]
    fn key_parsing_rejects_bad_prefix() {
        let err = AuditHmacKey::from_str("plain:key").unwrap_err();
        assert!(matches!(err, AuditKeyError::BadPrefix(_)));
    }

    #[test]
    fn happy_path_chain_verifies() {
        let key = key();
        // Build a 3-line file by hand.
        let lines = ["line one alpha", "line two beta", "line three gamma"];
        let mut buf = String::new();
        let mut prev = genesis_prev();
        for ln in &lines {
            let mac = chain_step(&key, &prev, ln.as_bytes());
            buf.push_str(ln);
            buf.push(' ');
            buf.push_str(&hex_encode(&mac));
            buf.push('\n');
            prev = mac;
        }
        let report =
            verify_audit_bytes(std::path::Path::new("<mem>"), buf.as_bytes(), &key).unwrap();
        assert_eq!(report.total_lines, 3);
        assert_eq!(report.ok_lines, 3);
        assert!(report.first_break.is_none());
    }

    #[test]
    fn tamper_one_byte_in_middle_breaks_at_that_line() {
        let key = key();
        let lines = ["line A", "line B middle", "line C tail"];
        let mut buf = String::new();
        let mut prev = genesis_prev();
        for ln in &lines {
            let mac = chain_step(&key, &prev, ln.as_bytes());
            buf.push_str(ln);
            buf.push(' ');
            buf.push_str(&hex_encode(&mac));
            buf.push('\n');
            prev = mac;
        }
        // Flip one character in the middle of line 2's body.
        let bad = buf.replace("middle", "MIDDLE");
        let report =
            verify_audit_bytes(std::path::Path::new("<mem>"), bad.as_bytes(), &key).unwrap();
        assert!(report.first_break.is_some(), "expected a break");
        let br = report.first_break.unwrap();
        assert_eq!(br.line_no, 2, "break should be on line 2");
        assert_eq!(report.ok_lines, 1, "line 1 OK before the break");
    }

    #[test]
    fn tamper_hmac_field_breaks_at_that_line() {
        let key = key();
        let line = "lonely line";
        let mac = chain_step(&key, &genesis_prev(), line.as_bytes());
        let s = format!("{} {}\n", line, hex_encode(&mac));
        // Flip a hex char in the HMAC suffix (penultimate byte; final
        // byte is '\n').
        let last = s.len() - 2;
        let c = s.as_bytes()[last];
        let new_c = if c == b'0' { '1' } else { '0' };
        let mut bad = String::with_capacity(s.len());
        bad.push_str(&s[..last]);
        bad.push(new_c);
        bad.push_str(&s[last + 1..]);
        let report =
            verify_audit_bytes(std::path::Path::new("<mem>"), bad.as_bytes(), &key).unwrap();
        let br = report.first_break.expect("expected break");
        assert_eq!(br.line_no, 1);
        // Actual byte was flipped, so c is unchanged in `bad`.
        let _ = c;
    }

    #[test]
    fn missing_hmac_column_reports_break_with_missing_marker() {
        let key = key();
        let s = "no hmac at all\n";
        let report =
            verify_audit_bytes(std::path::Path::new("<mem>"), s.as_bytes(), &key).unwrap();
        let br = report.first_break.expect("expected break");
        assert_eq!(br.actual_hmac, "<missing>");
    }

    #[test]
    fn cross_file_chain_via_prev_tail_comment() {
        let key = key();
        // First "file": one line, capture its tail.
        let line1 = "first file lone line";
        let mac1 = chain_step(&key, &genesis_prev(), line1.as_bytes());
        let f1 = format!("{} {}\n", line1, hex_encode(&mac1));
        let r1 =
            verify_audit_bytes(std::path::Path::new("<f1>"), f1.as_bytes(), &key).unwrap();
        assert!(r1.first_break.is_none());

        // Second "file": prev_file_tail comment, then one line whose
        // HMAC is computed from mac1 as its prev.
        let line2 = "second file lone line";
        let mac2 = chain_step(&key, &mac1, line2.as_bytes());
        let f2 = format!(
            "# prev_file_tail={}\n{} {}\n",
            hex_encode(&mac1),
            line2,
            hex_encode(&mac2)
        );
        let r2 =
            verify_audit_bytes(std::path::Path::new("<f2>"), f2.as_bytes(), &key).unwrap();
        assert!(r2.first_break.is_none(), "cross-file chain must verify");
        assert_eq!(r2.ok_lines, 1);
        assert_eq!(r2.total_lines, 2); // comment + entry
    }

    #[test]
    fn cross_file_chain_with_wrong_prev_tail_breaks() {
        let key = key();
        let line2 = "second file lone line";
        // Wrong prev: 32 zero bytes
        let wrong_prev = [0u8; 32];
        // But the producer wrote the HMAC computed from genesis (or
        // anything other than wrong_prev), so the verifier's recompute
        // will mismatch.
        let actual_mac = chain_step(&key, &genesis_prev(), line2.as_bytes());
        let f2 = format!(
            "# prev_file_tail={}\n{} {}\n",
            hex_encode(&wrong_prev),
            line2,
            hex_encode(&actual_mac)
        );
        let r =
            verify_audit_bytes(std::path::Path::new("<f2>"), f2.as_bytes(), &key).unwrap();
        assert!(r.first_break.is_some());
    }

    #[test]
    fn split_hmac_suffix_basic() {
        let hmac64 = "a".repeat(64);
        let s = format!("foo bar baz {hmac64}");
        let (body, hmac) = split_hmac_suffix(&s).unwrap();
        assert_eq!(body, "foo bar baz");
        assert_eq!(hmac.len(), 64);
        assert_eq!(hmac, hmac64.as_str());
    }

    #[test]
    fn split_hmac_suffix_rejects_short_or_nonhex() {
        assert!(split_hmac_suffix("short").is_none());
        // 64 chars but contains 'g' (not hex) — produce a 64-char
        // non-hex suffix to keep the length right.
        let bad_hmac = "g".repeat(64);
        let bad = format!("x {bad_hmac}");
        assert!(split_hmac_suffix(&bad).is_none());
    }

    #[test]
    fn hex_roundtrip() {
        let raw = [0u8, 1, 2, 0xff, 0x10, 0xab];
        let s = hex_encode(&raw);
        assert_eq!(s, "000102ff10ab");
        let dec = hex_decode(&s).unwrap();
        assert_eq!(dec, raw);
    }
}
