// PyO3 0.22 ? on PyResult triggers `useless_conversion` because `From<PyErr>
// for PyErr` is identity. The clippy warning is intrinsic to the binding
// idiom; suppress at file scope.
#![allow(clippy::useless_conversion)]
//! Python bindings for `s4-codec`.
//!
//! Exposes the CPU codecs (`CpuZstd`, `CpuGzip`) and a `gpu_available()`
//! helper. GPU codec classes are intentionally NOT exposed in v1.0 — the
//! `nvcomp-gpu` cargo feature on this crate forwards to the underlying
//! `s4-codec` GPU paths for the server build, but the Python module's
//! runtime classes remain CPU-only. See `crates/s4-codec-py/README.md`
//! for the rationale; GPU-Python exposure is a v1.x roadmap candidate.
//!
//! # Async bridge
//!
//! `s4_codec_rs::Codec` is async. Python callers expect blocking calls. We
//! resolve this by running each call on a process-wide multi-thread tokio
//! runtime stashed in a `OnceLock`. `Python::detach` (pyo3 0.26+ name for allow_threads) releases the
//! GIL across the await so other Python threads can progress while the
//! blocking compression worker churns.

use std::sync::{Arc, OnceLock};

use bytes::{Bytes, BytesMut};
use pyo3::create_exception;
use pyo3::exceptions::{PyIOError, PyNotImplementedError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};
use s4_codec_rs::index::{FrameIndex, IndexError};
use s4_codec_rs::multipart::{write_frame, FrameError, FrameHeader, FrameIter};
use s4_codec_rs::{cpu_gzip, cpu_zstd, cpu_zstd_dict, ChunkManifest, Codec, CodecError, CodecKind};
use tokio::runtime::{Builder, Runtime};

// v0.8.5 #85 M-5: surface CodecError variants as discriminable Python
// exception classes so callers can `except S4CrcMismatchError:` instead of
// string-matching on a flattened `PyValueError`. Hierarchy:
//
//   S4Error (base, ⊂ ValueError for backward-compat with code that catches
//            ValueError from the previous flat mapping)
//     ├─ S4CrcMismatchError              (CodecError::CrcMismatch)
//     ├─ S4SizeMismatchError             (CodecError::SizeMismatch)
//     ├─ S4CodecMismatchError            (CodecError::CodecMismatch)
//     ├─ S4UnregisteredCodecError        (CodecError::UnregisteredCodec)
//     ├─ S4ManifestSizeExceedsLimitError (CodecError::ManifestSizeExceedsLimit)
//     └─ S4ManifestSizeMismatchError     (CodecError::ManifestSizeMismatch)
//   S4BackendError (⊂ RuntimeError) — wraps anyhow / nvCOMP backend faults
//   S4IoError      (⊂ IOError)      — wraps std::io::Error
//
// `Backend` and `Io` deliberately do NOT inherit S4Error: they map onto
// stdlib semantics (RuntimeError / IOError) so frameworks already wired to
// retry-on-IOError continue to do the right thing. `TruncatedStream` is rare
// enough on the binding surface (server-side streaming) that we leave it on
// the S4Error base rather than minting another class.
create_exception!(s4_codec, S4Error, PyValueError);
create_exception!(s4_codec, S4CrcMismatchError, S4Error);
create_exception!(s4_codec, S4SizeMismatchError, S4Error);
create_exception!(s4_codec, S4CodecMismatchError, S4Error);
create_exception!(s4_codec, S4UnregisteredCodecError, S4Error);
create_exception!(s4_codec, S4ManifestSizeExceedsLimitError, S4Error);
create_exception!(s4_codec, S4ManifestSizeMismatchError, S4Error);
create_exception!(s4_codec, S4BackendError, PyRuntimeError);
create_exception!(s4_codec, S4IoError, PyIOError);
// v1.1 s4fs: frame-parse / sidecar-decode failures. Both sit under the
// S4Error base (⊂ ValueError) so existing `except S4Error:` handlers
// catch them without changes. Messages carry the upstream Display text
// (`FrameError` / `IndexError` are `#[non_exhaustive]`, so we map by
// message rather than per-variant subclasses).
create_exception!(s4_codec, S4FrameError, S4Error);
create_exception!(s4_codec, S4IndexError, S4Error);

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .thread_name("s4-codec-py")
            .build()
            .expect("failed to start tokio runtime for s4_codec python binding")
    })
}

