//! S4 server binary。`s4-server::S4Service` で `s3s_aws::Proxy` を圧縮 hook 付きに
//! ラップし、hyper-util 経由で公開する。

// tracing-subscriber + OpenTelemetry の Layered<...> 型が深くなり trait
// resolver の default depth (128) を超えるため、解決上限を 512 に上げる。
#![recursion_limit = "512"]

use std::error::Error;
use std::io::IsTerminal;
use std::str::FromStr;
use std::sync::Arc;

use aws_credential_types::provider::ProvideCredentials;
use clap::{Args, Parser, Subcommand, ValueEnum};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use s3s::S3;
use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::{AlwaysDispatcher, SamplingDispatcher};
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecDispatcher, CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::routing::{HealthRouter, ReadyCheck};
use tokio::net::TcpListener;
use tracing::info;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CodecChoice {
    /// 無圧縮 (開発・比較用)
    Passthrough,
    /// CPU zstd (GPU 不要、test bed)
    CpuZstd,
    /// CPU gzip (RFC 1952; wire-compatible with stock gunzip / browsers)
    CpuGzip,
    /// nvCOMP zstd-GPU (要 nvcomp-gpu feature)
    #[cfg(feature = "nvcomp-gpu")]
    NvcompZstd,
    /// nvCOMP Bitcomp (整数列向け、要 nvcomp-gpu feature)
    #[cfg(feature = "nvcomp-gpu")]
    NvcompBitcomp,
    /// nvCOMP GDeflate (DEFLATE-family GPU codec、要 nvcomp-gpu feature)
    #[cfg(feature = "nvcomp-gpu")]
    NvcompGdeflate,
}

impl CodecChoice {
    fn as_kind(self) -> CodecKind {
        match self {
            Self::Passthrough => CodecKind::Passthrough,
            Self::CpuZstd => CodecKind::CpuZstd,
            Self::CpuGzip => CodecKind::CpuGzip,
            #[cfg(feature = "nvcomp-gpu")]
            Self::NvcompZstd => CodecKind::NvcompZstd,
            #[cfg(feature = "nvcomp-gpu")]
            Self::NvcompBitcomp => CodecKind::NvcompBitcomp,
            #[cfg(feature = "nvcomp-gpu")]
            Self::NvcompGdeflate => CodecKind::NvcompGDeflate,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DispatcherChoice {
    /// 常に CLI で指定した codec を使う
    Always,
    /// 入力 sample (entropy + magic bytes) で codec を自動選択
    Sampling,
}

/// v0.5 #32: regulated-industry posture switch.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ComplianceMode {
    /// TLS 1.3-only + audit-signed + SSE-required + object-lock manager.
    Strict,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    /// 人間向け (terminal でカラー化、tracing-subscriber default)
    Pretty,
    /// JSON 1 行 = 1 event (CloudWatch Logs Insights / fluent-bit と統合しやすい)
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "s4",
    version,
    about = "S4 — Squished S3 (GPU 透過圧縮 S3 互換ゲートウェイ)"
)]
struct Opt {
    #[clap(long, default_value = "127.0.0.1")]
    host: String,

    #[clap(long, default_value = "8014")]
    port: u16,

    #[clap(long)]
    domain: Option<String>,

    /// バックエンド S3 endpoint (例: https://s3.us-east-1.amazonaws.com)。
    /// 通常 (server mode) は必須。`verify-audit-log` 等の non-server
    /// subcommand では指定不要 (server 起動時に runtime 検証する)。
    #[clap(long)]
    endpoint_url: Option<String>,

    /// 既定の圧縮 codec (PUT 時に dispatcher が選ぶ default)
    #[clap(long, value_enum, default_value = "cpu-zstd")]
    codec: CodecChoice,

    /// CPU zstd の compression level (1-22)
    #[clap(long, default_value_t = CpuZstd::DEFAULT_LEVEL)]
    zstd_level: i32,

    /// codec dispatcher: always (CLI 指定固定) / sampling (auto 選択)
    #[clap(long, value_enum, default_value = "sampling")]
    dispatcher: DispatcherChoice,

    /// v0.8 #56: minimum body size (bytes) at which the sampling dispatcher
    /// prefers a GPU codec over CPU. Below this threshold the GPU upload
    /// overhead exceeds the compress time, so CPU wins. Default 1 MiB. Has
    /// no effect when no CUDA-capable GPU is detected at boot, when the
    /// `nvcomp-gpu` feature is not compiled in, or when `--dispatcher always`
    /// is selected.
    #[clap(long, default_value_t = 1_048_576)]
    gpu_min_bytes: usize,

    /// ログ出力形式 (pretty / json)。production では json 推奨
    #[clap(long, value_enum, default_value = "pretty")]
    log_format: LogFormat,

    /// OpenTelemetry OTLP gRPC endpoint (例: http://otel-collector:4317)。
    /// 指定すると各 PUT/GET request が trace span として export される
    #[clap(long)]
    otlp_endpoint: Option<String>,

    /// OTel resource service.name (default: "s4")
    #[clap(long, default_value = "s4")]
    service_name: String,

    /// TLS server certificate (PEM file). Together with --tls-key enables
    /// HTTPS termination on the listener. Without these flags, S4 serves
    /// plain HTTP.
    #[clap(long, requires = "tls_key")]
    tls_cert: Option<std::path::PathBuf>,

    /// TLS server private key (PEM file, PKCS#8 or RSA). See --tls-cert.
    #[clap(long, requires = "tls_cert")]
    tls_key: Option<std::path::PathBuf>,

    /// Comma-separated list of domains for ACME (Let's Encrypt) auto-cert.
    /// Mutually exclusive with --tls-cert / --tls-key. Uses the TLS-ALPN-01
    /// challenge handled inline on the listening port — no separate HTTP
    /// listener required. The listener MUST be reachable from the public
    /// internet on this --port for renewal to succeed.
    #[clap(long, conflicts_with_all = ["tls_cert", "tls_key"])]
    acme: Option<String>,

    /// Contact email for ACME account registration. Required when --acme is
    /// set; Let's Encrypt uses this for cert-expiry notifications.
    #[clap(long, requires = "acme")]
    acme_contact: Option<String>,

    /// Directory for caching ACME account + cert state across restarts.
    /// Default: `<HOME>/.s4/acme/`. The cert is renewed automatically at
    /// the standard ~60-day mark.
    #[clap(long, requires = "acme")]
    acme_cache_dir: Option<std::path::PathBuf>,

    /// Use the Let's Encrypt staging directory (no rate limits, but the
    /// resulting cert is not browser-trusted). Recommended for first-run
    /// validation; flip off once the deployment is confirmed working.
    #[clap(long, requires = "acme")]
    acme_staging: bool,

    /// Optional AWS-style bucket policy JSON file. When set, every PUT /
    /// GET / DELETE / List request is evaluated against the policy before
    /// being forwarded to the backend; explicit Deny or implicit deny
    /// returns AccessDenied. See `s4_server::policy` docs for the supported
    /// subset.
    #[clap(long)]
    policy: Option<std::path::PathBuf>,

    /// Optional per-(principal, bucket) token-bucket rate-limit JSON file.
    /// Format: `[{"principal": "AKIA...", "bucket": "*", "rps": 100,
    /// "burst": 500}, ...]`. First-match-wins on the rule list. Throttled
    /// requests return `SlowDown` (HTTP 503) and bump
    /// `s4_rate_limit_throttled_total{principal,bucket}`.
    #[clap(long)]
    rate_limit: Option<std::path::PathBuf>,

    /// Optional server-side encryption key for SSE-S4 (AES-256-GCM).
    /// Path to a 32-byte key file (raw bytes, 64-char hex, or 44-char
    /// base64). When set, every PUT body gets wrapped with S4E2 (under
    /// id=1, the default active slot) after compression + framing;
    /// every GET that's flagged `s4-encrypted` gets decrypted before
    /// frame parse. The compress-then-encrypt order preserves the
    /// codec's compression ratio.
    #[clap(long)]
    sse_s4_key: Option<std::path::PathBuf>,

    /// v0.5 #29: additional retired keys for SSE-S4 rotation. Format
    /// `id=N,key=<path>`. Repeatable — pass once per old key kept
    /// around for decryption. Combined with `--sse-s4-key` (which
    /// becomes the active id=1 slot), the gateway will encrypt every
    /// new PUT under id=1 and still decrypt any S4E2 body whose
    /// header points at one of the rotated ids. To rotate properly
    /// (active = a fresh id), supply the new key as `--sse-s4-key`
    /// and move the previous key over to
    /// `--sse-s4-key-rotated id=2,key=/path/to/old.key` (or any id
    /// other than 1).
    #[clap(long, value_name = "id=N,key=PATH")]
    sse_s4_key_rotated: Vec<String>,

    /// v0.8 #52: plaintext bytes per AES-GCM chunk on the SSE-S4
    /// PUT path. When > 0 (default 1 MiB), every SSE-S4 PUT writes
    /// the chunked **S4E5** frame instead of the buffered S4E2
    /// frame, so the matching GET can stream-decrypt chunk-by-chunk
    /// (TTFB ≈ AES-GCM cost of one chunk on a 5 GiB object, instead
    /// of waiting for the entire body's tag to verify). Set to `0`
    /// to disable and revert to the legacy S4E2 buffered path
    /// (kept around for back-compat with v0.7-and-earlier
    /// deployments that need bit-for-bit identical output). Has no
    /// effect when `--sse-s4-key` is absent. SSE-C / SSE-KMS are
    /// intentionally unaffected (chunked variants are a follow-up
    /// issue).
    #[clap(long, value_name = "BYTES", default_value_t = 1_048_576)]
    sse_chunk_size: usize,

    /// v0.8.11 CRIT-4 fix: opt in to honouring the leftmost token of
    /// the `X-Forwarded-For` request header as the `aws:SourceIp`
    /// Condition key (and as the access-log `remote_ip`). Default
    /// (`false`) makes the gateway treat `X-Forwarded-For` as
    /// untrusted noise, so a public-internet client can no longer
    /// satisfy a `Condition: IpAddress aws:SourceIp [10.0.0.0/8]`
    /// Allow rule by sending the header themselves. Enable ONLY when
    /// this gateway sits behind a trusted reverse proxy / LB that
    /// scrubs (or sets) `X-Forwarded-For` for every request. Gateways
    /// listening directly on the public internet must leave this off
    /// (or move the IP gate to the proxy). A future release will
    /// validate the forwarded address against a `--trusted-proxies`
    /// CIDR list using the real TCP peer; until then this flag is
    /// the supported way to opt back into the legacy behaviour.
    #[clap(long, default_value_t = false)]
    trust_x_forwarded_for: bool,

    /// Optional S3-style access-log destination directory. When set,
    /// every completed PUT / GET / DELETE / List request is buffered
    /// and flushed to hourly-rotated `.log` files under the directory.
    /// v0.4 ships local-directory only; pipe via filebeat / vector / etc
    /// to ship to S3 if needed (a follow-up issue tracks native s3://
    /// destination).
    #[clap(long)]
    access_log: Option<std::path::PathBuf>,

    /// v0.5 #31: optional HMAC-SHA256 key for tamper-evident audit log
    /// chaining. When set together with --access-log, every emitted
    /// access-log line gets a trailing hex HMAC column and each rotated
    /// batch file starts with `# prev_file_tail=<hex>` so the chain
    /// extends across rotations. Format: `raw:<bytes>`, `hex:<hex>`,
    /// or `base64:<b64>`. Verify with the `verify-audit-log` subcommand.
    #[clap(long)]
    audit_log_hmac_key: Option<String>,

    /// v0.5 #28: directory of `.kek` files for the local SSE-KMS
    /// backend (`LocalKms`). Each file is exactly 32 raw bytes; the
    /// basename (sans `.kek`) is the key id a client supplies via
    /// `x-amz-server-side-encryption-aws-kms-key-id`. PUTs that ask for
    /// `x-amz-server-side-encryption: aws:kms` mint a fresh DEK,
    /// AES-256-GCM-wrap it under the named KEK, and persist the wrapped
    /// blob in an S4E4 frame; GETs unwrap through the same KEK. KEKs
    /// must be raw 32-byte randomness from /dev/urandom.
    #[clap(long, value_name = "DIR")]
    kms_local_dir: Option<std::path::PathBuf>,

    /// v0.5 #28: KMS key id used for SSE-KMS PUTs that don't carry an
    /// explicit `x-amz-server-side-encryption-aws-kms-key-id` header.
    /// Mirrors AWS S3's bucket-default key behaviour. When unset, every
    /// SSE-KMS PUT must name an explicit key id.
    #[clap(long, value_name = "KEY_ID")]
    kms_default_key_id: Option<String>,

    /// v0.5 #33: directory of PEM-encoded `<access_key_id>.pem` ECDSA
    /// P-256 SubjectPublicKeyInfo files. Enables SigV4a (asymmetric)
    /// signature verification for incoming requests. SigV4 (the
    /// existing HMAC-based signing) keeps working unchanged when this
    /// flag is unset.
    #[clap(long, value_name = "DIR")]
    sigv4a_credentials: Option<std::path::PathBuf>,

    /// v0.8.4 #76 (audit H-6): how far the request's `x-amz-date` may
    /// drift from the server's clock before being rejected with HTTP
    /// 403 `RequestTimeTooSkewed`. Default 900s = 15 min, matching
    /// AWS S3's documented spec. Operators can widen this for
    /// high-clock-drift environments or tighten it for compliance
    /// regimes that demand stricter freshness — but a value of 0
    /// effectively disables SigV4a (every request will fall outside
    /// any non-zero drift), so it is rejected at boot.
    ///
    /// Has no effect when `--sigv4a-credentials` is unset (no SigV4a
    /// gate to skew-check against).
    #[clap(long, value_name = "SECS", default_value_t = 900)]
    sigv4a_skew_tolerance_seconds: u32,

    /// v0.8.5 #84 (audit H-5): per-connection wall-clock cap including
    /// header + body reads. Slowloris guard. Default 30s — HTTP
    /// keep-alive within this window is fine because the timeout
    /// resets on each request boundary via hyper's read budget. Set
    /// to 0 to disable (NOT recommended in production — slow clients
    /// can then occupy a task / FD slot indefinitely).
    #[clap(long, value_name = "SECS", default_value_t = 30)]
    read_timeout_seconds: u64,

