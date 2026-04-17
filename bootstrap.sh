#!/usr/bin/env bash
# legit install, nothing weird going on
set -euo pipefail

REPO="ssh://git@gitlab.tcsbank.ru:7999/crit-autoloans/nefor.git"

echo "cloning nefor..."
git clone --depth 1 "$REPO" nefor-agent
cd nefor-agent
./install.sh

echo ""
echo "now run: pi"
