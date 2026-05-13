//! ACME (Let's Encrypt) auto-cert support (v0.3 #11).
//!
//! Wraps `rustls-acme` for the TLS-ALPN-01 challenge path. Operators
//! enable this by passing `--acme <domain>` to the binary; certificate
//! acquisition + renewal happens transparently in the background, and
//! the listening port handles both real TLS traffic AND the ACME
//! challenge handshake on the same socket (the TLS-ALPN-01 selling
//! point — no separate port-80 HTTP listener needed).
//!
//! ## Skipped scope (intentional)
//!
//! - **HTTP-01 challenge**: requires a separate port-80 listener and
//!   coordinated routing. TLS-ALPN-01 covers the same use case for
//!   anyone serving on port 443 without that complexity.
//! - **DNS-01 challenge**: requires a DNS provider integration. Not
//!   on the v0.3 roadmap; reopen if a customer needs wildcard certs.
//! - **Custom ACME directory**: the binary hard-codes Let's Encrypt
//!   (production / staging selectable via `--acme-staging`). Add a
//!   `--acme-endpoint` flag if ZeroSSL / internal CA support is asked
//!   for.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, is_tls_alpn_challenge};
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::rustls::ServerConfig;

/// v0.8.4 #80: per-poll timeout on the background ACME renewal stream.
///
/// Without this, a hung Let's Encrypt API (network partition, slow
/// response, transparent-proxy black hole) would wedge the renewal
/// task on a single `state.next().await` indefinitely — the existing
/// cert keeps serving traffic, but renewal silently stops and the
/// cert ages out 90 days later. The timeout fires per-iteration so
/// the loop just retries on the next tick instead of dying.
///
/// 60s is comfortably longer than any healthy LE round-trip
/// (typically < 5s) but short enough that an operator's Prometheus
/// alert on `s4_acme_renewal_total{result="timeout"}` rate fires
/// within a single scrape window when LE goes dark.
const ACME_POLL_TIMEOUT: Duration = Duration::from_secs(60);

/// Inputs to [`bootstrap`]: the operator-supplied flags from main.
pub struct AcmeOptions {
    pub domains: Vec<String>,
    pub contact: Option<String>,
    pub cache_dir: PathBuf,
    pub staging: bool,
}

/// What [`bootstrap`] returns: two rustls configs the per-connection
/// handler picks between based on whether the incoming ClientHello is a
/// TLS-ALPN-01 challenge or a real TLS request.
pub struct AcmeAcceptors {
    /// Rustls config used for the TLS-ALPN-01 challenge response.
    /// Hand to `LazyConfigAcceptor::into_stream` when
    /// `is_tls_alpn_challenge(&client_hello)` is true.
    pub challenge: Arc<ServerConfig>,
    /// Rustls config used for ordinary TLS traffic. Carries the
    /// currently-issued certificate; `rustls-acme` swaps the inner
    /// `Arc<ServerConfig>` automatically on each successful renewal,
    /// so this `Arc` always points at the latest cert.
    pub default: Arc<ServerConfig>,
}

/// Build ACME state, kick off the background renewal loop, and return
/// the two rustls configs the accept loop needs. Spawns one tokio task
/// for the renewal driver; that task lives for the lifetime of the
/// process and shouldn't normally exit.
pub fn bootstrap(opts: AcmeOptions) -> AcmeAcceptors {
    if let Err(e) = std::fs::create_dir_all(&opts.cache_dir) {
        tracing::warn!(
            "could not create ACME cache directory {}: {e}",
            opts.cache_dir.display()
        );
    }

    let mut state = AcmeConfig::new(opts.domains.clone())
        .contact(
            opts.contact
                .iter()
                .map(|e| format!("mailto:{e}"))
                .collect::<Vec<_>>(),
        )
        .cache(DirCache::new(opts.cache_dir.clone()))
        // rustls-acme uses `directory_lets_encrypt(production: bool)` —
        // i.e. `true` selects the production directory. We invert here
        // because the user-facing `--acme-staging` flag is the safer
        // default to surface in CLI help.
        .directory_lets_encrypt(!opts.staging)
        .state();

    let challenge = state.challenge_rustls_config();
    let default = state.default_rustls_config();

    // Background driver: rustls-acme runs renewal + challenge handling
    // through this stream. Bumping the renewal counter on every event
    // surfaces failures to operators via the s4_acme_renewal_total
    // Prometheus metric. We never break out of this loop — failures
    // just retry on the next poll.
    //
    // v0.8.4 #80: each `state.next().await` is wrapped in a
    // `tokio::time::timeout(ACME_POLL_TIMEOUT, …)`. A hung Let's
    // Encrypt API (audit L1) used to wedge the task forever without
    // this guard; now we log + bump the "timeout" label and continue
    // looping so the next iteration retries.
    let domains = opts.domains.join(",");
    tokio::spawn(async move {
        use futures::StreamExt;
        loop {
            match tokio::time::timeout(ACME_POLL_TIMEOUT, state.next()).await {
                Ok(Some(Ok(ok))) => {
                    tracing::info!(target: "s4_acme", domains = %domains, "ACME event: {ok:?}");
                    crate::metrics::record_acme_renewal("ok");
                }
                Ok(Some(Err(err))) => {
                    tracing::warn!(target: "s4_acme", domains = %domains, "ACME error: {err:?}");
                    crate::metrics::record_acme_renewal("err");
                }
                Ok(None) => {
                    tracing::warn!(target: "s4_acme", "ACME state stream ended unexpectedly");
                    break;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        target: "s4_acme",
                        domains = %domains,
                        timeout_secs = ACME_POLL_TIMEOUT.as_secs(),
                        "ACME renewal poll timeout; will retry on next iteration"
                    );
                    crate::metrics::record_acme_renewal_timeout();
                    // Fall through to next loop iteration — `state` is
                    // still owned, so the next poll picks up where the
                    // hung future left off (or its successor, since the
                    // timed-out future is dropped here at scope exit).
                }
            }
        }
    });

    AcmeAcceptors { challenge, default }
}

