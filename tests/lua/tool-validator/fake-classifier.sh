#!/bin/sh
# A stand-in command classifier for the validator tests. It keeps the exit
# contract the validator is written against (0 approve, 1 defer, 2 deny) and
# decides from its flags as well as from the command, so a test can tell which
# policy's flags it was given:
#   --git <list>   `git commit` is approved only when the list names commit
#   --model        a command containing MODEL is approved only with this flag
set -u
if [ "${1-}" = "--version" ]; then echo "fake-classifier 0"; exit 0; fi
git_scope=""
model=0
while [ $# -gt 0 ]; do
  case "$1" in
    --git) git_scope="${2-}"; shift ;;
    --model) model=1 ;;
  esac
  shift
done
command=$(cat)
case "$command" in
  *CRASH*) echo "classifier exploded" >&2; exit 70 ;;
  *MODEL*) [ "$model" = 1 ] && exit 0; exit 1 ;;
  "git commit"*) case ",$git_scope," in *,commit,*) exit 0 ;; *) exit 1 ;; esac ;;
  "'rm' "*|"rm "*|*"> "*) exit 2 ;;
  "'ls'"*|"ls"*|"cat "*|"'cat' "*|"rg "*) exit 0 ;;
  *) exit 1 ;;
esac
