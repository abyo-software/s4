# S4 — Social media post drafts

各プラットフォーム / sub ごとに文面を分けています。文字数は X で 280 字制限、URL=23 字換算で fit 済。Reddit は self-promo 規約に注意 (特に r/rust)、コメントで質問返しまでセットで運用。

---

## X (Twitter)

### 日本語

```
S3 ストレージ請求を 50-80% 削る OSS、S4 v0.8.10 公開しました。

`--endpoint-url` を差し替えるだけの S3 互換ゲートウェイ。GPU (NVIDIA nvCOMP) / CPU を payload 別に自動選択で透過圧縮。

実測: nginx 155× / Parquet 2.09× / u32 posting 11.9×

Rust / Apache-2.0
https://github.com/abyo-software/s4
```

### English

```
S4 v0.8.10 — OSS S3-compatible gateway with transparent GPU (NVIDIA nvCOMP) / CPU compression, auto-picked per payload.

Swap `--endpoint-url` and:
- nginx log 155×
- Parquet ints 2.09×
- u32 postings 11.9×

Rust / Apache-2.0
https://github.com/abyo-software/s4
```

画像添付候補: `docs/perf-v0.8.png`

---

## Reddit — r/rust

**Flair:** `🛠️ project`
**Title:**

```
S4 v0.8.10 — S3-compatible gateway in Rust with transparent GPU (nvCOMP) / CPU compression dispatch
```

**Body:**

```markdown
S4 (Squished S3) is an S3-compatible storage gateway I've been building in Rust.
It sits between any S3 client (boto3, aws-cli, Spark, DuckDB, ...) and a real S3
backend, and transparently compresses each object with a codec picked per payload:

- **text / logs** → `cpu-zstd` (zstd-rs, streaming)
- **integer / columnar** (Parquet, postings, time-series) → `nvcomp-bitcomp` /
  `nvcomp-gdeflate` (NVIDIA nvCOMP, GPU)
- **already-compressed** (mp4 / jpeg / gz / parquet-with-codec) → `passthrough`
  (entropy + magic-byte sniff so we never re-inflate)

Apps don't change — just swap `--endpoint-url`. Single-pass roundtrip numbers
on a 4070 Ti SUPER + 9950X (nvCOMP 5.2.0.10, CUDA 13.2):

| Workload                  | Best codec       | Ratio   | Compress |
|---------------------------|------------------|--------:|---------:|
| nginx access log 256 MiB  | cpu-zstd-3       | 155.0×  | 3.7 GB/s |
| Parquet-like mixed 256 MiB| nvcomp-bitcomp   |  2.09×  | 1.5 GB/s |
| u32 posting 64 MiB        | nvcomp-bitcomp   | 11.93×  | 1.6 GB/s |
| already-compressed 64 MiB | passthrough      |  1.00×  | 2.2 GB/s |

A few Rust-flavored bits I'm happy with:
- 38 proptest properties × 1M cases nightly, 7 bolero coverage-guided targets,
  in-process TLS E2E via rcgen + tokio-rustls
- Fuzz infra already caught 2 real bugs pre-release (FrameIter infinite-loop on
  1-byte input, zstd decoder bomb via attacker-controlled manifest size)
- `cargo install s4-server` (CPU build); GPU needs `--features nvcomp-gpu` +
  `NVCOMP_HOME`
- Open wire format (S4F2 frame + S4IX sidecar) — `s4-codec` (CLI), `s4-codec-py`
  (pip), `s4-codec-wasm` (browser) all decode without the gateway running
- Apache-2.0, edition 2024, rust 1.92+

Bench reproduction recipe + cost-savings honest-table (when it does *not* pay
off) in the README.

Repo: https://github.com/abyo-software/s4

Feedback / issues welcome — especially on the multipart streaming compress
pipeline (`crates/s4-codec/src/multipart.rs`) and the SamplingDispatcher's
12 magic-byte rules.
```

---

## Reddit — r/aws

**Title:**

```
Open-sourced an S3 gateway that transparently compresses your bucket — 50-80% storage savings with zero app changes
```

**Body:**

