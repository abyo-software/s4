//! S4 server crate — `S4Service` (圧縮 hook 付き S3 trait 実装) と関連 helper を提供。

pub mod access_log;
pub mod acme;
pub mod blob;
pub mod metrics;
pub mod policy;
pub mod rate_limit;
pub mod routing;
pub mod service;
pub mod sse;
pub mod streaming;
pub mod tls;

pub use s4_codec as codec;
pub use s4_config as config;
pub use service::S4Service;
