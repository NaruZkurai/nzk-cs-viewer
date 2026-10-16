/* csgraph v2 — the Blocks panel: a Scratch-like, read-only canvas over the graph.
 *
 * Part of the RUST-ONLY v2 viewer. Not loaded by v1. No writes: every request
 * goes to an existing /api/* read route.
 *
 * The panel shows the *current document's* declarations as block cards with
 * typed ports (method parameters on the left, the return type on the right),
 * draws the graph's reference edges between those cards, opens a hover peek
 * (source snippet + reference list from /api/node) and navigates the shell to a
 * node when a card is clicked.
 *
 * It follows the same live view state as the other panels: it reads
 * `state.selected` / `state.source.nodeId` / `state.focus` and re-renders when
 * the shared bus (`window.CSSync.push`) or a navigation entry point
 * (selectNode / goToNode / openSource / renderSource / closeSource) fires. It
 * documents itself through `window.CsScratch` so probes can drive it.
 *
 * Data sources (all reads):
 *   /api/decls?node=<scope>     block cards (the document's declarations)
 *   /api/subgraph?id=<scope>    reference edges between the cards
 *   /api/source?node=<id>       hover-peek source snippet
 *   /api/node?id=<id>           hover-peek reference list
 *
 * The scope is the declaration root of the anchor: a member anchor renders the
 * type that declares it (the same thing /api/doc would synthesize), a
 * namespace/type anchor renders itself.
 */
'use strict';

