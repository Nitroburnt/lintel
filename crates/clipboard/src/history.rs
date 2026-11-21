// The clipboard history: one hidden window on its own thread hears every
// change of the clipboard (`AddClipboardFormatListener`), decides whether the
// copy may be kept, cleans a link when asked to, and keeps what is left in
// history.json under the feature's data folder (pictures as PNG files beside
// it). The same thread empties the clipboard after the delay the user set.
//
// What is never kept: a copy made in or in front of a program on the user's
// own left-out list, anything while recording is paused, and whatever the app
// put on the clipboard itself. Everything else is recorded, keys and phone
// numbers included: neither the source's own marks nor the look of a text is
// looked at.
//
// Free of the app: the probe (examples/probe.rs) runs it with a folder and a
// print function of its own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notch_core::{log, storage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use svc_inject::clip;
use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{AddClipboardFormatListener, GetClipboardOwner, RemoveClipboardFormatListener};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW, SetTimer, UnregisterClassW, MSG,
    WM_APP, WM_CLIPBOARDUPDATE, WM_CLOSE, WM_DESTROY, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::urls;

/// Events for the page: `changed`, `captured`, `skipped`, `cleaned`, `cleared`. None carries what was copied.
pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

// ── Limits ────────────────────────────────────────────────────────────────────

/// One text: a million characters (UTF-16, as the clipboard holds it).
const TEXT_CAP: usize = 2_000_000;
/// One picture as PNG; as a raw bitmap before it is packed.
const PNG_CAP: usize = 16 << 20;
const BITMAP_CAP: usize = 64 << 20;
/// All texts that are not pinned, and all pictures that are not pinned.
const TEXTS_CAP: usize = 20 << 20;
const PICTURES_CAP: u64 = 128 << 20;
const FILES_CAP: usize = 100;

// ── Settings ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Config {
    pub recording: bool,
    /// Pictures and files too, not only text.
    pub media: bool,
    /// Program names (`chrome.exe`, lower case) whose copies are left out: the user's own list.
    pub ignored: Vec<String>,
    /// How many entries that are not pinned; `usize::MAX` (the user's "Unlimited") keeps them all.
    pub limit: usize,
    /// Entries that are not pinned go after this many days; 0 keeps them.
    pub keep_days: u64,
    /// Take the tracking parameters off a copied link, on the clipboard itself.
    pub clean_urls: bool,
    pub clean_extra: Vec<String>,
    /// Empty the clipboard this long after the last copy.
    pub clear_after: Option<Duration>,
    pub clear_on_lock: bool,
    pub clear_on_sleep: bool,
    pub clear_on_display_off: bool,
    /// Also empty Windows' own history (Win+V) when the clipboard is cleared.
    pub clear_windows_history: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            recording: true,
            media: true,
            ignored: Vec::new(),
            limit: 100,
            keep_days: 90,
            clean_urls: false,
            clean_extra: Vec::new(),
            clear_after: None,
            clear_on_lock: false,
            clear_on_sleep: false,
            clear_on_display_off: false,
            clear_windows_history: false,
        }
    }
}

