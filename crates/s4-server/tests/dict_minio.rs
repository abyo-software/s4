//! v1.1 `--zstd-dict` E2E against a real MinIO container.
//!
//! Docker required, so gated with `#[ignore]` exactly like
//! `migrate_minio.rs` / `estimate_minio.rs`:
//!
//! ```bash
//! cargo test --test dict_minio -- --ignored --nocapture
//! ```
//!
//! Covered acceptance criteria:
//! (a) `train-dict` (library form, `run_train_dict`) trains from raw
//!     small objects and writes `.s4dict/<dict-id>` (re-run idempotent);
//! (b) a gateway booted with the dict mapping compresses small JSON PUTs
//!     under the prefix measurably smaller than the same bodies under a
//!     non-matching prefix (dict-less cpu-zstd), and stamps
//!     `s4-codec: cpu-zstd-dict` + `s4-dict-id`;
//! (c) GET through the dict-configured gateway round-trips;
//! (d) GET through a gateway booted WITHOUT any `--zstd-dict` flag also
//!     round-trips (lazy `.s4dict/<id>` fetch + LRU);
//! (e) `.s4dict/` keys are hidden from gateway listings;
//! (f) the stored frame payload decodes with the plain `zstd` crate given
//!     the dictionary bytes — and with the `zstd` CLI when present on the
//!     host (`zstd -D <dictfile> -d`) — proving the no-gateway escape
//!     hatch documented in the README.
//!
//! v1.0.1 audit R1 additions:
//! (g) client-supplied `x-amz-meta-s4-dict-id` on a normal PUT is
//!     stripped (never stored) and the GET succeeds exactly like v1.0.0 —
//!     no dictionary fetch is attempted (P1 freeze fix);
//! (h) gateway-side PUT / DELETE against `.s4dict/<id>` keys are
//!     rejected with `InvalidObjectName` (dictionary objects are
//!     gateway-managed; `train-dict` writes backend-direct);
//! (i) a cross-bucket CopyObject of a dict-stamped object carries
//!     `.s4dict/<id>` (with its full-SHA-256 metadata stamp) into the
//!     destination bucket, so the copy stays readable after the source
//!     bucket is gone (idempotent re-copy included).
//!
//! v1.0.1 audit R2 addition (separate test,
//! `dict_e2e_versioned_copy_pins_source_version`):
//! (j) on a MinIO-versioned source bucket, a `?versionId=`-pinned
//!     CopyObject probes the *pinned* version for both the
//!     REPLACE-directive metadata merge and the cross-bucket dictionary
//!     propagation (pre-fix both probed "latest").

use std::collections::HashMap;
use std::sync::Arc;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
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
use s4_server::dict::{
    DEFAULT_DICT_MAX_OBJECT_BYTES, DictStore, TrainDictParams, dict_object_key,
    parse_zstd_dict_flag, run_train_dict,
};
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const MINIO_USER: &str = "minioadmin";
const MINIO_PASS: &str = "minioadmin";
const BUCKET: &str = "dictbkt";

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

