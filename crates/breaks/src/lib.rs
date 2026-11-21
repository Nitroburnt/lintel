// Breaks — eye and movement break reminders, the fade that can go with them, and screen time.
// The contract is docs/ARCHITECTURE.md §2: nothing runs before `start`, everything is gone after
// `stop`, and every command of the feature arrives through `call`.
//
// Everything inside is off until the person turns it on, and with everything off the feature runs
// nothing at all. With a reminder or screen time on there is one thread that wakes once a second
// (it sleeps through a lock or a sleep, woken by the session event), and two subscriptions of
// svc-sysevents: the session, and the window in front (its WinEvent hook, never a poll).
//
// Events: `state` { break: { kind, left, total } | null, eye, move, held, fade, ev? }, where
// `eye` and `move` are the seconds at the PC until the next break of that kind (null = off) and
// `ev` { phase: "soon" | "held" | "start" | "end", kind, why } is what just happened.
// Commands: `state`, `skip`, `snooze`, `take` { kind }, `usage` { day }, `app` { key, day },
// `icons` { keys }, `open` { key }, `reveal` { key }, `hide` { key }, `unhide` { key },
// `category` { key, cat }, `clear`, `window` (the report, in a window of its own), `close`.
//
// Also `counts` (today's breaks, by kind and by how they ended), `paused` (seconds left of a pause
// asked for from the island) and `fade`. The `ev` phase `budget` is the one notice a day that
// crosses the screen-time target. Commands `pause` { mins }, `resume`, `undo`, `forgetDay` { day },
// `forgetApp` { key }, `export`, `report`; `window` takes the day and the category to open on.

pub mod fade;
pub mod rule;
pub mod stats;
pub mod sys;
pub mod usage;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use fade::Fade;
use notch_core::{log, Ctx, Feature, Group, Manifest, Use, WindowSpec};
use rule::{Config, Ev, Kind, Plan, Rule};
use serde_json::{json, Value};
use stats::Stats;
use svc_sysevents::{Foreground, Session};
use tauri::LogicalSize;
use usage::{Prefs, Usage};

const ID: &str = "breaks";
/// The screen time report's window (winfactory.rs): bigger than any section, and resizable.
const WINDOW: &str = "breaks-screen";
/// The smallest the report gets, in logical pixels: the list, the ring and the chart still fit.
const LEAST: (f64, f64) = (760.0, 560.0);
/// No key and no pointer for this long, with nothing full screen: the person is away.
pub const IDLE_AFTER: u64 = 120;
/// With the fade on, the island says so this many seconds before the screens go dark.
const WARN: u64 = 10;
/// A tick this late was a sleep nobody announced: those seconds were spent away.
const GAP: u64 = 5;
/// How long "Undo" stands after a clear or a delete.
const UNDO_SECS: u64 = 20;

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Breaks and screen time",
    group: Group::Tools,
    default_on: false,
    // The fade reads Esc and clicks while it is on screen.
    uses: &[Use::InputHook],
    shortcuts: &[],
    tray: &[],
    // eyeEvery, moveEvery, moveMins, snooze: minutes. eyeSecs: seconds.
    defaults: r#"{ "eye": false, "eyeEvery": 20, "eyeSecs": 20, "move": false, "moveEvery": 50, "moveMins": 2, "snooze": 5, "holdCall": true, "holdFullscreen": true, "quietHours": false, "quietFrom": "22:00", "quietTo": "07:00", "quietSystem": true, "fade": false, "sound": true, "screenTime": false, "budget": 0, "budgetCat": "", "idleMins": 2, "historyDays": 30, "icons": true, "windowTitles": false, "hidden": [], "cats": {} }"#,
};

#[derive(Clone, Default)]
struct Cfg {
    rule: Config,
    fade: bool,
    hold_call: bool,
    hold_fullscreen: bool,
    /// The person's own quiet window, and Windows' own (Focus Assist, a presentation).
    quiet: bool,
    quiet_from: u32,
    quiet_to: u32,
    quiet_system: bool,
    screen_time: bool,
    /// The day's screen-time target in seconds, and the category it is about; 0 = none.
    budget: u64,
    budget_cat: Option<String>,
    /// The absence that takes back the seconds already counted, in seconds.
    idle_after: u64,
    /// How many days of screen time and break counts are kept; 0 keeps every day.
    history_days: usize,
    /// Window titles are read and kept: off unless the person turns them on.
    titles: bool,
    /// What the screen time page needs: hidden apps, the person's categories, icons and titles.
    prefs: Prefs,
}

