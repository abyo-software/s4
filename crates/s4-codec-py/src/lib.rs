// PyO3 0.22 ? on PyResult triggers `useless_conversion` because `From<PyErr>
// for PyErr` is identity. The clippy warning is intrinsic to the binding
// idiom; suppress at file scope.
#![allow(clippy::useless_conversion)]
//! Python bindings for `s4-codec` (v0.4 #23).
//!
//! Exposes the CPU codecs (`CpuZstd`, `CpuGzip`) and a `gpu_available()`
//! helper. GPU codec classes (`NvcompZstd`, `NvcompBitcomp`,
//! `NvcompGDeflate`) are gated behind the `nvcomp-gpu` cargo feature so
//! the default `pip install s4-codec` wheel doesn't require a CUDA toolchain
//! at build time. Build with `maturin build --release --features nvcomp-gpu`
//! on a machine with `NVCOMP_HOME` set to opt in.
//!
//! # Async bridge
//!
//! `s4_codec_rs::Codec` is async. Python callers expect blocking calls. We
//! resolve this by running each call on a process-wide multi-thread tokio
//! runtime stashed in a `OnceLock`. `Python::allow_threads` releases the
//! GIL across the await so other Python threads can progress while the
//! blocking compression worker churns.

use std::sync::OnceLock;

use bytes::Bytes;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use s4_codec_rs::{cpu_gzip, cpu_zstd, ChunkManifest, Codec, CodecError, CodecKind};
use tokio::runtime::{Builder, Runtime};

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
    PyValueError::new_err(format!("s4-codec error: {e}"))
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
    py.allow_threads(|| runtime().block_on(fut))
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
            PyBytes::new_bound(py, &out),
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
        Ok(PyBytes::new_bound(py, &out))
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
            PyBytes::new_bound(py, &out),
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
        Ok(PyBytes::new_bound(py, &out))
    }

    fn __repr__(&self) -> String {
        format!("CpuGzip(level={})", cpu_gzip::CpuGzip::DEFAULT_LEVEL)
    }
}

/// True iff the wheel was built with `--features nvcomp-gpu` AND a
/// CUDA-capable GPU is reachable at runtime. Default wheels return False.
#[pyfunction]
fn gpu_available() -> bool {
    s4_codec_rs::nvcomp::is_gpu_available()
}

#[pymodule]
fn s4_codec(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyCpuZstd>()?;
    m.add_class::<PyCpuGzip>()?;
    m.add_function(wrap_pyfunction!(gpu_available, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
