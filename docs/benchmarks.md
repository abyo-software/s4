# Benchmarks

Single-pass roundtrip through `s4-codec`. Hardware: RTX 4070 Ti SUPER 16 GB
+ nvCOMP 5.2.0.10 + CUDA 13.2 driver 595.58.03 + Ryzen 9 9950X. Throughput
is reported as **uncompressed bytes per second** (the convention nvCOMP /
lz4 / zstd publish). Last benchmarked 2026-05-13 (v0.8 #53,
`crates/s4-codec/examples/bench_codecs.rs`).

![v0.8 perf chart](perf-v0.8.png)

| Workload | Codec | Original | Compressed | Ratio | Compress | Decompress |
|---|---|---:|---:|---:|---:|---:|
| nginx access log (256 MiB) | cpu-zstd-3 | 256 MiB | 1 MiB | **155.01×** | 3.71 GB/s | 3.27 GB/s |
| nginx access log (256 MiB) | nvcomp-zstd | 256 MiB | 2 MiB | 95.60× | 1.70 GB/s | 2.86 GB/s |
| nginx access log (256 MiB) | nvcomp-gdeflate | 256 MiB | 169 MiB | 1.51× | 1.07 GB/s | 2.51 GB/s |
| Parquet-like mixed (256 MiB) | cpu-zstd-3 | 256 MiB | 133 MiB | 1.92× | 0.75 GB/s | 1.89 GB/s |
| Parquet-like mixed (256 MiB) | nvcomp-zstd | 256 MiB | 131 MiB | 1.94× | 1.44 GB/s | 2.62 GB/s |
| Parquet-like mixed (256 MiB) | nvcomp-gdeflate | 256 MiB | 183 MiB | 1.40× | 1.05 GB/s | 2.62 GB/s |
| Parquet-like mixed (256 MiB) | nvcomp-bitcomp | 256 MiB | 122 MiB | **2.09×** | 1.49 GB/s | 1.44 GB/s |
| Postings (u32, 64 MiB) | cpu-zstd-3 | 64 MiB | 43 MiB | 1.48× | 1.22 GB/s | 1.65 GB/s |
| Postings (u32, 64 MiB) | nvcomp-zstd | 64 MiB | 42 MiB | 1.52× | 1.29 GB/s | 2.52 GB/s |
| Postings (u32, 64 MiB) | nvcomp-gdeflate | 64 MiB | 42 MiB | 1.51× | 1.06 GB/s | 2.44 GB/s |
| Postings (u32, 64 MiB) | nvcomp-bitcomp | 64 MiB | 5 MiB | **11.93×** | 1.61 GB/s | 1.50 GB/s |
| Timestamps (i64, 64 MiB) | cpu-zstd-3 | 64 MiB | 24 MiB | 2.63× | 0.35 GB/s | 0.92 GB/s |
| Timestamps (i64, 64 MiB) | nvcomp-zstd | 64 MiB | 24 MiB | 2.61× | 1.14 GB/s | 2.70 GB/s |
| Timestamps (i64, 64 MiB) | nvcomp-gdeflate | 64 MiB | 48 MiB | 1.32× | 0.89 GB/s | 2.26 GB/s |
| Timestamps (i64, 64 MiB) | nvcomp-bitcomp | 64 MiB | 21 MiB | **2.95×** | 1.45 GB/s | 1.39 GB/s |
| doc_values (i64, 64 MiB) | cpu-zstd-3 | 64 MiB | 44 MiB | 1.45× | 0.26 GB/s | 1.01 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-zstd | 64 MiB | 34 MiB | **1.86×** | 1.04 GB/s | 2.59 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-gdeflate | 64 MiB | 48 MiB | 1.33× | 0.96 GB/s | 2.54 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-bitcomp | 64 MiB | 37 MiB | 1.72× | 1.41 GB/s | 1.48 GB/s |
| Already-compressed (64 MiB) | cpu-zstd-3 | 64 MiB | 64 MiB | 1.00× | 2.23 GB/s | 3.15 GB/s |
| Already-compressed (64 MiB) | nvcomp-zstd | 64 MiB | 64 MiB | 1.00× | 0.83 GB/s | 2.37 GB/s |
| Already-compressed (64 MiB) | nvcomp-gdeflate | 64 MiB | 64 MiB | 1.00× | 0.92 GB/s | 2.39 GB/s |

