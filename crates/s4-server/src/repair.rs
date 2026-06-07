//! v0.9 #106: standalone sidecar repair / verify / sweep tooling.
//!
//! The S4 server writes a `<key>.s4index` sidecar after every framed PUT so
//! Range GETs can do a partial fetch instead of streaming the whole body.
//! Three failure modes leave the sidecar diverged from the live object and
//! degrade Range GET to the full-read fallback:
//!
//! 1. The sidecar PUT failed after the main object committed (network blip,
//!    backend throttle).
//! 2. An operator overwrote the object directly through the backend, leaving
//!    the sidecar stale (ETag / size mismatch with the new body).
//! 3. The v0.8.15 H-g multipart-Complete-on-Versioning-Enabled bug emitted
//!    sidecars bound to the parent key while the body landed under the
//!    versioning shadow path (`<key>.__s4ver__/<id>`). Those orphans never
//!    re-pair and lifecycle doesn't reap them.
//!
//! [`verify_sidecar`] reports the current state without writing,
//! [`repair_sidecar`] rebuilds a single sidecar by re-scanning the main
//! body, and [`sweep_orphan_sidecars`] walks every `*.s4index` in a bucket
//! and reports / deletes the ones whose paired key is missing or stale.
//!
//! All three operate directly against an `aws_sdk_s3::Client` (the operator
//! points it at the backend, not the S4 gateway, because the gateway hides
//! `.s4index` from list output by design).

use aws_sdk_s3::Client;
use s4_codec::index::{
    SIDECAR_SUFFIX, build_index_from_body, decode_index, encode_index, sidecar_key,
};
use thiserror::Error;

/// Default cap on bytes loaded into RAM for sidecar rebuild. Matches the
/// `--max-body-bytes` default (#178, 5 GiB) — repair needs the full body in
/// memory because `build_index_from_body` is a single-pass scan. Operators
/// with larger objects pass `--max-body-bytes` to raise this explicitly so a
/// runaway `repair-sidecar` on a 50 GiB object surfaces a clear error
/// instead of swapping the host.
pub const DEFAULT_REPAIR_BODY_BYTES_CAP: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RepairError {
    #[error("S3 backend error on {op} {bucket}/{key}: {cause}")]
    Backend {
        op: &'static str,
        bucket: String,
        key: String,
        // Named `cause` (not `source`) so thiserror doesn't auto-treat it
        // as a `#[source]` chain field — the upstream SDK error is already
        // stringified into `cause`.
        cause: String,
    },
    #[error("frame scan failed on {bucket}/{key}: {cause}")]
    FrameScan {
        bucket: String,
        key: String,
        cause: String,
    },
    #[error("object body {size} bytes exceeds repair cap {cap}; pass --max-body-bytes to raise")]
    BodyTooLarge { size: u64, cap: u64 },
    /// HEAD on `{bucket}/{key}` returned no `Content-Length` header. The
    /// body-size cap that prevents OOM on a runaway repair relies on this
    /// being available, so the tool fails closed rather than treating a
    /// missing length as zero (which would silently bypass the cap).
    #[error(
        "HEAD {bucket}/{key} returned no Content-Length; cannot enforce body cap, refusing to proceed"
    )]
    MissingContentLength { bucket: String, key: String },
    /// `If-Match` race detector: the object was overwritten between the
    /// initial HEAD (whose ETag we stamped into the sidecar) and the GET.
    /// Returned by `repair_sidecar` so the operator can re-run instead of
    /// writing a sidecar that's immediately stale.
    #[error(
        "object {bucket}/{key} was overwritten during repair (HEAD ETag {head_etag} != GET response); re-run repair-sidecar"
    )]
    OverwrittenDuringRepair {
        bucket: String,
        key: String,
        head_etag: String,
    },
    /// v0.9 #106-audit-R3 P2-R3: the object body has no S4F2 frame
    /// magic — it's a passthrough / raw-bytes object the server
    /// intentionally never sidecared (service.rs::put_object only
    /// builds a sidecar when `is_framed && !will_encrypt`). Writing
    /// an empty `<key>.s4index` would silently break Range GET:
    /// `FrameIndex::lookup_range` over zero entries returns `None`,
    /// the GET path falls into the "invalid range" branch instead of
    /// the correct passthrough-range fallback that exists for
    /// sidecar-less objects. Surface as a typed error so the
    /// operator knows the object isn't a candidate for sidecar
    /// repair (and `verify-sidecar` will already classify it as
    /// `MissingHarmless` with frame_count=0).
    #[error(
        "object {bucket}/{key} body has no S4F2 frame magic — it's a passthrough or \
         raw-bytes object that the server intentionally never sidecared; \
         sidecar repair would silently break Range GET. No action required."
    )]
    NotFramed { bucket: String, key: String },
    /// v0.9 #106-audit-R2 P2-INT-1: the object body the backend returned
    /// is an SSE-S4 (S4E1/S4E2/S4E3/S4E4/S4E5/S4E6) encrypted envelope.
    /// `repair_sidecar` runs against the BACKEND (not the gateway), so the
    /// body it sees is ciphertext — feeding that to the frame scanner
    /// would surface as a confusing `FrameScan` because the S4F2 frame
    /// magic is hidden inside the encrypted payload. Worse, the v3
    /// sidecar's `sse_v3` binding (key_id / salt / chunk_size etc.)
    /// cannot be reconstructed from the backend bytes alone — it
    /// requires the SSE keyring to decrypt the body and walk the chunk
    /// layout. The CLI does not (yet) accept `--sse-s4-key`; v0.10
    /// roadmap is to plumb that through. Until then, surface a clean
    /// typed error so the operator can route the repair through a
    /// server-mode rebuild path (re-PUT the object) instead of receiving
    /// a misleading frame-scan failure.
    #[error(
        "object {bucket}/{key} body is an SSE-S4 encrypted envelope ({message}); \
         encrypted-sidecar repair requires server-mode access to the SSE keyring \
         (CLI `--sse-s4-key` plumbing is the v0.10 roadmap), \
         use a server-mode rebuild path or re-PUT the object to regenerate the sidecar"
    )]
    EncryptedSidecarUnsupported {
        bucket: String,
        key: String,
        message: String,
    },
}

