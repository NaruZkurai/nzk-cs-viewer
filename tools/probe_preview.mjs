#!/usr/bin/env node
/* probe_preview.mjs — prove the app-side change-preview panel (M3 slice 2).
 *
 * `POST /api/preview` is the app-facing twin of the MCP-only `preview_change`
 * tool (intercepted in `Engine::handle`, like `/api/edit`). This probe boots the
 * real engine + frontend through `tools/with_serve.sh` and checks, over plain
 * HTTP (no CDP, no daemon — with_serve.sh owns and tears down the server):
 *
 *   1. the served page exposes the panel: the Preview button, the modal, and
 *      /static/preview.js;
 *   2. preview.js is served and drives '/api/preview';
 *   3. GET /api/preview returns the read-only op list (rename/retype) as a
 *      plain body, not the MCP text envelope;
 *   4. a rename preview on a real declaration returns ok, readOnly, applied:false,
 *      writes:[], a current/proposed signature, edits and call sites with line
 *      numbers — and two identical calls are byte-identical (deterministic);
 *   5. a retype preview reports the declared type it would replace;
 *   6. refusals (unknown symbol, bad `to`) are ordinary ok:false bodies on a 200,
 *      never a transport error and never a write.
 *
 * Usage:
 *   tools/with_serve.sh -- node tools/probe_preview.mjs
 */
import process from 'node:process';

function arg(name, dflt) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
}

const URL_ = arg('--url', 'http://127.0.0.1:8799/').replace(/\/+$/, '');

let failures = 0;
function check(name, cond, detail) {
  const ok = !!cond;
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (detail ? ' — ' + detail : ''));
}

function readBody(res) {
  return res.text().then(function (text) {
    let json = null;
    try { json = JSON.parse(text); } catch (e) { /* keep raw text */ }
    return { res: res, text: text, json: json };
  });
}

function get(path) {
  return fetch(URL_ + path, { headers: { Accept: 'application/json' }, cache: 'no-store' }).then(readBody);
}

function post(path, body) {
  return fetch(URL_ + path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
    body: JSON.stringify(body),
  }).then(readBody);
}

/** A public declaration id to preview, from the graph itself. */
async function firstDeclaration() {
  const defs = await get('/api/definitions?scope=public&limit=40');
  const rows = (defs.json && defs.json.definitions) || [];
  for (const r of rows) {
    if (r && typeof r.id === 'string' && r.id && r.kind !== 'namespace') return r;
  }
  return null;
}

