//! Reolink Desktop — live view, recordings and detection alerts for Reolink cameras.
//!
//! Threads:
//! * GTK main thread: window, GStreamer buses, previews, central event loop.
//! * ksni thread: StatusNotifierItem tray (only when the GNOME Shell extension
//!   is not running; the extension provides the panel menu on GNOME).
//! * one event worker per camera: polls the HTTP API, sends notifications.
//!
//! Everything talks to the main thread through one `async_channel` of
//! `AppEvent`s. The GNOME Shell extension talks to the app through the
//! application's D-Bus actions (`org.gtk.Actions` on `/io/github/ReolinkDesktop`).

mod config;
mod events;
mod live;
mod player;
mod preview;
mod recordings;
mod reolink;
mod settings;
mod tray;
mod ui;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use adw::prelude::*;
use gtk::{gio, glib};

use config::Config;

const APP_ID: &str = "io.github.ReolinkDesktop";
/// Bus name owned by the GNOME Shell extension while it is enabled.
const EXTENSION_BUS_NAME: &str = "io.github.ReolinkDesktop.ShellExtension";
const EXTENSION_UUID: &str = "reolink-desktop@reolink-desktop.github.io";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CamState {
    Unconfigured,
    Connecting,
    Online,
    Alert,
    Offline,
}

impl CamState {
    pub fn label(self) -> &'static str {
        match self {
            CamState::Unconfigured => "Not configured",
            CamState::Connecting => "Connecting",
            CamState::Online => "Online",
            CamState::Alert => "Motion",
            CamState::Offline => "Offline",
        }
    }

    fn key(self) -> &'static str {
        match self {
            CamState::Unconfigured => "unconfigured",
            CamState::Connecting => "connecting",
            CamState::Online => "online",
            CamState::Alert => "alert",
            CamState::Offline => "offline",
        }
    }

    /// For combining several cameras into one tray icon: higher is worse.
    fn severity(self) -> u8 {
        match self {
            CamState::Online => 0,
            CamState::Unconfigured => 1,
            CamState::Connecting => 2,
            CamState::Offline => 3,
            CamState::Alert => 4,
        }
    }
}

#[derive(Debug)]
pub enum AppEvent {
    ToggleWindow,
    ShowLive,
    ShowCamera(String),
    ShowRecordings,
    ShowSettings,
    CameraState(String, CamState),
    /// Sent after settings were saved.
    ConfigChanged,
    TogglePause,
    PreviewKeepalive,
    ExtensionPresent(bool),
    Quit,
}

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if let Err(e) = gst::init() {
        eprintln!("failed to initialise GStreamer: {e}");
        return glib::ExitCode::FAILURE;
    }
    if let Err(e) = gstgtk4::plugin_register_static() {
        eprintln!("failed to register gtk4paintablesink: {e}");
        return glib::ExitCode::FAILURE;
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    let (tx, rx) = async_channel::unbounded::<AppEvent>();

    app.connect_startup({
        let tx = tx.clone();
        move |app| startup(app, tx.clone(), rx.clone())
    });

    // GApplication is single-instance: launching the binary again (e.g. from the
    // app grid) activates the running instance, which then opens the window.
    // The very first activation is the initial launch and stays silent.
    let first = std::cell::Cell::new(true);
    app.connect_activate(move |_| {
        if !first.replace(false) {
            let _ = tx.send_blocking(AppEvent::ShowLive);
        }
    });

    // Don't let GTK interpret our argv.
    app.run_with_args::<&str>(&[])
}

