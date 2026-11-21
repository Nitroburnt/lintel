// The components of the design language (docs/ARCHITECTURE.md §7): one builder
// per class of src/ui/components.css, plus the two the island strip is made of
// (`stripRow`, `dotMark`). The classes are the contract; these only save typing.

import { h } from "../views/dom";
import type { Msg } from "../i18n/i18n";
import { icon } from "./icons";

type Text = string | Msg;
type Kid = Node | string | Msg | null | undefined | false;

/** Colours a card, tile, meter, capsule or strip mark. */
export type Tone = "ok" | "warn" | "bad" | "info";

const cls = (...names: (string | null | undefined | false)[]) => names.filter(Boolean).join(" ");

/** `.card`; `wash` tints it: true for the accent, a tone for an alert card. */
export function card(children: Kid | Kid[], wash?: boolean | Tone): HTMLElement {
  const kids = Array.isArray(children) ? children : [children];
  return h("div", { class: cls("card", wash && "wash", typeof wash === "string" && wash) }, ...kids);
}

/** `.btn`: 28 px tall, with an optional key hint. */
export function btn(
  label: Text,
  kind: "primary" | "secondary" | "ghost" | "danger",
  onClick: () => void,
  kbdText?: string,
): HTMLButtonElement {
  return h("button", { class: `btn ${kind}`, onclick: onClick }, h("span", { text: label }), kbdText ? kbd(kbdText) : null);
}

/** `.btn.icon`: a square button holding one icon; `title` is its tooltip. */
export function iconBtn(
  name: string,
  title: Text,
  onClick: () => void,
  kind: "primary" | "secondary" | "ghost" | "danger" = "ghost",
): HTMLButtonElement {
  return h("button", { class: `btn ${kind} icon`, title, "aria-label": title, onclick: onClick }, icon(name, 14));
}

/** `.switch`: 34×20. `onChange` gets the new value; the switch flips itself. */
export function toggle(on: boolean, onChange: (on: boolean) => void): HTMLButtonElement {
  const el = h("button", { class: "switch", role: "switch", "aria-checked": String(on) });
  el.addEventListener("click", () => {
    const next = el.getAttribute("aria-checked") !== "true";
    el.setAttribute("aria-checked", String(next));
    onChange(next);
  });
  return el;
}

/** `.tab`: icon 13 and its title. The island header shows the title of the tab that is on. */
export function tab(iconName: string, title: Text, onClick: () => void): HTMLButtonElement {
  return h("button", { class: "tab", title, onclick: onClick }, icon(iconName, 13), h("span", { text: title }));
}

/** `.row`: a settings row, label left, control right, hint below the label. */
export function row(label: Text, control: Node, hint?: Text): HTMLElement {
  return h(
    "div",
    { class: "row" },
    h("span", { class: "row-label", text: label }),
    hint ? h("span", { class: "row-hint", text: hint }) : null,
    control,
  );
}

export interface FieldOpts {
  value?: string;
  placeholder?: Text;
  /** Icon name shown before the text. */
  icon?: string;
  type?: string;
  /** A textarea instead of a one-line input. */
  multiline?: boolean;
  onInput?: (value: string) => void;
  /** Enter in a one-line field. */
  onEnter?: (value: string) => void;
}

/** `.field`: 28 px. The input itself is `field(...).input`. */
export function field(opts: FieldOpts = {}): HTMLLabelElement & { input: HTMLInputElement | HTMLTextAreaElement } {
  const input = opts.multiline
    ? h("textarea", { placeholder: opts.placeholder, spellcheck: "false" })
    : h("input", { type: opts.type ?? "text", placeholder: opts.placeholder, spellcheck: "false" });
  if (opts.value != null) input.value = opts.value;
  input.addEventListener("input", () => opts.onInput?.(input.value));
  if (!opts.multiline) {
    input.addEventListener("keydown", (e) => {
      if ((e as KeyboardEvent).key === "Enter") opts.onEnter?.(input.value);
    });
  }
  const el = h("label", { class: "field" }, opts.icon ? icon(opts.icon, 14) : null, input);
  return Object.assign(el, { input });
}

