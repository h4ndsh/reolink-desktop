#!/usr/bin/env bash
# Builds dist/Reolink_Desktop-<ver>-x86_64.AppImage, which runs on other Linux
# distributions (Fedora, Arch, openSUSE…) with glibc 2.39 or newer.
#
# The build runs in an Ubuntu 24.04 container (packaging/appimage/Dockerfile)
# so the bundled libraries are old enough for most current distributions.
# Set NO_DOCKER=1 to build on the host instead (the result then needs a glibc
# at least as new as the host's).
set -euo pipefail

cd "$(dirname "$0")/.."
NAME=reolink-desktop
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
IMAGE=reolink-desktop-appimage-build

if [ "${NO_DOCKER:-0}" != 1 ] && [ "${IN_CONTAINER:-0}" != 1 ]; then
    docker build -t "$IMAGE" packaging/appimage
    exec docker run --rm \
        --user "$(id -u):$(id -g)" \
        -e IN_CONTAINER=1 -e HOME=/tmp \
        -v "$PWD:/src" -w /src \
        "$IMAGE" scripts/build-appimage.sh
fi

# Separate target dir: the host's build links against a newer glibc.
TARGET_DIR=${CARGO_TARGET_DIR:-target/appimage}
export CARGO_HOME="$TARGET_DIR/cargo-home"
cargo build --release --locked --target-dir "$TARGET_DIR"

MULTIARCH=$(gcc -dumpmachine | sed 's/-pc-/-/')   # x86_64-linux-gnu
SYSLIB=/usr/lib/$MULTIARCH
APPDIR="$TARGET_DIR/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR"
APPDIR=$(realpath "$APPDIR")
LIB="$APPDIR/usr/lib"

# ── App files ────────────────────────────────────────────────────────────────
install -Dm755 "$TARGET_DIR/release/$NAME" "$APPDIR/usr/bin/$NAME"
EXT_UUID=reolink-desktop@reolink-desktop.github.io
for f in metadata.json extension.js stylesheet.css; do
    install -Dm644 "gnome-extension/$EXT_UUID/$f" "$APPDIR/usr/share/gnome-shell/extensions/$EXT_UUID/$f"
done

# ── GStreamer: only the plugins the app's pipelines use ──────────────────────
# Live view and previews: RTSP → H.264/H.265 → GL/GTK sink, JPEG for the top bar.
# Recordings: MP4 over HTTPS (souphttpsrc) with AAC audio.
GST_PLUGINS=(
    coreelements typefindfunctions playback app autodetect
    videoconvertscale videorate deinterlace opengl
    audioconvert audioresample volume pulseaudio alsa
    rtsp rtp rtpmanager udp tcp
    videoparsersbad audioparsers isomp4 jpeg soup libav va
)
mkdir -p "$LIB/gstreamer-1.0"
for p in "${GST_PLUGINS[@]}"; do
    so="$SYSLIB/gstreamer-1.0/libgst$p.so"
    if [ -f "$so" ]; then
        cp "$so" "$LIB/gstreamer-1.0/"
    else
        echo "warning: GStreamer plugin '$p' not found ($so)" >&2
    fi
done
install -Dm755 "$SYSLIB/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner" \
    "$LIB/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner"

# ── GIO modules: TLS for HTTPS recordings, dconf so settings persist ────────
mkdir -p "$LIB/gio/modules"
cp "$SYSLIB/gio/modules/libgiognutls.so" "$SYSLIB/gio/modules/libdconfsettings.so" "$LIB/gio/modules/"

# ── gdk-pixbuf loaders (SVG icons) ──────────────────────────────────────────
mkdir -p "$LIB/gdk-pixbuf-2.0/loaders"
cp "$SYSLIB"/gdk-pixbuf-2.0/2.10.0/loaders/*.so "$LIB/gdk-pixbuf-2.0/loaders/"
"$SYSLIB/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders" "$LIB"/gdk-pixbuf-2.0/loaders/*.so \
    | sed "s|$APPDIR|@APPDIR@|g" > "$LIB/gdk-pixbuf-2.0/loaders.cache.in"

# ── GSettings schemas and icons GTK expects from the system ─────────────────
mkdir -p "$APPDIR/usr/share/glib-2.0/schemas"
cp /usr/share/glib-2.0/schemas/org.gtk.gtk4.*.gschema.xml "$APPDIR/usr/share/glib-2.0/schemas/"
glib-compile-schemas "$APPDIR/usr/share/glib-2.0/schemas"
mkdir -p "$APPDIR/usr/share/icons/Adwaita" "$APPDIR/usr/share/icons/hicolor"
cp -r /usr/share/icons/Adwaita/index.theme /usr/share/icons/Adwaita/symbolic "$APPDIR/usr/share/icons/Adwaita/"
cp /usr/share/icons/hicolor/index.theme "$APPDIR/usr/share/icons/hicolor/"

# ── Bundle libraries and pack ───────────────────────────────────────────────
# libsoup is dlopen()ed by the soup plugin, so linuxdeploy cannot see it.
# libva is left to the host: its drivers must match the host's libva.
mkdir -p dist
export ARCH=x86_64 LINUXDEPLOY_OUTPUT_VERSION="$VERSION"
export LDAI_OUTPUT="dist/Reolink_Desktop-$VERSION-x86_64.AppImage"
linuxdeploy --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/$NAME" \
    --desktop-file packaging/$NAME.desktop \
    --icon-file packaging/$NAME.svg \
    --custom-apprun packaging/appimage/AppRun \
    --library "$SYSLIB/libsoup-3.0.so.0" \
    --deploy-deps-only "$LIB/gstreamer-1.0" \
    --deploy-deps-only "$LIB/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner" \
    --deploy-deps-only "$LIB/gio/modules" \
    --deploy-deps-only "$LIB/gdk-pixbuf-2.0/loaders" \
    --exclude-library 'libva.so*' --exclude-library 'libva-*' \
    --output appimage
echo "Built $LDAI_OUTPUT"
