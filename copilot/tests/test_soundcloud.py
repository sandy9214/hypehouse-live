"""SoundCloud streaming-source client tests.

The :class:`copilot.streaming.soundcloud.SoundCloudClient` makes synchronous
HTTP GETs via :func:`urllib.request.urlopen` (see module docstring for
the transport choice rationale). Tests monkeypatch ``urlopen`` to return
canned JSON bodies — no network is touched.

Coverage:
    * search returns CC-filtered results (full happy path).
    * License filter rejects ``"all-rights-reserved"`` rows.
    * License filter rejects unknown license strings.
    * resolve_stream_url returns the expected URL with client_id.
    * Missing client_id raises clear error with apply-URL substring.
    * HTTP 401 raises StreamingAuthError, not generic API error.
    * HTTP 5xx raises StreamingApiError.
    * Non-streamable tracks are dropped (CC-licensed but ``streamable: false``).
    * add_streaming_track persists with ``source="soundcloud"`` and
      ``track_id`` namespaced ``"<provider>:<id>"``.
    * library_rpc ``streaming.search`` dispatches to the injected
      provider and returns the asdict shape the UI expects.
    * library_rpc ``streaming.add_to_library`` rejects non-CC metadata.
"""
from __future__ import annotations

import io
import json
from typing import Any
from unittest.mock import patch

import pytest

from copilot.library import SOURCE_LOCAL, TrackLibrary
from copilot.library_rpc import (
    JSONRPC_FEATURE_NOT_INSTALLED,
    JSONRPC_INVALID_PARAMS,
    LibraryRpcHandler,
    RpcError,
)
from copilot.streaming import (
    CC_LICENSES,
    StreamingApiError,
    StreamingAuthError,
    StreamingTrack,
    is_cc_license,
)
from copilot.streaming.soundcloud import (
    SOUNDCLOUD_APPLY_URL,
    SOUNDCLOUD_CLIENT_ID_ENV,
    SoundCloudClient,
)


# --- Fixtures --------------------------------------------------------------


@pytest.fixture
def fake_search_payload() -> list[dict[str, Any]]:
    """A canned ``/tracks`` response covering the full filter matrix.

    Six rows:
        * 2 CC tracks (one cc-by, one cc-by-sa) — both should pass.
        * 1 all-rights-reserved track — dropped by the CC filter.
        * 1 unknown-license track ("custom") — dropped.
        * 1 CC-licensed but ``streamable: false`` track — dropped (the
          stream_url would 403 on fetch).
        * 1 row missing ``stream_url`` — dropped (legacy upload).
    """
    return [
        {
            "id": 111,
            "title": "Lo-fi Beats Forever",
            "user": {"username": "ChillProducer"},
            "duration": 234_000,  # ms
            "license": "cc-by",
            "streamable": True,
            "genre": "lo-fi",
            "stream_url": "https://api.soundcloud.com/tracks/111/stream",
        },
        {
            "id": 222,
            "title": "Beach Drive",
            "user": {"username": "SunsetMix"},
            "duration": 198_500,
            "license": "cc-by-sa",
            "streamable": True,
            "genre": "house",
            "stream_url": "https://api.soundcloud.com/tracks/222/stream",
        },
        {
            "id": 333,
            "title": "Major Label Hit",
            "user": {"username": "BigArtist"},
            "duration": 210_000,
            "license": "all-rights-reserved",
            "streamable": True,
            "genre": "pop",
            "stream_url": "https://api.soundcloud.com/tracks/333/stream",
        },
        {
            "id": 444,
            "title": "Weird License Track",
            "user": {"username": "EdgeCase"},
            "duration": 180_000,
            "license": "custom",
            "streamable": True,
            "genre": "experimental",
            "stream_url": "https://api.soundcloud.com/tracks/444/stream",
        },
        {
            "id": 555,
            "title": "CC But Not Streamable",
            "user": {"username": "Withdrawn"},
            "duration": 200_000,
            "license": "cc-by-nc",
            "streamable": False,
            "genre": "ambient",
            "stream_url": "https://api.soundcloud.com/tracks/555/stream",
        },
        {
            "id": 666,
            "title": "Legacy Upload No Stream",
            "user": {"username": "OldSchool"},
            "duration": 150_000,
            "license": "cc-by",
            "streamable": True,
            "genre": "downtempo",
            # No ``stream_url`` field.
        },
    ]


