//! S4 サーバの設定型。

use serde::{Deserialize, Serialize};

/// 圧縮戦略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressionMode {
    /// 透過 (圧縮なし、開発/比較用)
    Passthrough,
    /// 入力 sampling で codec を自動選択 (本命モード)
    Auto,
    /// 固定 codec (例: 整数列のみのワークロード向け)
    Fixed,
}

impl Default for CompressionMode {
    fn default() -> Self {
        Self::Auto
    }
}

/// バックエンド S3 接続情報
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    /// バックエンドの endpoint (AWS S3 = `https://s3.<region>.amazonaws.com`)
    pub endpoint_url: String,
    /// バケット名 (S4 がオブジェクトを実保存する先)
    pub bucket: String,
    /// path style を強制するか (互換ストレージ向け)
    #[serde(default)]
    pub force_path_style: bool,
}

/// サーバ設定全体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S4Config {
    /// listen address
    pub listen_addr: String,
    /// listen port
    pub listen_port: u16,
    /// virtual-hosted-style 用 domain (任意)
    #[serde(default)]
    pub virtual_host_domain: Option<String>,
    /// 圧縮戦略
    #[serde(default)]
    pub compression: CompressionMode,
    /// バックエンド
    pub backend: BackendConfig,
}

impl S4Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        // TODO Phase 1: toml crate を加えて実装。spike ではコンパイル通すのみ。
        let _ = text;
        anyhow::bail!("toml loading not implemented yet")
    }
}
