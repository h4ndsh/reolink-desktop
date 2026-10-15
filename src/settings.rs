//! Settings: camera management (add / edit / test / remove), alerts and
//! general options. Switches apply immediately (GNOME style); the camera
//! editor has explicit Save.

use std::cell::{Cell, RefCell};
use std::net::{TcpStream, ToSocketAddrs};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;

use crate::config::{self, CameraConfig, Config, StreamQuality};
use crate::reolink::{ApiError, Client, DeviceInfo};
use crate::AppEvent;

pub struct SettingsView(Rc<Inner>);

struct Inner {
    widget: gtk::Stack,
    cameras_group: adw::PreferencesGroup,
    camera_rows: RefCell<Vec<adw::ActionRow>>,
    notify: adw::SwitchRow,
    motion: adw::SwitchRow,
    person: adw::SwitchRow,
    vehicle: adw::SwitchRow,
    animal: adw::SwitchRow,
    snapshot: adw::SwitchRow,
    cooldown: adw::SpinRow,
    autostart: adw::SwitchRow,
    editor: Editor,
    /// Set while widgets are filled from the config (suppresses auto-apply).
    loading: Cell<bool>,
    toasts: adw::ToastOverlay,
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    tx: async_channel::Sender<AppEvent>,
}

struct Editor {
    page: gtk::Box,
    group: adw::PreferencesGroup,
    name: adw::EntryRow,
    host: adw::EntryRow,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    stream: adw::ComboRow,
    https: adw::SwitchRow,
    preview: adw::SwitchRow,
    alerts: adw::SwitchRow,
    status: adw::ActionRow,
    status_icon: gtk::Image,
    spinner: gtk::Spinner,
    test: gtk::Button,
    web: adw::ActionRow,
    remove_group: adw::PreferencesGroup,
    remove: gtk::Button,
    cancel: gtk::Button,
    save: gtk::Button,
    /// `None` while adding a new camera.
    editing: RefCell<Option<String>>,
    /// Connection settings that last passed the test (see `CameraConfig::connection_key`).
    tested_ok: RefCell<Option<String>>,
    /// Bumped per test so late results of an older test are ignored.
    test_run: Cell<u64>,
}

impl SettingsView {
    pub fn new(
        config: Arc<RwLock<Config>>,
        generation: Arc<AtomicU64>,
        tx: async_channel::Sender<AppEvent>,
        toasts: adw::ToastOverlay,
    ) -> Self {
        let switch = |title: &str, subtitle: &str| adw::SwitchRow::builder().title(title).subtitle(subtitle).build();

        // ── List page ─────────────────────────────────────────────
        let add = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label("Add").build())
            .tooltip_text("Add camera")
            .valign(gtk::Align::Center)
            .build();
        add.add_css_class("flat");
        let cameras_group = adw::PreferencesGroup::builder().title("Cameras").build();
        cameras_group.set_header_suffix(Some(&add));

        let notify = switch("Notifications", "Desktop alerts for detections");
        let motion = switch("Motion", "");
        let person = switch("Person", "");
        let vehicle = switch("Vehicle", "");
        let animal = switch("Animal", "");
        let snapshot = switch("Attach snapshot", "Show the frame in the notification");
        let cooldown = adw::SpinRow::with_range(0.0, 3600.0, 5.0);
        cooldown.set_title("Cooldown");
        cooldown.set_subtitle("Seconds between alerts of the same kind");
        let alerts = adw::PreferencesGroup::builder().title("Alerts").build();
        for row in [&notify, &motion, &person, &vehicle, &animal, &snapshot] {
            alerts.add(row);
        }
        alerts.add(&cooldown);
        for dependent in [&motion, &person, &vehicle, &animal, &snapshot] {
            notify.bind_property("active", dependent, "sensitive").sync_create().build();
        }
        notify.bind_property("active", &cooldown, "sensitive").sync_create().build();

        let autostart = switch("Launch at login", "");
        let general = adw::PreferencesGroup::builder().title("General").build();
        general.add(&autostart);

        let list = adw::PreferencesPage::new();
        list.add(&cameras_group);
        list.add(&alerts);
        list.add(&general);

