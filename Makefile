# hypehouse-live — top-level Makefile.
#
# Thin wrappers over the per-subtree commands. Keep targets dumb so
# everything stays trivially reproducible by humans on the CLI.
#
# Requires:
#   * cargo + rustup with toolchain 1.88 (engine + tauri)
#   * node + npm (for the UI)
#   * `cargo install tauri-cli@2` once per workstation for `dev-tauri` / `build-tauri`.

.PHONY: help build-engine build-ui dev-tauri build-tauri test-engine test-tauri test-ui clean

help:
	@echo "hypehouse-live Make targets:"
	@echo "  make build-engine   — cargo build --release on engine/"
	@echo "  make build-ui       — npm install + npm run build on ui/"
	@echo "  make dev-tauri      — cargo tauri dev (live-reload window + vite + engine sidecar)"
	@echo "  make build-tauri    — cargo tauri build (production desktop binary)"
	@echo "  make test-engine    — cargo test --all-targets on engine/"
	@echo "  make test-tauri     — cargo test --all-targets on tauri/"
	@echo "  make test-ui        — npm run test on ui/"
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