fn codec_err_to_py(e: CodecError) -> PyErr {
    use s4_codec_rs::CodecError::*;
    match e {
        SizeMismatch { expected, got } => {
            S4SizeMismatchError::new_err(format!("size mismatch: expected {expected}, got {got}"))
        }
        CrcMismatch { expected, got } => S4CrcMismatchError::new_err(format!(
            "crc32c mismatch: expected {expected:#010x}, got {got:#010x}"
        )),
        CodecMismatch { expected, got } => S4CodecMismatchError::new_err(format!(
            "codec mismatch: expected {expected:?}, got {got:?}"
        )),
        UnregisteredCodec(k) => {
            S4UnregisteredCodecError::new_err(format!("codec {k:?} not registered"))
        }
        ManifestSizeExceedsLimit { requested, limit } => S4ManifestSizeExceedsLimitError::new_err(
            format!("manifest claims {requested} bytes but limit is {limit}"),
        ),
        ManifestSizeMismatch { manifest, actual } => S4ManifestSizeMismatchError::new_err(format!(
            "manifest claims {manifest} bytes but body is {actual}"
        )),
        Backend(msg) => S4BackendError::new_err(format!("backend: {msg}")),
        Io(e) => S4IoError::new_err(format!("io: {e}")),
        TruncatedStream { expected, got } => S4Error::new_err(format!(
            "stream truncated: expected {expected} input bytes, got {got}"
        )),
        // v0.8.15 M-4: AWS S3 over-length analogue (declared
        // Content-Length smaller than the wire body). Same shape as
        // TruncatedStream — surface to Python callers via the
        // generic `S4Error` since the in-process codec doesn't
        // emit HTTP status codes.
        OverlengthStream { expected, got } => S4Error::new_err(format!(
            "stream over-length: expected {expected} input bytes, got at least {got}"
        )),
        // `Join` is a tokio internal that surfaces only if a blocking worker
        // panics — surface as backend so retries hit the same class as
        // anyhow-wrapped backend faults.
        Join(e) => S4BackendError::new_err(format!("backend (worker join): {e}")),
        // v1.0 F1: `CodecError` is `#[non_exhaustive]`, so newly-added
        // variants in a future minor release must have a fallback here.
        // Map to the generic `S4Error` carrying the upstream Display
        // text — Python callers see the message but cannot pattern-
        // match on the specific subclass until this wrapper is updated.
        other => S4Error::new_err(format!("codec error: {other}")),
    }
}

fn manifest_from_parts(
    kind: CodecKind,
    payload_len: u64,
    original_size: u64,
    crc32c: u32,
) -> ChunkManifest {
    ChunkManifest {
        codec: kind,
        original_size,
        compressed_size: payload_len,
        crc32c,
    }
}

/// Run the supplied future on the shared multi-thread runtime, releasing
/// the GIL while it runs so other Python threads aren't starved by a
/// long-running compression worker.
fn block_on<F, T>(py: Python<'_>, fut: F) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
{
    py.detach(|| runtime().block_on(fut))
}

/// CPU zstd codec. Level is clamped to 1..=22 by the underlying crate;
/// default 3 matches `zstd(1)`'s out-of-the-box level.
#[pyclass(name = "CpuZstd", module = "s4_codec")]
struct PyCpuZstd {
    inner: cpu_zstd::CpuZstd,
}

#[pymethods]
impl PyCpuZstd {
    #[new]
    #[pyo3(signature = (level = 3))]
    fn new(level: i32) -> Self {
        Self {
            inner: cpu_zstd::CpuZstd::new(level),
        }
    }

    /// Compress `data`. Returns `(compressed: bytes, original_size: int, crc32c: int)`.
    /// The original size and crc32c are the manifest fields decompress needs;
    /// the caller is expected to round-trip them alongside the payload.
    fn compress<'py>(
        &self,
        py: Python<'py>,
        data: &Bound<'py, PyBytes>,
    ) -> PyResult<(Bound<'py, PyBytes>, u64, u32)> {
        let input = Bytes::copy_from_slice(data.as_bytes());
        let codec = self.inner.clone();
        let (out, manifest) =
            block_on(py, async move { codec.compress(input).await }).map_err(codec_err_to_py)?;
        Ok((
            PyBytes::new(py, &out),
            manifest.original_size,
            manifest.crc32c,
        ))
    }

    /// Decompress `data`. `original_size` and `crc32c` are the matching
    /// manifest fields returned by `compress`.
    fn decompress<'py>(
        &self,
        py: Python<'py>,
        data: &Bound<'py, PyBytes>,
        original_size: u64,
        crc32c: u32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let input = Bytes::copy_from_slice(data.as_bytes());
        let manifest = manifest_from_parts(
            CodecKind::CpuZstd,
            input.len() as u64,
            original_size,
            crc32c,
        );
        let codec = self.inner.clone();
        let out = block_on(py, async move { codec.decompress(input, &manifest).await })
            .map_err(codec_err_to_py)?;
        Ok(PyBytes::new(py, &out))
    }

    fn __repr__(&self) -> String {
        format!("CpuZstd(level={})", cpu_zstd::CpuZstd::DEFAULT_LEVEL)
    }
}

