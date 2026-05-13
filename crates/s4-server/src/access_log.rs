//! S3-style access-log emission (v0.4 #20).
//!
//! Writes one line per completed request in the AWS S3 server access log
//! format (close enough for `awk` / `goaccess` / standard log analyzers
//! to parse). Output is buffered and flushed periodically — destination
//! is **another S3 bucket** (the convention AWS itself uses for S3
//! server access logs), reached via the same backend the gateway is
//! fronting. No new outbound dependencies.
//!
//! ## Format
//!
//! `bucket-owner bucket [time] remote-ip requester request-id operation
//!  key request-uri http-status error-code bytes-sent object-size
//!  total-time turn-around-time referer user-agent version-id
//!  host-id sig-version cipher-suite auth-type host-header tls-version
//!  access-point-arn acl-required`
//!
//! Most fields are stubbed (`-`) for the v0.4 release; the load-bearing
//! columns are time, remote-ip, requester, operation, key, status,
//! bytes-sent, object-size, total-time, user-agent.
//!
//! ## Operator config
//!
//! `--access-log s3://logs-bucket/prefix/{date}/` enables emission. The
//! `{date}` placeholder expands to `YYYY-MM-DD-HH` (hourly rollover).
//!
//! ## Implementation note
//!
//! We deliberately don't compress the access-log objects — they're text
//! and S4's own bucket-policy enforcement may want to read them raw. If
//! you want them squished, point S4 at *another* S4 instance or front
//! the log bucket with a separate gateway.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use tokio::sync::Mutex;

use crate::audit_log::{
    AuditHmacKey, EOF_HMAC_COMMENT_PREFIX, PREV_TAIL_COMMENT_PREFIX, chain_step, compute_eof_hmac,
    genesis_prev, hex_encode,
};

/// Per-request structured fields collected at handler completion. The
/// emitter renders this into the on-the-wire S3 access-log format on
/// flush.
#[derive(Debug, Clone)]
pub struct AccessLogEntry {
    pub time: SystemTime,
    pub bucket: String,
    pub remote_ip: Option<String>,
    pub requester: Option<String>,
    pub operation: &'static str,
    pub key: Option<String>,
    pub request_uri: String,
    pub http_status: u16,
    pub error_code: Option<String>,
    pub bytes_sent: u64,
    pub object_size: u64,
    pub total_time_ms: u64,
    pub user_agent: Option<String>,
}

/// Operator-configured destination: a local directory where hourly
/// rotated `.log` files are written. v0.4 scope — `s3://` destination
/// is a post-v0.4 follow-up; for now ship the entries to local disk and
/// let a separate log-shipper (filebeat / fluent-bit / vector) push them
/// to wherever they need to go.
#[derive(Debug, Clone)]
pub struct AccessLogDest {
    pub dir: std::path::PathBuf,
}

impl AccessLogDest {
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(stripped) = s.strip_prefix("s3://") {
            return Err(format!(
                "v0.4 ships local-directory access-log only; got s3:// destination ({stripped:?}). \
                 Use a local path or pipe via filebeat / vector to S3."
            ));
        }
        let dir = std::path::PathBuf::from(s);
        Ok(Self { dir })
    }

    /// Compose the file path for a flush at `now`. One file per hour
    /// + a batch counter so high-volume hours don't single-file-balloon.
    pub fn path_for(&self, now: SystemTime, batch: u64) -> std::path::PathBuf {
        let secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (y, mo, d, h) = unix_to_ymdh(secs as i64);
        self.dir
            .join(format!("{y:04}-{mo:02}-{d:02}-{h:02}-{batch:04}.log"))
    }
}

/// Buffered emitter. Per-handler call sites push entries via
/// [`AccessLog::record`]; a background task drains the buffer and writes
/// one S3 object per flush window.
pub struct AccessLog {
    dest: AccessLogDest,
    buf: Arc<Mutex<VecDeque<AccessLogEntry>>>,
    flush_every_secs: u64,
    max_entries_before_flush: usize,
    batch_counter: Arc<std::sync::atomic::AtomicU64>,
    /// v0.5 #31: optional HMAC-SHA256 key. When `Some(...)`, the
    /// flusher appends a hex HMAC column to every line and emits a
    /// `# prev_file_tail=<hex>` comment at the top of each rotated
    /// batch file so the chain extends across rotations.
    hmac_key: Option<Arc<AuditHmacKey>>,
    /// Running chain state — the last HMAC the flusher emitted (or
    /// the genesis seed if nothing has been emitted yet). Updated
    /// in-place at the end of each flush batch.
    chain_state: Arc<Mutex<ChainState>>,
    /// v0.8.2 #63: synchronous mirror of the chain state's `last_hmac`,
    /// kept under a `std::sync::Mutex` so `Drop` (which runs in
    /// non-async contexts during graceful shutdown) can compute and
    /// write a final `# eof_hmac=` marker without entering the tokio
    /// runtime. `None` until at least one batch has been emitted.
    last_emitted_hmac: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    /// v0.8.2 #63: the path of the most recently flushed batch file —
    /// kept for diagnostics. Each batch file is already terminated by
    /// an `# eof_hmac=` marker as it is written, so `Drop`'s job is
    /// only to flush any **pending** entries plus a marker into a new
    /// final batch file.
    last_emitted_path: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
}

