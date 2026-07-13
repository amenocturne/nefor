default:
    @just --list

# Launch the nefor TUI against the in-repo starter and plugins (debug build).
run:
    RUST_LOG=debug NEFOR_DEV_DIR={{justfile_directory()}} NEFOR_CONFIG_DIR={{justfile_directory()}}/starter NEFOR_PLUGIN_DIR={{justfile_directory()}}/target/debug cargo run --bin nefor

# Fetch all workspace dependencies without compiling — warms the cache.
setup:
    cargo fetch

# Default fast validation for small, localized changes.
test: test-fast

# Formatting plus fast tests; use before committing ordinary scoped changes.
check: fmt-check test-fast

# Rust formatting check only.
fmt-check:
    cargo fmt --all --check

# Core confidence set for engine, starter Lua, and tool plumbing changes.
test-fast:
    cargo test -p nefor --lib
    cargo test -p nefor --test starter_tool_gate_test
    cargo test -p nefor --test read_only_tools_test
    cargo test -p tool-gate-plugin
    cargo test -p nefor-tui --test chat_test starter_tool_catalog_replay_freshness_and_atomic_replacement -- --exact --test-threads=1

# Starter Lua, session, workflow, role, and bundled tool integration tests.
test-starter:
    cargo test -p nefor --test starter_tool_gate_test
    cargo test -p nefor --test starter_sessions_test
    cargo test -p nefor --test starter_startup_test
    cargo test -p nefor --test starter_openai_provider_test
    cargo test -p nefor --test starter_agentic_workflow_test
    cargo test -p nefor --test starter_agentic_cli_test
    cargo test -p nefor --test starter_lead_workflow_test
    cargo test -p nefor --test starter_lead_role_test
    cargo test -p nefor --test starter_mag_kernel_test
    cargo test -p nefor --test starter_ncp_test
    cargo test -p nefor --test instruction_files_test
    cargo test -p nefor --test read_only_tools_test

# Provider/API translation tests; may need local socket binding permissions.
test-provider:
    cargo test -p openai-provider
    cargo test -p chatgpt-provider
    cargo test -p generic-provider
    cargo test -p nefor --test openai_provider_lib_test
    cargo test -p nefor --test starter_openai_provider_test

# TUI rendering, layout, input, scrolling, and widget unit tests.
test-tui:
    cargo test -p nefor-tui --lib

# Drive the real starter with the same pinned tui-driver locally and in CI.
test-tui-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    readonly tui_driver_rev="0919a8efb130f5e5784dab227a84550284e08ad4"
    tui_driver_dir="${TUI_DRIVER_DIR:-{{ justfile_directory() }}/tmp/tui-driver}"
    if [ ! -d "$tui_driver_dir/.git" ]; then
      git clone --no-checkout https://github.com/amenocturne/tui-driver.git "$tui_driver_dir"
    fi
    if ! git -C "$tui_driver_dir" cat-file -e "$tui_driver_rev^{commit}" 2>/dev/null; then
      git -C "$tui_driver_dir" fetch --depth 1 origin "$tui_driver_rev"
    fi
    git -C "$tui_driver_dir" checkout --detach "$tui_driver_rev"
    test "$(git -C "$tui_driver_dir" rev-parse HEAD)" = "$tui_driver_rev"
    mkdir -p "{{ justfile_directory() }}/tmp"
    data_dir="$(mktemp -d "{{ justfile_directory() }}/tmp/tui-driver-data.XXXXXX")"
    trap 'rm -rf "$data_dir"' EXIT
    rm -rf "{{ justfile_directory() }}/tmp/tui-driver-artifacts"
    cargo build --workspace --locked
    cargo run --quiet --locked --manifest-path "$tui_driver_dir/Cargo.toml" -- \
      run-script tests/tui/starter-mock-smoke.json \
      --repo-root "{{ justfile_directory() }}" \
      --artifacts-dir "{{ justfile_directory() }}/tmp/tui-driver-artifacts" \
      --env "NEFOR_DATA_DIR=$data_dir" \
      --env NEFOR_DEFAULT_PROVIDER=mock-plugin \
      --env NEFOR_DEFAULT_MODEL=mock-model \
      --env NEFOR_ENABLE_CHATGPT=0 \
      --env NEFOR_ENABLE_OLLAMA=0 \
      --env NEFOR_USE_REPO_PLUGIN_BINS=0 \
      --env NEFOR_TEST_FAST_MOCK=1

# All local TUI confidence: renderer/unit coverage plus the real starter smoke.
test-tui-all: test-tui test-tui-smoke

# TUI chat workflow, replay, autocomplete, and input integration tests.
test-tui-chat:
    cargo test -p nefor-tui --test chat_test -- --test-threads=1

# Full local suite for cross-cutting or release-level validation.
test-all:
    cargo test --workspace --exclude nefor-tui
    cargo test -p nefor-tui --lib
    cargo test -p nefor-tui --test chat_test -- --test-threads=1

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

# Print the workspace version — the single source of truth (Cargo.toml
# [workspace.package]). The pre-push hook derives the release tag from this, so
# bumping it here (and pushing) is all it takes to cut v<version>.
version:
    @cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name=="nefor") | .version'

