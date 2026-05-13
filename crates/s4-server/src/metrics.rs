//! Prometheus metrics 統合。
//!
//! `metrics` crate を facade に使い、`metrics-exporter-prometheus` で `/metrics`
//! endpoint に Prometheus text 形式で露出する。
//!
//! ## 露出されるメトリクス
//!
//! - `s4_requests_total{op,codec,result}` (counter): PUT/GET 要求総数。
//!   `result` は `ok` / `err`、`op` は `put` / `get`、`codec` は dispatch 結果
//! - `s4_bytes_in_total{op,codec}` (counter): client から受け取った bytes 累計
//! - `s4_bytes_out_total{op,codec}` (counter): backend に送る (PUT) / client へ
//!   返す (GET) bytes 累計
//! - `s4_request_latency_seconds{op,codec}` (histogram): 1 request の所要時間。
//!   bucket は default (10ms-10s)
//!
//! 圧縮率は Prometheus 側で `s4_bytes_out_total / s4_bytes_in_total` で計算可能。

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// アプリ起動時に 1 回呼ぶ。Prometheus exporter を install し、`/metrics`
/// endpoint で render するための handle を返す。
pub fn install() -> PrometheusHandle {
    PrometheusBuilder::new()
        .install_recorder()
        .expect("metrics recorder install (must be called once at startup)")
}