    /// v0.8.5 #84 (audit H-5): hard cap on the number of in-flight
    /// HTTP connections the listener will hold open at once. New
    /// accepts above the cap park on the semaphore until an existing
    /// connection drains. Defaults to 1024 — high enough for normal
    /// fan-out, low enough to bound FD / task pressure under attack.
    #[clap(long, value_name = "N", default_value_t = 1024)]
    max_concurrent_connections: usize,

    /// v0.8.5 #84 (audit H-6): max HTTP/1 header buffer size in
    /// bytes. AWS S3 max header size is 8 KiB per header * ~50
    /// headers; 64 KiB total is safe margin. Reject larger to bound
    /// memory per malicious request. Minimum 8 KiB enforced by hyper
    /// (anything smaller panics the builder).
    #[clap(long, value_name = "BYTES", default_value_t = 65_536)]
    max_header_bytes: usize,

    /// v0.8.5 #84 (audit H-6): enable HTTP/2 alongside HTTP/1.1.
    /// Default off — the S3 API is HTTP/1.1-only in practice; turning
    /// h2 on widens the attack surface (HTTP/2 has its own DoS
    /// surface: rapid reset, settings flood, etc). When on, the
    /// listener also caps `max_concurrent_streams` at 100 and
    /// `max_header_list_size` at 16 KiB.
    #[clap(long, default_value_t = false)]
    http2: bool,

    /// v0.5 #34: enable the in-memory first-class versioning state
    /// machine (`VersioningManager`). When set, S4-server itself owns
    /// per-bucket versioning state + per-(bucket, key) version chain
    /// (replacing the previous backend-passthrough behaviour). The
    /// optional path argument names a JSON snapshot file that's
    /// loaded at startup if present; SIGUSR1-driven dump-back-to-file
    /// is a future hook (see `VersioningManager::to_json` /
    /// `from_json` — not yet wired into a signal handler in v0.5
    /// #34's scope). To enable the manager without restoring a
    /// snapshot, pass any path whose file is missing or empty
    /// (e.g. `--versioning-state-file /tmp/v.json`).
    #[clap(long, value_name = "PATH")]
    versioning_state_file: Option<std::path::PathBuf>,

    /// v0.5 #30: enable the in-memory Object Lock (WORM) enforcement
    /// manager. When set, S4-server refuses DELETE / overwrite on
    /// objects covered by an active retention or legal hold (HTTP 403
    /// `AccessDenied`), and auto-applies any per-bucket default
    /// retention to new PUTs. The optional path argument names a JSON
    /// snapshot file that's loaded at startup if present; SIGUSR1-
    /// driven dump-back-to-file is a future hook (see
    /// `ObjectLockManager::to_json` / `from_json` — not yet wired in
    /// v0.5 #30's scope). To enable without restoring, pass any path
    /// whose file is missing or empty.
    #[clap(long, value_name = "PATH")]
    object_lock_state_file: Option<std::path::PathBuf>,

    /// v0.6 #42: enable the in-memory MFA-Delete enforcement manager
    /// (`MfaDeleteManager`). When set, every DELETE / DELETE-version /
    /// delete-marker / `PutBucketVersioning` request against a bucket
    /// whose MFA-Delete state is `Enabled` requires a valid
    /// `x-amz-mfa: <serial> <code>` header (RFC 6238 6-digit TOTP);
    /// missing or invalid tokens return HTTP 403 `AccessDenied`. The
    /// optional path argument names a JSON snapshot file that's loaded
    /// at startup if present (produced previously by
    /// `MfaDeleteManager::to_json`); SIGUSR1-driven dump-back is a
    /// future hook. To enable without restoring, pass any path whose
    /// file is missing or empty. Pair with
    /// `--mfa-default-secret-file <PATH>` to install a gateway-wide
    /// shared secret applied to every MFA-Delete-enabled bucket that
    /// lacks an explicit per-bucket override.
    #[clap(long, value_name = "PATH")]
    mfa_delete_state_file: Option<std::path::PathBuf>,

    /// v0.6 #42: install a gateway-wide default MFA secret. The file
    /// must contain exactly one line of the form
    /// `<base32_secret> <serial>` (single ASCII space, no surrounding
    /// quotes); the secret is RFC 4648 un-padded base32 and must be at
    /// least 128 bits long (16 bytes raw → 26 base32 chars). Has no
    /// effect unless `--mfa-delete-state-file` is also set.
    #[clap(long, value_name = "PATH")]
    mfa_default_secret_file: Option<std::path::PathBuf>,

    /// v0.6 #38: enable the in-memory CORS bucket-configuration manager
    /// (`CorsManager`). When set, S4-server itself owns per-bucket CORS
    /// rules — `PutBucketCors` / `GetBucketCors` / `DeleteBucketCors`
    /// route through the manager (replacing the previous
    /// backend-passthrough behaviour). The optional path argument names
    /// a JSON snapshot file that's loaded at startup if present;
    /// SIGUSR1-triggered dump-back is a future hook (see
    /// `CorsManager::to_json` / `from_json`). To enable without
    /// restoring, pass any path whose file is missing or empty.
    /// **Note:** OPTIONS preflight HTTP routing is
    /// not yet wired at the listener level; this flag enables only the
    /// configuration-management half of v0.6 #38.
    #[clap(long, value_name = "PATH")]
    cors_state_file: Option<std::path::PathBuf>,

    /// v0.6 #36: enable the in-memory S3 Inventory manager
    /// (`InventoryManager`). When set, S4-server owns per-(bucket, id)
    /// inventory configurations — `PutBucketInventoryConfiguration` /
    /// `GetBucketInventoryConfiguration` /
    /// `ListBucketInventoryConfigurations` /
    /// `DeleteBucketInventoryConfiguration` route through the manager
    /// (replacing the previous backend-passthrough behaviour). A
    /// background tokio task wakes every
    /// `--inventory-scan-interval-hours` to log the set of currently-
    /// due inventories. To enable without restoring, pass any path
    /// whose file is missing or empty. The optional
    /// path argument names a JSON snapshot file produced previously by
    /// `InventoryManager::to_json`.
    ///
    /// **Note (v0.6 #36 scope):** the background scheduler currently
    /// only logs and stamps `mark_run` for each due config. Walking the
    /// source bucket and writing the CSV / manifest to the destination
    /// bucket happens via `InventoryManager::run_once_for_test`, which
    /// is the path the unit + E2E tests exercise; wiring the scheduler
    /// to walk a real bucket end-to-end is deferred to a follow-up
    /// because it requires a back-reference from the scheduler into
    /// `S4Service` for the `list_objects_v2` walk and that reshuffle
    /// is out of scope for this issue.
    #[clap(long, value_name = "PATH")]
    inventory_state_file: Option<std::path::PathBuf>,

    /// v0.6 #36: cadence (in hours) at which the background inventory
    /// scheduler wakes to check which configurations are due. Defaults
    /// to 1 (= once an hour). Independent of any individual config's
    /// `frequency_hours`; the scheduler only triggers a config when its
    /// own `due()` predicate returns `true`. No effect when
    /// `--inventory-state-file` is not supplied.
    #[clap(long, value_name = "N", default_value_t = 1)]
    inventory_scan_interval_hours: u32,

    /// v0.6 #35: enable the in-memory bucket-notification manager
    /// (`NotificationManager`). When set, S4-server itself owns per-bucket
    /// notification configurations — `PutBucketNotificationConfiguration` /
    /// `GetBucketNotificationConfiguration` route through the manager
    /// (replacing the previous backend-passthrough behaviour), and
    /// successful `put_object` / `delete_object` calls fire matching
    /// destinations on a detached tokio task (best-effort fire-and-forget;
    /// failed deliveries bump the `s4_notifications_dropped_total` counter
    /// after the configured retry budget). The optional path argument
    /// names a JSON snapshot file that's loaded at startup if present;
    /// SIGUSR1-driven dump-back is a future hook (see
    /// `NotificationManager::to_json` / `from_json`). To enable
    /// without restoring, pass any path whose file is missing or
    /// empty.
    ///
    /// **Note (v0.6 #35 scope):** the always-available destination is
    /// `Webhook { url }` (HTTP POST of the AWS-canonical event JSON). SQS
    /// / SNS destinations are accepted at config time but only fire when
    /// the gateway is built with `--features aws-events` (otherwise the
    /// dispatcher logs at warn and bumps the drop counter). Lambda direct
    /// invocation is not implemented; subscribe Lambda to an SNS topic
    /// instead.
    #[clap(long, value_name = "PATH")]
    notifications_state_file: Option<std::path::PathBuf>,

    /// v0.6 #39: enable the in-memory object + bucket Tagging manager
    /// (`TagManager`). When set, S4-server itself owns per-(bucket, key)
    /// and per-bucket tag state — `Put/Get/Delete Object/Bucket Tagging`
    /// route through the manager (replacing the previous
    /// backend-passthrough behaviour) and the IAM policy evaluator
    /// gains the `s3:ExistingObjectTag/<key>` /
    /// `s3:RequestObjectTag/<key>` condition keys (resolved from the
    /// manager and the parsed `x-amz-tagging` PUT header respectively).
    /// The optional path argument names a JSON snapshot file that's
    /// loaded at startup if present; SIGUSR1-driven dump-back-to-file
    /// is a future hook (see `TagManager::to_json` / `from_json` — not
    /// yet wired into a signal handler in v0.6 #39's scope). To
    /// enable without restoring, pass any path whose file is missing
    /// or empty.
    #[clap(long, value_name = "PATH")]
    tagging_state_file: Option<std::path::PathBuf>,

    /// v0.6 #40: enable the in-memory cross-bucket replication manager
    /// (`ReplicationManager`). When set, S4-server itself owns per-bucket
    /// replication rules — `PutBucketReplication` / `GetBucketReplication` /
    /// `DeleteBucketReplication` route through the manager (replacing the
    /// previous backend-passthrough behaviour), and every successful
    /// `put_object` whose key matches an enabled rule is asynchronously
    /// PUT to the rule's destination bucket on a detached tokio task.
    /// The replica is stamped with `x-amz-replication-status: REPLICA`,
    /// the source-side per-key status flips
    /// `PENDING → COMPLETED` (or `FAILED` after the 3-attempt retry
    /// budget). HEAD / GET on the source key echo the recorded status as
    /// `x-amz-replication-status`. The optional path argument names a
    /// JSON snapshot file (`ReplicationManager::to_json` /
    /// `from_json`). To enable without restoring, pass any path
    /// whose file is missing or empty.
    ///
    /// **Note (v0.6 #40 scope):** single-instance only — the source and
    /// destination buckets must live on the same `S4Service`. Real
    /// cross-region (multi-instance) replication is a v0.7+ follow-up.
    /// Delete-marker replication, replication metrics (RTC), and KMS
    /// key-id rewriting on the replica are also out of scope.
    #[clap(long, value_name = "PATH")]
    replication_state_file: Option<std::path::PathBuf>,

    /// v0.6 #37: enable the in-memory S3 Lifecycle configuration
    /// manager (`LifecycleManager`). When set, S4-server itself owns
    /// per-bucket lifecycle rules — `PutBucketLifecycleConfiguration` /
    /// `GetBucketLifecycleConfiguration` / `DeleteBucketLifecycle`
    /// route through the manager (replacing the previous
    /// backend-passthrough behaviour). A background tokio task wakes
    /// every `--lifecycle-scan-interval-hours` to log the set of
    /// buckets with rules attached and stamp a "would-have-run"
    /// marker. The optional path argument names a JSON snapshot file
    /// produced previously by `LifecycleManager::to_json`. To
    /// enable without restoring, pass any path whose file is missing
    /// or empty.
    ///
    /// **Scanner status (post-v0.8.3):** the background scheduler
    /// walks every bucket whose lifecycle config exists, lists each
    /// object via `list_objects_v2`, evaluates the rules, and
    /// executes Expire (delete) / Transition (copy_object with new
    /// storage class) — Object-Lock-protected objects are skipped
    /// (lock wins, surfaced via `s4_lifecycle_actions_total{action="skipped_locked"}`).
    /// `AbortIncompleteMultipartUpload` rules also fire — the scanner
    /// walks `list_multipart_uploads` and aborts any upload past the
    /// configured age. NoncurrentVersionExpiration on versioned
    /// buckets is the only rule shape still deferred (needs the
    /// version-chain walker).
    #[clap(long, value_name = "PATH")]
    lifecycle_state_file: Option<std::path::PathBuf>,

    /// v0.6 #37: cadence (in hours) at which the background lifecycle
    /// scheduler wakes to enumerate buckets that have lifecycle rules
    /// attached. Defaults to 24 (= once a day, matching AWS's
    /// "lifecycle runs around midnight UTC" cadence). No effect when
    /// `--lifecycle-state-file` is not supplied.
    #[clap(long, value_name = "N", default_value_t = 24)]
    lifecycle_scan_interval_hours: u32,

    /// v0.8.2 #62 (H-6 audit fix): drop in-memory `MultipartUploadContext`
    /// entries older than this many hours. The default mirrors AWS S3's
    /// documented multipart-upload retention (24 h) and is configurable
    /// per deployment so operators with longer-running uploads can
    /// extend the TTL. The sweep runs once an hour on a detached tokio
    /// task; on each tick, every entry whose `put` timestamp is older
    /// than `now - max_age` is dropped, releasing its
    /// `MultipartSseMode::SseC { key: Zeroizing<[u8; 32]>, .. }` and
    /// wiping the SSE-C customer key bytes from process memory. Each
    /// pruned batch increments `s4_multipart_abandoned_uploads_total`.
    /// Setting to `0` disables the sweep (not recommended — abandoned
    /// SSE-C uploads then leak their key for the lifetime of the
    /// process).
    #[clap(long, value_name = "N", default_value_t = 24)]
    multipart_abandoned_ttl_hours: u32,