impl Cfg {
    fn read(s: &Value) -> Cfg {
        let on = |key: &str| s[key].as_bool().unwrap_or(false);
        let num = |key: &str, min: u64, max: u64, default: u64| s[key].as_f64().map_or(default, |v| (v as u64).clamp(min, max));
        let fade = on("fade");
        Cfg {
            rule: Config {
                eye: Plan { on: on("eye"), every: num("eyeEvery", 1, 180, 20) * 60, secs: num("eyeSecs", 5, 600, 20) },
                mov: Plan { on: on("move"), every: num("moveEvery", 1, 240, 50) * 60, secs: num("moveMins", 1, 30, 2) * 60 },
                snooze: num("snooze", 1, 60, 5) * 60,
                warn: if fade { WARN } else { 0 },
            },
            fade,
            hold_call: on("holdCall"),
            hold_fullscreen: on("holdFullscreen"),
            quiet: on("quietHours"),
            quiet_from: rule::minutes_of(s["quietFrom"].as_str().unwrap_or_default()).unwrap_or(22 * 60),
            quiet_to: rule::minutes_of(s["quietTo"].as_str().unwrap_or_default()).unwrap_or(7 * 60),
            quiet_system: s["quietSystem"].as_bool().unwrap_or(true),
            screen_time: on("screenTime"),
            budget: num("budget", 0, 24 * 60, 0) * 60,
            budget_cat: s["budgetCat"].as_str().filter(|c| usage::known_category(c)).map(str::to_string),
            idle_after: num("idleMins", 1, 30, 2) * 60,
            history_days: num("historyDays", 0, 3650, 30) as usize,
            prefs: Prefs {
                hidden: s["hidden"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                cats: s["cats"].as_object().map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|c| (k.clone(), c.to_string()))).collect()).unwrap_or_default(),
                icons: s["icons"].as_bool().unwrap_or(true),
                titles: on("windowTitles"),
            },
            titles: on("windowTitles"),
        }
    }

    /// A reminder is on, whatever else is.
    fn wants_breaks(&self) -> bool {
        self.rule.eye.on || self.rule.mov.on
    }

    fn wanted(&self) -> bool {
        self.wants_breaks() || self.screen_time
    }
}

#[derive(Default)]
struct St {
    cfg: Cfg,
    rule: Rule,
    /// How the day's breaks ended, by kind. Counted whenever a reminder runs.
    stats: Stats,
    /// There while screen time is on.
    usage: Option<Usage>,
    fade: Option<Fade>,
    /// Why the person cannot be there: 1 locked, 2 asleep, 4 the display is off.
    gone: u8,
    fullscreen: bool,
    /// Only read the front window's title while title tracking is enabled.
    title_hwnd: isize,
    title_key: String,
    /// The loop of this generation is the one that runs.
    gen: u64,
    live: bool,
    /// A break was started by hand: its seconds are counted from that moment, not from the last tick.
    rebase: bool,
    /// The island's own pause: breaks wait until then, whatever else is going on.
    pause_until: Option<Instant>,
    /// The day and category whose budget notice was already shown.
    noticed: Option<String>,
    /// What a clear or a delete took, while Undo still stands: when, and the store as data.
    cleared: Option<(Instant, Value)>,
    /// The day and category the report was last asked for: opening it again keeps them.
    report_day: String,
    report_cat: String,
}

impl St {
    /// Whole seconds left of the island's pause; 0 when there is none.
    fn pause_left(&self) -> u64 {
        self.pause_until
            .and_then(|until| until.checked_duration_since(Instant::now()))
            .map_or(0, |left| left.as_secs())
    }

    /// What a clear or a delete took, while Undo still stands.
    fn undoable(&self) -> bool {
        self.cleared.as_ref().is_some_and(|(at, _)| at.elapsed().as_secs() < UNDO_SECS)
    }
}

/// The `state` event, to every window.
pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;

/// Everything the feature runs. It knows the app only as `emit` and a file, so the probe runs it too.
pub struct Engine {
    emit: Emit,
    /// The screen-time file.
    file: PathBuf,
    st: Mutex<St>,
    wake: Condvar,
    /// What the loop listens to, and the generation it belongs to. Never touched with `st` held:
    /// dropping a subscription waits for the service's thread, which may be waiting for `st`.
    subs: Mutex<(u64, Vec<svc_sysevents::Sub>)>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Programs' icons as data URLs, by key, read once per run.
    icons: Mutex<HashMap<String, String>>,
    /// Paths the file never learned, found in the running processes or the registry.
    learned: Mutex<BTreeMap<String, String>>,
    /// What was already written to the log, so a missing icon is said once and not every sweep.
    noted: Mutex<BTreeSet<String>>,
}

impl Engine {
    pub fn new(emit: Emit, file: PathBuf) -> Arc<Engine> {
        // The break counts sit beside the screen time, in the same folder.
        let stats = Stats::load(Some(file.with_file_name(stats::FILE)));
        Arc::new(Engine {
            emit,
            file,
            st: Mutex::new(St { stats, ..Default::default() }),
            wake: Condvar::new(),
            subs: Mutex::default(),
            thread: Mutex::default(),
            icons: Mutex::default(),
            learned: Mutex::default(),
            noted: Mutex::default(),
        })
    }

    fn status(&self, g: &St) -> Value {
        let mut v = g.rule.status();
        v["fade"] = json!(g.cfg.fade);
        v["counts"] = g.stats.snapshot(&usage::today());
        v["paused"] = json!(g.pause_left());
        v
    }

    pub fn status_now(&self) -> Value {
        self.status(&self.st.lock().unwrap())
    }

