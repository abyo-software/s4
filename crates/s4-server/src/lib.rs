//! S4 server library — `S4Service` の実装は Phase 1 月 2 で具体化する。
//!
//! 現状はプレースホルダ。`s3s::S3` trait の defaults はすべて NotImplemented を返す
//! ため、実用化するには PUT/GET/HEAD/DELETE/List 系/Multipart 系の十数メソッドを
//! 自前で `s3s_aws::Proxy` に委譲しつつ、PUT/GET 経路にだけ
//! `s4_codec::Codec::compress` / `decompress` を挟む形で実装する予定。
//!
//! ```text
//! pub struct S4Service<C: Codec> {
//!     proxy: s3s_aws::Proxy,
//!     codec: Arc<C>,
//! }
//!
//! #[async_trait::async_trait]
//! impl<C: Codec + 'static> s3s::S3 for S4Service<C> {
//!     // PUT: pre-compress then forward
//!     async fn put_object(&self, req: ...) -> ... { ... }
//!     // GET: forward then post-decompress
//!     async fn get_object(&self, req: ...) -> ... { ... }
//!     // 残りは proxy へそのまま委譲 (delegation macro 検討)
//! }
//! ```

pub use s4_codec as codec;
pub use s4_config as config;
