"""Supabase PostgREST adapter — live `SyncClient` implementation.

Slice 2 of #102. Targets the auto-generated REST surface that Supabase
exposes on top of Postgres:

    GET    {url}/rest/v1/tracks?select=*&updated_at_micros=gte.{since}
    GET    {url}/rest/v1/tracks?track_id=eq.{id}&select=*
    POST   {url}/rest/v1/tracks   header: Prefer: resolution=merge-duplicates
    DELETE {url}/rest/v1/tracks?track_id=eq.{id}

Auth header on every request:

    apikey:        {anon_key}
    Authorization: Bearer {anon_key}

Stdlib `urllib.request` keeps this dependency-free; aiohttp is already
pulled in by the parent package but blocking calls here run on the
syncer's own thread so the simpler sync API is the right pick.

The schema lives in `migrations/001_tracks.sql` — see module docstring
in `copilot/cloud_sync/__init__.py` for the column rationale.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from typing import Iterable

from .client import RemoteTrack, SyncError

DEFAULT_TIMEOUT_S = 10.0
TABLE = "tracks"


class SupabaseSyncClient:
    """PostgREST adapter for the Supabase-hosted `tracks` table.

    Construct via `from_env()` in production — reads
    `SUPABASE_URL` + `SUPABASE_ANON_KEY` (both required). Bare-keyword
    constructor lets unit tests point at a mock server.
    """

    def __init__(
        self,
        *,
        url: str,
        anon_key: str,
        timeout_s: float = DEFAULT_TIMEOUT_S,
    ) -> None:
        if not url:
            raise SyncError("SupabaseSyncClient: empty url")
        if not anon_key:
            raise SyncError("SupabaseSyncClient: empty anon_key")
        self._base = url.rstrip("/")
        self._key = anon_key
        self._timeout = timeout_s

    @classmethod
    def from_env(
        cls,
        *,
        url_var: str = "SUPABASE_URL",
        key_var: str = "SUPABASE_ANON_KEY",
        timeout_s: float = DEFAULT_TIMEOUT_S,
    ) -> "SupabaseSyncClient":
        """Read URL + anon key from process env.

        Raises `SyncError` when either var is missing or empty. The
        copilot service catches this at startup and falls back to the
        in-memory client — local-only mode — so missing creds never
        crash the engine.
        """
        url = (os.environ.get(url_var) or "").strip()
        key = (os.environ.get(key_var) or "").strip()
        return cls(url=url, anon_key=key, timeout_s=timeout_s)

    # ------------------------------------------------------------------
    # SyncClient surface
    # ------------------------------------------------------------------

    def list_tracks(self, *, since_micros: int = 0) -> list[RemoteTrack]:
        # PostgREST filter syntax: `column=op.value`. `gte` covers the
        # "newer than the watermark" case; `since_micros=0` is the full
        # pull because every row has `updated_at_micros >= 0`.
        params = {
            "select": "*",
            "updated_at_micros": f"gte.{int(since_micros)}",
        }
        body = self._get(f"/rest/v1/{TABLE}", params)
        if not isinstance(body, list):
            raise SyncError(f"list_tracks: expected JSON array, got {type(body).__name__}")
        return [_remote_track_from_row(row) for row in body]

    def get_track(self, track_id: str) -> RemoteTrack | None:
        params = {
            "select": "*",
            "track_id": f"eq.{_quote(track_id)}",
            "limit": "1",
        }
        body = self._get(f"/rest/v1/{TABLE}", params)
        if not isinstance(body, list) or not body:
            return None
        return _remote_track_from_row(body[0])

    def upsert_track(self, track: RemoteTrack) -> None:
        # PostgREST upsert: POST with Prefer: resolution=merge-duplicates
        # + on_conflict pointing at the primary key. The matching SQL
        # migration declares the `updated_at_micros > excluded`
        # last-write-wins guard at the database level (see
        # migrations/001_tracks.sql).
        payload = [_row_from_remote_track(track)]
        self._post(
            f"/rest/v1/{TABLE}",
            payload,
            extra_headers={
                "Prefer": "resolution=merge-duplicates,return=minimal",
            },
            params={"on_conflict": "track_id"},
        )

    def delete_track(self, track_id: str) -> bool:
        params = {"track_id": f"eq.{_quote(track_id)}"}
        try:
            self._delete(f"/rest/v1/{TABLE}", params)
        except SyncError as e:
            # Treat "row not found" as `False` instead of an error.
            if "404" in str(e):
                return False
            raise
        return True

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _headers(self) -> dict[str, str]:
        return {
            "apikey": self._key,
            "Authorization": f"Bearer {self._key}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        }

    def _url(self, path: str, params: dict[str, str] | None = None) -> str:
        q = ""
        if params:
            q = "?" + urllib.parse.urlencode(params, safe=".,()")
        return f"{self._base}{path}{q}"

    def _get(self, path: str, params: dict[str, str] | None = None):
        return self._request("GET", path, params=params)

    def _post(
        self,
        path: str,
        body: object,
        *,
        extra_headers: dict[str, str] | None = None,
        params: dict[str, str] | None = None,
    ):
        return self._request(
            "POST",
            path,
            params=params,
            body=body,
            extra_headers=extra_headers,
        )

    def _delete(self, path: str, params: dict[str, str] | None = None):
        return self._request("DELETE", path, params=params)

    def _request(
        self,
        method: str,
        path: str,
        *,
        params: dict[str, str] | None = None,
        body: object | None = None,
        extra_headers: dict[str, str] | None = None,
    ):
        url = self._url(path, params)
        headers = self._headers()
        if extra_headers:
            headers.update(extra_headers)
        data: bytes | None = None
        if body is not None:
            data = json.dumps(body).encode("utf-8")
        req = urllib.request.Request(
            url, data=data, headers=headers, method=method
        )
        try:
            with urllib.request.urlopen(req, timeout=self._timeout) as resp:
                raw = resp.read()
        except urllib.error.HTTPError as e:
            raise SyncError(
                f"supabase {method} {path} → {e.code} {e.reason}"
            ) from e
        except urllib.error.URLError as e:
            raise SyncError(f"supabase {method} {path}: {e.reason}") from e
        if not raw:
            return None
        try:
            return json.loads(raw)
        except json.JSONDecodeError as e:
            raise SyncError(
                f"supabase {method} {path}: non-JSON response"
            ) from e


# ---- Codec helpers ---------------------------------------------------


def _remote_track_from_row(row: dict) -> RemoteTrack:
    cues = row.get("hot_cues_ms") or []
    cues = list(cues) + [-1] * (8 - len(cues))
    return RemoteTrack(
        track_id=str(row["track_id"]),
        path=str(row.get("path", "")),
        bpm=float(row.get("bpm", 120.0)),
        camelot_key=str(row.get("camelot_key", "8B")),
        energy=float(row.get("energy", 0.5)),
        duration_s=float(row.get("duration_s", 0.0)),
        hot_cues_ms=tuple(int(v) for v in cues[:8]),
        updated_at_micros=int(row.get("updated_at_micros", 0)),
    )


def _row_from_remote_track(track: RemoteTrack) -> dict:
    return {
        "track_id": track.track_id,
        "path": track.path,
        "bpm": track.bpm,
        "camelot_key": track.camelot_key,
        "energy": track.energy,
        "duration_s": track.duration_s,
        "hot_cues_ms": list(track.hot_cues_ms),
        "updated_at_micros": int(track.updated_at_micros),
    }


def _quote(value: str) -> str:
    # PostgREST treats commas + parens as filter syntax. Quote so a
    # `track_id` containing them lands as a literal value.
    return urllib.parse.quote(value, safe="")


__all__ = ["SupabaseSyncClient"]