        // ── Camera editor page ────────────────────────────────────
        let editor = Editor::new();

        let widget = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::SlideLeftRight)
            .vhomogeneous(false)
            .build();
        widget.set_size_request(480, 520);
        widget.add_named(&list, Some("list"));
        widget.add_named(&editor.page, Some("edit"));

        let inner = Rc::new(Inner {
            widget,
            cameras_group,
            camera_rows: RefCell::default(),
            notify,
            motion,
            person,
            vehicle,
            animal,
            snapshot,
            cooldown,
            autostart,
            editor,
            loading: Cell::new(false),
            toasts,
            config,
            generation,
            tx,
        });

        let w = Rc::downgrade(&inner);
        add.connect_clicked(move |_| with(&w, |i| i.edit(None)));

        for row in [&inner.notify, &inner.motion, &inner.person, &inner.vehicle, &inner.animal, &inner.snapshot, &inner.autostart] {
            let w = Rc::downgrade(&inner);
            row.connect_notify_local(Some("active"), move |_, _| with(&w, |i| i.apply_globals()));
        }
        let w = Rc::downgrade(&inner);
        inner.cooldown.connect_notify_local(Some("value"), move |_, _| with(&w, |i| i.apply_globals()));

        let w = Rc::downgrade(&inner);
        inner.editor.cancel.connect_clicked(move |_| with(&w, |i| i.show_list()));
        let w = Rc::downgrade(&inner);
        inner.editor.save.connect_clicked(move |_| with(&w, |i| i.save_camera()));
        let w = Rc::downgrade(&inner);
        inner.editor.test.connect_clicked(move |_| with(&w, |i| i.test_connection(None)));
        let w = Rc::downgrade(&inner);
        inner.editor.web.connect_activated(move |_| with(&w, |i| i.open_web_ui()));
        let w = Rc::downgrade(&inner);
        inner.editor.remove.connect_clicked(move |_| {
            with(&w, |i| {
                if let Some(id) = i.editor.editing.borrow().clone() {
                    i.confirm_remove(id);
                }
            })
        });
        // Editing connection fields invalidates a previous test result.
        for entry in [
            inner.editor.host.upcast_ref::<gtk::Editable>(),
            inner.editor.username.upcast_ref(),
            inner.editor.password.upcast_ref(),
        ] {
            let w = Rc::downgrade(&inner);
            entry.connect_changed(move |_| with(&w, |i| i.editor.reset_status()));
        }
        let w = Rc::downgrade(&inner);
        inner.editor.https.connect_notify_local(Some("active"), move |_, _| with(&w, |i| i.editor.reset_status()));

        let view = SettingsView(inner);
        view.reload();
        view
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.0.widget
    }

    /// Refreshes every widget from the config and shows the camera list (or
    /// the "add camera" form on first run).
    pub fn reload(&self) {
        self.0.load();
        self.0.show_list();
    }
}

impl Inner {
    fn load(self: &Rc<Self>) {
        self.loading.set(true);
        let cfg = self.config.read().unwrap().clone();

        for row in self.camera_rows.borrow_mut().drain(..) {
            self.cameras_group.remove(&row);
        }
        for cam in &cfg.cameras {
            let row = self.camera_row(cam);
            self.cameras_group.add(&row);
            self.camera_rows.borrow_mut().push(row);
        }
        let summary = match cfg.cameras.len() {
            0 => "No cameras yet. Click Add to set one up.".to_owned(),
            1 => "1 camera".to_owned(),
            n => format!("{n} cameras"),
        };
        self.cameras_group.set_description(Some(&summary));

        let n = &cfg.notifications;
        self.notify.set_active(n.enabled);
        self.motion.set_active(n.motion);
        self.person.set_active(n.person);
        self.vehicle.set_active(n.vehicle);
        self.animal.set_active(n.animal);
        self.snapshot.set_active(n.snapshot);
        self.cooldown.set_value(n.cooldown_secs as f64);
        self.autostart.set_active(cfg.ui.autostart);
        self.loading.set(false);
    }

