// The displays and their brightness. Reads happen when asked; a write happens
// only for a user's slider move (the worker in lib.rs paces and coalesces).
//
// Built-in panel: WMI (root\WMI, WmiMonitorBrightness / WmiSetBrightness).
// External monitor: DDC/CI luminance (VCP 0x10) through the monitor
// configuration API. Anything else can still be dimmed by the overlay.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Mutex;

use windows::core::{w, IUnknown, Interface, BSTR};
use windows::Win32::Devices::Display::*;
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::{
    IWbemLocator, IWbemServices, WbemLocator, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE,
};

#[derive(Clone, Debug)]
pub struct Display {
    /// The GDI name, `\\.\DISPLAY1`.
    pub id: String,
    pub name: String,
    pub internal: bool,
    pub primary: bool,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// The work area (the taskbar left out), physical pixels: left, top, right, bottom.
    pub work: (i32, i32, i32, i32),
    hmon: isize,
}

/// How the hardware backlight of a display can be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Wmi,
    /// DDC/CI answers luminance reads.
    Ddc,
    /// DDC/CI handle exists but luminance cannot be read: writes are tried, the start is a guess.
    DdcWrite,
    /// Only the dimming overlay.
    None,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Wmi => "wmi",
            Method::Ddc => "ddc",
            Method::DdcWrite => "ddc-write",
            Method::None => "none",
        }
    }
}

// ── Listing ───────────────────────────────────────────────────────────────────

unsafe extern "system" fn each(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    unsafe {
        let out = &mut *(data.0 as *mut Vec<Display>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let area = info.monitorInfo.rcWork;
            let id = String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').to_string();
            out.push(Display {
                name: id.clone(),
                id,
                internal: false,
                primary: info.monitorInfo.dwFlags & 1 != 0,
                x: r.left,
                y: r.top,
                w: r.right - r.left,
                h: r.bottom - r.top,
                work: (area.left, area.top, area.right, area.bottom),
                hmon: hmon.0 as isize,
            });
        }
        true.into()
    }
}

fn text(buf: &[u16]) -> String {
    String::from_utf16_lossy(buf).trim_end_matches('\0').to_string()
}

/// GDI name -> (friendly name, built-in) from the active display paths.
fn names() -> HashMap<String, (String, bool)> {
    let mut out = HashMap::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).0 != 0 {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).0 != 0 {
            return out;
        }
        for p in &paths[..np as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut tgt.header) != 0 {
                continue;
            }
            // INTERNAL, LVDS, DISPLAYPORT_EMBEDDED, UDI_EMBEDDED
            let tech = tgt.outputTechnology.0;
            let internal = matches!(tech, i32::MIN | 6 | 11 | 13);
            let friendly = text(&tgt.monitorFriendlyDeviceName);
            out.insert(text(&src.viewGdiDeviceName), (friendly, internal));
        }
    }
    out
}

pub fn list() -> Vec<Display> {
    let mut out: Vec<Display> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut out as *mut _ as isize));
    }
    let names = names();
    for d in &mut out {
        if let Some((friendly, internal)) = names.get(&d.id) {
            d.internal = *internal;
            d.name = if !friendly.is_empty() {
                friendly.clone()
            } else if *internal {
                "Built-in display".into()
            } else {
                d.id.clone()
            };
        }
    }
    out.sort_by(|a, b| b.primary.cmp(&a.primary).then(a.id.cmp(&b.id)));
    out
}

/// The display a screen point is on.
pub fn at_point(x: i32, y: i32) -> Option<Display> {
    list().into_iter().find(|d| x >= d.x && x < d.x + d.w && y >= d.y && y < d.y + d.h)
}

// ── DDC/CI ────────────────────────────────────────────────────────────────────

static DDC_MAX: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

fn with_physical<R>(d: &Display, f: impl FnOnce(windows::Win32::Foundation::HANDLE) -> R) -> Option<R> {
    unsafe {
        let hmon = HMONITOR(d.hmon as *mut _);
        let mut n = 0u32;
        GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut n).ok()?;
        if n == 0 {
            return None;
        }
        let mut arr = vec![PHYSICAL_MONITOR::default(); n as usize];
        GetPhysicalMonitorsFromHMONITOR(hmon, &mut arr).ok()?;
        let r = f(arr[0].hPhysicalMonitor);
        let _ = DestroyPhysicalMonitors(&arr);
        Some(r)
    }
}

/// (current, max) of one VCP code; `Ok(None)` = a handle but no answer to the read.
fn ddc_read_code(d: &Display, code: u8) -> Option<Option<(u32, u32)>> {
    with_physical(d, |h| unsafe {
        let (mut cur, mut max) = (0u32, 0u32);
        if GetVCPFeatureAndVCPFeatureReply(h, code, None, &mut cur, Some(&mut max)) != 0 {
            Some((cur, if max == 0 { 100 } else { max }))
        } else {
            None
        }
    })
}

/// (current, max) luminance; `Ok(None)` = a handle but no answer to the read.
fn ddc_read(d: &Display) -> Option<Option<(u32, u32)>> {
    ddc_read_code(d, 0x10)
}

