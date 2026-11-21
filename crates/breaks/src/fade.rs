// The fade: for a break, every screen goes to black with the countdown on it.
//
// One window per monitor on a thread of its own, there only for the break. The windows are
// topmost, without a border, and never take the keyboard, so they cannot hold the person: what is
// typed still goes to the window underneath. A click lands on the overlay itself and skips the
// break, so a click on a black screen never presses what it cannot see. Esc ends it at once: a
// plain Esc hotkey (which Windows still delivers while an elevated window such as Task Manager has
// the foreground, where a low-level hook of this process is not called at all) and, for the usual
// case, a keyboard hook; it ends by itself when the countdown does; `close` ends it from outside;
// and the windows go with the process, however that ends. Plain Win32 and GDI, not a web view:
// nothing to load, so it is on screen the moment the break starts.
//
// debt: drawn in Segoe UI; the app's bundled web font is not a font GDI can load.

use std::cell::RefCell;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{w, BOOL};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    EndPaint, EnumDisplayMonitors, FillRect, GetStockObject, InvalidateRect, SelectObject, SetBkMode, SetTextColor,
    SetViewportOrgEx, ANTIALIASED_QUALITY, BLACK_BRUSH, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_CENTER, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HMONITOR, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    KillTimer, PostQuitMessage, PostThreadMessageW, RegisterClassW, SetLayeredWindowAttributes, SetTimer,
    SetWindowsHookExW, ShowWindow, SystemParametersInfoW, UnhookWindowsHookEx, UnregisterClassW, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, LWA_ALPHA, MSG, SPI_GETCLIENTAREAANIMATION, SW_SHOWNOACTIVATE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WH_KEYBOARD_LL, WM_APP, WM_ERASEBKGND, WM_HOTKEY, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_MBUTTONDOWN, WM_PAINT, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_TIMER, WM_XBUTTONDOWN, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

/// The class of the fade's windows (the probe looks for it to see that none is left).
pub const CLASS: windows::core::PCWSTR = w!("lintel-breaks-fade");

const IN_MS: u64 = 700;
const OUT_MS: u64 = 400;
/// Esc, a click, or the break ending some other way: gone in a blink.
const FAST_MS: u64 = 120;
const TIMER_FADE: usize = 1;
const TIMER_SECOND: usize = 2;
/// Thread message: fade out and go.
const CLOSE: u32 = WM_APP + 1;
/// The Esc hotkey, registered for as long as the fade is on screen.
const HOTKEY: i32 = 1;
/// Where the number stands on a screen, in percent of its height from the top.
const NUMBER: (i32, i32) = (40, 62);

// The tokens of src/ui/tokens.css, as GDI wants them (0x00BBGGRR).
const INK: COLORREF = COLORREF(0x00F8_F6F5);
const INK_2: COLORREF = COLORREF(0x00D4_CDC9);
const DIM: COLORREF = COLORREF(0x00A1_9893);

struct Stage {
    wins: Vec<HWND>,
    hooks: Vec<HHOOK>,
    title: Vec<u16>,
    hint: Vec<u16>,
    /// When the countdown reaches zero.
    end: Instant,
    /// The running fade: from this opacity, begun then, to that one, over so many milliseconds.
    from: (u8, Instant),
    to: u8,
    ms: u64,
    alpha: u8,
    /// Told once when the person ends the fade; `None` once it is on its way out.
    skip: Option<Box<dyn FnOnce() + Send>>,
    /// The Esc hotkey was registered: it ends the fade even where the keyboard hook is not called.
    hotkey: bool,
}

thread_local! {
    static STAGE: RefCell<Option<Stage>> = const { RefCell::new(None) };
}

impl Stage {
    fn left(&self) -> u64 {
        self.end.saturating_duration_since(Instant::now()).as_millis().div_ceil(1000) as u64
    }

    unsafe fn set_alpha(&mut self, alpha: u8) {
        self.alpha = alpha;
        for win in &self.wins {
            let _ = unsafe { SetLayeredWindowAttributes(*win, COLORREF(0), alpha, LWA_ALPHA) };
        }
    }