    /// One camera in the list: name, address, and edit / remove buttons.
    fn camera_row(self: &Rc<Self>, cam: &CameraConfig) -> adw::ActionRow {
        let mut details = vec![cam.host.clone()];
        details.push(if cam.stream == StreamQuality::Main { "Main stream".into() } else { "Sub stream".into() });
        if cam.preview {
            details.push("panel preview".into());
        }
        if !cam.alerts {
            details.push("alerts off".into());
        }
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&cam.name))
            .subtitle(glib::markup_escape_text(&details.join(" · ")))
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("camera-web-symbolic"));

        let edit = icon_button("document-edit-symbolic", "Edit camera");
        let remove = icon_button("user-trash-symbolic", "Remove camera");
        remove.add_css_class("error");
        row.add_suffix(&edit);
        row.add_suffix(&remove);

        let id = cam.id.clone();
        let w = Rc::downgrade(self);
        row.connect_activated(move |_| with(&w, |i| i.edit(Some(id.clone()))));
        let id = cam.id.clone();
        let w = Rc::downgrade(self);
        edit.connect_clicked(move |_| with(&w, |i| i.edit(Some(id.clone()))));
        let id = cam.id.clone();
        let w = Rc::downgrade(self);
        remove.connect_clicked(move |_| with(&w, |i| i.confirm_remove(id.clone())));
        row
    }

    fn show_list(self: &Rc<Self>) {
        if self.config.read().unwrap().is_configured() {
            self.widget.set_visible_child_name("list");
        } else {
            self.edit(None);
        }
    }

    fn apply_globals(&self) {
        if self.loading.get() {
            return;
        }
        let mut cfg = self.config.read().unwrap().clone();
        let old_autostart = cfg.ui.autostart;
        let n = &mut cfg.notifications;
        n.enabled = self.notify.is_active();
        n.motion = self.motion.is_active();
        n.person = self.person.is_active();
        n.vehicle = self.vehicle.is_active();
        n.animal = self.animal.is_active();
        n.snapshot = self.snapshot.is_active();
        n.cooldown_secs = self.cooldown.value().max(0.0) as u64;
        cfg.ui.autostart = self.autostart.is_active();
        if cfg.ui.autostart != old_autostart {
            if let Err(e) = config::set_autostart(cfg.ui.autostart) {
                log::warn!("autostart: {e:#}");
            }
        }
        self.commit(cfg);
    }

    fn edit(self: &Rc<Self>, id: Option<String>) {
        let cfg = self.config.read().unwrap().clone();
        let cam = id.as_ref().and_then(|id| cfg.camera(id).cloned());
        let first_run = !cfg.is_configured();
        self.editor.load(cam.as_ref(), first_run);
        self.editor.editing.replace(cam.map(|c| c.id));
        self.widget.set_visible_child_name("edit");
        self.editor.name.grab_focus();
    }

    /// Camera from the form, based on the stored one (keeps hidden fields).
    fn camera_from_form(&self) -> Result<CameraConfig, String> {
        let cfg = self.config.read().unwrap();
        let base = self
            .editor
            .editing
            .borrow()
            .as_ref()
            .and_then(|id| cfg.camera(id).cloned())
            .unwrap_or_else(|| CameraConfig { id: config::new_camera_id(), ..Default::default() });
        self.editor.store(base)
    }

    /// Tests the connection with the values in the form. `then` runs with the
    /// result (used by Save when adding a camera).
    fn test_connection(self: &Rc<Self>, then: Option<Box<dyn FnOnce(&Rc<Inner>, CameraConfig, bool)>>) {
        let cam = match self.camera_from_form() {
            Ok(cam) => cam,
            Err(msg) => {
                self.editor.show_status(Status::Failed, &msg);
                return;
            }
        };
        let run = self.editor.test_run.get() + 1;
        self.editor.test_run.set(run);
        self.editor.show_status(Status::Testing, "Connecting to the camera…");

        let (tx, rx) = async_channel::bounded(1);
        let probe = cam.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(probe_camera(&probe));
        });
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            with(&w, |i| {
                if i.editor.test_run.get() != run {
                    return; // a newer test or an edit superseded this one
                }
                let ok = match result {
                    Ok(info) => {
                        let model = if info.model.is_empty() { "Reolink camera".to_owned() } else { info.model.clone() };
                        let mut text = format!("Connected to {model}");
                        if !info.firmware.is_empty() {
                            text.push_str(&format!(" · firmware {}", info.firmware));
                        }
                        i.editor.show_status(Status::Ok, &text);
                        i.editor.tested_ok.replace(Some(connection_key(&cam)));
                        // Suggest the name set on the camera.
                        if i.editor.name.text().trim().is_empty() && !info.name.is_empty() {
                            i.editor.name.set_text(&info.name);
                        }
                        true
                    }
                    Err(e) => {
                        i.editor.show_status(Status::Failed, &format!("{e:#}"));
                        false
                    }
                };
                if let Some(then) = then {
                    then(i, cam, ok);
                }
            });
        });
    }

    fn save_camera(self: &Rc<Self>) {
        let cam = match self.camera_from_form() {
            Ok(cam) => cam,
            Err(msg) => {
                self.toasts.add_toast(adw::Toast::new(&msg));
                return;
            }
        };
        let adding = self.editor.editing.borrow().is_none();
        let tested = self.editor.tested_ok.borrow().as_deref() == Some(connection_key(&cam).as_str());
        if !adding || tested {
            self.store_camera(cam);
            return;
        }
        // Adding: test first, ask before saving a camera that does not answer.
        self.test_connection(Some(Box::new(|i, cam, ok| {
            // The test may have filled in the name: take the form's values again.
            let cam = i.camera_from_form().map(|mut c| {
                c.id = cam.id;
                c
            });
            let Ok(cam) = cam else { return };
            if ok {
                i.store_camera(cam);
            } else {
                i.confirm_save_unreachable(cam);
            }
        })));
    }

    fn confirm_save_unreachable(self: &Rc<Self>, cam: CameraConfig) {
        let dialog = adw::AlertDialog::new(
            Some("Camera not reachable"),
            Some("The connection test failed (see the message in the form). Save this camera anyway?"),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("save", "Save Anyway");
        dialog.set_default_response(Some("cancel"));
        let w = Rc::downgrade(self);
        let cam = RefCell::new(Some(cam));
        dialog.connect_response(None, move |_, response| {
            if response == "save" {
                if let Some(cam) = cam.take() {
                    with(&w, |i| i.store_camera(cam));
                }
            }
        });
        dialog.present(Some(&self.widget));
    }

    fn store_camera(self: &Rc<Self>, cam: CameraConfig) {
        let mut cfg = self.config.read().unwrap().clone();
        let was_configured = cfg.is_configured();
        let name = cam.name.clone();
        match cfg.cameras.iter_mut().find(|c| c.id == cam.id) {
            Some(slot) => *slot = cam,
            None => cfg.cameras.push(cam),
        }
        if self.commit(cfg) {
            self.load();
            self.show_list();
            self.toasts.add_toast(adw::Toast::new(&format!("“{name}” saved")));
            if !was_configured {
                let _ = self.tx.send_blocking(AppEvent::ShowLive);
            }
        }
    }

    fn confirm_remove(self: &Rc<Self>, id: String) {
        let name = self.config.read().unwrap().camera(&id).map(|c| c.name.clone()).unwrap_or_default();
        let dialog = adw::AlertDialog::new(
            Some(&format!("Remove “{name}”?")),
            Some("Its settings are deleted from this computer. Recordings on the camera are not affected."),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", "Remove");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        let w = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "remove" {
                return;
            }
            with(&w, |i| {
                let mut cfg = i.config.read().unwrap().clone();
                cfg.cameras.retain(|c| c.id != id);
                if i.commit(cfg) {
                    i.load();
                    i.show_list();
                    i.toasts.add_toast(adw::Toast::new(&format!("“{name}” removed")));
                }
            })
        });
        dialog.present(Some(&self.widget));
    }

    fn open_web_ui(&self) {
        let Ok(cam) = self.camera_from_form() else { return };
        let url = cam.web_url();
        if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(&url, None::<&gtk::gio::AppLaunchContext>) {
            log::warn!("could not open {url}: {e}");
        }
    }

    /// Saves, publishes and announces a new config. Returns false on failure.
    fn commit(&self, cfg: Config) -> bool {
        if let Err(e) = cfg.save() {
            self.toasts.add_toast(adw::Toast::new(&format!("Could not save settings: {e:#}")));
            return false;
        }
        *self.config.write().unwrap() = cfg;
        self.generation.fetch_add(1, Ordering::SeqCst);
        let _ = self.tx.send_blocking(AppEvent::ConfigChanged);
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Untested,
    Testing,
    Ok,
    Failed,
}