fn names(list: &Value) -> Vec<String> {
    let text = list.as_str().unwrap_or_default().to_lowercase();
    text.split([',', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

impl Config {
    /// From the feature's settings (the manifest's defaults with the user's on top).
    pub fn from_settings(s: &Value) -> Self {
        let on = |key: &str, default: bool| s[key].as_bool().unwrap_or(default);
        let seconds = s["clearAfter"].as_u64().unwrap_or(0);
        Self {
            recording: on("recording", true),
            media: on("media", true),
            ignored: names(&s["ignoredApps"]),
            // 0 is the "Unlimited" the settings offer: no count cap at all.
            limit: match s["limit"].as_u64().unwrap_or(100) {
                0 => usize::MAX,
                n => n.clamp(10, 1000) as usize,
            },
            keep_days: s["keepDays"].as_u64().unwrap_or(90).min(3650),
            clean_urls: on("cleanUrls", false),
            clean_extra: names(&s["cleanExtra"]),
            clear_after: (seconds > 0).then(|| Duration::from_secs(seconds.clamp(5, 3600))),
            clear_on_lock: on("clearOnLock", false),
            clear_on_sleep: on("clearOnSleep", false),
            clear_on_display_off: on("clearOnDisplayOff", false),
            clear_windows_history: on("clearWindowsHistory", false),
        }
    }
}

// ── Entries ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Link,
    Image,
    Files,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Entry {
    pub id: String,
    pub kind: Option<Kind>,
    /// The text or the address, exactly as copied; for files, their paths, one a line.
    pub text: String,
    /// A picture: its size, the bytes of its PNG (`images/<id>.png`) and their SHA-256.
    pub width: u32,
    pub height: u32,
    pub size: u64,
    pub hash: String,
    /// The program it was copied in (`chrome.exe`) and where that program is.
    pub app: String,
    pub exe: String,
    /// When, in milliseconds since 1970.
    pub at: u64,
    pub pinned: bool,
}

impl Entry {
    pub fn kind(&self) -> Kind {
        self.kind.unwrap_or(Kind::Text)
    }

    /// The same content: a copy of it moves the entry to the front instead of adding one.
    fn same(&self, other: &Entry) -> bool {
        self.kind() == other.kind()
            && if self.kind() == Kind::Image { self.hash == other.hash } else { self.text == other.text }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn new_id() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("{:x}-{:x}", now_ms(), NEXT.fetch_add(1, Ordering::Relaxed))
}

// ── Pictures ──────────────────────────────────────────────────────────────────

/// Width and height of a PNG, from its first chunk.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    let be = |at: usize| Some(u32::from_be_bytes(png.get(at..at + 4)?.try_into().ok()?));
    (png.starts_with(b"\x89PNG\r\n\x1a\n")).then_some(())?;
    Some((be(16)?, be(20)?))
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header().ok()?.write_image_data(rgba).ok()?;
    Some(out)
}

/// A device-independent bitmap (`CF_DIB`: a header, then rows of pixels) as PNG.
// ponytail: 24 and 32 bits per pixel, uncompressed, which is what screenshots and browsers give;
// a palette or RLE bitmap is left out. WIC would read them all if one ever matters.
fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |at: usize| Some(u32::from_le_bytes(dib.get(at..at + 4)?.try_into().ok()?));
    let header = u32_at(0)? as usize;
    let width = u32_at(4)? as i32;
    let height = u32_at(8)? as i32;
    let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?) as usize;
    let compression = u32_at(16)?;
    if header < 40 || width <= 0 || height == 0 || !matches!(bits, 24 | 32) || !matches!(compression, 0 | 3) {
        return None;
    }
    // BI_BITFIELDS after a plain header: three colour masks before the pixels.
    let start = header + if compression == 3 && header == 40 { 12 } else { 0 };
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let stride = (w * bits).div_ceil(32) * 4;
    let pixels = dib.get(start..start + stride.checked_mul(h)?)?;
    let step = bits / 8;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        // Rows run bottom to top unless the height is negative.
        let line = if height > 0 { h - 1 - row } else { row };
        for px in pixels[line * stride..][..w * step].chunks_exact(step) {
            rgba.extend([px[2], px[1], px[0], if step == 4 { px[3] } else { 255 }]);
        }
    }
    // Most programs leave the fourth byte at zero and mean "opaque".
    if step == 4 && rgba.chunks_exact(4).all(|px| px[3] == 0) {
        rgba.chunks_exact_mut(4).for_each(|px| px[3] = 255);
    }
    encode_png(w as u32, h as u32, &rgba)
}

/// A PNG as the bitmap most Windows programs paste (`CF_DIB`, 32 bits, bottom row first).
// ponytail: transparency goes out as the fourth byte, which programs that ignore it draw as black;
// CF_DIBV5 with an alpha mask if that shows.
fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(png);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let (w, h) = (frame.width as usize, frame.height as usize);
    let step = frame.color_type.samples();
    let mut dib = Vec::with_capacity(40 + w * h * 4);
    dib.extend(40u32.to_le_bytes());
    dib.extend((w as i32).to_le_bytes());
    dib.extend((h as i32).to_le_bytes());
    dib.extend(1u16.to_le_bytes());
    dib.extend(32u16.to_le_bytes());
    dib.extend([0u8; 24]);
    for row in (0..h).rev() {
        for px in buffer[row * frame.line_size..][..w * step].chunks_exact(step) {
            dib.extend(match step {
                1 => [px[0], px[0], px[0], 255],
                2 => [px[0], px[0], px[0], px[1]],
                3 => [px[2], px[1], px[0], 255],
                _ => [px[2], px[1], px[0], px[3]],
            });
        }
    }
    Some(dib)
}

