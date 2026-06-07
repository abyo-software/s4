# S4 threat model

**Last reviewed:** v0.8.22 (2026-06-07)
**Stamp policy:** bumped on every cut so the threat model + runbook
review stamps stay aligned; companion doc is
[`docs/ops/runbook.md`](../ops/runbook.md).
**Scope:** S4 — S3-compatible gateway with transparent GPU/CPU
compression. Single-binary `s4-server` listening on HTTP/HTTPS,
talking to an operator-provided S3-protocol backend (AWS S3,
MinIO, Garage, Ceph RGW, …).

This document follows the **STRIDE** framework (Spoofing /
Tampering / Repudiation / Information disclosure / Denial of
service / Elevation of privilege) for each attack surface.

## In-scope assets

1. **User object bytes in transit** (client → S4 → backend).
2. **User object bytes at rest** on the backend, when SSE is
   configured.
3. **Encryption keys** (`--sse-s4-key` + rotated keys, SSE-C
   customer keys, SSE-KMS DEKs in memory, KEKs on disk).
4. **Bucket policy + manager state** (in-memory; optionally
   persisted via the per-manager `--<x>-state-file` flags
   exposed for versioning, object_lock, mfa_delete,
   cors, inventory, notifications, tagging, replication,
   lifecycle — each manager owns one file; there is no
   single `--state-dir` aggregating them).
5. **IAM permissions** (SigV4 / SigV4a credentials, MFA-Delete
   secrets).
6. **Object Lock state** (Compliance / Governance retention,
   legal holds).
7. **Operational integrity** (the gateway's availability + its
   refusal to silently corrupt data).

## Out-of-scope (explicit non-goals)

- **Confidentiality of the backend itself.** S4 trusts the
  operator-chosen S3 backend; if the backend is malicious or
  compromised, S4 cannot defend.
- **Network confidentiality without TLS.** Operators MUST
  terminate TLS at the gateway (`--tls-cert` / `--tls-key` or
  `--acme`) or at a reverse proxy in front of S4. Plain HTTP is
  intended for trusted-network localhost development only.
- **Side-channel attacks against AES-GCM via co-tenancy.** Single-
  tenant deployment model.
- **Denial-of-service against the host OS** (e.g. fork bombs,
  kernel exploits). Run inside `systemd` / k8s with the usual
  resource limits.
- **Insider attacks by the gateway operator.** The operator owns
  the keys; key compromise is a recovery-procedure problem
  (see `docs/ops/runbook.md`), not a threat-model problem.
- **Supply chain attacks against build-time dependencies.** We
  pin via `Cargo.lock`, run `cargo audit` in CI, and produce
  SBOM via cargo-about; further supply-chain hardening
  (sigstore, reproducible builds) is roadmap.

## Threat surfaces

### 1. Public S3 wire (client → S4 listener)

