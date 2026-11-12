// Every window but the island comes from here (docs/ARCHITECTURE.md §6): the
// settings window and the floating windows of features.
//
// Feature windows are created when they are asked for. The base's note that a
// WebView2 window created after launch comes up blank was a thread problem, not
// a timing one: building a webview on the main thread from inside a synchronous
// Tauri command runs inside another webview's message callback, which WebView2
// does not re-enter — the builder never returns and the window stays
// `about:blank`. Built from any other thread (an async command, a blocking
// task) or from `setup`, a late window loads normally. The registry keeps
// feature code off that path, so `open` is safe from `start`, `stop` and `call`.

use std::sync::atomic::{AtomicBool, Ordering};

use notch_core::{Monitor, WindowSpec};
use tauri::utils::config::Color;
use tauri::{AppHandle, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2_19, ICoreWebView2_3, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
};
use webview2_com::TrySuspendCompletedHandler;
use wv2_core::Interface;

use crate::{i18n, island, log, platform, Shared};

/// WebView2 allows exactly one browser environment per app, and its options are
/// fixed by whichever webview is created first. Every window therefore asks for
/// the *same* arguments as the island (`additionalBrowserArgs` in tauri.conf.json).
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion --autoplay-policy=no-user-gesture-required";

/// With native occlusion tracking off (so the island stays painted after the
/// display turns off or the PC locks), WebView2 no longer learns that a hidden
/// window is hidden: tell it, so a hidden window draws no frames.
pub(crate) fn web_visible(win: &WebviewWindow, on: bool) -> bool {
    win.with_webview(move |webview| unsafe {
        let _ = webview.controller().SetIsVisible(on);
    })
    .is_ok()
}

/// Tells WebView2 how much memory the page behind a window may keep. `low` is
/// for a window that is out of the way: the renderer collects sooner and hands
/// pages back. Nothing is suspended or reloaded, so the page is still there, and
/// it is only ever lowered while the island is shrunk to the wake strip.
///
/// Not part of `web_visible`: `island::recover` hides and shows the island again
/// within 16 ms to give it a fresh surface after a display wakes, and a trim
/// started then is the one thing that would make that restore slow.
///
/// `with_webview` waits for the main thread, so this is never called on it.
pub(crate) fn web_memory(win: &WebviewWindow, low: bool) -> bool {
    let level = if low {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
    } else {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
    };
    win.with_webview(move |webview| unsafe {
        let Ok(core) = webview.controller().CoreWebView2() else { return };
        // An older WebView2 runtime than the binding knows: the level is a hint
        // and nothing depends on it, so a refused cast is simply the old size.
        let Ok(core) = core.cast::<ICoreWebView2_19>() else { return };
        let _ = core.SetMemoryUsageTargetLevel(level);
    })
    .is_ok()
}

/// True while the settings window is out of sight, so a suspension that lands
/// after it was asked for again knows to undo itself.
static SETTINGS_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Hands the settings page's memory back while its window is hidden, and takes
/// the page back when it is shown. `TrySuspend` is WebView2's own answer for a
/// window nobody is looking at: the page and everything in it stay, the renderer
/// and its backing store go to the page file. A runtime that will not suspend the
/// page loses the window instead, which is what the app did before and is just
/// as small; `show_settings` builds it again.
///
/// Only ever the settings window: a suspended island would not answer a hover.
pub(crate) fn web_suspend(win: &WebviewWindow, on: bool) -> bool {
    let handle = win.clone();
    win.with_webview(move |webview| unsafe {
        let Ok(core) = webview.controller().CoreWebView2() else { return };
        let Ok(core) = core.cast::<ICoreWebView2_3>() else { return };
        if !on {
            // Not suspended: the call says so and nothing else happens.
            let _ = core.Resume();
            return;
        }
        let again = core.clone();
        let gone = handle.clone();
        let handler = TrySuspendCompletedHandler::create(Box::new(
            move |result: wv2_core::Result<()>, suspended: bool| -> wv2_core::Result<()> {
                if result.is_ok() && suspended {
                    // Shown again before the suspension finished: put it back at
                    // once, or the window comes up with a page that is not running.
                    if !SETTINGS_HIDDEN.load(Ordering::Relaxed) {
                        let _ = again.Resume();
                    }
                } else if SETTINGS_HIDDEN.load(Ordering::Relaxed) {
                    // Busy with something WebView2 will not suspend: keep the
                    // memory saved instead of the page, as before.
                    let _ = gone.destroy();
                }
                Ok(())
            },
        ));
        if core.TrySuspend(&handler).is_err() && SETTINGS_HIDDEN.load(Ordering::Relaxed) {
            let _ = handle.destroy();
        }
    })
    .is_ok()
}

/// The page behind a window before its own CSS paints: never a white flash.
const BACKDROP: Color = Color(11, 12, 14, 255);

