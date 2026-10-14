#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$repo/tmp"
uv run --no-project python -B "$repo/tools/runtime-package-tests.py"
