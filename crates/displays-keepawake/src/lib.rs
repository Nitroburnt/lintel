// Displays and keep awake — a Lintel feature. The contract is docs/ARCHITECTURE.md §2:
// nothing runs before `start`, everything is gone after `stop`, and every
// command of the feature arrives through `call`.
//
// Idle, this does nothing: no keep-awake request, no thread, no window. What it
// changes it puts back: the request is released when a session ends, when the
// feature is switched off and at quit (it dies with the process anyway); a
// dimming overlay is a click-through window that is closed when the level is
// back to full, when the feature stops and at quit; a radio is switched on again
// at wake only if this feature switched it off.
//
// Events: `session` { active, endsAt, startedAt, auto }, `ended` { reason },
// `brightness` { id, name, level, failed?, shortcut? }, `dim` { label, alpha },
// `displays` {} (the set of displays changed).

pub mod display;
pub mod power;
pub mod radio;
pub mod trayicon;
pub mod wall;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use display::{Display, Method};
use notch_core::{Anchor, Ctx, Feature, Group, Manifest, Monitor, ShortcutDef, WindowSpec};
use radio::Kind;
use serde_json::{json, Value};
use svc_input::{Key, Mouse, MouseKind, Verdict};
use svc_sysevents::Session as Sleep;
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, PhysicalPosition, PhysicalSize};

const ID: &str = "displays-keepawake";

/// One wheel notch over the tray icon moves the brightness this many points.
const WHEEL_STEP: u8 = 2;
/// A burst of notches waits this long for the wheel to stop, then writes once: the monitor's
/// DDC bus is slow, and a write per event is what made the level jump while the wheel turned.
const WHEEL_SETTLE: u64 = 90;

/// The brightness and contrast panel: a window and its own icon in the notification area.
const PANEL: &str = "brightness-panel";
const TRAY: &str = "brightness";

/// Where the slider turns from the backlight into the overlay (extra dimming).
const SPLIT: f32 = 0.15;
/// The overlay never gets blacker than this: at least 15 % of the picture stays.
const MAX_ALPHA: f32 = 0.85;
/// Brightness keys: one step is 1/16 of the range, the system's own.
const STEP: f32 = 1.0 / 16.0;

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Displays and keep awake",
    group: Group::System,
    default_on: true,
    uses: &[],
    shortcuts: &[
        ShortcutDef { id: "toggle", label: "Start or stop keep awake", default: None },
        ShortcutDef { id: "brighter", label: "Brighter (display under the pointer)", default: Some("Ctrl+Alt+PageUp") },
        ShortcutDef { id: "dimmer", label: "Dimmer (display under the pointer)", default: Some("Ctrl+Alt+PageDown") },
        ShortcutDef { id: "panel", label: "Brightness and contrast", default: None },
    ],
    tray: &[("panel", "Brightness and contrast…")],
    // minutes: the last duration picked (0 = until stopped); batteryLimit: a session ends on battery at or below it (0 = never)
    defaults: r#"{ "minutes": 60, "allowDisplaySleep": false, "batteryLimit": 10, "jiggle": false, "jiggleMinutes": 5,
        "autoExternal": false, "autoPower": false, "autoAll": false, "autoAgents": false, "agentsMinutes": 5,
        "linkKeys": false,
        "extraDimming": true, "linkDisplays": false, "panelDisplay": "",
        "bluetoothSleep": false, "wifiSleep": false, "wallpaperFolder": "" }"#,
};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn flag(s: &Value, key: &str) -> bool {
    s[key].as_bool().unwrap_or(false)
}

/// The slider levels the user set, kept across restarts: a display that cannot be read
/// (a monitor whose luminance reply is always 0) has no other way to say where it stands.
fn levels_path(cx: &Ctx) -> PathBuf {
    cx.data_dir(ID).join("levels.json")
}

/// The brightness and contrast the panel last set, kept across restarts: a display
/// whose reply is unreadable has no other way to say where it stands.
fn panel_path(cx: &Ctx) -> PathBuf {
    cx.data_dir(ID).join("panel.json")
}

/// What `panel.json` holds: the level set per display, and the monitor name behind each id, so that
/// the same screen is found again after a reboot renumbers it.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PanelFile {
    levels: HashMap<String, (u8, u8)>,
    #[serde(default)]
    names: HashMap<String, String>,
}

fn load_panel(cx: &Ctx) -> PanelFile {
    let Ok(text) = std::fs::read_to_string(panel_path(cx)) else { return PanelFile::default() };
    if let Ok(file) = serde_json::from_str::<PanelFile>(&text) {
        return file;
    }
    // The first format was the bare map of display id to levels.
    let levels = serde_json::from_str::<HashMap<String, (u8, u8)>>(&text).unwrap_or_default();
    PanelFile { levels, names: HashMap::new() }
}

/// How long after a restore a hardware read is a monitor still coming up, not a change.
const RESTORE_GRACE: u64 = 8000;
/// A read sooner than this after one of our writes is that write echoing back.
const WRITE_SETTLE: u64 = 1500;

/// Runs `f` on a thread of its own and waits at most `limit` for it.
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok()
}

/// Only the last of a burst runs, `ms` after it.
fn later(turn: &Arc<AtomicU64>, ms: u64, f: impl FnOnce() + Send + 'static) {
    let mine = turn.fetch_add(1, Relaxed) + 1;
    let turn = turn.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        if turn.load(Relaxed) == mine {
            f();
        }
    });
}

// ── Keep awake ────────────────────────────────────────────────────────────────

struct Session {
    _request: power::Request,
    /// Dropping it ends the session's thread.
    _alive: mpsc::Sender<()>,
    started: u64,
    ends: Option<u64>,
    auto: bool,
}

/// One real pointer movement of one pixel and back, so that the idle timer restarts.
fn nudge() {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut at = POINT::default();
        if GetCursorPos(&mut at).is_err() {
            return;
        }
        let send = |dx: i32| {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        };
        send(1);
        std::thread::sleep(Duration::from_millis(80));
        // Back only if the pointer is where the nudge left it: never fight the user.
        let mut now = POINT::default();
        if GetCursorPos(&mut now).is_ok() && now.x == at.x + 1 && now.y == at.y {
            send(-1);
        }
    }
}

// ── Brightness ────────────────────────────────────────────────────────────────

/// What the backlight (percent) and the overlay (alpha) should be for a slider value.
fn plan(method: Method, internal: bool, extra: bool, s: f32) -> (Option<u8>, f32) {
    let s = s.clamp(0.0, 1.0);
    if method == Method::None {
        return (None, (1.0 - s) * MAX_ALPHA);
    }
    if !extra {
        return (Some((s * 100.0).round() as u8), 0.0);
    }
    // The built-in backlight is off at zero: its lowest step is 1.
    let floor = if internal { 1 } else { 0 };
    if s >= SPLIT {
        (Some((((s - SPLIT) / (1.0 - SPLIT) * 100.0).round() as u8).max(floor)), 0.0)
    } else {
        (Some(floor), (1.0 - s / SPLIT) * MAX_ALPHA)
    }
}

/// The tray icon's rectangle in physical pixels, whichever units the shell reported it in.
fn tray_box(rect: tauri::Rect) -> (i32, i32, i32, i32) {
    let (x, y) = match rect.position {
        tauri::Position::Physical(p) => (p.x, p.y),
        tauri::Position::Logical(p) => (p.x.round() as i32, p.y.round() as i32),
    };
    let (w, h) = match rect.size {
        tauri::Size::Physical(s) => (s.width as i32, s.height as i32),
        tauri::Size::Logical(s) => (s.width.round() as i32, s.height.round() as i32),
    };
    (x, y, w, h)
}

/// The slider value a backlight reading stands for (no overlay).
fn level_of(extra: bool, internal: bool, hw: u8) -> f32 {
    let hw = f32::from(hw) / 100.0;
    if extra && internal && hw <= 0.01 {
        SPLIT
    } else if extra {
        SPLIT + hw * (1.0 - SPLIT)
    } else {
        hw
    }
}

