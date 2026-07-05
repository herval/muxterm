.PHONY: help setup run app install icon
.DEFAULT_GOAL := help

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP := target/release/muxterm.app

help: ## list available commands (the default)
	@echo "muxterm $(VERSION) - make <command>"
	@echo
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | \
		awk -F':.*## ' '{printf "  %-9s %s\n", $$1, $$2}'

setup: ## check the Rust toolchain, install tmux, fetch dependencies
	@command -v cargo >/dev/null || { echo "Rust toolchain not found — install it from https://rustup.rs"; exit 1; }
	@command -v tmux >/dev/null || brew install tmux
	cargo fetch

run: ## build and run the app (release)
	cargo run --release

# Ad-hoc signed; a bundle is just a directory.
app: ## assemble target/release/muxterm.app
	cargo build --release
	rm -rf $(APP)
	mkdir -p $(APP)/Contents/MacOS $(APP)/Contents/Resources
	sed 's/@VERSION@/$(VERSION)/g' packaging/Info.plist > $(APP)/Contents/Info.plist
	cp target/release/muxterm $(APP)/Contents/MacOS/muxterm
	cp assets/muxterm.icns $(APP)/Contents/Resources/muxterm.icns
	codesign --force --sign - $(APP)
	@echo "built $(APP)"

# Quit muxterm first if it's running; sessions survive, relaunch restores.
install: app ## ship the .app to /Applications and refresh ~/.cargo/bin/mux
	rm -rf /Applications/muxterm.app
	ditto $(APP) /Applications/muxterm.app
	@if [ -d $(HOME)/.cargo/bin ]; then \
		cp target/release/mux $(HOME)/.cargo/bin/mux; \
		echo "installed /Applications/muxterm.app and ~/.cargo/bin/mux"; \
	else \
		echo "installed /Applications/muxterm.app (put target/release/mux on your PATH yourself)"; \
	fi

# The icns is checked in, so plain builds don't need the Xcode CLT.
icon: ## regenerate assets/muxterm.icns from packaging/icon.swift
	rm -rf target/muxterm.iconset && mkdir -p target/muxterm.iconset assets
	xcrun swift packaging/icon.swift target/muxterm.iconset/icon_512x512@2x.png
	for s in 16 32 128 256 512; do \
		sips -z $$s $$s target/muxterm.iconset/icon_512x512@2x.png \
			--out target/muxterm.iconset/icon_$${s}x$${s}.png >/dev/null; \
		d=$$((s * 2)); \
		sips -z $$d $$d target/muxterm.iconset/icon_512x512@2x.png \
			--out target/muxterm.iconset/icon_$${s}x$${s}@2x.png >/dev/null; \
	done
	iconutil -c icns target/muxterm.iconset -o assets/muxterm.icns
	@echo "wrote assets/muxterm.icns"
