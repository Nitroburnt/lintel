// Breaks — the Screen time report in a window of its own (crates/breaks/src/lib.rs, `window`).
//
// The island section is a three-column summary of one day; this page is what does not fit there:
// the day and its arrows, every app of it, the category filter, one app's own page, and the hours
// or the last fourteen days. It is a window because the report needs the room, and a window the
// person can resize (see `.f-breaks-wgrip`) rather than a fixed island panel.

import { getCurrentWindow } from "@tauri-apps/api/window";
import type { FeatureCtx } from "../../core/feature";
import { h } from "../../views/dom";
import { language, t, tn, weekdayShort } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { btn, field, iconBtn, menu, meter } from "../../ui/components";
import { confirmAction } from "./confirm";

const P = "f-breaks-";

/** The days the chart draws. */
export const SPAN = 14;
/** The same as LINTEL in crates/breaks/src/usage.rs: this app's own row, and never a program. */
export const LINTEL = "lintel";

/** An hourglass: the Screen time tab and the window's title bar. */
export const HOURGLASS = "M6.5 3.5h11M6.5 20.5h11M8 3.5c0 4.5 4 5.5 4 8.5s-4 4-4 8.5M16 3.5c0 4.5-4 5.5-4 8.5s4 4 4 8.5";
/** Arrows of this feature's own (src/ui/icons.ts has none narrow enough for a day header). */
export const CHEV_L = "M14.5 6.5L9 12l5.5 5.5";
export const CHEV_R = "M9.5 6.5L15 12l-5.5 5.5";

export const CATEGORIES = ["development", "communication", "browsing", "entertainment", "productivity", "design", "utilities", "other"];
export const CATEGORY: Record<string, string> = {
  development: "Development",
  communication: "Communication",
  browsing: "Browsing",
  entertainment: "Entertainment",
  productivity: "Productivity",
  design: "Design",
  utilities: "Utilities",
  other: "Other",
};

/** One row of a day's apps, as `usage` sends it. */
export interface UsageApp {
  key: string;
  name: string;
  /** The program's file; empty when Windows would not say. Icons and "Open" need it. */
  path: string;
  /** The title of its window when it came to the front; "" unless the switch is on. */
  title?: string;
  secs: number;
  cat: string;
}

/** A day of screen time (crates/breaks/src/usage.rs, `snapshot`). */
export interface Usage {
  on: boolean;
  /** The local date of today, whatever day was asked for. */
  today: string;
  day: string;
  total: number;
  switches: number;
  apps: UsageApp[];
  /** Seconds by local hour, 24 of them. */
  hours: number[];
  /** Every day kept, by date. A date that is not there has no record. */
  days: Record<string, { secs: number; switches: number }>;
  hidden: string[];
  cats: Record<string, string>;
  icons: boolean;
  /** Window titles are read and may be shown. Off unless the person turned it on. */
  titles?: boolean;
  /** The day's screen-time target: 0 minutes when there is none, and the category it is about. */
  budget?: { mins: number; cat: string | null; spent: number };
  /** A clear or a delete is still kept aside by Rust for a few seconds. */
  undo?: boolean;
}

/** One program on one day (crates/breaks/src/usage.rs, `app`). */
export interface AppPage {
  key: string;
  name: string;
  path: string;
  title?: string;
  cat: string;
  day: string;
  secs: number;
  total: number;
  share: number;
  days: Record<string, number>;
  /** Time by exact window title for the selected day, with merged UTC time intervals. */
  windows?: { title: string; secs: number; intervals: { start: number; end: number }[] }[];
}

// ── readings ─────────────────────────────────────────────────────────────────

export const pad = (n: number) => String(n).padStart(2, "0");

/** "3 h 12 min", "12 min", "45 s". */
export function span(secs: number): string {
  const m = Math.floor(secs / 60);
  if (m >= 60) return t("{h} h {m} min", { h: Math.floor(m / 60), m: m % 60, count: Math.floor(m / 60) });
  return m >= 1 ? t("{count} min", { count: m }) : t("{count} s", { count: secs });
}

/** "2026-11-15" for a local day. */
export const dateKey = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
export const dayDate = (key: string) => {
  const [y, m, d] = key.split("-").map(Number);
  return new Date(y, m - 1, d);
};
export const shiftDay = (key: string, n: number) => {
  const d = dayDate(key);
  d.setDate(d.getDate() + n);
  return dateKey(d);
};

