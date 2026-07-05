.PHONY: setup run app install icon

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP := target/release/muxterm.app

setup:
	@command -v cargo >/dev/null || { echo "Rust toolchain not found — install it from https://rustup.rs"; exit 1; }
	@command -v tmux >/dev/null || brew install tmux
	cargo fetch

run:
	cargo run --release

# Assemble the .app bundle (ad-hoc signed; a bundle is just a directory).
app:
	cargo build --release
	rm -rf $(APP)
	mkdir -p $(APP)/Contents/MacOS $(APP)/Contents/Resources
	sed 's/@VERSION@/$(VERSION)/g' packaging/Info.plist > $(APP)/Contents/Info.plist
	cp target/release/muxterm $(APP)/Contents/MacOS/muxterm
	cp assets/muxterm.icns $(APP)/Contents/Resources/muxterm.icns
	codesign --force --sign - $(APP)
	@echo "built $(APP)"

# Ship the bundle to /Applications and keep the panes' mux CLI in sync
# (quit muxterm first if it's running; sessions survive, relaunch restores).
install: app
	rm -rf /Applications/muxterm.app
	ditto $(APP) /Applications/muxterm.app
	@if [ -d $(HOME)/.cargo/bin ]; then \
		cp target/release/mux $(HOME)/.cargo/bin/mux; \
		echo "installed /Applications/muxterm.app and ~/.cargo/bin/mux"; \
	else \
		echo "installed /Applications/muxterm.app (put target/release/mux on your PATH yourself)"; \
	fi

# Regenerate assets/muxterm.icns from packaging/icon.swift (the icns is
# checked in, so plain builds don't need the Xcode CLT).
icon:
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
