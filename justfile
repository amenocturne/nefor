default:
    @just --list

# Launch the nefor TUI against this exact checkout (debug build).
run:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUST_LOG=debug
    export NEFOR_DEV_DIR="{{justfile_directory()}}"
    export NEFOR_CONFIG_DIR="{{justfile_directory()}}/examples/nefor-agent"
    cargo run --bin nefor

# Fetch all workspace dependencies without compiling — warms the cache.
setup:
    cargo fetch

# Default fast validation for small, localized changes.
test: test-fast

# Formatting plus fast tests; use before committing ordinary scoped changes.
check: fmt-check test-docs test-fast

# Validate local Markdown links and anchors without network access.
test-docs:
    bun tools/check-markdown-links.ts

# Rust formatting check only.
fmt-check:
    cargo fmt --all --check

# Core confidence set for engine, example Lua, and tool plumbing changes.
test-fast:
    cargo test -p nefor --lib
    cargo test -p nefor --test conversation_manager_test
    cargo test -p nefor --test starter_tool_gate_test
    cargo test -p nefor --test read_only_tools_test
    cargo test -p tool-gate-plugin
    cargo test -p nefor-tui --test chat_test starter_tool_catalog_replay_freshness_and_atomic_replacement -- --exact --test-threads=1

# Nefor package-manager checkout, locking, local-source, and build behavior.
test-pm:
    cargo test -p nefor --test nefor_pm_test

# Starter Lua, session, workflow, role, and bundled tool integration tests.
test-example:
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

# Drive the real agent example through the environment-managed tui-driver.
test-tui-scenarios:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v tui-driver >/dev/null 2>&1; then
      echo "tui-driver is not installed; use this environment's managed installation" >&2
      exit 127
    fi
    mkdir -p "{{ justfile_directory() }}/tmp"
    suite_dir="$(mktemp -d "{{ justfile_directory() }}/tmp/tui-driver-suite.XXXXXX")"
    trap 'rm -rf "$suite_dir"' EXIT
    rm -rf "{{ justfile_directory() }}/tmp/tui-driver-artifacts"
    cargo build --workspace --locked
    scenarios=(
      tests/tui/starter-initial-prompt.json
      tests/tui/starter-mock-smoke.json
      tests/tui/starter-mock-multi-turn.json
      tests/tui/starter-permission-commands.json
      tests/tui/starter-sidebar-overflow.json
      tests/tui/starter-interrupt-recovery.json
    )
    for scenario in "${scenarios[@]}"; do
      scenario_name="$(basename "$scenario" .json)"
      data_dir="$suite_dir/$scenario_name"
      mkdir -p "$data_dir"
      fast_mock=1
      if [ "$scenario_name" = "starter-interrupt-recovery" ]; then fast_mock=0; fi
      tui-driver run-script "$scenario" \
        --repo-root "{{ justfile_directory() }}" \
        --artifacts-dir "{{ justfile_directory() }}/tmp/tui-driver-artifacts" \
        --env "NEFOR_DEV_DIR={{ justfile_directory() }}" \
        --env "NEFOR_CONFIG_DIR={{ justfile_directory() }}/examples/nefor-agent" \
        --env "NEFOR_DATA_DIR=$data_dir" \
        --env NEFOR_DEFAULT_PROVIDER=mock-plugin \
        --env NEFOR_DEFAULT_MODEL=mock-model \
        --env NEFOR_ENABLE_CHATGPT=0 \
        --env NEFOR_ENABLE_OLLAMA=0 \
        --env NEFOR_USE_REPO_PLUGIN_BINS=0 \
        --env "NEFOR_TEST_FAST_MOCK=$fast_mock" \
        --env "NEFOR_TEST_SIDEBAR_OVERFLOW=$([ "$scenario_name" = "starter-sidebar-overflow" ] && echo 1 || echo 0)"
    done

# Backwards-compatible name for the full deterministic TUI scenario pack.
test-tui-smoke: test-tui-scenarios

# All local TUI confidence: renderer/unit coverage plus the real agent example scenarios.
test-tui-all: test-tui test-tui-scenarios

# TUI chat workflow, replay, autocomplete, and input integration tests.
test-tui-chat:
    cargo test -p nefor-tui --test chat_test -- --test-threads=1

# Full local suite for cross-cutting or release-level validation. The first Cargo phase is watchdog-protected; override its 2h deadline with the timeout argument.
test-all timeout="7200":
    cargo run --quiet -p nefor-cargo-test-harness -- "{{timeout}}"
    cargo test -p nefor-tui --lib
    cargo test -p nefor-tui --test chat_test -- --test-threads=1

# Validate that a built workspace becomes a complete installable distribution.
test-release-bundle:
    cargo build --workspace --locked
    tools/test-release-bundle.sh "{{ justfile_directory() }}/target/debug"

# Deterministic process-level checks kept separate from the fast default suite.
test-integration: test-release-bundle test-tui-scenarios

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

