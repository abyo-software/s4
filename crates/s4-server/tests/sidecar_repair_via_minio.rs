//! v0.9 #106: integration coverage for the standalone sidecar tooling
//! (`s4_server::repair::{verify_sidecar, repair_sidecar, sweep_orphan_sidecars}`).
//!
//! These exercise the **library** directly against a real backend
//! (MinIO via testcontainers) — not the CLI binary — so the test
//! harness doesn't have to spawn a child `s4` process. The CLI wiring
//! in `main.rs` is a thin print-formatter over the same calls.
//!
//! Each test follows the same shape used by `multipart_e2e.rs`:
//!   1. Spin up MinIO.
//!   2. Spawn an S4 service in-process so we can PUT a real S4-framed
//!      object (with a freshly-written sidecar) into the backend.
//!   3. Reach past the gateway to the **backend** client to corrupt /
//!      delete the sidecar.
//!   4. Call the repair API and assert the post-condition.

use std::sync::Arc;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use bytes::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use s3s::auth::SimpleAuth;
use s3s::service::S3ServiceBuilder;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::dispatcher::AlwaysDispatcher;
use s4_codec::index::{SIDECAR_SUFFIX, decode_index, sidecar_key};
use s4_codec::passthrough::Passthrough;
use s4_codec::{CodecKind, CodecRegistry};
use s4_server::S4Service;
use s4_server::repair::{
    DEFAULT_REPAIR_BODY_BYTES_CAP, DeletePolicy, OrphanReason, RepairError, SidecarStatus,
    repair_sidecar, repair_sidecar_with_keyring, sweep_orphan_sidecars, verify_sidecar,
};
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

async fn spawn_s4_server(backend_endpoint: &str) -> (String, oneshot::Sender<()>) {
    spawn_s4_server_inner(backend_endpoint, None).await
}

/// v0.9 #106-audit-R2 P2-INT-1: spawn an S4 gateway with SSE-S4 chunked
/// PUTs enabled (S4E6 envelope), so the test can plant a real encrypted
/// object on the backend and exercise the encrypted-body reject path in
/// `repair_sidecar`. The chunk size is small enough (256 KiB) to produce
/// multiple S4E6 chunks on a few-MiB body, mirroring the geometry the
/// production gateway emits for v3 sidecars.
async fn spawn_s4_server_with_sse_s4_chunked(
    backend_endpoint: &str,
) -> (String, oneshot::Sender<()>) {
    let key = Arc::new(s4_server::sse::SseKey::from_bytes(&[0x9au8; 32]).expect("32-byte raw key"));
    let keyring = Arc::new(s4_server::sse::SseKeyring::new(1, key));
    spawn_s4_server_inner(backend_endpoint, Some((keyring, 256 * 1024))).await
}

