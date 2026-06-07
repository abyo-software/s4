//! v0.9 #106 — true streaming PUT checksum verify (tee-into-hasher).
//!
//! Wraps an inbound [`StreamingBlob`] so that every chunk pulled by a
//! downstream consumer (typically [`streaming_compress_to_frames`]) is
//! **also** fed into one or more client-declared digesters
//! (`Content-MD5`, `x-amz-checksum-{crc32, crc32c, sha1, sha256, crc64nvme}`).
//! When the upstream stream reaches EOF, the wrapper finalises every active
//! hasher and compares it against the client-supplied (base64) value; a
//! mismatch is surfaced as a [`std::io::Error`] tagged with
//! [`StreamingChecksumError`] so the s4-server PUT handler can translate it
//! into a typed `400 BadDigest` response instead of letting the bytes
//! reach the backend.
//!
//! ## Why this exists
//!
//! Before v0.9 #106, S4 only verified client-supplied whole-body
//! checksums on the **buffered** PUT path
//! (`crates/s4-server/src/service.rs::verify_client_body_checksums`),
//! because that path already held the entire body in memory. The
//! streaming-framed PUT path (CPU-zstd / passthrough single-PUT) accepted
//! `x-amz-checksum-*` headers and silently passed them through without
//! verifying them — the v0.8.13 #127 attempt to "force buffered when a
//! checksum is supplied" regressed sidecar correctness for AWS-SDK PUTs
//! (which auto-add `x-amz-checksum-crc32` by default), see v0.8.14 #129.
//!
//! True streaming verify computes the digests **as the bytes flow through
//! the codec pipeline**, so the buffered fallback is no longer required to
//! get integrity coverage. The wrapper preserves the streaming property:
//! it only holds one chunk's worth of bytes in flight (whatever the
//! upstream blob yields) plus the constant-size hasher state.
//!
//! ## Scope
//!
//! Wired in for **single-PUT, cpu-zstd / passthrough, non-multipart**.
//! Multipart `UploadPart` keeps the buffered per-part verify it already
//! had (the per-part body is already in memory there for the framing /
//! padding step, so there's nothing to win from streaming verify on that
//! branch). GPU codecs are bytes-buffered today; their verify happens on
//! the buffered fallback like before.
//!
//! ## Failure model
//!
//! - Header malformed (bad base64 / wrong byte length) → caller-side
//!   [`ClientChecksums::from_request_fields`] returns
//!   `S3Result<S3Error(InvalidDigest)>` **before** the wrapper is built,
//!   matching the buffered path's pre-stream validation.
//! - Stream errors mid-flight → propagated unchanged; we don't compare
//!   digests (the bytes the client intended never landed). The PUT
//!   eventually surfaces as a `TruncatedStream` / I/O error from the
//!   compressor, not a `BadDigest`.
//! - Stream completes but digest mismatch → wrapper emits one synthetic
//!   `io::ErrorKind::InvalidData` carrying [`StreamingChecksumError`] on
//!   the **next** `poll_next` call after EOF. The compressor sees this
//!   as an I/O error mid-read and returns `CodecError::Io(...)`. The
//!   PUT handler then downcasts the inner error chain to recover the
//!   `StreamingChecksumError` and maps to `BadDigest`.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use base64::Engine as _;
use bytes::Bytes;
use crc32fast::Hasher as Crc32Hasher;
use futures::{Stream, StreamExt};
use md5::{Digest as Md5Digest, Md5};
use s3s::dto::StreamingBlob;
use s3s::stream::{ByteStream, RemainingLength};
use s3s::{S3Error, S3ErrorCode, S3Result};
use sha1::Sha1;
use sha2::Sha256;
use std::sync::Mutex;

/// Sentinel error carried inside [`std::io::Error::other`] when the
/// streaming wrapper detects a client-vs-actual digest mismatch at EOF.
/// The PUT handler downcasts the error chain to recover this and emits
/// a typed `BadDigest` S3 response.
#[derive(Debug, Clone)]
pub struct StreamingChecksumError {
    /// Human-readable name of the checksum algorithm that failed
    /// (`Content-MD5`, `x-amz-checksum-crc32c`, ...). Used verbatim in
    /// the `BadDigest` message so operators see which header was wrong.
    pub algorithm: &'static str,
}

impl std::fmt::Display for StreamingChecksumError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "client-supplied {} did not match the streamed body",
            self.algorithm
        )
    }
}

impl std::error::Error for StreamingChecksumError {}

/// Parsed, byte-length-validated client checksum claims for a single PUT
/// request. Built once at request entry from the raw header strings;
/// then handed to [`tee_into_hashers`] to drive the streaming verifier.
///
/// `from_request_fields` performs the same pre-stream validation the
/// buffered path's `verify_client_body_checksums` did inline (base64
/// decodes, byte-length checks) so a malformed header fails with
/// `InvalidDigest` **before** the body ever flows. After this returns
/// `Ok`, every present claim has the correct decoded byte length.
#[derive(Debug, Default, Clone)]
pub struct ClientChecksums {
    content_md5: Option<[u8; 16]>,
    crc32: Option<[u8; 4]>,
    crc32c: Option<[u8; 4]>,
    sha1: Option<[u8; 20]>,
    sha256: Option<[u8; 32]>,
    crc64nvme: Option<[u8; 8]>,
}

