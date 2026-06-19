# S4 — Squished S3

[![CI](https://github.com/abyo-software/s4/actions/workflows/ci.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/ci.yml)
[![Nightly Fuzz](https://github.com/abyo-software/s4/actions/workflows/fuzz-nightly.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/fuzz-nightly.yml)
[![AWS E2E](https://github.com/abyo-software/s4/actions/workflows/aws-e2e.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/aws-e2e.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org)

## Cut your S3 storage bill 50–80% — drop-in, zero app-code changes

**S4 (Squished S3)** is an S3-compatible gateway that sits in front of your
existing S3 bucket and transparently compresses every object on the way in,
then decompresses it on the way out. Point boto3, aws-cli, Spark, Trino,
DuckDB — anything that speaks the S3 API — at S4, and your apps keep working
unchanged while your backend stores **50–80% fewer bytes**. The main thing that
changes is the endpoint URL.

```
   your app  ──▶  S4 (compress)  ──▶  AWS S3 (real bucket, fewer bytes)
 (boto3, Spark,        ▲
  Trino, …)            └── GET decompresses; clients see the original bytes
```

- **No app changes** — same S3 wire protocol, SigV4 auth, and SDK calls; just change `--endpoint-url`. (GET returns the original bytes; `HEAD` reports the stored compressed size, with the original in `x-amz-meta-s4-original-size`.)
- **Per-object smart codec** — CPU zstd for text/logs, GPU nvCOMP (Bitcomp/zstd/GDeflate) for integer/columnar data, passthrough for already-compressed inputs. You almost never need a GPU.
- **No lock-in** — stop the gateway and the compressed objects + S4IX sidecars stay S3-native, decodable by the Apache-2.0 `s4-codec` CLI / `pip` / WASM. ([format](docs/wire-format.md))
- **Range GET for framed objects** — sidecar-indexed byte ranges serve Parquet/ORC readers; some SSE / multipart-SSE modes use a buffered fallback.

> ☁️ **Run it on AWS Marketplace — AWS-billed hourly, no app changes:**
> **▶ [Container on EKS / ECS / Fargate](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e)** — runs on any small CPU node; the default path for most workloads.
> &nbsp;·&nbsp; **[GPU AMI on EC2 (g4dn / g5 / g6 / g6e)](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i)** — for integer/columnar data at high throughput.
>
> The open-source build is free for local testing; Marketplace adds AWS procurement, billing, and the supported commercial path.

## Why teams use S4

| Need | What S4 gives you |
|---|---|
| S3 bill grows linearly, but most data is ≥3× compressible | Compresses on the way in — you pay only for the squished bytes |
| Apps don't compress data themselves (and you can't change them) | Wire-compatible drop-in — only the endpoint URL changes |
| Mixed data (text, JSON, Parquet/ORC, numeric columns) | Dispatcher samples each object and picks the best codec automatically |
| Analytics need byte-range reads | `Range` GET via the S4IX sidecar frame index (arrow-rs / DuckDB / datafusion ready) |
| Worried about lock-in | Open S4F2/S4IX format + Apache-2.0 decoders — no gateway runtime needed to read your data |

**Best fit:** logs, JSON, Parquet/ORC, analytics archives, and other compressible S3 workloads.

## Does S4 make sense for your bill?

**You almost certainly do not need a GPU.** The `cpu-zstd` codecs capture
essentially all of the 50–80% storage savings on the common case (logs, JSON,
Parquet, mixed text) and run fine on a small or **burstable (t-series)** CPU
instance. GPU only pulls ahead on integer/columnar data at high throughput.
So the real question isn't "is my bill big enough for a GPU?" — it's "does cheap
CPU compute pay for itself against my S3 bill?", and the answer is yes far
sooner than a GPU-first framing suggests:

| Your monthly S3 bill | Likely savings (50–80%) | S4 host (CPU) | Net savings | Verdict |
|---:|---:|---:|---:|---|
| $100   | $50 – $80       | ~$30/mo (t3.medium, burstable) | **+$20 to +$50**    | ✅ Worth it even at small scale |
| $500   | $250 – $400     | ~$60/mo (t3.large / c7g.large) | **+$190 to +$340**  | ✅ Clear savings |
| $1,000 | $500 – $800     | ~$120/mo (c7g.xlarge)          | **+$380 to +$680**  | ✅ Clear savings |
| $3,000 | $1,500 – $2,400 | ~$120/mo (c7g.xlarge)          | **+$1,380 to +$2,280** | ✅✅ Strong ROI |
| $10,000 | $5,000 – $8,000 | ~$250/mo (c7g.2xlarge)        | **+$4,750 to +$7,750** | ✅✅✅ Material savings |

- **Storage bytes only.** PUT/GET request count and egress are unchanged (GET serves the decompressed payload).
- **Instance sizing tracks traffic, not your S3 bill** — a burstable t-series box suits low/spiky request rates; move to Graviton `c7g` as throughput climbs.
- **Start on CPU.** Reach for the GPU AMI only when your data is integer/columnar (`nvcomp-bitcomp` hits **>10×** vs zstd's ~2×) *and* ingest is high enough to keep a GPU busy. EC2 prices: us-east-1 on-demand, May 2026.
- **Host cost only.** If you run the paid Marketplace listing, its software fee is separate (the free OSS image has none) — plug your real numbers into `s4 estimate`.

Full methodology + the `s4 estimate` pre-deployment simulator: **[docs/savings.md](docs/savings.md)**.

> If this table matches your bill, point one test prefix at the **[Container listing](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e)** on a small CPU node — or the **[GPU AMI](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i)** if your data is integer/columnar.

## Proof

Headline roundtrip numbers (RTX 4070 Ti SUPER + Ryzen 9 9950X, single-pass
through `s4-codec`, 2026-05-13, nvCOMP 5.2.0.10 / CUDA 13.2). The dispatcher
samples entropy + magic bytes and routes **per object** — GPU is a multiplier
on the integer/columnar side, not a blanket "compress with GPU" claim:

| Workload | Best ratio | Best compress throughput | Codec verdict |
|---|---:|---:|---|
| nginx access log (256 MiB)   | **155×** (cpu-zstd-3) | 3.7 GB/s (cpu-zstd-3) | CPU wins — text deduplicates well at low CPU cost |
| Parquet-like mixed (256 MiB) | **2.09×** (nvcomp-bitcomp) | 1.5 GB/s (nvcomp-bitcomp) | GPU wins on Bitcomp for integer/columnar layouts |
| Postings (u32, 64 MiB)       | **11.9×** (nvcomp-bitcomp) | 1.6 GB/s (nvcomp-bitcomp) | GPU wins decisively on monotonic integer columns |
| Already-compressed (64 MiB)  | 1.00× (passthrough)  | 2.2 GB/s (passthrough)| Dispatcher detects + skips — no codec cost |

*These are single-pass codec ceilings measured offline; realistic production savings track the cost table above (50–80%). Multipart uploads at the default 4 MiB frame size compress repetitive logs far less than 155× until client chunk sizes are tuned — see [docs/benchmarks.md](docs/benchmarks.md).*

- **Compatibility** — S3-compatible for core object workflows, with 45+ S3 ops implemented (not a complete S3 API); MinIO is per-PR verified and AWS S3 E2E is opt-in. Full S3 / SDK / backend matrices: **[docs/compatibility.md](docs/compatibility.md)**; comparison vs MinIO / Garage / Wasabi / B2 below.
- **Trust signals** — 714+ workspace tests, a 24/7 fuzz farm (7 bolero targets), and adversarial Opus+Codex audit rounds; CVE-clean `cargo audit`. Details: **[docs/testing.md](docs/testing.md)** · **[docs/status.md](docs/status.md)** · **[full benchmarks](docs/benchmarks.md)**.

### How it compares

S4 runs **in front of** AWS S3 or any S3-compatible store — that's the backend
it compresses into, not something it competes with. Against the other tools you
might reach for to add compression to object storage:

| Feature | S4 | [MinIO](https://github.com/minio/minio) | [Garage](https://git.deuxfleurs.fr/Deuxfleurs/garage) | Wasabi / B2 |
|---|---|---|---|---|
| Stance | Compression gateway in front of your existing bucket | Standalone S3 system | Standalone S3 system | Hosted S3-compatible storage |
| **GPU compression** | ✅ nvCOMP zstd / Bitcomp / GDeflate | ❌ | ❌ | ❌ |
| **CPU compression** | ✅ zstd 1–22 / gzip | ⚠️ S2 only (legacy) | ✅ zstd 1–22 | ❌ |
| **Auto codec selection** | ✅ entropy + magic-byte sampling | ❌ | ❌ | — |
| **Range GET on compressed** | ✅ via S4IX sidecar | n/a | n/a | ✅ |
| **Works with your existing bucket** | ✅ (the whole point) | ❌ | ❌ | ❌ |
| **License** | Apache-2.0 | AGPLv3 (+ commercial) | AGPLv3 | proprietary |

*(License cells reflect upstream LICENSE files and can change between releases; not legal advice. Full matrices incl. SDK + backend coverage: [docs/compatibility.md](docs/compatibility.md).)*

## When NOT to use S4

Honest list of workloads where S4 doesn't pay off — it's better to know now:

- **Already-compressed payloads** (mp4, jpeg, gzip/zstd archives, Parquet with column codec on) — the dispatcher routes them to `passthrough`, so no harm, but no savings either.
- **Tiny / metadata-heavy workloads** (objects < 16 KiB, or `List`/`Head`/`Copy`-dominant traffic) — frame + sidecar overhead eats the ratio, and S4 adds a hop without touching the codec. Rule of thumb: objects > 1 MiB make the math comfortable.
- **Ultra-low-latency hot reads** (sub-10ms p99 GET) — streaming decode + sidecar fetch add latency. Great for analytics/archival; not for an OLTP read path.
- **Glacier-only cold storage** — Glacier already prices low enough that compression rarely pays for the compute.
- **Regulated workloads needing SOC2 / ISO 27001 / FedRAMP evidence today** — those reports don't exist yet; wait until they do.
- **Irreplaceable data, or a first production rollout** — there's no public production reference yet, so start on a replicated, versioned test prefix and keep backend-native recovery enabled.

> If your workload is compressible, object-sized, and not latency-critical, S4 is built for it. Start with the **[container listing](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e)** on a small CPU node — or the **[GPU AMI](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i)** for high-throughput integer/columnar data — and point one test prefix at the gateway first.

## Try it locally (60 seconds, CPU-only)

**Estimate your real bill first** — `s4 estimate` reads a bucket's object sizes + samples and projects savings, no gateway deploy required:

```bash
s4 estimate <bucket>[/prefix] --endpoint-url https://s3.<region>.amazonaws.com
```

Then kick the tires end-to-end against a throwaway local MinIO:

```bash
git clone https://github.com/abyo-software/s4 && cd s4
docker compose up -d                    # MinIO + S4 server on localhost:8014

# Generate a sample object so the cp lines have something to upload.
yes '2026-06-18T00:00:00Z INFO tenant=demo path=/api/v1/items status=200 bytes=1842' \
  | head -n 2000000 > big.log    # ~150 MiB of log-like text, compresses heavily

# Use any S3 client. Below uses aws-cli; replace endpoint with anything.
aws --endpoint-url http://localhost:8014 s3 mb s3://demo
aws --endpoint-url http://localhost:8014 s3 cp big.log s3://demo/big.log
aws --endpoint-url http://localhost:8014 s3 cp s3://demo/big.log ./big.log.roundtrip

# Inspect the compressed object directly on MinIO (different endpoint, bypasses S4).
aws --endpoint-url http://localhost:9000 s3 cp s3://demo/big.log ./big.log.compressed
ls -la big.log big.log.compressed big.log.roundtrip
# Expected: big.log == big.log.roundtrip (lossless), big.log.compressed is much smaller.
```

Other install paths — cargo, pip, WASM, build-from-source: **[docs/install.md](docs/install.md)**. GPU trial + tuning: **[docs/gpu.md](docs/gpu.md)**.

## Deploy

- **Container (EKS / ECS / Fargate)** — published image + Helm chart, runs on any CPU node, AWS-billed **per pod-hour** → **[Marketplace listing](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e)**. Same binary as the free `ghcr.io` image; metering is opt-in via `--marketplace-product-code` ([how it works](docs/marketplace/metering.md)). Marketplace pods need an entitlement + `aws-marketplace:RegisterUsage` and **fail closed at boot** if not entitled.
- **EC2 GPU AMI** — self-contained Amazon Linux 2023 image (NVIDIA drivers + S4 preinstalled), AWS-billed **per instance-hour** on g4dn / g5 / g6 / g6e; for integer/columnar data at high throughput. Launch, point your S3 clients at it, done → **[Marketplace listing](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i)**.
- **Self-managed Kubernetes** — `ghcr.io/abyo-software/s4` image + Helm chart: **[docs/deployment.md](docs/deployment.md)**.

## Operate

| Tool | Use |
|---|---|
| `s4 estimate` | Simulate storage savings on a bucket **before** deploying |
| `s4 savings` | Report measured, in-production savings (v1.2) |
| `s4 migrate` | Retro-compress objects already sitting in a bucket |
| `s4 recompact` | Re-pack cold data to higher zstd levels |
| `s4 maintain` | Policy-driven bucket maintenance (migrate / recompact / transition) |
| `s4 train-dict` | Shared zstd dictionaries for small, homogeneous objects |

Methodology + flags: **[savings](docs/savings.md)** · **[maintenance](docs/ops/maintenance.md)** · **[dictionaries](docs/ops/dictionaries.md)** · **[durability & repair](docs/ops/repair.md)** · **[runbook](docs/ops/runbook.md)** · **[configuration](docs/configuration.md)**.

## Stability & status

S4 is **v1.x** with a SemVer-stable surface: the backend wire format, core CLI
subcommands, library API, `s3s` HTTP trait set, and Helm `values.yaml` key
shape are frozen — pin `s4-server = "1"` or `ghcr.io/abyo-software/s4:1` and
rely on it not shifting under you. Full freeze contract: **[docs/stability.md](docs/stability.md)**.

> **No public production deployment reference yet.** The freeze is a contract on
> surface stability, not a substitute for operational track record. For
> TB-scale or irreplaceable data, pair S4 with backend-native versioning +
> replication, and please file an issue tagged `production-reference` if you
> deploy. Full status, audit history, and fuzz evidence: **[docs/status.md](docs/status.md)**.

## Documentation

| Area | Docs |
|---|---|
| Get started | [install](docs/install.md) · [GPU](docs/gpu.md) · [deploy (Helm)](docs/deployment.md) · [configuration](docs/configuration.md) |
| Use cases | [Elasticsearch frozen tier](docs/use-cases/elasticsearch-frozen-tier.md) — storage/throughput/frozen-search across LogsDB + zstd levels · [OpenSearch searchable snapshots](docs/use-cases/opensearch-searchable-snapshots.md) — −16–28% across index codecs (needs `--logical-etag`) · [Grafana Loki chunks](docs/use-cases/grafana-loki-chunks.md) — −18.4% on the immutable snappy backlog (honest split vs Loki-native zstd; ~1.7 ms read overhead) · [Kafka tiered storage](docs/use-cases/kafka-tiered-storage.md) — −74.7% on uncompressed (KIP-405) tiered segments / ~20% snappy-lz4 / ~0% producer-zstd (honest split vs producer-side compression) |
| Cost & operations | [savings & estimate](docs/savings.md) · [maintenance](docs/ops/maintenance.md) · [dictionaries](docs/ops/dictionaries.md) · [repair & durability](docs/ops/repair.md) · [runbook](docs/ops/runbook.md) · [observability](docs/observability.md) · [storage-class transitions](docs/storage-class-transitions.md) |
| Reference | [compatibility matrices](docs/compatibility.md) · [architecture](docs/architecture.md) · [on-the-wire format](docs/wire-format.md) · [production features](docs/features.md) |
| Proof & trust | [benchmarks](docs/benchmarks.md) · [testing & validation](docs/testing.md) · [stability contract](docs/stability.md) · [project status](docs/status.md) · [threat model](docs/security/threat-model.md) · [security overview](docs/security/overview.md) |
| Marketplace | [metering](docs/marketplace/metering.md) · [listing source-of-truth](docs/marketplace/listing.md) |

## More from abyo software

S4 is one of a family of AWS-native cost-optimization and security tools we
build in Rust, all on AWS Marketplace under one seller account — browse the
catalog at **[abyo software on AWS Marketplace](https://aws.amazon.com/marketplace/seller-profile?id=seller-65lhisp4ppavm)**.

| Product | What it does |
|---|---|
| **S4 — Squished S3** | This project: transparent GPU/CPU S3 compression gateway. → [Container](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e) · [GPU AMI](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i) |
| **S4 Logs** | CloudWatch Logs → S3 archiver that cuts log-storage cost. |
| **S4 LogForge** | Realistic SIEM test-log generator — parser-verified output across 13 formats. |
| **S4 Scan** | Amazon Athena scan-cost reducer. |
| **S4 NAT** | Cost-optimized NAT for Amazon VPC. |
| **S4 MockAPI** | Security API simulator for testing and demos. |

## Contributing

Pull requests welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for setup,
conventions, and the test/fuzz/soak protocol. Contributions are licensed under
Apache-2.0 (no separate CLA).

## Security

Found a vulnerability? Please **do not open a public issue** — follow
[SECURITY.md](SECURITY.md) for coordinated disclosure.

## License

Apache-2.0 ([LICENSE](LICENSE) / [NOTICE](NOTICE)). The optional `nvcomp-gpu`
feature pulls the proprietary NVIDIA nvCOMP SDK at build time (not bundled; BYO
under NVIDIA's terms). Full third-party disclosure:
[docs/THIRD_PARTY_LICENSES.html](docs/THIRD_PARTY_LICENSES.html).

`"S4"` and `"Squished S3"` are unregistered trademarks of abyo software 合同会社.
`"Amazon S3"` and `"AWS"` are trademarks of Amazon.com, Inc.; S4 is not
affiliated with, endorsed by, or sponsored by Amazon.

## Authors

- abyo software 合同会社 — sponsoring organization, commercial AMI distribution
- masumi-ryugo — original author / maintainer
