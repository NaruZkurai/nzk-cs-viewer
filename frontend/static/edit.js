/* csgraph — the edit-tools panel.
 *
 * User/agent parity: the MCP-only refactor/query tools (fuzzy_search,
 * resolve_chain, code_block, rename_symbol, qualify_symbols, expand_namespace,
 * write_file, apply_edits, select_decls, …) now also have a host route,
 * `GET/POST /api/edit`, handled by Engine::handle. This panel drives it.
 *
 * It is a thin, dependency-free UI over that route:
 *
 *   GET  /api/edit                 -> {"ok":true,"count":N,"tools":[...]}
 *   POST /api/edit {"tool":..,"args":{..}} -> the tool's own {"ok":..} JSON
 *   GET  /api/analyze              -> {"ok":true,"count":2,"ops":["parse_check","dedupe"]}
 *   POST /api/analyze {"op":..,"args":{..}} -> the CLI/MCP body unchanged
 *
 * The op list comes from the route, never from this file, so the panel and the
 * MCP server can never drift. Dry-run and Apply send the same body; Apply adds
 * `apply:true`, which is what the write tools read (the read tools ignore it).
 * `parse_check` is the Roslyn validity gate and `dedupe` the duplicate report;
 * a `skipped` gate is shown as an error, never as a pass.
 *
 * Picking an operation whose argument shape is not obvious (`grep`, the reshape
 * and `nzkcs` write tools) fills the args box with a skeleton — a hint only,
 * never a menu. Validate and Duplicates read that same box, so the app can pass
 * the gate's `files`/`timeout` and the dedupe threshold, not just their defaults.
 *
 * Classic script, one global scope, no build step — same as its neighbours.
 * Globals it consumes: $ (and the shimmed fetch).
 */
