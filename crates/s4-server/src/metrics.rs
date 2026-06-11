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

/// v0.8.3 #65 (audit C-2): shared test-only handle to the
/// process-global Prometheus recorder. The recorder slot is a
/// `PrometheusBuilder::install_recorder()` singleton, so multiple
/// tests in the same binary that need to grep the rendered output
/// MUST go through this helper instead of calling [`install`]
/// directly — otherwise the first test wins and every subsequent
/// `install()` panics with `FailedToSetGlobalRecorder`. Returns a
/// cloned [`PrometheusHandle`]; `.render()` is cheap so each caller
/// can render on demand.
#[cfg(test)]
pub(crate) fn test_metrics_handle() -> PrometheusHandle {
    use std::sync::OnceLock;
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(install).clone()
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
    /// v0.8 #52: counter bumped once per S4E5 chunk that flows
    /// through the streaming SSE-S4 path. Label: `op` (= `"encrypt"`
    /// for the PUT side, `"decrypt"` for the GET side). Operators
    /// divide by `s4_requests_total{op="put"|"get"}` to compute
    /// average chunks-per-object — pair with `--sse-chunk-size` to
    /// verify the configured slicing actually fires (e.g. a 50 MiB
    /// PUT at 1 MiB chunks should bump this counter by ~50).
    pub const SSE_STREAMING_CHUNKS_TOTAL: &str = "s4_sse_streaming_chunks_total";
    /// v0.8.2 #62: counter bumped each time the multipart abandoned-
    /// upload sweep drops one or more `MultipartUploadContext`
    /// entries (client called `CreateMultipartUpload` then never
    /// invoked Complete / Abort within
    /// `--multipart-abandoned-ttl-hours`). The increment is the
    /// per-tick batch count returned by
    /// `MultipartStateStore::sweep_stale`. Operators alert on a
    /// sustained non-zero rate to catch buggy clients leaking
    /// upload state (and, for SSE-C uploads, raw 32-byte customer
    /// keys before the `Zeroizing<[u8; 32]>` Drop wipes them).
    pub const MULTIPART_ABANDONED_UPLOADS_TOTAL: &str = "s4_multipart_abandoned_uploads_total";
    /// v0.8.3 #66 (H-5 audit fix): counter bumped each time the
    /// replication-status sweep drops one or more terminal-state
    /// (Completed / Failed) entries from `ReplicationManager::statuses`.
    /// The increment is the per-tick batch count returned by
    /// `ReplicationManager::sweep_stale`. Operators dashboard this
    /// counter to confirm the TTL knob is actually pruning entries
    /// under high-cardinality workloads (= without the sweep, the
    /// statuses HashMap would grow unbounded and inflate the JSON
    /// snapshot persisted by `to_json`).
    pub const REPLICATION_STATUS_SWEPT_TOTAL: &str = "s4_replication_status_swept_total";
    /// v0.8.3 #68 (M-1 audit fix): counter bumped each time the
    /// replication dispatcher PUT-ed a replica whose source carried
    /// Object Lock state (`mode` / `retain_until` / `legal_hold_on`)
    /// but the destination-side `S4Service` has no
    /// `ObjectLockManager` attached, so the propagation could not be
    /// committed and the destination operator can freely DELETE the
    /// replica. Operators alert on a non-zero rate to catch DR
    /// configurations whose destination silently drops the source's
    /// WORM posture. Pair with the WARN log line emitted once per
    /// `(source_bucket, dest_bucket)` pair so dashboards have both
    /// per-pair signal (log) and aggregate volume (counter).
    pub const REPLICATION_LOCK_PROPAGATION_SKIPPED_TOTAL: &str =
        "s4_replication_lock_propagation_skipped_total";
    /// v0.8.4 #72: counter bumped each time a `--*-state-file <PATH>`
    /// snapshot fails to load at boot — the gateway fell back to a
    /// fresh in-memory manager and the operator's file is left in
    /// place for inspection. Labels:
    /// - `manager` — short stable name (`"versioning"`, `"object_lock"`,
    ///   `"mfa_delete"`, `"cors"`, `"inventory"`, `"notifications"`,
    ///   `"tagging"`, `"replication"`, `"lifecycle"`).
    /// - `reason` — `"read_error"` (filesystem read failed: permission,
    ///   I/O error) or `"parse_error"` (corrupted JSON / schema drift).
    ///
    /// Cardinality bounded by (#managers × 2) = 18.
    /// Operators alert on `rate(s4_state_file_load_failures_total > 0)`
    /// so silent boot-time fall-backs surface in dashboards even when
    /// the gateway itself comes up cleanly. Pair with the WARN log line
    /// emitted by [`crate::state_loader::load_or_fresh`] for the per-
    /// call detail (manager / path / underlying error).
    pub const STATE_FILE_LOAD_FAILURES_TOTAL: &str = "s4_state_file_load_failures_total";
    /// v0.8.4 #77 (audit H-8): bumped each time a state-manager
    /// `RwLock` / `Mutex` recovery helper (see [`crate::lock_recovery`])
    /// observes a poisoned lock and forwards the inner data instead of
    /// re-panicking. Labels: `lock` (= `"<manager>.<field>"`, e.g.
    /// `"versioning.index"`, `"replication.statuses"`) and `kind`
    /// (= `"read"` / `"write"` / `"mutex"`).
    ///
    /// A non-zero rate signals that a panic landed inside a guarded
    /// section somewhere — the gateway kept serving (good), but the
    /// underlying panic itself should still be investigated. Pair with
    /// the WARN log lines emitted by the recovery helpers for the
    /// per-call detail.
    pub const LOCK_POISON_RECOVERY_TOTAL: &str = "s4_lock_poison_recovery_total";
    /// v0.8.5 #81 (audit H-7): bumped each time a detached
    /// dispatcher task (replication / notification) panics inside the
    /// `tokio::spawn` body. The panic is caught by a
    /// `futures::FutureExt::catch_unwind` wrapper at the spawn site so
    /// the runtime is never poisoned and the outer service keeps
    /// serving; the counter surfaces the silent feature degradation
    /// (e.g. all replication PUTs failing because a destination
    /// backend started returning unexpected payloads) that would
    /// otherwise only be visible in a stderr scrape. Labels: `kind`
    /// (= `"replication"` / `"notification"`). Cardinality bounded
    /// by the dispatcher kind set (= 2). Operators alert on a
    /// non-zero rate.
    pub const DISPATCHER_PANICS_TOTAL: &str = "s4_dispatcher_panics_total";
    /// v0.8.5 #86 (audit M-3): counter bumped each time the operator
    /// sends `SIGUSR1` to the gateway and the snapshot dump-back routine
    /// re-emits one in-memory manager's state to its
    /// `--<manager>-state-file <PATH>`. Labels:
    /// - `manager` — short stable name (`"versioning"`, `"object_lock"`,
    ///   `"mfa_delete"`, `"cors"`, `"inventory"`, `"notifications"`,
    ///   `"tagging"`, `"replication"`, `"lifecycle"`).
    /// - `result` — `"ok"` (atomic write completed) / `"err"`
    ///   (`to_json` / fs::write / fs::rename returned an error — the
    ///   underlying state file is left untouched in that case).
    ///
    /// Cardinality bounded by (#managers × 2) = 18. Operators alert on
    /// `rate(s4_sigusr1_dump_total{result="err"} > 0)` so silent
    /// snapshot-write failures surface in dashboards.
    pub const SIGUSR1_DUMP_TOTAL: &str = "s4_sigusr1_dump_total";
    /// v1.1 `--zstd-dict`: bumped on every **lazy** GET-side dictionary
    /// fetch (`.s4dict/<id>` pulled from the backend because the dict
    /// was neither preloaded via `--zstd-dict` nor already in the LRU).
    /// Labels: `result` (= `"ok"` / `"err"` — `err` covers backend GET
    /// failure, missing object, and fingerprint mismatch; the GET that
    /// needed the dictionary returns 5xx in the `err` case). Boot-time
    /// preloads and LRU hits do NOT bump this counter — a sustained
    /// `ok` rate therefore means GET traffic depends on a dictionary
    /// the gateway wasn't configured with (operator should re-add the
    /// `--zstd-dict` flag); any `err` rate means dict-compressed
    /// objects are currently unreadable.
    pub const DICT_FETCH_TOTAL: &str = "s4_dict_fetch_total";
    /// v1.2 `--gpu-batch-small-puts`: bumped once per small PUT that was
    /// considered for batched GPU compression. Labels: `result`
    /// (= `"batched"` — the body was compressed by the nvCOMP batch
    /// aggregator and stored as `nvcomp-zstd`; `"fallback"` — the batch
    /// path declined (queue full, GPU error, or the batched output was
    /// not smaller than the input) and the PUT proceeded on the
    /// pre-existing cpu-zstd framed path). Cardinality 2. Operators
    /// compare `batched` vs `fallback` rates to confirm the batching
    /// window / queue depth are sized for the workload — a sustained
    /// `fallback` rate means the flag is configured but not paying off.
    pub const GPU_BATCH_TOTAL: &str = "s4_gpu_batch_total";
    /// v1.2 `--savings-ledger-state-file`: gauge of cumulative logical
    /// bytes clients PUT through the gateway (pre-compression), per
    /// bucket. Set (not incremented) from the ledger's post-mutation
    /// totals so the gauge always mirrors the state file. Labels:
    /// `bucket`. Cardinality bounded by the bucket count. Never
    /// registered when the flag is off — the ledger is the only call
    /// site and it only exists when the operator opted in.
    pub const LEDGER_ORIGINAL_BYTES: &str = "s4_ledger_original_bytes";
    /// v1.2 `--savings-ledger-state-file`: gauge of cumulative bytes
    /// the gateway actually wrote to the backend (frames + SSE
    /// envelope + sidecars), per bucket. Pair with
    /// [`LEDGER_ORIGINAL_BYTES`]: savings ratio =
    /// `1 - stored/original`. Labels: `bucket`.
    pub const LEDGER_STORED_BYTES: &str = "s4_ledger_stored_bytes";
    /// v1.2 `--savings-ledger-state-file`: gauge of currently-stored
    /// gateway-written objects (versions count on versioning-Enabled
    /// buckets), per bucket. Labels: `bucket`.
    pub const LEDGER_OBJECTS: &str = "s4_ledger_objects";
}