**v0.3 → v0.8 throughput delta** (compress GB/s on the same hardware,
nvCOMP 5.0.x → 5.2.0.10, no source-code changes — pure runtime / driver gains):

| Workload | Codec | v0.3 (2026-04) | v0.8 (2026-05-13) | Delta |
|---|---|---:|---:|---:|
| nginx (256 MiB) | cpu-zstd-3 | 2.72 GB/s | **3.71 GB/s** | +36% |
| nginx (256 MiB) | nvcomp-zstd | 1.27 GB/s | **1.70 GB/s** | +34% |
| parquet (256 MiB) | nvcomp-zstd | 1.06 GB/s | **1.44 GB/s** | +36% |
| parquet (256 MiB) | nvcomp-bitcomp | 1.20 GB/s | **1.49 GB/s** | +24% |
| timestamps (64 MiB) | nvcomp-zstd | 0.95 GB/s | **1.14 GB/s** | +20% |
| timestamps (64 MiB) | nvcomp-bitcomp | 1.20 GB/s | **1.45 GB/s** | +21% |
| doc_values (64 MiB) | nvcomp-zstd | 0.80 GB/s | **1.04 GB/s** | +30% |

**Reading the table:**

- **`cpu-zstd-3`** dominates on text — 155× on nginx logs is hard to beat.
- **`nvcomp-bitcomp`** is the killer for typed numeric columns: 11.93× on
  sorted u32 posting lists (vs ~1.5× for everything else), 2.95× on
  monotonic i64 timestamps. The `data_type` hint is critical (`Char` on
  numeric data degrades to ~1.2×); see `s4_codec::nvcomp::BitcompDataType`
  for the typed constructors.
- **`nvcomp-zstd`** is competitive on Parquet-like / mixed workloads and
  frees the CPU for serving requests in parallel.
