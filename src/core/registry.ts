// The feature registry, page side (docs/ARCHITECTURE.md §2.3, §3).
//
// The only file with import.meta.glob: every src/features/<id>/index.ts is a
// feature and every src/features/<id>/strings.json its strings, found by Vite
// at build time. Nothing else imports a feature directly, and adding one edits
// no shared file. (import.meta.glob does not exist under `node --test`: tests
// never import this file.)

import { mergeStrings, t } from "../i18n/i18n";
import { Bridge, onEvent, type FeatureInfo } from "./bridge";
import { collectVoice, type FeatureCtx, type FeatureModule, type IslandApi, type Value, type VoiceEntry } from "./feature";

const modules = import.meta.glob("../features/*/index.ts", { eager: true });

// Feature strings are a chunk each, read when a window needs them, not at import:
// the island draws only the features that run, so the tables of the rest (all ten
// languages of them) are text it would never look up.
const tables = import.meta.glob("../features/*/strings.json") as Record<
  string,
  () => Promise<{ default?: { strings?: Record<string, unknown> } }>
>;

const merged = new Set<string>();

const idOfTable = (path: string) => path.slice("../features/".length).split("/")[0];

/** Merges the tables not merged yet: those of `only`, or every one of them. */
async function mergeTables(only?: ReadonlySet<string>): Promise<void> {
  const todo = Object.entries(tables).filter(([path]) => !merged.has(path) && (!only || only.has(idOfTable(path))));
  await Promise.all(
    todo.map(async ([path, load]) => {
      merged.add(path);
      try {
        const module = await load();
        mergeStrings(module.default?.strings ?? {});
      } catch (err) {
        console.error(`[feature ${idOfTable(path)}] strings failed`, err);
      }
    }),
  );
}

/**
 * Every feature's strings, for a window that names features whether they run or
 * not: Settings lists them all, so its titles must translate for a feature that
 * is off (the island never draws one).
 */
export function loadAllStrings(): Promise<void> {
  return mergeTables();
}

export const FEATURES: FeatureModule[] = Object.values(modules).map((m) => (m as { default: FeatureModule }).default);

export function featureById(id: string): FeatureModule | undefined {
  return FEATURES.find((f) => f.id === id);
}

/**
 * Every voice action of the features that are on (docs/VOICE.md §5): what the voice feature reads,
 * at its start and after `onFeaturesChanged`. A feature that is off contributes nothing, and no
 * `voice.ts` is loaded before this is first called.
 */
export function voiceActions(): Promise<VoiceEntry[]> {
  return collectVoice(FEATURES, featureOn);
}

// ── What a feature is handed ──────────────────────────────────────────────────

/** Until the island shell hands over its own (and for good in the other windows). */
const inert: IslandApi = {
  notice() {},
  reveal() {},
  open() {},
  activityChanged() {},
  inHeader: () => false,
  redraw() {},
  fullscreen() {},
  hold: () => () => {},
  mode: () => "hidden",
  onMode: () => () => {},
  close() {},
  ear() {},
  shape: () => ({ mode: "hidden", width: 0, height: 0, top: 0 }),
  face() {},
};
let islandFor: (feature: string) => IslandApi = () => inert;

/** The island page gives features its real API once it is built: one per feature id. */
export function setIslandApi(make: (feature: string) => IslandApi) {
  islandFor = make;
}

/** `onEvent` with an unsubscribe that works before the listener is in place. */
function listen<T>(name: string, fn: (payload: T) => void): () => void {
  let gone = false;
  let off = () => {};
  void onEvent<T>(name, fn).then((unlisten) => {
    if (gone) unlisten();
    else off = unlisten;
  });
  return () => {
    gone = true;
    off();
  };
}

export function featureCtx(id: string): FeatureCtx {
  return {
    id,
    call: <T>(method: string, args?: object) => Bridge.featureCall<T>(id, method, args),
    on: <T>(event: string, fn: (payload: T) => void) => listen<T>(`${id}:${event}`, fn),
    settings: {
      get: async () => (await Bridge.featureSettings(id)) ?? {},
      patch: async (patch) => void (await Bridge.featurePatchSettings(id, patch)),
      onChange: (fn) => listen<Value>(`settings-changed:${id}`, fn),
    },
    get island() {
      return islandFor(id);
    },
    t,
  };
}

// ── Which features are on ─────────────────────────────────────────────────────

let infos: FeatureInfo[] = [];
const watchers = new Set<() => void>();

/** Every feature as Rust lists it (id, name, group, on, running…), as last read. */
export function featureInfos(): readonly FeatureInfo[] {
  return infos;
}

/** True while the feature runs. The gate for every surface a feature owns. */
export function featureOn(id: string): boolean {
  return infos.some((f) => f.id === id && f.running);
}

/** Called after the list was read again (a feature was switched on or off). */
export function onFeaturesChanged(fn: () => void): () => void {
  watchers.add(fn);
  return () => watchers.delete(fn);
}

let loading: Promise<void> | null = null;

/** Reads the list and keeps it fresh. Calling it again waits for the same read. */
export async function loadFeatures(): Promise<readonly FeatureInfo[]> {
  const read = async () => {
    infos = (await Bridge.featuresList()) ?? [];
    // What runs is what this window draws, and what it draws needs its strings:
    // a feature switched on later is read here, before the watchers redraw.
    await mergeTables(new Set(infos.filter((f) => f.running).map((f) => f.id)));
  };
  loading ??= (async () => {
    await read();
    await onEvent("features-changed", async () => {
      await read();
      for (const fn of [...watchers]) fn();
    });
  })();
  await loading;
  return infos;
}

// ── Running them (island page only) ───────────────────────────────────────────

const running = new Map<string, () => void>();

function startOne(feature: FeatureModule) {
  if (running.has(feature.id)) return;
  const cx = featureCtx(feature.id);
  const shortcuts = feature.shortcuts;
  const offShortcuts = shortcuts
    ? cx.on<{ id: string }>("shortcut", ({ id }) => shortcuts[id]?.(cx))
    : () => {};
  let dispose: (() => void) | void;
  try {
    dispose = feature.start?.(cx);
  } catch (err) {
    console.error(`[feature ${feature.id}] start failed`, err);
  }
  running.set(feature.id, () => {
    offShortcuts();
    if (typeof dispose === "function") dispose();
  });
}

function stopOne(id: string) {
  const dispose = running.get(id);
  running.delete(id);
  try {
    dispose?.();
  } catch (err) {
    console.error(`[feature ${id}] dispose failed`, err);
  }
}

function syncRunning() {
  for (const feature of FEATURES) {
    if (featureOn(feature.id)) startOne(feature);
    else stopOne(feature.id);
  }
}

/**
 * The island page, once booted: runs `start` of every feature that is on, and
 * from then on follows the switches (start on, dispose off).
 */
export async function startFeatures() {
  await loadFeatures();
  syncRunning();
  onFeaturesChanged(syncRunning);
}
