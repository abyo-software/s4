//! v1.3 dict ops E2E against a real MinIO container AND the real `s4`
//! binary (the per-prefix metrics, `s4 dict-status` and the
//! `--zstd-dict-map` + SIGHUP reload all live in `main.rs`, so a
//! library-embedded gateway can't cover them).
//!
//! Docker required, so gated with `#[ignore]` exactly like
//! `dict_minio.rs` / `migrate_minio.rs`:
//!
//! ```bash
//! cargo test --test dict_ops_minio -- --ignored --nocapture
//! ```
//!
//! Covered acceptance criteria:
//! (a) a dict-enabled gateway PUTting N same-schema JSON objects exposes
//!     `s4_dict_put_total{prefix,outcome="win"}` +
//!     `s4_dict_put_bytes_total{prefix,kind}` on `/metrics`, and the real
//!     `s4 dict-status` binary reports the win rate / compression ratio
//!     and exits 0;
//! (b) a prefix mapped to a mismatched dictionary (random bodies) drives
//!     its win rate to ~0 → `s4 dict-status` exits 1 with the
//!     "dictionary may be stale; consider retraining (s4 train-dict)"
//!     warning;
//! (c) a gateway booted from `--zstd-dict-map <FILE>` picks up a NEW
//!     prefix→dict mapping appended to the file via `kill -HUP` —
//!     **without a restart** the next PUT under the new prefix is
//!     dict-compressed (backend metadata `s4-codec: cpu-zstd-dict`) and
//!     `s4_dict_reload_total{result="ok"}` is bumped; a subsequent
//!     SIGHUP pointing the map at a nonexistent dict-id FAILS the reload
//!     (`result="err"`) and the previous configuration keeps serving.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use s4_codec::cpu_zstd::CpuZstd;
use s4_server::dict::{TrainDictParams, parse_prom_sample, run_train_dict};
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

/// Real-binary gateway guard — kills the child on drop so a failing
/// assertion never leaks an `s4` process.
struct BinGateway {
    child: Child,
    endpoint_url: String,
    metrics_url: String,
}

