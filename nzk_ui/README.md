# css-shell — CSS-driven chrome renderer (task-11)

A renderer for the CS Viewer's **top bar and sidebar shell** that parses the
**real** `frontend/index.html` and `frontend/static/style.css` in Rust and
rasterises them. No browser engine, no webview, no JavaScript.

It is a separate cargo workspace inside the fork (`native-csviewer/css-shell/`)
with its own `Cargo.lock`, `target/` and `vendor/`, so it shares no lock or build
directory with the `app/` crate that other owners are editing.

## 1. Approach, and why

The task's architectural question was: *index.html is only the static shell; the
live DOM is built and mutated by 32 JS files, so what drives the CSS engine?*

**Decision: Blitz (option (a)).** Real HTML parsing (`html5ever` via
`blitz-html`), the real Stylo 0.8 cascade, real Taffy 0.9 layout, real Parley 0.6
shaping, painted through `blitz-paint` into `anyrender_vello_cpu` — a **CPU**
Vello rasteriser, so it is fully headless (no wgpu, no display, no GPU).

The decisive reason is that a hand-ported layout cannot be pixel-faithful to a
CSS layout: sharing the actual stylesheet is the only credible route. `(c)`
parse-CSS-into-Taffy would have re-implemented the cascade and lost it; `(b)`
Stylo+Taffy+hand-written paint would have been the same work as Blitz with more
surface to get wrong.

**The DOM IS constructed in Rust.** `src/dom.rs` reproduces every runtime
mutation that touches the chrome, and each builder cites the script and line
range it mirrors, so the equivalence can be re-checked against the source:

| construct | source |
|---|---|
| `data-ui-theme="nzk-dark+"` on `<html>`, `--side-w: 360px` | `core.js` DEFAULT_UI_THEME, `panels.js` applySideWidth |
| folder input value/title, status line, canvas notice | `shell.js` applyState/updateStatusLine/updateCanvasNotice |
| `.pane-grip` "Sidebar" header + `.pane-float-btn` + `.pane-resize` | `panels.js` installPane/addChrome (528-583) |
| Projects label + "no .sln/.csproj found" | `filters.js` buildProjectFilter (72-126) |
| Namespace `<select>`, `N kinds` | `filters.js` buildNamespaceFilter (253-275), buildFilters (382) |
| kind facets (swatch colour from `KIND_COLOR_NZK_DARKPLUS`) | `filters.js` buildFilters (325-357), `core.js` (91-107) |
| edge chips (colour/width/dash from `EDGE_STYLE`) | `filters.js` buildFilters (359-381), `core.js` (55-62) |
| Parser-warnings section (always last, after `#chainSection`) | `problems.js` build (100-163) |
| `.cs-problems-note` styling | `problems.js` injectStyle (40-59) |

Two lessons worth keeping:

* **`set_node_text` is text-node-only in Blitz** — it returns silently for an
  element. `element.textContent = x` has to be written as "clear children, append
  a text node". Getting this wrong made every label a no-op.
* **HTML attributes live in the empty namespace**, not `ns!(html)`. With the
  wrong `QualName`, `set_attribute` does not match the parsed attribute and
  *appends a second one* — `#notice` ended up as
  `class="notice hidden" class="notice"`, so the stale `.hidden` won.

### Fonts and the engine-gap shim

`src/compat.css` is a small, separately-marked `<style>` whose every rule must be
a **no-op under Chromium**. It supplies only what the Blitz UA layer lacks:

* `select { display: inline-block }` — Blitz's default.css has **no** rule for
  `<select>`, so `width`/`height` from style.css were ignored (live `#nsSelect`
  is 339x24; Blitz gave 0x0).
* `#rootInput { min-width: 0 }` — Blitz clamped the flex-shrunk input at 320px;
  Chromium already shrinks it to 276.66px.
* `#notice { width: 340px }` — Blitz sizes an absolute box with no width at 0x0;
  340px is Chromium's shrink-to-fit result here.
