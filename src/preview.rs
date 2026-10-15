//! Low-rate JPEG previews for the GNOME Shell panel menu.
//!
//! While the menu is open the extension calls the `preview-keepalive` action
//! every couple of seconds; for each camera marked "show in preview" we decode
//! the sub stream and write `$XDG_RUNTIME_DIR/reolink-desktop/preview/<id>.jpg`
//! (atomically, via rename) a few times per second. Without keepalives the
//! pipelines stop after `IDLE_TIMEOUT`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gst::prelude::*;
use gtk::glib;

use crate::config::{self, CameraConfig, Config, StreamQuality};

const IDLE_TIMEOUT: Duration = Duration::from_secs(6);
const FPS: u32 = 3;

struct Running {
    pipeline: gst::Pipeline,
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub struct Preview(Rc<Inner>);

struct Inner {
    config: Arc<RwLock<Config>>,
    running: RefCell<HashMap<String, Running>>,
    last_keepalive: Cell<Option<Instant>>,
    timer: RefCell<Option<glib::SourceId>>,
}

impl Preview {
    pub fn new(config: Arc<RwLock<Config>>) -> Self {
        Preview(Rc::new(Inner {
            config,
            running: RefCell::default(),
            last_keepalive: Cell::new(None),
            timer: RefCell::default(),
        }))
    }

    /// Starts (or keeps running) the previews.
    pub fn keepalive(&self) {
        let inner = &self.0;
        inner.last_keepalive.set(Some(Instant::now()));
        inner.ensure_running();
        if inner.timer.borrow().is_none() {
            let weak = Rc::downgrade(inner);
            let id = glib::timeout_add_seconds_local(2, move || {
                let Some(inner) = weak.upgrade() else { return glib::ControlFlow::Break };
                let idle = inner.last_keepalive.get().is_none_or(|t| t.elapsed() > IDLE_TIMEOUT);
                if idle {
                    log::debug!("preview idle; stopping");
                    inner.running.borrow_mut().clear();
                    inner.timer.take();
                    return glib::ControlFlow::Break;
                }
                // Restart pipelines that failed (camera offline, network drop).
                inner.ensure_running();
                glib::ControlFlow::Continue
            });
            inner.timer.replace(Some(id));
        }
    }

    /// Settings changed: restart with the new camera list.
    pub fn reload(&self) {
        self.0.running.borrow_mut().clear();
        if self.0.timer.borrow().is_some() {
            self.0.ensure_running();
        }
    }
}

impl Inner {
    fn ensure_running(self: &Rc<Self>) {
        let cameras: Vec<CameraConfig> =
            self.config.read().unwrap().cameras.iter().filter(|c| c.preview).cloned().collect();
        let mut running = self.running.borrow_mut();
        running.retain(|id, _| cameras.iter().any(|c| &c.id == id));
        for cam in cameras {
            if running.contains_key(&cam.id) {
                continue;
            }
            match start(&cam, Rc::downgrade(self)) {
                Ok(r) => {
                    log::debug!("preview started for {}", cam.name);
                    running.insert(cam.id.clone(), r);
                }
                Err(e) => log::warn!("preview for {}: {e:#}", cam.name),
            }
        }
    }
}

fn start(cam: &CameraConfig, owner: Weak<Inner>) -> Result<Running> {
    let dir = config::preview_dir();
    fs::create_dir_all(&dir)?;
    let target = dir.join(format!("{}.jpg", cam.id));
    let tmp: PathBuf = dir.join(format!(".{}.jpg.tmp", cam.id));

    let desc = format!(
        "rtspsrc name=src latency=200 protocols=tcp ! decodebin ! videoconvert ! videoscale ! \
         videorate drop-only=true ! \
         video/x-raw,format=I420,width=640,pixel-aspect-ratio=1/1,framerate={FPS}/1 ! \
         jpegenc quality=75 ! appsink name=sink max-buffers=1 drop=true sync=false"
    );
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;

    let src = pipeline.by_name("src").context("rtspsrc missing")?;
    src.set_property("location", cam.rtsp_url_for(StreamQuality::Sub));
    src.set_property("user-id", &cam.username);
    src.set_property("user-pw", &cam.password);

    let sink = pipeline
        .by_name("sink")
        .context("appsink missing")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("sink is not an appsink"))?;
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                // Write-then-rename so the shell never reads a half-written file.
                if fs::write(&tmp, map.as_slice()).and_then(|_| fs::rename(&tmp, &target)).is_err() {
                    return Err(gst::FlowError::Error);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let id = cam.id.clone();
    let bus = pipeline.bus().context("pipeline has no bus")?;
    let bus_watch = bus.add_watch_local(move |_, msg| {
        if let gst::MessageView::Error(err) = msg.view() {
            log::warn!("preview {id}: {}", err.error());
            // Drop it outside this callback; the keepalive timer restarts it.
            let (owner, id) = (owner.clone(), id.clone());
            glib::idle_add_local_once(move || {
                if let Some(inner) = owner.upgrade() {
                    inner.running.borrow_mut().remove(&id);
                }
            });
        }
        glib::ControlFlow::Continue
    })?;

    pipeline.set_state(gst::State::Playing)?;
    Ok(Running { pipeline, _bus_watch: bus_watch })
}
