default:
    @just --list

# Install the exact pinned nefor (stable) or use local dev nefor, then overwrite ~/.config/nefor with this repo's starter config.
sync mode="":
    #!/usr/bin/env bash
    set -eu
    ROOT="{{justfile_directory()}}"
    ENV_FILE="$ROOT/.env"
    ENV_LOCAL_FILE="$ROOT/.env.local"
    if [ ! -f "$ENV_FILE" ]; then
      echo "Missing $ENV_FILE" >&2
      exit 1
    fi
    set -a
    . "$ENV_FILE"
    if [ -f "$ENV_LOCAL_FILE" ]; then
      . "$ENV_LOCAL_FILE"
    fi
    set +a
    if [ -z "${NEFOR_VERSION:-}" ]; then
      echo "NEFOR_VERSION is not set in $ENV_FILE" >&2
      exit 1
    fi

    FORCE="${NEFOR_TEAM_INSTALL_FORCE:-0}"
    case "{{mode}}" in
      "") ;;
      force) FORCE=1 ;;
      safe) FORCE=0 ;;
      *) echo "Usage: just sync [force|safe]" >&2; exit 1 ;;
    esac

    if [ "${FORCE:-0}" != "1" ]; then
      if [ -n "${NEFOR_DEV_DIR:-}" ]; then
        printf 'This will use local nefor dev channel at %s and overwrite ~/.config/nefor from starter/. Continue? [y/N] ' "$NEFOR_DEV_DIR"
      else
        printf 'This will install nefor %s and overwrite ~/.config/nefor from starter/. Continue? [y/N] ' "$NEFOR_VERSION"
      fi
      read -r answer
      case "$answer" in
        y|Y|yes|YES) ;;
        *) echo "Aborted."; exit 1 ;;
      esac
    fi

    if [ -n "${NEFOR_DEV_DIR:-}" ]; then
      echo "Dev mode: NEFOR_DEV_DIR=$NEFOR_DEV_DIR"
      if command -v nefor >/dev/null 2>&1; then
        echo "Dev mode: $(nefor --version 2>/dev/null || true)"
      else
        echo "Dev mode: nefor binary not found on PATH; assuming local install will provide it at runtime" >&2
      fi
      echo "Dev mode: skipping Homebrew install and pinned-version check"
    else
      brew tap amenocturne/tap 2>/dev/null || true
      FORMULA="amenocturne/tap/nefor@$NEFOR_VERSION"
      BREW_INFO_OUTPUT="$(brew info "$FORMULA" 2>&1)" || {
        printf '%s\n' "$BREW_INFO_OUTPUT" >&2
        if printf '%s\n' "$BREW_INFO_OUTPUT" | grep -Eiq 'untrusted tap|Refusing to load formula|brew trust'; then
          echo "Homebrew refused to load the amenocturne/tap formula because the tap is not trusted." >&2
          echo "Review the tap before trusting it, then run: brew trust amenocturne/tap" >&2
          exit 2
        fi
        echo "Pinned nefor formula $FORMULA is unavailable; refusing to install latest." >&2
        exit 1
      }
      brew install "amenocturne/tap/nefor@$NEFOR_VERSION"
      INSTALLED="$(nefor --version 2>/dev/null | awk '{print $2}')"
      if [ "$INSTALLED" != "$NEFOR_VERSION" ]; then
        echo "Installed nefor version $INSTALLED does not match pinned $NEFOR_VERSION" >&2
        exit 1
      fi
    fi

    DEST="$HOME/.config/nefor"
    rm -rf "$DEST"
    mkdir -p "$DEST"
    cp -R "$ROOT/starter/." "$DEST/"
    cp "$ENV_FILE" "$DEST/.env"
    if [ -f "$ENV_LOCAL_FILE" ]; then
      cp "$ENV_LOCAL_FILE" "$DEST/.env.local"
    fi
    if [ -n "${NEFOR_DEV_DIR:-}" ]; then
      echo "Synced nefor dev config to $DEST"
    else
      echo "Synced nefor $NEFOR_VERSION config to $DEST"
    fi

# Backward-compatible alias for the sync flow.
install mode="": (sync mode)

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
      if [ -f "$ROOT/.env.local" ]; then
        cp "$ROOT/.env.local" "$DEST/.env.local"
      fi
      echo "Config copied to $DEST"
    fi

# Run nefor with team config.
run:
    #!/usr/bin/env bash
    set -eu
    ROOT="{{justfile_directory()}}"
    set -a
    . "$ROOT/.env"
    if [ -f "$ROOT/.env.local" ]; then
      . "$ROOT/.env.local"
    fi
    set +a
    RUST_LOG="${RUST_LOG:-info}" nefor --config "$ROOT/starter"
