//! Live view: a grid of every camera, or one camera at full size with its
//! controls (spotlight, snapshot). Streams run only while the view is shown.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, RwLock};

use adw::prelude::*;
use gtk::glib;

use crate::config::{CameraConfig, Config};
use crate::player::{Player, PlayerEvent};
use crate::reolink::Client;
use crate::CamState;

const RETRY_SECS: u32 = 3;
const STATE_CLASSES: [&str; 4] = ["connecting", "online", "alert", "offline"];

// ── One camera tile ───────────────────────────────────────────────────────

struct Tile {
    id: String,
    overlay: gtk::Overlay,
    status: gtk::Label,
    chip: gtk::Box,
    player: Option<Player>,
    streaming: Cell<bool>,
    retry: RefCell<Option<glib::SourceId>>,
    config: Arc<RwLock<Config>>,
}

impl Tile {
    fn new(cam: &CameraConfig, config: Arc<RwLock<Config>>, on_click: impl Fn(&str) + 'static) -> Rc<Tile> {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_size_request(320, 180);
        picture.set_hexpand(true);
        picture.set_vexpand(true);

        let status = gtk::Label::new(Some("Connecting…"));
        status.add_css_class("video-status");
        status.set_halign(gtk::Align::Center);
        status.set_valign(gtk::Align::Center);

        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chip.add_css_class("tile-chip");
        chip.add_css_class("state");
        chip.set_halign(gtk::Align::Start);
        chip.set_valign(gtk::Align::Start);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("state-dot");
        dot.set_valign(gtk::Align::Center);
        let name = gtk::Label::new(Some(&cam.name));
        name.add_css_class("caption-heading");
        chip.append(&dot);
        chip.append(&name);

        let overlay = gtk::Overlay::new();
        overlay.add_css_class("video-frame");
        overlay.set_overflow(gtk::Overflow::Hidden);
        overlay.set_child(Some(&picture));
        overlay.add_overlay(&status);
        overlay.add_overlay(&chip);

        let tile = Rc::new_cyclic(|weak: &Weak<Tile>| {
            let weak = weak.clone();
            let player = match Player::new(false, move |ev| {
                if let Some(t) = weak.upgrade() {
                    t.on_player_event(ev);
                }
            }) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::error!("video disabled: {e:#}");
                    status.set_label("Video unavailable — see log");
                    None
                }
            };
            Tile {
                id: cam.id.clone(),
                overlay,
                status,
                chip,
                player,
                streaming: Cell::new(false),
                retry: RefCell::new(None),
                config,
            }
        });
        if let Some(p) = &tile.player {
            picture.set_paintable(Some(p.paintable()));
        }

        let click = gtk::GestureClick::new();
        let id = tile.id.clone();
        click.connect_released(move |_, _, _, _| on_click(&id));
        tile.overlay.add_controller(click);
        tile.set_state(CamState::Connecting);
        tile
    }

    fn camera(&self) -> Option<CameraConfig> {
        self.config.read().unwrap().camera(&self.id).cloned()
    }

    fn set_streaming(self: &Rc<Self>, on: bool) {
        if on == self.streaming.get() {
            return;
        }
        self.streaming.set(on);
        if on {
            self.start();
        } else {
            self.cancel_retry();
            if let Some(p) = &self.player {
                p.stop();
            }
        }
    }

    fn start(&self) {
        self.cancel_retry();
        let (Some(player), Some(cam)) = (&self.player, self.camera()) else { return };
        self.show_status("Connecting…");
        log::info!("{}: playing {}", cam.name, cam.rtsp_url());
        player.play(&cam.rtsp_url(), &cam.username, &cam.password);
    }

    fn on_player_event(self: &Rc<Self>, ev: PlayerEvent) {
        if !self.streaming.get() {
            return;
        }
        match ev {
            PlayerEvent::Playing => self.status.set_visible(false),
            PlayerEvent::Eos | PlayerEvent::Error(_) => {
                if let PlayerEvent::Error(msg) = &ev {
                    self.status.set_tooltip_text(Some(msg));
                }
                self.show_status("Stream unavailable — retrying…");
                if self.retry.borrow().is_none() {
                    let weak = Rc::downgrade(self);
                    let id = glib::timeout_add_seconds_local_once(RETRY_SECS, move || {
                        if let Some(t) = weak.upgrade() {
                            t.retry.take(); // this source is done; forget its id
                            if t.streaming.get() {
                                t.start();
                            }
                        }
                    });
                    self.retry.replace(Some(id));
                }
            }
        }
    }

    fn cancel_retry(&self) {
        if let Some(id) = self.retry.take() {
            id.remove();
        }
    }

    fn show_status(&self, text: &str) {
        self.status.set_label(text);
        self.status.set_visible(true);
    }

    fn set_state(&self, state: CamState) {
        set_state_classes(&self.chip, state);
        self.chip.set_tooltip_text(Some(state.label()));
    }
}

