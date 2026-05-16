#!/usr/bin/env bash
# SessionStart hook: regenerate + lint atelier's memory and global memory.
# Runs at every session start. Output is captured by the harness and shown to Claude.

set -uo pipefail

# Derive ATELIER_PROJECT_DIR from this script's location — works regardless of
# where the repo is cloned, and avoids any host-harness env var.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ATELIER_PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
export ATELIER_PROJECT_DIR

MEMCHECK="$HOME/.atelier/bin/memcheck.sh"
ATELIER_MEM="$ATELIER_PROJECT_DIR/.atelier/memory"
GLOBAL_MEM="$HOME/.atelier/memory"

[[ -x "$MEMCHECK" ]] || { echo "memcheck.sh not executable at $MEMCHECK" >&2; exit 0; }

{
  [[ -d "$ATELIER_MEM" ]] && "$MEMCHECK" all "$ATELIER_MEM"
  [[ -d "$GLOBAL_MEM"  ]] && "$MEMCHECK" all "$GLOBAL_MEM"
} 2>&1 | sed 's/^/[memcheck] /'
exit 0
