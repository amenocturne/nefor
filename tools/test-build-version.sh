#!/usr/bin/env bash
set -euo pipefail

root=$(git rev-parse --show-toplevel)
tmp=$(mktemp -d "$root/tmp/build-version-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
repo="$tmp/repo"
target="$tmp/target"
mkdir -p "$repo/engine/src"
cp "$root/engine/build.rs" "$repo/engine/build.rs"
cat > "$repo/engine/Cargo.toml" <<'EOF'
[workspace]

[package]
name = "build-version-fixture"
version = "9.9.9"
edition = "2021"
EOF
cat > "$repo/engine/src/main.rs" <<'EOF'
fn main() { println!("{}", env!("NEFOR_VERSION")); }
EOF
cat > "$repo/README.md" <<'EOF'
fixture
EOF
cargo generate-lockfile --manifest-path "$repo/engine/Cargo.toml"

git -C "$repo" init -q
git -C "$repo" config user.email test@example.invalid
git -C "$repo" config user.name "Build version test"
git -C "$repo" add .
git -C "$repo" commit -qm initial
git -C "$repo" tag v1.0.0

build() {
  cargo build --manifest-path "$1/engine/Cargo.toml" --target-dir "$target" -v 2>&1
}
version() {
  "$target/debug/build-version-fixture"
}
assert_version() {
  local expected=$1 actual
  actual=$(version)
  if [[ "$actual" != "$expected" ]]; then
    echo "expected embedded version '$expected', got '$actual'" >&2
    exit 1
  fi
}

build "$repo" >/dev/null
assert_version 1.0.0
noop=$(build "$repo")
grep -q 'Fresh build-version-fixture' <<<"$noop" || {
  echo "unchanged normal checkout was not fresh" >&2
  echo "$noop" >&2
  exit 1
}

printf 'committed\n' >> "$repo/README.md"
git -C "$repo" add README.md
git -C "$repo" commit -qm move-head
build "$repo" >/dev/null
[[ $(version) == 1.0.0-1-g* ]] || { echo "HEAD movement did not update the version" >&2; exit 1; }

git -C "$repo" tag v1.1.0
git -C "$repo" pack-refs --all --prune
build "$repo" >/dev/null
assert_version 1.1.0

NEFOR_VERSION_OVERRIDE=release-test build "$repo" >/dev/null
assert_version release-test
build "$repo" >/dev/null
assert_version 1.1.0

linked="$tmp/linked"
git -C "$repo" worktree add -q -b linked-test "$linked"
rm -rf "$target"
build "$linked" >/dev/null
linked_noop=$(build "$linked")
grep -q 'Fresh build-version-fixture' <<<"$linked_noop" || {
  echo "unchanged linked checkout was not fresh" >&2
  echo "$linked_noop" >&2
  exit 1
}

printf 'build-version invalidation: ok\n'