def _make_urlopen_mock(body: Any, *, status: int = 200):
    """Build a context-manager-yielding mock for ``urllib.request.urlopen``.

    ``body`` is JSON-serialised and returned as ``response.read()`` bytes.
    ``status`` is encoded by raising HTTPError when non-2xx (the SoundCloud
    client maps HTTPError -> StreamingApiError / StreamingAuthError).
    """
    raw = json.dumps(body).encode("utf-8") if not isinstance(body, bytes) else body

    if status >= 400:
        import urllib.error

        def raiser(*_args, **_kwargs):
            raise urllib.error.HTTPError(
                url="http://test", code=status, msg="error", hdrs=None, fp=None
            )

        return raiser

    class _FakeResponse:
        def __enter__(self):
            self._buf = io.BytesIO(raw)
            return self

        def __exit__(self, *_exc):
            return False

        def read(self) -> bytes:
            return raw

    def opener(*_args, **_kwargs):
        return _FakeResponse()

    return opener


# --- License filter --------------------------------------------------------


def test_cc_licenses_constant_covers_all_six_families():
    """The hardcoded set must include all six standard CC families.

    A regression here (dropping cc-by-nd, say) would silently reject a
    legitimate CC catalog — a *strict* false negative that quietly
    shrinks the user's library. Brittle on purpose.
    """
    assert CC_LICENSES == {
        "cc-by",
        "cc-by-sa",
        "cc-by-nc",
        "cc-by-nc-sa",
        "cc-by-nd",
        "cc-by-nc-nd",
    }


def test_is_cc_license_rejects_arr_and_unknown():
    assert is_cc_license("cc-by") is True
    assert is_cc_license("CC-BY") is True  # case-insensitive
    assert is_cc_license("  cc-by-sa  ") is True  # whitespace tolerated
    assert is_cc_license("all-rights-reserved") is False
    assert is_cc_license("custom") is False
    assert is_cc_license("") is False
    assert is_cc_license(None) is False


# --- Construction / auth ---------------------------------------------------


def test_missing_client_id_raises_clear_error(monkeypatch):
    """Construction without env var or explicit id must point at the apply URL.

    A user staring at "auth error" with no remediation is the failure
    mode this test prevents. Asserting on the URL substring ensures the
    error message stays operator-actionable.
    """
    monkeypatch.delenv(SOUNDCLOUD_CLIENT_ID_ENV, raising=False)
    with pytest.raises(StreamingAuthError) as excinfo:
        SoundCloudClient()
    msg = str(excinfo.value)
    assert SOUNDCLOUD_CLIENT_ID_ENV in msg
    assert SOUNDCLOUD_APPLY_URL in msg


def test_explicit_client_id_overrides_env(monkeypatch):
    monkeypatch.setenv(SOUNDCLOUD_CLIENT_ID_ENV, "from-env")
    client = SoundCloudClient(client_id="explicit")
    # Sanity probe via resolve_stream_url — the only public surface
    # that reveals the client_id encoded into a URL.
    url = client.resolve_stream_url("abc")
    assert "client_id=explicit" in url
    assert "from-env" not in url


# --- search ----------------------------------------------------------------


