/* csgraph — reorganise & export.
 *
 * The graph knows every declaration and the exact span of its source. This
 * module turns that into the two operations the graph is a view of:
 *
 *   MERGE  the whole corpus as one compilable file;
 *   SPLIT  the graph selection (or the whole corpus) into chunks, by size,
 *          namespace or owning type, and download or write the result.
 *
 * It is a thin UI over POST /api/reorg and exposes all four engine actions:
 * `plan` (in memory, writes nothing), `verify` (plan + declaration/byte audit),
 * `zip` (download) and `write` (to the chosen folder); `compile` re-runs the
 * Roslyn differential when a Unity/VRChat toolchain exists. See cs_reorg.py for
 * the engine and compile_check.py for the differential proof.
 *
 * Classic script, one global scope, no build step — same as its neighbours.
 * Globals it consumes: $, state, apiPost, toast, window.CSArrange.
 */
(function () {
  'use strict';

  var els = {};
  var lastPlan = null;
  var busy = false;

  // ------------------------------------------------------------------ helpers

  function spec() {
    var mode = els.mode.value;
    var out = {
      header: !!els.header.checked,
      granularity: els.gran.value,
      group: mode === 'single' ? 'single' : els.group.value,
    };
    if (out.group === 'size') out.size = Math.max(50, Number(els.size.value) || 3000);
    if (els.scope.value === 'selection') {
      var ids = selectionIds();
      if (ids.length) out.units = ids;
    }
    return out;
  }

  function selectionIds() {
    if (!window.CSArrange || typeof window.CSArrange.ids !== 'function') return [];
    try {
      return window.CSArrange.ids() || [];
    } catch (e) {
      return [];
    }
  }

  function setNote(text, kind) {
    els.note.textContent = text || '';
    els.note.className = 'modal-note' + (kind ? ' ' + kind : '');
  }

  function setBusy(on, text) {
    busy = !!on;
    ['plan', 'zip', 'verify', 'write', 'compile'].forEach(function (k) {
      if (els[k]) els[k].disabled = busy;
    });
    setNote(busy ? (text || 'Working…') : '');
  }

  function row(label, value, cls) {
    return '<div class="reorg-row' + (cls ? ' ' + cls : '') + '">' +
      '<span class="reorg-k">' + label + '</span>' +
      '<span class="reorg-v">' + value + '</span></div>';
  }

  function esc(s) {
    return String(s == null ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  /** Render a plan and/or a verification report. */
  function render(data) {
    var html = '';
    var plan = data.plan || lastPlan;
    if (plan) {
      lastPlan = plan;
      html += row('files', plan.fileCount) +
        row('chunks', plan.unitCount) +
        row('lines', plan.lines) +
        row('grouping', esc(plan.group));
      var names = (plan.files || []).slice(0, 12).map(function (f) {
        return '<code>' + esc(f.path) + '</code> ' + f.lines + 'L';
      });
      if (names.length) {
        html += '<div class="reorg-files">' + names.join('') + '</div>';
      }
    }
    var rep = data.report;
    if (rep) {
      html += row('declarations', rep.after.declarations + ' / ' + rep.before.declarations,
        rep.missing.length || rep.added.length || rep.changed.length ? 'bad' : 'good');
      html += row('source text', (rep.text.checked - rep.text.missing.length) +
        ' / ' + rep.text.checked + ' spans kept',
        rep.text.ok ? 'good' : 'bad');
      html += row('balanced', rep.unbalanced.length ? rep.unbalanced.slice(0, 3).join(', ') : 'yes',
        rep.unbalanced.length ? 'bad' : 'good');
      if (rep.scope === 'corpus') {
        html += row('relations', rep.after.relations + ' / ' + rep.before.relations,
          rep.equal ? 'good' : 'warn');
      }
      if (rep.missing.length) html += row('missing', esc(rep.missing.slice(0, 5).join(', ')), 'bad');
      if (rep.added.length) html += row('invented', esc(rep.added.slice(0, 5).join(', ')), 'bad');
      if (rep.changed.length) html += row('changed', esc(rep.changed.slice(0, 5).join(', ')), 'bad');
      if (rep.scope === 'corpus' && !rep.equal) {
        html += row('edges drifted', rep.relationsMissing.length + ' lost, ' +
          rep.relationsAdded.length + ' new', 'warn');
      }
    }
    if (data.write) {
      html += row('written to', esc(data.write.dest || ''),
        data.write.ok ? 'good' : 'bad');
      if (data.write.errors && data.write.errors.length) {
        html += row('write errors', esc(data.write.errors.slice(0, 3).join('; ')), 'bad');
      }
    }
    if (data.skipped) {
      html += row('compile', esc(data.skipped), 'warn');
    } else if (data.newDiagnostics) {
      html += row('new diagnostics', data.newDiagnostics.length, 'bad');
      data.newDiagnostics.slice(0, 4).forEach(function (d) {
        html += row('', esc(d.code + ' ' + d.message), 'bad');
      });
      var worse = data.structuralDiagnostics || [];
      html += row('errors', data.candidate.errors + ' / ' + data.baseline.errors + ' (was)',
        worse.length ? 'bad' : 'good');
    }
    els.report.innerHTML = html || '<div class="reorg-row reorg-empty">Plan or verify to see what would be written.</div>';
  }

  // ------------------------------------------------------------------ actions

  function request(action, extra) {
    var body = { action: action, spec: spec() };
    if (extra) Object.keys(extra).forEach(function (k) { body[k] = extra[k]; });
    // Raw fetch, not apiPost: a failed verification is a 400 with a report
    // attached, and apiPost would turn that into an exception and lose it.
    return fetch('/api/reorg', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: JSON.stringify(body)
    }).then(function (res) {
      return res.json().catch(function () { return null; }).then(function (data) {
        if (!data) throw new Error('HTTP ' + res.status + ' ' + res.statusText);
        return data;
      });
    });
  }

  function run(action, extra, label) {
    setBusy(true, label);
    return request(action, extra).then(function (res) {
      setBusy(false);
      render(res || {});
      if (res && res.ok === false && res.error) toast(res.error, 'error');
      return res;
    }, function (err) {
      setBusy(false);
      toast((err && err.message) || 'reorganise failed', 'error');
      return null;
    });
  }

  function verify() {
    run('verify', null, 'Building and checking…').then(function (res) {
      if (!res) return;
      if (res.ok) toast('Verified: ' + res.plan.fileCount + ' file(s), no declaration lost');
      else toast('Verification found a problem — see the report', 'error');
    });
  }

  /** The read-only half of verify: build the output and show the plan without
   *  re-parsing the result for the audit. */
  function plan() {
    run('plan', null, 'Planning…').then(function (res) {
      if (!res) return;
      if (res.ok) toast('Plan: ' + res.plan.fileCount + ' file(s), ' + res.plan.unitCount + ' chunk(s)');
      else toast((res.error || 'plan failed') + ' — see the report', 'error');
    });
  }

  function compile() {
    run('compile', null, 'Compiling source and result…').then(function (res) {
      if (!res) return;
      if (res.skipped) toast('No Unity/Roslyn toolchain found — compile check skipped');
      else if (res.ok) toast('Compiles the same: no new diagnostics');
      else toast('Compile check found a difference — see the report', 'error');
    });
  }

  function write() {
    var dest = (els.dest.value || '').trim();
    if (!dest) { toast('Enter an absolute folder to write to', 'error'); return; }
    run('write', { dest: dest }, 'Writing…').then(function (res) {
      if (!res) return;
      var w = res.write || {};
      if (w.ok) toast('Wrote ' + (w.written || []).length + ' file(s) to ' + w.dest);
      else toast((w.errors && w.errors[0]) || w.error || 'write failed', 'error');
    });
  }

  function download() {
    // The native (Tauri) build carries API replies over an IPC bridge that is
    // JSON-only, so a binary response cannot reach the webview; the bridge
    // returns the archive base64-encoded instead and we rebuild the Blob here.
    if (window.__CSVIEWER_NATIVE__) return downloadNative();
    setBusy(true, 'Packaging…');
    var body = { action: 'zip', spec: spec() };
    fetch('/api/reorg', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body)
    }).then(function (res) {
      var kind = res.headers.get('Content-Type') || '';
      if (kind.indexOf('application/zip') !== 0) {
        return res.json().then(function (j) {
          setBusy(false);
          render(j);
          toast((j && j.error) || 'the output did not verify - not packaged', 'error');
        });
      }
      var name = /filename="([^"]+)"/.exec(res.headers.get('Content-Disposition') || '');
      return res.blob().then(function (blob) {
        saveBlob(blob, name ? name[1] : 'reorg.zip');
        setBusy(false);
        toast('Downloaded ' + (name ? name[1] : 'reorg.zip'));
        return previewZip();
      });
    }).catch(function () {
      setBusy(false);
      toast('download failed', 'error');
    });
  }

  function downloadNative() {
    run('zip', null, 'Packaging…').then(function (res) {
      if (!res || !res.data) return;
      var raw = window.atob(res.data);
      var bytes = new Uint8Array(raw.length);
      for (var i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
      var name = res.name || 'reorg.zip';
      saveBlob(new Blob([bytes], { type: 'application/zip' }), name);
      toast('Downloaded ' + name);
    });
  }

  function saveBlob(blob, name) {
    var url = URL.createObjectURL(blob);
    var a = document.createElement('a');
    a.href = url;
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(function () { URL.revokeObjectURL(url); }, 4000);
  }

  /** Best-effort: the plan alone, so the panel reflects what was packaged. */
  function previewZip() {
    return request('plan', null).then(function (res) { render(res); }, function () {});
  }

  // ------------------------------------------------------------------ chrome

  function open() {
    if (!state.indexed) { toast('Index a folder first', 'error'); return; }
    els.modal.classList.remove('hidden');
    els.modal.setAttribute('aria-hidden', 'false');
    var n = selectionIds().length;
    if (els.scope.value === 'selection' && !n) els.scope.value = 'all';
    els.scope.options[0].textContent = n
      ? 'The graph selection (' + n + ' node' + (n === 1 ? '' : 's') + ')'
      : 'The graph selection (nothing selected)';
    syncRows();
  }

  function close() {
    els.modal.classList.add('hidden');
    els.modal.setAttribute('aria-hidden', 'true');
  }

  function syncRows() {
    var split = els.mode.value === 'split';
    els.granRow.classList.toggle('hidden', !split);
    els.groupRow.classList.toggle('hidden', !split);
    els.sizeRow.classList.toggle('hidden', !(split && els.group.value === 'size'));
  }

  function install() {
    els.modal = $('#reorgModal');
    els.open = $('#exportBtn');
    if (!els.modal || !els.open) return;
    els.close = $('#reorgClose');
    els.mode = $('#reorgMode');
    els.scope = $('#reorgScope');
    els.gran = $('#reorgGran');
    els.group = $('#reorgGroup');
    els.size = $('#reorgSize');
    els.sizeRow = $('#reorgSizeRow');
    els.granRow = $('#reorgGranRow');
    els.groupRow = $('#reorgGroupRow');
    els.dest = $('#reorgDest');
    els.header = $('#reorgHeader');
    els.report = $('#reorgReport');
    els.note = $('#reorgNote');
    els.zip = $('#reorgZip');
    els.verify = $('#reorgVerify');
    els.write = $('#reorgWrite');
    els.compile = $('#reorgCompile');

    // The full action set is plan / verify / zip / write. The panel shipped
    // verify+write+zip buttons; Plan is added here (index.html is shared app
    // chrome), inserted next to the other foot actions.
    if (!els.plan) {
      els.plan = document.createElement('button');
      els.plan.id = 'reorgPlan';
      els.plan.type = 'button';
      els.plan.className = 'btn';
      els.plan.title = 'Build the output and show the file/line plan — writes nothing';
      els.plan.textContent = 'Plan';
      els.compile.parentNode.insertBefore(els.plan, els.compile);
      els.plan.addEventListener('click', plan);
    }

    els.open.addEventListener('click', open);
    els.close.addEventListener('click', close);
    els.modal.addEventListener('click', function (ev) {
      if (ev.target === els.modal) close();
    });
    els.mode.addEventListener('change', syncRows);
    els.group.addEventListener('change', syncRows);
    els.scope.addEventListener('change', syncRows);
    els.zip.addEventListener('click', download);
    els.verify.addEventListener('click', verify);
    els.write.addEventListener('click', write);
    els.compile.addEventListener('click', compile);
  }

  window.CSReorg = { install: install, open: open, close: close, plan: plan, verify: verify };
})();
