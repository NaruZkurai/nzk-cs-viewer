/* csgraph v2 — C# language features for the Monaco pane.
 *
 * Part of the RUST-ONLY v2 viewer. Not loaded by v1.
 *
 * The engine already knows the symbol graph: a node carries its fully-qualified
 * id, its declaration sites (`parts`) and its incoming edges (`callers`/
 * `usedBy`) with the line of every reference. This file is the thin bridge
 * between that graph and Monaco's own VSCode gestures, so the editor behaves
 * the way VSCode does rather than approximating it:
 *
 *   Ctrl+hover        the symbol under the pointer gets a link underline
 *   Ctrl+click / F12  open the REAL file at its true line (raw mode)
 *   Alt+F12           peek: when the declaration is in the open synthesized
 *                     document, peek the section in place; otherwise the file
 *   Shift+F12         peek every reference to the symbol
 *   F2                rename preview; every reference and declaration site is
 *                     edited as a real file model, and the preview is Monaco's
 *                     own
 *
 * The `csgraph://file/...` models are created on demand from `/api/source`, so
 * Monaco's peek and rename machinery has a real buffer per referenced file even
 * though the pane itself shows one document.
 */
'use strict';

window.CsLang = (function () {
  var monaco = null;
  var editor = null;
  var installed = false;

  var URI_SCHEME = 'csgraph';
  var modelByFile = {};        // repo path -> ITextModel
  var fileByUri = {};          // model URI string -> repo path
  var textPromises = {};       // repo path -> Promise<string[]>

  // ------------------------------------------------------------------ helpers

  function csUri(file) {
    return monaco.Uri.parse(URI_SCHEME + '://file/' +
      String(file).split('/').map(encodeURIComponent).join('/'));
  }

  /** Every line of one repo file, fetched once. */
  function fileLines(file) {
    if (textPromises[file]) return textPromises[file];
    textPromises[file] = apiGet('/api/source', {
      file: file, line: 1, to: 999999, mode: 'raw'
    }).then(function (d) {
      return (d.lines || []).map(function (l) { return l.text == null ? '' : l.text; });
    }).catch(function (err) {
      delete textPromises[file];
      throw err;
    });
    return textPromises[file];
  }

  /** A Monaco model for one repo file, with the file's real line numbers. */
  function ensureModel(file) {
    if (modelByFile[file]) return Promise.resolve(modelByFile[file]);
    return fileLines(file).then(function (lines) {
      if (modelByFile[file]) return modelByFile[file];
      var model = monaco.editor.createModel(lines.join('\n'), 'csharp', csUri(file));
      modelByFile[file] = model;
      fileByUri[model.uri.toString()] = file;
      return model;
    });
  }

  function fileOfModel(model) {
    return model && fileByUri[model.uri.toString()] || null;
  }

  function isDocModel(model) {
    return !!model && !fileOfModel(model);
  }

  /**
   * The line record behind one Monaco line of the open document (editor.js).
   *
   * Three cases: a `csgraph://` file model (real line numbers throughout), a
   * synthesized document (each record carries its `src`), and a *raw file
   * window*, which is neither — the pane knows the file (`state.source.file`)
   * and every record carries its real line number in `n`. Without that last
   * case a raw window resolves against an empty file name and every
   * Ctrl+click lands on whatever `/api/find` ranked first.
   */
  function recordAt(model, lineNumber) {
    var file = fileOfModel(model);
    if (file) return { src: { file: file, line: lineNumber } };
    var rec = (window.CsEditor && window.CsEditor.recordAt)
      ? window.CsEditor.recordAt(lineNumber) : null;
    if (rec && rec.src) return rec;
    var st = (typeof state === 'object' && state && state.source) ? state.source : null;
    if (st && st.file && !st.doc) {
      return { src: { file: st.file, line: (rec && rec.n) || lineNumber } };
    }
    return rec;
  }

  /** (file, real line, word) for a Monaco position, or null. */
  function infoAt(model, position) {
    if (!model || !position) return null;
    var w = model.getWordAtPosition(position);
    if (!w || !w.word) return null;
    var rec = recordAt(model, position.lineNumber);
    var src = rec && rec.src;
    return {
      file: (src && src.file) || fileOfModel(model) || '',
      line: (src && src.line) || position.lineNumber,
      word: w.word,
      column: w.startColumn,
      model: model,
      position: position
    };
  }

  /** First identifier column of `word` on `line` of `file` (1-based). */
  function columnOf(lines, line, word) {
    var text = lines && lines[line - 1];
    if (text == null) return 1;
    var re = new RegExp('\\b' + word.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + '\\b');
    var m = re.exec(text);
    return m ? m.index + 1 : 1;
  }

  function locForFile(file, line, word) {
    return ensureModel(file).then(function (model) {
      return fileLines(file).then(function (lines) {
        var col = columnOf(lines, line, word);
        return {
          uri: model.uri,
          range: new monaco.Range(line, col, line, col + String(word).length)
        };
      });
    });
  }

  /**
   * The lines of the currently open document that came from (file, line).
   * A reference that is already on screen should be peeked where it is, not
   * opened again in a second buffer.
   */
  function docLinesFor(model, file, line) {
    var out = [];
    if (!window.CsEditor || !window.CsEditor.recordAt) return out;
    var n = model.getLineCount();
    for (var i = 1; i <= n; i++) {
      var rec = window.CsEditor.recordAt(i);
      var src = rec && rec.src;
      if (src && src.file === file && src.line === line) out.push(i);
    }
    return out;
  }

  function docLocFor(model, file, line, word) {
    var hits = docLinesFor(model, file, line);
    if (!hits.length) return null;
    var ln = hits[0];
    var text = model.getLineContent(ln);
    var at = text.indexOf(word);
    var col = at >= 0 ? at + 1 : 1;
    return {
      uri: model.uri,
      range: new monaco.Range(ln, col, ln, col + String(word).length)
    };
  }

  // ------------------------------------------------------------------- graph

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

  /**
   * Resolve the symbol at (file, line, word) to a graph node.
   *
   * The enclosing declaration is the entry point — `/api/find` attaches it to
   * every usage — and from there the answer is the edge the declaration makes
   * to a member called `word`. That is what makes this resolve at the
   * fully-qualified level: the edge target is the exact node, not the closest
   * name match. The name ranking is only a fallback for a cursor sitting on a
   * declaration or on something the graph did not link.
   */
  function resolveSymbol(info) {
    var word = info.word;
    if (!word) return Promise.resolve(null);
    return apiGet('/api/find', { q: word, limit: 400 }).then(function (f) {
      var caller = null;
      var usages = f.usages || [];
      for (var i = 0; i < usages.length; i++) {
        var u = usages[i];
        if (u.file === info.file && u.line === info.line && u.node && u.node.id) { caller = u.node; break; }
      }
      if (!caller) {
        for (var j = 0; j < usages.length; j++) {
          var v = usages[j];
          if (v.file === info.file && v.node && v.node.id) { caller = v.node; break; }
        }
      }
      // Cursor on the declaration itself: the enclosing node *is* the symbol.
      if (caller && (caller.name === word ||
          (caller.full || '').slice(-(word.length + 1)) === '.' + word)) {
        return { node: caller, via: 'enclosing' };
      }
      var chain = caller && caller.id
        ? apiGet('/api/node', { id: caller.id }).catch(function () { return null; })
        : Promise.resolve(null);
      return chain.then(function (det) {
        if (det) {
          var buckets = ['callees', 'uses', 'usedBy', 'callers'];
          for (var b = 0; b < buckets.length; b++) {
            var items = det[buckets[b]] || [];
            for (var k = 0; k < items.length; k++) {
              if (endsWithName(items[k].node, word)) {
                return { node: items[k].node, via: buckets[b] };
              }
            }
          }
        }
        var ranked = (f.definitions || [])[0] || (f.implementations || [])[0] || null;
        return ranked ? { node: ranked, via: 'ranked' } : null;
      });
    });
  }

  /** Where the symbol is declared, in the file and (when on screen) the doc. */
  function definitionLocations(model, info, def) {
    var d = def.node;
    if (!d || !d.file) return Promise.resolve([]);
    var word = info.word;
    var doc = docLocFor(model, d.file, d.line, word);
    if (doc) return Promise.resolve([doc]);
    return locForFile(d.file, d.line, word).then(function (loc) { return [loc]; });
  }

  /**
   * Every place that references `id`, plus its declaration sites.
   *
   * `callers`/`usedBy` are the graph's own edges, so comments and string
   * literals are not touched. `contains` edges carry no line and are skipped.
   */
  function referenceSites(id) {
    return apiGet('/api/node', { id: id }).then(function (det) {
      var node = det.node || {};
      var sites = [];
      var seen = {};
      function add(file, line) {
        if (!file || !line) return;
        var key = file + ':' + line;
        if (seen[key]) return;
        seen[key] = 1;
        sites.push({ file: file, line: line });
      }
      (node.parts || []).forEach(function (p) { add(p.file || node.file, p.line); });
      add(node.file, node.line);
      ['callers', 'usedBy'].forEach(function (bucket) {
        (det[bucket] || []).forEach(function (it) {
          if (it.kind === 'contains') return;
          (it.lines || []).forEach(function (l) { add(it.node && it.node.file, l); });
        });
      });
      return sites;
    });
  }

  // --------------------------------------------------------------- providers

  /** Every edit a rename would make, grouped into Monaco's WorkspaceEdit shape. */
  function buildRenameEdits(info, newName) {
    return resolveSymbol(info).then(function (def) {
      if (!def || !def.node || !def.node.id) {
        return { edits: [], rejectReason: 'Nothing indexed to rename here.' };
      }
      return referenceSites(def.node.id).then(function (sites) {
        var byFile = {};
        sites.forEach(function (s) { (byFile[s.file] = byFile[s.file] || []).push(s.line); });
        var files = Object.keys(byFile);
        return Promise.all(files.map(function (file) {
          return fileLines(file).then(function (lines) {
            return { file: file, lines: lines, rows: byFile[file] };
          });
        })).then(function (loaded) {
          var edits = [];
          loaded.forEach(function (f) {
            ensureModel(f.file).then(function () { /* model exists for the preview */ });
            f.rows.forEach(function (line) {
              var text = f.lines[line - 1] || '';
              var re = new RegExp('\\b' + info.word.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + '\\b', 'g');
              var m;
              while ((m = re.exec(text)) !== null) {
                edits.push({
                  resource: csUri(f.file),
                  textEdit: {
                    range: new monaco.Range(line, m.index + 1, line, m.index + 1 + info.word.length),
                    text: newName
                  }
                });
              }
            });
          });
          return { edits: edits, symbol: def.node, sites: sites };
        });
      });
    });
  }

  /** Resolve a rename position the way the provider does. */
  function resolveRenameLocation(model, position) {
    var info = infoAt(model, position);
    if (!info) return Promise.resolve({ rejectReason: 'There is no identifier under the cursor.' });
    return resolveSymbol(info).then(function (def) {
      if (!def) {
        return { rejectReason: 'No indexed declaration or reference for "' + info.word + '".' };
      }
      var w = model.getWordAtPosition(position);
      return {
        range: new monaco.Range(position.lineNumber, w.startColumn, position.lineNumber, w.endColumn),
        text: w.word
      };
    });
  }

  function installProviders() {
    monaco.languages.registerDefinitionProvider('csharp', {
      provideDefinition: function (model, position) {
        var info = infoAt(model, position);
        if (!info) return null;
        return resolveSymbol(info).then(function (def) {
          if (!def) return null;
          return definitionLocations(model, info, def);
        });
      }
    });

    monaco.languages.registerReferenceProvider('csharp', {
      provideReferences: function (model, position) {
        var info = infoAt(model, position);
        if (!info) return null;
        return resolveSymbol(info).then(function (def) {
          if (!def || !def.node || !def.node.id) return null;
          return referenceSites(def.node.id).then(function (sites) {
            return Promise.all(sites.map(function (s) {
              var doc = docLocFor(model, s.file, s.line, info.word);
              if (doc) return Promise.resolve(doc);
              return locForFile(s.file, s.line, info.word);
            }));
          });
        });
      }
    });

    monaco.languages.registerRenameProvider('csharp', {
      resolveRenameLocation: resolveRenameLocation,
      provideRenameEdits: function (model, position, newName) {
        var info = infoAt(model, position);
        if (!info) return { edits: [] };
        return buildRenameEdits(info, newName);
      }
    });

    // F12 / Ctrl+click on a real-file location would otherwise open that file
    // as a model *inside this editor*, replacing the document. Hand it to the
    // source pane instead, which is where a file belongs.
    if (monaco.editor.registerEditorOpener) {
      monaco.editor.registerEditorOpener({
        openCodeEditor: function (source, resource, selectionOrPosition) {
          var file = fileByUri[resource.toString()];
          if (!file) return false;
          var line = 1;
          if (selectionOrPosition && selectionOrPosition.startLineNumber) {
            line = selectionOrPosition.startLineNumber;
          } else if (selectionOrPosition && selectionOrPosition.lineNumber) {
            line = selectionOrPosition.lineNumber;
          }
          if (typeof openSource === 'function') openSource({ file: file, line: line, mode: 'raw' });
          return true;
        }
      });
    }
  }

  // ------------------------------------------------------- Ctrl+hover / click

  var linkDeco = null;
  var linkKey = '';

  function setLink(range) {
    if (!editor) return;
    if (!linkDeco) linkDeco = editor.createDecorationsCollection([]);
    linkDeco.set(range ? [{
      range: range,
      options: { inlineClassName: 'cs-lang-link', stickiness: 0 }
    }] : []);
  }

  function wordRange(model, position) {
    var w = model.getWordAtPosition(position);
    if (!w) return null;
    return new monaco.Range(position.lineNumber, w.startColumn, position.lineNumber, w.endColumn);
  }

  function injectStyle() {
    if (document.getElementById('cs-lang-style')) return;
    var css = document.createElement('style');
    css.id = 'cs-lang-style';
    css.textContent = [
      '.cs-lang-link { text-decoration: underline; text-underline-offset: 2px;',
      '  text-decoration-color: rgba(134,170,249,.9); cursor: pointer; }'
    ].join('\n');
    document.head.appendChild(css);
  }

  function onMouseMove(e) {
    var mod = e.event && (e.event.ctrlKey || e.event.metaKey);
    if (!mod || !e.target || !e.target.position || !editor) { setLink(null); linkKey = ''; return; }
    var model = editor.getModel();
    if (!model) { setLink(null); return; }
    var pos = e.target.position;
    var w = model.getWordAtPosition(pos);
    if (!w) { setLink(null); linkKey = ''; return; }
    var key = pos.lineNumber + ':' + w.word;
    if (key === linkKey) return;
    linkKey = key;
    setLink(wordRange(model, pos));
  }

  /**
   * Ctrl/Cmd+click goes to the declaration (the real file, always);
   * Shift+click peeks the references — VSCode's gesture set, wired onto the
   * mouse because this pane is where the symbols are read from.
   */
  function ctrlClick(e) {
    var ev = e.event;
    if (!ev || !e.target || !e.target.position) return;
    var model = editor.getModel();
    if (!model) return;
    var shift = ev.shiftKey && !ev.ctrlKey && !ev.metaKey;
    if (shift) {
      if (!infoAt(model, e.target.position)) return;
      ev.preventDefault();
      ev.stopPropagation();
      editor.setPosition(e.target.position);
      editor.focus();
      editor.trigger('cs-lang', 'editor.action.referenceSearch.trigger', {});
      return;
    }
    if (!(ev.ctrlKey || ev.metaKey) || ev.shiftKey) return;
    var info = infoAt(model, e.target.position);
    if (!info) return;
    ev.preventDefault();
    ev.stopPropagation();
    resolveSymbol(info).then(function (def) {
      if (!def || !def.node || !def.node.file) {
        if (window.toast) toast('No indexed declaration for "' + info.word + '"', 'warn');
        return;
      }
      // Ctrl+click always opens the real file, even when the declaration is
      // also visible in the synthesized document.
      openSource({ file: def.node.file, line: def.node.line || 1, mode: 'raw' });
    });
  }

  // ----------------------------------------------------------------- rename UI

  var renameOpen = false;

  /**
   * F2 with the pane read-only.
   *
   * Monaco's RenameAction is disabled unless `writable` is on, and the pane is a
   * viewer that must not accept typing. So its own F2 binding is left disabled
   * (its precondition fails), this action owns F2, and it flips `readOnly` off
   * for exactly as long as the rename box is on screen. The edits themselves go
   * to the `csgraph://` file models, never to the document buffer.
   */
  function beginRename() {
    if (renameOpen) return;
    renameOpen = true;
    editor.updateOptions({ readOnly: false });
    editor.trigger('cs-lang', 'editor.action.rename', {});
    var tries = 0;
    var timer = window.setInterval(function () {
      tries++;
      var open = false;
      var boxes = document.querySelectorAll('.rename-box');
      for (var i = 0; i < boxes.length; i++) {
        if (boxes[i].getBoundingClientRect().width > 0) { open = true; break; }
      }
      if (!open || tries > 1600) {
        window.clearInterval(timer);
        renameOpen = false;
        editor.updateOptions({ readOnly: true });
      }
    }, 100);
  }

  function installRenameKey() {
    editor.addAction({
      id: 'csgraph.renameSymbol',
      label: 'Rename Symbol',
      keybindings: [monaco.KeyCode.F2],
      // No precondition: this action is what makes F2 available in a read-only
      // pane, so it must run even though `writable` is off.
      run: function () { beginRename(); }
    });
  }

  // ------------------------------------------------------------------- attach

  function attach(m, ed) {
    monaco = m;
    editor = ed;
    if (!monaco || !editor) return;
    injectStyle();
    if (!installed) {
      installed = true;
      installProviders();
    }
    installRenameKey();
    editor.onMouseMove(onMouseMove);
    editor.onMouseDown(ctrlClick);
    editor.onMouseLeave(function () { setLink(null); linkKey = ''; });
  }

  /** Test hook: run the real click handler without synthesising DOM events. */
  function testClick(lineNumber, column, mods) {
    mods = mods || {};
    var fake = {
      event: {
        ctrlKey: !!mods.ctrl, metaKey: !!mods.meta, shiftKey: !!mods.shift,
        preventDefault: function () {}, stopPropagation: function () {}
      },
      target: { position: { lineNumber: lineNumber, column: column } }
    };
    ctrlClick(fake);
  }

  return {
    attach: attach,
    testClick: testClick,
    /** Test hooks, used by tools/probe_language.mjs. */
    resolveAt: function (file, line, word) {
      return resolveSymbol({ file: file, line: line, word: word });
    },
    referencesOf: referenceSites,
    /** Test hooks: the exact provider inputs, for tools/probe_language.mjs. */
    infoAt: function (model, position) { return infoAt(model || editor.getModel(), position); },
    renameLocationAt: resolveRenameLocation,
    /** Test hook: exactly what F2 would do, without driving the rename widget. */
    renameEditsAt: function (file, line, word, newName) {
      return buildRenameEdits({ file: file, line: line, word: word }, newName);
    },
    models: function () { return modelByFile; },

    // --------------------------------------------------------- shared helpers
    // Exported for the editor feature modules (hover.js, symbols.js, codelens.js,
    // hierarchy.js, semantics.js, quickopen.js, nav.js). They are the same
    // functions the gestures above use, so every feature resolves symbols the
    // one way this file already does — no second resolver.
    /** (file, line, word) -> graph node via /api/find + /api/node. */
    resolveSymbol: resolveSymbol,
    /** Every reference site + declaration site of a node id. */
    referenceSites: referenceSites,
    /** All lines of one repo file (fetched once, cached). */
    fileLines: fileLines,
    /** A `csgraph://file/…` Monaco model for one repo file. */
    ensureModel: ensureModel,
    /** Monaco location of (file, line, word) in the file's own model. */
    locForFile: locForFile,
    /** Monaco location of (file, line, word) when it is inside `model`'s document. */
    docLocFor: docLocFor,
    /** Model lines of the open document that came from (file, line). */
    docLinesFor: docLinesFor,
    /** The repo path behind a `csgraph://` model, or null for a document model. */
    fileOfModel: fileOfModel,
    /** True when `model` is the synthesized/open document (not a csgraph file model). */
    isDocModel: isDocModel,
    /** The `src`/line record behind a model line. */
    recordAt: recordAt,
    /** `csgraph://file/…` URI for a repo path. */
    csUri: csUri,
    /** True once the providers are installed (test hook). */
    installed: function () { return installed; }
  };
})();
