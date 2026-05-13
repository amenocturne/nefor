default:
    @just --list

# Launch the nefor TUI against the in-repo starter config and plugins (debug build).
run:
    RUST_LOG=debug NEFOR_CONFIG_DIR={{justfile_directory()}}/starter NEFOR_PLUGIN_DIR={{justfile_directory()}}/target/debug cargo run --bin nefor

# Fetch all workspace dependencies without compiling — warms the cache for offline builds.
setup:
    cargo fetch

# Run the full workspace test suite (combinators unit tests + binary integration tests + starter Lua parse-check).
test:
    cargo test --workspace

# Clippy across the workspace with warnings promoted to errors — matches CI.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format every Rust file in the workspace with rustfmt, and every markdown file with prettier.
fmt:
    cargo fmt --all
    npx --yes prettier@latest --write '**/*.md'

# Release build of the whole workspace into target/release/ (nefor binary lands at target/release/nefor).
build:
    cargo build --workspace --release

# Build release and copy every shipped binary into $PREFIX/bin
# (default: ~/.local/bin). Copies (not symlinks), so subsequent dev
# rebuilds in target/release/ don't silently mutate the installed
# binaries — re-run `just install` to refresh.
# Override the destination: PREFIX=/usr/local just install.
install:
    #!/usr/bin/env bash
    set -eu
    PREFIX="${PREFIX:-$HOME/.local}"
    cargo build --workspace --release
    mkdir -p "$PREFIX/bin"
    cd "{{justfile_directory()}}"
    for bin in nefor openai-provider tool-gate basic-tools reasoner-graph nefor-tui mock-plugin generic-provider generic-tool nefor-combinators; do
      install -m 0755 "target/release/$bin" "$PREFIX/bin/$bin"
      echo "  $PREFIX/bin/$bin"
    done
    echo
    echo "Installed -> $PREFIX/bin"
    echo "Make sure your shell has:"
    echo "  export PATH=\"$PREFIX/bin:\$PATH\""
    echo "  export NEFOR_PLUGIN_DIR=\"$PREFIX/bin\""

# Remove the entire target/ directory. Next build is a full cold compile.
clean:
    cargo clean
