// Audio — a shared service (docs/ARCHITECTURE.md §8). Not a feature: the
// features that need it list this crate in their own Cargo.toml. Refcounted:
// nothing here runs without a live subscriber, and the handle `subscribe`
// returns unsubscribes on drop.
//
// Core Audio endpoints: the level and mute of the default output and input,
// the device list, the default device. Reads and writes are plain calls on the
// caller's thread. `on_endpoint_change` is the part that runs: one MTA thread
// that owns the device enumerator and the volume callbacks, from the first
// `Sub` to the drop of the last. COM calls back on its own threads; those only
// push a message to that thread, which is where subscribers are called.

#![allow(non_snake_case)] // COM method names

use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{implement, interface, IUnknown, IUnknown_Vtbl, GUID, HRESULT, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{E_FAIL, PROPERTYKEY};
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, PKEY_AudioEndpoint_FormFactor,
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

type WinResult<T> = windows::core::Result<T>;

mod sessions;
pub use sessions::{
    capture_in_use, on_sessions, process_path, sessions, set_session_mute, set_session_volume, Capture, Scope, Session,
    SessionSub,
};

mod loopback;
pub use loopback::{loopback, Loopback};

// ── The model ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Output: speakers, headphones.
    Render,
    /// Input: microphones.
    Capture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Console,
    Multimedia,
    Communications,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume {
    /// 0…1, what the Windows slider shows.
    pub scalar: f32,
    pub mute: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// `IMMDevice::GetId`: the stable key for anything stored about a device.
    pub id: String,
    pub name: String,
    /// `EndpointFormFactor`: 1 speakers, 3 headphones, 4 microphone, 5 headset,
    /// 8 S/PDIF, 9 display audio, 10 unknown.
    pub form_factor: u32,
    /// The roles it is the default device for; empty for most.
    pub default_for: Vec<Role>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// The level or mute of a default endpoint moved: the volume keys, a
    /// headset button, the Windows slider, another app. `own` = this process
    /// did it through `set_volume` or `set_mute`.
    Volume { flow: Flow, volume: Volume, own: bool },
    /// A device came or went, or a default changed (one call per burst). Read
    /// `devices` and `endpoint_volume` again.
    Devices,
}

impl Flow {
    fn raw(self) -> EDataFlow {
        if self == Flow::Render { eRender } else { eCapture }
    }
}

impl Role {
    fn raw(self) -> ERole {
        match self {
            Role::Console => eConsole,
            Role::Multimedia => eMultimedia,
            Role::Communications => eCommunications,
        }
    }
}

// ── Reads and writes (on the caller's thread) ─────────────────────────────────

/// Level and mute of the default device of `flow`. An error when there is none.
pub fn endpoint_volume(flow: Flow) -> Result<Volume, String> {
    let _com = Com::enter();
    (|| unsafe {
        let endpoint = default_endpoint(&enumerator()?, flow)?;
        Ok(Volume { scalar: endpoint.GetMasterVolumeLevelScalar()?, mute: endpoint.GetMute()?.as_bool() })
    })()
    .map_err(text)
}

/// Below this a level reads 0 % on the slider: an output is muted with it.
const SILENT: f32 = 0.005;

/// Output writes asked for and not yet applied, newest per device.
#[derive(Default)]
struct Writes {
    busy: bool,
    queue: Vec<(String, f32)>,
}

impl Writes {
    /// Queues the write and says whether this caller has to apply the queue: the
    /// caller that finds it idle owns it until it is empty again.
    fn push(&mut self, id: String, scalar: f32) -> bool {
        self.queue.retain(|q| q.0 != id);
        self.queue.push((id, scalar));
        !std::mem::replace(&mut self.busy, true)
    }

    /// The next write to apply, or `None` once the queue is empty: the caller that
    /// gets it hands the queue back, so the next write is applied by its own caller.
    fn next(&mut self) -> Option<(String, f32)> {
        if self.queue.is_empty() {
            self.busy = false;
            return None;
        }
        Some(self.queue.remove(0))
    }
}

static WRITES: Mutex<Writes> = Mutex::new(Writes { busy: false, queue: Vec::new() });

/// Hands the queue back even if the call that drains it unwinds: a `busy` flag left
/// set would queue every later output write and never apply one.
struct Owner;

impl Drop for Owner {
    fn drop(&mut self) {
        if let Ok(mut w) = WRITES.lock() {
            w.busy = false;
        }
    }
}

/// Sets the level (0…1) of the default device of `flow`. For an output, a level
/// that reads 0 % also mutes it and a higher one opens it; the write goes to the
/// device that is the default now, even if another is by the time it is applied;
/// and writes that come faster than they are applied collapse to the newest.
pub fn set_volume(flow: Flow, scalar: f32) -> Result<(), String> {
    let _com = Com::enter();
    let scalar = scalar.clamp(0.0, 1.0);
    if flow == Flow::Capture {
        return (|| unsafe { default_endpoint(&enumerator()?, flow)?.SetMasterVolumeLevelScalar(scalar, &OWN) })().map_err(text);
    }
    let id = (|| unsafe { device_id(&enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?) })().map_err(text)?;
    let mine = scalar;
    // Another call is applying writes: it will take this one too, and its outcome is
    // not this caller's to report.
    if !WRITES.lock().unwrap().push(id.clone(), scalar) {
        return Ok(());
    }
    let _owner = Owner;
    // Only this device's own refusal is this caller's news: the rest of the queue is
    // other callers' work, and a device that refuses does not stop the others.
    let mut refused: Option<String> = None;
    while let Some((next, scalar)) = WRITES.lock().unwrap().next() {
        if let Err(err) = apply_level(&next, scalar) {
            if next == id && scalar == mine {
                refused = Some(err);
            }
        }
    }
    refused.map_or(Ok(()), Err)
}

/// Mute first when going silent, open last when coming back: never a blip.
fn apply_level(id: &str, scalar: f32) -> Result<(), String> {
    (|| unsafe {
        let endpoint = endpoint_of(&enumerator()?, id)?;
        let (silent, muted) = (scalar < SILENT, endpoint.GetMute()?.as_bool());
        if silent && !muted {
            endpoint.SetMute(true, &OWN)?;
        }
        endpoint.SetMasterVolumeLevelScalar(scalar, &OWN)?;
        if !silent && muted {
            endpoint.SetMute(false, &OWN)?;
        }
        Ok(())
    })()
    .map_err(text)
}

pub fn set_mute(flow: Flow, mute: bool) -> Result<(), String> {
    let _com = Com::enter();
    (|| unsafe { default_endpoint(&enumerator()?, flow)?.SetMute(mute, &OWN) })().map_err(text)
}

/// Level and mute of the device `id` (from `devices`), default or not.
pub fn device_volume(id: &str) -> Result<Volume, String> {
    let _com = Com::enter();
    (|| unsafe {
        let endpoint = endpoint_of(&enumerator()?, id)?;
        Ok(Volume { scalar: endpoint.GetMasterVolumeLevelScalar()?, mute: endpoint.GetMute()?.as_bool() })
    })()
    .map_err(text)
}

/// Sets the level (0…1) of the device `id`.
pub fn set_device_volume(id: &str, scalar: f32) -> Result<(), String> {
    let _com = Com::enter();
    (|| unsafe { endpoint_of(&enumerator()?, id)?.SetMasterVolumeLevelScalar(scalar.clamp(0.0, 1.0), &OWN) })()
        .map_err(text)
}

/// Mutes or unmutes the device `id`.
pub fn set_device_mute(id: &str, mute: bool) -> Result<(), String> {
    let _com = Com::enter();
    (|| unsafe { endpoint_of(&enumerator()?, id)?.SetMute(mute, &OWN) })().map_err(text)
}

/// The active devices of `flow`: the defaults first, then by name, then by id.
pub fn devices(flow: Flow) -> Result<Vec<Device>, String> {
    let _com = Com::enter();
    (|| unsafe {
        let enumerator = enumerator()?;
        let defaults = [Role::Console, Role::Multimedia, Role::Communications].map(|role| {
            let id = enumerator.GetDefaultAudioEndpoint(flow.raw(), role.raw()).and_then(|d| device_id(&d));
            (role, id.unwrap_or_default())
        });
        let list = enumerator.EnumAudioEndpoints(flow.raw(), DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..list.GetCount()? {
            let device = list.Item(i)?;
            let id = device_id(&device)?;
            let props = device.OpenPropertyStore(STGM_READ)?;
            let name = props.GetValue(&PKEY_Device_FriendlyName).map(|v| v.to_string()).unwrap_or_default();
            let form_factor = props.GetValue(&PKEY_AudioEndpoint_FormFactor).ok().and_then(|v| u32::try_from(&v).ok());
            out.push(Device {
                default_for: defaults.iter().filter(|d| d.1 == id).map(|d| d.0).collect(),
                id,
                name,
                form_factor: form_factor.unwrap_or(10),
            });
        }
        out.sort_by(|a, b| (a.default_for.is_empty(), &a.name, &a.id).cmp(&(b.default_for.is_empty(), &b.name, &b.id)));
        Ok(out)
    })()
    .map_err(text)
}

/// Whether `set_default` can work on this build of Windows. The interface
/// behind it is undocumented: hide the picker when this is false.
pub fn can_set_default() -> bool {
    let _com = Com::enter();
    policy_config().is_ok()
}

/// Makes the device `id` (from `devices`) the default for `role`.
pub fn set_default(id: &str, role: Role) -> Result<(), String> {
    let _com = Com::enter();
    let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
    (|| unsafe { policy_config()?.SetDefaultEndpoint(PCWSTR(wide.as_ptr()), role.raw()).ok() })().map_err(text)
}

/// The event context of our own writes, so that `Change::Volume` can say `own`.
const OWN: GUID = GUID::from_u128(0x6e6f7463_682d_4a75_8d69_6f2d766f6c00);

/// COM on this thread for the length of one call. Declared first, so that it
/// is dropped after every interface the call made.
struct Com(bool);

impl Com {
    fn enter() -> Self {
        // S_FALSE (already entered) has to be left too; a refusal (the thread
        // is an STA) does not, and these interfaces work from either.
        Self(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn text(err: windows::core::Error) -> String {
    err.message()
}

fn enumerator() -> WinResult<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// The console default: the device the volume keys move.
fn default_endpoint(enumerator: &IMMDeviceEnumerator, flow: Flow) -> WinResult<IAudioEndpointVolume> {
    unsafe { enumerator.GetDefaultAudioEndpoint(flow.raw(), eConsole)?.Activate(CLSCTX_ALL, None) }
}

fn endpoint_of(enumerator: &IMMDeviceEnumerator, id: &str) -> WinResult<IAudioEndpointVolume> {
    let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
    unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr()))?.Activate(CLSCTX_ALL, None) }
}

fn device_id(device: &IMMDevice) -> WinResult<String> {
    unsafe {
        let raw = device.GetId()?;
        let id = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const _));
        // An id that is empty or unreadable addresses nothing: say so here rather
        // than three calls later, where the device looks merely absent.
        let id = id?;
        if id.is_empty() {
            return Err(windows::core::Error::new(E_FAIL, "the device has no id"));
        }
        Ok(id)
    }
}

