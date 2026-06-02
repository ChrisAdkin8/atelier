#!/usr/bin/env python3
"""Collect a JSON metrics snapshot for the atelier Rust workspace.

Designed to run without optional tooling: missing tools are recorded as null in
the output, never errors. Wire-up: `make metrics`. Output:
`.atelier/metrics/snapshot.json` plus a timestamped copy under
`.atelier/metrics/history/`.

The set of "always available" metrics is derived from CODE_QUALITY_METRICS.md
(production-path proxies, largest files, LOC) so this script mechanizes that
doc. Optional tools layered in:
  - rust-code-analysis-cli  : per-file CC/cognitive/Halstead/MI -> per-crate aggregates
  - cargo-geiger            : unsafe-usage breakdown per crate
  - cargo-modules           : module tree count/depth per crate
  - cargo-public-api        : exact public API line count per crate
  - cargo-audit             : workspace advisory count
  - cargo-machete           : workspace unused-dep list
  - cargo-outdated          : workspace dep-staleness summary

`cargo llvm-cov` (coverage) is intentionally excluded from the default run — it
requires a full instrumented build. Run it separately via `cargo llvm-cov ...`
when you want coverage trend.
"""

from __future__ import annotations

import json
import re
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
CRATES = ["atelier-core", "atelier-cli", "atelier-gui", "atelier-tui"]
OUT_DIR = ROOT / ".atelier" / "metrics"
SNAPSHOT = OUT_DIR / "snapshot.json"
HISTORY_DIR = OUT_DIR / "history"

# Hot-path tool subprocess timeouts (seconds). Clippy/geiger can be slow on a
# cold build; everything else is sub-second.
TIMEOUTS = {
    "clippy": 600,
    "geiger": 600,
    "rust-code-analysis-cli": 300,
    "default": 120,
}


