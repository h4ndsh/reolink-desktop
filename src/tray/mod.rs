//! Tray icon with the app menu: a StatusNotifierItem on Linux desktops other
//! than GNOME (KDE, XFCE, Cinnamon…; on GNOME the bundled Shell extension
//! replaces it) and the notification area on Windows.
//!
//! Both backends run on their own thread and expose the same `Handle`:
//! `update(state, summary)` and `shutdown()`.

use crate::{AppEvent, CamState};

#[cfg(target_os = "linux")]
mod sni;
#[cfg(target_os = "linux")]
pub use sni::{spawn, Handle};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{spawn, Handle};

/// One line of the tray menu.
enum Entry {
    Item {
        label: &'static str,
        /// Freedesktop icon name (the Windows menu has no icons).
        #[cfg_attr(windows, allow(dead_code))]
        icon: &'static str,
        event: fn() -> AppEvent,
    },
    /// "Pause Notifications" check item.
    Pause,
    Separator,
}

const MENU: &[Entry] = &[
    Entry::Item { label: "Live View", icon: "camera-web-symbolic", event: || AppEvent::ShowLive },
    Entry::Item { label: "Recordings", icon: "document-open-recent-symbolic", event: || AppEvent::ShowRecordings },
    Entry::Item { label: "Settings…", icon: "emblem-system-symbolic", event: || AppEvent::ShowSettings },
    Entry::Separator,
    Entry::Pause,
    Entry::Separator,
    Entry::Item { label: "Quit", icon: "application-exit-symbolic", event: || AppEvent::Quit },
];

const PAUSE_LABEL: &str = "Pause Notifications";

/// Status colours (macOS system palette).
fn status_rgb(state: CamState) -> [u8; 3] {
    match state {
        CamState::Online => [0x30, 0xD1, 0x58],     // green
        CamState::Alert => [0xFF, 0x45, 0x3A],      // red
        CamState::Connecting => [0xFF, 0x9F, 0x0A], // amber
        CamState::Offline | CamState::Unconfigured => [0x8E, 0x8E, 0x93], // gray
    }
}

/// Draws a video-camera glyph whose lens shows the status colour, as
/// non-premultiplied RGBA rows.
fn render_rgba(size: u32, state: CamState) -> Vec<u8> {
    const SS: u32 = 4; // supersampling per axis
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
                    (r / a).round() as u8,
                    (g / a).round() as u8,
                    (b / a).round() as u8,
                    (255.0 * a / total).round() as u8,
                ]);
            }
        }
    }
    data
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
        let data = render_rgba(32, CamState::Alert);
        assert_eq!(data.len(), 32 * 32 * 4);
        // Lens centre at (0.40, 0.50) of 32px.
        let i = (16 * 32 + 12) * 4;
        assert_eq!(&data[i..i + 4], &[0xFF, 0x45, 0x3A, 255]);
        // Top-left corner is transparent.
        assert_eq!(data[3], 0);
    }
}
