//! PUT 時にどの codec で圧縮するかを選ぶ dispatcher。
//!
//! Phase 1 では「常に同じ codec を選ぶ」`AlwaysDispatcher` を提供。
//! Phase 1 後半で `SamplingDispatcher` を追加し、入力先頭の sampling で
//! integer 主体 / text 主体 / 既圧縮 を判定して codec を切り替える。

use crate::CodecKind;

/// PUT body の先頭 sample から codec を選ぶ trait。
#[async_trait::async_trait]
pub trait CodecDispatcher: Send + Sync {
    async fn pick(&self, sample: &[u8]) -> CodecKind;
}

/// 常に同じ kind を返す dispatcher (固定 codec 運用)。
#[derive(Debug, Clone, Copy)]
pub struct AlwaysDispatcher(pub CodecKind);

#[async_trait::async_trait]
impl CodecDispatcher for AlwaysDispatcher {
    async fn pick(&self, _sample: &[u8]) -> CodecKind {
        self.0
    }
}

/// `Box<dyn CodecDispatcher>` からも `CodecDispatcher` として使えるようにする blanket impl
#[async_trait::async_trait]
impl<T: CodecDispatcher + ?Sized> CodecDispatcher for Box<T> {
    async fn pick(&self, sample: &[u8]) -> CodecKind {
        (**self).pick(sample).await
    }
}

#[async_trait::async_trait]
impl<T: CodecDispatcher + ?Sized> CodecDispatcher for std::sync::Arc<T> {
    async fn pick(&self, sample: &[u8]) -> CodecKind {
        (**self).pick(sample).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn always_dispatcher_returns_configured_kind() {
        let d = AlwaysDispatcher(CodecKind::CpuZstd);
        assert_eq!(d.pick(b"any input").await, CodecKind::CpuZstd);
    }

    #[tokio::test]
    async fn boxed_dispatcher_works() {
        let d: Box<dyn CodecDispatcher> = Box::new(AlwaysDispatcher(CodecKind::Passthrough));
        assert_eq!(d.pick(b"x").await, CodecKind::Passthrough);
    }
}