# Composite: install-nefor + install-starter. End-to-end first-time setup. `channel` (source|latest|nightly) forwards to install-nefor; `mode` (safe|force) forwards to install-starter.
install channel="source" mode="safe": (install-nefor channel) (install-starter mode)
    @echo
    @echo "Installed -> ~/.local/share/nefor/bin (plugins), ${PREFIX:-$HOME/.local}/bin/nefor (CLI entry)"
    @echo "Make sure your shell has:"
    @echo "  export PATH=\"${PREFIX:-$HOME/.local}/bin:\$PATH\""

# Install nefor for `channel`: source (cargo build) | latest (brew if available, else stable tarball) | nightly (rolling tarball). Plugins land in ~/.local/share/nefor/bin; only `nefor` goes on PATH (or wherever brew puts it). Cleans up binaries left by older install layouts.
install-nefor channel="source":
    #!/usr/bin/env bash
    set -eu
    PREFIX="${PREFIX:-$HOME/.local}"
    LIBEXEC_ROOT="$HOME/.local/share/nefor"
    LIBEXEC_BIN="$LIBEXEC_ROOT/bin"

    # Pre-libexec installs put every plugin next to `nefor` in $PREFIX/bin.
    # A leftover nefor-tui there makes the engine's plugin-root resolver
    # pick $PREFIX/bin over $LIBEXEC_BIN, so it spawns stale plugins and
    # misses ones that never existed in the old layout (mag-plugin).
    remove_old_layout_bins() {
      local name
      for name in openai-provider tool-gate basic-tools git-worktree reasoner-graph \
                  nefor-tui mock-plugin generic-provider generic-tool \
                  nefor-combinators chatgpt-provider mag mag-plugin; do
        if [ -e "$PREFIX/bin/$name" ]; then
          rm -f "$PREFIX/bin/$name"
          echo "  removed stale $PREFIX/bin/$name (old install layout)"
        fi
      done
    }

    # $@ = the bins this install just wrote. Anything else in $LIBEXEC_BIN
    # is from an older nefor (renamed/removed plugins); `da` is managed by
    # install-starter and kept.
    prune_libexec_bin() {
      local keep=" $* da " f name
      for f in "$LIBEXEC_BIN"/*; do
        [ -e "$f" ] || continue
        name=$(basename "$f")
        case "$keep" in
          *" $name "*) ;;
          *) rm -f "$f"; echo "  removed stale $LIBEXEC_BIN/$name" ;;
        esac
      done
    }

    remove_old_layout_bins

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
      local installed=""
      for bin in "$tmp/nefor-${target}/share/nefor/plugins/"*; do
        install -m 0755 "$bin" "$LIBEXEC_BIN/$(basename "$bin")"
        echo "  $LIBEXEC_BIN/$(basename "$bin")"
        installed="$installed $(basename "$bin")"
      done
      prune_libexec_bin $installed
    }

    case "{{channel}}" in
      source)
        cargo build --workspace --release
        mkdir -p "$PREFIX/bin" "$LIBEXEC_BIN"
        cd "{{justfile_directory()}}"
        install -m 0755 "target/release/nefor" "$PREFIX/bin/nefor"
        echo "  $PREFIX/bin/nefor"
        # Every workspace [[bin]] except the CLI entry and dev-only tools is
        # a plugin. cargo metadata keeps bins whose name differs from their
        # crate directory (mag-plugin) covered, and a missing build artifact
        # fails the install instead of being silently skipped.
        plugins=$(cargo metadata --no-deps --format-version 1 \
          | jq -r '.packages[].targets[] | select(.kind[]=="bin") | .name' \
          | grep -vx -e nefor -e fake-engine)
        for name in $plugins; do
          install -m 0755 "target/release/$name" "$LIBEXEC_BIN/$name"
          echo "  $LIBEXEC_BIN/$name"
        done
        prune_libexec_bin $plugins
        ;;
      latest)
        if command -v brew >/dev/null 2>&1; then
          echo "Installing nefor via brew (amenocturne/tap)..."
          brew install amenocturne/tap/nefor
        else
          echo "brew not on PATH; falling back to stable tarball download."
          tag=$(curl -fsSL "https://api.github.com/repos/amenocturne/nefor/releases/latest" \
                | grep -E '"tag_name"' | head -1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
          if [ -z "$tag" ]; then
            echo "Could not resolve latest release tag from GitHub API."
            exit 1
          fi
          install_tarball "$tag"
        fi
        ;;
      nightly)
        install_tarball nightly
        ;;
      *)
        echo "unknown channel '{{channel}}'; expected source | latest | nightly"
        exit 1
        ;;
    esac

# Copy starter/ to ~/.config/nefor and install its external dependencies (da). Refuses if the dir exists; pass `force` to wipe and re-copy.
install-starter mode="safe":
    #!/usr/bin/env bash
    set -eu
    DEST=~/.config/nefor
    LIBEXEC_ROOT="$HOME/.local/share/nefor"
    LIBEXEC_BIN="$LIBEXEC_ROOT/bin"

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

    # da — bash-command classifier used by starter's tool-validator.
    mkdir -p "$LIBEXEC_BIN"
    if [ -x "$LIBEXEC_BIN/da" ]; then
      echo "  da (already installed) -> $LIBEXEC_BIN/da"
    elif command -v brew >/dev/null 2>&1; then
      echo "Installing da via brew (amenocturne/tap)..."
      brew install amenocturne/tap/da
    else
      echo "Installing da -> $LIBEXEC_BIN/da..."
      cargo install --locked --root "$LIBEXEC_ROOT" dabin
      echo "  $LIBEXEC_BIN/da"
    fi

# Remove the entire target/ directory. Next build is a full cold compile.
clean:
    cargo clean
