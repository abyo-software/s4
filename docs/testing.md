# Testing & validation

| Tier | What runs | Where | Pass count |
|---|---|---|---|
| **Unit + integration** | parsers, registry, blob helpers, S3 trait, policy, TLS | every push (CI) | 1,300+ |
| **Chaos / fault-injection** | mid-stream GET error, HEAD timeout fail-close, concurrent overwrite, SSE keyring rotation, MPU complete failure (deterministic, in-memory) | every push (CI) | 6 |
| **proptest fuzz** | 39 properties × 256–10K cases (push), × 1M (nightly) | every push + nightly | 39 |
| **bolero coverage-guided** | 7 targets, libfuzzer engine | nightly (matrix, 30 min × 5) | 7 |
| **fuzz canary** | proves fuzz framework is alive | every push | 3 |
| **Docker MinIO E2E** | full HTTP wire + SigV4 against real MinIO + multipart + upload_part_copy | every push (CI) | 8 |
| **In-process TLS E2E** | rcgen self-signed cert + tokio-rustls + reqwest h2/h11 | every push | 2 |
| **GPU codec E2E** | real CUDA, nvCOMP zstd / Bitcomp / GDeflate, streaming + bytes API | manual (`--features nvcomp-gpu`) | 5 |
| **Real AWS S3 E2E** | OIDC role + actual S3, single-PUT / multipart / Range GET | nightly (`aws-e2e.yml`, opt-in) | 3 |
| **Soak / load** | 24h sustained load, RSS / FD / connection leak detection | manual (`scripts/soak/run.sh`) | continuous |

**1,355 default tests + 86 ignored (Docker / GPU / AWS env required) =
1,441 test functions** (measured 2026-07-06 on the workspace with
`cargo test --workspace -- --list`; the table above is the tier
structure, not a census), plus a PROPTEST_CASES=10000 stress run on
every push and 1M cases × 39 properties nightly (39M+ fuzz cases).

Two real bugs already caught by fuzz infrastructure:
1. `FrameIter` infinite-loop on 1-byte input (DoS) — fixed with `fused: bool`
2. `cpu_zstd::decompress` could OOM on attacker-controlled manifest claim —
   fixed with `Decoder + take(limit)`

```bash
cargo test --workspace                   # default
cargo test --workspace -- --ignored --test-threads=1   # E2E (Docker required)
PROPTEST_CASES=100000 cargo test --workspace --release --test fuzz_parsers --test fuzz_server --test fuzz_advanced
NVCOMP_HOME=... cargo test --workspace --features s4-server/nvcomp-gpu -- --ignored
./scripts/soak/run.sh                    # 24 h soak (Marketplace pre-release)
```