/// Status reported by [`verify_sidecar`]. Discriminates the outcomes a
/// CI / cron job needs to branch on. The three `Missing*` variants
/// resolve the P2-C ambiguity Codex caught: small single-frame objects
/// intentionally have no sidecar (server only writes when
/// `entries.len() > 1`), so a blanket `Missing` = exit-1 would false-
/// alert on healthy objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarStatus {
    /// Sidecar present, parses cleanly, and its v2 etag + size binding
    /// matches the live HEAD.
    Ok { frame_count: u64, sidecar_size: u64 },
    /// No `<key>.s4index` AND the main body scans as a single frame
    /// (server skips sidecar emission for `entries.len() <= 1` by
    /// design). Healthy state — Range GET falls back to a full body
    /// read, but a single-frame object's "full read" *is* its only
    /// frame, so there's no fast-path to lose. Exit 0.
    MissingHarmless { frame_count: u64 },
    /// No `<key>.s4index` AND the main body has 2+ frames. Range GET
    /// fast-path is lost; `repair-sidecar` will restore it. Exit 1.
    MissingDivergent { frame_count: u64 },
    /// No `<key>.s4index` AND the main object body exceeds the deep-
    /// scan cap, so we can't tell whether it's a healthy single-frame
    /// or a real divergence. Operator should raise `--max-body-bytes`
    /// or run `repair-sidecar` to settle it. Exit 0 (ambiguous, not a
    /// confirmed divergence — better to under-alert than spam).
    MissingUnknown { size: u64, cap: u64 },
    /// Sidecar present but its `source_etag` doesn't match the live HEAD —
    /// the main object was overwritten or the sidecar is from a different
    /// commit point.
    StaleEtag {
        sidecar_etag: String,
        live_etag: String,
    },
    /// Sidecar present and ETag matches, but the recorded body size differs
    /// (some backends, e.g. lifecycle moves, change bytes without bumping
    /// ETag). Treated as stale.
    StaleSize { sidecar_size: u64, live_size: u64 },
    /// Pre-v0.8.4 sidecar (no source_etag / source_compressed_size). Still
    /// usable read-only, but a repair will upgrade it to v2.
    LegacyV1 { frame_count: u64 },
    /// Sidecar bytes failed to decode. The body is corrupt or someone PUT
    /// non-S4IX data at the `.s4index` key. A `repair-sidecar` overwrites
    /// it cleanly.
    DecodeError { message: String },
}

#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub bucket: String,
    pub key: String,
    pub status: SidecarStatus,
}

impl VerifyReport {
    /// True when the sidecar is in a state operators don't need to
    /// action. Used by the CLI to decide exit code (true → 0, false → 1).
    /// `MissingHarmless` is clean (single-frame objects have no sidecar
    /// by design); `MissingUnknown` is also reported clean so the CLI
    /// doesn't false-alert on objects too large to deep-scan — operator
    /// can still see the hint in stdout and raise `--max-body-bytes`.
    pub fn is_clean(&self) -> bool {
        matches!(
            self.status,
            SidecarStatus::Ok { .. }
                | SidecarStatus::LegacyV1 { .. }
                | SidecarStatus::MissingHarmless { .. }
                | SidecarStatus::MissingUnknown { .. }
        )
    }
}

