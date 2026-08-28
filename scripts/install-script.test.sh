#!/bin/sh
set -eu

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
GRAFT_INSTALLER_TEST_MODE=true
export GRAFT_INSTALLER_TEST_MODE
. "${repository_root}/install.sh"

assert_latest_stable_tag() {
  expected="$1"
  release_json="$2"
  actual="$(printf '%s\n' "$release_json" | latest_stable_cli_tag /dev/stdin)"
  if [ "$actual" != "$expected" ]; then
    printf 'expected latest stable CLI tag %s, got %s\n' "$expected" "${actual:-<empty>}" >&2
    exit 1
  fi
}

assert_latest_stable_tag \
  "v0.15.4" \
  '[{"tag_name":"graft-sdk-v0.3.24"},{"tag_name":"v0.15.5-rc.1"},{"tag_name":"v0.15.4"},{"tag_name":"v0.15.3"}]'

assert_latest_stable_tag \
  "v0.15.4" \
  '[
    {
      "tag_name": "graft-sdk-v0.3.24"
    },
    {
      "tag_name": "v0.15.4"
    }
  ]'
