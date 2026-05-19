#!/usr/bin/env bash
# PreToolUse guard: nudges (non-blocking) when Read/Grep is called without bounds.
# Fires only on actually-problematic cases: Read on >500-line files without a limit,
# or Grep with output_mode=content and no head_limit.
#
# v60.55 — hardened to guarantee exit 0 and a silent stderr on every codepath:
#  * Dropped `set -u`. The script reads JSON-shaped input where every field
#    may be missing; nounset adds no value and risks an unbound-variable
#    exit when invoked from an unusual host context. Hooks must be
#    non-blocking per spec §15; a non-zero exit from a guard hook surfaces
#    in the host UI as a hook error.
#  * Every jq call has its stderr redirected to /dev/null. A malformed
#    payload should be a quiet no-op, not a hook-error banner.
#  * Path resolution uses POSIX `dirname` directly; no `cd` round-trip
#    that could silently leak the parent shell's cwd on failure.
#  * `exit 0` at every terminus.

set -o pipefail

# Derive ATELIER_PROJECT_DIR from this script's location — vendor-neutral; no
# dependency on a host-harness env var. Hook dir is <root>/.atelier/hooks/.
# Defensive: if BASH_SOURCE[0] is somehow unset, fall back to the cwd.
script_path="${BASH_SOURCE[0]:-$0}"
script_dir="$(cd "$(dirname "$script_path")" 2>/dev/null && pwd)" || script_dir="$PWD"
ATELIER_PROJECT_DIR="$(cd "$script_dir/../.." 2>/dev/null && pwd)" || ATELIER_PROJECT_DIR="$PWD"
export ATELIER_PROJECT_DIR

if ! command -v jq >/dev/null 2>&1; then
  # v60.37 C7/SH-2 — jq absence is an inert condition; don't leak to stderr
  # (Claude Code's hook surface treats any stderr line as a hook error).
  exit 0
fi

input="$(cat)"
# Silence jq's parse-error stderr on malformed payloads: the hook is
# non-blocking by spec §15, so a bad payload should mean a quiet no-op.
tool="$(jq -r '.tool_name // empty' <<<"$input" 2>/dev/null)"
[[ -z "$tool" ]] && exit 0
msg=""

case "$tool" in
  Read)
    limit="$(jq -r '.tool_input.limit // empty' <<<"$input" 2>/dev/null)"
    file="$(jq -r '.tool_input.file_path // empty' <<<"$input" 2>/dev/null)"
    if [[ -z "$limit" && -n "$file" && -f "$file" ]]; then
      # Only line-count text-like files; skip binaries to avoid awk on garbage.
      case "$file" in
        *.md|*.py|*.rs|*.json|*.yml|*.yaml|*.toml|*.sh|*.txt|*.tsv|*.csv|*.html|*.css|*.js|*.ts|*.tsx)
          # `wc -l` left-pads on macOS; strip to keep the nudge message tidy.
          lines="$(wc -l < "$file" 2>/dev/null | tr -d ' ')"
          lines="${lines:-0}"
          if [[ "$lines" =~ ^[0-9]+$ ]] && (( lines > 500 )); then
            msg="Read on ${lines}-line file without limit — pass limit/offset per the bounded-reads rule (ATELIER.md)."
          fi
          ;;
      esac
    fi
    ;;
  Grep)
    mode="$(jq -r '.tool_input.output_mode // "files_with_matches"' <<<"$input" 2>/dev/null)"
    head_limit="$(jq -r '.tool_input.head_limit // empty' <<<"$input" 2>/dev/null)"
    if [[ "$mode" == "content" && -z "$head_limit" ]]; then
      msg="Grep output_mode=content without head_limit — add head_limit:N to bound output."
    fi
    ;;
esac

if [[ -n "$msg" ]]; then
  # Wrap the jq emit so a downstream JSON error can't surface as stderr.
  jq -n --arg m "$msg" '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$m}}' 2>/dev/null || true
fi
exit 0
