#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$repo/tmp"
fixture=$(mktemp -d "$repo/tmp/lua-dylibs.XXXXXX")
trap 'rm -rf "$fixture"' EXIT
export DYLIB_FIXTURE_ROOT="$fixture"
mkdir -p "$fixture/bin" "$fixture/source" "$fixture/tree/lib/lua/5.4/nested"
printf ssl > "$fixture/source/libssl.3.dylib"
printf crypto > "$fixture/source/libcrypto.3.dylib"
printf module > "$fixture/tree/lib/lua/5.4/module.so"
printf nested > "$fixture/tree/lib/lua/5.4/nested/module_nested.so"
cat > "$fixture/bin/uname" <<'SH'
#!/usr/bin/env bash
printf 'Darwin\n'
SH
cat > "$fixture/bin/otool" <<'SH'
#!/usr/bin/env bash
set -eu
mode=$1
file=$2
name=$(basename "$file")
printf '%s:\n' "$file"
if [ "$mode" = -D ]; then
  case "$name" in lib*.dylib) printf '%s/source/%s\n' "$DYLIB_FIXTURE_ROOT" "$name";; esac
else
  printf '\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n'
  case "$name" in
    module*)
      if [ "${DYLIB_FIXTURE_MODE:-}" = unresolved ]; then
        printf '\t@rpath/libssl.3.dylib (compatibility version 1.0.0, current version 1.0.0)\n'
      else
        printf '\t%s/source/libssl.3.dylib (compatibility version 1.0.0, current version 1.0.0)\n' "$DYLIB_FIXTURE_ROOT"
      fi;;
    libssl*) printf '\t%s/source/libssl.3.dylib (compatibility version 1.0.0, current version 1.0.0)\n\t%s/source/libcrypto.3.dylib (compatibility version 1.0.0, current version 1.0.0)\n' "$DYLIB_FIXTURE_ROOT" "$DYLIB_FIXTURE_ROOT";;
    libcrypto*) printf '\t%s/source/libcrypto.3.dylib (compatibility version 1.0.0, current version 1.0.0)\n' "$DYLIB_FIXTURE_ROOT";;
  esac
fi
SH
cat > "$fixture/bin/install_name_tool" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$DYLIB_FIXTURE_ROOT/relocations"
SH
cat > "$fixture/bin/codesign" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$DYLIB_FIXTURE_ROOT/signatures"
SH
chmod +x "$fixture/bin/"*
export PATH="$fixture/bin:$PATH"
"$repo/tools/vendor-lua-dylibs.sh" "$fixture/tree"
cmp "$fixture/source/libssl.3.dylib" "$fixture/tree/lib/native/libssl.3.dylib"
cmp "$fixture/source/libcrypto.3.dylib" "$fixture/tree/lib/native/libcrypto.3.dylib"
grep -F -- '@loader_path/../../native/libssl.3.dylib' "$fixture/relocations" >/dev/null
grep -F -- '@loader_path/../../../native/libssl.3.dylib' "$fixture/relocations" >/dev/null
grep -F -- '@loader_path/../native/libcrypto.3.dylib' "$fixture/relocations" >/dev/null
grep -F -- '-id @loader_path/libssl.3.dylib' "$fixture/relocations" >/dev/null
test "$(wc -l < "$fixture/signatures" | tr -d ' ')" = 4
if DYLIB_FIXTURE_MODE=unresolved "$repo/tools/vendor-lua-dylibs.sh" "$fixture/tree" >"$fixture/error" 2>&1; then
  echo 'unresolved native dependency incorrectly accepted' >&2
  exit 1
fi
grep -F 'unresolved native dependency' "$fixture/error" >/dev/null
printf 'native dependency relocation contract passed\n'
