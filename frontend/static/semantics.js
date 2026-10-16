/* csgraph v2 — semantic colouring for the Monaco C# pane.
 *
 * Part of the RUST-ONLY v2 viewer. Registers through `window.CsEditorExtension`
 * (see editor.js); editor.js/language.js are untouched.
 *
 * The point of semantic tokens here is not full C# semantics — Monaco's own
 * Dark+ tokenizer already colours keywords, strings, numbers and BCL types.
 * It is to make *corpus* symbols visually distinct: a `DocumentSymbol`-coloured
 * identifier means "this name is a declaration in the indexed graph", so a
 * reader can tell project types from framework types at a glance.
 *
 * Tokens are built only from local data: the `/api/decls` rows already fetched
 * for this document (shared with symbols.js) plus identifiers in the model text
 * that match those declaration names. There is deliberately no `/api/find` call
 * per identifier — that would hammer the engine for every keystroke-sized
 * request. `kind` picks the token type, and the count is capped.
 */
'use strict';

(function () {
  var installed = false;          // Monaco providers are global
  var ctx = null;
  var monacoRef = null;
  var modelSubscribed = false;

  // Standard Monaco semantic token type names, so the existing themes know how
  // to colour them (custom names would render with the default foreground).
  var LEGEND = {
    tokenTypes: ['namespace', 'class', 'struct', 'interface', 'enum', 'method',
                 'property', 'field', 'event', 'parameter', 'variable',
                 'enumMember', 'type'],
    tokenModifiers: []
  };
  var TYPE_INDEX = {};
  for (var t = 0; t < LEGEND.tokenTypes.length; t++) TYPE_INDEX[LEGEND.tokenTypes[t]] = t;

  var MAX_TOKENS = 20000;
  var anonSeq = 0;

  var declCache = {};
  var declKeys = [];
  var DECL_CACHE_MAX = 24;

  var lastData = new Uint32Array(0);
  var lastTokens = [];
  var cached = { model: null, version: -1, data: null, tokens: null };

  // ------------------------------------------------------------------ helpers

  function srcState() {
    return (ctx && ctx.state) || window.state || null;
  }

  function lang() {
    return (ctx && ctx.lang) || window.CsLang || null;
  }

  function modelKey(model) {
    try {
      if (model && model.uri && typeof model.uri.toString === 'function') return 'u:' + model.uri.toString();
      if (model && model.id) return 'i:' + model.id;
    } catch (e) { /* fall through */ }
    try {
      if (!model.__csSemKey) model.__csSemKey = 'm:' + (++anonSeq);
      return model.__csSemKey;
    } catch (e2) {
      return 'm:anon';
    }
  }

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
   * The document's declaration rows. symbols.js already fetches exactly this,
   * so reuse its cache when it is on the page and fall back to our own.
   */
  function declsFor(model) {
    if (!model) return Promise.resolve([]);
    if (window.CsSymbols && typeof window.CsSymbols.declsFor === 'function') {
      try { return Promise.resolve(window.CsSymbols.declsFor(model)); } catch (e) { /* fall through */ }
    }
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
      } catch (e2) {
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

  // -------------------------------------------------------------- token type

  function typeForKind(kind) {
    switch (kind) {
      case 'namespace': return TYPE_INDEX.namespace;
      case 'class': case 'record': return TYPE_INDEX.class;
      case 'struct': return TYPE_INDEX.struct;
      case 'interface': return TYPE_INDEX.interface;
      case 'enum': return TYPE_INDEX.enum;
      case 'method': case 'constructor': case 'operator': return TYPE_INDEX.method;
      case 'property': return TYPE_INDEX.property;
      case 'field': return TYPE_INDEX.field;
      case 'event': return TYPE_INDEX.event;
      case 'parameter': return TYPE_INDEX.parameter;
      case 'variable': case 'local': return TYPE_INDEX.variable;
      case 'enumMember': case 'enummember': return TYPE_INDEX.enumMember;
      default: return TYPE_INDEX.type;
    }
  }

  /** Every identifier spelling a declaration row answers to. */
  function nameMapForRows(rows) {
    var map = {};
    for (var i = 0; i < rows.length; i++) {
      var row = rows[i];
      if (!row) continue;
      var t = typeForKind(row.kind);
      var names = [row.name];
      if (row.label) names.push(String(row.label).split('.').pop());
      if (row.full) names.push(String(row.full).split('.').pop());
      for (var k = 0; k < names.length; k++) {
        var nm = names[k];
        if (nm && nm.length >= 2 && map[nm] === undefined) map[nm] = t;
      }
    }
    return map;
  }

  // ----------------------------------------------------------------- tokens

  function tokensForModel(model, rows) {
    var nameType = nameMapForRows(rows || []);
    var data = [];
    var list = [];
    var lineCount = model.getLineCount();
    var prevLine = 0;      // 0-based line of the previous token
    var prevCol = 0;       // 0-based start column of the previous token
    var idRe = /[A-Za-z_][A-Za-z0-9_]*/g;

    for (var ln = 1; ln <= lineCount; ln++) {
      if (data.length / 5 >= MAX_TOKENS) break;
      var text = model.getLineContent(ln);
      if (!text) continue;
      var trimmed = text.replace(/^\s+/, '');
      // Declaration-site separators and ordinary comments are chrome, not code.
      if (trimmed.charAt(0) === '/' || trimmed.charAt(0) === '*') continue;
      idRe.lastIndex = 0;
      var m;
      while ((m = idRe.exec(text)) !== null) {
        var name = m[0];
        var type = nameType[name];
        if (type === undefined || name.length < 2) continue;
        var startCol = m.index;                      // 0-based
        var dl = (ln - 1) - prevLine;
        var ds = dl === 0 ? startCol - prevCol : startCol;
        if (dl === 0 && ds < 0) continue;            // never emit out of order
        data.push(dl, ds, name.length, type, 0);
        list.push({ line: ln, column: startCol + 1, length: name.length, type: type, name: name });
        prevLine = ln - 1;
        prevCol = startCol;
        if (data.length / 5 >= MAX_TOKENS) break;
      }
    }
    return { data: new Uint32Array(data), tokens: list };
  }

  function provideDocumentSemanticTokens(model) {
    if (!model) return Promise.resolve({ data: new Uint32Array(0) });
    var version = -1;
    try { version = model.getVersionId(); } catch (e) { /* ignore */ }
    if (cached.model === model && cached.version === version && cached.data) {
      return Promise.resolve({ data: cached.data });
    }
    return declsFor(model).then(function (rows) {
      try {
        var built = tokensForModel(model, rows);
        lastData = built.data;
        lastTokens = built.tokens;
        cached = { model: model, version: version, data: built.data, tokens: built.tokens };
        return { data: built.data };
      } catch (e) {
        lastData = new Uint32Array(0);
        lastTokens = [];
        return { data: lastData };
      }
    }).catch(function () {
      lastData = new Uint32Array(0);
      lastTokens = [];
      return { data: lastData };
    });
  }

  function releaseDocumentSemanticTokens() { /* nothing retained per result */ }

  // ------------------------------------------------------------------ attach

  function clearCaches() {
    declCache = {};
    declKeys = [];
    cached = { model: null, version: -1, data: null, tokens: null };
    lastData = new Uint32Array(0);
    lastTokens = [];
  }

  function attach(c) {
    ctx = c || ctx;
    monacoRef = (ctx && ctx.monaco) || monacoRef;
    var m = monacoRef;
    if (!m || !m.languages) return;
    if (!installed && typeof m.languages.registerDocumentSemanticTokensProvider === 'function') {
      installed = true;
      m.languages.registerDocumentSemanticTokensProvider('csharp', {
        // This vendored Monaco build forwards only (selector, provider) to the
        // registry — its styling service calls provider.getLegend() — so the
        // legend must live on the provider as well as in the third argument.
        getLegend: function () { return LEGEND; },
        provideDocumentSemanticTokens: function (model) {
          return provideDocumentSemanticTokens(model);
        },
        releaseDocumentSemanticTokens: releaseDocumentSemanticTokens
      }, LEGEND);
    }
    if (!modelSubscribed && typeof ctx.on === 'function') {
      modelSubscribed = true;
      ctx.on('model', function () { clearCaches(); });
    }
  }

  window.CsSemantics = {
    attach: attach,
    installed: function () { return installed; },
    legend: LEGEND,
    declsFor: declsFor,
    provideDocumentSemanticTokens: provideDocumentSemanticTokens,
    /** Headless hooks: readable tokens + the raw LSP-encoded array. */
    tokensFor: function () { return lastTokens; },
    lastData: function () { return lastData; },
    tokensForModel: tokensForModel,
    clearCaches: clearCaches
  };

  if (typeof window.CsEditorExtension === 'function') {
    window.CsEditorExtension({ name: 'semantics', attach: attach });
  }
})();
