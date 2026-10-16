/* csgraph - ForceAtlas2 + Noverlap layout, and the camera / fit geometry.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== layout
  //
  // A faithful port of graphology-layout-forceatlas2's iterate() (the same algorithm
  // VRCX runs in graphLayoutWorker.js) followed by a Noverlap separation pass:
  //
  //   * repulsion: k_r * mass_i * mass_j / d^2   (mass = 1 + weighted degree)
  //   * strong gravity: gravity * mass * distance toward the origin  -> hubs central
  //   * attraction (linear): one unit vector per unique pair, |F| = d
  //   * per-node speed from `swinging` / `traction` (FA2's adaptivity), divided by
  //     slowDown = 2, with the MAX_FORCE clamp
  //
  // The reference sets scalingRatio = spacing, gravity = lerp(1.6, 0.6, t) and
  // barnesHutOptimize = true; here repulsion uses the exact O(n^2) loop for small
  // graphs and a uniform-grid cutoff above FA2_GRID_NODES (repulsion decays as
  // 1/d^2, so the truncation is invisible) — the "Barnes-Hut style approximation"
  // the task allows, without a quadtree's allocation churn on the main thread.

  function layoutT() {
    return clamp((state.settings.spacing - SPACING_MIN) / (SPACING_MAX - SPACING_MIN), 0, 1);
  }

  function layoutGravity() {
    return 1.6 + (0.6 - 1.6) * layoutT();     // lerp(1.6, 0.6, t)
  }

  function noverlapRatio() { return 1.05 + (1.35 - 1.05) * layoutT(); }
  function noverlapMargin() { return 1 + (8 - 1) * layoutT(); }

  function isLayingOut() { return state.layout.mode !== 'idle'; }

  /** Build the FA2 working arrays from the current positions. */
  function buildLayoutRun() {
    var nodes = state.nodes;
    var n = nodes.length;
    var index = new Map();
    for (var i = 0; i < n; i++) index.set(nodes[i].id, i);

    var run = {
      n: n,
      index: index,
      x: new Float64Array(n), y: new Float64Array(n),
      dx: new Float64Array(n), dy: new Float64Array(n),
      odx: new Float64Array(n), ody: new Float64Array(n),
      mass: new Float64Array(n), conv: new Float64Array(n),
      fixed: new Uint8Array(n),
      hw: new Float64Array(n), hh: new Float64Array(n),   // chip half-extents
      edges: [],          // unique undirected pairs as flat index pairs
      cell: 0,
      grid: null
    };
    for (i = 0; i < n; i++) {
      var node = nodes[i];
      var p = posOf(node.id);
      run.x[i] = p.tx;
      run.y[i] = p.ty;
      run.mass[i] = degreeOf(node) + 1;
      run.conv[i] = 1;
      run.fixed[i] = p.fixed ? 1 : 0;
      var sz = chipSizeFor(node);
      run.hw[i] = sz.w / 2;
      run.hh[i] = sz.h / 2;
      p.r = Math.max(sz.h / 2, (sz.w + sz.h) / 4 * 0.8);
    }
    for (i = 0; i < state.pairs.length; i++) {
      var a = index.get(state.pairs[i].a);
      var b = index.get(state.pairs[i].b);
      if (a === undefined || b === undefined) continue;
      run.edges.push(a, b);
    }
    run.cell = Math.max(4 * state.settings.spacing, 160);
    return run;
  }

  /** Repulsion for one iteration: exact O(n^2) below the grid threshold, grid above. */
  function fa2Repulsion(run) {
    var n = run.n, xs = run.x, ys = run.y, dx = run.dx, dy = run.dy, mass = run.mass;
    var k = state.settings.spacing;
    var i, j, xd, yd, d2, f;

    if (n <= FA2_GRID_NODES) {
      for (i = 0; i < n; i++) {
        var xi = xs[i], yi = ys[i], mi = mass[i];
        var dxi = 0, dyi = 0;
        for (j = 0; j < n; j++) {
          if (j === i) continue;
          xd = xi - xs[j]; yd = yi - ys[j];
          d2 = xd * xd + yd * yd;
          if (d2 > 0) {
            f = (k * mi * mass[j]) / d2;
            dxi += xd * f; dyi += yd * f;
          }
        }
        dx[i] = dxi; dy[i] = dyi;
      }
      return;
    }

    // Uniform grid; cell size == cutoff, so scanning the 3x3 neighbourhood is exact
    // for every pair closer than the cutoff.
    var cell = run.cell;
    var grid = run.grid;
    if (!grid) grid = run.grid = new Map();
    else grid.clear();
    for (i = 0; i < n; i++) {
      var key = Math.floor(xs[i] / cell) + ':' + Math.floor(ys[i] / cell);
      var bucket = grid.get(key);
      if (bucket) bucket.push(i);
      else grid.set(key, [i]);
    }
    for (i = 0; i < n; i++) {
      var px = xs[i], py = ys[i], m = mass[i];
      var fx = 0, fy = 0;
      var cx = Math.floor(px / cell), cy = Math.floor(py / cell);
      for (var ox = -1; ox <= 1; ox++) {
        for (var oy = -1; oy <= 1; oy++) {
          var list = grid.get((cx + ox) + ':' + (cy + oy));
          if (!list) continue;
          for (var q = 0; q < list.length; q++) {
            j = list[q];
            if (j === i) continue;
            xd = px - xs[j]; yd = py - ys[j];
            d2 = xd * xd + yd * yd;
            if (d2 > 0) {
              f = (k * m * mass[j]) / d2;
              fx += xd * f; fy += yd * f;
            }
          }
        }
      }
      dx[i] = fx; dy[i] = fy;
    }
  }

  /** One ForceAtlas2 iteration (port of iterate.js with the reference's settings). */
  function fa2Iterate(run) {
    var n = run.n, xs = run.x, ys = run.y, dx = run.dx, dy = run.dy,
        odx = run.odx, ody = run.ody, mass = run.mass, conv = run.conv, fixed = run.fixed;
    var i;

    // 1) reset deltas, remember the previous ones (FA2's swinging/traction inputs)
    for (i = 0; i < n; i++) { odx[i] = dx[i]; ody[i] = dy[i]; }

    // 2) repulsion (also initialises dx/dy for this iteration)
    fa2Repulsion(run);

    var k = state.settings.spacing;
    var gravity = layoutGravity();
    var g = gravity / k;

    // 3) strong gravity: factor = k * mass * (gravity / k) = mass * gravity
    for (i = 0; i < n; i++) {
      var gx = xs[i], gy = ys[i];
      if (gx !== 0 || gy !== 0) {
        var gf = mass[i] * gravity;
        dx[i] -= gx * gf;
        dy[i] -= gy * gf;
      }
    }

    // 4) linear attraction: factor = -1 per unique pair (delta is the raw vector)
    var edges = run.edges;
    for (i = 0; i < edges.length; i += 2) {
      var a = edges[i], b = edges[i + 1];
      var xd = xs[a] - xs[b], yd = ys[a] - ys[b];
      if (xd !== 0 || yd !== 0) {
        dx[a] -= xd; dy[a] -= yd;
        dx[b] += xd; dy[b] += yd;
      }
    }

    // 5) apply forces with FA2's adaptive per-node speed
    for (i = 0; i < n; i++) {
      if (fixed[i]) { dx[i] = 0; dy[i] = 0; continue; }
      var swx = odx[i] - dx[i], swy = ody[i] - dy[i];
      var swinging = mass[i] * Math.sqrt(swx * swx + swy * swy);
      var trx = odx[i] + dx[i], tryy = ody[i] + dy[i];
      var traction = Math.sqrt(trx * trx + tryy * tryy) / 2;
      var nodespeed = (conv[i] * Math.log(1 + traction)) / (1 + Math.sqrt(swinging));
      conv[i] = Math.min(1, Math.sqrt((nodespeed * (dx[i] * dx[i] + dy[i] * dy[i])) /
                                      (1 + Math.sqrt(swinging))));
      xs[i] += dx[i] * (nodespeed / FA2_SLOWDOWN);
      ys[i] += dy[i] * (nodespeed / FA2_SLOWDOWN);
    }
  }

  /**
   * One Noverlap iteration. A chip is a wide box, not a circle: a long namespace
   * label overlaps a neighbour that is metres away by its centre, so separating
   * bounding *circles* (the old pass) left wide chips sitting on top of each other
   * — exactly what "the children do not move away from the namespace" looked like.
   * The pass now separates axis-aligned bounding boxes: an overlapping pair is
   * pushed apart along whichever axis needs the least correction, so the layout
   * stays compact while the boxes can never share a pixel.
   */
  function noverlapIterate(run) {
    var n = run.n, xs = run.x, ys = run.y, fixed = run.fixed;
    var hw = run.hw, hh = run.hh;
    var ratio = noverlapRatio(), margin = noverlapMargin();
    var nodes = state.nodes;
    var i, j;

    // bucket by a cell large enough for the biggest possible colliding pair:
    // needX/needY never exceed `cell`, so the 3x3 neighbourhood is exact.
    var maxExtent = 0;
    for (i = 0; i < n; i++) maxExtent = Math.max(maxExtent, Math.max(hw[i], hh[i]));
    var cell = Math.max(8, 2 * (maxExtent * ratio + margin));
    var grid = run.noverGrid || (run.noverGrid = new Map());
    grid.clear();
    for (i = 0; i < n; i++) {
      var key = Math.floor(xs[i] / cell) + ':' + Math.floor(ys[i] / cell);
      var bucket = grid.get(key);
      if (bucket) bucket.push(i); else grid.set(key, [i]);
    }

    var moved = 0;
    for (i = 0; i < n; i++) {
      var xi = xs[i], yi = ys[i];
      var hxi = hw[i] * ratio + margin, hyi = hh[i] * ratio + margin;
      var cx = Math.floor(xi / cell), cy = Math.floor(yi / cell);
      for (var ox = -1; ox <= 1; ox++) {
        for (var oy = -1; oy <= 1; oy++) {
          var list = grid.get((cx + ox) + ':' + (cy + oy));
          if (!list) continue;
          for (var q = 0; q < list.length; q++) {
            j = list[q];
            if (j <= i) continue;
            var needX = hxi + hw[j] * ratio + margin;
            var needY = hyi + hh[j] * ratio + margin;
            var dx = xs[j] - xi, dy = ys[j] - yi;
            var penX = needX - Math.abs(dx);
            if (penX <= 0) continue;               // already clear on x
            var penY = needY - Math.abs(dy);
            if (penY <= 0) continue;               // already clear on y
            moved++;
            // the shortest way out: resolve on the axis with the smaller overlap
            var ux = 0, uy = 0, push;
            if (penX < penY) {
              ux = dx < 0 ? -1 : (dx > 0 ? 1 : (hash32(nodes[i].id) % 2 ? 1 : -1));
              push = penX;
            } else {
              uy = dy < 0 ? -1 : (dy > 0 ? 1 : (hash32(nodes[i].id) % 2 ? 1 : -1));
              push = penY;
            }
            var mx = ux * push, my = uy * push;
            var iFixed = fixed[i], jFixed = fixed[j];
            if (!iFixed && !jFixed) {
              xs[i] -= mx * 0.5; ys[i] -= my * 0.5;
              xs[j] += mx * 0.5; ys[j] += my * 0.5;
            } else if (!iFixed) {
              xs[i] -= mx; ys[i] -= my;
            } else if (!jFixed) {
              xs[j] += mx; ys[j] += my;
            }
            // i may have moved a fraction of a cell; keep the scan honest
            xi = xs[i]; yi = ys[i];
            cx = Math.floor(xi / cell); cy = Math.floor(yi / cell);
          }
        }
      }
    }
    return moved;
  }

  /** Start (or restart) the FA2 + Noverlap pipeline. */
  function startLayout(opts) {
    opts = opts || {};
    if (!state.nodes.length) {
      state.layout = { mode: 'idle', iter: 0, total: 0, noverIter: 0, noverMax: 0, run: null };
      updateLayoutState();
      return;
    }
    if (opts.reinitialize) seedPositions();
    var run = buildLayoutRun();
    var iterations = clamp(state.settings.iterations, ITER_MIN, ITER_MAX);
    state.layout = {
      mode: 'fa2',
      iter: 0,
      total: iterations,
      noverIter: 0,
      noverMax: clamp(Math.round(Math.sqrt(run.n) * 6), 200, 600),
      run: run
    };
    updateLayoutState();
  }

  /** Run the active layout in ~LAYOUT_SLICE_MS slices, then freeze. */
  function stepLayout() {
    var L = state.layout;
    if (L.mode === 'idle') return;
    var run = L.run;
    if (!run) { L.mode = 'idle'; updateLayoutState(); return; }

    // pinned nodes may have been dragged since the run started
    for (var i = 0; i < run.n; i++) {
      if (!run.fixed[i]) continue;
      var p = state.pos.get(state.nodes[i].id);
      run.x[i] = p.x; run.y[i] = p.y;
    }

    var deadline = (window.performance && window.performance.now ? window.performance.now() : Date.now()) + LAYOUT_SLICE_MS;
    var now = function () {
      return window.performance && window.performance.now ? window.performance.now() : Date.now();
    };

    if (L.mode === 'fa2') {
      while (L.iter < L.total && now() < deadline) {
        fa2Iterate(run);
        L.iter++;
      }
      if (L.iter >= L.total) { L.mode = 'noverlap'; L.noverIter = 0; }
    } else {
      while (L.noverIter < L.noverMax && now() < deadline) {
        if (noverlapIterate(run) === 0) { L.noverIter = L.noverMax; break; }
        L.noverIter++;
      }
      if (L.noverIter >= L.noverMax) L.mode = 'idle';
    }

    // publish positions for the renderer (easing smooths them out)
    for (i = 0; i < run.n; i++) {
      var q = state.pos.get(state.nodes[i].id);
      q.tx = run.x[i];
      q.ty = run.y[i];
    }
    if (L.mode === 'idle') state.layout = { mode: 'idle', iter: 0, total: 0, noverIter: 0, noverMax: 0, run: null };
    updateLayoutState();
  }

  function updateLayoutState() {
    if (!layoutStateEl) return;
    var L = state.layout;
    if (L.mode === 'idle') { layoutStateEl.textContent = ''; return; }
    if (L.mode === 'fa2') layoutStateEl.textContent = 'FA2 ' + L.iter + '/' + L.total;
    else layoutStateEl.textContent = 'overlap ' + L.noverIter + '/' + L.noverMax;
  }

  /** Jitter positions slightly after a spacing change so the layout can re-expand. */
  function jitterPositions(magnitude) {
    state.nodes.forEach(function (node, i) {
      var p = posOf(node.id);
      if (p.fixed) return;
      p.x += (((hash32(node.id + 'jx') % 1000) / 1000) - 0.5) * magnitude;
      p.y += (((hash32(node.id + 'jy') % 1000) / 1000) - 0.5) * magnitude;
      p.tx = p.x; p.ty = p.y;
    });
  }


  // ================================================================== camera / geometry

  function worldToScreen(x, y) {
    return {
      x: (x - state.camera.x) * state.camera.zoom + state.w / 2,
      y: (y - state.camera.y) * state.camera.zoom + state.h / 2
    };
  }

  function screenToWorld(sx, sy) {
    return {
      x: (sx - state.w / 2) / state.camera.zoom + state.camera.x,
      y: (sy - state.h / 2) / state.camera.zoom + state.camera.y
    };
  }

  function localPoint(ev) {
    var rect = canvas.getBoundingClientRect();
    return { x: ev.clientX - rect.left, y: ev.clientY - rect.top };
  }

  function zoomAt(sx, sy, factor) {
    var cam = state.camera;
    state.autoFit = false;
    var nz = clamp(cam.zoom * factor, MIN_ZOOM, MAX_ZOOM);
    if (nz === cam.zoom) return;
    var w = screenToWorld(sx, sy);
    cam.zoom = nz;
    cam.x = w.x - (sx - state.w / 2) / nz;
    cam.y = w.y - (sy - state.h / 2) / nz;
  }

  /** Smoothly fly the camera to a node (clears once it arrives or the user takes over). */
  function followNode(id, zoom) {
    if (!state.nodeById.has(id)) return;
    state.autoFit = false;
    state.follow = { id: id, zoom: zoom === undefined ? null : zoom };
  }

  /**
   * A small neighbourhood settles into a small world-space extent (FA2 keeps node
   * sizes but not the coordinate scale), so pick a zoom from the node count to keep
   * focused subgraphs legible.
   */
  function focusZoom(n) {
    return clamp(Math.sqrt(220 / Math.max(1, n)), 0.7, 3);
  }

  function updateFollow() {
    var f = state.follow;
    if (!f) return;
    var p = state.pos.get(f.id);
    if (!p) { state.follow = null; return; }
    var k = 0.16;
    state.camera.x += (p.x - state.camera.x) * k;
    state.camera.y += (p.y - state.camera.y) * k;

    var zoomDone = true;
    if (f.zoom !== null) {
      state.camera.zoom += (f.zoom - state.camera.zoom) * k;
      if (Math.abs(f.zoom - state.camera.zoom) < 0.004) {
        state.camera.zoom = f.zoom;
        f.zoom = null;
      } else {
        zoomDone = false;
      }
    }
    if (Math.abs(p.x - state.camera.x) < 0.7 &&
        Math.abs(p.y - state.camera.y) < 0.7 && zoomDone) {
      state.camera.x = p.x;
      state.camera.y = p.y;
      state.follow = null;
    }
  }

  /** Camera transform that would frame the visible nodes. Returns null for an empty view. */
  function computeFit() {
    computeView();
    var ps = [];
    state.view.nodes.forEach(function (n) {
      var p = state.pos.get(n.id);
      if (p) ps.push(p);
    });
    if (!ps.length) return null;
    var minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    ps.forEach(function (p) {
      if (p.x < minX) minX = p.x;
      if (p.y < minY) minY = p.y;
      if (p.x > maxX) maxX = p.x;
      if (p.y > maxY) maxY = p.y;
    });
    var pad = 70;
    var w = Math.max(1, maxX - minX), h = Math.max(1, maxY - minY);
    return {
      zoom: clamp(Math.min((state.w - pad * 2) / w, (state.h - pad * 2) / h), MIN_ZOOM, 2.2),
      x: (minX + maxX) / 2,
      y: (minY + maxY) / 2
    };
  }

  function applyFit(t) {
    state.camera.zoom = t.zoom;
    state.camera.x = t.x;
    state.camera.y = t.y;
  }

  function fitView() {
    var t = computeFit();
    if (t) applyFit(t);
    state.follow = null;
    state.autoFit = false;
  }

  /** While `autoFit` is on, keep the settling graph framed (snaps when it stops). */
  function updateAutoFit() {
    if (!state.autoFit) return;
    var t = computeFit();
    if (!t) { state.autoFit = false; return; }
    var running = isLayingOut();
    var k = running ? 0.07 : 1;
    state.camera.x += (t.x - state.camera.x) * k;
    state.camera.y += (t.y - state.camera.y) * k;
    state.camera.zoom += (t.zoom - state.camera.zoom) * k;
    if (!running) state.autoFit = false;
  }

