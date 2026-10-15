//! Video player: `playbin` → (`glsinkbin` →) `gtk4paintablesink`, rendered by a
//! `gtk::Picture`. Used for the RTSP live view and for HTTPS playback of SD-card
//! recordings. Hardware decoding (VA-API) is picked automatically when available.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use gst::prelude::*;
use gtk::{gdk, glib};

#[derive(Debug)]
pub enum PlayerEvent {
    Playing,
    Eos,
    Error(String),
}

#[derive(Default)]
struct Credentials {
    user: String,
    password: String,
}

pub struct Player {
    playbin: gst::Element,
    paintable: gdk::Paintable,
    credentials: Arc<Mutex<Credentials>>,
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Player {
    /// Must be called on the GTK main thread after GTK is initialised.
    /// `audio`: also play the sound track (recordings); the live view stays silent.
    pub fn new(audio: bool, on_event: impl Fn(PlayerEvent) + 'static) -> Result<Self> {
        let gtksink = gst::ElementFactory::make("gtk4paintablesink")
            .build()
            .context("gtk4paintablesink is unavailable")?;
        let paintable = gtksink.property::<gdk::Paintable>("paintable");

        // Zero-copy GL upload when GTK has a GL context, CPU conversion otherwise.
        let has_gl = paintable.find_property("gl-context").is_some()
            && paintable.property::<Option<gdk::GLContext>>("gl-context").is_some();
        let video_sink = if has_gl {
            gst::ElementFactory::make("glsinkbin").property("sink", &gtksink).build()?
        } else {
            let bin = gst::Bin::new();
            let convert = gst::ElementFactory::make("videoconvert").build()?;
            bin.add_many([&convert, &gtksink])?;
            convert.link(&gtksink)?;
            let pad = convert.static_pad("sink").context("videoconvert has no sink pad")?;
            bin.add_pad(&gst::GhostPad::with_target(&pad)?)?;
            bin.upcast()
        };
        log::info!("video output: {}", if has_gl { "OpenGL" } else { "software" });

        // Classic playbin: playbin3's parsebin trips over Reolink's RTSP H.264
        // ("h264parse: No caps set").
        let playbin = gst::ElementFactory::make("playbin")
            .property("video-sink", &video_sink)
            .build()
            .context("playbin is unavailable (install gstreamer1.0-plugins-base)")?;
        playbin.set_property_from_str(
            "flags",
            if audio { "video+audio+soft-volume+deinterlace" } else { "video+deinterlace" },
        );

        // Credentials are injected into rtspsrc instead of the URL so they never
        // appear in logs. Runs on a streaming thread, hence Arc<Mutex<_>>.
        let credentials = Arc::new(Mutex::new(Credentials::default()));
        let creds = credentials.clone();
        playbin.connect("source-setup", false, move |args| {
            let src = args.get(1)?.get::<gst::Element>().ok()?;
            if src.find_property("user-id").is_some() {
                let c = creds.lock().ok()?;
                src.set_property("user-id", &c.user);
                src.set_property("user-pw", &c.password);
            }
            if src.find_property("latency").is_some() {
                src.set_property("latency", 200u32);
            }
            if src.find_property("ssl-strict").is_some() {
                // HTTPS recordings: the camera's certificate is self-signed.
                src.set_property("ssl-strict", false);
            }
            if src.find_property("protocols").is_some() {
                // TCP interleaved: robust through NAT/Wi-Fi, no UDP packet loss artefacts.
                src.set_property_from_str("protocols", "tcp");
            }
            None
        });

        let bus = playbin.bus().context("playbin has no bus")?;
        let weak = playbin.downgrade();
        let bus_watch = bus.add_watch_local(move |_, msg| {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    log::warn!("stream error: {} ({:?})", err.error(), err.debug());
                    on_event(PlayerEvent::Error(err.error().to_string()));
                }
                MessageView::Eos(_) => on_event(PlayerEvent::Eos),
                MessageView::StateChanged(s) => {
                    let from_playbin = weak
                        .upgrade()
                        .is_some_and(|p| msg.src() == Some(p.upcast_ref::<gst::Object>()));
                    if from_playbin && s.current() == gst::State::Playing {
                        log::info!("stream playing");
                        on_event(PlayerEvent::Playing);
                    }
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        })?;

        Ok(Self { playbin, paintable, credentials, _bus_watch: bus_watch })
    }

    pub fn paintable(&self) -> &gdk::Paintable {
        &self.paintable
    }

    pub fn play(&self, uri: &str, user: &str, password: &str) {
        self.stop();
        if let Ok(mut c) = self.credentials.lock() {
            c.user = user.to_owned();
            c.password = password.to_owned();
        }
        self.playbin.set_property("uri", uri);
        if let Err(e) = self.playbin.set_state(gst::State::Playing) {
            log::warn!("failed to start stream: {e}");
        }
    }

    pub fn set_paused(&self, paused: bool) {
        let state = if paused { gst::State::Paused } else { gst::State::Playing };
        let _ = self.playbin.set_state(state);
    }

    /// (position, duration) in seconds, once known.
    pub fn progress(&self) -> Option<(f64, f64)> {
        let pos = self.playbin.query_position::<gst::ClockTime>()?;
        let dur = self.playbin.query_duration::<gst::ClockTime>()?;
        Some((pos.nseconds() as f64 / 1e9, dur.nseconds() as f64 / 1e9))
    }

    pub fn seek(&self, secs: f64) {
        let target = gst::ClockTime::from_nseconds((secs.max(0.0) * 1e9) as u64);
        if let Err(e) = self.playbin.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target) {
            log::debug!("seek failed: {e}");
        }
    }

    pub fn stop(&self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}
