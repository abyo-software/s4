//! Multipart upload の per-part 圧縮を実 HTTP / 実 S3 (MinIO) に流して検証。
//!
//! - aws-sdk-s3 client → S4 server (hyper) → s3s_aws::Proxy → aws-sdk-s3 → MinIO
//! - create_multipart_upload で metadata に s4-multipart=true / s4-codec=cpu-zstd
//! - upload_part 各 5+ MB part が S4 で圧縮 + frame 化されて MinIO に書かれる
//! - get_object で frame 列が parse され、元バイト列が完全に再構築される
//! - raw aws-sdk-s3 で MinIO 直接読みすると全 part が圧縮済 + frame headers が
//!   見える (= production 互換性が wire レベルで成立)

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
                accept = listener.accept() => match accept {
                    Ok((socket, _)) => {
                        let conn = http_server.serve_connection(TokioIo::new(socket), service.clone());
                        let conn = graceful.watch(conn.into_owned());
                        tokio::spawn(async move { let _ = conn.await; });
                    }
                    Err(e) => { eprintln!("accept: {e}"); continue; }
                },
                _ = shutdown_rx.as_mut() => break,
            }
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), graceful.shutdown()).await;
    });
    (endpoint_url, shutdown_tx)
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn multipart_upload_through_s4_with_per_part_compression() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("mp-e2e").send().await;

    let s4_client = build_aws_client(&s4_endpoint);

    // 30 MB parts × 3 = 90 MB total。realistic log-like data (text + entropy)
    // で zstd 3-4x 圧縮を期待 → 各 part 圧縮後 ~7-10 MB、S3 multipart min 5 MB
    // を満たすので padding 不要。
    const PART_SIZE: usize = 30 * 1024 * 1024;
    fn make_part(seed: u64, size: usize) -> Bytes {
        let mut state = seed;
        let mut buf = Vec::with_capacity(size);
        while buf.len() < size {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            // text template + entropy: zstd 3-4x くらい縮む構成
            buf.extend_from_slice(b"INFO 2026-05-12 10:30:45 [worker-thread] processed record_id=");
            for b in state.to_le_bytes() {
                let hex = format!("{b:02x}");
                buf.extend_from_slice(hex.as_bytes());
            }
            buf.extend_from_slice(b" status=ok latency_ms=42\n");
        }
        buf.truncate(size);
        Bytes::from(buf)
    }
    let part_a = make_part(0xa, PART_SIZE);
    let part_b = make_part(0xb, PART_SIZE);
    let part_c = make_part(0xc, PART_SIZE);
    let mut full = Vec::with_capacity(part_a.len() + part_b.len() + part_c.len());
    full.extend_from_slice(&part_a);
    full.extend_from_slice(&part_b);
    full.extend_from_slice(&part_c);
    let full_payload = Bytes::from(full);

    // 1) create_multipart_upload via S4 — S4 が metadata に s4-multipart=true をセット
    let create = s4_client
        .create_multipart_upload()
        .bucket("mp-e2e")
        .key("big.log")
        .send()
        .await
        .expect("create_multipart_upload");
    let upload_id = create.upload_id().expect("upload_id").to_string();

    // 2) upload_part × 3 — S4 が各 part を圧縮 + frame 化して MinIO に転送
    let mut completed_parts = Vec::new();
    for (i, part_body) in [&part_a, &part_b, &part_c].iter().enumerate() {
        let part_number = (i + 1) as i32;
        let resp = s4_client
            .upload_part()
            .bucket("mp-e2e")
            .key("big.log")
            .upload_id(&upload_id)
            .part_number(part_number)
            .body((**part_body).clone().into())
            .send()
            .await
            .unwrap_or_else(|e| panic!("upload_part {part_number} failed: {e:?}"));
        completed_parts.push(
            aws_sdk_s3::types::CompletedPart::builder()
                .e_tag(resp.e_tag().unwrap_or_default())
                .part_number(part_number)
                .build(),
        );
    }

    // 3) complete_multipart_upload via S4 — S4 は素通し (metadata は create で設定済)
    let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
        .set_parts(Some(completed_parts))
        .build();
    s4_client
        .complete_multipart_upload()
        .bucket("mp-e2e")
        .key("big.log")
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("complete_multipart_upload");

    // 4) S4 経由 GET — frame parser が 3 chunks を順に解凍 + 連結 → 元バイト列
    let resp = s4_client
        .get_object()
        .bucket("mp-e2e")
        .key("big.log")
        .send()
        .await
        .expect("get");
    let roundtripped = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(roundtripped.len(), full_payload.len());
    assert_eq!(roundtripped, full_payload);

    // 5) MinIO 上の object を直接読んで「圧縮された」+「frame magic 入り」を確認
    let raw = backend
        .get_object()
        .bucket("mp-e2e")
        .key("big.log")
        .send()
        .await
        .expect("raw get");
    let raw_meta = raw.metadata().cloned().unwrap_or_default();
    assert_eq!(
        raw_meta.get("s4-multipart").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        raw_meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd")
    );
    let raw_bytes = raw.body.collect().await.expect("body").into_bytes();
    // 90 MB log-like data → zstd 3-4x 圧縮、padding は通常不要 (各 part が
    // 5 MB minimum を満たすため)。少なくとも 2x 以上は縮んでいるはず。
    assert!(
        raw_bytes.len() < full_payload.len() / 2,
        "expected at least 2x compression on log-like 90 MB, got {} -> {}",
        full_payload.len(),
        raw_bytes.len()
    );
    // 最低 1 つの S4F1 magic が入っているはず
    let needle = b"S4F1";
    assert!(
        raw_bytes.windows(4).any(|w| w == needle),
        "MinIO object should contain S4F1 frame magic"
    );

    let _ = shutdown.send(());
}