/// CPU gzip codec (RFC 1952). Level 0..=9, default 6 (matches `gzip(1)`).
/// Output is a real gzip stream — any standard `gunzip`-aware decoder
/// (browser, curl, Python's `gzip` module) decodes the payload bytes.
#[pyclass(name = "CpuGzip", module = "s4_codec")]
struct PyCpuGzip {
    inner: cpu_gzip::CpuGzip,
}

#[pymethods]
impl PyCpuGzip {
    #[new]
    #[pyo3(signature = (level = 6))]
    fn new(level: u32) -> Self {
        Self {
            inner: cpu_gzip::CpuGzip::new(level),
        }
    }

    fn compress<'py>(
        &self,
        py: Python<'py>,
        data: &Bound<'py, PyBytes>,
    ) -> PyResult<(Bound<'py, PyBytes>, u64, u32)> {
        let input = Bytes::copy_from_slice(data.as_bytes());
        let codec = self.inner.clone();
        let (out, manifest) =
            block_on(py, async move { codec.compress(input).await }).map_err(codec_err_to_py)?;
        Ok((
            PyBytes::new(py, &out),
            manifest.original_size,
            manifest.crc32c,
        ))
    }

    fn decompress<'py>(
        &self,
        py: Python<'py>,
        data: &Bound<'py, PyBytes>,
        original_size: u64,
        crc32c: u32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let input = Bytes::copy_from_slice(data.as_bytes());
        let manifest = manifest_from_parts(
            CodecKind::CpuGzip,
            input.len() as u64,
            original_size,
            crc32c,
        );
        let codec = self.inner.clone();
        let out = block_on(py, async move { codec.decompress(input, &manifest).await })
            .map_err(codec_err_to_py)?;
        Ok(PyBytes::new(py, &out))
    }

    fn __repr__(&self) -> String {
        format!("CpuGzip(level={})", cpu_gzip::CpuGzip::DEFAULT_LEVEL)
    }
}

/// CPU zstd codec bound to a shared trained dictionary (`cpu-zstd-dict`,
/// v1.1 `--zstd-dict`). The dictionary is a stock zstd dictionary
/// (`zstd --train` / `ZDICT_trainFromBuffer` output); the compressed
/// payload is a stock zstd frame referencing it. Level clamped to 1..=22.
#[pyclass(name = "CpuZstdDict", module = "s4_codec")]
struct PyCpuZstdDict {
    inner: cpu_zstd_dict::CpuZstdDict,
    level: i32,
}

#[pymethods]
impl PyCpuZstdDict {
    #[new]
    #[pyo3(signature = (dict_bytes, level = 3))]
    fn new(dict_bytes: &Bound<'_, PyBytes>, level: i32) -> PyResult<Self> {
        let dict: Arc<[u8]> = Arc::from(dict_bytes.as_bytes().to_vec().into_boxed_slice());
        let inner = cpu_zstd_dict::CpuZstdDict::new(dict, level).map_err(codec_err_to_py)?;
        Ok(Self {
            inner,
            level: level.clamp(1, 22),
        })
    }