// `IPolicyConfig`, the interface the Sound control panel sets the default
// device with. Undocumented but unchanged since Windows 7; `can_set_default`
// is the probe. Ten methods come before the one used: they are never called,
// so only their slots are declared.
#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    fn GetMixFormat(&self) -> HRESULT;
    fn GetDeviceFormat(&self) -> HRESULT;
    fn ResetDeviceFormat(&self) -> HRESULT;
    fn SetDeviceFormat(&self) -> HRESULT;
    fn GetProcessingPeriod(&self) -> HRESULT;
    fn SetProcessingPeriod(&self) -> HRESULT;
    fn GetShareMode(&self) -> HRESULT;
    fn SetShareMode(&self) -> HRESULT;
    fn GetPropertyValue(&self) -> HRESULT;
    fn SetPropertyValue(&self) -> HRESULT;
    fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
}

const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

fn policy_config() -> WinResult<IPolicyConfig> {
    unsafe { CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }
}

// ── Subscribing ───────────────────────────────────────────────────────────────

/// A live subscription. Dropping it unsubscribes; dropping the last one ends
/// the audio thread and unregisters every callback.
#[must_use = "dropping the handle unsubscribes"]
pub struct Sub(u64);

/// Called on the audio thread for every change of the default endpoints and of
/// the device list. Keep it short.
pub fn on_endpoint_change(f: impl Fn(&Change) + Send + Sync + 'static) -> Sub {
    let mut hub = HUB.lock().unwrap();
    hub.next += 1;
    let id = hub.next;
    hub.subs.push((id, Arc::new(f)));
    if needs_thread(&hub.thread) {
        // Nothing was started yet, or the thread that was there has ended: without
        // this the hub would be quiet for the rest of the run, with subscribers that
        // look healthy.
        let _ = hub.thread.take();
        let (tx, rx) = mpsc::channel();
        let callbacks = tx.clone();
        hub.thread = Some((tx, std::thread::spawn(move || run(rx, callbacks))));
    }
    Sub(id)
}

