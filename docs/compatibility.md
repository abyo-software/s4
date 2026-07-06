# Compatibility matrices

### S3 API compatibility matrix

S4 implements the parts of the S3 API needed to act as a transparent
compression proxy in front of an existing bucket. **It is not a complete
S3 implementation** — operations marked "—" return `NotImplemented` and
should not be called against an S4 endpoint. PRs welcome on the matrix
rows you need.

| Surface | Status | Notes |
|---|---|---|
| PUT / GET object | ✅ Full | single-PUT + range-GET (see below). **Client-transparent ETag** = `MD5(original)` by default (see [§ Client transparency](#client-transparency-compression-is-invisible-to-the-client)) |
| Multipart upload (create / part / complete / abort) | ✅ Full | per-part framing + final-part padding trim. Composite object ETag is the AWS `MD5(concat(original-part-MD5s))-N` form — **best-effort** (see § Client transparency) |
| HEAD object | ✅ Full | returns the **original** `Content-Length` and the client-transparent ETag; S4's `s4-*` control metadata is not exposed |
| Range GET | ✅ S3 spec | `bytes=N-M`, `bytes=-N` (suffix), `bytes=N-` (open-ended); range maps through the S4IX sidecar; `s4-*` stripped + logical ETag echoed on the partial response |
| Conditional GET / PUT / Copy (`If-Match` / `If-None-Match` / `If-Modified-Since`) | ✅ read-path full; write-path best-effort | read-path is fully evaluated against the logical ETag. Write-path (`If-Match` on PUT, `x-amz-copy-source-if-*` on Copy) is evaluated against the logical ETag too, but **non-atomically** (HEAD-then-write) — see § Client transparency |
| PutObjectAcl / GetObjectAcl | ✅ canned ACLs only | `private` / `public-read` / `public-read-write` / `authenticated-read` / `aws-exec-read` / `bucket-owner-read` / `bucket-owner-full-control` |
| Bucket versioning | ✅ Full | per-version UUIDv4 ID, delete-marker semantics |
| Object lock (Governance / Compliance) | ✅ Full | per-object retention + legal-hold |
| Bucket lifecycle (`LifecycleConfiguration`) | ✅ Full | Expiration / NoncurrentVersionExpiration / AbortIncompleteMultipartUpload |
| Bucket notifications (Webhook / SQS / SNS) | ✅ Full | SQS/SNS gated behind `aws-events` feature |
| Bucket replication | ⚠ experimental | rule-based, per-PUT dispatcher; ships as **experimental scaffolding** (wire path + config surface only). **Excluded from the v1.0 freeze** — promotion to production-grade is on the v1.x roadmap. |
| Bucket policy | ✅ AWS-style JSON | Allow / Deny, IAM Conditions subset (see #100) |
| Tagging (object / bucket) | ✅ Full | |
| CORS configuration | ✅ Full | |
| Inventory | ✅ Full | CSV / Parquet output |
| MFA Delete | ✅ Full | RFC 6238 TOTP |
| SSE-S3 (server-side, S4-managed keys) | ✅ Full | AES-256-GCM (S4E1/S4E2 wire) |
| SSE-KMS (envelope encryption) | ✅ Full | LocalKms (file-backed KEKs) default; AWS KMS gated behind `aws-kms` feature |
| SSE-C (customer-provided key) | ✅ Full | (S4E3 wire) |
| S3 Select | ✅ subset | CSV input, single-column equality / inequality / GT / LT / LIKE-prefix; falls back to CPU eval where unsupported |
| Presigned URLs | ✅ Full | both PUT and GET |
| SigV4 / SigV4a auth | ✅ Full | SigV4a requires `--sigv4a-credentials <DIR>` |
| Storage class transitions (Standard ↔ IA ↔ Glacier) | ✅ tagging-driven | see [docs/storage-class-transitions.md](storage-class-transitions.md) |
| Cross-region replication via S4 chain | — | use AWS S3 native CRR on the backend |
| RequestPayment / Accelerate / Logging configuration | — | not implemented; report a 501 |

### Client transparency (compression is invisible to the client)

By default S4 presents every object as if it were stored uncompressed — the
compression is invisible to the S3 client. This is what lets SDKs that validate
upload integrity (AWS SDK v2, OpenSearch `repository-s3`, …) work unchanged.

| Surface | Default (client-transparent) | Opt-out / notes |
|---|---|---|
| **Single-object ETag** (PUT / HEAD / GET) | `MD5(original payload)` — what a client computes from the bytes it uploaded/downloaded | `--physical-passthrough` presents the backend's compressed-object ETag instead |
| **Content-Length & GET body** | the original (decompressed) size / bytes | — |
| **`s4-*` control metadata** | stripped from GET / HEAD (incl. Range GET) responses | use a direct backend read to inspect S4's internal markers |
| **Multipart composite ETag** | AWS `MD5(concat(original-part-MD5s))-N` — **best-effort** (see below) | `--physical-passthrough` keeps the backend composite |
| **`ListObjects(V2)` Size & ETag** | the **original** size + logical ETag, matching HEAD/GET (v1.4.1+; resolved via bounded-concurrency backend HEADs, N+1) | **`--physical-listings`** opts out (backend compressed size + ETag, the v1.4.0 behavior, max list throughput); `--accurate-list-size` remains as a deprecated no-op alias |
| **Write-path `If-Match` / `If-None-Match`** (PUT) and **`x-amz-copy-source-if-*`** (Copy) | evaluated by S4 against the logical ETag | **non-atomic** (HEAD-then-write); a concurrent writer between the check and the write is not serialised — run conditional writes on cold / quiescent keys. A non-404 backend HEAD error fails the write (never silently proceeds) |

**Multipart composite ETag is best-effort.** Computing the AWS composite needs
every part's original-payload MD5, which S4 records in **in-memory** per-upload
state. When the full set is present (single gateway, state survived) the object
gets the client-transparent composite, stamped so HEAD/GET return it. When it is
not — a **gateway restart mid-upload** or a **multi-gateway deployment** where
parts landed on different instances — `CompleteMultipartUpload` still **succeeds**
(parts are reverse-mapped authoritatively via the backend's `ListParts` by part
number), but the object keeps the **backend** composite ETag with no logical
stamp, and HEAD / GET / List return that same value consistently. In that
recovery path S4 does not strictly re-validate the client's submitted part ETags
(it assembles by part number); durable per-part state (which would restore both
the composite and strict validation everywhere) is a roadmap item. Use
`--physical-passthrough` if you require uniform backend-ETag behavior.

**Known minor gaps** (low-impact; mostly upstream of S4):
- Whitespace-only object keys (`" "` / `"\t"` / `"\n"`) return `500` — emitted by
  the underlying `s3s` framework before S4's handler runs.
- Error responses carry no `x-amz-request-id`, and `304 Not Modified` omits the
  `ETag` header — both are `s3s` response-shape limitations.
- `GetObjectAttributes(ObjectSize)` returns the backend `400` and is not
  client-transparent — use `HeadObject` (which is) instead.
- `CopyObject` of a **URL-encoded key on a specific source `versionId`** can
  return `NoSuchKey`; copy by the unencoded key / latest version works.
- Browser-based `POST` object-upload **policy** violations return `400` where AWS
  returns `403`/`204` in a few cases (the happy path works).

> Measured against the Ceph `s3-tests` conformance suite, the campaign cut
> S4-introduced regressions from **21 → 11** (vs MinIO-direct, N=784); the
> remaining 11 are the gaps listed above.

**Range GET caveat** (#99): the S4IX sidecar gives a per-frame index, so
range maps to a contiguous read of the covering frames and a decode that's
sliced at the boundaries the caller asked for. Parquet/ORC readers
(arrow-rs, datafusion, duckdb's parquet reader) that issue suffix-range
GET against the footer work out of the box. Parallel range reads against
overlapping frame extents do extra decode work and are not yet optimized;
see #99 for the parquet/ORC reader cross-validation harness on the
roadmap.

### SDK compatibility matrix

Test status per major S3 client. "Tested" means a green E2E run in CI or
documented manual verification; "Should work" means the wire shape is
satisfied but no explicit test covers it yet; "Known issue" links to the
relevant issue.

| Client | Status | Notes |
|---|---|---|
| `aws-cli` (v2.x) | ✅ Tested | path-style + virtual-hosted URLs, presigned URLs, multipart, range GET |
| `boto3` (Python) | ✅ Tested | via `s4-codec-py` integration tests + `tests/test_binding.py` |
| `aws-sdk-rust` (v1.x) | ✅ Tested | the gateway is built on it; trait-level coverage in `tests/feature_e2e.rs` |
| `aws-sdk-go-v2` | ✅ Should work | wire-level shapes shared with aws-sdk-rust; no explicit smoke test yet |
| `aws-sdk-java-v2` | ✅ Should work | same as Go v2 caveat |
| `MinIO mc` | ✅ Should work | path-style + virtual-hosted both fine; one-off `mc cp` validated manually |
| `rclone` (s3 backend) | ✅ Should work | multipart chunk size driven by client; large objects respect S4 frame budget |
| `s3cmd` | ⚠️ Should work | older client; SigV2 fallback NOT supported (S4 is SigV4 + SigV4a only) |
| Presigned URLs (SigV4) | ✅ Tested | both PUT and GET; query-string signing path covered |
| Conditional GET / PUT | ✅ Tested | `If-Match` / `If-None-Match` / `If-Modified-Since` / `If-Unmodified-Since` |
| `Content-MD5` / `x-amz-content-sha256` | ✅ Tested | both unsigned (`UNSIGNED-PAYLOAD`) and SHA256-hashed payloads |
| `Content-Encoding: gzip` interplay | ⚠️ See note | S4 may double-encode if the client sends `Content-Encoding: gzip` AND S4 also picks `cpu-gzip` — use `--codec cpu-zstd` or set client `Content-Encoding: identity` |

**Endpoint URL style** (#101): S4 accepts both **virtual-hosted-style**
(`https://my-bucket.s4.example.com/key`) and **path-style**
(`https://s4.example.com/my-bucket/key`); the backend ` aws-sdk-s3 `
client uses whatever the operator's `--endpoint-url` configuration
specifies. If your client is fussy about this, set `--path-style` on
the s4 server side or `--force-path-style` on the AWS SDK side.

### Backend compatibility matrix

S4 is a transparent compression proxy in front of an S3-compatible
backend. Each row below is the **verification posture** S4 holds for
that backend — what CI actually exercises, not "should work" claims.
v0.11 #A7 added the weekly
[`compat-matrix.yml`](../.github/workflows/compat-matrix.yml) workflow
that drives the docker-tier verifications (and the real-cloud rows
when operators provide credentials).

| Backend | Verification | Notes |
|---|---|---|
| [AWS S3](https://aws.amazon.com/s3/) | ⚠️ Opt-in nightly CI ([`aws-e2e.yml`](../.github/workflows/aws-e2e.yml); gates only when `AWS_E2E_*` secrets are configured on the fork; this upstream repo has them unset) | real bucket, OIDC-assumed IAM role; the reference implementation when a fork wires the secrets |
| [MinIO](https://github.com/minio/minio) | ✅ Verified via per-PR CI (`http_e2e` / `multipart_e2e` testcontainers) + weekly compat-matrix | `quay.io/minio/minio:latest` |
| [Garage](https://git.deuxfleurs.fr/Deuxfleurs/garage) | ⚠️ Provisioning verified weekly via compat-matrix CI (docker `dxflrs/garage:v1.1.0`); round-trip is `continue-on-error` due to `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` signature drift between `aws-sdk-rust` and garage v1.1.0 | single-node `replication_mode = "none"`, CLI-provisioned bucket + key. Excluded from the v1.x freeze gate (see [stability.md](stability.md#backend-compatibility-matrix-ci-verified-surface)). |
| [Ceph RGW](https://docs.ceph.com/en/latest/radosgw/) | ⚠️ Best-effort weekly compat-matrix CI (`quay.io/ceph/demo:latest-quincy`) | the upstream `ceph/demo` image is no longer actively maintained; **both** the start step and the round-trip step are gated `continue-on-error` so pull / startup / wire-shape drift failures surface as warnings rather than blocking the matrix |
| [Backblaze B2](https://www.backblaze.com/b2/cloud-storage.html) | 🔧 Configurable in operator CI (real backend; requires `vars.B2_BUCKET` / `B2_ENDPOINT` / `B2_REGION` + `secrets.B2_KEY_ID` / `B2_APPLICATION_KEY`) | weekly when configured, silent skip otherwise |
| [Cloudflare R2](https://www.cloudflare.com/products/r2/) | 🔧 Configurable in operator CI (real backend; requires `vars.R2_BUCKET` / `R2_ENDPOINT` / `R2_REGION` + `secrets.R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY`) | weekly when configured, silent skip otherwise |
| [Wasabi](https://wasabi.com/) | 🔧 Configurable in operator CI (real backend; requires `vars.WASABI_BUCKET` / `WASABI_ENDPOINT` / `WASABI_REGION` + `secrets.WASABI_ACCESS_KEY_ID` / `WASABI_SECRET_ACCESS_KEY`) | weekly when configured, silent skip otherwise |

Per-provider cost math + a pre-production validation checklist for
these backends: [use-cases/s3-compatible-backends.md](use-cases/s3-compatible-backends.md).

Each compat-matrix job runs a 1 PUT + 1 GET + sidecar HEAD against
the live backend through an `s4 --codec cpu-zstd --dispatcher always`
server — sidecar HEAD on the backend asserts the second backend round-
trip (sidecar PUT) lands the way s4 expects, which is where most
S3-API-shape divergences would surface (PutObject without
`Content-MD5`, aws-chunked encoding, etc.).

### S3 API coverage (45+ ops)
- Compression hook: `put_object`, `get_object`, `upload_part`
- Range GET: full S3 spec (`bytes=N-M`, `bytes=-N`, `bytes=N-`)
- Multipart: `create_multipart_upload`, `upload_part`, `complete_multipart_upload`, `abort_multipart_upload`, `list_parts`, `list_multipart_uploads`
- Phase 2 delegations (passthrough): ACL, Tagging, Lifecycle, Versioning, Replication, CORS, Encryption, Logging, Notification, Website, Object Lock, Public Access Block, ...
- Hidden: `*.s4index` sidecars are filtered from `list_objects[_v2]` responses
