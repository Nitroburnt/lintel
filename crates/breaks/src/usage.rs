// Screen time: app totals remain the fast summary; merged intervals preserve which window was in
// front. Titles are only kept when the person enabled title tracking.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// The file, in the feature's data folder.
pub const FILE: &str = "screen-time.json";
/// Days kept when the person leaves the setting alone, today included. 0 (the setting's "Unlimited") keeps every day.
pub const KEEP: usize = 30;
/// The buckets of a day's timeline.
const HOURS: usize = 24;
/// The key of the time no program was in front: the desktop, the taskbar, the Start menu.
pub const DESKTOP: &str = "desktop";
/// The key of this app's own time: its island, its windows. Never a program's, even while one is
/// in front behind an island that does not take the keyboard.
pub const LINTEL: &str = "lintel";

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct App {
    pub name: String,
    pub secs: u64,
    /// The program's file, for its icon and for opening it. Empty when Windows would not say.
    #[serde(default)]
    pub path: String,
    /// The title of its window when it last came to the front, when the person asked for titles.
    #[serde(default)]
    pub title: String,
}

/// One contiguous period in which a program and, when enabled, its window title were in front.
/// `secs` preserves the monotonic tick duration; timestamps are UTC seconds for display/history.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Interval {
    pub start: u64,
    pub end: u64,
    pub secs: u64,
    pub key: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Day {
    /// By the program's file name, lower case, without ".exe".
    #[serde(default)]
    pub apps: BTreeMap<String, App>,
    #[serde(default)]
    pub switches: u64,
    /// Seconds by local hour, for the day's timeline.
    #[serde(default)]
    pub hours: [u64; HOURS],
    /// Adjacent ticks for the same app and title are merged to keep the journal compact.
    #[serde(default)]
    pub intervals: Vec<Interval>,
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    /// By local date, "2026-10-11".
    #[serde(default)]
    days: BTreeMap<String, Day>,
}

/// The program in front, as the window it belongs to names it.
#[derive(Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub key: String,
    pub name: String,
    pub path: String,
}

impl AppInfo {
    /// The time no program is in front: the desktop, the taskbar.
    fn desktop() -> AppInfo {
        AppInfo { key: DESKTOP.into(), name: "Desktop".into(), path: String::new() }
    }

    /// This app's own time, as its own row: kept apart from the program that was in front behind
    /// the island, and from the desktop's own time.
    pub fn own() -> AppInfo {
        AppInfo {
            key: LINTEL.into(),
            name: "Lintel".into(),
            path: std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        }
    }
}

/// What the page needs of the person's settings.
#[derive(Clone)]
pub struct Prefs {
    /// Apps left out of the list. Their time stays in the totals, which are the day's.
    pub hidden: BTreeSet<String>,
    /// The person's own category for a program key.
    pub cats: BTreeMap<String, String>,
    /// Icons are read from each program's file.
    pub icons: bool,
    /// Window titles are read and kept. Off unless the person turns it on: a title can name a
    /// document, a page or a message.
    pub titles: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { hidden: BTreeSet::new(), cats: BTreeMap::new(), icons: true, titles: false }
    }
}

impl Prefs {
    /// The category of a program: the person's own, or the table's.
    pub fn category(&self, key: &str) -> &str {
        self.cats.get(key).map(String::as_str).unwrap_or_else(|| category(key))
    }
}

pub struct Usage {
    /// `None` keeps it in memory only (the probe).
    path: Option<PathBuf>,
    store: Store,
    /// The program in front; `None` until one is known.
    front: Option<AppInfo>,
    /// The last program that was in front, to count a switch only when it changes.
    last: Option<String>,
    /// The title of the current front window and the program it belongs to.
    active_title: Option<(String, String)>,
    dirty: bool,
    /// The seconds counted lately, so that a tail spent away can be taken back.
    ring: VecDeque<(String, usize, String, u64)>,
    ring_secs: u64,
    /// The longest tail an absence takes back: the person's own idle limit.
    ring_limit: u64,
    /// How many days are kept, today included; 0 keeps every day.
    keep_days: usize,
}

/// Today's local date, the key of a day.
pub fn today() -> String {
    now().0
}

/// The day of a "2026-11-15" the page sent, if it is one: a local date, nothing else.
pub fn parse_day(day: &str) -> Option<String> {
    let b = day.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    if !b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() }) {
        return None;
    }
    let month: u32 = day[5..7].parse().ok()?;
    let date: u32 = day[8..10].parse().ok()?;
    ((1..=12).contains(&month) && (1..=31).contains(&date)).then(|| day.to_string())
}