    /// Compress `data` against the bound dictionary. Returns
    /// `(compressed: bytes, original_size: int, crc32c: int)` — same shape
    /// as `CpuZstd.compress`.
    fn compress<'py>(
        &self,
        py: Python<'py>,
        data: &Bound<'py, PyBytes>,
    ) -> PyResult<(Bound<'py, PyBytes>, u64, u32)> {
        let input = Bytes::copy_from_slice(data.as_bytes());
        let codec = self.inner.clone();
        let (out, manifest) =
            block_on(py, async move { codec.compress(input).await }).map_err(codec_err_to_py)?;
        Ok((
            PyBytes::new(py, &out),
            manifest.original_size,
            manifest.crc32c,
        ))
    }

    /// Decompress `data` against the bound dictionary. `original_size`
    /// and `crc32c` are the matching manifest fields from `compress` (or
    /// the S4F2 frame header for gateway-written objects).
    fn decompress<'py>(
        &self,
        py: Python<'py>,
        data: &Bound<'py, PyBytes>,
        original_size: u64,
        crc32c: u32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let input = Bytes::copy_from_slice(data.as_bytes());
        let manifest = manifest_from_parts(
            CodecKind::CpuZstdDict,
            input.len() as u64,
            original_size,
            crc32c,
        );
        let codec = self.inner.clone();
        let out = block_on(py, async move { codec.decompress(input, &manifest).await })
            .map_err(codec_err_to_py)?;
        Ok(PyBytes::new(py, &out))
    }

    fn __repr__(&self) -> String {
        format!(
            "CpuZstdDict(dict_len={}, level={})",
            self.inner.dict().len(),
            self.level
        )
    }
}

/// True iff the wheel was built with `--features nvcomp-gpu` AND a
/// CUDA-capable GPU is reachable at runtime. Default wheels return False.
#[pyfunction]
fn gpu_available() -> bool {
    s4_codec_rs::nvcomp::is_gpu_available()
}

fn frame_err_to_py(e: FrameError) -> PyErr {
    S4FrameError::new_err(format!("frame error: {e}"))
}

fn index_err_to_py(e: IndexError) -> PyErr {
    S4IndexError::new_err(format!("index error: {e}"))
}

/// S4F2 frame header → Python dict
/// `{codec: str, original_size: int, compressed_size: int, crc32c: int}`.
/// `codec` is the stable `CodecKind::as_str()` name (`"cpu-zstd"`, …).
fn frame_header_dict<'py>(py: Python<'py>, h: &FrameHeader) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("codec", h.codec.as_str())?;
    d.set_item("original_size", h.original_size)?;
    d.set_item("compressed_size", h.compressed_size)?;
    d.set_item("crc32c", h.crc32c)?;
    Ok(d)
}

/// Parse one S4F2 frame off the front of `data`.
///
/// Returns `(header: dict, payload: bytes, rest: bytes)` where `header`
/// is `{codec, original_size, compressed_size, crc32c}`, `payload` is the
/// (still-compressed) frame payload, and `rest` is everything after the
/// frame. Raises `S4FrameError` on truncated / bad-magic / unknown-codec
/// input. Thin wrapper over `s4_codec::multipart::read_frame` — the wire
/// layout is the frozen v1.0 S4F2 format.
#[pyfunction]
fn read_frame<'py>(
    py: Python<'py>,
    data: &Bound<'py, PyBytes>,
) -> PyResult<(Bound<'py, PyDict>, Bound<'py, PyBytes>, Bound<'py, PyBytes>)> {
    let input = Bytes::copy_from_slice(data.as_bytes());
    let (header, payload, rest) =
        s4_codec_rs::multipart::read_frame(input).map_err(frame_err_to_py)?;
    Ok((
        frame_header_dict(py, &header)?,
        PyBytes::new(py, &payload),
        PyBytes::new(py, &rest),
    ))
}

/// Parse `data` as a sequence of S4F2 frames (S4P1 padding frames are
/// skipped, matching the gateway's GET path). Returns a list of
/// `(header: dict, payload: bytes)` tuples. Raises `S4FrameError` on the
/// first malformed frame. Thin wrapper over `s4_codec::multipart::FrameIter`.
#[pyfunction]
fn frame_iter<'py>(py: Python<'py>, data: &Bound<'py, PyBytes>) -> PyResult<Bound<'py, PyList>> {
    let input = Bytes::copy_from_slice(data.as_bytes());
    let out = PyList::empty(py);
    for item in FrameIter::new(input) {
        let (header, payload) = item.map_err(frame_err_to_py)?;
        out.append((frame_header_dict(py, &header)?, PyBytes::new(py, &payload)))?;
    }
    Ok(out)
}