    /// What the rule decided goes to the page and to the fade. Called with `st` held.
    fn tell(self: &Arc<Self>, g: &mut St, evs: impl IntoIterator<Item = Ev>) {
        for ev in evs {
            let (phase, kind, why) = match ev {
                Ev::Soon(kind) => ("soon", kind, ""),
                Ev::Held(kind) => ("held", kind, ""),
                Ev::Start(kind, secs) => {
                    if let Some(old) = g.fade.take() {
                        old.close();
                    }
                    if g.cfg.fade {
                        let me = Arc::downgrade(self);
                        let title = match kind {
                            Kind::Eye => "Look at something far away",
                            Kind::Move => "Stand up and stretch",
                        };
                        g.fade = Some(Fade::show(
                            secs,
                            notch_core::i18n::t(title),
                            notch_core::i18n::t("Esc or click to skip"),
                            move || {
                                if let Some(me) = me.upgrade() {
                                    me.end("skip");
                                }
                            },
                        ));
                    }
                    ("start", kind, "")
                }
                Ev::End(kind, why) => {
                    if let Some(fade) = g.fade.take() {
                        fade.close();
                    }
                    // A break that ended for any of the person's own reasons is counted; the
                    // feature being turned off mid-break is not an answer.
                    if why != "off" && g.stats.record(&usage::today(), kind.name(), why) {
                        g.stats.save();
                    }
                    ("end", kind, why)
                }
            };
            let mut v = self.status(g);
            v["ev"] = json!({ "phase": phase, "kind": kind.name(), "why": why });
            (self.emit)(v);
        }
    }

    /// Skip or snooze, from the island or from the fade.
    pub fn end(self: &Arc<Self>, why: &'static str) {
        let mut g = self.st.lock().unwrap();
        let ev = if why == "skip" { g.rule.skip() } else { g.rule.stop(why) };
        self.tell(&mut g, ev);
    }

    /// A break now, asked for by hand: it runs even with every reminder off.
    pub fn take(self: &Arc<Self>, kind: Kind) {
        let launch = {
            let mut g = self.st.lock().unwrap();
            let ev = g.rule.take(kind);
            g.rebase |= ev.is_some();
            self.tell(&mut g, ev);
            self.wake.notify_all();
            self.ensure(&mut g)
        };
        if let Some(gen) = launch {
            self.launch(gen);
        }
    }

    /// The settings, as the app keeps them (the manifest's defaults with the person's on top).
    pub fn apply(self: &Arc<Self>, settings: &Value) {
        let cfg = Cfg::read(settings);
        let (launch, fresh, refresh_front) = {
            let mut g = self.st.lock().unwrap();
            let fresh = cfg.screen_time && g.usage.is_none();
            let refresh_front = !g.cfg.titles && cfg.titles && g.usage.is_some();
            if fresh {
                g.usage = Some(Usage::load(Some(self.file.clone())));
            } else if !cfg.screen_time {
                if let Some(mut usage) = g.usage.take() {
                    usage.save();
                }
            }
            if !cfg.fade {
                if let Some(fade) = g.fade.take() {
                    fade.close();
                }
            }
            g.rule.cfg = cfg.rule.clone();
            let (idle_after, history_days) = (cfg.idle_after, cfg.history_days);
            let had_titles = g.cfg.titles;
            g.cfg = cfg;
            if let Some(usage) = g.usage.as_mut() {
                usage.set_ring(idle_after);
                usage.set_keep(history_days);
                usage.save();
            } else {
                // The history is the person's even while screen time is off: a shorter window
                // still takes the days it no longer keeps.
                let mut kept = Usage::load(Some(self.file.clone()));
                if kept.set_keep(history_days) {
                    kept.save();
                }
            }
            if g.stats.set_keep(history_days) {
                g.stats.save();
            }
            // Turning the titles off forgets the ones already kept, on disk too: the seconds stay.
            if had_titles && !g.cfg.titles {
                match g.usage.as_mut() {
                    Some(usage) => {
                        usage.clear_titles();
                        usage.save();
                    }
                    None => {
                        let mut usage = Usage::load(Some(self.file.clone()));
                        usage.clear_titles();
                        usage.save();
                    }
                }
            }
            (self.emit)(self.status(&g));
            self.wake.notify_all();
            (self.ensure(&mut g), fresh, refresh_front)
        };
        if let Some(gen) = launch {
            self.launch(gen);
        } else if fresh || refresh_front {
            self.front(&svc_sysevents::foreground());
        }
    }

    /// The loop is needed (something is on, or a break is on screen) and does not run: its generation.
    fn ensure(&self, g: &mut St) -> Option<u64> {
        if g.live || !(g.cfg.wanted() || g.rule.on_break().is_some()) {
            return None;
        }
        g.live = true;
        g.gen += 1;
        Some(g.gen)
    }

    fn launch(self: &Arc<Self>, gen: u64) {
        let (a, b) = (Arc::downgrade(self), Arc::downgrade(self));
        let subs = vec![
            svc_sysevents::on_session(move |e| {
                if let Some(me) = a.upgrade() {
                    me.session(e);
                }
            }),
            svc_sysevents::on_foreground(move |fg| {
                if let Some(me) = b.upgrade() {
                    me.front(fg);
                }
            }),
        ];
        *self.subs.lock().unwrap() = (gen, subs);
        self.front(&svc_sysevents::foreground());
        let me = self.clone();
        *self.thread.lock().unwrap() = Some(std::thread::spawn(move || me.run(gen)));
    }

