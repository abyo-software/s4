//! `s4 migrate` E2E against a real MinIO container.
//!
//! Exercises the **library** (`s4_server::migrate::run_migrate`)
//! directly — the CLI wiring in `main.rs` is a thin print-formatter
//! over the same call. Docker required, so gated with `#[ignore]`
//! exactly like `estimate_minio.rs`:
//!
//! ```bash
//! cargo test --test migrate_minio -- --ignored --nocapture
//! ```
//!
//! Covered acceptance criteria:
//! (a) a compressible object shrinks after `--execute`, a GET routed
//!     through a real in-process S4 gateway returns the original
//!     bytes, and `verify_sidecar` reports `Ok` on the migrate-written
//!     sidecar;
//! (b) a re-run skips everything as `already-s4` (idempotent resume);
//! (c) the default dry-run writes nothing (ETags unchanged, no
//!     sidecar appears);
//! (d) an incompressible binary is left untouched (same ETag).

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
use s4_server::migrate::{DEFAULT_MIGRATE_CONCURRENCY, MigrateParams, run_migrate};
use s4_server::repair::{DEFAULT_REPAIR_BODY_BYTES_CAP, SidecarStatus, verify_sidecar};
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
/// `sidecar_repair_via_minio.rs`) so the test can prove migrate-written
/// objects decompress transparently on the production GET path.
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

fn default_params() -> MigrateParams {
    MigrateParams {
        prefix: None,
        execute: false,
        concurrency: DEFAULT_MIGRATE_CONCURRENCY,
        max_objects: None,
        max_body_bytes: DEFAULT_REPAIR_BODY_BYTES_CAP,
        default_codec: CodecKind::CpuZstd,
        zstd_level: CpuZstd::DEFAULT_LEVEL,
        use_sampling_dispatcher: true,
        gpu_min_bytes: SamplingDispatcher::DEFAULT_GPU_MIN_BYTES,
        prefer_columnar_gpu: false,
        gpu_present: false,
        no_tags: false,
    }
}