window.CsScratch = (function () {
  var MAX_CARDS = 160;             // render cap; the edge set still uses every decl
  var PEEK_LINES = 12;             // source lines shown in the hover peek
  var PEEK_REFS = 4;               // relation buckets shown in the hover peek
  var PEEK_REF_NODES = 3;          // node names shown per bucket
  var SUBGRAPH_MAX = 250;

  var TYPE_KINDS = { namespace: 1, class: 1, struct: 1, interface: 1, enum: 1, record: 1 };
  var EDGE_COLOR = {
    calls: '#86aaf9', uses: '#42c3d0', assigns: '#ffed2e',
    constructs: '#fa903e', inherits: '#d272ff', contains: '#6b7897'
  };

  var installed = false;
  var opened = false;
  var stale = true;
  var lastOk = false;

  var panelEl = null, headBtnEl = null, closeBtnEl = null, titleEl = null,
      metaEl = null, scrollEl = null, stageEl = null, cardsEl = null, svgEl = null,
      peekEl = null, peekKindEl = null, peekNameEl = null, peekFileEl = null,
      peekSrcEl = null, peekRefsEl = null;

  var currentAnchor = null;
  var renderSeq = 0;
  var peekSeq = 0;
  var peekCard = null;
  var drawQueued = false;
  var resizeObs = null;

  // ------------------------------------------------------------------ helpers

  function st() { return window.state || {}; }
  function apiGet(path, params) { return window.apiGet(path, params); }
  function esc(s) {
    if (typeof window.esc === 'function') return window.esc(s);
    return String(s === null || s === undefined ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }
  function num(n) {
    return (typeof n === 'number' && isFinite(n)) ? String(n) : '0';
  }
  function basename(p) {
    p = String(p || '');
    var i = p.lastIndexOf('/');
    return i >= 0 ? p.slice(i + 1) : p;
  }
  function shortId(id) {
    var bits = String(id || '').split('.');
    return bits[bits.length - 1] || String(id || '');
  }
  function kindColor(kind) {
    if (typeof window.kindColor === 'function') {
      try { return window.kindColor(kind); } catch (e) { /* fall through */ }
    }
    return '#9aa6c2';
  }
  function isContainer(d) { return !!(d && TYPE_KINDS[d.kind]); }

  /** Method/constructor parameter type list, taken from the node id's `(…)`. */
  function splitParams(id) {
    var m = /\(([\s\S]*)\)\s*$/.exec(String(id || ''));
    if (!m) return [];
    var inner = m[1];
    if (!inner.trim()) return [];
    var out = [], depth = 0, cur = '';
    for (var i = 0; i < inner.length; i++) {
      var c = inner[i];
      if (c === '<' || c === '(' || c === '[' || c === '{') depth++;
      else if (c === '>' || c === ')' || c === ']' || c === '}') depth = Math.max(0, depth - 1);
      if (c === ',' && depth === 0) { if (cur.trim()) out.push(cur.trim()); cur = ''; }
      else cur += c;
    }
    if (cur.trim()) out.push(cur.trim());
    return out.slice(0, 8);
  }

  /** A type's short label for a port (`System.String` -> `String`). */
  function shortType(t) {
    t = String(t || '').trim();
    if (!t) return '·';
    var m = /^(.*?)(<.*>)?$/.exec(t);
    var core = m ? m[1] : t;
    var bits = core.split('.');
    var tail = bits[bits.length - 1] || t;
    return (m && m[2] ? tail + m[2] : tail);
  }

  function typeSelectors(n) {
    var out = [];
    if (isContainer(n)) out.push(n.id);
    else if (n.owner) out.push(n.owner);
    else out.push(n.id);
    return out;
  }

  // ------------------------------------------------------- panel open / close

  function openPanel() {
    if (!panelEl) return;
    opened = true;
    panelEl.classList.remove('hidden');
    if (headBtnEl) { headBtnEl.classList.add('active'); headBtnEl.setAttribute('aria-pressed', 'true'); }
    showPeekFor(null);
    refresh(false);
    scheduleDraw();
  }

  function closePanel() {
    if (!panelEl) return;
    opened = false;
    panelEl.classList.add('hidden');
    if (headBtnEl) { headBtnEl.classList.remove('active'); headBtnEl.setAttribute('aria-pressed', 'false'); }
    hidePeek();
  }

  function togglePanel() { if (opened) closePanel(); else openPanel(); }

  // ------------------------------------------------------- live-view following

  /**
   * Re-render when the shared view state moves. `CSSync.push` is the live
   * view-state bus every panel change already goes through; the navigation
   * entry points are wrapped too, so a direct select/open in this window (where
   * the bus is not the transport) still lands here.
   */
  function wrapBus() {
    var bus = window.CSSync;
    if (bus && typeof bus.push === 'function' && !bus.push.__csScratch) {
      var origPush = bus.push;
      var wrappedPush = function () {
        var r = origPush.apply(this, arguments);
        scheduleRefresh();
        return r;
      };
      wrappedPush.__csScratch = true;
      bus.push = wrappedPush;
    }
    ['selectNode', 'goToNode', 'openSource', 'renderSource', 'closeSource', 'centerNode'].forEach(wrapFn);
  }

  function wrapFn(name) {
    var fn = window[name];
    if (typeof fn !== 'function' || fn.__csScratch) return;
    var wrapped = function () {
      var r = fn.apply(this, arguments);
      scheduleRefresh();
      return r;
    };
    wrapped.__csScratch = true;
    window[name] = wrapped;
  }

  var refreshPending = false;
  function scheduleRefresh() {
    if (!installed || refreshPending) return;
    refreshPending = true;
    var run = function () {
      refreshPending = false;
      refresh(false);
    };
    if (window.requestAnimationFrame) window.requestAnimationFrame(run);
    else window.setTimeout(run, 16);
  }

  /** The node the panel is about: the selection, else the open document, else focus. */
  function anchorId() {
    var s = st();
    if (s.selected) return s.selected;
    if (s.source && s.source.open && s.source.nodeId) return s.source.nodeId;
    if (s.focus) return s.focus;
    return null;
  }

  function refresh(force) {
    if (!installed) return;
    if (!opened) { stale = true; return; }
    var anchor = anchorId();
    if (!force && anchor && anchor === currentAnchor && lastOk) {
      markSelected();
      scheduleDraw();
      return;
    }
    currentAnchor = anchor;
    stale = false;
    if (!anchor) {
      lastOk = false;
      renderMessage('Select a node — or open a document — to see its blocks.');
      return;
    }
    titleEl.textContent = 'Blocks · ' + shortId(anchor);
    var seq = ++renderSeq;
    loadScope(anchor).then(function (scopeRes) {
      if (seq !== renderSeq) return null;
      return apiGet('/api/subgraph', { id: scopeRes.scope, depth: 1, max: SUBGRAPH_MAX })
        .then(function (sg) {
          if (seq !== renderSeq) return;
          renderBlocks(scopeRes, sg);
        });
    }).catch(function (err) {
      if (seq !== renderSeq) return;
      lastOk = false;
      renderMessage('Could not load blocks for ' + anchor + ': ' +
        ((err && err.message) || err));
    });
  }

  /** Resolve the declaration root for an anchor (a member renders its type). */
  function loadScope(anchor) {
    var s = st();
    var known = (s.nodeById && s.nodeById.get) ? s.nodeById.get(anchor) : null;
    var scope = known ? typeSelectors(known)[0] : anchor;
    return apiGet('/api/decls', { node: scope }).then(function (d) {
      var decls = (d && d.decls) || [];
      if (!known && decls.length && !isContainer(decls[0]) &&
          decls[0].owner && decls[0].owner !== scope) {
        return apiGet('/api/decls', { node: decls[0].owner }).then(function (d2) {
          return { scope: decls[0].owner, decls: (d2 && d2.decls) || [] };
        });
      }
      return { scope: scope, decls: decls };
    });
  }

  // -------------------------------------------------------------- rendering

  function cardHtml(d, idx) {
    var kind = d.kind || 'node';
    var params = (kind === 'method' || kind === 'constructor') ? splitParams(d.id) : [];
    var ret = String(d.ret || '');
    var inPorts = params.map(function (p) {
      return '<span class="port port-in" data-type="' + esc(p) + '" title="' + esc(p) + '">' +
        '<span class="port-dot"></span><span class="port-type">' + esc(shortType(p)) + '</span></span>';
    }).join('');
    var outType = ret || (isContainer(d) ? kind : (kind === 'field' || kind === 'property' || kind === 'event' ? 'value' : ''));
    var outPort = '<span class="port port-out" data-type="' + esc(outType) + '" title="' + esc(outType || kind) + '">' +
      '<span class="port-type">' + esc(outType || '·') + '</span><span class="port-dot"></span></span>';
    var sig;
    if (kind === 'method' || kind === 'constructor') {
      sig = '<span class="bc-ret">' + esc(ret) + '</span>' +
        '<span class="bc-name">' + esc(d.name) + '</span>' +
        '<span class="bc-args">(' + esc(params.join(', ')) + ')</span>';
    } else {
      sig = '<span class="bc-kw">' + esc(kind) + '</span>' +
        '<span class="bc-name">' + esc(d.name) + '</span>';
    }
    var flags = (d.access ? '<span class="bc-flag">' + esc(d.access) + '</span>' : '') +
      (d.static ? '<span class="bc-flag">static</span>' : '');
    return '<div class="block-card" data-kind="' + esc(kind) + '" data-index="' + idx +
      '" data-id="' + esc(d.id) + '">' +
      '<div class="bc-head">' +
      '<span class="bc-kind" style="color:' + kindColor(kind) + '">' + esc(kind) + '</span>' +
      '<span class="bc-title" title="' + esc(d.full || d.id) + '">' + esc(d.name) + '</span>' +
      '<span class="bc-flags">' + flags + '</span>' +
      '</div>' +
      '<div class="bc-sig">' + sig + '</div>' +
      '<div class="bc-ports"><div class="bc-in">' + inPorts + '</div>' +
      '<div class="bc-out">' + outPort + '</div></div>' +
      '<div class="bc-foot">' +
      '<span class="bc-loc" title="' + esc(d.file || '') + '">' + esc(basename(d.file)) +
      (d.line ? ':' + d.line : '') + '</span>' +
      '<span class="bc-deg" title="in-degree / out-degree">↑' + num(d.inDeg) + ' ↓' + num(d.outDeg) + '</span>' +
      '</div></div>';
  }

  function renderMessage(text) {
    if (!cardsEl) return;
    if (svgEl) svgEl.innerHTML = '';
    if (stageEl) stageEl.__edges = [];
    cardsEl.innerHTML = '<div class="blocks-empty">' + esc(text) + '</div>';
    if (titleEl) titleEl.textContent = 'Blocks';
    if (metaEl) metaEl.textContent = '';
  }

  function renderBlocks(scopeRes, sg) {
    var decls = scopeRes.decls || [];
    var cards = decls.slice(0, MAX_CARDS);
    if (!cards.length) {
      lastOk = false;
      renderMessage('No declarations for ' + scopeRes.scope + '.');
      return;
    }
    lastOk = true;
    var html = cards.map(cardHtml).join('');
    cardsEl.innerHTML = html;

    var elById = Object.create(null);
    var declById = Object.create(null);
    cards.forEach(function (d) { declById[d.id] = d; });
    var children = cardsEl.children;
    for (var i = 0; i < children.length; i++) {
      var el = children[i];
      var id = el.getAttribute('data-id');
      elById[id] = el;
      el.__decl = declById[id] || null;
    }

    // Reference edges between rendered cards (edges touching a card that was
    // not rendered — the namespace parent, an out-of-scope callee — are dropped).
    var edges = [];
    var seen = Object.create(null);
    (sg && sg.edges ? sg.edges : []).forEach(function (e) {
      var s = elById[e.source], t = elById[e.target];
      if (!s || !t) return;
      var key = e.source + '\u0000' + e.target;
      seen[key] = (seen[key] || 0) + 1;
      edges.push({ kind: e.kind, source: e.source, target: e.target,
                   count: e.count || 1, srcEl: s, dstEl: t, nest: seen[key] - 1 });
    });
    stageEl.__edges = edges;

    var more = decls.length > cards.length ? ' · +' + (decls.length - cards.length) + ' more' : '';
    metaEl.textContent = cards.length + ' blocks · ' + edges.length + ' edges' + more;
    titleEl.textContent = 'Blocks · ' + (scopeRes.scope || '').split('.').pop();
    markSelected();
    scheduleDraw();
  }

  function markSelected() {
    if (!cardsEl) return;
    var sel = st().selected || null;
    var children = cardsEl.querySelectorAll('.block-card');
    for (var i = 0; i < children.length; i++) {
      children[i].classList.toggle('selected', !!sel && children[i].getAttribute('data-id') === sel);
    }
  }

  // ------------------------------------------------------------------ edges

  function scheduleDraw() {
    if (drawQueued) return;
    drawQueued = true;
    var run = function () { drawQueued = false; drawEdges(); };
    if (window.requestAnimationFrame) window.requestAnimationFrame(run);
    else window.setTimeout(run, 16);
  }

  function portPoint(el, side, origin) {
    var port = el.querySelector(side === 'out' ? '.port-out' : '.port-in');
    var node = port || el;
    var r = node.getBoundingClientRect();
    return {
      x: r.left + r.width / 2 - origin.left,
      y: r.top + r.height / 2 - origin.top
    };
  }

  function drawEdges() {
    if (!svgEl || !stageEl || !cardsEl || !opened) return;
    var edges = stageEl.__edges || [];
    var origin = stageEl.getBoundingClientRect();
    // Size the SVG from the cards layer, never from the stage's own scroll box:
    // the SVG is an absolutely-positioned child of the stage, so measuring
    // `stageEl.scrollWidth/scrollHeight` would include the SVG itself and the
    // size could only ever grow (a narrow panel would keep a stale, spurious
    // horizontal scrollbar). `offsetLeft/Top` is the stage padding.
    var w = Math.max(cardsEl.offsetLeft + cardsEl.scrollWidth, 1);
    var h = Math.max(cardsEl.offsetTop + cardsEl.scrollHeight, 1);
    svgEl.setAttribute('width', String(w));
    svgEl.setAttribute('height', String(h));
    svgEl.setAttribute('viewBox', '0 0 ' + w + ' ' + h);

    var defs = '<defs><marker id="blocksArrow" viewBox="0 0 8 8" refX="7" refY="4"' +
      ' markerWidth="7" markerHeight="7" orient="auto-start-reverse">' +
      '<path d="M0,0 L8,4 L0,8 z" fill="context-stroke"></path></marker></defs>';
    var paths = '';
    for (var i = 0; i < edges.length; i++) {
      var e = edges[i];
      var a = portPoint(e.srcEl, 'out', origin);
      var b = portPoint(e.dstEl, 'in', origin);
      var dy = (e.nest || 0) * 4;
      var dx = Math.max(30, Math.abs(b.x - a.x) * 0.42);
      var d = 'M' + a.x.toFixed(1) + ',' + (a.y + dy).toFixed(1) +
        ' C' + (a.x + dx).toFixed(1) + ',' + (a.y + dy).toFixed(1) +
        ' ' + (b.x - dx).toFixed(1) + ',' + (b.y + dy).toFixed(1) +
        ' ' + b.x.toFixed(1) + ',' + (b.y + dy).toFixed(1);
      var color = EDGE_COLOR[e.kind] || '#6b7897';
      paths += '<path class="blocks-edge e-' + esc(e.kind) + '" data-kind="' + esc(e.kind) +
        '" data-source="' + esc(e.source) + '" data-target="' + esc(e.target) +
        '" d="' + d + '" fill="none" stroke="' + color + '" stroke-width="' +
        (e.kind === 'contains' ? 1 : 1.5) + '" marker-end="url(#blocksArrow)"></path>';
    }
    svgEl.innerHTML = defs + paths;
  }

  // ------------------------------------------------------------- hover peek

  function showPeekFor(card) {
    if (peekCard === card) return;
    peekCard = card || null;
    if (!card || !peekEl) { hidePeek(); return; }
  }

  function showPeek(card) {
    peekCard = card;
    var d = card.__decl || {};
    var seq = ++peekSeq;
    peekKindEl.textContent = d.kind || '';
    peekKindEl.style.color = kindColor(d.kind);
    peekNameEl.textContent = d.name || d.id || '';
    peekNameEl.title = d.full || d.id || '';
    peekFileEl.textContent = (d.file || '') + (d.line ? ':' + d.line : '');
    peekSrcEl.textContent = 'Loading source…';
    peekRefsEl.innerHTML = '<div class="bp-ref"><span class="bp-rel">refs</span>' +
      '<span class="bp-relcount">in ' + num(d.inDeg) + ' · out ' + num(d.outDeg) + '</span></div>';
    peekEl.classList.remove('hidden');
    positionPeek(card);

    apiGet('/api/source', { node: d.id, mode: 'raw' }).then(function (s) {
      if (seq !== peekSeq) return;
      var lines = (s && s.lines) || [];
      if (!lines.length) { peekSrcEl.textContent = '(no source lines)'; return; }
      peekSrcEl.textContent = lines.slice(0, PEEK_LINES)
        .map(function (l) { return l.text == null ? '' : l.text; }).join('\n');
      positionPeek(card);
    }).catch(function (err) {
      if (seq !== peekSeq) return;
      peekSrcEl.textContent = 'source unavailable: ' + ((err && err.message) || err);
    });

    apiGet('/api/node', { id: d.id }).then(function (node) {
      if (seq !== peekSeq) return;
      peekRefsEl.innerHTML = refsHtml(node, d);
      positionPeek(card);
    }).catch(function () { /* the degree line above is already shown */ });
  }

  function refsHtml(node, d) {
    var rows = [];
    var rels = (node && node.relations) || [];
    rels.slice(0, PEEK_REFS).forEach(function (r) {
      var names = (r.nodes || []).slice(0, PEEK_REF_NODES).map(function (n) {
        return (n && (n.label || n.name || n.id)) || '';
      }).filter(Boolean);
      rows.push('<div class="bp-ref"><span class="bp-rel">' + esc(r.label || r.kind || r.key) + '</span>' +
        '<span class="bp-relcount">' + num(r.count) + '</span>' +
        (names.length ? '<div class="bp-relnodes">' + esc(names.join(', ')) + '</div>' : '') +
        '</div>');
    });
    if (!rows.length) {
      var callers = (node && node.callers) || [];
      var callees = (node && node.callees) || [];
      if (callers.length) rows.push('<div class="bp-ref"><span class="bp-rel">callers</span>' +
        '<span class="bp-relcount">' + callers.length + '</span></div>');
      if (callees.length) rows.push('<div class="bp-ref"><span class="bp-rel">callees</span>' +
        '<span class="bp-relcount">' + callees.length + '</span></div>');
    }
    if (!rows.length) {
      rows.push('<div class="bp-ref"><span class="bp-rel">no recorded references</span>' +
        '<span class="bp-relcount">' + esc(d.kind || '') + '</span></div>');
    }
    return rows.join('');
  }

  function positionPeek(card) {
    if (!peekEl || !panelEl || !card) return;
    var pr = panelEl.getBoundingClientRect();
    var cr = card.getBoundingClientRect();
    var pw = peekEl.offsetWidth || 340;
    var ph = peekEl.offsetHeight || 200;
    var left = cr.right - pr.left + 10;
    if (left + pw > pr.width - 6) left = Math.max(6, cr.left - pr.left - pw - 10);
    var top = cr.top - pr.top;
    if (top + ph > pr.height - 8) top = Math.max(6, pr.height - ph - 8);
    peekEl.style.left = left + 'px';
    peekEl.style.top = top + 'px';
  }

  function hidePeek() {
    peekSeq++;
    peekCard = null;
    if (peekEl) peekEl.classList.add('hidden');
  }

  // -------------------------------------------------------------- navigate

  function navigateTo(id) {
    if (!id) return;
    if (typeof window.goToNode === 'function') window.goToNode(id);
    else if (typeof window.selectNode === 'function') window.selectNode(id);
    scheduleRefresh();
  }

  function cardUnder(target) {
    if (!target || !target.closest) return null;
    return target.closest('.block-card');
  }

  function wireCards() {
    cardsEl.addEventListener('mouseover', function (ev) {
      var card = cardUnder(ev.target);
      if (!card || card === peekCard) return;
      showPeek(card);
    });
    cardsEl.addEventListener('mouseout', function (ev) {
      var card = cardUnder(ev.target);
      if (!card || card !== peekCard) return;
      var to = ev.relatedTarget;
      if (to && card.contains(to)) return;
      hidePeek();
    });
    cardsEl.addEventListener('click', function (ev) {
      var card = cardUnder(ev.target);
      if (!card) return;
      ev.preventDefault();
      navigateTo(card.getAttribute('data-id'));
    });
  }

  function onKey(ev) {
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    if (ev.key !== 'b' && ev.key !== 'B') return;
    var t = ev.target;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT')) return;
    ev.preventDefault();
    togglePanel();
  }

  // ----------------------------------------------------------------- install

  function install() {
    if (installed) return true;
    panelEl = document.getElementById('blocksPanel');
    headBtnEl = document.getElementById('blocksBtn');
    if (!panelEl || !headBtnEl) return false;

    closeBtnEl = document.getElementById('blocksClose');
    titleEl = document.getElementById('blocksTitle');
    metaEl = document.getElementById('blocksMeta');
    scrollEl = document.getElementById('blocksScroll');
    stageEl = document.getElementById('blocksStage');
    cardsEl = document.getElementById('blocksCards');
    svgEl = document.getElementById('blocksEdges');
    peekEl = document.getElementById('blocksPeek');
    peekKindEl = document.getElementById('blocksPeekKind');
    peekNameEl = document.getElementById('blocksPeekName');
    peekFileEl = document.getElementById('blocksPeekFile');
    peekSrcEl = document.getElementById('blocksPeekSrc');
    peekRefsEl = document.getElementById('blocksPeekRefs');
    if (!cardsEl || !stageEl || !svgEl) return false;

    headBtnEl.addEventListener('click', togglePanel);
    if (closeBtnEl) closeBtnEl.addEventListener('click', closePanel);
    wireCards();
    document.addEventListener('keydown', onKey);

    // Cards are laid out in a responsive grid, so their measured port positions
    // change whenever the panel resizes: redraw the edges, not the blocks.
    if (window.ResizeObserver) {
      resizeObs = new ResizeObserver(scheduleDraw);
      resizeObs.observe(stageEl);
    }
    window.addEventListener('resize', scheduleDraw);

    wrapBus();
    installed = true;
    renderMessage('Select a node — or open a document — to see its blocks.');
    return true;
  }

  // Install once the DOM (and the other deferred modules) are ready. Deferred
  // scripts run with readyState 'interactive', so this runs in order before
  // app.js's init(): state/apiGet/selectNode/goToNode and CSSync all exist.
  function boot() {
    try { install(); } catch (e) { /* never break boot for a panel */ }
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot);
  else boot();

  return {
    install: install,
    open: openPanel,
    close: closePanel,
    toggle: togglePanel,
    isOpen: function () { return opened; },
    refresh: function () { refresh(true); },
    anchor: function () { return currentAnchor; },
    counts: function () {
      return {
        cards: cardsEl ? cardsEl.querySelectorAll('.block-card').length : 0,
        edges: stageEl && stageEl.__edges ? stageEl.__edges.length : 0,
        paths: svgEl ? svgEl.querySelectorAll('path.blocks-edge').length : 0
      };
    },
    peekVisible: function () { return !!(peekEl && !peekEl.classList.contains('hidden')); },
    clickCard: function (i) {
      if (!cardsEl) return false;
      var c = cardsEl.querySelectorAll('.block-card')[i];
      if (!c) return false;
      navigateTo(c.getAttribute('data-id'));
      return true;
    }
  };
})();
