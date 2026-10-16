/* csgraph - the canvas draw pass and the requestAnimationFrame loop.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== rendering

  function drawGrid(c) {
    var step = 140;
    var tl = screenToWorld(-8, -8);
    var br = screenToWorld(state.w + 8, state.h + 8);
    if ((br.x - tl.x) / step > 320) return;   // too dense when zoomed far out
    c.save();
    c.strokeStyle = 'rgba(120,160,210,0.05)';
    c.lineWidth = 1;
    c.beginPath();
    for (var x = Math.floor(tl.x / step) * step; x < br.x; x += step) {
      var px = Math.round(worldToScreen(x, 0).x) + 0.5;
      c.moveTo(px, 0); c.lineTo(px, state.h);
    }
    for (var y = Math.floor(tl.y / step) * step; y < br.y; y += step) {
      var py = Math.round(worldToScreen(0, y).y) + 0.5;
      c.moveTo(0, py); c.lineTo(state.w, py);
    }
    c.stroke();
    c.restore();
  }

  function drawArrowHead(c, x1, y1, x2, y2, color, size) {
    var a = Math.atan2(y2 - y1, x2 - x1);
    c.beginPath();
    c.moveTo(x2, y2);
    c.lineTo(x2 - size * Math.cos(a - 0.42), y2 - size * Math.sin(a - 0.42));
    c.lineTo(x2 - size * Math.cos(a + 0.42), y2 - size * Math.sin(a + 0.42));
    c.closePath();
    c.fillStyle = color;
    c.fill();
  }

  /** Rounded rectangle path (chips, pills). */
  function roundRect(c, x, y, w, h, r) {
    r = Math.min(r, w / 2, h / 2);
    c.beginPath();
    c.moveTo(x + r, y);
    c.lineTo(x + w - r, y);
    c.quadraticCurveTo(x + w, y, x + w, y + r);
    c.lineTo(x + w, y + h - r);
    c.quadraticCurveTo(x + w, y + h, x + w - r, y + h);
    c.lineTo(x + r, y + h);
    c.quadraticCurveTo(x, y + h, x, y + h - r);
    c.lineTo(x, y + r);
    c.quadraticCurveTo(x, y, x + r, y);
    c.closePath();
  }

  function draw() {
    var c = ctx;
    c.setTransform(state.dpr, 0, 0, state.dpr, 0, 0);
    c.clearRect(0, 0, state.w, state.h);
    drawGrid(c);

    var view = computeView();
    var zoom = state.camera.zoom;
    var hovered = state.hovered;
    var selected = state.selected;

    // neighbour set of whatever is being pointed at (for edge labels / emphasis)
    var focusId = (hovered && state.nodeById.has(hovered)) ? hovered
                : ((selected && state.nodeById.has(selected)) ? selected : null);
    var focusSet = (focusId && state.adjacency.get(focusId)) || null;

    var drawn = [];
    var byId = new Map();
    for (var i = 0; i < view.nodes.length; i++) {
      var node = view.nodes[i];
      var p = state.pos.get(node.id);
      if (!p) continue;
      var sz = chipSizeFor(node);
      var s = worldToScreen(p.x, p.y);
      var isMulti = arrange.has(node.id);
      var isRel = !!(focusId && (node.id === focusId || (focusSet && focusSet.has(node.id))));
      var rec = {
        node: node, p: p, sx: s.x, sy: s.y, label: sz.text,
        w: sz.w * zoom, h: sz.h * zoom, fs: CHIP_FONT * zoom,
        rel: isRel,
        // a selected node is never dimmed by the reducer, even if it is far from the focus
        dim: !!(focusId && node.id !== focusId && !isRel && !isMulti && !(node.id === selected))
      };
      if (node.id === selected || isMulti) rec.selected = true;
      drawn.push(rec);
      byId.set(node.id, rec);
    }

    // --- edges (thin VT links + relation pills) ------------------------
    var labelAll = zoom >= EDGE_LABEL_ZOOM;
    var showPills = !state.filters.hideEdgeLabels;
    for (var e = 0; e < view.edges.length; e++) {
      var edge = view.edges[e];
      var A = byId.get(edge.source), B = byId.get(edge.target);
      if (!A || !B) continue;
      var st = EDGE_STYLE[edge.kind] || DEFAULT_EDGE_STYLE;
      var touches = focusId && (edge.source === focusId || edge.target === focusId);
      if (focusId && !touches) { /* VT keeps the rest visible but faint */ }

      var alpha = st.alpha * (focusId ? (touches ? 1.25 : 0.35) : 1);
      if (alpha > 1) alpha = 1;

      var dx = B.sx - A.sx, dy = B.sy - A.sy;
      var len = Math.sqrt(dx * dx + dy * dy) || 1;
      var ux = dx / len, uy = dy / len;
      var trimA = A.w / 2 + 1, trimB = B.w / 2 + (st.arrow ? 7 : 1);
      var x1 = A.sx + ux * trimA, y1 = A.sy + uy * trimA;
      var x2 = B.sx - ux * trimB, y2 = B.sy - uy * trimB;
      if (len <= trimA + trimB) { x1 = A.sx; y1 = A.sy; x2 = B.sx; y2 = B.sy; }

      // stacking offset: strand i of k sits on its own parallel line
      var pairCount = edge._pairCount || 1;
      if (pairCount > 1) {
        var stack = (edge._pairIndex - (pairCount - 1) / 2) * EDGE_STACK_PX;
        var sox = -uy * stack, soy = ux * stack;
        x1 += sox; y1 += soy; x2 += sox; y2 += soy;
      }

      var curve = 0;
      if (edge._pairCount > 1 && state.settings.curvature > 0) {
        curve = state.settings.curvature * (edge._pairIndex - (edge._pairCount - 1) / 2);
      }
      var mx = (x1 + x2) / 2 - uy * curve * len;
      var my = (y1 + y2) / 2 + ux * curve * len;

      c.save();
      c.globalAlpha = alpha;
      c.strokeStyle = st.color;
      c.lineWidth = st.width * clamp(Math.sqrt(zoom), 0.7, 2.2);
      if (st.dash) c.setLineDash(st.dash);
      c.beginPath();
      c.moveTo(x1, y1);
      if (curve !== 0) c.quadraticCurveTo(mx, my, x2, y2);
      else c.lineTo(x2, y2);
      c.stroke();
      c.setLineDash([]);
      if (st.arrow && len > trimA + trimB + 2) {
        drawArrowHead(c, curve !== 0 ? mx : x1, curve !== 0 ? my : y1, x2, y2, st.color, 7.5 + st.width);
      }
      c.restore();

      if (showPills && (labelAll || touches)) {
        var lx = curve !== 0 ? 0.25 * x1 + 0.5 * mx + 0.25 * x2 : (x1 + x2) / 2;
        var ly = curve !== 0 ? 0.25 * y1 + 0.5 * my + 0.25 * y2 : (y1 + y2) / 2;
        drawEdgePill(c, lx, ly, EDGE_LABEL[edge.kind] || edge.kind, touches);
      }
    }

    // --- node chips (dim first, selected last) -------------------------
    drawn.sort(function (a, b) {
      return (a.dim ? 0 : 1) - (b.dim ? 0 : 1) || (a.selected ? 1 : 0) - (b.selected ? 1 : 0);
    });
    for (var k = 0; k < drawn.length; k++) drawChip(c, drawn[k]);

    // --- box-select marquee (screen space, drawn last) -----------------
    var box = arrange.marqueeRect();
    if (box) {
      var pal = uiPalette();
      c.save();
      c.fillStyle = 'rgba(134,170,249,0.13)';
      c.fillRect(box.x0, box.y0, box.x1 - box.x0, box.y1 - box.y0);
      c.strokeStyle = pal.chipSelBorder;
      c.lineWidth = 1;
      c.setLineDash([5, 4]);
      c.strokeRect(box.x0 + 0.5, box.y0 + 0.5,
                   Math.max(0, box.x1 - box.x0 - 1), Math.max(0, box.y1 - box.y0 - 1));
      c.restore();
    }
  }

  /** Small pill naming the relation (Calling / Uses / Assigns …). */
  function drawEdgePill(c, x, y, text, strong) {
    c.save();
    c.font = '10px ui-monospace, Menlo, Consolas, monospace';
    c.textBaseline = 'middle';
    var padX = 5, tw = c.measureText(text).width;
    var w = tw + padX * 2, h = 14;
    roundRect(c, x - w / 2, y - h / 2, w, h, 7);
    c.globalAlpha = strong ? 0.96 : 0.72;
    c.fillStyle = strong ? '#2f3d5c' : '#1b2334';
    c.fill();
    c.globalAlpha = strong ? 1 : 0.8;
    c.strokeStyle = '#3d4a69';
    c.lineWidth = 1;
    c.stroke();
    c.fillStyle = strong ? '#dbe6f5' : '#9aa6c2';
    c.fillText(text, x - tw / 2, y + 0.5);
    c.restore();
  }

  function drawChip(c, rec) {
    var w = rec.w, h = rec.h;
    var x = rec.sx - w / 2, y = rec.sy - h / 2;
    var z = state.camera.zoom;
    var pal = uiPalette();
    c.save();
    if (rec.dim) c.globalAlpha = 0.42;

    var dot = chipDot() * z;
    var r = Math.max(1.6, dot / 2);

    // when the text is no longer legible the chip degenerates to its glyph
    if (rec.fs < 6) {
      c.beginPath();
      c.arc(rec.sx, rec.sy, r, 0, Math.PI * 2);
      c.fillStyle = rec.dim ? pal.chipDimText : kindColor(rec.node.kind);
      c.fill();
      c.restore();
      return;
    }

    roundRect(c, x, y, w, h, Math.min(2, h / 2));
    c.fillStyle = rec.dim ? pal.chipDimBg : pal.chipBg;
    c.fill();
    c.lineWidth = rec.selected ? 1.6 : 1;
    c.strokeStyle = rec.selected ? pal.chipSelBorder : pal.chipBorder;
    c.stroke();

    if (rec.p && rec.p.fixed) {                  // pinned marker: violet dot at the top-right
      var pr = Math.max(2, Math.min(3.5, h * 0.3));
      c.beginPath();
      c.arc(x + w - pr - 2, y + pr + 2, pr, 0, Math.PI * 2);
      c.fillStyle = pal.pin;
      c.fill();
      c.strokeStyle = 'rgba(0, 0, 0, .55)';
      c.lineWidth = 1;
      c.stroke();
    }

    c.beginPath();
    c.arc(x + CHIP_PAD * z + r, rec.sy, r, 0, Math.PI * 2);
    c.fillStyle = rec.dim ? pal.chipDimText : kindColor(rec.node.kind);
    c.fill();

    c.font = rec.fs.toFixed(1) + 'px ui-monospace, Menlo, Consolas, monospace';
    c.textBaseline = 'middle';
    c.fillStyle = rec.dim ? pal.chipDimText : pal.chipText;
    c.fillText(rec.label, x + (CHIP_PAD + chipDot() + CHIP_GAP) * z, rec.sy + 0.5);
    c.restore();
  }

  // ================================================================== render loop  // ================================================================== render loop

  function easePositions() {
    var k = 0.18;   // fraction of the remaining distance closed each frame
    state.pos.forEach(function (p) {
      p.x += (p.tx - p.x) * k;
      p.y += (p.ty - p.y) * k;
      if (Math.abs(p.tx - p.x) < 0.02) p.x = p.tx;
      if (Math.abs(p.ty - p.y) < 0.02) p.y = p.ty;
    });
  }

  function frame() {
    if (isLayingOut()) stepLayout();
    easePositions();
    updateFollow();
    updateAutoFit();
    draw();
    window.requestAnimationFrame(frame);
  }

