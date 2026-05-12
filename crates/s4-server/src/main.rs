//! S4 server binary。`s4-server::S4Service` で `s3s_aws::Proxy` を圧縮 hook 付きに
//! ラップし、hyper-util 経由で公開する。

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
    /// nvCOMP zstd-GPU (要 nvcomp-gpu feature)
    #[cfg(feature = "nvcomp-gpu")]
    NvcompZstd,
    /// nvCOMP Bitcomp (整数列向け、要 nvcomp-gpu feature)
    #[cfg(feature = "nvcomp-gpu")]
    NvcompBitcomp,
}

impl CodecChoice {
    fn as_kind(self) -> CodecKind {
        match self {
            Self::Passthrough => CodecKind::Passthrough,
            Self::CpuZstd => CodecKind::CpuZstd,
            #[cfg(feature = "nvcomp-gpu")]
            Self::NvcompZstd => CodecKind::NvcompZstd,
            #[cfg(feature = "nvcomp-gpu")]
            Self::NvcompBitcomp => CodecKind::NvcompBitcomp,
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
}

fn setup_tracing(format: LogFormat) {
    use tracing_subscriber::EnvFilter;
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Pretty => {
            let enable_color = std::io::stdout().is_terminal();
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_ansi(enable_color)
                .init();
        }
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .init();
        }
    }
}

fn build_registry(default: CodecKind, zstd_level: i32) -> Arc<CodecRegistry> {
    let reg = CodecRegistry::new(default)
        .with(Arc::new(Passthrough))
        .with(Arc::new(CpuZstd::new(zstd_level)));
    #[cfg(feature = "nvcomp-gpu")]
    let reg = {
        use s4_codec::nvcomp::{NvcompBitcompCodec, NvcompZstdCodec, is_gpu_available};
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
    setup_tracing(opt.log_format);

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

    let s4 = S4Service::new(proxy, registry, dispatcher);
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
    let routed_service = HealthRouter::new(service, Some(ready_check));

    let listener = TcpListener::bind((opt.host.as_str(), opt.port)).await?;
    let http_server = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    info!(
        host = %opt.host,
        port = opt.port,
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
        let conn = http_server.serve_connection(TokioIo::new(socket), routed_service.clone());
        let conn = graceful.watch(conn.into_owned());
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }

    tokio::select! {
        () = graceful.shutdown() => tracing::debug!("graceful shutdown complete"),
        () = tokio::time::sleep(std::time::Duration::from_secs(10)) =>
            tracing::warn!("graceful shutdown timeout, aborting"),
    }
    info!("S4 stopped");
    Ok(())
}
