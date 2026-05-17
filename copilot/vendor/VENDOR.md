# Vendored HypeHouse v1 modules

The files in this directory are **byte-identical** copies of HypeHouse v1
sources. They are vendored here so the co-pilot service has the v1 analyzer
+ mashup primitives without taking a runtime dependency on the v1 repo.

## Rule

**Do not edit these files in tree.** Any fix is filed against HypeHouse v1
upstream and then re-vendored. Editing in-tree silently desyncs the two
repos and breaks the manifest below.

Adapter shims that smooth over import-path differences (e.g. v1's
``from src.shared_cache import ...``) live in
``copilot/vendor/__init__.py``, never inside the vendored modules.

## Source

Upstream repo: <https://github.com/sandy9214/HypeHouse>
Branch: ``main``
Vendored on: 2026-05-17

| File | Source path | Source commit | Upstream last-modified |
|------|-------------|---------------|------------------------|
| `analyzer.py` | `src/analyzer.py` | `ff2eb9bfc715a20f330e6dd5824098a1a3a33d4f` | 2026-05-17 11:59:36 -0400 |
| `mashup.py` | `src/mashup.py` | `7a773ae3c1b340d9803f1a54dd45c586db94a979` | 2026-05-17 11:37:34 -0400 |
| `shared_cache.py` | `src/shared_cache.py` | `ff2eb9bfc715a20f330e6dd5824098a1a3a33d4f` | 2026-05-17 11:59:36 -0400 |

(SHAs are the last commit touching the specific file; HypeHouse repo HEAD at
vendor time was `959af5cbb6a8ddda3563689e2a15a6150e33c7a3`.)

## Re-vendoring procedure

1. `cd ../../../HypeHouse && git pull`
2. Verify v1 tests still pass: `pytest`.
3. From this repo: `cp ../../HypeHouse/src/{analyzer,mashup,shared_cache}.py copilot/vendor/`
4. Bump the SHAs and "Vendored on" date in this file.
5. Run `pytest copilot/tests/` and `python -c "import copilot.vendor"` to confirm
   the shim layer in ``__init__.py`` still works.
6. If upstream added a new top-level import in any of these files (e.g. a new
   ``from .foo import bar``), either:
   - vendor ``foo.py`` too and add it to the manifest, **or**
   - add a matching stub in ``copilot/vendor/__init__.py``.
   Document the choice in the PR.

## Why not pip-install from git?

* Pinning a SHA in `pyproject.toml` works but couples co-pilot CI to the v1
  repo's network availability + private-repo auth in GHA.
* Vendoring keeps `pytest copilot/tests/` hermetic and the co-pilot service
  buildable offline.
* Trade-off: requires the re-vendor procedure above. Acceptable given the
  small surface (3 files, ~940 lines total).