- **`nvcomp-gdeflate`** sits between zstd and "no compression" — useful
  when you need DEFLATE-format wire compat (in v0.3 the
  [`gunzip`-compatible wrapper](https://github.com/abyo-software/s4/issues/26)
  will make this codec serve `Content-Encoding: gzip` to any HTTP client).
- **Already-compressed inputs** are correctly bypassed at ratio 1.0× by every
  codec — S4 never makes a file *bigger*.

**Throughput note**: nvCOMP runs through the FCG1-framed batched API at
the default 64 KiB chunk size, so per-call overhead dominates the 64 MiB
input cases. Production deployments using larger chunks via
`streaming_compress_to_frames` (v0.2 #1) push GPU compress >5 GB/s on
highly compressible inputs. The full head-to-head bench vs MinIO S2 /
Garage zstd is tracked in
[issue #14](https://github.com/abyo-software/s4/issues/14); the latest CSV
captured on 2026-05-13 lives at
[`benches/comparison/result-2026-05-13.csv`](../benches/comparison/result-2026-05-13.csv)
(MinIO + s4-cpu only; Garage's auto-issued keys and the s4-gpu image
require manual setup outside the driver script).

**Multipart streaming note** (v0.2 #1, surfaced again by the v0.8 #53
comparison run): per-part S4F2 framing (4 MiB chunks) means a 64 MiB
nginx-log multipart upload reports ~1.6× ratio at the storage layer
instead of the 155× single-pass ratio above — each chunk is too small
for zstd's longest-match window to amortize across the whole object.
Ratio scales back to single-pass numbers once `cargo install` users
configure larger multipart chunk sizes via the AWS SDK
`multipart_chunksize` knob (S4 itself stays at the 4 MiB default for
Range-GET granularity). The CSV captures end-to-end PUT/GET wall-clock
including framing overhead.

Separately from the ratio effect, every non-final compressible part is
also **padded to the S3 5 MiB minimum part size** — with the aws-cli
default 8 MiB `multipart_chunksize`, stored bytes are ≥62.5% of the
original *regardless of compressibility* until an `s4 recompact`
rewrite. Arithmetic, the `multipart_chunksize` mitigation, and the
reclaim path are in
[docs/savings.md](savings.md#multipart-uploads-the-5-mib-part-floor-caps-at-rest-savings).

### Performance regression tracking (criterion + GitHub Pages)

The single-pass numbers above are captured manually on the maintainer's
workstation; for **per-commit regression detection** S4 also runs a
criterion bench suite on every push to `main`
([`.github/workflows/bench.yml`](../.github/workflows/bench.yml)), stores
the timing history in the `gh-pages` branch via
[`benchmark-action/github-action-benchmark`](https://github.com/benchmark-action/github-action-benchmark),
and comments on a commit when any tracked target gets ≥ 1.1× slower
than its previous best. The targets cover the CPU hot paths every
default-build deployment runs through:

- `crates/s4-codec/benches/codec_roundtrip.rs` — `cpu-zstd` (levels
  1 / 3 / 22) / `cpu-gzip` / `passthrough` compress + decompress at
  1 KiB / 1 MiB / 16 MiB.
- `crates/s4-codec/benches/frame_codec.rs` — `write_frame` and the
  `FrameIter` walker, with the padding-skip branch exercised.
- `crates/s4-codec/benches/index_codec.rs` — S4IX sidecar
  `encode_index` / `decode_index` / `lookup_range` across 128 /
  1024 / 4096 frame counts.

GPU codecs (`nvcomp-*`) are intentionally not in the regression suite
because GitHub-hosted runners have no CUDA-capable GPU; the manual
table above remains the canonical source for those numbers.

The rendered trend chart lives at
`https://abyo-software.github.io/s4/dev/bench/` after the first
successful CI run on `main` initialises the `gh-pages` branch.

### SSE throughput (AES-NI vs software fallback)

S4's server-side encryption (`--sse-s4-key`) goes through the `aes-gcm`
crate, which selects the AES-NI hardware path automatically on x86_64
hosts where the `aes` + `pclmulqdq` CPU features are present. v0.8 #50
adds (a) a boot log line confirming which backend is live, (b) a
`s4_sse_aes_backend{kind="aes-ni"|"neon"|"software"}` Prometheus gauge
stamped at startup, and (c) the `bench_sse_throughput` example below
that measures the resulting encrypt / decrypt throughput.

Numbers below are from the same Ryzen 9 9950X host as the codec table.
Reproduce with `cargo run --release -p s4-server --example
bench_sse_throughput` (AES-NI is the default; force the software
backend with `RUSTFLAGS="--cfg aes_force_soft --cfg
polyval_force_soft"` and a clean target dir).

| Body size | AES-NI Encrypt | AES-NI Decrypt | Software Encrypt | Software Decrypt |
|-----------|---------------:|---------------:|-----------------:|-----------------:|
| 64 KiB    | 1661 MB/s      | 1692 MB/s      | 194 MB/s         | 194 MB/s         |
| 1 MiB     | 1709 MB/s      | 1718 MB/s      | 195 MB/s         | 195 MB/s         |
| 100 MiB   | 956 MB/s       | 925 MB/s       | 181 MB/s         | 180 MB/s         |

AES-NI delivers ~8.7× throughput on 64 KiB / 1 MiB bodies (the regime
that dominates real S3 object traffic). The 100 MiB row's narrower
gap (~5.2×) is the buffer allocator + page-fault floor — `aes-gcm`
uses a single contiguous `Vec` for the ciphertext, so 100 MiB cases
charge a `mmap` per iteration that's not on the AES path. Operators
running on hosts without AES-NI (very old / virtualized x86 or
non-x86 hardware) should expect ~190 MB/s encrypt / decrypt as the
sustained ceiling for SSE-S4 — still ahead of the network for most
deployments, but worth knowing when sizing CPU headroom.

**Detecting which backend is live**: the boot log emits
`S4 AES-NI feature detection ... aes_ni_available=true` (or `false`),
and `curl -s localhost:9100/metrics | grep s4_sse_aes_backend` shows
the gauge with the active `kind` label.

**Reproducing locally** (requires CUDA + nvCOMP):

```bash
NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
  cargo run --release --example bench_codecs \
    -p s4-codec --features nvcomp-gpu

# Streaming pipeline bench (1 GiB highly-compressible, in-flight chunks):
NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
  cargo run --release --example bench_pipeline \
    -p s4-server --features nvcomp-gpu

# Comparison vs MinIO / Garage (Docker required):
docker compose -f benches/comparison/docker-compose.yml up -d
AWS_REQUEST_CHECKSUM_CALCULATION=when_required \
AWS_RESPONSE_CHECKSUM_VALIDATION=when_required \
  ./benches/comparison/run.sh benches/comparison/result-$(date +%F).csv
```
