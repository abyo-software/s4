//! `s4 maintain` E2E against a real MinIO container.
//!
//! The policy-engine test exercises the **library**
//! (`s4_server::maintain::run_maintain`) directly — the CLI wiring in
//! `main.rs` is a thin print-formatter over the same call; the
//! `--interval` resident-mode test drives the real `s4` binary as a
//! child process (signal handling cannot be tested in-process).
//! Docker required, so gated with `#[ignore]` exactly like
//! `migrate_minio.rs` / `recompact_minio.rs`:
//!
//! ```bash
//! cargo test --test maintain_minio -- --ignored --nocapture
//! ```
//!
//! Covered acceptance criteria:
//! (a) a 3-rule policy (migrate + recompact + transition) parses and
//!     runs in file order;
//! (b) the default dry-run changes nothing (ETags, storage classes and
//!     sidecar absence all verified) while reporting measured
//!     would-do counts;
//! (c) `--execute` applies every rule: plain objects get framed (with
//!     a sidecar on the multi-frame one), the recompact target gets
//!     the `s4-zstd-level` stamp and shrinks, and the transition rule
//!     moves the mains to REDUCED_REDUNDANCY **with the `.s4index`
//!     sidecar accompanying its main into the same class**;
//! (d) a second `--execute` run is idempotent: every rule skips
//!     everything (already-s4 / already-compacted /
//!     already-target-class) and no sidecar moves again;
//! (e) framed-then-transitioned objects still decompress byte-for-byte
//!     through a real in-process S4 gateway;
//! (f) `--interval 1s` resident mode (child `s4` process) completes at
//!     least two cycles and exits 0 on SIGTERM.

use std::collections::HashMap;
use std::sync::Arc;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use bytes::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use s3s::auth::SimpleAuth;
use s3s::service::S3ServiceBuilder;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::{AlwaysDispatcher, SamplingDispatcher};
use s4_codec::index::sidecar_key;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::maintain::{MaintainParams, RuleOutcome, parse_policy, run_maintain};
use s4_server::migrate::{DEFAULT_MIGRATE_CONCURRENCY, MigrateParams, run_migrate};
use s4_server::repair::DEFAULT_REPAIR_BODY_BYTES_CAP;
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

/// Spawn a real S4 gateway in front of the MinIO backend (same shape as
/// `migrate_minio.rs` / `recompact_minio.rs`) so the test can prove
/// maintained objects decompress transparently on the production GET
/// path.
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

/// Varied (non-repetitive) log text — same corpus generator as the
/// migrate / recompact e2e suites: zstd-19 must beat zstd-3 by a clear
/// margin on this shape, so the recompact rule has real work.
fn varied_log_text(lines: u64) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut text = String::new();
    for i in 0..lines {
        let _ = writeln!(
            text,
            "level=info req={i:08} user=u{} path=/api/v1/items/{} status={} latency_ms={}",
            i % 997,
            (i * 7) % 10_000,
            if i % 17 == 0 { 404 } else { 200 },
            i % 250,
        );
    }
    text.into_bytes()
}