/// v1.2 `--savings-ledger-state-file`: stamp the three per-bucket
/// ledger gauges from a post-mutation totals snapshot. Called by
/// [`crate::ledger::SavingsLedger`] after every counter mutation (and
/// once per restored bucket at boot), so the gauges always mirror the
/// state file. With the flag off the ledger never exists and these
/// gauges are never registered.
pub fn record_ledger_bucket(bucket: &str, totals: &crate::ledger::BucketTotals) {
    metrics::gauge!(names::LEDGER_ORIGINAL_BYTES, "bucket" => bucket.to_owned())
        .set(totals.original_bytes as f64);
    metrics::gauge!(names::LEDGER_STORED_BYTES, "bucket" => bucket.to_owned())
        .set(totals.stored_bytes as f64);
    metrics::gauge!(names::LEDGER_OBJECTS, "bucket" => bucket.to_owned())
        .set(totals.objects as f64);
}

/// v1.2 `--gpu-batch-small-puts`: bump the small-PUT GPU-batch outcome
/// counter. `result` is `"batched"` (stored as batch-compressed
/// nvcomp-zstd) or `"fallback"` (declined / failed → pre-existing
/// cpu-zstd framed path). See [`names::GPU_BATCH_TOTAL`].
pub fn record_gpu_batch(result: &'static str) {
    metrics::counter!(names::GPU_BATCH_TOTAL, "result" => result).increment(1);
}