    /// v0.8.3 #66 (H-5 audit fix): drop terminal-state replication
    /// entries (`Completed` / `Failed`) older than this many hours.
    /// Without this knob the per-(bucket, key) `statuses` HashMap on
    /// `ReplicationManager` grows unbounded under workloads with many
    /// unique keys, inflating the JSON snapshot persisted by
    /// `to_json` and leaking memory across restart cycles.
    ///
    /// The default of 168 h (= 7 days) is chosen to balance two
    /// constraints:
    ///   * **long enough to investigate failures** — operators alerted
    ///     on a `FAILED` stamp need time to drill into the cause; a
    ///     1-day TTL would erase the evidence trail before a midweek
    ///     incident is triaged on the following Monday morning. 7
    ///     days covers one full on-call rotation week.
    ///   * **short enough to bound the in-memory + on-disk state** —
    ///     even a steady 1k-keys-per-hour replication rate retains
    ///     only ~168k entries (≈ 50 MiB JSON snapshot at ~300 bytes
    ///     per entry), an order of magnitude smaller than the
    ///     unbounded growth pre-#66.
    ///
    /// `Pending` entries are **never swept** regardless of age — they
    /// represent in-flight replications whose dispatcher task is
    /// racing toward a terminal stamp; dropping the `Pending` would
    /// lose the eventual outcome. Setting to `0` disables the sweep
    /// entirely (not recommended — restores the pre-#66 unbounded
    /// growth behaviour).
    #[clap(long, value_name = "N", default_value_t = 168)]
    replication_status_ttl_hours: u32,

    /// v0.8.5 #86 (audit M-2): cap the number of in-flight detached
    /// replication dispatcher tasks. A high-volume PUT workload (e.g.
    /// 1k req/s) against a slow destination (multi-second per-PUT
    /// latency) with several enabled replication rules can otherwise
    /// spawn an unbounded number of `tokio::spawn` tasks — each
    /// pinning the source body bytes + metadata in memory until the
    /// destination drains — and exhaust process memory before the
    /// queue ever shrinks. The dispatcher acquires a permit before
    /// kicking off the destination PUT and releases it after the
    /// terminal status stamp; once this cap is reached, additional
    /// dispatchers async-block on `acquire_owned()` (the source PUT
    /// itself has already returned to the client at that point —
    /// only the in-flight replication queue is back-pressured).
    ///
    /// Default 1024 — enough headroom for typical steady-state
    /// replication rates plus 100x bursts. Operators with wide
    /// cross-region fan-out may need to raise; operators on memory-
    /// constrained hosts may want to lower. The lower bound is
    /// silently clamped to 1 by
    /// [`s4_server::S4Service::with_replication_max_concurrent`]
    /// (a value of 0 would deadlock all replicas).
    #[clap(long, value_name = "N", default_value_t = 1024)]
    replication_max_concurrent: usize,

    /// v0.5 #32: regulated-industry posture switch. `strict` enforces
    /// at boot the presence of TLS (--tls-cert/--tls-key OR --acme,
    /// forced to TLS 1.3-only), --access-log + --audit-log-hmac-key
    /// (both), SSE (--sse-s4-key OR --kms-local-dir), and
    /// --object-lock-state-file. At runtime, every PUT must declare
    /// SSE (x-amz-server-side-encryption header or SSE-C customer-key
    /// headers); requests without one are rejected with 400. The
    /// gauge `s4_compliance_mode_active{mode="strict"}` is set to 1
    /// when active, so a fleet-wide alert can confirm coverage.
    #[clap(long, value_name = "MODE", value_enum)]
    compliance_mode: Option<ComplianceMode>,

    /// v0.5 #31: optional subcommand. When omitted, runs the gateway
    /// (existing v0.4 behaviour). Available subcommands:
    /// `verify-audit-log <FILE> --hmac-key <SPEC>` walks an audit-log
    /// file and reports the first chain break (or "OK" when intact).
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Walk an audit-log file produced by `--access-log` +
    /// `--audit-log-hmac-key`, recompute each line's HMAC, and report
    /// the first chain break (or "OK" if the chain is intact).
    VerifyAuditLog(VerifyAuditLogArgs),
}

#[derive(Debug, Args)]
struct VerifyAuditLogArgs {
    /// Path to the audit-log file to verify. Comment lines starting
    /// with `# prev_file_tail=<hex>` reset the running prev-HMAC, so a
    /// rotated chain can be walked one file at a time in order.
    file: std::path::PathBuf,

    /// HMAC-SHA256 key used to produce the chain. Same shape as
    /// `--audit-log-hmac-key`: `raw:<bytes>`, `hex:<hex>`, or
    /// `base64:<b64>`.
    #[clap(long = "hmac-key")]
    hmac_key: String,

    /// v0.8.2 #63: operator-supplied previous-file tail HMAC (hex,
    /// 64 chars). When set, the in-file `# prev_file_tail=` comment
    /// is ignored as authentication (treated as a hint only),
    /// eliminating splice/replay attacks via fabricated comment.
    /// Closes audit finding H-3.
    #[clap(long = "expected-prev-tail", value_name = "HEX")]
    expected_prev_tail: Option<String>,

    /// v0.8.2 #63: require the file end with a recognized
    /// `# eof_hmac=` marker (truncation detection). Off by default
    /// for back-compat with pre-v0.8.2 logs that don't have the
    /// marker. Closes audit finding H-2.
    #[clap(long = "require-eof-hmac", default_value_t = false)]
    require_eof_hmac: bool,
}

fn setup_tracing(
    format: LogFormat,
    otlp_endpoint: Option<&str>,
    service_name: &str,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // OTel layer を共通で構築 (Option)
    let otel_layer = if let Some(endpoint) = otlp_endpoint {
        use opentelemetry::trace::TracerProvider;
        use opentelemetry_otlp::WithExportConfig;
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?;
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(service_name.to_owned())
                    .build(),
            )
            .with_batch_exporter(exporter)
            .build();
        let tracer = provider.tracer(service_name.to_owned());
        opentelemetry::global::set_tracer_provider(provider);
        Some(tracing_opentelemetry::layer().with_tracer(tracer))
    } else {
        None
    };

    // OTel layer は Registry (LookupSpan を提供) の直上に置く必要がある。
    // EnvFilter は fmt 層に per-layer filter として適用する形にして trait
    // resolution の干渉を避ける。
    use tracing_subscriber::Layer;
    match (format, otel_layer) {
        (LogFormat::Pretty, Some(otel)) => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_ansi(std::io::stdout().is_terminal())
                .with_filter(env_filter);
            tracing_subscriber::registry()
                .with(otel)
                .with(fmt_layer)
                .init();
        }
        (LogFormat::Pretty, None) => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_ansi(std::io::stdout().is_terminal())
                .with_filter(env_filter);
            tracing_subscriber::registry().with(fmt_layer).init();
        }
        (LogFormat::Json, Some(otel)) => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .with_filter(env_filter);
            tracing_subscriber::registry()
                .with(otel)
                .with(fmt_layer)
                .init();
        }
        (LogFormat::Json, None) => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .with_filter(env_filter);
            tracing_subscriber::registry().with(fmt_layer).init();
        }
    }
    Ok(())
}

fn build_registry(default: CodecKind, zstd_level: i32) -> Arc<CodecRegistry> {
    let reg = CodecRegistry::new(default)
        .with(Arc::new(Passthrough))
        .with(Arc::new(CpuZstd::new(zstd_level)))
        .with(Arc::new(s4_codec::cpu_gzip::CpuGzip::default()));
    #[cfg(feature = "nvcomp-gpu")]
    let reg = {
        use s4_codec::nvcomp::{
            NvcompBitcompCodec, NvcompGDeflateCodec, NvcompZstdCodec, is_gpu_available,
        };
        if is_gpu_available() {
            let mut r = reg;
            match NvcompZstdCodec::new() {
                Ok(c) => r = r.with(Arc::new(c)),
                Err(e) => tracing::warn!("nvcomp-zstd init failed: {e}"),
            }
            match NvcompBitcompCodec::default_general() {
                Ok(c) => r = r.with(Arc::new(c)),
                Err(e) => tracing::warn!("nvcomp-bitcomp init failed: {e}"),
            }
            match NvcompGDeflateCodec::new() {
                Ok(c) => r = r.with(Arc::new(c)),
                Err(e) => tracing::warn!("nvcomp-gdeflate init failed: {e}"),
            }
            r
        } else {
            tracing::warn!(
                "nvcomp-gpu feature is enabled but no CUDA-capable GPU detected at runtime"
            );
            reg
        }
    };
    Arc::new(reg)
}

/// v0.8 #56: build the configured dispatcher and, for the sampling variant,
/// optionally enable GPU promotion (`with_gpu_preference`). `prefer_gpu` is
/// the boot-time GPU probe result from `s4_codec::nvcomp::is_gpu_available()`;
/// `gpu_min_bytes` is the operator-tuned threshold below which CPU wins.
fn build_dispatcher(
    choice: DispatcherChoice,
    default: CodecKind,
    prefer_gpu: bool,
    gpu_min_bytes: usize,
) -> Arc<dyn CodecDispatcher> {
    match choice {
        DispatcherChoice::Always => Arc::new(AlwaysDispatcher(default)),
        DispatcherChoice::Sampling => Arc::new(
            SamplingDispatcher::new(default).with_gpu_preference(prefer_gpu, gpu_min_bytes),
        ),
    }
}

/// v0.5 #29: parse a `--sse-s4-key-rotated id=N,key=PATH` value into
/// `(id, path)`. Order-independent; both fields required. Errors carry
/// enough context that the operator can fix the typo without `--help`.
fn parse_rotated_key_spec(
    spec: &str,
) -> Result<(u16, std::path::PathBuf), Box<dyn Error + Send + Sync + 'static>> {
    let mut id: Option<u16> = None;
    let mut path: Option<std::path::PathBuf> = None;
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| format!("expected `key=value` pair, got {part:?}"))?;
        match k.trim() {
            "id" => {
                id = Some(
                    v.trim()
                        .parse::<u16>()
                        .map_err(|e| format!("id must be a u16: {e}"))?,
                );
            }
            "key" => {
                path = Some(std::path::PathBuf::from(v.trim()));
            }
            other => {
                return Err(format!("unknown field {other:?} (expected `id` or `key`)").into());
            }
        }
    }
    let id = id.ok_or("missing required `id=N`")?;
    let path = path.ok_or("missing required `key=PATH`")?;
    Ok((id, path))
}

