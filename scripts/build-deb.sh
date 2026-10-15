#!/usr/bin/env bash
# Builds the release binary and packages it as dist/reolink-desktop_<ver>_<arch>.deb
# using only dpkg-deb (no cargo-deb needed).
set -euo pipefail

cd "$(dirname "$0")/.."
NAME=reolink-desktop
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(dpkg --print-architecture)
TARGET_DIR=${CARGO_TARGET_DIR:-target}

if [ "${SKIP_BUILD:-0}" != 1 ]; then
    cargo build --release --locked
fi

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
ROOT="$STAGE/${NAME}_${VERSION}_${ARCH}"

install -Dm755 "$TARGET_DIR/release/$NAME"         "$ROOT/usr/bin/$NAME"
install -Dm644 packaging/$NAME.desktop             "$ROOT/usr/share/applications/$NAME.desktop"
install -Dm644 packaging/$NAME.svg                 "$ROOT/usr/share/icons/hicolor/scalable/apps/$NAME.svg"
install -Dm644 README.md                           "$ROOT/usr/share/doc/$NAME/README.md"
EXT_UUID=reolink-desktop@reolink-desktop.github.io
for f in metadata.json extension.js stylesheet.css; do
    install -Dm644 "gnome-extension/$EXT_UUID/$f" "$ROOT/usr/share/gnome-shell/extensions/$EXT_UUID/$f"
done
cat > "$ROOT/usr/share/doc/$NAME/copyright" <<COPY
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: $NAME
License: MIT
COPY
chmod 644 "$ROOT/usr/share/doc/$NAME/copyright"

mkdir -p "$ROOT/DEBIAN"
SIZE=$(du -sk --exclude=DEBIAN "$ROOT" | cut -f1)
sed -e "s/@VERSION@/$VERSION/" -e "s/@ARCH@/$ARCH/" -e "s/@SIZE@/$SIZE/" \
    packaging/debian/control > "$ROOT/DEBIAN/control"
install -m755 packaging/debian/postinst "$ROOT/DEBIAN/postinst"
install -m755 packaging/debian/postrm   "$ROOT/DEBIAN/postrm"

mkdir -p dist
dpkg-deb --root-owner-group --build "$ROOT" "dist/${NAME}_${VERSION}_${ARCH}.deb"
echo "Built dist/${NAME}_${VERSION}_${ARCH}.deb"