# Build this checkout and copy a channel-neutral runtime without pruning sibling distributions.
# `expected_commit` lets an installer prove it is building the pm-resolved checkout.
install-source expected_commit command engine_dir mag_dir plugin_root build_target="target":
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    if [ -z "{{expected_commit}}" ]; then echo "expected_commit is required" >&2; exit 1; fi
    if [ "{{engine_dir}}" = "{{mag_dir}}" ] || [ "{{engine_dir}}" = "{{plugin_root}}" ] || [ "{{mag_dir}}" = "{{plugin_root}}" ]; then
      echo "engine_dir, mag_dir, and plugin_root must be distinct" >&2; exit 1
    fi
    if [ -n "{{expected_commit}}" ]; then
      actual_commit="$(git rev-parse --verify 'HEAD^{commit}')"
      expected="$(git rev-parse --verify '{{expected_commit}}^{commit}')"
      if [ "$actual_commit" != "$expected" ]; then
        echo "refusing runtime build: checkout HEAD $actual_commit != expected $expected" >&2
        exit 1
      fi
    fi
    cargo build --workspace --release --target-dir "{{build_target}}"
    release_dir="{{build_target}}/release"
    mkdir -p "{{engine_dir}}" "{{mag_dir}}" "{{plugin_root}}"
    install -m 0755 "$release_dir/nefor" "{{engine_dir}}/{{command}}"
    install -m 0755 "$release_dir/mag" "{{mag_dir}}/mag"
    plugins="$(tools/plugin-binaries.sh)"
    for name in $plugins; do
      install -m 0755 "$release_dir/$name" "{{plugin_root}}/$name"
    done
    echo "  {{engine_dir}}/{{command}}"
    echo "  {{mag_dir}}/mag"
    echo "  plugins -> {{plugin_root}}"

# Measure an explicit ref in a clean detached checkout; always runs cold then immediate warm and writes JSONL.
bench-build ref="HEAD" component="workspace" profile="dev" output="tmp/build-bench/results.jsonl":
    tools/bench-build.sh "{{ref}}" "{{component}}" "{{profile}}" "{{output}}"

# Focused version-stamp invalidation coverage for normal and linked Git worktrees.
test-build-version:
    tools/test-build-version.sh

# Opt-in optimized MAG compiler benchmark; writes a metadata-rich JSON report to stdout.
bench-mag *args:
    cargo bench -p nefor-mag --bench mag_compile -- {{args}}

# Browse aggregate byte anatomy for sessions in the resolved Nefor data directory.
inspect-sessions host="127.0.0.1" port="3939":
    NEFOR_SESSION_INSPECTOR_HOST={{host}} NEFOR_SESSION_INSPECTOR_PORT={{port}} bun tools/session-inspector/server.ts

# Focused backend and persistence-boundary tests for the session inspector.
test-session-inspector:
    bun test tools/session-inspector/server.test.ts

# Print the workspace version — the single source of truth (Cargo.toml
# [workspace.package]). The pre-push hook derives the release tag from this, so
# bumping it here (and pushing) is all it takes to cut v<version>.
version:
    @cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name=="nefor") | .version'

# Composite: install-nefor + install-example. End-to-end first-time setup. `channel` (source|latest|nightly) forwards to install-nefor; `mode` (safe|force) forwards to install-example.
install channel="source" mode="safe": (install-nefor channel) (install-example mode)
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
    # install-example and kept.
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
      install -m 0755 "$tmp/nefor-${target}/bin/mag" "$PREFIX/bin/mag"
      echo "  $PREFIX/bin/nefor"
      echo "  $PREFIX/bin/mag"
      rm -rf "$LIBEXEC_ROOT/runtime"
      cp -R "$tmp/nefor-${target}/share/nefor/runtime" "$LIBEXEC_ROOT/runtime"
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
        install -m 0755 "target/release/mag" "$PREFIX/bin/mag"
        echo "  $PREFIX/bin/nefor"
        echo "  $PREFIX/bin/mag"
        # Derive runtime plugins from packages physically under plugins/.
        # Target names, not crate directories, are authoritative: the MAG
        # runtime directory is `mag`, while its binary is `mag-plugin`.
        rm -rf "$LIBEXEC_ROOT/runtime"
        mkdir -p "$LIBEXEC_ROOT/runtime"
        cp -R lua plugins examples "$LIBEXEC_ROOT/runtime/"
        plugins=$(tools/plugin-binaries.sh)
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

# Copy examples/nefor-agent/ to ~/.config/nefor and install its external dependencies (da). Refuses if the dir exists; pass `force` to wipe and re-copy.
install-example mode="safe":
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
        echo "  (To wipe and re-copy: just install-example force)"
        exit 0
      fi
    fi
    mkdir -p "$DEST"
    cp -R "{{justfile_directory()}}/examples/nefor-agent/." "$DEST/"
    echo "  $DEST (copied from {{justfile_directory()}}/examples/nefor-agent)"

    # da — bash-command classifier used by example's tool-validator.
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
