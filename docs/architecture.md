# Architecture

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
│  │   ├─ NvcompZstd           (nvCOMP, GPU, per-chunk)      │     │
│  │   ├─ NvcompBitcomp        (nvCOMP, integer columns)     │     │
│  │   └─ NvcompGDeflate       (nvCOMP, DEFLATE-family GPU)  │     │
│  └─────────────────────────────────────────────────────────┘     │
│  ┌─────────────────────────────────────────────────────────┐     │
│  │ s4-codec::CodecDispatcher                               │     │
│  │   ├─ AlwaysDispatcher                                   │     │
│  │   └─ SamplingDispatcher  (entropy + 12 magic bytes)     │     │
│  └─────────────────────────────────────────────────────────┘     │
└──────────────────────────────────────────────────────────────────┘
        ▲              ▲              ▲                ▲
        │              │              │                │
   /health         /ready         /metrics         OTLP traces
   (probe)        (probe)       (Prometheus)       (Jaeger / X-Ray)
```

### Streaming I/O

**Measurement conditions for the numbers below** (#107): RTX 4070 Ti
SUPER + Ryzen 9 9950X, single-pass 256 MiB compressible input, codec
`cpu-zstd-3` (or as noted), single concurrent request, S4 colocated
with backend (no network RTT to amortise). TTFB excludes TLS handshake
+ SigV4 verification (those add 5–15 ms once per connection).

- **Streaming GET** for non-multipart `cpu-zstd` / `passthrough` objects:
  TTFB **8–20 ms** under the conditions above, memory ≈ zstd window
  (8 MiB at level 3) + 64 KiB buffer
- **Streaming PUT** for the same codecs: input never fully buffered, peak memory
  ≈ compressed size (5 GB → ~50 MB at 100× ratio). Client-supplied whole-body
  checksums (`Content-MD5`, `x-amz-checksum-{crc32, crc32c, sha1, sha256, crc64nvme}`)
  are verified **in-stream** via a tee-into-hasher wrapper (v0.9 #106): mismatched
  bytes surface as `400 BadDigest` without buffering the body. GPU codecs and
  multipart `UploadPart` keep the buffered per-body / per-part verify path
  (the bytes are already in memory there for framing / padding) —
  see [`docs/security/streaming-checksum-coverage.md`](security/streaming-checksum-coverage.md)
  for the full coverage matrix and the codec-API constraint that makes
  this a fundamental property of those branches, not deferred plumbing
- **GPU streaming compress** (v0.2): nvCOMP `zstd` / `gdeflate` PUTs run a
  per-chunk pipeline so a 10 GB highly-compressible upload peaks at ~210 MB
  host RAM instead of buffering the full input
- **Single-PUT framed format unification** (v0.2): every compressed PUT now
  uses the same `S4F2` multi-frame format multipart uploads use, with an
  optional `<key>.s4index` sidecar. Range GET partial-fetch optimisation
  applies to single-PUT objects too, not just multipart
- **Multipart per-part compression**: each part compressed and frame-encoded
  (`S4F2` magic), per-frame codec dispatch (mixed codecs in one object)
- **Multipart final-part padding trim** (v0.2): the final part of a multipart
  with a tiny highly-compressible tail skips `S4P1` padding (saves up to
  ~5 MiB per object on highly compressible workloads)
- **Multipart non-final parts are padded to the S3 5 MiB minimum**: a
  compressible part whose framed output lands under 5 MiB gets a zero-filled
  `S4P1` padding frame back up to the floor (S3 rejects smaller non-final
  parts with `EntityTooSmall`). At-rest multipart savings are therefore
  floored at `5 MiB ÷ client part size` — 62.5% stored at the aws-cli
  default 8 MiB `multipart_chunksize` — until an `s4 recompact` rewrite;
  arithmetic and mitigation in
  [docs/savings.md](savings.md#multipart-uploads-the-5-mib-part-floor-caps-at-rest-savings)
- **Range GET via sidecar `<key>.s4index`**: only the needed compressed bytes
  are fetched from backend, decoded, and sliced. Falls back to full read when
  sidecar is absent
- **Encryption-aware Range GET fast-path** (v0.9 #106): SSE-S4 chunked
  (`--sse-chunk-size > 0`, S4E6 frame) Range GETs now partial-fetch just
  the enclosing S4E6 chunks from backend instead of pulling the full
  encrypted body. The v3 `<key>.s4index` sidecar carries the per-PUT salt +
  chunk geometry so the GET path can compute the encrypted byte range
  without re-fetching the header. SSE-KMS / SSE-C / SSE-S4 buffered
  (`--sse-chunk-size 0`) keep the v0.8.12 #120 buffered fallback (= full
  decrypt → frame-parse → slice); covering them needs separate plumbing
  (KMS DEK envelope shape, customer-key per-request material) and is on
  the v0.10+ roadmap
- **Byte-range aware `upload_part_copy`** (v0.2): when the source is S4-framed,
  the user-visible byte range is what gets copied (decompressed and re-framed),
  not raw compressed bytes
