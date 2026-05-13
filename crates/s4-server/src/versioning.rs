//! v0.5 #34: First-class versioning state machine.
//!
//! S4-server に object version の **own state** を持たせる module。これまで
//! versioning は backend (s3s framework) への passthrough でしか機能していなかった
//! が、本 module で S4 自身が
//!
//! - per-bucket の Versioning state (Enabled / Suspended / Unversioned)
//! - per-(bucket, key) の version chain (`Vec<VersionEntry>`、最新が末尾)
//! - delete marker
//! - version-id 採番 (UUIDv4)
//!
//! を所有する。`crates/s4-server/src/service.rs` の `put_object` /
//! `get_object` / `delete_object` / `list_object_versions` /
//! `get_bucket_versioning` / `put_bucket_versioning` handler が `S4Service`
//! 経由で `VersioningManager` を呼び出して、AWS S3 wire-compat な振る舞いを
//! 実現する。
//!
//! ## scope (v0.5 #34)
//!
//! - in-memory only (single instance scope)。multi-instance replication は
//!   v0.6+ で別 issue として扱う
//! - `to_json` / `from_json` で snapshot を取る API は提供する。`main.rs` 側で
//!   `--versioning-state-file` flag を将来追加する hook として使える
//! - MFA delete はサポートしない (本 task の scope 外)
//!
//! ## semantics
//!
//! - **version_id format**: UUIDv4 を 32-char hex (no dash) で表現。AWS 互換
//!   実装では base64-url や custom encoding が多いが、UUIDv4 hex は十分一意で
//!   debug 容易、URL-safe 文字のみで構成され `x-amz-version-id` header /
//!   `versionId` query param に何の escape も不要
//! - **null version**: Suspended bucket での PUT、または初期 Unversioned bucket で
//!   作成された object の version_id は文字列 `"null"`。Suspended bucket の同 key
//!   への次 PUT は既存 null version を **上書き** する (S3 仕様準拠)
//! - **delete marker**: Enabled bucket への DELETE (version_id 指定なし) は
//!   新規 delete marker (version_id 採番) を chain の末尾に追加する。GET (version_id
//!   指定なし) は最新が delete marker なら NoSuchKey 404 を返す
//! - **specific-version DELETE**: version_id を指定した DELETE は当該 entry を
//!   chain から物理削除する。delete marker を狙い撃ちで消すと、その下の
//!   version が再び latest として可視になる (= "undelete")。Suspended /
//!   Unversioned bucket でも version_id 指定 DELETE は受け付ける (chain 中の
//!   null version も狙える)

use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Per-version metadata. `is_delete_marker` が true の entry は backend storage
/// に bytes を持たない (= tombstone) — `etag` は空 / `size` は 0 になる。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionEntry {
    /// `"null"` (Suspended / Unversioned に書かれた version) または UUIDv4 hex
    /// (Enabled bucket で生成された version)。
    pub version_id: String,
    /// 圧縮済 / 平文 bytes の MD5 / S4 内部 crc 由来 etag。delete marker は `""`。
    pub etag: String,
    /// 客 (= decompressed) サイズ。delete marker は 0。
    pub size: u64,
    pub is_delete_marker: bool,
    pub created_at: DateTime<Utc>,
}

/// per-(bucket, key) chain (最新版が `Vec` の末尾) の in-memory map。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VersionIndex {
    /// `bucket → key → chain (oldest..latest)`
    pub buckets: HashMap<String, HashMap<String, Vec<VersionEntry>>>,
}

/// Per-bucket versioning state。AWS S3 では `Enabled` / `Suspended` の二択
/// (作成直後の bucket は status 未設定 = Unversioned 相当) なので、3 値に分けて
/// 管理する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VersioningState {
    /// PUT は新 version_id を採番、DELETE は delete marker を追加。
    Enabled,
    /// PUT は version_id = `"null"` で既存 null version を overwrite、
    /// DELETE は null delete marker を追加 (chain 中の他 version は残る)。
    Suspended,
    /// 旧来の non-versioned 動作 (S4 が version index を持たない bucket。
    /// `get_bucket_versioning` は `None` 相当を返す)。
    Unversioned,
}