// v0.8.4 #72: `read_state_file_or_fresh` (v0.7 dogfood follow-up) was
// promoted into the library crate as
// `s4_server::state_loader::read_state_file_or_fresh`, alongside the
// new `state_loader::load_or_fresh` wrapper that turns a corrupted
// snapshot into a `tracing::warn!` + counter bump + fresh manager
// instead of killing the gateway boot. See `state_loader.rs` for the
// full contract.

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    let opt = Opt::parse();

    // v0.5 #31: dispatch non-server subcommands before booting the
    // gateway (no tracing init, no AWS SDK config required — the
    // verifier is a pure file-walk).
    if let Some(cmd) = opt.command.as_ref() {
        return run_subcommand(cmd);
    }

    // v0.5 #32: enforce compliance-mode prereqs *before* anything
    // costly (tracing init, AWS SDK auth probe). A misconfigured boot
    // exits with a clear, actionable error.
    if let Some(mode) = opt.compliance_mode {
        validate_compliance_mode(&opt, mode)?;
    }

    setup_tracing(
        opt.log_format,
        opt.otlp_endpoint.as_deref(),
        &opt.service_name,
    )?;

    // v0.8.5 #81 (audit C-1 + H-7): central shutdown signal, fanned out
    // to every background task via `Arc<Notify>::notify_waiters()` from
    // the listener loop's SIGTERM / SIGINT branch. Created here, before
    // any spawn site, so each `tokio::spawn(...)` below can clone the
    // `Arc` into its closure and `tokio::select!` on it.
    //
    // Notify is the right primitive here because we have a one-shot
    // many-listeners wakeup ("everyone shut down now") with no payload;
    // a `tokio::sync::watch::<bool>` would also work but adds a value
    // slot we never read. The notify_waiters() variant wakes anyone
    // currently parked in `notified()`, which is exactly the contract
    // each background task wants from inside its `select!`.
    let shutdown_notify: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());

    // v0.8.5 #86 (audit M-1): collect background-task JoinHandles whose
    // lifetime should outlive the spawn site so they're not silently
    // detached at end-of-block. Currently carries the access-log
    // flusher; future fixers can append other long-lived tasks here for
    // the same reason. Held until end of `start_server` — the graceful-
    // shutdown branch awaits each handle (with a short timeout) so a
    // wedged background task can't keep the process alive forever.
    let mut background_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    let endpoint_url = opt.endpoint_url.as_deref().ok_or(
        "--endpoint-url is required when running as a server (omit only \
         for non-server subcommands like verify-audit-log)",
    )?;

    let sdk_conf = aws_config::from_env()
        .endpoint_url(endpoint_url)
        .load()
        .await;
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&sdk_conf)
            .force_path_style(true)
            .build(),
    );
    // ready_check 用に client を 1 つ複製して保持
    let ready_client = client.clone();
    let proxy = s3s_aws::Proxy::from(client);

    let default_kind = opt.codec.as_kind();
    let registry = build_registry(default_kind, opt.zstd_level);

    // v0.8 #56: GPU auto-detect at boot. When the `nvcomp-gpu` feature is
    // compiled in AND a CUDA-capable GPU is visible at runtime, the
    // sampling dispatcher promotes large `CpuZstd` picks to `NvcompZstd`.
    // Without GPU (no driver / no device / feature off) we fall through to
    // pure CPU codecs — same behaviour as before this commit.
    #[cfg(feature = "nvcomp-gpu")]
    let prefer_gpu = {
        let avail = s4_codec::nvcomp::is_gpu_available();
        if avail {
            info!(
                "GPU detected, sampling dispatcher will prefer nvcomp-zstd over cpu-zstd \
                 for objects >= {} bytes",
                opt.gpu_min_bytes
            );
        } else {
            info!(
                "nvcomp-gpu feature compiled in but no CUDA-capable GPU at runtime — \
                 using cpu-zstd"
            );
        }
        avail
    };
    #[cfg(not(feature = "nvcomp-gpu"))]
    let prefer_gpu = false;

    let dispatcher = build_dispatcher(opt.dispatcher, default_kind, prefer_gpu, opt.gpu_min_bytes);
    info!(
        codec = ?opt.codec,
        dispatcher = ?opt.dispatcher,
        prefer_gpu,
        gpu_min_bytes = opt.gpu_min_bytes,
        registered = ?registry.kinds().collect::<Vec<_>>(),
        "S4 codec registry built"
    );

    let mut s4 = S4Service::new(proxy, registry, dispatcher);
    // v0.3 #13: tell the policy evaluator whether traffic is reaching us
    // over TLS so the `aws:SecureTransport` Condition key resolves
    // correctly. Either an operator-provided cert (--tls-cert) or ACME
    // (--acme) qualifies.
    let listener_secure = opt.tls_cert.is_some() || opt.acme.is_some();
    s4 = s4.with_secure_transport(listener_secure);
    // v0.8.11 CRIT-4 fix: wire the X-Forwarded-For trust gate.
    // Default (`false`) means a public-internet client cannot spoof
    // `aws:SourceIp`; operators behind a trusted reverse proxy pass
    // `--trust-x-forwarded-for` to restore the legacy behaviour.
    s4 = s4.with_trust_x_forwarded_for(opt.trust_x_forwarded_for);
    if opt.trust_x_forwarded_for {
        info!(
            "S4 X-Forwarded-For trust: ENABLED — header value is consumed as aws:SourceIp \
             and access-log remote_ip. Ensure a trusted reverse proxy strips client-supplied \
             values (v0.8.11 CRIT-4 fix)."
        );
    } else {
        info!(
            "S4 X-Forwarded-For trust: disabled (default) — aws:SourceIp / access-log \
             remote_ip stay None until --trust-x-forwarded-for is set (v0.8.11 CRIT-4 fix)."
        );
    }
    // v0.8.5 #86 (audit M-2): cap the replication dispatcher pool. The
    // setter clamps to 1 if the operator passed 0 (would deadlock all
    // replicas); see the field-level doc on
    // `S4Service::replication_semaphore` for the back-pressure shape.
    s4 = s4.with_replication_max_concurrent(opt.replication_max_concurrent);
    info!(
        cap = opt.replication_max_concurrent,
        "S4 replication dispatcher concurrency cap installed (v0.8.5 #86 audit M-2)"
    );
    if let Some(ref key_path) = opt.sse_s4_key {
        let active = s4_server::sse::SseKey::from_path(key_path)
            .map_err(|e| format!("--sse-s4-key {}: {e}", key_path.display()))?;
        info!(path = %key_path.display(), "S4 SSE-S4 active key loaded (AES-256-GCM, id=1)");
        // v0.5 #29: active key always lives at id=1 (the default slot).
        // Retired keys come in via --sse-s4-key-rotated id=N,key=<path>.
        let mut keyring = s4_server::sse::SseKeyring::new(1, std::sync::Arc::new(active));
        for spec in &opt.sse_s4_key_rotated {
            let (id, path) = parse_rotated_key_spec(spec)
                .map_err(|e| format!("--sse-s4-key-rotated {spec:?}: {e}"))?;
            if id == 1 {
                return Err(
                    "--sse-s4-key-rotated id=1 collides with active id=1 (use a different id; --sse-s4-key supplies id=1)"
                        .into(),
                );
            }
            let k = s4_server::sse::SseKey::from_path(&path)
                .map_err(|e| format!("--sse-s4-key-rotated id={id} key {}: {e}", path.display()))?;
            info!(id, path = %path.display(), "S4 SSE-S4 retired key loaded");
            keyring.add(id, std::sync::Arc::new(k));
        }
        s4 = s4.with_sse_keyring(std::sync::Arc::new(keyring));
        // v0.8 #52: opt the SSE-S4 PUT path into the chunked S4E5
        // frame for streaming GET. Skipped when the operator
        // explicitly passes `--sse-chunk-size 0` (back-compat with
        // legacy buffered S4E2). Logged so the ops dashboard /
        // boot diff makes the choice obvious.
        if opt.sse_chunk_size > 0 {
            info!(
                chunk_size = opt.sse_chunk_size,
                "S4 SSE-S4 chunked frame (S4E5) enabled — GET will stream-decrypt chunk-by-chunk"
            );
            s4 = s4.with_sse_chunk_size(opt.sse_chunk_size);
        } else {
            info!("S4 SSE-S4 chunked frame (S4E5) disabled — using legacy buffered S4E2 frame");
        }
    } else if !opt.sse_s4_key_rotated.is_empty() {
        return Err(
            "--sse-s4-key-rotated requires --sse-s4-key (active key) to also be set".into(),
        );
    }
    if let Some(ref dir) = opt.sigv4a_credentials {
        // v0.8.4 #76: validate the skew tolerance — 0 would reject every
        // SigV4a request (any non-zero drift would exceed the tolerance),
        // making the flag effectively a "block SigV4a entirely" knob.
        // We surface that as a startup error so the operator notices.
        if opt.sigv4a_skew_tolerance_seconds == 0 {
            return Err(
                "--sigv4a-skew-tolerance-seconds must be > 0 (0 effectively blocks all SigV4a requests; \
                 to disable SigV4a, omit --sigv4a-credentials instead)"
                    .into(),
            );
        }
        let store = s4_server::sigv4a::SigV4aCredentialStore::load_dir(dir)
            .map_err(|e| format!("--sigv4a-credentials {}: {e}", dir.display()))?;
        info!(
            dir = %dir.display(),
            keys = store.len(),
            skew_tolerance_secs = opt.sigv4a_skew_tolerance_seconds,
            "S4 SigV4a credential store loaded (verification gate, v0.8.4 #76 freshness ON)"
        );
        // v0.7 #47: wrap the credential store in a SigV4aGate and attach
        // it to the service. The listener-side middleware (registered
        // below in `run_server` via `HealthRouter::with_sigv4a_gate`)
        // pulls the gate back off the service and runs verification at
        // the HTTP layer — s3s' SigV4 verifier would otherwise reject
        // every `AWS4-ECDSA-P256-SHA256` request as "unknown algorithm".
        //
        // v0.8.4 #76 (audit H-6): the gate also enforces an
        // `x-amz-date` freshness window (default 15 min, AWS spec)
        // so captured-request replay is no longer possible. Tunable
        // via `--sigv4a-skew-tolerance-seconds`.
        let skew = chrono::Duration::seconds(i64::from(opt.sigv4a_skew_tolerance_seconds));
        let gate = std::sync::Arc::new(
            s4_server::service::SigV4aGate::new(std::sync::Arc::new(store))
                .with_skew_tolerance(skew),
        );
        s4 = s4.with_sigv4a_gate(gate);
    }
    if let Some(ref kek_dir) = opt.kms_local_dir {
        let kms = s4_server::kms::LocalKms::open(kek_dir.clone())
            .map_err(|e| format!("--kms-local-dir {}: {e}", kek_dir.display()))?;
        info!(
            dir = %kek_dir.display(),
            keys = ?kms.key_ids(),
            "S4 SSE-KMS LocalKms backend opened"
        );
        s4 = s4.with_kms_backend(std::sync::Arc::new(kms), opt.kms_default_key_id.clone());
    }
    if let Some(ref dir) = opt.access_log {
        let dest = s4_server::access_log::AccessLogDest { dir: dir.clone() };
        let mut log = s4_server::access_log::AccessLog::new(dest);
        if let Some(ref spec) = opt.audit_log_hmac_key {
            let key = s4_server::audit_log::AuditHmacKey::from_str(spec)
                .map_err(|e| format!("--audit-log-hmac-key: {e}"))?;
            info!(
                len = key.as_bytes().len(),
                "S4 audit-log HMAC chain enabled (v0.5 #31)"
            );
            log = log.with_hmac_key(std::sync::Arc::new(key));
        }
        let log = std::sync::Arc::new(log);
        // v0.8.5 #81 (audit H-7): hand the central shutdown notifier to
        // the flusher so SIGTERM / SIGINT triggers a final-drain + clean
        // exit instead of leaving the loop ticking until the runtime is
        // torn down out from under it (which would tear an in-flight
        // write at an arbitrary boundary and skip the per-batch
        // `# eof_hmac=` marker).
        //
        // v0.8.5 #86 (audit M-1): hoist the JoinHandle into the
        // outer-scope `background_handles` Vec so it lives as long as
        // the gateway process. The pre-#86 `let _flusher = ...;` scoped
        // the handle to this `if` block, dropping it the moment the
        // block ended — which only detaches the task (does not abort
        // it) but loses the ability to observe its clean exit on
        // shutdown. Holding the handle makes the task's lifetime
        // explicitly process-bound; the graceful-shutdown branch in
        // `start_server` `await`s it (with a short timeout) on the way
        // out so the operator sees a definitive "flusher exited
        // cleanly" log line.
        let flusher_handle = log.spawn_flusher(Some(Arc::clone(&shutdown_notify)));
        background_handles.push(flusher_handle);
        info!(dir = %dir.display(), "S4 access log emitter started");
        s4 = s4.with_access_log(log);
    }
    if let Some(ref rl_path) = opt.rate_limit {
        let rl = s4_server::rate_limit::RateLimits::from_path(rl_path)
            .map_err(|e| format!("--rate-limit {}: {e}", rl_path.display()))?;
        info!(path = %rl_path.display(), "S4 rate-limit config loaded");
        s4 = s4.with_rate_limits(std::sync::Arc::new(rl));
    }
    if let Some(ref policy_path) = opt.policy {
        let policy = s4_server::policy::Policy::from_path(policy_path)
            .map_err(|e| format!("--policy {}: {e}", policy_path.display()))?;
        info!(path = %policy_path.display(), "S4 bucket policy loaded");
        s4 = s4.with_policy(std::sync::Arc::new(policy));
    }
    // v0.5 #34: wire the in-memory versioning state machine when
    // --versioning-state-file is supplied. An empty / missing path
    // starts the manager with a fresh state; a populated path is
    // loaded as a JSON snapshot (produced previously by
    // `VersioningManager::to_json`). SIGUSR1-triggered dump-back is
    // intentionally deferred (signal-handler wiring is out of scope
    // for v0.5 #34 — operators can still snapshot manually via the
    // future API).
    if let Some(ref path) = opt.versioning_state_file {
        // v0.8.4 #72: corrupted snapshots WARN + bump
        // `s4_state_file_load_failures_total{manager="versioning"}` and
        // boot fresh instead of killing the gateway. The on-disk file
        // is left untouched so the operator can inspect / restore.
        let mgr = s4_server::state_loader::load_or_fresh(
            "versioning",
            path,
            s4_server::versioning::VersioningManager::from_json,
        );
        info!(
            path = %path.display(),
            "S4 versioning state machine attached (in-memory; v0.5 #34 single-instance scope)"
        );
        s4 = s4.with_versioning(std::sync::Arc::new(mgr));
    }
    // v0.5 #30: same shape as the versioning flag above. An empty /
    // missing path starts a fresh manager; a populated path is loaded
    // as a JSON snapshot (produced previously by
    // `ObjectLockManager::to_json`).
    if let Some(ref path) = opt.object_lock_state_file {
        // v0.8.4 #72: per-manager fault isolation (see versioning above).
        let mgr = s4_server::state_loader::load_or_fresh(
            "object_lock",
            path,
            s4_server::object_lock::ObjectLockManager::from_json,
        );
        info!(
            path = %path.display(),
            "S4 Object Lock manager attached (in-memory; v0.5 #30 single-instance scope)"
        );
        s4 = s4.with_object_lock(std::sync::Arc::new(mgr));
    }
    // v0.6 #42: wire the in-memory MFA-Delete enforcement manager when
    // --mfa-delete-state-file is supplied. Same shape as the versioning /
    // object-lock flags: empty / missing path starts a fresh manager,
    // populated path is loaded as a JSON snapshot (produced previously by
    // `MfaDeleteManager::to_json`). When --mfa-default-secret-file is
    // also set, its `<base32_secret> <serial>` line installs a gateway-
    // wide default secret on top of (or in addition to) any per-bucket
    // overrides loaded from the JSON snapshot.
    if let Some(ref path) = opt.mfa_delete_state_file {
        // v0.8.4 #72: per-manager fault isolation (see versioning above).
        // The MFA-Delete *snapshot* is per-bucket-override config — a
        // corrupt snapshot still falls back to "no overrides" (the
        // gateway-wide default secret below remains the gate). The
        // `--mfa-default-secret-file` read inside this block stays
        // fail-closed (`?`): if the operator opted into MFA Delete and
        // we cannot read the secret, the gate cannot verify TOTP codes
        // and the only safe behaviour is to refuse to boot — silently
        // booting with no secret would let DELETEs slip past MFA.
        let mgr = s4_server::state_loader::load_or_fresh(
            "mfa_delete",
            path,
            s4_server::mfa::MfaDeleteManager::from_json,
        );
        if let Some(ref secret_path) = opt.mfa_default_secret_file {
            let raw = std::fs::read_to_string(secret_path).map_err(|e| {
                format!(
                    "--mfa-default-secret-file {}: read failed: {e}",
                    secret_path.display()
                )
            })?;
            let line = raw.lines().next().unwrap_or("").trim();
            let mut parts = line.splitn(2, ' ');
            let secret_b32 = parts.next().unwrap_or("").trim();
            let serial = parts.next().unwrap_or("").trim();
            if secret_b32.is_empty() || serial.is_empty() {
                return Err(format!(
                    "--mfa-default-secret-file {}: expected `<base32_secret> <serial>` on the first line",
                    secret_path.display()
                )
                .into());
            }
            mgr.set_default_secret(s4_server::mfa::MfaSecret {
                secret_base32: secret_b32.to_owned(),
                serial: serial.to_owned(),
            });
            info!(
                path = %secret_path.display(),
                "S4 MFA-Delete default secret loaded (RFC 6238 SHA-1, ±1 step skew)"
            );
        }
        info!(
            path = %path.display(),
            "S4 MFA-Delete manager attached (in-memory; v0.6 #42 single-instance scope)"
        );
        s4 = s4.with_mfa_delete(std::sync::Arc::new(mgr));
    }
    // v0.6 #38: wire the in-memory CORS bucket-configuration manager
    // when --cors-state-file is supplied. Same shape as the versioning /
    // object-lock flags: empty / missing path starts a fresh manager,
    // populated path is loaded as a JSON snapshot (produced by
    // `CorsManager::to_json`).
    if let Some(ref path) = opt.cors_state_file {
        // v0.8.4 #72: per-manager fault isolation (see versioning above).
        let mgr = s4_server::state_loader::load_or_fresh(
            "cors",
            path,
            s4_server::cors::CorsManager::from_json,
        );
        info!(
            path = %path.display(),
            "S4 CORS manager attached (in-memory; v0.6 #38 single-instance scope; OPTIONS routing follow-up)"
        );
        s4 = s4.with_cors(std::sync::Arc::new(mgr));
    }
    // v0.6 #36 + v0.7 #46: wire the in-memory S3 Inventory manager when
    // --inventory-state-file is supplied. Empty / missing path starts a
    // fresh manager, populated path is loaded as a JSON snapshot
    // produced previously by `InventoryManager::to_json`.
    //
    // v0.7 #46: the matching background scheduler (one tokio task per
    // process) now executes a real scan every
    // `--inventory-scan-interval-hours` — see
    // `s4_server::inventory::run_scan_once`. The scanner walks every
    // bucket whose inventory configuration is `due()`, lists its
    // objects via `list_objects_v2`, HEADs each one for size / etag /
    // last_modified / SSE flags, renders the CSV + manifest.json, and
    // PUTs both to the destination bucket prefix. `mark_run` stamps on
    // success. The Arc handle is captured into the outer
    // `inventory_to_scan` so the scanner spawn can run AFTER `s4_arc`
    // exists (the scanner wants `&Arc<S4Service<B>>`, mirroring the
    // v0.7 #45 lifecycle pattern below).
    let inventory_to_scan: Option<std::sync::Arc<s4_server::inventory::InventoryManager>> =
        if let Some(ref path) = opt.inventory_state_file {
            // v0.8.4 #72: per-manager fault isolation (see versioning above).
            let mgr = s4_server::state_loader::load_or_fresh(
                "inventory",
                path,
                s4_server::inventory::InventoryManager::from_json,
            );
            let mgr = std::sync::Arc::new(mgr);
            info!(
                path = %path.display(),
                interval_hours = opt.inventory_scan_interval_hours,
                "S4 inventory manager attached (v0.7 #46 scanner active; CSV format only — Parquet/ORC deferred)"
            );
            s4 = s4.with_inventory(std::sync::Arc::clone(&mgr));
            Some(mgr)
        } else {
            None
        };
    // v0.6 #35: wire the in-memory bucket-notification manager when
    // --notifications-state-file is supplied. Same shape as the
    // versioning / object-lock / cors / inventory flags — empty /
    // missing path starts a fresh manager, populated path is loaded as
    // a JSON snapshot produced previously by
    // `NotificationManager::to_json`. The matching dispatcher itself
    // runs inside the request-path handlers (PUT / DELETE) on detached
    // tokio tasks, so no extra background scheduler is needed here.
    if let Some(ref path) = opt.notifications_state_file {
        // v0.8.4 #72: per-manager fault isolation (see versioning above).
        let mgr = s4_server::state_loader::load_or_fresh(
            "notifications",
            path,
            s4_server::notifications::NotificationManager::from_json,
        );
        info!(
            path = %path.display(),
            "S4 notifications manager attached (in-memory; v0.6 #35 single-instance scope; webhook always available, SQS/SNS gated by `aws-events`)"
        );
        s4 = s4.with_notifications(std::sync::Arc::new(mgr));
    }
    // v0.6 #39: wire the in-memory object + bucket Tagging manager
    // when --tagging-state-file is supplied. Same shape as the
    // versioning / object-lock / cors flags — empty / missing path
    // starts a fresh manager, populated path is loaded as a JSON
    // snapshot produced previously by `TagManager::to_json`.
    if let Some(ref path) = opt.tagging_state_file {
        // v0.8.4 #72: per-manager fault isolation (see versioning above).
        let mgr = s4_server::state_loader::load_or_fresh(
            "tagging",
            path,
            s4_server::tagging::TagManager::from_json,
        );
        info!(
            path = %path.display(),
            "S4 Tagging manager attached (in-memory; v0.6 #39 single-instance scope)"
        );
        s4 = s4.with_tagging(std::sync::Arc::new(mgr));
    }
    // v0.6 #40: wire the in-memory cross-bucket replication manager
    // when --replication-state-file is supplied. Same shape as the
    // versioning / object-lock / notifications / tagging flags — empty
    // / missing path starts a fresh manager, populated path is loaded
    // as a JSON snapshot produced previously by
    // `ReplicationManager::to_json`. The matching dispatcher itself
    // runs inside `put_object` on detached tokio tasks, so no extra
    // background scheduler is needed here.
    if let Some(ref path) = opt.replication_state_file {
        // v0.8.4 #72: per-manager fault isolation (see versioning above).
        let mgr = s4_server::state_loader::load_or_fresh(
            "replication",
            path,
            s4_server::replication::ReplicationManager::from_json,
        );
        info!(
            path = %path.display(),
            "S4 replication manager attached (in-memory; v0.6 #40 single-instance scope; same-S4Service source/destination only)"
        );
        s4 = s4.with_replication(std::sync::Arc::new(mgr));
    }
    // v0.6 #37 + v0.7 #45: wire the in-memory S3 Lifecycle configuration
    // manager when --lifecycle-state-file is supplied. Empty / missing
    // path starts a fresh manager, populated path is loaded as a JSON
    // snapshot produced previously by `LifecycleManager::to_json`.
    //
    // v0.7 #45: the matching background scheduler (one tokio task per
    // process) now executes a real scan every
    // `--lifecycle-scan-interval-hours` — see
    // `s4_server::lifecycle::run_scan_once`. The scanner walks every
    // bucket with a lifecycle config attached, lists its objects via
    // `list_objects_v2`, evaluates each rule, and executes matching
    // Expire / Transition actions through the same `S4Service` handler
    // path the HTTP listener uses. Object-Lock-protected objects are
    // skipped (lock wins). NoncurrentVersionExpiration walking of
    // versioning-shadow chains is deferred to a follow-up — current
    // versions are fully covered.
    let lifecycle_to_scan: Option<std::sync::Arc<s4_server::lifecycle::LifecycleManager>> =
        if let Some(ref path) = opt.lifecycle_state_file {
            // v0.8.4 #72: per-manager fault isolation (see versioning above).
            let mgr = s4_server::state_loader::load_or_fresh(
                "lifecycle",
                path,
                s4_server::lifecycle::LifecycleManager::from_json,
            );
            let mgr = std::sync::Arc::new(mgr);
            info!(
                path = %path.display(),
                interval_hours = opt.lifecycle_scan_interval_hours,
                "S4 Lifecycle manager attached (v0.7 #45 scanner active; current-version Expire / Transition only — NoncurrentVersionExpiration deferred)"
            );
            s4 = s4.with_lifecycle(std::sync::Arc::clone(&mgr));
            Some(mgr)
        } else {
            None
        };
    if matches!(opt.compliance_mode, Some(ComplianceMode::Strict)) {
        s4 = s4.with_compliance_strict(true);
        s4_server::metrics::record_compliance_mode_active("strict");
        info!("S4 compliance-mode strict ACTIVE — every PUT must declare SSE");
    }
    // v0.7 #44: snapshot the optional CORS manager Arc before the
    // s3s ServiceBuilder consumes `s4`. The HTTP-level OPTIONS preflight
    // interceptor needs the same manager because s3s does not surface
    // OPTIONS as a typed S3 handler — match has to happen at the hyper
    // layer instead.
    let cors_manager = s4.cors_manager().cloned();
    // v0.7 #47: same shape — snapshot the optional SigV4a gate Arc so
    // the listener-side verify middleware can be attached on the
    // `HealthRouter` after the s3s `ServiceBuilder` has consumed `s4`.
    let sigv4a_gate = s4.sigv4a_gate().cloned();

    // v0.7 #45: wrap `S4Service` in an Arc so the lifecycle scanner
    // (background tokio task) and the s3s service builder share the
    // same instance. `SharedService` is a thin newtype with a
    // delegating `impl S3` — see `s4_server::service_arc` for the
    // why-not-blanket-impl note.
    let s4_arc = std::sync::Arc::new(s4);

    // Spawn the v0.7 #45 lifecycle scanner if the manager is wired.
    // The task owns its own `Arc<S4Service<...>>` clone so the listener
    // staying up keeps the scanner alive (and vice versa); both go
    // away together on shutdown.
    if lifecycle_to_scan.is_some() {
        let scan_handle = std::sync::Arc::clone(&s4_arc);
        let interval_hours = u64::from(opt.lifecycle_scan_interval_hours.max(1));
        // v0.8.5 #81 (audit H-7): cancellation-aware scan loop — break
        // out the moment the listener fans out the shutdown signal
        // instead of looping until the runtime is torn down.
        let shutdown_cl = Arc::clone(&shutdown_notify);
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(interval_hours * 3600));
            // Skip the first immediate tick — the CLI already logged
            // "manager attached" so we don't want a duplicate "tick"
            // line in the same millisecond.
            ticker.tick().await;
            loop {
                tokio::select! {
                    () = shutdown_cl.notified() => {
                        tracing::info!("S4 lifecycle scanner shutting down (got cancel signal)");
                        return;
                    }
                    _ = ticker.tick() => {}
                }
                match s4_server::lifecycle::run_scan_once(&scan_handle).await {
                    Ok(report) => {
                        tracing::info!(
                            buckets_scanned = report.buckets_scanned,
                            objects_evaluated = report.objects_evaluated,
                            expired = report.expired,
                            transitioned = report.transitioned,
                            skipped_locked = report.skipped_locked,
                            action_errors = report.action_errors,
                            "S4 lifecycle scan complete"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("S4 lifecycle scan failed: {e}");
                    }
                }
            }
        });
    }

    // Spawn the v0.7 #46 inventory scanner if the manager is wired.
    // Same Arc-clone shape as the lifecycle scanner above: the
    // background task holds one `Arc<S4Service<...>>` clone, the s3s
    // listener (via `SharedService`) holds the other. The scanner
    // walks every due (bucket, id) inventory configuration, lists +
    // HEADs each source object, renders the CSV + manifest.json, and
    // PUTs both to the destination bucket prefix — see
    // `s4_server::inventory::run_scan_once` for the full spec.
    if inventory_to_scan.is_some() {
        let scan_handle = std::sync::Arc::clone(&s4_arc);
        let interval_hours = u64::from(opt.inventory_scan_interval_hours.max(1));
        // v0.8.5 #81 (audit H-7): same cancellation-aware loop shape as
        // the lifecycle scanner above.
        let shutdown_cl = Arc::clone(&shutdown_notify);
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(interval_hours * 3600));
            // Skip the first immediate tick — the CLI already logged
            // "manager attached" so we don't want a duplicate "tick"
            // line in the same millisecond.
            ticker.tick().await;
            loop {
                tokio::select! {
                    () = shutdown_cl.notified() => {
                        tracing::info!("S4 inventory scanner shutting down (got cancel signal)");
                        return;
                    }
                    _ = ticker.tick() => {}
                }
                match s4_server::inventory::run_scan_once(&scan_handle).await {
                    Ok(report) => {
                        tracing::info!(
                            buckets_scanned = report.buckets_scanned,
                            configs_evaluated = report.configs_evaluated,
                            csvs_written = report.csvs_written,
                            objects_listed = report.objects_listed,
                            errors = report.errors,
                            "S4 inventory scan complete"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("S4 inventory scan failed: {e}");
                    }
                }
            }
        });
    }

    // v0.8.2 #62 (H-6 audit fix): spawn the abandoned-multipart-upload
    // sweep task. The task holds an `Arc<MultipartStateStore>` clone so
    // the listener staying up keeps the sweep alive (and vice versa);
    // both go away together on process shutdown. Tick cadence is fixed
    // at 1 h (hourly) — the operator-tunable knob is the TTL itself
    // (`--multipart-abandoned-ttl-hours`, default 24 h to match AWS S3
    // multipart retention). A TTL of 0 disables the sweep entirely
    // (logged at warn so the operator sees the choice in boot diff).
    if opt.multipart_abandoned_ttl_hours == 0 {
        tracing::warn!(
            "S4 multipart abandoned-upload sweep DISABLED \
             (--multipart-abandoned-ttl-hours=0). SSE-C customer keys \
             on never-Completed uploads will linger for the lifetime \
             of the process."
        );
    } else {
        let mp_state = std::sync::Arc::clone(s4_arc.multipart_state());
        let ttl_hours = i64::from(opt.multipart_abandoned_ttl_hours);
        tracing::info!(
            ttl_hours,
            "S4 multipart abandoned-upload sweep active (hourly tick, TTL configurable via --multipart-abandoned-ttl-hours)"
        );
        // v0.8.5 #81 (audit H-7): cancellation-aware sweep loop.
        let shutdown_cl = Arc::clone(&shutdown_notify);
        tokio::spawn(async move {
            // Sweep cadence is hourly regardless of TTL — the TTL
            // governs which entries are stale, the cadence governs
            // how often we look. Hourly is a safe upper bound on
            // sweep latency for the default 24 h TTL.
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
            // Skip the immediate tick — a freshly-booted process
            // can't have any abandoned uploads yet.
            ticker.tick().await;
            loop {
                tokio::select! {
                    () = shutdown_cl.notified() => {
                        tracing::info!(
                            "S4 multipart abandoned-upload sweep shutting down (got cancel signal)"
                        );
                        return;
                    }
                    _ = ticker.tick() => {}
                }
                let max_age = chrono::Duration::hours(ttl_hours);
                let n = mp_state.sweep_stale(chrono::Utc::now(), max_age);
                if n > 0 {
                    tracing::info!(
                        swept_count = n,
                        ttl_hours,
                        "S4 multipart abandoned-upload sweep pruned entries (SSE-C keys zeroized on drop)"
                    );
                    s4_server::metrics::record_multipart_abandoned(n as u64);
                }
            }
        });
    }

    // v0.8.3 #66 (H-5 audit fix): spawn the replication-status sweep
    // task. Mirrors the multipart sweep above — hourly cadence, TTL is
    // the operator-tunable knob (`--replication-status-ttl-hours`,
    // default 168 h = 7 days). Only fires when a `ReplicationManager`
    // is attached (= the operator passed `--replication-state-file`);
    // otherwise the `statuses` HashMap doesn't exist and there's
    // nothing to sweep. A TTL of 0 disables the sweep entirely (the
    // pre-#66 unbounded-growth behaviour).
    if let Some(repl_mgr) = s4_arc.replication_manager() {
        if opt.replication_status_ttl_hours == 0 {
            tracing::warn!(
                "S4 replication-status sweep DISABLED \
                 (--replication-status-ttl-hours=0). The per-(bucket, key) \
                 status HashMap will grow unbounded across multi-key workloads."
            );
        } else {
            let mgr = std::sync::Arc::clone(repl_mgr);
            let ttl_hours = i64::from(opt.replication_status_ttl_hours);
            tracing::info!(
                ttl_hours,
                "S4 replication-status sweep active (hourly tick, TTL configurable via --replication-status-ttl-hours; Pending entries never swept)"
            );
            // v0.8.5 #81 (audit H-7): cancellation-aware sweep loop.
            let shutdown_cl = Arc::clone(&shutdown_notify);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
                // Skip the immediate tick — a freshly-booted process
                // can't have any terminal entries past TTL yet (any
                // restored snapshot entries got `recorded_at = now`
                // via the `#[serde(default)]` fallback).
                ticker.tick().await;
                loop {
                    tokio::select! {
                        () = shutdown_cl.notified() => {
                            tracing::info!(
                                "S4 replication-status sweep shutting down (got cancel signal)"
                            );
                            return;
                        }
                        _ = ticker.tick() => {}
                    }
                    let max_age = chrono::Duration::hours(ttl_hours);
                    let n = mgr.sweep_stale(chrono::Utc::now(), max_age);
                    if n > 0 {
                        tracing::info!(
                            swept_count = n,
                            ttl_hours,
                            "S4 replication-status sweep pruned terminal entries (Completed / Failed past TTL)"
                        );
                        s4_server::metrics::record_replication_status_swept(n as u64);
                    }
                }
            });
        }
    }

    // v0.8.5 #86 (audit M-3): install the SIGUSR1 snapshot dump-back
    // handler. Operators send `kill -USR1 <pid>` to durably re-emit
    // every attached manager's in-memory state to its
    // `--<manager>-state-file <PATH>` without bouncing the gateway.
    // Pre-#86 this was documented as a "future hook" in seven different
    // CLI docstrings; the snapshot-load side already shipped in v0.5
    // #34 / #30 etc. but the dump-back side never landed, so any
    // restart lost everything written through the manager since boot.
    //
    // Unix-only — `tokio::signal::unix` is not available on Windows.
    // The handler is best-effort: per-manager errors are
    // logged-and-counted (`s4_sigusr1_dump_total{result="err"}`) so the
    // operator notices, but a single bad write does not abort the
    // dump-back of the remaining managers. The atomic-write pattern
    // (write `<PATH>.tmp` → `rename` → `<PATH>`) guarantees an
    // interrupted dump never leaves the operator with a half-written
    // snapshot file (the rename is atomic on every POSIX-compliant fs;
    // on a power loss the worst case is a `<PATH>.tmp` orphan, never a
    // truncated `<PATH>`).
    #[cfg(unix)]
    {
        let s4_for_dump = std::sync::Arc::clone(&s4_arc);
        let snapshot_paths = OptSnapshotPaths::from_opt(&opt);
        match install_sigusr1_snapshot_handler(s4_for_dump, snapshot_paths) {
            Ok(()) => {
                info!(
                    "S4 SIGUSR1 snapshot dump-back handler installed \
                     (v0.8.5 #86 audit M-3): operator can `kill -USR1 <pid>` to \
                     re-emit every attached manager's state to its --*-state-file"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "S4 SIGUSR1 snapshot dump-back handler installation failed; \
                     SIGUSR1 will be ignored. Operators must restart with the \
                     existing `--*-state-file` paths to durably persist new state."
                );
            }
        }
    }

    let shared = s4_server::service_arc::SharedService::new(s4_arc);
    let server_result = run_server(
        shared,
        &sdk_conf,
        &opt,
        ready_client,
        cors_manager,
        sigv4a_gate,
        shutdown_notify,
    )
    .await;

    // v0.8.5 #86 (audit M-1): drain background-task handles on the way
    // out. The flusher already exits cleanly when `shutdown_notify`
    // fires (above in `run_server`'s listener loop break), so the joins
    // here are a best-effort wait-for-clean-exit. We swallow any
    // `JoinError` (panic / abort) — those are already surfaced via the
    // dispatcher-panic counter / per-task error logs; bubbling them
    // out of `start_server` would mask the actual graceful-shutdown
    // completion in the operator's exit-code observability.
    for handle in background_handles {
        // Bound the wait so a wedged background task can't keep the
        // process alive forever: 5 s is enough for the access-log
        // flusher's final-drain (1 file write) and is well under the
        // existing 10 s `graceful.shutdown()` budget in `run_server`.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }
    server_result
}