// ── Who copied ────────────────────────────────────────────────────────────────

/// The program a window belongs to, by its full path.
fn exe_of(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        let read = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        read.ok().map(|_| String::from_utf16_lossy(&path[..len as usize]))
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().unwrap_or_default().to_string_lossy().to_lowercase()
}

/// The program that owns the clipboard and the one in front (a password prompt
/// that closes right after the copy is no owner any more, but was in front).
fn source() -> (Option<String>, Option<String>) {
    unsafe { (GetClipboardOwner().ok().and_then(exe_of), exe_of(GetForegroundWindow())) }
}

// ── The history ───────────────────────────────────────────────────────────────

struct Inner {
    dir: PathBuf,
    emit: Emit,
    /// The app's Pause (tray): nothing is recorded while it answers true.
    paused: fn() -> bool,
    config: Mutex<Config>,
    /// Newest first.
    entries: Mutex<Vec<Entry>>,
    /// The listener's window; 0 while it does not run.
    hwnd: AtomicIsize,
    /// The clipboard's sequence number when it was last looked at.
    seen: AtomicU32,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// A running history. Clones share it; `stop` ends the listener.
#[derive(Clone)]
pub struct History(Arc<Inner>);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a change of the clipboard turned out to be.
enum Got {
    Skip(&'static str),
    Files(Vec<String>),
    Png(Vec<u8>),
    Bitmap(Vec<u8>),
    /// A text, and whether a link in it may be rewritten without losing anything.
    Text(String, bool),
}

/// A link may be rewritten when the clipboard holds nothing but text and notes
/// about it: any other format (a picture, a file, a document) means the link
/// is only the textual side of something richer.
fn only_text(clipboard: &clip::Open, formats: &[u32]) -> bool {
    const NOTES: &[&str] = &[
        "uniformresourcelocatorw", "uniformresourcelocator", "html format", "rich text format", "dataobject",
        "ole private data", "link preview format", "mssourceurl", "canincludeinclipboardhistory", "canuploadtocloudclipboard",
    ];
    let plain = formats.iter().all(|&f| {
        let name = clip::format_name(f).to_lowercase();
        matches!(f, clip::CF_TEXT | clip::CF_OEMTEXT | clip::CF_UNICODETEXT | clip::CF_LOCALE)
            || NOTES.contains(&name.as_str())
            || name.starts_with("chromium ")
            || name.starts_with("text/")
    });
    // ponytail: formatted text beside the link is given up for the clean link, unless it holds a picture
    // or is long; keeping a titled link (rewriting the HTML too) is the upgrade.
    let html = clip::format("HTML Format");
    let rich = !formats.contains(&html)
        || clipboard.bytes(html, 64 << 10).is_some_and(|bytes| {
            let text = String::from_utf8_lossy(&bytes).to_lowercase();
            !["<img", "<video", "<audio", "<picture", "<svg", "<iframe", "<object", "<embed"].iter().any(|tag| text.contains(tag))
        });
    plain && rich
}

impl Inner {
    fn config(&self) -> Config {
        lock(&self.config).clone()
    }

    fn images(&self) -> PathBuf {
        self.dir.join("images")
    }

    fn save(&self, entries: &[Entry]) {
        if let Err(err) = storage::json_save_atomic(&self.dir.join("history.json"), &entries) {
            log::line(format!("clipboard: cannot save the history: {err}"));
        }
        // A picture whose entry is gone goes too.
        for file in std::fs::read_dir(self.images()).into_iter().flatten().flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if !entries.iter().any(|e| e.kind() == Kind::Image && format!("{}.png", e.id) == name) {
                let _ = std::fs::remove_file(file.path());
            }
        }
    }

    /// Age, count and size: what is over goes, oldest first; a pinned entry never does.
    /// The user's "Unlimited" drops the count and the byte budgets: then only the age they set
    /// (if any) ever takes an entry away.
    fn trim(&self, entries: &mut Vec<Entry>) {
        let cfg = self.config();
        let unlimited = cfg.limit == usize::MAX;
        let now = now_ms();
        let (mut count, mut texts, mut pictures) = (0usize, 0usize, 0u64);
        entries.retain(|e| {
            if e.pinned {
                return true;
            }
            count += 1;
            texts += e.text.len();
            pictures += e.size;
            let fresh = cfg.keep_days == 0 || now.saturating_sub(e.at) < cfg.keep_days * 86_400_000;
            fresh && (unlimited || (count <= cfg.limit && texts <= TEXTS_CAP && pictures <= PICTURES_CAP))
        });
    }

