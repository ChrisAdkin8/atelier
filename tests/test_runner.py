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


def test_extract_meta_survives_partial_jsonschema_import(monkeypatch):
    """v60.36 H3 — if `import jsonschema` raises something other than
    ImportError (e.g. a broken editable install re-export), or the import
    succeeds but `validate` raises `SchemaError`, `extract_meta` must NOT
    crash the workload run. Before the fix the `except jsonschema.ValidationError`
    arm hit `NameError` because `jsonschema` was never bound.
    """
    import sys
    rn = _import_runner()
    stdout = '<<<atelier-meta>>>{"turn_count": 3}<<<end>>>'

    # Case 1: ImportError on import → returns payload + no violation.
    monkeypatch.setitem(sys.modules, "jsonschema", None)
    payload, violation = rn.extract_meta(stdout)
    assert payload == {"turn_count": 3}
    assert violation is None
    monkeypatch.delitem(sys.modules, "jsonschema", raising=False)

    # Case 2: import succeeds but `validate` raises `SchemaError` (broken
    # sentinel schema). Patch the real jsonschema with a stub that raises
    # SchemaError-compatible errors. Use the real jsonschema module's
    # SchemaError class so the except clause matches.
    import jsonschema as real_jsonschema
    class _StubJsonschema:
        ValidationError = real_jsonschema.ValidationError
        SchemaError = real_jsonschema.SchemaError
        def validate(self, payload, schema):  # noqa: D401
            raise real_jsonschema.SchemaError("intentionally broken sentinel")
    monkeypatch.setitem(sys.modules, "jsonschema", _StubJsonschema())
    payload, violation = rn.extract_meta(stdout)
    assert payload == {"turn_count": 3}
    assert violation is not None and "sentinel schema is broken" in violation


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
        # v60.40 — Shai-Hulud sweep record. References `.claude/worktrees/`
        # in the "out-of-scope checks" section because those agent
        # worktrees, when present, share lockfile content with the main
        # tree and need the same IoC battery. The string is descriptive,
        # not a code path the harness reads from.
        "tasks/shai_hulud_sweep_2026-05-19.md",
        # References `.claude/commands/` for competitive-survey comparison only;
        # not a code path the harness reads or writes.
        "tasks/plan_skills_implementation.md",
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
        # Skip excluded dirs that aren't part of tracked source.
        # `.claude` covers the harness-managed runtime tree (worktrees, plugins
        # cache, etc.) that the BYOM repo never tracks. The directive is about
        # *tracked source* — runtime trees are out of scope.
        if any(part in {".git", "target", "node_modules", ".pytest_cache", "__pycache__", ".atelier-bin", ".claude"} for part in path.parts):
            continue
        if rel in allowed:
            continue
        # v60.38 L5/RIG-L1 — inverted from include-list to skip-list.
        # The previous "scan only these suffixes" approach silently
        # skipped any new file type (e.g. .svelte, .ts, .tsx) — relevant
        # because the GUI is Svelte + TypeScript. New file types are now
        # scanned by default; opt out only for known-binary kinds.
        if path.suffix.lower() in {
            ".png", ".jpg", ".jpeg", ".gif", ".svg", ".ico",
            ".woff", ".woff2", ".ttf", ".otf",
            ".lock", ".pyc", ".so", ".dylib", ".dll", ".class",
            ".zip", ".gz", ".bz2", ".xz", ".tar",
            ".pdf", ".bin",
        }:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, PermissionError):
            # If the suffix wasn't on the binary skip-list but it's not
            # decodable as UTF-8, treat it as opaque and move on. Catches
            # the long tail of binary-ish files we didn't enumerate above.
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
        [sys.executable, str(RUNNER), "--task", "all", "--harness-cmd", "true"],
        cwd=ROOT, capture_output=True, text=True,
    )
    # Runner exits non-zero because the no-op harness fails every task; that's expected.
    # We care that the JSON output is well-formed and complete.
    # NOTE: no --out flag — letting the runner write to sys.stdout directly so
    # capture_output=True reliably intercepts it. --out /dev/stdout opens a
    # new file descriptor that doesn't reach the subprocess pipe on all platforms.
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