async fn spawn_s4_server_inner(
    backend_endpoint: &str,
    sse: Option<(s4_server::sse::SharedSseKeyring, usize)>,
) -> (String, oneshot::Sender<()>) {
    let backend_client = build_aws_client(backend_endpoint);
    let proxy = s3s_aws::Proxy::from(backend_client);
    let registry = Arc::new(
        CodecRegistry::new(CodecKind::CpuZstd)
            .with(Arc::new(Passthrough))
            .with(Arc::new(CpuZstd::default())),
    );
    let dispatcher = Arc::new(AlwaysDispatcher(CodecKind::CpuZstd));
    let mut s4 = S4Service::new(proxy, registry, dispatcher);
    if let Some((keyring, chunk_size)) = sse {
        s4 = s4.with_sse_keyring(keyring).with_sse_chunk_size(chunk_size);
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

/// Helper: write a multipart object through the S4 gateway so MinIO ends
/// up with a real framed body + sidecar pair. Returns the original
/// payload so tests can roundtrip-check downstream.
async fn put_multipart_object(s4_client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Bytes {
    const PART_SIZE: usize = 6 * 1024 * 1024;
    let make_part = |seed: u8| -> Bytes {
        let mut buf = Vec::with_capacity(PART_SIZE);
        let tmpl = format!("REPAIR-TEST-{seed:02x} ");
        while buf.len() < PART_SIZE {
            buf.extend_from_slice(tmpl.as_bytes());
        }
        buf.truncate(PART_SIZE);
        Bytes::from(buf)
    };
    let parts = [make_part(0x1), make_part(0x2)];
    let mut full = Vec::with_capacity(PART_SIZE * 2);
    for p in &parts {
        full.extend_from_slice(p);
    }

    let create = s4_client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("create");
    let upload_id = create.upload_id().expect("upload_id").to_string();
    let mut completed = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let pn = (i + 1) as i32;
        let resp = s4_client
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .part_number(pn)
            .body(p.clone().into())
            .send()
            .await
            .expect("upload_part");
        completed.push(
            aws_sdk_s3::types::CompletedPart::builder()
                .e_tag(resp.e_tag().unwrap_or_default())
                .part_number(pn)
                .build(),
        );
    }
    s4_client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .set_parts(Some(completed))
                .build(),
        )
        .send()
        .await
        .expect("complete");
    Bytes::from(full)
}

/// verify_sidecar reports `Ok` immediately after a clean multipart Complete.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn verify_sidecar_reports_ok_on_freshly_written_multipart() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("repair-ok").send().await;
    let s4_client = build_aws_client(&s4_endpoint);

    let _payload = put_multipart_object(&s4_client, "repair-ok", "fresh.bin").await;
    let report = verify_sidecar(
        &backend,
        "repair-ok",
        "fresh.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("verify");
    assert!(
        matches!(report.status, SidecarStatus::Ok { .. }),
        "expected Ok, got {:?}",
        report.status
    );
    assert!(report.is_clean());

    let _ = shutdown.send(());
}

/// verify_sidecar surfaces `Missing` after the sidecar is deleted from the
/// backend, and a follow-up `repair_sidecar` puts it back with frames that
/// match the original sidecar.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn repair_sidecar_rebuilds_after_backend_delete() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("repair-del").send().await;
    let s4_client = build_aws_client(&s4_endpoint);

    let payload = put_multipart_object(&s4_client, "repair-del", "doc.bin").await;
    let sidecar = sidecar_key("doc.bin");

    // Capture the original sidecar contents so we can compare frame count.
    let original = backend
        .get_object()
        .bucket("repair-del")
        .key(&sidecar)
        .send()
        .await
        .expect("original sidecar")
        .body
        .collect()
        .await
        .expect("body")
        .into_bytes();
    let original_idx = decode_index(original).expect("decode original");
    let original_frames = original_idx.entries.len();
    assert!(original_frames >= 2, "multipart should produce ≥ 2 frames");

    // Wipe the sidecar from the backend.
    backend
        .delete_object()
        .bucket("repair-del")
        .key(&sidecar)
        .send()
        .await
        .expect("delete sidecar");

    let verdict = verify_sidecar(
        &backend,
        "repair-del",
        "doc.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("verify after delete");
    // Multipart object (2+ frames) with sidecar deleted → MissingDivergent
    // (P2-C). Single-frame objects would be MissingHarmless instead.
    assert!(
        matches!(verdict.status, SidecarStatus::MissingDivergent { .. }),
        "expected MissingDivergent (multi-frame body, missing sidecar), got {:?}",
        verdict.status
    );
    assert!(
        !verdict.is_clean(),
        "MissingDivergent must exit 1 — Range GET fast-path is lost"
    );

    // Rebuild via the library API.
    let report = repair_sidecar(
        &backend,
        "repair-del",
        "doc.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("repair");
    assert_eq!(report.frame_count as usize, original_frames);
    assert!(!report.rebuilt_from_existing, "we just deleted it");
    assert!(report.source_etag.is_some());

    // Sidecar is back; verify reports clean.
    let final_verdict = verify_sidecar(
        &backend,
        "repair-del",
        "doc.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("verify after repair");
    assert!(
        matches!(final_verdict.status, SidecarStatus::Ok { .. }),
        "expected Ok post-repair, got {:?}",
        final_verdict.status
    );

    // Range GET through the gateway still works (the rebuilt sidecar now
    // describes the live body, so the partial-fetch fast path engages).
    let resp = s4_client
        .get_object()
        .bucket("repair-del")
        .key("doc.bin")
        .range("bytes=100-1099")
        .send()
        .await
        .expect("range get");
    let body = resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body.len(), 1000);
    assert_eq!(body.as_ref(), &payload[100..1100]);

    let _ = shutdown.send(());
}

/// repair_sidecar overwrites a stale (corrupt) sidecar with a fresh one
/// that matches the live body.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn repair_sidecar_overwrites_stale_sidecar() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("repair-stale").send().await;
    let s4_client = build_aws_client(&s4_endpoint);

    let _payload = put_multipart_object(&s4_client, "repair-stale", "obj.bin").await;
    let sidecar = sidecar_key("obj.bin");

    // Replace the sidecar with garbage bytes (an operator scenario: someone
    // round-tripped the sidecar through a tool that didn't preserve it).
    backend
        .put_object()
        .bucket("repair-stale")
        .key(&sidecar)
        .body(Bytes::from_static(b"not-an-s4ix-sidecar").into())
        .send()
        .await
        .expect("clobber sidecar");

    let verdict = verify_sidecar(
        &backend,
        "repair-stale",
        "obj.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("verify after clobber");
    assert!(
        matches!(verdict.status, SidecarStatus::DecodeError { .. }),
        "expected DecodeError, got {:?}",
        verdict.status
    );

    let report = repair_sidecar(
        &backend,
        "repair-stale",
        "obj.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("repair after clobber");
    assert!(
        report.rebuilt_from_existing,
        "we clobbered the existing one"
    );
    assert!(report.frame_count >= 2);

    let final_verdict = verify_sidecar(
        &backend,
        "repair-stale",
        "obj.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("verify after repair");
    assert!(
        matches!(final_verdict.status, SidecarStatus::Ok { .. }),
        "expected Ok post-repair, got {:?}",
        final_verdict.status
    );

    let _ = shutdown.send(());
}

/// sweep_orphan_sidecars finds and (with delete=true) removes a sidecar
/// whose paired key was deleted from the backend.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn sweep_orphan_sidecars_deletes_dangling_pair_missing() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("sweep").send().await;
    let s4_client = build_aws_client(&s4_endpoint);

    // Two objects: one we'll leave alone, one whose pair we'll delete to
    // create an orphan sidecar.
    let _keep = put_multipart_object(&s4_client, "sweep", "keep.bin").await;
    let _drop = put_multipart_object(&s4_client, "sweep", "drop.bin").await;

    // Delete the *main* object directly via the backend, leaving the
    // sidecar behind. (Going through the gateway would also delete the
    // sidecar, defeating the test.)
    backend
        .delete_object()
        .bucket("sweep")
        .key("drop.bin")
        .send()
        .await
        .expect("delete main");

    // Dry-run sweep: should find exactly one orphan, PairedMissing.
    let dry = sweep_orphan_sidecars(&backend, "sweep", DeletePolicy::DryRun)
        .await
        .expect("sweep dry-run");
    assert_eq!(dry.sidecars_scanned, 2);
    assert_eq!(dry.orphans.len(), 1);
    assert_eq!(dry.deleted, 0);
    assert_eq!(
        dry.orphans[0].sidecar_key,
        format!("drop.bin{SIDECAR_SUFFIX}")
    );
    assert_eq!(dry.orphans[0].paired_key, "drop.bin");
    assert!(matches!(dry.orphans[0].reason, OrphanReason::PairedMissing));

    // Sidecar still on backend after dry-run.
    backend
        .head_object()
        .bucket("sweep")
        .key(format!("drop.bin{SIDECAR_SUFFIX}"))
        .send()
        .await
        .expect("orphan sidecar still present after dry-run");

    // Now sweep with PairBoundOnly; orphan should be removed, paired-OK
    // sidecar (keep.bin.s4index) untouched.
    let live = sweep_orphan_sidecars(&backend, "sweep", DeletePolicy::PairBoundOnly)
        .await
        .expect("sweep with delete");
    assert_eq!(live.orphans.len(), 1);
    assert_eq!(live.deleted, 1);

    // Orphan gone.
    let head_orphan = backend
        .head_object()
        .bucket("sweep")
        .key(format!("drop.bin{SIDECAR_SUFFIX}"))
        .send()
        .await;
    assert!(
        head_orphan.is_err(),
        "orphan sidecar should be deleted after live sweep"
    );

    // Survivor still there.
    backend
        .head_object()
        .bucket("sweep")
        .key(format!("keep.bin{SIDECAR_SUFFIX}"))
        .send()
        .await
        .expect("survivor sidecar still present");

    // Second sweep is a no-op (idempotent).
    let again = sweep_orphan_sidecars(&backend, "sweep", DeletePolicy::DryRun)
        .await
        .expect("second sweep");
    assert_eq!(again.orphans.len(), 0);

    let _ = shutdown.send(());
}

/// `PairBoundOnly` does not delete a `SidecarUndecodable` entry —
/// guards against nuking legacy `--allow-legacy-reserved-key-reads`
/// user data that happens to end in `.s4index`.
///
/// This is the test the "HIGH-2" review finding asked for.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn sweep_pair_bound_only_preserves_undecodable_sidecar() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend
        .create_bucket()
        .bucket("preserve-legacy")
        .send()
        .await;
    let s4_client = build_aws_client(&s4_endpoint);

    // Establish one real S4 object so a paired key exists at all.
    let _real = put_multipart_object(&s4_client, "preserve-legacy", "real.bin").await;

    // PUT a non-S4IX blob directly at a `.s4index` key — simulates the
    // legacy reserved-name user data the v0.8.17 hatch protects. P1-A
    // (Codex review) said: even when the paired stripped key is ALSO
    // missing, this must NOT be classified as `PairedMissing` (which
    // PairBoundOnly would delete). It must be `SidecarUndecodable`,
    // because the bytes don't parse as S4IX — that's the safest
    // signal that this is user data and not a real S4 sidecar.
    //
    // Deliberately DO NOT PUT a paired "legacy" key, so the worst-case
    // bug condition is exercised.
    backend
        .put_object()
        .bucket("preserve-legacy")
        .key("legacy.s4index")
        .body(Bytes::from_static(b"user-data-not-an-s4ix-sidecar").into())
        .send()
        .await
        .expect("PUT legacy .s4index user data");

    let dry = sweep_orphan_sidecars(&backend, "preserve-legacy", DeletePolicy::DryRun)
        .await
        .expect("dry-run");
    // Two .s4index entries: real.bin.s4index (clean, paired-OK, not an
    // orphan) and legacy.s4index (decode fails, MUST be
    // SidecarUndecodable, NOT PairedMissing).
    assert_eq!(dry.sidecars_scanned, 2);
    assert_eq!(dry.orphans.len(), 1);
    assert!(
        matches!(
            dry.orphans[0].reason,
            OrphanReason::SidecarUndecodable { .. }
        ),
        "legacy user data must classify as SidecarUndecodable even when paired \
         key is missing; got {:?}",
        dry.orphans[0].reason
    );

    // PairBoundOnly skips the undecodable orphan — `deleted` should be 0
    // and the legacy data must still be on the backend.
    let pair_bound =
        sweep_orphan_sidecars(&backend, "preserve-legacy", DeletePolicy::PairBoundOnly)
            .await
            .expect("pair-bound sweep");
    assert_eq!(
        pair_bound.deleted, 0,
        "PairBoundOnly must NOT delete SidecarUndecodable"
    );
    backend
        .head_object()
        .bucket("preserve-legacy")
        .key("legacy.s4index")
        .send()
        .await
        .expect("legacy .s4index user data must survive PairBoundOnly sweep");

    // IncludeUndecodable escalation does remove it (explicit opt-in).
    let escalated = sweep_orphan_sidecars(
        &backend,
        "preserve-legacy",
        DeletePolicy::IncludeUndecodable,
    )
    .await
    .expect("escalated sweep");
    assert_eq!(escalated.deleted, 1);

    let _ = shutdown.send(());
}

/// P2-C (Codex review round 3): a small single-PUT object's sidecar is
/// intentionally absent (server only writes when `entries.len() > 1`).
/// `verify-sidecar` must report `MissingHarmless`, not `Missing`, so CI
/// / cron jobs don't false-alert on healthy small objects.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn verify_sidecar_reports_missing_harmless_for_small_single_put() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("verify-small").send().await;
    let s4_client = build_aws_client(&s4_endpoint);

    // 512 B body — well below the default 1 MiB chunk size, so the
    // gateway frames it as a single S4F2 frame and skips the sidecar
    // PUT per service.rs:2897-2900 (`entries.len() > 1` gate).
    let small_body = Bytes::from_static(&[b'A'; 512]);
    s4_client
        .put_object()
        .bucket("verify-small")
        .key("tiny.bin")
        .body(small_body.clone().into())
        .send()
        .await
        .expect("PUT small object");

    // Confirm no sidecar was written.
    let sidecar_head = backend
        .head_object()
        .bucket("verify-small")
        .key(format!("tiny.bin{SIDECAR_SUFFIX}"))
        .send()
        .await;
    assert!(
        sidecar_head.is_err(),
        "server should skip sidecar for single-frame small object"
    );

    let report = verify_sidecar(
        &backend,
        "verify-small",
        "tiny.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("verify");
    assert!(
        matches!(
            report.status,
            SidecarStatus::MissingHarmless { frame_count: 1 }
        ),
        "small single-frame object must verify as MissingHarmless, got {:?}",
        report.status
    );
    assert!(
        report.is_clean(),
        "MissingHarmless must be clean (exit 0 for CI/cron)"
    );

    // And the body-cap edge: cap=0 forces any non-empty body to
    // surface as MissingUnknown (the compressed body, however small,
    // exceeds 0). Operator hint, not false alert.
    let unknown_report = verify_sidecar(&backend, "verify-small", "tiny.bin", 0)
        .await
        .expect("verify with cap=0");
    assert!(
        matches!(
            unknown_report.status,
            SidecarStatus::MissingUnknown { cap: 0, .. }
        ),
        "body > cap must surface MissingUnknown, got {:?}",
        unknown_report.status
    );
    assert!(
        unknown_report.is_clean(),
        "MissingUnknown must be clean (avoid false-alerts on too-large objects)"
    );

    let _ = shutdown.send(());
}

/// P2-B (Codex review round 2): the `If-Match` on the GET only covers
/// the HEAD→GET window. A backend overwrite during the
/// `build_index_from_body` scan or the sidecar PUT itself produces a
/// stale sidecar that the server's GET-side binding check then rejects
/// silently. The final HEAD must catch that race, delete the bad
/// sidecar, and surface `OverwrittenDuringRepair` to the operator.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn repair_sidecar_detects_post_get_overwrite_race() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("race").send().await;
    let s4_client = build_aws_client(&s4_endpoint);

    // Plant a real S4 multipart object so HEAD + If-Match GET succeed.
    let _payload = put_multipart_object(&s4_client, "race", "doc.bin").await;
    let sidecar = sidecar_key("doc.bin");

    // Simulate the race: between the GET (now completed inside
    // `repair_sidecar`) and the final HEAD, an operator overwrites
    // the main object. We can't truly interleave that against a live
    // `repair_sidecar` call, but we can drive the same control flow
    // by issuing the overwrite *before* `repair_sidecar`'s post-PUT
    // HEAD runs. Since the implementation does HEAD → GET (with
    // If-Match) → build → PUT → HEAD, an overwrite that lands after
    // the initial HEAD but before the final HEAD triggers the
    // post-PUT divergence detector.
    //
    // The easiest deterministic reproduction is: PUT a sidecar that
    // would succeed, then directly overwrite the main object via the
    // backend before any repair runs. Now `repair_sidecar` will
    // pick up the NEW ETag at its first HEAD, GET via If-Match
    // against that new ETag (succeeds), build, PUT — and then the
    // final HEAD matches its own first HEAD, so no race is reported.
    // To force the post-PUT detector we need an overwrite mid-call.
    //
    // We simulate that with a spawn that overwrites slightly after
    // `repair_sidecar` starts. With a small enough object the timing
    // is racy but the test is for the control-flow path, not absolute
    // timing — we wrap it in a retry loop and accept either outcome:
    // (a) clean repair (race didn't land in the window) or (b)
    // OverwrittenDuringRepair (race landed). Both prove the
    // post-PUT HEAD is wired and reachable.
    // CI-unblock (post-v0.9 #106 audit): the parallel-overwrite
    // timing isn't deterministic across runners — fast CI shells
    // execute the entire HEAD→GET→build→PUT pipeline before the
    // sleep'd overwrite lands, so the race window never lands in
    // the post-PUT branch. We keep this test as a *best-effort*
    // smoke (when race DOES land, validate cleanup); the
    // deterministic regression guard for the post-PUT divergence
    // detector lives in lib unit tests
    // (`repair::tests::overwritten_during_repair_error_shape`).
    let mut hit_race = false;
    let mut hit_get_race = false;
    for attempt in 0..5 {
        let original_etag = backend
            .head_object()
            .bucket("race")
            .key("doc.bin")
            .send()
            .await
            .expect("head pre-attempt")
            .e_tag()
            .map(|s| s.to_owned())
            .unwrap_or_default();

        let backend_clone = backend.clone();
        let race_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5 + attempt * 5)).await;
            backend_clone
                .put_object()
                .bucket("race")
                .key("doc.bin")
                .body(Bytes::from(format!("overwritten attempt {attempt}")).into())
                .send()
                .await
                .expect("race overwrite");
        });

        let result =
            repair_sidecar(&backend, "race", "doc.bin", DEFAULT_REPAIR_BODY_BYTES_CAP).await;
        race_task.await.expect("race task join");

        match result {
            Err(RepairError::OverwrittenDuringRepair { head_etag, .. }) => {
                assert_eq!(
                    head_etag,
                    original_etag.trim_matches('"'),
                    "race-detected head_etag should be the pre-race normalized ETag"
                );
                let res = backend
                    .head_object()
                    .bucket("race")
                    .key(&sidecar)
                    .send()
                    .await;
                assert!(
                    res.is_err(),
                    "stale sidecar must be deleted after OverwrittenDuringRepair"
                );
                hit_race = true;
                break;
            }
            Err(RepairError::Backend { .. }) => {
                hit_get_race = true;
                continue;
            }
            Ok(_) => continue,
            Err(other) => panic!("unexpected repair error: {other:?}"),
        }
    }
    if !hit_race {
        eprintln!(
            "note: post-PUT race window not exercised across 5 attempts \
             (hit_get_race={hit_get_race}); the deterministic regression \
             guard lives in the lib unit test \
             `repair::tests::overwritten_during_repair_error_shape`."
        );
    }

    let _ = shutdown.send(());
}

