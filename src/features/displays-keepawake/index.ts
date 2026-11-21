// Displays and keep awake — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Rust holds the keep-awake request, the brightness writes, the overlays, the wallpaper and the radios;
// this file draws the strip, the three sections, the settings and the dimming overlay page.

import "./style.css";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { State } from "../../core/state";
import { h } from "../../views/dom";
import { t } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { btn, card, field, row, setMeter, slider, stripRow, toggle } from "../../ui/components";

interface SessionState { active: boolean; startedAt?: number; endsAt?: number | null; auto?: boolean }
interface DisplayRow { id: string; name: string; internal: boolean; primary: boolean; method: "wmi" | "ddc" | "ddc-write" | "none"; level: number }
interface PanelRow { id: string; name: string; internal: boolean; primary: boolean; brightness: number; contrast: number; hasBrightness: boolean; hasContrast: boolean }
interface PanelState { displays: PanelRow[]; selected: string }
interface Pic { path: string; name: string }

const P = "f-displays-keepawake-";
/** A circle with half of it filled: contrast. Not in the shared set. */
const CONTRAST = "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 5v14a7 7 0 0 1 0-14z";
let session: SessionState = { active: false };

// ── readings ─────────────────────────────────────────────────────────────────

/** Whole minutes left, rounded up: "45m", "1h35"; "∞" when open-ended. */
function left(): string {
  if (session.endsAt == null) return "∞";
  const m = Math.max(0, Math.ceil((session.endsAt - Date.now()) / 60000));
  return m >= 60 ? `${Math.floor(m / 60)}h${String(m % 60).padStart(2, "0")}` : `${m}m`;
}

const DURATIONS: [number, string][] = [[15, "15 min"], [30, "30 min"], [60, "1 hour"], [120, "2 hours"], [240, "4 hours"], [480, "8 hours"], [0, "Until stopped"]];