impl ClientChecksums {
    /// Returns true when at least one checksum claim is set — i.e. the
    /// streaming wrapper has work to do. When this returns false the
    /// caller should skip the wrapper entirely (zero-cost: no hashers
    /// allocated, no per-chunk update path) so non-checksummed PUTs
    /// keep their pre-#106 throughput.
    pub fn any(&self) -> bool {
        self.content_md5.is_some()
            || self.crc32.is_some()
            || self.crc32c.is_some()
            || self.sha1.is_some()
            || self.sha256.is_some()
            || self.crc64nvme.is_some()
    }

    /// Parse the six AWS-spec checksum header values supplied on a single
    /// PUT request. Each argument is the base64-encoded header value (or
    /// `None` when the header was absent). Returns `Err(InvalidDigest)`
    /// when any present value is malformed (bad base64 or wrong decoded
    /// length); identical pre-stream behaviour to the buffered path's
    /// inline validation.
    pub fn from_request_fields(
        content_md5: Option<&str>,
        crc32: Option<&str>,
        crc32c: Option<&str>,
        sha1: Option<&str>,
        sha256: Option<&str>,
        crc64nvme: Option<&str>,
    ) -> S3Result<Self> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let decode_fixed = |val: &str, expected_len: usize, label: &str| -> S3Result<Vec<u8>> {
            let v = b64.decode(val).map_err(|_| {
                S3Error::with_message(S3ErrorCode::InvalidDigest, format!("malformed {label}"))
            })?;
            if v.len() != expected_len {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidDigest,
                    format!("{label} must decode to {expected_len} bytes"),
                ));
            }
            Ok(v)
        };
        let mut out = ClientChecksums::default();
        if let Some(v) = content_md5 {
            let bytes = decode_fixed(v, 16, "Content-MD5")?;
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&bytes);
            out.content_md5 = Some(arr);
        }
        if let Some(v) = crc32 {
            let bytes = decode_fixed(v, 4, "x-amz-checksum-crc32")?;
            let mut arr = [0u8; 4];
            arr.copy_from_slice(&bytes);
            out.crc32 = Some(arr);
        }
        if let Some(v) = crc32c {
            let bytes = decode_fixed(v, 4, "x-amz-checksum-crc32c")?;
            let mut arr = [0u8; 4];
            arr.copy_from_slice(&bytes);
            out.crc32c = Some(arr);
        }
        if let Some(v) = sha1 {
            let bytes = decode_fixed(v, 20, "x-amz-checksum-sha1")?;
            let mut arr = [0u8; 20];
            arr.copy_from_slice(&bytes);
            out.sha1 = Some(arr);
        }
        if let Some(v) = sha256 {
            let bytes = decode_fixed(v, 32, "x-amz-checksum-sha256")?;
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            out.sha256 = Some(arr);
        }
        if let Some(v) = crc64nvme {
            let bytes = decode_fixed(v, 8, "x-amz-checksum-crc64nvme")?;
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&bytes);
            out.crc64nvme = Some(arr);
        }
        Ok(out)
    }
}

/// Which hashers the tee should drive on every chunk. Derived from
/// both the parsed request headers ([`ClientChecksums`]) AND any
/// algorithms the client announced via `x-amz-trailer` (the chunked /
/// SigV4-streaming SDK case, where the actual digest value arrives in
/// the request trailers after the body is fully consumed).
///
/// Each flag is on iff the corresponding algorithm must be computed —
/// avoiding the per-chunk cost of (in particular) the SHA family when
/// the client only wants a CRC.
#[derive(Debug, Default, Clone, Copy)]
pub struct WhichHashers {
    pub content_md5: bool,
    pub crc32: bool,
    pub crc32c: bool,
    pub sha1: bool,
    pub sha256: bool,
    pub crc64nvme: bool,
}

impl WhichHashers {
    pub fn any(&self) -> bool {
        self.content_md5 || self.crc32 || self.crc32c || self.sha1 || self.sha256 || self.crc64nvme
    }

    /// Union: enable a hasher in `self` if it was on in either side.
    pub fn or(self, other: Self) -> Self {
        Self {
            content_md5: self.content_md5 || other.content_md5,
            crc32: self.crc32 || other.crc32,
            crc32c: self.crc32c || other.crc32c,
            sha1: self.sha1 || other.sha1,
            sha256: self.sha256 || other.sha256,
            crc64nvme: self.crc64nvme || other.crc64nvme,
        }
    }

    /// Drop any hashers whose algorithm name appears in the
    /// comma-separated `x-amz-trailer` header value. Each name is
    /// trimmed; case-insensitive against the AWS spec names.
    pub fn from_trailer_header(value: &str) -> Self {
        let mut out = Self::default();
        for raw in value.split(',') {
            let name = raw.trim();
            if name.eq_ignore_ascii_case("x-amz-checksum-crc32") {
                out.crc32 = true;
            } else if name.eq_ignore_ascii_case("x-amz-checksum-crc32c") {
                out.crc32c = true;
            } else if name.eq_ignore_ascii_case("x-amz-checksum-sha1") {
                out.sha1 = true;
            } else if name.eq_ignore_ascii_case("x-amz-checksum-sha256") {
                out.sha256 = true;
            } else if name.eq_ignore_ascii_case("x-amz-checksum-crc64nvme") {
                out.crc64nvme = true;
            }
            // Other trailers (`x-amz-trailer-signature`, custom)
            // do not request hashing; they are ignored here.
        }
        out
    }
}