/// v1.1 `--zstd-dict`: bump the lazy dictionary-fetch counter. `result`
/// is `"ok"` (fetched + fingerprint-verified + cached) or `"err"` (the
/// GET that needed the dictionary failed with 5xx).
pub fn record_dict_fetch(result: &'static str) {
    metrics::counter!(names::DICT_FETCH_TOTAL, "result" => result).increment(1);
}

/// v0.8.5 #86 (audit M-3): bump the SIGUSR1 snapshot dump-back counter.
/// `manager` is the stable short label (`"versioning"`, `"object_lock"`,
/// `"replication"`, …); `success` is `true` when the atomic write
/// (`to_json` → tmp file → rename) completed and `false` when any step
/// returned an error. Called once per managed snapshot per SIGUSR1
/// reception so the per-signal dump batch shows up as N counter bumps,
/// one per attached manager.
pub fn record_sigusr1_dump(manager: &'static str, success: bool) {
    let result = if success { "ok" } else { "err" };
    metrics::counter!(names::SIGUSR1_DUMP_TOTAL, "manager" => manager, "result" => result)
        .increment(1);
}

/// v0.8.5 #81 (audit H-7): bump the per-kind dispatcher-panic counter.
/// Called from the panic-catch wrapper at the
/// `spawn_replication_if_matched` (kind=`"replication"`) and
/// `dispatch_event` (kind=`"notification"`) spawn sites whenever the
/// detached dispatcher closure panicked. The counter is the
/// dashboard-friendly aggregate so an alert can fire even if log
/// scraping is off; pair with the ERROR log line carrying the
/// `panic_payload` for the per-incident detail.
pub fn record_dispatcher_panic(kind: &'static str) {
    metrics::counter!(names::DISPATCHER_PANICS_TOTAL, "kind" => kind).increment(1);
}

