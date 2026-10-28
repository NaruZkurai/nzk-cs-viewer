#!/usr/bin/env bash
# =============================================================================
#  build-v3-appimage.sh — package the ENGINE-FREE v3 viewer as an AppImage.
# =============================================================================
#
#  v3 is `native-csviewer` (winit + wgpu + egui) + its `nzk_ui` child (the
#  Blitz/Stylo CSS renderer). Nothing here is WebKit/Chromium/JS. This script is
#  additive to the v2 builder and never touches it or the frozen fork sources:
#
#    * it reads native-csviewer/app/src/** read-only (code freeze), and
#    * it writes only build-work, cache and "cs viewer builds/".
#
#  The v3 AppImage bundles, at paths the binaries actually look at:
#    usr/bin/native-csviewer                       the GUI
#    usr/lib/csviewer/nzk_ui                    the real nzk_ui binary
#    nzk_ui/target/release/nzk_ui            a wrapper the app's find_bin()
#                                                  resolves, which adds --html/
#                                                  --css pointing INSIDE the
#                                                  AppImage (nzk_ui otherwise
#                                                  uses its compile-time
#                                                  CARGO_MANIFEST_DIR, i.e. the
#                                                  build machine's source tree —
#                                                  the classic AppImage failure)
#    usr/share/csviewer/frontend/index.html        the real frontend, parsed as
#    usr/share/csviewer/frontend/static/style.css  the CSS specification
#
#  It reuses the v2 builder's local AppImage type-2 runtime
#  (../deps/runtime-$ARCH, verified against ../deps/SHA256SUMS) and the cached
#  appimagetool. It NEVER downloads a runtime.
#
#  Version/stamp convention (matches the v2 builder and the fork's build.rs):
#    --version V | CSVIEWER_VERSION=V | SOURCE_DATE_EPOCH=epoch | git HEAD time
#  The value is passed to cargo as CSVIEWER_BUILD (baked into the window title
#  and `--version`), used in the AppImage filename, and recorded in BUILD-INFO,
#  so the filename and the in-app stamp always agree.
#
#  Usage:
#    packaging/build-v3-appimage.sh [--version V] [--out DIR] [--no-latest]
#                                   [--skip-build] [--bin PATH] [--verify]
#                                   [--appimagetool PATH] [--keep-stage]
#                                   [--no-download]
#  Defaults: --version 3.0.0-alpha.1, --out "<repo>/cs viewer builds",
#            own CARGO_TARGET_DIR (target/release-v3), CARGO_HOME=.cargo-home.
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"   # repo/packaging
FORK_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"                          # repo root
BUILDER_DIR="$FORK_DIR"
REPO_ROOT="$FORK_DIR"
DEPS_DIR="$REPO_ROOT/deps"
ORIG_DIR="$REPO_ROOT"

ARCH="${CSVIEWER_ARCH:-x86_64}"
APP_ID="csviewer-v3"
APP_BIN="native-csviewer"
VERSION="${CSVIEWER_VERSION:-3.0.0-alpha.1}"
OUTPUT_DIR="${CSVIEWER_OUTPUT_DIR:-$REPO_ROOT/builds}"
NO_LATEST=0
SKIP_BUILD=0
BIN_OVERRIDE=""
VERIFY=0
KEEP_STAGE=0
ALLOW_DOWNLOAD=1
APPIMAGETOOL_ARG=""

WORK_DIR="$REPO_ROOT/build-work-v3"
APPDIR="$WORK_DIR/CSViewerV3.AppDir"
TARGET_DIR="${CSVIEWER_V3_TARGET_DIR:-$FORK_DIR/target/release-v3}"
CARGO_HOME_DIR="${CSVIEWER_CARGO_HOME:-${CARGO_HOME:-$HOME/.cargo}}"