impl VersioningState {
    /// AWS wire format (`"Enabled"` / `"Suspended"` / `""`) との往復用。
    #[must_use]
    pub fn as_aws_status(self) -> Option<&'static str> {
        match self {
            Self::Enabled => Some("Enabled"),
            Self::Suspended => Some("Suspended"),
            Self::Unversioned => None,
        }
    }
}

/// AWS-style `"null"` literal は version-id 全体で唯一の予約名。chain 内に
/// 同時に複数存在することは無い (= Suspended bucket は最大 1 entry を保持)。
pub const NULL_VERSION_ID: &str = "null";

/// snapshot のシリアライズ format。`to_json` / `from_json` 用。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VersioningSnapshot {
    pub index: VersionIndex,
    pub state: HashMap<String, VersioningState>,
}

/// per-bucket versioning state + per-(bucket, key) version chain を一元管理する
/// 上位 manager。すべての書き込み操作は `RwLock` write 経由で atomic、すべての
/// 読み出しは read 経由 (chain は `Vec<VersionEntry>` の clone を返す)。
#[derive(Debug, Default)]
pub struct VersioningManager {
    index: RwLock<VersionIndex>,
    state: RwLock<HashMap<String, VersioningState>>,
}

/// `record_put` / `record_delete` の戻り値。handler 側で response の
/// `x-amz-version-id` 等を組み立てるために使う。
#[derive(Debug, Clone)]
pub struct PutOutcome {
    /// 新規採番された (or `"null"`) version_id。
    pub version_id: String,
    /// 当該 PUT が Enabled bucket で行われたか (= response に `x-amz-version-id`
    /// を含めるべきか) を示す。Unversioned bucket では false → handler は
    /// version_id を response に出さない。
    pub versioned_response: bool,
}

#[derive(Debug, Clone)]
pub struct DeleteOutcome {
    /// Enabled bucket での delete marker 追加なら新 version_id。Suspended で
    /// null version を消した場合 / specific-version delete の場合は消えた
    /// entry の version_id。Unversioned bucket は `None` (handler は単に
    /// backend に delete を流す)。
    pub version_id: Option<String>,
    /// 当該 delete 操作で生成 / 削除された entry が delete marker だったか。
    pub is_delete_marker: bool,
}

impl VersioningManager {
    /// 空 manager。bucket 毎の state も空 (= 全 bucket Unversioned 扱い)。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 新 version_id を採番 (UUIDv4 を `simple()` = 32-char hex で表現)。
    ///
    /// AWS S3 の `x-amz-version-id` は base64-url 風の不透明文字列だが、S4 では
    /// 「URL-safe な短い hex」を採用する。32 char で衝突確率は実用上ゼロ
    /// (UUIDv4 = 122-bit randomness)、`versionId` query param で escape 不要、
    /// debug log にそのまま貼れる。
    #[must_use]
    pub fn new_version_id() -> String {
        Uuid::new_v4().simple().to_string()
    }

    /// Bucket の versioning state を取得。未設定は `Unversioned`。
    #[must_use]
    pub fn state(&self, bucket: &str) -> VersioningState {
        crate::lock_recovery::recover_read(&self.state, "versioning.state")
            .get(bucket)
            .copied()
            .unwrap_or(VersioningState::Unversioned)
    }

    /// `put_bucket_versioning` handler から呼ぶ。
    pub fn set_state(&self, bucket: &str, state: VersioningState) {
        crate::lock_recovery::recover_write(&self.state, "versioning.state")
            .insert(bucket.to_owned(), state);
    }

