# S4 — Squished S3

GPU 透過圧縮の AWS S3 互換ストレージゲートウェイ。

## Status

**v0.2.0 release 済** (2026-05-12)。crates.io publish: `s4-server` /
`s4-codec` / `s4-config` 全 0.2.0、`cargo install s4-server` で `s4`
binary 即動可能。`docker compose up -d` で MinIO + S4 60 秒 trial。
詳細は repo root の [CHANGELOG.md](CHANGELOG.md) と
https://github.com/abyo-software/s4/releases/tag/v0.2.0 参照。

## アーキテクチャ

```
client (aws-sdk / boto3 / aws-cli) ──[S3 API]──> S4 server (hyper)
                                                  │
                                                  ├─ s4-codec::CodecRegistry
                                                  │   ├─ Passthrough
                                                  │   ├─ CpuZstd (zstd-rs)
                                                  │   ├─ NvcompZstd (nvCOMP, GPU)
                                                  │   └─ NvcompBitcomp (nvCOMP, GPU)
                                                  │
                                                  ├─ s4-codec::CodecDispatcher
                                                  │   ├─ AlwaysDispatcher
                                                  │   └─ SamplingDispatcher (entropy + magic-bytes)
                                                  │
                                                  └─ s3s_aws::Proxy ──[S3 API]──> AWS S3 / MinIO
```

PUT 経路: client が送ってきた body を sample → dispatcher が codec を選択 →
registry が圧縮 → S3 metadata (`s4-codec` / `s4-original-size` / `s4-compressed-size`
/ `s4-crc32c`) に manifest を書いて backend に forward。

GET 経路: backend から取得 → metadata の manifest から codec 復元 → registry が
解凍して元バイト列を return。`s4-multipart=true` のオブジェクトは frame parser
で chunk 列を解凍 → 連結。

## ビルド

```bash
# CPU 専用 (default)
cargo build --workspace --release

# GPU 有効化 (CUDA toolchain + nvCOMP SDK 必須)
export NVCOMP_HOME=/path/to/nvcomp-linux-x86_64-5.x.x_cuda12-archive
cargo build --workspace --release --features s4-server/nvcomp-gpu
```

要件:
- Rust 1.92+ (workspace 全体)、edition 2024 (vendored ferro-compress は 2021)
- CUDA 12+ (`nvcc`) と nvCOMP redist tarball (GPU codec を使う場合のみ)

## CLI

```bash
# CPU zstd で起動 (default)
target/release/s4 \
    --endpoint-url https://s3.us-east-1.amazonaws.com \
    --host 0.0.0.0 --port 8014

# GPU codec を選ぶ (要 --features nvcomp-gpu でビルド)
target/release/s4 \
    --endpoint-url https://s3.us-east-1.amazonaws.com \
    --codec nvcomp-zstd \
    --dispatcher sampling

# クライアント側
aws --endpoint-url http://localhost:8014 s3 cp foo.log s3://my-bucket/foo.log
aws --endpoint-url http://localhost:8014 s3 cp s3://my-bucket/foo.log -
```

`--dispatcher sampling` を選ぶと PUT body 先頭 4 KiB を sampling し、既圧縮
データ (gzip / zstd / png / jpeg / mp4 / zip / pdf / 7z / xz / bzip2 / webm /
webp) や 高 entropy (≥ 7.5 bits/byte) の bytes を **passthrough** で素通し
させ、無駄な再圧縮で膨張させないようにする。

## サポート S3 op

- 圧縮 hook あり: `put_object` / `get_object` / `upload_part` (multipart)
- delegation のみ (圧縮なし): `head_*`, `list_*`, `create_*`, `delete_*`,
  `copy_object`, `complete_multipart_upload`, `abort_multipart_upload`,
  `*_object_acl` / `*_object_tagging` / `*_object_attributes` /
  `*_object_lock_*` / `*_object_legal_hold` / `*_object_retention` /
  `list_object_versions` / `*_bucket_*` (versioning, location, policy, ACL,
  CORS, lifecycle, tagging, encryption, logging, notification, request_payment,
  website, replication, accelerate, ownership_controls), `*_public_access_block`
- その他 (Analytics / Inventory / Intelligent-Tiering / Metrics 等): trait
  default の `NotImplemented` を返す。今後の release で需要に応じて追加