struct Job {
    method: Method,
    percent: u8,
    /// The slider value this write stands for; it becomes the last good one when it succeeds.
    level: f32,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<HashMap<String, Job>>,
    wake: Condvar,
    quit: AtomicBool,
}

#[derive(Default)]
struct Subs {
    display: Option<svc_sysevents::Sub>,
    power: Option<svc_sysevents::Sub>,
    sleep: Option<svc_sysevents::Sub>,
}

#[derive(Default)]
struct Inner {
    session: Mutex<Option<Session>>,
    /// A session started by a condition was stopped by hand: no new one until the conditions clear.
    suppressed: AtomicBool,
    /// How each display can be dimmed, as the last read found out.
    methods: Mutex<HashMap<String, Method>>,
    /// The slider value that last took (or was last shown), per display.
    levels: Mutex<HashMap<String, f32>>,
    /// The last backlight percent queued, to not write the same twice.
    queued: Mutex<HashMap<String, u8>>,
    fails: Mutex<HashMap<String, u8>>,
    /// display id -> window label of its overlay
    opened: Mutex<HashMap<String, String>>,
    alphas: Mutex<HashMap<String, f32>>,
    queue: Arc<Queue>,
    worker: Mutex<bool>,
    subs: Mutex<Subs>,
    display_turn: Arc<AtomicU64>,
    power_turn: Arc<AtomicU64>,
    agents_turn: Arc<AtomicU64>,
    /// An agent works now (told by the page), and when one last did (ms); the quiet minutes count from it.
    agents_busy: AtomicBool,
    agents_last: AtomicU64,
    /// The debounced write of `levels` to its file.
    level_turn: Arc<AtomicU64>,
    /// The panel's own icon in the notification area, the wheel over it, and the
    /// screen rectangle the icon last reported. All three live only while the
    /// feature runs.
    tray: Mutex<Option<TrayIcon>>,
    wheel: Mutex<Option<svc_input::Sub>>,
    tray_rect: Mutex<Option<(i32, i32, i32, i32)>>,
    /// Where the panel last stood on screen, in physical pixels, and the hook that hears a
    /// click outside it: a panel opened from the tray often never gets the focus to lose.
    panel_rect: Mutex<Option<(i32, i32, i32, i32)>>,
    panel_click: Mutex<Option<svc_input::Sub>>,
    /// The hook that puts the panel away on Escape, from the keyboard as the click does from the mouse.
    panel_escape: Mutex<Option<svc_input::Sub>>,
    /// Notches the wheel turned that are not written yet (+ up), the debounced write of them,
    /// and the lock that keeps two of those writes off the monitor at the same time.
    wheel_steps: AtomicI32,
    wheel_turn: Arc<AtomicU64>,
    wheel_lock: Mutex<()>,
    /// What the panel last set, per display: (brightness, contrast), both 0–100.
    panel: Mutex<HashMap<String, (u8, u8)>>,
    panel_turn: Arc<AtomicU64>,
    /// The monitor name behind each display id, to find the same screen after a reboot.
    names: Mutex<HashMap<String, String>>,
    /// What was last written or restored per display, and when: a read that disagrees is somebody
    /// else's change (the monitor's own buttons), not our own echo.
    last_write: Mutex<HashMap<String, (u8, u8, u64)>>,
    /// Until this moment a hardware read is a monitor still coming up after a restore.
    restoring: AtomicU64,
    /// When the panel was last shown, and when it last lost the focus. The panel goes away when
    /// the user clicks anywhere else, and that same click may be on its own tray icon: the two
    /// moments tell a close from a second open.
    panel_shown: AtomicU64,
    panel_blur: AtomicU64,
}

impl Inner {
    // ── sessions ──────────────────────────────────────────────────────────────

    fn session_json(&self) -> Value {
        match &*self.session.lock().unwrap() {
            Some(s) => json!({ "active": true, "startedAt": s.started, "endsAt": s.ends, "auto": s.auto }),
            None => json!({ "active": false }),
        }
    }

    fn battery_low(limit: u64) -> bool {
        let p = svc_sysevents::power_status();
        limit > 0 && !p.ac && p.percent.is_some_and(|c| u64::from(c) <= limit)
    }

    fn begin(self: &Arc<Self>, cx: &Ctx, minutes: u64, auto: bool) -> Result<(), String> {
        let s = cx.settings(ID);
        if auto && Self::battery_low(s["batteryLimit"].as_u64().unwrap_or(10)) {
            return Err("The battery is low.".into());
        }
        let request = power::Request::new(flag(&s, "allowDisplaySleep"))?;
        let (alive, gone) = mpsc::channel::<()>();
        let started = now_ms();
        // Replacing a session drops the old request after the new one exists: the PC never gets a gap.
        *self.session.lock().unwrap() =
            Some(Session { _request: request, _alive: alive, started, ends: (minutes > 0).then(|| started + minutes * 60_000), auto });
        cx.emit(ID, "session", self.session_json());
        let (me, cx) = (self.clone(), cx.clone());
        std::thread::spawn(move || me.watch(&cx, gone));
        Ok(())
    }

    /// The session's own thread: wakes for the end and every 30 s for the battery and the pointer.
    fn watch(self: &Arc<Self>, cx: &Ctx, gone: mpsc::Receiver<()>) {
        let mut jiggled = Instant::now();
        loop {
            let ends = self.session.lock().unwrap().as_ref().and_then(|s| s.ends);
            let wait = ends.map_or(30_000, |e| e.saturating_sub(now_ms()).clamp(250, 30_000));
            if !matches!(gone.recv_timeout(Duration::from_millis(wait)), Err(mpsc::RecvTimeoutError::Timeout)) {
                return;
            }
            let s = cx.settings(ID);
            let ends = self.session.lock().unwrap().as_ref().and_then(|s| s.ends);
            if ends.is_some_and(|e| now_ms() >= e) {
                self.end(cx, "done");
                return;
            }
            if Self::battery_low(s["batteryLimit"].as_u64().unwrap_or(10)) {
                self.end(cx, "battery");
                return;
            }
            let every = Duration::from_secs(s["jiggleMinutes"].as_u64().unwrap_or(5).clamp(1, 15) * 60);
            if flag(&s, "jiggle") && jiggled.elapsed() >= every {
                jiggled = Instant::now();
                nudge();
            }
        }
    }

    /// Releases the request. `reason`: "done", "battery", "auto" (conditions cleared), "manual", "off".
    fn end(self: &Arc<Self>, cx: &Ctx, reason: &str) {
        let Some(old) = self.session.lock().unwrap().take() else { return };
        drop(old);
        cx.emit(ID, "session", json!({ "active": false }));
        if matches!(reason, "done" | "battery") {
            cx.emit(ID, "ended", json!({ "reason": reason }));
        }
    }

    /// Whether the automatic conditions hold now; `None` when none is enabled.
    fn conditions(&self, cx: &Ctx) -> Option<bool> {
        let s = cx.settings(ID);
        let mut all = Vec::new();
        if flag(&s, "autoExternal") {
            // A failed read of the displays counts as "not connected", never as the last answer.
            all.push(display::list().iter().any(|d| !d.internal));
        }
        if flag(&s, "autoPower") {
            all.push(svc_sysevents::power_status().ac);
        }
        if flag(&s, "autoAgents") {
            let quiet = s["agentsMinutes"].as_u64().unwrap_or(5).min(120) * 60_000;
            all.push(self.agents_busy.load(Relaxed) || now_ms() < self.agents_last.load(Relaxed) + quiet);
        }
        if all.is_empty() {
            None
        } else if flag(&s, "autoAll") {
            Some(all.iter().all(|c| *c))
        } else {
            Some(all.iter().any(|c| *c))
        }
    }

    fn automate(self: &Arc<Self>, cx: &Ctx) {
        let ok = self.conditions(cx);
        let auto = self.session.lock().unwrap().as_ref().map(|s| s.auto);
        match (ok, auto) {
            (Some(true), None) => {
                if !self.suppressed.load(Relaxed) {
                    let _ = self.begin(cx, 0, true);
                }
            }
            (Some(true), _) => {}
            (Some(false), a) => {
                self.suppressed.store(false, Relaxed);
                if a == Some(true) {
                    self.end(cx, "auto");
                }
            }
            (None, a) => {
                self.suppressed.store(false, Relaxed);
                if a == Some(true) {
                    self.end(cx, "auto");
                }
            }
        }
    }

