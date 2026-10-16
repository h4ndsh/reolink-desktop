#!/usr/bin/env bash
# Type-checks the Windows build from Linux (`cargo check --target
# x86_64-pc-windows-gnu`) in the AppImage build container plus MinGW. Catches
# compile errors in the Windows-only code; building the real .exe needs MSYS2
# (scripts/build-windows.sh, run by .github/workflows/windows.yml).
set -euo pipefail

cd "$(dirname "$0")/.."
IMAGE=reolink-desktop-windows-check

docker build -q -t reolink-desktop-appimage-build packaging/appimage >/dev/null
docker build -q -t "$IMAGE" - >/dev/null <<'DOCKERFILE'
FROM reolink-desktop-appimage-build
RUN apt-get update && apt-get install -y --no-install-recommends \
        gcc-mingw-w64-x86-64 binutils-mingw-w64-x86-64 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-pc-windows-gnu \
    && chmod -R a+rX /opt/rustup /opt/cargo
DOCKERFILE

# Linking never happens, so the build scripts may use the Linux .pc files;
# gtk4-win32 has none on Linux and gets a placeholder.
exec docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp -e CARGO_HOME=/src/target/windows-check/cargo-home \
    -e PKG_CONFIG_ALLOW_CROSS=1 \
    -e SYSTEM_DEPS_GTK4_WIN32_NO_PKG_CONFIG=1 -e SYSTEM_DEPS_GTK4_WIN32_LIB=gtk-4 \
    -v "$PWD:/src" -w /src \
    "$IMAGE" cargo check --locked --target x86_64-pc-windows-gnu --target-dir target/windows-check