impl Drop for Tile {
    fn drop(&mut self) {
        self.cancel_retry();
    }
}

// ── The view ──────────────────────────────────────────────────────────────

pub struct LiveView(Rc<Inner>);

struct Inner {
    widget: gtk::Box,
    grid: gtk::Grid,
    empty: adw::StatusPage,
    controls: gtk::Box,
    state_box: gtk::Box,
    state_label: gtk::Label,
    spotlight: gtk::ToggleButton,
    tiles: RefCell<Vec<Rc<Tile>>>,
    states: RefCell<HashMap<String, CamState>>,
    focused: RefCell<Option<String>>,
    active: Cell<bool>,
    syncing: Cell<bool>,
    on_layout: RefCell<Option<Box<dyn Fn()>>>,
    toasts: adw::ToastOverlay,
    config: Arc<RwLock<Config>>,
}

impl LiveView {
    pub fn new(config: Arc<RwLock<Config>>, toasts: adw::ToastOverlay) -> Self {
        let grid = gtk::Grid::builder()
            .row_spacing(8)
            .column_spacing(8)
            .row_homogeneous(true)
            .column_homogeneous(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("camera-web-symbolic")
            .title("No cameras")
            .description("Add a camera in Settings.")
            .vexpand(true)
            .build();

        let state_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        state_box.add_css_class("state");
        state_box.set_hexpand(true);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("state-dot");
        dot.set_valign(gtk::Align::Center);
        let state_label = gtk::Label::new(None);
        state_label.add_css_class("caption-heading");
        state_box.append(&dot);
        state_box.append(&state_label);

        let spotlight = crate::ui::toggle_button("display-brightness-symbolic", "Spotlight");
        spotlight.set_visible(false); // shown once the camera reports a spotlight
        let snapshot = crate::ui::icon_button("camera-photo-symbolic", "Save snapshot to Pictures");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        controls.add_css_class("controls");
        controls.append(&state_box);
        controls.append(&spotlight);
        controls.append(&snapshot);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("live");
        widget.append(&grid);
        widget.append(&empty);
        widget.append(&controls);

        let inner = Rc::new(Inner {
            widget,
            grid,
            empty,
            controls,
            state_box,
            state_label,
            spotlight,
            tiles: RefCell::default(),
            states: RefCell::default(),
            focused: RefCell::default(),
            active: Cell::new(false),
            syncing: Cell::new(false),
            on_layout: RefCell::default(),
            toasts,
            config,
        });

        let w = Rc::downgrade(&inner);
        inner.spotlight.connect_toggled(move |b| {
            with(&w, |i| {
                if !i.syncing.get() {
                    i.set_spotlight(b.is_active());
                }
            })
        });
        let w = Rc::downgrade(&inner);
        snapshot.connect_clicked(move |_| with(&w, |i| i.save_snapshot()));

        let view = LiveView(inner);
        view.rebuild();
        view
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.0.widget
    }

    /// Called after layout changes (focus), e.g. to update the header.
    pub fn connect_layout_changed(&self, f: impl Fn() + 'static) {
        self.0.on_layout.replace(Some(Box::new(f)));
    }

    /// Recreates the tiles from the current config.
    pub fn rebuild(&self) {
        let i = &self.0;
        for t in i.tiles.borrow().iter() {
            t.set_streaming(false);
        }
        let cameras = i.config.read().unwrap().cameras.clone();
        let weak = Rc::downgrade(i);
        let tiles = cameras
            .iter()
            .map(|cam| {
                let weak = weak.clone();
                let tile = Tile::new(cam, i.config.clone(), move |id| {
                    with(&weak, |i| {
                        // Clicking a tile in the grid opens that camera.
                        if i.focused.borrow().is_none() && i.tiles.borrow().len() > 1 {
                            i.focus(Some(id.to_owned()));
                        }
                    })
                });
                if let Some(state) = i.states.borrow().get(&cam.id) {
                    tile.set_state(*state);
                }
                tile
            })
            .collect();
        *i.tiles.borrow_mut() = tiles;
        let still_there = i.focused.borrow().as_ref().is_some_and(|id| cameras.iter().any(|c| &c.id == id));
        if !still_there {
            i.focused.replace(None);
        }
        i.relayout();
    }

    pub fn set_active(&self, active: bool) {
        self.0.active.set(active);
        self.0.sync_streams();
        if active {
            self.0.refresh_spotlight();
        }
    }

    pub fn focus(&self, id: Option<String>) {
        self.0.focus(id);
    }

    /// Camera shown alone (focused, or the only one).
    pub fn single_camera(&self) -> Option<CameraConfig> {
        self.0.single_id().and_then(|id| self.0.config.read().unwrap().camera(&id).cloned())
    }

    /// True when a camera was picked from a grid (a "back" button makes sense).
    pub fn is_focused(&self) -> bool {
        self.0.focused.borrow().is_some() && self.0.tiles.borrow().len() > 1
    }

    pub fn set_state(&self, id: &str, state: CamState) {
        let i = &self.0;
        i.states.borrow_mut().insert(id.to_owned(), state);
        if let Some(t) = i.tiles.borrow().iter().find(|t| t.id == id) {
            t.set_state(state);
        }
        if i.single_id().as_deref() == Some(id) {
            set_state_classes(&i.state_box, state);
            i.state_label.set_label(state.label());
        }
    }

    pub fn shutdown(&self) {
        for t in self.0.tiles.borrow().iter() {
            t.set_streaming(false);
        }
        self.0.tiles.borrow_mut().clear();
    }
}

impl Inner {
    fn single_id(&self) -> Option<String> {
        let tiles = self.tiles.borrow();
        match &*self.focused.borrow() {
            Some(id) => Some(id.clone()),
            None if tiles.len() == 1 => Some(tiles[0].id.clone()),
            None => None,
        }
    }

