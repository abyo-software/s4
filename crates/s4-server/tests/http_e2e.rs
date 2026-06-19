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
use s4_server::routing::HealthRouter;
use std::sync::OnceLock;
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

static METRICS_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

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
    spawn_s4_server_opts(backend_endpoint, false).await
}

/// `spawn_s4_server` with `--logical-etag` toggleable (default path passes `false`).
async fn spawn_s4_server_opts(
    backend_endpoint: &str,
    logical_etag: bool,
) -> (String, oneshot::Sender<()>) {
    let backend_client = build_aws_client(backend_endpoint);
    let proxy = s3s_aws::Proxy::from(backend_client);
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    );
    let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::CpuZstd));
    let s4 = S4Service::new(proxy, registry, dispatcher).with_logical_etag(logical_etag);

    let mut svc = S3ServiceBuilder::new(s4);
    svc.set_auth(SimpleAuth::from_single(MINIO_USER, MINIO_PASS));
    let service = svc.build();
    // /health は無条件 200、/ready は backend (MinIO) ListBuckets の成否で判定
    let backend_for_ready = build_aws_client(backend_endpoint);
    let ready_check: s4_server::routing::ReadyCheck = Arc::new(move || {
        let c = backend_for_ready.clone();
        Box::pin(async move {
            c.list_buckets()
                .send()
                .await
                .map(|_| ())
                .map_err(|e| format!("{e}"))
        })
    });
    // Prometheus metrics は同 process で 1 回しか install できないので、
    // 既に install 済なら再利用、未 install なら install。OnceCell で gate
    let metrics_handle = METRICS_HANDLE
        .get_or_init(s4_server::metrics::install)
        .clone();
    let service = HealthRouter::new(service, Some(ready_check)).with_metrics(metrics_handle);

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

/// `--logical-etag`: a compressed PUT must report the ETag of the ORIGINAL
/// payload (MD5), not the backend's MD5 of the compressed bytes — otherwise
/// AWS SDK v2 clients that validate upload integrity (e.g. OpenSearch's
/// `repository-s3`) reject every blob. We avoid pulling an `md5` dev-dep by
/// using the fact that a direct (uncompressed) PUT to MinIO returns
/// `ETag == MD5(payload)`.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn logical_etag_reports_original_payload_md5() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server_opts(&minio.endpoint_url, true).await;

    let backend_client = build_aws_client(&minio.endpoint_url);
    let _ = backend_client.create_bucket().bucket("le-e2e").send().await;

    // Highly compressible so the object genuinely takes the compress path.
    let payload = Bytes::from("logical-etag payload row; ".repeat(4000));

    // Ground truth: MD5(payload) == the ETag MinIO returns for the raw bytes.
    let raw_etag = backend_client
        .put_object()
        .bucket("le-e2e")
        .key("raw")
        .body(payload.clone().into())
        .send()
        .await
        .expect("direct PUT to MinIO")
        .e_tag()
        .map(str::to_owned);
    assert!(
        raw_etag.is_some(),
        "MinIO must return an ETag for the raw PUT"
    );

    // PUT through S4 (compresses). With --logical-etag the response ETag must
    // equal MD5(original), i.e. the raw ETag above.
    let s4_client = build_aws_client(&s4_endpoint);
    let put = s4_client
        .put_object()
        .bucket("le-e2e")
        .key("compressed")
        .body(payload.clone().into())
        .send()
        .await
        .expect("PUT through S4");
    assert_eq!(
        put.e_tag().map(str::to_owned),
        raw_etag,
        "PUT response ETag must be MD5(original payload), not the compressed object's MD5"
    );

    // HEAD through S4 must echo the same logical ETag.
    let head = s4_client
        .head_object()
        .bucket("le-e2e")
        .key("compressed")
        .send()
        .await
        .expect("HEAD through S4");
    assert_eq!(
        head.e_tag().map(str::to_owned),
        raw_etag,
        "HEAD ETag must echo the original-payload MD5"
    );

    // GET through S4 must echo the same logical ETag (consistent w/ PUT+HEAD)
    // and roundtrip losslessly.
    let get = s4_client
        .get_object()
        .bucket("le-e2e")
        .key("compressed")
        .send()
        .await
        .expect("GET through S4");
    assert_eq!(
        get.e_tag().map(str::to_owned),
        raw_etag,
        "GET ETag must echo the original-payload MD5"
    );
    let got = get.body.collect().await.expect("body").into_bytes();
    assert_eq!(got, payload, "GET roundtrip must be lossless");

    // And the object on the backend is genuinely compressed (the whole point).
    let raw = backend_client
        .get_object()
        .bucket("le-e2e")
        .key("compressed")
        .send()
        .await
        .expect("raw GET against MinIO");
    let stored = raw.body.collect().await.expect("body").into_bytes();
    assert!(
        stored.len() < payload.len() / 5,
        "stored object must be compressed: {} -> {} bytes",
        payload.len(),
        stored.len()
    );

    let _ = shutdown.send(());
}

