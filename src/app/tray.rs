//! Native system-tray integration for the desktop app.
//!
//! Linux needs its own GTK event-loop thread because eframe uses winit rather
//! than GTK. Windows and macOS create the tray icon on eframe's event thread.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use super::engine::AppSettings;

const OPEN_ID: &str = "inxm.tray.open";
const PAUSE_ID: &str = "inxm.tray.pause";
const QUIT_ID: &str = "inxm.tray.quit";

/// How long the tray's `Quit` handler waits for a graceful shutdown (the
/// window actually closing, `InxmApp` dropping, and `TrayController` with it)
/// before giving up and forcing the process to exit. See the `Quit` arm in
/// `MenuEvent::set_event_handler` below for why a hard fallback is needed at
/// all.
const QUIT_WATCHDOG_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_millis(1500);

thread_local! {
    /// A handle to the tray icon on whichever thread actually creates and
    /// services it: the main eframe/winit thread on Windows and macOS, or
    /// the dedicated GTK thread on Linux (see the module docs). `TrayIcon`
    /// is neither `Send` nor `Sync`, so it cannot be captured directly by
    /// the `MenuEvent` handler below (which `tray-icon` requires to be
    /// `Send + Sync`); stashing a clone here instead — `TrayIcon` is
    /// reference-counted, so this is cheap and keeps the real icon alive
    /// exactly as long as it already would be — lets that handler reach it
    /// safely, since the handler always runs on the same thread that
    /// populates this cell.
    static ACTIVE_TRAY_ICON: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    TogglePause,
    Quit,
}

pub struct TrayController {
    actions: mpsc::Receiver<TrayAction>,
    #[cfg(not(target_os = "linux"))]
    icon: tray_icon::TrayIcon,
}

