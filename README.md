# CS Viewer v2 — Rust-only engine, VSCode editor, document windows

This is the **second, Rust-only editor version**. It is a clone of the current
viewer (`tools/csgraph`), not a rewrite of its behaviour: the same graph, the
same API, the same documents, the same answers — with the engine in Rust, the
code pane replaced by the real VSCode editor, and every document able to live
in its own window and its own thread.

`tools/csgraph` is **frozen**. Nothing here edits it. It stays the reference
implementation and the differential oracle.

```
cs viewer builder/
├── build-appimage.sh              # the only supported builder (full Rust engine)
├── deps/runtime-x86_64            # tracked AppImage type-2 runtime (+ SHA256SUMS)
├── .old/builder/                  # archived: old Python / partial-Rust builders + docs
├── .old/csb_code/rust-csviewer/   # archived: partial-Rust crate (its vendor/ is still used)
└── rust-only-csviewer/            # <-- this tree
```

## The build

| build script | engine | UI host | Python that ships |
|---|---|---|---|
| `build-appimage.sh` | `csgraph-core` (Rust) | Tauri webview | **only the CS splitter** (`convert.py`) |

The two superseded builders are archived under `.old/builder/`:
`old.build-appimage.sh` (Python web app) and `old.build-rust-appimage.sh`
(partial Rust with the Python bridge).

## Layout

```
rust-only-csviewer/
├── Cargo.toml                  workspace: `core` + `app`
├── .cargo/config.toml          vendored crates, from ../.old/csb_code/rust-csviewer/vendor (offline)
├── core/                       csgraph-core — the whole engine, no GUI dependency
│   ├── src/{model,lex,parser,graph,doc,reorg,index,api,cache,hash}.rs
│   └── src/bin/csgraph.rs      headless CLI (`csgraph dump`) used by the clone check
├── app/                        csviewer — the Tauri host
│   └── src/main.rs             IPC `api`, document windows, detached panes,
│                               window placement, NVIDIA/Wayland quirk
├── frontend/                   index.html + static/*.js
│   ├── static/editor.js        the Monaco-backed source pane (v2)
│   ├── static/windows.js       document + pane windows and the cross-window bus
│   ├── static/panewin.js       a pane detached into its own OS window
│   ├── static/sync.js          the live view state every window replicates
│   └── vendor/monaco/min/vs    the self-contained Monaco AMD build
├── vendor/monaco/              the immutable FULL Monaco source distribution
├── python/splitter/convert.py  the one Python file v2 keeps
└── tools/                      vendor_monaco.py, devproxy.py, headless_check.mjs, oracle.py
```

## Why the editor changed

The v1 pane built one `<div>` plus 5–20 `<span>`s **per source line**. A
synthesized whole-corpus document is tens of thousands of lines, so opening one
created hundreds of thousands of DOM nodes and re-laid them all out on every
scroll, hover and resize. That is the jank.

v2 renders the code pane with **Monaco**, VSCode's own editor. It keeps the
buffer as a model and paints only the lines that intersect the viewport.

Measured on `#ns:NZK` — the whole toolkit; the synthesized document is
**35,715 line records, 8,784,331 bytes** (`/api/doc`), rendered in headless
Chromium 153 (`Chrome/153.0.8010.52`) on this machine by
`tools/headless_check.mjs`, including 60 scripted scroll jumps through the
document.

The **v1 column is historical** — it is the measurement that motivated the
rewrite, taken on an earlier payload, build and browser profile. The v2 column
was **re-measured 2026-10-14** (median of three runs, each through
`tools/with_serve.sh -- node tools/headless_check.mjs --node '#ns:NZK'`):

| | v1 line-DOM (historical) | v2 Monaco (re-measured 2026-10-14) |
|---|---|---|
| open (JS `renderMs`) | 320 ms | 1065 ms (964–1126) |
| DOM nodes in the pane | **357,429** | **35** |
| DOM nodes in the whole page | 674,816 | 376–554 |
| nodes in the pane after scrolling | 357,429 | **35** |
| Chrome `TaskDuration` (open + 60 scrolls) | 359.8 ms | 1130 ms (1006–1175) |
| Chrome `LayoutDuration` | — | 11.8–14.0 ms |
| Chrome `RecalcStyleDuration` | — | 795–921 ms (2001 recalc passes) |
| lines shown | 20,000 (capped) | **35,715 (all of them)** |

> **The older v2 figure of 97.6 ms is not reproducible today** and is kept only
> as the number the rewrite was judged by. The current cost is dominated by
> fetching and `JSON.parse`-ing the 8.8 MB payload and creating the Monaco
> model: a feature-free baseline (the same page with the viewer's feature
> modules disabled) measured the same order of magnitude during the feature
> pass, so this is not attributable to the hover/CodeLens/hierarchy/semantic
> token modules. The two timing columns are *not* like-for-like across eras;
> only the DOM-node counts were taken on comparable payloads.

The DOM node count is the number that matters: it is what every subsequent
layout and paint pass walks. 357,429 → 35, and it stays there while scrolling.

Monaco also brings the real Dark+ C# tokenizer, the find widget, folding, the
minimap, bracket-pair colorization and selection — VSCode behaviour, not an
impression of it.

### Monaco is vendored, not installed

`vendor/monaco/` holds the complete published Monaco source distribution
(`esm/` + `dev/` + `min/` + `monaco.d.ts` + licenses) — 1918 files,
101,674,083 bytes. Nothing in this tree is installed, hoisted, deduped or
rewritten by npm, pnpm, yarn or any other tool, and no build step runs over it.

```bash
python3 tools/vendor_monaco.py --verify
# ok   files        expected=1918 actual=1918
# ok   bytes        expected=101674083 actual=101674083
# ok   tree_sha256  expected=5cf876f8… actual=5cf876f8…
# PASS: monaco-editor 0.57.0 intact (1918 files, 101674083 bytes)
```

The runtime uses only `min/vs` (self-contained; `marked`/`dompurify` are inlined
in the bundle), staged into `frontend/vendor/monaco/min/vs`. `esm/` and `dev/`
stay in the clone and are never shipped.

## Panel layout — resize, undock into a real window, redock

`frontend/static/panels.js` gives the panes Blender/Unity-style area handling:

* **resize** — a vertical splitter between the canvas and the sidebar writes
  `--side-w`; a horizontal splitter above the source pane writes `--source-h`;
  the node drawer and find panel carry a corner grip;
* **undock** — drag a pane header (or press its ⤢ button) and the pane opens in
  its **own operating-system window**, placed beside the main window and free to
  be moved anywhere on the desktop. That is `panewin.js` on the other side;
* **redock** — press the pane window's Close (which redocks instead of merely
  hiding the pane), or the main window's `Bring back` placeholder where the pane
  used to be. Closing the pane's window with the window manager redocks it too
  (`cs:win-closed`).

