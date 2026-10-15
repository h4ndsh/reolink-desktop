//! Recordings page: browse the SD card by day and play clips over HTTPS
//! (`cmd=Download` supports range requests, so seeking works without
//! downloading the whole file).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, RwLock};

use adw::prelude::*;
use anyhow::{anyhow, Result};
use gtk::glib;

use crate::config::{CameraConfig, Config};
use crate::player::{Player, PlayerEvent};
use crate::reolink::{Client, Recording};

/// API session kept open while the page is shown (clip URLs carry its
/// token), tagged with the camera settings it was opened for.
type Session = Arc<Mutex<Option<(String, Client)>>>;

pub struct Recordings(Rc<Inner>);

struct Inner {
    widget: gtk::Box,
    calendar: gtk::Calendar,
    date_button: gtk::MenuButton,
    picture: gtk::Picture,
    status: gtk::Label,
    play_button: gtk::Button,
    scale: gtk::Scale,
    time_label: gtk::Label,
    save_button: gtk::Button,
    list: gtk::ListBox,
    toasts: adw::ToastOverlay,
    player: RefCell<Option<Player>>,
    clips: RefCell<Vec<Recording>>,
    current: Cell<Option<usize>>,
    paused: Cell<bool>,
    active: Cell<bool>,
    /// Bumped on every day change so late search results are dropped.
    generation: Cell<u64>,
    ticker: RefCell<Option<glib::SourceId>>,
    session: Session,
    camera_select: gtk::DropDown,
    camera_ids: RefCell<Vec<String>>,
    /// Set while the camera list is rebuilt (suppresses the change handler).
    updating: Cell<bool>,
    config: Arc<RwLock<Config>>,
}

impl Recordings {
    pub fn new(config: Arc<RwLock<Config>>, toasts: adw::ToastOverlay) -> Self {
        // ── Day picker ────────────────────────────────────────────
        let calendar = gtk::Calendar::new();
        let popover = gtk::Popover::new();
        popover.set_child(Some(&calendar));
        let date_button = gtk::MenuButton::builder().popover(&popover).label("Today").build();
        date_button.add_css_class("flat");
        let prev_day = icon_button("go-previous-symbolic", "Previous day");
        let next_day = icon_button("go-next-symbolic", "Next day");
        let camera_select = gtk::DropDown::from_strings(&[]);
        camera_select.set_tooltip_text(Some("Camera"));
        camera_select.set_margin_end(8);
        let day_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        day_bar.add_css_class("day-bar");
        day_bar.set_halign(gtk::Align::Center);
        day_bar.append(&camera_select);
        day_bar.append(&prev_day);
        day_bar.append(&date_button);
        day_bar.append(&next_day);

        // ── Video ─────────────────────────────────────────────────
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_size_request(448, 252);
        let status = gtk::Label::new(Some("Select a recording"));
        status.add_css_class("video-status");
        status.set_halign(gtk::Align::Center);
        status.set_valign(gtk::Align::Center);
        let video = gtk::Overlay::new();
        video.add_css_class("video-frame");
        video.set_overflow(gtk::Overflow::Hidden);
        video.set_child(Some(&picture));
        video.add_overlay(&status);

        let play_button = icon_button("media-playback-start-symbolic", "Play / pause");
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        let time_label = gtk::Label::new(Some("0:00 / 0:00"));
        time_label.add_css_class("numeric");
        time_label.add_css_class("caption");
        let save_button = icon_button("document-save-symbolic", "Save clip to Videos");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        controls.add_css_class("controls");
        controls.append(&play_button);
        controls.append(&scale);
        controls.append(&time_label);
        controls.append(&save_button);

        // ── Clip list ─────────────────────────────────────────────
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        let empty = gtk::Label::new(Some("No recordings on this day"));
        empty.add_css_class("dim-label");
        empty.set_margin_top(12);
        empty.set_margin_bottom(12);
        list.set_placeholder(Some(&empty));
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(180)
            .max_content_height(180)
            .child(&list)
            .build();
        scroller.add_css_class("clips");

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("live");
        widget.append(&day_bar);
        widget.append(&video);
        widget.append(&controls);
        widget.append(&scroller);

        let inner = Rc::new(Inner {
            widget,
            calendar,
            date_button,
            picture,
            status,
            play_button,
            scale,
            time_label,
            save_button,
            list,
            toasts,
            player: RefCell::new(None),
            clips: RefCell::new(Vec::new()),
            current: Cell::new(None),
            paused: Cell::new(false),
            active: Cell::new(false),
            generation: Cell::new(0),
            ticker: RefCell::new(None),
            session: Arc::new(Mutex::new(None)),
            camera_select,
            camera_ids: RefCell::default(),
            updating: Cell::new(false),
            config,
        });

        let weak = Rc::downgrade(&inner);
        match Player::new(true, move |ev| with(&weak, |i| i.on_player_event(ev))) {
            Ok(player) => {
                inner.picture.set_paintable(Some(player.paintable()));
                inner.player.replace(Some(player));
            }
            Err(e) => log::error!("playback disabled: {e:#}"),
        }

        // ── Signals ───────────────────────────────────────────────
        let w = Rc::downgrade(&inner);
        inner.camera_select.connect_selected_notify(move |_| {
            with(&w, |i| {
                if !i.updating.get() {
                    if i.active.get() {
                        i.load_day();
                        i.load_marks();
                    }
                }
            })
        });
        let w = Rc::downgrade(&inner);
        inner.calendar.connect_day_selected(move |_| {
            with(&w, |i| {
                if let Some(p) = i.date_button.popover() {
                    p.popdown();
                }
                i.load_day();
            })
        });
        let w = Rc::downgrade(&inner);
        let marks = move |_: &gtk::Calendar| with(&w, |i| i.load_marks());
        inner.calendar.connect_next_month(marks.clone());
        inner.calendar.connect_prev_month(marks.clone());
        inner.calendar.connect_next_year(marks.clone());
        inner.calendar.connect_prev_year(marks);
        for (button, days) in [(&prev_day, -1), (&next_day, 1)] {
            let w = Rc::downgrade(&inner);
            button.connect_clicked(move |_| {
                with(&w, |i| {
                    if let Ok(date) = i.calendar.date().add_days(days) {
                        i.calendar.select_day(&date);
                    }
                })
            });
        }
        let w = Rc::downgrade(&inner);
        inner.list.connect_row_activated(move |_, row| {
            with(&w, |i| i.play(row.index() as usize));
        });
        let w = Rc::downgrade(&inner);
        inner.play_button.connect_clicked(move |_| with(&w, |i| i.toggle_pause()));
        let w = Rc::downgrade(&inner);
        // change-value only fires on user interaction, not on set_value().
        inner.scale.connect_change_value(move |_, _, value| {
            with(&w, |i| {
                if let Some(p) = &*i.player.borrow() {
                    p.seek(value);
                }
            });
            glib::Propagation::Proceed
        });
        let w = Rc::downgrade(&inner);
        inner.save_button.connect_clicked(move |_| with(&w, |i| i.save_clip()));

        inner.update_controls();
        let view = Recordings(inner);
        view.reload_cameras();
        view
    }

