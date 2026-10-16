/* csgraph v2 — keeping the windows in step.
 *
 * Part of the RUST-ONLY v2 viewer. Not loaded by v1.
 *
 * Every window of the viewer is a full app instance that loads the same graph
 * from the shared in-process engine. What it does *not* share is the ephemeral
 * view: which node is selected, which document is open, what the find query is.
 * That is what this module replicates, so a pane detached into its own window
 * (panewin.js) shows the same thing the main window shows, and an action taken
 * in the detached pane is reflected back in the main window — Unity's floating
 * panels, not a screenshot.
 *
 * Transport is `CsWindows.emit/on`: Tauri events natively, a BroadcastChannel in
 * the plain-web build (where only document windows exist, so sync is a no-op).
 *
 * The persisted half of the state (camera, node positions, theme, filters,
 * depth) already travels through `/api/viewer_state`; this is only the live
 * half, and filters/theme/settings are included because a detached Sidebar
 * changes them and the main window's canvas must follow immediately.
 */
'use strict';

window.CSSync = (function () {
  var applying = false;
  var enabled = false;        // only a window that has loaded its state broadcasts
  var last = '';              // serialised payload, for echo suppression
  var timer = 0;

  function ready() {
    return typeof state === 'object' && state !== null;
  }

  /**
   * The live view state, small enough to ship on every change.
   *
   * Only the document and the selection are unconditional. Filters, layout
   * settings, themes, mode and the find query are included only by a window that
   * actually loaded the index (`state.kinds.loaded`): a document window runs none
   * of that code, and its empty kind set would otherwise switch every kind off in
   * the main window the moment it announced itself.
   */
  function view() {
    var s = state;
    var v = {
      selected: s.selected || null,
      focus: s.focus || null,
      source: {
        open: !!s.source.open,
        nodeId: s.source.nodeId || null,
        usageNode: s.source.usageNode ? s.source.usageNode.id : null,
        file: s.source.file || '',
        line: s.source.line || 0,
        mode: s.source.srcMode || 'doc'
      }
    };
    if (!s.kinds || !s.kinds.loaded) return v;

    v.hiddenChildren = Array.from(s.hiddenChildren || []);
    v.depth = s.depth;
    v.settings = {
      spacing: s.settings.spacing,
      iterations: s.settings.iterations,
      curvature: s.settings.curvature,
      nodeSize: s.settings.nodeSize
    };
    v.filters = {
      kinds: Array.from(s.filters.kinds),
      edges: Array.from(s.filters.edges),
      namespace: s.filters.namespace || '',
      namespaceDeep: s.filters.namespaceDeep !== false,
      hideIsolated: !!s.filters.hideIsolated,
      hideEdgeLabels: !!s.filters.hideEdgeLabels,
      groupChildren: !!s.filters.groupChildren,
      groupEdges: !!s.filters.groupEdges
    };
    v.uiTheme = s.uiTheme;
    v.sourceTheme = s.source.theme;
    v.mode = s.mode;
    v.find = s.find.query || '';
    return v;
  }

  /**
   * Announce this window's view state. Debounced, and skipped when nothing
   * changed, so the windows cannot ping-pong the same value at each other.
   *
   * Silent until `enable()`: a window that has not loaded `/api/state` yet would
   * otherwise broadcast its defaults (an empty kind filter, no document) over
   * the window that has.
   */
  function push(force) {
    if (!enabled || applying || !ready() || !window.CsWindows) return;
    window.clearTimeout(timer);
    timer = window.setTimeout(function () {
      var v = view();
      var json = JSON.stringify(v);
      if (json === last && !force) return;
      last = json;
      window.CsWindows.emit('cs:sync', { view: v });
    }, 80);
  }

  /** This window is loaded: start listening for, and publishing, the view state. */
  function enable() {
    if (!enabled) {
      enabled = true;
      listen();
    }
    push();
  }

  var listening = false;

  /** Apply the view state any other window publishes. */
  function listen() {
    if (listening || !window.CsWindows) return;
    listening = true;
    window.CsWindows.on('cs:sync', function (msg) {
      if (!msg || !msg.view) return;
      apply(msg.view);
    });
    // A window that came up without a handed-over state (a pane started with
    // `--open-pane`, a document window opened from the menu) asks whoever is
    // already running to publish, even if nothing changed since.
    window.CsWindows.on('cs:request', function () { push(true); });
  }

  /** Apply a view state from another window. Returns false for an echo. */
  function apply(v) {
    if (!v || !ready()) return false;
    var json = JSON.stringify(v);
    if (json === last) return false;          // our own broadcast, or a repeat
    last = json;
    applying = true;
    try {
      applyNow(v);
    } catch (e) {
      /* A failed application must never break the window it arrived in. */
      if (window.console && console.warn) console.warn('cs:sync apply failed', e);
    } finally {
      window.setTimeout(function () { applying = false; }, 0);
    }
    return true;
  }

  function applyNow(v) {
    var s = state;

    if (v.filters) {
      var f = v.filters;
      s.filters.kinds = new Set(f.kinds || []);
      s.kindsSeen = new Set(f.kinds || []);    // keep the saved toggles authoritative
      s.filters.edges = new Set(f.edges || []);
      s.filters.namespace = typeof f.namespace === 'string' ? f.namespace : '';
      s.filters.namespaceDeep = f.namespaceDeep !== false;
      if (nsDeepEl) nsDeepEl.checked = s.filters.namespaceDeep;
      s.filters.hideIsolated = !!f.hideIsolated;
      s.filters.hideEdgeLabels = !!f.hideEdgeLabels;
      s.filters.groupChildren = !!f.groupChildren;
      s.filters.groupEdges = !!f.groupEdges;
      if (typeof buildNamespaceFilter === 'function') buildNamespaceFilter();
      if (hideIsolatedEl) hideIsolatedEl.checked = s.filters.hideIsolated;
      if (hideEdgeLabelsEl) hideEdgeLabelsEl.checked = s.filters.hideEdgeLabels;
      if (groupChildrenEl) groupChildrenEl.checked = s.filters.groupChildren;
      if (groupEdgesEl) groupEdgesEl.checked = s.filters.groupEdges;
      if (hideChildrenEl) hideChildrenEl.checked = !s.filters.groupChildren;
      if (typeof buildFilters === 'function') buildFilters();
    }

    if (v.settings) {
      if (typeof v.settings.spacing === 'number') s.settings.spacing = v.settings.spacing;
      if (typeof v.settings.iterations === 'number') s.settings.iterations = v.settings.iterations;
      if (typeof v.settings.curvature === 'number') s.settings.curvature = v.settings.curvature;
      if (typeof v.settings.nodeSize === 'number') s.settings.nodeSize = v.settings.nodeSize;
      if (typeof syncLayoutControls === 'function') syncLayoutControls();
    }

    if (typeof v.depth === 'number' && isFinite(v.depth) && v.depth >= DEPTH_MIN) {
      s.depth = clamp(Math.round(v.depth), DEPTH_MIN, DEPTH_MAX);
      if (depthSelectEl) depthSelectEl.value = String(s.depth);
    }
    if (Array.isArray(v.hiddenChildren)) s.hiddenChildren = new Set(v.hiddenChildren);
    if (v.uiTheme && typeof applyUiTheme === 'function') applyUiTheme(v.uiTheme, { silent: true });
    if (v.sourceTheme && typeof applySourceTheme === 'function') applySourceTheme(v.sourceTheme);

    // The document: follow the window that moved it.
    if (v.source && typeof openSource === 'function' && typeof closeSource === 'function') {
      var cur = s.source;
      var curUsage = cur.usageNode ? cur.usageNode.id : null;
      var same = (cur.nodeId || null) === (v.source.nodeId || null) &&
                 curUsage === (v.source.usageNode || null) &&
                 (cur.file || '') === (v.source.file || '') &&
                 (cur.line || 0) === (v.source.line || 0) &&
                 (cur.srcMode || 'doc') === (v.source.mode || 'doc');
      if (!v.source.open) {
        // A remote close means "show nothing": with tabs (window.CsTabs) the pane
        // must drop its whole document stack, not just the active tab, or this
        // window would immediately push a different open document back.
        if (cur.open) {
          window.__csRemoteClose = true;
          try { closeSource(); } finally { window.__csRemoteClose = false; }
        }
      } else if (!same || !cur.open) {
        if (v.source.nodeId) {
          openSource({ node: v.source.nodeId, mode: v.source.mode });
        } else if (v.source.file && v.source.line) {
          openSource({ file: v.source.file, line: v.source.line, mode: v.source.mode });
        }
      }
    }

    // Selection: pull the node in if this window has not got it yet.
    if (v.selected && v.selected !== s.selected && typeof selectNode === 'function') {
      if (s.nodeById && s.nodeById.has(v.selected)) selectNode(v.selected, { noHistory: true });
      else if (typeof goToNode === 'function') goToNode(v.selected);
    }
    if (v.focus && v.focus !== s.focus) s.focus = v.focus;

    if (v.mode && v.mode !== s.mode && typeof setMode === 'function') setMode(v.mode, false);
    if (v.find && v.find !== s.find.query && typeof runFind === 'function') runFind(v.find);

    // The canvas (when there is one) must show the same filter subset.
    if (typeof computeView === 'function' && typeof canvas !== 'undefined' && canvas) {
      computeView();
      if (typeof updateCanvasNotice === 'function') updateCanvasNotice();
      if (typeof startLayout === 'function') startLayout({ reinitialize: false });
    }
  }

  return {
    view: view,
    push: push,
    enable: enable,
    listen: listen,
    apply: apply
  };
})();