def test_search_returns_cc_filtered_results(fake_search_payload):
    """Happy path: 2 CC tracks survive the 6-row fixture."""
    client = SoundCloudClient(client_id="test-key")
    with patch(
        "copilot.streaming.soundcloud.urllib.request.urlopen",
        _make_urlopen_mock(fake_search_payload),
    ):
        results = client.search("lo-fi", limit=20)

    assert len(results) == 2
    ids = [t.id for t in results]
    assert ids == ["111", "222"]
    # Survivors are typed StreamingTrack with the expected fields.
    first = results[0]
    assert isinstance(first, StreamingTrack)
    assert first.title == "Lo-fi Beats Forever"
    assert first.artist == "ChillProducer"
    assert first.license == "cc-by"
    assert first.duration_s == pytest.approx(234.0)
    assert first.key is None  # SoundCloud doesn't expose key
    assert "client_id=test-key" in first.stream_url
    assert "/tracks/111/stream" in first.stream_url


def test_search_rejects_arr_and_unknown_license(fake_search_payload):
    """Defence-in-depth: even if the server returns ARR, client drops it.

    The test fixture's row 333 is ``all-rights-reserved`` and row 444 is
    a fake ``"custom"`` license — neither should appear in results.
    """
    client = SoundCloudClient(client_id="test-key")
    with patch(
        "copilot.streaming.soundcloud.urllib.request.urlopen",
        _make_urlopen_mock(fake_search_payload),
    ):
        results = client.search("any", limit=20)
    surviving_ids = {t.id for t in results}
    assert "333" not in surviving_ids  # ARR
    assert "444" not in surviving_ids  # unknown
    # Also: license string on every surviving track must be CC.
    for t in results:
        assert is_cc_license(t.license)


def test_search_drops_non_streamable_and_missing_stream_url(fake_search_payload):
    """Rows 555 (streamable=False) and 666 (no stream_url) must drop."""
    client = SoundCloudClient(client_id="test-key")
    with patch(
        "copilot.streaming.soundcloud.urllib.request.urlopen",
        _make_urlopen_mock(fake_search_payload),
    ):
        results = client.search("any", limit=20)
    surviving_ids = {t.id for t in results}
    assert "555" not in surviving_ids
    assert "666" not in surviving_ids


def test_search_clamps_limit():
    """Limit > MAX_LIMIT is silently clamped, limit < 1 is bumped to 1.

    Both clamps are silent because the UI's pagination control is the
    user's primary affordance; an HTTP error for "you asked for 9999"
    would just bounce a working request. The URL query reflects the
    clamped value so we can verify it.
    """
    captured_url: list[str] = []

    def capture(request, **_kwargs):
        # ``request`` is a urllib.request.Request — extract .full_url.
        captured_url.append(request.full_url)

        class _R:
            def __enter__(self):
                return self

            def __exit__(self, *_exc):
                return False

            def read(self):
                return b"[]"

        return _R()

    client = SoundCloudClient(client_id="k")
    with patch(
        "copilot.streaming.soundcloud.urllib.request.urlopen", capture
    ):
        client.search("q", limit=9999)
    assert any("limit=50" in u for u in captured_url)  # MAX_LIMIT == 50


def test_resolve_stream_url_returns_expected_shape():
    """resolve_stream_url must build ``/tracks/<id>/stream?client_id=...``."""
    client = SoundCloudClient(client_id="my-key")
    url = client.resolve_stream_url("999")
    assert url.startswith("https://api.soundcloud.com/tracks/999/stream?")
    assert "client_id=my-key" in url


def test_resolve_stream_url_rejects_blank_id():
    client = SoundCloudClient(client_id="k")
    with pytest.raises(StreamingApiError):
        client.resolve_stream_url("")


# --- HTTP error mapping ----------------------------------------------------


def test_search_maps_401_to_auth_error():
    """401 -> StreamingAuthError (not the generic API error).

    The wire layer translates AuthError into ``-32000`` so the UI shows
    a "configure SoundCloud" affordance; mapping 401 into ApiError
    would mis-route it as a retry-able transient.
    """
    client = SoundCloudClient(client_id="bad-key")
    with patch(
        "copilot.streaming.soundcloud.urllib.request.urlopen",
        _make_urlopen_mock([], status=401),
    ):
        with pytest.raises(StreamingAuthError):
            client.search("q")


