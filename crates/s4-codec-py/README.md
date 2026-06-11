# s4-codec (Python bindings)

In-process CPU compression (zstd + gzip) from Python — no S4 gateway required.
Wraps the same Rust [`s4-codec`](../s4-codec) crate that powers the S4
S3-compatible storage gateway, so a Python notebook / Airflow task / Spark
UDF can compress and decompress with the exact same byte format as
objects sitting in an S4 bucket. (GPU codecs are intentionally NOT exposed
in Python in v1.0 — they require a CUDA toolchain + GPU at runtime, which
is a poor fit for `pip install`. Workloads that need GPU compression
should route through the `s4` server gateway instead. Python GPU exposure
is a v1.x roadmap candidate.)

## Install

```bash
pip install s4-codec        # CPU codecs only (zstd + gzip)
```

## Example

```python
from s4_codec import CpuZstd, CpuGzip, gpu_available

codec = CpuZstd(level=3)
data = b"hello squished s3 " * 10_000
compressed, original_size, crc = codec.compress(data)
roundtrip = codec.decompress(compressed, original_size, crc)
assert roundtrip == data

# RFC 1952 gzip output — decodable by any standard `gunzip`-aware client.
gz_compressed, *_ = CpuGzip(level=6).compress(data)
assert gz_compressed[:2] == b"\x1f\x8b"

print("GPU available:", gpu_available())
```

## API

| Class / function | Purpose |
| --- | --- |
| `CpuZstd(level: int = 3)` | CPU zstd, level 1..=22. |
| `CpuGzip(level: int = 6)` | CPU gzip (RFC 1952), level 0..=9. |
| `<codec>.compress(data: bytes) -> (bytes, int, int)` | Returns `(compressed, original_size, crc32c)`. |
| `<codec>.decompress(data, original_size, crc32c) -> bytes` | Inverse of `compress`. |
| `gpu_available() -> bool` | True iff the wheel was built with `--features nvcomp-gpu` and a CUDA-capable GPU is reachable. |
| `CpuZstdDict(dict_bytes, level: int = 3)` | CPU zstd bound to a trained dictionary (`cpu-zstd-dict` objects). |
| `read_frame(data)` / `frame_iter(data)` | Parse S4F2 frames (wire format the gateway writes); used by s4fs reads. |
| `decode_index(data) -> dict` | Decode a `<key>.s4index` sidecar (v1/v2/v3). |
| `crc32c(data) -> int` | CRC32C (Castagnoli), the S4F2 header checksum. |
| `encode_s4_object(data, codec="cpu-zstd", level=3) -> dict` | Gateway-identical single-PUT encoding: `{"body", "sidecar", "metadata"}` — framed body, optional multi-frame `.s4index` payload, and the S3 user-metadata manifest to stamp. Codecs: `cpu-zstd`, `passthrough`. Used by s4fs writes. |
| `bind_index(sidecar, source_compressed_size, source_etag=None) -> bytes` | Stamp the v2 version binding (backend ETag + size, post-PUT) into an `encode_s4_object` sidecar. |
| `pick_chunk_size(content_length: int) -> int` | The gateway's single-PUT chunk-size policy table (1 MiB / 4 MiB / 16 MiB). |

The `(original_size, crc32c)` tuple corresponds to the
`ChunkManifest.original_size` / `ChunkManifest.crc32c` fields the Rust
crate uses; round-trip them alongside the compressed payload (e.g. as
JSON sidecar fields).

## Build from source

```bash
pip install maturin
cd crates/s4-codec-py
maturin build --release
ls target/wheels/                          # *.whl is here
```

The `--features nvcomp-gpu` flag forwards to the underlying `s4-codec-rs`
crate's GPU codecs at the Rust level, but the Python module does NOT
expose Python classes for the GPU codecs in v1.0 (see the §Status note
above). Building with `--features nvcomp-gpu` therefore only affects
what `gpu_available()` reports, not which Python classes are importable.

`maturin develop` installs the wheel into the current virtualenv for
iterative development.

## Running tests

```sh
maturin develop
pip install -e ".[dev]"
pytest tests/
```

