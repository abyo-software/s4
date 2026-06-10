"""s4fs — fsspec adapter for S4 gateway-written objects (read-only).

Usage::

    import s4fs  # registers the "s4" protocol
    import pandas as pd

    df = pd.read_parquet(
        "s4://bucket/data.parquet",
        storage_options={"target_options": {"endpoint_url": "http://backend:9000"}},
    )
"""

from fsspec import register_implementation

from s4fs.core import S4File, S4FileSystem

__all__ = ["S4File", "S4FileSystem"]
__version__ = "1.0.0"

# Idempotent: the `fsspec.specs` entry point in pyproject.toml already
# advertises the implementation; registering here covers source checkouts
# and older fsspec versions. clobber=False keeps an operator override.
try:
    register_implementation("s4", S4FileSystem, clobber=False)
except ValueError:
    pass
