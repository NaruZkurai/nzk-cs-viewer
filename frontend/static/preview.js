/* csgraph — change preview (M3 slice 2).
 *
 * A READ-ONLY panel over `POST /api/preview` (the app-facing twin of the
 * MCP-only `preview_change` tool). Before anything is written it answers:
 * which signatures and call sites would a rename or a retype of one
 * declaration change?
 *
 * It shows the declaration's current and proposed signature, every affected
 * declaration and call site with file:line, the exact before/after edits an
 * apply would make, and the counts. There is deliberately NO apply button:
 * the route has no write path and always returns `writes: []`, `applied:false`.
 *
 * Classic script, one global scope, no build step — same as its neighbours.
 * Globals it consumes: $, state, toast. Route: GET/POST /api/preview.
 */
(function () {
  'use strict';

  var els = {};
  var ops = [];          // op names, read from the route (never hardcoded)
  var last = null;

  // ------------------------------------------------------------------ helpers

  function esc(s) {
    return String(s == null ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function row(label, value, cls) {
    return '<div class="reorg-row' + (cls ? ' ' + cls : '') + '">' +
      '<span class="reorg-k">' + esc(label) + '</span>' +
      '<span class="reorg-v">' + value + '</span></div>';
  }

  function setNote(text, kind) {
    els.note.textContent = text || '';
    els.note.className = 'modal-note' + (kind ? ' ' + kind : '');
  }

  function selectedId() {
    try {
      if (state && state.selected) return String(state.selected);
    } catch (e) { /* ignore */ }
    return '';
  }

  // ------------------------------------------------------------------ request

  function request(method, body) {
    var opts = { method: method, headers: { 'Accept': 'application/json' } };
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    return fetch('/api/preview', opts).then(function (res) {
      return res.json().catch(function () { return null; }).then(function (data) {
        if (!data) throw new Error('HTTP ' + res.status + ' ' + res.statusText);
        return data;
      });
    });
  }

  // ------------------------------------------------------------------ render

  function render(data) {
    var html = '';
    if (!data || data.ok !== true) {
      var why = (data && data.error) || 'preview refused';
      html += row('refused', esc(why), 'bad');
      var amb = (data && data.ambiguity) || [];
      if (amb.length) {
        html += '<div class="preview-sub">Candidates</div>';
        amb.slice(0, 12).forEach(function (c) {
          html += row(c.kind || '', esc(c.full || c.id) + ' <code>' +
            esc((c.file || '') + ':' + (c.line || '')) + '</code>');
        });
      }
      els.out.innerHTML = html;
      setNote(why, 'bad');
      return;
    }
    last = data;
    var c = data.counts || {};
    html += '<div class="preview-readonly">Read-only plan — nothing is written' +
      (data.truncated ? ' <b>(truncated)</b>' : '') + '</div>';
    html += row('op', esc(data.op + ' → ' + data.to));
    html += row('declaration', esc(data.declaration.full || data.declaration.id) +
      ' <code>' + esc(data.declaration.file + ':' + data.declaration.line) + '</code>');
    if (data.currentType) html += row('declared type', esc(data.currentType));
    html += '<div class="preview-sub">Signature</div>';
    html += '<pre class="preview-sig">' + esc(data.currentSignature) +
      '\n' + esc(data.proposedSignature) + '</pre>';
    html += '<div class="preview-sub">Counts</div>';
    html += row('edits', esc(c.edits) + ' line(s) in ' + esc(c.files) + ' file(s)');
    html += row('call sites', esc(c.callSites));
    html += row('affected declarations', esc(c.declarations));
    html += row('changed lines', esc(c.changedLines));

    var edits = data.edits || [];
    if (edits.length) {
      html += '<div class="preview-sub">Edits (' + edits.length + ')</div>';
      edits.slice(0, 60).forEach(function (e) {
        html += '<div class="preview-edit"><code>' + esc(e.file + ':' + e.line) + '</code>' +
          '<div class="preview-del">- ' + esc(e.before) + '</div>' +
          '<div class="preview-add">+ ' + esc(e.after) + '</div></div>';
      });
      if (edits.length > 60) html += row('', '…' + (edits.length - 60) + ' more');
    }

    var sites = data.callSites || [];
    html += '<div class="preview-sub">Call sites (' + sites.length + ')</div>';
    if (!sites.length) {
      html += '<div class="reorg-row reorg-empty">No incoming call site is attributed to this declaration.</div>';
    } else {
      sites.slice(0, 60).forEach(function (s) {
        html += '<div class="preview-site"><code>' + esc(s.file + ':' + s.line) + '</code> ' +
          esc(s.node) + ' <span class="preview-edge">' + esc(s.edge) + '</span>' +
          (s.changed ? ' <span class="preview-changed">edited</span>' : '') + '</div>';
      });
      if (sites.length > 60) html += row('', '…' + (sites.length - 60) + ' more');
    }

    var decls = data.affectedDeclarations || [];
    if (decls.length) {
      html += '<div class="preview-sub">Affected declarations (' + decls.length + ')</div>';
      decls.slice(0, 40).forEach(function (d) {
        html += '<div class="preview-site"><code>' + esc(d.file + ':' + d.line) + '</code> ' +
          esc(d.full || d.id) + ' <span class="preview-edge">' + esc(d.edge) + '</span></div>';
      });
    }
    els.out.innerHTML = html;
    setNote('Planned ' + c.edits + ' edit(s), ' + c.callSites + ' call site(s) — wrote nothing', 'ok');
  }

  // ------------------------------------------------------------------ actions

  function args() {
    var out = { op: els.op.value, to: els.to.value.trim() };
    var sel = els.sel.value.trim();
    if (sel) out.node = sel;
    return out;
  }

  function run() {
    if (!state || !state.indexed) { toast('Index a folder first', 'error'); return; }
    var a = args();
    if (!a.to) { setNote('Enter the new name/type (`to`)', 'bad'); return; }
    els.run.disabled = true;
    setNote('Planning…');
    request('POST', a).then(function (data) {
      els.run.disabled = false;
      render(data);
    }, function (err) {
      els.run.disabled = false;
      setNote((err && err.message) || 'preview failed', 'bad');
    });
  }

  function loadOps() {
    return request('GET').then(function (data) {
      ops = (data && data.ops) || [];
      els.op.innerHTML = ops.map(function (o) {
        return '<option value="' + esc(o) + '">' + esc(o) + '</option>';
      }).join('');
      return data;
    }, function () { return null; });
  }

  // ------------------------------------------------------------------ chrome

  function open() {
    if (!state || !state.indexed) { toast('Index a folder first', 'error'); return; }
    els.modal.classList.remove('hidden');
    els.modal.setAttribute('aria-hidden', 'false');
    var id = selectedId();
    if (id && !els.sel.value) els.sel.value = id;
    if (!ops.length) loadOps();
    els.to.focus();
  }

  function close() {
    els.modal.classList.add('hidden');
    els.modal.setAttribute('aria-hidden', 'true');
  }

  function install() {
    els.modal = $('#previewModal');
    els.open = $('#previewBtn');
    if (!els.modal || !els.open) return;
    els.close = $('#previewClose');
    els.sel = $('#previewSelector');
    els.op = $('#previewOp');
    els.to = $('#previewTo');
    els.run = $('#previewRun');
    els.out = $('#previewOut');
    els.note = $('#previewNote');

    els.open.addEventListener('click', open);
    els.close.addEventListener('click', close);
    els.modal.addEventListener('click', function (ev) {
      if (ev.target === els.modal) close();
    });
    els.run.addEventListener('click', run);
    els.to.addEventListener('keydown', function (ev) {
      if (ev.key === 'Enter') { ev.preventDefault(); run(); }
    });
  }

  window.CSPreview = { install: install, open: open, close: close, run: run };

  // Self-wiring: the script tag is deferred, so the modal is in the DOM by the
  // time this runs. (`install` no-ops if the panel markup is absent.)
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', install);
  } else {
    install();
  }
})();
