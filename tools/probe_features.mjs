#!/usr/bin/env node
/* probe_features.mjs — end-to-end acceptance for the v2 editor features.
 *
 * Drives the REAL viewer (frontend + Rust engine over `csviewer --serve`) in
 * headless Chromium and exercises every feature module through its own test
 * hooks, the way a user would reach it:
 *
 *   hover           window.CsHover.provideHover
 *   outline/symbols window.CsSymbols.provideDocumentSymbols / folding / highlight
 *   semantic tokens window.CsSemantics.provideDocumentSemanticTokens
 *   CodeLens        window.CsCodeLens.provider.provideCodeLenses
 *   call hierarchy  window.CsHierarchy.treeFor
 *   quick-open      window.CsQuickOpen.open/query/activate
 *   breadcrumbs     window.CsNav.crumbs/buildCrumbs
 *   problems        window.CsProblems.render/items
 *
 * It also asserts the page has no uncaught exception and no console.error.
 *
 * Usage:
 *   node tools/probe_features.mjs [--url http://127.0.0.1:8795/]
 *                                 [--node NZK.Core.NZKNaNimateWeightNormalizer]
 *                                 [--cdp-port 9340] [--chrome chromium]
 *
 * Requires a running `csviewer --serve` (see README "Development").
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
const TIMEOUT_MS = Number(arg('--timeout', '180000'));
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
  const profile = mkdtempSync(join(tmpdir(), 'csviewer-features-'));
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
    const loaded = new Promise((res) => cdp.on('Page.loadEventFired', res));
    await cdp.send('Page.navigate', { url: URL_ });
    await Promise.race([loaded, sleep(30000)]);

    // boot the engine + Monaco
    await cdp.eval(`(async () => {
      const t0 = Date.now();
      while (!(window.state && state.indexed)) {
        if (Date.now() - t0 > 90000) throw new Error('boot timeout');
        await new Promise(r => setTimeout(r, 100));
      }
      return state.indexed;
    })()`);
    await cdp.eval(`window.CsEditor.load(); 'ok'`);
    await cdp.eval(`(async () => {
      const t0 = Date.now();
      while (window.CsEditor.state() !== 'ready') {
        if (Date.now() - t0 > 60000) throw new Error('monaco timeout');
        await new Promise(r => setTimeout(r, 100));
      }
      return window.CsEditor.state();
    })()`);

    // every module must have registered itself
    for (const g of ['CsHover', 'CsSymbols', 'CsSemantics', 'CsCodeLens',
                     'CsHierarchy', 'CsQuickOpen', 'CsNav', 'CsProblems']) {
      const present = await cdp.eval(`typeof window.${g} === 'object' && window.${g} !== null`);
      check(`${g} loaded`, present);
    }

    // open a real partial type document (2 declaration sites) and wait for the
    // feature modules to settle on the new model
    const opened = await cdp.eval(`(async () => {
      openSource({ node: ${JSON.stringify(NODE)} });
      const t0 = Date.now();
      while (window.CsEditor.lineCount() < 10) {
        if (Date.now() - t0 > 30000) throw new Error('doc timeout');
        await new Promise(r => setTimeout(r, 100));
      }
      await new Promise(r => setTimeout(r, 800));
      return { lines: window.CsEditor.lineCount(),
               model: !!window.CsEditor.model(),
               title: document.getElementById('sourceTitle').textContent };
    })()`);
    check('document opened', opened.lines > 100,
      `${opened.lines} lines · ${opened.title}`);

    // ---- hover -------------------------------------------------------------
    const hover = await cdp.eval(`(async () => {
      const model = window.CsEditor.model();
      // Position on the first real declaration: take its outline symbol's start
      // line and find the name in the line text.
      const tree = await window.CsSymbols.provideDocumentSymbols(model) || [];
      let sym = null;
      (function walk(list) { (list || []).forEach(x => { if (!sym && x.range) sym = x; walk(x.children); }); })(tree);
      if (!sym) return { err: 'no symbols' };
      const name = (sym.name || '').split('(')[0].split('.').pop();
      // Put the cursor on the first model line that actually contains the name.
      // (A synthesized doc repeats the declaration's header as head lines, so
      // an outline range and a raw text search need not agree on the line.)
      let line = 0;
      for (let i = 1; i <= model.getLineCount(); i++) {
        if (model.getLineContent(i).indexOf(name) >= 0) { line = i; break; }
      }
      if (!line) return { err: 'name not found in model', name };
      const col = model.getLineContent(line).indexOf(name) + 1;
      const h = await window.CsHover.provideHover(model, { lineNumber: line, column: col });
      const md = h && h.contents ? h.contents.map(c => c.value || '').join('\\n') : '';
      const info = window.CsLang.infoAt(model, { lineNumber: line, column: col });
      return { line, col, name, hasHover: !!h, md,
               text: model.getLineContent(line).slice(0, 100),
               info: info && { file: info.file, line: info.line, word: info.word } };
    })()`);
    check('hover returns a card', hover.hasHover && hover.md.length > 10,
      hover.hasHover ? `line ${hover.line} "${hover.name}" -> ${JSON.stringify(hover.md.slice(0, 90))}`
                     : `null · sym="${hover.name}" line=${hover.line} col=${hover.col} ` +
                       `text=${JSON.stringify(hover.text)} info=${JSON.stringify(hover.info)}`);

    // ---- document symbols / outline ---------------------------------------
    const syms = await cdp.eval(`(async () => {
      const model = window.CsEditor.model();
      const s = await window.CsSymbols.provideDocumentSymbols(model);
      const flat = [];
      (function walk(list, depth) { (list || []).forEach(x => { flat.push(x); walk(x.children, depth + 1); }); })(s, 0);
      return { top: (s || []).length, total: flat.length,
               nested: flat.some(x => x.children && x.children.length),
               kinds: Array.from(new Set(flat.map(x => x.kind))).length };
    })()`);
    check('document symbols', syms.total > 5,
      `${syms.top} top-level, ${syms.total} total, nested=${syms.nested}, ${syms.kinds} kinds`);

    const fold = await cdp.eval(`(async () => {
      const r = await window.CsSymbols.provideFoldingRanges(window.CsEditor.model());
      return (r || []).length;
    })()`);
    check('folding ranges', fold > 0, `${fold} ranges`);

    // ---- semantic tokens ---------------------------------------------------
    const sem = await cdp.eval(`(async () => {
      const d = await window.CsSemantics.provideDocumentSemanticTokens(window.CsEditor.model());
      return { data: (d && d.data) ? d.data.length : 0,
               legend: (window.CsSemantics.legend && window.CsSemantics.legend.tokenTypes || []).length };
    })()`);
    check('semantic tokens', sem.data > 0 && sem.data % 5 === 0,
      `${sem.data} ints, ${sem.legend} token types`);

    // ---- CodeLens ----------------------------------------------------------
    const lens = await cdp.eval(`(async () => {
      const p = window.CsCodeLens.provider;
      const r = await p.provideCodeLenses(window.CsEditor.model());
      const list = Array.isArray(r) ? r : (r && r.lenses) || [];
      const first = list[0];
      return { n: list.length, title: first && first.command && first.command.title };
    })()`);
    check('CodeLens placed', lens.n > 0, `${lens.n} lenses · ${lens.title}`);

    // ---- call hierarchy ----------------------------------------------------
    const chain = await cdp.eval(`(async () => {
      const model = window.CsEditor.model();
      const info = window.CsLang.infoAt(model, { lineNumber: 1, column: 1 });
      const decls = await window.CsSymbols.declsFor(model);
      const method = (decls || []).find(d => d.kind === 'method');
      if (!method) return { err: 'no method decl' };
      const t = await window.CsHierarchy.treeFor(method.id, 'forward', { depth: 2 });
      const hops = JSON.stringify(t || {});
      const n = (t && (t.rows || t.hops || t.flat || t.children)) || [];
      return { n: Array.isArray(n) ? n.length : 0, bytes: hops.length, id: method.id };
    })()`);
    check('call hierarchy tree', chain.n > 0, `${chain.n} rows for ${chain.id}`);

    // ---- quick-open --------------------------------------------------------
    const qp = await cdp.eval(`(async () => {
      const rows = await window.CsQuickOpen.query('Normalize', 'symbols');
      const list = Array.isArray(rows) ? rows : (rows && rows.rows) || [];
      window.CsQuickOpen.open('symbols', 'Normalize');
      await new Promise(r => setTimeout(r, 500));
      const opened = window.CsQuickOpen.isOpen();
      const shown = document.querySelectorAll('#cs-quickopen .cs-qo-row').length;
      window.CsQuickOpen.close();
      return { n: list.length, opened, shown };
    })()`);
    check('quick-open', qp.n > 0 && qp.opened && qp.shown > 0,
      `${qp.n} rows, panel opened, ${qp.shown} rendered`);

    // ---- breadcrumbs -------------------------------------------------------
    const crumbs = await cdp.eval(`(async () => {
      await new Promise(r => setTimeout(r, 500));
      const c = window.CsNav.crumbs();
      const bar = document.getElementById('cs-nav-crumbs');
      return { n: (c || []).length, mounted: !!bar,
               labels: (c || []).map(x => x.label || x.text || '').slice(0, 6) };
    })()`);
    check('breadcrumbs', crumbs.n > 0 && crumbs.mounted,
      `${crumbs.n} segments: ${crumbs.labels.join(' > ')}`);

    // ---- nav history -------------------------------------------------------
    const nav = await cdp.eval(`(async () => {
      const before = window.CsNav.history().length;
      // A different, definitely-indexed node -> a different model key. (Opening
      // the node already on screen must NOT add a history entry, so pick one
      // that differs from the document under test.)
      const alt = (${JSON.stringify(NODE)}.indexOf('BoneHolder') >= 0)
        ? 'NZK.Core.NZKNaNimateWeightNormalizer'
        : 'NZK.Core.NZKNaNimateBoneHolder';
      openSource({ node: alt });
      await new Promise(r => setTimeout(r, 900));
      const after = window.CsNav.history().length;
      const back = window.CsNav.back();
      await new Promise(r => setTimeout(r, 700));
      return { before, after, alt, back: !!back, idx: window.CsNav.historyIndex() };
    })()`);
    check('pane nav history', nav.after > nav.before, `${nav.before} -> ${nav.after} via ${nav.alt}`);

    // ---- problems ----------------------------------------------------------
    const probs = await cdp.eval(`(async () => {
      window.CsProblems.render(['src/Foo.cs:12: parser: unterminated string',
                                'plain scope guard tripped']);
      const items = window.CsProblems.items();
      const section = !!document.getElementById('problemsSection');
      const rows = document.querySelectorAll('#problemsSection .cs-problem').length;
      window.CsProblems.render([]);
      return { n: items.length, section, rows,
               empty: document.querySelector('#problemsSection .list-empty') ? true : false,
               loc: items[0] && items[0].file + ':' + items[0].line };
    })()`);
    check('problems list', probs.n === 2 && probs.section,
      `${probs.n} parsed, ${probs.rows} rows, loc=${probs.loc}`);

    // Whole page must be clean.
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
