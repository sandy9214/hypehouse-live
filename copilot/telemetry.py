"""Opt-in Sentry telemetry for the copilot service.

This module is **import-clean**: if ``sentry_sdk`` is not installed,
:func:`init_telemetry` returns ``False`` instead of raising. The
copilot's hard runtime dependencies stay untouched — telemetry lives
in the ``telemetry`` extras (see ``pyproject.toml``).

# Privacy

Telemetry is OFF by default. To enable it the operator must set
``HYPEHOUSE_TELEMETRY_ENABLED=1`` **or** create
``~/.config/hypehouse-live/telemetry.toml`` with ``enabled = true``.
No DSN is contacted otherwise.

# Scrubbing

Every event is run through :func:`scrub_pii` before being sent. We
strip request headers, drop string values in ``extra`` /
``breadcrumbs`` that look like filesystem paths (collapsing home
paths to ``<scrubbed-path>``), and remove the ``user`` block
entirely. The mashup proposer occasionally logs a track id, which is
a stable opaque identifier and stays.
"""
from __future__ import annotations

import logging
import os
import re
from pathlib import Path
from typing import Any, Mapping

log = logging.getLogger(__name__)

#: Hardcoded placeholder DSN. Fork operators override via the
#: ``HYPEHOUSE_TELEMETRY_DSN`` env var (or by editing this constant)
#: before turning telemetry on.
PLACEHOLDER_DSN = (
    "https://examplePublicKey@o4500000.ingest.sentry.io/4500000000000000"
)

ENV_ENABLED = "HYPEHOUSE_TELEMETRY_ENABLED"
ENV_DSN = "HYPEHOUSE_TELEMETRY_DSN"
ENV_ENVIRONMENT = "HYPEHOUSE_TELEMETRY_ENVIRONMENT"

_TRUTHY = frozenset({"1", "true", "yes", "on"})

_HOME_PREFIXES = (
    "/Users/",
    "/home/",
    "C:\\Users\\",
    "C:/Users/",
)

# Match anything containing a / or \ — treated as a path.
_PATH_RE = re.compile(r"[\\/]")


def _is_truthy(value: str | None) -> bool:
    if value is None:
        return False
    return value.strip().lower() in _TRUTHY


def default_config_path() -> Path:
    """Default config file location.

    Honours ``XDG_CONFIG_HOME``; otherwise falls back to
    ``$HOME/.config/hypehouse-live/telemetry.toml``.
    """
    root_env = os.environ.get("XDG_CONFIG_HOME") or ""
    if root_env:
        root = Path(root_env)
    else:
        home = os.environ.get("HOME") or ""
        root = Path(home) / ".config" if home else Path(".config")
    return root / "hypehouse-live" / "telemetry.toml"


def _parse_enabled_from_toml(contents: str) -> bool:
    """Cheap TOML scanner — looks for a top-level ``enabled = true``.

    We deliberately avoid pulling in ``tomllib`` so this module stays
    fast to import on cold starts.
    """
    for raw in contents.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        normalised = "".join(ch for ch in line if not ch.isspace() and ch != '"')
        if normalised.lower() == "enabled=true":
            return True
    return False


def resolve_enabled(
    env_value: str | None = None,
    config_path: Path | None = None,
) -> str:
    """Return ``'env'`` / ``'config'`` / ``'off'``.

    Pure-ish (only reads the config file when provided). Test surface.
    """
    if _is_truthy(env_value):
        return "env"
    if config_path is not None:
        try:
            contents = config_path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            return "off"
        if _parse_enabled_from_toml(contents):
            return "config"
    return "off"


def scrub_string(s: str) -> str:
    """Collapse home-directory paths, keep only basename of other paths."""
    for prefix in _HOME_PREFIXES:
        if s.startswith(prefix):
            return "<scrubbed-path>"
    if _PATH_RE.search(s):
        tail = re.split(r"[\\/]", s)[-1]
        return tail if tail else "<scrubbed-path>"
    return s


def _scrub_value(v: Any) -> Any:
    if isinstance(v, str):
        return scrub_string(v)
    if isinstance(v, list):
        return [_scrub_value(item) for item in v]
    if isinstance(v, dict):
        return {k: _scrub_value(val) for k, val in v.items()}
    return v


def scrub_pii(event: dict[str, Any], hint: Mapping[str, Any] | None = None) -> dict[str, Any] | None:
    """Sentry ``before_send`` hook.

    Returns the scrubbed event back to the SDK, or ``None`` to drop.
    """
    del hint  # unused
    # Drop request headers + cookies wholesale — bearer tokens leak
    # through here otherwise.
    request = event.get("request")
    if isinstance(request, dict):
        request.pop("headers", None)
        request.pop("cookies", None)
        request.pop("query_string", None)
    # Strip user identity entirely.
    event.pop("user", None)
    event.pop("server_name", None)
    # Scrub extra + tags + contexts.
    for key in ("extra", "tags", "contexts"):
        if key in event:
            event[key] = _scrub_value(event[key])
    # Scrub breadcrumb messages — these often carry filenames.
    breadcrumbs = event.get("breadcrumbs")
    if isinstance(breadcrumbs, dict) and isinstance(breadcrumbs.get("values"), list):
        for b in breadcrumbs["values"]:
            if isinstance(b, dict):
                if isinstance(b.get("message"), str):
                    b["message"] = scrub_string(b["message"])
                if "data" in b:
                    b["data"] = _scrub_value(b["data"])
    elif isinstance(breadcrumbs, list):
        for b in breadcrumbs:
            if isinstance(b, dict):
                if isinstance(b.get("message"), str):
                    b["message"] = scrub_string(b["message"])
                if "data" in b:
                    b["data"] = _scrub_value(b["data"])
    return event


def init_telemetry() -> bool:
    """Initialise Sentry if the user has opted in.

    Returns ``True`` when the SDK was initialised, ``False`` otherwise.
    Never raises — a missing ``sentry_sdk`` import or a malformed DSN
    is logged and treated as "telemetry stays off". The copilot must
    keep running.
    """
    decision = resolve_enabled(
        os.environ.get(ENV_ENABLED),
        default_config_path(),
    )
    if decision == "off":
        log.info("telemetry: disabled (set %s=1 to opt in)", ENV_ENABLED)
        return False

    try:
        import sentry_sdk  # type: ignore[import-not-found]
    except ImportError:
        log.warning(
            "telemetry: %s is set but sentry-sdk is not installed — "
            "telemetry stays disabled. `pip install 'hypehouse-copilot[telemetry]'`",
            ENV_ENABLED,
        )
        return False

    dsn = (os.environ.get(ENV_DSN) or PLACEHOLDER_DSN).strip()
    if not dsn:
        log.info("telemetry: DSN empty — staying disabled")
        return False
    environment = (os.environ.get(ENV_ENVIRONMENT) or "").strip() or "production"
    try:
        sentry_sdk.init(
            dsn=dsn,
            release="hypehouse-copilot@0.1.0",
            environment=environment,
            traces_sample_rate=0.1,
            send_default_pii=False,
            attach_stacktrace=True,
            before_send=scrub_pii,
        )
    except Exception as e:  # noqa: BLE001 — never crash the copilot
        log.warning("telemetry: sentry_sdk.init failed: %s", e)
        return False
    log.info("telemetry: enabled via %s — Sentry SDK initialised", decision)
    return True