    fn session(self: &Arc<Self>, e: Session) {
        let (bit, on) = match e {
            Session::Lock => (1, true),
            Session::Unlock => (1, false),
            Session::Sleep => (2, true),
            Session::Resume => (2, false),
            Session::DisplayOff => (4, true),
            Session::DisplayOn => (4, false),
            _ => return,
        };
        let mut g = self.st.lock().unwrap();
        if on {
            g.gone |= bit;
            // The person left: that ends a break on screen, and the PC may not come back.
            let ev = g.rule.stop("away");
            self.tell(&mut g, ev);
            g.stats.save();
            if let Some(usage) = g.usage.as_mut() {
                usage.save();
            }
        } else {
            g.gone &= !bit;
        }
        self.wake.notify_all();
    }

    fn front(&self, fg: &Foreground) {
        let (counting, titles) = {
            let mut g = self.st.lock().unwrap();
            g.fullscreen = fg.fullscreen;
            (g.usage.is_some(), g.cfg.titles)
        };
        if !counting {
            return;
        }
        // This app's own windows are Lintel's own time, a row of its own. The name is read outside
        // the lock.
        let own = fg.pid == std::process::id();
        let title_hwnd = if titles && !own { fg.hwnd } else { 0 };
        let app = if own { Some(usage::AppInfo::own()) } else { sys::app_of(fg.hwnd, fg.pid, &fg.exe) };
        // The title is read here, on the event that makes a window the front one, and only when
        // the person asked for it: it can name a document, a page or a message.
        let title = if titles && !own { sys::title_of(fg.hwnd) } else { String::new() };
        let key = app.as_ref().map(|a| a.key.clone()).unwrap_or_default();
        let mut g = self.st.lock().unwrap();
        g.title_hwnd = title_hwnd;
        g.title_key = key.clone();
        if let Some(usage) = g.usage.as_mut() {
            usage.front(app);
            usage.title(&key, &title);
        }
    }

    fn run(self: Arc<Self>, gen: u64) {
        let mut last = Instant::now();
        let mut unsaved = 0u64;
        // Going idle takes back the seconds the person was already walking away.
        let mut was_idle = false;
        let mut g = self.st.lock().unwrap();
        while g.gen == gen {
            // Everything was turned off and no break is on screen: the loop ends itself.
            if !g.cfg.wanted() && g.rule.on_break().is_none() {
                g.live = false;
                break;
            }
            // Locked or asleep there is nothing to count: no tick, the session event wakes it.
            g = if g.gone != 0 && g.rule.on_break().is_none() {
                self.wake.wait(g).unwrap()
            } else {
                let next = (last + Duration::from_secs(1)).saturating_duration_since(Instant::now());
                self.wake.wait_timeout(g, next).unwrap().0
            };
            if g.gen != gen {
                break;
            }
            if std::mem::take(&mut g.rebase) {
                last = Instant::now();
                continue;
            }
            let dt = last.elapsed().as_secs();
            if dt == 0 {
                continue;
            }
            last += Duration::from_secs(dt);
            // Fullscreen describes a window, not whether a person is present. Idle policy applies
            // equally to fullscreen apps so unattended games and video do not accrue time.
            let idle = sys::idle_secs() >= g.cfg.idle_after;
            if g.cfg.titles && g.title_hwnd != 0 {
                let title = sys::title_of(g.title_hwnd);
                let key = g.title_key.clone();
                if let Some(usage) = g.usage.as_mut() {
                    usage.title(&key, &title);
                }
            }
            let away = g.gone != 0 || dt > GAP || idle;
            // The pointer over Lintel's own windows is Lintel's time — except during a break, when
            // the fade covers every screen: the pointer is over it wherever the person is looking.
            let own = g.usage.is_some() && g.fade.is_none() && sys::pointer_over_self();
            let (day, hour) = usage::now();
            let idle_after = g.cfg.idle_after;
            // Where the day stood before these seconds: the notice belongs to the crossing, so
            // opening the app with the budget already used says nothing.
            let before = if g.cfg.budget > 0 {
                g.usage.as_ref().map_or(0, |u| u.spent(&day, &g.cfg.prefs, g.cfg.budget_cat.as_deref()))
            } else {
                0
            };
            if let Some(usage) = g.usage.as_mut() {
                if !away {
                    usage.add(&day, hour, dt, own);
                    unsaved += dt;
                } else if idle && !was_idle {
                    usage.undo(idle_after);
                }
                // Once a minute while it counts, and when the person leaves.
                if unsaved >= 60 || (away && unsaved > 0) {
                    usage.save();
                    unsaved = 0;
                }
            }
            was_idle = idle;
            // The day's target: one notice a day, on the second it is crossed.
            let crossed = if g.cfg.budget > 0 {
                let key = format!("{day}|{}", g.cfg.budget_cat.as_deref().unwrap_or(""));
                let spent = g.usage.as_ref().map_or(0, |u| u.spent(&day, &g.cfg.prefs, g.cfg.budget_cat.as_deref()));
                (before < g.cfg.budget && spent >= g.cfg.budget && g.noticed.as_deref() != Some(key.as_str()))
                    .then_some((key, spent))
            } else {
                None
            };
            if let Some((key, spent)) = crossed {
                g.noticed = Some(key);
                let mut v = self.status(&g);
                v["ev"] = json!({
                    "phase": "budget",
                    "kind": "screen",
                    "why": "",
                    "day": day,
                    "spent": spent,
                    "limit": g.cfg.budget,
                    "cat": g.cfg.budget_cat,
                });
                (self.emit)(v);
            }
            let (cfg, fullscreen) = (g.cfg.clone(), g.fullscreen);
            // The tray's Pause keeps everything off the screen: a break waits for it like for a
            // call, a quiet window, a presentation, or a pause asked for from the island.
            let clock = notch_core::win::local_time();
            let quiet = (cfg.quiet && rule::within((clock.hour as u32) * 60 + clock.minute as u32, cfg.quiet_from, cfg.quiet_to))
                || (cfg.quiet_system && sys::quiet_time());
            let paused = g.pause_until.is_some_and(|until| until > Instant::now());
            let evs = g.rule.tick(dt, away, || {
                notch_core::paused()
                    || paused
                    || quiet
                    || (cfg.hold_fullscreen && fullscreen)
                    || (cfg.hold_call && !sys::mic_in_use().is_empty())
            });
            self.tell(&mut g, evs);
        }
        drop(g);
        let mut subs = self.subs.lock().unwrap();
        if subs.0 == gen {
            subs.1.clear();
        }
    }

