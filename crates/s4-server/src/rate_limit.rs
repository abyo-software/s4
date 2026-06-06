//! Per-(principal, bucket) token-bucket rate limiting (v0.4 #19).
//!
//! Operators describe the rules in JSON:
//!
//! ```json
//! [
//!   {"principal": "AKIATENANT_A", "bucket": "tenant-a-*", "rps": 100, "burst": 500},
//!   {"principal": "*",            "bucket": "*",          "rps":  20, "burst":  60}
//! ]
//! ```
//!
//! Match precedence is **most-specific-first** by walk order — the JSON
//! file's order is preserved, so put narrow rules above wildcards. Wildcards
//! are simple `*` glob (any sequence) only; `?` is also accepted.
//!
//! On each PUT / GET / DELETE / List, the matching rule's bucket consumes
//! one token. If the bucket is empty the request is rejected with
//! `S3ErrorCode::SlowDown` (HTTP 503; AWS-spec response for "you're
//! making requests faster than I can handle"). The
//! `s4_rate_limit_throttled_total{principal,bucket}` Prometheus counter is
//! bumped on every reject.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use serde::Deserialize;

use crate::policy; // re-use the glob_match helper if exposed; otherwise inline below

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    /// `*` for any principal.
    pub principal: String,
    /// `*` for any bucket.
    pub bucket: String,
    /// Sustained requests per second.
    pub rps: u32,
    /// Initial / replenished bucket size.
    pub burst: u32,
}

/// Compiled per-(principal, bucket) limiter pool. Rules are evaluated in
/// the order they appear in the JSON; the first match wins.
#[derive(Clone)]
pub struct RateLimits {
    rules: Arc<Vec<Rule>>,
    /// Per-(rule index, principal, bucket) limiters. Created lazily —
    /// the first request from a given principal/bucket pair instantiates
    /// the limiter, subsequent requests reuse it.
    limiters: Arc<DashMap<(usize, String, String), Arc<KeyLimiter>>>,
    /// v0.8.12 HIGH-11 fix: per-rule shared limiter used as the
    /// "overflow" fallback once `limiters` is at capacity. Lazily
    /// initialised per rule on first overflow so steady-state
    /// workloads under the cap don't allocate.
    overflow: Arc<DashMap<usize, Arc<KeyLimiter>>>,
}

/// v0.8.12 HIGH-11 fix: hard cap on the per-(rule, principal, bucket)
/// limiter pool. Without this, every distinct access-key-id /
/// bucket-name combo a (potentially attacker-controlled) request
/// stream presents allocates a fresh `Arc<KeyLimiter>` + a key tuple
/// that's never reclaimed. At 1k req/s of unique fake principals the
/// pool grows by ~3.6M entries/hour, each pinning ~200 B of memory
/// → gateway OOMs in a single day. Once the pool fills, new keys
/// fold onto a per-rule shared `overflow` limiter — they still get
/// rate-limited (correctly, by the matching rule's quota), just
/// share one bucket with every other overflowed key. Operators
/// who legitimately need millions of distinct principals can raise
/// the cap via `with_max_active_limiters(...)`; the default is a
/// trade between memory budget (~3 MiB at this cap) and the realistic
/// upper bound of distinct principals × buckets in a steady-state
/// workload.
pub const DEFAULT_MAX_ACTIVE_LIMITERS: usize = 16_384;

type KeyLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

impl RateLimits {
    pub fn from_json_str(s: &str) -> Result<Self, String> {
        let rules: Vec<Rule> =
            serde_json::from_str(s).map_err(|e| format!("rate-limit JSON parse error: {e}"))?;
        for r in &rules {
            if r.rps == 0 || r.burst == 0 {
                return Err(format!(
                    "rate-limit rule has rps=0 or burst=0 (would deny everything): {r:?}"
                ));
            }
        }
        Ok(Self {
            rules: Arc::new(rules),
            limiters: Arc::new(DashMap::new()),
            overflow: Arc::new(DashMap::new()),
        })
    }

    /// v0.8.12 HIGH-11 fix: current per-(rule, principal, bucket)
    /// limiter count. Surfaced for tests and the
    /// `rate_limit::active_limiters` Prometheus gauge.
    pub fn active_limiter_count(&self) -> usize {
        self.limiters.len()
    }

    pub fn from_path(path: &Path) -> Result<Self, String> {
        let txt = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::from_json_str(&txt)
    }

