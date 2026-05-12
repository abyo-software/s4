//! `s3s::S3` 実装 — `s3s_aws::Proxy` への delegation を default にしつつ、
//! `put_object` / `get_object` 経路で `s4_codec::Codec` を呼ぶ。
//!
//! ## カバー範囲 (Phase 1 月 2 想定)
//!
//! - 圧縮 hook あり: `put_object`, `get_object`
//! - 純 delegation (圧縮なし): `head_bucket`, `list_buckets`, `create_bucket`, `delete_bucket`,
//!   `head_object`, `delete_object`, `delete_objects`, `copy_object`, `list_objects`,
//!   `list_objects_v2`, `create_multipart_upload`, `upload_part`,
//!   `complete_multipart_upload`, `abort_multipart_upload`, `list_multipart_uploads`,
//!   `list_parts`
//! - 未対応 (デフォルトで NotImplemented): その他 80+ ops (Tagging / ACL / Lifecycle 等は Phase 2)
//!
//! ## 重要な制限事項
//!
//! - **Multipart Upload は per-part 圧縮が未実装**: 現状は upload_part を素通し
//!   している。Phase 1 月 2 後半で per-part 圧縮 + complete_multipart_upload で
//!   manifest を集約する設計を入れる。
//! - **codec mismatch**: GET 時に object metadata の codec が S4Service に設定された
//!   codec と異なる場合エラー。Phase 1 後半で `CodecRegistry` を導入し manifest 別に
//!   dispatch するまでの暫定挙動。
//! - **PUT body は memory に collect**: max_body_bytes 上限あり (default 5 GB = S3 単発 PUT 上限)。
//!   Streaming-aware 圧縮は Phase 2。

use std::sync::Arc;

use s3s::dto::*;
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use s4_codec::{ChunkManifest, Codec, CodecKind};
use tracing::{debug, warn};

use crate::blob::{bytes_to_blob, collect_blob};

/// S4 のメインサービス。任意の `S3` backend と `Codec` を組み合わせる。
///
/// production: `B = s3s_aws::Proxy` (AWS S3 への forward)
/// tests: `B = s3s_fs::FileSystem` (ローカル FS backend で in-process roundtrip)
pub struct S4Service<B: S3, C: Codec + 'static> {
    backend: B,
    codec: Arc<C>,
    max_body_bytes: usize,
}

impl<B: S3, C: Codec + 'static> S4Service<B, C> {
    /// AWS S3 単発 PUT の API 上限 (5 GiB)
    pub const DEFAULT_MAX_BODY_BYTES: usize = 5 * 1024 * 1024 * 1024;

    pub fn new(backend: B, codec: Arc<C>) -> Self {
        Self {
            backend,
            codec,
            max_body_bytes: Self::DEFAULT_MAX_BODY_BYTES,
        }
    }

    pub fn with_max_body_bytes(mut self, n: usize) -> Self {
        self.max_body_bytes = n;
        self
    }
}

const META_CODEC: &str = "s4-codec";
const META_ORIGINAL_SIZE: &str = "s4-original-size";
const META_COMPRESSED_SIZE: &str = "s4-compressed-size";
const META_CRC32C: &str = "s4-crc32c";

/// metadata から ChunkManifest を再構築。S4 が書いていないオブジェクト (= S4 経由前から
/// 既に bucket にあったもの) は None。
fn extract_manifest(metadata: &Option<Metadata>) -> Option<ChunkManifest> {
    let m = metadata.as_ref()?;
    let codec = m
        .get(META_CODEC)
        .and_then(|s| s.parse::<CodecKind>().ok())?;
    let original_size = m.get(META_ORIGINAL_SIZE)?.parse().ok()?;
    let compressed_size = m.get(META_COMPRESSED_SIZE)?.parse().ok()?;
    let crc32c = m.get(META_CRC32C)?.parse().ok()?;
    Some(ChunkManifest {
        codec,
        original_size,
        compressed_size,
        crc32c,
    })
}

fn write_manifest(metadata: &mut Option<Metadata>, manifest: &ChunkManifest) {
    let meta = metadata.get_or_insert_with(Default::default);
    meta.insert(META_CODEC.into(), manifest.codec.as_str().into());
    meta.insert(
        META_ORIGINAL_SIZE.into(),
        manifest.original_size.to_string(),
    );
    meta.insert(
        META_COMPRESSED_SIZE.into(),
        manifest.compressed_size.to_string(),
    );
    meta.insert(META_CRC32C.into(), manifest.crc32c.to_string());
}

fn internal<E: std::fmt::Display>(prefix: &'static str) -> impl FnOnce(E) -> S3Error {
    move |e| S3Error::with_message(S3ErrorCode::InternalError, format!("{prefix}: {e}"))
}