/** The last `count` local days ending at `today`, oldest first: the week the islands draw. */
export function lastDays(today: string, count: number): string[] {
  return Array.from({ length: count }, (_, i) => shiftDay(today, -(count - 1 - i)));
}

/** The oldest day the history still keeps, or today when nothing is kept: where navigation stops. */
export function oldestDay(usage: Usage | null | undefined, today: string): string {
  const days = usage ? Object.keys(usage.days ?? {}).sort() : [];
  return days[0] ?? today;
}

/** Whether an app is what was typed into the finder: its name, its key, or its category. */
export function matches(query: string, app: Pick<UsageApp, "name" | "key" | "cat">): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  return app.name.toLowerCase().includes(q) || app.key.toLowerCase().includes(q) || app.cat.includes(q);
}

/** The share of the day's budget that is used, 0…1; 0 when no budget is set. */
export function budgetFrac(spent: number, mins: number): number {
  if (!mins || mins <= 0) return 0;
  return Math.max(0, spent / (mins * 60));
}

/** "1h 30m", "45m", "30s": a target, short enough to sit inside the ring. */
export function shortSpan(secs: number): string {
  const m = Math.floor(secs / 60);
  if (m >= 60) return `${Math.floor(m / 60)}h ${m % 60}m`;
  return m >= 1 ? `${m}m` : `${secs}s`;
}

/** "Today", "Yesterday", or "Fri 14 Nov". */
export function dayLabel(day: string, today: string): string {
  if (day === today) return t("Today");
  if (day === shiftDay(today, -1)) return t("Yesterday");
  return new Intl.DateTimeFormat(language(), { weekday: "short", day: "numeric", month: "short" }).format(dayDate(day));
}

/**
 * The category ring: one arc per category, in the token colours of style.css. With `pick`, an arc
 * is a button for its category, and a click anywhere inside the ring that is not on an arc — the
 * middle, where the total is — is left to the caller's own handler: that is "every app".
 */
export function ring(parts: { cat: string; frac: number }[], pick?: (cat: string) => void, budget?: number): SVGSVGElement {
  const NS = "http://www.w3.org/2000/svg";
  const el = document.createElementNS(NS, "svg");
  el.setAttribute("viewBox", "0 0 36 36");
  el.setAttribute("class", `${P}ring${pick ? " pick" : ""}`);
  el.setAttribute("aria-hidden", "true");
  // A second, thinner ring well inside the day's: the goal, filled as the day goes.
  if (budget !== undefined) {
    const inner = document.createElementNS(NS, "circle");
    const len = Math.min(1, Math.max(0, budget)) * 100;
    const attrs = { cx: 18, cy: 18, r: 12.6, class: `${P}limit${len >= 100 ? " over" : ""}`, "stroke-dasharray": `${len} ${100 - len}`, "stroke-dashoffset": 0 };
    for (const [k, v] of Object.entries(attrs)) inner.setAttribute(k, String(v));
    el.append(inner);
  }
  // r = 100 / 2π: the circle is 100 long, so an arc's length is its percentage.
  const arc = (cls: string, len: number, from: number, cat?: string) => {
    const c = document.createElementNS(NS, "circle");
    const attrs = { cx: 18, cy: 18, r: 15.9155, class: cls, "stroke-dasharray": `${len} ${100 - len}`, "stroke-dashoffset": -from };
    for (const [k, v] of Object.entries(attrs)) c.setAttribute(k, String(v));
    if (pick && cat) {
      c.addEventListener("click", (e) => {
        // Not the ring's middle: the caller's handler shows every app again.
        e.stopPropagation();
        pick(cat);
      });
    }
    el.append(c);
  };
  arc(`${P}track`, 100, 0);
  let from = 0;
  for (const p of parts) {
    const len = p.frac * 100;
    // A hair of space between two arcs; a sliver stays visible.
    arc(`${P}c-${p.cat}`, Math.max(0.6, len - (parts.length > 1 ? 0.9 : 0)), from, p.cat);
    from += len;
  }
  return el;
}

// ── The window ───────────────────────────────────────────────────────────────

