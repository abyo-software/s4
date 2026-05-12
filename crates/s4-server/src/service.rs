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

use bytes::BytesMut;
use s3s::dto::*;
use s3s::{S3, S3Error, S3ErrorCode, S3Request, S3Response, S3Result};
use s4_codec::multipart::{
    FRAME_HEADER_BYTES, FrameHeader, FrameIter, S3_MULTIPART_MIN_PART_BYTES, pad_to_minimum,
    write_frame,
};
use s4_codec::{ChunkManifest, CodecDispatcher, CodecKind, CodecRegistry};
use tracing::debug;

use crate::blob::{bytes_to_blob, collect_blob};
use crate::streaming::{cpu_zstd_decompress_stream, supports_streaming_decompress};

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

    /// Multipart object (frame 列) を解凍 → 元 bytes を再構築。
    ///
    /// 各 frame の codec は object metadata の `s4-codec` で全 part 共通という前提。
    /// (Phase 2 で per-frame codec ID を入れる可能性あり、その時はここを拡張)
    async fn decompress_multipart(
        &self,
        bytes: bytes::Bytes,
        metadata: &Option<Metadata>,
    ) -> S3Result<bytes::Bytes> {
        let codec_kind = metadata
            .as_ref()
            .and_then(|m| m.get(META_CODEC))
            .and_then(|s| s.parse::<CodecKind>().ok())
            .ok_or_else(|| {
                S3Error::with_message(
                    S3ErrorCode::InternalError,
                    "multipart object missing s4-codec metadata",
                )
            })?;

        let mut out = BytesMut::new();
        for frame in FrameIter::new(bytes) {
            let (header, payload) = frame.map_err(|e| {
                S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!("multipart frame parse: {e}"),
                )
            })?;
            let chunk_manifest = ChunkManifest {
                codec: codec_kind,
                original_size: header.original_size,
                compressed_size: header.compressed_size,
                crc32c: header.crc32c,
            };
            let decompressed = self
                .registry
                .decompress(payload, &chunk_manifest)
                .await
                .map_err(internal("multipart frame decompress"))?;
            out.extend_from_slice(&decompressed);
        }
        Ok(out.freeze())
    }
}

fn is_multipart_object(metadata: &Option<Metadata>) -> bool {
    metadata
        .as_ref()
        .and_then(|m| m.get(META_MULTIPART))
        .map(|v| v == "true")
        .unwrap_or(false)
}

