//! StatusNotifierItem tray icon for desktops other than GNOME (KDE, XFCE,
//! Cinnamon…). On GNOME the bundled Shell extension replaces it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Category, Icon, MenuItem, ToolTip, Tray};

use crate::{AppEvent, CamState};

pub struct CamTray {
    /// Worst state across all cameras.
    pub state: CamState,
    /// One "name: state" line per camera, for the tooltip.
    pub summary: String,
    paused: Arc<AtomicBool>,
    tx: async_channel::Sender<AppEvent>,
}

pub fn spawn(
    paused: Arc<AtomicBool>,
    tx: async_channel::Sender<AppEvent>,
) -> Option<Handle<CamTray>> {
    let tray = CamTray { state: CamState::Connecting, summary: String::new(), paused, tx };
    match tray.spawn() {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::error!("system tray unavailable ({e}); is a StatusNotifier host running?");
            None
        }
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
        [16, 22, 24, 32, 48].iter().map(|&s| render_icon(s, self.state)).collect()
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
        vec![
            StandardItem {
                label: "Live View".into(),
                icon_name: "camera-web-symbolic".into(),
                activate: Box::new(|t: &mut Self| t.send(AppEvent::ShowLive)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Recordings".into(),
                icon_name: "document-open-recent-symbolic".into(),
                activate: Box::new(|t: &mut Self| t.send(AppEvent::ShowRecordings)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Settings…".into(),
                icon_name: "emblem-system-symbolic".into(),
                activate: Box::new(|t: &mut Self| t.send(AppEvent::ShowSettings)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Pause Notifications".into(),
                checked: self.paused.load(Ordering::Relaxed),
                activate: Box::new(|t: &mut Self| t.send(AppEvent::TogglePause)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|t: &mut Self| t.send(AppEvent::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Status colours (macOS system palette).
fn status_rgb(state: CamState) -> [u8; 3] {
    match state {
        CamState::Online => [0x30, 0xD1, 0x58],     // green
        CamState::Alert => [0xFF, 0x45, 0x3A],      // red
        CamState::Connecting => [0xFF, 0x9F, 0x0A], // amber
        CamState::Offline | CamState::Unconfigured => [0x8E, 0x8E, 0x93], // gray
    }
}

/// Draws a video-camera glyph whose lens shows the status colour.
/// Output is ARGB32 in network byte order, as required by the SNI spec.
pub fn render_icon(size: i32, state: CamState) -> Icon {
    const SS: i32 = 4; // supersampling per axis
    const GLYPH: [u8; 3] = [0xEE, 0xEE, 0xEE];
    let lens = status_rgb(state);
    let n = size as f32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);

    for py in 0..size {
        for px in 0..size {
            let (mut a, mut r, mut g, mut b) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / n;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / n;
                    let colour = if circle(x, y, 0.40, 0.50, 0.15) {
                        Some(lens)
                    } else if circle(x, y, 0.40, 0.50, 0.21) {
                        None // transparent ring around the lens
                    } else if round_rect(x, y, 0.04, 0.24, 0.74, 0.76, 0.11)
                        || round_rect(x, y, 0.78, 0.36, 0.97, 0.64, 0.05)
                    {
                        Some(GLYPH)
                    } else {
                        None
                    };
                    if let Some([cr, cg, cb]) = colour {
                        a += 1.0;
                        r += cr as f32;
                        g += cg as f32;
                        b += cb as f32;
                    }
                }
            }
            if a == 0.0 {
                data.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let total = (SS * SS) as f32;
                data.extend_from_slice(&[
                    (255.0 * a / total).round() as u8,
                    (r / a).round() as u8,
                    (g / a).round() as u8,
                    (b / a).round() as u8,
                ]);
            }
        }
    }
    Icon { width: size, height: size, data }
}

fn circle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> bool {
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn round_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    circle(x, y, cx, cy, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_expected_size_and_lens_colour() {
        let icon = render_icon(32, CamState::Alert);
        assert_eq!(icon.data.len(), 32 * 32 * 4);
        // Lens centre at (0.40, 0.50) of 32px.
        let i = ((16 * 32 + 12) * 4) as usize;
        assert_eq!(&icon.data[i..i + 4], &[255, 0xFF, 0x45, 0x3A]);
        // Top-left corner is transparent.
        assert_eq!(icon.data[0], 0);
    }
}