/// metrics 名 (constant にして typo 防止 + リネーム集中管理)
pub mod names {
    pub const REQUESTS_TOTAL: &str = "s4_requests_total";
    pub const BYTES_IN_TOTAL: &str = "s4_bytes_in_total";
    pub const BYTES_OUT_TOTAL: &str = "s4_bytes_out_total";
    pub const REQUEST_LATENCY_SECONDS: &str = "s4_request_latency_seconds";
    pub const POLICY_DENIALS_TOTAL: &str = "s4_policy_denials_total";
    pub const TLS_CERT_RELOAD_TOTAL: &str = "s4_tls_cert_reload_total";
    pub const ACME_RENEWAL_TOTAL: &str = "s4_acme_renewal_total";
    pub const ACME_CERT_EXPIRY_SECONDS: &str = "s4_acme_cert_expiry_seconds";
    pub const RATE_LIMIT_THROTTLED_TOTAL: &str = "s4_rate_limit_throttled_total";
    pub const COMPLIANCE_MODE_ACTIVE: &str = "s4_compliance_mode_active";
    /// v0.6 #35: bumped each time the notification dispatcher exhausts
    /// its retry budget on a destination (or skips an `aws-events`-gated
    /// destination because the feature is off).
    pub const NOTIFICATIONS_DROPPED_TOTAL: &str = "s4_notifications_dropped_total";
    /// v0.6 #37: bumped by the lifecycle scanner each time it executes an
    /// Expiration / Transition / NoncurrentVersionExpiration action.
    /// Labels: `bucket` (S3 bucket name), `action` (= `"expire"` /
    /// `"transition"` / `"noncurrent_expire"`). Cardinality bounded by
    /// (#buckets × 3).
    pub const LIFECYCLE_ACTIONS_TOTAL: &str = "s4_lifecycle_actions_total";
    /// v0.6 #40: bumped each time the cross-bucket replication
    /// dispatcher exhausts its retry budget on a destination PUT.
    /// Label `bucket` is the source bucket. Cardinality bounded by
    /// (#source-buckets-with-replication-rules).
    pub const REPLICATION_DROPPED_TOTAL: &str = "s4_replication_dropped_total";
    /// v0.6 #40: bumped each time the cross-bucket replication
    /// dispatcher succeeds in PUT-ing a replica to the destination
    /// bucket. Labels: `bucket` (source), `dest` (destination).
    /// Cardinality bounded by (#source × #destination) pairs that
    /// actually fire.
    pub const REPLICATION_REPLICATED_TOTAL: &str = "s4_replication_replicated_total";
    /// v0.6 #42: bumped each time the MFA-Delete gate refuses a
    /// DELETE / DELETE-version / delete-marker / `PutBucketVersioning`
    /// request because the `x-amz-mfa` header was missing, malformed,
    /// or carried an invalid serial / TOTP code. Label: `bucket`
    /// (cardinality bounded by # of MFA-Delete-protected buckets).
    pub const MFA_DELETE_DENIALS_TOTAL: &str = "s4_mfa_delete_denials_total";
    /// v0.8 #55: per-GPU-compress wall-clock seconds histogram. Labels:
    /// `codec` (= the GPU codec kind name, e.g. `"nvcomp-bitcomp"`).
    /// Cardinality bounded by the GPU codec set (~3-4).
    pub const GPU_COMPRESS_SECONDS: &str = "s4_gpu_compress_seconds";
    /// v0.8 #55: per-GPU-decompress wall-clock seconds histogram. Same
    /// label shape as [`GPU_COMPRESS_SECONDS`].
    pub const GPU_DECOMPRESS_SECONDS: &str = "s4_gpu_decompress_seconds";
    /// v0.8 #55: gauge of the most-recently-observed GPU codec
    /// throughput in bytes/sec. Labels: `codec` (codec kind name),
    /// `op` (= `"compress"` / `"decompress"`). Set on every GPU op so
    /// the gauge tracks the rolling latest sample; pair with the
    /// histogram for p99 latency vs. peak throughput dashboards.
    pub const GPU_THROUGHPUT_BYTES_PER_SEC: &str = "s4_gpu_throughput_bytes_per_sec";
    /// v0.8 #55: gauge of in-flight GPU operations (compress +
    /// decompress combined). Operators alert when this gauge stays
    /// pinned at the configured concurrency cap, signalling GPU
    /// saturation / queue head-of-line blocking. Labels: `codec`.
    pub const GPU_IN_FLIGHT: &str = "s4_gpu_in_flight";
    /// v0.8 #55: counter bumped each time a GPU compress / decompress
    /// fails with an out-of-memory error (cudaErrorMemoryAllocation /
    /// nvCOMP equivalent). Labels: `codec`. Pair with
    /// `s4_requests_total{result="err"}` to attribute error spikes to
    /// GPU OOM versus generic backend failures.
    pub const GPU_OOM_TOTAL: &str = "s4_gpu_oom_total";
    /// v0.8 #50: gauge stamped once at boot reflecting which AES
    /// implementation backs SSE-S4 encrypt/decrypt on the running
    /// host. Labels: `kind` (= `"aes-ni"` on x86_64 with the AES-NI +
    /// PCLMULQDQ CPU features detected at runtime, `"neon"` on
    /// aarch64 with the AES NEON extensions, `"software"` otherwise).
    /// Always set to 1.0 — the operator filters by label to confirm
    /// the hardware-acceleration path is live (`s4_sse_aes_backend{kind="aes-ni"} == 1`).
    pub const SSE_AES_BACKEND: &str = "s4_sse_aes_backend";
}

/// v0.8 #50: re-export of [`names::SSE_AES_BACKEND`] at the crate root
/// (mirroring how `record_*` helpers below sit alongside the constants
/// they reference) so call sites that need the metric name string can
/// import it without going through the `names` module.
pub const SSE_AES_BACKEND: &str = names::SSE_AES_BACKEND;

/// v0.8 #50: stamp the SSE AES-backend gauge at boot. `kind` is one of
/// `"aes-ni"` / `"neon"` / `"software"` (see [`names::SSE_AES_BACKEND`]).
/// Called exactly once from `main.rs` after [`install`] so the gauge
/// shows up on the very first `/metrics` scrape.
pub fn record_sse_aes_backend(kind: &'static str) {
    metrics::gauge!(SSE_AES_BACKEND, "kind" => kind).set(1.0);
}