```markdown
TL;DR: Apache-2.0 Rust gateway that speaks S3 on both sides. Point your SDK's
`--endpoint-url` at it; it compresses on PUT, decompresses on GET, and stores
the squished bytes in your real S3 bucket.

Why I built it: my S3 bill grew linearly with data, but most of that data was
≥3× compressible (logs, JSON, Parquet). MinIO's S2 codec is CPU-only and
legacy; nothing in front of AWS S3 just *did this*.

**Honest cost table** (us-east-1 on-demand, May 2026):

| Monthly S3 bill | Likely savings | EC2 GPU cost  | Net          | Verdict |
|----------------:|---------------:|--------------:|-------------:|---------|
| $500            | $250-$400      | $730 (g6.xl)  | -$330..-$480 | ❌ skip |
| $3,000          | $1.5k-$2.4k    | $730          | +$770..+$1.7k| ✅ yes  |
| $10,000         | $5k-$8k        | $1,860 (g6e)  | +$3.1k..$6.1k| ✅✅    |
| $50,000         | $25k-$40k      | $1,860        | +$23k..$38k  | ✅✅✅  |

Under ~$1k/mo, don't bother — use the CPU-only build on a small instance or
just front your bucket with nginx + gzip.

**What's covered**:
- S3 API: PUT/GET, full Range GET spec (`bytes=N-M`, suffix, open-ended),
  multipart (create/part/complete/abort), HEAD, conditional GET/PUT,
  versioning, object lock, lifecycle, replication, bucket policy (JSON
  Allow/Deny with IpAddress/StringLike/Bool conditions), SSE-S3/SSE-KMS/SSE-C,
  presigned URLs, SigV4 + SigV4a, S3 Select subset, tagging, CORS, inventory
- Drop-in for `aws-cli` / `boto3` / `aws-sdk-rust` / `mc` / `rclone`
- Range GET on compressed objects via per-frame index sidecar (Parquet/ORC
  readers work unmodified)
- Prometheus `/metrics`, OTel traces, structured JSON access log
- Native TLS termination (rustls + ring) + ACME / Let's Encrypt
- No lock-in: stop the gateway and the compressed objects stay S3-native;
  `s4-codec` CLI / pip / WASM all decode without the gateway

**What's NOT covered**: ultra-low-latency tail SLOs (sub-10ms p99 GET),
tiny objects (< 16 KiB — frame header eats the ratio), already-compressed
payloads (correctly bypassed but you pay the round-trip), strict regulatory
deployments (no SOC2/FedRAMP audit yet — pre-1.0, pair with backend
versioning).

Repo + 60s docker compose trial: https://github.com/abyo-software/s4

Happy to answer cost-modelling / IAM-scoping / SDK-compat questions in the
comments.
```

---

## Reddit — r/dataengineering

**Title:**

```
S3 gateway with transparent GPU compression — 2.09× on Parquet, 11.9× on sorted u32 posting lists (nvCOMP Bitcomp)
```

**Body:**

```markdown
S4 is an open-source S3-compatible gateway (Apache-2.0, Rust). The interesting
bit for /r/dataengineering: it routes per payload to the right codec, including
NVIDIA's nvCOMP Bitcomp for integer/columnar layouts.

Single-pass roundtrip on a 4070 Ti SUPER + 9950X (last benched 2026-05-13):

| Workload                       | Codec            | Ratio  | Compress  |
|--------------------------------|------------------|-------:|----------:|
| Parquet-like mixed 256 MiB     | nvcomp-bitcomp   | 2.09×  | 1.49 GB/s |
| u32 posting 64 MiB             | nvcomp-bitcomp   | 11.93× | 1.61 GB/s |
| i64 monotonic timestamps 64 MiB| nvcomp-bitcomp   | 2.95×  | 1.45 GB/s |
| i64 doc_values 64 MiB          | nvcomp-zstd      | 1.86×  | 1.04 GB/s |

Bitcomp's `data_type` hint is doing the heavy lifting — pass `Char` on numeric
data and you degrade to ~1.2×. Typed constructors are exposed in `s4_codec::
nvcomp::BitcompDataType`.

**Why it matters for analytics workloads**:
- Range GET on compressed objects works via an S4IX sidecar (per-frame offset
  + original-size + crc32c). arrow-rs / datafusion / duckdb's parquet reader
  issue suffix-range GETs against the footer and they work unmodified
- 4 MiB default frame size = decoders only fetch the covering frames, not the
  full object. Parallel range reads against overlapping frames currently do
  extra decode work (tracked in #99 — parquet/ORC cross-validation harness on
  the roadmap)
- Open format: `s4-codec` CLI / pip / WASM decode without the gateway in the
  read path. Stop the gateway and your data stays portable

**Caveat for storage-class transitions**: the `<key>` + `<key>.s4index` pair
must move together. S3 lifecycle rules with size- or suffix-scoped filters
that catch one but not the other will break Range GET (`InvalidObjectState`
when the sidecar lands in IA and the main is in Glacier). Use `"Filter": {}`
or a prefix that covers both — full example JSON in
`docs/storage-class-transitions.md`.

**When NOT to use it**:
- Small objects (< 16 KiB): frame header eats the ratio
- Metadata-ops-dominant workloads (heavy List/Head against millions of small
  keys): you pay the extra TLS hop without touching the codec
- Already column-compressed Parquet (parquet-zstd / parquet-snappy): the
  dispatcher detects + passthroughs, no harm done but no savings either

Repo: https://github.com/abyo-software/s4

Curious if anyone here has a workload where you wished S3 just *did* this —
especially happy to chat about Parquet footer access patterns and whether the
sidecar fetch deserves to be inlined.
```

---

## Reddit — r/devops

**Title:**

```
S4 — drop-in S3-compatible gateway that transparently compresses your bucket (Rust, Apache-2.0, Prometheus + OTel out of the box)
```

**Body:**

