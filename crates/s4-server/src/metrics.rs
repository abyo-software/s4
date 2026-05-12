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