    /// Changes the entries, saves and tells the page.
    fn edit<T>(&self, change: impl FnOnce(&mut Vec<Entry>) -> T) -> T {
        let mut entries = lock(&self.entries);
        let out = change(&mut entries);
        self.trim(&mut entries);
        self.save(&entries);
        drop(entries);
        (self.emit)("changed", Value::Null);
        out
    }

    fn skip(&self, why: &'static str, formats: &[u32]) {
        let formats: Vec<String> = formats.iter().map(|&f| clip::format_name(f)).collect();
        (self.emit)("skipped", json!({ "why": why, "formats": formats }));
    }

    // ── A change of the clipboard (the listener's thread) ───────────────────────

    fn on_change(&self) {
        let seq = clip::sequence();
        if self.seen.swap(seq, Ordering::SeqCst) == seq {
            return;
        }
        // Whatever it is, it gets its full delay before the clipboard is emptied.
        self.arm_clear();
        if clip::is_own() || clip::is_empty() {
            return;
        }
        let cfg = self.config();
        if !cfg.recording || (self.paused)() {
            return self.skip("recording is paused", &[]);
        }

        let (owner, front) = source();
        let left_out =
            |exe: &Option<String>| exe.as_deref().map(file_name).is_some_and(|name| cfg.ignored.contains(&name));
        // Read and close: nothing below this block holds the clipboard.
        let (formats, got) = {
            let Ok(clipboard) = clip::open() else { return };
            let formats = clipboard.formats();
            let png = clip::format("PNG");
            let got = if left_out(&owner) || left_out(&front) {
                // The only filter left: the user's own list of programs.
                Got::Skip("copied in an app that is left out")
            } else if formats.contains(&clip::CF_HDROP) {
                if cfg.media { Got::Files(clipboard.files()) } else { Got::Skip("files are not recorded") }
            } else if formats.contains(&clip::CF_UNICODETEXT) && !formats.contains(&png) {
                // Text first: a spreadsheet or a document also offers a picture of what was copied.
                // A real picture (a browser's "Copy image", a screenshot) comes with a PNG or without text.
                match clipboard.text(TEXT_CAP) {
                    Some(text) => {
                        let rewrite = cfg.clean_urls && urls::is_web(&text) && only_text(&clipboard, &formats);
                        Got::Text(text, rewrite)
                    }
                    None => Got::Skip("the text is longer than a million characters"),
                }
            } else if !cfg.media {
                Got::Skip("pictures are not recorded")
            } else if let Some(bytes) = clipboard.bytes(png, PNG_CAP) {
                Got::Png(bytes)
            } else if let Some(bytes) = clipboard.bytes(clip::CF_DIB, BITMAP_CAP) {
                Got::Bitmap(bytes)
            } else {
                Got::Skip("nothing the history keeps (or a picture too large)")
            };
            (formats, got)
        };

        let mut entry = Entry { id: new_id(), at: now_ms(), ..Default::default() };
        if let Some(exe) = owner.or(front) {
            entry.app = file_name(&exe);
            entry.exe = exe;
        }
        let mut picture = None;
        match got {
            Got::Skip(why) => return self.skip(why, &formats),
            Got::Files(paths) => {
                if paths.is_empty() {
                    return self.skip("no file in the copy", &formats);
                }
                entry.kind = Some(Kind::Files);
                entry.text = paths.into_iter().take(FILES_CAP).collect::<Vec<_>>().join("\n");
            }
            Got::Text(text, rewrite) => {
                if text.trim().is_empty() {
                    return self.skip("empty text", &formats);
                }
                entry.text = text;
                if urls::is_web(&entry.text) {
                    entry.kind = Some(Kind::Link);
                    entry.text = entry.text.trim().to_string();
                }
                let cleaned = if rewrite { urls::clean(&entry.text, &cfg.clean_extra) } else { None };
                if let Some(cleaned) = cleaned.filter(|c| !c.removed.is_empty()) {
                    // A newer copy wins: the clipboard is only rewritten while it still holds this one.
                    if clip::sequence() == seq {
                        if let Ok(clipboard) = clip::open() {
                            let wide: Vec<u8> = cleaned.url.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
                            clipboard.empty();
                            let _ = clipboard.set_text(&cleaned.url);
                            let _ = clipboard.set(clip::format("UniformResourceLocatorW"), &wide);
                        }
                        clip::mark_own();
                    }
                    (self.emit)("cleaned", json!({ "removed": cleaned.removed, "chars": cleaned.url.chars().count() }));
                    entry.text = cleaned.url;
                }
            }
            Got::Png(_) | Got::Bitmap(_) => {
                let png = match got {
                    Got::Png(bytes) => Some(bytes),
                    Got::Bitmap(bytes) => dib_to_png(&bytes).filter(|png| png.len() <= PNG_CAP),
                    _ => None,
                };
                let Some((png, (width, height))) = png.and_then(|png| png_size(&png).map(|size| (png, size))) else {
                    return self.skip("a picture in a form the history cannot keep", &formats);
                };
                entry.kind = Some(Kind::Image);
                (entry.width, entry.height, entry.size) = (width, height, png.len() as u64);
                entry.hash = Sha256::digest(&png).iter().map(|b| format!("{b:02x}")).collect();
                picture = Some(png);
            }
        }

        let told = json!({
            "kind": entry.kind(),
            "chars": entry.text.chars().count(),
            "bytes": entry.size,
            "app": entry.app,
            "formats": formats.iter().map(|&f| clip::format_name(f)).collect::<Vec<_>>(),
        });
        let images = self.images();
        let kept = self.edit(|entries| {
            if let Some(at) = entries.iter().position(|e| e.same(&entry)) {
                // Copied again: the entry comes to the front and keeps its id and its pin.
                let mut old = entries.remove(at);
                old.at = entry.at;
                entries.insert(0, old);
                return true;
            }
            if let Some(png) = &picture {
                let written = std::fs::create_dir_all(&images).and_then(|_| std::fs::write(images.join(format!("{}.png", entry.id)), png));
                if let Err(err) = written {
                    log::line(format!("clipboard: cannot keep a picture: {err}"));
                    return false;
                }
            }
            entries.insert(0, entry);
            true
        });
        if kept {
            (self.emit)("captured", told);
        }
    }

