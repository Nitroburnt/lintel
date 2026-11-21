// Global keyboard shortcuts — port of HotKeyCenter.swift + ShortcutLogic.swift.
//
// The shortcuts are registered from Rust through tauri-plugin-global-shortcut
// (RegisterHotKey). Two kinds share one registration pass:
//   * the island's own actions (`ACTIONS`): a press is handed to the island as
//     a `shortcut` event carrying the action id, the same way as the tray menu.
//     The ones that belong to a built-in feature (`owner`) are let go while
//     that feature is off;
//   * the shortcuts of the features that run (`Manifest.shortcuts`), under the
//     id `<feature>:<id>`: a press goes to the feature (registry::notify).
// What the user changed is in `settings.shortcuts`, by those same ids.
//
// Default keys. The Mac uses ⌃⌥ + a letter. On Windows Ctrl+Alt *is* AltGr on
// most European layouts, so a global Ctrl+Alt+E would swallow every € typed on
// a French or German keyboard. The defaults below were checked against the
// AltGr layer of the French (AZERTY), German (QWERTZ), Spanish, Italian,
// Portuguese and Brazilian (ABNT2) layouts, which between them put a character
// on E, Q, M, W, C and on every digit and most punctuation keys:
//
//   Ctrl+Alt+Space   open the chat           Ctrl+Alt+→ / ←  next / previous pill
//   Ctrl+Alt+A       waiting permission      Ctrl+Alt+S      mute Mascot
//   Ctrl+Alt+T       open the terminal
//   Ctrl+Alt+N       open / close the island (off by default, as on the Mac)
//
// ⌃⌥[ and ⌃⌥] became the arrows (brackets are AltGr characters almost
// everywhere) and ⌃⌥M became S (AltGr+M is µ in German). Layouts outside that
// list can still clash — Polish puts ą on AltGr+A and ś on AltGr+S — so on
// Windows every Ctrl+Alt combination is also checked against the keyboard
// layouts actually installed (platform::ctrl_alt_types) and left unregistered,
// flagged in Settings, when it types a character. The same table lives in
// src/core/shortcuts.ts; tests/shortcuts.test.mjs keeps the two in step.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_global_shortcut::{Code, GlobalShortcut, Modifiers, Shortcut, ShortcutState};

use crate::island::WINDOW_LABEL;

/// One global action, as on the Mac (`ShortcutAction`). The ids are the Mac's
/// raw values and are stored in settings.json: never rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionDef {
    pub id: &'static str,
    pub default_keys: &'static str,
    pub enabled_by_default: bool,
    /// False for the Mac actions this version cannot do yet. Their ids and
    /// defaults stay reserved so a later port lands on the same keys.
    pub ported: bool,
}

const fn action(id: &'static str, keys: &'static str, on: bool, ported: bool) -> ActionDef {
    ActionDef { id, default_keys: keys, enabled_by_default: on, ported }
}

/// Same order as `ShortcutAction.allCases`.
pub const ACTIONS: &[ActionDef] = &[
    action("toggleIsland", "Ctrl+Alt+N", false, true),
    action("openChat", "Ctrl+Alt+Space", true, true),
    action("goToAlert", "Ctrl+Alt+A", true, true),
    action("jumpToTerminal", "Ctrl+Alt+T", true, true),
    // Dragging Mascot onto a window is not in this version.
    action("attachFrontWindow", "Ctrl+Alt+F", true, false),
    action("nextPill", "Ctrl+Alt+Right", true, true),
    action("prevPill", "Ctrl+Alt+Left", true, true),
    action("muteToggle", "Ctrl+Alt+S", true, true),
    // Not one of the Mac's: the next tab of the island. No key until the user gives it one.
    action("nextTab", "", true, true),
];

/// The built-in feature an action belongs to: it is not registered while that
/// feature is off.
fn owner(id: &str) -> Option<&'static str> {
    match id {
        "openChat" => Some("chat"),
        "goToAlert" | "jumpToTerminal" => Some("agents"),
        _ => None,
    }
}

