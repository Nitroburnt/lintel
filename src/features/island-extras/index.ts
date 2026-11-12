// Island extras — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Mirror (camera only while its section is on screen), quick access, and follow-the-pointer
// (Rust says "off-display", the island repositions itself). The wheel and key switches are the
// island's own (Settings › Island).

import "./style.css";
import { defineFeature, type FeatureCtx, type SectionHost } from "../../core/feature";
import { Bridge } from "../../core/bridge";
import { State } from "../../core/state";
import { h } from "../../views/dom";
import { icon } from "../../ui/icons";
import { btn, card, field, row, tile, toggle } from "../../ui/components";

interface Quick { label: string; target: string }
interface Site { name: string; url: string }
const MAX_SITES = 12;

/** A row whose control is a list of rows: its label and hint sit above the list, at full width. */
const blockRow = (label: string, control: Node, hint?: string): HTMLElement => {
  const el = row(label, control, hint);
  el.classList.add("f-island-extras-block");
  return el;
};

export default defineFeature({
  id: "island-extras",
  voice: () => import("./voice"),

  sections: [
    { id: "mirror", title: "Mirror", icon: "camera", letter: "i", height: 280, build: (cx) => mirrorPage(cx) },
    { id: "quick", title: "Quick access", icon: "external", letter: "q", height: 264, build: (cx) => quickPage(cx) },
  ],

  settings(cx) {
    const box = card([]);
    const sitesBox = h("div", { class: "f-island-extras-edit" });
    const drawSites = (): Promise<void> =>
      cx.settings.get().then((s) => {
        const sites = ((s.sites as Site[]) ?? []).slice(0, MAX_SITES);
        const name = field({ placeholder: cx.t("Website name") });
        const addr = field({ placeholder: cx.t("Address, https://…") });
        const msg = h("p", { class: "f-island-extras-msg" });
        const add = async () => {
          if (sites.length >= MAX_SITES) return void (msg.textContent = cx.t("Up to {count} websites.", { count: MAX_SITES }));
          const url = await cx.call<string>("web_url", { url: addr.input.value });
          if (!url) return void (msg.textContent = cx.t("Only http and https addresses work."));
          const label = name.input.value.trim() || new URL(url).hostname.replace(/^www\./, "");
          await cx.settings.patch({ sites: [...sites, { name: label.slice(0, 30), url }] });
          void drawSites();
        };
        sitesBox.replaceChildren(
          ...sites.map((w, i) =>
            h("div", { class: "f-island-extras-erow" },
              h("span", { class: "f-island-extras-site-name", text: w.name, title: w.url }),
              btn(cx.t("Remove"), "ghost", () => void cx.settings.patch({ sites: sites.filter((_, j) => j !== i) }).then(drawSites)))),
          h("div", { class: "f-island-extras-erow" }, name, addr, btn(cx.t("Add"), "secondary", () => void add())),
          msg);
      });
    void drawSites();
    const draw = () =>
      cx.settings.get().then((s) => {
        const quick = (s.quick as Quick[]) ?? [];
        const save = (q: Quick[]) => cx.settings.patch({ quick: q });
        const shape = h("select", {}) as HTMLSelectElement;
        shape.append(h("option", { value: "square", text: cx.t("Square") }), h("option", { value: "circle", text: cx.t("Circle") }));
        shape.value = String(s.mirrorShape ?? "square");
        shape.addEventListener("change", () => cx.settings.patch({ mirrorShape: shape.value }));
        const list = h("div", { class: "f-island-extras-edit" },
          ...quick.map((q, i) =>
            h("div", { class: "f-island-extras-erow" },
              field({ value: q.label, placeholder: cx.t("Name"), onInput: (v) => { quick[i].label = v; save(quick); } }),
              field({ value: q.target, placeholder: cx.t("Program, folder or link"), onInput: (v) => { quick[i].target = v; save(quick); } }),
              btn(cx.t("Remove"), "ghost", () => { quick.splice(i, 1); save(quick).then(draw); }))),
          btn(cx.t("Add"), "secondary", () => { quick.push({ label: "", target: "" }); save(quick).then(draw); }));
        box.replaceChildren(
          row(cx.t("Show in screenshots"), toggle(s.showInScreenshots !== false, (on) => cx.settings.patch({ showInScreenshots: on })),
            cx.t("Off hides the island from screenshots and screen recordings.")),
          row(cx.t("Follow the pointer"), toggle(s.followPointer === true, (on) => cx.settings.patch({ followPointer: on })),
            cx.t("The island moves to the display the pointer is on while it is closed.")),
          row(cx.t("Mirror shape"), h("label", { class: "field" }, shape, icon("chevd", 14))),
          blockRow(cx.t("Quick access items"), list),
          blockRow(cx.t("Websites"), sitesBox, cx.t("Round tiles in Quick access. A click opens the address in your browser.")),
        );
      });
    draw();
    return box;
  },

  start(cx) {
    let s: Record<string, unknown> = {};
    const apply = (v: Record<string, unknown>) => {
      s = v;
      void follow(cx, v);
    };
    cx.settings.get().then(apply);
    const offS = cx.settings.onChange(apply);

    const offE = cx.on("off-display", () => {
      if (s.followPointer === true && State.mode !== "expanded") void Bridge.reposition();
    });

    return () => {
      offS();
      offE();
      void follow(cx, { followPointer: false, prevScreen: s.prevScreen });
    };
  },
});

