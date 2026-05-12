//! S4 server binary。`s4-server::S4Service` で `s3s_aws::Proxy` を圧縮 hook 付きに
//! ラップし、hyper-util 経由で公開する。

// tracing-subscriber + OpenTelemetry の Layered<...> 型が深くなり trait
// resolver の default depth (128) を超えるため、解決上限を 512 に上げる。
#![recursion_limit = "512"]

use std::error::Error;
use std::io::IsTerminal;
use std::sync::Arc;

use aws_credential_types::provider::ProvideCredentials;
use clap::{Parser, ValueEnum};
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

    /// バックエンド S3 endpoint (例: https://s3.us-east-1.amazonaws.com)
    #[clap(long)]
    endpoint_url: String,

    /// 既定の圧縮 codec (PUT 時に dispatcher が選ぶ default)
    #[clap(long, value_enum, default_value = "cpu-zstd")]
    codec: CodecChoice,

    /// CPU zstd の compression level (1-22)
    #[clap(long, default_value_t = CpuZstd::DEFAULT_LEVEL)]
    zstd_level: i32,

    /// codec dispatcher: always (CLI 指定固定) / sampling (auto 選択)
    #[clap(long, value_enum, default_value = "sampling")]
    dispatcher: DispatcherChoice,

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

    /// Optional S3-style access-log destination directory. When set,
    /// every completed PUT / GET / DELETE / List request is buffered
    /// and flushed to hourly-rotated `.log` files under the directory.
    /// v0.4 ships local-directory only; pipe via filebeat / vector / etc
    /// to ship to S3 if needed (a follow-up issue tracks native s3://
    /// destination).
    #[clap(long)]
    access_log: Option<std::path::PathBuf>,
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

fn build_dispatcher(choice: DispatcherChoice, default: CodecKind) -> Arc<dyn CodecDispatcher> {
    match choice {
        DispatcherChoice::Always => Arc::new(AlwaysDispatcher(default)),
        DispatcherChoice::Sampling => Arc::new(SamplingDispatcher::new(default)),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    let opt = Opt::parse();
    setup_tracing(
        opt.log_format,
        opt.otlp_endpoint.as_deref(),
        &opt.service_name,
    )?;

    let sdk_conf = aws_config::from_env()
        .endpoint_url(&opt.endpoint_url)
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
    let dispatcher = build_dispatcher(opt.dispatcher, default_kind);
    info!(
        codec = ?opt.codec,
        dispatcher = ?opt.dispatcher,
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
    if let Some(ref dir) = opt.access_log {
        let dest = s4_server::access_log::AccessLogDest { dir: dir.clone() };
        let log = std::sync::Arc::new(s4_server::access_log::AccessLog::new(dest));
        let _flusher = log.spawn_flusher();
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
    run_server(s4, &sdk_conf, &opt, ready_client).await
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

async fn run_server<S>(
    s4: S,
    sdk_conf: &aws_config::SdkConfig,
    opt: &Opt,
    ready_client: aws_sdk_s3::Client,
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
    let routed_service = HealthRouter::new(service, Some(ready_check)).with_metrics(metrics_handle);

    let listener = TcpListener::bind((opt.host.as_str(), opt.port)).await?;
    let http_server = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    let tls_state: Option<Arc<s4_server::tls::TlsState>> = match (&opt.tls_cert, &opt.tls_key) {
        (Some(cert), Some(key)) => {
            s4_server::tls::install_default_crypto_provider();
            let state = Arc::new(s4_server::tls::TlsState::load(cert, key)?);
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
        endpoint_url = %opt.endpoint_url,
        "S4 listening (paths /health and /ready served alongside S3 traffic)"
    );

    loop {
        let (socket, _) = tokio::select! {
            res = listener.accept() => match res {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::error!("accept error: {err}");
                    continue;
                }
            },
            _ = ctrl_c.as_mut() => break,
        };
        let svc = routed_service.clone();
        let server = http_server.clone();
        let watch_handle = graceful.watcher();
        if let Some(acceptors) = acme_acceptors.as_ref() {
            // ACME path: every connection is inspected for TLS-ALPN-01
            // challenge first; real TLS traffic gets the current cert.
            let acceptors = Arc::clone(acceptors);
            tokio::spawn(async move {
                match s4_server::acme::accept_one(socket, &acceptors).await {
                    Ok(Some(tls_stream)) => {
                        let conn = server.serve_connection(TokioIo::new(tls_stream), svc);
                        let conn = watch_handle.watch(conn.into_owned());
                        let _ = conn.await;
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
                let tls_stream = match acceptor.accept(socket).await {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!("tls handshake failed: {err}");
                        return;
                    }
                };
                let conn = server.serve_connection(TokioIo::new(tls_stream), svc);
                let conn = watch_handle.watch(conn.into_owned());
                let _ = conn.await;
            });
        } else {
            let conn = server.serve_connection(TokioIo::new(socket), svc);
            let conn = watch_handle.watch(conn.into_owned());
            tokio::spawn(async move {
                let _ = conn.await;
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
