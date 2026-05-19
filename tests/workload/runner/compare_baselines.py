#!/usr/bin/env python3
"""Compare an Atelier prompt-count file to a baseline-harness prompt-count file.

Both files conform to schemas/baselines/permission_prompts.v1.json. The schema
is vendor-neutral: the baseline can be any harness with a measurable prompt
count (Claude Code is the v0.1 reference per spec §8, but the format does not
hardcode it).

Usage:
  python compare_baselines.py \
      --baseline tests/baselines/permission_prompts.json \
      --atelier  tests/baselines/atelier_prompts.json

Outputs per-task and aggregate ratios. Reports pass/fail vs the configurable
ratio threshold (default 0.30 — §8's "≤30% of baseline" target).
"""

import argparse
import json
import sys
from pathlib import Path

DEFAULT_TARGET_RATIO = 0.30  # PROVISIONAL — see spec §8 calibration


def load(p):
    return json.loads(Path(p).read_text(encoding="utf-8"))


def index_by_task(data):
    return {t["task_id"]: t["median_prompt_count"] for t in data["tasks"]}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--baseline", required=True, help="path to Claude Code baseline JSON")
    ap.add_argument("--atelier", required=True, help="path to Atelier prompt-count JSON")
    ap.add_argument("--target-ratio", type=float, default=DEFAULT_TARGET_RATIO,
                    help=f"max acceptable Atelier/baseline ratio (default {DEFAULT_TARGET_RATIO})")
    args = ap.parse_args()

    bl = load(args.baseline)
    at = load(args.atelier)
    bl_idx = index_by_task(bl)
    at_idx = index_by_task(at)

    missing_in_atelier = sorted(set(bl_idx) - set(at_idx))
    missing_in_baseline = sorted(set(at_idx) - set(bl_idx))

    rows = []
    bl_total = 0
    at_total = 0
    for tid in sorted(set(bl_idx) & set(at_idx)):
        b, a = bl_idx[tid], at_idx[tid]
        bl_total += b
        at_total += a
        ratio = (a / b) if b > 0 else float("inf") if a > 0 else 0.0
        rows.append((tid, b, a, ratio))

    bl_label = f"{bl.get('baseline_harness_name', '?')} {bl.get('baseline_harness_version', '?')}"
    at_label = f"{at.get('baseline_harness_name', '?')} {at.get('baseline_harness_version', '?')}"
    print(f"Baseline: {Path(args.baseline).name} ({bl_label})")
    print(f"Atelier:  {Path(args.atelier).name} ({at_label})")
    print()
    print(f"{'task_id':40} {'baseline':>10} {'atelier':>10} {'ratio':>8}")
    print("-" * 72)
    for tid, b, a, r in rows:
        # v60.38 L6/RIG-L2 — render "n/a" instead of "inf" when the
        # baseline is zero. The infinite ratio is meaningless as a
        # value; "n/a" both reads correctly to humans and is greppable
        # by downstream scripts looking for fail-rows.
        if b == 0:
            print(f"{tid:40} {b:>10} {a:>10} {'n/a':>8}")
        else:
            print(f"{tid:40} {b:>10} {a:>10} {r:>8.2f}")
    print("-" * 72)
    agg_ratio = (at_total / bl_total) if bl_total > 0 else float("inf")
    print(f"{'AGGREGATE':40} {bl_total:>10} {at_total:>10} {agg_ratio:>8.2f}")

    if missing_in_atelier:
        print(f"\nMissing in atelier file: {missing_in_atelier}")
    if missing_in_baseline:
        print(f"\nMissing in baseline file: {missing_in_baseline}")

    passed = agg_ratio <= args.target_ratio
    print(f"\nTarget ratio: ≤{args.target_ratio:.2f}")
    print(f"Result: {'PASS' if passed else 'FAIL'} (aggregate {agg_ratio:.2f})")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