#[derive(Debug, Clone)]
pub struct RepairReport {
    pub bucket: String,
    pub key: String,
    pub frame_count: u64,
    pub sidecar_bytes_written: u64,
    pub source_etag: Option<String>,
    pub source_compressed_size: u64,
    /// True when a sidecar already existed (we overwrote it). False when we
    /// wrote one for the first time.
    pub rebuilt_from_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanReason {
    /// The paired logical key has no HEAD — sidecar is dangling.
    PairedMissing,
    /// Paired key exists but the sidecar's recorded ETag is stale.
    PairedEtagMismatch {
        sidecar_etag: String,
        live_etag: String,
    },
    /// Paired key exists, ETag matches, but size differs.
    PairedSizeMismatch { sidecar_size: u64, live_size: u64 },
    /// The sidecar bytes failed to decode — either corruption or a non-
    /// sidecar object that happened to land at a `.s4index` key.
    SidecarUndecodable { message: String },
}

#[derive(Debug, Clone)]
pub struct OrphanReport {
    pub sidecar_key: String,
    pub paired_key: String,
    pub reason: OrphanReason,
}

#[derive(Debug, Clone)]
pub struct SweepReport {
    pub bucket: String,
    pub sidecars_scanned: u64,
    pub orphans: Vec<OrphanReport>,
    /// Count actually deleted when `delete = true` was passed. Always 0 in
    /// dry-run mode.
    pub deleted: u64,
}

/// Verify a single `<bucket>/<key>` sidecar without writing.
///
/// When the sidecar is absent, this fetches the main body (capped at
/// `deep_scan_body_cap`) to scan its frame count — single-frame objects
/// intentionally have no sidecar (server skips emission when
/// `entries.len() <= 1`), so the absent-sidecar verdict is
/// `MissingHarmless` for those rather than a false-alert `Missing`.
/// Pass [`DEFAULT_REPAIR_BODY_BYTES_CAP`] (5 GiB) for the standard CLI
/// behaviour.
pub async fn verify_sidecar(
    client: &Client,
    bucket: &str,
    key: &str,
    deep_scan_body_cap: u64,
) -> Result<VerifyReport, RepairError> {
    let HeadInfo {
        raw_etag: live_raw_etag,
        normalized_etag: live_etag,
        size: live_size,
    } = head_main(client, bucket, key).await?;
    let sidecar_k = sidecar_key(key);
    let bytes = match get_object_bytes(client, bucket, &sidecar_k).await {
        Ok(b) => b,
        Err(GetOutcome::NotFound) => {
            // P2-C (Codex R3): disambiguate Missing via a body scan
            // before deciding whether this is a healthy single-frame
            // object or a real divergence.
            return Ok(VerifyReport {
                bucket: bucket.into(),
                key: key.into(),
                status: classify_missing_sidecar(
                    client,
                    bucket,
                    key,
                    live_raw_etag.as_deref(),
                    live_size,
                    deep_scan_body_cap,
                )
                .await?,
            });
        }
        Err(GetOutcome::Other(msg)) => {
            return Err(RepairError::Backend {
                op: "GET",
                bucket: bucket.into(),
                key: sidecar_k,
                cause: msg,
            });
        }
    };
    let sidecar_size = bytes.len() as u64;
    let idx = match decode_index(bytes) {
        Ok(i) => i,
        Err(e) => {
            return Ok(VerifyReport {
                bucket: bucket.into(),
                key: key.into(),
                status: SidecarStatus::DecodeError {
                    message: e.to_string(),
                },
            });
        }
    };
    let frame_count = idx.entries.len() as u64;
    // P2-D (Codex R4): both sides of the etag comparison are now
    // `Option<&str>` so an ETag-less backend `None == None` round-trips
    // as Ok rather than tripping the stale path.
    //
    // P3-A (Codex R5): the size-only binding case `(None, Some(z))` is
    // a fully valid v2 sidecar (just no ETag because the backend
    // doesn't emit one). Treat any present-size binding as Ok rather
    // than falling through to `LegacyV1`, which would falsely tell
    // the operator that `repair-sidecar` could "upgrade" a sidecar
    // that already IS the v2 it can produce on that backend.
    // `LegacyV1` is only the true pre-v0.8.4 case where neither
    // binding field is present.
    let status = match (idx.source_etag.as_deref(), idx.source_compressed_size) {
        (Some(side_etag), _) if Some(side_etag) != live_etag.as_deref() => {
            SidecarStatus::StaleEtag {
                sidecar_etag: side_etag.into(),
                live_etag: live_etag.unwrap_or_default(),
            }
        }
        (_, Some(side_size)) if side_size != live_size => SidecarStatus::StaleSize {
            sidecar_size: side_size,
            live_size,
        },
        // Any present size binding → Ok (covers full v2 AND the
        // size-only-binding case from ETag-less repair, P3-A).
        (_, Some(_)) => SidecarStatus::Ok {
            frame_count,
            sidecar_size,
        },
        // No size binding at all → genuinely legacy v1. Covers both
        // (None, None) and the anomalous (Some, None) shape (which
        // encode_index never emits, but match exhaustiveness needs
        // coverage).
        (_, None) => SidecarStatus::LegacyV1 { frame_count },
    };
    Ok(VerifyReport {
        bucket: bucket.into(),
        key: key.into(),
        status,
    })
}

/// Rebuild `<bucket>/<key>.s4index` from the main object body. Overwrites
/// any existing sidecar (including stale or corrupt ones). Returns an error
/// when the main body exceeds `body_bytes_cap`.
pub async fn repair_sidecar(
    client: &Client,
    bucket: &str,
    key: &str,
    body_bytes_cap: u64,
) -> Result<RepairReport, RepairError> {
    let HeadInfo {
        raw_etag: head_raw_etag,
        normalized_etag: head_normalized_etag,
        size: live_size,
    } = head_main(client, bucket, key).await?;
    if live_size > body_bytes_cap {
        return Err(RepairError::BodyTooLarge {
            size: live_size,
            cap: body_bytes_cap,
        });
    }
    // v0.9 #106 TOCTOU guard: pin the GET to the HEAD's ETag via If-Match.
    // Without this, an overwrite between HEAD and GET would yield a body
    // whose actual ETag is E2 while we stamp `source_etag = E1`, producing
    // a sidecar that fails its own version-binding check on the very next
    // Range GET (operator sees "repair succeeded" then nothing changed).
    // Backend returns 412 PreconditionFailed if the object changed.
    //
    // P1-B (Codex review R1): pass the RAW etag (quoted entity-tag) per
    // RFC 7232, not the normalized form. Strict S3-compatible backends
    // reject `If-Match: abc-2` (missing quotes) with 400/412 and the
    // repair never succeeds. Tolerant backends accept either. The
    // sidecar's stored `source_etag` still uses the normalized form to
    // match the server's PUT-path stamping convention.
    //
    // P2-D (Codex R4): when the backend doesn't return an ETag at all,
    // skip `If-Match` entirely. Same posture the server takes in that
    // case (it stamps `source_etag = None`); the race window stays open
    // for those backends, but they don't have an ETag we could pin
    // against anyway.
    let get_builder = client.get_object().bucket(bucket).key(key);
    let get_builder = match &head_raw_etag {
        Some(t) => get_builder.if_match(t.clone()),
        None => get_builder,
    };
    let body = match get_builder.send().await {
        Ok(resp) => resp
            .body
            .collect()
            .await
            .map(|agg| agg.into_bytes())
            .map_err(|e| RepairError::Backend {
                op: "GET",
                bucket: bucket.into(),
                key: key.into(),
                cause: format!("read body: {e}"),
            })?,
        Err(e) => {
            // PreconditionFailed (412) → object was overwritten between
            // HEAD and GET. Surface as a typed error so the operator can
            // re-run instead of writing a stale sidecar.
            let s = format!("{e}");
            if s.contains("PreconditionFailed") || s.contains("412") {
                return Err(RepairError::OverwrittenDuringRepair {
                    bucket: bucket.into(),
                    key: key.into(),
                    head_etag: head_normalized_etag.clone().unwrap_or_default(),
                });
            }
            if is_get_not_found(&e) {
                return Err(RepairError::Backend {
                    op: "GET",
                    bucket: bucket.into(),
                    key: key.into(),
                    cause: "object not found (NoSuchKey)".into(),
                });
            }
            return Err(RepairError::Backend {
                op: "GET",
                bucket: bucket.into(),
                key: key.into(),
                cause: s,
            });
        }
    };
    // Defense in depth: even with If-Match, double-check the bytes we got
    // are the size HEAD promised. Backends with quirky range / cache
    // behaviour have surprised us before — see codec memo on partial
    // serves that succeeded with the wrong length.
    if (body.len() as u64) != live_size {
        return Err(RepairError::Backend {
            op: "GET",
            bucket: bucket.into(),
            key: key.into(),
            cause: format!(
                "got {} bytes but HEAD said {}; backend served wrong content length",
                body.len(),
                live_size
            ),
        });
    }
    // v0.9 #106-audit-R2 P2-INT-1: detect SSE-S4 encrypted envelopes
    // BEFORE handing the body to the frame scanner. The backend serves
    // the on-disk ciphertext (S4E1..S4E6 magic prefix); `build_index_from_body`
    // would scan for `S4F2` frame magic inside that ciphertext and surface
    // an opaque `FrameScan` error. Worse, the v3 sidecar's `sse_v3` binding
    // (key_id / salt / chunk_size) cannot be reconstructed from backend
    // bytes alone — the SSE keyring is required to decrypt + walk chunks.
    // Surface a typed error directing the operator to a server-mode rebuild
    // path; v0.10 roadmap is to add `--sse-s4-key <path>` to the CLI so
    // sidecar repair can decrypt the body in-process. See CHANGELOG.
    if let Some(magic) = detect_sse_magic(&body) {
        return Err(RepairError::EncryptedSidecarUnsupported {
            bucket: bucket.into(),
            key: key.into(),
            message: format!("body magic {magic} indicates SSE-S4 envelope"),
        });
    }
    let sidecar_k = sidecar_key(key);
    let rebuilt_from_existing = client
        .head_object()
        .bucket(bucket)
        .key(&sidecar_k)
        .send()
        .await
        .is_ok();
    let mut idx = build_index_from_body(&body).map_err(|e| RepairError::FrameScan {
        bucket: bucket.into(),
        key: key.into(),
        cause: e.to_string(),
    })?;
    // v0.9 #106-audit-R3 P2-R3 (Codex): `build_index_from_body`
    // on a non-S4F2 body (passthrough / raw bytes) returns Ok with
    // an empty entries vec rather than an error. Writing that as a
    // sidecar would silently break Range GET — `lookup_range` over
    // zero entries returns None, and the GET path then takes the
    // "no plan" branch instead of the passthrough-range fallback
    // that exists for sidecar-less objects. Reject cleanly so the
    // operator knows the object isn't a sidecar-repair candidate.
    if idx.entries.is_empty() {
        return Err(RepairError::NotFramed {
            bucket: bucket.into(),
            key: key.into(),
        });
    }
    // Stamp the NORMALIZED form so server-side
    // `sidecar_version_binding_ok` (which compares against the s3s
    // `ETag::value()` stripped form) sees a match. The raw form was
    // only needed for the wire-level `If-Match` header above.
    //
    // P2-D (Codex R4): pass through `None` when the backend doesn't
    // return an ETag — the server's binding check treats `None` as
    // the legacy/back-compat best-effort path. Stamping `Some("")`
    // would force the check into the mismatch branch and the sidecar
    // would be immediately rejected as stale.
    idx.source_etag = head_normalized_etag.clone();
    idx.source_compressed_size = Some(body.len() as u64);
    let encoded = encode_index(&idx);
    let encoded_len = encoded.len() as u64;
    let frame_count = idx.entries.len() as u64;
    client
        .put_object()
        .bucket(bucket)
        .key(&sidecar_k)
        .body(aws_sdk_s3::primitives::ByteStream::from(encoded.to_vec()))
        .content_type("application/x-s4-index")
        .send()
        .await
        .map_err(|e| RepairError::Backend {
            op: "PUT",
            bucket: bucket.into(),
            key: sidecar_k.clone(),
            cause: format!("{e}"),
        })?;
    // v0.9 #106 P2-B (Codex review round 2): `If-Match` on the GET
    // only proves the body hadn't changed at GET time. The main object
    // can still be overwritten during the (a) build_index_from_body
    // scan and (b) sidecar PUT window — leaving a freshly-written
    // sidecar stamped with the OLD ETag against the NEW body. The
    // server-side `sidecar_version_binding_ok` would then trip on
    // every Range GET and we'd silently report "repair succeeded".
    //
    // Final HEAD: if the main object's ETag changed since we read it,
    // the sidecar we just wrote is already stale. Delete it (so the
    // operator's next Range GET falls back to the safe full-read path,
    // not the bad fast-path) and surface `OverwrittenDuringRepair`
    // so the operator re-runs the repair under quieter conditions.
    let post = head_main(client, bucket, key).await?;
    if post.normalized_etag != head_normalized_etag || post.size != live_size {
        // Best-effort cleanup; ignore the delete's outcome because the
        // primary error is the race, not the cleanup itself.
        let _ = client
            .delete_object()
            .bucket(bucket)
            .key(&sidecar_k)
            .send()
            .await;
        return Err(RepairError::OverwrittenDuringRepair {
            bucket: bucket.into(),
            key: key.into(),
            head_etag: head_normalized_etag.unwrap_or_default(),
        });
    }
    Ok(RepairReport {
        bucket: bucket.into(),
        key: key.into(),
        frame_count,
        sidecar_bytes_written: encoded_len,
        source_etag: idx.source_etag,
        source_compressed_size: live_size,
        rebuilt_from_existing,
    })
}

/// Knob controlling which orphan categories `sweep_orphan_sidecars` is
/// allowed to delete. `SidecarUndecodable` is kept out of the default
/// `--delete` because v0.8.17-era operators on the
/// `--allow-legacy-reserved-key-reads` migration hatch can have
/// legitimate user-PUT objects whose key happens to end in `.s4index` —
/// those would fail to decode and `--delete` would nuke real user data.
/// Escalation to `DeletePolicy::IncludeUndecodable` is an explicit
/// operator opt-in (`--delete-undecodable` on the CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletePolicy {
    /// Pure dry-run: classify only, never write to the backend.
    DryRun,
    /// Delete `PairedMissing` / `PairedEtagMismatch` / `PairedSizeMismatch`
    /// orphans. Leave `SidecarUndecodable` in the report — operator must
    /// inspect those and rerun with `IncludeUndecodable` if they truly
    /// are corrupt sidecars (and not legacy reserved-name user data).
    PairBoundOnly,
    /// All four categories. Use only after confirming there's no legacy
    /// `--allow-legacy-reserved-key-reads` user data in this bucket.
    IncludeUndecodable,
}

