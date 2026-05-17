default:
    @just --list

# Install nefor via brew and copy team config to ~/.config/nefor if missing.
install:
    #!/usr/bin/env bash
    set -eu
    brew tap amenocturne/tap 2>/dev/null || true
    brew install amenocturne/tap/nefor 2>/dev/null || brew upgrade amenocturne/tap/nefor
    DEST=~/.config/nefor
    if [ -e "$DEST" ]; then
      echo "$DEST already exists, skipping config copy."
    else
      mkdir -p "$DEST"
      cp -R "{{justfile_directory()}}/starter/." "$DEST/"
      echo "Config copied to $DEST"
    fi
    echo "Done. Run \`nefor\` from anywhere."

# Run nefor with team config.
run:
    RUST_LOG=info nefor --config $PWD/starter
