# Atelier rig commands. Pre-implementation; harness build itself not yet wired.
#
# Common usage:
#   make check         — meta + artifact validation + rig self-tests + dry-run all tasks
#   make schemas       — meta-validate every schema in schemas/
#   make artifacts     — validate every artifact against its declared schema
#   make rig-tests     — pytest the rig itself (validators, runner, schema regression)
#   make dry-run       — run the workload runner in --dry-run mode against all tasks
#   make summary       — one-line summary of dry-run for all tasks
#   make install-rig   — create .venv/ and install the rig dependencies into it
#   make quality-cheap — cargo-audit + cargo-machete (low-cost supply-chain + dead-dep gate)
#   make release-cli   — build a local release CLI binary
#   make release-gui   — build local unsigned Tauri GUI bundles
#   make clean         — remove pytest cache and pycache from the tree

# Prefer the project-local venv created by `make install-rig` when it exists.
# Falls back to system python3 (e.g. on CI, which manages its own environment
# and installs the rig deps directly — see .github/workflows/check.yml).
VENV_PY := .venv/bin/python
ifneq ($(wildcard $(VENV_PY)),)
PY ?= $(VENV_PY)
else
PY ?= python3
endif

.PHONY: check schemas artifacts rig-tests dry-run summary install-rig quality-cheap audit audit-install npm-ioc-sweep release-cli release-gui clean

check: schemas artifacts rig-tests summary

schemas:
	$(PY) tests/validate_schemas.py

artifacts:
	$(PY) tests/validate_artifacts.py

rig-tests:
	$(PY) -m pytest tests/test_schemas.py tests/test_validators.py tests/test_runner.py tests/test_ci.py tests/test_npm_ioc_sweep.py -q

dry-run:
	$(PY) tests/workload/runner/runner.py --task all --dry-run

summary:
	$(PY) tests/workload/runner/runner.py --task all --dry-run --summary

install-rig:
	@# Homebrew/system Pythons on macOS are PEP-668 externally-managed, which
	@# blocks a plain `pip install --user`. We sidestep that by always installing
	@# into a project-local venv. After this target, `make check` etc. pick up
	@# .venv/bin/python automatically via the VENV_PY detection above.
	test -x $(VENV_PY) || python3 -m venv .venv
	$(VENV_PY) -m pip install --upgrade pip
	$(VENV_PY) -m pip install ".[rig]"

# Low-cost supply-chain + dead-dep gate. Runs in a few seconds against
# Cargo.lock. Wired into CI (`.github/workflows/check.yml`).
#
# cargo-audit scope: workspace Cargo.lock vs RustSec advisory DB.
# cargo-machete scope: `crates/` only — experiments/rmcp_spike is
# exploratory and isn't a workspace member.
#
# Audit ignores (`--ignore` flags below). Each ignore needs a written
# reason and a removal trigger; drop the flag when the trigger fires:
#   RUSTSEC-2026-0009 — `time` DoS via stack exhaustion (medium, 6.8).
#     Fix lives in `time >= 0.3.47`, which requires rustc 1.88.
#     Workspace is pinned to rustc 1.85 via rust-toolchain.toml.
#     Exposure for atelier is theoretical: affected versions reach us
#     only through Tauri transitives (cookie/plist/serde_with), and
#     atelier-gui renders trusted local UI exclusively. Remove this
#     ignore when the toolchain pin moves to >= 1.88.
quality-cheap:
	@command -v cargo-audit >/dev/null 2>&1 || { \
		echo "cargo-audit missing; install with: cargo install --locked cargo-audit"; exit 1; }
	@command -v cargo-machete >/dev/null 2>&1 || { \
		echo "cargo-machete missing; install with: cargo install --locked --version 0.7.0 cargo-machete"; exit 1; }
	cargo audit --ignore RUSTSEC-2026-0009
	cargo machete crates/