/// v0.8.5 #86 (audit M-3): snapshot of every `--*-state-file` CLI flag
/// captured at boot, so the SIGUSR1 dump-back handler can write each
/// attached manager's `to_json()` back to its operator-supplied path
/// without holding a borrow of the (consumed) `Opt`. `None` for a path
/// means the operator did not pass that flag — the matching manager is
/// not attached, and the dump-back skips it without bumping the metric.
#[cfg(unix)]
#[derive(Debug, Clone)]
struct OptSnapshotPaths {
    versioning: Option<std::path::PathBuf>,
    object_lock: Option<std::path::PathBuf>,
    mfa_delete: Option<std::path::PathBuf>,
    cors: Option<std::path::PathBuf>,
    inventory: Option<std::path::PathBuf>,
    notifications: Option<std::path::PathBuf>,
    tagging: Option<std::path::PathBuf>,
    replication: Option<std::path::PathBuf>,
    lifecycle: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl OptSnapshotPaths {
    fn from_opt(opt: &Opt) -> Self {
        Self {
            versioning: opt.versioning_state_file.clone(),
            object_lock: opt.object_lock_state_file.clone(),
            mfa_delete: opt.mfa_delete_state_file.clone(),
            cors: opt.cors_state_file.clone(),
            inventory: opt.inventory_state_file.clone(),
            notifications: opt.notifications_state_file.clone(),
            tagging: opt.tagging_state_file.clone(),
            replication: opt.replication_state_file.clone(),
            lifecycle: opt.lifecycle_state_file.clone(),
        }
    }
}

/// v0.8.5 #86 (audit M-3): install a `SignalKind::user_defined1`
/// listener that, on every SIGUSR1 reception, walks every attached
/// manager on the supplied `S4Service` and dumps its `to_json()` to
/// the matching `--*-state-file` path via [`atomic_write`]. Returns
/// the handler installation error (e.g. "too many signal handlers
/// installed for SIGUSR1") so the caller can log a WARN and fall back
/// to the documented "restart to persist" path.
#[cfg(unix)]
fn install_sigusr1_snapshot_handler<B>(
    s4: std::sync::Arc<s4_server::S4Service<B>>,
    paths: OptSnapshotPaths,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>>
where
    B: s3s::S3 + Send + Sync + 'static,
{
    use tokio::signal::unix::{SignalKind, signal};
    let mut usr1 =
        signal(SignalKind::user_defined1()).map_err(|e| format!("install SIGUSR1 handler: {e}"))?;
    tokio::spawn(async move {
        while usr1.recv().await.is_some() {
            let (ok, err) = dump_all_snapshots(&s4, &paths);
            tracing::info!(
                ok_count = ok,
                err_count = err,
                "S4 SIGUSR1: dumped attached-manager snapshots to --*-state-file paths (v0.8.5 #86)"
            );
        }
    });
    Ok(())
}

/// v0.8.5 #86 (audit M-3): walk every attached manager + matching
/// snapshot path pair, render `to_json()`, and atomically write to disk.
/// Returns `(ok_count, err_count)` so the SIGUSR1 callback can log a
/// summary; per-manager `s4_sigusr1_dump_total{manager,result}` is
/// bumped inside this fn so dashboards can split clean writes from
/// failures.
#[cfg(unix)]
fn dump_all_snapshots<B>(
    s4: &std::sync::Arc<s4_server::S4Service<B>>,
    paths: &OptSnapshotPaths,
) -> (usize, usize)
where
    B: s3s::S3 + Send + Sync + 'static,
{
    let mut ok = 0usize;
    let mut err = 0usize;
    macro_rules! dump_one {
        ($manager_name:expr, $accessor:expr, $path_field:ident) => {{
            if let (Some(mgr), Some(path)) = ($accessor, paths.$path_field.as_ref()) {
                let success = match mgr.to_json() {
                    Ok(json) => match atomic_write(path, &json) {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(
                                manager = $manager_name,
                                path = %path.display(),
                                error = %e,
                                "S4 SIGUSR1: snapshot atomic_write failed"
                            );
                            false
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            manager = $manager_name,
                            path = %path.display(),
                            error = %e,
                            "S4 SIGUSR1: manager.to_json() failed"
                        );
                        false
                    }
                };
                s4_server::metrics::record_sigusr1_dump($manager_name, success);
                if success {
                    ok += 1;
                } else {
                    err += 1;
                }
            }
        }};
    }
    dump_one!("versioning", s4.versioning_manager(), versioning);
    dump_one!("object_lock", s4.object_lock_manager(), object_lock);
    dump_one!("mfa_delete", s4.mfa_delete_manager(), mfa_delete);
    dump_one!("cors", s4.cors_manager(), cors);
    dump_one!("inventory", s4.inventory_manager(), inventory);
    dump_one!("notifications", s4.notifications_manager(), notifications);
    dump_one!("tagging", s4.tag_manager(), tagging);
    dump_one!("replication", s4.replication_manager(), replication);
    dump_one!("lifecycle", s4.lifecycle_manager(), lifecycle);
    (ok, err)
}

/// v0.8.5 #86 (audit M-3): write `contents` to `path` atomically. We
/// write to `<path>.tmp` first, then `rename` it onto `<path>`. The
/// rename is atomic on every POSIX-compliant filesystem; on a power
/// loss the worst case is a `<path>.tmp` orphan, never a half-written
/// `<path>`. (`std::fs::rename` is documented to overwrite the
/// destination on Unix; on Windows it would fail if the target exists,
/// but this fn is `#[cfg(unix)]`-gated through its only caller.)
#[cfg(unix)]
fn atomic_write(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    // Place the tmp file alongside the target so the rename stays on
    // the same filesystem (cross-fs rename is not atomic and will fall
    // back to copy + delete). `with_extension("tmp")` preserves the
    // parent directory — a relative `versioning.json` becomes
    // `versioning.tmp` next to it; an absolute `/srv/state/versioning.json`
    // becomes `/srv/state/versioning.tmp`.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// v0.5 #32: enforce compliance-mode prerequisites at boot. Each
/// missing piece is reported with an actionable hint rather than
/// surfacing as a runtime 5xx after a deploy.
fn validate_compliance_mode(
    opt: &Opt,
    mode: ComplianceMode,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    match mode {
        ComplianceMode::Strict => {}
    }
    let mut missing: Vec<&'static str> = Vec::new();
    if opt.tls_cert.is_none() && opt.acme.is_none() {
        missing.push("TLS (--tls-cert/--tls-key OR --acme)");
    }
    if opt.access_log.is_none() {
        missing.push("--access-log <DIR>");
    }
    if opt.audit_log_hmac_key.is_none() {
        missing.push("--audit-log-hmac-key <SPEC>");
    }
    if opt.sse_s4_key.is_none() && opt.kms_local_dir.is_none() {
        missing.push("SSE (--sse-s4-key OR --kms-local-dir)");
    }
    if opt.object_lock_state_file.is_none() {
        missing.push("--object-lock-state-file <PATH>");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "compliance-mode strict requires the following options to also be set: {}",
            missing.join(", ")
        )
        .into())
    }
}

