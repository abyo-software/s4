//! `s3s::S3` 実装 — `s3s_aws::Proxy` への delegation を default にしつつ、
//! `put_object` / `get_object` 経路で `s4_codec::CodecRegistry` を呼ぶ。
//!
//! ## カバー範囲 (Phase 1 月 2)
//!
//! - 圧縮 hook あり: `put_object`, `get_object`
//! - 純 delegation (圧縮なし): `head_bucket`, `list_buckets`, `create_bucket`, `delete_bucket`,
//!   `head_object`, `delete_object`, `delete_objects`, `copy_object`, `list_objects`,
//!   `list_objects_v2`, `create_multipart_upload`, `upload_part`,
//!   `complete_multipart_upload`, `abort_multipart_upload`, `list_multipart_uploads`,
//!   `list_parts`
//! - 未対応 (デフォルトで NotImplemented): その他 80+ ops (Tagging / ACL / Lifecycle 等は Phase 2)
//!
//! ## アーキテクチャ
//!
//! - `S4Service<B>` は backend (B: S3) と `Arc<CodecRegistry>` と `Arc<dyn CodecDispatcher>`
//!   を保持する。`CodecRegistry` 経由で複数 codec を抱えられるので、ひとつの S4 インスタンスが
//!   複数 codec で書かれた object を透過的に GET できる
//! - PUT: dispatcher が body の先頭 sample から codec を選び、registry で compress、
//!   manifest を S3 metadata に書いて backend に forward
//! - GET: backend から取得 → metadata から manifest を復元 → registry.decompress で
//!   manifest 指定の codec で解凍 → 元の bytes を return
//!
//! ## 既知の制限事項
//!
//! - **Multipart Upload は per-part 圧縮が未実装**: 現状は upload_part を素通し。
//!   Phase 1 月 2 後半で per-part compress + complete_multipart_upload で manifest 集約。
//! - **PUT body は memory に collect**: max_body_bytes 上限あり (default 5 GiB = S3 単発 PUT 上限)。
//!   Streaming-aware 圧縮は Phase 2。

use std::sync::Arc;

use s3s::dto::*;
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use s4_codec::{ChunkManifest, CodecDispatcher, CodecKind, CodecRegistry};
use tracing::debug;

use crate::blob::{bytes_to_blob, collect_blob};

/// PUT body の先頭 sampling で渡す最大 byte 数。
const SAMPLE_BYTES: usize = 4096;

pub struct S4Service<B: S3> {
    backend: B,
    registry: Arc<CodecRegistry>,
    dispatcher: Arc<dyn CodecDispatcher>,
    max_body_bytes: usize,
}

impl<B: S3> S4Service<B> {
    /// AWS S3 単発 PUT の API 上限 (5 GiB)
    pub const DEFAULT_MAX_BODY_BYTES: usize = 5 * 1024 * 1024 * 1024;

    pub fn new(
        backend: B,
        registry: Arc<CodecRegistry>,
        dispatcher: Arc<dyn CodecDispatcher>,
    ) -> Self {
        Self {
            backend,
            registry,
            dispatcher,
            max_body_bytes: Self::DEFAULT_MAX_BODY_BYTES,
        }
    }

    #[must_use]
    pub fn with_max_body_bytes(mut self, n: usize) -> Self {
        self.max_body_bytes = n;
        self
    }

    /// テスト用: backend を取り戻す (test helper、production では使わない)
    pub fn into_backend(self) -> B {
        self.backend
    }
}

const META_CODEC: &str = "s4-codec";
const META_ORIGINAL_SIZE: &str = "s4-original-size";
const META_COMPRESSED_SIZE: &str = "s4-compressed-size";
const META_CRC32C: &str = "s4-crc32c";

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
impl<B: S3> S3 for S4Service<B> {
    // === 圧縮を挟む path (PUT) ===
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        if let Some(blob) = req.input.body.take() {
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect put body"))?;
            let sample_len = bytes.len().min(SAMPLE_BYTES);
            let kind = self.dispatcher.pick(&bytes[..sample_len]).await;
            debug!(
                bucket = ?req.input.bucket,
                key = ?req.input.key,
                bytes = bytes.len(),
                codec = kind.as_str(),
                "S4 put_object: compressing"
            );
            let (compressed, manifest) = self
                .registry
                .compress(bytes, kind)
                .await
                .map_err(internal("registry compress"))?;
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
            // S4 が書いていないオブジェクトは透過 (raw bucket pre-existing object 等)
            debug!("S4 get_object: object lacks s4-codec metadata, returning as-is");
            return Ok(resp);
        };
        if let Some(blob) = resp.output.body.take() {
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect get body"))?;
            let decompressed = self
                .registry
                .decompress(bytes, &manifest)
                .await
                .map_err(internal("registry decompress"))?;
            resp.output.body = Some(bytes_to_blob(decompressed));
        }
        Ok(resp)
    }

    // === passthrough delegations ===
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
        let opt = Some(meta);
        assert!(extract_manifest(&opt).is_none());
    }
}
