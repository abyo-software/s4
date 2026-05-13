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
        let handle = install();
        record_put("cpu-zstd", 1000, 100, 0.05, true);
        record_get("cpu-zstd", 100, 1000, 0.02, true);
        let rendered = handle.render();
        assert!(rendered.contains("s4_requests_total"));
        assert!(rendered.contains("s4_bytes_in_total"));
        assert!(rendered.contains("s4_bytes_out_total"));
        assert!(rendered.contains("s4_request_latency_seconds"));
        assert!(rendered.contains("op=\"put\""));
        assert!(rendered.contains("op=\"get\""));
        assert!(rendered.contains("codec=\"cpu-zstd\""));
    }
}
