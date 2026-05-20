# hypehouse-live — top-level Makefile.
#
# Thin wrappers over the per-subtree commands. Keep targets dumb so
# everything stays trivially reproducible by humans on the CLI.
#
# Requires:
#   * cargo + rustup with toolchain 1.88 (engine + tauri)
#   * node + npm (for the UI)
#   * `cargo install tauri-cli@2` once per workstation for `dev-tauri` / `build-tauri`.

.PHONY: help build-engine build-ui dev-tauri build-tauri test-engine test-tauri test-ui clean bake-in bake-in-tests supabase-print cloud-sync-status

# v0.2 bake-in defaults — 25-minute sanity run. Bump DURATION_MIN=240
# for the four-hour soak the release checklist demands.
DURATION_MIN ?= 25
BAKE_OUT_DIR ?= $(CURDIR)/bake-in-out
BAKE_COUNT   ?= 50
BAKE_PLAYLIST ?= 30
PYTHON       ?= python3

help:
	@echo "hypehouse-live Make targets:"
	@echo "  make build-engine   — cargo build --release on engine/"
	@echo "  make build-ui       — npm install + npm run build on ui/"
	@echo "  make dev-tauri      — cargo tauri dev (live-reload window + vite + engine sidecar)"
	@echo "  make build-tauri    — cargo tauri build (production desktop binary)"
	@echo "  make test-engine    — cargo test --all-targets on engine/"
	@echo "  make test-tauri     — cargo test --all-targets on tauri/"
	@echo "  make test-ui        — npm run test on ui/"
	@echo "  make bake-in        — full synthetic bake-in (default 25 min)"
	@echo "  make bake-in DURATION_MIN=240   — full 4-hour soak"
	@echo "  make bake-in-tests  — pytest the bake-in harness scripts only"
	@echo "  make supabase-print — print cloud-sync schema migrations + setup steps"
	@echo "  make cloud-sync-status — print library + pending-push counts (ops monitoring)"
	@echo "  make clean          — clean cargo + node build artifacts"

build-engine:
	cd engine && cargo build --release

build-ui:
	cd ui && npm install && npm run build

dev-tauri: build-engine
	cd tauri && cargo tauri dev

# build-tauri triggers the UI build via the `beforeBuildCommand` in
# tauri.conf.json — no separate `make build-ui` needed first.
build-tauri: build-engine
	cd tauri && cargo tauri build

test-engine:
	cd engine && cargo test --all-targets

test-tauri:
	cd tauri && cargo test --all-targets

test-ui:
	cd ui && npm run test

clean:
	cd engine && cargo clean
	cd tauri && cargo clean
	rm -rf ui/dist ui/node_modules

# Bake-in: generate synthetic tracks → drive engine + copilot → verify.
# The harness assumes the engine release binary exists; we depend on
# build-engine so a fresh checkout `make bake-in` just works. Catalog +
# session artifacts land under $(BAKE_OUT_DIR) (default ./bake-in-out)
# so they're easy to bundle as a CI artifact.
bake-in: build-engine
	mkdir -p $(BAKE_OUT_DIR)
	$(PYTHON) -m scripts.bake_in.generate_tracks \
		--out-dir $(BAKE_OUT_DIR) \
		--count $(BAKE_COUNT) \
		--duration-min $(DURATION_MIN)
	$(PYTHON) -m scripts.bake_in.run_set \
		--out-dir $(BAKE_OUT_DIR) \
		--manifest $(BAKE_OUT_DIR)/manifest.json \
		--duration-min $(DURATION_MIN) \
		--playlist-len $(BAKE_PLAYLIST)
	$(PYTHON) -m scripts.bake_in.verify \
		--report $(BAKE_OUT_DIR)/run_report.json \
		--summary-out $(BAKE_OUT_DIR)/verify_summary.json

# Pytest sub-target — just the harness smoke tests; doesn't build the
# engine or run the bake itself. Fast (< 5 s) and safe to wire into CI.
bake-in-tests:
	$(PYTHON) -m pytest scripts/bake_in/tests -q

# Print cloud-sync migrations + paste-ready setup instructions. Used
# by operators who don't have the supabase CLI installed — pipe to
# `pbcopy` (macOS) / `wl-copy` (Linux) then paste into the project's
# SQL editor. See docs/cloud-sync.md for the full guide.
supabase-print:
	$(PYTHON) scripts/print_supabase_migrations.py

# Print cloud-sync queue status for ops monitoring. Stdlib only;
# opens the library DB read-only. Use `--json` for machine output,
# or pass an explicit path:
#   make cloud-sync-status DB=/path/to/library.db
# Default DB path matches the co-pilot launchd plist
# (~/.config/hypehouse-live/library.db, overridable via
# $HYPEHOUSE_LIBRARY_DB).
cloud-sync-status:
	$(PYTHON) scripts/cloud_sync_status.py $(if $(DB),"$(DB)")
