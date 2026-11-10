// Audio sessions (docs/ARCHITECTURE.md §8): what each app plays on an output
// (and, for huds-power, records on a microphone), with a level and a mute of its
// own — the rows of the Windows volume mixer.
// Reads and writes are plain calls on the caller's thread. `on_sessions` is the
// part that runs: one MTA thread of its own, from the first `SessionSub` to the
// drop of the last, holding a session manager per active endpoint it was asked
// to follow (outputs always; microphones when a subscriber wants them) and a
// listener per session. COM calls back on its own threads; those only push a
// message to that thread, which is where subscribers are called.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Ref, BOOL, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, PROPERTYKEY, S_OK};
use windows::Win32::Media::Audio::{
    eCapture, eRender, AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateActive, AudioSessionStateExpired,
    EDataFlow, ERole, IAudioSessionControl, IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionEvents,
    IAudioSessionEvents_Impl, IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl,
    IMMDevice, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, ISimpleAudioVolume,
    DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::{device_id, enumerator, text, Com, WinResult, OWN};

// ── The model ─────────────────────────────────────────────────────────────────

/// One stream of sound from a process to an output. An app may hold several:
/// one per output it plays on, and one per helper process.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The process that plays; 0 for the system sounds.
    pub pid: u32,
    /// Its executable, as a full path. Empty for the system sounds and for a
    /// process that may not be asked.
    pub exe: String,
    /// The name the app gave its session. Most give none.
    pub name: String,
    /// The icon the app gave its session (a path, or `file,-resource`). Most give none.
    pub icon: String,
    /// 0…1, the app's own level under the output's.
    pub volume: f32,
    pub mute: bool,
    /// Sound is coming out of it right now.
    pub active: bool,
    /// The session of Windows' own sounds.
    pub system: bool,
    /// The output it plays on (`Device::id`).
    pub device: String,
}

// ── Reads and writes (on the caller's thread) ─────────────────────────────────

/// The sessions on the output `device` (a `Device::id`), or with `None` on
/// every active output: an app can be sent to another output than the default.
/// Sessions whose process has ended are left out.
pub fn sessions(device: Option<&str>) -> Result<Vec<Session>, String> {
    let _com = Com::enter();
    (|| {
        let mut out = Vec::new();
        for (id, device) in outputs(&enumerator()?, eRender, device)? {
            // An output on its way out answers nothing: it has no sessions.
            let Ok((_, controls)) = manager(&device).and_then(|m| controls(&m)) else { continue };
            out.extend(controls.iter().filter_map(|control| read(control, &id)));
        }
        Ok(out)
    })()
    .map_err(text)
}

/// The executables of the processes with a capture stream running on any active
/// microphone right now, sorted and without repeats: the live state behind the
/// Windows microphone icon, and what huds-power's privacy indicator follows.
/// (The ConsentStore registry is usage history, not this: it is not written when
/// a process holding the microphone is killed, so a reading from it can never
/// clear again.)
pub fn capture_in_use() -> Result<Vec<Capture>, String> {
    let _com = Com::enter();
    (|| {
        let mut out: Vec<Capture> = Vec::new();
        for (_, device) in outputs(&enumerator()?, eCapture, None)? {
            let Ok((_, controls)) = manager(&device).and_then(|m| controls(&m)) else { continue };
            for control in &controls {
                // A stream that is merely open is not in use; one that is running is.
                if unsafe { control.GetState() }.ok() != Some(AudioSessionStateActive) {
                    continue;
                }
                let Some((pid, _)) = whose(control) else { continue };
                if pid == 0 {
                    continue;
                }
                out.push(Capture { pid, exe: process_path(pid) });
            }
        }
        Ok(coalesce(out))
    })()
    .map_err(text)
}

/// One process with a capture stream running on an active microphone right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// The process holding the microphone.
    pub pid: u32,
    /// Its executable as a full path, or `None` for a process this one may not ask:
    /// an elevated or protected one. That process is still reported, because a
    /// privacy indicator that hides the apps it cannot open hides the wrong ones.
    pub exe: Option<String>,
}

/// One row per process: an app recording on two microphones is still one app.
fn coalesce(mut list: Vec<Capture>) -> Vec<Capture> {
    list.sort_by(|a, b| a.pid.cmp(&b.pid).then_with(|| a.exe.cmp(&b.exe)));
    list.dedup_by(|a, b| a.pid == b.pid);
    list
}

/// Sets the level (0…1) of every session of the processes `pids`, on every
/// output. Process 0 is the system sounds.
pub fn set_session_volume(pids: &[u32], scalar: f32) -> Result<(), String> {
    write(pids, |level| unsafe { level.SetMasterVolume(scalar.clamp(0.0, 1.0), &OWN) })
}

