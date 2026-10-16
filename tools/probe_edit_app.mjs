#!/usr/bin/env node
/* probe_edit_app.mjs — prove the app/agent edit parity route exists and is safe.
 *
 * The refactor/query tools live in core/src/edit.rs and were reachable only
 * over MCP. `GET/POST /api/edit` (intercepted in the host's Engine::handle) lets
 * the app drive the same named operations an agent can. This probe boots the
 * real engine + frontend through `tools/with_serve.sh` and checks:
 *
 *   1. GET /api/edit lists the core edit::TOOLS names (read from the route, so
 *      the panel never hardcodes them); the count is self-consistent;
 *   2. the served page exposes the panel (button + modal + /static/edit.js) and
 *      edit.js is the module that talks to the route;
 *   3. a write tool in DRY-RUN returns ok:true with applied:false and
 *      written:[] — and the target file's sha is identical on a second call, so
 *      nothing was touched;
 *   4. an unknown tool is a normal ok:false body, not a 500;
 *   5. GET/POST /api/analyze: the op list, an honest `parse_check` gate (a
 *      skipped gate is ok:false with a reason, never a pass), the `parse_check`
 *      `files`/`timeout` arguments, the `dedupe` report shape, and an unknown op
 *      refusal;
 *   6. grep: counts + file:line:col rows, count_only, determinism, an empty
 *      result and an invalid pattern;
 *   7. the reshape tools (`reformat`, `functionize`) and `move_decls` answer as
 *      tool bodies (dry-run, no MCP envelope), and reformat accepts the panel's
 *      {file, apply:false} skeleton;
 *   8. POST /api/reorg action=plan returns the plan shape (the fourth reorg
 *      action the panel exposes: plan/verify/zip/write).
 *
 * HTTP-only (Node's fetch), no CDP, no daemon: with_serve.sh owns the server and
 * tears it down. Exit status 0 only when every check passes.
 *
 * Sharding: the whole sequence is ~11 s because `dedupe` alone is ~5 s, over the
 * repo's 10 s per-unit rule. `--shard edit|analyze|reorg` runs one bounded
 * subset per process (each its own verify_all.sh unit); the union is the
 * original check set. The default `all` keeps the single-sequence behaviour for
 * interactive use.
 *
 * Usage:
 *   tools/with_serve.sh -- node tools/probe_edit_app.mjs                    # all
 *   tools/with_serve.sh -- node tools/probe_edit_app.mjs --shard edit
 *   tools/with_serve.sh -- node tools/probe_edit_app.mjs --shard analyze
 *   tools/with_serve.sh -- node tools/probe_edit_app.mjs --shard reorg
 *   node tools/probe_edit_app.mjs --url http://127.0.0.1:8799/
 */
import process from 'node:process';

function arg(name, dflt) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
}

const URL_ = arg('--url', 'http://127.0.0.1:8799/').replace(/\/+$/, '');
const TIMEOUT_MS = Number(arg('--timeout', '28000'));
const SHARDS = ['all', 'edit', 'analyze', 'reorg'];
const SHARD = arg('--shard', 'all');
if (SHARDS.indexOf(SHARD) < 0) {
  console.error('probe_edit_app: --shard must be one of ' + SHARDS.join('|') + ', got ' + SHARD);
  process.exit(2);
}

// The tools the route must name. `GET /api/edit` reads core edit::TOOLS, so this
// is a coverage check (all present), not an exact-count freeze: a tool added to
// TOOLS later must flow through without editing this probe. grep and the two
// reshape tools land here, so the app can reach every action MCP can.
const EXPECTED = [
  'fuzzy_search', 'resolve_chain', 'code_block',
  'rename_symbol', 'qualify_symbols', 'expand_namespace',
  'write_file', 'apply_edits', 'select_decls', 'move_decls',
  'grep', 'copy_path', 'reformat', 'functionize',
];

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

/** A repo-relative indexed file, for the analyze shard which does not run the
 *  edit shard's select_decls section. */
async function firstIndexedFile() {
  const sel = await post('/api/edit', { tool: 'select_decls', args: { like: ['a'], limit: 5 } });
  const results = (sel.json && sel.json.results) || [];
  return results.length ? results[0].file : null;
}

