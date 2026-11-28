#!/usr/bin/env bash
set -euo pipefail

# Offline integration test for carry-forward.sh. A stub gh serves fixture
# comments; nothing here reaches the network.
root=$(cd "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
script="$root/.github/scripts/carry-forward.sh"
workflow="$root/.github/workflows/opencode-review.yml"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/bin"
cat > "$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$GH_COMMENTS"
STUB
chmod +x "$tmp/bin/gh"
export PATH="$tmp/bin:$PATH"

marker='<!-- opencode-review -->'
head_sha=0123456789abcdef0123456789abcdef01234567
base_sha=fedcba9876543210fedcba9876543210fedcba98765432
other_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
other_base=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

meta_line() {
  jq -nc --arg head "$1" --arg base "$2" --arg verdict "$3" \
    '"<!-- opencode-review-meta " + ({"head":$head,"base":$base,"verdict":$verdict}|tojson) + " -->"'
}

comment() {
  jq -nc --argjson id "$1" --argjson author "$2" --arg body "$3" \
    '{id: $id, user: {id: $author}, body: $body}'
}

body_of() {
  printf '%s\n%s\n' "$marker" "$(meta_line "$1" "$2" "$3")"
}

check() {
  local name="$1" expected="$2" comments="$3" out="$tmp/out" actual
  : > "$out"
  GH_COMMENTS="$comments" \
    REPO=owner/repo PR=42 \
    HEAD_SHA="$head_sha" BASE_SHA="$base_sha" \
    GITHUB_OUTPUT="$out" \
    bash "$script" > "$tmp/log" 2>&1
  actual=$(sed -n 's/^carried=//p' "$out" | tail -n 1)
  if [ "$actual" != "$expected" ]; then
    printf 'FAIL %s: expected carried=%s, got %s\n' "$name" "$expected" "$actual" >&2
    cat "$tmp/log" >&2 || true
    exit 1
  fi
  printf 'ok: %s (carried=%s)\n' "$name" "$actual"
}

# 1. Newest marker comment from the bot on this head and base with MERGE AS IS;
# an older FIX FIRST comment must not win.
comments=$(printf '%s\n%s\n' \
  "$(comment 100 41898282 "$(body_of "$head_sha" "$base_sha" "FIX FIRST")")" \
  "$(comment 200 41898282 "$(body_of "$head_sha" "$base_sha" "MERGE AS IS")")" \
  | jq -s .)
check 'newest bot MERGE AS IS on this head and base' true "$comments"

# 2. Shared head SHA leak: an older match must not survive a newer comment on a
# different head.
comments=$(printf '%s\n%s\n' \
  "$(comment 100 41898282 "$(body_of "$head_sha" "$base_sha" "MERGE AS IS")")" \
  "$(comment 200 41898282 "$(body_of "$other_sha" "$base_sha" "MERGE AS IS")")" \
  | jq -s .)
check 'newest comment on a different head' false "$comments"

# 3. A FIX FIRST verdict is never carried.
comments=$(comment 200 41898282 "$(body_of "$head_sha" "$base_sha" "FIX FIRST")")
check 'FIX FIRST verdict' false "$comments"

# 4. The base must match too.
comments=$(comment 200 41898282 "$(body_of "$head_sha" "$other_base" "MERGE AS IS")")
check 'different base' false "$comments"

# 5. Only github-actions[bot] comments count.
comments=$(comment 200 12345 "$(body_of "$head_sha" "$base_sha" "MERGE AS IS")")
check 'different comment author' false "$comments"

# A body edit keeps the head and base unchanged, so the workflow must exclude
# body edits from carry-forward altogether.
if ! grep -qF '!github.event.changes.body' "$workflow"; then
  echo 'FAIL: workflow does not exclude body edits from carry-forward' >&2
  exit 1
fi
printf 'ok: workflow excludes body edits from carry-forward\n'