def run(cmd: list[str], *, cwd: Path | None = None, timeout: int = 120, env: dict | None = None) -> subprocess.CompletedProcess | None:
    """Run a subprocess; return None if the binary is missing or it times out."""
    try:
        return subprocess.run(
            cmd, cwd=cwd or ROOT, capture_output=True, text=True, timeout=timeout, env=env
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return None


def have(name: str) -> bool:
    return shutil.which(name) is not None


# ---------------------------------------------------------------------------
# Always-available collectors (no optional tooling required).
# ---------------------------------------------------------------------------


def git_state() -> dict[str, Any]:
    head = run(["git", "rev-parse", "HEAD"])
    branch = run(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    porcelain = run(["git", "status", "--porcelain"])
    return {
        "head": head.stdout.strip() if head and head.returncode == 0 else None,
        "branch": branch.stdout.strip() if branch and branch.returncode == 0 else None,
        "dirty": bool(porcelain.stdout.strip()) if porcelain and porcelain.returncode == 0 else None,
    }


def toolchain() -> dict[str, Any]:
    rustc = run(["rustc", "--version"])
    cargo = run(["cargo", "--version"])
    return {
        "rustc": rustc.stdout.strip() if rustc and rustc.returncode == 0 else None,
        "cargo": cargo.stdout.strip() if cargo and cargo.returncode == 0 else None,
    }


# A `#[cfg(test)]` annotation may sit on a `mod tests { ... }` block (most common)
# or directly on a `fn ...`. We strip both. Best-effort, brace-counted.
RE_CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]\s*$")
RE_MOD_OR_FN_OPEN = re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?(?:mod|fn)\b.*\{\s*$")


def strip_cfg_test_blocks(text: str) -> str:
    """Remove `#[cfg(test)] mod ... {...}` and `#[cfg(test)] fn ... {...}` blocks.

    Heuristic — not a parser. Good enough for grep-based proxy counts that
    only need to exclude obvious test wrappers.
    """
    out: list[str] = []
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        if RE_CFG_TEST.match(lines[i]):
            # Look ahead for the opening brace.
            j = i + 1
            while j < len(lines) and not RE_MOD_OR_FN_OPEN.match(lines[j]):
                # Also tolerate the attribute followed by extra attributes / blank lines.
                if lines[j].strip() and not lines[j].strip().startswith("#["):
                    break
                j += 1
            if j < len(lines) and RE_MOD_OR_FN_OPEN.match(lines[j]):
                depth = 1
                k = j + 1
                while k < len(lines) and depth > 0:
                    depth += lines[k].count("{")
                    depth -= lines[k].count("}")
                    k += 1
                i = k
                continue
        out.append(lines[i])
        i += 1
    return "\n".join(out)


def strip_line_comments(text: str) -> str:
    # Remove only full-line comments — don't try to handle inline comments
    # (would need a real lexer for correctness around strings).
    return "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("//"))


def rust_files(crate_dir: Path) -> list[Path]:
    return [p for p in crate_dir.rglob("*.rs") if "target" not in p.parts]


def is_test_file(path: Path) -> bool:
    parts = set(path.parts)
    if "tests" in parts or "benches" in parts or "examples" in parts:
        return True
    name = path.name
    if name == "tests.rs" or name.endswith("_test.rs") or name.endswith("_tests.rs"):
        return True
    return False


def nonblank_loc(text: str) -> int:
    return sum(1 for line in text.splitlines() if line.strip())


def file_loc(path: Path) -> int:
    try:
        return nonblank_loc(path.read_text(encoding="utf-8"))
    except (UnicodeDecodeError, PermissionError):
        return 0


PROD_PROXY_PATTERNS = {
    "unsafe": re.compile(r"\bunsafe\b\s*(?:fn|impl|trait|\{)"),
    "unwrap": re.compile(r"\.unwrap\("),
    "expect": re.compile(r"\.expect\("),
    "panic_macro": re.compile(r"\bpanic!\("),
    "todo_macro": re.compile(r"\btodo!\("),
    "unimplemented_macro": re.compile(r"\bunimplemented!\("),
}
IGNORE_RE = re.compile(r"#\[ignore\b")

# --- AI-slop indicators ------------------------------------------------------

# Tells: opinion/marketing phrases, pedagogical filler, first/second person,
# self-referential commentary. Each individually is benign in moderation. The
# signal is density per kLOC of comments. Patterns are case-insensitive.
AI_TELL_PHRASES = [
    # opinion / marketing
    r"\brobust\b", r"\bcomprehensive(?:ly)?\b", r"\bpowerful\b",
    r"\bgracefully\b", r"\bseamlessly\b", r"\belegantly\b",
    r"\bsophisticated\b",
    # pedagogical filler
    r"\bnote that\b", r"\bit'?s important\b", r"\bthe following\b",
    r"\bas we can see\b", r"\bkeep in mind\b", r"\bin other words\b",
    r"\bessentially\b", r"\bbasically\b",
    # first / second person
    r"\bI'?ll\b", r"\blet'?s\b", r"\bwe'?ll\b", r"\byou can\b", r"\bwe can\b",
    # self-referential
    r"\bthis function\b", r"\bthis method\b", r"\bin this code\b",
    r"\bthe above\b", r"\bthe below\b",
]
AI_TELL_RE = re.compile("|".join(AI_TELL_PHRASES), re.IGNORECASE)
EM_DASH_RE = re.compile(r"—")
MARKDOWN_IN_COMMENT_RE = re.compile(r"\*\*[^*]+\*\*|^\s*//[/!]*\s*#{1,3}\s")

# Assertion-like operations counted toward "test rigor". Includes is_*-style
# checks because they're a common slop pattern even though they're weaker than
# a real value assertion.
ASSERTION_RE = re.compile(
    r"\b(?:assert!|assert_eq!|assert_ne!|assert_matches!|debug_assert!|"
    r"debug_assert_eq!|debug_assert_ne!|panic!)|"
    r"\.is_ok\(\)|\.is_some\(\)|\.is_err\(\)|\.is_none\(\)"
)
TEST_ATTR_RE = re.compile(r"#\[(?:tokio::)?test\b")
FN_DECL_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?fn\s+\w+"
)


def comment_metrics(text: str) -> dict[str, int]:
    """Count comment / doc / code lines and how many comments sit immediately
    above an item (fn / struct / enum / trait / impl / mod). High "above-item"
    counts plus a high comment ratio is the classic slop shape.
    """
    code = 0
    comment = 0
    doc = 0
    above_item = 0
    prev_was_comment = False
    item_re = re.compile(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?"
        r"(?:fn|struct|enum|trait|impl|mod)\b"
    )
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            prev_was_comment = False
            continue
        if stripped.startswith("///") or stripped.startswith("//!"):
            doc += 1
            prev_was_comment = True
        elif stripped.startswith("//"):
            comment += 1
            prev_was_comment = True
        else:
            if prev_was_comment and item_re.match(line):
                above_item += 1
            code += 1
            prev_was_comment = False
    return {
        "code_lines": code,
        "comment_lines": comment,
        "doc_lines": doc,
        "comments_above_items": above_item,
    }


def ai_tell_hits(text: str) -> dict[str, int]:
    """Count AI-tell phrase hits inside `//` and `///` comment lines only."""
    phrase_hits = 0
    em_dash_hits = 0
    markdown_hits = 0
    for line in text.splitlines():
        stripped = line.strip()
        if not (stripped.startswith("//") or stripped.startswith("///")):
            continue
        phrase_hits += len(AI_TELL_RE.findall(stripped))
        em_dash_hits += len(EM_DASH_RE.findall(stripped))
        if MARKDOWN_IN_COMMENT_RE.search(stripped):
            markdown_hits += 1
    return {
        "phrase_hits": phrase_hits,
        "em_dash_hits": em_dash_hits,
        "markdown_in_comment_hits": markdown_hits,
    }


def test_assertion_counts(text: str) -> list[int]:
    """For each `#[test]` or `#[tokio::test]` fn in the text, return the count
    of assertion-like operations within its body. Brace-counted; same caveats
    as strip_cfg_test_blocks (string literals containing braces can mislead).
    """
    counts: list[int] = []
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        if TEST_ATTR_RE.search(lines[i]) and "//" not in lines[i].split("#[", 1)[0]:
            j = i + 1
            while j < len(lines) and not FN_DECL_RE.match(lines[j]):
                if j - i > 8:
                    break
                j += 1
            if j >= len(lines) or not FN_DECL_RE.match(lines[j]):
                i += 1
                continue
            while j < len(lines) and "{" not in lines[j]:
                j += 1
            if j >= len(lines):
                break
            depth = lines[j].count("{") - lines[j].count("}")
            count = len(ASSERTION_RE.findall(lines[j]))
            j += 1
            while j < len(lines) and depth > 0:
                count += len(ASSERTION_RE.findall(lines[j]))
                depth += lines[j].count("{") - lines[j].count("}")
                j += 1
            counts.append(count)
            i = j
        else:
            i += 1
    return counts


def fn_lengths_pure_python(text: str) -> list[int]:
    """Pure-Python fn-length fallback when rust-code-analysis-cli is missing.
    Returns body nonblank LOC per fn. Heuristic — see test_assertion_counts.
    """
    lengths: list[int] = []
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        if FN_DECL_RE.match(lines[i]):
            j = i
            broken = False
            while j < len(lines) and "{" not in lines[j]:
                # `fn foo();` in a trait declaration — no body to measure.
                if ";" in lines[j]:
                    broken = True
                    break
                j += 1
                if j - i > 12:
                    broken = True
                    break
            if broken or j >= len(lines):
                i += 1
                continue
            depth = lines[j].count("{") - lines[j].count("}")
            start = j
            k = j + 1
            while k < len(lines) and depth > 0:
                depth += lines[k].count("{") - lines[k].count("}")
                k += 1
            lengths.append(sum(1 for ln in lines[start:k] if ln.strip()))
            i = k
        else:
            i += 1
    return lengths


def percentile(values: list[float], q: float) -> float | None:
    if not values:
        return None
    if len(values) == 1:
        return values[0]
    s = sorted(values)
    k = (len(s) - 1) * q
    f = int(k)
    c = min(f + 1, len(s) - 1)
    return s[f] + (s[c] - s[f]) * (k - f)


def summarize_distribution(values: list[float]) -> dict[str, Any] | None:
    if not values:
        return None
    return {
        "count": len(values),
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "max": max(values),
        "mean": round(statistics.fmean(values), 2),
        "over_50": sum(1 for v in values if v > 50),
        "over_100": sum(1 for v in values if v > 100),
        "over_200": sum(1 for v in values if v > 200),
    }


def collect_crate(crate_name: str) -> dict[str, Any]:
    crate_dir = ROOT / "crates" / crate_name
    files = rust_files(crate_dir)
    src_files = [p for p in files if not is_test_file(p)]
    external_test_files = [p for p in files if is_test_file(p)]

    largest = sorted(((p, file_loc(p)) for p in files), key=lambda x: -x[1])[:5]

    proxies = {name: 0 for name in PROD_PROXY_PATTERNS}
    ignored_tests = 0
    pub_counts = {"pub_fn": 0, "pub_struct": 0, "pub_enum": 0, "pub_trait": 0}
    pub_re = {
        "pub_fn": re.compile(r"^\s*pub(?:\([^)]*\))?\s+(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?fn\s"),
        "pub_struct": re.compile(r"^\s*pub(?:\([^)]*\))?\s+struct\s"),
        "pub_enum": re.compile(r"^\s*pub(?:\([^)]*\))?\s+enum\s"),
        "pub_trait": re.compile(r"^\s*pub(?:\([^)]*\))?\s+(?:unsafe\s+)?trait\s"),
    }

    prod_loc = 0
    in_source_test_loc = 0
    comments = {"code_lines": 0, "comment_lines": 0, "doc_lines": 0, "comments_above_items": 0}
    tells = {"phrase_hits": 0, "em_dash_hits": 0, "markdown_in_comment_hits": 0}
    fn_lengths: list[float] = []
    for path in src_files:
        try:
            raw = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, PermissionError):
            continue
        raw_loc = nonblank_loc(raw)
        stripped = strip_cfg_test_blocks(raw)
        stripped_loc = nonblank_loc(stripped)
        prod_loc += stripped_loc
        in_source_test_loc += raw_loc - stripped_loc
        cleaned = strip_line_comments(stripped)
        for name, pattern in PROD_PROXY_PATTERNS.items():
            proxies[name] += len(pattern.findall(cleaned))
        for name, pattern in pub_re.items():
            pub_counts[name] += sum(1 for line in cleaned.splitlines() if pattern.match(line))
        # AI-slop: comment shape + tell phrases scanned on the raw file (so
        # we see what's actually in source, not the test-stripped version).
        cm = comment_metrics(raw)
        for k, v in cm.items():
            comments[k] += v
        th = ai_tell_hits(raw)
        for k, v in th.items():
            tells[k] += v
        # Function-length distribution measured on production-stripped source.
        fn_lengths.extend(fn_lengths_pure_python(stripped))

    # Test rigor — measured on test code, both in-source and external.
    test_assertions: list[int] = []
    for path in src_files:
        try:
            raw = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, PermissionError):
            continue
        test_assertions.extend(test_assertion_counts(raw))

    external_test_loc = 0
    for path in external_test_files:
        try:
            raw = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, PermissionError):
            continue
        external_test_loc += nonblank_loc(raw)
        ignored_tests += len(IGNORE_RE.findall(raw))
        test_assertions.extend(test_assertion_counts(raw))

    # Slop indicators ---------------------------------------------------------
    comment_code_ratio = (
        round((comments["comment_lines"] + comments["doc_lines"]) / comments["code_lines"], 3)
        if comments["code_lines"] else None
    )
    phrase_per_kloc = (
        round((tells["phrase_hits"] / prod_loc) * 1000, 2) if prod_loc else None
    )
    em_dash_per_kloc = (
        round((tells["em_dash_hits"] / prod_loc) * 1000, 2) if prod_loc else None
    )
    test_assertion_summary: dict[str, Any] | None = None
    if test_assertions:
        test_assertion_summary = {
            "test_count": len(test_assertions),
            "assertions_per_test_median": statistics.median(test_assertions),
            "assertions_per_test_mean": round(statistics.fmean(test_assertions), 2),
            "zero_assertion_tests": sum(1 for c in test_assertions if c == 0),
        }

    return {
        "rust_files": len(files),
        "rust_files_src": len(src_files),
        "rust_files_external_test": len(external_test_files),
        "loc_nonblank_production": prod_loc,
        "loc_nonblank_in_source_tests": in_source_test_loc,
        "loc_nonblank_external_tests": external_test_loc,
        "loc_nonblank_total": prod_loc + in_source_test_loc + external_test_loc,
        "largest_files": [
            {"path": str(p.relative_to(ROOT)), "loc_nonblank": n} for p, n in largest
        ],
        "production_proxies": proxies,
        "ignored_tests": ignored_tests,
        "public_api_proxy": pub_counts,
        "slop_indicators": {
            "fn_length_pure_python": summarize_distribution(fn_lengths),
            "comments": {
                **comments,
                "comment_to_code_ratio": comment_code_ratio,
            },
            "ai_tell_phrases": {
                **tells,
                "phrase_hits_per_kloc": phrase_per_kloc,
                "em_dash_hits_per_kloc": em_dash_per_kloc,
            },
            "test_rigor": test_assertion_summary,
        },
    }


