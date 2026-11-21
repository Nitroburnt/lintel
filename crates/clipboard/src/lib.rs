// Clipboard — what was copied, kept on this machine and brought back with a
// click: history, the auto clear, a plain-text paste and the cleaning of
// tracking links. Everything that is copied is recorded.
//
// The contract is docs/ARCHITECTURE.md §2: nothing runs before `start`,
// everything is gone after `stop`, every command arrives through `call`. The
// page never reads the clipboard: history.rs does, on a thread of its own, and
// the page is handed entries. Typing into other windows is svc-inject's.

pub mod history;
pub mod urls;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use base64::Engine;
use history::{Config, Entry, History, Kind};
use notch_core::{log, Ctx, Feature, Group, Manifest, ShortcutDef, Use};
use serde_json::{json, Value};
use svc_inject::{clip, ForegroundGuard};
use svc_shell::Item;
use svc_sysevents::Session;

const ID: &str = "clipboard";

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Clipboard",
    group: Group::Tools,
    default_on: true,
    uses: &[Use::Clipboard],
    shortcuts: &[
        ShortcutDef { id: "open", label: "Open the clipboard history", default: Some("Ctrl+Alt+V") },
        ShortcutDef { id: "paste-plain", label: "Paste as plain text", default: Some("Ctrl+Alt+Shift+V") },
    ],
    tray: &[],
    // clearAfter: seconds, 0 = never. limit: 0 = no limit. keepDays: 0 = for good.
    // ignoredApps, cleanExtra: names separated by commas.
    defaults: r#"{ "recording": true, "media": true, "ignoredApps": "", "limit": 100, "keepDays": 90, "copyOnly": false, "notify": false, "cleanUrls": false, "cleanExtra": "", "clearAfter": 0, "clearOnLock": false, "clearOnSleep": false, "clearOnDisplayOff": false, "clearWindowsHistory": false }"#,
};

/// The last window in front that was not one of the app's: where a click on an
/// entry pastes when the island itself has the keyboard.
static LAST_FRONT: AtomicIsize = AtomicIsize::new(0);

struct Running {
    history: History,
    _front: svc_sysevents::Sub,
    _session: svc_sysevents::Sub,
}

#[derive(Default)]
pub struct Clipboard {
    run: Mutex<Option<Running>>,
    /// A plain paste is on its way: its own Ctrl+V must not start another.
    pasting: AtomicBool,
}

fn ids(args: &Value) -> Vec<String> {
    match (&args["ids"], &args["id"]) {
        (Value::Array(ids), _) => ids.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        (_, Value::String(id)) => vec![id.clone()],
        _ => Vec::new(),
    }
}

/// An entry as the page draws it.
fn view(e: &Entry) -> Value {
    json!({
        "id": e.id,
        "kind": e.kind(),
        // ponytail: the page previews and searches the first 4 000 characters of an entry; the paste is always whole.
        "text": e.text.chars().take(4000).collect::<String>(),
        "chars": e.text.chars().count(),
        "width": e.width,
        "height": e.height,
        "size": e.size,
        "app": e.app,
        "exe": e.exe,
        "at": e.at,
        "pinned": e.pinned,
    })
}

fn data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
}

fn first_file(entry: &Entry) -> Result<&str, String> {
    entry.text.lines().find(|p| Path::new(p).exists()).ok_or_else(|| "Those files are not there any more.".to_string())
}

impl Clipboard {
    fn history(&self) -> Result<History, String> {
        self.run.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.history.clone()).ok_or_else(|| "Clipboard is turned off.".into())
    }

    /// Ctrl+Alt+Shift+V: what is on the clipboard goes in as text only, and the clipboard keeps its formatted copy.
    fn paste_plain(&self) -> Result<(), String> {
        if self.pasting.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let text = clip::open().ok().filter(|_| !clip::has(clip::CF_HDROP)).and_then(|c| c.text(8 << 20));
        let sent = match text.filter(|t| !t.is_empty()) {
            Some(text) => svc_inject::paste_text(&text),
            // A picture, files: there is no plain form of those, so it is an ordinary paste.
            None => svc_inject::paste_clipboard(),
        };
        self.pasting.store(false, Ordering::SeqCst);
        sent
    }
}

