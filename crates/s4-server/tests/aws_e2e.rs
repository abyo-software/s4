//! Real AWS S3 integration test (issue #3).
//!
//! Mirrors `multipart_e2e.rs` but routes the S4 server at the real AWS
//! endpoint instead of MinIO. Run only by the `aws-e2e.yml` GitHub Actions
//! workflow (or manually with the env vars set):
//!
//! ```text
//! AWS_E2E_BUCKET=...        # provisioned by infra/aws-e2e/main.tf
//! AWS_E2E_REGION=us-east-1
//! AWS_E2E_PREFIX=local/$USER/$(date +%s)   # to scope object keys
//! ```
//!
//! Skipped by default — `--ignored` gates each test.
//!
//! AWS credentials are taken from the standard AWS chain (env vars,
//! ~/.aws/credentials, IAM role, OIDC web-identity from the workflow).
//! The workflow assumes a least-privilege role via OIDC; locally you can
//! `aws sso login` or export `AWS_PROFILE`.

use std::sync::Arc;
use std::sync::OnceLock;

use aws_sdk_s3::config::{BehaviorVersion, Region};
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
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Sentinel credentials passed to the in-process S4 server's auth layer.
/// Real AWS auth happens at the s3s_aws::Proxy → backend boundary using the
/// environment-provided creds (OIDC role / AWS_PROFILE / etc.).
const S4_FRONT_USER: &str = "s4-aws-e2e";
const S4_FRONT_PASS: &str = "s4-aws-e2e-secret";

/// Returns Some(value) if set; on miss, prints a one-line skip notice and
/// returns None so the caller can early-return — the test then shows as
/// "passed" (because we just don't run the body), which keeps the MinIO
/// CI job clean while still letting the AWS workflow exercise the body.
fn opt_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) => Some(v),
        Err(_) => {
            eprintln!(
                "[aws-e2e] skipping: env var {key} not set. \
                 See infra/aws-e2e/README.md for setup."
            );
            None
        }
    }
}

fn aws_credentials_present() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        // Either an explicit env var…
        if std::env::var("AWS_ACCESS_KEY_ID").is_ok()
            || std::env::var("AWS_PROFILE").is_ok()
            || std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE").is_ok()
        {
            return true;
        }
        // …or a usable credentials/config file with a [default] profile.
        // Covers the `aws configure` / `aws sso login` happy path without
        // forcing the user to set AWS_PROFILE=default.
        let home = std::env::var("HOME").unwrap_or_default();
        let creds_default = std::path::Path::new(&home)
            .join(".aws/credentials")
            .exists();
        let config_default = std::path::Path::new(&home).join(".aws/config").exists();
        creds_default || config_default
    })
}

/// Pull the three required env vars + AWS creds together. Returns None
/// (after printing skip notice) if any are missing.
fn aws_e2e_config() -> Option<(String, String, String)> {
    let bucket = opt_env("AWS_E2E_BUCKET")?;
    let region = opt_env("AWS_E2E_REGION")?;
    let prefix = opt_env("AWS_E2E_PREFIX")?;
    if !aws_credentials_present() {
        eprintln!(
            "[aws-e2e] skipping: AWS credentials not in env \
             (AWS_ACCESS_KEY_ID / AWS_PROFILE / AWS_WEB_IDENTITY_TOKEN_FILE)."
        );
        return None;
    }
    Some((bucket, region, prefix))
}

async fn build_aws_client(region: &str) -> aws_sdk_s3::Client {
    // Caller has already validated env via aws_e2e_config(); no panic here.
    let conf = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region.to_owned()))
        .load()
        .await;
    aws_sdk_s3::Client::new(&conf)
}

async fn spawn_s4_server(region: &str) -> (String, oneshot::Sender<()>) {
    let backend_client = build_aws_client(region).await;
    let proxy = s3s_aws::Proxy::from(backend_client);
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    );
    let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::CpuZstd));
    let s4 = S4Service::new(proxy, registry, dispatcher);

    let mut svc = S3ServiceBuilder::new(s4);
    svc.set_auth(SimpleAuth::from_single(S4_FRONT_USER, S4_FRONT_PASS));
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

