# Extension-side review — final adjudication

**Date:** 2026-07-19 · **Scope:** `extension/src` (103 files) + `package.json` contributions
**Status:** CLOSED. Four verification rounds; no fifth.

**Method:** 4 finders (68 raw) → dedup → filtered against `known-issues.md` + SRS → skeptics under
default-reject → max-effort skeptics under default-reject (8 proven / 8 narrowed / 9 rejected) →
**terminal round with the burden FLIPPED to default-suspect** over the rejected and narrowed piles,
plus fix-feasibility with class checks.

Rounds 2 and 3 both ran default-reject, which is right for deciding where to spend effort but is
systematically biased toward false negatives. Round 4 audited that bias. It overturned one rejection
on measured evidence, promoted one finding to the top of the list, and **reversed one of this
review's own recommendations.**

---

## Reversals and corrections this round produced

1. **The MB tier is now recommended AGAINST.** Uniform kB is defensible: the report's brotli and
   minified columns are *sorted by the number being formatted*, so an MB tier makes a ranked column
   mix units. Splitting the formatter to avoid that would violate one-mechanism-per-concern. Record
   as a limit instead.
2. **The `copyImportDiagnostics` fix was wrong.** Do not add a `when` clause. **Delete the command
   contribution.** All three real callers pass arguments (`tooltipMarkdown.ts:46`,
   `packageJsonTooltip.ts:181`, `treeShakeActions.ts:54-58`) and a registered command stays invokable
   without a palette entry.
3. **Manage Cache was under-ranked.** It is now the highest-risk item here — see FIX-1.
4. **The AC-008 refutation was a category error.** No spec text governs budget-diagnostic code
   actions; AC-008 forbids *silent rewriting*, not a user-selected action. Struck.
5. **The markdown-escaping scope was too narrow** — the class is 4 sites, not 1.

Plus the five factual errors round 3 caught in the first draft (drift check, `budgets` schema,
diagnostic `source`, formatter bases, mangling mechanism), all corrected below.

---

## FIX NOW — 5 items

### FIX-1. Manage Cache: a confirmation that understates its scope, and a toast that reports zero
**The only item here that touches consent and a wrong count.**

`cacheManager.ts:199` describes less than `purge_orphans` actually does — surviving projects' caches
are scrubbed of stale entries and stale registry metadata is pruned, none of which the confirmation
states. Then `cacheManagerItems.ts:160-167` reports "No orphaned Import Lens caches to reclaim"
*after data was removed*, because the purge reports only project-level counts.

**Fix:** correct the confirmation text to state entry-level and registry-level scrubbing; make
`purge_orphans` report entry- and registry-level counts so the completion toast stops saying zero.
Byte figures in the other confirmations are secondary polish.

### FIX-2. History chart draws one polyline across unrelated files
`extension.ts:228-231` → `currentFileSize.ts:113,129` passes unfiltered global history;
`bundleImpactHistoryView.ts:186` emits one `<polyline>` under `aria-label="File Cost trend"`.
Executed: `app.ts`→`util.ts`→`app.ts` renders a ~98% drop-and-rebound that happened to no file.

**Fix (adjudicated):** one `<polyline>` per distinct `fileName`, grouped inside `historyChartSvg`.
- **Keep the global `maxBrotli` scale** (`:21`) — a shared y-scale is what makes two series
  comparable. Filtering to the active file was considered and rejected: the table shows up to 20 rows
  across N files, and the command has no active-editor contract.
- **Switch x from array index to `timestamp`** — with a **mandatory span guard**: naive
  `(t-min)/(max-min)` yields `NaN` on equal timestamps and the polyline silently fails to render. The
  existing `history.length === 1` guard does not cover length>1-with-equal-timestamps, and the new
  tests will produce ties immediately.
- **Zero existing assertions break — which is the hazard.** The current suite never looks at
  `<polyline>`, so it would have passed the buggy renderer. Add a test asserting polyline count
  equals distinct-file count, and that no polyline mixes two files. Seen red first.
- **SRS:** no amendment required. Recommended one line in FR-036f: "a trend line joins only rows for
  the same file." **Do not** amend FR-036f to *describe* current behavior — that is narrowing the
  spec to bless the gap.

