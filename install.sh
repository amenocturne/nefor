#!/usr/bin/env bash
set -euo pipefail

NEFOR_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  echo "Usage: install.sh [target-dir] [--overlay <overlay-dir>]"
  echo "Install nefor-agent into <target-dir>/.pi/"
  echo "  target-dir       Directory to install into (default: ../)"
  echo "  --overlay <dir>  Layer additional files on top of .pi/ after install"
  exit 1
}

# Parse arguments
TARGET_DIR=""
OVERLAY_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --overlay)
      OVERLAY_DIR="${2:?--overlay requires a directory argument}"
      shift 2
      ;;
    -h|--help)
      usage
      ;;
    *)
      if [[ -z "$TARGET_DIR" ]]; then
        TARGET_DIR="$1"
      else
        echo "Error: unexpected argument '$1'" >&2
        usage
      fi
      shift
      ;;
  esac
done

[[ -z "$TARGET_DIR" ]] && TARGET_DIR=".."

# Resolve relative paths against cwd (not NEFOR_DIR)
[[ "$TARGET_DIR" != /* ]] && TARGET_DIR="$(pwd)/$TARGET_DIR"
[[ -n "$OVERLAY_DIR" && "$OVERLAY_DIR" != /* ]] && OVERLAY_DIR="$(pwd)/$OVERLAY_DIR"

# Validate
if [[ ! -d "$TARGET_DIR" ]]; then
  echo "Error: target directory does not exist: $TARGET_DIR" >&2
  exit 1
fi

if [[ -n "$OVERLAY_DIR" && ! -d "$OVERLAY_DIR" ]]; then
  echo "Error: overlay directory does not exist: $OVERLAY_DIR" >&2
  exit 1
fi

PI_DIR="$TARGET_DIR/.pi"
mkdir -p "$PI_DIR"

# Copy core files
DIRS=(lib extensions prompts instructions config)
for dir in "${DIRS[@]}"; do
  if [[ -d "$NEFOR_DIR/$dir" ]]; then
    rm -rf "$PI_DIR/$dir"
    cp -r "$NEFOR_DIR/$dir" "$PI_DIR/$dir"
    # Remove test files from the install
    find "$PI_DIR/$dir" -name '*.test.ts' -o -name '*.test.js' -o -name '*.spec.ts' -o -name '*.spec.js' | xargs rm -f 2>/dev/null || true
  fi
done

FILES=(disguise.ts prompt.md package.json)
for file in "${FILES[@]}"; do
  if [[ -f "$NEFOR_DIR/$file" ]]; then
    cp "$NEFOR_DIR/$file" "$PI_DIR/$file"
  fi
done

# Skip package-lock.json — npm creates empty scope dirs from it even with --omit=dev

# Copy includes/ for prompt assembly
if [[ -d "$NEFOR_DIR/includes" ]]; then
  rm -rf "$PI_DIR/includes"
  cp -r "$NEFOR_DIR/includes" "$PI_DIR/includes"
fi

# Install npm dependencies (strip devDependencies to keep node_modules clean)
if [[ -f "$PI_DIR/package.json" ]]; then
  (cd "$PI_DIR" && node -e "
    const p = JSON.parse(require('fs').readFileSync('package.json','utf8'));
    delete p.devDependencies; delete p.scripts;
    require('fs').writeFileSync('package.json', JSON.stringify(p, null, 2) + '\n');
  " 2>/dev/null || true)
  echo "Installing dependencies..."
  (cd "$PI_DIR" && npm install --omit=dev --silent 2>&1) || {
    echo "Warning: npm install failed, continuing anyway" >&2
  }
fi

# Assemble system prompt: prompt.md + includes/*
assemble_prompt() {
  local base="$PI_DIR/prompt.md"
  local includes_dir="$PI_DIR/includes"
  local assembled=""

  if [[ -f "$base" ]]; then
    assembled="$(cat "$base")"
  fi

  if [[ -d "$includes_dir" ]]; then
    for f in "$includes_dir"/*.md; do
      [[ -f "$f" ]] || continue
      assembled="${assembled}"$'\n\n'"$(cat "$f")"
    done
  fi

  if [[ -n "$assembled" ]]; then
    echo "$assembled" > "$base"
  fi
}

assemble_prompt

# Default hooks config
if [[ ! -f "$PI_DIR/hooks.yaml" ]]; then
  cat > "$PI_DIR/hooks.yaml" <<'YAML'
# Nefor hooks configuration
hooks:
  pre_tool_use:
    - command: "{hook_dir}/perm-hooks/smart-approve.sh"
YAML
fi

# Default settings
if [[ ! -f "$PI_DIR/settings.json" ]]; then
  cat > "$PI_DIR/settings.json" <<'JSON'
{
  "defaultProvider": "nestor",
  "defaultModel": "nestor/tgpt/qwen35-397b-a17b-fp8",
  "defaultThinkingLevel": "medium",
  "quietStartup": true
}
JSON
fi

# Apply overlay
if [[ -n "$OVERLAY_DIR" ]]; then
  echo "Applying overlay from $OVERLAY_DIR..."
  if command -v rsync &>/dev/null; then
    rsync -a "$OVERLAY_DIR/" "$PI_DIR/"
  else
    cp -r "$OVERLAY_DIR/." "$PI_DIR/"
  fi
  # Re-assemble prompt in case overlay added includes
  assemble_prompt
fi

# Create 'nefor' CLI alias (symlink to pi)
PI_BIN="$(command -v pi 2>/dev/null || true)"
if [[ -n "$PI_BIN" ]]; then
  BIN_DIR="$(dirname "$PI_BIN")"
  if [[ ! -e "$BIN_DIR/nefor" ]]; then
    ln -s "$PI_BIN" "$BIN_DIR/nefor"
    echo "Created 'nefor' symlink → $PI_BIN"
  fi
fi

echo "Nefor installed to $PI_DIR"
echo "  Core: lib/ extensions/ prompts/ instructions/ config/ hooks/"
echo "  Config: package.json, disguise.ts, prompt.md"
[[ -n "$OVERLAY_DIR" ]] && echo "  Overlay: applied from $OVERLAY_DIR"
echo ""
echo "Prerequisites:"
echo "  dp auth login   — authenticate with Nestor (required on first use)"
echo ""
echo "Run 'pi' from $TARGET_DIR to start."
echo "Tip: './install.sh <dir>' to install to a specific directory instead of the default (../)"
