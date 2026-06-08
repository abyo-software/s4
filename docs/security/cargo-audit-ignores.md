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

The titles below match `cargo audit` output exactly; the
"Reachable from S4" line cites the actual dep path verified with
`cargo tree -i <crate>@<version>`.

## RUSTSEC-2026-0098 — Name constraints for URI names were incorrectly accepted

| Field | Value |
|---|---|
| Crate | `rustls-webpki 0.101.7` |
| Severity | Medium (X.509 name-constraint extension parsing bug). |
| Reachable from S4 | Yes, via `s4-server` → `aws-config 1.8.15` → `aws-smithy-runtime 1.11.3` → `aws-smithy-http-client 1.1.13` → `rustls 0.21.12` → `rustls-webpki 0.101.7` (TLS path against backends + KMS). The newer rustls-webpki 0.103.13 is also in the graph via `rustls 0.23.40` (also pulled by aws-smithy-http-client), but the 0.21/0.101.7 path remains because the AWS SDK still depends on it transitively. |
| Why ignored | Bumping rustls-webpki in isolation would require forking `rustls 0.21` (the API broke between 0.21 and 0.22+, and AWS SDK still references the 0.21 path through multiple intermediate crates). Waiting on the AWS SDK to drop the legacy rustls 0.21 path. |
| Mitigation | The vulnerable code path is reached only when validating a server certificate whose URI-typed Subject Alternative Names use name-constraint extensions. S4's outbound TLS is to operator-controlled S3 endpoints + KMS endpoints; cert provenance is controlled by the deployment. Production deployments terminate inbound TLS at an ingress (cert-manager / ALB) that is also operator-controlled. There is no adversary-controlled cert path. |
| Upstream tracking | Open AWS SDK migration issues track the rustls 0.21 → 0.23 cutover. Re-evaluate when `cargo tree -i rustls-webpki@0.101.7` returns no path. |
| Re-evaluate by | Each release cycle: re-run the tree command above; drop the ignore the day the path is gone. |

## RUSTSEC-2026-0099 — Name constraints were accepted for certificates asserting a wildcard name

| Field | Value |
|---|---|
| Crate | `rustls-webpki 0.101.7` |
| Severity | Medium (X.509 name-constraint + wildcard name interaction bug). |
| Reachable from S4 | Same transitive path as RUSTSEC-2026-0098 (rustls 0.21.12). |
| Why ignored | Same AWS SDK transitive blockage as 2026-0098. |
| Mitigation | Same operator-controlled cert provenance argument as 2026-0098 — the wildcard-cert path requires an adversary-controlled certificate authority in the cert chain, which is not part of any S4-shipped deployment topology. |
| Upstream tracking | Same as 2026-0098 (AWS SDK rustls 0.21 → 0.23 migration). |
| Re-evaluate by | Same as 2026-0098. |

## RUSTSEC-2026-0104 — Reachable panic in certificate revocation list parsing

| Field | Value |
|---|---|
| Crate | `rustls-webpki 0.101.7` |
| Severity | Low (panic, not memory unsafety). |
| Reachable from S4 | Same transitive path as 2026-0098 / 2026-0099. |
| Why ignored | Same AWS SDK transitive blockage. |
| Mitigation | S4 does not configure rustls with CRL checking enabled — `rustls 0.21` requires opt-in to CRL validation (via `WebPkiClientVerifier::with_crls`), and `aws-smithy-http-client` does not opt in. The panic path is therefore unreachable in the S4-shipped deployments. |
| Upstream tracking | Same as 2026-0098. |
| Re-evaluate by | Same as 2026-0098. |

## RUSTSEC-2025-0134 — rustls-pemfile is unmaintained

| Field | Value |
|---|---|
| Crate | `rustls-pemfile 2.x` (currently pinned at `2.2.0` per Cargo.lock; declared in `crates/s4-server/Cargo.toml` as `rustls-pemfile = "2"`). |
| Severity | Unmaintained warning (not an exploitable advisory; the crate's API surface still works correctly, but upstream is no longer actively patched). |
| Reachable from S4 | Yes, runtime dependency of `s4-server`. Used in `crates/s4-server/src/tls.rs` at lines 32, 38, 63, 68 to load the PEM-encoded TLS certificate + private key for the HTTPS listener (`--tls-cert` + `--tls-key` flags). NOT dev-only — this is on the production HTTPS startup path. |
| Why ignored | "Unmaintained" is a caretaker advisory, not an exploit notice. The crate's parsing logic has been stable for years; there is no known bug. The replacement is `rustls-pki-types`'s PEM parser, but the migration touches our `tls.rs` and is scoped for v1.x. |
| Mitigation | The TLS cert + key files are operator-controlled (paths passed via `--tls-cert` + `--tls-key`, read at startup). PEM input is therefore trusted operator-controlled material, not adversary-controlled — even a hypothetical parsing bug in rustls-pemfile would not be reachable from an unauthenticated network attacker. The risk window is closer to "operator misconfigures their own TLS material" than a remote vulnerability. |
| Upstream tracking | Issue #92 in this repo (migrate `tls.rs` to `rustls-pki-types`'s PEM API). |
| Re-evaluate by | When the migration to `rustls-pki-types` lands in v1.x. |

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

## Verification

To re-verify the facts in this doc:

```
cargo audit --json | jq '.vulnerabilities.list[].advisory.title, .warnings.unmaintained[].advisory.title'
cargo tree -i rustls-webpki@0.101.7
cargo tree -i rustls-pemfile
grep -n rustls-pemfile crates/s4-server/Cargo.toml
grep -rn 'rustls_pemfile' crates/s4-server/src/
```