## ベンチマーク (実 GPU 計測)

RTX 4070 Ti SUPER + nvCOMP 5.x + Ryzen 9 9950X、`s4-codec` を直接 single-pass
roundtrip。throughput は **uncompressed bytes / sec** (nvCOMP / lz4 / zstd 公称
慣習)。

| Workload | Codec | Original | Compressed | Ratio | Compress | Decompress |
|---|---|---:|---:|---:|---:|---:|
| nginx access log (256 MiB) | cpu-zstd-3 | 256 MiB | 1 MiB | **155.01×** | 2.72 GB/s | 2.26 GB/s |
| nginx access log (256 MiB) | nvcomp-zstd | 256 MiB | 2 MiB | 95.60× | 1.27 GB/s | 2.06 GB/s |
| nginx access log (256 MiB) | nvcomp-gdeflate | 256 MiB | 169 MiB | 1.51× | 0.79 GB/s | 1.98 GB/s |
| Parquet 風 mixed (256 MiB) | cpu-zstd-3 | 256 MiB | 133 MiB | 1.92× | 0.55 GB/s | 1.38 GB/s |
| Parquet 風 mixed (256 MiB) | nvcomp-zstd | 256 MiB | 131 MiB | 1.94× | 1.06 GB/s | 2.07 GB/s |
| Parquet 風 mixed (256 MiB) | nvcomp-gdeflate | 256 MiB | 183 MiB | 1.40× | 0.79 GB/s | 1.93 GB/s |
| Parquet 風 mixed (256 MiB) | nvcomp-bitcomp | 256 MiB | 122 MiB | **2.09×** | 1.20 GB/s | 1.08 GB/s |
| Posting list (u32, 64 MiB) | cpu-zstd-3 | 64 MiB | 43 MiB | 1.48× | 1.03 GB/s | 1.47 GB/s |
| Posting list (u32, 64 MiB) | nvcomp-zstd | 64 MiB | 42 MiB | 1.52× | 0.92 GB/s | 2.23 GB/s |
| Posting list (u32, 64 MiB) | nvcomp-gdeflate | 64 MiB | 42 MiB | 1.51× | 0.67 GB/s | 1.97 GB/s |
| Posting list (u32, 64 MiB) | nvcomp-bitcomp | 64 MiB | 5 MiB | **11.93×** | 1.09 GB/s | 1.27 GB/s |
| Timestamp (i64, 64 MiB) | cpu-zstd-3 | 64 MiB | 24 MiB | 2.63× | 0.25 GB/s | 0.79 GB/s |
| Timestamp (i64, 64 MiB) | nvcomp-zstd | 64 MiB | 24 MiB | 2.61× | 0.95 GB/s | 1.97 GB/s |
| Timestamp (i64, 64 MiB) | nvcomp-gdeflate | 64 MiB | 48 MiB | 1.32× | 0.69 GB/s | 1.95 GB/s |
| Timestamp (i64, 64 MiB) | nvcomp-bitcomp | 64 MiB | 21 MiB | **2.95×** | 1.20 GB/s | 1.04 GB/s |
| doc_values (i64, 64 MiB) | cpu-zstd-3 | 64 MiB | 44 MiB | 1.45× | 0.23 GB/s | 0.89 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-zstd | 64 MiB | 34 MiB | **1.86×** | 0.80 GB/s | 2.28 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-gdeflate | 64 MiB | 48 MiB | 1.33× | 0.60 GB/s | 1.88 GB/s |
| doc_values (i64, 64 MiB) | nvcomp-bitcomp | 64 MiB | 37 MiB | 1.72× | 0.85 GB/s | 1.08 GB/s |
| 既圧縮入力 (64 MiB) | cpu-zstd-3 | 64 MiB | 64 MiB | 1.00× | 1.45 GB/s | 2.21 GB/s |
| 既圧縮入力 (64 MiB) | nvcomp-zstd | 64 MiB | 64 MiB | 1.00× | 0.69 GB/s | 1.97 GB/s |
| 既圧縮入力 (64 MiB) | nvcomp-gdeflate | 64 MiB | 64 MiB | 1.00× | 0.62 GB/s | 1.77 GB/s |

