#!/usr/bin/env node
/* probe_inheritance.mjs — prove the type hierarchy against a corpus that
 * actually has inheritance (`tools/fixtures/inherit/`).
 *
 * The default `_sealed_src` corpus has 0 `inherits` edges, so graph `bases_of`
 * / `derived_of` and the frontend type-hierarchy panel can only be stubbed
 * there. This probe boots the REAL engine over the tiny `inherit` fixture and
 * then drives the REAL page in headless Chromium:
 *
 *   1. `/api/export.json` — the engine really emitted the expected `inherits`
 *      edges (class -> base, class -> two interfaces, interface -> interface,
 *      derived -> generic base, second-level derivation).
 *   2. `window.CsHierarchy.typeTreeFor(id)` — bases / derived as the frontend
 *      reads them from `/api/node` relations, in both directions, for classes
 *      and interfaces, plus the transitive base `chain` from `/api/chain`.
 *   3. `window.CsHierarchy.implementationTargets(null, member)` — an abstract
 *      method in a base resolves to the override in the derived type.
 *
 * It is meant to be run under the committed wrapper, which owns the ports and
 * always tears the engine down:
 *
 *   tools/with_serve.sh --repo "$PWD/tools/fixtures" --root inherit -- \
 *     node tools/probe_inheritance.mjs
 *
 * Requires `chromium` on PATH. `--url` / `--cdp-port` are wired by the wrapper.
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
const PORT = Number(arg('--cdp-port', '9340'));
const CHROME = arg('--chrome', 'chromium');
const TIMEOUT_MS = Number(arg('--timeout', '180000'));
const BASE = URL_.endsWith('/') ? URL_ : URL_ + '/';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Fixture node ids (namespace Inherit.Fixture, see tools/fixtures/inherit/).
const T = {
  Shape: 'Inherit.Fixture.Shape',
  Circle: 'Inherit.Fixture.Circle',
  ColoredCircle: 'Inherit.Fixture.ColoredCircle',
  INamed: 'Inherit.Fixture.INamed',
  IShape2D: 'Inherit.Fixture.IShape2D',
  IRenderable: 'Inherit.Fixture.IRenderable',
  GenericBox: 'Inherit.Fixture.GenericBox',
  IntBox: 'Inherit.Fixture.IntBox',
};
const EXPECTED_EDGES = [
  ['Inherit.Fixture.Circle', 'Inherit.Fixture.Shape'],
  ['Inherit.Fixture.Circle', 'Inherit.Fixture.IShape2D'],
  ['Inherit.Fixture.Circle', 'Inherit.Fixture.IRenderable'],
  ['Inherit.Fixture.IShape2D', 'Inherit.Fixture.INamed'],
  ['Inherit.Fixture.IntBox', 'Inherit.Fixture.GenericBox'],
  ['Inherit.Fixture.ColoredCircle', 'Inherit.Fixture.Circle'],
];

const short = (id) => String(id || '').split('.').pop();
const setEq = (a, b) =>
  JSON.stringify([...new Set(a)].sort()) === JSON.stringify([...new Set(b)].sort());

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

/** Read one type's hierarchy through the real frontend, both directions. */
async function typeTree(cdp, id) {
  return cdp.eval(`(async () => {
    const t = await window.CsHierarchy.typeTreeFor(${JSON.stringify(id)});
    const chain = [];
    (function walk(n) {
      (n && n.children || []).forEach(function (c) {
        if (c.edge === 'inherits') chain.push(c.id || '');
        walk(c);
      });
    })(t.chain && t.chain.root);
    return {
      id: ${JSON.stringify(id)},
      node: t.node || null,
      bases: (t.bases || []).map(function (b) { return b.id; }),
      derived: (t.derived || []).map(function (d) { return d.id; }),
      chain: chain
    };
  })()`);
}