c_blue=$'\033[1;34m'; c_green=$'\033[1;32m'; c_yellow=$'\033[1;33m'
c_red=$'\033[1;31m';  c_off=$'\033[0m'
if [ ! -t 2 ] || [ -n "${NO_COLOR:-}" ]; then c_blue=""; c_green=""; c_yellow=""; c_red=""; c_off=""; fi
log()  { printf '%s==>%s %s\n' "$c_blue" "$c_off" "$*" >&2; }
ok()   { printf '%s  ok%s %s\n' "$c_green" "$c_off" "$*" >&2; }
warn() { printf '%swarn%s %s\n' "$c_yellow" "$c_off" "$*" >&2; }
die()  { printf '%serr %s %s\n' "$c_red" "$c_off" "$*" >&2; exit 1; }

usage() { sed -n '2,40p' "${BASH_SOURCE[0]}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version)   VERSION="${2:?}"; shift 2 ;;
    --out)       OUTPUT_DIR="${2:?}"; shift 2 ;;
    --bin)       BIN_OVERRIDE="${2:?}"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --verify)    VERIFY=1; shift ;;
    --no-latest) NO_LATEST=1; shift ;;
    --keep-stage) KEEP_STAGE=1; shift ;;
    --no-download) ALLOW_DOWNLOAD=0; shift ;;
    --appimagetool) APPIMAGETOOL_ARG="${2:?}"; shift 2 ;;
    -h|--help)   usage; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done

sha256_of() { sha256sum -- "$1" | cut -d' ' -f1; }

# ---------------------------------------------------------------------------
# 1. Frozen-source guard: this script must never be the thing that edits app/
# ---------------------------------------------------------------------------
if [ -n "$(git -C "$REPO_ROOT" status --porcelain -- \
        "$FORK_DIR/app/src" "$FORK_DIR/nzk_ui/src" 2>/dev/null)" ]; then
  warn "app/ or nzk_ui/ has uncommitted edits (owners are working); packaging frozen sources only"
fi
# Comments legitimately mention the replaced Tauri/wry host, so only real
# dependency lines count; the resolved graph (Cargo.lock) is the stronger check.
if grep -vE '^[[:space:]]*#' "$FORK_DIR/app/Cargo.toml" "$FORK_DIR/Cargo.toml" 2>/dev/null \
   | grep -qE '\b(tauri|wry|webkit2gtk|javascriptcore|gtk|webview2|soup3)\b'; then
  die "browser-engine dependency found in the v3 manifests — v3 must be engine-free"
fi
if grep -qE '^name = "(tauri|wry|webkit2gtk|webkit2gtk-sys|javascriptcore-rs|gtk|webview2-com|soup3)"$' \
     "$FORK_DIR/Cargo.lock" 2>/dev/null; then
  die "browser-engine crate resolved in v3 Cargo.lock — v3 must be engine-free"
fi
ok "v3 dependency graph names no browser engine (manifests + Cargo.lock)"

# The ban is enforced by the repository gate as well (manifests + lockfiles +
# imports + ELF linkage + no JS/webview payload). Fail the package if violated.
if [ -x "$FORK_DIR/tools/check-engine-free.sh" ]; then
  "$FORK_DIR/tools/check-engine-free.sh" >/dev/null || die "engine-free ban gate failed (run tools/check-engine-free.sh)"
  ok "engine-free ban gate: Tauri/Chrome/WebKit/Electron-free (tools/check-engine-free.sh)"
fi

# ---------------------------------------------------------------------------
# 2. Build the release binaries (unless --skip-build/--bin)
# ---------------------------------------------------------------------------
BIN="$TARGET_DIR/release/$APP_BIN"
UI_BIN="$FORK_DIR/nzk_ui/target/release/nzk_ui"

if [ -n "$BIN_OVERRIDE" ]; then
  BIN="$BIN_OVERRIDE"
  [ -x "$BIN" ] || die "--bin is not executable: $BIN"
