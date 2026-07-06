# Why trust S4 with your data

S4 sits in your production data path and rewrites your bytes. The right
first question for any evaluator is: **"what happens to my data if this
proxy corrupts something, or if the project disappears?"** This page
consolidates the evidence — the offline decode path, the integrity
mechanisms on the wire path, the verification tooling you can run
yourself, and the honest list of what is *not* covered. Every command
and number here comes from this repo's docs, source, or CI definitions,
with links.

## 1. The escape hatch: reading your data without S4

If you stop the gateway tomorrow — or the project stops tomorrow — your
objects are still standard S3 objects in an **open, documented,
Apache-2.0 format**, and the decoders ship independently of the gateway:

- The stored body is a sequence of self-describing `S4F2` frames
  (28-byte header: magic + codec id + original size + compressed size +
  CRC32C of the original bytes), with the manifest in plain S3 user
  metadata (`x-amz-meta-s4-*`). Full byte layout: [wire-format.md](wire-format.md).
- The frame payload inside each `cpu-zstd` / `cpu-gzip` frame is a
  standard zstd / RFC 1952 gzip stream — the format is reimplementable
  from the spec without any S4 code.
- The wire format is **SemVer-frozen**: "a v1.x reader can read any byte
  stream another v1.x server has written, in either direction"
  ([stability.md](stability.md)).

**Offline decode, no gateway anywhere** (Python bindings of the same
Apache-2.0 [`s4-codec`](../crates/s4-codec) crate the gateway uses):

```bash
pip install s4-codec
# fetch the raw stored bytes straight from the backend (not through S4)
aws s3api get-object --endpoint-url https://s3.example.com \
    --bucket my-bucket --key big.log /tmp/big.log.s4f2
```

```python
from s4_codec import frame_iter, CpuZstd

body = open("/tmp/big.log.s4f2", "rb").read()
codec = CpuZstd()          # for a cpu-zstd object; h["codec"] names each frame's codec
# each frame header carries {codec, original_size, compressed_size, crc32c};
# decompress() re-verifies the CRC32C of the original bytes per frame
original = b"".join(
    codec.decompress(payload, h["original_size"], h["crc32c"])
    for h, payload in frame_iter(body)
)
```

**Dataframe workloads** read S4 objects straight off the backend via the
fsspec filesystem ([python/s4fs](../python/s4fs/README.md) — its README
calls this "the lock-in escape hatch" in as many words):

```bash
pip install -e python/s4fs[s3]    # from a source checkout
```

```python
import pandas as pd
opts = {"target_options": {"endpoint_url": "http://backend:9000"}}
df = pd.read_parquet("s4://bucket/data.parquet", storage_options=opts)
```

**Browser / Node** gets the same decode path as a WASM bundle
([`s4-codec-wasm`](../crates/s4-codec-wasm/README.md), exports
`decompressFramed` / `decompressSingle`) — read S4 objects over a
presigned URL with no server in the read path. Shared zstd dictionaries
(`s4 train-dict`) are stored as raw zstd dictionary bytes at
`.s4dict/<dict-id>` in the bucket, decodable with stock
`zstd -D <dictfile> -d`.

Honest caveats on the escape hatch:

- The pip / WASM / s4fs decoders cover the CPU codec subset
  (`passthrough`, `cpu-zstd`, `cpu-gzip`; `cpu-zstd-dict` in the Python
  paths). GPU-codec (`nvcomp-*`) frames and SSE-encrypted objects
  **raise a hard error rather than decode wrong**; decode those through
  the `s4` binary — which is itself Apache-2.0, so the worst case is
  "run the open-source gateway once to drain", not vendor lock-in.
  SSE keys are operator-held (`--sse-s4-key` etc.), never S4-hosted.
- Objects that never went through the gateway are untouched — s4fs and
  the gateway both pass them through byte-for-byte.

## 2. How byte-identity is protected by design

The failure the design centers on is *silent* corruption — S4 is built
to fail loudly rather than return wrong bytes.