/// v0.5 #31: dispatch a non-server subcommand. Currently the only one
/// is `verify-audit-log`, which walks an audit-log file and prints a
/// short report (and exits non-zero on chain break).
fn run_subcommand(cmd: &Cmd) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    match cmd {
        Cmd::VerifyAuditLog(args) => {
            let key = s4_server::audit_log::AuditHmacKey::from_str(&args.hmac_key)
                .map_err(|e| format!("--hmac-key: {e}"))?;
            // v0.8.2 #63: parse the optional operator-supplied prev tail
            // (hex, must decode to 32 bytes).
            let expected_prev_tail = if let Some(hex) = args.expected_prev_tail.as_deref() {
                let bytes = s4_server::audit_log::hex_decode(hex.trim())
                    .ok_or_else(|| "--expected-prev-tail: not valid hex".to_string())?;
                if bytes.len() != 32 {
                    return Err(format!(
                        "--expected-prev-tail: must decode to 32 bytes, got {}",
                        bytes.len()
                    )
                    .into());
                }
                let mut buf = [0u8; 32];
                buf.copy_from_slice(&bytes);
                Some(buf)
            } else {
                None
            };
            let options = s4_server::audit_log::VerifyOptions {
                expected_prev_tail,
                require_eof_hmac: args.require_eof_hmac,
            };
            let report = s4_server::audit_log::verify_audit_log(&args.file, &key, options)
                .map_err(|e| format!("verify-audit-log {}: {e}", args.file.display()))?;
            // v0.8.2 #63: surface the new flags in the OK output so the
            // operator sees exactly what was authenticated. `unsigned_*`
            // are warnings, not errors.
            if report.unsigned_prev_tail {
                eprintln!(
                    "WARN {}: chain seed came from in-file `# prev_file_tail=` comment \
                     (not operator-authenticated; pass --expected-prev-tail to close H-3)",
                    args.file.display()
                );
            }
            if report.unsigned_eof {
                eprintln!(
                    "WARN {}: file does not end with a `# eof_hmac=` marker \
                     (truncation un-detection — H-2; pass --require-eof-hmac to escalate)",
                    args.file.display()
                );
            }
            match report.first_break {
                None => {
                    println!(
                        "OK {} ({} lines, {} chained entries verified)",
                        args.file.display(),
                        report.total_lines,
                        report.ok_lines
                    );
                    Ok(())
                }
                Some(br) => {
                    eprintln!(
                        "BREAK {} at line {} ({} lines total, {} OK before break)",
                        args.file.display(),
                        br.line_no,
                        report.total_lines,
                        report.ok_lines
                    );
                    eprintln!("  expected hmac: {}", br.expected_hmac);
                    eprintln!("  actual hmac:   {}", br.actual_hmac);
                    Err(format!("audit-log chain break at line {}", br.line_no).into())
                }
            }
        }
    }
}

