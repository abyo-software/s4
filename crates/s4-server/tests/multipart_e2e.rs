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
    // 最低 1 つの S4F2 magic が入っているはず (per-part codec dispatch 対応で
    // frame format を v1 → v2 に bump、24 → 28 byte header)
    let needle = b"S4F2";
    assert!(
        raw_bytes.windows(4).any(|w| w == needle),
        "MinIO object should contain S4F2 frame magic"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn multipart_range_get_uses_sidecar_partial_fetch() {
    // multipart object に対する Range GET が sidecar `<key>.s4index` を経由して
    // partial fetch (帯域節約) で動作することを確認。
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("range-mp").send().await;

    let s4_client = build_aws_client(&s4_endpoint);

    // 30 MB × 3 part の multipart を作る
    const PART_SIZE: usize = 30 * 1024 * 1024;
    fn make_part(seed: u8, size: usize) -> Bytes {
        let mut buf = Vec::with_capacity(size);
        let pattern = format!("PART-{seed:02x} ");
        while buf.len() < size {
            buf.extend_from_slice(pattern.as_bytes());
        }
        buf.truncate(size);
        Bytes::from(buf)
    }
    let parts = [
        make_part(0xa, PART_SIZE),
        make_part(0xb, PART_SIZE),
        make_part(0xc, PART_SIZE),
    ];
    let mut full = Vec::with_capacity(PART_SIZE * 3);
    for p in &parts {
        full.extend_from_slice(p);
    }
    let full_payload = Bytes::from(full);

    let create = s4_client
        .create_multipart_upload()
        .bucket("range-mp")
        .key("big.dat")
        .send()
        .await
        .expect("create");
    let upload_id = create.upload_id().expect("upload_id").to_string();
    let mut completed_parts = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let pn = (i + 1) as i32;
        let resp = s4_client
            .upload_part()
            .bucket("range-mp")
            .key("big.dat")
            .upload_id(&upload_id)
            .part_number(pn)
            .body(p.clone().into())
            .send()
            .await
            .expect("upload_part");
        completed_parts.push(
            aws_sdk_s3::types::CompletedPart::builder()
                .e_tag(resp.e_tag().unwrap_or_default())
                .part_number(pn)
                .build(),
        );
    }
    s4_client
        .complete_multipart_upload()
        .bucket("range-mp")
        .key("big.dat")
        .upload_id(&upload_id)
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .set_parts(Some(completed_parts))
                .build(),
        )
        .send()
        .await
        .expect("complete");

    // sidecar が backend に書かれていることを raw で確認
    let sidecar = backend
        .get_object()
        .bucket("range-mp")
        .key("big.dat.s4index")
        .send()
        .await
        .expect("sidecar must be written by complete_multipart_upload");
    let sidecar_bytes = sidecar
        .body
        .collect()
        .await
        .expect("sidecar body")
        .into_bytes();
    assert!(
        sidecar_bytes.len() > 32,
        "sidecar should have header + entries, got {} bytes",
        sidecar_bytes.len()
    );
    assert_eq!(&sidecar_bytes[..4], b"S4IX", "sidecar magic mismatch");

    // mid-range request: PART_SIZE × 1.5 周辺 = part 1 後半 + part 2 前半の境界
    let start = (PART_SIZE * 3 / 2 - 1000) as u64;
    let end = (PART_SIZE * 3 / 2 + 1000) as u64;
    let resp = s4_client
        .get_object()
        .bucket("range-mp")
        .key("big.dat")
        .range(format!("bytes={start}-{}", end - 1))
        .send()
        .await
        .expect("range get");
    let body = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body.len(), 2000);
    assert_eq!(body.as_ref(), &full_payload[start as usize..end as usize]);

    // suffix range
    let resp = s4_client
        .get_object()
        .bucket("range-mp")
        .key("big.dat")
        .range("bytes=-1024")
        .send()
        .await
        .expect("suffix range");
    let body = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body.len(), 1024);
    assert_eq!(body.as_ref(), &full_payload[full_payload.len() - 1024..]);

    // sidecar 削除後は full-read fallback でも動作すること
    backend
        .delete_object()
        .bucket("range-mp")
        .key("big.dat.s4index")
        .send()
        .await
        .expect("delete sidecar");
    let resp = s4_client
        .get_object()
        .bucket("range-mp")
        .key("big.dat")
        .range(format!("bytes={start}-{}", end - 1))
        .send()
        .await
        .expect("range get after sidecar deleted (fallback)");
    let body = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body.as_ref(), &full_payload[start as usize..end as usize]);

    let _ = shutdown.send(());
}

