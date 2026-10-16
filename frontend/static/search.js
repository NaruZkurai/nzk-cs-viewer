/* csgraph - symbol search (suggestions) and Find definitions / implementations / usages.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== search

  function showSuggestions() {
    suggestionsEl.classList.remove('hidden');
    searchInputEl.setAttribute('aria-expanded', 'true');
  }
  function hideSuggestions() {
    suggestionsEl.classList.add('hidden');
    suggestionsEl.innerHTML = '';
    searchInputEl.setAttribute('aria-expanded', 'false');
    state.searchIndex = -1;
  }

  function renderSuggestions(message) {
    if (!message && !state.searchResults.length) message = 'no matches';
    if (message) {
      suggestionsEl.innerHTML = '<div class="sug-empty">' + esc(message) + '</div>';
      showSuggestions();
      return;
    }
    suggestionsEl.innerHTML = state.searchResults.map(function (n, i) {
      return '<button class="sug' + (i === state.searchIndex ? ' active' : '') + '" type="button" role="option" data-id="' + esc(n.id) + '" data-i="' + i + '" title="Open this symbol in the graph">' +
        '<span class="dot" style="background:' + kindColor(n.kind) + ';width:7px;height:7px;border-radius:50%;flex:none"></span>' +
        '<span class="sug-kind">' + esc(n.kind || '') + '</span>' +
        '<span class="sug-label">' + esc(n.label || n.name || n.id) + '</span>' +
        '<span class="sug-full">' + esc(n.ns || '') + '</span>' +
      '</button>';
    }).join('');
    showSuggestions();
  }

  function runSearch(q) {
    var seq = ++searchSeq;
    apiGet('/api/search', { q: q, limit: 30 })
      .then(function (data) {
        if (seq !== searchSeq) return;
        state.searchResults = data.results || [];
        state.searchIndex = -1;
        renderSuggestions(null);
      })
      .catch(function (err) {
        if (seq !== searchSeq) return;
        state.searchResults = [];
        renderSuggestions('search unavailable: ' + err.message);
      });
  }

  function onSearchInput() {
    if (state.mode === 'find') { onFindInput(); return; }
    var q = searchInputEl.value.trim();
    window.clearTimeout(searchTimer);
    if (!q) { searchSeq++; hideSuggestions(); return; }
    searchTimer = window.setTimeout(function () { runSearch(q); }, 140);
  }

  function moveSuggestion(delta) {
    var rs = state.searchResults;
    if (!rs.length) return;
    var idx = state.searchIndex + delta;
    if (idx < -1) idx = rs.length - 1;
    if (idx >= rs.length) idx = -1;
    state.searchIndex = idx;
    renderSuggestions(null);
    var el = suggestionsEl.querySelector('.sug.active');
    if (el && el.scrollIntoView) el.scrollIntoView({ block: 'nearest' });
  }

  function commitSearch() {
    var rs = state.searchResults;
    var i = state.searchIndex >= 0 ? state.searchIndex : 0;
    if (!rs.length || !rs[i]) { hideSuggestions(); return; }
    var node = rs[i];
    hideSuggestions();
    searchInputEl.blur();
    centerNode(node.id);
  }

  // ================================================================== find (code lookup)

  function setMode(mode, focusInput) {
    if (mode !== 'graph' && mode !== 'find') return;
    state.mode = mode;
    var find = mode === 'find';
    modeFindEl.classList.toggle('active', find);
    modeGraphEl.classList.toggle('active', !find);
    modeFindEl.setAttribute('aria-selected', String(find));
    modeGraphEl.setAttribute('aria-selected', String(!find));
    searchInputEl.placeholder = find ? 'Find symbol or code…  ( f )' : 'Search symbols…  ( / )';
    if (!find) hideSuggestions();
    findPanelEl.classList.toggle('hidden', !find);
    if (find && !state.find.data && !state.find.loading) renderFind();
    if (find) nodeDrawerEl.classList.add('hidden');
    else if (state.detail) renderNodeDrawer();
    if (focusInput) { searchInputEl.focus(); searchInputEl.select(); }
    if (window.CSSync) window.CSSync.push();
  }

  /** Append `text` to a container, wrapping every occurrence of `needle` in <mark>. */
  function appendHighlighted(container, text, needle, trim) {
    var t = String(text === null || text === undefined ? '' : text);
    t = trim ? t.trim() : t.replace(/\r$/, '');
    if (!needle) { container.textContent = t; return; }
    var from = 0, idx;
    while ((idx = t.indexOf(needle, from)) >= 0) {
      if (idx > from) container.appendChild(document.createTextNode(t.slice(from, idx)));
      var mark = document.createElement('mark');
      mark.textContent = t.slice(idx, idx + needle.length);
      container.appendChild(mark);
      from = idx + needle.length;
    }
    if (from < t.length) container.appendChild(document.createTextNode(t.slice(from)));
  }

  function resetFindWindow() {
    state.find.shown = { definitions: FIND_PAGE, implementations: FIND_PAGE, usages: FIND_PAGE };
    state.find.sel = -1;
  }

  function onFindInput() {
    window.clearTimeout(findTimer);
    var q = searchInputEl.value.trim();
    if (!q) {
      findSeq++;
      state.find.query = '';
      state.find.data = null;
      state.find.error = '';
      state.find.loading = false;
      state.find.rows = [];
      state.find.sel = -1;
      renderFind();
      return;
    }
    findTimer = window.setTimeout(function () { runFind(q); }, FIND_DEBOUNCE);
  }

  function runFind(q) {
    q = (q || '').trim();
    window.clearTimeout(findTimer);
    if (!q) { onFindInput(); return; }
    var seq = ++findSeq;
    state.find.query = q;
    state.find.loading = true;
    state.find.error = '';
    renderFind();
    if (window.CSSync) window.CSSync.push();
    apiGet('/api/find', { q: q, limit: FIND_LIMIT })
      .then(function (data) {
        if (seq !== findSeq) return;
        state.find.data = data;
        state.find.simple = data.simple || '';
        state.find.loading = false;
        resetFindWindow();
        renderFind();
      })
      .catch(function (err) {
        if (seq !== findSeq) return;
        state.find.loading = false;
        state.find.data = null;
        state.find.error = err.message;
        state.find.rows = [];
        state.find.sel = -1;
        renderFind();
        toast('Find failed: ' + err.message, 'error');
      });
  }

  function findRowKey(row) {
    if (row.node && row.node.id) return row.node.id;
    if (row.usage) return row.usage.file + ':' + row.usage.line;
    return '';
  }

  function buildDefRow(node) {
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'frow';
    btn.title = 'Open ' + (node.full || node.label || node.id || 'this result') + ' in the graph';

    var top = document.createElement('span');
    top.className = 'frow-top';
    var chip = document.createElement('span');
    chip.className = 'fkind';
    chip.textContent = node.kind || 'node';
    var name = document.createElement('span');
    name.className = 'fname';
    name.textContent = node.full || node.label || node.id || '';
    name.title = name.textContent;
    top.appendChild(chip);
    top.appendChild(name);
    btn.appendChild(top);

    var sub = document.createElement('span');
    sub.className = 'frow-sub';
    var bits = [];
    if (node.label && node.label !== name.textContent) bits.push(node.label);
    if (node.file) bits.push(node.file + (node.line ? ':' + node.line : ''));
    sub.textContent = bits.join('  ·  ');
    if (sub.textContent) btn.appendChild(sub);
    return btn;
  }

  function buildUsageRow(u) {
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'frow';
    btn.title = 'Jump to ' + (u.file || 'this usage') + ':' + (u.line || 0) + ' in the source pane';

    var top = document.createElement('span');
    top.className = 'frow-top';
    var chip = document.createElement('span');
    chip.className = 'fkind';
    chip.textContent = (u.node && u.node.kind) ? u.node.kind : 'usage';
    var loc = document.createElement('span');
    loc.className = 'fname';
    loc.textContent = (u.file || '?') + ':' + (u.line || 0);
    loc.title = loc.textContent;
    var tag = document.createElement('span');
    tag.className = 'fmatch ' + (u.match === 'exact' ? 'fq' : 'short');
    tag.textContent = u.match === 'exact' ? 'fq' : 'short';
    tag.title = u.match === 'exact'
      ? 'exact literal match of the query'
      : 'match on the bare (short) name';
    top.appendChild(chip);
    top.appendChild(loc);
    top.appendChild(tag);
    btn.appendChild(top);

    var src = document.createElement('span');
    src.className = 'fsrc';
    var needle = u.match === 'exact'
      ? state.find.query
      : (state.find.simple || state.find.query);
    appendHighlighted(src, u.text, needle, true);
    btn.appendChild(src);

    if (u.node && (u.node.label || u.node.name)) {
      var sub = document.createElement('span');
      sub.className = 'frow-sub';
      sub.textContent = 'in ' + (u.node.label || u.node.name);
      btn.appendChild(sub);
    }
    return btn;
  }

  function buildGroup(kind, title, items, builder) {
    var cssKind = { definitions: 'defs', implementations: 'impls', usages: 'uses' }[kind] || kind;
    var group = document.createElement('div');
    group.className = 'fgroup fgroup-' + cssKind;

    var head = document.createElement('div');
    head.className = 'fgroup-head';
    head.appendChild(document.createTextNode(title));
    var count = document.createElement('span');
    count.className = 'fcount';
    count.textContent = fmtNum(items.length);
    head.appendChild(count);
    group.appendChild(head);

    var shown = Math.min(items.length, state.find.shown[kind] || FIND_PAGE);
    for (var i = 0; i < shown; i++) {
      var row = builder(items[i]);
      var desc = { el: row, kind: kind, node: items[i].node !== undefined ? items[i].node : items[i], usage: kind === 'usages' ? items[i] : null };
      row.addEventListener('click', function (d) {
        return function () { activateFindRow(d); };
      }(desc));
      row.addEventListener('mousemove', function (d) {
        return function () {
          var idx = state.find.rows.indexOf(d);
          if (idx >= 0 && idx !== state.find.sel) selectFindRow(idx, false);
        };
      }(desc));
      group.appendChild(row);
      state.find.rows.push(desc);
    }
    if (items.length > shown) {
      var rest = items.length - shown;
      var more = document.createElement('button');
      more.type = 'button';
      more.className = 'fshowmore';
      more.title = 'Show more find results';
      more.textContent = 'show ' + Math.min(FIND_MORE, rest) + ' more of ' + fmtNum(rest) + ' remaining…';
      more.addEventListener('click', function () {
        state.find.shown[kind] = shown + FIND_MORE;
        var keep = state.find.sel >= 0 ? findRowKey(state.find.rows[state.find.sel]) : '';
        renderFind();
        if (keep) {
          for (var j = 0; j < state.find.rows.length; j++) {
            if (findRowKey(state.find.rows[j]) === keep) { selectFindRow(j, false); break; }
          }
        }
      });
      group.appendChild(more);
    }
    return group;
  }

  function renderFind() {
    var f = state.find;
    var d = f.data;
    var defs = (d && d.definitions) || [];
    var impls = (d && d.implementations) || [];
    var uses = (d && d.usages) || [];

    findQueryEl.textContent = f.query ? f.query : 'Find';
    findQueryEl.title = f.query || '';
    if (f.loading) findMetaEl.textContent = 'searching…';
    else if (d) findMetaEl.textContent = fmtNum(defs.length + impls.length + uses.length) + ' hits';
    else findMetaEl.textContent = '';

    findNoticeEl.classList.toggle('hidden', !(d && d.truncated));
    if (d && d.truncated) {
      findNoticeEl.textContent = 'Results truncated by the server (limit ' + fmtNum(FIND_LIMIT) +
        '). Narrow the query for complete results.';
    }

    findResultsEl.innerHTML = '';
    f.rows = [];
    f.sel = -1;

    if (f.loading && !d) {
      findResultsEl.appendChild(elWithClass('div', 'find-empty', 'Searching…'));
      return;
    }
    if (f.error) {
      findResultsEl.appendChild(elWithClass('div', 'find-empty', 'Find failed: ' + f.error));
      return;
    }
    if (!f.query) {
      findResultsEl.appendChild(elWithClass('div', 'find-empty',
        'Type a symbol or a literal line of code and press Enter.\n' +
        'Definitions, implementations and every usage are listed here; click a row to open its source.'));
      return;
    }
    if (!d) return;
    if (!defs.length && !impls.length && !uses.length) {
      findResultsEl.appendChild(elWithClass('div', 'find-empty', 'No definitions, implementations or usages.'));
      return;
    }

    var frag = document.createDocumentFragment();
    if (defs.length) frag.appendChild(buildGroup('definitions', 'Definitions', defs, buildDefRow));
    if (impls.length) frag.appendChild(buildGroup('implementations', 'Implementations', impls, buildDefRow));
    if (uses.length) frag.appendChild(buildGroup('usages', 'Usages', uses, buildUsageRow));
    findResultsEl.appendChild(frag);

    selectFindRow(0, false);
  }

  function elWithClass(tag, cls, text) {
    var el = document.createElement(tag);
    el.className = cls;
    el.textContent = text;
    el.style.whiteSpace = 'pre-wrap';
    return el;
  }

  function selectFindRow(idx, scroll) {
    var rows = state.find.rows;
    if (!rows.length) { state.find.sel = -1; return; }
    if (idx < 0) idx = 0;
    if (idx >= rows.length) idx = rows.length - 1;
    for (var i = 0; i < rows.length; i++) rows[i].el.classList.toggle('active', i === idx);
    state.find.sel = idx;
    if (scroll !== false && rows[idx].el.scrollIntoView) {
      rows[idx].el.scrollIntoView({ block: 'nearest' });
    }
  }

  function moveFindSelection(delta) {
    var rows = state.find.rows;
    if (!rows.length) return;
    var idx = state.find.sel < 0 ? (delta > 0 ? 0 : rows.length - 1) : state.find.sel + delta;
    if (idx < 0) idx = rows.length - 1;
    if (idx >= rows.length) idx = 0;
    selectFindRow(idx, true);
  }

  function activateFindSelection() {
    var rows = state.find.rows;
    if (state.find.sel < 0 || !rows[state.find.sel]) return;
    activateFindRow(rows[state.find.sel]);
  }

  function activateFindRow(row) {
    if (!row) return;
    if (row.kind === 'usages' && row.usage) openUsage(row.usage);
    else if (row.node && row.node.id) openFindNode(row.node.id);
  }

  /** Definition/implementation hit: behave exactly like a search-suggestion selection. */
  function openFindNode(id) {
    if (!id) return;
    centerNode(id);
    openSource({ node: id });
  }

  /** Usage hit: open the file window and remember the enclosing member for the chip. */
  function applyUsage(u, keepHits) {
    var st = state.source;
    st.usageNode = u.node || null;
    st.match = (u.match === 'exact') ? state.find.query : (state.find.simple || state.find.query);
    st.matchKind = u.match || '';
    if (!keepHits) {
      var all = (state.find.data && state.find.data.usages) || [];
      st.hits = all.filter(function (x) { return x.file === u.file; })
                   .sort(function (a, b) { return a.line - b.line; });
      st.hitIndex = 0;
      for (var i = 0; i < st.hits.length; i++) {
        if (st.hits[i].line === u.line && st.hits[i].col === u.col) { st.hitIndex = i; break; }
      }
    }
    openSource({ file: u.file, line: u.line, context: SOURCE_CONTEXT });
  }

  function openUsage(u) { applyUsage(u, false); }

