//! Vendored subset of `ferro-compress` (Apache-2.0 OR MIT) integrated as an
//! internal s4-codec module. Originally a separate crate vendored under
//! `crates/s4-codec/vendor/ferro-compress/`; physically merged for crates.io
//! publication so downstream `cargo install --features gpu` works without an
//! upstream crates.io release of ferro-compress.
//!
//! ## What is included
//!
//! Only the GPU/nvCOMP path needed by S4's [`crate::nvcomp`] adapter. Tantivy
//! bitmap-op kernels, CPU codec features, and stats reduction are NOT vendored.
//!
//! ## License
//!
//! These files retain their original `Apache-2.0 OR MIT` dual-license. See the
//! repository-root `NOTICE` file for the upstream attribution.

#![cfg_attr(feature = "nvcomp-gpu", deny(unsafe_op_in_unsafe_fn))]
#![allow(unsafe_code)]
#![allow(dead_code)]
// nvCOMP / CUDA bindings are FFI-only; the workspace-wide `unsafe_code = deny`
// is overridden here. Each `unsafe` call site keeps its own SAFETY comment.
// `dead_code` is allowed because S4 only uses a subset of upstream's API surface
// (NvcompCodec) — keeping the rest minimises diff against upstream vendor.

mod algo;
mod error;

#[cfg(feature = "nvcomp-gpu")]
pub mod nvcomp_sys;

// v1.2 GPU small-PUT batching: `pub(crate)` (was private) so the sibling
// `crate::nvcomp_batched` module can reuse the FCG1 framing helpers
// (`write_header`, `check_cuda`, ...) — visibility-only change, zero
// behaviour difference for existing callers.
#[cfg(feature = "nvcomp-gpu")]
pub(crate) mod nvcomp;

#[cfg(feature = "nvcomp-gpu")]
mod nvcomp_hlif;

#[cfg(feature = "nvcomp-gpu")]
mod bitcomp_device;

#[cfg(feature = "nvcomp-gpu")]
mod slab_alloc;

pub use algo::{Algo, BitcompDataType, Tier};
pub use error::{Error, Result};

#[cfg(feature = "nvcomp-gpu")]
pub use nvcomp::NvcompCodec;

#[cfg(feature = "nvcomp-gpu")]
pub use nvcomp_hlif::{
    BitcompHlifBackend, DEFAULT_HLIF_CHUNK_SIZE, ZstdHlifBackend, cuda_available,
};

#[cfg(feature = "nvcomp-gpu")]
pub use bitcomp_device::BitcompDeviceCodec;

#[cfg(feature = "nvcomp-gpu")]
pub use slab_alloc::{SLAB_MAX_BUCKET_BYTES, SLAB_MIN_BUCKET_BYTES, SlabAllocator};

/// Compression / decompression interface (verbatim from upstream
/// `ferro_compress::Codec`).
pub trait Codec: Send + Sync {
    fn algo(&self) -> Algo;

    fn compress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()>;

    fn decompress(&self, input: &[u8], output: &mut Vec<u8>) -> Result<()>;

    fn compress_batch(&self, inputs: &[&[u8]], outputs: &mut [Vec<u8>]) -> Result<()> {
        if inputs.len() != outputs.len() {
            return Err(Error::BatchLenMismatch {
                inputs: inputs.len(),
                outputs: outputs.len(),
            });
        }
        for (i, out) in inputs.iter().zip(outputs.iter_mut()) {
            self.compress(i, out)?;
        }
        Ok(())
    }

    fn decompress_batch(&self, inputs: &[&[u8]], outputs: &mut [Vec<u8>]) -> Result<()> {
        if inputs.len() != outputs.len() {
            return Err(Error::BatchLenMismatch {
                inputs: inputs.len(),
                outputs: outputs.len(),
            });
        }
        for (i, out) in inputs.iter().zip(outputs.iter_mut()) {
            self.decompress(i, out)?;
        }
        Ok(())
    }

    fn max_compressed_len(&self, uncompressed_len: usize) -> usize;
}