def test_run_with_pg_timeout_kills_grandchildren():
    """M11: a timeout must group-kill the whole subprocess tree, not just the
    direct child. Parent spawns a long-lived `sleep` grandchild, then hangs; the
    helper's timeout-path must reap both. We verify by checking the
    grandchild's PID no longer exists after the helper returns.
    """
    import os
    import platform
    import time
    import pytest

    if platform.system() == "Windows":
        pytest.skip("process-group kill is a Unix discipline")
    rn = _import_runner()

    with tempfile.TemporaryDirectory() as tmp:
        pidfile = Path(tmp) / "grandchild.pid"
        # Parent: spawn long-lived `sleep`, write its pid, then hang.
        parent = (
            "import os, subprocess, sys, time, pathlib;\n"
            f"pidpath = pathlib.Path({str(pidfile)!r});\n"
            "p = subprocess.Popen(['sleep', '120']);\n"
            "pidpath.write_text(str(p.pid));\n"
            "time.sleep(120)\n"
        )
        try:
            rn._run_with_pg_timeout(
                [sys.executable, "-c", parent], cwd=tmp, timeout_s=1
            )
        except subprocess.TimeoutExpired:
            pass
        else:
            raise AssertionError("expected TimeoutExpired")

        deadline = time.monotonic() + 5
        while not pidfile.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        assert pidfile.exists(), "grandchild never wrote its pidfile"
        grandchild_pid = int(pidfile.read_text())

        deadline = time.monotonic() + 5
        alive = True
        while time.monotonic() < deadline:
            try:
                os.kill(grandchild_pid, 0)
            except ProcessLookupError:
                alive = False
                break
            time.sleep(0.05)
        assert not alive, (
            f"grandchild pid={grandchild_pid} survived the parent's timeout — "
            "process-group kill leaked"
        )


def test_harness_run_timeout_surfaces_stderr(tmp_path):
    """v60.36 H4 — `harness_run` must surface BOTH stdout and stderr tails
    even when the subprocess times out. Before the fix, the harness-result
    dict dropped stderr entirely on timeout, leaving a "hung gate" with no
    debugging signal.
    """
    import platform
    import pytest

    if platform.system() == "Windows":
        pytest.skip("process-group kill is a Unix discipline; harness_run timeout uses it")
    rn = _import_runner()

    # Synthesise a minimal task that lives entirely in tmp_path.
    fixture = tmp_path / "fixture"
    fixture.mkdir()
    # The runner copies the fixture root into a workdir and runs the
    # harness with $task["prompt"] piped to stdin. Use a tiny meta.json
    # with the absent-test_command default; run_test_command(default)
    # will succeed trivially.
    (fixture / "meta.json").write_text('{"expected_starting_returncode": 0}', encoding="utf-8")

    task = {
        "task_id": "t-timeout-stderr",
        "fixture": str(fixture),
        "prompt": "ignored",
        "meta": {"expected_starting_returncode": 0},
        "checks": [],
    }
    # A harness that emits identifiable strings on both stdout and stderr,
    # then hangs forever. The 1-second timeout forces the harness_run
    # timeout branch.
    harness = (
        sys.executable
        + ' -c "import sys, time; '
        + 'sys.stdout.write(\\"OUT_MARKER\\"); sys.stdout.flush(); '
        + 'sys.stderr.write(\\"ERR_MARKER\\"); sys.stderr.flush(); '
        + 'time.sleep(60)"'
    )

    out = rn.harness_run(task, harness, timeout_s=1)
    assert out["harness"]["timed_out"] is True
    # H4: both fields populated post-timeout.
    assert "ERR_MARKER" in out["harness"]["stderr_tail"], (
        f"expected ERR_MARKER in stderr_tail, got {out['harness']['stderr_tail']!r}"
    )
    assert "OUT_MARKER" in out["harness"]["stdout_tail"], (
        f"expected OUT_MARKER in stdout_tail, got {out['harness']['stdout_tail']!r}"
    )


def test_atelier_sessions_is_gitignored():
    """`.atelier/sessions/` holds per-user runtime data (UUID-keyed dirs);
    one `git add .` after a local run must not stage it.

    Probes via `git check-ignore` against a synthetic path inside the dir.
    Skipped when not in a git work-tree.
    """
    in_git = subprocess.run(
        ["git", "rev-parse", "--is-inside-work-tree"],
        cwd=ROOT, capture_output=True, text=True,
    )
    if in_git.returncode != 0 or in_git.stdout.strip() != "true":
        import pytest
        pytest.skip("not inside a git work-tree")
    probe = ".atelier/sessions/00000000-0000-0000-0000-000000000000/turn_log.jsonl"
    r = subprocess.run(
        ["git", "check-ignore", "--no-index", probe],
        cwd=ROOT, capture_output=True, text=True,
    )
    assert r.returncode == 0, (
        f"expected `.atelier/sessions/` to be gitignored; "
        f"`git check-ignore {probe}` returned {r.returncode}, stdout={r.stdout!r}"
    )
