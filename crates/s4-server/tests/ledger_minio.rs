//! v1.2 `--savings-ledger-state-file` E2E against a real MinIO
//! container.
//!
//! Docker required, so gated with `#[ignore]` exactly like
//! `dict_minio.rs` / `migrate_minio.rs`:
//!
//! ```bash
//! cargo test --test ledger_minio -- --ignored --nocapture
//! ```
//!
//! Covered acceptance criteria:
//! (a) a ledger-enabled gateway records `original_bytes` == the exact
//!     client-PUT byte counts and `stored_bytes` == the **measured**
//!     backend usage (compressed frames + sidecars), for both a
//!     compressed (cpu-zstd, multi-frame + sidecar) and a passthrough
//!     (incompressible) PUT;
//! (b) overwriting an existing key swaps the footprint (no
//!     double-count, `objects` unchanged);
//! (c) a multipart upload (Create / UploadPart ×2 / Complete) is
//!     accounted at Complete time — original == uploaded part bytes,
//!     stored == backend bytes incl. the multipart sidecar;
//! (d) DELETE subtracts the object's footprint (manifest-metadata HEAD
//!     probe) including its sidecar;
//! (e) a gateway restart reloads the state file and keeps
//!     accumulating on top of the persisted counters;
//! (f) the `s4 savings` CLI (real binary, `--format json` + `table`)
//!     reports exactly the state file's numbers — gateway running or
//!     not.

use std::sync::Arc;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use s3s::auth::SimpleAuth;
use s3s::service::S3ServiceBuilder;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::SamplingDispatcher;
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::ledger::{LedgerSnapshot, SavingsLedger};
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const MINIO_USER: &str = "minioadmin";
const MINIO_PASS: &str = "minioadmin";
const BUCKET: &str = "ledgerbkt";

struct MinioFixture {
    _container: ContainerAsync<MinIO>,
    endpoint_url: String,
}

async fn start_minio() -> MinioFixture {
    let container = MinIO::default().start().await.expect("start MinIO");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(9000).await.expect("api port");
    MinioFixture {
        _container: container,
        endpoint_url: format!("http://{host}:{port}"),
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

/// Spawn a real S4 gateway in front of MinIO with the savings ledger
/// attached (same harness shape as `dict_minio.rs::spawn_s4_server`).
/// SamplingDispatcher so compressible bodies take cpu-zstd and random
/// bodies take passthrough — both ledger paths get exercised.
async fn spawn_s4_server(
    backend_endpoint: &str,
    ledger: Arc<SavingsLedger>,
) -> (String, oneshot::Sender<()>) {
    let backend_client = build_aws_client(backend_endpoint);
    let proxy = s3s_aws::Proxy::from(backend_client);
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    );
    let dispatcher = Arc::new(SamplingDispatcher::new(CodecKind::CpuZstd));
    let s4 = S4Service::new(proxy, registry, dispatcher).with_savings_ledger(ledger);

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

/// Repetitive JSON-ish lines — compresses hard (cpu-zstd pick).
fn compressible_body(len: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(len + 256);
    let mut i = 0u64;
    while buf.len() < len {
        buf.extend_from_slice(
            format!(
                "{{\"ts\":\"2026-06-10T12:00:{:02}Z\",\"level\":\"info\",\
                 \"service\":\"checkout-api\",\"event\":\"order_created\",\
                 \"seq\":{i},\"region\":\"ap-northeast-1\"}}\n",
                i % 60
            )
            .as_bytes(),
        );
        i += 1;
    }
    buf.truncate(len);
    buf
}

/// xorshift64* noise — does not compress (passthrough pick).
fn random_body(len: usize, seed: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(len + 8);
    let mut x = seed | 1;
    while buf.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        buf.extend_from_slice(&x.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes());
    }
    buf.truncate(len);
    buf
}

async fn put_via(client: &aws_sdk_s3::Client, key: &str, body: Vec<u8>) {
    client
        .put_object()
        .bucket(BUCKET)
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(body))
        .send()
        .await
        .unwrap_or_else(|e| panic!("PUT {key}: {e}"));
}

/// Sum of ALL backend object sizes in the bucket (compressed bodies +
/// sidecars + multipart assemblies) — the measured ground truth the
/// ledger's `stored_bytes` must equal.
async fn backend_stored_total(client: &aws_sdk_s3::Client) -> u64 {
    let resp = client
        .list_objects_v2()
        .bucket(BUCKET)
        .send()
        .await
        .expect("backend list");
    resp.contents()
        .iter()
        .map(|o| o.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0))
        .sum()
}