/// What goes into a window label or the page's query string.
fn plain(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn monitor(app: &AppHandle, which: Monitor) -> Option<tauri::Monitor> {
    let pref = match which {
        Monitor::Island => app.state::<Shared>().settings.lock().unwrap().screen.clone(),
        Monitor::Cursor => "cursor".to_string(),
        Monitor::Primary => "primary".to_string(),
    };
    island::target_monitor(app, &pref)
}

/// Creates the window, or shows it again when its label already exists.
pub fn open(app: &AppHandle, spec: WindowSpec) -> Result<WebviewWindow, String> {
    if let Some(win) = app.get_webview_window(&spec.label) {
        let _ = win.unminimize();
        if spec.no_activate {
            platform::show_inactive(&win);
        } else {
            let _ = win.show();
            let _ = win.set_focus();
        }
        return Ok(win);
    }
    if !(plain(&spec.label) && plain(&spec.feature) && plain(&spec.page)) {
        return Err(format!("window {:?}: label, feature and page are letters, digits, - and _", spec.label));
    }
    // `transparent` tells the page to leave its background out.
    let see_through = if spec.transparent { "&transparent=1" } else { "" };
    let url = WebviewUrl::App(format!("window.html?feature={}&page={}{see_through}", spec.feature, spec.page).into());
    let mut builder = WebviewWindowBuilder::new(app, &spec.label, url)
        .additional_browser_args(BROWSER_ARGS)
        .title(notch_brand::NAME)
        .inner_size(spec.w, spec.h)
        .decorations(!spec.frameless)
        .transparent(spec.transparent)
        .shadow(!spec.transparent)
        .always_on_top(spec.topmost)
        .content_protected(spec.exclude_from_capture)
        .skip_taskbar(spec.no_activate)
        .focused(!spec.no_activate)
        .focusable(!spec.no_activate)
        // Shown once it is in place.
        .visible(false);
    if !spec.transparent {
        builder = builder.background_color(BACKDROP);
    }
    let win = builder.build().map_err(|e| format!("window {}: {e}", spec.label))?;
    // Its taskbar button wears the mascot in the product's colour, when that is not the program's own.
    crate::brand_icon::dress(app, &win);
    if spec.no_activate {
        platform::make_non_activating(&win);
    }
    // Centred on its display, in physical pixels: displays may differ in scale.
    if let (Some(m), Ok(size)) = (monitor(app, spec.monitor), win.outer_size()) {
        let (origin, area) = (m.position(), m.size());
        let x = origin.x + (area.width as i32 - size.width as i32) / 2;
        let y = origin.y + (area.height as i32 - size.height as i32) / 2;
        let _ = win.set_position(PhysicalPosition::new(x, y));
    }
    if spec.no_activate {
        platform::show_inactive(&win);
    } else {
        let _ = win.show();
    }
    log::line(format!("window {} opened ({}/{})", spec.label, spec.feature, spec.page));
    Ok(win)
}

pub fn close(app: &AppHandle, label: &str) {
    if label == island::WINDOW_LABEL || label == SETTINGS {
        return;
    }
    if let Some(win) = app.get_webview_window(label) {
        let _ = win.destroy();
    }
}

/// A feature was switched off: none of its windows stays.
pub fn close_all_of(app: &AppHandle, feature: &str) {
    for (label, win) in app.webview_windows() {
        let Ok(url) = win.url() else { continue };
        let its = url.path().ends_with("/window.html") && url.query_pairs().any(|(k, v)| k == "feature" && v == feature);
        if its {
            close(app, &label);
        }
    }
}

// ── Settings window ───────────────────────────────────────────────────────────

const SETTINGS: &str = "settings";

/// Built when Settings is first asked for, not at launch: the app used to keep a
/// second web page — its renderer, its share of the browser process and its
/// frames — for the whole session whether or not Settings was ever opened.
///
/// Closing it hides it and suspends its page (`web_suspend`), so the memory goes
/// and the next open is instant rather than a rebuild.
pub fn create_settings(app: &AppHandle) {
    match WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
        .additional_browser_args(BROWSER_ARGS)
        .title(i18n::t("Settings — {app}"))
        .inner_size(760.0, 620.0)
        .min_inner_size(640.0, 480.0)
        .background_color(BACKDROP)
        .resizable(true)
        .visible(false)
        // Shown only by `show_settings`, which also gives it the keyboard: built
        // this way it never takes the focus from whatever the user is typing in.
        .focused(false)
        .center()
        .build()
    {
        Ok(win) => {
            SETTINGS_HIDDEN.store(false, Ordering::Relaxed);
            let hidden = win.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    // Out of sight, not away: the keyboard goes back where it came
                    // from, and the page is put down until Settings is asked for
                    // again, which then costs nothing.
                    api.prevent_close();
                    platform::settings_closing(&hidden);
                    SETTINGS_HIDDEN.store(true, Ordering::Relaxed);
                    let _ = hidden.hide();
                    // Off this thread: `with_webview` waits for the main thread,
                    // which runs this handler (island::recover says why).
                    let win = hidden.clone();
                    std::thread::spawn(move || {
                        web_visible(&win, false);
                        web_memory(&win, true);
                        web_suspend(&win, true);
                    });
                }
            });
        }
        Err(err) => log::line(format!("settings window failed: {err}")),
    }
}

pub fn show_settings(app: &AppHandle) {
    let Some(win) = app.get_webview_window(SETTINGS) else {
        // Closed for good, or never built yet: build it, from async code, never
        // from the command or menu callback that asked.
        log::line("settings window missing: creating it");
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            create_settings(&app);
            if app.get_webview_window(SETTINGS).is_some() {
                show_settings(&app);
            }
        });
        return;
    };
    // Asked for before the window comes up: the page is resumed first, so it is
    // running by the time it is on screen, and its memory is normal again.
    SETTINGS_HIDDEN.store(false, Ordering::Relaxed);
    web_suspend(&win, false);
    web_memory(&win, false);
    platform::settings_opening(&win);
    web_visible(&win, true);
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();
}

/// The title follows the language.
pub fn retitle_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS) {
        let _ = window.set_title(&i18n::t("Settings — {app}"));
    }
}
