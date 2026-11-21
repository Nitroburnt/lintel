// Breaks — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Rust owns the clock, the rule, the fade and the screen-time file; this file draws the strip
// row of a break, the two sections (Breaks, Screen time) and the settings.
//
// The Screen time section is a summary of today: the ring, the apps that took the most, and the
// week. The report itself — every app of any of the last thirty days, the filter, one app's page,
// the hours — is a window of its own (screen.ts), opened by the section's Open button.

import "./style.css";
import { defineFeature, type FeatureCtx, type SectionHost } from "../../core/feature";
import { Sound } from "../../core/sound";
import { h } from "../../views/dom";
import { language, t, tn } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { btn, card, menu, meter, row, setMeter, slider, stripRow, toggle } from "../../ui/components";
import { CATEGORIES, CATEGORY, HOURGLASS, budgetFrac, dayDate, dayLabel, lastDays, mountScreen, pad, ring, shortSpan, span, type Usage } from "./screen";
import { confirmAction } from "./confirm";

const P = "f-breaks-";

type Kind = "eye" | "move";

/** How a day's breaks ended, by kind (crates/breaks/src/stats.rs). */
interface Counts {
  done: number;
  skip: number;
  snooze: number;
  away: number;
}

/** The `state` event and the answer of every break command (crates/breaks/src/lib.rs). */
interface Status {
  break: { kind: Kind; left: number; total: number } | null;
  /** Seconds at the PC until the next break of the kind; null = that reminder is off. */
  eye: number | null;
  move: number | null;
  held: boolean;
  fade: boolean;
  /** Today's breaks, by kind and by how they ended. */
  counts?: { day: string; eye: Counts; move: Counts };
  /** Seconds left of a pause asked for from the island; 0 when there is none. */
  paused?: number;
  ev?: { phase: "soon" | "held" | "start" | "end" | "budget"; kind: Kind | "screen"; why: string; day?: string; spent?: number; limit?: number; cat?: string | null };
}

const NONE: Status = { break: null, eye: null, move: null, held: false, fade: false };
/** The same as WARN in crates/breaks/src/lib.rs: the notice before the screens fade. */
const WARN = 10;
let status: Status = NONE;
let at = Date.now(); // when `status` was received; a running break counts from here
let sound = true;
/** The island page's context, for the buttons of the strip row. */
let app: FeatureCtx | null = null;
/** The sections, while they are on screen: told when the state changed. */
let drawBreak: (() => void) | undefined;
let loadUsage: (() => void) | undefined;

const TITLE: Record<Kind, string> = { eye: "Rest your eyes", move: "Time to move" };
const LINE: Record<Kind, string> = { eye: "Look at something far away", move: "Stand up and stretch" };
const NAME: Record<Kind, string> = { eye: "Eye breaks", move: "Movement breaks" };
const MARK: Record<Kind, string> = { eye: "eye", move: "coffee" };
// ── readings ─────────────────────────────────────────────────────────────────

