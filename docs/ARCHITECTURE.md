# Architecture

Technical notes for contributors. For installation and everyday use, see the
[README](../README.md).

Reolink Desktop is a Rust application (GTK 4, libadwaita, GStreamer) with a small GNOME Shell
extension in JavaScript.

## Overview

```
 GNOME Shell extension ──D-Bus (GApplication actions)──┐
   panel menu, preview tiles                           │
   reads $XDG_RUNTIME_DIR/reolink-desktop/preview/*.jpg ▼
                                    ┌──────── GTK main thread ───────────┐
 tray (ksni / Windows) ──────────►  │ async_channel<AppEvent> event loop │
                                    │  ├─ ui::MainWindow                 │
                                    │  │    live grid · recordings · settings
                                    │  ├─ preview::Preview (JPEG writer) │
                                    │  └─ actions: cameras/paused state  │
                                    └──────────────▲─────────────────────┘
 one events worker per camera ─────────────────────┘ CameraState(id, …)
   polls the HTTP API, sends notifications
```

- **GTK main thread:** the window, GStreamer buses, preview pipelines and the central event
  loop. Everything else sends it `AppEvent`s over one `async_channel`.
- **Event workers:** one OS thread per camera, polling the camera's HTTP API with blocking I/O.
- **Tray:** a StatusNotifierItem (`ksni`) is used only while the GNOME Shell extension is
  absent. The app watches the extension's bus name and switches over automatically. On
  Windows there is no extension: a notification-area icon (`tray-icon`, on its own thread
  with a Win32 message loop) is always used.

## Windows

The same code base builds for Windows (`x86_64-pc-windows-gnu`, MSYS2 UCRT64). Everything
that is Linux-only is behind `cfg(target_os = "linux")`: the GNOME extension handling,
the D-Bus name watching and `ksni`. The differences:

- tray: `src/tray/windows.rs` instead of `src/tray/sni.rs`; both show the menu from `src/tray/mod.rs`;
- launch at login: the `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value instead of
  an XDG autostart file;
- notifications: WinRT toasts through `notify-rust`. They need the AppUserModelID
  `io.github.ReolinkDesktop`, which the installer's Start-menu shortcut registers;
- `build.rs` embeds the icon in the `.exe`.

`scripts/build-windows.sh` stages the exe with its DLLs, GStreamer plugins, GIO modules,
pixbuf loaders, schemas and icons in a relocatable `bin/ lib/ share/` layout, and then
builds the Inno Setup installer (`packaging/windows/reolink-desktop.iss`). From Linux,
`make check-windows` type-checks the Windows code in Docker.

## Source map

| File | Role |
|---|---|
| `src/main.rs` | `adw::Application` (single instance), event loop, D-Bus actions, tray/extension switch-over, enables the extension once on GNOME |
| `src/ui.rs` | Main window: header (back, recordings, settings), page switching; only the visible page streams |
| `src/live.rs` | Live grid / single camera: one `Player` per tile with auto-retry, spotlight and snapshot |
| `src/recordings.rs` | SD-card playback: camera selector, calendar with recording days, clip list, seek over HTTPS (`cmd=Download` with range requests), auto-advance, save clip |
| `src/settings.rs` | Camera list (add / edit / remove), alerts, launch at login |
| `src/preview.rs` | While the panel menu is open: sub stream → 640 px JPEG at 3 fps per preview camera, written atomically |
| `src/events.rs` | Per-camera poller: rising-edge detection, per-type cooldown, "person replaces motion" |
| `src/reolink.rs` | Reolink HTTP JSON API: token login, HTTP→HTTPS upgrade, motion/AI state, snapshot, spotlight, recording search and download |
| `src/player.rs` | `playbin → glsinkbin → gtk4paintablesink` (plugin linked statically), RTSP over TCP, pause/seek |
| `src/tray/` | Tray menu and icon drawing (`mod.rs`); StatusNotifierItem for KDE, XFCE and others (`sni.rs`); Windows notification area (`windows.rs`) |
| `src/config.rs` | TOML config (mode 0600), launch at login, migration from older formats and from "Reolink Tray" |
| `gnome-extension/…/extension.js` | Panel button and menu, preview tiles (`St.ImageContent`), focuses the window it opens |

## D-Bus interface

The app exports GApplication actions on `io.github.ReolinkDesktop`, at
`/io/github/ReolinkDesktop`. The extension uses them, and they also work for custom keyboard
shortcuts:

```sh
gdbus call --session --dest io.github.ReolinkDesktop --object-path /io/github/ReolinkDesktop \
  --method org.gtk.Actions.Activate show-live [] {}
```

| Action | Purpose |
|---|---|
| `show-live`, `show-recordings`, `show-settings` | Open the window on that page |
| `show-camera` (string id) | Open one camera |
| `toggle-pause` | Pause or resume notifications |
| `preview-keepalive` | Keep the panel preview pipelines running (they stop after 6 s without it) |
| `quit` | Quit |
| `cameras` (state: JSON), `paused` (state: bool) | Read-only state for the extension |

The extension owns `io.github.ReolinkDesktop.ShellExtension` while it is enabled. That is how
the app knows to hide its tray icon.

## Camera API notes (tested with an RLC-510A, firmware v3.0.0.4348)

| Port | Service | Use |
|---|---|---|
| 443 | HTTPS JSON API (`/cgi-bin/api.cgi`) | events, snapshots, spotlight, recordings. HTTP/80 is often disabled and redirects, which the client follows by switching to HTTPS. |
| 554 | RTSP | `Preview_01_main` / `Preview_01_sub`; older firmware uses `h264Preview_01_*` |
| 1935 / 8000 / 9000 | RTMP / ONVIF / Baichuan | not used |

API commands used: `Login`, `Logout`, `GetMdState`, `GetAiState`, `Snap`, `GetWhiteLed`,
`SetWhiteLed`, `Search` and `Download`.

The main stream on many models is H.265. Decoding uses VA-API (`vah265dec`) automatically
when `va-driver-all` is installed.

## Configuration reference

`~/.config/reolink-desktop/config.toml` (mode 0600). All fields are optional:

```toml
[[camera]]
id = "cam-…"               # generated
name = "Front door"
host = "192.168.1.50"      # host[:port] of the HTTP(S) API
username = "admin"
password = "…"
stream = "sub"             # "main" | "sub" for the window; the panel preview always uses sub
rtsp_port = 554
rtsp_path = "Preview_01_{stream}"
use_https = true
channel = 0
preview = true             # show in the panel menu
alerts = true              # desktop notifications for this camera

[notifications]
enabled = true
motion = true
person = true
vehicle = false
animal = false
snapshot = true
cooldown_secs = 30
poll_interval_ms = 1000

[ui]
autostart = false
extension_offered = false  # set once the GNOME extension has been auto-enabled
```

## Build targets

```bash
make deps          # apt packages + rustup (needs sudo)
make test          # unit tests
make run           # run from source with debug logs
make               # release build + dist/reolink-desktop_<ver>_<arch>.deb
make install       # sudo apt install ./dist/…deb
make uninstall
make reset-config  # forget all cameras (the first-run setup appears again)
make extension-dev # symlink the extension from this checkout into ~/.local/share
```

Alternatively, `cargo install cargo-deb && cargo deb` uses `[package.metadata.deb]` in
`Cargo.toml`.

## Security notes

- Passwords are stored in plain text in a file only the user can read.
- RTSP credentials are passed to `rtspsrc` as properties, never in URLs or logs.
- TLS certificates are not verified, because cameras use self-signed certificates on the LAN.
