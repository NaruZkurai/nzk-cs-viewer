/* csgraph v2 — document outline, folding and occurrence highlight.
 *
 * Part of the RUST-ONLY v2 viewer. Registers through `window.CsEditorExtension`
 * (see editor.js); editor.js/language.js are untouched.
 *
 * Three Monaco providers share one engine query, `/api/decls`:
 *   documentSymbols     Ctrl+Shift+O outline, breadcrumbs and — because
 *                       editor.js set stickyScroll.defaultModel='outlineModel'
 *                       — the sticky-scroll header
 *   foldingRanges       one fold per declaration span
 *   documentHighlights  the word under the cursor, resolved through the graph
 *
 * `/api/decls?node=<id>` returns the node plus every declaration nested through
 * containment edges; `/api/decls?file=<path>` returns every declaration in a
 * file. Rows are `lite` node objects in document order. Turning a row into a
 * Monaco line means matching its real `(file, line)` against the open model's
 * line records (`ctx.lines()` / `ctx.recordAt`), because a synthesized document
 * interleaves several files and skips `#if`-inactive lines.
 */
'use strict';

(function () {
  var installed = false;          // Monaco providers are global
  var ctx = null;
  var monacoRef = null;
  var modelSubscribed = false;

  var declCache = {};             // model key -> Promise<row[]>
  var declKeys = [];
  var DECL_CACHE_MAX = 24;
  var MAX_SYMBOLS = 5000;
  var MAX_FOLDS = 5000;
  var MAX_HIGHLIGHTS = 2000;
  var MAX_SITES = 400;

  var lastSymbolsValue = null;
  var lastDeclsValue = [];
  var lastError = null;
  var anonSeq = 0;

  // ------------------------------------------------------------------ helpers

  function srcState() {
    return (ctx && ctx.state) || window.state || null;
  }

  function lang() {
    return (ctx && ctx.lang) || window.CsLang || null;
  }

  function clampLine(model, n) {
    var max = model.getLineCount();
    if (!n || n < 1) return 1;
    return n > max ? max : n;
  }

  function modelKey(model) {
    try {
      if (model && model.uri && typeof model.uri.toString === 'function') return 'u:' + model.uri.toString();
      if (model && model.id) return 'i:' + model.id;
    } catch (e) { /* fall through */ }
    try {
      if (!model.__csDeclKey) model.__csDeclKey = 'm:' + (++anonSeq);
      return model.__csDeclKey;
    } catch (e2) {
      return 'm:anon';
    }
  }

  /** The node whose synthesized document is open, or null for a file window. */
  function docNodeId() {
    var st = srcState();
    var s = st && st.source;
    if (!s) return null;
    if (s.nodeId) return s.nodeId;
    if (s.usageNode && s.usageNode.id) return s.usageNode.id;
    var p = s.payload;
    if (p && p.node) {
      if (typeof p.node === 'string') return p.node;
      if (p.node.id) return p.node.id;
    }
    return null;
  }

  function fileOfModel(model) {
    var l = lang();
    if (!l || typeof l.fileOfModel !== 'function' || !model) return null;
    try { return l.fileOfModel(model) || null; } catch (e) { return null; }
  }

  /**
   * The declarations behind a model, fetched once per model and shared with
   * semantics.js through `window.CsSymbols.declsFor`.
   */
  function declsFor(model) {
    if (!model) return Promise.resolve([]);
    var key = modelKey(model);
    if (declCache[key]) return declCache[key];
    var get = (ctx && ctx.apiGet) || window.apiGet || null;
    var file = fileOfModel(model);
    var params = null;
    if (file) {
      params = { file: file };
    } else {
      var id = docNodeId();
      var st = srcState();
      var s = st && st.source;
      if (id) params = { node: id };
      else if (s && s.file) params = { file: s.file };
    }
    var p;
    if (!get || !params) {
      p = Promise.resolve([]);
    } else {
      try {
        p = Promise.resolve(get('/api/decls', params)).then(function (d) {
          return (d && d.decls) || [];
        }).catch(function () { return []; });
      } catch (e) {
        p = Promise.resolve([]);
      }
    }
    if (declKeys.length >= DECL_CACHE_MAX) {
      var drop = declKeys.shift();
      if (drop) delete declCache[drop];
    }
    declKeys.push(key);
    declCache[key] = p;
    return p;
  }

  // ----------------------------------------------------- (file,line) -> model

  /**
   * Map `file:line` -> Monaco line for one model.
   *
   * A `csgraph://` file model has real file line numbers, so mapping is the
   * identity. Otherwise the open document's records carry `src`, and a raw /
   * effective file window carries `n` (the real line) plus the pane's file.
   */
  function lineIndexFor(model) {
    var file = fileOfModel(model);
    var idx = {
      map: {},
      identity: !!file,
      file: file || '',
      window: false,
      start: 1,
      lineCount: model.getLineCount(),
      model: model
    };
    if (file) return idx;

    var lines = (ctx && ctx.lines) ? ctx.lines() : [];
    var st = srcState();
    var s = st && st.source;
    var windowFile = (s && !s.doc && s.file) ? s.file : '';
    idx.window = !!windowFile;
    if (windowFile && s && s.start) idx.start = s.start;
    for (var i = 0; i < lines.length; i++) {
      var rec = lines[i];
      if (!rec) continue;
      var src = rec.src;
      if (src && src.file && src.line) {
        idx.map[src.file + ':' + src.line] = i + 1;
      } else if (rec.n && windowFile) {
        idx.map[windowFile + ':' + rec.n] = i + 1;
      }
    }
    return idx;
  }

  /** Monaco start line for a decl row, or null when it is not on screen. */
  function startLineFor(idx, row) {
    if (!row) return null;
    var line = row.line || 0;
    if (row.file && idx.map[row.file + ':' + line] != null) return idx.map[row.file + ':' + line];
    if (idx.identity) return clampLine(idx.model, line);
    var st = srcState();
    var openFile = st && st.source && !st.source.doc ? st.source.file : '';
    if (idx.window && (!row.file || row.file === idx.file || row.file === openFile)) {
      var mapped = line - idx.start + 1;
      if (mapped >= 1 && mapped <= idx.lineCount) {
        // Only trust the offset fallback when the record at that line really is
        // (or contains) the declaration; a filtered window skips lines, so a
        // bare offset can otherwise land on a different declaration's line.
        var rec = (ctx && typeof ctx.recordAt === 'function') ? ctx.recordAt(mapped) : null;
        if (rec && rec.n === line) return mapped;
        if (!rec || !row.name) {
          var text = idx.model.getLineContent(mapped) || '';
          if (!row.name || text.indexOf(row.name) >= 0) return mapped;
        }
      }
    }
    return null;
  }

  function endLineFor(idx, row, start) {
    if (!row) return start;
    var end = row.endLine || row.line || start;
    if (row.file && idx.map[row.file + ':' + end] != null) return idx.map[row.file + ':' + end];
    if (idx.identity) return clampLine(idx.model, end);
    return Math.min(idx.lineCount, start + Math.max(0, end - (row.line || end)));
  }

  // ----------------------------------------------------------------- symbols

  function symbolKind(kind) {
    var K = monacoRef && monacoRef.languages && monacoRef.languages.SymbolKind;
    if (!K) return 0;
    switch (kind) {
      case 'namespace': return K.Namespace;
      case 'class': case 'record': return K.Class;
      case 'struct': return K.Struct;
      case 'interface': return K.Interface;
      case 'enum': return K.Enum;
      case 'method': case 'constructor': case 'operator': return K.Method;
      case 'property': return K.Property;
      case 'field': return K.Field;
      case 'event': return K.Event;
      case 'enumMember': return K.EnumMember;
      default: return K.Variable;
    }
  }

  function displayName(row) {
    return row.label || row.full || row.name || row.id || 'symbol';
  }

  /**
   * Nest flat decl rows via `owner`/`parent` (both are fully-qualified owner
   * names, not ids) and emit Monaco DocumentSymbols in document order. A row
   * whose line is not on screen is dropped and its children are promoted.
   */
  function buildSymbols(model, rows) {
    rows = (rows || []).slice(0, MAX_SYMBOLS);
    var idx = lineIndexFor(model);
    var n = rows.length;
    var byFull = {};
    var byId = {};
    var i;
    for (i = 0; i < n; i++) {
      if (rows[i].full) byFull[rows[i].full] = i;
      if (rows[i].id) byId[rows[i].id] = i;
    }

    var parent = new Array(n);
    for (i = 0; i < n; i++) {
      var row = rows[i];
      var p = -1;
      if (row.owner && byFull[row.owner] !== undefined && byFull[row.owner] !== i) {
        p = byFull[row.owner];
      } else if (row.parent) {
        if (byId[row.parent] !== undefined && byId[row.parent] !== i) p = byId[row.parent];
        else if (byFull[row.parent] !== undefined && byFull[row.parent] !== i) p = byFull[row.parent];
      }
      parent[i] = p;
    }
    // Cut cycles (malformed owner chains must not hang the outline).
    for (i = 0; i < n; i++) {
      var seen = {};
      var q = i;
      var guard = 0;
      while (q >= 0 && guard++ <= n) {
        if (seen[q]) { parent[i] = -1; break; }
        seen[q] = 1;
        q = parent[q];
      }
    }

    var children = [];
    for (i = 0; i < n; i++) children.push([]);
    var roots = [];
    for (i = 0; i < n; i++) {
      if (parent[i] >= 0) children[parent[i]].push(i);
      else roots.push(i);
    }

    function makeSymbol(row, start, end) {
      var endLine = end < start ? start : end;
      var maxCol;
      try { maxCol = model.getLineMaxColumn(endLine); } catch (e) { maxCol = 1; }
      var endCol;
      try { endCol = model.getLineLength(start) + 1; } catch (e2) { endCol = 1; }
      var R = monacoRef.Range;
      return {
        name: displayName(row),
        detail: row.kind || '',
        kind: symbolKind(row.kind),
        tags: [],
        range: new R(start, 1, endLine, maxCol),
        selectionRange: new R(start, 1, start, endCol)
      };
    }

    function buildList(list, depth) {
      var out = [];
      if (depth > 12) return out;
      for (var k = 0; k < list.length; k++) {
        var row = rows[list[k]];
        var start = startLineFor(idx, row);
        if (start === null) {
          // Not visible in this model: keep its children under the same parent.
          out = out.concat(buildList(children[list[k]], depth));
          continue;
        }
        var sym = makeSymbol(row, start, endLineFor(idx, row, start));
        sym.children = buildList(children[list[k]], depth + 1);
        out.push(sym);
      }
      return out;
    }

    return buildList(roots, 0);
  }

  function provideDocumentSymbols(model) {
    if (!model || !monacoRef) return Promise.resolve([]);
    return declsFor(model).then(function (rows) {
      lastDeclsValue = rows || [];
      try {
        lastSymbolsValue = buildSymbols(model, rows);
        lastError = null;
        return lastSymbolsValue;
      } catch (e) {
        lastError = e;
        lastSymbolsValue = [];
        return [];
      }
    }).catch(function () {
      lastSymbolsValue = [];
      return [];
    });
  }

  // ----------------------------------------------------------------- folding

  function provideFoldingRanges(model) {
    if (!model || !monacoRef) return Promise.resolve([]);
    return declsFor(model).then(function (rows) {
      var idx = lineIndexFor(model);
      var out = [];
      var seen = {};
      for (var i = 0; i < rows.length && out.length < MAX_FOLDS; i++) {
        var row = rows[i];
        var start = startLineFor(idx, row);
        if (start === null) continue;
        var end = endLineFor(idx, row, start);
        if (!end || end <= start) continue;
        var key = start + ':' + end;
        if (seen[key]) continue;
        seen[key] = 1;
        out.push({ start: start, end: end, kind: 'region' });
      }
      out.sort(function (a, b) { return a.start - b.start || b.end - a.end; });
      return out;
    }).catch(function () { return []; });
  }

  // ---------------------------------------------------------------- highlight

  function escapeRegExp(word) {
    return String(word).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  }

  function hlKind() {
    var D = monacoRef && monacoRef.languages && monacoRef.languages.DocumentHighlightKind;
    return D && D.Text !== undefined ? D.Text : 2;
  }

  function pushMatch(out, seen, line, col, len) {
    var key = line + ':' + col;
    if (seen[key]) return;
    seen[key] = 1;
    out.push({
      range: new monacoRef.Range(line, col, line, col + len),
      kind: hlKind()
    });
  }

  function scanModelForWord(model, word, out, seen) {
    var re = new RegExp('\\b' + escapeRegExp(word) + '\\b', 'g');
    var n = model.getLineCount();
    for (var i = 1; i <= n && out.length < MAX_HIGHLIGHTS; i++) {
      var text = model.getLineContent(i);
      if (!text || text.indexOf(word) < 0) continue;
      re.lastIndex = 0;
      var m;
      while ((m = re.exec(text)) !== null) {
        pushMatch(out, seen, i, m.index + 1, word.length);
        if (out.length >= MAX_HIGHLIGHTS) break;
        if (re.lastIndex === m.index) re.lastIndex++;
      }
    }
  }

  function provideDocumentHighlights(model, position) {
    if (!model || !position || !monacoRef) return Promise.resolve(null);
    var l = lang();
    var info = null;
    try { info = l && l.infoAt ? l.infoAt(model, position) : null; } catch (e) { info = null; }
    if (!info || !info.word) return Promise.resolve(null);
    var w;
    try { w = model.getWordAtPosition(position); } catch (e2) { w = null; }
    if (!w || !w.word) return Promise.resolve(null);

    var out = [];
    var seen = {};
    var word = info.word;

    function localOnly() {
      out = []; seen = {};
      scanModelForWord(model, word, out, seen);
      return out.length ? out : null;
    }

    if (!l || typeof l.resolveSymbol !== 'function') return Promise.resolve(localOnly());

    return Promise.resolve(l.resolveSymbol(info)).then(function (def) {
      if (!def || !def.node || !def.node.id || typeof l.referenceSites !== 'function') {
        return localOnly();
      }
      return Promise.resolve(l.referenceSites(def.node.id)).then(function (sites) {
        var list = (sites || []).slice(0, MAX_SITES);
        var onDocModel = !fileOfModel(model);
        if (onDocModel && typeof l.docLinesFor === 'function') {
          for (var i = 0; i < list.length && out.length < MAX_HIGHLIGHTS; i++) {
            var s = list[i];
            if (!s || !s.file || !s.line) continue;
            var modelLines = [];
            // docLinesFor maps a real (file, line) onto the open document; a
            // `csgraph://` peek model is not that document, so it uses the
            // (file, line) -> line index directly.
            try { modelLines = l.docLinesFor(model, s.file, s.line) || []; } catch (e) { modelLines = []; }
            if (!modelLines.length) {
              var idx = lineIndexFor(model);
              var mapped = idx.map[s.file + ':' + s.line];
              if (mapped != null) modelLines = [mapped];
            }
            for (var k = 0; k < modelLines.length && out.length < MAX_HIGHLIGHTS; k++) {
              var ln = modelLines[k];
              var text = model.getLineContent(ln);
              var re = new RegExp('\\b' + escapeRegExp(word) + '\\b', 'g');
              re.lastIndex = 0;
              var m;
              while ((m = re.exec(text)) !== null) {
                pushMatch(out, seen, ln, m.index + 1, word.length);
                if (out.length >= MAX_HIGHLIGHTS) break;
                if (re.lastIndex === m.index) re.lastIndex++;
              }
            }
          }
        }
        // Word-level occurrences the graph did not list (declaration sites
        // already included by referenceSites) are still useful context.
        if (!out.length) return localOnly();
        return out;
      }).catch(function () { return localOnly(); });
    }).catch(function () { return localOnly(); });
  }

  // ------------------------------------------------------------------ attach

  function clearCaches() {
    declCache = {};
    declKeys = [];
    lastSymbolsValue = null;
    lastDeclsValue = [];
  }

  function attach(c) {
    ctx = c || ctx;
    monacoRef = (ctx && ctx.monaco) || monacoRef;
    var m = monacoRef;
    if (!m || !m.languages) return;
    if (!installed) {
      installed = true;
      if (typeof m.languages.registerDocumentSymbolProvider === 'function') {
        m.languages.registerDocumentSymbolProvider('csharp', {
          provideDocumentSymbols: function (model) { return provideDocumentSymbols(model); }
        });
      }
      if (typeof m.languages.registerFoldingRangeProvider === 'function') {
        m.languages.registerFoldingRangeProvider('csharp', {
          provideFoldingRanges: function (model) { return provideFoldingRanges(model); }
        });
      }
      if (typeof m.languages.registerDocumentHighlightProvider === 'function') {
        m.languages.registerDocumentHighlightProvider('csharp', {
          provideDocumentHighlights: function (model, position) {
            return provideDocumentHighlights(model, position);
          }
        });
      }
    }
    if (!modelSubscribed && typeof ctx.on === 'function') {
      modelSubscribed = true;
      ctx.on('model', function () { clearCaches(); });
    }
  }

  window.CsSymbols = {
    attach: attach,
    installed: function () { return installed; },
    /** Shared with semantics.js so the outline and the colouring use one query. */
    declsFor: declsFor,
    provideDocumentSymbols: provideDocumentSymbols,
    provideFoldingRanges: provideFoldingRanges,
    provideDocumentHighlights: provideDocumentHighlights,
    /** Headless hooks. */
    lastSymbols: function () { return lastSymbolsValue; },
    lastDecls: function () { return lastDeclsValue; },
    lastError: function () { return lastError; },
    clearCaches: clearCaches
  };

  if (typeof window.CsEditorExtension === 'function') {
    window.CsEditorExtension({ name: 'symbols', attach: attach });
  }
})();