/** Whole seconds the break on screen has left. */
const left = () => (status.break ? Math.max(0, Math.ceil(status.break.left - (Date.now() - at) / 1000)) : 0);
/** "0:17", "1:45". */
const clock = (s: number) => `${Math.floor(s / 60)}:${pad(s % 60)}`;
const minutes = (n: number) => t("{count} min", { count: n });
const seconds = (n: number) => t("{count} s", { count: n });
export default defineFeature({
  id: "breaks",

  activities: [
    {
      // A break outranks a timer for the seconds it lasts.
      id: "break",
      order: -1,
      tick: true,
      hold: true,
      section: "break",
      isActive: () => status.break != null,
      render() {
        const b = status.break;
        if (!b) return stripRow({ icon: "eye", title: t("Breaks") });
        const el = stripRow({
          icon: MARK[b.kind],
          tone: "info",
          title: t(TITLE[b.kind]),
          value: clock(left()),
          action: { label: t("Skip"), run: () => void app?.call("skip") },
        });
        // Snooze sits before Skip: the strip has both.
        el.lastElementChild?.before(
          h("button", {
            class: "btn sm ghost",
            text: t("Snooze"),
            onclick: (e: Event) => {
              e.stopPropagation();
              void app?.call("snooze");
            },
          }),
        );
        return el;
      },
    },
  ],

  sections: [
    {
      id: "screen",
      title: "Screen time",
      icon: HOURGLASS,
      letter: "t",
      height: 268,
      keys: [],
      build: screenPage,
    },
    { id: "break", title: "Breaks", icon: "eye", height: 204, build: breakPage },
  ],

  settings: settingsPage,

  // The report, in a window of its own: `window` in crates/breaks/src/lib.rs.
  windows: { screen: mountScreen },

  start(cx) {
    app = cx;
    const take = (s: Status | null) => {
      if (!s) return;
      const was = status.break != null;
      status = s;
      at = Date.now();
      const ev = s.ev;
      if (ev?.phase === "start" && sound) Sound.play("pop");
      // Eyes are elsewhere: the tone is what says the break is over.
      if (ev?.phase === "end" && ev.why === "done" && sound) Sound.play("finish");
      if (ev?.phase === "soon") {
        cx.island.notice({
          kind: "timer",
          title: tn("Break in {count} second", "Break in {count} seconds", WARN),
          detail: t("The screens will fade"),
          icon: MARK[ev.kind as Kind],
          tone: "info",
          section: "break",
          action: { label: t("Skip"), run: () => void cx.call("skip") },
        });
      }
      // The day's target, crossed: one notice a day, and the report is one click away.
      if (ev?.phase === "budget") {
        cx.island.notice({
          kind: "timer",
          title: t("Screen time target reached"),
          detail: ev.cat ? t("The budget for {category} is used up.", { category: t(CATEGORY[ev.cat] ?? "Other") }) : t("The screen-time budget for today is used up."),
          icon: HOURGLASS,
          tone: "warn",
          section: "screen",
          action: { label: t("Open report"), run: () => void cx.call("window", ev.day ? { day: ev.day, cat: "" } : {}) },
        });
      }
      if (was || s.break) cx.island.activityChanged("break");
      drawBreak?.();
      if (!ev) loadUsage?.(); // the settings changed
    };
    void cx.settings.get().then((v) => (sound = v.sound !== false));
    const offs = [cx.settings.onChange((v) => (sound = v.sound !== false)), cx.on<Status>("state", take)];
    void cx.call<Status>("state").then(take);
    return () => {
      offs.forEach((off) => off());
      status = NONE;
      app = null;
    };
  },
});

// ── Breaks section ───────────────────────────────────────────────────────────

function breakPage(cx: FeatureCtx): SectionHost {
  const el = h("div", { class: `${P}brk` });
  let timer: number | undefined;
  let big: HTMLElement | undefined;
  let bar: HTMLElement | undefined;
  let ticks = 0;

  const paint = () => {
    if (big) big.textContent = clock(left());
    if (bar && status.break) setMeter(bar, 1 - left() / status.break.total);
  };

  /** One reminder while no break is on: its switch, when it is next due, and a break by hand. */
  const plan = (kind: Kind) => {
    const next = status[kind];
    const when =
      next == null ? t("Off")
      : next === 0 && status.held ? t("Waiting for a call or a full-screen app to end")
      : t("Next after {count} min at the PC", { count: Math.max(1, Math.ceil(next / 60)) });
    return card([
      h("div", { class: `${P}plan` },
        h("span", { class: `isl-ico${next == null ? "" : " info"}` }, icon(MARK[kind], 14)),
        h("b", { text: t(NAME[kind]) }),
        toggle(next != null, (on) => void cx.settings.patch({ [kind]: on }))),
      h("p", { text: when }),
      btn(t("Take one now"), "secondary", () => void cx.call("take", { kind })),
    ]);
  };

  /** Today's breaks, as counts the person can read, and the pause of their own. */
  const foot = () => {
    const c = status.counts;
    const taken = (c?.eye.done ?? 0) + (c?.move.done ?? 0);
    const skipped = (c?.eye.skip ?? 0) + (c?.move.skip ?? 0);
    const paused = (status.paused ?? 0) > 0;
    return h("div", { class: `${P}foot` },
      h("small", {
        text: taken || skipped
          ? t("Today: {taken} taken · {skipped} skipped", { taken, skipped })
          : t("No breaks taken yet today."),
      }),
      btn(paused ? t("Resume") : t("Pause for 30 min"), "ghost", () =>
        void cx.call<Status>(paused ? "resume" : "pause", paused ? {} : { mins: 30 }).then((s) => {
          if (!s) return;
          status = s;
          at = Date.now();
          draw();
        })));
  };

  const draw = () => {
    big = bar = undefined;
    const b = status.break;
    if (!b) {
      el.replaceChildren(h("div", { class: `${P}plans` }, plan("eye"), plan("move")), foot());
      return;
    }
    big = h("div", { class: `${P}clock` });
    bar = meter(0, "info");
    el.replaceChildren(
      h("div", { class: `${P}now` },
        h("span", { class: "isl-ico info" }, icon(MARK[b.kind], 14)),
        h("b", { "data-shared": "title", text: t(TITLE[b.kind]) }),
        h("span", { text: t(LINE[b.kind]) })),
      big,
      bar,
      h("div", { class: `${P}actions` },
        btn(t("Snooze"), "secondary", () => void cx.call("snooze")),
        btn(t("Skip"), "ghost", () => void cx.call("skip"))),
    );
    paint();
  };

  /** Asks Rust again: the minutes to the next break move only while the person is at the PC. */
  const refresh = () =>
    void cx.call<Status>("state").then((s) => {
      if (!s || s.break || status.break) return;
      status = s;
      at = Date.now();
      draw();
    });

  return {
    el,
    show() {
      drawBreak = draw;
      draw();
      refresh();
      timer = window.setInterval(() => (status.break ? paint() : ++ticks % 20 === 0 && refresh()), 1000);
    },
    hide() {
      drawBreak = undefined;
      clearInterval(timer);
      timer = undefined;
    },
    sync() {},
  };
}

