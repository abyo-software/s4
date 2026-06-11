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
//!
//! v1.2 audit R1 additions:
//! (g) CopyObject accounting — same-bucket copy adds, REPLACE-directive
//!     overwrite swaps (no double count), copy destinations are
//!     marker-accounted so their DELETE subtracts symmetrically, and
//!     cross-bucket copies open the destination bucket's row;
//! (h) the `s4-ledger` marker gate — a backend-direct ("around the
//!     gateway") S4-stamped object DELETEd / overwritten through the
//!     gateway is NEVER subtracted (no negative / nonsense counters
//!     even with forged `s4-original-size`), the skip is tallied in
//!     `skipped_unaccounted`, and the `s4 savings` report discloses it;
//! (i) versioning-Enabled accounting — each stored version adds one
//!     ledger object, a delete-marker DELETE changes nothing, and a
//!     specific-version DELETE subtracts exactly that version.

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
    spawn_s4_server_with(backend_endpoint, ledger, None).await
}

/// Variant with an optional VersioningManager for the (i) coverage.
async fn spawn_s4_server_with(
    backend_endpoint: &str,
    ledger: Arc<SavingsLedger>,
    versioning: Option<Arc<s4_server::versioning::VersioningManager>>,
) -> (String, oneshot::Sender<()>) {
    let backend_client = build_aws_client(backend_endpoint);
    let proxy = s3s_aws::Proxy::from(backend_client);
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    );
    let dispatcher = Arc::new(SamplingDispatcher::new(CodecKind::CpuZstd));
    let mut s4 = S4Service::new(proxy, registry, dispatcher).with_savings_ledger(ledger);
    if let Some(mgr) = versioning {
        s4 = s4.with_versioning(mgr);
    }

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
    stored_total_in(client, BUCKET).await
}

fn read_snapshot(path: &std::path::Path) -> LedgerSnapshot {
    let raw = std::fs::read_to_string(path).expect("read ledger state file");
    LedgerSnapshot::from_json(&raw).expect("parse ledger state file")
}

fn bucket_totals(snap: &LedgerSnapshot) -> s4_server::ledger::BucketTotals {
    totals_in(snap, BUCKET)
}

fn totals_in(snap: &LedgerSnapshot, bucket: &str) -> s4_server::ledger::BucketTotals {
    snap.buckets
        .get(bucket)
        .copied()
        .unwrap_or_else(|| panic!("no ledger entry for bucket {bucket}: {snap:?}"))
}

async fn put_key(client: &aws_sdk_s3::Client, bucket: &str, key: &str, body: Vec<u8>) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(body))
        .send()
        .await
        .unwrap_or_else(|e| panic!("PUT {bucket}/{key}: {e}"));
}

/// Sum of ALL backend object sizes in `bucket` (compressed bodies +
/// sidecars + versioning shadow keys) — the measured ground truth.
async fn stored_total_in(client: &aws_sdk_s3::Client, bucket: &str) -> u64 {
    let resp = client
        .list_objects_v2()
        .bucket(bucket)
        .send()
        .await
        .expect("backend list");
    resp.contents()
        .iter()
        .map(|o| o.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0))
        .sum()
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

