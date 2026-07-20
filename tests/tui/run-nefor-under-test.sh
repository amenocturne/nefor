#!/usr/bin/env bash

set -u

just run
status=$?
printf '\nNEFOR_EXIT_CODE=%s\n' "$status"
exit "$status"
