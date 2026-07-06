# Production features

### Server-side encryption — Range GET fast-path matrix

S4 supports four SSE modes (table below). The **Range GET fast-path**
introduced in v0.9 #106 partial-fetches only the enclosing encrypted
chunks for a given byte range instead of pulling the full body — but it
only works for **SSE-S4 chunked** (`--sse-chunk-size > 0`, `S4E6` wire
envelope). The other three modes fall back to the v0.8.12 #120 buffered
path (full decrypt → frame-parse → slice).

| SSE mode | CLI flag | Wire envelope | Range GET fast-path? |
|---|---|---|---|
| SSE-S4 chunked (default since v0.8 #52) | `--sse-s4-key <path>` + `--sse-chunk-size 1048576` (default) | `S4E6` | ✅ partial-fetch via v3 sidecar |
| SSE-S4 buffered (back-compat) | `--sse-s4-key <path>` + `--sse-chunk-size 0` | `S4E2` | ❌ buffered fallback |
| SSE-C (customer-provided key) | per-request `x-amz-server-side-encryption-customer-*` headers | `S4E3` | ❌ buffered fallback |
| SSE-KMS (envelope, per-object DEK) | `--kms-local-dir <dir>` (or `--features aws-kms`) | `S4E4` | ❌ buffered fallback |
| Multipart with any SSE | (any of the above on a multipart PUT) | per-part `S4Ex` | ❌ no sidecar emitted (v0.8.16 #151) |

**Why only chunked SSE-S4?** Non-chunked envelopes (`S4E2` / `S4E3` /
`S4E4`) wrap the entire body under one AES-256-GCM authentication tag.
AEAD decrypt is only defined over the full ciphertext + AAD + tag
quadruple — there is no "verify just the prefix" mode — so partial
plaintext cannot be exposed without fetching and tag-verifying the
whole body. This is the AEAD security contract, not an optimization
deferment. The `S4E6` chunked envelope (v0.8 #52, refined in
v0.8.1 #57) explicitly slices the plaintext into fixed-size chunks
and emits one tag per chunk with a nonce derived from a per-PUT
salt + chunk index, which is what makes chunk-aligned partial
decrypt well-defined. Full per-mode walkthrough lives in
[`security/sse-partial-fetch-constraint.md`](security/sse-partial-fetch-constraint.md).

**Operator recommendation**: for Range-GET-heavy workloads on large
objects (parquet / ORC footer reads, video segment seeks, log-line
slice reads) where SSE is required, scope your data to **SSE-S4
chunked** to keep the fast-path. The 1 MiB default chunk size
matches the typical parquet row-group read pattern; smaller chunks
give finer-grained partial fetch at higher tag overhead, larger
chunks reduce on-disk tag bytes but do more wasted decrypt per Range
GET.

```bash
s4-server \
  --sse-s4-key /etc/s4/sse.key \
  --sse-chunk-size 1048576 \
  ...
```

If SSE-KMS or SSE-C is required by your key-management posture,
either accept the buffered Range GET cost or restructure the data
into smaller objects so the buffered fetch is bounded. Chunked-KMS
(provisional `S4E7`) and chunked-SSE-C (provisional `S4E8`)
envelopes are v0.11+ roadmap candidates, not promised features.

### Observability
- **`/health`** — liveness probe, always 200 OK
- **`/ready`** — readiness probe, runs `ListBuckets` against the backend
- **`/metrics`** — Prometheus text format
  (`s4_requests_total{op,codec,result}`, `s4_bytes_in_total`, `s4_bytes_out_total`,
  `s4_request_latency_seconds`, `s4_policy_denials_total{action,bucket}`)
- **Structured JSON logs** (`--log-format json`) with per-request fields:
  `op`, `bucket`, `key`, `codec`, `bytes_in`, `bytes_out`, `ratio`, `latency_ms`, `ok`
- **OpenTelemetry traces** (`--otlp-endpoint http://collector:4317`) — each
  PUT/GET emitted as `s4.put_object` / `s4.get_object` span with semantic
  attributes; export to Jaeger / Tempo / Grafana / AWS X-Ray.

### Security
- **Native HTTPS / TLS** (v0.2) — `--tls-cert` / `--tls-key` for direct
  termination via `tokio-rustls + ring`, ALPN advertises `h2` then
  `http/1.1`. No reverse-proxy required for HTTPS deployments.
- **Bucket policy enforcement at the gateway** (v0.2) — `--policy <path>`
  accepts an AWS-style bucket policy JSON; every PUT / GET / DELETE / List /
  Copy / UploadPartCopy is evaluated with explicit Deny > explicit Allow >
  implicit Deny semantics (matches AWS). Subset: `Effect`, `Action` (e.g.
  `s3:GetObject` / `s3:*`), `Resource` with glob, `Principal` (SigV4
  access-key match). Denials are bumped on
  `s4_policy_denials_total{action,bucket}`.

### Durable multipart state
- Every successful `UploadPart` / `UploadPartCopy` persists that part's
  `(original MD5, backend ETag)` pair as one small JSON record at
  `.s4mpu/<hex(uploadId)>/<partNumber>` in the backend bucket (**default**;
  opt out with `--no-durable-multipart-state`). A `CompleteMultipartUpload`
  handled by a **restarted** gateway — or a **different instance** of a
  multi-gateway deployment — therefore still returns the client-transparent
  composite ETag `MD5(concat(original-part-MD5s))-N` with strict part-ETag
  validation. Cost: one extra small backend PUT per part + record cleanup
  (prefix LIST + per-record DELETE) on Complete/Abort; record writes are
  best-effort (failures degrade that part to the pre-durable `ListParts`
  fallback, warned as `S4 durable multipart state` in the logs).
- Records contain **no SSE key material** — only content fingerprints. The
  per-upload SSE recipe remains in-memory-only (see
  [compatibility.md](compatibility.md#client-transparency-compression-is-invisible-to-the-client)).
- Orphaned records (gateway crashed between the backend Complete/Abort and
  its own cleanup) are reaped by an `s4 maintain` rule with
  `action = "mpu-state-gc"` — see [`s4 maintain`](ops/maintenance.md).

### Data Integrity
- **CRC32C** stored per-object (single PUT) or per-frame (multipart), verified on GET
- **`copy_object` S4-aware**: source's `s4-*` metadata is preserved across
  `MetadataDirective: REPLACE` (prevents silent corruption of the destination)
- **Zstd decompression bomb hardening**: `Decoder + take(manifest.original_size + 1024)`
  caps the decode at the manifest's declared size (+ a small overshoot margin) so a
  zero-size manifest paired with a high-ratio frame surfaces as a typed `Io("bomb
  detected")` instead of unbounded RAM growth. The cap is still bound by the
  manifest claim itself — a 5 GiB manifest is honored up to 5 GiB, so operators
  must additionally enforce a per-request memory ceiling at the listener
  (`--max-body-bytes` / a future per-frame cap) for adversarial uploads

### Storage class transitions
- Each compressed object is stored as `<key>` + `<key>.s4index` sidecar.
  S3 lifecycle rules must move both files together — a split pair breaks
  Range GET (sidecar in IA + main in Glacier ⇒ `InvalidObjectState`).
- Recommended: `"Filter": {}` (whole bucket) or a `Filter.Prefix` rule
  that covers both `foo/...` and `foo/....s4index`. Avoid size- or
  suffix-scoped filters that catch one but not the other.
- See [storage-class-transitions.md](storage-class-transitions.md)
  for two example lifecycle JSONs (IA-after-30d and prefix→Glacier-after-60d),
  the anti-pattern walkthrough, and a `head-object` drift-audit recipe.
- v1.2: a `transition` rule in an `s4 maintain` policy automates the
  same change from the S4 side, with the sidecar guaranteed to
  accompany its main object — see [`s4 maintain`](ops/maintenance.md).

### Parquet recompaction (offline, off-by-default feature)
- `s4 parquet-recompact <bucket>/<prefix>` reads cold Parquet objects and
  re-encodes their column chunks to **zstd**, writing back a **native** Parquet
  (pyarrow / Spark / Trino / DuckDB read it directly — **no S4 in the read
  path**). It is an offline rewrite (like `s4 recompact`), not the transparent
  gateway.
- **Build-time feature**, off by default (keeps the Arrow tree out of the default
  build, the same shape as `--features aws-kms`): build with
  `cargo install s4-server --features parquet-recompact`.
- **Safety**: dry-run by default; `--execute` additionally requires
  `--allow-lossy-physical-rewrite`. Each object is value-verified (per row group,
  bounded memory, Parquet physical-schema-tree compared) **before** the in-place
  overwrite — structural drift is a conservative skip, a decoded-value mismatch is
  a hard failure (downgradable with `--tolerate-value-mismatch`), a corrupt footer
  is a hard failure; it never overwrites with unverified data. Already-zstd
  objects are detected from the footer and skipped (idempotent). Objects under
  SSE / Object-Lock / `Expires` / archive tier / sort-order / bloom-filter
  metadata are skipped, not silently rewritten. The PUT is conditional
  (`If-Match` + pre-PUT re-HEAD of ETag / Last-Modified / version-id); run on
  cold/quiescent prefixes (`--older-than`).
- Measured −36.6% over snappy / −51.7% over uncompressed in a local benchmark —
  see the [cold-Parquet use case](use-cases/cold-parquet.md).
