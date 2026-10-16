//! Notification-area icon on Windows (tray-icon). The icon belongs to the
//! thread that created it and needs a Win32 message loop there, so it gets its
//! own thread, like ksni on Linux. The GTK thread hands it updates through
//! `pending` and wakes it with a thread message.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG, PM_NOREMOVE, WM_APP,
    WM_QUIT,
};

use super::{render_rgba, Entry, MENU, PAUSE_LABEL};
use crate::{AppEvent, CamState};

const ICON_SIZE: u32 = 32;
/// Posted to the tray thread after `pending` was filled.
const WM_UPDATE: u32 = WM_APP + 1;
/// Windows cuts tooltips at 128 UTF-16 units, terminator included.
const TOOLTIP_MAX: usize = 127;

type Pending = Arc<Mutex<Option<(CamState, String)>>>;

pub struct Handle {
    thread_id: u32,
    pending: Pending,
    thread: JoinHandle<()>,
}

pub fn spawn(paused: Arc<AtomicBool>, tx: async_channel::Sender<AppEvent>) -> Option<Handle> {
    let pending = Pending::default();
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("tray".into())
        .spawn({
            let pending = pending.clone();
            move || run(paused, tx, pending, ready_tx)
        })
        .ok()?;
    match ready_rx.recv() {
        Ok(Ok(thread_id)) => Some(Handle { thread_id, pending, thread }),
        Ok(Err(e)) => {
            log::error!("tray icon unavailable: {e}");
            let _ = thread.join();
            None
        }
        Err(_) => {
            log::error!("tray icon thread exited during start-up");
            None
        }
    }
}

impl Handle {
    pub fn update(&self, state: CamState, summary: String) {
        *self.pending.lock().unwrap() = Some((state, summary));
        unsafe { PostThreadMessageW(self.thread_id, WM_UPDATE, 0, 0) };
    }

    pub fn shutdown(self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        let _ = self.thread.join();
    }
}

fn run(
    paused: Arc<AtomicBool>,
    tx: async_channel::Sender<AppEvent>,
    pending: Pending,
    ready: mpsc::Sender<Result<u32, String>>,
) {
    // Creates this thread's message queue, so `update` can post to it as soon
    // as `spawn` returns.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) };

    let (tray, pause_item) = match build(&paused, &tx) {
        Ok(built) => built,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    // 0 is WM_QUIT, -1 an error.
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        if msg.hwnd.is_null() && msg.message == WM_UPDATE {
            if let Some((state, summary)) = pending.lock().unwrap().take() {
                let _ = tray.set_icon(icon(state));
                let _ = tray.set_tooltip(Some(tooltip(&summary)));
                pause_item.set_checked(paused.load(Ordering::Relaxed));
            }
            continue;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    drop(tray); // removes the icon
}

fn build(paused: &AtomicBool, tx: &async_channel::Sender<AppEvent>) -> Result<(TrayIcon, CheckMenuItem), String> {
    let menu = Menu::new();
    let pause_item = CheckMenuItem::new(PAUSE_LABEL, true, paused.load(Ordering::Relaxed), None);
    let mut actions: Vec<(MenuId, fn() -> AppEvent)> = vec![(pause_item.id().clone(), || AppEvent::TogglePause)];
    for entry in MENU {
        let added = match *entry {
            Entry::Item { label, event, .. } => {
                let item = MenuItem::new(label, true, None);
                actions.push((item.id().clone(), event));
                menu.append(&item)
            }
            Entry::Pause => menu.append(&pause_item),
            Entry::Separator => menu.append(&PredefinedMenuItem::separator()),
        };
        added.map_err(|e| e.to_string())?;
    }

    // Both handlers are process-wide and run on this thread, from DispatchMessageW.
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some((_, event)) = actions.iter().find(|(id, _)| *id == e.id) {
            let _ = menu_tx.send_blocking(event());
        }
    }));
    let click_tx = tx.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            let _ = click_tx.send_blocking(AppEvent::ToggleWindow);
        }
    }));

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Reolink");
    if let Some(icon) = icon(CamState::Connecting) {
        builder = builder.with_icon(icon);
    }
    let tray = builder.build().map_err(|e| e.to_string())?;
    Ok((tray, pause_item))
}

fn icon(state: CamState) -> Option<Icon> {
    Icon::from_rgba(render_rgba(ICON_SIZE, state), ICON_SIZE, ICON_SIZE).ok()
}

fn tooltip(summary: &str) -> String {
    let text = if summary.is_empty() { "Reolink".to_owned() } else { format!("Reolink\n{summary}") };
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= TOOLTIP_MAX
        })
        .collect()
}