/// `--logical-etag`: `If-Match` / `If-None-Match` must be evaluated against the
/// LOGICAL ETag (MD5 of original), not the backend's compressed-object ETag.
/// A correct logical ETag matching proves S4 owns the precondition (it would
/// 412 if the backend compared against the compressed bytes).
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn logical_etag_if_match_uses_logical_etag() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server_opts(&minio.endpoint_url, true).await;
    let backend_client = build_aws_client(&minio.endpoint_url);
    let _ = backend_client
        .create_bucket()
        .bucket("le-cond")
        .send()
        .await;

    let payload = Bytes::from("conditional payload row; ".repeat(4000));
    // MD5(payload) == the ETag MinIO returns for a direct (raw) PUT.
    let raw_etag = backend_client
        .put_object()
        .bucket("le-cond")
        .key("raw")
        .body(payload.clone().into())
        .send()
        .await
        .expect("raw PUT to MinIO")
        .e_tag()
        .map(str::to_owned)
        .expect("MinIO ETag");

    let s4 = build_aws_client(&s4_endpoint);
    s4.put_object()
        .bucket("le-cond")
        .key("k")
        .body(payload.clone().into())
        .send()
        .await
        .expect("PUT through S4");

    // If-Match with the logical ETag -> succeeds (would 412 if compared to the
    // backend's compressed-object ETag).
    let ok = s4
        .get_object()
        .bucket("le-cond")
        .key("k")
        .if_match(raw_etag.clone())
        .send()
        .await;
    assert!(
        ok.is_ok(),
        "If-Match with the logical ETag must succeed: {:?}",
        ok.err()
    );
    let body = ok.unwrap().body.collect().await.expect("body").into_bytes();
    assert_eq!(body, payload, "conditional GET roundtrip must be lossless");

    // A wrong If-Match -> 412 (Err).
    let bad = s4
        .get_object()
        .bucket("le-cond")
        .key("k")
        .if_match("\"00000000000000000000000000000000\"")
        .send()
        .await;
    assert!(bad.is_err(), "wrong If-Match must fail the precondition");

    // If-None-Match with the logical ETag -> 304 Not Modified (surfaces as an
    // SDK error with HTTP 304, NOT a 200-with-body).
    let nm = s4
        .get_object()
        .bucket("le-cond")
        .key("k")
        .if_none_match(raw_etag.clone())
        .send()
        .await;
    let status = nm
        .as_ref()
        .err()
        .and_then(|e| e.raw_response())
        .map(|r| r.status().as_u16());
    assert_eq!(
        status,
        Some(304),
        "If-None-Match on a matching ETag must return 304, got {:?}",
        nm.map(|_| "200 OK with body")
            .map_err(|e| e.raw_response().map(|r| r.status().as_u16()))
    );

    let _ = shutdown.send(());
}

