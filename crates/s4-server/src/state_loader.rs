//! v0.8.4 #72 — load manager snapshot files with **per-manager fault
//! isolation**.
//!
//! ## Why
//!
//! Pre-#72 each of the nine `--*-state-file` loaders in `main.rs` used
//! the `from_json(&raw).map_err(|e| format!(...))?` pattern: a single
//! corrupted, truncated, or schema-incompatible snapshot would bubble
//! `Err` out of the boot sequence and **kill the gateway start-up**.
//! The operator was forced to either restore the file from backup or
//! manually `rm` it before the gateway would even bind its listener —
//! a loud restart-loop that took the entire data-plane down for one
//! manager's bad JSON.
//!
//! ## What changed
//!
//! [`load_or_fresh`] turns the read-side `Err`/parse-side `Err` into:
//!
//! 1. a `tracing::warn!` log line carrying the manager name, the
//!    file path, and the underlying error (operators grep for
//!    `state file parse failed` in logs);
//! 2. a bump to the
//!    `s4_state_file_load_failures_total{manager,reason}` Prometheus
//!    counter (operators alert on `rate(... > 0)` so silent boot-time
//!    fall-backs surface in dashboards);
//! 3. a fresh `T::default()` manager — the gateway boots with empty
//!    in-memory state for the affected manager and the operator's
//!    snapshot file is **left in place** for post-mortem inspection
//!    (we never touch the operator's bytes — recovering / re-importing
//!    is their call).
//!
//! Every other manager keeps loading normally. One bad file no longer
//! cascades into a gateway-wide DoS.
//!
//! ## What did NOT change
//!
//! - `--mfa-default-secret-file` keeps its **fail-closed** read path.
//!   A missing or unreadable MFA secret means MFA verification cannot
//!   succeed; silently booting with no secret would let DELETEs slip
//!   past the MFA gate. That call site stays inside the MFA loader
//!   block and continues to surface a hard error.
//! - The on-disk snapshot is never deleted, renamed, or rewritten by
//!   the boot path. Operators decide whether to `rm` the bad file or
//!   restore from a known-good copy.

use std::path::Path;

/// Read a `--*-state-file <PATH>` snapshot, returning `Ok(None)` for
/// the three "start fresh" cases and `Ok(Some(json))` for the actual
/// restore-from-snapshot case:
///
/// 1. empty path (`--flag=`)
/// 2. file doesn't exist
/// 3. file exists but is empty / whitespace-only
///
/// The third case used to surface as a `from_json("")` parse error
/// ("EOF while parsing"), which forced operators to hand-write a
/// non-trivial empty-snapshot JSON before the manager would attach.
/// `touch /tmp/foo.json && --flag /tmp/foo.json` is now equivalent to
/// "fresh manager, dump snapshots back here" once the SIGUSR1 hook
/// lands.
///
/// Originally lived in `main.rs` as a binary-private helper (v0.7
/// dogfood follow-up); promoted to the library crate in v0.8.4 #72 so
/// [`load_or_fresh`] can compose it without forcing main.rs to
/// re-export.
pub fn read_state_file_or_fresh(path: &Path) -> Result<Option<String>, std::io::Error> {
    if path.as_os_str().is_empty() || !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(raw))
    }
}

