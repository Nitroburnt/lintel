// A Bluetooth accessory connects and the sound output goes to it: what the strip shows.
// The pages of src/features/notifications-accessories and src/features/huds-power post to
// the island's own arbiter (src/island/arbiter.ts), as they do in the app.
//
// What comes before the page is Rust's. Who is told of, and when:
// `cargo run -p notch-feat-notifications-accessories --example accessories-probe`.
// Which level is `quiet`: `cargo run -p notch-feat-huds-power --example huds-probe`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { installFakeDom } from "./fakedom.mjs";

// A feature's index.ts imports its stylesheet, which only the bundler understands.
registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier.endsWith(".css")) return { url: "data:text/javascript,", shortCircuit: true };
    return nextResolve(specifier, context);
  },
});

installFakeDom();
const { Arbiter } = await import("../src/island/arbiter.ts");
const { default: accessories } = await import("../src/features/notifications-accessories/index.ts");
const { default: levels } = await import("../src/features/huds-power/index.ts");

const BUDS = { name: "Aria Buds", kind: "audio", audio: true, connected: true, battery: { percent: 80, left: null, right: null }, tone: "ok" };
const BANNER = "Aria Buds Connected · 80%";
/** The earbuds say their own volume once the output is theirs: Rust marks it `quiet`. */
const THEIRS = { level: 0.96, mute: false, quiet: true };
/** A volume key: the user's. */
const KEY = { level: 0.5, mute: false, quiet: false };

/** Both features started against one strip. `strip()` is the notice on it: its title and detail. */
function start() {
  const arbiter = new Arbiter({ blocked: () => false, open: () => false, hovered: () => false, reveal() {}, changed() {} });
  const events = new Map();
  const cx = (id) => ({
    id,
    t: (key, vars) => key.replace(/\{(\w+)\}/g, (_, name) => String(vars?.[name])),
    call: async () => null,
    on: (event, fn) => (events.set(`${id}:${event}`, fn), () => events.delete(`${id}:${event}`)),
    settings: { get: async () => ({}), patch: async () => {}, onChange: () => () => {} },
    island: { notice: (n) => arbiter.post(n, id), activityChanged() {}, redraw() {}, fullscreen() {} },
  });
  const stops = [accessories.start(cx("notifications-accessories")), levels.start(cx("huds-power"))];
  return {
    strip: () => arbiter.notice && [arbiter.notice.notice.title, arbiter.notice.notice.detail].filter(Boolean).join(" "),
    accessory: (a) => events.get("notifications-accessories:accessory")(a),
    volume: (v) => events.get("huds-power:volume")(v),
    stop: () => stops.forEach((stop) => stop()),
  };
}

test("a connect is announced by name with the battery Windows reports, a disconnect by name", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const { strip, accessory, stop } = start();
  accessory(BUDS);
  assert.equal(strip(), BANNER);
  accessory({ ...BUDS, battery: { percent: null, left: 90, right: 40 }, tone: "warn" });
  assert.equal(strip(), "Aria Buds Connected · L 90% · R 40%");
  accessory({ ...BUDS, battery: null, tone: null });
  assert.equal(strip(), "Aria Buds Connected");
  accessory({ ...BUDS, connected: false, battery: null, tone: null });
  assert.equal(strip(), "Aria Buds Disconnected");
  stop();
});

test("the output goes to the earbuds: the level that switch brings is no notice, and the banner has the strip for its time", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const { strip, accessory, volume, stop } = start();
  // Before the banner (it waits two seconds for the battery)…
  volume(THEIRS);
  assert.equal(strip(), null);
  accessory(BUDS);
  assert.equal(strip(), BANNER);
  // …and after it.
  volume(THEIRS);
  assert.equal(strip(), BANNER);
  t.mock.timers.tick(3999);
  assert.equal(strip(), BANNER);
  t.mock.timers.tick(1);
  assert.equal(strip(), null);
  stop();
});

// A level takes the strip for its 1.6 s, and the banner that waited behind it follows.
test("a volume key at that very moment: the banner follows the level", async (t) => {
  await t.test("the level first", (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const { strip, accessory, volume, stop } = start();
    volume(KEY);
    assert.equal(strip(), "Volume");
    accessory(BUDS);
    assert.equal(strip(), "Volume");
    t.mock.timers.tick(1600);
    assert.equal(strip(), BANNER);
    t.mock.timers.tick(4000);
    assert.equal(strip(), null);
    stop();
  });
  await t.test("the banner first", (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const { strip, accessory, volume, stop } = start();
    accessory(BUDS);
    volume(KEY);
    assert.equal(strip(), "Volume");
    t.mock.timers.tick(1600);
    assert.equal(strip(), BANNER);
    stop();
  });
});
