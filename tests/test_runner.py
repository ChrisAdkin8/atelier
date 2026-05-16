"""Tests for the workload runner.

Imports runner.py as a module (via importlib) and exercises its internals
against the real canonical fixtures plus a few synthetic harness commands.
"""
import importlib.util
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUNNER = ROOT / "tests" / "workload" / "runner" / "runner.py"


def _import_runner():
    spec = importlib.util.spec_from_file_location("runner", RUNNER)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# ---- load_task ----

def test_load_task_returns_priority_task():
    rn = _import_runner()
    t = rn.load_task("t01")
    assert t["task_id"] == "t01_add_pure_function"
    assert t["meta"]["priority"] is True
    assert t["meta"]["expected_starting_returncode"] == 5
    assert len(t["checks"]) >= 1


def test_load_task_unknown_raises():
    rn = _import_runner()
    try:
        rn.load_task("t99")
    except SystemExit as e:
        assert "not found" in str(e)
    else:
        raise AssertionError("expected SystemExit")


# ---- extract_meta ----

def test_extract_meta_finds_block():
    rn = _import_runner()
    stdout = 'some text\n<<<atelier-meta>>>{"turn_count": 3}<<<end>>>\nmore text'
    payload, violation = rn.extract_meta(stdout)
    assert payload == {"turn_count": 3}
    # violation is None when payload validates (or when jsonschema isn't importable)
    assert violation is None


def test_extract_meta_returns_none_when_absent():
    rn = _import_runner()
    payload, violation = rn.extract_meta("just some plain text")
    assert payload is None and violation is None


def test_extract_meta_reports_json_parse_error():
    rn = _import_runner()
    stdout = "<<<atelier-meta>>>{not valid<<<end>>>"
    payload, violation = rn.extract_meta(stdout)
    assert payload is None
    assert violation is not None and "not valid JSON" in violation


def test_extract_meta_reports_schema_violation():
    rn = _import_runner()
    # 'turn_count' must be non-negative integer; -5 fails the sentinel schema's minimum
    stdout = '<<<atelier-meta>>>{"turn_count": -5}<<<end>>>'
    payload, violation = rn.extract_meta(stdout)
    assert payload == {"turn_count": -5}
    assert violation is not None and "sentinel validation" in violation


# ---- run_check ----

def test_run_check_command_exit_code_match(tmp_path):
    rn = _import_runner()
    chk = {"name": "true exits 0", "command": "true", "expect": {"exit_code": 0}}
    r = rn.run_check(chk, tmp_path, tmp_path)
    assert r["ok"] is True
    assert r["exit_code"] == 0


def test_run_check_command_exit_code_mismatch(tmp_path):
    rn = _import_runner()
    chk = {"name": "false exits non-zero", "command": "false", "expect": {"exit_code": 0}}
    r = rn.run_check(chk, tmp_path, tmp_path)
    assert r["ok"] is False
    assert "exit_code" in r["reason"]


def test_run_check_file_unchanged_passes(tmp_path):
    rn = _import_runner()
    src = tmp_path / "src"
    dst = tmp_path / "dst"
    src.mkdir(); dst.mkdir()
    (src / "f.txt").write_text("hello")
    (dst / "f.txt").write_text("hello")
    r = rn.run_check({"name": "f", "file_unchanged": "f.txt"}, dst, src)
    assert r["ok"] is True


def test_run_check_file_unchanged_detects_modification(tmp_path):
    rn = _import_runner()
    src = tmp_path / "src"
    dst = tmp_path / "dst"
    src.mkdir(); dst.mkdir()
    (src / "f.txt").write_text("hello")
    (dst / "f.txt").write_text("hello, world")
    r = rn.run_check({"name": "f", "file_unchanged": "f.txt"}, dst, src)
    assert r["ok"] is False
    assert "contents differ" in r["reason"]


def test_run_check_file_unchanged_detects_deletion(tmp_path):
    rn = _import_runner()
    src = tmp_path / "src"
    dst = tmp_path / "dst"
    src.mkdir(); dst.mkdir()
    (src / "f.txt").write_text("hello")
    # dst missing f.txt
    r = rn.run_check({"name": "f", "file_unchanged": "f.txt"}, dst, src)
    assert r["ok"] is False
    assert "deleted by agent" in r["reason"]


def test_run_check_stdout_pattern(tmp_path):
    rn = _import_runner()
    chk = {"name": "echo", "command": "echo hello-world", "expect": {"exit_code": 0, "stdout_pattern": "hello-\\w+"}}
    r = rn.run_check(chk, tmp_path, tmp_path)
    assert r["ok"] is True


