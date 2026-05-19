"""Streaming source providers — CC-licensed catalogs for the library.

v1.0 software-only pivot ships with SoundCloud as the first streaming
source (closes ``#101``). Beatport Free / Mixcloud / Jamendo follow on
the same :class:`StreamingProvider` abstract interface so the library +
RPC + UI surfaces stay one shape per provider.

A *streaming track* is a remote audio object the user can preview,
analyze, and load into a deck without owning the file. The library
stores it with ``source="<provider name>"`` (schema v9) so a row knows
whether to resolve ``path`` against the filesystem or against the
provider's resolve-URL endpoint.

License gate (hard rule):
    Only **Creative Commons** licenses are accepted. The provider client
    is responsible for filtering out all-rights-reserved tracks **at
    search time** — never store an ARR track in the library; the user's
    mixtape export risks copyright strikes otherwise. The accepted set
    is :data:`CC_LICENSES`; see :func:`is_cc_license`.
"""
from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Final


# Accepted Creative Commons identifiers (lowercased, no version suffix).
# We accept all six standard CC license families. Tracks that report
# ``"all-rights-reserved"`` / ``"no-rights-reserved"`` / anything else
# are filtered out at search time and never reach the library.
#
# Why include ``cc-by-nc-nd``: the *no-derivatives* clause technically
# restricts remix; we still allow ingest because the v0.1 player path
# is straight playback + crossfade, which is fair use of the released
# work. The mashup / mixtape export path will need a per-license gate
# (a future PR — track on GH #101 follow-ups). For now the user gets a
# wire-level ``license`` field on every streaming track so the UI can
# warn before export.
CC_LICENSES: Final[frozenset[str]] = frozenset(
    {
        "cc-by",
        "cc-by-sa",
        "cc-by-nc",
        "cc-by-nc-sa",
        "cc-by-nd",
        "cc-by-nc-nd",
    }
)


def is_cc_license(license_str: str | None) -> bool:
    """Return True iff ``license_str`` names a CC license we accept.

    Case-insensitive; tolerates ``None`` / empty (returns False). Used
    by every provider's search-time filter — keep the comparison in one
    place so a typo can't open a back door to ARR ingest.
    """
    if not license_str:
        return False
    return license_str.strip().lower() in CC_LICENSES


@dataclass(frozen=True)
class StreamingTrack:
    """A track surfaced by a streaming provider's search endpoint.

    The fields mirror what :class:`copilot.library.TrackRef` needs to
    persist a row, minus the analysis-derived fields (BPM, key, energy,
    beat-grid, downbeats) which are deferred to lazy analysis on first
    deck-load — the catalog browse path is search-driven, not analysis-
    driven, so we don't pay for analysis on every search hit.

    Attributes:
        id: Provider-scoped track identifier. Combined with the
            provider name to form the library ``track_id``
            (``"<provider>:<id>"``).
        title: Display title.
        artist: Display artist string. Some providers split
            featured-artist out; the client folds them into one string.
        duration_s: Track length in seconds. The provider's reported
            value (we trust it for catalog browse; lazy analyzer will
            re-measure on deck load).
        key: Camelot key string (``"8B"``, etc.) — provider-supplied
            when available, ``None`` otherwise. SoundCloud doesn't
            publish musical-key metadata so this is always ``None`` for
            that provider; the lazy analyzer fills it on first load.
        genre: Display genre string. Free-form; provider-defined
            vocabulary.
        license: CC license identifier (lowercased — one of
            :data:`CC_LICENSES`). Required by the provider's search-time
            filter; an ingested row without a CC license is a bug.
        stream_url: HTTP(S) URL the engine can decode from. Some
            providers (SoundCloud) require a follow-up ``resolve`` call
            with the client_id to mint a playable URL — the client
            handles that during search so the wire shape is uniform.
    """

    id: str
    title: str
    artist: str
    duration_s: float
    key: str | None
    genre: str
    license: str
    stream_url: str


class StreamingProvider(ABC):
    """Abstract base class for a streaming-source provider client.

    Concrete subclasses (``SoundCloudClient``, future Beatport /
    Mixcloud / Jamendo) implement :meth:`search` + :meth:`resolve_stream_url`.
    The ABC keeps the RPC dispatch table provider-agnostic — the wire
    surface (``streaming.search`` / ``streaming.add_to_library``) takes a
    ``provider`` field and the handler looks up the matching client by
    name.
    """

    #: Provider name as it appears on the wire (``"soundcloud"``,
    #: ``"beatport"``, ...). Lowercase, no spaces — used to namespace
    #: library ``track_id`` and ``source`` column.
    name: str

    @abstractmethod
    def search(
        self, query: str, limit: int = 20
    ) -> list[StreamingTrack]:
        """Search the provider catalog for tracks matching ``query``.

        Must filter results to CC-licensed tracks **before** returning —
        callers (the RPC layer + library ingest) trust the gate.

        Args:
            query: User-supplied free-text query.
            limit: Max results to return; provider may clamp.

        Returns:
            A list of :class:`StreamingTrack`, possibly empty.

        Raises:
            StreamingAuthError: API credentials missing / invalid.
            StreamingApiError: Provider returned a non-2xx / malformed
                response; details in the exception message.
        """

    @abstractmethod
    def resolve_stream_url(self, track_id: str) -> str:
        """Return a fresh, decoder-ready HTTP URL for ``track_id``.

        Some providers (SoundCloud) mint short-lived URLs that need to
        be re-resolved per playback session. The engine asks the
        library, which proxies to the provider, on every ``DeckLoad``
        of a streaming row.

        Args:
            track_id: The provider-scoped id (the ``id`` field on
                :class:`StreamingTrack`, **not** the prefixed library
                ``track_id``).

        Returns:
            A fully-qualified HTTP(S) URL.

        Raises:
            StreamingAuthError: API credentials missing / invalid.
            StreamingApiError: Track unavailable / removed / geo-blocked.
        """


class StreamingError(Exception):
    """Base exception for streaming-provider failures."""


class StreamingAuthError(StreamingError):
    """API credentials missing or rejected by the provider.

    Surfaced at construction time when the env var is unset, or at call
    time when the provider returns 401 / 403. The RPC layer maps this
    to a JSON-RPC ``-32000`` (feature-not-installed) so the UI can
    show a "configure SoundCloud" affordance with the apply-for-key
    link.
    """


class StreamingApiError(StreamingError):
    """The provider returned a non-2xx response or unparseable body.

    Distinct from :class:`StreamingAuthError` so the UI can decide
    whether to retry (transient API error) or re-prompt for creds (auth
    failure).
    """


__all__ = [
    "CC_LICENSES",
    "is_cc_license",
    "StreamingTrack",
    "StreamingProvider",
    "StreamingError",
    "StreamingAuthError",
    "StreamingApiError",
]
