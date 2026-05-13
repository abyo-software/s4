//! SigV4a (AWS asymmetric ECDSA-P256) signature verification (v0.5 #33).
//!
//! S4 already accepts AWS SigV4 (HMAC-SHA256) via the underlying `s3s`
//! framework. SigV4a is the asymmetric, **region-agnostic** variant that
//! AWS uses by default for S3 Multi-Region Access Points and a handful
//! of newer Lambda runtimes. The two are wire-distinguished by the
//! `Authorization` header prefix:
//!
//! - SigV4 : `AWS4-HMAC-SHA256 Credential=..., ...`
//! - SigV4a: `AWS4-ECDSA-P256-SHA256 Credential=..., ...`
//!
//! SigV4a additionally requires the request to carry an
//! `X-Amz-Region-Set` header listing the region(s) the signature is
//! valid for (`*` for "any region"). The signature itself is an
//! ECDSA-P-256 signature (DER-encoded, as defined by AWS) over the
//! same canonical-request bytes that the framework already builds for
//! SigV4 — only the algorithm is swapped.
//!
//! # Scope (v0.5 #33)
//!
//! This module provides only the **verification primitives** needed to
//! plug into the S3 service path: parse the `Authorization` header,
//! detect whether a request claims SigV4a, and verify a P-256 signature
//! given a pre-loaded ECDSA verifying key. Issuing fresh SigV4a
//! credentials (which would mean running an internal AWS-style trusted
//! key derivation service) is **explicitly out of scope**; operators
//! configure a directory of PEM-encoded P-256 public keys via
//! `--sigv4a-credentials <DIR>` and S4 trusts whatever lands there.
//!
//! # Wire details we care about
//!
//! - Algorithm token: `AWS4-ECDSA-P256-SHA256`
//! - Credential scope: `<access-key-id>/<date>/<service>/aws4_request`
//!   (no region — region is in the request header instead)
//! - Region set header: `X-Amz-Region-Set: us-east-1,us-west-2` or `*`
//! - Signature: lowercase-hex DER-encoded ECDSA-P256 signature

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use thiserror::Error;

/// HTTP `Authorization` header prefix that identifies a SigV4a request.
pub const SIGV4A_ALGORITHM: &str = "AWS4-ECDSA-P256-SHA256";

/// Header that lists the region(s) the SigV4a signature is valid for.
/// Comma-separated list of region names, or the wildcard `*`.
pub const REGION_SET_HEADER: &str = "x-amz-region-set";

/// Errors surfaced by [`verify`] / [`SigV4aCredentialStore::load_dir`].
#[derive(Debug, Error)]
pub enum SigV4aError {
    /// The DER-encoded ECDSA signature failed to parse.
    #[error("malformed ECDSA-P256 signature: {0}")]
    BadSignature(String),
    /// The signature parsed but did not verify against the supplied key.
    #[error("ECDSA-P256 signature verification failed")]
    VerificationFailed,
    /// `requested_region` is not a member of the request's region set.
    #[error("region '{requested}' not in signed region-set '{set}'")]
    RegionMismatch { requested: String, set: String },
    /// PEM file did not contain a P-256 SubjectPublicKeyInfo.
    #[error("invalid P-256 public key in '{path}': {reason}")]
    BadPublicKey { path: String, reason: String },
    /// I/O error while loading a credential directory.
    #[error("credential store I/O for '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    // ------------------------------------------------------------------
    // v0.8.4 #76 — replay protection (audit H-6).
    // ------------------------------------------------------------------
    /// `x-amz-date` header is missing on a SigV4a request. Without it
    /// the gate cannot enforce a freshness window, so the request is
    /// rejected outright (matches AWS S3 behaviour).
    #[error("missing x-amz-date header (required for SigV4a)")]
    MissingXAmzDate,
    /// `x-amz-date` is not in the AWS canonical `YYYYMMDDTHHMMSSZ`
    /// format, or the credential-scope date is not 8 ASCII digits.
    #[error("x-amz-date format must be YYYYMMDDTHHMMSSZ")]
    InvalidDateFormat,
    /// The request timestamp is outside the configured skew window
    /// (`--sigv4a-skew-tolerance-seconds`, default 900s = 15min).
    /// Maps to HTTP 403 `RequestTimeTooSkewed` per AWS S3 spec.
    #[error("request time too skewed: {drift_secs}s drift, tolerance {tolerance_secs}s")]
    RequestTimeTooSkewed {
        drift_secs: i64,
        tolerance_secs: i64,
    },
    /// `x-amz-date` date portion (first 8 chars) does not match the
    /// date in the credential scope. Defends against replays whose
    /// scope and timestamp were mixed and matched.
    #[error("x-amz-date date does not match credential scope date")]
    DateScopeMismatch,
    /// `x-amz-date` is not listed in `SignedHeaders=` so the gate
    /// cannot trust that the timestamp was actually covered by the
    /// signature — must be present in the signed-headers list.
    #[error("x-amz-date must be in SignedHeaders list")]
    XAmzDateNotSigned,
    /// Credential-scope terminator (the trailing component) is not the
    /// literal string `aws4_request` AWS mandates.
    #[error("credential scope must end with /aws4_request")]
    InvalidTerminator,
    /// Credential-scope service component is not the literal `s3`.
    /// Defends against replaying a SigV4a signature scoped to e.g.
    /// `ec2` or `lambda` against this S3 listener.
    #[error("credential scope service must be 's3', got {got:?}")]
    WrongService { got: String },
    /// Credential string doesn't have the right number of `/`-separated
    /// components for SigV4a (`<access-key>/<date>/<service>/aws4_request`,
    /// 4 components total).
    #[error("credential scope must have 4 components separated by '/'")]
    InvalidCredentialScope,
}

