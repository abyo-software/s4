# cargo audit — accepted advisory ignores

`cargo audit` is gated as a hard merge-block in CI
(`.github/workflows/ci.yml::security-audit`). The job runs:

```
cargo audit \
  --ignore RUSTSEC-2025-0134
```

Each ignore is an **accepted risk** with the rationale + mitigation
+ upstream-tracking documented here. Removing an ignore from the CI
list MUST happen the moment its upstream fix lands; this doc is the
record of *why* each one is still here.

The titles below match `cargo audit` output exactly; the
"Reachable from S4" line cites the actual dep path verified with
`cargo tree -i <crate>@<version>`.

## Resolved ignores

- **RUSTSEC-2026-0098 / 2026-0099 / 2026-0104** (`rustls-webpki 0.101.7`, three
  CVEs on the legacy rustls 0.21 TLS path pulled transitively by the AWS SDK) —
  **removed 2026-07-06** (issue #91). The legacy path turned out to be
  feature-gated, not hard-wired: the aws-sdk crates' default `rustls` feature
  maps to `aws-smithy-runtime/tls-rustls` (rustls 0.21 / hyper 0.14 legacy
  connector), while TLS is actually served by `default-https-client`
  (rustls 0.23). Setting `default-features = false` on `aws-sdk-s3` (workspace
  + `crates/s3s-aws`), `aws-sdk-kms`, `aws-sdk-sns`, `aws-sdk-sqs` and
  `aws-sdk-marketplacemetering` — keeping their remaining default features —
  drops rustls 0.21 / rustls-webpki 0.101.7 / hyper-rustls 0.24 /
  tokio-rustls 0.24 from `Cargo.lock` entirely. Verified:
  `cargo tree -i rustls-webpki@0.101.7` → no matching package; only
  rustls-webpki 0.103.x remains.

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
