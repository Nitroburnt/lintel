// Input — a shared service (docs/ARCHITECTURE.md §8). Not a feature: the
// features that need it list this crate in their own Cargo.toml. Refcounted:
// nothing here runs without a live subscriber, and the handle `keyboard` and
// `mouse` return unsubscribes on drop.
//
// One thread owns the two low-level hooks (`WH_KEYBOARD_LL`, `WH_MOUSE_LL`) of
// every input feature. Each hook exists only while it has a subscriber, and the
// thread only while there is a hook. An event walks the subscribers in their
// order until one swallows it; whatever nobody swallows goes on to the next
// hook untouched, so the island's own key hook (src-tauri keyhook.rs) and other
// programs' hooks keep working beside this one.
//
// A subscriber runs on the hook thread, between a key and the window it is
// for: it must come back within a millisecond or so, take no lock another
// thread holds for long, and never call COM, the clipboard or anything that
// waits. Windows silently removes a hook that is slow. Hand real work to a
// thread of your own. Never send mouse input from a subscriber yourself:
// Windows asks the mouse hook about every mouse event, and the hook's own
// thread would wait a second for itself. `inject_wheel` knows.
//
// Order of the stages, as built so far (lower first):
//   keyboard  0 recorder · 10 key debounce · 20 blocked shortcuts · 30 remaps · 40 snippets
//   mouse     10 click debounce · 15 snippets (a click moves the caret) · 30 buttons · 40 wheel

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use svc_sysevents::Session;
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, LASTINPUTINFO, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK,
    RIDEV_REMOVE, RID_INPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetMessageW, PeekMessageW, PostThreadMessageW, RegisterClassW, SetWindowsHookExW, UnhookWindowsHookEx,
    UnregisterClassW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_APP, WM_INPUT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

// ── What a subscriber is told ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not mine: the next subscriber, then the window it was for.
    Pass,
    /// Nobody else sees it. Swallow the release of a key whose press you swallowed.
    Swallow,
}

#[derive(Debug, Clone, Copy)]
pub struct Key {
    /// Virtual key, left and right told apart (0xA2 left Ctrl, 0xA3 right Ctrl).
    pub vk: u16,
    /// Where the key is on the keyboard, whatever the layout.
    pub scan: u16,
    pub extended: bool,
    pub down: bool,
    /// Sent by another program. The app's own keys never get here.
    pub injected: bool,
    /// Microseconds on the performance counter, read as the event arrived
    /// (the event's own time has the resolution of the system timer).
    pub t: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    /// The side buttons: Back and Forward.
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Move,
    Down(Button),
    Up(Button),
    /// `delta` is 120 a notch, positive away from the user.
    Wheel,
    /// `delta` is 120 a notch, positive to the right.
    HWheel,
}

#[derive(Debug, Clone, Copy)]
pub struct Mouse {
    pub kind: MouseKind,
    /// Screen pixels.
    pub x: i32,
    pub y: i32,
    pub delta: i32,
    pub injected: bool,
    pub t: u64,
}

/// What Settings shows about the hooks.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// The hook is installed right now.
    pub keyboard: bool,
    pub mouse: bool,
    pub keyboard_subscribers: usize,
    pub mouse_subscribers: usize,
    /// Who holds `suspend_all`.
    pub suspended_by: Vec<&'static str>,
    /// The window in front runs as administrator: subscribers that inject are skipped.
    pub elevated: bool,
    /// Times a hook was installed again (resume, unlock, a display change, or it had gone quiet).
    pub refreshed: u32,
}

// ── The app's own events ──────────────────────────────────────────────────────

/// `dwExtraInfo` of everything the app injects: this magic above, the origin in
/// the low byte (0x68 is svc-inject's typing, 1 keys and 2 the wheel from here).
/// The hooks pass these on before any subscriber sees them.
pub const MAGIC: usize = 0x6E74_6300;

/// An event with this `dwExtraInfo` was sent by the app itself.
pub fn is_own(extra: usize) -> bool {
    extra & !0xFF == MAGIC
}