/** `.meter`: a 6 px bar filled to `fraction` (0…1). Move it with `setMeter`. */
export function meter(fraction: number, tone?: Tone): HTMLElement {
  const el = h("span", { class: cls("meter", tone) });
  setMeter(el, fraction);
  return el;
}

export function setMeter(el: HTMLElement, fraction: number) {
  const v = Math.max(0, Math.min(1, fraction));
  el.style.setProperty("--v", String(v));
  if (el.getAttribute("role") === "slider") el.setAttribute("aria-valuenow", String(Math.round(v * 100)));
}

export interface SliderOpts {
  /** What it sets, for a screen reader. */
  label: Text;
  tone?: Tone;
  /** While it is dragged, 0…1: show what letting go here would give. */
  onInput?: (fraction: number) => void;
  /** Let go, or stepped with an arrow key: 0…1. */
  onChange: (fraction: number) => void;
  /** A drag began (true) or ended: in the island, hold it for as long. */
  onDrag?: (dragging: boolean) => void;
  /** The mouse wheel steps it 5 % a notch, up is more (`onChange` fires for each step). */
  wheel?: boolean;
  /** How much of the range one wheel notch moves it; 0.05 when not given. */
  wheelStep?: number;
}

/**
 * `.meter[role=slider]`: a meter that is dragged or stepped with the arrow keys.
 * Move it with `setMeter`; it has the class `dragging` while the pointer holds
 * it (leave it alone then), and `aria-disabled="true"` makes it a plain meter.
 */
export function slider(fraction: number, o: SliderOpts): HTMLElement {
  const el = h("span", {
    class: cls("meter", o.tone),
    role: "slider",
    tabindex: "0",
    "aria-label": o.label,
    "aria-valuemin": "0",
    "aria-valuemax": "100",
  });
  setMeter(el, fraction);
  const off = () => el.getAttribute("aria-disabled") === "true";
  const at = (e: PointerEvent) => {
    const r = el.getBoundingClientRect();
    return Math.max(0, Math.min(1, (e.clientX - r.left) / Math.max(1, r.width)));
  };
  const drag = (e: PointerEvent) => {
    setMeter(el, at(e));
    o.onInput?.(at(e));
  };
  const end = () => {
    if (!el.classList.contains("dragging")) return false;
    el.classList.remove("dragging");
    o.onDrag?.(false);
    return true;
  };
  el.addEventListener("pointerdown", (e) => {
    if (off() || e.button !== 0) return;
    el.classList.add("dragging");
    // The pointer may leave the bar (and the window) while it drags.
    el.setPointerCapture(e.pointerId);
    o.onDrag?.(true);
    drag(e);
  });
  el.addEventListener("pointermove", (e) => el.classList.contains("dragging") && drag(e));
  el.addEventListener("pointerup", (e) => end() && o.onChange(at(e)));
  el.addEventListener("pointercancel", end);
  el.addEventListener("keydown", (e) => {
    const step = e.key === "ArrowRight" || e.key === "ArrowUp" ? 0.05 : e.key === "ArrowLeft" || e.key === "ArrowDown" ? -0.05 : 0;
    if (!step || off()) return;
    e.preventDefault();
    const v = Math.max(0, Math.min(1, Number(el.style.getPropertyValue("--v") || 0) + step));
    setMeter(el, v);
    o.onChange(v);
  });
  if (o.wheel) {
    const step = o.wheelStep ?? 0.05;
    // A mouse sends about 100 px a notch, a trackpad many small deltas: both add up here, so a
    // trackpad moves it smoothly instead of once per event. A wheel pushed forward (up the page,
    // `deltaY` negative) raises the value, as it raises every other slider.
    let acc = 0;
    el.addEventListener(
      "wheel",
      (e) => {
        if (off() || e.deltaY === 0) return;
        e.preventDefault();
        acc += e.deltaY;
        let notches = 0;
        while (acc <= -100) {
          acc += 100;
          notches -= 1;
        }
        while (acc >= 100) {
          acc -= 100;
          notches += 1;
        }
        if (!notches) return;
        const now = Number(el.style.getPropertyValue("--v") || 0);
        const v = Math.round(Math.max(0, Math.min(1, now - notches * step)) * 1000) / 1000;
        setMeter(el, v);
        o.onChange(v);
      },
      { passive: false },
    );
  }
  return el;
}

