// Breaks probe: the Windows-facing parts on this PC, the break rule over a made-up timeline, and
// the feature's engine run as the app runs it. It reads and it draws; it plays no sound and sends
// no input. The fade is put on every screen twice, for about two seconds and for about one.
//
//   cargo run -p notch-feat-breaks --example breaks-probe
//   cargo run -p notch-feat-breaks --example breaks-probe -- --fade 15    (only the fade: try Esc, try a click)

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notch_feat_breaks::fade::{Fade, CLASS};
use notch_feat_breaks::rule::{Config, Ev, Kind, Plan, Rule, RECHECK};
use notch_feat_breaks::usage::{self, Usage};
use notch_feat_breaks::{sys, Engine, IDLE_AFTER};
use serde_json::{json, Value};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetSystemMetrics, SM_CMONITORS};

fn wait(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn front(fg: &svc_sysevents::Foreground) {
    match sys::app_of(fg.hwnd, fg.pid, &fg.exe) {
        Some(app) => println!("  in front: {} [{}] path={:?} category={} fullscreen={}", app.name, app.key, app.path, usage::category(&app.key), fg.fullscreen),
        None => println!("  in front: a window Windows will not name (exe {:?})", fg.exe),
    }
}

fn fade_on_screen() -> bool {
    unsafe { FindWindowW(CLASS, None) }.is_ok()
}

fn main() {
    // As in the app: a monitor's rectangle is in real pixels.
    unsafe { _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    // `-- --fade 15`: only the fade, for that many seconds, to look at it and to try Esc and a click.
    let args: Vec<String> = std::env::args().collect();
    if let Some(secs) = args.iter().position(|a| a == "--fade").and_then(|i| args.get(i + 1)?.parse::<u64>().ok()) {
        let t0 = Instant::now();
        let fade = Fade::show(secs, "Look at something far away".into(), "Esc or click to skip".into(), || println!("skipped by Esc or a click"));
        while !fade.gone() {
            wait(50);
        }
        println!("gone after {} ms; a window left: {}", t0.elapsed().as_millis(), fade_on_screen());
        return;
    }

    println!("== idle clock (GetLastInputInfo)");
    let a = sys::idle_secs();
    wait(2000);
    let b = sys::idle_secs();
    let note = if b >= a + 2 { "no input meanwhile" } else { "something typed or moved the pointer meanwhile" };
    println!("  idle for {a} s, and two seconds later for {b} s: {note}");
    println!("  away after {IDLE_AFTER} s without input: {}", if b >= IDLE_AFTER { "away now" } else { "at the PC now" });

    println!("== microphone (ConsentStore: LastUsedTimeStop = 0 means open)");
    let mic = sys::mic_in_use();
    println!("  {} app(s) have it open: {mic:?} -> a break would {}", mic.len(), if mic.is_empty() { "start" } else { "wait" });

    println!("== window in front (the WinEvent hook of svc-sysevents), 4 s");
    front(&svc_sysevents::foreground());
    let changes = Arc::new(AtomicU32::new(0));
    let sub = svc_sysevents::on_foreground({
        let changes = changes.clone();
        move |fg| {
            changes.fetch_add(1, Relaxed);
            front(fg);
        }
    });
    wait(4000);
    drop(sub);
    println!("  {} change(s) in 4 s, told by the hook, never asked for", changes.load(Relaxed));

    println!("== the break rule over a made-up timeline (eye: every 60 s for 5 s; movement: every 200 s for 10 s)");
    rule_timeline();

    println!("== screen time accounting (names and seconds only)");
    screen_time();

    println!("== the engine, as the app runs it");
    engine_live();

    println!("== the fade");
    let monitors = unsafe { GetSystemMetrics(SM_CMONITORS) };
    let skipped = Arc::new(AtomicBool::new(false));
    let told = skipped.clone();
    let t0 = Instant::now();
    let fade = Fade::show(2, "Look at something far away".into(), "Esc or click to skip".into(), move || told.store(true, Relaxed));
    println!("  {} window(s) for {monitors} monitor(s), on screen: {}", fade.windows, fade_on_screen());
    assert_eq!(fade.windows as i32, monitors, "one window per monitor");
    assert!(fade_on_screen());
    while !fade.gone() && t0.elapsed() < Duration::from_secs(6) {
        wait(50);
    }
    println!("  gone by itself after {} ms (a 2 s countdown, then the fade-out); a window left: {}", t0.elapsed().as_millis(), fade_on_screen());
    assert!(fade.gone() && !fade_on_screen(), "it must end with its countdown");
    if skipped.load(Relaxed) {
        println!("  (someone pressed Esc or clicked meanwhile: that ended it early)");
    }

    // The real path: the feature fades the screens for a break, and is switched off a second in.
    let engine = Engine::new(Arc::new(|_| {}), std::path::PathBuf::from("unused"));
    engine.apply(&json!({ "eye": true, "eyeEvery": 20, "eyeSecs": 60, "fade": true }));
    engine.take(Kind::Eye);
    wait(1000);
    println!("  a 60 s break with the fade on: on screen: {}", fade_on_screen());
    assert!(fade_on_screen());
    let t0 = Instant::now();
    engine.stop();
    println!("  the feature switched off: stop returned after {} ms; a window left: {}", t0.elapsed().as_millis(), fade_on_screen());
    assert!(!fade_on_screen(), "stop must give the screens back before it returns");

    println!("probe ok");
}

/// Threads of this process, as Windows counts them.
fn threads() -> usize {
    let mut n = 0;
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return 0 };
        let mut entry = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut more = Thread32First(snapshot, &mut entry).is_ok();
        while more {
            n += (entry.th32OwnerProcessID == std::process::id()) as usize;
            more = Thread32Next(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    n
}

/// The real engine with the app's settings: off, on, a break by hand, snoozed, off again, stopped.
fn engine_live() {
    // Inside the repository's build folder, and removed at the end.
    let file = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/breaks-probe-screen-time.json");
    let _ = std::fs::remove_file(&file);
    let t0 = Instant::now();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let engine = Engine::new(
        Arc::new({
            let seen = seen.clone();
            move |v: Value| {
                if let Some(ev) = v.get("ev") {
                    println!("  [{:>5} ms] {} {} {}", t0.elapsed().as_millis(), ev["phase"], ev["kind"], ev["why"]);
                    seen.lock().unwrap().push(format!("{} {} {}", ev["phase"].as_str().unwrap_or(""), ev["kind"].as_str().unwrap_or(""), ev["why"].as_str().unwrap_or("")));
                }
            }
        }),
        file.clone(),
    );
    let settings = |eye: bool, screen_time: bool| {
        json!({ "eye": eye, "eyeEvery": 20, "eyeSecs": 5, "move": false, "moveEvery": 50, "moveMins": 2, "snooze": 5,
                "holdCall": true, "holdFullscreen": true, "fade": false, "sound": false, "screenTime": screen_time })
    };
    let before = threads();
    engine.apply(&settings(false, false));
    wait(300);
    println!("  everything off: {} thread(s) before, {} after -> the feature runs nothing", before, threads());
    assert!(threads() <= before, "with everything off nothing may start");

    engine.apply(&settings(true, true));
    wait(2500);
    println!("  eye breaks and screen time on: {} thread(s) (the loop, and the service that listens to Windows)", threads());
    println!("  status: {}", engine.status_now());
    let usage = engine.usage(None, false);
    println!("  screen time after 2.5 s: {} s counted, apps {} ({})", usage["total"], usage["apps"],
        if sys::idle_secs() >= IDLE_AFTER { "the person is away: the clock stands still" } else { "the person is here" });

    engine.take(Kind::Eye);
    wait(6500);
    assert_eq!(*seen.lock().unwrap(), ["start eye ", "end eye done"], "a 5 s break by hand starts and ends by itself");
    engine.take(Kind::Eye);
    wait(1200);
    engine.end("snooze");
    assert_eq!(seen.lock().unwrap().last().map(String::as_str), Some("end eye snooze"));
    println!("  status after the snooze: {}", engine.status_now());

    engine.apply(&settings(false, false));
    wait(1500);
    println!("  everything off again: {} thread(s)", threads());
    engine.stop();
    wait(300);
    println!("  stopped: {} thread(s) (what Windows' own thread pool keeps for a while is not the feature's)", threads());
    let _ = std::fs::remove_file(&file);
}

fn rule_timeline() {
    let mut rule = Rule::default();
    rule.cfg = Config {
        eye: Plan { on: true, every: 60, secs: 5 },
        mov: Plan { on: true, every: 200, secs: 10 },
        snooze: 20,
        warn: 10,
    };
    let mut sim = Sim { rule, t: 0, log: Vec::new() };
    let eye = Ev::Start(Kind::Eye, 5);

    sim.span("at the PC", 60, false, false);
    assert_eq!(sim.log, [(50, Ev::Soon(Kind::Eye)), (60, eye)], "20 minutes in small: announced, then started");
    sim.span("the break runs", 5, false, false);
    assert_eq!(sim.log.last(), Some(&(65, Ev::End(Kind::Eye, "done"))));

    sim.span("at the PC", 30, false, false);
    let n = sim.log.len();
    sim.span("idle (no input)", 20, true, false);
    assert_eq!(sim.log.len(), n, "nothing happens while idle");
    let back = sim.t;
    let at = sim.until("back at the PC", |ev| ev == eye);
    assert_eq!(at, back + 60, "the idle time was the break: a full interval again");
    sim.by_hand("Skip", |rule| rule.skip());
    assert_eq!(sim.log.last(), Some(&(at, Ev::End(Kind::Eye, "skip"))));

    sim.span("at the PC", 40, false, false);
    let n = sim.log.len();
    sim.span("locked", 30, true, false);
    assert_eq!(sim.log.len(), n, "nothing happens while locked");

    let unlocked = sim.t;
    sim.span("at the PC, on a call", 85, false, true);
    assert_eq!(sim.log.last(), Some(&(unlocked + 60, Ev::Held(Kind::Eye))), "due a full interval after the lock, and held");
    let hung_up = sim.t;
    let at = sim.until("the call is over", |ev| ev == eye);
    assert!(at <= hung_up + RECHECK, "it starts within {RECHECK} s of the call ending");
    sim.by_hand("Snooze", |rule| rule.stop("snooze"));
    let again = sim.until("at the PC", |ev| ev == eye);
    assert_eq!(again, at + 20, "snoozed: due again after 20 s at the PC");
    sim.span("the break runs", 5, false, false);

    sim.until("at the PC until the movement break", |ev| ev == Ev::Start(Kind::Move, 10));
    sim.span("the break runs", 10, false, false);
    assert_eq!(sim.log.last().map(|e| e.1), Some(Ev::End(Kind::Move, "done")));
    let status = sim.rule.status();
    println!("  status at the end: {status}");
    assert_eq!(status["eye"], 60, "standing up rested the eyes too");
    println!("  rule ok: {} events over {} made-up seconds", sim.log.len(), sim.t);
}

/// The rule fed one made-up second at a time.
struct Sim {
    rule: Rule,
    t: u64,
    log: Vec<(u64, Ev)>,
}

impl Sim {
    /// `away` is idle, locked or asleep; `busy` a call or a full-screen app.
    fn second(&mut self, away: bool, busy: bool) -> Vec<Ev> {
        self.t += 1;
        let evs = self.rule.tick(1, away, || busy);
        for ev in &evs {
            println!("  t={:>3}    -> {ev:?}", self.t);
            self.log.push((self.t, *ev));
        }
        evs
    }

    fn span(&mut self, what: &str, secs: u64, away: bool, busy: bool) {
        println!("  t={:>3}  {what}, {secs} s", self.t + 1);
        for _ in 0..secs {
            self.second(away, busy);
        }
    }

    /// At the PC and free until `wanted` happens: the second it did.
    fn until(&mut self, what: &str, wanted: impl Fn(Ev) -> bool) -> u64 {
        println!("  t={:>3}  {what}", self.t + 1);
        for _ in 0..1000 {
            if self.second(false, false).into_iter().any(&wanted) {
                return self.t;
            }
        }
        panic!("{what}: it never happened");
    }

    fn by_hand(&mut self, what: &str, act: impl FnOnce(&mut Rule) -> Option<Ev>) {
        let ev = act(&mut self.rule);
        println!("  t={:>3}  {what} -> {ev:?}", self.t);
        self.log.extend(ev.map(|e| (self.t, e)));
    }
}

fn screen_time() {
    let prefs = usage::Prefs::default();
    let mut usage = Usage::load(None);
    let app = |key: &str, name: &str| {
        Some(usage::AppInfo { key: key.to_string(), name: name.to_string(), path: format!("C:\\Program Files\\{name}\\{key}.exe") })
    };
    let day = "2026-10-11";
    usage.front(app("code", "Visual Studio Code"));
    usage.add(day, 9, 1500, false);
    usage.front(app("chrome", "Google Chrome"));
    usage.add(day, 10, 600, false);
    // Nobody in front (the desktop) is screen time, not nothing.
    usage.front(None);
    usage.add(day, 14, 30, false);
    usage.front(app("code", "Visual Studio Code"));
    usage.add(day, 15, 300, false);

    let snap = usage.snapshot(day, &prefs, true);
    println!("  the day: {} s, {} switch(es), {} app(s): {}", snap["total"], snap["switches"], snap["apps"].as_array().map_or(0, |a| a.len()), snap["apps"]);
    assert_eq!((snap["total"].as_u64(), snap["switches"].as_u64()), (Some(2430), Some(3)));
    assert_eq!(
        snap["apps"][0],
        serde_json::json!({ "key": "code", "name": "Visual Studio Code", "path": "C:\\Program Files\\Visual Studio Code\\code.exe", "title": "", "secs": 1800, "cat": "development" })
    );
    assert!(snap["apps"].as_array().unwrap().iter().any(|a| a["key"] == "desktop"), "the desktop's time is a row too");
    let hours: u64 = snap["hours"].as_array().unwrap().iter().map(|h| h.as_u64().unwrap()).sum();
    assert_eq!(hours, snap["total"].as_u64().unwrap(), "the hours are the day's seconds");
    assert_eq!(snap["hours"][9], 1500);

    // Hidden apps leave the list and not the day.
    let mut hidden = prefs.clone();
    hidden.hidden.insert("chrome".into());
    let hid = usage.snapshot(day, &hidden, true);
    assert!(!hid["apps"].as_array().unwrap().iter().any(|a| a["key"] == "chrome"), "a hidden app is not listed");
    assert_eq!(hid["total"], 2430, "its time is still the day's");
    println!("  hidden chrome: {} row(s), total {} s", hid["apps"].as_array().map_or(0, |a| a.len()), hid["total"]);

    // The person's own category wins over the table.
    let mut cats = prefs.clone();
    cats.cats.insert("code".into(), "productivity".into());
    let recat = usage.snapshot(day, &cats, true);
    let code = recat["apps"].as_array().unwrap().iter().find(|a| a["key"] == "code").unwrap();
    assert_eq!(code["cat"], "productivity");

    // One program's own page, and where its file is.
    let one = usage.app("code", day, &prefs);
    println!("  Visual Studio Code: {} s of {} s ({}%): {}", one["secs"], one["total"], (one["share"].as_f64().unwrap_or(0.0) * 100.0).round(), one["days"]);
    assert_eq!(one["secs"], 1800);
    assert_eq!(usage.path_of("code").as_deref(), Some("C:\\Program Files\\Visual Studio Code\\code.exe"));
    assert!(one["days"].as_object().unwrap().contains_key(day));

    // Thirty days are kept by default, and not thirty-one.
    for d in 12..=45 {
        usage.add(&format!("2026-10-{d}"), 12, 60, false);
    }
    let days = usage.snapshot(day, &prefs, true)["days"].as_object().map_or(0, |d| d.len());
    println!("  days kept after thirty-five days of use: {days}");
    assert_eq!(days, 30);

    // The person's own window: unlimited keeps every day.
    let mut all = Usage::load(None);
    all.set_keep(0);
    for d in 12..=56 {
        all.add(&format!("2026-10-{d}"), 12, 60, false);
    }
    let kept = all.snapshot(day, &prefs, true)["days"].as_object().map_or(0, |d| d.len());
    println!("  days kept with unlimited: {kept}");
    assert_eq!(kept, 45);

    // Going idle takes back the tail the person was already walking away from.
    let mut idle = Usage::load(None);
    idle.front(app("chrome", "Google Chrome"));
    for _ in 0..200 {
        idle.add(day, 12, 1, false);
    }
    idle.undo(IDLE_AFTER);
    let after = idle.snapshot(day, &prefs, true);
    println!("  200 s counted, {IDLE_AFTER} s taken back: {} s left", after["total"]);
    assert_eq!(after["total"], 80);
    assert_eq!(after["apps"][0]["secs"], 80);
    assert_eq!(after["hours"][12], 80);

    // Lintel's own windows are a row of their own, apart from the desktop and from whatever was in
    // front behind the island.
    let mut mine = Usage::load(None);
    mine.front(app("chrome", "Google Chrome"));
    mine.add(day, 9, 60, false);
    mine.add(day, 9, 90, true);
    let own = mine.snapshot(day, &prefs, true);
    println!("  the pointer over the island: {}", own["apps"]);
    assert!(
        own["apps"].as_array().unwrap().iter().any(|a| a["key"] == usage::LINTEL && a["secs"] == 90),
        "Lintel is a row of its own"
    );
    assert!(!own["apps"].as_array().unwrap().iter().any(|a| a["key"] == "explorer"), "and not Windows Explorer's");
    assert_eq!(own["switches"], 1, "the pointer over the island is a switch like a window");

    for key in ["idea64", "gimp-2.10", "ms-teams", "explorer", "some-game"] {
        print!("  {key}={}", usage::category(key));
    }
    println!();
    assert!(usage::known_category("design") && !usage::known_category("nonsense"));
}
