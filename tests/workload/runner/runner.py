#!/usr/bin/env python3
"""Run a canonical workload task against a harness, or validate fixture starting state.

Modes
-----
--dry-run:
  Copy the fixture to a temp dir; run the task's test_command; assert the returncode
  matches the task's meta.json `expected_starting_returncode`. Reports.

--harness-cmd CMD:
  Copy the fixture to a temp dir; invoke CMD ({dir} replaced with the temp path;
  prompt piped to stdin); run the test_command; run every check from checks.json;
  validate any <<<atelier-meta>>> stdout block against atelier_meta_sentinel.v1.json.

Output: single JSON object on stdout (or --out PATH), conforming to
        schemas/workload/runner_result.v1.json.

Exit code: zero iff every task passed.
"""

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent  # repo root
TASKS_DIR = Path(__file__).resolve().parent.parent / "canonical"
META_RE = re.compile(r"<<<atelier-meta>>>(.*?)<<<end>>>", re.DOTALL)
DEFAULT_HARNESS_TIMEOUT_S = 300  # PROVISIONAL; calibrate against canonical workload
DEFAULT_TEST_COMMAND_TIMEOUT_S = 120  # post-fixture pytest run; canonical tasks complete in <10s
DEFAULT_TEST_COMMAND = ["python3", "-m", "pytest", "--tb=short", "-q"]
SENTINEL_SCHEMA_PATH = ROOT / "schemas" / "workload" / "atelier_meta_sentinel.v1.json"


def list_tasks():
    return sorted(p.name for p in TASKS_DIR.iterdir() if p.is_dir() and p.name.startswith("t"))


def load_task(task_id):
    matches = [p for p in TASKS_DIR.iterdir() if p.is_dir() and p.name.startswith(task_id + "_")]
    if not matches:
        raise SystemExit(f"task {task_id!r} not found in {TASKS_DIR}")
    if len(matches) > 1:
        raise SystemExit(f"ambiguous task {task_id!r}: {[m.name for m in matches]}")
    task_dir = matches[0]
    fixture = task_dir / "fixture"
    meta_path = task_dir / "meta.json"
    if not fixture.is_dir():
        raise SystemExit(f"fixture dir missing: {fixture}")
    if not meta_path.is_file():
        raise SystemExit(f"meta.json missing: {meta_path}")
    meta = json.loads(meta_path.read_text(encoding="utf-8"))
    checks_path = task_dir / "checks.json"
    checks = json.loads(checks_path.read_text(encoding="utf-8"))["checks"] if checks_path.is_file() else []
    return {
        "task_id": task_dir.name,
        "dir": task_dir,
        "prompt": (task_dir / "prompt.md").read_text(encoding="utf-8"),
        "fixture": fixture,
        "meta": meta,
        "checks": checks,
    }


def copy_fixture(src, dst):
    shutil.copytree(src, dst, dirs_exist_ok=True)


def _kill_process_group(proc):
    """SIGKILL the process group on timeout so grandchildren don't leak.

    Mirrors the v25 P1 discipline in `crates/atelier-core/src/subprocess.rs`.
    `start_new_session=True` sets the child up as its own process-group leader;
    `os.killpg(os.getpgid(pid), SIGKILL)` then reaps the whole tree. The
    fallback `proc.kill()` covers Windows (where setsid doesn't apply) and the
    race where the child has already exited.
    """
    if proc.poll() is not None:
        return
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (ProcessLookupError, OSError, AttributeError):
        try:
            proc.kill()
        except OSError:
            pass