/// v1.2 audit R1 — (g) CopyObject ledger hooks + (h) the `s4-ledger`
/// marker gate against backend-direct ("around the gateway") objects.
#[tokio::test]
#[ignore = "requires Docker (MinIO testcontainer); run with --ignored"]
async fn ledger_e2e_copy_hooks_and_unaccounted_marker_gate() {
    const SRC: &str = "cb-src";
    const DST: &str = "cb-dst";
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    for b in [SRC, DST] {
        backend
            .create_bucket()
            .bucket(b)
            .send()
            .await
            .unwrap_or_else(|e| panic!("create bucket {b}: {e}"));
    }
    let state_dir = tempfile::tempdir().expect("tempdir");
    let state_path = state_dir.path().join("savings-ledger.json");
    let ledger = Arc::new(SavingsLedger::attach(
        LedgerSnapshot::default(),
        state_path.clone(),
    ));
    let (gw_url, gw_stop) = spawn_s4_server(&minio.endpoint_url, Arc::clone(&ledger)).await;
    let gw = build_aws_client(&gw_url);

    // Seed: one compressed multi-frame object (body + sidecar).
    let src_len: u64 = 6 * 1024 * 1024;
    put_key(
        &gw,
        SRC,
        "logs/orig.log",
        compressible_body(src_len as usize),
    )
    .await;
    let t = totals_in(&read_snapshot(&state_path), SRC);
    assert_eq!(t.objects, 1);
    assert_eq!(t.original_bytes, src_len);
    assert_eq!(t.stored_bytes, stored_total_in(&backend, SRC).await);

    // (g1) same-bucket copy to a new key: +1 object, original doubles,
    // stored matches the measured backend bytes (copy carries the body
    // but not the sidecar — the ledger must agree with what's actually
    // there, not with an assumption).
    gw.copy_object()
        .bucket(SRC)
        .key("logs/copy.log")
        .copy_source(format!("{SRC}/logs/orig.log"))
        .send()
        .await
        .expect("same-bucket copy");
    let t = totals_in(&read_snapshot(&state_path), SRC);
    assert_eq!(t.objects, 2, "copy to a new key adds one ledger object");
    assert_eq!(t.original_bytes, 2 * src_len);
    assert_eq!(t.stored_bytes, stored_total_in(&backend, SRC).await);

    // (g2) REPLACE-directive copy onto the existing destination:
    // footprint swap (identical bytes), objects unchanged.
    gw.copy_object()
        .bucket(SRC)
        .key("logs/copy.log")
        .copy_source(format!("{SRC}/logs/orig.log"))
        .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace)
        .metadata("note", "replaced")
        .send()
        .await
        .expect("REPLACE copy onto existing key");
    let t = totals_in(&read_snapshot(&state_path), SRC);
    assert_eq!(t.objects, 2, "REPLACE overwrite must not double-count");
    assert_eq!(t.original_bytes, 2 * src_len);
    assert_eq!(t.stored_bytes, stored_total_in(&backend, SRC).await);

    // (g3) the copy destination is marker-accounted: its DELETE
    // subtracts symmetrically.
    gw.delete_object()
        .bucket(SRC)
        .key("logs/copy.log")
        .send()
        .await
        .expect("delete copy.log");
    let t = totals_in(&read_snapshot(&state_path), SRC);
    assert_eq!(t.objects, 1, "copy destination DELETE must subtract");
    assert_eq!(t.original_bytes, src_len);
    assert_eq!(t.stored_bytes, stored_total_in(&backend, SRC).await);

    // (g4) cross-bucket copy opens the destination bucket's row.
    gw.copy_object()
        .bucket(DST)
        .key("replica/orig.log")
        .copy_source(format!("{SRC}/logs/orig.log"))
        .send()
        .await
        .expect("cross-bucket copy");
    let snap = read_snapshot(&state_path);
    let td = totals_in(&snap, DST);
    assert_eq!(td.objects, 1);
    assert_eq!(td.original_bytes, src_len);
    assert_eq!(td.stored_bytes, stored_total_in(&backend, DST).await);
    // ...without disturbing the source bucket's row.
    assert_eq!(totals_in(&snap, SRC).objects, 1);

    // (h1) backend-direct S4-stamped object (simulates s4fs / migrate /
    // recompact output: S4 metadata, NO `s4-ledger` marker — with a
    // deliberately absurd `s4-original-size` that would crater the
    // counters if the old asymmetric subtraction were still in place).
    backend
        .put_object()
        .bucket(SRC)
        .key("rogue/direct.bin")
        .metadata("s4-original-size", "999999999999")
        .metadata("s4-codec", "cpu-zstd")
        .body(aws_sdk_s3::primitives::ByteStream::from(random_body(
            256 * 1024,
            0xD1CE,
        )))
        .send()
        .await
        .expect("backend-direct PUT");
    let before = totals_in(&read_snapshot(&state_path), SRC);
    gw.delete_object()
        .bucket(SRC)
        .key("rogue/direct.bin")
        .send()
        .await
        .expect("gateway DELETE of backend-direct object");
    let snap = read_snapshot(&state_path);
    let after = totals_in(&snap, SRC);
    assert_eq!(
        after, before,
        "DELETE of a non-ledger-managed object must not move the counters"
    );
    assert_eq!(
        snap.skipped_unaccounted.get(SRC).copied().unwrap_or(0),
        1,
        "the skipped removal must be tallied"
    );

    // (h2) gateway overwrite of a backend-direct object: the forged
    // old footprint is NOT subtracted; the new write is a fresh add.
    backend
        .put_object()
        .bucket(SRC)
        .key("rogue/overwrite.bin")
        .metadata("s4-original-size", "888888888888")
        .body(aws_sdk_s3::primitives::ByteStream::from(random_body(
            128 * 1024,
            0xFEED,
        )))
        .send()
        .await
        .expect("backend-direct PUT (overwrite target)");
    let ow_len: u64 = 1024 * 1024;
    put_key(
        &gw,
        SRC,
        "rogue/overwrite.bin",
        compressible_body(ow_len as usize),
    )
    .await;
    let snap = read_snapshot(&state_path);
    let t = totals_in(&snap, SRC);
    assert_eq!(
        t.objects, 2,
        "overwrite of an unaccounted object is a fresh add"
    );
    assert_eq!(
        t.original_bytes,
        src_len + ow_len,
        "forged old original-size must never be subtracted"
    );
    assert_eq!(t.stored_bytes, stored_total_in(&backend, SRC).await);
    assert_eq!(snap.skipped_unaccounted.get(SRC).copied().unwrap_or(0), 2);

    // (h3) the CLI disclosure: per-bucket + total skip tallies and the
    // dedicated note.
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
    assert_eq!(json["total_skipped_unaccounted"], 2);
    let src_row = json["buckets"]
        .as_array()
        .expect("buckets array")
        .iter()
        .find(|b| b["bucket"] == SRC)
        .expect("src bucket row");
    assert_eq!(src_row["skipped_unaccounted"], 2);
    assert!(
        json["notes"].as_array().is_some_and(|notes| notes
            .iter()
            .any(|n| n.as_str().is_some_and(|s| s.contains("NOT subtracted")))),
        "skip disclosure note missing from CLI output: {json}"
    );
    // Ratio / $ sanity: never negative, even with the rogue objects.
    for row in json["buckets"].as_array().expect("buckets array") {
        let ratio = row["savings_ratio"].as_f64().expect("ratio f64");
        let usd = row["monthly_savings_usd"].as_f64().expect("usd f64");
        assert!(ratio >= 0.0, "negative ratio leaked: {row}");
        assert!(usd >= 0.0, "negative $ leaked: {row}");
    }

    let _ = gw_stop.send(());
}