# ---------------------------------------------------------------------------
# Workspace-level always-available collectors.
# ---------------------------------------------------------------------------


def cargo_tree_duplicates() -> dict[str, Any]:
    p = run(["cargo", "tree", "-d", "-e", "normal", "--workspace"])
    if not p or p.returncode != 0:
        return {"available": False, "stderr": (p.stderr.strip()[:400] if p else None)}
    lines = [ln for ln in p.stdout.splitlines() if ln.strip()]
    # Count entries that look like crate version roots ("name vX.Y.Z").
    roots = [ln for ln in lines if re.match(r"^[a-zA-Z0-9_\-]+ v\d", ln)]
    return {"available": True, "total_lines": len(lines), "duplicate_crate_roots": len(roots)}


# ---------------------------------------------------------------------------
# Optional collectors. Each returns None if the tool is missing.
# ---------------------------------------------------------------------------


def cargo_audit() -> dict[str, Any] | None:
    if not have("cargo-audit"):
        return None
    # JSON output gives us a structured advisory list. `cargo audit` exits
    # non-zero when advisories are present even without --deny; that's expected.
    p = run(["cargo", "audit", "--json"], timeout=TIMEOUTS["default"])
    if not p:
        return {"error": "subprocess failed"}
    try:
        data = json.loads(p.stdout)
    except json.JSONDecodeError:
        return {"error": "could not parse cargo-audit JSON", "stderr": p.stderr[:400]}
    vulnerabilities = data.get("vulnerabilities", {})
    warnings = data.get("warnings", {})
    return {
        "vulnerability_count": vulnerabilities.get("count", 0),
        "vulnerability_ids": [v.get("advisory", {}).get("id") for v in vulnerabilities.get("list", [])],
        "warning_count": sum(len(v) for v in warnings.values()) if isinstance(warnings, dict) else 0,
    }