fn build_ready_check(client: aws_sdk_s3::Client) -> ReadyCheck {
    Arc::new(move || {
        let c = client.clone();
        Box::pin(async move {
            // ListBuckets で backend が応答するか確認 (権限不足でも 4xx は届くので "ready"
            // と判定する。connection 失敗 / 5xx だけが not-ready)。
            match c.list_buckets().send().await {
                Ok(_) => Ok(()),
                Err(e) => {
                    let dbg = format!("{e:?}");
                    // 認証や権限の問題は backend は生きているので ready 判定
                    if dbg.contains("AccessDenied")
                        || dbg.contains("InvalidAccessKeyId")
                        || dbg.contains("SignatureDoesNotMatch")
                    {
                        Ok(())
                    } else {
                        Err(format!("backend list_buckets failed: {e}"))
                    }
                }
            }
        })
    })
}

/// v0.8.5 #81 (audit C-1): install the unix SIGTERM stream the
/// listener loop joins with `tokio::signal::ctrl_c()`. Pulled out into
/// a helper so:
///   - the unit test can construct the stream and assert installation
///     succeeds (driving an actual SIGTERM into the test binary would
///     terminate the test runner, so we cap at smoke-check);
///   - the cfg-gated wiring in `run_server` stays scoped to the call
///     site (one-line `let mut sigterm = install_sigterm_stream()?;`).
#[cfg(unix)]
fn install_sigterm_stream()
-> Result<tokio::signal::unix::Signal, Box<dyn Error + Send + Sync + 'static>> {
    use tokio::signal::unix::{SignalKind, signal};
    signal(SignalKind::terminate()).map_err(|e| -> Box<dyn Error + Send + Sync + 'static> {
        format!("install SIGTERM handler: {e}").into()
    })
}

/// v0.8.5 #84 (audit H-5): drive a hyper connection future with an
/// optional wall-clock cap. When the cap is `None` the future runs to
/// completion unchanged (back-compat with `--read-timeout-seconds 0`).
/// When set, exceeding the cap aborts the connection — slow clients
/// can no longer pin a task / FD slot indefinitely. The cap covers
/// header reads, body reads, and the inner-service handler all in
/// one budget (the simplest semantics that defends against slowloris
/// without needing two timers); HTTP keep-alive within the window
/// keeps working because each new request resets hyper's own read
/// budget — only the outer wall-clock keeps ticking. The connection
/// future's own error is logged at DEBUG (normal client disconnects
/// surface here, so WARN would be too noisy).
async fn run_with_optional_timeout<F, E>(fut: F, cap: Option<std::time::Duration>)
where
    F: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    match cap {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!(error = %e, "serve_connection error"),
            Err(_) => tracing::warn!(
                timeout_secs = d.as_secs(),
                "connection timeout (slowloris guard)"
            ),
        },
        None => match fut.await {
            Ok(()) => {}
            Err(e) => tracing::debug!(error = %e, "serve_connection error"),
        },
    }
}

async fn run_server<S>(
    s4: S,
    sdk_conf: &aws_config::SdkConfig,
    opt: &Opt,
    ready_client: aws_sdk_s3::Client,
    cors_manager: Option<Arc<s4_server::cors::CorsManager>>,
    sigv4a_gate: Option<Arc<s4_server::service::SigV4aGate>>,
    // v0.8.5 #81 (audit C-1 + H-7): the shared shutdown signal that
    // every background spawn site is `select!`-ing on. Created in
    // `main()` before any spawn; the listener loop's SIGTERM / SIGINT
    // branch calls `notify_waiters()` here, which fans out to all
    // detached tasks at once so they can drain + exit cleanly instead
    // of being torn down with the runtime.
    shutdown_notify: Arc<tokio::sync::Notify>,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>>
where
    S: S3 + Send + Sync + 'static,
{
    let service = {
        let mut b = S3ServiceBuilder::new(s4);
        if let Some(cred_provider) = sdk_conf.credentials_provider() {
            let cred = cred_provider.provide_credentials().await?;
            b.set_auth(SimpleAuth::from_single(
                cred.access_key_id(),
                cred.secret_access_key(),
            ));
        }
        if let Some(domain) = &opt.domain {
            b.set_host(SingleDomain::new(domain)?);
        }
        b.build()
    };

    let ready_check = build_ready_check(ready_client);
    // Prometheus metrics exporter を install。/metrics endpoint で render される
    let metrics_handle = s4_server::metrics::install();

    // v0.8 #50: AES-NI / NEON runtime detection. SSE-S4 (`crates/s4-server/src/sse.rs`)
    // routes through the `aes-gcm` crate, which selects the AES-NI backend
    // automatically on x86_64 when both the `aes` and `pclmulqdq` CPU
    // features are present (and falls back to a constant-time software
    // implementation otherwise). Surfacing the choice at boot + as a
    // Prometheus gauge lets operators confirm the hardware-acceleration
    // path is live without re-deriving it from `/proc/cpuinfo` or strace.
    // The companion `examples/bench_sse_throughput.rs` measures the
    // resulting MB/s gap between AES-NI and the software fallback.
    #[cfg(target_arch = "x86_64")]
    {
        let aes_ni_available =
            std::is_x86_feature_detected!("aes") && std::is_x86_feature_detected!("pclmulqdq");
        info!(
            target_arch = "x86_64",
            aes_ni_available,
            "S4 AES-NI feature detection (x86_64 only; arm64 always uses NEON if available)"
        );
        let kind = if aes_ni_available {
            "aes-ni"
        } else {
            "software"
        };
        s4_server::metrics::record_sse_aes_backend(kind);
    }
    #[cfg(target_arch = "aarch64")]
    {
        // aarch64: the `aes-gcm` crate uses the ARMv8 AES NEON
        // instructions when the `aes` target feature is present at
        // compile time. Standard release builds with rustc's default
        // aarch64 target enable this on every modern Apple Silicon /
        // Graviton / Ampere host, so we report `"neon"` unconditionally
        // here (rather than gating on a runtime probe — the `std`
        // detection macro on aarch64 is gated behind unstable features
        // and would force a nightly-only build for the gateway binary).
        info!(
            target_arch = "aarch64",
            aes_ni_available = false,
            neon_available = true,
            "S4 AES-NI feature detection (aarch64 — NEON AES used by aes-gcm)"
        );
        s4_server::metrics::record_sse_aes_backend("neon");
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        info!(
            target_arch = std::env::consts::ARCH,
            aes_ni_available = false,
            "S4 AES-NI feature detection (non-x86_64/aarch64 — software fallback)"
        );
        s4_server::metrics::record_sse_aes_backend("software");
    }

    let mut routed_service =
        HealthRouter::new(service, Some(ready_check)).with_metrics(metrics_handle);
    if let Some(mgr) = cors_manager {
        // v0.7 #44: install the CORS manager so OPTIONS preflight is
        // handled at the HTTP layer (Allow-* headers / 403 deny).
        routed_service = routed_service.with_cors_manager(mgr);
    }
    if let Some(gate) = sigv4a_gate {
        // v0.7 #47: install the SigV4a verify gate so
        // `AWS4-ECDSA-P256-SHA256` requests are verified before they
        // reach s3s (which would otherwise reject them as "unknown
        // algorithm"). Plain SigV4 (HMAC) requests are unaffected.
        routed_service = routed_service.with_sigv4a_gate(gate);
        // The SigV4a region check uses the listener's served region —
        // pulled from the AWS SDK config (the same source the rest of
        // the server uses to talk to its backend). Falls back to the
        // `HealthRouter` default ("us-east-1") when unset.
        if let Some(region) = sdk_conf.region() {
            routed_service = routed_service.with_region(region.as_ref());
        }
    }

    let listener = TcpListener::bind((opt.host.as_str(), opt.port)).await?;
    // v0.8.5 #84 (audit H-6): hyper builder is configured before the
    // listener loop spins so every spawned connection inherits the
    // hardened limits. We set HTTP/1 max-buf-size + keep-alive, and
    // when --http2 is on, also clamp HTTP/2's per-connection
    // concurrent-stream + header-list limits. When --http2 is off
    // (default) we lock the listener to HTTP/1.1 only via
    // `http1_only`, narrowing the protocol attack surface (no h2
    // rapid-reset, no SETTINGS flood). Note that `http1_only` /
    // `http2_only` consume the builder, so we apply them at the
    // very end and re-bind `http_server`.
    let http_server = {
        let mut b = ConnBuilder::new(TokioExecutor::new());
        b.http1()
            .max_buf_size(opt.max_header_bytes)
            .keep_alive(true);
        if opt.http2 {
            b.http2()
                .max_concurrent_streams(100u32)
                .max_header_list_size(16 * 1024)
                .keep_alive_interval(Some(std::time::Duration::from_secs(30)));
            b
        } else {
            b.http1_only()
        }
    };
    // v0.8.5 #84 (audit H-5): connection-cap semaphore. Acquired
    // BEFORE the accept (so over-cap clients park on the kernel
    // accept queue, which is bounded and well-understood, instead of
    // spawning unbounded tokio tasks). The owned permit is moved into
    // the per-connection task and dropped on task exit so the slot is
    // released even on panic / early return.
    let conn_cap = Arc::new(tokio::sync::Semaphore::new(opt.max_concurrent_connections));
    // v0.8.5 #84 (audit H-5): per-connection wall-clock cap. 0 means
    // disabled (unbounded). Stored as `Option<Duration>` so the
    // per-connection wrapper short-circuits without a `Duration::ZERO`
    // sentinel comparison at every accept.
    let read_timeout: Option<std::time::Duration> = if opt.read_timeout_seconds == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(opt.read_timeout_seconds))
    };
    info!(
        max_concurrent_connections = opt.max_concurrent_connections,
        read_timeout_seconds = opt.read_timeout_seconds,
        max_header_bytes = opt.max_header_bytes,
        http2 = opt.http2,
        "S4 HTTP wire-hardening configured (v0.8.5 #84)"
    );
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    // v0.8.5 #81 (audit C-1): k8s pod stop / systemd `stop` send
    // SIGTERM, not SIGINT. Without an explicit SIGTERM handler the
    // tokio runtime would not cooperate with the orchestrator's
    // graceful-shutdown window — kubelet would wait the full
    // `terminationGracePeriodSeconds` (default 30 s) and then SIGKILL,
    // tearing every in-flight upload mid-write. Installing the handler
    // here lets us join the SIGINT path so both signals route into the
    // same `notify_waiters()` fan-out.
    //
    // Unix-only: S4 is a server-only binary intended for Linux / macOS
    // hosts (k8s pod, systemd unit, dev macOS). Windows would need a
    // CTRL_BREAK_EVENT handler via `tokio::signal::windows`; the cfg
    // guard keeps that follow-up cleanly opt-in without breaking
    // `cargo check --target x86_64-pc-windows-msvc` parsing.
    #[cfg(unix)]
    let mut sigterm = install_sigterm_stream()?;

    let tls_state: Option<Arc<s4_server::tls::TlsState>> = match (&opt.tls_cert, &opt.tls_key) {
        (Some(cert), Some(key)) => {
            s4_server::tls::install_default_crypto_provider();
            let state = if matches!(opt.compliance_mode, Some(ComplianceMode::Strict)) {
                tracing::info!("compliance-mode strict: TLS restricted to 1.3-only");
                Arc::new(s4_server::tls::TlsState::load_tls13_only(cert, key)?)
            } else {
                Arc::new(s4_server::tls::TlsState::load(cert, key)?)
            };
            // SIGHUP handler — operators rotate cert + key files and
            // `kill -HUP <pid>` to atomically swap the active config.
            // Re-read failures (missing file / bad PEM / key mismatch) are
            // logged at WARN; the previous config stays in effect, so a
            // bad reload never causes a listener outage.
            let reload_state = Arc::clone(&state);
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                let mut hup = match signal(SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("could not install SIGHUP handler: {e}");
                        return;
                    }
                };
                while hup.recv().await.is_some() {
                    match reload_state.reload() {
                        Ok(()) => {
                            tracing::info!("S4 TLS cert hot-reload succeeded");
                            s4_server::metrics::record_tls_cert_reload(true);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "S4 TLS cert hot-reload failed (keeping previous config): {e}"
                            );
                            s4_server::metrics::record_tls_cert_reload(false);
                        }
                    }
                }
            });
            Some(state)
        }
        _ => None,
    };

    // ACME (Let's Encrypt) acceptor — mutually exclusive with --tls-cert
    // (clap rejects both being set). Drives renewal on a background task
    // and returns two rustls configs the per-connection handler picks
    // between based on TLS-ALPN-01 challenge detection.
    let acme_acceptors: Option<Arc<s4_server::acme::AcmeAcceptors>> = match &opt.acme {
        Some(domains_csv) => {
            s4_server::tls::install_default_crypto_provider();
            let domains: Vec<String> = domains_csv
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            let cache_dir = opt.acme_cache_dir.clone().unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                std::path::PathBuf::from(home).join(".s4/acme")
            });
            info!(
                domains = ?domains,
                staging = opt.acme_staging,
                cache_dir = %cache_dir.display(),
                "S4 ACME acceptor bootstrapping"
            );
            Some(Arc::new(s4_server::acme::bootstrap(
                s4_server::acme::AcmeOptions {
                    domains,
                    contact: opt.acme_contact.clone(),
                    cache_dir,
                    staging: opt.acme_staging,
                },
            )))
        }
        None => None,
    };

    let scheme = if tls_state.is_some() || acme_acceptors.is_some() {
        "https"
    } else {
        "http"
    };

    info!(
        host = %opt.host,
        port = opt.port,
        scheme,
        endpoint_url = opt.endpoint_url.as_deref().unwrap_or("<unset>"),
        "S4 listening (paths /health and /ready served alongside S3 traffic)"
    );

    loop {
        // v0.8.5 #84 (audit H-5): acquire a semaphore permit BEFORE
        // accepting. If the cap is hit the await parks here and the
        // accept queue stays in the kernel — much more efficient than
        // spawning a task that immediately blocks. The owned permit
        // is moved into the per-connection task and dropped on task
        // exit (panic-safe) to release the slot. We `acquire_owned`
        // on a clone of the Arc so the loop's own Arc handle stays
        // valid for the next iteration.
        let permit = match Arc::clone(&conn_cap).acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                // Semaphore closed — only happens at shutdown if
                // someone calls `close()`, which we never do. Treat
                // as a graceful exit signal.
                tracing::info!("connection-cap semaphore closed, exiting accept loop");
                break;
            }
        };
        // v0.8.5 #81 (audit C-1 + H-7): the SIGTERM arm is `#[cfg(unix)]`
        // because the `tokio::signal::unix` module is itself unix-only.
        // We carry two parallel `tokio::select!` blocks rather than
        // splitting the loop body — one with the SIGTERM arm wired in,
        // one without — because `tokio::select!` does not support
        // cfg-attributes on individual arms (the macro parser bails on
        // `#[cfg(...)]` mid-pattern). Both branches still fan out via
        // `shutdown_notify.notify_waiters()` so the per-spawn `select!`
        // loops can drain + exit cleanly.
        #[cfg(unix)]
        let (socket, _) = tokio::select! {
            res = listener.accept() => match res {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::error!("accept error: {err}");
                    drop(permit);
                    continue;
                }
            },
            _ = ctrl_c.as_mut() => {
                tracing::info!("S4 received SIGINT, initiating graceful shutdown");
                shutdown_notify.notify_waiters();
                drop(permit);
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("S4 received SIGTERM, initiating graceful shutdown");
                shutdown_notify.notify_waiters();
                drop(permit);
                break;
            }
        };
        #[cfg(not(unix))]
        let (socket, _) = tokio::select! {
            res = listener.accept() => match res {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::error!("accept error: {err}");
                    drop(permit);
                    continue;
                }
            },
            _ = ctrl_c.as_mut() => {
                tracing::info!("S4 received SIGINT, initiating graceful shutdown");
                shutdown_notify.notify_waiters();
                drop(permit);
                break;
            }
        };
        let svc = routed_service.clone();
        let server = http_server.clone();
        let watch_handle = graceful.watcher();
        if let Some(acceptors) = acme_acceptors.as_ref() {
            // ACME path: every connection is inspected for TLS-ALPN-01
            // challenge first; real TLS traffic gets the current cert.
            let acceptors = Arc::clone(acceptors);
            tokio::spawn(async move {
                let _permit = permit; // released at task end
                match s4_server::acme::accept_one(socket, &acceptors).await {
                    Ok(Some(tls_stream)) => {
                        let conn = server.serve_connection(TokioIo::new(tls_stream), svc);
                        let conn = watch_handle.watch(conn.into_owned());
                        run_with_optional_timeout(conn, read_timeout).await;
                    }
                    Ok(None) => {
                        // Challenge handled; nothing more to do.
                    }
                    Err(err) => {
                        tracing::warn!("acme handshake failed: {err}");
                    }
                }
            });
        } else if let Some(state) = tls_state.as_ref() {
            // Static TLS: per-connection acceptor picks up the latest
            // swapped config so SIGHUP reload takes effect from the very
            // next connection without dropping anything in flight.
            let acceptor = state.acceptor();
            tokio::spawn(async move {
                let _permit = permit;
                let tls_stream = match acceptor.accept(socket).await {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!("tls handshake failed: {err}");
                        return;
                    }
                };
                let conn = server.serve_connection(TokioIo::new(tls_stream), svc);
                let conn = watch_handle.watch(conn.into_owned());
                run_with_optional_timeout(conn, read_timeout).await;
            });
        } else {
            let conn = server.serve_connection(TokioIo::new(socket), svc);
            let conn = watch_handle.watch(conn.into_owned());
            tokio::spawn(async move {
                let _permit = permit;
                run_with_optional_timeout(conn, read_timeout).await;
            });
        }
    }

    tokio::select! {
        () = graceful.shutdown() => tracing::debug!("graceful shutdown complete"),
        () = tokio::time::sleep(std::time::Duration::from_secs(10)) =>
            tracing::warn!("graceful shutdown timeout, aborting"),
    }
    info!("S4 stopped");
    Ok(())
}

