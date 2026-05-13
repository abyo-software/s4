//! v0.7 #45 — Lifecycle scanner E2E against the MinIO container backend.
//!
//! Boots a MinIO container, points an `S4Service<s3s_aws::Proxy>` at it
//! with a `LifecycleManager` attached, PUTs three objects under two
//! prefixes, then drives the v0.7 #45 `run_scan_once` end-to-end:
//!
//! 1. Object under the rule's prefix gets DELETEd from MinIO (verified
//!    by a raw `aws-sdk-s3` `head_object` returning `NoSuchKey`).
//! 2. Objects outside the prefix survive (raw HEAD returns 200).
//! 3. The `ScanReport` counter agrees with the backend post-condition.
//!
//! Because the backend stamps `last_modified` itself (we cannot fake an
//! object into the past), this test uses an `expire_after_days(0)` rule
//! — every object whose age is `>= 0d` matches, which is every backend
//! object. This is also the canonical "operator just enabled aggressive
//! expiration" scenario.
//!
//! ## Running
//!
//! ```bash
//! cargo test --test feature_e2e -- --ignored --nocapture
//! ```
//!
//! Requires Docker (the test starts a MinIO container via
//! `testcontainers-modules`).

use std::sync::Arc;

use s4_codec::dispatcher::AlwaysDispatcher;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::lifecycle::{LifecycleConfig, LifecycleManager, LifecycleRule, run_scan_once};
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
        CodecRegistry::new(CodecKind::Passthrough).with(Arc::new(Passthrough)),
    )
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn lifecycle_scanner_expires_objects_via_minio_backend() {
    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    let bucket = "s4-lc-test";
    ensure_bucket(&aws_client, bucket).await;

    // PUT three objects via the raw aws-sdk-s3 client (the scanner
    // walks the backend irrespective of the codec layer; using raw PUT
    // keeps the test focused on the lifecycle decision path rather
    // than re-validating the codec roundtrip already covered in
    // minio_e2e.rs).
    for (key, body) in [
        ("expirable/log-1.txt", "x"),
        ("expirable/log-2.txt", "y"),
        ("keep/important.bin", "z"),
    ] {
        aws_client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from(body.as_bytes().to_vec()))
            .send()
            .await
            .expect("seed put");
    }

    // Build the S4Service with a Lifecycle manager attached. Rule
    // expires every object under `expirable/` immediately.
    let proxy = s3s_aws::Proxy::from(aws_client.clone());
    let mgr = Arc::new(LifecycleManager::new());
    let mut rule = LifecycleRule::expire_after_days("e2e-rule", 0);
    rule.filter.prefix = Some("expirable/".into());
    mgr.put(bucket, LifecycleConfig { rules: vec![rule] });
    let s4 = Arc::new(
        S4Service::new(
            proxy,
            make_registry(),
            Arc::new(AlwaysDispatcher(CodecKind::Passthrough)),
        )
        .with_lifecycle(Arc::clone(&mgr)),
    );

    // Drive the v0.7 #45 scanner end-to-end against the MinIO backend.
    let report = run_scan_once(&s4).await.expect("scan");
    eprintln!("scan report: {report:?}");
    assert_eq!(report.buckets_scanned, 1);
    assert_eq!(report.objects_evaluated, 3);
    assert_eq!(report.expired, 2, "two objects under `expirable/` must expire");
    assert_eq!(report.transitioned, 0);
    assert_eq!(report.skipped_locked, 0);
    assert_eq!(report.action_errors, 0);

    // Verify backend post-condition via raw aws-sdk-s3 HEAD calls.
    for gone in ["expirable/log-1.txt", "expirable/log-2.txt"] {
        let res = aws_client.head_object().bucket(bucket).key(gone).send().await;
        assert!(
            res.is_err(),
            "{gone} should have been deleted from MinIO; got {res:?}"
        );
    }
    let kept = aws_client
        .head_object()
        .bucket(bucket)
        .key("keep/important.bin")
        .send()
        .await;
    assert!(kept.is_ok(), "keep/important.bin must survive: {kept:?}");

    // The lifecycle manager's per-bucket counter records the actions.
    let snap = mgr.actions_snapshot();
    assert_eq!(
        snap.get(&(bucket.into(), "expire".into())).copied(),
        Some(2)
    );
}

/// Spec note (`evaluate_batch` direct logic check): even though the
/// backend's `last_modified` cannot be aged backward, the evaluator
/// itself takes the age as an explicit `Duration` argument. This test
/// drives the same rule shape as the scanner above with a forged 90d
/// age + a tag filter, asserting that the rule fires only on the
/// matching tag set. Acts as the "logic verification" companion the
/// v0.7 #45 spec calls out (real walk + delete is exercised by the
/// preceding test).
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn lifecycle_evaluate_batch_logic_against_minio_backed_service() {
    use chrono::Duration;
    use s4_server::lifecycle::evaluate_batch;

    let fixture = start_minio().await;
    let aws_client = build_aws_client(&fixture.endpoint_url).await;
    let bucket = "s4-lc-eval";
    ensure_bucket(&aws_client, bucket).await;

    let mgr = LifecycleManager::new();
    let mut rule = LifecycleRule::expire_after_days("tagged-only", 30);
    rule.filter.tags = vec![("env".into(), "dev".into())];
    mgr.put(bucket, LifecycleConfig { rules: vec![rule] });

    let inputs = vec![
        (
            "tagged.log".to_string(),
            Duration::days(90),
            10u64,
            vec![("env".to_string(), "dev".to_string())],
        ),
        (
            "untagged.log".to_string(),
            Duration::days(90),
            10u64,
            vec![],
        ),
        (
            "wrong-tag.log".to_string(),
            Duration::days(90),
            10u64,
            vec![("env".to_string(), "prod".to_string())],
        ),
    ];
    let actions = evaluate_batch(&mgr, bucket, &inputs);
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].0, "tagged.log");
}