impl ClientChecksums {
    /// Project the parsed claim set onto the boolean hasher-selector
    /// used by [`WhichHashers`]. A header-supplied claim implies the
    /// hasher must run (for EOF eager-compare).
    pub fn which_hashers(&self) -> WhichHashers {
        WhichHashers {
            content_md5: self.content_md5.is_some(),
            crc32: self.crc32.is_some(),
            crc32c: self.crc32c.is_some(),
            sha1: self.sha1.is_some(),
            sha256: self.sha256.is_some(),
            crc64nvme: self.crc64nvme.is_some(),
        }
    }
}

/// Finalised digest values for every algorithm whose hasher was
/// active. Populated by the tee on the EOF poll and exposed via the
/// [`DigestHandle`] returned by [`tee_into_hashers_with_handle`]; the
/// PUT handler reads it after body consumption to compare against
/// request **trailer** values (the chunked / SigV4-streaming SDK case
/// where the checksum is delivered post-body rather than as a header).
#[derive(Debug, Default, Clone)]
pub struct ComputedDigests {
    pub content_md5: Option<[u8; 16]>,
    pub crc32_be: Option<[u8; 4]>,
    pub crc32c_be: Option<[u8; 4]>,
    pub sha1: Option<[u8; 20]>,
    pub sha256: Option<[u8; 32]>,
    pub crc64nvme_be: Option<[u8; 8]>,
}

impl ComputedDigests {
    /// Compare one finalised digest against a base64-encoded
    /// trailer-supplied claim. Returns `Err(BadDigest)` on mismatch,
    /// `Err(InvalidDigest)` on malformed input, `Ok(())` on match.
    /// `algorithm` is the wire header name used in the error message;
    /// the match is **case-insensitive** because HTTP header field
    /// names are case-insensitive per RFC 9110 §5.1 and AWS SDKs may
    /// announce trailers as `X-Amz-Checksum-Crc32c` (or any other
    /// casing) — we keep the original casing in error messages for
    /// fidelity but normalise for the dispatch.
    pub fn compare_b64(&self, algorithm: &str, claim_b64: &str) -> S3Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let want = b64.decode(claim_b64).map_err(|_| {
            S3Error::with_message(S3ErrorCode::InvalidDigest, format!("malformed {algorithm}"))
        })?;
        let bad = || {
            let code =
                S3ErrorCode::from_bytes(b"BadDigest").unwrap_or(S3ErrorCode::InvalidArgument);
            S3Error::with_message(
                code,
                format!("client-supplied {algorithm} did not match the received body"),
            )
        };
        let len_err = |expected: usize| {
            S3Error::with_message(
                S3ErrorCode::InvalidDigest,
                format!("{algorithm} must decode to {expected} bytes"),
            )
        };
        // Lowercase only for dispatch — header field names are
        // case-insensitive (RFC 9110 §5.1) but we keep the
        // client-supplied form for the surface text so operators see
        // what the client actually sent.
        let lc = algorithm.to_ascii_lowercase();
        match lc.as_str() {
            "content-md5" => {
                if want.len() != 16 {
                    return Err(len_err(16));
                }
                if let Some(got) = self.content_md5
                    && got[..] == want[..]
                {
                    return Ok(());
                }
                Err(bad())
            }
            "x-amz-checksum-crc32" => {
                if want.len() != 4 {
                    return Err(len_err(4));
                }
                if let Some(got) = self.crc32_be
                    && got[..] == want[..]
                {
                    return Ok(());
                }
                Err(bad())
            }
            "x-amz-checksum-crc32c" => {
                if want.len() != 4 {
                    return Err(len_err(4));
                }
                if let Some(got) = self.crc32c_be
                    && got[..] == want[..]
                {
                    return Ok(());
                }
                Err(bad())
            }
            "x-amz-checksum-sha1" => {
                if want.len() != 20 {
                    return Err(len_err(20));
                }
                if let Some(got) = self.sha1
                    && got[..] == want[..]
                {
                    return Ok(());
                }
                Err(bad())
            }
            "x-amz-checksum-sha256" => {
                if want.len() != 32 {
                    return Err(len_err(32));
                }
                if let Some(got) = self.sha256
                    && got[..] == want[..]
                {
                    return Ok(());
                }
                Err(bad())
            }
            "x-amz-checksum-crc64nvme" => {
                if want.len() != 8 {
                    return Err(len_err(8));
                }
                if let Some(got) = self.crc64nvme_be
                    && got[..] == want[..]
                {
                    return Ok(());
                }
                Err(bad())
            }
            _ => Err(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!("unknown checksum trailer: {algorithm}"),
            )),
        }
    }
}

/// Shared, post-EOF-readable digest container. The tee's `poll_next`
/// EOF branch deposits the finalised [`ComputedDigests`] here; the PUT
/// handler reads it after the body has been fully consumed by the
/// codec to compare against any trailer-supplied claims.
pub type DigestHandle = Arc<Mutex<Option<ComputedDigests>>>;