/// Today's local date and the hour, for counting a tick.
pub fn now() -> (String, u32) {
    let t = notch_core::win::local_time();
    (format!("{:04}-{:02}-{:02}", t.year, t.month, t.day), t.hour)
}

impl Usage {
    pub fn load(path: Option<PathBuf>) -> Self {
        let store = path.as_deref().map(notch_core::storage::json_load).unwrap_or_default();
        Usage {
            path,
            store,
            front: None,
            last: None,
            active_title: None,
            dirty: false,
            ring: VecDeque::new(),
            ring_secs: 0,
            ring_limit: crate::IDLE_AFTER,
            keep_days: KEEP,
        }
    }

    /// An absence takes back at most this many seconds: the person's own idea of being away.
    pub fn set_ring(&mut self, secs: u64) {
        self.ring_limit = secs.max(1);
    }

    /// How many days the history keeps, today included; 0 keeps every day. A shorter window
    /// takes the days it no longer keeps at once. True when it did.
    pub fn set_keep(&mut self, days: usize) -> bool {
        self.keep_days = days;
        self.trim()
    }

    /// What is over the person's own window goes, oldest first; 0 keeps every day.
    fn trim(&mut self) -> bool {
        let mut gone = false;
        while self.keep_days > 0 && self.store.days.len() > self.keep_days {
            self.store.days.pop_first();
            gone = true;
        }
        if gone {
            self.dirty = true;
        }
        gone
    }

    fn day(&mut self, day: &str) -> &mut Day {
        if !self.store.days.contains_key(day) {
            self.store.days.insert(day.to_string(), Day::default());
            self.trim();
        }
        self.store.days.get_mut(day).expect("just inserted")
    }

    /// Another window came to the front; nobody in front is the desktop's own time.
    pub fn front(&mut self, app: Option<AppInfo>) {
        self.front = Some(app.unwrap_or_else(AppInfo::desktop));
    }

    /// The title of the window in front, for the program `key`: kept with that program on the day
    /// it was seen. Empty when Windows would not say.
    pub fn title(&mut self, key: &str, title: &str) {
        if !key.is_empty() {
            self.active_title = Some((key.to_string(), title.trim().to_string()));
        }
    }

    /// Forgets every title kept, when the person turns the switch off. The seconds stay.
    pub fn clear_titles(&mut self) {
        let mut any = false;
        for day in self.store.days.values_mut() {
            for app in day.apps.values_mut() {
                any |= !app.title.is_empty();
                app.title.clear();
            }
        }
        self.active_title = None;
        for day in self.store.days.values_mut() {
            for interval in &mut day.intervals {
                any |= !interval.title.is_empty();
                interval.title.clear();
            }
        }
        if any {
            self.dirty = true;
        }
    }

    /// `secs` seconds at the PC, in the hour `hour`, go to the program in front — or to Lintel
    /// itself while the person uses one of its windows (`own`): the island never takes the
    /// keyboard, so the window in front is another program's the whole time it is being used.
    pub fn add(&mut self, day: &str, hour: u32, secs: u64, own: bool) {
        let info = if own { AppInfo::own() } else { self.front.clone().unwrap_or_else(AppInfo::desktop) };
        let title = self.active_title.as_ref().filter(|(key, _)| key == &info.key).map_or("", |(_, title)| title.as_str()).to_string();
        let hour = (hour as usize).min(HOURS - 1);
        // A switch is counted where the seconds are, so that the pointer over the island counts
        // as Lintel coming to the front like any other window does.
        if self.last.as_deref().is_some_and(|last| last != info.key) {
            self.day(day).switches += 1;
        }
        self.last = Some(info.key.clone());
        let d = self.day(day);
        d.hours[hour] += secs;
        let app = d.apps.entry(info.key.clone()).or_default();
        app.name = info.name;
        if !info.path.is_empty() {
            app.path = info.path;
        }
        app.secs += secs;
        // The title is only ever put on the program it was read from, and never on Lintel's row.
        if !title.is_empty() { app.title = title.clone(); }
        let end = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let start = end.saturating_sub(secs);
        if let Some(previous) = d.intervals.last_mut().filter(|interval| {
            interval.key == info.key && interval.title == title && start <= interval.end.saturating_add(1) && start >= interval.end.saturating_sub(1)
        }) {
            previous.end = end.max(previous.end);
            previous.secs += secs;
        } else {
            d.intervals.push(Interval { start, end, secs, key: info.key.clone(), title });
        }
        self.dirty = true;
        self.ring.push_back((day.to_string(), hour, info.key, secs));
        self.ring_secs += secs;
        // A tick can be longer than a second: the ring holds at most the last few minutes.
        while self.ring_secs > self.ring_limit {
            let Some(old) = self.ring.pop_front() else { break };
            self.ring_secs -= old.3;
        }
    }

