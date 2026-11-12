// Voice — the page side of the voice assistant (docs/VOICE.md; docs/ARCHITECTURE.md §2.3).
// Rust is ears, mouth and engines (crates/voice); this page is language and actions (VOICE §3.1).
// Nothing runs at import: everything begins in `start` and ends in the function it returns.
//
// Two things live here.
// 1. The contract with crates/voice/src/lib.rs, as types: the events it sends (`VoiceEvents`), the
//    methods it takes (`VoiceMethods`) and the settings of its manifest (`VoiceSettings`). Both halves
//    are written against these names; a change on one side is a change on the other.
// 2. The wiring: which module gives the feature its activity, its window page, its settings page, its
//    own actions, and what `start` starts. Every module is one package's (VOICE §10) and exports its
//    final signatures from the first day, so no package edits another's file.
//
// Owned by the package voice-runner (with turn.ts, runner.ts, registry.ts, base-actions.ts). The turn
// loop and the runner know neither the registry nor the models: `start` hands them over, so they run
// under `node --test` (this file, with its stylesheet and `import.meta.glob` behind it, does not).

import "./style.css";
import { Bridge } from "../../core/bridge";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { pillDefinition } from "../../core/pills";
import { featureCtx, featureOn, loadAllStrings } from "../../core/registry";
import { State } from "../../core/state";
import { stateAbout } from "./answers/index";
import { startChatAloud } from "./brain/claude";
import { choose, prepare, pushTools } from "./brain/needle";
import { answer, type BrainId } from "./brain/route";
import { cxOf, entries, onRegistry, startRegistry } from "./registry";
import { settingsPage } from "./settings";
import { startTurns } from "./turn";
import { capsulePage, startUi, turnActivity } from "./ui/index";
import type { Caption, Origin } from "./ui/rules";
import { startWake } from "./wake";

// ── The contract with Rust (VOICE §3.5, as crates/voice/src/lib.rs names it) ──

/** §3.4. `paused` comes with a `reason`. */
export type VoiceState =
  | "off" | "idle" | "waiting" | "hearing" | "listening" | "thinking" | "speaking" | "asking" | "closing" | "paused";

export type PauseReason = "tray" | "lock" | "sleep" | "displayOff" | "energySaver" | "fullscreen" | "micBusy" | "micMuted";

/**
 * What a turn waits for (the first-word and end-of-turn rows of §3.4). `command`: after a wake or a
 * tap. `reply`: an answer to a question. `yesNo`. `file`: a dropped file, 20 s. `freeText`: a message
 * being dictated, the long silence.
 */
export type ListenKind = "command" | "reply" | "yesNo" | "file" | "freeText";

/** One voice of the picker. `id` is `<engine>:<the engine's own id>` (`kokoro:af_heart`): what the setting `voice` holds. */
export interface VoiceDesc {
  id: string;
  name: string;
  engine: "kokoro" | "windows" | "eleven";
  /** BCP 47: `en-US`, `en-GB`. */
  lang: string;
}

/** A download or an engine, as its `status` method says. */
export interface EngineStatus {
  installed: boolean;
  downloading: boolean;
  running: boolean;
}

/** What the action model chose for a sentence (`needle.choose`). `tool` is empty when it called nothing; `no_action` is a tool too. */
export interface NeedleChoice {
  tool: string;
  args: Record<string, unknown>;
  confidence: number;
  /** The engine itself held the call back. */
  suppressed: boolean;
  /** The further calls it found in the same sentence, in the order said. */
  more?: { tool: string; args: Record<string, unknown> }[];
}

/** The events, each sent as `voice:<name>`: listen with `voiceOn(cx, name, fn)`. */
export interface VoiceEvents {
  /** On every change of state. `gen` is the generation: it goes into `mouth.say`, and what is held for an older one is stale (§7.4). */
  state: { state: VoiceState; reason?: PauseReason; gen: number };
  /** 20 times a second, during a turn only; each 0…1. The ears send `mic` alone and the mouth `out` alone: whoever draws keeps the last of each. */
  level: { mic?: number; out?: number };
  /** A partial about once a second, then the final one. */
  heard: { text: string; final: boolean };
  /** A chunk starts playing. */
  spoke: { gen: number; seq: number };
  /** Progress of a model download. */
  download: { what: "needle" | "speech" | "generator" | "whistle"; received: number; total: number; done: boolean; error?: string };
  /**
   * How a turn ended (ears.rs), in milliseconds: the row of §3.4 it ended by, what was recorded, how
   * much of it was voiced and from where to where, the speech engine's time over its runs.
   */
  turn: {
    gen: number;
    end: "silence" | "noSpeech" | "limit" | "released";
    rule: number;
    audio: number;
    voiced: number;
    stt: number;
    runs: number;
    voice: [number, number];
  };
  /** Something the user should be told: a turn that could not start (the speech model is missing), a microphone that is gone. Shown, and said once (turn.ts). */
  error: { message: string };
  /** `caption.show` passed on to the capsule's page: what it draws, or `out` when it is put away. */
  caption: Caption | { out: true };
  /** `caption.act` passed on to the island's page. */
  "caption-act": CaptionAct;
}

