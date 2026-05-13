//! S4 server crate — `S4Service` (圧縮 hook 付き S3 trait 実装) と関連 helper を提供。

pub mod access_log;
pub mod acme;
pub mod audit_log;
pub mod blob;
pub mod cors;
pub mod inventory;
pub mod kms;
pub mod lifecycle;
pub mod metrics;
pub mod mfa;
pub mod notifications;
pub mod object_lock;
pub mod policy;
pub mod rate_limit;
pub mod replication;
pub mod routing;
pub mod select;
pub mod service;
pub mod sigv4a;
pub mod sse;
pub mod streaming;
pub mod tagging;
pub mod tls;
pub mod versioning;

pub use s4_codec as codec;
pub use s4_config as config;
pub use service::S4Service;