/** `.bars`: the four playing bars; `paused` holds them still. */
export function bars(paused = false): HTMLElement {
  return h("span", { class: cls("bars", paused && "paused") }, h("i"), h("i"), h("i"), h("i"));
}

/** `.capsule`: a chip with an icon, a text and a meter or bars at its right end. */
export function capsule(o: { icon?: string; text: Text; meter?: number; bars?: boolean; tone?: Tone }): HTMLElement {
  return h(
    "span",
    { class: cls("capsule", o.tone) },
    o.icon ? icon(o.icon, 14) : null,
    h("span", { text: o.text }),
    o.meter != null ? meter(o.meter, o.tone) : null,
    o.bars ? bars() : null,
  );
}

/** `.tile`: 76×74, icon 20 over a label; `state` colours it (on, running, recording). */
export function tile(iconName: string, label: Text, onClick: () => void, state?: Tone): HTMLButtonElement {
  return h("button", { class: cls("tile", state), onclick: onClick }, icon(iconName, 20), h("span", { text: label }));
}

export interface ListItem {
  label: Text;
  icon?: string;
  /** Small text at the right end. */
  meta?: Text;
  kbd?: string;
  /** Selected. */
  on?: boolean;
  onClick?: () => void;
}

/** `.list` of 32 px `.list-item` rows. */
export function list(items: ListItem[]): HTMLElement {
  return h(
    "div",
    { class: "list", role: "listbox" },
    ...items.map((it) =>
      h(
        "button",
        { class: cls("list-item", it.on && "on"), role: "option", onclick: it.onClick },
        it.icon ? icon(it.icon, 16) : null,
        h("span", { text: it.label }),
        it.meta ? h("small", { text: it.meta }) : null,
        it.kbd ? kbd(it.kbd) : null,
      ),
    ),
  );
}

export interface MenuItem {
  label: Text;
  icon?: string;
  kbd?: string;
  danger?: boolean;
  onClick: () => void;
}

/**
 * `.menu`: a context menu at `at` (viewport pixels), kept inside the window; in
 * the island's window, inside the island's shape (the window lets clicks through
 * everywhere else). A `null` item is a separator. It closes on a pick, a click
 * elsewhere or Esc.
 */
export function menu(items: (MenuItem | null)[], at: { x: number; y: number }): HTMLElement {
  const el = h("div", { class: "menu fx-in", role: "menu", style: "position:fixed;z-index:1000" });
  const close = () => {
    el.remove();
    window.removeEventListener("pointerdown", outside, true);
    window.removeEventListener("keydown", esc, true);
  };
  const outside = (e: Event) => {
    if (!el.contains(e.target as Node)) close();
  };
  const esc = (e: KeyboardEvent) => {
    if (e.key !== "Escape") return;
    e.stopPropagation();
    close();
  };
  for (const it of items) {
    if (!it) {
      el.append(h("hr"));
      continue;
    }
    const run = () => {
      close();
      it.onClick();
    };
    el.append(
      h(
        "button",
        { class: cls(it.danger && "danger"), role: "menuitem", onclick: run },
        it.icon ? icon(it.icon, 14) : null,
        h("span", { text: it.label }),
        it.kbd ? kbd(it.kbd) : null,
      ),
    );
  }
  document.body.append(el);
  const box = document.getElementById("island")?.getBoundingClientRect() ??
    { left: 0, top: 0, right: window.innerWidth, bottom: window.innerHeight };
  // Longer than the room there is: it scrolls (components.css).
  el.style.maxHeight = `${box.bottom - box.top - 4}px`;
  const r = el.getBoundingClientRect();
  el.style.left = `${Math.max(box.left + 4, Math.min(at.x, box.right - r.width - 4))}px`;
  el.style.top = `${Math.max(box.top + 2, Math.min(at.y, box.bottom - r.height - 2))}px`;
  window.addEventListener("pointerdown", outside, true);
  window.addEventListener("keydown", esc, true);
  return el;
}