impl TrayController {
    pub fn new(
        ctx: &egui::Context,
        schedules_paused: bool,
        quit_requested: Arc<AtomicBool>,
        settings_path: PathBuf,
        // Raw HWND of the main window, if available (Windows only; always
        // `None` elsewhere). See the `Open` arm below for why this exists.
        window_hwnd: Option<isize>,
    ) -> Result<Self, String> {
        let (action_tx, actions) = mpsc::channel();
        let repaint = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            // Referenced unconditionally so this closure always captures
            // `window_hwnd` (the meaningful use below is behind
            // `cfg(windows)`, which would otherwise leave it fully unused,
            // and therefore warned about, on other platforms).
            let _ = window_hwnd;
            let action = match event.id().as_ref() {
                OPEN_ID => Some(TrayAction::Open),
                PAUSE_ID => Some(TrayAction::TogglePause),
                QUIT_ID => Some(TrayAction::Quit),
                _ => None,
            };
            if let Some(action) = action {
                if action == TrayAction::TogglePause {
                    // A hidden window produces no frames on Windows, so the
                    // mpsc-based channel drain in `handle_tray_actions` might
                    // not run for a long time. The scheduler loop, however,
                    // re-reads the settings file from disk on every tick
                    // (see `AppSettings::load` in the engine's tick), so the
                    // settings file is the actual source of truth for
                    // pausing: flip and persist it right here, directly on
                    // the OS thread that delivered the menu event, so the
                    // pause takes effect immediately regardless of whether
                    // the window is visible. The channel send below is only
                    // there to let the app resync its in-memory copy once a
                    // frame eventually runs.
                    //
                    // The tray thread and the UI thread can both write this
                    // file (the Settings view saves through the engine
                    // whenever the user edits a setting), but the UI can
                    // only save while the window is visible and the user is
                    // actively interacting with it, so the odds of that
                    // save landing in the same instant as a tray click are
                    // effectively nil; a lost update here is accepted as a
                    // rare, low-stakes race rather than something worth a
                    // lock for.
                    let mut settings = AppSettings::load(&settings_path);
                    settings.schedules_paused = !settings.schedules_paused;
                    if let Err(error) = settings.save(&settings_path) {
                        tracing::warn!(
                            operation = "system_tray.toggle_pause",
                            app_version = env!("CARGO_PKG_VERSION"),
                            triggered_by = "tray_menu",
                            outcome = "failure",
                            error = %error,
                            "failed to persist paused-schedules toggle from tray"
                        );
                    }
                }
                if action == TrayAction::Open {
                    // Defense against the case where the window is currently
                    // minimized and hidden: eframe may not be pumping frames
                    // (observed on Windows), so the channel drain in
                    // `update()` would never run to process this action.
                    // Send the restore commands directly through the
                    // context here as well; they're idempotent with the
                    // app-side handler.
                    repaint.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    repaint.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    repaint.send_viewport_cmd(egui::ViewportCommand::Focus);

                    // The commands above are not enough by themselves:
                    // confirmed empirically that a hidden window on
                    // Windows never resumes producing frames from them,
                    // because egui/winit only apply queued viewport
                    // commands as part of running an actual frame, and a
                    // hidden window never receives the WM_PAINT message
                    // that would trigger one -- the exact chicken-and-egg
                    // deadlock this whole tray module otherwise works
                    // around with direct-send tricks. Break it here by
                    // calling the Win32 APIs directly on the raw window
                    // handle, entirely outside of egui's command queue:
                    // ShowWindow/SetForegroundWindow still reach the
                    // window's own message queue, so winit observes the
                    // resulting WM_SHOWWINDOW/WM_SIZE and resumes pumping
                    // frames normally from there on.
                    #[cfg(windows)]
                    if let Some(hwnd) = window_hwnd {
                        restore_window_win32(hwnd);
                    }
                }
                if action == TrayAction::Quit {
                    // Quit needs more than the direct-send trick used for
                    // Open above: a hidden window produces no frames on
                    // Windows, so `handle_tray_actions` (which drains the
                    // mpsc channel below) would never run, but simply
                    // sending `Close` through the context isn't enough
                    // either, because `handle_close_request` cancels any
                    // close request and re-hides the window to the tray
                    // unless `quit_requested` is already `true`. Set the
                    // shared atomic first so the app side sees the real
                    // intent to quit, then nudge the window visible before
                    // closing it, since resuming visibility maximizes the
                    // chance the event loop starts producing frames again
                    // (and applying viewport commands) on platforms where a
                    // hidden window otherwise stalls; a brief window flash
                    // during quit is an acceptable trade-off.
                    quit_requested.store(true, Ordering::SeqCst);
                    repaint.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    repaint.send_viewport_cmd(egui::ViewportCommand::Close);

                    // Hide the icon immediately, from this same thread,
                    // instead of waiting for `TrayController` (and the
                    // `TrayIcon` inside it) to drop once the window
                    // actually finishes closing. On Windows that teardown
                    // has been observed to never happen for a window that
                    // was hidden to the tray, leaving a stuck process and a
                    // tray icon that outlives it (reachable only via Task
                    // Manager). `set_visible(false)` is best-effort: if the
                    // icon was somehow never stashed, there is nothing more
                    // to do here.
                    ACTIVE_TRAY_ICON.with(|cell| {
                        if let Some(icon) = cell.borrow_mut().take() {
                            let _ = icon.set_visible(false);
                        }
                    });

                    // Guarantee the process actually terminates even if the
                    // graceful shutdown above stalls. Quitting from the
                    // tray must never be able to leave the app stuck alive
                    // (still holding the single-instance mutex, so it can
                    // then neither be reopened nor quit again except from
                    // Task Manager). If graceful shutdown does complete
                    // first, this thread is torn down with the rest of the
                    // process well before the grace period elapses and
                    // never actually calls `exit`.
                    std::thread::Builder::new()
                        .name("inxm-quit-watchdog".to_owned())
                        .spawn(|| {
                            std::thread::sleep(QUIT_WATCHDOG_GRACE_PERIOD);
                            std::process::exit(0);
                        })
                        .ok();
                }
                let _ = action_tx.send(action);
                repaint.request_repaint();
            }
        }));

        let icon = app_icon(super::theme::is_dark())?;

        #[cfg(target_os = "linux")]
        {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("inxm-tray".to_owned())
                .spawn(move || {
                    let result = (|| -> Result<tray_icon::TrayIcon, String> {
                        gtk::init().map_err(|error| format!("initialize GTK: {error}"))?;
                        build_tray(icon, schedules_paused)
                    })();
                    match result {
                        Ok(icon) => {
                            ACTIVE_TRAY_ICON.with(|cell| *cell.borrow_mut() = Some(icon.clone()));
                            let _ = ready_tx.send(Ok(()));
                            let _icon = icon;
                            gtk::main();
                        }
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                        }
                    }
                })
                .map_err(|error| format!("start tray thread: {error}"))?;

            ready_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .map_err(|error| format!("wait for tray startup: {error}"))??;
            Ok(Self { actions })
        }

        #[cfg(not(target_os = "linux"))]
        {
            let icon = build_tray(icon, schedules_paused)?;
            ACTIVE_TRAY_ICON.with(|cell| *cell.borrow_mut() = Some(icon.clone()));
            Ok(Self { actions, icon })
        }
    }

    pub fn drain_actions(&self) -> impl Iterator<Item = TrayAction> + '_ {
        self.actions.try_iter()
    }

    /// Swap the tray icon to match the app theme. On Linux the tray handle
    /// lives on its own GTK thread, so the icon keeps whatever theme was
    /// active at startup there.
    pub fn set_dark_mode(&self, dark_mode: bool) {
        #[cfg(not(target_os = "linux"))]
        if let Ok(icon) = app_icon(dark_mode) {
            if let Err(error) = self.icon.set_icon(Some(icon)) {
                tracing::warn!(
                    operation = "system_tray.set_icon",
                    app_version = env!("CARGO_PKG_VERSION"),
                    triggered_by = "theme_change",
                    outcome = "failure",
                    error = %error,
                    "failed to update tray icon for theme change"
                );
            }
        }
        #[cfg(target_os = "linux")]
        let _ = dark_mode;
    }
}

