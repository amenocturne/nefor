#!/usr/bin/env bash

set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <target-bin-dir>" >&2
  exit 2
fi

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bundle_tmp=$(mktemp -d "${TMPDIR:-/tmp}/nefor-release-bundle.XXXXXX")
trap 'rm -rf "$bundle_tmp"' EXIT

dist_dir="$bundle_tmp/nefor-test-target"
"$repo_root/tools/bundle-release.sh" "$1" "$dist_dir"

expected="$bundle_tmp/expected"
actual="$bundle_tmp/actual"
"$repo_root/tools/plugin-binaries.sh" > "$expected"
for plugin_path in "$dist_dir/share/nefor/plugins/"*; do
  [ -f "$plugin_path" ] || continue
  basename "$plugin_path"
done | LC_ALL=C sort > "$actual"

diff -u "$expected" "$dist_dir/share/nefor/plugins.manifest"
diff -u "$expected" "$actual"
grep -qx 'mag-plugin' "$expected"
if grep -qx 'mag' "$expected"; then
  echo "compiler CLI 'mag' must not be packaged as a runtime plugin" >&2
  exit 1
fi
test -x "$dist_dir/bin/nefor"
test -x "$dist_dir/bin/mag"
test -f "$dist_dir/share/nefor/starter/init.lua"