#[async_trait::async_trait]
impl<B: S3, C: Codec + 'static> S3 for S4Service<B, C> {
    // === 圧縮を挟む path (PUT) ===
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        if let Some(blob) = req.input.body.take() {
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect put body"))?;
            debug!(
                bucket = ?req.input.bucket,
                key = ?req.input.key,
                bytes = bytes.len(),
                codec = self.codec.kind().as_str(),
                "S4 put_object: compressing"
            );
            let (compressed, manifest) = self
                .codec
                .compress(bytes)
                .await
                .map_err(internal("codec compress"))?;
            write_manifest(&mut req.input.metadata, &manifest);
            req.input.body = Some(bytes_to_blob(compressed));
        }
        self.backend.put_object(req).await
    }

    // === 圧縮を解く path (GET) ===
    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let mut resp = self.backend.get_object(req).await?;
        let Some(manifest) = extract_manifest(&resp.output.metadata) else {
            // S4 が書いていないオブジェクト (passthrough)。bucket に S4 経由前から
            // あった可能性、または PUT-without-S4 で書かれたもの。そのまま返す。
            debug!("S4 get_object: object lacks s4-codec metadata, returning as-is");
            return Ok(resp);
        };
        if manifest.codec != self.codec.kind() && manifest.codec != CodecKind::Passthrough {
            warn!(
                want = self.codec.kind().as_str(),
                got = manifest.codec.as_str(),
                "S4 get_object: codec mismatch (Phase 1 limitation: single-codec service)"
            );
            return Err(S3Error::with_message(
                S3ErrorCode::InternalError,
                format!(
                    "object compressed with {} but this S4 instance is configured with {}; \
                     multi-codec dispatch is Phase 2",
                    manifest.codec.as_str(),
                    self.codec.kind().as_str()
                ),
            ));
        }
        if let Some(blob) = resp.output.body.take() {
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect get body"))?;
            // codec mismatch を上で弾いているので、Passthrough manifest を current codec で
            // 解凍するケースが残るが、Passthrough の manifest を非Passthrough codec が
            // decompress すると codec.decompress() 側で CodecMismatch が返る。
            // そのため Passthrough manifest かつ current codec が非Passthrough のときは
            // Passthrough codec で処理する分岐を別に用意する。
            let decompressed = if manifest.codec == CodecKind::Passthrough {
                let pt = s4_codec::passthrough::Passthrough;
                pt.decompress(bytes, &manifest)
                    .await
                    .map_err(internal("passthrough decompress"))?
            } else {
                self.codec
                    .decompress(bytes, &manifest)
                    .await
                    .map_err(internal("codec decompress"))?
            };
            resp.output.body = Some(bytes_to_blob(decompressed));
        }
        Ok(resp)
    }

    // === passthrough delegations (1 op = 3 行) ===
    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        self.backend.head_bucket(req).await
    }
    async fn list_buckets(
        &self,
        req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        self.backend.list_buckets(req).await
    }
    async fn create_bucket(
        &self,
        req: S3Request<CreateBucketInput>,
    ) -> S3Result<S3Response<CreateBucketOutput>> {
        self.backend.create_bucket(req).await
    }
    async fn delete_bucket(
        &self,
        req: S3Request<DeleteBucketInput>,
    ) -> S3Result<S3Response<DeleteBucketOutput>> {
        self.backend.delete_bucket(req).await
    }
    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        self.backend.head_object(req).await
    }
    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        self.backend.delete_object(req).await
    }
    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        self.backend.delete_objects(req).await
    }
    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        self.backend.copy_object(req).await
    }
    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        self.backend.list_objects(req).await
    }
    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        self.backend.list_objects_v2(req).await
    }
    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        self.backend.create_multipart_upload(req).await
    }
    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        // TODO Phase 1 後半: per-part 圧縮を入れる (現状は素通し)
        self.backend.upload_part(req).await
    }
    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        self.backend.complete_multipart_upload(req).await
    }
    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        self.backend.abort_multipart_upload(req).await
    }
    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        self.backend.list_multipart_uploads(req).await
    }
    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        self.backend.list_parts(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s4_codec::passthrough::Passthrough;

    #[test]
    fn manifest_roundtrip_via_metadata() {
        let original = ChunkManifest {
            codec: CodecKind::CpuZstd,
            original_size: 1234,
            compressed_size: 567,
            crc32c: 0xdead_beef,
        };
        let mut meta: Option<Metadata> = None;
        write_manifest(&mut meta, &original);
        let extracted = extract_manifest(&meta).expect("manifest must round-trip");
        assert_eq!(extracted.codec, original.codec);
        assert_eq!(extracted.original_size, original.original_size);
        assert_eq!(extracted.compressed_size, original.compressed_size);
        assert_eq!(extracted.crc32c, original.crc32c);
    }

    #[test]
    fn missing_metadata_yields_none() {
        let meta: Option<Metadata> = None;
        assert!(extract_manifest(&meta).is_none());
    }

    #[test]
    fn partial_metadata_yields_none() {
        let mut meta = Metadata::new();
        meta.insert(META_CODEC.into(), "cpu-zstd".into());
        // 残りのキーが欠落している
        let opt = Some(meta);
        assert!(extract_manifest(&opt).is_none());
    }

    // Passthrough codec が S4Service に組み込めることを type 上で確認 (compile-time check)。
    #[allow(dead_code)]
    fn _assert_compiles_with_passthrough(
        p: s3s_aws::Proxy,
    ) -> S4Service<s3s_aws::Proxy, Passthrough> {
        S4Service::new(p, Arc::new(Passthrough))
    }
}
