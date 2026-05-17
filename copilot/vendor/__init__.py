"""Vendored HypeHouse v1 modules.

The files in this package (``analyzer.py``, ``mashup.py``, ``shared_cache.py``)
are byte-identical copies of the upstream HypeHouse v1 sources. They MUST NOT
be edited in tree — see ``VENDOR.md`` for the rule and the source commit SHAs.

Upstream uses absolute imports rooted at the v1 package layout
(``from src.shared_cache import ...``) and relative imports between sibling
modules (``from .mixer import BEATS_PER_BAR, SR, ...``). To keep the vendored
files literally unchanged, we register import-time shims here so the v1 code
resolves cleanly inside the ``copilot.vendor`` package:

* ``src`` and ``src.shared_cache`` are aliased to ``copilot.vendor`` and
  ``copilot.vendor.shared_cache`` so ``analyzer.py``'s
  ``from src.shared_cache import Cache, get_cache`` resolves.
* ``copilot.vendor.mixer`` is a minimal stub exposing the constants and the
  two private helpers that ``mashup.py`` imports at module load. The full
  ``smart_mashup`` pipeline isn't exercised by the co-pilot v0.1 service
  (live mixing happens in the Rust engine), so the stub is sufficient for
  import-time satisfaction. If you ever need real ``smart_mashup`` behavior
  inside the co-pilot, replace these shims with a properly vendored
  ``mixer.py`` and ``pipeline.py`` — do NOT edit the vendored files.
"""
from __future__ import annotations

import sys
import types

# --- shim 1: src.shared_cache  → copilot.vendor.shared_cache
#
# Done before the in-package modules are imported so ``analyzer.py``'s
# ``from src.shared_cache import Cache, get_cache`` resolves.
if "src" not in sys.modules:
    _src_pkg = types.ModuleType("src")
    _src_pkg.__path__ = []  # mark as package
    sys.modules["src"] = _src_pkg

from . import shared_cache as _vendor_shared_cache  # noqa: E402

sys.modules.setdefault("src.shared_cache", _vendor_shared_cache)

# --- shim 2: copilot.vendor.mixer
#
# ``mashup.py`` does ``from .mixer import BEATS_PER_BAR, SR, _crossfade_segment,
# _loudness_match`` at module load. Co-pilot v0.1 never calls smart_mashup, so
# the stubs just need to exist and have compatible signatures.
_mixer = types.ModuleType("copilot.vendor.mixer")
_mixer.BEATS_PER_BAR = 4
_mixer.SR = 44100


def _crossfade_segment(a, b):  # pragma: no cover — stub, not exercised in v0.1
    raise NotImplementedError(
        "copilot.vendor.mixer._crossfade_segment is a stub. The co-pilot v0.1 "
        "service doesn't run smart_mashup; if you need it, vendor v1's mixer.py."
    )


def _loudness_match(seg, target=0.15):  # pragma: no cover — stub
    raise NotImplementedError(
        "copilot.vendor.mixer._loudness_match is a stub. The co-pilot v0.1 "
        "service doesn't run smart_mashup; if you need it, vendor v1's mixer.py."
    )


_mixer._crossfade_segment = _crossfade_segment
_mixer._loudness_match = _loudness_match
sys.modules["copilot.vendor.mixer"] = _mixer

__all__ = ["analyzer", "mashup", "shared_cache"]
