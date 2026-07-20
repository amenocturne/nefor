#!/usr/bin/env bash

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
plugin_prefix="$repo_root/plugins/"

cargo metadata --no-deps --format-version 1 --manifest-path "$repo_root/Cargo.toml" \
  | jq -r --arg prefix "$plugin_prefix" '
      .packages[]
      | select(.manifest_path | startswith($prefix))
      | .targets[]
      | select(.kind | index("bin"))
      | .name
    ' \
  | LC_ALL=C sort