impl Editor {
    fn new() -> Self {
        let entry = |title: &str| adw::EntryRow::builder().title(title).build();
        let switch = |title: &str, subtitle: &str| adw::SwitchRow::builder().title(title).subtitle(subtitle).build();

        let editor = Self {
            page: gtk::Box::new(gtk::Orientation::Vertical, 0),
            group: adw::PreferencesGroup::new(),
            name: entry("Name"),
            host: entry("Address (IP or host)"),
            username: entry("Username"),
            password: adw::PasswordEntryRow::builder().title("Password").build(),
            stream: adw::ComboRow::builder()
                .title("Stream")
                .model(&gtk::StringList::new(&["Main · HD", "Sub · low bandwidth"]))
                .build(),
            https: switch("Use HTTPS", "Required by recent firmware"),
            preview: switch("Show in panel preview", "Appears when you click the top-bar icon"),
            alerts: switch("Desktop alerts", "Notify on detections from this camera"),
            status: adw::ActionRow::builder().title("Connection").build(),
            status_icon: gtk::Image::new(),
            spinner: gtk::Spinner::new(),
            test: gtk::Button::with_label("Test Connection"),
            web: adw::ActionRow::builder().title("Open Web Interface").activatable(true).build(),
            remove_group: adw::PreferencesGroup::new(),
            remove: gtk::Button::with_label("Remove Camera"),
            cancel: gtk::Button::with_label("Cancel"),
            save: gtk::Button::with_label("Save"),
            editing: RefCell::new(None),
            tested_ok: RefCell::new(None),
            test_run: Cell::new(0),
        };
        // Connection test: result row with an icon, spinner and the button,
        // right below the fields it checks.
        editor.status.set_subtitle_lines(3);
        editor.status.add_prefix(&editor.status_icon);
        editor.status.add_suffix(&editor.spinner);
        editor.test.set_valign(gtk::Align::Center);
        editor.status.add_suffix(&editor.test);
        for row in [
            editor.name.upcast_ref::<gtk::Widget>(),
            editor.host.upcast_ref(),
            editor.username.upcast_ref(),
            editor.password.upcast_ref(),
            editor.https.upcast_ref(),
            editor.status.upcast_ref(),
        ] {
            editor.group.add(row);
        }
        let options = adw::PreferencesGroup::builder().title("Options").build();
        options.add(&editor.stream);
        options.add(&editor.preview);
        options.add(&editor.alerts);

        editor.web.add_suffix(&gtk::Image::from_icon_name("web-browser-symbolic"));
        editor.remove_group.add(&editor.web);
        editor.remove.add_css_class("destructive-action");
        editor.remove.add_css_class("pill");
        editor.remove.set_halign(gtk::Align::Center);
        editor.remove.set_margin_top(12);
        editor.remove_group.add(&editor.remove);

        let page = adw::PreferencesPage::new();
        page.set_vexpand(true);
        page.add(&editor.group);
        page.add(&options);
        page.add(&editor.remove_group);

        editor.save.add_css_class("pill");
        editor.save.add_css_class("suggested-action");
        editor.cancel.add_css_class("pill");
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.add_css_class("actions");
        actions.set_halign(gtk::Align::End);
        actions.append(&editor.cancel);
        actions.append(&editor.save);

        editor.page.append(&page);
        editor.page.append(&actions);
        editor
    }

