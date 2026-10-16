#!/usr/bin/env node
/* probe_sidebar.mjs — reproduce + prove the sidebar's visual scaling.
 *
 * Boots the real engine + frontend with `tools/with_serve.sh` (which wires
 * --url/--cdp-port), drives the page in headless Chromium over CDP, and at a
 * set of window widths measures the right-hand `<aside class="side">`:
 *
 *   - how the `.layout` grid divides into canvas / sidebar (`--side-w`)
 *   - whether the sidebar stays inside the viewport and leaves the canvas a
 *     usable width (a fixed `--side-w` wider than the window crushes the 1fr
 *     canvas track to 0 and pushes the sidebar off-screen)
 *   - whether the sidebar's content is reachable: either it fits, or it
 *     scrolls. A flex column with `overflow-y:auto` whose children can shrink
 *     clips content instead of scrolling
 *   - the saved width (`csgraph.panels.v1.sideW`) restore path, which used to
 *     come back unclamped after the window got smaller
 *   - the vertical splitter drag: `--side-w` must follow the pointer and stay
 *     clamped so the canvas is never crushed
 *
 * Each width gets a full-viewport screenshot plus a sidebar-clipped one under
 * the ignored `<builder>/target/sidebar-probe/` scratch dir (never tracked).
 *
 * Usage:
 *   tools/with_serve.sh -- node tools/probe_sidebar.mjs [--label before]
 *   node tools/probe_sidebar.mjs --url http://127.0.0.1:8795/ --cdp-port 9341
 *
 * Exit status 0 only when every check passes.
 */
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';

function arg(name, dflt) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
}
const URL_ = arg('--url', 'http://127.0.0.1:8795/');
const PORT = Number(arg('--cdp-port', '9341'));
const CHROME = arg('--chrome', 'chromium');
const TIMEOUT_MS = Number(arg('--timeout', '180000'));
const LABEL = arg('--label', 'run');
const WIDTHS = arg('--widths', '1920,1440,1100,980,760')
  .split(',').map((s) => Number(s.trim())).filter((n) => n > 0);