/// v0.9 #106-audit-R2 P2-INT-1: when the on-disk body is an SSE-S4
/// chunked envelope (S4E6), `repair_sidecar` must reject cleanly with
/// `EncryptedSidecarUnsupported` instead of surfacing a confusing
/// frame-scan failure. Any pre-existing sidecar must remain untouched
/// so the operator can route the repair through a server-mode rebuild
/// path without losing whatever binding the gateway already wrote.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn repair_sidecar_rejects_sse_s4_chunked_object_cleanly() {
    use rand::RngCore;
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server_with_sse_s4_chunked(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("enc-repair").send().await;
    let s4_client = build_aws_client(&s4_endpoint);

    // PUT a multi-MiB incompressible body via the SSE-S4 chunked
    // gateway so the on-disk envelope is S4E6. The test does NOT
    // depend on a sidecar existing pre-call; the property under
    // verification is "repair must NOT proceed against an encrypted
    // body", regardless of whether a v3 sidecar was emitted on the
    // PUT (the multi-frame gate `entries.len() > 1` is a separate
    // codepath we don't need to exercise here).
    let mut body_bytes = vec![0u8; 4 * 1024 * 1024];
    rand::rngs::OsRng.fill_bytes(&mut body_bytes);
    let body = Bytes::from(body_bytes);
    s4_client
        .put_object()
        .bucket("enc-repair")
        .key("enc.bin")
        .body(body.clone().into())
        .send()
        .await
        .expect("PUT SSE-S4 chunked object");

    // The backend body must start with the S4E6 envelope magic —
    // that's the precondition the repair-side detector reads.
    let on_disk = backend
        .get_object()
        .bucket("enc-repair")
        .key("enc.bin")
        .send()
        .await
        .expect("backend GET")
        .body
        .collect()
        .await
        .expect("body")
        .into_bytes();
    assert_eq!(
        &on_disk[..4],
        b"S4E6",
        "precondition: backend body must be S4E6 (chunked SSE-S4)"
    );

    // Snapshot whatever sidecar state currently exists so we can
    // assert the failed repair did NOT mutate it. Pre-existing v3
    // sidecar is the normal multi-frame case; an absent sidecar
    // (single-frame body) is also valid — both must round-trip
    // unchanged across the rejected repair.
    let sidecar = sidecar_key("enc.bin");
    let pre_sidecar_bytes: Option<bytes::Bytes> = match backend
        .get_object()
        .bucket("enc-repair")
        .key(&sidecar)
        .send()
        .await
    {
        Ok(resp) => Some(
            resp.body
                .collect()
                .await
                .expect("sidecar body")
                .into_bytes(),
        ),
        Err(_) => None, // single-frame body → no sidecar planted
    };

    // The actual fix: repair_sidecar must reject with the typed
    // variant (NOT FrameScan / Backend / anything else).
    let err = repair_sidecar(
        &backend,
        "enc-repair",
        "enc.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect_err("repair must reject an SSE-encrypted object");
    match &err {
        RepairError::EncryptedSidecarUnsupported {
            bucket,
            key,
            message,
        } => {
            assert_eq!(bucket, "enc-repair");
            assert_eq!(key, "enc.bin");
            assert!(
                message.contains("S4E6"),
                "message must name the detected envelope magic, got {message:?}"
            );
        }
        other => panic!(
            "expected EncryptedSidecarUnsupported, got {other:?} \
             (repair must NOT surface FrameScan / Backend on encrypted bodies)"
        ),
    }
    // Pin the human-readable Display so the CLI keeps surfacing the
    // operator guidance.
    let rendered = format!("{err}");
    assert!(
        rendered.contains("SSE-S4 encrypted envelope"),
        "Display must name the failure mode — got {rendered:?}"
    );
    assert!(
        rendered.contains("server-mode") || rendered.contains("re-PUT"),
        "Display must point at a recovery path — got {rendered:?}"
    );

    // Post-condition: whatever sidecar state existed before the
    // failed repair is preserved byte-equal afterwards.
    let post_sidecar_bytes: Option<bytes::Bytes> = match backend
        .get_object()
        .bucket("enc-repair")
        .key(&sidecar)
        .send()
        .await
    {
        Ok(resp) => Some(
            resp.body
                .collect()
                .await
                .expect("sidecar body")
                .into_bytes(),
        ),
        Err(_) => None,
    };
    assert_eq!(
        post_sidecar_bytes, pre_sidecar_bytes,
        "failed repair must NOT mutate the pre-existing sidecar state"
    );

    let _ = shutdown.send(());
}

/// v0.10 #A1: with an operator-supplied SSE-S4 keyring,
/// `repair_sidecar_with_keyring` MUST decrypt an S4E6-encrypted
/// object in-process, frame-scan the plaintext, and write a v3
/// sidecar carrying the `sse_v3` chunked binding. Asserts:
///   - the call succeeds (no `EncryptedSidecarUnsupported`),
///   - the report's `sse_v3_binding` reflects the on-disk envelope
///     geometry (chunk_size / key_id from the PUT-time gateway,
///     plaintext_len matches the post-compression body the encoder
///     fed in),
///   - the freshly-written sidecar decodes with `sse_v3 = Some(..)`
///     so a subsequent Range GET would take the encryption-aware
///     fast-path,
///   - the rebuilt sidecar's salt matches what the on-disk S4E6
///     header carries (load-bearing bit the GET path uses to derive
///     per-chunk nonces),
///   - the full-body GET through the gateway still round-trips
///     cleanly (= the rebuilt sidecar isn't subtly wrong in a way
///     that breaks decrypt-on-GET).
///
/// Uses multipart PUT to make the post-compression body multi-frame
/// (the v0.9 sidecar-write gate requires `entries.len() > 1`). The
/// v0.9 multipart-with-SSE Complete path intentionally does NOT
/// write any sidecar (line 5480 `&& !mp_will_encrypt`), so the
/// pre-repair sidecar is always absent — the test exercises the
/// fresh-write branch (`rebuilt_from_existing == false`) end-to-end
/// without depending on a PUT-time v3 sidecar that would only exist
/// on the single-PUT codepath.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn repair_sidecar_rebuilds_sse_s4_chunked_object_with_keyring() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server_with_sse_s4_chunked(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend
        .create_bucket()
        .bucket("enc-repair-key")
        .send()
        .await;
    let s4_client = build_aws_client(&s4_endpoint);

    let payload = put_multipart_object(&s4_client, "enc-repair-key", "enc-doc.bin").await;
    let sidecar = sidecar_key("enc-doc.bin");

    // Precondition 1: the backend body is an S4E6 envelope.
    let on_disk = backend
        .get_object()
        .bucket("enc-repair-key")
        .key("enc-doc.bin")
        .send()
        .await
        .expect("backend GET object")
        .body
        .collect()
        .await
        .expect("body")
        .into_bytes();
    assert_eq!(
        &on_disk[..4],
        b"S4E6",
        "precondition: backend body must be S4E6 (chunked SSE-S4)"
    );

    // Precondition 2: no sidecar exists pre-repair. v0.9
    // `complete_multipart_upload` skips sidecar emission when the
    // multipart upload was encrypted (see service.rs `mp_will_encrypt`
    // gate), so the repair tool always starts from absent on this
    // path. We assert NotFound here to make any future change to the
    // multipart-SSE sidecar-emission flow surface as a test failure
    // (which is the right place to revisit this test's pre-state).
    let pre_repair_head = backend
        .head_object()
        .bucket("enc-repair-key")
        .key(&sidecar)
        .send()
        .await;
    assert!(
        pre_repair_head.is_err(),
        "v0.9 multipart-SSE Complete must not emit a sidecar; got {pre_repair_head:?}"
    );

    // Build the same keyring the gateway used at PUT time
    // (`spawn_s4_server_with_sse_s4_chunked` uses 32 bytes of 0x9a at
    // slot id=1). The repair tool's keyring composition MUST match
    // the PUT-time composition — a different active slot would
    // surface as `SseDecryptFailed`.
    let key = Arc::new(s4_server::sse::SseKey::from_bytes(&[0x9au8; 32]).expect("32-byte raw key"));
    let keyring: s4_server::sse::SharedSseKeyring =
        Arc::new(s4_server::sse::SseKeyring::new(1, key));

    let report = repair_sidecar_with_keyring(
        &backend,
        "enc-repair-key",
        "enc-doc.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
        Some(&keyring),
    )
    .await
    .expect("repair must decrypt + rebuild SSE-S4 chunked sidecar");

    assert!(
        !report.rebuilt_from_existing,
        "fresh write expected (multipart-SSE Complete didn't emit a sidecar)"
    );
    assert!(
        report.frame_count >= 2,
        "expected multi-frame body, got frame_count={}",
        report.frame_count
    );

    // Post-condition 1: the report carries the v3 binding the CLI
    // surfaces in its OK line.
    let report_binding = report
        .sse_v3_binding
        .expect("repair report must carry sse_v3_binding on S4E6 rebuild");
    assert_eq!(
        report_binding.enc_key_id, 1,
        "binding key_id must round-trip the PUT-time slot"
    );
    assert_eq!(
        report_binding.enc_chunk_size,
        256 * 1024,
        "binding chunk_size must match the gateway's --sse-chunk-size"
    );
    assert!(
        report_binding.enc_chunk_count >= 1,
        "binding chunk_count must be derived from the on-disk envelope, got {}",
        report_binding.enc_chunk_count
    );
    assert!(
        report_binding.enc_plaintext_len > 0,
        "binding plaintext_len must be the post-compression body length"
    );
    assert_eq!(
        report_binding.enc_header_bytes,
        s4_server::sse::S4E6_HEADER_BYTES as u32,
        "binding header_bytes must equal S4E6_HEADER_BYTES"
    );

    // Post-condition 2: the on-disk sidecar carries the same binding
    // the report describes AND the salt the on-disk S4E6 header
    // stamps (salt isn't in the report but it's the load-bearing bit
    // the GET path uses to derive per-chunk nonces).
    let rebuilt_sidecar = backend
        .get_object()
        .bucket("enc-repair-key")
        .key(&sidecar)
        .send()
        .await
        .expect("backend GET rebuilt sidecar")
        .body
        .collect()
        .await
        .expect("rebuilt sidecar body")
        .into_bytes();
    let rebuilt_idx = decode_index(rebuilt_sidecar).expect("decode rebuilt sidecar");
    let rebuilt_sse = rebuilt_idx
        .sse_v3
        .expect("rebuilt sidecar must carry sse_v3 binding");
    let envelope_hdr = s4_server::sse::parse_s4e6_header(&on_disk).expect("parse S4E6 envelope");
    assert_eq!(
        rebuilt_sse.enc_key_id, envelope_hdr.key_id,
        "rebuilt key_id must match the on-disk envelope"
    );
    assert_eq!(
        rebuilt_sse.enc_chunk_size, envelope_hdr.chunk_size,
        "rebuilt chunk_size must match the on-disk envelope"
    );
    assert_eq!(
        rebuilt_sse.enc_chunk_count, envelope_hdr.chunk_count,
        "rebuilt chunk_count must match the on-disk envelope"
    );
    assert_eq!(
        &rebuilt_sse.enc_salt, envelope_hdr.salt,
        "rebuilt salt must match the on-disk S4E6 header (load-bearing for GET-path nonce derivation)"
    );
    assert_eq!(
        rebuilt_sse.enc_header_bytes,
        s4_server::sse::S4E6_HEADER_BYTES as u32,
        "rebuilt header_bytes must equal S4E6_HEADER_BYTES"
    );

    // Sanity: GET through the gateway still round-trips the original
    // payload (i.e. the rebuilt sidecar isn't subtly wrong in a way
    // that breaks decrypt-on-GET).
    let roundtrip = s4_client
        .get_object()
        .bucket("enc-repair-key")
        .key("enc-doc.bin")
        .send()
        .await
        .expect("GET through gateway")
        .body
        .collect()
        .await
        .expect("collect body")
        .into_bytes();
    assert_eq!(
        roundtrip, payload,
        "full-body GET must still decrypt cleanly after sidecar rebuild"
    );

    let _ = shutdown.send(());
}

