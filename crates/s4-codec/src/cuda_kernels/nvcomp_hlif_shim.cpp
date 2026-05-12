/*
 * Phase F-1.5 HLIF C ABI shim.
 *
 * Exposes a flat extern-"C" surface for ferro-compress to drive the nvCOMP
 * High-Level Interface (`nvcompManagerBase`) without the Rust side needing a
 * C++ ABI. The HLIF is what the Phase F-0 PCIe E2E + segment-pipeline
 * benchmarks measured (1300-1800× ingestion vs CPU zstd-22 on gen4 L4/L40S),
 * so wiring the same codepath into the production stack is the shortest
 * path to that performance evidence.
 *
 * Constraints (locked-in choices from Phase F-0 closure):
 *   - Bitcomp algorithm = 0 ("default"). Algo 1 (sparse) is reserved for
 *     i32_low workloads only; this shim doesn't expose it.
 *   - zstd uses nvcompBatchedZstdCompressDefaultOpts — no dict, default
 *     level. Phase F-0 #2 + #2.5 showed dict + high-level both lose on
 *     whole-file segment compression.
 *   - BitstreamKind::NVCOMP_NATIVE — so the compressed buffer is
 *     self-describing and decompress() can run without an external
 *     metadata sidecar.
 *
 * Landmines preserved from Phase F-0 implementation (avoid regressing):
 *   - `CompressionConfig` / `DecompressionConfig` carry a `shared_ptr<Impl>`
 *     and are NOT trivially zero-constructible. Always allocate via
 *     `std::make_unique<CompressionConfig>(uncomp_size)` (HLIF will overwrite
 *     in place during configure_compression). Brace-init `{0,0}` triggers
 *     ctor mismatch.
 *   - The manager owns stream-bound device state and MUST be destroyed
 *     BEFORE the CUDA stream is destroyed. The shim stores its own copy
 *     of the stream and destroys the manager first in its handle dtor.
 *     The caller (Rust side) still owns the stream lifetime.
 *
 * Error convention: int return, 0 = success, non-zero = an opaque code
 * (combined nvcompStatus_t / cudaError_t / shim sentinel). The Rust side
 * maps this back to ferro_compress::Error::Compress / Decompress with a
 * helpful message via ferro_nvcomp_hlif_last_error_message().
 */

#include <cuda_runtime.h>
#include <cstdint>
#include <cstring>
#include <memory>
#include <mutex>
#include <new>
#include <string>

#include "nvcomp/bitcomp.hpp"
#include "nvcomp/zstd.hpp"
#include "nvcomp/nvcompManager.hpp"

using namespace nvcomp;

namespace {

// ferro shim error codes (top 16 bits). Bottom 16 bits carry either a
// nvcompStatus_t or a cudaError_t depending on the source. Picked so the
// Rust side can `errcode >> 16` to discriminate without dragging the full
// nvcomp/cuda enum tables across the FFI boundary.
constexpr int FERRO_SHIM_OK = 0;
constexpr int FERRO_SHIM_NULL_HANDLE = 0x0001'0000;
constexpr int FERRO_SHIM_BAD_ARG     = 0x0002'0000;
constexpr int FERRO_SHIM_CUDA_ERROR  = 0x0003'0000;
constexpr int FERRO_SHIM_NVCOMP_ERR  = 0x0004'0000;
constexpr int FERRO_SHIM_BAD_ALLOC   = 0x0005'0000;
constexpr int FERRO_SHIM_EXCEPTION   = 0x0006'0000;

// Thread-local last error message — appended by every fallible entry point.
// `extern "C"` getter copies the bytes into a caller-supplied buffer so the
// Rust side doesn't have to manage the C++ string lifetime.
thread_local std::string g_last_error_msg;

void set_err(const std::string& msg)
{
  g_last_error_msg = msg;
}

void clear_err()
{
  g_last_error_msg.clear();
}

// HLIF manager handle. Holds the manager + the user_stream it was created
// against. The manager's destructor must run BEFORE the stream is destroyed
// — this is enforced by the caller (Rust side) keeping the stream alive
// past `ferro_nvcomp_hlif_destroy`. We don't own the stream.
struct ManagerHandle {
  std::shared_ptr<nvcompManagerBase> manager;
  cudaStream_t stream = nullptr;
  size_t chunk_size = 0;
  // Cached compression config for the current input size. HLIF allows the
  // config to be reused across calls of the same size; we lazily build /
  // cache it per `compress()` call (size may change call-to-call) but
  // never destroy it before the manager.
  std::unique_ptr<CompressionConfig> cached_comp_cfg;
  size_t cached_comp_cfg_size = 0;
};

}  // namespace