/// Mutes or unmutes every session of the processes `pids`; the level is kept.
pub fn set_session_mute(pids: &[u32], mute: bool) -> Result<(), String> {
    write(pids, |level| unsafe { level.SetMute(mute, &OWN) })
}

/// The executable of a running process as a full path. None for a process that
/// has ended or may not be asked: a protected one, or an elevated one when Lintel
/// is not — `capture_in_use` reports those by pid instead.
pub fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        // A path too long for the buffer is not a failure to report: the call says
        // how it failed, so the buffer doubles until it fits.
        let mut buf = vec![0u16; 512];
        let path = loop {
            let mut len = buf.len() as u32;
            let read = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
            if read.is_ok() {
                break Some(String::from_utf16_lossy(&buf[..len as usize]));
            }
            if GetLastError() != ERROR_INSUFFICIENT_BUFFER || buf.len() >= 32 * 1024 {
                break None;
            }
            buf.resize(buf.len() * 2, 0);
        };
        let _ = CloseHandle(process);
        path
    }
}

fn write(pids: &[u32], set: impl Fn(&ISimpleAudioVolume) -> WinResult<()>) -> Result<(), String> {
    let _com = Com::enter();
    (|| {
        let mut done = Ok(());
        for (_, device) in outputs(&enumerator()?, eRender, None)? {
            let Ok((_, controls)) = manager(&device).and_then(|m| controls(&m)) else { continue };
            for control in &controls {
                if !whose(control).is_some_and(|(pid, _)| pids.contains(&pid)) {
                    continue;
                }
                // A session that refuses does not keep the others from being set.
                if let Err(err) = control.cast::<ISimpleAudioVolume>().and_then(|level| set(&level)) {
                    done = Err(err);
                }
            }
        }
        done
    })()
    .map_err(text)
}

/// The active endpoints of `flow`, with their ids; `only` keeps the one with that id.
fn outputs(enumerator: &IMMDeviceEnumerator, flow: EDataFlow, only: Option<&str>) -> WinResult<Vec<(String, IMMDevice)>> {
    unsafe {
        let list = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..list.GetCount()? {
            let device = list.Item(i)?;
            let id = device_id(&device)?;
            if only.is_none_or(|want| want == id) {
                out.push((id, device));
            }
        }
        Ok(out)
    }
}

fn manager(device: &IMMDevice) -> WinResult<IAudioSessionManager2> {
    unsafe { device.Activate(CLSCTX_ALL, None) }
}

/// Every session of an output. Counting them is also what makes Windows start
/// telling the manager's listener about new ones.
fn controls(manager: &IAudioSessionManager2) -> WinResult<(IAudioSessionEnumerator, Vec<IAudioSessionControl>)> {
    unsafe {
        let list = manager.GetSessionEnumerator()?;
        let controls = (0..list.GetCount()?).filter_map(|i| list.GetSession(i).ok()).collect();
        Ok((list, controls))
    }
}

/// The process of a session, and whether it is the system sounds (process 0).
fn whose(control: &IAudioSessionControl) -> Option<(u32, bool)> {
    unsafe {
        let more: IAudioSessionControl2 = control.cast().ok()?;
        if more.IsSystemSoundsSession() == S_OK {
            return Some((0, true));
        }
        more.GetProcessId().ok().filter(|pid| *pid != 0).map(|pid| (pid, false))
    }
}

fn read(control: &IAudioSessionControl, device: &str) -> Option<Session> {
    unsafe {
        let state = control.GetState().ok()?;
        if state == AudioSessionStateExpired {
            return None;
        }
        let (pid, system) = whose(control)?;
        let level: ISimpleAudioVolume = control.cast().ok()?;
        Some(Session {
            pid,
            exe: if system { String::new() } else { process_path(pid).unwrap_or_default() },
            name: taken(control.GetDisplayName()),
            icon: taken(control.GetIconPath()),
            volume: level.GetMasterVolume().ok()?,
            mute: level.GetMute().ok()?.as_bool(),
            active: state == AudioSessionStateActive,
            system,
            device: device.to_owned(),
        })
    }
}

/// A string COM allocated for us: copied, then freed.
unsafe fn taken(raw: WinResult<PWSTR>) -> String {
    match raw {
        Ok(raw) if !raw.is_null() => unsafe {
            let text = raw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(raw.0 as *const _));
            text
        },
        _ => String::new(),
    }
}

// ── Subscribing ───────────────────────────────────────────────────────────────

/// What a subscriber wants to hear about. The mixer shows the outputs only, so a
/// microphone session that starts (a dictation, a call) must not wake it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The outputs: what each app plays.
    Render,
    /// The outputs and the microphones: also what each app records.
    All,
}