/// Decode a `<key>.s4index` sidecar (v1 / v2 / v3 layouts).
///
/// Returns a dict:
///
/// ```text
/// {
///   "total_padded_size": int,      # backend object size incl. padding
///   "total_original_size": int,    # decompressed size (sum of entries)
///   "source_etag": str | None,     # v2+: backend ETag binding
///   "source_compressed_size": int | None,
///   "entries": [ {original_offset, original_size,
///                 compressed_offset, compressed_size}, ... ],
///   "sse": None | {enc_chunk_size, enc_chunk_count, enc_key_id,
///                  enc_salt: bytes, enc_plaintext_len, enc_header_bytes},
/// }
/// ```
///
/// `entries[i].compressed_size` includes the 28-byte frame header.
/// Raises `S4IndexError` on malformed input. Thin wrapper over
/// `s4_codec::index::decode_index`.
#[pyfunction]
fn decode_index<'py>(py: Python<'py>, data: &Bound<'py, PyBytes>) -> PyResult<Bound<'py, PyDict>> {
    let input = Bytes::copy_from_slice(data.as_bytes());
    let idx: FrameIndex = s4_codec_rs::index::decode_index(input).map_err(index_err_to_py)?;
    let d = PyDict::new(py);
    d.set_item("total_padded_size", idx.total_padded_size)?;
    d.set_item("total_original_size", idx.total_original_size())?;
    d.set_item("source_etag", idx.source_etag.as_deref())?;
    d.set_item("source_compressed_size", idx.source_compressed_size)?;
    let entries = PyList::empty(py);
    for e in &idx.entries {
        let ed = PyDict::new(py);
        ed.set_item("original_offset", e.original_offset)?;
        ed.set_item("original_size", e.original_size)?;
        ed.set_item("compressed_offset", e.compressed_offset)?;
        ed.set_item("compressed_size", e.compressed_size)?;
        entries.append(ed)?;
    }
    d.set_item("entries", entries)?;
    match idx.sse_v3 {
        Some(sse) => {
            let sd = PyDict::new(py);
            sd.set_item("enc_chunk_size", sse.enc_chunk_size)?;
            sd.set_item("enc_chunk_count", sse.enc_chunk_count)?;
            sd.set_item("enc_key_id", sse.enc_key_id)?;
            sd.set_item("enc_salt", PyBytes::new(py, &sse.enc_salt))?;
            sd.set_item("enc_plaintext_len", sse.enc_plaintext_len)?;
            sd.set_item("enc_header_bytes", sse.enc_header_bytes)?;
            d.set_item("sse", sd)?;
        }
        None => d.set_item("sse", py.None())?,
    }
    Ok(d)
}

/// CRC32C (Castagnoli) of `data` — the checksum the S4F2 frame header
/// carries. Exposed so pure-Python readers (s4fs) can verify
/// `passthrough` frame payloads without an extra dependency.
#[pyfunction]
#[pyo3(name = "crc32c")]
fn crc32c_py(data: &Bound<'_, PyBytes>) -> u32 {
    ::crc32c::crc32c(data.as_bytes())
}

// ---------------------------------------------------------------------------
// v1.2 s4fs write support: gateway-equivalent client-side object encoder.
// ---------------------------------------------------------------------------