/// Newtype wrapper around the bytes that the SigV4a signature was
/// computed over. SigV4a signs the same canonical-request bytes that
/// SigV4 does — only the algorithm differs — so callers that already
/// build a SigV4 canonical request can wrap those bytes here without
/// rebuilding them.
///
/// Keeping this as a distinct type (rather than `&[u8]`) makes it
/// obvious at every call-site which byte stream is being signed and
/// avoids accidentally passing, say, the raw HTTP body.
pub struct CanonicalRequest<'a> {
    bytes: &'a [u8],
}

impl<'a> CanonicalRequest<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes
    }
}

/// Parsed `Authorization: AWS4-ECDSA-P256-SHA256 ...` header.
///
/// v0.8.4 #76 (audit H-6): the credential scope is now broken out into
/// typed fields (`date`, `service`, `terminator`) so the verifier can
/// cross-check against the request's `x-amz-date` and reject malformed
/// or off-service scopes (`/ec2/aws4_request`, etc.) before any ECDSA
/// math runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigV4aAuth {
    /// AWS access key id (the `Credential=<key>/...` first segment).
    pub access_key_id: String,
    /// Credential-scope date — `YYYYMMDD`, 8 ASCII digits.
    pub date: String,
    /// Credential-scope service — must be the literal `"s3"` for
    /// requests targeting this listener.
    pub service: String,
    /// Credential-scope terminator — must be the literal
    /// `"aws4_request"` per AWS spec.
    pub terminator: String,
    /// Credential scope path elements after the access-key-id, e.g.
    /// `["20260513", "s3", "aws4_request"]`. SigV4a omits the region
    /// (it lives in `X-Amz-Region-Set`), so this slice is one element
    /// shorter than the SigV4 equivalent. Retained alongside the typed
    /// fields above for callers that prefer the slice form.
    pub credential_scope: Vec<String>,
    /// The list of header names that participated in the canonical
    /// request, lowercase, in the order signed.
    pub signed_headers: Vec<String>,
    /// DER-encoded ECDSA signature, decoded from the lowercase-hex
    /// representation in the header.
    pub signature_der: Vec<u8>,
}

/// Parse an `Authorization` header value as a SigV4a credential.
///
/// Returns `Ok(SigV4aAuth)` on a fully-valid SigV4a header. Returns
/// `Err(SigV4aError)` when the header begins with the SigV4a algorithm
/// token but is malformed in some recoverable way the caller should
/// surface as a 400 / 403 (e.g. wrong service, malformed date).
/// Returns `Err(SigV4aError::BadSignature(...))` for non-SigV4a
/// headers (callers typically test [`detect`] first and should treat
/// this as "not our request").
///
/// v0.8.4 #76 (audit H-6): credential-scope shape is now strictly
/// validated. Previously any `<key>/<...>/<...>/<...>` shape parsed,
/// which let attackers replay a captured SigV4a signature scoped to
/// e.g. `lambda` against this S3 listener.
pub fn parse_authorization_header(header: &str) -> Result<SigV4aAuth, SigV4aError> {
    let rest = header
        .trim()
        .strip_prefix(SIGV4A_ALGORITHM)
        .ok_or_else(|| SigV4aError::BadSignature("not a SigV4a Authorization header".into()))?;
    let rest = rest.trim_start();

    let mut credential: Option<&str> = None;
    let mut signed_headers: Option<&str> = None;
    let mut signature: Option<&str> = None;

    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = Some(v);
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed_headers = Some(v);
        } else if let Some(v) = part.strip_prefix("Signature=") {
            signature = Some(v);
        }
    }

    let cred =
        credential.ok_or_else(|| SigV4aError::BadSignature("missing Credential= field".into()))?;
    // SigV4a credential format: `<access-key>/<date>/<service>/aws4_request`
    // (4 slash-separated components). The region lives in the
    // `X-Amz-Region-Set` header, NOT in the credential scope, which is
    // what differentiates SigV4a from SigV4 here.
    let scope_parts: Vec<&str> = cred.split('/').collect();
    if scope_parts.len() != 4 {
        return Err(SigV4aError::InvalidCredentialScope);
    }
    let access_key_id = scope_parts[0].to_owned();
    let date = scope_parts[1].to_owned();
    let service = scope_parts[2].to_owned();
    let terminator = scope_parts[3].to_owned();
    if access_key_id.is_empty() {
        return Err(SigV4aError::InvalidCredentialScope);
    }
    if date.len() != 8 || !date.chars().all(|c| c.is_ascii_digit()) {
        return Err(SigV4aError::InvalidDateFormat);
    }
    if service != "s3" {
        return Err(SigV4aError::WrongService { got: service });
    }
    if terminator != "aws4_request" {
        return Err(SigV4aError::InvalidTerminator);
    }
    let credential_scope: Vec<String> = scope_parts[1..].iter().map(|s| (*s).to_owned()).collect();

    let signed_headers_raw = signed_headers
        .ok_or_else(|| SigV4aError::BadSignature("missing SignedHeaders= field".into()))?;
    let signed_headers: Vec<String> = signed_headers_raw
        .split(';')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if signed_headers.is_empty() {
        return Err(SigV4aError::BadSignature(
            "empty SignedHeaders= list".into(),
        ));
    }

    let signature_hex =
        signature.ok_or_else(|| SigV4aError::BadSignature("missing Signature= field".into()))?;
    let signature_der = decode_hex(signature_hex)
        .ok_or_else(|| SigV4aError::BadSignature("non-hex Signature= value".into()))?;

    Ok(SigV4aAuth {
        access_key_id,
        date,
        service,
        terminator,
        credential_scope,
        signed_headers,
        signature_der,
    })
}

