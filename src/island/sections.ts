// The sections and live activities features register (docs/ARCHITECTURE.md §5.2,
// §5.3): which ones exist right now, the section on screen and its show / hide /
// sync moments, and the gallery of all sections.
//
// A section belongs to a feature, so it exists only while that feature runs.
// Its `build` is called the first time it is shown, never before.

import type { ActivityDef, SectionCommand, SectionDef, SectionHost } from "../core/feature";
import { HEADER_H, INSET, SECTION_MAX_H, SECTION_MIN_H } from "../core/layout";
import { FEATURES, featureCtx, featureOn } from "../core/registry";
import { State } from "../core/state";
import { N_, language, t, tl } from "../i18n/i18n";
import { field, tile } from "../ui/components";
import { clear, h } from "../views/dom";
import type { ViewHost } from "../views/views";

export interface Section {
  /** `<feature>.<id>`: what the island and the header go by. */
  key: string;
  feature: string;
  def: SectionDef;
}

export interface Activity {
  key: string;
  feature: string;
  def: ActivityDef;
}

export const surfaceKey = (feature: string, id: string) => `${feature}.${id}`;

/**
 * The default order of the header tabs and the gallery: what is used most
 * first. The header holds the first seven; the rest are in the gallery. A
 * section that is not listed comes after these, in the order it was found.
 * debt: the user's own order and hide list (Settings › Island) will replace this.
 */
const SECTION_ORDER = [
  "island.home", "media.music", "controls-toggles.controls", "clipboard.clipboard", "timers-downloads.timer", "calendar.calendar", "shelf.shelf",
  "island.utilities", "capture.captures", "audio-mixer.mixer", "controls-toggles.notes", "weather.weather", "agent-usage.agents", "breaks.screen", "calendar.reminders",
  "apps.apps", "sysmon.system", "window-layout.layout", "displays-keepawake.keep", "huds-power.battery", "displays-keepawake.displays",
  "island-extras.quick", "media-tools.tools", "cleaner-uninstaller.cleaner", "island-extras.mirror", "displays-keepawake.wallpaper", "breaks.break",
];
const rank = (key: string) => (SECTION_ORDER.indexOf(key) + 1 || SECTION_ORDER.length + 1);

/**
 * The island's own section: the Dashboard, its Home page of smart cards
 * (src/island/home.ts). It is a section like any other (tab, gallery tile,
 * Alt+H, "Open on"); the island says how it is built, and Settings › Island
 * "Dashboard cards" set to Off takes it away.
 */
export const HOME_KEY = "island.home";
/** How the island's own sections are built: the island says, once it is there. */
const builders = new Map<string, () => SectionHost>();
export const setBuilder = (key: string, build: () => SectionHost) => void builders.set(key, build);
const own = (id: string, title: string, icon: string, letter: string, height: number): Section => ({
  key: surfaceKey("island", id),
  feature: "island",
  def: { id, title, icon, letter, height, build: () => builders.get(surfaceKey("island", id))?.() ?? broken(title) },
});
const HOME = own("home", N_("Dashboard"), "M4 5h16v6H4zM4 14h7v5H4zM14 14h6v5h-6z", "h", 212);
/** Every tool the running features offer as a shortcut, as a list (src/island/utilities.ts). */
const UTILITIES = own("utilities", N_("Utilities"), "M14.5 6.5a4 4 0 0 0-5 5L4 17v3h3l5.5-5.5a4 4 0 0 0 5-5l-2.5 2.5-2-2z", "d", 280);

/** How many section tabs the header holds at most (Settings › Island lets the user pick that many). */
export const MAX_TABS = 10;
/** The header held this many sections before the user could choose (Settings › Island, `islandTabs`). */
const HEADER_SECTIONS = 7;

/**
 * The sections that are tabs of the header: the user's own list, in its order,
 * without the sections that are not there right now (a feature that is off),
 * and never more than MAX_TABS — a hand-edited list longer than the settings
 * page allows is clamped here, so the header cannot overflow. No list: the
 * first few in the order above. The section that is open is added by the header
 * itself when the user left it out (src/views/views.ts).
 */