/// A live subscription to the sessions. Dropping it unsubscribes; dropping the
/// last one ends the sessions thread and unregisters every listener.
#[must_use = "dropping the handle unsubscribes"]
pub struct SessionSub(u64);

/// Called on the sessions thread when a session came, went, started or stopped
/// playing, or had its level or mute changed by someone else (our own writes
/// are not told): read `sessions` again. At most once per 200 ms. Keep it short:
/// a panic in it is contained, not propagated.
pub fn on_sessions(scope: Scope, f: impl Fn() + Send + Sync + 'static) -> SessionSub {
    let mut hub = HUB.lock().unwrap();
    hub.next += 1;
    let id = hub.next;
    hub.subs.push((id, scope, Arc::new(f)));
    WANT_CAPTURE.store(wants_capture(&hub.subs), Relaxed);
    if crate::needs_thread(&hub.thread) {
        // Nothing was started yet, or the thread that was there has ended: without
        // this the hub would be quiet for the rest of the run.
        let _ = hub.thread.take();
        let (tx, rx) = mpsc::channel();
        let callbacks = tx.clone();
        hub.thread = Some((tx, std::thread::spawn(move || run(rx, callbacks))));
    }
    SessionSub(id)
}

/// Whether any subscriber wants the microphones too. They are listened to only
/// then: the listeners are COM objects per endpoint and per session.
fn wants_capture(subs: &[(u64, Scope, Handler)]) -> bool {
    subs.iter().any(|s| s.1 == Scope::All)
}

