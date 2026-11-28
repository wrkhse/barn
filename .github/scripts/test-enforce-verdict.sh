#!/usr/bin/env bash
set -euo pipefail

# Offline fixture for the verdict-enforcement jq program. Comments have the
# shape gh api --paginate --jq emits: one JSON object per line, slurped by jq.
root=$(cd "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
program="$root/.github/scripts/enforce-review-verdict.jq"
marker='<!-- opencode-review -->'
author=41898282
head=0123456789abcdef0123456789abcdef01234567
other_head=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

[ -f "$program" ] || {
  echo 'FAIL: enforce-review-verdict.jq is missing' >&2
  exit 1
}

tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

comment() {
  jq -nc --argjson id "$1" --argjson author "$2" --arg body "$3" \
    '{id:$id, user:{id:$author}, body:$body}'
}

marker_body() {
  local verdict="$1" sha="${2:-$head}" meta
  meta=$(jq -nc --arg head "$sha" --arg verdict "$verdict" \
    '{head:$head, base:"base", verdict:$verdict, body:"body"}')
  printf '%s\n%s\nreviewer\n<!-- opencode-review-meta %s -->\n' "$marker" "$verdict" "$meta"
}

check() {
  local name="$1" expected="$2" comments="$3" status actual
  # jq exits non-zero on error(); either outcome is a result to compare.
  set +e
  printf '%s\n' "$comments" | jq -rs \
    --arg marker "$marker" --argjson author "$author" \
    --arg head "$head" \
    "$(cat "$program")" > "$tmp/out" 2> "$tmp/err"
  status=$?
  set -e
  if [ "$status" -eq 0 ]; then
    actual=pass
  else
    actual=fail
  fi
  if [ "$actual" != "$expected" ]; then
    printf 'FAIL %s: expected %s, got %s\n' "$name" "$expected" "$actual" >&2
    cat "$tmp/err" >&2 || true
    exit 1
  fi
  printf 'ok: %s (%s)\n' "$name" "$actual"
}

# 1. A single MERGE AS IS marker from the bot passes.
check 'MERGE AS IS' pass \
  "$(comment 200 "$author" "$(marker_body '**Verdict: MERGE AS IS**')")"

# 2. An advisory verdict passes.
check 'ADVISORY' pass \
  "$(comment 200 "$author" "$(marker_body '**Verdict: ADVISORY**')")"

# 3. FIX FIRST blocks the merge.
check 'FIX FIRST' fail \
  "$(comment 200 "$author" "$(marker_body '**Verdict: FIX FIRST**')")"

# 4. A comment without the marker cannot supply a verdict.
check 'no marker comment' fail "$(comment 200 "$author" 'Just a comment.')"

# 5. One marker with two verdict lines has no single verdict.
two=$(printf '**Verdict: MERGE AS IS**\n**Verdict: FIX FIRST**')
check 'two verdict lines in one marker' fail \
  "$(comment 200 "$author" "$(marker_body "$two")")"

# 6. The newest bot marker wins: an older pass cannot hide a newer FIX FIRST.
comments=$(printf '%s\n%s\n' \
  "$(comment 100 "$author" "$(marker_body '**Verdict: MERGE AS IS**')")" \
  "$(comment 200 "$author" "$(marker_body '**Verdict: FIX FIRST**')")")
check 'newest marker wins' fail "$comments"

# 7. The newest marker must be bound to this head; an older head fails.
check 'newest marker bound to another head' fail \
  "$(comment 200 "$author" "$(marker_body '**Verdict: MERGE AS IS**' "$other_head")")"
