//! StatusNotifierItem tray icon (ksni) for Linux desktops other than GNOME.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Category, Icon, MenuItem, ToolTip, Tray};

use super::{render_rgba, Entry, MENU, PAUSE_LABEL};
use crate::{AppEvent, CamState};

pub struct Handle(ksni::blocking::Handle<CamTray>);

pub struct CamTray {
    /// Worst state across all cameras.
    state: CamState,
    /// One "name: state" line per camera, for the tooltip.
    summary: String,
    paused: Arc<AtomicBool>,
    tx: async_channel::Sender<AppEvent>,
}

pub fn spawn(paused: Arc<AtomicBool>, tx: async_channel::Sender<AppEvent>) -> Option<Handle> {
    let tray = CamTray { state: CamState::Connecting, summary: String::new(), paused, tx };
    match tray.spawn() {
        Ok(handle) => Some(Handle(handle)),
        Err(e) => {
            log::error!("system tray unavailable ({e}); is a StatusNotifier host running?");
            None
        }
    }
}

impl Handle {
    pub fn update(&self, state: CamState, summary: String) {
        self.0.update(|t| {
            t.state = state;
            t.summary = summary;
        });
    }

    pub fn shutdown(self) {
        self.0.shutdown().wait();
    }
}

impl CamTray {
    fn send(&self, ev: AppEvent) {
        let _ = self.tx.send_blocking(ev);
    }
}

impl Tray for CamTray {
    fn id(&self) -> String {
        "reolink-desktop".into()
    }

    fn title(&self) -> String {
        format!("Reolink — {}", self.state.label())
    }

    fn category(&self) -> Category {
        Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        [16, 22, 24, 32, 48].iter().map(|&s| sni_icon(s, self.state)).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Reolink".into(),
            description: self.summary.clone(),
            ..Default::default()
        }
    }

    /// Left click (on hosts that support it) toggles the popup.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(AppEvent::ToggleWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        MENU.iter()
            .map(|entry| match *entry {
                Entry::Item { label, icon, event } => StandardItem {
                    label: label.into(),
                    icon_name: icon.into(),
                    activate: Box::new(move |t: &mut Self| t.send(event())),
                    ..Default::default()
                }
                .into(),
                Entry::Pause => CheckmarkItem {
                    label: PAUSE_LABEL.into(),
                    checked: self.paused.load(Ordering::Relaxed),
                    activate: Box::new(|t: &mut Self| t.send(AppEvent::TogglePause)),
                    ..Default::default()
                }
                .into(),
                Entry::Separator => MenuItem::Separator,
            })
            .collect()
    }
}

/// The SNI spec wants ARGB32 in network byte order.
fn sni_icon(size: u32, state: CamState) -> Icon {
    let data = render_rgba(size, state).chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
    Icon { width: size as i32, height: size as i32, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sni_icon_is_argb() {
        let icon = sni_icon(32, CamState::Alert);
        let i = (16 * 32 + 12) * 4;
        assert_eq!(&icon.data[i..i + 4], &[255, 0xFF, 0x45, 0x3A]);
    }
}