| STRIDE | Threat | Mitigation |
|---|---|---|
| Spoofing | Attacker forges a SigV4 / SigV4a signature for another principal | SigV4 verified by `s3s` framework; SigV4a verified by `crate::sigv4a` against an operator-supplied credential store. Per v0.8.16 #148/#150 the canonical request is byte-level RFC 3986 encoded (no `decode_utf8_lossy` mangling); per v0.8.15 #126 / v0.8.16 #148 the string-to-sign matches the AWS canonical recipe. `x-amz-content-sha256` is **required** to be present AND in `SignedHeaders=` (v0.8.16 #148); `host` is required in `SignedHeaders=` (v0.8.15 #133). |
| Spoofing | Attacker spoofs `X-Forwarded-For` to satisfy a policy `IpAddress` condition | v0.8.11 CRIT-4 (`--trust-x-forwarded-for` opt-in). Default ignores the header; operators behind a trusted reverse proxy opt in explicitly. |
| Spoofing | SigV4a Authorization header with duplicate `Credential=` / `SignedHeaders=` / `Signature=` (auth-confusion) | v0.8.12 #123 rejects duplicates. |
| Tampering | Client mutates request body after signing | `x-amz-content-sha256` SHA-256 of body, signed → body integrity covered. |
| Tampering | Client manipulates `MetadataDirective: REPLACE` CopyObject to inject `s4-*` metadata | v0.8.15 #138 / v0.8.16 #152 strip every `s4-*` key from client-supplied metadata before re-populating from source HEAD. Strip runs unconditionally even when HEAD fails. |
| Tampering | Client over-declares `Content-Length` (body shorter) or under-declares (body longer) | v0.8.4 #73 / v0.8.15 #140 / v0.8.16 #154 — `TruncatedStream` (short) + `OverlengthStream` (long) both surface mid-flight as 400 `IncompleteBody` / `RequestBodyLengthMismatch`. |
| Repudiation | Client denies having made a request | Structured access logs (`--access-log`) carry per-request signature material; v0.5 #31 audit log HMAC chain provides tamper-evident sequencing across rotations. |
| Info disclosure | Client lists or reads internal sidecar artifacts (`<key>.s4index`) | v0.8.15 #137 → v0.8.17 #161: reserved-name guard rejects PUT / Copy / Create-Multipart / GET / HEAD / DELETE / ACL / Tagging / Attributes / restore / upload_part_copy on any key ending in `.s4index`. List filter already hides them. |
| Info disclosure | SigV4a request without `x-amz-content-sha256` falls back to `UNSIGNED-PAYLOAD` and bypasses body integrity | v0.8.16 #148 requires the header. |
| DoS | Forged manifest claims huge `original_size`, drives memory alloc | v0.8.6 #89: bootstrap-capped `Vec::with_capacity` + `Decoder::take` cap. v0.8.16 #136 adds aggregate output cap across multipart frames. |
| DoS | Forged sidecar `n = huge` value drives allocation | v0.8.12 #124: `BOOTSTRAP_ENTRIES = 4096` cap on initial alloc. v0.8.15 #131 adds `MAX_FRAMES = 16M` + `MAX_ETAG_BYTES = 4 KiB` ceilings rejected before the `as usize` cast. |
| DoS | Attacker generates unique fake `AKIA*` access-key-ids to balloon rate-limit DashMap | v0.8.12 #121 caps at 16384 active limiters with per-rule shared overflow. |
| DoS | Slowloris / connection exhaustion | v0.8.5 #84 — `--max-concurrent-connections` semaphore + `--read-timeout-seconds` + HTTP/2 stream cap. |
| Elevation | Bucket policy with `NotAction` / `NotResource` / `NotPrincipal` silently widens permissions | v0.8.11 CRIT-5 — `#[serde(deny_unknown_fields)]` fails policy parse closed. |
| Elevation | `x-amz-bypass-governance-retention` header lets unprivileged caller break Governance lock | v0.8.12 #117 — requires `s3:BypassGovernanceRetention` IAM Allow; flag silently downgraded to false otherwise. |
| Elevation | Multipart wire path bypasses bucket policy that denies `s3:PutObject` | v0.8.12 #119 — Create / UploadPart / Complete / Abort / upload_part_copy all run `enforce_policy("s3:PutObject", …)`. |
| Elevation | Object Lock admin APIs (legal hold, retention, lock config) ungated | v0.8.12 #118 — `s3:PutObjectLegalHold` etc. enforced. |
| Elevation | MITM rewrites `Host` header to redirect to a different bucket on the same listener | v0.8.15 #133 — SigV4a requires `host` in `SignedHeaders=`. |
| Elevation | SigV4a presigned URLs silently accepted as unsigned | v0.8.16 #149 / v0.8.17 #160 — 501 `NotImplemented` returned unconditionally. |

### 2. Compressed payload at rest

