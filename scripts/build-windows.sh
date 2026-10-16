#!/usr/bin/env bash
# Builds the Windows packages:
#   dist/Reolink_Desktop-<ver>-x86_64-setup.exe     installer (Inno Setup)
#   dist/Reolink_Desktop-<ver>-x86_64-portable.zip  unzip and run bin\reolink-desktop.exe
#
# Runs in an MSYS2 UCRT64 shell on Windows; .github/workflows/windows.yml does
# exactly that on every tag. The MSYS2 packages it needs are listed in that
# workflow. Without Inno Setup (iscc) only the zip is built.
set -euo pipefail

cd "$(dirname "$0")/.."
if [ "${MSYSTEM:-}" != UCRT64 ]; then
    echo "Run this in an MSYS2 UCRT64 shell on Windows, or let GitHub Actions build it" >&2
    echo "(.github/workflows/windows.yml). On Linux, 'make check-windows' type-checks the code." >&2
    exit 1
fi

NAME=reolink-desktop
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
PREFIX=${MINGW_PREFIX:-/ucrt64}
TARGET_DIR=${CARGO_TARGET_DIR:-target}
cargo build --release --locked --target-dir "$TARGET_DIR"

STAGE="$TARGET_DIR/windows/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin"

# ── App, and the GLib helpers it needs at runtime ───────────────────────────
# gdbus.exe starts the session bus that keeps the app single-instance;
# gdk-pixbuf-query-loaders.exe rebuilds the loader cache after installing.
install -m755 "$TARGET_DIR/release/$NAME.exe" "$STAGE/bin/"
install -m755 "$PREFIX/bin/gdbus.exe" "$PREFIX/bin/gdk-pixbuf-query-loaders.exe" "$STAGE/bin/"
install -Dm644 packaging/$NAME.svg "$STAGE/share/icons/hicolor/scalable/apps/$NAME.svg"
install -Dm644 README.md "$STAGE/README.md"
install -Dm644 LICENSE "$STAGE/LICENSE.txt"

# ── GStreamer: only the plugins the app's pipelines use ──────────────────────
# Live view and previews: RTSP → H.264/H.265 (Direct3D or libav) → GTK sink.
# Recordings: MP4 over HTTPS (souphttpsrc) with AAC audio.
GST_PLUGINS=(
    coreelements typefindfunctions playback app autodetect
    videoconvertscale videorate deinterlace opengl
    audioconvert audioresample volume wasapi directsound
    rtsp rtp rtpmanager udp tcp
    videoparsersbad audioparsers isomp4 jpeg soup libav d3d11 d3d12 mediafoundation
)
mkdir -p "$STAGE/lib/gstreamer-1.0"
for p in "${GST_PLUGINS[@]}"; do
    dll="$PREFIX/lib/gstreamer-1.0/libgst$p.dll"
    if [ -f "$dll" ]; then
        cp "$dll" "$STAGE/lib/gstreamer-1.0/"
    else
        echo "warning: GStreamer plugin '$p' not found ($dll)" >&2
    fi
done
install -Dm755 "$PREFIX/libexec/gstreamer-1.0/gst-plugin-scanner.exe" \
    "$STAGE/libexec/gstreamer-1.0/gst-plugin-scanner.exe"

# ── GIO modules (TLS for HTTPS recordings) and gdk-pixbuf loaders (SVG) ──────
mkdir -p "$STAGE/lib/gio/modules" "$STAGE/lib/gdk-pixbuf-2.0/2.10.0/loaders"
cp "$PREFIX"/lib/gio/modules/*.dll "$STAGE/lib/gio/modules/"
cp "$PREFIX"/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.dll "$STAGE/lib/gdk-pixbuf-2.0/2.10.0/loaders/"

# ── GSettings schemas, icons and fonts config GTK expects ───────────────────
mkdir -p "$STAGE/share/glib-2.0/schemas"
cp "$PREFIX"/share/glib-2.0/schemas/org.gtk.gtk4.*.gschema.xml "$STAGE/share/glib-2.0/schemas/"
glib-compile-schemas "$STAGE/share/glib-2.0/schemas"
mkdir -p "$STAGE/share/icons/Adwaita"
cp -r "$PREFIX/share/icons/Adwaita/index.theme" "$PREFIX/share/icons/Adwaita/symbolic" "$STAGE/share/icons/Adwaita/"
cp "$PREFIX/share/icons/hicolor/index.theme" "$STAGE/share/icons/hicolor/"
if [ -d "$PREFIX/etc/fonts" ]; then
    mkdir -p "$STAGE/etc"
    cp -r "$PREFIX/etc/fonts" "$STAGE/etc/"
fi

# ── DLLs: everything the executables and modules load from the MSYS2 prefix ─
# libsoup is dlopen()ed by the soup plugin, so ntldd cannot see it.
cp "$PREFIX/bin/libsoup-3.0-0.dll" "$STAGE/bin/"
mapfile -t binaries < <(find "$STAGE" -name '*.exe' -o -name '*.dll')
prefix_win=$(cygpath -m "$PREFIX/bin" | tr '[:upper:]' '[:lower:]')
ntldd -R "${binaries[@]}" | tr '\\' '/' \
    | grep -io "[a-z]:/[^ ]*\.dll" | sort -fu \
    | while read -r dll; do
        case "$(echo "$dll" | tr '[:upper:]' '[:lower:]')" in
            "$prefix_win"/*) cp -n "$(cygpath -u "$dll")" "$STAGE/bin/" ;;
        esac
    done

# On Windows the cache stores loader paths relative to the install folder, so
# the zip works wherever it is unpacked (the installer rebuilds it anyway).
"$STAGE/bin/gdk-pixbuf-query-loaders.exe" --update-cache

# ── Packages ────────────────────────────────────────────────────────────────
mkdir -p dist
BASE="Reolink_Desktop-$VERSION-x86_64"
rm -f "dist/$BASE-portable.zip"
(cd "$STAGE" && zip -qr9 "$OLDPWD/dist/$BASE-portable.zip" .)
echo "Built dist/$BASE-portable.zip"

ISCC=$(command -v iscc || command -v ISCC.exe || true)
for dir in "/c/Program Files (x86)/Inno Setup 6" "/c/Program Files/Inno Setup 6"; do
    [ -z "$ISCC" ] && [ -x "$dir/ISCC.exe" ] && ISCC="$dir/ISCC.exe"
done
if [ -z "$ISCC" ]; then
    echo "warning: Inno Setup (ISCC.exe) not found; skipping the installer" >&2
    exit 0
fi
"$ISCC" -Q \
    "-DAppVersion=$VERSION" \
    "-DSourceDir=$(cygpath -w "$STAGE")" \
    "-DOutputDir=$(cygpath -w "$PWD/dist")" \
    "-DOutputName=$BASE-setup" \
    "$(cygpath -w packaging/windows/reolink-desktop.iss)"
echo "Built dist/$BASE-setup.exe"