fn build_s4_client(endpoint_url: &str, region: &str) -> aws_sdk_s3::Client {
    let creds = aws_sdk_s3::config::Credentials::new(
        S4_FRONT_USER,
        S4_FRONT_PASS,
        None,
        None,
        "s4-front-test",
    );
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(endpoint_url)
        .credentials_provider(creds)
        .region(Region::new(region.to_owned()))
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

fn key(prefix: &str, name: &str) -> String {
    format!("{prefix}/{name}")
}

/// Cleanup helper — best-effort delete a key from real S3.
async fn cleanup(client: &aws_sdk_s3::Client, bucket: &str, k: &str) {
    let _ = client.delete_object().bucket(bucket).key(k).send().await;
    // sidecar (might or might not exist)
    let _ = client
        .delete_object()
        .bucket(bucket)
        .key(format!("{k}.s4index"))
        .send()
        .await;
}

#[tokio::test]
#[ignore = "AWS E2E — requires AWS_E2E_BUCKET / AWS_E2E_REGION / AWS_E2E_PREFIX env vars"]
async fn aws_s3_single_put_roundtrip() {
    let Some((bucket, region, prefix)) = aws_e2e_config() else {
        return;
    };
    let direct = build_aws_client(&region).await;

    let (s4_endpoint, shutdown) = spawn_s4_server(&region).await;
    let s4 = build_s4_client(&s4_endpoint, &region);
    let k = key(&prefix, "single-put-roundtrip");

    let payload = Bytes::from(vec![b'x'; 10 * 1024]); // 10 KiB highly compressible
    s4.put_object()
        .bucket(&bucket)
        .key(&k)
        .body(payload.clone().into())
        .send()
        .await
        .expect("S4 put_object via real AWS");

    let resp = s4
        .get_object()
        .bucket(&bucket)
        .key(&k)
        .send()
        .await
        .expect("S4 get_object via real AWS");
    let got = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(got, payload, "roundtrip via real AWS S3 must match");

    cleanup(&direct, &bucket, &k).await;
    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "AWS E2E — requires env vars"]
async fn aws_s3_multipart_roundtrip_compresses_and_unframes() {
    let Some((bucket, region, prefix)) = aws_e2e_config() else {
        return;
    };
    let direct = build_aws_client(&region).await;

    let (s4_endpoint, shutdown) = spawn_s4_server(&region).await;
    let s4 = build_s4_client(&s4_endpoint, &region);
    let k = key(&prefix, "multipart-roundtrip");

    // 6 MiB × 2 part: both parts are >= 5 MiB so they qualify as non-final
    // for the v0.2 #5 padding-trim heuristic (i.e. they DO get padded to
    // the S3 multipart 5 MiB minimum even though the highly compressible
    // 'a' / 'b' bytes shrink to <100 bytes per part). That means the lower
    // bound on stored bytes is `2 × 5 MiB ≈ 10 MiB` regardless of how well
    // the codec compresses — and that is exactly the wire-format compromise
    // S3 multipart imposes. The assertion below honours that.
    let part_a = Bytes::from(vec![b'a'; 6 * 1024 * 1024]);
    let part_b = Bytes::from(vec![b'b'; 6 * 1024 * 1024]);
    let mut full = Vec::with_capacity(part_a.len() + part_b.len());
    full.extend_from_slice(&part_a);
    full.extend_from_slice(&part_b);

    let create = s4
        .create_multipart_upload()
        .bucket(&bucket)
        .key(&k)
        .send()
        .await
        .expect("create_multipart_upload");
    let upload_id = create.upload_id().expect("upload_id").to_string();

    let mut completed = Vec::new();
    for (i, body) in [&part_a, &part_b].iter().enumerate() {
        let part_number = (i + 1) as i32;
        let resp = s4
            .upload_part()
            .bucket(&bucket)
            .key(&k)
            .upload_id(&upload_id)
            .part_number(part_number)
            .body((**body).clone().into())
            .send()
            .await
            .expect("upload_part");
        completed.push(
            aws_sdk_s3::types::CompletedPart::builder()
                .e_tag(resp.e_tag().unwrap_or_default())
                .part_number(part_number)
                .build(),
        );
    }
    s4.complete_multipart_upload()
        .bucket(&bucket)
        .key(&k)
        .upload_id(&upload_id)
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .set_parts(Some(completed))
                .build(),
        )
        .send()
        .await
        .expect("complete_multipart_upload");

    // Roundtrip via S4
    let resp = s4
        .get_object()
        .bucket(&bucket)
        .key(&k)
        .send()
        .await
        .expect("S4 get_object");
    let got = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(got.as_ref(), full.as_slice());

    // Verify on the AWS side: the stored body should sit close to the S3
    // multipart minimum (2 parts × 5 MiB padded + frame headers + small slack)
    // — that's the best a compressed 2-part upload can do on AWS. It must NOT
    // grow beyond the original 12 MiB (i.e. at minimum the codec has done
    // *some* work; the 5 MiB-per-part floor is an S3 spec constraint, not an
    // S4 inefficiency).
    let raw = direct
        .get_object()
        .bucket(&bucket)
        .key(&k)
        .send()
        .await
        .expect("direct AWS get");
    let raw_bytes = raw.body.collect().await.expect("body").into_bytes();
    let min_floor = 2 * 5 * 1024 * 1024; // 2 padded non-final parts
    let max_acceptable = min_floor + 64 * 1024; // padding floor + a little slack
    assert!(
        raw_bytes.len() <= max_acceptable,
        "stored bytes ({}) exceeded the multipart-floor + slack ({}) — codec or padding regression?",
        raw_bytes.len(),
        max_acceptable
    );
    assert!(
        raw_bytes.len() < full.len(),
        "stored bytes ({}) should be smaller than the original ({})",
        raw_bytes.len(),
        full.len()
    );
    assert!(
        raw_bytes.windows(4).any(|w| w == b"S4F2"),
        "stored body should contain at least one S4F2 frame magic"
    );

    cleanup(&direct, &bucket, &k).await;
    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "AWS E2E — requires env vars"]
async fn aws_s3_range_get_via_sidecar() {
    let Some((bucket, region, prefix)) = aws_e2e_config() else {
        return;
    };
    let direct = build_aws_client(&region).await;

    let (s4_endpoint, shutdown) = spawn_s4_server(&region).await;
    let s4 = build_s4_client(&s4_endpoint, &region);
    let k = key(&prefix, "range-get");

    // 6 MiB so single-PUT framed produces multi-frame body + sidecar
    let payload: Bytes = (0u32..(6 * 1024 * 1024 / 4))
        .flat_map(|n| n.to_le_bytes())
        .collect::<Vec<u8>>()
        .into();
    s4.put_object()
        .bucket(&bucket)
        .key(&k)
        .body(payload.clone().into())
        .send()
        .await
        .expect("put_object");

    // Range request inside the first frame
    let resp = s4
        .get_object()
        .bucket(&bucket)
        .key(&k)
        .range("bytes=1500000-1500999")
        .send()
        .await
        .expect("range get");
    let got = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(got.len(), 1000);
    assert_eq!(got.as_ref(), &payload[1_500_000..1_501_000]);

    cleanup(&direct, &bucket, &k).await;
    let _ = shutdown.send(());
}
