//! v0.6 #38: Bucket CORS configuration + preflight matcher.
//!
//! S4-server に bucket-level CORS の **own state** を持たせる module。これまで
//! `get_bucket_cors` / `put_bucket_cors` / `delete_bucket_cors` は backend (s3s
//! framework) への passthrough だったが、本 module で S4 自身が
//!
//! - per-bucket の [`CorsConfig`] (= ordered list of [`CorsRule`])
//! - rule 評価器 (S3 仕様準拠: 宣言順で先頭マッチ採用)
//!
//! を所有する。`crates/s4-server/src/service.rs` の CORS handler が `S4Service`
//! 経由で [`CorsManager`] を呼び出して、AWS S3 wire-compat な PutBucketCors /
//! GetBucketCors / DeleteBucketCors の振る舞いを実現する。
//!
//! ## scope (v0.6 #38)
//!
//! - in-memory only (single instance scope)。multi-instance replication は
//!   将来 issue で扱う
//! - `to_json` / `from_json` で snapshot を取る API は提供する。`main.rs` 側で
//!   `--cors-state-file` flag で起動時に snapshot を load できる
//! - **OPTIONS preflight routing は本 task の scope 外**。s3s framework は
//!   OPTIONS verb を専用 handler として持たないため、実際の HTTP-level
//!   preflight 応答 (Access-Control-Allow-* header の組み立て) は `routing.rs`
//!   側で hyper-util listener intercept として wire する follow-up が必要。
//!   本 module は match 評価エンジン (= [`CorsManager::match_preflight`]) と、
//!   service.rs から呼べる public method ([`crate::S4Service::handle_preflight`])
//!   を提供するところまで
//!
//! ## semantics
//!
//! - **rule 評価順序**: AWS S3 は rule を **宣言順** で評価し、最初にマッチ
//!   した rule を採用する。同一 bucket に対する PutBucketCors は configuration
//!   全体を **置き換える** (上書き)、partial update は無し
//! - **wildcard `*`**: origin / method / header いずれも `*` 単独で「任意」を意
//!   味する (S3 は per-component 部分マッチ wildcard `https://*.example.com` は
//!   サポートしない — `*` は「すべて」のみ)。ただし AWS docs の最新版では
//!   `https://*.example.com` 形式も受け付けるとあるので、本実装でも `*` を
//!   single-segment glob として扱える [`matches_glob`] を提供する
//! - **origin matching**: case-sensitive (scheme + host + port は RFC 6454 で
//!   ASCII-lowercase 正規化対象だが、S3 は client が送ってきた string をその
//!   ままバイト比較する仕様)
//! - **method matching**: 大文字必須 (HTTP verb は uppercase)、exact match
//! - **header matching**: case-insensitive (HTTP header name は RFC 7230 で
//!   case-insensitive)

use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// v0.8.15 M-3: validation errors surfaced by [`CorsManager::validate`].
#[derive(Debug, thiserror::Error)]
pub enum CorsValidationError {
    #[error(
        "AllowedMethod {0:?} is not a valid AWS S3 CORS verb (must be one of GET / PUT / POST / DELETE / HEAD; `*` is rejected)"
    )]
    UnsupportedMethod(String),
}

/// 1つの CORS rule。AWS S3 `CORSRule` element に対応する。
///
/// `id` は rule の human-readable label (operator が trace 用に付ける)。
/// `expose_headers` はレスポンスに含まれる header 名 — preflight ではなく
/// **actual response** で使われる (`Access-Control-Expose-Headers`)。
/// `max_age_seconds` は browser 側 preflight cache TTL。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsRule {
    /// `"*"` for any origin, or exact origin string like
    /// `"https://example.com"`. Multiple values are evaluated as OR within
    /// this rule.
    pub allowed_origins: Vec<String>,
    /// Uppercase HTTP verbs: `"GET"`, `"PUT"`, `"POST"`, `"DELETE"`,
    /// `"HEAD"`. AWS S3 only allows this set; we don't validate (caller
    /// is responsible).
    pub allowed_methods: Vec<String>,
    /// `"*"` or specific header names. Matched case-insensitively against
    /// `Access-Control-Request-Headers` from the preflight request.
    pub allowed_headers: Vec<String>,
    /// Header names to expose in the actual response via
    /// `Access-Control-Expose-Headers`. Empty = no header.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// `Access-Control-Max-Age` value (browser preflight cache TTL).
    /// `None` = header omitted.
    #[serde(default)]
    pub max_age_seconds: Option<u32>,
    /// Optional rule identifier (operator-supplied label).
    #[serde(default)]
    pub id: Option<String>,
}

