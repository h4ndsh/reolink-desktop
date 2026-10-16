//! Persistent settings, stored as TOML in `$XDG_CONFIG_HOME/reolink-desktop/config.toml`
//! with `0600` permissions (the file contains camera passwords). On Windows it
//! is `%LOCALAPPDATA%\reolink-desktop\config.toml`, private to the user by default.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const APP_DIR: &str = "reolink-desktop";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "camera")]
    pub cameras: Vec<CameraConfig>,
    pub notifications: NotifyConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraConfig {
    /// Stable identifier (preview file names, D-Bus state). Assigned on load.
    pub id: String,
    /// Friendly name shown in the window, panel menu and notifications.
    pub name: String,
    /// IP or hostname, optionally with `:port` for the HTTP API.
    pub host: String,
    pub username: String,
    pub password: String,
    pub stream: StreamQuality,
    pub rtsp_port: u16,
    /// RTSP path; `{stream}` is replaced with `main` or `sub`. `Preview_01_*` is
    /// codec-agnostic (what `GetRtspUrl` reports); older firmwares may need
    /// `h264Preview_01_{stream}`.
    pub rtsp_path: String,
    /// Talk to the HTTP API over HTTPS (newer firmwares disable plain HTTP).
    pub use_https: bool,
    pub channel: u8,
    /// Shown in the panel menu preview.
    pub preview: bool,
    /// Raise desktop notifications for this camera.
    pub alerts: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamQuality {
    Main,
    Sub,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub motion: bool,
    pub person: bool,
    pub vehicle: bool,
    pub animal: bool,
    /// Attach a JPEG snapshot to the notification.
    pub snapshot: bool,
    /// Minimum seconds between two notifications of the same kind.
    pub cooldown_secs: u64,
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub autostart: bool,
    /// The GNOME Shell extension was enabled for the user once; never again
    /// (respects a later "disable" in the Extensions app).
    pub extension_offered: bool,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "Camera".into(),
            host: String::new(),
            username: "admin".into(),
            password: String::new(),
            stream: StreamQuality::Sub,
            rtsp_port: 554,
            rtsp_path: "Preview_01_{stream}".into(),
            use_https: true,
            channel: 0,
            preview: true,
            alerts: true,
        }
    }
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            motion: true,
            person: true,
            vehicle: false,
            animal: false,
            snapshot: true,
            cooldown_secs: 30,
            poll_interval_ms: 1000,
        }
    }
}

impl StreamQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Sub => "sub",
        }
    }
}

impl CameraConfig {
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty()
    }

    /// Host without an optional `:port` suffix (the port belongs to the HTTP API).
    pub fn host_only(&self) -> &str {
        self.host.split(':').next().unwrap_or(&self.host).trim()
    }

    /// RTSP URL *without* credentials; they are handed to `rtspsrc` as
    /// properties so they never end up in logs or the process list.
    pub fn rtsp_url(&self) -> String {
        self.rtsp_url_for(self.stream)
    }

    pub fn rtsp_url_for(&self, stream: StreamQuality) -> String {
        let path = self.rtsp_path.trim_start_matches('/').replace("{stream}", stream.as_str());
        format!("rtsp://{}:{}/{}", self.host_only(), self.rtsp_port, path)
    }

    pub fn web_url(&self) -> String {
        let scheme = if self.use_https { "https" } else { "http" };
        format!("{scheme}://{}", self.host.trim())
    }
}

pub fn new_camera_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("cam-{:x}", nanos & 0xffff_ffff_ffff)
}

pub fn config_dir() -> PathBuf {
    gtk::glib::user_config_dir().join(APP_DIR)
}

pub fn cache_dir() -> PathBuf {
    gtk::glib::user_cache_dir().join(APP_DIR)
}