/** A click in the capsule (`stop`, `undo`, `open`), its page just made (`ready`), or the settings page's "Try the voice" (`try`). */
export interface CaptionAct {
  what: "ready" | "stop" | "undo" | "open" | "try";
}

/**
 * The methods of `Feature::call`, dispatched by their prefix to the Rust module of that name: what
 * each takes and what it answers. Call them with `voiceCall(cx, method, args)`. A method a package
 * adds to its own Rust module is called with `cx.call` until it is written down here.
 */
export interface VoiceMethods {
  "state": () => VoiceEvents["state"];
  /** The page's phases, when Rust cannot know them. */
  "state.set": (a: { state: Exclude<VoiceState, "off" | "paused"> }) => VoiceEvents["state"];

  /** Starts a turn or the next part of one: opens the microphone, interrupts what is being said. */
  "ears.listen": (a: { kind?: ListenKind }) => { gen: number };
  /** The turn ends. */
  "ears.stop": () => object;
  /** `mic`: the microphone is open now. `model`: the speech model is downloaded. `stalled`: the capture stopped answering and is being rebuilt. */
  "ears.status": () => { mic: boolean; model: boolean; stalled: boolean };
  /** Fetches the speech-to-text engine and its model (19 MB); progress as `voice:download {what: "whistle"}`. */
  "ears.download": () => object;

  /** Queues a chunk; `seq` counts from 0 within the generation. `voice`: a `VoiceDesc.id` for this chunk only (the picker's Test). */
  "mouth.say": (a: { gen: number; seq: number; text: string; voice?: string }) => object;
  /** Silences at once; answers the new generation. */
  "mouth.stop": () => { gen: number };
  "mouth.voices": () => VoiceDesc[];
  "mouth.speaking": () => { speaking: boolean };

  /** The registry's tools, at start and when a feature is switched. `sentence`: the few that fit one sentence; `warm`: start the server with them now. */
  "needle.tools": (a: { tools: unknown[]; sentence?: boolean; warm?: boolean }) => object;
  "needle.choose": (a: { text: string; system?: string }) => NeedleChoice;
  "needle.status": () => EngineStatus;
  "needle.download": () => object;

  "speech.status": () => EngineStatus;
  "speech.download": () => object;
  "speech.remove": () => object;
  "speech.warm": () => object;
  "speech.unload": () => object;

  "generator.status": () => EngineStatus & { model: string };
  "generator.download": () => object;
  "generator.remove": () => object;
  /** Starts the local server if it must; what `chat.ask {provider: "builtin"}` is handed. */
  "generator.endpoint": () => { url: string; model: string };
  /** A token arrived: the idle clock starts again. */
  "generator.touch": () => object;
  "generator.unload": () => object;
  /** The system prompt and tools the first real request will begin with: the warm-up caches them. */
  "generator.prefix": (a: { messages: unknown[]; tools?: unknown[] }) => object;

  /** Physical pixels of the screen; opens the capsule's window if it is closed, never activates it. */
  "caption.place": (a: { x: number; y: number; w: number; h: number }) => object;
  "caption.hide": () => object;
  /** Where the island's window hangs, physical pixels: its top centre, its scale, the bounds of its display. */
  "caption.origin": () => Origin;
  /** The island's page to the capsule's, as `voice:caption`. The words pass through and are never logged. */
  "caption.show": (a: VoiceEvents["caption"]) => object;
  /** To the island's page, as `voice:caption-act`. */
  "caption.act": (a: CaptionAct) => object;

  "eleven.status": () => { on: boolean; key: boolean };
  "eleven.voices": () => VoiceDesc[];

  /** `day`: 0 today, 1 tomorrow. Refused while the switch is off or the city is empty. */
  "weather.now": (a: { day?: 0 | 1 }) => {
    city: string; temp: number; unit: string; feels: number; high: number; low: number; code: number; text: string; rainChance: number;
  };
  "weather.find": (a: { city: string }) => { name: string; country: string; lat: number; lon: number }[];
  /** A short answer from Wikipedia. Refused while the lookup switch is off. */
  "weather.lookup": (a: { query: string }) => { source: string; title: string; text: string };

