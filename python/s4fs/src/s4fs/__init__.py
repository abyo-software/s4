"""s4fs — fsspec adapter for S4 gateway-format objects.

Reads work out of the box; writes are opt-in via ``write_enabled=True``
(client-side S4 encode, fully gateway-compatible). Usage::

    import s4fs  # registers the "s4" protocol
    import pandas as pd

    df = pd.read_parquet(
        "s4://bucket/data.parquet",
        storage_options={"target_options": {"endpoint_url": "http://backend:9000"}},
    )

    df.to_parquet(
        "s4://bucket/data.parquet",
        storage_options={
            "write_enabled": True,
            "target_options": {"endpoint_url": "http://backend:9000"},
        },
    )
"""

from fsspec import register_implementation

from s4fs.core import (
    S4File,
    S4FileSystem,
    S4MetadataUnsupportedError,
    S4SidecarWriteError,
)

__all__ = [
    "S4File",
    "S4FileSystem",
    "S4MetadataUnsupportedError",
    "S4SidecarWriteError",
]

# Single source of truth is pyproject.toml; a hardcoded string here
# shipped 1.1.0 wheels reporting __version__ == "1.0.0" (post-release
# audit catch), so resolve it from the installed dist metadata instead.
try:
    from importlib.metadata import PackageNotFoundError, version

    __version__ = version("s4fs")
except PackageNotFoundError:  # source checkout without an install
    __version__ = "0.0.0+unknown"

# Idempotent: the `fsspec.specs` entry point in pyproject.toml already
# advertises the implementation; registering here covers source checkouts
# and older fsspec versions. clobber=False keeps an operator override.
try:
    register_implementation("s4", S4FileSystem, clobber=False)
except ValueError:
    pass