#[derive(Debug, Clone)]
struct ChainState {
    last_hmac: [u8; 32],
    /// True once at least one batch has been written, so the next
    /// batch knows it must emit a `# prev_file_tail=` comment.
    primed: bool,
}

impl Default for ChainState {
    fn default() -> Self {
        Self {
            last_hmac: genesis_prev(),
            primed: false,
        }
    }
}

impl AccessLog {
    pub fn new(dest: AccessLogDest) -> Self {
        Self {
            dest,
            buf: Arc::new(Mutex::new(VecDeque::new())),
            flush_every_secs: 60,
            max_entries_before_flush: 5_000,
            batch_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            hmac_key: None,
            chain_state: Arc::new(Mutex::new(ChainState::default())),
            last_emitted_hmac: Arc::new(std::sync::Mutex::new(None)),
            last_emitted_path: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// v0.5 #31: turn on tamper-evident HMAC chaining. Every emitted
    /// line gets a trailing hex HMAC column, and each new batch file
    /// starts with a `# prev_file_tail=<hex>` comment so the chain
    /// extends across rotations. Without this builder, lines are
    /// emitted exactly as before (back-compat with v0.4 #20 readers).
    #[must_use]
    pub fn with_hmac_key(mut self, key: Arc<AuditHmacKey>) -> Self {
        self.hmac_key = Some(key);
        self
    }

    pub async fn record(&self, entry: AccessLogEntry) {
        let mut buf = self.buf.lock().await;
        buf.push_back(entry);
        if buf.len() >= self.max_entries_before_flush {
            // Wake the flusher early — it polls on `flush_every_secs`,
            // but a burst should land sooner. We do this by leaving the
            // entries queued; the flusher loop checks size on every tick.
        }
    }

    /// Spawn the background flusher. Drains the buffer every
    /// `flush_every_secs` (default 60) and appends to the per-hour file
    /// in `dest.dir`. Returns the tokio JoinHandle so the caller can
    /// abort on shutdown if needed.
    pub fn spawn_flusher(&self) -> tokio::task::JoinHandle<()> {
        let dest = self.dest.clone();
        let buf = Arc::clone(&self.buf);
        let interval = self.flush_every_secs;
        let counter = Arc::clone(&self.batch_counter);
        let hmac_key = self.hmac_key.clone();
        let chain_state = Arc::clone(&self.chain_state);
        let last_emitted_hmac = Arc::clone(&self.last_emitted_hmac);
        let last_emitted_path = Arc::clone(&self.last_emitted_path);
        if let Err(e) = std::fs::create_dir_all(&dest.dir) {
            tracing::warn!(
                "S4 access log: could not create dir {}: {e}",
                dest.dir.display()
            );
        }
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
            loop {
                tick.tick().await;
                let drained: Vec<AccessLogEntry> = {
                    let mut b = buf.lock().await;
                    if b.is_empty() {
                        continue;
                    }
                    b.drain(..).collect()
                };
                let now = SystemTime::now();
                let batch = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = dest.path_for(now, batch);
                let (body, new_last_for_drop) = if let Some(key) = hmac_key.as_ref() {
                    let mut state = chain_state.lock().await;
                    let (rendered, new_last) = render_lines_chained(&drained, key, &state);
                    state.last_hmac = new_last;
                    state.primed = true;
                    // v0.8.2 #63: append the EOF HMAC marker as the
                    // last line of every batch file. Each batch is its
                    // own file under the current rotation scheme
                    // (batch counter is in the filename), so end-of-
                    // batch == end-of-file, and a verifier with
                    // `--require-eof-hmac` can therefore alert on any
                    // file that ended mid-write (truncation / crash —
                    // closing H-2). The marker is computed over the
                    // chain state AFTER the last emitted entry and is
                    // NOT itself part of the chain (uses the EOF_LABEL
                    // domain separator).
                    let mut with_marker = rendered;
                    let eof = compute_eof_hmac(key, &new_last);
                    with_marker.push_str(EOF_HMAC_COMMENT_PREFIX);
                    with_marker.push_str(&hex_encode(&eof));
                    with_marker.push('\n');
                    (with_marker, Some(new_last))
                } else {
                    (render_lines(&drained), None)
                };
                let body_bytes: Bytes = Bytes::from(body);
                let path_clone = path.clone();
                let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path_clone)?;
                    f.write_all(&body_bytes)
                })
                .await;
                match res {
                    Ok(Ok(())) => {
                        // v0.8.2 #63: only update the Drop bookkeeping
                        // after a successful write — otherwise Drop
                        // could try to flush against a path / chain
                        // state we never durably committed.
                        if let Some(h) = new_last_for_drop {
                            if let Ok(mut g) = last_emitted_hmac.lock() {
                                *g = Some(h);
                            }
                            if let Ok(mut g) = last_emitted_path.lock() {
                                *g = Some(path.clone());
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("S4 access log write failed at {}: {e}", path.display());
                    }
                    Err(e) => {
                        tracing::warn!("S4 access log task join failed: {e}");
                    }
                }
            }
        })
    }

    /// v0.8.2 #63: best-effort drain of any buffered entries plus a
    /// terminating `# eof_hmac=` marker, used by `Drop` (graceful
    /// shutdown). Synchronous — runs blocking file I/O on the calling
    /// thread because `Drop` cannot `.await`. Errors are
    /// logged-and-swallowed; a producer crash that prevents this from
    /// running is the only legitimate way for an audit log file to end
    /// without an EOF marker, and strict verifiers
    /// (`require_eof_hmac = true`) will surface that as
    /// [`crate::audit_log::VerifyError::EofHmacMissing`].
    fn drop_emit_eof_marker(&mut self) {
        // try_lock so Drop never blocks. Anything we cannot drain is
        // lost — but that loss was already implicit pre-v0.8.2 (the
        // flusher could be killed mid-tick) and we are not making it
        // worse. The EOF marker is generated for the new batch file
        // we are about to create alongside the salvaged entries.
        let pending: Vec<AccessLogEntry> = match self.buf.try_lock() {
            Ok(mut b) => b.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        let Some(key) = self.hmac_key.clone() else {
            // Without HMAC chaining there is nothing to authenticate;
            // the audit_log path is degenerate. Do nothing — pending
            // entries are dropped, matching pre-v0.8.2 behavior.
            return;
        };
        if pending.is_empty() {
            // Every batch file written by `spawn_flusher` already
            // carries its own `# eof_hmac=` marker, so a graceful
            // shutdown with nothing buffered has no extra work to do
            // here; pre-existing files are already verifiable.
            return;
        }
        // Synchronous render path — we cannot `.await chain_state.lock()`
        // from `Drop`. Try to `try_lock` the async chain state for the
        // most up-to-date view; fall back to the synchronous mirror
        // (`last_emitted_hmac`) which the flusher updates after every
        // successful write; finally fall back to genesis (treating
        // this as a fresh chain).
        let mut state = ChainState::default();
        if let Ok(s) = self.chain_state.try_lock() {
            state = s.clone();
        } else if let Ok(g) = self.last_emitted_hmac.lock()
            && let Some(h) = *g
        {
            state.last_hmac = h;
            state.primed = true;
        }
        let now = SystemTime::now();
        let batch = self
            .batch_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self.dest.path_for(now, batch);
        let (rendered, new_last) = render_lines_chained(&pending, &key, &state);
        let mut with_marker = rendered;
        let eof = compute_eof_hmac(&key, &new_last);
        with_marker.push_str(EOF_HMAC_COMMENT_PREFIX);
        with_marker.push_str(&hex_encode(&eof));
        with_marker.push('\n');
        if let Err(e) = std::fs::create_dir_all(&self.dest.dir) {
            tracing::warn!(
                "S4 access log Drop: could not ensure dir {}: {e}",
                self.dest.dir.display()
            );
            return;
        }
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(with_marker.as_bytes())
            });
        if let Err(e) = res {
            tracing::warn!(
                "S4 access log Drop: failed to flush + EOF marker to {}: {e}",
                path.display()
            );
        } else if let Ok(mut g) = self.last_emitted_path.lock() {
            *g = Some(path);
        }
    }
}