  /** The Start Menu's programs, for "open <app>". */
  "files.apps": () => { name: string; path: string }[];
  /** A path `files.apps` gave, nothing else. */
  "files.open": (a: { path: string }) => object;

  "wake.status": () => { wanted: boolean; phrase: string; emptyWakes: number };
}

/** The settings of the Rust manifest (VOICE §9 with the owner's decisions). */
export interface VoiceSettings {
  /** The talk key, pressed on its own: a key name, "" when cleared. */
  key: string;
  /** Holding the talk key talks until it is released. */
  hold: boolean;
  /** `plugged`: only on mains power. */
  wake: "off" | "on" | "plugged";
  speak: boolean;
  shortReplies: "spoken" | "sound";
  captions: boolean;
  /** A `VoiceDesc.id`; "" is Kokoro when it is downloaded, else the first English Windows voice. */
  voice: string;
  kokoroVoice: string;
  rate: number;
  winRate: number;
  winPitch: number;
  /** Minutes the natural voice stays loaded. */
  keepVoice: number;
  bargeIn: boolean;
  esc: boolean;
  duck: boolean;
  needle: boolean;
  brain: "auto" | "off" | BrainId;
  threads: number;
  /** Minutes the text model stays loaded. */
  keepModel: number;
  localServer: boolean;
  freeModel: boolean;
  claude: boolean;
  claudeSearch: boolean;
  eleven: boolean;
  elevenVoice: string;
  weather: boolean;
  city: string;
  /** Questions the local model cannot answer go to a free lookup service. */
  lookup: boolean;
  chatAloud: boolean;
}

/** `cx.call`, typed by `VoiceMethods`. Null when the call failed (logged) or voice is off. */
export function voiceCall<M extends keyof VoiceMethods>(
  cx: Pick<FeatureCtx, "call">,
  method: M,
  ...args: Parameters<VoiceMethods[M]>
): Promise<ReturnType<VoiceMethods[M]> | null> {
  return cx.call(method, args[0] as object | undefined);
}

/** `cx.on`, typed by `VoiceEvents`; returns the unsubscribe. */
export function voiceOn<E extends keyof VoiceEvents>(
  cx: Pick<FeatureCtx, "on">,
  event: E,
  fn: (payload: VoiceEvents[E]) => void,
): () => void {
  return cx.on(event, fn);
}

// ── The feature ───────────────────────────────────────────────────────────────

/**
 * What the card that waits is asking, for the refusal that reads it aloud ("I can't approve by
 * voice"): who, and the tool or the question. Never the command.
 * debt: read from the island's state, which has no seam for it yet.
 */
function asking(cx: FeatureCtx): string {
  const card = State.pendingApproval;
  if (!card) return "";
  const who = pillDefinition(card.family)?.name ?? cx.t("An agent");
  const question = card.questions?.[0]?.question;
  if (question) return cx.t("{who} asks: {question}", { who, question });
  return cx.t("{who} wants to use {tool}.", { who, tool: card.tool });
}

/** Esc as the island's keyboard model hands it to the page (ARCHITECTURE §4.1): heard on `window`, never taken. */
function onEsc(fn: () => void): () => void {
  const key = (e: KeyboardEvent) => {
    if (e.key === "Escape") fn();
  };
  window.addEventListener("keydown", key);
  return () => window.removeEventListener("keydown", key);
}

export default defineFeature({
  id: "voice",
  activities: [turnActivity],
  settings: settingsPage,
  windows: { capsule: capsulePage },
  voice: () => import("./base-actions"),
  start: (cx) => {
    // Said to it, a feature that is off is still named: its strings are read here,
    // where the island otherwise reads only those of the features that run.
    void loadAllStrings();
    const stops = [
      // The action model gets its tools after each read of the registry, the first one included.
      onRegistry((list) => void pushTools(cx, list, cxOf)),
      startRegistry(),
      startTurns(cx, {
        entries,
        cxOf,
        choose,
        prepare,
        answer,
        stateAbout,
        // What plays is lowered through the mixer, while that feature is on.
        mixer: () => (featureOn("audio-mixer") ? featureCtx("audio-mixer") : null),
        asking: () => asking(cx),
        onEsc,
        log: (line) => void Bridge.log(line),
      }),
      startUi(cx),
      startWake(cx),
      startChatAloud(cx),
    ];
    return () => {
      for (const stop of stops) stop();
    };
  },
});