**ハイライト**:
- **`nvcomp-bitcomp` posting list u32: 11.93×** — Bitcomp の killer
  use case (sorted u32 doc-id 列)。`cpu-zstd-3` の 1.48× と比べて 8 倍。
- **`nvcomp-bitcomp` timestamps i64: 2.95×** — ms 増分 monotonic 時系列。
- **`cpu-zstd-3` nginx log: 155.01×** — text/log の絶対王者。
- **既圧縮入力**: 全 codec が 1.0× で素通し (= S4 が file を大きくしない)。

## コスト試算 — S4 を入れる価値があるか?

S4 が向いてるかどうかは **(a) 月額 S3 額、(b) データ圧縮率、(c) GPU EC2 instance
コスト** の関数。誠実に自己診断できる表:

| 月額 S3 | 圧縮で削減 (50–80%) | EC2 GPU 月額 | 純削減 | 判定 |
|---:|---:|---:|---:|---|
| $500   | $250 – $400     | ~$730 (g6.xlarge)    | **−$330 〜 −$480**    | ❌ 入れない方がいい |
| $1,000 | $500 – $800     | ~$730                | **−$230 〜 +$70**     | ⚠️ 損益分岐、GPU を別用途でも使うなら |
| $3,000 | $1,500 – $2,400 | ~$730                | **+$770 〜 +$1,670**  | ✅ 確かに節約 |
| $10,000 | $5,000 – $8,000 | ~$1,860 (g6e.xlarge) | **+$3,140 〜 +$6,140** | ✅✅ 強い ROI |
| $50,000 | $25,000 – $40,000 | ~$1,860            | **+$23,140 〜 +$38,140** | ✅✅✅ 大幅節約 |

**注**:
- 「圧縮で削減 50–80%」は log 系 (`cpu-zstd-3` 155×) や Parquet
  (`nvcomp-zstd` ~2× + Range GET 効率化) の典型値。Pure 数値カラム
  + `nvcomp-bitcomp` (sorted posting list 等) では **>10×** = 90%+ saved。
- EC2 価格は us-east-1 on-demand (2026-05)。Spot だと ~70% off → 損益分岐
  ラインが $1,000 から $300 に下がる。
- S4 自体は OSS (Apache-2.0)、コストは EC2 instance + 運用工数のみ。
- **月額 S3 が $1,000 未満で GPU 用途が他にないなら、入れない方がいい**。
  S4 の `cpu-zstd` codec を小さい CPU instance で、 もしくは bucket 前段に
  nginx + gzip を置くだけでもほぼ同じ削減ができる。

**再現方法** (CUDA + nvCOMP 環境):

```bash
NVCOMP_HOME=/opt/nvcomp LD_LIBRARY_PATH=/opt/nvcomp/lib \
  cargo run --release --example bench_codecs \
    -p s4-codec --features nvcomp-gpu
```