/// Internal hasher state. Each variant maintains a rolling digest fed
/// chunk-by-chunk from the wrapper's `poll_next`. Wrapped in a `Mutex`
/// on the wrapper side because pin-projection-friendly interior
/// mutability is the cleanest way to keep the wrapper `Send + Sync`
/// (the codec dispatcher holds the blob inside an `Arc`-cloned closure
/// in places).
struct HasherSet {
    expected: ClientChecksums,
    which: WhichHashers,
    // CRC32 (IEEE) and CRC32C use accumulators, not the `Digest` trait.
    crc32: Crc32Hasher,
    crc32c_acc: u32,
    crc64nvme_acc: u64,
    md5: Md5,
    sha1: Sha1,
    sha256: Sha256,
}

impl HasherSet {
    fn new(expected: ClientChecksums, which: WhichHashers) -> Self {
        Self {
            expected,
            which,
            crc32: Crc32Hasher::new(),
            crc32c_acc: 0,
            crc64nvme_acc: !0u64,
            md5: Md5::new(),
            sha1: Sha1::new(),
            sha256: Sha256::new(),
        }
    }

    fn update(&mut self, chunk: &[u8]) {
        // Only feed hashers whose flag is on — saves the per-byte cost
        // on PUTs that supply (say) only crc32c and not sha256.
        // CRC32 / CRC32C are cheap (SIMD on modern CPUs); the SHA
        // family is the expensive one to skip.
        if self.which.crc32 {
            self.crc32.update(chunk);
        }
        if self.which.crc32c {
            self.crc32c_acc = crc32c::crc32c_append(self.crc32c_acc, chunk);
        }
        if self.which.crc64nvme {
            self.crc64nvme_acc = crc64_nvme_append(self.crc64nvme_acc, chunk);
        }
        if self.which.content_md5 {
            self.md5.update(chunk);
        }
        if self.which.sha1 {
            self.sha1.update(chunk);
        }
        if self.which.sha256 {
            self.sha256.update(chunk);
        }
    }

    /// Finalise every active hasher and produce a [`ComputedDigests`]
    /// snapshot. Consumes self — the hasher state is destructively
    /// finalised by the underlying crates' `.finalize()` calls.
    fn finalize(self) -> ComputedDigests {
        let mut out = ComputedDigests::default();
        if self.which.content_md5 {
            let d = self.md5.finalize();
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&d);
            out.content_md5 = Some(arr);
        }
        if self.which.crc32 {
            out.crc32_be = Some(self.crc32.finalize().to_be_bytes());
        }
        if self.which.crc32c {
            out.crc32c_be = Some(self.crc32c_acc.to_be_bytes());
        }
        if self.which.sha1 {
            let d = self.sha1.finalize();
            let mut arr = [0u8; 20];
            arr.copy_from_slice(&d);
            out.sha1 = Some(arr);
        }
        if self.which.sha256 {
            let d = self.sha256.finalize();
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&d);
            out.sha256 = Some(arr);
        }
        if self.which.crc64nvme {
            out.crc64nvme_be = Some((!self.crc64nvme_acc).to_be_bytes());
        }
        out
    }

    /// Eager EOF-time comparison against the **header-supplied** claim
    /// set captured at request entry. Returns `Err(StreamingChecksumError)`
    /// on the first mismatch (deterministic order: Content-MD5, CRC32,
    /// CRC32C, SHA-1, SHA-256, CRC64-NVME — mirrors the buffered path's
    /// `verify_client_body_checksums` order so error messages are
    /// reproducible across the two paths). Header claims are checked
    /// at EOF so a streaming body fails fast at the codec layer;
    /// trailer claims are checked after EOF by the PUT handler via
    /// [`ComputedDigests::compare_b64`].
    fn compare_header_claims(
        digests: &ComputedDigests,
        expected: &ClientChecksums,
    ) -> Result<(), StreamingChecksumError> {
        if let (Some(want), Some(got)) = (expected.content_md5, digests.content_md5)
            && got != want
        {
            return Err(StreamingChecksumError {
                algorithm: "Content-MD5",
            });
        }
        if let (Some(want), Some(got)) = (expected.crc32, digests.crc32_be)
            && got != want
        {
            return Err(StreamingChecksumError {
                algorithm: "x-amz-checksum-crc32",
            });
        }
        if let (Some(want), Some(got)) = (expected.crc32c, digests.crc32c_be)
            && got != want
        {
            return Err(StreamingChecksumError {
                algorithm: "x-amz-checksum-crc32c",
            });
        }
        if let (Some(want), Some(got)) = (expected.sha1, digests.sha1)
            && got != want
        {
            return Err(StreamingChecksumError {
                algorithm: "x-amz-checksum-sha1",
            });
        }
        if let (Some(want), Some(got)) = (expected.sha256, digests.sha256)
            && got != want
        {
            return Err(StreamingChecksumError {
                algorithm: "x-amz-checksum-sha256",
            });
        }
        if let (Some(want), Some(got)) = (expected.crc64nvme, digests.crc64nvme_be)
            && got != want
        {
            return Err(StreamingChecksumError {
                algorithm: "x-amz-checksum-crc64nvme",
            });
        }
        Ok(())
    }
}

