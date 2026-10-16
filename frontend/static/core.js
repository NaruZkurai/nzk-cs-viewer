/* csgraph — vanilla-JS canvas frontend for the local C# "function relation graph" tool.
 *
 * Split into feature files; no dependencies, no build step, no network beyond same-origin /api/* and /static/*.
 *
 * The graph view follows VRCX's working friends graph (MutualFriends.vue +
 * graphLayoutWorker.js): a ForceAtlas2 pass (strong gravity, scalingRatio = the
 * "spacing" slider, slowDown 2) settles the graph, a Noverlap pass then separates
 * overlapping nodes, and hover/selection applies the VRCX reducer — the focused node
 * turns yellow and grows, its neighbours stay coloured, everything else collapses to
 * a faint dot and its edges disappear.
 *
 * Structure:
 *   constants    — kind palette, edge styles, FA2/Noverlap and reducer tuning
 *   state        — single mutable app state object (plus small DOM refs)
 *   api helpers  — apiGet / apiPost with {ok:false} error surfacing
 *   graph model  — setGraph / adjacency / pair grouping / visible-subset computation
 *   layout       — ForceAtlas2 + Noverlap, time-sliced over animation frames
 *   render loop  — requestAnimationFrame: step layout, ease, camera follow, draw
 *   interaction  — pointer select(middle-drag pan | left-drag box | node move | pin)
 *                  / wheel zoom-to-cursor / hover tooltip
 *                  (the card is interactive; right-click pins it open for its actions)
 *   panels       — detail sidebar, layout+filters, folder-picker modal, search
 */
