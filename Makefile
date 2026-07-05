.PHONY: setup run

setup:
	@command -v cargo >/dev/null || { echo "Rust toolchain not found — install it from https://rustup.rs"; exit 1; }
	@command -v tmux >/dev/null || brew install tmux
	cargo fetch

run:
	cargo run --release