export default defineFeature({
  id: "displays-keepawake",
  voice: () => import("./voice"),

  activities: [
    {
      id: "keepawake",
      order: 6,
      tick: true,
      section: "keep",
      isActive: () => session.active,
      render: () => stripRow({ icon: "coffee", tone: "warn", title: t("Keep awake"), detail: session.auto ? t("Automatic") : undefined, value: left() }),
      mark: () => h("span", { class: "isl-ico warn" }, icon("coffee", 14)),
    },
  ],

  sections: [
    { id: "keep", title: "Keep awake", icon: "coffee", letter: "k", height: 250, build: (cx) => keepPage(cx) },
    { id: "displays", title: "Displays", icon: "monitor", letter: "y", height: 250, build: (cx) => displaysPage(cx) },
    { id: "wallpaper", title: "Wallpaper", icon: "image", letter: "w", height: 250, build: (cx) => wallpaperPage(cx) },
  ],

  settings(cx) {
    const box = card([]);
    cx.settings.get().then(async (s) => {
      const radios = (await cx.call<{ bluetooth: string; wifi: string }>("radios")) ?? { bluetooth: "none", wifi: "none" };
      const set = (k: string) => (on: boolean) => void cx.settings.patch({ [k]: on });
      const pick = (k: string, opts: [string, string][], num = true) =>
        select(opts, String(s[k]), (v) => void cx.settings.patch({ [k]: num ? Number(v) : v }));
      const rows: (HTMLElement | null)[] = [
        h("div", { class: "section-title", text: cx.t("Keep awake") }),
        row(cx.t("Let the display sleep"), toggle(s.allowDisplaySleep === true, set("allowDisplaySleep")), cx.t("The PC stays awake; only the screen may turn off.")),
        row(cx.t("Stop on low battery"), pick("batteryLimit", [["0", cx.t("Never")], ["5", "5%"], ["10", "10%"], ["15", "15%"], ["20", "20%"]]), cx.t("Ends a session on battery at or below this charge.")),
        row(cx.t("Keep the pointer active"), toggle(s.jiggle === true, set("jiggle")), cx.t("Moves the pointer one pixel now and then, so that chat apps and lock timers see activity.")),
        row(cx.t("Pointer interval"), pick("jiggleMinutes", [["1", "1 min"], ["2", "2 min"], ["5", "5 min"], ["10", "10 min"], ["15", "15 min"]])),
        row(cx.t("Start with an external display"), toggle(s.autoExternal === true, set("autoExternal")), cx.t("A session runs while a monitor other than the built-in one is connected.")),
        row(cx.t("Start on power"), toggle(s.autoPower === true, set("autoPower")), cx.t("A session runs while the charger is plugged in.")),
        row(cx.t("Start while agents work"), toggle(s.autoAgents === true, set("autoAgents")), cx.t("A session runs while an agent works, and for a few quiet minutes after.")),
        row(cx.t("Quiet minutes after agents"), pick("agentsMinutes", [["0", "0"], ["2", "2 min"], ["5", "5 min"], ["10", "10 min"], ["30", "30 min"]])),
        row(cx.t("Need every condition"), toggle(s.autoAll === true, set("autoAll")), cx.t("Off: any one of them is enough.")),
        h("div", { class: "section-title", text: cx.t("Displays") }),
        row(cx.t("Dim below the minimum"), toggle(s.extraDimming !== false, set("extraDimming")), cx.t("The lowest part of a slider darkens the picture over the backlight's lowest step.")),
        row(cx.t("Brightness keys move every display"), toggle(s.linkKeys === true, set("linkKeys")), cx.t("The shortcuts step all displays by the same amount, keeping the gaps between them.")),
        row(cx.t("Move displays together"), toggle(s.linkDisplays === true, set("linkDisplays")), cx.t("Moving one slider moves the others by the same amount.")),
        radios.bluetooth !== "none" || radios.wifi !== "none" ? h("div", { class: "section-title", text: cx.t("Sleep") }) : null,
        radios.bluetooth !== "none"
          ? row(cx.t("Bluetooth off during sleep"), toggle(s.bluetoothSleep === true, set("bluetoothSleep")), cx.t("Back on at wake only if Lintel turned it off."))
          : null,
        radios.wifi !== "none"
          ? row(cx.t("Wi-Fi off during sleep"), toggle(s.wifiSleep === true, set("wifiSleep")), cx.t("Back on at wake only if Lintel turned it off."))
          : null,
        h("div", { class: "section-title", text: cx.t("Wallpaper") }),
        row(
          cx.t("Picture folder"),
          field({ value: String(s.wallpaperFolder ?? ""), placeholder: cx.t("Your Pictures folder"), onEnter: (v) => void cx.settings.patch({ wallpaperFolder: v.trim() }) }),
          cx.t("Press Enter to apply. Only pictures directly in this folder are listed."),
        ),
      ];
      box.append(...rows.filter((r): r is HTMLElement => r !== null));
    });
    return box;
  },

  // The dimming overlay: a black page whose opacity is the dimming. Rust makes the window click-through.
  windows: {
    dim(root, cx) {
      root.className = `${P}dim`;
      const meta = (window as unknown as { __TAURI_INTERNALS__?: { metadata?: { currentWindow?: { label?: string } } } }).__TAURI_INTERNALS__;
      const label = meta?.metadata?.currentWindow?.label ?? "";
      const set = (a: number) => (root.style.opacity = String(Math.max(0, Math.min(1, a))));
      cx.on<{ label: string; alpha: number }>("dim", (e) => e.label === label && set(e.alpha));
      void cx.call<number>("dimAlpha", { label }).then((a) => set(a ?? 0));
    },
    // The brightness and contrast panel, opened by its tray icon (Rust).
    panel(root, cx) {
      brightnessPanel(root, cx);
    },
  },

  start(cx) {
    cx.call<SessionState>("state").then((s) => {
      if (s) {
        session = s;
        cx.island.activityChanged("keepawake");
      }
    });
    // Read-only: whether an agent works, told to Rust on each change (it decides if the condition is on).
    // debt: reads State directly (as agent-usage does); a host event would be cleaner.
    let working = false;
    const sync = () => {
      const w = State.tasks.some((t) => t.state !== "idle" && t.state !== "sleeping");
      if (w !== working) void cx.call("agents", { working: (working = w) });
    };
    sync();
    const offs = [
      State.subscribe(sync),
      cx.on<SessionState>("session", (s) => {
        session = s;
        cx.island.activityChanged("keepawake");
      }),
      cx.on<{ reason: string }>("ended", (e) =>
        cx.island.notice(
          e.reason === "battery"
            ? { kind: "battery", title: cx.t("Keep awake stopped"), detail: cx.t("The battery is low."), icon: "battery", tone: "warn", section: "keep" }
            : { kind: "timer", title: cx.t("Keep awake ended"), icon: "coffee", tone: "ok", section: "keep" },
        ),
      ),
      cx.on<{ name: string; level: number; shortcut?: boolean; count?: number }>("brightness", (e) => {
        // The display is named only when there is more than one to tell apart.
        if (e.shortcut) cx.island.notice({ kind: "brightness", title: (e.count ?? 1) > 1 ? e.name : cx.t("Brightness"), level: e.level, icon: "sun", section: "displays" });
      }),
    ];
    return () => {
      offs.forEach((f) => f());
      session = { active: false };
    };
  },
});