def cargo_machete() -> dict[str, Any] | None:
    if not have("cargo-machete"):
        return None
    p = run(["cargo", "machete", "crates/", "--with-metadata"], timeout=TIMEOUTS["default"])
    if not p:
        return {"error": "subprocess failed"}
    # machete exits 1 when it finds unused deps; that's a finding, not a failure.
    unused: list[str] = []
    for line in p.stdout.splitlines():
        line = line.strip()
        # machete formats findings as "  crate_name" indented under the crate header.
        if line.startswith("- ") or (line and line[0].isspace() and not line.lower().startswith("warning")):
            unused.append(line.lstrip("- ").strip())
    return {"unused_dep_lines": len(unused), "sample": unused[:20]}


def cargo_outdated() -> dict[str, Any] | None:
    if not have("cargo-outdated"):
        return None
    p = run(["cargo", "outdated", "--workspace", "--format", "json"], timeout=TIMEOUTS["default"])
    if not p or p.returncode != 0:
        return {"error": "cargo-outdated failed", "stderr": (p.stderr[:400] if p else None)}
    try:
        data = json.loads(p.stdout)
    except json.JSONDecodeError:
        return {"error": "could not parse cargo-outdated JSON"}
    deps = data.get("dependencies") or []
    return {"outdated_deps": len(deps), "sample": [d.get("name") for d in deps[:20]]}


