#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -ne 1 ]; then
  echo "usage: $0 <nefor-target-directory>" >&2
  exit 2
fi
directory=$1
parent=$(cd "$(dirname "$directory")" && pwd)
name=$(basename "$directory")
case "$name" in
  nefor-aarch64-apple-darwin|nefor-x86_64-unknown-linux-gnu|nefor-aarch64-unknown-linux-gnu) ;;
  *) echo "unsupported release root: $name" >&2; exit 2 ;;
esac
test -x "$directory/bin/nefor"
test -x "$directory/bin/mag"
test ! -e "$parent/$name.tar.gz"
# The package operation closes metadata before creating any archive.
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
python3 -B "$repo_root/tools/package-release.py" --finalize "$directory"
(cd "$parent" && COPYFILE_DISABLE=1 tar -czf "$name.tar.gz" "$name")
sha=$(shasum -a 256 "$parent/$name.tar.gz" | awk '{print $1}')
printf '%s  %s\n' "$sha" "$name.tar.gz" > "$parent/$name.tar.gz.sha256"