    /// PUT 経路 (テスト / state machine 単独実証用)。state に応じて新 version_id を
    /// 採番 (Enabled) / `"null"` を使う (Suspended / Unversioned)。Suspended は既存
    /// null version を overwrite する (chain 中の null version を 1 件まで restrict)。
    ///
    /// `service.rs` の handler は backend write の前後で `new_version_id` の
    /// 事前採番 + [`commit_put_with_version`] を使うので本関数を直接は呼ばないが、
    /// state machine 単体テスト + 公開 API として残しておく (snapshot loader 等から
    /// programmatic に index を組む経路で便利)。
    pub fn record_put(&self, bucket: &str, key: &str, etag: String, size: u64) -> PutOutcome {
        let state = self.state(bucket);
        let now = Utc::now();
        let (version_id, versioned_response) = match state {
            VersioningState::Enabled => (Self::new_version_id(), true),
            VersioningState::Suspended | VersioningState::Unversioned => {
                (NULL_VERSION_ID.to_owned(), false)
            }
        };
        self.commit_put_with_version(
            bucket,
            key,
            VersionEntry {
                version_id: version_id.clone(),
                etag,
                size,
                is_delete_marker: false,
                created_at: now,
            },
        );
        PutOutcome {
            version_id,
            versioned_response,
        }
    }

    /// 事前採番済 [`VersionEntry`] を chain に commit する。`service.rs` の PUT
    /// handler は backend write の **前** に [`new_version_id`] で vid を確保し
    /// (rewrite 用)、backend write が成功したら本関数で commit する。これにより
    /// response の `x-amz-version-id` と shadow backend key (`<key>.__s4ver__/<vid>`)
    /// が同じ vid で揃う。
    ///
    /// Suspended (vid = `"null"`) を commit する場合は既存 null version を物理
    /// overwrite する (S3 仕様: Suspended bucket の null version は唯一)。Enabled の
    /// vid (UUIDv4) を commit する場合は単純に末尾 push。
    pub fn commit_put_with_version(&self, bucket: &str, key: &str, entry: VersionEntry) {
        let mut idx = crate::lock_recovery::recover_write(&self.index, "versioning.index");
        let chain = idx
            .buckets
            .entry(bucket.to_owned())
            .or_default()
            .entry(key.to_owned())
            .or_default();
        if entry.version_id == NULL_VERSION_ID {
            chain.retain(|e| e.version_id != NULL_VERSION_ID);
        }
        chain.push(entry);
    }

    /// version_id 指定なしの DELETE 経路。
    ///
    /// - Enabled → 新 version_id を採番した delete marker を chain 末尾に push。
    ///   `DeleteOutcome.version_id = Some(<new_vid>)`、`is_delete_marker = true`。
    /// - Suspended → null delete marker を 1 件追加 (既存 null version を replace、
    ///   S3 仕様)。
    /// - Unversioned → chain 全消し (= 単純物理削除)。
    pub fn record_delete(&self, bucket: &str, key: &str) -> DeleteOutcome {
        let state = self.state(bucket);
        let now = Utc::now();
        let mut idx = crate::lock_recovery::recover_write(&self.index, "versioning.index");
        let chain = idx
            .buckets
            .entry(bucket.to_owned())
            .or_default()
            .entry(key.to_owned())
            .or_default();
        match state {
            VersioningState::Enabled => {
                let vid = Self::new_version_id();
                chain.push(VersionEntry {
                    version_id: vid.clone(),
                    etag: String::new(),
                    size: 0,
                    is_delete_marker: true,
                    created_at: now,
                });
                DeleteOutcome {
                    version_id: Some(vid),
                    is_delete_marker: true,
                }
            }
            VersioningState::Suspended => {
                chain.retain(|e| e.version_id != NULL_VERSION_ID);
                chain.push(VersionEntry {
                    version_id: NULL_VERSION_ID.to_owned(),
                    etag: String::new(),
                    size: 0,
                    is_delete_marker: true,
                    created_at: now,
                });
                DeleteOutcome {
                    version_id: Some(NULL_VERSION_ID.to_owned()),
                    is_delete_marker: true,
                }
            }
            VersioningState::Unversioned => {
                chain.clear();
                DeleteOutcome {
                    version_id: None,
                    is_delete_marker: false,
                }
            }
        }
    }

