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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigV4aAuth {
    /// AWS access key id (the `Credential=<key>/...` first segment).
    pub access_key_id: String,
    /// Credential scope path elements after the access-key-id, e.g.
    /// `["20260513", "s3", "aws4_request"]`. SigV4a omits the region
    /// (it lives in `X-Amz-Region-Set`), so this slice is one element
    /// shorter than the SigV4 equivalent.
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
/// Returns `None` for any header that doesn't begin with the SigV4a
/// algorithm token, or that is malformed. Successful parses fully
/// populate the [`SigV4aAuth`] struct.
#[must_use]
pub fn parse_authorization_header(header: &str) -> Option<SigV4aAuth> {
    let rest = header.trim().strip_prefix(SIGV4A_ALGORITHM)?;
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

    let cred = credential?;
    let mut cred_iter = cred.split('/');
    let access_key_id = cred_iter.next()?.to_owned();
    let credential_scope: Vec<String> = cred_iter.map(str::to_owned).collect();
    if credential_scope.is_empty() {
        return None;
    }

    let signed_headers: Vec<String> = signed_headers?
        .split(';')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if signed_headers.is_empty() {
        return None;
    }

    let signature_hex = signature?;
    let signature_der = decode_hex(signature_hex)?;

    Some(SigV4aAuth {
        access_key_id,
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
    if let Some(auth) = h.get(http::header::AUTHORIZATION).and_then(|v| v.to_str().ok())
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
    let sig = Signature::from_der(signature)
        .map_err(|e| SigV4aError::BadSignature(e.to_string()))?;
    pubkey
        .verify(request_bytes.as_bytes(), &sig)
        .map_err(|_| SigV4aError::VerificationFailed)
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
            let key = parse_p256_public_key_pem(&pem).map_err(|reason| {
                SigV4aError::BadPublicKey {
                    path: path.display().to_string(),
                    reason,
                }
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
        let header = "AWS4-HMAC-SHA256 \
             Credential=AKIA/20260513/us-east-1/s3/aws4_request, \
             SignedHeaders=host, \
             Signature=deadbeef";
        assert!(parse_authorization_header(header).is_none());
    }

    #[test]
    fn parse_authorization_header_rejects_missing_fields() {
        let header = "AWS4-ECDSA-P256-SHA256 Credential=AKIA/20260513/s3/aws4_request, \
                      SignedHeaders=host";
        assert!(parse_authorization_header(header).is_none());
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
}