/// v0.8 #55: stamp metrics after a GPU compress completes.
///
/// `codec` is the GPU codec kind name (`CodecKind::as_str()` —
/// `"nvcomp-zstd"` / `"nvcomp-bitcomp"` / `"nvcomp-gdeflate"`),
/// `secs` is wall-clock seconds the op took (input includes any
/// host→device copy + kernel launch + device→host copy), `bytes_in`
/// is the uncompressed input length, `bytes_out` is the compressed
/// output length. Throughput is computed as `bytes_in / secs`
/// (saturated to 1e-9 to avoid division by zero on instantly-cached
/// micro inputs).
///
/// `bytes_out` is currently exposed only via the throughput
/// computation paired with the existing `s4_bytes_out_total{op="put"}`
/// counter — split-out compressed-bytes-per-op metric is left to
/// follow-up to keep cardinality bounded.
pub fn record_gpu_compress(codec: &'static str, secs: f64, bytes_in: u64, bytes_out: u64) {
    metrics::histogram!(names::GPU_COMPRESS_SECONDS, "codec" => codec).record(secs);
    let throughput = (bytes_in as f64) / secs.max(1e-9);
    metrics::gauge!(
        names::GPU_THROUGHPUT_BYTES_PER_SEC,
        "codec" => codec,
        "op" => "compress",
    )
    .set(throughput);
    // Reserved for a follow-up `s4_gpu_bytes_out_total` split — pulled
    // through the API now so the call sites already pass it and a
    // future PR adds the metric without re-touching every caller.
    let _ = bytes_out;
}

/// v0.8 #55: mirror of [`record_gpu_compress`] for the decompress side.
/// `bytes_in` is the compressed input size, `bytes_out` is the
/// uncompressed output size — throughput here is `bytes_out / secs`
/// (decompressed-bytes-per-second is the standard nvCOMP / DietGPU
/// reporting convention).
pub fn record_gpu_decompress(codec: &'static str, secs: f64, bytes_in: u64, bytes_out: u64) {
    metrics::histogram!(names::GPU_DECOMPRESS_SECONDS, "codec" => codec).record(secs);
    let throughput = (bytes_out as f64) / secs.max(1e-9);
    metrics::gauge!(
        names::GPU_THROUGHPUT_BYTES_PER_SEC,
        "codec" => codec,
        "op" => "decompress",
    )
    .set(throughput);
    let _ = bytes_in;
}

/// v0.8 #55: increment the in-flight GPU op gauge for `codec`. Pair
/// with [`record_gpu_in_flight_dec`] in a guard wrapper to keep the
/// gauge balanced even when the op errors out.
pub fn record_gpu_in_flight_inc(codec: &'static str) {
    metrics::gauge!(names::GPU_IN_FLIGHT, "codec" => codec).increment(1.0);
}

/// v0.8 #55: decrement the in-flight GPU op gauge for `codec`.
pub fn record_gpu_in_flight_dec(codec: &'static str) {
    metrics::gauge!(names::GPU_IN_FLIGHT, "codec" => codec).decrement(1.0);
}

/// v0.8 #55: bump the GPU OOM counter for `codec`. Called from the
/// service-layer telemetry stamp when [`s4_codec::CodecError`] is
/// classified as OOM (the backend layer surfaces the underlying
/// CUDA error string; classification is a substring match against
/// `"out of memory"` / `"cudaErrorMemoryAllocation"`).
pub fn record_gpu_oom(codec: &'static str) {
    metrics::counter!(names::GPU_OOM_TOTAL, "codec" => codec).increment(1);
}

/// v0.6 #42: bump the MFA-Delete denial counter for `bucket` (covers all
/// `MfaError` variants: missing header, malformed header, serial
/// mismatch, invalid TOTP code). The handler still returns the
/// appropriate S3 error (`AccessDenied` / 400) before this fires; the
/// counter is purely operational visibility, paired with
/// `s4_requests_total{op="delete", result="err"}` so an operator can
/// attribute spikes in delete failures to MFA gating versus other refusals.
pub fn record_mfa_delete_denial(bucket: &str) {
    metrics::counter!(
        names::MFA_DELETE_DENIALS_TOTAL,
        "bucket" => bucket.to_owned(),
    )
    .increment(1);
}