export function mountScreen(root: HTMLElement, cx: FeatureCtx) {
  const bar = h("div", { class: "win-bar" },
    icon(HOURGLASS, 14),
    h("span", { text: t("Screen time") }),
    h("button", { class: "close", title: t("Close"), text: "×", onclick: () => void cx.call("close") }));
  const body = h("div", { class: `${P}wbody` });
  // The window is undecorated, like every window of the app: a corner to pull says it resizes.
  const grip = h("div", { class: `${P}wgrip`, title: t("Drag a corner to resize") });
  grip.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    void getCurrentWindow().startResizeDragging("SouthEast").catch(() => undefined);
  });
  root.append(h("div", { class: `win ${P}win` }, bar, body, grip));

  let usage: Usage | null = null;
  /** The day on screen; "" until the first answer names it. */
  let day = "";
  /** The category the list is narrowed to, or null for all of them. */
  let filter: string | null = null;
  /** What was typed into the finder; "" shows every app of the day. */
  let find = "";
  /** The row the arrow keys are on. */
  let cursor = 0;
  /** The app whose own page is open. */
  let detail: string | null = null;
  let pageData: AppPage | null = null;
  let chart: "days" | "hours" = "days";
  let timer: number | undefined;
  let listEl: HTMLElement | null = null;
  let countEl: HTMLElement | null = null;
  /** Programs' icons as data URLs, by key. `tries` asks a missing one again a few times: the
      first answer can be a moment early (a path learned late, a shell that had not answered). */
  const images = new Map<string, string>();
  const tries = new Map<string, number>();
  /** How often a key without an icon is asked for before its tile stays. */
  const TRIES = 3;
  /** The finder of the app list: one field for the window's life, so typing is not interrupted. */
  const finder = field({
    placeholder: t("Find an app"),
    icon: "search",
    onInput: (value) => {
      find = value;
      cursor = 0;
      paintList();
    },
  });
  finder.classList.add(`${P}wfind`);

  const visible = () => (usage?.apps ?? []).filter((a) => (!filter || a.cat === filter) && matches(find, a));
  const dayNow = () => day || usage?.today || dateKey(new Date());

  /** What is on screen, kept in Rust: closing the window and opening it again shows the same day,
      and the island's own Open starts where the report was left. */
  let sent = "";
  const remember = () => {
    const key = `${day}|${filter ?? ""}`;
    if (key === sent) return;
    sent = key;
    void cx.call("keep", { day, cat: filter ?? "" });
  };

  // ── Asking Rust ──

  const load = () => void cx.call<Usage>("usage", day ? { day } : {}).then(take);

  /** The rows of the list, from the last answer and what is typed: typing does not redraw the day. */
  const paintList = () => {
    const u = usage;
    if (!u || !listEl) return;
    const apps = visible();
    const total = u.total;
    const most = Math.max(1, ...apps.map((a) => a.secs));
    if (cursor >= apps.length) cursor = Math.max(0, apps.length - 1);
    listEl.replaceChildren(
      ...(apps.length
        ? apps.map((a, i) =>
            h("button", {
              class: `${P}wrow ${P}c-${a.cat}${i === cursor ? " on" : ""}`,
              title: a.path || a.name,
              onclick: () => {
                cursor = i;
                openApp(a.key);
              },
            },
            mark(a),
            h("span", { class: `${P}wtext` },
              h("span", { class: `${P}wname`, text: a.key === "desktop" ? t("Desktop") : a.name }),
              u.titles && a.title ? h("span", { class: `${P}wtitle`, text: a.title, title: a.title }) : null),
            meter(a.secs / most),
            h("small", { text: span(a.secs) }),
            h("em", { text: total ? t("{count}%", { count: Math.round((a.secs / total) * 100) }) : "" })))
        : [h("p", { class: `${P}wempty`, text: find ? t("No app matches.") : (day || u.today) === u.today ? t("Nothing counted yet today.") : t("Nothing was recorded this day.") })]),
    );
    if (countEl) countEl.textContent = tn("{count} app", "{count} apps", apps.length);
  };

  const take = (u: Usage | null) => {
    if (!u) return;
    usage = u;
    day = u.day;
    if (cursor >= visible().length) cursor = Math.max(0, visible().length - 1);
    draw();
    if (detail) void loadDetail();
    loadIcons();
  };

  const loadDetail = () => {
    const key = detail;
    if (!key) return;
    void cx.call<AppPage>("app", { key, day }).then((p) => {
      if (!p || detail !== key) return;
      pageData = p;
      draw();
    });
  };

  const loadIcons = () => {
    const u = usage;
    if (!u || !u.icons) return;
    const keys = [...visible().map((a) => a.key), ...(detail ? [detail] : [])];
    const want = [...new Set(keys)].filter((k) => !images.has(k) && (tries.get(k) ?? 0) < TRIES);
    if (!want.length) return;
    for (const k of want) tries.set(k, (tries.get(k) ?? 0) + 1);
    void cx.call<Record<string, string>>("icons", { keys: want }).then((map) => {
      if (!map) return;
      let added = false;
      for (const [k, url] of Object.entries(map)) {
        images.set(k, url);
        added = true;
      }
      if (added) draw();
    });
  };

  const go = (n: number) => {
    const today = usage?.today ?? dateKey(new Date());
    const next = shiftDay(dayNow(), n);
    if (next > today || next < oldestDay(usage, today)) return;
    day = next;
    cursor = 0;
    void load();
  };

  const openApp = (key: string) => {
    detail = key;
    pageData = null;
    draw();
    loadDetail();
    loadIcons();
  };

  const closeApp = () => {
    detail = null;
    pageData = null;
    draw();
  };

  const moveCursor = (n: number) => {
    const count = visible().length;
    if (!count) return;
    cursor = (cursor + n + count) % count;
    draw();
    listEl?.children[cursor]?.scrollIntoView({ block: "nearest" });
  };

  /** A command of the person's own, from the app's page: reload rather than take Rust's answer. */
  const command = (method: string, args: object) => {
    void cx.call(method, args).then(() => {
      load();
      if (detail) loadDetail();
    });
  };

  const clearBtn = () => {
    const b = btn(t("Clear"), "ghost", () => void confirmAction().then((yes) => {
      if (yes) void cx.call<Usage>("clear").then(take);
    }));
    b.classList.add("sm");
    return b;
  };

  /** One day gone, the rest kept; the window moves to today. */
  const deleteBtn = () => {
    const b = btn(t("Delete day"), "ghost", () => void confirmAction().then((yes) => {
      if (!yes) return;
      const gone = day || usage?.today || "";
      day = usage?.today ?? dateKey(new Date());
      void cx.call<Usage>("forgetDay", { day: gone }).then(take);
    }));
    b.classList.add("sm");
    b.title = t("Delete this day from the history");
    return b;
  };

  /** A program whose icon could not be read gets a mark of its own rather than a bare dot. */
  const tile = (a: UsageApp) => {
    if (a.key === "desktop") return h("span", { class: `${P}wtile ${P}c-${a.cat}` }, icon("monitor", 13));
    const name = a.name.trim();
    return h("span", { class: `${P}wtile ${P}c-${a.cat}`, text: (name[0] ?? "?").toUpperCase() });
  };

  /** The icon of a list row, or its tile until the icon arrives. */
  const mark = (a: UsageApp) =>
    images.has(a.key) ? h("img", { class: `${P}wico`, src: images.get(a.key), alt: "" }) : tile(a);

  /** Bars, one per entry: the height is `--h`, the label under it may be empty. */
  const bars = (entries: { key: string; label: string; title: string; frac: number; state: string; onClick?: () => void }[]) =>
    h("div", { class: `${P}wbars` },
      ...entries.map((b) =>
        b.onClick
          ? h("button", { class: `${P}wcol${b.state}`, title: b.title, style: `--h:${Math.max(2, Math.round(b.frac * 100))}%`, onclick: b.onClick },
              h("i"), h("small", { text: b.label }))
          : h("div", { class: `${P}wcol${b.state}`, title: b.title },
              h("i", { style: `height:${Math.max(2, Math.round(b.frac * 100))}%` }), h("small", { text: b.label }))));

  // ── The day's calendar ──

  /** The open calendar, if one is. The native date picker is drawn by Windows, in Windows' own
      theme; this one is drawn with the app's tokens, in the app's language. */
  let cal: HTMLElement | null = null;
  /** When the last calendar closed: the click that closed it must not open it again. */
  let closedAt = 0;

  const closeCal = () => {
    if (!cal) return;
    cal.remove();
    cal = null;
    closedAt = performance.now();
    document.removeEventListener("pointerdown", outsideCal, true);
  };

  function outsideCal(e: Event) {
    if (cal && !cal.contains(e.target as Node)) closeCal();
  }

  const openCal = (anchor: HTMLElement) => {
    if (cal) {
      closeCal();
      return;
    }
    // The pointerdown that closed it has already been; this is the click on the same label.
    if (performance.now() - closedAt < 250) return;
    const today = usage?.today ?? dateKey(new Date());
    const first = oldestDay(usage, today);
    const chosen = day || today;
    // The month on screen: the day's own, and it moves without moving the day until a date is picked.
    let month = chosen.slice(0, 7);
    const box = h("div", { class: `${P}cal`, role: "dialog", "aria-label": t("Pick a day") });
    const narrow = new Intl.DateTimeFormat(language(), { weekday: "narrow" });
    // 4 January 2026 is a Sunday: the header starts on the same weekday as the grid.
    const week = Array.from({ length: 7 }, (_, i) => narrow.format(new Date(2026, 0, 4 + i)));
    const monthName = new Intl.DateTimeFormat(language(), { month: "long", year: "numeric" });

    const paint = () => {
      const start = dayDate(`${month}-01`);
      // The blanks before the 1st: the weekday it falls on, Sunday first, as `getDay` counts.
      const lead = start.getDay();
      const count = new Date(start.getFullYear(), start.getMonth() + 1, 0).getDate();
      const left = iconBtn(CHEV_L, t("Previous month"), () => {
        month = shiftDay(dateKey(start), -start.getDate()).slice(0, 7);
        paint();
      });
      left.disabled = month <= first.slice(0, 7);
      const right = iconBtn(CHEV_R, t("Next month"), () => {
        month = shiftDay(dateKey(start), count - start.getDate() + 1).slice(0, 7);
        paint();
      });
      right.disabled = month >= today.slice(0, 7);
      const cell = (n: number) => {
        if (n < 1 || n > count) return h("span", { class: `${P}calday empty` });
        const key = `${month}-${pad(n)}`;
        const b = h("button", {
          class: `${P}calday${key === chosen ? " on" : ""}${key === today ? " today" : ""}`,
          text: String(n),
          title: dayLabel(key, today),
          onclick: () => {
            closeCal();
            if (key === day) return;
            day = key;
            cursor = 0;
            void load();
          },
        });
        // A day outside the kept history is shown and not offered.
        b.disabled = key < first || key > today;
        return b;
      };
      box.replaceChildren(
        h("div", { class: `${P}calhead` }, left, h("b", { text: monthName.format(start) }), right),
        h("div", { class: `${P}calweek` }, ...week.map((w) => h("small", { text: w }))),
        // Six rows whatever the month, so the box never jumps between one month and the next.
        h("div", { class: `${P}calgrid` }, ...Array.from({ length: 42 }, (_, i) => cell(i - lead + 1))),
      );
    };

    paint();
    document.body.append(box);
    const r = anchor.getBoundingClientRect();
    box.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - box.offsetWidth - 8))}px`;
    box.style.top = `${r.bottom + 6}px`;
    cal = box;
    document.addEventListener("pointerdown", outsideCal, true);
  };

  // ── Drawing: the day ──

  function drawDay(u: Usage) {
    const today = u.today;
    const dayKey = day || today;
    const all = u.apps;
    const total = u.total;

    const first = oldestDay(u, today);
    const prev = iconBtn(CHEV_L, t("Previous day"), () => go(-1));
    const next = iconBtn(CHEV_R, t("Next day"), () => go(1));
    next.disabled = dayKey >= today;
    prev.disabled = dayKey <= first;
    // The day itself opens a month of it: the app's own calendar, not Windows'.
    const date = h("span", {
      class: `${P}wday`,
      title: t("Pick a day"),
      "aria-haspopup": "dialog",
      onclick: () => openCal(date),
    }, h("b", { class: `${P}wdate`, text: dayLabel(dayKey, today) }));
    const seg = h("span", { class: `${P}wseg` },
      h("button", { class: `btn sm ${chart === "hours" ? "secondary" : "ghost"}`, text: t("Hours"), onclick: () => { chart = "hours"; draw(); } }),
      h("button", { class: `btn sm ${chart === "days" ? "secondary" : "ghost"}`, text: t("Days"), onclick: () => { chart = "days"; draw(); } }));
    const off = u.on ? null : h("span", { class: `${P}woff` }, h("b", { text: t("Screen time is off") }));

    const top = h("div", { class: `${P}wtop` },
      prev,
      date,
      next,
      h("span", { class: `${P}wsum`, text: total ? `${span(total)} · ${tn("{count} app", "{count} apps", all.length)} · ${tn("{count} switch", "{count} switches", u.switches)}` : "" }),
      h("span", { class: `${P}wgap` }),
      off,
      seg,
      deleteBtn(),
      clearBtn());

    // The categories: the ring with the day's total in it, and a row each that narrows the list.
    const byCat = new Map<string, number>();
    for (const a of all) byCat.set(a.cat, (byCat.get(a.cat) ?? 0) + a.secs);
    const parts = [...byCat].sort((a, b) => b[1] - a[1]).map(([cat, secs]) => ({ cat, frac: total ? secs / total : 0 }));
    const target = u.budget?.mins ? budgetFrac(u.budget.spent, u.budget.mins) : undefined;
    const dial = h("div", { class: `${P}wdial`, title: filter ? t("Show all apps") : "" },
      ring(parts, (cat) => {
        filter = cat;
        cursor = 0;
        draw();
      }, target),
      h("div", {},
        h("b", { text: span(total) }),
        h("small", { text: dayKey === today ? t("today") : weekdayShort(dayDate(dayKey).getDay()) }),
        u.budget?.mins
          ? h("small", { class: `${P}wbudget${target && target >= 1 ? " over" : ""}`, text: t("Goal: {count}", { count: shortSpan(u.budget.mins * 60) }) })
          : null));
    // The middle of the ring, and the space inside it, is every app again.
    dial.addEventListener("click", () => {
      if (!filter) return;
      filter = null;
      cursor = 0;
      draw();
    });
    const legend = h("div", { class: `${P}wlegend` },
      ...parts.map((p) =>
        h("button", {
          class: `${P}wkey ${P}c-${p.cat}${filter === p.cat ? " on" : ""}`,
          title: t("Filter by category"),
          onclick: () => {
            filter = filter === p.cat ? null : p.cat;
            cursor = 0;
            draw();
          },
        },
        h("i"),
        h("span", { text: t(CATEGORY[p.cat] ?? "Other") }),
        h("small", { text: t("{count}%", { count: Math.round(p.frac * 100) }) }))));

    // Every app of the day, by time: nothing folded away, the list scrolls. Typing in the finder
    // rebuilds these rows only, so the field keeps its focus and its caret.
    countEl = h("small", { text: tn("{count} app", "{count} apps", visible().length) });
    listEl = h("div", { class: `${P}wlist` });
    finder.input.value = find;
    paintList();

    // The chart: the hours of this day, or the days around it. A day bar is how a day is chosen.
    const days = Array.from({ length: SPAN }, (_, i) => shiftDay(today, -(SPAN - 1 - i)));
    const tallest = Math.max(1, ...days.map((d) => u.days[d]?.secs ?? 0));
    const hourTop = Math.max(1, ...u.hours);
    const drawn =
      chart === "days"
        ? bars(days.map((d) => {
            const rec = u.days[d];
            return {
              key: d,
              label: weekdayShort(dayDate(d).getDay()),
              title: `${dayLabel(d, today)} · ${rec ? span(rec.secs) : t("Nothing was recorded this day.")}`,
              frac: (rec?.secs ?? 0) / tallest,
              state: `${d === today ? " today" : ""}${d === dayKey ? " on" : ""}${rec ? "" : " empty"}`,
              onClick: () => {
                day = d;
                cursor = 0;
                void load();
              },
            };
          }))
        : h("div", { class: `${P}whours` },
            ...u.hours.map((secs, hour) =>
              h("div", { class: `${P}whour`, title: `${pad(hour)}:00 · ${span(secs)}` },
                h("i", { style: `height:${Math.max(2, Math.round((secs / hourTop) * 100))}%` }),
                h("small", { text: pad(hour) }))));

    const chartBox = h("div", { class: `${P}wchart` },
      h("div", { class: `${P}whead` }, h("span", { text: chart === "days" ? t("Last 14 days") : t("By hour") })),
      drawn);

    body.replaceChildren(
      h("div", { class: `${P}wreport` },
        top,
        h("div", { class: `${P}wmain` },
          h("div", { class: `${P}wleft` }, dial, legend),
          h("div", { class: `${P}wapps` },
            h("div", { class: `${P}whead` }, h("span", { text: filter ? t(CATEGORY[filter] ?? "Other") : t("Apps") }), countEl),
            finder,
            listEl)),
        chartBox),
    );
  }

  // ── Drawing: one app's page ──

  function drawAppPage(u: Usage) {
    const key = detail ?? "";
    const listed = u.apps.find((a) => a.key === key);
    const p = pageData && pageData.key === key ? pageData : null;
    const name = p?.name || listed?.name || key;
    const cat = p?.cat ?? listed?.cat ?? "other";
    const secs = p?.secs ?? listed?.secs ?? 0;
    const share = p?.share ?? (u.total ? secs / u.total : 0);
    const path = p?.path || listed?.path || "";
    // Lintel's own row: its file is there for the icon, and there is nothing to start.
    const self = key === LINTEL;

    const back = btn(t("All apps"), "ghost", closeApp);
    back.classList.add("sm");
    back.prepend(icon(CHEV_L, 12));

    const catBtn = btn(t(CATEGORY[cat] ?? "Other"), "secondary", () => {
      const box = catBtn.getBoundingClientRect();
      menu(
        [
          ...CATEGORIES.map((c) => ({ label: t(CATEGORY[c]), onClick: () => command("category", { key, cat: c }) })),
          null,
          { label: t("Use the default category"), onClick: () => command("category", { key, cat: "" }) },
        ],
        { x: box.left, y: box.bottom + 4 },
      );
    });
    catBtn.classList.add("sm");

    const hide = btn(t("Hide this app"), "ghost", () => {
      closeApp();
      command("hide", { key });
    });
    hide.classList.add("sm");

    // Hide keeps the time; this takes it away for good — the privacy answer to an app opened once.
    const forget = btn(t("Remove from history"), "ghost", () => void confirmAction().then((yes) => {
      if (!yes) return;
      closeApp();
      command("forgetApp", { key });
    }));
    forget.classList.add("sm");
    forget.title = t("Delete every second of this app, on every day");

    const open = btn(t("Open"), "secondary", () => void cx.call("open", { key }));
    const reveal = btn(t("Show in Explorer"), "ghost", () => void cx.call("reveal", { key }));
    for (const b of [open, reveal]) {
      b.disabled = !path;
      b.classList.add("sm");
      if (!path) b.title = t("No program file.");
    }
    const acts = h("div", { class: `${P}wdacts` }, ...(self ? [] : [open, reveal]));

    const head = h("div", { class: `${P}wdhead` },
      back,
      images.has(key) ? h("img", { class: `${P}wico big`, src: images.get(key), alt: "" }) : h("span", { class: "isl-ico" }, icon(HOURGLASS, 16)),
      h("span", { class: `${P}wdtext` },
        h("b", { class: `${P}wdname`, text: key === "desktop" ? t("Desktop") : name, title: name }),
        u.titles && p?.title ? h("small", { class: `${P}wdtitle`, text: p.title, title: p.title }) : null),
      catBtn,
      h("span", { class: `${P}wgap` }),
      h("small", { class: `${P}wdday`, text: day === u.today ? t("Today") : dayLabel(day, u.today) }),
      forget,
      hide);

    const facts = h("div", { class: `${P}wdinfo` },
      h("div", { class: `${P}wdstat` },
        h("b", { class: `${P}wdtime`, text: span(secs) }),
        h("span", { class: `${P}wdshare`, text: t("{count}% of the day", { count: Math.round(share * 100) }) }),
        h("small", { class: `${P}wdpath`, text: path || t("No program file."), title: path })),
      acts);

    // This app's own fourteen days; a bar switches the day under it.
    const series = Array.from({ length: SPAN }, (_, i) => {
      const d = shiftDay(u.today, -(SPAN - 1 - i));
      return { d, i, secs: p?.days[d] ?? 0 };
    });
    const tallest = Math.max(1, ...series.map((x) => x.secs));
    const card = h("div", { class: `${P}wchart` },
      h("div", { class: `${P}whead` }, h("span", { text: t("Last 14 days") })),
      bars(series.map((x) => ({
        key: x.d,
        label: weekdayShort(dayDate(x.d).getDay()),
        title: `${dayLabel(x.d, u.today)} · ${p?.days[x.d] ? span(x.secs) : t("Nothing was recorded this day.")}`,
        frac: x.secs / tallest,
        state: `${x.d === u.today ? " today" : ""}${x.d === day ? " on" : ""}${p?.days[x.d] ? "" : " empty"}`,
        onClick: () => {
          day = x.d;
          void load();
        },
      }))));

    const windows = u.titles && p?.windows?.length
      ? h("section", { class: `${P}wwindows` },
          h("div", { class: `${P}whead` }, h("span", { text: t("Time by window") })),
          ...p.windows.map((w) => {
            const fmt = new Intl.DateTimeFormat(language(), { hour: "numeric", minute: "2-digit" });
            const recent = w.intervals.slice(-3).reverse().map(({ start, end }) => {
              const from = new Date(start * 1000);
              const to = new Date(end * 1000);
              return `${fmt.format(from)}–${fmt.format(to)}`;
            });
            const title = w.title || t("Window title unavailable");
            return h("div", { class: `${P}wwindow` },
              h("span", { class: `${P}wtitle`, text: title, title }),
              h("b", { text: span(w.secs) }),
              h("small", { text: recent.join(" · "), title: recent.join(" · ") }));
          }))
      : null;

    body.replaceChildren(h("div", { class: `${P}wpage${windows ? " has-windows" : ""}` }, head, facts, windows, card));
  }

  // ── Drawing: the whole of it ──

  function draw() {
    if (!usage) return;
    if (!usage.on && usage.total === 0 && Object.keys(usage.days).length === 0) {
      body.replaceChildren(
        h("div", { class: `${P}woffpage` },
          h("b", { text: t("Screen time is off") }),
          h("p", { text: t("It counts the time each app is in front: app names, durations and, if you turn them on, window titles. Kept on this PC.") }),
          btn(t("Turn on"), "primary", () => void cx.settings.patch({ screenTime: true }).then(load))),
      );
      return;
    }
    if (detail) drawAppPage(usage);
    else drawDay(usage);
    remember();
  }

  // ── Keys, and the life of the window ──

  const onKey = (e: KeyboardEvent) => {
    // The finder takes its own keys: an app is typed, not navigated past.
    if ((e.target as HTMLElement | null)?.tagName === "INPUT") return;
    const k = e.key;
    if (k === "Escape" && cal) {
      e.preventDefault();
      closeCal();
      return;
    }
    if (k === "Escape") {
      e.preventDefault();
      if (detail) closeApp();
      else void cx.call("close");
      return;
    }
    // A calendar on screen is the thing the keys are about.
    if (cal) return;
    if (detail) return;
    if (k === "ArrowLeft" || k === "ArrowRight") {
      e.preventDefault();
      go(k === "ArrowLeft" ? -1 : 1);
    } else if (k === "Home") {
      e.preventDefault();
      day = usage?.today ?? dateKey(new Date());
      cursor = 0;
      void load();
    } else if (k === "ArrowUp" || k === "ArrowDown") {
      e.preventDefault();
      moveCursor(k === "ArrowUp" ? -1 : 1);
    } else if (k === "Enter") {
      const a = visible()[cursor];
      if (!a) return;
      e.preventDefault();
      openApp(a.key);
    }
  };

  /** The day being counted moves; a window nobody can see asks nothing. */
  const pace = () => {
    window.clearInterval(timer);
    timer = undefined;
    if (document.hidden) return;
    load();
    timer = window.setInterval(() => {
      // A finder being typed in is not redrawn out from under the person.
      const typing = document.activeElement === finder.input;
      if (!typing && !detail && usage?.on && (day || usage.today) === usage.today) load();
    }, 10_000);
  };

  window.addEventListener("keydown", onKey);
  document.addEventListener("visibilitychange", pace);
  window.addEventListener("pagehide", () => {
    window.removeEventListener("keydown", onKey);
    document.removeEventListener("visibilitychange", pace);
    clearInterval(timer);
    closeCal();
  });
  // The day and the category the window was opened on (crates/breaks/src/lib.rs, `report`);
  // "" when it was never opened before, and then the first answer names today.
  let asked = "";
  const begin = () =>
    void cx.call<{ day: string; cat: string }>("report").then((r) => {
      const key = `${r?.day ?? ""}|${r?.cat ?? ""}`;
      // Only a new ask moves the window: a state event that says the same thing is not a reason
      // to take the person's own filter away.
      if (key === asked) return;
      asked = key;
      if (r?.day && r.day !== day) {
        day = r.day;
        cursor = 0;
      }
      if (r) filter = r.cat || null;
      load();
    });
  begin();
  // The island asks for another day while this window is already open: the window follows it.
  const off = cx.on("state", begin);
  window.addEventListener("pagehide", off);
}