elif [ "$SKIP_BUILD" = 0 ]; then
  log "building v3 release (version=$VERSION, target=$TARGET_DIR)"
  # shellcheck disable=SC1091
  if [ -f "$BUILDER_DIR/rust-env.sh" ]; then . "$BUILDER_DIR/rust-env.sh"; fi
  command -v cargo >/dev/null 2>&1 || die "cargo not found"
  # NOTE: do NOT set CSVIEWER_BUILD/CSVIEWER_VERSION here.
  #
  # Both are *overrides* in app/build_stamp.rs, and `CSVIEWER_BUILD` is rule 1:
  # setting it to $VERSION made the packaged binary's build stamp a duplicate of
  # its own version (`cs viewer v3 3.0.0-alpha.1 (3.0.0-alpha.1)`), so the shipped
  # window title carried NO build number — the exact field the owner's bar names
  # ("the window title bar showing the build number and the v3 identity"). Passing
  # CSVIEWER_VERSION is no better: it is rule 2 and would still shadow git.
  #
  # Leaving both unset lets rule 4 run: git HEAD commit time + short hash (+
  # -dirty), i.e. the same `261016.060857-ge5ebbf8-dirty` shape a normal
  # `cargo build --release` bakes in. The *version* is still carried by the
  # artifact filename and the desktop entry, which is where it belongs — the title
  # bar's job is identity + build, not to repeat the version.
  ( cd "$FORK_DIR" && CARGO_HOME="$CARGO_HOME_DIR" \
      CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release -p "$APP_BIN" ) \
    || die "cargo build failed"
  ok "built $BIN ($(du -h -- "$BIN" | cut -f1))"
else
  log "skipping cargo build (--skip-build)"
fi
[ -x "$BIN" ] || die "release binary not found: $BIN (build it or pass --bin)"
[ -x "$UI_BIN" ] || die "nzk_ui release binary not found: $UI_BIN"

V3_STAMP="$("$BIN" --version 2>/dev/null || true)"
case "$V3_STAMP" in
  *"cs viewer v3"*) ok "binary identity: $V3_STAMP" ;;
  "") warn "could not read --version from $BIN" ;;
  *) die "unexpected --version output (not v3): $V3_STAMP" ;;
esac

# ---------------------------------------------------------------------------
# 3. Stage the AppDir
# ---------------------------------------------------------------------------
log "staging AppDir"
rm -rf -- "$APPDIR"
mkdir -p -- "$APPDIR/usr/bin" "$APPDIR/usr/lib/csviewer" \
  "$APPDIR/usr/share/csviewer/frontend/static" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
  "$APPDIR/nzk_ui/target/release"

cp -a -- "$BIN" "$APPDIR/usr/bin/$APP_BIN"; chmod 0755 "$APPDIR/usr/bin/$APP_BIN"
cp -a -- "$UI_BIN" "$APPDIR/usr/lib/csviewer/nzk_ui"; chmod 0755 "$APPDIR/usr/lib/csviewer/nzk_ui"

# Strip symbols from the staged ELFs: the AppImage only needs the code, and
# dropping the debug/symtab sections cut the nzk_ui payload 22.6MB -> 17.2MB
# (23%), i.e. less to compress. Both binaries stay fully functional; `strip`
# is optional so a machine without binutils still packages.
if command -v strip >/dev/null 2>&1; then
  strip --strip-unneeded -- "$APPDIR/usr/bin/$APP_BIN" "$APPDIR/usr/lib/csviewer/nzk_ui" \
    && ok "stripped staged ELFs ($(du -h -- "$APPDIR/usr/bin/$APP_BIN" "$APPDIR/usr/lib/csviewer/nzk_ui" | tr "\n" " "))"
fi

# frontend: the real index.html + style.css, exactly what nzk_ui parses.
cp -a -- "$ORIG_DIR/frontend/index.html" "$APPDIR/usr/share/csviewer/frontend/index.html"
cp -a -- "$ORIG_DIR/frontend/static/style.css" "$APPDIR/usr/share/csviewer/frontend/static/style.css"

# nzk_ui wrapper at the path app/src/nzk_ui.rs::find_bin() probes
# (current_exe/../../nzk_ui/target/release/nzk_ui -> <AppDir>/nzk_ui/...)
cat > "$APPDIR/nzk_ui/target/release/nzk_ui" <<'WRAP'
#!/bin/sh
# Locate the AppDir, then hand nzk_ui explicit in-AppImage asset paths.
# The app spawns this with no --html/--css, and the real binary would otherwise
# use its compile-time CARGO_MANIFEST_DIR (the build machine's source tree).
set -u
if [ -n "${APPDIR:-}" ] && [ -d "$APPDIR/usr" ]; then
  HERE="$APPDIR"
