## Summary

<!-- 1-3 bullet points -->

## Test plan

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets` clean
- [ ] `cargo fmt --all -- --check` clean
- [ ] Added tests for new behavior (proptest if parser/decoder change)
- [ ] (if applicable) `cargo test --workspace -- --ignored` E2E pass on MinIO Docker

## Wire format / API impact

- [ ] No breaking change
- [ ] Breaking change documented in CHANGELOG.md
- [ ] Migration path described

## Performance

<!-- If relevant: criterion benchmark deltas, soak harness results -->
