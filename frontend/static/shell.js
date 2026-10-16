/* csgraph - folder picker, index status, resize, shortcuts panel, event wiring and layout controls.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== folder picker modal  // ================================================================== folder picker modal

  function openModal() {
    modalEl.classList.remove('hidden');
    modalEl.setAttribute('aria-hidden', 'false');
  }
  function closeModal() {
    modalEl.classList.add('hidden');
    modalEl.setAttribute('aria-hidden', 'true');
  }
  function modalIsOpen() { return !modalEl.classList.contains('hidden'); }

  function pickerHasCs(d) {
    // .cs.nzk (the toolkit's sealed C# format) counts as C# too, otherwise a
    // folder holding only sealed sources would not be selectable.
    var direct = (d.files || []).some(function (f) { return /\.cs(\.nzk)?$/i.test(f.name); });
    var inDirs = (d.dirs || []).some(function (x) { return !!x.hasCs; });
    return direct || inDirs;
  }

  function browseTo(path) {
    modalBodyEl.innerHTML = '<div class="modal-loading">Loading…</div>';
    apiGet('/api/browse', path ? { path: path } : {})
      .then(function (d) { renderPicker(d); })
      .catch(function (err) {
        modalBodyEl.innerHTML = '<div class="modal-empty">Could not list folder: ' + esc(err.message) + '</div>';
        modalSelectEl.disabled = true;
        modalNoteEl.textContent = '';
        modalNoteEl.className = 'modal-note warn';
        toast('Cannot open "' + (path || '.') + '": ' + err.message, 'error');
      });
  }

  function renderPicker(d) {
    var path = d.path || '';
    var hasCs = pickerHasCs(d);
    state.picker = {
      path: path,
      parent: (d.parent === undefined) ? null : d.parent,
      hasCs: hasCs,
      csCount: d.csCount || 0
    };

    modalPathEl.textContent = path || '/ (repository root)';
    if (modalPathInputEl) modalPathInputEl.value = path || '';
    modalUpEl.disabled = (d.parent === null || d.parent === undefined);

    var rows = [];
    (d.dirs || []).forEach(function (dir) {
      rows.push('<button class="pick' + (dir.hasCs ? '' : ' no-cs') + '" type="button" data-nav="' + esc(dir.path) + '" title="Open this folder' + (dir.hasCs ? ' — indexable .cs files in its subtree' : ' — no .cs files in its subtree') + '">' +
        '<span class="picon">▸</span>' +
        '<span class="pname">' + esc(dir.name) + '/</span>' +
        (dir.symlink ? '<span class="plink" title="symbolic link — followed when indexing">link</span>' : '') +
        '<span class="pmeta">' + (dir.hasCs ? (dir.csCount ? dir.csCount + ' .cs' : 'has .cs') : 'no .cs') + '</span>' +
        '</button>');
    });
    (d.files || []).forEach(function (f) {
      rows.push('<div class="pick file"><span class="picon">·</span>' +
        '<span class="pname">' + esc(f.name) + '</span>' +
        (f.symlink ? '<span class="plink" title="symbolic link — followed when indexing">link</span>' : '') +
        '</div>');
    });

    modalBodyEl.innerHTML = rows.length ? rows.join('') :
      '<div class="modal-empty">Empty folder.</div>';

    modalSelectEl.disabled = !hasCs;
    if (hasCs) {
      modalNoteEl.textContent = 'indexable';
      modalNoteEl.className = 'modal-note ok';
    } else {
      modalNoteEl.textContent = 'no .cs files in this subtree';
      modalNoteEl.className = 'modal-note warn';
    }
  }

  /** Home button: derive /home/<user> or /Users/<user> from the current path;
   *  otherwise ask the server for `~`, which it expands to the real home. */
  function homePath() {
    var p = (state.picker && state.picker.path) || '';
    var m = /^(\/(?:home|Users)\/[^\/]+)/.exec(p);
    if (m) return m[1];
    return '~';
  }

  function openPicker(startPath) {
    openModal();
    browseTo(startPath || '');
  }

  function chooseCurrentFolder() {
    var p = state.picker.path || '';
    rootInputEl.value = p;
    state.rootChoice = p || '.';
    indexBtnEl.disabled = false;
    closeModal();
    toast('Root selected: ' + (p || 'repository root') + ' — press Index to build the graph', 'ok');
  }

  // ================================================================== state / indexing

  function applyState(s) {
    state.indexed = !!s.indexed;
    var root = s.root || (s.roots && s.roots[0]) || '';
    state.root = root;
    state.counts = s.counts || {};
    // The folder input always shows the indexed root — it is the one place the
    // path lives; the status line never repeats it.
    if (root) {
      rootInputEl.value = root;
      rootInputEl.title = root;
      state.rootChoice = root;
    }
    indexBtnEl.disabled = !(rootInputEl.value.trim() || state.indexed);
    restoreViewerState(s.viewer);
    updateStatusLine(s);
    // Tell any window that opened before us (a detached pane started with
    // `--open-pane`) what this window is showing.
    if (window.CSSync) window.CSSync.push();
  }

  function updateStatusLine(s) {
    s = s || { indexed: state.indexed, root: state.root, counts: state.counts };
    if (!s.indexed) {
      statusEl.textContent = 'not indexed';
      statusEl.title = 'No index built yet';
      return;
    }
    var c = s.counts || {};
    var text = [
      'files ' + fmtNum(s.fileCount),
      'parse ' + fmtNum(s.parseMs) + 'ms',
      'nodes ' + fmtNum(c.nodes),
      'edges ' + fmtNum(c.edges)
    ].join('  ·  ');
    statusEl.textContent = text;
    statusEl.title = text;
  }

  // ---------------------------------------------------------------- stop indexing
  //
  // Wrong-folder escape hatch. A /api/index POST can run for seconds on a large
  // corpus; while it is in flight the Stop button is shown on the loading
  // overlay and POSTs /api/index/cancel, which flips the engine's cancel flag.
  // The engine then unwinds without installing anything, so the previous index
  // stays exactly as it was — we deliberately do NOT applyState() the response.

  var indexRunning = false;
  var stopIndexBtnEl = null;

  function stopButton() {
    if (!stopIndexBtnEl) stopIndexBtnEl = $('#stopIndexBtn');
    return stopIndexBtnEl;
  }

  function setIndexRunning(running) {
    indexRunning = running;
    var b = stopButton();
    if (b) {
      b.style.display = running ? '' : 'none';
      b.disabled = !running;
      b.textContent = 'Stop';
    }
    // The Index button is inert while a build runs; re-enable it from the
    // current input state when the build (or its cancel) settles.
    indexBtnEl.disabled = running || !(rootInputEl.value.trim() || state.indexed);
  }

  function stopIndex() {
    if (!indexRunning) return;
    var b = stopButton();
    if (b) { b.disabled = true; b.textContent = 'Stopping…'; }
    showLoading('Stopping index …');
    apiPost('/api/index/cancel', {}).catch(function (err) {
      toast('Could not stop the index: ' + err.message, 'error');
      // The build is still going; put the overlay back to its indexing text.
      showLoading('Indexing ' + (state.rootChoice || rootInputEl.value.trim() || '.') + ' …');
      if (b) { b.disabled = false; b.textContent = 'Stop'; }
    });
  }

  function doIndex() {
    var root = (rootInputEl.value.trim() || state.rootChoice || '.').trim();
    state.rootChoice = root;
    showLoading('Indexing ' + root + ' …');
    setIndexRunning(true);
    apiPost('/api/index', { roots: [root] })
      .then(function (s) {
        if (s && s.cancelled) {
          // Stop was pressed: the engine left the previous graph in place and
          // answered with its state, flagged cancelled. Applying that state is
          // harmless but pointless — the old index is already on screen.
          toast('Index cancelled — previous index is still active', 'warn');
          return;
        }
        applyState(s);
        // A fresh index is a fresh corpus: the namespace remembered from the last
        // one must not hide it. Show every namespace until the user picks one.
        state.filters.namespace = '';
        if (nsSelectEl) nsSelectEl.value = '';
        closeModal();
        state.selected = null;
        state.detail = null;
        state.focus = null;
        state.hovered = null;
        nodeDrawerEl.classList.add('hidden');
        if (s.warnings && s.warnings.length) {
          s.warnings.slice(0, 3).forEach(function (w) { toast('Index warning: ' + w, 'warn'); });
        }
        if (!s.fileCount || !((s.counts || {}).nodes)) {
          toast('Nothing indexed under "' + root + '": no .cs / .cs.nzk files found', 'error');
        } else {
          toast('Indexed ' + fmtNum(s.fileCount) + ' files · ' + fmtNum((s.counts || {}).nodes) +
                ' nodes · ' + fmtNum((s.counts || {}).edges) + ' edges', 'ok');
        }
        return loadNamespaces();
      })
      .catch(function (err) {
        toast('Index failed: ' + err.message, 'error');
      })
      .then(function () { hideLoading(); setIndexRunning(false); });
  }

  // ================================================================== resize / input

  function resize() {
    var rect = canvasWrap.getBoundingClientRect();
    state.w = Math.max(1, Math.round(rect.width));
    state.h = Math.max(1, Math.round(rect.height));
    state.dpr = window.devicePixelRatio || 1;
    canvas.width = Math.round(state.w * state.dpr);
    canvas.height = Math.round(state.h * state.dpr);
    canvas.style.width = state.w + 'px';
    canvas.style.height = state.h + 'px';
  }

  // ================================================================== shortcuts / help

  // Single source of truth for the panel and (later) rebinding. `keys` is a list
  // of alternatives; each alternative is a list of chords (chips joined by "+").
  var SHORTCUTS = [
    { group: 'View', items: [
      { keys: [['Ctrl', 'Shift', 'C']], desc: 'Recenter the view (fit the graph)' },
      { keys: [['+'], ['=']], desc: 'Zoom in' },
      { keys: [['−'], ['_']], desc: 'Zoom out' },
      { keys: [['R']], desc: 'Re-settle the layout' }
    ] },
    { group: 'Navigate', items: [
      { keys: [['/']], desc: 'Search symbols' },
      { keys: [['F']], desc: 'Find definitions / usages' },
      { keys: [['Backspace']], desc: 'Back to the previous node' },
      { keys: [['↑'], ['↓']], desc: 'Move the selection (Find results)' },
      { keys: [['Enter']], desc: 'Open the selected result' },
      { keys: [['Esc']], desc: 'Close panels / clear the selection' }
    ] },
    { group: 'Code', items: [
      { keys: [['C']], desc: 'View the selected or hovered node’s code' }
    ] },
    { group: 'Tabs', items: [
      { keys: [['Ctrl', 'Tab']], desc: 'Next document tab' },
      { keys: [['Ctrl', 'Shift', 'Tab']], desc: 'Previous document tab' },
      { keys: [['Alt', 'W']], desc: 'Close the active document tab' },
      { keys: [['Alt', 'Shift', 'W']], desc: 'Close every other tab' },
      { keys: [['Alt', 'S']], desc: 'Split the source pane / collapse the split' }
    ] },
    { group: 'Mouse', items: [
      { keys: [['Double-click']], desc: 'Expand a node' },
      { keys: [['Right-click']], desc: 'Pin the node card open' },
      { keys: [['Drag node']], desc: 'Pin a node in place' },
      { keys: [['Alt', 'click']], desc: 'Pin / unpin a node' },
      { keys: [['Middle', 'drag']], desc: 'Pan the view' },
      { keys: [['Wheel']], desc: 'Zoom at the pointer' }
    ] },
    { group: 'Help', items: [
      { keys: [['?']], desc: 'Open this shortcuts panel' }
    ] }
  ];

  /** The panel list: this file's groups with arrange.js's Select/Arrange pair folded in. */
  function shortcutGroups() {
    var groups = SHORTCUTS.slice();
    var extra = arrange.shortcuts();
    var at = 0;
    while (at < groups.length && groups[at].group !== 'Mouse') at++;
    return groups.slice(0, at).concat(extra, groups.slice(at));
  }

  function shortcutRow(entry) {
    var row = document.createElement('div');
    row.className = 'shortcut-row';
    var desc = document.createElement('span');
    desc.className = 'shortcut-desc';
    desc.textContent = entry.desc;
    var keys = document.createElement('span');
    keys.className = 'shortcut-keys';
    entry.keys.forEach(function (chord, ci) {
      if (ci) {
        var or = document.createElement('span');
        or.className = 'shortcut-plus';
        or.textContent = '/';
        keys.appendChild(or);
      }
      chord.forEach(function (part, pi) {
        if (pi) {
          var plus = document.createElement('span');
          plus.className = 'shortcut-plus';
          plus.textContent = '+';
          keys.appendChild(plus);
        }
        var k = document.createElement('kbd');
        k.textContent = part;
        keys.appendChild(k);
      });
    });
    row.appendChild(desc);
    row.appendChild(keys);
    return row;
  }

  function renderShortcuts() {
    if (!shortcutsBodyEl) return;
    shortcutsBodyEl.innerHTML = '';
    var frag = document.createDocumentFragment();
    shortcutGroups().forEach(function (grp) {
      var head = document.createElement('div');
      head.className = 'shortcut-group';
      head.textContent = grp.group;
      frag.appendChild(head);
      grp.items.forEach(function (item) { frag.appendChild(shortcutRow(item)); });
    });
    shortcutsBodyEl.appendChild(frag);
  }

  function openShortcuts() {
    if (!shortcutsModalEl) return;
    renderShortcuts();
    shortcutsModalEl.classList.remove('hidden');
    shortcutsModalEl.setAttribute('aria-hidden', 'false');
  }

  function closeShortcuts() {
    if (!shortcutsModalEl) return;
    shortcutsModalEl.classList.add('hidden');
    shortcutsModalEl.setAttribute('aria-hidden', 'true');
  }

  function shortcutsIsOpen() {
    return !!(shortcutsModalEl && !shortcutsModalEl.classList.contains('hidden'));
  }

  /** Ctrl+Shift+C — bring the graph back into frame without changing selection. */
  function recenterView() {
    state.follow = null;
    fitView();
    scheduleViewerSave();
  }

  function onKeyDown(ev) {
    // While an index is running, Escape is the Stop control: cancelling the
    // wrong-folder build is the only thing that matters then, so it takes
    // priority over closing a panel or clearing the selection.
    if (ev.key === 'Escape' && indexRunning) {
      ev.preventDefault();
      stopIndex();
      return;
    }
    var t = ev.target;
    var typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT');

    // Ctrl/Cmd+Shift+C works everywhere (even while typing) and is checked before
    // the plain "C" code shortcut below.
    if ((ev.ctrlKey || ev.metaKey) && ev.shiftKey && (ev.key === 'c' || ev.key === 'C')) {
      ev.preventDefault();
      recenterView();
      return;
    }

    if (ev.key === 'Escape') {
      if (typing && t === searchInputEl && !suggestionsEl.classList.contains('hidden')) {
        hideSuggestions();
        return;
      }
      if (shortcutsIsOpen()) { closeShortcuts(); return; }
      if (modalIsOpen()) { closeModal(); return; }
      if (state.source.open) { closeSource(); return; }
      if (state.mode === 'find') { setMode('graph', false); return; }
      if (typing) { t.blur(); return; }
      if (tooltipPinned) { hideTooltip(true); return; }
      clearSelection();
      return;
    }

    if (typing) return;

    // The shortcuts panel is modal: Escape closes it, "?" toggles it, everything
    // else must not leak through to the graph underneath.
    if (shortcutsIsOpen() && ev.key !== '?') return;

    // Document tabs / split (window.CsTabs): the pane's document stack.
    if (window.CsTabs) {
      if (ev.ctrlKey && !ev.altKey && !ev.metaKey && ev.key === 'Tab') {
        ev.preventDefault();
        window.CsTabs.cycle(ev.shiftKey ? -1 : 1);
        return;
      }
      if (ev.altKey && ev.shiftKey && !ev.ctrlKey && !ev.metaKey && (ev.key === 'w' || ev.key === 'W')) {
        ev.preventDefault();
        window.CsTabs.closeOthers();
        return;
      }
      if (ev.altKey && !ev.shiftKey && !ev.ctrlKey && !ev.metaKey && (ev.key === 'w' || ev.key === 'W')) {
        ev.preventDefault();
        window.CsTabs.closeActive();
        return;
      }
      if (ev.altKey && !ev.shiftKey && !ev.ctrlKey && !ev.metaKey && (ev.key === 's' || ev.key === 'S')) {
        ev.preventDefault();
        window.CsTabs.toggleSplit();
        return;
      }
    }

    // Arrange / spacing / diagonal shortcuts live in arrange.js.
    if (arrange.key(ev)) return;

    // Find mode: Up/Down move the result selection, Enter opens it (source viewer too).
    if (state.mode === 'find') {
      if (ev.key === 'ArrowDown') { ev.preventDefault(); moveFindSelection(1); return; }
      if (ev.key === 'ArrowUp') { ev.preventDefault(); moveFindSelection(-1); return; }
      if (ev.key === 'Enter') { ev.preventDefault(); activateFindSelection(); return; }
    }

    if (ev.key === 'Backspace') {
      ev.preventDefault();
      navigateBack();
    } else if ((ev.key === 'c' || ev.key === 'C') && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
      ev.preventDefault();
      viewCodeFor(state.selected || state.hovered);
    } else if (ev.key === '?') {
      ev.preventDefault();
      if (shortcutsIsOpen()) closeShortcuts(); else openShortcuts();
    } else if (ev.key === '/') {
      ev.preventDefault();
      setMode('graph', false);
      searchInputEl.focus();
      searchInputEl.select();
    } else if (ev.key === 'f' || ev.key === 'F') {
      ev.preventDefault();
      setMode('find', true);
    } else if (ev.key === '+' || ev.key === '=') {
      ev.preventDefault();
      zoomAt(state.w / 2, state.h / 2, 1.25);
    } else if (ev.key === '-' || ev.key === '_') {
      ev.preventDefault();
      zoomAt(state.w / 2, state.h / 2, 1 / 1.25);
    } else if (ev.key === 'r' || ev.key === 'R') {
      startLayout({ reinitialize: false });   // re-settle (pinned nodes stay pinned)
    }
  }

  function wireEvents() {
    $('#browseBtn').addEventListener('click', function () {
      openPicker(state.rootChoice && state.rootChoice !== '.' ? state.rootChoice : '');
    });
    indexBtnEl.addEventListener('click', doIndex);
    var stopBtnEl = $('#stopIndexBtn');
    if (stopBtnEl) stopBtnEl.addEventListener('click', stopIndex);

    // top-right shortcuts / settings button
    if (shortcutsBtnEl) shortcutsBtnEl.addEventListener('click', openShortcuts);
    if (shortcutsModalEl) {
      shortcutsModalEl.addEventListener('click', function (ev) {
        if (ev.target === shortcutsModalEl) closeShortcuts();
      });
      var scClose = document.getElementById('shortcutsClose');
      var scDone = document.getElementById('shortcutsDone');
      if (scClose) scClose.addEventListener('click', closeShortcuts);
      if (scDone) scDone.addEventListener('click', closeShortcuts);
    }
    if (uiThemeEl) {
      uiThemeEl.addEventListener('change', function () { applyUiTheme(uiThemeEl.value); });
    }
    rootInputEl.addEventListener('input', function () {
      state.rootChoice = rootInputEl.value.trim() || '.';
      indexBtnEl.disabled = !rootInputEl.value.trim() && !state.indexed;
    });
    rootInputEl.addEventListener('keydown', function (ev) {
      if (ev.key === 'Enter' && !indexBtnEl.disabled) doIndex();
    });

    searchInputEl.addEventListener('input', onSearchInput);
    searchInputEl.addEventListener('focus', function () {
      if (state.mode === 'find') return;
      if (searchInputEl.value.trim() && (state.searchResults.length || suggestionsEl.innerHTML)) {
        showSuggestions();
      }
    });
    searchInputEl.addEventListener('keydown', function (ev) {
      if (state.mode === 'find') {
        if (ev.key === 'Enter') {
          ev.preventDefault();
          window.clearTimeout(findTimer);
          runFind(searchInputEl.value.trim());
        } else if (ev.key === 'ArrowDown') {
          ev.preventDefault();
          moveFindSelection(1);
        } else if (ev.key === 'ArrowUp') {
          ev.preventDefault();
          moveFindSelection(-1);
        }
        return;
      }
      if (ev.key === 'ArrowDown') { ev.preventDefault(); moveSuggestion(1); }
      else if (ev.key === 'ArrowUp') { ev.preventDefault(); moveSuggestion(-1); }
      else if (ev.key === 'Enter') { ev.preventDefault(); commitSearch(); }
    });
    suggestionsEl.addEventListener('mousedown', function (ev) {
      // mousedown (not click) so the input does not blur first
      var btn = ev.target.closest ? ev.target.closest('.sug') : null;
      if (!btn) return;
      ev.preventDefault();
      var id = btn.getAttribute('data-id');
      hideSuggestions();
      searchInputEl.blur();
      centerNode(id);
    });
    document.addEventListener('mousedown', function (ev) {
      if (!suggestionsEl.classList.contains('hidden') &&
          !suggestionsEl.contains(ev.target) && ev.target !== searchInputEl) {
        hideSuggestions();
      }
    });

    // collapsible sections
    [['filtersHead', 'filtersSection'], ['relationsHead', 'relationsSection'],
     ['pubHead', 'pubSection'], ['strHead', 'strSection'], ['chainHead', 'chainSection']]
      .forEach(function (pair) {
        var head = document.getElementById(pair[0]);
        var sec = document.getElementById(pair[1]);
        if (!head || !sec) return;
        head.addEventListener('click', function () {
          var collapsed = sec.classList.toggle('collapsed');
          var caret = head.querySelector('.caret');
          if (caret) caret.textContent = collapsed ? '\u25b8' : '\u25be';
          if (!collapsed) {
            if (sec === pubSectionEl) loadDefinitions();
            if (sec === strSectionEl) loadStrings();
          }
        });
      });

    // node drawer
    $('#nodeCodeBtn').addEventListener('click', function () { viewCodeFor(state.selected); });
    $('#nodeDrawerClose').addEventListener('click', function () {
      state.selected = null;
      state.detail = null;
      nodeDrawerEl.classList.add('hidden');
    });
    nodePropsEl.addEventListener('click', function (ev) {
      var b = ev.target.closest ? ev.target.closest('button[data-act]') : null;
      if (!b || !state.detail || !state.detail.node) return;
      var n = state.detail.node;
      var act = b.getAttribute('data-act');
      if (act === 'expand') expandNode(n.id, { all: true });
      else if (act === 'hidech') toggleHideChildren(n.id);
      else if (act === 'source') viewCodeFor(n.id);
      else if (act === 'chain') {
        chainSectionEl.classList.remove('collapsed');
        var caret = $('#chainHead').querySelector('.caret');
        if (caret) caret.textContent = '\u25be';
        runChain();
      }
    });

    // facets / filters
    forEachNodeList(document.querySelectorAll('.linkbtn[data-kinds]'), function (b) {
      b.addEventListener('click', function () { setAllKinds(b.getAttribute('data-kinds') === 'all'); });
    });
    forEachNodeList(document.querySelectorAll('.linkbtn[data-edges]'), function (b) {
      b.addEventListener('click', function () { setAllEdges(b.getAttribute('data-edges') === 'all'); });
    });
    $('#resetFilters').addEventListener('click', resetFilters);
    $('#pruneHidden').addEventListener('click', pruneHidden);
    // one namespace at a time: the select isolates a namespace, "all" clears it
    if (nsSelectEl) {
      nsSelectEl.addEventListener('change', function () { setNamespaceFilter(nsSelectEl.value); });
    }
    if (nsDeepEl) {
      nsDeepEl.addEventListener('change', function () { setNamespaceDeep(nsDeepEl.checked); });
    }
    if ($('#nsAllBtn')) $('#nsAllBtn').addEventListener('click', function () { setNamespaceFilter(''); });
    groupChildrenEl.addEventListener('change', function () {
      state.filters.groupChildren = groupChildrenEl.checked;
      if (hideChildrenEl) hideChildrenEl.checked = !groupChildrenEl.checked;
      updateHideChildrenToggle();
      updateCanvasNotice();
      scheduleViewerSave();
    });
    groupEdgesEl.addEventListener('change', function () {
      state.filters.groupEdges = groupEdgesEl.checked;
      $('#kindEdgeWrap').classList.toggle('off', !groupEdgesEl.checked);
      scheduleViewerSave();
    });

    // public definitions / strings
    pubSearchEl.addEventListener('input', function () {
      state.defs.filter = pubSearchEl.value.trim();
      renderPubList();
    });
    strSearchEl.addEventListener('input', onStringsFilter);

    // resolve chain
    $('#chainRun').addEventListener('click', runChain);
    $('#chainFlatBtn').addEventListener('click', function () {
      state.chain.flat = !state.chain.flat;
      renderChain();
    });
    chainDepthEl.addEventListener('change', function () { if (state.selected) runChain(); });
    chainDirEl.addEventListener('change', function () { if (state.selected) runChain(); });

    // source theme
    if (sourceThemeEl) {
      sourceThemeEl.addEventListener('change', function () {
        applySourceTheme(sourceThemeEl.value);
        if (state.source.payload) renderSource(state.source.payload);
        scheduleViewerSave();
      });
    }

    // navigation: back / up
    $('#backBtn').addEventListener('click', navigateBack);
    $('#upBtn').addEventListener('click', navigateUp);

    // global "Hide relations" + the group chip stay in sync
    hideChildrenEl.addEventListener('change', function () {
      state.filters.groupChildren = !hideChildrenEl.checked;
      if (groupChildrenEl) groupChildrenEl.checked = state.filters.groupChildren;
      updateHideChildrenToggle();
      updateCanvasNotice();
      scheduleViewerSave();
    });

    // mode switch + find panel
    modeGraphEl.addEventListener('click', function () { setMode('graph', true); });
    modeFindEl.addEventListener('click', function () { setMode('find', true); });
    findCloseEl.addEventListener('click', function () { setMode('graph', false); });
    findResultsEl.addEventListener('click', function (ev) {
      var el = ev.target.closest ? ev.target.closest('.frow') : null;
      if (!el) return;
      for (var i = 0; i < state.find.rows.length; i++) {
        if (state.find.rows[i].el === el) { activateFindRow(state.find.rows[i]); return; }
      }
    });

    // source viewer
    $('#srcCloseBtn').addEventListener('click', closeSource);
    $('#srcCopyBtn').addEventListener('click', copySourceCode);
    $('#srcRefBtn').addEventListener('click', copySourceRef);
    $('#srcModeBtn').addEventListener('click', toggleSourceMode);
    $('#srcMethodBtn').addEventListener('click', sourceWholeMethod);
    // v2: hand the current document to a brand-new window with its own thread
    // and its own Monaco worker, so a huge document cannot slow this one down.
    $('#srcPopBtn').addEventListener('click', function () {
      var st = state.source;
      if (!window.CsWindows) return;
      var target = st.nodeId || (st.usageNode && st.usageNode.id) || null;
      if (target) window.CsWindows.openDoc(target, st.srcMode || 'doc');
      else if (st.file && st.line) window.CsWindows.openFile(st.file, st.line, 'raw');
    });
    $('#srcWindowBtn').addEventListener('click', function () {
      var st = state.source;
      if (st.file && st.line) openSource({ file: st.file, line: st.line, context: SOURCE_CONTEXT });
    });
    $('#srcPrevBtn').addEventListener('click', function () { sourceStep(-1); });
    $('#srcNextBtn').addEventListener('click', function () { sourceStep(1); });

    forEachNodeList(document.querySelectorAll('.linkbtn[data-kinds]'), function (b) {
      b.addEventListener('click', function () { setAllKinds(b.getAttribute('data-kinds') === 'all'); });
    });
    forEachNodeList(document.querySelectorAll('.linkbtn[data-edges]'), function (b) {
      b.addEventListener('click', function () { setAllEdges(b.getAttribute('data-edges') === 'all'); });
    });

    hideIsolatedEl.addEventListener('change', function () {
      state.filters.hideIsolated = hideIsolatedEl.checked;
      scheduleViewerSave();
    });

    if (hideEdgeLabelsEl) {
      hideEdgeLabelsEl.addEventListener('change', function () {
        state.filters.hideEdgeLabels = hideEdgeLabelsEl.checked;
        scheduleViewerSave();
      });
    }

    // Depth: a free-form number box (0–9999999) with the native up/down
    // arrows and wheel support, replacing the fixed 1/2/3 dropdown.
    function commitDepth() {
      var n = parseInt(depthSelectEl.value, 10);
      if (!isFinite(n)) n = state.depth;
      n = clamp(Math.round(n), DEPTH_MIN, DEPTH_MAX);
      depthSelectEl.value = String(n);
      if (n === state.depth) return;
      state.depth = n;
      scheduleViewerSave();
      if (state.focus) centerNode(state.focus);
      else loadWholeGraph(false);
    }
    var wheelDepthTimer = null;
    if (depthSelectEl) {
      depthSelectEl.value = String(state.depth);
      depthSelectEl.addEventListener('change', commitDepth);
      depthSelectEl.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter') { ev.preventDefault(); commitDepth(); depthSelectEl.blur(); }
      });
      depthSelectEl.addEventListener('blur', function () {
        commitDepth();   // never leave the box empty or out of range
      });
      depthSelectEl.addEventListener('wheel', function (ev) {
        ev.preventDefault();
        var n = parseInt(depthSelectEl.value, 10);
        if (!isFinite(n)) n = state.depth;
        var step = ev.shiftKey ? 10 : 1;
        depthSelectEl.value = String(clamp(n + (ev.deltaY < 0 ? step : -step), DEPTH_MIN, DEPTH_MAX));
        // Spin the number on every tick, but re-layout once the wheel settles.
        if (wheelDepthTimer) clearTimeout(wheelDepthTimer);
        wheelDepthTimer = setTimeout(commitDepth, 180);
      }, { passive: false });
    }

    // modal
    $('#modalClose').addEventListener('click', closeModal);
    $('#modalCancel').addEventListener('click', closeModal);
    modalSelectEl.addEventListener('click', chooseCurrentFolder);
    modalUpEl.addEventListener('click', function () {
      var parent = state.picker.parent;
      browseTo(parent === null || parent === undefined ? '' : parent);
    });
    $('#modalGo').addEventListener('click', function () { browseTo(modalPathInputEl.value.trim()); });
    modalPathInputEl.addEventListener('keydown', function (ev) {
      if (ev.key === 'Enter') { ev.preventDefault(); browseTo(modalPathInputEl.value.trim()); }
    });
    $('#modalRoot').addEventListener('click', function () { browseTo('/'); });
    $('#modalRepo').addEventListener('click', function () { browseTo(state.root || ''); });
    $('#modalHome').addEventListener('click', function () { browseTo(homePath()); });
    modalBodyEl.addEventListener('click', function (ev) {
      var btn = ev.target.closest ? ev.target.closest('.pick[data-nav]') : null;
      if (btn) browseTo(btn.getAttribute('data-nav'));
    });
    modalEl.addEventListener('mousedown', function (ev) {
      if (ev.target === modalEl) closeModal();
    });

    // canvas
    canvas.addEventListener('pointerdown', onPointerDown);
    canvas.addEventListener('pointermove', onPointerMove);
    canvas.addEventListener('pointerup', onPointerUp);
    canvas.addEventListener('pointercancel', onPointerUp);
    canvas.addEventListener('pointerleave', function () { scheduleTooltipHide(); state.hovered = null; });
    canvas.addEventListener('contextmenu', onContextMenu);
    // Middle-button drag pans: swallow the native middle-click autoscroll so the
    // pointermove stream reaches onPointerMove uninterrupted.
    canvas.addEventListener('mousedown', function (ev) { if (ev.button === 1) ev.preventDefault(); });
    tooltipEl.addEventListener('pointerenter', function () {
      tooltipHover = true;
      window.clearTimeout(tooltipHideTimer);
    });
    tooltipEl.addEventListener('pointerleave', function () { tooltipHover = false; hideTooltip(); });
    tooltipEl.addEventListener('mousedown', function (ev) { ev.preventDefault(); });
    tooltipEl.addEventListener('contextmenu', function (ev) { ev.preventDefault(); ev.stopPropagation(); });
    tooltipEl.addEventListener('click', onTooltipClick);
    canvas.addEventListener('wheel', onWheel, { passive: false });
    canvas.addEventListener('dblclick', onDblClick);

    $('#zoomInBtn').addEventListener('click', function () { zoomAt(state.w / 2, state.h / 2, 1.25); });
    $('#zoomOutBtn').addEventListener('click', function () { zoomAt(state.w / 2, state.h / 2, 1 / 1.25); });
    $('#fitBtn').addEventListener('click', fitView);
    $('#wholeGraphBtn').addEventListener('click', function () { loadFullGraph(true); });
    $('#resettleBtn').addEventListener('click', function () { startLayout({ reinitialize: false }); });

    wireLayoutControls();

    window.addEventListener('keydown', onKeyDown);
    // Shift / marquee cursor and box-selection chrome belong to arrange.js
    arrange.install();
    // The MCP-parity edit panel (rename / qualify / select / write / …). It is
    // installed here rather than in app.js so a missing edit.js cannot break boot.
    if (window.CSEdit && typeof window.CSEdit.install === 'function') {
      try { window.CSEdit.install(); } catch (e) { report(e); }
    }
    window.addEventListener('resize', resize);
    if (window.ResizeObserver) new ResizeObserver(resize).observe(canvasWrap);
  }

  // ================================================================== layout controls

  function syncLayoutControls() {
    if (spacingEl) spacingEl.value = String(state.settings.spacing);
    if (spacingOutEl) spacingOutEl.textContent = String(state.settings.spacing);
    if (iterEl) iterEl.value = String(state.settings.iterations);
    if (iterOutEl) iterOutEl.textContent = String(state.settings.iterations);
    if (curveEl) curveEl.value = String(state.settings.curvature);
    if (curveOutEl) curveOutEl.textContent = state.settings.curvature.toFixed(2);
    if (nodeSizeEl) nodeSizeEl.value = String(state.settings.nodeSize);
    if (nodeSizeOutEl) nodeSizeOutEl.textContent = String(state.settings.nodeSize);
  }

  function wireLayoutControls() {
    if (spacingEl) {
      // live readout while dragging, re-layout on release (the FA2 pass is heavy)
      spacingEl.addEventListener('input', function () {
        spacingOutEl.textContent = spacingEl.value;
      });
      spacingEl.addEventListener('change', function () {
        var next = clamp(parseFloat(spacingEl.value) || SPACING_DEFAULT, SPACING_MIN, SPACING_MAX);
        var delta = Math.abs(next - state.settings.spacing);
        state.settings.spacing = next;
        spacingOutEl.textContent = String(next);
        scheduleViewerSave();
        if (delta >= 1) {
          // VRCX jitters by lerp(0.5, 2.0, t) when the spacing moves, then re-runs FA2
          var magnitude = 0.5 + (2.0 - 0.5) * layoutT();
          if (delta >= 8) jitterPositions(magnitude);
          startLayout({ reinitialize: false });
        }
      });
    }
    if (iterEl) {
      iterEl.addEventListener('input', function () { iterOutEl.textContent = iterEl.value; });
      iterEl.addEventListener('change', function () {
        state.settings.iterations = clamp(parseInt(iterEl.value, 10) || ITER_DEFAULT, ITER_MIN, ITER_MAX);
        iterOutEl.textContent = String(state.settings.iterations);
        scheduleViewerSave();
        startLayout({ reinitialize: false });
      });
    }
    if (curveEl) {
      curveEl.addEventListener('input', function () {
        state.settings.curvature = clamp(parseFloat(curveEl.value) || 0, CURVE_MIN, CURVE_MAX);
        curveOutEl.textContent = state.settings.curvature.toFixed(2);
      });
      curveEl.addEventListener('change', scheduleViewerSave);
    }
    if (nodeSizeEl) {
      // live: the glyph grows under the cursor without a layout pass
      nodeSizeEl.addEventListener('input', function () {
        state.settings.nodeSize = clamp(parseInt(nodeSizeEl.value, 10) || NODE_SIZE_DEFAULT,
                                        NODE_SIZE_MIN, NODE_SIZE_MAX);
        nodeSizeOutEl.textContent = String(state.settings.nodeSize);
        chipMetricsCache.clear();
      });
      // release: the glyph feeds the FA2 node radius, so re-settle once
      nodeSizeEl.addEventListener('change', function () {
        chipMetricsCache.clear();
        startLayout({ reinitialize: false });
        scheduleViewerSave();
      });
    }
  }

  // ================================================================== document tabs & splits
  //
  // Several documents in one pane (VSCode-style tabs), a second document beside
  // them (a split), and restoring all of it through /api/viewer_state.
  //
  // The pane's plumbing (openSource/closeSource/renderSource) lives in source.js
  // and editor.js, which other work owns; this module therefore wraps the three
  // globals those files publish instead of editing them:
  //
  //   * openSource   — every open becomes a tab (or activates the tab that already
  //                    holds that document) instead of replacing the pane;
  //   * renderSource — remembers each tab's payload + source-pane snapshot, and is
  //                    the cache key for the Monaco model below;
  //   * closeSource  — closes the active tab; the pane only hides when the last
  //                    one goes.
  //
  // Monaco models are cached per document by routing `monaco.editor.createModel`
  // through this module for the duration of a render. editor.js keeps owning the
  // model bookkeeping; a document that is already open simply reuses its model
  // instead of rebuilding a 35k-line buffer on every tab switch.
  window.CsTabs = (function () {
    var MAX_TABS = 24;
    var MAX_MODELS = 16;

    var installed = false;
    var tabs = [];
    var activeTab = null;
    var splitKeyState = null;
    var splitFrac = 0.5;
    var implicitSeq = 0;

    var barEl = null, wrapEl = null, dividerEl = null,
        splitBodyEl = null, splitHostEl = null, splitLabelEl = null;
    var splitEditor = null;

    var origOpenSource = null, origRender = null, origCloseSource = null;

    var pendingRenderTab = null;
    var pendingModelKey = null;
    var lastRenderedKey = null;
    var lastModelEvent = null;
    var modelCache = Object.create(null);
    var modelOrder = [];
    var modelsCreated = 0, modelsReused = 0;
    var patchedMonaco = null;
    var dragging = false;

    function report(err) {
      // console.warn only: the tab layer must never add a console.error.
      if (window.console && console.warn) console.warn('[csgraph tabs]', err);
    }

    function scheduleSave() {
      try { if (typeof scheduleViewerSave === 'function') scheduleViewerSave(); } catch (e) { /* ignore */ }
    }

    // ------------------------------------------------------------- tab model

    function basename(p) {
      p = String(p || '');
      var i = p.lastIndexOf('/');
      return i >= 0 ? p.slice(i + 1) : p;
    }

    function shortNode(id) {
      var bits = String(id || '').split('.');
      return bits.length > 2 ? bits.slice(-2).join('.') : (bits[bits.length - 1] || id);
    }

    function numOr(v, dflt) {
      return (typeof v === 'number' && isFinite(v) && v > 0) ? Math.floor(v) : dflt;
    }

    /** The document identity of an openSource() call, independent of mode/line. */
    function keyFor(p) {
      if (p && p.node) return 'node:' + p.node;
      if (p && p.file) return 'file:' + p.file;
      return 'doc:unknown';
    }

    function copyParams(p) {
      var out = {};
      if (!p) return out;
      Object.keys(p).forEach(function (k) { out[k] = p[k]; });
      return out;
    }

    /** Accept only the shapes openSource() understands; anything else is not a tab. */
    function normalize(params) {
      if (!params || typeof params !== 'object') return null;
      if (typeof params.node === 'string' && params.node) {
        var p = { node: params.node };
        if (typeof params.mode === 'string') p.mode = params.mode;
        if (typeof params.match === 'string') p.match = params.match;
        return p;
      }
      if (typeof params.file === 'string' && params.file) {
        var f = { file: params.file, line: numOr(params.line, 1) };
        if (typeof params.mode === 'string') f.mode = params.mode;
        if (typeof params.context === 'number') f.context = params.context;
        if (typeof params.to === 'number') f.to = params.to;
        if (typeof params.match === 'string') f.match = params.match;
        return f;
      }
      return null;
    }

    function mergeParams(tab, p) {
      if (!p) return;
      Object.keys(p).forEach(function (k) { tab.params[k] = p[k]; });
    }

    function defaultLabel(params) {
      if (params && params.node) return shortNode(params.node);
      if (params && params.file) return basename(params.file) + (params.line ? ':' + params.line : '');
      return 'document';
    }

    function makeTab(key, params) {
      return {
        key: key, params: params || {}, data: null, snap: null,
        label: defaultLabel(params), modelKey: null
      };
    }

    function byKey(key) {
      for (var i = 0; i < tabs.length; i++) if (tabs[i].key === key) return tabs[i];
      return null;
    }

    function indexOfKey(key) {
      for (var i = 0; i < tabs.length; i++) if (tabs[i].key === key) return i;
      return -1;
    }

    function firstOther(except) {
      for (var i = 0; i < tabs.length; i++) if (tabs[i] !== except) return tabs[i];
      return null;
    }

    /** Per-tab snapshot of the global state.source object. */
    function snapshotSource() {
      var st = state.source || {};
      var out = {};
      Object.keys(st).forEach(function (k) { out[k] = st[k]; });
      return out;
    }

    function restoreSnapshot(snap) {
      var st = state.source;
      if (!st || !snap) return;
      Object.keys(st).forEach(function (k) { delete st[k]; });
      Object.keys(snap).forEach(function (k) { st[k] = snap[k]; });
    }

    // ----------------------------------------------------- Monaco model cache

    function attachedModel(m) {
      if (!m) return false;
      if (window.CsEditor && window.CsEditor.model && window.CsEditor.model() === m) return true;
      if (splitEditor && splitEditor.getModel && splitEditor.getModel() === m) return true;
      return false;
    }

    function touchModel(key) {
      var i = modelOrder.indexOf(key);
      if (i >= 0) modelOrder.splice(i, 1);
      modelOrder.push(key);
    }

    function keepModel(m) {
      if (!m || m.__csKeep) return;
      try {
        m.__csKeep = true;
        m.__realDispose = m.dispose;
        m.dispose = function () { /* cached: keep the buffer for tab switches */ };
      } catch (e) { /* a frozen model just falls back to editor.js behaviour */ }
    }

    function storeModel(key, m) {
      modelCache[key] = m;
      touchModel(key);
      var guard = modelOrder.length * 2 + 4;
      while (modelOrder.length > MAX_MODELS && guard-- > 0) {
        var victim = modelOrder.shift();
        var vm = modelCache[victim];
        if (!vm) continue;
        if (attachedModel(vm)) { modelOrder.push(victim); continue; }  // in use: keep
        delete modelCache[victim];
        try { if (vm.__realDispose) vm.__realDispose(); } catch (e) { /* ignore */ }
      }
    }

    function disposeTabModel(tab) {
      if (!tab || !tab.modelKey) return;
      var m = modelCache[tab.modelKey];
      if (!m) return;
      delete modelCache[tab.modelKey];
      var i = modelOrder.indexOf(tab.modelKey);
      if (i >= 0) modelOrder.splice(i, 1);
      // A model the split editor is still showing must be detached before it can
      // be disposed (or it will be dropped from the cache still attached).
      if (splitEditor && splitEditor.getModel && splitEditor.getModel() === m) {
        try { splitEditor.setModel(null); } catch (e) { /* ignore */ }
      }
      if (!attachedModel(m)) {
        try { if (m.__realDispose) m.__realDispose(); } catch (e) { /* ignore */ }
      }
    }

    /** True when a new openSource() call changes what the tab should display. */
    function paramsDiffer(tab, p) {
      var a = tab.params || {};
      return (a.mode || '') !== (p.mode || '') ||
             (a.line || 0) !== (p.line || 0) ||
             (a.context || 0) !== (p.context || 0) ||
             (a.to || 0) !== (p.to || 0) ||
             (a.match || '') !== (p.match || '');
    }

    /** Route editor.js's createModel through the cache while a render is in flight. */
    function ensureModelPatch() {
      var ed = window.CsEditor;
      if (!ed || !ed.monaco) return;
      var monaco = ed.monaco();
      if (!monaco || !monaco.editor || typeof monaco.editor.createModel !== 'function') return;
      if (patchedMonaco === monaco) return;
      var orig = monaco.editor.createModel;
      monaco.editor.createModel = function (value, language, uri) {
        var key = pendingModelKey;
        if (key && !uri) {
          var cached = modelCache[key];
          if (cached && (typeof cached.isDisposed !== 'function' || !cached.isDisposed())) {
            modelsReused++;
            lastModelEvent = 'hit:' + key;
            touchModel(key);
            return cached;
          }
          var made = orig.apply(monaco.editor, arguments);
          keepModel(made);
          storeModel(key, made);
          modelsCreated++;
          lastModelEvent = 'create:' + key;
          return made;
        }
        return orig.apply(monaco.editor, arguments);
      };
      patchedMonaco = monaco;
    }

    function modelKeyFor(d, tab) {
      var st = state.source || {};
      var params = (tab && tab.params) || {};
      var nodeId = params.node || st.nodeId || '';
      var mode = (d && d.mode) || st.srcMode || '';
      if (d && d.doc && nodeId) return 'doc|' + nodeId + '|' + mode;
      var path = (d && (d.path || d.file)) || st.file || params.file || '';
      var start = d ? (d.start || 0) : (st.start || 0);
      var end = d ? (d.end || 0) : (st.end || 0);
      return 'raw|' + path + '|' + start + '-' + end + '|' + mode;
    }

    function linesText(lines) {
      var out = [];
      lines = lines || [];
      for (var i = 0; i < lines.length; i++) out.push(lines[i].text == null ? '' : lines[i].text);
      return out.join('\n');
    }

    // ---------------------------------------------------------- source fetch

    /** The /api/source query for a tab that has not been rendered yet. */
    function queryForTab(tab) {
      var p = tab.params || {};
      var mode = p.mode;
      var q = {};
      if (p.node) {
        q.node = p.node;
        q.mode = mode || 'doc';
      } else {
        q.file = p.file;
        q.line = p.line || 1;
        if (p.to) q.to = p.to; else q.context = p.context || SOURCE_CONTEXT;
        q.mode = mode || 'raw';
      }
      return q;
    }

    function fetchTab(tab) {
      return apiGet('/api/source', queryForTab(tab)).then(function (d) {
        tab.data = d;
        return d;
      });
    }

    // ------------------------------------------------------------- tab bar

    /** A pop-out document window is one document and stays chrome-free. */
    function docWindow() {
      return !!(window.CsWindows && window.CsWindows.isDocWindow && window.CsWindows.isDocWindow());
    }

    function renderTabBar() {
      if (!barEl) return;
      barEl.innerHTML = '';
      // No document at all, or a single document in a pop-out window: no bar.
      barEl.classList.toggle('hidden', tabs.length === 0 || (docWindow() && tabs.length <= 1));
      if (!tabs.length) return;
      var frag = document.createDocumentFragment();
      tabs.forEach(function (t) {
        var b = document.createElement('div');
        b.className = 'cs-tab' + (t === activeTab ? ' active' : '');
        b.setAttribute('role', 'tab');
        b.setAttribute('data-key', t.key);
        b.setAttribute('aria-selected', t === activeTab ? 'true' : 'false');
        b.title = (t.params && (t.params.file || t.params.node)) || t.key;
        var lab = document.createElement('span');
        lab.className = 'cs-tab-label';
        lab.textContent = t.label || t.key;
        b.appendChild(lab);
        if (splitKeyState === t.key) {
          var mark = document.createElement('span');
          mark.className = 'cs-tab-splitmark';
          mark.textContent = '▌';
          mark.title = 'Shown in the split';
          b.appendChild(mark);
        }
        var x = document.createElement('button');
        x.type = 'button';
        x.className = 'cs-tab-x';
        x.setAttribute('data-close', t.key);
        x.title = 'Close tab';
        x.textContent = '✕';
        b.appendChild(x);
        frag.appendChild(b);
      });
      var actions = document.createElement('div');
      actions.className = 'cs-tab-actions';
      if (!docWindow()) {
        var splitBtn = document.createElement('button');
        splitBtn.type = 'button';
        splitBtn.id = 'csSplitBtn';
        splitBtn.className = 'btn btn-ghost';
        splitBtn.textContent = isSplitOn() ? '▤ Unsplit' : '▥ Split';
        splitBtn.title = 'Show a second document beside this one (Alt+S)';
        splitBtn.disabled = tabs.length < 2 && !isSplitOn();
        actions.appendChild(splitBtn);
      }
      var others = document.createElement('button');
      others.type = 'button';
      others.id = 'csTabCloseOthers';
      others.className = 'btn btn-ghost';
      others.textContent = 'Close others';
      others.title = 'Close every tab but this one (Alt+Shift+W)';
      others.disabled = tabs.length < 2;
      actions.appendChild(others);
      frag.appendChild(actions);
      barEl.appendChild(frag);
    }

    function onBarClick(ev) {
      var t = ev.target;
      var close = t.closest ? t.closest('[data-close]') : null;
      if (close) { ev.preventDefault(); closeTab(close.getAttribute('data-close')); return; }
      if (t.closest && t.closest('#csSplitBtn')) { ev.preventDefault(); toggleSplit(); return; }
      if (t.closest && t.closest('#csTabCloseOthers')) { ev.preventDefault(); closeOthers(); return; }
      var tab = t.closest ? t.closest('.cs-tab[data-key]') : null;
      if (tab) { ev.preventDefault(); activateByKey(tab.getAttribute('data-key')); }
    }

    function onBarAux(ev) {
      if (ev.button !== 1) return;
      var tab = ev.target.closest ? ev.target.closest('.cs-tab[data-key]') : null;
      if (!tab) return;
      ev.preventDefault();
      closeTab(tab.getAttribute('data-key'));
    }

    // ------------------------------------------------------------- activate

    function activateByKey(key) {
      var tab = byKey(key);
      if (tab) activateTab(tab);
    }

    function activateTab(tab) {
      if (!tab || !installed) return;
      activeTab = tab;
      if (tab.data) {
        pendingRenderTab = tab;
        window.srcSeq = (window.srcSeq || 0) + 1;   // a pending fetch must not win
        restoreSnapshot(tab.snap);
        // Route through the wrapper, not the captured original: the wrapper is
        // what re-caches the payload, snapshots the pane and keys the model.
        try { window.renderSource(tab.data); } catch (e) { report(e); }
      } else {
        pendingRenderTab = tab;
        try { origOpenSource(copyParams(tab.params)); } catch (e) { report(e); }
      }
      renderTabBar();
      scheduleSave();
    }

    function cycle(dir) {
      if (tabs.length < 2) return;
      var i = tabs.indexOf(activeTab);
      if (i < 0) i = 0;
      i = (i + (dir < 0 ? -1 : 1) + tabs.length) % tabs.length;
      activateTab(tabs[i]);
    }

    function trimTabs() {
      var guard = tabs.length * 2 + 4;
      while (tabs.length > MAX_TABS && guard-- > 0) {
        var victim = null;
        for (var i = 0; i < tabs.length; i++) {
          if (tabs[i] !== activeTab && tabs[i].key !== splitKeyState) { victim = tabs[i]; break; }
        }
        if (!victim) break;
        tabs.splice(tabs.indexOf(victim), 1);
        disposeTabModel(victim);
      }
    }

    // ------------------------------------------------------------- close

    function closeTab(key) {
      var idx = indexOfKey(key);
      if (idx < 0) return false;
      var tab = tabs[idx];
      var wasActive = (tab === activeTab);
      tabs.splice(idx, 1);
      if (splitKeyState === key) {
        var alt = firstOther(wasActive ? null : activeTab);
        splitKeyState = alt ? alt.key : null;
      }
      disposeTabModel(tab);
      if (wasActive) {
        activeTab = null;
        var next = tabs[Math.min(idx, tabs.length - 1)] || null;
        if (next) { activateTab(next); return true; }
        closeSplitEditor();
        try { origCloseSource(); } catch (e) { report(e); }
      }
      renderTabBar();
      if (isSplitOn()) renderSplit();
      scheduleSave();
      return true;
    }

    function closeActive() {
      if (activeTab) return closeTab(activeTab.key);
      return false;
    }

    function closeOthers() {
      if (!activeTab) return false;
      var keep = activeTab;
      tabs.slice().forEach(function (t) { if (t !== keep) disposeTabModel(t); });
      tabs = [keep];
      if (splitKeyState && splitKeyState !== keep.key) splitKeyState = null;
      if (isSplitOn() && !splitKeyState) setSplitVisible(false);
      renderTabBar();
      scheduleSave();
      return true;
    }

    function closeAll() {
      // Invalidate any fetch still in flight: its render must not re-open a tab
      // after everything has been closed.
      window.srcSeq = (window.srcSeq || 0) + 1;
      pendingRenderTab = null;
      tabs.forEach(disposeTabModel);
      tabs = [];
      activeTab = null;
      splitKeyState = null;
      lastRenderedKey = null;
      setSplitVisible(false);
      closeSplitEditor();
      renderTabBar();
    }

    // ------------------------------------------------------------- split

    function isSplitOn() {
      return !!(wrapEl && wrapEl.classList.contains('cs-split-on'));
    }

    function splitThemeId() {
      var t = (state.source && state.source.theme) || 'darkplus';
      if (['darkplus', 'darkmodern', 'monokai', 'solarized'].indexOf(t) < 0) t = 'darkplus';
      return 'csgraph-' + t;
    }

    function ensureSplitEditor() {
      if (splitEditor || !splitHostEl) return;
      var ed = window.CsEditor;
      if (!ed || !ed.monaco) return;
      var create = function () {
        var monaco = ed.monaco();
        if (!monaco || !splitHostEl || splitEditor) return;
        ensureModelPatch();
        try {
          splitEditor = monaco.editor.create(splitHostEl, {
            value: '', language: 'csharp', theme: splitThemeId(),
            readOnly: true, domReadOnly: true, automaticLayout: true,
            minimap: { enabled: false }, folding: false,
            renderLineHighlight: 'line', scrollBeyondLastLine: false,
            fontSize: 13, lineHeight: 0, glyphMargin: false, lineNumbers: 'off',
            padding: { top: 6, bottom: 6 },
            scrollbar: { verticalScrollbarSize: 12, horizontalScrollbarSize: 12, useShadows: false },
            fixedOverflowWidgets: true, contextmenu: false
          });
        } catch (e) { report(e); }
        // The editor may arrive after the split document was already fetched
        // (Monaco loads lazily): paint what this split is showing.
        var t = byKey(splitKeyState);
        if (splitEditor && t && t.data) renderSplitModel(t, t.data);
      };
      if (typeof ed.state === 'function' && ed.state() === 'ready') create();
      else if (typeof ed.onReady === 'function') ed.onReady(create);
    }

    function closeSplitEditor() {
      if (!splitEditor) return;
      try { splitEditor.setModel(null); splitEditor.dispose(); } catch (e) { /* ignore */ }
      splitEditor = null;
    }

    function updateSplitHead(tab) {
      if (splitLabelEl) splitLabelEl.textContent = (tab && (tab.label || tab.key)) || '';
    }

    function renderSplitModel(tab, d) {
      ensureSplitEditor();
      if (!splitEditor) return;
      var ed = window.CsEditor;
      var monaco = ed && ed.monaco && ed.monaco();
      if (!monaco) return;
      var key = modelKeyFor(d, tab);
      pendingModelKey = key;
      var model = null;
      try { model = monaco.editor.createModel(linesText(d.lines), 'csharp'); }
      catch (e) { report(e); }
      pendingModelKey = null;
      if (!model) return;
      tab.modelKey = key;
      try { splitEditor.setModel(model); } catch (e) { report(e); }
      updateSplitHead(tab);
    }

    function renderSplit() {
      if (!isSplitOn()) return;
      var tab = byKey(splitKeyState) || firstOther(activeTab);
      if (!tab) { setSplitVisible(false); return; }
      splitKeyState = tab.key;
      updateSplitHead(tab);
      renderTabBar();
      if (tab.data) { renderSplitModel(tab, tab.data); return; }
      fetchTab(tab).then(function (d) {
        if (isSplitOn() && splitKeyState === tab.key) renderSplitModel(tab, d);
      }).catch(function (err) {
        if (splitLabelEl) splitLabelEl.textContent = 'Could not load: ' + ((err && err.message) || err);
      });
    }

    function setSplitVisible(on) {
      if (!wrapEl) return;
      wrapEl.classList.toggle('cs-split-on', !!on);
      if (on) {
        applySplitWidth();
        ensureSplitEditor();
      } else {
        splitKeyState = null;
      }
      if (window.CsPanels && window.CsPanels.positionSplitters) window.CsPanels.positionSplitters();
      if (typeof window.resize === 'function') { try { window.resize(); } catch (e) { /* ignore */ } }
      renderTabBar();
      scheduleSave();
    }

    function applySplitWidth() {
      if (wrapEl) wrapEl.style.setProperty('--cs-split-w', (splitFrac * 100).toFixed(1) + '%');
    }

    function toggleSplit(force) {
      var on = (typeof force === 'boolean') ? force : !isSplitOn();
      if (!on) { if (isSplitOn()) setSplitVisible(false); return false; }
      // A pop-out document window shows exactly one document.
      if (docWindow()) return false;
      if (tabs.length < 2) {
        if (typeof toast === 'function') toast('Open at least two documents to split the pane', 'warn');
        return false;
      }
      if (isSplitOn() && splitKeyState) { renderSplit(); return true; }
      var target = byKey(splitKeyState) || firstOther(activeTab) || tabs[0];
      splitKeyState = target ? target.key : null;
      setSplitVisible(true);
      renderSplit();
      return true;
    }

    function setSplitTab(ref) {
      var tab = (typeof ref === 'number') ? tabs[ref] : byKey(ref);
      if (!tab || tab === activeTab) return false;
      splitKeyState = tab.key;
      if (!isSplitOn()) setSplitVisible(true); else { renderSplit(); scheduleSave(); }
      return true;
    }

    // ------------------------------------------------------- divider drag
    // Same conventions as panels.js: the .pane-splitter look, the
    // `panel-resizing` cursor class and `data-panel-cursor`.

    function onDividerDown(ev) {
      if (ev.button !== 0) return;
      ev.preventDefault();
      dragging = true;
      document.documentElement.classList.add('panel-resizing');
      document.documentElement.dataset.panelCursor = 'col-resize';
      window.addEventListener('pointermove', onDividerMove);
      window.addEventListener('pointerup', endDivider);
    }

    function onDividerMove(ev) {
      if (!dragging || !wrapEl) return;
      var r = wrapEl.getBoundingClientRect();
      if (!r.width) return;
      splitFrac = clamp((ev.clientX - r.left) / r.width, 0.15, 0.85);
      applySplitWidth();
      if (window.CsPanels && window.CsPanels.positionSplitters) window.CsPanels.positionSplitters();
      if (typeof window.resize === 'function') { try { window.resize(); } catch (e) { /* ignore */ } }
    }

    function endDivider() {
      dragging = false;
      document.documentElement.classList.remove('panel-resizing');
      delete document.documentElement.dataset.panelCursor;
      window.removeEventListener('pointermove', onDividerMove);
      window.removeEventListener('pointerup', endDivider);
      scheduleSave();
    }

    // ----------------------------------------------------- wrapped globals

    function openSourceWrapper(params) {
      if (!installed || !origOpenSource) return origOpenSource.apply(null, arguments);
      var p = normalize(params);
      if (!p) return origOpenSource.apply(null, arguments);
      var key = keyFor(p);
      var existing = byKey(key);
      if (existing) {
        var changed = paramsDiffer(existing, p);
        mergeParams(existing, p);
        if (existing === activeTab) {
          pendingRenderTab = existing;
          try { origOpenSource(copyParams(existing.params)); } catch (e) { report(e); }
          renderTabBar();
        } else if (changed) {
          // The cached payload is for the old line/mode: drop it and refetch.
          existing.data = null;
          existing.snap = null;
          activateTab(existing);
        } else {
          activateTab(existing);
        }
        scheduleSave();
        return;
      }
      var tab = makeTab(key, p);
      tabs.push(tab);
      activeTab = tab;
      trimTabs();
      pendingRenderTab = tab;
      renderTabBar();
      try { origOpenSource(copyParams(p)); } catch (e) { report(e); }
      renderTabBar();
      scheduleSave();
    }

    function paramsFromPayload(d) {
      if (d && d.doc) {
        var node = d.node;
        var id = (typeof node === 'string') ? node : (node && (node.id || node.full));
        if (id) return { node: id, mode: d.mode || 'doc' };
      }
      var f = d && (d.path || d.file);
      if (f) return { file: f, line: d.line || 1, mode: d.mode || 'raw' };
      return null;
    }

    function mergeDataParams(p, d) {
      var out = p || {};
      if (d && d.doc) {
        if (!out.node) {
          var node = d.node;
          var id = (typeof node === 'string') ? node : (node && (node.id || node.full));
          if (id) out.node = id;
        }
      } else if (d && (d.path || d.file)) {
        if (!out.file) out.file = d.path || d.file;
        if (!out.line && d.line) out.line = d.line;
      }
      if (d && d.mode) out.mode = d.mode;
      return out;
    }

    function labelFor(d, params) {
      if (d && d.doc) {
        var node = d.node;
        var full = (node && (node.full || node.id)) || (typeof node === 'string' ? node : '');
        if (full) return shortNode(full);
      }
      if (d && (d.path || d.file)) {
        return basename(d.path || d.file) + (d.doc ? '' : (d.line ? ':' + d.line : ''));
      }
      return defaultLabel(params || {});
    }

    function syncSplitTheme() {
      if (!splitEditor) return;
      try { splitEditor.updateOptions({ theme: splitThemeId() }); } catch (e) { /* ignore */ }
    }

    function renderSourceWrapper(d) {
      if (!installed || !origRender) return origRender.apply(null, arguments);
      var tab = pendingRenderTab || activeTab;
      pendingRenderTab = null;
      if (!tab) {
        var p = paramsFromPayload(d);
        tab = makeTab(p ? keyFor(p) : ('doc:render:' + (++implicitSeq)), p || {});
        tabs.push(tab);
        activeTab = tab;
      }
      var key = modelKeyFor(d, tab);
      ensureModelPatch();                 // makes editor.js's createModel reuse the cache
      pendingModelKey = key;
      var r;
      try { r = origRender.apply(null, arguments); }
      finally { pendingModelKey = null; }
      tab.data = d;
      tab.modelKey = key;
      lastRenderedKey = tab.key;
      tab.params = mergeDataParams(tab.params, d);
      tab.snap = snapshotSource();
      tab.label = labelFor(d, tab.params);
      syncSplitTheme();
      if (isSplitOn() && splitKeyState === tab.key) renderSplitModel(tab, d);
      renderTabBar();
      return r;
    }

    function closeSourceWrapper() {
      if (!installed || !origCloseSource) return origCloseSource.apply(null, arguments);
      // Another window asked for the document to close: match it, don't bounce
      // a different open document back at it.
      if (window.__csRemoteClose) { closeAll(); return origCloseSource.apply(null, arguments); }
      if (!tabs.length) return origCloseSource.apply(null, arguments);
      return closeTab(activeTab.key) ? undefined : origCloseSource.apply(null, arguments);
    }

    // ------------------------------------------------------------- session

    function sanParams(p) {
      if (!p || typeof p !== 'object') return null;
      var out;
      if (typeof p.node === 'string' && p.node) {
        out = { node: p.node.slice(0, 400) };
      } else if (typeof p.file === 'string' && p.file) {
        out = { file: p.file.slice(0, 1024), line: numOr(p.line, 1) };
        if (typeof p.context === 'number' && isFinite(p.context)) out.context = Math.max(1, Math.floor(p.context));
        if (typeof p.to === 'number' && isFinite(p.to)) out.to = Math.max(1, Math.floor(p.to));
      } else return null;
      if (p.mode === 'raw' || p.mode === 'doc' || p.mode === 'effective') out.mode = p.mode;
      return out;
    }

    function session() {
      return {
        v: 1,
        tabs: tabs.slice(0, MAX_TABS).map(function (t) {
          return {
            key: t.key,
            params: copyParams(t.params),
            mode: (t.snap && t.snap.srcMode) || t.params.mode || '',
            label: t.label || ''
          };
        }),
        active: activeTab ? activeTab.key : null,
        split: {
          on: isSplitOn(),
          key: splitKeyState || null,
          w: Math.round(splitFrac * 1000) / 1000
        }
      };
    }

    /** Rebuild tabs from a persisted session; every bad row is skipped. */
    function restoreSession(sess) {
      if (!installed || !sess || typeof sess !== 'object') return false;
      var list = Array.isArray(sess.tabs) ? sess.tabs : null;
      if (!list || !list.length) return false;

      var restored = [];
      for (var i = 0; i < list.length && restored.length < MAX_TABS; i++) {
        var row = list[i];
        if (!row || typeof row !== 'object') continue;
        var params = sanParams(row.params || row);
        if (!params) continue;
        var key = keyFor(params);
        if (byKeyIn(restored, key)) continue;
        var tab = makeTab(key, params);
        if (typeof row.mode === 'string' && (row.mode === 'raw' || row.mode === 'doc' || row.mode === 'effective')) {
          tab.params.mode = row.mode;
        }
        if (typeof row.label === 'string') tab.label = row.label.slice(0, 120);
        restored.push(tab);
      }
      if (!restored.length) return false;

      closeAll();
      tabs = restored;
      var want = null;
      if (typeof sess.active === 'string') want = byKeyIn(tabs, sess.active);
      activeTab = want || tabs[0];

      var sp = sess.split;
      var wantSplit = !!(sp && sp.on) ? byKeyIn(tabs, sp && sp.key) : null;
      if (!wantSplit) wantSplit = firstOther(activeTab);
      if (sp && typeof sp.w === 'number' && isFinite(sp.w)) splitFrac = clamp(sp.w, 0.15, 0.85);
      splitKeyState = wantSplit ? wantSplit.key : null;

      // Render the active document first, then the split one beside it.
      activateTab(activeTab);
      if (splitKeyState) { setSplitVisible(true); renderSplit(); }
      return true;
    }

    function byKeyIn(list, key) {
      for (var i = 0; i < list.length; i++) if (list[i].key === key) return list[i];
      return null;
    }

    // ------------------------------------------------------------- install

    function install() {
      if (installed) return true;
      var pane = document.getElementById('sourcePane');
      var body = document.getElementById('sourceBody');
      if (!pane || !body || !body.parentNode) return false;

      origOpenSource = window.openSource;
      origRender = window.renderSource;
      origCloseSource = window.closeSource;
      if (typeof origOpenSource !== 'function' || typeof origRender !== 'function') return false;

      barEl = document.createElement('div');
      barEl.id = 'cs-tabs';
      barEl.className = 'cs-tabs hidden';
      barEl.setAttribute('role', 'tablist');
      barEl.setAttribute('aria-label', 'Open documents');
      pane.insertBefore(barEl, pane.firstChild);

      wrapEl = document.createElement('div');
      wrapEl.id = 'csSplitWrap';
      body.parentNode.insertBefore(wrapEl, body);

      dividerEl = document.createElement('div');
      dividerEl.id = 'csSplitDivider';
      dividerEl.className = 'pane-splitter pane-splitter-v';
      dividerEl.setAttribute('role', 'separator');
      dividerEl.setAttribute('aria-orientation', 'vertical');
      dividerEl.title = 'Drag to resize the split';
      dividerEl.addEventListener('pointerdown', onDividerDown);

      splitBodyEl = document.createElement('div');
      splitBodyEl.id = 'csSplitBody';
      var shead = document.createElement('div');
      shead.className = 'cs-split-head';
      splitLabelEl = document.createElement('span');
      splitLabelEl.className = 'cs-split-label';
      shead.appendChild(splitLabelEl);
      var spacer = document.createElement('span');
      spacer.className = 'cs-split-spacer';
      shead.appendChild(spacer);
      var collapse = document.createElement('button');
      collapse.type = 'button';
      collapse.id = 'csSplitCollapse';
      collapse.className = 'btn btn-ghost';
      collapse.title = 'Collapse the split';
      collapse.textContent = '✕';
      collapse.addEventListener('click', function () { setSplitVisible(false); });
      shead.appendChild(collapse);
      splitHostEl = document.createElement('div');
      splitHostEl.id = 'csSplitHost';
      splitBodyEl.appendChild(shead);
      splitBodyEl.appendChild(splitHostEl);

      wrapEl.appendChild(body);
      wrapEl.appendChild(dividerEl);
      wrapEl.appendChild(splitBodyEl);

      barEl.addEventListener('click', onBarClick);
      barEl.addEventListener('auxclick', onBarAux);

      window.openSource = openSourceWrapper;
      window.renderSource = renderSourceWrapper;
      window.closeSource = closeSourceWrapper;

      installed = true;
      renderTabBar();
      return true;
    }

    // ------------------------------------------------------------- public API

    return {
      install: function () {
        try { return install(); } catch (e) { report(e); return false; }
      },
      isInstalled: function () { return installed; },
      // state
      tabs: function () {
        return tabs.map(function (t) {
          return {
            key: t.key, label: t.label, active: t === activeTab,
            split: t.key === splitKeyState, loaded: !!t.data,
            node: (t.params && t.params.node) || null,
            file: (t.params && t.params.file) || null,
            mode: t.params && t.params.mode
          };
        });
      },
      count: function () { return tabs.length; },
      activeKey: function () { return activeTab ? activeTab.key : null; },
      /** The tab whose renderSource() most recently completed (test hook). */
      renderedKey: function () { return lastRenderedKey; },
      // commands
      activate: function (ref) {
        var tab = (typeof ref === 'number') ? tabs[ref] : byKey(ref);
        if (tab) { activateTab(tab); return true; }
        return false;
      },
      close: closeTab,
      closeActive: closeActive,
      closeOthers: closeOthers,
      closeAll: closeAll,
      cycle: cycle,
      // split
      isSplit: isSplitOn,
      splitKey: function () { return splitKeyState; },
      splitLineCount: function () {
        if (!splitEditor || !splitEditor.getModel) return 0;
        var m = splitEditor.getModel();
        return m ? m.getLineCount() : 0;
      },
      toggleSplit: toggleSplit,
      setSplit: setSplitTab,
      splitFrac: function () { return splitFrac; },
      // session
      session: session,
      restoreSession: function (sess) {
        try { return restoreSession(sess); } catch (e) { report(e); return false; }
      },
      saveNow: function () { try { if (typeof saveViewerState === 'function') saveViewerState(); } catch (e) { /* ignore */ } },
      // test hooks
      stats: function () {
        return {
          modelsCreated: modelsCreated,
          modelsReused: modelsReused,
          cacheSize: Object.keys(modelCache).length,
          models: Object.keys(modelCache),
          disposed: Object.keys(modelCache).map(function (k) {
            var mm = modelCache[k];
            return k + (mm && typeof mm.isDisposed === 'function' && mm.isDisposed() ? '=DISPOSED' : '=live');
          }),
          lastEvent: lastModelEvent,
          activeModelKey: activeTab ? activeTab.modelKey : null
        };
      }
    };
  })();

  window.CsTabs.install();