**On PUT (before anything is committed):**

- Client-supplied checksums are verified against the received body —
  all six AWS algorithms (`Content-MD5`,
  `x-amz-checksum-{crc32, crc32c, sha1, sha256, crc64nvme}`), including
  SigV4-streaming trailer checksums. Mismatch → `400 BadDigest`, nothing
  stored. Which paths verify in-stream vs buffered is documented
  per-shape in [security/streaming-checksum-coverage.md](security/streaming-checksum-coverage.md).
- `Content-Length` over/under-declaration surfaces mid-flight as a 400
  (`TruncatedStream` / `OverlengthStream`) — no partial object is
  created ([security/threat-model.md](security/threat-model.md) §1,
  [ops/repair.md](ops/repair.md) failure-mode table).
- Every frame S4 writes stamps a **CRC32C computed over the original
  (decompressed) payload** into both the frame header and the `.s4index`
  sidecar entry ([ops/repair.md](ops/repair.md) §CRC scope).

**On GET:**

- The per-frame CRC32C is recomputed on decode. A mismatch (backend
  bit-flip, codec bug, forged body) returns a 500 with a diagnostic —
  "S4 won't return corrupted bytes" ([ops/repair.md](ops/repair.md)).
  The same check runs in the offline Python decoder (raises
  `S4CrcMismatchError`).
