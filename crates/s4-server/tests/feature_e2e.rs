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

// ===========================================================================
// v0.7 #47 — SigV4a verify gate end-to-end against a real hyper listener.
// ===========================================================================
//
// This test stands up a `HealthRouter` wrapped around a tiny "echo OK"
// inner service and binds it to a 127.0.0.1:0 socket. We then issue
// raw HTTP requests via reqwest:
//
// - One signed with a SigV4a-shaped Authorization header whose
//   ECDSA-P-256 signature is valid → 200.
// - The same request with one byte of the signature flipped → 403
//   `SignatureDoesNotMatch`.
//
// Unlike the lifecycle tests above, no MinIO container is required — the
// SigV4a gate sits at the HTTP layer in front of the s3s framework, so
// we can swap in a noop inner service without losing test coverage of
// the wire path. The signing logic uses our own
// `build_canonical_request_bytes` helper because no AWS SDK currently
// supports SigV4a request signing for S3 outside of MRAP / EventBridge,
// and reproducing the AWS-exact canonical-request byte sequence from
// scratch (URI percent-encoding edge cases, query-string sorting,
// multi-value header collation) would be a feature in its own right.

use bytes::Bytes;
use http::Method as HttpMethod;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::Service;
use hyper_util::rt::TokioIo;
use p256::ecdsa::SigningKey;
use p256::ecdsa::signature::Signer;
use rand::rngs::OsRng;
use s4_server::routing::HealthRouter;
use s4_server::service::SigV4aGate;
use s4_server::sigv4a::{REGION_SET_HEADER, SigV4aCredentialStore};
use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use tokio::net::TcpListener;

/// Minimal inner service that returns a fixed 200 OK for any request.
/// Used only by the SigV4a E2E test below — keeps the test focused on
/// the verify gate path without dragging in `s3s_aws::Proxy` + a backend
/// container.
#[derive(Clone)]
struct AlwaysOk;

impl Service<http::Request<Incoming>> for AlwaysOk {
    type Response = http::Response<s3s::Body>;
    type Error = s3s::HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, _req: http::Request<Incoming>) -> Self::Future {
        Box::pin(async move {
            let body = Bytes::from_static(b"inner-ok");
            Ok(http::Response::builder()
                .status(200)
                .header("content-length", body.len().to_string())
                .body(s3s::Body::http_body(http_body_util::BodyExt::map_err(
                    Full::new(body),
                    |never: Infallible| match never {},
                )))
                .expect("ok response"))
        })
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Reproduce the SigV4 canonical-request format the routing-layer
/// middleware expects (lifted byte-for-byte from
/// `routing::build_canonical_request_bytes`). Kept here as a tiny copy
/// rather than re-exposing the helper publicly because it is only ever
/// useful in this end-to-end shape test.
fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    signed_headers: &[(&str, &str)],
    payload_hash: &str,
) -> Vec<u8> {
    let mut buf = String::new();
    buf.push_str(method);
    buf.push('\n');
    buf.push_str(path);
    buf.push('\n');
    if query.is_empty() {
        buf.push('\n');
    } else {
        let mut pairs: Vec<(&str, &str)> = query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|kv| kv.split_once('=').unwrap_or((kv, "")))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(b.1)));
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                buf.push('&');
            }
            buf.push_str(k);
            buf.push('=');
            buf.push_str(v);
        }
        buf.push('\n');
    }
    for (name, value) in signed_headers {
        buf.push_str(name);
        buf.push(':');
        buf.push_str(value.trim());
        buf.push('\n');
    }
    buf.push('\n');
    let names: Vec<&str> = signed_headers.iter().map(|(n, _)| *n).collect();
    buf.push_str(&names.join(";"));
    buf.push('\n');
    buf.push_str(payload_hash);
    buf.into_bytes()
}

#[tokio::test]
async fn sigv4a_verify_real_listener_e2e() {
    // Boot the SigV4a gate with a fresh keypair under access-key-id
    // "AKIAE2E", wrap a `HealthRouter` around a noop inner service,
    // and bind to a random port.
    let signing = SigningKey::random(&mut OsRng);
    let verifying = p256::ecdsa::VerifyingKey::from(&signing);
    let mut keys = HashMap::new();
    keys.insert("AKIAE2E".to_string(), verifying);
    let store = std::sync::Arc::new(SigV4aCredentialStore::from_map(keys));
    let gate = std::sync::Arc::new(SigV4aGate::new(store));

    let router = HealthRouter::new(AlwaysOk, None)
        .with_sigv4a_gate(std::sync::Arc::clone(&gate))
        .with_region("us-east-1");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local addr");
    let server_router = router.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let io = TokioIo::new(stream);
            let svc = server_router.clone();
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(
                        io,
                        hyper::service::service_fn(move |req| {
                            let svc = svc.clone();
                            async move { svc.call(req).await }
                        }),
                    )
                    .await;
            });
        }
    });

    // Build the canonical bytes the same way the middleware will, then
    // sign over them.
    let host = format!("127.0.0.1:{}", local_addr.port());
    let signed_headers = [
        ("host", host.as_str()),
        (
            "x-amz-content-sha256",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        ("x-amz-date", "20260513T120000Z"),
        (REGION_SET_HEADER, "us-east-1"),
    ];
    let canonical = canonical_request(
        "GET",
        "/test-bucket/key",
        "",
        &signed_headers,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
    let sig: p256::ecdsa::Signature = signing.sign(&canonical);
    let sig_hex = lower_hex(sig.to_der().as_bytes());

    let names: Vec<&str> = signed_headers.iter().map(|(n, _)| *n).collect();
    let auth = format!(
        "AWS4-ECDSA-P256-SHA256 \
         Credential=AKIAE2E/20260513/s3/aws4_request, \
         SignedHeaders={}, \
         Signature={sig_hex}",
        names.join(";")
    );

    let client = reqwest::Client::builder()
        .build()
        .expect("reqwest client");
    let url = format!("http://{host}/test-bucket/key");

    // Happy path: valid signature → 200 from the inner AlwaysOk.
    let resp = client
        .request(HttpMethod::GET, &url)
        .header(
            "x-amz-content-sha256",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .header("x-amz-date", "20260513T120000Z")
        .header(REGION_SET_HEADER, "us-east-1")
        .header("authorization", &auth)
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "valid SigV4a signature must reach inner service: body={:?}",
        resp.text().await.ok()
    );

    // Tamper path: flip one signature hex char → 403
    // `SignatureDoesNotMatch` from the gate, inner service must NOT see
    // the request.
    let mut chars: Vec<char> = auth.chars().collect();
    let last = chars.len() - 1;
    chars[last] = if chars[last] == '0' { '1' } else { '0' };
    let tampered_auth: String = chars.into_iter().collect();
    let resp = client
        .request(HttpMethod::GET, &url)
        .header(
            "x-amz-content-sha256",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .header("x-amz-date", "20260513T120000Z")
        .header(REGION_SET_HEADER, "us-east-1")
        .header("authorization", &tampered_auth)
        .send()
        .await
        .expect("send tampered");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::FORBIDDEN,
        "tampered SigV4a signature must be rejected by the gate"
    );
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("<Code>SignatureDoesNotMatch</Code>"),
        "403 body must surface SignatureDoesNotMatch: {body}"
    );
}
