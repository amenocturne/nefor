#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -ne 1 ]; then echo "usage: $0 <prepared-lua-tree>" >&2; exit 2; fi
[ "$(uname -s)" = Darwin ] || exit 0
tree=$(cd "$1" && pwd)
mkdir -p "$tree/lib/native"
files=()
while IFS= read -r file; do files+=("$file"); done < <(find "$tree/lib/lua/5.4" -type f \( -name '*.so' -o -name '*.dylib' \))
origins=()
names=()
index=0
while [ "$index" -lt "${#files[@]}" ]; do
  file=${files[$index]}
  directory=$(dirname "$file")
  relative=""
  while [ "$directory" != "$tree/lib" ]; do
    [ "$directory" != / ] || { echo "native module outside tree: $file" >&2; exit 1; }
    relative="../$relative"
    directory=$(dirname "$directory")
  done
  library_id=$(otool -D "$file" | sed -n '2p')
  while IFS= read -r dependency; do
    if [ "$dependency" = "$library_id" ]; then
      install_name_tool -id "@loader_path/$(basename "$file")" "$file"
      continue
    fi
    case "$dependency" in
      /System/Library/*|/usr/lib/*) continue;;
      @*) echo "unresolved native dependency $dependency in $file" >&2; exit 1;;
    esac
    # Package managers reference one library through several symlinked paths,
    # so copies are keyed by the resolved file and keep the first referenced name.
    origin_path=$(realpath "$dependency") || { echo "missing native dependency $dependency in $file" >&2; exit 1; }
    name=""
    position=0
    while [ "$position" -lt "${#origins[@]}" ]; do
      if [ "${origins[$position]}" = "$origin_path" ]; then name=${names[$position]}; break; fi
      position=$((position + 1))
    done
    if [ -z "$name" ]; then
      name=$(basename "$dependency")
      for existing in ${names[@]+"${names[@]}"}; do
        if [ "$existing" = "$name" ]; then
          echo "conflicting native library basename: $name" >&2; exit 1
        fi
      done
      destination="$tree/lib/native/$name"
      cp -L "$dependency" "$destination"
      chmod u+w "$destination"
      origins+=("$origin_path")
      names+=("$name")
      files+=("$destination")
    fi
    chmod u+w "$file"
    install_name_tool -change "$dependency" "@loader_path/${relative}native/$name" "$file"
  done < <(otool -L "$file" | sed -n '2,$s/^[[:space:]]*\(.*\) (compatibility version.*$/\1/p')
  index=$((index + 1))
done
for file in ${files[@]+"${files[@]}"}; do codesign --force --sign - "$file"; done
