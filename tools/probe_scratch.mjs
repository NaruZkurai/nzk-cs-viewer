#!/usr/bin/env node
/* probe_scratch.mjs — acceptance for the read-only "Blocks" panel.
 *
 * Drives the REAL viewer (frontend + Rust engine over `csviewer --serve`) in
 * headless Chromium and exercises the Scratch-like block canvas the way a user
 * reaches it:
 *
 *   open       the Blocks button in the top bar
 *   blocks     /api/decls declarations rendered as cards with typed ports
 *   edges      /api/subgraph reference edges drawn between those cards
 *   hover      the peek (source snippet + reference list)
 *   click      navigate the shell to that node
 *
 * It also asserts the page has no uncaught exception and no console.error.
 *
 * The viewport is pinned to a desktop size: below 980px the viewer stacks its
 * sidebar under the canvas (media query), which leaves the canvas too short to
 * hold the panel.
 *
 * Usage:
 *   node tools/probe_scratch.mjs [--url http://127.0.0.1:8795/]
 *                                [--node NZK.Core.NZKNaNimateWeightNormalizer]
 *                                [--cdp-port 9340] [--chrome chromium]
 *
 * Requires a running `csviewer --serve` (tools/with_serve.sh wires it up).
 */
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import process from 'node:process';

function arg(name, dflt) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
}
const URL_ = arg('--url', 'http://127.0.0.1:8795/');
const NODE = arg('--node', 'NZK.Core.NZKNaNimateWeightNormalizer');
const PORT = Number(arg('--cdp-port', '9340'));
const CHROME = arg('--chrome', 'chromium');
const TIMEOUT_MS = Number(arg('--timeout', '120000'));
const MIN_CARDS = Number(arg('--min-cards', '8'));
const MIN_EDGES = Number(arg('--min-edges', '10'));
const VIEW_W = Number(arg('--width', '1400'));
const VIEW_H = Number(arg('--height', '900'));
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