def test_search_maps_500_to_api_error():
    """5xx -> StreamingApiError (transient; UI retries)."""
    client = SoundCloudClient(client_id="k")
    with patch(
        "copilot.streaming.soundcloud.urllib.request.urlopen",
        _make_urlopen_mock([], status=503),
    ):
        with pytest.raises(StreamingApiError):
            client.search("q")


def test_search_raises_on_non_list_body():
    """Successful HTTP but ``{errors: ...}`` body -> StreamingApiError."""
    client = SoundCloudClient(client_id="k")
    with patch(
        "copilot.streaming.soundcloud.urllib.request.urlopen",
        _make_urlopen_mock({"errors": ["something"]}),
    ):
        with pytest.raises(StreamingApiError):
            client.search("q")


# --- Library integration ---------------------------------------------------


def test_add_streaming_track_persists_with_source_column():
    """Schema v9: streaming row sets ``source="soundcloud"`` + namespaced id.

    The track_id must be ``"soundcloud:<id>"`` so a streaming row can't
    collide with a local-file ``Path.stem`` id. ``path`` holds the
    playable HTTP URL (the engine treats it like any other input).
    """
    lib = TrackLibrary(":memory:")
    try:
        ref = lib.add_streaming_track(
            provider="soundcloud",
            track_id="111",
            title="Lo-fi Beats",
            artist="ChillProducer",
            duration_s=234.0,
            stream_url="https://api.soundcloud.com/tracks/111/stream?client_id=k",
            genre="lo-fi",
        )
        assert ref.source == "soundcloud"
        assert ref.track_id == "soundcloud:111"
        assert ref.path.startswith("https://")
        # Re-read from the DB to confirm round-trip.
        fetched = lib.get("soundcloud:111")
        assert fetched is not None
        assert fetched.source == "soundcloud"
        # Camelot key defaults to ``"?"`` when provider didn't supply
        # one — gets filtered out of mashup gates until lazy analysis
        # back-fills.
        assert fetched.camelot_key == "?"
    finally:
        lib.close()


def test_existing_rows_get_source_local_after_migration():
    """A row inserted by the older ``add_track`` path picks up the default.

    Defends the v8 -> v9 migration: the SQL DEFAULT 'local' clause
    on ``ALTER TABLE ADD COLUMN`` must backfill, not leave NULL.
    """
    from copilot.library import TrackRef

    lib = TrackLibrary(":memory:")
    try:
        lib.add_track(
            TrackRef(
                track_id="local-file-1",
                path="/tmp/song.mp3",
                bpm=120.0,
                camelot_key="8B",
                energy=0.2,
                duration_s=200.0,
            )
        )
        fetched = lib.get("local-file-1")
        assert fetched is not None
        assert fetched.source == SOURCE_LOCAL
    finally:
        lib.close()


# --- RPC dispatch ----------------------------------------------------------


class _FakeProvider:
    """In-memory streaming provider for RPC-level tests."""

    name = "soundcloud"

    def __init__(self, results: list[StreamingTrack]):
        self._results = results

    def search(self, query: str, limit: int = 20) -> list[StreamingTrack]:
        return list(self._results[:limit])

    def resolve_stream_url(self, track_id: str) -> str:
        return f"https://test/tracks/{track_id}/stream"


async def test_rpc_streaming_search_returns_results_dict():
    """``streaming.search`` calls the injected provider + wraps as dict."""
    lib = TrackLibrary(":memory:")
    fake = _FakeProvider(
        [
            StreamingTrack(
                id="111",
                title="Lo-fi",
                artist="Chill",
                duration_s=234.0,
                key=None,
                genre="lo-fi",
                license="cc-by",
                stream_url="https://test/tracks/111/stream",
            )
        ]
    )
    handler = LibraryRpcHandler(
        lib, streaming_providers={"soundcloud": fake}
    )
    try:
        result = await handler.dispatch(
            "streaming.search",
            {"provider": "soundcloud", "query": "lo-fi", "limit": 5},
        )
        assert result["provider"] == "soundcloud"
        assert result["query"] == "lo-fi"
        assert len(result["results"]) == 1
        assert result["results"][0]["id"] == "111"
        assert result["results"][0]["license"] == "cc-by"
    finally:
        lib.close()


