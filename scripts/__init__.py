"""Top-level ``scripts`` package for hypehouse-live operational tooling.

Sub-packages here are intentionally test-light: they wrap the engine +
copilot binaries from the outside so we can exercise the real wire
surface without re-vendoring engine internals into Python. The bake-in
harness (``scripts.bake_in``) is the first occupant — see its module
docstring for the v0.2 beta acceptance contract it implements.
"""
