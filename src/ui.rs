//! Main window (libadwaita, so it follows the user's light/dark preference and
//! accent colour): live view, recordings and settings. Closing only hides it.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::config::Config;
use crate::live::LiveView;
use crate::recordings::Recordings;
use crate::settings::SettingsView;
use crate::{AppEvent, CamState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Live,
    Recordings,
    Settings,
}

/// The few styles libadwaita does not provide; colours come from the theme.
pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub struct MainWindow(Rc<Inner>);

struct Inner {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    back: gtk::Button,
    stack: gtk::Stack,
    recordings_btn: gtk::ToggleButton,
    settings_btn: gtk::ToggleButton,
    live: LiveView,
    recordings: Recordings,
    settings: SettingsView,
    /// Set while toggle buttons are updated programmatically.
    syncing: Cell<bool>,
    config: Arc<RwLock<Config>>,
}

impl MainWindow {
    pub fn new(
        app: &adw::Application,
        config: Arc<RwLock<Config>>,
        generation: Arc<AtomicU64>,
        tx: async_channel::Sender<AppEvent>,
    ) -> Self {
        let title = adw::WindowTitle::new("Reolink", "");
        let back = icon_button("go-previous-symbolic", "All cameras");
        let recordings_btn = toggle_button("document-open-recent-symbolic", "Recordings");
        let settings_btn = toggle_button("emblem-system-symbolic", "Settings");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));
        header.pack_start(&back);
        header.pack_end(&settings_btn);
        header.pack_end(&recordings_btn);

        let toasts = adw::ToastOverlay::new();
        let live = LiveView::new(config.clone(), toasts.clone());
        let recordings = Recordings::new(config.clone(), toasts.clone());
        let settings = SettingsView::new(config.clone(), generation, tx, toasts.clone());

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(150)
            .build();
        stack.add_named(live.widget(), Some("live"));
        stack.add_named(recordings.widget(), Some("recordings"));
        stack.add_named(settings.widget(), Some("settings"));
        toasts.set_child(Some(&stack));

        let content = adw::ToolbarView::new();
        content.add_top_bar(&header);
        content.set_content(Some(&toasts));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Reolink")
            .default_width(960)
            .default_height(640)
            .content(&content)
            .build();

        let inner = Rc::new(Inner {
            window,
            title,
            back,
            stack,
            recordings_btn,
            settings_btn,
            live,
            recordings,
            settings,
            syncing: Cell::new(false),
            config,
        });

        for (button, page) in [(&inner.settings_btn, Page::Settings), (&inner.recordings_btn, Page::Recordings)] {
            let w = Rc::downgrade(&inner);
            button.connect_toggled(move |b| {
                with(&w, |i| {
                    if !i.syncing.get() {
                        i.set_page(if b.is_active() { page } else { Page::Live });
                    }
                })
            });
        }
        let w = Rc::downgrade(&inner);
        inner.back.connect_clicked(move |_| with(&w, |i| i.live.focus(None)));
        let w = Rc::downgrade(&inner);
        inner.live.connect_layout_changed(move || with(&w, |i| i.update_header()));
        let w = Rc::downgrade(&inner);
        inner.window.connect_close_request(move |_| {
            with(&w, |i| i.hide());
            glib::Propagation::Stop
        });

        inner.update_header();
        MainWindow(inner)
    }

    pub fn toggle(&self) {
        if self.0.window.is_visible() {
            self.0.hide();
        } else {
            self.show(Page::Live);
        }
    }

    pub fn show(&self, page: Page) {
        let configured = self.0.config.read().unwrap().is_configured();
        self.0.window.present();
        self.0.set_page(if configured { page } else { Page::Settings });
    }

    pub fn show_camera(&self, id: &str) {
        let several = self.0.config.read().unwrap().cameras.len() > 1;
        self.0.live.focus(several.then(|| id.to_owned()));
        self.show(Page::Live);
    }

    pub fn set_camera_state(&self, id: &str, state: CamState) {
        self.0.live.set_state(id, state);
    }

    /// Settings were saved: rebuild everything that depends on the camera list.
    pub fn config_changed(&self) {
        let i = &self.0;
        i.live.rebuild();
        i.recordings.reload_cameras();
        if i.page() != Page::Settings {
            i.settings.reload();
        }
        i.sync_activity();
        i.update_header();
    }

    pub fn shutdown(&self) {
        self.0.recordings.deactivate();
        self.0.live.shutdown();
        self.0.window.destroy();
    }
}

impl Inner {
    fn page(&self) -> Page {
        match self.stack.visible_child_name().as_deref() {
            Some("settings") => Page::Settings,
            Some("recordings") => Page::Recordings,
            _ => Page::Live,
        }
    }

    fn set_page(&self, page: Page) {
        if page == Page::Settings && self.page() != Page::Settings {
            self.settings.reload();
        }
        self.stack.set_visible_child_name(match page {
            Page::Live => "live",
            Page::Recordings => "recordings",
            Page::Settings => "settings",
        });
        self.set_toggle(&self.settings_btn, page == Page::Settings);
        self.set_toggle(&self.recordings_btn, page == Page::Recordings);
        self.sync_activity();
        self.update_header();
    }

    /// Only the visible page streams.
    fn sync_activity(&self) {
        let visible = self.window.is_visible();
        let page = self.page();
        self.live.set_active(visible && page == Page::Live);
        if visible && page == Page::Recordings {
            self.recordings.activate();
        } else {
            self.recordings.deactivate();
        }
    }

    fn hide(&self) {
        self.window.set_visible(false);
        self.sync_activity();
    }

    fn update_header(&self) {
        let count = self.config.read().unwrap().cameras.len();
        let (title, subtitle) = match self.page() {
            Page::Recordings => ("Recordings".to_owned(), String::new()),
            Page::Settings => ("Settings".to_owned(), String::new()),
            Page::Live => match self.live.single_camera() {
                Some(cam) => (cam.name.clone(), cam.host.clone()),
                None if count == 0 => ("Reolink".to_owned(), "No cameras".to_owned()),
                None => ("Cameras".to_owned(), format!("{count} cameras")),
            },
        };
        self.title.set_title(&title);
        self.title.set_subtitle(&subtitle);
        self.window.set_title(Some(&title));
        self.back.set_visible(self.page() == Page::Live && self.live.is_focused());
    }

    fn set_toggle(&self, button: &gtk::ToggleButton, active: bool) {
        self.syncing.set(true);
        button.set_active(active);
        self.syncing.set(false);
    }
}

fn with(weak: &Weak<Inner>, f: impl FnOnce(&Rc<Inner>)) {
    if let Some(inner) = weak.upgrade() {
        f(&inner);
    }
}

pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("flat");
    b.set_valign(gtk::Align::Center);
    b
}

pub fn toggle_button(icon: &str, tooltip: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::new();
    b.set_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("flat");
    b.set_valign(gtk::Align::Center);
    b
}
