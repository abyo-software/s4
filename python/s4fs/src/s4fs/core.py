"""S4FileSystem — read S4 gateway-written objects directly from the backend.

The S4 gateway (https://github.com/abyo-software/s4) stores objects on an
S3-compatible backend as a sequence of S4F2 frames (compressed chunks with
per-frame CRC32C), optionally accompanied by a ``<key>.s4index`` sidecar
that maps decompressed byte ranges to compressed byte ranges.

This module lets fsspec-aware tools (pandas, pyarrow, DuckDB, Polars, dask)
read those objects **without the gateway**:

- objects are transparently decompressed on read;
- ``ls`` / ``info`` report the *original* (decompressed) size;
- range reads use the ``.s4index`` sidecar to fetch + decode only the
  frames that overlap the requested range;
- non-S4 objects (no S4F2 magic) pass through byte-for-byte.

The filesystem is **read-only**: writes must go through the S4 gateway,
which owns the framing / sidecar / metadata contract.

Wire-format references (frozen as of s4 v1.0):

- frame layout:   ``crates/s4-codec/src/multipart.rs`` (S4F2 / S4P1)
- sidecar layout: ``crates/s4-codec/src/index.rs``     (S4IX v1/v2/v3)
- metadata keys:  ``crates/s4-server/src/service.rs``  (``s4-codec``,
  ``s4-original-size``, ``s4-dict-id``, …)
- dictionaries:   ``crates/s4-server/src/dict.rs``     (``.s4dict/<id>``,
  id = first 16 hex chars of SHA-256 of the dictionary bytes)
"""

from __future__ import annotations

import bisect
import hashlib
import re
import warnings
from typing import Any, Dict, List, Optional, Tuple

import fsspec
import s4_codec
from fsspec import AbstractFileSystem
from fsspec.spec import AbstractBufferedFile

#: ``<key>.s4index`` sidecar suffix (mirrors ``s4_codec::index::SIDECAR_SUFFIX``).
SIDECAR_SUFFIX = s4_codec.SIDECAR_SUFFIX
#: Reserved bucket-root prefix where trained zstd dictionaries live.
DICT_KEY_PREFIX = ".s4dict/"
#: Reserved *infix* marking the gateway's versioning shadow objects.
#: Shadow keys are ``<key>.__s4ver__/<version-id>`` — the marker appears
#: after the user key, not at the bucket root (see
#: ``s4-server/src/service.rs`` ``version_shadow_key`` /
#: ``is_version_shadow_key``, which filters ``key.contains(".__s4ver__/")``).
VERSION_SHADOW_PREFIX = ".__s4ver__/"

_FRAME_MAGICS = (s4_codec.FRAME_MAGIC, s4_codec.PADDING_MAGIC)
#: SSE envelope magics — the first 4 bytes of every body the gateway
#: stored encrypted. Transcribed from ``crates/s4-server/src/sse.rs``
#: (``SSE_MAGIC_V1`` .. ``SSE_MAGIC_V6``): S4E1 (v0.4 single key),
#: S4E2 (keyring), S4E3 (SSE-C), S4E4 (SSE-KMS), S4E5/S4E6 (chunked).
_SSE_MAGICS = (b"S4E1", b"S4E2", b"S4E3", b"S4E4", b"S4E5", b"S4E6")
_DICT_ID_RE = re.compile(r"^[0-9a-f]{16}$")

_READ_ONLY_MSG = "s4fs is read-only; write through the S4 gateway"
_SSE_MSG = (
    "s4fs does not decrypt SSE objects; read through the gateway "
    "(the encryption keyring / KMS / SSE-C key never leaves the gateway)"
)
_GPU_FRAME_MSG = (
    "GPU-written frames (codec {codec!r}) require the gateway or the "
    "s4-codec CLI to decode; s4fs only decodes CPU codecs "
    "(passthrough / cpu-zstd / cpu-gzip / cpu-zstd-dict)"
)


def _is_framed(prefix: bytes) -> bool:
    """True if ``prefix`` starts with the S4F2 frame or S4P1 padding magic."""
    return len(prefix) >= 4 and prefix[:4] in _FRAME_MAGICS


def _is_sse(prefix: bytes) -> bool:
    """True if ``prefix`` starts with an S4E1..S4E6 SSE envelope magic."""
    return len(prefix) >= 4 and prefix[:4] in _SSE_MAGICS


