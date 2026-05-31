default:
    @just --list

# Install the exact pinned nefor and overwrite ~/.config/nefor with this repo's starter config.
sync:
    #!/usr/bin/env bash
    set -eu
    ROOT="{{justfile_directory()}}"
    ENV_FILE="$ROOT/.env"
    if [ ! -f "$ENV_FILE" ]; then
      echo "Missing $ENV_FILE" >&2
      exit 1
    fi
    set -a
    . "$ENV_FILE"
    set +a
    if [ -z "${NEFOR_VERSION:-}" ]; then
      echo "NEFOR_VERSION is not set in $ENV_FILE" >&2
      exit 1
    fi
    printf 'This will install nefor %s and overwrite ~/.config/nefor from starter/. Continue? [y/N] ' "$NEFOR_VERSION"
    read -r answer
    case "$answer" in
      y|Y|yes|YES) ;;
      *) echo "Aborted."; exit 1 ;;
    esac
    brew tap amenocturne/tap 2>/dev/null || true
    if ! brew info "amenocturne/tap/nefor@$NEFOR_VERSION" >/dev/null 2>&1; then
      echo "Pinned nefor formula amenocturne/tap/nefor@$NEFOR_VERSION is unavailable; refusing to install latest." >&2
      exit 1
    fi
    brew install "amenocturne/tap/nefor@$NEFOR_VERSION"
    INSTALLED="$(nefor --version 2>/dev/null | awk '{print $2}')"
    if [ "$INSTALLED" != "$NEFOR_VERSION" ]; then
      echo "Installed nefor version $INSTALLED does not match pinned $NEFOR_VERSION" >&2
      exit 1
    fi
    DEST="$HOME/.config/nefor"
    rm -rf "$DEST"
    mkdir -p "$DEST"
    cp -R "$ROOT/starter/." "$DEST/"
    cp "$ENV_FILE" "$DEST/.env"
    echo "Synced nefor $NEFOR_VERSION config to $DEST"

# Backward-compatible alias for the exact pinned sync flow.
install: sync

# Backward-compatible config copy; force overwrites, safe skips if present.
copy mode="safe":
    #!/usr/bin/env bash
    set -eu
    ROOT="{{justfile_directory()}}"
    DEST="$HOME/.config/nefor"
    if [ "{{mode}}" = "force" ]; then
      rm -rf "$DEST"
      echo "Removed $DEST"
    fi
    if [ -e "$DEST" ]; then
      echo "$DEST already exists, skipping. Use \`just copy force\` to overwrite."
    else
      mkdir -p "$DEST"
      cp -R "$ROOT/starter/." "$DEST/"
      cp "$ROOT/.env" "$DEST/.env"
      echo "Config copied to $DEST"
    fi

# Run nefor with team config.
run:
    RUST_LOG=info nefor --config $PWD/starter
