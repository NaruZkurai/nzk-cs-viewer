/* csgraph v2 — call hierarchy, type hierarchy and go-to-implementation.
 *
 * Part of the RUST-ONLY v2 viewer. Not loaded by v1.
 *
 * This Monaco build is the standalone 0.57 bundle: it has no
 * `registerCallHierarchyProvider`, no `registerTypeHierarchyProvider` and no
 * `showCallHierarchy` action (verified against vendor/monaco/monaco.d.ts). So
 * the hierarchy is built the way the rest of this viewer is: from the engine's
 * own graph, rendered in a small peek/tree panel anchored over the source pane
 * and styled like the "Resolve chain" drawer.
 *
 *   Shift+Alt+H / context menu   call hierarchy for the symbol under the cursor
 *   context menu                 type hierarchy (bases + derived types)
 *   context menu                 go to implementation
 *
 * The call tree comes from `/api/chain` (edge kind, line number and cycle flag
 * per hop); the type tree comes from `/api/node` relations — `inherits:out` is
 * "this type inherits from" (bases), `inherits:in` is "types that inherit from
 * this" (derived) — plus a forward chain for the transitive base chain. The
 * implementation provider derives candidates from those same relations and maps
 * them through CsLang's `docLocFor`/`locForFile`, preferring a location that is
 * already on screen, and returns [] rather than a guess.
 */
'use strict';