    /// Rebuilds the camera selector after the camera list changed.
    pub fn reload_cameras(&self) {
        let i = &self.0;
        let cameras = i.config.read().unwrap().cameras.clone();
        let previous = i.selected_id();
        i.updating.set(true);
        let names: Vec<&str> = cameras.iter().map(|c| c.name.as_str()).collect();
        i.camera_select.set_model(Some(&gtk::StringList::new(&names)));
        let index = previous.and_then(|id| cameras.iter().position(|c| c.id == id)).unwrap_or(0);
        i.camera_select.set_selected(index as u32);
        *i.camera_ids.borrow_mut() = cameras.into_iter().map(|c| c.id).collect();
        i.camera_select.set_visible(i.camera_ids.borrow().len() > 1);
        i.updating.set(false);
        if i.active.get() {
            i.load_day();
            i.load_marks();
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.0.widget
    }

    /// Called when the page becomes visible: (re)loads the selected day.
    pub fn activate(&self) {
        let i = &self.0;
        if i.active.replace(true) {
            return;
        }
        i.load_day();
        i.load_marks();
        let weak = Rc::downgrade(i);
        let id = glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            with(&weak, |i| i.tick());
            glib::ControlFlow::Continue
        });
        i.ticker.replace(Some(id));
    }

    /// Called when the page is left or the window hidden: stops playback and
    /// closes the API session.
    pub fn deactivate(&self) {
        let i = &self.0;
        if !i.active.replace(false) {
            return;
        }
        if let Some(id) = i.ticker.take() {
            id.remove();
        }
        if let Some(p) = &*i.player.borrow() {
            p.stop();
        }
        i.current.set(None);
        i.list.unselect_all();
        i.show_status("Select a recording");
        i.update_controls();
        i.end_session();
    }
}

impl Inner {
    fn selected_id(&self) -> Option<String> {
        self.camera_ids.borrow().get(self.camera_select.selected() as usize).cloned()
    }

    /// The selected camera (an unconfigured placeholder when there is none).
    fn camera(&self) -> CameraConfig {
        let cfg = self.config.read().unwrap();
        self.selected_id()
            .and_then(|id| cfg.camera(&id).cloned())
            .or_else(|| cfg.cameras.first().cloned())
            .unwrap_or_default()
    }

