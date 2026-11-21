// The panel's icon in the notification area: a sun of its own, not the program's icon, and the push
// that keeps it next to the clock. Windows 11 keeps each tray icon's pinned state in
// HKCU\Control Panel\NotifyIconSettings, one key per icon, found by the program behind it
// (`ExecutablePath`) and the tooltip the icon was registered with (`InitialTooltip`).
//
// Only this feature's icon is pinned there: every other notification-area entry of this program,
// the island's own icon included, is set back to unpinned. The check runs on each start and writes
// only a value that is wrong, so the sun is always the one icon of this program on the taskbar and
// no stale entry from an earlier version stays there.
//
// debt: the key and its values are not documented; the taskbar settings write the same ones.

use std::time::Duration;

use tauri::image::Image;
use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    KEY_SET_VALUE, REG_DWORD, REG_SZ, REG_VALUE_TYPE,
};

/// The tooltip the tray icon is registered with; Windows remembers it as `InitialTooltip`.
const TOOLTIP: &str = "Brightness and contrast";

/// A warm sun, 32 × 32 RGBA: the icon in the notification area and on the panel's taskbar button.
pub fn sun() -> Image<'static> {
    const SIDE: usize = 32;
    const SAMPLES: usize = 4;
    const CENTRE: f32 = 16.0;
    const CORE: f32 = 7.4;
    const RAY_IN: f32 = 10.4;
    const RAY_OUT: f32 = 14.6;
    const RAY_HALF: f32 = 1.7;
    let mut px = vec![0u8; SIDE * SIDE * 4];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let (mut hits, mut rgb) = (0u32, [0u32; 3]);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let dx = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32 - CENTRE;
                    let dy = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32 - CENTRE;
                    let r = (dx * dx + dy * dy).sqrt();
                    // Eight rays, and the disc at the middle.
                    let angle = dy.atan2(dx);
                    let off = (angle.rem_euclid(std::f32::consts::FRAC_PI_4) - std::f32::consts::FRAC_PI_8).abs();
                    let ray = r >= RAY_IN && r <= RAY_OUT && r * off.sin() <= RAY_HALF;
                    if r > CORE && !ray {
                        continue;
                    }
                    // Amber, a little lighter at the top.
                    let t = (dy + CENTRE) / (2.0 * CENTRE);
                    let col = [255.0 - 24.0 * t, 204.0 - 44.0 * t, 108.0 - 66.0 * t];
                    for i in 0..3 {
                        rgb[i] += col[i].round() as u32;
                    }
                    hits += 1;
                }
            }
            if hits == 0 {
                continue;
            }
            let o = (y * SIDE + x) * 4;
            for i in 0..3 {
                px[o + i] = (rgb[i] / hits) as u8;
            }
            px[o + 3] = (hits * 255 / (SAMPLES * SAMPLES) as u32) as u8;
        }
    }
    Image::new_owned(px, SIDE as u32, SIDE as u32)
}

/// Keeps this feature's icon next to the clock, and it alone. Explorer writes an icon's settings key
/// a moment after the icon is registered, so this retries for a few seconds and then gives up.
pub fn enforce_pin() {
    let Some(exe) = std::env::current_exe().ok().map(|p| p.to_string_lossy().to_lowercase()) else { return };
    for _ in 0..30 {
        if promote(&exe) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Pins the icon whose first tooltip is ours and unpins every other icon of this program. True when
/// that icon's settings key was found, false while Explorer has not written it yet.
fn promote(exe: &str) -> bool {
    let Some(store) = open(r"Control Panel\NotifyIconSettings", KEY_READ) else { return false };
    let mut found = false;
    let mut seen: Vec<String> = Vec::new();
    for name in subkeys(store) {
        let Some(key) = open_on(store, &name, KEY_READ | KEY_SET_VALUE) else { continue };
        let tip = string_of(key, w!("InitialTooltip")).unwrap_or_default();
        let path = string_of(key, w!("ExecutablePath")).unwrap_or_default().to_lowercase();
        // Another program's icon is never touched.
        if path == exe {
            let mine = tip == TOOLTIP;
            found |= mine;
            let want = u32::from(mine);
            if dword_of(key, w!("IsPromoted")) != Some(want) {
                let bytes = want.to_ne_bytes();
                unsafe { _ = RegSetValueExW(key, w!("IsPromoted"), None, REG_DWORD, Some(&bytes)) };
            }
            seen.push(format!("{tip:?}{}", if mine { " (pinned)" } else { "" }));
        }
        unsafe { _ = RegCloseKey(key) };
    }
    unsafe { _ = RegCloseKey(store) };
    if found {
        notch_core::log::line(format!("displays-keepawake: notification area: {}", seen.join(", ")));
    }
    found
}

fn open(path: &str, access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    let wide = wide(path);
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr()), None, access, &mut key) }.is_ok().then_some(key)
}

fn open_on(parent: HKEY, name: &str, access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    let wide = wide(name);
    unsafe { RegOpenKeyExW(parent, PCWSTR(wide.as_ptr()), None, access, &mut key) }.is_ok().then_some(key)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn subkeys(key: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        let ok = unsafe { RegEnumKeyExW(key, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
        if ok.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    out
}

fn string_of(key: HKEY, name: PCWSTR) -> Option<String> {
    let mut buffer = [0u16; 256];
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = (buffer.len() * 2) as u32;
    unsafe { RegQueryValueExW(key, name, None, Some(&mut kind), Some(buffer.as_mut_ptr() as *mut u8), Some(&mut size)) }
        .ok()
        .ok()?;
    if kind != REG_SZ {
        return None;
    }
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

fn dword_of(key: HKEY, name: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    unsafe { RegQueryValueExW(key, name, None, None, Some(&mut v as *mut u32 as *mut u8), Some(&mut size)) }
        .is_ok()
        .then_some(v)
}
