#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: $0 <rockspec> <dependency-lock> <output-tree> <source-root>" >&2
  exit 2
fi
rockspec=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
lock=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
source_root=$(cd "$4" && pwd)
tools_root=$(cd "$(dirname "$0")" && pwd)
lua_options=(--lua-version=5.4)
if [ -n "${LUA_DEPENDENCIES_LUA_DIR:-}" ]; then lua_options+=(--lua-dir="$LUA_DEPENDENCIES_LUA_DIR"); fi
build_options=()
if [ -n "${LUA_DEPENDENCIES_OPENSSL_DIR:-}" ]; then
  build_options+=("OPENSSL_DIR=$LUA_DEPENDENCIES_OPENSSL_DIR" "CRYPTO_DIR=$LUA_DEPENDENCIES_OPENSSL_DIR")
fi
mkdir -p "$(dirname "$3")"
tree=$(cd "$(dirname "$3")" && pwd)/$(basename "$3")
command -v luarocks >/dev/null || { echo "LuaRocks with Lua 5.4 is required for distribution preparation" >&2; exit 1; }

hash_file() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d ' ' -f 1
  else shasum -a 256 "$1" | cut -d ' ' -f 1; fi
}

stage=$(mktemp -d "${tree}.prepare.XXXXXX")
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/downloads" "$stage/tree"
while read -r name version architecture digest extra; do
  case "$name" in ''|'#'*) continue;; esac
  if [ -n "${extra:-}" ] || [[ ! "$name" =~ ^[a-zA-Z0-9_-]+$ ]] || [[ ! "$version" =~ ^[a-zA-Z0-9_.-]+$ ]] || [[ ! "$architecture" =~ ^(src|all)$ ]] || [[ ! "$digest" =~ ^[a-f0-9]{64}$ ]]; then
    echo "invalid dependency lock row for $name" >&2
    exit 1
  fi
  (
    cd "$stage/downloads"
    luarocks "${lua_options[@]}" download --arch "$architecture" "$name" "$version"
  )
  archive="$stage/downloads/$name-$version.$architecture.rock"
  if [ "$(hash_file "$archive")" != "$digest" ]; then
    echo "dependency checksum mismatch: $name $version" >&2
    exit 1
  fi
  luarocks "${lua_options[@]}" --tree="$stage/tree" install --deps-mode=none "$archive" ${build_options[@]+"${build_options[@]}"}
done < "$lock"
(
  cd "$source_root"
  luarocks "${lua_options[@]}" --tree="$stage/tree" make --deps-mode=none "$rockspec" ${build_options[@]+"${build_options[@]}"}
)
"$tools_root/vendor-lua-dylibs.sh" "$stage/tree"
cp "$lock" "$stage/tree/dependencies.lock"
luarocks "${lua_options[@]}" --tree="$stage/tree" list --porcelain > "$stage/tree/installed-packages.tsv"
if [ -e "$tree" ]; then
  backup=$(mktemp -d "${tree}.previous.XXXXXX")
  rmdir "$backup"
  mv "$tree" "$backup"
  if ! mv "$stage/tree" "$tree"; then mv "$backup" "$tree"; exit 1; fi
  rm -rf "$backup"
else
  mv "$stage/tree" "$tree"
fi