    // ── Screen time: what the page reads ──────────────────────────────────────

    /// One day of screen time, all of its programs; with `clear`, after forgetting all of it.
    /// With screen time off the file is read as it is: the history stays the person's.
    pub fn usage(&self, day: Option<&str>, clear: bool) -> Value {
        let mut g = self.st.lock().unwrap();
        if clear {
            // What is about to go is kept in memory for a few seconds, so a clear can be undone.
            let kept = match g.usage.as_ref() {
                Some(usage) => usage.dump(),
                None => Usage::load(Some(self.file.clone())).dump(),
            };
            match g.usage.as_mut() {
                Some(usage) => usage.clear(),
                // Off: what was kept is on disk only.
                None => _ = std::fs::remove_file(&self.file),
            }
            g.cleared = Some((Instant::now(), kept));
        }
        let (prefs, budget, cat) = (g.cfg.prefs.clone(), g.cfg.budget, g.cfg.budget_cat.clone());
        let undo = g.undoable();
        let day = view_day(day);
        let mut v = match g.usage.as_ref() {
            Some(usage) => usage.snapshot(&day, &prefs, true),
            None => Usage::load(Some(self.file.clone())).snapshot(&day, &prefs, false),
        };
        let spent = match g.usage.as_ref() {
            Some(usage) => usage.spent(&day, &prefs, cat.as_deref()),
            None => Usage::load(Some(self.file.clone())).spent(&day, &prefs, cat.as_deref()),
        };
        v["undo"] = json!(undo);
        v["budget"] = json!({ "mins": budget / 60, "cat": cat, "spent": spent });
        v
    }

    /// Puts back what the last clear or delete took, when Undo still stands.
    pub fn undo(self: &Arc<Self>) -> bool {
        let mut g = self.st.lock().unwrap();
        let Some((at, data)) = g.cleared.clone() else { return false };
        if at.elapsed().as_secs() >= UNDO_SECS {
            g.cleared = None;
            return false;
        }
        // Screen time may have been turned off since: the file is the store then, and is put back
        // the same way.
        let ok = match g.usage.as_mut() {
            Some(usage) => {
                let ok = usage.restore(&data);
                if ok {
                    usage.save();
                }
                ok
            }
            None => {
                let mut usage = Usage::load(Some(self.file.clone()));
                let ok = usage.restore(&data);
                if ok {
                    usage.save();
                }
                ok
            }
        };
        if ok {
            g.cleared = None;
        }
        ok
    }

    /// One day gone. Kept aside like a clear, so it can be undone the same way.
    pub fn forget_day(&self, day: &str) -> bool {
        let Some(day) = usage::parse_day(day) else { return false };
        let mut g = self.st.lock().unwrap();
        // With screen time off the history is a file: it is changed on disk and read back.
        let mut loaded = g.usage.is_none().then(|| Usage::load(Some(self.file.clone())));
        let Some(usage) = g.usage.as_mut().or(loaded.as_mut()) else { return false };
        let kept = usage.dump();
        if !usage.remove_day(&day) {
            return false;
        }
        usage.save();
        g.cleared = Some((Instant::now(), kept));
        true
    }

    /// A program gone from the whole history: the privacy answer to an app opened once.
    pub fn forget_app(&self, key: &str) -> bool {
        let mut g = self.st.lock().unwrap();
        let mut loaded = g.usage.is_none().then(|| Usage::load(Some(self.file.clone())));
        let Some(usage) = g.usage.as_mut().or(loaded.as_mut()) else { return false };
        let kept = usage.dump();
        if !usage.forget(key) {
            return false;
        }
        usage.save();
        g.cleared = Some((Instant::now(), kept));
        true
    }

