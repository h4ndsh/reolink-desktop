# Reolink Desktop — build & packaging
#   make deps      install build dependencies (apt + rustup, needs sudo)
#   make           build every package for this system (same as `make all`):
#                  the .deb and the AppImage on Linux, the Windows installer in MSYS2
#   make deb       build release binary and the .deb
#   make install   install the .deb with apt
#   make appimage  build a portable AppImage for other distributions (needs Docker)
#   make windows   build the Windows installer and zip (MSYS2 UCRT64 shell only)
#   make windows-ci       build them on GitHub Actions and download them to dist/ (needs gh)
#   make check-windows    type-check the Windows code from Linux (needs Docker)
# Finds rustup's cargo even if ~/.cargo/env was not sourced in this shell.
CARGO      ?= $(or $(shell command -v cargo 2>/dev/null),$(wildcard $(HOME)/.cargo/bin/cargo))
TARGET_DIR ?= $(or $(CARGO_TARGET_DIR),target)
VERSION    := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH       := $(shell dpkg --print-architecture 2>/dev/null)
DEB        := dist/reolink-desktop_$(VERSION)_$(ARCH).deb

.PHONY: all build test run deb appimage windows windows-ci check-windows install uninstall reset-config clean deps check-cargo extension-dev

ifdef MSYSTEM
all: windows
else
all: deb appimage
endif

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

# Built in an Ubuntu 24.04 container so it also runs on older distributions.
appimage:
	scripts/build-appimage.sh

windows:
	scripts/build-windows.sh

# Runs .github/workflows/windows.yml on the pushed current branch and waits.
windows-ci:
	@git diff --quiet HEAD && git rev-parse --abbrev-ref --symbolic-full-name @{u} >/dev/null 2>&1 \
		&& test -z "$$(git log @{u}..HEAD --oneline)" \
		|| { echo "Commit and push first: GitHub Actions builds what is on GitHub."; exit 1; }
	gh workflow run windows.yml --ref $$(git rev-parse --abbrev-ref HEAD)
	@sleep 5; run=$$(gh run list --workflow windows.yml --event workflow_dispatch --limit 1 --json databaseId --jq '.[0].databaseId'); \
		gh run watch --exit-status $$run && gh run download $$run --name reolink-desktop-windows --dir dist

check-windows:
	scripts/check-windows.sh

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