const META_CODEC: &str = "s4-codec";
const META_ORIGINAL_SIZE: &str = "s4-original-size";
const META_COMPRESSED_SIZE: &str = "s4-compressed-size";
const META_CRC32C: &str = "s4-crc32c";
/// Multipart upload で per-part frame format を使ったオブジェクトであることを示す。
/// GET 時にこの flag を見て frame parser を起動する。
const META_MULTIPART: &str = "s4-multipart";

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
            // 重要: content_length を圧縮後サイズで更新する。
            // これを忘れると下流 (aws-sdk-s3 → S3) が宣言サイズ分の bytes を
            // 待ち続けて RequestTimeout で失敗する (S3 仕様)。
            req.input.content_length = Some(compressed.len() as i64);
            // body を書き換えたので、客側が送ってきた original body 用の
            // checksum / MD5 ヘッダは無効化する (そのまま転送すると下流 S3 が
            // XAmzContentChecksumMismatch を返す)。S4 自身の整合性は
            // ChunkManifest.crc32c で担保している。
            req.input.checksum_algorithm = None;
            req.input.checksum_crc32 = None;
            req.input.checksum_crc32c = None;
            req.input.checksum_crc64nvme = None;
            req.input.checksum_sha1 = None;
            req.input.checksum_sha256 = None;
            req.input.content_md5 = None;
            req.input.body = Some(bytes_to_blob(compressed));
        }
        self.backend.put_object(req).await
    }

    // === 圧縮を解く path (GET) ===
    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        // Range GET は currently 非対応 (multipart frame parser が full-object 前提で
        // 動くため)。Range が指定されていたら head_object で codec metadata を見て、
        // S4-managed object なら 416 で reject (silent corruption より明確な拒否)。
        // 通常 (S4 が触っていない) object なら range を素通しさせる。
        if req.input.range.is_some() {
            let head_input = HeadObjectInput {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                ..Default::default()
            };
            let head_req = S3Request {
                input: head_input,
                method: req.method.clone(),
                uri: req.uri.clone(),
                headers: req.headers.clone(),
                extensions: http::Extensions::new(),
                credentials: req.credentials.clone(),
                region: req.region.clone(),
                service: req.service.clone(),
                trailing_headers: None,
            };
            if let Ok(head) = self.backend.head_object(head_req).await
                && (is_multipart_object(&head.output.metadata)
                    || extract_manifest(&head.output.metadata).is_some())
            {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidRange,
                    "Range GET on S4-compressed objects is not supported in Phase 1; \
                     issue full-object GET (omit Range header)",
                ));
            }
        }
        let mut resp = self.backend.get_object(req).await?;
        let is_multipart = is_multipart_object(&resp.output.metadata);
        let manifest_opt = extract_manifest(&resp.output.metadata);

        if !is_multipart && manifest_opt.is_none() {
            // S4 が書いていないオブジェクトは透過 (raw bucket pre-existing object 等)
            debug!("S4 get_object: object lacks s4-codec metadata, returning as-is");
            return Ok(resp);
        }

        if let Some(blob) = resp.output.body.take() {
            // ====== Streaming fast path (CpuZstd, non-multipart, codec supports it) ======
            // 大規模 object (e.g. 5 GB) を memory に collect すると OOM するので、
            // codec が streaming-aware なら body を chunk-by-chunk で decompress して
            // 即座に client に流す。
            if !is_multipart
                && let Some(ref m) = manifest_opt
                && supports_streaming_decompress(m.codec)
                && m.codec == CodecKind::CpuZstd
            {
                let decompressed_blob = cpu_zstd_decompress_stream(blob);
                resp.output.content_length = Some(m.original_size as i64);
                resp.output.checksum_crc32 = None;
                resp.output.checksum_crc32c = None;
                resp.output.checksum_crc64nvme = None;
                resp.output.checksum_sha1 = None;
                resp.output.checksum_sha256 = None;
                resp.output.e_tag = None;
                resp.output.body = Some(decompressed_blob);
                debug!(
                    codec = "cpu-zstd",
                    original_size = m.original_size,
                    "S4 get_object: streaming fast path"
                );
                return Ok(resp);
            }
            // Passthrough: そのまま流す
            if !is_multipart
                && let Some(ref m) = manifest_opt
                && m.codec == CodecKind::Passthrough
            {
                resp.output.content_length = Some(m.original_size as i64);
                resp.output.checksum_crc32 = None;
                resp.output.checksum_crc32c = None;
                resp.output.checksum_crc64nvme = None;
                resp.output.checksum_sha1 = None;
                resp.output.checksum_sha256 = None;
                resp.output.e_tag = None;
                resp.output.body = Some(blob);
                debug!("S4 get_object: passthrough streaming");
                return Ok(resp);
            }

            // ====== Buffered slow path (multipart frame parser, GPU codecs) ======
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect get body"))?;

            let decompressed = if is_multipart {
                self.decompress_multipart(bytes, &resp.output.metadata)
                    .await?
            } else {
                let manifest = manifest_opt.expect("non-multipart guarded above");
                self.registry
                    .decompress(bytes, &manifest)
                    .await
                    .map_err(internal("registry decompress"))?
            };

            // 解凍後の真のサイズを返す (S3 client は content_length を信頼するので
            // 圧縮 size のままだと downstream が body を途中で切ってしまう)
            resp.output.content_length = Some(decompressed.len() as i64);
            // 圧縮済 bytes の checksum を返すと AWS SDK 側で StreamingError
            // (ChecksumMismatch) になる。ETag も backend が返した「圧縮済 bytes の
            // MD5/checksum」なので意味的にズレる — クリアして S4 自身の crc32c
            // (manifest 内 / frame 内) で integrity を保証する設計にする。
            resp.output.checksum_crc32 = None;
            resp.output.checksum_crc32c = None;
            resp.output.checksum_crc64nvme = None;
            resp.output.checksum_sha1 = None;
            resp.output.checksum_sha256 = None;
            resp.output.e_tag = None;
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
        let mut resp = self.backend.head_object(req).await?;
        if let Some(manifest) = extract_manifest(&resp.output.metadata) {
            // 客側には decompress 後の意味のある content_length / checksum を返す。
            // backend が返す圧縮済 bytes の checksum / e_tag は意味が違うため除去
            // (S4 は manifest 内の crc32c で integrity を担保する)。
            resp.output.content_length = Some(manifest.original_size as i64);
            resp.output.checksum_crc32 = None;
            resp.output.checksum_crc32c = None;
            resp.output.checksum_crc64nvme = None;
            resp.output.checksum_sha1 = None;
            resp.output.checksum_sha256 = None;
            resp.output.e_tag = None;
        }
        Ok(resp)
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
        mut req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        // Multipart object は per-part 圧縮 + frame 形式で書く。GET 時に
        // frame parse を起動するため、object metadata に flag を立てる。
        // codec は dispatcher の default kind を採用 (per-part 別 codec は Phase 2)。
        let codec_kind = self.registry.default_kind();
        let meta = req.input.metadata.get_or_insert_with(Default::default);
        meta.insert(META_MULTIPART.into(), "true".into());
        meta.insert(META_CODEC.into(), codec_kind.as_str().into());
        debug!(
            bucket = ?req.input.bucket,
            key = ?req.input.key,
            codec = codec_kind.as_str(),
            "S4 create_multipart_upload: marking object for per-part compression"
        );
        self.backend.create_multipart_upload(req).await
    }

    async fn upload_part(
        &self,
        mut req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        // 各 part を圧縮して frame header 付きで forward。GET 時に
        // `decompress_multipart` が frame iter で順に解凍する。
        if let Some(blob) = req.input.body.take() {
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect upload_part body"))?;
            // codec は registry の default を使う (multipart は uniform codec)。
            // create_multipart_upload で metadata に書いた codec と整合する
            // 必要があるが、現状は同じ S4Service インスタンス前提なので一致する
            let codec_kind = self.registry.default_kind();
            let original_size = bytes.len() as u64;
            let (compressed, manifest) = self
                .registry
                .compress(bytes, codec_kind)
                .await
                .map_err(internal("registry compress part"))?;
            let header = FrameHeader {
                original_size,
                compressed_size: compressed.len() as u64,
                crc32c: manifest.crc32c,
            };
            let mut framed = BytesMut::with_capacity(FRAME_HEADER_BYTES + compressed.len());
            write_frame(&mut framed, header, &compressed);
            // S3 multipart の non-final part 最小サイズ (5 MiB) を満たすため
            // padding frame を追加。FrameIter が S4P1 padding を skip する。
            // 注: 最終 part も常に pad してしまっているが、最終 part だけ pad しない
            // 最適化は S4Service が「最終 part か」を upload_part 時点で知れない
            // ため Phase 2 (CompleteMultipartUpload で trim) で対応。
            pad_to_minimum(&mut framed, S3_MULTIPART_MIN_PART_BYTES);
            let framed_bytes = framed.freeze();
            let new_len = framed_bytes.len() as i64;
            // 同じ wire 互換問題が multipart にもある (content-length / checksum)
            req.input.content_length = Some(new_len);
            req.input.checksum_algorithm = None;
            req.input.checksum_crc32 = None;
            req.input.checksum_crc32c = None;
            req.input.checksum_crc64nvme = None;
            req.input.checksum_sha1 = None;
            req.input.checksum_sha256 = None;
            req.input.content_md5 = None;
            req.input.body = Some(bytes_to_blob(framed_bytes));
            debug!(
                part_number = ?req.input.part_number,
                upload_id = ?req.input.upload_id,
                original_size,
                framed_size = new_len,
                "S4 upload_part: framed compressed payload"
            );
        }
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

    // =========================================================================
    // Phase 2 — pure passthrough delegations。S4 はこれらに対して圧縮 hook を
    // 持たないので、backend (= AWS S3) の動作と完全に同一。
    //
    // 既知の制限事項:
    // - copy_object / upload_part_copy: source object が S4-compressed の場合、
    //   backend が bytes を copy するだけなので metadata (s4-codec etc) も一緒に
    //   coppied される (AWS S3 default = MetadataDirective COPY)。GET は manifest
    //   経由で正しく decompress できる。MetadataDirective REPLACE で上書き
    //   されると圧縮 metadata が消えて壊れる — 顧客側の運用で注意
    // - list_object_versions: versioning enabled bucket では各 version も S4
    //   metadata を維持する。古い version も S4 経由で正しく GET できる。
    // =========================================================================

    // ---- Object ACL / tagging / attributes ----
    async fn get_object_acl(
        &self,
        req: S3Request<GetObjectAclInput>,
    ) -> S3Result<S3Response<GetObjectAclOutput>> {
        self.backend.get_object_acl(req).await
    }
    async fn put_object_acl(
        &self,
        req: S3Request<PutObjectAclInput>,
    ) -> S3Result<S3Response<PutObjectAclOutput>> {
        self.backend.put_object_acl(req).await
    }
    async fn get_object_tagging(
        &self,
        req: S3Request<GetObjectTaggingInput>,
    ) -> S3Result<S3Response<GetObjectTaggingOutput>> {
        self.backend.get_object_tagging(req).await
    }
    async fn put_object_tagging(
        &self,
        req: S3Request<PutObjectTaggingInput>,
    ) -> S3Result<S3Response<PutObjectTaggingOutput>> {
        self.backend.put_object_tagging(req).await
    }
    async fn delete_object_tagging(
        &self,
        req: S3Request<DeleteObjectTaggingInput>,
    ) -> S3Result<S3Response<DeleteObjectTaggingOutput>> {
        self.backend.delete_object_tagging(req).await
    }
    async fn get_object_attributes(
        &self,
        req: S3Request<GetObjectAttributesInput>,
    ) -> S3Result<S3Response<GetObjectAttributesOutput>> {
        self.backend.get_object_attributes(req).await
    }
    async fn restore_object(
        &self,
        req: S3Request<RestoreObjectInput>,
    ) -> S3Result<S3Response<RestoreObjectOutput>> {
        self.backend.restore_object(req).await
    }
    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        // 注: source が S4-compressed の場合、bytes の partial copy は壊れる。
        //     S3 spec の仕様上 byte-range で copy できるが、S4 の compress block
        //     boundary とは無関係。Phase 2 で per-part 圧縮を入れた後に再考。
        self.backend.upload_part_copy(req).await
    }

    // ---- Object lock / retention / legal hold ----
    async fn get_object_lock_configuration(
        &self,
        req: S3Request<GetObjectLockConfigurationInput>,
    ) -> S3Result<S3Response<GetObjectLockConfigurationOutput>> {
        self.backend.get_object_lock_configuration(req).await
    }
    async fn put_object_lock_configuration(
        &self,
        req: S3Request<PutObjectLockConfigurationInput>,
    ) -> S3Result<S3Response<PutObjectLockConfigurationOutput>> {
        self.backend.put_object_lock_configuration(req).await
    }
    async fn get_object_legal_hold(
        &self,
        req: S3Request<GetObjectLegalHoldInput>,
    ) -> S3Result<S3Response<GetObjectLegalHoldOutput>> {
        self.backend.get_object_legal_hold(req).await
    }
    async fn put_object_legal_hold(
        &self,
        req: S3Request<PutObjectLegalHoldInput>,
    ) -> S3Result<S3Response<PutObjectLegalHoldOutput>> {
        self.backend.put_object_legal_hold(req).await
    }
    async fn get_object_retention(
        &self,
        req: S3Request<GetObjectRetentionInput>,
    ) -> S3Result<S3Response<GetObjectRetentionOutput>> {
        self.backend.get_object_retention(req).await
    }
    async fn put_object_retention(
        &self,
        req: S3Request<PutObjectRetentionInput>,
    ) -> S3Result<S3Response<PutObjectRetentionOutput>> {
        self.backend.put_object_retention(req).await
    }

    // ---- Versioning ----
    async fn list_object_versions(
        &self,
        req: S3Request<ListObjectVersionsInput>,
    ) -> S3Result<S3Response<ListObjectVersionsOutput>> {
        self.backend.list_object_versions(req).await
    }
    async fn get_bucket_versioning(
        &self,
        req: S3Request<GetBucketVersioningInput>,
    ) -> S3Result<S3Response<GetBucketVersioningOutput>> {
        self.backend.get_bucket_versioning(req).await
    }
    async fn put_bucket_versioning(
        &self,
        req: S3Request<PutBucketVersioningInput>,
    ) -> S3Result<S3Response<PutBucketVersioningOutput>> {
        self.backend.put_bucket_versioning(req).await
    }

    // ---- Bucket location ----
    async fn get_bucket_location(
        &self,
        req: S3Request<GetBucketLocationInput>,
    ) -> S3Result<S3Response<GetBucketLocationOutput>> {
        self.backend.get_bucket_location(req).await
    }

    // ---- Bucket policy ----
    async fn get_bucket_policy(
        &self,
        req: S3Request<GetBucketPolicyInput>,
    ) -> S3Result<S3Response<GetBucketPolicyOutput>> {
        self.backend.get_bucket_policy(req).await
    }
    async fn put_bucket_policy(
        &self,
        req: S3Request<PutBucketPolicyInput>,
    ) -> S3Result<S3Response<PutBucketPolicyOutput>> {
        self.backend.put_bucket_policy(req).await
    }
    async fn delete_bucket_policy(
        &self,
        req: S3Request<DeleteBucketPolicyInput>,
    ) -> S3Result<S3Response<DeleteBucketPolicyOutput>> {
        self.backend.delete_bucket_policy(req).await
    }
    async fn get_bucket_policy_status(
        &self,
        req: S3Request<GetBucketPolicyStatusInput>,
    ) -> S3Result<S3Response<GetBucketPolicyStatusOutput>> {
        self.backend.get_bucket_policy_status(req).await
    }

    // ---- Bucket ACL ----
    async fn get_bucket_acl(
        &self,
        req: S3Request<GetBucketAclInput>,
    ) -> S3Result<S3Response<GetBucketAclOutput>> {
        self.backend.get_bucket_acl(req).await
    }
    async fn put_bucket_acl(
        &self,
        req: S3Request<PutBucketAclInput>,
    ) -> S3Result<S3Response<PutBucketAclOutput>> {
        self.backend.put_bucket_acl(req).await
    }

    // ---- Bucket CORS ----
    async fn get_bucket_cors(
        &self,
        req: S3Request<GetBucketCorsInput>,
    ) -> S3Result<S3Response<GetBucketCorsOutput>> {
        self.backend.get_bucket_cors(req).await
    }
    async fn put_bucket_cors(
        &self,
        req: S3Request<PutBucketCorsInput>,
    ) -> S3Result<S3Response<PutBucketCorsOutput>> {
        self.backend.put_bucket_cors(req).await
    }
    async fn delete_bucket_cors(
        &self,
        req: S3Request<DeleteBucketCorsInput>,
    ) -> S3Result<S3Response<DeleteBucketCorsOutput>> {
        self.backend.delete_bucket_cors(req).await
    }

    // ---- Bucket lifecycle ----
    async fn get_bucket_lifecycle_configuration(
        &self,
        req: S3Request<GetBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketLifecycleConfigurationOutput>> {
        self.backend.get_bucket_lifecycle_configuration(req).await
    }
    async fn put_bucket_lifecycle_configuration(
        &self,
        req: S3Request<PutBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketLifecycleConfigurationOutput>> {
        self.backend.put_bucket_lifecycle_configuration(req).await
    }
    async fn delete_bucket_lifecycle(
        &self,
        req: S3Request<DeleteBucketLifecycleInput>,
    ) -> S3Result<S3Response<DeleteBucketLifecycleOutput>> {
        self.backend.delete_bucket_lifecycle(req).await
    }

    // ---- Bucket tagging ----
    async fn get_bucket_tagging(
        &self,
        req: S3Request<GetBucketTaggingInput>,
    ) -> S3Result<S3Response<GetBucketTaggingOutput>> {
        self.backend.get_bucket_tagging(req).await
    }
    async fn put_bucket_tagging(
        &self,
        req: S3Request<PutBucketTaggingInput>,
    ) -> S3Result<S3Response<PutBucketTaggingOutput>> {
        self.backend.put_bucket_tagging(req).await
    }
    async fn delete_bucket_tagging(
        &self,
        req: S3Request<DeleteBucketTaggingInput>,
    ) -> S3Result<S3Response<DeleteBucketTaggingOutput>> {
        self.backend.delete_bucket_tagging(req).await
    }

    // ---- Bucket encryption ----
    async fn get_bucket_encryption(
        &self,
        req: S3Request<GetBucketEncryptionInput>,
    ) -> S3Result<S3Response<GetBucketEncryptionOutput>> {
        self.backend.get_bucket_encryption(req).await
    }
    async fn put_bucket_encryption(
        &self,
        req: S3Request<PutBucketEncryptionInput>,
    ) -> S3Result<S3Response<PutBucketEncryptionOutput>> {
        self.backend.put_bucket_encryption(req).await
    }
    async fn delete_bucket_encryption(
        &self,
        req: S3Request<DeleteBucketEncryptionInput>,
    ) -> S3Result<S3Response<DeleteBucketEncryptionOutput>> {
        self.backend.delete_bucket_encryption(req).await
    }

    // ---- Bucket logging ----
    async fn get_bucket_logging(
        &self,
        req: S3Request<GetBucketLoggingInput>,
    ) -> S3Result<S3Response<GetBucketLoggingOutput>> {
        self.backend.get_bucket_logging(req).await
    }
    async fn put_bucket_logging(
        &self,
        req: S3Request<PutBucketLoggingInput>,
    ) -> S3Result<S3Response<PutBucketLoggingOutput>> {
        self.backend.put_bucket_logging(req).await
    }

    // ---- Bucket notification ----
    async fn get_bucket_notification_configuration(
        &self,
        req: S3Request<GetBucketNotificationConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketNotificationConfigurationOutput>> {
        self.backend
            .get_bucket_notification_configuration(req)
            .await
    }
    async fn put_bucket_notification_configuration(
        &self,
        req: S3Request<PutBucketNotificationConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketNotificationConfigurationOutput>> {
        self.backend
            .put_bucket_notification_configuration(req)
            .await
    }

    // ---- Bucket request payment ----
    async fn get_bucket_request_payment(
        &self,
        req: S3Request<GetBucketRequestPaymentInput>,
    ) -> S3Result<S3Response<GetBucketRequestPaymentOutput>> {
        self.backend.get_bucket_request_payment(req).await
    }
    async fn put_bucket_request_payment(
        &self,
        req: S3Request<PutBucketRequestPaymentInput>,
    ) -> S3Result<S3Response<PutBucketRequestPaymentOutput>> {
        self.backend.put_bucket_request_payment(req).await
    }

    // ---- Bucket website ----
    async fn get_bucket_website(
        &self,
        req: S3Request<GetBucketWebsiteInput>,
    ) -> S3Result<S3Response<GetBucketWebsiteOutput>> {
        self.backend.get_bucket_website(req).await
    }
    async fn put_bucket_website(
        &self,
        req: S3Request<PutBucketWebsiteInput>,
    ) -> S3Result<S3Response<PutBucketWebsiteOutput>> {
        self.backend.put_bucket_website(req).await
    }
    async fn delete_bucket_website(
        &self,
        req: S3Request<DeleteBucketWebsiteInput>,
    ) -> S3Result<S3Response<DeleteBucketWebsiteOutput>> {
        self.backend.delete_bucket_website(req).await
    }

    // ---- Bucket replication ----
    async fn get_bucket_replication(
        &self,
        req: S3Request<GetBucketReplicationInput>,
    ) -> S3Result<S3Response<GetBucketReplicationOutput>> {
        self.backend.get_bucket_replication(req).await
    }
    async fn put_bucket_replication(
        &self,
        req: S3Request<PutBucketReplicationInput>,
    ) -> S3Result<S3Response<PutBucketReplicationOutput>> {
        self.backend.put_bucket_replication(req).await
    }
    async fn delete_bucket_replication(
        &self,
        req: S3Request<DeleteBucketReplicationInput>,
    ) -> S3Result<S3Response<DeleteBucketReplicationOutput>> {
        self.backend.delete_bucket_replication(req).await
    }

    // ---- Bucket accelerate ----
    async fn get_bucket_accelerate_configuration(
        &self,
        req: S3Request<GetBucketAccelerateConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketAccelerateConfigurationOutput>> {
        self.backend.get_bucket_accelerate_configuration(req).await
    }
    async fn put_bucket_accelerate_configuration(
        &self,
        req: S3Request<PutBucketAccelerateConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketAccelerateConfigurationOutput>> {
        self.backend.put_bucket_accelerate_configuration(req).await
    }

    // ---- Bucket ownership controls ----
    async fn get_bucket_ownership_controls(
        &self,
        req: S3Request<GetBucketOwnershipControlsInput>,
    ) -> S3Result<S3Response<GetBucketOwnershipControlsOutput>> {
        self.backend.get_bucket_ownership_controls(req).await
    }
    async fn put_bucket_ownership_controls(
        &self,
        req: S3Request<PutBucketOwnershipControlsInput>,
    ) -> S3Result<S3Response<PutBucketOwnershipControlsOutput>> {
        self.backend.put_bucket_ownership_controls(req).await
    }
    async fn delete_bucket_ownership_controls(
        &self,
        req: S3Request<DeleteBucketOwnershipControlsInput>,
    ) -> S3Result<S3Response<DeleteBucketOwnershipControlsOutput>> {
        self.backend.delete_bucket_ownership_controls(req).await
    }

    // ---- Public access block ----
    async fn get_public_access_block(
        &self,
        req: S3Request<GetPublicAccessBlockInput>,
    ) -> S3Result<S3Response<GetPublicAccessBlockOutput>> {
        self.backend.get_public_access_block(req).await
    }
    async fn put_public_access_block(
        &self,
        req: S3Request<PutPublicAccessBlockInput>,
    ) -> S3Result<S3Response<PutPublicAccessBlockOutput>> {
        self.backend.put_public_access_block(req).await
    }
    async fn delete_public_access_block(
        &self,
        req: S3Request<DeletePublicAccessBlockInput>,
    ) -> S3Result<S3Response<DeletePublicAccessBlockOutput>> {
        self.backend.delete_public_access_block(req).await
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