/// Per-bucket CORS configuration (ordered list of rules).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsConfig {
    /// Rules in declaration order — first match wins (S3 spec).
    pub rules: Vec<CorsRule>,
}

/// snapshot のシリアライズ format。`to_json` / `from_json` 用。
#[derive(Debug, Default, Serialize, Deserialize)]
struct CorsSnapshot {
    by_bucket: HashMap<String, CorsConfig>,
}

/// per-bucket CORS configuration を一元管理する manager。
///
/// すべての書き込み (`put` / `delete`) は `RwLock` write 経由で atomic、
/// すべての読み出し (`get` / `match_preflight`) は read 経由で `CorsConfig`
/// の clone (or 派生 `CorsRule` の clone) を返す。
#[derive(Debug, Default)]
pub struct CorsManager {
    by_bucket: RwLock<HashMap<String, CorsConfig>>,
}

impl CorsManager {
    /// 空 manager。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `put_bucket_cors` handler から呼ぶ。bucket の既存 configuration は
    /// **完全に置き換える** (S3 spec: PutBucketCors は upsert ではなく replace)。
    pub fn put(&self, bucket: &str, config: CorsConfig) {
        crate::lock_recovery::recover_write(&self.by_bucket, "cors.by_bucket")
            .insert(bucket.to_owned(), config);
    }

    /// v0.8.15 M-3: validate a `CorsConfig` against the AWS S3 spec
    /// before persisting. Returns the typed validation error so the
    /// listener can surface `InvalidArgument` (mirrors what AWS S3
    /// does at PutBucketCors time, instead of silently accepting a
    /// non-compliant rule and returning a 404-shaped preflight
    /// behaviour later).
    ///
    /// Current rules:
    ///
    /// - `AllowedMethods` ⊆ `{GET, PUT, POST, DELETE, HEAD}`. AWS S3
    ///   rejects every other verb and the `*` wildcard.
    pub fn validate(config: &CorsConfig) -> Result<(), CorsValidationError> {
        const VALID_METHODS: &[&str] = &["GET", "PUT", "POST", "DELETE", "HEAD"];
        for rule in &config.rules {
            for m in &rule.allowed_methods {
                if !VALID_METHODS.contains(&m.as_str()) {
                    return Err(CorsValidationError::UnsupportedMethod(m.clone()));
                }
            }
        }
        Ok(())
    }

    /// `get_bucket_cors` handler から呼ぶ。configuration が無ければ `None`
    /// (handler 側で `NoSuchCORSConfiguration` 404 を返す材料)。
    #[must_use]
    pub fn get(&self, bucket: &str) -> Option<CorsConfig> {
        crate::lock_recovery::recover_read(&self.by_bucket, "cors.by_bucket")
            .get(bucket)
            .cloned()
    }

    /// `delete_bucket_cors` handler から呼ぶ。bucket が無くても idempotent。
    pub fn delete(&self, bucket: &str) {
        crate::lock_recovery::recover_write(&self.by_bucket, "cors.by_bucket").remove(bucket);
    }

    /// snapshot を JSON 文字列にして返す。`--cors-state-file` 経路で
    /// 起動時 dump-load を将来 wire するための hook。
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let snap = CorsSnapshot {
            by_bucket: crate::lock_recovery::recover_read(&self.by_bucket, "cors.by_bucket")
                .clone(),
        };
        serde_json::to_string(&snap)
    }

    /// snapshot JSON から restore。起動時に `--cors-state-file` を読み込む
    /// 経路で使える。
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: CorsSnapshot = serde_json::from_str(s)?;
        Ok(Self {
            by_bucket: RwLock::new(snap.by_bucket),
        })
    }

    /// CORS preflight (OPTIONS) request を bucket の rule list に対して評価
    /// する。S3 仕様通り、宣言順で **最初にマッチした rule** を返す。マッチ
    /// しない / bucket に config が無い場合は `None`。
    ///
    /// rule マッチ条件 (AND):
    /// 1. `origin` が `rule.allowed_origins` のどれか 1 つに [`matches_glob`] でマッチ
    /// 2. `method` (uppercase) が `rule.allowed_methods` の exact-match 1 つに含まれる
    /// 3. `request_headers` の **全要素** が `rule.allowed_headers` のいずれかに [`matches_glob`] (case-insensitive) でマッチ
    #[must_use]
    pub fn match_preflight(
        &self,
        bucket: &str,
        origin: &str,
        method: &str,
        request_headers: &[String],
    ) -> Option<CorsRule> {
        let map = crate::lock_recovery::recover_read(&self.by_bucket, "cors.by_bucket");
        let cfg = map.get(bucket)?;
        for rule in &cfg.rules {
            if !rule_matches_origin(rule, origin) {
                continue;
            }
            if !rule_matches_method(rule, method) {
                continue;
            }
            if !rule_matches_headers(rule, request_headers) {
                continue;
            }
            return Some(rule.clone());
        }
        None
    }
}