const _: () = assert!(svc_inject::INJECTED & !0xFF == MAGIC);

// ── Subscribing ───────────────────────────────────────────────────────────────

type KeyFn = Arc<dyn Fn(&Key) -> Verdict + Send + Sync>;
type MouseFn = Arc<dyn Fn(&Mouse) -> Verdict + Send + Sync>;

#[derive(Clone)]
struct Entry<F> {
    id: u64,
    order: i32,
    injects: bool,
    f: F,
}

/// Swapped whole on every change: the hooks take a copy of the pointer and let go.
type List<F> = RwLock<Option<Arc<Vec<Entry<F>>>>>;

static KEYS: List<KeyFn> = RwLock::new(None);
static MICE: List<MouseFn> = RwLock::new(None);

struct Hub {
    next: u64,
    thread: Option<(u32, JoinHandle<()>)>,
    events: Vec<svc_sysevents::Sub>,
}

static HUB: Mutex<Hub> = Mutex::new(Hub { next: 0, thread: None, events: Vec::new() });
/// The hook thread's id while it runs, for the messages posted to it.
static THREAD: AtomicU32 = AtomicU32::new(0);
static ON: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];
/// Tick of the last event each hook was called for.
static SEEN: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static ELEVATED: AtomicBool = AtomicBool::new(false);
static SUSPENDED: AtomicU32 = AtomicU32::new(0);
static SUSPENDERS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static REFRESHED: AtomicU32 = AtomicU32::new(0);

const SYNC: u32 = WM_APP + 1;
const REFRESH: u32 = WM_APP + 2;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn current<F>(list: &List<F>) -> Option<Arc<Vec<Entry<F>>>> {
    list.read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Key,
    Mouse,
}

/// A live subscription. Dropping it unsubscribes; dropping the last one of a
/// kind removes that hook, and the last of all ends the thread.
#[must_use = "dropping the handle unsubscribes"]
pub struct Sub {
    id: u64,
    kind: Kind,
}

/// Every key press and release, in `order` among the keyboard subscribers.
/// `injects`: the subscriber sends keys of its own; it is skipped while the
/// window in front runs as administrator (it could not reach it anyway).
/// While skipped or suspended it still hears releases, so nothing it holds
/// stays held.
pub fn keyboard(order: i32, injects: bool, f: impl Fn(&Key) -> Verdict + Send + Sync + 'static) -> Sub {
    subscribe(&KEYS, Kind::Key, order, injects, Arc::new(f))
}

/// Every mouse move, button and wheel event, in `order` among the mouse subscribers.
pub fn mouse(order: i32, injects: bool, f: impl Fn(&Mouse) -> Verdict + Send + Sync + 'static) -> Sub {
    subscribe(&MICE, Kind::Mouse, order, injects, Arc::new(f))
}

fn subscribe<F: Clone>(list: &List<F>, kind: Kind, order: i32, injects: bool, f: F) -> Sub {
    let id = {
        let mut hub = lock(&HUB);
        hub.next += 1;
        let id = hub.next;
        {
            let mut slot = list.write().unwrap_or_else(|e| e.into_inner());
            let mut next: Vec<Entry<F>> = slot.as_deref().cloned().unwrap_or_default();
            next.push(Entry { id, order, injects, f });
            next.sort_by_key(|e| e.order);
            *slot = Some(Arc::new(next));
        }
        if hub.thread.is_none() {
            let (tx, rx) = mpsc::channel();
            let handle = std::thread::spawn(move || run(tx));
            let thread = rx.recv().unwrap_or(0);
            THREAD.store(thread, Relaxed);
            hub.thread = Some((thread, handle));
            ELEVATED.store(foreground_elevated(), Relaxed);
            hub.events = vec![
                svc_sysevents::on_foreground(|front| {
                    ELEVATED.store(svc_inject::is_elevated(front.hwnd), Relaxed);
                    heartbeat();
                }),
                svc_sysevents::on_session(|event| {
                    if matches!(event, Session::Resume | Session::Unlock) {
                        post(REFRESH);
                    }
                }),
                svc_sysevents::on_display_change(|| post(REFRESH)),
            ];
        } else {
            post(SYNC);
        }
        id
    };
    settle();
    Sub { id, kind }
}

