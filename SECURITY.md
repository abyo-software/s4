# Security Policy

## Reporting a vulnerability

S4 handles untrusted byte streams (S3 object bodies, sidecar indices, multipart
frames) and is intended for production deployment in front of paid AWS S3
buckets. We take security seriously and welcome coordinated disclosure.

**Please do not open a public GitHub issue for vulnerabilities.**

Instead, email **security@abyo.net** with:

- A description of the vulnerability
- Steps to reproduce (or a minimal proof-of-concept)
- Affected versions / commits
- Any suggested mitigation

We aim to acknowledge within **3 business days** and provide an initial
assessment within **7 days**. Critical issues will be fixed and disclosed
within **30 days**; lower-severity issues within **90 days**.

We follow a coordinated disclosure model and credit reporters in the security
advisory unless requested otherwise.

## Supported versions

S4 is currently pre-1.0. We provide security fixes for the **latest commit on
`main`**. Once a stable release is cut (1.0+), the policy will expand to cover
the latest minor release line.

## Known hardening

The current codebase has shipped these security-relevant fixes:

| Issue | Class | Mitigation | Discovery |
|---|---|---|---|
| `FrameIter` infinite-loop on 1-byte input | DoS | `fused: bool` flag; iterator stops returning Err after first error | proptest fuzz |
| `cpu_zstd::decompress` zstd bomb | OOM-DoS | `Decoder + take(manifest.original_size + 1024)` caps output regardless of attacker manifest | added defensively + verified by `cpu_zstd_bomb_caps_at_manifest_size` proptest |
| Range GET on S4-managed object before sidecar support | Silent corruption | `InvalidRange` reject (Phase 1) → sidecar frame index (Phase 2.1) | manual review |
| `copy_object` with `MetadataDirective::REPLACE` | Silent corruption | source `s4-*` metadata force-merged into destination | manual review + E2E test |

## Threat model

S4 assumes:

- The **backend S3 bucket** is trusted (you own it / IAM-controlled).
- The **client** is authenticated via SigV4 (handled by upstream `s3s`
  framework).
- The **sidecar `<key>.s4index`** may be tampered with by an attacker who
  has write access to the backend bucket — S4 must still fail safely
  (no OOM, no out-of-bounds reads).
- The **manifest in S3 metadata** may be tampered with — same constraint.
- The **frame headers** in the object body may be tampered with — same.

S4 does **not** currently provide:

- End-to-end encryption (use `SSE-S3` / `SSE-KMS` on the backend bucket)
- Authentication beyond what s3s framework provides (SigV4 single-key via
  `SimpleAuth`; multi-tenant IAM is Phase 3)
- Client-side anti-replay protection (covered by SigV4 timestamp + S3 backend)

## Fuzz infrastructure

S4 runs continuous fuzz testing as part of CI:

- **Per-push CI**: `cargo test` + `PROPTEST_CASES=10000` stress run (~1.3 min)
- **Nightly fuzz**: `PROPTEST_CASES=1000000` (~6 h) + `cargo-bolero` libfuzzer
  coverage-guided fuzz on 5 parser targets (30 min/target matrix)

Crash artifacts are saved as GitHub Actions artifacts and a `fuzz-failure`
labeled issue is opened automatically. See [README.md](README.md#fuzz-が-ci-を-fail-させることの動作保証)
for details.

## See also

- [LICENSE](LICENSE) — Apache-2.0
- [NOTICE](NOTICE) — third-party attributions
- [CONTRIBUTING.md](CONTRIBUTING.md) — development setup
