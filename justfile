default:
    @just --list

# Launch the nefor TUI against the in-repo starter and plugins (debug build).
run:
    RUST_LOG=debug NEFOR_CONFIG_DIR={{justfile_directory()}}/starter NEFOR_PLUGIN_DIR={{justfile_directory()}}/target/debug cargo run --bin nefor

# Fetch all workspace dependencies without compiling — warms the cache.
setup:
    cargo fetch

# Run the full workspace test suite.
test:
    cargo test --workspace

# Clippy across the workspace with warnings promoted to errors — matches CI.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format every Rust file with rustfmt, every markdown file with prettier.
fmt:
    cargo fmt --all
    npx --yes prettier@latest --write '**/*.md'

# Release build of the whole workspace into target/release/.
build:
    cargo build --workspace --release

# Composite: install-nefor + install-starter. End-to-end first-time setup.
install: install-nefor install-starter
    @echo
    @echo "Installed -> ${PREFIX:-$HOME/.local}/bin"
    @echo "Make sure your shell has:"
    @echo "  export PATH=\"${PREFIX:-$HOME/.local}/bin:\$PATH\""
    @echo "  export NEFOR_PLUGIN_DIR=\"${PREFIX:-$HOME/.local}/bin\""

# Build release + copy every shipped binary to $PREFIX/bin (default ~/.local/bin). Installs `da` if missing. PREFIX=/usr/local just install-nefor to override.
install-nefor:
    #!/usr/bin/env bash
    set -eu
    PREFIX="${PREFIX:-$HOME/.local}"
    cargo build --workspace --release
    mkdir -p "$PREFIX/bin"
    cd "{{justfile_directory()}}"
    # Copies (not symlinks) so future dev rebuilds in target/release/
    # don't silently mutate the installed binaries — re-run this recipe
    # to refresh.
    for bin in nefor openai-provider tool-gate basic-tools reasoner-graph nefor-tui mock-plugin generic-provider generic-tool nefor-combinators; do
      install -m 0755 "target/release/$bin" "$PREFIX/bin/$bin"
      echo "  $PREFIX/bin/$bin"
    done
    # The starter ships a tool-validator that classifies bash commands
    # via `da` (https://github.com/amenocturne/da) before any popup
    # fires. The validator falls back to "always defer" without it, but
    # auto-approval of safe read-only commands needs the binary.
    if command -v da >/dev/null 2>&1; then
      echo "  da (already installed) -> $(command -v da)"
    else
      echo "Installing da (bash-command classifier)..."
      cargo install --locked dabin
      echo "  da -> $(command -v da || echo '?')"
    fi

# Copy starter/ to ~/.config/nefor. Refuses if the dir exists; pass `force` to wipe and re-copy.
install-starter mode="safe":
    #!/usr/bin/env bash
    set -eu
    DEST=~/.config/nefor
    if [ -e "$DEST" ]; then
      if [ "{{mode}}" = "force" ]; then
        rm -rf "$DEST"
        echo "  removed $DEST (force)"
      else
        echo "  $DEST already exists; leaving it alone."
        echo "  (To wipe and re-copy: just install-starter force)"
        exit 0
      fi
    fi
    mkdir -p "$DEST"
    cp -R "{{justfile_directory()}}/starter/." "$DEST/"
    echo "  $DEST (copied from {{justfile_directory()}}/starter)"

# Remove the entire target/ directory. Next build is a full cold compile.
clean:
    cargo clean
