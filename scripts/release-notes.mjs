// Prints the notes of a release: how to install, the installer's SHA-256, and
// that version's entry in CHANGELOG.md. Used by the release workflow.
//
//   node scripts/release-notes.mjs <version>      (after `npm run pack`)

import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const version = process.argv[2];
if (!/^\d+\.\d+\.\d+$/.test(version ?? "")) {
  console.error("release-notes: give the version, for example 1.0.1");
  process.exit(1);
}

// The version's section of the changelog: from its heading to the next one.
const changelog = readFileSync(join(root, "CHANGELOG.md"), "utf8").split(/\r?\n/);
const from = changelog.findIndex((l) => new RegExp(`^## \\[?${version.replaceAll(".", "\\.")}\\b`).test(l));
if (from < 0) {
  console.error(`release-notes: CHANGELOG.md has no entry for ${version}`);
  process.exit(1);
}
const rest = changelog.slice(from + 1);
const to = rest.findIndex((l) => /^## /.test(l));
const entry = (to < 0 ? rest : rest.slice(0, to)).join("\n").trim();

const file = `lintel-${version}-setup.exe`;
const path = join(root, "release", file);
const sha = existsSync(path) ? createHash("sha256").update(readFileSync(path)).digest("hex") : null;

console.log(`> **Notice:** This is an unofficial fork release of Lintel maintained by **Nitroburnt**. It contains additional features and refinements that will be submitted upstream.
>
> - **Fork Repository & Source:** https://github.com/Nitroburnt/lintel/tree/v${version}
> - **Upstream Project & Guide:** https://lintel.thisissbk.com (Original by Shakib Bin Kabir)

Lintel is a notch toolkit for Windows: an island that stays hidden at the top of the screen and comes out when you point at it or when something needs you.

## Install

1. Download \`${file}\` below.
2. Run it. It installs for your account only and does not ask for administrator rights.
3. The installer is not code-signed, so Windows SmartScreen shows a warning. Choose "More info", then "Run anyway", if you trust this download.

Requires Windows 10 or 11 (64-bit).
${sha ? `\n**SHA-256 of \`${file}\`:** \`${sha}\`\n` : ""}
## What changed in ${version}

${entry}

## License and Attribution

This release is licensed under the GNU General Public License v3.0 or later (GPL-3.0-or-later).
- Fork source tag: [v${version}](https://github.com/Nitroburnt/lintel/tree/v${version})
- Fork [LICENSE](https://github.com/Nitroburnt/lintel/blob/v${version}/LICENSE)
- Upstream attribution & credits: [NOTICE.md](https://github.com/Nitroburnt/lintel/blob/v${version}/NOTICE.md)
- Upstream code signing policy: https://lintel.thisissbk.com/download.html#signing`);