/** `.kbd`: a key hint. */
export function kbd(text: string): HTMLElement {
  return h("span", { class: "kbd", text });
}

// ── The island strip ──────────────────────────────────────────────────────────

export interface StripOpts {
  /** Left mark: an icon name. With `tone` it sits in a tinted 22 px chip. */
  icon?: string;
  /** Left mark instead of an icon: album art, an app icon, a `dotMark`… (22 px). */
  mark?: HTMLElement;
  tone?: Tone;
  title: Text;
  /** Dimmer text after the title; it is what gets cut when the row is full. */
  detail?: Text;
  /** Right reading, one of: a level 0…1 (a meter and its number)… */
  level?: number;
  /** …the playing bars ("paused" holds them still)… */
  bars?: boolean | "paused";
  /** …a short value in tabular digits ("18:42", "3 of 5")… */
  value?: Text;
  /** …or a small button. */
  action?: { label: Text; run: () => void };
}

/**
 * One row of the compact strip: mark and title on the left, the reading on the
 * right. What a live activity's `render()` returns and what a notice is drawn as.
 */
export function stripRow(o: StripOpts): HTMLElement {
  const mark =
    o.mark ?? (o.icon ? (o.tone ? h("span", { class: `isl-ico ${o.tone}` }, icon(o.icon, 14)) : icon(o.icon, 14)) : null);
  // What a section may show as well travels to it when the island opens
  // (docs/ARCHITECTURE.md §4): the section marks its own with the same names.
  if (o.mark && !o.mark.dataset.shared) o.mark.dataset.shared = "mark";
  return h(
    "div",
    { class: "strip-row" },
    mark,
    h("b", { "data-shared": "title", text: o.title }),
    o.detail ? h("span", { class: "d", text: o.detail }) : null,
    h("span", { class: "isl-gap" }),
    o.level != null ? meter(o.level, o.tone) : null,
    o.level != null ? h("span", { class: "isl-num", text: String(Math.round(o.level * 100)) }) : null,
    o.bars ? bars(o.bars === "paused") : null,
    o.value ? h("span", { class: "isl-num", text: o.value }) : null,
    o.action
      ? h("button", {
          class: "btn sm ghost",
          text: o.action.label,
          onclick: (e: Event) => {
            e.stopPropagation();
            o.action!.run();
          },
        })
      : null,
  );
}

/**
 * The mascot when it is hidden (§9): a 10 px dot in the pill colour with a 2 px
 * ring for the state (approval, question, error, finished; an arc while busy). Listening
 * and talking read `--level` (§7.6).
 */
export function dotMark(color: string, state = "idle"): HTMLElement {
  const el = h("i", { class: "dot-mark" });
  setDotMark(el, color, state);
  return el;
}

/** `level` (0…1) is the sound level of listening or talking; left alone when not given. */
export function setDotMark(el: HTMLElement, color: string, state: string, level?: number) {
  if (level !== undefined) el.style.setProperty("--level", Math.max(0, Math.min(1, level || 0)).toFixed(2));
  if (color) el.style.setProperty("--c", color);
  else el.style.removeProperty("--c");
  el.dataset.state = state;
}