    /// Both files a spreadsheet and a backup can use, in the feature's own folder, and the
    /// folder shown.
    pub fn export(&self) -> Result<Value, String> {
        let (csv, data) = {
            let g = self.st.lock().unwrap();
            let prefs = g.cfg.prefs.clone();
            match g.usage.as_ref() {
                Some(usage) => (usage.csv(&prefs), usage.dump()),
                None => {
                    let usage = Usage::load(Some(self.file.clone()));
                    (usage.csv(&prefs), usage.dump())
                }
            }
        };
        let dir = self.file.parent().ok_or("no data folder")?.to_path_buf();
        let csv_path = dir.join("screen-time.csv");
        let json_path = dir.join("screen-time-export.json");
        std::fs::write(&csv_path, csv).map_err(|e| e.to_string())?;
        notch_core::storage::json_save_atomic(&json_path, &data).map_err(|e| e.to_string())?;
        let _ = svc_shell::reveal(&dir);
        Ok(json!({
            "folder": dir.to_string_lossy(),
            "csv": csv_path.to_string_lossy(),
            "json": json_path.to_string_lossy(),
        }))
    }

    /// Breaks wait for this long, asked for from the island; 0 lets them go on again.
    pub fn pause(&self, mins: Option<u64>) {
        let mut g = self.st.lock().unwrap();
        g.pause_until = mins.filter(|m| *m > 0).map(|m| Instant::now() + Duration::from_secs(m.clamp(1, 180) * 60));
        self.wake.notify_all();
    }

    /// The day and the category the report is to open on; empty means the first answer names it.
    pub fn report(&self) -> Value {
        let g = self.st.lock().unwrap();
        json!({ "day": g.report_day, "cat": g.report_cat })
    }

    /// What the report is looking at, remembered across its closing and for the island's Open.
    pub fn remember(&self, day: Option<&str>, cat: Option<&str>) {
        let mut g = self.st.lock().unwrap();
        if let Some(day) = day.and_then(usage::parse_day) {
            g.report_day = day;
        }
        if let Some(cat) = cat {
            g.report_cat = if usage::known_category(cat) { cat.to_string() } else { String::new() };
        }
    }

    /// One program on one day, for its own page.
    pub fn app(&self, key: &str, day: Option<&str>) -> Value {
        let g = self.st.lock().unwrap();
        let prefs = g.cfg.prefs.clone();
        let day = view_day(day);
        match g.usage.as_ref() {
            Some(usage) => usage.app(key, &day, &prefs),
            None => Usage::load(Some(self.file.clone())).app(key, &day, &prefs),
        }
    }

    /// The icons of these programs, as data URLs, by key; the ones already read are kept.
    ///
    /// A program the file knows no path for — its time was counted before the path was kept —
    /// gets one from `learn_paths` first: every running app has a file, and so does every
    /// installed app the registry knows.
    pub fn icons(&self, keys: &[String]) -> Value {
        let prefs = self.st.lock().unwrap().cfg.prefs.clone();
        let mut out = serde_json::Map::new();
        if !prefs.icons {
            return json!(out);
        }
        let keys: Vec<String> = keys.iter().take(64).cloned().collect();
        let mut paths = self.paths();
        let unknown: Vec<String> = keys.iter().filter(|k| !paths.contains_key(*k)).cloned().collect();
        if !unknown.is_empty() {
            paths.extend(self.learn_paths(&unknown));
        }
        for key in &keys {
            let Some(path) = paths.get(key) else { continue };
            if let Some(url) = self.icons.lock().unwrap().get(key).cloned() {
                out.insert(key.clone(), json!(url));
                continue;
            }
            match svc_shell::icon(path, 32) {
                Ok(png) => {
                    let url = data_url(&png);
                    self.icons.lock().unwrap().insert(key.clone(), url.clone());
                    out.insert(key.clone(), json!(url));
                }
                Err(why) => self.note(&format!("icon {key}: {why}")),
            }
        }
        json!(out)
    }

    /// Where a program's file is: to open it, to show it. Lintel's own row has a file for its
    /// icon and none for these: another copy of the app is not what the person asked for.
    pub fn path_of(&self, key: &str) -> Option<String> {
        if key == usage::LINTEL {
            return None;
        }
        let stored = match self.st.lock().unwrap().usage.as_ref() {
            Some(usage) => usage.path_of(key),
            None => Usage::load(Some(self.file.clone())).path_of(key),
        };
        stored.filter(|p| !p.is_empty()).or_else(|| self.learn_paths(std::slice::from_ref(&key.to_string())).get(key).cloned())
    }

    /// Paths the file never learned: a running process's file first, then App Paths and PATH for
    /// a program that is closed. Learned once per run, then written into the store.
    fn learn_paths(&self, keys: &[String]) -> BTreeMap<String, String> {
        let want: Vec<String> = {
            let known = self.learned.lock().unwrap();
            keys.iter().filter(|k| !known.contains_key(*k)).cloned().collect()
        };
        if !want.is_empty() {
            let running = sys::exe_paths();
            let mut found: Vec<(String, String)> = Vec::new();
            for key in want {
                match running.get(&key).cloned().or_else(|| sys::app_path(&key)) {
                    Some(path) => found.push((key, path)),
                    None => self.note(&format!("path {key}: no program file")),
                }
            }
            if !found.is_empty() {
                if let Some(usage) = self.st.lock().unwrap().usage.as_mut() {
                    usage.learn(&found);
                }
                self.learned.lock().unwrap().extend(found);
            }
        }
        self.learned.lock().unwrap().clone()
    }

