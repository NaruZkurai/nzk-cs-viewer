/* csgraph v2 — the keyboard quick-open palette (Ctrl/Cmd+P and Ctrl/Cmd+T).
 *
 * Part of the RUST-ONLY v2 viewer.
 *
 * Ctrl/Cmd+P  files & symbols  — /api/search ranked rows, file hits first
 * Ctrl/Cmd+T  symbols          — /api/search merged with /api/find
 *                                (definitions, implementations, then usages)
 *
 * The palette owns its own DOM and CSS and registers its keybindings twice: on
 * the Monaco editor (`editor.addCommand`) so it works while the editor has
 * focus, and on `window` (capture) so it works everywhere else. It deliberately
 * touches neither `/` (symbol search) nor `f` (find panel) from shell.js.
 *
 * Enter opens the source (`ctx.openSource`) and selects the node in the graph
 * (`ctx.goToNode`); a usage row opens the real file at its line.
 *
 * The Monaco build under vendor/ has no built-in quick-open command wired to
 * Ctrl+P, so this is the viewer's own implementation.
 */
'use strict';

window.CsQuickOpen = (function () {
  var MAX_ROWS = 60;
  var DEBOUNCE = 140;
  var RECENT_MAX = 8;

  var ctx = null;               // extension context, set once per editor attach
  var overlay = null, boxEl = null, inputEl = null, listEl = null,
      modeEl = null, noteEl = null;
  var rows = [];
  var sel = -1;
  var mode = 'files';           // 'files' | 'symbols'
  var seq = 0;                  // stale-response guard
  var timer = 0;
  var shown = false;
  var seedQuery = '';           // remembered query while the palette is open
  var windowBound = false;

  var MODE_LABEL = { files: 'Files & symbols', symbols: 'Symbols' };
  var MODE_PLACEHOLDER = {
    files: 'Type a file or symbol name…',
    symbols: 'Type a symbol name…'
  };

  function toast(msg, kind) {
    var fn = (ctx && ctx.toast) || window.toast;
    if (typeof fn === 'function') fn(msg, kind || 'warn');
  }

  function esc(s) {
    var fn = (ctx && ctx.esc) || window.esc;
    if (typeof fn === 'function') return fn(s);
    return String(s === null || s === undefined ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  function apiGet() {
    var fn = (ctx && ctx.apiGet) || window.apiGet;
    return (typeof fn === 'function') ? fn : null;
  }

  // ------------------------------------------------------------------- style

  function injectStyle() {
    if (document.getElementById('cs-quickopen-style')) return;
    var css = document.createElement('style');
    css.id = 'cs-quickopen-style';
    css.textContent = [
      '#cs-quickopen { position: fixed; inset: 0; z-index: 80; display: flex;',
      '  align-items: flex-start; justify-content: center; padding-top: 8vh;',
      '  background: rgba(4,7,14,.45); }',
      '#cs-quickopen.hidden { display: none; }',
      '.cs-qo-box { width: min(680px, 92vw); max-height: 70vh; display: flex;',
      '  flex-direction: column; background: var(--bg-panel, #161625);',
      '  border: 1px solid var(--border, #2f3d5c); border-radius: 8px;',
      '  box-shadow: 0 18px 50px rgba(0,0,0,.55); overflow: hidden; }',
      '.cs-qo-head { display: flex; align-items: center; gap: 8px; padding: 8px 10px;',
      '  border-bottom: 1px solid var(--border-soft, #232c42); background: var(--bg-input, #131722); }',
      '.cs-qo-mode { flex: none; font-family: var(--mono, monospace); font-size: 9.5px;',
      '  letter-spacing: .12em; text-transform: uppercase; color: var(--accent, #86aaf9); }',
      '.cs-qo-input { flex: 1 1 auto; min-width: 0; height: 26px; padding: 0 6px;',
      '  background: transparent; border: 0; outline: none; color: var(--text, #e3e8f4);',
      '  font-family: var(--mono, monospace); font-size: 13px; }',
      '.cs-qo-input::placeholder { color: var(--text-faint, #6b7897); }',
      '.cs-qo-hint { flex: none; font-family: var(--mono, monospace); font-size: 9.5px;',
      '  color: var(--text-faint, #6b7897); white-space: nowrap; }',
      '.cs-qo-list { flex: 1 1 auto; overflow-y: auto; padding: 4px 0; }',
      '.cs-qo-row { display: flex; align-items: baseline; gap: 9px; width: 100%;',
      '  padding: 5px 10px; cursor: pointer; font-family: var(--mono, monospace);',
      '  font-size: 11.5px; color: var(--text, #e3e8f4); }',
      '.cs-qo-row.sel { background: rgba(134,170,249,.16); box-shadow: inset 2px 0 0 var(--accent, #86aaf9); }',
      '.cs-qo-kind { flex: none; width: 78px; color: var(--text-faint, #6b7897);',
      '  font-size: 9.5px; text-transform: uppercase; letter-spacing: .05em;',
      '  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }',
      '.cs-qo-name { flex: 1 1 auto; min-width: 0; overflow: hidden;',
      '  text-overflow: ellipsis; white-space: nowrap; color: var(--text, #e3e8f4); }',
      '.cs-qo-full { color: var(--text-faint, #6b7897); margin-left: 7px; font-size: 10.5px; }',
      '.cs-qo-loc { flex: none; max-width: 44%; overflow: hidden; text-overflow: ellipsis;',
      '  white-space: nowrap; color: var(--vt-info, #42c3d0); font-size: 10.5px; }',
      '.cs-qo-note { flex: none; padding: 5px 10px 7px; font-family: var(--mono, monospace);',
      '  font-size: 10px; color: var(--text-faint, #6b7897);',
      '  border-top: 1px solid var(--border-soft, #232c42); }',
      '.cs-qo-note.hidden { display: none; }',
      '.cs-qo-empty { padding: 10px; font-family: var(--mono, monospace); font-size: 11px;',
      '  color: var(--text-faint, #6b7897); }'
    ].join('\n');
    document.head.appendChild(css);
  }

  // --------------------------------------------------------------- palette DOM

  function ensureDom() {
    if (overlay) return;
    injectStyle();
    overlay = document.createElement('div');
    overlay.id = 'cs-quickopen';
    overlay.className = 'hidden';
    overlay.setAttribute('role', 'dialog');
    overlay.setAttribute('aria-modal', 'true');
    overlay.setAttribute('aria-label', 'Quick open');

    boxEl = document.createElement('div');
    boxEl.className = 'cs-qo-box';

    var head = document.createElement('div');
    head.className = 'cs-qo-head';
    modeEl = document.createElement('span');
    modeEl.className = 'cs-qo-mode';
    modeEl.textContent = MODE_LABEL[mode];
    inputEl = document.createElement('input');
    inputEl.id = 'cs-qo-input';
    inputEl.className = 'cs-qo-input';
    inputEl.type = 'text';
    inputEl.setAttribute('spellcheck', 'false');
    inputEl.setAttribute('autocomplete', 'off');
    inputEl.setAttribute('role', 'combobox');
    inputEl.setAttribute('aria-autocomplete', 'list');
    inputEl.placeholder = MODE_PLACEHOLDER[mode];
    var hint = document.createElement('span');
    hint.className = 'cs-qo-hint';
    hint.textContent = '↑↓ · Enter · Esc';
    head.appendChild(modeEl);
    head.appendChild(inputEl);
    head.appendChild(hint);

    listEl = document.createElement('div');
    listEl.id = 'cs-qo-list';
    listEl.className = 'cs-qo-list';
    listEl.setAttribute('role', 'listbox');

    noteEl = document.createElement('div');
    noteEl.id = 'cs-qo-note';
    noteEl.className = 'cs-qo-note hidden';

    boxEl.appendChild(head);
    boxEl.appendChild(listEl);
    boxEl.appendChild(noteEl);
    overlay.appendChild(boxEl);
    document.body.appendChild(overlay);

    overlay.addEventListener('mousedown', function (ev) {
      if (ev.target === overlay) close();
    });
    listEl.addEventListener('mousedown', function (ev) {
      ev.preventDefault();           // do not blur the input before we activate
      var row = ev.target.closest ? ev.target.closest('.cs-qo-row') : null;
      if (!row) return;
      var i = parseInt(row.getAttribute('data-i'), 10);
      if (isFinite(i)) activate(i);
    });
    inputEl.addEventListener('input', function () {
      seedQuery = inputEl.value;
      schedule();
    });
  }

  // ------------------------------------------------------------------- search

  /** One /api/search row -> the palette's row shape. */
  function fromNode(n, source) {
    if (!n) return null;
    var id = n.id || '';
    if (!id && !n.file) return null;
    return {
      id: id, kind: n.kind || n.label || '',
      name: n.name || n.label || n.full || id,
      full: n.full || '', label: n.label || n.full || n.name || id,
      file: n.file || '', line: n.line || 0,
      source: source || 'search'
    };
  }

  function dedupe(list) {
    var seen = {}, out = [];
    list.forEach(function (r) {
      if (!r) return;
      var key = r.id ? ('id:' + r.id) : ('loc:' + r.file + ':' + r.line);
      if (seen[key]) return;
      seen[key] = 1;
      out.push(r);
    });
    return out;
  }

  function composeSearch(api, q, wantSymbols) {
    var jobs = [
      api('/api/search', { q: q, limit: wantSymbols ? 50 : 80 })
        .catch(function () { return []; })
    ];
    if (wantSymbols) {
      jobs.push(api('/api/find', { q: q, limit: 50 }).catch(function () { return null; }));
    }
    return Promise.all(jobs).then(function (res) {
      // `/api/search` answers `{ok, results:[…]}` (api.rs); accept a bare array
      // too so the palette does not depend on the wrapper.
      var raw = res[0];
      if (raw && !Array.isArray(raw)) raw = raw.results || [];
      var searchRows = (raw || []).map(function (n) { return fromNode(n, 'search'); });
      var out = [];
      if (wantSymbols) {
        var find = res[1];
        if (find) {
          (find.definitions || []).forEach(function (n) {
            var r = fromNode(n, 'definition'); if (r) { r.kind = r.kind || 'definition'; out.push(r); }
          });
          (find.implementations || []).forEach(function (n) {
            var r = fromNode(n, 'implementation'); if (r) { r.kind = r.kind || 'implementation'; out.push(r); }
          });
          (find.usages || []).slice(0, 20).forEach(function (u) {
            var n = u.node || {};
            out.push({
              id: (n && n.id) || '', kind: 'usage',
              name: (n && (n.name || n.label)) || (u.text || ''),
              full: (n && n.full) || '', label: (n && (n.label || n.full)) || u.text || '',
              file: u.file || '', line: u.line || 0, source: 'usage'
            });
          });
        }
      }
      var merged = dedupe(out.concat(searchRows));
      if (!wantSymbols && q.indexOf('/') >= 0) {
        // A query with a slash is a path: keep only rows whose file matches.
        var fileHits = merged.filter(function (r) {
          return r.file && r.file.toLowerCase().indexOf(q.toLowerCase()) >= 0;
        });
        if (fileHits.length) merged = fileHits;
      } else if (!wantSymbols) {
        // Prefer file hits without hiding symbol hits.
        var outRows = [], fileRows = [];
        merged.forEach(function (r) {
          if (r.file && r.file.toLowerCase().indexOf(q.toLowerCase()) >= 0) fileRows.push(r);
          else outRows.push(r);
        });
        merged = fileRows.concat(outRows);
      }
      return merged.slice(0, MAX_ROWS);
    });
  }

  /** Run the search for `q` and record the rows (render is the caller's job). */
  function runSearch(q, wantSymbols) {
    var api = apiGet();
    var mine = ++seq;
    q = (q || '').trim();
    if (!api || !q) { setRows([]); renderList(); return Promise.resolve([]); }
    return composeSearch(api, q, wantSymbols).then(function (list) {
      if (mine !== seq) return rows.slice();
      setRows(list);
      renderList();
      if (!list.length) setNote('No files or symbols match “' + q + '”.');
      else clearNote();
      return list.slice();
    }).catch(function (err) {
      if (mine !== seq) return rows.slice();
      setRows([]); renderList();
      setNote('Search failed: ' + (err && err.message ? err.message : err));
      return [];
    });
  }

  function schedule() {
    if (timer) window.clearTimeout(timer);
    timer = window.setTimeout(function () { timer = 0; runSearch(inputEl.value, mode === 'symbols'); }, DEBOUNCE);
  }

  // ------------------------------------------------------------------ render

  function setRows(list) { rows = list || []; sel = rows.length ? 0 : -1; }

  function setNote(text) {
    if (!noteEl) return;
    if (!text) { noteEl.classList.add('hidden'); noteEl.textContent = ''; return; }
    noteEl.classList.remove('hidden');
    noteEl.textContent = text;
  }
  function clearNote() { setNote(''); }

  function kindOf(r) {
    if (r.kind) return r.kind;
    if (r.source === 'usage') return 'usage';
    return '';
  }

  function renderList() {
    if (!listEl) return;
    listEl.innerHTML = '';
    if (!rows.length) {
      var empty = document.createElement('div');
      empty.className = 'cs-qo-empty';
      empty.textContent = seedQuery
        ? 'No matches.'
        : 'Type to search files and symbols.';
      listEl.appendChild(empty);
      return;
    }
    var frag = document.createDocumentFragment();
    rows.forEach(function (r, i) {
      var row = document.createElement('div');
      row.className = 'cs-qo-row' + (i === sel ? ' sel' : '');
      row.setAttribute('role', 'option');
      row.setAttribute('data-i', String(i));
      row.setAttribute('aria-selected', i === sel ? 'true' : 'false');

      var kind = document.createElement('span');
      kind.className = 'cs-qo-kind';
      kind.textContent = kindOf(r);

      var name = document.createElement('span');
      name.className = 'cs-qo-name';
      name.textContent = r.label || r.name || r.id || r.file || '';
      if (r.full && r.full !== r.label && r.full !== r.name) {
        var full = document.createElement('span');
        full.className = 'cs-qo-full';
        full.textContent = r.full;
        name.appendChild(full);
      }

      var loc = document.createElement('span');
      loc.className = 'cs-qo-loc';
      loc.textContent = r.file ? (r.file + (r.line ? ':' + r.line : '')) : (r.id ? r.id : '');

      row.title = (r.full || r.label || r.name || '') +
        (r.file ? '  —  ' + r.file + (r.line ? ':' + r.line : '') : '');
      row.appendChild(kind);
      row.appendChild(name);
      row.appendChild(loc);
      frag.appendChild(row);
    });
    listEl.appendChild(frag);
    var active = listEl.querySelector('.cs-qo-row.sel');
    if (active && active.scrollIntoView) active.scrollIntoView({ block: 'nearest' });
  }

  function move(delta) {
    if (!rows.length) return;
    sel += delta;
    if (sel < 0) sel = rows.length - 1;
    if (sel >= rows.length) sel = 0;
    var all = listEl.querySelectorAll('.cs-qo-row');
    for (var i = 0; i < all.length; i++) {
      all[i].classList.toggle('sel', i === sel);
      all[i].setAttribute('aria-selected', i === sel ? 'true' : 'false');
    }
    if (all[sel] && all[sel].scrollIntoView) all[sel].scrollIntoView({ block: 'nearest' });
  }

  /** Open one row: the source pane for the document, the graph for the node. */
  function openRow(r) {
    if (!r) return null;
    var c = ctx;
    if (r.id && c && c.openSource) {
      try { c.openSource({ node: r.id }); } catch (e) { /* fall through */ }
      if (c.goToNode) { try { c.goToNode(r.id); } catch (e2) { /* ignore */ } }
    } else if (r.file && c && c.openSource) {
      c.openSource({ file: r.file, line: r.line || 1, mode: 'raw' });
      if (r.id && c.goToNode) { try { c.goToNode(r.id); } catch (e3) { /* ignore */ } }
    } else if (!c) {
      toast('The editor is not ready yet', 'warn');
    }
    return r;
  }

  function activate(i) {
    var r = rows[i];
    if (!r) return null;
    close();
    return openRow(r);
  }

  // ------------------------------------------------------------- open / close

  /** Recent documents from nav.js, so an empty palette is still useful. */
  function recentRows() {
    var nav = window.CsNav;
    if (!nav || typeof nav.history !== 'function') return [];
    var hist = nav.history() || [];
    var out = [];
    for (var i = hist.length - 1; i >= 0 && out.length < RECENT_MAX; i--) {
      var e = hist[i];
      if (!e) continue;
      out.push({
        id: e.nodeId || '', kind: e.mode === 'doc' ? 'node' : 'file',
        name: e.label || e.nodeId || e.file || '',
        label: e.label || e.nodeId || e.file || '',
        full: '', file: e.file || '', line: e.line || 0,
        source: 'recent'
      });
    }
    return out;
  }

  function seed(q) {
    seedQuery = q || '';
    if (seedQuery) { runSearch(seedQuery, mode === 'symbols'); return; }
    var recents = recentRows();
    setRows(recents);
    renderList();
    if (recents.length) setNote('Recent documents. Type to search files and symbols.');
    else setNote('');
  }

  /**
   * Show the palette in `m` ('files' or 'symbols'), optionally pre-filtered by
   * `initialQuery` (used by the tests and by callers that want a head start).
   */
  function open(m, initialQuery) {
    ensureDom();
    mode = (m === 'symbols') ? 'symbols' : 'files';
    modeEl.textContent = MODE_LABEL[mode];
    inputEl.placeholder = MODE_PLACEHOLDER[mode];
    overlay.classList.remove('hidden');
    shown = true;
    inputEl.value = initialQuery || '';
    inputEl.focus();
    if (initialQuery) { try { inputEl.select(); } catch (e) { /* ignore */ } }
    seed(inputEl.value);
    return rows.slice();
  }

  function close() {
    if (!overlay) { shown = false; return; }
    overlay.classList.add('hidden');
    shown = false;
    seq++;                       // drop any in-flight response
    if (timer) { window.clearTimeout(timer); timer = 0; }
  }

  function isOpen() { return shown; }

  /** Test/caller hook: set the query and return the resulting rows. */
  function query(q, m) {
    if (m) mode = (m === 'symbols') ? 'symbols' : 'files';
    ensureDom();
    if (modeEl) modeEl.textContent = MODE_LABEL[mode];
    if (inputEl) { inputEl.value = q || ''; }
    seedQuery = q || '';
    return runSearch(seedQuery, mode === 'symbols');
  }

  // -------------------------------------------------------------- keybindings

  function onWindowKeyDown(ev) {
    var mod = ev.ctrlKey || ev.metaKey;
    if (mod && !ev.shiftKey && !ev.altKey && (ev.key === 'p' || ev.key === 'P')) {
      ev.preventDefault();
      ev.stopPropagation();
      open('files');
      return;
    }
    if (mod && !ev.shiftKey && !ev.altKey && (ev.key === 't' || ev.key === 'T')) {
      ev.preventDefault();
      ev.stopPropagation();
      open('symbols');
      return;
    }
    if (!shown) return;
    if (ev.key === 'Escape') {
      ev.preventDefault();
      ev.stopPropagation();
      close();
      if (ctx && ctx.editor && ctx.editor.focus) ctx.editor.focus();
      return;
    }
    if (ev.key === 'ArrowDown') { ev.preventDefault(); ev.stopPropagation(); move(1); return; }
    if (ev.key === 'ArrowUp') { ev.preventDefault(); ev.stopPropagation(); move(-1); return; }
    if (ev.key === 'PageDown') { ev.preventDefault(); ev.stopPropagation(); move(10); return; }
    if (ev.key === 'PageUp') { ev.preventDefault(); ev.stopPropagation(); move(-10); return; }
    if (ev.key === 'Enter') { ev.preventDefault(); ev.stopPropagation(); activate(sel); return; }
    if (ev.key === 'Tab') { ev.preventDefault(); ev.stopPropagation(); move(ev.shiftKey ? -1 : 1); return; }
  }

  function bindWindow() {
    if (windowBound) return;
    windowBound = true;
    // Capture phase: the palette must see Escape and Ctrl+P before shell.js's
    // bubble-phase handler can close the source pane.
    window.addEventListener('keydown', onWindowKeyDown, true);
  }

  // ------------------------------------------------------------------- attach

  function attach(c) {
    ctx = c;
    injectStyle();
    bindWindow();
    if (c && c.monaco && c.editor && c.editor.addCommand) {
      var M = c.monaco;
      try {
        c.editor.addCommand(M.KeyMod.CtrlCmd | M.KeyCode.KeyP, function () { open('files'); });
        c.editor.addCommand(M.KeyMod.CtrlCmd | M.KeyCode.KeyT, function () { open('symbols'); });
      } catch (e) { /* the window binding still covers it */ }
    }
  }

  return {
    attach: attach,
    // test hooks
    open: open,
    close: close,
    query: query,
    rows: function () { return rows.slice(); },
    isOpen: isOpen,
    mode: function () { return mode; },
    move: move,
    activate: activate,
    /** Alias used by probes: re-render the current list. */
    render: renderList,
    /** The recent-document seed rows (from CsNav history). */
    history: recentRows
  };
})();

window.CsEditorExtension({ name: 'quickopen', attach: window.CsQuickOpen.attach });
