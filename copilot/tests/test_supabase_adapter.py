"""Live-shape tests for :class:`SupabaseSyncClient`.

We spin up a tiny `http.server` instance and point the adapter at it
instead of a live Supabase project. Same code path the real client
runs; only the upstream changes. Keeps the suite hermetic + cheap (<
50 ms per test on a laptop).
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

import pytest

from copilot.cloud_sync import RemoteTrack, SyncError
from copilot.cloud_sync.supabase import SupabaseSyncClient


# ----- mock server ---------------------------------------------------


class _MockState:
    """Per-test shared state — captured request + canned response."""

    def __init__(self) -> None:
        self.captured_method: str = ""
        self.captured_path: str = ""
        self.captured_query: dict[str, list[str]] = {}
        self.captured_headers: dict[str, str] = {}
        self.captured_body: bytes = b""
        self.response_status: int = 200
        self.response_body: object = []


def _build_handler(state: _MockState) -> type[BaseHTTPRequestHandler]:
    class _H(BaseHTTPRequestHandler):
        def log_message(self, *_args):  # quiet test output
            pass

        def _capture(self) -> None:
            parsed = urlparse(self.path)
            state.captured_method = self.command
            state.captured_path = parsed.path
            state.captured_query = parse_qs(parsed.query)
            state.captured_headers = {
                k.lower(): v for k, v in self.headers.items()
            }
            length = int(self.headers.get("content-length", "0"))
            state.captured_body = self.rfile.read(length) if length else b""

        def _respond(self) -> None:
            payload = json.dumps(state.response_body).encode("utf-8")
            self.send_response(state.response_status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self):
            self._capture()
            self._respond()

        def do_POST(self):
            self._capture()
            self._respond()

        def do_DELETE(self):
            self._capture()
            self._respond()

    return _H


@pytest.fixture
def mock_server():
    state = _MockState()
    server = HTTPServer(("127.0.0.1", 0), _build_handler(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    host, port = server.server_address
    base = f"http://{host}:{port}"
    try:
        yield base, state
    finally:
        server.shutdown()
        thread.join(timeout=2.0)


def _client(base: str) -> SupabaseSyncClient:
    return SupabaseSyncClient(url=base, anon_key="test-key", timeout_s=2.0)


# ----- constructor + from_env ----------------------------------------


def test_constructor_rejects_empty_url() -> None:
    with pytest.raises(SyncError):
        SupabaseSyncClient(url="", anon_key="k")


def test_constructor_rejects_empty_key() -> None:
    with pytest.raises(SyncError):
        SupabaseSyncClient(url="https://x.supabase.co", anon_key="")


def test_from_env_uses_default_var_names(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SUPABASE_URL", "https://abc.supabase.co")
    monkeypatch.setenv("SUPABASE_ANON_KEY", "eyJ-test")
    c = SupabaseSyncClient.from_env()
    # Sanity — base URL trimmed of trailing slash; we don't expose
    # the key but constructor would have rejected empty.
    assert c is not None


def test_from_env_missing_url_raises(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("SUPABASE_URL", raising=False)
    monkeypatch.setenv("SUPABASE_ANON_KEY", "k")
    with pytest.raises(SyncError):
        SupabaseSyncClient.from_env()


# ----- list_tracks ---------------------------------------------------


def test_list_tracks_constructs_postgrest_filter(mock_server) -> None:
    base, state = mock_server
    state.response_body = [
        {
            "track_id": "a",
            "path": "/a",
            "bpm": 124.0,
            "camelot_key": "8B",
            "energy": 0.6,
            "duration_s": 200.0,
            "hot_cues_ms": [1000, -1, -1, -1, -1, -1, -1, -1],
            "updated_at_micros": 100,
        }
    ]
    rows = _client(base).list_tracks(since_micros=50)
    assert state.captured_method == "GET"
    assert state.captured_path == "/rest/v1/tracks"
    assert state.captured_query["select"] == ["*"]
    assert state.captured_query["updated_at_micros"] == ["gte.50"]
    # apikey + bearer auth on every request.
    assert state.captured_headers["apikey"] == "test-key"
    assert state.captured_headers["authorization"] == "Bearer test-key"
    assert len(rows) == 1
    assert rows[0].track_id == "a"
    assert rows[0].bpm == 124.0
    assert rows[0].updated_at_micros == 100


def test_list_tracks_empty_body_returns_empty_list(mock_server) -> None:
    base, state = mock_server
    state.response_body = []
    assert _client(base).list_tracks() == []


def test_list_tracks_pads_hot_cues_to_8_slots(mock_server) -> None:
    base, state = mock_server
    state.response_body = [
        {
            "track_id": "a",
            "path": "/a",
            "bpm": 120.0,
            "camelot_key": "8B",
            "energy": 0.5,
            "duration_s": 100.0,
            "hot_cues_ms": [1000, 2000],
            "updated_at_micros": 0,
        }
    ]
    rows = _client(base).list_tracks()
    assert len(rows[0].hot_cues_ms) == 8
    assert rows[0].hot_cues_ms[:2] == (1000, 2000)
    assert rows[0].hot_cues_ms[2:] == (-1, -1, -1, -1, -1, -1)


# ----- get_track ----------------------------------------------------


def test_get_track_returns_none_on_empty_array(mock_server) -> None:
    base, state = mock_server
    state.response_body = []
    assert _client(base).get_track("missing") is None


def test_get_track_uses_eq_filter_and_limit(mock_server) -> None:
    base, state = mock_server
    state.response_body = [
        {
            "track_id": "abc",
            "path": "/p",
            "bpm": 120.0,
            "camelot_key": "8B",
            "energy": 0.5,
            "duration_s": 1.0,
            "hot_cues_ms": [-1] * 8,
            "updated_at_micros": 1,
        }
    ]
    row = _client(base).get_track("abc")
    assert row is not None
    assert state.captured_query["track_id"] == ["eq.abc"]
    assert state.captured_query["limit"] == ["1"]


# ----- upsert_track -------------------------------------------------


def test_upsert_track_uses_merge_duplicates_header_and_on_conflict(mock_server) -> None:
    base, state = mock_server
    state.response_body = []
    track = RemoteTrack(
        track_id="x",
        path="/x",
        bpm=120.0,
        camelot_key="8B",
        energy=0.5,
        duration_s=100.0,
        hot_cues_ms=(1000, -1, -1, -1, -1, -1, -1, -1),
        updated_at_micros=42,
    )
    _client(base).upsert_track(track)
    assert state.captured_method == "POST"
    assert "resolution=merge-duplicates" in state.captured_headers["prefer"]
    assert state.captured_query["on_conflict"] == ["track_id"]
    sent = json.loads(state.captured_body)
    assert isinstance(sent, list) and len(sent) == 1
    assert sent[0]["track_id"] == "x"
    assert sent[0]["hot_cues_ms"] == [1000, -1, -1, -1, -1, -1, -1, -1]
    assert sent[0]["updated_at_micros"] == 42


# ----- delete_track ------------------------------------------------


def test_delete_track_returns_true_on_success(mock_server) -> None:
    base, state = mock_server
    state.response_body = []
    assert _client(base).delete_track("x") is True
    assert state.captured_method == "DELETE"
    assert state.captured_query["track_id"] == ["eq.x"]


def test_delete_track_returns_false_on_404(mock_server) -> None:
    base, state = mock_server
    state.response_status = 404
    state.response_body = {"message": "not found"}
    assert _client(base).delete_track("nope") is False


# ----- transport errors --------------------------------------------


def test_5xx_raises_sync_error(mock_server) -> None:
    base, state = mock_server
    state.response_status = 500
    state.response_body = {"message": "boom"}
    with pytest.raises(SyncError) as exc:
        _client(base).list_tracks()
    assert "500" in str(exc.value)


def test_connection_refused_raises_sync_error() -> None:
    # 127.0.0.1:1 is reserved + unconnectable on every CI runner.
    client = SupabaseSyncClient(
        url="http://127.0.0.1:1", anon_key="k", timeout_s=0.5
    )
    with pytest.raises(SyncError):
        client.list_tracks()
