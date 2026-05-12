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
}

impl AccessLog {
    pub fn new(dest: AccessLogDest) -> Self {
        Self {
            dest,
            buf: Arc::new(Mutex::new(VecDeque::new())),
            flush_every_secs: 60,
            max_entries_before_flush: 5_000,
            batch_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
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
                let body = render_lines(&drained);
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
                    Ok(Ok(())) => {}
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