def _strip_etag(etag: Optional[str]) -> Optional[str]:
    """Normalize an entity-tag for comparison (strip quotes / W/ prefix)."""
    if etag is None:
        return None
    etag = etag.strip()
    if etag.startswith("W/"):
        etag = etag[2:]
    return etag.strip('"')


def _normalize_range(start: Optional[int], end: Optional[int], size: int) -> Tuple[int, int]:
    """Resolve fsspec ``cat_file`` start/end semantics against ``size``."""
    if start is None:
        start = 0
    elif start < 0:
        start = max(0, size + start)
    if end is None:
        end = size
    elif end < 0:
        end = max(0, size + end)
    return min(start, size), min(end, size)


class S4FileSystem(AbstractFileSystem):
    """Read-only fsspec filesystem decoding S4 gateway-written objects.

    Parameters
    ----------
    fs:
        An already-constructed underlying fsspec filesystem (e.g. an
        ``s3fs.S3FileSystem`` pointed at the backend, or any stub in
        tests). Takes precedence over ``target_protocol``.
    target_protocol:
        Protocol of the underlying filesystem to construct when ``fs`` is
        not given. Defaults to ``"s3"`` (requires the ``s3fs`` extra).
    target_options:
        Keyword arguments for the underlying filesystem constructor
        (e.g. ``{"endpoint_url": "http://minio:9000"}``).
    allow_inexact_open:
        By default ``open()`` refuses S4-framed objects whose original
        size cannot be resolved exactly (no usable ``.s4index`` sidecar
        and no ``s4-original-size`` metadata): ``AbstractBufferedFile``
        clamps reads to ``info()["size"]``, which would be the
        *compressed* size — silently truncating the decompressed stream.
        Pass ``True`` to restore the pre-1.0.1 clamped behavior.

    Notes
    -----
    - ``ls`` / ``info`` return the *original* (decompressed) size for
      S4-framed objects. Resolution order: ``<key>.s4index`` sidecar →
      ``s4-original-size`` object metadata → compressed size with
      ``info()["s4_size_exact"] = False``.
    - Resolving exact sizes may cost one extra backend request per object
      (sidecar GET or metadata HEAD); results are cached per instance.
    - GPU-written frames (``nvcomp-*`` / ``dietgpu-ans``) raise
      ``NotImplementedError`` — never silently-wrong bytes.
    - SSE-encrypted objects (gateway keyring / SSE-KMS / SSE-C) raise
      ``NotImplementedError`` — the decryption keys live gateway-side
      and returning ciphertext would be silently-wrong bytes.
    """

    protocol = "s4"

    def __init__(
        self,
        fs: Optional[AbstractFileSystem] = None,
        target_protocol: str = "s3",
        target_options: Optional[Dict[str, Any]] = None,
        allow_inexact_open: bool = False,
        **kwargs: Any,
    ):
        super().__init__(**kwargs)
        self.allow_inexact_open = allow_inexact_open
        if fs is None:
            fs = fsspec.filesystem(target_protocol, **(target_options or {}))
        self.fs = fs
        # path -> decoded sidecar dict (or None when absent/undecodable)
        self._index_cache: Dict[str, Optional[Dict[str, Any]]] = {}
        # path -> lowercased user-metadata dict
        self._meta_cache: Dict[str, Dict[str, str]] = {}
        # (bucket, dict_id) -> dictionary bytes (fingerprint-verified)
        self._zstd_dict_cache: Dict[Tuple[str, str], bytes] = {}

    # -- internal helpers --------------------------------------------------

    def _hidden(self, path: str) -> bool:
        """S4-internal keys hidden from listings (sidecars / dicts / shadow
        versions), mirroring the gateway's listing filter."""
        if path.endswith(SIDECAR_SUFFIX):
            return True
        # Versioning shadow keys carry the marker as an *infix* after the
        # user key — ``<key>.__s4ver__/<version-id>`` (service.rs
        # ``is_version_shadow_key`` filters ``contains(".__s4ver__/")``).
        # Also hide the bare ``….__s4ver__`` virtual-directory entry that
        # fsspec synthesizes when listing the parent prefix.
        if VERSION_SHADOW_PREFIX in path or path.endswith(VERSION_SHADOW_PREFIX.rstrip("/")):
            return True
        parts = path.split("/", 1)
        if len(parts) == 2:
            key = parts[1]
            # Trained dictionaries live under a bucket-root prefix.
            if key == DICT_KEY_PREFIX.rstrip("/") or key.startswith(DICT_KEY_PREFIX):
                return True
        return False

    def _load_index(self, path: str) -> Optional[Dict[str, Any]]:
        """Fetch + decode the ``<key>.s4index`` sidecar, or None."""
        if path in self._index_cache:
            return self._index_cache[path]
        idx: Optional[Dict[str, Any]] = None
        try:
            raw = self.fs.cat_file(path + SIDECAR_SUFFIX)
            idx = s4_codec.decode_index(raw)
            if not idx["entries"]:
                idx = None
        except FileNotFoundError:
            idx = None
        except (s4_codec.S4IndexError, OSError) as exc:
            warnings.warn(
                f"s4fs: undecodable sidecar for {path!r} ({exc}); "
                "falling back to full-object reads",
                stacklevel=2,
            )
            idx = None
        self._index_cache[path] = idx
        return idx

    def _object_metadata(self, path: str) -> Dict[str, str]:
        """Lowercased user metadata of the underlying object (best effort).

        s3fs surfaces head_object's ``Metadata`` dict inside ``info()``;
        other filesystems may not carry metadata at all, in which case the
        caller falls back to less-exact behavior.
        """
        if path in self._meta_cache:
            return self._meta_cache[path]
        meta: Any = None
        try:
            info = self.fs.info(path)
            meta = info.get("Metadata") or info.get("metadata")
        except FileNotFoundError:
            meta = None
        if meta is None and hasattr(self.fs, "metadata"):
            try:
                meta = self.fs.metadata(path)
            except Exception:  # noqa: BLE001 — metadata is best-effort
                meta = None
        out = {str(k).lower(): str(v) for k, v in (meta or {}).items()}
        self._meta_cache[path] = out
        return out

    def _guard_sse(
        self,
        path: str,
        head: Optional[bytes] = None,
        idx: Optional[Dict[str, Any]] = None,
    ) -> None:
        """Refuse SSE-encrypted objects loudly — never return ciphertext.

        The gateway stamps ``s4-encrypted: aes-256-gcm`` metadata on every
        encrypted PUT and the stored body starts with an S4E1..S4E6
        envelope (``s4-server/src/sse.rs`` / ``service.rs``). s4fs has no
        access to the keyring / KMS / SSE-C key, so any byte it could
        return would be ciphertext. Three independent signals are checked
        (metadata may be unreachable on some filesystems, sidecars may be
        absent), any one of which triggers the refusal:

        - ``s4-encrypted`` object metadata;
        - the sidecar's v3 SSE chunk binding (``idx["sse"]``);
        - the S4E* envelope magic in the first body bytes (``head``).
        """
        if self._object_metadata(path).get("s4-encrypted"):
            raise NotImplementedError(_SSE_MSG)
        if idx is not None and idx.get("sse") is not None:
            raise NotImplementedError(_SSE_MSG)
        if head is not None and _is_sse(head):
            raise NotImplementedError(_SSE_MSG)

    def _resolve_size(self, path: str, base_info: Dict[str, Any]) -> Tuple[int, bool]:
        """Original (decompressed) size of ``path`` and whether it is exact.

        Order: sidecar entries (only when the sidecar's source binding
        matches the live object — see ``_sidecar_matches``) →
        ``s4-original-size`` metadata → for raw (non-framed) objects the
        backend size is already exact → otherwise the compressed size with
        ``exact=False``.
        """
        idx = self._load_index(path)
        if idx is not None and self._sidecar_matches(path, idx):
            return int(idx["total_original_size"]), True
        meta = self._object_metadata(path)
        orig = meta.get("s4-original-size")
        if orig is not None:
            try:
                return int(orig), True
            except ValueError:
                pass
        backend_size = int(base_info.get("size") or 0)
        try:
            head = self.fs.cat_file(path, start=0, end=4)
        except (OSError, ValueError):
            head = b""
        if not _is_framed(head):
            return backend_size, True  # raw object — transparent passthrough
        return backend_size, False

    def _zstd_dict(self, bucket: str, dict_id: str) -> bytes:
        """Fetch ``<bucket>/.s4dict/<dict_id>`` and verify its fingerprint
        (first 16 hex chars of SHA-256 — see s4-server/src/dict.rs)."""
        if not _DICT_ID_RE.match(dict_id):
            raise ValueError(f"invalid s4-dict-id {dict_id!r} (expected 16 lowercase hex chars)")
        cache_key = (bucket, dict_id)
        cached = self._zstd_dict_cache.get(cache_key)
        if cached is not None:
            return cached
        data = self.fs.cat_file(f"{bucket}/{DICT_KEY_PREFIX}{dict_id}")
        actual = hashlib.sha256(data).hexdigest()[: len(dict_id)]
        if actual != dict_id:
            raise ValueError(
                f"dictionary fingerprint mismatch for {DICT_KEY_PREFIX}{dict_id}: "
                f"object hashes to {actual} (corrupted / tampered dictionary?)"
            )
        self._zstd_dict_cache[cache_key] = data
        return data

    def _decode_payload(self, header: Dict[str, Any], payload: bytes, path: str) -> bytes:
        """Decompress one frame payload according to its header."""
        codec = header["codec"]
        orig = header["original_size"]
        crc = header["crc32c"]
        if codec == "passthrough":
            if len(payload) != orig:
                raise s4_codec.S4Error(
                    f"passthrough frame size mismatch: header says {orig}, got {len(payload)}"
                )
            if s4_codec.crc32c(payload) != crc:
                raise s4_codec.S4Error("passthrough frame crc32c mismatch")
            return payload
        if codec == "cpu-zstd":
            return s4_codec.CpuZstd().decompress(payload, orig, crc)
        if codec == "cpu-gzip":
            return s4_codec.CpuGzip().decompress(payload, orig, crc)
        if codec == "cpu-zstd-dict":
            dict_id = self._object_metadata(path).get("s4-dict-id")
            if not dict_id:
                raise ValueError(
                    f"{path!r} carries a cpu-zstd-dict frame but no s4-dict-id "
                    "metadata; cannot resolve the dictionary"
                )
            bucket = path.split("/", 1)[0]
            dictionary = self._zstd_dict(bucket, dict_id)
            return s4_codec.CpuZstdDict(dictionary).decompress(payload, orig, crc)
        # nvcomp-* / dietgpu-ans / future codecs: refuse loudly, never
        # return silently-wrong bytes.
        raise NotImplementedError(_GPU_FRAME_MSG.format(codec=codec))

    def _decode_framed(self, data: bytes, path: str) -> bytes:
        """Decode a full S4F2-framed body (padding frames are skipped)."""
        return b"".join(
            self._decode_payload(header, payload, path)
            for header, payload in s4_codec.frame_iter(data)
        )

    def _metadata_codec(self, path: str) -> Optional[str]:
        """``s4-codec`` metadata for objects the gateway stored *without*
        S4F2 framing (``cpu-gzip``, ``passthrough``, legacy v0.1 raw zstd,
        non-framable GPU codecs). Returns None for plain objects."""
        return self._object_metadata(path).get("s4-codec")

    def _decode_unframed(self, data: bytes, path: str) -> bytes:
        """Decode a non-framed gateway object using the manifest stamped in
        object metadata (``s4-codec`` / ``s4-original-size`` / ``s4-crc32c``
        — see ``s4-server/src/service.rs``)."""
        # SSE bodies are never S4F2-framed (the envelope wraps the
        # compressed body), so they land here — refuse before the
        # codec dispatch can fall through to the raw-passthrough branch.
        self._guard_sse(path, head=data)
        meta = self._object_metadata(path)
        codec = meta.get("s4-codec")
        if codec is None or codec == "passthrough":
            return data
        try:
            orig = int(meta["s4-original-size"])
            crc = int(meta["s4-crc32c"])
        except (KeyError, ValueError) as exc:
            raise ValueError(
                f"{path!r} is stamped s4-codec={codec!r} but its manifest "
                f"metadata is missing/invalid ({exc}); refusing to guess"
            ) from None
        header = {"codec": codec, "original_size": orig, "crc32c": crc}
        return self._decode_payload(header, data, path)

    def _sidecar_matches(self, path: str, idx: Dict[str, Any]) -> bool:
        """Staleness / binding check: compare the sidecar's source ETag /
        compressed size against the live backend object. A mismatch means
        the object was overwritten after the sidecar was written — fall
        back to a full read instead of fetching wrong byte ranges.

        Legacy v1 sidecars carry *no* source binding at all (both
        ``source_etag`` and ``source_compressed_size`` are None — see
        ``s4-codec/src/index.rs`` v0.8.4 #73 H-2): they cannot be tied to
        the live object, so they are never trusted to drive range reads
        or exact sizes."""
        etag = _strip_etag(idx.get("source_etag"))
        scs = idx.get("source_compressed_size")
        if etag is None and scs is None:
            return False  # unbound v1 sidecar — full-read fallback
        try:
            info = self.fs.info(path)
        except FileNotFoundError:
            return False
        live_etag = _strip_etag(info.get("ETag") or info.get("etag"))
        if etag and live_etag and etag != live_etag:
            return False
        live_size = info.get("size")
        if scs is not None and live_size is not None and int(scs) != int(live_size):
            return False
        return True

    def _range_via_index(self, path: str, idx: Dict[str, Any], start: int, end: int) -> bytes:
        """Partial fetch: read only the compressed window covering frames
        overlapping ``[start, end)`` of the decompressed stream, decode
        those frames, and slice. Mirrors ``FrameIndex::lookup_range``."""
        # A v3 sidecar with an SSE chunk binding describes *pre-encrypt*
        # offsets of an encrypted body — refuse before fetching ciphertext.
        self._guard_sse(path, idx=idx)
        entries: List[Dict[str, Any]] = idx["entries"]
        offsets = [e["original_offset"] for e in entries]
        first = max(0, bisect.bisect_right(offsets, start) - 1)
        if entries[first]["original_offset"] + entries[first]["original_size"] <= start:
            first += 1  # defensive: gap in original space (not gateway-written)
        last = max(0, bisect.bisect_right(offsets, end - 1) - 1)
        byte_start = entries[first]["compressed_offset"]
        byte_end = entries[last]["compressed_offset"] + entries[last]["compressed_size"]
        window = self.fs.cat_file(path, start=byte_start, end=byte_end)
        combined = self._decode_framed(window, path)
        offset = start - entries[first]["original_offset"]
        return combined[offset : offset + (end - start)]

    # -- read API ----------------------------------------------------------

    def ls(self, path: str, detail: bool = False, **kwargs: Any) -> List[Any]:
        path = self._strip_protocol(path)
        raw = self.fs.ls(path, detail=True, **kwargs)
        out = []
        for entry in raw:
            name = entry.get("name", "")
            if self._hidden(name):
                continue
            entry = dict(entry)
            if entry.get("type") == "file":
                size, exact = self._resolve_size(name, entry)
                entry["size"] = size
                entry["s4_size_exact"] = exact
            out.append(entry)
        return out if detail else [e["name"] for e in out]

    def info(self, path: str, **kwargs: Any) -> Dict[str, Any]:
        path = self._strip_protocol(path)
        if self._hidden(path):
            raise FileNotFoundError(path)
        base = dict(self.fs.info(path, **kwargs))
        base["name"] = path
        if base.get("type") == "file":
            size, exact = self._resolve_size(path, base)
            base["size"] = size
            base["s4_size_exact"] = exact
        return base

    def exists(self, path: str, **kwargs: Any) -> bool:
        path = self._strip_protocol(path)
        if self._hidden(path):
            return False
        return self.fs.exists(path, **kwargs)

    def cat_file(
        self,
        path: str,
        start: Optional[int] = None,
        end: Optional[int] = None,
        **kwargs: Any,
    ) -> bytes:
        path = self._strip_protocol(path)
        # SSE-encrypted objects are refused up front (metadata check;
        # the magic-sniff / sidecar-binding fallbacks below catch the
        # metadata-unreachable case) — never return ciphertext.
        self._guard_sse(path)
        if start is None and end is None:
            data = self.fs.cat_file(path)
            if _is_framed(data):
                return self._decode_framed(data, path)
            return self._decode_unframed(data, path)

        # Range read. Prefer the sidecar partial-fetch path.
        idx = self._load_index(path)
        if idx is not None and self._sidecar_matches(path, idx):
            total = int(idx["total_original_size"])
            s, e = _normalize_range(start, end, total)
            if s >= e:
                # Still refuse empty-range reads of SSE objects loudly.
                self._guard_sse(path, idx=idx)
                return b""
            return self._range_via_index(path, idx, s, e)

        # No usable sidecar — sniff the first bytes to tell raw from framed.
        head = self.fs.cat_file(path, start=0, end=4)
        self._guard_sse(path, head=head)
        if not _is_framed(head):
            if self._metadata_codec(path) in (None, "passthrough"):
                # Plain object: delegate the range read untouched.
                return self.fs.cat_file(path, start=start, end=end)
            # Unframed compressed object (cpu-gzip / legacy raw zstd):
            # the whole stream is one compression unit — full read + slice.
            data = self._decode_unframed(self.fs.cat_file(path), path)
            s, e = _normalize_range(start, end, len(data))
            return data[s:e]

        # Framed without a usable sidecar: decode the whole object, then
        # slice. Warn only for multi-frame bodies — a single frame is one
        # compression unit and the full fetch is already minimal.
        frames = s4_codec.frame_iter(self.fs.cat_file(path))
        if len(frames) > 1:
            warnings.warn(
                f"s4fs: range read on multi-frame object {path!r} without a "
                "usable .s4index sidecar — fell back to a full-object read",
                stacklevel=2,
            )
        data = b"".join(self._decode_payload(h, p, path) for h, p in frames)
        s, e = _normalize_range(start, end, len(data))
        return data[s:e]

    def _open(
        self,
        path: str,
        mode: str = "rb",
        block_size: Optional[int] = None,
        autocommit: bool = True,
        cache_options: Optional[Dict[str, Any]] = None,
        **kwargs: Any,
    ) -> "S4File":
        if mode != "rb":
            raise NotImplementedError(_READ_ONLY_MSG)
        # AbstractBufferedFile clamps every read to info()["size"]. For a
        # framed object whose original size could not be resolved exactly
        # (no usable sidecar, no s4-original-size metadata) that size is
        # the *compressed* size — reads would silently stop short of the
        # decompressed stream's tail. Refuse instead of truncating.
        if not self.allow_inexact_open:
            info = self.info(path)
            if info.get("type") == "file" and not info.get("s4_size_exact", True):
                raise ValueError(
                    f"cannot open() {path!r}: it is S4-framed but its original "
                    "(decompressed) size is inexact (no usable .s4index sidecar "
                    "and no s4-original-size metadata), so buffered reads would "
                    "be silently truncated to the compressed size. Use "
                    "cat_file() (full decode), fetch through the S4 gateway, or "
                    "ensure s3 metadata access; or pass "
                    "S4FileSystem(allow_inexact_open=True) to accept truncation."
                )
        return S4File(
            self,
            path,
            mode=mode,
            block_size=block_size,
            autocommit=autocommit,
            cache_options=cache_options,
            **kwargs,
        )

    def created(self, path: str):
        return self.fs.created(self._strip_protocol(path))

    def modified(self, path: str):
        return self.fs.modified(self._strip_protocol(path))

    def invalidate_cache(self, path: Optional[str] = None) -> None:
        super().invalidate_cache(path)
        if path is None:
            self._index_cache.clear()
            self._meta_cache.clear()
        else:
            path = self._strip_protocol(path)
            self._index_cache.pop(path, None)
            self._meta_cache.pop(path, None)
        if hasattr(self.fs, "invalidate_cache"):
            self.fs.invalidate_cache(path)

    # -- write API: intentionally unsupported --------------------------------

    def pipe_file(self, path: str, value: bytes, **kwargs: Any) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def put_file(self, lpath: str, rpath: str, **kwargs: Any) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def cp_file(self, path1: str, path2: str, **kwargs: Any) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def mv(self, path1: str, path2: str, **kwargs: Any) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def rm_file(self, path: str) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def _rm(self, path: str) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def rm(self, path: str, recursive: bool = False, maxdepth: Optional[int] = None) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def touch(self, path: str, truncate: bool = True, **kwargs: Any) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def mkdir(self, path: str, create_parents: bool = True, **kwargs: Any) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def makedirs(self, path: str, exist_ok: bool = False) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)

    def rmdir(self, path: str) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)


class S4File(AbstractBufferedFile):
    """Read-only file handle; range fetches decode through the parent fs."""

    def _fetch_range(self, start: int, end: int) -> bytes:
        return self.fs.cat_file(self.path, start=start, end=end)

    def _upload_chunk(self, final: bool = False) -> bool:
        raise NotImplementedError(_READ_ONLY_MSG)

    def _initiate_upload(self) -> None:
        raise NotImplementedError(_READ_ONLY_MSG)
