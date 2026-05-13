//! `/health` と `/ready` の HTTP routing layer + CORS OPTIONS preflight
//! interceptor + SigV4a verify gate。
//!
//! S3 server と同じポートで health probe に応答できると AWS ALB / NLB / k8s
//! readiness probe との統合が単純になる。
//!
//! - `GET /health` → 常に `200 OK` (server プロセスが生きていれば返す)
//! - `GET /ready` → `ready_check` future を await し、`Ok(())` なら 200、
//!   それ以外 (backend 不通等) は 503。
//! - `OPTIONS /<bucket>[/<key>]` (Origin + Access-Control-Request-Method 付き)
//!   → v0.7 #44: `cors_manager` が attach されていれば、bucket の登録された
//!   rule list に対して preflight match を実行し、200 + Allow-* header を
//!   組み立てて返す (no match なら 403)。s3s framework は OPTIONS verb を
//!   typed handler として持たないため、HTTP-level の interceptor で寄せる。
//! - `Authorization: AWS4-ECDSA-P256-SHA256 ...` (SigV4a) を持つ request
//!   → v0.7 #47: `sigv4a_gate` が attach されていれば、listener 側で署名を
//!   verify し、success なら inner S3Service へ forward、failure なら 403
//!   `SignatureDoesNotMatch` / `InvalidAccessKeyId` を直接返す。s3s 既存の
//!   SigV4 verifier は `AWS4-ECDSA-P256-SHA256` を "unknown algorithm" として
//!   reject するため、middleware を挟まないと SigV4a request は届かない。
//! - その他のパス → inner S3Service へ委譲

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::Service;
use hyper::{Method, Request, Response, StatusCode};
use metrics_exporter_prometheus::PrometheusHandle;

use crate::cors::{CorsManager, CorsRule};
use crate::service::SigV4aGate;

/// readiness check 関数。bound is `Send + Sync` for cross-task use.
pub type ReadyCheck =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// inner service と health/ready/metrics + CORS preflight handler +
/// SigV4a verify gate を合成する hyper Service。
#[derive(Clone)]
pub struct HealthRouter<S> {
    pub inner: S,
    pub ready_check: Option<ReadyCheck>,
    pub metrics_handle: Option<PrometheusHandle>,
    /// v0.7 #44: optional CORS bucket-config manager. When attached,
    /// OPTIONS requests carrying `Origin` + `Access-Control-Request-Method`
    /// are intercepted before reaching the s3s service and answered
    /// directly with Access-Control-Allow-* headers (or 403 if no rule
    /// matches). When `None`, OPTIONS falls through to the inner service
    /// (s3s typically returns 405 since no S3 handler maps to OPTIONS).
    pub cors_manager: Option<Arc<CorsManager>>,
    /// v0.7 #47: optional SigV4a verify gate. When attached, requests
    /// whose `Authorization` header begins with `AWS4-ECDSA-P256-SHA256`
    /// (or that carry `X-Amz-Region-Set`) are verified at the HTTP
    /// layer using the configured ECDSA-P-256 credential store; on
    /// failure the listener returns 403 directly. When `None`, the
    /// gate is a no-op so plain SigV4 deployments are unaffected.
    pub sigv4a_gate: Option<Arc<SigV4aGate>>,
    /// v0.7 #47: region name used when checking
    /// `X-Amz-Region-Set` membership during SigV4a verification. The
    /// listener is single-region in this milestone — operators that
    /// front S4 with a Multi-Region Access Point set this to the
    /// canonical "this listener's region" string. Defaults to
    /// `"us-east-1"` (the AWS-default region when none is configured).
    pub region: String,
}

impl<S> HealthRouter<S> {
    pub fn new(inner: S, ready_check: Option<ReadyCheck>) -> Self {
        Self {
            inner,
            ready_check,
            metrics_handle: None,
            cors_manager: None,
            sigv4a_gate: None,
            region: "us-east-1".to_string(),
        }
    }

    #[must_use]
    pub fn with_metrics(mut self, handle: PrometheusHandle) -> Self {
        self.metrics_handle = Some(handle);
        self
    }

    /// v0.7 #44: attach an `Arc<CorsManager>` so OPTIONS preflight
    /// requests are handled at the HTTP layer instead of falling through
    /// to s3s.
    #[must_use]
    pub fn with_cors_manager(mut self, mgr: Arc<CorsManager>) -> Self {
        self.cors_manager = Some(mgr);
        self
    }

    /// v0.7 #47: attach an `Arc<SigV4aGate>` so `AWS4-ECDSA-P256-SHA256`
    /// requests are verified at the HTTP layer instead of being
    /// rejected by s3s' SigV4 verifier as "unknown algorithm".
    #[must_use]
    pub fn with_sigv4a_gate(mut self, gate: Arc<SigV4aGate>) -> Self {
        self.sigv4a_gate = Some(gate);
        self
    }

    /// v0.7 #47: override the listener's "served region" string used
    /// to check `X-Amz-Region-Set` membership during SigV4a
    /// verification. Defaults to `"us-east-1"`.
    #[must_use]
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = region.into();
        self
    }
}

