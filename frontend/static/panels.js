/* csgraph v2 — Blender-style panel management.
 *
 * Part of the RUST-ONLY v2 viewer. Not loaded by v1.
 *
 * Every pane that renders content can be resized, undocked into a floating
 * window, dragged around, and redocked:
 *
 *   resize    a splitter between the canvas and the sidebar, a splitter above
 *             the source pane, and a corner grip on the overlays;
 *   undock    drag a pane's header (or press its float button) and it lifts
 *             into a floating window over the app;
 *   redock    drag a floating pane over its dock zone (highlighted while you
 *             drag), or press its dock button, and it returns to its slot.
 *
 * The graph canvas itself stays docked: it owns the shared `state` and the
 * `<canvas>`, so it cannot be transplanted. Everything around it can float.
 *
 * Layout (sizes, floating geometry, docked/floating state) is stored in
 * localStorage under `csgraph.panels.v1` and restored on boot.
 */
'use strict';

window.CSPanels = (function () {
  var STORE_KEY = 'csgraph.panels.v1';
  var MIN_SIDE = 220, MIN_SOURCE = 120, MIN_PANE = 160;
  // The canvas keeps at least this much width no matter how wide the sidebar
  // was dragged or restored: the grid's first track is `minmax(0, 1fr)`, so an
  // unclamped `--side-w` collapses it to 0 and pushes the sidebar off-screen.
  var MIN_CANVAS = 260;

  // id, element, and how the pane docks. `slot` is 'col' (grid column 2),
  // 'row' (grid row 2) or 'abs' (absolutely positioned inside canvasWrap).
  var PANES = [
    { id: 'side', el: '.side', slot: 'col', title: 'Sidebar', grip: true },
    { id: 'source', el: '#sourcePane', slot: 'row', title: 'Source' },
    { id: 'nodeDrawer', el: '#nodeDrawer', slot: 'abs', title: 'Node' },
    { id: 'findPanel', el: '#findPanel', slot: 'abs', title: 'Find' }
  ];

  var panes = [];
  var layoutEl = null, floatLayer = null, hintEl = null;
  var saved = {};
  var splitterV = null, splitterH = null;
  var active = null;                    // live drag: {kind, ...}
  var rafPending = false;
  /**
   * Unity-style focus order: the pane you last touched is the pane on top.
   * Floating panes live in `#floatLayer`, which is already above every docked
   * pane, but they also need an order among themselves.
   */
  var zTop = 1;

  // ------------------------------------------------------------------ store

  function load() {
    try { return JSON.parse(window.localStorage.getItem(STORE_KEY)) || {}; }
    catch (e) { return {}; }
  }
  function save() {
    try { window.localStorage.setItem(STORE_KEY, JSON.stringify(saved)); } catch (e) { /* ignore */ }
  }

  // ------------------------------------------------------------------ helpers

  function clamp(v, lo, hi) { return v < lo ? lo : (v > hi ? hi : v); }
  function el(sel) { return typeof sel === 'string' ? document.querySelector(sel) : sel; }
  function rectOf(node) { return node.getBoundingClientRect(); }

  function isShown(node) {
    if (!node || node.classList.contains('hidden')) return false;
    var r = node.getBoundingClientRect();
    return r.width > 1 && r.height > 1;
  }

  function paneById(id) { return panes.filter(function (p) { return p.id === id; })[0]; }
  /** Floating in the in-page layer, or detached into its own OS window. */
  function isFloating(id) { var p = paneById(id); return !!(p && (p.floating || p.detached)); }
  function isNarrow() { return window.matchMedia('(max-width: 980px)').matches; }

  /**
   * The sidebar width the user asked for, before the viewport clamp below. It
   * survives a shrink-and-grow cycle: the *applied* width is clamped, this
   * desire is not, so re-widening the window restores the width that was
   * dragged, not the smaller one the clamp forced.
   */
  var desiredSideW = null;

  /**
   * Apply `--side-w`, clamped so the sidebar stays inside the viewport and the
   * canvas keeps MIN_CANVAS. Runs on every relayout (window resize, splitter
   * drag, pane visibility), which is what keeps a saved or dragged width from
   * crushing the canvas when the window later gets smaller. Below 980px the
   * grid is one column and `--side-w` no longer sizes the sidebar; the clamp is
   * harmless there.
   */
  function applySideWidth() {
    if (!layoutEl) return 0;
    if (desiredSideW == null) {
      desiredSideW = saved.sideW ||
        parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--side-w')) || 360;
    }
    var w = clamp(desiredSideW, MIN_SIDE, Math.max(MIN_SIDE, rectOf(layoutEl).width - MIN_CANVAS));
    document.documentElement.style.setProperty('--side-w', w + 'px');
    return w;
  }

  /** True for the main sidebar while it is docked into the layout grid. */
  function isDockedSide(p) {
    return !!(p && p.id === 'side' && p.slot === 'col' && !p.floating && !p.detached);
  }

  /** Raise a floating pane above the other floating panes. */
  function bringToFront(p) {
    if (!p || !p.floating) return;
    zTop += 1;
    p.node.style.zIndex = String(zTop);
  }

  // ------------------------------------------------------------------ splitters

  function makeSplitter(axis) {
    var s = document.createElement('div');
    s.className = 'pane-splitter pane-splitter-' + (axis === 'v' ? 'v' : 'h');
    s.setAttribute('role', 'separator');
    s.setAttribute('aria-orientation', axis === 'v' ? 'vertical' : 'horizontal');
    s.addEventListener('pointerdown', function (ev) { startSplitter(ev, axis); });
    return s;
  }

  function startSplitter(ev, axis) {
    if (ev.button !== 0) return;
    ev.preventDefault();
    var cs = getComputedStyle(document.documentElement);
    active = {
      kind: axis === 'v' ? 'side' : 'source',
      layout: rectOf(layoutEl),
      startX: ev.clientX, startY: ev.clientY,
      sideW: parseFloat(cs.getPropertyValue('--side-w')) || 360,
      sourceH: parseFloat(cs.getPropertyValue('--source-h')) || 300
    };
    document.documentElement.classList.add('panel-resizing');
    document.documentElement.dataset.panelCursor = axis === 'v' ? 'col-resize' : 'row-resize';
    window.addEventListener('pointermove', onSplitterMove);
    window.addEventListener('pointerup', endDrag);
  }

  function onSplitterMove(ev) {
    if (!active) return;
    if (active.kind === 'side') {
      var w = clamp(active.sideW - (ev.clientX - active.startX), MIN_SIDE,
                    Math.max(MIN_SIDE, active.layout.width - MIN_CANVAS));
      document.documentElement.style.setProperty('--side-w', w + 'px');
      saved.sideW = w;
      desiredSideW = w;
    } else if (active.kind === 'source') {
      var h = clamp(active.sourceH - (ev.clientY - active.startY), MIN_SOURCE,
                    Math.max(MIN_SOURCE, active.layout.height - 160));
      document.documentElement.style.setProperty('--source-h', h + 'px');
      if (layoutEl.classList.contains('with-source')) {
        layoutEl.style.gridTemplateRows = 'minmax(0, 1fr) minmax(' + MIN_SOURCE + 'px, ' + h + 'px)';
      }
      saved.sourceH = h;
    }
    positionSplitters();
    relayout();
  }

  /** Keep both splitters on the current pane boundaries (layout-relative). */
  function positionSplitters() {
    if (!layoutEl) return;
    // Re-clamp first: a resize may have made the applied sidebar wider than the
    // viewport / narrower than the canvas, and the splitter must sit on the
    // boundary the clamp produced.
    applySideWidth();
    var layout = rectOf(layoutEl);
    var canvas = document.querySelector('#canvasWrap');
    var side = document.querySelector('.side');
    var source = document.querySelector('#sourcePane');

    if (splitterV) {
      var showV = !isNarrow() && isShown(side) && isShown(canvas) && !isFloating('side');
      splitterV.style.display = showV ? '' : 'none';
      if (showV) {
        var cr = rectOf(canvas);
        splitterV.style.left = (cr.right - layout.left - 3) + 'px';
        splitterV.style.top = '0px';
        splitterV.style.height = layout.height + 'px';
      }
    }
    if (splitterH) {
      var showH = layoutEl.classList.contains('with-source') && isShown(source) && !isFloating('source');
      splitterH.style.display = showH ? '' : 'none';
      if (showH) {
        var sr = rectOf(source);
        splitterH.style.left = '0px';
        splitterH.style.width = layout.width + 'px';
        splitterH.style.top = (sr.top - layout.top - 3) + 'px';
      }
    }
  }

  function schedulePosition() {
    if (rafPending) return;
    rafPending = true;
    window.requestAnimationFrame(function () { rafPending = false; positionSplitters(); });
  }

  // ------------------------------------------------------------------ pane drag

  function startPaneDrag(ev, p) {
    if (ev.button !== 0) return;
    if (ev.target.closest && ev.target.closest('button, input, select, a, .pane-resize')) return;
    ev.preventDefault();
    bringToFront(p);
    var r = rectOf(p.node);
    active = {
      kind: 'pane', pane: p,
      startX: ev.clientX, startY: ev.clientY,
      nodeX: r.left, nodeY: r.top,
      lastX: ev.clientX, lastY: ev.clientY
    };
    p.node.classList.add('dragging');
    document.documentElement.classList.add('panel-dragging');
    document.documentElement.dataset.panelCursor = 'move';
    window.addEventListener('pointermove', onPaneMove);
    window.addEventListener('pointerup', endDrag);
  }

  function onPaneMove(ev) {
    if (!active || active.kind !== 'pane') return;
    var p = active.pane;
    active.lastX = ev.clientX; active.lastY = ev.clientY;
    var dx = ev.clientX - active.startX, dy = ev.clientY - active.startY;
    if (!p.floating && !p.detached) {
      if (Math.abs(dx) + Math.abs(dy) < 6) return;    // let a click be a click
      // Dragging a pane header out of the main window is the gesture that pulls
      // it out: into its own native window when we can (Unity), otherwise into
      // the in-page float layer.
      if (detachNative(p)) { endDrag(); return; }
      undock(p, { x: active.nodeX, y: active.nodeY, w: p.node.offsetWidth, h: p.node.offsetHeight });
      // Re-anchor so the pane does not jump on the first move.
      active.startX = ev.clientX; active.startY = ev.clientY;
      active.nodeX = p.pos.x; active.nodeY = p.pos.y;
      dx = 0; dy = 0;
    }
    if (p.detached) return;
    var w = p.node.offsetWidth, h = p.node.offsetHeight;
    p.pos = {
      x: clamp(active.nodeX + dx, -w + 80, window.innerWidth - 80),
      y: clamp(active.nodeY + dy, 0, window.innerHeight - 34),
      w: w, h: h
    };
    applyFloatingGeometry(p);
    showDockHint(ev.clientX, ev.clientY);
  }

  function applyFloatingGeometry(p) {
    if (!p.pos) return;
    p.node.style.left = p.pos.x + 'px';
    p.node.style.top = p.pos.y + 'px';
    if (p.pos.w) p.node.style.width = p.pos.w + 'px';
    if (p.pos.h) p.node.style.height = p.pos.h + 'px';
  }

  // ------------------------------------------------------------------ pane resize

  function startPaneResize(ev, p) {
    if (ev.button !== 0) return;
    ev.preventDefault();
    ev.stopPropagation();
    bringToFront(p);
    var r = rectOf(p.node);
    active = { kind: 'resize', pane: p, startX: ev.clientX, startY: ev.clientY,
               w: r.width, h: r.height };
    document.documentElement.classList.add('panel-resizing');
    document.documentElement.dataset.panelCursor = 'nwse-resize';
    window.addEventListener('pointermove', onPaneResizeMove);
    window.addEventListener('pointerup', endDrag);
  }

  function onPaneResizeMove(ev) {
    if (!active || active.kind !== 'resize') return;
    var p = active.pane;
    if (p.floating) {
      var fw = Math.max(MIN_PANE, active.w + (ev.clientX - active.startX));
      var fh = Math.max(80, active.h + (ev.clientY - active.startY));
      p.pos = p.pos || { x: rectOf(p.node).left, y: rectOf(p.node).top, w: fw, h: fh };
      p.pos.w = fw; p.pos.h = fh;
      applyFloatingGeometry(p);
    } else if (isDockedSide(p)) {
      // A docked sidebar is sized by the layout's grid column, not by an inline
      // width on the item: route its corner grip through the same clamped
      // `--side-w` the splitter writes, or the sidebar drifts off its column.
      var sw = clamp(active.w + (ev.clientX - active.startX), MIN_SIDE,
                     Math.max(MIN_SIDE, rectOf(layoutEl).width - MIN_CANVAS));
      desiredSideW = sw;
      saved.sideW = sw;
      document.documentElement.style.setProperty('--side-w', sw + 'px');
    } else {
      var w = Math.max(MIN_PANE, active.w + (ev.clientX - active.startX));
      var h = Math.max(80, active.h + (ev.clientY - active.startY));
      p.node.style.width = w + 'px';
      p.node.style.height = h + 'px';
    }
    relayout();
  }

  // ------------------------------------------------------------------ drop handling

  function endDrag() {
    window.removeEventListener('pointermove', onSplitterMove);
    window.removeEventListener('pointermove', onPaneMove);
    window.removeEventListener('pointermove', onPaneResizeMove);
    window.removeEventListener('pointerup', endDrag);

    var a = active;
    active = null;
    if (a && a.kind === 'pane') {
      var p = a.pane;
      p.node.classList.remove('dragging');
      if (p.floating) {
        var zone = dockZoneAt(a.lastX, a.lastY, p);
        if (zone && dockAffinity(p, zone)) dock(p);
        else rememberFloating(p);
      }
    } else if (a && a.kind === 'resize') {
      var q = a.pane;
      if (q.floating) rememberFloating(q);
      else if (isDockedSide(q)) {
        // Its width already lives in saved.sideW; an inline size on the grid
        // item would stop following the viewport (see applySaved).
        delete saved['size-' + q.id];
      } else saved['size-' + q.id] = { w: q.node.offsetWidth, h: q.node.offsetHeight };
      save();
    } else if (a && (a.kind === 'side' || a.kind === 'source')) {
      save();
    }
    hideDockHint();
    document.documentElement.classList.remove('panel-dragging', 'panel-resizing');
    delete document.documentElement.dataset.panelCursor;
    relayout();
  }

  function dockZoneAt(clientX, clientY, p) {
    if (!p) return null;
    var lr = rectOf(layoutEl);
    if (p.slot === 'row' && clientY > lr.bottom - lr.height * 0.32) return 'source';
    if (p.slot === 'col' && clientX > lr.right - lr.width * 0.22) return 'side';
    if (p.slot === 'abs') {
      var canvas = document.querySelector('#canvasWrap');
      if (!canvas) return null;
      var cr = rectOf(canvas);
      if (clientX >= cr.left && clientX <= cr.right && clientY >= cr.top && clientY <= cr.bottom) {
        return p.id;
      }
    }
    return null;
  }

  function dockAffinity(p, zone) {
    if (zone === p.id) return true;
    if (zone === 'source' && p.slot === 'row') return true;
    if (zone === 'side' && p.slot === 'col') return true;
    return false;
  }

  function dockTargetEl(zone, p) {
    if (zone === 'source') return document.querySelector('#sourcePane');
    if (zone === 'side') return document.querySelector('.side');
    return document.querySelector('#canvasWrap');
  }

  function showDockHint(x, y) {
    if (!hintEl || !active || active.kind !== 'pane') return;
    var zone = dockZoneAt(x, y, active.pane);
    if (!zone) { hideDockHint(); return; }
    var target = dockTargetEl(zone, active.pane);
    if (!target) { hideDockHint(); return; }
    var r = rectOf(target);
    hintEl.style.display = 'block';
    hintEl.style.left = r.left + 'px';
    hintEl.style.top = r.top + 'px';
    hintEl.style.width = r.width + 'px';
    hintEl.style.height = r.height + 'px';
  }

  function hideDockHint() { if (hintEl) hintEl.style.display = 'none'; }

  // ------------------------------------------------------------------ dock / undock

  function undock(p, geometry) {
    if (p.floating || p.detached) return;
    // Native build: the pane becomes a real operating-system window of its own,
    // which is the only way it can render outside the main window. The in-page
    // float layer below is the plain-web fallback (a browser window cannot keep
    // this pane's live handlers).
    if (detachNative(p)) return;
    p.floating = true;
    p.node.classList.add('pane', 'floating');
    if (p.slotEl) p.slotEl.style.display = 'block';
    floatLayer.appendChild(p.node);
    var w = p.node.offsetWidth || 420, h = p.node.offsetHeight || 320;
    p.pos = geometry || {
      x: Math.max(20, Math.round((window.innerWidth - w) / 2)),
      y: Math.max(20, Math.round((window.innerHeight - h) / 2)),
      w: w, h: h
    };
    if (!p.pos.w) p.pos.w = w;
    if (!p.pos.h) p.pos.h = h;
    applyFloatingGeometry(p);
    bringToFront(p);
    schedulePosition();
  }

  // ------------------------------------------------------- native pane windows

  /** True when this build can open real operating-system windows (Tauri host). */
  function canDetach() {
    return !!(window.CsWindows && window.CsWindows.native && window.CsWindows.native());
  }

  /**
   * Detach `p` into its own window. Returns false when that is not possible, so
   * the caller can fall back to the in-page float layer.
   *
   * The pane element itself stays in the main window (hidden, with a "Bring
   * back" placeholder in its slot): the app's render code keeps writing to it,
   * and the detached window is a second live instance of the same pane —
   * a Unity-style floating panel, not a snapshot.
   */
  function detachNative(p) {
    if (!canDetach() || p.detached) return false;
    p.detached = true;
    p.node.classList.add('pane-detached');
    document.documentElement.classList.add('pane-detached-' + p.id);
    if (p.slotEl) p.slotEl.style.display = 'block';
    showDetachedSlot(p);
    var view = window.CSSync ? window.CSSync.view() : null;
    window.CsWindows.openPane(p.id, view).then(function (label) {
      if (!label) restoreNative(p);          // the window could not be created
    });
    schedulePosition();
    return true;
  }

  /** The "this pane lives in its own window" placeholder, with a way back. */
  function showDetachedSlot(p) {
    if (!p.detachEl) {
      var d = document.createElement('div');
      d.className = 'pane-detached-slot';
      var t = document.createElement('span');
      t.className = 'pds-title';
      var n = document.createElement('span');
      n.className = 'pds-note';
      n.textContent = 'is in its own window';
      var b = document.createElement('button');
      b.type = 'button';
      b.className = 'btn btn-ghost pds-btn';
      b.textContent = 'Bring back';
      b.title = 'Close the pane window and dock this pane again';
      b.addEventListener('click', function () {
        if (window.CsWindows) window.CsWindows.closePane(p.id);
        else restoreNative(p);
      });
      d.appendChild(t); d.appendChild(n); d.appendChild(b);
      p.detachEl = d;
    }
    p.detachEl.querySelector('.pds-title').textContent = p.title || p.id;
    var host = p.slotEl || p.dockTarget || document.querySelector('#canvasWrap');
    if (host && p.detachEl.parentNode !== host) host.appendChild(p.detachEl);
  }

  /** Dock a pane back after its window went away. */
  function restoreNative(p) {
    if (!p.detached) return;
    p.detached = false;
    p.node.classList.remove('pane-detached');
    document.documentElement.classList.remove('pane-detached-' + p.id);
    if (p.detachEl && p.detachEl.parentNode) p.detachEl.parentNode.removeChild(p.detachEl);
    if (p.slotEl && !p.floating) p.slotEl.style.display = 'none';
    schedulePosition();
  }

  function dock(p) {
    if (p.detached) {
      if (window.CsWindows) window.CsWindows.closePane(p.id);
      else restoreNative(p);
      return;
    }
    if (!p.floating) return;
    p.floating = false;
    p.node.classList.remove('floating', 'dragging');
    p.node.style.left = p.node.style.top = p.node.style.width = p.node.style.height = '';
    p.node.style.zIndex = '';
    if (p.slotEl) {
      p.slotEl.style.display = 'none';
      p.slotEl.parentNode.insertBefore(p.node, p.slotEl.nextSibling);
    } else if (p.homeParent) {
      p.homeParent.appendChild(p.node);
    }
    delete saved['float-' + p.id];
    delete saved['pos-' + p.id];
    save();
    schedulePosition();
  }

  function rememberFloating(p) {
    saved['float-' + p.id] = true;
    saved['pos-' + p.id] = p.pos;
    save();
  }

  function toggleFloat(p) {
    if (p.detached) { dock(p); return; }
    if (p.floating) dock(p); else undock(p);
  }

  // ------------------------------------------------------------------ relayout

  /** The canvas must re-measure whenever a pane changes size. */
  function relayout() {
    if (typeof window.resize === 'function') window.resize();
    else window.dispatchEvent(new Event('resize'));
  }

  // ------------------------------------------------------------------ chrome

  function addChrome(p, cfg) {
    var header = p.node.querySelector('.drawer-head, .source-head');
    if (!header) {
      header = document.createElement('div');
      header.className = 'pane-grip';
      var t = document.createElement('span');
      t.className = 'pane-grip-title';
      t.textContent = cfg.title || p.id;
      header.appendChild(t);
      p.node.insertBefore(header, p.node.firstChild);
    }
    header.classList.add('pane-drag-head');

    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'btn btn-ghost pane-float-btn';
    btn.title = canDetach()
      ? 'Detach this pane into its own window / dock it back'
      : 'Undock this pane into a floating window / dock it back';
    btn.textContent = '\u2922';                    // ⤢
    btn.addEventListener('click', function (ev) { ev.stopPropagation(); toggleFloat(p); });
    header.appendChild(btn);
    p.header = header;
    p.floatBtn = btn;

    header.addEventListener('pointerdown', function (ev) { startPaneDrag(ev, p); });

    if (cfg.slot === 'abs' || cfg.grip) {
      var rz = document.createElement('div');
      rz.className = 'pane-resize';
      rz.title = 'Resize';
      rz.addEventListener('pointerdown', function (ev) { startPaneResize(ev, p); });
      p.node.appendChild(rz);
    }
  }

  function installPane(cfg) {
    var node = el(cfg.el);
    if (!node) return null;
    var p = {
      id: cfg.id, node: node, floating: false, detached: false,
      title: cfg.title || cfg.id,
      slot: cfg.slot, homeParent: node.parentNode
    };
    if (cfg.slot === 'abs') {
      p.dockTarget = document.querySelector('#canvasWrap');
    } else {
      var slotEl = document.createElement('div');
      slotEl.className = 'pane-slot pane-slot-' + cfg.slot;
      slotEl.style.display = 'none';
      node.parentNode.insertBefore(slotEl, node);
      p.slotEl = slotEl;
      p.dockTarget = slotEl;
    }
    panes.push(p);
    addChrome(p, cfg);
    // Any interaction with a floating pane makes it the top one (Unity-like).
    p.node.addEventListener('pointerdown', function () { bringToFront(p); }, true);
    return p;
  }

  function applySaved() {
    // Restore the requested width clamped to *this* viewport, not the one the
    // width was last dragged on.
    applySideWidth();
    if (saved.sourceH) {
      document.documentElement.style.setProperty('--source-h', saved.sourceH + 'px');
      if (layoutEl.classList.contains('with-source')) {
        layoutEl.style.gridTemplateRows =
          'minmax(0, 1fr) minmax(' + MIN_SOURCE + 'px, ' + saved.sourceH + 'px)';
      }
    }
    panes.forEach(function (p) {
      var sz = saved['size-' + p.id];
      if (isDockedSide(p)) {
        // The docked sidebar follows --side-w; a legacy inline size would pin it
        // to the viewport it was last dragged on. Drop both the style and the
        // stored value.
        delete saved['size-' + p.id];
        p.node.style.width = '';
        p.node.style.height = '';
      } else if (sz && !p.floating) {
        p.node.style.width = sz.w + 'px';
        p.node.style.height = sz.h + 'px';
      }
      if (saved['float-' + p.id]) {
        // A pane that was left floating in the page belongs in its own window now
        // that the host can make one; do not surprise the user with a window at
        // startup, so the flag is cleared and the pane docks back.
        if (canDetach()) {
          delete saved['float-' + p.id];
          delete saved['pos-' + p.id];
        } else {
          undock(p, saved['pos-' + p.id] || null);
        }
      }
    });
    save();
  }

  /** Watch the panes' class changes so the splitters follow open/close/float. */
  function watchVisibility() {
    if (!window.MutationObserver) return;
    var obs = new MutationObserver(schedulePosition);
    [layoutEl, document.querySelector('.side'), document.querySelector('#sourcePane'),
     document.querySelector('#nodeDrawer'), document.querySelector('#findPanel')].forEach(function (n) {
      if (n) obs.observe(n, { attributes: true, attributeFilter: ['class', 'style'] });
    });
  }

  function install() {
    if (window.location.search.indexOf('win=doc') !== -1) return;   // doc window: full-window source
    if (window.location.search.indexOf('win=pane') !== -1) return;  // pane window: one pane only
    if (window.CsPaneWin && window.CsPaneWin.active()) return;      // injected pane target
    if (document.getElementById('floatLayer')) return;
    layoutEl = document.querySelector('.layout');
    if (!layoutEl) return;

    saved = load();

    hintEl = document.createElement('div');
    hintEl.className = 'dock-hint';
    document.body.appendChild(hintEl);
    floatLayer = document.createElement('div');
    floatLayer.id = 'floatLayer';
    document.body.appendChild(floatLayer);

    PANES.forEach(installPane);

    splitterV = makeSplitter('v');
    splitterH = makeSplitter('h');
    layoutEl.appendChild(splitterV);
    layoutEl.appendChild(splitterH);

    applySaved();
    positionSplitters();
    watchVisibility();

    // A detached pane's window went away (closed by the user, or the WM): dock
    // the pane back where it was.
    if (window.CsWindows) {
      window.CsWindows.on('cs:win-closed', function (msg) {
        if (!msg || msg.kind !== 'pane') return;
        var p = paneById(msg.pane);
        if (p) restoreNative(p);
      });
    }

    window.addEventListener('resize', schedulePosition);
    if (window.ResizeObserver) new ResizeObserver(schedulePosition).observe(layoutEl);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', install);
  } else {
    install();
  }

  return {
    install: install,
    positionSplitters: positionSplitters,
    float: function (id) { var p = paneById(id); if (p) toggleFloat(p); },
    isFloating: isFloating,
    dockAll: function () { panes.forEach(function (p) { dock(p); }); }
  };
})();