impl Drop for BinGateway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Spawn the real `s4` binary as a gateway in front of MinIO and wait
/// for `/ready`. `extra_args` carries the dict flags under test.
async fn spawn_bin_gateway(backend_endpoint: &str, extra_args: &[String]) -> BinGateway {
    let port = free_port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_s4"));
    cmd.arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--endpoint-url")
        .arg(backend_endpoint)
        .args(extra_args)
        .env("AWS_ACCESS_KEY_ID", MINIO_USER)
        .env("AWS_SECRET_ACCESS_KEY", MINIO_PASS)
        .env("AWS_REGION", "us-east-1")
        // Null sinks: an undrained pipe could fill and wedge the
        // gateway under per-request logging.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn s4 gateway binary");
    let endpoint_url = format!("http://127.0.0.1:{port}");
    let metrics_url = format!("{endpoint_url}/metrics");
    let gw = BinGateway {
        child,
        endpoint_url,
        metrics_url,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(resp) = reqwest::get(format!("{}/ready", gw.endpoint_url)).await
            && resp.status().is_success()
        {
            return gw;
        }
        assert!(
            Instant::now() < deadline,
            "gateway did not become ready within 30s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn scrape_metrics(metrics_url: &str) -> String {
    let resp = reqwest::get(metrics_url).await.expect("GET /metrics");
    assert!(
        resp.status().is_success(),
        "/metrics status {}",
        resp.status()
    );
    resp.text().await.expect("metrics body")
}

/// Sum every sample of `metric` whose labels include all of `want`.
fn metric_sum(metrics_text: &str, metric: &str, want: &[(&str, &str)]) -> f64 {
    metrics_text
        .lines()
        .filter_map(parse_prom_sample)
        .filter(|(name, labels, _)| {
            name == metric
                && want
                    .iter()
                    .all(|(k, v)| labels.iter().any(|(ln, lv)| ln == k && lv == v))
        })
        .map(|(_, _, value)| value)
        .sum()
}

/// Run the real `s4 dict-status` binary; returns (exit_ok, stdout, stderr).
fn run_dict_status(metrics_url: &str, extra: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_s4"))
        .arg("dict-status")
        .arg("--metrics-url")
        .arg(metrics_url)
        .args(extra)
        .output()
        .expect("run s4 dict-status");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn send_sighup(child_id: u32) {
    let status = Command::new("kill")
        .arg("-HUP")
        .arg(child_id.to_string())
        .status()
        .expect("send SIGHUP");
    assert!(status.success(), "kill -HUP must succeed");
}

/// Homogeneous small JSON event bodies (same schema as `dict_minio.rs`).
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

/// A second, deliberately different JSON schema (per-host metric lines)
/// so its prefix genuinely needs its own dictionary.
fn metric_line(i: u32) -> Vec<u8> {
    format!(
        "{{\"host\":\"node-{:03}.internal\",\"cpu_pct\":{}.{:02},\"mem_mb\":{},\
         \"disk_io_ops\":{},\"net_rx_kbps\":{},\"net_tx_kbps\":{},\
         \"collector\":\"telegraf\",\"interval_s\":10}}",
        i % 64,
        i % 100,
        (i * 7) % 100,
        2048 + (i * 13) % 4096,
        i * 17 % 100_000,
        i * 23 % 50_000,
        i * 29 % 50_000,
    )
    .into_bytes()
}

/// Deterministic pseudo-random (incompressible) bytes — the workload a
/// JSON-trained dictionary cannot win on.
fn random_body(seed: u32, len: usize) -> Vec<u8> {
    let mut state = u64::from(seed) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let word = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.truncate(len);
    out
}

async fn put_via(client: &aws_sdk_s3::Client, bucket: &str, key: &str, body: Vec<u8>) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(body))
        .send()
        .await
        .unwrap_or_else(|e| panic!("PUT {bucket}/{key}: {e}"));
}

async fn backend_codec_meta(
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
        .unwrap_or_else(|e| panic!("backend HEAD {bucket}/{key}: {e}"))
        .metadata()
        .and_then(|m| m.get("s4-codec"))
        .cloned()
}

/// Seed a training corpus, train a dictionary backend-direct, and return
/// the dict-id.
async fn train_corpus(
    backend: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
    bodies: impl Fn(u32) -> Vec<u8>,
) -> String {
    for i in 0..80u32 {
        backend
            .put_object()
            .bucket(bucket)
            .key(format!("{prefix}train/{i:04}.json"))
            .body(aws_sdk_s3::primitives::ByteStream::from(bodies(i)))
            .send()
            .await
            .expect("seed train object");
    }
    let params = TrainDictParams {
        prefix: prefix.to_owned(),
        max_samples: 1000,
        max_dict_bytes: 112_640,
        min_samples: 8,
        sample_max_bytes: 64 * 1024,
        zstd_level: CpuZstd::DEFAULT_LEVEL,
    };
    run_train_dict(backend, bucket, &params)
        .await
        .expect("train-dict")
        .dict_id
}

/// (a) + (b): per-prefix win/loss metrics on `/metrics`, healthy
/// `dict-status` exit 0, stale prefix → exit 1 + retrain warning.
#[tokio::test]
#[ignore = "requires Docker (MinIO testcontainer); run with --ignored"]
async fn dict_ops_e2e_metrics_and_dict_status() {
    const BUCKET: &str = "dictops";
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    backend
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("create bucket");

    let dict_id = train_corpus(&backend, BUCKET, "events/", |i| json_event(i, "t")).await;

    // Map the SAME JSON-trained dictionary to two prefixes: `events/`
    // (matching workload → wins) and `rand/` (random bodies → losses).
    let gw = spawn_bin_gateway(
        &minio.endpoint_url,
        &[
            "--zstd-dict".to_owned(),
            format!("{BUCKET}/events/={dict_id}"),
            "--zstd-dict".to_owned(),
            format!("{BUCKET}/rand/={dict_id}"),
        ],
    )
    .await;
    let gw_client = build_aws_client(&gw.endpoint_url);

    // ---- (a) matching workload → win counters + healthy dict-status ----
    const N: u32 = 30;
    for i in 0..N {
        put_via(
            &gw_client,
            BUCKET,
            &format!("events/new/{i:04}.json"),
            json_event(10_000 + i, "x"),
        )
        .await;
    }
    let metrics = scrape_metrics(&gw.metrics_url).await;
    let events_prefix = format!("{BUCKET}/events/");
    let wins = metric_sum(
        &metrics,
        "s4_dict_put_total",
        &[("prefix", &events_prefix), ("outcome", "win")],
    );
    assert!(
        wins >= 1.0,
        "expected win counter for {events_prefix} in metrics:\n{metrics}"
    );
    for kind in ["original", "dict", "plain"] {
        assert!(
            metric_sum(
                &metrics,
                "s4_dict_put_bytes_total",
                &[("prefix", &events_prefix), ("kind", kind)],
            ) > 0.0,
            "expected byte counter kind={kind} for {events_prefix} in metrics:\n{metrics}"
        );
    }

    let (ok, stdout, stderr) = run_dict_status(&gw.metrics_url, &[]);
    println!("--- s4 dict-status (healthy) ---\n{stdout}");
    assert!(
        ok,
        "dict-status must exit 0 while every prefix is healthy\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains(&events_prefix), "stdout:\n{stdout}");
    assert!(stdout.contains("WIN-RATE"), "stdout:\n{stdout}");
    assert!(stdout.contains("DICT-RATIO"), "stdout:\n{stdout}");

    // JSON shape sanity (same scrape, machine-readable).
    let (ok_json, stdout_json, _) = run_dict_status(&gw.metrics_url, &["--format", "json"]);
    assert!(ok_json);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout_json).expect("dict-status --format json must emit JSON");
    let prefixes = parsed["prefixes"].as_array().expect("prefixes array");
    let events = prefixes
        .iter()
        .find(|p| p["prefix"] == serde_json::Value::String(events_prefix.clone()))
        .expect("events prefix in JSON report");
    assert!(events["win_rate"].as_f64().expect("win_rate") > 0.5);
    let ratio = events["dict_ratio"].as_f64().expect("dict_ratio");
    assert!(
        ratio > 0.0 && ratio < 1.0,
        "homogeneous JSON must compress: dict_ratio {ratio}"
    );

    // ---- (b) mismatched workload → losses → exit 1 + retrain warning ----
    for i in 0..20u32 {
        put_via(
            &gw_client,
            BUCKET,
            &format!("rand/{i:04}.bin"),
            random_body(i + 1, 400),
        )
        .await;
    }
    let metrics = scrape_metrics(&gw.metrics_url).await;
    let rand_prefix = format!("{BUCKET}/rand/");
    let losses = metric_sum(
        &metrics,
        "s4_dict_put_total",
        &[("prefix", &rand_prefix), ("outcome", "loss")],
    );
    assert!(
        losses >= 1.0,
        "random bodies must lose to plain zstd in metrics:\n{metrics}"
    );

    let (ok, stdout, stderr) = run_dict_status(&gw.metrics_url, &[]);
    println!("--- s4 dict-status (stale prefix) ---\n{stdout}{stderr}");
    assert!(
        !ok,
        "dict-status must exit 1 with a stale prefix\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("dictionary may be stale; consider retraining (s4 train-dict)"),
        "stderr must carry the retrain warning:\n{stderr}"
    );
    assert!(
        stderr.contains(&rand_prefix),
        "warning must name the stale prefix:\n{stderr}"
    );
    assert!(
        stdout.contains("STALE"),
        "table must flag the stale prefix:\n{stdout}"
    );
    // The healthy prefix must NOT be in the warnings.
    assert!(
        !stderr.contains(&events_prefix),
        "healthy prefix must not warn:\n{stderr}"
    );
}

/// (c): `--zstd-dict-map` boot + SIGHUP rotation without a restart, and
/// the fail-safe on a broken map (current store survives).
#[tokio::test]
#[ignore = "requires Docker (MinIO testcontainer); run with --ignored"]
async fn dict_ops_e2e_map_file_sighup_reload() {
    const BUCKET: &str = "dictops2";
    let minio = start_minio().await;
    let backend = build_aws_client(&minio.endpoint_url);
    backend
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("create bucket");

    let dict_events = train_corpus(&backend, BUCKET, "events/", |i| json_event(i, "t")).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let map_path = dir.path().join("s4-dict-map.toml");
    std::fs::write(
        &map_path,
        format!("[mappings]\n\"{BUCKET}/events/\" = \"{dict_events}\"\n"),
    )
    .expect("write map file");

    let gw = spawn_bin_gateway(
        &minio.endpoint_url,
        &["--zstd-dict-map".to_owned(), map_path.display().to_string()],
    )
    .await;
    let gw_client = build_aws_client(&gw.endpoint_url);
    let pid = gw.child.id();

    // Boot from the map file works like the flag.
    put_via(
        &gw_client,
        BUCKET,
        "events/boot.json",
        json_event(999, "boot"),
    )
    .await;
    assert_eq!(
        backend_codec_meta(&backend, BUCKET, "events/boot.json").await,
        Some("cpu-zstd-dict".to_owned()),
        "map-file boot must dict-compress the mapped prefix"
    );

    // A not-yet-mapped prefix stays on plain cpu-zstd.
    put_via(&gw_client, BUCKET, "metrics/pre.json", metric_line(1)).await;
    assert_eq!(
        backend_codec_meta(&backend, BUCKET, "metrics/pre.json").await,
        Some("cpu-zstd".to_owned()),
        "unmapped prefix must stay dict-less before the rotation"
    );

    // ---- rotation: train → edit map → SIGHUP → no restart ----------------
    let dict_metrics = train_corpus(&backend, BUCKET, "metrics/", metric_line).await;
    std::fs::write(
        &map_path,
        format!(
            "[mappings]\n\"{BUCKET}/events/\" = \"{dict_events}\"\n\
             \"{BUCKET}/metrics/\" = \"{dict_metrics}\"\n"
        ),
    )
    .expect("append mapping to map file");
    send_sighup(pid);

    // Reload is asynchronous: poll the reload counter, then prove the
    // NEW prefix is dict-compressed without any restart.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let metrics = scrape_metrics(&gw.metrics_url).await;
        if metric_sum(&metrics, "s4_dict_reload_total", &[("result", "ok")]) >= 1.0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "SIGHUP reload did not report ok within 15s; metrics:\n{metrics}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    put_via(&gw_client, BUCKET, "metrics/post.json", metric_line(2)).await;
    assert_eq!(
        backend_codec_meta(&backend, BUCKET, "metrics/post.json").await,
        Some("cpu-zstd-dict".to_owned()),
        "after SIGHUP the new prefix must be dict-compressed WITHOUT a restart"
    );
    let head = backend
        .head_object()
        .bucket(BUCKET)
        .key("metrics/post.json")
        .send()
        .await
        .expect("backend HEAD");
    assert_eq!(
        head.metadata().and_then(|m| m.get("s4-dict-id")).cloned(),
        Some(dict_metrics.clone()),
        "the rotated-in mapping must use the NEW dictionary"
    );
    // The pre-existing mapping survived the swap.
    put_via(
        &gw_client,
        BUCKET,
        "events/after-reload.json",
        json_event(1_000, "ar"),
    )
    .await;
    assert_eq!(
        backend_codec_meta(&backend, BUCKET, "events/after-reload.json").await,
        Some("cpu-zstd-dict".to_owned())
    );

    // ---- fail-safe: broken map (nonexistent dict-id) ----------------------
    std::fs::write(
        &map_path,
        format!("[mappings]\n\"{BUCKET}/events/\" = \"00000000deadbeef\"\n"),
    )
    .expect("write broken map file");
    send_sighup(pid);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let metrics = scrape_metrics(&gw.metrics_url).await;
        if metric_sum(&metrics, "s4_dict_reload_total", &[("result", "err")]) >= 1.0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "broken-map SIGHUP did not report err within 15s; metrics:\n{metrics}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The current configuration survived the failed reload — BOTH
    // previously-live mappings keep dict-compressing.
    put_via(
        &gw_client,
        BUCKET,
        "events/survivor.json",
        json_event(2_000, "sv"),
    )
    .await;
    assert_eq!(
        backend_codec_meta(&backend, BUCKET, "events/survivor.json").await,
        Some("cpu-zstd-dict".to_owned()),
        "a failed reload must keep the previous store live (fail-safe)"
    );
    put_via(&gw_client, BUCKET, "metrics/survivor.json", metric_line(3)).await;
    assert_eq!(
        backend_codec_meta(&backend, BUCKET, "metrics/survivor.json").await,
        Some("cpu-zstd-dict".to_owned()),
        "a failed reload must keep the previous store live (fail-safe)"
    );
}
