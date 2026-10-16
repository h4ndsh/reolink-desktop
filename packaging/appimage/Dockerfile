# Build environment for the AppImage. Ubuntu 24.04 is the oldest base with
# GTK >= 4.12 and libadwaita >= 1.5, and its glibc (2.39) sets the minimum the
# AppImage needs on the target system — so keep it old, not latest.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config curl ca-certificates file desktop-file-utils \
        libgtk-4-dev libadwaita-1-dev \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
        gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad gstreamer1.0-libav gstreamer1.0-pulseaudio \
        libsoup-3.0-0 glib-networking dconf-gsettings-backend \
        librsvg2-common adwaita-icon-theme \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --no-modify-path \
    && chmod -R a+rX /opt/rustup /opt/cargo

# linuxdeploy (+ its AppImage output plugin), run without FUSE inside the container.
ENV APPIMAGE_EXTRACT_AND_RUN=1
RUN for url in \
        https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage \
        https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-x86_64.AppImage; do \
        name=$(basename "$url" -x86_64.AppImage); \
        curl -fsSL -o "/usr/local/bin/$name" "$url" && chmod +x "/usr/local/bin/$name"; \
    done