/// v1.2 audit R1 — (i) versioning-Enabled accounting: every stored
/// version is one ledger object; a delete-marker DELETE moves nothing;
/// a specific-version DELETE subtracts exactly that version.
#[tokio::test]
#[ignore = "requires Docker (MinIO testcontainer); run with --ignored"]
async fn ledger_e2e_versioning_enabled_accounting() {
    const VBKT: &str = "ver-bkt";
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    backend
        .create_bucket()
        .bucket(VBKT)
        .send()
        .await
        .expect("create bucket");
    let state_dir = tempfile::tempdir().expect("tempdir");
    let state_path = state_dir.path().join("savings-ledger.json");
    let ledger = Arc::new(SavingsLedger::attach(
        LedgerSnapshot::default(),
        state_path.clone(),
    ));
    let v_mgr = Arc::new(s4_server::versioning::VersioningManager::new());
    let (gw_url, gw_stop) =
        spawn_s4_server_with(&minio.endpoint_url, Arc::clone(&ledger), Some(v_mgr)).await;
    let gw = build_aws_client(&gw_url);
    gw.put_bucket_versioning()
        .bucket(VBKT)
        .versioning_configuration(
            aws_sdk_s3::types::VersioningConfiguration::builder()
                .status(aws_sdk_s3::types::BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await
        .expect("enable versioning");

    // Two versions of the same key: each PUT is a pure add (+1 object —
    // every stored version occupies backend bytes).
    let v1_len: u64 = 6 * 1024 * 1024;
    let v2_len: u64 = 5 * 1024 * 1024;
    put_key(&gw, VBKT, "logs/v.log", compressible_body(v1_len as usize)).await;
    let v2_resp = gw
        .put_object()
        .bucket(VBKT)
        .key("logs/v.log")
        .body(aws_sdk_s3::primitives::ByteStream::from(compressible_body(
            v2_len as usize,
        )))
        .send()
        .await
        .expect("PUT version 2");
    let v2_id = v2_resp.version_id().expect("v2 version id").to_owned();
    let t = totals_in(&read_snapshot(&state_path), VBKT);
    assert_eq!(t.objects, 2, "each stored version is one ledger object");
    assert_eq!(t.original_bytes, v1_len + v2_len);
    assert_eq!(
        t.stored_bytes,
        stored_total_in(&backend, VBKT).await,
        "versioned stored must equal the measured backend bytes (shadow keys + sidecar)"
    );

    // DELETE without a version id: pushes a delete marker — NO backend
    // bytes move, NO ledger movement.
    let del = gw
        .delete_object()
        .bucket(VBKT)
        .key("logs/v.log")
        .send()
        .await
        .expect("delete (marker)");
    assert_eq!(del.delete_marker(), Some(true), "must be a delete marker");
    let t_after_marker = totals_in(&read_snapshot(&state_path), VBKT);
    assert_eq!(
        t_after_marker, t,
        "a delete marker must not move the counters"
    );

    // Specific-version DELETE of v2: subtracts exactly that version's
    // footprint (v1's bytes and the latest sidecar stay on the backend
    // — the ledger must agree with the measured remainder).
    gw.delete_object()
        .bucket(VBKT)
        .key("logs/v.log")
        .version_id(&v2_id)
        .send()
        .await
        .expect("delete specific version v2");
    let t = totals_in(&read_snapshot(&state_path), VBKT);
    assert_eq!(t.objects, 1, "specific-version DELETE drops one object");
    assert_eq!(t.original_bytes, v1_len);
    assert_eq!(t.stored_bytes, stored_total_in(&backend, VBKT).await);

    let _ = gw_stop.send(());
}