- The default **ETag is `MD5(original payload)`** on PUT response, HEAD,
  and GET — so any client that validates `ETag == MD5(bytes I
  uploaded/downloaded)` (AWS SDK v2 upload integrity, OpenSearch
  `repository-s3`, …) is doing an independent end-to-end check *through*
  S4 on every transfer ([compatibility.md § Client transparency](compatibility.md#client-transparency-compression-is-invisible-to-the-client)).
  `Content-Length` and listings likewise present original sizes by
  default (listings since v1.4.1; `--physical-listings` /
  `--physical-passthrough` opt out — see `s4 --help`).

**Write protocol / blast radius:**

- A PUT is: main object PUT (**the commit point**) → sidecar PUT. The
  `.s4index` sidecar is a Range-GET optimization only — it is
  **rebuildable from the main object** and its loss degrades performance,
  never correctness ([ops/repair.md](ops/repair.md) §Write protocol).
- The offline maintenance writers (`s4 migrate`, `s4 recompact`) run a
  **mandatory decompress-roundtrip byte comparison before every write,
  with no off switch**, plus a pre-PUT HEAD ETag check, and are dry-run
  by default (subcommand docs in
  [crates/s4-server/src/main.rs](../crates/s4-server/src/main.rs)).

What the CRC does **not** catch is listed just as explicitly in
[ops/repair.md](ops/repair.md) §CRC scope: it verifies the bytes match
what was encoded, not that a writer with backend credentials was
legitimate (that's what SigV4 auth on the PUT side covers).

## 3. Verification tooling you can run yourself

Four `s4` subcommands exist so an operator can audit S4's on-backend
state without trusting this page. All sidecar tooling points at the
**backend** endpoint, not the gateway (the gateway hides `.s4index`
from listings by design). Exact semantics: [ops/repair.md](ops/repair.md)
and the subcommand docs in
[crates/s4-server/src/main.rs](../crates/s4-server/src/main.rs).

```bash
# Read-only: is <key>.s4index intact, missing, or stale vs the live object?
# Checks the sidecar's embedded source ETag + compressed-size binding against
# a live HEAD; deep-scans the body's frames when the sidecar is absent.
# Exit 0 on Ok / LegacyV1 / MissingHarmless / MissingUnknown;
# exit 1 on MissingDivergent / StaleEtag / StaleSize / DecodeError /
# EncryptedSidecarUnsupported.
s4 verify-sidecar bucket/key --endpoint-url https://s3.example.com

# Rebuild a sidecar by re-scanning the main object's frames. Overwrites
# stale/corrupt sidecars. Body capped at 5 GiB by default (--max-body-bytes).
s4 repair-sidecar bucket/key --endpoint-url https://s3.example.com

# Bucket-wide: find dangling .s4index whose paired key is missing or whose
# embedded ETag/size disagrees with the live HEAD. Dry-run by default
# (exit 1 when any orphan is found — cron-able); --delete removes pair-bound
# orphans; undecodable entries need an explicit --delete-undecodable.
s4 sweep-orphan-sidecars bucket --endpoint-url https://s3.example.com [--delete]

# Walk an access-log file written with --access-log + --audit-log-hmac-key,
# recompute each line's HMAC-SHA256, and report the first chain break (or
# "OK"). Key spec: raw:<bytes> | hex:<hex> | base64:<b64>. Cross-file chain:
# --expected-prev-tail <HEX>; truncation detection: --require-eof-hmac.
s4 verify-audit-log access.log --hmac-key hex:<hex-key>
```

`verify-sidecar` / `repair-sidecar` / `sweep-orphan-sidecars` /
`verify-audit-log` are part of the frozen v1.x CLI surface
([stability.md](stability.md)) — they can't be removed in a minor
release. For a first-contact byte-equality check, the README's
[60-second local quickstart](../README.md#try-it-locally-60-seconds-cpu-only)
round-trips an object through S4 + MinIO and compares the files.

## 4. The testing evidence

Numbers below are the ones published in this repo (linked), or
reproducible with the command shown.

**Conformance.** Against the Ceph `s3-tests` suite (N=784 tests), S4
introduces **11 regressions vs MinIO-direct** (down from 21 before the
client-transparency campaign); the remaining gaps are listed in
[compatibility.md § Known minor gaps](compatibility.md#client-transparency-compression-is-invisible-to-the-client).

**Per-push CI** ([ci.yml](../.github/workflows/ci.yml)): `cargo fmt` +
`clippy` + workspace tests, a 10K-cases-per-property proptest stress
run, **E2E against a real MinIO container** (full HTTP wire + SigV4 +
multipart), an i686 32-bit build/runtime smoke, coverage
(`cargo llvm-cov` → Codecov), and `cargo audit` as a **hard
merge-block** (1 accepted advisory, with rationale + mitigation
+ upstream tracking in
[security/cargo-audit-ignores.md](security/cargo-audit-ignores.md)).
PRs additionally get coverage-guided fuzzing via
[ClusterFuzzLite](../.github/workflows/cflite_pr.yml).

**Nightly / weekly CI:**

- [fuzz-nightly.yml](../.github/workflows/fuzz-nightly.yml) — proptest
  at 1M cases per property, plus a libfuzzer matrix over 5 of the
  bolero fuzz targets (30 min each); crashes auto-file issues with the
  repro input.
- [aws-e2e.yml](../.github/workflows/aws-e2e.yml) — E2E against a
  **real AWS S3 bucket** (OIDC-assumed role; single-PUT / multipart /
  Range GET). Honest note: it gates only on forks that configure the
  `AWS_E2E_*` secrets — this upstream repo has them unset
  ([compatibility.md § Backend compatibility matrix](compatibility.md#backend-compatibility-matrix)).
  [aws-kms-e2e.yml](../.github/workflows/aws-kms-e2e.yml) is the
  SSE-KMS sibling.
- [compat-matrix.yml](../.github/workflows/compat-matrix.yml) — weekly
  PUT/GET/sidecar round-trips against MinIO / Garage / Ceph RGW
  containers, plus real B2 / R2 / Wasabi when an operator configures
  credentials. Per-backend verification posture (including which rows
  are `continue-on-error`) is in the matrix, not glossed.

**Fuzzing.** 7 coverage-guided bolero targets over the format parsers
and codecs (count them: `ls crates/s4-codec/tests/__fuzz__/`) — 5 run
in the nightly CI matrix, and all 7 run continuously on a maintainer
fuzz farm ([status.md](status.md)). The fuzz infrastructure has caught and
same-day-fixed real bugs — a `FrameIter` infinite-loop DoS on 1-byte
input and an attacker-controlled decompression-bomb OOM
([testing.md](testing.md)) — which is the point: the parsers that face
your bytes are the most-fuzzed code in the project.

**Volume.** The full tier-by-tier matrix (unit / chaos fault-injection /
proptest / E2E / soak) with pass counts lives in
[testing.md](testing.md); cumulative scope through v1.0 is 714+
workspace tests ([status.md](status.md)). Release cuts additionally go
through multi-round adversarial audits (two independent reviewers);
history in [status.md](status.md).

## 5. Honest limitations

Read these before the evidence above — they are the current edges of
the guarantees, maintained in their linked source-of-truth docs (not
duplicated here, so this page can't silently drift from them):

- **S4 is not a complete S3 implementation.** The op-by-op matrix and
  the 11 known conformance gaps: [compatibility.md](compatibility.md).
- **Write-path conditional requests are non-atomic.** `If-Match` /
  `If-None-Match` on PUT and `x-amz-copy-source-if-*` on Copy are
  evaluated HEAD-then-write; a concurrent writer between check and
  write is not serialized. Run conditional writes on quiescent keys
  ([compatibility.md § Client transparency](compatibility.md#client-transparency-compression-is-invisible-to-the-client)).
- **Multipart composite ETag is best-effort.** Per-part state is held
  in memory today: a gateway restart mid-upload or a multi-gateway
  deployment completes correctly (parts reverse-mapped via the
  backend's `ListParts`) but keeps the backend composite ETag; durable
  per-part state is a tracked roadmap item. Current behavior:
  [compatibility.md § Client transparency](compatibility.md#client-transparency-compression-is-invisible-to-the-client)
  — treat that section as the source of truth as this evolves.
- **Range GET on most SSE modes is a buffered full-body
  decrypt** — an AEAD algorithm-level constraint, not deferred
  plumbing; only SSE-S4 chunked (`S4E6`) has the partial-fetch
  fast-path ([security/sse-partial-fetch-constraint.md](security/sse-partial-fetch-constraint.md)).
- **Streaming checksum verify covers the streaming single-PUT path;**
  other shapes verify buffered — same checksums, different memory
  profile ([security/streaming-checksum-coverage.md](security/streaming-checksum-coverage.md)).
- **Cross-region replication is experimental scaffolding**, excluded
  from the v1.x freeze ([stability.md](stability.md)).
- **No public production deployment reference yet.** For TB-scale or
  irreplaceable data, pair S4 with backend-native versioning +
  replication, and start on a test prefix
  ([README](../README.md#stability--status), [status.md](status.md)).
- **Known residual security risks** are itemized (not hidden) in
  [security/threat-model.md § Known residual risks](security/threat-model.md#known-residual-risks).

## 6. Recovery runbooks

When something does go wrong, the procedures are already written:

| Scenario | Runbook |
|---|---|
| Failure modes on the write path (mid-PUT disconnect, sidecar PUT failure, backend corruption, diverged sidecar) — symptom → recovery table | [ops/repair.md](ops/repair.md) |
| Orphaned `.s4index` sidecars (incl. the v0.8.15 versioned-multipart window) | [orphan-sidecar-recovery.md](orphan-sidecar-recovery.md) |
| Operational incidents: disk full, GPU OOM, backend 5xx storm, SSE key rotation/compromise, KMS KEK loss, MFA secret loss, TLS rotation, graceful shutdown — each as Symptom → Diagnose → Mitigate → Recover → Prevent | [ops/runbook.md](ops/runbook.md) |
| Security boundaries and what S4 does *not* defend against | [security/overview.md](security/overview.md) · [security/threat-model.md](security/threat-model.md) |

---

If you find a gap between this page and the repo's actual behavior,
that's a bug in this page — please
[file an issue](https://github.com/abyo-software/s4/issues). Suspected
vulnerabilities go through [SECURITY.md](../SECURITY.md) instead.