async def test_rpc_streaming_add_rejects_non_cc():
    """``streaming.add_to_library`` re-validates the license at the trust boundary.

    Even if a buggy provider returned an ARR track, the library must
    refuse to persist it — copyright strike risk on the user's mixtape
    export otherwise.
    """
    lib = TrackLibrary(":memory:")
    handler = LibraryRpcHandler(lib, streaming_providers={"soundcloud": _FakeProvider([])})
    try:
        with pytest.raises(RpcError) as excinfo:
            await handler.dispatch(
                "streaming.add_to_library",
                {
                    "provider": "soundcloud",
                    "track_id": "111",
                    "metadata": {
                        "title": "Major Label Hit",
                        "artist": "BigArtist",
                        "duration_s": 200.0,
                        "stream_url": "https://test/x",
                        "license": "all-rights-reserved",
                    },
                },
            )
        assert excinfo.value.code == JSONRPC_INVALID_PARAMS
        # Library must be empty — the row never made it in.
        assert lib.count_tracks() == 0
    finally:
        lib.close()


async def test_rpc_streaming_add_persists_cc_metadata():
    """Happy-path RPC add stores the track and returns a ``track`` wire dict."""
    lib = TrackLibrary(":memory:")
    handler = LibraryRpcHandler(lib, streaming_providers={"soundcloud": _FakeProvider([])})
    try:
        result = await handler.dispatch(
            "streaming.add_to_library",
            {
                "provider": "soundcloud",
                "track_id": "222",
                "metadata": {
                    "title": "Beach Drive",
                    "artist": "SunsetMix",
                    "duration_s": 198.5,
                    "stream_url": "https://api.soundcloud.com/tracks/222/stream?client_id=k",
                    "license": "cc-by-sa",
                    "genre": "house",
                    "key": None,
                },
            },
        )
        assert result["track"]["id"] == "soundcloud:222"
        assert result["track"]["source"] == "soundcloud"
        assert result["track"]["duration_s"] == pytest.approx(198.5)
        # And it's queryable via list_tracks now.
        assert lib.count_tracks() == 1
    finally:
        lib.close()


async def test_rpc_unknown_provider_raises_invalid_params():
    """``streaming.search`` with an unknown provider -> -32602."""
    lib = TrackLibrary(":memory:")
    handler = LibraryRpcHandler(lib)
    try:
        with pytest.raises(RpcError) as excinfo:
            await handler.dispatch(
                "streaming.search",
                {"provider": "beatport", "query": "x"},
            )
        assert excinfo.value.code == JSONRPC_INVALID_PARAMS
    finally:
        lib.close()


async def test_rpc_missing_creds_maps_to_feature_not_installed(monkeypatch):
    """Lazy provider construction with no env var -> -32000 (not generic error).

    The UI's "configure optional feature" handler keys on ``-32000``,
    so a misroute here would silently hide the apply-for-key affordance.
    """
    monkeypatch.delenv(SOUNDCLOUD_CLIENT_ID_ENV, raising=False)
    lib = TrackLibrary(":memory:")
    handler = LibraryRpcHandler(lib)  # no injected providers
    try:
        with pytest.raises(RpcError) as excinfo:
            await handler.dispatch(
                "streaming.search",
                {"provider": "soundcloud", "query": "x"},
            )
        assert excinfo.value.code == JSONRPC_FEATURE_NOT_INSTALLED
        assert SOUNDCLOUD_APPLY_URL in excinfo.value.message
    finally:
        lib.close()
