/* csgraph - the node/edge model, ownership, expansion and the visible subset.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== graph model

  function posOf(id) {
    var p = state.pos.get(id);
    if (!p) {
      p = { x: 0, y: 0, tx: 0, ty: 0, r: 6, fixed: false };
      state.pos.set(id, p);
    }
    return p;
  }

  function pairKey(a, b) {
    return a < b ? a + '\u0000' + b : b + '\u0000' + a;
  }

  /** Rebuild indexes/adjacency/unique-pair list from the node and edge arrays. */
  function buildModel() {
    state.nodeById = new Map();
    state.edgeById = new Map();
    state.edgeByKey = new Map();
    state.adjacency = new Map();
    state.nodes.forEach(function (n) {
      state.nodeById.set(n.id, n);
      state.adjacency.set(n.id, new Set());
    });
    state.edges.forEach(function (e) {
      if (!state.adjacency.has(e.source)) state.adjacency.set(e.source, new Set());
      if (!state.adjacency.has(e.target)) state.adjacency.set(e.target, new Set());
      state.adjacency.get(e.source).add(e.target);
      state.adjacency.get(e.target).add(e.source);
      state.edgeById.set(e.id, e);
      state.edgeByKey.set(e.kind + '\u0000' + e.source + '\u0000' + e.target, e);
    });

    var seen = new Map();
    state.edges.forEach(function (e) {
      if (!state.adjacency.has(e.source) || !state.adjacency.has(e.target)) return;
      var key = pairKey(e.source, e.target);
      if (!seen.has(key)) seen.set(key, { a: e.source, b: e.target });
    });
    state.pairs = Array.from(seen.values());

    // curvature grouping: parallel links between the same pair fan out
    var groups = new Map();
    state.edges.forEach(function (e) {
      var key = pairKey(e.source, e.target);
      var group = groups.get(key);
      if (!group) { group = []; groups.set(key, group); }
      e._pairIndex = group.length;
      group.push(e);
    });
    state.edges.forEach(function (e) {
      var group = groups.get(pairKey(e.source, e.target));
      e._pairCount = group ? group.length : 1;
    });

    // the box-selection and its step order are keyed on the live graph
    if (arrange) arrange.prune();
  }

  function degreeOf(node) {
    var nbrs = state.adjacency.get(node.id);
    if (nbrs) return nbrs.size;
    return (node.degree === undefined || node.degree === null) ? 0 : node.degree;
  }

  // Chip geometry. The box shrink-wraps the measured label: at most 1px padding
  // horizontally and vertically, a configurable kind dot and a 2px gap. Measurement
  // happens once per distinct label at a fixed world-unit font size, so every chip
  // scales linearly with the camera zoom and nothing is width-capped.
  var CHIP_FONT = 11;        // world-unit font size
  var CHIP_PAD = 1;          // <=1px padding on every side
  var CHIP_GAP = 2;          // gap between glyph and text
  var CHIP_MAX_CHARS = 40;   // labels are only truncated above this
  var chipMetricsCache = new Map();

  // The kind-dot diameter is user configurable (Layout ▸ Node size). Changing it
  // invalidates the measurement cache and re-runs FA2 (node radius feeds it).
  function chipDot() {
    var v = state.settings && state.settings.nodeSize;
    return clamp(typeof v === 'number' ? v : NODE_SIZE_DEFAULT,
                 NODE_SIZE_MIN, NODE_SIZE_MAX);
  }

  function chipLabel(node) {
    var t = String(node.label || node.name || node.id || '');
    if (t.length > CHIP_MAX_CHARS) t = t.slice(0, CHIP_MAX_CHARS - 1) + '…';
    return t;
  }

  function chipSizeFor(node) {
    var text = chipLabel(node);
    var m = chipMetricsCache.get(text);
    if (m) return m;
    var dot = chipDot();
    ctx.font = CHIP_FONT + 'px ui-monospace, Menlo, Consolas, monospace';
    var tw = ctx.measureText(text).width;
    m = {
      text: text,
      w: CHIP_PAD + dot + CHIP_GAP + tw + CHIP_PAD,        // exactly the words + 1px
      h: Math.max(CHIP_FONT + CHIP_PAD * 2, dot + 2)       // text height or the glyph
    };
    if (chipMetricsCache.size > 5000) chipMetricsCache.clear();
    chipMetricsCache.set(text, m);
    return m;
  }

  // ---------------------------------------------------------------- ownership model
  //
  // Every node and edge added to the graph is tagged with the owner key(s) that want
  // it: 'base' for the namespace view, or '<nodeId>|<relationKey>' for a Relations row.
  // Collapsing a row merely drops its tag, so nodes shared by several expanded rows
  // (or belonging to the base view) survive; rows toggle independently.

  function tagOwner(map, id, ownerKey) {
    var set = map.get(id);
    if (!set) { set = new Set(); map.set(id, set); }
    set.add(ownerKey);
  }

  function addNodes(list, ownerKey) {
    var added = [];
    (list || []).forEach(function (n) {
      if (!n || !n.id) return;
      var existing = state.nodeById.get(n.id);
      if (existing) {
        // fill in fields the deeper payload knows better
        Object.keys(n).forEach(function (k) {
          if (existing[k] === undefined || existing[k] === null || existing[k] === '') existing[k] = n[k];
        });
      } else {
        state.nodes.push(n);
        state.nodeById.set(n.id, n);
        added.push(n.id);
      }
      tagOwner(state.nodeOwners, n.id, ownerKey);
    });
    return added;
  }

  function addEdges(list, ownerKey) {
    (list || []).forEach(function (e) {
      if (!e || !e.source || !e.target) return;
      // One arrow per logical relation: the same edge arrives under several ids
      // (server 'eNNN' vs a relation toggle's 'rel:…'), and buildModel fans every
      // copy into its own stacked arc — so dedupe on (kind, source, target).
      var key = e.kind + '\u0000' + e.source + '\u0000' + e.target;
      var known = state.edgeByKey.get(key);
      if (known) {
        tagOwner(state.edgeOwners, known.id, ownerKey);
        return;
      }
      var id = e.id || ('e:' + e.kind + ':' + e.source + '->' + e.target);
      var existing = state.edgeById.get(id);
      if (existing) {
        state.edgeByKey.set(key, existing);
        tagOwner(state.edgeOwners, id, ownerKey);
        return;
      }
      var edge = {
        id: id, source: e.source, target: e.target, kind: e.kind,
        count: e.count || 1, lines: e.lines || [], ambiguous: !!e.ambiguous
      };
      state.edges.push(edge);
      state.edgeById.set(id, edge);
      state.edgeByKey.set(key, edge);
      tagOwner(state.edgeOwners, id, ownerKey);
    });
  }

  function dropOwner(ownerKey) {
    var removed = 0;
    state.nodes = state.nodes.filter(function (n) {
      var set = state.nodeOwners.get(n.id);
      if (!set || !set.has(ownerKey)) return true;
      set.delete(ownerKey);
      if (set.size === 0) { state.nodeOwners.delete(n.id); removed++; return false; }
      return true;
    });
    state.edges = state.edges.filter(function (e) {
      var set = state.edgeOwners.get(e.id);
      if (!set || !set.has(ownerKey)) return true;
      set.delete(ownerKey);
      if (set.size === 0) { state.edgeOwners.delete(e.id); return false; }
      return true;
    });
    // orphaned position entries
    var live = new Set(state.nodes.map(function (n) { return n.id; }));
    Array.from(state.pos.keys()).forEach(function (id) {
      if (!live.has(id)) state.pos.delete(id);
    });
    buildModel();
    return removed;
  }

  function clearGraph() {
    state.nodes = [];
    state.edges = [];
    state.nodeOwners = new Map();
    state.edgeOwners = new Map();
    state.pos = new Map();
    state.relations.expandedByNode = new Map();
    state.relations.listOpenByNode = new Map();
    state.selected = null;
    state.detail = null;
    state.focus = null;
    buildModel();
  }

  /** Place newly added nodes around an already-positioned neighbour (or in a disc). */
  function seedNewNodes(ids) {
    if (!ids || !ids.length) return;
    var n = ids.length;
    ids.forEach(function (id, i) {
      var p = posOf(id);
      if (p.seeded) return;
      // a persisted position from the last run wins over the spiral seed
      var saved = pendingRestore && pendingRestore.positions &&
                  pendingRestore.positions.get(id);
      if (saved) {
        p.x = saved[0]; p.y = saved[1];
        p.tx = p.x; p.ty = p.y;
        p.seeded = true;
        return;
      }
      var anchor = null;
      var nbrs = state.adjacency.get(id);
      if (nbrs) {
        nbrs.forEach(function (nb) {
          if (anchor) return;
          var q = state.pos.get(nb);
          if (q && q.seeded) anchor = q;
        });
      }
      if (anchor) {
        var a = i * 2.399963229728653 + (hash32(id) % 100) / 100;
        p.x = anchor.x + Math.cos(a) * (34 + (hash32(id + 'r') % 40));
        p.y = anchor.y + Math.sin(a) * (34 + (hash32(id + 'r') % 40));
      } else {
        var t = (i + 0.5) / n;
        var ang = i * 2.399963229728653;
        var r = 60 + Math.sqrt(n) * 40 * Math.sqrt(t);
        p.x = Math.cos(ang) * r;
        p.y = Math.sin(ang) * r;
      }
      p.tx = p.x; p.ty = p.y;
      p.seeded = true;
    });
  }

  /** Install a /api/subgraph payload as a fresh base graph. */
  function setGraph(data, opts) {
    opts = opts || {};
    clearGraph();
    state.truncated = !!data.truncated;
    state.focus = data.focus || null;
    var ids = addNodes(data.nodes || [], 'base');
    addEdges(data.edges || [], 'base');
    buildModel();
    seedNewNodes(ids.length ? ids : state.nodes.map(function (n) { return n.id; }));
    startLayout({ reinitialize: false });
    if (opts.selectFocus && state.focus && state.nodeById.has(state.focus)) selectNode(state.focus);
    if (opts.follow && state.focus) {
      state.follow = { id: state.focus, zoom: focusZoom(state.nodes.length) };
    } else {
      state.follow = null;
    }
    updateCanvasNotice();
    updateEmptyState();
    renderNodeDrawer();
    state.autoFit = false;
    if (opts.fit) {
      var t0 = computeFit();
      if (t0) { applyFit(t0); state.autoFit = true; }
    }
  }

  /** Re-run the layout after an expansion (short pass so the view stays responsive). */
  function quickLayout() {
    var keep = state.settings.iterations;
    state.settings.iterations = Math.min(keep, 260);
    startLayout({ reinitialize: false });
    state.settings.iterations = keep;
    updateLayoutState();
  }

  function hasPosition(id) {
    var p = state.pos.get(id);
    return !!(p && p.seeded);
  }

  // ---------------------------------------------------------------- expansion spline
  //
  // A force layout answers "where do the new nodes go?" by pushing everything out
  // from the centre, which destroys an arrangement made by hand. So when the node
  // being expanded already has placed neighbours that lie on a line, the new
  // children are appended to that same line: they sit on a spline that leaves the
  // node along the existing direction and curls gently outward, and they are pinned
  // so FA2 cannot scatter them. Without a line the old spiral seed still applies.

  /** Half-extent of a chip's box along (ux,uy): the box support function. */
  function chipSupport(node, ux, uy) {
    var sz = chipSizeFor(node);
    return Math.abs(ux) * sz.w / 2 + Math.abs(uy) * sz.h / 2;
  }

  /** Placed neighbours of `rootId` (seeded and still in the graph), root-relative. */
  function placedNeighbours(rootId) {
    var root = state.pos.get(rootId);
    var nbrs = state.adjacency.get(rootId);
    if (!root || !nbrs) return [];
    var far = 12 * state.settings.spacing;
    var out = [];
    nbrs.forEach(function (nb) {
      var q = state.pos.get(nb);
      if (!q || !q.seeded || !state.nodeById.has(nb)) return;
      var dx = q.x - root.x, dy = q.y - root.y;
      if (dx * dx + dy * dy > far * far) return;
      out.push({ id: nb, x: dx, y: dy, p: q });
    });
    return out;
  }

  /**
   * The straight line the placed neighbours already follow, as a unit direction
   * away from the root, or null when they do not form one. Principal component of
   * the root-relative points; a run must be at least two neighbours, its residual
   * spread must stay small against the run, and its angle must sit near a 45°
   * multiple (horizontal, vertical or a diagonal) — an unarranged blob is rejected.
   */
  function neighbourLine(nbrs) {
    var n = nbrs.length;
    if (n < 2) return null;                 // one neighbour is not a line
    var mx = 0, my = 0;
    nbrs.forEach(function (q) { mx += q.x; my += q.y; });
    mx /= n; my /= n;
    var sxx = 0, sxy = 0, syy = 0;
    nbrs.forEach(function (q) {
      var dx = q.x - mx, dy = q.y - my;
      sxx += dx * dx; sxy += dx * dy; syy += dy * dy;
    });
    var theta = 0.5 * Math.atan2(2 * sxy, sxx - syy);
    var lo = Infinity, hi = -Infinity, spread = 0;
    var tx = Math.cos(theta), ty = Math.sin(theta);
    nbrs.forEach(function (q) {
      var a = q.x * tx + q.y * ty;          // along the line
      var b = q.x * -ty + q.y * tx;         // across it
      if (a < lo) lo = a;
      if (a > hi) hi = a;
      spread = Math.max(spread, Math.abs(b));
    });
    var run = hi - lo;
    if (run < 1) return null;               // every neighbour in one spot
    if (spread > Math.max(SPLINE_FIT_MIN, SPLINE_FIT_SLACK * run)) return null;
    // snap to the nearest 45° axis, and reject a run that is too far off any of them
    var step = Math.PI / 4;
    var snapped = Math.round(theta / step) * step;
    if (Math.abs(theta - snapped) > SPLINE_AXIS_SLACK) return null;
    var ux = Math.cos(snapped), uy = Math.sin(snapped);
    // Point the line away from the root: toward the neighbours' mass. A symmetric
    // run (the root in the middle) falls back to its farther end.
    var along = 0;
    nbrs.forEach(function (q) { along += q.x * ux + q.y * uy; });
    if (along < 0) { ux = -ux; uy = -uy; }
    else if (along < 1e-6 && Math.abs(lo) > Math.abs(hi)) { ux = -ux; uy = -uy; }
    return { ux: ux, uy: uy };
  }

  /** Centre of the placed graph: the mean of the seeded node positions. */
  function graphCenter() {
    var n = 0, x = 0, y = 0;
    state.pos.forEach(function (p) {
      if (!p.seeded) return;
      x += p.x; y += p.y; n++;
    });
    return n ? { x: x / n, y: y / n } : { x: 0, y: 0 };
  }

  /**
   * The chip centres of one spline run: start at the point `edge` along (ux,uy) from
   * the root, walk the tuple list, and turn the tangent by `bend` before every step
   * after the first. Pure geometry so both curl directions can be compared.
   */
  function splinePoints(root, ux, uy, edge, bend, ids) {
    var pts = [];
    var cx = root.x + ux * edge, cy = root.y + uy * edge;
    ids.forEach(function (id, i) {
      if (i) {
        var c = Math.cos(bend), s = Math.sin(bend);
        var rx = ux * c - uy * s, ry = ux * s + uy * c;
        ux = rx; uy = ry;
      }
      var sup = chipSupport(state.nodeById.get(id), ux, uy);
      var x = cx + ux * sup, y = cy + uy * sup;
      pts.push({ x: x, y: y });
      cx = x + ux * (sup + SPLINE_GAP);     // far edge + gap, ready for the next chip
      cy = y + uy * (sup + SPLINE_GAP);
    });
    return pts;
  }

  /**
   * Append `ids` to the line its root already follows. Returns true when the spline
   * placed at least one node; false leaves the caller to fall back to `seedNewNodes`.
   * Restored positions still win over the spline.
   */
  function seedExpansion(rootId, ids) {
    var root = state.pos.get(rootId);
    if (!root || !root.seeded) return false;

    var todo = [];
    (ids || []).forEach(function (id) {
      var p = posOf(id);
      if (p.seeded) return;
      var saved = pendingRestore && pendingRestore.positions &&
                  pendingRestore.positions.get(id);
      if (saved) {
        p.x = saved[0]; p.y = saved[1];
        p.tx = p.x; p.ty = p.y;
        p.seeded = true;
        return;
      }
      todo.push(id);
    });
    if (!todo.length) return false;

    var nbrs = placedNeighbours(rootId);
    var line = neighbourLine(nbrs);
    if (!line) return false;
    var ux = line.ux, uy = line.uy;

    // The spline always heads away from the centre of the graph: of the line's two
    // senses, keep the one pointing outward from the centre. When the existing
    // children already run outward this simply continues their direction.
    var centre = graphCenter();
    var rx = root.x - centre.x, ry = root.y - centre.y;
    if (rx * rx + ry * ry > 1 && ux * rx + uy * ry < 0) { ux = -ux; uy = -uy; }

    // first chip starts just past the furthest neighbour edge...
    var edge = 0;
    nbrs.forEach(function (q) {
      var proj = q.x * ux + q.y * uy;
      var sup = chipSupport(state.nodeById.get(q.id), ux, uy);
      if (proj + sup > edge) edge = proj + sup;
    });
    edge += SPLINE_GAP;
    // ...and never inside the root itself: an existing child can sit closer than
    // the root is wide, and the run must still leave the root's own box behind.
    var rootNode = state.nodeById.get(rootId);
    if (rootNode) edge = Math.max(edge, chipSupport(rootNode, ux, uy) + SPLINE_GAP);

    // The curl also goes away from the centre: lay out both curl directions and keep
    // the one whose chips end up farther from the centre, so an expansion never loops
    // back into the graph it came from.
    var turn = Math.min(SPLINE_TURN, SPLINE_TURN_MAX / Math.max(1, todo.length - 1));
    var best = null;
    [1, -1].forEach(function (sign) {
      var pts = splinePoints(root, ux, uy, edge, turn * sign, todo);
      var reach = 0;
      pts.forEach(function (pt) {
        reach += Math.sqrt((pt.x - centre.x) * (pt.x - centre.x) +
                           (pt.y - centre.y) * (pt.y - centre.y));
      });
      if (!best || reach > best.reach) best = { reach: reach, pts: pts };
    });
    todo.forEach(function (id, i) {
      var p = posOf(id);
      p.x = p.tx = best.pts[i].x;
      p.y = p.ty = best.pts[i].y;
      p.fixed = true;                       // arranged-like: the run survives the layout
      p.seeded = true;
    });
    return true;
  }

  /**
   * The expansion fallback (no arranged line to continue): fan `ids` out from
   * `rootId` on a golden-angle ring, each chip clear of the root's measured box
   * and of every chip already placed in this run. The older fallback used a fixed
   * 34..74 world-unit spiral that ignored how wide a chip is, so a long namespace
   * label swallowed its own children; this one moves them away from it, always.
   * Restored positions still win; the result is only a seed, the layout settles
   * it afterwards.
   */
  function seedFan(rootId, ids) {
    var root = state.pos.get(rootId);
    var rootNode = state.nodeById.get(rootId);
    var todo = [];
    (ids || []).forEach(function (id) {
      var p = posOf(id);
      if (p.seeded) return;
      var saved = pendingRestore && pendingRestore.positions &&
                  pendingRestore.positions.get(id);
      if (saved) {
        p.x = saved[0]; p.y = saved[1];
        p.tx = p.x; p.ty = p.y;
        p.seeded = true;
        return;
      }
      todo.push(id);
    });
    if (!todo.length || !root || !root.seeded || !rootNode) return false;

    var placed = [];                                  // this run's boxes, world units
    var base = (hash32(rootId) % 100) / 100;
    todo.forEach(function (id, i) {
      var node = state.nodeById.get(id);
      var sz = chipSizeFor(node);
      var hw = sz.w / 2, hh = sz.h / 2;
      var ang = base + i * 2.399963229728653;
      var ux = Math.cos(ang), uy = Math.sin(ang);
      // first clear the root box along this direction, then step outward until
      // this chip also clears the chips already fanned out in this same run
      var dist = chipSupport(rootNode, ux, uy) + chipSupport(node, ux, uy) + SPLINE_GAP;
      for (var guard = 0; guard < 96; guard++) {
        var x = root.x + ux * dist, y = root.y + uy * dist;
        var clear = true;
        for (var k = 0; k < placed.length; k++) {
          var o = placed[k];
          if (Math.abs(x - o.x) < hw + o.hw + SPLINE_GAP &&
              Math.abs(y - o.y) < hh + o.hh + SPLINE_GAP) { clear = false; break; }
        }
        if (clear) break;
        dist += 6;
      }
      var p = posOf(id);
      p.x = p.tx = root.x + ux * dist;
      p.y = p.ty = root.y + uy * dist;
      p.seeded = true;
      placed.push({ x: p.x, y: p.y, hw: hw, hh: hh });
    });
    return true;
  }

  // ---------------------------------------------------------------- expansion

  /**
   * Expand one node: children (contains:out) always, plus inherits for types and
   * calls for members. `extra` may name additional relation keys to pull in.
   */
  function expandNode(id, opts) {
    opts = opts || {};
    return apiGet('/api/node', { id: id }).then(function (d) {
      var rels = d.relations || [];
      var byKey = {};
      rels.forEach(function (r) { byKey[r.key] = r; });
      var wanted = ['contains:out'];
      if (opts.keys && opts.keys.length) {
        wanted = opts.keys.slice();
      } else if (opts.all) {
        rels.forEach(function (r) { if (wanted.indexOf(r.key) < 0) wanted.push(r.key); });
      } else {
        var node = d.node || state.nodeById.get(id) || {};
        var typeLike = ['namespace', 'class', 'struct', 'interface', 'enum', 'record'].indexOf(node.kind) >= 0;
        wanted.push(typeLike ? 'inherits:out' : 'calls:out');
      }
      var newNodeIds = [];
      var newEdges = [];
      wanted.forEach(function (key) {
        var r = byKey[key];
        if (!r) return;
        var owner = id + '|' + key;
        var nodes = r.nodes || [];
        newNodeIds = newNodeIds.concat(addNodes(nodes, owner));
        nodes.forEach(function (n) {
          newEdges.push(relationEdge(id, r, n));
        });
        addEdges(newEdges.splice(0, newEdges.length), owner);
        addRelationNodesToExpanded(id, key);
      });
      buildModel();
      if (!seedExpansion(id, newNodeIds) && !seedFan(id, newNodeIds)) seedNewNodes(newNodeIds);
      quickLayout();
      if (state.selected === id) loadDetail(id);
      updateCanvasNotice();
      return d;
    }).catch(function (err) {
      toast('Expand failed: ' + err.message, 'error');
    });
  }

  function relationEdge(nodeId, relation, other) {
    var out = relation.dir === 'out';
    return {
      id: 'rel:' + nodeId + ':' + relation.key + ':' + other.id,
      source: out ? nodeId : other.id,
      target: out ? other.id : nodeId,
      kind: relation.kind,
      count: 1
    };
  }

  function expandedSet(nodeId) {
    var set = state.relations.expandedByNode.get(nodeId);
    if (!set) { set = new Set(); state.relations.expandedByNode.set(nodeId, set); }
    return set;
  }
  function addRelationNodesToExpanded(nodeId, key) { expandedSet(nodeId).add(key); }

  /** Per-row toggle: adds exactly that bucket, removes exactly that bucket. */
  function toggleRelation(relation) {
    var nodeId = state.selected;
    if (!nodeId || !relation) return;
    var set = expandedSet(nodeId);
    var key = relation.key;
    var owner = nodeId + '|' + key;
    if (set.has(key)) {
      set.delete(key);
      dropOwner(owner);
      quickLayout();
      renderRelations();
      updateCanvasNotice();
      return;
    }
    var nodes = relation.nodes || [];
    var newIds = addNodes(nodes, owner);
    var edges = nodes.map(function (n) { return relationEdge(nodeId, relation, n); });
    addEdges(edges, owner);
    set.add(key);
    openRelationList(nodeId, key);      // show what just landed on the map
    buildModel();
    if (!seedExpansion(nodeId, newIds) && !seedFan(nodeId, newIds)) seedNewNodes(newIds);
    quickLayout();
    renderRelations();
    updateCanvasNotice();
  }

  /** Per-node "hide children": drops that node's contains bucket and hides its kids. */
  function toggleHideChildren(nodeId) {
    if (!nodeId) return;
    var set = expandedSet(nodeId);
    if (state.hiddenChildren.has(nodeId)) {
      state.hiddenChildren.delete(nodeId);
      state.hiddenChildNodes.delete(nodeId);
      updateHideChildrenToggle();
      renderRelations();
      updateCanvasNotice();
      // bring the children back if they had been dropped from the graph
      expandNode(nodeId, { keys: ['contains:out'] });
      return;
    }
    var kids = [];
    var d = state.detail;
    if (d && d.node && d.node.id === nodeId) {
      (d.relations || []).forEach(function (r) {
        if (r.key === 'contains:out') (r.nodes || []).forEach(function (n) { kids.push(n.id); });
      });
    }
    state.edges.forEach(function (e) {
      if (e.kind === 'contains' && e.source === nodeId) kids.push(e.target);
    });
    state.hiddenChildren.add(nodeId);
    state.hiddenChildNodes.set(nodeId, kids);
    if (set.has('contains:out')) {
      set.delete('contains:out');
      dropOwner(nodeId + '|contains:out');
    }
    buildModel();
    quickLayout();
    updateHideChildrenToggle();
    renderRelations();
    updateCanvasNotice();
  }

  /** Keep the global checkbox and the group chip consistent with the per-node state. */
  function updateHideChildrenToggle() {
    // global toggle mirrors filters.groupChildren; per-node hidden state is separate
    if (hideChildrenEl) hideChildrenEl.checked = !state.filters.groupChildren;
    if (groupChildrenEl) groupChildrenEl.checked = state.filters.groupChildren;
    var wrap = $('#kindChildWrap');
    if (wrap) wrap.classList.toggle('off', !state.filters.groupChildren);
  }

  function relationIsActive(relation) {
    var nodeId = state.selected;
    if (!nodeId) return false;
    var set = state.relations.expandedByNode.get(nodeId);
    return !!(set && set.has(relation.key));
  }

  /** Namespace-first default view: only the namespaces, with an expand hint. */
  function loadNamespaces() {
    return apiGet('/api/kinds').then(function (k) {
      applyKinds(k);
      clearGraph();
      var nsNodes = (k.namespaces || []).map(function (n) {
        return {
          id: n.id, name: n.full, label: n.full, full: n.full, kind: 'namespace',
          ns: '', owner: '', ownerKind: '', parent: '', file: '', line: 0, endLine: 0,
          degree: n.count || 0, literal: false, external: false
        };
      });
      if (!nsNodes.length) {
        // no namespace nodes: fall back to the top-level types
        nsNodes = (k.types || []).slice();
      }
      var ids = addNodes(nsNodes, 'base');
      buildModel();
      seedNewNodes(ids);
      startLayout({ reinitialize: false });
      state.truncated = false;
      updateCanvasNotice();
      updateEmptyState(nsNodes.length
        ? 'Indexed. Expand a namespace (double-click) to pull in its types.'
        : 'Nothing to show for this index.');
      if (nsNodes.length === 1) expandNode(nsNodes[0].id);
      state.autoFit = true;
      applyPendingRestore();
      return k;
    }).catch(function (err) {
      toast('Could not load kinds: ' + err.message, 'error');
      showEmpty('Could not load the index facets.<br>' + esc(err.message));
    });
  }

  /** "Full graph": every namespace expanded with its types. */
  function loadFullGraph(withOverlay) {
    var seq = ++graphSeq;
    if (withOverlay) showLoading('Loading graph…');
    return apiGet('/api/kinds').then(function (k) {
      if (seq !== graphSeq) return;
      applyKinds(k);
      clearGraph();
      var roots = (k.namespaces || []).map(function (n) {
        return {
          id: n.id, name: n.full, label: n.full, full: n.full, kind: 'namespace',
          ns: '', owner: '', ownerKind: '', parent: '', file: '', line: 0, endLine: 0,
          degree: n.count || 0
        };
      });
      if (!roots.length) roots = (k.types || []).slice();
      var ids = addNodes(roots, 'base');
      buildModel();
      seedNewNodes(ids);
      startLayout({ reinitialize: false });
      state.autoFit = true;
      updateCanvasNotice();
      updateEmptyState();
      // expand each root once (children; types also get their inherits)
      return Promise.all(roots.slice(0, 40).map(function (r) {
        return expandNode(r.id).catch(function () { return null; });
      })).then(function () {
        if (seq === graphSeq) startLayout({ reinitialize: false });
      });
    }).catch(function (err) {
      if (seq !== graphSeq) return;
      toast('Could not load graph: ' + err.message, 'error');
    }).then(function () { if (withOverlay) hideLoading(); });
  }

  /**
   * Does `node` live inside namespace `ns`? A namespace node belongs when it is
   * that namespace or (unless "include sub-namespaces" is off) a descendant of
   * it; every other node belongs when the namespace it was declared in (`ns`,
   * from the engine) is that namespace or a descendant. `''` means "no namespace
   * filter" and matches everything.
   */
  function nodeInNamespace(node, ns) {
    if (!ns) return true;
    var own = node.kind === 'namespace'
      ? (node.full || node.label || node.name || '')
      : (node.ns || '');
    own = String(own || '');
    // Off: isolate exactly one namespace, so ns.subns and ns.subns.subsubns are
    // filtered out instead of coming along with it.
    if (state.filters.namespaceDeep === false) return own === ns;
    return own === ns || own.indexOf(ns + '.') === 0;
  }

  /** Compute the subset of nodes/edges passing the filters, plus view degrees. */
  function computeView() {
    var f = state.filters;
    var anyKind = f.kinds.size > 0;
    var ns = f.namespace || '';
    var nodes = state.nodes.filter(function (n) {
      if (anyKind && !f.kinds.has(n.kind)) return false;
      if (ns && !nodeInNamespace(n, ns)) return false;
      return true;
    });
    var nodeSet = new Set(nodes.map(function (n) { return n.id; }));

    // per-node "hide children" removes those child nodes entirely
    if (state.hiddenChildNodes.size) {
      state.hiddenChildNodes.forEach(function (kids) {
        kids.forEach(function (id) { nodeSet.delete(id); });
      });
      nodes = nodes.filter(function (n) { return nodeSet.has(n.id); });
    }

    var edges = state.edges.filter(function (e) {
      if (!f.edges.has(e.kind) || !nodeSet.has(e.source) || !nodeSet.has(e.target)) return false;
      if (e.kind === 'contains' && !f.groupChildren) return false;
      if (e.kind === 'contains' && state.hiddenChildren.has(e.source)) return false;
      if (e.kind !== 'contains' && !f.groupEdges) return false;
      return true;
    });

    if (f.hideIsolated) {
      var used = new Set();
      edges.forEach(function (e) { used.add(e.source); used.add(e.target); });
      nodes = nodes.filter(function (n) { return used.has(n.id); });
      nodeSet = new Set(nodes.map(function (n) { return n.id; }));
      edges = edges.filter(function (e) { return nodeSet.has(e.source) && nodeSet.has(e.target); });
    }

    var deg = new Map();
    nodes.forEach(function (n) { deg.set(n.id, 0); });
    edges.forEach(function (e) {
      deg.set(e.source, (deg.get(e.source) || 0) + 1);
      deg.set(e.target, (deg.get(e.target) || 0) + 1);
    });

    state.view = { nodes: nodes, edges: edges, deg: deg };
    return state.view;
  }

  function updateEmptyState(message) {
    if (!state.nodes.length) {
      showEmpty(message || (state.indexed
        ? 'Nothing loaded yet.<br>Use <b>Full graph</b> or expand a namespace.'
        : 'Nothing indexed yet.<br>Pick a folder and press <b>Index</b>.'));
    } else {
      hideEmpty();
    }
  }