/** "Follow the pointer" is the island's own "display under the cursor"; this switches it on and puts the old choice back. */
async function follow(cx: FeatureCtx, v: Record<string, unknown>) {
  const cur = State.settings.screen;
  if (v.followPointer === true && cur !== "cursor") {
    // Set first: the patch comes back as a settings change and must find this done.
    State.settings.screen = "cursor";
    await cx.settings.patch({ prevScreen: cur });
    await Bridge.saveSettings(State.settings);
  } else if (v.followPointer !== true && cur === "cursor" && v.prevScreen) {
    State.settings.screen = String(v.prevScreen);
    await Bridge.saveSettings(State.settings);
  }
}

// ── Mirror ───────────────────────────────────────────────────────────────────

function mirrorPage(cx: FeatureCtx): SectionHost {
  const video = h("video", { class: "f-island-extras-video", autoplay: "", playsinline: "" }) as HTMLVideoElement;
  video.muted = true;
  const msg = h("p", { class: "f-island-extras-msg" });
  const el = h("div", { class: "f-island-extras-mirror" }, video, msg);
  let stream: MediaStream | undefined;
  let seq = 0;
  let release: (() => void) | undefined;

  const stop = () => {
    seq++;
    stream?.getTracks().forEach((t) => t.stop());
    stream = undefined;
    video.srcObject = null;
    release?.();
    release = undefined;
  };

  return {
    el,
    show() {
      stop();
      const mine = seq;
      msg.replaceChildren();
      cx.settings.get().then((s) => el.classList.toggle("circle", s.mirrorShape === "circle"));
      release = cx.island.hold(); // the island stays open while the camera is on
      navigator.mediaDevices
        .getUserMedia({ video: { width: 640, height: 480 }, audio: false })
        .then((st) => {
          // hidden before the camera answered: let it go again at once
          if (mine !== seq) return st.getTracks().forEach((t) => t.stop());
          stream = st;
          video.srcObject = st;
        })
        .catch((e: Error) => {
          if (mine !== seq) return;
          const denied = e.name === "NotAllowedError" || e.name === "SecurityError";
          msg.replaceChildren(
            h("span", { text: cx.t(denied ? "The camera is blocked." : e.name === "NotFoundError" ? "No camera found." : "The camera could not start.") }),
            ...(denied ? [btn(cx.t("Camera privacy settings"), "secondary", () => void cx.call("open", { target: "ms-settings:privacy-webcam" }))] : []),
          );
        });
    },
    hide: stop,
    sync() {},
  };
}

// ── Quick access ─────────────────────────────────────────────────────────────

function quickPage(cx: FeatureCtx): SectionHost {
  const grid = h("div", { class: "f-island-extras-quick" });
  const first = (w: Site) => (Array.from(w.name.trim())[0] ?? "?").toUpperCase();
  const draw = () =>
    cx.settings.get().then((s) => {
      const items = ((s.quick as Quick[]) ?? []).filter((q) => q.target);
      // Only http(s) addresses reach the browser: Bridge.openUrl (open_url) refuses anything else.
      const sites = ((s.sites as Site[]) ?? []).filter((w) => /^https?:\/\//i.test(w.url)).slice(0, MAX_SITES);
      grid.replaceChildren(
        ...(items.length || sites.length
          ? [
              ...items.map((q) => {
                const el = tile("external", q.label || q.target, () => void cx.call("open", { target: q.target }));
                el.title = q.target;
                return el;
              }),
              ...(sites.length
                ? [h("div", { class: "f-island-extras-sites" },
                    ...sites.map((w) =>
                      h("button", { class: "f-island-extras-site", title: w.url, onclick: () => void Bridge.openUrl(w.url) },
                        h("span", { class: "f-island-extras-dot", text: first(w) }),
                        h("span", { class: "f-island-extras-sl", text: w.name }))))]
                : []),
            ]
          : [h("p", { class: "f-island-extras-msg", text: cx.t("Add items in Settings.") })]),
      );
    });
  return { el: grid, show: () => void draw(), hide() {}, sync() {} };
}