    fn load(&self, cam: Option<&CameraConfig>, first_run: bool) {
        let defaults = CameraConfig::default();
        let c = cam.unwrap_or(&defaults);
        self.group.set_title(if cam.is_some() { "Edit Camera" } else { "Add Camera" });
        self.group.set_description(first_run.then_some(
            "Enter your camera's address and login, then test the connection. They are saved on this computer only.",
        ));
        self.host.remove_css_class("error");
        self.name.set_text(if cam.is_some() { &c.name } else { "" });
        self.host.set_text(&c.host);
        self.username.set_text(&c.username);
        self.password.set_text(&c.password);
        self.stream.set_selected(match c.stream {
            StreamQuality::Main => 0,
            StreamQuality::Sub => 1,
        });
        self.https.set_active(c.use_https);
        self.preview.set_active(c.preview);
        self.alerts.set_active(c.alerts);
        self.remove_group.set_visible(cam.is_some());
        self.cancel.set_visible(!first_run);
        self.save.set_label(if cam.is_some() { "Save" } else { "Add Camera" });
        self.reset_status();
    }

    fn reset_status(&self) {
        self.test_run.set(self.test_run.get() + 1);
        self.tested_ok.replace(None);
        self.show_status(Status::Untested, "Not tested yet");
    }

    fn show_status(&self, status: Status, text: &str) {
        self.status.set_subtitle(&glib::markup_escape_text(text));
        self.spinner.set_visible(status == Status::Testing);
        self.spinner.set_spinning(status == Status::Testing);
        self.test.set_sensitive(status != Status::Testing);
        self.save.set_sensitive(status != Status::Testing);
        let (icon, class) = match status {
            Status::Untested | Status::Testing => ("network-wired-symbolic", None),
            Status::Ok => ("emblem-ok-symbolic", Some("success")),
            Status::Failed => ("dialog-warning-symbolic", Some("error")),
        };
        self.status_icon.set_icon_name(Some(icon));
        for c in ["success", "error"] {
            self.status_icon.remove_css_class(c);
        }
        if let Some(c) = class {
            self.status_icon.add_css_class(c);
        }
    }