impl Drop for Sub {
    fn drop(&mut self) {
        fn remove<F: Clone>(list: &List<F>, id: u64) {
            let mut slot = list.write().unwrap_or_else(|e| e.into_inner());
            let next: Vec<Entry<F>> = slot.as_deref().map(|l| l.iter().filter(|e| e.id != id).cloned().collect()).unwrap_or_default();
            *slot = if next.is_empty() { None } else { Some(Arc::new(next)) };
        }
        let (ended, events) = {
            let mut hub = lock(&HUB);
            match self.kind {
                Kind::Key => remove(&KEYS, self.id),
                Kind::Mouse => remove(&MICE, self.id),
            }
            if current(&KEYS).is_none() && current(&MICE).is_none() {
                THREAD.store(0, Relaxed);
                (hub.thread.take(), std::mem::take(&mut hub.events))
            } else {
                post(SYNC);
                (None, Vec::new())
            }
        };
        // Outside the lock: the hook thread may itself be waiting for it.
        match ended {
            Some((thread, handle)) => {
                unsafe {
                    let _ = PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                // Dropped from inside a subscriber: the thread is this one and ends on its own.
                if handle.thread().id() != std::thread::current().id() {
                    let _ = handle.join();
                }
            }
            None => settle(),
        }
        drop(events);
    }
}

/// Waits (half a second at most) until the hooks are what the lists ask for,
/// so a caller can rely on its hook the moment `keyboard` or `mouse` returns.
fn settle() {
    let thread = THREAD.load(Relaxed);
    if thread == 0 || thread == unsafe { GetCurrentThreadId() } {
        return;
    }
    let until = Instant::now() + Duration::from_millis(500);
    while (ON[0].load(Relaxed) != current(&KEYS).is_some() || ON[1].load(Relaxed) != current(&MICE).is_some())
        && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn post(message: u32) {
    let thread = THREAD.load(Relaxed);
    if thread != 0 {
        unsafe {
            let _ = PostThreadMessageW(thread, message, WPARAM(0), LPARAM(0));
        }
    }
}

// ── Suspending ────────────────────────────────────────────────────────────────

/// While it lives, no subscriber hears a press, a move or the wheel: every
/// event goes straight on. Releases are still told.
#[must_use = "dropping the handle resumes"]
pub struct Suspended(&'static str);

/// For the shortcut recorder and cleaning mode. `token` says who, for Settings.
pub fn suspend_all(token: &'static str) -> Suspended {
    lock(&SUSPENDERS).push(token);
    SUSPENDED.fetch_add(1, Relaxed);
    Suspended(token)
}

impl Drop for Suspended {
    fn drop(&mut self) {
        SUSPENDED.fetch_sub(1, Relaxed);
        let mut who = lock(&SUSPENDERS);
        if let Some(at) = who.iter().position(|t| *t == self.0) {
            who.remove(at);
        }
    }
}

/// Installs the hooks that exist again, now. What resume, unlock and a display
/// change do by themselves; for a "repair" button, and for the probe.
pub fn refresh() {
    post(REFRESH);
}

/// The window in front runs as administrator while the app does not: keys
/// sent to it are dropped by Windows, and its own input may not reach the hooks.
pub fn foreground_elevated() -> bool {
    svc_inject::is_elevated(unsafe { GetForegroundWindow() }.0 as isize)
}

pub fn status() -> Status {
    Status {
        keyboard: ON[0].load(Relaxed),
        mouse: ON[1].load(Relaxed),
        keyboard_subscribers: current(&KEYS).map_or(0, |l| l.len()),
        mouse_subscribers: current(&MICE).map_or(0, |l| l.len()),
        suspended_by: lock(&SUSPENDERS).clone(),
        elevated: if THREAD.load(Relaxed) != 0 { ELEVATED.load(Relaxed) } else { foreground_elevated() },
        refreshed: REFRESHED.load(Relaxed),
    }
}

// ── The hook thread ───────────────────────────────────────────────────────────

/// The clock of `Key::t` and `Mouse::t`, in microseconds.
pub fn now() -> u64 {
    // `Instant` is the performance counter on Windows.
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

fn run(ready: mpsc::Sender<u32>) {
    unsafe {
        // Every key and every click of the session waits for this thread.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        let mut msg = MSG::default();
        // Makes the thread's message queue, so that what is posted next is not lost.
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let _ = ready.send(GetCurrentThreadId());
        // The raw wheel sink: a hidden window of this thread, so a wheel turn still arrives while
        // an elevated window has the foreground and Windows does not call the mouse hook at all.
        let raw = raw_sink();
        // Wheel turns a subscriber asks for are sent by a thread beside this one (see `inject_wheel`).
        let (wheels, turns) = mpsc::channel::<(i32, bool)>();
        std::thread::spawn(move || {
            for (delta, horizontal) in turns {
                send_wheel(delta, horizontal);
            }
        });
        WHEELS.with(|slot| *slot.borrow_mut() = Some(wheels));
        let mut hooks: [Option<HHOOK>; 2] = [None, None];
        install(&mut hooks, false);
        // The hooks are called from inside GetMessageW. False on WM_QUIT, -1 on an error: either way they go.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            match msg.message {
                SYNC => install(&mut hooks, false),
                REFRESH => install(&mut hooks, true),
                // The sink's own messages (WM_INPUT).
                _ if !msg.hwnd.is_invalid() => {
                    let _ = DispatchMessageW(&msg);
                }
                _ => {}
            }
        }
        for hook in hooks.into_iter().flatten() {
            let _ = UnhookWindowsHookEx(hook);
        }
        raw_sink_end(raw);
        ON[0].store(false, Relaxed);
        ON[1].store(false, Relaxed);
        // The sending thread ends with its channel.
        WHEELS.with(|slot| slot.borrow_mut().take());
    }
}

thread_local! {
    /// On the hook thread only: where its wheel turns go.
    static WHEELS: std::cell::RefCell<Option<mpsc::Sender<(i32, bool)>>> = const { std::cell::RefCell::new(None) };
}

// ── The raw wheel sink ────────────────────────────────────────────────────────
//
// A wheel turn is the one mouse event a feature waits for over a window that is not its
// own (the tray icon's). The hook reads it -- except while an elevated window has the
// foreground: then Windows calls no low-level hook of this unelevated process, and the
// wheel goes to that window and nowhere else. Raw input is not a hook: registered with
// RIDEV_INPUTSINK, the hidden window below is told of the wheel even as a background
// window, whatever has the foreground. Only the wheel is taken from it (the hook is
// the only place that can swallow a click), and only while the hooks are blind, so no
// event is ever told twice.

/// The sink window's class (hidden, no taskbar entry, never activated).
const RAW_CLASS: windows::core::PCWSTR = w!("lintel-input-raw");
/// `RAWMOUSE.usButtonFlags`: the vertical and the horizontal wheel (Winuser.h; the
/// windows crate exposes the constants of the message, not those of the packet).
const RAW_WHEEL: u16 = 0x0400;
const RAW_HWHEEL: u16 = 0x0800;
/// `RAWINPUTHEADER.dwType` of a mouse packet: `RIM_TYPEMOUSE`.
const RAW_MOUSE: u32 = 0;

/// The hidden window and its registration; `None` when neither could be made.
struct RawSink {
    hwnd: HWND,
    module: HINSTANCE,
}

unsafe fn raw_sink() -> Option<RawSink> {
    let module = unsafe { GetModuleHandleW(None) }.map(|m| HINSTANCE(m.0)).unwrap_or_default();
    let class = WNDCLASSW {
        lpfnWndProc: Some(raw_proc),
        hInstance: module.into(),
        lpszClassName: RAW_CLASS,
        ..Default::default()
    };
    let _ = unsafe { RegisterClassW(&class) };
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            RAW_CLASS,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        )
    };
    let Ok(hwnd) = hwnd else { return None };
    let devices = [RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd }];
    match unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) } {
        Ok(()) => Some(RawSink { hwnd, module }),
        Err(err) => {
            // Another window of this process already holds the mouse; the tray wheel then waits
            // for a foreground window that is not elevated, as it did before.
            notch_core::log::line(format!("input: the raw wheel sink could not be registered: {err}"));
            let _ = unsafe { DestroyWindow(hwnd) };
            None
        }
    }
}