def jscpd_duplication() -> dict[str, Any] | None:
    """Run jscpd over `crates/**/src/**/*.rs` to surface near-duplicate blocks.

    jscpd is npm-distributed; install with `npm install -g jscpd`. Slop fns
    that regenerate the same logic with renamed identifiers show up here.
    """
    if not have("jscpd"):
        return None
    import tempfile
    with tempfile.TemporaryDirectory() as tmp:
        p = run(
            [
                "jscpd",
                "--silent",
                "--reporters", "json",
                "--output", tmp,
                "--min-tokens", "50",
                "--min-lines", "8",
                "--mode", "mild",
                "--ignore", "**/target/**,**/tests/**,**/benches/**,**/examples/**",
                "crates",
            ],
            timeout=TIMEOUTS["default"],
        )
        if not p:
            return {"error": "subprocess failed"}
        report = Path(tmp) / "jscpd-report.json"
        if not report.exists():
            return {"error": "jscpd produced no report", "stderr": p.stderr[:400]}
        try:
            data = json.loads(report.read_text())
        except json.JSONDecodeError:
            return {"error": "could not parse jscpd JSON"}
        total = (data.get("statistics") or {}).get("total") or {}
        rust = ((data.get("statistics") or {}).get("formats") or {}).get("rust") or {}
        return {
            "duplicated_lines": total.get("duplicatedLines", 0),
            "duplicated_tokens": total.get("duplicatedTokens", 0),
            "total_lines": total.get("lines", 0),
            "duplication_percent": total.get("percentage", 0.0),
            "clones_found": total.get("clones", 0),
            "rust_only": {
                "duplicated_lines": (rust.get("total") or {}).get("duplicatedLines"),
                "duplication_percent": (rust.get("total") or {}).get("percentage"),
                "clones_found": (rust.get("total") or {}).get("clones"),
            },
        }