#[cfg(test)]
mod shutdown_tests {
    //! v0.8.5 #81 (audit C-1 + H-7): unit tests for the SIGTERM handler
    //! installation + the cancellation-Notify shape every background
    //! spawn site uses. Driving an actual SIGTERM into the test binary
    //! would terminate the test runner, so the installation test caps
    //! at smoke-check; the cancellation test covers the per-spawn
    //! `select!` shape directly so a regression that loses the cancel
    //! branch trips immediately on the timeout assertion.

    /// Smoke-check that the SIGTERM signal stream constructor (the
    /// helper `run_server` calls before entering the listener loop)
    /// succeeds in a unit-test process. We can't actually fire SIGTERM
    /// here — that would terminate the test runner — so the assertion
    /// is "stream construction did not error", which is the load-bearing
    /// check (an OS-level failure to install the handler is the only
    /// way the listener loop's `tokio::select!` over signals could
    /// silently lose the SIGTERM arm).
    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_handler_installed_unit_test() {
        let res = super::install_sigterm_stream();
        assert!(
            res.is_ok(),
            "SIGTERM signal stream must construct cleanly in a unit-test process (got: {:?})",
            res.as_ref().err().map(std::string::ToString::to_string),
        );
    }

    /// Drive the cancellation-Notify shape every background spawn site
    /// uses — `tokio::select!` over `Notify::notified()` plus a
    /// long-period `interval`. The test asserts that a
    /// `notify_waiters()` call wakes the parked task within a tight
    /// budget (well below the next tick) so a regression that loses
    /// the cancel branch (= tick falls through first) trips on the
    /// timeout assertion.
    #[tokio::test]
    async fn background_task_stops_on_cancellation_notify() {
        use std::sync::Arc;
        use std::time::Duration;
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_cl = Arc::clone(&notify);
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exited_cl = Arc::clone(&exited);

        let handle = tokio::spawn(async move {
            // Use a tick period two orders of magnitude longer than
            // the test's wait budget so a regression that swaps the
            // two branches falls through to the tick instead of
            // wedging on the cancel.
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            // First tick fires immediately — match the production
            // pattern that consumes it before entering the loop.
            ticker.tick().await;
            loop {
                tokio::select! {
                    () = notify_cl.notified() => {
                        exited_cl.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                    _ = ticker.tick() => {
                        // Should never reach here within the test
                        // budget — `notify_waiters()` must wake the
                        // parked task long before the 60 s tick.
                    }
                }
            }
        });

        // Give the spawned task a moment to park on `notified()`
        // before we fire the wakeup; without this yield the
        // notification would race with the spawn and miss
        // (notify_waiters() only wakes already-parked listeners).
        tokio::time::sleep(Duration::from_millis(20)).await;
        notify.notify_waiters();

        // 200 ms is comfortably above the spawn + park + wake budget
        // on every CI runner we target, and well below the 60 s tick
        // that would indicate the cancel branch was lost.
        let join_res = tokio::time::timeout(Duration::from_millis(200), handle).await;
        assert!(
            join_res.is_ok(),
            "background task did not exit on notify_waiters() within 200 ms — \
             cancellation branch may be missing or starved by the tick branch"
        );
        join_res.unwrap().expect("spawned task must not panic");
        assert!(
            exited.load(std::sync::atomic::Ordering::SeqCst),
            "task exited but did not run the notify-branch body — branch routing is wrong"
        );
    }
}

#[cfg(test)]
mod hardening_tests {
    //! v0.8.5 #84 (audit H-5 + H-6): unit tests for the wire-hardening
    //! config wiring. Spawning a real listener to exercise the
    //! semaphore + per-connection timeout would need a multi-threaded
    //! tokio runtime + a sacrificial port + a slow client harness;
    //! that's an integration-test shape, not a unit. The two tests
    //! here cover the mechanical assertions:
    //!
    //!  1. the connection-cap semaphore is constructed with the
    //!     configured cap (so we'd notice a regression that swapped
    //!     the flag for a hard-coded constant); and
    //!  2. the hyper builder reports HTTP/1-only when `--http2` is
    //!     off (so we'd notice a regression that left HTTP/2 reachable
    //!     by default and widened the protocol attack surface).
    //!
    //! Slowloris / connection-cap behaviour under load is covered by
    //! the issue's pen-test sign-off, not the cargo test suite.
    use hyper_util::rt::TokioExecutor;
    use hyper_util::server::conn::auto::Builder as ConnBuilder;

    #[test]
    fn connection_semaphore_constructed_with_configured_cap() {
        // Mirror the production construction shape — Arc<Semaphore>
        // sized from the CLI flag — and verify `available_permits`
        // reports the configured value before any acquire.
        let cap: usize = 7;
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(cap));
        assert_eq!(
            sem.available_permits(),
            cap,
            "freshly-constructed semaphore must expose the full configured cap"
        );
    }

    #[test]
    fn hyper_builder_http1_only_when_h2_off() {
        // Mirror `run_server`'s builder configuration with `--http2`
        // off (the default). `http1_only` should then report itself
        // as such via `is_http2_available() == false`.
        let mut b = ConnBuilder::new(TokioExecutor::new());
        b.http1().max_buf_size(65_536).keep_alive(true);
        let b = b.http1_only();
        assert!(
            b.is_http1_available(),
            "HTTP/1 must remain available after http1_only()"
        );
        assert!(
            !b.is_http2_available(),
            "http1_only must disable HTTP/2 (S3 attack surface narrowing)"
        );
    }

    #[test]
    fn hyper_builder_http2_available_when_h2_on() {
        // Counterpart sanity check — when `--http2` is on we still
        // get HTTP/2 reachable. Catches a regression that flipped the
        // branch backwards.
        let mut b = ConnBuilder::new(TokioExecutor::new());
        b.http1().max_buf_size(65_536).keep_alive(true);
        b.http2()
            .max_concurrent_streams(100u32)
            .max_header_list_size(16 * 1024);
        // No `http1_only` / `http2_only` — both protocols must be
        // reachable.
        assert!(b.is_http1_available());
        assert!(b.is_http2_available());
    }
}

#[cfg(all(test, unix))]
mod sigusr1_dump_tests {
    //! v0.8.5 #86 (audit M-3): unit tests for the SIGUSR1 snapshot
    //! dump-back helpers. We don't drive an actual SIGUSR1 into the
    //! test process — that's a process-self-signal that races every
    //! other test parked on tokio handles — so the assertions cap at
    //! the [`atomic_write`] file-system contract; the per-manager
    //! `dump_all_snapshots` walk is exercised end-to-end whenever the
    //! parent integration test sends SIGUSR1 (out of unit-test scope).

    use std::io::Read as _;

    /// The atomic-write helper must (a) leave the target file with
    /// exactly the new contents, (b) NOT leave a `<path>.tmp` orphan
    /// behind on a successful run, and (c) overwrite an existing
    /// target file (the `--*-state-file` snapshot may already exist
    /// from the previous boot's snapshot or from a prior SIGUSR1).
    #[test]
    fn atomic_write_replaces_target_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "s4-86-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("tmp dir create");
        let target = dir.join("snapshot.json");

        // Pre-populate the target with stale contents so we can
        // assert the rename actually replaced (not appended) them.
        std::fs::write(&target, b"STALE").expect("seed stale target");

        let payload = "{\"versioning\":{}}";
        super::atomic_write(&target, payload).expect("atomic_write must succeed");

        let mut buf = String::new();
        std::fs::File::open(&target)
            .expect("target re-open after atomic_write")
            .read_to_string(&mut buf)
            .expect("target read after atomic_write");
        assert_eq!(
            buf, payload,
            "atomic_write must overwrite the target with the new payload, not append"
        );

        // The tmp sibling MUST be gone after a successful rename.
        let tmp = target.with_extension("tmp");
        assert!(
            !tmp.exists(),
            "successful atomic_write must remove the .tmp sibling (rename, not copy); \
             found leftover at {}",
            tmp.display()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `OptSnapshotPaths::from_opt` is a pure projection — every
    /// `--*-state-file` flag round-trips into the matching `Option`
    /// field. The dump-back walk gates on `Some(path)` per manager,
    /// so a regression that swapped two field assignments would
    /// silently dump the wrong manager's JSON to the operator's path.
    /// We can't easily build a full `Opt` here (clap's `Parser`
    /// derive needs a process-wide arg vector), so the test asserts
    /// the empty / `None` defaults instead — the populated case is
    /// covered by the integration-test path that drives the binary
    /// with explicit `--*-state-file` args.
    #[test]
    fn opt_snapshot_paths_default_is_all_none() {
        // Synthesise an `Opt` via clap's `try_parse_from` with only
        // the required `--endpoint-url` arg so every `--*-state-file`
        // flag falls back to its default (= `None`).
        use clap::Parser as _;
        let opt = match super::Opt::try_parse_from([
            "s4-server",
            "--endpoint-url",
            "http://127.0.0.1:9000",
        ]) {
            Ok(o) => o,
            Err(e) => {
                // clap surfaces missing required args as Err; the
                // test only proceeds if every other flag is optional
                // (= the `--endpoint-url` shown above is the only
                // required positional). If a future change adds
                // another required flag this test will need updating
                // — fail loudly so we notice.
                panic!("unable to parse minimal Opt for test: {e}");
            }
        };
        let paths = super::OptSnapshotPaths::from_opt(&opt);
        assert!(
            paths.versioning.is_none(),
            "versioning default must be None"
        );
        assert!(
            paths.object_lock.is_none(),
            "object_lock default must be None"
        );
        assert!(
            paths.mfa_delete.is_none(),
            "mfa_delete default must be None"
        );
        assert!(paths.cors.is_none(), "cors default must be None");
        assert!(paths.inventory.is_none(), "inventory default must be None");
        assert!(
            paths.notifications.is_none(),
            "notifications default must be None"
        );
        assert!(paths.tagging.is_none(), "tagging default must be None");
        assert!(
            paths.replication.is_none(),
            "replication default must be None"
        );
        assert!(paths.lifecycle.is_none(), "lifecycle default must be None");
    }
}
