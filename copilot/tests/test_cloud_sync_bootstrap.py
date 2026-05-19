"""Bootstrap wiring tests (#102 slice 3)."""

from __future__ import annotations

import logging

import pytest

from copilot.cloud_sync import InMemorySyncClient, RemoteTrack
from copilot.cloud_sync.bootstrap import (
    bootstrap_pull,
    build_sync_client_from_env,
)
from copilot.cloud_sync.supabase import SupabaseSyncClient


def _mk(track_id: str, *, updated_at: int = 0) -> RemoteTrack:
    return RemoteTrack(
        track_id=track_id,
        path="/tmp/x.mp3",
        bpm=120.0,
        camelot_key="8B",
        energy=0.5,
        duration_s=180.0,
        updated_at_micros=updated_at,
    )


# ----- build_sync_client_from_env -------------------------------------


def test_falls_back_to_in_memory_when_url_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("SUPABASE_URL", raising=False)
    monkeypatch.setenv("SUPABASE_ANON_KEY", "k")
    client = build_sync_client_from_env()
    assert isinstance(client, InMemorySyncClient)


def test_falls_back_to_in_memory_when_key_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("SUPABASE_URL", "https://x.supabase.co")
    monkeypatch.delenv("SUPABASE_ANON_KEY", raising=False)
    client = build_sync_client_from_env()
    assert isinstance(client, InMemorySyncClient)


def test_falls_back_when_both_empty(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("SUPABASE_URL", "   ")
    monkeypatch.setenv("SUPABASE_ANON_KEY", "")
    client = build_sync_client_from_env()
    assert isinstance(client, InMemorySyncClient)


def test_builds_supabase_when_both_envs_present(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("SUPABASE_URL", "https://abc.supabase.co")
    monkeypatch.setenv("SUPABASE_ANON_KEY", "eyJtest")
    client = build_sync_client_from_env()
    assert isinstance(client, SupabaseSyncClient)


def test_logs_fallback_reason_at_info(
    monkeypatch: pytest.MonkeyPatch,
    caplog: pytest.LogCaptureFixture,
) -> None:
    monkeypatch.delenv("SUPABASE_URL", raising=False)
    monkeypatch.delenv("SUPABASE_ANON_KEY", raising=False)
    with caplog.at_level(logging.INFO):
        build_sync_client_from_env()
    assert any("local-only mode" in r.message for r in caplog.records)


# ----- bootstrap_pull -------------------------------------------------


def test_bootstrap_pull_logs_fetched_count(
    caplog: pytest.LogCaptureFixture,
) -> None:
    client = InMemorySyncClient(seed=[_mk("a"), _mk("b")])
    with caplog.at_level(logging.INFO):
        result = bootstrap_pull(client)
    assert result.fetched_count == 2
    assert result.inserted_count == 2  # local_updated_at stub returns None
    assert any("fetched=2" in r.message for r in caplog.records)


def test_bootstrap_pull_logs_transport_error_at_warning(
    caplog: pytest.LogCaptureFixture,
) -> None:
    from copilot.cloud_sync.client import SyncError

    class BoomClient:
        def list_tracks(self, *, since_micros: int = 0):
            raise SyncError("simulated outage")

        def get_track(self, _id):
            raise NotImplementedError

        def upsert_track(self, _t):
            raise NotImplementedError

        def delete_track(self, _id):
            raise NotImplementedError

    with caplog.at_level(logging.WARNING):
        result = bootstrap_pull(BoomClient())
    assert result.transport_error == "simulated outage"
    assert any("transport error" in r.message for r in caplog.records)


def test_bootstrap_pull_handles_empty_remote(
    caplog: pytest.LogCaptureFixture,
) -> None:
    with caplog.at_level(logging.INFO):
        result = bootstrap_pull(InMemorySyncClient())
    assert result.fetched_count == 0
    assert result.inserted_count == 0
    assert result.transport_error is None