/** A `.field` around a native select. */
function select(opts: [string, string][], value: string, on: (v: string) => void): HTMLElement {
  const sel = h("select");
  for (const [v, label] of opts) sel.append(h("option", { value: v, text: label }));
  sel.value = value;
  sel.addEventListener("change", () => on(sel.value));
  return h("label", { class: "field" }, sel);
}

// ── Keep awake ───────────────────────────────────────────────────────────────

function keepPage(cx: FeatureCtx) {
  const el = h("div", { class: `${P}page` });
  let last = 60;
  let timer: number | undefined;
  cx.settings.get().then((s) => {
    if (typeof s.minutes === "number") last = s.minutes;
    draw();
  });

  const clock = () => {
    if (session.endsAt == null) return cx.t("Until you stop it");
    const at = new Date(session.endsAt);
    return cx.t("Ends at {time}", { time: at.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) });
  };

  function draw() {
    if (!session.active) {
      el.replaceChildren(
        h("p", { class: `${P}hint`, text: cx.t("Keeps this PC from sleeping. Pick how long.") }),
        h("div", { class: `${P}chips` },
          ...DURATIONS.map(([m, label]) =>
            h("button", { class: `tab${m === last ? " on" : ""}`, onclick: () => void cx.call("start", { minutes: m }) }, h("span", { text: cx.t(label) })))),
      );
      return;
    }
    el.replaceChildren(
      h("div", { class: `${P}clock`, text: left() }),
      h("p", { class: `${P}hint`, text: session.auto ? `${cx.t("Automatic")} · ${clock()}` : clock() }),
      h("div", { class: `${P}chips` },
        session.endsAt != null ? btn(cx.t("+15 min"), "secondary", () => void cx.call("extend", { minutes: 15 })) : null,
        btn(cx.t("Stop"), "danger", () => void cx.call("stop")),
      ),
    );
  }

  // Listeners of one event are not called in the order they were added: this one may run before the
  // one in `start`, so it takes the session from the event itself instead of reading it a step late.
  const off = cx.on<SessionState>("session", (s) => {
    session = s;
    draw();
  });
  void off;
  return {
    el,
    show() {
      draw();
      // The reading moves by the minute.
      timer = window.setInterval(() => session.active && draw(), 15000);
    },
    hide() {
      if (timer != null) clearInterval(timer);
      timer = undefined;
    },
    sync() {},
  };
}

// ── Displays ─────────────────────────────────────────────────────────────────

