"""Pluggable cache abstraction for HypeHouse.

Two backends:

- ``LocalCache(path)`` — wraps the existing ``cache/`` directory layout. Default
  for dev + when no GCS bucket is configured. Keys map directly to filenames so
  pre-existing dev caches keep working without migration.
- ``GcsCache(bucket, prefix)`` — multi-instance shared cache backed by Google
  Cloud Storage. Lazy-imports ``google-cloud-storage`` so the dependency stays
  optional (``requirements-cloud.txt`` only).

Selection happens at boot time via env vars:

    CACHE_BACKEND    one of {"local", "gcs"}.  Defaults to ``"local"``.
    CACHE_GCS_BUCKET required when CACHE_BACKEND=gcs.
    CACHE_GCS_PREFIX optional prefix inside the bucket. Defaults to ``"cache/"``.

This module exposes a single public construction helper, :func:`get_cache`, that
returns a configured :class:`Cache` based on the environment.

Design notes
------------

* The interface intentionally mirrors how callers actually use the cache today:
  raw bytes (audio files) and JSON dicts (analysis sidecars).  Existing local
  callers can keep using ``Path``-based reads — the abstraction is opt-in.
* ``GcsCache`` keeps a tiny in-process LRU for ``exists()`` so the hot path of
  the pipeline (which calls ``exists()`` repeatedly per track) doesn't hammer
  GCS during a single render.
* The LRU is intentionally tiny (~64 keys) and invalidated on every mutation
  so we never serve stale "missing" answers after a write.
"""
from __future__ import annotations

import json
import logging
import os
import threading
import uuid
from abc import ABC, abstractmethod
from collections import OrderedDict
from pathlib import Path

_log = logging.getLogger(__name__)

__all__ = [
    "Cache",
    "GcsCache",
    "LocalCache",
    "get_cache",
]


class Cache(ABC):
    """Abstract bytes + JSON KV store. Keys are arbitrary strings that
    backends are free to treat as filenames / object names."""

    @abstractmethod
    def get_bytes(self, key: str) -> bytes | None:
        """Return the bytes for ``key`` or ``None`` if missing."""

    @abstractmethod
    def put_bytes(self, key: str, data: bytes) -> None:
        """Write ``data`` under ``key``. Overwrites existing value."""

    @abstractmethod
    def get_json(self, key: str) -> dict | None:
        """Return parsed JSON for ``key`` or ``None`` if missing/unreadable."""

    @abstractmethod
    def put_json(self, key: str, obj: dict) -> None:
        """Serialize ``obj`` as JSON and write under ``key``."""

    @abstractmethod
    def exists(self, key: str) -> bool:
        """True if ``key`` is present in the cache."""

    @abstractmethod
    def delete(self, key: str) -> None:
        """Remove ``key`` if present. Idempotent — missing key is not an error."""


# ---------------------------------------------------------------------------
# LocalCache — wraps the existing on-disk cache/ directory
# ---------------------------------------------------------------------------


