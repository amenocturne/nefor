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
# The assembly contract stays offline; real native loading has separate tests.
lua_dependencies=${NEFOR_LUA_DEPENDENCIES_ROOT:-"$bundle_tmp/lua-dependencies"}
if [ -z "${NEFOR_LUA_DEPENDENCIES_ROOT:-}" ]; then
  mkdir -p "$lua_dependencies/share/lua/5.4/http" "$lua_dependencies/lib/lua/5.4"
  printf 'locked dependency fixture\n' > "$lua_dependencies/dependencies.lock"
  printf 'Lua module fixture\n' > "$lua_dependencies/share/lua/5.4/http/request.lua"
  printf 'native module fixture\n' > "$lua_dependencies/lib/lua/5.4/_cqueues.so"
fi
NEFOR_LUA_DEPENDENCIES_ROOT="$lua_dependencies" "$repo_root/tools/bundle-release.sh" "$1" "$dist_dir"
diff -qr "$lua_dependencies" "$dist_dir/share/nefor/runtime/lua-dependencies"

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
test -f "$dist_dir/share/nefor/examples/nefor-agent/init.lua"

test -f "$dist_dir/share/nefor/runtime/lua/nefor-pm/init.lua"
test -f "$dist_dir/share/nefor/runtime/lua-dependencies/dependencies.lock"
test -f "$dist_dir/share/nefor/runtime/lua-dependencies/share/lua/5.4/http/request.lua"
test -f "$dist_dir/share/nefor/runtime/lua-dependencies/lib/lua/5.4/_cqueues.so"
test -f "$dist_dir/share/nefor/runtime/lua/core/ncp.lua"
test -f "$dist_dir/share/nefor/runtime/lua/libs/agentic-loop/init.lua"
test -f "$dist_dir/share/nefor/runtime/plugins/nefor-tui/lua/init.lua"
test -f "$dist_dir/share/nefor/runtime/plugins/mag/lua/mag-kernel/init.lua"
test -f "$dist_dir/share/nefor/runtime/examples/nefor-agent/agentic-loop/lead-turn.mag"
test -f "$dist_dir/share/nefor/runtime/mag/lib/nefor/actors.mag"
test -f "$dist_dir/share/nefor/runtime/mag/book/README.md"
test -f "$dist_dir/share/nefor/runtime/mag/book/01. core/00. MAG in Five Minutes.md"
test -f "$dist_dir/share/nefor/runtime/mag/book/02. nefor/00. Nefor MAG in Five Minutes.md"
test ! -e "$dist_dir/share/nefor/runtime/examples/nefor-agent/mag/lib/nefor/actors.mag"
diff -qr "$repo_root/mag" "$dist_dir/share/nefor/runtime/mag"
if rg -n 'plugins\.manifest|resolve_plugin_root|PluginRoot|NEFOR_PLUGIN_DIR' "$repo_root/engine/src"; then
  echo "engine must not consume distribution inventory or discover plugin roots" >&2
  exit 1
fi