extern "C" {

// Returns 0 on success and writes the manager handle to `*out_handle`.
// On error, writes nullptr to `*out_handle` and returns a non-zero
// FERRO_SHIM_* code (or-merged with the upstream cuda/nvcomp status code
// in the low 16 bits). The Rust side fetches the human-readable detail
// via `ferro_nvcomp_hlif_last_error_message`.
int ferro_nvcomp_hlif_create_bitcomp(
    size_t chunk_size,
    int algorithm,
    nvcompType_t data_type,
    cudaStream_t user_stream,
    void** out_handle)
{
  clear_err();
  if (out_handle == nullptr) {
    set_err("ferro_nvcomp_hlif_create_bitcomp: out_handle is null");
    return FERRO_SHIM_BAD_ARG;
  }
  *out_handle = nullptr;
  if (chunk_size == 0 || chunk_size > nvcompBitcompCompressionMaxAllowedChunkSize) {
    set_err("ferro_nvcomp_hlif_create_bitcomp: chunk_size out of range");
    return FERRO_SHIM_BAD_ARG;
  }
  // Phase F-0 closure pins algorithm to 0. We accept the parameter for
  // forward-compat with algo 1 (sparse i32_low) but reject anything else.
  if (algorithm != 0 && algorithm != 1) {
    set_err("ferro_nvcomp_hlif_create_bitcomp: algorithm must be 0 or 1");
    return FERRO_SHIM_BAD_ARG;
  }
  try {
    nvcompBatchedBitcompCompressOpts_t comp_opts{algorithm, data_type, {0}};
    auto mgr = std::make_shared<BitcompManager>(
        chunk_size,
        comp_opts,
        nvcompBatchedBitcompDecompressDefaultOpts,
        user_stream,
        NoComputeNoVerify);
    auto handle = new (std::nothrow) ManagerHandle();
    if (handle == nullptr) {
      set_err("ferro_nvcomp_hlif_create_bitcomp: bad_alloc on ManagerHandle");
      return FERRO_SHIM_BAD_ALLOC;
    }
    handle->manager = std::move(mgr);
    handle->stream = user_stream;
    handle->chunk_size = chunk_size;
    *out_handle = handle;
    return FERRO_SHIM_OK;
  } catch (const std::bad_alloc& e) {
    set_err(std::string("BitcompManager: bad_alloc: ") + e.what());
    return FERRO_SHIM_BAD_ALLOC;
  } catch (const std::exception& e) {
    set_err(std::string("BitcompManager: exception: ") + e.what());
    return FERRO_SHIM_EXCEPTION;
  } catch (...) {
    set_err("BitcompManager: unknown exception");
    return FERRO_SHIM_EXCEPTION;
  }
}

// zstd HLIF — Phase F-0 採択 codec for text / keyword / json columns.
// Uses default opts (no dict, default level). Phase F-0 #2 + #2.5
// validation pinned dict-off as the canonical setting for whole-file
// segment compression.
int ferro_nvcomp_hlif_create_zstd(
    size_t chunk_size,
    cudaStream_t user_stream,
    void** out_handle)
{
  clear_err();
  if (out_handle == nullptr) {
    set_err("ferro_nvcomp_hlif_create_zstd: out_handle is null");
    return FERRO_SHIM_BAD_ARG;
  }
  *out_handle = nullptr;
  // Zstd HLIF requires chunk_size <= nvcompZstdCompressionMaxAllowedChunkSize
  // (16 MiB), and Phase F-0 measurement showed 64-128 KiB optimal. Accept
  // anything <= 16 MiB; let nvcomp itself enforce the upper bound during
  // configure_compression for early-detection.
  if (chunk_size == 0 || chunk_size > (1 << 24)) {
    set_err("ferro_nvcomp_hlif_create_zstd: chunk_size out of range");
    return FERRO_SHIM_BAD_ARG;
  }
  try {
    auto mgr = std::make_shared<ZstdManager>(
        chunk_size,
        nvcompBatchedZstdCompressDefaultOpts,
        nvcompBatchedZstdDecompressDefaultOpts,
        user_stream,
        NoComputeNoVerify);
    auto handle = new (std::nothrow) ManagerHandle();
    if (handle == nullptr) {
      set_err("ferro_nvcomp_hlif_create_zstd: bad_alloc on ManagerHandle");
      return FERRO_SHIM_BAD_ALLOC;
    }
    handle->manager = std::move(mgr);
    handle->stream = user_stream;
    handle->chunk_size = chunk_size;
    *out_handle = handle;
    return FERRO_SHIM_OK;
  } catch (const std::bad_alloc& e) {
    set_err(std::string("ZstdManager: bad_alloc: ") + e.what());
    return FERRO_SHIM_BAD_ALLOC;
  } catch (const std::exception& e) {
    set_err(std::string("ZstdManager: exception: ") + e.what());
    return FERRO_SHIM_EXCEPTION;
  } catch (...) {
    set_err("ZstdManager: unknown exception");
    return FERRO_SHIM_EXCEPTION;
  }
}

// Destroy the manager handle. The user_stream MUST still be alive at this
// point — the manager's dtor runs synchronous cleanup on that stream. The
// shim never destroys the stream itself; ownership stays with the Rust
// caller.
void ferro_nvcomp_hlif_destroy(void* handle)
{
  if (handle == nullptr) {
    return;
  }
  auto* h = static_cast<ManagerHandle*>(handle);
  // Order matters: drop cached_comp_cfg before the manager, then the
  // manager itself, then the handle. Configs hold shared_ptr<Impl> bound
  // to manager-owned state.
  h->cached_comp_cfg.reset();
  h->manager.reset();
  delete h;
}

// Returns the maximum size in bytes of the compressed buffer for an input
// of `uncomp_bytes`. This is HLIF's `configure_compression(uncomp_bytes)
// .max_compressed_buffer_size`. The result is conservative (worst case
// pass-through + nvCOMP framing overhead) so the caller can safely
// allocate an output buffer of this size up-front.
//
// Side effect: caches the config object inside the handle so the matching
// compress() call doesn't have to re-build it.
int ferro_nvcomp_hlif_max_compressed_size(
    void* handle,
    size_t uncomp_bytes,
    size_t* out_max_bytes)
{
  clear_err();
  if (handle == nullptr || out_max_bytes == nullptr) {
    set_err("ferro_nvcomp_hlif_max_compressed_size: null handle or out_max_bytes");
    return FERRO_SHIM_NULL_HANDLE;
  }
  auto* h = static_cast<ManagerHandle*>(handle);
  try {
    auto cfg = std::make_unique<CompressionConfig>(
        h->manager->configure_compression(uncomp_bytes));
    *out_max_bytes = cfg->max_compressed_buffer_size;
    h->cached_comp_cfg = std::move(cfg);
    h->cached_comp_cfg_size = uncomp_bytes;
    return FERRO_SHIM_OK;
  } catch (const std::exception& e) {
    set_err(std::string("configure_compression: ") + e.what());
    return FERRO_SHIM_EXCEPTION;
  } catch (...) {
    set_err("configure_compression: unknown exception");
    return FERRO_SHIM_EXCEPTION;
  }
}

// Compress `d_in` (uncomp_bytes bytes on device) into `d_out` (sized to
// the value from max_compressed_size). On success the runtime-compressed
// size (≤ max) is written to `*out_comp_bytes` (host pointer).
//
// Both `d_in` and `d_out` are DEVICE pointers — H2D / D2H staging is the
// caller's responsibility. The HLIF call runs entirely on the user_stream
// the manager was created against; this entry point synchronises that
// stream before reading the compressed size so the caller doesn't need
// to. (HLIF's `get_compressed_output_size` does a sync internally too.)
int ferro_nvcomp_hlif_compress(
    void* handle,
    const uint8_t* d_in,
    size_t uncomp_bytes,
    uint8_t* d_out,
    size_t* out_comp_bytes)
{
  clear_err();
  if (handle == nullptr || out_comp_bytes == nullptr) {
    set_err("ferro_nvcomp_hlif_compress: null handle or out_comp_bytes");
    return FERRO_SHIM_NULL_HANDLE;
  }
  if (d_in == nullptr || d_out == nullptr) {
    set_err("ferro_nvcomp_hlif_compress: null device pointer");
    return FERRO_SHIM_BAD_ARG;
  }
  auto* h = static_cast<ManagerHandle*>(handle);
  try {
    // Re-configure if the cached config doesn't match the requested size.
    // (Compression configs are size-specific.)
    if (!h->cached_comp_cfg || h->cached_comp_cfg_size != uncomp_bytes) {
      h->cached_comp_cfg = std::make_unique<CompressionConfig>(
          h->manager->configure_compression(uncomp_bytes));
      h->cached_comp_cfg_size = uncomp_bytes;
    }
    h->manager->compress(d_in, d_out, *h->cached_comp_cfg);
    // get_compressed_output_size synchronises the stream internally and
    // copies the runtime compressed size back from device memory. This
    // is the same pattern Phase F-0 #3 used.
    *out_comp_bytes = h->manager->get_compressed_output_size(d_out);
    return FERRO_SHIM_OK;
  } catch (const std::exception& e) {
    set_err(std::string("compress: ") + e.what());
    return FERRO_SHIM_EXCEPTION;
  } catch (...) {
    set_err("compress: unknown exception");
    return FERRO_SHIM_EXCEPTION;
  }
}

// Decompress `d_comp` (NVCOMP_NATIVE framed compressed payload, on device)
// into `d_out`. Caller pre-sizes `d_out` to at least the value from
// `ferro_nvcomp_hlif_get_decompressed_output_size`. The actual decompressed
// size is written to `*out_decomp_bytes` for the caller's bookkeeping.
//
// Both pointers are DEVICE pointers. Stream synchronisation is the
// caller's responsibility for further work (we don't sync on the way out
// — Rust side calls cudaStreamSynchronize when it wants the bytes
// host-side).
int ferro_nvcomp_hlif_decompress(
    void* handle,
    const uint8_t* d_comp,
    uint8_t* d_out,
    size_t* out_decomp_bytes)
{
  clear_err();
  if (handle == nullptr) {
    set_err("ferro_nvcomp_hlif_decompress: null handle");
    return FERRO_SHIM_NULL_HANDLE;
  }
  if (d_comp == nullptr || d_out == nullptr) {
    set_err("ferro_nvcomp_hlif_decompress: null device pointer");
    return FERRO_SHIM_BAD_ARG;
  }
  auto* h = static_cast<ManagerHandle*>(handle);
  try {
    // configure_decompression(d_comp) synchronises the stream internally
    // because it has to read the NVCOMP_NATIVE header from device memory.
    auto decomp_cfg = h->manager->configure_decompression(d_comp);
    h->manager->decompress(d_out, d_comp, decomp_cfg);
    if (out_decomp_bytes != nullptr) {
      *out_decomp_bytes = decomp_cfg.decomp_data_size;
    }
    return FERRO_SHIM_OK;
  } catch (const std::exception& e) {
    set_err(std::string("decompress: ") + e.what());
    return FERRO_SHIM_EXCEPTION;
  } catch (...) {
    set_err("decompress: unknown exception");
    return FERRO_SHIM_EXCEPTION;
  }
}

// Inspect the decompressed size encoded in an NVCOMP_NATIVE frame without
// actually decompressing. Used by the Rust side to size the output buffer.
// Synchronises the stream because it parses the header from device memory.
int ferro_nvcomp_hlif_get_decompressed_output_size(
    void* handle,
    const uint8_t* d_comp,
    size_t* out_decomp_bytes)
{
  clear_err();
  if (handle == nullptr || d_comp == nullptr || out_decomp_bytes == nullptr) {
    set_err("ferro_nvcomp_hlif_get_decompressed_output_size: null arg");
    return FERRO_SHIM_NULL_HANDLE;
  }
  auto* h = static_cast<ManagerHandle*>(handle);
  try {
    *out_decomp_bytes = h->manager->get_decompressed_output_size(d_comp);
    return FERRO_SHIM_OK;
  } catch (const std::exception& e) {
    set_err(std::string("get_decompressed_output_size: ") + e.what());
    return FERRO_SHIM_EXCEPTION;
  } catch (...) {
    set_err("get_decompressed_output_size: unknown exception");
    return FERRO_SHIM_EXCEPTION;
  }
}

// Copy the last-error message into the caller-supplied buffer. Returns the
// number of bytes that WOULD have been written (excluding the NUL
// terminator) — same semantics as snprintf. The Rust side passes a fresh
// Vec<u8> of `buf_capacity` length; if the function returns >= buf_capacity
// the caller can grow and retry, but in practice ~1 KiB is enough for
// every message the shim produces.
size_t ferro_nvcomp_hlif_last_error_message(char* buf, size_t buf_capacity)
{
  const std::string& msg = g_last_error_msg;
  if (buf == nullptr || buf_capacity == 0) {
    return msg.size();
  }
  const size_t copy_len = msg.size() < (buf_capacity - 1) ? msg.size() : (buf_capacity - 1);
  std::memcpy(buf, msg.data(), copy_len);
  buf[copy_len] = '\0';
  return msg.size();
}

}  // extern "C"