| STRIDE | Threat | Mitigation |
|---|---|---|
| Tampering | Attacker with backend write access flips bits in a compressed object | Per-frame CRC32C verified on GET; SSE modes wrap with AES-256-GCM whose tag covers the framed bytes. Out-of-band overwrite without ETag change still possible — `s4index` sidecar's `source_etag` + `source_compressed_size` binding (v0.8.4 #73 H-2) trips the staleness check on Range GET. |
| Tampering | Sidecar entry overflow / non-monotonic offsets to crash the range planner | v0.8.15 #130, v0.8.16 #146 — per-entry `checked_add` + inter-entry monotonicity check, typed errors. |
| Info disclosure | Range GET on encrypted object slices ciphertext at pre-encrypt offsets | v0.8.12 #120 suppresses sidecar when SSE is on; encrypted Range GET buffers + decrypts + frame-parses + slices. Trade partial-fetch perf for correctness. |
| DoS | Decompression bomb — small compressed manifest, huge decompressed output | v0.8.6 #89 — `Decoder::take(manifest.original_size + 1024)` cap. v0.8.16 #145 fixes the dead-code probe so log messages distinguish "truncated at cap" from "decoder hit EOF". v0.8.16 #136 caps aggregate multipart output at `--max-body-bytes`. |
| DoS | 32-bit WASM client (`s4-codec-wasm`) tricked by forged `compressed_size = 4 GiB+` | v0.8.15 #131 — `usize::try_from` rejects with `PayloadTooLarge` instead of silent truncation. |

### 3. Encryption key handling

| STRIDE | Threat | Mitigation |
|---|---|---|
| Spoofing | SSE-C customer-key MD5 collision | MD5 is a non-secret fingerprint only — actual decrypt fails closed via AES-GCM tag on key mismatch. |
| Info disclosure | SSE-C key bytes survive in memory after `AbortMultipartUpload` | v0.8.2 #62 / v0.8.4 #71 — `Zeroizing<[u8; 32]>` wrap + drop on Abort/Complete. |
| Info disclosure | SSE-KMS DEK plaintext lingers across `put_object` scope | v0.8.1 #58 — `Zeroizing` on the stack + heap copies. |
| Info disclosure | Replication ships plaintext to destination even when source is SSE-encrypted | v0.8.11 #112 — `replication_body` refreshed with post-encrypt body. |
| Info disclosure | Chunked SSE-S4 GET returns un-decompressed framed bytes (data corruption shape rather than disclosure, but listed here for completeness) | v0.8.11 #111 — streaming early-return restricted to `codec == Passthrough && !needs_frame_parse`. |
| Elevation | Operator forgets to rotate the active key but adds a new one | `--sse-s4-key-rotated id=N,key=PATH` keeps old keys around for decrypt; the active slot is always id=1. The `keyring_rotation_e2e` integration test covers the happy path. |

### 4. Backend trust boundary

S4 trusts the operator-configured backend endpoint. The backend
sees compressed (and optionally encrypted) bytes; the operator
controls backend access via their normal AWS / MinIO / etc.
mechanisms. S4 enforces:

- **`--max-body-bytes`** — refuses to forward bodies larger
  than the operator-set cap (default 5 GiB = AWS S3 single-PUT
  limit). Exposed as a CLI flag since v0.8.19; older builds
  could only set it via the `with_max_body_bytes` library
  builder.
- **No SSRF** — outbound HTTP only via `aws-sdk-s3` against the
  configured endpoint; cross-region replication is restricted
  to the same single-instance backend in v0.8.x.
- **No client-driven destination override** — `replication`
  config is operator-supplied at boot; clients cannot redirect
  it.

### 5. Object Lock compliance posture

| STRIDE | Threat | Mitigation |
|---|---|---|
| Tampering | Client uses `DeleteObjects` batch to remove a Compliance-locked object | v0.8.11 CRIT-3 — every batch entry dispatches through gated `delete_object`. |
| Tampering | Client uses `CompleteMultipartUpload` to overwrite a legal-held key | v0.8.12 #116 — Complete re-verifies Object Lock; Compliance / legal hold never bypassable on Complete. |
| Tampering | Bucket-default retention silently overwrites a legal-hold-only key | v0.8.15 #141 / v0.8.16 #158 — `apply_default_on_put` skip predicate includes `legal_hold_on`; expired retention correctly re-arms on next PUT. |