impl Drop for TrayController {
    fn drop(&mut self) {
        MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
    }
}

fn build_tray(icon: Icon, schedules_paused: bool) -> Result<tray_icon::TrayIcon, String> {
    let open = MenuItem::with_id(OPEN_ID, "Open INXM Local", true, None);
    let pause = CheckMenuItem::with_id(PAUSE_ID, "Pause schedules", true, schedules_paused, None);
    let quit = MenuItem::with_id(QUIT_ID, "Quit", true, None);
    let menu = Menu::with_items(&[&open, &pause, &quit])
        .map_err(|error| format!("create tray menu: {error}"))?;

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(true)
        .with_tooltip("INXM Local")
        .with_icon(icon)
        .build()
        .map_err(|error| format!("create tray icon: {error}"))
}

/// Shows and focuses the main window via raw Win32 calls, bypassing egui's
/// viewport-command queue entirely. See the `Open` arm in
/// `MenuEvent::set_event_handler` above for why this is necessary: a hidden
/// window never resumes producing frames from `ViewportCommand`s alone.
#[cfg(windows)]
fn restore_window_win32(hwnd: isize) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
    };

    let hwnd = hwnd as HWND;
    unsafe {
        // `SW_RESTORE` un-minimizes as well as showing; plain `SW_SHOW`
        // would leave a previously minimized window iconic.
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        // Best-effort: Windows can refuse to grant foreground focus to a
        // background process (the "foreground lock" heuristic); the window
        // still gets shown and available on the taskbar regardless.
        SetForegroundWindow(hwnd);
    }
}

fn app_icon(dark_mode: bool) -> Result<Icon, String> {
    let bytes: &[u8] = match dark_mode {
        true => include_bytes!("../../assets/favicon192-dark.png"),
        false => include_bytes!("../../assets/favicon192-light.png"),
    };
    let icon = eframe::icon_data::from_png_bytes(bytes)
        .map_err(|error| format!("decode tray icon: {error}"))?;
    Icon::from_rgba(icon.rgba, icon.width, icon.height)
        .map_err(|error| format!("prepare tray icon: {error}"))
}
