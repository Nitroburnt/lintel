// cargo run -p notch-feat-clipboard --example clipboard-probe [-- --unmarked]
//
// Proves the history without the app: a folder of its own, the real listener
// (a hidden window of its own), test copies of its own.
//
// What the user has on the clipboard is saved first and put back exactly at
// the end, and is never printed: only formats, lengths and decisions are.
//
// Two modes, because of Windows' own clipboard history (Win+V, 25 entries):
//
//   default      every test copy carries the marks that keep it out of every
//                clipboard history, Windows' included. The app does not look at
//                those marks: they are recorded like any other copy. Shows the
//                link rules, the listener, and that the user's clipboard comes
//                back unchanged.
//   --unmarked   also makes ordinary copies (a text, a tracking link, a
//                picture…), which is the only way to show "recorded" and
//                "cleaned" end to end. Each of those ALSO lands in the user's
//                Win+V history when it is on, and pushes their oldest entry
//                out of it for good. Run it only when that is acceptable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notch_feat_clipboard::history::{Config, History, Kind};
use notch_feat_clipboard::urls;
use serde_json::Value;
use svc_inject::clip;

static PAUSED: AtomicBool = AtomicBool::new(false);
static EVENTS: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());

fn paused() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

/// The events since the last call, by name.
fn events() -> Vec<(String, Value)> {
    std::thread::sleep(Duration::from_millis(450));
    std::mem::take(&mut *EVENTS.lock().unwrap())
}

fn had(events: &[(String, Value)], name: &str) -> Option<Value> {
    events.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
}

struct Probe {
    /// The sequence number after the probe's last write: anything else means somebody else copied.
    last: u32,
    ok: bool,
}

impl Probe {
    /// Nobody but the probe (and the history's own rewrites) has written since the last step.
    fn alone(&self) -> bool {
        clip::sequence() == self.last || clip::is_own()
    }

    /// A test copy. `marked` adds the formats that keep it out of every history.
    fn put(&mut self, marked: bool, fill: impl FnOnce(&clip::Open) -> Result<(), String>) -> bool {
        if !self.alone() {
            return false;
        }
        let done = clip::open().and_then(|c| {
            c.empty();
            fill(&c)?;
            if marked { c.set_excluded() } else { Ok(()) }
        });
        self.last = clip::sequence();
        done.is_ok()
    }

    fn check(&mut self, what: &str, pass: bool, detail: String) {
        self.ok &= pass;
        println!("{} {what}: {detail}", if pass { "ok  " } else { "FAIL" });
    }
}

