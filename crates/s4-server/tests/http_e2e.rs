//! HTTP-level E2E test。
//!
//! `S4Service` を実際の hyper server として spawn し、aws-sdk-s3 client を S4 server へ
//! 向けて PUT/GET を流す。in-process trait test では拾えない:
//! - HTTP body parsing (chunked encoding、AWS-chunked encoding)
//! - SigV4 検証 (s3s 内蔵)
//! - hyper response 組立てとヘッダ伝搬
//! - aws-sdk-s3 client の実装ミスマッチ
//!
//! トポロジ: aws-sdk-s3 client → [S4 server (hyper, ephemeral port)] → s3s_aws::Proxy
//!   → aws-sdk-s3 client → MinIO container
//!
//! `#[ignore]` で gate (Docker 必須)。

use std::sync::Arc;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use bytes::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use s3s::auth::SimpleAuth;
use s3s::service::S3ServiceBuilder;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::AlwaysDispatcher;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const MINIO_USER: &str = "minioadmin";
const MINIO_PASS: &str = "minioadmin";

struct MinioFixture {
    _container: ContainerAsync<MinIO>,
    endpoint_url: String,
}

async fn start_minio() -> MinioFixture {
    let container = MinIO::default().start().await.expect("start MinIO");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(9000).await.expect("api port");
    let endpoint_url = format!("http://{host}:{port}");
    MinioFixture {
        _container: container,
        endpoint_url,
    }
}

fn build_aws_client(endpoint_url: &str) -> aws_sdk_s3::Client {
    let creds = Credentials::new(MINIO_USER, MINIO_PASS, None, None, "test");
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(endpoint_url)
        .credentials_provider(creds)
        .region(Region::new("us-east-1"))
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

/// S4Service を hyper server として spawn し、(endpoint_url, shutdown_tx) を返す。
async fn spawn_s4_server(backend_endpoint: &str) -> (String, oneshot::Sender<()>) {
    let backend_client = build_aws_client(backend_endpoint);
    let proxy = s3s_aws::Proxy::from(backend_client);
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    );
    let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::CpuZstd));
    let s4 = S4Service::new(proxy, registry, dispatcher);

    let mut svc = S3ServiceBuilder::new(s4);
    svc.set_auth(SimpleAuth::from_single(MINIO_USER, MINIO_PASS));
    let service = svc.build();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local = listener.local_addr().expect("local addr");
    let endpoint_url = format!("http://{local}");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let http_server = ConnBuilder::new(TokioExecutor::new());
        let graceful = hyper_util::server::graceful::GracefulShutdown::new();
        let mut shutdown_rx = std::pin::pin!(shutdown_rx);
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((socket, _)) => {
                            let conn = http_server
                                .serve_connection(TokioIo::new(socket), service.clone());
                            let conn = graceful.watch(conn.into_owned());
                            tokio::spawn(async move {
                                let _ = conn.await;
                            });
                        }
                        Err(e) => {
                            eprintln!("S4 test server accept error: {e}");
                            continue;
                        }
                    }
                }
                _ = shutdown_rx.as_mut() => {
                    break;
                }
            }
        }
        // graceful shutdown with 5s timeout
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), graceful.shutdown()).await;
    });

    (endpoint_url, shutdown_tx)
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn http_roundtrip_through_full_s4_stack() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;

    // backend (MinIO) に bucket を作っておく — S4 経由だと create_bucket もできるが、
    // wire test なので backend 直接で setup
    let backend_client = build_aws_client(&minio.endpoint_url);
    let _ = backend_client
        .create_bucket()
        .bucket("http-e2e")
        .send()
        .await;

    // S4 server を経由する client
    let s4_client = build_aws_client(&s4_endpoint);

    // PUT (圧縮されるはず)
    let payload = Bytes::from("HTTP-level S4 roundtrip data; ".repeat(2000));
    let put_resp = s4_client
        .put_object()
        .bucket("http-e2e")
        .key("hello.txt")
        .body(payload.clone().into())
        .send()
        .await;
    assert!(
        put_resp.is_ok(),
        "PUT through S4 HTTP server failed: {:?}",
        put_resp.err()
    );

    // GET (S4 が解凍するはず) — S4 経由だと元バイト列が返る
    let get_resp = s4_client
        .get_object()
        .bucket("http-e2e")
        .key("hello.txt")
        .send()
        .await
        .expect("GET through S4 HTTP server");
    let body = get_resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body, payload, "HTTP roundtrip body must match");

    // backend を直接読むと圧縮済 bytes が見える (s4-codec metadata 付き)
    let raw = backend_client
        .get_object()
        .bucket("http-e2e")
        .key("hello.txt")
        .send()
        .await
        .expect("raw GET against MinIO");
    let raw_meta = raw.metadata().cloned().unwrap_or_default();
    assert_eq!(
        raw_meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd"),
        "object on MinIO must carry s4-codec metadata"
    );
    let raw_bytes = raw.body.collect().await.expect("body").into_bytes();
    assert!(
        raw_bytes.len() < payload.len() / 10,
        "MinIO 上の object は圧縮されているべき: {} -> {} bytes",
        payload.len(),
        raw_bytes.len()
    );

    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn http_list_objects_through_s4_proxies_correctly() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("list-e2e").send().await;

    let s4_client = build_aws_client(&s4_endpoint);

    // 3 個 PUT
    for key in ["a.log", "b.log", "c.log"] {
        s4_client
            .put_object()
            .bucket("list-e2e")
            .key(key)
            .body(Bytes::from(vec![b'x'; 1024]).into())
            .send()
            .await
            .unwrap_or_else(|e| panic!("PUT {key} failed: {e:?}"));
    }

    // S4 経由で list_objects_v2
    let list = s4_client
        .list_objects_v2()
        .bucket("list-e2e")
        .send()
        .await
        .expect("list_objects_v2");
    let keys: Vec<&str> = list.contents().iter().filter_map(|o| o.key()).collect();
    assert_eq!(keys.len(), 3);
    assert!(keys.contains(&"a.log"));
    assert!(keys.contains(&"b.log"));
    assert!(keys.contains(&"c.log"));

    let _ = shutdown.send(());
}
