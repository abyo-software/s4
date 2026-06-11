//! `s4 recompact` E2E against a real MinIO container.
//!
//! Exercises the **library** (`s4_server::recompact::run_recompact`)
//! directly — the CLI wiring in `main.rs` is a thin print-formatter
//! over the same call. Docker required, so gated with `#[ignore]`
//! exactly like `migrate_minio.rs`:
//!
//! ```bash
//! cargo test --test recompact_minio -- --ignored --nocapture
//! ```
//!
//! Covered acceptance criteria:
//! (a) objects migrated at zstd-3 shrink after recompact `--execute`,
//!     a GET routed through a real in-process S4 gateway returns the
//!     original bytes, `verify_sidecar` reports `Ok` on the rewritten
//!     sidecar, and the `s4-zstd-level` stamp lands;
//! (b) a re-run skips the rewritten objects as `already-compacted`
//!     (idempotent resume, ETags unchanged);
//! (c) the default dry-run writes nothing (ETags unchanged, no stamp);
//! (d) `--older-than 30d` skips the just-written objects (`too-recent`);
//! (e) a gateway-shape passthrough object skips as `unsupported-codec`,
//!     a plain (never-migrated) object skips as `not-s4`, and a framed
//!     body without the gateway metadata stamp skips as
//!     `unstamped-framed` (untouched, hint note present);
//! (f) object tags and a non-STANDARD storage class survive both the
//!     migrate seed and the recompact rewrite (MinIO supports
//!     REDUCED_REDUNDANCY and object tagging; ACL / Object Lock
//!     retention carry-over is NOT covered — MinIO needs lock-enabled
//!     buckets at creation and the tools document the non-carry-over).

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
use s4_server::recompact::{
    DEFAULT_MIN_GAIN_PERCENT, DEFAULT_RECOMPACT_CONCURRENCY, DEFAULT_TARGET_ZSTD_LEVEL,
    RecompactParams, run_recompact,
};
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
/// `migrate_minio.rs`) so the test can prove recompacted objects
/// decompress transparently on the production GET path.
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

/// Migrate seed params: frame at zstd-3, the gateway's latency-first
/// default — exactly the population recompact exists for.
fn migrate_params_level3(prefix: &str) -> MigrateParams {
    MigrateParams {
        prefix: Some(prefix.to_owned()),
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
    }
}

fn default_recompact_params() -> RecompactParams {
    RecompactParams {
        prefix: None,
        execute: false,
        concurrency: DEFAULT_RECOMPACT_CONCURRENCY,
        max_objects: None,
        max_body_bytes: DEFAULT_REPAIR_BODY_BYTES_CAP,
        target_zstd_level: DEFAULT_TARGET_ZSTD_LEVEL,
        min_gain_percent: DEFAULT_MIN_GAIN_PERCENT,
        older_than: None,
        assume_unstamped_framed: false,
    }
}

/// Varied (non-repetitive) log text — level 19 must beat level 3 by a
/// clear margin on this shape (measured ~12% chunked, see the
/// `decode_then_recompress_pipeline` lib test).
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

/// Pseudo-random bytes (xorshift) — high entropy, gateway-passthrough
/// shape.
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