async function main() {
  // 1. the page exposes the panel
  const page = await get('/');
  check('index.html served', page.res.status === 200 && /<html/i.test(page.text), 'status=' + page.res.status);
  check('page has a Preview button', page.text.indexOf('id="previewBtn"') >= 0);
  check('page has the preview modal', page.text.indexOf('id="previewModal"') >= 0);
  check('page has a Preview action button', page.text.indexOf('id="previewRun"') >= 0);
  check('page loads preview.js', page.text.indexOf('/static/preview.js') >= 0);

  // 2. preview.js talks to the route, read-only
  const js = await get('/static/preview.js');
  check('preview.js served', js.res.status === 200 && js.text.indexOf('window.CSPreview') >= 0,
    'status=' + js.res.status);
  check('preview.js drives /api/preview',
    js.text.indexOf("'/api/preview'") >= 0 || js.text.indexOf('"/api/preview"') >= 0);
  check('preview.js has no apply path', !/apply\s*:\s*true/.test(js.text));
  check('preview.js renders writes:none note', js.text.indexOf('nothing is written') >= 0 ||
    js.text.indexOf('wrote nothing') >= 0);

  // 3. GET /api/preview: the read-only op list
  const listing = await get('/api/preview');
  const ops = (listing.json && listing.json.ops) || [];
  check('GET /api/preview ok', listing.res.status === 200 && listing.json && listing.json.ok === true,
    'status=' + listing.res.status);
  check('GET lists rename + retype', ops.indexOf('rename') >= 0 && ops.indexOf('retype') >= 0,
    'ops=' + ops.join(','));
  check('GET says readOnly', listing.json && listing.json.readOnly === true);
  check('GET is not the MCP text envelope',
    listing.json && !Array.isArray(listing.json.content), JSON.stringify(listing.json).slice(0, 120));

  // 4. a real rename preview
  const decl = await firstDeclaration();
  check('found a public declaration to preview', !!decl, decl ? decl.id : 'none');
  if (!decl) return finish();

  const args = { node: decl.id, op: 'rename', to: 'PreviewedName' };
  const one = await post('/api/preview', args);
  const two = await post('/api/preview', args);
  check('POST /api/preview rename ok', one.res.status === 200 && one.json && one.json.ok === true,
    'status=' + one.res.status + ' body=' + one.text.slice(0, 160));
  if (!one.json || one.json.ok !== true) return finish();
  check('preview is read-only by construction',
    one.json.readOnly === true && one.json.applied === false && Array.isArray(one.json.writes) &&
    one.json.writes.length === 0,
    'readOnly=' + one.json.readOnly + ' applied=' + one.json.applied);
  check('preview reports current + proposed signatures',
    typeof one.json.currentSignature === 'string' &&
    typeof one.json.proposedSignature === 'string' &&
    one.json.currentSignature.length > 0 &&
    one.json.proposedSignature.indexOf('PreviewedName') >= 0,
    JSON.stringify(one.json.proposedSignature));
  check('preview resolves the declaration', !!one.json.declaration && one.json.declaration.id === decl.id,
    JSON.stringify(one.json.declaration));
  check('preview counts are present and consistent',
    one.json.counts && typeof one.json.counts.edits === 'number' &&
    one.json.counts.edits === (one.json.edits || []).length &&
    one.json.counts.callSites === (one.json.callSites || []).length,
    JSON.stringify(one.json.counts));
  check('every edit carries file:line and before/after',
    (one.json.edits || []).every(function (e) {
      return typeof e.file === 'string' && e.line >= 1 &&
        typeof e.before === 'string' && typeof e.after === 'string';
    }), JSON.stringify((one.json.edits || []).slice(0, 2)));
  check('every call site carries a line number',
    (one.json.callSites || []).every(function (s) {
      return typeof s.file === 'string' && s.line >= 1 && typeof s.node === 'string';
    }), JSON.stringify((one.json.callSites || []).slice(0, 2)));
  check('two identical previews are byte-identical', one.text === two.text,
    'len ' + one.text.length + ' vs ' + two.text.length);
  check('preview never writes on a repeat', two.json && two.json.writes &&
    two.json.writes.length === 0 && two.json.applied === false);

  // 5. a retype preview, when the declaration has a declared type
  const rt = await post('/api/preview', { node: decl.id, op: 'retype', to: 'System.Int64' });
  if (rt.json && rt.json.ok === true) {
    check('POST /api/preview retype reports the replaced type',
      typeof rt.json.currentType === 'string' && rt.json.currentType.length > 0 &&
      rt.json.proposedSignature.indexOf('System.Int64') >= 0,
      'currentType=' + rt.json.currentType);
    check('retype is read-only too', rt.json.writes.length === 0 && rt.json.applied === false);
  } else {
    check('POST /api/preview retype refuses cleanly when unsupported',
      rt.res.status === 200 && rt.json && rt.json.ok === false,
      JSON.stringify(rt.json).slice(0, 160));
  }

  // 6. refusals are 200 bodies, never transport errors
  const unknown = await post('/api/preview', { node: 'N.Nope(Nope)', op: 'rename', to: 'X' });
  check('unknown symbol is a 200 ok:false body',
    unknown.res.status === 200 && unknown.json && unknown.json.ok === false &&
    /unknown symbol|does not resolve/.test(unknown.json.error || ''),
    'status=' + unknown.res.status + ' error=' + (unknown.json && unknown.json.error));
  const bad = await post('/api/preview', { node: decl.id, op: 'rename', to: 'not a name' });
  check('bad `to` is a 200 ok:false body',
    bad.res.status === 200 && bad.json && bad.json.ok === false,
    'status=' + bad.res.status + ' error=' + (bad.json && bad.json.error));
  const badop = await post('/api/preview', { node: decl.id, op: 'teleport', to: 'X' });
  check('bad `op` is a 200 ok:false body',
    badop.res.status === 200 && badop.json && badop.json.ok === false);

  return finish();
}

function finish() {
  if (failures) {
    console.error('probe_preview: ' + failures + ' check(s) failed');
    process.exit(1);
  }
  console.log('PASS: /api/preview + the Preview panel are read-only, deterministic and refuse loudly');
  process.exit(0);
}

main().catch(function (err) {
  console.error('probe_preview: ' + (err && err.message ? err.message : err));
  process.exit(1);
});