pub fn find(id: &str) -> Option<&'static ActionDef> {
    ACTIONS.iter().find(|a| a.id == id)
}

/// What an action does, in English: the key of its label in Settings
/// (SHORTCUT_TEXT in src/core/shortcuts.ts; tests/shortcuts.test.mjs keeps
/// the two in step).

/// What the user chose for one action. An action missing from settings.json
/// uses its default; `keys` empty means no key at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Binding {
    pub keys: String,
    pub enabled: bool,
}

impl Default for Binding {
    fn default() -> Self {
        Self { keys: String::new(), enabled: true }
    }
}

/// `settings.shortcuts`: only the actions the user changed.
pub type Bindings = BTreeMap<String, Binding>;

/// The binding in force for `def`: the stored one, or the default.
pub fn effective(def: &ActionDef, stored: &Bindings) -> Binding {
    stored.get(def.id).cloned().unwrap_or_else(|| Binding {
        keys: def.default_keys.to_string(),
        enabled: def.enabled_by_default,
    })
}

// ── Status shown in Settings ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// Registered and listening.
    Active,
    /// Turned off, or no key.
    Off,
    /// Another app already holds this combination.
    InUse,
    /// Another Lintel shortcut has the same combination.
    Duplicate,
    /// Not a combination the OS can register.
    Invalid,
    /// Ctrl+Alt+key types a character on one of the keyboard layouts.
    TypesCharacter,
    /// The action itself isn't in this version.
    NotPorted,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionStatus {
    /// An action of `ACTIONS`, or `<feature>:<id>` for a feature's shortcut.
    pub id: String,
    pub status: Status,
    /// The character a `TypesCharacter` combination types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typed: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub actions: Vec<ActionStatus>,
}

/// What is registered right now: hot-key id → action id, plus the last report.
#[derive(Default)]
pub struct Registry {
    by_id: Mutex<HashMap<u32, String>>,
    report: Mutex<Vec<ActionStatus>>,
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// Parses an accelerator the way the plugin will. `None` for an empty or
/// unknown one, and for a bare or Shift-only key: a global shortcut needs
/// Ctrl, Alt or the Windows key, or it would take that key away from every
/// other app (same rule as parseKeys in src/core/shortcuts.ts).
pub fn parse(keys: &str) -> Option<Shortcut> {
    if keys.trim().is_empty() {
        return None;
    }
    let shortcut = Shortcut::from_str(keys).ok()?;
    let has_modifier = shortcut
        .mods
        .intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER | Modifiers::META);
    (has_modifier && shortcut.key != Code::Escape).then_some(shortcut)
}

/// True when Windows reads the combination as AltGr: Ctrl and Alt held, no
/// Windows key.
pub fn is_altgr_like(shortcut: &Shortcut) -> bool {
    shortcut.mods.contains(Modifiers::CONTROL | Modifiers::ALT)
        && !shortcut.mods.intersects(Modifiers::SUPER | Modifiers::META)
}

/// The Windows virtual-key code global-hotkey registers for `code`, for the
/// keys that can type a character. Others (arrows, F-keys…) never do: `None`.
pub fn character_vk(code: Code) -> Option<u16> {
    use Code::*;
    let letters = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN,
        KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    if let Some(i) = letters.iter().position(|c| *c == code) {
        return Some(0x41 + i as u16);
    }
    let digits = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
    if let Some(i) = digits.iter().position(|c| *c == code) {
        return Some(0x30 + i as u16);
    }
    Some(match code {
        Space => 0x20,
        Semicolon => 0xBA,
        Equal => 0xBB,
        Comma => 0xBC,
        Minus => 0xBD,
        Period => 0xBE,
        Slash => 0xBF,
        Backquote => 0xC0,
        BracketLeft => 0xDB,
        Backslash => 0xDC,
        BracketRight => 0xDD,
        Quote => 0xDE,
        IntlBackslash => 0xE2,
        _ => return None,
    })
}