impl Drop for Sub {
    fn drop(&mut self) {
        let ended = {
            let mut hub = HUB.lock().unwrap();
            hub.subs.retain(|s| s.0 != self.0);
            if !hub.subs.is_empty() {
                return;
            }
            let Some(thread) = hub.thread.as_mut() else { return };
            let _ = thread.0.send(Msg::Stop);
            // Dropped from inside a callback: this thread ends on its own, and its
            // handle has to stay, or the next subscriber would start a second thread
            // beside one that has not stopped yet.
            if thread.1.thread().id() == std::thread::current().id() {
                return;
            }
            hub.thread.take()
        };
        let Some((_, handle)) = ended else { return };
        // Outside the lock: a join waits for a callback, which may want this hub.
        let _ = handle.join();
    }
}

type Handler = Arc<dyn Fn(&Change) + Send + Sync>;

struct Hub {
    next: u64,
    subs: Vec<(u64, Handler)>,
    thread: Option<(mpsc::Sender<Msg>, JoinHandle<()>)>,
}

static HUB: Mutex<Hub> = Mutex::new(Hub { next: 0, subs: Vec::new(), thread: None });

fn emit(change: Change) {
    // Copied out first: a callback may subscribe or drop a `Sub`.
    let subs: Vec<Handler> = HUB.lock().unwrap().subs.iter().map(|s| s.1.clone()).collect();
    for f in subs {
        guard(|| f(&change));
    }
}