const HEIGHT = Number(arg('--height', '900'));
const HERE = dirname(fileURLToPath(import.meta.url));
// `../target` is the builder's shared cargo target dir — ignored by git.
const OUT = resolve(arg('--out', join(HERE, '..', '..', 'target', 'sidebar-probe')));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitForDevtools(port, deadline) {
  let last;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/json/version`);
      if (r.ok) return await r.json();
      last = `HTTP ${r.status}`;
    } catch (e) { last = e.message; }
    await sleep(120);
  }
  throw new Error(`devtools never came up on ${port}: ${last}`);
}

class CDP {
  constructor(ws) {
    this.ws = ws; this.id = 0; this.pending = new Map(); this.handlers = new Map();
    ws.addEventListener('message', (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
      } else if (msg.method) {
        const hs = this.handlers.get(msg.method);
        if (hs) hs.forEach((h) => h(msg.params));
      }
    });
  }
  on(method, fn) {
    if (!this.handlers.has(method)) this.handlers.set(method, []);
    this.handlers.get(method).push(fn);
  }
  send(method, params = {}) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }
  async eval(expression) {
    const r = await this.send('Runtime.evaluate', {
      expression, awaitPromise: true, returnByValue: true,
      allowUnsafeEvalBlockedByCSP: true
    });
    if (r.exceptionDetails) {
      throw new Error('eval threw: ' + (r.exceptionDetails.exception?.description ||
        r.exceptionDetails.text));
    }
    return r.result.value;
  }
}

const results = [];
function check(name, ok, detail) {
  results.push({ name, ok: !!ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);
}

/** Measurements taken in the page, once per width. */
const MEASURE = `(() => {
  const p = (v) => Math.round(v * 100) / 100;
  const box = (n) => { const b = n.getBoundingClientRect();
    return { x: p(b.left), y: p(b.top), w: p(b.width), h: p(b.height),
             r: p(b.right), b: p(b.bottom), name: n.id || n.className || n.tagName }; };
  const layout = document.querySelector('.layout');
  const side = document.querySelector('.side');
  const canvas = document.querySelector('#canvasWrap');
  if (!layout || !side || !canvas) return { error: 'missing .layout/.side/#canvasWrap' };
  const cs = getComputedStyle(side);
  const html = getComputedStyle(document.documentElement);
  const lr = box(layout), sr = box(side), cr = box(canvas);
  const sections = [...side.querySelectorAll(':scope > .section')].map((s) => {
    const b = s.getBoundingClientRect();
    return { id: s.id || '(unnamed)', top: p(b.top), bottom: p(b.bottom), h: p(b.height),
             scrollH: s.scrollHeight, clientH: s.clientHeight,
             squeezedInside: s.scrollHeight > s.clientHeight + 1,
             overflowsSide: b.bottom > sr.b + 0.5 };
  });
  const contentBottom = Math.max(sr.y, ...sections.map((s) => s.bottom));
  const scrollRange = p(side.scrollHeight - side.clientHeight);
  return {
    viewport: { w: innerWidth, h: innerHeight },
    narrow: matchMedia('(max-width: 980px)').matches,
    sideW: html.getPropertyValue('--side-w').trim(),
    layout: lr, side: sr, canvas: cr,
    gridColumns: getComputedStyle(layout).gridTemplateColumns,
    gridRows: getComputedStyle(layout).gridTemplateRows,
    sideOverflowY: cs.overflowY, sideMaxHeight: cs.maxHeight,
    sideDisplay: cs.display, sideFlexDirection: cs.flexDirection,
    sideScrollH: side.scrollHeight, sideClientH: side.clientHeight, scrollRange,
    contentBottom,
    // Clipped = content hangs below the sidebar and the sidebar cannot scroll.
    contentClipped: contentBottom > sr.b + 0.5 && scrollRange <= 1,
    // Squeezed = a flex child is shorter than its own content.
    squeezedSections: sections.filter((s) => s.squeezedInside).map((s) => s.id),
    canvasSqueezed: cr.w < 240,
    sideBeyondViewport: sr.r > innerWidth + 0.5,
    sections
  };
})()`;

const resultsByWidth = [];

async function setViewport(cdp, width, height) {
  await cdp.send('Emulation.setDeviceMetricsOverride',
    { width, height, deviceScaleFactor: 1, mobile: false });
  // Two rAFs so the grid re-lays-out and the splitter position watcher runs.
  await cdp.eval('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))');
  await sleep(120);
}

async function shot(cdp, path, clip) {
  const params = { format: 'png' };
  if (clip) params.clip = { ...clip, scale: 1 };
  const r = await cdp.send('Page.captureScreenshot', params);
  writeFileSync(path, Buffer.from(r.data, 'base64'));
  return path;
}

async function capture(cdp, tag, m) {
  const full = join(OUT, `sidebar-${LABEL}-${tag}-full.png`);
  await shot(cdp, full);
  const clipW = Math.max(1, Math.min(m.side.w, m.viewport.w - m.side.x, m.viewport.w));
  const clipH = Math.max(1, Math.min(m.side.h, m.viewport.h - m.side.y, m.viewport.h));
  const clip = join(OUT, `sidebar-${LABEL}-${tag}-clip.png`);
  await shot(cdp, clip, { x: Math.max(0, m.side.x), y: Math.max(0, m.side.y),
                          width: clipW, height: clipH });
  m.shots = { full, clip };
  return m;
}

async function measureAt(cdp, width) {
  await setViewport(cdp, width, HEIGHT);
  const m = await cdp.eval(MEASURE);
  if (m.error) throw new Error(`measure@${width}: ${m.error}`);
  m.width = width;
  // Full viewport, then the sidebar's own box (clamped to the viewport so a
  // sidebar pushed off-screen still yields an image).
  return capture(cdp, String(width), m);
}

async function main() {
  mkdirSync(OUT, { recursive: true });
  const profile = mkdtempSync(join(tmpdir(), 'csviewer-sidebar-'));
  const chrome = spawn(CHROME, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check', '--disable-extensions',
    '--hide-scrollbars=false',
    `--user-data-dir=${profile}`, `--remote-debugging-port=${PORT}`, 'about:blank'
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  let chromeErr = '';
  chrome.stderr.on('data', (d) => { chromeErr += d.toString(); });

  const deadline = Date.now() + TIMEOUT_MS;
  let ws, cdp, targetId;
  const consoleErrors = [];
  try {
    const ver = await waitForDevtools(PORT, deadline);
    const create = await fetch(`http://127.0.0.1:${PORT}/json/new?${encodeURIComponent('about:blank')}`,
      { method: 'PUT' });
    if (!create.ok) throw new Error('could not open a target: HTTP ' + create.status);
    const target = await create.json();
    targetId = target.id;
    ws = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((res, rej) => {
      ws.addEventListener('open', res, { once: true });
      ws.addEventListener('error', () => rej(new Error('cdp websocket error')), { once: true });
    });
    cdp = new CDP(ws);
    cdp.on('Runtime.exceptionThrown', (p) => {
      consoleErrors.push('uncaught: ' + (p.exceptionDetails?.exception?.description ||
        p.exceptionDetails?.text));
    });
    cdp.on('Runtime.consoleAPICalled', (p) => {
      if (p.type === 'error') {
        consoleErrors.push('console.error: ' +
          p.args.map((a) => a.value ?? a.description ?? '').join(' '));
      }
    });

    await cdp.send('Runtime.enable');
    await cdp.send('Page.enable');
    await cdp.send('Log.enable');

    const boot = async () => {
      const loaded = new Promise((res) => cdp.on('Page.loadEventFired', res));
      await cdp.send('Page.navigate', { url: URL_ });
      await Promise.race([loaded, sleep(30000)]);
      await cdp.eval(`(async () => {
        const t0 = Date.now();
        for (;;) {
          if (window.state && state.indexed && state.nodes && state.nodes.length &&
              document.getElementById('floatLayer') &&
              document.querySelector('.side')) return true;
          if (Date.now() - t0 > 90000) throw new Error('timeout waiting for boot');
          await new Promise(r => setTimeout(r, 80));
        }
      })()`);
    };

    // Seed the saved width BEFORE any document script runs, so the very first
    // (and only) page load exercises applySaved's restore path. A single page
    // load keeps the whole probe inside the 30s budget.
    await cdp.send('Page.addScriptToEvaluateOnNewDocument', {
      source: "try { localStorage.setItem('csgraph.panels.v1', " +
              "JSON.stringify({ sideW: 1400 })); } catch (e) {}"
    });
    await setViewport(cdp, WIDTHS[0], HEIGHT);
    await boot();

    // ------------------------------------------------- restore saved width
    // A width dragged out on a wide monitor, restored on a smaller window:
    // applySaved must clamp it to *this* viewport, not the one it was saved on.
    const big = await cdp.eval(MEASURE);
    await setViewport(cdp, 1100, HEIGHT);
    const shrunk = await capture(cdp, 'restore1100', await cdp.eval(MEASURE));
    resultsByWidth.push({ ...shrunk, width: 1100, restoredFrom: 1400 });
    console.log(`\n--- saved sideW=1400 restored, then window 1100 ---`);
    console.log(`  at 1920: --side-w=${big.sideW} canvas=${big.canvas.w} side=${big.side.w}`);
    console.log(`  at 1100: --side-w=${shrunk.sideW} canvas=${shrunk.canvas.w} ` +
                `side right=${shrunk.side.r} (viewport 1100) scrollRange=${shrunk.scrollRange}`);
    console.log(`  shots: ${shrunk.shots.full}  ${shrunk.shots.clip}`);
    check('saved sideW is clamped when the window shrinks',
      shrunk.side.r <= 1100 + 0.5 && !shrunk.canvasSqueezed,
      `side right=${shrunk.side.r}, canvas=${shrunk.canvas.w}px, --side-w=${shrunk.sideW}`);

    // ------------------------------------------------- splitter drag helper
    // The sidebar splitter is panels.js's, a direct child of `.layout`;
    // shell.js's document-split divider deliberately reuses the same class.
    await setViewport(cdp, 1440, HEIGHT);
    const spl = await cdp.eval(`(() => { const s = document.querySelector('.layout > .pane-splitter-v');
      if (!s || s.style.display === 'none') return null; const b = s.getBoundingClientRect();
      return { x: b.left + b.width / 2, y: b.top + Math.min(200, b.height / 2) }; })()`);
    if (!spl) check('vertical splitter exists', false, 'no .layout > .pane-splitter-v');
    const splitterAt = () => cdp.eval(`(() => { const s = document.querySelector('.layout > .pane-splitter-v');
      const b = s.getBoundingClientRect();
      return { x: b.left + b.width / 2, y: b.top + Math.min(200, b.height / 2) }; })()`);
    const drag = async (delta) => {
      const from = await splitterAt();
      const toX = Math.max(2, Math.min(1438, from.x + delta));
      await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: from.x, y: from.y,
        button: 'left', clickCount: 1, buttons: 1 });
      for (let i = 1; i <= 4; i++) {
        // Keep the final move exact so the width delta is not rounded off.
        const x = i === 4 ? toX : Math.round(from.x + ((toX - from.x) * i) / 4);
        await cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y: from.y,
          button: 'none', buttons: 1 });
      }
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: toX, y: from.y,
        button: 'left', buttons: 0 });
      await cdp.eval('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))');
      return cdp.eval(MEASURE);
    };
    if (spl) {
      // Reset the requested width from the restored 1400 to the 360px default:
      // 220 is a known clamp floor, +140 lands exactly on the CSS default.
      await drag(2000);
      await drag(-140);
    }

    // ---------------------------------------------------------- per width
    for (const w of WIDTHS) {
      const m = await measureAt(cdp, w);
      resultsByWidth.push(m);
      console.log(`\n--- width ${w}x${HEIGHT} ${LABEL} ---`);
      console.log(`  --side-w=${m.sideW}  grid-cols=${m.gridColumns}`);
      console.log(`  canvas ${m.canvas.w}x${m.canvas.h} @ x=${m.canvas.x}  ` +
                  `sidebar ${m.side.w}x${m.side.h} @ x=${m.side.x} (right=${m.side.r}, ` +
                  `viewport=${m.viewport.w})`);
      console.log(`  sidebar scroll: clientH=${m.sideClientH} scrollH=${m.sideScrollH} ` +
                  `range=${m.scrollRange} overflow-y=${m.sideOverflowY} max-height=${m.sideMaxHeight}`);
      console.log(`  content bottom=${m.contentBottom} (sidebar bottom=${m.side.b}) ` +
                  `clipped=${m.contentClipped} squeezed=[${m.squeezedSections.join(',')}]`);
      console.log(`  shots: ${m.shots.full}  ${m.shots.clip}`);
      check(`sidebar within viewport @${w}`, !m.sideBeyondViewport && m.side.r <= w + 0.5,
        `sidebar right=${m.side.r} viewport=${w}`);
      check(`canvas keeps >=240px @${w}`, !m.canvasSqueezed, `canvas=${m.canvas.w}px`);
      // Below 980px the grid is deliberately one column, so the sidebar spans
      // the viewport on purpose and `--side-w` no longer sizes it.
      check(`sidebar width == --side-w @${w}`,
        m.narrow || Math.abs(m.side.w - parseFloat(m.sideW)) <= 1.5,
        m.narrow ? `narrow layout: sidebar spans the viewport (${m.side.w}px)`
                 : `side=${m.side.w}px, --side-w=${m.sideW}`);
      check(`sidebar content reachable @${w}`,
        !m.contentClipped && m.squeezedSections.length === 0,
        m.contentClipped
          ? `content bottom ${m.contentBottom} > sidebar bottom ${m.side.b}, scrollRange=${m.scrollRange}`
          : (m.squeezedSections.length ? `squeezed sections: ${m.squeezedSections.join(',')}` : 'fits or scrolls'));
    }

    // ------------------------------------------------------- splitter drag
    if (spl) {
      // The per-width loop ended on the narrow layout; come back to a wide one.
      await setViewport(cdp, 1440, HEIGHT);
      const beforeDrag = await cdp.eval(MEASURE);
      console.log(`\n--- splitter drag at 1440 (start --side-w=${beforeDrag.sideW}, ` +
                  `canvas=${beforeDrag.canvas.w}) ---`);
      // Drag 120px left: the sidebar should widen by ~120px.
      const widened = await drag(-120);
      check('splitter drag changes --side-w',
        Math.abs(parseFloat(widened.sideW) - (parseFloat(beforeDrag.sideW) + 120)) <= 14 &&
        Math.abs(widened.side.w - parseFloat(widened.sideW)) <= 1.5,
        `${beforeDrag.sideW} -> ${widened.sideW} (side=${widened.side.w}px)`);
      // Drag far right: the sidebar must not shrink below MIN_SIDE (220).
      const min = await drag(2000);
      check('splitter drag clamps to a minimum side width',
        parseFloat(min.sideW) >= 219 && min.canvas.w >= 240,
        `--side-w=${min.sideW}, side=${min.side.w}px, canvas=${min.canvas.w}px`);
      // Drag far left: the canvas must keep a usable width.
      const max = await drag(-2000);
      check('splitter drag clamps so the canvas is not crushed',
        max.side.r <= 1440 + 0.5 && max.canvas.w >= 240,
        `--side-w=${max.sideW}, side right=${max.side.r}, canvas=${max.canvas.w}px`);
      // Smaller window with that wide width still saved: must adapt.
      await setViewport(cdp, 1000, HEIGHT);
      const small = await capture(cdp, 'dragwide1000', await cdp.eval(MEASURE));
      resultsByWidth.push({ ...small, width: 1000, afterDragWide: max.sideW });
      console.log(`\n--- wide dragged width (${max.sideW}) at window 1000 ---`);
      console.log(`  canvas=${small.canvas.w} side=${small.side.w} right=${small.side.r}`);
      console.log(`  shots: ${small.shots.full}  ${small.shots.clip}`);
      check('dragged width still fits a smaller window',
        small.side.r <= 1000 + 0.5 && !small.canvasSqueezed,
        `side right=${small.side.r}, canvas=${small.canvas.w}px`);
      // Widening again restores the width the user dragged, not the clamped one.
      await setViewport(cdp, 1440, HEIGHT);
      const regrown = await cdp.eval(MEASURE);
      check('requested width returns when the window widens again',
        Math.abs(parseFloat(regrown.sideW) - parseFloat(max.sideW)) <= 1.5,
        `--side-w=${regrown.sideW} (was ${max.sideW} before the shrink), ` +
        `canvas=${regrown.canvas.w}px`);
    }

    // -------------------------------------------- docked sidebar corner grip
    // panels.js also gives the sidebar a `.pane-resize` corner grip. On a docked
    // pane the old handler wrote an inline width/height on the grid item, which
    // does not resize the grid column: the sidebar drifts off its column edge.
    // No reload here — the splitter test left the sidebar at the 1180px max, so
    // the grip has 200px of shrink room at this viewport.
    await setViewport(cdp, 1440, HEIGHT);
    const grip = await cdp.eval(`(() => { const s = document.querySelector('.side > .pane-resize');
      if (!s || getComputedStyle(s).display === 'none') return null;
      const b = s.getBoundingClientRect();
      return { x: b.left + b.width / 2, y: b.top + b.height / 2 }; })()`);
    if (!grip) {
      check('sidebar corner grip exists', false, 'no .side > .pane-resize');
    } else {
      const gripBefore = await cdp.eval(MEASURE);
      const toX = Math.max(2, grip.x - 200);          // drag left: shrink the sidebar
      await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: grip.x, y: grip.y,
        button: 'left', clickCount: 1, buttons: 1 });
      for (let i = 1; i <= 4; i++) {
        await cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved',
          x: Math.round(grip.x + ((toX - grip.x) * i) / 4), y: grip.y, button: 'none', buttons: 1 });
      }
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: Math.round(toX),
        y: grip.y, button: 'left', buttons: 0 });
      await cdp.eval('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))');
      const gripAfter = await capture(cdp, 'grip', await cdp.eval(MEASURE));
      console.log(`\n--- docked sidebar corner grip drag (-200px) at 1440 ---`);
      console.log(`  before: ${gripBefore.gridColumns} | --side-w=${gripBefore.sideW} ` +
                  `sidebar w=${gripBefore.side.w} right=${gripBefore.side.r}`);
      console.log(`  after:  ${gripAfter.gridColumns} | --side-w=${gripAfter.sideW} ` +
                  `sidebar w=${gripAfter.side.w} right=${gripAfter.side.r} canvas=${gripAfter.canvas.w}`);
      console.log(`  shots: ${gripAfter.shots.full}  ${gripAfter.shots.clip}`);
      check('corner grip keeps the sidebar on its grid column',
        Math.abs(gripAfter.side.w - parseFloat(gripAfter.sideW)) <= 1.5 &&
        gripAfter.side.r <= 1440 + 0.5,
        `sidebar w=${gripAfter.side.w}px vs --side-w=${gripAfter.sideW}, ` +
        `right=${gripAfter.side.r} (viewport 1440)`);
    }

    // ---------------------------------------------------- detached pane
    // A pane window (`panewin.js`) moves the pane into `#paneRoot`, which fills
    // the window; the app grid is hidden and `#paneRoot > *` forces width:100%.
    // The native window cannot be built under `--serve`, so reproduce the exact
    // DOM/CSS contract: a stale 1400px `--side-w` must not push the detached
    // sidebar off-screen.
    await setViewport(cdp, 1100, HEIGHT);
    const paneWin = await cdp.eval(`(() => {
      document.documentElement.style.setProperty('--side-w', '1400px');
      const root = document.createElement('div');
      root.id = 'paneRoot';
      document.body.appendChild(root);
      root.appendChild(document.querySelector('.side'));
      document.documentElement.setAttribute('data-win', 'pane');
      const b = document.querySelector('#paneRoot > .side').getBoundingClientRect();
      return { w: b.width, h: b.height, right: b.right,
               viewportW: innerWidth, viewportH: innerHeight }; })()`);
    console.log(`\n--- detached pane window (simulated #paneRoot), --side-w=1400 at 1100 ---`);
    console.log(`  sidebar ${paneWin.w}x${paneWin.h} in a ${paneWin.viewportW}x${paneWin.viewportH} window`);
    check('detached pane fills its own window regardless of --side-w',
      Math.abs(paneWin.w - paneWin.viewportW) <= 1.5 && paneWin.right <= paneWin.viewportW + 0.5,
      `sidebar ${paneWin.w}x${paneWin.h}, right=${paneWin.right}, viewport=${paneWin.viewportW}`);

    const failed = results.filter((r) => !r.ok);
    const summary = { label: LABEL, widths: WIDTHS, checks: results.length,
                      failed: failed.length, consoleErrors, screenshots: OUT,
                      measurements: resultsByWidth };
    writeFileSync(join(OUT, `sidebar-${LABEL}-metrics.json`), JSON.stringify(summary, null, 2));
    console.log(`\nmetrics: ${join(OUT, `sidebar-${LABEL}-metrics.json`)}`);
    console.log(`CHECKS ${results.length - failed.length}/${results.length} passed` +
                `  consoleErrors=${consoleErrors.length}`);
    for (const e of consoleErrors) console.log('  console error: ' + e);
    if (failed.length || consoleErrors.length) process.exitCode = 1;
  } catch (e) {
    process.stdout.write(JSON.stringify({
      ok: false, error: String(e && e.stack || e), consoleErrors,
      chromeStderr: chromeErr.slice(-2000)
    }, null, 2) + '\n');
    process.exitCode = 1;
  } finally {
    try { if (targetId) await fetch(`http://127.0.0.1:${PORT}/json/close/${targetId}`); } catch {}
    try { ws && ws.close(); } catch {}
    chrome.kill('SIGKILL');
    // Give the renderers a beat to die before unlinking their profile dir,
    // otherwise the recursive remove races them and leaves a temp dir behind.
    await sleep(300);
    try { rmSync(profile, { recursive: true, force: true }); } catch {}
    // Headless Chromium's renderer children inherit the stderr pipe, so the
    // event loop can sit ~25s after the parent is SIGKILLed waiting for it to
    // close. Exit explicitly rather than let teardown blow the 30s budget.
    process.exit(process.exitCode || 0);
  }
}

main();