// ── Screen time section ──────────────────────────────────────────────────────

/** One day in three columns: the ring, the apps that took the most, and the week. */
function screenPage(cx: FeatureCtx): SectionHost {
  const el = h("div", { class: `${P}st` });
  let timer: number | undefined;
  /** The day on screen; "" until an answer names it. */
  let day = "";
  /** The category the middle column is narrowed to, or null for all of them. */
  let filter: string | null = null;
  /** The last answer: the filter redraws from it rather than asking Rust again. */
  let shown: Usage | null = null;

  const load = () => void cx.call<Usage>("usage", day ? { day } : {}).then(draw);
  const paint = () => {
    if (shown) draw(shown);
  };
  // What the section shows is remembered by Rust, so the report and the section open together.
  let sent = "";
  const remember = () => {
    const key = `${day}|${filter ?? ""}`;
    if (key === sent) return;
    sent = key;
    void cx.call("keep", { day, cat: filter ?? "" });
  };

  const clearBtn = () => {
    const b = btn(t("Clear"), "ghost", () => void confirmAction().then((yes) => {
      if (yes) void cx.call<Usage>("clear").then(draw);
    }));
    b.classList.add("sm");
    return b;
  };

  /** The report itself, opened on the day and the category the section is showing. */
  const openBtn = () => {
    const b = btn(t("View Details"), "primary", () => void cx.call("window", { day, cat: filter ?? "" }));
    b.classList.add("sm");
    b.title = t("Open the full report");
    return b;
  };

  function draw(u: Usage | null) {
    if (!u) return;
    shown = u;
    day = u.day;
    if (!u.on) {
      el.replaceChildren(
        h("div", { class: `${P}off` },
          h("b", { text: t("Screen time is off") }),
          h("p", { text: t("It counts the time each app is in front: app names, durations and, if you turn them on, window titles. Kept on this PC.") }),
          btn(t("Turn on"), "primary", () => void cx.settings.patch({ screenTime: true }).then(load))),
      );
      return;
    }
    const all = u.apps ?? [];
    const apps = all.filter((a) => !filter || a.cat === filter);
    const total = u.total ?? 0;

    // Left: the ring, the total in its middle, the three largest categories under it. The middle
    // of the ring is every app; a category — its arc or its row — narrows the list to that one.
    const byCat = new Map<string, number>();
    for (const a of all) byCat.set(a.cat, (byCat.get(a.cat) ?? 0) + a.secs);
    const parts = [...byCat].sort((a, b) => b[1] - a[1]).map(([cat, secs]) => ({ cat, frac: total ? secs / total : 0 }));
    const target = u.budget?.mins ? budgetFrac(u.budget.spent, u.budget.mins) : undefined;
    const dial = h("div", { class: `${P}dial`, title: filter ? t("Show all apps") : "" },
      ring(parts, (cat) => {
        filter = cat;
        paint();
      }, target),
      h("div", {},
        h("b", { text: span(total) }),
        h("small", { text: day === u.today ? t("today") : dayLabel(day, u.today) }),
        u.budget?.mins
          ? h("small", { class: `${P}budget${target && target >= 1 ? " over" : ""}`, text: t("Goal: {count}", { count: shortSpan(u.budget.mins * 60) }) })
          : null));
    dial.addEventListener("click", () => {
      if (!filter) return;
      filter = null;
      paint();
    });
    const overview = h("div", { class: `${P}overview` },
      dial,
      ...parts.slice(0, 3).map((p) =>
        h("button", {
          class: `${P}key ${P}c-${p.cat}${filter === p.cat ? " on" : ""}`,
          title: t("Filter by category"),
          onclick: () => {
            filter = filter === p.cat ? null : p.cat;
            paint();
          },
        },
        h("i"),
        h("span", { text: t(CATEGORY[p.cat] ?? "Other") }),
        h("small", { text: t("{count}%", { count: Math.round(p.frac * 100) }) }))),
    );

    // Middle: the apps by time; a long tail folds into one row.
    const top = apps.slice(0, apps.length > 6 ? 5 : 6);
    const tail = apps.slice(top.length);
    const rows = top.map((a) => ({
      name: a.key === "desktop" ? t("Desktop") : a.name,
      title: u.titles ? a.title ?? "" : "",
      secs: a.secs,
      cat: a.cat,
    }));
    if (tail.length) {
      rows.push({ name: tn("{count} other app", "{count} other apps", tail.length), title: "", secs: tail.reduce((n, a) => n + a.secs, 0), cat: "other" });
    }
    const most = Math.max(1, ...rows.map((r) => r.secs));
    const ranked = h("div", { class: `${P}apps` },
      h("div", { class: `${P}head` },
        h("span", { text: filter ? t(CATEGORY[filter] ?? "Other") : t("Apps") }),
        h("small", { text: tn("{count} switch", "{count} switches", u.switches ?? 0) })),
      ...(rows.length
        ? rows.map((r) =>
            h("div", { class: `${P}app ${P}c-${r.cat}` },
              h("i"),
              h("span", { text: r.name, title: r.title || r.name }),
              meter(r.secs / most),
              h("small", { text: span(r.secs) })))
        : [h("p", { text: day === u.today ? t("Nothing counted yet today.") : t("Nothing was recorded this day.") })]),
    );

    // Right: the last seven days as bars, today last, and the two ways on: the report, and Clear.
    // A bar is how a day is chosen: the ring, the list and the total below it become that day's.
    const narrow = new Intl.DateTimeFormat(language(), { weekday: "narrow" });
    // The week comes from Rust's own today, not the page's clock: across midnight they differ.
    const days = lastDays(u.today, 7).map((key) => ({ d: dayDate(key), key, secs: u.days?.[key]?.secs ?? 0 }));
    const tallest = Math.max(1, ...days.map((x) => x.secs));
    const week = h("div", { class: `${P}history` },
      h("div", { class: `${P}head` }, h("span", { text: t("Last 7 days") })),
      h("div", { class: `${P}week` },
        ...days.map((x) =>
          h("button", {
            class: `${P}day${x.key === u.today ? " today" : ""}${x.key === u.day ? " on" : ""}`,
            title: `${dayLabel(x.key, u.today)} · ${span(x.secs)}`,
            onclick: () => {
              day = x.key;
              load();
            },
          },
          h("i", { style: `height:${Math.max(2, Math.round((x.secs / tallest) * 100))}%` }),
          h("small", { text: narrow.format(x.d) })))),
      h("div", { class: `${P}acts` }, openBtn(), clearBtn()),
    );

    el.replaceChildren(overview, ranked, week);
    remember();
  }

  return {
    el,
    show() {
      loadUsage = load;
      // The section opens on the day the report was left on, not always at today.
      void cx.call<{ day: string }>("report").then((r) => {
        if (r?.day) day = r.day;
        load();
      });
      // Today is still being counted; a day that is over does not change under the reader.
      timer = window.setInterval(() => {
        if (shown && day === shown.today) load();
      }, 10_000);
    },
    hide() {
      loadUsage = undefined;
      clearInterval(timer);
      timer = undefined;
    },
    sync() {},
  };
}

