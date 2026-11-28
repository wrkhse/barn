#!/usr/bin/env bash
set -euo pipefail
: "${REPO:?REPO is required}" "${PR:?PR is required}"
gh api "repos/$REPO/pulls/$PR" --jq '.body // ""' | sha256sum | cut -d' ' -f1