/// Where a paste goes: the window in front, or the one that was before the island took the keyboard.
fn target() -> ForegroundGuard {
    let front = ForegroundGuard::capture();
    let mut pid = 0u32;
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            windows::Win32::Foundation::HWND(front.hwnd() as *mut _),
            Some(&mut pid),
        );
    }
    if pid == std::process::id() { ForegroundGuard::of(LAST_FRONT.load(Ordering::SeqCst)) } else { front }
}

/// What a paste came to, for the page: done, or why not (the entry is on the clipboard either way).
fn outcome(sent: Result<(), String>) -> Value {
    match sent {
        Ok(()) => json!({ "pasted": true }),
        Err(why) => json!({ "pasted": false, "elevated": why == svc_inject::ELEVATED, "why": why }),
    }
}

impl Feature for Clipboard {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, cx: &Ctx) -> Result<(), String> {
        let mut run = self.run.lock().unwrap_or_else(|e| e.into_inner());
        if run.is_some() {
            return Ok(());
        }
        let emit = {
            let cx = cx.clone();
            Arc::new(move |event: &str, payload: Value| cx.emit(ID, event, payload))
        };
        let history = History::start(cx.data_dir(ID), Config::from_settings(&cx.settings(ID)), emit, notch_core::paused)?;

        let me = std::process::id();
        let remember = move |front: &svc_sysevents::Foreground| {
            if front.hwnd != 0 && front.pid != me {
                LAST_FRONT.store(front.hwnd, Ordering::SeqCst);
            }
        };
        remember(&svc_sysevents::foreground());
        let session = {
            let history = history.clone();
            svc_sysevents::on_session(move |event| match event {
                Session::Lock => history.clear_on("lock"),
                Session::Sleep => history.clear_on("sleep"),
                Session::DisplayOff => history.clear_on("display"),
                _ => {}
            })
        };
        *run = Some(Running { history, _front: svc_sysevents::on_foreground(remember), _session: session });
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        if let Some(run) = self.run.lock().unwrap_or_else(|e| e.into_inner()).take() {
            run.history.stop();
        }
    }

    fn settings_changed(&self, _cx: &Ctx, settings: &Value) {
        if let Ok(history) = self.history() {
            history.set_config(Config::from_settings(settings));
        }
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let history = self.history()?;
        let one = || history.picked(&ids(&args)).into_iter().next().ok_or_else(|| "That entry is not in the history any more.".to_string());
        match method {
            "list" => Ok(json!({
                "entries": history.entries().iter().map(view).collect::<Vec<_>>(),
                "recording": history.config().recording && !cx.paused(),
            })),
            // A click on an entry: it goes on the clipboard and, unless the user
            // only wants it copied, into the window they were in.
            "paste" => {
                let into = target();
                history.write(&ids(&args))?;
                if cx.settings(ID)["copyOnly"].as_bool() == Some(true) {
                    return Ok(json!({ "pasted": false, "copied": true }));
                }
                Ok(outcome(into.restore().and_then(|_| svc_inject::paste_clipboard())))
            }
            "copy" => history.write(&ids(&args)).map(|_| Value::Null),
            "pin" => {
                history.pin(args["id"].as_str().unwrap_or_default(), args["on"].as_bool().unwrap_or(true));
                Ok(Value::Null)
            }
            "remove" => {
                history.remove(&ids(&args));
                Ok(Value::Null)
            }
            // Deleted from memory and from disk: all but the pinned entries, or those too with `all`.
            "clear" => {
                history.clear(args["all"].as_bool() == Some(true));
                Ok(Value::Null)
            }
            // A picture entry's own picture; for files, the first file's picture or icon. `size` is the side
            // of the square the Shell draws it in: the row asks for 128, the preview for more.
            "thumb" => {
                let entry = one()?;
                let px = args["size"].as_u64().unwrap_or(128).clamp(32, 1024) as u32;
                let png = match entry.kind() {
                    Kind::Image => svc_shell::thumbnail(&history.image_path(&entry), px)?,
                    Kind::Files => {
                        // Files that are gone have no picture: the row keeps its icon, and nothing is an error.
                        let Ok(path) = first_file(&entry) else { return Ok(Value::Null) };
                        svc_shell::thumbnail(Path::new(path), px).or_else(|_| svc_shell::icon(path, 32))?
                    }
                    _ => return Err("That entry has no picture.".into()),
                };
                Ok(json!(data_url(&png)))
            }
            // The icon of a program something was copied in.
            "icon" => {
                let exe = args["exe"].as_str().unwrap_or_default();
                if !history.entries().iter().any(|e| e.exe == exe) {
                    return Err("That program is not in the history.".into());
                }
                Ok(json!(data_url(&svc_shell::icon(exe, 32)?)))
            }
            "open" => {
                let entry = one()?;
                match entry.kind() {
                    Kind::Link => notch_core::win::open_url(&entry.text),
                    Kind::Files => svc_shell::open(first_file(&entry)?)?,
                    Kind::Image => svc_shell::open(&history.image_path(&entry).to_string_lossy())?,
                    Kind::Text => return Err("A text has nothing to open.".into()),
                }
                Ok(Value::Null)
            }
            "reveal" => {
                let entry = one()?;
                match entry.kind() {
                    Kind::Files => svc_shell::reveal(Path::new(first_file(&entry)?))?,
                    Kind::Image => svc_shell::reveal(&history.image_path(&entry))?,
                    _ => return Err("Only files can be shown in their folder.".into()),
                }
                Ok(Value::Null)
            }
            // An OLE drag out of the island, on the UI thread, until the button is released.
            "drag" => {
                let items: Vec<Item> = history
                    .picked(&ids(&args))
                    .iter()
                    .flat_map(|e| match e.kind() {
                        Kind::Text => vec![Item::Text(e.text.clone())],
                        Kind::Link => vec![Item::Link(e.text.clone())],
                        Kind::Files => e.text.lines().filter(|p| Path::new(p).exists()).map(|p| Item::File(PathBuf::from(p))).collect(),
                        Kind::Image => vec![Item::File(history.image_path(e))],
                    })
                    .collect();
                if items.is_empty() {
                    return Ok(json!("none"));
                }
                let (done, dropped) = mpsc::channel();
                cx.app()
                    .run_on_main_thread(move || {
                        let _ = done.send(svc_shell::drag_out(&items));
                    })
                    .map_err(|e| e.to_string())?;
                Ok(json!(dropped.recv().is_ok_and(|effect| effect != svc_shell::DropEffect::None)))
            }
            // The link cleaner, for the tester in Settings: `{ url, removed }`, or null when it is no link.
            "clean" => {
                let extra = history.config().clean_extra;
                Ok(urls::clean(args["text"].as_str().unwrap_or_default(), &extra).map_or(Value::Null, |c| json!({ "url": c.url, "removed": c.removed })))
            }
            // Cleans the link that is on the clipboard now, whatever the automatic mode is set to.
            "clean-clipboard" => {
                let text = clip::open()?.text(1 << 16).unwrap_or_default();
                let Some(cleaned) = urls::clean(&text, &history.config().clean_extra) else { return Ok(Value::Null) };
                if !cleaned.removed.is_empty() {
                    {
                        let clipboard = clip::open()?;
                        clipboard.empty();
                        clipboard.set_text(&cleaned.url)?;
                    }
                    clip::mark_own();
                }
                Ok(json!({ "removed": cleaned.removed }))
            }
            "shortcut" => {
                if args["id"].as_str() == Some("paste-plain") {
                    if let Err(why) = self.paste_plain() {
                        log::line(format!("clipboard: plain paste: {why}"));
                        cx.emit(ID, "paste-failed", outcome(Err(why)));
                    }
                }
                // "open" is the page's: it opens the section.
                Ok(Value::Null)
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/clipboard/strings.json")
    }
}

pub fn feature() -> Arc<dyn Feature> {
    Arc::new(Clipboard::default())
}