* `--mono` / `--sans` **face resolution pin**, to `"Liberation Mono"` and
  `"Noto Sans"`. Chromium+fontconfig resolve the stylesheet's stacks to exactly
  those faces (measured by task-13 via `CSS.getPlatformFontsForNode`,
  `evidence/text-parity/faces.md`); fontique picks different ones, whose normal
  line metrics re-wrapped the Parser-warnings note from 2 lines to 3. Only the
  family is pinned — size, weight, letter-spacing, line-height and colour all
  still come from the real stylesheet.

`src/problems-injected.css` is the CSS `problems.js` appends at runtime,
reproduced verbatim.

## 2. Library API (the integration surface)

The CLI is a thin wrapper; integrators should use the library:

```rust
use css_shell::Shell;

let mut shell = Shell::load_default((1600, 1000), "/tmp/fid-corpus")?; // parse+style+layout ONCE
let side = shell.box_of(".side").unwrap();          // resolved border box
let all  = shell.boxes();                            // the fixed probe list
let hit  = shell.hit(1300.0, 300.0);                 // deepest element at a point
let mut rgba = Vec::new();
shell.paint_into(&mut rgba);                         // repaint on demand
shell.resize(1920, 1080);                            // re-layout, no reparse
```

The intended split (agreed with the Lead) is **css-shell paints the chrome;
native widgets own behaviour and are positioned from `Shell::boxes()`**. A warm
full-frame repaint is ~8-9 ms, so the library path is interactive; a child
process per frame is not needed.

## 3. Build, run, measure

```bash
cd "/2tb/@/nzk/git/vrcCS/cs viewer builder" && . ./rust-env.sh
export CARGO_HOME="$PWD/native-csviewer/.cargo-home"      # fork-local
export CARGO_TARGET_DIR="$PWD/native-csviewer/css-shell/target"
# run from /tmp so the parent native-csviewer/.cargo/config.toml is not picked up
cd /tmp && cargo build --release --manifest-path "$OLDPWD/native-csviewer/css-shell/Cargo.toml"
# offline, from inside the crate (css-shell/.cargo/config.toml points at its own vendor/):
cd "$OLDPWD/native-csviewer/css-shell" && cargo build --offline --release
```

* `--out PATH` render a PNG; `--boxes PATH` write resolved geometry as TSV
  (`--boxes -` prints to stdout); `--hit X,Y` hit-test; `--repeat N` repaint N
  times in one process and report each stage.
* `--facts PATH` render a host's real corpus facts instead of the built-in
  fixture. The file is exactly a serialized `dom::Facts`, so a host can emit it
  with `serde_json::to_string(&facts)` and no adapter:

```json
{
  "repo": "/tmp/fid-dense",
  "status_override": null,
  "kinds": [["method", 60], ["class", 20], ["namespace", 10]],
  "ns_count": 10,
  "indexed": true,
  "file_count": 18,
  "parse_ms": 63,
  "node_count": 90,
  "edge_count": 84,
  "projects_empty_text": "no .sln/.csproj found",
  "parser_warning_count": 0,
  "nodes_in_view": 18
}
```

Three further fields are optional and additive, so older documents still parse
(they carry `#[serde(default)]`; the original fields stay required, so a
malformed document still fails loudly):

* `projects: [{id, name, file_count, path, targets, defines}]` — replaces the
  "no .sln/.csproj found" chip with the real project chip row (`filters.js`
  buildProjectFilter).
* `warnings: [{file, line, col, text, raw}]` — replaces "No parser warnings"
  with real rows and sets the section count from the row count
  (`problems.js` rowEl).
* `ui: {collapsed: [sectionId], active_mode: "graph"|"find"}` — **host-owned
  view state**, not corpus data. `null`/absent leaves the document exactly as
  `index.html` ships it (verified byte-identical to the fixture render), so the
  state path is pixel-neutral at rest.

