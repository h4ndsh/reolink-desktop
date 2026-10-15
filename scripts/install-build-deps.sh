#!/usr/bin/env bash
# Installs everything needed to build reolink-desktop on Debian/Ubuntu.
set -euo pipefail

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
    build-essential pkg-config curl ca-certificates dpkg-dev \
    libgtk-4-dev libadwaita-1-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-libav

if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    echo "Rust installed in ~/.cargo (the Makefile finds it automatically)."
fi
