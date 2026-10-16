/* csgraph - viewer-state persistence, init, and the entry point.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== persistence

  var VIEWER_POS_CAP = 20000;     // never persist an unbounded position table

  // A viewer_state blob that arrived from the cache but cannot be applied until
  // the index is loaded (the source pane needs the node table). Set by
  // restoreViewerState(), consumed by init() after loadNamespaces().
  var pendingTabSession = null;

  function snapshotViewerState() {
    var positions = [];
    state.pos.forEach(function (p, id) {
      if (!p || !p.seeded || positions.length >= VIEWER_POS_CAP) return;
      positions.push([id, Math.round(p.x * 10) / 10, Math.round(p.y * 10) / 10]);
    });
    var snap = {
      version: 1,
      settings: {
        spacing: state.settings.spacing,
        iterations: state.settings.iterations,
        curvature: state.settings.curvature,
        nodeSize: state.settings.nodeSize
      },
      theme: state.source.theme,
      sourceMode: state.source.srcMode,
      depth: state.depth,
      camera: { x: state.camera.x, y: state.camera.y, zoom: state.camera.zoom },
      filters: {
        kinds: Array.from(state.filters.kinds),
        edges: Array.from(state.filters.edges),
        namespace: state.filters.namespace || '',
        namespaceDeep: state.filters.namespaceDeep !== false,
        hideIsolated: state.filters.hideIsolated,
        hideEdgeLabels: state.filters.hideEdgeLabels,
        groupChildren: state.filters.groupChildren,
        groupEdges: state.filters.groupEdges
      },
      positions: positions
    };
    // Open documents + split (shell.js owns the tab stack). It rides the same
    // blob as the camera/theme/filters so a reload reopens the same pane.
    if (window.CsTabs && typeof window.CsTabs.session === 'function') {
      try { snap.tabs = window.CsTabs.session(); } catch (e) { /* never block a save */ }
    }
    return snap;
  }

  function scheduleViewerSave() {
    viewerDirty = true;
    window.clearTimeout(viewerSaveTimer);
    viewerSaveTimer = window.setTimeout(saveViewerState, 700);
    // Every meaningful state change lands here, so this is also where the other
    // windows are told what changed (filters, theme, depth, layout settings).
    if (window.CSSync) window.CSSync.push();
  }

  function saveViewerState() {
    if (!state.indexed) return;
    viewerDirty = false;
    apiPost('/api/viewer_state', snapshotViewerState())
      .catch(function () { /* a cache write is never worth a toast */ });
  }

  function restoreViewerState(v) {
    if (!v || typeof v !== 'object') return;
    var s = v.settings || {};
    if (typeof s.spacing === 'number') state.settings.spacing = clamp(s.spacing, SPACING_MIN, SPACING_MAX);
    if (typeof s.iterations === 'number') state.settings.iterations = clamp(s.iterations, ITER_MIN, ITER_MAX);
    if (typeof s.curvature === 'number') state.settings.curvature = clamp(s.curvature, CURVE_MIN, CURVE_MAX);
    if (typeof s.nodeSize === 'number') state.settings.nodeSize = clamp(s.nodeSize, NODE_SIZE_MIN, NODE_SIZE_MAX);
    chipMetricsCache.clear();

    if (typeof v.theme === 'string' && v.theme) {
      state.source.theme = v.theme;
      if (sourceThemeEl) sourceThemeEl.value = v.theme;
      applySourceTheme(v.theme);
    }
    if (v.sourceMode === 'raw' || v.sourceMode === 'doc' || v.sourceMode === 'effective') {
      state.source.srcMode = v.sourceMode === 'effective' ? 'doc' : v.sourceMode;
    }
    if (typeof v.depth === 'number' && isFinite(v.depth) && v.depth >= DEPTH_MIN) {
      state.depth = clamp(Math.round(v.depth), DEPTH_MIN, DEPTH_MAX);
      if (depthSelectEl) depthSelectEl.value = String(state.depth);
    }

    var f = v.filters;
    if (f && typeof f === 'object') {
      if (Array.isArray(f.kinds)) {
        state.filters.kinds = new Set(f.kinds);
        state.kindsSeen = new Set(f.kinds);   // applyKinds keeps the saved toggles
      }
      if (Array.isArray(f.edges)) state.filters.edges = new Set(f.edges);
      if (typeof f.namespace === 'string') state.filters.namespace = f.namespace;
      if (typeof f.namespaceDeep === 'boolean') state.filters.namespaceDeep = f.namespaceDeep;
      if (nsDeepEl) nsDeepEl.checked = state.filters.namespaceDeep !== false;
      if (typeof f.hideIsolated === 'boolean') state.filters.hideIsolated = f.hideIsolated;
      if (typeof f.hideEdgeLabels === 'boolean') state.filters.hideEdgeLabels = f.hideEdgeLabels;
      if (typeof f.groupChildren === 'boolean') state.filters.groupChildren = f.groupChildren;
      if (typeof f.groupEdges === 'boolean') state.filters.groupEdges = f.groupEdges;
      if (hideIsolatedEl) hideIsolatedEl.checked = state.filters.hideIsolated;
      if (hideEdgeLabelsEl) hideEdgeLabelsEl.checked = state.filters.hideEdgeLabels;
      if (hideChildrenEl) hideChildrenEl.checked = !state.filters.groupChildren;
      if (groupChildrenEl) groupChildrenEl.checked = state.filters.groupChildren;
      if (groupEdgesEl) groupEdgesEl.checked = state.filters.groupEdges;
    }

    var positions = new Map();
    if (Array.isArray(v.positions)) {
      v.positions.forEach(function (row) {
        if (Array.isArray(row) && row.length >= 3 && typeof row[0] === 'string') {
          positions.set(row[0], [Number(row[1]) || 0, Number(row[2]) || 0]);
        }
      });
    }
    var camera = null;
    if (v.camera && typeof v.camera === 'object' &&
        typeof v.camera.zoom === 'number' && v.camera.zoom > 0) {
      camera = {
        x: Number(v.camera.x) || 0,
        y: Number(v.camera.y) || 0,
        zoom: clamp(v.camera.zoom, MIN_ZOOM, MAX_ZOOM)
      };
    }
    if (camera || positions.size) {
      pendingRestore = { positions: positions, camera: camera };
    }
    // Open documents are restored only after the index is loaded; keep the blob
    // here and apply it in init(). Anything malformed is ignored.
    if (v.tabs && typeof v.tabs === 'object') pendingTabSession = v.tabs;
    syncLayoutControls();
  }

  /** Called once the base graph exists: restore the camera and stop auto-fit. */
  function applyPendingRestore() {
    if (!pendingRestore) return;
    if (pendingRestore.camera) state.camera = pendingRestore.camera;
    if (pendingRestore.camera) state.autoFit = false;
    pendingRestore = null;
  }

  function init() {
    canvas = $('#graph');
    canvasWrap = $('#canvasWrap');
    ctx = canvas.getContext('2d');
    tooltipEl = $('#tooltip');
    suggestionsEl = $('#suggestions');
    statusEl = $('#statusLine');
    emptyHintEl = $('#emptyHint');
    noticeEl = $('#notice');
    loadingEl = $('#loading');
    loadingTextEl = $('#loadingText');
    toastsEl = $('#toasts');
    modalEl = $('#modal');
    modalBodyEl = $('#modalBody');
    modalPathEl = $('#modalPath');
    modalNoteEl = $('#modalNote');
    modalSelectEl = $('#modalSelect');
    modalUpEl = $('#modalUp');
    rootInputEl = $('#rootInput');
    indexBtnEl = $('#indexBtn');
    searchInputEl = $('#searchInput');
    depthSelectEl = $('#depthSelect');
    nsSelectEl = $('#nsSelect');
    nsDeepEl = $('#nsDeep');
    hideIsolatedEl = $('#hideIsolated');
    hideEdgeLabelsEl = $('#hideEdgeLabels');
    layoutStateEl = $('#layoutState');
    spacingEl = $('#spacingRange');
    spacingOutEl = $('#spacingOut');
    iterEl = $('#iterRange');
    iterOutEl = $('#iterOut');
    curveEl = $('#curveRange');
    curveOutEl = $('#curveOut');
    nodeSizeEl = $('#nodeSizeRange');
    nodeSizeOutEl = $('#nodeSizeOut');
    modeGraphEl = $('#modeGraph');
    modeFindEl = $('#modeFind');
    layoutEl = document.querySelector('.layout');
    findPanelEl = $('#findPanel');
    findQueryEl = $('#findQuery');
    findMetaEl = $('#findMeta');
    findNoticeEl = $('#findNotice');
    findResultsEl = $('#findResults');
    findCloseEl = $('#findClose');
    sourcePaneEl = $('#sourcePane');
    sourceTitleEl = $('#sourceTitle');
    sourceBodyEl = $('#sourceBody');
    sourceNoteEl = $('#sourceNote');
    sourceChipWrapEl = $('#sourceChipWrap');
    sourceMatchTagEl = $('#sourceMatchTag');
    sourceThemeEl = $('#sourceTheme');
    nodeCrumbsEl = $('#nodeCrumbs');
    hideChildrenEl = $('#hideChildren');
    modalPathInputEl = $('#modalPathInput');
    nodeDrawerEl = $('#nodeDrawer');
    nodeTitleEl = $('#nodeTitle');
    nodePropsEl = $('#nodeProps');
    relationsBodyEl = $('#relationsBody');
    relationsCountEl = $('#relationsCount');
    kindFiltersEl = $('#kindFilters');
    edgeFiltersEl = $('#edgeFilters');
    filtersCountEl = $('#filtersCount');
    groupChildrenEl = $('#groupChildren');
    groupEdgesEl = $('#groupEdges');
    pubListEl = $('#pubList');
    pubSearchEl = $('#pubSearch');
    pubCountEl = $('#pubCount');
    pubSectionEl = $('#pubSection');
    strListEl = $('#strList');
    strSearchEl = $('#strSearch');
    strCountEl = $('#strCount');
    strSectionEl = $('#strSection');
    chainTreeEl = $('#chainTree');
    chainDepthEl = $('#chainDepth');
    chainDirEl = $('#chainDir');
    chainCountEl = $('#chainCount');
    chainSectionEl = $('#chainSection');
    sourceThemeEl = $('#sourceTheme');
    shortcutsBtnEl = $('#shortcutsBtn');
    shortcutsModalEl = $('#shortcutsModal');
    shortcutsBodyEl = $('#shortcutsBody');
    uiThemeEl = $('#uiTheme');

    if (!window.CSArrange) throw new Error('arrange.js failed to load');
    arrange = window.CSArrange({
      state: state, canvas: canvas, nodeDrawerEl: nodeDrawerEl, toast: toast,
      posOf: posOf, chipSizeFor: chipSizeFor, worldToScreen: worldToScreen,
      selectNode: selectNode, loadDetail: loadDetail, renderNodeDrawer: renderNodeDrawer,
      modeIsFind: modeIsFind, scheduleViewerSave: scheduleViewerSave
    });

    applyUiTheme(state.uiTheme, { silent: true });   // paint before the fetch lands
    loadUiTheme();

    if (window.CSReorg) window.CSReorg.install();

    var savedTheme = 'darkplus';
    try { savedTheme = window.localStorage.getItem('csgraph.theme') || 'darkplus'; } catch (e) { /* ignore */ }
    state.source.theme = savedTheme;
    if (sourceThemeEl) sourceThemeEl.value = savedTheme;
    applySourceTheme(savedTheme);

    setMode('graph', false);
    syncLayoutControls();
    buildFilters();
    wireEvents();

    // A pane window (panewin.js) hosts ONE pane and nothing else: the pane is
    // moved into #paneRoot before the canvas is measured or the render loop is
    // started, because there is no canvas to draw here. The graph model itself is
    // still loaded below, so the pane's own actions work.
    var paneWin = !!(window.CsPaneWin && window.CsPaneWin.active());
    if (paneWin) window.CsPaneWin.boot();

    resize();
    showEmpty('Connecting to server…');
    if (!paneWin) window.requestAnimationFrame(frame);

    // Flush any unsaved viewer state when the window goes away, and as a
    // backstop while it stays open (the cache write is tiny and local).
    window.addEventListener('pagehide', function () { if (viewerDirty) saveViewerState(); });
    document.addEventListener('visibilitychange', function () {
      if (document.visibilityState === 'hidden' && viewerDirty) saveViewerState();
    });
    window.setInterval(function () { if (viewerDirty) saveViewerState(); }, 20000);

    apiGet('/api/state')
      .then(function (s) {
        applyState(s);
        // A document window (?win=doc&node=…) shows nothing but its document:
        // no graph load, no layout, no canvas work at all — and no part in the
        // window-to-window sync either. It is a document you asked to pop out, so
        // it must not follow the main window's selection, nor push its own (it
        // has no index loaded, and its "no document open" default would close the
        // main window's source pane).
        if (window.CsWindows && window.CsWindows.bootDocWindow()) return;
        // A pane window shows one pane; it loads the same graph model so the
        // pane's actions (select, expand, walk a chain) behave as they do in the
        // main window, then hands control to panewin.js.
        if (window.CsPaneWin && window.CsPaneWin.active()) return window.CsPaneWin.start(s);
        // The state is loaded: from here on this window publishes what it shows.
        if (s.indexed) {
          return loadNamespaces().then(function (kinds) {
            // Reopen the documents the last session left open (tabs + split).
            // Failing to restore must never keep the app from loading.
            if (pendingTabSession && window.CsTabs) {
              try { window.CsTabs.restoreSession(pendingTabSession); }
              catch (e) { /* a bad session is simply not restored */ }
            }
            pendingTabSession = null;
            if (window.CSSync) window.CSSync.enable();
            return kinds;
          });
        }
        if (window.CSSync) window.CSSync.enable();
        showEmpty('Nothing indexed yet.<br>Pick a folder with <b>Browse</b>, then press <b>Index</b>.');
      })
      .catch(function (err) {
        statusEl.textContent = 'server unavailable';
        showEmpty('Could not reach the server.<br>' + esc(err.message));
        toast('Could not reach the API: ' + err.message, 'error');
      });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
