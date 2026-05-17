# Path to the upstream nefor checkout. Only needed for `source` channel
# and `dev` recipe. Defaults to a sibling clone (../nefor relative to
# this repo). Override by exporting NEFOR_UPSTREAM in your shell.
NEFOR_UPSTREAM := env_var_or_default("NEFOR_UPSTREAM", justfile_directory() / ".." / "nefor")
NEFOR_BIN      := NEFOR_UPSTREAM / "target" / "release" / "nefor"
NEFOR_PLUGINS  := NEFOR_UPSTREAM / "target" / "release"

default:
    @just --list

# Composite: install-nefor + install-starter. Defaults to `latest` (brew if available, else GitHub release tarball). Pass `source` to build from NEFOR_UPSTREAM checkout.
install channel="latest" mode="safe": (install-nefor channel) (install-starter mode)
    @echo
    @echo "Run \`nefor\` from anywhere."

# Install the nefor binary + plugins. Channels: latest (brew → tarball fallback) | source (cargo build from NEFOR_UPSTREAM) | nightly (rolling tarball).
install-nefor channel="latest":
    #!/usr/bin/env bash
    set -eu
    case "{{channel}}" in
      source)
        cd "{{NEFOR_UPSTREAM}}" && just install-nefor source
        ;;
      latest|nightly)
        # Delegate to upstream's install-nefor if checkout exists, otherwise
        # run the brew/tarball logic inline.
        if [ -f "{{NEFOR_UPSTREAM}}/justfile" ]; then
          cd "{{NEFOR_UPSTREAM}}" && just install-nefor "{{channel}}"
        else
          PREFIX="${PREFIX:-$HOME/.local}"
          LIBEXEC_BIN="$HOME/.local/share/nefor/bin"
          if [ "{{channel}}" = "latest" ] && command -v brew >/dev/null 2>&1; then
            echo "Installing nefor via brew (amenocturne/tap)..."
            brew tap amenocturne/tap 2>/dev/null || true
            brew install amenocturne/tap/nefor 2>/dev/null || brew upgrade amenocturne/tap/nefor
            if ! command -v da >/dev/null 2>&1; then
              brew install amenocturne/tap/da 2>/dev/null || true
            fi
          else
            tag="{{channel}}"
            if [ "$tag" = "latest" ]; then
              tag=$(curl -fsSL "https://api.github.com/repos/amenocturne/nefor/releases/latest" \
                    | grep -E '"tag_name"' | head -1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
              [ -n "$tag" ] || { echo "Could not resolve latest release tag."; exit 1; }
            fi
            os=unknown-linux-gnu; [ "$(uname -s)" = "Darwin" ] && os=apple-darwin
            arch=x86_64; case "$(uname -m)" in arm64|aarch64) arch=aarch64 ;; esac
            target="${arch}-${os}"
            url="https://github.com/amenocturne/nefor/releases/download/${tag}/nefor-${target}.tar.gz"
            tmp="$(mktemp -d)"; trap "rm -rf '$tmp'" EXIT
            echo "Downloading $url..."
            curl -fsSL "$url" -o "$tmp/nefor.tar.gz"
            tar -xzf "$tmp/nefor.tar.gz" -C "$tmp"
            mkdir -p "$PREFIX/bin" "$LIBEXEC_BIN"
            install -m 0755 "$tmp/nefor-${target}/bin/nefor" "$PREFIX/bin/nefor"
            for bin in "$tmp/nefor-${target}/share/nefor/plugins/"*; do
              install -m 0755 "$bin" "$LIBEXEC_BIN/$(basename "$bin")"
            done
            echo "  nefor + plugins installed to $PREFIX/bin and $LIBEXEC_BIN"
            # Install da if missing
            if ! command -v da >/dev/null 2>&1; then
              mkdir -p "$LIBEXEC_BIN"
              if command -v cargo >/dev/null 2>&1; then
                cargo install --locked --root "$HOME/.local/share/nefor" dabin
              else
                echo "  warning: da not found and cargo not available to install it"
              fi
            fi
          fi
        fi
        ;;
      *)
        echo "unknown channel '{{channel}}'; expected latest | source | nightly"
        exit 1
        ;;
    esac

# Copy this checkout's starter/ to ~/.config/nefor. Refuses if the dir exists; pass `force` to wipe and re-copy.
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

# Run the installed nefor against the team starter via PATH lookup.
run:
    RUST_LOG=info nefor --config $PWD/starter

# In-tree dev iteration: rebuild upstream at NEFOR_UPSTREAM, run team starter against that target/release/nefor. Bypasses the bootstrap clone via NEFOR_DEV_DIR. Doesn't touch the installed binary.
dev:
    #!/usr/bin/env bash
    set -eu
    upstream=$(cd "{{NEFOR_UPSTREAM}}" && pwd -P)
    (cd "$upstream" && cargo build --workspace --release)
    exec env \
      RUST_LOG=info \
      NEFOR_DEV_DIR="$upstream" \
      NEFOR_PLUGIN_DIR="$upstream/target/release" \
      "$upstream/target/release/nefor" --config "{{justfile_directory()}}/starter"

# Sandbox the github-clone bootstrap path against a fresh NEFOR_DATA_DIR. Reproduces a clean machine's first boot. Removes the sandbox first. NEFOR_CONFIG defaults to mock so no auth is required.
test-bootstrap NEFOR_CONFIG="mock":
    #!/usr/bin/env bash
    set -eu
    SANDBOX="${TMPDIR:-/tmp}/nefor-team-bootstrap-test"
    rm -rf "$SANDBOX"
    mkdir -p "$SANDBOX"
    echo "[test-bootstrap] sandbox: $SANDBOX"
    NEFOR_DATA_DIR="$SANDBOX" \
    NEFOR_CONFIG="{{NEFOR_CONFIG}}" \
    NEFOR_PLUGIN_DIR={{NEFOR_PLUGINS}} \
    RUST_LOG=info \
    {{NEFOR_BIN}} --config $PWD/starter

# End-user install simulation. Wipes ~/.config/nefor and ~/.local/share/nefor for a true clean state, runs the full install, then launches `nefor` via PATH.
test-fresh-install:
    rm -rf ~/.config/nefor ~/.local/share/nefor
    just install-nefor
    just install-starter force
    nefor

# Tail the live engine log next to init.lua. Useful when the TUI hides errors.
tail-logs:
    tail -f $PWD/starter/nefor.log
