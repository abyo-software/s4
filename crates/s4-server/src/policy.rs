//! Bucket policy / IAM enforcement at the gateway (v0.2 #7).
//!
//! Loads a subset of AWS bucket policy JSON and evaluates incoming requests
//! against it before delegating to the backend. Out of scope for v0.2:
//! full IAM Conditions, STS / AssumeRole chains, cross-account delegation,
//! resource-based ACLs.
//!
//! Supported AWS S3 actions:
//! - `s3:GetObject`
//! - `s3:PutObject`
//! - `s3:DeleteObject`
//! - `s3:ListBucket`
//! - `s3:*` (wildcard, matches all of the above)
//! - `*` (wildcard, matches everything)
//!
//! Supported Resource patterns (case-sensitive):
//! - `arn:aws:s3:::<bucket>` — bucket-level ops (ListBucket etc.)
//! - `arn:aws:s3:::<bucket>/<key>` — object-level ops
//! - Trailing or interior `*` glob in the key portion
//! - `arn:aws:s3:::*` — any bucket / any key
//!
//! Supported Principal forms:
//! - `"Principal": "*"` — anyone authenticated by S4's auth layer
//! - `"Principal": {"AWS": ["AKIA...", "AKIA..."]}` — match by SigV4 access
//!   key ID. (Full IAM user/role ARN matching is a future extension once
//!   STS integration lands.)
//!
//! Decision: **explicit Deny > explicit Allow > implicit Deny** — the
//! standard AWS evaluation order.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StringOrVec {
    Single(String),
    Many(Vec<String>),
}