    fn focus(self: &Rc<Self>, id: Option<String>) {
        self.focused.replace(id);
        self.relayout();
        self.refresh_spotlight();
    }

    fn relayout(self: &Rc<Self>) {
        while let Some(child) = self.grid.first_child() {
            self.grid.remove(&child);
        }
        let single = self.single_id();
        let tiles = self.tiles.borrow();
        let visible: Vec<&Rc<Tile>> = tiles.iter().filter(|t| single.as_ref().is_none_or(|id| &t.id == id)).collect();
        let columns = (visible.len() as f64).sqrt().ceil().max(1.0) as usize;
        for (n, tile) in visible.iter().enumerate() {
            self.grid.attach(&tile.overlay, (n % columns) as i32, (n / columns) as i32, 1, 1);
            // In a single view the name chip is redundant with the header.
            tile.chip.set_visible(single.is_none());
            tile.overlay.set_cursor_from_name(if single.is_none() { Some("pointer") } else { None });
        }
        self.empty.set_visible(tiles.is_empty());
        self.grid.set_visible(!tiles.is_empty());
        self.controls.set_visible(single.is_some());
        if let Some(id) = &single {
            let state = self.states.borrow().get(id).copied().unwrap_or(CamState::Connecting);
            set_state_classes(&self.state_box, state);
            self.state_label.set_label(state.label());
        }
        drop(tiles);
        self.sync_streams();
        if let Some(f) = &*self.on_layout.borrow() {
            f();
        }
    }

    /// Streams exactly the tiles on screen while the view is active.
    fn sync_streams(&self) {
        let single = self.single_id();
        for t in self.tiles.borrow().iter() {
            let visible = single.as_ref().is_none_or(|id| &t.id == id);
            t.set_streaming(self.active.get() && visible);
        }
    }