def _walk_rca(node: Any, acc: dict[str, list[float]]) -> None:
    """Walk a rust-code-analysis JSON tree, harvesting per-function metrics.

    Only `kind == "function"` spaces are counted, not file-level rollups, so
    aggregates reflect the per-function distribution (which is what's useful
    for the AI-slop fn-length signal).
    """
    if isinstance(node, dict):
        if node.get("kind") == "function":
            metrics = node.get("metrics") or {}
            cyc = (metrics.get("cyclomatic") or {}).get("sum")
            cog = (metrics.get("cognitive") or {}).get("sum")
            hal = (metrics.get("halstead") or {}).get("volume")
            mi = (metrics.get("mi") or {}).get("mi_original")
            sloc = (metrics.get("loc") or {}).get("sloc")
            for key, src in (
                ("cyclomatic", cyc),
                ("cognitive", cog),
                ("halstead_volume", hal),
                ("mi_original", mi),
                ("fn_sloc", sloc),
            ):
                if isinstance(src, (int, float)):
                    acc[key].append(float(src))
        spaces = node.get("spaces")
        if isinstance(spaces, list):
            for s in spaces:
                _walk_rca(s, acc)
    elif isinstance(node, list):
        for v in node:
            _walk_rca(v, acc)


def rust_code_analysis(crate_name: str) -> dict[str, Any] | None:
    if not have("rust-code-analysis-cli"):
        return None
    crate_dir = ROOT / "crates" / crate_name / "src"
    p = run(
        ["rust-code-analysis-cli", "-m", "-O", "json", "-p", str(crate_dir)],
        timeout=TIMEOUTS["rust-code-analysis-cli"],
    )
    if not p or p.returncode != 0:
        return {"error": "rust-code-analysis-cli failed", "stderr": (p.stderr[:400] if p else None)}
    acc: dict[str, list[float]] = {
        "cyclomatic": [], "cognitive": [], "halstead_volume": [],
        "mi_original": [], "fn_sloc": [],
    }
    # rust-code-analysis-cli emits one JSON object per file, one per line.
    for line in p.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            _walk_rca(json.loads(line), acc)
        except json.JSONDecodeError:
            continue
    out: dict[str, Any] = {}
    for name, vals in acc.items():
        if not vals:
            out[name] = None
            continue
        out[name] = summarize_distribution(vals)
    # Maintainability Index is "lower is worse", flag the min explicitly.
    if acc["mi_original"]:
        out["mi_original"]["min"] = min(acc["mi_original"])
    return out