/// What each action should do, before the OS is asked anything: the shortcut
/// to register, or the reason it won't be. `types` answers whether a Ctrl+Alt
/// combination types a character (see platform::ctrl_alt_types).
pub fn plan(
    stored: &Bindings,
    types: impl Fn(&Shortcut) -> Option<String>,
) -> Vec<(&'static ActionDef, Result<Shortcut, ActionStatus>)> {
    let mut seen = HashSet::new();
    ACTIONS
        .iter()
        .map(|def| {
            let refuse = |status, typed| Err(ActionStatus { id: def.id.to_string(), status, typed });
            let binding = effective(def, stored);
            let outcome = if !def.ported {
                refuse(Status::NotPorted, None)
            } else if !binding.enabled || binding.keys.trim().is_empty() {
                refuse(Status::Off, None)
            } else {
                match parse(&binding.keys) {
                    None => refuse(Status::Invalid, None),
                    Some(shortcut) => {
                        if let Some(typed) = types(&shortcut) {
                            refuse(Status::TypesCharacter, Some(typed))
                        } else if !seen.insert(shortcut.id()) {
                            refuse(Status::Duplicate, None)
                        } else {
                            Ok(shortcut)
                        }
                    }
                }
            };
            (def, outcome)
        })
        .collect()
}

/// `Lintel --shortcut <id>` → the action id, when it names a ported one.
pub fn from_args(args: &[String]) -> Option<&'static str> {
    let at = args.iter().position(|a| a == "--shortcut")?;
    let def = find(args.get(at + 1)?)?;
    def.ported.then_some(def.id)
}

// ── Registration ──────────────────────────────────────────────────────────────

/// The plugin, with every press routed through `dispatch`.
pub fn plugin() -> tauri::plugin::TauriPlugin<Wry> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, shortcut, event| {
            if event.state != ShortcutState::Pressed {
                return;
            }
            let Some(registry) = app.try_state::<Registry>() else { return };
            let action = registry.by_id.lock().unwrap().get(&shortcut.id()).cloned();
            if let Some(action) = action {
                dispatch(app, &action);
            }
        })
        .build()
}

/// Hands an action to the island, or a feature's shortcut to its feature.
pub fn dispatch(app: &AppHandle, action: &str) {
    crate::log::line(format!("shortcut {action}"));
    match action.split_once(':') {
        Some((feature, id)) => crate::registry::notify(app, feature, "shortcut", id),
        None => {
            let _ = app.emit_to(WINDOW_LABEL, "shortcut", action.to_string());
        }
    }
}

/// Unregisters everything Lintel holds.
fn release(app: &AppHandle) {
    if let Some(gs) = app.try_state::<GlobalShortcut<Wry>>() {
        if let Err(err) = gs.unregister_all() {
            crate::log::line(format!("shortcuts: unregister failed: {err}"));
        }
    }
    if let Some(registry) = app.try_state::<Registry>() {
        registry.by_id.lock().unwrap().clear();
    }
}