    /// version_id 指定 DELETE 経路。当該 entry を chain から物理削除する。
    /// Enabled / Suspended / Unversioned 関係なく動く (specific-version DELETE は
    /// state に依存しない S3 仕様)。chain が空になった場合は entry を index から
    /// 削除する (cleanup)。
    pub fn record_delete_specific(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Option<DeleteOutcome> {
        let mut idx = crate::lock_recovery::recover_write(&self.index, "versioning.index");
        let bucket_map = idx.buckets.get_mut(bucket)?;
        let chain = bucket_map.get_mut(key)?;
        let pos = chain.iter().position(|e| e.version_id == version_id)?;
        let removed = chain.remove(pos);
        if chain.is_empty() {
            bucket_map.remove(key);
        }
        Some(DeleteOutcome {
            version_id: Some(removed.version_id),
            is_delete_marker: removed.is_delete_marker,
        })
    }

    /// version_id 指定 GET 経路。当該 entry の clone を返す。
    pub fn lookup_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Option<VersionEntry> {
        let idx = crate::lock_recovery::recover_read(&self.index, "versioning.index");
        idx.buckets
            .get(bucket)?
            .get(key)?
            .iter()
            .find(|e| e.version_id == version_id)
            .cloned()
    }

    /// 最新 (= chain 末尾) の version を返す。chain 末尾が delete marker の場合
    /// もそのまま返す — 客側 (handler) が `is_delete_marker` を見て 404 を
    /// 投げるかどうか決める。
    pub fn lookup_latest(&self, bucket: &str, key: &str) -> Option<VersionEntry> {
        let idx = crate::lock_recovery::recover_read(&self.index, "versioning.index");
        idx.buckets.get(bucket)?.get(key)?.last().cloned()
    }

    /// `list_object_versions` 経路。bucket 内の全 (key, version) を S3 仕様の
    /// 順序 (key asc → 同 key 内は新→旧) に展開する。
    ///
    /// `prefix` で key 先頭一致 filter、`key_marker` (key より大), `version_id_marker`
    /// (key_marker と組で使う、当該 version より後の entry から) で paginate、
    /// `max_keys` 件で truncate。
    ///
    /// 戻り値は `(versions, delete_markers, is_truncated, next_key_marker,
    /// next_version_id_marker)`。`is_truncated = true` の時のみ next_* が
    /// `Some(...)` を返す。
    #[allow(clippy::too_many_arguments)]
    pub fn list_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        version_id_marker: Option<&str>,
        max_keys: usize,
    ) -> ListVersionsPage {
        let idx = crate::lock_recovery::recover_read(&self.index, "versioning.index");
        let Some(bucket_map) = idx.buckets.get(bucket) else {
            return ListVersionsPage::default();
        };
        let mut keys: Vec<&String> = bucket_map.keys().collect();
        keys.sort();
        let mut versions: Vec<ListVersionEntry> = Vec::new();
        let mut delete_markers: Vec<ListVersionEntry> = Vec::new();
        let mut version_marker_consumed = version_id_marker.is_none();
        let mut last_key: Option<String> = None;
        let mut last_vid: Option<String> = None;
        let mut truncated = false;
        let max_keys = max_keys.max(1);

        'outer: for key in keys {
            if let Some(p) = prefix
                && !key.starts_with(p)
            {
                continue;
            }
            // key_marker: skip everything strictly less than the marker.
            if let Some(km) = key_marker
                && key.as_str() < km
            {
                continue;
            }
            // If we're past the marker key, the version-id marker no longer
            // gates anything.
            if let Some(km) = key_marker
                && key.as_str() > km
            {
                version_marker_consumed = true;
            }
            let chain = bucket_map.get(key).expect("just iterated");
            let entries: Vec<&VersionEntry> = chain.iter().rev().collect();
            for (i, e) in entries.iter().enumerate() {
                if !version_marker_consumed {
                    if Some(e.version_id.as_str()) == version_id_marker {
                        version_marker_consumed = true;
                    }
                    continue;
                }
                let total_emitted = versions.len() + delete_markers.len();
                if total_emitted >= max_keys {
                    truncated = true;
                    last_key = Some(key.clone());
                    last_vid = Some(e.version_id.clone());
                    break 'outer;
                }
                let is_latest = i == 0;
                let row = ListVersionEntry {
                    key: key.clone(),
                    version_id: e.version_id.clone(),
                    is_latest,
                    is_delete_marker: e.is_delete_marker,
                    etag: e.etag.clone(),
                    size: e.size,
                    last_modified: e.created_at,
                };
                if e.is_delete_marker {
                    delete_markers.push(row);
                } else {
                    versions.push(row);
                }
            }
            // moving to the next key: any version-id marker only applied to
            // the first (resumed) key.
            version_marker_consumed = true;
        }
        ListVersionsPage {
            versions,
            delete_markers,
            is_truncated: truncated,
            next_key_marker: last_key,
            next_version_id_marker: last_vid,
        }
    }

    /// snapshot を JSON 文字列にして返す。`--versioning-state-file` を将来追加
    /// する時に SIGUSR1 等で dump するために使える。今 task では in-memory 専用
    /// なので公開 API としてのみ提供。
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let snap = VersioningSnapshot {
            index: VersionIndex {
                buckets: crate::lock_recovery::recover_read(&self.index, "versioning.index")
                    .buckets
                    .clone(),
            },
            state: crate::lock_recovery::recover_read(&self.state, "versioning.state").clone(),
        };
        serde_json::to_string(&snap)
    }

    /// snapshot JSON から restore。起動時に `--versioning-state-file` を読み
    /// 込む経路で使える。
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let snap: VersioningSnapshot = serde_json::from_str(s)?;
        Ok(Self {
            index: RwLock::new(snap.index),
            state: RwLock::new(snap.state),
        })
    }
}

