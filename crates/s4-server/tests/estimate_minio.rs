//! `s4 estimate` E2E against a real MinIO container: PUT a known mix of
//! objects directly to the backend (no S4 gateway involved — estimate is
//! a pre-deployment tool), run `s4_server::estimate::run_estimate`, and
//! check the report's inventory / stratification / extrapolation against
//! ground truth. Docker required, so gated with `#[ignore]` exactly like
//! `minio_e2e.rs`:
//!
//! ```bash
//! cargo test --test estimate_minio -- --ignored --nocapture
//! ```

use s4_codec::CodecKind;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::SamplingDispatcher;
use s4_server::estimate::{
    DEFAULT_MAX_LIST_KEYS, DEFAULT_MAX_SAMPLE_BYTES, DEFAULT_PRICE_PER_GB_MONTH,
    DEFAULT_SAMPLES_PER_STRATUM, DEFAULT_SEED, EstimateParams, run_estimate,
};
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

fn default_params() -> EstimateParams {
    EstimateParams {
        prefix: None,
        max_list_keys: DEFAULT_MAX_LIST_KEYS,
        samples_per_stratum: DEFAULT_SAMPLES_PER_STRATUM,
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        seed: DEFAULT_SEED,
        price_per_gb_month: DEFAULT_PRICE_PER_GB_MONTH,
        default_codec: CodecKind::CpuZstd,
        zstd_level: CpuZstd::DEFAULT_LEVEL,
        use_sampling_dispatcher: true,
        gpu_min_bytes: SamplingDispatcher::DEFAULT_GPU_MIN_BYTES,
        prefer_columnar_gpu: false,
        simulate_gpu: false,
        gpu_present: false,
    }
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn estimate_against_minio_known_mix() {
    let fixture = start_minio().await;
    let client = build_aws_client(&fixture.endpoint_url).await;
    let bucket = "s4-estimate-test";
    client
        .create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create bucket");

    // Known mix: 3 highly compressible logs, 1 incompressible (random)
    // .bin, 1 extension-less, and a `.s4index` sidecar that MUST be
    // excluded from the inventory.
    let log_body =
        "level=info msg=\"request handled\" path=/api/v1/items status=200\n".repeat(2048);
    for i in 0..3 {
        client
            .put_object()
            .bucket(bucket)
            .key(format!("logs/app-{i}.log"))
            .body(log_body.clone().into_bytes().into())
            .send()
            .await
            .expect("put log");
    }
    // Pseudo-random bytes (xorshift) — high entropy, dispatcher should
    // pick passthrough.
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut random_body = Vec::with_capacity(64 * 1024);
    while random_body.len() < 64 * 1024 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        random_body.extend_from_slice(&x.to_le_bytes());
    }
    client
        .put_object()
        .bucket(bucket)
        .key("blobs/noise.bin")
        .body(random_body.clone().into())
        .send()
        .await
        .expect("put bin");
    client
        .put_object()
        .bucket(bucket)
        .key("README")
        .body(b"plain text readme contents, short".to_vec().into())
        .send()
        .await
        .expect("put readme");
    client
        .put_object()
        .bucket(bucket)
        .key("logs/app-0.log.s4index")
        .body(b"not a real sidecar, must be excluded".to_vec().into())
        .send()
        .await
        .expect("put sidecar");
    // Other S4-internal keys that MUST be excluded too: a `.s4dict/`
    // shared dictionary (train-dict output) and a `.__s4ver__/`
    // versioning shadow key. Both live under `logs/` shapes that would
    // otherwise change total_objects / the prefix-scoped count below.
    client
        .put_object()
        .bucket(bucket)
        .key(".s4dict/0123456789abcdef")
        .body(b"dictionary bytes, must be excluded".to_vec().into())
        .send()
        .await
        .expect("put dict");
    client
        .put_object()
        .bucket(bucket)
        .key("logs/app-0.log.__s4ver__/9c1f8c4e-0001")
        .body(log_body.clone().into_bytes().into())
        .send()
        .await
        .expect("put version shadow");

    let params = default_params();
    let report = run_estimate(&client, bucket, &params)
        .await
        .expect("estimate");

    // Inventory: 5 objects (sidecar / dict / version shadow excluded).
    assert_eq!(report.total_objects, 5, "report: {report:?}");
    assert!(!report.listing_truncated);
    let expected_total = (log_body.len() * 3
        + random_body.len()
        + b"plain text readme contents, short".len()) as u64;
    assert_eq!(report.total_bytes, expected_total);

    // Strata: (none) for README, .bin, .log — and NOT .s4index.
    let names: Vec<&str> = report.strata.iter().map(|s| s.stratum.as_str()).collect();
    assert_eq!(names, vec!["(none)", ".bin", ".log"]);

    // Every object fits under samples_per_stratum=8, so coverage is 100%.
    assert_eq!(report.sampled_objects, 5);
    assert!((report.sampled_fraction_of_total_bytes - 1.0).abs() < 1e-9);

    // The repetitive logs must compress hard; the random blob must not.
    let log_stratum = report
        .strata
        .iter()
        .find(|s| s.stratum == ".log")
        .expect(".log stratum");
    assert!(
        log_stratum.ratio < 0.10,
        "repetitive logs should compress >10x, got ratio {}",
        log_stratum.ratio
    );
    assert_eq!(log_stratum.codecs.len(), 1);
    assert_eq!(log_stratum.codecs[0].codec, "cpu-zstd");
    let bin_stratum = report
        .strata
        .iter()
        .find(|s| s.stratum == ".bin")
        .expect(".bin stratum");
    assert!(
        bin_stratum.ratio > 0.95,
        "random bytes should not compress, got ratio {}",
        bin_stratum.ratio
    );
    assert_eq!(bin_stratum.codecs[0].codec, "passthrough");

    // Extrapolation arithmetic: projected total = Σ stratum projections,
    // and the overall ratio matches.
    let sum: u64 = report.strata.iter().map(|s| s.projected_bytes).sum();
    assert_eq!(report.projected_total_bytes, sum);
    assert!(report.projected_total_bytes < report.total_bytes);
    let expected_ratio = report.projected_total_bytes as f64 / report.total_bytes as f64;
    assert!((report.overall_ratio - expected_ratio).abs() < 1e-12);
    assert!(report.projected_monthly_cost_usd < report.current_monthly_cost_usd);

    // Determinism: same seed -> byte-identical JSON report.
    let report2 = run_estimate(&client, bucket, &params)
        .await
        .expect("estimate rerun");
    assert_eq!(
        serde_json::to_string(&report).expect("json"),
        serde_json::to_string(&report2).expect("json2"),
        "same seed must produce an identical report"
    );

    // Prefix scoping: only the logs (the `logs/...__s4ver__/` shadow
    // key under the same prefix stays excluded).
    let prefixed = EstimateParams {
        prefix: Some("logs/".into()),
        ..default_params()
    };
    let scoped = run_estimate(&client, bucket, &prefixed)
        .await
        .expect("scoped estimate");
    assert_eq!(scoped.total_objects, 3);
    assert_eq!(scoped.total_bytes, (log_body.len() * 3) as u64);

    // Empty prefix: 0 objects, "no objects found" note, exit-0 path.
    let empty = EstimateParams {
        prefix: Some("does-not-exist/".into()),
        ..default_params()
    };
    let none = run_estimate(&client, bucket, &empty)
        .await
        .expect("empty estimate");
    assert_eq!(none.total_objects, 0);
    assert!(none.notes.iter().any(|n| n == "no objects found"));
}
