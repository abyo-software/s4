//! S4 server binary (spike 版、2026-05-12)。
//!
//! 現時点では `s3s_aws::Proxy` をそのまま使い、AWS S3 への素通しが動くことを確認する。
//! Phase 1 で `s3s::S3` trait を独自実装した `S4Service` でラップし、PUT/GET 経路に
//! `s4_codec::Codec` の hook を差し込む。

use std::error::Error;
use std::io::IsTerminal;

use aws_credential_types::provider::ProvideCredentials;
use clap::Parser;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use tokio::net::TcpListener;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "s4", version, about = "S4 — Squished S3 (GPU 透過圧縮 S3 互換ゲートウェイ)")]
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    setup_tracing();
    let opt = Opt::parse();

    let sdk_conf = aws_config::from_env().endpoint_url(&opt.endpoint_url).load().await;
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&sdk_conf)
            .force_path_style(true)
            .build(),
    );
    let proxy = s3s_aws::Proxy::from(client);

    let service = {
        let mut b = S3ServiceBuilder::new(proxy);
        if let Some(cred_provider) = sdk_conf.credentials_provider() {
            let cred = cred_provider.provide_credentials().await?;
            b.set_auth(SimpleAuth::from_single(
                cred.access_key_id(),
                cred.secret_access_key(),
            ));
        }
        if let Some(domain) = opt.domain {
            b.set_host(SingleDomain::new(&domain)?);
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
