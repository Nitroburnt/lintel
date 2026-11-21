// What the feature reads from Windows: how long the person has been idle, whether an app has the
// microphone open, and which program a window belongs to. Reads only, each on the caller's time.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, POINT};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ,
    RRF_RT_REG_QWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetCursorPos, GetWindowTextW, GetWindowThreadProcessId, WindowFromPoint,
};

/// Seconds since the last key or pointer input of this session (`GetLastInputInfo`).
pub fn idle_secs() -> u64 {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return 0;
    }
    (unsafe { GetTickCount() }.wrapping_sub(info.dwTime) / 1000) as u64
}

/// Whether Windows itself says not to interrupt: its quiet hours (Focus Assist), or a
/// presentation. A full-screen game is `QUNS_BUSY` and is the full-screen hold's business, not
/// this one.
pub fn quiet_time() -> bool {
    unsafe { SHQueryUserNotificationState() }
        .map(|state| state == QUNS_QUIET_TIME || state == QUNS_PRESENTATION_MODE)
        .unwrap_or(false)
}

/// The text of a window's title bar ("main.rs - lintel - Visual Studio Code"). Empty when it has
/// none or Windows will not say. Read only when the person turns window titles on.
pub fn title_of(hwnd: isize) -> String {
    let mut text = [0u16; 512];
    let len = unsafe { GetWindowTextW(HWND(hwnd as *mut _), &mut text) };
    String::from_utf16_lossy(&text[..len.max(0) as usize])
}

/// Whether the pointer is over a window of this program, its island and its floating panels
/// included. Those never take the keyboard, so the window in front stays somebody else's while
/// the person uses them: that time is Lintel's, not that program's.
pub fn pointer_over_self() -> bool {
    unsafe {
        let mut at = POINT::default();
        if GetCursorPos(&mut at).is_err() {
            return false;
        }
        // What a click at the pointer would land on: a click-through part answers with the window
        // under it, which is right — the person is looking at that one.
        let win = WindowFromPoint(at);
        if win.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(win, Some(&mut pid));
        pid == std::process::id()
    }
}

// ── The microphone ────────────────────────────────────────────────────────────
//
// Windows keeps, for its privacy page, one key per app that ever used the microphone: packaged
// apps directly under the store, desktop programs under `NonPackaged` (the path with `#` for `\`).
// While an app has the microphone open its `LastUsedTimeStop` is 0.

const STORE: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone");

fn open(parent: HKEY, path: PCWSTR) -> Option<HKEY> {
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(parent, path, None, KEY_READ, &mut key) }.is_ok().then_some(key)
}

fn subkeys(key: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        if unsafe { RegEnumKeyExW(key, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) }.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    out
}

fn open_now(key: HKEY, app: &str) -> bool {
    let mut stop = u64::MAX;
    let mut size = 8u32;
    let read = unsafe {
        RegGetValueW(
            key,
            &HSTRING::from(app),
            w!("LastUsedTimeStop"),
            RRF_RT_REG_QWORD,
            None,
            Some(&mut stop as *mut u64 as *mut _),
            Some(&mut size),
        )
    };
    read.is_ok() && stop == 0
}

/// The apps that have the microphone open now ("Teams.exe"), this app left out: its own dictation
/// is not somebody's call.
pub fn mic_in_use() -> Vec<String> {
    let me = std::env::current_exe().ok().map(|p| p.to_string_lossy().replace('\\', "#").to_lowercase());
    let mut out = Vec::new();
    let Some(store) = open(HKEY_CURRENT_USER, STORE) else { return out };
    let mut scan = |key: HKEY| {
        for app in subkeys(key) {
            if open_now(key, &app) && me.as_deref() != Some(app.to_lowercase().as_str()) {
                out.push(app.rsplit('#').next().unwrap_or(&app).to_string());
            }
        }
    };
    scan(store);
    if let Some(loose) = open(store, w!("NonPackaged")) {
        scan(loose);
        unsafe { _ = RegCloseKey(loose) };
    }
    unsafe { _ = RegCloseKey(store) };
    out
}

