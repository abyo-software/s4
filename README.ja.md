# S4 — Squished S3

[![CI](https://github.com/abyo-software/s4/actions/workflows/ci.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/ci.yml)
[![Nightly Fuzz](https://github.com/abyo-software/s4/actions/workflows/fuzz-nightly.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/fuzz-nightly.yml)
[![AWS E2E](https://github.com/abyo-software/s4/actions/workflows/aws-e2e.yml/badge.svg)](https://github.com/abyo-software/s4/actions/workflows/aws-e2e.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org)

## S3 ストレージ料金を 50–80% 削減 — ドロップイン、アプリ側コード変更ゼロ

**S4 (Squished S3)** は、既存の S3 バケットの前段に配置される S3 互換ゲートウェイです。
オブジェクトを入ってくる時に透過的に圧縮し、出ていく時に解凍します。boto3、
aws-cli、Spark、Trino、DuckDB — S3 API を話すものなら何でも S4 に向けるだけで、
アプリは一切変更せず動き続け、バックエンドは **50–80% 少ないバイト数**で保存します。
変わるのは基本的にエンドポイント URL だけです。

```
   your app  ──▶  S4 (compress)  ──▶  AWS S3 (real bucket, fewer bytes)
 (boto3, Spark,        ▲
  Trino, …)            └── GET decompresses; clients see the original bytes
```

- **アプリ変更不要** — 同じ S3 ワイヤプロトコル、SigV4 認証、SDK 呼び出しのまま。変えるのは `--endpoint-url` だけ。(GET は元のバイト列を返し、`HEAD` は保存された圧縮後サイズを報告します。元サイズは `x-amz-meta-s4-original-size` に入ります。)
- **オブジェクト単位のスマートコーデック** — テキスト/ログには CPU zstd、整数/カラムナデータには GPU nvCOMP (Bitcomp/zstd/GDeflate)、既に圧縮済みの入力にはパススルー。GPU はほとんどの場合必要ありません。
- **ロックインなし** — ゲートウェイを止めても、圧縮されたオブジェクト + S4IX サイドカーは S3 ネイティブのまま残り、Apache-2.0 の `s4-codec` CLI / `pip` / WASM でデコードできます。([フォーマット](docs/wire-format.md))
- **フレーム化オブジェクトの Range GET** — サイドカーでインデックス化されたバイト範囲が Parquet/ORC リーダに対応します。一部の SSE / multipart-SSE モードはバッファ付きフォールバックを使用します。

> ☁️ **AWS Marketplace で実行 — AWS による時間課金、アプリ変更なし:**
> **▶ [EKS / ECS / Fargate 上のコンテナ](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e)** — 任意の小型 CPU ノードで動作。ほとんどのワークロードでのデフォルト経路。
> &nbsp;·&nbsp; **[EC2 上の GPU AMI (g4dn / g5 / g6 / g6e)](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i)** — 整数/カラムナデータを高スループットで処理する場合。
>
> オープンソースビルドはローカルテスト用に無料です。Marketplace は AWS の調達、課金、そしてサポート付きの商用経路を追加します。

## なぜチームは S4 を使うのか

| ニーズ | S4 が提供するもの |
|---|---|
| S3 料金が線形に増えるが、データの大半は 3 倍以上圧縮できる | 入ってくる時点で圧縮 — 縮んだバイト数の分だけ支払う |
| アプリ自身はデータを圧縮しない (かつアプリを変更できない) | ワイヤ互換のドロップイン — 変わるのはエンドポイント URL だけ |
| 混在データ (テキスト、JSON、Parquet/ORC、数値カラム) | ディスパッチャが各オブジェクトをサンプリングし、自動で最適なコーデックを選択 |
| 分析でバイト範囲読み込みが必要 | S4IX サイドカーのフレームインデックス経由の `Range` GET (arrow-rs / DuckDB / datafusion 対応) |
| ロックインが心配 | オープンな S4F2/S4IX フォーマット + Apache-2.0 デコーダ — データ読み込みにゲートウェイランタイムは不要 |

**最適なケース:** ログ、JSON、Parquet/ORC、分析アーカイブ、その他の圧縮可能な S3 ワークロード。

## S4 はあなたの料金にとって意味があるか?

**ほぼ確実に GPU は不要です。** `cpu-zstd` コーデックは、一般的なケース (ログ、JSON、
Parquet、混在テキスト) において 50–80% のストレージ削減のほぼ全てを取り込み、小型または
**バースト可能 (t シリーズ)** CPU インスタンスで問題なく動作します。GPU が優位に立つのは
整数/カラムナデータを高スループットで処理する場合だけです。つまり本当の問いは「GPU を
正当化できるほど料金が大きいか?」ではなく、「安価な CPU コンピュートは S3 料金に対して
元が取れるか?」であり、その答えは GPU ありきの捉え方が示すよりもはるかに早く「イエス」に
なります:

| 月額 S3 料金 | 想定削減額 (50–80%) | S4 ホスト (CPU) | 純削減額 | 判定 |
|---:|---:|---:|---:|---|
| $100   | $50 – $80       | ~$30/mo (t3.medium, burstable) | **+$20 to +$50**    | ✅ 小規模でも価値あり |
| $500   | $250 – $400     | ~$60/mo (t3.large / c7g.large) | **+$190 to +$340**  | ✅ 明確な節約 |
| $1,000 | $500 – $800     | ~$120/mo (c7g.xlarge)          | **+$380 to +$680**  | ✅ 明確な節約 |
| $3,000 | $1,500 – $2,400 | ~$120/mo (c7g.xlarge)          | **+$1,380 to +$2,280** | ✅✅ 強い ROI |
| $10,000 | $5,000 – $8,000 | ~$250/mo (c7g.2xlarge)        | **+$4,750 to +$7,750** | ✅✅✅ 大幅な節約 |

- **ストレージのバイト数のみ。** PUT/GET のリクエスト数とエグレスは変わりません (GET は解凍済みのペイロードを配信します)。
- **インスタンスのサイジングは S3 料金ではなくトラフィックに追従します** — バースト可能な t シリーズのボックスは低/スパイク的なリクエストレートに適し、スループットが上がるにつれて Graviton `c7g` へ移行します。
- **CPU から始めてください。** GPU AMI に手を伸ばすのは、データが整数/カラムナで (`nvcomp-bitcomp` は zstd の約 2 倍に対し **>10×** に達します) *かつ* GPU を稼働させ続けられるほど取り込み量が多い場合だけです。EC2 価格: us-east-1 オンデマンド、2026 年 5 月時点。
- **ホストコストのみ。** 有料の Marketplace リスティングを実行する場合、そのソフトウェア料金は別途発生します (無料の OSS イメージにはありません) — 実際の数値を `s4 estimate` に入れてください。

完全な算出方法 + `s4 estimate` によるデプロイ前シミュレータ: **[docs/savings.md](docs/savings.md)**。

> この表があなたの料金に当てはまるなら、小型 CPU ノード上の **[コンテナリスティング](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e)** に — もしくはデータが整数/カラムナなら **[GPU AMI](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i)** に — テスト用のプレフィックスを 1 つ向けてみてください。

## 実証

主要なラウンドトリップ数値 (RTX 4070 Ti SUPER + Ryzen 9 9950X、`s4-codec` を
通したシングルパス、2026-05-13、nvCOMP 5.2.0.10 / CUDA 13.2)。ディスパッチャは
エントロピー + マジックバイトをサンプリングし、**オブジェクト単位**でルーティング
します — GPU は整数/カラムナ側での乗数であり、一律「GPU で圧縮する」という主張では
ありません:

| ワークロード | 最良の比率 | 最良の圧縮スループット | コーデックの判定 |
|---|---:|---:|---|
| nginx access log (256 MiB)   | **155×** (cpu-zstd-3) | 3.7 GB/s (cpu-zstd-3) | CPU が勝つ — テキストは低い CPU コストでよく重複排除される |
| Parquet-like mixed (256 MiB) | **2.09×** (nvcomp-bitcomp) | 1.5 GB/s (nvcomp-bitcomp) | 整数/カラムナレイアウトでは Bitcomp による GPU が勝つ |
| Postings (u32, 64 MiB)       | **11.9×** (nvcomp-bitcomp) | 1.6 GB/s (nvcomp-bitcomp) | 単調増加の整数カラムでは GPU が決定的に勝つ |
| Already-compressed (64 MiB)  | 1.00× (passthrough)  | 2.2 GB/s (passthrough)| ディスパッチャが検出してスキップ — コーデックコストなし |

*これらはオフラインで計測したシングルパスのコーデック上限です。現実的な本番削減は上記のコスト表 (50–80%) に追従します。デフォルトの 4 MiB フレームサイズでのマルチパートアップロードは、クライアントのチャンクサイズをチューニングするまで、反復的なログを 155× ほどには圧縮しません — [docs/benchmarks.md](docs/benchmarks.md) を参照してください。*

- **互換性** — コアなオブジェクトワークフローについて S3 互換で、45 以上の S3 操作を実装しています (S3 API の完全実装ではありません)。MinIO は PR ごとに検証され、AWS S3 E2E はオプトインです。完全な S3 / SDK / バックエンドのマトリクス: **[docs/compatibility.md](docs/compatibility.md)**。MinIO / Garage / Wasabi / B2 との比較は下記。
- **信頼の根拠** — 714 以上のワークスペーステスト、24/7 のファズファーム (7 つの bolero ターゲット)、敵対的な Opus+Codex 監査ラウンド。`cargo audit` は CVE クリーン。詳細: **[docs/testing.md](docs/testing.md)** · **[docs/status.md](docs/status.md)** · **[完全なベンチマーク](docs/benchmarks.md)**。

### 他との比較

S4 は AWS S3 や任意の S3 互換ストアの**前段**で動作します — それは S4 が圧縮して
書き込むバックエンドであり、競合する相手ではありません。オブジェクトストレージに
圧縮を加えるために手を伸ばすかもしれない他のツールとの比較:

| 機能 | S4 | [MinIO](https://github.com/minio/minio) | [Garage](https://git.deuxfleurs.fr/Deuxfleurs/garage) | Wasabi / B2 |
|---|---|---|---|---|
| 立ち位置 | 既存バケットの前段に立つ圧縮ゲートウェイ | スタンドアロンの S3 システム | スタンドアロンの S3 システム | ホスト型 S3 互換ストレージ |
| **GPU 圧縮** | ✅ nvCOMP zstd / Bitcomp / GDeflate | ❌ | ❌ | ❌ |
| **CPU 圧縮** | ✅ zstd 1–22 / gzip | ⚠️ S2 のみ (レガシー) | ✅ zstd 1–22 | ❌ |
| **自動コーデック選択** | ✅ エントロピー + マジックバイトのサンプリング | ❌ | ❌ | — |
| **圧縮済みデータの Range GET** | ✅ S4IX サイドカー経由 | 該当なし | 該当なし | ✅ |
| **既存バケットで動作** | ✅ (まさにその目的) | ❌ | ❌ | ❌ |
| **ライセンス** | Apache-2.0 | AGPLv3 (+ 商用) | AGPLv3 | プロプライエタリ |

*(ライセンスのセルは上流の LICENSE ファイルを反映しており、リリースごとに変わり得ます。法的助言ではありません。SDK + バックエンドカバレッジを含む完全なマトリクス: [docs/compatibility.md](docs/compatibility.md)。)*

## S4 を使わない方がよい場合

S4 が元を取れないワークロードを正直に列挙します — 今知っておく方がよいでしょう:

- **既に圧縮済みのペイロード** (mp4、jpeg、gzip/zstd アーカイブ、列コーデックを有効にした Parquet) — ディスパッチャがこれらを `passthrough` にルーティングするため害はありませんが、節約もありません。
- **小さい / メタデータの多いワークロード** (16 KiB 未満のオブジェクト、あるいは `List`/`Head`/`Copy` が支配的なトラフィック) — フレーム + サイドカーのオーバーヘッドが比率を食い潰し、S4 はコーデックに触れないままホップを 1 つ増やします。経験則: オブジェクトが 1 MiB を超えると計算が快適になります。
- **超低レイテンシのホットリード** (p99 GET が 10ms 未満) — ストリーミングデコード + サイドカーフェッチがレイテンシを加えます。分析/アーカイブには最適ですが、OLTP の読み込み経路には向きません。
- **Glacier 専用のコールドストレージ** — Glacier は既に十分安価な価格設定で、圧縮がコンピュートの元を取ることはまれです。
- **SOC2 / ISO 27001 / FedRAMP の証跡が今すぐ必要な規制対象ワークロード** — それらのレポートはまだ存在しません。揃うまで待ってください。
- **代替不可能なデータ、あるいは初めての本番ロールアウト** — まだ公開された本番リファレンスがないため、レプリケーション済み・バージョニング済みのテストプレフィックスから始め、バックエンドネイティブのリカバリを有効に保ってください。

> ワークロードが圧縮可能で、オブジェクトサイズが十分で、レイテンシクリティカルでないなら、S4 はそのために作られています。小型 CPU ノード上の **[コンテナリスティング](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e)** から — もしくは高スループットの整数/カラムナデータには **[GPU AMI](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i)** から — 始めて、まずテスト用のプレフィックスを 1 つゲートウェイに向けてください。

## ローカルで試す (60 秒、CPU のみ)

**まず実際の料金を見積もってください** — `s4 estimate` はバケットのオブジェクトサイズ + サンプルを読み取り、節約額を予測します。ゲートウェイのデプロイは不要です:

```bash
s4 estimate <bucket>[/prefix] --endpoint-url https://s3.<region>.amazonaws.com
```

次に、使い捨てのローカル MinIO に対してエンドツーエンドで試してみてください:

```bash
git clone https://github.com/abyo-software/s4 && cd s4
docker compose up -d                    # MinIO + S4 server on localhost:8014

# Generate a sample object so the cp lines have something to upload.
yes '2026-06-18T00:00:00Z INFO tenant=demo path=/api/v1/items status=200 bytes=1842' \
  | head -n 2000000 > big.log    # ~150 MiB of log-like text, compresses heavily

# Use any S3 client. Below uses aws-cli; replace endpoint with anything.
aws --endpoint-url http://localhost:8014 s3 mb s3://demo
aws --endpoint-url http://localhost:8014 s3 cp big.log s3://demo/big.log
aws --endpoint-url http://localhost:8014 s3 cp s3://demo/big.log ./big.log.roundtrip

# Inspect the compressed object directly on MinIO (different endpoint, bypasses S4).
aws --endpoint-url http://localhost:9000 s3 cp s3://demo/big.log ./big.log.compressed
ls -la big.log big.log.compressed big.log.roundtrip
# Expected: big.log == big.log.roundtrip (lossless), big.log.compressed is much smaller.
```

その他のインストール経路 — cargo、pip、WASM、ソースからのビルド: **[docs/install.md](docs/install.md)**。GPU トライアル + チューニング: **[docs/gpu.md](docs/gpu.md)**。

## デプロイ

- **コンテナ (EKS / ECS / Fargate)** — 公開イメージ + Helm チャート、任意の CPU ノードで動作、AWS による **pod-hour 課金** → **[Marketplace リスティング](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e)**。無料の `ghcr.io` イメージと同じバイナリです。メータリングは `--marketplace-product-code` (+ カスタムディメンション製品では `--marketplace-usage-dimension`、[仕組み](docs/marketplace/metering.md)) によるオプトインです。Marketplace の pod はエンタイトルメント + `aws-marketplace:MeterUsage` を必要とし、エンタイトルメントがない場合は**起動時にフェイルクローズ**します。
- **EC2 GPU AMI** — 自己完結型の Amazon Linux 2023 イメージ (NVIDIA ドライバ + S4 がプリインストール済み)、g4dn / g5 / g6 / g6e での AWS による **instance-hour 課金**。整数/カラムナデータを高スループットで処理する場合向け。起動し、S3 クライアントを向けるだけで完了 → **[Marketplace リスティング](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i)**。
- **セルフマネージド Kubernetes** — `ghcr.io/abyo-software/s4` イメージ + Helm チャート: **[docs/deployment.md](docs/deployment.md)**。

## 運用

| ツール | 用途 |
|---|---|
| `s4 estimate` | デプロイ**前**にバケットのストレージ節約をシミュレート |
| `s4 savings` | 本番で計測した実際の節約をレポート (v1.2) |
| `s4 migrate` | 既にバケットに置かれているオブジェクトを後追いで圧縮 |
| `s4 recompact` | コールドデータをより高い zstd レベルに再パック |
| `s4 maintain` | ポリシー駆動のバケットメンテナンス (migrate / recompact / transition) |
| `s4 train-dict` | 小さく均質なオブジェクト向けの共有 zstd 辞書 |

算出方法 + フラグ: **[savings](docs/savings.md)** · **[maintenance](docs/ops/maintenance.md)** · **[dictionaries](docs/ops/dictionaries.md)** · **[durability & repair](docs/ops/repair.md)** · **[runbook](docs/ops/runbook.md)** · **[configuration](docs/configuration.md)**。

## 安定性とステータス

S4 は SemVer で安定した表面を持つ **v1.x** です: バックエンドのワイヤフォーマット、
コアな CLI サブコマンド、ライブラリ API、`s3s` HTTP トレイト群、Helm の `values.yaml`
キー形状は凍結されています — `s4-server = "1"` または `ghcr.io/abyo-software/s4:1` を
ピン留めし、それが足元で動かないことに依存してください。完全な凍結契約:
**[docs/stability.md](docs/stability.md)**。

> **まだ公開された本番デプロイのリファレンスはありません。** 凍結は表面の安定性に
> 関する契約であり、運用実績の代わりにはなりません。TB 規模または代替不可能なデータには、
> S4 をバックエンドネイティブのバージョニング + レプリケーションと組み合わせ、デプロイした
> 際は `production-reference` タグを付けた issue を提出してください。完全なステータス、監査
> 履歴、ファズの証跡: **[docs/status.md](docs/status.md)**。

## ドキュメント

| 領域 | ドキュメント |
|---|---|
| はじめに | [install](docs/install.md) · [GPU](docs/gpu.md) · [deploy (Helm)](docs/deployment.md) · [configuration](docs/configuration.md) |
| コストと運用 | [savings & estimate](docs/savings.md) · [maintenance](docs/ops/maintenance.md) · [dictionaries](docs/ops/dictionaries.md) · [repair & durability](docs/ops/repair.md) · [runbook](docs/ops/runbook.md) · [observability](docs/observability.md) · [storage-class transitions](docs/storage-class-transitions.md) |
| リファレンス | [compatibility matrices](docs/compatibility.md) · [architecture](docs/architecture.md) · [on-the-wire format](docs/wire-format.md) · [production features](docs/features.md) |
| 実証と信頼 | [benchmarks](docs/benchmarks.md) · [testing & validation](docs/testing.md) · [stability contract](docs/stability.md) · [project status](docs/status.md) · [threat model](docs/security/threat-model.md) · [security overview](docs/security/overview.md) |
| Marketplace | [metering](docs/marketplace/metering.md) · [listing source-of-truth](docs/marketplace/listing.md) |

## abyo software の他の製品

S4 は、私たちが Rust で構築する AWS ネイティブのコスト最適化・セキュリティツール群の
1 つで、すべて単一のセラーアカウントで AWS Marketplace に掲載されています — カタログは
**[AWS Marketplace の abyo software](https://aws.amazon.com/marketplace/seller-profile?id=seller-65lhisp4ppavm)** で閲覧できます。

| 製品 | 何をするか |
|---|---|
| **S4 — Squished S3** | このプロジェクト: 透過的な GPU/CPU S3 圧縮ゲートウェイ。→ [Container](https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e) · [GPU AMI](https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i) |
| **S4 Logs** | CloudWatch Logs → S3 のアーカイバ。ログストレージコストを削減。 |
| **S4 LogForge** | 現実的な SIEM テストログジェネレータ — 13 フォーマットにわたりパーサ検証済みの出力。 |
| **S4 Scan** | Amazon Athena のスキャンコスト削減ツール。 |
| **S4 NAT** | Amazon VPC 向けのコスト最適化 NAT。 |
| **S4 MockAPI** | テストとデモ向けのセキュリティ API シミュレータ。 |

## コントリビュート

プルリクエスト歓迎 — セットアップ、規約、テスト/ファズ/ソークのプロトコルについては
[CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。コントリビューションは Apache-2.0 で
ライセンスされます (別途の CLA はありません)。

## セキュリティ

脆弱性を見つけましたか? **公開 issue は開かないでください** — 協調的開示については
[SECURITY.md](SECURITY.md) に従ってください。

## ライセンス

Apache-2.0 ([LICENSE](LICENSE) / [NOTICE](NOTICE))。オプションの `nvcomp-gpu`
機能は、ビルド時にプロプライエタリな NVIDIA nvCOMP SDK を取得します (バンドルされておらず、
NVIDIA の条件のもとで各自で用意してください)。完全な第三者開示:
[docs/THIRD_PARTY_LICENSES.html](docs/THIRD_PARTY_LICENSES.html)。

`"S4"` および `"Squished S3"` は abyo software 合同会社の未登録商標です。
`"Amazon S3"` および `"AWS"` は Amazon.com, Inc. の商標です。S4 は Amazon と
提携、推奨、後援のいずれの関係にもありません。

## 著者

- abyo software 合同会社 — スポンサー組織、商用 AMI 配布
- masumi-ryugo — 原作者 / メンテナ
