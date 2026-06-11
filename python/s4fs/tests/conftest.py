"""Shared test scaffolding: a dict-backed stub filesystem and loaders for
the gateway-captured fixtures (see ``fixtures/generate_fixtures.py``).

The fixture bytes were written by the *real* S4 gateway / ``s4 train-dict``
against MinIO and captured straight off the backend — unit tests never
hand-assemble wire bytes (beyond the one synthesized nvcomp frame header
explicitly allowed for the GPU-codec refusal test).
"""

from __future__ import annotations

import json
import pathlib
from typing import Any, Dict, List, Optional

import pytest
from fsspec import AbstractFileSystem

FIXTURES = pathlib.Path(__file__).resolve().parent / "fixtures"
BUCKET = "s4fs-fixtures"


class StubFileSystem(AbstractFileSystem):
    """Minimal in-memory backend standing in for s3fs in unit tests.

    Tracks how many payload bytes each ``cat_file`` call transferred so
    tests can assert that sidecar-driven range reads fetch less than the
    full object.

    Writes mimic the slice of the s3fs surface s4fs needs: ``pipe_file``
    accepts a metadata dict (declared via ``s4fs_metadata_pipe_kwarg``,
    the s4fs capability hook), stores it alongside the body, and assigns
    an MD5 ETag the way S3/MinIO do for single PUTs.
    """

    protocol = "stub"
    cachable = False
    #: s4fs capability hook: pipe_file(..., metadata={...}) stamps S3
    #: user metadata (the s3fs equivalent is ``Metadata=``).
    s4fs_metadata_pipe_kwarg = "metadata"

    def __init__(
        self,
        files: Dict[str, bytes],
        metadata: Optional[Dict[str, Dict[str, str]]] = None,
        etags: Optional[Dict[str, str]] = None,
        **kwargs: Any,
    ):
        super().__init__(**kwargs)
        self.files = dict(files)
        self.meta = metadata or {}
        self.etags = etags or {}
        self.bytes_fetched = 0
        self.calls: List[tuple] = []

    def ls(self, path: str, detail: bool = False, **kwargs: Any) -> List[Any]:
        path = self._strip_protocol(path).rstrip("/")
        prefix = path + "/" if path else ""
        seen: Dict[str, Dict[str, Any]] = {}
        for name, data in self.files.items():
            if not name.startswith(prefix):
                continue
            rest = name[len(prefix) :]
            if "/" in rest:  # virtual directory
                dirname = prefix + rest.split("/", 1)[0]
                seen.setdefault(
                    dirname, {"name": dirname, "size": 0, "type": "directory"}
                )
            else:
                seen[name] = self._file_entry(name, data)
        if not seen and path in self.files:
            seen[path] = self._file_entry(path, self.files[path])
        if not seen:
            raise FileNotFoundError(path)
        entries = sorted(seen.values(), key=lambda e: e["name"])
        return entries if detail else [e["name"] for e in entries]

    def _file_entry(self, name: str, data: bytes) -> Dict[str, Any]:
        entry: Dict[str, Any] = {"name": name, "size": len(data), "type": "file"}
        if name in self.meta:
            entry["Metadata"] = dict(self.meta[name])
        if name in self.etags:
            entry["ETag"] = self.etags[name]
        return entry

    def info(self, path: str, **kwargs: Any) -> Dict[str, Any]:
        path = self._strip_protocol(path)
        if path in self.files:
            return self._file_entry(path, self.files[path])
        prefix = path.rstrip("/") + "/"
        if any(n.startswith(prefix) for n in self.files):
            return {"name": path.rstrip("/"), "size": 0, "type": "directory"}
        raise FileNotFoundError(path)

    def cat_file(
        self, path: str, start: Optional[int] = None, end: Optional[int] = None, **kwargs: Any
    ) -> bytes:
        path = self._strip_protocol(path)
        if path not in self.files:
            raise FileNotFoundError(path)
        data = self.files[path]
        if start is None and end is None:
            out = data
        else:
            s = 0 if start is None else (start if start >= 0 else max(0, len(data) + start))
            e = len(data) if end is None else (end if end >= 0 else max(0, len(data) + end))
            out = data[s:e]
        self.bytes_fetched += len(out)
        self.calls.append((path, start, end))
        return out

    def exists(self, path: str, **kwargs: Any) -> bool:
        path = self._strip_protocol(path)
        if path in self.files:
            return True
        prefix = path.rstrip("/") + "/"
        return any(n.startswith(prefix) for n in self.files)

    def pipe_file(
        self,
        path: str,
        value: bytes,
        metadata: Optional[Dict[str, str]] = None,
        **kwargs: Any,
    ) -> Dict[str, Any]:
        import hashlib

        path = self._strip_protocol(path)
        self.files[path] = bytes(value)
        if metadata is not None:
            self.meta[path] = dict(metadata)
        else:
            self.meta.pop(path, None)
        # Single-PUT S3 semantics: ETag is the quoted body MD5.
        etag = f'"{hashlib.md5(bytes(value)).hexdigest()}"'
        self.etags[path] = etag
        self.calls.append(("pipe_file", path, metadata))
        return {"ETag": etag}

    def rm_file(self, path: str) -> None:
        path = self._strip_protocol(path)
        self.files.pop(path, None)
        self.meta.pop(path, None)
        self.etags.pop(path, None)


class NoMetadataStubFileSystem(StubFileSystem):
    """Stub WITHOUT the metadata capability hook — pipe_file silently drops
    metadata, like fsspec's memory:// would. s4fs must refuse to write
    through it (unstamped-framed hazard)."""

    s4fs_metadata_pipe_kwarg = None

    def pipe_file(self, path: str, value: bytes, **kwargs: Any) -> None:  # type: ignore[override]
        path = self._strip_protocol(path)
        self.files[path] = bytes(value)


def load_fixture(name: str) -> Dict[str, Any]:
    """Load one captured fixture: body / metadata / optional sidecar / orig."""
    out: Dict[str, Any] = {"body": (FIXTURES / f"{name}.body").read_bytes()}
    meta_path = FIXTURES / f"{name}.meta.json"
    out["meta"] = json.loads(meta_path.read_text()) if meta_path.exists() else {}
    sidecar_path = FIXTURES / f"{name}.s4index"
    out["sidecar"] = sidecar_path.read_bytes() if sidecar_path.exists() else None
    orig_path = FIXTURES / f"{name}.orig"
    out["orig"] = orig_path.read_bytes() if orig_path.exists() else None
    return out


def stub_with(*specs: tuple) -> StubFileSystem:
    """Build a stub backend from (key, fixture-name) pairs, wiring up the
    sidecar object and per-object metadata exactly as MinIO stores them."""
    files: Dict[str, bytes] = {}
    metadata: Dict[str, Dict[str, str]] = {}
    for key, name in specs:
        fx = load_fixture(name)
        path = f"{BUCKET}/{key}"
        files[path] = fx["body"]
        if fx["meta"]:
            metadata[path] = fx["meta"]
        if fx["sidecar"] is not None:
            files[path + ".s4index"] = fx["sidecar"]
    dict_blob = FIXTURES / "zstd_dict.bin"
    if dict_blob.exists():
        import hashlib

        dict_id = hashlib.sha256(dict_blob.read_bytes()).hexdigest()[:16]
        files[f"{BUCKET}/.s4dict/{dict_id}"] = dict_blob.read_bytes()
    return StubFileSystem(files, metadata=metadata)


@pytest.fixture
def s4fs_factory():
    from s4fs import S4FileSystem

    def make(*specs: tuple):
        stub = stub_with(*specs)
        return S4FileSystem(fs=stub), stub

    return make