/// v0.7 #44: HTTP-level OPTIONS preflight interceptor.
///
/// Returns:
/// - `Some(response)` if `req` is an OPTIONS preflight (Origin +
///   Access-Control-Request-Method headers present) targeting a bucket
///   with CORS configured. The response is 200 with Allow-* headers
///   when a rule matches, or 403 when no rule matches the
///   (origin, method, headers) triple.
/// - `None` if the request is not a preflight, or no CORS config is
///   registered for the target bucket — caller forwards to the s3s
///   service.
///
/// `cors` is `Option<&Arc<CorsManager>>` so callers can pass through
/// the inner service's optional manager without unwrapping first.
///
/// Generic over the request body type `B` so unit tests can drive the
/// matcher with `Request<()>` without constructing a real `Incoming`
/// stream (only headers, method, and URI are inspected).
#[must_use]
pub fn try_handle_preflight<B>(
    req: &Request<B>,
    cors: Option<&Arc<CorsManager>>,
) -> Option<Response<s3s::Body>> {
    if req.method() != Method::OPTIONS {
        return None;
    }
    let mgr = cors?;
    // Path is `/<bucket>` or `/<bucket>/<key>` — first segment is bucket.
    // Empty path or a query-only request has no bucket and is not a
    // preflight we can answer.
    let path = req.uri().path();
    let bucket = path.trim_start_matches('/').split('/').next()?;
    if bucket.is_empty() {
        return None;
    }
    let origin = req.headers().get("origin")?.to_str().ok()?;
    let method = req
        .headers()
        .get("access-control-request-method")?
        .to_str()
        .ok()?;
    // Access-Control-Request-Headers is a comma-separated list, optional
    // (browsers omit it when no custom headers are being sent).
    let req_headers: Vec<String> = req
        .headers()
        .get("access-control-request-headers")
        .and_then(|h| h.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // No config for this bucket → not our problem (let s3s handle / 404).
    // We need to distinguish "no config" from "config but no rule matches"
    // to correctly fall through vs. return 403.
    let _ = mgr.get(bucket)?;
    match mgr.match_preflight(bucket, origin, method, &req_headers) {
        Some(rule) => Some(build_preflight_allow_response(&rule, origin)),
        None => Some(build_preflight_deny_response()),
    }
}

/// 200 response with the matched rule's Allow-* headers.
fn build_preflight_allow_response(rule: &CorsRule, origin: &str) -> Response<s3s::Body> {
    let mut builder = Response::builder().status(StatusCode::OK);
    // Echo the matched origin: literal "*" if the rule used a wildcard,
    // otherwise the requesting origin verbatim (S3 spec).
    let allow_origin: String = if rule.allowed_origins.iter().any(|o| o == "*") {
        "*".into()
    } else {
        origin.to_owned()
    };
    builder = builder.header("Access-Control-Allow-Origin", allow_origin);
    builder = builder.header(
        "Access-Control-Allow-Methods",
        rule.allowed_methods.join(", "),
    );
    if !rule.allowed_headers.is_empty() {
        builder = builder.header(
            "Access-Control-Allow-Headers",
            rule.allowed_headers.join(", "),
        );
    }
    if !rule.expose_headers.is_empty() {
        builder = builder.header(
            "Access-Control-Expose-Headers",
            rule.expose_headers.join(", "),
        );
    }
    if let Some(secs) = rule.max_age_seconds {
        builder = builder.header("Access-Control-Max-Age", secs.to_string());
    }
    // Empty body, but set content-length explicitly for clarity.
    let bytes = Bytes::new();
    builder = builder.header("content-length", "0");
    builder
        .body(s3s::Body::http_body(
            Full::new(bytes).map_err(|never| match never {}),
        ))
        .expect("preflight response builder")
}

/// 403 response when an OPTIONS preflight reaches a bucket with CORS
/// configured but no rule matches the (origin, method, headers) triple.
fn build_preflight_deny_response() -> Response<s3s::Body> {
    let body = Bytes::from_static(b"CORSResponse: This CORS request is not allowed.");
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("content-type", "text/plain; charset=utf-8")
        .header("content-length", body.len().to_string())
        .body(s3s::Body::http_body(
            Full::new(body).map_err(|never| match never {}),
        ))
        .expect("preflight deny response builder")
}

// ===========================================================================
// v0.7 #47 — SigV4a verify gate middleware.
// ===========================================================================

/// v0.7 #47: Try to verify the request as SigV4a-signed.
///
/// Returns:
/// - `None` if the request is not SigV4a-signed (no `AWS4-ECDSA-P256-SHA256`
///   `Authorization` prefix and no `X-Amz-Region-Set` header) — the
///   caller forwards the request to s3s for the default SigV4 path.
/// - `Some(Ok(()))` if SigV4a verify succeeded — the caller forwards to
///   the inner service so the S3 handler runs.
/// - `Some(Err(response))` if SigV4a verify failed — the caller returns
///   the 403 response directly without ever invoking the inner service.
///
/// `gate` is `Option<&Arc<SigV4aGate>>` so callers can pass through the
/// router's optional gate without unwrapping first; when `None`, this
/// function always returns `None` (no SigV4a verification configured).
///
/// `requested_region` is the listener's served region (used to validate
/// the request's `X-Amz-Region-Set` header membership).
///
/// Generic over the request body type `B` so unit tests can drive the
/// matcher with `Request<()>` without constructing a real `Incoming`
/// stream — only headers, method, and URI participate in the canonical
/// request bytes built here.
///
/// # Canonical request bytes
///
/// We build a SigV4-shaped canonical request from the HTTP-layer
/// signal alone (method, URI path, sorted query string, headers in the
/// order listed by `SignedHeaders=`, and `x-amz-content-sha256` as the
/// payload hash — the standard "client-supplied body hash" convention
/// every AWS SDK uses). Reading the body would force a `Request<Bytes>`
/// rebuild and break the s3s framework's streaming-body assumptions, so
/// the payload-hash header is the only correct source for SigV4a.
///
/// Clients that want to sign over the body must include the actual
/// SHA-256 of the body in `x-amz-content-sha256`; clients that don't
/// (most S3 SDKs default to `UNSIGNED-PAYLOAD` for streaming PUTs) sign
/// over that literal string instead. Either way the bytes the gate
/// compares against are exactly what the client computed.
pub fn try_sigv4a_verify<B>(
    req: &Request<B>,
    gate: Option<&Arc<SigV4aGate>>,
    requested_region: &str,
) -> Option<Result<(), Response<s3s::Body>>> {
    let gate = gate?;
    if !crate::sigv4a::detect(req) {
        // Not a SigV4a request — caller forwards to the SigV4 path.
        return None;
    }
    // Pre-parse the Authorization header so we know which signed-headers
    // list to canonicalise in. If the header is malformed, fail fast
    // with 403 rather than building canonical bytes that can never
    // verify.
    let auth_hdr = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let signed_headers: Vec<String> = match auth_hdr
        .and_then(crate::sigv4a::parse_authorization_header)
    {
        Some(parsed) => parsed.signed_headers,
        None => {
            // No / unparseable Authorization header but `detect` flagged
            // it as SigV4a-shaped (e.g. only the region-set header is
            // present) — surface as SignatureDoesNotMatch directly.
            return Some(Err(build_sigv4a_error_response(
                "SignatureDoesNotMatch",
                "missing or malformed Authorization header for SigV4a request",
            )));
        }
    };
    let canonical = build_canonical_request_bytes(req, &signed_headers);
    match gate.pre_route(req, requested_region, &canonical) {
        Ok(()) => Some(Ok(())),
        Err(err) => {
            tracing::warn!(error = %err, "SigV4a verify rejected request");
            Some(Err(build_sigv4a_error_response(
                err.s3_error_code(),
                &err.to_string(),
            )))
        }
    }
}

/// v0.7 #47: build a SigV4-shaped canonical request from the HTTP
/// surface alone (no body access). Returns the bytes that the
/// SigV4a gate will check the ECDSA signature against.
///
/// Format (one element per line, joined with `\n`):
/// 1. HTTP method (uppercase)
/// 2. canonical URI (path; we leave it untouched since AWS SDKs
///    pre-encode it the same way s3s receives it)
/// 3. canonical query string (sorted by name, name=value pairs joined
///    by `&`; empty when no query string)
/// 4. canonical headers (one `name:trimmed-value\n` per signed header,
///    in the **order** they appear in `SignedHeaders=`)
/// 5. signed headers list (lowercase names joined by `;`)
/// 6. payload hash (value of `x-amz-content-sha256`, or `UNSIGNED-PAYLOAD`
///    if absent)
fn build_canonical_request_bytes<B>(
    req: &Request<B>,
    signed_headers: &[String],
) -> Vec<u8> {
    let mut buf = String::with_capacity(512);
    buf.push_str(req.method().as_str());
    buf.push('\n');
    buf.push_str(req.uri().path());
    buf.push('\n');
    buf.push_str(&canonical_query_string(req.uri().query().unwrap_or("")));
    buf.push('\n');
    for name in signed_headers {
        let value = req
            .headers()
            .get(name.as_str())
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        buf.push_str(name);
        buf.push(':');
        // Trim whitespace and collapse repeated inner whitespace per
        // SigV4 canonicalisation rules. This is the same trimming AWS
        // SDKs do when they sign.
        buf.push_str(&trim_collapse_ws(value));
        buf.push('\n');
    }
    buf.push('\n');
    buf.push_str(&signed_headers.join(";"));
    buf.push('\n');
    let payload_hash = req
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("UNSIGNED-PAYLOAD");
    buf.push_str(payload_hash);
    buf.into_bytes()
}

/// SigV4 canonical query string: split on `&`, parse each `k=v` (or
/// `k`), sort lexicographically by name (then by value), re-join with
/// `&`. Empty input → empty string. We do **not** re-encode the values
/// — they already arrived URL-encoded over the wire, and AWS SDKs
/// expect the server to compare the bytes verbatim.
fn canonical_query_string(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k, v),
            None => (kv, ""),
        })
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(b.1)));
    let mut out = String::with_capacity(query.len());
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