/// Default S4F2 chunk size for client-side framed writes.
///
/// KEEP IN SYNC with `crates/s4-server/src/streaming.rs`
/// (`DEFAULT_S4F2_CHUNK_SIZE` + `pick_chunk_size`). This crate cannot
/// depend on `s4-server` (the server depends on this codec layer, and the
/// Python wheel must not pull the full gateway), so the gateway's
/// single-PUT chunk policy is replicated here verbatim. A drift between
/// the two would NOT corrupt data (each S4F2 frame is self-describing and
/// the GET path parses whatever frame sizes it finds) but it would change
/// the Range-GET partial-fetch granularity vs gateway-written objects.
const DEFAULT_S4F2_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Replicates `s4_server::streaming::pick_chunk_size(Some(len))` — the
/// chunk size the gateway uses for a single PUT whose `Content-Length`
/// is known (a client-side writer always knows the buffer length, so the
/// `None`/chunked-transfer arm is intentionally not replicated).
///
/// KEEP IN SYNC with `crates/s4-server/src/streaming.rs::pick_chunk_size`:
/// `<= 1 MiB → 1 MiB`, `<= 100 MiB → 4 MiB`, `> 100 MiB → 16 MiB`.
fn pick_chunk_size_impl(content_length: u64) -> usize {
    if content_length <= 1024 * 1024 {
        1024 * 1024
    } else if content_length <= 100 * 1024 * 1024 {
        DEFAULT_S4F2_CHUNK_SIZE
    } else {
        16 * 1024 * 1024
    }
}

/// Chunk size (bytes) the S4 gateway — and [`encode_s4_object`] — uses
/// for an object of `content_length` bytes. Exposed so tests can pin the
/// threshold table without allocating 100 MiB+ bodies.
#[pyfunction]
fn pick_chunk_size(content_length: u64) -> usize {
    pick_chunk_size_impl(content_length)
}

/// Compress `input` into a gateway-identical S4F2 framed body with
/// `cpu-zstd` frames. Mirrors the server's
/// `streaming_compress_to_frames` output for a fully-buffered input:
/// per-chunk frame header carries the *chunk* original size / crc32c,
/// the aggregate manifest carries the rolling crc32c of the whole
/// original body and the framed body length as `compressed_size`.
fn encode_framed_zstd(
    py: Python<'_>,
    input: Bytes,
    level: i32,
) -> PyResult<(Bytes, ChunkManifest)> {
    let chunk_size = pick_chunk_size_impl(input.len() as u64);
    let codec = cpu_zstd::CpuZstd::new(level);
    let res: Result<(Bytes, ChunkManifest), CodecError> = block_on(py, async move {
        let mut framed = BytesMut::new();
        let mut rolling_crc: u32 = 0;
        let total = input.len() as u64;
        let mut off = 0usize;
        while off < input.len() {
            let end = usize::min(off + chunk_size, input.len());
            let chunk = input.slice(off..end);
            let chunk_crc = ::crc32c::crc32c(&chunk);
            rolling_crc = ::crc32c::crc32c_append(rolling_crc, &chunk);
            let chunk_len = chunk.len() as u64;
            let (compressed, _manifest) = codec.compress(chunk).await?;
            let header = FrameHeader {
                codec: CodecKind::CpuZstd,
                original_size: chunk_len,
                compressed_size: compressed.len() as u64,
                crc32c: chunk_crc,
            };
            write_frame(&mut framed, header, &compressed);
            off = end;
        }
        let body = framed.freeze();
        let compressed_size = body.len() as u64;
        Ok((
            body,
            ChunkManifest {
                codec: CodecKind::CpuZstd,
                original_size: total,
                compressed_size,
                crc32c: rolling_crc,
            },
        ))
    });
    res.map_err(codec_err_to_py)
}