fn rule_matches_origin(rule: &CorsRule, origin: &str) -> bool {
    rule.allowed_origins
        .iter()
        .any(|pat| matches_glob(pat, origin))
}

fn rule_matches_method(rule: &CorsRule, method: &str) -> bool {
    // HTTP verbs are case-sensitive uppercase; we still tolerate the
    // wildcard pattern but otherwise require exact match.
    rule.allowed_methods
        .iter()
        .any(|pat| pat == "*" || pat == method)
}

fn rule_matches_headers(rule: &CorsRule, request_headers: &[String]) -> bool {
    if request_headers.is_empty() {
        return true;
    }
    request_headers.iter().all(|h| {
        rule.allowed_headers
            .iter()
            .any(|pat| matches_glob_ci(pat, h))
    })
}

/// AWS S3 CORS の `*` matching。
///
/// - `pattern == "*"` → 任意の `candidate` にマッチ (true)
/// - それ以外は **exact byte equality** で比較
///
/// AWS docs は `https://*.example.com` 形式も受け付けるとあるが、`*` は
/// segment 単位ではなく「全体のいずれか 1 つ」として S3 上で動くケースが
/// 大半なので、本実装は wildcard を `*` 単独 token に限定する。case
/// sensitivity は呼び出し側で制御 (origin は case-sensitive、header は
/// [`matches_glob_ci`] 経由で case-insensitive)。
#[must_use]
pub fn matches_glob(pattern: &str, candidate: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    pattern == candidate
}