def test_run_check_stderr_contains(tmp_path):
    rn = _import_runner()
    chk = {"name": "err", "command": "python3 -c 'import sys; sys.stderr.write(\"ValueError: bad input\")'", "expect": {"exit_code": 0, "stderr_contains": "ValueError"}}
    r = rn.run_check(chk, tmp_path, tmp_path)
    assert r["ok"] is True


# ---- end-to-end runner invocation ----

def test_runner_dry_run_all_tasks_passes():
    """Every canonical fixture's starting state matches its declared expected_starting_returncode."""
    r = subprocess.run(
        [sys.executable, str(RUNNER), "--task", "all", "--dry-run", "--summary"],
        cwd=ROOT, capture_output=True, text=True,
    )
    assert r.returncode == 0, f"stderr: {r.stderr}\nstdout: {r.stdout}"
    lines = [l for l in r.stdout.splitlines() if l.startswith(("OK  ", "FAIL"))]
    fails = [l for l in lines if l.startswith("FAIL")]
    assert not fails, f"expected zero failures, got: {fails}"
    assert len(lines) >= 10, f"expected at least 10 tasks summarised, got {len(lines)}: {r.stdout}"


def test_runner_catches_no_op_harness_on_t05():
    """A do-nothing harness on t05 must fail at least one check (the bug remains)."""
    r = subprocess.run(
        [sys.executable, str(RUNNER), "--task", "t05", "--harness-cmd", "true", "--summary"],
        cwd=ROOT, capture_output=True, text=True,
    )
    assert r.returncode != 0
    assert "FAIL" in r.stdout


def test_runner_catches_no_op_harness_on_t07():
    """t07 starts at rc=0 (do-nothing exploit candidate); the callable-count check must catch it."""
    r = subprocess.run(
        [sys.executable, str(RUNNER), "--task", "t07", "--harness-cmd", "true", "--summary"],
        cwd=ROOT, capture_output=True, text=True,
    )
    assert r.returncode != 0
    assert "FAIL" in r.stdout


# ---- checks.json hygiene (regression guard for B1) ----

def test_tool_name_mentions_resolve():
    """Doc-drift guard: backticked file/dir-shaped tool-name mentions in
    bundled tool-manifest descriptions must resolve to an actual manifest under
    `crates/atelier-core/tools/`.

    Regression target: when v21 removed `delete_file.v1.json`, three other
    manifests' descriptions still referenced `delete_file`. The lint inspects
    every `description` string inside every bundled manifest, finds backticked
    identifiers shaped like `*_file` or `*_dir`, and asserts each resolves to
    a real tool name.

    Why this narrow shape? It matches the canonical bug (`delete_file`) while
    skipping field names like `old_text`, `expected_count`, `subagent_type`,
    which are JSON-Schema property identifiers, not tool references. Bare
    tool names (`shell`, `grep`) are fundamental enough that removal/rename
    would be a deliberate, wide-visibility change — out of scope for this
    lint.
    """
    tools_dir = ROOT / "crates" / "atelier-core" / "tools"
    manifests = sorted(tools_dir.glob("*.v1.json"))
    assert manifests, "no built-in tool manifests found"

    real_names = {json.loads(m.read_text())["name"] for m in manifests}
    candidate = re.compile(r"`([a-z][a-z_]*(?:_file|_dir))`")

    offenders = []
    for manifest_path in manifests:
        manifest = json.loads(manifest_path.read_text())
        descriptions = []
        def collect(node):
            if isinstance(node, dict):
                if "description" in node and isinstance(node["description"], str):
                    descriptions.append(node["description"])
                for v in node.values():
                    collect(v)
            elif isinstance(node, list):
                for item in node:
                    collect(item)
        collect(manifest)

        for desc in descriptions:
            for word in candidate.findall(desc):
                if word == manifest["name"] or word in real_names:
                    continue
                offenders.append(f"{manifest_path.name}: `{word}` does not resolve")
    assert not offenders, "stale tool-name references: " + "; ".join(offenders)


