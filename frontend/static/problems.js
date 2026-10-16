/* csgraph v2 — the parser-warning list in the sidebar.
 *
 * Part of the RUST-ONLY v2 viewer.
 *
 * The engine's `/api/state.warnings` array is what the C# *parser* reported while
 * indexing: files it could not read, files whose parse threw, scope-guard trips.
 * There is no Roslyn toolchain in this build, so there are no compiler
 * diagnostics here and this panel says so in as many words rather than
 * pretending otherwise. Real compiler errors belong to `compile_check.rs`, which
 * is a separate, optional pass.
 *
 * The section is built and appended from JS (`.side`, after `#chainSection`) so
 * index.html is not touched. Rows whose warning names a file and a line get a
 * clickable `file:line` that opens the source pane; bare text (for example
 * "scope guard tripped") is shown as text, with no dead link.
 */
'use strict';

window.CsProblems = (function () {
  var items = [];               // parsed warnings
  var filter = '';
  var seq = 0;
  var sectionEl = null, listEl = null, countEl = null, headEl = null, caretEl = null;
  var built = false;

  function esc(s) {
    var fn = window.esc;
    if (typeof fn === 'function') return fn(s);
    return String(s === null || s === undefined ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  function toast(msg, kind) {
    if (typeof window.toast === 'function') window.toast(msg, kind || 'warn');
  }

  // ------------------------------------------------------------------- style

  function injectStyle() {
    if (document.getElementById('cs-problems-style')) return;
    var css = document.createElement('style');
    css.id = 'cs-problems-style';
    css.textContent = [
      '.cs-problems-note { padding: 0 0 7px; color: var(--text-faint, #6b7897);',
      '  font-family: var(--mono, monospace); font-size: 9.5px; line-height: 1.45; }',
      '.cs-problem { display: flex; align-items: baseline; gap: 6px; padding: 4px 8px;',
      '  border-bottom: 1px solid var(--border-soft, #232c42);',
      '  font-family: var(--mono, monospace); font-size: 10.5px; line-height: 1.4; }',
      '.cs-problem:last-child { border-bottom: 0; }',
      '.cs-problem-loc { flex: none; padding: 0; border: 0; background: transparent;',
      '  color: var(--vt-info, #42c3d0); font: inherit; cursor: pointer; text-align: left; }',
      '.cs-problem-loc:hover { text-decoration: underline; }',
      '.cs-problem-text { flex: 1 1 auto; min-width: 0; color: var(--text-dim, #9aa6c2);',
      '  word-break: break-word; }',
      '.cs-problem-count-tag { color: var(--warn, #ffed2e); }'
    ].join('\n');
    document.head.appendChild(css);
  }

  // ------------------------------------------------------------------ parsing

  /**
   * One raw warning string -> {file, line, col, text, raw}.
   *
   * Two shapes are recognised, because that is what the engine emits:
   *   `src/Foo.cs:12: parser: …`        (repo-relative file + line)
   *   `src/Foo.cs(12,5): warning …`     (MSBuild-style)
   * Everything else is kept verbatim with no location, so the panel never
   * invents a clickable line that does not exist.
   */
  function parseWarning(raw) {
    var s = String(raw === null || raw === undefined ? '' : raw).trim();
    var m = /^(.*?)\((\d+),(\d+)\)\s*:\s*(.*)$/.exec(s);
    if (m) return { file: m[1], line: parseInt(m[2], 10), col: parseInt(m[3], 10), text: m[4], raw: s };
    m = /^(.*?):(\d+)(?::(\d+))?:\s*(.*)$/.exec(s);
    if (m) {
      return {
        file: m[1], line: parseInt(m[2], 10),
        col: m[3] ? parseInt(m[3], 10) : 0, text: m[4], raw: s
      };
    }
    return { file: '', line: 0, col: 0, text: s, raw: s };
  }

  function parseAll(warnings) {
    return (warnings || []).map(parseWarning);
  }

  function visible() {
    if (!filter) return items;
    var f = filter.toLowerCase();
    return items.filter(function (it) {
      return it.raw.toLowerCase().indexOf(f) >= 0;
    });
  }

  // ------------------------------------------------------------------- render

  function build() {
    if (built) return;
    injectStyle();
    var side = document.querySelector('.side');
    if (!side) return;                        // pane window booted without a side
    var anchor = document.getElementById('chainSection');

    sectionEl = document.createElement('section');
    sectionEl.className = 'section';
    sectionEl.id = 'problemsSection';

    headEl = document.createElement('button');
    headEl.className = 'section-head';
    headEl.id = 'problemsHead';
    headEl.type = 'button';
    headEl.title = 'Collapse or expand the parser warnings';
    caretEl = document.createElement('span');
    caretEl.className = 'caret';
    caretEl.textContent = '\u25be';
    headEl.appendChild(caretEl);
    headEl.appendChild(document.createTextNode('Parser warnings'));
    countEl = document.createElement('span');
    countEl.className = 'scount';
    countEl.id = 'problemsCount';
    headEl.appendChild(countEl);

    var body = document.createElement('div');
    body.className = 'section-body';

    var note = document.createElement('div');
    note.className = 'cs-problems-note';
    note.textContent = 'Parser warnings only — this build has no Roslyn toolchain, ' +
      'so compiler diagnostics are not available here.';

    var search = document.createElement('input');
    search.className = 'mini-input';
    search.id = 'problemsSearch';
    search.type = 'text';
    search.setAttribute('spellcheck', 'false');
    search.placeholder = 'filter warnings…';
    search.addEventListener('input', function () {
      filter = search.value.trim();
      paint();
    });

    listEl = document.createElement('div');
    listEl.className = 'listbox tall';
    listEl.id = 'problemsList';

    body.appendChild(note);
    body.appendChild(search);
    body.appendChild(listEl);
    sectionEl.appendChild(headEl);
    sectionEl.appendChild(body);

    headEl.addEventListener('click', function () {
      var collapsed = sectionEl.classList.toggle('collapsed');
      caretEl.textContent = collapsed ? '\u25b8' : '\u25be';
      if (!collapsed) refresh();
    });

    if (anchor && anchor.parentNode) anchor.parentNode.insertBefore(sectionEl, anchor.nextSibling);
    else side.appendChild(sectionEl);
    built = true;
  }

  function rowEl(it) {
    var row = document.createElement('div');
    row.className = 'cs-problem';
    if (it.file && it.line > 0) {
      var loc = document.createElement('button');
      loc.className = 'cs-problem-loc';
      loc.type = 'button';
      loc.textContent = it.file + ':' + it.line;
      loc.title = 'Open ' + it.file + ':' + it.line + ' in the source pane';
      loc.addEventListener('click', function () { openAt(it.file, it.line); });
      row.appendChild(loc);
      var text = document.createElement('span');
      text.className = 'cs-problem-text';
      text.textContent = it.text || it.raw;
      row.appendChild(text);
    } else {
      var only = document.createElement('span');
      only.className = 'cs-problem-text';
      only.textContent = it.raw;
      row.appendChild(only);
    }
    row.title = it.raw;
    return row;
  }

  function paint() {
    if (!listEl) return;
    var rows = visible();
    listEl.innerHTML = '';
    if (!rows.length) {
      var empty = document.createElement('div');
      empty.className = 'list-empty';
      empty.textContent = items.length
        ? 'No warning matches “' + filter + '”.'
        : 'No parser warnings';
      listEl.appendChild(empty);
    } else {
      var frag = document.createDocumentFragment();
      rows.forEach(function (it) { frag.appendChild(rowEl(it)); });
      listEl.appendChild(frag);
    }
    if (countEl) {
      countEl.textContent = String(items.length);
      countEl.title = items.length + ' parser warning' + (items.length === 1 ? '' : 's');
    }
  }

  function openAt(file, line) {
    if (typeof window.openSource === 'function') {
      window.openSource({ file: file, line: line || 1, context: 30, mode: 'raw' });
    } else {
      toast('The source pane is not ready yet', 'warn');
    }
  }

  /** Render a warnings array (or a whole /api/state payload). */
  function render(warnings) {
    var arr = warnings;
    if (arr && !Array.isArray(arr) && Array.isArray(arr.warnings)) arr = arr.warnings;
    items = parseAll(Array.isArray(arr) ? arr : []);
    paint();
    return items.slice();
  }

  /** Re-read /api/state and repaint. */
  function refresh() {
    build();
    var api = window.apiGet;
    if (typeof api !== 'function') return Promise.resolve(items.slice());
    var mine = ++seq;
    return api('/api/state').then(function (d) {
      if (mine !== seq) return items.slice();
      render((d && d.warnings) || []);
      return items.slice();
    }).catch(function (err) {
      if (mine !== seq) return items.slice();
      if (listEl) {
        listEl.innerHTML = '';
        var e = document.createElement('div');
        e.className = 'list-empty';
        e.textContent = 'Could not read parser warnings: ' + (err && err.message ? err.message : err);
        listEl.appendChild(e);
      }
      return items.slice();
    });
  }

  function boot() {
    build();
    refresh();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }

  return {
    // test hooks
    build: build,
    render: render,
    refresh: refresh,
    open: function () {
      build();
      if (sectionEl) {
        sectionEl.classList.remove('collapsed');
        if (caretEl) caretEl.textContent = '\u25be';
      }
      return refresh();
    },
    /** Filter the currently-rendered warnings (same as typing in the box). */
    query: function (q) {
      filter = q == null ? '' : String(q).trim();
      paint();
      return visible().slice();
    },
    items: function () { return items.slice(); },
    visible: function () { return visible().slice(); },
    count: function () { return items.length; },
    parseWarning: parseWarning
  };
})();

window.CsEditorExtension({
  name: 'problems',
  attach: function (ctx) {
    if (ctx && ctx.apiGet && typeof window.apiGet !== 'function') window.apiGet = ctx.apiGet;
    // `build()` is private to the IIFE above; this attach lives outside it, so
    // it must go through the exported hook (the naive `build()` here threw
    // "ReferenceError: build is not defined" in the browser).
    window.CsProblems.build();
    window.CsProblems.refresh();
  }
});