(function () {
  var V = 'csgraph.hierarchy';
  var TYPE_KINDS = {
    'class': 1, 'struct': 1, 'interface': 1, 'enum': 1, 'record': 1,
    'delegate': 1, 'type': 1
  };
  var MEMBER_KINDS = {
    'method': 1, 'constructor': 1, 'operator': 1, 'property': 1,
    'field': 1, 'event': 1, 'indexer': 1
  };
  var EDGE_LABELS = {
    calls: 'calls', constructs: 'constructs', uses: 'uses', assigns: 'assigns',
    inherits: 'inherits', contains: 'contains'
  };
  var MAX_DERIVED_DEPTH = 3;

  var monaco = null;
  var editor = null;
  var ctxRef = null;
  var installed = false;
  var actionsInstalled = false;
  var providerInstalled = false;
  var lastErr = null;

  var panel = null;
  var panelBody = null;
  var panelTitle = null;
  var dirSelect = null;
  var depthSelect = null;

  var state = {
    mode: 'call',       // call | type
    nodeId: null,       // resolved symbol
    word: '',
    dir: 'both',
    depth: 2,
    collapse: {},
    generation: 0
  };
  var typeData = null;

  function report(e) {
    lastErr = e;
    if (window.console && console.error) console.error('[csgraph hierarchy]', e);
  }

  function api(path, params) {
    var fn = (ctxRef && ctxRef.apiGet) || window.apiGet;
    if (typeof fn !== 'function') return Promise.reject(new Error('apiGet unavailable'));
    return fn(path, params);
  }

  function esc(s) {
    var fn = (ctxRef && ctxRef.esc) || window.esc;
    if (typeof fn === 'function') return fn(s == null ? '' : String(s));
    return String(s == null ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  function toast(msg, kind) {
    var fn = (ctxRef && ctxRef.toast) || window.toast;
    if (typeof fn === 'function') fn(msg, kind);
  }

  function labelOf(n) {
    return (n && (n.label || n.name || n.full || n.id)) || '?';
  }

  function resolveAt(info) {
    var lang = ctxRef && ctxRef.lang;
    if (lang && typeof lang.resolveAt === 'function') {
      return lang.resolveAt(info.file, info.line, info.word);
    }
    if (lang && typeof lang.resolveSymbol === 'function') return lang.resolveSymbol(info);
    return Promise.resolve(null);
  }

  function infoAtCursor() {
    if (!editor) return null;
    var model = editor.getModel();
    var pos = editor.getPosition();
    if (!model || !pos || !(ctxRef && ctxRef.lang && ctxRef.lang.infoAt)) return null;
    return ctxRef.lang.infoAt(model, pos);
  }

  // ==================================================================== tree
  /* One normalized hop: id/name/label/kind/file/line/edge/cycle/children. */
  function normChain(t, depth) {
    if (!t) return null;
    var n = t.node || {};
    var kids = (t.children || []).map(function (c) { return normChain(c, depth + 1); })
      .filter(Boolean);
    return {
      id: n.id || '',
      name: n.name || '',
      label: labelOf(n),
      full: n.full || '',
      kind: n.kind || '',
      file: n.file || '',
      line: (t.line != null ? t.line : n.line) || 0,
      edge: t.edge || null,
      cycle: !!t.cycle,
      depth: depth,
      children: kids
    };
  }

  function flattenTree(node, out) {
    if (!node) return out || [];
    out = out || [];
    out.push(node);
    (node.children || []).forEach(function (c) { flattenTree(c, out); });
    return out;
  }

  /** The call tree for a node, exactly as `/api/chain` returns it. */
  function treeFor(id, dir, opts) {
    dir = dir || 'forward';
    opts = opts || {};
    var depth = opts.depth || state.depth || 2;
    var max = opts.max || 300;
    return api('/api/chain', { id: id, depth: depth, dir: dir, max: max })
      .then(function (d) {
        if (!d || d.ok === false) throw new Error('chain failed');
        var root = normChain(d.tree, 0);
        return {
          id: id, dir: dir, depth: depth, truncated: !!d.truncated,
          root: root, rows: flattenTree(root, [])
        };
      })
      .catch(function (err) {
        report(err);
        return { id: id, dir: dir, root: null, rows: [], error: String(err && err.message || err) };
      });
  }

  // ============================================================ type hierarchy
  function derivedTypes(id, maxDepth) {
    var seen = {}; seen[id] = 1;
    var out = [];
    var frontier = [{ id: id, d: 0 }];
    function round() {
      var next = [];
      var jobs = frontier.map(function (f) {
        if (f.d >= maxDepth) return Promise.resolve();
        return api('/api/node', { id: f.id }).then(function (det) {
          if (!det || !det.relations) return;
          det.relations.forEach(function (g) {
            if (g.key !== 'inherits:in') return;
            (g.nodes || []).forEach(function (n) {
              if (!n || !n.id || seen[n.id]) return;
              seen[n.id] = 1;
              out.push(n);
              next.push({ id: n.id, d: f.d + 1 });
            });
          });
        }).catch(function (err) { report(err); });
      });
      return Promise.all(jobs).then(function () {
        frontier = next;
        if (next.length) return round();
        return out;
      });
    }
    return round();
  }

  function relationsOf(id) {
    return api('/api/node', { id: id }).then(function (d) {
      if (!d || !d.node) return null;
      var bases = [], derived = [];
      (d.relations || []).forEach(function (g) {
        if (g.key === 'inherits:out') bases = g.nodes || [];
        else if (g.key === 'inherits:in') derived = g.nodes || [];
      });
      return { node: d.node, bases: bases, derived: derived, relations: d.relations || [] };
    });
  }

  function typeTreeFor(id) {
    return relationsOf(id).then(function (rel) {
      if (!rel) return { id: id, node: null, bases: [], derived: [], chain: { rows: [] } };
      return treeFor(id, 'forward', { depth: 4 }).then(function (chain) {
        return { id: id, node: rel.node, bases: rel.bases, derived: rel.derived, chain: chain };
      });
    }).catch(function (err) {
      report(err);
      return { id: id, node: null, bases: [], derived: [], chain: { rows: [] }, error: String(err) };
    });
  }

  // ============================================================ implementation
  function sameMemberKind(a, b) {
    if (!a || !b) return true;
    if (a === b) return true;
    return (MEMBER_KINDS[a] && MEMBER_KINDS[b]) || (TYPE_KINDS[a] && TYPE_KINDS[b]);
  }

  function ownerTypeId(node) {
    if (!node) return Promise.resolve(null);
    if (TYPE_KINDS[node.kind]) return Promise.resolve(node.id);
    if (!node.parent) return Promise.resolve(null);
    return api('/api/node', { id: node.parent }).then(function (d) {
      if (d && d.node && TYPE_KINDS[d.node.kind]) return d.node.id;
      return null;
    }).catch(function (err) { report(err); return null; });
  }

  /**
   * The real declaration rows of implementations/overrides of `node`.
   *
   * For a member: the derived types of its owner, then the member of that same
   * name declared in each. For a type: its derived types. `/api/find`
   * implementations are only accepted when their owner is one of those derived
   * types, so a same-named but unrelated member is never returned.
   */
  function implementationTargets(model, node) {
    if (!node || !node.id) return Promise.resolve([]);
    return ownerTypeId(node).then(function (ownerId) {
      if (!ownerId) return [];
      if (TYPE_KINDS[node.kind]) {
        return derivedTypes(ownerId, MAX_DERIVED_DEPTH);
      }
      return derivedTypes(ownerId, MAX_DERIVED_DEPTH).then(function (types) {
        var derivedIds = {};
        types.forEach(function (t) { derivedIds[t.id] = t; });
        var found = [];
        var jobs = types.map(function (t) {
          return api('/api/decls', { node: t.id }).then(function (d) {
            (d && d.decls || []).forEach(function (row) {
              if (row && row.id && row.id !== node.id && row.name === node.name &&
                  sameMemberKind(row.kind, node.kind)) {
                found.push(row);
              }
            });
          }).catch(function (err) { report(err); });
        });
        return Promise.all(jobs).then(function () {
          return api('/api/find', { q: node.name, limit: 200 }).then(function (f) {
            (f && f.implementations || []).forEach(function (row) {
              if (!row || !row.id || row.id === node.id) return;
              if (row.name !== node.name) return;
              if (!row.file || !row.line) return;
              if (!row.parent || !derivedIds[row.parent]) return;
              found.push(row);
            });
            return found;
          }).catch(function (err) { report(err); return found; });
        });
      });
    }).then(function (rows) {
      var seen = {};
      return (rows || []).filter(function (r) {
        if (!r || !r.file || !r.line || !r.id || r.id === node.id) return false;
        var k = r.file + ':' + r.line;
        if (seen[k]) return false;
        seen[k] = 1;
        return true;
      });
    }).catch(function (err) { report(err); return []; });
  }

  /** Map declaration rows to Monaco locations, preferring the open document. */
  function locsFromRows(model, rows) {
    var lang = ctxRef && ctxRef.lang;
    if (!lang || !model) return Promise.resolve([]);
    var inDoc = [];
    var fileJobs = [];
    (rows || []).forEach(function (r) {
      var word = r.name || labelOf(r);
      if (lang.docLocFor) {
        var doc = lang.docLocFor(model, r.file, r.line, word);
        if (doc) { inDoc.push(Promise.resolve(doc)); return; }
      }
      if (lang.locForFile) {
        fileJobs.push(lang.locForFile(r.file, r.line, word).catch(function () { return null; }));
      }
    });
    if (inDoc.length) return Promise.all(inDoc).then(function (locs) { return locs.filter(Boolean); });
    return Promise.all(fileJobs).then(function (locs) { return locs.filter(Boolean); });
  }

  function provideImplementation(model, position) {
    try {
      var lang = ctxRef && ctxRef.lang;
      if (!lang || !lang.infoAt) return [];
      var info = lang.infoAt(model, position);
      if (!info) return [];
      return resolveAt(info).then(function (def) {
        if (!def || !def.node || !def.node.id) return [];
        return implementationTargets(model, def.node).then(function (rows) {
          return locsFromRows(model, rows);
        });
      }).catch(function (err) { report(err); return []; });
    } catch (e) {
      report(e);
      return [];
    }
  }

  function implementationsAt(model, position) {
    try {
      var lang = ctxRef && ctxRef.lang;
      if (!lang || !lang.infoAt) return Promise.resolve([]);
      var info = lang.infoAt(model, position);
      if (!info) return Promise.resolve([]);
      return resolveAt(info).then(function (def) {
        if (!def || !def.node || !def.node.id) return [];
        return implementationTargets(model, def.node);
      }).catch(function (err) { report(err); return []; });
    } catch (e) {
      report(e);
      return Promise.resolve([]);
    }
  }

  function goToImplementation() {
    if (!editor) return;
    var model = editor.getModel();
    var pos = editor.getPosition();
    if (!model || !pos) return;
    implementationsAt(model, pos).then(function (rows) {
      if (!rows.length) { toast('No implementation found', 'warn'); return; }
      var r = rows[0];
      if (ctxRef && ctxRef.openSource) ctxRef.openSource({ file: r.file, line: r.line, mode: 'raw' });
    });
  }

  // ================================================================== panel
  function injectStyle() {
    if (typeof document === 'undefined' || !document.head) return;
    if (document.getElementById('cs-hier-style')) return;
    var css = document.createElement('style');
    css.id = 'cs-hier-style';
    css.textContent = [
      '.cs-hier { position: fixed; z-index: 60; width: 380px; display: flex; flex-direction: column;',
      '  background: #0f1320; border: 1px solid var(--border-soft, #2a3550); border-radius: 6px;',
      '  box-shadow: 0 12px 30px rgba(0,0,0,.45); font-family: var(--mono, monospace); overflow: hidden; }',
      '.cs-hier-head { display: flex; align-items: center; gap: 6px; padding: 6px 8px;',
      '  border-bottom: 1px solid var(--border-soft, #2a3550); background: #131a2b; }',
      '.cs-hier-title { flex: 1 1 auto; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;',
      '  color: var(--text, #d4d4d4); font-size: 11px; font-weight: 600; }',
      '.cs-hier-sub { color: var(--text-faint, #858585); font-size: 9.5px; font-weight: 400; }',
      '.cs-hier-controls { display: flex; align-items: center; gap: 4px; padding: 4px 8px;',
      '  border-bottom: 1px solid var(--border-soft, #2a3550); }',
      '.cs-hier-select { height: 20px; background: var(--bg-input, #10141f); color: var(--text, #d4d4d4);',
      '  border: 1px solid var(--border, #2a3550); border-radius: 4px; font-family: var(--mono, monospace); font-size: 10px; }',
      '.cs-hier-btn { background: none; border: 1px solid var(--border, #2a3550); border-radius: 4px;',
      '  color: var(--text-faint, #858585); font-size: 10px; cursor: pointer; padding: 1px 5px; }',
      '.cs-hier-btn:hover { color: var(--text, #d4d4d4); background: #1e2740; }',
      '.cs-hier-body { overflow: auto; padding: 3px 0; min-height: 36px; }',
      '.cs-hier-section { padding: 5px 8px 2px; color: var(--text-faint, #858585); font-size: 9px;',
      '  text-transform: uppercase; letter-spacing: .07em; }',
      '.cs-hier-empty { padding: 7px 9px; color: var(--text-faint, #858585); font-size: 10.5px; }',
      '.cs-hier-kind { flex: none; color: var(--type, #4ec9b0); font-size: 9px; border: 1px solid #23414d;',
      '  border-radius: 6px; padding: 0 3px; }'
    ].join('\n');
    document.head.appendChild(css);
  }

  function placePanel() {
    if (!panel || !editor || !editor.getDomNode) return;
    try {
      var rect = editor.getDomNode().getBoundingClientRect();
      var width = Math.max(280, Math.min(420, rect.width - 24));
      var left = Math.max(8, rect.right - width - 16);
      var top = Math.max(8, rect.top + 10);
      var maxH = Math.max(160, Math.min(rect.height - 24, window.innerHeight - top - 16));
      panel.style.width = width + 'px';
      panel.style.left = left + 'px';
      panel.style.top = top + 'px';
      if (panelBody) panelBody.style.maxHeight = Math.max(120, maxH - 64) + 'px';
    } catch (e) { report(e); }
  }

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  }

  function ensurePanel() {
    if (typeof document === 'undefined' || !document.body) return null;
    injectStyle();
    if (panel && document.body.contains(panel)) return panel;
    panel = el('div', 'cs-hier');
    var head = el('div', 'cs-hier-head');
    panelTitle = el('div', 'cs-hier-title', 'Hierarchy');
    var close = el('button', 'cs-hier-btn', '\u2715');
    close.title = 'Close';
    close.addEventListener('click', function () { hide(); });
    head.appendChild(panelTitle);
    head.appendChild(close);

    var controls = el('div', 'cs-hier-controls');
    dirSelect = el('select', 'cs-hier-select');
    [['both', 'Both'], ['forward', 'Outgoing'], ['backward', 'Incoming']].forEach(function (o) {
      var opt = el('option', null, o[1]); opt.value = o[0];
      dirSelect.appendChild(opt);
    });
    dirSelect.value = state.dir;
    dirSelect.addEventListener('change', function () {
      state.dir = dirSelect.value;
      if (state.mode === 'call' && state.nodeId) loadCall(state.nodeId);
    });
    depthSelect = el('select', 'cs-hier-select');
    [1, 2, 3, 4].forEach(function (d) {
      var opt = el('option', null, 'depth ' + d); opt.value = String(d);
      depthSelect.appendChild(opt);
    });
    depthSelect.value = String(state.depth);
    depthSelect.addEventListener('change', function () {
      state.depth = parseInt(depthSelect.value, 10) || 2;
      if (state.mode === 'call' && state.nodeId) loadCall(state.nodeId);
    });
    controls.appendChild(dirSelect);
    controls.appendChild(depthSelect);

    panelBody = el('div', 'cs-hier-body');
    panel.appendChild(head);
    panel.appendChild(controls);
    panel.appendChild(panelBody);
    document.body.appendChild(panel);
    return panel;
  }

  function hide() {
    if (panel) panel.style.display = 'none';
  }

  function show(mode, titleText, subText) {
    ensurePanel();
    if (!panel) return;
    panel.style.display = 'flex';
    state.mode = mode;
    panelTitle.textContent = '';
    panelTitle.appendChild(document.createTextNode(titleText + (subText ? ' ' : '')));
    if (subText) {
      var s = el('span', 'cs-hier-sub', subText);
      panelTitle.appendChild(s);
    }
    var isCall = mode === 'call';
    if (dirSelect) dirSelect.style.display = isCall ? '' : 'none';
    if (depthSelect) depthSelect.style.display = isCall ? '' : 'none';
    state.collapse = {};
    placePanel();
  }

  function edgeChip(edge) {
    var chip = el('span', 'hop-edge', EDGE_LABELS[edge] || edge || '');
    return chip;
  }

  function rowEl(node, depth, path) {
    var wrap = el('div');
    var row = el('button', 'hop');
    row.type = 'button';
    row.style.paddingLeft = (8 + depth * 12) + 'px';

    var kids = node.children || [];
    var collapsed = !!state.collapse[path] && depth > 0;
    var caret = el('span', 'hop-caret', kids.length ? (collapsed ? '\u25b8' : '\u25be') : '\u00b7');
    row.appendChild(caret);

    if (node.edge) row.appendChild(edgeChip(node.edge));
    if (node.line) row.appendChild(el('span', 'hop-line', ':' + node.line));
    var lab = el('span', 'hop-label', node.label || node.name || node.id || '?');
    lab.title = (node.full || node.id || '') + (node.file ? ' \u2014 ' + node.file : '');
    row.appendChild(lab);
    if (node.kind) row.appendChild(el('span', 'cs-hier-kind', node.kind));
    if (node.cycle) row.appendChild(el('span', 'hop-cycle', 'cycle'));

    row.addEventListener('click', function () { openNode(node); });
    if (kids.length) {
      row.addEventListener('dblclick', function (ev) {
        ev.preventDefault();
        state.collapse[path] = !state.collapse[path];
        renderCurrent();
      });
    }
    wrap.appendChild(row);

    if (kids.length && !collapsed) {
      kids.forEach(function (kid, i) {
        wrap.appendChild(rowEl(kid, depth + 1, path + '/' + i + ':' + (kid.id || '')));
      });
    }
    return wrap;
  }

  function openNode(node) {
    if (!node || !node.id) return;
    try { if (ctxRef && ctxRef.openSource) ctxRef.openSource({ node: node.id }); } catch (e) { report(e); }
    try { if (ctxRef && ctxRef.goToNode) ctxRef.goToNode(node.id); } catch (e) { report(e); }
  }

  function section(title, hint) {
    var s = el('div', 'cs-hier-section', title + (hint ? ' \u00b7 ' + hint : ''));
    return s;
  }

  // ------------------------------------------------------------ call rendering
  function renderCall() {
    if (!panelBody) return;
    panelBody.innerHTML = '';
    var d = state.callData;
    if (!d) { panelBody.appendChild(el('div', 'cs-hier-empty', 'No hierarchy loaded.')); return; }
    var any = false;
    (d.sections || []).forEach(function (sec) {
      if (!sec.tree || !sec.tree.root) return;
      any = true;
      var count = (sec.tree.rows || []).length - 1;
      panelBody.appendChild(section(sec.label, count + (count === 1 ? ' hop' : ' hops')));
      panelBody.appendChild(rowEl(sec.tree.root, 0, sec.dir));
      if (sec.tree.truncated) {
        panelBody.appendChild(el('div', 'cs-hier-empty', 'truncated at ' + count + ' hops'));
      }
    });
    if (!any) panelBody.appendChild(el('div', 'cs-hier-empty', 'Nothing linked in this direction.'));
  }

  function loadCall(id) {
    var gen = ++state.generation;
    state.nodeId = id;
    var dirs = state.dir === 'both' ? ['forward', 'backward'] : [state.dir];
    var labels = { forward: 'Outgoing calls', backward: 'Incoming callers' };
    if (!panelBody) return Promise.resolve(null);
    panelBody.innerHTML = '';
    panelBody.appendChild(el('div', 'cs-hier-empty', 'walking\u2026'));
    var jobs = dirs.map(function (dir) {
      return treeFor(id, dir, { depth: state.depth }).then(function (t) {
        return { dir: dir, label: labels[dir] || dir, tree: t };
      });
    });
    return Promise.all(jobs).then(function (sections) {
      if (gen !== state.generation) return null;
      state.callData = { id: id, sections: sections };
      renderCall();
      return state.callData;
    });
  }

  function renderCurrent() {
    if (state.mode === 'type') renderType();
    else renderCall();
  }

  function openCallHierarchy() {
    var info = infoAtCursor();
    if (!info) { toast('Put the cursor on a symbol first', 'warn'); return; }
    show('call', 'Call Hierarchy', info.word);
    state.word = info.word;
    resolveAt(info).then(function (def) {
      if (!def || !def.node || !def.node.id) {
        if (panelBody) panelBody.innerHTML = '<div class="cs-hier-empty">No indexed symbol for "' + esc(info.word) + '".</div>';
        return;
      }
      if (panelTitle) {
        panelTitle.textContent = '';
        panelTitle.appendChild(document.createTextNode('Call Hierarchy '));
        panelTitle.appendChild(el('span', 'cs-hier-sub', labelOf(def.node)));
      }
      loadCall(def.node.id);
    }).catch(function (err) {
      report(err);
      if (panelBody) panelBody.innerHTML = '<div class="cs-hier-empty">Resolve failed: ' + esc(err.message || err) + '</div>';
    });
  }

  // ------------------------------------------------------------ type rendering
  function renderType() {
    if (!panelBody) return;
    panelBody.innerHTML = '';
    var d = typeData;
    if (!d || !d.node) { panelBody.appendChild(el('div', 'cs-hier-empty', 'No type selected.')); return; }
    var root = {
      id: d.node.id, label: labelOf(d.node), kind: d.node.kind,
      file: d.node.file, line: d.node.line || 0, edge: null, cycle: false,
      children: []
    };
    panelBody.appendChild(section('Type'));
    panelBody.appendChild(rowEl(root, 0, 'type-root'));

    var chainKids = ((d.chain && d.chain.root && d.chain.root.children) || []).filter(function (k) {
      return k.edge === 'inherits';   // the chain walk also reports uses/contains
    });
    panelBody.appendChild(section('Base chain', chainKids.length ? chainKids.length + ' bases' : ''));
    if (chainKids.length) {
      chainKids.forEach(function (k) {
        var base = { id: k.id, label: k.label, kind: k.kind, file: k.file, line: k.line, edge: 'inherits', cycle: k.cycle, children: k.children || [] };
        panelBody.appendChild(rowEl(base, 1, 'base:' + k.id));
      });
    } else if ((d.bases || []).length) {
      d.bases.forEach(function (b) {
        panelBody.appendChild(rowEl({ id: b.id, label: labelOf(b), kind: b.kind, file: b.file, line: b.line, edge: 'inherits', cycle: false, children: [] }, 1, 'base:' + b.id));
      });
    } else {
      panelBody.appendChild(el('div', 'cs-hier-empty', 'No base type in this index.'));
    }

    panelBody.appendChild(section('Derived types', (d.derived || []).length ? d.derived.length + ' types' : ''));
    if ((d.derived || []).length) {
      d.derived.forEach(function (b) {
        panelBody.appendChild(rowEl({ id: b.id, label: labelOf(b), kind: b.kind, file: b.file, line: b.line, edge: 'inherited by', cycle: false, children: [] }, 1, 'derived:' + b.id));
      });
    } else {
      panelBody.appendChild(el('div', 'cs-hier-empty', 'Nothing inherits from this type.'));
    }
  }

  function openTypeHierarchy() {
    var info = infoAtCursor();
    if (!info) { toast('Put the cursor on a type first', 'warn'); return; }
    show('type', 'Type Hierarchy', info.word);
    resolveAt(info).then(function (def) {
      if (!def || !def.node || !def.node.id) {
        if (panelBody) panelBody.innerHTML = '<div class="cs-hier-empty">No indexed type for "' + esc(info.word) + '".</div>';
        return;
      }
      return ownerTypeId(def.node).then(function (ownerId) {
        if (!ownerId) { if (panelBody) panelBody.innerHTML = '<div class="cs-hier-empty">Not a type or member of a type.</div>'; return; }
        return typeTreeFor(ownerId).then(function (t) {
          typeData = t;
          if (panelTitle) {
            panelTitle.textContent = '';
            panelTitle.appendChild(document.createTextNode('Type Hierarchy '));
            panelTitle.appendChild(el('span', 'cs-hier-sub', labelOf(t.node || def.node)));
          }
          renderType();
          return t;
        });
      });
    }).catch(function (err) {
      report(err);
      if (panelBody) panelBody.innerHTML = '<div class="cs-hier-empty">Resolve failed: ' + esc(err.message || err) + '</div>';
    });
  }

  // ================================================================ registration
  function registerProvider() {
    if (providerInstalled) return;
    if (!monaco || !monaco.languages || !monaco.languages.registerImplementationProvider) return;
    providerInstalled = true;
    try {
      monaco.languages.registerImplementationProvider('csharp', {
        provideImplementation: provideImplementation
      });
    } catch (e) { report(e); }
  }

  function installActions() {
    if (actionsInstalled || !editor || !monaco) return;
    actionsInstalled = true;
    try {
      editor.addAction({
        id: V + '.callHierarchy',
        label: 'Show Call Hierarchy',
        keybindings: [monaco.KeyMod.Shift | monaco.KeyMod.Alt | monaco.KeyCode.KeyH],
        contextMenuGroupId: 'navigation',
        contextMenuOrder: 1.5,
        run: function () { openCallHierarchy(); }
      });
    } catch (e) { report(e); }
    try {
      editor.addAction({
        id: V + '.typeHierarchy',
        label: 'Show Type Hierarchy',
        contextMenuGroupId: 'navigation',
        contextMenuOrder: 1.6,
        run: function () { openTypeHierarchy(); }
      });
    } catch (e) { report(e); }
    try {
      editor.addAction({
        id: V + '.goToImplementation',
        label: 'Go to Implementation',
        contextMenuGroupId: 'navigation',
        contextMenuOrder: 1.7,
        run: function () { goToImplementation(); }
      });
    } catch (e) { report(e); }
  }

  function attach(ctx) {
    ctxRef = ctx;
    monaco = ctx.monaco;
    editor = ctx.editor;
    installActions();
    registerProvider();
    if (!installed) installed = true;
  }

  // =================================================================== hooks
  window.CsHierarchy = {
    attach: attach,
    treeFor: treeFor,
    typeTreeFor: typeTreeFor,
    provideImplementation: provideImplementation,
    implementationsAt: implementationsAt,
    implementationTargets: implementationTargets,
    openCallHierarchy: openCallHierarchy,
    openTypeHierarchy: openTypeHierarchy,
    goToImplementation: goToImplementation,
    hide: hide,
    panel: function () { return panel; },
    lastError: function () { return lastErr; }
  };

  window.CsEditorExtension({ name: 'hierarchy', attach: attach });
})();