def cargo_geiger(crate_name: str) -> dict[str, Any] | None:
    if not have("cargo-geiger"):
        return None
    p = run(
        ["cargo", "geiger", "--package", crate_name, "--output-format", "Json"],
        timeout=TIMEOUTS["geiger"],
    )
    if not p:
        return {"error": "subprocess failed"}
    # geiger may print summary text before the JSON body when run interactively;
    # find the first '{' to be safe.
    idx = p.stdout.find("{")
    if idx < 0:
        return {"error": "no JSON in cargo-geiger output", "stderr": p.stderr[:400]}
    try:
        data = json.loads(p.stdout[idx:])
    except json.JSONDecodeError:
        return {"error": "could not parse cargo-geiger JSON"}
    # geiger's schema: top-level "packages" -> [{ "unsafety": { "used": {...}, "unused": {...} } } ...]
    pkgs = data.get("packages") or []
    used = unused = 0
    for pkg in pkgs:
        u = pkg.get("unsafety", {}).get("used", {})
        n = pkg.get("unsafety", {}).get("unused", {})
        for bucket in ("functions", "exprs", "impls", "traits", "methods"):
            used += (u.get(bucket, {}) or {}).get("safe", 0) + (u.get(bucket, {}) or {}).get("unsafe_", 0)
            unused += (n.get(bucket, {}) or {}).get("safe", 0) + (n.get(bucket, {}) or {}).get("unsafe_", 0)
    return {"used_total": used, "unused_total": unused, "packages_scanned": len(pkgs)}


_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
_TREE_PREFIX_RE = re.compile(r"^((?:│   |    )*)(├── |└── )")


def _modules_depth(line: str) -> int:
    clean = _ANSI_RE.sub("", line)
    m = _TREE_PREFIX_RE.match(clean)
    return len(m.group(1)) // 4 + 1 if m else 0


def cargo_modules(crate_name: str) -> dict[str, Any] | None:
    if not have("cargo-modules"):
        return None
    base_cmd = ["cargo", "modules", "structure", "--package", crate_name, "--no-fns", "--no-types"]
    p = run(base_cmd, timeout=TIMEOUTS["default"])
    # Multi-target crates (lib + bin) require an explicit --lib flag.
    if (not p or p.returncode != 0) and (not p or "Multiple targets" in (p.stderr or "")):
        p = run(base_cmd + ["--lib"], timeout=TIMEOUTS["default"])
    if not p or p.returncode != 0:
        return {"error": "cargo-modules failed", "stderr": (p.stderr[:400] if p else None)}
    lines = [ln for ln in p.stdout.splitlines() if ln.strip()]
    depths = [_modules_depth(ln) for ln in lines]
    return {
        "module_count": len(lines),
        "max_depth": max(depths) if depths else 0,
    }


def cargo_public_api(crate_name: str) -> dict[str, Any] | None:
    if not have("cargo-public-api"):
        return None
    p = run(
        ["cargo", "public-api", "--package", crate_name],
        timeout=TIMEOUTS["default"],
    )
    if not p or p.returncode != 0:
        return {"error": "cargo-public-api failed", "stderr": (p.stderr[:400] if p else None)}
    api_lines = [ln for ln in p.stdout.splitlines() if ln.strip() and not ln.startswith("#")]
    return {"public_api_items": len(api_lines)}


# ---------------------------------------------------------------------------
# Top-level orchestration.
# ---------------------------------------------------------------------------


def collect() -> dict[str, Any]:
    started = time.monotonic()
    snapshot: dict[str, Any] = {
        "schema_version": 1,
        "captured_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "git": git_state(),
        "toolchain": toolchain(),
        "tools_available": {
            tool: have(tool)
            for tool in (
                "rust-code-analysis-cli", "cargo-geiger", "cargo-modules",
                "cargo-public-api", "cargo-audit", "cargo-machete",
                "cargo-outdated", "cargo-llvm-cov", "jscpd",
            )
        },
        "workspace": {
            "duplicate_deps": cargo_tree_duplicates(),
            "audit": cargo_audit(),
            "machete": cargo_machete(),
            "outdated": cargo_outdated(),
            "duplicate_blocks": jscpd_duplication(),
        },
        "crates": {},
    }
    for crate in CRATES:
        base = collect_crate(crate)
        base["complexity"] = rust_code_analysis(crate)
        base["geiger"] = cargo_geiger(crate)
        base["modules"] = cargo_modules(crate)
        base["public_api"] = cargo_public_api(crate)
        snapshot["crates"][crate] = base
    snapshot["elapsed_seconds"] = round(time.monotonic() - started, 2)
    return snapshot


