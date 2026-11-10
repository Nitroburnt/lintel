// Which apps hold the microphone open, and which hold the camera.
//
// The microphone comes from the live audio sessions on the microphones
// (`svc_audio::capture_in_use`): a capture stream that is running, and the process behind it —
// the same state Windows' own microphone icon follows. It does not come from the ConsentStore
// registry under
// HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone:
// that record (one subkey per packaged app, and under `NonPackaged` one per desktop exe, its path
// with `#` for `\`; `LastUsedTimeStop` is 0 while an app records) is usage history for the privacy
// page. Windows does not write it when a process holding the microphone is killed, so a row read
// from it can never clear again: the island's "Microphone in use" stayed up until the app was
// restarted or the page was refreshed.
//
// Two things the live reading adds. A process this app may not open — an elevated or protected one
// — is still reported, by pid: a privacy indicator that hides what it cannot inspect hides the
// wrong apps. And an app stays listed for `GRACE` after it stops, so one that pauses between
// pushes does not blink and `call-quiet` is not turned off in the middle of a call. Nothing wakes
// the watcher when that time is up, so a thread of its own sleeps until it is.
//
// The camera has no session API: it keeps the same record under `webcam`, read the way it always
// was, with `RegNotifyChangeKeyValue` waking the watcher on a change — the registry is only ever
// read and nothing polls. That record still has the microphone's old defect: a camera app that is
// killed leaves its row up until something else writes the key (docs/FEATURES.md says so).

use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use svc_audio::Capture;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegNotifyChangeKeyValue, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_NOTIFY, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE};

const STORE: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone");
// The camera keeps its record here.
const CAMERA: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\webcam");

/// An app that stopped is still listed this long: a call app that mutes itself, or a
/// push-to-talk key held and let go, must not blink the row or turn `call-quiet` off.
pub const GRACE: Duration = Duration::from_secs(2);

fn open(parent: HKEY, path: PCWSTR, access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(parent, path, None, access, &mut key) }.is_ok().then_some(key)
}