/// Runs one subscriber so that a panic in it cannot take the hub thread with it.
pub(crate) fn guard(f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        // debt: this crate has no logger (notch-core would pull the app in), so a
        // subscriber that throws is only visible as a HUD that stopped moving.
        eprintln!("lintel: an audio subscriber panicked");
    }
}

/// Whether the hub needs a thread: one was never started, or the one there ended.
pub(crate) fn needs_thread<T>(thread: &Option<(mpsc::Sender<T>, JoinHandle<()>)>) -> bool {
    thread.as_ref().is_none_or(|(_, handle)| handle.is_finished())
}

// ── The audio thread ──────────────────────────────────────────────────────────

enum Msg {
    Volume(Flow, Volume, bool),
    Devices,
    Stop,
}

/// A device switch calls back once per role and per flow: one pass after the burst.
const SETTLE: Duration = Duration::from_millis(200);

type Bound = Option<(IAudioEndpointVolume, IAudioEndpointVolumeCallback)>;

fn run(rx: mpsc::Receiver<Msg>, tx: mpsc::Sender<Msg>) {
    let _com = Com::enter();
    let Ok(enumerator) = enumerator() else { return };
    let devices: IMMNotificationClient = DeviceEvents(tx.clone()).into();
    unsafe {
        // debt: a refusal here means no device changes ever, and this crate has no logger
        // to say so; the hub's restart path is the only recovery.
        let _ = enumerator.RegisterEndpointNotificationCallback(&devices);
    }
    let bind_all = || [Flow::Render, Flow::Capture].map(|flow| bind(&enumerator, flow, &tx));
    let mut bound = bind_all();
    let mut rebind_at: Option<Instant> = None;
    loop {
        let msg = match rebind_at {
            None => rx.recv().ok(),
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(msg) => Some(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    rebind_at = None;
                    unbind(bound);
                    bound = bind_all();
                    emit(Change::Devices);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            },
        };
        match msg {
            Some(Msg::Volume(flow, volume, own)) => emit(Change::Volume { flow, volume, own }),
            Some(Msg::Devices) => {
                rebind_at.get_or_insert(Instant::now() + SETTLE);
            }
            Some(Msg::Stop) | None => break,
        }
    }
    unbind(bound);
    unsafe {
        let _ = enumerator.UnregisterEndpointNotificationCallback(&devices);
    }
}