    // ── brightness ────────────────────────────────────────────────────────────

    fn extra(cx: &Ctx) -> bool {
        cx.settings(ID)["extraDimming"].as_bool().unwrap_or(true)
    }

    fn display_of(id: &str) -> Option<Display> {
        display::list().into_iter().find(|d| d.id == id)
    }

    fn method_of(&self, d: &Display) -> Method {
        let known = self.methods.lock().unwrap().get(&d.id).copied();
        known.unwrap_or_else(|| {
            let (m, _) = display::probe(d);
            self.methods.lock().unwrap().insert(d.id.clone(), m);
            m
        })
    }

    fn worker(self: &Arc<Self>, cx: &Ctx) {
        let mut running = self.worker.lock().unwrap();
        if *running {
            return;
        }
        *running = true;
        let (me, cx) = (self.clone(), cx.clone());
        std::thread::spawn(move || {
            let q = me.queue.clone();
            let mut last: HashMap<String, Instant> = HashMap::new();
            loop {
                let (id, job) = {
                    let mut jobs = q.jobs.lock().unwrap();
                    loop {
                        if q.quit.load(Relaxed) {
                            return;
                        }
                        if let Some(id) = jobs.keys().next().cloned() {
                            let job = jobs.remove(&id).unwrap();
                            break (id, job);
                        }
                        jobs = q.wake.wait(jobs).unwrap();
                    }
                };
                // 50 ms between two commands to one display: some monitors drop out when streamed faster.
                if let Some(t) = last.get(&id) {
                    std::thread::sleep(Duration::from_millis(50).saturating_sub(t.elapsed()));
                }
                let ok = Self::display_of(&id).is_some_and(|d| display::write(&d, job.method, job.percent));
                last.insert(id.clone(), Instant::now());
                me.written(&cx, &id, &job, ok);
            }
        });
    }

    /// Writes the levels down once the burst of a drag is over.
    fn save_levels(self: &Arc<Self>, cx: &Ctx) {
        let (me, cx) = (self.clone(), cx.clone());
        later(&self.level_turn, 400, move || {
            let levels = me.levels.lock().unwrap().clone();
            if let Err(e) = notch_core::storage::json_save_atomic(&levels_path(&cx), &levels) {
                notch_core::log::line(format!("{ID}: levels: {e}"));
            }
        });
    }

    // ── brightness and contrast panel ─────────────────────────────────────────

    /// Every display with the brightness and contrast the panel shows, and the one it controls.
    fn panel_state(&self, cx: &Ctx) -> Value {
        let remembered = self.panel.lock().unwrap().clone();
        let want = cx.settings(ID)["panelDisplay"].as_str().unwrap_or_default().to_string();
        let mut list: Vec<Value> = Vec::new();
        for d in display::list() {
            let (method, hw) = display::probe(&d);
            self.methods.lock().unwrap().insert(d.id.clone(), method);
            let mem = remembered.get(&d.id).copied();
            // The level the user left is what a wake restores: a monitor that came back at its own
            // default must not replace it with what it reports.
            let brightness = mem.map(|m| m.0).or(hw).unwrap_or(100);
            let contrast = if d.internal {
                mem.map_or(50, |m| m.1)
            } else {
                mem.map(|m| m.1).or_else(|| display::read_contrast(&d)).unwrap_or(50)
            };
            list.push(json!({
                "id": d.id, "name": d.name, "internal": d.internal, "primary": d.primary,
                "brightness": brightness, "contrast": contrast,
                "hasBrightness": method != Method::None,
                "hasContrast": !d.internal && method != Method::None,
                "method": method.name(),
            }));
        }
        let id_of = |row: &Value| row["id"].as_str().unwrap_or_default().to_string();
        // The chosen display if it is still here, else the primary one, else the first.
        let selected = if list.iter().any(|r| id_of(r) == want) {
            want
        } else {
            list.iter().find(|r| r["primary"].as_bool() == Some(true)).or_else(|| list.first()).map(id_of).unwrap_or_default()
        };
        json!({ "displays": list, "selected": selected })
    }

    /// One slider move from the panel, or one wheel notch over its icon.
    fn panel_set(self: &Arc<Self>, cx: &Ctx, id: &str, kind: &str, value: u8) -> Result<Value, String> {
        let d = Self::display_of(id).ok_or("That display is not connected any more.")?;
        let value = value.min(100);
        let mut pair = self.panel.lock().unwrap().get(id).copied().unwrap_or((100, 50));
        let ok = match kind {
            "brightness" => {
                let method = self.method_of(&d);
                if method == Method::None {
                    return Err("This display's brightness cannot be changed.".into());
                }
                let ok = display::write(&d, method, value);
                if ok {
                    pair.0 = value;
                }
                ok
            }
            "contrast" => {
                if d.internal {
                    return Err("A built-in display has no contrast control.".into());
                }
                let ok = display::write_contrast(&d, value);
                if ok {
                    pair.1 = value;
                }
                ok
            }
            _ => return Err("unknown setting".into()),
        };
        self.panel.lock().unwrap().insert(id.to_string(), pair);
        if ok {
            // Our own write: from now on a read that disagrees is somebody else's change.
            self.last_write.lock().unwrap().insert(id.to_string(), (pair.0, pair.1, now_ms()));
        }
        self.save_panel(cx);
        cx.emit(ID, "panel", json!({ "id": id, "brightness": pair.0, "contrast": pair.1, "ok": ok }));
        if kind == "brightness" {
            cx.emit(ID, "brightness", json!({ "id": id, "name": d.name, "level": f32::from(pair.0) / 100.0, "shortcut": true, "count": display::list().len() }));
        }
        Ok(json!({ "ok": ok, "brightness": pair.0, "contrast": pair.1 }))
    }

    /// Reads the display the panel controls again, from the hardware, without touching the others.
    /// The page compares what comes back with what it showed, and says whether anything moved. Its
    /// refresh button asks for this; the panel also asks silently while it is open, so that a change
    /// made at the monitor's own buttons shows up there by itself.
    fn panel_refresh(self: &Arc<Self>, cx: &Ctx) -> Value {
        let want = cx.settings(ID)["panelDisplay"].as_str().unwrap_or_default().to_string();
        let list = display::list();
        let Some(d) = list.iter().find(|d| d.id == want).or_else(|| list.iter().find(|d| d.primary)).or_else(|| list.first()) else {
            return Value::Null;
        };
        let (method, hw) = display::probe(d);
        self.methods.lock().unwrap().insert(d.id.clone(), method);
        let contrast = if d.internal { None } else { display::read_contrast(d) };
        let (brightness, contrast) = self.adopt(cx, d, hw, contrast);
        json!({
            "id": d.id, "brightness": brightness, "contrast": contrast,
            "hasBrightness": method != Method::None,
            "hasContrast": !d.internal && method != Method::None,
        })
    }

    /// What a read from the hardware means for the saved level. A write of ours still travelling to
    /// the panel, or a monitor still coming up after a restore, says nothing new; anything else is a
    /// change somebody made at the monitor itself, and becomes the level a wake puts back.
    fn adopt(self: &Arc<Self>, cx: &Ctx, d: &Display, hw: Option<u8>, contrast: Option<u8>) -> (u8, u8) {
        let saved = self.panel.lock().unwrap().get(&d.id).copied();
        let last = self.last_write.lock().unwrap().get(&d.id).copied();
        let (mut b, mut c) = saved.unwrap_or((hw.unwrap_or(100), contrast.unwrap_or(50)));
        let settling = now_ms() < self.restoring.load(Relaxed)
            || last.is_some_and(|(_, _, at)| now_ms().saturating_sub(at) < WRITE_SETTLE);
        if !settling {
            if let Some(v) = hw {
                if v.abs_diff(b) > 1 {
                    b = v;
                }
            }
            if let Some(v) = contrast {
                if v.abs_diff(c) > 1 {
                    c = v;
                }
            }
        }
        if saved != Some((b, c)) {
            self.panel.lock().unwrap().insert(d.id.clone(), (b, c));
            self.last_write.lock().unwrap().insert(d.id.clone(), (b, c, now_ms()));
            self.save_panel(cx);
        }
        (b, c)
    }

