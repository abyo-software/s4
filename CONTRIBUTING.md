# Contributing to S4

Thanks for considering a contribution! S4 is a young project and we welcome
issues, bug reports, code, docs, and ideas.

## Code of Conduct

By participating, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing, you agree your contributions will be licensed under
**Apache License 2.0** (the same license as the project). No separate CLA
required — the [Apache 2.0 License header in `LICENSE`](LICENSE) is sufficient
under the [Inbound = Outbound](https://opensource.guide/legal/#which-open-source-license-is-appropriate-for-my-project)
convention.

## Development setup

```bash
git clone https://github.com/abyo-software/s4
cd s4
cargo build --workspace                    # CPU-only build
cargo test --workspace                     # 99 tests, ~3 sec
```

For GPU codecs (optional):

```bash
# Download nvCOMP redist tarball from NVIDIA Developer
export NVCOMP_HOME=/path/to/nvcomp-linux-x86_64-X.X.X.X_cuda12-archive
export LD_LIBRARY_PATH=$NVCOMP_HOME/lib:$LD_LIBRARY_PATH
cargo build --workspace --features s4-server/nvcomp-gpu
cargo test --workspace --features s4-server/nvcomp-gpu -- --ignored
```

For Docker-based E2E (requires Docker daemon running):

```bash
cargo test --workspace -- --ignored --test-threads=1   # 10 E2E tests, ~12 sec
```

## Coding conventions

- Format with `cargo fmt --all` (rustfmt, default settings).
- Lint with `cargo clippy --workspace --all-targets` — must be clean.
- Test with `cargo test --workspace`. Adding a feature? Add a test.
- Adding a parser / decoder? Add a `proptest` property too (see
  `crates/s4-codec/tests/fuzz_*.rs`).
- Comments in Japanese or English are both fine. README and public-facing docs
  should be English (with optional Japanese counterpart `*.ja.md`).

## Commit messages

Conventional-style prefixes encouraged but not required:

- `feat: ...` for new features
- `fix: ...` for bug fixes
- `test: ...` for test-only changes
- `docs: ...` for documentation
- `refactor: ...` for code restructuring without behavior change
- `chore: ...` for tooling, build, deps

One concise sentence summarizing the *why*; longer body for context if useful.

## Pull request process

1. Fork → branch → push → PR against `main`.
2. CI must pass (cargo fmt, clippy, test, 10K-cases proptest stress).
3. Reviewer is `@masumi-ryugo` for now; aim to respond within 1 week.
4. Squash-merge is preferred for small PRs; merge-commit OK for larger work.
5. We may suggest changes; large/contentious changes are best discussed in an
   issue first.

## What we like

- Bug reports with a minimal reproduction.
- Performance benchmarks (criterion-rs preferred).
- Fuzz-target additions (proptest or bolero — see `crates/s4-codec/tests/fuzz_*.rs`).
- Documentation improvements, especially the English README.
- Real-world deployment write-ups → great as blog posts to link from the README.

## What we'll push back on

- Changes that broaden the S3 wire-format we emit (`S4F2` / `S4P1` / `S4IX`)
  without thorough fuzz coverage and a documented migration plan.
- New runtime dependencies without strong justification (we keep `Cargo.lock`
  small).
- Features that lock S4 to a single backend (we want AWS S3 and S3-compatible
  alternatives to keep working interchangeably).

## Open opportunities (v0.3 and beyond)

The big v0.2 items (GPU streaming, single-PUT framed unification,
`upload_part_copy` byte-range, GDeflate codec, AWS-E2E CI) all shipped in
v0.2.0 — see [CHANGELOG.md](CHANGELOG.md). Currently open:

- **TLS cert hot-reload on SIGHUP** — operationally nice for cert rotation
  without a server restart
- **ACME / Let's Encrypt opt-in flag** — `--acme` to skip the manual
  cert-management for public deployments
- **In-flight pipelining for the GPU streaming path** — overlap chunk K-1
  compress with chunk K PCIe transfer (currently sequential per chunk)
- **Full IAM Conditions** in the bucket-policy evaluator (`IpAddress`,
  `StringEquals`, `aws:SourceVpc`, etc.) — v0.2 ships only the core 4
  actions + Resource glob + Principal-by-access-key
- **DietGPU codec backend** — closed in v0.2 ([#8](https://github.com/abyo-software/s4/issues/8));
  reopen if a concrete user need surfaces
- **More streaming GPU codecs** — Bitcomp streaming (currently bytes-API
  only because the FCG1 framing isn't concatenable like zstd)

If any of these interest you, please open an issue first to coordinate scope.
