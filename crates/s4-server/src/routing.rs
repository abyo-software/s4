//! `/health` と `/ready` の HTTP routing layer + CORS OPTIONS preflight
//! interceptor。
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

/// readiness check 関数。bound is `Send + Sync` for cross-task use.
pub type ReadyCheck =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// inner service と health/ready/metrics + CORS preflight handler を合成する
/// hyper Service。
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
}

impl<S> HealthRouter<S> {
    pub fn new(inner: S, ready_check: Option<ReadyCheck>) -> Self {
        Self {
            inner,
            ready_check,
            metrics_handle: None,
            cors_manager: None,
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
