/* csgraph v2 — CodeLens over the synthesized C# document.
 *
 * Part of the RUST-ONLY v2 viewer. Not loaded by v1.
 *
 * The engine already knows, per declaration, how many callers and references
 * the graph holds (`/api/node` detail: callers/usedBy counts and the node's own
 * declaration sites). `/api/decls` says which declarations the open document
 * contains, with the exact (file, line) each one starts on. This file is the
 * thin bridge between those two: it maps the declaration rows onto Monaco lines
 * (the document numbers its lines itself — Monaco line i is record i, whose
 * `src` carries the real file/line) and puts a lens on each declaration's first
 * line:
 *
 *     "12 references · 3 callers · show in graph"
 *
 * Clicking the counts moves the cursor onto the declaration name and runs
 * Monaco's own reference search; "show in graph" selects the node in the graph.
 *
 * The provider is deliberately cheap: `provideCodeLenses` only ever answers from
 * cache (or the lightweight `degree` on the decl row) and kicks the `/api/decls`
 * fetch in the background, firing `onDidChange` when it lands. `/api/node` — the
 * expensive one — is fetched only through `resolveCodeLens`, which Monaco calls
 * for the lenses that are actually on screen, and every node id is cached.
 */
'use strict';

(function () {
  var V = 'csgraph.codelens';
  var MAX_DECLS = 400;          // never build more lenses than this in one pass
  var DECLS_TTL_MS = 30000;

  var monaco = null;
  var editor = null;
  var ctxRef = null;
  var providerInstalled = false;
  var commandsInstalled = false;
  var lastErr = null;

  var nodeCache = {};           // node id -> Promise<detail>
  var declCache = {};           // key id -> {promise, rows, at}
  var recCache = { model: null, map: null };

  function report(e) {
    lastErr = e;
    if (window.console && console.error) console.error('[csgraph codelens]', e);
  }

  function api(path, params) {
    var fn = (ctxRef && ctxRef.apiGet) || window.apiGet;
    if (typeof fn !== 'function') return Promise.reject(new Error('apiGet unavailable'));
    return fn(path, params);
  }

  function num(v) {
    var n = typeof v === 'number' ? v : parseInt(v, 10);
    return isFinite(n) ? n : 0;
  }

  function fmt(v) {
    var fn = (ctxRef && ctxRef.fmtNum) || window.fmtNum;
    return typeof fn === 'function' ? fn(num(v)) : String(num(v));
  }

  function countItems(items) {
    var total = 0;
    (items || []).forEach(function (it) {
      if (!it) return;
      var c = num(it.count);
      if (c > 0) { total += c; return; }
      var lines = it.lines || [];
      if (lines.length) { total += lines.length; return; }
      total += 1;
    });
    return total;
  }

  // ---------------------------------------------------------------- document key
  /* The document is one of three things: a `csgraph://file/…` model (Monaco
   * peek/rename buffers — real line numbers), a raw file window (`state.source`
   * doc=false, file set), or the synthesized document for a node (`doc=true`).
   * Each needs a different `/api/decls` argument and a different line mapping. */
  function sourceKey(model) {
    if (!model) return null;
    var lang = ctxRef && ctxRef.lang;
    if (lang && lang.fileOfModel) {
      var file = lang.fileOfModel(model);
      if (file) return { kind: 'file', file: file, isFileModel: true, id: 'file:' + file };
    }
    var st = ctxRef && ctxRef.state && ctxRef.state.source;
    if (st) {
      if (st.doc && st.nodeId) return { kind: 'node', node: st.nodeId, id: 'node:' + st.nodeId };
      if (!st.doc && st.file) return { kind: 'file', file: st.file, isFileModel: false, id: 'file:' + st.file };
    }
    return null;
  }

  function loadDecls(key) {
    var e = declCache[key.id];
    if (e) {
      if (e.rows) {
        if (Date.now() - e.at < DECLS_TTL_MS) return Promise.resolve(e.rows);
      } else if (e.promise) {
        return e.promise;
      }
    }
    e = declCache[key.id] = { rows: null, at: 0, promise: null };
    var params = key.kind === 'node' ? { node: key.node } : { file: key.file };
    e.promise = api('/api/decls', params).then(function (d) {
      e.rows = (d && d.decls) || [];
      e.at = Date.now();
      return e.rows;
    }).catch(function (err) {
      report(err);
      e.rows = [];
      e.at = Date.now();
      return e.rows;
    }).then(function (rows) {
      if (emitter) emitter.fire();
      return rows;
    });
    return e.promise;
  }

  // ------------------------------------------------------------------- emitter
  var listeners = [];
  var emitter = {
    event: function (cb) {
      listeners.push(cb);
      return {
        dispose: function () {
          var i = listeners.indexOf(cb);
          if (i >= 0) listeners.splice(i, 1);
        }
      };
    },
    fire: function () {
      listeners.slice().forEach(function (cb) { try { cb(emitter); } catch (e) { report(e); } });
    }
  };

  // -------------------------------------------------------------- line mapping
  /* Monaco line for a declaration's (file, line). For the synthesized document
   * every record carries its real `src`; build the reverse map once per model. */
  /* A source line can appear more than once in a synthesized document (a head
   * section repeats part of the body; an #if branch is emitted twice), so the
   * *first* record for (file, line) wins — the same rule CsLang.docLinesFor
   * uses, and the one that lands on the declaration rather than a duplicate. */
  function recordIndex(model) {
    if (recCache.model === model && recCache.map) return recCache.map;
    var map = {};
    var lines = (ctxRef && ctxRef.lines ? ctxRef.lines() : []) || [];
    for (var i = 0; i < lines.length; i++) {
      var src = lines[i] && lines[i].src;
      if (!src || !src.file) continue;
      var k = src.file + ':' + src.line;
      if (!(k in map)) map[k] = i + 1;
    }
    recCache = { model: model, map: map };
    return map;
  }

  function lineFor(model, key, decl) {
    if (!decl || !decl.line) return 0;
    if (key.isFileModel) return decl.file === key.file ? num(decl.line) : 0;
    var idx = recordIndex(model);
    return idx[decl.file + ':' + decl.line] || 0;
  }

  // -------------------------------------------------------------------- counts
  /* Fallback when `/api/node` has not been fetched: the decl row itself carries
   * a `degree`, which is the number of distinct graph neighbours. */
  function countsFromDecl(decl) {
    return { refs: num(decl && decl.degree), callers: 0, resolved: false };
  }

  function countsFor(detail, decl) {
    if (detail && detail.ok !== false && detail.node) {
      var refs = countItems(detail.usedBy);
      var parts = detail.node.parts || [];
      refs += parts.length;
      return { refs: refs, callers: countItems(detail.callers), resolved: true };
    }
    return countsFromDecl(decl);
  }

  function title(c) {
    return fmt(c.refs) + ' references \u00b7 ' + fmt(c.callers) + ' callers \u00b7 show in graph';
  }

  function nodeDetail(id) {
    if (nodeCache[id]) return nodeCache[id];
    nodeCache[id] = api('/api/node', { id: id }).then(function (d) {
      return d || { ok: false };
    }).catch(function (err) {
      report(err);
      return { ok: false };
    });
    return nodeCache[id];
  }

  function labelOf(row) {
    return (row && (row.name || row.label || row.full || row.id)) || '';
  }

  // ---------------------------------------------------------------- lenses
  function buildLenses(model, key, rows) {
    var out = [];
    var used = {};
    var placed = 0;
    var lineCount = model.getLineCount ? model.getLineCount() : 0;
    for (var i = 0; i < rows.length; i++) {
      if (placed >= MAX_DECLS) break;
      var row = rows[i];
      if (!row || !row.id || !row.line) continue;
      var ln = lineFor(model, key, row);
      if (ln < 1 || ln > lineCount || used[ln]) continue;
      used[ln] = 1;
      placed++;
      var range = new monaco.Range(ln, 1, ln, 1);
      var c = countsFromDecl(row);
      out.push({
        range: range,
        id: key.id + '#' + ln,
        _kind: 'main',
        _nodeId: row.id,
        _decl: row,
        command: {
          id: V + '.showInGraph',
          title: title(c),
          arguments: [row.id]
        }
      });
      out.push({
        range: range,
        id: key.id + '#' + ln + ':refs',
        _kind: 'references',
        _nodeId: row.id,
        _decl: row,
        command: {
          id: V + '.references',
          title: 'references',
          arguments: [row.id, row.file, row.line, labelOf(row)]
        }
      });
    }
    return out;
  }

  function resolveLens(model, lens) {
    try {
      if (!lens || !lens._nodeId) return Promise.resolve(lens);
      return nodeDetail(lens._nodeId).then(function (detail) {
        if (!detail || detail.ok === false || !detail.node) return lens;
        var c = countsFor(detail, lens._decl);
        lens._counts = c;
        if (lens._kind === 'main' && lens.command) lens.command.title = title(c);
        return lens;
      }).catch(function (err) { report(err); return lens; });
    } catch (e) {
      report(e);
      return Promise.resolve(lens);
    }
  }

  // ---------------------------------------------------------------- provider
  function provideCodeLenses(model) {
    try {
      var key = sourceKey(model);
      if (!key) return { lenses: [] };
      var e = declCache[key.id];
      if (e && e.rows && Date.now() - e.at < DECLS_TTL_MS) {
        return { lenses: buildLenses(model, key, e.rows) };
      }
      loadDecls(key);   // async; fires onDidChange, provider re-asks
      return { lenses: [] };
    } catch (err) {
      report(err);
      return { lenses: [] };
    }
  }

  function resolveCodeLens(model, lens) {
    return resolveLens(model, lens);
  }

  function registerProvider() {
    if (providerInstalled) return;
    if (!monaco || !monaco.languages || !monaco.languages.registerCodeLensProvider) return;
    providerInstalled = true;
    monaco.languages.registerCodeLensProvider('csharp', {
      onDidChange: function (cb) { return emitter.event(cb); },
      provideCodeLenses: provideCodeLenses,
      resolveCodeLens: resolveCodeLens
    });
  }

  // ----------------------------------------------------------------- commands
  function wordPosition(model, word, fallbackLine) {
    if (!model || !word) return fallbackLine || 1;
    var n = model.getLineCount ? model.getLineCount() : 0;
    for (var i = 1; i <= n; i++) {
      var text = model.getLineContent ? model.getLineContent(i) : '';
      if (text && text.indexOf(word) >= 0) return i;
    }
    return fallbackLine || 1;
  }

  function showReferences(nodeId, file, line, word) {
    if (!editor || !monaco) return;
    var model = editor.getModel();
    if (!model) return;
    var key = sourceKey(model);
    var ln = 0;
    if (word) ln = wordPosition(model, word, 0);
    if (!ln && key && key.isFileModel && line) ln = num(line);
    if (!ln && key && key.id && declCache[key.id] && declCache[key.id].rows) {
      var rows = declCache[key.id].rows;
      for (var i = 0; i < rows.length; i++) {
        if (rows[i].id === nodeId) { ln = lineFor(model, key, rows[i]); break; }
      }
    }
    if (!ln) {
      if (ctxRef && ctxRef.toast) ctxRef.toast('Declaration is not on screen', 'warn');
      return;
    }
    var col = 1;
    var text = model.getLineContent ? model.getLineContent(ln) : '';
    var at = word ? text.indexOf(word) : -1;
    if (at >= 0) col = at + 1;
    try {
      editor.setPosition({ lineNumber: ln, column: col });
      editor.focus();
      editor.trigger('codelens', 'editor.action.referenceSearch.trigger', {});
    } catch (e) { report(e); }
  }

  function installCommands() {
    if (commandsInstalled || !editor) return;
    commandsInstalled = true;
    try {
      editor.addAction({
        id: V + '.showInGraph',
        label: 'Show in Graph',
        run: function (ed, nodeId) {
          if (nodeId && ctxRef && ctxRef.goToNode) ctxRef.goToNode(nodeId);
        }
      });
    } catch (e) { report(e); }
    try {
      editor.addAction({
        id: V + '.references',
        label: 'Show References',
        run: function (ed, nodeId, file, line, word) {
          showReferences(nodeId, file, line, word);
        }
      });
    } catch (e) { report(e); }
  }

  // ------------------------------------------------------------------- attach
  function attach(ctx) {
    ctxRef = ctx;
    monaco = ctx.monaco;
    editor = ctx.editor;
    registerProvider();
    installCommands();
    // Warm the decl cache for the document as soon as it is shown.
    if (ctx.on) {
      ctx.on('model', function () {
        var model = ctx.model ? ctx.model() : (editor && editor.getModel());
        var key = sourceKey(model);
        if (key) loadDecls(key);
      });
    }
  }

  // --------------------------------------------------------------- test hooks
  function lensesFor(model) {
    try {
      model = model || (editor && editor.getModel());
      var key = sourceKey(model);
      if (!key || !model) return Promise.resolve([]);
      return loadDecls(key).then(function (rows) {
        var lenses = buildLenses(model, key, rows);
        return Promise.all(lenses.map(function (l) { return resolveLens(model, l); }))
          .then(function (resolved) {
            return resolved.map(function (l) {
              return {
                line: l.range ? l.range.startLineNumber : 0,
                kind: l._kind,
                nodeId: l._nodeId,
                title: l.command ? l.command.title : '',
                commands: l.command ? [l.command.id] : [],
                counts: l._counts || countsFromDecl(l._decl)
              };
            });
          });
      }).catch(function (err) { report(err); return []; });
    } catch (e) {
      report(e);
      return Promise.resolve([]);
    }
  }

  function clearCache() {
    nodeCache = {};
    declCache = {};
    recCache = { model: null, map: null };
  }

  window.CsCodeLens = {
    attach: attach,
    provider: {
      provideCodeLenses: provideCodeLenses,
      resolveCodeLens: resolveCodeLens
    },
    lensesFor: lensesFor,
    countsFor: countsFor,
    countsFromDecl: countsFromDecl,
    sourceKey: sourceKey,
    nodeDetail: nodeDetail,
    clearCache: clearCache,
    installed: function () { return providerInstalled; },
    lastError: function () { return lastErr; }
  };

  window.CsEditorExtension({ name: 'codelens', attach: attach });
})();
