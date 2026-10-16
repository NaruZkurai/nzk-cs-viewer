/* csgraph - pointer, wheel and keyboard input, hover card, selection + navigation.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== interaction

  var drag = { mode: null, nodeId: null, offX: 0, offY: 0, startX: 0, startY: 0, camX: 0, camY: 0,
               moved: false };

  function hitTest(sx, sy) {
    var view = state.view;
    var zoom = state.camera.zoom;
    for (var i = view.nodes.length - 1; i >= 0; i--) {
      var node = view.nodes[i];
      var p = state.pos.get(node.id);
      if (!p) continue;
      var sz = chipSizeFor(node);
      var s = worldToScreen(p.x, p.y);
      var hw = Math.max(6, sz.w * zoom / 2);
      var hh = Math.max(5, sz.h * zoom / 2);
      var dx = sx - s.x, dy = sy - s.y;
      if (dx * dx + dy * dy <= Math.max(hw, hh) * Math.max(hw, hh)) {
        // prefer the rectangle, fall back to the circumscribed circle for tiny chips
        if (Math.abs(dx) <= hw + 2 && Math.abs(dy) <= hh + 2) return node;
      }
    }
    return null;
  }

  // Middle-button drag pans the camera; the left button selects — box-select
  // starting on empty canvas, click to select, drag a node to move it. Shift is
  // kept as a second way to box-select.
  function onPointerDown(ev) {
    var middle = ev.pointerType === 'mouse' && ev.button === 1;
    if (ev.pointerType === 'mouse' && ev.button !== 0 && !middle) return;
    var pt = localPoint(ev);
    var hit = hitTest(pt.x, pt.y);
    if (tooltipPinned && !middle) hideTooltip(true);   // a click on the canvas dismisses the card
    drag.startX = pt.x; drag.startY = pt.y;
    drag.moved = false;
    state.follow = null;                        // any direct interaction releases the camera
    state.autoFit = false;
    try { canvas.setPointerCapture(ev.pointerId); } catch (err) { /* ignore */ }

    if (middle) {
      ev.preventDefault();                      // no middle-click autoscroll
      canvas.classList.add('dragging', 'panning');
      drag.mode = 'pan';
      drag.camX = state.camera.x;
      drag.camY = state.camera.y;
      hideTooltip();
      return;
    }

    // Shift (box-select / highlight), Ctrl+click (add / remove), and a plain
    // left-drag over empty canvas (box-select) all belong to arrange.js.
    if (arrange.pointerDown(ev, pt, hit, { box: !hit })) { drag.mode = null; return; }

    // Alt+click: pin / unpin (Shift is selection, so pinning lives here).
    if (ev.altKey && hit) {
      var hp = posOf(hit.id);
      hp.fixed = !hp.fixed;
      hp.vx = 0; hp.vy = 0;
      selectNode(hit.id);
      drag.mode = null;
      return;
    }

    if (!hit) { drag.mode = null; return; }     // empty canvas is arrange.js's box-select

    canvas.classList.add('dragging');
    drag.mode = 'node';
    drag.nodeId = hit.id;
    var p = posOf(hit.id);
    var w = screenToWorld(pt.x, pt.y);
    drag.offX = w.x - p.x;
    drag.offY = w.y - p.y;
    p.fixed = true;                           // dragging pins the node
    selectNode(hit.id);
    hideTooltip();
    // let the pinned node's neighbours ease around it
    if (!isLayingOut()) startLayout({ reinitialize: false });
  }

  function onPointerMove(ev) {
    var pt = localPoint(ev);

    if (arrange.pointerMove(pt)) return;

    if (drag.mode === 'node') {
      var p = state.pos.get(drag.nodeId);
      if (p) {
        var w = screenToWorld(pt.x, pt.y);
        p.x = p.tx = w.x - drag.offX;
        p.y = p.ty = w.y - drag.offY;
      }
      if (Math.abs(pt.x - drag.startX) + Math.abs(pt.y - drag.startY) > 3) drag.moved = true;
      return;
    }

    if (drag.mode === 'pan') {
      var dx = pt.x - drag.startX, dy = pt.y - drag.startY;
      state.camera.x = drag.camX - dx / state.camera.zoom;
      state.camera.y = drag.camY - dy / state.camera.zoom;
      if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
      hideTooltip();
      return;
    }

    if (ev.shiftKey) canvas.classList.add('shift-ready');
    else canvas.classList.remove('shift-ready');

    var hit = hitTest(pt.x, pt.y);
    var id = hit ? hit.id : null;
    if (id !== state.hovered) {
      state.hovered = id;
      if (id) updateTooltip(pt);
      else scheduleTooltipHide();               // grace period lets the card be reached and clicked
    } else if (id) {
      positionTooltip(pt);
    } else {
      scheduleTooltipHide();
    }
    canvas.classList.toggle('node-hover', !!id);
  }

  function onPointerUp(ev) {
    var owned = arrange.pointerUp();
    var moved = drag.moved;
    drag.mode = null; drag.nodeId = null;
    canvas.classList.remove('dragging');
    canvas.classList.remove('panning');
    try { canvas.releasePointerCapture(ev.pointerId); } catch (err) { /* ignore */ }
    if (owned) return;
    if (moved) scheduleViewerSave();
  }

  function onWheel(ev) {
    ev.preventDefault();
    var pt = localPoint(ev);
    state.follow = null;
    // exponential mapping keeps zoom speed constant in perceived terms
    var factor = Math.exp(-ev.deltaY * (ev.deltaMode === 1 ? 0.05 : 0.0015));
    zoomAt(pt.x, pt.y, factor);
    scheduleViewerSave();
  }

  function onDblClick(ev) {
    var pt = localPoint(ev);
    var hit = hitTest(pt.x, pt.y);
    if (!hit) return;
    selectNode(hit.id);
    followNode(hit.id, Math.max(state.camera.zoom, 0.9));
    expandNode(hit.id);          // VT: double-click pulls in Children + Calling
  }

  // tooltip — the hover card is interactive ("{ } View code", Pin, Expand). Right-click
  // a node to pin the card open in place; a pinned card ignores hover-driven hides and
  // stays until dismissed (Esc, its ✕, or a left-click on empty canvas).

  var tooltipHover = false;          // pointer is over the card itself
  var tooltipHideTimer = 0;          // pending grace-period hide
  var tooltipHoverId = null;         // node the hover card shows (kept while the pointer is on the card)
  var tooltipPinned = false;         // right-click card: locked open at a fixed spot
  var tooltipPinnedId = null;
  var tooltipPinnedPos = { x: 0, y: 0 };

  function tooltipTargetId() { return tooltipPinned ? tooltipPinnedId : tooltipHoverId; }

  function renderTooltip() {
    var id = tooltipTargetId();
    if (!id) { hideTooltip(); return; }
    var n = state.nodeById.get(id);
    if (!n) { hideTooltip(); return; }
    var loc = n.file ? (n.file + (n.line ? ':' + n.line : '')) : '';
    var pinnedNode = !!posOf(id).fixed;
    var actions = [];
    if (n.file) {
      actions.push('<button class="tt-action" data-act="code" data-id="' + esc(id) +
                   '" type="button" title="Open this declaration in the source pane">{ } View code</button>');
    }
    actions.push('<button class="tt-action" data-act="pin" data-id="' + esc(id) +
                 '" type="button" title="' + (pinnedNode ? 'Release this node' : 'Freeze this node in place') +
                 '">' + (pinnedNode ? 'Unpin' : 'Pin node') + '</button>');
    actions.push('<button class="tt-action" data-act="expand" data-id="' + esc(id) +
                 '" type="button" title="Pull in this node\'s children and relations">Expand</button>');
    if (tooltipPinned) {
      actions.push('<button class="tt-action tt-action-close" data-act="close" type="button" title="Close (Esc)">\u2715</button>');
    }
    tooltipEl.innerHTML =
      '<div class="tt-label">' + esc(n.full || n.label || n.name || id) + '</div>' +
      '<div class="tt-kindrow"><span class="tt-kind">' +
        '<i class="tt-dot" style="background:' + kindColor(n.kind) + '"></i>' +
        esc(n.kind || '?') + '</span>' +
      (n.static ? ' <span class="tt-meta">static</span>' : '') +
      (n.access ? ' <span class="tt-meta">' + esc(n.access) + '</span>' : '') +
      (pinnedNode ? ' <span class="tt-badge">pinned</span>' : '') + '</div>' +
      (loc ? '<div class="tt-meta">' + esc(loc) + '</div>' : '') +
      (n.signature ? '<div class="tt-sig">' + esc(n.signature) + '</div>' : '') +
      '<div class="tt-actions">' + actions.join('') + '</div>' +
      (tooltipPinned ? '<div class="tt-hint">Right-click another node to move this card \u00b7 Esc closes it</div>' : '');
    window.clearTimeout(tooltipHideTimer);
    tooltipEl.classList.toggle('pinned', tooltipPinned);
    tooltipEl.classList.remove('hidden');
  }

  function updateTooltip(pt) {
    if (tooltipPinned) return;                 // a pinned card never follows the cursor
    if (!state.hovered || !state.nodeById.has(state.hovered)) { hideTooltip(); return; }
    tooltipHoverId = state.hovered;            // remember it so card actions work after pointerleave
    renderTooltip();
    positionTooltip(pt);
  }

  function pinTooltip(id, pt) {
    if (!state.nodeById.has(id)) return;
    tooltipPinned = true;
    tooltipPinnedId = id;
    tooltipPinnedPos.x = pt.x + 10;
    tooltipPinnedPos.y = pt.y + 10;
    renderTooltip();
    positionTooltip(pt);
  }

  function positionTooltip(pt) {
    if (tooltipEl.classList.contains('hidden')) return;
    var pad = 14;
    var w = tooltipEl.offsetWidth, h = tooltipEl.offsetHeight;
    var x, y;
    if (tooltipPinned) {
      x = clamp(tooltipPinnedPos.x, 6, Math.max(6, state.w - w - 6));
      y = clamp(tooltipPinnedPos.y, 6, Math.max(6, state.h - h - 6));
    } else {
      x = pt.x + pad; y = pt.y + pad;
      if (x + w > state.w - 6) x = Math.max(6, pt.x - w - pad);
      if (y + h > state.h - 6) y = Math.max(6, pt.y - h - pad);
    }
    tooltipEl.style.left = x + 'px';
    tooltipEl.style.top = y + 'px';
  }

  /** Hide the hover card. A pinned card is only removed by `hideTooltip(true)`. */
  function hideTooltip(force) {
    if (tooltipPinned && !force) return;
    window.clearTimeout(tooltipHideTimer);
    if (force) { tooltipPinned = false; tooltipPinnedId = null; }
    tooltipHoverId = null;
    tooltipEl.classList.add('hidden');
    tooltipEl.classList.remove('pinned');
  }

  // Leaving the node starts a short grace timer instead of hiding at once, so the
  // pointer can travel the few pixels onto the card and click its actions.
  function scheduleTooltipHide() {
    if (tooltipPinned) return;
    window.clearTimeout(tooltipHideTimer);
    tooltipHideTimer = window.setTimeout(function () {
      if (!tooltipHover) hideTooltip();
    }, 220);
  }

  /** Toggle the physics pin on a node (same effect as Shift+click). */
  function toggleNodePin(id) {
    if (!id || !state.nodeById.has(id)) return;
    var p = posOf(id);
    p.fixed = !p.fixed;
    p.vx = 0; p.vy = 0;
    if (!tooltipPinned) tooltipHoverId = id;    // the card is the click target, not the canvas
    if (!tooltipEl.classList.contains('hidden')) renderTooltip();
    toast((p.fixed ? 'Pinned ' : 'Unpinned ') + crumbLabel(id), p.fixed ? '' : 'warn');
    if (!isLayingOut()) startLayout({ reinitialize: false });
  }

  function onTooltipClick(ev) {
    var btn = ev.target.closest ? ev.target.closest('.tt-action') : null;
    if (!btn) return;
    ev.preventDefault();
    ev.stopPropagation();
    var act = btn.getAttribute('data-act');
    var id = btn.getAttribute('data-id') || tooltipTargetId();
    if (act === 'code') viewCodeFor(id);
    else if (act === 'pin') toggleNodePin(id);
    else if (act === 'expand') expandNode(id, { all: true });
    else if (act === 'close') hideTooltip(true);
  }

  /** Right-click a node: select it and pin its action card open in place. */
  function onContextMenu(ev) {
    var pt = localPoint(ev);
    var hit = hitTest(pt.x, pt.y);
    if (!hit) {
      if (tooltipPinned) { ev.preventDefault(); hideTooltip(true); }
      return;                                   // empty canvas keeps the browser menu
    }
    ev.preventDefault();
    selectNode(hit.id);
    pinTooltip(hit.id, pt);
  }

  // selection

  function selectNode(id, opts) {
    opts = opts || {};
    if (!id || !state.nodeById.has(id)) return;
    if (!opts.noHistory && state.selected && state.selected !== id) {
      state.history.push(state.selected);
      if (state.history.length > 80) state.history.shift();
    }
    state.selected = id;
    if (!opts.keepMulti) arrange.set([id], { silent: true });   // collapse the box-selection
    loadDetail(id);
    if (!modeIsFind()) renderNodeDrawer();
    if (window.CSSync) window.CSSync.push();
  }

  function modeIsFind() { return state.mode === 'find'; }

  /** Back: return to the previously selected node and re-expand it. */
  function navigateBack() {
    var id = null;
    while (state.history.length && !id) {
      var cand = state.history.pop();
      if (cand && state.nodeById.has(cand)) id = cand;
    }
    if (!id) { toast('Nothing to go back to', 'warn'); return; }
    selectNode(id, { noHistory: true });
    expandNode(id);
    followNode(id, null);
  }

  /** Select any node id, pulling it into the graph first when necessary. */
  function goToNode(id) {
    if (!id) return;
    if (state.nodeById.has(id)) {
      selectNode(id);
      expandNode(id);
      followNode(id, null);
      return;
    }
    apiGet('/api/node', { id: id }).then(function (d) {
      if (!d || !d.node) return;
      var ids = addNodes([d.node], 'base');
      buildModel();
      seedNewNodes(ids);
      quickLayout();
      selectNode(id);
      expandNode(id);
      followNode(id, null);
    }).catch(function (err) {
      toast('Could not open ' + id + ': ' + err.message, 'error');
    });
  }

  /** Up: walk to the containment parent (namespace / declaring type). */
  function navigateUp() {
    var n = (state.detail && state.detail.node) || state.nodeById.get(state.selected);
    if (!n) { toast('Select a node first', 'warn'); return; }
    var parent = n.parent || '';
    if (!parent) { toast((n.label || n.id) + ' has no parent in this index', 'warn'); return; }
    goToNode(parent);
  }

  function crumbLabel(id) {
    var n = state.nodeById.get(id);
    if (n) return n.label || n.name || id;
    var bare = String(id).indexOf('#ns:') === 0 ? String(id).slice(4) : String(id);
    var parts = bare.split('.');
    return parts[parts.length - 1] || bare;
  }

  /** Breadcrumb: walk `parent` upward (max 20 hops) and render clickable crumbs.
   *  The selected node itself is omitted — the drawer header already names it. */
  function renderCrumbs() {
    if (!nodeCrumbsEl) return;
    nodeCrumbsEl.innerHTML = '';
    var d = state.detail;
    if (!d || !d.node) return;
    var ids = [];
    var id = d.node.id;
    var guard = 0;
    while (id && guard++ < 20) {
      ids.unshift(id);
      var n = (d.node.id === id) ? d.node : state.nodeById.get(id);
      var parent = n ? n.parent : '';
      if (!parent) break;
      if (state.nodeById.has(parent)) { id = parent; continue; }
      ids.unshift(parent);
      break;
    }
    ids.pop();                       // drop the selected node: it is the header title
    ids.forEach(function (cid, i) {
      if (i) {
        var sep = document.createElement('span');
        sep.className = 'crumb-sep';
        sep.textContent = '›';
        nodeCrumbsEl.appendChild(sep);
      }
      var b = document.createElement('button');
      b.type = 'button';
      b.className = 'crumb';
      b.textContent = crumbLabel(cid);
      b.title = cid;
      b.addEventListener('click', function () { goToNode(cid); });
      nodeCrumbsEl.appendChild(b);
    });
  }

  /** Drop the selection (and the drawer with it); arrange.js owns the set itself. */
  function clearSelection() {
    arrange.clear();
  }

  /**
   * Adopt `id` as the centre of the loaded neighbourhood: fetch /api/subgraph at the
   * current depth, re-run the force pass over it, select it and fly the camera to it.
   * Used by search results and by the detail-panel relation lists.
   */
  function centerNode(id) {
    if (!id) return;
    var seq = ++graphSeq;
    apiGet('/api/subgraph', { id: id, depth: state.depth })
      .then(function (data) {
        if (seq !== graphSeq) return;
        setGraph(data, { selectFocus: true, follow: true });
        loadDetail(id);
      })
      .catch(function (err) {
        if (seq !== graphSeq) return;
        toast('Could not load neighbourhood: ' + err.message, 'error');
      });
  }

  /** Focused neighbourhood (used by search / relation clicks). */
  function loadWholeGraph(withOverlay) { return loadFullGraph(withOverlay); }

