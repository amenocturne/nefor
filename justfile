set shell := ["bash", "-cu"]

app := justfile_directory() / "i-swear-it-is-not-gui"

# List available commands
default:
    @just --list

# Install npm + cargo dependencies
setup:
    cd {{app}} && npm install
    cd {{app}}/src-tauri && cargo check

# Run in dev mode (hot reload)
run:
    cd {{app}} && npm run tauri dev

# Build release (.app only)
build:
    cd {{app}} && npm run tauri build -- --bundles app

# Build release with installer (.app + .dmg)
install:
    cd {{app}} && npm run tauri build

# Build debug
build-debug:
    cd {{app}} && npm run tauri build -- --debug --bundles app

# Type check frontend
check:
    cd {{app}} && npx svelte-check

# Check rust compiles
check-rust:
    cd {{app}}/src-tauri && cargo check

# Clean build artifacts
clean:
    cd {{app}} && rm -rf dist
    cd {{app}}/src-tauri && cargo clean

# Build and open the .app
open: build
    open {{app}}/src-tauri/target/release/bundle/macos/Nefor.app