Sizes persist in `localStorage` (`csgraph.panels.v1`). The graph canvas itself
stays docked: it owns the shared `state` and the `<canvas>` element.

The sidebar's width is clamped against the **live** layout, not against the drag
that set it. `applySideWidth()` in `panels.js` keeps `--side-w` between
`MIN_SIDE` (220 px) and `layout.width − MIN_CANVAS` (260 px), runs on every
relayout and restore, and remembers the *requested* width separately, so
re-widening the window hands back the width you dragged. Restoring a width saved
on a wider window used to put the sidebar past the viewport edge and collapse the
`minmax(0, 1fr)` canvas track to zero — the graph disappeared; and the docked
sidebar's corner grip wrote an inline size on the grid *item*, which left a gap
instead of resizing the track. Both paths now go through `--side-w`.

In the plain-web build (no Tauri) there are no native windows, so undocking falls
back to the in-page `#floatLayer`, which is all a browser can do.

## One namespace at a time, and expansions that leave their parent

The **Namespace** select at the top of Filters isolates one namespace's subtree
(the namespace node, its descendant namespaces, and everything declared in them).
It is a view filter, not a reload: the model keeps every node, so *All
namespaces* restores the whole graph instantly. Picking a namespace that is not
in the current subgraph pulls it in, expands it once and frames it. The choice
persists through `/api/viewer_state` and travels between windows with the other
filters (`sync.js`).

Expanding a node now always places its children clear of the node they came
from. The spline path measures the parent's own chip box before the first chip,
and the fallback (`seedFan`) fans the children outward from the parent's measured
box — the old fallback used a fixed 34..74 unit spiral, so a long namespace label
swallowed its own children. `noverlapIterate` separates axis-aligned chip *boxes*
rather than bounding circles, so a wide label can no longer overlap a neighbour
that is metres away by its centre.

## Detached panes — Sidebar, Source, Node, Find as real windows

Every pane can be its own window, the way Unity's editor panels can:

* **its own process window.** `open_pane_window` builds a Tauri `WebviewWindow`
  (`pane-side`, `pane-source`, `pane-nodeDrawer`, `pane-findPanel`) with the same
  webview/JS-thread/Monaco-worker isolation as a document window. The pane is
  moved into `#paneRoot`, which fills the window (`panewin.js`); the app chrome
  is hidden, and a pane that would normally hide itself cannot leave a blank
  window;
* **it is live, not a copy.** The window loads the same graph from the shared
  in-process engine, so every pane action — toggling a filter, selecting a node,
  expanding a relation, walking a resolve chain, editing the find query — works
  there exactly as in the main window;
* **the windows stay in step.** `sync.js` replicates the live view (selected and
  focused node, hidden children, filters, layout settings, depth, UI and editor
  themes, find query, open document) over Tauri events, in both directions: a
  filter toggled in a detached Sidebar immediately re-filters the main window's
  canvas, and a document opened in the main window appears in a detached Source
  pane. Echoes are suppressed by comparing the serialised payload, so the windows
  cannot ping-pong;
* **one window per pane.** Asking for an already-detached pane focuses its window
  instead of stacking a second copy;
* **closing the main window closes the viewer**, even while panes are open.

The `Find` pane gets its own query box in a pane window (the app's box lives in
the top bar, which a pane window does not have).

## Document windows — their own webview and thread

`⧉ New window` in the code pane opens the current document in its own native
window:

* the window opens **beside the main window**, cascaded so several documents do
  not land on top of each other, then is explicitly `show()`n, un-minimised and
  focused. When the main window's geometry is not known yet (a window requested
  from the command line before the main window is mapped) the monitor rectangle
  is used instead, so a secondary window is never left centred over the main
  window — or off-screen, which read as "the window never rendered";
* the host does **not** set `.incognito(true)`. wry maps that to
  `WebContext::new_ephemeral()` (`wry/src/webkitgtk/mod.rs`), and on WebKitGTK a
  second ephemeral context does not get its own frame — the document window
  stayed blank until the main window happened to repaint. Sharing the default
  `WebContext` is the well-tested multi-window path and is what makes a document
  window render on its own. Each window still has its own JS main thread and its
  own Monaco worker;
* the document target is injected as `window.__CS_DOC__` through
  `initialization_script`, so the window never depends on a query string
  surviving `Url::join` (see `open_doc_window`);
* each window owns a separate Monaco instance, and Monaco tokenizes in its own
  Web Worker, so the heavy editor work is off that window's main thread too;
* every `/api/*` call is answered on a Rust worker thread (`spawn_blocking`),
  so a slow document synthesis never blocks the window that asked for it;
* all windows share one `Index` behind a mutex, so they see one consistent
  graph instead of racing to re-index it.

In the plain-web build the same button degrades to `window.open`, which gets
equivalent isolation from the browser's own per-window processes.

### Opening windows from the command line

Useful on its own, and the only way to drive a multi-window session from a script
(synthetic input does not reach a Wayland client):

```bash
csviewer --repo /path/to/vrcCS --root _sealed_src \
  --open-pane side --open-pane source --open-doc '#ns:NZK'
```

`--open-pane` and `--open-doc` are repeatable; the windows are created once the
main window is mapped, so they land beside it.

## The Rust engine is a clone, and is checked like one

"Clone" here means *the same answers*, not a resemblance. `tools/oracle.py`
imports the frozen v1 modules by path and dumps everything the engine knows —
every node, every edge, every per-node relation bucket, find/refs results,
kinds, definitions, resolve chains, synthesized documents (as a hash plus head
and tail), and the reorg plan/verify report — into one canonical JSON document
with absolute paths rewritten to repo-relative. `core`'s `csgraph dump`
subcommand must produce the same document, and `tools/diff_clone.py` diffs them
and reports the first differing JSON key.