/// Pseudo-random bytes (xorshift) — high entropy, the dispatcher must
/// pick passthrough so migrate leaves the object untouched.
fn random_bytes(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
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

async fn head_size(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> u64 {
    u64::try_from(
        client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .expect("head")
            .content_length()
            .expect("content-length"),
    )
    .expect("non-negative size")
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

#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn migrate_dry_run_execute_idempotence_and_gateway_readback() {
    let fixture = start_minio().await;
    let backend = build_aws_client(&fixture.endpoint_url);
    let bucket = "s4-migrate-test";
    backend
        .create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create bucket");

    // Seed mix, written DIRECTLY to the backend (pre-gateway world):
    // - big/logs.log: 5 MiB repetitive text → multi-frame + sidecar
    // - small/readme.txt: 64 KiB text + user metadata + content-type →
    //   single frame, no sidecar
    // - noise.bin: 256 KiB xorshift bytes → passthrough pick, untouched
    // - empty.txt: zero bytes → untouched
    let big_key = "big/logs.log";
    let big_body = Bytes::from(
        b"level=info msg=\"request handled\" path=/api/v1/items status=200\n"
            .repeat(80_000)
            .to_vec(),
    );
    assert!(
        big_body.len() > 4 * 1024 * 1024,
        "must span multiple 4 MiB frames"
    );
    backend
        .put_object()
        .bucket(bucket)
        .key(big_key)
        .body(big_body.clone().into())
        .send()
        .await
        .expect("put big");

    let small_key = "small/readme.txt";
    let small_body = Bytes::from(b"plain text readme body line\n".repeat(2340).to_vec());
    backend
        .put_object()
        .bucket(bucket)
        .key(small_key)
        .body(small_body.clone().into())
        .content_type("text/plain")
        .metadata("owner", "alice")
        .send()
        .await
        .expect("put small");

    let noise_key = "noise.bin";
    let noise_body = Bytes::from(random_bytes(256 * 1024));
    backend
        .put_object()
        .bucket(bucket)
        .key(noise_key)
        .body(noise_body.clone().into())
        .send()
        .await
        .expect("put noise");

    let empty_key = "empty.txt";
    backend
        .put_object()
        .bucket(bucket)
        .key(empty_key)
        .body(Vec::new().into())
        .send()
        .await
        .expect("put empty");

    let etags_before: HashMap<&str, String> = {
        let mut m = HashMap::new();
        for k in [big_key, small_key, noise_key, empty_key] {
            m.insert(k, head_etag(&backend, bucket, k).await);
        }
        m
    };

    // ---- (c) dry-run (default mode) writes NOTHING ----
    let dry = run_migrate(&backend, bucket, &default_params())
        .await
        .expect("dry run");
    assert!(dry.dry_run);
    assert_eq!(dry.total_objects, 4);
    assert_eq!(dry.migrated, 2, "report: {dry:?}"); // big + small (would)
    assert_eq!(dry.skipped_not_compressible, 2); // noise + empty
    assert_eq!(dry.failed, 0);
    assert!(dry.migrated_bytes_after < dry.migrated_bytes_before);
    assert!(!dry.versioning_enabled);
    assert!(dry.notes.iter().any(|n| n.contains("dry-run")));
    for (k, etag) in &etags_before {
        assert_eq!(
            &head_etag(&backend, bucket, k).await,
            etag,
            "dry-run must not touch {k}"
        );
    }
    assert!(
        !key_exists(&backend, bucket, &sidecar_key(big_key)).await,
        "dry-run must not write a sidecar"
    );

    // ---- (a) execute: compressible objects shrink, sidecar appears ----
    let exec_params = MigrateParams {
        execute: true,
        ..default_params()
    };
    let report = run_migrate(&backend, bucket, &exec_params)
        .await
        .expect("execute run");
    assert!(!report.dry_run);
    assert_eq!(report.migrated, 2, "report: {report:?}");
    assert_eq!(report.skipped_not_compressible, 2);
    assert_eq!(report.failed, 0, "failures: {:?}", report.failures);
    assert_eq!(report.codecs.len(), 1);
    assert_eq!(report.codecs[0].picked, "cpu-zstd");
    assert_eq!(report.codecs[0].wrote_with, "cpu-zstd");
    assert_eq!(report.codecs[0].objects, 2);
    assert_eq!(
        report.migrated_bytes_before,
        (big_body.len() + small_body.len()) as u64
    );

    // Backend sizes shrank to exactly the reported after-bytes.
    let big_size = head_size(&backend, bucket, big_key).await;
    let small_size = head_size(&backend, bucket, small_key).await;
    assert!(big_size < big_body.len() as u64, "big must shrink");
    assert!(small_size < small_body.len() as u64, "small must shrink");
    assert_eq!(report.migrated_bytes_after, big_size + small_size);

    // (d) incompressible binary untouched (same ETag, same bytes).
    assert_eq!(
        head_etag(&backend, bucket, noise_key).await,
        etags_before[noise_key],
        "noise.bin must be untouched"
    );
    assert_eq!(
        head_etag(&backend, bucket, empty_key).await,
        etags_before[empty_key],
        "empty.txt must be untouched"
    );

    // Multi-frame object got a sidecar that verify-sidecar accepts;
    // the single-frame object correctly has none (gateway parity).
    let verify = verify_sidecar(&backend, bucket, big_key, DEFAULT_REPAIR_BODY_BYTES_CAP)
        .await
        .expect("verify sidecar");
    assert!(
        matches!(verify.status, SidecarStatus::Ok { .. }),
        "sidecar must verify Ok, got {:?}",
        verify.status
    );
    assert!(
        !key_exists(&backend, bucket, &sidecar_key(small_key)).await,
        "single-frame object must not get a sidecar"
    );

    // User metadata + content-type survived the rewrite.
    let small_head = backend
        .head_object()
        .bucket(bucket)
        .key(small_key)
        .send()
        .await
        .expect("head small");
    assert_eq!(small_head.content_type(), Some("text/plain"));
    let small_meta = small_head.metadata().expect("metadata");
    assert_eq!(small_meta.get("owner").map(String::as_str), Some("alice"));
    assert_eq!(
        small_meta.get("s4-framed").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        small_meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd")
    );

    // Gateway readback: GETs through a real S4 gateway return the
    // original plaintext for every key (migrated and untouched alike).
    let (s4_endpoint, shutdown) = spawn_s4_server(&fixture.endpoint_url).await;
    let via_gateway = build_aws_client(&s4_endpoint);
    assert_eq!(get_via(&via_gateway, bucket, big_key).await, big_body);
    assert_eq!(get_via(&via_gateway, bucket, small_key).await, small_body);
    assert_eq!(get_via(&via_gateway, bucket, noise_key).await, noise_body);
    // Range GET through the gateway exercises the migrate-written
    // sidecar's partial-fetch path on the multi-frame object.
    let ranged = via_gateway
        .get_object()
        .bucket(bucket)
        .key(big_key)
        .range("bytes=4194304-4194403")
        .send()
        .await
        .expect("range get")
        .body
        .collect()
        .await
        .expect("range body")
        .into_bytes();
    assert_eq!(ranged.as_ref(), &big_body[4_194_304..4_194_404]);
    let _ = shutdown.send(());

    // ---- (b) idempotence: a re-run skips everything, writes nothing ----
    let etag_after_big = head_etag(&backend, bucket, big_key).await;
    let etag_after_small = head_etag(&backend, bucket, small_key).await;
    let rerun = run_migrate(&backend, bucket, &exec_params)
        .await
        .expect("re-run");
    assert_eq!(rerun.migrated, 0, "report: {rerun:?}");
    assert_eq!(rerun.skipped_already_s4, 2);
    assert_eq!(rerun.skipped_not_compressible, 2);
    assert_eq!(rerun.failed, 0);
    assert_eq!(head_etag(&backend, bucket, big_key).await, etag_after_big);
    assert_eq!(
        head_etag(&backend, bucket, small_key).await,
        etag_after_small
    );
}

/// Regression coverage for the v1.0 audit round-1 findings:
/// (1) `.s4dict/<id>` dictionary objects (train-dict output) must be
///     byte-identical after `migrate --execute` — re-compressing one
///     would break every `cpu-zstd-dict` GET that references it;
/// (2) `.__s4ver__/` versioning shadow keys must be excluded from the
///     listing (rewriting one would break version restore);
/// (3) object tags and a non-STANDARD storage class must survive the
///     rewrite PUT (MinIO supports REDUCED_REDUNDANCY + tagging; ACL /
///     Object Lock retention carry-over is NOT covered — MinIO needs
///     lock-enabled buckets at creation, and the tools document the
///     non-carry-over explicitly instead).
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn migrate_excludes_internal_keys_and_carries_tags_and_storage_class() {
    let fixture = start_minio().await;
    let backend = build_aws_client(&fixture.endpoint_url);
    let bucket = "s4-migrate-internal";
    backend
        .create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create bucket");

    // Highly compressible bytes everywhere: if migrate listed any of
    // the internal keys it WOULD rewrite them, so "unchanged bytes" is
    // a real assertion, not vacuous.
    let compressible = Bytes::from(
        b"dictionary sample line: user=alice op=put\n"
            .repeat(2_000)
            .to_vec(),
    );

    // (1) A train-dict-shaped dictionary object at the bucket root.
    let dict_key = ".s4dict/0123456789abcdef";
    backend
        .put_object()
        .bucket(bucket)
        .key(dict_key)
        .body(compressible.clone().into())
        .send()
        .await
        .expect("put dict");

    // (2) A versioning shadow key, as `service::versioned_shadow_key`
    // lays them out (`<key>.__s4ver__/<version-id>`).
    let shadow_key = "data/file.log.__s4ver__/9c1f8c4e-0001";
    backend
        .put_object()
        .bucket(bucket)
        .key(shadow_key)
        .body(compressible.clone().into())
        .send()
        .await
        .expect("put shadow");

    // (3) A normal object carrying tags + REDUCED_REDUNDANCY.
    let tagged_key = "data/tagged.log";
    backend
        .put_object()
        .bucket(bucket)
        .key(tagged_key)
        .body(compressible.clone().into())
        // Tag value with a space exercises the URL-encoding carry-over.
        .tagging("env=prod&team=s4%20core")
        .storage_class(aws_sdk_s3::types::StorageClass::ReducedRedundancy)
        .send()
        .await
        .expect("put tagged");

    let dict_etag = head_etag(&backend, bucket, dict_key).await;
    let shadow_etag = head_etag(&backend, bucket, shadow_key).await;

    let report = run_migrate(
        &backend,
        bucket,
        &MigrateParams {
            execute: true,
            ..default_params()
        },
    )
    .await
    .expect("execute run");

    // Only the customer object was listed; the internal keys were
    // never even examined.
    assert_eq!(report.total_objects, 1, "report: {report:?}");
    assert_eq!(report.migrated, 1);
    assert_eq!(report.failed, 0, "failures: {:?}", report.failures);

    // (1) Dictionary bytes untouched, byte-for-byte.
    assert_eq!(head_etag(&backend, bucket, dict_key).await, dict_etag);
    assert_eq!(
        get_via(&backend, bucket, dict_key).await,
        compressible,
        ".s4dict/<id> must be byte-identical after migrate --execute"
    );
    // (2) Shadow key untouched.
    assert_eq!(head_etag(&backend, bucket, shadow_key).await, shadow_etag);
    assert_eq!(get_via(&backend, bucket, shadow_key).await, compressible);

    // (3) Tagged object rewritten (shrunk) with tags + storage class.
    assert!(head_size(&backend, bucket, tagged_key).await < compressible.len() as u64);
    let mut tags: Vec<(String, String)> = backend
        .get_object_tagging()
        .bucket(bucket)
        .key(tagged_key)
        .send()
        .await
        .expect("get tagging")
        .tag_set()
        .iter()
        .map(|t| (t.key().to_owned(), t.value().to_owned()))
        .collect();
    tags.sort();
    assert_eq!(
        tags,
        vec![
            ("env".to_owned(), "prod".to_owned()),
            ("team".to_owned(), "s4 core".to_owned()),
        ],
        "object tags must survive the rewrite"
    );
    let storage_class = backend
        .head_object()
        .bucket(bucket)
        .key(tagged_key)
        .send()
        .await
        .expect("head tagged")
        .storage_class()
        .map(|sc| sc.as_str().to_owned());
    assert_eq!(
        storage_class.as_deref(),
        Some("REDUCED_REDUNDANCY"),
        "storage class must survive the rewrite"
    );

    // The report tells the operator what is NOT carried over.
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("ACLs and Object Lock")),
        "ACL / Object Lock non-carry-over note expected: {:?}",
        report.notes
    );
}