export const headerKeys = (tabs: string[] | null = State.settings.islandTabs): string[] =>
  tabs
    ? sections(tabs).filter((s) => tabs.includes(s.key)).slice(0, MAX_TABS).map((s) => s.key)
    : sections().slice(0, HEADER_SECTIONS).map((s) => s.key);

/**
 * The user's tab list with `key` moved to `to`'s place: `to` is the tab it was
 * dropped on (a drag in the header), or the tab beside it (Ctrl+Shift+← / →).
 * `key` may be the open section shown on top of a list that does not hold it: it
 * then joins the list, and a full list lets its last tab go. Null when nothing
 * moves.
 */
export function moveTab(tabs: string[] | null, key: string, to: string): string[] | null {
  if (key === to) return null;
  const base = [...(tabs ?? headerKeys(null))];
  const target = base.indexOf(to);
  if (target < 0) return null;
  const from = base.indexOf(key);
  if (from >= 0) base.splice(from, 1);
  else if (base.length >= MAX_TABS) base.pop();
  base.splice(Math.min(target, base.length), 0, key);
  return base;
}

/** `key` one place along the header (Ctrl+Shift+← / →), as a drop on its neighbour there. Null at the row's end. */
export function stepTab(tabs: string[] | null, key: string, delta: 1 | -1): string[] | null {
  const row = headerKeys(tabs);
  const at = row.indexOf(key);
  const to = row[at + delta];
  return at < 0 || !to ? null : moveTab(tabs, key, to);
}

/**
 * Every section of the features that are on, and the island's own. The ones the
 * user chose as tabs come first, in their order; the rest follow in the default
 * order, so the gallery and Ctrl+Tab use the same order.
 */
export function sections(tabs: string[] | null = State.settings.islandTabs): Section[] {
  const chosen = tabs ?? [];
  return [
    ...(State.settings.homeCards === "off" ? [] : [HOME]),
    UTILITIES,
    ...FEATURES.filter((f) => featureOn(f.id)).flatMap((f) =>
      (f.sections ?? []).map((def) => ({ key: surfaceKey(f.id, def.id), feature: f.id, def })),
    ),
  ].sort((a, b) => {
    const ia = chosen.indexOf(a.key);
    const ib = chosen.indexOf(b.key);
    if (ia !== ib) {
      if (ia < 0) return 1;
      if (ib < 0) return -1;
      return ia - ib;
    }
    return rank(a.key) - rank(b.key); // stable: equal ranks keep the order they were found in
  });
}

/** Every live activity of the features that are on. */
export function activities(): Activity[] {
  return FEATURES.filter((f) => featureOn(f.id)).flatMap((f) =>
    (f.activities ?? []).map((def) => ({ key: surfaceKey(f.id, def.id), feature: f.id, def })),
  );
}

/** A section that failed to build says so instead of taking the island down. */
function broken(title: string): SectionHost {
  const el = h("div", { class: "sub", text: t("{name} could not be shown.", { name: title }) });
  return { el, show() {}, hide() {}, sync() {} };
}

export class Sections {
  /** The section the island is on while its view is "section". */
  active: string | null = null;

  private hosts = new Map<string, { host: SectionHost; view: HTMLElement }>();
  private shown: string | null = null;

  /** `parent` is the island's view stack. */
  constructor(private parent: HTMLElement) {}

  find(key: string | null): Section | undefined {
    return key ? sections().find((s) => s.key === key) : undefined;
  }

  /** Height of the open island on the active section. */
  height(): number {
    const want = this.find(this.active)?.def.height ?? SECTION_MIN_H;
    return Math.max(SECTION_MIN_H, Math.min(SECTION_MAX_H, want));
  }

  /**
   * Called on every island sync with the section that is on screen (null when
   * the island is on something else, folded or hidden). This is the one place
   * `show` and `hide` come from, so every way out of a section passes here.
   */
  sync(visible: string | null) {
    if (visible !== this.shown) {
      const old = this.shown ? this.hosts.get(this.shown) : undefined;
      if (old) {
        old.view.classList.remove("on");
        this.guard(() => old.host.hide());
      }
      this.shown = visible;
      const next = visible ? this.host(visible) : undefined;
      if (next) {
        next.view.classList.add("on");
        this.guard(() => next.host.show());
      }
    }
    const now = this.shown ? this.hosts.get(this.shown) : undefined;
    if (now) this.guard(() => now.host.sync());
  }