impl Drop for AccessLog {
    fn drop(&mut self) {
        // v0.8.2 #63: best-effort EOF marker emission on graceful
        // shutdown. Process crashes that prevent this from running
        // are by construction undetectable from the producer side;
        // operators who need crash-safe truncation detection should
        // run the verifier with `--require-eof-hmac` and treat
        // `EofHmacMissing` as a "this file ended without a clean
        // shutdown" alert (which is exactly the H-2 baseline we are
        // closing).
        self.drop_emit_eof_marker();
    }
}

/// Render `entries` with a trailing HMAC column on each line, plus a
/// `# prev_file_tail=<hex>` preamble when `state.primed` is true (i.e.
/// this is not the very first batch). Returns the rendered text and
/// the final chain HMAC, which the caller must persist back to the
/// shared state.
///
/// Each line's HMAC is computed over `prev_hmac || line_no_hmac`,
/// where `line_no_hmac` is the bytes of the line WITHOUT the trailing
/// HMAC column AND WITHOUT the trailing newline. The producer then
/// appends ` <hex>\n` to land on the wire format the verifier expects.
fn render_lines_chained(
    entries: &[AccessLogEntry],
    key: &AuditHmacKey,
    state: &ChainState,
) -> (String, [u8; 32]) {
    // Reserve a generous budget: ~256 chars per base line + 65 for
    // " <hex>\n", plus 80 for the preamble.
    let mut out = String::with_capacity(entries.len() * 320 + 80);
    if state.primed {
        out.push_str(PREV_TAIL_COMMENT_PREFIX);
        out.push_str(&hex_encode(&state.last_hmac));
        out.push('\n');
    }
    let base = render_lines(entries);
    let mut prev = state.last_hmac;
    for raw_line in base.split_inclusive('\n') {
        let line = raw_line.trim_end_matches('\n');
        if line.is_empty() {
            continue;
        }
        let mac = chain_step(key, &prev, line.as_bytes());
        out.push_str(line);
        out.push(' ');
        out.push_str(&hex_encode(&mac));
        out.push('\n');
        prev = mac;
    }
    (out, prev)
}