`status_override: null` builds the status line from the four numeric fields;
a non-null value is used verbatim and **only** for the status line — the numeric
fields still drive everything else. That is how a host pins the unreproducible
wall-clock `parse Nms` of a captured reference (used by native-source's dense
measurement harness; verified by setting `parse_ms: 1` and
`status_override: "… parse 999ms …"` and rendering "999ms"). `repo` drives the
folder input; `kinds`/`ns_count` drive the facets and the namespace select. **When `--facts` is absent the built-in fixture is used
unchanged**, so existing A/B runs are byte-for-byte identical. `--help`
advertises the flag, so a host can feature-probe it.

Library equivalents: `Shell::from_source_with_facts(html, css_path, css, size,
&facts)` and `Shell::load_with_status(size, repo, status)`; `Facts` implements
`Serialize`/`Deserialize`.
* `tools/ab-css.sh` — the A/B harness (same corpus, size and diff math as
  `native-csviewer/tools/ab.sh`, but the fork side is css-shell).
* `tools/domdump.mjs` — read-only CDP dump of the **live** original: the sidebar
  DOM skeleton plus the box/colour/font of every element that matters. This is
  what settled contested questions instead of reading the scripts.
* `tools/region-metrics.py` — whole-frame and per-region fidelity with both
  images composited over black, so an RGBA/RGB mismatch cannot inflate the
  number.

**Always measure a release build.** The debug build's `vello_cpu` raster is
orders of magnitude slower (a 341 MB debug binary): 4.7 s to paint versus 8 ms.

## 4. Evidence

Reference, fork, diff strip and `metrics.json` live in
`native-csviewer/evidence/css-ab/`. Headline, 1600x1000, `/tmp/fid-corpus`,
against the same Chromium reference the egui fork is measured on:

| metric | egui fork (shipped) | css-shell (final, pinned reference) |
|---|---|---|
| SSIM | 0.793792 | **0.934513** |
| MAE (harness, RGB) | 18.8313 | **1.7271** |
| RMSE | 35.5131 | **11.7670** |
| pixels > 16 | 29.2014 % | **2.4209 %** |
| identical pixels | 0.0067 % | **91.8978 %** |

The reference is regenerated by the shared harness on demand and the original
re-measures `parse Nms` every run, so a stale copy silently compares two
different status strings. `tools/ab-css.sh` therefore captures a fresh reference
and passes the exact status text it saw to css-shell, and writes
`evidence/css-ab/inputs.json` with the sha256 of both frames plus that text
(final run: reference `18f6eb8a8f85203d`, fork `de153320e170441c`,
`files 2 · parse 50ms · nodes 9 · edges 10`). The reference sha changes per
capture; run-to-run reference variance is about ±0.02 MAE.

