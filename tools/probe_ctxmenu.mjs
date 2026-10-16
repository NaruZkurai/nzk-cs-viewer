#!/usr/bin/env node
/* probe_ctxmenu.mjs — open the v2 editor, right-click in it, and report exactly
 * which box appears and where its colours come from, per source theme.
 *
 * Dev/verification only. Spawns headless Chromium over CDP against the running
 * dev stack (v1 engine on :8790 behind devproxy on :8799).
 */

import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const URL_ = process.argv.find((a) => a.startsWith('--url='))?.slice(6) || 'http://127.0.0.1:8799/';
const OUT = process.argv.find((a) => a.startsWith('--out='))?.slice(6) || '/tmp/ctxmenu.png';
const PORT = 9344;
const CHROME = 'chromium';
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
  throw new Error(`devtools never came up: ${last}`);
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
        (this.handlers.get(msg.method) || []).forEach((h) => h(msg.params));
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
    const r = await this.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) {
      throw new Error('eval failed: ' + (r.exceptionDetails.exception?.description || r.exceptionDetails.text));
    }
    return r.result.value;
  }
}

const READ_MENU = `(() => {
  const pick = ['.monaco-menu-container', '.monaco-menu', '.context-view']
    .flatMap((s) => Array.from(document.querySelectorAll(s)));
  const boxes = pick.map((el) => {
    const cs = getComputedStyle(el);
    const r = el.getBoundingClientRect();
    return {
      cls: String(el.className).slice(0, 80),
      rect: { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) },
      bg: cs.backgroundColor, fg: cs.color, display: cs.display, z: cs.zIndex,
      htmlLen: el.innerHTML.length,
      itemCount: el.querySelectorAll('.action-item, .monaco-action-bar .action-label').length
    };
  });
  const menu = pick.find((el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; }) || null;
  const cs = menu ? getComputedStyle(menu) : null;
  const inner = menu ? (menu.querySelector('.monaco-menu') || menu) : null;
  const ics = inner ? getComputedStyle(inner) : null;
  const item = inner ? inner.querySelector('.action-item') : null;
  const itemCs = item ? getComputedStyle(item) : null;
  const ed = window.CsEditor && window.CsEditor.editor && window.CsEditor.editor();
  const edBg = ed ? getComputedStyle(ed.getDomNode()).backgroundColor : null;
  const vars = {};
  const rootEl = document.querySelector('.monaco-editor') || document.documentElement;
  const rootStyle = getComputedStyle(rootEl);
  ['--vscode-menu-background','--vscode-menu-foreground','--vscode-menu-border',
   '--vscode-menu-selectionBackground','--vscode-menu-separatorBackground',
   '--vscode-editorWidget-background','--vscode-dropdown-background',
   '--vscode-list-activeSelectionBackground','--vscode-editor-background'
  ].forEach((k) => { vars[k] = rootStyle.getPropertyValue(k) || null; });
  const r = menu ? menu.getBoundingClientRect() : null;
  return {
    present: !!menu,
    target: window.__ctxTarget || null,
    boxes,
    menuClass: menu ? String(menu.className) : null,
    bg: ics && ics.backgroundColor,
    fg: ics && ics.color,
    border: ics && (ics.borderTopWidth + ' ' + ics.borderTopStyle + ' ' + ics.borderTopColor),
    shadow: ics && ics.boxShadow,
    radius: ics && ics.borderRadius,
    itemBg: itemCs && itemCs.backgroundColor,
    rect: r && { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) },
    viewBg: cs && cs.backgroundColor,
    editorBg: edBg,
    vars
  };
})()`;