/// SigV4 header-value canonicalisation: trim leading + trailing
/// whitespace and collapse runs of internal whitespace to a single
/// space. This mirrors what AWS SDKs do client-side when computing the
/// canonical request — without it, a header value with extra spaces
/// would canonicalise differently on each side.
fn trim_collapse_ws(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_ws = false;
    for c in trimmed.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// v0.7 #47: build an AWS-shaped 403 XML response for a SigV4a verify
/// failure. The response body matches the wire format AWS S3 emits for
/// the same conditions so SDKs surface the right exception class to the
/// caller.
fn build_sigv4a_error_response(code: &str, message: &str) -> Response<s3s::Body> {
    let body_str = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Error>\n  <Code>{code}</Code>\n  <Message>{message}</Message>\n</Error>"
    );
    let bytes = Bytes::from(body_str.into_bytes());
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("content-type", "application/xml")
        .header("content-length", bytes.len().to_string())
        .body(s3s::Body::http_body(
            Full::new(bytes).map_err(|never| match never {}),
        ))
        .expect("sigv4a error response builder")
}


/// `/health` と `/ready` のレスポンス Body。
/// inner S3Service の Body と互換する形にするために `s3s::Body` でラップ可能な
/// `Full<Bytes>` を `s3s::Body::http_body` 経由で構築する。
type RespBody = s3s::Body;