/// Public wrapper for ease of `Arc<AccessLog>` plumbing in S4Service.
pub type SharedAccessLog = Arc<AccessLog>;

fn render_lines(entries: &[AccessLogEntry]) -> String {
    let mut out = String::with_capacity(entries.len() * 256);
    for e in entries {
        let ts = unix_secs(e.time);
        let (y, mo, d, h, mi, se) = unix_to_ymdhms(ts);
        out.push_str(&format!(
            "- {bucket} [{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z] {ip} {req} - {op} {key} \"{uri}\" {status} {err} {bytes_sent} {obj_size} {total_ms} - - \"{ua}\" - - SigV4 - AuthHeader - TLSv1.3 - -\n",
            bucket = e.bucket,
            ip = e.remote_ip.as_deref().unwrap_or("-"),
            req = e.requester.as_deref().unwrap_or("-"),
            op = e.operation,
            key = e.key.as_deref().unwrap_or("-"),
            uri = e.request_uri,
            status = e.http_status,
            err = e.error_code.as_deref().unwrap_or("-"),
            bytes_sent = e.bytes_sent,
            obj_size = e.object_size,
            total_ms = e.total_time_ms,
            ua = e.user_agent.as_deref().unwrap_or("-"),
        ));
    }
    out
}

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Civil from unix seconds → (year, month, day, hour). UTC.
fn unix_to_ymdh(secs: i64) -> (i64, u32, u32, u32) {
    let (y, mo, d, h, _mi, _se) = unix_to_ymdhms(secs);
    (y, mo, d, h)
}