    /// Returns `true` if the request passes the limiter, `false` if
    /// throttled. `principal_id` may be `None` (anonymous); rules with
    /// `"principal": "*"` still match.
    pub fn check(&self, principal_id: Option<&str>, bucket: &str) -> bool {
        let principal = principal_id.unwrap_or("");
        for (idx, rule) in self.rules.iter().enumerate() {
            if !glob_match(&rule.principal, principal) {
                continue;
            }
            if !glob_match(&rule.bucket, bucket) {
                continue;
            }
            // v0.8.12 HIGH-11 fix: if the per-key pool is already at
            // the cap AND we'd be inserting a new entry, fold this
            // request onto the rule's shared `overflow` limiter
            // instead. Pool growth is bounded by
            // `DEFAULT_MAX_ACTIVE_LIMITERS` regardless of how many
            // distinct principal / bucket strings the request stream
            // presents.
            let key = (idx, principal.to_owned(), bucket.to_owned());
            let limiter = if let Some(existing) = self.limiters.get(&key) {
                existing.clone()
            } else if self.limiters.len() >= DEFAULT_MAX_ACTIVE_LIMITERS {
                self.overflow
                    .entry(idx)
                    .or_insert_with(|| Self::build_limiter(rule))
                    .clone()
            } else {
                self.limiters
                    .entry(key)
                    .or_insert_with(|| Self::build_limiter(rule))
                    .clone()
            };
            return limiter.check().is_ok();
        }
        // No rule matched → no limit applies.
        true
    }

    /// Helper: build a fresh `KeyLimiter` from a `Rule`'s `rps` /
    /// `burst`. Pulled out so the cap-driven overflow fallback and
    /// the normal-path `or_insert_with` share one construction
    /// recipe.
    fn build_limiter(rule: &Rule) -> Arc<KeyLimiter> {
        let burst = NonZeroU32::new(rule.burst).expect("burst > 0 (validated)");
        let rps = NonZeroU32::new(rule.rps).expect("rps > 0 (validated)");
        let quota = Quota::per_second(rps).allow_burst(burst);
        Arc::new(RateLimiter::direct(quota))
    }
}

impl std::fmt::Debug for RateLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimits")
            .field("rules", &self.rules.len())
            .field("active_limiters", &self.limiters.len())
            .finish()
    }
}

pub type SharedRateLimits = Arc<RateLimits>;

/// Local minimal glob — same semantics as policy::glob_match but
/// re-exposed here so we don't have to expose internals from `policy`.
/// `*` = any sequence, `?` = any single char. Case-sensitive.
fn glob_match(pattern: &str, s: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), s.as_bytes())
}

fn glob_match_bytes(p: &[u8], s: &[u8]) -> bool {
    let mut pi = 0;
    let mut si = 0;
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, si));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// Touch a policy item to keep the import live (otherwise unused-imports fires
// without changing visibility); we use the same matching shape on purpose.
#[allow(dead_code)]
fn _link() -> Option<policy::Effect> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rl(s: &str) -> RateLimits {
        RateLimits::from_json_str(s).expect("rate-limit parse")
    }

    #[test]
    fn parse_rejects_zero_rps_or_burst() {
        let err = RateLimits::from_json_str(
            r#"[{"principal": "*", "bucket": "*", "rps": 0, "burst": 10}]"#,
        )
        .unwrap_err();
        assert!(err.contains("rps=0"));

        let err = RateLimits::from_json_str(
            r#"[{"principal": "*", "bucket": "*", "rps": 1, "burst": 0}]"#,
        )
        .unwrap_err();
        assert!(err.contains("burst=0"));
    }

    #[test]
    fn match_principal_and_bucket_globs() {
        let r = rl(r#"[
            {"principal": "AKIA*", "bucket": "tenant-a-*", "rps": 1000, "burst": 1000},
            {"principal": "*",     "bucket": "*",          "rps": 1,    "burst": 1}
        ]"#);
        // First rule matches → high quota
        assert!(r.check(Some("AKIATEST"), "tenant-a-foo"));
        // Other principal falls to second rule → 1 token left after first
        assert!(r.check(Some("anonymous"), "any"));
        // Burst exhausted → throttle
        assert!(!r.check(Some("anonymous"), "any"));
    }

    #[test]
    fn no_rule_means_no_limit() {
        let r = rl(r#"[{"principal": "AKIATENANT", "bucket": "*", "rps": 1, "burst": 1}]"#);
        // Different principal → no rule matches → unlimited
        for _ in 0..100 {
            assert!(r.check(Some("AKIAOTHER"), "anything"));
        }
    }

    #[test]
    fn refill_after_wait() {
        let r = rl(r#"[{"principal": "*", "bucket": "*", "rps": 100, "burst": 1}]"#);
        assert!(r.check(None, "b"));
        assert!(!r.check(None, "b"));
        std::thread::sleep(Duration::from_millis(15)); // 100 rps = 1 token / 10 ms
        assert!(r.check(None, "b"));
    }
}