function displaysPage(cx: FeatureCtx) {
  const el = h("div", { class: `${P}list` });
  const bars = new Map<string, HTMLElement>();
  const nums = new Map<string, HTMLElement>();
  let shown = false;

  const put = (id: string, level: number) => {
    const bar = bars.get(id);
    // A slider being dragged is not moved under the pointer.
    if (bar && !bar.classList.contains("dragging")) setMeter(bar, level);
    const n = nums.get(id);
    if (n) n.textContent = `${Math.round(level * 100)}%`;
  };

  const hint = (d: DisplayRow) =>
    d.method === "none" ? cx.t("Dimming only") : d.method === "ddc-write" ? cx.t("Current level unknown") : d.internal ? cx.t("Built-in") : cx.t("External");

  async function load() {
    const list = await cx.call<DisplayRow[]>("displays");
    bars.clear();
    nums.clear();
    if (!list || !list.length) {
      el.replaceChildren(h("p", { class: `${P}hint`, text: cx.t("No display found.") }));
      return;
    }
    el.replaceChildren(
      ...list.map((d) => {
        // A drag writes at most every 120 ms; letting go writes the last value.
        let sent = 0;
        let pending: number | undefined;
        const send = (v: number) => {
          sent = Date.now();
          void cx.call<{ id: string; level: number }[]>("set", { id: d.id, level: v }).then((moved) =>
            moved?.forEach((m) => m.id !== d.id && put(m.id, m.level)),
          );
        };
        const bar = slider(d.level, {
          label: d.name,
          onInput: (v) => {
            put(d.id, v);
            clearTimeout(pending);
            if (Date.now() - sent >= 120) send(v);
            else pending = window.setTimeout(() => send(v), 120);
          },
          onChange: (v) => {
            clearTimeout(pending);
            put(d.id, v);
            send(v);
          },
          onDrag: (on) => {
            if (shown) (on ? holds.set(d.id, cx.island.hold()) : holds.get(d.id)?.());
          },
        });
        bars.set(d.id, bar);
        const num = h("b", { text: `${Math.round(d.level * 100)}%` });
        nums.set(d.id, num);
        return h("div", { class: `${P}disp` },
          h("span", { class: `${P}ico` }, icon(d.internal ? "sun" : "monitor", 16)),
          h("span", { class: `${P}name` }, h("b", { text: d.name }), h("small", { text: hint(d) })),
          bar,
          num,
        );
      }),
    );
  }
  const holds = new Map<string, () => void>();

  const offs = [
    cx.on<{ id: string; level: number }>("brightness", (e) => shown && put(e.id, e.level)),
    cx.on("displays", () => shown && void load()),
  ];
  void offs;
  return {
    el,
    show() {
      shown = true;
      void load();
    },
    hide() {
      shown = false;
      holds.forEach((release) => release());
      holds.clear();
    },
    sync() {},
  };
}

// ── Brightness and contrast ──────────────────────────────────────────────────

