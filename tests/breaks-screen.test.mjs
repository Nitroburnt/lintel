// The Screen time report's own, pure parts (src/features/breaks/screen.ts): the days the week
// draws, the finder's matching, the budget's share of the day, and the ring — including the
// second, thinner arc the budget adds. No window, no Rust: only what the page can answer alone.

import { test } from "node:test";
import assert from "node:assert/strict";
import { installFakeDom } from "./fakedom.mjs";

installFakeDom();

const { budgetFrac, dayDate, lastDays, matches, ring, shiftDay } = await import("../src/features/breaks/screen.ts");

const app = (over = {}) => ({ key: "chrome", name: "Google Chrome", cat: "browsing", secs: 60, path: "", ...over });

// ── The days a week shows ─────────────────────────────────────────────────────

test("the last days end at today and run oldest first", () => {
  assert.deepEqual(lastDays("2026-11-18", 3), ["2026-11-16", "2026-11-17", "2026-11-18"]);
  const week = lastDays("2026-11-18", 7);
  assert.equal(week.length, 7);
  assert.equal(week[6], "2026-11-18");
  assert.equal(week[0], "2026-11-12");
});

test("the week crosses a month and a year without drifting", () => {
  assert.deepEqual(lastDays("2026-12-31", 3), ["2026-12-29", "2026-12-30", "2026-12-31"]);
  assert.deepEqual(lastDays("2027-01-02", 4), ["2026-12-30", "2026-12-31", "2027-01-01", "2027-01-02"]);
  // 2028 is a leap year: 29 February is a day like any other.
  assert.deepEqual(lastDays("2028-03-01", 3), ["2028-02-28", "2028-02-29", "2028-03-01"]);
});

test("a day shifts both ways and back", () => {
  assert.equal(shiftDay("2026-03-01", -1), "2026-02-28");
  assert.equal(shiftDay("2026-02-28", 1), "2026-03-01");
  assert.equal(shiftDay("2026-12-31", 1), "2027-01-01");
  assert.equal(shiftDay(shiftDay("2026-11-18", -5), 5), "2026-11-18");
  // A date built from the key is that local day, whatever the time zone.
  const d = dayDate("2026-11-18");
  assert.equal(d.getFullYear(), 2026);
  assert.equal(d.getMonth(), 10);
  assert.equal(d.getDate(), 18);
});

// ── The finder ────────────────────────────────────────────────────────────────

test("the finder matches a name, a key or a category, ignoring case and spaces", () => {
  assert.ok(matches("", app()));
  assert.ok(matches("   ", app()));
  assert.ok(matches("chrome", app()));
  assert.ok(matches("CHROME", app()));
  assert.ok(matches("google", app()));
  assert.ok(matches("brows", app()));
  assert.ok(matches(" chro ", app()));
  assert.equal(matches("firefox", app()), false);
  assert.equal(matches("editor", app()), false);
});

test("the finder never matches what the row does not show", () => {
  // The path is not searched: a key that happens to be a path part is not the app's name.
  assert.equal(matches("program files", app({ path: "C:\\Program Files\\Chrome\\chrome.exe" })), false);
});

// ── The budget ────────────────────────────────────────────────────────────────

test("the budget's share of the day runs from none to over", () => {
  assert.equal(budgetFrac(0, 0), 0); // no budget: no arc
  assert.equal(budgetFrac(3600, 120), 0.5);
  assert.equal(budgetFrac(0, 120), 0);
  assert.equal(budgetFrac(7200, 120), 1);
  assert.equal(budgetFrac(10_800, 120), 1.5);
  assert.equal(budgetFrac(-5, 120), 0);
});

// ── The ring ─────────────────────────────────────────────────────────────────

const circles = (svg) => svg.children.filter((c) => c.tagName === "CIRCLE");
const classes = (c) => c.getAttribute("class") ?? "";

test("the ring draws a track and one arc per category", () => {
  const svg = ring([{ cat: "development", frac: 0.5 }, { cat: "browsing", frac: 0.5 }]);
  const all = circles(svg);
  assert.equal(all.length, 3);
  assert.match(classes(all[0]), /f-breaks-track/);
  assert.match(classes(all[1]), /f-breaks-c-development/);
  assert.match(classes(all[2]), /f-breaks-c-browsing/);
  // The arc's length is its share of the 100-long circle, minus the hair between arcs.
  assert.equal(all[1].getAttribute("stroke-dasharray"), "49.1 50.9");
  assert.equal(all[1].getAttribute("stroke-dashoffset"), "0");
  assert.equal(all[2].getAttribute("stroke-dashoffset"), "-50");
});

test("an arc is a button of its own and leaves the middle to the caller", () => {
  const picked = [];
  const svg = ring([{ cat: "utilities", frac: 1 }], (cat) => picked.push(cat));
  const arc = circles(svg)[1];
  arc.fire("click");
  assert.deepEqual(picked, ["utilities"]);
});

test("a budget adds a second, thinner ring inside the day's", () => {
  const withBudget = circles(ring([{ cat: "other", frac: 1 }], undefined, 0.25));
  const limit = withBudget.find((c) => classes(c).includes("f-breaks-limit"));
  assert.ok(limit, "the budget arc is missing");
  assert.equal(limit.getAttribute("r"), "12.6");
  assert.equal(limit.getAttribute("stroke-dasharray"), "25 75");
  assert.doesNotMatch(classes(limit), /over/);
});

test("a budget already used, or passed, fills the inner ring and turns it", () => {
  const full = circles(ring([{ cat: "other", frac: 1 }], undefined, 1)).find((c) => classes(c).includes("f-breaks-limit"));
  assert.equal(full.getAttribute("stroke-dasharray"), "100 0");
  assert.match(classes(full), /over/);
  const past = circles(ring([{ cat: "other", frac: 1 }], undefined, 1.8)).find((c) => classes(c).includes("f-breaks-limit"));
  // Never more than a full ring: the day is measured, not counted twice.
  assert.equal(past.getAttribute("stroke-dasharray"), "100 0");
  assert.match(classes(past), /over/);
});

test("without a budget there is no second ring", () => {
  const svg = ring([{ cat: "other", frac: 1 }]);
  assert.equal(circles(svg).some((c) => classes(c).includes("f-breaks-limit")), false);
});
