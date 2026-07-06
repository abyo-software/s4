# Installing S4

### Install via cargo (Rust devs)

```bash
cargo install s4-server                                  # CPU build
s4 --endpoint-url https://s3.us-east-1.amazonaws.com     # binary is `s4`, not `s4-server`
cargo install s4-codec                                   # standalone offline decoder CLI
s4-codec decode stored-object.s4f2 -o original.bin       # decode without any gateway (docs/trust.md §1)
```

**Caveats** (v0.8.8, #98):
- Requires Rust 1.92+ (`rustup update stable` first).
- The default `cargo install` builds **CPU codecs only**. GPU codecs
  (`nvcomp-zstd` / `Bitcomp` / `GDeflate`) require `cargo install s4-server
  --features nvcomp-gpu`, which needs the CUDA toolchain and `NVCOMP_HOME`
  pointing at an extracted nvCOMP SDK at build time. Without these the build
  fails at link time with an `nvcomp` lib not found error.
- The installed binary is `s4` (not `s4-server`); check with `which s4`.

### Verifying the image / chart locally

The published image + chart pair is exercised in CI on every push that
touches the distribution surface
([.github/workflows/docker-smoke.yml](../.github/workflows/docker-smoke.yml) —
v0.10 wave-2 #B2): `helm lint` + `helm template` against `charts/s4`
with a placeholder backend URL (catches values-schema / template
regressions), `docker compose config` against both compose files
(catches reference / image-tag drift), and `docker pull` +
`s4 --help` / `s4 --version` against the latest published ghcr.io tag
(tolerates the not-yet-published case via `continue-on-error`).
Operators can reproduce the same checks locally before deploying:

```bash
# Helm chart sanity (with placeholder so backend.endpointUrl is satisfied)
helm lint ./charts/s4 --set backend.endpointUrl=https://s3.example.com
helm template s4 ./charts/s4 --set backend.endpointUrl=https://s3.example.com \
  | kubectl apply --dry-run=client -f -

# Compose file syntax + image-ref validation
docker compose -f docker-compose.yml config > /dev/null
docker compose -f docker-compose.gpu.yml config > /dev/null

# Image smoke (run this after a release lands on ghcr.io)
docker pull ghcr.io/abyo-software/s4:1.2.0
docker run --rm ghcr.io/abyo-software/s4:1.2.0 --help
docker run --rm ghcr.io/abyo-software/s4:1.2.0 --version
```

### Python (pip)

For ML / ETL pipelines that just want the codec without the gateway:

```python
from s4_codec import CpuZstd, CpuGzip, gpu_available
codec = CpuZstd(level=3)
compressed, original_size, crc = codec.compress(data_bytes)
roundtrip = codec.decompress(compressed, original_size, crc)
```

PyO3 bindings live in [`crates/s4-codec-py/`](../crates/s4-codec-py/) — build
with `maturin build --release` (and `--features nvcomp-gpu` for GPU).

### Browser (WASM)

For frontend apps that read S4-compressed objects directly from S3 over a
presigned URL, no S4 server in the read path:

```bash
rustup target add wasm32-unknown-unknown
wasm-pack build --release --target web crates/s4-codec-wasm  # → pkg/
```

The bundle exports `decompressFramed` / `decompressSingle` for the CPU
codec subset (`passthrough`, `cpu-zstd`, `cpu-gzip`). See
[`crates/s4-codec-wasm/README.md`](../crates/s4-codec-wasm/README.md) for
the API and a 10-line example.

### Python dataframes (s4fs / fsspec)

For pandas / pyarrow / DuckDB / Polars reading S4 objects **straight off the
backend** — no gateway in the read path. Range reads use the `.s4index`
sidecar to fetch only the overlapping frames; non-S4 objects pass through
byte-for-byte. Read-only by default; pass `write_enabled=True` to also
*write* gateway-compatible S4 objects directly to the backend (S4F2
framed `cpu-zstd` body + manifest metadata + ETag-bound sidecar — gateway
GET / Range GET and `s4 verify-sidecar` accept the result; append, SSE,
dictionaries and gateway versioning still go through the gateway; a
sidecar PUT that fails after the body landed raises a typed
`S4SidecarWriteError` — the object stays fully readable and
`s4 repair-sidecar` restores the Range fast-path). GPU
(`nvcomp-*`) frames and SSE-encrypted objects raise `NotImplementedError`
rather than decode wrong (SSE detection is triple-layered: `s4-encrypted`
metadata, sidecar SSE binding, and `S4E1`–`S4E6` magic-byte sniff).

```python
import pandas as pd
opts = {"target_options": {"endpoint_url": "http://backend:9000"}}
df = pd.read_parquet("s4://bucket/data.parquet", storage_options=opts)
df.to_parquet(  # write-back without the gateway (opt-in)
    "s4://bucket/data.parquet", storage_options={**opts, "write_enabled": True}
)
```

See [`python/s4fs/README.md`](../python/s4fs/README.md) for pyarrow / DuckDB
examples, the supported-codec matrix and the write constraints.

### Build from source

```bash
cargo build --release --workspace                       # CPU-only
NVCOMP_HOME=/path/to/nvcomp cargo build --release --workspace --features s4-server/nvcomp-gpu

target/release/s4 --endpoint-url https://s3.us-east-1.amazonaws.com \
    --host 0.0.0.0 --port 8014 --codec cpu-zstd --log-format json
```

### Supported targets

| Crate                          | 64-bit Linux (`x86_64` / `aarch64`) | 32-bit Linux (`i686`) | Browser (`wasm32-unknown-unknown`) |
|--------------------------------|:-----------------------------------:|:---------------------:|:----------------------------------:|
| `s4-codec` (library)           | ✅ tier 1                           | ✅ compiles + tests   | ✅ via `s4-codec-wasm`             |
| `s4-codec-wasm` (browser)      | n/a                                 | n/a                   | ✅ tier 1                          |
| `s4-config`                    | ✅ tier 1                           | ✅                    | ✅                                 |
| `s4-server` (gateway binary)   | ✅ tier 1                           | ✅ compiles + `--help` / `--version` + advisory PUT/GET round-trip (CI) | ❌ not applicable           |
| `nvcomp-gpu` feature (any crate above) | ✅ x86_64 only (NVIDIA driver) | ❌ (no 32-bit nvCOMP) | ❌                            |

Runtime-tested platform is **`x86_64-unknown-linux-gnu`** and
**`aarch64-unknown-linux-gnu`** (CI matrix). The 32-bit `i686-unknown-linux-gnu`
target builds clean for `s4-codec` / `s4-config` / `s4-server` as of
v0.9 #106 (default-bytes constants are now `target_pointer_width` cfg-gated
so the 5 GiB AWS S3 single-PUT ceiling no longer const-overflows `usize` on
32-bit). v0.10 wave-2 #A4 adds a per-push CI job that (a) executes the
`s4-codec` + `s4-config` test suites under `--target i686-unknown-linux-gnu`
and (b) builds the `s4` binary itself for i686 + invokes
`s4 --help` / `s4 --version` as a runtime smoke. v0.11 #A4 extends the
same job with an **end-to-end PUT/GET round-trip** — the i686 `s4` binary
runs in front of a stock MinIO container and the AWS CLI puts then gets
a small object back through it, byte-equality-checked. The round-trip
step lands in CI as **advisory (`continue-on-error: true`)** so a
first-time 32-bit runtime bug surfaces in the job log without turning
the badge red while a fix lands in a follow-up v0.11.x commit; promotion
to a required gate happens once a stretch of green main pushes is
observed. Operators running on i686 should still treat
`--max-body-bytes` carefully (auto-clamps to `isize::MAX as usize`
≈ 2 GiB on 32-bit — Rust caps any single `Vec` / `Bytes` allocation
at `isize::MAX`, so a higher gateway guard would let oversized requests
panic inside the SSE buffered-decrypt pre-alloc path).

The `wasm32-unknown-unknown` target is the public release channel for the
browser decoder (`s4-codec-wasm`); the criterion regression-tracking suite
and `cargo check --target wasm32-unknown-unknown` keep it green on every CI
push to `main`.