    fn single_camera(&self) -> Option<CameraConfig> {
        self.single_id().and_then(|id| self.config.read().unwrap().camera(&id).cloned())
    }

    fn set_toggle(&self, button: &gtk::ToggleButton, active: bool) {
        self.syncing.set(true);
        button.set_active(active);
        self.syncing.set(false);
    }

    /// Queries the spotlight; hides the button on cameras without one.
    fn refresh_spotlight(self: &Rc<Self>) {
        self.spotlight.set_visible(false);
        let Some(cam) = self.single_camera() else { return };
        let id = cam.id.clone();
        let weak = Rc::downgrade(self);
        camera_task(cam, |c| c.spotlight(), move |res| {
            with(&weak, |i| {
                if i.single_id().as_deref() != Some(id.as_str()) {
                    return;
                }
                match res {
                    Ok(state) => {
                        i.spotlight.set_visible(state.is_some());
                        i.set_toggle(&i.spotlight, state.unwrap_or(false));
                    }
                    Err(e) => log::debug!("spotlight state: {e:#}"),
                }
            })
        });
    }

    fn set_spotlight(self: &Rc<Self>, on: bool) {
        let Some(cam) = self.single_camera() else { return };
        let weak = Rc::downgrade(self);
        camera_task(cam, move |c| c.set_spotlight(on), move |res| {
            if let Err(e) = res {
                log::warn!("spotlight: {e:#}");
                with(&weak, |i| {
                    i.set_toggle(&i.spotlight, !on);
                    i.toasts.add_toast(adw::Toast::new("Could not switch the spotlight"));
                });
            }
        });
    }

    fn save_snapshot(self: &Rc<Self>) {
        let Some(cam) = self.single_camera() else { return };
        let dir = glib::user_special_dir(glib::UserDirectory::Pictures)
            .unwrap_or_else(glib::home_dir)
            .join("Reolink");
        let stamp = glib::DateTime::now_local()
            .and_then(|d| d.format("%Y-%m-%d %H.%M.%S"))
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "snapshot".into());
        let path = dir.join(format!("{} {stamp}.jpg", cam.name.replace('/', "-")));
        let weak = Rc::downgrade(self);
        camera_task(
            cam,
            move |c| {
                let jpeg = c.snapshot()?;
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&path, jpeg)?;
                Ok(path)
            },
            move |res| {
                with(&weak, |i| {
                    let msg = match res {
                        Ok(path) => format!("Saved {}", path.file_name().unwrap_or_default().to_string_lossy()),
                        Err(e) => {
                            log::warn!("snapshot: {e:#}");
                            "Snapshot failed".into()
                        }
                    };
                    i.toasts.add_toast(adw::Toast::new(&msg));
                })
            },
        );
    }
}

fn set_state_classes(widget: &impl IsA<gtk::Widget>, state: CamState) {
    for class in STATE_CLASSES {
        widget.remove_css_class(class);
    }
    widget.add_css_class(match state {
        CamState::Connecting => "connecting",
        CamState::Online => "online",
        CamState::Alert => "alert",
        CamState::Offline | CamState::Unconfigured => "offline",
    });
}

/// Runs a short camera API job on a worker thread (own session, so it never
/// blocks the UI or the event poller) and delivers the result on the GTK thread.
fn camera_task<T: Send + 'static>(
    cam: CameraConfig,
    job: impl FnOnce(&mut Client) -> anyhow::Result<T> + Send + 'static,
    done: impl FnOnce(anyhow::Result<T>) + 'static,
) {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let res = Client::new(&cam).and_then(|mut c| {
            c.login()?;
            let res = job(&mut c);
            c.logout();
            res
        });
        let _ = tx.send_blocking(res);
    });
    glib::spawn_future_local(async move {
        if let Ok(res) = rx.recv().await {
            done(res);
        }
    });
}

fn with(weak: &Weak<Inner>, f: impl FnOnce(&Rc<Inner>)) {
    if let Some(inner) = weak.upgrade() {
        f(&inner);
    }
}