unsafe fn raw_sink_end(sink: Option<RawSink>) {
    let Some(sink) = sink else { return };
    let devices = [RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_REMOVE, hwndTarget: HWND::default() }];
    let _ = unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) };
    let _ = unsafe { DestroyWindow(sink.hwnd) };
    let _ = unsafe { UnregisterClassW(RAW_CLASS, Some(sink.module.into())) };
}

unsafe extern "system" fn raw_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Only while the hooks are blind: otherwise the hook has told the subscribers already.
    if msg == WM_INPUT && blocked() {
        let mut bytes = [0u8; 128];
        let mut size = bytes.len() as u32;
        let got = unsafe {
            GetRawInputData(
                HRAWINPUT(lparam.0 as *mut _),
                RID_INPUT,
                Some(bytes.as_mut_ptr() as *mut _),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if got != 0 && got != u32::MAX {
            let raw = unsafe { &*(bytes.as_ptr() as *const RAWINPUT) };
            if raw.header.dwType == RAW_MOUSE {
                let buttons = unsafe { raw.data.mouse.Anonymous.Anonymous };
                let (kind, delta) = if buttons.usButtonFlags & RAW_WHEEL != 0 {
                    (MouseKind::Wheel, buttons.usButtonData as i16 as i32)
                } else if buttons.usButtonFlags & RAW_HWHEEL != 0 {
                    (MouseKind::HWheel, buttons.usButtonData as i16 as i32)
                } else {
                    return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
                };
                let mut at = POINT::default();
                let _ = unsafe { GetCursorPos(&mut at) };
                tell(&Mouse { kind, x: at.x, y: at.y, delta, injected: false, t: now() });
            }
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Makes the hooks what the lists ask for. `fresh` installs the wanted ones
/// again: Windows takes a hook out without a word when it was slow once, and
/// after a sleep or a lock it may not be called any more.
unsafe fn install(hooks: &mut [Option<HHOOK>; 2], fresh: bool) {
    let module = unsafe { GetModuleHandleW(None) }.map(|m| HINSTANCE(m.0)).unwrap_or_default();
    let wanted = [current(&KEYS).is_some(), current(&MICE).is_some()];
    for i in 0..2 {
        let old = hooks[i];
        if wanted[i] && (old.is_none() || fresh) {
            let new = unsafe {
                match i {
                    0 => SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), Some(module), 0),
                    _ => SetWindowsHookExW(WH_MOUSE_LL, Some(on_mouse), Some(module), 0),
                }
            };
            match new {
                Ok(hook) => {
                    // The new one first: no event passes between the two. Nothing is
                    // told twice either, the hooks are only called from GetMessageW.
                    hooks[i] = Some(hook);
                    SEEN[i].store(unsafe { GetTickCount() }, Relaxed);
                    if let Some(old) = old {
                        let _ = unsafe { UnhookWindowsHookEx(old) };
                        REFRESHED.fetch_add(1, Relaxed);
                    }
                }
                Err(err) => notch_core::log::line(format!("input: hook {i} could not be installed: {err}")),
            }
        } else if !wanted[i] {
            if let Some(old) = hooks[i].take() {
                let _ = unsafe { UnhookWindowsHookEx(old) };
            }
        }
        ON[i].store(hooks[i].is_some(), Relaxed);
    }
}

/// The heartbeat, without a timer: when another window comes to the front,
/// a hook that has not been called since input that is more than two seconds
/// newer is taken for removed and installed again.
// debt: a keyboard hook that Windows removed is noticed at the next window switch, not at once.
fn heartbeat() {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return;
    }
    let quiet = |i: usize| ON[i].load(Relaxed) && info.dwTime.wrapping_sub(SEEN[i].load(Relaxed)) as i32 > 2000;
    if quiet(0) || quiet(1) {
        post(REFRESH);
    }
}

const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_INJECTED: u32 = 0x10;
const LLKHF_UP: u32 = 0x80;
const LLMHF_INJECTED: u32 = 0x01;

unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let raw = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        SEEN[0].store(raw.time, Relaxed);
        if !is_own(raw.dwExtraInfo) {
            if let Some(list) = current(&KEYS) {
                let flags = raw.flags.0;
                let key = Key {
                    vk: raw.vkCode as u16,
                    scan: raw.scanCode as u16,
                    extended: flags & LLKHF_EXTENDED != 0,
                    down: flags & LLKHF_UP == 0,
                    injected: flags & LLKHF_INJECTED != 0,
                    t: now(),
                };
                let (suspended, elevated) = (SUSPENDED.load(Relaxed) != 0, ELEVATED.load(Relaxed));
                for entry in list.iter() {
                    if key.down && (suspended || (entry.injects && elevated)) {
                        continue;
                    }
                    if (entry.f)(&key) == Verdict::Swallow {
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn on_mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let raw = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        SEEN[1].store(raw.time, Relaxed);
        let high = (raw.mouseData >> 16) as u16;
        let side = if high == 1 { Button::X1 } else { Button::X2 };
        let kind = match wparam.0 as u32 {
            WM_MOUSEMOVE => Some(MouseKind::Move),
            WM_LBUTTONDOWN => Some(MouseKind::Down(Button::Left)),
            WM_LBUTTONUP => Some(MouseKind::Up(Button::Left)),
            WM_RBUTTONDOWN => Some(MouseKind::Down(Button::Right)),
            WM_RBUTTONUP => Some(MouseKind::Up(Button::Right)),
            WM_MBUTTONDOWN => Some(MouseKind::Down(Button::Middle)),
            WM_MBUTTONUP => Some(MouseKind::Up(Button::Middle)),
            WM_XBUTTONDOWN => Some(MouseKind::Down(side)),
            WM_XBUTTONUP => Some(MouseKind::Up(side)),
            WM_MOUSEWHEEL => Some(MouseKind::Wheel),
            WM_MOUSEHWHEEL => Some(MouseKind::HWheel),
            _ => None,
        };
        if let (Some(kind), false) = (kind, is_own(raw.dwExtraInfo)) {
            let wheel = matches!(kind, MouseKind::Wheel | MouseKind::HWheel);
            let event = Mouse {
                kind,
                x: raw.pt.x,
                y: raw.pt.y,
                delta: if wheel { high as i16 as i32 } else { 0 },
                injected: raw.flags & LLMHF_INJECTED != 0,
                t: now(),
            };
            if tell(&event) {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// One mouse event to every subscriber, in their order: true when one swallows it.
/// A press, a move or the wheel waits while the hooks are suspended, and while the
/// window in front runs as administrator a subscriber that injects is skipped (it
/// could not reach that window anyway). A release always goes through.
fn tell(event: &Mouse) -> bool {
    let Some(list) = current(&MICE) else { return false };
    let release = matches!(event.kind, MouseKind::Up(_));
    let (suspended, elevated) = (SUSPENDED.load(Relaxed) != 0, ELEVATED.load(Relaxed));
    for entry in list.iter() {
        if !release && (suspended || (entry.injects && elevated)) {
            continue;
        }
        if (entry.f)(event) == Verdict::Swallow {
            return true;
        }
    }
    false
}

// ── Injecting ─────────────────────────────────────────────────────────────────
//
// Marked as the app's own, so no subscriber hears them again. `inject_keys`
// and `inject_wheel` may be called from a subscriber: they come back at once
// and what they send arrives behind the event being handled. Measured with
// the probe: keys sent from the hook thread return in under a millisecond,
// a wheel turn sent from it would wait a second for its own hook.

/// Nothing new is pressed into a window that runs as administrator.
fn blocked() -> bool {
    if THREAD.load(Relaxed) != 0 {
        ELEVATED.load(Relaxed)
    } else {
        foreground_elevated()
    }
}

fn key_input(vk: u16, down: bool) -> INPUT {
    let mut flags = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    // The navigation block, the Win keys, the menu key, right Ctrl and right Alt are "extended" keys.
    if matches!(vk, 0x21..=0x28 | 0x2C..=0x2E | 0x5B..=0x5D | 0xA3 | 0xA5) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: MAGIC | 1 } },
    }
}

fn send(inputs: &[INPUT]) -> bool {
    inputs.is_empty() || unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) } as usize == inputs.len()
}

/// Presses (`true`) and releases (`false`) virtual keys, in the order given,
/// sixteen at most. Refused when it would press a key into a window that runs
/// as administrator; releases always go out.
pub fn inject_keys(keys: &[(u16, bool)]) -> bool {
    if keys.len() > 16 || (keys.iter().any(|k| k.1) && blocked()) {
        return false;
    }
    let mut inputs = [INPUT::default(); 16];
    for (slot, (vk, down)) in inputs.iter_mut().zip(keys) {
        *slot = key_input(*vk, *down);
    }
    send(&inputs[..keys.len()])
}

/// Backspace `deletes` times, then `text` typed character by character (any
/// script, no layout involved; a line break is Enter, a tab is Tab). Not from
/// a subscriber: a long text is sent in pieces.
pub fn inject_text(deletes: usize, text: &str) -> bool {
    if blocked() {
        return false;
    }
    let unit = |code: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: code,
                dwFlags: if up { KEYEVENTF_UNICODE | KEYEVENTF_KEYUP } else { KEYEVENTF_UNICODE },
                time: 0,
                dwExtraInfo: MAGIC | 1,
            },
        },
    };
    let tap = |vk: u16| [key_input(vk, true), key_input(vk, false)];
    let mut inputs: Vec<INPUT> = (0..deletes).flat_map(|_| tap(0x08)).collect();
    for code in text.replace("\r\n", "\n").encode_utf16() {
        match code {
            0x0A | 0x0D => inputs.extend(tap(0x0D)),
            0x09 => inputs.extend(tap(0x09)),
            _ => inputs.extend([unit(code, false), unit(code, true)]),
        }
    }
    // In pieces: a long text in one call can overrun the receiver's queue.
    let long = inputs.len() > 64;
    inputs.chunks(64).all(|piece| {
        let sent = send(piece);
        if long {
            std::thread::sleep(Duration::from_millis(2));
        }
        sent
    })
}

/// One turn of the wheel at the pointer: 120 a notch, positive away from the
/// user, or to the right with `horizontal`. Asked for by a subscriber, it is
/// sent by another thread: Windows asks the mouse hook about every mouse event,
/// this one included, and the hook's own thread would wait a second for itself.
pub fn inject_wheel(delta: i32, horizontal: bool) -> bool {
    if blocked() {
        return false;
    }
    WHEELS.with(|slot| match slot.borrow().as_ref() {
        Some(wheels) => wheels.send((delta, horizontal)).is_ok(),
        None => send_wheel(delta, horizontal),
    })
}

fn send_wheel(delta: i32, horizontal: bool) -> bool {
    send(&[INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: delta as u32,
                dwFlags: if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL },
                time: 0,
                dwExtraInfo: MAGIC | 2,
            },
        },
    }])
}
