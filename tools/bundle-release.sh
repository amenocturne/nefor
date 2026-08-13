#!/usr/bin/env bash

set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <target-bin-dir> <dist-dir>" >&2
  exit 2
fi

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
target_bin=$1
dist_dir=$2

if [ -e "$dist_dir" ]; then
  echo "distribution path already exists: $dist_dir" >&2
  exit 2
fi
if [ ! -x "$target_bin/nefor" ]; then
  echo "missing engine binary: $target_bin/nefor" >&2
  exit 1
fi

plugin_dir="$dist_dir/share/nefor/plugins"
manifest="$dist_dir/share/nefor/plugins.manifest"
runtime_root="$dist_dir/share/nefor/runtime"
mkdir -p "$dist_dir/bin" "$plugin_dir" "$dist_dir/share/nefor/examples/nefor-agent" "$runtime_root/lua" "$runtime_root/plugins"
cp "$target_bin/nefor" "$dist_dir/bin/nefor"
if [ ! -x "$target_bin/mag" ]; then
  echo "missing compiler binary: $target_bin/mag" >&2
  exit 1
fi
cp "$target_bin/mag" "$dist_dir/bin/mag"

while IFS= read -r name; do
  [ -n "$name" ] || continue
  if [ ! -x "$target_bin/$name" ]; then
    echo "missing plugin binary: $target_bin/$name" >&2
    exit 1
  fi
  cp "$target_bin/$name" "$plugin_dir/$name"
  printf '%s\n' "$name" >> "$manifest"
done < <("$repo_root/tools/plugin-binaries.sh")

if [ ! -s "$manifest" ]; then
  echo "plugin manifest is empty" >&2
  exit 1
fi


cp -R "$repo_root/lua/." "$runtime_root/lua/"
for plugin_lua in "$repo_root"/plugins/*/lua; do
  [ -d "$plugin_lua" ] || continue
  plugin_name=$(basename "$(dirname "$plugin_lua")")
  mkdir -p "$runtime_root/plugins/$plugin_name"
  cp -R "$plugin_lua" "$runtime_root/plugins/$plugin_name/lua"
done
mkdir -p "$runtime_root/examples/nefor-agent"
cp -R "$repo_root/examples/nefor-agent/." "$runtime_root/examples/nefor-agent/"

cp -R "$repo_root/examples/nefor-agent/." "$dist_dir/share/nefor/examples/nefor-agent/"
cp "$repo_root/LICENSE" "$repo_root/README.md" "$dist_dir/share/nefor/"
if [ -f "$repo_root/CHANGELOG.md" ]; then
  cp "$repo_root/CHANGELOG.md" "$dist_dir/share/nefor/"
fi
