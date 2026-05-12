//! MinIO container を実際に起動してそこに対して `S4Service` 経由で put/get する
//! E2E roundtrip テスト。Docker が必要なため `#[ignore]` で gate。
//!
//! ## 実行方法
//!
//! ```bash
//! # docker daemon が動いている前提
//! cargo test --workspace -- --ignored --nocapture
//! # または特定 test だけ:
//! cargo test --test minio_e2e -- --ignored --nocapture
//! ```
//!
//! ## 何を検証しているか
//!
//! 1. `S4Service<s3s_aws::Proxy>` の **PUT 経由で MinIO に書かれた object が実際に
//!    圧縮されている** (raw aws-sdk-s3 で MinIO を直接読んで bytes 数を確認)
//! 2. **GET 経由で元バイト列が取り戻せる** (compress + decompress wire 互換)
//! 3. **s4-codec metadata が MinIO 上に永続化される** (S4 を再起動してもデータは
//!    同じ意味で読める)
//! 4. **既圧縮データ (gzip) は SamplingDispatcher が passthrough を選ぶ** ので
//!    無駄な再圧縮で膨らまない

use std::sync::Arc;

use bytes::Bytes;
use s3s::S3;
use s3s::dto::*;
use s3s::{S3Request, S3Response};
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::{AlwaysDispatcher, SamplingDispatcher};
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::blob::{bytes_to_blob, collect_blob};
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

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

async fn build_aws_client(endpoint_url: &str) -> aws_sdk_s3::Client {
    let creds = aws_sdk_s3::config::Credentials::new(MINIO_USER, MINIO_PASS, None, None, "test");
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .endpoint_url(endpoint_url)
        .credentials_provider(creds)
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

async fn ensure_bucket(client: &aws_sdk_s3::Client, bucket: &str) {
    let _ = client.create_bucket().bucket(bucket).send().await;
}

fn make_registry() -> Arc<CodecRegistry> {
    Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    )
}