    fn stream_type(&self) -> &'static str {
        self.camera().stream.as_str()
    }

    /// Logs out of the API session in the background (clip URLs die with it).
    fn end_session(&self) {
        let session = self.session.clone();
        std::thread::spawn(move || {
            if let Some((_, mut client)) = session.lock().ok().and_then(|mut s| s.take()) {
                client.logout();
            }
        });
    }

    fn load_day(self: &Rc<Self>) {
        let date = self.calendar.date();
        self.date_button.set_label(&day_label(&date));
        let (y, m, d) = (date.year(), date.month() as u32, date.day_of_month() as u32);
        if !self.camera().is_configured() {
            self.show_status("No cameras configured");
            return;
        }
        let stream = self.stream_type();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        if let Some(p) = &*self.player.borrow() {
            p.stop();
        }
        self.current.set(None);
        self.clips.borrow_mut().clear();
        self.list.remove_all();
        self.list.set_visible(false);
        self.show_status("Loading…");
        self.update_controls();

        let weak = Rc::downgrade(self);
        session_task(&self.session, self.camera(), move |c| c.recordings(y, m, d, stream), move |res| {
            with(&weak, |i| {
                if i.generation.get() != generation || !i.active.get() {
                    return;
                }
                match res {
                    Ok(clips) => i.set_clips(clips),
                    Err(e) => {
                        log::warn!("recordings: {e:#}");
                        i.show_status("Could not load recordings");
                    }
                }
            })
        });
    }

    fn load_marks(self: &Rc<Self>) {
        let year: i32 = self.calendar.property("year");
        let month = self.calendar.property::<i32>("month") as u32 + 1; // 0-based
        if !self.camera().is_configured() {
            return;
        }
        let stream = self.stream_type();
        let weak = Rc::downgrade(self);
        session_task(&self.session, self.camera(), move |c| c.recording_days(year, month, stream), move |res| {
            with(&weak, |i| {
                // Ignore results for a month that is no longer displayed.
                let shown = (i.calendar.property::<i32>("year"), i.calendar.property::<i32>("month") as u32 + 1);
                if shown != (year, month) {
                    return;
                }
                i.calendar.clear_marks();
                match res {
                    Ok(days) => days.into_iter().for_each(|d| i.calendar.mark_day(d)),
                    Err(e) => log::debug!("recording days: {e:#}"),
                }
            })
        });
    }

    fn set_clips(&self, clips: Vec<Recording>) {
        log::info!("{} recordings on {}", clips.len(), self.date_button.label().unwrap_or_default());
        for clip in &clips {
            let row = adw::ActionRow::builder()
                .title(format!("{} – {}", hms(clip.start), hms(clip.end)))
                .subtitle(format!("{} · {:.1} MB", duration(clip.duration_secs()), clip.size as f64 / 1e6))
                .activatable(true)
                .build();
            self.list.append(&row);
        }
        // An empty boxed list is just a grey box; the status label says it all.
        self.list.set_visible(!clips.is_empty());
        self.show_status(if clips.is_empty() { "No recordings on this day" } else { "Select a recording" });
        *self.clips.borrow_mut() = clips;
    }

    fn play(self: &Rc<Self>, index: usize) {
        let Some(clip) = self.clips.borrow().get(index).cloned() else { return };
        self.current.set(Some(index));
        self.paused.set(false);
        self.list.select_row(self.list.row_at_index(index as i32).as_ref());
        self.show_status("Loading…");
        self.update_controls();

        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        session_task(&self.session, self.camera(), move |c| c.download_url(&clip.name), move |res| {
            with(&weak, |i| {
                if i.generation.get() != generation || i.current.get() != Some(index) || !i.active.get() {
                    return;
                }
                match (res, &*i.player.borrow()) {
                    (Ok(url), Some(player)) => player.play(&url, "", ""),
                    (Err(e), _) => {
                        log::warn!("playback: {e:#}");
                        i.show_status("Could not open this recording");
                    }
                    _ => {}
                }
            })
        });
    }

    fn toggle_pause(self: &Rc<Self>) {
        match self.current.get() {
            None if !self.clips.borrow().is_empty() => self.play(0),
            None => {}
            Some(_) => {
                let paused = !self.paused.get();
                self.paused.set(paused);
                if let Some(p) = &*self.player.borrow() {
                    p.set_paused(paused);
                }
                self.update_controls();
            }
        }
    }

    fn on_player_event(self: &Rc<Self>, ev: PlayerEvent) {
        if self.current.get().is_none() {
            return;
        }
        match ev {
            PlayerEvent::Playing => self.status.set_visible(false),
            // Clips are listed newest first: continue with the next one in time.
            PlayerEvent::Eos => match self.current.get() {
                Some(i) if i > 0 => self.play(i - 1),
                _ => {
                    self.paused.set(true);
                    self.update_controls();
                }
            },
            PlayerEvent::Error(msg) => {
                log::warn!("playback error: {msg}");
                self.show_status("Could not play this recording");
            }
        }
    }

    fn tick(&self) {
        if self.current.get().is_none() {
            return;
        }
        if let Some((pos, dur)) = self.player.borrow().as_ref().and_then(Player::progress) {
            if dur > 0.0 {
                self.scale.set_range(0.0, dur);
                self.scale.set_value(pos);
                self.time_label.set_label(&format!("{} / {}", duration(pos as u32), duration(dur as u32)));
            }
        }
    }

    fn save_clip(self: &Rc<Self>) {
        let Some(clip) = self.current.get().and_then(|i| self.clips.borrow().get(i).cloned()) else { return };
        let dir = glib::user_special_dir(glib::UserDirectory::Videos)
            .unwrap_or_else(|| glib::home_dir().join("Videos"))
            .join("Reolink");
        let path = dir.join(clip.file_name());
        self.toasts.add_toast(adw::Toast::new("Downloading clip…"));
        let weak = Rc::downgrade(self);
        session_task(
            &self.session,
            self.camera(),
            move |c| {
                let data = c.download(&clip.name)?;
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&path, data)?;
                Ok(path)
            },
            move |res| {
                with(&weak, |i| {
                    let msg = match res {
                        Ok(path) => format!("Saved to {}", path.display()),
                        Err(e) => {
                            log::warn!("save clip: {e:#}");
                            "Could not save the clip".into()
                        }
                    };
                    i.toasts.add_toast(adw::Toast::new(&msg));
                })
            },
        );
    }

    fn update_controls(&self) {
        let has_clip = self.current.get().is_some();
        self.save_button.set_sensitive(has_clip);
        self.scale.set_sensitive(has_clip);
        let playing = has_clip && !self.paused.get();
        self.play_button.set_icon_name(if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
        if !has_clip {
            self.scale.set_value(0.0);
            self.time_label.set_label("0:00 / 0:00");
        }
    }

    fn show_status(&self, text: &str) {
        self.status.set_label(text);
        self.status.set_visible(true);
    }
}