The `--features nvcomp-gpu` build flag forwards to the underlying
`s4-codec-rs` GPU paths at the Rust level. In v1.0 this only affects
what `gpu_available()` reports; the Python module does NOT add GPU
codec classes when built with this feature (see the §Status note at
the top of this file).

```sh
maturin develop --release --features nvcomp-gpu
```

The pytest suite covers CPU codec round-trips, RFC 1952 gzip compatibility,
GIL-release threading, version inheritance, and the per-`CodecError`
exception class hierarchy (v0.8.5 #85). A separate Rust-side test
(`tests/version_matches_workspace.rs`) guards the workspace semver inherit.

## Error handling

The binding raises a subclass tree per `CodecError` variant so callers can
branch programmatically instead of string-matching:

| Exception class | `CodecError` variant | Base class |
| --- | --- | --- |
| `S4Error` | (base + `TruncatedStream`) | `ValueError` |
| `S4CrcMismatchError` | `CrcMismatch` | `S4Error` |
| `S4SizeMismatchError` | `SizeMismatch` | `S4Error` |
| `S4CodecMismatchError` | `CodecMismatch` | `S4Error` |
| `S4UnregisteredCodecError` | `UnregisteredCodec` | `S4Error` |
| `S4ManifestSizeExceedsLimitError` | `ManifestSizeExceedsLimit` | `S4Error` |
| `S4ManifestSizeMismatchError` | `ManifestSizeMismatch` | `S4Error` |
| `S4BackendError` | `Backend` / `Join` | `RuntimeError` |
| `S4IoError` | `Io` | `OSError` |

`S4Error` inherits from `ValueError` for backward compat with code that
caught the previous flat `ValueError` mapping. `S4BackendError` and
`S4IoError` deliberately escape that hierarchy so existing retry-on-IOError
middleware continues to fire on the right class.

## Workspace integration

The crate ships a `cdylib` only and uses PyO3's `extension-module`
feature, so `cargo check -p s4-codec-py` and `cargo build --workspace`
succeed on a CI runner with no Python development headers installed —
no libpython link is performed; the Python interpreter that loads the
`.so` provides those symbols at runtime.

If you ever see a link error like
`undefined reference to PyExc_…`, drop `pyo3/extension-module` from the
features and you'll get the diagnostic build that does link libpython.

## Threading / GIL

Both `CpuZstd.compress()` and `CpuGzip.compress()` (and their `decompress()`
counterparts) **release the Python GIL** while running, so other Python threads
make progress concurrently. This is safe for:

- Django / Flask workers
- ASGI / asyncio event loops (use `asyncio.to_thread()` to wrap the blocking call)
- multi-threaded data pipelines

Example (asyncio):

```python
import asyncio
from s4_codec import CpuZstd

async def compress_async(data: bytes) -> bytes:
    codec = CpuZstd()
    compressed, orig_size, crc = await asyncio.to_thread(codec.compress, data)
    return compressed
```

Note: the methods themselves are **synchronous** — they don't return awaitables.
The GIL release means another Python thread can run during the compress; it
doesn't make the call async-aware.

## Supported codecs

| Codec | Default |
|---|---|
| `CpuZstd` | ✓ |
| `CpuGzip` | ✓ |

The GPU codecs (`nvcomp-zstd`, `nvcomp-bitcomp`, `nvcomp-gdeflate`) are intentionally **not** exposed as Python classes in v1.0 — they require a CUDA toolchain at build time and a GPU at runtime, which is a poor fit for the `pip install s4-codec` packaging story. The `nvcomp-gpu` feature on the underlying Rust crate exists for the server path; the Python module's runtime classes are the two CPU codecs above. `gpu_available() -> bool` is exposed for clients that want to gate their own logic on GPU presence (e.g. to decide whether to route a workload through the `s4` server gateway instead of the in-process Python decoder), but it does not enable any new Python class on its own. GPU codec exposure in Python is a v1.x roadmap candidate.

## Publishing status

- PyPI publish is **manual** (no CI automation as of v0.8.5):
  ```sh
  cd crates/s4-codec-py
  maturin build --release
  twine upload target/wheels/*
  ```
- Workspace version inheritance was fixed in v0.8.5 #82 — the published wheel
  version now matches the gateway version.

`target/wheels/` is gitignored — never commit `.whl` files.

## License

Apache-2.0 — same as the rest of the S4 project.
