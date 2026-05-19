#!/usr/bin/env bash
# PreToolUse guard: nudges (non-blocking) when Read/Grep is called without bounds.
# Fires only on actually-problematic cases: Read on >500-line files without a limit,
# or Grep with output_mode=content and no head_limit.

set -uo pipefail  # discipline matches save-nudge.sh and session-start-memcheck.sh:
                  # hooks must remain non-blocking (spec §15), so we use inline
                  # `|| exit 0` chains rather than `set -e`.

# Derive ATELIER_PROJECT_DIR from this script's location — vendor-neutral; no
# dependency on a host-harness env var. (Hooks dir is <root>/.atelier/hooks/.)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ATELIER_PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
export ATELIER_PROJECT_DIR

if ! command -v jq >/dev/null; then
  # v60.37 C7/SH-2 — surface jq absence on stderr exactly once so a host
  # harness without jq isn't silently treated as "hook healthy". Still
  # exit 0: hooks must remain non-blocking per spec §15.
  echo "[atelier:bounded-reads] jq not on PATH — bounded-reads hook is inert" >&2
  exit 0
fi

input=$(cat)
# Silence jq's parse-error stderr on malformed payloads (N44): the hook is
# non-blocking by spec §15, so a bad payload should mean a quiet no-op, not a
# stderr line in the host's log on every invocation.
tool=$(jq -r '.tool_name' <<<"$input" 2>/dev/null)
[[ -z "$tool" || "$tool" == "null" ]] && exit 0
msg=""

case "$tool" in
  Read)
    limit=$(jq -r '.tool_input.limit // empty' <<<"$input")
    file=$(jq -r '.tool_input.file_path // empty' <<<"$input")
    if [[ -z "$limit" && -f "$file" ]]; then
      # Only line-count text-like files; skip binaries to avoid awk on garbage.
      case "$file" in
        *.md|*.py|*.rs|*.json|*.yml|*.yaml|*.toml|*.sh|*.txt|*.tsv|*.csv|*.html|*.css|*.js|*.ts|*.tsx)
          # `wc -l` left-pads on macOS; strip to keep the nudge message tidy (N47).
          lines=$(wc -l < "$file" 2>/dev/null | tr -d ' ')
          lines=${lines:-0}
          if (( lines > 500 )); then
            msg="Read on ${lines}-line file without limit — pass limit/offset per the bounded-reads rule (ATELIER.md)."
          fi
          ;;
      esac
    fi
    ;;
  Grep)
    mode=$(jq -r '.tool_input.output_mode // "files_with_matches"' <<<"$input")
    head_limit=$(jq -r '.tool_input.head_limit // empty' <<<"$input")
    if [[ "$mode" == "content" && -z "$head_limit" ]]; then
      msg="Grep output_mode=content without head_limit — add head_limit:N to bound output."
    fi
    ;;
esac

if [[ -n "$msg" ]]; then
  jq -n --arg m "$msg" '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$m}}'
fi
exit 0
