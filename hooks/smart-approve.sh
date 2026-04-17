#!/usr/bin/env bash
# Auto-approve read-only tool calls. All others abstain (pi will prompt the user).
# Protocol: stdin = JSON payload, stdout = JSON decision, exit 0.
input=$(cat)
tool=$(printf '%s' "$input" | sed -n 's/.*"tool_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
case "$tool" in
  Read|Grep|Ls|Find|Glob)
    printf '{"decision":"allow"}\n'
    ;;
  *)
    printf '{"decision":"abstain"}\n'
    ;;
esac
