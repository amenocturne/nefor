#!/usr/bin/env bash
# Sourced by native preparation; discovery never changes user configuration.
source_library() {
  local directory=$1 name extension
  shift
  for name in "$@"; do
    for extension in so dylib a; do
      if [ -f "$directory/lib$name.$extension" ]; then printf "%s\n" "$directory/lib$name.$extension"; return 0; fi
    done
  done
  return 1
}
source_luarocks_version() (
  set -e
  mkdir -p "${SOURCE_PREREQUISITES_TMP:-tmp}"
  local probe
  probe=$(mktemp -d "${SOURCE_PREREQUISITES_TMP:-tmp}/luarocks-version.XXXXXX")
  trap 'rm -rf "$probe"' EXIT
  export HOME="$probe" XDG_CONFIG_HOME="$probe/config" XDG_CACHE_HOME="$probe/cache"
  export LUAROCKS_SYSCONFDIR="$probe/system" LUAROCKS_CONFIG=/dev/null
  export LUAROCKS_CONFIG_5_1=/dev/null LUAROCKS_CONFIG_5_2=/dev/null LUAROCKS_CONFIG_5_3=/dev/null LUAROCKS_CONFIG_5_4=/dev/null LUAROCKS_CONFIG_5_5=/dev/null
  unset LUA_PATH LUA_CPATH LUA_INIT LUA_INIT_5_1 LUA_INIT_5_2 LUA_INIT_5_3 LUA_INIT_5_4 LUA_INIT_5_5
  luarocks --version > "$probe/version"
  cat "$probe/version" >&2
  grep -Eq "[[:space:]]3\.[0-9]+" "$probe/version" || { echo "source install requires LuaRocks 3.x" >&2; exit 1; }
)
source_prerequisites() {
  local os tool prefix candidate probe
  os=$(uname -s)
  case "$os" in Linux|Darwin) ;; *) echo "unsupported source-install OS: $os" >&2; return 1;; esac
  for tool in bash cc make m4 pkg-config luarocks curl unzip tar gzip git just cargo rustc; do
    command -v "$tool" >/dev/null || { echo "source install requires $tool (see docs/source-install.md)" >&2; return 1; }
  done
  make --version | grep -q 'GNU Make' || { echo 'source install requires GNU make' >&2; return 1; }
  m4 --version | grep -q "GNU M4" || { echo "source install requires GNU m4" >&2; return 1; }
  command -v sha256sum >/dev/null || command -v shasum >/dev/null || { echo 'source install requires sha256sum or shasum' >&2; return 1; }
  RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" RUSTUP_AUTO_INSTALL=0 rustc --version >&2 || { echo "usable installed Rust toolchain required" >&2; return 1; }
  RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" RUSTUP_AUTO_INSTALL=0 cargo --version >&2 || { echo "usable installed Cargo required" >&2; return 1; }
  source_luarocks_version || { echo "usable LuaRocks 3.x required (no global configuration is loaded)" >&2; return 1; }
  if [ "$os" = Darwin ]; then
    for tool in otool install_name_tool codesign realpath; do
      command -v "$tool" >/dev/null || { echo "source install requires $tool" >&2; return 1; }
    done
  fi
  if [ "${LUA_DEPENDENCIES_LUA_DIR+x}" = x ]; then
    prefix=$LUA_DEPENDENCIES_LUA_DIR
    [ -n "$prefix" ] && [ -d "$prefix" ] || { echo 'invalid LUA_DEPENDENCIES_LUA_DIR' >&2; return 1; }
  elif [ "$os" = Darwin ]; then
    prefix=$(brew --prefix lua@5.4 2>/dev/null) || { echo 'Lua 5.4 required: install lua@5.4 or set LUA_DEPENDENCIES_LUA_DIR' >&2; return 1; }
  else
    pkg-config --exists lua5.4 || { echo 'Lua 5.4 headers/library required (pkg-config lua5.4)' >&2; return 1; }
    prefix=$(pkg-config --variable=prefix lua5.4)
  fi
  LUA_SOURCE_PREFIX=$prefix
  LUA_SOURCE_BIN=''
  for candidate in "$prefix/bin/lua5.4" "$prefix/bin/lua54" "$prefix/bin/lua"; do
    if [ -x "$candidate" ] && [ "$("$candidate" -E -e 'io.write(_VERSION)' 2>/dev/null)" = 'Lua 5.4' ]; then LUA_SOURCE_BIN=$candidate; break; fi
  done
  [ -n "$LUA_SOURCE_BIN" ] || { echo "Lua 5.4 interpreter missing/wrong version in $prefix/bin" >&2; return 1; }
  LUA_SOURCE_INCLUDE=''
  for candidate in "$prefix/include/lua5.4" "$prefix/include/lua54" "$prefix/include"; do
    if [ -f "$candidate/lua.h" ] && grep -Eq 'LUA_VERSION_NUM[[:space:]]+504' "$candidate/lua.h"; then LUA_SOURCE_INCLUDE=$candidate; break; fi
  done
  [ -n "$LUA_SOURCE_INCLUDE" ] && [ -f "$LUA_SOURCE_INCLUDE/lauxlib.h" ] || { echo "Lua 5.4 headers missing/wrong version in $prefix/include" >&2; return 1; }
  if [ "$os" = Linux ] && [ "${LUA_DEPENDENCIES_LUA_DIR+x}" != x ]; then
    LUA_SOURCE_LIB=$(pkg-config --variable=libdir lua5.4)
  else LUA_SOURCE_LIB="$prefix/lib"; fi
  if [ "$os" = Linux ] && [ "${LUA_DEPENDENCIES_LUA_DIR+x}" = x ] && ! source_library "$LUA_SOURCE_LIB" lua5.4 lua54 lua >/dev/null; then
    for candidate in "$LUA_SOURCE_PREFIX/lib/$(cc -dumpmachine)" "$LUA_SOURCE_PREFIX/lib64"; do
      if source_library "$candidate" lua5.4 lua54 lua >/dev/null; then LUA_SOURCE_LIB=$candidate; break; fi
    done
  fi
  LUA_SOURCE_LIBRARY=$(source_library "$LUA_SOURCE_LIB" lua5.4 lua54 lua) || { echo "Lua 5.4 development library missing in $LUA_SOURCE_LIB" >&2; return 1; }
  if [ "${LUA_DEPENDENCIES_OPENSSL_DIR+x}" = x ]; then
    prefix=$LUA_DEPENDENCIES_OPENSSL_DIR
    [ -n "$prefix" ] && [ -d "$prefix" ] || { echo 'invalid LUA_DEPENDENCIES_OPENSSL_DIR' >&2; return 1; }
    OPENSSL_SOURCE_INCLUDE="$prefix/include"; OPENSSL_SOURCE_LIB="$prefix/lib"
  elif [ "$os" = Darwin ]; then
    prefix=$(brew --prefix openssl@3 2>/dev/null) || { echo 'OpenSSL 3 required: install openssl@3 or set LUA_DEPENDENCIES_OPENSSL_DIR' >&2; return 1; }
    OPENSSL_SOURCE_INCLUDE="$prefix/include"; OPENSSL_SOURCE_LIB="$prefix/lib"
  else
    pkg-config --exists openssl || { echo 'OpenSSL 3 development headers/library required (pkg-config openssl)' >&2; return 1; }
    prefix=$(pkg-config --variable=prefix openssl)
    OPENSSL_SOURCE_INCLUDE=$(pkg-config --variable=includedir openssl)
    OPENSSL_SOURCE_LIB=$(pkg-config --variable=libdir openssl)
  fi
  OPENSSL_SOURCE_PREFIX=$prefix
  if [ "$os" = Linux ] && [ "${LUA_DEPENDENCIES_OPENSSL_DIR+x}" = x ] && ! source_library "$OPENSSL_SOURCE_LIB" ssl >/dev/null; then
    for candidate in "$prefix/lib/$(cc -dumpmachine)" "$prefix/lib64"; do
      if source_library "$candidate" ssl >/dev/null; then OPENSSL_SOURCE_LIB=$candidate; break; fi
    done
  fi
  OPENSSL_SOURCE_SSL=$(source_library "$OPENSSL_SOURCE_LIB" ssl) || { echo "OpenSSL development library missing in $OPENSSL_SOURCE_LIB" >&2; return 1; }
  OPENSSL_SOURCE_CRYPTO=$(source_library "$OPENSSL_SOURCE_LIB" crypto) || { echo "OpenSSL crypto development library missing in $OPENSSL_SOURCE_LIB" >&2; return 1; }
  [ -f "$OPENSSL_SOURCE_INCLUDE/openssl/opensslv.h" ] && [ -f "$OPENSSL_SOURCE_INCLUDE/openssl/ssl.h" ] && [ -f "$OPENSSL_SOURCE_INCLUDE/openssl/crypto.h" ] || { echo "OpenSSL headers missing: $OPENSSL_SOURCE_INCLUDE" >&2; return 1; }
  # Compile/link probes establish usable headers and libraries, not just directories.
  mkdir -p "${SOURCE_PREREQUISITES_TMP:-tmp}"
  probe=$(mktemp -d "${SOURCE_PREREQUISITES_TMP:-tmp}/source-prerequisites.XXXXXX")
  if ! printf '#include <lua.h>\n#include <lauxlib.h>\nint main(void){lua_State *s=luaL_newstate();int ok=lua_version(s)==504;lua_close(s);return !ok;}\n' | cc -x c - -I"$LUA_SOURCE_INCLUDE" -L"$LUA_SOURCE_LIB" -Wl,-rpath,"$LUA_SOURCE_LIB" -x none "$LUA_SOURCE_LIBRARY" -o "$probe/lua" 2>"$probe/error"; then
    echo "Lua 5.4 development library not linkable" >&2; cat "$probe/error" >&2; rm -rf "$probe"; return 1
  fi
  if ! printf '#include <openssl/ssl.h>\n#if OPENSSL_VERSION_MAJOR != 3\n#error OpenSSL 3 required\n#endif\nint main(void){return OPENSSL_init_ssl(0,0)==0 || (OpenSSL_version_num() >> 28)!=3;}\n' | cc -x c - -I"$OPENSSL_SOURCE_INCLUDE" -L"$OPENSSL_SOURCE_LIB" -Wl,-rpath,"$OPENSSL_SOURCE_LIB" -x none "$OPENSSL_SOURCE_SSL" "$OPENSSL_SOURCE_CRYPTO" -o "$probe/ssl" 2>"$probe/error"; then
    echo 'OpenSSL 3 headers/library not usable' >&2; cat "$probe/error" >&2; rm -rf "$probe"; return 1
  fi
  if ! "$probe/lua" || ! "$probe/ssl"; then echo 'native prerequisite link probes cannot run' >&2; rm -rf "$probe"; return 1; fi
  rm -rf "$probe"
  echo "source prerequisites: Lua 5.4 $LUA_SOURCE_BIN; headers $LUA_SOURCE_INCLUDE; libs $LUA_SOURCE_LIB; OpenSSL 3 $OPENSSL_SOURCE_LIB" >&2
}
