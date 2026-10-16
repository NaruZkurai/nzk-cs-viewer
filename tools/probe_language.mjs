#!/usr/bin/env node
/* probe_language.mjs — drive the v2 editor's C# language features against the
 * live dev stack and report what actually happened.
 *
 * Checks, on a real generated document:
 *   - symbol resolution at the fully-qualified level
 *   - the reference set (graph edges, declaration sites)
 *   - Alt+F12 peek definition (does the peek widget open, on what)
 *   - Shift+F12 peek references
 *   - F2's edit computation: how many edits, in how many files
 *
 * Dev/verification only, like probe_ctxmenu.mjs.
 */

import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const URL_ = process.argv.find((a) => a.startsWith('--url='))?.slice(6) || 'http://127.0.0.1:8799/';
const OUT = process.argv.find((a) => a.startsWith('--out='))?.slice(6) || '/tmp/lang.png';
const PORT = 9355;
const CHROME = 'chromium';
const NODE = 'NZK.Core.MeshesMerge.MergeOne(UnityEngine.GameObject, UnityEngine.Mesh)';
const SITE = { file: '_sealed_src/MeshesMerge.cs.nzk', line: 268, word: 'FindSourceMesh' };
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

const PEEK = `(() => {
  const w = document.querySelector('.peekview-widget');
  if (!w) return { open: false };
  const title = w.querySelector('.peekview-title');
  const body = w.querySelector('.monaco-list-rows, .monaco-editor, .peekview-body');
  const rows = w.querySelectorAll('.monaco-list-row').length;
  return {
    open: true,
    visible: w.offsetWidth > 0,
    title: title ? title.textContent.replace(/\\s+/g, ' ').trim().slice(0, 140) : null,
    kind: w.className,
    rows,
    bodyText: body ? body.textContent.replace(/\\s+/g, ' ').trim().slice(0, 200) : null,
    rect: (() => { const r = w.getBoundingClientRect(); return { w: Math.round(r.width), h: Math.round(r.height) }; })()
  };
})()`;