/// Rolling CRC-64/NVMe accumulator (matches the buffered-path table in
/// `service.rs::crc64_nvme`). The full byte-for-byte digest is
/// `!crc64_nvme_append(!0u64, bytes)` — the `!0u64` init is bit-flipped
/// on output to match the NVMe spec (`xorout = 0xffff_ffff_ffff_ffff`).
fn crc64_nvme_append(init: u64, bytes: &[u8]) -> u64 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u64; 256]> = OnceLock::new();
    let tbl = TABLE.get_or_init(|| {
        // Reflected polynomial (bit-reverse of 0xad93d23594c93659) —
        // identical constant to `service.rs::crc64_nvme`; intentionally
        // duplicated rather than re-exported so the streaming wrapper
        // has no cross-module dependency on the buffered helper (the
        // table is 2 KiB total, cost is negligible).
        const POLY_REFLECTED: u64 = 0x9a6c_9329_ac4b_c9b5;
        let mut t = [0u64; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut c = i as u64;
            let mut j = 0;
            while j < 8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ POLY_REFLECTED
                } else {
                    c >> 1
                };
                j += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    });
    let mut crc = init;
    for &b in bytes {
        let idx = ((crc as u8) ^ b) as usize;
        crc = (crc >> 8) ^ tbl[idx];
    }
    crc
}

/// Wraps `inner` so every chunk yielded by `poll_next` is **also** fed
/// into a [`HasherSet`] before being passed downstream. On the EOF poll
/// the hashers are finalised and compared against `expected`; a
/// mismatch is emitted as a fresh [`io::Error`] (`InvalidData`) carrying
/// [`StreamingChecksumError`] in its source chain.
///
/// The wrapper holds the [`HasherSet`] in a `Mutex` so it stays
/// `Send + Sync` (the s3s `StreamingBlob` is wrapped in a `Sync`-
/// erased trait object and any non-`Sync` field would force a deeper
/// rework of the blob constructor). Lock contention is zero in
/// practice — only one task polls a given stream at a time.
struct TeeStream {
    inner: StreamingBlob,
    state: Arc<Mutex<TeeState>>,
    /// Cloned to the PUT handler so it can read the post-EOF
    /// `ComputedDigests` and run any trailer-supplied comparisons that
    /// only arrive after the body is consumed.
    digests_out: DigestHandle,
}

/// `Some(HasherSet)` while the stream is live; `None` once finalised
/// (EOF reached AND comparison performed) so we don't double-finalise
/// if the downstream consumer keeps polling past EOF.
struct TeeState {
    hashers: Option<HasherSet>,
}

