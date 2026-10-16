/* csgraph - kind/edge facets, public definitions, string literals and the resolve chain.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== projects (.sln/.csproj)

  /* The engine's additive `/api/projects` is the only source of project data.
   * `files=1` returns each project's source set, which is enough to map a
   * node's `file` (already in the node payload) to the project(s) that own it:
   * that map drives the project filter below and the hover attribution. No
   * existing node field is added or changed. */

  var csProjects = {
    loaded: false, promise: null, lists: [], solutions: [],
    byId: {}, fileMap: {}
  };
  var projectFilterBlock = null;
  var projectFiltersEl = null;

  function loadProjects() {
    if (csProjects.loaded) return Promise.resolve(csProjects);
    if (csProjects.promise) return csProjects.promise;
    csProjects.promise = apiGet('/api/projects', { files: 1 }).then(function (d) {
      csProjects.lists = (d && d.projects) || [];
      csProjects.solutions = (d && d.solutions) || [];
      csProjects.byId = {};
      csProjects.fileMap = {};
      csProjects.lists.forEach(function (p) {
        csProjects.byId[p.id] = p;
        (p.files || []).forEach(function (f) {
          if (!csProjects.fileMap[f]) csProjects.fileMap[f] = [];
          if (csProjects.fileMap[f].indexOf(p.id) < 0) csProjects.fileMap[f].push(p.id);
        });
      });
      csProjects.loaded = true;
      buildProjectFilter();
      return csProjects;
    }).catch(function () {
      // No project model is not an error: the filter simply stays empty.
      csProjects.loaded = true;
      buildProjectFilter();
      return csProjects;
    });
    return csProjects.promise;
  }

  /** Project ids owning the node's file, or [] when none is known. */
  function projectIdsFor(node) {
    if (!node || !node.file) return [];
    return csProjects.fileMap[node.file] || [];
  }

  function projectNameOf(id) {
    var p = csProjects.byId[id];
    return (p && p.name) || id;
  }

  function projectFilterActive() {
    return !!(state.filters.projects && state.filters.projects.size);
  }

  function ensureProjectKeys() {
    if (!state.filters.projects) state.filters.projects = new Set();
    return state.filters.projects;
  }

  /** Build (once) and repaint the project chip row inside the Filters section. */
  function buildProjectFilter() {
    ensureProjectKeys();
    var ns = (typeof nsSelectEl !== 'undefined' && nsSelectEl) || document.getElementById('nsSelect');
    if (!projectFilterBlock) {
      var anchor = ns;
      if (anchor && anchor.previousElementSibling &&
          anchor.previousElementSibling.classList.contains('filter-label')) {
        anchor = anchor.previousElementSibling;
      }
      if (!anchor || !anchor.parentNode) return;
      var block = document.createElement('div');
      block.id = 'projectFilterBlock';

      var label = document.createElement('div');
      label.className = 'filter-label';
      label.appendChild(document.createTextNode('Projects'));
      var actions = document.createElement('span');
      actions.className = 'filter-actions';
      var allBtn = document.createElement('button');
      allBtn.type = 'button';
      allBtn.className = 'linkbtn';
      allBtn.textContent = 'all';
      allBtn.title = 'Show every project';
      allBtn.addEventListener('click', function () { setAllProjects(true); });
      var noneBtn = document.createElement('button');
      noneBtn.type = 'button';
      noneBtn.className = 'linkbtn';
      noneBtn.textContent = 'none';
      noneBtn.title = 'Filter out every project';
      noneBtn.addEventListener('click', function () { setAllProjects(false); });
      actions.appendChild(allBtn);
      actions.appendChild(noneBtn);
      label.appendChild(actions);

      var list = document.createElement('div');
      list.className = 'chips';
      list.id = 'projectFilters';

      block.appendChild(label);
      block.appendChild(list);
      anchor.parentNode.insertBefore(block, anchor);
      // Assigned only after the insert succeeds, so a missing anchor cannot
      // leave `projectFilterBlock` set with `projectFiltersEl` still null.
      projectFilterBlock = block;
      projectFiltersEl = list;
    }

    projectFiltersEl.innerHTML = '';
    if (!csProjects.lists.length) {
      var empty = document.createElement('div');
      empty.className = 'list-empty';
      empty.textContent = state.indexed ? 'no .sln/.csproj found' : 'not indexed';
      projectFiltersEl.appendChild(empty);
      return;
    }
    var sel = ensureProjectKeys();
    csProjects.lists.forEach(function (p) {
      var el = document.createElement('span');
      el.className = 'edgekey' + (sel.has(p.id) ? '' : ' off');
      el.setAttribute('data-project', p.id);
      var bars = document.createElement('span');
      bars.className = 'bars';
      bars.style.borderTopColor = '#4ec9b0';
      bars.style.borderTopWidth = '2px';
      el.appendChild(bars);
      var bits = [p.name];
      if (p.fileCount) bits.push(' (' + p.fileCount + ')');
      el.appendChild(document.createTextNode(bits.join('')));
      var tip = [p.path];
      if (p.targets && p.targets.length) tip.push(p.targets.join(';'));
      if (p.defines && p.defines.length) tip.push('defines: ' + p.defines.join(';'));
      el.title = tip.join(' · ');
      el.addEventListener('click', function () {
        if (sel.has(p.id)) sel.delete(p.id);
        else sel.add(p.id);
        el.classList.toggle('off', !sel.has(p.id));
        applyProjectFilterChange();
      });
      projectFiltersEl.appendChild(el);
    });
  }

  function setAllProjects(on) {
    var sel = ensureProjectKeys();
    if (on) csProjects.lists.forEach(function (p) { sel.add(p.id); });
    else sel.clear();
    buildProjectFilter();
    applyProjectFilterChange();
  }

  /** Repaint after a project toggle; draw() calls computeView every frame. */
  function applyProjectFilterChange() {
    scheduleViewerSave();
    if (typeof computeView === 'function') computeView();
    if (typeof updateCanvasNotice === 'function') updateCanvasNotice();
  }

  /**
   * Project filtering is a *view* filter, like the namespace one: the graph
   * model keeps every node and the wrapper below drops what the selected
   * project(s) do not own. Namespace containers stay so the hierarchy reads.
   */
  function nodeInProjects(n) {
    if (!projectFilterActive()) return true;
    var ids = projectIdsFor(n);
    if (ids.length) {
      var sel = state.filters.projects;
      for (var i = 0; i < ids.length; i++) { if (sel.has(ids[i])) return true; }
      return false;
    }
    return n.kind === 'namespace';
  }

  function csProjectView(view) {
    if (!projectFilterActive() || !view) return view;
    var sel = state.filters.projects;
    var nodes = view.nodes.filter(nodeInProjects);
    var keep = new Set(nodes.map(function (n) { return n.id; }));
    var edges = view.edges.filter(function (e) {
      return keep.has(e.source) && keep.has(e.target);
    });
    var deg = new Map();
    nodes.forEach(function (n) { deg.set(n.id, 0); });
    edges.forEach(function (e) {
      deg.set(e.source, (deg.get(e.source) || 0) + 1);
      deg.set(e.target, (deg.get(e.target) || 0) + 1);
    });
    state.view = { nodes: nodes, edges: edges, deg: deg };
    return state.view;
  }

  // graph.js is loaded before this file, so `computeView` exists here; wrap the
  // global so every existing caller (draw, notices) gets the filtered view.
  var csBaseComputeView = (typeof computeView === 'function') ? computeView : null;
  if (csBaseComputeView) {
    computeView = function () {
      return csProjectView(csBaseComputeView.apply(this, arguments));
    };
  }

  /** Shared with hover.js (loaded earlier, but only called at hover time). */
  window.CsProjects = {
    load: loadProjects,
    name: projectNameOf,
    idsFor: projectIdsFor,
    lists: function () { return csProjects.lists; },
    ready: function () { return csProjects.loaded; }
  };

  // ================================================================== filters (facets)

  /** Install /api/kinds: dynamic kind rows with counts, never a hard-coded list. */
  function applyKinds(k) {
    state.kinds.list = k.kinds || [];
    state.kinds.counts = k.counts || {};
    state.kinds.namespaces = k.namespaces || [];
    state.kinds.types = k.types || [];
    state.kinds.public = k.public || [];
    state.kinds.loaded = true;
    if (!state.filters.kinds.size && !state.kindsSeen) {
      state.filters.kinds = new Set(state.kinds.list.map(function (x) { return x.kind; }));
    } else {
      // keep existing toggles, adopt new kinds as visible
      state.kinds.list.forEach(function (x) {
        if (!state.kindsSeen || !state.kindsSeen.has(x.kind)) state.filters.kinds.add(x.kind);
      });
    }
    state.kindsSeen = new Set(state.kinds.list.map(function (x) { return x.kind; }));
    buildFilters();
    buildNamespaceFilter();
    reloadProjects();
  }

  /** Re-read the project model for the index that just loaded. */
  function reloadProjects() {
    csProjects.loaded = false;
    csProjects.promise = null;
    return loadProjects();
  }

  /** Rebuild the "one namespace" select from the live /api/kinds facet. */
  function buildNamespaceFilter() {
    if (!nsSelectEl) return;
    var list = state.kinds.namespaces || [];
    var keep = state.filters.namespace || '';
    nsSelectEl.innerHTML = '';
    var all = document.createElement('option');
    all.value = '';
    all.textContent = list.length ? 'All namespaces (' + list.length + ')' : 'All namespaces';
    nsSelectEl.appendChild(all);
    list.forEach(function (n) {
      var full = n.full || '';
      if (!full) return;
      var opt = document.createElement('option');
      opt.value = full;
      opt.textContent = full + (n.count ? '  (' + n.count + ')' : '');
      nsSelectEl.appendChild(opt);
    });
    // a remembered namespace that this index does not have falls back to everything
    var known = list.some(function (n) { return n.full === keep; });
    if (keep && !known) { state.filters.namespace = ''; keep = ''; }
    nsSelectEl.value = keep;
    if (nsDeepEl) nsDeepEl.checked = state.filters.namespaceDeep !== false;
  }

  /**
   * Show only one namespace ('' = every namespace). This is a *view* filter: the
   * graph model keeps every node, and computeView drops anything outside the
   * chosen namespace subtree, so clearing it restores the whole picture with no
   * reload. Picking a namespace also pulls it into the current graph (and expands
   * it once) when it is not there yet, then frames it.
   */
  function setNamespaceFilter(ns, opts) {
    opts = opts || {};
    ns = ns || '';
    state.filters.namespace = ns;
    if (nsSelectEl && nsSelectEl.value !== ns) nsSelectEl.value = ns;
    var expanded = false;
    if (ns && state.nodeById) {
      var id = '#ns:' + ns;
      if (!state.nodeById.has(id)) {
        // not in this subgraph: pull it in first, then filter when it lands
        if (!opts.silent && typeof goToNode === 'function') { goToNode(id); expanded = true; }
      } else if (!opts.silent && !expandedSet(id).has('contains:out')) {
        expandNode(id);
        expanded = true;
      }
      if (state.pos.has(id)) followNode(id, null);
    }
    if (!opts.silent) {
      state.autoFit = true;
      if (typeof computeView === 'function') computeView();
      if (typeof updateCanvasNotice === 'function') updateCanvasNotice();
      scheduleViewerSave();
      if (expanded && typeof toast === 'function') toast('Isolated ' + ns, 'ok');
    }
  }

  /**
   * "Include sub-namespaces": on (default) the chosen namespace keeps its whole
   * subtree (ns.subns.subsubns); off keeps only what is declared directly in
   * that one namespace. A view filter, like the namespace choice itself, so it
   * costs one recompute and no reload.
   */
  function setNamespaceDeep(on) {
    state.filters.namespaceDeep = !!on;
    if (nsDeepEl) nsDeepEl.checked = !!on;
    scheduleViewerSave();
    if (typeof computeView === 'function') computeView();
    if (typeof updateCanvasNotice === 'function') updateCanvasNotice();
  }

  function buildFilters() {
    kindFiltersEl.innerHTML = '';
    var frag = document.createDocumentFragment();
    state.kinds.list.forEach(function (item) {
      var lab = document.createElement('label');
      lab.className = 'facet' + (state.filters.kinds.has(item.kind) ? ' on' : ' off');
      lab.setAttribute('data-kind', item.kind);
      var sw = document.createElement('span');
      sw.className = 'swatch';
      sw.style.background = kindColor(item.kind);
      var nm = document.createElement('span');
      nm.className = 'facet-name';
      nm.textContent = item.kind;
      var ct = document.createElement('span');
      ct.className = 'facet-count';
      ct.textContent = fmtNum(item.count);
      lab.appendChild(sw); lab.appendChild(nm); lab.appendChild(ct);
      lab.addEventListener('click', function (ev) {
        ev.preventDefault();
        if (state.filters.kinds.has(item.kind)) state.filters.kinds.delete(item.kind);
        else state.filters.kinds.add(item.kind);
        lab.classList.toggle('on', state.filters.kinds.has(item.kind));
        lab.classList.toggle('off', !state.filters.kinds.has(item.kind));
        scheduleViewerSave();
      });
      frag.appendChild(lab);
    });
    if (!state.kinds.list.length) {
      var e = document.createElement('div');
      e.className = 'list-empty';
      e.textContent = state.indexed ? 'no kinds (index is empty)' : 'not indexed';
      frag.appendChild(e);
    }
    kindFiltersEl.appendChild(frag);

    edgeFiltersEl.innerHTML = '';
    var efrag = document.createDocumentFragment();
    EDGE_KINDS.forEach(function (k) {
      var st = EDGE_STYLE[k] || DEFAULT_EDGE_STYLE;
      var el = document.createElement('span');
      el.className = 'edgekey' + (state.filters.edges.has(k) ? '' : ' off');
      el.setAttribute('data-edge', k);
      var bars = document.createElement('span');
      bars.className = 'bars';
      bars.style.borderTopColor = st.color;
      bars.style.borderTopStyle = st.dash ? 'dashed' : 'solid';
      bars.style.borderTopWidth = Math.max(2, st.width) + 'px';
      el.appendChild(bars);
      el.appendChild(document.createTextNode(k));
      el.addEventListener('click', function () {
        if (state.filters.edges.has(k)) state.filters.edges.delete(k);
        else state.filters.edges.add(k);
        el.classList.toggle('off', !state.filters.edges.has(k));
        scheduleViewerSave();
      });
      efrag.appendChild(el);
    });
    edgeFiltersEl.appendChild(efrag);
    filtersCountEl.textContent = state.kinds.list.length ? state.kinds.list.length + ' kinds' : '';
  }

  function setAllKinds(on) {
    state.kinds.list.forEach(function (x) {
      if (on) state.filters.kinds.add(x.kind);
      else state.filters.kinds.delete(x.kind);
    });
    buildFilters();
    scheduleViewerSave();
  }

  function setAllEdges(on) {
    state.filters.edges = new Set(on ? EDGE_KINDS : []);
    buildFilters();
    scheduleViewerSave();
  }

  function resetFilters() {
    state.filters.kinds = new Set(state.kinds.list.map(function (x) { return x.kind; }));
    state.filters.edges = new Set(EDGE_KINDS);
    state.filters.namespace = '';
    state.filters.namespaceDeep = true;
    if (nsSelectEl) nsSelectEl.value = '';
    if (nsDeepEl) nsDeepEl.checked = true;
    ensureProjectKeys().clear();
    buildProjectFilter();
    state.filters.hideIsolated = false;
    state.filters.hideEdgeLabels = false;
    state.filters.groupChildren = true;
    state.filters.groupEdges = true;
    state.hiddenChildren = new Set();
    state.hiddenChildNodes = new Map();
    hideIsolatedEl.checked = false;
    if (hideEdgeLabelsEl) hideEdgeLabelsEl.checked = false;
    groupChildrenEl.checked = true;
    groupEdgesEl.checked = true;
    if (hideChildrenEl) hideChildrenEl.checked = false;
    updateHideChildrenToggle();
    buildFilters();
    scheduleViewerSave();
  }

  /** Delete nodes currently hidden by the filters from the graph model. */
  function pruneHidden() {
    var view = computeView();
    var visible = new Set(view.nodes.map(function (n) { return n.id; }));
    var before = state.nodes.length;
    state.nodes = state.nodes.filter(function (n) { return visible.has(n.id); });
    state.edges = state.edges.filter(function (e) {
      return visible.has(e.source) && visible.has(e.target);
    });
    state.nodeOwners.forEach(function (set, id) { if (!visible.has(id)) state.nodeOwners.delete(id); });
    state.edgeOwners.forEach(function (set, id) { if (!state.edgeById.has(id)) state.edgeOwners.delete(id); });
    buildModel();
    // re-anchor remaining nodes
    state.edges.forEach(function (e) { tagOwner(state.edgeOwners, e.id, 'base'); });
    state.nodes.forEach(function (n) { tagOwner(state.nodeOwners, n.id, 'base'); });
    quickLayout();
    updateEmptyState();
    toast('Removed ' + fmtNum(before - state.nodes.length) + ' hidden nodes', 'ok');
  }

  // ================================================================== public definitions / strings

  function renderListRows(container, items, emptyText) {
    container.innerHTML = '';
    if (!items.length) {
      container.innerHTML = '<div class="list-empty">' + esc(emptyText) + '</div>';
      return;
    }
    var frag = document.createDocumentFragment();
    items.slice(0, 300).forEach(function (n) {
      var b = document.createElement('button');
      b.type = 'button';
      b.className = 'lrow';
      b.title = n.full || n.id;
      var dot = document.createElement('span');
      dot.className = 'dot';
      dot.style.background = kindColor(n.kind);
      var lab = document.createElement('span');
      lab.className = 'l-label';
      lab.textContent = n.label || n.name || n.full || n.id;
      var kd = document.createElement('span');
      kd.className = 'l-kind';
      kd.textContent = n.kind || '';
      b.appendChild(dot); b.appendChild(lab); b.appendChild(kd);
      b.addEventListener('click', function () { openListNode(n); });
      frag.appendChild(b);
    });
    container.appendChild(frag);
  }

  /** Bring a node from a side list into the graph, select it and expand it once. */
  function openListNode(n) {
    if (!n || !n.id) return;
    if (!state.nodeById.has(n.id)) {
      var ids = addNodes([n], 'base');
      buildModel();
      seedNewNodes(ids);
    }
    selectNode(n.id);
    followNode(n.id, null);
    quickLayout();
    expandNode(n.id);
  }

  function loadDefinitions() {
    if (state.defs.loaded) return Promise.resolve();
    return apiGet('/api/definitions', { scope: 'public', limit: 400 }).then(function (d) {
      state.defs.list = d.definitions || [];
      state.defs.total = d.total || state.defs.list.length;
      state.defs.loaded = true;
      pubCountEl.textContent = fmtNum(state.defs.total);
      renderPubList();
    }).catch(function (err) {
      pubListEl.innerHTML = '<div class="list-empty">' + esc('failed: ' + err.message) + '</div>';
    });
  }

  function renderPubList() {
    var q = state.defs.filter.toLowerCase();
    var items = state.defs.list.filter(function (n) {
      if (!q) return true;
      return (n.full || '').toLowerCase().indexOf(q) >= 0 ||
             (n.label || '').toLowerCase().indexOf(q) >= 0;
    });
    renderListRows(pubListEl, items, 'no matches');
  }

  function loadStrings() {
    if (state.strings.loaded) return Promise.resolve();
    strListEl.innerHTML = '<div class="list-empty">type to search string literals…</div>';
    return Promise.resolve();
  }

  var strTimer = 0;
  function onStringsFilter() {
    window.clearTimeout(strTimer);
    var q = strSearchEl.value.trim();
    state.strings.filter = q;
    if (!q) { strListEl.innerHTML = '<div class="list-empty">type to search string literals…</div>'; strCountEl.textContent = ''; return; }
    strTimer = window.setTimeout(function () {
      apiGet('/api/search', { q: q, limit: 300 }).then(function (d) {
        state.strings.list = (d.results || []).filter(function (n) { return n.kind === 'string'; });
        state.strings.loaded = true;
        strCountEl.textContent = fmtNum(state.strings.list.length);
        renderStrList();
      }).catch(function (err) {
        strListEl.innerHTML = '<div class="list-empty">' + esc('search failed: ' + err.message) + '</div>';
      });
    }, 220);
  }

  function renderStrList() {
    renderListRows(strListEl, state.strings.list, 'no string matches');
  }

  // ================================================================== resolve chain

  function runChain() {
    var id = state.selected;
    if (!id) { chainTreeEl.innerHTML = '<div class="list-empty">Select a node first.</div>'; return; }
    var depth = parseInt(chainDepthEl.value, 10) || 3;
    var dir = chainDirEl.value;
    state.chain.loading = true;
    state.chain.nodeId = id;
    chainTreeEl.innerHTML = '<div class="list-empty">walking…</div>';
    apiGet('/api/chain', { id: id, depth: depth, dir: dir, max: 300 })
      .then(function (d) {
        state.chain.data = d;
        state.chain.loading = false;
        state.chain.collapsed = state.chain.collapsed || new Set();
        chainCountEl.textContent = (d.flat || []).length + ' hops' + (d.truncated ? ' (truncated)' : '');
        renderChain();
      })
      .catch(function (err) {
        state.chain.loading = false;
        chainTreeEl.innerHTML = '<div class="list-empty">' + esc('chain failed: ' + err.message) + '</div>';
        toast('Resolve chain failed: ' + err.message, 'error');
      });
  }

  /** Recursive body with a hard depth guard so a cycle can never recurse forever. */
  function chainNodeEl(hop, depthGuard) {
    var wrap = document.createElement('div');
    var row = document.createElement('button');
    row.type = 'button';
    row.className = 'hop';
    row.title = 'Select this node in the graph — double-click to collapse its hops';
    row.style.paddingLeft = (8 + depthGuard * 12) + 'px';
    if (hop.node && hop.node.id === state.selected) row.style.background = '#1d2b47';

    var caret = document.createElement('span');
    caret.className = 'hop-caret';
    var kids = hop.children || [];
    var collapsed = state.chain.collapsed && state.chain.collapsed.has(hop.node && hop.node.id) && depthGuard > 0;
    caret.textContent = kids.length ? (collapsed ? '▸' : '▾') : '·';

    if (hop.edge) {
      var chip = document.createElement('span');
      chip.className = 'hop-edge';
      chip.textContent = EDGE_LABEL[hop.edge] || hop.edge;
      row.appendChild(chip);
    }
    if (hop.line) {
      var ln = document.createElement('span');
      ln.className = 'hop-line';
      ln.textContent = ':' + hop.line;
      row.appendChild(ln);
    }
    var lab = document.createElement('span');
    lab.className = 'hop-label';
    lab.textContent = (hop.node && (hop.node.label || hop.node.name || hop.node.id)) || '?';
    lab.title = (hop.node && hop.node.id) || '';
    row.appendChild(lab);
    if (hop.cycle) {
      var cy = document.createElement('span');
      cy.className = 'hop-cycle';
      cy.textContent = 'cycle';
      row.appendChild(cy);
    }
    caret && row.insertBefore(caret, row.firstChild);

    row.addEventListener('click', function () {
      if (hop.node && hop.node.id) {
        selectNode(hop.node.id);
        followNode(hop.node.id, null);
      }
    });
    if (kids.length) {
      row.addEventListener('dblclick', function (ev) {
        ev.preventDefault();
        var key = hop.node && hop.node.id;
        if (!state.chain.collapsed) state.chain.collapsed = new Set();
        if (state.chain.collapsed.has(key)) state.chain.collapsed.delete(key);
        else state.chain.collapsed.add(key);
        renderChain();
      });
    }
    wrap.appendChild(row);

    if (kids.length && !collapsed && depthGuard < 12) {
      kids.forEach(function (ch) { wrap.appendChild(chainNodeEl(ch, depthGuard + 1)); });
    }
    return wrap;
  }

  function renderChain() {
    var d = state.chain.data;
    chainTreeEl.innerHTML = '';
    if (!d) { chainTreeEl.innerHTML = '<div class="list-empty">Select a node, then Run.</div>'; return; }
    if (state.chain.flat) {
      var frag = document.createDocumentFragment();
      (d.flat || []).forEach(function (h) {
        frag.appendChild(chainNodeEl({ node: h.node, edge: h.edge, line: h.line, cycle: false, children: [] }, Math.min(h.depth || 0, 6)));
      });
      chainTreeEl.appendChild(frag);
      return;
    }
    chainTreeEl.appendChild(chainNodeEl(d.tree || {}, 0));
  }

