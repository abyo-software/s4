---
name: Bug report
about: Report something that's broken
title: "[bug] "
labels: bug
---

## Summary

<!-- One-sentence description -->

## Reproduction

```
# Minimal commands / config to reproduce
```

## Expected behavior

## Actual behavior

## Environment

- S4 version (commit SHA): `git rev-parse HEAD`
- Build: `cargo build --release` / `--features nvcomp-gpu` / Docker?
- Backend: AWS S3 / MinIO / Cloudian / ...
- OS / kernel: `uname -srm`
- Rust version: `rustc --version`
- Codec / dispatcher: `--codec ... --dispatcher ...`

## Logs

<details>
<summary>relevant logs</summary>

```
RUST_LOG=debug s4 ... 2>&1 | head -200
```

</details>

## Additional context