fn read_snapshot(path: &std::path::Path) -> LedgerSnapshot {
    let raw = std::fs::read_to_string(path).expect("read ledger state file");
    LedgerSnapshot::from_json(&raw).expect("parse ledger state file")
}

fn bucket_totals(snap: &LedgerSnapshot) -> s4_server::ledger::BucketTotals {
    snap.buckets
        .get(BUCKET)
        .copied()
        .unwrap_or_else(|| panic!("no ledger entry for bucket {BUCKET}: {snap:?}"))
}

#[tokio::test]
#[ignore = "requires Docker (MinIO testcontainer); run with --ignored"]
async fn ledger_e2e_put_overwrite_multipart_delete_restart_cli() {
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    backend
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("create bucket");

    let state_dir = tempfile::tempdir().expect("tempdir");
    let state_path = state_dir.path().join("savings-ledger.json");

    // ---- gateway 1: fresh ledger ---------------------------------------
    let ledger = Arc::new(SavingsLedger::attach(
        LedgerSnapshot::default(),
        state_path.clone(),
    ));
    let (gw_url, gw_stop) = spawn_s4_server(&minio.endpoint_url, Arc::clone(&ledger)).await;
    let gw = build_aws_client(&gw_url);

    // (a) compressed PUT: 6 MiB of repetitive JSON → cpu-zstd, 2 frames
    // (4 MiB chunking) + a `.s4index` sidecar.
    let log_len: u64 = 6 * 1024 * 1024;
    put_via(&gw, "logs/app.log", compressible_body(log_len as usize)).await;
    // (a) passthrough PUT: 64 KiB of noise → stored == original.
    let bin_len: u64 = 64 * 1024;
    put_via(&gw, "data/blob.bin", random_body(bin_len as usize, 0x5EED)).await;

    let snap = read_snapshot(&state_path);
    let t = bucket_totals(&snap);
    assert_eq!(t.objects, 2, "two gateway-written objects");
    assert_eq!(
        t.original_bytes,
        log_len + bin_len,
        "original must be the exact client-PUT byte count"
    );
    let measured = backend_stored_total(&backend).await;
    assert_eq!(
        t.stored_bytes, measured,
        "ledger stored_bytes must equal the measured backend usage (frames + sidecar)"
    );
    assert!(
        t.stored_bytes < t.original_bytes,
        "compressible workload must show savings (stored {} >= original {})",
        t.stored_bytes,
        t.original_bytes
    );

    // (b) overwrite the log with a smaller body: footprint swap, not a
    // double count.
    let log2_len: u64 = 5 * 1024 * 1024;
    put_via(&gw, "logs/app.log", compressible_body(log2_len as usize)).await;
    let t = bucket_totals(&read_snapshot(&state_path));
    assert_eq!(t.objects, 2, "overwrite must not bump the object count");
    assert_eq!(t.original_bytes, log2_len + bin_len);
    assert_eq!(t.stored_bytes, backend_stored_total(&backend).await);

    // (c) multipart upload: 8 MiB compressible part + 1 MiB noise part.
    let mp_part1 = compressible_body(8 * 1024 * 1024);
    let mp_part2 = random_body(1024 * 1024, 0xBADC0DE);
    let mp_len = (mp_part1.len() + mp_part2.len()) as u64;
    let create = gw
        .create_multipart_upload()
        .bucket(BUCKET)
        .key("mp/parts.bin")
        .send()
        .await
        .expect("create multipart");
    let upload_id = create.upload_id().expect("upload id").to_owned();
    let mut completed_parts = Vec::new();
    for (part_number, body) in [(1, mp_part1), (2, mp_part2)] {
        let part = gw
            .upload_part()
            .bucket(BUCKET)
            .key("mp/parts.bin")
            .upload_id(&upload_id)
            .part_number(part_number)
            .body(aws_sdk_s3::primitives::ByteStream::from(body))
            .send()
            .await
            .unwrap_or_else(|e| panic!("upload part {part_number}: {e}"));
        completed_parts.push(
            CompletedPart::builder()
                .part_number(part_number)
                .e_tag(part.e_tag().expect("part etag"))
                .build(),
        );
    }
    gw.complete_multipart_upload()
        .bucket(BUCKET)
        .key("mp/parts.bin")
        .upload_id(&upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(completed_parts))
                .build(),
        )
        .send()
        .await
        .expect("complete multipart");
    let t = bucket_totals(&read_snapshot(&state_path));
    assert_eq!(t.objects, 3, "multipart Complete adds one object");
    assert_eq!(
        t.original_bytes,
        log2_len + bin_len + mp_len,
        "multipart original must be the uploaded part bytes"
    );
    assert_eq!(
        t.stored_bytes,
        backend_stored_total(&backend).await,
        "multipart stored must match the measured backend usage"
    );

    // (d) DELETE the compressed object: footprint (incl. sidecar) is
    // subtracted via the HEAD probe.
    gw.delete_object()
        .bucket(BUCKET)
        .key("logs/app.log")
        .send()
        .await
        .expect("delete app.log");
    let after_delete = read_snapshot(&state_path);
    let t = bucket_totals(&after_delete);
    assert_eq!(t.objects, 2, "DELETE must drop the object count");
    assert_eq!(t.original_bytes, bin_len + mp_len);
    assert_eq!(
        t.stored_bytes,
        backend_stored_total(&backend).await,
        "post-DELETE stored must match the backend (object + sidecar both subtracted)"
    );

    // (e) restart: stop gateway 1, reload the state file the way
    // main.rs does (load_or_fresh + attach), spawn gateway 2.
    let _ = gw_stop.send(());
    let reloaded = s4_server::state_loader::load_or_fresh(
        "savings_ledger",
        &state_path,
        LedgerSnapshot::from_json,
    );
    assert_eq!(
        reloaded, after_delete,
        "restart must reload exactly the persisted counters"
    );
    let ledger2 = Arc::new(SavingsLedger::attach(reloaded, state_path.clone()));
    let (gw2_url, gw2_stop) = spawn_s4_server(&minio.endpoint_url, ledger2).await;
    let gw2 = build_aws_client(&gw2_url);
    let extra_len: u64 = 32 * 1024;
    put_via(
        &gw2,
        "data/extra.bin",
        random_body(extra_len as usize, 0x7777),
    )
    .await;
    let t = bucket_totals(&read_snapshot(&state_path));
    assert_eq!(t.objects, 3, "post-restart write must accumulate");
    assert_eq!(t.original_bytes, bin_len + mp_len + extra_len);
    assert_eq!(t.stored_bytes, backend_stored_total(&backend).await);

    // (f) the real `s4 savings` CLI agrees with the state file —
    // JSON shape first.
    let final_snap = read_snapshot(&state_path);
    let final_totals = bucket_totals(&final_snap);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_s4"))
        .args([
            "savings",
            "--state-file",
            state_path.to_str().expect("utf8 path"),
            "--format",
            "json",
        ])
        .output()
        .expect("run s4 savings --format json");
    assert!(
        out.status.success(),
        "s4 savings must exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("CLI JSON output parses");
    assert_eq!(json["total_objects"], final_totals.objects);
    assert_eq!(json["total_original_bytes"], final_totals.original_bytes);
    assert_eq!(json["total_stored_bytes"], final_totals.stored_bytes);
    assert_eq!(json["buckets"][0]["bucket"], BUCKET);
    assert_eq!(json["buckets"][0]["objects"], final_totals.objects);
    assert_eq!(
        json["buckets"][0]["original_bytes"],
        final_totals.original_bytes
    );
    assert_eq!(
        json["buckets"][0]["stored_bytes"],
        final_totals.stored_bytes
    );
    assert!(
        json["notes"].as_array().is_some_and(|a| !a.is_empty()),
        "honesty notes must be present in the CLI output"
    );

    // ...and the human table renders the same figures.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_s4"))
        .args([
            "savings",
            "--state-file",
            state_path.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run s4 savings (table)");
    assert!(out.status.success());
    let table = String::from_utf8_lossy(&out.stdout).into_owned();
    println!("--- s4 savings table output ---\n{table}");
    assert!(table.contains("S4 measured savings"));
    assert!(table.contains(BUCKET));
    assert!(
        table.contains(&format!("total: {} objects", final_totals.objects)),
        "table must carry the object total: {table}"
    );
    assert!(table.contains("gateway-traversing writes only"));

    let _ = gw2_stop.send(());
}