def _run_with_pg_timeout(argv, *, cwd, timeout_s, stdin_text=None):
    """`subprocess.run` equivalent that group-kills on timeout.

    Returns either a `subprocess.CompletedProcess`-shaped namespace
    (`returncode`, `stdout`, `stderr`) or raises `subprocess.TimeoutExpired`
    after killing the whole process group.
    """
    proc = subprocess.Popen(
        argv,
        cwd=cwd,
        stdin=subprocess.PIPE if stdin_text is not None else None,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, stderr = proc.communicate(input=stdin_text, timeout=timeout_s)
        return subprocess.CompletedProcess(argv, proc.returncode, stdout, stderr)
    except subprocess.TimeoutExpired:
        _kill_process_group(proc)
        try:
            stdout, stderr = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            stdout, stderr = "", ""
        raise subprocess.TimeoutExpired(argv, timeout_s, output=stdout, stderr=stderr)


def run_test_command(workdir, meta, timeout_s=DEFAULT_TEST_COMMAND_TIMEOUT_S):
    cmd = meta.get("test_command", DEFAULT_TEST_COMMAND)
    start = time.monotonic()
    try:
        result = _run_with_pg_timeout(cmd, cwd=workdir, timeout_s=timeout_s)
        return {
            "returncode": result.returncode,
            "elapsed_s": round(time.monotonic() - start, 3),
            "timed_out": False,
            "tail": result.stdout[-500:] if result.stdout else "",
            "stderr_tail": result.stderr[-200:] if result.stderr else "",
        }
    except subprocess.TimeoutExpired as e:
        partial = e.stdout or ""
        if isinstance(partial, bytes):
            partial = partial.decode("utf-8", "replace")
        return {
            "returncode": -1,
            "elapsed_s": round(time.monotonic() - start, 3),
            "timed_out": True,
            "tail": partial[-500:],
            "stderr_tail": f"test_command timed out after {timeout_s}s",
        }


def sha256_file(path):
    h = hashlib.sha256()
    h.update(path.read_bytes())
    return h.hexdigest()


def run_check(check, workdir, fixture_src):
    name = check["name"]
    if "file_unchanged" in check:
        rel = check["file_unchanged"]
        src = fixture_src / rel
        dst = workdir / rel
        if not src.is_file():
            return {"name": name, "ok": False, "kind": "file_unchanged",
                    "exit_code": None, "reason": f"baseline missing: {rel}"}
        if not dst.is_file():
            return {"name": name, "ok": False, "kind": "file_unchanged",
                    "exit_code": None, "reason": f"deleted by agent: {rel}"}
        if sha256_file(src) != sha256_file(dst):
            return {"name": name, "ok": False, "kind": "file_unchanged",
                    "exit_code": None, "reason": f"contents differ: {rel}"}
        return {"name": name, "ok": True, "kind": "file_unchanged", "exit_code": None, "reason": None}

    cmd = check["command"]
    expect = check["expect"]
    argv = shlex.split(cmd)
    try:
        result = _run_with_pg_timeout(argv, cwd=workdir, timeout_s=DEFAULT_TEST_COMMAND_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        return {
            "name": name,
            "ok": False,
            "kind": "command",
            "exit_code": None,
            "reason": f"check command timed out after {DEFAULT_TEST_COMMAND_TIMEOUT_S}s",
        }
    except OSError as e:
        return {
            "name": name,
            "ok": False,
            "kind": "command",
            "exit_code": None,
            "reason": f"check command not found or unrunnable: {e}",
        }
    rc = result.returncode
    failures = []
    if "exit_code" in expect and rc != expect["exit_code"]:
        failures.append(f"exit_code: want={expect['exit_code']} got={rc}")
    if "exit_code_ne" in expect and rc == expect["exit_code_ne"]:
        failures.append(f"exit_code_ne: rejected value {rc}")
    if "stdout_contains" in expect and expect["stdout_contains"] not in result.stdout:
        failures.append(f"stdout_contains: missing {expect['stdout_contains']!r}")
    if "stderr_contains" in expect and expect["stderr_contains"] not in result.stderr:
        failures.append(f"stderr_contains: missing {expect['stderr_contains']!r}")
    if "stdout_pattern" in expect and not re.search(expect["stdout_pattern"], result.stdout):
        failures.append(f"stdout_pattern: no match for {expect['stdout_pattern']!r}")
    if "stderr_pattern" in expect and not re.search(expect["stderr_pattern"], result.stderr):
        failures.append(f"stderr_pattern: no match for {expect['stderr_pattern']!r}")
    return {
        "name": name,
        "ok": not failures,
        "kind": "command",
        "exit_code": rc,
        "reason": "; ".join(failures) if failures else None,
    }


def extract_meta(stdout):
    if not stdout:
        return None, None
    match = META_RE.search(stdout)
    if not match:
        return None, None
    try:
        payload = json.loads(match.group(1))
    except json.JSONDecodeError as e:
        return None, f"atelier-meta block was not valid JSON: {e}"
    # v60.36 H3 — guard against the import succeeding partially (editable
    # install with a broken re-export → the `except jsonschema.ValidationError`
    # in the original single-try structure would raise NameError because
    # `jsonschema` was never bound, crashing the entire workload run). Two
    # nested try blocks: the import succeeds-or-skips first, then validation
    # gets a separate handler that also catches SchemaError so a malformed
    # sentinel schema can't kill the run either.
    try:
        import jsonschema
    except ImportError:
        return payload, None
    try:
        schema = json.loads(SENTINEL_SCHEMA_PATH.read_text(encoding="utf-8"))
        jsonschema.validate(payload, schema)
    except jsonschema.ValidationError as e:
        return payload, f"atelier-meta failed sentinel validation: {e.message}"
    except jsonschema.SchemaError as e:
        return payload, f"atelier-meta sentinel schema is broken: {e.message}"
    return payload, None


def dry_run(task):
    expected_rc = task["meta"]["expected_starting_returncode"]
    with tempfile.TemporaryDirectory(prefix=f"{task['task_id']}_") as tmp:
        copy_fixture(task["fixture"], tmp)
        pt = run_test_command(tmp, task["meta"])
        ok = pt["returncode"] == expected_rc
        return {
            "mode": "dry-run",
            "task_id": task["task_id"],
            "ok": ok,
            "expected_starting_returncode": expected_rc,
            "starting_state": pt,
            "divergence": None if ok else f"expected rc={expected_rc}, got rc={pt['returncode']}",
        }


def harness_run(task, harness_cmd, timeout_s):
    with tempfile.TemporaryDirectory(prefix=f"{task['task_id']}_") as tmp:
        copy_fixture(task["fixture"], tmp)
        cmd = harness_cmd.replace("{dir}", tmp)
        argv = shlex.split(cmd)
        start = time.monotonic()
        # v60.36 H4 — surface stdout AND stderr even on timeout. Before
        # this fix the harness-result dict dropped stderr entirely, so a
        # timed-out workload run produced an artifact with empty
        # `stdout_tail`, no stderr, and no signal about what the harness
        # was actually doing. `_run_with_pg_timeout` already populates
        # `TimeoutExpired.{output,stderr}` from the post-kill drain; we
        # just need to read them off the exception.
        timeout_stdout = ""
        timeout_stderr = ""
        try:
            result = _run_with_pg_timeout(
                argv, cwd=None, timeout_s=timeout_s, stdin_text=task["prompt"]
            )
            elapsed = round(time.monotonic() - start, 3)
            timed_out = False
        except subprocess.TimeoutExpired as e:
            elapsed = round(time.monotonic() - start, 3)
            result = None
            timed_out = True
            timeout_stdout = e.output or ""
            if isinstance(timeout_stdout, bytes):
                timeout_stdout = timeout_stdout.decode("utf-8", "replace")
            timeout_stderr = e.stderr or ""
            if isinstance(timeout_stderr, bytes):
                timeout_stderr = timeout_stderr.decode("utf-8", "replace")

        pt = run_test_command(tmp, task["meta"])
        meta_payload, meta_violation = (None, None)
        # Even on timeout, the harness may have produced a meta block
        # before being killed. Probe whatever stdout we captured.
        stdout_for_meta = (result.stdout if result else "") or timeout_stdout
        if stdout_for_meta:
            meta_payload, meta_violation = extract_meta(stdout_for_meta)

        check_results = []
        fixture_path = Path(task["fixture"])
        tmp_path = Path(tmp)
        for chk in task["checks"]:
            check_results.append(run_check(chk, tmp_path, fixture_path))

        all_checks_ok = all(c["ok"] for c in check_results) if check_results else True
        overall_ok = (not timed_out) and pt["returncode"] == 0 and all_checks_ok and meta_violation is None

        # Build the two tail snippets. On a normal run we use the result
        # struct's pipes; on a timeout we use the snippets captured off
        # `TimeoutExpired`.
        if result:
            stdout_tail = result.stdout[-1000:] if result.stdout else ""
            stderr_tail = result.stderr[-1000:] if result.stderr else ""
        else:
            stdout_tail = timeout_stdout[-1000:]
            stderr_tail = timeout_stderr[-1000:]

        return {
            "mode": "harness",
            "task_id": task["task_id"],
            "ok": overall_ok,
            "harness": {
                "returncode": None if timed_out else result.returncode,
                "elapsed_s": elapsed,
                "timed_out": timed_out,
                "stdout_tail": stdout_tail,
                "stderr_tail": stderr_tail,
                "meta": meta_payload,
                "meta_schema_violation": meta_violation,
            },
            "post_state": pt,
            "checks": check_results,
        }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--task", required=True, help="task id (e.g. 't01'), or 'all'")
    ap.add_argument("--dry-run", action="store_true", help="validate starting state vs meta.json")
    ap.add_argument("--harness-cmd", help="shell command for the harness; '{dir}' is workdir; prompt piped to stdin")
    ap.add_argument("--harness-timeout-s", type=int, default=DEFAULT_HARNESS_TIMEOUT_S, help=f"per-task harness timeout (default {DEFAULT_HARNESS_TIMEOUT_S}s)")
    ap.add_argument("--out", help="write JSON result to this path instead of stdout")
    ap.add_argument("--summary", action="store_true", help="print one-line summary per task in addition to / instead of full JSON")
    args = ap.parse_args()

    if args.dry_run and args.harness_cmd:
        raise SystemExit("--dry-run is incompatible with --harness-cmd")
    if not args.dry_run and not args.harness_cmd:
        raise SystemExit("specify --dry-run or --harness-cmd")

    if args.task == "all":
        task_ids = [t.split("_")[0] for t in list_tasks()]
    else:
        task_ids = [args.task]

    results = []
    for tid in task_ids:
        task = load_task(tid)
        results.append(dry_run(task) if args.dry_run else harness_run(task, args.harness_cmd, args.harness_timeout_s))

    payload = {"runner_version": 1, "results": results}

    if args.out:
        # v60.37 D5/RIG-M5 — handle OSError on write so a full disk /
        # permission failure surfaces explicitly instead of leaving the
        # rig step green while the artifact never materialised. CI
        # gates downstream of `--out` (the Phase A nightly's
        # `tests/phase_a_gate/last_run.json` chain) would otherwise
        # fail at a confusing step.
        try:
            Path(args.out).write_text(
                json.dumps(payload, indent=2, allow_nan=False),
                encoding="utf-8",
            )
        except OSError as e:
            print(f"runner: failed to write --out path {args.out!r}: {e}", file=sys.stderr)
            raise SystemExit(2) from e
    if args.summary or not args.out:
        if args.summary:
            for r in results:
                status = "OK  " if r["ok"] else "FAIL"
                note = r.get("divergence") or ""
                # Failed-check tally for harness runs
                if not r["ok"] and r.get("checks"):
                    failed = [c["name"] for c in r["checks"] if not c["ok"]]
                    if failed:
                        note = f"failed checks: {', '.join(failed[:3])}"
                print(f"{status}  {r['task_id']}  {note}".rstrip())
        else:
            json.dump(payload, sys.stdout, indent=2)
            print()

    sys.exit(0 if all(r["ok"] for r in results) else 1)


if __name__ == "__main__":
    main()