    // ── Auto clear ────────────────────────────────────────────────────────────

    /// Starts the wait again (the listener's thread only: the timer is its window's).
    fn arm_clear(&self) {
        let hwnd = HWND(self.hwnd.load(Ordering::SeqCst) as *mut _);
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_CLEAR);
            if let Some(after) = self.config().clear_after.filter(|_| !clip::is_empty()) {
                SetTimer(Some(hwnd), TIMER_CLEAR, after.as_millis() as u32, None);
            }
        }
    }

    fn clear_clipboard(&self, why: &str) {
        if clip::is_empty() {
            return;
        }
        match clip::open() {
            Ok(clipboard) => clipboard.empty(),
            Err(_) => return,
        }
        clip::mark_own();
        if self.config().clear_windows_history {
            // False when the user has Windows' history off: nothing to clear then.
            let _ = windows::ApplicationModel::DataTransfer::Clipboard::ClearHistory();
        }
        (self.emit)("cleared", json!({ "why": why }));
    }
}

// ── The listener's thread ─────────────────────────────────────────────────────

/// A burst of writes (Office makes several per copy) is read once, this long after the last.
const SETTLE_MS: u32 = 80;
const TIMER_READ: usize = 1;
const TIMER_CLEAR: usize = 2;
/// The settings changed: start the wait for the auto clear again.
const WM_REARM: u32 = WM_APP + 1;

thread_local! {
    static LISTENER: std::cell::RefCell<Option<Arc<Inner>>> = const { std::cell::RefCell::new(None) };
}