/// Encode `data` exactly the way the S4 gateway's single-PUT path would
/// store it, so a client (s4fs) can write S4-format objects **without**
/// the gateway and the gateway / `s4 verify-sidecar` / s4fs itself can
/// read them back.
///
/// Returns a dict:
///
/// ```text
/// {
///   "body":     bytes,         # what to PUT as the object body
///   "sidecar":  bytes | None,  # `<key>.s4index` payload (multi-frame only,
///                              # UNBOUND — pass through `bind_index` with the
///                              # backend ETag/size after the body PUT)
///   "metadata": {str: str},    # S3 user metadata to stamp on the body PUT
/// }
/// ```
///
/// Codec support: `"cpu-zstd"` (S4F2 framed, the gateway default) and
/// `"passthrough"` (raw body, manifest metadata only — matching the
/// gateway, which never frames passthrough). Anything else (cpu-gzip /
/// cpu-zstd-dict / nvcomp-* / dietgpu-ans) raises `NotImplementedError`
/// pointing at the gateway, which owns those codec paths.
///
/// Metadata keys/values restate the gateway's PUT-path stamps.
/// KEEP IN SYNC with `crates/s4-server/src/service.rs` (`write_manifest`
/// + the `META_*` constants and the `META_FRAMED` stamp) and
/// `crates/s4-server/src/migrate.rs` (same five keys):
///
/// - `s4-codec`:           `CodecKind::as_str()` of the manifest codec
/// - `s4-original-size`:   decimal length of the original body
/// - `s4-compressed-size`: decimal length of the stored body (framed
///   bytes incl. the 28-byte headers for cpu-zstd; == original for
///   passthrough)
/// - `s4-crc32c`:          decimal CRC32C of the **original** body
/// - `s4-framed`:          `"true"` — framed bodies only (absent on
///   passthrough, exactly like the gateway)
#[pyfunction]
#[pyo3(signature = (data, codec = "cpu-zstd", level = 3))]
fn encode_s4_object<'py>(
    py: Python<'py>,
    data: &Bound<'py, PyBytes>,
    codec: &str,
    level: i32,
) -> PyResult<Bound<'py, PyDict>> {
    let input = Bytes::copy_from_slice(data.as_bytes());
    let (body, manifest, framed) = match codec {
        "cpu-zstd" => {
            let (body, manifest) = encode_framed_zstd(py, input, level)?;
            (body, manifest, true)
        }
        // Passthrough buys nothing from S4F2 wrapping; the gateway stores
        // it as a raw blob with manifest metadata and NO `s4-framed` flag
        // (see service.rs "Passthrough buys nothing…" comment). Mirror that.
        "passthrough" => {
            // CRC32C is the only real work on this path, but it still
            // scans the whole body — release the GIL for it like the
            // cpu-zstd branch does (audit v1.2-R1 P3).
            let crc = py.detach(|| ::crc32c::crc32c(&input));
            let len = input.len() as u64;
            let manifest = ChunkManifest {
                codec: CodecKind::Passthrough,
                original_size: len,
                compressed_size: len,
                crc32c: crc,
            };
            (input, manifest, false)
        }
        other => {
            return Err(PyNotImplementedError::new_err(format!(
                "encode_s4_object supports codecs 'cpu-zstd' and 'passthrough' only; \
                 {other:?} (cpu-gzip / cpu-zstd-dict / nvcomp-* / dietgpu-ans) must be \
                 written through the S4 gateway, which owns those codec paths"
            )));
        }
    };
    // Sidecar policy mirrors the gateway PUT path (service.rs): emit only
    // for framed bodies with MORE than one frame — a single-frame sidecar
    // buys nothing (partial fetch == full body).
    let sidecar = if framed {
        let idx = s4_codec_rs::index::build_index_from_body(&body).map_err(frame_err_to_py)?;
        if idx.entries.len() > 1 {
            Some(s4_codec_rs::index::encode_index(&idx))
        } else {
            None
        }
    } else {
        None
    };
    let out = PyDict::new(py);
    out.set_item("body", PyBytes::new(py, &body))?;
    match sidecar {
        Some(s) => out.set_item("sidecar", PyBytes::new(py, &s))?,
        None => out.set_item("sidecar", py.None())?,
    }
    let meta = PyDict::new(py);
    // KEEP IN SYNC: literal metadata key names restate s4-server/src/
    // service.rs META_CODEC / META_ORIGINAL_SIZE / META_COMPRESSED_SIZE /
    // META_CRC32C / META_FRAMED (pub(crate) there, so not importable).
    meta.set_item("s4-codec", manifest.codec.as_str())?;
    meta.set_item("s4-original-size", manifest.original_size.to_string())?;
    meta.set_item("s4-compressed-size", manifest.compressed_size.to_string())?;
    meta.set_item("s4-crc32c", manifest.crc32c.to_string())?;
    if framed {
        meta.set_item("s4-framed", "true")?;
    }
    out.set_item("metadata", meta)?;
    Ok(out)
}