/// v0.2 #5: the final part of a multipart with a tiny highly-compressible
/// tail must NOT be padded. Validates that the stored S3 size of the last
/// part is close to (compressed payload + frame header), nowhere near the
/// 5 MiB padding ceiling.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn multipart_final_part_skips_padding_for_tiny_tail() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("trim-mp").send().await;
    let s4_client = build_aws_client(&s4_endpoint);

    // Two normal-size parts (8 MiB each, mostly random so they DON'T compress)
    // + one tiny final part (200 KiB of highly compressible 'x' bytes).
    fn random_part(seed: u64, size: usize) -> Bytes {
        let mut state = seed;
        let mut buf = Vec::with_capacity(size);
        while buf.len() < size {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            buf.extend_from_slice(&state.to_le_bytes());
        }
        buf.truncate(size);
        Bytes::from(buf)
    }
    let part_1 = random_part(0x1, 8 * 1024 * 1024);
    let part_2 = random_part(0x2, 8 * 1024 * 1024);
    // tiny final part: 200 KiB of highly compressible content
    let part_final = Bytes::from(vec![b'x'; 200 * 1024]);

    let create = s4_client
        .create_multipart_upload()
        .bucket("trim-mp")
        .key("trim.bin")
        .send()
        .await
        .expect("create_multipart_upload");
    let upload_id = create.upload_id().expect("upload_id").to_string();

    let mut completed_parts = Vec::new();
    for (i, part_body) in [&part_1, &part_2, &part_final].iter().enumerate() {
        let part_number = (i + 1) as i32;
        let resp = s4_client
            .upload_part()
            .bucket("trim-mp")
            .key("trim.bin")
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
    let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
        .set_parts(Some(completed_parts))
        .build();
    s4_client
        .complete_multipart_upload()
        .bucket("trim-mp")
        .key("trim.bin")
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("complete_multipart_upload");

    // Roundtrip: GET reconstructs original bytes exactly
    let resp = s4_client
        .get_object()
        .bucket("trim-mp")
        .key("trim.bin")
        .send()
        .await
        .expect("get");
    let got = resp.body.collect().await.expect("body").into_bytes();
    let mut expected = Vec::new();
    expected.extend_from_slice(&part_1);
    expected.extend_from_slice(&part_2);
    expected.extend_from_slice(&part_final);
    assert_eq!(got.as_ref(), expected.as_slice());

    // Probe S3 directly: the last part stored should be tiny (no padding).
    // We use list_parts to check sizes. Since CompleteMultipartUpload merges
    // parts into one S3 object, individual part sizes are no longer queryable
    // post-Complete — but the *combined* S3 object size tells us whether
    // padding was applied. Without padding, total ≈ part_1_compressed +
    // part_2_compressed + part_final_compressed + headers. With padding on the
    // final part, total += ~5 MiB.
    let stored = backend
        .head_object()
        .bucket("trim-mp")
        .key("trim.bin")
        .send()
        .await
        .expect("head stored");
    let stored_size = stored.content_length().unwrap_or(0) as usize;
    // Two random 8 MiB parts: ~8 MiB each compressed (random => no compression
    // gain). Each padded to 5 MiB minimum but already over 5 MiB → no padding.
    // Final part: 200 KiB of 'x' compresses to <300 bytes + frame header (28 B).
    // Without padding skip, final part would be padded to 5 MiB.
    // Expected total ≈ 16 MiB + small overhead, NOT 21+ MiB.
    let expected_max = 16 * 1024 * 1024 + 100 * 1024; // generous slack for frame headers
    assert!(
        stored_size < expected_max,
        "expected total stored size < {expected_max} (no padding on final part), got {stored_size}"
    );

    let _ = shutdown.send(());
}