fn make_text_response(status: StatusCode, body: &'static str) -> Response<RespBody> {
    let bytes = Bytes::from_static(body.as_bytes());
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("content-length", bytes.len().to_string())
        .body(s3s::Body::http_body(
            Full::new(bytes).map_err(|never| match never {}),
        ))
        .expect("static response")
}

fn make_owned_text_response(
    status: StatusCode,
    content_type: &'static str,
    body: String,
) -> Response<RespBody> {
    let bytes = Bytes::from(body.into_bytes());
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("content-length", bytes.len().to_string())
        .body(s3s::Body::http_body(
            Full::new(bytes).map_err(|never| match never {}),
        ))
        .expect("owned response")
}

impl<S> Service<Request<Incoming>> for HealthRouter<S>
where
    S: Service<Request<Incoming>, Response = Response<s3s::Body>, Error = s3s::HttpError>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<RespBody>;
    type Error = s3s::HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        // v0.7 #44: short-circuit CORS OPTIONS preflight at the HTTP layer
        // before health/metrics dispatch. Preflight must run only for
        // OPTIONS requests, and only when a CORS manager is attached and
        // a config exists for the requested bucket; otherwise fall
        // through to the existing routing logic.
        if let Some(resp) = try_handle_preflight(&req, self.cors_manager.as_ref()) {
            return Box::pin(async move { Ok(resp) });
        }
        // v0.7 #47: SigV4a verify gate. When the request is signed with
        // `AWS4-ECDSA-P256-SHA256` and a credential store is configured,
        // verify here at the HTTP layer (s3s' SigV4 verifier would
        // otherwise reject the request as "unknown algorithm" before
        // any handler ran). Plain SigV4 (HMAC) requests return `None`
        // and fall through to the inner service untouched.
        if let Some(result) =
            try_sigv4a_verify(&req, self.sigv4a_gate.as_ref(), &self.region)
        {
            match result {
                Ok(()) => {
                    // verified — fall through to the path-routing logic
                    // below (the health/metrics/inner-service dispatch).
                }
                Err(resp) => return Box::pin(async move { Ok(resp) }),
            }
        }
        let path = req.uri().path();
        match (req.method(), path) {
            (&hyper::Method::GET, "/health") | (&hyper::Method::HEAD, "/health") => {
                Box::pin(async { Ok(make_text_response(StatusCode::OK, "ok\n")) })
            }
            (&hyper::Method::GET, "/metrics") | (&hyper::Method::HEAD, "/metrics") => {
                let handle = self.metrics_handle.clone();
                Box::pin(async move {
                    match handle {
                        Some(h) => {
                            let body = h.render();
                            Ok(make_owned_text_response(
                                StatusCode::OK,
                                "text/plain; version=0.0.4; charset=utf-8",
                                body,
                            ))
                        }
                        None => Ok(make_text_response(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "metrics not configured\n",
                        )),
                    }
                })
            }
            (&hyper::Method::GET, "/ready") | (&hyper::Method::HEAD, "/ready") => {
                let check = self.ready_check.clone();
                Box::pin(async move {
                    match check {
                        Some(f) => match f().await {
                            Ok(()) => Ok(make_text_response(StatusCode::OK, "ready\n")),
                            Err(reason) => {
                                tracing::warn!(%reason, "readiness check failed");
                                Ok(make_text_response(
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    "not ready\n",
                                ))
                            }
                        },
                        None => Ok(make_text_response(StatusCode::OK, "ready (no check)\n")),
                    }
                })
            }
            _ => {
                let inner = self.inner.clone();
                Box::pin(async move { inner.call(req).await })
            }
        }
    }
}