async fn head_etag(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> String {
    client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("head")
        .e_tag()
        .expect("etag")
        .to_owned()
}

async fn head_meta(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> HashMap<String, String> {
    client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("head")
        .metadata()
        .cloned()
        .unwrap_or_default()
}

/// Storage class with the absent form normalized: `HeadObject` omits
/// the header for STANDARD objects on some backends.
async fn head_storage_class(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> String {
    client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("head")
        .storage_class()
        .map(|sc| sc.as_str().to_owned())
        .unwrap_or_else(|| "STANDARD".to_owned())
}

async fn key_exists(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .is_ok()
}

async fn get_via(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Bytes {
    client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("get")
        .body
        .collect()
        .await
        .expect("collect body")
        .into_bytes()
}

async fn put_plain(client: &aws_sdk_s3::Client, bucket: &str, key: &str, body: Bytes) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body.into())
        .send()
        .await
        .expect("put");
}

const BUCKET: &str = "s4-maintain-test";

/// The 3-rule policy under test: frame `plain/`, bake `compact/` at 19,
/// then push `plain/` (mains + sidecars) to REDUCED_REDUNDANCY — the
/// only non-STANDARD storage class MinIO accepts.
const POLICY: &str = r#"
[[rule]]
name = "frame-plain"
bucket = "s4-maintain-test"
prefix = "plain/"
action = "migrate"

[[rule]]
name = "bake-compact"
bucket = "s4-maintain-test"
prefix = "compact/"
action = "recompact"
target-zstd-level = 19
min-gain-percent = 1.0

[[rule]]
name = "cool-plain"
bucket = "s4-maintain-test"
prefix = "plain/"
action = "transition"
storage-class = "REDUCED_REDUNDANCY"
"#;

fn maintain_params(execute: bool) -> MaintainParams {
    MaintainParams {
        execute,
        default_codec: CodecKind::CpuZstd,
        zstd_level: 3,
        use_sampling_dispatcher: true,
        gpu_min_bytes: SamplingDispatcher::DEFAULT_GPU_MIN_BYTES,
        prefer_columnar_gpu: false,
        gpu_present: false,
    }
}

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn maintain_dry_run_execute_idempotence_and_gateway_readback() {
    let fixture = start_minio().await;
    let backend = build_aws_client(&fixture.endpoint_url);
    backend
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("create bucket");

    // Seed:
    // - plain/big.log + plain/small.txt: plain text, targets of the
    //   migrate rule; big spans multiple 4 MiB frames (→ sidecar).
    // - compact/cold.log: framed at zstd-3 via `run_migrate` up front,
    //   the population the recompact rule exists for.
    let big_key = "plain/big.log";
    let big_body = Bytes::from(varied_log_text(60_000));
    assert!(
        big_body.len() > 4 * 1024 * 1024,
        "must span multiple 4 MiB frames"
    );
    put_plain(&backend, BUCKET, big_key, big_body.clone()).await;

    let small_key = "plain/small.txt";
    let small_body = Bytes::from(varied_log_text(900));
    assert!(small_body.len() < 1024 * 1024, "single-frame body");
    put_plain(&backend, BUCKET, small_key, small_body.clone()).await;

    let cold_key = "compact/cold.log";
    let cold_body = Bytes::from(varied_log_text(60_000));
    put_plain(&backend, BUCKET, cold_key, cold_body.clone()).await;
    let seed = run_migrate(
        &backend,
        BUCKET,
        &MigrateParams {
            prefix: Some("compact/".to_owned()),
            execute: true,
            concurrency: DEFAULT_MIGRATE_CONCURRENCY,
            max_objects: None,
            max_body_bytes: DEFAULT_REPAIR_BODY_BYTES_CAP,
            default_codec: CodecKind::CpuZstd,
            zstd_level: 3,
            use_sampling_dispatcher: true,
            gpu_min_bytes: SamplingDispatcher::DEFAULT_GPU_MIN_BYTES,
            prefer_columnar_gpu: false,
            gpu_present: false,
            no_tags: false,
        },
    )
    .await
    .expect("seed migrate");
    assert_eq!(seed.migrated, 1, "seed report: {seed:?}");

    let policy = parse_policy(POLICY).expect("valid policy");
    assert_eq!(policy.rules.len(), 3);

    let etags_before: HashMap<&str, String> = {
        let mut m = HashMap::new();
        for k in [big_key, small_key, cold_key] {
            m.insert(k, head_etag(&backend, BUCKET, k).await);
        }
        m
    };

    // ---- (b) dry-run: measured counts, zero writes ----
    let dry = run_maintain(&backend, &policy, &maintain_params(false), None).await;
    assert!(dry.dry_run);
    assert_eq!(dry.rules_total, 3);
    assert_eq!(dry.rules_run, 3);
    assert_eq!(dry.rules_failed, 0, "dry-run report: {dry:?}");
    assert!(!dry.interrupted);
    // Execution order == policy file order.
    let names: Vec<&str> = dry.rules.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["frame-plain", "bake-compact", "cool-plain"]);

    match &dry.rules[0].outcome {
        RuleOutcome::Migrate {
            report,
            skipped_too_recent,
        } => {
            assert!(report.dry_run);
            assert_eq!(report.migrated, 2, "would migrate both plain objects");
            assert_eq!(*skipped_too_recent, 0);
        }
        other => panic!("rule 0 must be migrate, got {other:?}"),
    }
    match &dry.rules[1].outcome {
        RuleOutcome::Recompact { report } => {
            assert!(report.dry_run);
            assert_eq!(report.recompacted, 1, "would recompact the seeded object");
        }
        other => panic!("rule 1 must be recompact, got {other:?}"),
    }
    match &dry.rules[2].outcome {
        RuleOutcome::Transition { report } => {
            assert!(report.dry_run);
            assert_eq!(report.storage_class, "REDUCED_REDUNDANCY");
            assert_eq!(report.transitioned, 2, "both plain mains would move");
            // Honest dry-run limitation: the sidecar the migrate rule
            // *would* create does not exist yet, so it cannot be counted.
            assert_eq!(report.transitioned_sidecars, 0);
        }
        other => panic!("rule 2 must be transition, got {other:?}"),
    }

    // Nothing changed: ETags, storage classes, no sidecar.
    for k in [big_key, small_key, cold_key] {
        assert_eq!(
            head_etag(&backend, BUCKET, k).await,
            etags_before[k],
            "dry-run must not rewrite {k}"
        );
        assert_eq!(
            head_storage_class(&backend, BUCKET, k).await,
            "STANDARD",
            "dry-run must not transition {k}"
        );
    }
    assert!(
        !key_exists(&backend, BUCKET, &sidecar_key(big_key)).await,
        "dry-run must not write a sidecar"
    );

    // ---- (c) execute: every rule applies ----
    let exec = run_maintain(&backend, &policy, &maintain_params(true), None).await;
    assert!(!exec.dry_run);
    assert_eq!(exec.rules_run, 3);
    assert_eq!(exec.rules_failed, 0, "execute report: {exec:?}");

    match &exec.rules[0].outcome {
        RuleOutcome::Migrate { report, .. } => {
            assert_eq!(report.migrated, 2, "migrate rule: {report:?}");
            assert_eq!(report.failed, 0, "failures: {:?}", report.failures);
        }
        other => panic!("rule 0 must be migrate, got {other:?}"),
    }
    match &exec.rules[1].outcome {
        RuleOutcome::Recompact { report } => {
            assert_eq!(report.recompacted, 1, "recompact rule: {report:?}");
            assert!(
                report.recompacted_bytes_after < report.recompacted_bytes_before,
                "level 19 must shrink the level-3 frames: {report:?}"
            );
        }
        other => panic!("rule 1 must be recompact, got {other:?}"),
    }
    match &exec.rules[2].outcome {
        RuleOutcome::Transition { report } => {
            assert_eq!(report.transitioned, 2, "transition rule: {report:?}");
            assert_eq!(
                report.transitioned_sidecars, 1,
                "the multi-frame object's sidecar must accompany it: {report:?}"
            );
            assert_eq!(report.failed, 0, "failures: {:?}", report.failures);
        }
        other => panic!("rule 2 must be transition, got {other:?}"),
    }

    // Migrate rule landed: framed metadata + sidecar on the big object.
    let big_meta = head_meta(&backend, BUCKET, big_key).await;
    assert_eq!(
        big_meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd")
    );
    let big_sidecar = sidecar_key(big_key);
    assert!(
        key_exists(&backend, BUCKET, &big_sidecar).await,
        "multi-frame object must have a sidecar"
    );
    assert!(
        !key_exists(&backend, BUCKET, &sidecar_key(small_key)).await,
        "single-frame object must not have a sidecar"
    );
    // Recompact rule landed: the idempotency stamp.
    let cold_meta = head_meta(&backend, BUCKET, cold_key).await;
    assert_eq!(
        cold_meta.get("s4-zstd-level").map(String::as_str),
        Some("19"),
        "recompact must stamp the level: {cold_meta:?}"
    );
    // Transition rule landed: mains AND the sidecar share the class.
    for k in [big_key, small_key] {
        assert_eq!(
            head_storage_class(&backend, BUCKET, k).await,
            "REDUCED_REDUNDANCY",
            "{k} must be transitioned"
        );
    }
    assert_eq!(
        head_storage_class(&backend, BUCKET, &big_sidecar).await,
        "REDUCED_REDUNDANCY",
        "sidecar must accompany its main object into the target class"
    );
    // The transition copy must not have stripped the migrate stamps.
    let big_meta_after = head_meta(&backend, BUCKET, big_key).await;
    assert_eq!(
        big_meta_after.get("s4-codec").map(String::as_str),
        Some("cpu-zstd"),
        "transition copy must preserve the s4-* metadata: {big_meta_after:?}"
    );
    // The recompact target was NOT in the transition prefix.
    assert_eq!(
        head_storage_class(&backend, BUCKET, cold_key).await,
        "STANDARD"
    );

    // ---- (e) gateway readback: framed + transitioned objects still
    // decompress byte-for-byte on the production GET path ----
    let (gateway_url, gateway_shutdown) = spawn_s4_server(&fixture.endpoint_url).await;
    let via_gateway = build_aws_client(&gateway_url);
    assert_eq!(
        get_via(&via_gateway, BUCKET, big_key).await,
        big_body,
        "gateway GET must return the original bytes"
    );
    assert_eq!(get_via(&via_gateway, BUCKET, small_key).await, small_body);
    assert_eq!(get_via(&via_gateway, BUCKET, cold_key).await, cold_body);
    let _ = gateway_shutdown.send(());

    // ---- (d) re-run: every rule skips everything (idempotence) ----
    let rerun = run_maintain(&backend, &policy, &maintain_params(true), None).await;
    assert_eq!(rerun.rules_failed, 0, "re-run report: {rerun:?}");
    match &rerun.rules[0].outcome {
        RuleOutcome::Migrate { report, .. } => {
            assert_eq!(report.migrated, 0);
            assert_eq!(report.skipped_already_s4, 2, "re-run migrate: {report:?}");
        }
        other => panic!("rule 0 must be migrate, got {other:?}"),
    }
    match &rerun.rules[1].outcome {
        RuleOutcome::Recompact { report } => {
            assert_eq!(report.recompacted, 0);
            assert_eq!(
                report.skipped_already_compacted, 1,
                "re-run recompact: {report:?}"
            );
        }
        other => panic!("rule 1 must be recompact, got {other:?}"),
    }
    match &rerun.rules[2].outcome {
        RuleOutcome::Transition { report } => {
            assert_eq!(report.transitioned, 0);
            assert_eq!(report.transitioned_sidecars, 0, "no sidecar realign needed");
            assert_eq!(
                report.skipped_already_target_class, 2,
                "re-run transition: {report:?}"
            );
        }
        other => panic!("rule 2 must be transition, got {other:?}"),
    }
}

/// (f) `--interval` resident mode: the real `s4` binary cycles on the
/// interval and exits 0 on SIGTERM. Child process because signal
/// delivery and the binary's tracing setup cannot be exercised
/// in-process.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn maintain_interval_resident_mode_sigterm() {
    let fixture = start_minio().await;
    let backend = build_aws_client(&fixture.endpoint_url);
    let bucket = "s4-maintain-resident";
    backend
        .create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create bucket");
    put_plain(
        &backend,
        bucket,
        "logs/a.txt",
        Bytes::from(varied_log_text(200)),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let policy_path = dir.path().join("s4-maintain.toml");
    std::fs::write(
        &policy_path,
        format!(
            r#"
[[rule]]
name = "cool-logs"
bucket = "{bucket}"
prefix = "logs/"
action = "transition"
storage-class = "REDUCED_REDUNDANCY"
"#
        ),
    )
    .expect("write policy");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_s4"))
        .arg("--endpoint-url")
        .arg(&fixture.endpoint_url)
        .arg("maintain")
        .arg("--policy")
        .arg(&policy_path)
        .arg("--execute")
        .arg("--interval")
        .arg("1s")
        .env("AWS_ACCESS_KEY_ID", MINIO_USER)
        .env("AWS_SECRET_ACCESS_KEY", MINIO_PASS)
        .env("AWS_REGION", "us-east-1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn s4 maintain --interval");

    // Let it complete ~2-3 cycles (1s interval), then SIGTERM.
    tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "resident process must still be running before SIGTERM"
    );
    let kill = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "kill -TERM must succeed");

    // Graceful exit within a generous bound.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("resident maintain did not exit within 15s of SIGTERM");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(
        status.success(),
        "SIGTERM must produce a graceful zero exit, got {status:?}"
    );

    // The structured per-cycle logs must show >= 2 completed cycles.
    let out = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let cycles = stdout.matches("maintain cycle complete").count();
    assert!(
        cycles >= 2,
        "expected >= 2 completed cycles, got {cycles}; stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The transition rule really ran while resident.
    assert_eq!(
        backend
            .head_object()
            .bucket(bucket)
            .key("logs/a.txt")
            .send()
            .await
            .expect("head")
            .storage_class()
            .map(|sc| sc.as_str().to_owned())
            .unwrap_or_else(|| "STANDARD".to_owned()),
        "REDUCED_REDUNDANCY"
    );
}
