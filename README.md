# S4 — Squished S3

GPU 透過圧縮の AWS S3 互換ストレージゲートウェイ。

## Status

Phase 1 開発中 (2026-05-12 時点で 8 commits、43 tests pass)。本リポジトリは
未公開。`S4_PROPOSAL.md` (`.gitignore` 対象、git 管理外) に企画書 v0.3 を保管。

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
  default の `NotImplemented` を返す。Phase 2 で逐次追加

## Production 機能 (Phase 1 + 2.0 + 2.1 完了状態、19 commits、67 tests)

### Streaming I/O
- **GET**: CpuZstd / Passthrough の non-multipart object は `async-compression`
  経由で chunk-by-chunk decompress、TTFB は数 ms、memory peak ≈ zstd window
  + 64 KiB chunk
- **PUT**: input body を `tokio::io::AsyncRead` で zstd encoder に直接 pipe、
  uncompressed input を memory に保持しない。memory peak ≈ compressed output
  size (圧縮率 100x なら 5 GB → 50 MB)
- **multipart / GPU codec**: bytes-buffered (per-part) — Phase 2.1 で
  per-frame batch streaming に拡張
- **Range GET**: S4-compressed object に対する `bytes=N-M` / `bytes=-N` /
  `bytes=N-` を full read + decompress + slice で実装、`Content-Range` +
  `206 PARTIAL_CONTENT` を返す。Parquet/ORC reader 互換 (帯域節約は Phase 2.1
  の sidecar frame index で)

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

## 既知の制限事項 / Phase 2.2 計画

| 項目 | 現状 | Phase 2.2 計画 |
|------|------|---------------|
| GPU streaming compress | bytes-buffered (5 GB max) | nvCOMP batched API を per-chunk pipeline に + framed-everywhere 統一 |
| Multipart 最終 part padding | 全 part 一律 5 MiB pad (典型 30 MB+ part では padding 発動せず) | UploadPartCopy で trim、または framed-everywhere で整合 |
| upload_part_copy byte-range | 圧縮 chunk boundary と無関係 | frame index と連動 |
| copy_object S4-aware | metadata は preserve、bytes は backend が複製 | dest object の sidecar 自動再生成 |
| 単発 PUT への sidecar | 未対応 (frame 化していないため) | framed-everywhere 統一で対応 |

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

主要な E2E カバレッジ:
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
│ S4F1 + payload  ││ S4P1 + zeros ││ S4F1 + payload  ││ S4P1 + zeros │
```

各 frame:

```
[magic: 4 bytes "S4F1"]
[original_size: u64 LE]
[compressed_size: u64 LE]
[crc32c: u32 LE]
[compressed payload: compressed_size bytes]
```

各 padding:

```
[magic: 4 bytes "S4P1"]
[length: u64 LE]
[zeros: length bytes]
```

GET 時に `FrameIter` が S4F1 → 解凍 / S4P1 → skip でストリーミング parse。

## ライセンス

S4 自身: Proprietary (workspace で `license = "Proprietary"` 宣言)。

ただし `crates/s4-codec/vendor/ferro-compress/` は別ライセンス (Apache-2.0
OR MIT、`LICENSE` と `NOTICE` を vendor dir に同梱) — abyo software の
姉妹プロジェクト Ferro 由来の nvCOMP Rust binding を vendoring したもの。
配布する binary は両者の組合せライセンスを honor する必要あり。

nvCOMP 自体は NVIDIA proprietary EULA。`nvcomp-gpu` feature を有効化した
binary を hourly 課金で再配布する場合は NVIDIA Developer Relations
(`nvidia-compute-license-questions@nvidia.com`) の書面確認推奨
(2026-05 nvcomp 商用ライセンス調査結果より)。

## 関連リポジトリ

- vendored: `~/git/ferroSearchProjects/ferrosearch-gpu-compress/crates/ferro-compress/`
  (Apache-2.0 OR MIT、`publish=false`)