fn startup(app: &adw::Application, tx: async_channel::Sender<AppEvent>, rx: async_channel::Receiver<AppEvent>) {
    let hold = app.hold(); // keep running with no visible window
    ui::load_css();

    let config = Arc::new(RwLock::new(Config::load()));
    let generation = Arc::new(AtomicU64::new(0));
    let paused = Arc::new(AtomicBool::new(false));

    let workers = events::Workers::new(config.clone(), generation.clone(), paused.clone(), tx.clone());
    workers.sync();
    let window = ui::MainWindow::new(app, config.clone(), generation, tx.clone());
    let preview = preview::Preview::new(config.clone());
    let actions = register_actions(app, &tx);

    // First run: nothing is stored yet, so open the setup page.
    enable_extension_once(&config);

    let configured = config.read().unwrap().is_configured();
    if !configured {
        let _ = tx.send_blocking(AppEvent::ShowSettings);
    }

    // The tray icon is only needed when the GNOME Shell extension is absent.
    let tx_watch = tx.clone();
    let tx_gone = tx.clone();
    // Lives as long as the app; never unwatched.
    let _ = gio::bus_watch_name(
        gio::BusType::Session,
        EXTENSION_BUS_NAME,
        gio::BusNameWatcherFlags::NONE,
        move |_, _, _| {
            let _ = tx_watch.send_blocking(AppEvent::ExtensionPresent(true));
        },
        move |_, _| {
            let _ = tx_gone.send_blocking(AppEvent::ExtensionPresent(false));
        },
    );

    // Without any panel integration there is no other way to reach the app.
    let tx_check = tx.clone();
    glib::timeout_add_seconds_local_once(2, move || {
        if configured && !extension_running() && !status_notifier_host_present() {
            log::warn!("no panel extension or StatusNotifier host found; showing the window");
            let _ = tx_check.send_blocking(AppEvent::ShowLive);
        }
    });

    let app = app.clone();
    glib::spawn_future_local(async move {
        let _hold = hold;
        let tray: RefCell<Option<ksni::blocking::Handle<tray::CamTray>>> = RefCell::new(None);
        let mut states: HashMap<String, CamState> = HashMap::new();
        let publish = |states: &HashMap<String, CamState>, tray: &RefCell<Option<_>>| {
            publish_state(&config.read().unwrap(), states, &paused, &actions, tray);
        };
        publish(&states, &tray);

        while let Ok(event) = rx.recv().await {
            match event {
                AppEvent::ToggleWindow => window.toggle(),
                AppEvent::ShowLive => window.show(ui::Page::Live),
                AppEvent::ShowCamera(id) => window.show_camera(&id),
                AppEvent::ShowRecordings => window.show(ui::Page::Recordings),
                AppEvent::ShowSettings => window.show(ui::Page::Settings),
                AppEvent::CameraState(id, state) => {
                    window.set_camera_state(&id, state);
                    states.insert(id, state);
                    publish(&states, &tray);
                }
                AppEvent::ConfigChanged => {
                    let ids: Vec<String> = config.read().unwrap().cameras.iter().map(|c| c.id.clone()).collect();
                    states.retain(|id, _| ids.contains(id));
                    workers.sync();
                    window.config_changed();
                    preview.reload();
                    publish(&states, &tray);
                }
                AppEvent::TogglePause => {
                    paused.fetch_xor(true, Ordering::Relaxed);
                    publish(&states, &tray);
                }
                AppEvent::PreviewKeepalive => preview.keepalive(),
                AppEvent::ExtensionPresent(present) => {
                    if present {
                        log::info!("GNOME Shell extension active; hiding the tray icon");
                        if let Some(handle) = tray.borrow_mut().take() {
                            handle.shutdown().wait();
                        }
                    } else if tray.borrow().is_none() {
                        *tray.borrow_mut() = tray::spawn(paused.clone(), tx.clone());
                        publish(&states, &tray);
                    }
                }
                AppEvent::Quit => {
                    window.shutdown();
                    if let Some(handle) = tray.borrow_mut().take() {
                        handle.shutdown().wait();
                    }
                    app.quit();
                    break;
                }
            }
        }
    });
}

/// D-Bus-visible application actions, used by the GNOME Shell extension (and
/// handy for custom keyboard shortcuts):
///
/// ```sh
/// gdbus call --session --dest io.github.ReolinkDesktop --object-path /io/github/ReolinkDesktop \
///   --method org.gtk.Actions.Activate show-live [] {}
/// ```
struct Actions {
    /// JSON list of cameras with their state, read by the extension.
    cameras: gio::SimpleAction,
    paused: gio::SimpleAction,
}

fn register_actions(app: &adw::Application, tx: &async_channel::Sender<AppEvent>) -> Actions {
    let simple: [(&str, fn() -> AppEvent); 6] = [
        ("show-live", || AppEvent::ShowLive),
        ("show-recordings", || AppEvent::ShowRecordings),
        ("show-settings", || AppEvent::ShowSettings),
        ("toggle-pause", || AppEvent::TogglePause),
        ("preview-keepalive", || AppEvent::PreviewKeepalive),
        ("quit", || AppEvent::Quit),
    ];
    for (name, event) in simple {
        let action = gio::SimpleAction::new(name, None);
        let tx = tx.clone();
        action.connect_activate(move |_, _| {
            let _ = tx.send_blocking(event());
        });
        app.add_action(&action);
    }

    let show_camera = gio::SimpleAction::new("show-camera", Some(glib::VariantTy::STRING));
    let tx_cam = tx.clone();
    show_camera.connect_activate(move |_, param| {
        if let Some(id) = param.and_then(|p| p.get::<String>()) {
            let _ = tx_cam.send_blocking(AppEvent::ShowCamera(id));
        }
    });
    app.add_action(&show_camera);

    // Read-only state holders (activating them does nothing).
    let cameras = gio::SimpleAction::new_stateful("cameras", None, &"[]".to_variant());
    let paused = gio::SimpleAction::new_stateful("paused", None, &false.to_variant());
    app.add_action(&cameras);
    app.add_action(&paused);
    Actions { cameras, paused }
}