fn subkeys(key: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        let ok = unsafe {
            RegEnumKeyExW(key, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
        };
        if ok.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    out
}

fn qword(key: HKEY, name: PCWSTR) -> u64 {
    let mut v = 0u64;
    let mut size = 8u32;
    let ok = unsafe { RegQueryValueExW(key, name, None, None, Some(&mut v as *mut u64 as *mut u8), Some(&mut size)) };
    if ok.is_ok() && size == 8 { v } else { 0 }
}

/// Is this app recording now? Start was written and Stop is still 0.
fn recording(parent: HKEY, name: &str) -> bool {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let Some(key) = open(parent, PCWSTR(wide.as_ptr()), KEY_READ) else { return false };
    let on = qword(key, w!("LastUsedTimeStart")) > 0 && qword(key, w!("LastUsedTimeStop")) == 0;
    unsafe { _ = RegCloseKey(key) }
    on
}

/// "Microsoft.WindowsCamera_8wekyb3d8bbwe" -> "WindowsCamera"; "C:#Apps#Teams.exe" and the same
/// path with backslashes -> "Teams".
fn nice(name: &str, packaged: bool) -> String {
    if packaged {
        let base = name.split('_').next().unwrap_or(name);
        base.rsplit('.').next().unwrap_or(base).to_string()
    } else {
        let file = name.rsplit(['#', '\\']).next().unwrap_or(name);
        file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string()
    }
}

/// What the row says for one holding process: the executable's name, or its pid when
/// this app may not ask for the path (an elevated or protected process).
fn label(capture: &Capture) -> String {
    match &capture.exe {
        Some(path) => nice(path, false),
        None => format!("PID {}", capture.pid),
    }
}

/// Whether this capture is Lintel itself: this process, or a child running the same
/// executable — the probe's `self` mode, and a helper that records for Lintel. The pid
/// settles this process; a path that only nearly matches settles nothing else.
fn ours(capture: &Capture, me: u32, exe: Option<&str>) -> bool {
    if capture.pid == me {
        return true;
    }
    match (capture.exe.as_deref(), exe) {
        (Some(path), Some(mine)) => path.eq_ignore_ascii_case(mine),
        _ => false,
    }
}

/// The apps with the microphone open now, other than Lintel itself (its dictation is announced
/// by its own HUD, not as a stranger). Sorted, without repeats.
pub fn in_use() -> Vec<String> {
    let me = std::process::id();
    let exe = std::env::current_exe().ok().map(|p| p.to_string_lossy().to_lowercase());
    let mut out: Vec<String> = svc_audio::capture_in_use()
        .unwrap_or_default()
        .iter()
        .filter(|capture| !ours(capture, me, exe.as_deref()))
        .map(label)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The apps with the camera open now.
pub fn camera_in_use() -> Vec<String> {
    holders(CAMERA)
}

/// What the microphone registry remembers, with this program in it: the probe prints it beside the
/// live reading, which is what the app shows.
pub fn everyone() -> Vec<String> {
    holders(STORE)
}

/// What the registry under `path` records as holding it open right now (the camera's record, or
/// the microphone's for `everyone`).
fn holders(path: PCWSTR) -> Vec<String> {
    let Some(store) = open(HKEY_CURRENT_USER, path, KEY_READ) else { return Vec::new() };
    let mut out = Vec::new();
    for name in subkeys(store) {
        if name == "NonPackaged" {
            let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
            let Some(loose) = open(store, PCWSTR(wide.as_ptr()), KEY_READ) else { continue };
            for app in subkeys(loose) {
                if recording(loose, &app) {
                    out.push(nice(&app, false));
                }
            }
            unsafe { _ = RegCloseKey(loose) }
        } else if recording(store, &name) {
            out.push(nice(&name, true));
        }
    }
    unsafe { _ = RegCloseKey(store) }
    out.sort();
    out.dedup();
    out
}

/// The apps that just went quiet: each stays listed for `GRACE` from the moment it was
/// first missed, so a stream that pauses and starts again reads as one app that was
/// there all along.
#[derive(Default)]
struct Grace {
    /// What was listed last time, so that a name that disappears is noticed.
    listed: Vec<String>,
    /// The ones that went quiet, and when they were first missed.
    gone: Vec<(String, Instant)>,
}

impl Grace {
    /// The names to show at `at`: what is in use, plus what went quiet less than `GRACE` ago.
    fn apply(&mut self, now: Vec<String>, at: Instant) -> Vec<String> {
        for name in &self.listed {
            if !now.contains(name) && !self.gone.iter().any(|(gone, _)| gone == name) {
                self.gone.push((name.clone(), at));
            }
        }
        // Held for the grace from when it was missed, not from when it was last seen: a
        // quiet call gives no event in between, and that wait must not eat the grace.
        self.gone.retain(|(name, since)| !now.contains(name) && at.saturating_duration_since(*since) < GRACE);
        let mut out = now;
        out.extend(self.gone.iter().map(|(name, _)| name.clone()));
        out.sort();
        out.dedup();
        self.listed = out.clone();
        out
    }

    /// How long until the first held-over app goes: nothing else would wake the watcher then.
    fn left(&self, at: Instant) -> Option<Duration> {
        self.gone.iter().map(|(_, since)| GRACE.saturating_sub(at.saturating_duration_since(*since))).min()
    }
}

/// The two lists as the page gets them: (microphone, camera).
type Pair = (Vec<String>, Vec<String>);

/// Why the grace thread was woken: something was listed or held over, or the watcher is going.
enum Wake {
    Changed,
    Stop,
}

/// Reads the lists and tells the page when the pair changes. One reader for both halves: the
/// microphone's session subscription and the camera's registry watcher each read their own half,
/// and the pair is told once however many halves moved.
struct Report {
    on: Arc<dyn Fn(Vec<String>, Vec<String>) + Send + Sync>,
    grace: Mutex<Grace>,
    last: Mutex<Option<Pair>>,
    /// Wakes the grace thread when an app is held over.
    wake: mpsc::Sender<Wake>,
}

impl Report {
    fn new(on: impl Fn(Vec<String>, Vec<String>) + Send + Sync + 'static, wake: mpsc::Sender<Wake>) -> Self {
        Self { on: Arc::new(on), grace: Mutex::new(Grace::default()), last: Mutex::new(None), wake }
    }

    /// The microphone woke this half: read it, and keep the camera half as it was.
    fn microphone(&self) {
        let at = Instant::now();
        let (now, left) = {
            let mut grace = self.grace.lock().unwrap_or_else(|e| e.into_inner());
            let now = grace.apply(in_use(), at);
            (now, grace.left(at))
        };
        self.tell(Some(now), None);
        if left.is_some() {
            // A held-over app has no event of its own when its time is up.
            let _ = self.wake.send(Wake::Changed);
        }
    }

    /// The camera woke this half: read it, and keep the microphone half as it was.
    fn camera(&self) {
        self.tell(None, Some(camera_in_use()));
    }

    /// Both, at the start, as the page is told nothing when it loads.
    fn both(&self) {
        self.tell(Some(in_use()), Some(camera_in_use()));
    }

    /// How long until a held-over app goes, if one is held over.
    fn grace_left(&self) -> Option<Duration> {
        self.grace.lock().unwrap_or_else(|e| e.into_inner()).left(Instant::now())
    }

    /// Tells the page when the pair moved. Nothing of this hub is locked while the
    /// subscriber runs: one that reads the state back must not deadlock here.
    fn tell(&self, microphone: Option<Vec<String>>, camera: Option<Vec<String>>) {
        let (on, now) = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let now = match last.as_ref() {
                Some((was_mic, was_camera)) => (
                    microphone.unwrap_or_else(|| was_mic.clone()),
                    camera.unwrap_or_else(|| was_camera.clone()),
                ),
                None => (microphone.unwrap_or_default(), camera.unwrap_or_default()),
            };
            if last.as_ref() == Some(&now) {
                return;
            }
            *last = Some(now.clone());
            (self.on.clone(), now)
        };
        on(now.0, now.1);
    }
}

/// A watcher; dropping it ends all three halves.
pub struct Watch {
    /// The microphone's half: the live capture sessions, alive while this is.
    _sessions: svc_audio::SessionSub,
    stop: Arc<AtomicIsize>,
    wake: Option<mpsc::Sender<Wake>>,
    grace: Option<JoinHandle<()>>,
    camera: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        // The grace thread first: it reads the microphone too, and stops on its own.
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(Wake::Stop);
        }
        if let Some(grace) = self.grace.take() {
            _ = grace.join();
        }
        unsafe { _ = SetEvent(HANDLE(self.stop.load(Relaxed) as *mut _)) };
        if let Some(camera) = self.camera.take() {
            _ = camera.join();
        }
        unsafe { _ = CloseHandle(HANDLE(self.stop.load(Relaxed) as *mut _)) }
    }
}