fn listener() -> Option<Arc<Inner>> {
    LISTENER.with_borrow(Clone::clone)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            unsafe { SetTimer(Some(hwnd), TIMER_READ, SETTLE_MS, None) };
            LRESULT(0)
        }
        WM_TIMER => {
            let _ = unsafe { KillTimer(Some(hwnd), wparam.0) };
            if let Some(inner) = listener() {
                match wparam.0 {
                    TIMER_READ => inner.on_change(),
                    // A copy that landed a moment ago and is not read yet is owed its own delay.
                    TIMER_CLEAR if clip::sequence() == inner.seen.load(Ordering::SeqCst) => inner.clear_clipboard("delay"),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_REARM => {
            if let Some(inner) = listener() {
                inner.arm_clear();
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = RemoveClipboardFormatListener(hwnd);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn run(inner: Arc<Inner>, ready: mpsc::Sender<Result<(), String>>) {
    unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("lintel-clipboard");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: module.into(), lpszClassName: class, ..Default::default() });
        // Never shown: it exists to be told of clipboard changes and to own two timers.
        let made = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, class, w!(""), WS_POPUP, 0, 0, 0, 0, None, None, Some(module.into()), None)
            .and_then(|hwnd| AddClipboardFormatListener(hwnd).map(|_| hwnd));
        let hwnd = match made {
            Ok(hwnd) => hwnd,
            Err(err) => {
                let _ = ready.send(Err(format!("Windows would not report clipboard changes: {}", err.message())));
                return;
            }
        };
        // What is on the clipboard now was copied before anyone listened: it is not recorded.
        inner.seen.store(clip::sequence(), Ordering::SeqCst);
        inner.hwnd.store(hwnd.0 as isize, Ordering::SeqCst);
        LISTENER.set(Some(inner.clone()));
        inner.arm_clear();
        let _ = ready.send(Ok(()));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }

        inner.hwnd.store(0, Ordering::SeqCst);
        LISTENER.take();
        let _ = UnregisterClassW(class, Some(module.into()));
    }
}

impl History {
    /// Loads what was kept in `dir` and starts listening.
    pub fn start(dir: PathBuf, config: Config, emit: Emit, paused: fn() -> bool) -> Result<Self, String> {
        let inner = Arc::new(Inner {
            entries: Mutex::new(storage::json_load(&dir.join("history.json"))),
            dir,
            emit,
            paused,
            config: Mutex::new(config),
            hwnd: AtomicIsize::new(0),
            seen: AtomicU32::new(0),
            thread: Mutex::new(None),
        });
        {
            // What became too old while the app was closed goes now.
            let mut entries = lock(&inner.entries);
            inner.trim(&mut entries);
            inner.save(&entries);
        }
        let (ready, started) = mpsc::channel();
        let thread = {
            let inner = inner.clone();
            std::thread::spawn(move || run(inner, ready))
        };
        started.recv().unwrap_or_else(|_| Err("The clipboard listener did not start.".into()))?;
        *lock(&inner.thread) = Some(thread);
        Ok(Self(inner))
    }

