#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 REF COMPONENT PROFILE OUTPUT_JSONL" >&2
  exit 2
}
[[ $# == 4 ]] || usage
ref=$1 component=$2 profile=$3 output=$4
root=$(git rev-parse --show-toplevel)
commit=$(git -C "$root" rev-parse --verify "$ref^{commit}")
case "$component" in
  workspace) packages=(--workspace) ;;
  engine) packages=(-p nefor) ;;
  tui) packages=(-p nefor-tui) ;;
  mag) packages=(-p mag-plugin) ;;
  providers) packages=(-p openai-provider -p chatgpt-provider) ;;
  *) echo "unknown component '$component'" >&2; exit 2 ;;
esac
case "$profile" in
  dev) profile_args=() ;;
  release) profile_args=(--release) ;;
  *) echo "unknown profile '$profile'" >&2; exit 2 ;;
esac

mkdir -p "$root/tmp/build-bench" "$(dirname "$output")"
run_dir=$(mktemp -d "$root/tmp/build-bench/run.XXXXXX")
checkout="$run_dir/checkout"
target="$run_dir/target"
trap 'git -C "$root" worktree remove --force "$checkout" >/dev/null 2>&1 || true; rm -rf "$run_dir"' EXIT
git -C "$root" worktree add --detach -q "$checkout" "$commit"
command=(cargo build --locked "${packages[@]}" "${profile_args[@]}")
command_text=$(printf '%q ' "${command[@]}")
: > "$output"

measure() {
  local mode=$1 timing log status elapsed size
  timing="$run_dir/$mode.time"
  log="$run_dir/$mode.log"
  set +e
  (cd "$checkout" && CARGO_TARGET_DIR="$target" /usr/bin/time -p -o "$timing" "${command[@]}") >"$log" 2>&1
  status=$?
  set -e
  elapsed=$(awk '$1 == "real" { print $2 }' "$timing")
  size=$(du -sk "$target" | awk '{ print $1 * 1024 }')
  jq -cn \
    --arg ref "$ref" --arg commit "$commit" --arg mode "$mode" \
    --arg component "$component" --arg profile "$profile" \
    --arg command "$command_text" --arg target "$target" \
    --arg rustc "$(rustc -Vv)" --arg cargo "$(cargo -V)" \
    --arg host "$(uname -smr)" --arg rustflags "${RUSTFLAGS-}" \
    --arg cargo_profile_dev_debug "${CARGO_PROFILE_DEV_DEBUG-}" \
    --argjson wall_seconds "$elapsed" --argjson exit_status "$status" \
    --argjson target_bytes "$size" \
    '{ref:$ref,commit:$commit,dirty:false,mode:$mode,component:$component,profile:$profile,command:$command,target:$target,wall_seconds:$wall_seconds,exit_status:$exit_status,target_bytes:$target_bytes,rustc:$rustc,cargo:$cargo,host:$host,rustflags:$rustflags,cargo_profile_dev_debug:$cargo_profile_dev_debug}' >> "$output"
  if (( status != 0 )); then
    cat "$log" >&2
    exit "$status"
  fi
}

rm -rf "$target"
measure cold
measure warm
cat "$output"