/// Stamp the v2 source binding (`source_etag` + `source_compressed_size`)
/// into a `.s4index` sidecar produced by [`encode_s4_object`].
///
/// The gateway stamps these AFTER its backend PUT returns the
/// authoritative ETag (service.rs `put_object` → `write_sidecar`); a
/// client-side writer must do the same: PUT the body, read the backend
/// ETag, then `bind_index` and PUT the sidecar. Without the binding the
/// sidecar decodes as "legacy v1 / unbound" and neither the gateway's
/// Range-GET fast-path nor s4fs's partial fetch will trust it
/// (`s4 verify-sidecar` reports `LegacyV1` instead of `OK`).
///
/// `source_etag` must be the **quote-stripped** entity-tag (the form s3s
/// `ETag::value()` yields and the gateway writes; see
/// `s4-server/src/repair.rs::normalize_etag`). Pass `None` on
/// ETag-less backends — the size-only binding is still a valid v2
/// sidecar the verifier accepts.
#[pyfunction]
#[pyo3(signature = (sidecar, source_compressed_size, source_etag = None))]
fn bind_index<'py>(
    py: Python<'py>,
    sidecar: &Bound<'py, PyBytes>,
    source_compressed_size: u64,
    source_etag: Option<String>,
) -> PyResult<Bound<'py, PyBytes>> {
    let raw = Bytes::copy_from_slice(sidecar.as_bytes());
    let mut idx: FrameIndex = s4_codec_rs::index::decode_index(raw).map_err(index_err_to_py)?;
    idx.source_compressed_size = Some(source_compressed_size);
    idx.source_etag = source_etag;
    Ok(PyBytes::new(py, &s4_codec_rs::index::encode_index(&idx)))
}

#[pymodule]
fn s4_codec(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyCpuZstd>()?;
    m.add_class::<PyCpuGzip>()?;
    m.add_class::<PyCpuZstdDict>()?;
    m.add_function(wrap_pyfunction!(gpu_available, m)?)?;
    // v1.1 s4fs: read-side wire-format helpers (S4F2 frames + .s4index
    // sidecars) so pure-Python clients can decode gateway-written
    // objects without the gateway.
    m.add_function(wrap_pyfunction!(read_frame, m)?)?;
    m.add_function(wrap_pyfunction!(frame_iter, m)?)?;
    m.add_function(wrap_pyfunction!(decode_index, m)?)?;
    m.add_function(wrap_pyfunction!(crc32c_py, m)?)?;
    // v1.2 s4fs write support: gateway-equivalent client-side encoder +
    // sidecar version binding + the gateway's chunk-size policy table.
    m.add_function(wrap_pyfunction!(encode_s4_object, m)?)?;
    m.add_function(wrap_pyfunction!(bind_index, m)?)?;
    m.add_function(wrap_pyfunction!(pick_chunk_size, m)?)?;
    m.add(
        "FRAME_MAGIC",
        PyBytes::new(py, s4_codec_rs::multipart::FRAME_MAGIC),
    )?;
    m.add(
        "PADDING_MAGIC",
        PyBytes::new(py, s4_codec_rs::multipart::PADDING_MAGIC),
    )?;
    m.add(
        "FRAME_HEADER_BYTES",
        s4_codec_rs::multipart::FRAME_HEADER_BYTES,
    )?;
    m.add("SIDECAR_SUFFIX", s4_codec_rs::index::SIDECAR_SUFFIX)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    // v0.8.5 #85 M-5: export per-CodecError exception classes so Python
    // callers can branch on error kind. See module-level doc comments above
    // `create_exception!` for the inheritance hierarchy.
    m.add("S4Error", py.get_type::<S4Error>())?;
    m.add("S4CrcMismatchError", py.get_type::<S4CrcMismatchError>())?;
    m.add("S4SizeMismatchError", py.get_type::<S4SizeMismatchError>())?;
    m.add(
        "S4CodecMismatchError",
        py.get_type::<S4CodecMismatchError>(),
    )?;
    m.add(
        "S4UnregisteredCodecError",
        py.get_type::<S4UnregisteredCodecError>(),
    )?;
    m.add(
        "S4ManifestSizeExceedsLimitError",
        py.get_type::<S4ManifestSizeExceedsLimitError>(),
    )?;
    m.add(
        "S4ManifestSizeMismatchError",
        py.get_type::<S4ManifestSizeMismatchError>(),
    )?;
    m.add("S4BackendError", py.get_type::<S4BackendError>())?;
    m.add("S4IoError", py.get_type::<S4IoError>())?;
    m.add("S4FrameError", py.get_type::<S4FrameError>())?;
    m.add("S4IndexError", py.get_type::<S4IndexError>())?;
    Ok(())
}
