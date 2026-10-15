# Reolink Desktop — build & packaging
#   make deps      install build dependencies (apt + rustup, needs sudo)
#   make           build release binary and the .deb (same as `make deb`)
#   make install   install the .deb with apt
# Finds rustup's cargo even if ~/.cargo/env was not sourced in this shell.
CARGO      ?= $(or $(shell command -v cargo 2>/dev/null),$(wildcard $(HOME)/.cargo/bin/cargo))
TARGET_DIR ?= $(or $(CARGO_TARGET_DIR),target)
VERSION    := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH       := $(shell dpkg --print-architecture)
DEB        := dist/reolink-desktop_$(VERSION)_$(ARCH).deb

.PHONY: all build test run deb install uninstall reset-config clean deps check-cargo extension-dev

all: deb

check-cargo:
	@test -n "$(CARGO)" || { echo "cargo not found. Run 'make deps' first (installs Rust via rustup and the system libraries)."; exit 1; }

build: check-cargo
	$(CARGO) build --release --locked

test: check-cargo
	$(CARGO) test --locked

run: check-cargo
	RUST_LOG=$${RUST_LOG:-reolink_desktop=debug} $(CARGO) run --release --locked

deb: build
	SKIP_BUILD=1 CARGO_TARGET_DIR=$(TARGET_DIR) scripts/build-deb.sh

install: deb
	sudo apt install -y ./$(DEB)

uninstall:
	sudo apt remove -y reolink-desktop

# Development: use the extension straight from this checkout (log out/in once
# on Wayland, then `gnome-extensions enable reolink-desktop@reolink-desktop.github.io`).
EXT_UUID := reolink-desktop@reolink-desktop.github.io
extension-dev:
	mkdir -p $${XDG_DATA_HOME:-$$HOME/.local/share}/gnome-shell/extensions
	ln -sfn $(CURDIR)/gnome-extension/$(EXT_UUID) $${XDG_DATA_HOME:-$$HOME/.local/share}/gnome-shell/extensions/$(EXT_UUID)

# Forget the saved camera settings (the first-run setup appears again).
reset-config:
	rm -f $${XDG_CONFIG_HOME:-$$HOME/.config}/reolink-desktop/config.toml

clean: check-cargo
	$(CARGO) clean
	rm -rf dist

deps:
	scripts/install-build-deps.sh
