/* csgraph v2 — rich hover cards for the Monaco C# pane.
 *
 * Part of the RUST-ONLY v2 viewer. Registers itself through the extension API
 * in editor.js (`window.CsEditorExtension`), so editor.js/language.js stay
 * untouched.
 *
 * The engine already knows everything a hover needs: `/api/node` returns the
 * declaration's signature, access, degree and its incoming/outgoing edges with
 * the line of every reference. language.js owns the one symbol resolver
 * (`resolveSymbol`), which resolves a cursor position either to the enclosing
 * declaration or — through that declaration's own edges — to the member the
 * reference names. This file only turns that answer into markdown.
 *
 * Hover is deliberately silent for comments, string literals, whitespace and
 * words that do not map to a graph node: a viewer that pops a card on `public`
 * or on an arbitrary comment word trains the reader to ignore it.
 */
'use strict';

(function () {
  var installed = false;         // Monaco providers are global; register once
  var ctx = null;
  var monacoRef = null;
  var lastHover = null;

  // /api/node detail per node id. The graph is static for an index, so the
  // cache lives for the whole session; it is bounded so a long walk over a
  // huge corpus cannot grow without limit.
  var detailCache = {};
  var detailKeys = [];
  var CACHE_MAX = 256;

  var blockCache = { model: null, version: -1, starts: null };

  // ------------------------------------------------------------------ helpers

  function srcState() {
    return (ctx && ctx.state) || window.state || null;
  }

  function lang() {
    return (ctx && ctx.lang) || window.CsLang || null;
  }

  function esc(s) {
    if (ctx && typeof ctx.esc === 'function') return ctx.esc(s);
    return String(s === null || s === undefined ? '' : s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  function fmtNum(n) {
    if (ctx && typeof ctx.fmtNum === 'function') return ctx.fmtNum(n);
    return n === null || n === undefined || isNaN(n) ? '—' : String(n);
  }

  function apiGet() {
    return (ctx && ctx.apiGet) || window.apiGet || null;
  }

  /** `/api/node` for one id, resolved once per session. */
  function detailFor(id) {
    if (!id) return Promise.resolve(null);
    if (detailCache[id]) return detailCache[id];
    var get = apiGet();
    if (!get) return Promise.resolve(null);
    if (detailKeys.length >= CACHE_MAX) {
      var drop = detailKeys.shift();
      if (drop) delete detailCache[drop];
    }
    detailKeys.push(id);
    var p;
    try {
      p = Promise.resolve(get('/api/node', { id: id })).catch(function () { return null; });
    } catch (e) {
      p = Promise.resolve(null);
    }
    detailCache[id] = p;
    return p;
  }

  /** Does `word` actually name this node? Guards the ranked-fallback match. */
  function endsWithName(node, word) {
    if (!node || !word) return false;
    var name = node.name || '';
    var label = node.label || '';
    var full = node.full || '';
    var id = node.id || '';
    return name === word || label === word ||
      label.slice(-(word.length + 1)) === '.' + word ||
      full.slice(-(word.length + 1)) === '.' + word ||
      id.indexOf(word + '(') === 0 || id === word;
  }

  // ------------------------------------------------- comment / string guard

  /**
   * Per-line "does this line begin inside a block comment" flags.
   *
   * A single-line tokenizer cannot know that line 12 is the tail of a block
   * comment that opened on line 3, so that state is tracked once per model.
   */
  function blockStarts(model) {
    try {
      if (blockCache.model === model && blockCache.version === model.getVersionId()) {
        return blockCache.starts;
      }
    } catch (e) { /* fall through and recompute */ }
    var n = model.getLineCount();
    var starts = new Array(n);
    var inBlock = false;
    for (var i = 1; i <= n; i++) {
      starts[i - 1] = inBlock;
      var t = model.getLineContent(i) || '';
      var j = 0;
      while (j < t.length) {
        if (inBlock) {
          var end = t.indexOf('*/', j);
          if (end < 0) break;
          inBlock = false;
          j = end + 2;
          continue;
        }
        var c = t.charAt(j);
        if (c === '/' && t.charAt(j + 1) === '/') break;
        if (c === '/' && t.charAt(j + 1) === '*') { inBlock = true; j += 2; continue; }
        if (c === '@' && t.charAt(j + 1) === '"') {           // verbatim string
          j += 2;
          while (j < t.length) {
            if (t.charAt(j) === '"' && t.charAt(j + 1) === '"') { j += 2; continue; }
            if (t.charAt(j) === '"') { j++; break; }
            j++;
          }
          continue;
        }
        if (c === '"' || c === "'") {                          // string / char
          var quote = c;
          j++;
          while (j < t.length) {
            if (t.charAt(j) === '\\') { j += 2; continue; }
            if (t.charAt(j) === quote) { j++; break; }
            j++;
          }
          continue;
        }
        j++;
      }
    }
    blockCache = { model: model, version: model.getVersionId(), starts: starts };
    return starts;
  }

  /** Ranges (0-based, [start,end)) that are comment or string text on one line. */
  function commentRanges(text) {
    var ranges = [];
    var i = 0;
    while (i < text.length) {
      var c = text.charAt(i);
      if (c === '/' && text.charAt(i + 1) === '/') {
        ranges.push([i, text.length]); break;
      }
      if (c === '/' && text.charAt(i + 1) === '*') {
        var close = text.indexOf('*/', i + 2);
        var end = close < 0 ? text.length : close + 2;
        ranges.push([i, end]); i = end; continue;
      }
      if (c === '@' && text.charAt(i + 1) === '"') {
        var j = i + 2;
        while (j < text.length) {
          if (text.charAt(j) === '"' && text.charAt(j + 1) === '"') { j += 2; continue; }
          if (text.charAt(j) === '"') { j++; break; }
          j++;
        }
        ranges.push([i, j]); i = j; continue;
      }
      if (c === '"' || c === "'") {
        var q = c; var k = i + 1;
        while (k < text.length) {
          if (text.charAt(k) === '\\') { k += 2; continue; }
          if (text.charAt(k) === q) { k++; break; }
          k++;
        }
        ranges.push([i, k]); i = k; continue;
      }
      i++;
    }
    return ranges;
  }

  /**
   * True when (line, column) — both 1-based — sits in a comment or string.
   * Monaco's own tokenizer is authoritative when available; the local scanner
   * is the fallback and also covers a block comment's tail lines.
   */
  function insideCommentOrString(model, lineNumber, column) {
    if (!model || !lineNumber) return false;
    var text = '';
    try { text = model.getLineContent(lineNumber) || ''; } catch (e) { return false; }

    var starts = blockStarts(model);
    if (starts && starts[lineNumber - 1]) {
      var close = text.indexOf('*/');
      if (close < 0 || column <= close + 2) return true;   // still inside
    }

    if (monacoRef && monacoRef.editor && typeof monacoRef.editor.tokenize === 'function') {
      try {
        var toks = monacoRef.editor.tokenize(text, 'csharp')[0] || [];
        for (var i = 0; i < toks.length; i++) {
          var s = (toks[i].offset || 0) + 1;
          var e = i + 1 < toks.length ? (toks[i + 1].offset || 0) + 1 : text.length + 1;
          if (column >= s && column < e && /^(comment|string)/.test(toks[i].type || '')) return true;
        }
      } catch (e2) { /* tokenizer unavailable — scanner below still runs */ }
    }

    var ranges = commentRanges(text);
    for (var r = 0; r < ranges.length; r++) {
      if (column - 1 >= ranges[r][0] && column - 1 < ranges[r][1]) return true;
    }
    return false;
  }

  // ---------------------------------------------------------------- markdown

  function signatureOf(node) {
    if (!node) return '';
    if (node.signature) return String(node.signature);
    var params = node.params;
    var sig = node.name || node.label || node.full || '';
    if (params && params.length) {
      var ps = [];
      for (var i = 0; i < params.length; i++) {
        var p = params[i];
        if (p === null || p === undefined) continue;
        if (typeof p === 'string') { ps.push(p); continue; }
        ps.push((p.type ? p.type + ' ' : '') + (p.name || ''));
      }
      sig += '(' + ps.join(', ') + ')';
    }
    if (node.ret) sig = node.ret + ' ' + sig;
    return sig;
  }

  function locationMarkdown(node) {
    if (!node || !node.file) return '';
    var line = node.line || 1;
    var uri = '';
    var l = lang();
    try { uri = l && l.csUri ? l.csUri(node.file).toString() : ''; } catch (e) { uri = ''; }
    var label = esc(node.file) + ':' + line;
    return uri ? '[' + label + '](' + uri + ')' : label;
  }

  function linesOf(list) {
    var n = 0;
    (list || []).forEach(function (it) {
      if (it && it.lines && it.lines.length) n += it.lines.length;
    });
    return n;
  }

  function countsMarkdown(det, node) {
    var parts = [];
    var degree = node && node.degree !== undefined && node.degree !== null ? node.degree : null;
    if (degree !== null) parts.push('degree ' + fmtNum(degree));
    if (det) {
      var callers = det.callers || [];
      var callees = det.callees || [];
      var refs = linesOf(det.callers) + linesOf(det.uses) + linesOf(det.usedBy) +
        linesOf(det.callees);
      if (callers.length) {
        parts.push(fmtNum(callers.length) + ' caller' + (callers.length === 1 ? '' : 's'));
      } else if (callees.length) {
        parts.push(fmtNum(callees.length) + ' callee' + (callees.length === 1 ? '' : 's'));
      }
      if (refs) parts.push(fmtNum(refs) + ' reference' + (refs === 1 ? '' : 's'));
    }
    return parts.join(' · ');
  }

  /**
   * The owning .sln/.csproj project(s) for the node's file.
   *
   * Attribution comes from the engine's additive `/api/projects?files=1` map
   * (filters.js loads it and exposes it as `window.CsProjects`); the node
   * payload only carries `file`, which is what the map is keyed on. Empty when
   * no `.sln`/`.csproj` is known — the card simply omits the line.
   */
  function projectsMarkdown(node) {
    var api = window.CsProjects;
    if (!api || !node || typeof api.idsFor !== 'function') return '';
    var ids = api.idsFor(node) || [];
    if (!ids.length) return '';
    var names = ids.map(function (id) {
      return (typeof api.name === 'function' && api.name(id)) || id;
    });
    return 'Project: ' + esc(names.join(', '));
  }

  function render(node, det) {
    var md = [];
    var sig = signatureOf(node);
    if (sig) md.push('```csharp\n' + sig + '\n```');

    var name = node.label || node.full || node.name || node.id || 'symbol';
    var head = '**' + esc(name) + '** · ' + esc(node.kind || 'symbol');
    if (node.access) head += ' · ' + esc(node.access);
    if (node.static) head += ' · static';
    if (node.external) head += ' · external';
    md.push(head);

    var loc = locationMarkdown(node);
    if (loc) md.push(loc);

    var proj = projectsMarkdown(node);
    if (proj) md.push(proj);

    var counts = countsMarkdown(det, node);
    if (counts) md.push(counts);

    if (!md.length) return null;
    return [
      { value: md.join('\n\n') },
      { value: '$(go-to-file) Ctrl+click opens the declaration' }
    ];
  }

  // ---------------------------------------------------------------- provider

  /**
   * The plain provider function, also used directly by headless checks.
   * Resolves the symbol at `position`, then answers with `/api/node` detail.
   */
  function provideHover(model, position) {
    lastHover = null;
    var l = lang();
    if (!model || !position || !l || typeof l.infoAt !== 'function') return Promise.resolve(null);
    var info;
    try { info = l.infoAt(model, position); } catch (e) { info = null; }
    if (!info || !info.word) return Promise.resolve(null);
    if (insideCommentOrString(model, position.lineNumber, info.column)) return Promise.resolve(null);

    var word = model.getWordAtPosition(position);
    if (!word || !word.word) return Promise.resolve(null);

    return Promise.resolve(l.resolveSymbol(info)).then(function (def) {
      if (!def || !def.node || !def.node.id) return null;
      // The ranked fallback is a name match, not a graph edge; only trust it
      // when the node really is named like the word under the cursor.
      if (def.via === 'ranked' && !endsWithName(def.node, info.word)) return null;
      return detailFor(def.node.id).then(function (det) {
        // The project map comes from /api/projects; a failure just omits the
        // line, it never suppresses the card.
        var api = window.CsProjects;
        var ready = (api && typeof api.load === 'function')
          ? Promise.resolve(api.load()).catch(function () { return null; })
          : Promise.resolve(null);
        return ready.then(function () {
          var node = (det && det.node) || def.node;
          var contents = render(node, det);
          if (!contents) return null;
          lastHover = { node: node, via: def.via, contents: contents };
          return {
            contents: contents,
            range: new monacoRef.Range(
              position.lineNumber, word.startColumn, position.lineNumber, word.endColumn)
          };
        });
      });
    }).catch(function () { return null; });
  }

  // ------------------------------------------------------------------ attach

  function attach(c) {
    ctx = c || ctx;
    monacoRef = (ctx && ctx.monaco) || monacoRef;
    var m = monacoRef;
    if (!m || !m.languages) return;
    if (!installed && typeof m.languages.registerHoverProvider === 'function') {
      installed = true;
      m.languages.registerHoverProvider('csharp', {
        provideHover: function (model, position) { return provideHover(model, position); }
      });
    }
  }

  window.CsHover = {
    attach: attach,
    installed: function () { return installed; },
    provideHover: provideHover,
    /** Headless hooks. */
    insideCommentOrString: function (model, lineNumber, column) {
      return insideCommentOrString(model, lineNumber, column);
    },
    signatureOf: signatureOf,
    projectsMarkdown: projectsMarkdown,
    detailFor: detailFor,
    lastHover: function () { return lastHover; }
  };

  if (typeof window.CsEditorExtension === 'function') {
    window.CsEditorExtension({ name: 'hover', attach: attach });
  }
})();
