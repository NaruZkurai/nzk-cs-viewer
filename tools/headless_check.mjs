#!/usr/bin/env node
/* headless_check.mjs — drive the v2 viewer in headless Chromium over CDP and
 * measure what opening a huge synthesized document actually costs.
 *
 * Verifies, against a live server:
 *   - the page loads with no uncaught exception and no console error
 *   - Monaco reaches the `ready` state
 *   - a real /api/doc payload (default #ns:NZK, the whole toolkit) renders
 *   - how long that render takes, how many model lines, how many DOM nodes
 *
 * With --legacy it sets __CSVIEWER_LEGACY_RENDERER__ before any page script
 * runs, so the same payload goes through the v1 per-line <div>/<span> renderer.
 * Running both gives the before/after number.
 *
 * Usage:
 *   node tools/headless_check.mjs --url http://127.0.0.1:8799/ [--node '#ns:NZK'] [--legacy]
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
const URL_ = arg('--url', 'http://127.0.0.1:8799/');
const NODE = arg('--node', '#ns:NZK');
const LEGACY = process.argv.includes('--legacy');
const PORT = Number(arg('--cdp-port', '9333'));
const CHROME = arg('--chrome', 'chromium');
const TIMEOUT_MS = Number(arg('--timeout', '120000'));

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
    this.ws = ws; this.id = 0; this.pending = new Map(); this.events = [];
    this.handlers = new Map();
    ws.addEventListener('message', (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
      } else if (msg.method) {
        this.events.push(msg);
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
  async eval(expression, { awaitPromise = true } = {}) {
    const r = await this.send('Runtime.evaluate', {
      expression, awaitPromise, returnByValue: true, allowUnsafeEvalBlockedByCSP: true
    });
    if (r.exceptionDetails) {
      throw new Error('eval threw: ' + (r.exceptionDetails.exception?.description ||
        r.exceptionDetails.text));
    }
    return r.result.value;
  }
}

const HELPERS = `
window.__probe = {
  async waitFor(fn, ms, label) {
    const t0 = Date.now();
    for (;;) {
      try { if (fn()) return Date.now() - t0; } catch (e) {}
      if (Date.now() - t0 > ms) throw new Error('timeout waiting for ' + label);
      await new Promise(r => setTimeout(r, 50));
    }
  }
};
`;

async function readMetrics(cdp) {
  const r = await cdp.send('Performance.getMetrics');
  const out = {};
  for (const m of r.metrics || []) out[m.name] = m.value;
  return out;
}

async function main() {
  const profile = mkdtempSync(join(tmpdir(), 'csviewer-cdp-'));
  const chrome = spawn(CHROME, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check', '--disable-extensions',
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
    await cdp.send('Page.addScriptToEvaluateOnNewDocument',
      { source: HELPERS + (LEGACY ? 'window.__CSVIEWER_LEGACY_RENDERER__ = true;' : '') });

    const loaded = new Promise((res) => cdp.on('Page.loadEventFired', res));
    await cdp.send('Page.navigate', { url: URL_ });
    await Promise.race([loaded, sleep(30000)]);
    await cdp.eval('typeof window.__probe', { awaitPromise: false });

    // Wait for the app to finish its initial index (auto-index-on-start). A
    // document window (?win=doc) deliberately skips loadNamespaces, so accept
    // the booted state there instead of insisting on the node list.
    await cdp.eval(`(async () => {
      await window.__probe.waitFor(
        () => window.state && state.indexed &&
              (${JSON.stringify(URL_)}.indexOf('win=doc') >= 0 || (state.nodes && state.nodes.length)),
        90000, 'boot');
      return state.indexed;
    })()`);

    const renderer = LEGACY ? 'legacy' : 'monaco';
    if (!LEGACY) {
      await cdp.eval(`window.CsEditor.load(); 'ok'`, { awaitPromise: false });
      await cdp.eval(`(async () => {
        await window.__probe.waitFor(() => window.CsEditor.state() === 'ready', 60000, 'monaco');
        return window.CsEditor.state();
      })()`);
    }

    const node = NODE;
    // Real browser cost, not just JS stopwatch time: Chrome's own cumulative
    // LayoutDuration / RecalcStyleDuration / ScriptDuration for the render.
    await cdp.send('Performance.enable');
    const m0 = await readMetrics(cdp);
    const result = await cdp.eval(`(async () => {
      const r = await fetch('/api/doc?node=' + encodeURIComponent(${JSON.stringify(NODE)}));
      if (!r.ok) throw new Error('/api/doc HTTP ' + r.status);
      const d = await r.json();
      const lines = d.lines || [];
      const autoLines = (window.CsEditor && window.CsEditor.lineCount()) || 0;
      const t0 = performance.now();
      renderSource(d);
      const t1 = performance.now();
      // Force a layout+style pass so the number includes real paint cost.
      void document.getElementById('sourceBody').getBoundingClientRect();
      const t2 = performance.now();
      const nodes = document.querySelectorAll('#sourceBody *').length;
      const rows = document.querySelectorAll('#sourceBody .sline').length;

      // Scroll stress: 60 jumps down the whole document. This is what the
      // user actually feels — with the v1 renderer every step re-lays-out the
      // entire 357k-node pane; with Monaco the pane stays viewport-sized.
      const pane = document.getElementById('sourceBody');
      const med = window.CsEditor && window.CsEditor.editor && window.CsEditor.editor();
      const total = pane.scrollHeight || 1;
      const t3 = performance.now();
      for (let i = 0; i < 60; i++) {
        const y = Math.round((i / 60) * total);
        if (med) med.setScrollTop(y); else pane.scrollTop = y;
      }
      void pane.getBoundingClientRect();
      const t4 = performance.now();
      const nodesAfter = document.querySelectorAll('#sourceBody *').length;
      return {
        renderer: ${JSON.stringify(renderer)},
        node: ${JSON.stringify(NODE)},
        payloadLines: lines.length,
        total: d.total || lines.length,
        doc: !!d.doc,
        renderMs: Math.round(t1 - t0),
        layoutMs: Math.round(t2 - t1),
        scrollMs: Math.round(t4 - t3),
        domNodes: nodes,
        domNodesAfterScroll: nodesAfter,
        legacyRows: rows,
        autoRenderedLines: autoLines,
        popButton: !!document.getElementById('srcPopBtn'),
        monacoLines: (window.CsEditor && window.CsEditor.lineCount()) || 0,
        monacoState: window.CsEditor ? window.CsEditor.state() : 'absent',
        capped: (typeof SOURCE_MAX_DOC_LINES !== 'undefined') ? SOURCE_MAX_DOC_LINES : null
      };
    })()`);

    const m1 = await readMetrics(cdp);
    const browserCost = {};
    for (const k of ['LayoutDuration', 'RecalcStyleDuration', 'ScriptDuration',
                     'TaskDuration', 'LayoutCount', 'RecalcStyleCount', 'Nodes']) {
      const d = (m1[k] ?? 0) - (m0[k] ?? 0);
      // Chrome reports durations in seconds and counts as integers.
      browserCost[k] = k.endsWith('Duration') ? Math.round(d * 1e6) / 1e3 : d;
    }
    const out = { ok: true, result, browserCostMs: browserCost, consoleErrors,
                  chromeVersion: ver.Browser };
    process.stdout.write(JSON.stringify(out, null, 2) + '\n');
    if (consoleErrors.length) process.exitCode = 2;
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