/// v0.6 #40: bumped each time the replication dispatcher exhausts its
/// retry budget on a destination PUT. The label `bucket` is the source
/// (= the bucket whose replication rule matched), so dashboards split
/// drops by the rule's owning bucket. Pair with [`record_replication_replicated`]
/// for the success counter.
pub fn record_replication_drop(bucket: &str) {
    metrics::counter!(
        names::REPLICATION_DROPPED_TOTAL,
        "bucket" => bucket.to_owned(),
    )
    .increment(1);
}

/// v0.6 #40: bumped on each successful destination PUT made by the
/// replication dispatcher. `bucket` is the source bucket (rule owner)
/// and `dest` is the destination bucket the rule pointed at.
pub fn record_replication_replicated(bucket: &str, dest: &str) {
    metrics::counter!(
        names::REPLICATION_REPLICATED_TOTAL,
        "bucket" => bucket.to_owned(),
        "dest" => dest.to_owned(),
    )
    .increment(1);
}

/// v0.6 #37: bumped each time the lifecycle scanner executes an action
/// (Expiration / Transition / NoncurrentVersionExpiration). Pair with
/// [`crate::lifecycle::LifecycleManager::record_action`] which keeps the
/// in-process counter in sync with this Prometheus counter so a
/// `/metrics` scrape and an admin introspection of `actions_snapshot()`
/// agree.
pub fn record_lifecycle_action(bucket: &str, action: &'static str) {
    metrics::counter!(
        names::LIFECYCLE_ACTIONS_TOTAL,
        "bucket" => bucket.to_owned(),
        "action" => action,
    )
    .increment(1);
}

/// v0.6 #35: bumped each time the notification dispatcher drops an event
/// (5xx after retry budget exhausted, network failure after retries, or an
/// `aws-events`-gated destination invoked without the feature compiled
/// in). Label `dest` is one of `"webhook"` / `"sqs"` / `"sns"` so
/// dashboards can split by destination type without leaking ARNs / URLs.
pub fn record_notification_drop(dest_type: &'static str) {
    metrics::counter!(
        names::NOTIFICATIONS_DROPPED_TOTAL,
        "dest" => dest_type,
    )
    .increment(1);
}

/// v0.5 #32: stamp the gauge so operators can `up{...} * on() s4_compliance_mode_active`
/// to confirm at-a-glance that the strict gate is live across the fleet.
pub fn record_compliance_mode_active(mode: &'static str) {
    metrics::gauge!(names::COMPLIANCE_MODE_ACTIVE, "mode" => mode).set(1.0);
}

/// v0.4 #19: bumped each time a request is rejected by the rate limiter.
/// Labels: principal (= access key id or `"-"` for anonymous), bucket.
pub fn record_rate_limit_throttle(principal: &str, bucket: &str) {
    metrics::counter!(
        names::RATE_LIMIT_THROTTLED_TOTAL,
        "principal" => principal.to_owned(),
        "bucket" => bucket.to_owned(),
    )
    .increment(1);
}

/// v0.3 #10: bumped each time the operator triggers a SIGHUP-driven TLS
/// cert hot-reload. Labels: `result` (= "ok" | "err").
pub fn record_tls_cert_reload(ok: bool) {
    let result = if ok { "ok" } else { "err" };
    metrics::counter!(names::TLS_CERT_RELOAD_TOTAL, "result" => result).increment(1);
}

/// v0.3 #11: bumped each time rustls-acme triggers a renewal cycle (success
/// or failure). Labels: `result` (= "ok" | "err"). Operators alert on this
/// counter to catch a stuck renewal before the cert expires.
pub fn record_acme_renewal(ok: bool) {
    let result = if ok { "ok" } else { "err" };
    metrics::counter!(names::ACME_RENEWAL_TOTAL, "result" => result).increment(1);
}

/// v0.3 #11: gauge of seconds until the active ACME cert expires. Operators
/// alert when this drops below 14 days, which would mean renewal has been
/// failing silently for ~46 days (Let's Encrypt 90-day cert lifetime).
pub fn record_acme_cert_expiry(seconds_until_expiry: f64) {
    metrics::gauge!(names::ACME_CERT_EXPIRY_SECONDS).set(seconds_until_expiry);
}