    fn save_panel(self: &Arc<Self>, cx: &Ctx) {
        let (me, cx) = (self.clone(), cx.clone());
        later(&self.panel_turn, 350, move || {
            let levels = me.panel.lock().unwrap().clone();
            let mut names = me.names.lock().unwrap().clone();
            // The name goes with the id: a reboot may give the same screen another number.
            for d in display::list() {
                names.insert(d.id.clone(), d.name.clone());
            }
            *me.names.lock().unwrap() = names.clone();
            let file = PanelFile { levels, names };
            if let Err(e) = notch_core::storage::json_save_atomic(&panel_path(&cx), &file) {
                notch_core::log::line(format!("{ID}: panel levels: {e}"));
            }
        });
    }

    /// Just before the screen goes away: what the monitor's own buttons changed is what the wake
    /// should bring back, so it is read now and kept as the saved level.
    fn remember(self: &Arc<Self>, cx: &Ctx) {
        for d in display::list() {
            let (method, hw) = display::probe(&d);
            self.methods.lock().unwrap().insert(d.id.clone(), method);
            let contrast = if d.internal { None } else { display::read_contrast(&d) };
            self.adopt(cx, &d, hw, contrast);
        }
    }

    fn restore(self: &Arc<Self>, cx: &Ctx) {
        let (me, cx) = (self.clone(), cx.clone());
        std::thread::spawn(move || me.restore_now(&cx));
    }

    /// Puts the saved level back on the displays that have one: a monitor that came back at its own
    /// default — a wake, a power cycle, a reboot — is where the user left it before anything is read.
    fn restore_now(self: &Arc<Self>, cx: &Ctx) {
        let saved = self.panel.lock().unwrap().clone();
        if saved.is_empty() {
            return;
        }
        let names = self.names.lock().unwrap().clone();
        // A read is a monitor still coming up, or the echo of these writes, until the grace ends.
        self.restoring.store(now_ms() + RESTORE_GRACE, Relaxed);
        let mut wrote = false;
        for d in display::list() {
            let Some((b, c)) = Self::saved_for(&saved, &names, &d) else { continue };
            let method = self.method_of(&d);
            if method == Method::None || !display::write(&d, method, b) {
                continue;
            }
            if !d.internal {
                let _ = display::write_contrast(&d, c);
            }
            self.panel.lock().unwrap().insert(d.id.clone(), (b, c));
            self.last_write.lock().unwrap().insert(d.id.clone(), (b, c, now_ms()));
            wrote = true;
        }
        if wrote {
            self.save_panel(cx);
        }
        // Monitors take a write and then drift back to their own default: read once it has landed.
        std::thread::sleep(Duration::from_millis(3000));
        for d in display::list() {
            let Some((b, c)) = Self::saved_for(&saved, &names, &d) else { continue };
            let (method, hw) = display::probe(&d);
            let Some(seen) = hw else { continue };
            if method == Method::None || seen.abs_diff(b) <= 2 {
                continue;
            }
            if display::write(&d, method, b) {
                if !d.internal {
                    let _ = display::write_contrast(&d, c);
                }
                self.last_write.lock().unwrap().insert(d.id.clone(), (b, c, now_ms()));
            }
        }
    }

    /// The saved level of `d`: by its id, or by the monitor's name when a reboot renumbered it.
    fn saved_for(saved: &HashMap<String, (u8, u8)>, names: &HashMap<String, String>, d: &Display) -> Option<(u8, u8)> {
        saved
            .get(&d.id)
            .copied()
            .or_else(|| saved.iter().find(|(id, _)| names.get(*id).is_some_and(|n| n == &d.name)).map(|(_, p)| *p))
    }

    /// A wheel turn over the tray icon: `steps` notches up (negative: down), two points each, on
    /// the display the panel controls. Only that display is touched.
    fn nudge(self: &Arc<Self>, cx: &Ctx, steps: i32) {
        // One write at a time: two DDC writes in flight at once is what made the level jump.
        let _one = self.wheel_lock.lock().unwrap();
        let list = display::list();
        let want = cx.settings(ID)["panelDisplay"].as_str().unwrap_or_default().to_string();
        let d = list
            .iter()
            .find(|d| d.id == want)
            .or_else(|| list.iter().find(|d| d.primary))
            .or_else(|| list.first());
        let Some(d) = d else { return };
        // A notch starts from the level last set: the hardware may still report the one before it,
        // and a read per notch is the slow part of a wheel turn. Only with nothing remembered is
        // the display read.
        let remembered = self.panel.lock().unwrap().get(&d.id).copied().map(|p| p.0);
        let mut method = self.method_of(d);
        let hw = if method == Method::None || remembered.is_none() {
            let (m, hw) = display::probe(d);
            method = m;
            self.methods.lock().unwrap().insert(d.id.clone(), m);
            hw
        } else {
            None
        };
        if method == Method::None {
            return;
        }
        let now = remembered.or(hw).unwrap_or(100);
        let to = (i32::from(now) + steps * i32::from(WHEEL_STEP)).clamp(0, 100) as u8;
        if to != now {
            let _ = self.panel_set(cx, &d.id, "brightness", to);
        }
    }

    /// Centred over the panel's own icon, just above it; with no icon reading yet, the bottom-right
    /// corner of the work area, which the window factory keeps on the screen either way.
    fn panel_anchor(&self) -> Option<Anchor> {
        if let Some((x, y, w, _)) = *self.tray_rect.lock().unwrap() {
            // The window's width is only known once it is built, so the middle of the icon is all
            // this can say (winfactory.rs).
            return Some(Anchor::Centred(x + w / 2, y - 8));
        }
        let list = display::list();
        let d = list.iter().find(|d| d.primary).or_else(|| list.first())?;
        Some(Anchor::Centred(d.work.2, d.work.3 - 8))
    }

    fn open_panel(self: &Arc<Self>, cx: &Ctx) -> Result<(), String> {
        let fresh = cx.app().get_webview_window(PANEL).is_none();
        let win = cx.window()
            .open(WindowSpec {
                label: PANEL.into(),
                feature: ID.into(),
                page: "panel".into(),
                w: 360.0,
                h: 300.0,
                frameless: true,
                topmost: true,
                monitor: Monitor::Cursor,
                anchor: self.panel_anchor(),
                ..Default::default()
            })?;
        // A panel at its icon, not a window of its own: no taskbar button (that button was the
        // Lintel icon the user saw pinned along with the brightness one), and the sun on what
        // is left of its frame.
        let _ = win.set_skip_taskbar(true);
        let _ = win.set_icon(trayicon::sun());
        self.panel_shown.store(now_ms(), Relaxed);
        // As the window factory left it, before anything can move it.
        if let (Ok(pos), Ok(size)) = (win.outer_position(), win.outer_size()) {
            *self.panel_rect.lock().unwrap() = Some((pos.x, pos.y, size.width as i32, size.height as i32));
        }
        if fresh {
            // It goes when it loses the focus: a click anywhere else puts it away. The window is
            // destroyed off this thread, which runs inside the window's own message callback.
            let me = self.clone();
            let cx = cx.clone();
            win.on_window_event(move |event| {
                if !matches!(event, tauri::WindowEvent::Focused(false)) {
                    return;
                }
                // A blur that lands while the window is still being put up is not a click away.
                if now_ms().saturating_sub(me.panel_shown.load(Relaxed)) < 400 {
                    return;
                }
                me.panel_blur.store(now_ms(), Relaxed);
                let cx = cx.clone();
                std::thread::spawn(move || cx.window().close(PANEL));
            });
        }
        Ok(())
    }