/** The panel behind the tray icon. Wheel over a slider steps it 2 % a notch, smoothly. */
function brightnessPanel(root: HTMLElement, cx: FeatureCtx) {
  type Kind = "brightness" | "contrast";
  let state: PanelState = { displays: [], selected: "" };
  let sel = "";
  // Our value is the one on screen until the hardware has caught up: a write in flight, one
  // waiting, or one that landed less than QUIET_MS ago. Nothing from Rust moves the sliders then,
  // which is what made a wheel turn jitter back and forth.
  const want: Record<Kind, number | null> = { brightness: null, contrast: null };
  const busy: Record<Kind, boolean> = { brightness: false, contrast: false };
  const quiet: Record<Kind, number> = { brightness: 0, contrast: 0 };
  const QUIET_MS = 700;

  const deviceIco = h("span", { class: `${P}device-ico` }, icon("monitor", 18));
  const name = h("b", { text: "" });
  const sub = h("small", { text: "" });
  const brightBar = slider(1, { label: cx.t("Brightness"), wheel: true, wheelStep: 0.02, onChange: (v) => send("brightness", v) });
  const brightNum = h("b", { class: `${P}num`, text: "—" });
  const contrastBar = slider(1, { label: cx.t("Contrast"), wheel: true, wheelStep: 0.02, onChange: (v) => send("contrast", v) });
  const contrastNum = h("b", { class: `${P}num`, text: "—" });
  const brightRow = h("div", { class: `${P}row` }, h("span", { class: `${P}ico` }, icon("sun", 16)), brightBar, brightNum);
  const contrastRow = h("div", { class: `${P}row` }, h("span", { class: `${P}ico` }, icon(CONTRAST, 16)), contrastBar, contrastNum);
  const devices = h("div", { class: `${P}devices` });
  const nums: Record<Kind, HTMLElement> = { brightness: brightNum, contrast: contrastNum };
  // The reading of the refresh button, left of it and out of the layout: it comes and goes
  // without moving the button, which stays in line with the numbers below.
  const note = h("small", { class: `${P}note`, text: "" });
  const refreshBtn = h(
    "button",
    { class: `btn ghost icon ${P}refresh`, title: cx.t("Refresh"), "aria-label": cx.t("Refresh"), onclick: () => void refresh() },
    icon("refresh", 14),
  );

  const settled = (kind: Kind) => want[kind] == null && !busy[kind] && performance.now() >= quiet[kind];

  /** One write at a time per setting; a burst leaves only its last value waiting, which is sent
   *  when the write before it is done. */
  const flush = (kind: Kind) => {
    if (busy[kind]) return;
    const value = want[kind];
    if (value == null) return;
    want[kind] = null;
    const id = sel;
    busy[kind] = true;
    void cx
      .call<{ ok?: boolean }>("panelSet", { id, kind, value })
      .then((r) => {
        busy[kind] = false;
        if (want[kind] != null) window.setTimeout(() => flush(kind), 30);
        // A refused write (a display that cannot read or take it) puts the slider back.
        else if (!r || r.ok === false) void load();
      })
      .catch(() => {
        busy[kind] = false;
        void load();
      });
  };

  /** The number moves at once; the hardware follows, and its late answer is ignored. */
  const send = (kind: Kind, fraction: number) => {
    if (!sel) return;
    const value = Math.max(0, Math.min(100, Math.round(fraction * 100)));
    want[kind] = value;
    quiet[kind] = performance.now() + QUIET_MS;
    nums[kind].textContent = String(value);
    flush(kind);
  };

  const select = async (id: string) => {
    // A write still waiting belongs to the display that is being left behind.
    want.brightness = null;
    want.contrast = null;
    quiet.brightness = 0;
    quiet.contrast = 0;
    sel = id;
    const s = await cx.call<PanelState>("panelSelect", { id });
    if (s) {
      state = s;
      sel = s.selected;
    }
    paint();
  };

  const load = async () => {
    // A fresh reading is the truth again, including after a write the display refused.
    quiet.brightness = 0;
    quiet.contrast = 0;
    const s = await cx.call<PanelState>("panel");
    if (s) {
      state = s;
      sel = s.selected;
    }
    paint();
  };

  function paint() {
    const row = state.displays.find((d) => d.id === sel);
    devices.replaceChildren(
      ...state.displays.map((d) =>
        h("button", { class: `tab${d.id === sel ? " on" : ""}`, title: d.id, onclick: () => void select(d.id) },
          icon(d.internal ? "sun" : "monitor", 14), h("span", { text: d.name }))));
    if (!row) {
      name.textContent = cx.t("No display found.");
      sub.textContent = "";
      setMeter(brightBar, 0);
      setMeter(contrastBar, 0);
      brightNum.textContent = "—";
      contrastNum.textContent = "—";
      return;
    }
    name.textContent = row.name;
    // The monitor id for an external display, the screen's own name for a laptop panel.
    sub.textContent = row.internal ? cx.t("Built-in display") : row.id;
    // A setting being written is where the pointer or the wheel left it, not where the hardware was.
    if (settled("brightness")) {
      setMeter(brightBar, row.hasBrightness ? row.brightness / 100 : 0);
      brightNum.textContent = row.hasBrightness ? String(row.brightness) : "—";
    }
    if (settled("contrast")) {
      setMeter(contrastBar, row.hasContrast ? row.contrast / 100 : 0);
      contrastNum.textContent = row.hasContrast ? String(row.contrast) : "—";
    }
    brightBar.setAttribute("aria-disabled", String(!row.hasBrightness));
    contrastBar.setAttribute("aria-disabled", String(!row.hasContrast));
    brightRow.classList.toggle(`${P}off`, !row.hasBrightness);
    contrastRow.classList.toggle(`${P}off`, !row.hasContrast);
    deviceIco.replaceChildren(icon(row.internal ? "sun" : "monitor", 18));
  }

  // The tray icon's wheel writes where this window can see it. Our own writes come back here too,
  // a little late: while one is in flight the sliders stay where the user put them.
  cx.on<{ id: string; brightness: number; contrast: number }>("panel", (e) => {
    const d = state.displays.find((x) => x.id === e.id);
    if (!d) return;
    d.brightness = e.brightness;
    d.contrast = e.contrast;
    if (e.id === sel && settled("brightness") && settled("contrast")) paint();
  });

  /** A value that just changed gets the accent for a moment. */
  const flash = (el: HTMLElement, changed: boolean) => {
    if (!changed) return;
    el.classList.remove(`${P}changed`);
    void el.offsetWidth;
    el.classList.add(`${P}changed`);
    window.setTimeout(() => el.classList.remove(`${P}changed`), 1600);
  };

  /** Reads this display again and says whether the hardware moved since it was last shown. */
  let reading = false;
  const refresh = async () => {
    if (reading) return;
    reading = true;
    // One turn of the arrow for each click, restarted even when clicked again before it ended.
    refreshBtn.classList.remove(`${P}spin`);
    void refreshBtn.offsetWidth;
    refreshBtn.classList.add(`${P}spin`);
    note.textContent = "";
    const was = state.displays.find((d) => d.id === sel);
    const before = was && { brightness: was.brightness, contrast: was.contrast, hasBrightness: was.hasBrightness, hasContrast: was.hasContrast };
    let row: PanelRow | null = null;
    try {
      row = await cx.call<PanelRow | null>("panelRefresh");
    } finally {
      reading = false;
    }
    if (!row) {
      note.textContent = cx.t("No display found.");
      return;
    }
    const d = state.displays.find((x) => x.id === row.id);
    if (d) Object.assign(d, row);
    sel = row.id;
    paint();
    // What was just read is on screen, whatever a write of ours was doing.
    setMeter(brightBar, row.hasBrightness ? row.brightness / 100 : 0);
    setMeter(contrastBar, row.hasContrast ? row.contrast / 100 : 0);
    brightNum.textContent = row.hasBrightness ? String(row.brightness) : "—";
    contrastNum.textContent = row.hasContrast ? String(row.contrast) : "—";
    if (before) {
      const bright = before.brightness !== row.brightness || before.hasBrightness !== row.hasBrightness;
      const contrast = before.contrast !== row.contrast || before.hasContrast !== row.hasContrast;
      flash(brightNum, bright);
      flash(contrastNum, contrast);
      note.textContent = cx.t(bright || contrast ? "Updated" : "No change");
    }
    window.setTimeout(() => {
      if (!reading) note.textContent = "";
    }, 2200);
  };

  // While the panel is open, a change made at the monitor's own buttons shows up here by itself, and
  // Rust takes it as the level a wake restores. Silent: the note and the arrow belong to the button.
  let polling = false;
  const poll = async () => {
    if (reading || polling || !sel) return;
    if (want.brightness != null || want.contrast != null || busy.brightness || busy.contrast) return;
    const at = performance.now();
    if (at < quiet.brightness || at < quiet.contrast) return;
    polling = true;
    let row: PanelRow | null = null;
    try {
      row = await cx.call<PanelRow | null>("panelPoll");
    } finally {
      polling = false;
    }
    if (!row) return;
    const d = state.displays.find((x) => x.id === row!.id);
    if (!d) return;
    const moved =
      d.brightness !== row.brightness ||
      d.contrast !== row.contrast ||
      d.hasBrightness !== row.hasBrightness ||
      d.hasContrast !== row.hasContrast;
    Object.assign(d, row);
    if (moved) paint();
  };
  const timer = window.setInterval(() => void poll(), 2000);
  window.addEventListener("pagehide", () => clearInterval(timer));

  root.append(
    h("div", { class: `win ${P}panel-win` },
      h("div", { class: "win-bar" }, icon("sun", 14), h("span", { text: cx.t("Brightness and contrast") }),
        h("button", { class: "close", title: cx.t("Close"), onclick: () => void cx.call("close"), text: "×" })),
      h("div", { class: `${P}panel` },
        h("div", { class: `${P}device` }, deviceIco, h("div", { class: `${P}device-text` }, name, sub), note, refreshBtn),
        brightRow,
        contrastRow,
        devices)),
  );
  void load();
}

