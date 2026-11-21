// The page behind every feature window (docs/ARCHITECTURE.md §6).
//
// Rust opens `window.html?feature=<id>&page=<name>` (winfactory.rs); this boots
// the bridge, the language and the registry, then hands the root element to
// `windows[page]` of that feature. One Vite entry serves every window, so a
// feature adds a window without touching a shared file.

import "./window.css";
import { Bridge, onEvent } from "../core/bridge";
import { featureCtx, loadFeature } from "../core/registry";
import type { Settings } from "../core/state";
import { resolveLanguage, setLanguage, systemLanguages } from "../i18n/i18n";
import { installTooltip } from "../ui/tooltip";

// The design language, as far as it exists: tokens first, then the components.
import.meta.glob("../ui/tokens.css", { eager: true });
import.meta.glob("../ui/components.css", { eager: true });

async function main() {
  const root = document.getElementById("window-root");
  if (!root) return;
  const params = new URLSearchParams(location.search);
  const id = params.get("feature") ?? "";
  const page = params.get("page") ?? "";
  // A transparent window (winfactory.rs) shows only what its page draws.
  document.body.classList.toggle("transparent", params.has("transparent"));
  installTooltip();

  const boot = await Bridge.boot();
  const follow = (picked: string | undefined) => setLanguage(resolveLanguage(picked, systemLanguages()));
  follow(boot?.settings.language);
  void onEvent<Settings>("settings-changed", (s) => follow(s.language));

  // Only this feature's chunk, not the other sixty: a window opens quickly.
  const mount = (await loadFeature(id))?.windows?.[page];
  if (!mount) {
    void Bridge.log(`window ${id}/${page}: no such page`);
    return;
  }
  mount(root, featureCtx(id));
  void Bridge.log(`window ${id}/${page} mounted`);
}

void main();
