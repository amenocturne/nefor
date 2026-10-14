#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <rockspec> <dependency-lock> <output-tree>" >&2
  exit 2
fi
rockspec=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
lock=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
tools_root=$(cd "$(dirname "$0")" && pwd)
source "$tools_root/source-prerequisites.sh"
source_prerequisites
lua_options=(--lua-version=5.4 --lua-dir="$LUA_SOURCE_PREFIX")
build_options=("LUA=$LUA_SOURCE_BIN" "LUA_BINDIR=$(dirname "$LUA_SOURCE_BIN")" "LUA_INCDIR=$LUA_SOURCE_INCLUDE" "LUA_LIBDIR=$LUA_SOURCE_LIB" "OPENSSL_DIR=$OPENSSL_SOURCE_PREFIX" "OPENSSL_INCDIR=$OPENSSL_SOURCE_INCLUDE" "OPENSSL_LIBDIR=$OPENSSL_SOURCE_LIB" "CRYPTO_DIR=$OPENSSL_SOURCE_PREFIX" "CRYPTO_INCDIR=$OPENSSL_SOURCE_INCLUDE" "CRYPTO_LIBDIR=$OPENSSL_SOURCE_LIB")
mkdir -p "$(dirname "$3")"
tree=$(cd "$(dirname "$3")" && pwd)/$(basename "$3")

hash_file() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d ' ' -f 1
  else shasum -a 256 "$1" | cut -d ' ' -f 1; fi
}

stage=$(mktemp -d "${tree}.prepare.XXXXXX")
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/downloads" "$stage/tree" "$stage/home" "$stage/config" "$stage/cache" "$stage/sysconfig"
export HOME="$stage/home" XDG_CONFIG_HOME="$stage/config" XDG_CACHE_HOME="$stage/cache"
export LUAROCKS_SYSCONFDIR="$stage/sysconfig" LUAROCKS_CONFIG="$stage/config.lua" LUAROCKS_CONFIG_5_4="$stage/config.lua"
printf "rocks_servers = {}\n" > "$stage/config.lua"
unset LUA_PATH LUA_CPATH LUA_INIT LUA_INIT_5_4 LUAROCKS_CONFIG_5_5 LUAROCKS_ROCKS_TREE LUAROCKS_CONFIG_5_1 LUAROCKS_CONFIG_5_2 LUAROCKS_CONFIG_5_3
cd "$stage"
luarocks --version >&2

while read -r name version architecture digest url extra; do
  case "$name" in ''|'#'*) continue;; esac
  if [ -n "${extra:-}" ] || [[ ! "$name" =~ ^[a-zA-Z0-9_-]+$ ]] || [[ ! "$version" =~ ^[a-zA-Z0-9_.-]+$ ]] || [[ ! "$architecture" =~ ^(src|all)$ ]] || [[ ! "$digest" =~ ^[a-f0-9]{64}$ ]] || [[ ! "$url" =~ ^https://[a-zA-Z0-9.-]+/[^[:space:]]+\.rock$ ]]; then
    echo "invalid dependency lock row for $name" >&2
    exit 1
  fi
  if grep -qxF "$name" "$stage/identities" 2>/dev/null; then echo "duplicate dependency: $name" >&2; exit 1; fi
  echo "$name" >> "$stage/identities"
done < "$lock"
[ -s "$stage/identities" ] || { echo "empty dependency lock" >&2; exit 1; }
# Validate the entire lock before any network request or native build.
while read -r name version architecture digest url extra; do
  case "$name" in ""|'#'*) continue;; esac
  echo "Downloading $name $version: $url" >&2
  curl --fail --location --show-error --proto =https --proto-redir =https --connect-timeout 20 --max-time 180 "$url" -o "$stage/downloads/$name-$version.$architecture.rock"
  archive="$stage/downloads/$name-$version.$architecture.rock"
  if [ "$(hash_file "$archive")" != "$digest" ]; then
    echo "dependency checksum mismatch: $name $version" >&2
    exit 1
  fi
done < "$lock"
while read -r name version architecture digest url extra; do
  case "$name" in ""|'#'*) continue;; esac
  archive="$stage/downloads/$name-$version.$architecture.rock"
  luarocks "${lua_options[@]}" --tree="$stage/tree" install --deps-mode=none "$archive" ${build_options[@]+"${build_options[@]}"}
done < "$lock"
# The declared rock and every locked rock must be satisfied by the locked closure.
# An empty server makes an unsatisfied dependency fail instead of resolving a new version.
mkdir -p "$stage/no-server"
satisfied() {
  luarocks "${lua_options[@]}" --tree="$stage/tree" --only-server="$stage/no-server" \
    make --only-deps --deps-mode=one "$1" "${build_options[@]}" >/dev/null \
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