    fn toggle_panel(self: &Arc<Self>, cx: &Ctx) {
        match cx.app().get_webview_window(PANEL) {
            Some(win) if win.is_visible().unwrap_or(false) => {
                cx.window().close(PANEL);
                return;
            }
            Some(_) => {}
            None => {
                // The panel was just closed by the click that took the focus away — possibly this
                // very click, on its icon: it stays closed rather than opening again.
                if now_ms().saturating_sub(self.panel_blur.load(Relaxed)) < 350 {
                    return;
                }
            }
        }
        if let Err(e) = self.open_panel(cx) {
            notch_core::log::line(format!("{ID}: brightness panel: {e}"));
        }
    }

    /// The panel's own icon in the notification area. A click opens the panel;
    /// `wheel_on` turns the wheel over it into brightness steps.
    fn tray_on(self: &Arc<Self>, cx: &Ctx) {
        if self.tray.lock().unwrap().is_some() {
            return;
        }
        let me = self.clone();
        let cx2 = cx.clone();
        let mut builder = TrayIconBuilder::with_id(TRAY)
            .tooltip("Brightness and contrast")
            .show_menu_on_left_click(false)
            .on_tray_icon_event(move |_tray, event| match event {
                TrayIconEvent::Enter { rect, .. } | TrayIconEvent::Move { rect, .. } | TrayIconEvent::Leave { rect, .. } => {
                    *me.tray_rect.lock().unwrap() = Some(tray_box(rect));
                }
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } => {
                    // The panel opens above the icon where it is now, not where a hover last saw it.
                    *me.tray_rect.lock().unwrap() = Some(tray_box(rect));
                    let (me, cx) = (me.clone(), cx2.clone());
                    // A window must never be built on the main thread inside an event callback (winfactory.rs).
                    std::thread::spawn(move || me.toggle_panel(&cx));
                }
                _ => {}
            });
        builder = builder.icon(trayicon::sun());
        match builder.build(cx.app()) {
            Ok(tray) => {
                if let Ok(Some(rect)) = tray.rect() {
                    *self.tray_rect.lock().unwrap() = Some(tray_box(rect));
                }
                *self.tray.lock().unwrap() = Some(tray);
                // Next to the clock, and the only icon of this program pinned there: the island's
                // own icon is put back off the taskbar, whichever of them was pinned before.
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(1000));
                    trayicon::enforce_pin();
                });
            }
            Err(e) => notch_core::log::line(format!("{ID}: tray icon: {e}")),
        }
    }

    /// The wheel over the tray icon. The hook only compares the pointer with the
    /// rectangle the icon last reported and counts the notches; the write goes out once
    /// the wheel has stopped.
    fn wheel_on(self: &Arc<Self>, cx: &Ctx) {
        if self.wheel.lock().unwrap().is_some() {
            return;
        }
        let me = self.clone();
        let cx = cx.clone();
        // Windows counts a notch as 120. A free-spinning wheel and a touchpad send many smaller
        // steps: they are added up here, so one notch is one step however it arrives, and the rest
        // of a notch is dropped once the pointer leaves the icon.
        let rest = AtomicI32::new(0);
        let sub = svc_input::mouse(45, false, move |m: &Mouse| {
            if m.kind == MouseKind::Wheel && m.delta != 0 {
                let inside = me
                    .tray_rect
                    .lock()
                    .unwrap()
                    .is_some_and(|(x, y, w, h)| m.x >= x && m.x < x + w && m.y >= y && m.y < y + h);
                if !inside {
                    rest.store(0, Relaxed);
                } else {
                    let total = rest.fetch_add(m.delta, Relaxed) + m.delta;
                    let notches = total / 120;
                    rest.store(total - notches * 120, Relaxed);
                    if notches != 0 {
                        me.wheel_steps.fetch_add(notches.clamp(-20, 20), Relaxed);
                        let turn = me.wheel_turn.clone();
                        let (me, cx) = (me.clone(), cx.clone());
                        later(&turn, WHEEL_SETTLE, move || me.wheel_write(&cx));
                    }
                }
            }
            Verdict::Pass
        });
        *self.wheel.lock().unwrap() = Some(sub);
    }

    /// A click anywhere but on the panel puts it away. A panel opened from a tray click often
    /// never gets the focus, so it has none to lose and no blur to hear: this hook hears the
    /// press whatever window is in front. The window is destroyed beside the hook thread, and
    /// `panel_blur` tells `toggle_panel` that this very click on its icon must not open it again.
    fn panel_click_on(self: &Arc<Self>, cx: &Ctx) {
        if self.panel_click.lock().unwrap().is_some() {
            return;
        }
        let me = self.clone();
        let cx = cx.clone();
        let sub = svc_input::mouse(46, false, move |m: &Mouse| {
            if matches!(m.kind, MouseKind::Down(_)) {
                if let Some((x, y, w, h)) = *me.panel_rect.lock().unwrap() {
                    let inside = m.x >= x && m.x < x + w && m.y >= y && m.y < y + h;
                    if !inside && cx.app().get_webview_window(PANEL).is_some() {
                        me.panel_blur.store(now_ms(), Relaxed);
                        let cx = cx.clone();
                        std::thread::spawn(move || cx.window().close(PANEL));
                    }
                }
            }
            Verdict::Pass
        });
        *self.panel_click.lock().unwrap() = Some(sub);
    }

    /// Escape puts the panel away from the keyboard, as a click elsewhere does from the mouse.
    /// The key is only heard, not taken (`Verdict::Pass`): the window in front still gets it.
    fn panel_escape_on(self: &Arc<Self>, cx: &Ctx) {
        if self.panel_escape.lock().unwrap().is_some() {
            return;
        }
        let cx = cx.clone();
        let sub = svc_input::keyboard(46, false, move |k: &Key| {
            if k.down && k.vk == 0x1B && cx.app().get_webview_window(PANEL).is_some() {
                let cx = cx.clone();
                std::thread::spawn(move || cx.window().close(PANEL));
            }
            Verdict::Pass
        });
        *self.panel_escape.lock().unwrap() = Some(sub);
    }

    /// The notches one burst of the wheel left, written in a single go.
    fn wheel_write(self: &Arc<Self>, cx: &Ctx) {
        let steps = self.wheel_steps.swap(0, Relaxed);
        if steps != 0 {
            self.nudge(cx, steps);
        }
    }

    /// A hardware write finished. A failure puts the slider back to the last level that took.
    fn written(self: &Arc<Self>, cx: &Ctx, id: &str, job: &Job, ok: bool) {
        if ok {
            self.fails.lock().unwrap().remove(id);
            self.levels.lock().unwrap().insert(id.into(), job.level);
            self.save_levels(cx);
            return;
        }
        let strikes = {
            let mut f = self.fails.lock().unwrap();
            let n = f.entry(id.into()).or_default();
            *n += 1;
            *n
        };
        self.queued.lock().unwrap().remove(id);
        // Two failures: the display's backlight is not ours to move; the overlay still is.
        if strikes >= 2 {
            self.methods.lock().unwrap().insert(id.into(), Method::None);
        }
        let Some(d) = Self::display_of(id) else { return };
        let good = self.levels.lock().unwrap().get(id).copied().unwrap_or(1.0);
        let (_, alpha) = plan(self.method_of(&d), d.internal, Self::extra(cx), good);
        self.overlay(cx, &d, alpha);
        cx.emit(ID, "brightness", json!({ "id": id, "name": d.name, "level": good, "failed": true, "method": self.method_of(&d).name() }));
        if strikes >= 2 {
            cx.emit(ID, "displays", json!({}));
        }
    }

    /// Moves one display to a slider value. The overlay follows at once; the backlight is queued.
    fn apply(self: &Arc<Self>, cx: &Ctx, d: &Display, s: f32) {
        let method = self.method_of(d);
        let (hw, alpha) = plan(method, d.internal, Self::extra(cx), s);
        // Brighter: the overlay goes first, so that a late backlight write never leaves the screen black.
        self.overlay(cx, d, alpha);
        match hw {
            Some(percent) => {
                if self.queued.lock().unwrap().insert(d.id.clone(), percent) == Some(percent) {
                    // Same backlight as already asked for: only the overlay moved.
                    self.levels.lock().unwrap().insert(d.id.clone(), s);
                    return;
                }
                self.worker(cx);
                self.queue.jobs.lock().unwrap().insert(d.id.clone(), Job { method, percent, level: s });
                self.queue.wake.notify_one();
            }
            None => {
                self.levels.lock().unwrap().insert(d.id.clone(), s);
            }
        }
    }

    /// The dimming overlay of a display: a click-through window, there only while alpha > 0.
    fn overlay(self: &Arc<Self>, cx: &Ctx, d: &Display, alpha: f32) {
        let label = format!("dim-{}", d.id.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>());
        let was = self.opened.lock().unwrap().contains_key(&d.id);
        if alpha < 0.004 {
            self.alphas.lock().unwrap().remove(&d.id);
            if was {
                cx.emit(ID, "dim", json!({ "label": label, "alpha": 0.0 }));
                cx.window().close(&label);
                self.opened.lock().unwrap().remove(&d.id);
                self.sync_subs(cx);
            }
            return;
        }
        self.alphas.lock().unwrap().insert(d.id.clone(), alpha);
        if !was {
            let spec = WindowSpec {
                label: label.clone(),
                feature: ID.into(),
                page: "dim".into(),
                w: 200.0,
                h: 200.0,
                frameless: true,
                topmost: true,
                no_activate: true,
                exclude_from_capture: true,
                transparent: true,
                monitor: Monitor::Primary,
                anchor: None,
            };
            match cx.window().open(spec) {
                Ok(win) => {
                    // Click-through before it is anywhere: it can never take the pointer or a key.
                    let _ = win.set_ignore_cursor_events(true);
                    let _ = win.set_position(PhysicalPosition::new(d.x, d.y));
                    let _ = win.set_size(PhysicalSize::new(d.w as u32, d.h as u32));
                }
                Err(e) => {
                    notch_core::log::line(format!("{ID}: dimming overlay: {e}"));
                    self.alphas.lock().unwrap().remove(&d.id);
                    return;
                }
            }
            self.opened.lock().unwrap().insert(d.id.clone(), label.clone());
            self.sync_subs(cx);
        }
        cx.emit(ID, "dim", json!({ "label": label, "alpha": alpha }));
    }

    fn close_overlays(&self, cx: &Ctx) {
        let all: Vec<String> = self.opened.lock().unwrap().drain().map(|(_, l)| l).collect();
        for label in all {
            cx.emit(ID, "dim", json!({ "label": label, "alpha": 0.0 }));
            cx.window().close(&label);
        }
        self.alphas.lock().unwrap().clear();
    }

    /// Displays came or went: overlays follow their display or go, the page reads again.
    fn displays_changed(self: &Arc<Self>, cx: &Ctx) {
        self.methods.lock().unwrap().clear();
        let now = display::list();
        let open: Vec<(String, String)> = self.opened.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (id, label) in open {
            match now.iter().find(|d| d.id == id) {
                Some(d) => {
                    if let Some(win) = tauri::Manager::get_webview_window(cx.app(), &label) {
                        let _ = win.set_position(PhysicalPosition::new(d.x, d.y));
                        let _ = win.set_size(PhysicalSize::new(d.w as u32, d.h as u32));
                    }
                }
                None => {
                    cx.emit(ID, "dim", json!({ "label": label, "alpha": 0.0 }));
                    cx.window().close(&label);
                    self.opened.lock().unwrap().remove(&id);
                    self.alphas.lock().unwrap().remove(&id);
                }
            }
        }
        // A write queued for a display that went away is never sent.
        self.queue.jobs.lock().unwrap().retain(|id, _| now.iter().any(|d| &d.id == id));
        self.queued.lock().unwrap().retain(|id, _| now.iter().any(|d| &d.id == id));
        self.sync_subs(cx);
        cx.emit(ID, "displays", json!({}));
        self.automate(cx);
        // A monitor that was switched off and on, or plugged back in, gets the saved level again.
        self.restore(cx);
    }

    /// The display under the pointer, one step brighter or dimmer.
    fn step(self: &Arc<Self>, cx: &Ctx, up: bool) {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut at = POINT::default();
        if unsafe { GetCursorPos(&mut at) }.is_err() {
            return;
        }
        // Only a display that is there and holds the pointer: no guess when the pointer is on none.
        let all = display::list();
        let count = all.len();
        let Some(d) = all.into_iter().find(|d| at.x >= d.x && at.x < d.x + d.w && at.y >= d.y && at.y < d.y + d.h) else { return };
        let mut next = 0.0;
        // With "linkKeys" every display moves by the same step, so the gaps between them stay (until one hits an end).
        let linked = flag(&cx.settings(ID), "linkKeys");
        for o in display::list().into_iter().filter(|o| o.id == d.id || linked) {
            let known = self.levels.lock().unwrap().get(&o.id).copied();
            let now = known.unwrap_or_else(|| {
                let (m, hw) = display::probe(&o);
                self.methods.lock().unwrap().insert(o.id.clone(), m);
                hw.map_or(1.0, |hw| level_of(Self::extra(cx), o.internal, hw))
            });
            let to = (now + if up { STEP } else { -STEP }).clamp(0.0, 1.0);
            self.apply(cx, &o, to);
            if o.id == d.id {
                next = to;
            }
        }
        cx.emit(ID, "brightness", json!({ "id": d.id, "name": d.name, "level": next, "shortcut": true, "count": count }));
    }

    /// One step now; then, while the keys stay down, a step every 100 ms. The shortcut registry
    /// registers hot keys without repeat, so it reports one press however long they are held.
    /// The keys watched are the ones bound in settings.json (read only), the defaults if none.
    fn hold_step(self: &Arc<Self>, cx: &Ctx, up: bool) {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        static HELD: AtomicBool = AtomicBool::new(false);
        self.step(cx, up);
        // A binding that cannot be read, or has a key we cannot watch, steps once.
        let Some(keys) = bound_keys(if up { "brighter" } else { "dimmer" }) else { return };
        if HELD.swap(true, Relaxed) {
            return;
        }
        let (me, cx) = (self.clone(), cx.clone());
        std::thread::spawn(move || {
            let down = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
            std::thread::sleep(std::time::Duration::from_millis(400));
            while keys.iter().all(|&(a, b)| down(a) || down(b)) {
                me.step(&cx, up);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            HELD.store(false, Relaxed);
        });
    }

    // ── radios at sleep ───────────────────────────────────────────────────────

    fn journal(cx: &Ctx) -> PathBuf {
        cx.data_dir(ID).join("radios.json")
    }

    /// Before sleep: the radios that are on go off, and the debt is written down first.
    fn radios_off(cx: &Ctx) {
        let s = cx.settings(ID);
        let path = Self::journal(cx);
        for (kind, key, name) in [(Kind::Bluetooth, "bluetoothSleep", "bluetooth"), (Kind::Wifi, "wifiSleep", "wifi")] {
            if !flag(&s, key) {
                continue;
            }
            if radio::states(kind).is_ok_and(|on| on.contains(&true)) {
                let mut owed: HashMap<String, bool> = notch_core::storage::json_load(&path);
                owed.insert(name.into(), true);
                let _ = notch_core::storage::json_save_atomic(&path, &owed);
                if let Err(e) = radio::set(kind, false) {
                    notch_core::log::line(format!("{ID}: {name} off: {e}"));
                }
            }
        }
    }

    /// At wake, at start and at quit: a radio this feature switched off comes back on,
    /// unless the user already did it.
    fn radios_back(cx: &Ctx) {
        let path = Self::journal(cx);
        let mut owed: HashMap<String, bool> = notch_core::storage::json_load(&path);
        owed.retain(|_, v| *v);
        if owed.is_empty() {
            return;
        }
        for (kind, name) in [(Kind::Bluetooth, "bluetooth"), (Kind::Wifi, "wifi")] {
            if !owed.contains_key(name) {
                continue;
            }
            // The radios are slow to come back after a wake: a few tries.
            for _ in 0..4 {
                match radio::states(kind) {
                    Ok(on) if on.contains(&true) || on.is_empty() => break,
                    Ok(_) if radio::set(kind, true).is_ok() => break,
                    _ => std::thread::sleep(Duration::from_secs(2)),
                }
            }
            owed.remove(name);
        }
        let _ = notch_core::storage::json_save_atomic(&path, &owed);
    }

    // ── what listens ──────────────────────────────────────────────────────────

    /// Subscribes to exactly what the settings and the open overlays need.
    fn sync_subs(self: &Arc<Self>, cx: &Ctx) {
        let s = cx.settings(ID);
        let want_power = flag(&s, "autoPower");
        let mut subs = self.subs.lock().unwrap();
        // Displays and the session are watched the whole time the feature runs: a monitor that comes
        // back, a wake or a reboot is given the saved brightness. Neither is a timer.
        if subs.display.is_none() {
            let (me, cx) = (self.clone(), cx.clone());
            subs.display = Some(svc_sysevents::on_display_change(move || {
                // A burst of changes is read once, after it.
                let (me2, cx2) = (me.clone(), cx.clone());
                later(&me.display_turn, 1500, move || me2.displays_changed(&cx2));
            }));
        }
        if !want_power {
            subs.power = None;
        } else if subs.power.is_none() {
            let (me, cx) = (self.clone(), cx.clone());
            subs.power = Some(svc_sysevents::on_power(move |_| {
                let (me2, cx2) = (me.clone(), cx.clone());
                later(&me.power_turn, 1000, move || me2.automate(&cx2));
            }));
        }
        if subs.sleep.is_none() {
            let (me, cx) = (self.clone(), cx.clone());
            subs.sleep = Some(svc_sysevents::on_session(move |ev| match ev {
                // Held here for up to 2.5 s, so that the radios are off before Windows suspends.
                Sleep::Sleep => {
                    let cx = cx.clone();
                    let _ = within(Duration::from_millis(2500), move || Self::radios_off(&cx));
                }
                // The screen goes away: what the monitor's buttons changed is what the wake brings back.
                Sleep::DisplayOff => {
                    let (me, cx) = (me.clone(), cx.clone());
                    std::thread::spawn(move || me.remember(&cx));
                }
                // The monitor comes back at its own default: the saved level goes on again.
                Sleep::DisplayOn => {
                    let (me, cx) = (me.clone(), cx.clone());
                    me.restore(&cx);
                }
                Sleep::Resume => {
                    let (me, cx) = (me.clone(), cx.clone());
                    let radios = cx.clone();
                    std::thread::spawn(move || Self::radios_back(&radios));
                    me.restore(&cx);
                }
                _ => {}
            }));
        }
    }
}

