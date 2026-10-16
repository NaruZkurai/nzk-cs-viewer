/* csgraph — box-selection and node arranging.
 *
 * Split out of app.js so the canvas file stays about the graph and camera. This
 * module owns the *set* of selected nodes and the commands that lay that set out:
 *
 *   selection   drag on empty canvas (or Shift+drag) box-select, Shift+click
 *               highlight, Ctrl/Cmd+click add; the middle button pans the camera
 *   arranging   Ctrl+Shift+V | H | D lay the set out along an axis in step order;
 *               pressing the matching command again flips it; Shift+= / Shift+-
 *               widen or tighten the spacing along the axis already in use
 *
 * "step" is the depth-first position in the `contains` hierarchy (parent -> child,
 * siblings in declaration order), so arranging a subtree puts every child of a node
 * directly beneath it, left edges flush.
 *
 * Loaded before app.js. app.js builds one instance with the collaborators below and
 * calls into it from the pointer handlers, the key handler, the render loop and the
 * shortcuts panel; everything else stays private here.
 */
(function () {
  'use strict';

  var ARRANGE_GAP_DEFAULT = 16, ARRANGE_GAP_MIN = 2, ARRANGE_GAP_MAX = 400;

  function clamp(v, lo, hi) { return v < lo ? lo : (v > hi ? hi : v); }

  /**
   * ctx = {
   *   state, canvas, nodeDrawerEl, toast,
   *   posOf, chipSizeFor, worldToScreen,   // geometry
   *   selectNode, loadDetail, renderNodeDrawer, modeIsFind,   // primary selection
   *   scheduleViewerSave
   * }
   */
  window.CSArrange = function createArrange(ctx) {
    var state = ctx.state;
    var canvas = ctx.canvas;

    var selection = new Set();     // selected node ids (the drawer's primary is state.selected)
    var stepIndex = null;          // cached Map(node id -> depth-first index)
    var gap = ARRANGE_GAP_DEFAULT;
    var marquee = null;            // live box-select gesture

    // ------------------------------------------------------------------ selection

    /** The selection, falling back to the drawer's primary when nothing is box-selected. */
    function ids() {
      var out = [];
      selection.forEach(function (id) { if (state.nodeById.has(id)) out.push(id); });
      if (!out.length && state.selected && state.nodeById.has(state.selected)) out.push(state.selected);
      return out;
    }

    function applySelection(next, opts) {
      opts = opts || {};
      var keep = new Set();
      (next || []).forEach(function (id) {
        if (id && !keep.has(id) && state.nodeById.has(id)) keep.add(id);
      });
      selection = keep;
      if (opts.silent) return;                 // caller is driving the drawer itself
      if (!keep.size) {
        state.selected = null;
        state.detail = null;
        ctx.nodeDrawerEl.classList.add('hidden');
        return;
      }
      var primary = (opts.primary && keep.has(opts.primary)) ? opts.primary : null;
      if (!primary) primary = sortByStep(Array.from(keep))[0] || null;
      if (primary && primary !== state.selected) {
        state.selected = primary;
        ctx.loadDetail(primary);
        if (!ctx.modeIsFind()) ctx.renderNodeDrawer();
      }
    }

    function toggle(id) {
      if (!id || !state.nodeById.has(id)) return;
      var next = Array.from(selection);
      if (!next.length && state.selected) next.push(state.selected);
      var at = next.indexOf(id);
      if (at >= 0) next.splice(at, 1); else next.push(id);
      applySelection(next, { primary: id });
    }

    /** Drop ids the graph no longer holds and invalidate the cached step order. */
    function prune() {
      var keep = new Set();
      selection.forEach(function (id) { if (state.nodeById.has(id)) keep.add(id); });
      selection = keep;
      stepIndex = null;
    }

    // ------------------------------------------------------------------ step order

    function declRank(id) {
      var n = state.nodeById.get(id);
      if (!n) return [1, '', 0, String(id)];
      return [0, String(n.file || ''), Number(n.line) || 0, String(n.label || n.name || id)];
    }

    function cmpDecl(a, b) {
      var ra = declRank(a), rb = declRank(b);
      for (var i = 0; i < ra.length; i++) {
        if (ra[i] < rb[i]) return -1;
        if (ra[i] > rb[i]) return 1;
      }
      return 0;
    }

    /** Depth-first step index over the contains hierarchy (cached until the graph changes). */
    function buildStepIndex() {
      if (stepIndex) return stepIndex;
      var parentOf = new Map();
      var kidsOf = new Map();
      state.edges.forEach(function (e) {
        if (e.kind !== 'contains' || e.source === e.target) return;
        if (!state.nodeById.has(e.source) || !state.nodeById.has(e.target)) return;
        if (parentOf.has(e.target)) return;                 // first container wins
        parentOf.set(e.target, e.source);
        var child = state.nodeById.get(e.target);
        var line = (e.lines && e.lines.length) ? e.lines[0] : (child.line || 0);
        if (!kidsOf.has(e.source)) kidsOf.set(e.source, []);
        kidsOf.get(e.source).push({ id: e.target, line: line });
      });
      kidsOf.forEach(function (list) {
        list.sort(function (a, b) { return (a.line - b.line) || cmpDecl(a.id, b.id); });
      });

      var order = new Map();
      var visited = new Set();
      var counter = 0;
      function walk(id, depth) {
        if (visited.has(id) || depth > 64) return;          // depth guard = cycle guard
        visited.add(id);
        order.set(id, counter++);
        var kids = kidsOf.get(id);
        if (!kids) return;
        for (var i = 0; i < kids.length; i++) walk(kids[i].id, depth + 1);
      }
      var roots = [];
      state.nodes.forEach(function (n) { if (!parentOf.has(n.id)) roots.push(n.id); });
      roots.sort(cmpDecl);
      roots.forEach(function (id) { walk(id, 0); });
      if (visited.size < state.nodes.length) {             // cycles / orphaned islands
        var rest = state.nodes.map(function (n) { return n.id; })
                              .filter(function (id) { return !visited.has(id); });
        rest.sort(cmpDecl);
        rest.forEach(function (id) { walk(id, 0); });
      }
      stepIndex = order;
      return order;
    }

    function sortByStep(list) {
      var order = buildStepIndex();
      return list.slice().sort(function (a, b) {
        var oa = order.has(a) ? order.get(a) : Infinity;
        var ob = order.has(b) ? order.get(b) : Infinity;
        if (oa !== ob) return oa - ob;
        return cmpDecl(a, b);
      });
    }

    // ------------------------------------------------------------------ geometry

    /** World-space bounding box of the chips for `list` (left/top edges, not centres). */
    function bounds(list) {
      var b = { left: Infinity, top: Infinity, right: -Infinity, bottom: -Infinity };
      list.forEach(function (id) {
        var p = ctx.posOf(id), sz = ctx.chipSizeFor(state.nodeById.get(id));
        b.left = Math.min(b.left, p.x - sz.w / 2);
        b.right = Math.max(b.right, p.x + sz.w / 2);
        b.top = Math.min(b.top, p.y - sz.h / 2);
        b.bottom = Math.max(b.bottom, p.y + sz.h / 2);
      });
      return b;
    }

    /**
     * Which axis the current selection most likely follows: 'h' | 'v' | 'd' | null.
     * Measures edge alignment, not the bounding box: a column has zero spread in the
     * chips' *left* edges whatever the label widths, a row has zero spread in their tops.
     */
    function detectAxis(list) {
      var minLeft = Infinity, maxLeft = -Infinity, minTop = Infinity, maxTop = -Infinity;
      list.forEach(function (id) {
        var p = ctx.posOf(id), sz = ctx.chipSizeFor(state.nodeById.get(id));
        var left = p.x - sz.w / 2, top = p.y - sz.h / 2;
        minLeft = Math.min(minLeft, left); maxLeft = Math.max(maxLeft, left);
        minTop = Math.min(minTop, top);   maxTop = Math.max(maxTop, top);
      });
      var dx = maxLeft - minLeft, dy = maxTop - minTop;
      if (dx < 0.5 && dy < 0.5) return null;
      if (dx < 0.5) return 'v';
      if (dy < 0.5) return 'h';
      var ratio = dx / dy;
      if (ratio > 2.2) return 'h';
      if (ratio < 0.45) return 'v';
      return 'd';
    }

    /**
     * Place `order` along `axis`, anchored at the selection's current top-left.
     * 'v' = column with the left edges flush, 'h' = row with the top edges flush,
     * 'd' = 45° diagonal running down-right (a counter-clockwise turn from vertical).
     */
    function place(order, axis, spacing) {
      if (!order.length) return;
      var sizes = order.map(function (id) { return ctx.chipSizeFor(state.nodeById.get(id)); });
      var b = bounds(order);
      var run = 0;
      var maxSide = 0;
      sizes.forEach(function (s) { maxSide = Math.max(maxSide, s.w, s.h); });
      var diagStep = (spacing + maxSide) * Math.SQRT1_2;
      order.forEach(function (id, i) {
        var p = ctx.posOf(id), sz = sizes[i];
        var x, y;
        if (axis === 'h') {
          if (i) run += (sizes[i - 1].w + sz.w) / 2 + spacing;
          x = b.left + sz.w / 2 + run;
          y = b.top + sz.h / 2;
        } else if (axis === 'd') {
          x = b.left + sz.w / 2 + i * diagStep;
          y = b.top + sz.h / 2 + i * diagStep;
        } else {
          if (i) run += (sizes[i - 1].h + sz.h) / 2 + spacing;
          x = b.left + sz.w / 2;
          y = b.top + sz.h / 2 + run;
        }
        p.x = p.tx = x;
        p.y = p.ty = y;
        p.fixed = true;                 // arranged nodes stay where they were put
        p.seeded = true;
      });
      // an in-flight layout run snapshotted fixed[] when it started; refresh it
      var active = state.layout.run;
      if (active && active.index) {
        order.forEach(function (id) {
          var i = active.index.get(id);
          if (i === undefined) return;
          active.fixed[i] = 1;
          active.x[i] = ctx.posOf(id).x;
          active.y[i] = ctx.posOf(id).y;
        });
      }
      ctx.scheduleViewerSave();
    }

    // ------------------------------------------------------------------ commands

    /** The ids sorted by where they currently sit along `axis` ('h' | 'v' | 'd'). */
    function orderAlong(list, axis) {
      var out = list.slice();
      if (axis === 'h') {
        out.sort(function (a, b) {
          return ctx.posOf(a).x - ctx.posOf(b).x || ctx.posOf(a).y - ctx.posOf(b).y;
        });
      } else if (axis === 'd') {
        out.sort(function (a, b) {
          return (ctx.posOf(a).x + ctx.posOf(a).y) - (ctx.posOf(b).x + ctx.posOf(b).y);
        });
      } else {
        out.sort(function (a, b) {
          return ctx.posOf(a).y - ctx.posOf(b).y || ctx.posOf(a).x - ctx.posOf(b).x;
        });
      }
      return out;
    }

    function sameOrder(a, b) {
      if (a.length !== b.length) return false;
      for (var i = 0; i < a.length; i++) { if (a[i] !== b[i]) return false; }
      return true;
    }

    /**
     * Ctrl/Cmd+Shift+H | V | D — arrange the selection; pressing the same command
     * again flips it, and a third press flips it back.
     *
     * The toggle is read from the geometry, not from a stored flag: if the nodes
     * currently run in step order along `axis`, the next layout is reversed;
     * otherwise (already reversed, or never arranged) it is step order. Deriving
     * the flip from a fresh `sortByStep` instead would reverse the same base
     * order on every press, so the selection would flip once and stay flipped.
     */
    function arrange(axis) {
      var list = ids();
      if (list.length < 2) { ctx.toast('Box-select at least two nodes first', 'warn'); return; }
      var step = sortByStep(list);
      var inStepOrder = sameOrder(orderAlong(list, axis), step);
      var order = inStepOrder ? step.slice().reverse() : step;
      place(order, axis, gap);
      var label = axis === 'v' ? 'vertically' : axis === 'h' ? 'horizontally' : 'diagonally';
      ctx.toast('Arranged ' + order.length + ' nodes ' + label +
                (inStepOrder ? ' (flipped)' : '') + ' · step order', 'info');
    }

    /** Shift+= / Shift+- — widen or tighten the spacing along the axis already in use. */
    function spacing(dir) {
      var list = ids();
      if (list.length < 2) { ctx.toast('Box-select at least two nodes first', 'warn'); return; }
      var axis = detectAxis(list);
      if (!axis) return;
      var order = orderAlong(list, axis);
      gap = clamp(dir > 0 ? gap * 1.25 + 0.5 : (gap - 0.5) / 1.25, ARRANGE_GAP_MIN, ARRANGE_GAP_MAX);
      place(order, axis, gap);
      ctx.toast('Spacing ' + Math.round(gap) + 'px ' +
                (axis === 'v' ? '(vertical)' : axis === 'h' ? '(horizontal)' : '(diagonal)'), 'info');
    }

    // ------------------------------------------------------------------ gestures

    /** Node ids whose chip rectangle intersects the marquee (screen coordinates). */
    function nodesInMarquee(x0, y0, x1, y1) {
      var left = Math.min(x0, x1), right = Math.max(x0, x1);
      var top = Math.min(y0, y1), bottom = Math.max(y0, y1);
      var zoom = state.camera.zoom;
      var out = [];
      state.view.nodes.forEach(function (node) {
        var p = state.pos.get(node.id);
        if (!p) return;
        var sz = ctx.chipSizeFor(node);
        var s = ctx.worldToScreen(p.x, p.y);
        var hw = Math.max(6, sz.w * zoom / 2);
        var hh = Math.max(5, sz.h * zoom / 2);
        if (s.x - hw <= right && s.x + hw >= left && s.y - hh <= bottom && s.y + hh >= top) {
          out.push(node.id);
        }
      });
      return out;
    }

    /**
     * Open the box-select. It starts on Shift (the pointer highlights whatever is
     * under it, then a drag turns into a marquee) and on a plain left-drag over
     * empty canvas (`opts.box`), which is now the default selection gesture —
     * panning moved to the middle mouse button. Ctrl/Cmd+click adds or removes a
     * single node. Returns true when it took the event.
     */
    function pointerDown(ev, pt, hit, opts) {
      opts = opts || {};
      if (ev.shiftKey || opts.box) {
        var add = !!(ev.ctrlKey || ev.metaKey);
        if (hit && ev.shiftKey) { if (add) toggle(hit.id); else ctx.selectNode(hit.id); }
        marquee = { x0: pt.x, y0: pt.y, x1: pt.x, y1: pt.y,
                    add: add, seed: Array.from(selection), moved: false,
                    plain: !ev.shiftKey };
        canvas.classList.add('marquee');
        return true;
      }
      if ((ev.ctrlKey || ev.metaKey) && hit) { toggle(hit.id); return true; }
      return false;
    }

    function pointerMove(pt) {
      if (!marquee) return false;
      marquee.x1 = pt.x; marquee.y1 = pt.y;
      if (Math.abs(pt.x - marquee.x0) + Math.abs(pt.y - marquee.y0) > 3) marquee.moved = true;
      return true;
    }

    function pointerUp() {
      if (!marquee) return false;
      var m = marquee;
      marquee = null;
      canvas.classList.remove('marquee');
      if (m.moved) {
        var picked = nodesInMarquee(m.x0, m.y0, m.x1, m.y1);
        if (m.add) {
          var seen = new Set(m.seed || []);
          picked.forEach(function (id) { seen.add(id); });
          picked = Array.from(seen);
        }
        applySelection(picked);
        ctx.scheduleViewerSave();
      } else if (m.plain) {
        applySelection([]);              // a plain left click on empty canvas clears
      }
      return true;
    }

    /** Screen-space box for the render loop, or null when no box is being dragged. */
    function marqueeRect() {
      if (!marquee || !marquee.moved) return null;
      return {
        x0: Math.min(marquee.x0, marquee.x1), y0: Math.min(marquee.y0, marquee.y1),
        x1: Math.max(marquee.x0, marquee.x1), y1: Math.max(marquee.y0, marquee.y1)
      };
    }

    /** Shortcut keys. Returns true when the event belonged to this module. */
    function key(ev) {
      if ((ev.ctrlKey || ev.metaKey) && ev.shiftKey) {
        var chord = ev.key;
        if (chord === 'v' || chord === 'V') { ev.preventDefault(); arrange('v'); return true; }
        if (chord === 'h' || chord === 'H') { ev.preventDefault(); arrange('h'); return true; }
        if (chord === 'd' || chord === 'D') { ev.preventDefault(); arrange('d'); return true; }
      }
      if (ev.shiftKey && (ev.key === '+' || ev.key === '=')) { ev.preventDefault(); spacing(1); return true; }
      if (ev.shiftKey && (ev.key === '-' || ev.key === '_')) { ev.preventDefault(); spacing(-1); return true; }
      if (ev.shiftKey && (ev.key === 'd' || ev.key === 'D')) { ev.preventDefault(); arrange('d'); return true; }
      return false;
    }

    /** The canvas is a selection surface: crosshair cursor; Shift is a modifier, not a mode. */
    function install() {
      window.addEventListener('keydown', function (ev) {
        if (ev.key === 'Shift') canvas.classList.add('shift-ready');
      });
      window.addEventListener('keyup', function (ev) {
        if (ev.key === 'Shift') canvas.classList.remove('shift-ready');
      });
      window.addEventListener('blur', function () {
        canvas.classList.remove('shift-ready');
        canvas.classList.remove('marquee');
      });
    }

    /** The shortcuts-panel groups this module contributes. */
    function shortcuts() {
      return [
        { group: 'Select', items: [
          { keys: [['drag']], desc: 'Box-select the nodes inside the box' },
          { keys: [['Middle', 'drag']], desc: 'Pan the canvas' },
          { keys: [['Shift', 'drag']], desc: 'Box-select (same as a plain drag on empty canvas)' },
          { keys: [['Shift', 'click']], desc: 'Highlight one node' },
          { keys: [['Ctrl', 'click']], desc: 'Add / remove a node from the selection' }
        ] },
        { group: 'Arrange', items: [
          { keys: [['Ctrl', 'Shift', 'V']], desc: 'Stack the selection vertically in step order (press again to flip)' },
          { keys: [['Ctrl', 'Shift', 'H']], desc: 'Lay the selection out horizontally in step order (press again to flip)' },
          { keys: [['Ctrl', 'Shift', 'D']], desc: 'Align the selection on a 45° diagonal' },
          { keys: [['Shift', '+']], desc: 'Widen the arranged spacing' },
          { keys: [['Shift', '−']], desc: 'Tighten the arranged spacing' }
        ] }
      ];
    }

    return {
      // selection
      ids: ids,
      has: function (id) { return selection.has(id); },
      set: applySelection,
      toggle: toggle,
      clear: function () { applySelection([]); },
      prune: prune,
      // gestures
      pointerDown: pointerDown,
      pointerMove: pointerMove,
      pointerUp: pointerUp,
      marqueeRect: marqueeRect,
      // commands + chrome
      arrange: arrange,
      spacing: spacing,
      key: key,
      install: install,
      shortcuts: shortcuts
    };
  };
})();