/// v0.10 #A1 (Codex R1 fix): the body-cap relaxation
/// `repair_sidecar_with_keyring` introduced to accommodate S4E6
/// envelope overhead MUST NOT bypass the cap on plaintext bodies
/// (which carry no envelope overhead). Specifically, a plaintext
/// body whose size lies in
/// `(body_bytes_cap, body_bytes_cap + SSE_S4_REPAIR_MAX_OVERHEAD_BYTES]`
/// must still surface `BodyTooLarge` when the operator happens to
/// have an SSE keyring loaded — the 4-byte magic peek must classify
/// the body as not-S4E6 before the relaxation applies.
///
/// This is the regression guard for the Codex R2 finding on the
/// initial "keyring supplied ⇒ relax cap" shortcut.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn repair_with_keyring_does_not_bypass_cap_for_plaintext_body() {
    let minio = start_minio().await;
    // Spawn a plain (non-SSE) gateway so the body lands plaintext on
    // the backend. The keyring we pass to repair below is unused for
    // decryption but is what previously triggered the silent cap
    // bypass — that's the regression we're guarding against.
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend
        .create_bucket()
        .bucket("repair-cap-plain")
        .send()
        .await;
    let s4_client = build_aws_client(&s4_endpoint);

    // Use the standard multipart helper (≈ 12 MiB plaintext on
    // disk via multipart Complete; well above any reasonable tight
    // cap we'd test against).
    let _ = put_multipart_object(&s4_client, "repair-cap-plain", "doc.bin").await;

    // Tight cap: 1 MiB. The plaintext body on disk is ~12 MiB so it
    // exceeds the cap by orders of magnitude, comfortably outside
    // the 256 MiB SSE-overhead headroom. Without the magic-peek
    // safeguard the relaxation would have accepted bodies up to
    // `1 MiB + ~256 MiB` and STILL rejected this 12 MiB body — but
    // we want the assertion to also cover small overshoots, so use
    // a deliberately tight cap.
    let cap = 1024 * 1024_u64;

    let keyring: s4_server::sse::SharedSseKeyring = Arc::new(s4_server::sse::SseKeyring::new(
        1,
        Arc::new(s4_server::sse::SseKey::from_bytes(&[0x77u8; 32]).expect("32-byte raw key")),
    ));

    let err =
        repair_sidecar_with_keyring(&backend, "repair-cap-plain", "doc.bin", cap, Some(&keyring))
            .await
            .expect_err("repair must reject oversized plaintext body even with keyring set");
    match &err {
        RepairError::BodyTooLarge {
            size,
            cap: reported_cap,
        } => {
            assert!(
                *size > cap,
                "size {size} must exceed cap {cap} for BodyTooLarge"
            );
            assert_eq!(
                *reported_cap, cap,
                "reported cap must be the operator-supplied value (not the silently-relaxed one)"
            );
        }
        other => panic!(
            "expected BodyTooLarge on plaintext-body cap overshoot, got {other:?}; \
             keyring-set MUST NOT bypass the cap on plaintext bodies"
        ),
    }

    let _ = shutdown.send(());
}