/// Pushes the combined state to the extension (action state) and the tray.
fn publish_state(
    config: &Config,
    states: &HashMap<String, CamState>,
    paused: &AtomicBool,
    actions: &Actions,
    tray: &RefCell<Option<ksni::blocking::Handle<tray::CamTray>>>,
) {
    let state_of = |id: &str| states.get(id).copied().unwrap_or(CamState::Connecting);
    let list: Vec<serde_json::Value> = config
        .cameras
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "name": c.name,
                "state": state_of(&c.id).key(),
                "label": state_of(&c.id).label(),
                "preview": c.preview,
            })
        })
        .collect();
    let json = serde_json::Value::Array(list).to_string();
    actions.cameras.set_state(&json.to_variant());
    let is_paused = paused.load(Ordering::Relaxed);
    actions.paused.set_state(&is_paused.to_variant());

    let overall = if config.cameras.is_empty() {
        CamState::Unconfigured
    } else {
        config.cameras.iter().map(|c| state_of(&c.id)).max_by_key(|s| s.severity()).unwrap_or(CamState::Connecting)
    };
    let summary = config
        .cameras
        .iter()
        .map(|c| format!("{}: {}", c.name, state_of(&c.id).label()))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(handle) = &*tray.borrow() {
        handle.update(|t| {
            t.state = overall;
            t.summary = summary;
        });
    }
}

fn name_has_owner(name: &str) -> bool {
    let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else {
        return false;
    };
    let reply = conn.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&(name,).to_variant()),
        Some(glib::VariantTy::new("(b)").unwrap()),
        gio::DBusCallFlags::NONE,
        1000,
        None::<&gio::Cancellable>,
    );
    matches!(reply.ok().and_then(|v| v.get::<(bool,)>()), Some((true,)))
}

/// On GNOME, turns the bundled Shell extension on the first time the app runs,
/// so the panel menu works without a manual `gnome-extensions enable`.
///
/// It edits the shell's `enabled-extensions` list instead of calling
/// `EnableExtension` over D-Bus: that also works when the shell has not
/// discovered the extension yet (it only scans at login) and when the app
/// starts before the shell finished loading extensions. Runs once per user: if
/// they disable the extension later, that choice stays.
fn enable_extension_once(config: &Arc<RwLock<Config>>) {
    let on_gnome = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_uppercase().contains("GNOME"));
    if !on_gnome || config.read().unwrap().ui.extension_offered {
        return;
    }
    let has_schema = gio::SettingsSchemaSource::default()
        .and_then(|src| src.lookup("org.gnome.shell", true))
        .is_some_and(|schema| schema.has_key("enabled-extensions"));
    if !has_schema {
        return;
    }
    let shell = gio::Settings::new("org.gnome.shell");
    let mut enabled: Vec<String> = shell.strv("enabled-extensions").iter().map(|s| s.to_string()).collect();
    if !enabled.iter().any(|u| u == EXTENSION_UUID) {
        enabled.push(EXTENSION_UUID.to_owned());
        let refs: Vec<&str> = enabled.iter().map(String::as_str).collect();
        if let Err(e) = shell.set_strv("enabled-extensions", refs.as_slice()) {
            log::warn!("could not enable the panel extension: {e}");
            return;
        }
        log::info!("enabled the GNOME Shell panel extension (active after the next login if newly installed)");
    }
    let disabled: Vec<String> = shell.strv("disabled-extensions").iter().map(|s| s.to_string()).collect();
    if disabled.iter().any(|u| u == EXTENSION_UUID) {
        let rest: Vec<&str> = disabled.iter().map(String::as_str).filter(|u| *u != EXTENSION_UUID).collect();
        let _ = shell.set_strv("disabled-extensions", rest.as_slice());
    }
    gio::Settings::sync();

    let mut cfg = config.read().unwrap().clone();
    cfg.ui.extension_offered = true;
    if cfg.save().is_ok() {
        *config.write().unwrap() = cfg;
    }
}

fn extension_running() -> bool {
    name_has_owner(EXTENSION_BUS_NAME)
}

/// Is a tray host (`org.kde.StatusNotifierWatcher`) on the session bus?
fn status_notifier_host_present() -> bool {
    name_has_owner("org.kde.StatusNotifierWatcher")
}