## Cryptographic primitives

- **AES-256-GCM** via `aes-gcm` 0.10 (RustCrypto, audit history
  publicly tracked).
- **ECDSA-P256-SHA256** via `p256` 0.13 for SigV4a.
- **HMAC-SHA256** via `hmac` 0.12 for SigV4 and audit-log
  HMAC chain.
- **CRC32C** via `crc32c` 0.6 (Castagnoli polynomial).
- **SHA-1 / SHA-256 / CRC32 (IEEE) / CRC64-NVME** for client-
  supplied integrity headers (v0.8.13 #128).
- **MD5** via `md-5` 0.10 — used **only** as the SSE-C key
  fingerprint and `Content-MD5` body integrity, never as a
  security primitive.

Random number generation goes through `rand 0.8` (`OsRng`) for
nonces, DEKs, and version IDs.

## Known residual risks

These items are acknowledged and tracked, not silently hidden:

1. **`rustls-webpki 0.101.7` CVE chain via `aws-config`** —
   pinned by the `s3s 0.13` + `aws-sdk-s3 1.124` dependency
   constraint. The vulnerabilities (RUSTSEC-2026-0098 / 0099 /
   0104) live in certificate name-constraint / CRL parsing
   on the **backend** TLS path, which talks only to the
   operator-trusted backend endpoint. Mitigation: pin to a
   trusted backend; upgrade path follows the s3s + aws-sdk-s3
   release schedule. Tracked in issue #91.
2. **Streaming PUT now verifies client checksums (v0.9 #106)** —
   the v0.8.13 #127 attempt regressed sidecar correctness
   (v0.8.14 #129). v0.9 #106 lands true streaming verify
   (tee-into-hasher) for single-PUT cpu-zstd / nvcomp-zstd
   (the streaming-framed path). Mismatched bodies surface
   as `400 BadDigest` without buffering. Both delivery
   shapes are covered: client checksums carried as request
   headers (`Content-MD5`, `x-amz-checksum-*`) are
   compared eagerly at EOF inside the tee, and the
   chunked / SigV4-streaming SDK case where the value
   arrives in **request trailers** (announced via
   `x-amz-trailer`) is compared after the body has been
   consumed, against the digest stashed by the tee. GPU
   codecs that fall into the bytes-buffered branch
   (currently the GPU codecs that don't yet
   `supports_streaming_compress`) and `UploadPart` still
   use the existing buffered `verify_client_body_checksums`
   (#122 / #128), which covers all six AWS checksum
   algorithms.
3. **Range GET on encrypted objects is buffered** — no
   sidecar fast-path until an encryption-aware sidecar format
   lands (post-launch roadmap).
4. **Versioned multipart Complete writes no sidecar** —
   v0.8.16 #151 skips sidecar emission entirely for those
   bucket states. Range GET falls back to full read. **Cost
   note**: for large multipart objects (≥ 100 MiB) on a
   versioned bucket, a range-heavy client workload pays the
   full-object read cost on every Range GET until the
   shadow-key-bound sidecar lands as a follow-up. Operators
   serving heavy Range traffic should weigh the option of
   leaving versioning Disabled for that bucket against the
   cost of full reads.

## Recovery procedures

See `docs/ops/runbook.md` for operator procedures on:

- SSE key rotation / compromise
- Backend partition recovery
- Sidecar orphan sweep (v0.8.15 H-g window)
- Migration of pre-v0.8.15 `.s4index` user data
- MFA secret loss

## Review cadence

Threat model is reviewed at every minor release that touches
the listener edge or the codec wire format. Last full review:
v0.8.22 (post seven audit cycles).