    /// Starts a fade to `to`. With animations turned off in Windows it is a cut.
    unsafe fn fade(&mut self, to: u8, ms: u64) {
        let mut animated = BOOL(1);
        let _ = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some(&mut animated as *mut BOOL as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        (self.from, self.to, self.ms) = ((self.alpha, Instant::now()), to, if animated.as_bool() { ms } else { 0 });
        // The one fast timer of the feature: a few hundred milliseconds of it, then it is killed.
        unsafe { SetTimer(self.wins.first().copied(), TIMER_FADE, 16, None) };
    }

    /// On its way out: the hooks go first, so not one more key or click passes through here.
    unsafe fn out(&mut self, ms: u64) {
        self.skip = None;
        for hook in self.hooks.drain(..) {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
        if std::mem::take(&mut self.hotkey) {
            if let Some(win) = self.wins.first().copied() {
                let _ = unsafe { UnregisterHotKey(Some(win), HOTKEY) };
            }
        }
        let _ = unsafe { KillTimer(self.wins.first().copied(), TIMER_SECOND) };
        if self.to != 0 || ms < self.ms {
            unsafe { self.fade(0, ms) };
        }
    }

    unsafe fn timer(&mut self, id: usize) {
        let first = self.wins.first().copied();
        if id == TIMER_SECOND {
            let left = self.end.saturating_duration_since(Instant::now()).as_millis() as u32;
            if left == 0 {
                unsafe { self.out(OUT_MS) };
                return;
            }
            // Only the band the number is in is drawn again.
            for win in &self.wins {
                let mut all = RECT::default();
                let _ = unsafe { GetClientRect(*win, &mut all) };
                let band = RECT { top: all.bottom * NUMBER.0 / 100, bottom: all.bottom * NUMBER.1 / 100, ..all };
                let _ = unsafe { InvalidateRect(Some(*win), Some(&band), false) };
            }
            // To the next whole second, so the number never skips one.
            unsafe { SetTimer(first, TIMER_SECOND, left % 1000 + 5, None) };
            return;
        }
        let t = if self.ms == 0 { 1.0 } else { (self.from.1.elapsed().as_millis() as f32 / self.ms as f32).min(1.0) };
        let eased = t * t * (3.0 - 2.0 * t);
        let alpha = self.from.0 as f32 + (self.to as f32 - self.from.0 as f32) * eased;
        unsafe { self.set_alpha(alpha.round() as u8) };
        if t < 1.0 {
            return;
        }
        let _ = unsafe { KillTimer(first, TIMER_FADE) };
        if self.to == 0 {
            for win in self.wins.drain(..) {
                let _ = unsafe { DestroyWindow(win) };
            }
            unsafe { PostQuitMessage(0) };
        }
    }
}

/// The person pressed Esc or clicked: true when that ended the fade (the event is then eaten, so
/// a click on a black screen presses nothing it cannot see).
fn skipped() -> bool {
    let told = STAGE.with(|stage| {
        let mut stage = stage.try_borrow_mut().ok()?;
        let stage = stage.as_mut()?;
        let tell = stage.skip.take()?;
        unsafe { stage.out(FAST_MS) };
        Some(tell)
    });
    // Off this thread: a hook must answer at once, and the feature may be busy.
    told.map(std::thread::spawn).is_some()
}

unsafe extern "system" fn key_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
        let key = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // Keys a program sends (a text expander, an automation) are not the person's.
        if key.vkCode == VK_ESCAPE.0 as u32 && key.flags.0 & LLKHF_INJECTED.0 == 0 && skipped() {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn each_monitor(_: HMONITOR, _: HDC, rect: *mut RECT, out: LPARAM) -> BOOL {
    unsafe { (*(out.0 as *mut Vec<RECT>)).push(*rect) };
    BOOL(1)
}

unsafe fn text(hdc: HDC, text: &[u16], px: i32, weight: i32, color: COLORREF, mut rect: RECT) {
    unsafe {
        let font = CreateFontW(
            -px.max(12),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            w!("Segoe UI"),
        );
        let old = SelectObject(hdc, font.into());
        SetTextColor(hdc, color);
        DrawTextW(hdc, &mut text.to_vec(), &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
    }
}

/// The words in the middle of the screen, sized from the monitor so every display reads the same.
/// Drawn off screen and copied in one go: the number changes without a flicker.
unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut all = RECT::default();
        let _ = GetClientRect(hwnd, &mut all);
        let dirty = ps.rcPaint;
        let (w, h) = (dirty.right - dirty.left, dirty.bottom - dirty.top);
        if w > 0 && h > 0 {
            let mem = CreateCompatibleDC(Some(hdc));
            let bitmap = CreateCompatibleBitmap(hdc, w, h);
            let old = SelectObject(mem, bitmap.into());
            let _ = SetViewportOrgEx(mem, -dirty.left, -dirty.top, None);
            FillRect(mem, &dirty, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            SetBkMode(mem, TRANSPARENT);
            STAGE.with(|stage| {
                let Ok(stage) = stage.try_borrow() else { return };
                let Some(stage) = stage.as_ref() else { return };
                let left = stage.left();
                let number: Vec<u16> =
                    if left >= 60 { format!("{}:{:02}", left / 60, left % 60) } else { left.to_string() }.encode_utf16().collect();
                let band = |top: i32, bottom: i32| RECT { top: all.bottom * top / 100, bottom: all.bottom * bottom / 100, ..all };
                text(mem, &stage.title, all.bottom / 30, 400, INK_2, band(33, 40));
                text(mem, &number, all.bottom / 6, 300, INK, band(NUMBER.0, NUMBER.1));
                text(mem, &stage.hint, all.bottom / 54, 400, DIM, band(86, 92));
            });
            let _ = BitBlt(hdc, dirty.left, dirty.top, w, h, Some(mem), dirty.left, dirty.top, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => unsafe { paint(hwnd) },
        WM_ERASEBKGND => return LRESULT(1),
        // Esc while an elevated window has the foreground (the hook is not called then), and a
        // click on any screen: both end the break.
        WM_HOTKEY => {
            if wparam.0 == HOTKEY as usize {
                skipped();
            }
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
            skipped();
        }
        WM_TIMER => STAGE.with(|stage| {
            if let Ok(mut stage) = stage.try_borrow_mut() {
                if let Some(stage) = stage.as_mut() {
                    unsafe { stage.timer(wparam.0) };
                }
            }
        }),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

fn run(secs: u64, title: String, hint: String, skip: Box<dyn FnOnce() + Send>, ready: mpsc::Sender<(u32, usize)>) {
    unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: CLASS,
            ..Default::default()
        });
        let mut rects: Vec<RECT> = Vec::new();
        let _ = EnumDisplayMonitors(None, None, Some(each_monitor), LPARAM(&mut rects as *mut Vec<RECT> as isize));
        let wins: Vec<HWND> = rects
            .iter()
            .filter_map(|r| {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    CLASS,
                    w!(""),
                    WS_POPUP,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    None,
                    None,
                    Some(module.into()),
                    None,
                )
                .ok()
            })
            .collect();
        let _ = ready.send((GetCurrentThreadId(), wins.len()));
        if !wins.is_empty() {
            let mut stage = Stage {
                wins,
                hooks: Vec::new(),
                title: title.encode_utf16().collect(),
                hint: hint.encode_utf16().collect(),
                end: Instant::now() + Duration::from_secs(secs),
                from: (0, Instant::now()),
                to: 255,
                ms: 0,
                alpha: 0,
                skip: Some(skip),
                hotkey: false,
            };
            // Invisible before it is shown, so nothing flashes.
            stage.set_alpha(0);
            for win in &stage.wins {
                let _ = ShowWindow(*win, SW_SHOWNOACTIVATE);
            }
            // A click is the window's own now, so only the keyboard is hooked. Esc also comes as
            // a plain hotkey: Windows calls a low-level hook of this (unelevated) process for
            // nothing while an elevated window has the foreground, but hotkeys still arrive.
            stage.hooks = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), Some(module.into()), 0)
                .into_iter()
                .collect();
            stage.hotkey = stage
                .wins
                .first()
                .is_some_and(|win| RegisterHotKey(Some(*win), HOTKEY, HOT_KEY_MODIFIERS(0), VK_ESCAPE.0 as u32).is_ok());
            if !stage.hotkey {
                notch_core::log::line("breaks fade: the Esc hotkey is taken; the keyboard hook is the only way out");
            }
            stage.fade(255, IN_MS);
            SetTimer(stage.wins.first().copied(), TIMER_SECOND, 1000, None);
            STAGE.set(Some(stage));

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.hwnd.0.is_null() && msg.message == CLOSE {
                    STAGE.with_borrow_mut(|stage| {
                        if let Some(stage) = stage.as_mut() {
                            stage.out(FAST_MS);
                        }
                    });
                    continue;
                }
                DispatchMessageW(&msg);
            }
            // Whatever ended the loop, nothing stays behind.
            if let Some(stage) = STAGE.take() {
                for hook in stage.hooks {
                    let _ = UnhookWindowsHookEx(hook);
                }
                if stage.hotkey {
                    if let Some(win) = stage.wins.first().copied() {
                        let _ = UnregisterHotKey(Some(win), HOTKEY);
                    }
                }
                for win in stage.wins {
                    let _ = DestroyWindow(win);
                }
            }
        }
        let _ = UnregisterClassW(CLASS, Some(module.into()));
    }
}