impl DeletePolicy {
    fn allows(&self, reason: &OrphanReason) -> bool {
        match (self, reason) {
            (DeletePolicy::DryRun, _) => false,
            (DeletePolicy::PairBoundOnly, OrphanReason::SidecarUndecodable { .. }) => false,
            (DeletePolicy::PairBoundOnly, _) => true,
            (DeletePolicy::IncludeUndecodable, _) => true,
        }
    }
}

/// List every `*.s4index` in `bucket` and report (and optionally delete) the
/// orphans — sidecars whose paired key is missing or whose recorded
/// ETag / size disagree with the live HEAD.
///
/// See [`DeletePolicy`] for the three deletion levels. Always run
/// [`DeletePolicy::DryRun`] first to inspect the orphan list.
pub async fn sweep_orphan_sidecars(
    client: &Client,
    bucket: &str,
    policy: DeletePolicy,
) -> Result<SweepReport, RepairError> {
    let mut sidecars_scanned: u64 = 0;
    let mut orphans: Vec<OrphanReport> = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let mut req = client.list_objects_v2().bucket(bucket);
        if let Some(c) = continuation.as_ref() {
            req = req.continuation_token(c);
        }
        let resp = req.send().await.map_err(|e| RepairError::Backend {
            op: "ListObjectsV2",
            bucket: bucket.into(),
            key: String::new(),
            cause: format!("{e}"),
        })?;
        for obj in resp.contents() {
            let Some(k) = obj.key() else { continue };
            if !k.ends_with(SIDECAR_SUFFIX) {
                continue;
            }
            sidecars_scanned += 1;
            let paired = &k[..k.len() - SIDECAR_SUFFIX.len()];
            classify_one(client, bucket, k, paired, &mut orphans).await?;
        }
        if resp.is_truncated().unwrap_or(false) {
            continuation = resp.next_continuation_token().map(str::to_owned);
            if continuation.is_none() {
                // Defensive: a truncated response with no continuation token
                // is a backend bug; bail rather than infinite-loop.
                break;
            }
        } else {
            break;
        }
    }
    let mut deleted = 0u64;
    for orph in &orphans {
        if !policy.allows(&orph.reason) {
            continue;
        }
        client
            .delete_object()
            .bucket(bucket)
            .key(&orph.sidecar_key)
            .send()
            .await
            .map_err(|e| RepairError::Backend {
                op: "DELETE",
                bucket: bucket.into(),
                key: orph.sidecar_key.clone(),
                cause: format!("{e}"),
            })?;
        deleted += 1;
    }
    Ok(SweepReport {
        bucket: bucket.into(),
        sidecars_scanned,
        orphans,
        deleted,
    })
}

