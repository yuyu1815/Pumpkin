# Third-party notices: PrismarineJS `minecraft-data` cache

This directory uses a pinned upstream data source only for reproducible 26.1 comparison work. The raw JSON is **not a distributed repository asset**: it is stored, when fetched, only below the ignored `tools/compat-data/.cache/` directory.

## Source and pin

- Repository: <https://github.com/PrismarineJS/minecraft-data>
- Commit: `20a0b0142d99d9a069aa8fec98512ec082b4f0a3`
- Raw source URL pattern: `https://raw.githubusercontent.com/PrismarineJS/minecraft-data/20a0b0142d99d9a069aa8fec98512ec082b4f0a3/data/pc/26.1/`
- Reproduction: `py -3 tools/compat-data/compat_data.py fetch`
- Integrity: `tools/compat-data/manifest.json` records the exact raw URL, byte count, and SHA-256 for each fetched file. A mismatch fails closed.

## What the pinned upstream files actually say

At this commit, the repository root has no separately fetchable `LICENSE` or `NOTICE` file: the pinned raw URLs `.../LICENSE` and `.../NOTICE` return HTTP 404. The pinned `README.md` contains the following exact license section:

> ## License
>
> MIT
>
> Some of the data was extracted manually or automatically from wiki.vg and minecraft.gamepedia.com.
> If required by one of the sources the license might change to something more appropriate.

The same README identifies the data sources used by the project, including `minecraft.wiki`, `wiki.vg`, Minecraft server JAR extraction, Minecraft data generators, and other extractors. The README does **not** provide a per-file copyright grant or prove that every generated JSON file is unconditionally MIT-licensed. Accordingly, this repository does not make that stronger claim.

## Redistribution treatment

- The five 26.1 raw JSON files (`version.json`, `protocol.json`, `items.json`, `blocks.json`, and `recipes.json`) are cache-only and are not covered by Pumpkin's GPLv3 or by a new unconditional MIT assertion.
- `manifest.json` records the upstream README's repository-level MIT statement as provenance, while explicitly recording that per-file data provenance/license is not asserted.
- `assets/NOTICE.md` remains the applicable notice for repository Mojang-derived assets. This cache notice does not grant rights to Mojang/Microsoft, wiki.vg, minecraft.gamepedia.com, minecraft.wiki, or any other upstream data source.
- Do not commit the ignored cache or copy these raw files into `assets/`, generated source, or another distributed path without a separate source-by-source rights review.

The committed `protocol-summary.json` and version contracts are local summaries/contracts, not copies of the upstream raw JSON. They retain source URL, commit, and hash references for auditability.
