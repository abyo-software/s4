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
use s4_codec::index::{FrameIndex, build_index_from_body, decode_index, encode_index, sidecar_key};
use s4_codec::multipart::{
    FRAME_HEADER_BYTES, FrameHeader, FrameIter, S3_MULTIPART_MIN_PART_BYTES, pad_to_minimum,
    write_frame,
};
use s4_codec::{ChunkManifest, CodecDispatcher, CodecKind, CodecRegistry};
use std::time::Instant;
use tracing::{debug, info};

use crate::blob::{
    bytes_to_blob, chain_sample_with_rest, collect_blob, collect_with_sample, peek_sample,
};
use crate::streaming::{
    DEFAULT_S4F2_CHUNK_SIZE, cpu_zstd_decompress_stream, streaming_compress_to_frames,
    supports_streaming_compress, supports_streaming_decompress,
};

/// PUT body の先頭 sampling で渡す最大 byte 数。
const SAMPLE_BYTES: usize = 4096;

pub struct S4Service<B: S3> {
    backend: B,
    registry: Arc<CodecRegistry>,
    dispatcher: Arc<dyn CodecDispatcher>,
    max_body_bytes: usize,
    policy: Option<crate::policy::SharedPolicy>,
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
            policy: None,
        }
    }

    #[must_use]
    pub fn with_max_body_bytes(mut self, n: usize) -> Self {
        self.max_body_bytes = n;
        self
    }

    /// Attach an optional bucket policy (v0.2 #7). When `Some(...)`, every
    /// PUT / GET / DELETE / List handler runs `policy.evaluate(...)` before
    /// delegating to the backend; failures return `S3ErrorCode::AccessDenied`.
    /// When `None` (the default), no policy enforcement happens.
    #[must_use]
    pub fn with_policy(mut self, policy: crate::policy::SharedPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Pull the SigV4 access key id off the request's credentials, if any.
    /// Used as the `principal_id` for policy evaluation.
    fn principal_of<I>(req: &S3Request<I>) -> Option<&str> {
        req.credentials.as_ref().map(|c| c.access_key.as_str())
    }

    /// Helper used by request handlers to enforce the optional policy.
    /// Returns `Ok(())` when allowed (or no policy is configured), or an
    /// `AccessDenied` S3Error otherwise. Bumps the policy denial Prometheus
    /// counter on deny.
    fn enforce_policy(
        &self,
        action: &'static str,
        bucket: &str,
        key: Option<&str>,
        principal_id: Option<&str>,
    ) -> S3Result<()> {
        let Some(policy) = self.policy.as_ref() else {
            return Ok(());
        };
        let decision = policy.evaluate(action, bucket, key, principal_id);
        if decision.allow {
            Ok(())
        } else {
            crate::metrics::record_policy_denial(action, bucket);
            tracing::info!(
                action,
                bucket,
                key = ?key,
                principal = ?principal_id,
                matched_sid = ?decision.matched_sid,
                effect = ?decision.matched_effect,
                "S4 policy denied request"
            );
            Err(S3Error::with_message(
                S3ErrorCode::AccessDenied,
                format!("denied by S4 policy: {action} on bucket={bucket}"),
            ))
        }
    }

    /// テスト用: backend を取り戻す (test helper、production では使わない)
    pub fn into_backend(self) -> B {
        self.backend
    }

    /// 必要 frame だけを backend に Range GET し、frame parse + decompress + slice
    /// した結果を返す sidecar fast path。Range request の **帯域節約版**。
    async fn partial_range_get(
        &self,
        req: &S3Request<GetObjectInput>,
        plan: s4_codec::index::RangePlan,
        client_start: u64,
        client_end_exclusive: u64,
        total_original: u64,
        get_start: Instant,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        // 必要 byte 範囲だけを backend に partial GET
        let backend_range = s3s::dto::Range::Int {
            first: plan.byte_start,
            last: Some(plan.byte_end_exclusive - 1),
        };
        let backend_input = GetObjectInput {
            bucket: req.input.bucket.clone(),
            key: req.input.key.clone(),
            range: Some(backend_range),
            ..Default::default()
        };
        let backend_req = S3Request {
            input: backend_input,
            method: req.method.clone(),
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let mut backend_resp = self.backend.get_object(backend_req).await?;
        let blob = backend_resp.output.body.take().ok_or_else(|| {
            S3Error::with_message(
                S3ErrorCode::InternalError,
                "backend partial GET returned empty body",
            )
        })?;
        let bytes = collect_blob(blob, self.max_body_bytes)
            .await
            .map_err(internal("collect partial body"))?;

        // frame parse + decompress
        let mut combined = BytesMut::new();
        for frame in FrameIter::new(bytes) {
            let (header, payload) = frame.map_err(|e| {
                S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!("partial-range frame parse: {e}"),
                )
            })?;
            let chunk_manifest = ChunkManifest {
                codec: header.codec,
                original_size: header.original_size,
                compressed_size: header.compressed_size,
                crc32c: header.crc32c,
            };
            let decompressed = self
                .registry
                .decompress(payload, &chunk_manifest)
                .await
                .map_err(internal("partial-range decompress"))?;
            combined.extend_from_slice(&decompressed);
        }
        let combined = combined.freeze();
        let sliced = combined
            .slice(plan.slice_start_in_combined as usize..plan.slice_end_in_combined as usize);

        // response 組立て
        let returned_size = sliced.len() as u64;
        backend_resp.output.content_length = Some(returned_size as i64);
        backend_resp.output.content_range = Some(format!(
            "bytes {client_start}-{}/{total_original}",
            client_end_exclusive - 1
        ));
        backend_resp.output.checksum_crc32 = None;
        backend_resp.output.checksum_crc32c = None;
        backend_resp.output.checksum_crc64nvme = None;
        backend_resp.output.checksum_sha1 = None;
        backend_resp.output.checksum_sha256 = None;
        backend_resp.output.e_tag = None;
        backend_resp.output.body = Some(bytes_to_blob(sliced));
        backend_resp.status = Some(http::StatusCode::PARTIAL_CONTENT);

        let elapsed = get_start.elapsed();
        crate::metrics::record_get(
            "partial",
            plan.byte_end_exclusive - plan.byte_start,
            returned_size,
            elapsed.as_secs_f64(),
            true,
        );
        info!(
            op = "get_object",
            bucket = %req.input.bucket,
            key = %req.input.key,
            bytes_in = plan.byte_end_exclusive - plan.byte_start,
            bytes_out = returned_size,
            total_object_size = total_original,
            range = true,
            path = "sidecar-partial",
            latency_ms = elapsed.as_millis() as u64,
            "S4 partial Range GET via sidecar index"
        );
        Ok(backend_resp)
    }

    /// `<key>.s4index` sidecar object を backend に書く。失敗しても本体 PUT は
    /// 成功扱いにしたいので、err は warn ログのみ (Range GET の partial path が
    /// 使えなくなるが、full read fallback で意味的には正しい結果を返す)。
    async fn write_sidecar(&self, bucket: &str, key: &str, index: &FrameIndex) {
        let bytes = encode_index(index);
        let len = bytes.len() as i64;
        let put_input = PutObjectInput {
            bucket: bucket.into(),
            key: sidecar_key(key),
            body: Some(bytes_to_blob(bytes)),
            content_length: Some(len),
            content_type: Some("application/x-s4-index".into()),
            ..Default::default()
        };
        let put_req = S3Request {
            input: put_input,
            method: http::Method::PUT,
            uri: format!("/{bucket}/{}", sidecar_key(key)).parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        if let Err(e) = self.backend.put_object(put_req).await {
            tracing::warn!(
                bucket,
                key,
                "S4 write_sidecar failed (Range GET will fall back to full read): {e}"
            );
        }
    }

    /// `<key>.s4index` sidecar を backend から読み出す。なければ None。
    async fn read_sidecar(&self, bucket: &str, key: &str) -> Option<FrameIndex> {
        let get_input = GetObjectInput {
            bucket: bucket.into(),
            key: sidecar_key(key),
            ..Default::default()
        };
        let get_req = S3Request {
            input: get_input,
            method: http::Method::GET,
            uri: format!("/{bucket}/{}", sidecar_key(key)).parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let resp = self.backend.get_object(get_req).await.ok()?;
        let blob = resp.output.body?;
        let bytes = collect_blob(blob, 64 * 1024 * 1024).await.ok()?;
        decode_index(bytes).ok()
    }

    /// Multipart object (frame 列) を解凍 → 元 bytes を再構築。
    ///
    /// **per-frame codec dispatch**: 各 frame header に codec_id が入っているので、
    /// frame ごとに registry が違う codec を呼ぶことができる。同一 object 内で
    /// 異なる codec が混在していても透過的に解凍可能 (parquet 風 mixed columns 等)。
    async fn decompress_multipart(&self, bytes: bytes::Bytes) -> S3Result<bytes::Bytes> {
        let mut out = BytesMut::new();
        for frame in FrameIter::new(bytes) {
            let (header, payload) = frame.map_err(|e| {
                S3Error::with_message(
                    S3ErrorCode::InternalError,
                    format!("multipart frame parse: {e}"),
                )
            })?;
            let chunk_manifest = ChunkManifest {
                codec: header.codec,
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

/// Parse a CopySourceRange header value (`bytes=N-M`, `bytes=N-`, `bytes=-N`)
/// into the s3s::dto::Range used by the GetObject path. The S3 spec only
/// allows `bytes=N-M` for upload_part_copy (no suffix or open-ended), so
/// reject the other variants for parity with AWS.
fn parse_copy_source_range(s: &str) -> Result<s3s::dto::Range, String> {
    let rest = s
        .strip_prefix("bytes=")
        .ok_or_else(|| format!("CopySourceRange must start with 'bytes=', got {s:?}"))?;
    let (a, b) = rest
        .split_once('-')
        .ok_or_else(|| format!("CopySourceRange must be 'bytes=N-M', got {s:?}"))?;
    let first: u64 = a
        .parse()
        .map_err(|_| format!("CopySourceRange first byte not a number: {a:?}"))?;
    let last: u64 = b
        .parse()
        .map_err(|_| format!("CopySourceRange last byte not a number: {b:?}"))?;
    if last < first {
        return Err(format!("CopySourceRange last < first: {s:?}"));
    }
    Ok(s3s::dto::Range::Int {
        first,
        last: Some(last),
    })
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
/// v0.2 #4: single-PUT でも S4F2 framed format で書かれていることを示す。
/// 旧 v0.1 single-PUT は raw 圧縮 bytes (この flag なし)。GET 時にこの flag を
/// 見て framed 経路 (= multipart と同じ FrameIter parse) に流す。
const META_FRAMED: &str = "s4-framed";

fn is_framed_v2_object(metadata: &Option<Metadata>) -> bool {
    metadata
        .as_ref()
        .and_then(|m| m.get(META_FRAMED))
        .map(|v| v == "true")
        .unwrap_or(false)
}

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

/// `Range` request を decompressed object サイズ `total` に適用して `(start, end_exclusive)`
/// を返す。`Range::Int { first, last }` は `bytes=first-last` (last は inclusive)、
/// `Range::Suffix { length }` は末尾 `length` byte。S3 仕様に準拠。
pub fn resolve_range(range: &s3s::dto::Range, total: u64) -> Result<(u64, u64), String> {
    if total == 0 {
        return Err("cannot range-get zero-length object".into());
    }
    match range {
        s3s::dto::Range::Int { first, last } => {
            let start = *first;
            let end_inclusive = match last {
                Some(l) => (*l).min(total - 1),
                None => total - 1,
            };
            if start > end_inclusive || start >= total {
                return Err(format!(
                    "range bytes={start}-{:?} out of object size {total}",
                    last
                ));
            }
            Ok((start, end_inclusive + 1))
        }
        s3s::dto::Range::Suffix { length } => {
            let len = (*length).min(total);
            Ok((total - len, total))
        }
    }
}

#[async_trait::async_trait]
impl<B: S3> S3 for S4Service<B> {
    // === 圧縮を挟む path (PUT) ===
    #[tracing::instrument(
        name = "s4.put_object",
        skip(self, req),
        fields(bucket = %req.input.bucket, key = %req.input.key, codec, bytes_in, bytes_out, latency_ms)
    )]
    async fn put_object(
        &self,
        mut req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let put_start = Instant::now();
        let put_bucket = req.input.bucket.clone();
        let put_key = req.input.key.clone();
        self.enforce_policy(
            "s3:PutObject",
            &put_bucket,
            Some(&put_key),
            Self::principal_of(&req),
        )?;
        if let Some(blob) = req.input.body.take() {
            // Sample 4 KiB から codec を決定。streaming-aware codec なら streaming
            // compress fast path、そうでなければ従来の collect-then-compress。
            let (sample, rest_stream) = peek_sample(blob, SAMPLE_BYTES)
                .await
                .map_err(internal("peek put sample"))?;
            let sample_len = sample.len().min(SAMPLE_BYTES);
            let kind = self.dispatcher.pick(&sample[..sample_len]).await;

            // Passthrough buys nothing from S4F2 wrapping (no compression =
            // no per-chunk frame to skip past) and the +28-byte header
            // overhead breaks size-sensitive callers that expect a true
            // pass-through. So passthrough always uses the legacy raw-blob
            // path; only compressing codecs go through the framed path.
            let use_framed = supports_streaming_compress(kind) && kind != CodecKind::Passthrough;
            let (compressed, manifest, is_framed) = if use_framed {
                // streaming fast path: input は memory に collect しない
                let chained = chain_sample_with_rest(sample, rest_stream);
                debug!(
                    bucket = ?req.input.bucket,
                    key = ?req.input.key,
                    codec = kind.as_str(),
                    path = "streaming-framed",
                    "S4 put_object: compressing (streaming, S4F2 multi-frame)"
                );
                let (body, manifest) = streaming_compress_to_frames(
                    chained,
                    Arc::clone(&self.registry),
                    kind,
                    DEFAULT_S4F2_CHUNK_SIZE,
                )
                .await
                .map_err(internal("streaming framed compress"))?;
                (body, manifest, true)
            } else {
                // GPU codec 等で streaming-aware でないものは bytes-buffered path
                // (raw 圧縮 bytes、framed なし — back-compat 互換 path)
                let bytes = collect_with_sample(sample, rest_stream, self.max_body_bytes)
                    .await
                    .map_err(internal("collect put body (buffered path)"))?;
                debug!(
                    bucket = ?req.input.bucket,
                    key = ?req.input.key,
                    bytes = bytes.len(),
                    codec = kind.as_str(),
                    path = "buffered",
                    "S4 put_object: compressing (buffered, raw blob)"
                );
                let (body, m) = self
                    .registry
                    .compress(bytes, kind)
                    .await
                    .map_err(internal("registry compress"))?;
                (body, m, false)
            };

            write_manifest(&mut req.input.metadata, &manifest);
            if is_framed {
                // v0.2 #4: framed body であることを GET 側に伝える meta flag。
                req.input
                    .metadata
                    .get_or_insert_with(Default::default)
                    .insert(META_FRAMED.into(), "true".into());
            }
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
            let original_size = manifest.original_size;
            let compressed_size = manifest.compressed_size;
            let codec_label = manifest.codec.as_str();
            // framed body は GET 側で sidecar partial-fetch を効かせるため
            // build_index_from_body で sidecar を組み立てて backend に PUT する。
            let sidecar_index = if is_framed {
                s4_codec::index::build_index_from_body(&compressed).ok()
            } else {
                None
            };
            req.input.body = Some(bytes_to_blob(compressed));
            let backend_resp = self.backend.put_object(req).await;
            if let Some(idx) = sidecar_index
                && backend_resp.is_ok()
                && idx.entries.len() > 1
            {
                // 1 chunk しかない (small object) なら sidecar は意味がない (=
                // partial fetch しても full body と同じ範囲) ので省略。
                self.write_sidecar(&put_bucket, &put_key, &idx).await;
            }
            let _ = (original_size, compressed_size); // mute unused warnings
            let elapsed = put_start.elapsed();
            crate::metrics::record_put(
                codec_label,
                original_size,
                compressed_size,
                elapsed.as_secs_f64(),
                backend_resp.is_ok(),
            );
            info!(
                op = "put_object",
                bucket = %put_bucket,
                key = %put_key,
                codec = codec_label,
                bytes_in = original_size,
                bytes_out = compressed_size,
                ratio = format!(
                    "{:.3}",
                    if original_size == 0 { 1.0 } else { compressed_size as f64 / original_size as f64 }
                ),
                latency_ms = elapsed.as_millis() as u64,
                ok = backend_resp.is_ok(),
                "S4 put completed"
            );
            return backend_resp;
        }
        self.backend.put_object(req).await
    }

    // === 圧縮を解く path (GET) ===
    #[tracing::instrument(
        name = "s4.get_object",
        skip(self, req),
        fields(bucket = %req.input.bucket, key = %req.input.key, codec, bytes_out, range, path)
    )]
    async fn get_object(
        &self,
        mut req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let get_start = Instant::now();
        let get_bucket = req.input.bucket.clone();
        let get_key = req.input.key.clone();
        self.enforce_policy(
            "s3:GetObject",
            &get_bucket,
            Some(&get_key),
            Self::principal_of(&req),
        )?;
        // Range request の事前検出 (decompress 後 slice する path に使う)。
        let range_request = req.input.range.take();

        // ====== Range GET の partial-fetch fast path (sidecar index 利用) ======
        // sidecar `<key>.s4index` が存在し、multipart-framed object であれば
        // 必要 frame だけを backend に Range GET し帯域節約する。
        if let Some(ref r) = range_request
            && let Some(index) = self.read_sidecar(&req.input.bucket, &req.input.key).await
        {
            let total = index.total_original_size();
            let (start, end_exclusive) = match resolve_range(r, total) {
                Ok(v) => v,
                Err(e) => {
                    return Err(S3Error::with_message(S3ErrorCode::InvalidRange, e));
                }
            };
            if let Some(plan) = index.lookup_range(start, end_exclusive) {
                return self
                    .partial_range_get(&req, plan, start, end_exclusive, total, get_start)
                    .await;
            }
        }
        let mut resp = self.backend.get_object(req).await?;
        let is_multipart = is_multipart_object(&resp.output.metadata);
        let is_framed_v2 = is_framed_v2_object(&resp.output.metadata);
        // v0.2 #4: framed-v2 single-PUT は多 frame parse が必要なので
        // multipart と同じ path に流す。
        let needs_frame_parse = is_multipart || is_framed_v2;
        let manifest_opt = extract_manifest(&resp.output.metadata);

        if !needs_frame_parse && manifest_opt.is_none() {
            // S4 が書いていないオブジェクトは透過 (raw bucket pre-existing object 等)
            debug!("S4 get_object: object lacks s4-codec metadata, returning as-is");
            return Ok(resp);
        }

        if let Some(blob) = resp.output.body.take() {
            // ====== Streaming fast path (CpuZstd, non-multipart, codec supports it) ======
            // 大規模 object (e.g. 5 GB) を memory に collect すると OOM するので、
            // codec が streaming-aware なら body を chunk-by-chunk で decompress して
            // 即座に client に流す。
            //
            // ただし Range request 時は streaming できない (slice するため total bytes
            // が必要) → buffered path に fall through。
            if range_request.is_none()
                && !needs_frame_parse
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
                let elapsed = get_start.elapsed();
                crate::metrics::record_get(
                    m.codec.as_str(),
                    m.compressed_size,
                    m.original_size,
                    elapsed.as_secs_f64(),
                    true,
                );
                info!(
                    op = "get_object",
                    bucket = %get_bucket,
                    key = %get_key,
                    codec = m.codec.as_str(),
                    bytes_in = m.compressed_size,
                    bytes_out = m.original_size,
                    path = "streaming",
                    setup_latency_ms = elapsed.as_millis() as u64,
                    "S4 get started (streaming)"
                );
                return Ok(resp);
            }
            // Passthrough: そのまま流す (Range なしの場合のみ streaming)
            if range_request.is_none()
                && !needs_frame_parse
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

            let decompressed = if needs_frame_parse {
                // multipart objects と framed-v2 single-PUT objects は同じ
                // S4F2 frame 列なので decompress_multipart で統一処理
                self.decompress_multipart(bytes).await?
            } else {
                let manifest = manifest_opt.as_ref().expect("non-multipart guarded above");
                self.registry
                    .decompress(bytes, manifest)
                    .await
                    .map_err(internal("registry decompress"))?
            };

            // Range request があれば slice。なければ full body を返す。
            let total_size = decompressed.len() as u64;
            let (final_bytes, status_override) = if let Some(r) = range_request.as_ref() {
                let (start, end) = resolve_range(r, total_size)
                    .map_err(|e| S3Error::with_message(S3ErrorCode::InvalidRange, e))?;
                let sliced = decompressed.slice(start as usize..end as usize);
                resp.output.content_range = Some(format!(
                    "bytes {start}-{}/{total_size}",
                    end.saturating_sub(1)
                ));
                (sliced, Some(http::StatusCode::PARTIAL_CONTENT))
            } else {
                (decompressed, None)
            };
            // 解凍後の真のサイズを返す (S3 client は content_length を信頼するので
            // 圧縮 size のままだと downstream が body を途中で切ってしまう)
            resp.output.content_length = Some(final_bytes.len() as i64);
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
            let returned_size = final_bytes.len() as u64;
            let codec_label = manifest_opt
                .as_ref()
                .map(|m| m.codec.as_str())
                .unwrap_or("multipart");
            resp.output.body = Some(bytes_to_blob(final_bytes));
            if let Some(status) = status_override {
                resp.status = Some(status);
            }
            let elapsed = get_start.elapsed();
            crate::metrics::record_get(codec_label, 0, returned_size, elapsed.as_secs_f64(), true);
            info!(
                op = "get_object",
                bucket = %get_bucket,
                key = %get_key,
                codec = codec_label,
                bytes_out = returned_size,
                total_object_size = total_size,
                range = range_request.is_some(),
                path = "buffered",
                latency_ms = elapsed.as_millis() as u64,
                "S4 get completed (buffered)"
            );
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
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        self.enforce_policy(
            "s3:DeleteObject",
            &bucket,
            Some(&key),
            Self::principal_of(&req),
        )?;
        // sidecar も best-effort で削除 (失敗は無視 — 存在しない場合や IAM 制限を許容)
        let resp = self.backend.delete_object(req).await?;
        let sidecar_input = DeleteObjectInput {
            bucket: bucket.clone(),
            key: sidecar_key(&key),
            ..Default::default()
        };
        let sidecar_req = S3Request {
            input: sidecar_input,
            method: http::Method::DELETE,
            uri: format!("/{bucket}/{}", sidecar_key(&key)).parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        let _ = self.backend.delete_object(sidecar_req).await;
        Ok(resp)
    }
    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        self.backend.delete_objects(req).await
    }
    async fn copy_object(
        &self,
        mut req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        // copy is conceptually "GetObject src + PutObject dst" — enforce both.
        let dst_bucket = req.input.bucket.clone();
        let dst_key = req.input.key.clone();
        self.enforce_policy(
            "s3:PutObject",
            &dst_bucket,
            Some(&dst_key),
            Self::principal_of(&req),
        )?;
        if let CopySource::Bucket { bucket, key, .. } = &req.input.copy_source {
            self.enforce_policy("s3:GetObject", bucket, Some(key), Self::principal_of(&req))?;
        }
        // S4-aware copy: source object に s4-* metadata がある場合、それを
        // destination に確実に preserve する。
        //
        // - MetadataDirective::COPY (default): backend が source metadata を
        //   そのまま copy するので S4 metadata も自動で渡る。介入不要
        // - MetadataDirective::REPLACE: 客が指定した metadata で source を
        //   上書き → s4-* metadata が消えると destination は decompress 不能に
        //   なる (silent corruption)。S4 が source metadata を HEAD で取得し、
        //   s4-* fields を input.metadata に強制 merge する
        let needs_merge = req
            .input
            .metadata_directive
            .as_ref()
            .map(|d| d.as_str() == MetadataDirective::REPLACE)
            .unwrap_or(false);
        if needs_merge && let CopySource::Bucket { bucket, key, .. } = &req.input.copy_source {
            let head_input = HeadObjectInput {
                bucket: bucket.to_string(),
                key: key.to_string(),
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
                && let Some(src_meta) = head.output.metadata.as_ref()
            {
                let dest_meta = req.input.metadata.get_or_insert_with(Default::default);
                for key in [
                    META_CODEC,
                    META_ORIGINAL_SIZE,
                    META_COMPRESSED_SIZE,
                    META_CRC32C,
                    META_MULTIPART,
                    META_FRAMED,
                ] {
                    if let Some(v) = src_meta.get(key) {
                        // 客が同じ key を指定していたら preserve しない (= 上書き許可)
                        // していたら何もしない。指定していなければ insert
                        dest_meta
                            .entry(key.to_string())
                            .or_insert_with(|| v.clone());
                    }
                }
                debug!(
                    src_bucket = %bucket,
                    src_key = %key,
                    "S4 copy_object: preserved s4-* metadata across REPLACE directive"
                );
            }
        }
        self.backend.copy_object(req).await
    }
    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        self.enforce_policy(
            "s3:ListBucket",
            &req.input.bucket,
            None,
            Self::principal_of(&req),
        )?;
        let mut resp = self.backend.list_objects(req).await?;
        // S4 内部 object (`*.s4index` sidecar 等) を顧客から隠す
        if let Some(contents) = resp.output.contents.as_mut() {
            contents.retain(|o| {
                o.key
                    .as_ref()
                    .map(|k| !k.ends_with(".s4index"))
                    .unwrap_or(true)
            });
        }
        Ok(resp)
    }
    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        self.enforce_policy(
            "s3:ListBucket",
            &req.input.bucket,
            None,
            Self::principal_of(&req),
        )?;
        let mut resp = self.backend.list_objects_v2(req).await?;
        if let Some(contents) = resp.output.contents.as_mut() {
            let before = contents.len();
            contents.retain(|o| {
                o.key
                    .as_ref()
                    .map(|k| !k.ends_with(".s4index"))
                    .unwrap_or(true)
            });
            // key_count も補正 (S3 spec compliance)
            if let Some(kc) = resp.output.key_count.as_mut() {
                *kc -= (before - contents.len()) as i32;
            }
        }
        Ok(resp)
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
        // **per-part codec dispatch**: dispatcher が body 先頭 sample から
        // codec を選ぶので、parquet 風の mixed-content multipart で part ごとに
        // 最適 codec を使える (整数列 part → Bitcomp、text 列 part → zstd 等)。
        if let Some(blob) = req.input.body.take() {
            let bytes = collect_blob(blob, self.max_body_bytes)
                .await
                .map_err(internal("collect upload_part body"))?;
            let sample_len = bytes.len().min(SAMPLE_BYTES);
            let codec_kind = self.dispatcher.pick(&bytes[..sample_len]).await;
            let original_size = bytes.len() as u64;
            let (compressed, manifest) = self
                .registry
                .compress(bytes, codec_kind)
                .await
                .map_err(internal("registry compress part"))?;
            let header = FrameHeader {
                codec: codec_kind,
                original_size,
                compressed_size: compressed.len() as u64,
                crc32c: manifest.crc32c,
            };
            let mut framed = BytesMut::with_capacity(FRAME_HEADER_BYTES + compressed.len());
            write_frame(&mut framed, header, &compressed);
            // v0.2 #5: heuristic-based padding skip for likely-final parts.
            //
            // AWS SDK / aws-cli / boto3 always send the final (and only the
            // final) part below the configured part_size. So if the raw user
            // part is already smaller than S3's 5 MiB multipart minimum, this
            // is overwhelmingly likely to be the final part — and the final
            // part is exempt from S3's size constraint. Skipping padding here
            // saves up to ~5 MiB per object on highly compressible workloads.
            //
            // If a misbehaving client sends a tiny **non-final** part, S3
            // itself rejects with EntityTooSmall at CompleteMultipartUpload —
            // identical outcome to a vanilla S3 PUT, just earlier than
            // padding-then-complete would catch it.
            let likely_final = original_size < S3_MULTIPART_MIN_PART_BYTES as u64;
            if !likely_final {
                pad_to_minimum(&mut framed, S3_MULTIPART_MIN_PART_BYTES);
            }
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
        let bucket = req.input.bucket.clone();
        let key = req.input.key.clone();
        let resp = self.backend.complete_multipart_upload(req).await?;
        // CompleteMultipartUpload 成功 → 完成した object を full fetch して frame
        // index を build、`<key>.s4index` sidecar として保存。これで Range GET の
        // partial fetch path が利用可能になる (Range request の帯域節約)。
        // 注: 巨大 object の場合この pass は重いが、Range query は一度 sidecar が
        // できれば爆速になるので 1 回の cost は payback される
        let bucket_clone = bucket.clone();
        let key_clone = key.clone();
        let get_input = GetObjectInput {
            bucket: bucket_clone.clone(),
            key: key_clone.clone(),
            ..Default::default()
        };
        let get_req = S3Request {
            input: get_input,
            method: http::Method::GET,
            uri: format!("/{bucket_clone}/{key_clone}").parse().unwrap(),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        if let Ok(get_resp) = self.backend.get_object(get_req).await
            && let Some(blob) = get_resp.output.body
            && let Ok(body) = collect_blob(blob, self.max_body_bytes).await
            && let Ok(index) = build_index_from_body(&body)
        {
            self.write_sidecar(&bucket, &key, &index).await;
        }
        Ok(resp)
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
        // v0.2 #6: byte-range aware copy when the source is S4-framed.
        //
        // For a framed source (multipart upload OR single-PUT framed-v2),
        // a naive byte-range passthrough would copy compressed bytes that
        // don't align with S4 frame boundaries — silently corrupting the
        // result. Instead we GET the source through S4 (which handles
        // decompression + Range), re-compress + re-frame as a new part,
        // and forward as upload_part. For non-framed sources (S4-untouched
        // raw objects), passthrough is correct and we keep the original
        // (cheaper) code path.
        let CopySource::Bucket {
            bucket: src_bucket,
            key: src_key,
            ..
        } = &req.input.copy_source
        else {
            return self.backend.upload_part_copy(req).await;
        };
        let src_bucket = src_bucket.to_string();
        let src_key = src_key.to_string();

        // Probe metadata to decide whether the source needs S4-aware copy.
        let head_input = HeadObjectInput {
            bucket: src_bucket.clone(),
            key: src_key.clone(),
            ..Default::default()
        };
        let head_req = S3Request {
            input: head_input,
            method: http::Method::HEAD,
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let needs_s4_copy = match self.backend.head_object(head_req).await {
            Ok(h) => {
                is_multipart_object(&h.output.metadata) || is_framed_v2_object(&h.output.metadata)
            }
            Err(_) => false,
        };
        if !needs_s4_copy {
            return self.backend.upload_part_copy(req).await;
        }

        // Resolve the optional source byte range to pass to GET.
        let source_range = req
            .input
            .copy_source_range
            .as_ref()
            .map(|r| parse_copy_source_range(r))
            .transpose()
            .map_err(|e| S3Error::with_message(S3ErrorCode::InvalidRange, e))?;

        // GET source via S4 (handles decompression + sidecar partial fetch
        // when range is present). The result is the requested user-visible
        // byte range, fully decompressed.
        let mut get_input = GetObjectInput {
            bucket: src_bucket.clone(),
            key: src_key.clone(),
            ..Default::default()
        };
        get_input.range = source_range;
        let get_req = S3Request {
            input: get_input,
            method: http::Method::GET,
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let get_resp = self.get_object(get_req).await?;
        let blob = get_resp.output.body.ok_or_else(|| {
            S3Error::with_message(
                S3ErrorCode::InternalError,
                "upload_part_copy: empty body from source GET",
            )
        })?;
        let bytes = collect_blob(blob, self.max_body_bytes)
            .await
            .map_err(internal("collect upload_part_copy source body"))?;

        // Compress + frame as a fresh part (mirrors upload_part path).
        let sample_len = bytes.len().min(SAMPLE_BYTES);
        let codec_kind = self.dispatcher.pick(&bytes[..sample_len]).await;
        let original_size = bytes.len() as u64;
        let (compressed, manifest) = self
            .registry
            .compress(bytes, codec_kind)
            .await
            .map_err(internal("registry compress upload_part_copy"))?;
        let header = FrameHeader {
            codec: codec_kind,
            original_size,
            compressed_size: compressed.len() as u64,
            crc32c: manifest.crc32c,
        };
        let mut framed = BytesMut::with_capacity(FRAME_HEADER_BYTES + compressed.len());
        write_frame(&mut framed, header, &compressed);
        let likely_final = original_size < S3_MULTIPART_MIN_PART_BYTES as u64;
        if !likely_final {
            pad_to_minimum(&mut framed, S3_MULTIPART_MIN_PART_BYTES);
        }
        let framed_bytes = framed.freeze();
        let framed_len = framed_bytes.len() as i64;

        // Forward as upload_part to the destination multipart upload.
        let part_input = UploadPartInput {
            bucket: req.input.bucket.clone(),
            key: req.input.key.clone(),
            part_number: req.input.part_number,
            upload_id: req.input.upload_id.clone(),
            body: Some(bytes_to_blob(framed_bytes)),
            content_length: Some(framed_len),
            ..Default::default()
        };
        let part_req = S3Request {
            input: part_input,
            method: http::Method::PUT,
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            extensions: http::Extensions::new(),
            credentials: req.credentials.clone(),
            region: req.region.clone(),
            service: req.service.clone(),
            trailing_headers: None,
        };
        let upload_resp = self.backend.upload_part(part_req).await?;

        let copy_output = UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: upload_resp.output.e_tag.clone(),
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(S3Response::new(copy_output))
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

    #[test]
    fn parse_copy_source_range_basic() {
        let r = parse_copy_source_range("bytes=10-20").unwrap();
        match r {
            s3s::dto::Range::Int { first, last } => {
                assert_eq!(first, 10);
                assert_eq!(last, Some(20));
            }
            _ => panic!("expected Int range"),
        }
    }

    #[test]
    fn parse_copy_source_range_rejects_inverted() {
        let err = parse_copy_source_range("bytes=20-10").unwrap_err();
        assert!(err.contains("last < first"));
    }

    #[test]
    fn parse_copy_source_range_rejects_missing_prefix() {
        let err = parse_copy_source_range("10-20").unwrap_err();
        assert!(err.contains("must start with 'bytes='"));
    }

    #[test]
    fn parse_copy_source_range_rejects_open_ended() {
        // S3 upload_part_copy spec requires N-M (closed); suffix and
        // open-ended forms are not allowed for this header.
        assert!(parse_copy_source_range("bytes=10-").is_err());
        assert!(parse_copy_source_range("bytes=-10").is_err());
    }
}
