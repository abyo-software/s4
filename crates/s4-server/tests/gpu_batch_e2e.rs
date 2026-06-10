//! E2E acceptance for v1.2 `--gpu-batch-small-puts` (GPU small-PUT batch
//! compression).
//!
//! Run with:
//!   NVCOMP_HOME=/path/to/nvcomp-archive cargo test -p s4-server \
//!     --features nvcomp-gpu --test gpu_batch_e2e -- --ignored --test-threads=1
//!
//! Requires Docker (testcontainers MinIO) + a CUDA-capable GPU +
//! NVCOMP_HOME at build time, hence the `#[ignore]` gates (same regime as
//! `minio_e2e.rs` / `gpu_streaming.rs`).
//!
//! What is proven here (the wire-compat acceptance from the design):
//!
//! 1. Many concurrent small PUTs (below `--gpu-min-bytes`, where the
//!    dispatcher picks cpu-zstd) get compressed through the nvCOMP batch
//!    aggregator and land in MinIO as **standard `nvcomp-zstd` objects**
//!    (`s4-codec: nvcomp-zstd` metadata, unframed buffered shape).
//! 2. The **unmodified GET path** — per-buffer nvCOMP decompress, zero
//!    batch awareness — returns every body byte-equal.
//! 3. Incompressible bodies (batched output >= input) fall back to the
//!    pre-existing cpu-zstd framed path and still round-trip.

#![cfg(feature = "nvcomp-gpu")]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use s3s::S3;
use s3s::dto::*;
use s3s::{S3Request, S3Response};
use s4_codec::CodecDispatcher;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::{AlwaysDispatcher, SamplingDispatcher};
use s4_codec::nvcomp::{NvcompZstdCodec, is_gpu_available};
use s4_codec::nvcomp_batched::NvcompZstdBatchEncoder;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::blob::{bytes_to_blob, collect_blob};
use s4_server::gpu_batch::{GpuBatchConfig, spawn};
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

const MINIO_USER: &str = "minioadmin";
const MINIO_PASS: &str = "minioadmin";
const GPU_MIN_BYTES: u64 = 1024 * 1024;

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

/// Mirrors the production `main.rs` wiring: registry with CPU + GPU zstd,
/// sampling dispatcher with GPU preference (so bodies under
/// `--gpu-min-bytes` pick cpu-zstd — the exact population the batch path
/// targets), and the batch aggregator attached via `with_gpu_batch`.
fn build_s4_with_batch(
    aws_client: &aws_sdk_s3::Client,
    window: Duration,
    max_items: usize,
    dispatcher: Arc<dyn CodecDispatcher>,
) -> S4Service<s3s_aws::Proxy> {
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default()))
            .with(Arc::new(NvcompZstdCodec::new().expect("nvcomp init"))),
    );
    let encoder = Arc::new(NvcompZstdBatchEncoder::new().expect("batch encoder init"));
    let handle = spawn(
        encoder,
        GpuBatchConfig {
            max_items,
            window,
            floor_bytes: 4096,
            max_bytes: GPU_MIN_BYTES,
            queue_depth: 256,
        },
    );
    let proxy = s3s_aws::Proxy::from(aws_client.clone());
    S4Service::new(proxy, registry, dispatcher).with_gpu_batch(handle)
}

fn sampling_gpu_dispatcher() -> Arc<dyn CodecDispatcher> {
    Arc::new(
        SamplingDispatcher::new(CodecKind::CpuZstd)
            .with_gpu_preference(true, GPU_MIN_BYTES as usize),
    )
}

