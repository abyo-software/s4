# Project status

> **Status: v1.0 — stable surface, no public production deployment
> reference yet.** v1.0 is the SemVer-stable freeze of the wire formats,
> library API surface, CLI subcommands, `s3s 0.13` HTTP trait set, and
> Helm `values.yaml` key shape enumerated in the [stability contract](stability.md).
> It is *not* a marketing claim that "S4 has been battle-tested
> at every Fortune 500." The freeze means downstream consumers can pin
> `s4-server = "1"` (or `s4-codec = "1"`, or `s4-config = "1"` in a
> `Cargo.toml`; or `ghcr.io/abyo-software/s4:1` for the container) and
> rely on the surface not changing
> under them; first public production deployment references are still
> being collected. If you're putting S4 into a TB-scale workload, please
> file an issue tagged `production-reference` so we can list your
> deployment alongside the audit + fuzz evidence below.

- **Release line:** [CHANGELOG.md](../CHANGELOG.md) has the full
  per-version history; the GitHub Releases page has the cut-points.
  Cumulative scope through v1.0 is **714+ workspace tests + 14+
  production milestones** covering S3-compatible PUT / GET / multipart
  / Select / SSE-S3 / SSE-KMS / SSE-C / IAM Conditions / bucket
  policy / versioning / object-lock / lifecycle / inventory /
  notifications (Webhook / SQS / SNS) / CORS / tagging / MFA delete /
  SigV4 + SigV4a, plus Python (`s4-codec-py`) and browser
  (`s4-codec-wasm`) bindings, all on crates.io as the
  [`s4-server`](https://crates.io/crates/s4-server) /
  [`s4-codec`](https://crates.io/crates/s4-codec) /
  [`s4-config`](https://crates.io/crates/s4-config) trio. **Cross-region
  replication** ships as experimental scaffolding (config surface + wire
  stub) and is intentionally **excluded from the v1.0 freeze** — promotion
  to production-grade is on the v1.x roadmap.
- **Audit history:** three rounds of deep audit (`第一弾` / `第二弾` /
  `第三弾`) closed in v0.8.2 → v0.8.5; pre-launch audit (claude + codex
  cross-review, tracker #111) in v0.8.7 → v0.8.8; integrated audit
  rounds R1–R6 across v0.9 / v0.10 / v0.11 cuts; v1.0 readiness audit
  (Opus + Codex adversarial review) drove 13 surfaced findings to
  closure — including the v1.0 stability section in this README, the
  `#[non_exhaustive]` annotations on every public enum, gating
  test-only helpers out of the public API contract, and qualifying the
  backend compatibility matrix in the [compatibility matrices](compatibility.md). Findings spanned CRITICAL
  pre-auth state-machine bugs, HTTP wire hardening, GPU codec safety,
  binding correctness, background-task lifecycle, README claim
  accuracy, and v1.0 freeze surface completeness. CVE clean
  (`cargo audit`, see CI `security-audit` job); 1 advisory accepted
  as risk-with-mitigation per
  [`security/cargo-audit-ignores.md`](security/cargo-audit-ignores.md)
  (down from 4 — the rustls-webpki trio was resolved 2026-07-06 by
  dropping the legacy rustls 0.21 chain, issue #91).
- **Continuous fuzz farm** (since v0.8.6) — 7 bolero targets running 24/7
  under a `systemd-user` slice budgeted at 8 cores / 30 GiB (1/4 of the
  build host). Coverage compounds across `Restart=always` wakeups; any
  crash auto-files a GitHub issue (label `fuzz-crash`, deduped by SHA1
  of the input). First catch: **#89** (CpuZstd / CpuGzip
  alloc-before-validate) found within seconds, fixed and shipped
  same-day in v0.8.6.
- **Real-GPU validation** done on RTX 4070 Ti SUPER + nvCOMP 5.x:
  streaming zstd 1 GiB roundtrip + GDeflate roundtrip both green; OMB
  bench runs on EC2 c7gd.8xlarge (latest v0.8 perf chart at
  `perf-v0.8.png`).
- **Suitable for** log archival, data lake / parquet/ORC analytics,
  drop-in transparent-compression proxy in front of any S3-compatible
  backend. The v1.0 surface freeze means you can integrate against a
  stable contract; the "no public production reference yet" caveat
  means we still recommend pairing with backend-native replication /
  versioning for irreplaceable data until at least one production
  reference is published.
- **Roadmap is driven by audit findings + continuous fuzz** rather than
  feature checklists; file issues at
  https://github.com/abyo-software/s4/issues to influence it.