async function main() {
  const profile = mkdtempSync(join(tmpdir(), 'csviewer-scratch-'));
  const chrome = spawn(CHROME, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check', '--disable-extensions',
    `--user-data-dir=${profile}`, `--remote-debugging-port=${PORT}`, 'about:blank'
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  let chromeErr = '';
  chrome.stderr.on('data', (d) => { chromeErr += d.toString(); });

  let ws, cdp, targetId;
  const consoleErrors = [];
  try {
    const ver = await waitForDevtools(PORT, Date.now() + TIMEOUT_MS);
    const create = await fetch(`http://127.0.0.1:${PORT}/json/new?about:blank`, { method: 'PUT' });
    const target = await create.json();
    targetId = target.id;
    ws = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((res, rej) => {
      ws.addEventListener('open', res, { once: true });
      ws.addEventListener('error', () => rej(new Error('cdp websocket error')), { once: true });
    });
    cdp = new CDP(ws);
    cdp.on('Runtime.exceptionThrown', (p) => consoleErrors.push('uncaught: ' +
      (p.exceptionDetails?.exception?.description || p.exceptionDetails?.text)));
    cdp.on('Runtime.consoleAPICalled', (p) => {
      if (p.type === 'error') {
        consoleErrors.push('console.error: ' +
          p.args.map((a) => a.value ?? a.description ?? '').join(' '));
      }
    });
    await cdp.send('Runtime.enable');
    await cdp.send('Page.enable');
    // Desktop viewport: below 980px the layout stacks the sidebar under the
    // canvas and the block panel has no room (that stacking is by design).
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: VIEW_W, height: VIEW_H, deviceScaleFactor: 1, mobile: false
    });
    const loaded = new Promise((res) => cdp.on('Page.loadEventFired', res));
    await cdp.send('Page.navigate', { url: URL_ });
    await Promise.race([loaded, sleep(30000)]);

    const click = async (x, y) => {
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
      await cdp.send('Input.dispatchMouseEvent', {
        type: 'mousePressed', x, y, button: 'left', buttons: 1, clickCount: 1
      });
      await cdp.send('Input.dispatchMouseEvent', {
        type: 'mouseReleased', x, y, button: 'left', buttons: 0, clickCount: 1
      });
    };
    const waitFor = async (expr, ms) => {
      const t0 = Date.now();
      let last;
      while (Date.now() - t0 < ms) {
        try { last = await cdp.eval(expr); } catch (e) { last = undefined; }
        if (last) return last;
        await sleep(80);
      }
      return last;
    };

    // ---- boot --------------------------------------------------------------
    check('engine booted', !!(await waitFor('!!(window.state && state.indexed)', 60000)));
    check('CsScratch loaded', !!(await waitFor(
      `typeof window.CsScratch === 'object' && window.CsScratch !== null`, 15000)));

    // The panel follows the live view state. Wait for the graph facets (the
    // session restore that follows them re-opens the last document), then select
    // a real node the way the graph does — goToNode pulls it in and selects it.
    await waitFor('!!(state.kinds && state.kinds.loaded)', 30000);
    await cdp.eval(`goToNode(${JSON.stringify(NODE)}); 'ok'`);
    const selected = await waitFor(`state.selected === ${JSON.stringify(NODE)}`, 12000);
    check('real node selected', !!selected, NODE);

    // ---- the panel opens ---------------------------------------------------
    const btn = await cdp.eval(`(() => {
      const b = document.getElementById('blocksBtn');
      if (!b) return null;
      const r = b.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    })()`);
    check('Blocks button exists', !!btn);
    if (btn) await click(btn.x, btn.y);
    const open = await waitFor(
      `!document.getElementById('blocksPanel').classList.contains('hidden')`, 6000);
    check('blocks panel opens', !!open);

    // ---- blocks + edges ----------------------------------------------------
    const counts = await waitFor(`(() => {
      const cards = document.querySelectorAll('#blocksCards .block-card').length;
      const paths = document.querySelectorAll('#blocksEdges path.blocks-edge').length;
      return (cards >= ${MIN_CARDS} && paths >= ${MIN_EDGES}) ? { cards, paths } : null;
    })()`, 12000);
    check(`block cards render (>= ${MIN_CARDS})`, !!(counts && counts.cards >= MIN_CARDS),
      counts ? `${counts.cards} cards` : 'none');

    const edgeInfo = await cdp.eval(`(() => {
      const paths = Array.from(document.querySelectorAll('#blocksEdges path.blocks-edge'));
      const kinds = {};
      paths.forEach(p => { const k = p.getAttribute('data-kind'); kinds[k] = (kinds[k] || 0) + 1; });
      return { n: paths.length, kinds };
    })()`);
    check(`reference edges render (>= ${MIN_EDGES})`, edgeInfo.n >= MIN_EDGES,
      `${edgeInfo.n} edges ${JSON.stringify(edgeInfo.kinds)}`);

    // ---- typed ports -------------------------------------------------------
    const ports = await cdp.eval(`(() => {
      const cards = document.querySelectorAll('#blocksCards .block-card');
      const inP = document.querySelectorAll('#blocksCards .port-in').length;
      const outP = document.querySelectorAll('#blocksCards .port-out').length;
      const typed = Array.from(document.querySelectorAll('#blocksCards .port'))
        .filter(p => (p.getAttribute('data-type') || '').length > 0).length;
      const paramPorts = Array.from(document.querySelectorAll('#blocksCards .port-in'))
        .map(p => p.getAttribute('data-type')).slice(0, 4);
      return { cards: cards.length, inP, outP, typed, paramPorts };
    })()`);
    check('typed ports render', ports.inP >= 1 && ports.outP >= 1 && ports.typed >= 2,
      `${ports.inP} in, ${ports.outP} out, ${ports.typed} typed (${ports.paramPorts.join(', ')})`);

    // ---- hover peek --------------------------------------------------------
    const hoverTarget = await cdp.eval(`(() => {
      const c = document.querySelector('#blocksCards .block-card');
      if (!c) return null;
      c.scrollIntoView({ block: 'center' });
      const r = c.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2, id: c.getAttribute('data-id') };
    })()`);
    if (hoverTarget) {
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: 1, y: 1 });
      await cdp.send('Input.dispatchMouseEvent', {
        type: 'mouseMoved', x: hoverTarget.x, y: hoverTarget.y
      });
    }
    const peekShown = await waitFor(`(() => {
      const p = document.getElementById('blocksPeek');
      const s = document.getElementById('blocksPeekSrc');
      return p && !p.classList.contains('hidden') && s && s.textContent.length > 20;
    })()`, 6000);
    check('hover peek opens with content', !!peekShown, hoverTarget ? hoverTarget.id : 'no card');

    const peek = await cdp.eval(`(() => {
      const s = document.getElementById('blocksPeekSrc');
      const refs = document.getElementById('blocksPeekRefs');
      return {
        visible: !document.getElementById('blocksPeek').classList.contains('hidden'),
        srcLen: (s && s.textContent || '').length,
        refRows: refs ? refs.querySelectorAll('.bp-ref').length : 0,
        name: (document.getElementById('blocksPeekName') || {}).textContent || ''
      };
    })()`);
    check('peek shows a source snippet', peek.srcLen > 20, `${peek.srcLen} chars`);
    check('peek shows references', peek.refRows > 0, `${peek.refRows} rows for ${peek.name}`);

    // ---- click to navigate -------------------------------------------------
    const clickTarget = await cdp.eval(`(() => {
      const cards = Array.from(document.querySelectorAll('#blocksCards .block-card'));
      const c = cards.find(x => x.getAttribute('data-kind') === 'method') || cards[0];
      if (!c) return null;
      c.scrollIntoView({ block: 'center' });
      const r = c.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2, id: c.getAttribute('data-id') };
    })()`);
    if (clickTarget) await click(clickTarget.x, clickTarget.y);
    const navigated = clickTarget && await waitFor(
      `(state.selected === ${JSON.stringify(clickTarget.id)})`, 8000);
    check('click navigates the shell', !!navigated,
      clickTarget ? clickTarget.id : 'no method card');

    // ---- resize: the SVG must follow the panel, not ratchet ----------------
    // Regression: the SVG is an absolutely-positioned child of the stage, so
    // sizing it from the stage's own scrollWidth made it grow-only and left a
    // spurious horizontal scrollbar in .blocks-scroll when the panel narrowed.
    const resize = await cdp.eval(`(async () => {
      const panel = document.getElementById('blocksPanel');
      const scroll = document.getElementById('blocksScroll');
      const svg = document.getElementById('blocksEdges');
      const geom = () => ({
        panel: Math.round(panel.getBoundingClientRect().width),
        client: scroll.clientWidth,
        scroll: scroll.scrollWidth,
        svg: Number(svg.getAttribute('width')),
        vb: svg.getAttribute('viewBox'),
        paths: document.querySelectorAll('#blocksEdges path.blocks-edge').length
      });
      const before = geom();
      panel.style.width = '420px';
      window.dispatchEvent(new Event('resize'));
      await new Promise(r => setTimeout(r, 500));
      const narrow = geom();
      panel.style.width = '';
      window.dispatchEvent(new Event('resize'));
      await new Promise(r => setTimeout(r, 500));
      const after = geom();
      return { before, narrow, after };
    })()`);
    check('narrow panel has no spurious horizontal scroll',
      resize.narrow.scroll <= resize.narrow.client + 1 && resize.narrow.svg <= resize.narrow.client,
      `panel ${resize.before.panel}->${resize.narrow.panel}px: client ${resize.narrow.client}, ` +
      `scrollWidth ${resize.narrow.scroll}, svg ${resize.narrow.svg}`);
    const vbW = Number(String(resize.narrow.vb || '').split(/\s+/)[2]);
    check('viewBox tracks the resized panel',
      vbW === resize.narrow.svg && vbW <= resize.narrow.client + 1,
      `viewBox "${resize.narrow.vb}" for svg ${resize.narrow.svg}px ` +
      `(client ${resize.narrow.client})`);
    check('edges survive the resize and restore',
      resize.narrow.paths >= MIN_EDGES && resize.after.paths >= MIN_EDGES &&
      resize.after.scroll <= resize.after.client + 1 &&
      resize.after.svg > resize.narrow.svg,
      `${resize.narrow.paths} paths narrow -> ${resize.after.paths} restored ` +
      `(svg ${resize.narrow.svg} -> ${resize.after.svg})`);

    // ---- clean page --------------------------------------------------------
    check('no console errors', consoleErrors.length === 0,
      consoleErrors.slice(0, 3).join(' | ') || 'none');

    const failed = results.filter((r) => !r.ok).length;
    console.log(`\n${results.length - failed}/${results.length} checks passed ` +
      `(chrome ${ver.Browser}, node ${NODE})`);
    if (consoleErrors.length) console.error(JSON.stringify(consoleErrors, null, 2));
    process.exitCode = failed ? 1 : 0;
  } catch (e) {
    console.error(JSON.stringify({
      ok: false, error: String((e && e.stack) || e), consoleErrors,
      chromeStderr: chromeErr.slice(-1500), results
    }, null, 2));
    process.exitCode = 1;
  } finally {
    try { if (targetId) await fetch(`http://127.0.0.1:${PORT}/json/close/${targetId}`); } catch {}
    try { ws && ws.close(); } catch {}
    chrome.kill('SIGKILL');
    await sleep(300);
    try { rmSync(profile, { recursive: true, force: true }); } catch {}
    process.exit(process.exitCode || 0);
  }
}

main();
