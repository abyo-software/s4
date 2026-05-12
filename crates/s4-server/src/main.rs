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
use s4_codec::dispatcher::AlwaysDispatcher;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use tokio::net::TcpListener;
use tracing::info;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CodecChoice {
    /// 無圧縮 (開発・比較用)
    Passthrough,
    /// CPU zstd (GPU 不要、test bed)
    CpuZstd,
    // TODO Phase 1 後半: NvcompBitcomp / NvcompZstd / NvcompGans / DietGpuAns
}

impl CodecChoice {
    fn as_kind(self) -> CodecKind {
        match self {
            Self::Passthrough => CodecKind::Passthrough,
            Self::CpuZstd => CodecKind::CpuZstd,
        }
    }
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

    /// 既定の圧縮 codec (PUT 時に dispatcher が選ぶ)
    #[clap(long, value_enum, default_value = "cpu-zstd")]
    codec: CodecChoice,

    /// CPU zstd の compression level (1-22)
    #[clap(long, default_value_t = CpuZstd::DEFAULT_LEVEL)]
    zstd_level: i32,
}

fn setup_tracing() {
    use tracing_subscriber::EnvFilter;
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let enable_color = std::io::stdout().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(enable_color)
        .init();
}

fn build_registry(zstd_level: i32) -> Arc<CodecRegistry> {
    Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::new(zstd_level))),
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    setup_tracing();
    let opt = Opt::parse();

    let sdk_conf = aws_config::from_env()
        .endpoint_url(&opt.endpoint_url)
        .load()
        .await;
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&sdk_conf)
            .force_path_style(true)
            .build(),
    );
    let proxy = s3s_aws::Proxy::from(client);

    let registry = build_registry(opt.zstd_level);
    let dispatcher = Arc::new(AlwaysDispatcher(opt.codec.as_kind()));
    info!(
        codec = ?opt.codec,
        registered = ?registry.kinds().collect::<Vec<_>>(),
        "S4 codec registry built"
    );

    let s4 = S4Service::new(proxy, registry, dispatcher);
    run_server(s4, &sdk_conf, &opt).await
}

async fn run_server<S>(
    s4: S,
    sdk_conf: &aws_config::SdkConfig,
    opt: &Opt,
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

    let listener = TcpListener::bind((opt.host.as_str(), opt.port)).await?;
    let http_server = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    info!("S4 listening at http://{}:{}/", opt.host, opt.port);
    info!("forwarding to {}", opt.endpoint_url);

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
        let conn = http_server.serve_connection(TokioIo::new(socket), service.clone());
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
