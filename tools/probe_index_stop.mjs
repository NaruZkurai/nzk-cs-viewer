#!/usr/bin/env node
/* probe_index_stop.mjs — acceptance for stopping an in-progress index.
 *
 * Boots a private engine with `tools/with_serve.sh` (which passes --url and
 * --cdp-port; this probe speaks plain HTTP, no Chromium needed) and proves the
 * wrong-folder escape hatch end to end:
 *
 *   1. the wrapper's startup index is up and answers /api/state;
 *   2. POST /api/index over the REAL corpus is started, Stop
 *      (POST /api/index/cancel) is pressed while it runs, and the build
 *      actually stops — the response is flagged `cancelled` and the wall time
 *      from Stop to quiescence is measured and must be small;
 *   3. the previous index still answers /api/state, with the same
 *      fileCount / node / edge counts as before (it was not overwritten by a
 *      partial result);
 *   4. a subsequent Index succeeds and re-installs the graph.
 *
 * Usage:
 *   tools/with_serve.sh -- node tools/probe_index_stop.mjs
 *   node tools/probe_index_stop.mjs --url http://127.0.0.1:8795/ \
 *     --roots 'source,build,_sealed_src'
 *
 * Options:
 *   --roots A,B,C     corpus roots the cancellable index walks (default: the
 *                     checkout's real C# corpus, minus the huge target tree)
 *   --settle-ms N     wait after starting the index before pressing Stop
 *   --stop-budget-ms N  Stop -> quiescence must stay under this
 *   --timeout-ms N    per-request timeout
 */
import process from 'node:process';

function arg(name, dflt) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
}
const URL_ = arg('--url', 'http://127.0.0.1:8795/');
const BASE = URL_.replace(/\/+$/, '');   // no trailing slash on BASE
const ROOTS = arg('--roots', 'source,build,nan_ref,testing,_bak_nzk_consolidate,_sealed_src')
  .split(',').map((s) => s.trim()).filter(Boolean);
const SETTLE_MS = Number(arg('--settle-ms', '250'));
const STOP_BUDGET_MS = Number(arg('--stop-budget-ms', '15000'));
const REQUEST_TIMEOUT_MS = Number(arg('--timeout-ms', '30000'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function api(path, { method = 'GET', body } = {}) {
  const r = await fetch(BASE + path, {
    method,
    headers: { 'Accept': 'application/json', 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: 'no-store',
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  const text = await r.text();
  let data = null;
  try { data = JSON.parse(text); } catch { /* keep null, report raw */ }
  return { status: r.status, data, text };
}

let failed = 0;
const results = [];
function check(name, ok, detail) {
  results.push({ name, ok: !!ok });
  if (!ok) failed++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? '  — ' + detail : ''}`);
}

async function main() {
  console.log(`probe_index_stop: base=${BASE} roots=${JSON.stringify(ROOTS)} ` +
              `settle=${SETTLE_MS}ms stopBudget=${STOP_BUDGET_MS}ms`);

  // 1. The wrapper's startup index (default `_sealed_src`) is up.
  const state0 = await api('/api/state');
  check('/api/state answers', state0.status === 200 && state0.data,
    `status=${state0.status}`);
  const before = state0.data || {};
  check('previous index is active', before.indexed === true,
    `indexed=${before.indexed} root=${before.root}`);
  check('previous index has a graph', (before.counts || {}).nodes > 0,
    `nodes=${(before.counts || {}).nodes} edges=${(before.counts || {}).edges}`);

  // 2. Start a real index and Stop it mid-flight.
  const tStart = Date.now();
  const indexPromise = api('/api/index', { method: 'POST', body: { roots: ROOTS } });
  await sleep(SETTLE_MS);
  const tStop = Date.now();
  const cancel = await api('/api/index/cancel', { method: 'POST', body: {} });
  const stopWall = Date.now() - tStop;
  const indexRes = await indexPromise;
  const tQuiesced = Date.now();
  const stopToQuiescence = tQuiesced - tStop;
  const buildWall = tQuiesced - tStart;

  check('POST /api/index/cancel answers', cancel.status === 200 && cancel.data,
    `status=${cancel.status} body=${cancel.text.slice(0, 120)}`);
  check('cancel is acknowledged', (cancel.data || {}).cancelled === true);
  check('index POST succeeded (not a 5xx)',
    indexRes.status === 200 && !!indexRes.data,
    `status=${indexRes.status} body=${indexRes.text.slice(0, 160)}`);
  check('build stopped instead of installing a partial graph',
    (indexRes.data || {}).cancelled === true,
    `body=${indexRes.text.slice(0, 200)}`);
  check(`Stop -> quiescence under ${STOP_BUDGET_MS}ms`,
    stopToQuiescence <= STOP_BUDGET_MS,
    `measured ${stopToQuiescence}ms (start->cancel ${tStop - tStart}ms, start->quiesce ${buildWall}ms)`);

  // 3. The previous index survived: same counts, still indexed.
  const state1 = await api('/api/state');
  const after = state1.data || {};
  check('/api/state still answers after the cancel',
    state1.status === 200 && after.indexed === true,
    `status=${state1.status} indexed=${after.indexed}`);
  check('previous fileCount preserved',
    after.fileCount === before.fileCount,
    `before=${before.fileCount} after=${after.fileCount}`);
  check('previous node count preserved (no partial overwrite)',
    (after.counts || {}).nodes === (before.counts || {}).nodes,
    `before=${(before.counts || {}).nodes} after=${(after.counts || {}).nodes}`);
  check('previous edge count preserved',
    (after.counts || {}).edges === (before.counts || {}).edges,
    `before=${(before.counts || {}).edges} after=${(after.counts || {}).edges}`);
  check('previous root preserved',
    after.root === before.root,
    `before=${before.root} after=${after.root}`);

  // 4. A fresh Index still works after a cancelled one.
  const reindex = await api('/api/index', { method: 'POST', body: { roots: [before.root] } });
  const state2 = await api('/api/state');
  const final = state2.data || {};
  check('subsequent Index succeeds',
    reindex.status === 200 && reindex.data && reindex.data.cancelled !== true &&
      reindex.data.indexed === true && final.indexed === true,
    `status=${reindex.status} indexed=${final.indexed} nodes=${(final.counts || {}).nodes}`);
  check('subsequent Index restored the startup corpus',
    (final.counts || {}).nodes === (before.counts || {}).nodes &&
      final.fileCount === before.fileCount,
    `nodes=${(final.counts || {}).nodes} files=${final.fileCount}`);

  console.log(`\n${results.filter((r) => r.ok).length}/${results.length} checks passed`);
  console.log(`STOP_WALL_MS=${stopWall} STOP_TO_QUIESCENCE_MS=${stopToQuiescence} BUILD_WALL_MS=${buildWall}`);
  process.exitCode = failed ? 1 : 0;
}

main().catch((e) => {
  console.error(JSON.stringify({ ok: false, error: String((e && e.stack) || e) }, null, 2));
  process.exitCode = 1;
});