impl StringOrVec {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(s) => vec![s],
            Self::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PrincipalSet {
    /// `"Principal": "*"` — JSON string form. The string content is
    /// untyped (only the string variant matters), so we accept any but
    /// don't read the value.
    Wildcard(#[allow(dead_code)] String),
    Map {
        #[serde(rename = "AWS", default)]
        aws: Option<StringOrVec>,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct StatementJson {
    #[serde(rename = "Sid")]
    sid: Option<String>,
    #[serde(rename = "Effect")]
    effect: Effect,
    #[serde(rename = "Action")]
    action: StringOrVec,
    #[serde(rename = "Resource")]
    resource: StringOrVec,
    #[serde(rename = "Principal", default)]
    principal: Option<PrincipalSet>,
}

#[derive(Debug, Clone, Deserialize)]
struct PolicyJson {
    #[serde(rename = "Version")]
    _version: Option<String>,
    #[serde(rename = "Statement")]
    statements: Vec<StatementJson>,
}

/// Compiled bucket policy ready to evaluate requests.
#[derive(Debug, Clone)]
pub struct Policy {
    statements: Vec<Statement>,
}

#[derive(Debug, Clone)]
struct Statement {
    sid: Option<String>,
    effect: Effect,
    actions: Vec<String>,   // `s3:GetObject`, `s3:*`, `*`
    resources: Vec<String>, // `arn:aws:s3:::bucket/key*`
    /// `None` = no Principal field = match anyone (for resource-attached
    /// bucket policies the convention is to require Principal, but for our
    /// gateway we treat absence as "any authenticated caller").
    /// `Some(vec![])` after parsing wildcard "*" = same effect.
    /// `Some(vec!["AKIA..."])` = match those access key ids.
    /// An empty `principals` vector means "wildcard (any principal)".
    principals: Option<Vec<String>>,
}

impl Policy {
    pub fn from_json_str(s: &str) -> Result<Self, String> {
        let raw: PolicyJson =
            serde_json::from_str(s).map_err(|e| format!("policy JSON parse error: {e}"))?;
        let mut statements = Vec::with_capacity(raw.statements.len());
        for s in raw.statements {
            statements.push(Statement {
                sid: s.sid,
                effect: s.effect,
                actions: s.action.into_vec(),
                resources: s.resource.into_vec(),
                principals: s.principal.map(|p| match p {
                    PrincipalSet::Wildcard(_) => Vec::new(),
                    PrincipalSet::Map { aws } => aws.map(|v| v.into_vec()).unwrap_or_default(),
                }),
            });
        }
        Ok(Self { statements })
    }

    pub fn from_path(path: &Path) -> Result<Self, String> {
        let txt = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::from_json_str(&txt)
    }

    /// Evaluate a request against the policy.
    ///
    /// `principal_id` is typically the SigV4 access key id taken from the
    /// authenticated request. Pass `None` for anonymous (will only match
    /// statements with wildcard or absent Principal).
    pub fn evaluate(
        &self,
        action: &str,
        bucket: &str,
        key: Option<&str>,
        principal_id: Option<&str>,
    ) -> Decision {
        let object_resource = match key {
            Some(k) => format!("arn:aws:s3:::{bucket}/{k}"),
            None => format!("arn:aws:s3:::{bucket}"),
        };
        let bucket_resource = format!("arn:aws:s3:::{bucket}");

        let mut matched_allow: Option<Option<String>> = None;
        let mut matched_deny: Option<Option<String>> = None;

        for st in &self.statements {
            if !st.actions.iter().any(|p| action_matches(p, action)) {
                continue;
            }
            let any_resource_matches = st.resources.iter().any(|p| {
                resource_matches(p, &object_resource) || resource_matches(p, &bucket_resource)
            });
            if !any_resource_matches {
                continue;
            }
            if !principal_matches(&st.principals, principal_id) {
                continue;
            }
            match st.effect {
                Effect::Deny => {
                    matched_deny = Some(st.sid.clone());
                    // Any explicit Deny wins; no need to keep scanning, but
                    // continue so the matched Sid reflects the LAST matching
                    // Deny (deterministic for telemetry).
                }
                Effect::Allow => {
                    if matched_allow.is_none() {
                        matched_allow = Some(st.sid.clone());
                    }
                }
            }
        }

        if let Some(sid) = matched_deny {
            Decision::deny(sid)
        } else if let Some(sid) = matched_allow {
            Decision::allow(sid)
        } else {
            Decision::implicit_deny()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    pub matched_sid: Option<String>,
    /// `None` = implicit deny (no statement matched), `Some(Allow|Deny)` =
    /// explicit decision.
    pub matched_effect: Option<Effect>,
}

impl Decision {
    fn allow(sid: Option<String>) -> Self {
        Self {
            allow: true,
            matched_sid: sid,
            matched_effect: Some(Effect::Allow),
        }
    }
    fn deny(sid: Option<String>) -> Self {
        Self {
            allow: false,
            matched_sid: sid,
            matched_effect: Some(Effect::Deny),
        }
    }
    fn implicit_deny() -> Self {
        Self {
            allow: false,
            matched_sid: None,
            matched_effect: None,
        }
    }
}

/// Match an action pattern against a concrete action.
/// Patterns: `*`, `s3:*`, `s3:GetObject`. Case-sensitive (AWS is too).
fn action_matches(pattern: &str, action: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(":*") {
        return action.starts_with(prefix) && action[prefix.len()..].starts_with(':');
    }
    pattern == action
}

/// Match a resource ARN pattern against a concrete resource ARN. Supports
/// `*` and `?` glob characters.
fn resource_matches(pattern: &str, resource: &str) -> bool {
    glob_match(pattern, resource)
}

/// Hand-rolled glob (`*` = any sequence, `?` = any single char) so we don't
/// pull in the `globset` crate for a single use site.
fn glob_match(pattern: &str, s: &str) -> bool {
    let p_bytes = pattern.as_bytes();
    let s_bytes = s.as_bytes();
    glob_match_bytes(p_bytes, s_bytes)
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

fn principal_matches(allowed: &Option<Vec<String>>, principal_id: Option<&str>) -> bool {
    match allowed {
        // No Principal field on the statement → match any caller (incl. anonymous).
        None => true,
        Some(list) if list.is_empty() => true,
        Some(list) => match principal_id {
            None => false,
            Some(id) => list.iter().any(|p| p == "*" || p == id),
        },
    }
}

/// Wrap a Policy in an Arc so cloning the S4Service stays cheap.
pub type SharedPolicy = Arc<Policy>;

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Policy {
        Policy::from_json_str(s).expect("policy")
    }

    #[test]
    fn allow_then_deny_explicit_deny_wins() {
        let pol = p(r#"{
            "Version": "2012-10-17",
            "Statement": [
              {"Sid": "AllowAll", "Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::b/*"},
              {"Sid": "DenyDelete", "Effect": "Deny", "Action": "s3:DeleteObject", "Resource": "arn:aws:s3:::b/*"}
            ]
        }"#);
        let d = pol.evaluate("s3:GetObject", "b", Some("k"), None);
        assert!(d.allow);
        assert_eq!(d.matched_sid.as_deref(), Some("AllowAll"));
        let d = pol.evaluate("s3:DeleteObject", "b", Some("k"), None);
        assert!(!d.allow);
        assert_eq!(d.matched_effect, Some(Effect::Deny));
        assert_eq!(d.matched_sid.as_deref(), Some("DenyDelete"));
    }

    #[test]
    fn implicit_deny_when_no_statement_matches() {
        let pol = p(r#"{
            "Version": "2012-10-17",
            "Statement": [
              {"Effect": "Allow", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::other/*"}
            ]
        }"#);
        let d = pol.evaluate("s3:GetObject", "mine", Some("k"), None);
        assert!(!d.allow);
        assert_eq!(d.matched_effect, None);
    }

    #[test]
    fn resource_glob_matches_prefix() {
        let pol = p(r#"{
            "Version": "2012-10-17",
            "Statement": [{
              "Effect": "Allow",
              "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/data/*.parquet"
            }]
        }"#);
        assert!(
            pol.evaluate("s3:GetObject", "b", Some("data/foo.parquet"), None)
                .allow
        );
        assert!(
            pol.evaluate("s3:GetObject", "b", Some("data/sub/bar.parquet"), None)
                .allow
        );
        assert!(
            !pol.evaluate("s3:GetObject", "b", Some("data/foo.txt"), None)
                .allow
        );
    }

    #[test]
    fn s3_action_wildcard() {
        let pol = p(r#"{
            "Version": "2012-10-17",
            "Statement": [{"Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::*"}]
        }"#);
        assert!(pol.evaluate("s3:GetObject", "any", Some("k"), None).allow);
        assert!(pol.evaluate("s3:PutObject", "any", Some("k"), None).allow);
        // Non-s3 action would not match (we don't generate any non-s3 actions
        // from S4Service handlers, but verify the matcher behaves correctly)
        assert!(!pol.evaluate("iam:ListUsers", "any", None, None).allow);
    }

    #[test]
    fn principal_match_by_access_key_id() {
        let pol = p(r#"{
            "Version": "2012-10-17",
            "Statement": [{
              "Effect": "Allow",
              "Action": "s3:*",
              "Resource": "arn:aws:s3:::b/*",
              "Principal": {"AWS": ["AKIATEST123"]}
            }]
        }"#);
        assert!(
            pol.evaluate("s3:GetObject", "b", Some("k"), Some("AKIATEST123"))
                .allow
        );
        assert!(
            !pol.evaluate("s3:GetObject", "b", Some("k"), Some("AKIAOTHER"))
                .allow
        );
        assert!(!pol.evaluate("s3:GetObject", "b", Some("k"), None).allow);
    }

    #[test]
    fn principal_wildcard_matches_anyone() {
        let pol = p(r#"{
            "Version": "2012-10-17",
            "Statement": [{
              "Effect": "Allow",
              "Action": "s3:*",
              "Resource": "arn:aws:s3:::b/*",
              "Principal": "*"
            }]
        }"#);
        assert!(
            pol.evaluate("s3:GetObject", "b", Some("k"), Some("AKIAANY"))
                .allow
        );
        assert!(pol.evaluate("s3:GetObject", "b", Some("k"), None).allow);
    }

    #[test]
    fn resource_can_be_string_or_array() {
        let single = p(r#"{
            "Statement": [{"Effect": "Allow", "Action": "s3:GetObject",
                          "Resource": "arn:aws:s3:::a/*"}]
        }"#);
        let multi = p(r#"{
            "Statement": [{"Effect": "Allow", "Action": "s3:GetObject",
                          "Resource": ["arn:aws:s3:::a/*", "arn:aws:s3:::b/*"]}]
        }"#);
        assert!(single.evaluate("s3:GetObject", "a", Some("k"), None).allow);
        assert!(!single.evaluate("s3:GetObject", "b", Some("k"), None).allow);
        assert!(multi.evaluate("s3:GetObject", "b", Some("k"), None).allow);
    }

    #[test]
    fn bucket_level_resource_for_listbucket() {
        let pol = p(r#"{
            "Statement": [{"Effect": "Allow", "Action": "s3:ListBucket",
                          "Resource": "arn:aws:s3:::b"}]
        }"#);
        // ListBucket uses a key=None resource, formatted as bucket-only ARN
        assert!(pol.evaluate("s3:ListBucket", "b", None, None).allow);
        assert!(!pol.evaluate("s3:ListBucket", "other", None, None).allow);
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("foo", "foo"));
        assert!(!glob_match("foo", "bar"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("foo*", "foobar"));
        assert!(glob_match("*bar", "foobar"));
        assert!(glob_match("foo*bar", "fooXYZbar"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("a*b*c", "axxxbyyyc"));
    }
}