/// Spawn a real S4 gateway in front of MinIO, optionally with a dict
/// store attached (same shape as `migrate_minio.rs::spawn_s4_server`).
async fn spawn_s4_server(
    backend_endpoint: &str,
    dicts: Option<Arc<DictStore>>,
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
    if let Some(store) = dicts {
        s4 = s4.with_zstd_dicts(store);
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

/// Homogeneous small JSON event bodies — the workload shared dictionaries
/// exist for. `salt` decorrelates the train / live sets.
fn json_event(i: u32, salt: &str) -> Vec<u8> {
    format!(
        "{{\"timestamp\":\"2026-06-10T{:02}:{:02}:{:02}Z\",\"level\":\"info\",\
         \"service\":\"checkout-api\",\"event\":\"order_created\",\
         \"order_id\":\"ord_{salt}{i:08}\",\"customer_id\":\"cus_{:08}\",\
         \"amount_cents\":{},\"currency\":\"USD\",\"region\":\"ap-northeast-1\",\
         \"items\":{},\"payment\":\"card\"}}",
        i % 24,
        (i * 3) % 60,
        (i * 7) % 60,
        i * 31 % 10_000_000,
        100 + i * 13,
        i % 9
    )
    .into_bytes()
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

async fn get_via(client: &aws_sdk_s3::Client, key: &str) -> Vec<u8> {
    client
        .get_object()
        .bucket(BUCKET)
        .key(key)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {key}: {e}"))
        .body
        .collect()
        .await
        .expect("collect body")
        .into_bytes()
        .to_vec()
}

/// Sum of backend (= stored / compressed) object sizes under a prefix.
async fn backend_size_sum(client: &aws_sdk_s3::Client, prefix: &str) -> (u64, usize) {
    let resp = client
        .list_objects_v2()
        .bucket(BUCKET)
        .prefix(prefix)
        .send()
        .await
        .expect("backend list");
    let mut total = 0u64;
    let mut count = 0usize;
    for obj in resp.contents() {
        total += obj.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0);
        count += 1;
    }
    (total, count)
}

#[tokio::test]
#[ignore = "requires Docker (MinIO testcontainer); run with --ignored"]
async fn dict_e2e_train_compress_get_and_external_decode() {
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    backend
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("create bucket");

    // ---- seed a raw training corpus straight on the backend ----------
    const TRAIN_N: u32 = 80;
    for i in 0..TRAIN_N {
        backend
            .put_object()
            .bucket(BUCKET)
            .key(format!("events/train/{i:04}.json"))
            .body(aws_sdk_s3::primitives::ByteStream::from(json_event(i, "t")))
            .send()
            .await
            .expect("seed train object");
    }

    // ---- (a) train ----------------------------------------------------
    let params = TrainDictParams {
        prefix: "events/".to_owned(),
        max_samples: 1000,
        max_dict_bytes: 112_640,
        min_samples: 8,
        sample_max_bytes: 64 * 1024,
        zstd_level: CpuZstd::DEFAULT_LEVEL,
    };
    let report = run_train_dict(&backend, BUCKET, &params)
        .await
        .expect("train-dict");
    assert_eq!(report.sampled_objects as u32, TRAIN_N);
    assert!(s4_server::dict::is_valid_dict_id(&report.dict_id));
    assert!(!report.dict_already_existed);
    assert!(
        report.gateway_flag.contains(&report.dict_id),
        "flag output: {}",
        report.gateway_flag
    );
    // Dictionary object exists on the backend with the exact bytes.
    let dict_bytes = get_via(&backend, &dict_object_key(&report.dict_id)).await;
    assert_eq!(dict_bytes.len(), report.dict_bytes);
    assert_eq!(s4_server::dict::dict_id_of(&dict_bytes), report.dict_id);

    // Idempotent re-train on the unchanged corpus: same id, no conflict.
    let report2 = run_train_dict(&backend, BUCKET, &params)
        .await
        .expect("re-train");
    assert_eq!(report2.dict_id, report.dict_id);
    assert!(report2.dict_already_existed);

    // ---- (b) gateway with the dict mapping -----------------------------
    let entry =
        parse_zstd_dict_flag(&format!("{BUCKET}/events/={}", report.dict_id)).expect("flag parse");
    let mut loaded = HashMap::new();
    loaded.insert(report.dict_id.clone(), dict_bytes.clone());
    let store = Arc::new(
        DictStore::new(
            vec![entry],
            loaded,
            DEFAULT_DICT_MAX_OBJECT_BYTES,
            CpuZstd::DEFAULT_LEVEL,
        )
        .expect("store"),
    );
    let (gw_url, gw_stop) = spawn_s4_server(&minio.endpoint_url, Some(store)).await;
    let gw = build_aws_client(&gw_url);

    const LIVE_N: u32 = 100;
    let mut bodies: Vec<(String, String, Vec<u8>)> = Vec::new();
    for i in 0..LIVE_N {
        let body = json_event(10_000 + i, "x");
        bodies.push((
            format!("events/new/{i:04}.json"),
            format!("control/{i:04}.json"),
            body,
        ));
    }
    for (dict_key, control_key, body) in &bodies {
        put_via(&gw, dict_key, body.clone()).await;
        put_via(&gw, control_key, body.clone()).await;
    }

    // Stored (backend) sizes: dict prefix must genuinely shrink vs the
    // dict-less control prefix carrying the identical bodies.
    let (dict_total, dict_count) = backend_size_sum(&backend, "events/new/").await;
    let (control_total, control_count) = backend_size_sum(&backend, "control/").await;
    assert_eq!(dict_count as u32, LIVE_N);
    assert_eq!(control_count as u32, LIVE_N);
    assert!(
        dict_total < control_total,
        "dict-compressed total ({dict_total}) must beat dict-less total ({control_total})"
    );
    println!(
        "stored bytes: dict={dict_total} control={control_total} \
         ({}% of dict-less size)",
        dict_total * 100 / control_total
    );

    // Metadata stamps on a dict object.
    let head = backend
        .head_object()
        .bucket(BUCKET)
        .key(&bodies[0].0)
        .send()
        .await
        .expect("backend HEAD");
    let meta = head.metadata().expect("metadata");
    assert_eq!(
        meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd-dict")
    );
    assert_eq!(
        meta.get("s4-dict-id").map(String::as_str),
        Some(report.dict_id.as_str())
    );
    assert_eq!(meta.get("s4-framed").map(String::as_str), Some("true"));

    // ---- (c) GET round-trip through the dict gateway --------------------
    for (dict_key, control_key, body) in &bodies {
        assert_eq!(&get_via(&gw, dict_key).await, body, "{dict_key}");
        assert_eq!(&get_via(&gw, control_key).await, body, "{control_key}");
    }

    // ---- (e) `.s4dict/` hidden from gateway listings --------------------
    let listed = gw
        .list_objects_v2()
        .bucket(BUCKET)
        .send()
        .await
        .expect("gateway list");
    for obj in listed.contents() {
        let k = obj.key().unwrap_or("");
        assert!(
            !k.starts_with(".s4dict/"),
            "gateway listing must hide dictionary objects, saw {k}"
        );
    }
    // ...but the backend (truth) still has it.
    let (dict_obj_total, dict_obj_count) = backend_size_sum(&backend, ".s4dict/").await;
    assert_eq!(dict_obj_count, 1);
    assert!(dict_obj_total > 0);

    let _ = gw_stop.send(());

    // ---- (d) flag-less gateway still reads via lazy fetch ---------------
    let (plain_url, plain_stop) = spawn_s4_server(&minio.endpoint_url, None).await;
    let plain_gw = build_aws_client(&plain_url);
    for (dict_key, _, body) in bodies.iter().take(10) {
        assert_eq!(
            &get_via(&plain_gw, dict_key).await,
            body,
            "lazy-fetch GET must round-trip without --zstd-dict: {dict_key}"
        );
    }
    let _ = plain_stop.send(());

    // ---- (f) external decode: no gateway involved ------------------------
    // Stored layout: one S4F2 frame; payload = stock zstd frame that
    // references the dictionary.
    let stored = get_via(&backend, &bodies[0].0).await;
    let (header, payload, rest) =
        s4_codec::multipart::read_frame(Bytes::from(stored)).expect("frame parse");
    assert_eq!(header.codec, CodecKind::CpuZstdDict);
    assert!(rest.is_empty(), "small object must be a single frame");

    // f-1: plain zstd crate (no S4 types in the decode path).
    {
        use std::io::Read;
        let mut decoder =
            zstd::stream::read::Decoder::with_dictionary(payload.as_ref(), &dict_bytes)
                .expect("decoder");
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("zstd crate decode");
        assert_eq!(out, bodies[0].2);
    }

    // f-2: zstd CLI when available on the host (`zstd -D <dict> -d`).
    let cli_available = std::process::Command::new("zstd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if cli_available {
        let dir = tempfile::tempdir().expect("tempdir");
        let dict_path = dir.path().join("dict.bin");
        let frame_path = dir.path().join("payload.zst");
        let out_path = dir.path().join("decoded.json");
        std::fs::write(&dict_path, &dict_bytes).expect("write dict");
        std::fs::write(&frame_path, payload.as_ref()).expect("write payload");
        let status = std::process::Command::new("zstd")
            .arg("-D")
            .arg(&dict_path)
            .arg("-d")
            .arg("-f")
            .arg(&frame_path)
            .arg("-o")
            .arg(&out_path)
            .status()
            .expect("run zstd CLI");
        assert!(status.success(), "zstd CLI decode failed");
        let decoded = std::fs::read(&out_path).expect("read decoded");
        assert_eq!(decoded, bodies[0].2, "zstd CLI output must match original");
        println!("zstd CLI external-decode recipe verified");
    } else {
        println!("zstd CLI not found on host — crate-level external decode verified only");
    }

    // =====================================================================
    // v1.0.1 audit R1 sections (g) / (h) / (i) — fresh flag-less gateway.
    // =====================================================================
    let (gw3_url, gw3_stop) = spawn_s4_server(&minio.endpoint_url, None).await;
    let gw3 = build_aws_client(&gw3_url);

    // ---- (g) client-supplied `s4-dict-id` metadata is inert --------------
    // P1 freeze fix: pre-fix, this PUT stored the metadata verbatim and
    // the GET routed into the dictionary path → 5xx (`.s4dict/0123...`
    // doesn't exist). Post-fix the reserved key is stripped on PUT and
    // the GET path additionally requires `s4-codec: cpu-zstd-dict`.
    let evil_body = json_event(424_242, "meta").repeat(4);
    gw3.put_object()
        .bucket(BUCKET)
        .key("plainmeta/normal.json")
        .metadata("s4-dict-id", "0123456789abcdef")
        .metadata("s4-codec", "cpu-zstd-dict")
        .metadata("app-team", "checkout")
        .body(aws_sdk_s3::primitives::ByteStream::from(evil_body.clone()))
        .send()
        .await
        .expect("PUT with client-supplied s4-* metadata must succeed");
    assert_eq!(
        get_via(&gw3, "plainmeta/normal.json").await,
        evil_body,
        "GET must succeed like v1.0.0 — no dict fetch for a normal object"
    );
    let head = backend
        .head_object()
        .bucket(BUCKET)
        .key("plainmeta/normal.json")
        .send()
        .await
        .expect("backend HEAD");
    let meta = head.metadata().expect("metadata");
    assert!(
        meta.get("s4-dict-id").is_none(),
        "client-supplied s4-dict-id must be stripped, got {meta:?}"
    );
    assert_eq!(
        meta.get("s4-codec").map(String::as_str),
        Some("cpu-zstd"),
        "s4-codec must be the gateway's own stamp, not the client's forgery"
    );
    assert_eq!(
        meta.get("app-team").map(String::as_str),
        Some("checkout"),
        "non-reserved client metadata must survive"
    );

    // ---- (h) `.s4dict/` is write-protected through the gateway -----------
    let dict_key = dict_object_key(&report.dict_id);
    gw3.put_object()
        .bucket(BUCKET)
        .key(&dict_key)
        .body(aws_sdk_s3::primitives::ByteStream::from(vec![0u8; 32]))
        .send()
        .await
        .expect_err("gateway PUT over a dictionary object must be rejected");
    gw3.delete_object()
        .bucket(BUCKET)
        .key(&dict_key)
        .send()
        .await
        .expect_err("gateway DELETE of a dictionary object must be rejected");
    backend
        .head_object()
        .bucket(BUCKET)
        .key(&dict_key)
        .send()
        .await
        .expect("dictionary object must survive the rejected mutations");

    // ---- (i) cross-bucket CopyObject carries the dictionary --------------
    const BUCKET2: &str = "dictbkt2";
    backend
        .create_bucket()
        .bucket(BUCKET2)
        .send()
        .await
        .expect("create second bucket");
    gw3.copy_object()
        .copy_source(format!("{BUCKET}/{}", bodies[0].0))
        .bucket(BUCKET2)
        .key("copied/0000.json")
        .send()
        .await
        .expect("cross-bucket copy of a dict-stamped object");
    // The dictionary travelled with the copy (content-addressed PUT into
    // the destination bucket, full-SHA-256 metadata stamped).
    let dict_head = backend
        .head_object()
        .bucket(BUCKET2)
        .key(&dict_key)
        .send()
        .await
        .expect("destination bucket must now hold .s4dict/<id>");
    assert_eq!(
        dict_head
            .metadata()
            .and_then(|m| m.get("s4-dict-sha256"))
            .map(String::as_str),
        Some(s4_server::dict::dict_sha256_hex(&dict_bytes).as_str()),
        "propagated dict must carry the full-SHA-256 metadata stamp"
    );
    let dict_copy = backend
        .get_object()
        .bucket(BUCKET2)
        .key(&dict_key)
        .send()
        .await
        .expect("GET propagated dict")
        .body
        .collect()
        .await
        .expect("collect propagated dict")
        .into_bytes();
    assert_eq!(
        dict_copy.as_ref(),
        dict_bytes.as_slice(),
        "propagated dictionary bytes must be identical (content-addressed)"
    );
    // The copied object round-trips out of the destination bucket via a
    // flag-less gateway (lazy fetch now resolves from BUCKET2 itself).
    let copied = gw3
        .get_object()
        .bucket(BUCKET2)
        .key("copied/0000.json")
        .send()
        .await
        .expect("GET copied object")
        .body
        .collect()
        .await
        .expect("collect copied object")
        .into_bytes();
    assert_eq!(copied.as_ref(), bodies[0].2.as_slice());
    // Idempotency: a second cross-bucket copy hits the existing
    // `.s4dict/<id>` (HEAD-skip) and must not fail.
    gw3.copy_object()
        .copy_source(format!("{BUCKET}/{}", bodies[1].0))
        .bucket(BUCKET2)
        .key("copied/0001.json")
        .send()
        .await
        .expect("second cross-bucket copy (idempotent dict propagation)");

    let _ = gw3_stop.send(());
}

/// v1.0.1 audit R2 P2: CopyObject's two source HEAD probes (the
/// REPLACE-directive metadata merge and the cross-bucket dictionary
/// propagation) must honor a `?versionId=`-pinned copy source. Pre-fix
/// both probed "latest" — this test makes "latest" maximally wrong (a raw
/// backend-direct overwrite with no `s4-*` metadata at all) and pins the
/// copy to the older, dict-compressed version:
///
/// - cross-bucket COPY-directive copy: the pinned version's
///   `.s4dict/<id>` must travel to the destination bucket (pre-fix: the
///   probe saw the raw latest, found no `s4-dict-id`, skipped
///   propagation → dangling dict reference, destination GET 5xx);
/// - REPLACE-directive copy: the destination must carry the *pinned*
///   version's s4-* manifest (pre-fix: the merge read latest's metadata
///   — here none — so the destination lost its codec markers and a GET
///   returned compressed bytes instead of the original body).
#[tokio::test]
#[ignore = "requires Docker (MinIO testcontainer); run with --ignored"]
async fn dict_e2e_versioned_copy_pins_source_version() {
    const SRC: &str = "verbkt";
    const DST: &str = "verbkt2";
    const KEY: &str = "events/v/pinned.json";

    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    backend
        .create_bucket()
        .bucket(SRC)
        .send()
        .await
        .expect("create src bucket");
    backend
        .create_bucket()
        .bucket(DST)
        .send()
        .await
        .expect("create dst bucket");
    backend
        .put_bucket_versioning()
        .bucket(SRC)
        .versioning_configuration(
            aws_sdk_s3::types::VersioningConfiguration::builder()
                .status(aws_sdk_s3::types::BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await
        .expect("enable MinIO versioning on the source bucket");

    // Train a dictionary on the source bucket (backend-direct, same
    // corpus shape as the main e2e).
    for i in 0..80u32 {
        backend
            .put_object()
            .bucket(SRC)
            .key(format!("events/train/{i:04}.json"))
            .body(aws_sdk_s3::primitives::ByteStream::from(json_event(i, "t")))
            .send()
            .await
            .expect("seed train object");
    }
    let params = TrainDictParams {
        prefix: "events/".to_owned(),
        max_samples: 1000,
        max_dict_bytes: 112_640,
        min_samples: 8,
        sample_max_bytes: 64 * 1024,
        zstd_level: CpuZstd::DEFAULT_LEVEL,
    };
    let report = run_train_dict(&backend, SRC, &params)
        .await
        .expect("train-dict");
    let dict_key = dict_object_key(&report.dict_id);
    let dict_bytes = backend
        .get_object()
        .bucket(SRC)
        .key(&dict_key)
        .send()
        .await
        .expect("GET trained dict")
        .body
        .collect()
        .await
        .expect("collect dict")
        .into_bytes()
        .to_vec();

    // Gateway with the dict mapping attached.
    let entry =
        parse_zstd_dict_flag(&format!("{SRC}/events/={}", report.dict_id)).expect("flag parse");
    let mut loaded = HashMap::new();
    loaded.insert(report.dict_id.clone(), dict_bytes);
    let store = Arc::new(
        DictStore::new(
            vec![entry],
            loaded,
            DEFAULT_DICT_MAX_OBJECT_BYTES,
            CpuZstd::DEFAULT_LEVEL,
        )
        .expect("store"),
    );
    let (gw_url, gw_stop) = spawn_s4_server(&minio.endpoint_url, Some(store)).await;
    let gw = build_aws_client(&gw_url);

    // Version A through the gateway: dict-compressed, `s4-dict-id` stamped.
    let body_a = json_event(777_777, "vA");
    let put_a = gw
        .put_object()
        .bucket(SRC)
        .key(KEY)
        .body(aws_sdk_s3::primitives::ByteStream::from(body_a.clone()))
        .send()
        .await
        .expect("PUT version A via gateway");
    let vid_a = put_a
        .version_id()
        .expect("versioned MinIO bucket must mint a version id")
        .to_owned();
    let head_a = backend
        .head_object()
        .bucket(SRC)
        .key(KEY)
        .version_id(&vid_a)
        .send()
        .await
        .expect("backend HEAD version A");
    assert_eq!(
        head_a
            .metadata()
            .and_then(|m| m.get("s4-dict-id"))
            .map(String::as_str),
        Some(report.dict_id.as_str()),
        "precondition: version A must be dict-compressed"
    );

    // Version B straight on the backend: raw bytes, NO s4-* metadata —
    // the worst case for a probe that incorrectly reads "latest".
    let body_b = b"latest version is a raw backend-direct overwrite".to_vec();
    backend
        .put_object()
        .bucket(SRC)
        .key(KEY)
        .body(aws_sdk_s3::primitives::ByteStream::from(body_b))
        .send()
        .await
        .expect("PUT version B backend-direct");

    // ---- cross-bucket COPY-directive copy pinned to version A ----------
    gw.copy_object()
        .copy_source(format!("{SRC}/{KEY}?versionId={vid_a}"))
        .bucket(DST)
        .key("copied/pinned.json")
        .send()
        .await
        .expect("pinned cross-bucket copy");
    backend
        .head_object()
        .bucket(DST)
        .key(&dict_key)
        .send()
        .await
        .expect(
            "pinned version's dictionary must travel to the destination bucket \
             (pre-fix the probe saw the raw latest version and skipped propagation)",
        );
    // The copied object round-trips through a flag-less gateway (lazy
    // `.s4dict/` fetch resolves from DST itself).
    let (plain_url, plain_stop) = spawn_s4_server(&minio.endpoint_url, None).await;
    let plain = build_aws_client(&plain_url);
    let copied = plain
        .get_object()
        .bucket(DST)
        .key("copied/pinned.json")
        .send()
        .await
        .expect("GET copied object via flag-less gateway")
        .body
        .collect()
        .await
        .expect("collect copied object")
        .into_bytes();
    assert_eq!(
        copied.as_ref(),
        body_a.as_slice(),
        "pinned copy must carry version A's bytes and stay readable"
    );

    // ---- REPLACE-directive copy pinned to version A ---------------------
    gw.copy_object()
        .copy_source(format!("{SRC}/{KEY}?versionId={vid_a}"))
        .bucket(DST)
        .key("replaced/pinned.json")
        .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace)
        .metadata("app-team", "checkout")
        .send()
        .await
        .expect("pinned REPLACE-directive copy");
    let head = backend
        .head_object()
        .bucket(DST)
        .key("replaced/pinned.json")
        .send()
        .await
        .expect("backend HEAD REPLACE destination");
    let meta = head.metadata().expect("metadata");
    assert_eq!(
        meta.get("s4-dict-id").map(String::as_str),
        Some(report.dict_id.as_str()),
        "REPLACE merge must propagate the PINNED version's s4-* manifest, got {meta:?}"
    );
    assert_eq!(
        meta.get("app-team").map(String::as_str),
        Some("checkout"),
        "client REPLACE metadata must survive the merge"
    );
    let replaced = plain
        .get_object()
        .bucket(DST)
        .key("replaced/pinned.json")
        .send()
        .await
        .expect("GET REPLACE destination via flag-less gateway")
        .body
        .collect()
        .await
        .expect("collect REPLACE destination")
        .into_bytes();
    assert_eq!(
        replaced.as_ref(),
        body_a.as_slice(),
        "REPLACE destination must decode with version A's manifest"
    );

    let _ = plain_stop.send(());
    let _ = gw_stop.send(());
}