    fn store(&self, mut c: CameraConfig) -> Result<CameraConfig, String> {
        let host = self.host.text().trim().to_owned();
        if host.is_empty() || host.contains(['/', ' ', '@']) {
            self.host.add_css_class("error");
            return Err("Enter the camera's IP address or hostname".into());
        }
        self.host.remove_css_class("error");
        let name = self.name.text().trim().to_owned();
        c.name = if name.is_empty() { host.clone() } else { name };
        c.host = host;
        c.username = self.username.text().trim().to_owned();
        c.password = self.password.text().to_string();
        c.stream = if self.stream.selected() == 0 { StreamQuality::Main } else { StreamQuality::Sub };
        c.use_https = self.https.is_active();
        c.preview = self.preview.is_active();
        c.alerts = self.alerts.is_active();
        Ok(c)
    }
}

/// What the connection test checks: API login, device info, RTSP port.
fn probe_camera(cam: &CameraConfig) -> Result<DeviceInfo> {
    let mut client = Client::new(cam)?;
    if let Err(e) = client.login() {
        // A camera that answered but refused the login vs. no answer at all.
        return Err(match e.downcast_ref::<ApiError>() {
            Some(api) => anyhow::anyhow!("Wrong username or password (camera says: {})", api.detail),
            None => anyhow::anyhow!("Cannot reach the camera at {} — check the address and that it is on", cam.host),
        });
    }
    let info = client.device_info().unwrap_or_default();
    client.logout();

    let addr = (cam.host_only(), cam.rtsp_port)
        .to_socket_addrs()
        .context("Cannot resolve the camera address")?
        .next()
        .context("Cannot resolve the camera address")?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(3)).with_context(|| {
        format!("Logged in, but the video port {} is closed — enable RTSP on the camera", cam.rtsp_port)
    })?;
    Ok(info)
}

/// Fields that decide whether a connection test result still applies.
fn connection_key(cam: &CameraConfig) -> String {
    format!("{}|{}|{}|{}", cam.host, cam.use_https, cam.username, cam.password)
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("flat");
    b.set_valign(gtk::Align::Center);
    b
}

fn with(weak: &Weak<Inner>, f: impl FnOnce(&Rc<Inner>)) {
    if let Some(inner) = weak.upgrade() {
        f(&inner);
    }
}