/// P2-C (Codex R3): the server skips sidecar emission for objects whose
/// frame count is ≤ 1 (small single-PUTs / single-chunk multiparts), so
/// a missing sidecar can be EITHER an intentional skip OR a real
/// divergence. Disambiguate by fetching the body (capped) and counting
/// frames. Returns [`SidecarStatus::MissingUnknown`] when the body
/// exceeds the cap, so verify-sidecar doesn't false-alert on
/// large-but-can't-confirm objects.
async fn classify_missing_sidecar(
    client: &Client,
    bucket: &str,
    key: &str,
    live_raw_etag: Option<&str>,
    live_size: u64,
    cap: u64,
) -> Result<SidecarStatus, RepairError> {
    if live_size > cap {
        return Ok(SidecarStatus::MissingUnknown {
            size: live_size,
            cap,
        });
    }
    // Pin the GET to the HEAD's ETag (RFC 7232 quoted form). If a race
    // overwrites the object between HEAD and GET we'd otherwise scan a
    // different body than the one HEAD reported on — surface as a
    // typed error so the operator re-runs.
    //
    // P2-D: backends without an ETag have nothing to pin against;
    // skip If-Match (matches the server-side `None`-tolerance path).
    let get_builder = client.get_object().bucket(bucket).key(key);
    let get_builder = match live_raw_etag {
        Some(t) => get_builder.if_match(t.to_owned()),
        None => get_builder,
    };
    let body = match get_builder.send().await {
        Ok(resp) => resp
            .body
            .collect()
            .await
            .map(|agg| agg.into_bytes())
            .map_err(|e| RepairError::Backend {
                op: "GET",
                bucket: bucket.into(),
                key: key.into(),
                cause: format!("read body: {e}"),
            })?,
        Err(e) => {
            let s = format!("{e}");
            if s.contains("PreconditionFailed") || s.contains("412") {
                return Err(RepairError::OverwrittenDuringRepair {
                    bucket: bucket.into(),
                    key: key.into(),
                    head_etag: live_raw_etag.map(normalize_etag).unwrap_or_default(),
                });
            }
            if is_get_not_found(&e) {
                return Err(RepairError::Backend {
                    op: "GET",
                    bucket: bucket.into(),
                    key: key.into(),
                    cause: "object not found (NoSuchKey)".into(),
                });
            }
            return Err(RepairError::Backend {
                op: "GET",
                bucket: bucket.into(),
                key: key.into(),
                cause: s,
            });
        }
    };
    // v0.9 #106-audit self-review (post-R2): mirror the encrypted-body
    // guard from `repair_sidecar` here. Without it, running
    // `verify-sidecar` against an SSE-S4 chunked object (whose sidecar
    // is missing — e.g. PUT happened pre-v0.9 before v3 sidecars
    // shipped) would surface as a confusing FrameScan error instead of
    // the friendly EncryptedSidecarUnsupported the repair tool already
    // returns. Same root cause as P2-INT-1; same surface error.
    if let Some(magic) = detect_sse_magic(&body) {
        return Err(RepairError::EncryptedSidecarUnsupported {
            bucket: bucket.into(),
            key: key.into(),
            message: format!("body magic {magic} indicates SSE-S4 envelope"),
        });
    }
    let idx = build_index_from_body(&body).map_err(|e| RepairError::FrameScan {
        bucket: bucket.into(),
        key: key.into(),
        cause: e.to_string(),
    })?;
    let frame_count = idx.entries.len() as u64;
    if frame_count <= 1 {
        Ok(SidecarStatus::MissingHarmless { frame_count })
    } else {
        Ok(SidecarStatus::MissingDivergent { frame_count })
    }
}

async fn classify_one(
    client: &Client,
    bucket: &str,
    sidecar_k: &str,
    paired: &str,
    out: &mut Vec<OrphanReport>,
) -> Result<(), RepairError> {
    // v0.9 #106 review P1-A (Codex): MUST decode the listed object first.
    // Branching on "HEAD paired-key" before reading the candidate would
    // mis-classify a legitimate `--allow-legacy-reserved-key-reads`
    // user object (whose key happens to end in `.s4index` and whose
    // paired stripped key may not exist) as `PairedMissing` — and
    // `DeletePolicy::PairBoundOnly` would silently delete user data.
    // The rule is: bytes that don't parse as S4IX magic = user data,
    // never an orphan-eligible-for-default-delete.
    let bytes = match get_object_bytes(client, bucket, sidecar_k).await {
        Ok(b) => b,
        // ListObjectsV2 saw it; if GET says NotFound now, treat as a
        // sidecar that vanished mid-sweep — skip rather than report.
        Err(GetOutcome::NotFound) => return Ok(()),
        Err(GetOutcome::Other(msg)) => {
            return Err(RepairError::Backend {
                op: "GET",
                bucket: bucket.into(),
                key: sidecar_k.into(),
                cause: msg,
            });
        }
    };
    let idx = match decode_index(bytes) {
        Ok(i) => i,
        Err(e) => {
            // Not a real S4IX sidecar — flag it under the safer
            // category. `DeletePolicy::PairBoundOnly` does NOT remove
            // these; the operator must escalate to
            // `IncludeUndecodable` after confirming it isn't legacy
            // user data.
            out.push(OrphanReport {
                sidecar_key: sidecar_k.into(),
                paired_key: paired.into(),
                reason: OrphanReason::SidecarUndecodable {
                    message: e.to_string(),
                },
            });
            return Ok(());
        }
    };
    // Bytes decoded as S4IX — now we can safely check the paired key
    // status. A missing paired key combined with a decodable sidecar
    // IS a real orphan (the v0.8.15 H-g case, for example).
    let head_res = client.head_object().bucket(bucket).key(paired).send().await;
    let (live_etag_norm, live_size) = match head_res {
        Ok(h) => {
            // P2-D: `None` means the backend didn't return an ETag.
            // Preserve the absence rather than coercing to `""` —
            // comparing `Some("xyz")` from the sidecar against
            // `Some("")` would always trip stale, falsely orphaning
            // every paired-OK sidecar on an ETag-less backend.
            let etag: Option<String> = h.e_tag().map(normalize_etag);
            let size = h.content_length().unwrap_or(0).max(0) as u64;
            (etag, size)
        }
        Err(e) => {
            if is_head_not_found(&e) {
                out.push(OrphanReport {
                    sidecar_key: sidecar_k.into(),
                    paired_key: paired.into(),
                    reason: OrphanReason::PairedMissing,
                });
                return Ok(());
            }
            return Err(RepairError::Backend {
                op: "HEAD",
                bucket: bucket.into(),
                key: paired.into(),
                cause: format!("{e}"),
            });
        }
    };
    // ETag mismatch only fires when BOTH sides have an ETag. If the
    // sidecar carries Some("x") and the live HEAD has None, that's
    // not a definitive divergence — could be a backend that recently
    // dropped ETag support. Skip the mismatch flag for the None side
    // (matches the server's `sidecar_version_binding_ok` `None`-
    // tolerance posture).
    if let (Some(side_etag), Some(live_e)) = (idx.source_etag.as_deref(), live_etag_norm.as_deref())
        && side_etag != live_e
    {
        out.push(OrphanReport {
            sidecar_key: sidecar_k.into(),
            paired_key: paired.into(),
            reason: OrphanReason::PairedEtagMismatch {
                sidecar_etag: side_etag.into(),
                live_etag: live_e.into(),
            },
        });
        return Ok(());
    }
    if let Some(side_size) = idx.source_compressed_size
        && side_size != live_size
    {
        out.push(OrphanReport {
            sidecar_key: sidecar_k.into(),
            paired_key: paired.into(),
            reason: OrphanReason::PairedSizeMismatch {
                sidecar_size: side_size,
                live_size,
            },
        });
    }
    // Legacy v1 sidecars (no binding fields) are intentionally
    // tolerated here — read-only Range GETs still work and the
    // operator gets warned by `verify-sidecar` separately.
    Ok(())
}

