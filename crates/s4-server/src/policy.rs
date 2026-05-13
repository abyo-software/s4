//! Bucket policy / IAM enforcement at the gateway (v0.2 #7).
//!
//! Loads a subset of AWS bucket policy JSON and evaluates incoming requests
//! against it before delegating to the backend. Out of scope for v0.2:
//! full IAM Conditions, STS / AssumeRole chains, cross-account delegation,
//! resource-based ACLs.
//!
//! Supported AWS S3 actions:
//! - `s3:GetObject` / `s3:PutObject` / `s3:DeleteObject` (object-level)
//! - `s3:GetObjectTagging` / `s3:PutObjectTagging` / `s3:DeleteObjectTagging`
//! - `s3:GetObjectAcl` / `s3:PutObjectAcl`
//! - `s3:RestoreObject` / `s3:GetObjectVersion` / `s3:DeleteObjectVersion`
//! - `s3:GetObjectRetention` / `s3:PutObjectRetention`
//! - `s3:GetObjectLegalHold` / `s3:PutObjectLegalHold`
//! - `s3:BypassGovernanceRetention` / `s3:AbortMultipartUpload`
//! - `s3:ListBucket` / `s3:GetBucketLocation` / `s3:GetBucketAcl` (bucket-level)
//! - `s3:GetBucketCors` / `s3:PutBucketCors` / `s3:DeleteBucketCors`
//! - `s3:GetBucketVersioning` / `s3:PutBucketVersioning`
//! - `s3:GetBucketTagging` / `s3:PutBucketTagging` / `s3:DeleteBucketTagging`
//! - `s3:GetBucketReplication` / `s3:PutBucketReplication` / `s3:DeleteBucketReplication`
//! - `s3:GetBucketLifecycleConfiguration` / `s3:PutBucketLifecycleConfiguration`
//! - `s3:GetBucketNotification` / `s3:PutBucketNotification`
//! - `s3:GetInventoryConfiguration` / `s3:PutInventoryConfiguration`
//! - `s3:GetObjectLockConfiguration` / `s3:PutObjectLockConfiguration`
//! - `s3:CreateBucket` / `s3:DeleteBucket` / `s3:ListMultipartUploads`
//! - `s3:*` (wildcard, matches all of the above)
//! - `*` (wildcard, matches everything)
//!
//! Supported Resource patterns (case-sensitive):
//! - `arn:aws:s3:::<bucket>` — bucket-level ops (ListBucket etc.)
//! - `arn:aws:s3:::<bucket>/<key>` — object-level ops
//! - Trailing or interior `*` glob in the key portion
//! - `arn:aws:s3:::*` — any bucket / any key
//!
//! v0.8.4 #75 (audit H-4): Resource ARN scoping is now namespace-aware. A
//! bucket-form ARN (`arn:aws:s3:::b`) only matches bucket-level actions; an
//! object-form ARN (`arn:aws:s3:::b/k`) only matches object-level actions.
//! Pre-v0.8.4 a single bucket-form ARN with a `s3:GetObject` Action would
//! silently grant the object op — this is now correctly rejected so a
//! mis-typed policy fails closed instead of escalating privilege.
//!
//! Supported Principal forms:
//! - `"Principal": "*"` — anyone authenticated by S4's auth layer. The
//!   string MUST be exactly `"*"` (v0.8.4 #75 / audit H-5); any other
//!   bare-string form (e.g. `"AKIA..."`) is rejected at parse time.
//! - `"Principal": {"AWS": ["AKIA...", "AKIA..."]}` — match by SigV4
//!   access key ID. The list MUST be non-empty (an empty list previously
//!   widened to "match anyone", which is unsafe). `Service`, `Federated`
//!   and `CanonicalUser` Principal types are NOT supported and are
//!   rejected at parse time so silent acceptance can't hide a policy gap.
//!   (Full IAM user/role ARN matching is a future extension once STS
//!   integration lands.)
//!
//! Decision: **explicit Deny > explicit Allow > implicit Deny** — the
//! standard AWS evaluation order.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

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

/// v0.8.4 #75 (audit H-5): Principal set is parsed by hand from
/// `serde_json::Value` so we can reject silently-permissive shapes (a
/// bare string that isn't `"*"`, an empty `{"AWS": []}` list, or any
/// `Service` / `Federated` / `CanonicalUser` Principal type that we
/// don't support).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalSet {
    /// `"Principal": "*"` — match any caller (incl. anonymous / unauth).
    /// The bare string must be exactly `"*"`; any other string form is a
    /// parse error.
    Wildcard,
    /// `"Principal": {"AWS": ["AKIA...", ...]}` — match those access key
    /// ids. Guaranteed non-empty after parse.
    Specific(Vec<String>),
}