/// `list_versions` の戻り値 row。`service.rs` 側で s3s `ObjectVersion` /
/// `DeleteMarkerEntry` に詰め直す。
#[derive(Debug, Clone)]
pub struct ListVersionEntry {
    pub key: String,
    pub version_id: String,
    pub is_latest: bool,
    pub is_delete_marker: bool,
    pub etag: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct ListVersionsPage {
    pub versions: Vec<ListVersionEntry>,
    pub delete_markers: Vec<ListVersionEntry>,
    pub is_truncated: bool,
    pub next_key_marker: Option<String>,
    pub next_version_id_marker: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_put_creates_unique_version_id() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let p1 = m.record_put("b", "k", "etag1".into(), 10);
        let p2 = m.record_put("b", "k", "etag2".into(), 20);
        assert_ne!(p1.version_id, p2.version_id);
        assert!(p1.versioned_response);
        assert!(p2.versioned_response);
        let chain_len = m.list_versions("b", None, None, None, 100).versions.len();
        assert_eq!(chain_len, 2);
    }

    #[test]
    fn suspended_put_overwrites_null_version() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Suspended);
        let p1 = m.record_put("b", "k", "etag1".into(), 10);
        let p2 = m.record_put("b", "k", "etag2".into(), 20);
        assert_eq!(p1.version_id, NULL_VERSION_ID);
        assert_eq!(p2.version_id, NULL_VERSION_ID);
        let page = m.list_versions("b", None, None, None, 100);
        assert_eq!(page.versions.len(), 1);
        assert_eq!(page.versions[0].etag, "etag2");
    }

    #[test]
    fn enabled_delete_creates_marker_at_tail() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let _p = m.record_put("b", "k", "e".into(), 1);
        let d = m.record_delete("b", "k");
        assert!(d.is_delete_marker);
        let latest = m.lookup_latest("b", "k").unwrap();
        assert!(latest.is_delete_marker);
    }

    #[test]
    fn delete_specific_version_keeps_others() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let p1 = m.record_put("b", "k", "e1".into(), 1);
        let p2 = m.record_put("b", "k", "e2".into(), 2);
        let removed = m.record_delete_specific("b", "k", &p1.version_id).unwrap();
        assert_eq!(removed.version_id.as_deref(), Some(p1.version_id.as_str()));
        assert!(!removed.is_delete_marker);
        let page = m.list_versions("b", None, None, None, 100);
        assert_eq!(page.versions.len(), 1);
        assert_eq!(page.versions[0].version_id, p2.version_id);
        assert!(page.versions[0].is_latest);
    }

    #[test]
    fn list_versions_orders_latest_first_per_key() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let p1 = m.record_put("b", "k", "e1".into(), 1);
        let p2 = m.record_put("b", "k", "e2".into(), 2);
        let page = m.list_versions("b", None, None, None, 100);
        assert_eq!(page.versions.len(), 2);
        assert_eq!(page.versions[0].version_id, p2.version_id);
        assert!(page.versions[0].is_latest);
        assert_eq!(page.versions[1].version_id, p1.version_id);
        assert!(!page.versions[1].is_latest);
    }

    #[test]
    fn list_versions_separates_delete_markers() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let _ = m.record_put("b", "k", "e1".into(), 1);
        let _ = m.record_delete("b", "k");
        let page = m.list_versions("b", None, None, None, 100);
        assert_eq!(page.versions.len(), 1);
        assert_eq!(page.delete_markers.len(), 1);
        assert!(page.delete_markers[0].is_latest);
        assert!(!page.versions[0].is_latest);
    }

    #[test]
    fn list_versions_prefix_filter() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let _ = m.record_put("b", "fruit/apple", "e".into(), 1);
        let _ = m.record_put("b", "fruit/banana", "e".into(), 1);
        let _ = m.record_put("b", "veg/carrot", "e".into(), 1);
        let page = m.list_versions("b", Some("fruit/"), None, None, 100);
        assert_eq!(page.versions.len(), 2);
        for v in &page.versions {
            assert!(v.key.starts_with("fruit/"));
        }
    }

    #[test]
    fn list_versions_paginates_and_truncates() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let _ = m.record_put("b", "a", "e".into(), 1);
        let _ = m.record_put("b", "b", "e".into(), 1);
        let _ = m.record_put("b", "c", "e".into(), 1);
        let page = m.list_versions("b", None, None, None, 2);
        assert_eq!(page.versions.len(), 2);
        assert!(page.is_truncated);
        assert_eq!(page.next_key_marker.as_deref(), Some("c"));
        let page2 = m.list_versions("b", None, page.next_key_marker.as_deref(), None, 10);
        assert_eq!(page2.versions.len(), 1);
        assert_eq!(page2.versions[0].key, "c");
        assert!(!page2.is_truncated);
    }

    #[test]
    fn snapshot_roundtrip() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let _ = m.record_put("b", "k", "e1".into(), 1);
        let _ = m.record_delete("b", "k");
        let json = m.to_json().expect("to_json");
        let m2 = VersioningManager::from_json(&json).expect("from_json");
        let p1 = m.list_versions("b", None, None, None, 100);
        let p2 = m2.list_versions("b", None, None, None, 100);
        assert_eq!(p1.versions.len(), p2.versions.len());
        assert_eq!(p1.delete_markers.len(), p2.delete_markers.len());
        assert_eq!(m.state("b"), m2.state("b"));
    }

    /// v0.8.4 #77 (audit H-8): a panic inside a write-guarded section
    /// poisons the inner `index` `RwLock`. Without
    /// [`crate::lock_recovery::recover_read`] the next `to_json` call
    /// (e.g. SIGUSR1 dump-back) would re-panic and take the gateway
    /// down. The recover-on-poison path must surface the post-panic
    /// data instead.
    #[test]
    fn versioning_to_json_after_panic_recovers_via_poison() {
        let m = VersioningManager::new();
        m.set_state("b", VersioningState::Enabled);
        let _ = m.record_put("b", "k", "etag1".into(), 10);
        // Force-poison the index lock by panicking inside a write guard.
        let m = std::sync::Arc::new(m);
        let m_cl = std::sync::Arc::clone(&m);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = m_cl.index.write().expect("clean lock");
            g.buckets.entry("b".into()).or_default();
            panic!("force-poison");
        }));
        assert!(m.index.is_poisoned(), "write panic must poison index lock");
        // to_json must NOT re-panic and must round-trip the pre-panic data.
        let json = m.to_json().expect("to_json after poison must succeed");
        let m2 = VersioningManager::from_json(&json).expect("from_json");
        let page = m2.list_versions("b", None, None, None, 100);
        assert_eq!(page.versions.len(), 1, "recovered snapshot keeps version");
        assert_eq!(m2.state("b"), VersioningState::Enabled);
    }
}