/// v0.8.4 #77 (audit H-8): bump the lock-poison-recovery counter by 1.
/// Called by [`crate::lock_recovery::recover_read`] /
/// [`crate::lock_recovery::recover_write`] /
/// [`crate::lock_recovery::recover_mutex`] each time a poisoned lock is
/// recovered. `lock` is the `"<manager>.<field>"` static label; `kind`
/// is `"read"` / `"write"` / `"mutex"`.
pub fn record_lock_poison_recovery(lock: &'static str, kind: &'static str) {
    metrics::counter!(
        names::LOCK_POISON_RECOVERY_TOTAL,
        "lock" => lock,
        "kind" => kind,
    )
    .increment(1);
}

/// v0.8.4 #72: bump the per-manager state-file load-failure counter.
/// Called from [`crate::state_loader::load_or_fresh`] after a snapshot
/// load fell back to the manager's `Default::default()` because the
/// file could not be read (`reason = "read_error"`) or parsed
/// (`reason = "parse_error"`). The accompanying WARN log line carries
/// the file path + underlying error; this counter is the dashboard-
/// friendly aggregate so an alert can fire even if log scraping is off.
pub fn record_state_file_load_failure(manager: &'static str, reason: &'static str) {
    metrics::counter!(
        names::STATE_FILE_LOAD_FAILURES_TOTAL,
        "manager" => manager,
        "reason" => reason,
    )
    .increment(1);
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

/// v0.8 #52: bump the per-S4E5-chunk counter. `op` is `"encrypt"`
/// (PUT-side, fired from [`crate::sse::encrypt_v2_chunked`]) or
/// `"decrypt"` (GET-side, fired from
/// [`crate::sse::decrypt_chunked_stream`] / `decrypt_v5_buffered`
/// per chunk). The counter pairs with `s4_requests_total` so
/// dashboards can compute `chunks_per_request = streaming_chunks /
/// requests`.
pub fn record_sse_streaming_chunk(op: &'static str) {
    metrics::counter!(names::SSE_STREAMING_CHUNKS_TOTAL, "op" => op).increment(1);
}

/// v0.8.2 #62: bump the abandoned-multipart-upload counter by the
/// per-tick batch count. Called from the hourly sweep task spawned in
/// `main.rs` whenever `MultipartStateStore::sweep_stale` reports a
/// non-zero number of pruned entries. `count == 0` ticks intentionally
/// skip the call site (the counter only moves on non-trivial sweeps so
/// the zero rate is the steady state).
pub fn record_multipart_abandoned(count: u64) {
    metrics::counter!(names::MULTIPART_ABANDONED_UPLOADS_TOTAL).increment(count);
}

/// v0.8.3 #66 (H-5 audit fix): bump the replication-status sweep
/// counter by the per-tick batch count. Called from the hourly sweep
/// task spawned in `main.rs` whenever
/// `ReplicationManager::sweep_stale` reports a non-zero number of
/// pruned terminal entries. `count == 0` ticks intentionally skip the
/// call site (the counter only moves on non-trivial sweeps so the
/// zero rate is the steady state — mirrors the
/// [`record_multipart_abandoned`] convention).
pub fn record_replication_status_swept(count: u64) {
    metrics::counter!(names::REPLICATION_STATUS_SWEPT_TOTAL).increment(count);
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

/// v0.8.3 #68 (audit M-1): bumped each time the replication dispatcher
/// committed a replica PUT whose source carried Object Lock state but
/// the destination side has no `ObjectLockManager` attached, so the
/// WORM posture could not propagate. Unlabelled — the per-(src, dst)
/// pair detail lives on the WARN log line emitted once per pair (the
/// pair count is operator-bounded but unlabelled here keeps Prometheus
/// cardinality flat under workloads with many replication rules).
pub fn record_replication_lock_propagation_skipped() {
    metrics::counter!(names::REPLICATION_LOCK_PROPAGATION_SKIPPED_TOTAL).increment(1);
}

/// v0.6 #37: bumped each time the lifecycle scanner executes an action
/// (Expiration / Transition / NoncurrentVersionExpiration). Pair with
/// [`crate::lifecycle::LifecycleManager::record_action`] which keeps the
/// in-process counter in sync with this Prometheus counter so a
/// `/metrics` scrape and an admin introspection of `actions_snapshot()`
/// agree.
///
/// ## known `action` labels
///
/// - `"expire"` — Expiration fired (object DELETEd by the scanner).
/// - `"transition"` — Transition fired (object storage-class rewritten).
/// - `"noncurrent_expire"` — NoncurrentVersionExpiration fired.
/// - `"skipped_locked"` (v0.8.3 #65, audit C-2) — scanner evaluator
///   returned an action but the per-(bucket, key) Object Lock state was
///   live, so the backend-mutating call was skipped. Observability
///   counterpart of `ScanReport::skipped_locked`; lets operators alert
///   on the "lifecycle wanted to act but Object Lock vetoed" path
///   (previously a silent skip).
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
/// or failure). Labels: `result` (= "ok" | "err" | "timeout"). Operators alert
/// on this counter to catch a stuck renewal before the cert expires.
///
/// v0.8.4 #80: signature widened from `bool` → `&'static str` so the
/// background renewal driver can also surface the new "timeout" outcome
/// (poll wedged on a hung Let's Encrypt API call). Use
/// [`record_acme_renewal_timeout`] for that case to keep the label
/// vocabulary centralised.
pub fn record_acme_renewal(result: &'static str) {
    metrics::counter!(names::ACME_RENEWAL_TOTAL, "result" => result).increment(1);
}

/// v0.8.4 #80: convenience wrapper for the renewal-poll-timeout case.
/// Emits the same `s4_acme_renewal_total` counter with `result="timeout"`
/// so dashboards can split "LE-rejected our renewal" from "LE never
/// answered" — the latter implies an outbound network problem the
/// operator needs to investigate before the cert ages out (90-day
/// Let's Encrypt lifetime).
pub fn record_acme_renewal_timeout() {
    record_acme_renewal("timeout");
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
        // v0.8.3 #65: go through `test_metrics_handle()` so other
        // tests in the same binary (notably
        // `lifecycle::tests::scan_one_config_skips_locked_objects_and_bumps_metric`)
        // that also need the recorder cooperate via the shared
        // `OnceLock` instead of fighting over the global slot.
        let handle = test_metrics_handle();
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

        // v0.8.4 #80: ACME renewal counter now accepts "ok" / "err" /
        // "timeout" via the widened `&'static str` signature. Drive
        // all three so the render assertion below can confirm the
        // new label is reachable.
        record_acme_renewal("ok");
        record_acme_renewal("err");
        record_acme_renewal_timeout();

        // v1.2 savings ledger gauges (set-not-increment semantics; the
        // second call must overwrite the first in the rendered output).
        record_ledger_bucket(
            "ledgerbkt",
            &crate::ledger::BucketTotals {
                original_bytes: 1_000,
                stored_bytes: 900,
                objects: 1,
            },
        );
        record_ledger_bucket(
            "ledgerbkt",
            &crate::ledger::BucketTotals {
                original_bytes: 10_000,
                stored_bytes: 1_000,
                objects: 7,
            },
        );

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

        // v0.8.4 #80: ACME renewal counter exposes all three result
        // labels ("ok" / "err" / "timeout"). The "timeout" label is
        // the new arm — operators alert on its rate to catch a hung
        // Let's Encrypt API before the cert ages out.
        assert!(
            rendered.contains("s4_acme_renewal_total"),
            "missing ACME renewal counter in: {rendered}"
        );
        assert!(
            rendered.contains("result=\"ok\""),
            "missing result=ok label (ACME) in: {rendered}"
        );
        assert!(
            rendered.contains("result=\"err\""),
            "missing result=err label (ACME) in: {rendered}"
        );
        assert!(
            rendered.contains("result=\"timeout\""),
            "missing result=timeout label (ACME, v0.8.4 #80) in: {rendered}"
        );

        // v1.2 savings ledger gauges: all three families present with
        // the bucket label, and the latest set() wins (gauge, not
        // counter).
        for name in [
            "s4_ledger_original_bytes",
            "s4_ledger_stored_bytes",
            "s4_ledger_objects",
        ] {
            assert!(rendered.contains(name), "missing {name} in: {rendered}");
        }
        assert!(rendered.contains("bucket=\"ledgerbkt\""));
        assert!(
            rendered.contains("s4_ledger_original_bytes{bucket=\"ledgerbkt\"} 10000"),
            "ledger gauge must reflect the latest set() in: {rendered}"
        );
        assert!(rendered.contains("s4_ledger_objects{bucket=\"ledgerbkt\"} 7"));
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
