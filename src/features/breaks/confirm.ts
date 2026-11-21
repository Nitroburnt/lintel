// Breaks — the question asked before something that cannot be taken back: a cleared history, a
// deleted day, an app forgotten. The report window and the island section ask it the same way, in
// the app's own tokens, over a veil that dims whatever is behind it.

import { h } from "../../views/dom";
import { t } from "../../i18n/i18n";
import { btn } from "../../ui/components";

const P = "f-breaks-";

/**
 * "Are you sure you want to perform this action?" — true for Yes; false for No, for Escape, and
 * for a click on the veil.
 */
export function confirmAction(): Promise<boolean> {
  return new Promise((resolve) => {
    let answered = false;
    const answer = (yes: boolean) => {
      if (answered) return;
      answered = true;
      window.removeEventListener("keydown", onKey, true);
      veil.remove();
      resolve(yes);
    };
    const no = btn(t("No"), "ghost", () => answer(false));
    const yes = btn(t("Yes"), "danger", () => answer(true));
    const box = h("div",
      { class: `${P}conf`, role: "dialog", "aria-modal": "true", "aria-label": t("Are you sure you want to perform this action?") },
      h("p", { text: t("Are you sure you want to perform this action?") }),
      h("div", { class: `${P}confacts` }, no, yes));
    const veil = h("div", { class: `${P}veil` }, box);
    // The window behind closes on Escape and moves on the arrow keys: while the question is up,
    // the keys are the question's.
    function onKey(e: KeyboardEvent) {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      answer(false);
    }
    veil.addEventListener("pointerdown", (e) => {
      if (e.target === veil) answer(false);
    });
    window.addEventListener("keydown", onKey, true);
    document.body.append(veil);
    // No has the focus: the safe answer is the one Enter would give by accident.
    no.focus();
  });
}