const READ_CARD = `(() => {
  const card = document.getElementById('tooltip');
  if (!card) return { present: false };
  const cs = getComputedStyle(card);
  const badge = card.querySelector('.tt-badge');
  const bcs = badge ? getComputedStyle(badge) : null;
  const r = card.getBoundingClientRect();
  return {
    present: !card.classList.contains('hidden'),
    classes: card.className,
    visible: card.offsetParent !== null && !card.classList.contains('hidden'),
    bg: cs.backgroundColor,
    fg: cs.color,
    borderLeft: cs.borderLeftWidth + ' ' + cs.borderLeftStyle + ' ' + cs.borderLeftColor,
    border: cs.borderTopWidth + ' ' + cs.borderTopStyle + ' ' + cs.borderTopColor,
    shadow: cs.boxShadow,
    radius: cs.borderRadius,
    badge: badge ? { text: badge.textContent, bg: bcs.backgroundColor, fg: bcs.color, border: bcs.borderTopColor } : null,
    rect: { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) },
    uiTheme: document.documentElement.getAttribute('data-ui-theme'),
    label: (card.querySelector('.tt-label') || {}).textContent || null
  };
})()`;

async function canvasRightClick(cdp) {
  return cdp.eval(`(() => {
    const ids = (state.view && state.view.nodes || []).map((n) => n.id);
    let picked = null;
    for (const id of ids) {
      const p = state.pos.get(id);
      if (!p) continue;
      const s = worldToScreen(p.x, p.y);
      const r = canvas.getBoundingClientRect();
      const cx = r.left + s.x, cy = r.top + s.y;
      if (cx > r.left + 4 && cx < r.right - 4 && cy > r.top + 4 && cy < r.bottom - 4) {
        picked = { id, cx, cy };
        break;
      }
    }
    if (!picked) return { ok: false, reason: 'no on-screen node' };
    const opt = { bubbles: true, cancelable: true, clientX: picked.cx, clientY: picked.cy,
                  button: 2, buttons: 2, view: window };
    canvas.dispatchEvent(new MouseEvent('contextmenu', opt));
    return { ok: true, ...picked, hit: (hitTest(worldToScreen(state.pos.get(picked.id).x, state.pos.get(picked.id).y).x,
                                                 worldToScreen(state.pos.get(picked.id).x, state.pos.get(picked.id).y).y) || {}).id };
  })()`);
}

async function rightClick(cdp) {
  // Monaco opens its menu from the DOM `contextmenu` event on the editor node;
  // synthetic CDP mouse input alone does not raise it in headless Chromium.
  await cdp.eval(`(() => {
    const ed = window.CsEditor.editor();
    const el = ed.getDomNode();
    const r = el.getBoundingClientRect();
    const x = Math.round(r.left + 260), y = Math.round(r.top + 70);
    // The listener lives on a descendant of .monaco-editor (the view), so the
    // event must be dispatched at the element actually under the point.
    const target = document.elementFromPoint(x, y) || el;
    window.__ctxTarget = { x, y, tag: target.tagName, cls: String(target.className).slice(0, 120) };
    const opt = { bubbles: true, cancelable: true, clientX: x, clientY: y, button: 2, buttons: 2, view: window };
    target.dispatchEvent(new MouseEvent('mousedown', opt));
    target.dispatchEvent(new MouseEvent('mouseup', { ...opt, buttons: 0 }));
    target.dispatchEvent(new MouseEvent('contextmenu', opt));
    return window.__ctxTarget;
  })()`);
  await sleep(700);
}

