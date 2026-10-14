#!/usr/bin/env bash
# Opt-in public-network acceptance. Run in a fresh host/container with declared prerequisites.
set -euo pipefail
if [ "$#" != 2 ]; then echo "usage: $0 <exact-commit> <new-absolute-scratch-root>" >&2; exit 2; fi
cd "$(dirname "$0")/.."
rev=$(git rev-parse --verify "$1^{commit}")
[ "$(git rev-parse HEAD)" = "$rev" ] || { echo 'acceptance requires exact checkout HEAD' >&2; exit 1; }
[ -z "$(git status --porcelain --untracked-files=no)" ] || { echo 'acceptance requires clean tracked source' >&2; exit 1; }
root=$2
case "$root" in /*) ;; *) echo 'scratch root must be absolute' >&2; exit 1;; esac
[ ! -e "$root" ] || { echo 'scratch root must not already exist' >&2; exit 1; }
mkdir -p "$root/home" "$root/config" "$root/cache" "$root/data" "$root/state" "$root/cargo"
# Keep the explicitly installed rustup toolchain, but not its Cargo crate cache.
export HOME="$root/home" XDG_CONFIG_HOME="$root/config" XDG_CACHE_HOME="$root/cache" XDG_DATA_HOME="$root/data" XDG_STATE_HOME="$root/state" CARGO_HOME="$root/cargo"
unset LUA_DEPENDENCIES_LUA_DIR LUA_DEPENDENCIES_OPENSSL_DIR LUA_PATH LUA_CPATH LUAROCKS_CONFIG LUAROCKS_CONFIG_5_4 LUAROCKS_SYSCONFDIR CARGO_TARGET_DIR
export SOURCE_PREREQUISITES_TMP="$root/probes"
just install-source "$rev" nefor "$root/engine" "$root/mag" "$root/plugins" "$root/target"
cmp lua/nefor-pm/dependencies.lock "$root/target/lua-dependencies/dependencies.lock"
just test-pm-http "$root/engine/nefor" "$root/target/lua-dependencies" "$root/http-smoke"
echo "source-install acceptance passed: $rev at $root"