Per region (composited over black, `tools/region-metrics.py`; the egui column is
the Lead's independently measured decomposition):

| region | egui MAE | css-shell MAE |
|---|---|---|
| whole frame | 5.30 | **1.73** |
| top bar (0-60) | 17.07 | **4.48** |
| second row (60-110) | 12.06 | **3.36** |
| body left / canvas | 1.56 | 0.25 |
| body right / sidebar | 12.95 | **5.39** |

**Do not read the canvas number as a win: css-shell draws no graph.** It paints a
correctly-coloured empty canvas while the reference holds ~12,884 non-background
pixels of graph. css-shell's chrome-only surfaces are the top bar, the second
row and the sidebar.

Timing (release, this box; other teammates were building concurrently, so
process-level wall times are noisy):

| stage | cost |
|---|---|
| parse + style + layout (in library) | 28-33 ms |
| paint #1, first paint in a fresh process | ~10 ms quiet, up to ~376 ms under heavy load |
| paint #2..N, warm repaint in the same process | **8-9 ms** (3-4 ms commands + 3-5 ms raster) |
| end-to-end single-shot process (parse+layout+paint+PNG write) | 47 ms quiet, 79-163 ms loaded |

### Text vs non-text (my own decomposition)

`tools/textshare.py` over the same frames splits the error mass exactly:

| mask | pixels | MAE | share of whole error mass |
|---|---|---|---|
| reference text ink | 39,081 (2.44 %) | 29.85 | **42.22 %** |
| non-text ink | 1,560,919 | 1.02 | **57.78 %** |

So for css-shell roughly **42 % of the remaining error is text ink** — more than
double the ~19 % task-13 measured on the egui fork. That is the expected
consequence of fixing the geometry: once the boxes are in the right place, glyph
rasterisation is a larger fraction of what is left. It also means the "spend on
the non-text 80 %" advice applies with a smaller margin here.

An independent check that the residual really is rasterisation and not
misalignment: shifting the whole fork frame by every (dx, dy) in {-1, 0, 1}
never improves any region — (0, 0) is optimal for the top bar and the sidebar.

### `text-transform` (kept, despite a small metric cost)

Blitz tracks `text_transform` for invalidation (`blitz-dom/src/layout/damage.rs`)
but never reads it in `stylo_to_parley.rs`, so `text-transform: uppercase` on
`.section-head` and `.pane-grip` was dropped and the sidebar rendered
"Filters"/"Sidebar" where Chromium renders "FILTERS"/"SIDEBAR".
`dom::apply_text_transform` supplies that one property, reading the rule list and
selectors out of the **real** stylesheet rather than a hard-coded label list, and
runs before layout because uppercase glyphs are wider.

Measured against one pinned reference:

* without it: MAE 1.7119, ink coverage 111 px for "FILTERS" (reference 146)
* with it: MAE 1.7271, ink coverage 148 px for "FILTERS"

The metric prefers the *incorrect* lowercase rendering by 0.015 MAE because
Parley's `line-height: normal` for Liberation Mono at 10px is 1px shorter than
Chromium's, so the (now correct) uppercase sits 1px high and overlaps the
reference's glyphs while misaligned. Forcing `.section-head { line-height: 13px }`
(Chromium's own computed value — a no-op there) raised identical-pixel share to
92.0 % but pushed MAE to 1.7540, so it was reverted. The uppercase is kept: it is
what the stylesheet says and what the original shows, and 1 % of MAE is not worth
shipping visibly wrong text.

## 5. Residual differences that are NOT closed

1. **Glyph rasterisation.** Chromium rasterises with Skia; this paints with
   Vello-CPU from Parley/`harfrust`/`swash` outlines. Two different antialiasing
   and hinting engines produce per-pixel differences on every glyph even when the
   advance widths agree. The top bar is 66.5 % identical pixels and 5.25 % above
   a delta of 16 for this reason; task-13 measures text at ~19 % of the error
   mass, so this is the smaller half of the remaining error.
2. **`line-height: normal`.** Where the stylesheet leaves line-height at `normal`,
   Blitz resolves ~1.3px taller per line than Chromium (e.g. `.facet` rows:
   live 18px, here 20px before the font pin, ~19px after; `#kindFilters` 54 vs
   58). With the faces pinned the *wrap points* now match, so this is a residual
   metric difference, not a re-layout.
3. **`.scount` right alignment**: `#filtersCount` / `#problemsCount` sit 14-15px
   left of where Chromium puts them (right edge 1576 vs 1590). All other
   right-aligned sidebar children (`.facet-count`, `.range-val`) match exactly.
4. **Form-control painting.** Chromium draws `<input type=range>` via its
   `::-webkit-slider-*` pseudo-elements; Blitz has no such pseudo-element, so the
   four Layout sliders paint as plain boxes. Checkboxes likewise do not match the
   native dark checked style (`.check input { accent-color }`).
5. **No graph, no interactive behaviour.** The canvas is empty by design; hover,
   focus rings, the suggestion dropdown, the toasts stack and every modal are not
   exercised. Only the first-paint resting state is reproduced.
6. **Data is input, not measured.** `Facts::for_corpus` hard-codes the kind and
   namespace counts, and the status line unless `--status` is given; the A/B
   harness passes the exact status string the reference session reported. These
   are `/api/*` data the runtime would have fetched. An integration must feed
   real values.
7. **`::placeholder` is not rendered.** Blitz does not implement it, so the top
   bar's "Search symbols…  ( / )" and the Parser-warnings "filter warnings…"
   placeholders are blank. Faking them by writing the text into `value` would put
   the right pixels there in the wrong colour, and was deliberately not done.
8. **Range sliders.** Emulating them was scoped and rejected: the four Layout
   sliders are only **4.6 %** of the total error mass (measured: each widget band
   is 1.15 %), because most of the band's error is the surrounding label text.
   This is the largest single identified non-text gap that remains.

## 6. D4 — pane-aware, scrollable chrome (task-30, PARTIAL)

Landed and green, then **stopped** for the beta code freeze:

* **Real pane grid.** `ui.pane_open` adds `.with-source` to `.layout` and
  unhides `#sourcePane`, so the stylesheet's
  `grid-template-rows: minmax(0,1fr) minmax(120px, var(--source-h))` resolves
  from the real CSS instead of a hand-cut frame. At 1600x1000 the DOM reports
  `#sourcePane` = (0, 660, 1600x340) — exactly 34vh, spanning both columns —
  `#graph`/`.side` = (0, 52, 1240x608)/(1240, 52, 360x608), and the canvas row
  keeps the height the old 66vh frame produced.
* **Sidebar scroll.** `.side { overflow-y: auto }` is a real scroll container.
  `ui.scroll` names containers and offsets; `dom::apply_scroll` applies the
  offset to the engine's own `scroll_offset`, which `blitz-paint` subtracts and
  clips to, and `--boxes` reports `@scroll <selector> <x> <y> <maxX> <maxY>`.
  Scrolling the pane-open sidebar by 240 moves 113,785 px, confined to
  x 1241-1599 / y 52-659 — inside the sidebar box, never over the pane.
* **Region hit-test.** `chrome::region_at(x, y) -> SourcePane | Sidebar | Canvas
  | Chrome`, and `hit_in`/`hit_selector` for controls.
* Evidence: `evidence/task-30/` (`geometry.txt`, `scroll.txt`,
  `scroll-side-by-side.png`, `regions.txt`, `cost.txt`), regenerable with
  `tools/task30-evidence.sh`.

Not done, and not started under the freeze: native pane content composited from
the DOM, panes as first-class windows, and any per-region/dirty repaint.

## 7. Integration contract (cs viewer v3)

### Versioning

css-shell is **deliberately its own version** (`0.1.0`) and does not track the
product's `3.0.0`. It is a separate cargo workspace, so the parent's
`[workspace.package] version` cannot reach it in any case; forcing it to `3.0.0`
would only create a second, silently-diverging place to edit the product number.
The product version belongs to the app, the incumbent Tauri viewer keeps `2.0.0`,
and css-shell is an internal child-process rasteriser plus library — versioned by
the parent, not by the product.

What *does* need a version is the interface between the two, because it is
cross-process and has two halves that can drift independently:

* the **input** contract — the `--facts` JSON shape (`dom::Facts`);
* the **output** contract — the 68 selectors in `--boxes` plus `--hit`.

Both are currently guarded by feature-probing rather than a number: the app runs
`css-shell --help` once and only passes `--facts` if the flag is advertised, and
falls back to `--status` otherwise. That is the safe pattern and it is what makes
the current split work. If the interface grows past what `--help` can express,
the honest fix is one machine-readable probe (`--contract` printing a schema
revision, the facts field list and the selector list) so a mismatch is a hard
error instead of a silent wrong render. **Not built yet** — there is one
consumer today and it feature-probes correctly; building it now would be
speculative.

### Landed in the host (task-23)

`app/src/chrome.rs` now holds a selector-keyed registry and the chrome's own
presentation state:

* `chrome::CONTROLS: &[Control]` — every control the chrome paints, with
  `selector`, `label` and an owner. **Keyed by the DOM selector, never a node
  id.** The owner is the single source of truth for what is wired and what is
  not, and the unwired entries are listed rather than faked.
* `chrome::hit_in(&boxes, x, y)` / `hit_selector(x, y)` — smallest containing
  box wins, so a nested control beats its container.
* the collapsed list comes from the host's `actions::ChromeState`
  (`collapsed_facts()`, verbatim) — chrome.rs holds no second copy. It is
  emitted as the facts document's `ui.collapsed`, and a change rebuilds the
  **whole** chrome.
* **Cost, stated correctly.** Three different numbers, and only the last is the
  user-visible budget:
  1. in-process warm repaint (rasteriser only): **8-9 ms**
     (blitz-paint 3-4 ms + vello_cpu multithreaded raster 3-5 ms);
  2. one `--boxes` child invocation: **34-35 ms** release;
  3. one `--out` (paint) child invocation: **46-48 ms** release.
  A chrome state change runs **both** (`chrome.rs::build` spawns paint and
  `--boxes`), so the rebuild budget is **~80-90 ms** unloaded, and the app's own
  `build_ms` has been measured at 198-295 ms under load. Quoting 8-9 ms for a
  rebuild is wrong. Closing the gap needs the shell in-process (linking this
  crate pulls Blitz/Stylo into the app) or a persistent child — architecture
  decisions, not CSS work, and neither is done.
* `chrome_ui(ui, view) -> Option<ChromeAction>` — a click on a host control
  returns `actions::ChromeAction` for `app.rs` to execute through
  `actions::apply`; `Effect::reindex` stays with `App`.

There is exactly one action enum: `app/src/actions.rs::ChromeAction` (task-26,
native-source). chrome.rs names it and calls `from_selector`; it does not define
a second one.

### Hit-test / event model (for D2, "the shell owns interaction")

Already present and verified: `Shell::hit(x, y) -> Hit { node_id, tag, id,
class }`, also reachable as `css-shell --hit X,Y`. It returns the deepest element,
so it is a ready dispatch key:

```
$ css-shell --hit 480,25   # → hit 112 button #indexBtn .btn btn-primary
$ css-shell --hit 1270,340 # → hit 1061 span # .edgekey
$ css-shell --hit 300,90   # → hit 163 canvas #graph
```

The proposed model, in the order it should be built:

1. **Named controls, not node ids.** Node ids are not stable across a reparse or
   a `resize`; selectors and ids are. Map the hit result onto a small, explicit
   control table (selector → action) in the host, mirroring `panels.js` /
   `filters.js`. `#id` first, then the nearest ancestor with a class the table
   knows; unknown hits are no-ops, never guesses.
2. **Hit-test on the same layout that painted.** `--boxes` and `--hit` already
   share the resolved document, so a click cannot disagree with the frame the
   user saw — but only if the host re-reads geometry after `Shell::resize`
   (a resize re-runs layout and can move every box).
3. **State lives in the host; the DOM is rebuilt from it.** When a control
   changes state (a facet toggles, the namespace select changes), the host must
   mutate the facts/DOM and repaint — not paint an overlay on top of a stale
   frame. That is what makes D3 a prerequisite for D2 rather than a parallel
   task: with the counts still constant in `chrome.rs::facts_json`, a wired
   checkbox cannot show an effect that matches the graph.
4. **Dirty-region repaint is optional at 8-9 ms.** A full 1600x1000 warm repaint
   is 8-9 ms, so the simplest correct implementation is "any state change
   repaints the whole chrome". Worth revisiting only if chrome and graph must
   paint in the same frame budget.

### What the facts JSON must grow for D3 (live state)

The current `dom::Facts` covers counts and the empty states. Two constants in
`chrome.rs::facts_json` are not yet data:

* **Projects rows** — `projects_empty_text` is the zero-project case; a
  non-empty list needs `projects: [{id, name, fileCount, path, targets,
  defines}]` to drive `filters.js`'s chip row.
* **Parser-warning rows** — `parser_warning_count` drives the section-head
  number, but the body is still the empty-state "No parser warnings"; real rows
  need `warnings: [{file, line, col, text}]` to drive `problems.js`'s row
  renderer.

Both are additive fields in the same JSON document, and neither is built until a
real corpus can produce them (same rule as `--facts` itself: no speculative
interface). The filter controls that only change *view* state (facets, edge
kinds, Hide isolated, Depth) need no new facts at all — they are host state plus
a repaint.