/// Listens to the default device of `flow`. None when there is no such device.
fn bind(enumerator: &IMMDeviceEnumerator, flow: Flow, tx: &mpsc::Sender<Msg>) -> Bound {
    let endpoint = default_endpoint(enumerator, flow).ok()?;
    let callback: IAudioEndpointVolumeCallback = VolumeEvents { flow, tx: tx.clone() }.into();
    // debt: a refusal here means this flow's levels are never told, with nothing said about it.
    unsafe { endpoint.RegisterControlChangeNotify(&callback).ok()? };
    Some((endpoint, callback))
}

fn unbind(bound: [Bound; 2]) {
    for (endpoint, callback) in bound.into_iter().flatten() {
        unsafe {
            let _ = endpoint.UnregisterControlChangeNotify(&callback);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_queue_collapses_to_the_newest_write_per_device() {
        let mut w = Writes::default();
        assert!(w.push("a".into(), 0.2));
        assert!(!w.push("a".into(), 0.9));
        assert_eq!(w.next(), Some(("a".into(), 0.9)));
        assert_eq!(w.next(), None);
    }

    #[test]
    fn the_caller_that_finds_the_queue_idle_owns_it_and_hands_it_back() {
        let mut w = Writes::default();
        assert!(w.push("a".into(), 0.5), "the first write is applied by its caller");
        assert!(!w.push("b".into(), 0.7), "the second waits for the first caller");
        assert_eq!(w.next(), Some(("a".into(), 0.5)));
        assert_eq!(w.next(), Some(("b".into(), 0.7)));
        assert_eq!(w.next(), None);
        assert!(!w.busy, "the queue is free for the next caller");
        assert!(w.push("c".into(), 1.0), "and is taken by it");
    }

    #[test]
    fn a_panicking_subscriber_does_not_stop_the_hub() {
        let was = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        guard(|| panic!("a subscriber that throws"));
        std::panic::set_hook(was);
        let mut after = false;
        guard(|| after = true);
        assert!(after, "the next subscriber still runs");
    }

    #[test]
    fn a_ended_thread_is_replaced_and_a_live_one_is_kept() {
        let none: Option<(mpsc::Sender<Msg>, JoinHandle<()>)> = None;
        assert!(needs_thread(&none), "a hub that never started needs a thread");

        let live = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(50)));
        let (tx, _rx) = mpsc::channel::<Msg>();
        assert!(!needs_thread(&Some((tx, live))), "a running thread is kept");

        // A thread that has returned, without joining it: joining consumes the handle.
        let ended = std::thread::spawn(|| {});
        while !ended.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let (tx, _rx) = mpsc::channel::<Msg>();
        assert!(needs_thread(&Some((tx, ended))), "a thread that returned is replaced");
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeEvents {
    flow: Flow,
    tx: mpsc::Sender<Msg>,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeEvents_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> WinResult<()> {
        if let Some(data) = unsafe { data.as_ref() } {
            let volume = Volume { scalar: data.fMasterVolume, mute: data.bMuted.as_bool() };
            let _ = self.tx.send(Msg::Volume(self.flow, volume, data.guidEventContext == OWN));
        }
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct DeviceEvents(mpsc::Sender<Msg>);

impl DeviceEvents_Impl {
    fn changed(&self) -> WinResult<()> {
        let _ = self.0.send(Msg::Devices);
        Ok(())
    }
}

impl IMMNotificationClient_Impl for DeviceEvents_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> WinResult<()> {
        self.changed()
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> WinResult<()> {
        self.changed()
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> WinResult<()> {
        self.changed()
    }
    fn OnDefaultDeviceChanged(&self, _flow: EDataFlow, _role: ERole, _id: &PCWSTR) -> WinResult<()> {
        self.changed()
    }
    // Fires for every property of every device, many times a second while one is set up.
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> WinResult<()> {
        Ok(())
    }
}