/// `Infallible` を anything に変換するためのトリック (`Full::map_err` 用)
trait FullExt<B> {
    fn map_err<E, F: FnMut(Infallible) -> E>(
        self,
        f: F,
    ) -> http_body_util::combinators::MapErr<Self, F>
    where
        Self: Sized;
}
impl<B> FullExt<B> for Full<B>
where
    B: bytes::Buf,
{
    fn map_err<E, F: FnMut(Infallible) -> E>(
        self,
        f: F,
    ) -> http_body_util::combinators::MapErr<Self, F>
    where
        Self: Sized,
    {
        http_body_util::BodyExt::map_err(self, f)
    }
}

#[cfg(test)]
mod preflight_tests {
    //! v0.7 #44: unit tests for the OPTIONS preflight interceptor.
    //!
    //! These exercise [`try_handle_preflight`] directly — no hyper
    //! `Incoming` body is needed because the function is generic over
    //! the body type. Behavioural matrix:
    //!
    //! 1. matching preflight → 200 + Allow-* headers
    //! 2. no matching rule (config exists, but origin/method/headers fail)
    //!    → 403
    //! 3. missing `Origin` header → `None` (not a CORS preflight)
    //! 4. non-OPTIONS verb → `None`
    //! 5. no CORS config registered for the bucket → `None`
    //! 6. no manager attached → `None`

    use super::*;
    use crate::cors::{CorsConfig, CorsManager, CorsRule};

    fn rule(origins: &[&str], methods: &[&str], headers: &[&str]) -> CorsRule {
        CorsRule {
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            allowed_headers: headers.iter().map(|s| (*s).to_owned()).collect(),
            expose_headers: vec!["ETag".into()],
            max_age_seconds: Some(600),
            id: Some("test".into()),
        }
    }

    /// Helper: build a `Request<()>` with the given method, path, and
    /// headers — body is ignored by the matcher.
    fn req(
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Request<()> {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).expect("request builder")
    }

    fn manager_with_rule() -> Arc<CorsManager> {
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(
                    &["https://app.example.com"],
                    &["GET", "PUT", "DELETE"],
                    &["Content-Type", "X-Amz-Date"],
                )],
            },
        );
        Arc::new(mgr)
    }

    #[test]
    fn preflight_match_returns_allow_response() {
        let mgr = manager_with_rule();
        let r = req(
            Method::OPTIONS,
            "/b/key.txt",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "PUT"),
                ("access-control-request-headers", "content-type, x-amz-date"),
            ],
        );
        let resp = try_handle_preflight(&r, Some(&mgr)).expect("must intercept");
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers();
        assert_eq!(
            h.get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com")
        );
        assert_eq!(
            h.get("access-control-allow-methods")
                .and_then(|v| v.to_str().ok()),
            Some("GET, PUT, DELETE")
        );
        assert_eq!(
            h.get("access-control-allow-headers")
                .and_then(|v| v.to_str().ok()),
            Some("Content-Type, X-Amz-Date")
        );
        assert_eq!(
            h.get("access-control-max-age")
                .and_then(|v| v.to_str().ok()),
            Some("600")
        );
        assert_eq!(
            h.get("access-control-expose-headers")
                .and_then(|v| v.to_str().ok()),
            Some("ETag")
        );
    }

    #[test]
    fn preflight_no_match_returns_403() {
        let mgr = manager_with_rule();
        // Origin not in allow-list → no rule matches but bucket has CORS
        // config, so we must answer 403 directly (not fall through to
        // s3s, which would otherwise leak the bucket existence via 405).
        let r = req(
            Method::OPTIONS,
            "/b/key.txt",
            &[
                ("origin", "https://evil.example.com"),
                ("access-control-request-method", "PUT"),
            ],
        );
        let resp = try_handle_preflight(&r, Some(&mgr)).expect("must intercept");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // 403 deny response must NOT carry Allow-Origin (RFC 7234 + S3 wire compat).
        assert!(resp.headers().get("access-control-allow-origin").is_none());
    }

    #[test]
    fn preflight_no_origin_falls_through() {
        // OPTIONS without Origin is a generic OPTIONS (e.g. `OPTIONS *`)
        // — not a CORS preflight, must not be intercepted.
        let mgr = manager_with_rule();
        let r = req(
            Method::OPTIONS,
            "/b/key.txt",
            &[("access-control-request-method", "PUT")],
        );
        assert!(try_handle_preflight(&r, Some(&mgr)).is_none());
    }

    #[test]
    fn non_options_falls_through() {
        let mgr = manager_with_rule();
        // Even with Origin + ACRM headers, GET is not a preflight.
        let r = req(
            Method::GET,
            "/b/key.txt",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "PUT"),
            ],
        );
        assert!(try_handle_preflight(&r, Some(&mgr)).is_none());
    }

    #[test]
    fn no_cors_config_for_bucket_falls_through() {
        // Manager attached but no rule registered for "ghost" → fall
        // through to inner service so backend can respond naturally.
        let mgr = manager_with_rule();
        let r = req(
            Method::OPTIONS,
            "/ghost/key.txt",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "PUT"),
            ],
        );
        assert!(try_handle_preflight(&r, Some(&mgr)).is_none());
    }

    #[test]
    fn no_manager_attached_falls_through() {
        let r = req(
            Method::OPTIONS,
            "/b/key.txt",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "PUT"),
            ],
        );
        assert!(try_handle_preflight(&r, None).is_none());
    }

    #[test]
    fn preflight_wildcard_origin_echoes_star() {
        // Rule with `*` origin → response echoes literal "*" (S3 spec).
        let mgr = CorsManager::new();
        mgr.put(
            "b",
            CorsConfig {
                rules: vec![rule(&["*"], &["GET", "PUT"], &["*"])],
            },
        );
        let mgr = Arc::new(mgr);
        let r = req(
            Method::OPTIONS,
            "/b/key",
            &[
                ("origin", "https://anywhere.example"),
                ("access-control-request-method", "PUT"),
                ("access-control-request-headers", "x-custom-header"),
            ],
        );
        let resp = try_handle_preflight(&r, Some(&mgr)).expect("must intercept");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*"),
            "wildcard rule must echo literal '*' instead of requesting origin"
        );
    }

    #[test]
    fn preflight_empty_path_falls_through() {
        let mgr = manager_with_rule();
        let r = req(
            Method::OPTIONS,
            "/",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "PUT"),
            ],
        );
        assert!(try_handle_preflight(&r, Some(&mgr)).is_none());
    }
}