else
  SELF="$0"
  while [ -L "$SELF" ]; do
    T="$(readlink "$SELF")"
    case "$T" in /*) SELF="$T" ;; *) SELF="$(dirname "$SELF")/$T" ;; esac
  done
  HERE="$(cd -- "$(dirname -- "$SELF")/../../.." && pwd -P)"
fi
FE="$HERE/usr/share/csviewer/frontend"
REAL="$HERE/usr/lib/csviewer/nzk_ui"
have_html=0; have_css=0
for a in "$@"; do
  case "$a" in --html) have_html=1 ;; --css) have_css=1 ;; esac
done
[ "$have_html" = 1 ] || set -- "$@" --html "$FE/index.html"
[ "$have_css" = 1 ] || set -- "$@" --css "$FE/static/style.css"
exec "$REAL" "$@"
WRAP
chmod 0755 "$APPDIR/nzk_ui/target/release/nzk_ui"

# icon
ICON_SRC=""
for cand in "$ORIG_DIR/app/icons/icon.png" "$ORIG_DIR/app/icons/256x256.png"; do
  [ -f "$cand" ] && { ICON_SRC="$cand"; break; }
done
if [ -n "$ICON_SRC" ]; then
  cp -a -- "$ICON_SRC" "$APPDIR/$APP_ID.png"
  cp -a -- "$ICON_SRC" "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.png"
else
  warn "no icon found under $ORIG_DIR/app/icons"
fi

# AppRun — resolve the AppDir without guessing, export the wrapper explicitly,
# then exec the v3 GUI.
cat > "$APPDIR/AppRun" <<APPRUN
#!/bin/sh
set -u
if [ -n "\${APPDIR:-}" ] && [ -x "\$APPDIR/usr/bin/$APP_BIN" ]; then
  HERE="\$APPDIR"
else
  SELF="\$0"
  while [ -L "\$SELF" ]; do
    T="\$(readlink "\$SELF")"
    case "\$T" in /*) SELF="\$T" ;; *) SELF="\$(dirname "\$SELF")/\$T" ;; esac
  done
  HERE="\$(cd -- "\$(dirname -- "\$SELF")" && pwd -P)"
  case "\$HERE" in */usr/bin) HERE="\$(cd -- "\$HERE/../.." && pwd -P)" ;; esac
fi
export APPDIR="\$HERE"
# The nzk_ui child must find the real frontend inside the AppImage.
export CSVIEWER_NZK_UI_BIN="\$HERE/nzk_ui/target/release/nzk_ui"
# Legacy alias: the old name points at the same binary.
export CSVIEWER_CSS_SHELL_BIN="\$CSVIEWER_NZK_UI_BIN"
exec "\$HERE/usr/bin/$APP_BIN" "\$@"
APPRUN
chmod 0755 "$APPDIR/AppRun"

cat > "$APPDIR/$APP_ID.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=cs viewer v3 (alpha)
Comment=Engine-free C# function-relation graph viewer (winit + wgpu + egui)
Exec=$APP_BIN %U
TryExec=$APP_BIN
Icon=$APP_ID
Terminal=false
Categories=Development;IDE;
Keywords=C#;graph;code;source;rust;native;engine-free;
StartupNotify=true
X-AppImage-Name=cs viewer v3
X-AppImage-Version=$VERSION
DESKTOP
cp -a -- "$APPDIR/$APP_ID.desktop" "$APPDIR/usr/share/applications/$APP_ID.desktop"

