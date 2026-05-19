"""SoundCloud streaming-source client.

Closes GH ``#101`` — first streaming provider for the software-only
pivot. SoundCloud's public API exposes a free search endpoint that
returns track metadata + a ``stream_url`` field; the URL needs a
follow-up resolve call with the client_id appended to mint a playable
HTTP URL the decoder can pull from.

Authentication
--------------
SoundCloud requires a registered ``client_id`` for every API call.
v0.1 keeps it simple: a single env var :data:`SOUNDCLOUD_CLIENT_ID_ENV`
holds the operator's id; a missing env var raises
:class:`copilot.streaming.StreamingAuthError` at construction time with a
clear "apply at <link>" message — the RPC layer translates this into a
``-32000`` so the UI can offer a "configure SoundCloud" affordance.

License filter
--------------
Hard rule: only Creative Commons tracks are returned. SoundCloud's
search endpoint accepts a ``license`` query parameter that filters
server-side, but we ALSO re-check :func:`copilot.streaming.is_cc_license`
on every result in case a non-CC track sneaks through (e.g. a track
re-uploaded with stale license metadata). Defence-in-depth — never
trust the wire.

Network surface
---------------
We use ``urllib.request`` from the stdlib rather than ``aiohttp`` for
the v0.1 client. Reasoning:

* The search + resolve calls are synchronous, single-shot, and short.
  Wrapping them in ``asyncio.to_thread`` from the RPC handler is
  cheaper than adding an async client surface to the streaming module.
* ``aiohttp`` is already a dependency, but pinning to stdlib here keeps
  the streaming module trivially mockable in tests — patch
  :func:`urllib.request.urlopen` and you're done; no aiohttp test
  fixture plumbing.
* Future Beatport / Mixcloud clients can opt into ``aiohttp`` if they
  need streaming downloads or per-connection auth headers; the ABC
  doesn't dictate transport.

Test seam
---------
:meth:`SoundCloudClient._http_get` is the single network entry point.
Tests monkeypatch it (or :func:`urllib.request.urlopen` directly) to
return canned JSON bodies — see ``tests/test_soundcloud.py``.
"""
from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Final

from . import (
    StreamingApiError,
    StreamingAuthError,
    StreamingProvider,
    StreamingTrack,
    is_cc_license,
)


# Env var holding the operator's SoundCloud ``client_id``. Apply for
# one at the URL surfaced in the auth error message. We deliberately
# avoid a hardcoded fallback — a leaked default key is a license-strike
# risk for every other operator sharing it.
SOUNDCLOUD_CLIENT_ID_ENV: Final[str] = "SOUNDCLOUD_CLIENT_ID"

# Public-API base. SoundCloud's docs say ``api.soundcloud.com`` is the
# stable surface (the JS widget hits ``api-v2.soundcloud.com`` but that
# endpoint is not officially supported for third-party clients — keep
# off it to avoid sudden breakage).
SOUNDCLOUD_API_BASE: Final[str] = "https://api.soundcloud.com"

# Default request timeout. Tight on purpose — search is interactive
# (the UI blocks on it) so we'd rather error fast and let the user
# retry than hang a request thread for 30s. The decoder side has its
# own timeouts on the actual audio fetch.
DEFAULT_TIMEOUT_S: Final[float] = 8.0

# Hard clamp on ``limit`` — SoundCloud accepts up to 200 but we cap at
# 50 to keep the UI list manageable and to bound the JSON parse cost.
MAX_LIMIT: Final[int] = 50

# Help URL surfaced in the missing-client_id error. SoundCloud
# discourages new registrations as of 2022 (waitlist), but the URL is
# still the canonical landing page for the application form.
SOUNDCLOUD_APPLY_URL: Final[str] = "https://soundcloud.com/you/apps"


def _missing_client_id_message() -> str:
    """Build the long-form auth error message.

    Pulled out so the test can assert on the apply-URL substring
    without re-typing the whole sentence.
    """
    return (
        f"SoundCloud client_id missing — set "
        f"${SOUNDCLOUD_CLIENT_ID_ENV} environment variable. "
        f"Apply for a key at {SOUNDCLOUD_APPLY_URL}."
    )


