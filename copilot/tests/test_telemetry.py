"""Tests for ``copilot.telemetry``.

These tests are import-clean: they never import ``sentry_sdk``. The
:func:`init_telemetry` function is exercised both with and without the
SDK present by monkey-patching ``sys.modules``.
"""
from __future__ import annotations

import sys
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

from copilot import telemetry


def test_resolve_enabled_off_when_nothing_set(tmp_path: Path) -> None:
    assert telemetry.resolve_enabled(None, None) == "off"
    assert telemetry.resolve_enabled("", None) == "off"
    assert telemetry.resolve_enabled("0", tmp_path / "missing.toml") == "off"


def test_resolve_enabled_env_truthy() -> None:
    for v in ("1", "true", "yes", "on", "TRUE", "Yes"):
        assert telemetry.resolve_enabled(v, None) == "env", f"value {v!r}"


def test_resolve_enabled_config_file(tmp_path: Path) -> None:
    p = tmp_path / "telemetry.toml"
    p.write_text("# comment\nenabled = true\n", encoding="utf-8")
    assert telemetry.resolve_enabled("0", p) == "config"
    p.write_text("enabled = false\n", encoding="utf-8")
    assert telemetry.resolve_enabled("0", p) == "off"


def test_parse_enabled_from_toml_edge_cases() -> None:
    assert telemetry._parse_enabled_from_toml("enabled = true")
    assert telemetry._parse_enabled_from_toml('  "enabled"  =  true  ')
    assert not telemetry._parse_enabled_from_toml("# enabled = true")
    assert not telemetry._parse_enabled_from_toml("enabled = false")
    assert not telemetry._parse_enabled_from_toml("")


def test_scrub_string_home_paths_collapsed() -> None:
    assert telemetry.scrub_string("/Users/jane/Music/x.mp3") == "<scrubbed-path>"
    assert telemetry.scrub_string("/home/jane/x") == "<scrubbed-path>"
    assert telemetry.scrub_string("C:\\Users\\jane\\x") == "<scrubbed-path>"


def test_scrub_string_basename_for_other_paths() -> None:
    assert telemetry.scrub_string("/tmp/cache/file.dat") == "file.dat"
    assert telemetry.scrub_string("relative/sub/file.dat") == "file.dat"
    assert telemetry.scrub_string("hello") == "hello"


def test_scrub_pii_strips_headers_user_and_paths() -> None:
    event: dict[str, Any] = {
        "request": {
            "headers": {"Authorization": "Bearer secret"},
            "cookies": "sess=abc",
            "url": "https://x/y",
        },
        "user": {"username": "jane"},
        "server_name": "jane-mbp",
        "extra": {
            "track_path": "/Users/jane/Music/x.mp3",
            "ok": "leave-me",
            "nested": {"q": "/home/jane/y"},
        },
        "breadcrumbs": {
            "values": [
                {
                    "message": "/Users/jane/Music/y.mp3",
                    "data": {"path": "/Users/jane/z"},
                },
            ]
        },
    }
    out = telemetry.scrub_pii(event)
    assert out is not None
    assert "headers" not in out["request"]
    assert "cookies" not in out["request"]
    assert "user" not in out
    assert "server_name" not in out
    assert out["extra"]["track_path"] == "<scrubbed-path>"
    assert out["extra"]["ok"] == "leave-me"
    assert out["extra"]["nested"]["q"] == "<scrubbed-path>"
    assert out["breadcrumbs"]["values"][0]["message"] == "<scrubbed-path>"
    assert out["breadcrumbs"]["values"][0]["data"]["path"] == "<scrubbed-path>"


def test_init_telemetry_off_by_default(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(telemetry.ENV_ENABLED, raising=False)
    # Even if sentry-sdk is installed, init must stay off when no env
    # var is set. We don't bother stubbing sys.modules — the resolve
    # path short-circuits before the import would happen.
    assert telemetry.init_telemetry() is False


def test_init_telemetry_skips_when_sdk_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(telemetry.ENV_ENABLED, "1")
    # Force the import to fail by inserting a None placeholder, which
    # is the canonical "module unavailable" sentinel CPython uses
    # internally.
    monkeypatch.setitem(sys.modules, "sentry_sdk", None)
    assert telemetry.init_telemetry() is False


def test_init_telemetry_calls_sentry_when_enabled(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(telemetry.ENV_ENABLED, "1")
    monkeypatch.setenv(
        telemetry.ENV_DSN, "https://example@o0.ingest.sentry.io/1"
    )

    # Stub the sentry_sdk module so we don't need it installed.
    captured: dict[str, Any] = {}

    fake = ModuleType("sentry_sdk")

    def fake_init(**kwargs: Any) -> None:
        captured.update(kwargs)

    fake.init = fake_init  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "sentry_sdk", fake)

    assert telemetry.init_telemetry() is True
    assert captured["dsn"] == "https://example@o0.ingest.sentry.io/1"
    assert captured["traces_sample_rate"] == 0.1
    assert captured["send_default_pii"] is False
    # before_send hook should be wired and scrub headers.
    before = captured["before_send"]
    scrubbed = before({"request": {"headers": {"x": "y"}}}, None)
    assert "headers" not in scrubbed["request"]