# Full supply-chain gate (v60.35 M27). Stricter than `quality-cheap`:
#   * `cargo audit --deny warnings` fails on any advisory that isn't
#     explicitly ignored — unmaintained / informational rows included.
#   * `npm audit --audit-level=high` covers the atelier-gui Svelte
#     deps that `quality-cheap` doesn't touch.
# Both gates must exit 0. CONTRIBUTING.md requires this target before
# opening a PR; CI runs it via `.github/workflows/check.yml`.
#
# The `--ignore` set carries the same documented exemptions as
# `quality-cheap` (`RUSTSEC-2026-0009`) plus the long-tail unmaintained
# rows that reach us only through Tauri's GTK3 transitives (gtk-rs
# family, atk, gdk*, soup2, glib, javascriptcore-rs, webkit2gtk family,
# pango, cairo-rs, gio, gdk-pixbuf, libappindicator) and a few orphaned
# proc-macro / util crates (instant, paste, proc-macro-error,
# unic-char-property/range/runtime_macros, derivative, fxhash, lru
# 0.12.5's unsound IterMut). Removal triggers:
#   * The gtk-rs / Tauri-transitive rows lift when Tauri publishes a
#     GTK4 backend or the workspace moves off `atelier-gui` on Linux.
#   * `RUSTSEC-2026-0009` lifts when the workspace rustc pin moves to
#     >= 1.88 (see `quality-cheap` comment for the long form).
#   * Each unmaintained shim lifts when its upstream is retired or
#     when the depending crate cuts a release that no longer pulls it.
audit:
	@command -v cargo-audit >/dev/null 2>&1 || { \
		echo "cargo-audit missing; install with: make audit-install"; exit 1; }
	cargo audit --deny warnings \
		--ignore RUSTSEC-2024-0370 \
		--ignore RUSTSEC-2024-0384 \
		--ignore RUSTSEC-2024-0411 \
		--ignore RUSTSEC-2024-0412 \
		--ignore RUSTSEC-2024-0413 \
		--ignore RUSTSEC-2024-0414 \
		--ignore RUSTSEC-2024-0415 \
		--ignore RUSTSEC-2024-0416 \
		--ignore RUSTSEC-2024-0417 \
		--ignore RUSTSEC-2024-0418 \
		--ignore RUSTSEC-2024-0419 \
		--ignore RUSTSEC-2024-0420 \
		--ignore RUSTSEC-2024-0429 \
		--ignore RUSTSEC-2024-0436 \
		--ignore RUSTSEC-2025-0075 \
		--ignore RUSTSEC-2025-0080 \
		--ignore RUSTSEC-2025-0081 \
		--ignore RUSTSEC-2025-0098 \
		--ignore RUSTSEC-2025-0100 \
		--ignore RUSTSEC-2026-0002 \
		--ignore RUSTSEC-2026-0009
	cd crates/atelier-gui/ui && npm audit --audit-level=high
	$(MAKE) npm-ioc-sweep

# v60.40 — Shai-Hulud / npm supply-chain IoC sweep.
#
# Three cheap, sub-second checks covering the worm's mechanical
# footholds (see tasks/shai_hulud_sweep_2026-05-19.md):
#
#   1. No `shai-hulud-workflow.yml` GH Actions file anywhere in tree.
#   2. No `preinstall` / `postinstall` script in any lockfile entry.
#   3. Every npm `resolved` URL points at `registry.npmjs.org`.
#
# Folds into `make audit` (so the per-PR `audit` job picks it up) and
# is also callable standalone. Runs Python only — no jq, no npm. Safe
# to run without `node_modules/` present because it reads the lockfile
# directly.
npm-ioc-sweep:
	$(PY) scripts/npm_ioc_sweep.py

release-cli:
	cargo build --locked --release -p atelier-cli

release-gui:
	npm --prefix crates/atelier-gui/ui ci
	cd crates/atelier-gui && cargo tauri build

audit-install:
	cargo install cargo-audit --locked

clean:
	find . -type d -name "__pycache__" -prune -exec rm -rf {} +
	find . -type d -name ".pytest_cache" -prune -exec rm -rf {} +
