/* csgraph v2 — the source pane, powered by the real VSCode editor.
 *
 * This file is part of the RUST-ONLY v2 viewer. It is not loaded by v1.
 *
 * The v1 pane built one <div> plus 5-20 <span>s per source line. A synthesized
 * whole-corpus document is tens of thousands of lines, so that approach created
 * hundreds of thousands of DOM nodes, laid them all out, and re-tokenized every
 * line on every open — which is exactly why the UI janked.
 *
 * Monaco is VSCode's editor: it keeps the buffer as a model and only renders
 * the lines that intersect the viewport (plus a small overscan), so cost is
 * bounded by the window, not by the file. It also brings the real Dark+ C#
 * tokenizer, the find widget, folding, the minimap, bracket-pair colorization
 * and selection handling — VSCode behaviour, not an impression of it.
 *
 * The Monaco build is vendored whole under vendor/monaco/ and is never touched
 * by npm/pnpm/yarn (see tools/vendor_monaco.py --verify).
 */
'use strict';

window.CsEditor = (function () {
  var MONO_BASE = '/vendor/monaco/min/vs';
  var FONT_STACK = "'Droid Sans Mono', 'monospace', monospace, 'Droid Sans Fallback'";

  var monaco = null;
  var editor = null;
  var model = null;
  var decorations = null;
  var host = null;
  var loadState = 'idle';       // idle | loading | ready | failed
  var waiting = [];             // callbacks fired once Monaco is up
  var pending = null;           // last show() payload, replayed on ready
  var lineClick = null;
  var lastLines = [];
  var readyAt = 0;

  // ---------------------------------------------------------- extension points
  // Every editor feature that lives outside this file (hover, symbols, CodeLens,
  // call hierarchy, quick-open, …) registers itself here instead of editing
  // editor.js. `attach()` is called once per Monaco editor instance, after the
  // editor exists and CsLang is attached; it gets a context object with the
  // shared globals (apiGet/toast/state/openSource/…) plus the hooks below.
  // `on('model', cb)` fires after every show(), so a feature can recompute its
  // decorations/providers for the new document without polling.
  var extensions = [];          // {name, attach(ctx)} registered by feature files
  var listeners = {};           // event name -> [cb]
  var attachedFor = null;       // editor instance already wired to extensions

  function on(evt, cb) {
    (listeners[evt] = listeners[evt] || []).push(cb);
    return function () { off(evt, cb); };
  }

  function off(evt, cb) {
    var list = listeners[evt];
    if (!list) return;
    var i = list.indexOf(cb);
    if (i >= 0) list.splice(i, 1);
  }

  function emit(evt, payload) {
    var list = listeners[evt];
    if (!list || !list.length) return;
    list.slice().forEach(function (cb) {
      try { cb(payload); } catch (e) { report(e); }
    });
  }

  /** Feature files call this at load time: `CsEditorExtension({name, attach})`. */
  function registerExtension(ext) {
    if (ext && typeof ext.attach === 'function') extensions.push(ext);
  }
  window.CsEditorExtension = registerExtension;

  /** The shared context handed to every extension's attach(). */
  function extensionContext() {
    return {
      monaco: monaco,
      editor: editor,
      // core.js globals — feature files must not re-implement these
      apiGet: window.apiGet,
      apiPost: window.apiPost,
      toast: window.toast,
      esc: window.esc,
      fmtNum: window.fmtNum,
      state: window.state,
      openSource: window.openSource,
      goToNode: window.goToNode,
      lang: window.CsLang,
      on: on,
      off: off,
      /** Monaco line n (1-based) -> the source record, or null. */
      recordAt: function (n) { return lastLines[n - 1] || null; },
      lines: function () { return lastLines; },
      model: function () { return model; },
      /** Scroll Monaco to a model line (1-based), centred by default. */
      revealLine: function (line, center) {
        if (!editor || !line) return;
        if (center === false) editor.revealLine(line);
        else editor.revealLineInCenter(line);
      },
      isDocModel: function () {
        return !!(window.CsLang && window.CsLang.isDocModel && window.CsLang.isDocModel(model));
      }
    };
  }

  function attachExtensions() {
    if (!editor || attachedFor === editor) return;
    attachedFor = editor;
    var ctx = extensionContext();
    extensions.forEach(function (ext) {
      try { ext.attach(ctx); } catch (e) { report(ext && ext.name ? ext.name + ': ' + e : e); }
    });
  }

  // ------------------------------------------------------------------ themes
  // Token colours are the same values style.css uses for the v1 pane, so the
  // two renderers are visually interchangeable; the dark themes are the
  // editor defaults from microsoft/vscode extensions/theme-defaults.
  var PALETTES = {
    darkplus: {
      bg: '#1e1e1e', fg: '#d4d4d4', gutter: '#858585', gutterActive: '#c6c6c6',
      keyword: '#569cd6', control: '#c586c0', type: '#4ec9b0', iface: '#b8d7a3',
      string: '#ce9178', number: '#b5cea8', comment: '#6a9955', preproc: '#c586c0',
      func: '#dcdcaa', variable: '#9cdcfe', op: '#d4d4d4',
      lineHighlight: '#282828', selection: '#264f78', base: 'vs-dark'
    },
    darkmodern: {
      bg: '#1f1f1f', fg: '#cccccc', gutter: '#6e7681', gutterActive: '#cccccc',
      keyword: '#569cd6', control: '#c586c0', type: '#4ec9b0', iface: '#b8d7a3',
      string: '#ce9178', number: '#b5cea8', comment: '#6a9955', preproc: '#c586c0',
      func: '#dcdcaa', variable: '#9cdcfe', op: '#d4d4d4',
      lineHighlight: '#2a2d2e', selection: '#264f78', base: 'vs-dark'
    },
    monokai: {
      bg: '#272822', fg: '#f8f8f2', gutter: '#75715e', gutterActive: '#cfcfc2',
      keyword: '#f92672', control: '#f92672', type: '#66d9ef', iface: '#a6e22e',
      string: '#e6db74', number: '#ae81ff', comment: '#75715e', preproc: '#f92672',
      func: '#a6e22e', variable: '#f8f8f2', op: '#f92672',
      lineHighlight: '#3e3d32', selection: '#49483e', base: 'vs-dark'
    },
    solarized: {
      bg: '#002b36', fg: '#93a1a1', gutter: '#586e75', gutterActive: '#93a1a1',
      keyword: '#859900', control: '#cb4b16', type: '#b58900', iface: '#2aa198',
      string: '#2aa198', number: '#d33682', comment: '#586e75', preproc: '#cb4b16',
      func: '#268bd2', variable: '#839496', op: '#93a1a1',
      lineHighlight: '#073642', selection: '#0d4b5a', base: 'vs-dark'
    }
  };

  function themeId(name) { return 'csgraph-' + (PALETTES[name] ? name : 'darkplus'); }

  function defineThemes() {
    Object.keys(PALETTES).forEach(function (name) {
      var p = PALETTES[name];
      monaco.editor.defineTheme(themeId(name), {
        base: p.base,
        inherit: true,
        rules: [
          { token: 'comment', foreground: p.comment.slice(1), fontStyle: 'italic' },
          { token: 'keyword', foreground: p.keyword.slice(1) },
          { token: 'keyword.control', foreground: p.control.slice(1) },
          { token: 'keyword.operator', foreground: p.op.slice(1) },
          { token: 'keyword.directive', foreground: p.preproc.slice(1) },
          { token: 'preprocessor', foreground: p.preproc.slice(1) },
          { token: 'string', foreground: p.string.slice(1) },
          { token: 'string.escape', foreground: p.number.slice(1) },
          { token: 'number', foreground: p.number.slice(1) },
          { token: 'type', foreground: p.type.slice(1) },
          { token: 'type.identifier', foreground: p.type.slice(1) },
          { token: 'namespace', foreground: p.type.slice(1) },
          { token: 'identifier', foreground: p.fg.slice(1) },
          { token: 'identifier.function', foreground: p.func.slice(1) },
          { token: 'delimiter', foreground: p.op.slice(1) },
          { token: 'operator', foreground: p.op.slice(1) },
          { token: 'annotation', foreground: p.func.slice(1) },
          { token: 'variable', foreground: p.variable.slice(1) }
        ],
        colors: {
          'editor.background': p.bg,
          'editor.foreground': p.fg,
          'editorLineNumber.foreground': p.gutter,
          'editorLineNumber.activeForeground': p.gutterActive,
          'editor.lineHighlightBackground': p.lineHighlight,
          'editor.selectionBackground': p.selection,
          'editor.inactiveSelectionBackground': p.selection,
          'editorCursor.foreground': p.gutterActive,
          'editorWhitespace.foreground': '#3b3b3b',
          'editorIndentGuide.background1': '#404040',
          'editorGutter.background': p.bg,
          'editorWidget.background': p.bg,
          'minimap.background': p.bg
        }
      });
    });
  }

  // ------------------------------------------------------------------- style
  // Decoration classes. Injected here so the editor integration stays in one
  // file and v1's stylesheet keeps describing only the v1 renderer.
  function injectStyle() {
    if (document.getElementById('cs-editor-style')) return;
    var css = document.createElement('style');
    css.id = 'cs-editor-style';
    css.textContent = [
      '.source-body.monaco-on { overflow: hidden; padding: 0; position: relative; }',
      '#cs-monaco-host { position: absolute; inset: 0; }',
      '.cs-deco-part { background: rgba(110,118,129,0.12); font-style: italic; }',
      '.cs-deco-part-inline { font-style: italic; }',
      '.cs-deco-head, .cs-deco-tail { font-style: italic; }',
      '.cs-deco-cond { background: rgba(56,189,248,0.07); border-left: 2px solid rgba(56,189,248,0.45); }',
      '.cs-deco-cond-close { background: rgba(56,189,248,0.05); border-left: 2px solid rgba(148,163,184,0.45); }',
      '.cs-deco-inactive { opacity: 0.45; }',
      '.cs-deco-target { background: rgba(86,156,214,0.16); }',
      '.cs-match { background: rgba(255,237,46,0.22); border-radius: 2px; }',
      '.cs-gutter-hidden { color: transparent !important; }',
      // Monaco scopes its theme variables to `.monaco-editor, .monaco-diff-editor,
      // .monaco-component` (standaloneThemeService). A context view mounted in the
      // light DOM (document.body) is outside that scope, so it renders transparent
      // with unstyled text. syncThemeVars() mirrors the variables out to `:root`;
      // these fallbacks keep that menu readable even before it lands.
      '.context-view .monaco-menu-container { background-color: var(--vscode-menu-background, #252526); border: 1px solid var(--vscode-menu-border, #454545); }',
      '.context-view .monaco-menu { background-color: var(--vscode-menu-background, #252526); color: var(--vscode-menu-foreground, #cccccc); }',
      '.context-view .monaco-menu .action-label { color: var(--vscode-menu-foreground, #cccccc); }',
      '.context-view .monaco-menu .action-item.focused .action-label,',
      '.context-view .monaco-menu .action-item:hover .action-label { background-color: var(--vscode-menu-selectionBackground, #04395e); color: var(--vscode-menu-selectionForeground, #ffffff); }'
    ].join('\n');
    document.head.appendChild(css);
  }

  // ------------------------------------------------------- shadow-root context menu
  //
  // Monaco 0.57 does not put the editor context menu on the document: the
  // ContextViewService mounts it in an OPEN SHADOW ROOT (`div.shadow-root-host`
  // -> attachShadow), and the stylesheet it injects there
  // (base/browser/ui/menu/menu.css) has NO background rule for either
  // `.context-view.monaco-menu-container` or `.monaco-menu` — so the menu paints
  // clear over the canvas and its text floats, and no document stylesheet can
  // reach it (shadow DOM does not see them). The `--vscode-*` variables do
  // inherit through the host, so the fix is to add the missing background (and
  // selection) rules inside every shadow root Monaco creates. attachShadow is
  // hooked once, before Monaco's first context view exists.
  var SHADOW_MENU_CSS = [
    '.context-view.monaco-menu-container {',
    '  background-color: var(--vscode-menu-background, #252526);',
    '  color: var(--vscode-menu-foreground, #cccccc);',
    '  border: 1px solid var(--vscode-menu-border, #454545);',
    '  border-radius: var(--vscode-cornerRadius-large, 6px);',
    '}',
    '.monaco-menu { background-color: transparent; }',
    '.monaco-menu .monaco-action-bar.vertical .action-menu-item:hover,',
    '.monaco-menu .monaco-action-bar.vertical .action-item.focused > .action-menu-item {',
    '  background-color: var(--vscode-menu-selectionBackground, #04395e);',
    '  color: var(--vscode-menu-selectionForeground, #ffffff);',
    '}',
    '.monaco-menu .monaco-action-bar.vertical .action-menu-item:hover .keybinding,',
    '.monaco-menu .monaco-action-bar.vertical .action-item.focused > .action-menu-item .keybinding,',
    '.monaco-menu .monaco-action-bar.vertical .action-menu-item:hover .submenu-indicator,',
    '.monaco-menu .monaco-action-bar.vertical .action-item.focused > .action-menu-item .submenu-indicator {',
    '  color: inherit;',
    '}'
  ].join('\n');
  var shadowMenuHooked = false;

  function installShadowMenuStyle() {
    if (shadowMenuHooked) return;
    shadowMenuHooked = true;
    if (!window.Element || !Element.prototype.attachShadow) return;
    var nativeAttachShadow = Element.prototype.attachShadow;
    Element.prototype.attachShadow = function (init) {
      var root = nativeAttachShadow.call(this, init);
      try {
        if (root && !root.querySelector('style[data-csgraph-menu]')) {
          var st = document.createElement('style');
          st.setAttribute('data-csgraph-menu', '1');
          st.textContent = SHADOW_MENU_CSS;
          root.appendChild(st);
        }
      } catch (e) { /* a menu skin must never break the editor */ }
      return root;
    };
  }
  installShadowMenuStyle();

  /**
   * Copy Monaco's `--vscode-*` theme variables from the editor root out to
   * `:root`, so Monaco chrome that is mounted outside the editor (the context
   * menu, and anything else the standalone layout service puts on `body`) still
   * inherits the theme instead of rendering transparent.
   */
  function syncThemeVars() {
    if (!editor || !editor.getDomNode) return;
    var src = window.getComputedStyle(editor.getDomNode());
    var root = document.documentElement;
    for (var i = 0; i < src.length; i++) {
      var name = src.item(i);
      if (name && name.indexOf('--vscode-') === 0) {
        root.style.setProperty(name, src.getPropertyValue(name));
      }
    }
  }

  // -------------------------------------------------------------------- load
  function onReady(cb) {
    if (loadState === 'ready') { cb(); return; }
    if (loadState === 'failed') return;      // caller falls back to v1 rendering
    waiting.push(cb);
  }

  function flushReady() {
    var cbs = waiting; waiting = [];
    cbs.forEach(function (cb) { try { cb(); } catch (e) { report(e); } });
    if (pending) { var p = pending; pending = null; show(p.lines, p.opts); }
  }

  function report(err) {
    if (window.console && console.error) console.error('[csgraph editor]', err);
  }

  function load() {
    if (loadState === 'loading' || loadState === 'ready') return;
    loadState = 'loading';
    if (window.__CSVIEWER_LEGACY_RENDERER__) { loadState = 'failed'; return; }
    if (typeof window.require !== 'function') {
      loadState = 'failed';
      report('Monaco AMD loader not present; using the v1 line renderer');
      return;
    }
    try {
      window.require.config({ paths: { vs: MONO_BASE }, ignoreDuplicateModules: true });
      window.require(['vs/editor/editor.main'], function (m) {
        monaco = m;
        defineThemes();
        injectStyle();
        loadState = 'ready';
        readyAt = Date.now();
        flushReady();
      }, function (err) {
        loadState = 'failed';
        report('Monaco failed to load: ' + err);
        flushReady();
      });
    } catch (e) {
      loadState = 'failed';
      report(e);
    }
  }

  // ------------------------------------------------------------------ editor
  function ensureEditor() {
    if (editor) return editor;
    host = document.createElement('div');
    host.id = 'cs-monaco-host';
    sourceBodyEl.innerHTML = '';
    sourceBodyEl.classList.add('monaco-on');
    sourceBodyEl.appendChild(host);
    editor = monaco.editor.create(host, {
      value: '',
      language: 'csharp',
      theme: themeId((state.source && state.source.theme) || 'darkplus'),
      // The pane is a viewer and stays read-only. F2 rename is gated on this
      // option (`EditorContextKeys.writable = !readOnly`, and the RenameAction
      // needs `writable`), so language.js lifts it for the length of the rename
      // gesture alone and puts it back when the rename box closes — you can
      // rename a symbol, but you still cannot type into the document.
      readOnly: true,
      domReadOnly: true,
      automaticLayout: true,
      // VSCode defaults: minimap on, folding on, bracket colouring on.
      minimap: { enabled: true, renderCharacters: true, showSlider: 'mouseover' },
      folding: true,
      foldingHighlight: true,
      showFoldingControls: 'mouseover',
      bracketPairColorization: { enabled: true },
      guides: { bracketPairs: true, indentation: true },
      renderLineHighlight: 'all',
      renderWhitespace: 'selection',
      // Occurrences highlight (same-symbol under cursor) and sticky scroll
      // (enclosing declaration pinned to the top) are both editor options; the
      // symbol side of sticky scroll comes from the document-symbol provider in
      // symbols.js, so it activates the moment that provider is registered.
      occurrencesHighlight: 'multiFile',
      occurrencesHighlightDelay: 120,
      stickyScroll: { enabled: true, maxLineCount: 5, defaultModel: 'outlineModel' },
      'semanticHighlighting.enabled': true,
      scrollBeyondLastLine: false,
      smoothScrolling: true,
      cursorBlinking: 'blink',
      mouseWheelZoom: true,
      fontSize: 14,
      lineHeight: 0,
      fontFamily: FONT_STACK,
      fontLigatures: false,
      lineNumbers: function (n) {
        var rec = lastLines[n - 1];
        return rec && rec.n ? String(rec.n) : '';
      },
      glyphMargin: false,
      lineDecorationsWidth: 10,
      overviewRulerBorder: false,
      wordWrap: 'off',
      padding: { top: 6, bottom: 6 },
      scrollbar: { verticalScrollbarSize: 12, horizontalScrollbarSize: 12, useShadows: false },
      fixedOverflowWidgets: true,
      contextmenu: true
    });
    decorations = editor.createDecorationsCollection([]);
    syncThemeVars();
    // The C# language features (definition/references/rename, Ctrl+hover) live
    // in language.js and need the editor instance to hang their gestures on.
    if (window.CsLang && window.CsLang.attach) window.CsLang.attach(monaco, editor);
    // Then every registered extension (hover, symbols, CodeLens, hierarchy, …).
    attachExtensions();
    emit('ready', { monaco: monaco, editor: editor });
    editor.onMouseDown(function (e) {
      var t = e.target;
      if (!lineClick) return;
      if (t.type === monaco.editor.MouseTargetType.GUTTER_LINE_NUMBERS && t.position) {
        var rec = lastLines[t.position.lineNumber - 1];
        if (rec) lineClick(rec);
      }
    });
    return editor;
  }

  // -------------------------------------------------------------------- show
  /**
   * Render a source payload (the /api/source shape) into the pane.
   *
   * lines: [{n, text, ctx, sec, active, src}]  — identical to v1
   * opts:  {match, target, onClickLine}
   *
   * The model text is the concatenation of every record in order, so Monaco's
   * line i is lines[i-1] and the custom lineNumbers() callback can print the
   * record's own file/doc number (or nothing for viewer-only separator rows).
   */
  function show(lines, opts) {
    opts = opts || {};
    if (opts.onClickLine) lineClick = opts.onClickLine;
    if (loadState !== 'ready') {
      if (loadState === 'idle') load();
      pending = { lines: lines, opts: opts };
      return false;
    }
    var ed = ensureEditor();
    lastLines = lines || [];

    var text = [];
    for (var i = 0; i < lastLines.length; i++) text.push(lastLines[i].text == null ? '' : lastLines[i].text);
    var body = text.join('\n');

    // The model is replaced rather than mutated: a whole-corpus document is a
    // different buffer every time, and replace() keeps Monaco from diffing a
    // 30k-line text against an unrelated one.
    var old = ed.getModel();
    model = monaco.editor.createModel(body, 'csharp');
    ed.setModel(model);
    if (old) old.dispose();

    var decos = [];
    var needle = opts.match || '';
    for (var L = 0; L < lastLines.length; L++) {
      var rec = lastLines[L], line = L + 1;
      var cls = [], gutter = null;
      if (rec.ctx === 'part') { cls.push('cs-deco-part'); }
      else if (rec.ctx === 'head' || rec.ctx === 'tail') { cls.push('cs-deco-head'); }
      if (rec.ctx === 'cond') {
        cls.push(rec.sec === 'close' ? 'cs-deco-cond-close' : 'cs-deco-cond');
      }
      if (rec.active === false) cls.push('cs-deco-inactive');
      if (rec.n && opts.target && rec.n === opts.target) cls.push('cs-deco-target');
      if (!rec.n) gutter = 'cs-gutter-hidden';
      if (cls.length || gutter) {
        decos.push({
          range: new monaco.Range(line, 1, line, 1),
          options: {
            isWholeLine: true,
            className: cls.join(' '),
            linesDecorationsClassName: gutter || undefined
          }
        });
      }
      if (needle) {
        var at = (rec.text || '').indexOf(needle);
        if (at >= 0) {
          decos.push({
            range: new monaco.Range(line, at + 1, line, at + 1 + needle.length),
            options: { inlineClassName: 'cs-match' }
          });
        }
      }
    }
    decorations.set(decos);
    ed.setScrollTop(0);
    var reveal = -1;
    if (opts.target) {
      for (var k = 0; k < lastLines.length; k++) {
        if (lastLines[k].n && lastLines[k].n === opts.target) { reveal = k + 1; break; }
      }
    }
    if (reveal > 0) ed.revealLineInCenter(reveal);
    emit('model', { model: model, lines: lastLines, opts: opts });
    return true;
  }

  function clear() {
    if (editor) { editor.setModel(null); }
    if (model) { model.dispose(); model = null; }
    lastLines = [];
    if (sourceBodyEl) {
      sourceBodyEl.classList.remove('monaco-on');
      sourceBodyEl.innerHTML = '';
    }
    host = null; editor = null; decorations = null;
  }

  function setTheme(name) {
    if (loadState !== 'ready') return;
    var id = themeId(name);
    if (editor) { editor.updateOptions({ theme: id }); syncThemeVars(); }
  }

  return {
    enabled: function () { return !window.__CSVIEWER_LEGACY_RENDERER__; },
    state: function () { return loadState; },
    readyAt: function () { return readyAt; },
    load: load,
    onReady: onReady,
    show: show,
    clear: clear,
    setTheme: setTheme,
    monaco: function () { return monaco; },
    editor: function () { return editor; },
    /** The line record behind Monaco line `n` (language.js maps to real files). */
    recordAt: function (n) { return lastLines[n - 1] || null; },
    /** The whole record list behind the current Monaco model. */
    lines: function () { return lastLines; },
    /** The live Monaco text model (null before the first show()). */
    model: function () { return model; },
    /** The declaration-site records (`ctx === 'part'`) of the current document. */
    partRecords: function () { return lastLines.filter(function (l) { return l.ctx === 'part'; }); },
    // extension bus — see the feature files
    register: registerExtension,
    on: on,
    off: off,
    emit: emit,
    context: extensionContext,
    /** Test hook: the model's line count, for headless verification. */
    lineCount: function () { return model ? model.getLineCount() : 0; }
  };
})();
