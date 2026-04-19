default:
    @just --list

run:
    cargo run --bin nefor

setup:
    cargo fetch

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

build:
    cargo build --workspace --release

clean:
    cargo clean