impl Drop for SessionSub {
    fn drop(&mut self) {
        let ended = {
            let mut hub = HUB.lock().unwrap();
            hub.subs.retain(|s| s.0 != self.0);
            WANT_CAPTURE.store(wants_capture(&hub.subs), Relaxed);
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

type Handler = Arc<dyn Fn() + Send + Sync>;

struct Hub {
    next: u64,
    subs: Vec<(u64, Scope, Handler)>,
    thread: Option<(mpsc::Sender<Msg>, JoinHandle<()>)>,
}

static HUB: Mutex<Hub> = Mutex::new(Hub { next: 0, subs: Vec::new(), thread: None });

/// Whether any live subscriber asked for the microphones too; read by the thread
/// when it (re)binds.
static WANT_CAPTURE: AtomicBool = AtomicBool::new(false);

fn emit() {
    // Copied out first: a callback may subscribe or drop a `SessionSub`.
    let subs: Vec<Handler> = HUB.lock().unwrap().subs.iter().map(|s| s.2.clone()).collect();
    for f in subs {
        crate::guard(move || f());
    }
}

// ── The sessions thread ───────────────────────────────────────────────────────

enum Msg {
    /// What a session says changed: read again.
    Changed,
    /// The set of sessions or of outputs changed: listen again, then read again.
    Rebind,
    Stop,
}

/// A fader dragged in another mixer, or an app that starts, calls back many
/// times: subscribers are told once per window.
const SETTLE: Duration = Duration::from_millis(200);

/// What listens on one output. Kept alive for as long as it listens.
struct Bound {
    manager: IAudioSessionManager2,
    created: IAudioSessionNotification,
    _list: IAudioSessionEnumerator,
    sessions: Vec<(IAudioSessionControl, IAudioSessionEvents)>,
}

fn run(rx: mpsc::Receiver<Msg>, tx: mpsc::Sender<Msg>) {
    let _com = Com::enter();
    let Ok(enumerator) = enumerator() else { return };
    let devices: IMMNotificationClient = Events(tx.clone()).into();
    unsafe {
        let _ = enumerator.RegisterEndpointNotificationCallback(&devices);
    }
    let mut bound = bind(&enumerator, &tx, WANT_CAPTURE.load(Relaxed));
    'life: while let Ok(first) = rx.recv() {
        let until = Instant::now() + SETTLE;
        let mut rebind = false;
        let mut msg = first;
        loop {
            match msg {
                Msg::Stop => break 'life,
                Msg::Rebind => rebind = true,
                Msg::Changed => {}
            }
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(next) => msg = next,
                Err(_) => break,
            }
        }
        if rebind {
            unbind(std::mem::take(&mut bound));
            bound = bind(&enumerator, &tx, WANT_CAPTURE.load(Relaxed));
        }
        emit();
    }
    unbind(bound);
    unsafe {
        let _ = enumerator.UnregisterEndpointNotificationCallback(&devices);
    }
}

/// Listens for new sessions on every endpoint the subscribers asked for, and to
/// every session on it.
fn bind(enumerator: &IMMDeviceEnumerator, tx: &mpsc::Sender<Msg>, capture: bool) -> Vec<Bound> {
    let mut out = Vec::new();
    // Every output, and every microphone when a subscriber asked for them: an app
    // recording is a session on a capture endpoint, and it is that half which tells
    // huds-power who holds the microphone. The mixer reads the outputs only.
    let flows: &[EDataFlow] = if capture { &[eRender, eCapture] } else { &[eRender] };
    let both = flows
        .iter()
        .flat_map(|flow| outputs(enumerator, *flow, None).unwrap_or_default())
        .collect::<Vec<_>>();
    for (_, device) in both {
        let Ok(manager) = manager(&device) else { continue };
        let created: IAudioSessionNotification = Events(tx.clone()).into();
        unsafe {
            // Registered before the sessions are counted, as Windows wants it.
            // debt: a refusal here means new sessions are never told; no logger here to say it.
            let _ = manager.RegisterSessionNotification(&created);
            let Ok((list, controls)) = controls(&manager) else {
                let _ = manager.UnregisterSessionNotification(&created);
                continue;
            };
            let sessions = controls
                .into_iter()
                .filter_map(|control| {
                    let events: IAudioSessionEvents = Events(tx.clone()).into();
                    control.RegisterAudioSessionNotification(&events).ok()?;
                    Some((control, events))
                })
                .collect();
            out.push(Bound { manager, created, _list: list, sessions });
        }
    }
    out
}

fn unbind(bound: Vec<Bound>) {
    for output in bound {
        unsafe {
            for (control, events) in &output.sessions {
                let _ = control.UnregisterAudioSessionNotification(events);
            }
            let _ = output.manager.UnregisterSessionNotification(&output.created);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_row_per_process() {
        let list = vec![
            Capture { pid: 9, exe: Some("b.exe".into()) },
            Capture { pid: 4, exe: None },
            Capture { pid: 9, exe: Some("b.exe".into()) },
        ];
        assert_eq!(
            coalesce(list),
            vec![Capture { pid: 4, exe: None }, Capture { pid: 9, exe: Some("b.exe".into()) }]
        );
    }

    #[test]
    fn the_current_process_has_a_path() {
        let path = process_path(std::process::id()).expect("a process can be asked about itself");
        assert!(path.to_ascii_lowercase().ends_with(".exe"), "{path}");
    }

    #[test]
    fn only_a_subscriber_that_asked_gets_the_microphones() {
        let handler: Handler = Arc::new(|| {});
        let render = [(1, Scope::Render, handler.clone())];
        assert!(!wants_capture(&render), "the mixer alone must not open capture endpoints");
        let both = [(1, Scope::Render, handler.clone()), (2, Scope::All, handler)];
        assert!(wants_capture(&both));
    }
}

/// Every callback of the sessions thread: each only says that something changed.
#[implement(IAudioSessionNotification, IAudioSessionEvents, IMMNotificationClient)]
struct Events(mpsc::Sender<Msg>);

impl Events_Impl {
    fn say(&self, msg: Msg) -> WinResult<()> {
        let _ = self.0.send(msg);
        Ok(())
    }
}

impl IAudioSessionNotification_Impl for Events_Impl {
    fn OnSessionCreated(&self, _session: Ref<'_, IAudioSessionControl>) -> WinResult<()> {
        self.say(Msg::Rebind)
    }
}

impl IAudioSessionEvents_Impl for Events_Impl {
    fn OnDisplayNameChanged(&self, _name: &PCWSTR, _context: *const GUID) -> WinResult<()> {
        self.say(Msg::Changed)
    }
    fn OnIconPathChanged(&self, _path: &PCWSTR, _context: *const GUID) -> WinResult<()> {
        self.say(Msg::Changed)
    }
    fn OnSimpleVolumeChanged(&self, _volume: f32, _mute: BOOL, context: *const GUID) -> WinResult<()> {
        // Our own write: whoever wrote it knows.
        if unsafe { context.as_ref() } == Some(&OWN) {
            return Ok(());
        }
        self.say(Msg::Changed)
    }
    fn OnChannelVolumeChanged(&self, _count: u32, _volumes: *const f32, _changed: u32, _context: *const GUID) -> WinResult<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _grouping: *const GUID, _context: *const GUID) -> WinResult<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _state: AudioSessionState) -> WinResult<()> {
        self.say(Msg::Changed)
    }
    fn OnSessionDisconnected(&self, _reason: AudioSessionDisconnectReason) -> WinResult<()> {
        self.say(Msg::Rebind)
    }
}

impl IMMNotificationClient_Impl for Events_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> WinResult<()> {
        self.say(Msg::Rebind)
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> WinResult<()> {
        self.say(Msg::Rebind)
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> WinResult<()> {
        self.say(Msg::Rebind)
    }
    // A new default moves no session and adds no output.
    fn OnDefaultDeviceChanged(&self, _flow: EDataFlow, _role: ERole, _id: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    // Fires for every property of every device, many times a second while one is set up.
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> WinResult<()> {
        Ok(())
    }
}