(function () {
  'use strict';

  var els = {};
  var busy = false;
  var installed = false;
  var lastTemplate = null;

  // A skeleton for the operations whose argument shape is not obvious, filled
  // into the args box when a tool is picked (and only while the box still holds
  // the previous skeleton or `{}`, so a typed document is never overwritten).
  // The op list itself still comes from GET /api/edit; this is a hint, not a
  // hardcoded menu.
  var TEMPLATES = {
    grep: { pattern: 'public static', literal: false, case: true, count_only: false, limit: 200, max_per_file: 50 },
    select_decls: { like: ['sps', 'socket'], mtype: ['void', 'System.String'] },
    move_decls: { like: ['sps', 'socket'], newfile: 'NZK.VF.cs', dir: 'Assets/nzk/vf', prefix: 'NZK.VF.NS' },
    fuzzy_search: { q: 'socket', limit: 30 },
    resolve_chain: { symbol: '#ns:NZK', dir: 'both', depth: 2 },
    code_block: { node: '#ns:NZK' },
    rename_symbol: { from: 'NZK.Core.Fn.Folder', to: 'NZK.Core.Fn.FolderFor' },
    qualify_symbols: { files: ['<repo-relative file>'], remove_usings: true },
    expand_namespace: { namespace: 'NZK', files: ['<repo-relative file>'] },
    write_file: { path: '<repo-relative file>', content: '// …' },
    apply_edits: { edits: [{ file: '<repo-relative file>', old: '<exact text>', new: '<replacement>' }] },
    copy_path: { from: '<repo-relative src>', to: '<repo-relative dst>', overwrite: false, apply: false },
    reformat: { file: '<repo-relative file>', apply: false },
    functionize: { members: ['<node id>', '<node id>'], newfile: 'NZK.Fn.cs', dir: 'Assets/nzk/fn', member: 'SharedHelper' },
  };

  function fillTemplate() {
    var t = TEMPLATES[els.tool.value];
    if (!t) return;
    var cur = (els.args.value || '').trim();
    if (cur === '' || cur === '{}' || cur === lastTemplate) {
      lastTemplate = JSON.stringify(t, null, 2);
      els.args.value = lastTemplate;
    }
  }

  // ------------------------------------------------------------------ helpers

  function note(text, kind) {
    els.note.textContent = text || '';
    els.note.className = 'modal-note' + (kind ? ' ' + kind : '');
  }

  function setBusy(on, text) {
    busy = !!on;
    els.dry.disabled = busy;
    els.apply.disabled = busy;
    els.tool.disabled = busy;
    if (els.validate) els.validate.disabled = busy;
    if (els.dedupe) els.dedupe.disabled = busy;
    note(busy ? (text || 'Working…') : '');
  }

  /** The raw tool JSON, refusals (`{ok:false}`) included — never thrown away. */
  function editFetch(method, body) {
    var opts = { method: method, headers: { 'Accept': 'application/json' } };
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    return fetch('/api/edit', opts).then(function (res) {
      return res.json().catch(function () { return null; }).then(function (data) {
        if (data === null) throw new Error('HTTP ' + res.status + ': malformed JSON');
        return data;
      });
    });
  }

  function render(data) {
    els.out.textContent = JSON.stringify(data, null, 2);
    els.out.classList.toggle('bad', !!(data && (data.ok === false || data.skipped)));
  }

  /** Tell the truth about a `parse_check` / `dedupe` body. A skipped gate is
   *  never a success; only `ok:true` with a null `skipped` is clean. */
  function noteAnalyze(op, data) {
    if (!data) { note('Empty response', 'error'); return; }
    if (op === 'parse_check') {
      if (data.skipped) { note('Skipped: ' + data.skipped, 'error'); return; }
      if (data.ok === false) {
        note('Diagnostics: ' + (data.count || 0) + ' in ' + (data.files || 0) + ' file(s)', 'error');
        return;
      }
      note('Clean: ' + (data.files || 0) + ' file(s), no diagnostics', 'ok');
      return;
    }
    if (op === 'dedupe') {
      if (data.ok === false) { note('Refused: ' + (data.error || 'error'), 'error'); return; }
      var stats = data.stats || {};
      note('Duplicates: ' + (stats.exactClusters || 0) + ' exact, ' +
        (stats.nearClusters || 0) + ' near, ' + (stats.blockGroups || 0) + ' blocks' +
        (data.truncated ? ' (truncated)' : ''), 'ok');
      return;
    }
    note(data.ok === false ? 'Refused: ' + (data.error || 'error') : 'Done',
      data.ok === false ? 'error' : 'ok');
  }

  // --------------------------------------------------------------- analysis

  function analyzeFetch(op, args) {
    return fetch('/api/analyze', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: JSON.stringify({ op: op, args: args || {} }),
    }).then(function (res) {
      return res.json().catch(function () { return null; }).then(function (data) {
        if (data === null) throw new Error('HTTP ' + res.status + ': malformed JSON');
        return data;
      });
    });
  }

  function analyze(op, args, label) {
    if (busy) return;
    setBusy(true, label + '…');
    analyzeFetch(op, args).then(function (data) {
      render(data);
      noteAnalyze(op, data);
    }).catch(function (e) {
      render({ ok: false, error: e.message });
      note(e.message, 'error');
    }).then(function () {
      setBusy(false);
    });
  }

  /** The args box as an object, for Validate/Duplicates too: `parse_check`
   *  reads `files` + `timeout`, `dedupe` reads `min_lines`/`threshold`. */
  function boxArgs() {
    var raw = (els.args.value || '').trim();
    if (!raw) return {};
    var v = JSON.parse(raw);
    if (!v || typeof v !== 'object' || Array.isArray(v)) {
      throw new Error('args must be a JSON object');
    }
    return v;
  }

  function validate() {
    var args;
    try { args = boxArgs(); } catch (e) { note('Args: ' + e.message, 'error'); return; }
    if (args.timeout === undefined) args.timeout = 25;
    analyze('parse_check', args, 'Validating the corpus');
  }

  function duplicates() {
    var args;
    try { args = boxArgs(); } catch (e) { note('Args: ' + e.message, 'error'); return; }
    analyze('dedupe', args, 'Scanning for duplicates');
  }

  function parseArgs() {
    return boxArgs();
  }

  // ------------------------------------------------------------------ actions

  function loadOps() {
    return editFetch('GET').then(function (data) {
      if (!data || data.ok === false) throw new Error((data && data.error) || 'could not list edit tools');
      var tools = data.tools || [];
      els.tool.textContent = '';
      tools.forEach(function (name) {
        var opt = document.createElement('option');
        opt.value = name;
        opt.textContent = name;
        els.tool.appendChild(opt);
      });
      return tools;
    });
  }

  function run(apply) {
    if (busy) return;
    var tool = els.tool.value;
    if (!tool) { note('No operation available', 'error'); return; }
    var args;
    try {
      args = parseArgs();
    } catch (e) {
      note('Args: ' + e.message, 'error');
      return;
    }
    if (apply) args.apply = true;
    setBusy(true, apply ? 'Applying ' + tool + '…' : 'Dry-running ' + tool + '…');
    editFetch('POST', { tool: tool, args: args }).then(function (data) {
      render(data);
      if (data && data.ok === false) {
        note('Refused: ' + (data.error || 'error'), 'error');
      } else {
        note(tool + (apply ? ' applied' : ' dry run'), 'ok');
      }
    }).catch(function (e) {
      render({ ok: false, error: e.message });
      note(e.message, 'error');
    }).then(function () {
      setBusy(false);
    });
  }

  // ------------------------------------------------------------------ chrome

  function open() {
    els.modal.classList.remove('hidden');
    els.modal.setAttribute('aria-hidden', 'false');
    if (!els.tool.options.length) {
      loadOps().then(function () {
        fillTemplate();
        note(els.tool.options.length + ' operations');
      }).catch(function (e) {
        note(e.message, 'error');
      });
    } else {
      fillTemplate();
    }
  }

  function close() {
    els.modal.classList.add('hidden');
    els.modal.setAttribute('aria-hidden', 'true');
  }

  function install() {
    if (installed) return true;
    els.modal = $('#editModal');
    els.open = $('#editBtn');
    if (!els.modal || !els.open) return false;
    els.close = $('#editClose');
    els.tool = $('#editTool');
    els.args = $('#editArgs');
    els.out = $('#editOut');
    els.note = $('#editNote');
    els.dry = $('#editDry');
    els.apply = $('#editApply');
    els.validate = $('#editValidate');
    els.dedupe = $('#editDedupe');

    els.open.addEventListener('click', open);
    els.close.addEventListener('click', close);
    els.modal.addEventListener('click', function (ev) {
      if (ev.target === els.modal) close();
    });
    els.dry.addEventListener('click', function () { run(false); });
    els.apply.addEventListener('click', function () { run(true); });
    els.tool.addEventListener('change', fillTemplate);
    if (els.validate) els.validate.addEventListener('click', validate);
    if (els.dedupe) els.dedupe.addEventListener('click', duplicates);
    // Ctrl/Cmd+Enter runs a dry run without leaving the args box.
    els.args.addEventListener('keydown', function (ev) {
      if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) {
        ev.preventDefault();
        run(false);
      }
    });
    installed = true;
    return true;
  }

  window.CSEdit = {
    install: install, open: open, close: close, run: run, loadOps: loadOps,
    validate: validate, duplicates: duplicates,
  };
})();