/// case-insensitive 版の [`matches_glob`]。HTTP header name 用。
#[must_use]
pub fn matches_glob_ci(pattern: &str, candidate: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    pattern.eq_ignore_ascii_case(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(origins: &[&str], methods: &[&str], headers: &[&str]) -> CorsRule {
        CorsRule {
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            allowed_headers: headers.iter().map(|s| (*s).to_owned()).collect(),
            expose_headers: Vec::new(),
            max_age_seconds: Some(3600),
            id: None,
        }
    }

    #[test]
    fn matches_glob_wildcard_matches_anything() {
        assert!(matches_glob("*", "https://example.com"));
        assert!(matches_glob("*", ""));
        assert!(matches_glob("*", "GET"));
    }

    #[test]
    fn matches_glob_exact_match() {
        assert!(matches_glob("https://example.com", "https://example.com"));
        assert!(matches_glob("GET", "GET"));
    }

    #[test]
    fn matches_glob_no_match() {
        assert!(!matches_glob("https://example.com", "https://evil.com"));
        assert!(!matches_glob("GET", "PUT"));
    }

    #[test]
    fn matches_glob_origin_is_case_sensitive() {
        // S3 origin matching is case-sensitive byte equality.
        assert!(!matches_glob("https://Example.com", "https://example.com"));
    }

    #[test]
    fn matches_glob_ci_header_is_case_insensitive() {
        assert!(matches_glob_ci("Content-Type", "content-type"));
        assert!(matches_glob_ci("X-Amz-Date", "x-amz-date"));
        assert!(!matches_glob_ci("X-Other", "X-Different"));
    }

    #[test]
    fn match_preflight_happy_path() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(
                    &["https://app.example.com"],
                    &["GET", "PUT"],
                    &["Content-Type"],
                )],
            },
        );
        let m = mgr.match_preflight(
            "b",
            "https://app.example.com",
            "PUT",
            &["Content-Type".to_owned()],
        );
        assert!(m.is_some());
        let rule = m.unwrap();
        assert_eq!(rule.max_age_seconds, Some(3600));
    }

    #[test]
    fn match_preflight_no_rule_for_bucket() {
        let mgr = CorsManager::new();
        let m = mgr.match_preflight("ghost", "https://anything", "GET", &[]);
        assert!(m.is_none());
    }

    #[test]
    fn match_preflight_method_not_allowed() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["*"], &["GET"], &["*"])],
            },
        );
        // Rule allows GET only — DELETE preflight must miss.
        assert!(
            mgr.match_preflight("b", "https://x", "DELETE", &[])
                .is_none()
        );
        // Sanity: GET still matches.
        assert!(mgr.match_preflight("b", "https://x", "GET", &[]).is_some());
    }

    #[test]
    fn match_preflight_origin_not_allowed() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["https://good.example.com"], &["GET"], &["*"])],
            },
        );
        assert!(
            mgr.match_preflight("b", "https://evil.example.com", "GET", &[])
                .is_none()
        );
    }

    #[test]
    fn match_preflight_wildcard_origin() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["*"], &["GET"], &[])],
            },
        );
        let m = mgr.match_preflight("b", "https://anywhere", "GET", &[]);
        assert!(m.is_some());
    }

    #[test]
    fn match_preflight_wildcard_header() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["*"], &["PUT"], &["*"])],
            },
        );
        let m = mgr.match_preflight(
            "b",
            "https://x",
            "PUT",
            &["X-Custom-Header".to_owned(), "Content-Type".to_owned()],
        );
        assert!(m.is_some());
    }

    #[test]
    fn match_preflight_first_matching_rule_wins() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![
                    CorsRule {
                        allowed_origins: vec!["*".into()],
                        allowed_methods: vec!["GET".into()],
                        allowed_headers: vec!["*".into()],
                        expose_headers: Vec::new(),
                        max_age_seconds: Some(60),
                        id: Some("first".into()),
                    },
                    CorsRule {
                        allowed_origins: vec!["*".into()],
                        allowed_methods: vec!["GET".into()],
                        allowed_headers: vec!["*".into()],
                        expose_headers: Vec::new(),
                        max_age_seconds: Some(7200),
                        id: Some("second".into()),
                    },
                ],
            },
        );
        let m = mgr
            .match_preflight("b", "https://x", "GET", &[])
            .expect("should match");
        // First-match-wins: shorter max_age_seconds, id="first".
        assert_eq!(m.id.as_deref(), Some("first"));
        assert_eq!(m.max_age_seconds, Some(60));
    }

    #[test]
    fn match_preflight_header_case_insensitive() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["*"], &["PUT"], &["Content-Type"])],
            },
        );
        // request header sent in lowercase — must still match the
        // CamelCase pattern (HTTP header names are case-insensitive).
        let m = mgr.match_preflight("b", "https://x", "PUT", &["content-type".to_owned()]);
        assert!(m.is_some());
    }

    #[test]
    fn put_replaces_previous_config() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["https://a"], &["GET"], &["*"])],
            },
        );
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["https://b"], &["PUT"], &["*"])],
            },
        );
        let cfg = mgr.get("b").expect("config present");
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].allowed_origins, vec!["https://b".to_string()]);
    }

    #[test]
    fn delete_is_idempotent() {
        let mgr = CorsManager::new();
        mgr.delete("never-existed"); // must not panic
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["*"], &["GET"], &[])],
            },
        );
        mgr.delete("b");
        assert!(mgr.get("b").is_none());
    }

    #[test]
    fn json_round_trip() {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![CorsRule {
                    allowed_origins: vec!["https://example.com".into()],
                    allowed_methods: vec!["GET".into(), "PUT".into()],
                    allowed_headers: vec!["Content-Type".into()],
                    expose_headers: vec!["ETag".into()],
                    max_age_seconds: Some(3600),
                    id: Some("rule-1".into()),
                }],
            },
        );
        let json = mgr.to_json().expect("to_json");
        let mgr2 = CorsManager::from_json(&json).expect("from_json");
        assert_eq!(mgr.get("b"), mgr2.get("b"));
    }

    /// v0.8.4 #77 (audit H-8): a panic inside the `by_bucket` write
    /// guard poisons the lock. `to_json` must recover via
    /// [`crate::lock_recovery::recover_read`] and surface the data
    /// instead of re-panicking on the SIGUSR1 dump-back path.
    #[test]
    fn cors_to_json_after_panic_recovers_via_poison() {
        let mgr = std::sync::Arc::new(CorsManager::new());
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["*"], &["GET"], &[])],
            },
        );
        let mgr_cl = std::sync::Arc::clone(&mgr);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = mgr_cl.by_bucket.write().expect("clean lock");
            g.entry("b2".into()).or_default();
            panic!("force-poison");
        }));
        assert!(
            mgr.by_bucket.is_poisoned(),
            "write panic must poison by_bucket lock"
        );
        let json = mgr.to_json().expect("to_json after poison must succeed");
        let mgr2 = CorsManager::from_json(&json).expect("from_json");
        assert!(
            mgr2.get("b").is_some(),
            "recovered snapshot keeps original config"
        );
    }
}
