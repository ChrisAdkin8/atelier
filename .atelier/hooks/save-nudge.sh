#!/usr/bin/env bash
# UserPromptSubmit hook: when the user's prompt looks like a durable directive
# (correction, preference, "remember this"), inject a reminder to consider
# saving a memory.
#
# Conservative — only fires on clear signals to avoid noise.

set -uo pipefail

# Derive ATELIER_PROJECT_DIR from this script's location — vendor-neutral.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ATELIER_PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
export ATELIER_PROJECT_DIR

if ! command -v jq >/dev/null; then
  # v60.37 C7/SH-2 — surface jq absence on stderr so a host harness
  # without jq isn't silently treated as "hook healthy". Still exit 0:
  # hooks must remain non-blocking per spec §15.
  echo "[atelier:save-nudge] jq not on PATH — save-nudge hook is inert" >&2
  exit 0
fi
if ! command -v python3 >/dev/null; then
  echo "[atelier:save-nudge] python3 not on PATH — save-nudge hook is inert" >&2
  exit 0
fi

input=$(cat)
prompt=$(python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get("prompt", ""))
except Exception:
    pass
' <<<"$input")

[[ -z "$prompt" ]] && exit 0

# Case-insensitive pattern test. Each pattern is anchored to typical phrasing,
# not bare keywords, to limit false positives.
shopt -s nocasematch
match=""
case "$prompt" in
  *"remember this"*|*"remember that"*|*"please remember"*) match="explicit-remember" ;;
  *"don't forget"*|*"do not forget"*) match="don't-forget" ;;
  *"from now on"*|*"going forward"*) match="going-forward" ;;
  *"next time"*|*"last time we"*) match="time-reference" ;;
  *"stop doing"*|*"never do"*|*"don't do that again"*) match="negative-directive" ;;
  *"always "*|*"never "*) match="always-never" ;;
esac
shopt -u nocasematch

[[ -z "$match" ]] && exit 0

note="The user's prompt contains a durable-directive signal ($match). Per the auto-memory rules, evaluate whether this turn warrants a feedback/user/project memory under .atelier/memory/ (project-scoped) or ~/.atelier/memory/ (cross-project). Save only if the directive is non-obvious and applies beyond this turn."

jq -n --arg m "$note" '{hookSpecificOutput:{hookEventName:"UserPromptSubmit",additionalContext:$m}}'
exit 0