// ── Wallpaper ────────────────────────────────────────────────────────────────

const PER_PAGE = 12;

function wallpaperPage(cx: FeatureCtx) {
  const el = h("div", { class: `${P}wall` });
  let lists: { system: Pic[]; own: Pic[]; folder: string } = { system: [], own: [], folder: "" };
  let tab: "system" | "own" = "system";
  let page = 0;
  let turn = 0;
  const thumbs = new Map<string, string>();

  async function thumbs_(box: HTMLElement[], pics: Pic[], mine: number) {
    // One at a time, and only while this page is still the one on show.
    for (let i = 0; i < pics.length && mine === turn; i++) {
      let url = thumbs.get(pics[i].path);
      if (!url) {
        url = (await cx.call<string>("wallThumb", { path: pics[i].path })) ?? "";
        if (url) thumbs.set(pics[i].path, url);
      }
      if (url && mine === turn) box[i].append(h("img", { src: url, alt: "", draggable: "false" }));
    }
  }

  function draw() {
    turn++;
    const pics = lists[tab];
    const pages = Math.max(1, Math.ceil(pics.length / PER_PAGE));
    page = Math.min(page, pages - 1);
    const shownPics = pics.slice(page * PER_PAGE, (page + 1) * PER_PAGE);
    const cells = shownPics.map((p) =>
      h("button", {
        class: `${P}pic`,
        title: p.name,
        onclick: (e: Event) => {
          const b = e.currentTarget as HTMLElement;
          void cx.call("wallSet", { path: p.path }).then((r) => {
            if (r !== null) {
              b.classList.add("done");
              setTimeout(() => b.classList.remove("done"), 1500);
            }
          });
        },
      }));
    el.replaceChildren(
      h("div", { class: `${P}wall-bar` },
        h("div", { class: `${P}chips` },
          ...(["system", "own"] as const).map((k) =>
            h("button", { class: `tab${tab === k ? " on" : ""}`, onclick: () => { tab = k; page = 0; draw(); } }, h("span", { text: cx.t(k === "system" ? "Windows" : "My pictures") })))),
        h("span", { class: `${P}pager` },
          btn("‹", "ghost", () => { page = Math.max(0, page - 1); draw(); }),
          h("small", { text: `${page + 1} / ${pages}` }),
          btn("›", "ghost", () => { page = Math.min(pages - 1, page + 1); draw(); })),
      ),
      pics.length
        ? h("div", { class: `${P}grid` }, ...cells)
        : h("p", { class: `${P}hint`, text: tab === "own" ? cx.t("No pictures in {folder}.", { folder: lists.folder }) : cx.t("No pictures found.") }),
    );
    void thumbs_(cells, shownPics, turn);
  }

  return {
    el,
    show() {
      void cx.call<typeof lists>("wallpapers").then((l) => {
        if (l) lists = l;
        draw();
      });
    },
    hide() {
      turn++;
    },
    sync() {},
  };
}