class LocalCache(Cache):
    """Disk-backed cache rooted at ``path``.

    Keys are used verbatim as filenames inside ``path`` — this keeps the new
    abstraction byte-compatible with the existing ``cache/<id>.wav`` /
    ``cache/<id>.analysis.json`` layout so a dev tree's pre-existing caches
    Just Work without migration.
    """

    def __init__(self, path: Path):
        self.path = Path(path)
        self.path.mkdir(parents=True, exist_ok=True)

    def _key_path(self, key: str) -> Path:
        # Reject path-traversal — caller bug if it ever fires.
        if "/" in key or "\\" in key or key in ("", ".", ".."):
            raise ValueError(f"LocalCache: unsafe key {key!r}")
        return self.path / key

    def get_bytes(self, key: str) -> bytes | None:
        p = self._key_path(key)
        if not p.exists():
            return None
        try:
            return p.read_bytes()
        except OSError:
            return None

    def put_bytes(self, key: str, data: bytes) -> None:
        p = self._key_path(key)
        # Same atomic write pattern as analyzer.py / mixer.py — tmp + os.replace.
        # Multi-writer safe; reader either sees old or new bytes, never torn.
        tmp = p.with_name(f"{p.name}.tmp.{os.getpid()}.{uuid.uuid4().hex[:8]}")
        try:
            tmp.write_bytes(data)
            os.replace(str(tmp), str(p))
        finally:
            if tmp.exists():
                try:
                    tmp.unlink()
                except OSError:
                    pass

    def get_json(self, key: str) -> dict | None:
        raw = self.get_bytes(key)
        if raw is None:
            return None
        try:
            return json.loads(raw.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            return None

    def put_json(self, key: str, obj: dict) -> None:
        self.put_bytes(key, json.dumps(obj).encode("utf-8"))

    def exists(self, key: str) -> bool:
        return self._key_path(key).exists()

    def delete(self, key: str) -> None:
        p = self._key_path(key)
        try:
            p.unlink()
        except FileNotFoundError:
            pass
        except OSError:
            # Best-effort — match cache_mgr.evict_lru semantics.
            pass


# ---------------------------------------------------------------------------
# GcsCache — Google Cloud Storage backend
# ---------------------------------------------------------------------------


def _import_storage():
    """Lazy import google.cloud.storage. Raises RuntimeError with a helpful
    hint if the dependency is missing — keeps it optional.

    Wrapped in a function (not a module-level try/except) so test code can
    monkeypatch ``sys.modules['google.cloud.storage'] = None`` to simulate
    a missing dependency on a machine where the lib is actually installed.
    """
    try:
        from google.cloud import storage  # type: ignore[import-not-found]
    except ImportError as exc:  # pragma: no cover — exercised via monkeypatch
        raise RuntimeError(
            "GcsCache requires google-cloud-storage. "
            "Install it via `pip install google-cloud-storage` "
            "(or use requirements-cloud.txt)."
        ) from exc
    if storage is None:
        # Test hook: monkeypatch set sys.modules['google.cloud.storage'] = None.
        raise RuntimeError(
            "GcsCache requires google-cloud-storage. "
            "Install it via `pip install google-cloud-storage` "
            "(or use requirements-cloud.txt)."
        )
    return storage


class _ExistsLru:
    """Tiny thread-safe LRU for exists() results.

    Bounded to ``maxsize`` entries; on overflow the oldest entry is evicted.
    Every mutation (``put_*`` / ``delete``) invalidates the matching key so a
    stale ``False`` is never served after a write completes.

    Pipeline.py calls exists() multiple times per track during preflight,
    so even a small cache cuts GCS round-trips dramatically inside one run.
    """

    def __init__(self, maxsize: int = 64):
        self._maxsize = maxsize
        self._data: OrderedDict[str, bool] = OrderedDict()
        self._lock = threading.Lock()

    def get(self, key: str) -> bool | None:
        with self._lock:
            if key in self._data:
                self._data.move_to_end(key)
                return self._data[key]
            return None

    def set(self, key: str, value: bool) -> None:
        with self._lock:
            self._data[key] = value
            self._data.move_to_end(key)
            while len(self._data) > self._maxsize:
                self._data.popitem(last=False)

    def invalidate(self, key: str) -> None:
        with self._lock:
            self._data.pop(key, None)

    def clear(self) -> None:
        with self._lock:
            self._data.clear()


class GcsCache(Cache):
    """Google Cloud Storage backend.

    All keys are stored as objects named ``prefix + key`` inside ``bucket``.
    JSON values get ``Content-Type: application/json``; raw bytes get
    ``application/octet-stream``.

    A small in-process LRU on top of ``exists()`` keeps repeated existence
    checks during a single pipeline run from hammering GCS.

    Notes
    -----
    * The ``google-cloud-storage`` client is lazy-imported so the dep stays
      optional. Construction raises :class:`RuntimeError` if the library is
      missing — see :func:`_import_storage`.
    * The client + bucket handles are created once per :class:`GcsCache`
      instance. Re-use the instance across the process where possible.
    """

    def __init__(
        self,
        bucket: str,
        prefix: str = "cache/",
        *,
        client=None,
        exists_cache_size: int = 64,
    ):
        if not bucket:
            raise ValueError("GcsCache requires a non-empty bucket name")
        self.bucket_name = bucket
        # Normalize: prefix should end with "/" iff non-empty.
        if prefix and not prefix.endswith("/"):
            prefix = prefix + "/"
        self.prefix = prefix
        if client is None:
            storage = _import_storage()
            client = storage.Client()
        self._client = client
        self._bucket = client.bucket(bucket)
        self._exists_cache = _ExistsLru(maxsize=exists_cache_size)

    def _blob(self, key: str):
        return self._bucket.blob(self.prefix + key)

    def get_bytes(self, key: str) -> bytes | None:
        blob = self._blob(key)
        try:
            data = blob.download_as_bytes()
        except Exception as exc:
            # Codex PR #289 P2: only treat NotFound (HTTP 404 / google
            # NotFound) as cache miss. Auth/transport errors must be
            # visible — otherwise every Forbidden / Unauthenticated /
            # network failure becomes silent recompute churn in prod.
            kind = type(exc).__name__
            if kind == "NotFound" or "404" in str(exc):
                self._exists_cache.set(key, False)
                return None
            _log.warning(
                "GcsCache.get_bytes(%r) failed: %s: %s — surfacing as cache miss "
                "but please investigate (likely auth/transport, not absent key)",
                key, kind, exc,
            )
            # Don't poison the exists-cache on transient errors.
            return None
        self._exists_cache.set(key, True)
        return data

    def put_bytes(self, key: str, data: bytes) -> None:
        blob = self._blob(key)
        blob.upload_from_string(data, content_type="application/octet-stream")
        self._exists_cache.set(key, True)

    def get_json(self, key: str) -> dict | None:
        raw = self.get_bytes(key)
        if raw is None:
            return None
        try:
            return json.loads(raw.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            return None

    def put_json(self, key: str, obj: dict) -> None:
        blob = self._blob(key)
        blob.upload_from_string(
            json.dumps(obj), content_type="application/json"
        )
        self._exists_cache.set(key, True)

    def exists(self, key: str) -> bool:
        cached = self._exists_cache.get(key)
        if cached is not None:
            return cached
        blob = self._blob(key)
        present = bool(blob.exists())
        self._exists_cache.set(key, present)
        return present

    def delete(self, key: str) -> None:
        blob = self._blob(key)
        try:
            blob.delete()
        except Exception:
            # Match LocalCache semantics — missing-key delete is a no-op.
            pass
        self._exists_cache.set(key, False)


# ---------------------------------------------------------------------------
# Factory — env-driven selection
# ---------------------------------------------------------------------------


def get_cache(local_path: Path | None = None) -> Cache:
    """Construct the configured cache backend.

    Selection rules:

    * ``CACHE_BACKEND=gcs``  →  :class:`GcsCache` using ``CACHE_GCS_BUCKET``
      and ``CACHE_GCS_PREFIX`` (default ``"cache/"``).
    * anything else (including unset)  →  :class:`LocalCache` rooted at
      ``local_path`` (or ``./cache`` if not given).

    Keeping the default at ``local`` means this PR adds the option without
    flipping prod behavior — operators opt in to GCS explicitly via env.
    """
    backend = (os.environ.get("CACHE_BACKEND") or "local").strip().lower()
    if backend == "gcs":
        bucket = os.environ.get("CACHE_GCS_BUCKET", "").strip()
        if not bucket:
            raise RuntimeError(
                "CACHE_BACKEND=gcs requires CACHE_GCS_BUCKET to be set"
            )
        prefix = os.environ.get("CACHE_GCS_PREFIX", "cache/")
        return GcsCache(bucket=bucket, prefix=prefix)
    # Default: local
    path = local_path if local_path is not None else Path("cache")
    return LocalCache(path)