async function main() {
  const edit = SHARD === 'all' || SHARD === 'edit';
  const analyze = SHARD === 'all' || SHARD === 'analyze';
  const reorg = SHARD === 'all' || SHARD === 'reorg';
  let file = null;

  // ---- edit shard ----------------------------------------------------------
  if (edit) {
    // 1. the op list comes from edit::TOOLS
    const ops = await get('/api/edit');
    const tools = ops.json && Array.isArray(ops.json.tools) ? ops.json.tools : [];
    check('GET /api/edit ok', ops.res.status === 200 && ops.json && ops.json.ok === true,
      'status=' + ops.res.status + ' ok=' + (ops.json && ops.json.ok));
    check('ops list is non-empty', tools.length >= EXPECTED.length && tools.every(function (t) { return typeof t === 'string' && t.length > 0; }),
      'got ' + tools.length + ': ' + tools.join(','));
    const missing = EXPECTED.filter(function (n) { return tools.indexOf(n) < 0; });
    check('ops list names every shipped edit tool', missing.length === 0,
      missing.length ? 'missing ' + missing.join(',') : tools.join(','));
    check('ops count field matches', ops.json && ops.json.count === tools.length);
    check('GET is not the MCP text envelope',
      ops.json && !Array.isArray(ops.json.content), JSON.stringify(ops.json).slice(0, 120));

    // 2. the page exposes the panel
    const page = await get('/');
    check('index.html served', page.res.status === 200 && /<html/i.test(page.text));
    check('page has edit button', page.text.indexOf('id="editBtn"') >= 0);
    check('page has edit panel', page.text.indexOf('id="editModal"') >= 0);
    check('page has a dry-run button', page.text.indexOf('id="editDry"') >= 0);
    check('page has an apply button', page.text.indexOf('id="editApply"') >= 0);
    check('page loads edit.js', page.text.indexOf('/static/edit.js') >= 0);

    const js = await get('/static/edit.js');
    check('edit.js served', js.res.status === 200 && js.text.indexOf('window.CSEdit') >= 0,
      'status=' + js.res.status);
    check('edit.js drives /api/edit', js.text.indexOf("'/api/edit'") >= 0 || js.text.indexOf('"/api/edit"') >= 0);
    check('page has a Validate button', page.text.indexOf('id="editValidate"') >= 0);
    check('page has a Duplicates button', page.text.indexOf('id="editDedupe"') >= 0);
    check('edit.js drives /api/analyze', js.text.indexOf("'/api/analyze'") >= 0 || js.text.indexOf('"/api/analyze"') >= 0);
    check('edit.js renders a skipped gate as an error',
      js.text.indexOf('data.skipped') >= 0 && js.text.indexOf("'Skipped: '") >= 0);
    check('edit.js carries an arg skeleton for grep',
      js.text.indexOf('grep:') >= 0 && js.text.indexOf('count_only') >= 0);
    check('edit.js carries arg skeletons for the reshape tools',
      js.text.indexOf('reformat:') >= 0 && js.text.indexOf('functionize:') >= 0 &&
      js.text.indexOf("file: '<repo-relative file>'") >= 0 && js.text.indexOf('members:') >= 0);
    check('edit.js passes the args box to parse_check',
      /analyze\('parse_check',\s*args/.test(js.text));
    check('edit.js passes the args box to dedupe',
      /analyze\('dedupe',\s*args/.test(js.text));

    // 2c. grep: pattern search with counts, scopes and refusals
    const GARGS = { pattern: 'public static', limit: 50 };
    const gre = await post('/api/edit', { tool: 'grep', args: GARGS });
    check('POST grep returns counts and match rows',
      gre.res.status === 200 && !!gre.json && gre.json.ok === true &&
      gre.json.total_matches > 0 && gre.json.file_count > 0 &&
      Array.isArray(gre.json.files) && gre.json.files.length > 0 &&
      typeof gre.json.files[0].count === 'number' &&
      Array.isArray(gre.json.files[0].matches) && gre.json.files[0].matches.length > 0,
      JSON.stringify(gre.json && { total: gre.json.total_matches, files: gre.json.file_count }).slice(0, 160));
    const greRow = (gre.json && gre.json.files && gre.json.files[0] && gre.json.files[0].matches && gre.json.files[0].matches[0]) || {};
    check('grep rows carry file/line/col/text',
      typeof greRow.line === 'number' && typeof greRow.col === 'number' &&
      typeof greRow.text === 'string' && greRow.text.length > 0,
      JSON.stringify(greRow).slice(0, 160));
    const greAgain = await post('/api/edit', { tool: 'grep', args: GARGS });
    check('grep is deterministic', JSON.stringify(greAgain.json) === JSON.stringify(gre.json));
    const greCount = await post('/api/edit', { tool: 'grep', args: { pattern: 'public static', count_only: true, limit: 50 } });
    check('grep count_only counts without rows',
      !!greCount.json && greCount.json.ok === true && greCount.json.count_only === true &&
      greCount.json.total_matches > 0 &&
      (greCount.json.files || []).every(function (f) { return Array.isArray(f.matches) && f.matches.length === 0; }),
      JSON.stringify(greCount.json && { total: greCount.json.total_matches, files: greCount.json.file_count }).slice(0, 160));
    const greNone = await post('/api/edit', { tool: 'grep', args: { pattern: 'zzz_no_such_symbol_zzz' } });
    check('grep with no matches is ok:true and empty',
      !!greNone.json && greNone.json.ok === true && greNone.json.total_matches === 0 &&
      greNone.json.file_count === 0 && (greNone.json.files || []).length === 0,
      JSON.stringify(greNone.json).slice(0, 160));
    const greBad = await post('/api/edit', { tool: 'grep', args: { pattern: '(' } });
    check('grep with an invalid pattern is a refusal body, not a 500',
      greBad.res.status === 200 && !!greBad.json && greBad.json.ok === false &&
      typeof greBad.json.error === 'string' && greBad.json.error.indexOf('invalid pattern') >= 0,
      JSON.stringify(greBad.json).slice(0, 160));

    // 2d. the reshape tools answer as tool bodies (dry-run)
    for (const op of ['reformat', 'functionize']) {
      const r = await post('/api/edit', { tool: op, args: {} });
      check('POST ' + op + ' is a tool body, not an MCP envelope',
        r.res.status === 200 && !!r.json && typeof r.json.ok === 'boolean' && !Array.isArray(r.json.content),
        JSON.stringify(r.json).slice(0, 160));
    }

    // 2e. move_decls still defaults to a dry run
    const mv = await post('/api/edit', {
      tool: 'move_decls',
      args: { like: ['FolderFor'], newfile: 'NZK.Probe.cs', dir: 'Assets/nzk/probe' },
    });
    check('POST move_decls defaults to a non-writing body',
      mv.res.status === 200 && !!mv.json && typeof mv.json.ok === 'boolean' && mv.json.applied !== true,
      JSON.stringify(mv.json).slice(0, 160));

    // 3. a dry-run returns ok:true and writes nothing
    const sel = await post('/api/edit', { tool: 'select_decls', args: { like: ['a'], limit: 5 } });
    check('POST select_decls (read tool) ok', sel.json && sel.json.ok === true,
      JSON.stringify(sel.json).slice(0, 200));
    const results = (sel.json && sel.json.results) || [];
    check('select_decls found an indexed file', results.length > 0, 'results=' + results.length);

    if (results.length) {
      file = results[0].file;
      const body = {
        tool: 'write_file',
        args: { path: file, content: '// probe_edit_app dry run — never written\n' },
      };
      const dry = await post('/api/edit', body);
      check('write_file dry-run ok:true', dry.json && dry.json.ok === true,
        JSON.stringify(dry.json).slice(0, 200));
      check('dry-run reports applied:false', dry.json && dry.json.applied === false,
        'applied=' + (dry.json && dry.json.applied));
      check('dry-run reports written:[]', dry.json && Array.isArray(dry.json.written) && dry.json.written.length === 0,
        'written=' + JSON.stringify(dry.json && dry.json.written));
      check('dry-run reports the current sha', dry.json && typeof dry.json.old_sha256 === 'string' && dry.json.old_sha256.length === 64,
        'old_sha256=' + (dry.json && dry.json.old_sha256));
      check('dry-run sha differs from the proposed content',
        dry.json && dry.json.old_sha256 !== dry.json.new_sha256);

      const again = await post('/api/edit', body);
      check('file untouched after the dry-run',
        again.json && again.json.ok === true && again.json.old_sha256 === (dry.json && dry.json.old_sha256),
        'before=' + (dry.json && dry.json.old_sha256) + ' after=' + (again.json && again.json.old_sha256));

      // 3a. the reshape tools accept the panel's arg skeleton
      const rf = await post('/api/edit', { tool: 'reformat', args: { file: file, apply: false } });
      check('reformat dry-run accepts {file, apply:false} and writes nothing',
        rf.res.status === 200 && !!rf.json && typeof rf.json.ok === 'boolean' &&
        rf.json.error !== 'argument "file" is required' &&
        (rf.json.ok !== true || (rf.json.applied !== true && typeof rf.json.after === 'string')),
        JSON.stringify(rf.json).slice(0, 180));
    }

    // 4. refusals are bodies, not transport errors
    const bad = await post('/api/edit', { tool: 'not_a_tool', args: {} });
    check('unknown tool refused with ok:false',
      bad.res.status === 200 && bad.json && bad.json.ok === false && typeof bad.json.error === 'string',
      'status=' + bad.res.status + ' ' + JSON.stringify(bad.json).slice(0, 160));
    check('unknown tool refusal still lists the tools',
      bad.json && Array.isArray(bad.json.tools) && bad.json.tools.length >= EXPECTED.length);
  }

  // ---- analyze shard -------------------------------------------------------
  if (analyze) {
    // 2b. the analysis route: parse_check + dedupe
    const opsA = await get('/api/analyze');
    check('GET /api/analyze ok', opsA.res.status === 200 && opsA.json && opsA.json.ok === true,
      'status=' + opsA.res.status);
    const analyzeOps = (opsA.json && opsA.json.ops) || [];
    check('analyze ops are parse_check + dedupe',
      analyzeOps.indexOf('parse_check') >= 0 && analyzeOps.indexOf('dedupe') >= 0, analyzeOps.join(','));

    const gate = await post('/api/analyze', { op: 'parse_check', args: { timeout: 12 } });
    check('POST parse_check returns the gate body',
      gate.res.status === 200 && gate.json && typeof gate.json.ok === 'boolean' &&
      'toolchain' in gate.json && Array.isArray(gate.json.diagnostics) && 'count' in gate.json,
      JSON.stringify(gate.json).slice(0, 200));
    if (gate.json && gate.json.skipped) {
      check('skipped gate is ok:false with a reason and no diagnostics',
        gate.json.ok === false && typeof gate.json.skipped === 'string' && gate.json.count === 0 &&
        gate.json.diagnostics.length === 0, 'skipped=' + gate.json.skipped);
    } else {
      check('clean gate is ok:true, count 0, skipped null',
        !!gate.json && gate.json.ok === true && gate.json.count === 0 && gate.json.skipped === null,
        'ok=' + (gate.json && gate.json.ok) + ' count=' + (gate.json && gate.json.count));
    }
    check('gate never claims ok:true alongside a skipped reason',
      !(gate.json && gate.json.ok === true && gate.json.skipped),
      JSON.stringify(gate.json).slice(0, 200));

    const t0 = Date.now();
    const dup = await post('/api/analyze', { op: 'dedupe', args: { min_lines: 4, threshold: 0.8 } });
    const dupMs = Date.now() - t0;
    check('POST dedupe is the CLI report shape',
      !!dup.json && dup.json.ok === true && Array.isArray(dup.json.exact) && Array.isArray(dup.json.near) &&
      Array.isArray(dup.json.blocks) && !!dup.json.stats && typeof dup.json.files === 'number' &&
      typeof dup.json.truncated === 'boolean',
      JSON.stringify(dup.json && {
        files: dup.json.files,
        exact: dup.json.stats && dup.json.stats.exactClusters,
        near: dup.json.stats && dup.json.stats.nearClusters,
        blocks: dup.json.stats && dup.json.stats.blockGroups,
      }).slice(0, 220));
    check('dedupe ran under the unit budget', dupMs < 9000, dupMs + 'ms');

    const badOp = await post('/api/analyze', { op: 'not_an_op', args: {} });
    check('unknown analyze op refused with ok:false',
      badOp.res.status === 200 && badOp.json && badOp.json.ok === false && typeof badOp.json.error === 'string',
      JSON.stringify(badOp.json).slice(0, 160));

    // 3b. parse_check arguments are honoured (files, timeout)
    if (!file) file = await firstIndexedFile();
    if (!file) {
      check('analyze shard found an indexed file', false, 'select_decls returned nothing');
    } else {
      const gate1 = await post('/api/analyze', { op: 'parse_check', args: { files: [file], timeout: 12 } });
      const gateShape = gate1.res.status === 200 && !!gate1.json &&
        typeof gate1.json.ok === 'boolean' && 'count' in gate1.json;
      if (gate1.json && gate1.json.skipped) {
        check('parse_check accepts args.files (toolchain skipped; count not asserted)',
          gateShape, 'skipped=' + gate1.json.skipped);
      } else {
        check('parse_check honours args.files (exactly the one file)',
          gateShape && gate1.json.files === 1, 'files=' + (gate1.json && gate1.json.files));
      }
      const gate2 = await post('/api/analyze', { op: 'parse_check', args: { files: file, timeout: 12 } });
      if (gate2.json && gate2.json.skipped) {
        check('parse_check accepts the comma-string files form (skipped)',
          gate2.res.status === 200 && typeof gate2.json.ok === 'boolean', 'skipped=' + gate2.json.skipped);
      } else {
        check('parse_check accepts the comma-string files form',
          gate2.res.status === 200 && !!gate2.json && gate2.json.files === 1,
          'files=' + (gate2.json && gate2.json.files));
      }
    }
  }

  // ---- reorg shard ---------------------------------------------------------
  if (reorg) {
    const rjs = await get('/static/reorg.js');
    check('reorg.js served', rjs.res.status === 200 && rjs.text.indexOf('window.CSReorg') >= 0,
      'status=' + rjs.res.status);
    check('reorg.js exposes the Plan action',
      rjs.text.indexOf('reorgPlan') >= 0 && rjs.text.indexOf("run('plan'") >= 0);

    // 3c. the full reorg action set: plan is reachable and read-only
    const rp = await post('/api/reorg', {
      action: 'plan',
      spec: { header: true, granularity: 'leaf', group: 'single' },
    });
    check('POST /api/reorg plan returns the plan shape',
      rp.res.status === 200 && !!rp.json && rp.json.ok === true && !!rp.json.plan &&
      typeof rp.json.plan.fileCount === 'number' && rp.json.plan.fileCount > 0 &&
      typeof rp.json.plan.unitCount === 'number' && rp.json.plan.unitCount > 0,
      JSON.stringify(rp.json && rp.json.plan && { files: rp.json.plan.fileCount, units: rp.json.plan.unitCount }).slice(0, 160));
    // An unknown action is refused, but the engine emits + verifies the plan
    // before the dispatch match, so probing it here would cost a full verify
    // (~25 s). The refusal path is covered by probe_mcp.py instead.
  }
}

const watchdog = setTimeout(function () {
  console.log('FAIL probe timed out after ' + TIMEOUT_MS + 'ms');
  console.log('PROBE_EDIT_APP FAILED (1)');
  process.exit(1);
}, TIMEOUT_MS);

try {
  await main();
} catch (e) {
  check('unexpected error', false, (e && e.stack) || String(e));
} finally {
  clearTimeout(watchdog);
}

console.log((failures === 0 ? 'PROBE_EDIT_APP OK' : 'PROBE_EDIT_APP FAILED (' + failures + ')') +
  (SHARD === 'all' ? '' : ' [' + SHARD + ' shard]'));
process.exit(failures === 0 ? 0 : 1);