async function main() {
  const profile = mkdtempSync(join(tmpdir(), 'csviewer-lang-'));
  const chrome = spawn(CHROME, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check', '--disable-extensions',
    `--user-data-dir=${profile}`, `--remote-debugging-port=${PORT}`, 'about:blank'
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  let chromeErr = '';
  chrome.stderr.on('data', (d) => { chromeErr += d.toString(); });
  const errors = [];

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
    cdp.on('Runtime.exceptionThrown', (p) => errors.push('uncaught: ' +
      (p.exceptionDetails?.exception?.description || p.exceptionDetails?.text)));
    cdp.on('Runtime.consoleAPICalled', (p) => {
      if (p.type === 'error') errors.push('console.error: ' +
        p.args.map((a) => a.value ?? a.description ?? '').join(' '));
    });
    const net = [];
    await cdp.send('Runtime.enable');
    await cdp.send('Page.enable');
    await cdp.send('Network.enable');
    cdp.on('Network.requestWillBeSent', (p) => net.push(p.request.url));
    cdp.on('Network.loadingFailed', (p) => net.push('FAILED: ' + (p.errorText || '') + ' ' + (p.requestId || '')));
    await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1500, height: 1000, deviceScaleFactor: 1, mobile: false });

    const MODE = (process.argv.find((a) => a.startsWith('--mode='))?.slice(7)) || 'lang';

    const loaded = new Promise((res) => cdp.on('Page.loadEventFired', res));
    await cdp.send('Page.navigate', { url: URL_ });
    await Promise.race([loaded, sleep(30000)]);

    if (MODE === 'interact') {
      const runInteract = async () => cdp.eval(`(async () => {
        const wait = (f, ms) => new Promise((res, rej) => { const t0 = Date.now();
          (function poll(){ if (f()) return res(true); if (Date.now()-t0>ms) return rej(new Error('timeout')); setTimeout(poll, 100); })(); });
        // A raw file window, exactly what Ctrl+click lands on.
        openSource({ file: '_sealed_src/MeshesMerge.cs.nzk', line: 268, context: 60, mode: 'raw' });
        await wait(() => window.CsEditor.lineCount() > 0, 30000);
        const ed = window.CsEditor.editor();
        const model = ed.getModel();
        let line = -1;
        for (let i = 1; i <= model.getLineCount(); i++) {
          if (model.getLineContent(i).indexOf('FindSourceMesh') >= 0) { line = i; break; }
        }
        const col = model.getLineContent(line).indexOf('FindSourceMesh') + 1;
        const pos = new (window.CsEditor.monaco().Position)(line, col);
        const info = window.CsLang.infoAt(model, pos);
        // Spy on openSource so a Ctrl+click is observable.
        const opened = [];
        const orig = window.openSource;
        window.openSource = function (p) { opened.push(p); return orig.apply(this, arguments); };
        window.CsLang.testClick(line, col, { ctrl: true });
        await new Promise((r) => setTimeout(r, 2500));
        window.openSource = orig;
        return { mode: 'ctrl', line, col, info: info && { file: info.file, line: info.line, word: info.word }, opened };
      })()`);
      const ctrlResult = await runInteract();
      // Now Shift+click: it must open the references peek on the word.
      const shiftResult = await cdp.eval(`(async () => {
        const ed = window.CsEditor.editor();
        const model = ed.getModel();
        let line = -1;
        for (let i = 1; i <= model.getLineCount(); i++) {
          if (model.getLineContent(i).indexOf('FindSourceMesh') >= 0) { line = i; break; }
        }
        const col = model.getLineContent(line).indexOf('FindSourceMesh') + 1;
        window.CsLang.testClick(line, col, { shift: true });
        const t0 = Date.now();
        while (Date.now() - t0 < 6000) {
          const w = document.querySelector('.peekview-widget');
          if (w && w.getBoundingClientRect().width > 0) {
            const title = w.querySelector('.peekview-title');
            return { open: true, lines: line, title: title ? title.textContent.replace(/\s+/g, ' ').trim().slice(0, 90) : null,
                     rows: w.querySelectorAll('.monaco-list-row').length };
          }
          await new Promise((r) => setTimeout(r, 200));
        }
        return { open: false, line };
      })()`);
      const shot2 = await cdp.send('Page.captureScreenshot', { format: 'png' });
      writeFileSync(OUT, Buffer.from(shot2.data, 'base64'));
      console.log(JSON.stringify({ url: URL_, mode: MODE, screenshot: OUT, ctrlResult, shiftResult }, null, 2));
      return;
    }

    const boot = await cdp.eval(`(async () => {
      const wait = (f, ms) => new Promise((res, rej) => {
        const t0 = Date.now();
        (function poll() {
          if (f()) return res(true);
          if (Date.now() - t0 > ms) return rej(new Error('timeout'));
          setTimeout(poll, 100);
        })();
      });
      await wait(() => window.state && state.indexed && window.CsLang, 90000);
      window.CsEditor.load();
      await wait(() => window.CsEditor.state() === 'ready', 60000);
      openSource({ node: ${JSON.stringify(NODE)}, mode: 'doc' });
      await wait(() => window.CsEditor.lineCount() > 0, 40000);
      // Where in this document is the call site we are going to test?
      const doc = window.CsEditor;
      let docLine = -1;
      for (let i = 1; i <= doc.lineCount(); i++) {
        const rec = doc.recordAt(i);
        if (rec && rec.src && rec.src.file === ${JSON.stringify(SITE.file)} &&
            rec.src.line === ${SITE.line}) { docLine = i; break; }
      }
      const ed = doc.editor();
      if (docLine > 0) {
        const text = ed.getModel().getLineContent(docLine);
        const col = text.indexOf(${JSON.stringify(SITE.word)}) + 1;
        ed.setPosition({ lineNumber: docLine, column: Math.max(1, col) });
        ed.focus();
      }
      return { docLines: doc.lineCount(), docLine, hasLang: !!window.CsLang, models: Object.keys(window.CsLang.models()).length };
    })()`);

    console.error('boot', JSON.stringify(boot));
    // The pane is a viewer: domReadOnly must still reject real typing even
    // though readOnly is off (which is what makes F2 rename available).
    const typeBefore = await cdp.eval(`(() => { const ed = window.CsEditor.editor(); ed.focus();
      return ed.getModel().getValue().length; })()`);
    await cdp.send('Input.insertText', { text: 'ZZZZ' });
    await sleep(400);
    const typing = { before: typeBefore, after: await cdp.eval(`window.CsEditor.editor().getModel().getValue().length`) };
    const resolution = await cdp.eval(`window.CsLang.resolveAt(${JSON.stringify(SITE.file)}, ${SITE.line}, ${JSON.stringify(SITE.word)})
      .then((d) => d && d.via + ' → ' + (d.node && d.node.id))`);

    console.error('resolution', resolution);
    const refs = await cdp.eval(`window.CsLang.resolveAt(${JSON.stringify(SITE.file)}, ${SITE.line}, ${JSON.stringify(SITE.word)})
      .then((d) => window.CsLang.referencesOf(d.node.id))
      .then((s) => ({ count: s.length, sites: s.map((x) => x.file + ':' + x.line) }))`);

    const actions = await cdp.eval(`window.CsEditor.editor().getSupportedActions().map((a) => a.id)
      .filter((id) => /eek|eveal|eference|ename|efinition/i.test(id))`);

    const RUN = (id) => `(() => {
      const ed = window.CsEditor.editor();
      const a = ed.getAction(${JSON.stringify(id)});
      if (a) { a.run(); return 'action'; }
      ed.trigger('probe', ${JSON.stringify(id)}, {});
      return 'trigger';
    })()`;

    // Alt+F12 peek definition
    await cdp.eval(RUN('editor.action.peekDefinition'));
    await sleep(1500);
    console.error('refs', JSON.stringify(refs).slice(0,300));
    const peekDef = await cdp.eval(PEEK);
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
    await sleep(400);

    // Shift+F12 peek references
    await cdp.eval(`window.CsEditor.editor().setPosition({ lineNumber: ${boot.docLine}, column: window.CsEditor.editor().getModel().getLineContent(${boot.docLine}).indexOf(${JSON.stringify(SITE.word)}) + 1 }); true`);
    await cdp.eval(RUN('editor.action.referenceSearch.trigger'));
    await sleep(1800);
    console.error('peekDef', JSON.stringify(peekDef));
    const peekRefs = await cdp.eval(PEEK);

    const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
    writeFileSync(OUT, Buffer.from(shot.data, 'base64'));

    console.error('peekRefs', JSON.stringify(peekRefs));
    // Real F2, the way a user does it: cursor on the symbol, then the F2 key.
    const pos = await cdp.eval(`(() => {
      const ed = window.CsEditor.editor();
      const m = ed.getModel();
      const line = ${boot.docLine};
      const col = m.getLineContent(line).indexOf(${JSON.stringify(SITE.word)}) + 1;
      ed.setPosition({ lineNumber: line, column: Math.max(1, col) });
      ed.focus();
      const p = ed.getScrolledVisiblePosition(ed.getPosition());
      const r = ed.getDomNode().getBoundingClientRect();
      return { x: Math.round(r.left + p.left + 20), y: Math.round(r.top + p.top + 8) };
    })()`);
    await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: pos.x, y: pos.y, button: 'left', buttons: 1, clickCount: 1 });
    await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: pos.x, y: pos.y, button: 'left', buttons: 0, clickCount: 1 });
    await sleep(200);
    for (const type of ['keyDown', 'keyUp']) {
      await cdp.send('Input.dispatchKeyEvent', {
        type, key: 'F2', code: 'F2', windowsVirtualKeyCode: 113, nativeVirtualKeyCode: 113
      });
    }
    for (let i = 0; i < 24; i++) {
      const vis = await cdp.eval(`Array.from(document.querySelectorAll('.rename-box'))
        .some((b) => b.getBoundingClientRect().width > 0)`);
      if (vis) break;
      await sleep(250);
    }
    const renameWidget = await cdp.eval(`(() => {
      const boxes = Array.from(document.querySelectorAll('.rename-box'));
      return { boxes: boxes.map((b) => {
        const r = b.getBoundingClientRect();
        const i = b.querySelector('input');
        return { w: Math.round(r.width), h: Math.round(r.height),
                 value: i ? i.value : null, placeholder: i ? i.placeholder : null,
                 focused: document.activeElement === i,
                 rect: { x: Math.round(r.x), y: Math.round(r.y) } };
      }) };
    })()`);
    // Type the new name and apply it (Enter), then see what the buffers hold.
    const typed = await cdp.eval(`(() => {
      const b = Array.from(document.querySelectorAll('.rename-box'))
        .find((x) => x.getBoundingClientRect().width > 0);
      if (!b) return 'no visible rename box';
      const i = b.querySelector('input');
      if (!i) return 'no input';
      i.focus();
      i.value = 'FindSourceMeshX';
      i.dispatchEvent(new Event('input', { bubbles: true }));
      return i.value;
    })()`);
    await sleep(300);
    for (const type of ['keyDown', 'keyUp']) {
      await cdp.send('Input.dispatchKeyEvent', {
        type, key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13, text: '\r'
      });
    }
    await sleep(1500);
    const applied = await cdp.eval(`(() => {
      const out = {};
      const models = window.CsLang.models();
      Object.keys(models).forEach((f) => {
        const t = models[f].getValue();
        out[f] = { len: t.length,
                   old: t.split('FindSourceMesh').length - 1 - (t.split('FindSourceMeshX').length - 1),
                   renamed: t.split('FindSourceMeshX').length - 1,
                   head: t.slice(0, 32) };
      });
      return out;
    })()`);
    const rename = await cdp.eval(`window.CsLang.renameEditsAt(${JSON.stringify(SITE.file)}, ${SITE.line}, ${JSON.stringify(SITE.word)}, 'FindSourceMeshRenamed')
      .then((r) => ({
        edits: (r.edits || []).length,
        files: Array.from(new Set((r.edits || []).map((e) => e.resource.path))).length,
        rejected: r.rejectReason || null,
        symbol: r.symbol && r.symbol.full,
        samples: (r.edits || []).slice(0, 4).map((e) => e.resource.path + ':' + e.textEdit.range.startLineNumber + ' ' + e.textEdit.text)
      }))`);

    console.log(JSON.stringify({ url: URL_, screenshot: OUT, boot, actions, resolution, refs, typing, peekDef, peekRefs, renameWidget, typed, applied, jsChunks: net.filter((u) => /\.js$/.test(u) || u.startsWith('FAILED')), rename, errors }, null, 2));
  } catch (e) {
    console.error('PROBE FAILED:', e.message);
    if (chromeErr) console.error('chrome stderr tail:', chromeErr.slice(-600));
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