Two subtrees of that document are deliberately layout-different: `docs` and
`reorg`, because v2 merges the repeated `partial` headers of a type into one
(see [Known, deliberate deltas](#known-deliberate-deltas-from-v1)). Those are
compared — with the merge-aware allowance — by `tools/diff_doc.py` and
`tools/diff_reorg.py`, so `diff_clone.py` skips them by default and compares
every other section exactly; `--strict` compares the whole document. Runtime
cache counters are not part of the contract either: the dump drops
`state.{parseMs,fromCache,cache,docCache,viewer}`.

```bash
python3 tools/oracle.py --repo /path/to/vrcCS -o /tmp/gold.json
cargo run --offline -p csgraph-core --bin csgraph -- dump --repo /path/to/vrcCS -o /tmp/rust.json
python3 tools/diff_clone.py /tmp/gold.json /tmp/rust.json          # CLONE OK
python3 tools/diff_clone.py /tmp/gold.json /tmp/rust.json --strict  # full document
```

The dump itself is not quadratic: the five reorg specs are independent and
share one corpus graph, so `csgraph dump` builds its `before` graph once and
runs the specs as parallel units (85.8 s -> **23.0 s** on `_sealed_src`; the
document is byte-identical to the pre-change dump apart from the dropped
`state.docCache`).

## Building

```bash
cd "cs viewer builder"
./build-appimage.sh --verify
```

Artifacts land in `cs viewer builds/`:

```
csviewer-<yymmdd>.<hhmmss>-x86_64.AppImage
csviewer-Latest-x86_64.AppImage
```

Cargo builds are offline (`--offline` against the vendored crate directory) and
the Rust toolchain lives inside the builder directory (`rust-env.sh`); nothing
is written to `~/.cargo` or `~/.rustup`.

Final packaging is local-only too. An AppImage is a type-2 runtime with the
squashfs appended, and `appimagetool` downloads that runtime from GitHub on
every run unless it is handed one. It is therefore vendored as a tracked build
dependency:

```
cs viewer builder/deps/runtime-x86_64     the upstream type-2 runtime
cs viewer builder/deps/SHA256SUMS         its pinned digest
```

`build-appimage.sh` always passes `--runtime-file deps/runtime-<arch>` (after
verifying the digest), so packaging never fetches a runtime. The only things
that may touch the network once, into the ignored `cache/`, are `appimagetool`
itself and the optional portable CPython tarball; with both cached,
`./build-appimage.sh --verify --no-download` is a fully offline build. If a
future architecture needs a runtime, add `deps/runtime-<arch>` and its line in
`deps/SHA256SUMS` from
`https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-<arch>`.

`build-appimage.sh` self-heals ownership before it builds: if a previous root
run left root-owned `frontend/static/*.js` (mode 0600) or root-owned build
artifacts, the files are re-owned to whoever owns the checkout. Run as root it
just fixes them; run as an ordinary user it asks for sudo, and if sudo is
unavailable it prints the exact `chown` command instead of failing later with a
bare "Permission denied" from cargo or `cp`.

Launching the AppImage with no `--repo` and no repo marker (`.git`,
`tools/csgraph`) in the working directory — a plain file-manager double-click —
no longer auto-indexes that directory. A file-manager launch from `$HOME` or `/`
used to walk the whole tree while the canvas sat on "Connecting to server…";
now it opens with "Nothing indexed yet" and lets you pick a folder. Launching
from inside the checkout (or passing `--repo`/`CSVIEWER_REPO`) still indexes
`_sealed_src` immediately.

## MCP server — `csgraph --mcp`

The engine can also be driven by an agent over the Model Context Protocol
instead of the GUI. `csgraph --mcp` runs a stdio JSON-RPC 2.0 server over an
indexed repo:

```bash
target/debug/csgraph --mcp --repo /path/to/project --root . --threads 4
```

* **multithreaded** — `--threads K` worker slots, each a complete index; reads
  run in parallel and the stdio read loop never blocks on a long tool call;
* **pre-indexed** — with `--root` the **primary** repo's graph is built once at
  startup (reusing the `.csgraph.v2` cache) and held in memory for the life of
  the process; secondary repos build on first call; with no `--root` the server
  starts cold and the `index` tool builds it;
* **any command the app can execute** — `tools/list` exposes a typed tool per
  common route plus a generic `api_call`, all routed through the same
  `api::handle_payload` / `api::handle` the HTTP host uses, so the whole
  `/api/*` surface is reachable, reads and writes (`/api/index`,
  `/api/index/cancel`, `/api/settings`, `/api/viewer_state`, `/api/reorg`).
  `index_cancel` is answered out of band, so it interrupts a rebuild that holds
  the write gate instead of queueing behind it. **42 tools** as of 2026-10-15:
  the routed set plus fourteen MCP-only edit tools (`select_decls`, `move_decls`,
  `grep`, `copy_path`, `fuzzy_search`, `resolve_chain`, `code_block`,
  `rename_symbol`, `qualify_symbols`, `expand_namespace`, `write_file`,
  `apply_edits`, `reformat`, `functionize`), the read-only `preview_change`
  signature/call-site preview (M3 slice 2), and five aux tools — `parse_check`
  (the Roslyn validity gate), `compile_diff` (the differential Roslyn compile,
  no Unity process/licence/focus), `find_duplicates` (the dedupe report),
  `counts` (first-class corpus/symbol counts, now per-root scoped) and
  `workspace` (which
  now enumerates **every** bound repo in `repos`). The MCP-only tools dispatch
  before the route layer; `/api/edit`, `/api/analyze` and the read-only
  `/api/preview` give the app the same named actions;
* **several repos in one server** — `--repo` is repeatable (the first is the
  primary) and **every tool** takes an optional `repo` argument (1-based index,
  path, or unique basename; absent/`null` = primary; unknown/ambiguous is an
  error listing every bound repo). `index`/`index_cancel` act on the selected
  repo only, and `repo` is stripped before dispatch so frozen routed payloads are
  unchanged. The DSH plugin defaults to one server with repeated `--repo`; the
  old split is opt-in via `--split-servers`/`CSGRAPH_SPLIT_SERVERS=1`;
* **durable `copy_path`** — `action:"recover"` (dry-run unless `apply:true`) plus a
  fsynced, engine-shaped journal at `<repo>/.csgraph-copy/<nonce>.json` and
  per-target backup-by-rename make a crashed apply restorable across process
  boundaries (idempotent); a non-engine-shaped journal and a hash-mismatched
  `existed:false` target are refused rather than obeyed; hardlinked sources and
  targets are refused; test-only `CSGRAPH_COPY_FAULT` injects the crash.
  `.csgraph-copy` is a default exclude;
* **no daemon and no new dependencies** — stdout carries protocol traffic only,
  logs go to stderr, EOF exits; the framing is `serde_json` over the existing
  vendored crates.

`tools/MCP.md` is the full reference: argv, a harness `mcpServers` snippet, the
tool/route table, a worked transcript, threading semantics, lifecycle and
troubleshooting.

## Editor features

Everything the pane can do beyond showing text is a Monaco provider (or a
panel) registered through one extension point, so each feature is its own file
and `editor.js` stays the editor host.

| feature | file | what |
|---|---|---|
| rich hover | `hover.js` | kind, signature, access, `file:line` link, degree, caller/reference counts (`/api/node`) |
| document symbols | `symbols.js` | Ctrl+Shift+O outline, nested, document order — also feeds Monaco sticky scroll |
| folding | `symbols.js` | one `FoldingRange` per declaration span |
| document highlight | `symbols.js` | cursor word → the graph's reference sites (word-boundary scan as fallback) |
| semantic tokens | `semantics.js` | corpus symbols coloured by graph kind; BCL identifiers keep their syntax colour |
| CodeLens | `codelens.js` | `N references · M callers · show in graph` per declaration; lazy, 400-declaration cap |
| call hierarchy | `hierarchy.js` | floating peek/tree over `/api/chain` (direction, depth, edge kind, `file:line`, cycle badges), Shift+Alt+H |
| type hierarchy | `hierarchy.js` | bases/derived from `inherits` relations (legitimately empty in `_sealed_src`: 0 inherits edges there) |
| go to implementation | `hierarchy.js` | interface/abstract member → implementing members (Ctrl+F12) |
| quick-open | `quickopen.js` | Ctrl+P and Ctrl+T over `/api/search` + `/api/find` |
| breadcrumbs | `nav.js` | `namespace › type › member` bar built from `/api/decls` |
| pane navigation history | `nav.js` | Alt+Left / Alt+Right, with the graph's history as fallback |
| open definition aside | `nav.js` | `editor.action.revealDefinitionAside`, else a second document window |
| open in your real IDE | `nav.js` + `app/src/main.rs` | `open_external` → `code`/`rider` at `file:line` |
| problems | `problems.js` | parser warnings from `/api/state.warnings`; labelled as parser warnings, no Roslyn here |

Two capabilities are deliberately hand-built because this vendored Monaco 0.57
standalone does not ship them — `grep -c registerCallHierarchyProvider
vendor/monaco/monaco.d.ts` is `0` — the call/type hierarchy is a peek/tree
panel, and Ctrl+P/Ctrl+T are custom palettes.

A feature file ends with:

```js
window.CsEditorExtension({ name: 'hover', attach: function (ctx) { … } });
```

`attach` runs once per Monaco editor instance with the shared context
(`monaco`, `editor`, `apiGet`, `openSource`, `goToNode`, `lang`, `recordAt`,
`lines()`, `model()`, `revealLine`, `on(evt, cb)`, …), and
`ctx.on('model', cb)` fires after every `show()`. Symbol resolution stays in
`window.CsLang` — one resolver, exported — so no feature re-implements it.
`editor.js` turns on `occurrencesHighlight`, `stickyScroll` and semantic
highlighting.

Acceptance, against the Rust engine over `--serve`. `tools/with_serve.sh` boots
a *private* engine, waits for `/api/state`, wires `--url`/`--cdp-port` into the
probe it runs, and always tears the server down — so no check leaves a daemon
behind:

```bash
tools/with_serve.sh -- node tools/headless_check.mjs --node '#ns:NZK'
tools/with_serve.sh -- node tools/probe_features.mjs
```

`probe_features.mjs` drives the real page in headless Chromium and asserts the
page is clean and every provider answers: hover card, nested outline, folding,
semantic tokens, CodeLens, a call-hierarchy tree with edge kinds and lines,
quick-open rows, breadcrumbs, pane history, and the problems list (20 checks).

Two more probes run through the same wrapper:

```bash
# type hierarchy against a corpus that really has inheritance (tools/fixtures/inherit)
tools/with_serve.sh --repo "$PWD/tools/fixtures" --root inherit -- node tools/probe_inheritance.mjs

# tabs / split / session restore on the real viewer
tools/with_serve.sh -- node tools/probe_tabs.mjs
```

## Peak load — a document is synthesized once and shared

Showing a *partial* declaration means synthesizing its whole document: every
partial site, every `#if` section, wrapped in the namespace/type chain that
encloses it. `#ns:NZK` is 35,715 lines / 8.8 MB of JSON. At peak that synthesis
must happen once, not once per viewer, and a hit must not copy the result.

What the engine guarantees now (`core/src/index.rs`, `core/src/doc.rs`):

* **one synthesis per node.** `doc_source()` memoizes the payload in
  `doc_cache`; a second open is a lookup.
* **one shared payload, not a clone per request.** The cache holds
  `Arc<Value>` and the Tauri host serializes that `Arc` directly
  (`api::handle_payload`, `Payload::Shared`). The old cache-hit path deep-cloned
  the entire document before answering.
* **one lex per file, not one per document.** `Index` owns a single `DocCache`
  for the session (the analogue of v1's `doc_cache`); it used to pass
  `&DocCache::new()`, so *every* document build re-parsed every file it touched.
  `Rc -> Arc` keeps the index `Send` for the worker threads.
* **one node map, built once.** `nodes_map()` rebuilt and deep-cloned every node
  object on each synthesis; `nodes_by_id` is built at install time.
* **a memory ceiling.** `doc_cache` is bounded to `DOC_CACHE_MAX = 16`
  documents, oldest first — clicking through nodes at peak cannot grow it
  without limit.

`/api/state` reports it:

```json
"docCache": { "documents": 1, "files": 39, "modelHits": 0, "modelMisses": 39 }
```

Measured on `_sealed_src`, `#ns:NZK` (35,715 lines, 8.78 MB), over `--serve`:

| request | time | note |
|---|---|---|
| first `GET /api/doc?node=#ns:NZK` | **767 ms** | 39 files lexed (`modelMisses: 39`) |
| repeated (memoized, shared) | **183 ms** | serialization only, no re-synthesis |
| a second document sharing those files | — | `modelMisses` stays 39 |

Two regressions pin this down: `doc_cache_is_shared_across_documents_and_hits_are_shared`
(model misses do not grow, and a hit returns the same `Arc`) and
`decls_lists_nested_declarations_in_order`.

## Cancelling an index — Stop

A wrong folder used to mean waiting out the whole build. The loading overlay now
carries a **Stop** button (tooltip: "Stop the running index. The previous graph
stays loaded and usable."), and **Esc** does the same while it is up. Both send
`POST /api/index/cancel`.

The route flips one `Arc<AtomicBool>` the host shares with the index; the host
intercepts it *before* taking the engine lock, because an in-flight build holds
that lock for its whole run. The build checks the flag with a relaxed atomic load
between files while parsing, between files while reading source text, every 1,024
nodes, and at each phase boundary — an O(1) test inside loops that already exist,
not an extra scan over the corpus. A cancelled build never installs: it restores
its pre-build bookkeeping from a `BuildSnapshot` and returns the **previous**
state with `cancelled: true`, so the graph you already had stays loaded and
queryable, the overlay closes, and the toast reads "Index cancelled — previous
index is still active" rather than a failure. A later Index runs normally (every
build clears the flag first).

Measured on this checkout: Stop → quiescence **4–26 ms warm**, **~7.0 s worst
case cold-cache** — the check is not inside a single file's parse or inside the
pre-parse directory walk, so Stop can take as long as the current file or the
remaining discovery pass. The previous index stayed byte-for-byte the same
(39 files / 5,222 nodes / 11,911 edges) and the next Index rebuilt it.

## Engine performance

The engine is on a path to a multi-hundred-thousand-line corpus, so the hot
paths are linear, not just memoized. On `_sealed_src` (39 files, 5,222 nodes,
11,911 edges) the parse used to rebuild its haystack on every regex probe,
recompile the member regexes on every call, and re-slice a whole file per node
in the reorg audit — all of them quadratic in the file size:

| operation | before | now |
|---|---|---|
| `Parser::parse` of one 772 KB / 35,715-line file | >25 s | **2.2 s** |
| graph for the whole corpus (39 files, parallel) | >30 s | **5.4 s** |
| `diff_parser.py` (parser + graph clone) | ~88 s | **8.6 s** |
| reorg `audit_text` per spec | 9.0 s | **0.84 s** |
| reorg `verify` for `group=owner` | 23.8 s | **10.2 s** (was 15.7 s; `before` reused) |
| `csgraph dump-reorg`, all five specs | ~90 s (serial, 5× `before`) | **17.1 s** (one `before`, parallel specs) |
| `csgraph dump` (canonical clone document) | 85.8 s | **23.0 s** (same reuse) |

What changed:

* `model::Prepared` converts a file to a `String` and its byte→char table
  **once**; `Re::find_p` / `find_from_p` / `match_at_p` reuse it, so the parser
  no longer copies the file per probe.
* the per-member regexes (`mem_name`, `name_before_paren`) are compiled once
  behind `OnceLock` instead of per declaration.
* `parser::parse_files_parallel` fans files across cores while preserving file
  order (`csgraph dump-parser` uses it; `corpus_graph` does the same).
* `reorg::audit_text` converts each file to chars once per call, not once per
  node.

`csgraph dump-reorg` also gained `--spec KEY` (repeatable, so a caller can keep
each invocation inside a wall-clock budget) and `--time`, which prints the
per-phase timings:

```bash
csgraph dump-reorg --repo /path/to/vrcCS --root _sealed_src --spec 'group=owner' --time
# [reorg] graph: 5222 node(s), 39 file(s), 5.4s
# [reorg] group=owner: plan 0.6s, emit 0.07s (1 file(s)), verify 10.2s
```

The `after` re-parse — re-parsing the emitted text — stays: that re-parse *is*
the audit, and it is what proves the emitted *text* still declares the same
things. What changed in the reorg-budget pass is everything around it:

- the `before` pass (a full corpus re-parse, identical for every spec) is now
  built once by `corpus_graph_with_builder` and shared into every spec through
  `Reorg::verify_with_before` — `verify` still builds its own when called
  without one, so other callers are unchanged;
- `cli_dump_reorg` runs the wanted specs as **parallel units**, one engine per
  worker, and joins the results in `default_specs` order, so the JSON is
  byte-identical to a serial run;
- `tools/diff_reorg.py --spec KEY` (repeatable) passes the shard to both
  producers, and `verify_all.sh` queues five `reorg clone: <spec>` units, so no
  unit except the build exceeds 30 s.

Per spec, `dump-reorg --spec 'group=owner' --time` went from 22.67 s wall /
`verify 16.20 s` to **16.51 s wall / `verify 10.15 s`**, and the full five-spec
run from ~90 s to **17.06 s** (the two per-spec JSON documents are byte-identical
to the pre-change dumps).

## `/api/decls` — the declarations a document contains

The outline, folding, sticky scroll, breadcrumbs and CodeLens all need the same
thing: the declarations the open document contains, with their line ranges.

```
GET /api/decls?node=<id>   # the node + its containment subtree, document order
GET /api/decls?file=<path> # every declaration in one file
```

Rows are `lite` node objects (`id, name, label, full, kind, file, line,
endLine, signature, ret, access, static, degree, owner, parent`), so the
frontend maps them onto its Monaco model by `(file, line)` and colours them by
`kind`. String-literal nodes and unresolved `external` stubs are filtered out;
the response is capped at `DECLS_LIMIT = 5000`. No frontend re-parsing of C#.

## Development

**Working rules (owner-set):** no operation may exceed **30 s**, a build at
most **90 s**; testing units run in parallel, one unit per task; never leave a
`--serve` daemon running. The repo README and
[thingstodo.md](../../thingstodo.md) carry the same rules and the open work.

Run the **real Rust engine** behind HTTP — no window, no Tauri, no display —
and point the frontend and the headless checks at it:

```bash
# the v2 engine + frontend over HTTP (new)
target/debug/csviewer --serve --port 8795 --repo /path/to/vrcCS --root _sealed_src
node tools/headless_check.mjs --url http://127.0.0.1:8795/ --node '#ns:NZK'
```

`--serve` hands out `frontend/` for everything outside `/api/*` and routes
`/api/*` through the same `Engine::handle_payload` the Tauri IPC command uses,
so the frontend needs no change (it already falls through to real `fetch` when
`window.__TAURI__` is absent). It is a development surface: 127.0.0.1 only, one
thread per connection, no keep-alive.

`--serve` is a daemon, so a test must never leave it running. Use the wrapper:
it picks private ports, waits for `/api/state` (the engine 404s on a doubled
slash, so it probes the base URL), appends `--url`/`--cdp-port` to the probe
unless they were given, and always tears the engine down — including on Ctrl-C:

```bash
tools/with_serve.sh -- node tools/probe_features.mjs
tools/with_serve.sh --port 8795 -- node tools/headless_check.mjs --node '#ns:NZK'
```

Against the frozen v1 engine instead (the API oracle, no v1 edits). The frozen
tree lives at the **repo root** (`<repo>/tools/csgraph/`), so from this
directory it is `../../tools/csgraph/`:

```bash
# terminal 1 — the frozen v1 engine, as the API oracle
python3 ../../tools/csgraph/server.py --repo /path/to/vrcCS --port 8790

# terminal 2 — the v2 frontend, proxying /api to v1 (no v1 edits)
python3 tools/devproxy.py --port 8799 --upstream 127.0.0.1:8790

# terminal 3 — drive it headlessly and measure
node tools/headless_check.mjs --url http://127.0.0.1:8799/ --node '#ns:NZK'
node tools/headless_check.mjs --url http://127.0.0.1:8799/ --node '#ns:NZK' --legacy
```

### Open in your real IDE

The viewer browses and reads; editing stays in a real editor. `open_external`
is a Tauri command (`app/src/main.rs`) invoked from the source pane:

```js
window.__TAURI__.core.invoke('open_external', { file, line })
```

It honours `CSVIEWER_EDITOR`, which may contain `{file}`/`{line}`
placeholders (`CSVIEWER_EDITOR='rider --line {line} {file}'`); otherwise it
tries `code -g file:line`, then `codium`/`code-insiders`, then
`rider`/`idea --line N file` on `PATH`.

## Status

| piece | state |
|---|---|
| Monaco source pane, 4 themes, line-click, match highlight, decorations | **done, verified** |
| Lag fix (35 DOM nodes for a 35,715-line document) | **done, measured** (re-measured 2026-10-14; see [Why the editor changed](#why-the-editor-changed)) |
| Per-document native windows, each its own webview + JS thread | **done** |
| Every pane detachable into its own OS window, placed beside the main window | **done** |
| Live view-state sync between windows (selection, document, filters, find, theme) | **done** |
| Offline vendored Rust build for both crates | **done** |
| `csgraph-core` lexer + parser + graph (`cs_parser.py`) | **done — 0 differences** on 39 files / 5,222 nodes / 11,911 edges |
| `csgraph-core` cache + config/defines | **done — unit-tested** |
| `csgraph-core` doc (`cs_doc.py`) | **done — byte-identical**, incl. a 381-node independent stratified run |
| `csgraph-core` reorg (`cs_reorg.py`) | **done — `OK: 0 differences`** via the merged-partial-header allowance in `tools/diff_reorg.py`. The allowance is directional (v2 may only *remove* lines/chars), explains every declaration v2 fails to re-parse by the unbalanced parts v2 itself reports, and is not vacuous: `--strict` still fails and mutated dumps are still caught. Residual: for `granularity=leaf\|group=size\|size=3000`, v2's own self-audit reports 3 unbalanced parts / 59 re-parse misses where v1 reports none — inside the merged-header delta, see [thingstodo.md](../../thingstodo.md). The runner shards the five specs (`diff_reorg.py --spec KEY`); the heaviest reorg unit is 21.9 s |
| `.sln`/`.csproj` project model | **done** — `core/src/config.rs` parses solutions and projects (name, TFM, `<DefineConstants>`, SDK globs + explicit `<Compile Include>`); `/api/projects` exposes the model plus `defineSources` attribution and per-file lookup. Additive: node payload and `/api/state` shape unchanged |
| document tabs, split, session restore | **done** — `window.CsTabs` in `shell.js`; open documents become tabs, a second Monaco editor splits the pane, and `viewer_state` round-trips `{tabs, active, split}` |
| `open_external` (open in your real IDE) | **done, unit-tested** — 10 tests in `app/src/main.rs`, incl. a real spawned fake editor recording argv |
| `csgraph-core` index + API (`server.py`) | **done — CLONE OK** (all 5,222 nodes, 11,911 edges, `nodeDetail` for all 2,507 declarations, find/kinds/definitions/chain/docs/reorg) |
| `compile_check.py` (differential Roslyn) | **done — 0 differences** on a *real* compile, plus the `skipped` paths |
| `build-appimage.sh` | **done** — 40 MB AppImage, `--verify` green with the Rust engine selftest |
| Stop / cancel an in-progress index | **done, verified** — Stop button + Esc → `POST /api/index/cancel`; one shared atomic checked in the parse and source-read loops, every 1,024 nodes and at phase boundaries; a cancelled build never installs and the previous index stays live. `probe_index_stop.mjs` 15/15; Stop → quiescence 4–26 ms warm, ~7.0 s worst cold-cache |
| Sidebar scales to the live layout | **done, verified** — `--side-w` clamped to `MIN_SIDE … layout.width − MIN_CANVAS`, re-clamped on every relayout and restore, requested width remembered; the docked corner grip resizes the grid track. `probe_sidebar.mjs` 28/28, 0 console errors, before/after screenshots at 1920/1440/1100/980/760 |
| MCP server (`csgraph --mcp`) | **done, verified** — multithreaded stdio JSON-RPC over a pre-indexed repo with full `/api/*` read+write parity; `index_cancel` interrupts a rebuild out of band; every result carries a unified `ok`; `--mcp --help` prints usage without starting. **42 tools** (22 routed + 14 MCP-only edit + `preview_change` + 5 aux). Fourth pass: a **multi-repo server** (`--repo` repeatable, first = primary) with an optional per-call `repo` on every tool (index/path/unique basename; absent/`null` = primary) and `workspace.repos`; a **durable `copy_path`** (`action:"recover"`, fsynced `<repo>/.csgraph-copy/<nonce>.json` journal, `recover:true`, test-only `CSGRAPH_COPY_FAULT`); the DSH plugin defaults to one server per repo set with opt-in `--split-servers`. Fifth pass: `preview_change` (M3 slice 2, read-only); `copy_path` refuses hardlinked sources/targets and re-checks `O_NOFOLLOW`-opened files by `dev`/`ino` before the displacing rename; `action:"recover"` is dry-run unless `apply:true` and refuses non-engine-shaped journals; `--threads 0` now really means all cores. `probe_mcp.py` is sharded into `read` 14/14 4.7 s, `concurrency` 5/5 2.8 s, `cancel` 9/9 5.7 s, `restart` 3/3 4.5 s; `probe_edit_mcp.py` **76/76** (3.3 s), `probe_multirepo.py` **10/10** (0.07 s), `probe_copy_recover.py` (durable copy_path), `probe_preview.py` **PASS** (read-only preview); reference in [tools/MCP.md](tools/MCP.md) |
| `nzkcs` write half — `move_decls` | **done, verified** — dry-run default; `select_decls` selector, leading-comment slice, extract form with the dialect wrapper, rename-into `to="<Ns>.<Class>.<member>"`, delete + call-site re-reference, all-or-nothing atomic commit, and a Roslyn parse gate on `apply` (`validated:"roslyn"`; refuses on any diagnostic). 12 unit tests, `probe_edit_mcp.py` 74/74 |
| M2 slice 2 — `reformat` + `functionize` | **done, verified** — dry-run-by-default dialect rewrite (`/* */` only, no usings, graph-proven qualification, trivial-body collapse, one-namespace/one-static-class shape) and exact-cluster extraction into one shared helper with every duplicate site rewritten; both atomic and Roslyn-gated (`validated`/`unverified`/`require_validated`). 9 unit tests, `probe_reshape.py` 12/12 in 0.4 s; refuses comments containing `*/`, raw strings, differing bodies and unresolvable identifiers |
| MCP gap 4 — first-class `counts` | **done, verified** — one MCP-only call for corpus counts (`files/nodes/declarations/byKind/edges/byEdgeKind`) or one symbol's attributed counts (`definitions/matches/usages/incoming/outgoing/callers/usageLines`); no field is added to the frozen `find`/`refs` payloads |
| user/agent parity (first pass) | **done, verified** — `GET/POST /api/edit` (11 edit tools) and `GET/POST /api/analyze` (`parse_check`, `dedupe`) intercepted in `Engine::handle`, so Tauri IPC and `--serve` share them; app Edit panel + Validate/Duplicates. `probe_edit_app.mjs` 35 PASS, `cargo test -p csviewer` 20 passed; `core/src/api.rs` untouched |
| M0 validity gate (parse level) | **done, verified** — `csgraph parse-check` + MCP `parse_check` run a real Roslyn driver built offline from the SDK (`dotnet-roslyn 4.12.0.0`); `_sealed_src` 52 files / 0 diagnostics, exit 1 on a diagnostic, exit 3 + a concrete `skipped` reason with no toolchain — never a false ok. A *compile* gate still needs Unity/VRChat refs |
| M2 slice 1 — dedupe | **done, verified** — `csgraph dedupe` + MCP `find_duplicates` (byte-identical bodies): 52 files / 2156 bodies / exact 345 / near 9 / blocks 945 in 4.8 s, deterministic, hashed, capped |
| bounded indexing | **done, verified** — default excludes prune `cache/`, `target/`, vendored trees; a 200k-file / 15 s discovery guard returns `ok:false, aborted:"discovery-bounds"` and keeps the previous graph. `index {roots:["."]}` on vrcCS ~14 s (was a 120 s timeout); `_sealed_src` unchanged at 52 files / 6119 nodes |
| M3 slice 2 — signature/call-site change preview | **done, verified** — MCP-only read-only `preview_change` (`core/src/preview.rs`): one declaration + `op:"rename"\|"retype"`, current/proposed signature, `affectedDeclarations`, `callSites` with lines, exact before/after `edits`, counts, loud refusals (unknown/ambiguous/bad `to`/bad `op`); `rename` plans through the real `rename_symbol` dry run. No write path (`readOnly:true, applied:false, writes:[]`). App twin `GET|POST /api/preview` + a read-only preview panel. `probe_preview.py` PASS (0.07 s, sha256-verified zero writes), `probe_preview.mjs` PASS via `with_serve.sh`; independent verifier 52/52 |
| M2 on the real corpus — `reformat` + `functionize` applied | **done, verified** — a `copy_path` copy of the 123-file `_sealed_src` tree (41 MB, byte-identical) was reshaped through MCP: `reformat` 52/52 ok (34 applied / 18 no-op; 777 comments, 1 usings, 1103 qualified names, 7 collapses), then `functionize` on the top clusters (2 applied, `NZK.Shared.cs` + 4 call sites). Copy parse-check **0/53**; `diff_parser.py` **0 differences** both sides; `_sealed_src` provably untouched (123/123 sha256). Eight `reshape.rs` defects fixed with 9 regression tests (`probe_reshape_corpus.py` 7/7). Honest nuance: default `qualify:true` adds explicit-prefix `uses` edges, so the raw graph moves; the additive `reformat{qualify:false}` is structurally graph-identical (0 nodes/edges). Report: `target/reshape-corpus/REPORT.md`. Independent verifier: parse/untouched NOT FALSIFIED, "graph byte-identical" falsified for the default and documented |
| compile the toolkit for real | **done, verified** — `find_unity_data()` now auto-discovers the Unity 2022.3.22f1 Editor at `/500gb/Unity/Hub/Editor/*/Editor/Data` (no env var, no `--data`), `UNITY_REFS` tolerates the `Managed/UnityEngine/UnityEditor.CoreModule.dll` layout, and `compile-diff` carries `toolchain`/`references` plus additive `baselineDiagnostics`/`candidateDiagnostics`. `_sealed_src` really compiles under `unity-roslyn` (31 CS1022 from the corpus' own stray `}` fragments; no Unity CS0234/CS0246 remain). Honest skips preserved: `--data /nonexistent` still exits 3. Independent verifier 15/15. Report: `target/compile-real/REPORT.md` |
| SPS slice 2 — real geometry bake + real compile | **done, verified** — the `SpsSocketBaker` stub is replaced (through MCP) by the real VRCFury-modelled pipeline: `SampleRenderer`/`BakeMesh`, the SPS data texture (`BakeDataTexture` + `SpsBakedTexture`), radius sampling/backfill, `AutoRig`/`CalculateWeight`, socket placement and `AssetDatabase` output; one genuinely missing leaf (`Systems.Baking.GetGenRoot`) added as a real definition, the "undefined" `Vars`/`Systems`/`Overloads` were in fact behind `#if UNITY_EDITOR && VRC_SDK_VRCSDK3`. Result: `compile-diff` **exit 0, 0 errors, 0 warnings, `unity-roslyn`** on the 24-file bench; `parse_check` 0. Honest: the bake compiles but has never been executed (no Unity runtime). Transcript: `target/sps-bench/REPORT.md` |

### Known, deliberate deltas from v1

`cargo test --offline -p csgraph-core` → **173 passed**; `cargo test --offline -p csviewer` → **20 passed**.

* **viewer state storage.** v1 keeps theme/camera/node positions in the corpus'
  SQLite file (`<root>/.csgraph.db`); v2 keeps them in its own store
  (`<root>/.csgraph.v2/viewer.json`, see `cache.rs`). `/api/state` therefore
  reports the same shape but a different backing store. The canonical dump
  drops that field for both sides.
* **ZIP bytes.** v1 uses `zipfile`, v2 writes its own container (local headers +
  central directory + EOCD, DEFLATE via flate2). Entry names, order and contents
  match and the archive is valid; the compressed bytes are not identical by
  design.
* **timing.** No `ms` value is compared; the two engines cannot agree on how
  long they took.
* **`default_vrc_dirs()` ordering.** v2 returns sorted glob matches where
  Python's `glob.glob` is unsorted. Zero matches on a machine without a VRChat
  package dir, and the reference order cannot change parsed diagnostics.
* **external-symbol "view code".** The graph carries both an external stub
  (`NZK`, from a bare reference) and the declared namespace (`#ns:NZK`). v1
  answers `file not indexed` for the stub even though the declaration exists; v2
  resolves the stub to the declared node with the same `full`/`label` and
  synthesizes its document (`Index::doc_alias`). A symbol with no local
  declaration at all is *not* a 404 either: `Index::external_usage_doc` gathers
  every edge that touches the stub, in file order, and emits a document whose
  sections name the referencing declaration and print the real source lines —
  so a leaf node always has something to read, and every section is a hint about
  which folder to add to the index. `/api/source` also accepts a `full` name, a
  label, or a `file:line` target (the pop-out button's older link shape);
  `Index::resolve_source_id` and `Index::split_file_line` keep a valid
  declaration from ever failing to resolve.
* **nested namespace blocks.** `namespace A { namespace B { … } }` used to be
  read as two unrelated declarations: the inner block was started again from the
  top-level scan under the bare name `B`, so its declarations were emitted as
  `B.…`, and `A` produced **no namespace node at all**. In a corpus whose files
  all nest that way — `namespace Company { namespace Product { … } }` — the
  namespace list collapsed to the single inner name, even though two namespaces
  are declared. The Rust parser now walks a nested block as part of its parent
  (`A.B`) and starts only the outermost block from the top-level scan, so both
  `Company` and `Company.Product` become nodes. The frozen Python oracle keeps
  the old behaviour, but `_sealed_src` declares a flat `namespace NZK` and has
  no nested blocks, so `tools/diff_parser.py` is still **0 differences**
  (`parser::tests::nested_namespace_keeps_parent_name`).
* **document windows are the document and nothing else.** `?win=doc` (or the
  native host's injected `window.__CS_DOC__`) hides the sidebar, canvas,
  overlays and splitters, and the window says so in the pane when it is asked
  for nothing, instead of rendering the graph chrome behind a hidden canvas.
* **Unity-style panel layering.** Floating panes live in one `#floatLayer`
  above every docked pane, and the last pane you touch is raised
  (`panels.js::bringToFront`), so a docked sidebar/source/drawer can never paint
  over a floating pane.
* **merged partial declarations in synthesized documents.** v1 repeats the whole
  wrapper + type header for every `partial` declaration site, so `NZK.B` (8
  sites) or `NZK.Core` (39 sites) reads `class B {` again and again. v2 groups
  the sites that share a wrapper *and* a conditional context, emits the
  namespace, the `#if`/`#elif`/`#else` prefix and one `partial` type header
  once, then every fragment's inner body under its own
  `// ── label · file:a-b ──` marker. A fragment under a different condition
  still gets its own labelled section, because there the condition is the
  context. `doc.rs::emit_merged_group` + `frame_context_key`; covered by
  `partial_sites_share_one_header`, `same_conditional_context_merges_once` and
  `different_conditionals_stay_separate`. `tools/diff_doc.py` compares the
  layout-independent invariants (declaration sites, `#if` records, and that the
  merged side never emits *more* headers); pass `--strict` for the old
  byte-identical v1 layout.

`tools/csgraph` is verifiably untouched: no file under it has been modified, and
no vendored Monaco or v2 tooling was left behind in it.

### Current engine checks

The whole matrix now runs as bounded, parallel units (one test per process, a
job cap, per-unit logs and wall times); only the offline build is allowed to
take longer, and it is verbose and monitored. The full run is **230 units, 230
PASS, 0 FAIL, max non-build unit wall 28.160 s** (the five reorg specs, the
clone dump and the real-Roslyn compile units are the heavy ones), so every
non-build unit is under the 30 s cap. The five
reorg specs, the clone-dump comparison, the Stop/cancel probe, the four MCP
shards, the edit-app probe, the MCP edit probe, the multi-repo, preview,
reshape-corpus and copy-recover probes, the validity gate and the dedupe probe
are separate units:

```bash
tools/verify_all.sh              # build + 230 units; max 28.160 s
tools/verify_all.sh --quick      # skips the clone dump
tools/verify_all.sh --dry-run    # print the unit plan without executing

# the browser-free acceptance probes are units above; the browser ones are not
tools/with_serve.sh -- node tools/probe_sidebar.mjs        # 28/28, 0 console errors
tools/with_serve.sh -- node tools/probe_features.mjs       # 20/20
tools/with_serve.sh -- node tools/probe_tabs.mjs           # 11/11
tools/with_serve.sh -- node tools/probe_preview.mjs        # /api/preview + panel, read-only
tools/with_serve.sh -- node tools/probe_scratch.mjs        # 16/16 Blocks canvas
tools/with_serve.sh --repo "$PWD/tools/fixtures" --root inherit \
  -- node tools/probe_inheritance.mjs                      # 11/11
python3 tools/probe_mcp.py --shard read                    # 14/14, 4.7 s
python3 tools/probe_mcp.py --shard concurrency             # 5/5, 2.8 s
python3 tools/probe_mcp.py --shard cancel                  # 9/9, 5.7 s
python3 tools/probe_mcp.py --shard restart                 # 3/3, 4.5 s
python3 tools/probe_edit_mcp.py                            # 76/76, 3.3 s, 42 tools
python3 tools/probe_preview.py                             # preview_change: read-only, deterministic
python3 tools/probe_multirepo.py                           # 10/10, 0.07 s (multi-repo server)
python3 tools/probe_copy_recover.py                        # 26/26 (durable copy_path + forgery refusals)
python3 tools/probe_validity.py                            # real Roslyn / honest skipped
python3 tools/probe_dedupe.py                              # dedupe report; MCP == CLI
python3 tools/probe_reshape.py                             # 12/12, 0.4 s (reformat + functionize)
python3 tools/probe_reshape_corpus.py                      # 7/7 corpus-defect fixtures
node dsh_plugin/test/config.test.mjs                       # 15/15, both multi-repo modes
node dsh_plugin/test/smoke.mjs                             # 42 tools PASS

# one reorg spec at a time, both producers through the differential
python3 tools/diff_reorg.py --repo /path/to/vrcCS --root _sealed_src --spec 'group=owner'

# the canonical clone document, then its differential
python3 tools/oracle.py --repo /path/to/vrcCS -o /tmp/gold.json
target/debug/csgraph dump --repo /path/to/vrcCS -o /tmp/rust.json
python3 tools/diff_clone.py /tmp/gold.json /tmp/rust.json   # CLONE OK (--strict: full)

python3 tools/diff_parser.py --repo /path/to/vrcCS --root _sealed_src
# counts python: {'files': 52, 'nodes': 6119, 'edges': 14169, 'graphWarnings': 0}
# counts rust:   {'files': 52, 'nodes': 6119, 'edges': 14169, 'graphWarnings': 0}
# OK: 0 differences

cargo test --offline -p csgraph-core     # 198 passed (+1 bin)
cargo test --offline -p csviewer         # 22 passed
```

Packaging for this pass (fifth): `./build-appimage.sh --verify --no-download`
produced `cs viewer builds/csviewer-261015.190707-x86_64.AppImage`
(**81,312,248 bytes**, sha256
`d9952650ed6ef05791b46979f9c51b015eb7ba080954348e50769d61585ba1e6`), exit 0,
bundled driver reporting real diagnostics, `Latest` refreshed.

Every headless-Chromium probe now calls `process.exit()` in its `finally`: a
killed Chromium leaves the renderer children holding the stderr pipe, and the
Node event loop would otherwise idle ~25 s in teardown, pushing `probe_features`,
`probe_tabs` and `probe_inheritance` past the 30 s rule (they run in 8 s, 12 s
and 2 s now, measured 2026-10-14).

## Things to do

Open work is tracked in [thingstodo.md](../../thingstodo.md) and the
next-session handoff is [prompt.md](../../prompt.md). The feature, reorg-budget
and the 2026-10-15 finish/second/third/fourth/**fifth** passes are closed: the
real-corpus `reformat` + `functionize` run, the SPS slice-2 bake with a real
Unity-Roslyn compile, the M3 slice-2 signature/call-site preview, and the
compile-for-real milestone all landed, and the ordered backlog is now the M3
slice-3 editing flow, executing the SPS bake, the `group=single` compile-level
layout decision, and the owner-vision extras — each listed there with evidence,
in the owner's `(2,3,1)` order.

### Accepted residuals

- `copy_path` recover variant B (a forged engine-shaped journal can still delete
  an **in-repo** file; same trust domain as writing the repo — see
  `tools/MCP.md` and `core/src/edit.rs`).
- Reorg `group=single` re-declares the shared namespace wrapper and does not
  compile (the frozen oracle emits the same layout).
- Default `reformat` is not graph-byte-identical (`qualify:false` is).
- The SPS bake compiles but has never been executed.