/// Returns `true` iff the request claims to be a SigV4a request, i.e.
/// either its `Authorization` header begins with the SigV4a algorithm
/// token, or it carries the `X-Amz-Region-Set` header (which only
/// SigV4a clients emit).
///
/// Generic over the body type so callers don't have to choose a
/// particular `hyper::body` flavor; the body bytes are not inspected.
pub fn detect<B>(req: &http::Request<B>) -> bool {
    let h = req.headers();
    if let Some(auth) = h
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && auth.trim_start().starts_with(SIGV4A_ALGORITHM)
    {
        return true;
    }
    h.contains_key(REGION_SET_HEADER)
}

/// Verify an ECDSA-P256-SHA256 signature over a SigV4a canonical
/// request, additionally enforcing that `requested_region` is a member
/// of the signed `region_set` (the comma-separated value of
/// `X-Amz-Region-Set`, or `*` for "any region").
///
/// The signature **must** be DER-encoded — that's the format AWS SDKs
/// emit and the format [`parse_authorization_header`] returns.
///
/// # Errors
/// - [`SigV4aError::RegionMismatch`] — `requested_region` is not in
///   `region_set`.
/// - [`SigV4aError::BadSignature`] — the signature failed to parse as
///   a DER ECDSA signature.
/// - [`SigV4aError::VerificationFailed`] — the signature parsed but
///   does not match the canonical-request bytes under the supplied
///   public key.
pub fn verify(
    request_bytes: &CanonicalRequest<'_>,
    signature: &[u8],
    pubkey: &VerifyingKey,
    region_set: &str,
    requested_region: &str,
) -> Result<(), SigV4aError> {
    if !region_set_contains(region_set, requested_region) {
        return Err(SigV4aError::RegionMismatch {
            requested: requested_region.to_owned(),
            set: region_set.to_owned(),
        });
    }
    let sig =
        Signature::from_der(signature).map_err(|e| SigV4aError::BadSignature(e.to_string()))?;
    pubkey
        .verify(request_bytes.as_bytes(), &sig)
        .map_err(|_| SigV4aError::VerificationFailed)
}

/// v0.8.4 #76 (audit H-6): full SigV4a request verification with
/// **replay protection**. Builds on the lower-level [`verify`] above
/// by additionally enforcing every constraint AWS requires for a
/// SigV4a request to be considered "fresh and well-scoped":
///
/// 1. `x-amz-date` header **MUST** be present (no fallback to the
///    legacy `Date` header — modern AWS SDKs always send `x-amz-date`).
/// 2. `x-amz-date` MUST parse as `YYYYMMDDTHHMMSSZ`.
/// 3. `|now − x-amz-date| <= skew_tolerance` (default 15 min, AWS spec).
/// 4. The first 8 chars of `x-amz-date` MUST match the credential
///    scope `date` field (defends against scope/timestamp mix-and-match).
/// 5. `x-amz-date` MUST appear in the `SignedHeaders=` list (so the
///    signature actually covers the timestamp the gate just checked).
/// 6. The ECDSA-P256 signature MUST verify and the requested region
///    MUST be in the signed region-set (legacy [`verify`] checks).
///
/// `headers` is a flat lowercase-name → value map of the request's
/// HTTP headers. Callers building this from a `http::HeaderMap` must
/// lowercase the names; the function does case-insensitive lookup of
/// `x-amz-date` only.
///
/// # Errors
///
/// Each check has a dedicated `SigV4aError` variant — see [`SigV4aError`]
/// docs for the per-variant HTTP-status mapping the gate uses.
#[allow(clippy::too_many_arguments)]
// 8 args: parsed scope + headers + canonical bytes + key + region pair
// + now + tolerance. Splitting into a builder struct would just push
// the cohesion sideways without helping the call site (the gate is the
// only caller and threads each through directly).
pub fn verify_request(
    parsed: &SigV4aAuth,
    headers: &HashMap<String, String>,
    canonical_request_bytes: &[u8],
    pubkey: &VerifyingKey,
    region_set: &str,
    requested_region: &str,
    now: chrono::DateTime<chrono::Utc>,
    skew_tolerance: chrono::Duration,
) -> Result<(), SigV4aError> {
    // (1) x-amz-date present?
    let x_amz_date = lookup_header_ci(headers, "x-amz-date").ok_or(SigV4aError::MissingXAmzDate)?;
    // (2) format check — AWS canonical: YYYYMMDDTHHMMSSZ (16 chars).
    if x_amz_date.len() != 16 || !x_amz_date.ends_with('Z') {
        return Err(SigV4aError::InvalidDateFormat);
    }
    let request_time = chrono::NaiveDateTime::parse_from_str(x_amz_date, "%Y%m%dT%H%M%SZ")
        .map_err(|_| SigV4aError::InvalidDateFormat)?
        .and_utc();

    // (3) skew window. Compute drift as an absolute `chrono::Duration`
    // and compare to the operator-supplied tolerance. Using
    // `Duration::abs()` (>= 0.4.31) keeps the math overflow-safe for
    // the year-range we care about.
    let drift = (now - request_time).abs();
    if drift > skew_tolerance {
        return Err(SigV4aError::RequestTimeTooSkewed {
            drift_secs: drift.num_seconds(),
            tolerance_secs: skew_tolerance.num_seconds(),
        });
    }

    // (4) date portion must match credential scope.
    if x_amz_date[..8] != parsed.date {
        return Err(SigV4aError::DateScopeMismatch);
    }

    // (5) x-amz-date must be in SignedHeaders.
    if !parsed
        .signed_headers
        .iter()
        .any(|h| h.eq_ignore_ascii_case("x-amz-date"))
    {
        return Err(SigV4aError::XAmzDateNotSigned);
    }

    // (6) signature + region check via the existing verifier.
    verify(
        &CanonicalRequest::new(canonical_request_bytes),
        &parsed.signature_der,
        pubkey,
        region_set,
        requested_region,
    )
}

