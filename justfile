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

# Composite: install-nefor + install-starter. End-to-end first-time setup. Channel forwarded to install-nefor.
install channel="source": (install-nefor channel) install-starter
    @echo
    @echo "Installed -> ~/.local/share/nefor/bin (plugins + da), ${PREFIX:-$HOME/.local}/bin/nefor (CLI entry)"
    @echo "Make sure your shell has:"
    @echo "  export PATH=\"${PREFIX:-$HOME/.local}/bin:\$PATH\""

# Install nefor binaries (and `da`) for `channel`: source (cargo build, default for now) | latest (brew, TODO) | nightly (brew --HEAD, TODO). Plugins + da land in ~/.local/share/nefor/bin (the engine's default plugin root); only `nefor` itself goes on PATH.
install-nefor channel="source":
    #!/usr/bin/env bash
    set -eu
    PREFIX="${PREFIX:-$HOME/.local}"
    LIBEXEC_ROOT="$HOME/.local/share/nefor"
    LIBEXEC_BIN="$LIBEXEC_ROOT/bin"
    case "{{channel}}" in
      source)
        cargo build --workspace --release
        mkdir -p "$PREFIX/bin" "$LIBEXEC_BIN"
        cd "{{justfile_directory()}}"
        # Only `nefor` (the CLI entry point) goes on PATH. Every plugin
        # binary lands in the libexec dir, which the engine treats as
        # the default plugin root via the data_root_bin resolver path —
        # no NEFOR_PLUGIN_DIR export required.
        install -m 0755 "target/release/nefor" "$PREFIX/bin/nefor"
        echo "  $PREFIX/bin/nefor"
        for bin in openai-provider tool-gate basic-tools reasoner-graph nefor-tui mock-plugin generic-provider generic-tool nefor-combinators; do
          install -m 0755 "target/release/$bin" "$LIBEXEC_BIN/$bin"
          echo "  $LIBEXEC_BIN/$bin"
        done
        ;;
      latest)
        echo "channel=latest is not yet implemented (brew formula + release pipeline pending). Use 'source' for now: \`just install-nefor source\`"
        exit 1
        ;;
      nightly)
        echo "channel=nightly is not yet implemented (nightly tag + brew --HEAD pending). Use 'source' for now: \`just install-nefor source\`"
        exit 1
        ;;
      *)
        echo "unknown channel '{{channel}}'; expected source | latest | nightly"
        exit 1
        ;;
    esac
    # `da` lands in the same libexec dir (`cargo install --root` puts
    # the binary at <root>/bin/da). Keeps the user's PATH clean — da is
    # a nefor implementation detail. The validator finds it via the same
    # path; PATH is the fallback for users who happen to have it
    # installed elsewhere (e.g. their own `cargo install dabin`).
    if [ -x "$LIBEXEC_BIN/da" ]; then
      echo "  da (already installed) -> $LIBEXEC_BIN/da"
    else
      echo "Installing da -> $LIBEXEC_BIN/da..."
      cargo install --locked --root "$LIBEXEC_ROOT" dabin
      echo "  $LIBEXEC_BIN/da"
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
