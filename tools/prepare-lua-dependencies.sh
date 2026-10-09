#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <rockspec> <dependency-lock> <output-tree>" >&2
  exit 2
fi
rockspec=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
lock=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
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
# The declared rock and every locked rock must be satisfied by the locked closure.
# An empty server makes an unsatisfied dependency fail instead of resolving a new version.
mkdir -p "$stage/no-server"
satisfied() {
  luarocks "${lua_options[@]}" --tree="$stage/tree" --only-server="$stage/no-server" \
    make --only-deps --deps-mode=one "$1" >/dev/null \
    || { echo "locked dependency closure does not satisfy $1" >&2; exit 1; }
}
satisfied "$rockspec"
while IFS= read -r installed; do satisfied "$installed"; done \
  < <(find "$stage/tree/lib/luarocks" -name '*.rockspec' | sort)
"$tools_root/vendor-lua-dylibs.sh" "$stage/tree"
cp "$lock" "$stage/tree/dependencies.lock"
luarocks "${lua_options[@]}" --tree="$stage/tree" list --porcelain | cut -f 1-3 > "$stage/tree/installed-packages.tsv"
if [ -e "$tree" ]; then
  backup=$(mktemp -d "${tree}.previous.XXXXXX")
  rmdir "$backup"
  mv "$tree" "$backup"
  if ! mv "$stage/tree" "$tree"; then mv "$backup" "$tree"; exit 1; fi
  rm -rf "$backup"
else
  mv "$stage/tree" "$tree"
fi