  /** Esc and the like go to the section first; true = it used it. */
  command(command: SectionCommand): boolean {
    const now = this.shown ? this.hosts.get(this.shown) : undefined;
    let used = false;
    if (now) this.guard(() => (used = now.host.command?.(command) === true));
    return used;
  }

  /** Features were switched: what belonged to one that is off goes away. */
  prune() {
    for (const [key, entry] of this.hosts) {
      if (this.find(key)) continue;
      if (this.shown === key) {
        this.guard(() => entry.host.hide());
        this.shown = null;
      }
      entry.view.remove();
      this.hosts.delete(key);
    }
    if (this.active && !this.find(this.active)) this.active = null;
  }

  /**
   * The language changed: a section draws its texts once, when it is built, so
   * every one is built again the next time it is shown.
   */
  rebuild() {
    for (const [key, entry] of this.hosts) {
      if (this.shown === key) this.guard(() => entry.host.hide());
      entry.view.remove();
    }
    this.hosts.clear();
    this.shown = null;
  }

  private host(key: string) {
    const existing = this.hosts.get(key);
    if (existing) return existing;
    const section = this.find(key);
    if (!section) return undefined;
    let host: SectionHost;
    try {
      host = section.def.build(featureCtx(section.feature));
    } catch (err) {
      console.error(`[section ${key}] build failed`, err);
      host = broken(section.def.title);
    }
    const view = h("div", { class: "view section" }, host.el);
    this.parent.append(view);
    const entry = { host, view };
    this.hosts.set(key, entry);
    return entry;
  }

  private guard(fn: () => void) {
    try {
      fn();
    } catch (err) {
      console.error("[section] failed", err);
    }
  }
}

// ── Gallery ───────────────────────────────────────────────────────────────────

const TILE_H = 74;
const TILES_PER_ROW = 7;

/** Height of the open island on the gallery: the filter, then up to three rows of tiles. */
export function galleryHeight(count: number): number {
  const rows = Math.max(1, Math.min(3, Math.ceil(count / TILES_PER_ROW)));
  // Three whole rows are 330: ten more than a section may be, and the window (360) has the room.
  return HEADER_H + 28 + 8 + rows * TILE_H + (rows - 1) * 8 + INSET;
}

/** Every section as a tile; typing filters, Enter opens the first one left. */
export function buildGallery(open: (key: string) => void): ViewHost {
  const grid = h("div", { class: "gallery-grid" });
  let shownKey = "";
  let matches: Section[] = [];

  const draw = () => {
    const all = sections();
    const query = filter.input.value.trim().toLowerCase();
    matches = all.filter((s) => !query || t(s.def.title).toLowerCase().includes(query) || s.key.includes(query));
    const key = `${language()}~${query}~${all.map((s) => s.key).join("|")}`;
    if (key === shownKey) return;
    shownKey = key;
    clear(grid);
    for (const s of matches) {
      const el = tile(s.def.icon, t(s.def.title), () => open(s.key));
      if (s.def.letter) el.title = `${t(s.def.title)} (Alt+${s.def.letter.toUpperCase()})`;
      grid.append(el);
    }
    if (matches.length === 0) {
      grid.append(
        h("div", {
          class: "sub",
          text: all.length === 0 ? t("No sections yet. Features you turn on in Settings add them here.") : t("Nothing matches."),
        }),
      );
    }
  };

  const filter = field({
    icon: "search",
    placeholder: tl("Find a section"),
    onInput: draw,
    onEnter: () => {
      if (matches[0]) open(matches[0].key);
    },
  });
  const el = h("div", { class: "view gallery" }, filter, grid);

  return {
    el,
    sync: draw,
    focus() {
      filter.input.value = "";
      draw();
      filter.input.focus();
    },
  };
}
