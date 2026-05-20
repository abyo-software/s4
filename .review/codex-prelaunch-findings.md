以下、HN / r/rust 投下前に刺されやすい順です。

**CRITICAL**: 「No lock-in: stop the gateway, read your bucket directly with aws-cli」は、本文の仕様と矛盾します。S4が圧縮済みオブジェクト＋sidecar indexを保存するなら、通常のaws-cliで元バイト列を直接読めないはずなので、「raw compressed objects remain accessible; original bytes require S4/codec/WASM tool」などに修正。

**CRITICAL**: 「S3 API compatibility: Full」は危険です。S3互換ゲートウェイでFullを名乗ると、multipart edge cases、versioning、SSE、object lock、ACL、presigned URL、checksums、conditional requests、Select、lifecycle等を突かれるため、実装済みAPIの表に落とす。

**HIGH**: 「Cuts your AWS S3 bill 50–80%」と直後の「99% saved」が過大に見えます。データ分布、request cost、egress、compute/GPU、index overhead、minimum object size、S3 IA等を含まないので、「on highly compressible payload storage bytes; not total bill」へ限定。

**HIGH**: benchmark表の「Best compress throughput」がnginxでcpu-zstd-3になっており、GPU加速プロダクトの主張を弱めます。GPUが勝つケースとCPUが勝つケースを明示し、「GPU accelerates selected columnar/integer workloads」へ正直に絞る。

**HIGH**: 「transparent compression every object with GPU codecs」は、already-compressedや小物体、非GPU環境ではpassthrough/CPUになるはずで誤解を招きます。dispatcherの条件、fallback、codec選択結果の観測方法をREADME上部に書く。

**HIGH**: cargo installが罠です。`cargo install s4-server`で本当にcrates.io公開済みか、binary名が`s4`か、Rust 1.92+が現実的に入るか、GPU featureが使えないことを明記しないと初手で失敗報告が出ます。

**HIGH**: Docker quickstartの検証コマンドが怪しいです。`aws ... cp s3://demo/big.log -.compressed`は宛先名として不自然で、`ls -la big.log -.compressed`もローカルに`big.log`が存在する前提なので、コピペ実行で混乱します。明示的に`./big.log.compressed`へ保存し、元ファイル生成コマンドも入れる。

**HIGH**: 「Range GET parquet/ORC reader compatible」は強い主張です。圧縮形式のブロック境界、sidecar index不整合時、multipart、suffix ranges、parallel ranges、ETag/Content-Rangeの意味を検証表で支えないと突かれます。

**HIGH**: security境界が薄いです。SigV4を受けるなら、認証情報の扱い、backend credential delegation、bucket policy enforcementの限界、TLS終端、request smuggling/body size limits、tenant isolationをREADMEに最低限書く。

**MEDIUM**: 「same SDK calls / no app changes」は、endpoint URL変更、path-style/virtual-hosted-style、region、TLS証明書、presigned URLs、checksum headers、content-encoding期待値で壊れる可能性があります。互換性マトリクスと既知制限を追加。

**MEDIUM**: 「MinIO S2 / Garage zstd are CPU-only」だけでは差別化が薄いです。競合は圧縮以外に成熟度、分散配置、運用実績、管理UI、IAM互換性が強いので、S4は「existing S3 gateway + GPU compression + range index」に絞って比較。

**MEDIUM**: MinIO licenseが「AGPLv3 / commercial」とだけあるのは議論を呼びます。比較表のライセンス欄は正確なプロジェクト名・バージョン・リンク付きにし、法的断定を避ける。

**MEDIUM**: Apache-2.0を掲げるならnvCOMP/CUDA/NVIDIA Container Toolkit、zstd、ring、PyO3、WASM周辺のライセンス・再配布条件を明示したほうが安全です。`cargo deny`/`cargo about`出力へのリンクを追加。

**MEDIUM**: operational costが抜けています。GPUノード代、CPU fallback時のレイテンシ、S3 PUT/GET増加、sidecar object数、metrics cardinality、cacheなし時のrange性能を「when not to use S4」に書く。

**MEDIUM**: durability/corruption時の説明が足りません。compressed objectとsidecar indexの原子的更新、CRCの対象、partial PUT失敗、backend retry後の整合性、repair tool有無を明記。

**LOW**: 「TTFB ms-class, ~10 MiB peak」は根拠が見えません。測定条件、object size、codec、concurrencyを添えるか削る。

**LOW**: Helm欄で「image is not yet on Docker Hub」はlaunch前にかなり目立つ未完成感があります。公開するか、最初からlocal build手順を主導線にする。

**LOW**: README冒頭の数字が強すぎて、研究プロトタイプ感と商用ゲートウェイ感が混ざっています。最初に「alpha / experimental / production readiness status」を明示すると炎上しにくいです。