/// A fade on screen. Dropping the handle does not end it: it ends by its countdown, by the person
/// or by `close`.
pub struct Fade {
    thread: u32,
    handle: JoinHandle<()>,
    /// How many screens it covers.
    pub windows: usize,
}

impl Fade {
    /// Fades every screen to black for `secs` seconds, counting down under `title`, with `hint`
    /// at the bottom. `skip` runs (on a thread of its own) if the person ends it with Esc or a click.
    pub fn show(secs: u64, title: String, hint: String, skip: impl FnOnce() + Send + 'static) -> Fade {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || run(secs, title, hint, Box::new(skip), tx));
        let (thread, windows) = rx.recv().unwrap_or((0, 0));
        Fade { thread, handle, windows }
    }

    /// Its windows are gone and its hooks with them.
    pub fn gone(&self) -> bool {
        self.handle.is_finished()
    }

    /// Fades out and goes. From any thread; nothing happens if it has gone already.
    pub fn close(&self) {
        if self.thread != 0 && !self.gone() {
            let _ = unsafe { PostThreadMessageW(self.thread, CLOSE, WPARAM(0), LPARAM(0)) };
        }
    }

    /// `close`, and waits until every window is gone.
    pub fn close_wait(self) {
        self.close();
        let _ = self.handle.join();
    }
}