/// Versioning-enabled bucket: migrate still works, the report carries
/// the double-billing warning, and prefix scoping + max-objects
/// truncation behave like `estimate`.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn migrate_versioning_warning_and_scoping() {
    let fixture = start_minio().await;
    let backend = build_aws_client(&fixture.endpoint_url);
    let bucket = "s4-migrate-versioned";
    backend
        .create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create bucket");
    backend
        .put_bucket_versioning()
        .bucket(bucket)
        .versioning_configuration(
            aws_sdk_s3::types::VersioningConfiguration::builder()
                .status(aws_sdk_s3::types::BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await
        .expect("enable versioning");

    let body = Bytes::from(
        b"versioned log line repeated for compression\n"
            .repeat(4_000)
            .to_vec(),
    );
    for key in ["in/a.log", "in/b.log", "out/c.log"] {
        backend
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body.clone().into())
            .send()
            .await
            .expect("put");
    }
    // A fake sidecar key that the listing must exclude.
    backend
        .put_object()
        .bucket(bucket)
        .key("in/a.log.s4index")
        .body(b"not a real sidecar".to_vec().into())
        .send()
        .await
        .expect("put fake sidecar");

    // Prefix-scoped execute with a 1-object cap: only one key is
    // examined, the truncation is flagged, the versioning warning rides
    // along even on this partial run.
    let params = MigrateParams {
        prefix: Some("in/".into()),
        execute: true,
        max_objects: Some(1),
        ..default_params()
    };
    let report = run_migrate(&backend, bucket, &params)
        .await
        .expect("scoped run");
    assert_eq!(report.total_objects, 1);
    assert!(report.listing_truncated);
    assert_eq!(report.migrated, 1);
    assert_eq!(report.failed, 0, "failures: {:?}", report.failures);
    assert!(report.versioning_enabled);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("versioning is Enabled")),
        "notes: {:?}",
        report.notes
    );
    assert!(report.notes.iter().any(|n| n.contains("truncated")));

    // The old (uncompressed) version is still listed — that's the
    // double-billing the warning is about.
    let versions = backend
        .list_object_versions()
        .bucket(bucket)
        .prefix("in/a.log")
        .send()
        .await
        .expect("list versions");
    let a_log_versions = versions
        .versions()
        .iter()
        .filter(|v| v.key() == Some("in/a.log"))
        .count();
    assert!(
        a_log_versions >= 2,
        "expected old + migrated versions, got {a_log_versions}"
    );

    // Out-of-prefix key untouched.
    let out_head = backend
        .head_object()
        .bucket(bucket)
        .key("out/c.log")
        .send()
        .await
        .expect("head out/c.log");
    assert!(
        out_head
            .metadata()
            .map(|m| !m.contains_key("s4-codec"))
            .unwrap_or(true),
        "out-of-prefix key must not be migrated"
    );
}
