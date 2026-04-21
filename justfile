default:
    @just --list

# Launch the nefor TUI (debug build; reads ~/.config/nefor/init.lua by default).
run:
    cargo run --bin nefor

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

# Remove the entire target/ directory. Next build is a full cold compile.
clean:
    cargo clean