// ── The feature ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct DisplaysKeepawake {
    inner: Arc<Inner>,
    running: AtomicBool,
}

/// Virtual keys (a pair: left or right of a modifier) of an accelerator such as "Ctrl+Alt+PageUp".
pub fn vks_of(keys: &str) -> Option<Vec<(u16, u16)>> {
    let mut out = Vec::new();
    for t in keys.split('+').map(str::trim) {
        let up = t.to_ascii_uppercase();
        let vk = match up.as_str() {
            "CTRL" | "CONTROL" | "COMMANDORCONTROL" | "CMDORCTRL" => (0xA2, 0xA3),
            "ALT" | "OPTION" => (0xA4, 0xA5),
            "SHIFT" => (0xA0, 0xA1),
            "SUPER" | "META" | "WIN" | "CMD" | "COMMAND" => (0x5B, 0x5C),
            _ => {
                let k = match up.as_str() {
                    "SPACE" => 0x20, "ENTER" | "RETURN" => 0x0D, "TAB" => 0x09, "BACKSPACE" => 0x08,
                    "DELETE" | "DEL" => 0x2E, "INSERT" => 0x2D, "HOME" => 0x24, "END" => 0x23,
                    "PAGEUP" => 0x21, "PAGEDOWN" => 0x22, "LEFT" | "ARROWLEFT" => 0x25,
                    "UP" | "ARROWUP" => 0x26, "RIGHT" | "ARROWRIGHT" => 0x27, "DOWN" | "ARROWDOWN" => 0x28,
                    _ if up.len() == 1 && up.as_bytes()[0].is_ascii_alphanumeric() => u16::from(up.as_bytes()[0]),
                    _ if up.len() >= 2 && up.starts_with('F') => match up[1..].parse::<u16>() {
                        Ok(n @ 1..=24) => 0x6F + n,
                        _ => return None,
                    },
                    _ => return None,
                };
                (k, k)
            }
        };
        out.push(vk);
    }
    (out.len() >= 2).then_some(out)
}