impl PrincipalSet {
    /// Parse the JSON value attached to a Statement's `Principal` field.
    pub fn parse(value: &serde_json::Value) -> Result<Self, PolicyParseError> {
        match value {
            serde_json::Value::String(s) if s == "*" => Ok(PrincipalSet::Wildcard),
            serde_json::Value::String(other) => {
                Err(PolicyParseError::InvalidWildcard(other.clone()))
            }
            serde_json::Value::Object(map) => {
                if map.len() != 1 || !map.contains_key("AWS") {
                    return Err(PolicyParseError::UnsupportedPrincipalType);
                }
                let aws = &map["AWS"];
                let principals: Vec<String> = match aws {
                    serde_json::Value::String(s) => vec![s.clone()],
                    serde_json::Value::Array(arr) => {
                        let mut out = Vec::with_capacity(arr.len());
                        for v in arr {
                            match v {
                                serde_json::Value::String(s) => out.push(s.clone()),
                                _ => return Err(PolicyParseError::InvalidPrincipalShape),
                            }
                        }
                        out
                    }
                    _ => return Err(PolicyParseError::InvalidPrincipalShape),
                };
                if principals.is_empty() {
                    return Err(PolicyParseError::EmptyPrincipalList);
                }
                Ok(PrincipalSet::Specific(principals))
            }
            _ => Err(PolicyParseError::InvalidPrincipalShape),
        }
    }
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
    /// Captured as a raw JSON value so [`PrincipalSet::parse`] can apply
    /// the v0.8.4 #75 strict-shape validation (bare `"*"` only, non-empty
    /// `AWS` list, no `Service` / `Federated` / `CanonicalUser`).
    #[serde(rename = "Principal", default)]
    principal: Option<serde_json::Value>,
    /// Optional Condition map (v0.3 #13): operator → key → values.
    /// `{"IpAddress": {"aws:SourceIp": ["10.0.0.0/8"]}, ...}`.
    #[serde(rename = "Condition", default)]
    condition: Option<HashMap<String, HashMap<String, StringOrVec>>>,
}

#[derive(Debug, Clone, Deserialize)]
struct PolicyJson {
    #[serde(rename = "Version")]
    _version: Option<String>,
    #[serde(rename = "Statement")]
    statements: Vec<StatementJson>,
}

/// v0.8.4 #75: a parsed Resource ARN with namespace tag (bucket-form vs
/// object-form). Built once at policy-parse time so statement evaluation
/// can route bucket-only / object-only actions to the right ARN shape
/// without re-parsing on every request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceArn {
    /// `arn:aws:s3:::<bucket>` — matches bucket-level actions only.
    Bucket(String),
    /// `arn:aws:s3:::<bucket>/<key-pattern>` — matches object-level
    /// actions only. `key_pattern` may carry `*` / `?` glob characters.
    Object {
        bucket: String,
        key_pattern: String,
    },
}

/// v0.8.4 #75: which resource ARN shape an Action accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceKind {
    /// Action operates on an object — only `Object{...}` ARNs match.
    ObjectOnly,
    /// Action operates on a bucket — only `Bucket(...)` ARNs match.
    BucketOnly,
    /// Action's namespace is ambiguous (e.g. `s3:*`, or an unknown
    /// forward-compat action) — both ARN forms may match.
    Either,
}

/// v0.8.4 #75: structured parse / validation errors. Surfaced via
/// [`Display`] for the public `Result<_, String>` boundary so existing
/// CLI / test call sites that string-match on the message keep working.
#[derive(Debug, thiserror::Error)]
pub enum PolicyParseError {
    #[error("policy JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "Resource ARN must start with \"arn:aws:s3:::\" — got {0:?}"
    )]
    InvalidResourceArn(String),
    #[error("Resource ARN bucket name is empty: {0:?}")]
    EmptyBucketInArn(String),
    #[error("Principal wildcard must be exact \"*\" — got {0:?}")]
    InvalidWildcard(String),
    #[error(
        "unsupported Principal type (only AWS principals are supported, no Service / Federated / CanonicalUser)"
    )]
    UnsupportedPrincipalType,
    #[error("Principal AWS list must not be empty")]
    EmptyPrincipalList,
    #[error("Principal value must be the string \"*\" or a {{AWS: ...}} object")]
    InvalidPrincipalShape,
    #[error(
        "unsupported policy Condition operator: {op:?}. v0.3 supports IpAddress / NotIpAddress / StringEquals / StringNotEquals / StringLike / StringNotLike / DateGreaterThan / DateLessThan / Bool."
    )]
    UnsupportedConditionOperator { op: String },
}

/// v0.8.4 #75: parse a Resource string into the typed [`ResourceArn`].
/// Accepts `arn:aws:s3:::<bucket>` and `arn:aws:s3:::<bucket>/<key>`
/// (with optional glob chars in `<key>`). The all-buckets wildcard
/// `arn:aws:s3:::*` parses as `Bucket("*")` for the bare form and
/// `Object{bucket="*", ..}` when a slash follows.
pub fn parse_resource_arn(s: &str) -> Result<ResourceArn, PolicyParseError> {
    const PREFIX: &str = "arn:aws:s3:::";
    let rest = s
        .strip_prefix(PREFIX)
        .ok_or_else(|| PolicyParseError::InvalidResourceArn(s.to_owned()))?;
    match rest.split_once('/') {
        None => {
            if rest.is_empty() {
                return Err(PolicyParseError::EmptyBucketInArn(s.to_owned()));
            }
            Ok(ResourceArn::Bucket(rest.to_owned()))
        }
        Some((bucket, key_pattern)) => {
            if bucket.is_empty() {
                return Err(PolicyParseError::EmptyBucketInArn(s.to_owned()));
            }
            Ok(ResourceArn::Object {
                bucket: bucket.to_owned(),
                key_pattern: key_pattern.to_owned(),
            })
        }
    }
}

