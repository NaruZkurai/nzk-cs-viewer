# cs viewer v3 — native, engine-free

`cs viewer v3` is the native C# relation-graph viewer/IDE. It opens a window
with **winit + wgpu + egui** and paints the chrome from the real
`frontend/index.html` + `frontend/static/style.css` using Blitz/Stylo/Taffy/
Parley/Vello-CPU through the `nzk_ui` rasteriser.

There is **no browser engine anywhere**: no Tauri, wry, WebKitGTK, GTK,
WebView2, JavaScriptCore, Electron, or JS runtime. The graph engine
(`core/csgraph-core`) is pure Rust.

> **Status: 3.0.0-beta.9.** v3 is not "finished" merely because it is named
> v3. The CSS shell paints the chrome while native widgets still own part of
> the behaviour; controls without behaviour are drawn but inert rather than
> faked. Treat this as a beta.

## Layout

```
Cargo.toml                  workspace: app/ + core/
app/                        winit + wgpu + egui host
  src/nzk_ui.rs             runs nzk_ui as a child, uploads its pixels, routes hits
  src/graphviz.rs           exact FA2 + Noveria graph layout
  src/windows.rs            attach/snap/detach window geometry
core/                       csgraph engine: lexer, parser, graph, doc, reorg, cache, MCP
nzk_ui/                     separate workspace: HTML/CSS -> RGBA + resolved boxes
frontend/                   the real index.html + static/style.css
packaging/                  AppImage builder
```

The three IDE addons are **git dependencies pinned by commit** in
`app/Cargo.toml`:

| crate | repository | pinned commit |
|---|---|---|
| `nzkide-module` | `NaruZkurai/nzk-cs-nzkide_module` | `e4f104f` |
| `scratch-rs-ide-addon` | `NaruZkurai/nzk-csViewer-scratch_rs_ide_addon` | `d5491fe` |
| `def2mcpfnnumber` | `NaruZkurai/nzk-cs-def2mcpfnnumber_rs_ide_addon` | `a063dfc` |

`nzk_ui/` is a second workspace with its own lockfile so its Blitz/Vello
dependency set never leaks into the host.

## Build

Requirements: a Rust toolchain with edition-2024 support, a C linker, and the
usual Linux windowing/GPU development libraries for winit/wgpu (X11 or Wayland,
Vulkan or GL). `nzk_ui` additionally needs the system font setup used by
Parley/fontique. The first build fetches the three pinned addon commits from
GitHub.

```bash
# host (puts the binary at target/release/native-csviewer)
cargo build --release

# the CSS shell child, beside the host as the app expects
( cd nzk_ui && cargo build --release )
```

The host looks for `nzk_ui/target/{release,debug}/nzk_ui` relative to its own
executable (and the current directory), so the layout above works without any
environment variable. Override with `CSVIEWER_NZK_UI_BIN` if you install the
two binaries elsewhere. `CSVIEWER_NO_NZK_UI=1` falls back to the legacy
hand-ported egui UI.

## Run

```bash
./target/release/native-csviewer --repo /path/to/cs/repo
./target/release/native-csviewer --repo /path/to/cs/repo --root src/cs
./target/release/native-csviewer --version
./target/release/native-csviewer --modules       # what was linked
```

Headless / offscreen:

```bash
./target/release/native-csviewer --screenshot graph.png --size 1600x1000 \
  --repo /path/to/cs/repo --root src/cs
```

## Test

```bash
cargo test --workspace
( cd nzk_ui && cargo test )
```

The in-tree unit tests are self-contained. The nzk_ui <-> host wire test lives
in the optional testing repo below, alongside the fixture corpora. A few host tests exercise a real
project fixture corpus; when that optional fixture is not present they print a
skip line and pass.

The heavier smoke/screenshot/parity harness is an **optional, development-only
companion repository**, not a build dependency:
[nzk-cs-viewer-testing-files-only](https://github.com/NaruZkurai/nzk-cs-viewer-testing-files-only).
Clone it beside this checkout and run its `./run-all.sh` if you want those
gates; the viewer builds and tests fine without it.

## Packaging

```bash
( cd nzk_ui && cargo build --release )
./packaging/build-v3-appimage.sh --version 3.0.0-beta.9 --verify
```

The AppImage needs `appimagetool` and a type-2 runtime at
`deps/runtime-x86_64` (not committed; see `packaging/build-v3-appimage.sh`).
It bundles the host, `nzk_ui`, and the two frontend files it reads.

## Archived v1/v2

The previous Tauri/WebKitGTK/Monaco tree (v2) is preserved in git history.
The v1/v2 source is deliberately not part of the v3 tree.