fn put_request(bucket: &str, key: &str, body: Bytes) -> S3Request<PutObjectInput> {
    let input = PutObjectInput {
        bucket: bucket.into(),
        key: key.into(),
        body: Some(bytes_to_blob(body)),
        ..Default::default()
    };
    S3Request {
        input,
        method: http::Method::PUT,
        uri: format!("/{bucket}/{key}").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

fn get_request(bucket: &str, key: &str) -> S3Request<GetObjectInput> {
    let input = GetObjectInput {
        bucket: bucket.into(),
        key: key.into(),
        ..Default::default()
    };
    S3Request {
        input,
        method: http::Method::GET,
        uri: format!("/{bucket}/{key}").parse().unwrap(),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: None,
        region: None,
        service: None,
        trailing_headers: None,
    }
}

async fn read_back(resp: S3Response<GetObjectOutput>) -> Bytes {
    collect_blob(resp.output.body.expect("body"), 100 * 1024 * 1024)
        .await
        .expect("collect")
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn minio_roundtrip_through_s4_with_cpu_zstd() {
    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    ensure_bucket(&aws_client, "s4-test").await;

    let proxy = s3s_aws::Proxy::from(aws_client.clone());
    let s4 = S4Service::new(
        proxy,
        make_registry(),
        Arc::new(AlwaysDispatcher(CodecKind::CpuZstd)),
    );

    let payload = Bytes::from("the quick brown fox jumps over the lazy dog. ".repeat(2048));
    let original_size = payload.len();

    s4.put_object(put_request(
        "s4-test",
        "log/2026-05-12.log",
        payload.clone(),
    ))
    .await
    .expect("put");

    // 1. S4 経由で GET → 元バイト列が返る
    let resp = s4
        .get_object(get_request("s4-test", "log/2026-05-12.log"))
        .await
        .expect("get");
    let roundtripped = read_back(resp).await;
    assert_eq!(roundtripped, payload, "roundtrip body must match");

    // 2. raw aws-sdk-s3 で MinIO 上の実 object を直接読み、圧縮されていることを確認
    let raw = aws_client
        .get_object()
        .bucket("s4-test")
        .key("log/2026-05-12.log")
        .send()
        .await
        .expect("raw get");
    let raw_meta = raw.metadata().cloned().unwrap_or_default();
    assert_eq!(
        raw_meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd"),
        "MinIO 上の object には s4-codec メタが書かれているべき"
    );
    let raw_bytes = raw.body.collect().await.expect("body collect").into_bytes();
    assert!(
        raw_bytes.len() < original_size / 10,
        "expected zstd to compress repeated text by 10x+, got {} -> {} bytes",
        original_size,
        raw_bytes.len()
    );
}

#[cfg(feature = "nvcomp-gpu")]
#[tokio::test]
#[ignore = "requires Docker for MinIO container + CUDA-capable GPU + NVCOMP_HOME"]
async fn minio_roundtrip_through_s4_with_nvcomp_zstd() {
    use s4_codec::nvcomp::{NvcompZstdCodec, is_gpu_available};
    if !is_gpu_available() {
        eprintln!("skipping: no CUDA GPU detected at runtime");
        return;
    }

    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    ensure_bucket(&aws_client, "s4-gpu-test").await;

    let proxy = s3s_aws::Proxy::from(aws_client.clone());
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::NvcompZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(NvcompZstdCodec::new().expect("nvcomp init"))),
    );
    let s4 = S4Service::new(
        proxy,
        registry,
        Arc::new(AlwaysDispatcher(CodecKind::NvcompZstd)),
    );

    // 1 MB の repeated text — GPU zstd で大きく縮むはず
    let payload = Bytes::from("the quick brown fox ".repeat(50_000));
    let original_size = payload.len();

    s4.put_object(put_request(
        "s4-gpu-test",
        "log/2026-05-12.log",
        payload.clone(),
    ))
    .await
    .expect("put");

    // S4 経由 GET → 元バイト列が返る
    let resp = s4
        .get_object(get_request("s4-gpu-test", "log/2026-05-12.log"))
        .await
        .expect("get");
    let roundtripped = read_back(resp).await;
    assert_eq!(roundtripped, payload, "GPU compress + decompress roundtrip");

    // raw aws-sdk-s3 で MinIO 上の実 object を直接読み、GPU zstd で大きく縮んでいることを確認
    let raw = aws_client
        .get_object()
        .bucket("s4-gpu-test")
        .key("log/2026-05-12.log")
        .send()
        .await
        .expect("raw get");
    let raw_meta = raw.metadata().cloned().unwrap_or_default();
    assert_eq!(
        raw_meta.get("s4-codec").map(String::as_str),
        Some("nvcomp-zstd"),
        "MinIO 上の object には s4-codec=nvcomp-zstd が書かれているべき"
    );
    let raw_bytes = raw.body.collect().await.expect("body collect").into_bytes();
    assert!(
        raw_bytes.len() < original_size / 20,
        "expected GPU zstd to compress repeated text by 20x+, got {} -> {} bytes",
        original_size,
        raw_bytes.len()
    );
}

#[cfg(feature = "nvcomp-gpu")]
#[tokio::test]
#[ignore = "requires Docker for MinIO container + CUDA-capable GPU + NVCOMP_HOME"]
async fn minio_roundtrip_through_s4_with_nvcomp_bitcomp() {
    use s4_codec::nvcomp::{NvcompBitcompCodec, is_gpu_available};
    if !is_gpu_available() {
        eprintln!("skipping: no CUDA GPU detected at runtime");
        return;
    }

    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    ensure_bucket(&aws_client, "s4-gpu-test").await;

    let proxy = s3s_aws::Proxy::from(aws_client.clone());
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::NvcompBitcomp)
            .with(Arc::new(Passthrough))
            .with(Arc::new(
                NvcompBitcompCodec::default_general().expect("init"),
            )),
    );
    let s4 = S4Service::new(
        proxy,
        registry,
        Arc::new(AlwaysDispatcher(CodecKind::NvcompBitcomp)),
    );

    // Parquet 風 = 単調 i64 column 16 KB 分
    let mut payload: Vec<u8> = Vec::with_capacity(16384);
    for i in 0i64..2048 {
        payload.extend_from_slice(&i.to_le_bytes());
    }
    let payload = Bytes::from(payload);

    s4.put_object(put_request("s4-gpu-test", "col/i64.bin", payload.clone()))
        .await
        .expect("put");

    let resp = s4
        .get_object(get_request("s4-gpu-test", "col/i64.bin"))
        .await
        .expect("get");
    let roundtripped = read_back(resp).await;
    assert_eq!(roundtripped, payload);

    let raw = aws_client
        .get_object()
        .bucket("s4-gpu-test")
        .key("col/i64.bin")
        .send()
        .await
        .expect("raw get");
    let raw_meta = raw.metadata().cloned().unwrap_or_default();
    assert_eq!(
        raw_meta.get("s4-codec").map(String::as_str),
        Some("nvcomp-bitcomp")
    );
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn minio_sampling_dispatcher_skips_already_compressed() {
    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    ensure_bucket(&aws_client, "s4-test").await;

    let proxy = s3s_aws::Proxy::from(aws_client.clone());
    let s4 = S4Service::new(
        proxy,
        make_registry(),
        Arc::new(SamplingDispatcher::new(CodecKind::CpuZstd)),
    );

    // gzip magic + ランダムっぽい bytes
    let mut gz_payload = vec![0x1f, 0x8b, 0x08, 0x00];
    let mut state: u64 = 0xc0ffee;
    for _ in 0..4096 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        gz_payload.push((state >> 33) as u8);
    }
    let gz_payload = Bytes::from(gz_payload);
    let orig_len = gz_payload.len();

    s4.put_object(put_request("s4-test", "blob.gz", gz_payload.clone()))
        .await
        .expect("put");

    // codec が passthrough になっていることを raw GET で確認
    let raw = aws_client
        .get_object()
        .bucket("s4-test")
        .key("blob.gz")
        .send()
        .await
        .expect("raw get");
    let raw_meta = raw.metadata().cloned().unwrap_or_default();
    assert_eq!(
        raw_meta.get("s4-codec").map(String::as_str),
        Some("passthrough"),
        "既圧縮データには passthrough を選ぶべき (SamplingDispatcher)"
    );
    let raw_bytes = raw.body.collect().await.expect("body collect").into_bytes();
    assert_eq!(
        raw_bytes.len(),
        orig_len,
        "passthrough なので raw object size は元と同じであるべき"
    );

    // S4 経由 GET で当然 roundtrip
    let resp = s4
        .get_object(get_request("s4-test", "blob.gz"))
        .await
        .expect("get");
    let roundtripped = read_back(resp).await;
    assert_eq!(roundtripped, gz_payload);
}