/// Default-off contract: with `--logical-etag` NOT set, a compressed PUT must
/// behave exactly as before — no `s4-logical-etag` metadata stamped, and HEAD
/// returns no ETag for the framed object (the pre-flag behaviour).
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn logical_etag_off_leaves_etag_and_metadata_unchanged() {
    let minio = start_minio().await;
    // spawn_s4_server passes logical_etag = false.
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend_client = build_aws_client(&minio.endpoint_url);
    let _ = backend_client.create_bucket().bucket("le-off").send().await;

    let payload = Bytes::from("logical-etag OFF payload; ".repeat(4000));
    let s4_client = build_aws_client(&s4_endpoint);
    s4_client
        .put_object()
        .bucket("le-off")
        .key("k")
        .body(payload.clone().into())
        .send()
        .await
        .expect("PUT through S4 (flag off)");

    // HEAD via S4: a framed object reports no ETag (pre-flag behaviour).
    let head = s4_client
        .head_object()
        .bucket("le-off")
        .key("k")
        .send()
        .await
        .expect("HEAD through S4");
    assert!(
        head.e_tag().is_none(),
        "flag-off compressed object must report no ETag, got {:?}",
        head.e_tag()
    );

    // The backend object must NOT carry the logical-etag stamp.
    let raw = backend_client
        .get_object()
        .bucket("le-off")
        .key("k")
        .send()
        .await
        .expect("raw GET against MinIO");
    let meta = raw.metadata().cloned().unwrap_or_default();
    assert!(
        !meta.contains_key("s4-logical-etag"),
        "flag-off PUT must not stamp s4-logical-etag, metadata = {meta:?}"
    );
    // Roundtrip still lossless.
    let body = raw.body.collect().await.expect("body");
    assert!(
        body.into_bytes().len() < payload.len() / 5,
        "still compressed"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn http_range_get_on_s4_compressed_object_returns_partial_bytes() {
    // S4 で圧縮された object に Range request を送って、part 抜き出しが
    // 正しく返ることを wire-level で検証 (parquet/ORC reader 互換の核機能)
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("range-e2e").send().await;

    let s4_client = build_aws_client(&s4_endpoint);

    // PUT 100 KB (zstd で大幅圧縮されるが decompress 後 100 KB)
    let payload: Vec<u8> = (0..100_000u32).map(|i| (i & 0xff) as u8).collect();
    let payload_bytes = Bytes::from(payload.clone());
    s4_client
        .put_object()
        .bucket("range-e2e")
        .key("ramp.bin")
        .body(payload_bytes.clone().into())
        .send()
        .await
        .expect("put");

    // Case 1: 中間 1000 byte を取得 (bytes=50000-50999)
    let resp = s4_client
        .get_object()
        .bucket("range-e2e")
        .key("ramp.bin")
        .range("bytes=50000-50999")
        .send()
        .await
        .expect("range get mid");
    let body = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body.len(), 1000);
    assert_eq!(body.as_ref(), &payload[50000..51000]);

    // Case 2: 末尾 256 byte を取得 (bytes=-256, suffix range)
    let resp = s4_client
        .get_object()
        .bucket("range-e2e")
        .key("ramp.bin")
        .range("bytes=-256")
        .send()
        .await
        .expect("range get suffix");
    let body = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body.len(), 256);
    assert_eq!(body.as_ref(), &payload[100_000 - 256..]);

    // Case 3: 先頭から (bytes=0-99)
    let resp = s4_client
        .get_object()
        .bucket("range-e2e")
        .key("ramp.bin")
        .range("bytes=0-99")
        .send()
        .await
        .expect("range get prefix");
    let body = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body.len(), 100);
    assert_eq!(body.as_ref(), &payload[..100]);

    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn http_metrics_endpoint_exposes_prometheus_text() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("metrics-e2e").send().await;

    let s4_client = build_aws_client(&s4_endpoint);

    // 1 PUT で counter を発火
    let payload = Bytes::from("metrics test ".repeat(2000));
    s4_client
        .put_object()
        .bucket("metrics-e2e")
        .key("hit.log")
        .body(payload.into())
        .send()
        .await
        .expect("put");

    // /metrics を取得
    let resp = reqwest::get(format!("{s4_endpoint}/metrics"))
        .await
        .expect("/metrics");
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.contains("text/plain"),
        "content-type should be Prometheus text format, got {ct}"
    );
    let body = resp.text().await.expect("metrics body");
    assert!(
        body.contains("s4_requests_total"),
        "missing s4_requests_total in metrics: {body}"
    );
    assert!(body.contains("op=\"put\""), "missing put op label: {body}");
    assert!(
        body.contains("s4_bytes_in_total"),
        "missing bytes_in counter: {body}"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn http_health_and_ready_endpoints_respond_correctly() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;

    // /health は無条件 200
    let h_resp = reqwest::get(format!("{s4_endpoint}/health"))
        .await
        .expect("/health request");
    assert_eq!(h_resp.status(), 200);
    let h_body = h_resp.text().await.expect("/health body");
    assert!(h_body.contains("ok"));

    // /ready は backend が動いているので 200
    let r_resp = reqwest::get(format!("{s4_endpoint}/ready"))
        .await
        .expect("/ready request");
    assert_eq!(r_resp.status(), 200);

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

// v0.7 #44: HTTP-level OPTIONS preflight interceptor wired into the
// hyper listener. The S4 server spawned below has a CORS manager
// attached and a rule registered for bucket `cors-e2e`; the test sends
// a raw OPTIONS request via reqwest (browsers send these for non-simple
// PUT/DELETE requests) and asserts the Access-Control-Allow-* headers
// are echoed correctly.

/// Variant of [`spawn_s4_server`] that attaches a CORS manager pre-seeded
/// with one rule for `bucket`. Used by the v0.7 #44 preflight test below.
async fn spawn_s4_server_with_cors(
    backend_endpoint: &str,
    bucket: &str,
    rule: s4_server::cors::CorsRule,
) -> (String, oneshot::Sender<()>) {
    let backend_client = build_aws_client(backend_endpoint);
    let proxy = s3s_aws::Proxy::from(backend_client);
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    );
    let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::CpuZstd));
    let cors_mgr = Arc::new(s4_server::cors::CorsManager::new());
    cors_mgr.put(bucket, s4_server::cors::CorsConfig { rules: vec![rule] });
    let s4 = S4Service::new(proxy, registry, dispatcher).with_cors(Arc::clone(&cors_mgr));

    let mut svc = S3ServiceBuilder::new(s4);
    svc.set_auth(SimpleAuth::from_single(MINIO_USER, MINIO_PASS));
    let service = svc.build();
    let backend_for_ready = build_aws_client(backend_endpoint);
    let ready_check: s4_server::routing::ReadyCheck = Arc::new(move || {
        let c = backend_for_ready.clone();
        Box::pin(async move {
            c.list_buckets()
                .send()
                .await
                .map(|_| ())
                .map_err(|e| format!("{e}"))
        })
    });
    let metrics_handle = METRICS_HANDLE
        .get_or_init(s4_server::metrics::install)
        .clone();
    // v0.7 #44: install the same CORS manager into the HealthRouter so
    // OPTIONS preflight is intercepted at the HTTP layer.
    let service = HealthRouter::new(service, Some(ready_check))
        .with_metrics(metrics_handle)
        .with_cors_manager(cors_mgr);

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
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), graceful.shutdown()).await;
    });

    (endpoint_url, shutdown_tx)
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn cors_preflight_options_returns_allow_headers() {
    // Boot MinIO + S4 with a CORS rule pre-seeded for bucket `cors-e2e`.
    let minio = start_minio().await;
    let rule = s4_server::cors::CorsRule {
        allowed_origins: vec!["https://app.example.com".into()],
        allowed_methods: vec!["GET".into(), "PUT".into(), "DELETE".into()],
        allowed_headers: vec!["Content-Type".into(), "X-Amz-Date".into()],
        expose_headers: vec!["ETag".into()],
        max_age_seconds: Some(600),
        id: Some("e2e-rule".into()),
    };
    let (s4_endpoint, shutdown) =
        spawn_s4_server_with_cors(&minio.endpoint_url, "cors-e2e", rule).await;

    // 1. Allowed preflight — must return 200 with Allow-* headers.
    let client = reqwest::Client::new();
    let resp = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{s4_endpoint}/cors-e2e/some-key"),
        )
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "PUT")
        .header("Access-Control-Request-Headers", "content-type, x-amz-date")
        .send()
        .await
        .expect("OPTIONS preflight request");
    assert_eq!(
        resp.status(),
        200,
        "matching preflight must be 200 (got body: {:?})",
        resp.text().await.unwrap_or_default()
    );
    let h = resp.headers();
    assert_eq!(
        h.get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com"),
        "Allow-Origin must echo the matched explicit origin"
    );
    let allow_methods = h
        .get("access-control-allow-methods")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        allow_methods.contains("PUT") && allow_methods.contains("GET"),
        "Allow-Methods missing PUT/GET: {allow_methods}"
    );
    let allow_headers = h
        .get("access-control-allow-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        allow_headers.contains("Content-Type"),
        "Allow-Headers missing Content-Type: {allow_headers}"
    );
    assert_eq!(
        h.get("access-control-max-age")
            .and_then(|v| v.to_str().ok()),
        Some("600")
    );
    assert_eq!(
        h.get("access-control-expose-headers")
            .and_then(|v| v.to_str().ok()),
        Some("ETag")
    );

    // 2. Origin not allowed → 403 (bucket has CORS but rule doesn't match).
    let resp = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{s4_endpoint}/cors-e2e/some-key"),
        )
        .header("Origin", "https://evil.example.com")
        .header("Access-Control-Request-Method", "PUT")
        .send()
        .await
        .expect("OPTIONS preflight (denied)");
    assert_eq!(
        resp.status(),
        403,
        "origin outside rule must be 403 (got body: {:?})",
        resp.text().await.unwrap_or_default()
    );

    // 3. OPTIONS to a bucket without CORS config falls through (s3s typically
    // returns 4xx; we only assert the interceptor did NOT inject Allow-Origin).
    let resp = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{s4_endpoint}/no-cors-bucket/key"),
        )
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "PUT")
        .send()
        .await
        .expect("OPTIONS preflight (no config)");
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "interceptor must NOT inject Allow-Origin when bucket has no CORS config"
    );

    let _ = shutdown.send(());
}