    /// One line per key and reason, however often it is asked.
    fn note(&self, line: &str) {
        if self.noted.lock().unwrap().insert(line.to_string()) {
            log::line(line.to_string());
        }
    }

    fn paths(&self) -> std::collections::BTreeMap<String, String> {
        match self.st.lock().unwrap().usage.as_ref() {
            Some(usage) => usage.paths(),
            None => Usage::load(Some(self.file.clone())).paths(),
        }
    }

    /// Leaves nothing behind: no thread, no subscription, no window.
    pub fn stop(self: &Arc<Self>) {
        let fade = {
            let mut g = self.st.lock().unwrap();
            g.gen += 1;
            g.live = false;
            let fade = g.fade.take();
            let ev = g.rule.stop("off");
            self.tell(&mut g, ev);
            if let Some(mut usage) = g.usage.take() {
                usage.save();
            }
            g.stats.save();
            self.wake.notify_all();
            fade
        };
        // The screens are given back before `stop` returns.
        if let Some(fade) = fade {
            fade.close_wait();
        }
        *self.subs.lock().unwrap() = (0, Vec::new());
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
        self.icons.lock().unwrap().clear();
    }
}

/// The day the page asked for, or today; anything that is not a local date is today.
fn view_day(day: Option<&str>) -> String {
    day.and_then(usage::parse_day).unwrap_or_else(usage::today)
}

/// A PNG as a data URL, for an `<img>` of the page.
/// debt: the same line as crates/clipboard and crates/radial-menu; it belongs in notch_core.
fn data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
}

#[derive(Default)]
pub struct Breaks {
    inner: Mutex<Option<Arc<Engine>>>,
}