def write_snapshot(snapshot: dict[str, Any]) -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    HISTORY_DIR.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(snapshot, indent=2, sort_keys=True) + "\n"
    SNAPSHOT.write_text(payload, encoding="utf-8")
    stamp = snapshot["captured_at"].replace(":", "").replace("-", "")
    (HISTORY_DIR / f"{stamp}.json").write_text(payload, encoding="utf-8")


def print_summary(snapshot: dict[str, Any]) -> None:
    print(f"atelier metrics snapshot @ {snapshot['captured_at']}  ({snapshot['elapsed_seconds']}s)")
    git = snapshot["git"]
    print(f"  git: {git.get('branch')}  {(git.get('head') or '')[:10]}  dirty={git.get('dirty')}")
    print(f"  toolchain: {snapshot['toolchain'].get('rustc')}")
    avail = [t for t, ok in snapshot["tools_available"].items() if ok]
    missing = [t for t, ok in snapshot["tools_available"].items() if not ok]
    print(f"  optional tools present: {', '.join(avail) or '(none)'}")
    if missing:
        print(f"  optional tools missing: {', '.join(missing)}  (run `make metrics-install`)")
    dup = snapshot["workspace"]["duplicate_deps"]
    if dup.get("available"):
        print(f"  duplicate dep crate-roots: {dup['duplicate_crate_roots']}")
    blocks = snapshot["workspace"].get("duplicate_blocks")
    if blocks and "duplicated_lines" in blocks:
        print(
            f"  duplicate code blocks (jscpd): "
            f"{blocks['clones_found']} clones, "
            f"{blocks['duplicated_lines']} dup lines "
            f"({blocks['duplication_percent']}%)"
        )
    print("  per-crate hotspot proxies:")
    for crate, data in snapshot["crates"].items():
        proxies = data["production_proxies"]
        print(
            f"    {crate:<14} "
            f"prod_loc={data['loc_nonblank_production']:<6} "
            f"files={data['rust_files']:<3} "
            f"unsafe={proxies['unsafe']:<3} "
            f"unwrap={proxies['unwrap']:<4} "
            f"expect={proxies['expect']:<3} "
            f"panic/todo/unimpl={proxies['panic_macro']+proxies['todo_macro']+proxies['unimplemented_macro']:<3} "
            f"pub_api≈{sum(data['public_api_proxy'].values())}"
        )
    print("  per-crate AI-slop indicators:")
    for crate, data in snapshot["crates"].items():
        slop = data["slop_indicators"]
        fn = slop["fn_length_pure_python"] or {}
        cmts = slop["comments"]
        tells = slop["ai_tell_phrases"]
        rigor = slop["test_rigor"] or {}
        fn_p95 = fn.get("p95")
        fn_max = fn.get("max")
        fn_over100 = fn.get("over_100", 0)
        rigor_med = rigor.get("assertions_per_test_median")
        zero_asserts = rigor.get("zero_assertion_tests", 0)
        test_count = rigor.get("test_count", 0)
        print(
            f"    {crate:<14} "
            f"fn_p95={int(fn_p95) if fn_p95 is not None else '-':<4} "
            f"fn_max={int(fn_max) if fn_max is not None else '-':<5} "
            f"fn>100={fn_over100:<3} "
            f"cmt/code={cmts['comment_to_code_ratio']:<5} "
            f"above_item={cmts['comments_above_items']:<4} "
            f"tells/kloc={tells['phrase_hits_per_kloc']:<5} "
            f"em—/kloc={tells['em_dash_hits_per_kloc']:<5} "
            f"asserts/test={rigor_med if rigor_med is not None else '-':<3} "
            f"zero_assert={zero_asserts}/{test_count}"
        )
    print(f"  snapshot: {SNAPSHOT.relative_to(ROOT)}")


def main(argv: list[str]) -> int:
    snapshot = collect()
    write_snapshot(snapshot)
    if "--quiet" not in argv:
        print_summary(snapshot)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