### FIX-3. Compare Imports silently drops across three channels
Daemon pre-filter (`service.rs:1013-1018`), resultless items (`compareImports.ts:71`, which also
discards the daemon's `message`), and unmeasured pairs (`compareImportItems.ts:27-34`). Executed:
five specifiers in, **two rendered**, three vanish with no trace — in a QuickPick whose whole purpose
is "pick the cheapest."

**Fix (adjudicated): extension-only, protocol untouched, no daemon change.** Change
`compareImportItemsForResults` to take `(requested: readonly string[], items: readonly ImportAnalysisItem[] | null)`
and compute all three channels in that one pure function (channel A = `requested` minus the
specifiers present on the wire).
- **TRAP:** `CompareImportItemsResult.warning` currently means *show nothing* —
  `compareImports.ts:74-77` returns early on it. Routing disclosure through `warning` deletes the
  entire comparison. Use a distinct field.
- **Surface:** a `QuickPickItemKind.Separator` labelled with the count, one plain item per lost
  specifier with its reason in `detail`, plus a `title` suffix so the count is visible before
  scrolling. Map `separator` → the vscode enum at the edge in `compareImports.ts`; do **not** import
  vscode into `compareImportItems.ts` (breaks the unit test).
- A follow-up `showWarningMessage` was considered and rejected: it fires after the QuickPick is
  dismissed, so the disclosure arrives after the decision it exists to inform.
- **Tests:** `deepStrictEqual` means an always-present disclosure key breaks all three existing
  tests. Updated tests stay **Logic** (expected values derived from the fixture). Add the channel-A
  case (`['react','./local']` → `./local` disclosed) — nothing covers it today.
- **Class check: clean.** No other surface has this filter-without-disclosure pattern. Fix in place;
  building a shared "dropped items" abstraction for a population of one is the exact
  adding-beside-instead-of-replacing growth the repo guards against.

### FIX-4. Report tables overflow horizontally
**Overturned rejection — settled by headless layout measurement**, not argument. At an 810px Beside
panel: `client=769, scroll=1572` — **803px of overflow.** Even a single `react` row with no reasons
or warnings gives `client=769, scroll=1014`. Even a 1600px full-width window overflows. The reader
must scroll horizontally to see Confidence, Confidence Reasons, Top Modules and Warning.

**Fix:** wrap each table in an `overflow-x:auto` container, or add `word-break` to the path/reasons
cells. One line.

### FIX-5. Treemap label contrast
`reportContent.ts:103` — one line, no markup change, no color-token change (so FR-031f's `charts.*`
mapping is untouched, which is what it requires):

```css
.treemap text{fill:var(--vscode-editor-foreground);font-size:12px;
  paint-order:stroke;stroke:var(--vscode-editorWidget-background);
  stroke-width:3px;stroke-linejoin:round}
```

Breaks nothing — `reportContent.test.ts:15` passes `treemap: []`, so `svgTreemap` has no coverage
today. That also means it ships unguarded.

---

## FIX — spec reconciliation (2 items)

### SPEC-1. Status bar diverges from FR-033 in five places
**Overturned rejection.** FR-033 mandates `Import Lens: Ready` / `Import Lens: Computing...` /
`Import Lens: Unavailable`, repeated at SRS:655, :834, :835, :836 and NFR-004b :858. The code renders
`IL: …` (`statusbarText.ts:50-56`) and the tooltips are `Import Lens: Computing current file size`
and `Import Lens: Daemon unavailable` — neither matches.

Three of the four supports the prior rejection rested on were false: the tooltip does **not** carry
FR-033's text; `item.name` is **not** `accessibilityInformation` (`@types/vscode:7209` vs `:7259`);
and known-issue G4 records compression staleness, not the `IL:` prefix — it quotes `IL: 1.2 kB
brotli`, a string the code cannot produce (`labelForCompression` maps brotli→`br`). The existing test
is real but inert: it pins the function's own output, proving the text is *intentional*, not correct.

**Fix:** reconcile in one task — either render the mandated strings (or carry them in the tooltip),
or amend all five SRS locations to bless the short form. Do not leave them disagreeing. Set
`accessibilityInformation` while there: FR-039b designates this item as *the* screen-reader surface.
This also absorbs the "daemon death is invisible" residue — death **is** visible
(`extension.ts:364`); only the wording diverges.

### SPEC-2. FR-039a mandates an API VS Code does not have
`extension.ts:316` passes markdown source to `showInformationMessage` (every overload takes
`message: string`). FR-039a (SRS:792) requires "a hover-style `MarkdownString` notification" — **no
such API exists.** This is spec *repair*, not spec-narrowing. Amend FR-039a to name a real surface
(webview, or plain text + action buttons) **and** change line 316 in the same task; the spec edit
alone leaves the toast rendering literal `**`.

---

## CHEAP CLEANUPS (3)

- **Delete** the `importLens.copyImportDiagnostics` entry from `contributes.commands`. Not a `when`
  clause — deletion. Breaks nothing.
- **Escape at the shared boundary,** not per-site: one helper over all four daemon/user-controlled
  interpolations (`tooltipMarkdown.ts:160`; `packageJsonTooltip.ts:79,107,145`). Backslash escaping
  currently *deletes* path separators — `\.pnpm` renders as `.pnpm`, a wrong path in the surface a
  user reads to find a path.
- **Rewrite `importLens.compression`'s description** — it is actively wrong about the default
  configuration, telling the user the setting does nothing when it governs every size figure on
  screen. Add `enumDescriptions` to all four enums (`display`, `inlineRenderer`, `compression`,
  `logLevel`), `all` most of all.

---

## DO NOT FIX — record in `docs/known-issues.md` (6)

| Item | Why not |
|---|---|
| **`formatBytes` MB tier** | *Reversed this round.* Uniform kB keeps sorted report columns from mixing units. If ever overridden: threshold 1,000,000 and **2** decimals (1 decimal renders 1,000,000 and 1,049,000 both as "1.0 MB" in a column sorted by that number). The `cli/importlens.mjs:844-850` copy is unpinned — add the Drift check or delete the duplicate. |
| **Slot cap folds "over budget"** | Text is never lost, only tone; Problems panel unaffected; unreachable on stock settings (`budgets` defaults to `{}`). Fix costs 3 extra process-wide decoration types across 4 files. Max segments is provably 8, so a cap of 7 would close it. |
| **Compare Imports progress** | Hard-bounded at 10 s by `ipc/client.ts:209-214`; cannot hang; ends in an explanatory warning. Worth noting 10 of 11 daemon-calling commands *do* wrap in `withProgress`. |
| **Two timestamp formats** | Both values correct. Preferred future shape: lift `formatLastUsedAge` into a shared module, and relabel "Latest published" — its source is the packument `modified` field, not a publish time. |
| **Raw `component` runtime** | *Overturned rejection, downgraded.* The prior dismissal conflated "not a spec violation" with "not a defect." Runtime is the only wire enum in the UI with no label map. Small fix beside `assetKindLabel`; not required by the bar. |
| **Triple `getText()`** | *Overturned rejection, downgraded.* "Microtask-separated" is false for two of three gaps: the loose-file `access()` walk is real macrotask I/O, and read #3 (`listener.ts:367`) is separated from #2 by a full daemon round trip. Can produce a git-delta caption computed from text the document no longer has. Self-healing next cycle. |

## DISMISSED — no entry needed (8)

Views/menus (FR-041 *forbids* gating Show Logs; SRS:1807 covers only a cache view) · failed-vs-loading
color (FR-004a mandates neutral; primary text differs) · the six verbatim-mandated state strings ·
`✦` glyph (U+2726 confirmed; a Guard test at `packageJsonLabels.test.ts:165` already holds it) ·
hex fallbacks (consumer set closed at eight call sites, one webview) · localization (no requirement;
the SRS pins English literals in five places) · dead Copy-diagnostics link (state unrepresentable) ·
`"Selected Brotli"` under `compression:"all"` (no wrong label on any surface).

## Adjacent defects found while verifying (not part of the ask)

- `importHintParts.ts:71` asks "is there an error?" where the codebase standardised on
  `measuredSizes()` — a second way to ask one question.
- All nine commands carry both `category: "Import Lens"` and a title already prefixed
  `"Import Lens: "`, so the palette doubles the prefix.
- `bundleImpactHistoryLabel` (`analysis/history.ts:140-146`) has **no production caller** — grep
  finds only tests. Speculative surface; deletion candidate.
- `confidenceVisuals.fallbackColor` (`:22,31,40,49`) is unreachable. Deletion candidate.
- `treeShakeActions.ts:64-67` declares a QuickFix no diagnostic can reach.
- `is_durable()` (`protocol.rs:453`) returns true for the shape the dead-link dismissal depends on
  being impossible — that invariant rests on constructor discipline, not a checked gate.