async function main() {
  const profile = mkdtempSync(join(tmpdir(), 'csviewer-ctx-'));
  const chrome = spawn(CHROME, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check', '--disable-extensions',
    `--user-data-dir=${profile}`, `--remote-debugging-port=${PORT}`, 'about:blank'
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  let chromeErr = '';
  chrome.stderr.on('data', (d) => { chromeErr += d.toString(); });

  const deadline = Date.now() + 120000;
  try {
    await waitForDevtools(PORT, deadline);
    const create = await fetch(`http://127.0.0.1:${PORT}/json/new?${encodeURIComponent('about:blank')}`, { method: 'PUT' });
    const target = await create.json();
    const ws = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((res, rej) => {
      ws.addEventListener('open', res, { once: true });
      ws.addEventListener('error', () => rej(new Error('cdp websocket error')), { once: true });
    });
    const cdp = new CDP(ws);
    await cdp.send('Runtime.enable');
    await cdp.send('Page.enable');
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 1400, height: 950, deviceScaleFactor: 1, mobile: false
    });

    const loaded = new Promise((res) => cdp.on('Page.loadEventFired', res));
    await cdp.send('Page.navigate', { url: URL_ });
    await Promise.race([loaded, sleep(30000)]);

    const MODE = (process.argv.find((a) => a.startsWith('--mode='))?.slice(7)) || 'menu';

    if (MODE === 'emenu') {
      await cdp.eval(`(async () => {
        const wait = (f, ms) => new Promise((res, rej) => { const t0 = Date.now();
          (function poll(){ if (f()) return res(true); if (Date.now()-t0>ms) return rej(new Error('timeout')); setTimeout(poll, 100); })(); });
        window.CsEditor.load();
        await wait(() => window.CsEditor.state() === 'ready', 60000);
        openSource({ file: '_sealed_src/nan.cs.nzk', line: 40, context: 60, mode: 'raw' });
        await wait(() => window.CsEditor.lineCount() > 0, 30000);
        return true;
      })()`);
      const opened = await cdp.eval(`(() => {
        const ed = window.CsEditor.editor();
        const c = ed.getContribution('editor.contrib.contextmenu');
        if (!c) return 'no contextmenu contribution';
        c.showContextMenu();
        return 'shown';
      })()`);
      await sleep(900);
      const dump = await cdp.eval(`(() => {
        const varsAt = (el) => {
          if (!el) return null;
          const cs = getComputedStyle(el);
          const out = {};
          ['--vscode-menu-background','--vscode-menu-foreground','--vscode-menu-selectionBackground',
           '--vscode-editorWidget-background','--vscode-editor-background','--vscode-foreground'].forEach((k) => {
            out[k] = cs.getPropertyValue(k) || '(unset)';
          });
          return out;
        };
        const menu = document.querySelector('.monaco-menu');
        const view = document.querySelector('.context-view');
        const item = document.querySelector('.monaco-menu .action-item .action-label');
        const cs = menu ? getComputedStyle(menu) : null;
        const ics = item ? getComputedStyle(item) : null;
        const r = menu ? menu.getBoundingClientRect() : null;
        const parentChain = [];
        let el = menu;
        while (el && parentChain.length < 6) { parentChain.push(el.tagName + '.' + String(el.className).slice(0,60)); el = el.parentElement; }
        return {
          opened: ${JSON.stringify(opened)},
          menuPresent: !!menu, menuRect: r && { w: Math.round(r.width), h: Math.round(r.height), x: Math.round(r.x), y: Math.round(r.y) },
          menuBg: cs && cs.backgroundColor, menuColor: cs && cs.color,
          menuBorder: cs && cs.borderTopWidth + ' ' + cs.borderTopColor,
          itemColor: ics && ics.color, itemBg: ics && ics.backgroundColor,
          itemCount: document.querySelectorAll('.monaco-menu .action-item').length,
          parentChain,
          viewBg: view ? getComputedStyle(view).backgroundColor : null,
          varsRoot: varsAt(document.documentElement),
          varsBody: varsAt(document.body),
          syntheticMenu: (() => {
            const wrap = document.createElement('div');
            wrap.className = 'context-view';
            wrap.innerHTML = '<div class="monaco-menu-container"><div class="monaco-menu">' +
              '<div class="monaco-action-bar vertical"><ul class="actions-container">' +
              '<li class="action-item"><a class="action-label">Go to Definition</a></li>' +
              '</ul></div></div></div>';
            document.body.appendChild(wrap);
            const box = getComputedStyle(wrap.querySelector('.monaco-menu-container'));
            const txt = getComputedStyle(wrap.querySelector('.action-label'));
            const out = { bg: box.backgroundColor, color: txt.color, text: txt.color };
            wrap.remove();
            return out;
          })(),
          varsEditor: varsAt(document.querySelector('.monaco-editor')),
          varsView: varsAt(view),
          varsMenu: varsAt(menu),
          allViews: Array.from(document.querySelectorAll('.context-view')).map((v) => {
            const rr = v.getBoundingClientRect();
            return { cls: String(v.className).slice(0,60), w: Math.round(rr.width), h: Math.round(rr.height), bg: getComputedStyle(v).backgroundColor };
          })
        };
      })()`);
      const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
      writeFileSync(OUT, Buffer.from(shot.data, 'base64'));
      console.log(JSON.stringify({ url: URL_, mode: MODE, screenshot: OUT, dump }, null, 2));
      return;
    }

    await cdp.eval(`(async () => {
      const wait = (f, ms) => new Promise((res, rej) => {
        const t0 = Date.now();
        (function poll() {
          if (f()) return res(true);
          if (Date.now() - t0 > ms) return rej(new Error('timeout'));
          setTimeout(poll, 100);
        })();
      });
      await wait(() => window.state && state.indexed && typeof openSource === 'function', 90000);
      const wantCanvas = ${JSON.stringify(MODE)} === 'canvas';
      await wait(() => (state.view && state.view.nodes && state.view.nodes.length) > 0, 30000);
      await wait(() => (wantCanvas ? true : (window.CsEditor && window.CsEditor.state() !== 'idle')), 5000).catch(() => {});
      if (!wantCanvas) {
        window.CsEditor.load();
        await wait(() => window.CsEditor.state() === 'ready', 60000);
        // A raw file window is enough to study the menu, and it is fast.
        openSource({ file: '_sealed_src/nan.cs.nzk', line: 40, context: 60, mode: 'raw' });
        await wait(() => window.CsEditor.lineCount() > 0, 30000);
      }
      return true;
    })()`);

    if (MODE === 'canvas') {
      const results = {};
      for (const ui of ['dark-v1', 'nzk-dark+']) {
        if (ui) { await cdp.eval(`applyUiTheme(${JSON.stringify(ui)}, { silent: true }); true`); }
        await sleep(400);
        const click = await canvasRightClick(cdp);
        await sleep(500);
        results[ui || '(default)'] = { click, card: await cdp.eval(READ_CARD) };
      }
      const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
      writeFileSync(OUT, Buffer.from(shot.data, 'base64'));
      console.log(JSON.stringify({ url: URL_, mode: MODE, screenshot: OUT, results }, null, 2));
      return;
    }

    const results = {};
    for (const theme of ['darkplus', 'darkmodern', 'monokai', 'solarized']) {
      await cdp.eval(`applySourceTheme(${JSON.stringify(theme)}); true`);
      await sleep(300);
      await rightClick(cdp);
      results[theme] = await cdp.eval(READ_MENU);
      // close the menu for the next round
      await cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
      await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
      await sleep(200);
    }

    // screenshot with the menu open, monokai (the loudest theme)
    await cdp.eval(`applySourceTheme('monokai'); true`);
    await sleep(300);
    await rightClick(cdp);
    const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
    writeFileSync(OUT, Buffer.from(shot.data, 'base64'));

    console.log(JSON.stringify({ url: URL_, screenshot: OUT, results }, null, 2));
  } catch (e) {
    console.error('PROBE FAILED:', e.message);
    if (chromeErr) console.error('chrome stderr tail:', chromeErr.slice(-800));
    process.exitCode = 1;
  } finally {
    chrome.kill('SIGKILL');
    // Headless Chromium's renderer children inherit the stderr pipe, so the
    // event loop can sit ~25s after the parent is SIGKILLed waiting for it to
    // close. Exit explicitly rather than let teardown blow the 30s budget.
    process.exit(process.exitCode || 0);
  }
}

main();
