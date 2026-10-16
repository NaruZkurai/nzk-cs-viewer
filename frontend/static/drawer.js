/* csgraph - the node detail drawer and its relation lists.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== node drawer (VT)

  function loadDetail(id) {
    var seq = ++detailSeq;
    state.relations.loading = true;
    apiGet('/api/node', { id: id })
      .then(function (data) {
        if (seq !== detailSeq) return;
        state.detail = data;
        state.relations.loading = false;
        renderNodeDrawer();
      })
      .catch(function (err) {
        if (seq !== detailSeq) return;
        state.relations.loading = false;
        state.detail = null;
        renderNodeDrawer();
        toast('Could not load node details: ' + err.message, 'error');
      });
  }

  /** Prominent "{ } View code": open the synthesized document for the node. */
  function viewCodeFor(id) {
    if (!id) { toast('Select a node first', 'warn'); return; }
    openSource({ node: id });
  }

  /** File path relative to the indexed root, when it sits underneath it. */
  function shortFile(file) {
    if (!file) return '';
    var root = state.root || '';
    if (root && file.indexOf(root) === 0) {
      var rel = file.slice(root.length).replace(/^[\/\\]+/, '');
      if (rel) return rel;
    }
    return file;
  }

  /** `file:line` / `file:line–endLine` — either long (tooltip) or root-relative. */
  function fileLoc(n, short) {
    if (!n.file) return '';
    var file = short ? shortFile(n.file) : n.file;
    if (!n.line) return file;
    var loc = file + ':' + n.line;
    if (n.endLine && n.endLine !== n.line) loc += '–' + n.endLine;
    return loc;
  }

  // Leading tokens the kind badge or the header title already say. Stripping them
  // lets a type's bare "class Foo" declaration collapse into the header instead of
  // repeating it, while "public class Foo : Bar" keeps its useful bases line.
  var SIG_PREFIX = /^(?:(?:public|private|protected|internal|static|abstract|sealed|partial|readonly|const|virtual|override|new|unsafe|extern|async|external|class|struct|interface|enum|record|namespace|delegate|event|type)\s+)+/i;

  /** The declaration line, or '' when it would only repeat the badge + title. */
  function heroSignature(n) {
    var sig = (n.signature || '').trim();
    if (!sig) return '';
    var rest = sig.replace(/[;{}]+$/, '').replace(SIG_PREFIX, '').trim();
    var names = [n.name, n.label, n.full];
    for (var i = 0; i < names.length; i++) {
      if (names[i] && rest === names[i]) return '';
    }
    return sig;
  }

  function renderNodeDrawer() {
    var d = state.detail;
    if (modeIsFind()) { nodeDrawerEl.classList.add('hidden'); return; }
    if (!d || !d.node) {
      nodeDrawerEl.classList.add('hidden');
      return;
    }
    var n = d.node;
    nodeDrawerEl.classList.remove('hidden');
    // The header is the identity, the crumbs are the ancestry: no repeated full name.
    nodeTitleEl.innerHTML =
      '<i class="drawer-kdot" style="background:' + kindColor(n.kind) + '"></i>' +
      esc(n.name || n.label || n.id);
    nodeTitleEl.title = n.full || n.id;
    renderCrumbs();

    // The declaration line already carries access/static/return type, so the chips
    // only show what it cannot: graph degree, literal/external provenance, and the
    // modifiers themselves when there is no signature to read them from.
    var flags = '';
    if (!n.signature) {
      if (n.static) flags += '<span class="flag">static</span>';
      if (n.access) flags += '<span class="flag">' + esc(n.access) + '</span>';
    }
    if (n.literal) flags += '<span class="flag">literal</span>';
    if (n.external) flags += '<span class="flag">external</span>';
    if (n.degree !== undefined && n.degree !== null) flags += '<span class="flag">deg ' + esc(n.degree) + '</span>';

    var loc = fileLoc(n, true);
    var locFull = fileLoc(n, false);
    var hero = heroSignature(n);

    nodePropsEl.innerHTML =
      '<div class="detail-head">' +
        '<span class="kind-badge" style="background:' + kindColor(n.kind) + '">' + esc(n.kind || '?') + '</span>' +
        (flags ? '<span class="detail-flags">' + flags + '</span>' : '') +
      '</div>' +
      (hero ? '<div class="detail-sig">' + esc(hero) + '</div>' : '') +
      (loc ? '<div class="detail-loc mono-sig" title="' + esc(locFull) + '">' + esc(loc) + '</div>' : '') +
      '<div class="prop-actions">' +
        '<button class="btn btn-ghost" data-act="expand" type="button" title="Add every relation bucket">Expand</button>' +
        '<button class="btn btn-ghost" data-act="hidech" type="button" title="Hide or show the contains children of this node">' +
          (state.hiddenChildren.has(n.id) ? 'Show children' : 'Hide children') + '</button>' +
        (n.file && n.line ? '<button class="btn btn-ghost" data-act="chain" type="button" title="Walk the resolve chain from this declaration">Resolve chain</button>' : '') +
      '</div>';

    renderRelations();
  }

  // The relation list can be browsed without touching the map: the caret (and the
  // label) open it, the ± box and the count add the whole bucket to the graph.
  function relationListOpen(nodeId, key) {
    var s = state.relations.listOpenByNode.get(nodeId);
    return !!(s && s.has(key));
  }

  function openRelationList(nodeId, key) {
    var s = state.relations.listOpenByNode.get(nodeId);
    if (!s) { s = new Set(); state.relations.listOpenByNode.set(nodeId, s); }
    s.add(key);
  }

  function toggleRelationList(relation) {
    var nodeId = state.selected;
    if (!nodeId || !relation) return;
    var s = state.relations.listOpenByNode.get(nodeId);
    if (!s) { s = new Set(); state.relations.listOpenByNode.set(nodeId, s); }
    if (s.has(relation.key)) s.delete(relation.key);
    else s.add(relation.key);
    renderRelations();
  }

  function relationChildOwner(nodeId, key, childId) {
    return nodeId + '|' + key + '|' + childId;
  }

  /** Neighbours known for a bucket: the /api/node sample plus any paged-in extras. */
  function relationNodesAll(r) {
    return r._more ? r.nodes.concat(r._more) : (r.nodes || []);
  }

  /** "show more" — reveal in-memory rows first, then page the rest from the server. */
  function loadMoreRelation(r, nodeId) {
    var all = relationNodesAll(r);
    var shown = r._shown || 200;
    if (all.length > shown) {
      r._shown = shown + 200;
      renderRelations();
      return;
    }
    apiGet('/api/relation', { node: nodeId, key: r.key, offset: all.length, limit: 200 })
      .then(function (page) {
        r._more = (r._more || []).concat(page.nodes || []);
        if (typeof page.total === 'number') r._total = page.total;
        r._shown = shown + 200;
        renderRelations();
      })
      .catch(function (err) {
        toast('Could not load more of ' + (r.label || r.key) + ': ' + err.message, 'error');
      });
  }

  /** Per-child ±: put just this neighbour on the map (or take it off again). */
  function toggleRelationChild(relation, nb) {
    var nodeId = state.selected;
    if (!nodeId || !nb) return;
    var owner = relationChildOwner(nodeId, relation.key, nb.id);
    var set = state.nodeOwners.get(nb.id);
    if (set && set.has(owner)) {
      dropOwner(owner);
    } else {
      var added = addNodes([nb], owner);
      addEdges([relationEdge(nodeId, relation, nb)], owner);
      buildModel();
      seedNewNodes(added);
    }
    openRelationList(nodeId, relation.key);
    quickLayout();
    renderRelations();
    updateCanvasNotice();
  }

  function renderRelations() {
    var d = state.detail;
    relationsBodyEl.innerHTML = '';
    if (!d || !d.node) { relationsCountEl.textContent = ''; return; }
    var rels = d.relations || [];
    var total = 0;
    rels.forEach(function (r) { total += r.count || 0; });
    relationsCountEl.textContent = total ? fmtNum(total) : '';

    if (!rels.length) {
      relationsBodyEl.innerHTML = '<div class="drawer-hint">No relations for this node.</div>';
      return;
    }

    var nodeId = d.node.id;
    var frag = document.createDocumentFragment();
    rels.forEach(function (r) {
      var active = relationIsActive(r);
      var open = relationListOpen(nodeId, r.key);
      var all = relationNodesAll(r);
      var hasNodes = all.length > 0;

      var row = document.createElement('div');
      row.className = 'relationship' + (active ? ' active' : '');
      row.setAttribute('data-key', r.key);

      // ± on the left: add / remove the whole bucket on the map.
      var tog = document.createElement('span');
      tog.className = 'item rel-toggle';
      tog.textContent = active ? '−' : '+';
      tog.title = (active ? 'Remove all ' : 'Add all ') + (r.label || r.key) +
                  ' to the graph';
      tog.addEventListener('click', function (ev) {
        ev.stopPropagation();
        toggleRelation(r);
      });

      // caret immediately left of the name: show / hide the list only.
      var caret = document.createElement('span');
      caret.className = 'item rel-caret';
      caret.textContent = open ? '▾' : '▸';
      caret.title = 'Show/hide ' + (r.label || r.key) + ' (does not touch the graph)';
      if (hasNodes) {
        caret.addEventListener('click', function (ev) {
          ev.stopPropagation();
          toggleRelationList(r);
        });
      } else {
        caret.classList.add('rel-caret-empty');
      }

      var label = document.createElement('span');
      label.className = 'item rel-label';
      label.textContent = r.label || r.key;
      if (hasNodes) label.addEventListener('click', function () { toggleRelationList(r); });
      if (r.key === 'contains:out' && state.hiddenChildren.has(nodeId)) {
        row.classList.add('hidden-children');
        var hid = document.createElement('span');
        hid.className = 'item rel-hidden';
        hid.textContent = 'hidden';
        label.appendChild(hid);
      }

      // count stays on the right; clicking it also expands the whole bucket.
      var count = document.createElement('span');
      count.className = 'item num-results';
      count.textContent = (r.count > 999 ? '999+' : String(r.count)) + (r.truncated ? '+' : '');
      count.title = 'Add all ' + (r.label || r.key) + ' to the graph';
      count.addEventListener('click', function (ev) {
        ev.stopPropagation();
        toggleRelation(r);
      });

      row.appendChild(tog);
      row.appendChild(caret);
      row.appendChild(label);
      row.appendChild(count);
      frag.appendChild(row);

      if (open && hasNodes) {
        var list = document.createElement('div');
        list.className = 'rel-nodes';
        var shown = r._shown || 200;
        all.slice(0, shown).forEach(function (nb) {
          var b = document.createElement('button');
          b.type = 'button';
          b.className = 'rel-node';
          b.title = nb.full || nb.id;
          // while the bucket is not on the map, each row can add just itself
          if (!active) {
            var owner = relationChildOwner(nodeId, r.key, nb.id);
            var own = state.nodeOwners.get(nb.id);
            var on = !!(own && own.has(owner));
            var ctog = document.createElement('span');
            ctog.className = 'child-toggle' + (on ? ' on' : '');
            ctog.textContent = on ? '−' : '+';
            ctog.title = on ? 'Remove from the graph' : 'Add just this one to the graph';
            ctog.addEventListener('click', function (ev) {
              ev.stopPropagation();
              toggleRelationChild(r, nb);
            });
            b.appendChild(ctog);
          }
          var dot = document.createElement('span');
          dot.className = 'dot';
          dot.style.background = kindColor(nb.kind);
          var lab = document.createElement('span');
          lab.className = 'rn-label';
          lab.textContent = nb.label || nb.name || nb.id;
          var kd = document.createElement('span');
          kd.className = 'rn-kind';
          kd.textContent = nb.kind || '';
          b.appendChild(dot); b.appendChild(lab); b.appendChild(kd);
          b.addEventListener('click', function (ev) {
            ev.stopPropagation();
            selectNode(nb.id);
            followNode(nb.id, null);
          });
          list.appendChild(b);
        });
        var totalKnown = (typeof r._total === 'number') ? r._total : null;
        var moreInMemory = all.length > shown;
        var moreOnServer = (totalKnown !== null) ? (all.length < totalKnown) : !!r.truncated;
        if (moreInMemory || moreOnServer) {
          var remain = moreInMemory
            ? (all.length - shown)
            : (totalKnown !== null ? (totalKnown - all.length) : 'more');
          var more = document.createElement('button');
          more.type = 'button';
          more.className = 'rel-more';
          more.title = 'Load more entries in this relation bucket';
          more.textContent = '+ show ' + (remain === 'more' ? 'more' : fmtNum(remain) + ' more');
          more.addEventListener('click', function (ev) {
            ev.stopPropagation();
            loadMoreRelation(r, nodeId);
          });
          list.appendChild(more);
        }
        frag.appendChild(list);
      }
    });
    relationsBodyEl.appendChild(frag);
  }

