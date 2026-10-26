#!/usr/bin/env bash

set -u

# The scenario recipe already built or prepared these exact runtime binaries.
repo=$(cd "$(dirname "$0")/../.." && pwd)
target="${CARGO_TARGET_DIR:-$repo/target}"
if [[ "$target" != /* ]]; then target="$repo/$target"; fi
"$target/debug/nefor"
status=$?
printf '\nNEFOR_EXIT_CODE=%s\n' "$status"
exit "$status"