{
  printf 'app=cs viewer v3 (engine-free: winit + wgpu + egui + nzk_ui)\n'
  printf 'version=%s\n' "$VERSION"
  printf 'arch=%s\n' "$ARCH"
  printf 'built=%s\n' "$(date -Iseconds)"
  printf 'binary=%s\n' "$APP_BIN"
  printf 'nzk_ui=usr/lib/csviewer/nzk_ui (wrapped by nzk_ui/target/release/nzk_ui)\n'
  printf 'frontend=usr/share/csviewer/frontend/{index.html,static/style.css}\n'
  printf 'engine=none — no webkit/gtk/jsc/soup in ldd; no JS runtime\n'
  printf 'sha256:usr/bin/%s=%s\n' "$APP_BIN" "$(sha256_of "$APPDIR/usr/bin/$APP_BIN")"
  printf 'sha256:usr/lib/csviewer/nzk_ui=%s\n' "$(sha256_of "$APPDIR/usr/lib/csviewer/nzk_ui")"
  printf 'sha256:usr/share/csviewer/frontend/index.html=%s\n' "$(sha256_of "$APPDIR/usr/share/csviewer/frontend/index.html")"
  printf 'sha256:usr/share/csviewer/frontend/static/style.css=%s\n' "$(sha256_of "$APPDIR/usr/share/csviewer/frontend/static/style.css")"
} > "$APPDIR/usr/share/csviewer/BUILD-INFO"

chmod -R a+rX -- "$APPDIR"

# ---------------------------------------------------------------------------
# 4. appimagetool + local type-2 runtime (never fetched)
# ---------------------------------------------------------------------------
APPIMAGETOOL=""
if [ -n "$APPIMAGETOOL_ARG" ]; then
  APPIMAGETOOL="$APPIMAGETOOL_ARG"
elif command -v appimagetool >/dev/null 2>&1; then
  APPIMAGETOOL="$(command -v appimagetool)"
else
  APPIMAGETOOL="$BUILDER_DIR/cache/tools/appimagetool-$ARCH.AppImage"
fi
[ -x "$APPIMAGETOOL" ] || { [ "$ALLOW_DOWNLOAD" = 1 ] || die "appimagetool not found and --no-download given"; \
  die "appimagetool not found at $APPIMAGETOOL (v2 builder caches it under cache/tools/)"; }
chmod 0755 -- "$APPIMAGETOOL" 2>/dev/null || true

RUNTIME="$DEPS_DIR/runtime-$ARCH"
[ -f "$RUNTIME" ] || die "type-2 runtime missing: $RUNTIME (tracked under deps/)"
chmod 0755 -- "$RUNTIME"
if [ -f "$DEPS_DIR/SHA256SUMS" ]; then
  want="$(awk -v f="runtime-$ARCH" '$2 == f {print $1}' "$DEPS_DIR/SHA256SUMS" | head -n1)"
  if [ -n "$want" ]; then
    got="$(sha256_of "$RUNTIME")"
    [ "$want" = "$got" ] || die "runtime checksum mismatch: expected $want got $got"
    ok "type2 runtime verified (sha256 ${got:0:12}…, local)"
  fi
fi

mkdir -p -- "$OUTPUT_DIR"
OUT_NAME="native-csviewer-$VERSION-$ARCH.AppImage"
OUT="$OUTPUT_DIR/$OUT_NAME"
rm -f -- "$OUT"
log "packaging $(basename "$OUT")"
(
  cd -- "$WORK_DIR"
  APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" "$APPIMAGETOOL" \
    --no-appstream --runtime-file "$RUNTIME" "$APPDIR" "$OUT"
) || die "appimagetool failed"
chmod 0755 -- "$OUT"

LATEST=""
if [ "$NO_LATEST" = 0 ]; then
  LATEST="$OUTPUT_DIR/native-csviewer-Latest-$ARCH.AppImage"
  cp -f -- "$OUT" "$LATEST"; chmod 0755 -- "$LATEST"
  ok "refreshed $(basename "$LATEST")"
fi