fn ddc_write(d: &Display, percent: u32) -> bool {
    let max = DDC_MAX.lock().unwrap().as_ref().and_then(|m| m.get(&d.id).copied()).unwrap_or(100);
    with_physical(d, |h| unsafe { SetVCPFeature(h, 0x10, percent.min(100) * max / 100) != 0 }).unwrap_or(false)
}

/// Contrast (VCP 0x12), 0–100. A built-in panel has no such control; `None` also
/// when the monitor has a DDC handle but will not answer.
pub fn read_contrast(d: &Display) -> Option<u8> {
    if d.internal {
        return None;
    }
    let (cur, max) = ddc_read_code(d, 0x12)??;
    // A 0 on a read that claims a range is the firmware (the same quirk the luminance
    // read has on some panels), not a screen with no contrast at all: the level stays ours.
    (cur > 0).then(|| (cur * 100 / max).min(100) as u8)
}

/// One contrast write. True when the monitor took it.
pub fn write_contrast(d: &Display, percent: u8) -> bool {
    if d.internal {
        return false;
    }
    with_physical(d, |h| unsafe { SetVCPFeature(h, 0x12, percent.min(100) as u32) != 0 }).unwrap_or(false)
}

// ── WMI (built-in panel) ──────────────────────────────────────────────────────

struct Wmi(IWbemServices);

fn wmi() -> windows::core::Result<Wmi> {
    unsafe {
        // Already initialised on this thread is fine; a thread in another mode is left as it is.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let none = BSTR::new();
        let svc = locator.ConnectServer(&BSTR::from("ROOT\\WMI"), &none, &none, &none, 0, &none, None)?;
        impersonate(&svc)?;
        Ok(Wmi(svc))
    }
}

unsafe fn impersonate(proxy: &impl Interface) -> windows::core::Result<()> {
    unsafe {
        let raw = proxy.as_raw();
        let Some(proxy) = IUnknown::from_raw_borrowed(&raw) else { return Ok(()) };
        CoSetProxyBlanket(
            proxy,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
    }
}

impl Wmi {
    fn first(&self, query: &str) -> windows::core::Result<Option<windows::Win32::System::Wmi::IWbemClassObject>> {
        unsafe {
            let rows = self.0.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(query),
                WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_FORWARD_ONLY.0 | WBEM_FLAG_RETURN_IMMEDIATELY.0),
                None,
            )?;
            impersonate(&rows)?;
            let mut row = [None];
            let mut n = 0u32;
            // 3 s: a PC without the class answers at once with nothing.
            let _ = rows.Next(3000, &mut row, &mut n);
            Ok(row[0].take())
        }
    }
}

/// The built-in panel's level, 0–100. `None` on a PC without such a panel.
pub fn wmi_get() -> Option<u8> {
    let w = wmi().ok()?;
    unsafe {
        let row = w.first("SELECT CurrentBrightness FROM WmiMonitorBrightness WHERE Active=TRUE").ok()??;
        let mut v = VARIANT::default();
        row.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
        u32::try_from(&v).ok().map(|p| p.min(100) as u8)
    }
}

fn wmi_set(percent: u8) -> bool {
    (|| -> windows::core::Result<()> {
        let w = wmi()?;
        unsafe {
            let inst = w
                .first("SELECT * FROM WmiMonitorBrightnessMethods WHERE Active=TRUE")?
                .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let mut path = VARIANT::default();
            inst.Get(w!("__PATH"), 0, &mut path, None, None)?;
            let path = BSTR::try_from(&path)?;
            let mut class = None;
            w.0.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None)?;
            let class = class.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let mut sig = None;
            class.GetMethod(w!("WmiSetBrightness"), 0, &mut sig, &mut None)?;
            let args = sig.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?.SpawnInstance(0)?;
            args.Put(w!("Timeout"), 0, &VARIANT::from(1i32), 0)?;
            args.Put(w!("Brightness"), 0, &VARIANT::from(percent as i32), 0)?;
            w.0.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &args, None, None)
        }
    })()
    .is_ok()
}

// ── Together ──────────────────────────────────────────────────────────────────

/// How `d` can be dimmed in hardware and where its level stands now (0–100), by one read.
pub fn probe(d: &Display) -> (Method, Option<u8>) {
    if d.internal {
        return match wmi_get() {
            Some(p) => (Method::Wmi, Some(p)),
            None => (Method::None, None),
        };
    }
    match ddc_read(d) {
        Some(Some((cur, max))) => {
            DDC_MAX.lock().unwrap().get_or_insert_with(Default::default).insert(d.id.clone(), max);
            // A 0 on a read that claims a range is the firmware, not a black screen: some panels
            // (a Samsung LS24D300 here) answer 0 to luminance on every read while their other codes
            // reply correctly. `DdcWrite` already means "writes are tried, the level stays ours".
            if cur == 0 && max > 0 {
                (Method::DdcWrite, None)
            } else {
                (Method::Ddc, Some((cur * 100 / max).min(100) as u8))
            }
        }
        Some(None) => (Method::DdcWrite, None),
        None => (Method::None, None),
    }
}

/// One hardware write. True when the display took it.
pub fn write(d: &Display, method: Method, percent: u8) -> bool {
    match method {
        Method::Wmi => wmi_set(percent),
        Method::Ddc | Method::DdcWrite => ddc_write(d, percent as u32),
        Method::None => false,
    }
}