/// Runs a blocking API job on a worker thread using the page's shared session
/// (created on demand) and delivers the result on the GTK thread.
fn session_task<T: Send + 'static>(
    session: &Session,
    cam: CameraConfig,
    job: impl FnOnce(&mut Client) -> Result<T> + Send + 'static,
    done: impl FnOnce(Result<T>) + 'static,
) {
    let session = session.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let res = (|| -> Result<T> {
            let key = format!("{}|{}|{}|{}|{}", cam.id, cam.host, cam.use_https, cam.username, cam.password);
            let mut guard = session.lock().map_err(|_| anyhow!("session lock poisoned"))?;
            if guard.as_ref().is_some_and(|(k, _)| *k != key) {
                // Another camera (or changed settings): close the old session.
                if let Some((_, mut old)) = guard.take() {
                    old.logout();
                }
            }
            if guard.is_none() {
                let mut client = Client::new(&cam)?;
                client.login()?;
                *guard = Some((key, client));
            }
            let res = job(&mut guard.as_mut().expect("session just created").1);
            if res.is_err() {
                *guard = None; // start fresh next time
            }
            res
        })();
        let _ = tx.send_blocking(res);
    });
    glib::spawn_future_local(async move {
        if let Ok(res) = rx.recv().await {
            done(res);
        }
    });
}

fn day_label(date: &glib::DateTime) -> String {
    let ymd = |d: &glib::DateTime| (d.year(), d.day_of_year());
    let today = glib::DateTime::now_local().ok();
    let yesterday = today.as_ref().and_then(|t| t.add_days(-1).ok());
    if today.as_ref().map(ymd) == Some(ymd(date)) {
        "Today".into()
    } else if yesterday.as_ref().map(ymd) == Some(ymd(date)) {
        "Yesterday".into()
    } else {
        date.format("%a, %d %b %Y").map(|s| s.to_string()).unwrap_or_default()
    }
}

fn hms((h, m, s): (u32, u32, u32)) -> String {
    format!("{h:02}:{m:02}:{s:02}")
}

fn duration(secs: u32) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn with(weak: &Weak<Inner>, f: impl FnOnce(&Rc<Inner>)) {
    if let Some(inner) = weak.upgrade() {
        f(&inner);
    }
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("flat");
    b.set_valign(gtk::Align::Center);
    b
}