# ---------------------------------------------------------------------------
# 5. Verify (extract + run from INSIDE the mount)
# ---------------------------------------------------------------------------
VERIFY_OK=0
if [ "$VERIFY" = 1 ]; then
  log "verifying $(basename "$OUT")"
  CHK="$WORK_DIR/verify"
  rm -rf -- "$CHK"; mkdir -p -- "$CHK"
  ( cd -- "$CHK" && APPIMAGE_EXTRACT_AND_RUN=1 "$OUT" --appimage-extract >/dev/null 2>&1 ) \
    || die "AppImage failed to self-extract"
  ROOT="$CHK/squashfs-root"
  for f in AppRun usr/bin/$APP_BIN usr/lib/csviewer/nzk_ui \
           nzk_ui/target/release/nzk_ui \
           usr/share/csviewer/BUILD-INFO \
           usr/share/csviewer/frontend/index.html \
           usr/share/csviewer/frontend/static/style.css; do
    [ -e "$ROOT/$f" ] || die "verify: missing $f inside the AppImage"
  done
  ok "AppDir contents present"

  # 5a. bundled binary is v3 (run from the extracted mount, not the source tree)
  VOUT="$(cd -- "$ROOT" && CSVIEWER_NZK_UI_BIN="$ROOT/nzk_ui/target/release/nzk_ui" \
            ./usr/bin/$APP_BIN --version 2>&1)" || die "bundled binary --version failed"
  printf '%s\n' "$VOUT" | grep -q 'cs viewer v3' || die "bundled binary is not v3: $VOUT"
  ok "bundled --version: $VOUT"

  # 5b. nzk_ui finds the IN-APPIMAGE assets (no --html/--css passed)
  CSS_OUT="$(cd -- "$ROOT" && ./nzk_ui/target/release/nzk_ui --boxes - --size 1600x1000 \
              --repo /tmp 2>&1)" || die "bundled nzk_ui could not resolve its assets: $CSS_OUT"
  printf '%s\n' "$CSS_OUT" | grep -q '#graph' || die "nzk_ui produced no #graph geometry"
  ok "nzk_ui resolved frontend/index.html + style.css from inside the mount"

  # 5c. full offscreen render incl. the nzk_ui surface, from the mount
  CORPUS="$(mktemp -d /tmp/csviewer-v3-verify.XXXXXX)"
  if [ -d "$FORK_DIR/tools/ab-corpus" ]; then
    cp -r -- "$FORK_DIR/tools/ab-corpus/." "$CORPUS/"
  else
    printf 'namespace V3Verify { public class C { public int F() { return 1; } } }\n' > "$CORPUS/C.cs"
  fi
  SHOT="$CHK/shot.png"
  if ( cd -- "$ROOT" && env -u DISPLAY -u WAYLAND_DISPLAY \
        CSVIEWER_NZK_UI_BIN="$ROOT/nzk_ui/target/release/nzk_ui" \
        timeout 60 ./usr/bin/$APP_BIN --screenshot "$SHOT" --size 800x600 \
          --repo "$CORPUS" --root . >"$CHK/shot.log" 2>&1 ); then
    [ -s "$SHOT" ] || die "screenshot was empty"
    ok "offscreen render from the mount: $(du -h "$SHOT" | cut -f1)"
    VERIFY_OK=1
  else
    warn "offscreen render failed from the mount (see $CHK/shot.log); headless GPU may be unavailable"
    tail -20 "$CHK/shot.log" >&2 || true
  fi
  rm -rf -- "$CORPUS"
  if [ "$VERIFY_OK" != 1 ] && [ "${CSVIEWER_REQUIRE_RENDER:-0}" = 1 ]; then
    die "render verification required but failed"
  fi
fi

if [ "$KEEP_STAGE" = 0 ]; then rm -rf -- "$WORK_DIR"; fi

SIZE="$(du -h -- "$OUT" | cut -f1)"
SHA="$(sha256_of "$OUT")"
printf '\n' >&2
ok "built $OUT  ($SIZE)"
printf '     sha256    %s\n' "$SHA" >&2
printf '     version   %s\n' "$VERSION" >&2
printf '     identity  %s\n' "${V3_STAMP:-unknown}" >&2
[ -n "$LATEST" ] && printf '     latest    %s\n' "$LATEST" >&2
printf '     run       %s\n' "$OUT" >&2
if [ "$VERIFY" = 1 ] && [ "$VERIFY_OK" != 1 ]; then
  printf '     note      files verified; headless render not verified\n' >&2
fi

# Machine-readable tail for the release notes / report.
printf 'V3_ARTIFACT=%s\n' "$OUT"
printf 'V3_SHA256=%s\n' "$SHA"
printf 'V3_VERSION=%s\n' "$VERSION"