impl Feature for Breaks {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let inner = self
            .inner
            .lock()
            .unwrap()
            .get_or_insert_with(|| {
                let cx = cx.clone();
                let file = cx.data_dir(ID).join(usage::FILE);
                Engine::new(Arc::new(move |v| cx.emit(ID, "state", v)), file)
            })
            .clone();
        inner.apply(&cx.settings(ID));
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        let inner = self.inner.lock().unwrap().take();
        if let Some(inner) = inner {
            inner.stop();
        }
    }

    fn settings_changed(&self, _cx: &Ctx, settings: &Value) {
        let inner = self.inner.lock().unwrap().clone();
        if let Some(inner) = inner {
            inner.apply(settings);
        }
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let inner = self.inner.lock().unwrap().clone().ok_or("turned off")?;
        match method {
            // The report is a window of its own: the section is a summary, this is the whole of it.
            // The day and the category it was asked for are kept, so opening it again — or the
            // window closing and Opening once more — shows what the person was looking at.
            "window" => {
                inner.remember(args["day"].as_str(), args["cat"].as_str());
                // The report may already be open: it hears of the day and category it is asked for.
                (inner.emit)(inner.status_now());
                let spec = WindowSpec {
                    label: WINDOW.into(),
                    feature: ID.into(),
                    page: "screen".into(),
                    w: 860.0,
                    h: 660.0,
                    frameless: true,
                    ..Default::default()
                };
                if let Ok(win) = cx.window().open(spec) {
                    let _ = win.set_min_size(Some(LogicalSize::new(LEAST.0, LEAST.1)));
                }
                return Ok(json!({}));
            }
            "close" => {
                cx.window().close(WINDOW);
                return Ok(json!({}));
            }
            "state" => {}
            "skip" => inner.end("skip"),
            "snooze" => inner.end("snooze"),
            "take" => inner.take(args["kind"].as_str().and_then(Kind::parse).ok_or("unknown break")?),
            "usage" => return Ok(inner.usage(args["day"].as_str(), false)),
            "report" => return Ok(inner.report()),
            "keep" => {
                inner.remember(args["day"].as_str(), args["cat"].as_str());
                return Ok(json!({}));
            }
            "clear" => return Ok(inner.usage(None, true)),
            "undo" => {
                inner.undo();
                return Ok(inner.usage(None, false));
            }
            "forgetDay" => {
                let day = args["day"].as_str().ok_or("no day")?;
                inner.forget_day(day);
                return Ok(inner.usage(Some(day), false));
            }
            "forgetApp" => {
                let key = args["key"].as_str().ok_or("no app")?;
                inner.forget_app(key);
                // A program gone from the history has no place in the person's own lists either.
                let mut hidden: Vec<String> = cx.settings(ID)["hidden"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                hidden.retain(|k| k != key);
                let mut cats: serde_json::Map<String, Value> =
                    cx.settings(ID)["cats"].as_object().cloned().unwrap_or_default();
                cats.remove(key);
                cx.patch_settings(ID, json!({ "hidden": hidden, "cats": cats }));
                inner.apply(&cx.settings(ID));
                return Ok(inner.usage(None, false));
            }
            "export" => return inner.export(),
            "pause" => {
                inner.pause(args["mins"].as_u64());
                return Ok(inner.status_now());
            }
            "resume" => {
                inner.pause(None);
                return Ok(inner.status_now());
            }
            "app" => {
                let key = args["key"].as_str().ok_or("no app")?;
                return Ok(inner.app(key, args["day"].as_str()));
            }
            "icons" => {
                let keys: Vec<String> = args["keys"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                return Ok(inner.icons(&keys));
            }
            "open" | "reveal" => {
                let key = args["key"].as_str().ok_or("no app")?;
                let path = inner.path_of(key).ok_or("no program file")?;
                if method == "open" {
                    svc_shell::open(&path)?;
                } else {
                    svc_shell::reveal(std::path::Path::new(&path))?;
                }
                return Ok(json!({}));
            }
            // The person's own list and categories: settings, so that a disabled feature keeps them.
            "hide" | "unhide" => {
                let key = args["key"].as_str().ok_or("no app")?;
                let mut hidden: Vec<String> = cx.settings(ID)["hidden"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                hidden.retain(|k| k != key);
                if method == "hide" {
                    hidden.push(key.to_string());
                }
                hidden.sort();
                cx.patch_settings(ID, json!({ "hidden": hidden }));
                inner.apply(&cx.settings(ID));
                return Ok(inner.usage(None, false));
            }
            "category" => {
                let key = args["key"].as_str().ok_or("no app")?;
                let cat = args["cat"].as_str().unwrap_or_default();
                let mut cats: serde_json::Map<String, Value> =
                    cx.settings(ID)["cats"].as_object().cloned().unwrap_or_default();
                if cat.is_empty() || !usage::known_category(cat) {
                    cats.remove(key);
                } else {
                    cats.insert(key.to_string(), json!(cat));
                }
                cx.patch_settings(ID, json!({ "cats": cats }));
                inner.apply(&cx.settings(ID));
                return Ok(inner.usage(None, false));
            }
            _ => return Err("unknown method".into()),
        }
        Ok(inner.status_now())
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/breaks/strings.json")
    }
}

pub fn feature() -> std::sync::Arc<dyn Feature> {
    std::sync::Arc::new(Breaks::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(extra: Value) -> Value {
        // The manifest's own default object, with the test's own keys on top.
        let mut base: Value = serde_json::from_str(MANIFEST.defaults).expect("the defaults are JSON");
        for (key, value) in extra.as_object().expect("an object") {
            base[key] = value.clone();
        }
        base
    }

    #[test]
    fn the_defaults_leave_the_budget_and_quiet_hours_off() {
        let cfg = Cfg::read(&settings(json!({})));
        assert_eq!(cfg.budget, 0);
        assert_eq!(cfg.budget_cat, None);
        assert!(!cfg.quiet);
        assert_eq!((cfg.quiet_from, cfg.quiet_to), (22 * 60, 7 * 60));
        assert!(cfg.quiet_system);
        assert_eq!(cfg.idle_after, 2 * 60);
        assert!(!cfg.wants_breaks());
    }

    #[test]
    fn a_budget_is_minutes_a_day_and_a_category_only_when_it_is_one() {
        let cfg = Cfg::read(&settings(json!({ "budget": 90, "budgetCat": "browsing" })));
        assert_eq!(cfg.budget, 90 * 60);
        assert_eq!(cfg.budget_cat.as_deref(), Some("browsing"));
        // A category nobody knows is no category, not a budget that never counts.
        let odd = Cfg::read(&settings(json!({ "budget": 90, "budgetCat": "elbows" })));
        assert_eq!(odd.budget_cat, None);
        // A budget past a day is a day.
        let huge = Cfg::read(&settings(json!({ "budget": 5000 })));
        assert_eq!(huge.budget, 24 * 60 * 60);
    }

    #[test]
    fn quiet_hours_are_read_as_clock_times_and_bad_ones_fall_back() {
        let cfg = Cfg::read(&settings(json!({ "quietHours": true, "quietFrom": "09:15", "quietTo": "10:45" })));
        assert!(cfg.quiet);
        assert_eq!((cfg.quiet_from, cfg.quiet_to), (9 * 60 + 15, 10 * 60 + 45));
        let bad = Cfg::read(&settings(json!({ "quietFrom": "later", "quietTo": "99:99" })));
        assert_eq!((bad.quiet_from, bad.quiet_to), (22 * 60, 7 * 60));
        let none = Cfg::read(&settings(json!({ "quietSystem": false })));
        assert!(!none.quiet_system);
    }

    #[test]
    fn the_absence_that_takes_seconds_back_is_the_persons_own_and_bounded() {
        assert_eq!(Cfg::read(&settings(json!({ "idleMins": 5 }))).idle_after, 5 * 60);
        assert_eq!(Cfg::read(&settings(json!({ "idleMins": 0 }))).idle_after, 60);
        assert_eq!(Cfg::read(&settings(json!({ "idleMins": 600 }))).idle_after, 30 * 60);
    }

    #[test]
    fn screen_time_alone_keeps_the_loop_alive() {
        let cfg = Cfg::read(&settings(json!({ "screenTime": true })));
        assert!(cfg.wanted());
        assert!(!cfg.wants_breaks());
        let eye = Cfg::read(&settings(json!({ "eye": true })));
        assert!(eye.wanted() && eye.wants_breaks());
    }

    #[test]
    fn the_report_is_only_opened_on_a_real_day() {
        assert_eq!(view_day(Some("2026-11-15")), "2026-11-15");
        assert_eq!(view_day(Some("2026-13-15")), usage::today());
        assert_eq!(view_day(Some("yesterday")), usage::today());
        assert_eq!(view_day(None), usage::today());
    }
}