#[cfg(test)]
mod sigv4a_gate_tests {
    //! v0.7 #47: unit tests for the SigV4a verify gate middleware.
    //!
    //! These exercise [`try_sigv4a_verify`] directly — no hyper
    //! `Incoming` body is needed because the function is generic over
    //! the body type. The canonical-request bytes computed by the
    //! middleware are the same bytes the test signs over (we use the
    //! `build_canonical_request_bytes` helper for both sides), so the
    //! happy-path verify is end-to-end byte-exact.
    //!
    //! Behavioural matrix:
    //!
    //! 1. no `AWS4-ECDSA-P256-SHA256` prefix and no region-set header
    //!    → `None` (caller forwards to s3s SigV4 path)
    //! 2. SigV4a Authorization + valid signature → `Some(Ok(()))`
    //! 3. SigV4a Authorization + tampered signature → `Some(Err(403))`
    //!    with `SignatureDoesNotMatch` body
    //! 4. SigV4a Authorization + region-set mismatch → `Some(Err(403))`
    //! 5. gate is `None` (no credential store) → `None` even when the
    //!    request looks SigV4a-shaped (caller forwards, and s3s will
    //!    surface its own "unknown algorithm" error — operator sees the
    //!    misconfiguration rather than a silent pass)
    //! 6. unknown access-key-id → `Some(Err(403))` with
    //!    `InvalidAccessKeyId` body
    //! 7. SigV4a-shaped (region-set header only, no SigV4a auth header)
    //!    → `Some(Err(403))` (we cannot verify without a parseable
    //!    Authorization, fail closed)

    use super::*;

    use std::collections::HashMap;

    use http_body_util::BodyExt;
    use p256::ecdsa::SigningKey;
    use p256::ecdsa::signature::Signer;
    use rand::rngs::OsRng;

    use crate::service::SigV4aGate;
    use crate::sigv4a::{REGION_SET_HEADER, SigV4aCredentialStore};

