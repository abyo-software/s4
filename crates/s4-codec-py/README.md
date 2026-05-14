# s4-codec (Python bindings)

In-process GPU/CPU compression from Python — no S4 gateway required.
Wraps the same Rust [`s4-codec`](../s4-codec) crate that powers the S4
S3-compatible storage gateway, so a Python notebook / Airflow task / Spark
UDF can compress and decompress with the exact same byte format as
objects sitting in an S4 bucket.

## Install

```bash
pip install s4-codec        # CPU codecs only (zstd + gzip), no CUDA needed
```

For GPU (nvCOMP) codecs you currently have to build from source, because
the wheel needs to be linked against your CUDA toolchain. See
**Build from source** below.

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

The `(original_size, crc32c)` tuple corresponds to the
`ChunkManifest.original_size` / `ChunkManifest.crc32c` fields the Rust
crate uses; round-trip them alongside the compressed payload (e.g. as
JSON sidecar fields).

## Build from source

```bash
# CPU-only wheel
pip install maturin
cd crates/s4-codec-py
maturin build --release
ls target/wheels/                          # *.whl is here

# GPU wheel — requires NVCOMP_HOME pointed at an extracted nvCOMP redist
# tarball, plus a CUDA toolchain (nvcc) on the build host.
export NVCOMP_HOME=/path/to/nvcomp-linux-x86_64-5.x.x.x_cuda12-archive
maturin build --release --features nvcomp-gpu
```

`maturin develop` installs the wheel into the current virtualenv for
iterative development.

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

| Codec | Default | Requires `--features nvcomp-gpu` |
|---|---|---|
| `CpuZstd` | ✓ | — |
| `CpuGzip` | ✓ | — |
| `NvcompZstd` | — | ✓ + CUDA 12.x at runtime |
| `NvcompBitcomp` | — | ✓ + CUDA 12.x at runtime |
| `NvcompGDeflate` | — | ✓ + CUDA 12.x at runtime |

Use `gpu_available() -> bool` at runtime to confirm a CUDA-capable GPU is present
before constructing a GPU codec — building the wheel with `--features nvcomp-gpu`
on a host with no GPU still produces a wheel that loads but raises at codec
construction time.

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