// ── Settings ─────────────────────────────────────────────────────────────────

function settingsPage(cx: FeatureCtx): HTMLElement {
  const box = h("div", { class: `${P}settings` });
  void cx.settings.get().then((s) => {
    const sw = (key: string) => toggle(s[key] === true, (on) => void cx.settings.patch({ [key]: on }));
    /** A number as a slider with its reading: saved when the slider is let go. */
    const num = (key: string, label: string, min: number, max: number, step: number, unit: (n: number) => string) => {
      let v = Math.min(max, Math.max(min, Number(s[key]) || min));
      const out = h("b", { text: unit(v) });
      const set = (f: number) => {
        v = Math.round((min + f * (max - min)) / step) * step;
        out.textContent = unit(v);
      };
      const bar = slider((v - min) / (max - min), {
        label,
        onInput: set,
        onChange: (f) => {
          set(f);
          void cx.settings.patch({ [key]: v });
        },
      });
      return h("span", { class: `${P}num` }, bar, out);
    };
    /** How long the history is kept: 30 days unless the person picks another window. */
    const keepPick = () => {
      const options: [number, string][] = [
        [30, t("Keep {count} days", { count: 30 })],
        [90, t("Keep {count} days", { count: 90 })],
        [180, t("Keep {count} days", { count: 180 })],
        [365, t("Keep {count} days", { count: 365 })],
        [0, t("Unlimited")],
      ];
      const select = h("select", {}, ...options.map(([value, name]) => h("option", { value: String(value), text: name })));
      select.value = String(s.historyDays ?? 30);
      select.addEventListener("change", () => void cx.settings.patch({ historyDays: Number(select.value) }));
      return h("label", { class: "field" }, select);
    };
    // Nothing under Clear can be taken back, so it asks first.
    const clear = btn(t("Clear"), "danger", () => {
      void confirmAction().then((yes) => {
        if (yes) void cx.call("clear");
      });
    });

    /** A clock time, patched when it is left. */
    const time = (key: string) => {
      const input = h("input", {
        class: `${P}time`,
        type: "time",
        value: String(s[key] ?? ""),
        "aria-label": t(key === "quietFrom" ? "Quiet hours start" : "Quiet hours end"),
      });
      input.addEventListener("change", () => void cx.settings.patch({ [key]: input.value }));
      return input;
    };

    /** The category a budget is about; every app of the day when it is "". */
    const cat = () => String(s.budgetCat ?? "");
    const budgetCat = btn(cat() ? t(CATEGORY[cat()] ?? "Other") : t("All apps"), "secondary", () => {
      const box = budgetCat.getBoundingClientRect();
      // The row shows what was picked at once; the settings are what Rust reads.
      const pick = (value: string, label: string) => {
        if (budgetCat.firstElementChild) budgetCat.firstElementChild.textContent = label;
        void cx.settings.patch({ budgetCat: value });
      };
      menu(
        [
          ...CATEGORIES.map((c) => ({ label: t(CATEGORY[c]), onClick: () => pick(c, t(CATEGORY[c])) })),
          null,
          { label: t("All apps"), onClick: () => pick("", t("All apps")) },
        ],
        { x: box.left, y: box.bottom + 4 },
      );
    });
    budgetCat.classList.add("sm");

    // A copy of the whole history, in the feature's own folder, which is then shown.
    const exported = h("small", { class: `${P}exported` });
    const exportBtn = btn(t("Export a copy"), "secondary", () =>
      void cx.call<{ folder: string }>("export").then((r) => {
        if (r) exported.textContent = t("Saved to {folder}", { folder: r.folder });
      }));

    // The apps the person hid: one chip each, with the way back.
    const hidden = Array.isArray(s.hidden) ? (s.hidden as string[]) : [];
    const chips = h("span", { class: `${P}chips` });
    const paintHidden = (keys: string[]) => {
      chips.replaceChildren(
        ...(keys.length
          ? keys.map((key) =>
              h("button", {
                class: `${P}chip`,
                title: t("Show again"),
                onclick: () => void cx.call("unhide", { key }).then(() => paintHidden(keys.filter((k) => k !== key))),
              },
              h("span", { text: key }),
              icon("close", 11)))
          : [h("small", { text: t("No app is hidden.") })]),
      );
    };
    paintHidden(hidden);

    box.append(
      h("div", { class: "section-title", text: t("Eye breaks") }),
      card([
        row(t("Eye breaks"), sw("eye"), t("The 20-20-20 habit: every 20 minutes, look at something far away for 20 seconds.")),
        row(t("Break every"), num("eyeEvery", t("Eye breaks"), 5, 60, 5, minutes), t("Only time at the PC counts. Being away for as long as a break counts as having taken it.")),
        row(t("Break lasts"), num("eyeSecs", t("Eye breaks"), 10, 60, 5, seconds)),
      ]),
      h("div", { class: "section-title", text: t("Movement breaks") }),
      card([
        row(t("Movement breaks"), sw("move"), t("A reminder to stand up, on its own interval.")),
        row(t("Break every"), num("moveEvery", t("Movement breaks"), 15, 120, 5, minutes)),
        row(t("Break lasts"), num("moveMins", t("Movement breaks"), 1, 10, 1, minutes)),
      ]),
      h("div", { class: "section-title", text: t("During a break") }),
      card([
        row(t("Fade the screens"), sw("fade"), t("Every screen fades to black with the countdown, after a 10-second notice on the island. Esc or a click ends it at once.")),
        row(t("Sound"), sw("sound"), t("A soft tone when a break starts and when it ends.")),
        row(t("Snooze for"), num("snooze", t("Snooze for"), 1, 15, 1, minutes)),
        row(t("Wait while on a call"), sw("holdCall"), t("A break waits while an app has the microphone open.")),
        row(t("Wait while something is full screen"), sw("holdFullscreen"), t("A film, a game or a presentation is not interrupted.")),
        row(t("Quiet hours"), sw("quietHours"), t("No fade between these times: meetings and deep work. A window past midnight works too.")),
        row(t("From – to"), h("span", { class: `${P}times` }, time("quietFrom"), h("span", { text: "–" }), time("quietTo"))),
        row(t("Wait during Windows quiet hours"), sw("quietSystem"), t("Focus Assist and presentations: a fade would be the interruption.")),
        row(t("Try it"),
          h("span", { class: `${P}try` },
            btn(t("Eye break"), "secondary", () => void cx.call("take", { kind: "eye" })),
            btn(t("Movement break"), "secondary", () => void cx.call("take", { kind: "move" })))),
      ]),
      h("div", { class: "section-title", text: t("Screen time") }),
      card([
        row(t("Screen time"), sw("screenTime"), t("Counts the time each app is in front and shows it on the island, with each program's own file for its icon. Kept on this PC.")),
        row(t("Show window titles"), sw("windowTitles"), t("Keep the title of the window in front with its app in Screen time, such as the document or page it has open. Off by default: a title can name a document, a page or a message.")),
        row(t("App icons"), sw("icons"), t("Read each program's own icon for the list. Its file is kept on this PC with the app's name and time.")),
        row(t("Daily budget"), num("budget", t("Daily budget"), 0, 720, 30, (n) => (n === 0 ? t("Off") : minutes(n))), t("A second, thinner ring shows the day against it, and one notice says so when it is reached. Off by default.")),
        row(t("Budget applies to"), budgetCat, t("The whole day, or one category of it.")),
        row(t("Away after"), num("idleMins", t("Away after"), 1, 30, 1, minutes), t("No key and no pointer this long: the time counted so far is taken back and nothing more is counted. Fullscreen apps count as idle too.")),
        row(t("Hidden apps"), chips, t("An app left out of the list; its time still counts in the day's total.")),
        row(t("Keep history for"), keepPick(), t("Days of screen time and break counts kept on this PC: 30 unless you change it. Unlimited keeps every day until you delete it.")),
        row(t("Export a copy"), h("span", { class: `${P}export` }, exportBtn, exported), t("A CSV and a JSON of every day kept, written to the feature's own folder.")),
        row(t("History"), h("span", { class: `${P}try` }, clear), t("The screen-time history kept on this PC.")),
      ]),
    );
  });
  return box;
}