def test_no_claude_paths_in_tracked_source():
    """Directive: Atelier uses .atelier/ paths, never .claude/. See
    .atelier/memory/feedback_atelier_path_directive.md.

    Tracked source files must not introduce new `.claude/` references. Allowed
    entries below are an *exact-match* set — adding a new file that legitimately
    needs to mention `.claude/` requires adding it here by name (no glob).
    `.atelier/settings.local.json` is intentionally NOT in this list: it's now
    gitignored (per-user, regenerated locally) so the lint never sees it.

    Allowed paths and why each gets a pass:
      .gitignore                                         — the `.claude/` exclusion lives here
      CHANGELOG.md                                       — historical record of the directive
      ATELIER.md                                         — documents the harness-shim exception
      .atelier/README.md                                 — documents the directory layout incl. shims
      .atelier/docs/memory-system.md                     — describes the harness's memory-preload symlink
      .atelier/memory/feedback_config_scope.md           — predecessor feedback memory
      .atelier/memory/feedback_atelier_path_directive.md — the directive itself
      .atelier/memory/MEMORY.md                          — memory index, echoes the entry descriptions
      tests/test_runner.py                               — this file (the allowlist)
      tests/README.md                                    — documents this lint
      coding-harness-spec.md                             — spec records the §8 baseline choice
      tasks/todo.md                                      — echoes spec state
    """
    allowed = {
        ".gitignore",
        "CHANGELOG.md",
        "ATELIER.md",
        ".atelier/README.md",
        ".atelier/docs/memory-system.md",
        ".atelier/memory/feedback_config_scope.md",
        ".atelier/memory/feedback_atelier_path_directive.md",
        ".atelier/memory/MEMORY.md",
        "tests/test_runner.py",
        "tests/README.md",
        "coding-harness-spec.md",
        "tasks/todo.md",
    }
    offenders = []
    for path in ROOT.rglob("*"):
        # Skip symlinks: they're harness-required shims (e.g., CLAUDE.md →
        # ATELIER.md, .claude/settings.json → ../.atelier/settings.json). The
        # actual content lives at the target and is scanned via its real path.
        if path.is_symlink():
            continue
        if not path.is_file():
            continue
        rel = path.relative_to(ROOT).as_posix()
        # Skip excluded dirs that aren't part of tracked source
        if any(part in {".git", "target", "node_modules", ".pytest_cache", "__pycache__", ".atelier-bin"} for part in path.parts):
            continue
        if rel in allowed:
            continue
        # Only scan text-like extensions
        if path.suffix not in {".md", ".json", ".py", ".rs", ".yml", ".yaml", ".toml", ".sh", ""}:
            continue
        try:
            text = path.read_text()
        except (UnicodeDecodeError, PermissionError):
            continue
        if ".claude" in text or "claudeignore" in text:
            offenders.append(rel)
    assert not offenders, (
        "tracked source contains `.claude` references outside the documented "
        "allowlist. If a new entry is genuinely needed, extend the `allowed` set "
        f"in this test with a rationale. Offenders: {offenders}"
    )


def test_checks_commands_do_not_reference_fixture_prefix():
    """Lint: no check command may reference 'fixture/' — the runner copies fixture
    *contents* flat into the workdir, so a 'fixture/foo' path becomes a spurious
    FileNotFoundError instead of a meaningful check failure.

    Regression guard for the original t03 bug.
    """
    tasks_dir = ROOT / "tests" / "workload" / "canonical"
    offenders = []
    for task_dir in sorted(tasks_dir.iterdir()):
        if not task_dir.is_dir():
            continue
        checks_path = task_dir / "checks.json"
        if not checks_path.is_file():
            continue
        checks = json.loads(checks_path.read_text()).get("checks", [])
        for chk in checks:
            cmd = chk.get("command", "")
            if "fixture/" in cmd:
                offenders.append(f"{task_dir.name}: {chk['name']!r}")
    assert not offenders, "checks reference 'fixture/' prefix: " + "; ".join(offenders)


def test_runner_harness_smoke_all_tasks_emit_checks():
    """End-to-end smoke: every task's checks.json executes when --harness-cmd runs.

    Runs the runner against every task with a no-op harness ('true') and parses
    the JSON output. Asserts:
      - runner emitted JSON with one result per task (>= 11 tasks)
      - every task ran at least one check
      - every check has a 'kind' set (so run_check actually evaluated it)

    A regression like B1 (path bug → check evaluator crashes or skips) surfaces
    here because run_check would either not append a kind or the task would
    have an empty checks array.
    """
    r = subprocess.run(
        [sys.executable, str(RUNNER), "--task", "all", "--harness-cmd", "true", "--out", "/dev/stdout"],
        cwd=ROOT, capture_output=True, text=True,
    )
    # Runner exits non-zero because the no-op harness fails every task; that's expected.
    # We care that the JSON output is well-formed and complete.
    payload = json.loads(r.stdout)
    assert payload["runner_version"] == 1
    results = payload["results"]
    assert len(results) >= 11, f"expected >= 11 tasks, got {len(results)}"
    for res in results:
        assert res["mode"] == "harness"
        assert res.get("checks"), f"{res['task_id']}: empty checks array"
        for chk in res["checks"]:
            assert chk.get("kind") in ("command", "file_unchanged"), (
                f"{res['task_id']}: check {chk.get('name')!r} missing kind"
            )