fn main() {
    let unmarked = std::env::args().any(|a| a == "--unmarked");
    urls::self_check();
    println!("ok   link cleaning rules: 8 checks");

    // ── What the user has: saved, never shown ───────────────────────────────────
    let held = match clip::open() {
        Ok(clipboard) => clipboard.snapshot(),
        Err(err) => return println!("STOP the clipboard could not be opened ({err}); nothing was touched"),
    };
    let names: Vec<String> = held.formats.iter().map(|(f, b)| format!("{} {}b", clip::format_name(*f), b.len())).collect();
    println!("saved the user's clipboard: {} formats, {} bytes [{}]", held.formats.len(), held.bytes(), names.join(", "));
    if !held.left.is_empty() {
        let left: Vec<String> = held.left.iter().map(|f| clip::format_name(*f)).collect();
        return println!("STOP it holds formats that cannot be saved ({}); nothing was touched", left.join(", "));
    }

    // Putting the user's content back is itself an unmarked copy. A plain text that is already the
    // newest entry of Win+V is not added a second time; for anything else that is not known, so the
    // default mode does not start.
    let text_only = held.formats.iter().all(|(f, _)| matches!(*f, clip::CF_TEXT | clip::CF_OEMTEXT | clip::CF_UNICODETEXT | clip::CF_LOCALE));
    if !unmarked && !text_only {
        return println!("STOP the clipboard holds more than plain text: restoring it could add an entry to Windows' own history. Nothing was touched (run with --unmarked to accept that).");
    }

    let dir = std::env::temp_dir().join(format!("lintel-clipboard-probe-{}", std::process::id()));
    let emit: notch_feat_clipboard::history::Emit = Arc::new(|event: &str, payload: Value| {
        println!("     event {event} {payload}");
        EVENTS.lock().unwrap().push((event.to_string(), payload));
    });
    let config = Config { clean_urls: true, ..Config::default() };
    let history = match History::start(dir.clone(), config.clone(), emit, paused) {
        Ok(history) => history,
        Err(err) => return println!("STOP {err}; nothing was touched"),
    };
    let mut p = Probe { last: clip::sequence(), ok: true };
    let ev = events();
    p.check("listener started; what was already on the clipboard is not recorded", ev.is_empty() && history.entries().is_empty(), format!("{} entries", history.entries().len()));

    // ── Copies their source marked: recorded like any other (the marks still keep them out of Win+V) ──
    let wide = |s: &str| s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
    let link = "https://example.com/watch?v=1&utm_source=probe&fbclid=abc#t";
    let tests: [(&str, &str, Box<dyn Fn(&clip::Open) -> Result<(), String>>); 2] = [
        ("text with all three marks", "text", Box::new(|c| c.set_text("clipboard-probe: marked text"))),
        ("a tracking link with all three marks, not rewritten", "link", Box::new(move |c| c.set_text(link))),
    ];
    for (n, (name, kind, fill)) in tests.iter().enumerate() {
        if !p.put(true, |c| fill(c)) {
            p.check(name, false, "somebody else copied meanwhile; stopping".into());
            break;
        }
        let ev = events();
        let got = had(&ev, "captured");
        p.check(
            name,
            got.as_ref().is_some_and(|c| c["kind"] == *kind) && had(&ev, "skipped").is_none() && history.entries().len() == n + 1,
            format!("recorded as {}, entries={}", got.map_or("none".into(), |g| g["kind"].to_string()), history.entries().len()),
        );
    }
    // CanIncludeInClipboardHistory=0 on its own, without the Exclude format: Windows' history honours
    // it as well, so this copy stays out of Win+V. (CanUploadToCloudClipboard=0 alone does not keep a
    // copy out of the local Win+V list, so that rule is only run with --unmarked.)
    let no_history = |c: &clip::Open| c.empty_then(|c| c.set_text("clipboard-probe: no history").and_then(|_| c.set(clip::format(clip::NO_HISTORY), &[0; 4])));
    if p.alone() && clip::open().and_then(|c| no_history(&c)).is_ok() {
        p.last = clip::sequence();
        let ev = events();
        p.check(
            "CanIncludeInClipboardHistory=0 alone",
            had(&ev, "captured").is_some_and(|c| c["kind"] == "text") && history.entries().len() == 3,
            format!("recorded; entries={}", history.entries().len()),
        );
    }

    // Paused: nothing is read at all.
    PAUSED.store(true, Ordering::SeqCst);
    if p.put(true, |c| c.set_text("clipboard-probe: while paused")) {
        let ev = events();
        p.check("a copy while the app is paused", had(&ev, "skipped").is_some_and(|s| s["why"] == "recording is paused") && history.entries().is_empty(), "skipped unread".into());
    }
    PAUSED.store(false, Ordering::SeqCst);

    // ── Ordinary copies: only when asked, they reach the user's Win+V history too ──
    if unmarked {
        println!("--unmarked: the next copies also enter Windows' own clipboard history if it is on");
        if p.put(false, |c| c.set_text("clipboard-probe: no cloud").and_then(|_| c.set(clip::format(clip::NO_CLOUD), &[0; 4]))) {
            let ev = events();
            p.check("CanUploadToCloudClipboard=0 alone", had(&ev, "captured").is_some_and(|c| c["kind"] == "text") && history.entries().len() == 4, format!("recorded; entries={}", history.entries().len()));
        }
        if p.put(false, |c| c.set_text("clipboard-probe: plain text")) {
            let ev = events();
            let got = had(&ev, "captured");
            p.check("plain text", got.as_ref().is_some_and(|c| c["kind"] == "text" && c["chars"] == 27) && history.entries().len() == 5, format!("{}", got.unwrap_or_default()));
        }
        if p.put(false, |c| c.set_text(link).and_then(|_| c.set(clip::format("UniformResourceLocatorW"), &wide(link)))) {
            let ev = events();
            let now = clip::open().ok().and_then(|c| c.text(1 << 16)).unwrap_or_default();
            let first = history.entries().into_iter().next().unwrap_or_default();
            p.check(
                "a link with tracking parameters",
                had(&ev, "cleaned").is_some_and(|c| c["removed"] == serde_json::json!(["utm_source", "fbclid"])) && now == "https://example.com/watch?v=1#t" && first.kind() == Kind::Link && first.text == now && clip::is_own(),
                format!("clipboard now holds {now:?}; entry kind={:?}", first.kind()),
            );
        }
        if p.put(false, |c| c.set_text("ghp_Xk9mP2vL7qR4wT6zN8aB3cD5eF1gH0")) {
            let ev = events();
            p.check("a text that looks like a key", had(&ev, "captured").is_some_and(|c| c["kind"] == "text") && history.entries().len() == 7, format!("recorded; entries={}", history.entries().len()));
        }
        if p.put(false, |c| c.set_text("clipboard-probe: plain text")) {
            let _ = events();
            let all = history.entries();
            p.check("the same text copied again", all.len() == 7 && all[0].kind() == Kind::Text, format!("{} entries, the text is first again", all.len()));
        }
        // A 2×2 bitmap, as a screenshot tool gives it.
        let mut dib = Vec::new();
        for n in [40u32, 2, 2, 1 | 32 << 16, 0, 16, 0, 0, 0, 0] {
            dib.extend(n.to_le_bytes());
        }
        dib.extend([255u8, 0, 0, 0].repeat(4));
        if p.put(false, |c| c.set(clip::CF_DIB, &dib)) {
            let ev = events();
            let first = history.entries().into_iter().next().unwrap_or_default();
            let file = history.image_path(&first);
            p.check("a picture", had(&ev, "captured").is_some_and(|c| c["kind"] == "image") && (first.width, first.height) == (2, 2) && file.exists(), format!("{}x{}, {} bytes of PNG kept", first.width, first.height, first.size));
            let back = history.write(&[first.id.clone()]);
            let formats = clip::open().map(|c| c.formats()).unwrap_or_default();
            p.last = clip::sequence();
            let ev = events();
            p.check("the picture put back on the clipboard", back.is_ok() && formats.contains(&clip::format("PNG")) && formats.contains(&clip::CF_DIB) && had(&ev, "captured").is_none(), "PNG and CF_DIB, not recorded again".into());
        }
        history.set_config(Config { clear_after: Some(Duration::from_secs(1)), ..config });
        if p.put(false, |c| c.set_text("clipboard-probe: to be cleared")) {
            std::thread::sleep(Duration::from_millis(1400));
            let ev = events();
            p.check("auto clear after 1 s", had(&ev, "cleared").is_some() && clip::is_empty(), format!("clipboard empty={}", clip::is_empty()));
        }
        let count = history.entries().len();
        history.clear(true);
        let on_disk = std::fs::read_to_string(dir.join("history.json")).unwrap_or_default();
        let pictures = std::fs::read_dir(dir.join("images")).map_or(0, |d| d.count());
        p.check("clear history", history.entries().is_empty() && on_disk.trim() == "[]" && pictures == 0, format!("{count} entries before, 0 after; history.json={on_disk:?}; pictures left={pictures}"));
    } else {
        println!("     not run (needs --unmarked): CanUploadToCloudClipboard=0 alone, recorded text, cleaned link, repeat, picture, auto clear, clear history");
    }

    // ── Back to what the user had ──────────────────────────────────────────────
    // The listener ends first, so it never sees the user's content come back.
    let alone = p.alone();
    history.stop();
    if alone {
        let back = clip::open().map(|c| {
            c.restore(&held);
            c.snapshot()
        });
        let same = back.as_ref().is_ok_and(|b| *b == held);
        p.check("the user's clipboard restored exactly", same, format!("{} formats, {} bytes, identical={same}", held.formats.len(), held.bytes()));
    } else {
        println!("     somebody copied during the probe: their copy is left on the clipboard, nothing was restored over it");
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("temp folder removed: {}. {}", !dir.exists(), if p.ok { "ALL OK" } else { "NOT ALL OK" });
}
