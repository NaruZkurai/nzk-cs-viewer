#!/usr/bin/env node
/* probe_tabs.mjs — acceptance for document tabs, splits and session restore.
 *
 * Boots a private engine with `tools/with_serve.sh` (which passes --url and
 * --cdp-port), drives the REAL page in headless Chromium over CDP, and checks:
 *
 *   open 3 documents            -> 3 tabs
 *   switch tabs                 -> the right node renders, Monaco model reused
 *   split                       -> 2 visible editors side by side
 *   collapse / re-open the split
 *   close a tab                 -> it goes away
 *   reload                      -> the same tabs + split + active tab come back
 *   whole run                   -> no uncaught exception, no console.error
 *
 * Usage:
 *   tools/with_serve.sh -- node tools/probe_tabs.mjs
 *   node tools/probe_tabs.mjs --url http://127.0.0.1:8795/ --cdp-port 9341
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
const PORT = Number(arg('--cdp-port', '9341'));
const CHROME = arg('--chrome', 'chromium');
const TIMEOUT_MS = Number(arg('--timeout', '180000'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Three documents that exist under the default indexed root (_sealed_src):
// two real methods plus the whole Nzk namespace document.
const A = arg('--node-a', 'NZK.Core.NZKNaNimateWeightNormalizer');
const B = arg('--node-b', 'NZK.Core.NZKNaNimateBoneHolder');
const C = arg('--node-c', '#ns:NZK');
const KEY_A = 'node:' + A;
const KEY_B = 'node:' + B;
const KEY_C = 'node:' + C;

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

let cdp = null;
async function wait(expr, ms, label) {
  const t0 = Date.now();
  let last = null;
  for (;;) {
    try {
      last = await cdp.eval(`(async () => { try { return (${expr}); } catch (e) { return null; } })()`);
    } catch (e) { last = null; }
    if (last) return last;
    if (Date.now() - t0 > ms) throw new Error('timeout waiting for ' + (label || expr));
    await sleep(80);
  }
}

async function reloadPage() {
  const loaded = new Promise((res) => cdp.on('Page.loadEventFired', res));
  await cdp.send('Page.reload');
  await Promise.race([loaded, sleep(30000)]);
}

async function main() {
  const profile = mkdtempSync(join(tmpdir(), 'csviewer-tabs-'));
  const chrome = spawn(CHROME, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check', '--disable-extensions',
    `--user-data-dir=${profile}`, `--remote-debugging-port=${PORT}`, 'about:blank'
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  let chromeErr = '';
  chrome.stderr.on('data', (d) => { chromeErr += d.toString(); });

  let ws, targetId;
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

    const loaded = new Promise((res) => cdp.on('Page.loadEventFired', res));
    await cdp.send('Page.navigate', { url: URL_ });
    await Promise.race([loaded, sleep(30000)]);

    await wait('window.state && state.indexed ? 1 : 0', 90000, 'index');
    await wait('window.state.kinds && state.kinds.loaded ? 1 : 0', 60000, 'facets');

    // The engine persists the session, so a previous run can have left tabs
    // behind. Drop them in memory AND on disk, then boot fresh: clearing only
    // the blob is not enough, because pagehide would re-save the live tabs.
    await cdp.eval(`(function () {
      window.CsTabs.closeAll();
      if (state.source.open) closeSource();
      return 'ok';
    })()`);
    await cdp.eval(`apiPost('/api/viewer_state', { version: 1 })`);
    await reloadPage();
    await wait('window.state && state.indexed ? 1 : 0', 90000, 'fresh index');
    await wait('window.state.kinds && state.kinds.loaded ? 1 : 0', 60000, 'fresh facets');
    check('CsTabs installed', await cdp.eval('!!(window.CsTabs && window.CsTabs.isInstalled())'));

    // Monaco must be up before documents are opened: editor.js replays only the
    // last pending show() while the loader is still running.
    await cdp.eval(`window.CsEditor.load(); 'ok'`);
    await wait(`window.CsEditor.state() === 'ready' ? 1 : 0`, 60000, 'monaco');
    await wait(`window.CsTabs.count() === 0 ? 1 : 0`, 10000, 'clean session');

    const rendered = (key, n) =>
      `(window.CsTabs.count() === ${n} && window.CsTabs.activeKey() === ${JSON.stringify(key)} && ` +
      `window.CsTabs.renderedKey() === ${JSON.stringify(key)} && window.CsEditor.lineCount() > 0) ? 1 : 0`;

    // ---- open three documents ---------------------------------------------
    for (const [n, node, key] of [[1, A, KEY_A], [2, B, KEY_B], [3, C, KEY_C]]) {
      await cdp.eval(`openSource({ node: ${JSON.stringify(node)} }); 'ok'`);
      await wait(rendered(key, n), n === 3 ? 90000 : 40000, `tab ${n}`);
    }
    const opened = await cdp.eval(`({
      count: window.CsTabs.count(),
      domTabs: document.querySelectorAll('#cs-tabs .cs-tab').length,
      active: window.CsTabs.activeKey(),
      title: document.getElementById('sourceTitle').textContent
    })`);
    check('3 documents -> 3 tabs', opened.count === 3 && opened.domTabs === 3 && opened.active === KEY_C,
      `${opened.count} tabs (${opened.domTabs} in DOM), active=${opened.active}, title="${opened.title}"`);

    // ---- switch tabs: right document renders, model reused -----------------
    const stats0 = await cdp.eval('window.CsTabs.stats()');
    await cdp.eval(`document.querySelector('#cs-tabs .cs-tab[data-key=${JSON.stringify(KEY_A)}]').click(); 'ok'`);
    await wait(`window.CsTabs.activeKey() === ${JSON.stringify(KEY_A)} && window.CsTabs.renderedKey() === ${JSON.stringify(KEY_A)} && window.CsEditor.lineCount() > 0 ? 1 : 0`,
      20000, 'switch to A');
    const stats1 = await cdp.eval('window.CsTabs.stats()');
    const switched = await cdp.eval(`({
      active: window.CsTabs.activeKey(),
      title: document.getElementById('sourceTitle').textContent,
      nodeId: state.source.nodeId,
      lines: window.CsEditor.lineCount(),
      activeCls: document.querySelector('#cs-tabs .cs-tab.active').getAttribute('data-key')
    })`);
    check('switch tab renders the right document',
      switched.nodeId === A && switched.activeCls === KEY_A && switched.lines > 100,
      `active=${switched.active} node=${switched.nodeId} lines=${switched.lines}`);
    check('Monaco model reused on tab switch',
      stats1.modelsCreated === stats0.modelsCreated && stats1.modelsReused > stats0.modelsReused,
      `created ${stats0.modelsCreated}->${stats1.modelsCreated}, reused ${stats0.modelsReused}->${stats1.modelsReused}, ` +
      `event=${stats1.lastEvent}, disposed=${JSON.stringify(stats0.disposed)} -> ${JSON.stringify(stats1.disposed)}`);

    // ---- split -------------------------------------------------------------
    await cdp.eval(`document.getElementById('csSplitBtn').click(); 'ok'`);
    await wait(`window.CsTabs.isSplit() && window.CsTabs.splitLineCount() > 5 ? 1 : 0`, 30000, 'split');
    const split = await cdp.eval(`(() => {
      const eds = Array.from(document.querySelectorAll('#csSplitWrap .monaco-editor'));
      return {
        n: eds.length,
        widths: eds.map(e => Math.round(e.getBoundingClientRect().width)),
        active: window.CsTabs.activeKey(),
        splitKey: window.CsTabs.splitKey(),
        label: (document.querySelector('.cs-split-label') || {}).textContent || '',
        splitLines: window.CsTabs.splitLineCount(),
        primaryLines: window.CsEditor.lineCount()
      };
    })()`);
    check('split -> 2 visible editors side by side',
      split.n === 2 && split.widths.every((w) => w > 0) && split.splitKey && split.splitKey !== split.active &&
      split.splitLines > 0,
      `${split.n} editors ${JSON.stringify(split.widths)}px · split=${split.splitKey} (${split.label}) ` +
      `${split.splitLines} lines vs primary ${split.primaryLines}`);

    // ---- collapsible -------------------------------------------------------
    await cdp.eval(`document.getElementById('csSplitCollapse').click(); 'ok'`);
    await wait(`window.CsTabs.isSplit() === false ? 1 : 0`, 10000, 'collapse');
    const collapsed = await cdp.eval(`(() => {
      const body = document.getElementById('csSplitBody');
      return { hidden: getComputedStyle(body).display === 'none', isSplit: window.CsTabs.isSplit() };
    })()`);
    check('split is collapsible', collapsed.hidden && !collapsed.isSplit, JSON.stringify(collapsed));
    await cdp.eval(`document.getElementById('csSplitBtn').click(); 'ok'`);
    await wait(`window.CsTabs.isSplit() && window.CsTabs.splitLineCount() > 5 ? 1 : 0`,
      20000, 're-split');
    const resplit = await cdp.eval(`({ split: window.CsTabs.isSplit(), key: window.CsTabs.splitKey() })`);
    check('split can be reopened', resplit.split && !!resplit.key,
      `split=${resplit.key}`);

    // ---- close a tab -------------------------------------------------------
    await cdp.eval(`document.querySelector('#cs-tabs .cs-tab[data-key=${JSON.stringify(KEY_C)}] .cs-tab-x').click(); 'ok'`);
    await wait(`window.CsTabs.count() === 2 ? 1 : 0`, 20000, 'close C');
    const closed = await cdp.eval(`({
      count: window.CsTabs.count(),
      domTabs: document.querySelectorAll('#cs-tabs .cs-tab').length,
      hasC: !!document.querySelector('#cs-tabs .cs-tab[data-key=${JSON.stringify(KEY_C)}]'),
      active: window.CsTabs.activeKey(),
      splitKey: window.CsTabs.splitKey()
    })`);
    check('close tab removes it', closed.count === 2 && closed.domTabs === 2 && !closed.hasC,
      `${closed.count} tabs left, C present=${closed.hasC}, active=${closed.active}`);

    // ---- persist + reload --------------------------------------------------
    const before = await cdp.eval(`({
      count: window.CsTabs.count(),
      active: window.CsTabs.activeKey(),
      split: window.CsTabs.isSplit(),
      splitKey: window.CsTabs.splitKey(),
      keys: window.CsTabs.tabs().map(t => t.key)
    })`);
    await cdp.eval(`apiPost('/api/viewer_state', snapshotViewerState())`);
    await reloadPage();
    await wait('window.state && state.indexed ? 1 : 0', 90000, 'reload index');
    await wait('window.state.kinds && state.kinds.loaded ? 1 : 0', 60000, 'reload facets');
    await wait(`window.CsTabs && window.CsTabs.count() === ${before.count} ? 1 : 0`, 60000, 'restore tabs');
    await wait(`window.CsEditor && window.CsEditor.state() === 'ready' ? 1 : 0`, 60000, 'restore monaco');
    await wait(`window.CsTabs.renderedKey() === window.CsTabs.activeKey() ? 1 : 0`, 30000, 'restore active doc');
    await wait(`window.CsTabs.isSplit() && window.CsTabs.splitLineCount() > 5 ? 1 : 0`, 30000, 'restore split');
    const restored = await cdp.eval(`({
      count: window.CsTabs.count(),
      domTabs: document.querySelectorAll('#cs-tabs .cs-tab').length,
      active: window.CsTabs.activeKey(),
      split: window.CsTabs.isSplit(),
      splitKey: window.CsTabs.splitKey(),
      eds: document.querySelectorAll('#csSplitWrap .monaco-editor').length,
      title: document.getElementById('sourceTitle').textContent,
      keys: window.CsTabs.tabs().map(t => t.key)
    })`);
    check('reload restores tabs + split + active tab',
      restored.count === before.count && restored.domTabs === before.count &&
      restored.active === before.active && restored.split === before.split &&
      restored.splitKey === before.splitKey && restored.eds === 2 &&
      JSON.stringify(restored.keys) === JSON.stringify(before.keys),
      `before ${JSON.stringify(before)} -> after ${JSON.stringify(restored)}`);

    // ---- graceful degradation: corrupt session must not throw ---------------
    const degraded = await cdp.eval(`(() => {
      const before = window.CsTabs.count();
      const ok = window.CsTabs.restoreSession({ tabs: 'nonsense', split: null });
      const ok2 = window.CsTabs.restoreSession({ tabs: [null, 7, { params: {} }, { params: { node: '' } }] });
      return { before, ok, ok2, after: window.CsTabs.count(), alive: !!window.state.indexed };
    })()`);
    check('corrupt session degrades gracefully',
      degraded.ok === false && degraded.ok2 === false && degraded.after === degraded.before && degraded.alive,
      JSON.stringify(degraded));

    check('no console errors', consoleErrors.length === 0,
      consoleErrors.slice(0, 3).join(' | ') || 'none');

    const failed = results.filter((r) => !r.ok).length;
    console.log(`\n${results.length - failed}/${results.length} checks passed ` +
      `(chrome ${ver.Browser}, nodes ${A} / ${B} / ${C})`);
    if (consoleErrors.length) console.error(JSON.stringify(consoleErrors, null, 2));
    process.exitCode = failed ? 1 : 0;
  } catch (e) {
    console.error(JSON.stringify({
      ok: false, error: String((e && e.stack) || e), consoleErrors,
      chromeStderr: chromeErr.slice(-1500), results
    }, null, 2));
    process.exitCode = 1;
  } finally {
    try { if (targetId) await fetch(`http://127.0.0.1:${PORT}/json/close/${targetId}`); } catch { /* ignore */ }
    try { ws && ws.close(); } catch { /* ignore */ }
    chrome.kill('SIGKILL');
    // Give the renderers a beat to die before unlinking their profile dir,
    // otherwise the recursive remove races them and leaves a temp dir behind.
    await sleep(300);
    try { rmSync(profile, { recursive: true, force: true }); } catch { /* ignore */ }
    // Headless Chromium's renderer children inherit the stderr pipe, so the
    // event loop can sit ~25s after the parent is SIGKILLed waiting for it to
    // close. Exit explicitly rather than let teardown blow the 30s budget.
    process.exit(process.exitCode || 0);
  }
}

main();