MinIO S2 / Garage zstd との head-to-head ベンチは
[issue #14](https://github.com/abyo-software/s4/issues/14) で track 中。

## Production 機能 (v0.2.0 完了状態、~40 commits、140 tests)

### Streaming I/O
- **GET**: CpuZstd / Passthrough / NvcompZstd / NvcompGDeflate の
  non-multipart object は `async-compression` 経由で chunk-by-chunk
  decompress、TTFB は数 ms、memory peak ≈ zstd window + 64 KiB chunk
- **PUT**: input body を `tokio::io::AsyncRead` で encoder に直接 pipe、
  uncompressed input を memory に保持しない。memory peak ≈ compressed output
  size (圧縮率 100x なら 5 GB → 50 MB)
- **GPU streaming compress** (v0.2): nvCOMP `zstd` / `gdeflate` PUT は
  per-chunk pipeline 化 → 10 GB highly-compressible upload で host RAM peak
  ~210 MB (vs naive batch だと full 10 GB)
- **Single-PUT framed format unification** (v0.2): 全圧縮 PUT が S4F2
  multi-frame 形式 + sidecar (`<key>.s4index`) で書かれる。Range GET の
  partial-fetch optimization が single-PUT object にも効く
- **multipart per-part 圧縮**: 各 part 圧縮 + frame 化 (`S4F2` magic)、
  per-frame codec dispatch (mixed codec 対応)
- **Multipart final-part padding trim** (v0.2): 最終 part が tiny かつ
  highly-compressible なら `S4P1` padding を skip (highly-compressible な
  workload で最大 ~5 MiB/object 削減)
- **Range GET via sidecar `<key>.s4index`**: 必要な圧縮 byte range だけ
  backend に partial GET、decompress + slice。sidecar なしなら full read fallback
- **Byte-range aware `upload_part_copy`** (v0.2): source object が S4-framed
  なら user-visible byte range を copy (decompressed + re-framed)、raw 圧縮
  bytes 直 copy ではなく

### Multi-codec dispatch
- **Per-frame codec** (multipart): frame header v2 (`S4F2`) に codec_id を
  含み、part ごとに違う codec を使える。Parquet 風 mixed-content (整数列 →
  Bitcomp、text 列 → zstd) で実効圧縮率 1.5-2× 改善余地
- **Object 横断**: 同一 S4 instance が異なる codec で書かれた object を
  manifest dispatch で透過読込

### Health probes + metrics + traces
- `/health` 常に 200 OK
- `/ready` backend connectivity check (4xx 認証エラーは ready 判定)
- `/metrics` Prometheus text format
  (`s4_requests_total{op,codec,result}`, `s4_bytes_in_total{op,codec}`,
  `s4_bytes_out_total{op,codec}`, `s4_request_latency_seconds{op,codec}`)
- **OpenTelemetry traces** (`--otlp-endpoint http://otel-collector:4317`):
  各 PUT/GET request が `s4.put_object` / `s4.get_object` span として
  Jaeger / Tempo / CloudWatch X-Ray / Grafana Tempo に送信される。
  `--service-name` で resource service.name 設定
- AWS ALB / NLB / k8s readiness probe との直接統合可

### Structured JSON logging (`--log-format json`)
- tracing-subscriber JSON formatter で 1 event = 1 JSON line
- CloudWatch Logs Insights / fluent-bit で直接 ingest 可能
- 各 PUT / GET 完了時に下記 fields を emit:
  - `op`, `bucket`, `key`, `codec`, `bytes_in`, `bytes_out`, `ratio`,
    `latency_ms` (or `setup_latency_ms` for streaming), `path` (streaming/
    buffered), `range`, `ok`

### Data integrity (silent corruption 防止)
- **`copy_object` S4-aware**: source の s4-* metadata を destination に
  必ず preserve。`MetadataDirective::REPLACE` でも上書きされない (= 客が
  custom metadata を指定しても圧縮 metadata は merge back)
- **Manifest crc32c**: PUT 時に元 bytes の CRC32C を frame / metadata に記録、
  GET 時に decompress 後 crc 一致を検証
- **Range GET reject for invalid bounds**: `start > total` で `InvalidRange`

### Wire 互換性 (E2E 検証済)
- SigV4 chunked encoding 対応 (`bytes_to_blob` で content-length 既知)
- 圧縮で変わる content-length / checksum / etag を request/response で適切に
  書き換え (実 MinIO + 実 aws-sdk-s3 で検証)
- HEAD で original_size を返すため client tools が正しく扱える

### Storage class transition (Standard → IA / Glacier)
- 圧縮 object は `<key>` + `<key>.s4index` sidecar の 2 file 構成。S3
  lifecycle rule は両方を **同じ class に揃えて** transition させる必要
  あり。片方だけ Glacier に落ちると Range GET が `InvalidObjectState` で
  落ちる、または sidecar fallback で full read 化して Range 最適化が無効
- 推奨は `"Filter": {}` (bucket 全体) か、`foo/` prefix のように main +
  sidecar 両方を確実にカバーする `Filter.Prefix`。`.s4index` suffix だけ /
  size 閾値だけ等の分離 filter は drift の温床
- 設定例 2 種 (30 日で IA / prefix 単位で 60 日後 Glacier)、anti-pattern
  解説、`head-object` での drift 監査 recipe を
  [docs/storage-class-transitions.md](docs/storage-class-transitions.md) に
  まとめている

## v0.2.0 で完了した項目 (旧 Phase 2.2 計画)

| 項目 | v0.2.0 status |
|------|---------------|
| GPU streaming compress | ✅ ([#1](https://github.com/abyo-software/s4/issues/1)) per-chunk pipeline + S4F2 framed-everywhere 統一 |
| Multipart 最終 part padding | ✅ ([#5](https://github.com/abyo-software/s4/issues/5)) heuristic-based trim (raw < 5 MiB なら padding 省略) |
| upload_part_copy byte-range | ✅ ([#6](https://github.com/abyo-software/s4/issues/6)) framed source は decompress + re-frame、raw passthrough は維持 |
| 単発 PUT への sidecar | ✅ ([#4](https://github.com/abyo-software/s4/issues/4)) S4F2 multi-frame + `<key>.s4index` 統一 |
| HTTPS / TLS termination | ✅ ([#2](https://github.com/abyo-software/s4/issues/2)) `--tls-cert` / `--tls-key`、ALPN h2/http1.1 |
| Bucket policy enforcement | ✅ ([#7](https://github.com/abyo-software/s4/issues/7)) `--policy <path>`、AWS-style JSON、Allow/Deny |
| GDeflate codec | ✅ ([#9](https://github.com/abyo-software/s4/issues/9)) `nvcomp-gdeflate` codec choice、wire id=6 |
| AWS S3 (real) integration tests | ✅ ([#3](https://github.com/abyo-software/s4/issues/3)) Terraform + workflow scaffold |

## v0.3 以降の roadmap

- TLS cert hot-reload on SIGHUP / ACME (Let's Encrypt) opt-in
- GPU streaming の in-flight pipelining (chunk K-1 compress を chunk K の
  PCIe transfer と overlap)
- Bucket policy の full IAM Conditions (`IpAddress`、`StringEquals`、
  `aws:SourceVpc` 等)
- DietGPU codec backend ([#8](https://github.com/abyo-software/s4/issues/8) で
  v0.2 では cost/value 評価で見送り、user need surfacing したら reopen)

要望 / 提案は https://github.com/abyo-software/s4/issues へ

## テスト

```bash
# unit + in-process integration tests (45 tests、~1 sec)
cargo test --workspace

# Docker MinIO 経由の E2E (9 tests、~9 sec、要 docker daemon)
cargo test --workspace -- --ignored --test-threads=1

# GPU E2E (要 NVCOMP_HOME + CUDA-capable GPU、追加 4 tests = 計 13)
export NVCOMP_HOME=/path/to/nvcomp-linux-x86_64-5.x.x_cuda12-archive
export LD_LIBRARY_PATH=$NVCOMP_HOME/lib:$LD_LIBRARY_PATH
cargo test --workspace --features s4-server/nvcomp-gpu -- --ignored
```

### Fuzz testing (3 層構成)

#### 1. proptest (random / structural、stable Rust、CI で常時)

```bash
cargo test --workspace --test fuzz_parsers --test fuzz_server --test fuzz_advanced
PROPTEST_CASES=100000 cargo test --workspace --test fuzz_parsers --test fuzz_server --test fuzz_advanced
```

**38 properties × 100K cases = 3.8M executions in 6 minutes, zero failures**
(stress run 確認済、PROPTEST_CASES=10000 デフォルト + 100K 拡張)。

| ファイル | 責務 | property 数 |
|---|---|---|
| `crates/s4-codec/tests/fuzz_parsers.rs` | 低層 parser DoS 耐性、roundtrip、enum 完全性、zstd bomb 防御 | 19 |
| `crates/s4-server/tests/fuzz_server.rs` | resolve_range overflow、collect_blob、SamplingDispatcher 不変式 | 10 |
| `crates/s4-codec/tests/fuzz_advanced.rs` | mutational (1 byte flip) / multi-frame sequence / **differential** (production parser vs naive reference) / pad+iter | 9 |

特に重要:
- `cpu_zstd_bomb_caps_at_manifest_size` — 1 KB payload + 10 GB 主張 manifest
  でも bounded memory で安全に SizeMismatch (zstd `Decoder + take(limit)` で実装)
- `read_frame_matches_naive_reference` — production の最適化 parser と naive
  reference parser の output が任意 input で完全一致 (差が出れば最適化バグ)
- `frame_iter_with_trailing_garbage_doesnt_lose_prefix` — multi-frame の途中
  以降が garbage でも、それまでの frame は完全に拾えて FrameIter が fused

#### 2. bolero (coverage-guided 候補、stable Rust → nightly で本格)

```bash
# stable で軽く回す (random engine)
cargo test --test fuzz_bolero

# 本格 coverage-guided (要 nightly + cargo-bolero)
cargo install cargo-bolero
cargo bolero test --engine libfuzzer frame_parser_bolero -- -max_total_time=86400
# crash artifact を replay
cargo bolero test --engine libfuzzer frame_parser_bolero -- corpus/<crash-input>
```

7 bolero targets (`crates/s4-codec/tests/fuzz_bolero.rs`)。corpus は
`crates/s4-codec/tests/__fuzz__/<target>/corpus/` に蓄積、libfuzzer で新 branch
を狙う。

#### 3. CI nightly fuzz (自動 6h job)

`.github/workflows/fuzz-nightly.yml` が毎日 03:00 UTC に走る:

- **proptest stress**: PROPTEST_CASES=1M で全 38 property を release build で実行
  (~6h 想定)、`*.proptest-regressions` を artifact として保存
- **bolero coverage-guided**: nightly Rust + libfuzzer で 5 target を 30 min/target
  並列実行 (matrix)、corpus + crash を artifact 保存
- **issue 自動 open**: 失敗時に `fuzz-failure` ラベル付きで GitHub issue 生成、
  reproduce 手順をテンプレ化

#### Fuzz が実バグを検出した履歴

- `FrameIter` 1 byte 入力で **無限 Err ループ** (DoS) → `fused: bool` で修正
- `cpu_zstd::decompress` で **解凍 bomb で OOM** 可能 → `Decoder + take(limit)`
  で hardening、SizeMismatch で安全に reject

#### 「fuzz failure → CI red」の動作保証

| いつ | 何 cases | Failure 時の挙動 |
|---|---|---|
| push / PR (`ci.yml`) | default + **10K cases stress** (~1.3 min) | `cargo test` 非0 exit → CI 赤 → PR merge block + `proptest-regressions` を artifact 保存 |
| nightly 03:00 UTC (`fuzz-nightly.yml`) | **1M cases × 38 properties + bolero libfuzzer 30min × 5 target** | artifact 保存 + `fuzz-failure` ラベル付き **GitHub Issue 自動 open** |

**Regression 永続化**: proptest が見つけた crash input は `*.proptest-regressions`
ファイルに自動保存される。これは `.gitignore` で whitelist (`!**/*.proptest-regressions`)
されているので **commit すれば将来の test run で必ず replay** される (= 一度
塞いだ穴は二度と空けない設計)。

**CI 動作の自己検証** (`fuzz_canary.rs` の 3 test):

- `canary_proptest_does_run`: proptest framework が確実に最低 100 cases 実行した
  ことを `AtomicUsize` で count + assert (silently skipped を防ぐ)
- `canary_known_invariant_holds`: `write_frame` の出力長 = `header + payload`
  という単純不変式を 1024 cases × proptest で検証 (誰かが header size を変えると
  fail = canary 機能)
- `canary_zstd_bomb_protection_active`: cpu_zstd の bomb hardening が有効
  であることを直接検証 (誰かが `Decoder + take(limit)` を revert すると fail)

**Local で「fuzz が本当に CI を落とすか」を verify する手順**:

```bash
# 1. 既知の不変式を意図的に破壊 (例: write_frame で header size を変える、または
#    cpu_zstd の bomb hardening を revert)
# 2. 実行
cargo test --workspace --release --test fuzz_canary
# → assert 失敗、exit code 1
# 3. 修正を revert
git stash pop
```

**GitHub Actions workflow の事前 validation**: GitHub remote 接続後、
manual trigger で軽量 run を実施:

```bash
gh workflow run fuzz-nightly.yml -f cases=1000 -f bolero_minutes=2
```

(`workflow_dispatch` 経由で 5 分以内の smoke run を回せる、permissions 動作
確認用)。

### Soak / load testing

### Soak / load testing

`scripts/soak/run.sh` で 24h+ 持続負荷を投げて memory leak / FD leak / connection
pool 枯渇を検出する harness。Marketplace AMI 出品前の最終 production
validation 用:

```bash
# default 24h, concurrency 16
./scripts/soak/run.sh

# 短い smoke run
DURATION=300 CONCURRENCY=4 ./scripts/soak/run.sh

# カスタム endpoint / bucket
S4_ENDPOINT=http://localhost:8014 BUCKET=my-soak ./scripts/soak/run.sh
```

`/tmp/s4-soak/{date}/` に下記を出力:

- `monitor.csv` — 1 分ごとの RSS / FD count / open conn / VmSize
- `load.log` — PUT/GET 結果ログ
- `summary.txt` — 最終サマリ + leak verdict (`final_rss < 2 × initial_rss` で pass)

### E2E カバレッジ詳細
- `tests/roundtrip.rs`: in-process trait roundtrip (4 tests、CpuZstd + Passthrough
  + multi-codec dispatch + raw object passthrough)
- `tests/minio_e2e.rs`: MinIO container 経由 wire roundtrip (5 tests、CpuZstd +
  NvcompZstd + NvcompBitcomp + SamplingDispatcher with gzip + copy_object
  REPLACE-directive metadata preserve)
- `tests/http_e2e.rs`: 実 hyper server を spawn し aws-sdk-s3 client で叩く
  (5 tests、roundtrip + list_objects + health/ready + /metrics + Range GET)
- `tests/multipart_e2e.rs`: 90 MB multipart upload を full HTTP スタックで
  per-part 圧縮 + frame parser GET (1 test)

## On-the-wire format

### 単発 PUT (`put_object`)

S3 metadata に以下を格納:

| Key | Value |
|-----|-------|
| `s4-codec` | `passthrough` / `cpu-zstd` / `nvcomp-zstd` / `nvcomp-bitcomp` 等 |
| `s4-original-size` | 元バイト数 (decimal string) |
| `s4-compressed-size` | 圧縮後バイト数 (decimal string) |
| `s4-crc32c` | 元 bytes の CRC32C (decimal string) |

object body そのものは backend から見ると圧縮済 raw bytes。`s4-codec` が
存在しない場合は S4 が触っていないオブジェクトとして透過 (raw return)。

### Multipart PUT (`upload_part` × N + `complete_multipart_upload`)

S3 metadata:

| Key | Value |
|-----|-------|
| `s4-multipart` | `true` |
| `s4-codec` | 全 part 共通 codec |

object body は frame の sequence:

```
┌──── frame 1 ────┐┌── padding ──┐┌──── frame 2 ────┐┌── padding ──┐ ...
│ S4F2 + payload  ││ S4P1 + zeros ││ S4F2 + payload  ││ S4P1 + zeros │
```

各 frame (28 byte header):

```
[magic: 4 bytes "S4F2"]
[codec_id: u32 LE]      # per-frame codec dispatch (mixed codec 対応)
[original_size: u64 LE]
[compressed_size: u64 LE]
[crc32c: u32 LE]
[compressed payload: compressed_size bytes]
```

各 padding (12 byte header + zeros):

```
[magic: 4 bytes "S4P1"]
[length: u64 LE]
[zeros: length bytes]
```

GET 時に `FrameIter` が S4F2 → 解凍 / S4P1 → skip でストリーミング parse。
Single-PUT object も v0.2.0 から S4F2 multi-frame で書かれる (`x-amz-meta-s4-framed: true` flag、v0.1 raw-blob は back-compat path)。

## ライセンス

S4 自身: **Apache-2.0** (workspace で `license = "Apache-2.0"`)。

`crates/s4-codec/src/ferro_compress/` は別ライセンス (Apache-2.0 OR MIT、
v0.1.0 で `crates/s4-codec/vendor/ferro-compress/` から物理統合) — abyo
software の姉妹プロジェクト Ferro 由来の nvCOMP Rust binding。配布する
binary は両者の組合せライセンスを honor する必要あり (詳細は repo root の
`NOTICE` 参照)。

nvCOMP 自体は NVIDIA proprietary EULA。`nvcomp-gpu` feature を有効化した
binary を hourly 課金で再配布する場合は NVIDIA Developer Relations
(`nvidia-compute-license-questions@nvidia.com`) の書面確認推奨
(2026-05 nvcomp 商用ライセンス調査結果より)。

## 関連リポジトリ

- vendored: `~/git/ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress/`
  (Apache-2.0 OR MIT、`publish=false`)