async function main() {
  // --- engine truth: real inherits edges over the fixture ------------------
  const exp = await (await fetch(`${BASE}api/export.json`)).json();
  const edges = (exp.edges || []).filter((e) => e.kind === 'inherits');
  const gotEdges = edges.map((e) => [e.source, e.target]);
  const missing = EXPECTED_EDGES.filter(
    ([s, t]) => !gotEdges.some(([gs, gt]) => gs === s && gt === t));
  check('engine emitted the fixture inherits edges',
    edges.length === EXPECTED_EDGES.length && missing.length === 0,
    `${edges.length} inherits edges · ` +
    edges.map((e) => `${short(e.source)}->${short(e.target)}`).join(', '));

  const profile = mkdtempSync(join(tmpdir(), 'csviewer-inherit-'));
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
    await cdp.send('Page.navigate', { url: BASE });
    await Promise.race([loaded, sleep(30000)]);

    // Wait for the engine's index and for the hierarchy module to register.
    await cdp.eval(`(async () => {
      const t0 = Date.now();
      while (!(window.state && state.indexed)) {
        if (Date.now() - t0 > 90000) throw new Error('boot timeout');
        await new Promise(r => setTimeout(r, 100));
      }
      while (typeof window.CsHierarchy !== 'object' || window.CsHierarchy === null) {
        if (Date.now() - t0 > 90000) throw new Error('CsHierarchy timeout');
        await new Promise(r => setTimeout(r, 50));
      }
      return true;
    })()`);
    check('real page exposes CsHierarchy', await cdp.eval(
      `typeof window.CsHierarchy.typeTreeFor === 'function' && ` +
      `typeof window.CsHierarchy.implementationTargets === 'function'`));

    // --- classes: bases and derived, both directions ----------------------
    const circle = await typeTree(cdp, T.Circle);
    check('class Circle bases/derived',
      circle.node && circle.node.kind === 'class' &&
      setEq(circle.bases.map(short), ['Shape', 'IShape2D', 'IRenderable']) &&
      setEq(circle.derived.map(short), ['ColoredCircle']),
      `bases=[${circle.bases.map(short).join(', ')}] ` +
      `derived=[${circle.derived.map(short).join(', ')}]`);

    const shape = await typeTree(cdp, T.Shape);
    check('class Shape derived (reverse direction)',
      shape.node && shape.node.kind === 'class' &&
      setEq(shape.bases.map(short), []) &&
      setEq(shape.derived.map(short), ['Circle']),
      `bases=[${shape.bases.map(short).join(', ')}] ` +
      `derived=[${shape.derived.map(short).join(', ')}]`);

    const colored = await typeTree(cdp, T.ColoredCircle);
    check('class ColoredCircle transitive base chain',
      colored.node && setEq(colored.bases.map(short), ['Circle']) &&
      colored.chain.map(short).includes('Circle') &&
      colored.chain.map(short).includes('Shape'),
      `bases=[${colored.bases.map(short).join(', ')}] ` +
      `chain=[${colored.chain.map(short).join(', ')}]`);

    // --- interfaces -------------------------------------------------------
    const ishape = await typeTree(cdp, T.IShape2D);
    check('interface IShape2D bases/derived',
      ishape.node && ishape.node.kind === 'interface' &&
      setEq(ishape.bases.map(short), ['INamed']) &&
      setEq(ishape.derived.map(short), ['Circle']),
      `bases=[${ishape.bases.map(short).join(', ')}] ` +
      `derived=[${ishape.derived.map(short).join(', ')}]`);

    const inamed = await typeTree(cdp, T.INamed);
    check('interface INamed derived (extended by interface)',
      inamed.node && inamed.node.kind === 'interface' &&
      setEq(inamed.bases.map(short), []) &&
      setEq(inamed.derived.map(short), ['IShape2D']),
      `bases=[${inamed.bases.map(short).join(', ')}] ` +
      `derived=[${inamed.derived.map(short).join(', ')}]`);

    // --- generic base -----------------------------------------------------
    const gbox = await typeTree(cdp, T.GenericBox);
    check('generic base GenericBox derived',
      gbox.node && gbox.node.kind === 'class' &&
      setEq(gbox.bases.map(short), []) &&
      setEq(gbox.derived.map(short), ['IntBox']),
      `bases=[${gbox.bases.map(short).join(', ')}] ` +
      `derived=[${gbox.derived.map(short).join(', ')}]`);

    // --- overrides --------------------------------------------------------
    const override = await cdp.eval(`(async () => {
      const d = await window.apiGet('/api/decls', { node: ${JSON.stringify(T.Shape)} });
      const rows = (d && d.decls) || [];
      const m = rows.find(r => r.name === 'Area' && r.kind === 'method');
      if (!m) return { err: 'no Shape.Area decl', names: rows.map(r => r.name) };
      const t = await window.CsHierarchy.implementationTargets(null, m);
      return { id: m.id, parent: m.parent,
               found: (t || []).map(r => ({ id: r.id, name: r.name, file: r.file, line: r.line })) };
    })()`);
    const overrideRow = (override.found || []).find((r) => r.name === 'Area');
    check('override Shape.Area -> Circle.Area',
      overrideRow && /Circle/.test(overrideRow.id) &&
      /inherit\/Shapes\.cs$/.test(overrideRow.file || ''),
      overrideRow ? `${override.id} -> ${overrideRow.id} @ ${overrideRow.file}:${overrideRow.line}`
                  : `found=${JSON.stringify(override)}`);

    const override2 = await cdp.eval(`(async () => {
      const d = await window.apiGet('/api/decls', { node: ${JSON.stringify(T.GenericBox)} });
      const rows = (d && d.decls) || [];
      const m = rows.find(r => r.name === 'Unwrap' && r.kind === 'method');
      if (!m) return { err: 'no GenericBox.Unwrap decl', names: rows.map(r => r.name) };
      const t = await window.CsHierarchy.implementationTargets(null, m);
      return { id: m.id, found: (t || []).map(r => ({ id: r.id, name: r.name, file: r.file, line: r.line })) };
    })()`);
    const override2Row = (override2.found || []).find((r) => r.name === 'Unwrap');
    check('override GenericBox.Unwrap -> IntBox.Unwrap',
      override2Row && /IntBox/.test(override2Row.id) &&
      /inherit\/Boxes\.cs$/.test(override2Row.file || ''),
      override2Row ? `${override2.id} -> ${override2Row.id} @ ${override2Row.file}:${override2Row.line}`
                   : `found=${JSON.stringify(override2)}`);

    check('no console errors', consoleErrors.length === 0,
      consoleErrors.slice(0, 3).join(' | ') || 'none');

    const failed = results.filter((r) => !r.ok).length;
    console.log(`\n${results.length - failed}/${results.length} checks passed ` +
      `(chrome ${ver.Browser}, fixture inherit)`);
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
