//! `/health` と `/ready` の HTTP routing layer。
//!
//! S3 server と同じポートで health probe に応答できると AWS ALB / NLB / k8s
//! readiness probe との統合が単純になる。
//!
//! - `GET /health` → 常に `200 OK` (server プロセスが生きていれば返す)
//! - `GET /ready` → `ready_check` future を await し、`Ok(())` なら 200、
//!   それ以外 (backend 不通等) は 503。
//! - その他のパス → inner S3Service へ委譲

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::Service;
use hyper::{Request, Response, StatusCode};

/// readiness check 関数。bound is `Send + Sync` for cross-task use.
pub type ReadyCheck =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// inner service と health/ready handler を合成する hyper Service。
#[derive(Clone)]
pub struct HealthRouter<S> {
    pub inner: S,
    pub ready_check: Option<ReadyCheck>,
}

impl<S> HealthRouter<S> {
    pub fn new(inner: S, ready_check: Option<ReadyCheck>) -> Self {
        Self { inner, ready_check }
    }
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
        let path = req.uri().path();
        match (req.method(), path) {
            (&hyper::Method::GET, "/health") | (&hyper::Method::HEAD, "/health") => {
                Box::pin(async { Ok(make_text_response(StatusCode::OK, "ok\n")) })
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