/// Registers the shortcuts in `stored` in place of the current ones, and
/// tells the settings window how each one went. Never fails: a shortcut that
/// can't be had is reported, not fatal.
pub fn apply(app: &AppHandle, stored: &Bindings) {
    release(app);
    let gs = app.try_state::<GlobalShortcut<Wry>>();
    let refused = |id: &str, status| Err(ActionStatus { id: id.to_string(), status, typed: None });

    // The island's actions first, then the running features' shortcuts: on a
    // clash the island keeps the keys.
    let mut entries: Vec<(String, Result<Shortcut, ActionStatus>)> = plan(stored, typed_character)
        .into_iter()
        .map(|(def, outcome)| {
            let off = owner(def.id).is_some_and(|feature| !crate::registry::is_running(app, feature));
            (def.id.to_string(), if off && outcome.is_ok() { refused(def.id, Status::Off) } else { outcome })
        })
        .collect();
    let mut seen: HashSet<u32> = entries.iter().filter_map(|(_, o)| o.as_ref().ok().map(Shortcut::id)).collect();
    for (feature, def) in crate::registry::shortcut_defs(app) {
        let id = format!("{feature}:{}", def.id);
        let binding = stored.get(&id).cloned().unwrap_or_else(|| Binding {
            keys: def.default.unwrap_or_default().to_string(),
            enabled: true,
        });
        let outcome = if !binding.enabled || binding.keys.trim().is_empty() {
            refused(&id, Status::Off)
        } else {
            match parse(&binding.keys) {
                None => refused(&id, Status::Invalid),
                Some(shortcut) => match typed_character(&shortcut) {
                    Some(typed) => Err(ActionStatus { id: id.clone(), status: Status::TypesCharacter, typed: Some(typed) }),
                    None if !seen.insert(shortcut.id()) => refused(&id, Status::Duplicate),
                    None => Ok(shortcut),
                },
            }
        };
        entries.push((id, outcome));
    }

    let mut by_id = HashMap::new();
    let mut report = Vec::new();
    let total = entries.len();
    let mut held: Vec<String> = Vec::new();
    for (id, outcome) in entries {
        let status = match outcome {
            Err(status) => status,
            Ok(_) if gs.is_none() => ActionStatus { id, status: Status::Off, typed: None },
            Ok(shortcut) => {
                // No lock of ours is held here: the plugin calls our handler
                // with its own lock taken, on the thread that registers.
                let result = gs.as_ref().map(|gs| gs.register(shortcut));
                let status = match result {
                    Some(Ok(())) => {
                        by_id.insert(shortcut.id(), id.clone());
                        Status::Active
                    }
                    Some(Err(err)) => {
                        held.push(format!("{id}: {err}"));
                        Status::InUse
                    }
                    None => Status::Off,
                };
                ActionStatus { id, status, typed: None }
            }
        };
        report.push(status);
    }

    if let Some(registry) = app.try_state::<Registry>() {
        // One line per registration, not one per key: while another app holds
        // these keys the same conflict would be written at every apply.
        if !held.is_empty() {
            crate::log::line(format!("shortcuts: {} of {total} already held: {}", held.len(), held.join(", ")));
        }
        *registry.by_id.lock().unwrap() = by_id;
        *registry.report.lock().unwrap() = report;
    }
    let _ = app.emit("shortcuts-status", status(app));
}

/// Lets go of every shortcut while a new one is being recorded in Settings,
/// so pressing a combination Lintel already holds records it instead of
/// running it.
pub fn suspend(app: &AppHandle) {
    release(app);
}

pub fn status(app: &AppHandle) -> Report {
    let registry = app.try_state::<Registry>();
    let actions = registry.as_ref().map(|r| r.report.lock().unwrap().clone()).unwrap_or_default();
    Report { actions }
}