/// Whether this failed attempt is worth a line: the first, then one a minute.
fn worth_saying(failed: u32) -> bool {
    failed == 1 || failed % 60 == 0
}

/// How long to wait before arming again after `failed` failures: a second, then longer,
/// up to five. Only a watcher that will not arm ever waits at all.
fn retry_wait(failed: u32) -> u32 {
    1000 * failed.clamp(1, 5)
}

/// Calls `on` with the apps holding the microphone and the camera whenever either list changes
/// (and once at the start). The microphone follows the live capture sessions, which is also
/// what the Windows microphone icon follows; the camera keeps the registry and its watcher.
pub fn watch(on: impl Fn(Vec<String>, Vec<String>) + Send + Sync + 'static) -> Option<Watch> {
    let stop = unsafe { CreateEventW(None, true, false, None) }.ok()?;
    let (wake, woken) = mpsc::channel::<Wake>();
    let report = Arc::new(Report::new(on, wake.clone()));

    // The grace's clock: an app held over has no event of its own when its time is up, so
    // one thread sleeps until it is and reads the list again.
    let grace = std::thread::spawn({
        let report = report.clone();
        move || loop {
            match report.grace_left() {
                // Nothing held over: sleep until something is, or until the end.
                None => match woken.recv() {
                    Ok(Wake::Changed) => continue,
                    _ => break,
                },
                Some(left) if left.is_zero() => report.microphone(),
                Some(left) => match woken.recv_timeout(left) {
                    Ok(Wake::Changed) => continue,
                    Ok(Wake::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => report.microphone(),
                },
            }
        }
    });

    // Subscribed before reading, so a session that starts in between is not lost.
    let sessions = svc_audio::on_sessions(svc_audio::Scope::All, {
        let report = report.clone();
        move || report.microphone()
    });
    // Once at the start, as the page is told nothing when it loads.
    report.both();

    let stop_raw = Arc::new(AtomicIsize::new(stop.0 as isize));
    let raw = stop.0 as isize;
    let camera = std::thread::spawn(move || {
        let stop = HANDLE(raw as *mut _);
        // A PC whose camera was never used has no such key: only the microphone wakes it then.
        let Some(cam) = open(HKEY_CURRENT_USER, CAMERA, KEY_READ | KEY_NOTIFY) else { return };
        let Ok(woken) = (unsafe { CreateEventW(None, false, false, None) }) else {
            unsafe { _ = RegCloseKey(cam) };
            return;
        };
        // A watcher that will not arm keeps trying, waiting longer each time: giving up would
        // leave the camera's row up with nothing ever able to take it down, so only the log
        // lines get quieter.
        let mut failed = 0u32;
        loop {
            // Armed before reading, so a change in between is not lost.
            let flags = REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME;
            let armed = unsafe { RegNotifyChangeKeyValue(cam, true, flags, Some(woken), true) };
            report.camera();
            if armed.is_err() {
                failed += 1;
                if worth_saying(failed) {
                    notch_core::log::line(format!("{}: the camera registry would not be watched: {}", crate::ID, armed.0));
                }
                if unsafe { WaitForSingleObject(stop, retry_wait(failed)) } == WAIT_OBJECT_0 {
                    break;
                }
                continue;
            }
            failed = 0;
            if unsafe { WaitForMultipleObjects(&[woken, stop], false, INFINITE) } != WAIT_OBJECT_0 {
                break;
            }
            // A burst of writes (open, stop, several apps) settles into one reading.
            if unsafe { WaitForSingleObject(stop, 150) } == WAIT_OBJECT_0 {
                break;
            }
        }
        unsafe {
            _ = RegCloseKey(cam);
            _ = CloseHandle(woken);
        }
    });
    Some(Watch { _sessions: sessions, stop: stop_raw, wake: Some(wake), grace: Some(grace), camera: Some(camera) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn capture(pid: u32, exe: Option<&str>) -> Capture {
        Capture { pid, exe: exe.map(str::to_owned) }
    }

    #[test]
    fn a_row_names_the_app_by_its_file_or_by_its_pid() {
        assert_eq!(label(&capture(4, Some(r"C:\Apps\Teams.exe"))), "Teams");
        assert_eq!(label(&capture(5, None)), "PID 5", "a process that may not be asked is not hidden");
    }

    #[test]
    fn a_name_is_the_last_segment_of_the_path() {
        assert_eq!(nice(r"C:\Program Files\Brave\brave.exe", false), "brave");
        assert_eq!(nice("C:#Apps#Teams.exe", false), "Teams");
        assert_eq!(nice("Microsoft.WindowsCamera_8wekyb3d8bbwe", true), "WindowsCamera");
    }

    #[test]
    fn lintel_is_not_a_stranger_to_itself() {
        let (me, mine) = (42, Some(r"C:\Program Files\Lintel\lintel.exe"));
        assert!(ours(&capture(42, Some(r"C:\Program Files\Lintel\lintel.exe")), me, mine), "this process");
        // A child running the same program: another pid, the same executable.
        assert!(ours(&capture(43, Some(r"c:\program files\lintel\LINTEL.exe")), me, mine));
        assert!(!ours(&capture(43, Some(r"C:\Apps\Teams.exe")), me, mine), "someone else");
        assert!(!ours(&capture(43, None), me, mine), "an app that cannot be asked is not Lintel");
    }

    #[test]
    fn an_app_that_stops_is_kept_for_a_moment_and_then_let_go() {
        let mut grace = Grace::default();
        let t0 = Instant::now();
        assert_eq!(grace.apply(vec!["teams".into()], t0), vec!["teams"]);
        assert_eq!(grace.left(t0), None, "nothing is held over yet");

        // A whole minute of quiet went by before anything looked again: the grace runs
        // from the moment the app was missed, not from when it was last seen.
        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(grace.apply(vec![], t1), vec!["teams"]);
        assert_eq!(grace.left(t1), Some(GRACE));

        // Half the grace later: still listed, and half the wait left.
        let t2 = t1 + GRACE / 2;
        assert_eq!(grace.apply(vec![], t2), vec!["teams"]);
        assert_eq!(grace.left(t2), Some(GRACE / 2));

        // Past it: gone, with nothing left to wake for.
        let t3 = t1 + GRACE;
        assert!(grace.apply(vec![], t3).is_empty());
        assert_eq!(grace.left(t3), None);
    }

    #[test]
    fn an_app_that_comes_back_within_the_grace_is_listed_once() {
        let mut grace = Grace::default();
        let t0 = Instant::now();
        assert_eq!(grace.apply(vec!["teams".into()], t0), vec!["teams"]);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(grace.apply(vec![], t1), vec!["teams"], "held over");
        assert_eq!(grace.apply(vec!["teams".into()], t1), vec!["teams"], "and only once");
        assert_eq!(grace.left(t1), None, "it is back, so nothing is waiting to go");
    }

    #[test]
    fn a_subscriber_may_read_the_state_back_without_deadlocking() {
        let hits = Arc::new(AtomicUsize::new(0));
        let slot: Arc<Mutex<Option<Arc<Report>>>> = Arc::new(Mutex::new(None));
        let (wake, _woken) = mpsc::channel::<Wake>();
        let report = Arc::new(Report::new(
            {
                let (hits, slot) = (hits.clone(), slot.clone());
                move |microphone, _camera| {
                    hits.fetch_add(1, Relaxed);
                    // A consumer that asks the hub to report again: with the lock held
                    // around this call it would wait on itself for good.
                    if microphone.len() == 1 && microphone[0] == "x" {
                        if let Some(report) = slot.lock().unwrap().as_ref() {
                            report.tell(Some(vec!["y".into()]), None);
                        }
                    }
                }
            },
            wake,
        ));
        *slot.lock().unwrap() = Some(report.clone());

        report.tell(Some(vec!["x".into()]), None);
        assert_eq!(hits.load(Relaxed), 2, "the nested reading was told once");

        // The same pair again is not told a second time.
        report.tell(Some(vec!["y".into()]), None);
        assert_eq!(hits.load(Relaxed), 2);
    }

    #[test]
    fn a_watcher_that_will_not_arm_waits_longer_but_never_gives_up() {
        assert_eq!(retry_wait(1), 1000);
        assert_eq!(retry_wait(3), 3000);
        assert_eq!(retry_wait(40), 5000, "capped, so a broken watcher is one wake a second at most");
        assert!(worth_saying(1), "the first failure is always worth a line");
        assert!(!worth_saying(2));
        assert!(worth_saying(60), "then one a minute");
    }
}