impl Stream for TeeStream {
    type Item = Result<Bytes, s3s::StdError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                // Feed the hashers before yielding the chunk
                // downstream. Holding the lock across update() is fine
                // — single-polling-task contract on a Stream.
                let mut guard = this.state.lock().expect("tee hasher lock poisoned");
                if let Some(h) = guard.hashers.as_mut() {
                    h.update(&chunk);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                // Drop the hashers on upstream error so any subsequent
                // polls don't fire the EOF comparison (the bytes that
                // were supposed to land never did — comparing a partial
                // digest would yield a meaningless mismatch).
                let mut guard = this.state.lock().expect("tee hasher lock poisoned");
                guard.hashers = None;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                let mut guard = this.state.lock().expect("tee hasher lock poisoned");
                if let Some(hashers) = guard.hashers.take() {
                    let expected_header_claims = hashers.expected.clone();
                    let digests = hashers.finalize();
                    // Stash the finalised digests so the PUT handler
                    // can run trailer comparisons after the body has
                    // been consumed.
                    *this
                        .digests_out
                        .lock()
                        .expect("digest handle lock poisoned") = Some(digests.clone());
                    // Eager EOF-time check against header claims:
                    // surface a mismatch as a synthetic
                    // `InvalidData` I/O error carrying
                    // `StreamingChecksumError`. The PUT handler
                    // downcasts back to map to `BadDigest`. Trailer
                    // claims aren't reachable here (they arrive
                    // post-stream); the PUT handler runs them
                    // separately via `ComputedDigests::compare_b64`.
                    if let Err(mismatch) =
                        HasherSet::compare_header_claims(&digests, &expected_header_claims)
                    {
                        let io_err = std::io::Error::new(std::io::ErrorKind::InvalidData, mismatch);
                        let boxed: s3s::StdError = Box::new(io_err);
                        return Poll::Ready(Some(Err(boxed)));
                    }
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ByteStream for TeeStream {
    fn remaining_length(&self) -> RemainingLength {
        // The wrapper is a 1:1 byte pass-through; defer to inner.
        self.inner.remaining_length()
    }
}

/// Build a [`StreamingBlob`] that streams the underlying body through
/// `inner`, simultaneously feeding every chunk into the hashers
/// selected by `which`. On EOF the wrapper finalises every hasher,
/// (a) deposits the result into the returned [`DigestHandle`] for
/// post-body trailer comparisons, and (b) eagerly compares against
/// every claim already present in `expected` (the
/// header-supplied set); a mismatch surfaces as a synthetic
/// `io::Error` carrying [`StreamingChecksumError`].
///
/// **Caller MUST consume the returned blob to completion** for the
/// verification step to fire. Dropping the blob mid-stream is treated
/// as a stream abort (same as any other stream consumer) and skips
/// the final comparison — the PUT itself will already have failed by
/// then, so there's nothing useful to compare against.
pub fn tee_into_hashers_with_handle(
    inner: StreamingBlob,
    expected: ClientChecksums,
    which: WhichHashers,
) -> (StreamingBlob, DigestHandle) {
    let digests_out: DigestHandle = Arc::new(Mutex::new(None));
    let state = Arc::new(Mutex::new(TeeState {
        hashers: Some(HasherSet::new(expected, which)),
    }));
    let tee = TeeStream {
        inner,
        state,
        digests_out: Arc::clone(&digests_out),
    };
    (StreamingBlob::new(tee), digests_out)
}

/// Convenience wrapper for the common case: only header-supplied
/// claims are present, no trailer expectation. Discards the digest
/// handle. Kept for the existing call sites and unit tests; new code
/// that needs trailer support should use
/// [`tee_into_hashers_with_handle`].
pub fn tee_into_hashers(inner: StreamingBlob, expected: ClientChecksums) -> StreamingBlob {
    let which = expected.which_hashers();
    let (blob, _handle) = tee_into_hashers_with_handle(inner, expected, which);
    blob
}

/// v0.9 #106-audit-R2 P2-INT-2: compute the algorithms named by `which`
/// over `body` in one shot, producing a [`ComputedDigests`] suitable
/// for trailer comparison via [`ComputedDigests::compare_b64`]. This is
/// the **buffered-path** counterpart of the streaming tee: the body is
/// already in memory (e.g. GPU codec branch or non-streaming-framed
/// PUT), so we don't need the chunk-by-chunk wrapper — a single
/// in-place hash run suffices.
///
/// Lives in this module so the trailer-verify logic on both paths
/// (streaming-framed and buffered) calls the same finaliser
/// (`compare_b64`) and the test surface that covers it
/// (`computed_digests_compare_b64_*`) keeps both paths honest.
pub fn compute_digests(body: &[u8], which: WhichHashers) -> ComputedDigests {
    let mut hashers = HasherSet::new(ClientChecksums::default(), which);
    hashers.update(body);
    hashers.finalize()
}

/// Walk an `io::Error` source chain looking for a
/// [`StreamingChecksumError`]. Returns the algorithm name when found.
/// Used by the PUT handler to decide whether a `CodecError::Io` was
/// the streaming verifier's mismatch (→ `BadDigest`) or a genuine
/// transport-layer I/O failure (→ `InternalError`).
///
/// The chain we have to walk:
///
/// ```text
///   CodecError::Io(outer)                            ← service.rs sees this
///     outer = io::Error::other(boxed_std_err)        ← blob_to_async_read wraps
///       boxed_std_err = Box<io::Error(InvalidData, StreamingChecksumError)>
///                                                    ← tee_into_hashers emits
/// ```
///
/// `outer.get_ref()` returns `&dyn Error` pointing at `boxed_std_err`'s
/// inner. We try the direct downcast first (covers tests / callers
/// that pass the inner io::Error directly), then peel one Error::other
/// wrapper to recover the nested io::Error built by the tee.
pub fn extract_streaming_checksum_error(err: &std::io::Error) -> Option<&'static str> {
    // 1. Direct: the io::Error we were given carries the
    //    StreamingChecksumError as its inner. This happens in unit
    //    tests that construct the error themselves and in any future
    //    caller that doesn't add the StreamReader wrapper.
    if let Some(inner) = err.get_ref()
        && let Some(s) = inner.downcast_ref::<StreamingChecksumError>()
    {
        return Some(s.algorithm);
    }
    // 2. One-deep: the StreamReader → io::Error::other(StdError) wrap
    //    added by `blob_to_async_read`. The inner is a
    //    `Box<dyn Error + Send + Sync>` which we built ourselves
    //    around an `io::Error`; recover that nested io::Error and
    //    repeat the lookup.
    if let Some(inner) = err.get_ref()
        && let Some(nested_io) = inner.downcast_ref::<std::io::Error>()
        && let Some(deeper) = nested_io.get_ref()
        && let Some(s) = deeper.downcast_ref::<StreamingChecksumError>()
    {
        return Some(s.algorithm);
    }
    // 3. Fallback: best-effort walk of the conventional source chain
    //    for any future re-wrap that uses `Error::source` properly.
    let mut src: Option<&dyn std::error::Error> = std::error::Error::source(err);
    while let Some(e) = src {
        if let Some(s) = e.downcast_ref::<StreamingChecksumError>() {
            return Some(s.algorithm);
        }
        src = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    fn b64encode(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    fn make_chunked_blob(chunks: Vec<Bytes>) -> StreamingBlob {
        let stream = stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
        StreamingBlob::wrap(stream)
    }

    async fn drain(blob: StreamingBlob) -> Result<Vec<u8>, String> {
        let mut s = blob;
        let mut out = Vec::new();
        while let Some(chunk) = s.next().await {
            let chunk = chunk.map_err(|e| format!("{e}"))?;
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Plain pass-through (no claims set) yields the original bytes
    /// unchanged and never errors at EOF.
    #[tokio::test]
    async fn tee_with_no_claims_is_passthrough() {
        let body = Bytes::from_static(b"hello streaming s4");
        let blob = make_chunked_blob(vec![body.clone()]);
        let wrapped = tee_into_hashers(blob, ClientChecksums::default());
        let got = drain(wrapped).await.unwrap();
        assert_eq!(got, body.to_vec());
    }

    /// crc32c claim matches → drain succeeds, no synthetic error.
    #[tokio::test]
    async fn crc32c_match_yields_full_body() {
        let body: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
        let crc = crc32c::crc32c(&body).to_be_bytes();
        let claims = ClientChecksums::from_request_fields(
            None,
            None,
            Some(&b64encode(&crc)),
            None,
            None,
            None,
        )
        .unwrap();
        let blob = make_chunked_blob(vec![
            Bytes::copy_from_slice(&body[..20_000]),
            Bytes::copy_from_slice(&body[20_000..]),
        ]);
        let wrapped = tee_into_hashers(blob, claims);
        let got = drain(wrapped).await.unwrap();
        assert_eq!(got, body);
    }

    /// crc32c claim mismatched (off-by-one byte) → EOF poll emits a
    /// synthetic InvalidData carrying StreamingChecksumError.
    #[tokio::test]
    async fn crc32c_mismatch_fires_at_eof() {
        let body: Vec<u8> = vec![b'a'; 4096];
        // Use a deliberately wrong CRC.
        let wrong_crc = (crc32c::crc32c(&body) ^ 0xFFFF_FFFF).to_be_bytes();
        let claims = ClientChecksums::from_request_fields(
            None,
            None,
            Some(&b64encode(&wrong_crc)),
            None,
            None,
            None,
        )
        .unwrap();
        let blob = make_chunked_blob(vec![Bytes::copy_from_slice(&body)]);
        let wrapped = tee_into_hashers(blob, claims);
        let err = drain(wrapped).await.unwrap_err();
        assert!(
            err.contains("x-amz-checksum-crc32c"),
            "error must name the failing algorithm, got: {err}"
        );
    }

    /// sha256 claim matches → drain succeeds.
    #[tokio::test]
    async fn sha256_match_succeeds_across_many_small_chunks() {
        let body: Vec<u8> = (0..123_456u32).map(|i| (i ^ 0x5a) as u8).collect();
        let digest = {
            let mut h = Sha256::new();
            h.update(&body);
            h.finalize()
        };
        let claims = ClientChecksums::from_request_fields(
            None,
            None,
            None,
            None,
            Some(&b64encode(&digest)),
            None,
        )
        .unwrap();
        // Split body into many small chunks to exercise the per-chunk
        // update path.
        let chunks: Vec<Bytes> = body.chunks(1024).map(Bytes::copy_from_slice).collect();
        let blob = make_chunked_blob(chunks);
        let wrapped = tee_into_hashers(blob, claims);
        let got = drain(wrapped).await.unwrap();
        assert_eq!(got, body);
    }

    /// Multiple algorithms set together — all must verify; mismatch in
    /// any one fires.
    #[tokio::test]
    async fn multi_algorithm_one_wrong_fires() {
        let body = vec![0u8; 8192];
        let crc32c_be = crc32c::crc32c(&body).to_be_bytes();
        let mut sha = Sha256::new();
        sha.update(&body);
        let sha_correct = sha.finalize();
        // Flip a byte in the SHA-256 claim.
        let mut sha_wrong = sha_correct.to_vec();
        sha_wrong[0] ^= 0xFF;
        let claims = ClientChecksums::from_request_fields(
            None,
            None,
            Some(&b64encode(&crc32c_be)),
            None,
            Some(&b64encode(&sha_wrong)),
            None,
        )
        .unwrap();
        let blob = make_chunked_blob(vec![Bytes::copy_from_slice(&body)]);
        let wrapped = tee_into_hashers(blob, claims);
        let err = drain(wrapped).await.unwrap_err();
        assert!(
            err.contains("x-amz-checksum-sha256"),
            "expected sha256 mismatch, got: {err}"
        );
    }

    /// Malformed base64 in the header → InvalidDigest BEFORE any body
    /// flows.
    #[test]
    fn from_request_fields_rejects_malformed_base64() {
        let err = ClientChecksums::from_request_fields(
            None,
            None,
            Some("not-base-64!!!"),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidDigest);
    }

    /// Correct base64 but wrong decoded length → InvalidDigest.
    #[test]
    fn from_request_fields_rejects_wrong_length() {
        // base64 of 3 bytes — crc32c demands 4.
        let too_short = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        let err =
            ClientChecksums::from_request_fields(None, None, Some(&too_short), None, None, None)
                .unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidDigest);
    }

    /// CRC-64/NVME table cross-check against the buffered helper:
    /// `crc64_nvme_append(!0u64, b"")` xor-out should be 0 (empty
    /// input has crc 0 by NVMe spec — init xor xorout).
    #[test]
    fn crc64_nvme_empty_input_is_zero() {
        let crc = crc64_nvme_append(!0u64, b"");
        assert_eq!(!crc, 0u64, "NVMe empty-input CRC must be 0");
    }

    /// extract_streaming_checksum_error round-trips for an error we
    /// constructed the same way the wrapper does.
    #[test]
    fn extract_recovers_algorithm() {
        let mismatch = StreamingChecksumError {
            algorithm: "x-amz-checksum-crc32c",
        };
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, mismatch);
        assert_eq!(
            extract_streaming_checksum_error(&io),
            Some("x-amz-checksum-crc32c")
        );
    }

    /// Returns None for an unrelated io error.
    #[test]
    fn extract_returns_none_for_unrelated_io_error() {
        let io = std::io::Error::other("unrelated");
        assert_eq!(extract_streaming_checksum_error(&io), None);
    }

    /// `WhichHashers::from_trailer_header` recognises every AWS
    /// checksum trailer name (case-insensitive) and ignores
    /// unrelated entries.
    #[test]
    fn which_hashers_from_trailer_header_parses_all_known_names() {
        let w = WhichHashers::from_trailer_header(
            "x-amz-checksum-crc32, X-Amz-Checksum-Crc32c, x-amz-trailer-signature",
        );
        assert!(w.crc32);
        assert!(w.crc32c);
        assert!(!w.sha1);
        assert!(!w.sha256);
        assert!(!w.crc64nvme);
        assert!(!w.content_md5);

        let w2 = WhichHashers::from_trailer_header("x-amz-checksum-sha256");
        assert!(w2.sha256);
        assert!(!w2.crc32c);
        let w3 = WhichHashers::from_trailer_header("x-amz-checksum-crc64nvme");
        assert!(w3.crc64nvme);
    }

    /// Trailer-deferred path: tee runs the hasher because
    /// `x-amz-trailer` announced it; the header claim itself is
    /// empty; the digest handle exposes the finalised value for the
    /// PUT handler to compare against the actual trailer value.
    #[tokio::test]
    async fn tee_with_handle_stashes_digests_for_trailer_compare() {
        let body: Vec<u8> = vec![7u8; 9000];
        // No header claim — only the trailer hasher selector.
        let which = WhichHashers {
            crc32c: true,
            sha256: true,
            ..Default::default()
        };
        let blob = make_chunked_blob(vec![Bytes::copy_from_slice(&body)]);
        let (wrapped, handle) =
            tee_into_hashers_with_handle(blob, ClientChecksums::default(), which);
        let got = drain(wrapped).await.unwrap();
        assert_eq!(got, body);

        let computed = handle.lock().unwrap().clone().expect("digests stashed");
        let expected_crc32c = crc32c::crc32c(&body).to_be_bytes();
        assert_eq!(computed.crc32c_be, Some(expected_crc32c));
        let expected_sha256 = {
            let mut h = Sha256::new();
            h.update(&body);
            h.finalize()
        };
        assert_eq!(computed.sha256.unwrap(), expected_sha256[..]);
    }

    /// `ComputedDigests::compare_b64` matches a correct claim and
    /// rejects a mismatched / malformed claim with the right S3 code.
    #[test]
    fn computed_digests_compare_b64_match_and_mismatch() {
        let body = b"sample-bytes";
        let d = ComputedDigests {
            crc32c_be: Some(crc32c::crc32c(body).to_be_bytes()),
            ..Default::default()
        };
        // Correct
        d.compare_b64(
            "x-amz-checksum-crc32c",
            &b64encode(&crc32c::crc32c(body).to_be_bytes()),
        )
        .expect("match must succeed");
        // Mismatch
        let err = d
            .compare_b64("x-amz-checksum-crc32c", &b64encode(&[0u8; 4]))
            .unwrap_err();
        assert_eq!(err.code().as_str(), "BadDigest");
        // Malformed base64
        let err = d
            .compare_b64("x-amz-checksum-crc32c", "@@@not-b64@@@")
            .unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidDigest);
        // Wrong length
        let err = d
            .compare_b64("x-amz-checksum-crc32c", &b64encode(&[0u8; 8]))
            .unwrap_err();
        assert_eq!(err.code(), &S3ErrorCode::InvalidDigest);
    }

    /// `compare_b64` against an algorithm the tee never hashed (slot
    /// is `None`) returns `BadDigest` — we cannot verify what the
    /// client promised, so we must refuse the PUT rather than
    /// silently accept it.
    #[test]
    fn computed_digests_compare_b64_against_unhashed_algorithm_rejects() {
        let d = ComputedDigests::default(); // nothing hashed
        let err = d
            .compare_b64("x-amz-checksum-sha256", &b64encode(&[0u8; 32]))
            .unwrap_err();
        assert_eq!(err.code().as_str(), "BadDigest");
    }

    /// `compare_b64` accepts any-casing trailer names (HTTP header
    /// names are case-insensitive per RFC 9110 §5.1; AWS SDKs may
    /// announce `X-Amz-Checksum-Crc32c` or `x-amz-checksum-crc32c`
    /// interchangeably).
    #[test]
    fn computed_digests_compare_b64_case_insensitive_algorithm() {
        let body = b"sample";
        let d = ComputedDigests {
            crc32c_be: Some(crc32c::crc32c(body).to_be_bytes()),
            ..Default::default()
        };
        let want = b64encode(&crc32c::crc32c(body).to_be_bytes());
        for variant in [
            "x-amz-checksum-crc32c",
            "X-Amz-Checksum-Crc32c",
            "X-AMZ-CHECKSUM-CRC32C",
            "x-AMZ-checksum-CRC32C",
        ] {
            d.compare_b64(variant, &want)
                .unwrap_or_else(|e| panic!("variant {variant} must match, got {e:?}"));
        }
    }
}