/// v0.8.4 #75: map an Action verb to the ARN namespace it operates on.
/// Unknown actions resolve to [`ResourceKind::Either`] to stay forward-
/// compatible with future S3 actions a policy author wants to gate
/// today.
fn action_resource_kind(action: &str) -> ResourceKind {
    match action {
        // ----- Object-level -----
        "s3:GetObject"
        | "s3:PutObject"
        | "s3:DeleteObject"
        | "s3:GetObjectTagging"
        | "s3:PutObjectTagging"
        | "s3:DeleteObjectTagging"
        | "s3:GetObjectAcl"
        | "s3:PutObjectAcl"
        | "s3:RestoreObject"
        | "s3:GetObjectVersion"
        | "s3:DeleteObjectVersion"
        | "s3:GetObjectRetention"
        | "s3:PutObjectRetention"
        | "s3:GetObjectLegalHold"
        | "s3:PutObjectLegalHold"
        | "s3:BypassGovernanceRetention"
        | "s3:AbortMultipartUpload" => ResourceKind::ObjectOnly,
        // ----- Bucket-level -----
        "s3:ListBucket"
        | "s3:GetBucketLocation"
        | "s3:GetBucketAcl"
        | "s3:GetBucketCors"
        | "s3:PutBucketCors"
        | "s3:DeleteBucketCors"
        | "s3:GetBucketVersioning"
        | "s3:PutBucketVersioning"
        | "s3:GetBucketTagging"
        | "s3:PutBucketTagging"
        | "s3:DeleteBucketTagging"
        | "s3:GetBucketReplication"
        | "s3:PutBucketReplication"
        | "s3:DeleteBucketReplication"
        | "s3:GetBucketLifecycleConfiguration"
        | "s3:PutBucketLifecycleConfiguration"
        | "s3:GetBucketNotification"
        | "s3:PutBucketNotification"
        | "s3:GetInventoryConfiguration"
        | "s3:PutInventoryConfiguration"
        | "s3:GetObjectLockConfiguration"
        | "s3:PutObjectLockConfiguration"
        | "s3:CreateBucket"
        | "s3:DeleteBucket"
        | "s3:ListMultipartUploads" => ResourceKind::BucketOnly,
        // `s3:*`, `*`, and any unknown verb stay permissive so a policy
        // gating a future action keeps working without a server upgrade.
        _ => ResourceKind::Either,
    }
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
    actions: Vec<String>, // `s3:GetObject`, `s3:*`, `*`
    /// v0.8.4 #75: pre-parsed Resource ARNs, tagged by namespace so
    /// statement evaluation can route bucket-only / object-only Actions
    /// to the correct ARN shape (audit H-4).
    resources: Vec<ResourceArn>,
    /// `None` = no Principal field on the statement → match anyone
    /// (incl. anonymous). `Some(PrincipalSet::Wildcard)` (the literal
    /// `"*"` form) is semantically the same. `Some(Specific(vec))` =
    /// only those access key ids. Validated at parse time by
    /// [`PrincipalSet::parse`] (v0.8.4 #75 / audit H-5).
    principals: Option<PrincipalSet>,
    /// Compiled Condition clauses; empty vec = no condition restriction
    /// (statement always matches once Action / Resource / Principal pass).
    conditions: Vec<Condition>,
}

/// Per-request context fed into the policy evaluator. Caller is expected to
/// fill what's available; missing fields make any Condition that depends on
/// them fail (= statement skipped, never silently allowed).
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub source_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub request_time: Option<SystemTime>,
    pub secure_transport: bool,
    /// v0.6 #39: tags currently attached to the object the request
    /// targets (resolved by the caller via `TagManager` ahead of
    /// `evaluate_with`). Surfaced to policy via the
    /// `s3:ExistingObjectTag/<key>` condition key. `None` here is
    /// treated identically to "no tags exist" — every
    /// `ExistingObjectTag` clause then fails.
    pub existing_object_tags: Option<crate::tagging::TagSet>,
    /// v0.6 #39: tags carried in the *request* itself (PutObject's
    /// `x-amz-tagging` URL-encoded header, or PutObjectTagging's
    /// `Tagging` body). Surfaced to policy via the
    /// `s3:RequestObjectTag/<key>` condition key.
    pub request_object_tags: Option<crate::tagging::TagSet>,
    /// Generic key → value map for any aws:* or s3:* context key not
    /// covered by the typed fields above (keeps the door open for any
    /// key the caller wants to plumb without changing the struct).
    pub extra: HashMap<String, String>,
}

/// One compiled Condition clause inside a Statement.
#[derive(Debug, Clone)]
struct Condition {
    op: ConditionOp,
    key: String,         // e.g. `aws:SourceIp`, `aws:UserAgent`, `aws:CurrentTime`
    values: Vec<String>, // operator-specific (CIDR, glob, ISO-8601 timestamp, "true" / "false", ...)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionOp {
    IpAddress,
    NotIpAddress,
    StringEquals,
    StringNotEquals,
    StringLike,
    StringNotLike,
    DateGreaterThan,
    DateLessThan,
    Bool,
}

impl ConditionOp {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "IpAddress" => Self::IpAddress,
            "NotIpAddress" => Self::NotIpAddress,
            "StringEquals" => Self::StringEquals,
            "StringNotEquals" => Self::StringNotEquals,
            "StringLike" => Self::StringLike,
            "StringNotLike" => Self::StringNotLike,
            "DateGreaterThan" => Self::DateGreaterThan,
            "DateLessThan" => Self::DateLessThan,
            "Bool" => Self::Bool,
            _ => return None,
        })
    }
}

impl Policy {
    /// Parse a JSON bucket policy. Returns the human-readable [`Display`]
    /// of the underlying [`PolicyParseError`] for backward compatibility
    /// with the pre-v0.8.4 `Result<_, String>` callers (CLI flag handler,
    /// existing E2E tests). Use [`Policy::from_json_str_typed`] when you
    /// need to inspect the error variant programmatically.
    pub fn from_json_str(s: &str) -> Result<Self, String> {
        Self::from_json_str_typed(s).map_err(|e| e.to_string())
    }

