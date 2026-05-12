# S4 — Squished S3

[![CI](https://github.com/abyo-software/s4/actions/workflows/ci.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/ci.yml)
[![Nightly Fuzz](https://github.com/abyo-software/s4/actions/workflows/fuzz-nightly.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/fuzz-nightly.yml)
[![AWS E2E](https://github.com/abyo-software/s4/actions/workflows/aws-e2e.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/aws-e2e.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org)

> **Drop-in S3-compatible storage gateway with GPU-accelerated transparent compression.**
> Cuts your AWS S3 bill 50–80% without changing a single line of application code.

[日本語版 README → `README.ja.md`](README.ja.md)

---

## What is S4?

S4 (**Squished S3**) is an S3-compatible storage gateway written in Rust that
sits between your applications (boto3 / aws-sdk / aws-cli / Spark / Trino /
DuckDB / anything S3) and your real S3 bucket — and **transparently compresses
every object with GPU codecs** (NVIDIA nvCOMP zstd / Bitcomp / gANS) or CPU
zstd before storing it.

```
                        endpoint: s4.example.com
   your application ──────────────────────────▶  S4 (this project)
   (boto3, Spark,                                       │
    Trino, ...)                                         ▼
                                            (compress with GPU)
                                                        │
                                                        ▼
                                                 AWS S3 (real bucket)
```

- **No app changes**: same S3 wire protocol, same SigV4 auth, same SDK calls
- **Transparent**: PUT compresses, GET decompresses; clients see the original bytes
- **No lock-in**: stop the gateway, read your bucket directly with aws-cli

## Why S4?

| Problem | Solution |
|---|---|
| Your S3 bill grows linearly with data, but most data is ≥3× compressible | S4 compresses on the way in, charging you only for the squished bytes |
| Your apps don't compress data themselves (and you don't want to change them) | S4 is a wire-compatible drop-in — just change `--endpoint-url` |
| Existing object-storage compressors (MinIO S2, Garage zstd) are CPU-only | S4 supports nvCOMP **GPU** codecs — Bitcomp gives 3.6–7.5× on integer columns |
| Analytics workloads need byte-range reads | S4 supports `Range` GET via sidecar frame index (parquet/ORC reader compatible) |

## Quick Start

### 60-second local trial (Docker, CPU-only)

```bash
git clone https://github.com/abyo-software/s4 && cd s4
docker compose up -d                    # MinIO + S4 server on localhost:8014

# Use any S3 client. Below uses aws-cli; replace endpoint with anything.
aws --endpoint-url http://localhost:8014 s3 mb s3://demo
aws --endpoint-url http://localhost:8014 s3 cp big.log s3://demo/big.log
aws --endpoint-url http://localhost:8014 s3 cp s3://demo/big.log -

# Inspect the compressed object directly on MinIO (different endpoint):
aws --endpoint-url http://localhost:9000 s3 cp s3://demo/big.log -.compressed
ls -la big.log -.compressed             # the .compressed file is much smaller
```

### Try with GPU compression (NVIDIA nvCOMP)

```bash
# Requires NVIDIA Container Toolkit + a CUDA-capable GPU
docker compose -f docker-compose.gpu.yml up -d
aws --endpoint-url http://localhost:8014 s3 cp parquet-file.parq s3://demo/
```

See [docker-compose.gpu.yml](docker-compose.gpu.yml) for details.

### Build from source

```bash
cargo build --release --workspace                       # CPU-only
NVCOMP_HOME=/path/to/nvcomp cargo build --release --workspace --features s4-server/nvcomp-gpu

target/release/s4 --endpoint-url https://s3.us-east-1.amazonaws.com \
    --host 0.0.0.0 --port 8014 --codec cpu-zstd --log-format json
```

## How it Compares

| Feature | S4 | MinIO (built-in S2) | Garage | Wasabi / B2 | AWS S3 |
|---|---|---|---|---|---|
| S3 API compatibility | ✅ Full | ✅ Full | ⚠️ Subset | ✅ Full | ✅ Native |
| **GPU compression** | ✅ nvCOMP zstd / Bitcomp / gANS | ❌ | ❌ | ❌ | ❌ |
| **CPU compression** | ✅ zstd 1–22 | ⚠️ S2 only | ✅ zstd 1–22 | ❌ | ❌ |
| **Auto codec selection** | ✅ entropy + magic-byte sampling | ❌ | ❌ | — | — |
| **Range GET on compressed** | ✅ via sidecar frame index | n/a | n/a | ✅ | ✅ |
| **Streaming I/O** | ✅ TTFB ms-class, ~10 MiB peak | ✅ | ✅ | ✅ | ✅ |
| **Acts as gateway to existing S3** | ✅ | ❌ (gateway mode removed) | ❌ | ❌ | n/a |
| **License** | Apache-2.0 | AGPLv3 / commercial | AGPLv3 | proprietary | proprietary |

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                          S4 server                               │
│  ┌──────────────────┐  ┌─────────────────┐  ┌────────────────┐   │
│  │ s3s framework    │→ │ S4Service       │→ │ s3s_aws::Proxy │ → │ → backend (AWS S3 / MinIO)
│  │ (HTTP + SigV4)   │  │ (compress hook) │  │ (aws-sdk-s3)   │   │
│  └──────────────────┘  └────────┬────────┘  └────────────────┘   │
│                                 ▼                                │
│  ┌─────────────────────────────────────────────────────────┐     │
│  │ s4-codec::CodecRegistry  (multi-codec dispatch by id)   │     │
│  │   ├─ Passthrough          (no compression)              │     │
│  │   ├─ CpuZstd              (zstd-rs, streaming)          │     │
│  │   ├─ NvcompZstd           (nvCOMP, GPU, batch)          │     │
│  │   └─ NvcompBitcomp        (nvCOMP, integer columns)     │     │
│  └─────────────────────────────────────────────────────────┘     │
│  ┌─────────────────────────────────────────────────────────┐     │
│  │ s4-codec::CodecDispatcher                               │     │
│  │   ├─ AlwaysDispatcher                                   │     │
│  │   └─ SamplingDispatcher  (entropy + 14 magic bytes)     │     │
│  └─────────────────────────────────────────────────────────┘     │
└──────────────────────────────────────────────────────────────────┘
        ▲              ▲              ▲                ▲
        │              │              │                │
   /health         /ready         /metrics         OTLP traces
   (probe)        (probe)       (Prometheus)       (Jaeger / X-Ray)
```

## Production Features

### Streaming I/O
- **Streaming GET** for non-multipart `cpu-zstd` / `passthrough` objects:
  TTFB ms-class, memory ≈ zstd window + 64 KiB buffer
- **Streaming PUT** for the same codecs: input never fully buffered, peak memory
  ≈ compressed size (5 GB → ~50 MB at 100× ratio)
- **Multipart per-part compression**: each part compressed and frame-encoded
  (`S4F2` magic), per-frame codec dispatch (mixed codecs in one object)
- **Range GET via sidecar `<key>.s4index`**: only the needed compressed bytes
  are fetched from backend, decoded, and sliced. Falls back to full read when
  sidecar is absent.

### Observability
- **`/health`** — liveness probe, always 200 OK
- **`/ready`** — readiness probe, runs `ListBuckets` against the backend
- **`/metrics`** — Prometheus text format
  (`s4_requests_total{op,codec,result}`, `s4_bytes_in_total`, `s4_bytes_out_total`,
  `s4_request_latency_seconds`)
- **Structured JSON logs** (`--log-format json`) with per-request fields:
  `op`, `bucket`, `key`, `codec`, `bytes_in`, `bytes_out`, `ratio`, `latency_ms`, `ok`
- **OpenTelemetry traces** (`--otlp-endpoint http://collector:4317`) — each
  PUT/GET emitted as `s4.put_object` / `s4.get_object` span with semantic
  attributes; export to Jaeger / Tempo / Grafana / AWS X-Ray.

### Data Integrity
- **CRC32C** stored per-object (single PUT) or per-frame (multipart), verified on GET
- **`copy_object` S4-aware**: source's `s4-*` metadata is preserved across
  `MetadataDirective: REPLACE` (prevents silent corruption of the destination)
- **Zstd decompression bomb hardening**: `Decoder + take(manifest.original_size + margin)`
  caps memory regardless of an attacker-controlled manifest claim

### S3 API coverage (45+ ops)
- Compression hook: `put_object`, `get_object`, `upload_part`
- Range GET: full S3 spec (`bytes=N-M`, `bytes=-N`, `bytes=N-`)
- Multipart: `create_multipart_upload`, `upload_part`, `complete_multipart_upload`, `abort_multipart_upload`, `list_parts`, `list_multipart_uploads`
- Phase 2 delegations (passthrough): ACL, Tagging, Lifecycle, Versioning, Replication, CORS, Encryption, Logging, Notification, Website, Object Lock, Public Access Block, ...
- Hidden: `*.s4index` sidecars are filtered from `list_objects[_v2]` responses

## Testing & Validation

| Tier | What runs | Where | Pass count |
|---|---|---|---|
| **Unit + integration** | parsers, registry, blob helpers, S3 trait | every push (CI) | 51 |
| **proptest fuzz** | 38 properties × 256–10K cases (push), × 1M (nightly) | every push + nightly | 38 |
| **bolero coverage-guided** | 7 targets, libfuzzer engine | nightly (matrix, 30 min × 5) | 7 |
| **fuzz canary** | proves fuzz framework is alive | every push | 3 |
| **Docker MinIO E2E** | full HTTP wire + SigV4 against real MinIO | nightly | 10 |
| **GPU codec E2E** | real CUDA, nvCOMP zstd/Bitcomp roundtrip | manual (`--features nvcomp-gpu`) | 4 |
| **Soak / load** | 24h sustained load, RSS / FD / connection leak detection | manual (`scripts/soak/run.sh`) | continuous |

**99 default tests + 10 ignored E2E + 4 GPU + canary = 116+ tests**, plus
PROPTEST_CASES=10000 stress run on every push (~73 sec, 380K fuzz cases),
1M cases × 38 properties nightly (~6 h, 38M+ fuzz cases).

Two real bugs already caught by fuzz infrastructure:
1. `FrameIter` infinite-loop on 1-byte input (DoS) — fixed with `fused: bool`
2. `cpu_zstd::decompress` could OOM on attacker-controlled manifest claim —
   fixed with `Decoder + take(limit)`

```bash
cargo test --workspace                   # default
cargo test --workspace -- --ignored --test-threads=1   # E2E (Docker required)
PROPTEST_CASES=100000 cargo test --workspace --release --test fuzz_parsers --test fuzz_server --test fuzz_advanced
NVCOMP_HOME=... cargo test --workspace --features s4-server/nvcomp-gpu -- --ignored
./scripts/soak/run.sh                    # 24 h soak (Marketplace pre-release)
```

## Configuration

| CLI flag | Default | Description |
|---|---|---|
| `--endpoint-url` | (required) | Backend S3 endpoint (e.g. `https://s3.us-east-1.amazonaws.com`) |
| `--host` | `127.0.0.1` | Bind host |
| `--port` | `8014` | Bind port |
| `--domain` | (none) | Virtual-hosted-style requests domain |
| `--codec` | `cpu-zstd` | Default codec: `passthrough`, `cpu-zstd`, `nvcomp-zstd`, `nvcomp-bitcomp` |
| `--zstd-level` | `3` | CPU zstd compression level (1–22) |
| `--dispatcher` | `sampling` | `always` (use `--codec`) or `sampling` (entropy + magic byte) |
| `--log-format` | `pretty` | `pretty` (terminal) or `json` (CloudWatch / fluent-bit) |
| `--otlp-endpoint` | (none) | OpenTelemetry OTLP gRPC endpoint |
| `--service-name` | `s4` | OTel resource `service.name` |
| `--tls-cert` | (none) | TLS server certificate (PEM). Together with `--tls-key`, terminates HTTPS on the listener |
| `--tls-key` | (none) | TLS server private key (PEM, PKCS#8 or RSA) |
| `--policy` | (none) | AWS-style bucket policy JSON. When set, every PUT/GET/DELETE/List request is evaluated before backend dispatch |

AWS credentials are read from the standard AWS chain (`AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY` / `AWS_PROFILE` / IAM role on EC2).

### HTTPS

S4 can terminate TLS itself — no fronting reverse proxy required:

```bash
s4 --endpoint-url https://s3.us-east-1.amazonaws.com \
   --host 0.0.0.0 --port 8443 \
   --tls-cert /etc/ssl/s4.crt --tls-key /etc/ssl/s4.key
aws --endpoint-url https://localhost:8443 s3 ls
```

Backed by `tokio-rustls` + `ring`. ALPN advertises `h2` then `http/1.1`, so
HTTP/2 is negotiated automatically with capable clients. Without these
flags, S4 serves plain HTTP (the default).

### Bucket policy enforcement

Pass an AWS-style bucket policy JSON to `--policy` to gate requests at the
gateway:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {"Sid": "ReadOnly",  "Effect": "Allow", "Action": ["s3:GetObject", "s3:ListBucket"],
     "Resource": ["arn:aws:s3:::my-bucket", "arn:aws:s3:::my-bucket/*"]},
    {"Sid": "DenyDelete", "Effect": "Deny",  "Action": "s3:DeleteObject",
     "Resource": "arn:aws:s3:::my-bucket/*"}
  ]
}
```

Supported subset (v0.2): `Effect`, `Action`, `Resource`, `Principal`
(SigV4 access-key match). Decision order is the standard AWS one:
**explicit Deny > explicit Allow > implicit Deny**. Denials are exposed as
the `s4_policy_denials_total{action,bucket}` Prometheus counter.

For more advanced needs (full IAM Conditions, STS / AssumeRole), front
S4 with an IAM-aware proxy and use this flag for in-gateway last-mile
checks.

## On-the-wire Format

S4 stores data as either:

### Single PUT (non-framed, used for one-shot `put_object`)
S3 metadata holds the manifest:

```
x-amz-meta-s4-codec:           passthrough | cpu-zstd | nvcomp-zstd | ...
x-amz-meta-s4-original-size:   <decoded bytes>
x-amz-meta-s4-compressed-size: <stored bytes>
x-amz-meta-s4-crc32c:          <CRC32C of original bytes>
```

Object body is the raw compressed bytes.

### Multipart (framed, `S4F2` magic, per-part compression)

```
x-amz-meta-s4-multipart: true
x-amz-meta-s4-codec:     <default codec for the object>
```

Object body is a sequence of:

```
┌──────────── 28-byte frame header ────────────┐
│ "S4F2" │ codec_id u32 │ orig u64 │ comp u64  │ crc32c u32 │  payload (comp bytes)
└────────────────────────────────────────────────┘

(optional) ┌──── padding ────┐
           │ "S4P1" │ len u64 │ <len zero bytes>
           └─────────────────┘
```

A sidecar object `<key>.s4index` (binary, `S4IX` magic) maps decompressed
byte ranges to compressed byte offsets — used by Range GET to fetch only the
needed bytes from S3.

## Project Status

- **Phase 1 + 2.0 + 2.1 complete** (24 commits, 116+ tests, fuzz / soak / OTel /
  Prometheus all wired)
- **Production-ready** for log archival, data lake, parquet/ORC analytics
- **Known limitations / Phase 2.2 plans**:
  - GPU streaming compress (currently bytes-buffered, batch-API): per-chunk
    pipeline + framed-everywhere unification
  - Multipart final-part padding trim (typical workloads not affected; up to
    5 MiB overhead per object on highly-compressible last parts)
  - `upload_part_copy` byte-range awareness (currently passes through)
  - Single-PUT sidecar (currently multipart-only)

## Contributing

Pull requests are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for the
development setup, coding conventions, and the test/fuzz/soak protocol.

By contributing, you agree your contributions will be licensed under
Apache-2.0 (no separate CLA required).

## Security

Found a vulnerability? Please **do not open a public issue**. Instead, follow
[SECURITY.md](SECURITY.md) for coordinated disclosure.

## License

Licensed under the **Apache License, Version 2.0** ([LICENSE](LICENSE)).
See [NOTICE](NOTICE) for third-party attributions including the vendored
`ferro-compress` (Apache-2.0 OR MIT) and the optional NVIDIA nvCOMP SDK
(proprietary, BYO).

`"S4"` and `"Squished S3"` are unregistered trademarks of abyo software 合同会社.
`"Amazon S3"` and `"AWS"` are trademarks of Amazon.com, Inc. S4 is not
affiliated with, endorsed by, or sponsored by Amazon.

## Authors

- abyo software 合同会社 — sponsoring organization, commercial AMI distribution
- masumi-ryugo — original author / maintainer

---

**Looking for the Japanese-language version?** → [README.ja.md](README.ja.md)