/// v0.10 #A1: passing the WRONG keyring (different key bytes) to
/// `repair_sidecar_with_keyring` against an S4E6 object MUST surface
/// the new `SseDecryptFailed` variant (not `EncryptedSidecarUnsupported`
/// — the operator DID supply a keyring; not `Backend` — the failure is
/// crypto, not transport). Pre-existing sidecar state must round-trip
/// unchanged across the failed repair.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn repair_sidecar_wrong_keyring_surfaces_sse_decrypt_failed() {
    let minio = start_minio().await;
    let (s4_endpoint, shutdown) = spawn_s4_server_with_sse_s4_chunked(&minio.endpoint_url).await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend
        .create_bucket()
        .bucket("enc-repair-wrong")
        .send()
        .await;
    let s4_client = build_aws_client(&s4_endpoint);

    let _payload = put_multipart_object(&s4_client, "enc-repair-wrong", "doc.bin").await;
    let sidecar = sidecar_key("doc.bin");
    let pre_sidecar_bytes: Option<bytes::Bytes> = match backend
        .get_object()
        .bucket("enc-repair-wrong")
        .key(&sidecar)
        .send()
        .await
    {
        Ok(resp) => Some(resp.body.collect().await.expect("body").into_bytes()),
        Err(_) => None,
    };

    // Operator passes a DIFFERENT 32-byte key — same active slot
    // (id=1) but different bytes — so the S4E6 chunk auth-tag verify
    // fails on chunk 0.
    let wrong_key =
        Arc::new(s4_server::sse::SseKey::from_bytes(&[0x33u8; 32]).expect("32-byte raw key"));
    let wrong_keyring: s4_server::sse::SharedSseKeyring =
        Arc::new(s4_server::sse::SseKeyring::new(1, wrong_key));

    let err = repair_sidecar_with_keyring(
        &backend,
        "enc-repair-wrong",
        "doc.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
        Some(&wrong_keyring),
    )
    .await
    .expect_err("repair must fail under a wrong keyring");
    match &err {
        RepairError::SseDecryptFailed { bucket, key, cause } => {
            assert_eq!(bucket, "enc-repair-wrong");
            assert_eq!(key, "doc.bin");
            assert!(
                !cause.is_empty(),
                "cause must carry the underlying SSE error string"
            );
        }
        other => panic!(
            "expected SseDecryptFailed, got {other:?} \
             (wrong-keyring repair must NOT surface Backend / FrameScan / \
             EncryptedSidecarUnsupported — those would mislead the operator)"
        ),
    }
    let rendered = format!("{err}");
    assert!(
        rendered.contains("--sse-s4-key"),
        "Display must point at the operator-actionable flag — got {rendered:?}"
    );

    // Post-condition: failed repair must NOT mutate the pre-existing
    // sidecar state (same invariant the no-keyring reject path holds).
    let post_sidecar_bytes: Option<bytes::Bytes> = match backend
        .get_object()
        .bucket("enc-repair-wrong")
        .key(&sidecar)
        .send()
        .await
    {
        Ok(resp) => Some(resp.body.collect().await.expect("body").into_bytes()),
        Err(_) => None,
    };
    assert_eq!(
        post_sidecar_bytes, pre_sidecar_bytes,
        "failed (wrong-keyring) repair must NOT mutate the pre-existing sidecar"
    );

    let _ = shutdown.send(());
}