    /// Takes back the last `secs` counted: the person walked away and that tail was not screen time.
    pub fn undo(&mut self, secs: u64) {
        let mut left = secs;
        while left > 0 {
            let Some((day, hour, key, n)) = self.ring.pop_back() else { break };
            let take = n.min(left);
            self.ring_secs = self.ring_secs.saturating_sub(take);
            if let Some(d) = self.store.days.get_mut(&day) {
                d.hours[hour] = d.hours[hour].saturating_sub(take);
                if let Some(app) = d.apps.get_mut(&key) {
                    app.secs = app.secs.saturating_sub(take);
                }
                let empty_interval = d.intervals.last_mut().filter(|interval| interval.key == key).is_some_and(|interval| {
                    interval.secs = interval.secs.saturating_sub(take);
                    interval.end = interval.end.saturating_sub(take);
                    interval.secs == 0
                });
                if empty_interval {
                    d.intervals.pop();
                }
                if d.apps.get(&key).is_some_and(|a| a.secs == 0) {
                    d.apps.remove(&key);
                }
            }
            left -= take;
            if n > take {
                self.ring.push_back((day, hour, key, n - take));
                self.ring_secs += n - take;
            }
        }
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.store.days.clear();
        self.ring.clear();
        self.ring_secs = 0;
        self.dirty = true;
        self.save();
    }

    /// Seconds of `day` that went to `cat` (every program when it is `None`). Hidden apps are
    /// part of the day, so they count: a budget is about the time, not about the list.
    pub fn spent(&self, day: &str, prefs: &Prefs, cat: Option<&str>) -> u64 {
        self.store.days.get(day).map_or(0, |d| {
            d.apps
                .iter()
                .filter(|(key, _)| cat.map_or(true, |c| prefs.category(key) == c))
                .map(|(_, app)| app.secs)
                .sum()
        })
    }

    /// One day gone, for the person's own "delete this day": true when it was there.
    pub fn remove_day(&mut self, day: &str) -> bool {
        let gone = self.store.days.remove(day).is_some();
        if gone {
            self.ring.retain(|(d, ..)| d != day);
            self.ring_secs = self.ring.iter().map(|e| e.3).sum();
            self.dirty = true;
        }
        gone
    }

    /// Every second a program was in front, on every day: true when it had any.
    pub fn forget(&mut self, key: &str) -> bool {
        let mut any = false;
        for day in self.store.days.values_mut() {
            any |= day.apps.remove(key).is_some();
            let old_len = day.intervals.len();
            day.intervals.retain(|interval| interval.key != key);
            any |= old_len != day.intervals.len();
        }
        if any {
            self.ring.retain(|(_, _, k, _)| k != key);
            self.ring_secs = self.ring.iter().map(|e| e.3).sum();
            self.dirty = true;
        }
        any
    }

    /// The store as data, for an undo of a clear or a delete, and for an export.
    pub fn dump(&self) -> Value {
        serde_json::to_value(&self.store).unwrap_or(Value::Null)
    }

    /// Puts back what `dump` took, whole. False when it was not a store.
    pub fn restore(&mut self, data: &Value) -> bool {
        let Ok(store) = serde_json::from_value::<Store>(data.clone()) else { return false };
        self.store = store;
        self.ring.clear();
        self.ring_secs = 0;
        self.dirty = true;
        true
    }

    /// Every day and every program as lines a spreadsheet reads, with the person's categories.
    pub fn csv(&self, prefs: &Prefs) -> String {
        let mut out = String::from("date,app,seconds,category,path\n");
        for (date, day) in &self.store.days {
            let mut apps: Vec<(&String, &App)> = day.apps.iter().collect();
            apps.sort_by(|a, b| b.1.secs.cmp(&a.1.secs).then_with(|| a.0.cmp(b.0)));
            for (key, app) in apps {
                out.push_str(&format!(
                    "{date},{},{},{},{}\n",
                    quote(&app.name),
                    app.secs,
                    quote(prefs.category(key)),
                    quote(&app.path),
                ));
            }
        }
        out
    }

    pub fn save(&mut self) {
        if let (true, Some(path)) = (self.dirty, self.path.as_deref()) {
            if let Err(e) = notch_core::storage::json_save_atomic(path, &self.store) {
                notch_core::log::line(format!("breaks: screen time not saved: {e}"));
                return;
            }
        }
        self.dirty = false;
    }

