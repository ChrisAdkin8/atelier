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

.PHONY: check schemas artifacts rig-tests dry-run summary install-rig quality-cheap clean

check: schemas artifacts rig-tests summary

schemas:
	$(PY) tests/validate_schemas.py

artifacts:
	$(PY) tests/validate_artifacts.py

rig-tests:
	$(PY) -m pytest tests/test_schemas.py tests/test_validators.py tests/test_runner.py tests/test_ci.py -q

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

clean:
	find . -type d -name "__pycache__" -prune -exec rm -rf {} +
	find . -type d -name ".pytest_cache" -prune -exec rm -rf {} +