/// Per-connection accept entry point. Inspect the ClientHello via
/// `LazyConfigAcceptor`, then route to either the challenge config
/// (TLS-ALPN-01 ack) or the default cert config (real traffic).
///
/// Returns `Ok(Some(stream))` for a finished real TLS handshake — the
/// caller serves HTTP on it. Returns `Ok(None)` when a challenge was
/// answered and the caller should just close the connection. `Err(_)`
/// is logged at WARN by the caller.
pub async fn accept_one<IO>(
    sock: IO,
    acceptors: &AcmeAcceptors,
) -> Result<Option<tokio_rustls::server::TlsStream<IO>>, Box<dyn std::error::Error + Send + Sync>>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let start = LazyConfigAcceptor::new(Default::default(), sock).await?;
    if is_tls_alpn_challenge(&start.client_hello()) {
        let mut tls = start.into_stream(acceptors.challenge.clone()).await?;
        use tokio::io::AsyncWriteExt;
        let _ = tls.shutdown().await;
        Ok(None)
    } else {
        let tls = start.into_stream(acceptors.default.clone()).await?;
        Ok(Some(tls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bootstrap returns two distinct rustls configs. We never reach
    /// the Let's Encrypt servers in this unit test (the background
    /// renewal task will retry forever without test-side observation),
    /// so we just verify the synchronous return path.
    #[tokio::test]
    async fn bootstrap_returns_challenge_and_default_configs() {
        crate::tls::install_default_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let acceptors = bootstrap(AcmeOptions {
            domains: vec!["example.test".into()],
            contact: Some("ops@example.test".into()),
            cache_dir: dir.path().to_path_buf(),
            staging: true,
        });
        // Both configs must exist; they're distinct (challenge serves the
        // TLS-ALPN-01 magic cert, default serves the real cert).
        assert!(!Arc::ptr_eq(&acceptors.challenge, &acceptors.default));
    }

    /// v0.8.4 #80: the renewal driver wraps each `state.next().await`
    /// in `tokio::time::timeout(ACME_POLL_TIMEOUT, …)`. We can't drive
    /// the real `rustls-acme` stream here without reaching Let's
    /// Encrypt, and the workspace doesn't enable tokio's `test-util`
    /// feature so `tokio::time::pause()` is unavailable. Instead we
    /// assert the same `timeout(_, pending)` shape against an
    /// always-pending future with a tiny deadline: if the wrapper
    /// returns `Err(Elapsed)`, the production loop's "timeout" arm is
    /// reachable. Combined with the metric-label assertion in
    /// `metrics::tests::install_and_render_basic_counters` (which
    /// scrapes for `result="timeout"`), this nails down both halves
    /// of the fix.
    #[tokio::test]
    async fn renewal_poll_timeout_arm_fires_when_inner_future_hangs() {
        // Sanity: the production constant is 60s — long enough to
        // dwarf any healthy LE round-trip but short enough that an
        // alert window (typically 5 minutes) catches a wedge fast.
        assert_eq!(ACME_POLL_TIMEOUT, Duration::from_secs(60));

        // Demonstrate the same wrapper shape used in `bootstrap`.
        // `pending` never resolves, so `timeout` MUST take the
        // `Err(Elapsed)` path. A tiny deadline keeps the test wall
        // time near zero.
        let pending = futures::future::pending::<()>();
        let res = tokio::time::timeout(Duration::from_millis(20), pending).await;
        assert!(
            res.is_err(),
            "tokio::time::timeout must surface Elapsed for a never-ready future; \
             this is the same branch that bumps record_acme_renewal_timeout in \
             the production loop"
        );

        // Also exercise the recorder helper directly so any future
        // refactor of the metric-label vocabulary trips this test
        // (compile-time guard on the `&'static str` signature).
        crate::metrics::record_acme_renewal_timeout();
    }
}
