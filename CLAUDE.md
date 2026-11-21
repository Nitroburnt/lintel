# Lintel — guide for AI coding agents

Lintel is a Windows-only notch toolkit: an island that stays hidden and slides out of the top edge of the screen when hovered or when something needs the user. It hosts AI coding agent sessions and approvals, chat, media, system HUDs, a file shelf and a large set of system utilities, each of which can be turned off. Version 2.1.0, GPL-3.0-or-later.

It is a Tauri 2 app (Rust backend, plain TypeScript + Vite frontend, no UI framework). The three projects it grew from are credited in `NOTICE.md`. The crates and some identifiers keep the old code name `notch` (`notch-feat-*`, `notch_core`); do not rename them (`docs/ARCHITECTURE.md` §13).

## Where things are
- `crates/<feature>/` — one Rust crate per feature (`notch-feat-<id>`), implementing `notch_core::Feature`. `crates/svc-<name>/` — one shared service per crate. `crates/core/` — the `Feature` trait, `Ctx`, storage, log, net, secrets, i18n, config_file, Win32 helpers. `crates/brand/` — the name constants.
- `src-tauri/` — the app host: island window, registry host (`feature_call`), settings, tray, shortcuts, pipe server, window factory, and the base features (agents, chat, integrations, recap, drop) as built-in adapters. Its `build.rs` generates the feature list from its `Cargo.toml`; `scripts/new-feature.mjs` is the only thing that edits that file.
- `src/features/<feature>/` — one TypeScript directory per feature: `index.ts` (`defineFeature`), `style.css` (`.f-<id>-` prefix), `strings.json` (both sides read it), `tsconfig.json` (checks this feature alone). Discovered by `src/core/registry.ts` with `import.meta.glob`.
- `src/core/` (bridge, state, registry, feature types, brand, sound+synth), `src/ui/` (design tokens, components, icons: the only look), `src/island/` (fsm, arbiter, sections, island), `src/views/` (base views, `dom.ts`), `src/mascot/`, `src/settings/`, `src/window/` (host page for every feature window), `src/i18n/`.
- `hook/` — the relay `lintel-hook` that agent CLIs spawn on each hook event; named pipe to the app. `helper/` — the elevated hardware helper `lintel-hw` (.NET).
- `docs/ARCHITECTURE.md` — layout, registry, island content model, design language, windowing, shared services, mascot, sounds, helper, privacy rules, the name. Read §2, §3, §5, §7 before a feature; §6 for windows; §8 for the services you use.
- `docs/FEATURES.md` — every feature, its upstream, Windows mapping, crate and status. `docs/VOICE.md` — the voice assistant. `docs/PRIVACY.md`, `docs/ROADMAP.md`.
- `NOTICE.md`, `licenses/` — upstream attribution.

## Build
```
npm install
npm run build            # type-check + vite build (also builds the hook relay)
npm test                 # node --test suites
cargo check --workspace  # needs dist/ to exist, so run `npm run build` first
npm run tauri dev        # run the app
npm run pack             # installer into release/
npm run fake-session     # a pretend agent session through the relay, while the app runs
node scripts/check-links.mjs && node scripts/check-brand.mjs   # docs links, upstream names
```
For one feature: `cargo check -p notch-feat-<id>` and `npx tsc --noEmit -p src/features/<id>`.

## Rules
- **Windows 10/11 only.** Do not add or keep Linux or macOS branches.
- **No upstream branding.** The names and characters of the projects Lintel grew from, and their icons, sounds and media, must not appear in shipped code or assets; they belong in `NOTICE.md`, `licenses/` and `docs/` only (`scripts/check-brand.mjs`).
- **Ported code is marked.** When a file is substantially ported from an upstream file, name that upstream file in a header comment.
- **No telemetry, no account.** Network calls go only to services the user configured.
- **Secrets live in Windows Credential Manager** (the `keyring` crate), never on disk or in git.
- **Never block an agent CLI.** If the app does not answer, the hook exits immediately.
- **Never overwrite an agent's settings file.** Dated backup, merge, show the diff, write only after the user confirms.
- **Never act for the user without a click**: no approving a permission, sending an email or deleting files without an explicit confirmation.
- **Idle means idle.** 0 % CPU when the island is hidden and nothing is active. Prefer event-driven Windows APIs over polling; any poller pauses when nothing is watching it.
- **Every feature can be turned off**, and a disabled feature loads nothing: no hooks, timers, threads or windows.
- **The app runs unelevated.** Only the hardware helper (temperatures and fan control) elevates, and only after the user opts in.
- **Plain TypeScript, DOM and Canvas.** No UI framework. Reuse the helpers in `src/views/dom.ts` and the components in `src/ui/`; follow the design language in `docs/ARCHITECTURE.md` §7.
- **Minimal code.** Reuse what is already in the tree before writing new helpers. Prefer the `windows` crate and the standard library over a new dependency. Mark a known shortcut with a one-line `// debt:` comment.
- **Nothing runs unless `start` ran.** No side effects at Rust construction or TypeScript import; everything a feature does begins in `start`/`start()` and ends in `stop`. Subscribe to shared services through their handles so dropping the handle unsubscribes.
- **Use the island through `IslandApi`** (`notice`, `reveal`, `open`, `activityChanged`, `redraw`, `fullscreen`, `hold`) and the settings through `FeatureCtx.settings`; never reach into `State` or `Island` from a feature.
- **Strings** go in the feature's `strings.json`, English key to the ten languages. Never put upstream names in a string.
- **Examples have crate-unique names.** Every example of the workspace links to `target/<profile>/examples/<name>.exe`, so name it after its crate (`media-probe`, `shell-probe`), never just `probe`.
- **Only touch what you started.** Other programs run on the machine at the same time. Never attach to a browser, WebView or debug port you did not start yourself (use a random free port), and never stop a process you did not start.
- **Never open a window from a synchronous Tauri command.** Building a webview on the main thread inside a sync `#[tauri::command] fn` hangs with a blank page. Open windows through the window factory from async code.
- **Git.** Do not commit or push unless asked.