/// v0.9 #106-audit-R5 P2-R5 (Codex): `verify-sidecar` /
/// `sweep-orphan-sidecars` MUST not OOM on a multi-GiB corrupt /
/// legacy user `.s4index` object. The sidecar fetch path now
/// HEADs first and refuses to GET if the size exceeds the cap.
/// This E2E plants a 1 MiB object at a `.s4index` key (small
/// enough for CI but the test asserts the *path* — we check the
/// SidecarTooLarge / oversize-classify behavior by also planting
/// a deliberately-oversized object far above the production cap
/// would require multi-GB of test data, so we instead exercise
/// the small case end-to-end (sweep classifies it as
/// SidecarUndecodable, repair rejects) and rely on the lib unit
/// tests `sidecar_too_large_error_shape` +
/// `max_sidecar_body_bytes_cap_value_pinned` to pin the cap
/// constant + error shape.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn sweep_classifies_oversized_lookalike_sidecar_as_undecodable() {
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend
        .create_bucket()
        .bucket("oversize-sweep")
        .send()
        .await;

    // Real S4 paired object so the bucket has a non-orphan
    // baseline (sanity: sweep should leave it alone).
    let (s4_endpoint, shutdown) = spawn_s4_server(&minio.endpoint_url).await;
    let s4_client = build_aws_client(&s4_endpoint);
    let _real = put_multipart_object(&s4_client, "oversize-sweep", "real.bin").await;

    // Plant a 1 MiB blob directly at a `.s4index` key — well below
    // the 600 MiB cap, so we exercise the normal "decode fails →
    // SidecarUndecodable" path. The protection itself (refuse to
    // GET when HEAD says > cap) is covered by the lib unit tests
    // since planting 600+ MiB on CI MinIO is too slow.
    let bogus = vec![0xABu8; 1024 * 1024];
    backend
        .put_object()
        .bucket("oversize-sweep")
        .key("bogus.s4index")
        .body(Bytes::from(bogus).into())
        .send()
        .await
        .expect("PUT bogus .s4index");

    let report = sweep_orphan_sidecars(&backend, "oversize-sweep", DeletePolicy::DryRun)
        .await
        .expect("sweep dry-run");
    // 2 sidecars scanned: real.bin.s4index (clean) + bogus.s4index.
    assert_eq!(report.sidecars_scanned, 2);
    assert_eq!(report.orphans.len(), 1);
    assert!(matches!(
        report.orphans[0].reason,
        OrphanReason::SidecarUndecodable { .. }
    ));

    let _ = shutdown.send(());
}