fn typed_character(shortcut: &Shortcut) -> Option<String> {
    if !is_altgr_like(shortcut) {
        return None;
    }
    let vk = character_vk(shortcut.key)?;
    crate::platform::ctrl_alt_types(vk, shortcut.mods.contains(Modifiers::SHIFT))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_IDS: [&str; 9] = [
        "toggleIsland", "openChat", "goToAlert", "jumpToTerminal", "attachFrontWindow",
        "nextPill", "prevPill", "muteToggle", "nextTab",
    ];

    fn never(_: &Shortcut) -> Option<String> {
        None
    }

    // testDefaultsExhaustive
    #[test]
    fn every_mac_action_has_a_default_and_keeps_its_id() {
        let ids: Vec<_> = ACTIONS.iter().map(|a| a.id).collect();
        assert_eq!(ids, MAC_IDS);
    }

    // testAllDefaultsHaveModifier
    #[test]
    fn every_default_parses_and_has_a_modifier() {
        for def in ACTIONS.iter().filter(|a| !a.default_keys.is_empty()) {
            let shortcut = parse(def.default_keys)
                .unwrap_or_else(|| panic!("{} default does not parse", def.id));
            assert!(is_altgr_like(&shortcut), "{} default is not Ctrl+Alt", def.id);
        }
    }

    // testNoDefaultDuplicates
    #[test]
    fn no_two_defaults_share_a_combination() {
        let mut seen = HashSet::new();
        for def in ACTIONS.iter().filter(|a| !a.default_keys.is_empty()) {
            assert!(seen.insert(parse(def.default_keys).unwrap().id()), "{} duplicates", def.id);
        }
    }

    // testEnabledByDefault
    #[test]
    fn only_the_island_toggle_is_off_by_default() {
        for def in ACTIONS {
            assert_eq!(def.enabled_by_default, def.id != "toggleIsland", "{}", def.id);
        }
    }

    #[test]
    fn the_action_not_ported_yet_is_reserved_not_registered() {
        let plan = plan(&Bindings::new(), never);
        for (def, outcome) in plan {
            let reserved = def.id == "attachFrontWindow";
            assert_eq!(def.ported, !reserved);
            match outcome {
                Ok(_) => assert!(def.ported && def.enabled_by_default, "{}", def.id),
                Err(s) if reserved => assert_eq!(s.status, Status::NotPorted),
                Err(s) => assert!(s.status == Status::Off && (def.id == "toggleIsland" || def.default_keys.is_empty()), "{}", def.id),
            }
        }
    }

    #[test]
    fn parsing_needs_a_modifier_and_a_known_key() {
        assert!(parse("").is_none());
        assert!(parse("A").is_none(), "a bare key would take A from every app");
        assert!(parse("Shift+A").is_none(), "so would Shift+A");
        assert!(parse("Ctrl+Escape").is_none());
        assert!(parse("Ctrl+Alt+Nope").is_none());
        assert!(parse("Ctrl+Alt").is_none());
        let s = parse("Ctrl+Alt+Space").unwrap();
        assert_eq!(s.key, Code::Space);
        assert!(s.mods.contains(Modifiers::CONTROL | Modifiers::ALT));
        assert!(!s.mods.contains(Modifiers::SHIFT));
        let all = parse("Ctrl+Alt+Shift+Super+K").unwrap();
        assert!(all.mods.contains(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::SUPER));
        // Case and spelling don't change the combination.
        assert_eq!(parse("ctrl+alt+right").unwrap().id(), parse("Control+Alt+ArrowRight").unwrap().id());
    }

    #[test]
    fn only_ctrl_alt_without_the_windows_key_reads_as_altgr() {
        assert!(is_altgr_like(&parse("Ctrl+Alt+E").unwrap()));
        assert!(is_altgr_like(&parse("Ctrl+Alt+Shift+E").unwrap()));
        assert!(!is_altgr_like(&parse("Ctrl+Shift+E").unwrap()));
        assert!(!is_altgr_like(&parse("Ctrl+Alt+Super+E").unwrap()));
    }

    #[test]
    fn virtual_keys_match_the_ones_registered() {
        assert_eq!(character_vk(Code::KeyA), Some(0x41));
        assert_eq!(character_vk(Code::KeyZ), Some(0x5A));
        assert_eq!(character_vk(Code::Digit0), Some(0x30));
        assert_eq!(character_vk(Code::Digit9), Some(0x39));
        assert_eq!(character_vk(Code::BracketRight), Some(0xDD));
        assert_eq!(character_vk(Code::Space), Some(0x20));
        assert_eq!(character_vk(Code::ArrowLeft), None);
        assert_eq!(character_vk(Code::F5), None);
    }

    // testDuplicateDetection
    #[test]
    fn a_combination_used_twice_is_registered_once() {
        let mut stored = Bindings::new();
        let same = Binding { keys: "Ctrl+Alt+A".into(), enabled: true };
        stored.insert("jumpToTerminal".into(), same.clone());
        let plan = plan(&stored, never);
        let alert = plan.iter().find(|(d, _)| d.id == "goToAlert").unwrap();
        let term = plan.iter().find(|(d, _)| d.id == "jumpToTerminal").unwrap();
        assert!(alert.1.is_ok());
        assert_eq!(term.1.as_ref().unwrap_err().status, Status::Duplicate);

        // Same key, other modifiers: not a duplicate.
        stored.insert("jumpToTerminal".into(), Binding { keys: "Ctrl+Shift+A".into(), enabled: true });
        let plan = super::plan(&stored, never);
        assert!(plan.iter().all(|(d, o)| o.is_ok() || !d.ported || d.id == "toggleIsland" || d.default_keys.is_empty()));

        // A disabled action doesn't hold its keys.
        stored.insert("goToAlert".into(), Binding { keys: "Ctrl+Alt+A".into(), enabled: false });
        stored.insert("jumpToTerminal".into(), same);
        let plan = super::plan(&stored, never);
        let term = plan.iter().find(|(d, _)| d.id == "jumpToTerminal").unwrap();
        assert!(term.1.is_ok());
    }

    #[test]
    fn a_combination_that_types_a_character_is_not_registered() {
        let euro = |s: &Shortcut| (s.key == Code::KeyA).then(|| "ą".to_string());
        let plan = plan(&Bindings::new(), euro);
        let alert = plan.iter().find(|(d, _)| d.id == "goToAlert").unwrap();
        let refused = alert.1.as_ref().unwrap_err();
        assert_eq!(refused.status, Status::TypesCharacter);
        assert_eq!(refused.typed.as_deref(), Some("ą"));
    }

    #[test]
    fn stored_bindings_override_the_defaults_and_the_rest_fall_back() {
        let mut stored = Bindings::new();
        stored.insert("openChat".into(), Binding { keys: "Ctrl+Shift+K".into(), enabled: true });
        stored.insert("muteToggle".into(), Binding { keys: String::new(), enabled: true });
        let chat = effective(find("openChat").unwrap(), &stored);
        assert_eq!(chat.keys, "Ctrl+Shift+K");
        let alert = effective(find("goToAlert").unwrap(), &stored);
        assert_eq!(alert, Binding { keys: "Ctrl+Alt+A".into(), enabled: true });
        let toggle = effective(find("toggleIsland").unwrap(), &stored);
        assert!(!toggle.enabled);
        let plan = plan(&stored, never);
        let mute = plan.iter().find(|(d, _)| d.id == "muteToggle").unwrap();
        assert_eq!(mute.1.as_ref().unwrap_err().status, Status::Off);
    }

    // testLoadSaveRoundTrip
    #[test]
    fn bindings_round_trip_through_json() {
        let mut stored = Bindings::new();
        stored.insert("openChat".into(), Binding { keys: "Ctrl+Alt+K".into(), enabled: false });
        let json = serde_json::to_string(&stored).unwrap();
        assert_eq!(json, r#"{"openChat":{"keys":"Ctrl+Alt+K","enabled":false}}"#);
        assert_eq!(serde_json::from_str::<Bindings>(&json).unwrap(), stored);
        // A hand-edited entry without `enabled` stays on.
        let partial: Bindings = serde_json::from_str(r#"{"openChat":{"keys":"Ctrl+Alt+K"}}"#).unwrap();
        assert!(partial["openChat"].enabled);
    }

    #[test]
    fn the_command_line_names_an_action() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(from_args(&args(&["lintel", "--shortcut", "openChat"])), Some("openChat"));
        assert_eq!(from_args(&args(&["lintel", "--shortcut", "muteToggle"])), Some("muteToggle"));
        assert_eq!(from_args(&args(&["lintel", "--shortcut"])), None);
        assert_eq!(from_args(&args(&["lintel", "--shortcut", "rm -rf"])), None);
        assert_eq!(from_args(&args(&["lintel", "--shortcut", "attachFrontWindow"])), None);
        assert_eq!(from_args(&args(&["lintel"])), None);
    }
}