/// The keys the user bound to a shortcut of this feature, or its default; None if not watchable.
fn bound_keys(id: &str) -> Option<Vec<(u16, u16)>> {
    let stored: Value = std::fs::read_to_string(notch_core::win::config_dir().join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let bound = &stored["shortcuts"][format!("{ID}:{id}")];
    if bound["enabled"].as_bool() == Some(false) {
        return None;
    }
    let default = if id == "brighter" { "Ctrl+Alt+PageUp" } else { "Ctrl+Alt+PageDown" };
    vks_of(bound["keys"].as_str().unwrap_or(default))
}

fn name_of(p: &std::path::Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn radio_word(kind: Kind) -> &'static str {
    match radio::states(kind) {
        Ok(on) if on.is_empty() => "none",
        Ok(on) if on.contains(&true) => "on",
        Ok(_) => "off",
        Err(_) => "denied",
    }
}

impl Feature for DisplaysKeepawake {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        if self.running.swap(true, Relaxed) {
            return Ok(());
        }
        let inner = self.inner.clone();
        inner.queue.quit.store(false, Relaxed);
        *inner.levels.lock().unwrap() = notch_core::storage::json_load(&levels_path(cx));
        let panel = load_panel(cx);
        *inner.panel.lock().unwrap() = panel.levels;
        *inner.names.lock().unwrap() = panel.names;
        inner.tray_on(cx);
        inner.wheel_on(cx);
        inner.panel_click_on(cx);
        inner.panel_escape_on(cx);
        inner.sync_subs(cx);
        let (boot, cx) = (cx.clone(), cx.clone());
        let rest = inner.clone();
        // A radio switched off before a shutdown while asleep comes back first of all.
        std::thread::spawn(move || {
            DisplaysKeepawake::radios_debt(&cx);
            inner.automate(&cx);
        });
        // The monitors are not ready the instant the app starts: the saved level goes back after a moment.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            rest.restore_now(&boot);
        });
        Ok(())
    }

    fn stop(&self, cx: &Ctx) {
        if !self.running.swap(false, Relaxed) {
            return;
        }
        let inner = &self.inner;
        inner.end(cx, "off");
        inner.close_overlays(cx);
        // The panel's icon, its wheel, its click-away and its Escape hook go with the feature; dropping them removes them.
        let tray = inner.tray.lock().unwrap().take();
        let wheel = inner.wheel.lock().unwrap().take();
        let click = inner.panel_click.lock().unwrap().take();
        let escape = inner.panel_escape.lock().unwrap().take();
        *inner.tray_rect.lock().unwrap() = None;
        *inner.panel_rect.lock().unwrap() = None;
        drop(tray);
        drop(wheel);
        drop(click);
        drop(escape);
        *inner.subs.lock().unwrap() = Subs::default();
        inner.queue.quit.store(true, Relaxed);
        inner.queue.jobs.lock().unwrap().clear();
        inner.queue.wake.notify_all();
        *inner.worker.lock().unwrap() = false;
        inner.queued.lock().unwrap().clear();
        let cx2 = cx.clone();
        let _ = within(Duration::from_secs(3), move || Inner::radios_back(&cx2));
    }

    fn settings_changed(&self, cx: &Ctx, settings: &Value) {
        let inner = &self.inner;
        inner.sync_subs(cx);
        if let Some(s) = inner.session.lock().unwrap().as_mut() {
            // The display request follows "allow display sleep" at once.
            if let Ok(r) = power::Request::new(flag(settings, "allowDisplaySleep")) {
                s._request = r;
            }
        }
        if !flag(settings, "extraDimming") {
            inner.close_overlays(cx);
            inner.queued.lock().unwrap().clear();
        }
        let inner = inner.clone();
        let cx = cx.clone();
        std::thread::spawn(move || inner.automate(&cx));
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let inner = &self.inner;
        match method {
            // ── keep awake
            "state" => Ok(inner.session_json()),
            "start" => {
                let minutes = args["minutes"].as_u64().unwrap_or(0).min(24 * 60);
                inner.suppressed.store(false, Relaxed);
                inner.begin(cx, minutes, false)?;
                cx.patch_settings(ID, json!({ "minutes": minutes }));
                Ok(inner.session_json())
            }
            "extend" => {
                let add = args["minutes"].as_u64().unwrap_or(15).min(24 * 60) * 60_000;
                if let Some(s) = inner.session.lock().unwrap().as_mut() {
                    // Open-ended sessions stay open-ended; the others end later.
                    if let Some(e) = s.ends {
                        s.ends = Some(e.max(now_ms()) + add);
                    }
                }
                cx.emit(ID, "session", inner.session_json());
                Ok(inner.session_json())
            }
            "stop" => {
                // Stopped by hand while the conditions still hold: no new automatic session until they clear.
                inner.suppressed.store(inner.conditions(cx) == Some(true), Relaxed);
                inner.end(cx, "manual");
                Ok(inner.session_json())
            }
            "toggle" => {
                if inner.session.lock().unwrap().is_some() {
                    self.call(cx, "stop", Value::Null)
                } else {
                    let minutes = cx.settings(ID)["minutes"].as_u64().unwrap_or(60);
                    self.call(cx, "start", json!({ "minutes": minutes }))
                }
            }
            // The page tells whether an agent works; the quiet minutes after the last one count from here.
            "agents" => {
                let busy = args["working"].as_bool().unwrap_or(false);
                if busy {
                    inner.agents_last.store(now_ms(), Relaxed);
                } else if inner.agents_busy.load(Relaxed) {
                    inner.agents_last.store(now_ms(), Relaxed);
                    // The condition lapses once the quiet time has passed.
                    let quiet = cx.settings(ID)["agentsMinutes"].as_u64().unwrap_or(5).min(120) * 60_000;
                    let (me, cx2) = (inner.clone(), cx.clone());
                    later(&inner.agents_turn, quiet + 500, move || me.automate(&cx2));
                }
                inner.agents_busy.store(busy, Relaxed);
                if flag(&cx.settings(ID), "autoAgents") {
                    let (me, cx2) = (inner.clone(), cx.clone());
                    std::thread::spawn(move || me.automate(&cx2));
                }
                Ok(Value::Null)
            }
            "shortcut" => {
                match args["id"].as_str() {
                    Some("toggle") => return self.call(cx, "toggle", Value::Null),
                    Some(id @ ("brighter" | "dimmer")) => inner.hold_step(cx, id == "brighter"),
                    Some("panel") => inner.toggle_panel(cx),
                    _ => {}
                }
                Ok(Value::Null)
            }
            // ── displays
            "displays" => {
                let extra = Inner::extra(cx);
                let out: Vec<Value> = display::list()
                    .iter()
                    .map(|d| {
                        let (m, hw) = display::probe(d);
                        // Fresh reads: a display that was marked unsupported gets another chance on reopening.
                        inner.methods.lock().unwrap().insert(d.id.clone(), m);
                        inner.fails.lock().unwrap().remove(&d.id);
                        let remembered = inner.levels.lock().unwrap().get(&d.id).copied();
                        let level = match (remembered, hw) {
                            // What the user left, if the hardware still agrees.
                            (Some(r), Some(hw)) => match plan(m, d.internal, extra, r).0 {
                                Some(p) if p.abs_diff(hw) <= 3 => r,
                                _ => level_of(extra, d.internal, hw),
                            },
                            (Some(r), None) => r,
                            (None, Some(hw)) => level_of(extra, d.internal, hw),
                            (None, None) => 1.0,
                        };
                        inner.levels.lock().unwrap().insert(d.id.clone(), level);
                        json!({ "id": d.id, "name": d.name, "internal": d.internal, "primary": d.primary, "method": m.name(), "level": level })
                    })
                    .collect();
                Ok(json!(out))
            }
            "set" => {
                let id = args["id"].as_str().ok_or("no display")?;
                let level = args["level"].as_f64().ok_or("no level")? as f32;
                let d = Inner::display_of(id).ok_or("That display is not connected any more.")?;
                let before = inner.levels.lock().unwrap().get(id).copied().unwrap_or(1.0);
                inner.apply(cx, &d, level);
                inner.levels.lock().unwrap().entry(id.to_string()).or_insert(level);
                let mut moved = vec![json!({ "id": id, "level": level })];
                if flag(&cx.settings(ID), "linkDisplays") {
                    // The others move by the same amount.
                    let delta = level - before;
                    for o in display::list().iter().filter(|o| o.id != id) {
                        let was = inner.levels.lock().unwrap().get(&o.id).copied().unwrap_or(1.0);
                        let to = (was + delta).clamp(0.0, 1.0);
                        inner.apply(cx, o, to);
                        moved.push(json!({ "id": o.id, "level": to }));
                    }
                }
                Ok(json!(moved))
            }
            "dimAlpha" => {
                let label = args["label"].as_str().unwrap_or_default();
                let id = inner.opened.lock().unwrap().iter().find(|(_, l)| l.as_str() == label).map(|(k, _)| k.clone());
                Ok(json!(id.and_then(|id| inner.alphas.lock().unwrap().get(&id).copied()).unwrap_or(0.0)))
            }
            // ── brightness and contrast panel
            "panel" => Ok(inner.panel_state(cx)),
            "panelRefresh" | "panelPoll" => Ok(inner.panel_refresh(cx)),
            "panelSet" => {
                let id = args["id"].as_str().ok_or("no display")?;
                let kind = args["kind"].as_str().unwrap_or("brightness");
                let value = args["value"].as_u64().unwrap_or(0).min(100) as u8;
                inner.panel_set(cx, id, kind, value)
            }
            "panelSelect" => {
                let id = args["id"].as_str().unwrap_or_default().to_string();
                cx.patch_settings(ID, json!({ "panelDisplay": id }));
                Ok(inner.panel_state(cx))
            }
            "open" => {
                inner.open_panel(cx)?;
                Ok(Value::Null)
            }
            "close" => {
                cx.window().close(PANEL);
                Ok(Value::Null)
            }
            "nudge" => {
                let steps = args["steps"].as_i64().unwrap_or(1).clamp(1, 20) as i32;
                inner.nudge(cx, if args["up"].as_bool().unwrap_or(true) { steps } else { -steps });
                Ok(Value::Null)
            }
            "tray" => {
                if args["id"].as_str() == Some("panel") {
                    inner.toggle_panel(cx);
                }
                Ok(Value::Null)
            }
            // ── wallpaper
            "wallpapers" => {
                let own = wall::own_dir(cx.settings(ID)["wallpaperFolder"].as_str().unwrap_or(""));
                let (sys, mine) = wall::list(&own);
                let row = |p: &PathBuf| json!({ "path": p.to_string_lossy(), "name": name_of(p) });
                Ok(json!({ "system": sys.iter().map(row).collect::<Vec<_>>(), "own": mine.iter().map(row).collect::<Vec<_>>(), "folder": own.to_string_lossy() }))
            }
            "wallThumb" | "wallSet" => {
                let own = wall::own_dir(cx.settings(ID)["wallpaperFolder"].as_str().unwrap_or(""));
                let path = PathBuf::from(args["path"].as_str().ok_or("no picture")?);
                if !wall::allowed(&path, &own) {
                    return Err("That is not one of the wallpaper pictures.".into());
                }
                if method == "wallSet" {
                    wall::set(&path)?;
                    return Ok(Value::Null);
                }
                let png = svc_shell::thumbnail(&path, 160)?;
                Ok(json!(format!("data:image/png;base64,{}", base64(&png))))
            }
            // ── radios
            "radios" => Ok(json!({ "bluetooth": radio_word(Kind::Bluetooth), "wifi": radio_word(Kind::Wifi) })),
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/displays-keepawake/strings.json")
    }
}

impl DisplaysKeepawake {
    fn radios_debt(cx: &Ctx) {
        Inner::radios_back(cx);
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(DisplaysKeepawake::default())
}