/// v0.8.4 #72: load a manager snapshot with **per-manager graceful
/// degradation**. See module docs for the contract.
///
/// `manager_name` is the static label used in both the `tracing::warn`
/// log and the `s4_state_file_load_failures_total{manager}` Prometheus
/// label — keep it short and stable (e.g. `"versioning"`,
/// `"object_lock"`, `"mfa_delete"`).
///
/// `parse` is the manager's `from_json` constructor: a `FnOnce(&str)
/// -> Result<T, serde_json::Error>` pointer / closure that converts a
/// snapshot string into the typed manager. On parse failure the
/// `serde_json::Error` is logged (the operator can grep the file at
/// `path` for the exact byte offset) and the function returns
/// `T::default()`.
///
/// `T: Default` is enforced because every snapshot-loaded manager in
/// the gateway has a meaningful "empty in-memory state" — that's
/// precisely the boot state operators would have hit if they had not
/// passed `--*-state-file` at all.
pub fn load_or_fresh<T, F>(manager_name: &'static str, path: &Path, parse: F) -> T
where
    T: Default,
    F: FnOnce(&str) -> Result<T, serde_json::Error>,
{
    let raw = match read_state_file_or_fresh(path) {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::info!(
                manager = manager_name,
                path = %path.display(),
                "state file missing or empty; starting fresh",
            );
            return T::default();
        }
        Err(e) => {
            tracing::warn!(
                manager = manager_name,
                path = %path.display(),
                error = %e,
                "state file read failed; starting fresh — file left in place for inspection",
            );
            crate::metrics::record_state_file_load_failure(manager_name, "read_error");
            return T::default();
        }
    };
    match parse(&raw) {
        Ok(mgr) => mgr,
        Err(e) => {
            tracing::warn!(
                manager = manager_name,
                path = %path.display(),
                error = %e,
                "state file parse failed (corrupted JSON); starting fresh — file left in place for inspection",
            );
            crate::metrics::record_state_file_load_failure(manager_name, "parse_error");
            T::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Minimal `T: Default + from_json`-shaped manager for the unit
    /// tests below. Mirrors the real managers' API surface (a
    /// `from_json` returning `serde_json::Error` and a `Default`
    /// fresh-state).
    #[derive(Debug, Default, PartialEq, Eq)]
    struct ToyManager {
        items: Vec<String>,
    }

    impl ToyManager {
        fn from_json(s: &str) -> Result<Self, serde_json::Error> {
            let items: Vec<String> = serde_json::from_str(s)?;
            Ok(Self { items })
        }
    }

    #[test]
    fn load_or_fresh_with_valid_json_returns_parsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snap.json");
        std::fs::write(&path, r#"["a","b","c"]"#).expect("write");

        let got: ToyManager = load_or_fresh("toy", &path, ToyManager::from_json);
        assert_eq!(
            got,
            ToyManager {
                items: vec!["a".into(), "b".into(), "c".into()],
            },
            "valid snapshot must round-trip into the typed manager",
        );
    }

    #[test]
    fn load_or_fresh_with_corrupted_json_logs_warn_and_returns_default() {
        // Truncated JSON — the parser will fail with an EOF / syntax
        // error which load_or_fresh must catch and convert into a
        // default manager (NOT propagate as an error).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snap.json");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(br#"{ "broken json"#).expect("write");
        drop(f);
        // Confirm the file actually survives the call (the operator
        // gets the bytes back for inspection / restore from backup).
        let pre_bytes = std::fs::read(&path).expect("pre read");

        let got: ToyManager = load_or_fresh("toy", &path, ToyManager::from_json);
        assert_eq!(
            got,
            ToyManager::default(),
            "corrupted snapshot must fall back to T::default(), not propagate Err",
        );

        let post_bytes = std::fs::read(&path).expect("post read");
        assert_eq!(
            pre_bytes, post_bytes,
            "the operator's snapshot bytes MUST be left untouched on parse failure",
        );
    }

    #[test]
    fn load_or_fresh_with_missing_file_returns_default() {
        // Path that explicitly does not exist — read_state_file_or_fresh
        // returns Ok(None) so we hit the "info! + default" branch (not
        // the "warn! + bump metric" branch).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");

        let got: ToyManager = load_or_fresh("toy", &path, ToyManager::from_json);
        assert_eq!(
            got,
            ToyManager::default(),
            "missing snapshot must fall back to T::default()",
        );
    }

    #[test]
    fn load_or_fresh_with_empty_file_returns_default() {
        // touch <PATH> then load — read_state_file_or_fresh returns
        // Ok(None) for whitespace-only files; load_or_fresh must NOT
        // hand the empty string to the parser (which would return
        // "EOF while parsing").
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.json");
        std::fs::write(&path, "   \n  \t\n").expect("write");

        let got: ToyManager = load_or_fresh("toy", &path, ToyManager::from_json);
        assert_eq!(got, ToyManager::default());
    }

    #[test]
    fn read_state_file_or_fresh_normalises_empty_path() {
        // Empty `--flag=` is parsed by clap as a Path of `""`.
        let raw = read_state_file_or_fresh(Path::new("")).expect("ok");
        assert!(raw.is_none(), "empty path must surface as Ok(None)");
    }
}