    /// The whole of one day: its programs by time (all of them), the switches, the hours it was
    /// used, and every day kept with its total. `on` is whether screen time is counting now.
    pub fn snapshot(&self, day: &str, prefs: &Prefs, on: bool) -> Value {
        let d = self.store.days.get(day).cloned().unwrap_or_default();
        let total: u64 = d.apps.values().map(|a| a.secs).sum();
        // A hidden app is left out of the list; the day's totals are the day's.
        let mut apps: Vec<(&String, &App)> = d.apps.iter().filter(|(key, _)| !prefs.hidden.contains(*key)).collect();
        apps.sort_by(|a, b| b.1.secs.cmp(&a.1.secs).then_with(|| a.1.name.cmp(&b.1.name)));
        json!({
            "on": on,
            "today": today(),
            "day": day,
            "total": total,
            "switches": d.switches,
            "apps": apps
                .iter()
                .map(|(key, app)| json!({
                    "key": key,
                    "name": app.name,
                    "path": app.path,
                    // A title is only shown while the switch is on, and never one kept before it
                    // was turned off: turning it off forgets them.
                    "title": if prefs.titles { app.title.as_str() } else { "" },
                    "secs": app.secs,
                    "cat": prefs.category(key),
                }))
                .collect::<Vec<_>>(),
            "hours": d.hours,
            "days": self
                .store
                .days
                .iter()
                .map(|(date, day)| (date.clone(), json!({ "secs": day.apps.values().map(|a| a.secs).sum::<u64>(), "switches": day.switches })))
                .collect::<serde_json::Map<_, _>>(),
            "hidden": prefs.hidden,
            "cats": prefs.cats,
            "icons": prefs.icons,
            "titles": prefs.titles,
        })
    }