// ── Which program ─────────────────────────────────────────────────────────────

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let read = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        read.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// `FileDescription` of the program's version information: "Google Chrome", "Spotify".
/// debt: the same reading as crates/audio-mixer/src/apps.rs; it belongs in notch_core::win.
fn description(exe: &str) -> Option<String> {
    let path = HSTRING::from(exe);
    unsafe {
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&path, None, size, data.as_mut_ptr().cast()).ok()?;
        let mut at = std::ptr::null_mut();
        let mut len = 0u32;
        // The languages the file has strings in; the first is the one it was written in.
        if !VerQueryValueW(data.as_ptr().cast(), w!("\\VarFileInfo\\Translation"), &mut at, &mut len).as_bool() || len < 4 {
            return None;
        }
        let [language, page] = (at as *const [u16; 2]).read_unaligned();
        let key = HSTRING::from(format!("\\StringFileInfo\\{language:04x}{page:04x}\\FileDescription"));
        if !VerQueryValueW(data.as_ptr().cast(), &key, &mut at, &mut len).as_bool() || at.is_null() {
            return None;
        }
        // Up to its end: some files give a length that runs on into the next entry.
        let text: Vec<u16> =
            (0..len as usize).map(|i| (at as *const u16).add(i).read_unaligned()).take_while(|c| *c != 0).collect();
        Some(String::from_utf16_lossy(&text).trim().to_owned())
    }
}

fn stem(file: &str) -> &str {
    let file = file.rsplit(['\\', '/']).next().unwrap_or(file);
    match file.rsplit_once('.') {
        Some((name, ext)) if ext.eq_ignore_ascii_case("exe") => name,
        _ => file,
    }
}

/// What a program calls itself, by its path: read from the file once.
static NAMES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// The program a window in front belongs to, as (key, name, path): ("chrome", "Google Chrome",
/// "C:\\…\\chrome.exe"). The key is the file name, lower case, without ".exe". Never the window's
/// title. `None` when Windows will not say (a window of another account, the lock screen).
pub fn app_of(hwnd: isize, pid: u32, exe: &str) -> Option<crate::usage::AppInfo> {
    let mut pid = pid;
    // A Store app's frame belongs to a host every such app shares; the app is the window inside.
    if exe.eq_ignore_ascii_case("ApplicationFrameHost.exe") {
        unsafe {
            if let Ok(core) = FindWindowExW(Some(HWND(hwnd as *mut _)), None, w!("Windows.UI.Core.CoreWindow"), None) {
                let mut inner = 0u32;
                GetWindowThreadProcessId(core, Some(&mut inner));
                if inner != 0 {
                    pid = inner;
                }
            }
        }
    }
    // A program Windows will not name keeps the file name it was seen with, and an empty path:
    // `exe_paths`/`app_path` fill that in later, from the running process or the registry.
    let path = process_path(pid).unwrap_or_default();
    let file = if path.is_empty() { exe.to_string() } else { path.clone() };
    let key = stem(&file).to_lowercase();
    if key.is_empty() || key == "lockapp" {
        return None;
    }
    let mut names = NAMES.lock().unwrap();
    let name = names.get_or_insert_with(HashMap::new).entry(file.clone()).or_insert_with(|| {
        description(&file)
            .map(|d| stem(&d).to_string())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| stem(&file).to_string())
    });
    Some(crate::usage::AppInfo { key, name: name.clone(), path })
}

/// Every running program's file, by key — the exe's stem, lower case, the key the store uses.
/// A process Windows will not open (another account, a protected one) is left out.
pub fn exe_paths() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pid in notch_core::win::process_table().keys() {
        if let Some(path) = process_path(*pid) {
            let key = stem(&path).to_lowercase();
            if !key.is_empty() {
                out.entry(key).or_insert(path);
            }
        }
    }
    out
}

/// Where an installed program says it lives, for one that is not running: App Paths in the
/// registry (the user's, the machine's, the 32-bit view), then PATH.
pub fn app_path(key: &str) -> Option<String> {
    const ROOTS: [HKEY; 3] = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, HKEY_LOCAL_MACHINE];
    const BASES: [&str; 3] = [
        r"Software\Microsoft\Windows\CurrentVersion\App Paths",
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
        r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\App Paths",
    ];
    for (root, base) in ROOTS.into_iter().zip(BASES) {
        // The subkey is the program's file; its default value is the program's path.
        let sub = HSTRING::from(format!("{base}\\{key}.exe"));
        let mut buf = [0u16; 1024];
        let mut size = std::mem::size_of_val(&buf) as u32;
        let read = unsafe { RegGetValueW(root, &sub, None, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size)) };
        if read.is_ok() {
            // The size counts the nul, in bytes.
            let text = String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]);
            if std::path::Path::new(&text).is_file() {
                return Some(text);
            }
        }
    }
    notch_core::win::find_on_path(key).map(|p| p.to_string_lossy().into_owned())
}