fn put_request(bucket: &str, key: &str, body: Bytes) -> S3Request<PutObjectInput> {
    let len = body.len() as i64;
    let input = PutObjectInput {
        bucket: bucket.into(),
        key: key.into(),
        body: Some(bytes_to_blob(body)),
        // The batch eligibility gate keys off the declared Content-Length
        // (chunked transfers stay on the unchanged path), so set it the
        // way a real SDK PUT does.
        content_length: Some(len),
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

/// Deterministic compressible small body, unique per index so response
/// pairing bugs (oneshot cross-delivery) would show up as byte mismatches.
fn small_body(i: usize) -> Bytes {
    let line = format!("object-{i:04} the quick brown fox jumps over the lazy dog\n");
    let mut v = Vec::with_capacity(8 * 1024 + i);
    while v.len() < 8 * 1024 + i {
        v.extend_from_slice(line.as_bytes());
    }
    Bytes::from(v)
}

/// Deterministic pseudo-random (incompressible) small body.
fn noise_body(n: usize, seed: u64) -> Bytes {
    let mut state = seed;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push((state >> 33) as u8);
    }
    Bytes::from(v)
}

/// Acceptance 1 + 2: concurrent small PUTs batch onto the GPU, land as
/// standard nvcomp-zstd objects, and the unmodified GET path returns every
/// body byte-equal.
#[tokio::test]
#[ignore = "requires Docker for MinIO container + CUDA-capable GPU + NVCOMP_HOME"]
async fn gpu_batched_small_puts_roundtrip_via_unmodified_get_path() {
    if !is_gpu_available() {
        eprintln!("skipping: no CUDA GPU detected at runtime");
        return;
    }
    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    let _ = aws_client
        .create_bucket()
        .bucket("s4-gpu-batch")
        .send()
        .await;

    // 50 ms window so the join_all burst below reliably coalesces into
    // multi-item batches regardless of scheduler jitter.
    let s4 = build_s4_with_batch(
        &aws_client,
        Duration::from_millis(50),
        32,
        sampling_gpu_dispatcher(),
    );

    const N: usize = 48;
    let bodies: Vec<Bytes> = (0..N).map(small_body).collect();

    // Concurrent PUT burst — all in flight at once so the aggregator
    // actually batches them.
    let puts: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| {
            s4.put_object(put_request(
                "s4-gpu-batch",
                &format!("small/{i}"),
                b.clone(),
            ))
        })
        .collect();
    for (i, res) in futures::future::join_all(puts)
        .await
        .into_iter()
        .enumerate()
    {
        res.unwrap_or_else(|e| panic!("put {i}: {e}"));
    }

    for (i, body) in bodies.iter().enumerate() {
        // GET through S4 — the per-buffer decompress path with no batch
        // awareness whatsoever.
        let resp = s4
            .get_object(get_request("s4-gpu-batch", &format!("small/{i}")))
            .await
            .unwrap_or_else(|e| panic!("get {i}: {e}"));
        let roundtripped = read_back(resp).await;
        assert_eq!(&roundtripped, body, "object {i} byte mismatch");

        // Raw MinIO read: the stored object must be a standard
        // nvcomp-zstd body (proof the batch path actually ran — the
        // dispatcher alone would have stored cpu-zstd at this size) in
        // the unframed buffered shape, and visibly compressed.
        let raw = aws_client
            .get_object()
            .bucket("s4-gpu-batch")
            .key(format!("small/{i}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("raw get {i}: {e}"));
        let meta = raw.metadata().cloned().unwrap_or_default();
        assert_eq!(
            meta.get("s4-codec").map(String::as_str),
            Some("nvcomp-zstd"),
            "object {i} should be stored as batch-compressed nvcomp-zstd"
        );
        assert_eq!(
            meta.get("s4-framed"),
            None,
            "object {i} batch path stores the raw-blob (unframed) shape"
        );
        let raw_bytes = raw.body.collect().await.expect("collect").into_bytes();
        assert!(
            raw_bytes.len() < body.len() / 2,
            "object {i}: expected >2x compression on repeated text, got {} -> {}",
            body.len(),
            raw_bytes.len()
        );
    }
}

/// Acceptance 3: incompressible small bodies make the batched output >=
/// input; the PUT must fall back to the pre-existing cpu-zstd framed path
/// and still round-trip byte-equal.
#[tokio::test]
#[ignore = "requires Docker for MinIO container + CUDA-capable GPU + NVCOMP_HOME"]
async fn gpu_batch_ratio_fallback_stores_cpu_zstd_frames() {
    if !is_gpu_available() {
        eprintln!("skipping: no CUDA GPU detected at runtime");
        return;
    }
    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    let _ = aws_client
        .create_bucket()
        .bucket("s4-gpu-batch-fb")
        .send()
        .await;

    // AlwaysDispatcher: the sampling dispatcher would route high-entropy
    // noise to passthrough before the batch gate is consulted; forcing
    // cpu-zstd is the way to drive an incompressible body INTO the batch
    // path so the ratio fallback fires.
    let s4 = build_s4_with_batch(
        &aws_client,
        Duration::from_millis(5),
        32,
        Arc::new(AlwaysDispatcher(CodecKind::CpuZstd)),
    );

    let body = noise_body(16 * 1024, 0xDEADBEEF);
    s4.put_object(put_request("s4-gpu-batch-fb", "noise/0", body.clone()))
        .await
        .expect("put noise");

    let resp = s4
        .get_object(get_request("s4-gpu-batch-fb", "noise/0"))
        .await
        .expect("get noise");
    let roundtripped = read_back(resp).await;
    assert_eq!(roundtripped, body, "fallback roundtrip byte mismatch");

    let raw = aws_client
        .get_object()
        .bucket("s4-gpu-batch-fb")
        .key("noise/0")
        .send()
        .await
        .expect("raw get");
    let meta = raw.metadata().cloned().unwrap_or_default();
    assert_eq!(
        meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd"),
        "incompressible body must fall back to the cpu-zstd framed path"
    );
    assert_eq!(
        meta.get("s4-framed").map(String::as_str),
        Some("true"),
        "fallback objects keep the framed shape the flag-off path produces"
    );
}

/// Bodies outside the batch window (here: a 2 MiB body, above
/// --gpu-min-bytes) must be untouched by the batch path — the sampling
/// dispatcher promotes them to the per-object GPU path as before.
#[tokio::test]
#[ignore = "requires Docker for MinIO container + CUDA-capable GPU + NVCOMP_HOME"]
async fn bodies_outside_window_take_per_object_path() {
    if !is_gpu_available() {
        eprintln!("skipping: no CUDA GPU detected at runtime");
        return;
    }
    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    let _ = aws_client
        .create_bucket()
        .bucket("s4-gpu-batch-big")
        .send()
        .await;

    let s4 = build_s4_with_batch(
        &aws_client,
        Duration::from_millis(5),
        32,
        sampling_gpu_dispatcher(),
    );

    let body = Bytes::from("the quick brown fox ".repeat(110_000)); // ~2.1 MiB
    s4.put_object(put_request("s4-gpu-batch-big", "big/0", body.clone()))
        .await
        .expect("put big");

    let resp = s4
        .get_object(get_request("s4-gpu-batch-big", "big/0"))
        .await
        .expect("get big");
    let roundtripped = read_back(resp).await;
    assert_eq!(roundtripped, body, "big-object roundtrip byte mismatch");

    let raw = aws_client
        .get_object()
        .bucket("s4-gpu-batch-big")
        .key("big/0")
        .send()
        .await
        .expect("raw get");
    let meta = raw.metadata().cloned().unwrap_or_default();
    assert_eq!(
        meta.get("s4-codec").map(String::as_str),
        Some("nvcomp-zstd"),
        ">= gpu-min-bytes body takes the existing per-object GPU promotion"
    );
}