    /// One program on one day, and every day it was used, for its own page.
    pub fn app(&self, key: &str, day: &str, prefs: &Prefs) -> Value {
        let d = self.store.days.get(day);
        let app = d.and_then(|d| d.apps.get(key));
        let total: u64 = d.map_or(0, |d| d.apps.values().map(|a| a.secs).sum());
        let secs = app.map_or(0, |a| a.secs);
        let mut windows = BTreeMap::<String, (u64, Vec<(u64, u64)>)>::new();
        if prefs.titles {
            if let Some(d) = d {
                for interval in d.intervals.iter().filter(|interval| interval.key == key) {
                    let entry = windows.entry(interval.title.clone()).or_default();
                    entry.0 += interval.secs;
                    entry.1.push((interval.start, interval.end));
                }
            }
        }
        let mut windows: Vec<_> = windows.into_iter().collect();
        windows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
        json!({
            "key": key,
            "name": app.map_or("", |a| a.name.as_str()),
            "path": app.map_or("", |a| a.path.as_str()),
            "title": app.filter(|_| prefs.titles).map_or("", |a| a.title.as_str()),
            "cat": prefs.category(key),
            "day": day,
            "secs": secs,
            "total": total,
            "share": if total > 0 { secs as f64 / total as f64 } else { 0.0 },
            "days": self
                .store
                .days
                .iter()
                .filter_map(|(date, d)| d.apps.get(key).map(|a| (date.clone(), json!(a.secs))))
                .collect::<serde_json::Map<_, _>>(),
            "windows": windows.into_iter().map(|(title, (secs, intervals))| json!({
                "title": title,
                "secs": secs,
                "intervals": intervals.into_iter().map(|(start, end)| json!({ "start": start, "end": end })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    }

    /// The program's file, from the last day it was seen with one.
    pub fn path_of(&self, key: &str) -> Option<String> {
        self.store
            .days
            .values()
            .rev()
            .filter_map(|d| d.apps.get(key))
            .map(|a| a.path.as_str())
            .find(|p| !p.is_empty())
            .map(str::to_string)
    }

    /// The programs' files, learned from a running process or the registry: written onto today's
    /// row where it has none, so `paths` — and the icons built on it — answer from then on.
    pub fn learn(&mut self, paths: &[(String, String)]) {
        let day = today();
        let Some(d) = self.store.days.get_mut(&day) else { return };
        for (key, path) in paths {
            if let Some(app) = d.apps.get_mut(key) {
                if app.path.is_empty() && !path.is_empty() {
                    app.path = path.clone();
                    self.dirty = true;
                }
            }
        }
    }

    /// Every program's file, for its icon: the one from the last day that knew it.
    pub fn paths(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for d in self.store.days.values() {
            for (key, app) in &d.apps {
                if !app.path.is_empty() {
                    out.insert(key.clone(), app.path.clone());
                }
            }
        }
        out
    }
}

/// A CSV field, quoted when it has to be.
fn quote(text: &str) -> String {
    if text.contains(&[',', '"', '\n', '\r'][..]) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

// ── Categories ────────────────────────────────────────────────────────────────

/// Programs by what they are for, by key with a trailing version cut off ("idea64" is "idea").
/// debt: a fixed table; the person's own categories are an override, and an ignore list comes next.
const CATEGORIES: &[(&str, &[&str])] = &[
    (
        "development",
        &[
            "code", "cursor", "windsurf", "zed", "devenv", "idea", "pycharm", "webstorm", "rider", "clion", "goland",
            "rustrover", "phpstorm", "datagrip", "studio", "sublime_text", "notepad++", "windowsterminal", "wt", "cmd",
            "powershell", "pwsh", "conhost", "mintty", "wezterm-gui", "alacritty", "githubdesktop", "gitkraken", "fork",
            "docker desktop", "postman", "insomnia", "dbeaver", "ssms", "wsl", "claude",
        ],
    ),
    (
        "communication",
        &[
            "teams", "ms-teams", "slack", "discord", "zoom", "outlook", "olk", "hxoutlook", "thunderbird", "telegram",
            "whatsapp", "whatsapp.root", "signal", "skype", "webex", "webexhost", "element", "mattermost",
        ],
    ),
    (
        "browsing",
        &["chrome", "msedge", "firefox", "brave", "opera", "vivaldi", "arc", "zen", "iexplore", "librewolf", "chromium"],
    ),
    (
        "entertainment",
        &[
            "spotify", "vlc", "steam", "steamwebhelper", "epicgameslauncher", "netflix", "mpc-hc", "mpv", "wmplayer",
            "music.ui", "video.ui", "microsoft.media.player", "itunes", "applemusic", "tidal", "deezer", "plex",
            "gamebar", "xboxapp", "battle.net", "riotclientux", "leagueclientux",
        ],
    ),
    (
        "productivity",
        &[
            "winword", "excel", "powerpnt", "onenote", "onenoteim", "msaccess", "mspub", "visio", "notion", "obsidian",
            "acrobat", "acrord", "sumatrapdf", "notepad", "wordpad", "evernote", "todoist", "soffice", "soffice.bin",
            "logseq", "anki", "calibre", "ticktick",
        ],
    ),
    (
        "design",
        &[
            "figma", "photoshop", "illustrator", "indesign", "afterfx", "adobe premiere pro", "lightroom", "blender",
            "gimp", "inkscape", "mspaint", "krita", "canva", "resolve", "xd", "affinity photo", "affinity designer",
            "paintdotnet", "obs", "sketchup",
        ],
    ),
    (
        "utilities",
        &[
            "explorer", "taskmgr", "systemsettings", "searchhost", "startmenuexperiencehost", "shellexperiencehost",
            "calculatorapp", "calc", "snippingtool", "screenclippinghost", "7zfm", "winrar", "regedit", "mmc", "control",
            "everything", "powertoys", "powertoys.settings", "msiexec", "perfmon", "resmon", "procexp", DESKTOP, LINTEL,
        ],
    ),
];

pub fn category(key: &str) -> &'static str {
    let base = key.trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | '_' | ' '));
    CATEGORIES.iter().find(|(_, names)| names.contains(&key) || names.contains(&base)).map_or("other", |c| c.0)
}

/// Whether `name` is one of the categories: the page's menu offers exactly these.
pub fn known_category(name: &str) -> bool {
    CATEGORIES.iter().any(|(id, _)| *id == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(key: &str, name: &str) -> AppInfo {
        AppInfo { key: key.into(), name: name.into(), path: String::new() }
    }

    fn usage() -> Usage {
        Usage::load(None)
    }

    fn prefs() -> Prefs {
        Prefs::default()
    }

    // ── Counting ─────────────────────────────────────────────────────────────

    #[test]
    fn seconds_go_to_the_program_in_front_and_to_the_hour() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        u.add("2026-11-15", 9, 30, false);
        u.add("2026-11-15", 9, 15, false);
        let day = &u.store.days["2026-11-15"];
        assert_eq!(day.apps["code"].secs, 45);
        assert_eq!(day.apps["code"].name, "Code");
        assert_eq!(day.hours[9], 45);
        assert_eq!(day.hours[10], 0);
    }

    #[test]
    fn nobody_in_front_is_the_desktops_own_time() {
        let mut u = usage();
        u.front(None);
        u.add("2026-11-15", 8, 5, false);
        assert_eq!(u.store.days["2026-11-15"].apps[DESKTOP].secs, 5);
        assert_eq!(category(DESKTOP), "utilities");
    }

    #[test]
    fn a_switch_is_counted_when_the_program_changes_and_not_before() {
        let mut u = usage();
        // The first seconds of the day are not a switch: nothing changed.
        u.add("2026-11-15", 9, 5, false);
        assert_eq!(u.store.days["2026-11-15"].switches, 0);
        u.front(Some(app("code", "Code")));
        u.add("2026-11-15", 9, 5, false);
        u.add("2026-11-15", 9, 5, false);
        assert_eq!(u.store.days["2026-11-15"].switches, 1);
        u.front(Some(app("chrome", "Chrome")));
        u.add("2026-11-15", 9, 5, false);
        assert_eq!(u.store.days["2026-11-15"].switches, 2);
    }

    #[test]
    fn a_hour_past_the_end_of_the_day_is_the_last_bucket() {
        let mut u = usage();
        u.add("2026-11-15", 99, 3, false);
        assert_eq!(u.store.days["2026-11-15"].hours[HOURS - 1], 3);
    }

    #[test]
    fn only_the_last_thirty_days_are_kept_by_default() {
        let mut u = usage();
        for day in 1..=31 {
            u.add(&format!("2026-10-{day:02}"), 9, 1, false);
        }
        assert_eq!(u.store.days.len(), KEEP);
        assert!(!u.store.days.contains_key("2026-10-01"));
        assert!(u.store.days.contains_key("2026-10-31"));
    }

    #[test]
    fn unlimited_keeps_every_day() {
        let mut u = usage();
        u.set_keep(0);
        for day in 0..400 {
            u.add(&format!("day-{day:03}"), 9, 1, false);
        }
        assert_eq!(u.store.days.len(), 400);
        assert!(u.store.days.contains_key("day-000"));
    }

    #[test]
    fn lowering_the_window_trims_at_once() {
        let mut u = usage();
        u.set_keep(0);
        for day in 0..50 {
            u.add(&format!("day-{day:02}"), 9, 1, false);
        }
        assert!(u.set_keep(10));
        assert_eq!(u.store.days.len(), 10);
        assert!(!u.store.days.contains_key("day-00"));
        assert!(u.store.days.contains_key("day-49"));
    }

    // ── Lintel's own row ─────────────────────────────────────────────────────

    #[test]
    fn time_over_lintels_own_windows_is_lintels_row() {
        let mut u = usage();
        u.front(Some(app("chrome", "Google Chrome")));
        u.add("2026-11-15", 9, 10, false);
        u.add("2026-11-15", 9, 5, true);
        u.add("2026-11-15", 9, 5, true);
        let day = &u.store.days["2026-11-15"];
        assert_eq!(day.apps[LINTEL].secs, 10);
        assert_eq!(day.apps[LINTEL].name, "Lintel");
        assert_eq!(day.apps["chrome"].secs, 10);
        // Chrome → Lintel is one switch; staying on Lintel is not another.
        assert_eq!(day.switches, 1);
        assert_eq!(category(LINTEL), "utilities");
        assert_eq!(AppInfo::own().key, LINTEL);
    }

    #[test]
    fn the_owning_row_is_not_the_desktop_and_not_hidden_by_accident() {
        let mut u = usage();
        u.front(Some(app("chrome", "Google Chrome")));
        u.add("2026-11-15", 9, 30, false);
        u.add("2026-11-15", 9, 30, true);
        let v = u.snapshot("2026-11-15", &prefs(), true);
        assert_eq!(v["total"], 60);
        let keys: Vec<&str> = v["apps"].as_array().unwrap().iter().map(|a| a["key"].as_str().unwrap()).collect();
        assert_eq!(keys, vec!["chrome", LINTEL]); // tied on 30 s each, so by name
    }

    // ── Undo of an absence ───────────────────────────────────────────────────

    #[test]
    fn going_idle_takes_back_the_last_seconds_counted() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        for _ in 0..10 {
            u.add("2026-11-15", 9, 1, false);
        }
        u.undo(4);
        let day = &u.store.days["2026-11-15"];
        assert_eq!(day.apps["code"].secs, 6);
        assert_eq!(day.hours[9], 6);
    }

    #[test]
    fn undo_crosses_programs_and_drops_a_row_left_at_zero() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        u.add("2026-11-15", 9, 10, false);
        u.front(Some(app("chrome", "Chrome")));
        u.add("2026-11-15", 9, 5, false);
        u.undo(12);
        let day = &u.store.days["2026-11-15"];
        assert!(!day.apps.contains_key("chrome"));
        assert_eq!(day.apps["code"].secs, 3);
        assert_eq!(day.hours[9], 3);
    }

    #[test]
    fn undo_never_below_zero_and_never_past_what_was_counted() {
        let mut u = usage();
        u.add("2026-11-15", 9, 2, false);
        u.undo(60);
        assert!(u.store.days["2026-11-15"].apps.is_empty());
        // The ring is empty: nothing else moves.
        u.undo(10);
        assert_eq!(u.store.days["2026-11-15"].hours[9], 0);
    }

    #[test]
    fn the_ring_holds_only_the_idle_window() {
        let mut u = usage();
        u.set_ring(120);
        for _ in 0..200 {
            u.add("2026-11-15", 9, 1, false);
        }
        u.undo(120);
        // 200 counted, 120 of them still in the ring: 80 stand.
        assert_eq!(u.store.days["2026-11-15"].apps[DESKTOP].secs, 80);
    }

    // ── The day the page asked for ───────────────────────────────────────────

    #[test]
    fn a_local_date_is_taken_and_anything_else_is_not() {
        assert_eq!(parse_day("2026-11-15").as_deref(), Some("2026-11-15"));
        assert_eq!(parse_day("2024-02-29").as_deref(), Some("2024-02-29"));
        for bad in ["2026-1-15", "2026-11-15 ", "2026-13-01", "2026-00-10", "2026-11-32", "2026/11/15", "today", "", "2026_11_15"] {
            assert_eq!(parse_day(bad), None, "{bad} was taken");
        }
    }

    // ── What the page reads ──────────────────────────────────────────────────

    #[test]
    fn a_hidden_app_leaves_the_list_and_keeps_its_time_in_the_day() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        u.add("2026-11-15", 9, 30, false);
        u.front(Some(app("chrome", "Chrome")));
        u.add("2026-11-15", 9, 20, false);
        let mut p = prefs();
        p.hidden.insert("chrome".into());
        let v = u.snapshot("2026-11-15", &p, true);
        assert_eq!(v["total"], 50);
        assert_eq!(v["apps"].as_array().unwrap().len(), 1);
        assert_eq!(v["apps"][0]["key"], "code");
    }

    #[test]
    fn the_persons_own_category_wins_and_is_reported() {
        let mut u = usage();
        u.front(Some(app("chrome", "Chrome")));
        u.add("2026-11-15", 9, 10, false);
        let mut p = prefs();
        p.cats.insert("chrome".into(), "development".into());
        assert_eq!(p.category("chrome"), "development");
        assert_eq!(u.snapshot("2026-11-15", &p, true)["apps"][0]["cat"], "development");
        assert_eq!(p.category("firefox"), "browsing");
        assert_eq!(p.category("a-program-nobody-knows"), "other");
    }

    #[test]
    fn the_snapshot_carries_every_kept_day_and_its_switches() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        u.add("2026-11-14", 9, 30, false);
        u.front(Some(app("chrome", "Chrome")));
        u.add("2026-11-15", 9, 60, false);
        u.add("2026-11-15", 9, 60, false);
        let v = u.snapshot("2026-11-15", &prefs(), true);
        assert_eq!(v["day"], "2026-11-15");
        assert_eq!(v["total"], 120);
        assert_eq!(v["switches"], 1);
        assert_eq!(v["days"]["2026-11-14"]["secs"], 30);
        assert_eq!(v["days"]["2026-11-15"]["secs"], 120);
        // A day with nothing in it is an empty day, not an error.
        let empty = u.snapshot("2026-11-01", &prefs(), true);
        assert_eq!(empty["total"], 0);
        assert_eq!(empty["apps"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn one_program_on_one_day_has_its_share_and_its_own_days() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        u.add("2026-11-14", 9, 30, false);
        u.add("2026-11-15", 9, 30, false);
        u.front(Some(app("chrome", "Chrome")));
        u.add("2026-11-15", 9, 90, false);
        let v = u.app("code", "2026-11-15", &prefs());
        assert_eq!(v["name"], "Code");
        assert_eq!(v["secs"], 30);
        assert_eq!(v["total"], 120);
        assert_eq!(v["share"], 0.25);
        assert_eq!(v["days"]["2026-11-14"], 30);
        assert_eq!(v["days"]["2026-11-15"], 30);
        // A program that was never in front: no name, no seconds, no share.
        let none = u.app("firefox", "2026-11-15", &prefs());
        assert_eq!(none["secs"], 0);
        assert_eq!(none["share"], 0.0);
    }

    #[test]
    fn the_budget_counts_the_day_or_one_category_of_it() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        u.add("2026-11-15", 9, 30, false);
        u.front(Some(app("chrome", "Chrome")));
        u.add("2026-11-15", 9, 40, false);
        u.add("2026-11-15", 9, 20, true);
        let p = prefs();
        assert_eq!(u.spent("2026-11-15", &p, None), 90);
        assert_eq!(u.spent("2026-11-15", &p, Some("development")), 30);
        assert_eq!(u.spent("2026-11-15", &p, Some("browsing")), 40);
        assert_eq!(u.spent("2026-11-15", &p, Some("utilities")), 20); // Lintel's own row
        assert_eq!(u.spent("2026-11-01", &p, None), 0);
    }

    // ── Window titles ────────────────────────────────────────────────────────

    #[test]
    fn a_title_goes_to_its_own_program_and_is_shown_only_while_the_switch_is_on() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        u.title("code", "  main.rs - lintel  ");
        u.add("2026-11-15", 9, 5, false);
        assert_eq!(u.store.days["2026-11-15"].apps["code"].title, "main.rs - lintel");
        // Another program in front is never given the title of the one before it.
        u.front(Some(app("chrome", "Chrome")));
        u.title("chrome", "");
        u.add("2026-11-15", 9, 5, false);
        assert_eq!(u.store.days["2026-11-15"].apps["chrome"].title, "");

        let code_title = |v: &Value| {
            v["apps"].as_array().unwrap().iter().find(|a| a["key"] == "code").unwrap()["title"].clone()
        };
        let on = Prefs { titles: true, ..prefs() };
        let shown = u.snapshot("2026-11-15", &on, true);
        assert_eq!(shown["titles"], true);
        assert_eq!(code_title(&shown), "main.rs - lintel");
        // Off: what was kept is not shown.
        let off = u.snapshot("2026-11-15", &prefs(), true);
        assert_eq!(off["titles"], false);
        assert_eq!(code_title(&off), "");

        // Turning the switch off forgets them; the seconds stay.
        u.clear_titles();
        assert_eq!(u.store.days["2026-11-15"].apps["code"].title, "");
        assert_eq!(u.store.days["2026-11-15"].apps["code"].secs, 5);
    }