    /// Ends the listener. Nothing is recorded and nothing is cleared after this.
    pub fn stop(&self) {
        let Some(thread) = lock(&self.0.thread).take() else { return };
        let hwnd = self.0.hwnd.load(Ordering::SeqCst);
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        // ponytail: a source that never answers a read would hold the thread; it is left behind after
        // half a second instead of holding the app (it ends when that read returns).
        for _ in 0..50 {
            if thread.is_finished() {
                let _ = thread.join();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        log::line("clipboard: the listener is still inside a read; left to end on its own");
    }

    pub fn set_config(&self, config: Config) {
        *lock(&self.0.config) = config;
        self.0.edit(|_| ());
        unsafe {
            let _ = PostMessageW(Some(HWND(self.0.hwnd.load(Ordering::SeqCst) as *mut _)), WM_REARM, WPARAM(0), LPARAM(0));
        }
    }

    pub fn config(&self) -> Config {
        self.0.config()
    }

    /// Pinned entries first, then the rest, newest first.
    pub fn entries(&self) -> Vec<Entry> {
        let mut all = lock(&self.0.entries).clone();
        all.sort_by_key(|e| !e.pinned);
        all
    }

    /// The entries with these ids, in the order of the list.
    pub fn picked(&self, ids: &[String]) -> Vec<Entry> {
        self.entries().into_iter().filter(|e| ids.contains(&e.id)).collect()
    }

    /// Where the PNG of a picture entry is.
    pub fn image_path(&self, entry: &Entry) -> PathBuf {
        self.0.images().join(format!("{}.png", entry.id))
    }

    pub fn pin(&self, id: &str, on: bool) {
        self.0.edit(|entries| entries.iter_mut().filter(|e| e.id == id).for_each(|e| e.pinned = on));
    }

    pub fn remove(&self, ids: &[String]) {
        self.0.edit(|entries| entries.retain(|e| !ids.contains(&e.id)));
    }

    /// Deletes the history, from memory and from disk: everything, or all but the pinned entries.
    pub fn clear(&self, pinned_too: bool) {
        self.0.edit(|entries| entries.retain(|e| e.pinned && !pinned_too));
    }

    /// Puts entries back on the clipboard: one as it was copied; several as
    /// files when they all are, as lines of text otherwise. The write is the
    /// app's own, so it is not recorded again; one entry comes to the front.
    pub fn write(&self, ids: &[String]) -> Result<(), String> {
        let picked = self.picked(ids);
        let (files, rest): (Vec<&Entry>, Vec<&Entry>) = picked.iter().partition(|e| e.kind() == Kind::Files);
        let fill: Box<dyn Fn(&clip::Open) -> Result<(), String>> = match (picked.as_slice(), rest.is_empty()) {
            ([], _) => return Err("That entry is not in the history any more.".into()),
            ([one], _) if one.kind() == Kind::Image => {
                let gone = "The picture of that entry is gone.";
                let png = std::fs::read(self.image_path(one)).map_err(|_| gone)?;
                let dib = png_to_dib(&png).ok_or(gone)?;
                Box::new(move |c| c.set(clip::format("PNG"), &png).and_then(|_| c.set(clip::CF_DIB, &dib)))
            }
            (_, true) => {
                let paths: Vec<String> =
                    files.iter().flat_map(|e| e.text.lines()).filter(|p| Path::new(p).exists()).map(str::to_string).collect();
                if paths.is_empty() {
                    return Err("Those files are not there any more.".into());
                }
                Box::new(move |c| c.set_files(&paths))
            }
            _ => {
                let text = picked.iter().filter(|e| e.kind() != Kind::Image).map(|e| e.text.as_str()).collect::<Vec<_>>().join("\r\n");
                Box::new(move |c| c.set_text(&text))
            }
        };
        {
            let clipboard = clip::open()?;
            clipboard.empty();
            fill(&clipboard)?;
        }
        clip::mark_own();
        if let [one] = picked.as_slice() {
            self.0.edit(|entries| {
                if let Some(at) = entries.iter().position(|e| e.id == one.id) {
                    let mut entry = entries.remove(at);
                    entry.at = now_ms();
                    entries.insert(0, entry);
                }
            });
        }
        Ok(())
    }

    /// Empties the clipboard now ("lock", "sleep", "display"), when the user asked for it on that occasion.
    pub fn clear_on(&self, occasion: &str) {
        let cfg = self.0.config();
        let wanted = match occasion {
            "lock" => cfg.clear_on_lock,
            "sleep" => cfg.clear_on_sleep,
            "display" => cfg.clear_on_display_off,
            _ => true,
        };
        if wanted {
            self.0.clear_clipboard(occasion);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(limit: usize) -> Inner {
        Inner {
            dir: std::env::temp_dir().join("lintel-clipboard-test"),
            emit: Arc::new(|_, _| {}),
            paused: || false,
            config: Mutex::new(Config { limit, keep_days: 0, ..Config::default() }),
            entries: Mutex::new(Vec::new()),
            hwnd: AtomicIsize::new(0),
            seen: AtomicU32::new(0),
            thread: Mutex::new(None),
        }
    }

    fn copies(n: usize) -> Vec<Entry> {
        (0..n).map(|i| Entry { id: i.to_string(), text: format!("copy {i}"), at: now_ms(), ..Default::default() }).collect()
    }

    #[test]
    fn unlimited_keeps_every_entry_while_a_count_still_trims() {
        let mut many = copies(1200);
        history(usize::MAX).trim(&mut many);
        assert_eq!(many.len(), 1200);

        let mut capped = copies(1200);
        history(100).trim(&mut capped);
        assert_eq!(capped.len(), 100);
        assert_eq!(capped[99].id, "99");
    }

    #[test]
    fn a_limit_of_zero_in_the_settings_is_unlimited() {
        assert_eq!(Config::from_settings(&json!({ "limit": 0 })).limit, usize::MAX);
        assert_eq!(Config::from_settings(&json!({ "limit": 250 })).limit, 250);
        assert_eq!(Config::from_settings(&json!({})).limit, 100);
    }
}
