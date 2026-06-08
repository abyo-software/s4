# cargo audit — accepted advisory ignores

`cargo audit` is gated as a hard merge-block in CI
(`.github/workflows/ci.yml::security-audit`). The job runs:

```
cargo audit \
  --ignore RUSTSEC-2026-0098 \
  --ignore RUSTSEC-2026-0099 \
  --ignore RUSTSEC-2026-0104 \
  --ignore RUSTSEC-2025-0134
```

Each ignore is an **accepted risk** with the rationale + mitigation
+ upstream-tracking documented here. Removing an ignore from the CI
list MUST happen the moment its upstream fix lands; this doc is the
record of *why* each one is still here.

## RUSTSEC-2026-0098 — rustls-webpki `Time` constructor panic

| Field | Value |
|---|---|
| Crate | `rustls-webpki = 0.101.x` |
| Severity | Informational (panic on invalid certificate Time) |
| Reachable from S4 | Yes, via `aws-smithy-http-client` → `hyper-rustls 0.24` → `rustls 0.21` → `rustls-webpki 0.101` (TLS path against backends + KMS) |
| Why ignored | Upstream rustls-webpki 0.102+ is the fix, but `hyper-rustls 0.24` (the version aws-smithy-http-client pins) does not bump to it without a major version of the AWS SDK transitively. We cannot bump in isolation. |
| Mitigation | The panic is only triggered by a server presenting a malformed-time certificate; production deployments terminate TLS at a certificate-pinned ingress (cert-manager / ALB), so the panic-able code path is not reached with adversary-controlled certs. The `s4` binary's outbound TLS is to the operator's own S3 backend / KMS endpoint, where cert provenance is operator-controlled. |
| Upstream tracking | `aws/aws-sdk-rust#1xxx` (waiting on smithy-http-client to bump hyper-rustls). |
| Re-evaluate by | Each release cycle: re-run `cargo tree -i rustls-webpki` and check if the version is now 0.102+. Drop the ignore when it is. |

## RUSTSEC-2026-0099 — rustls-webpki PKCS#7 cert chain validation

| Field | Value |
|---|---|
| Crate | `rustls-webpki = 0.101.x` |
| Severity | Informational (relies on path-building bug that does not match the published x509 spec corner case) |
| Reachable from S4 | Yes, same transitive dep path as RUSTSEC-2026-0098. |
| Why ignored | Same upstream blockage as 0098 — `hyper-rustls 0.24` pins `rustls-webpki 0.101`. |
| Mitigation | Same operator-controlled cert provenance as 0098. The path-building bug only matters when the validating side accepts adversary-supplied chain links, which the S4 outbound TLS path does not in any deployment we ship a Helm chart for. |
| Upstream tracking | Same AWS SDK transitive bump as 0098. |
| Re-evaluate by | Same as 0098. |

## RUSTSEC-2026-0104 — rustls-webpki name-constraint validation

| Field | Value |
|---|---|
| Crate | `rustls-webpki = 0.101.x` |
| Severity | Informational. |
| Reachable from S4 | Yes, same transitive dep path as 0098 / 0099. |
| Why ignored | Same upstream blockage. |
| Mitigation | Same. |
| Upstream tracking | Same. |
| Re-evaluate by | Same. |

## RUSTSEC-2025-0134 — rustls-pemfile unmaintained

| Field | Value |
|---|---|
| Crate | `rustls-pemfile = 1.x` (dev-only) |
| Severity | Unmaintained warning (not an exploitable advisory). |
| Reachable from S4 | Yes, but only via `dev-dependencies` (test fixtures that load throwaway PEM material). The released `s4` / `s4-server` binaries do not link `rustls-pemfile`. |
| Why ignored | The dev-only usage is bounded to test code that loads our own throwaway certs; an unmaintained crate in `dev-dependencies` is not a runtime risk. |
| Mitigation | The crate is only invoked from `#[cfg(test)]` paths. |
| Upstream tracking | Issue #92 in this repo. |
| Re-evaluate by | When we cut over the test fixtures to `rustls-pki-types`, drop this ignore. |

## Policy: when to add a new ignore

A new ignore is added **only when all three** hold:

1. The advisory is reachable from S4 only through a transitive
   dependency that we cannot bump without forking an upstream
   maintained by a stable, responsive third party (= AWS SDK,
   rustls workspace, hyper, tokio).
2. The threat model document
   [`docs/security/threat-model.md`](threat-model.md) describes a
   mitigation (typically: the vulnerable code path is unreachable
   in the deployments S4 ships Helm / Docker manifests for).
3. The upstream fix is tracked (GitHub / GitLab issue link).

If any of (1)–(3) does not hold, the advisory **must** trigger a
release-blocker, not an ignore.

## Removing an ignore

When an upstream bump lands:

1. Drop the `--ignore RUSTSEC-XXXX-XXXX` line from
   `.github/workflows/ci.yml::security-audit`.
2. Drop the section from this file.
3. Bump the affected dependency in `Cargo.toml`.
4. Verify `cargo audit` is clean without the ignore.
5. Note the drop in `CHANGELOG.md` under `### Security`.
