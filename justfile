default:
    @just --list

# Install nefor via brew and copy team config to ~/.config/nefor if missing.
install:
    #!/usr/bin/env bash
    set -eu
    brew tap amenocturne/tap 2>/dev/null || true
    brew install amenocturne/tap/nefor 2>/dev/null || brew upgrade amenocturne/tap/nefor
    just copy

# Copy team config to ~/.config/nefor. "safe" (default) skips if dest exists; "force" deletes first.
copy mode="safe":
    #!/usr/bin/env bash
    set -eu
    DEST=~/.config/nefor
    if [ "{{mode}}" = "force" ]; then
      rm -rf "$DEST"
      echo "Removed $DEST"
    fi
    if [ -e "$DEST" ]; then
      echo "$DEST already exists, skipping. Use \`just copy force\` to overwrite."
    else
      mkdir -p "$DEST"
      cp -R "{{justfile_directory()}}/starter/." "$DEST/"
      echo "Config copied to $DEST"
    fi

# Run nefor with team config.
run:
    RUST_LOG=info nefor --config $PWD/starter