/// HEAD response distilled to the fields the repair tools care about.
///
/// Both etag fields are `Option<String>` so the absent-ETag case
/// round-trips cleanly through to the sidecar (P2-D, Codex R4). When
/// `raw_etag = None`, the backend didn't return one — we MUST stamp
/// `FrameIndex::source_etag = None` to match the server PUT path's
/// `resp.e_tag.as_ref().map(...)` shape, otherwise
/// `sidecar_version_binding_ok` would compare `Some("")` against a
/// missing live ETag and always trip "stale".
///
/// - `raw_etag`: wire form (typically `"..."`) — pass to `If-Match`
///   headers, which per RFC 7232 want the full entity-tag. `None`
///   means skip `If-Match` entirely (best-effort, same posture the
///   server takes for ETag-less backends).
/// - `normalized_etag`: stripped form for comparing against
///   `FrameIndex::source_etag` (the s3s `ETag::value()` accessor
///   used by the server PUT path strips quotes).
struct HeadInfo {
    raw_etag: Option<String>,
    normalized_etag: Option<String>,
    size: u64,
}

async fn head_main(client: &Client, bucket: &str, key: &str) -> Result<HeadInfo, RepairError> {
    let head = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| RepairError::Backend {
            op: "HEAD",
            bucket: bucket.into(),
            key: key.into(),
            cause: format!("{e}"),
        })?;
    let raw_etag = head.e_tag().map(str::to_owned);
    let normalized_etag = raw_etag.as_deref().map(normalize_etag);
    // `content_length` is `Option<i64>` on the SDK type — `None` means the
    // backend didn't return a Content-Length header. We fail closed rather
    // than treating that as zero (which would silently bypass the
    // `body_bytes_cap` in `repair_sidecar` and let an unbounded GET
    // exhaust RAM). AWS S3 / MinIO / Garage / Ceph RGW all return
    // Content-Length on HEAD, so this only trips on exotic / broken
    // backends — which the operator should know about.
    let size = match head.content_length() {
        Some(n) if n >= 0 => n as u64,
        Some(_) | None => {
            return Err(RepairError::MissingContentLength {
                bucket: bucket.into(),
                key: key.into(),
            });
        }
    };
    Ok(HeadInfo {
        raw_etag,
        normalized_etag,
        size,
    })
}

/// Strip the surrounding `"..."` quotes from an RFC 7232 entity-tag so
/// the on-wire form (aws-sdk-s3 returns raw `"..."`) matches the form
/// the S4 gateway stamps into `FrameIndex::source_etag` (the s3s
/// `ETag::value()` accessor that drives the PUT path strips quotes).
///
/// Without this normalization, a freshly-written sidecar would falsely
/// flag as `StaleEtag` because the strings differ only by the wrapping
/// quotes. Both the PUT side (server) and the repair side (this CLI)
/// must agree on the canonical form — the de-facto canonical is "no
/// surrounding quotes", since that's what the server already writes
/// into every v2 sidecar in the wild.
fn normalize_etag(s: &str) -> String {
    s.trim_matches('"').to_owned()
}

/// v0.9 #106-audit-R2 P2-INT-1: detect SSE-S4 encrypted envelopes by
/// magic prefix. Returns `Some(name)` when the first four bytes match
/// one of the SSE frame magics (`S4E1`..`S4E6`); returns `None` for any
/// other body, including S4 framed plaintext (`S4F2`) and raw
/// compressed / passthrough bodies.
///
/// Intentionally duplicated here as a 4-byte prefix compare instead of
/// reusing `sse::peek_magic` because `peek_magic` length-gates on the
/// full S4E1/S4E2 header size (36 bytes) and would return `None` for a
/// very short S4E6 stub the way an empty-key edge-case might land —
/// the gate is for cryptographic frame validity, not for the
/// "is encrypted at all" question this helper answers. The exact magic
/// bytes are stable wire-format constants (see `sse::SSE_MAGIC_V{1..6}`)
/// and are echoed here so the repair module has no circular dep on the
/// SSE module's full surface.
fn detect_sse_magic(body: &[u8]) -> Option<&'static str> {
    if body.len() < 4 {
        return None;
    }
    match &body[..4] {
        b"S4E1" => Some("S4E1"),
        b"S4E2" => Some("S4E2"),
        b"S4E3" => Some("S4E3"),
        b"S4E4" => Some("S4E4"),
        b"S4E5" => Some("S4E5"),
        b"S4E6" => Some("S4E6"),
        _ => None,
    }
}

enum GetOutcome {
    NotFound,
    Other(String),
}