    fn lower_hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Build a `Request<()>` with the given method, path, and headers.
    fn req(method: Method, path: &str, headers: &[(&str, &str)]) -> Request<()> {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).expect("request builder")
    }

    /// Build the SigV4a Authorization header for the given access-key,
    /// signed-headers list, and signature (lowercase hex DER).
    fn build_auth_header(access_key: &str, signed_headers: &[&str], sig_hex: &str) -> String {
        format!(
            "AWS4-ECDSA-P256-SHA256 \
             Credential={access_key}/20260513/s3/aws4_request, \
             SignedHeaders={}, \
             Signature={sig_hex}",
            signed_headers.join(";")
        )
    }

    /// Build a fully-signed SigV4a `Request<()>` ready for the gate to
    /// verify. Returns the request and the verifying key it should be
    /// loaded against.
    fn make_signed_request(
        access_key: &str,
        method: Method,
        path: &str,
        region_set: &str,
    ) -> (Request<()>, p256::ecdsa::VerifyingKey) {
        let signing = SigningKey::random(&mut OsRng);
        let verifying = p256::ecdsa::VerifyingKey::from(&signing);
        let signed_headers_list = ["host", "x-amz-content-sha256", "x-amz-date", REGION_SET_HEADER];
        // Build the request first WITHOUT the Authorization header so we
        // can compute canonical bytes and sign them; then re-build the
        // request with the Authorization header attached.
        let pre = Request::builder()
            .method(method.clone())
            .uri(path)
            .header("host", "s3.example.com")
            .header(
                "x-amz-content-sha256",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            )
            .header("x-amz-date", "20260513T120000Z")
            .header(REGION_SET_HEADER, region_set)
            .body(())
            .expect("pre-request");
        let signed_headers: Vec<String> =
            signed_headers_list.iter().map(|s| (*s).to_string()).collect();
        let canonical = build_canonical_request_bytes(&pre, &signed_headers);
        let sig: p256::ecdsa::Signature = signing.sign(&canonical);
        let sig_hex = lower_hex(sig.to_der().as_bytes());
        let auth = build_auth_header(access_key, &signed_headers_list, &sig_hex);

        // Rebuild with the Authorization header — every other header
        // value is identical so the canonical bytes the gate computes
        // match what we signed.
        let r = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "s3.example.com")
            .header(
                "x-amz-content-sha256",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            )
            .header("x-amz-date", "20260513T120000Z")
            .header(REGION_SET_HEADER, region_set)
            .header("authorization", auth)
            .body(())
            .expect("signed request");
        (r, verifying)
    }

    fn make_gate_with(access_key: &str, vk: p256::ecdsa::VerifyingKey) -> Arc<SigV4aGate> {
        let mut m = HashMap::new();
        m.insert(access_key.to_string(), vk);
        let store = Arc::new(SigV4aCredentialStore::from_map(m));
        Arc::new(SigV4aGate::new(store))
    }

    /// Drain a `s3s::Body` into bytes for body-content assertions.
    async fn body_to_bytes(resp: Response<s3s::Body>) -> Vec<u8> {
        resp.into_body()
            .collect()
            .await
            .expect("body collect")
            .to_bytes()
            .to_vec()
    }

    #[test]
    fn no_sigv4a_prefix_returns_none() {
        // Plain SigV4 (HMAC-SHA256) request — gate must defer to s3s.
        let (_, vk) = (
            (),
            p256::ecdsa::VerifyingKey::from(&SigningKey::random(&mut OsRng)),
        );
        let gate = make_gate_with("AKIAOK", vk);
        let r = req(
            Method::GET,
            "/bucket/key",
            &[(
                "authorization",
                "AWS4-HMAC-SHA256 Credential=AKIA/20260513/us-east-1/s3/aws4_request, \
                 SignedHeaders=host, Signature=deadbeef",
            )],
        );
        assert!(
            try_sigv4a_verify(&r, Some(&gate), "us-east-1").is_none(),
            "plain SigV4 request must fall through to the inner service"
        );
    }

    #[test]
    fn sigv4a_valid_signature_returns_ok() {
        let (r, vk) =
            make_signed_request("AKIAOK", Method::GET, "/bucket/key", "us-east-1,us-west-2");
        let gate = make_gate_with("AKIAOK", vk);
        let result = try_sigv4a_verify(&r, Some(&gate), "us-east-1")
            .expect("must intercept SigV4a request");
        assert!(
            result.is_ok(),
            "valid SigV4a signature must verify: {result:?}"
        );
    }

    #[tokio::test]
    async fn sigv4a_tampered_signature_returns_403() {
        let (r, vk) = make_signed_request("AKIAOK", Method::GET, "/bucket/key", "us-east-1");
        let gate = make_gate_with("AKIAOK", vk);

        // Tamper one byte of the signature hex inside the Authorization
        // header — the DER decode may still succeed, but ECDSA verify
        // will fail (or the DER decode itself will fail; both surface
        // as `SignatureDoesNotMatch`).
        let auth = r
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .expect("auth header")
            .to_string();
        // Flip the last hex char to corrupt the signature.
        let mut chars: Vec<char> = auth.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == '0' { '1' } else { '0' };
        let tampered_auth: String = chars.into_iter().collect();
        let tampered = req(
            Method::GET,
            "/bucket/key",
            &[
                ("host", "s3.example.com"),
                (
                    "x-amz-content-sha256",
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                ),
                ("x-amz-date", "20260513T120000Z"),
                (REGION_SET_HEADER, "us-east-1"),
                ("authorization", &tampered_auth),
            ],
        );
        let result = try_sigv4a_verify(&tampered, Some(&gate), "us-east-1")
            .expect("must intercept SigV4a request");
        let resp = result.expect_err("tampered signature must surface a 403 response");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_to_bytes(resp).await;
        let body_str = String::from_utf8(body).expect("xml utf-8");
        assert!(
            body_str.contains("<Code>SignatureDoesNotMatch</Code>"),
            "403 body must surface SignatureDoesNotMatch: {body_str}"
        );
    }

    #[tokio::test]
    async fn sigv4a_region_set_mismatch_returns_403() {
        // Sign for `us-east-1` only, then verify with the listener
        // region claiming `eu-west-1` — must fail with
        // SignatureDoesNotMatch (the region-set check sits inside the
        // gate's verify path, and any failure there folds to
        // SignatureDoesNotMatch).
        let (r, vk) = make_signed_request("AKIAOK", Method::GET, "/bucket/key", "us-east-1");
        let gate = make_gate_with("AKIAOK", vk);
        let result = try_sigv4a_verify(&r, Some(&gate), "eu-west-1")
            .expect("must intercept SigV4a request");
        let resp = result.expect_err("region mismatch must produce 403");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_to_bytes(resp).await;
        let body_str = String::from_utf8(body).expect("xml utf-8");
        assert!(
            body_str.contains("<Code>SignatureDoesNotMatch</Code>"),
            "region-set mismatch must surface SignatureDoesNotMatch: {body_str}"
        );
    }

    #[test]
    fn no_gate_attached_returns_none() {
        // Even a SigV4a-shaped request returns None when no gate is
        // installed — the listener will hand it to s3s, which surfaces
        // its own "unknown algorithm" error so the misconfiguration is
        // visible to the operator.
        let (r, _vk) = make_signed_request("AKIAOK", Method::GET, "/bucket/key", "us-east-1");
        assert!(
            try_sigv4a_verify(&r, None, "us-east-1").is_none(),
            "missing gate must defer to inner service"
        );
    }

    #[tokio::test]
    async fn unknown_access_key_returns_403_invalid_access_key_id() {
        // Sign with one key but load the credential store with a
        // different access-key-id → InvalidAccessKeyId.
        let (r, _vk_unused) =
            make_signed_request("AKIAOK", Method::GET, "/bucket/key", "us-east-1");
        let other_signing = SigningKey::random(&mut OsRng);
        let other_vk = p256::ecdsa::VerifyingKey::from(&other_signing);
        let gate = make_gate_with("AKIASOMEONEELSE", other_vk);
        let result = try_sigv4a_verify(&r, Some(&gate), "us-east-1")
            .expect("must intercept SigV4a request");
        let resp = result.expect_err("unknown key must produce 403");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_to_bytes(resp).await;
        let body_str = String::from_utf8(body).expect("xml utf-8");
        assert!(
            body_str.contains("<Code>InvalidAccessKeyId</Code>"),
            "unknown access-key must surface InvalidAccessKeyId: {body_str}"
        );
    }

    #[tokio::test]
    async fn region_set_header_only_without_sigv4a_auth_returns_403() {
        // Some legacy clients stamp the `X-Amz-Region-Set` header
        // before swapping the algorithm string. `detect` flags this as
        // SigV4a-shaped but we cannot verify without a parseable
        // Authorization → fail closed (SignatureDoesNotMatch).
        let signing = SigningKey::random(&mut OsRng);
        let vk = p256::ecdsa::VerifyingKey::from(&signing);
        let gate = make_gate_with("AKIAOK", vk);
        let r = req(
            Method::GET,
            "/bucket/key",
            &[
                // SigV4 algorithm + region-set header → detected, but
                // the Authorization is plain SigV4 so `parse_authorization_header`
                // returns None.
                (
                    "authorization",
                    "AWS4-HMAC-SHA256 Credential=AKIA/20260513/us-east-1/s3/aws4_request, \
                     SignedHeaders=host, Signature=deadbeef",
                ),
                (REGION_SET_HEADER, "us-east-1"),
            ],
        );
        let result = try_sigv4a_verify(&r, Some(&gate), "us-east-1")
            .expect("must intercept SigV4a-shaped request");
        let resp = result.expect_err("region-set without sigv4a auth must produce 403");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_to_bytes(resp).await;
        let body_str = String::from_utf8(body).expect("xml utf-8");
        assert!(
            body_str.contains("<Code>SignatureDoesNotMatch</Code>"),
            "missing/malformed Authorization for SigV4a-shaped request must fail closed: {body_str}"
        );
    }

    /// Cover the canonical-request builder directly: empty query
    /// string, sorted multi-pair query, and header value collapsed
    /// whitespace all hit the right code paths.
    #[test]
    fn canonical_request_bytes_format() {
        let r = req(
            Method::PUT,
            "/bucket/key?z=1&a=2",
            &[
                ("host", "s3.example.com"),
                ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
                ("x-amz-date", "  20260513T120000Z  "),
            ],
        );
        let signed: Vec<String> =
            ["host", "x-amz-content-sha256", "x-amz-date"].iter().map(|s| (*s).into()).collect();
        let bytes = build_canonical_request_bytes(&r, &signed);
        let s = std::str::from_utf8(&bytes).expect("utf-8");
        let expected = "PUT\n\
                        /bucket/key\n\
                        a=2&z=1\n\
                        host:s3.example.com\n\
                        x-amz-content-sha256:UNSIGNED-PAYLOAD\n\
                        x-amz-date:20260513T120000Z\n\
                        \n\
                        host;x-amz-content-sha256;x-amz-date\n\
                        UNSIGNED-PAYLOAD";
        assert_eq!(s, expected, "canonical request bytes mismatch:\n{s}");
    }
}