async fn get_tags(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Vec<(String, String)> {
    let mut tags: Vec<(String, String)> = client
        .get_object_tagging()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("get tagging")
        .tag_set()
        .iter()
        .map(|t| (t.key().to_owned(), t.value().to_owned()))
        .collect();
    tags.sort();
    tags
}

async fn head_storage_class(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> Option<String> {
    client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("head")
        .storage_class()
        .map(|sc| sc.as_str().to_owned())
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
async fn recompact_dry_run_execute_idempotence_and_gateway_readback() {
    let fixture = start_minio().await;
    let backend = build_aws_client(&fixture.endpoint_url);
    let bucket = "s4-recompact-test";
    backend
        .create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create bucket");

    // Seed mix:
    // - in/big.log + in/small.txt: plain text PUT directly, then framed
    //   at zstd-3 by `s4 migrate` (the population recompact targets);
    //   big spans multiple 4 MiB frames (→ sidecar), small is a single
    //   frame with user metadata + content-type to carry over.
    // - plain.txt: never migrated → recompact must skip `not-s4`.
    // - pt.bin: raw random bytes + the `s4-codec: passthrough` stamp a
    //   gateway passthrough PUT leaves → skip `unsupported-codec`.
    // - fake.s4f2: S4F2-prefixed bytes with NO s4-codec metadata stamp
    //   (backend-written) → skip `unstamped-framed` by default.
    // small.txt also carries object tags + a REDUCED_REDUNDANCY storage
    // class so the test proves both survive the migrate seed AND the
    // recompact rewrite.
    let big_key = "in/big.log";
    let big_body = Bytes::from(varied_log_text(60_000));
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

    let small_key = "in/small.txt";
    let small_body = Bytes::from(varied_log_text(900));
    assert!(small_body.len() < 1024 * 1024, "single-frame body");
    backend
        .put_object()
        .bucket(bucket)
        .key(small_key)
        .body(small_body.clone().into())
        .content_type("text/plain")
        .metadata("owner", "alice")
        // Tag value with a space exercises the URL-encoding carry-over.
        .tagging("env=prod&team=s4%20core")
        .storage_class(aws_sdk_s3::types::StorageClass::ReducedRedundancy)
        .send()
        .await
        .expect("put small");

    let plain_key = "plain.txt";
    let plain_body = Bytes::from(b"never migrated plain text body\n".repeat(100).to_vec());
    backend
        .put_object()
        .bucket(bucket)
        .key(plain_key)
        .body(plain_body.clone().into())
        .send()
        .await
        .expect("put plain");

    let pt_key = "pt.bin";
    let pt_body = Bytes::from(random_bytes(64 * 1024));
    backend
        .put_object()
        .bucket(bucket)
        .key(pt_key)
        .body(pt_body.clone().into())
        .metadata("s4-codec", "passthrough")
        .send()
        .await
        .expect("put passthrough-shape");

    let unstamped_key = "fake.s4f2";
    let mut unstamped_body = b"S4F2".to_vec();
    unstamped_body.extend_from_slice(&random_bytes(8 * 1024));
    let unstamped_body = Bytes::from(unstamped_body);
    backend
        .put_object()
        .bucket(bucket)
        .key(unstamped_key)
        .body(unstamped_body.clone().into())
        .send()
        .await
        .expect("put unstamped-framed-shape");

    // Frame in/ at zstd-3 (the gateway's latency-first write level).
    let mig = run_migrate(&backend, bucket, &migrate_params_level3("in/"))
        .await
        .expect("seed migrate");
    assert_eq!(mig.migrated, 2, "seed migrate report: {mig:?}");
    assert_eq!(mig.failed, 0, "failures: {:?}", mig.failures);

    let big_l3_size = head_size(&backend, bucket, big_key).await;
    let small_l3_size = head_size(&backend, bucket, small_key).await;
    let etags_after_migrate: HashMap<&str, String> = {
        let mut m = HashMap::new();
        for k in [big_key, small_key, plain_key, pt_key, unstamped_key] {
            m.insert(k, head_etag(&backend, bucket, k).await);
        }
        m
    };

    // The migrate seed must already have carried small.txt's tags and
    // storage class through its rewrite.
    let small_tags_after_migrate = get_tags(&backend, bucket, small_key).await;
    assert_eq!(
        small_tags_after_migrate,
        vec![
            ("env".to_owned(), "prod".to_owned()),
            ("team".to_owned(), "s4 core".to_owned()),
        ],
        "migrate must carry object tags over"
    );
    assert_eq!(
        head_storage_class(&backend, bucket, small_key)
            .await
            .as_deref(),
        Some("REDUCED_REDUNDANCY"),
        "migrate must carry the storage class over"
    );

    // ---- (c) dry-run (default mode) writes NOTHING ----
    let dry = run_recompact(&backend, bucket, &default_recompact_params())
        .await
        .expect("dry run");
    assert!(dry.dry_run);
    assert_eq!(dry.total_objects, 5, "report: {dry:?}");
    assert_eq!(dry.recompacted, 2, "report: {dry:?}"); // big + small (would)
    assert_eq!(dry.skipped_not_s4, 1); // plain.txt
    assert_eq!(dry.skipped_unsupported_codec, 1); // pt.bin
    assert_eq!(dry.skipped_unstamped_framed, 1); // fake.s4f2
    assert!(
        dry.notes
            .iter()
            .any(|n| n.contains("--assume-unstamped-framed")),
        "unstamped-framed hint note expected: {:?}",
        dry.notes
    );
    assert_eq!(dry.failed, 0, "failures: {:?}", dry.failures);
    assert!(dry.recompacted_bytes_after < dry.recompacted_bytes_before);
    assert_eq!(dry.recompacted_bytes_before, big_l3_size + small_l3_size);
    assert!(dry.notes.iter().any(|n| n.contains("dry-run")));
    assert!(
        dry.notes.iter().any(|n| n.contains("s4 migrate")),
        "not-s4 hint note expected: {:?}",
        dry.notes
    );
    for (k, etag) in &etags_after_migrate {
        assert_eq!(
            &head_etag(&backend, bucket, k).await,
            etag,
            "dry-run must not touch {k}"
        );
    }
    assert!(
        !head_meta(&backend, bucket, big_key)
            .await
            .contains_key("s4-zstd-level"),
        "dry-run must not stamp"
    );

    // ---- (d) --older-than 30d: everything just written is too recent ----
    let aged = run_recompact(
        &backend,
        bucket,
        &RecompactParams {
            older_than: Some(std::time::Duration::from_secs(30 * 86_400)),
            execute: true,
            ..default_recompact_params()
        },
    )
    .await
    .expect("older-than run");
    assert_eq!(aged.recompacted, 0, "report: {aged:?}");
    assert_eq!(aged.skipped_too_recent, 5);
    assert_eq!(aged.failed, 0);
    for (k, etag) in &etags_after_migrate {
        assert_eq!(
            &head_etag(&backend, bucket, k).await,
            etag,
            "--older-than must not touch {k}"
        );
    }

    // ---- (a) execute: zstd-3 objects shrink, stamp + sidecar land ----
    let exec_params = RecompactParams {
        execute: true,
        ..default_recompact_params()
    };
    let report = run_recompact(&backend, bucket, &exec_params)
        .await
        .expect("execute run");
    println!("{}", s4_server::recompact::render_human(&report));
    assert!(!report.dry_run);
    assert_eq!(report.recompacted, 2, "report: {report:?}");
    assert_eq!(report.skipped_not_s4, 1);
    assert_eq!(report.skipped_unsupported_codec, 1);
    assert_eq!(report.skipped_unstamped_framed, 1);
    assert_eq!(report.failed, 0, "failures: {:?}", report.failures);
    assert_eq!(report.recompacted_bytes_before, big_l3_size + small_l3_size);

    // Backend sizes shrank to exactly the reported after-bytes.
    let big_l19_size = head_size(&backend, bucket, big_key).await;
    let small_l19_size = head_size(&backend, bucket, small_key).await;
    assert!(
        big_l19_size < big_l3_size,
        "big must shrink: {big_l3_size} -> {big_l19_size}"
    );
    assert!(
        small_l19_size < small_l3_size,
        "small must shrink: {small_l3_size} -> {small_l19_size}"
    );
    assert_eq!(
        report.recompacted_bytes_after,
        big_l19_size + small_l19_size
    );

    // Untouched skips keep their ETags.
    assert_eq!(
        head_etag(&backend, bucket, plain_key).await,
        etags_after_migrate[plain_key]
    );
    assert_eq!(
        head_etag(&backend, bucket, pt_key).await,
        etags_after_migrate[pt_key]
    );
    assert_eq!(
        head_etag(&backend, bucket, unstamped_key).await,
        etags_after_migrate[unstamped_key],
        "unstamped-framed object must be untouched without --assume-unstamped-framed"
    );

    // Tags + storage class survived the recompact rewrite too.
    assert_eq!(
        get_tags(&backend, bucket, small_key).await,
        vec![
            ("env".to_owned(), "prod".to_owned()),
            ("team".to_owned(), "s4 core".to_owned()),
        ],
        "recompact must carry object tags over"
    );
    assert_eq!(
        head_storage_class(&backend, bucket, small_key)
            .await
            .as_deref(),
        Some("REDUCED_REDUNDANCY"),
        "recompact must carry the storage class over"
    );

    // `s4-zstd-level` stamp + manifest re-stamp + user metadata carry.
    let big_meta = head_meta(&backend, bucket, big_key).await;
    assert_eq!(
        big_meta.get("s4-zstd-level").map(String::as_str),
        Some("19")
    );
    assert_eq!(
        big_meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd")
    );
    assert_eq!(big_meta.get("s4-framed").map(String::as_str), Some("true"));
    assert_eq!(
        big_meta.get("s4-original-size").map(String::as_str),
        Some(big_body.len().to_string().as_str())
    );
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
        small_meta.get("s4-zstd-level").map(String::as_str),
        Some("19")
    );

    // Multi-frame object's rewritten sidecar verifies Ok; the
    // single-frame object correctly has none (gateway parity).
    let verify = verify_sidecar(&backend, bucket, big_key, DEFAULT_REPAIR_BODY_BYTES_CAP)
        .await
        .expect("verify sidecar");
    assert!(
        matches!(verify.status, SidecarStatus::Ok { .. }),
        "sidecar must verify Ok, got {:?}",
        verify.status
    );
    let small_sidecar = backend
        .head_object()
        .bucket(bucket)
        .key(sidecar_key(small_key))
        .send()
        .await;
    assert!(
        small_sidecar.is_err(),
        "single-frame object must not get a sidecar"
    );

    // Gateway readback: GETs through a real S4 gateway return the
    // original plaintext for every key (recompacted and skipped alike).
    let (s4_endpoint, shutdown) = spawn_s4_server(&fixture.endpoint_url).await;
    let via_gateway = build_aws_client(&s4_endpoint);
    assert_eq!(get_via(&via_gateway, bucket, big_key).await, big_body);
    assert_eq!(get_via(&via_gateway, bucket, small_key).await, small_body);
    assert_eq!(get_via(&via_gateway, bucket, plain_key).await, plain_body);
    assert_eq!(get_via(&via_gateway, bucket, pt_key).await, pt_body);
    // Range GET through the gateway exercises the rewritten sidecar's
    // partial-fetch path on the multi-frame object.
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
    let rerun = run_recompact(&backend, bucket, &exec_params)
        .await
        .expect("re-run");
    assert_eq!(rerun.recompacted, 0, "report: {rerun:?}");
    assert_eq!(rerun.skipped_already_compacted, 2);
    assert_eq!(rerun.skipped_not_s4, 1);
    assert_eq!(rerun.skipped_unsupported_codec, 1);
    assert_eq!(rerun.skipped_unstamped_framed, 1);
    assert_eq!(rerun.failed, 0);
    assert_eq!(head_etag(&backend, bucket, big_key).await, etag_after_big);
    assert_eq!(
        head_etag(&backend, bucket, small_key).await,
        etag_after_small
    );
}