/// Case-insensitive header lookup against a flat
/// `HashMap<lowercase-name, value>`. The map is canonicalised by the
/// caller (the `routing` middleware lowercases all header names when
/// it snapshots them); this helper exists so unit tests can pass an
/// arbitrarily-cased map and still get a hit.
fn lookup_header_ci<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a String> {
    let needle = name.to_ascii_lowercase();
    if let Some(v) = headers.get(&needle) {
        return Some(v);
    }
    // Fallback for callers that didn't pre-lowercase the keys.
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// Returns `true` iff `region` is listed in the comma-separated
/// `region_set`. `*` matches any region (AWS Multi-Region Access
/// Points use this); empty `region` is never matched.
#[must_use]
pub fn region_set_contains(region_set: &str, region: &str) -> bool {
    if region.is_empty() {
        return false;
    }
    region_set
        .split(',')
        .map(str::trim)
        .any(|item| item == "*" || item.eq_ignore_ascii_case(region))
}

/// In-memory map from AWS access-key-id to its trusted ECDSA P-256
/// verifying key. Populated at boot from a directory of PEM files
/// (`--sigv4a-credentials <DIR>`); each file is `<access_key_id>.pem`
/// containing a SubjectPublicKeyInfo P-256 public key.
///
/// Cheap to clone — the inner map sits behind `Arc`, so handler code
/// can pass a `SharedSigV4aCredentialStore` around without copying.
#[derive(Debug, Default, Clone)]
pub struct SigV4aCredentialStore {
    keys: Arc<HashMap<String, VerifyingKey>>,
}

/// Convenience alias used at the service-builder boundary.
pub type SharedSigV4aCredentialStore = Arc<SigV4aCredentialStore>;

impl SigV4aCredentialStore {
    /// Build a store directly from an `(access_key_id, key)` map.
    /// Mostly useful for tests; production code uses [`Self::load_dir`].
    #[must_use]
    pub fn from_map(map: HashMap<String, VerifyingKey>) -> Self {
        Self {
            keys: Arc::new(map),
        }
    }

    /// Look up the verifying key for an access-key-id. Returns `None`
    /// for unknown keys (callers must reject the request, e.g. with
    /// `InvalidAccessKeyId`).
    #[must_use]
    pub fn get(&self, access_key_id: &str) -> Option<&VerifyingKey> {
        self.keys.get(access_key_id)
    }

    /// Number of keys currently loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// `true` iff no keys are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Load every `*.pem` file under `dir` as `<file-stem>` -> P-256
    /// public key. Files that don't end in `.pem` are skipped silently;
    /// files that look like PEM but don't parse as a P-256 SPKI surface
    /// a [`SigV4aError::BadPublicKey`] error so the operator notices a
    /// mis-installed credential at boot rather than at first request.
    pub fn load_dir(dir: impl AsRef<Path>) -> Result<Self, SigV4aError> {
        let dir = dir.as_ref();
        let read = fs::read_dir(dir).map_err(|source| SigV4aError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let mut keys: HashMap<String, VerifyingKey> = HashMap::new();
        for entry in read {
            let entry = entry.map_err(|source| SigV4aError::Io {
                path: dir.display().to_string(),
                source,
            })?;
            let path: PathBuf = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pem") {
                continue;
            }
            let access_key_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if !s.is_empty() => s.to_owned(),
                _ => continue,
            };
            let pem = fs::read_to_string(&path).map_err(|source| SigV4aError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let key =
                parse_p256_public_key_pem(&pem).map_err(|reason| SigV4aError::BadPublicKey {
                    path: path.display().to_string(),
                    reason,
                })?;
            keys.insert(access_key_id, key);
        }
        Ok(Self {
            keys: Arc::new(keys),
        })
    }
}

/// Parse a PEM-encoded P-256 public key (`-----BEGIN PUBLIC KEY-----`,
/// SubjectPublicKeyInfo). Returns the [`VerifyingKey`] on success.
fn parse_p256_public_key_pem(pem: &str) -> Result<VerifyingKey, String> {
    use p256::pkcs8::DecodePublicKey;
    VerifyingKey::from_public_key_pem(pem.trim()).map_err(|e| e.to_string())
}

/// Lowercase / uppercase hex decode. Returns `None` on any non-hex
/// character or odd length. We avoid pulling in a `hex` crate since
/// the SigV4a signature decode is the only consumer and `p256` already
/// drags in plenty of indirect deps.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use p256::ecdsa::SigningKey;
    use p256::ecdsa::signature::Signer;
    use rand::rngs::OsRng;

    fn lower_hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// AWS-style sample SigV4a Authorization header (mirrors the
    /// public docs example for an MRAP request, but with a synthetic
    /// signature — we only test header parsing here, not verify).
    #[test]
    fn parse_authorization_header_aws_sample() {
        // 64-byte DER-ish hex blob (parser doesn't validate DER, just hex).
        let sig_hex = "30440220".to_owned() + &"ab".repeat(32) + &"cd".repeat(34);
        // pad/truncate to even length below 1k just to look like real
        let sig_hex = &sig_hex[..sig_hex.len() & !1];
        let header = format!(
            "AWS4-ECDSA-P256-SHA256 \
             Credential=AKIAEXAMPLEKEYID/20260513/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-region-set, \
             Signature={sig_hex}"
        );
        let parsed = parse_authorization_header(&header).expect("parses");
        assert_eq!(parsed.access_key_id, "AKIAEXAMPLEKEYID");
        assert_eq!(
            parsed.credential_scope,
            vec!["20260513", "s3", "aws4_request"],
        );
        assert_eq!(
            parsed.signed_headers,
            vec![
                "host",
                "x-amz-content-sha256",
                "x-amz-date",
                "x-amz-region-set"
            ],
        );
        assert_eq!(parsed.signature_der, decode_hex(sig_hex).unwrap());
    }

    #[test]
    fn parse_authorization_header_rejects_sigv4_hmac() {
        // Plain SigV4 (HMAC-SHA256) Authorization header must NOT parse
        // as SigV4a — that's how `detect` keeps the two paths separate.
        // v0.8.4 #76: parse now returns `Err(BadSignature(..))` instead
        // of `None`; either shape signals "not for us" to the caller.
        let header = "AWS4-HMAC-SHA256 \
             Credential=AKIA/20260513/us-east-1/s3/aws4_request, \
             SignedHeaders=host, \
             Signature=deadbeef";
        let err = parse_authorization_header(header).expect_err("not SigV4a");
        assert!(matches!(err, SigV4aError::BadSignature(_)));
    }

    #[test]
    fn parse_authorization_header_rejects_missing_fields() {
        let header = "AWS4-ECDSA-P256-SHA256 Credential=AKIA/20260513/s3/aws4_request, \
                      SignedHeaders=host";
        let err = parse_authorization_header(header).expect_err("missing Signature=");
        assert!(matches!(err, SigV4aError::BadSignature(_)));
    }

    #[test]
    fn detect_picks_up_sigv4a_authorization_header() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/bucket/key")
            .header(
                "authorization",
                "AWS4-ECDSA-P256-SHA256 Credential=A/20260513/s3/aws4_request, \
                 SignedHeaders=host, Signature=ab",
            )
            .body(())
            .unwrap();
        assert!(detect(&req));
    }

    #[test]
    fn detect_picks_up_region_set_header() {
        // Some clients (ancient SDKs that pre-stamp the region-set
        // header before swapping the algorithm in) may carry the
        // region-set header without yet having flipped the algorithm
        // string. We treat this as "claims SigV4a" so the verifier
        // gets a chance to reject it cleanly with an auth error
        // instead of dropping it on the SigV4 floor.
        let req = http::Request::builder()
            .method("GET")
            .uri("/bucket/key")
            .header("authorization", "AWS4-HMAC-SHA256 ...")
            .header(REGION_SET_HEADER, "us-east-1,us-west-2")
            .body(())
            .unwrap();
        assert!(detect(&req));
    }

    #[test]
    fn detect_ignores_plain_sigv4() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/bucket/key")
            .header(
                "authorization",
                "AWS4-HMAC-SHA256 Credential=A/20260513/us-east-1/s3/aws4_request, \
                 SignedHeaders=host, Signature=ab",
            )
            .body(())
            .unwrap();
        assert!(!detect(&req));
    }

    #[test]
    fn region_set_membership() {
        assert!(region_set_contains("us-east-1,us-west-2", "us-east-1"));
        assert!(region_set_contains("us-east-1,us-west-2", "us-west-2"));
        assert!(region_set_contains("*", "ap-northeast-1"));
        assert!(region_set_contains("us-east-1, us-west-2", "us-west-2"));
        // case-insensitive on the region (AWS regions are lowercase
        // in practice, but the SDK occasionally emits mixed case).
        assert!(region_set_contains("us-east-1", "US-EAST-1"));
    }

    #[test]
    fn region_set_non_member_rejected() {
        assert!(!region_set_contains("us-east-1,us-west-2", "eu-west-1"));
        assert!(!region_set_contains("", "us-east-1"));
        assert!(!region_set_contains("us-east-1", ""));
    }

    /// Happy path — sign with a freshly generated P-256 key, verify
    /// with the corresponding public key. Exercises the full
    /// `verify` API including the region-set check.
    #[test]
    fn ecdsa_p256_sign_then_verify_ok() {
        let signing_key = SigningKey::random(&mut OsRng);
        let verifying_key = VerifyingKey::from(&signing_key);
        let canonical = b"AWS4-ECDSA-P256-SHA256\n20260513T120000Z\n\
                          20260513/s3/aws4_request\n\
                          deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let sig: Signature = signing_key.sign(canonical);
        let der = sig.to_der().as_bytes().to_vec();

        verify(
            &CanonicalRequest::new(canonical),
            &der,
            &verifying_key,
            "us-east-1,us-west-2",
            "us-east-1",
        )
        .expect("happy-path verify must succeed");
    }

    #[test]
    fn ecdsa_p256_verify_wildcard_region() {
        let signing_key = SigningKey::random(&mut OsRng);
        let verifying_key = VerifyingKey::from(&signing_key);
        let canonical = b"canonical-request-bytes";
        let sig: Signature = signing_key.sign(canonical);
        let der = sig.to_der().as_bytes().to_vec();
        verify(
            &CanonicalRequest::new(canonical),
            &der,
            &verifying_key,
            "*",
            "ap-northeast-1",
        )
        .expect("wildcard region should match anything");
    }

    #[test]
    fn ecdsa_p256_verify_region_mismatch() {
        let signing_key = SigningKey::random(&mut OsRng);
        let verifying_key = VerifyingKey::from(&signing_key);
        let canonical = b"canonical";
        let sig: Signature = signing_key.sign(canonical);
        let der = sig.to_der().as_bytes().to_vec();
        let err = verify(
            &CanonicalRequest::new(canonical),
            &der,
            &verifying_key,
            "us-east-1,us-west-2",
            "eu-west-1",
        )
        .expect_err("region mismatch must reject");
        assert!(matches!(err, SigV4aError::RegionMismatch { .. }));
    }

    /// Tamper one byte of the signed payload — the signature must no
    /// longer verify. This is the core integrity guarantee SigV4a is
    /// supposed to give us.
    #[test]
    fn ecdsa_p256_verify_tamper_one_byte_fails() {
        let signing_key = SigningKey::random(&mut OsRng);
        let verifying_key = VerifyingKey::from(&signing_key);
        let canonical = b"canonical-request-bytes-original";
        let sig: Signature = signing_key.sign(canonical);
        let der = sig.to_der().as_bytes().to_vec();

        // Flip one byte and re-verify — must fail.
        let mut tampered = canonical.to_vec();
        tampered[0] ^= 0x01;
        let err = verify(
            &CanonicalRequest::new(&tampered),
            &der,
            &verifying_key,
            "*",
            "us-east-1",
        )
        .expect_err("tampered payload must not verify");
        assert!(matches!(err, SigV4aError::VerificationFailed));
    }

    #[test]
    fn ecdsa_p256_verify_malformed_signature() {
        let signing_key = SigningKey::random(&mut OsRng);
        let verifying_key = VerifyingKey::from(&signing_key);
        let err = verify(
            &CanonicalRequest::new(b"x"),
            b"\x00\x01not-a-der-sig",
            &verifying_key,
            "*",
            "us-east-1",
        )
        .expect_err("malformed signature must not verify");
        assert!(matches!(err, SigV4aError::BadSignature(_)));
    }

    #[test]
    fn hex_decode_rejects_invalid() {
        assert_eq!(decode_hex("00ff"), Some(vec![0x00, 0xff]));
        assert_eq!(decode_hex("ABcd"), Some(vec![0xab, 0xcd]));
        assert!(decode_hex("0").is_none()); // odd length
        assert!(decode_hex("zz").is_none()); // non-hex
    }

    #[test]
    fn credential_store_from_map_lookup() {
        let signing = SigningKey::random(&mut OsRng);
        let verifying = VerifyingKey::from(&signing);
        let mut m = HashMap::new();
        m.insert("AKIATEST".to_owned(), verifying);
        let store = SigV4aCredentialStore::from_map(m);
        assert_eq!(store.len(), 1);
        assert!(!store.is_empty());
        assert!(store.get("AKIATEST").is_some());
        assert!(store.get("UNKNOWN").is_none());
    }

    #[test]
    fn credential_store_load_dir_pem() {
        use p256::pkcs8::EncodePublicKey;
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tmp");
        // Write two PEM keys + one .txt that should be ignored.
        for id in ["AKIA1", "AKIA2"] {
            let signing = SigningKey::random(&mut OsRng);
            let verifying = VerifyingKey::from(&signing);
            let pem = verifying
                .to_public_key_pem(p256::pkcs8::LineEnding::LF)
                .unwrap();
            let mut f = std::fs::File::create(dir.path().join(format!("{id}.pem"))).unwrap();
            f.write_all(pem.as_bytes()).unwrap();
        }
        std::fs::write(dir.path().join("ignored.txt"), b"ignored").unwrap();

        let store = SigV4aCredentialStore::load_dir(dir.path()).expect("load");
        assert_eq!(store.len(), 2);
        assert!(store.get("AKIA1").is_some());
        assert!(store.get("AKIA2").is_some());
    }

    #[test]
    fn credential_store_load_dir_rejects_bad_pem() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("AKIABAD.pem"), b"not a pem").unwrap();
        let err = SigV4aCredentialStore::load_dir(dir.path()).expect_err("bad pem");
        assert!(matches!(err, SigV4aError::BadPublicKey { .. }));
    }

    /// End-to-end shape test wiring `parse_authorization_header` ->
    /// `verify` together with a real key. Produces a SigV4a-shaped
    /// authorization header, parses it back out, and verifies.
    #[test]
    fn parse_then_verify_round_trip() {
        let signing = SigningKey::random(&mut OsRng);
        let verifying = VerifyingKey::from(&signing);
        let canonical = b"GET\n/bucket/key\n\nhost:s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD";
        let sig: Signature = signing.sign(canonical);
        let sig_hex = lower_hex(sig.to_der().as_bytes());

        let header = format!(
            "AWS4-ECDSA-P256-SHA256 \
             Credential=AKIARTRIP/20260513/s3/aws4_request, \
             SignedHeaders=host, \
             Signature={sig_hex}"
        );
        let parsed = parse_authorization_header(&header).expect("parse");
        assert_eq!(parsed.access_key_id, "AKIARTRIP");
        verify(
            &CanonicalRequest::new(canonical),
            &parsed.signature_der,
            &verifying,
            "*",
            "us-east-1",
        )
        .expect("round-trip verify");
    }

    // ======================================================================
    // v0.8.4 #76 (audit H-6) — credential-scope shape + x-amz-date freshness.
    //
    // Captured-request replay was the audit's H-6 finding: a stolen valid
    // SigV4a request could be replayed indefinitely (including DELETE
    // ops), because the verifier only checked the ECDSA signature. These
    // tests cover the new validations:
    //
    // 1. credential scope must be 4 components, terminator `aws4_request`,
    //    service `s3`, date 8-digit `YYYYMMDD`.
    // 2. `x-amz-date` must be present, parseable, in-window, match the
    //    scope's date, and listed in `SignedHeaders=`.
    // ======================================================================

    /// Build a fully-signed `(SigV4aAuth, headers, canonical, vk)` tuple
    /// scoped to the given x-amz-date timestamp. Helper used by the
    /// freshness tests so each test can dial in `now` independently.
    fn build_signed_request(
        x_amz_date: &str,
        scope_date: &str,
    ) -> (SigV4aAuth, HashMap<String, String>, Vec<u8>, VerifyingKey) {
        let signing = SigningKey::random(&mut OsRng);
        let verifying = VerifyingKey::from(&signing);
        let canonical = b"GET\n/bucket/key\n\nhost:s3.example.com\nx-amz-date:placeholder\n\nhost;x-amz-date\nUNSIGNED-PAYLOAD".to_vec();
        let sig: Signature = signing.sign(&canonical);
        let sig_hex = lower_hex(sig.to_der().as_bytes());
        let header = format!(
            "AWS4-ECDSA-P256-SHA256 \
             Credential=AKIATEST/{scope_date}/s3/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature={sig_hex}"
        );
        let parsed = parse_authorization_header(&header).expect("parse");
        let mut headers = HashMap::new();
        headers.insert("host".to_string(), "s3.example.com".to_string());
        headers.insert("x-amz-date".to_string(), x_amz_date.to_string());
        (parsed, headers, canonical, verifying)
    }

    #[test]
    fn sigv4a_rejects_missing_x_amz_date() {
        let (parsed, mut headers, canonical, vk) =
            build_signed_request("20260514T120000Z", "20260514");
        headers.remove("x-amz-date");
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let err = verify_request(
            &parsed,
            &headers,
            &canonical,
            &vk,
            "*",
            "us-east-1",
            now,
            chrono::Duration::seconds(900),
        )
        .expect_err("missing x-amz-date must reject");
        assert!(matches!(err, SigV4aError::MissingXAmzDate));
    }

    #[test]
    fn sigv4a_rejects_skew_beyond_15min_past() {
        // Request signed at 12:00:00, "now" is 12:16:00 — 16 min drift,
        // beyond the 15-min default tolerance.
        let (parsed, headers, canonical, vk) = build_signed_request("20260514T120000Z", "20260514");
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T12:16:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let err = verify_request(
            &parsed,
            &headers,
            &canonical,
            &vk,
            "*",
            "us-east-1",
            now,
            chrono::Duration::seconds(900),
        )
        .expect_err("16min past drift must reject");
        match err {
            SigV4aError::RequestTimeTooSkewed {
                drift_secs,
                tolerance_secs,
            } => {
                assert_eq!(drift_secs, 960);
                assert_eq!(tolerance_secs, 900);
            }
            other => panic!("expected RequestTimeTooSkewed, got {other:?}"),
        }
    }

    #[test]
    fn sigv4a_rejects_skew_beyond_15min_future() {
        // Request signed at 12:16:00, "now" is 12:00:00 — clock skew
        // 16 min into the future is equally rejected.
        let (parsed, headers, canonical, vk) = build_signed_request("20260514T121600Z", "20260514");
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let err = verify_request(
            &parsed,
            &headers,
            &canonical,
            &vk,
            "*",
            "us-east-1",
            now,
            chrono::Duration::seconds(900),
        )
        .expect_err("16min future drift must reject");
        assert!(matches!(err, SigV4aError::RequestTimeTooSkewed { .. }));
    }

    #[test]
    fn sigv4a_rejects_malformed_credential_scope() {
        // 3 components (missing terminator) → InvalidCredentialScope.
        let header = "AWS4-ECDSA-P256-SHA256 \
             Credential=AKIA/20260514/s3, \
             SignedHeaders=host, \
             Signature=ab";
        let err = parse_authorization_header(header).expect_err("3 components must reject");
        assert!(matches!(err, SigV4aError::InvalidCredentialScope));

        // 5 components (looks like SigV4 with embedded region) → also reject.
        let header = "AWS4-ECDSA-P256-SHA256 \
             Credential=AKIA/20260514/us-east-1/s3/aws4_request, \
             SignedHeaders=host, \
             Signature=ab";
        let err = parse_authorization_header(header).expect_err("5 components must reject");
        assert!(matches!(err, SigV4aError::InvalidCredentialScope));
    }

    #[test]
    fn sigv4a_rejects_wrong_service() {
        // SigV4a captured against `ec2` MUST NOT verify against this
        // S3 listener. Defends against scope-mixing replay attacks.
        let header = "AWS4-ECDSA-P256-SHA256 \
             Credential=AKIA/20260514/ec2/aws4_request, \
             SignedHeaders=host, \
             Signature=ab";
        let err = parse_authorization_header(header).expect_err("ec2 scope must reject");
        match err {
            SigV4aError::WrongService { got } => assert_eq!(got, "ec2"),
            other => panic!("expected WrongService, got {other:?}"),
        }
    }

    #[test]
    fn sigv4a_accepts_within_skew_window() {
        // Request signed 14 min ago — inside the 15-min window, must
        // verify cleanly. Establishes that `verify_request` is not
        // accidentally rejecting fresh requests.
        let (parsed, headers, canonical, vk) = build_signed_request("20260514T120000Z", "20260514");
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T12:14:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        verify_request(
            &parsed,
            &headers,
            &canonical,
            &vk,
            "*",
            "us-east-1",
            now,
            chrono::Duration::seconds(900),
        )
        .expect("14min drift within window must verify");
    }

    /// Bonus coverage: terminator validation. `aws4_REQUEST` (case
    /// variant) is treated as malformed — AWS specifies the literal
    /// lowercase form.
    #[test]
    fn sigv4a_rejects_invalid_terminator() {
        let header = "AWS4-ECDSA-P256-SHA256 \
             Credential=AKIA/20260514/s3/AWS4_REQUEST, \
             SignedHeaders=host, \
             Signature=ab";
        let err = parse_authorization_header(header).expect_err("uppercase terminator must reject");
        assert!(matches!(err, SigV4aError::InvalidTerminator));
    }

    /// Bonus coverage: `x-amz-date` not in `SignedHeaders=` means the
    /// signature didn't actually cover the timestamp the gate just
    /// approved — fail closed.
    #[test]
    fn sigv4a_rejects_x_amz_date_not_in_signed_headers() {
        // Build an auth header whose SignedHeaders only lists `host`.
        let signing = SigningKey::random(&mut OsRng);
        let verifying = VerifyingKey::from(&signing);
        let canonical = b"x".to_vec();
        let sig: Signature = signing.sign(&canonical);
        let sig_hex = lower_hex(sig.to_der().as_bytes());
        let header = format!(
            "AWS4-ECDSA-P256-SHA256 \
             Credential=AKIA/20260514/s3/aws4_request, \
             SignedHeaders=host, \
             Signature={sig_hex}"
        );
        let parsed = parse_authorization_header(&header).expect("parse");
        let mut headers = HashMap::new();
        headers.insert("x-amz-date".to_string(), "20260514T120000Z".to_string());
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T12:00:30Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let err = verify_request(
            &parsed,
            &headers,
            &canonical,
            &verifying,
            "*",
            "us-east-1",
            now,
            chrono::Duration::seconds(900),
        )
        .expect_err("x-amz-date not in SignedHeaders must reject");
        assert!(matches!(err, SigV4aError::XAmzDateNotSigned));
    }

    /// Bonus coverage: scope date (`20260101`) ≠ `x-amz-date`'s date
    /// portion (`20260514`) → mismatch.
    #[test]
    fn sigv4a_rejects_date_scope_mismatch() {
        let (parsed, headers, canonical, vk) = build_signed_request("20260514T120000Z", "20260101");
        // now matches the x-amz-date so we get past the skew check.
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-14T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let err = verify_request(
            &parsed,
            &headers,
            &canonical,
            &vk,
            "*",
            "us-east-1",
            now,
            // Wide tolerance so the skew check doesn't fire first
            // (we want to reach the date-scope mismatch branch).
            chrono::Duration::days(365),
        )
        .expect_err("scope date mismatch must reject");
        assert!(matches!(err, SigV4aError::DateScopeMismatch));
    }
}