    /// v0.8.4 #75: typed-error variant of [`Policy::from_json_str`]. Lets
    /// new callers (and the unit tests for audit H-4 / H-5) match on
    /// [`PolicyParseError`] directly instead of grepping a String.
    pub fn from_json_str_typed(s: &str) -> Result<Self, PolicyParseError> {
        let raw: PolicyJson = serde_json::from_str(s)?;
        let mut statements = Vec::with_capacity(raw.statements.len());
        for stmt in raw.statements {
            let mut conditions = Vec::new();
            if let Some(cond_map) = stmt.condition {
                for (op_name, key_map) in cond_map {
                    let op = ConditionOp::parse(&op_name).ok_or(
                        PolicyParseError::UnsupportedConditionOperator {
                            op: op_name.clone(),
                        },
                    )?;
                    for (key, values) in key_map {
                        conditions.push(Condition {
                            op,
                            key,
                            values: values.into_vec(),
                        });
                    }
                }
            }
            // v0.8.4 #75 / audit H-4: pre-parse every Resource ARN here
            // so statement_matches_resource() can dispatch on namespace
            // without re-parsing on the hot path.
            let mut resources = Vec::with_capacity(stmt.resource.clone().into_vec().len());
            for raw_arn in stmt.resource.into_vec() {
                resources.push(parse_resource_arn(&raw_arn)?);
            }
            // v0.8.4 #75 / audit H-5: validate Principal shape strictly.
            let principals = match stmt.principal {
                None => None,
                Some(value) => Some(PrincipalSet::parse(&value)?),
            };
            statements.push(Statement {
                sid: stmt.sid,
                effect: stmt.effect,
                actions: stmt.action.into_vec(),
                resources,
                principals,
                conditions,
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
    ///
    /// Convenience for the common case with no Condition data; calls the
    /// full [`Policy::evaluate_with`] with a default `RequestContext`.
    pub fn evaluate(
        &self,
        action: &str,
        bucket: &str,
        key: Option<&str>,
        principal_id: Option<&str>,
    ) -> Decision {
        self.evaluate_with(
            action,
            bucket,
            key,
            principal_id,
            &RequestContext::default(),
        )
    }

    /// Same as [`Policy::evaluate`] but lets the caller plumb a populated
    /// [`RequestContext`] for v0.3 #13 IAM Conditions (IP allowlists,
    /// user-agent restrictions, time windows, etc.).
    pub fn evaluate_with(
        &self,
        action: &str,
        bucket: &str,
        key: Option<&str>,
        principal_id: Option<&str>,
        ctx: &RequestContext,
    ) -> Decision {
        let mut matched_allow: Option<Option<String>> = None;
        let mut matched_deny: Option<Option<String>> = None;

        for st in &self.statements {
            if !st.actions.iter().any(|p| action_matches(p, action)) {
                continue;
            }
            if !Self::statement_matches_resource(st, action, bucket, key) {
                continue;
            }
            if !principal_matches(st.principals.as_ref(), principal_id) {
                continue;
            }
            // v0.3 #13: Conditions are ALL-AND — a statement applies only
            // when every Condition clause matches the request context.
            // A clause failing simply skips the statement (no error).
            if !st.conditions.iter().all(|c| condition_matches(c, ctx)) {
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

    /// v0.8.4 #75 (audit H-4): namespace-aware Resource match.
    ///
    /// * `BucketOnly` actions (e.g. `s3:ListBucket`) only match
    ///   `Bucket(...)` ARNs.
    /// * `ObjectOnly` actions (e.g. `s3:GetObject`) only match
    ///   `Object{...}` ARNs, whose `key_pattern` is glob-matched against
    ///   the request key.
    /// * `Either` (the catch-all for `s3:*`, `*`, and any unknown
    ///   forward-compat verb) accepts whichever ARN form the policy
    ///   carries — this preserves the existing `{"Action":"s3:*"}`
    ///   ergonomics where a single statement covers an entire bucket.
    ///
    /// Pre-v0.8.4 behaviour conflated bucket-form and object-form ARNs
    /// for **every** action, which silently widened any bucket-only
    /// statement into object grants. That conflation is removed; mis-
    /// typed policies now fail closed.
    fn statement_matches_resource(
        stmt: &Statement,
        action: &str,
        bucket: &str,
        key: Option<&str>,
    ) -> bool {
        let kind = action_resource_kind(action);
        for parsed in &stmt.resources {
            match (parsed, kind) {
                // ----- bucket-only actions -----
                (ResourceArn::Bucket(b), ResourceKind::BucketOnly) => {
                    if glob_match(b, bucket) {
                        return true;
                    }
                }
                // ----- object-only actions -----
                (
                    ResourceArn::Object {
                        bucket: b,
                        key_pattern: kp,
                    },
                    ResourceKind::ObjectOnly,
                ) => {
                    if !glob_match(b, bucket) {
                        continue;
                    }
                    if let Some(k) = key
                        && glob_match(kp, k)
                    {
                        return true;
                    }
                }
                // ----- Either: forward-compat permissive branch -----
                (ResourceArn::Bucket(b), ResourceKind::Either) => {
                    if glob_match(b, bucket) {
                        return true;
                    }
                }
                (
                    ResourceArn::Object {
                        bucket: b,
                        key_pattern: kp,
                    },
                    ResourceKind::Either,
                ) => {
                    if !glob_match(b, bucket) {
                        continue;
                    }
                    match key {
                        Some(k) => {
                            if glob_match(kp, k) {
                                return true;
                            }
                        }
                        None => {
                            // No key in the request and the ARN is
                            // object-form — only the all-objects glob
                            // counts as covering "the bucket itself".
                            if kp == "*" {
                                return true;
                            }
                        }
                    }
                }
                // ----- mismatched namespace → skip (the H-4 fix) -----
                (ResourceArn::Bucket(_), ResourceKind::ObjectOnly)
                | (ResourceArn::Object { .. }, ResourceKind::BucketOnly) => continue,
            }
        }
        false
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

fn principal_matches(allowed: Option<&PrincipalSet>, principal_id: Option<&str>) -> bool {
    match allowed {
        // No Principal field on the statement → match any caller (incl. anonymous).
        None => true,
        // `"Principal": "*"` → match any caller (incl. anonymous).
        Some(PrincipalSet::Wildcard) => true,
        // `"Principal": {"AWS": [...]}` → match by access-key-id only;
        // anonymous callers (`principal_id = None`) are rejected here so
        // an unauth request can never satisfy a Specific list (closes a
        // sibling of audit H-5: silent anonymous widening).
        Some(PrincipalSet::Specific(list)) => match principal_id {
            None => false,
            Some(id) => list.iter().any(|p| p == "*" || p == id),
        },
    }
}

/// v0.3 #13: evaluate one Condition clause against the request context.
/// Returns `true` when the clause matches (statement may apply), `false`
/// when it doesn't (statement is skipped).
fn condition_matches(c: &Condition, ctx: &RequestContext) -> bool {
    match c.op {
        ConditionOp::IpAddress => match ctx.source_ip {
            Some(ip) => c.values.iter().any(|cidr| ip_in_cidr(ip, cidr)),
            None => false,
        },
        ConditionOp::NotIpAddress => match ctx.source_ip {
            Some(ip) => !c.values.iter().any(|cidr| ip_in_cidr(ip, cidr)),
            None => false,
        },
        ConditionOp::StringEquals => match context_value(&c.key, ctx) {
            Some(v) => c.values.iter().any(|x| x == &v),
            None => false,
        },
        ConditionOp::StringNotEquals => match context_value(&c.key, ctx) {
            Some(v) => !c.values.iter().any(|x| x == &v),
            None => false,
        },
        ConditionOp::StringLike => match context_value(&c.key, ctx) {
            Some(v) => c.values.iter().any(|pat| glob_match(pat, &v)),
            None => false,
        },
        ConditionOp::StringNotLike => match context_value(&c.key, ctx) {
            Some(v) => !c.values.iter().any(|pat| glob_match(pat, &v)),
            None => false,
        },
        ConditionOp::DateGreaterThan | ConditionOp::DateLessThan => {
            // aws:CurrentTime is the only date key we materialise today.
            let now = ctx.request_time.unwrap_or_else(SystemTime::now);
            let now_unix = match now.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(d) => d.as_secs() as i64,
                Err(_) => 0,
            };
            c.values.iter().any(|s| match parse_iso8601(s) {
                Some(t) => match c.op {
                    ConditionOp::DateGreaterThan => now_unix > t,
                    ConditionOp::DateLessThan => now_unix < t,
                    _ => unreachable!(),
                },
                None => false,
            })
        }
        ConditionOp::Bool => match context_value(&c.key, ctx) {
            Some(v) => c.values.iter().any(|x| x.eq_ignore_ascii_case(&v)),
            None => false,
        },
    }
}

/// Resolve a Condition key against the request context. Handles the
/// well-known `aws:SourceIp` / `aws:UserAgent` / `aws:CurrentTime` /
/// `aws:SecureTransport` keys, the v0.6 #39 `s3:ExistingObjectTag/*` /
/// `s3:RequestObjectTag/*` tag keys, plus any free-form key the caller
/// stuffed into `ctx.extra`.
fn context_value(key: &str, ctx: &RequestContext) -> Option<String> {
    match key {
        "aws:UserAgent" | "aws:userAgent" => ctx.user_agent.clone(),
        "aws:SourceIp" | "aws:sourceIp" => ctx.source_ip.map(|ip| ip.to_string()),
        "aws:SecureTransport" => Some(ctx.secure_transport.to_string()),
        other => {
            // v0.6 #39: tag-based condition keys are slash-suffixed
            // (`s3:ExistingObjectTag/<tag-key>` /
            // `s3:RequestObjectTag/<tag-key>`). Resolve to the named
            // tag's value if present in the relevant set; `None`
            // otherwise — which makes the clause fail (statement
            // skipped) for both `StringEquals` and `StringNotEquals`.
            if let Some(tag_key) = other.strip_prefix("s3:ExistingObjectTag/") {
                return ctx
                    .existing_object_tags
                    .as_ref()
                    .and_then(|s| s.get(tag_key).map(str::to_owned));
            }
            if let Some(tag_key) = other.strip_prefix("s3:RequestObjectTag/") {
                return ctx
                    .request_object_tags
                    .as_ref()
                    .and_then(|s| s.get(tag_key).map(str::to_owned));
            }
            ctx.extra.get(other).cloned()
        }
    }
}

/// Minimal CIDR-or-bare-IP membership test for `IpAddress`. Supports both
/// IPv4 and IPv6, with or without the `/N` mask.
fn ip_in_cidr(ip: IpAddr, cidr: &str) -> bool {
    match cidr.split_once('/') {
        None => cidr.parse::<IpAddr>().is_ok_and(|c| c == ip),
        Some((net_str, mask_str)) => {
            let Ok(net) = net_str.parse::<IpAddr>() else {
                return false;
            };
            let Ok(mask_bits) = mask_str.parse::<u8>() else {
                return false;
            };
            match (ip, net) {
                (IpAddr::V4(ip4), IpAddr::V4(net4)) => {
                    if mask_bits > 32 {
                        return false;
                    }
                    if mask_bits == 0 {
                        return true;
                    }
                    let shift = 32 - mask_bits;
                    (u32::from(ip4) >> shift) == (u32::from(net4) >> shift)
                }
                (IpAddr::V6(ip6), IpAddr::V6(net6)) => {
                    if mask_bits > 128 {
                        return false;
                    }
                    if mask_bits == 0 {
                        return true;
                    }
                    let shift = 128 - mask_bits;
                    (u128::from(ip6) >> shift) == (u128::from(net6) >> shift)
                }
                _ => false, // IPv4 vs IPv6 mismatch
            }
        }
    }
}

/// Minimal ISO-8601 parser tailored to the AWS bucket-policy
/// `aws:CurrentTime` format: `YYYY-MM-DDTHH:MM:SSZ` (UTC, second
/// granularity). Returns unix epoch seconds. AWS also accepts the
/// `+00:00` offset variants and millisecond fractions — out of scope
/// for v0.3, can be relaxed later if a real policy needs them.
fn parse_iso8601(s: &str) -> Option<i64> {
    // Accept `YYYY-MM-DDTHH:MM:SSZ` only; reject anything else.
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let date_parts: Vec<&str> = date.split('-').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let year: i64 = date_parts[0].parse().ok()?;
    let month: i64 = date_parts[1].parse().ok()?;
    let day: i64 = date_parts[2].parse().ok()?;
    let time_parts: Vec<&str> = time.split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let h: i64 = time_parts[0].parse().ok()?;
    let m: i64 = time_parts[1].parse().ok()?;
    let s: i64 = time_parts[2].parse().ok()?;
    // Days from 1970-01-01 via a quick civil-from-date algorithm
    // (Howard Hinnant — public domain). Good for AD years.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as u64;
    let days_from_epoch = era * 146097 + doe as i64 - 719468;
    Some(days_from_epoch * 86_400 + h * 3600 + m * 60 + s)
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
        // v0.8.4 #75: a bare `arn:aws:s3:::*` is bucket-form and only
        // covers bucket-level actions; object reach requires the
        // explicit `/*` glob in the key portion. Use a two-statement
        // policy that grants both ARN namespaces.
        let pol = p(r#"{
            "Version": "2012-10-17",
            "Statement": [
              {"Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::*"},
              {"Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::*/*"}
            ]
        }"#);
        assert!(pol.evaluate("s3:GetObject", "any", Some("k"), None).allow);
        assert!(pol.evaluate("s3:PutObject", "any", Some("k"), None).allow);
        assert!(pol.evaluate("s3:ListBucket", "any", None, None).allow);
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

    // ===== v0.3 #13 IAM Condition tests =====

    fn ctx_ip(ip: &str) -> RequestContext {
        RequestContext {
            source_ip: Some(ip.parse().unwrap()),
            ..Default::default()
        }
    }

    #[test]
    fn condition_ip_address_cidr_match() {
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {"IpAddress": {"aws:SourceIp": ["10.0.0.0/8", "192.168.1.0/24"]}}
            }]
        }"#);
        assert!(
            pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ctx_ip("10.5.6.7"))
                .allow
        );
        assert!(
            pol.evaluate_with(
                "s3:GetObject",
                "b",
                Some("k"),
                None,
                &ctx_ip("192.168.1.50")
            )
            .allow
        );
        assert!(
            !pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ctx_ip("203.0.113.1"))
                .allow
        );
        // No source IP in context → condition fails → statement skipped
        assert!(
            !pol.evaluate_with(
                "s3:GetObject",
                "b",
                Some("k"),
                None,
                &RequestContext::default()
            )
            .allow
        );
    }

    #[test]
    fn condition_not_ip_address_negates() {
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Deny", "Action": "s3:DeleteObject",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {"NotIpAddress": {"aws:SourceIp": ["10.0.0.0/8"]}}
            },
            {"Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::b/*"}]
        }"#);
        // Outside the trusted CIDR → Deny applies (NotIpAddress = true) → AccessDenied
        assert!(
            !pol.evaluate_with(
                "s3:DeleteObject",
                "b",
                Some("k"),
                None,
                &ctx_ip("203.0.113.1")
            )
            .allow
        );
        // Inside the trusted CIDR → Deny condition fails → Allow remains
        assert!(
            pol.evaluate_with("s3:DeleteObject", "b", Some("k"), None, &ctx_ip("10.0.0.7"))
                .allow
        );
    }