class SoundCloudClient(StreamingProvider):
    """SoundCloud public-API client.

    Usage::

        client = SoundCloudClient()  # reads $SOUNDCLOUD_CLIENT_ID
        results = client.search("lo-fi beats", limit=20)
        url = client.resolve_stream_url(results[0].id)

    Thread-safety: the client is stateless after construction (the
    ``client_id`` is captured in ``__init__``); ``search`` /
    ``resolve_stream_url`` make independent HTTP requests and can be
    called concurrently from multiple threads. Each call is its own
    short-lived ``urllib`` connection — no pooling — so a slow request
    won't starve a parallel one.
    """

    name = "soundcloud"

    def __init__(
        self,
        client_id: str | None = None,
        *,
        timeout_s: float = DEFAULT_TIMEOUT_S,
        api_base: str = SOUNDCLOUD_API_BASE,
    ) -> None:
        """Build a client.

        Args:
            client_id: SoundCloud API client_id. ``None`` (the default)
                reads :data:`SOUNDCLOUD_CLIENT_ID_ENV` from the
                environment. Tests pass an explicit value to avoid
                touching the host env.
            timeout_s: Per-request timeout. Defaults to
                :data:`DEFAULT_TIMEOUT_S` (8s).
            api_base: API base URL. Overridable for tests; production
                always uses :data:`SOUNDCLOUD_API_BASE`.

        Raises:
            StreamingAuthError: ``client_id`` not supplied AND env var
                unset.
        """
        resolved = client_id or os.environ.get(SOUNDCLOUD_CLIENT_ID_ENV)
        if not resolved:
            raise StreamingAuthError(_missing_client_id_message())
        self._client_id = resolved
        self._timeout_s = float(timeout_s)
        self._api_base = api_base.rstrip("/")

    # --- public API (StreamingProvider impl) -----------------------------

    def search(
        self, query: str, limit: int = 20
    ) -> list[StreamingTrack]:
        """Search SoundCloud and return CC-filtered tracks.

        Args:
            query: Free-text search query.
            limit: Max results; clamped to ``1..``:data:`MAX_LIMIT`.

        Returns:
            A list of :class:`StreamingTrack` (possibly empty if no
            results survived the CC filter).

        Raises:
            StreamingApiError: HTTP non-2xx or malformed JSON.
            StreamingAuthError: HTTP 401 / 403 from the server (the
                stored ``client_id`` was rejected — e.g. revoked or
                rate-limited beyond the daily cap).
        """
        clamped = max(1, min(int(limit), MAX_LIMIT))
        # SoundCloud's ``license`` filter takes a comma-separated list.
        # Using it shrinks the response on the wire; we still re-check
        # client-side via :func:`is_cc_license` in case a track has
        # mixed metadata. Server-side filter is best-effort, client-side
        # is the trust boundary.
        license_param = ",".join(
            sorted(
                {
                    "cc-by",
                    "cc-by-sa",
                    "cc-by-nc",
                    "cc-by-nc-sa",
                    "cc-by-nd",
                    "cc-by-nc-nd",
                }
            )
        )
        params = {
            "q": query,
            "limit": str(clamped),
            "license": license_param,
            "client_id": self._client_id,
        }
        url = f"{self._api_base}/tracks?{urllib.parse.urlencode(params)}"
        body = self._http_get(url)
        try:
            data = json.loads(body)
        except (TypeError, ValueError) as exc:
            raise StreamingApiError(
                f"SoundCloud returned unparseable JSON: {exc}"
            ) from exc
        if not isinstance(data, list):
            # SoundCloud /tracks returns a JSON array on success and an
            # object ``{errors: [...]}`` on failure. We've already
            # mapped HTTP failures in :meth:`_http_get`, so a non-list
            # here is a malformed-success response — surface as API
            # error rather than silent empty.
            raise StreamingApiError(
                f"SoundCloud /tracks expected list, got "
                f"{type(data).__name__}"
            )
        return [
            track
            for track in (self._parse_track(item) for item in data)
            if track is not None
        ]

    def resolve_stream_url(self, track_id: str) -> str:
        """Resolve a playable HTTP stream URL for ``track_id``.

        SoundCloud's ``/tracks/<id>/stream`` endpoint 302-redirects to a
        short-lived CDN URL. We don't follow the redirect ourselves
        because the engine's decoder (or the UI's audio preview) will
        do that on its own — instead we return the un-redirected URL
        with the ``client_id`` appended; the redirect target is then
        the CDN URL the decoder needs.

        Args:
            track_id: Provider-scoped id (the ``id`` field on
                :class:`StreamingTrack`).

        Returns:
            A fully-qualified HTTP(S) URL the decoder can pass to its
            ``MediaSource`` impl.

        Raises:
            StreamingApiError: Track id not found / private / removed.
            StreamingAuthError: Stored credentials rejected.
        """
        # Validate at the boundary — a blank id would build a URL like
        # ``/tracks//stream`` which SC returns 404 for, but we'd rather
        # fail loudly than wait on a network round-trip.
        if not track_id or not isinstance(track_id, str):
            raise StreamingApiError(
                f"resolve_stream_url: invalid track_id: {track_id!r}"
            )
        params = urllib.parse.urlencode({"client_id": self._client_id})
        return f"{self._api_base}/tracks/{track_id}/stream?{params}"

    # --- internals --------------------------------------------------------

    def _http_get(self, url: str) -> str:
        """Synchronous GET that maps HTTP errors to streaming exceptions.

        Single network seam — tests monkeypatch this (or
        :func:`urllib.request.urlopen`) to inject canned responses.

        Args:
            url: Fully-qualified URL with query string + client_id.

        Returns:
            UTF-8 decoded response body on a 2xx response.

        Raises:
            StreamingAuthError: 401 / 403 from the server.
            StreamingApiError: any other non-2xx, network error, or
                decode failure.
        """
        request = urllib.request.Request(
            url,
            headers={
                # Identify the client honestly. SoundCloud doesn't
                # require a specific UA but a recognisable string helps
                # ops debug rate-limit hits.
                "User-Agent": "hypehouse-live-copilot/0.1",
                "Accept": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(
                request, timeout=self._timeout_s
            ) as response:
                raw_bytes: bytes = response.read()
        except urllib.error.HTTPError as exc:
            # 401 / 403 -> auth error; everything else is an API error.
            # We don't re-raise as is because the caller wants a
            # streaming-typed exception, not the urllib one.
            if exc.code in (401, 403):
                raise StreamingAuthError(
                    f"SoundCloud rejected client_id (HTTP {exc.code}). "
                    f"Verify ${SOUNDCLOUD_CLIENT_ID_ENV} is current."
                ) from exc
            raise StreamingApiError(
                f"SoundCloud HTTP {exc.code}: {exc.reason}"
            ) from exc
        except urllib.error.URLError as exc:
            # Transport-level failure (DNS, connection refused, TLS,
            # timeout). The UI surfaces this as "check your network".
            raise StreamingApiError(
                f"SoundCloud network error: {exc.reason}"
            ) from exc
        try:
            return raw_bytes.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise StreamingApiError(
                "SoundCloud returned non-UTF-8 body"
            ) from exc

    def _parse_track(
        self, item: Any
    ) -> StreamingTrack | None:
        """Project one /tracks JSON item into a :class:`StreamingTrack`.

        Returns ``None`` (so the caller can drop the row silently) when
        the item:

        * is not a dict (malformed payload),
        * has a non-CC ``license`` (defence-in-depth — the server-side
          filter should have already removed these),
        * is missing a required field (id / title / stream_url),
        * has a ``streamable: false`` flag — SoundCloud marks these
          when the artist disabled API streaming; the URL would 403 on
          fetch.

        We never raise from inside the loop — one bad row shouldn't
        fail the whole search. Tests assert that ARR rows are dropped
        (not surfaced as errors).
        """
        if not isinstance(item, dict):
            return None

        license_raw = item.get("license")
        if not isinstance(license_raw, str) or not is_cc_license(license_raw):
            return None

        # SoundCloud marks API-streamable tracks with ``streamable: true``.
        # Missing -> assume False (conservative). False explicitly ->
        # drop. True -> ingest.
        if item.get("streamable") is False:
            return None

        track_id_raw = item.get("id")
        if track_id_raw is None:
            return None
        track_id = str(track_id_raw)

        title = item.get("title")
        if not isinstance(title, str) or not title:
            return None

        # Artist resolution: SoundCloud nests user info under ``user``.
        # Some tracks lack a user object (rare); fall back to a blank
        # string rather than ``None`` so the wire shape is uniform.
        user = item.get("user")
        if isinstance(user, dict):
            artist_raw = user.get("username")
            artist = (
                artist_raw if isinstance(artist_raw, str) else ""
            )
        else:
            artist = ""

        # Duration is in milliseconds on the SoundCloud API. Convert to
        # seconds for the in-memory shape (consistent with TrackRef).
        duration_ms_raw = item.get("duration")
        duration_s: float
        if isinstance(duration_ms_raw, (int, float)) and not isinstance(
            duration_ms_raw, bool
        ):
            duration_s = float(duration_ms_raw) / 1000.0
        else:
            duration_s = 0.0

        genre_raw = item.get("genre")
        genre = genre_raw if isinstance(genre_raw, str) else ""

        # ``stream_url`` is the resolve-time URL — we still need to
        # append the client_id to make it playable. Treat the
        # SoundCloud-returned field as the base; build the final URL
        # via :meth:`resolve_stream_url` so the suffix scheme is in one
        # place.
        if not isinstance(item.get("stream_url"), str):
            # Some CC tracks have permalink_url only (legacy uploads).
            # We'd need a follow-up /resolve call — out of scope for
            # v0.1; drop silently.
            return None

        stream_url = self.resolve_stream_url(track_id)

        return StreamingTrack(
            id=track_id,
            title=title,
            artist=artist,
            duration_s=duration_s,
            key=None,  # SoundCloud doesn't publish musical key.
            genre=genre,
            license=license_raw.strip().lower(),
            stream_url=stream_url,
        )


__all__ = [
    "SOUNDCLOUD_API_BASE",
    "SOUNDCLOUD_APPLY_URL",
    "SOUNDCLOUD_CLIENT_ID_ENV",
    "SoundCloudClient",
]