'use strict';

  // ------------------------------------------------------------------ constants

  // Fallback kind list; the live list comes from GET /api/kinds (never hard-code facets).
  var NODE_KINDS = ['namespace', 'class', 'struct', 'interface', 'enum', 'record', 'method',
                    'constructor', 'operator', 'property', 'field', 'event', 'string', 'external'];
  var EDGE_KINDS = ['calls', 'uses', 'assigns', 'constructs', 'inherits', 'contains'];

  // VirusTotal-ish kind colours on the dark navy canvas.
  var KIND_COLOR = {
    namespace:   '#86aaf9',
    class:       '#42c3d0',
    struct:      '#54ab98',
    interface:   '#d272ff',
    enum:        '#ffed2e',
    record:      '#f48385',
    method:      '#fa903e',
    constructor: '#e57300',
    operator:    '#ff5a50',
    property:    '#9db8f0',
    field:       '#8fd3a8',
    event:       '#ffc46b',
    string:      '#c8b6ff',
    external:    '#7a86a8',
    usage:       '#7a86a8'
  };
  var FALLBACK_COLOR = '#9aa6c2';

  // Edge line character. VT draws thin light-grey links; kind stays legible through the
  // dash pattern plus the pill label drawn on the edge.
  var EDGE_STYLE = {
    calls:      { color: '#c9d3e5', width: 1.2, dash: null,    arrow: true, alpha: 0.62 },
    uses:       { color: '#c9d3e5', width: 1.0, dash: [4, 4],  arrow: true, alpha: 0.50 },
    assigns:    { color: '#c9d3e5', width: 1.2, dash: [1, 3],  arrow: true, alpha: 0.55 },
    constructs: { color: '#c9d3e5', width: 1.2, dash: null,    arrow: true, alpha: 0.60 },
    inherits:   { color: '#c9d3e5', width: 2.0, dash: null,    arrow: true, alpha: 0.70 },
    contains:   { color: '#c9d3e5', width: 1.0, dash: [1, 5],  arrow: true, alpha: 0.42 }
  };
  // Parallel relations between the same pair are drawn as separate offset strands
  // (>=1px clear gap at any zoom) instead of overprinting each other.
  var EDGE_STACK_PX = 3.6;
  var EDGE_HIT_PX = 5;       // click tolerance around an edge
  var DEFAULT_EDGE_STYLE = EDGE_STYLE.uses;
  var EDGE_LABEL = {
    calls: 'Calling', uses: 'Uses', assigns: 'Assigns',
    constructs: 'Constructs', inherits: 'Inherits', contains: 'Children'
  };

  // Node chips (VT visual language)
  var CHIP_BG = '#232c42';
  var CHIP_BORDER = '#3d4a69';
  var CHIP_TEXT = '#dbe2f0';
  var CHIP_SEL_BORDER = '#86aaf9';
  var CHIP_DIM_BG = 'rgba(35,44,66,0.30)';
  var CHIP_DIM_TEXT = 'rgba(219,226,240,0.18)';
  var PIN_COLOR = '#c084fc';        // pinned-node marker
  var BG = '#0e1424';

  // ---------------------------------------------------------------- UI themes
  // `dark-v1` is the VirusTotal palette above. `nzk-dark+` recolours the whole
  // viewer from VS Code's Dark+ theme; editor values come from microsoft/vscode
  // extensions/theme-defaults/themes/dark_vs.json + dark_plus.json, and the
  // interface/enum (#b8d7a3) and struct (#86c691) entries are the Roslyn C#
  // extension's own additions. Types all share #4ec9b0 and members all share
  // #9cdcfe because that is what the editor does — the kind is still named in
  // the tooltip, the drawer and the filter chips.
  var KIND_COLOR_NZK_DARKPLUS = {
    namespace:   '#4ec9b0',  // entity.name.namespace
    class:       '#4ec9b0',  // entity.name.type
    struct:      '#86c691',  // C# extension: struct
    interface:   '#b8d7a3',  // C# extension: interface
    enum:        '#b8d7a3',  // C# extension: enum
    record:      '#4ec9b0',
    method:      '#dcdcaa',  // entity.name.function
    constructor: '#dcdcaa',
    operator:    '#c586c0',  // keyword.control / entity.name.operator
    property:    '#9cdcfe',  // entity.name.variable
    field:       '#9cdcfe',
    event:       '#9cdcfe',
    string:      '#ce9178',  // string
    external:    '#808080',  // out-of-corpus reference
    usage:       '#808080'
  };

  var UI_THEMES = {
    'dark-v1': {
      label: 'dark v1',
      kinds: KIND_COLOR, fallback: FALLBACK_COLOR,
      chipBg: CHIP_BG, chipBorder: CHIP_BORDER, chipText: CHIP_TEXT,
      chipSelBorder: CHIP_SEL_BORDER, chipDimBg: CHIP_DIM_BG,
      chipDimText: CHIP_DIM_TEXT, pin: PIN_COLOR
    },
    'nzk-dark+': {
      label: 'nzk-dark+',
      kinds: KIND_COLOR_NZK_DARKPLUS, fallback: '#858585',
      chipBg: '#252526', chipBorder: '#3c3c3c', chipText: '#d4d4d4',
      chipSelBorder: '#007acc', chipDimBg: 'rgba(45,45,48,0.30)',
      chipDimText: 'rgba(212,212,212,0.20)', pin: '#c586c0'
    }
  };
  var DEFAULT_UI_THEME = 'nzk-dark+';
  var uiThemeName = DEFAULT_UI_THEME;

  function uiPalette() {
    return UI_THEMES[uiThemeName] || UI_THEMES[DEFAULT_UI_THEME];
  }

  var MIN_ZOOM = 0.06, MAX_ZOOM = 6;
  // Labels are gated by the rendered radius in CSS pixels: a node is named once it
  // occupies at least this much screen space (so labels appear as you zoom in).
  // Focusing a node drops the threshold to zero for its closed neighbourhood.
  var LABEL_MIN_PX = 10;
  var EDGE_LABEL_ZOOM = 0.55;   // show relation pills above this zoom

  // ForceAtlas2 (graphology defaults + the reference's explicit overrides)
  var FA2_THETA = 0.8;              // barnesHutTheta
  var FA2_THETA_SQ = FA2_THETA * FA2_THETA;
  var FA2_SLOWDOWN = 2;
  var FA2_MAX_FORCE = 10;           // MAX_FORCE clamp from iterate.js
  var FA2_GRID_NODES = 600;         // exact repulsion below this, grid cutoff above
  var FA2_SUBDIVISION_ATTEMPTS = 3; // quadtree leaf overflow guard (iterate.js)
  // VRCX settings sliders
  var SPACING_MIN = 8, SPACING_MAX = 240, SPACING_DEFAULT = 120;
  var ITER_MIN = 300, ITER_MAX = 1500, ITER_DEFAULT = 800;
  var CURVE_MIN = 0, CURVE_MAX = 0.2, CURVE_DEFAULT = 0.1;
  // Neighbourhood depth box (focus subgraph hops). 0 = just the focused node.
  var DEPTH_MIN = 0, DEPTH_MAX = 9999999, DEPTH_DEFAULT = 1;
  // Node glyph ("dot") diameter in world units — the Layout panel slider.
  var NODE_SIZE_MIN = 2, NODE_SIZE_MAX = 24, NODE_SIZE_DEFAULT = 8;

  // Expansion spline. When the node being expanded already has placed neighbours
  // that lie on a line (a row, a column, a 45° diagonal — i.e. anything arranged
  // with Ctrl+Shift+H/V/D or dragged into place), the new children are appended to
  // that same line instead of being scattered by the force pass: they are laid on a
  // spline that leaves the node along the existing direction and curls gently
  // outward, and they are pinned so the layout cannot break the run.
  var SPLINE_GAP = 16;          // world-unit gap between consecutive chips
  var SPLINE_TURN = 0.10;       // radians the tangent bends per step (gentle curl)
  var SPLINE_TURN_MAX = 1.30;   // cap on the total bend, so long runs still read as a line
  var SPLINE_FIT_SLACK = 0.22;  // perpendicular spread / run: above this it is not a line
  var SPLINE_FIT_MIN = 4;       // ...and the absolute slack (world units) for short runs
  var SPLINE_AXIS_SLACK = 0.21; // radians a run may sit off the nearest 45° axis and still snap

  // Layout chunking: run the iteration loops in ~10 ms slices so the page stays
  // interactive while 300..1500 iterations settle on the main thread.
  var LAYOUT_SLICE_MS = 10;

  var MAX_SUBGRAPH = 450;

  // Code lookup (GET /api/find, /api/refs, /api/source)
  var FIND_LIMIT = 400;          // server-side cap requested per query
  var FIND_DEBOUNCE = 250;       // ms, per the spec
  var FIND_PAGE = 60;            // rows rendered per group before "show more"
  var FIND_MORE = 120;           // rows added by one "show more" click
  var SOURCE_CONTEXT = 30;       // ± lines for the window view
  var SOURCE_MAX_LINES = 700;    // hard cap on rendered source lines
  var SOURCE_MAX_DOC_LINES = 20000;  // a synthesized whole-node doc may be large

  // ------------------------------------------------------------------ tiny helpers

  function $(sel) { return document.querySelector(sel); }

  function clamp(v, lo, hi) { return v < lo ? lo : (v > hi ? hi : v); }

  /** FNV-1a: deterministic per-id jitter / seeding (no Math.random, so views are stable). */
  function hash32(str) {
    var h = 2166136261 >>> 0;
    for (var i = 0; i < str.length; i++) {
      h ^= str.charCodeAt(i);
      h = Math.imul(h, 16777619) >>> 0;
    }
    return h >>> 0;
  }

  function esc(s) {
    return String(s === null || s === undefined ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  function fmtNum(n) {
    if (n === null || n === undefined || isNaN(n)) return '—';
    return Number(n).toLocaleString('en-US');
  }

  function truncate(s, n) {
    s = String(s === null || s === undefined ? '' : s);
    return s.length > n ? s.slice(0, n - 1) + '…' : s;
  }

  function kindColor(kind) {
    var p = uiPalette();
    return p.kinds[kind] || p.fallback;
  }

  function forEachNodeList(list, fn) { Array.prototype.forEach.call(list, fn); }

  // ------------------------------------------------------------------ state

  var state = {
    // server / index
    indexed: false,
    root: '',
    rootChoice: '',
    counts: {},

    // graph model (incrementally built: namespaces first, then expansions)
    nodes: [],
    edges: [],
    pairs: [],
    nodeById: new Map(),
    edgeById: new Map(),
    edgeByKey: new Map(),    // 'kind\0source\0target' -> edge (one arrow per relation)
    adjacency: new Map(),
    pos: new Map(),
    nodeOwners: new Map(),   // node id -> Set(owner key): 'base' or a relation key
    edgeOwners: new Map(),   // edge id -> Set(owner key)
    focus: null,
    selected: null,                      // primary node (drawer anchor; the box-selection
                                         // set lives in arrange.js and drives this)
    hovered: null,
    truncated: false,

    // view
    view: { nodes: [], edges: [], deg: new Map() },
    camera: { x: 0, y: 0, zoom: 1 },
    follow: null,
    autoFit: false,
    w: 1, h: 1, dpr: 1,

    // facets (live from /api/kinds)
    kinds: { list: [], counts: {}, namespaces: [], types: [], public: [], loaded: false },

    // filters
    filters: {
      kinds: new Set(),        // off by default until /api/kinds arrives
      edges: new Set(EDGE_KINDS),
      namespace: '',           // '' = every namespace, else isolate one namespace
      namespaceDeep: true,     // true = ns + its descendants, false = ns only
      hideIsolated: false,
      hideEdgeLabels: false,   // the Calling / Uses / … pills
      groupChildren: true,
      groupEdges: true
    },
    depth: DEPTH_DEFAULT,

    // layout settings
    settings: { spacing: SPACING_DEFAULT, iterations: ITER_DEFAULT, curvature: CURVE_DEFAULT,
                nodeSize: NODE_SIZE_DEFAULT },
    layout: { mode: 'idle', iter: 0, total: 0, noverIter: 0, noverMax: 0, run: null },

    // panels
    history: [],                         // selection back-stack
    hiddenChildren: new Set(),           // node ids whose contains children are hidden
    hiddenChildNodes: new Map(),         // node id -> [child ids hidden from the canvas]
    detail: null,                        // last /api/node payload (properties + relations)
    relations: { expandedByNode: new Map(), listOpenByNode: new Map(), loading: false },
    chain: { data: null, flat: false, nodeId: null, loading: false },
    defs: { list: [], total: 0, filter: '', loaded: false },
    strings: { list: [], filter: '', loaded: false },
    searchResults: [],
    searchIndex: -1,
    picker: { path: '', parent: null, hasCs: false, csCount: 0 },

    // code lookup
    mode: 'graph',
    find: {
      query: '', simple: '', data: null, error: '', loading: false,
      shown: { definitions: FIND_PAGE, implementations: FIND_PAGE, usages: FIND_PAGE },
      rows: [], sel: -1
    },

    // source viewer
    source: {
      open: false, file: '', line: 0, start: 0, end: 0, total: 0,
      nodeId: null, match: '', matchKind: '', usageNode: null,
      hits: [], hitIndex: -1, theme: 'darkplus', lines: [],
      srcMode: 'doc'        // 'doc' = synthesized whole-node document, 'raw' = file as written
    },

    // whole-viewer theme (chrome + node kinds); persisted in its own file
    uiTheme: DEFAULT_UI_THEME
  };

  var graphSeq = 0;      // guards stale /api/subgraph responses
  var detailSeq = 0;     // guards stale /api/node responses
  var searchSeq = 0;
  var searchTimer = 0;
  var findSeq = 0;
  var findTimer = 0;
  var srcSeq = 0;

  // Persistence: the viewer state is mirrored into the index cache db (root of
  // the indexed folder) so a restart reopens where the last run left off.
  var viewerSaveTimer = 0;
  var viewerDirty = false;
  var pendingRestore = null;      // { positions: Map(id->[x,y]), camera: {...} }

  var canvas, ctx, canvasWrap, tooltipEl, suggestionsEl, statusEl,
      emptyHintEl, noticeEl, loadingEl, loadingTextEl, toastsEl, modalEl, modalBodyEl,
      modalPathEl, modalNoteEl, modalSelectEl, modalUpEl, rootInputEl, indexBtnEl,
      searchInputEl, depthSelectEl, hideIsolatedEl, hideEdgeLabelsEl, layoutStateEl,
      nsSelectEl, nsDeepEl,
      spacingEl, spacingOutEl, iterEl, iterOutEl, curveEl, curveOutEl,
      nodeSizeEl, nodeSizeOutEl,
      nodeDrawerEl, nodeTitleEl, nodePropsEl, relationsBodyEl, relationsCountEl,
      kindFiltersEl, edgeFiltersEl, filtersCountEl, groupChildrenEl, groupEdgesEl,
      pubListEl, pubSearchEl, pubCountEl, pubSectionEl, strListEl, strSearchEl,
      strCountEl, strSectionEl, chainTreeEl, chainDepthEl, chainDirEl, chainCountEl,
      chainSectionEl, sourceThemeEl, nodeCrumbsEl, hideChildrenEl, modalPathInputEl,
      modeGraphEl, modeFindEl, layoutEl, findPanelEl, findQueryEl, findMetaEl,
      findNoticeEl, findResultsEl, findCloseEl, sourcePaneEl, sourceTitleEl,
      sourceBodyEl, sourceNoteEl, sourceChipWrapEl, sourceMatchTagEl,
      shortcutsBtnEl, shortcutsModalEl, shortcutsBodyEl, uiThemeEl;

  // Box-selection + arranging live in their own file; built in init() once the DOM
  // refs exist, then called from the pointer / key / render paths below.
  var arrange;

  // ================================================================== API helpers

  function apiUrl(path, params) {
    var url = new URL(path, window.location.origin);
    if (params) {
      Object.keys(params).forEach(function (k) {
        var v = params[k];
        if (v !== null && v !== undefined) url.searchParams.set(k, String(v));
      });
    }
    return url.toString();
  }

  function unwrap(res) {
    return res.json().catch(function () { return null; }).then(function (data) {
      if (!res.ok || (data && data.ok === false)) {
        throw new Error((data && data.error) || ('HTTP ' + res.status + ' ' + res.statusText));
      }
      if (!data) throw new Error('Malformed JSON response');
      return data;
    });
  }

  function unreachable(err) {
    return new Error('Server unreachable (' + (err && err.message ? err.message : 'network error') + ')');
  }

  /** GET json; rejects with Error(message) on transport error, non-2xx, or {ok:false}. */
  function apiGet(path, params) {
    return fetch(apiUrl(path, params), {
      headers: { 'Accept': 'application/json' },
      cache: 'no-store'
    }).then(unwrap, unreachable);
  }

  function apiPost(path, body) {
    return fetch(apiUrl(path), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: JSON.stringify(body || {})
    }).then(unwrap, unreachable);
  }

  // ================================================================== chrome: toasts / overlay

  function toast(message, kind) {
    var el = document.createElement('div');
    el.className = 'toast' + (kind ? ' ' + kind : '');
    el.textContent = message;
    toastsEl.appendChild(el);
    var life = kind === 'error' ? 7000 : 3800;
    setTimeout(function () {
      el.style.transition = 'opacity .25s ease';
      el.style.opacity = '0';
      setTimeout(function () { el.remove(); }, 260);
    }, life);
    while (toastsEl.children.length > 5) toastsEl.removeChild(toastsEl.firstChild);
  }

  function showLoading(text) {
    loadingTextEl.textContent = text || 'Working…';
    loadingEl.classList.remove('hidden');
  }
  function hideLoading() { loadingEl.classList.add('hidden'); }

  function showNotice(text) {
    if (!text) { noticeEl.classList.add('hidden'); noticeEl.textContent = ''; return; }
    noticeEl.textContent = text;
    noticeEl.classList.remove('hidden');
  }

  function showEmpty(text) {
    emptyHintEl.innerHTML = text;
    emptyHintEl.classList.remove('hidden');
  }
  function hideEmpty() { emptyHintEl.classList.add('hidden'); }

  /** Notice bar text: focus context + server truncation, if any. */
  function updateCanvasNotice() {
    var msgs = [];
    msgs.push(state.nodes.length + ' nodes in view · double-click a node to expand its children and calling edges' +
      (state.hiddenChildren.size ? ' · children hidden for ' + state.hiddenChildren.size + ' node(s)' : ''));
    if (state.filters.namespace) {
      var nsDeep = state.filters.namespaceDeep !== false;
      msgs.push('Namespace ' + state.filters.namespace + (nsDeep ? ' + sub-namespaces' : ' only') +
        (state.view && state.view.nodes ? ' (' + state.view.nodes.length + ' shown)' : ''));
    }
    if (state.focus && state.nodeById.has(state.focus)) {
      var n = state.nodeById.get(state.focus);
      msgs.push('Neighbourhood of ' + (n.label || n.name || state.focus) +
        ' (depth ' + state.depth + ', ' + fmtNum(state.nodes.length) + ' nodes) — ' +
        'use “Full graph” to return.');
    }
    if (state.truncated) {
      msgs.push('Truncated by the server: showing ' + fmtNum(state.nodes.length) +
        ' highest-degree nodes. Focus a node to explore beyond the cap.');
    }
    showNotice(msgs.join('  ·  '));
  }