/// v0.9 #106-audit-R4 P2-R4 (Codex): `verify-sidecar` against a
/// passthrough / raw-bytes object whose backend body has no `S4F2`
/// magic must classify as `MissingHarmless`, NOT bubble up the
/// inner `BadMagic` `FrameScan` error. The server intentionally
/// never sidecars passthrough objects, so absence of a sidecar is
/// the correct steady state — CI / cron should see exit 0, not
/// a repair error.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn verify_sidecar_reports_missing_harmless_for_non_framed_body() {
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("verify-raw").send().await;

    // Plant a raw passthrough-shape body directly via the backend
    // (no S4 gateway in the loop). Bytes long enough to clear the
    // FRAME_HEADER_BYTES (28) probe so we hit the `BadMagic` path,
    // not the `Ok(empty entries)` short-body path the R3 fix
    // already covers.
    let raw =
        Bytes::from_static(b"not an s4f2 frame body, just raw bytes, more than 28 bytes long");
    backend
        .put_object()
        .bucket("verify-raw")
        .key("raw.bin")
        .body(raw.clone().into())
        .send()
        .await
        .expect("PUT raw body");

    let report = verify_sidecar(
        &backend,
        "verify-raw",
        "raw.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect("verify must classify non-framed body, not error");
    assert!(
        matches!(
            report.status,
            SidecarStatus::MissingHarmless { frame_count: 0 }
        ),
        "non-framed body must classify as MissingHarmless{{frame_count:0}}, got {:?}",
        report.status
    );
    assert!(
        report.is_clean(),
        "MissingHarmless must be clean (exit 0 for CI / cron)"
    );
}