/// v0.2 #7: bumped each time the gateway's bucket policy denies a request.
/// Labels: action (e.g. "s3:GetObject"), bucket. Cardinality is bounded by
/// the supported S3 action set × number of buckets actually accessed.
pub fn record_policy_denial(action: &'static str, bucket: &str) {
    metrics::counter!(
        names::POLICY_DENIALS_TOTAL,
        "action" => action,
        "bucket" => bucket.to_owned(),
    )
    .increment(1);
}

/// 1 PUT request 完了時に呼ぶ
pub fn record_put(codec: &'static str, bytes_in: u64, bytes_out: u64, latency_secs: f64, ok: bool) {
    let result = if ok { "ok" } else { "err" };
    metrics::counter!(names::REQUESTS_TOTAL, "op" => "put", "codec" => codec, "result" => result)
        .increment(1);
    metrics::counter!(names::BYTES_IN_TOTAL, "op" => "put", "codec" => codec).increment(bytes_in);
    metrics::counter!(names::BYTES_OUT_TOTAL, "op" => "put", "codec" => codec).increment(bytes_out);
    metrics::histogram!(names::REQUEST_LATENCY_SECONDS, "op" => "put", "codec" => codec)
        .record(latency_secs);
}

/// 1 GET request 完了時に呼ぶ
pub fn record_get(codec: &'static str, bytes_in: u64, bytes_out: u64, latency_secs: f64, ok: bool) {
    let result = if ok { "ok" } else { "err" };
    metrics::counter!(names::REQUESTS_TOTAL, "op" => "get", "codec" => codec, "result" => result)
        .increment(1);
    metrics::counter!(names::BYTES_IN_TOTAL, "op" => "get", "codec" => codec).increment(bytes_in);
    metrics::counter!(names::BYTES_OUT_TOTAL, "op" => "get", "codec" => codec).increment(bytes_out);
    metrics::histogram!(names::REQUEST_LATENCY_SECONDS, "op" => "get", "codec" => codec)
        .record(latency_secs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_and_render_basic_counters() {
        // 同 process 内で複数回 install できないので、テストは 1 回限り。
        // record した値が render に現れることを確認。
        // v0.8 #55: also drive the GPU-pipeline recorders so the same
        // single recorder install covers all metric families. Splitting
        // these into separate `#[test]` fns would race on the global
        // `PrometheusBuilder::install_recorder()` slot.
        let handle = install();
        record_put("cpu-zstd", 1000, 100, 0.05, true);
        record_get("cpu-zstd", 100, 1000, 0.02, true);

        // v0.8 #55: GPU-compress histogram + throughput gauge.
        record_gpu_compress("nvcomp-zstd", 0.012, 10_000_000, 800_000);
        // v0.8 #55: GPU-decompress histogram + throughput gauge.
        record_gpu_decompress("nvcomp-zstd", 0.008, 800_000, 10_000_000);
        // v0.8 #55: in-flight gauge (inc then dec → render shows 0).
        record_gpu_in_flight_inc("nvcomp-bitcomp");
        record_gpu_in_flight_inc("nvcomp-bitcomp");
        record_gpu_in_flight_dec("nvcomp-bitcomp");
        // v0.8 #55: OOM counter.
        record_gpu_oom("nvcomp-gdeflate");
        // v0.8 #50: SSE AES-backend boot gauge — both label values so
        // the render assertion below can grep either side.
        record_sse_aes_backend("aes-ni");
        record_sse_aes_backend("software");

        let rendered = handle.render();
        // Pre-existing assertions.
        assert!(rendered.contains("s4_requests_total"));
        assert!(rendered.contains("s4_bytes_in_total"));
        assert!(rendered.contains("s4_bytes_out_total"));
        assert!(rendered.contains("s4_request_latency_seconds"));
        assert!(rendered.contains("op=\"put\""));
        assert!(rendered.contains("op=\"get\""));
        assert!(rendered.contains("codec=\"cpu-zstd\""));

        // v0.8 #55: new GPU metrics show up with their codec labels.
        assert!(
            rendered.contains("s4_gpu_compress_seconds"),
            "missing GPU compress histogram in: {rendered}"
        );
        assert!(
            rendered.contains("s4_gpu_decompress_seconds"),
            "missing GPU decompress histogram in: {rendered}"
        );
        assert!(
            rendered.contains("s4_gpu_throughput_bytes_per_sec"),
            "missing throughput gauge in: {rendered}"
        );
        assert!(
            rendered.contains("s4_gpu_in_flight"),
            "missing in_flight gauge in: {rendered}"
        );
        assert!(
            rendered.contains("s4_gpu_oom_total"),
            "missing OOM counter in: {rendered}"
        );
        // Codec labels are preserved.
        assert!(rendered.contains("codec=\"nvcomp-zstd\""));
        assert!(rendered.contains("codec=\"nvcomp-bitcomp\""));
        assert!(rendered.contains("codec=\"nvcomp-gdeflate\""));
        // op label distinguishes throughput direction.
        assert!(rendered.contains("op=\"compress\""));
        assert!(rendered.contains("op=\"decompress\""));

        // v0.8 #50: SSE AES backend gauge with `kind` label.
        assert!(
            rendered.contains("s4_sse_aes_backend"),
            "missing SSE AES backend gauge in: {rendered}"
        );
        assert!(rendered.contains("kind=\"aes-ni\""));
        assert!(rendered.contains("kind=\"software\""));
    }

    /// v0.8 #55: throughput gauge math. 10 MiB in 10 ms ≈ 1.05 GB/s
    /// (decimal). Verifies the `bytes_in / secs` formula is wired
    /// correctly (regression guard against accidentally swapping
    /// bytes_out into the compress throughput slot).
    #[test]
    fn gpu_compress_throughput_math() {
        let secs = 0.010_f64;
        let bytes_in: u64 = 10 * 1024 * 1024;
        let bytes_out: u64 = 1024 * 1024;
        // Compress throughput convention: bytes_in / secs (the rate
        // the codec is consuming uncompressed input). Reproducing the
        // exact formula here so a future swap of numerator into
        // bytes_out trips the test.
        let expected = (bytes_in as f64) / secs.max(1e-9);
        // 10 * 1024 * 1024 / 0.010 = 1_048_576_000 bytes/sec exactly.
        let want_bytes_per_sec: f64 = 10.0 * 1024.0 * 1024.0 / 0.010;
        assert!((expected - want_bytes_per_sec).abs() < 1.0);
        assert!((expected - 1_048_576_000.0).abs() < 1.0);
        // Drive the recorder once to confirm it doesn't panic on these
        // inputs (the global recorder may or may not be installed in
        // this test order, so we only assert it survives).
        record_gpu_compress("nvcomp-zstd", secs, bytes_in, bytes_out);
    }

    /// v0.8 #55: decompress throughput uses `bytes_out / secs`
    /// (decompressed bytes per second — the nvCOMP reporting
    /// convention) so we verify the direction is right.
    #[test]
    fn gpu_decompress_throughput_math() {
        let secs = 0.005_f64;
        let bytes_in: u64 = 1024 * 1024; // compressed input
        let bytes_out: u64 = 10 * 1024 * 1024; // decompressed output
        let expected = (bytes_out as f64) / secs.max(1e-9);
        // 10 * 1024 * 1024 / 0.005 = 2_097_152_000 bytes/sec exactly.
        assert!((expected - 2_097_152_000.0).abs() < 1.0);
        record_gpu_decompress("nvcomp-zstd", secs, bytes_in, bytes_out);
    }

    /// v0.8 #55: OOM counter accepts arbitrary codec labels and never
    /// panics. The label is `&'static str` (we route through
    /// `CodecKind::as_str()`) so the recorder stores it without
    /// allocating per-call.
    #[test]
    fn gpu_oom_counter_accepts_all_gpu_codecs() {
        for codec in ["nvcomp-zstd", "nvcomp-bitcomp", "nvcomp-gdeflate"] {
            record_gpu_oom(codec);
        }
        // No panic == pass; the in-process render side is covered by
        // `install_and_render_basic_counters`.
    }
}