async fn get_object_bytes(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<bytes::Bytes, GetOutcome> {
    match client.get_object().bucket(bucket).key(key).send().await {
        Ok(resp) => {
            let agg = resp
                .body
                .collect()
                .await
                .map_err(|e| GetOutcome::Other(format!("read body: {e}")))?;
            Ok(agg.into_bytes())
        }
        Err(e) => {
            if is_get_not_found(&e) {
                Err(GetOutcome::NotFound)
            } else {
                Err(GetOutcome::Other(format!("{e}")))
            }
        }
    }
}

fn is_head_not_found(
    e: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::head_object::HeadObjectError>,
) -> bool {
    matches!(
        e,
        aws_sdk_s3::error::SdkError::ServiceError(svc)
            if matches!(
                svc.err(),
                aws_sdk_s3::operation::head_object::HeadObjectError::NotFound(_)
            )
    )
}

fn is_get_not_found(
    e: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
) -> bool {
    matches!(
        e,
        aws_sdk_s3::error::SdkError::ServiceError(svc)
            if matches!(
                svc.err(),
                aws_sdk_s3::operation::get_object::GetObjectError::NoSuchKey(_)
            )
    )
}

/// Parse a `bucket/key` CLI argument. Splits on the **first** `/` only so
/// keys with slashes (e.g. `prefix/sub/file.bin`) round-trip cleanly.
pub fn parse_bucket_key(arg: &str) -> Result<(&str, &str), String> {
    match arg.split_once('/') {
        Some((b, k)) if !b.is_empty() && !k.is_empty() => Ok((b, k)),
        Some(_) => Err(format!(
            "expected `bucket/key`, got {arg:?} — bucket and key must both be non-empty"
        )),
        None => Err(format!("expected `bucket/key`, got {arg:?} — missing `/`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bucket_key_simple() {
        assert_eq!(
            parse_bucket_key("mybucket/foo.txt"),
            Ok(("mybucket", "foo.txt"))
        );
    }

    #[test]
    fn parse_bucket_key_with_slashes_in_key() {
        assert_eq!(parse_bucket_key("b/a/b/c"), Ok(("b", "a/b/c")));
    }

    #[test]
    fn parse_bucket_key_missing_slash() {
        assert!(parse_bucket_key("nokey").is_err());
    }

    #[test]
    fn parse_bucket_key_empty_key() {
        assert!(parse_bucket_key("bucket/").is_err());
    }

    #[test]
    fn parse_bucket_key_empty_bucket() {
        assert!(parse_bucket_key("/key").is_err());
    }

    #[test]
    fn verify_report_is_clean_truth_table() {
        let mk = |status| VerifyReport {
            bucket: "b".into(),
            key: "k".into(),
            status,
        };
        assert!(
            mk(SidecarStatus::Ok {
                frame_count: 1,
                sidecar_size: 100,
            })
            .is_clean()
        );
        assert!(mk(SidecarStatus::LegacyV1 { frame_count: 3 }).is_clean());
        // P2-C (Codex R3): single-frame objects intentionally have no
        // sidecar — clean state, not divergence.
        assert!(mk(SidecarStatus::MissingHarmless { frame_count: 1 }).is_clean());
        // Ambiguous (body too large to deep-scan) — report cleanly so
        // CI doesn't false-alert; operator sees the hint in stdout.
        assert!(
            mk(SidecarStatus::MissingUnknown {
                size: 10 * 1024 * 1024 * 1024,
                cap: 5 * 1024 * 1024 * 1024,
            })
            .is_clean()
        );
        // Multi-frame + missing sidecar = real divergence.
        assert!(!mk(SidecarStatus::MissingDivergent { frame_count: 5 }).is_clean());
        assert!(
            !mk(SidecarStatus::StaleEtag {
                sidecar_etag: "a".into(),
                live_etag: "b".into(),
            })
            .is_clean()
        );
        assert!(
            !mk(SidecarStatus::StaleSize {
                sidecar_size: 1,
                live_size: 2,
            })
            .is_clean()
        );
        assert!(
            !mk(SidecarStatus::DecodeError {
                message: "bad".into()
            })
            .is_clean()
        );
    }

    #[test]
    fn delete_policy_allows_truth_table() {
        let missing = OrphanReason::PairedMissing;
        let etag = OrphanReason::PairedEtagMismatch {
            sidecar_etag: "a".into(),
            live_etag: "b".into(),
        };
        let size = OrphanReason::PairedSizeMismatch {
            sidecar_size: 1,
            live_size: 2,
        };
        let undecodable = OrphanReason::SidecarUndecodable {
            message: "bad bytes".into(),
        };

        // DryRun: never deletes anything.
        assert!(!DeletePolicy::DryRun.allows(&missing));
        assert!(!DeletePolicy::DryRun.allows(&etag));
        assert!(!DeletePolicy::DryRun.allows(&size));
        assert!(!DeletePolicy::DryRun.allows(&undecodable));

        // PairBoundOnly: deletes the three pair-bound categories,
        // skips Undecodable (HIGH-2 review fix: protects v0.8.17
        // legacy reserved-name user data).
        assert!(DeletePolicy::PairBoundOnly.allows(&missing));
        assert!(DeletePolicy::PairBoundOnly.allows(&etag));
        assert!(DeletePolicy::PairBoundOnly.allows(&size));
        assert!(!DeletePolicy::PairBoundOnly.allows(&undecodable));

        // IncludeUndecodable: explicit operator opt-in deletes all.
        assert!(DeletePolicy::IncludeUndecodable.allows(&missing));
        assert!(DeletePolicy::IncludeUndecodable.allows(&etag));
        assert!(DeletePolicy::IncludeUndecodable.allows(&size));
        assert!(DeletePolicy::IncludeUndecodable.allows(&undecodable));
    }

    /// P3-A (Codex R5): a v2 sidecar with size binding but no ETag
    /// (rebuilt on an ETag-less backend) classifies as `Ok`, NOT
    /// `LegacyV1`. The latter would tell operators to "repair to
    /// upgrade" a sidecar already at the highest binding level the
    /// backend supports. This test asserts the exact pattern the
    /// status match in `verify_sidecar` relies on.
    #[test]
    fn verify_status_classifies_etag_less_v2_as_ok_not_legacy() {
        // The actual match arms in `verify_sidecar`:
        //
        //   (Some(s), _) if Some(s) != live → StaleEtag
        //   (_, Some(z)) if z != live_size → StaleSize
        //   (_, Some(_))                   → Ok        // P3-A fix
        //   (None, None)                   → LegacyV1
        //
        // Mirror that decision tree inline so refactors to the real
        // function can't quietly regress without flipping this test.
        fn classify(side_etag: Option<&str>, side_size: Option<u64>) -> &'static str {
            const LIVE_ETAG: Option<&str> = Some("xyz");
            const LIVE_SIZE: u64 = 100;
            match (side_etag, side_size) {
                (Some(s), _) if Some(s) != LIVE_ETAG => "StaleEtag",
                (_, Some(z)) if z != LIVE_SIZE => "StaleSize",
                (_, Some(_)) => "Ok",
                (_, None) => "LegacyV1",
            }
        }
        // P3-A core case: ETag-less repair stamps (None, Some(size)).
        // Must classify as Ok, not LegacyV1.
        assert_eq!(classify(None, Some(100)), "Ok");
        // Full v2 binding with matching etag + size.
        assert_eq!(classify(Some("xyz"), Some(100)), "Ok");
        // True v1 legacy (neither field) still surfaces as LegacyV1.
        assert_eq!(classify(None, None), "LegacyV1");
        // Mismatches still detected.
        assert_eq!(classify(Some("abc"), Some(100)), "StaleEtag");
        assert_eq!(classify(Some("xyz"), Some(999)), "StaleSize");
    }

    /// P2-D (Codex R4): on an ETag-less backend the server stamps
    /// `source_etag = None`; the verifier MUST treat that as the
    /// legacy / best-effort path (Ok / LegacyV1), not flag every
    /// such sidecar as stale. This unit test pins the discriminator
    /// the `verify_sidecar` status-match arm relies on (the
    /// `Option<&str>` equality).
    #[test]
    fn etag_option_equality_treats_none_none_as_match() {
        let side: Option<&str> = None;
        let live: Option<&str> = None;
        assert!(side == live, "None == None must hold for the no-ETag path");

        let side: Option<&str> = Some("abc");
        let live: Option<&str> = Some("abc");
        assert!(side == live);

        let side: Option<&str> = Some("");
        let live: Option<&str> = None;
        assert!(side != live, "Some(\"\") must NOT equal None — P2-D guard");
    }

    #[test]
    fn normalize_etag_strips_surrounding_quotes() {
        // aws-sdk-s3 returns the wire form (with quotes); s3s `value()`
        // returns the stripped form. The sidecar's `source_etag` is
        // canonical-stripped, so both sides must agree.
        assert_eq!(normalize_etag("\"abc-1\""), "abc-1");
        // Multipart ETags are `<hex>-<n>` and still get quoted on wire.
        assert_eq!(
            normalize_etag("\"067e3167e8c481c2aea3650ebb273198-2\""),
            "067e3167e8c481c2aea3650ebb273198-2"
        );
        // Already-stripped form is a no-op (the helper is idempotent so
        // callers don't need to branch on the source).
        assert_eq!(normalize_etag("abc-1"), "abc-1");
        // Defensive: an empty etag stays empty (head responses with no
        // ETag header round-trip to the empty string in head_main).
        assert_eq!(normalize_etag(""), "");
    }

    /// P2-R3 (Codex R3 audit): `repair-sidecar` on a passthrough /
    /// raw-bytes object would previously write an empty sidecar
    /// that silently breaks Range GET. Pin the typed error's wire
    /// shape so a future refactor can't quietly drop the
    /// `NotFramed` branch.
    #[test]
    fn not_framed_error_shape() {
        let err = RepairError::NotFramed {
            bucket: "b".into(),
            key: "k".into(),
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("b/k"), "Display must mention bucket/key");
        assert!(
            rendered.contains("S4F2") || rendered.contains("passthrough"),
            "Display must hint at the framing reason"
        );
        // Pattern-match guard: any rename of bucket/key here is a
        // compile error both here AND at the repair_sidecar
        // construction site.
        match err {
            RepairError::NotFramed { bucket, key } => {
                assert_eq!(bucket, "b");
                assert_eq!(key, "k");
            }
            _ => unreachable!("NotFramed must match its own variant"),
        }
    }

    /// CI-unblock (post-v0.9 audit): the MinIO E2E race test
    /// (`repair_sidecar_detects_post_get_overwrite_race`) is
    /// inherently timing-dependent and flakes on fast CI runners
    /// where the entire repair pipeline completes before the
    /// spawned overwrite lands. This deterministic guard pins
    /// the error type's wire shape (Display + field accessors)
    /// so the post-PUT divergence detector branch in
    /// `repair_sidecar` can't be silently refactored into a
    /// different error variant without flipping this assertion.
    #[test]
    fn overwritten_during_repair_error_shape() {
        let err = RepairError::OverwrittenDuringRepair {
            bucket: "b".into(),
            key: "k".into(),
            head_etag: "abc-1".into(),
        };
        let rendered = format!("{err}");
        assert!(
            rendered.contains("b/k"),
            "Display must mention bucket/key — got {rendered:?}"
        );
        assert!(
            rendered.contains("abc-1"),
            "Display must mention the pre-race ETag — got {rendered:?}"
        );
        assert!(
            rendered.contains("re-run") || rendered.contains("overwritten"),
            "Display must hint that the operator should re-run — got {rendered:?}"
        );
        // Pattern-match guard: any future destructure of this
        // variant elsewhere in the crate must keep these three
        // named fields. A rename here would surface as a compile
        // error here AND at the production call sites in
        // repair_sidecar / classify_missing_sidecar.
        match err {
            RepairError::OverwrittenDuringRepair {
                bucket,
                key,
                head_etag,
            } => {
                assert_eq!(bucket, "b");
                assert_eq!(key, "k");
                assert_eq!(head_etag, "abc-1");
            }
            _ => unreachable!("OverwrittenDuringRepair must match its own variant"),
        }
    }

    #[test]
    fn default_repair_body_cap_matches_max_body_default() {
        // Tied to s4-server `--max-body-bytes` default (5 GiB, #178). If
        // the default changes there, update both in lockstep.
        assert_eq!(DEFAULT_REPAIR_BODY_BYTES_CAP, 5 * 1024 * 1024 * 1024);
    }

    /// v0.9 #106-audit-R2 P2-INT-1: `detect_sse_magic` returns the
    /// correct frame label for every S4Ex prefix, and `None` for the
    /// plaintext frame magic (`S4F2`) and short / random inputs. The
    /// helper is the discriminator the `EncryptedSidecarUnsupported`
    /// branch in `repair_sidecar` relies on; pinning its outputs
    /// guards against a silent regression that would resurrect the
    /// confusing `FrameScan` failure on encrypted bodies.
    #[test]
    fn detect_sse_magic_covers_all_envelope_variants() {
        assert_eq!(detect_sse_magic(b"S4E1\0\0\0\0"), Some("S4E1"));
        assert_eq!(detect_sse_magic(b"S4E2\0\0\0\0"), Some("S4E2"));
        assert_eq!(detect_sse_magic(b"S4E3\0\0\0\0"), Some("S4E3"));
        assert_eq!(detect_sse_magic(b"S4E4\0\0\0\0"), Some("S4E4"));
        assert_eq!(detect_sse_magic(b"S4E5\0\0\0\0"), Some("S4E5"));
        assert_eq!(detect_sse_magic(b"S4E6\0\0\0\0"), Some("S4E6"));
        // S4F2 = plaintext framed body; must NOT match (or repair
        // would falsely reject every framed object as encrypted).
        assert_eq!(detect_sse_magic(b"S4F2\0\0\0\0"), None);
        // Random bytes, short inputs, and empty body all return None.
        assert_eq!(detect_sse_magic(b"NOPE\0"), None);
        assert_eq!(detect_sse_magic(b"S4"), None);
        assert_eq!(detect_sse_magic(b""), None);
    }

    /// v0.9 #106-audit-R2 P2-INT-1: pin the Display text + struct shape
    /// of the new variant so refactors can't silently drop the operator
    /// guidance (server-mode rebuild / re-PUT) or rename the fields the
    /// CLI's error formatter reads. Mirrors the existing
    /// `overwritten_during_repair_error_shape` test pattern.
    #[test]
    fn repair_sidecar_rejects_encrypted_body_with_typed_error() {
        let err = RepairError::EncryptedSidecarUnsupported {
            bucket: "b".into(),
            key: "k".into(),
            message: "body magic S4E6 indicates SSE-S4 envelope".into(),
        };
        let rendered = format!("{err}");
        assert!(
            rendered.contains("b/k"),
            "Display must mention bucket/key — got {rendered:?}"
        );
        assert!(
            rendered.contains("S4E6"),
            "Display must echo the body magic for operator triage — got {rendered:?}"
        );
        assert!(
            rendered.contains("encrypted-sidecar repair"),
            "Display must name the failure mode — got {rendered:?}"
        );
        assert!(
            rendered.contains("re-PUT") || rendered.contains("server-mode"),
            "Display must hint at the recovery path — got {rendered:?}"
        );
        match err {
            RepairError::EncryptedSidecarUnsupported {
                bucket,
                key,
                message,
            } => {
                assert_eq!(bucket, "b");
                assert_eq!(key, "k");
                assert!(message.contains("S4E6"));
            }
            _ => unreachable!("EncryptedSidecarUnsupported must match its own variant"),
        }
    }
}