```markdown
Spent the last couple of months building this and v0.8.10 just shipped.
Posting in case it saves someone an "our S3 bill is doing what now" meeting.

**The pitch**: S4 sits in front of your real S3 bucket and transparently
compresses each PUT, decompresses each GET. Apps don't change — same SigV4,
same SDK calls, just swap `--endpoint-url`. GPU codecs (NVIDIA nvCOMP) for
columnar/integer data, CPU zstd for text/logs, passthrough for
already-compressed payloads (entropy + magic-byte sniff).

**Ops surface**:
- `/health` (always 200), `/ready` (runs `ListBuckets` against backend),
  `/metrics` (Prometheus text)
- Metrics: `s4_requests_total{op,codec,result}`, `s4_bytes_in_total`,
  `s4_bytes_out_total`, `s4_request_latency_seconds`,
  `s4_policy_denials_total{action,bucket}`, `s4_codec_chosen_total{codec}`,
  `s4_sse_aes_backend{kind}`
- Structured JSON access log (`--log-format json`) with `op`, `bucket`,
  `key`, `codec`, `bytes_in`, `bytes_out`, `ratio`, `latency_ms`, `ok`
- OpenTelemetry OTLP traces (Jaeger / Tempo / Grafana / AWS X-Ray)
- Native TLS termination (rustls + ring, ALPN h2) with SIGHUP cert hot-reload
- ACME / Let's Encrypt via TLS-ALPN-01 (no separate :80 listener)
- AWS-style bucket policy JSON evaluated at the gateway *before* backend
  dispatch (Allow/Deny + IpAddress/StringLike/Bool/Date conditions)

**Deploy**:
- `cargo install s4-server` (CPU build) — needs Rust 1.92+
- `docker compose up -d` for 60-second MinIO + S4 trial
- Helm chart at `charts/s4/` — build + push to your own registry first
  (no `abyosoftware/s4` image published yet, public release pending first
  prod user)

**Honest cost table** (us-east-1 on-demand): breakeven is ~$1k/mo S3 bill on
on-demand g6.xl, ~$300/mo on spot. Below that, run the CPU-only build on a
small instance or just front the bucket with nginx + gzip.

**Failure modes documented** in the README (the "Durability, corruption
recovery, and the repair tool" section): client mid-PUT disconnect, sidecar
divergence, backend corruption (per-frame crc32c catches it, returns 500
rather than corrupted bytes). `s4-tool repair-sidecar` lands in v0.9.

Repo: https://github.com/abyo-software/s4

Critique welcome on the threat model / TLS termination / multi-tenant story
(short answer: single-tenant by design, one instance per security boundary).
```

---

## Reddit — r/selfhosted

**Title:**

```
S4 — compression gateway that sits in front of MinIO / Garage / AWS S3 (Rust, Apache-2.0)
```

**Body:**

```markdown
Just shipped v0.8.10. Posting because the typical /r/selfhosted setup
(MinIO + a few TB of media / backups / logs) is exactly where this pays off
if the data is compressible.

**What it is**: S3-compatible gateway. Talks S3 on the client side
(boto3, aws-cli, restic, rclone, ...), talks S3 on the backend side (your
MinIO, Garage, Ceph RGW, or AWS S3). Transparently compresses each PUT,
decompresses each GET. No app changes — just swap the endpoint.

**Codec dispatch is the interesting bit**: it samples the first 4 KiB of
each payload (entropy + 12 magic-byte rules) and routes per object:
- text / logs → `cpu-zstd`
- integer / columnar → `nvcomp-bitcomp` (needs NVIDIA GPU)
- already-compressed (mp4 / jpeg / gz / zip / 7z / pdf / ...) → `passthrough`,
  no harm done

CPU-only build is the default — GPU is opt-in (`--features nvcomp-gpu`).
For a homelab that's mostly media (already compressed) + nightly DB dumps +
logs, the cpu-zstd path on the dumps and logs alone is the win.

**60-second trial** (MinIO + S4):

    git clone https://github.com/abyo-software/s4 && cd s4
    docker compose up -d
    aws --endpoint-url http://localhost:8014 s3 mb s3://demo
    aws --endpoint-url http://localhost:8014 s3 cp big.log s3://demo/big.log

**No lock-in**: stop the gateway and the compressed objects + `.s4index`
sidecars are still S3-native (any client can list / download them). To
decode the original bytes without the gateway running: `s4-codec` CLI,
`s4-codec-py` (pip), or `s4-codec-wasm` (browser, 10-line example in repo).
All Apache-2.0.

**When NOT to use it for a homelab**:
- Mostly media (movies / photos): already compressed, dispatcher passthroughs,
  you pay the extra TLS hop for no savings
- Sub-millisecond latency requirements: there's a streaming GET warm-up +
  sidecar fetch overhead (one extra round-trip when not cached)
- Tiny objects (< 16 KiB): frame header eats the ratio

Repo: https://github.com/abyo-software/s4

Happy to help anyone wire it in front of MinIO — the docker-compose in the
repo already does that combination.
```
