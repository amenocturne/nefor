default:
    @just --list

# Launch the nefor TUI against the in-repo starter and plugins (debug build).
run:
    RUST_LOG=debug NEFOR_DEV_DIR={{justfile_directory()}} NEFOR_CONFIG_DIR={{justfile_directory()}}/starter NEFOR_PLUGIN_DIR={{justfile_directory()}}/target/debug cargo run --bin nefor

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

# Composite: install-nefor + install-starter. End-to-end first-time setup. `channel` (source|latest|nightly) forwards to install-nefor; `mode` (safe|force) forwards to install-starter.
install channel="source" mode="safe": (install-nefor channel) (install-starter mode)
    @echo
    @echo "Installed -> ~/.local/share/nefor/bin (plugins + da), ${PREFIX:-$HOME/.local}/bin/nefor (CLI entry)"
    @echo "Make sure your shell has:"
    @echo "  export PATH=\"${PREFIX:-$HOME/.local}/bin:\$PATH\""

# Install nefor for `channel`: source (cargo build) | latest (brew if available, else stable tarball) | nightly (rolling tarball). Plugins + da land in ~/.local/share/nefor/bin; only `nefor` goes on PATH (or wherever brew puts it).
install-nefor channel="source":
    #!/usr/bin/env bash
    set -eu
    PREFIX="${PREFIX:-$HOME/.local}"
    LIBEXEC_ROOT="$HOME/.local/share/nefor"
    LIBEXEC_BIN="$LIBEXEC_ROOT/bin"

    install_da_to_libexec() {
      mkdir -p "$LIBEXEC_BIN"
      if [ -x "$LIBEXEC_BIN/da" ]; then
        echo "  da (already installed) -> $LIBEXEC_BIN/da"
      else
        echo "Installing da -> $LIBEXEC_BIN/da..."
        cargo install --locked --root "$LIBEXEC_ROOT" dabin
        echo "  $LIBEXEC_BIN/da"
      fi
    }

    install_tarball() {
      # Args: $1 = release tag (e.g. v0.1.5 or "nightly")
      local tag="$1"
      local os arch target
      case "$(uname -s)" in
        Darwin) os=apple-darwin ;;
        Linux)  os=unknown-linux-gnu ;;
        *) echo "unsupported OS: $(uname -s)"; exit 1 ;;
      esac
      case "$(uname -m)" in
        arm64|aarch64) arch=aarch64 ;;
        x86_64|amd64)  arch=x86_64 ;;
        *) echo "unsupported arch: $(uname -m)"; exit 1 ;;
      esac
      target="${arch}-${os}"
      local url="https://github.com/amenocturne/nefor/releases/download/${tag}/nefor-${target}.tar.gz"
      local tmp="$(mktemp -d)"
      trap "rm -rf '$tmp'" EXIT
      echo "Downloading $url..."
      curl -fsSL "$url" -o "$tmp/nefor.tar.gz"
      tar -xzf "$tmp/nefor.tar.gz" -C "$tmp"
      mkdir -p "$PREFIX/bin" "$LIBEXEC_BIN"
      install -m 0755 "$tmp/nefor-${target}/bin/nefor" "$PREFIX/bin/nefor"
      echo "  $PREFIX/bin/nefor"
      for bin in "$tmp/nefor-${target}/share/nefor/plugins/"*; do
        install -m 0755 "$bin" "$LIBEXEC_BIN/$(basename "$bin")"
        echo "  $LIBEXEC_BIN/$(basename "$bin")"
      done
    }

    case "{{channel}}" in
      source)
        cargo build --workspace --release
        mkdir -p "$PREFIX/bin" "$LIBEXEC_BIN"
        cd "{{justfile_directory()}}"
        install -m 0755 "target/release/nefor" "$PREFIX/bin/nefor"
        echo "  $PREFIX/bin/nefor"
        for p in "{{justfile_directory()}}"/plugins/*/; do
          name=$(basename "$p")
          bin="target/release/$name"
          [ -f "$bin" ] && install -m 0755 "$bin" "$LIBEXEC_BIN/$name" && echo "  $LIBEXEC_BIN/$name"
        done
        install_da_to_libexec
        ;;
      latest)
        # Prefer brew (handles upgrades + uninstalls), fall back to the
        # stable tarball when brew isn't on PATH. Both end up with the
        # same engine layout — brew installs to its own prefix and the
        # resolver finds plugins via exe_relative_share_plugins; the
        # tarball path uses the libexec layout the source channel uses.
        if command -v brew >/dev/null 2>&1; then
          echo "Installing nefor via brew (amenocturne/tap)..."
          brew install amenocturne/tap/nefor
          # `da` ships as a separate formula in the same tap.
          if ! command -v da >/dev/null 2>&1; then
            brew install amenocturne/tap/da
          fi
        else
          echo "brew not on PATH; falling back to stable tarball download."
          # Resolve the latest non-prerelease release tag from GitHub.
          tag=$(curl -fsSL "https://api.github.com/repos/amenocturne/nefor/releases/latest" \
                | grep -E '"tag_name"' | head -1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
          if [ -z "$tag" ]; then
            echo "Could not resolve latest release tag from GitHub API."
            exit 1
          fi
          install_tarball "$tag"
          install_da_to_libexec
        fi
        ;;
      nightly)
        # Always tarball — brew --HEAD would mean cargo-build-from-main
        # on the user's machine, defeating the point of "nightly =
        # pre-built fast install". The release workflow keeps a rolling
        # `nightly` tag whose assets are rebuilt on every main push.
        install_tarball nightly
        install_da_to_libexec
        ;;
      *)
        echo "unknown channel '{{channel}}'; expected source | latest | nightly"
        exit 1
        ;;
    esac

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
