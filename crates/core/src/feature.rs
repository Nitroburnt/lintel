// The contract between the app and a feature (docs/ARCHITECTURE.md §2, §6).
//
// A feature is one object implementing `Feature`. The app owns the list
// (src-tauri/src/registry.rs), calls `start` when the feature is switched on
// and `stop` when it is switched off or the app quits, and routes every command
// of the feature through `call`. `Ctx` is everything a feature may ask of the
// app: its own settings, events, a data folder, windows and the pause flag.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, WebviewWindow};

/// Where the feature is listed in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Group {
    Island,
    Agents,
    Chat,
    Media,
    System,
    Input,
    Windows,
    Files,
    Capture,
    Apps,
    Tools,
}

/// What a feature can do that the user should know about (Settings › Privacy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Use {
    Network,
    Microphone,
    Camera,
    Clipboard,
    InputHook,
    Capture,
    Elevation,
    FileDelete,
}

/// A global shortcut the feature offers. `default: None` stays unbound until
/// the user picks keys in Settings.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ShortcutDef {
    pub id: &'static str,
    /// English, translated through strings.json.
    pub label: &'static str,
    pub default: Option<&'static str>,
}

pub struct Manifest {
    /// Lowercase kebab, frozen forever.
    pub id: &'static str,
    /// English, translated through strings.json.
    pub name: &'static str,
    pub group: Group,
    /// true for the core product; opt-in tools false.
    pub default_on: bool,
    pub uses: &'static [Use],
    pub shortcuts: &'static [ShortcutDef],
    /// Tray menu items: (item id, English label).
    pub tray: &'static [(&'static str, &'static str)],
    /// JSON object: the feature's default settings.
    pub defaults: &'static str,
}

pub trait Feature: Send + Sync {
    fn manifest(&self) -> &'static Manifest;
    /// Called when enabled at launch or when switched on. Spawn everything here. Must be idempotent.
    fn start(&self, cx: &Ctx) -> Result<(), String>;
    /// Called when switched off and at quit. Must leave nothing running. Must be idempotent.
    fn stop(&self, cx: &Ctx);
    /// The feature's settings changed (already merged with defaults).
    fn settings_changed(&self, _cx: &Ctx, _settings: &Value) {}
    /// Every command of the feature. Runs on a blocking thread; return fast,
    /// spawn long work and emit events. The host also sends `"shortcut"` and
    /// `"tray"`, each with `{ "id" }`.
    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String>;
    /// `include_str!` of src/features/<id>/strings.json (the same file the TS side loads).
    fn strings(&self) -> &'static str {
        "{}"
    }
}

// ── The app, as a feature sees it ─────────────────────────────────────────────

/// What the app implements for `Ctx` (src-tauri/src/registry.rs). Features
/// never see this: they go through `Ctx`.
pub trait Host: Send + Sync + 'static {
    fn settings(&self, app: &AppHandle, id: &str) -> Value;
    fn patch_settings(&self, app: &AppHandle, id: &str, patch: Value);
    fn open_window(&self, app: &AppHandle, spec: WindowSpec) -> Result<WebviewWindow, String>;
    fn close_window(&self, app: &AppHandle, label: &str);
}

/// Cheap to clone: move a copy into the tasks and threads a feature spawns.
#[derive(Clone)]
pub struct Ctx {
    app: AppHandle,
    host: &'static dyn Host,
}

impl Ctx {
    pub fn new(app: AppHandle, host: &'static dyn Host) -> Self {
        Self { app, host }
    }

    /// The Tauri handle, for what `Ctx` does not wrap (managed state, the
    /// island window, `run_on_main_thread`).
    pub fn app(&self) -> &AppHandle {
        &self.app
    }

    /// The feature's settings: its manifest defaults with what is stored on top.
    pub fn settings(&self, id: &str) -> Value {
        self.host.settings(&self.app, id)
    }

    /// Shallow merge into the stored settings, saved and broadcast as
    /// `settings-changed:<id>`.
    pub fn patch_settings(&self, id: &str, patch: Value) {
        self.host.patch_settings(&self.app, id, patch);
    }

    /// `<id>:<event>` to every window.
    pub fn emit<T: Serialize + Clone>(&self, id: &str, event: &str, payload: T) {
        let _ = self.app.emit(&format!("{id}:{event}"), payload);
    }

    /// %LOCALAPPDATA%\lintel\features\<id>\, created on the way.
    pub fn data_dir(&self, id: &str) -> PathBuf {
        let dir = crate::win::local_dir().join("features").join(id);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Floating windows (§6).
    pub fn window(&self) -> WindowFactory<'_> {
        WindowFactory { cx: self }
    }

    /// Tray › Pause: nothing reaches the network or the screen while it is on.
    pub fn paused(&self) -> bool {
        paused()
    }
}

// ── Pause ─────────────────────────────────────────────────────────────────────

static PAUSED: AtomicBool = AtomicBool::new(false);

/// Set from the tray's Pause item. Pausing Lintel has to mean pausing Lintel, not
/// just hiding the island: pollers decline to work while it is on.
pub fn set_paused(on: bool) {
    PAUSED.store(on, Ordering::Relaxed);
}

pub fn paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

// ── Windows ───────────────────────────────────────────────────────────────────

/// The display a window opens on, centred.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Monitor {
    /// The one the island lives on.
    #[default]
    Island,
    /// The one under the pointer.
    Cursor,
    Primary,
}

/// Where a window goes instead of centred on its `monitor`, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// The window's bottom-right corner sits here.
    Corner(i32, i32),
    /// The window's bottom edge sits at `y`, centred on `x`, and kept inside the screen that
    /// holds the point: a panel over a tray icon, whose width is only known once it is built.
    Centred(i32, i32),
}

/// One feature window. Every window loads `window.html?feature=<feature>&page=<page>`,
/// which mounts `windows[page]` of the feature's TypeScript module.
#[derive(Debug, Clone, Default)]
pub struct WindowSpec {
    /// Unique among windows; opening a label that exists shows that window.
    pub label: String,
    pub feature: String,
    pub page: String,
    /// Logical pixels.
    pub w: f64,
    pub h: f64,
    /// No OS frame: the page draws its own (`.win` in components.css).
    pub frameless: bool,
    pub topmost: bool,
    /// Never takes focus and stays out of Alt-Tab (overlays, HUD surfaces).
    pub no_activate: bool,
    /// Left out of screenshots and recordings.
    pub exclude_from_capture: bool,
    pub transparent: bool,
    pub monitor: Monitor,
    /// Instead of centred on `monitor`: where the window sits, in physical pixels (a panel
    /// above a tray icon, say).
    pub anchor: Option<Anchor>,
}

pub struct WindowFactory<'a> {
    cx: &'a Ctx,
}

impl WindowFactory<'_> {
    /// Creates the window, or shows it again when its label already exists.
    /// Never call this from the main thread inside a synchronous Tauri command:
    /// WebView2 cannot create a webview from within another webview's callback.
    /// `Feature::start`, `stop` and `call` are always off that path.
    pub fn open(&self, spec: WindowSpec) -> Result<WebviewWindow, String> {
        self.cx.host.open_window(&self.cx.app, spec)
    }

    pub fn close(&self, label: &str) {
        self.cx.host.close_window(&self.cx.app, label);
    }
}