/// Where preview JPEGs for the GNOME Shell extension are written.
pub fn preview_dir() -> PathBuf {
    gtk::glib::user_runtime_dir().join(APP_DIR).join("preview")
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// The app used to be called "Reolink Tray": carry its settings and
/// autostart entry over once.
fn migrate_from_reolink_tray() {
    let base = gtk::glib::user_config_dir();
    let old = base.join("reolink-tray").join("config.toml");
    let new = config_path();
    if old.exists() && !new.exists() {
        let copied = fs::create_dir_all(config_dir()).and_then(|_| fs::copy(&old, &new));
        match copied {
            Ok(_) => log::info!("imported settings from {}", old.display()),
            Err(e) => log::warn!("could not import {}: {e}", old.display()),
        }
    }
    let old_autostart = base.join("autostart").join("reolink-tray.desktop");
    if old_autostart.exists() {
        let _ = fs::remove_file(&old_autostart);
        if let Err(e) = set_autostart(true) {
            log::warn!("autostart: {e:#}");
        }
    }
}

/// Accepts both the current format (`[[camera]]` array) and the old
/// single-camera format (`[camera]` table).
#[derive(Deserialize)]
#[serde(untagged)]
enum CamerasField {
    Many(Vec<CameraConfig>),
    One(CameraConfig),
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    camera: Option<CamerasField>,
    notifications: NotifyConfig,
    ui: UiConfig,
}

impl Config {
    /// Loads the saved config, or an empty one on first run. Never fails: a
    /// broken file is logged and replaced in memory by defaults.
    pub fn load() -> Self {
        migrate_from_reolink_tray();
        let path = config_path();
        let Ok(text) = fs::read_to_string(&path) else { return Self::default() };
        match Self::parse(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                log::error!("invalid config {}: {e}; using defaults", path.display());
                Self::default()
            }
        }
    }

    fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text)?;
        let cameras = match raw.camera {
            Some(CamerasField::Many(cams)) => cams,
            Some(CamerasField::One(cam)) => vec![cam],
            None => Vec::new(),
        };
        let mut cfg = Self { cameras, notifications: raw.notifications, ui: raw.ui };
        cfg.cameras.retain(CameraConfig::is_configured);
        for (i, cam) in cfg.cameras.iter_mut().enumerate() {
            if cam.id.is_empty() {
                cam.id = format!("{}-{i}", new_camera_id());
            }
        }
        Ok(cfg)
    }

    pub fn is_configured(&self) -> bool {
        !self.cameras.is_empty()
    }

    pub fn camera(&self, id: &str) -> Option<&CameraConfig> {
        self.cameras.iter().find(|c| c.id == id)
    }

    /// Atomically writes the config with owner-only permissions.
    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join("config.toml.tmp");
        let text = toml::to_string_pretty(self)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("opening {}", tmp.display()))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, config_path())?;
        Ok(())
    }
}

/// Creates or removes `~/.config/autostart/reolink-desktop.desktop`.
#[cfg(not(windows))]
pub fn set_autostart(enabled: bool) -> Result<()> {
    let dir = gtk::glib::user_config_dir().join("autostart");
    let path = dir.join("reolink-desktop.desktop");
    if enabled {
        fs::create_dir_all(&dir)?;
        fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Reolink Desktop\nExec=reolink-desktop\n\
             Icon=reolink-desktop\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
        )?;
    } else if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Adds or removes the app in `HKCU\…\CurrentVersion\Run` (the uninstaller
/// removes the value too).
#[cfg(windows)]
pub fn set_autostart(enabled: bool) -> Result<()> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "Reolink Desktop";

    let run = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        .context("opening the Run registry key")?;
    if enabled {
        let exe = std::env::current_exe()?;
        run.set_value(VALUE, &format!("\"{}\"", exe.display()))?;
    } else {
        match run.delete_value(VALUE) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtsp_url_uses_stream_and_strips_http_port() {
        let mut cam = CameraConfig { host: "192.168.10.10:8080".into(), ..Default::default() };
        assert!(cam.is_configured());
        assert!(!CameraConfig::default().is_configured());
        cam.stream = StreamQuality::Main;
        assert_eq!(cam.rtsp_url(), "rtsp://192.168.10.10:554/Preview_01_main");
        assert_eq!(cam.rtsp_url_for(StreamQuality::Sub), "rtsp://192.168.10.10:554/Preview_01_sub");
    }

    #[test]
    fn migrates_single_camera_format() {
        let cfg = Config::parse("[camera]\nhost = \"10.0.0.5\"\ntheme = \"dark\"\n[ui]\nautohide = true\n").unwrap();
        assert_eq!(cfg.cameras.len(), 1);
        assert_eq!(cfg.cameras[0].host, "10.0.0.5");
        assert_eq!(cfg.cameras[0].username, "admin");
        assert!(!cfg.cameras[0].id.is_empty());
        assert_eq!(cfg.notifications, NotifyConfig::default());
    }

    #[test]
    fn roundtrip_many_cameras() {
        let mut cfg = Config::default();
        for host in ["10.0.0.1", "10.0.0.2"] {
            cfg.cameras.push(CameraConfig { id: new_camera_id() + host, host: host.into(), ..Default::default() });
        }
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("[[camera]]"));
        assert_eq!(Config::parse(&text).unwrap(), cfg);
    }

    #[test]
    fn empty_config_is_unconfigured() {
        assert!(!Config::parse("").unwrap().is_configured());
    }
}