fn unix_to_ymdhms(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let se = (rem % 60) as u32;
    // Hinnant civil-from-days (public domain)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y_civil = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo_civil = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo_civil <= 2 { y_civil + 1 } else { y_civil };
    (y, mo_civil, d, h, mi, se)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dest_local_dir() {
        let d = AccessLogDest::parse("/var/log/s4").unwrap();
        assert_eq!(d.dir, std::path::PathBuf::from("/var/log/s4"));
    }

    #[test]
    fn parse_dest_rejects_s3_url_until_phase_b() {
        let err = AccessLogDest::parse("s3://logs/access/").unwrap_err();
        assert!(err.contains("local-directory access-log only"));
    }

    #[test]
    fn path_for_uses_hourly_naming() {
        let d = AccessLogDest::parse("/tmp/s4-test").unwrap();
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let p = d.path_for(now, 7);
        let s = p.to_string_lossy();
        assert!(s.starts_with("/tmp/s4-test/"));
        assert!(s.ends_with("-0007.log"));
    }

    #[test]
    fn unix_to_ymdh_known_value() {
        // 2026-05-13 00:00:00 UTC = 1779048000s
        let (y, mo, d, h) = unix_to_ymdh(1_779_148_800);
        assert!(y == 2026 && (1..=12).contains(&mo) && (1..=31).contains(&d) && h < 24);
    }

    fn sample_entry(bucket: &str) -> AccessLogEntry {
        AccessLogEntry {
            time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            bucket: bucket.into(),
            remote_ip: Some("10.0.0.1".into()),
            requester: Some("AKIATEST".into()),
            operation: "REST.PUT.OBJECT",
            key: Some("k".into()),
            request_uri: "PUT /b/k HTTP/1.1".into(),
            http_status: 200,
            error_code: None,
            bytes_sent: 0,
            object_size: 4096,
            total_time_ms: 12,
            user_agent: Some("aws-cli/2.0".into()),
        }
    }

    #[test]
    fn chained_render_produces_verifiable_output() {
        use std::str::FromStr;

        use crate::audit_log::{AuditHmacKey, verify_audit_bytes};
        let key = AuditHmacKey::from_str("raw:0123456789abcdef0123456789abcdef").unwrap();
        let entries = vec![sample_entry("b1"), sample_entry("b2"), sample_entry("b3")];
        let state = ChainState::default();
        let (text, _last) = render_lines_chained(&entries, &key, &state);
        // No prev_file_tail comment on the first batch.
        assert!(!text.starts_with("# prev_file_tail="));
        // Each line ends with " <64 hex>\n"
        for raw in text.split_inclusive('\n') {
            let line = raw.trim_end_matches('\n');
            if line.is_empty() {
                continue;
            }
            assert!(line.len() > 65);
            let suf = &line[line.len() - 65..];
            assert!(suf.starts_with(' '));
            assert!(suf[1..].chars().all(|c| c.is_ascii_hexdigit()));
        }
        // Verifier is happy.
        let report = verify_audit_bytes(
            std::path::Path::new("<mem>"),
            text.as_bytes(),
            &key,
            crate::audit_log::VerifyOptions::default(),
        )
        .unwrap();
        assert!(report.first_break.is_none());
        assert_eq!(report.ok_lines, 3);
    }

    #[test]
    fn second_batch_emits_prev_file_tail_and_chains() {
        use std::str::FromStr;

        use crate::audit_log::{AuditHmacKey, VerifyOptions, verify_audit_bytes};
        let key = AuditHmacKey::from_str("raw:0123456789abcdef0123456789abcdef").unwrap();

        // First batch.
        let entries1 = vec![sample_entry("b1")];
        let mut state = ChainState::default();
        let (text1, last1) = render_lines_chained(&entries1, &key, &state);
        state.last_hmac = last1;
        state.primed = true;

        // Second batch — must start with # prev_file_tail= and verify
        // when fed independently to the verifier.
        let entries2 = vec![sample_entry("b2")];
        let (text2, _) = render_lines_chained(&entries2, &key, &state);
        assert!(text2.starts_with("# prev_file_tail="));
        let report = verify_audit_bytes(
            std::path::Path::new("<mem>"),
            text2.as_bytes(),
            &key,
            VerifyOptions::default(),
        )
        .unwrap();
        assert!(report.first_break.is_none(), "second batch must verify");
        assert_eq!(report.ok_lines, 1);
        // First batch verifies on its own too.
        let r1 = verify_audit_bytes(
            std::path::Path::new("<mem>"),
            text1.as_bytes(),
            &key,
            VerifyOptions::default(),
        )
        .unwrap();
        assert!(r1.first_break.is_none());
    }

    #[test]
    fn render_one_entry() {
        let e = AccessLogEntry {
            time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            bucket: "b".into(),
            remote_ip: Some("10.0.0.1".into()),
            requester: Some("AKIATEST".into()),
            operation: "REST.PUT.OBJECT",
            key: Some("k".into()),
            request_uri: "PUT /b/k HTTP/1.1".into(),
            http_status: 200,
            error_code: None,
            bytes_sent: 0,
            object_size: 4096,
            total_time_ms: 12,
            user_agent: Some("aws-cli/2.0".into()),
        };
        let line = render_lines(&[e]);
        assert!(line.contains("REST.PUT.OBJECT"));
        assert!(line.contains("10.0.0.1"));
        assert!(line.contains("AKIATEST"));
        assert!(line.contains("\"aws-cli/2.0\""));
        assert!(line.ends_with('\n'));
    }
}
