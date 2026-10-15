//! Background worker: polls the camera's motion/AI state and raises desktop
//! notifications on rising edges. Runs on its own OS thread (blocking I/O),
//! reporting connection state to the GTK thread through `AppEvent`s.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::config::{self, CameraConfig, Config, NotifyConfig};
use crate::reolink::Client;
use crate::{AppEvent, CamState};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const KEEP_SNAPSHOTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Person,
    Vehicle,
    Animal,
    Motion,
}

impl Kind {
    /// Most specific first: when several fire together only the first AI kind
    /// and never the generic motion event is reported.
    const ALL: [Kind; 4] = [Kind::Person, Kind::Vehicle, Kind::Animal, Kind::Motion];

    fn title(self) -> &'static str {
        match self {
            Kind::Person => "Person detected",
            Kind::Vehicle => "Vehicle detected",
            Kind::Animal => "Animal detected",
            Kind::Motion => "Motion detected",
        }
    }

    fn wanted(self, n: &NotifyConfig) -> bool {
        match self {
            Kind::Person => n.person,
            Kind::Vehicle => n.vehicle,
            Kind::Animal => n.animal,
            Kind::Motion => n.motion,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub motion: bool,
    pub person: bool,
    pub vehicle: bool,
    pub animal: bool,
}

impl Detection {
    fn get(&self, kind: Kind) -> bool {
        match kind {
            Kind::Person => self.person,
            Kind::Vehicle => self.vehicle,
            Kind::Animal => self.animal,
            Kind::Motion => self.motion,
        }
    }

    pub fn any(&self) -> bool {
        self.motion || self.person || self.vehicle || self.animal
    }
}

/// Edge detector with a per-kind cooldown.
#[derive(Default)]
pub struct Tracker {
    prev: Detection,
    last_fired: HashMap<Kind, Instant>,
}

impl Tracker {
    /// Returns the kinds that should be notified for this sample.
    pub fn update(&mut self, det: Detection, prefs: &NotifyConfig, now: Instant) -> Vec<Kind> {
        let cooldown = Duration::from_secs(prefs.cooldown_secs);
        let mut fired = Vec::new();
        for kind in Kind::ALL {
            let rising = det.get(kind) && !self.prev.get(kind);
            let cooled = self.last_fired.get(&kind).is_none_or(|t| now.duration_since(*t) >= cooldown);
            if rising && kind.wanted(prefs) && cooled {
                self.last_fired.insert(kind, now);
                fired.push(kind);
            }
        }
        // A specific AI event makes the simultaneous generic motion event redundant.
        if fired.len() > 1 {
            fired.retain(|k| *k != Kind::Motion);
        }
        self.prev = det;
        fired
    }
}

/// Owns the per-camera worker threads.
pub struct Workers {
    pub config: Arc<RwLock<Config>>,
    /// Bumped whenever settings are saved; workers reload their camera.
    pub generation: Arc<AtomicU64>,
    pub paused: Arc<AtomicBool>,
    pub tx: async_channel::Sender<AppEvent>,
    running: Arc<Mutex<HashSet<String>>>,
}

impl Workers {
    pub fn new(
        config: Arc<RwLock<Config>>,
        generation: Arc<AtomicU64>,
        paused: Arc<AtomicBool>,
        tx: async_channel::Sender<AppEvent>,
    ) -> Self {
        Self { config, generation, paused, tx, running: Arc::default() }
    }

    /// Starts a worker for every camera that has none yet. Workers of removed
    /// cameras stop by themselves once the generation changes.
    pub fn sync(&self) {
        let ids: Vec<String> = self.config.read().unwrap().cameras.iter().map(|c| c.id.clone()).collect();
        let mut running = self.running.lock().unwrap();
        for id in ids {
            if running.insert(id.clone()) {
                let worker = Worker {
                    id: id.clone(),
                    config: self.config.clone(),
                    generation: self.generation.clone(),
                    paused: self.paused.clone(),
                    tx: self.tx.clone(),
                    running: self.running.clone(),
                    last_state: None,
                };
                thread::Builder::new()
                    .name(format!("events-{id}"))
                    .spawn(move || worker.run())
                    .expect("failed to spawn event worker");
            }
        }
    }
}

struct Worker {
    id: String,
    config: Arc<RwLock<Config>>,
    generation: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    tx: async_channel::Sender<AppEvent>,
    running: Arc<Mutex<HashSet<String>>>,
    last_state: Option<CamState>,
}

impl Worker {
    fn run(mut self) {
        let mut failures = 0u32;
        loop {
            let generation = self.generation.load(Ordering::SeqCst);
            let (camera, prefs) = {
                let config = self.config.read().expect("config lock poisoned");
                (config.camera(&self.id).cloned(), config.notifications.clone())
            };
            let Some(camera) = camera else {
                log::info!("camera {} removed; worker stops", self.id);
                self.running.lock().unwrap().remove(&self.id);
                return;
            };
            match self.session(generation, &camera, &prefs) {
                Ok(()) => {
                    failures = 0;
                    self.set_state(CamState::Connecting);
                }
                Err(e) => {
                    log::warn!("{}: camera session ended: {e:#}", camera.name);
                    self.set_state(CamState::Offline);
                    failures = failures.saturating_add(1);
                    let backoff = Duration::from_secs(1u64 << failures.min(5)).min(MAX_BACKOFF);
                    if self.sleep(backoff, generation) {
                        failures = 0;
                        self.set_state(CamState::Connecting);
                    }
                }
            }
        }
    }

    /// Polls until an error occurs (`Err`) or the settings change (`Ok`).
    fn session(&mut self, generation: u64, camera: &CameraConfig, prefs: &NotifyConfig) -> Result<()> {
        let mut client = Client::new(camera)?;
        client.login()?;
        let ai_supported = client.ai_state().map(|a| a.supported).unwrap_or(false);
        log::info!(
            "{}: connected; AI detection {}",
            camera.name,
            if ai_supported { "available" } else { "unavailable" }
        );

        let interval = Duration::from_millis(prefs.poll_interval_ms.max(250));
        let mut tracker = Tracker::default();
        loop {
            let ai = if ai_supported { client.ai_state()? } else { Default::default() };
            let det = Detection {
                motion: client.motion_state()?,
                person: ai.person,
                vehicle: ai.vehicle,
                animal: ai.animal,
            };
            self.set_state(if det.any() { CamState::Alert } else { CamState::Online });

            let fired = tracker.update(det, prefs, Instant::now());
            if prefs.enabled && camera.alerts && !self.paused.load(Ordering::Relaxed) {
                for kind in fired {
                    self.notify(&mut client, kind, camera, prefs);
                }
            }

            if self.sleep(interval, generation) {
                client.logout();
                return Ok(());
            }
        }
    }

    fn notify(&self, client: &mut Client, kind: Kind, camera: &CameraConfig, prefs: &NotifyConfig) {
        let image = prefs
            .snapshot
            .then(|| client.snapshot().and_then(save_snapshot))
            .and_then(|r| r.inspect_err(|e| log::warn!("snapshot: {e:#}")).ok());
        let time = gtk::glib::DateTime::now_local()
            .and_then(|d| d.format("%H:%M:%S"))
            .map(|s| s.to_string())
            .unwrap_or_default();

        let mut n = notify_rust::Notification::new();
        n.appname("Reolink Desktop")
            .summary(kind.title())
            .body(&format!("{} · {time}", camera.name))
            .icon("reolink-desktop")
            .action("default", "Open Live View")
            .timeout(notify_rust::Timeout::Milliseconds(8000));
        if let Some(path) = &image {
            n.image_path(&path.to_string_lossy());
        }
        match n.show() {
            Ok(handle) => {
                // Clicking the notification opens this camera. The handle blocks
                // until the notification closes, so wait on a throwaway thread.
                let tx = self.tx.clone();
                let id = self.id.clone();
                thread::spawn(move || {
                    handle.wait_for_action(|action| {
                        if action == "default" {
                            let _ = tx.send_blocking(AppEvent::ShowCamera(id));
                        }
                    })
                });
            }
            Err(e) => log::warn!("notification failed: {e}"),
        }
    }

    fn set_state(&mut self, state: CamState) {
        if self.last_state != Some(state) {
            self.last_state = Some(state);
            let _ = self.tx.send_blocking(AppEvent::CameraState(self.id.clone(), state));
        }
    }

    /// Sleeps for `dur`, returning early with `true` if the settings changed.
    fn sleep(&self, dur: Duration, generation: u64) -> bool {
        let deadline = Instant::now() + dur;
        loop {
            if self.generation.load(Ordering::SeqCst) != generation {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            thread::sleep(left.min(Duration::from_millis(200)));
        }
    }
}

/// Writes the JPEG to the cache dir under a unique name (notification daemons
/// may cache by path) and prunes old snapshots.
fn save_snapshot(jpeg: Vec<u8>) -> Result<PathBuf> {
    let dir = config::cache_dir().join("snapshots");
    fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let path = dir.join(format!("snap-{stamp}.jpg"));
    fs::write(&path, jpeg)?;

    let mut old: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jpg"))
        .collect();
    old.sort();
    let excess = old.len().saturating_sub(KEEP_SNAPSHOTS);
    for p in &old[..excess] {
        let _ = fs::remove_file(p);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs() -> NotifyConfig {
        NotifyConfig { cooldown_secs: 10, ..Default::default() }
    }

    #[test]
    fn fires_on_rising_edge_only() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let on = Detection { motion: true, ..Default::default() };
        assert_eq!(t.update(on, &prefs(), now), vec![Kind::Motion]);
        assert!(t.update(on, &prefs(), now + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn respects_cooldown() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let on = Detection { motion: true, ..Default::default() };
        let off = Detection::default();
        t.update(on, &prefs(), now);
        t.update(off, &prefs(), now + Duration::from_secs(1));
        assert!(t.update(on, &prefs(), now + Duration::from_secs(2)).is_empty());
        t.update(off, &prefs(), now + Duration::from_secs(3));
        assert_eq!(t.update(on, &prefs(), now + Duration::from_secs(11)), vec![Kind::Motion]);
    }

    #[test]
    fn person_supersedes_motion_and_disabled_kinds_are_ignored() {
        let mut t = Tracker::default();
        let det = Detection { motion: true, person: true, vehicle: true, ..Default::default() };
        // vehicle is disabled by default
        assert_eq!(t.update(det, &prefs(), Instant::now()), vec![Kind::Person]);
    }
}