    #[test]
    fn condition_string_equals_user_agent() {
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {"StringEquals": {"aws:UserAgent": ["MyApp/1.0", "MyApp/2.0"]}}
            }]
        }"#);
        let ua = |s: &str| RequestContext {
            user_agent: Some(s.into()),
            ..Default::default()
        };
        assert!(
            pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ua("MyApp/1.0"))
                .allow
        );
        assert!(
            !pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ua("OtherApp/1.0"))
                .allow
        );
    }

    #[test]
    fn condition_string_like_glob() {
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {"StringLike": {"aws:UserAgent": ["MyApp/*", "boto3/*"]}}
            }]
        }"#);
        let ua = |s: &str| RequestContext {
            user_agent: Some(s.into()),
            ..Default::default()
        };
        assert!(
            pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ua("MyApp/3.14"))
                .allow
        );
        assert!(
            pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ua("boto3/1.34.5"))
                .allow
        );
        assert!(
            !pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ua("curl/8"))
                .allow
        );
    }

    #[test]
    fn condition_date_window() {
        // Allow only requests between two dates.
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {
                "DateGreaterThan": {"aws:CurrentTime": ["2026-01-01T00:00:00Z"]},
                "DateLessThan":    {"aws:CurrentTime": ["2026-12-31T23:59:59Z"]}
              }
            }]
        }"#);
        let mid_year = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000); // ~mid-2026
        let after = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000); // ~early-2027
        let ctx_at = |t: SystemTime| RequestContext {
            request_time: Some(t),
            ..Default::default()
        };
        assert!(
            pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ctx_at(mid_year))
                .allow
        );
        assert!(
            !pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &ctx_at(after))
                .allow
        );
    }

    #[test]
    fn condition_bool_secure_transport() {
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Deny", "Action": "s3:*",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {"Bool": {"aws:SecureTransport": ["false"]}}
            },
            {"Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::b/*"}]
        }"#);
        let plain = RequestContext {
            secure_transport: false,
            ..Default::default()
        };
        let tls = RequestContext {
            secure_transport: true,
            ..Default::default()
        };
        // Plain HTTP → SecureTransport=false → Deny matches
        assert!(
            !pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &plain)
                .allow
        );
        // TLS → SecureTransport=true → Deny condition fails → Allow remains
        assert!(
            pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &tls)
                .allow
        );
    }

    #[test]
    fn condition_unknown_operator_rejected() {
        let err = Policy::from_json_str(
            r#"{
            "Statement": [{"Effect": "Allow", "Action": "s3:*",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {"NumericGreaterThan": {"k": ["1"]}}
            }]
        }"#,
        )
        .expect_err("should reject unsupported operator");
        assert!(err.contains("unsupported policy Condition operator"));
        assert!(err.contains("NumericGreaterThan"));
    }

    // ===== v0.6 #39 tag-based condition tests =====

    #[test]
    fn condition_existing_object_tag_matches_via_tagmanager_state() {
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {
                "StringEquals": {"s3:ExistingObjectTag/Project": ["Phoenix"]}
              }
            }]
        }"#);
        let with_tag = RequestContext {
            existing_object_tags: Some(
                crate::tagging::TagSet::from_pairs(vec![
                    ("Project".into(), "Phoenix".into()),
                    ("Env".into(), "prod".into()),
                ])
                .unwrap(),
            ),
            ..Default::default()
        };
        let other_tag = RequestContext {
            existing_object_tags: Some(
                crate::tagging::TagSet::from_pairs(vec![("Project".into(), "Other".into())])
                    .unwrap(),
            ),
            ..Default::default()
        };
        // Tag matches → Allow.
        assert!(
            pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &with_tag)
                .allow
        );
        // Tag value mismatched → implicit deny.
        assert!(
            !pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &other_tag)
                .allow
        );
    }

    #[test]
    fn condition_request_object_tag_matches_via_x_amz_tagging() {
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow", "Action": "s3:PutObject",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {
                "StringEquals": {"s3:RequestObjectTag/Env": ["prod", "staging"]}
              }
            }]
        }"#);
        let req_tags = |v: &str| RequestContext {
            request_object_tags: Some(
                crate::tagging::TagSet::from_pairs(vec![("Env".into(), v.into())]).unwrap(),
            ),
            ..Default::default()
        };
        assert!(
            pol.evaluate_with("s3:PutObject", "b", Some("k"), None, &req_tags("prod"))
                .allow
        );
        assert!(
            pol.evaluate_with(
                "s3:PutObject",
                "b",
                Some("k"),
                None,
                &req_tags("staging")
            )
            .allow
        );
        assert!(
            !pol.evaluate_with("s3:PutObject", "b", Some("k"), None, &req_tags("dev"))
                .allow
        );
    }

    #[test]
    fn condition_tag_not_present_fails_closed() {
        // Statement gates on a tag the request doesn't carry → the
        // clause must fail (not silently match), so the only Allow is
        // skipped and we get implicit deny.
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Condition": {
                "StringEquals": {"s3:ExistingObjectTag/Owner": ["alice"]}
              }
            }]
        }"#);
        // No `existing_object_tags` at all → tag look-up returns None
        // → clause fails → statement skipped.
        let none_ctx = RequestContext::default();
        assert!(
            !pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &none_ctx)
                .allow
        );
        // Tag set exists but lacks the named key → also fails.
        let other_only = RequestContext {
            existing_object_tags: Some(
                crate::tagging::TagSet::from_pairs(vec![("Project".into(), "X".into())])
                    .unwrap(),
            ),
            ..Default::default()
        };
        assert!(
            !pol.evaluate_with("s3:GetObject", "b", Some("k"), None, &other_only)
                .allow
        );
    }

    #[test]
    fn condition_legacy_evaluate_unchanged() {
        // Old `evaluate` (no context) still works: a policy without
        // Condition clauses is unaffected by the v0.3 changes.
        let pol = p(r#"{
            "Statement": [{"Effect": "Allow", "Action": "s3:*",
              "Resource": "arn:aws:s3:::b/*"}]
        }"#);
        assert!(pol.evaluate("s3:GetObject", "b", Some("k"), None).allow);
    }

    // ===== v0.8.4 #75 (audit H-4 + H-5) =====

    #[test]
    fn parse_resource_arn_bucket_form() {
        let arn = parse_resource_arn("arn:aws:s3:::mybucket").expect("parse");
        assert_eq!(arn, ResourceArn::Bucket("mybucket".into()));
    }

    #[test]
    fn parse_resource_arn_object_form() {
        let arn = parse_resource_arn("arn:aws:s3:::mybucket/some/key").expect("parse");
        assert_eq!(
            arn,
            ResourceArn::Object {
                bucket: "mybucket".into(),
                key_pattern: "some/key".into(),
            }
        );
    }

    #[test]
    fn parse_resource_arn_object_wildcard() {
        let arn = parse_resource_arn("arn:aws:s3:::mybucket/*").expect("parse");
        assert_eq!(
            arn,
            ResourceArn::Object {
                bucket: "mybucket".into(),
                key_pattern: "*".into(),
            }
        );
        // Trailing-glob in key portion stays in `key_pattern` verbatim.
        let pre = parse_resource_arn("arn:aws:s3:::b/data/*.parquet").expect("parse");
        assert_eq!(
            pre,
            ResourceArn::Object {
                bucket: "b".into(),
                key_pattern: "data/*.parquet".into(),
            }
        );
        // Bad ARN: missing prefix.
        assert!(matches!(
            parse_resource_arn("not-an-arn"),
            Err(PolicyParseError::InvalidResourceArn(_))
        ));
        // Bad ARN: empty bucket.
        assert!(matches!(
            parse_resource_arn("arn:aws:s3:::"),
            Err(PolicyParseError::EmptyBucketInArn(_))
        ));
        assert!(matches!(
            parse_resource_arn("arn:aws:s3:::/key"),
            Err(PolicyParseError::EmptyBucketInArn(_))
        ));
    }

    #[test]
    fn bucket_only_arn_does_not_grant_object_action() {
        // Audit H-4: a bare `arn:aws:s3:::b` Resource MUST NOT grant
        // `s3:GetObject` (or any other object-level action). Pre-v0.8.4
        // it did, silently widening privilege.
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow",
              "Principal": "*",
              "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::mybucket"
            }]
        }"#);
        let d = pol.evaluate("s3:GetObject", "mybucket", Some("k"), None);
        assert!(!d.allow, "bucket-form ARN must not grant s3:GetObject");
        assert_eq!(d.matched_effect, None, "should be implicit deny");
        // Sanity: an object-form ARN against the same bucket DOES grant.
        let pol_ok = p(r#"{
            "Statement": [{
              "Effect": "Allow",
              "Principal": "*",
              "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::mybucket/*"
            }]
        }"#);
        assert!(
            pol_ok
                .evaluate("s3:GetObject", "mybucket", Some("k"), None)
                .allow
        );
    }

    #[test]
    fn object_arn_does_not_grant_bucket_action() {
        // Audit H-4 (other direction): an `arn:aws:s3:::b/k` Resource
        // MUST NOT grant `s3:ListBucket` (a bucket-level action).
        let pol = p(r#"{
            "Statement": [{
              "Effect": "Allow",
              "Principal": "*",
              "Action": "s3:ListBucket",
              "Resource": "arn:aws:s3:::b/k"
            }]
        }"#);
        let d = pol.evaluate("s3:ListBucket", "b", None, None);
        assert!(!d.allow, "object-form ARN must not grant s3:ListBucket");
        assert_eq!(d.matched_effect, None);
    }

    #[test]
    fn principal_wildcard_only_accepts_literal_star() {
        // Audit H-5: a bare `"Principal": "AKIA..."` must be rejected
        // at parse time — pre-v0.8.4 it deserialized to
        // `PrincipalSet::Wildcard("AKIA...")` and silently matched any
        // caller (incl. anonymous).
        let err = Policy::from_json_str_typed(
            r#"{"Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Principal": "AKIATESTNOTAWILDCARD"
            }]}"#,
        )
        .expect_err("non-* string principal must be rejected");
        assert!(
            matches!(err, PolicyParseError::InvalidWildcard(ref s) if s == "AKIATESTNOTAWILDCARD"),
            "expected InvalidWildcard, got {err:?}"
        );
        // The literal "*" still parses fine.
        let ok = PrincipalSet::parse(&serde_json::Value::String("*".into())).expect("ok");
        assert_eq!(ok, PrincipalSet::Wildcard);
    }

    #[test]
    fn principal_unsupported_service_type_rejected() {
        // Audit H-5: `{"Service": "..."}` (and other non-AWS principal
        // types) must be rejected at parse time so a policy author
        // can't think they granted access to a Lambda role when in
        // fact the field was silently dropped to "match anyone".
        let err = Policy::from_json_str_typed(
            r#"{"Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Principal": {"Service": "lambda.amazonaws.com"}
            }]}"#,
        )
        .expect_err("Service principal must be rejected");
        assert!(
            matches!(err, PolicyParseError::UnsupportedPrincipalType),
            "expected UnsupportedPrincipalType, got {err:?}"
        );
        // Federated / CanonicalUser also rejected.
        for shape in [
            r#"{"Federated": "cognito-identity.amazonaws.com"}"#,
            r#"{"CanonicalUser": "abcdef"}"#,
            r#"{"AWS": "AKIA", "Service": "x"}"#,
        ] {
            let v: serde_json::Value = serde_json::from_str(shape).unwrap();
            assert!(
                matches!(
                    PrincipalSet::parse(&v),
                    Err(PolicyParseError::UnsupportedPrincipalType)
                ),
                "expected UnsupportedPrincipalType for {shape}"
            );
        }
    }

    #[test]
    fn principal_empty_aws_list_rejected() {
        // Audit H-5: `{"AWS": []}` must be rejected — pre-v0.8.4 the
        // empty list flowed through `principals: Some(vec![])` which
        // the matcher treated as "any caller", silently widening.
        let err = Policy::from_json_str_typed(
            r#"{"Statement": [{
              "Effect": "Allow", "Action": "s3:GetObject",
              "Resource": "arn:aws:s3:::b/*",
              "Principal": {"AWS": []}
            }]}"#,
        )
        .expect_err("empty AWS principal list must be rejected");
        assert!(
            matches!(err, PolicyParseError::EmptyPrincipalList),
            "expected EmptyPrincipalList, got {err:?}"
        );
        // Sanity: a single-element list parses to Specific.
        let v: serde_json::Value = serde_json::from_str(r#"{"AWS": "AKIAONE"}"#).unwrap();
        assert_eq!(
            PrincipalSet::parse(&v).unwrap(),
            PrincipalSet::Specific(vec!["AKIAONE".into()])
        );
        // Anonymous caller against a Specific list → no match.
        let pol = p(r#"{"Statement": [{
            "Effect": "Allow", "Action": "s3:GetObject",
            "Resource": "arn:aws:s3:::b/*",
            "Principal": {"AWS": ["AKIAONE"]}
        }]}"#);
        assert!(!pol.evaluate("s3:GetObject", "b", Some("k"), None).allow);
    }
}