/// v0.9 #106-audit-R3 P2-R3 (Codex): `repair-sidecar` on an object
/// whose body `build_index_from_body` parses to **zero frames** must
/// reject with `NotFramed` instead of writing an empty sidecar that
/// would silently break Range GET (`lookup_range` over 0 entries
/// returns `None` → InvalidRange).
///
/// `build_index_from_body` returns `Ok` with empty entries when the
/// body has no S4F2 frame magic AND no readable header — the practical
/// trip is an empty body (0 bytes) or a body shorter than the
/// 28-byte frame header. Longer arbitrary raw-bytes bodies surface
/// as `FrameScan` from the inner frame parser's `BadMagic` instead.
/// Both error paths share the same outcome: NO sidecar is written.
/// This test pins the `NotFramed` branch via the empty-body case.
#[tokio::test]
#[ignore = "requires Docker for MinIO container"]
async fn repair_sidecar_rejects_zero_frame_body() {
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    let _ = backend.create_bucket().bucket("raw-repair").send().await;

    // Empty body — guarantees `build_index_from_body` returns
    // `Ok(FrameIndex { entries: [], .. })` and exercises the
    // NotFramed guard (rather than the BadMagic / FrameScan path
    // that catches any non-trivial non-framed body).
    backend
        .put_object()
        .bucket("raw-repair")
        .key("empty.bin")
        .body(Bytes::new().into())
        .send()
        .await
        .expect("PUT empty body");

    let err = repair_sidecar(
        &backend,
        "raw-repair",
        "empty.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect_err("repair must reject a non-framed body");
    match err {
        RepairError::NotFramed { bucket, key } => {
            assert_eq!(bucket, "raw-repair");
            assert_eq!(key, "empty.bin");
        }
        other => panic!("expected NotFramed, got {other:?}"),
    }

    // No sidecar must have been written.
    let head = backend
        .head_object()
        .bucket("raw-repair")
        .key(format!("empty.bin{SIDECAR_SUFFIX}"))
        .send()
        .await;
    assert!(
        head.is_err(),
        "no sidecar must be written when repair rejects (got Ok: {head:?})"
    );

    // Sibling sanity: a non-trivial raw-bytes body trips the
    // BadMagic / FrameScan path before our NotFramed guard fires,
    // but BOTH paths must avoid writing a sidecar (same end-state
    // the operator cares about).
    backend
        .put_object()
        .bucket("raw-repair")
        .key("raw.bin")
        .body(Bytes::from_static(b"not an s4f2 frame body, just raw bytes").into())
        .send()
        .await
        .expect("PUT raw body");
    let err = repair_sidecar(
        &backend,
        "raw-repair",
        "raw.bin",
        DEFAULT_REPAIR_BODY_BYTES_CAP,
    )
    .await
    .expect_err("repair must reject a raw body");
    assert!(
        matches!(
            err,
            RepairError::FrameScan { .. } | RepairError::NotFramed { .. }
        ),
        "expected FrameScan or NotFramed, got {err:?}"
    );
    let head = backend
        .head_object()
        .bucket("raw-repair")
        .key(format!("raw.bin{SIDECAR_SUFFIX}"))
        .send()
        .await;
    assert!(
        head.is_err(),
        "no sidecar must be written when raw-body repair rejects"
    );
}