    // ── Deleting, forgetting, restoring, exporting ───────────────────────────

    #[test]
    fn clear_takes_every_day_and_a_dump_puts_them_back() {
        let mut u = usage();
        u.front(Some(app("code", "Code")));
        u.add("2026-11-15", 9, 30, false);
        let kept = u.dump();
        u.clear();
        assert_eq!(u.snapshot("2026-11-15", &prefs(), true)["total"], 0);
        assert!(u.restore(&kept));
        assert_eq!(u.snapshot("2026-11-15", &prefs(), true)["total"], 30);
        // Anything that is not a store is refused rather than wiping the history.
        assert!(!u.restore(&json!({ "days": 7 })));
        assert_eq!(u.store.days.len(), 1);
        assert!(!u.restore(&Value::Null));
    }

    #[test]
    fn one_day_can_be_deleted() {
        let mut u = usage();
        u.add("2026-11-14", 9, 10, false);
        u.add("2026-11-15", 9, 10, false);
        assert!(u.remove_day("2026-11-14"));
        assert!(!u.remove_day("2026-11-14"));
        assert!(!u.store.days.contains_key("2026-11-14"));
        assert!(u.store.days.contains_key("2026-11-15"));
    }

    #[test]
    fn forgetting_a_program_takes_it_from_every_day_and_leaves_the_rest() {
        let mut u = usage();
        u.front(Some(app("chrome", "Chrome")));
        u.add("2026-11-14", 9, 10, false);
        u.add("2026-11-15", 9, 20, false);
        u.front(Some(app("code", "Code")));
        u.add("2026-11-15", 9, 5, false);
        assert!(u.forget("chrome"));
        assert!(!u.forget("chrome"));
        assert_eq!(u.path_of("chrome"), None);
        let v = u.snapshot("2026-11-15", &prefs(), true);
        assert_eq!(v["total"], 5);
        assert_eq!(v["days"]["2026-11-14"]["secs"], 0);
    }

    #[test]
    fn a_program_path_is_learned_once_and_kept_for_opening() {
        let mut u = usage();
        u.add("2026-11-15", 9, 5, false);
        // `learn` writes onto today's row only, so the day is `today`.
        let day = today();
        u.add(&day, 9, 5, false);
        u.learn(&[(DESKTOP.to_string(), r"C:\Windows\explorer.exe".to_string())]);
        assert_eq!(u.path_of(DESKTOP).as_deref(), Some(r"C:\Windows\explorer.exe"));
        assert_eq!(u.paths()[DESKTOP], r"C:\Windows\explorer.exe");
    }

    #[test]
    fn a_csv_holds_a_line_per_program_and_quotes_what_needs_it() {
        let mut u = usage();
        u.front(Some(AppInfo { key: "code".into(), name: "Code, the editor".into(), path: r"C:\a\code.exe".into() }));
        u.add("2026-11-15", 9, 30, false);
        let mut p = prefs();
        p.cats.insert("code".into(), "development".into());
        let csv = u.csv(&p);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "date,app,seconds,category,path");
        assert_eq!(lines[1], "2026-11-15,\"Code, the editor\",30,development,C:\\a\\code.exe");
        assert_eq!(lines.len(), 2);
    }
}
