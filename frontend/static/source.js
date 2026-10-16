/* csgraph - the source pane, C# tokenizer and the source/UI themes.
 *
 * Part of csgraph. These files are classic scripts loaded in order from
 * index.html and share one global scope, so a plain function name is visible
 * everywhere - exactly as it was when the whole app lived in one file.
 */
'use strict';

  // ================================================================== source viewer

  function showSourcePane() {
    sourcePaneEl.classList.remove('hidden');
    layoutEl.classList.add('with-source');
    resize();
  }

  function closeSource() {
    state.source.open = false;
    sourcePaneEl.classList.add('hidden');
    layoutEl.classList.remove('with-source');
    resize();
    if (window.CSSync) window.CSSync.push();
  }

  function openSource(params) {
    var seq = ++srcSeq;
    var st = state.source;
    var q = { mode: st.srcMode || 'doc' };
    if (params.mode) { q.mode = params.mode; st.srcMode = params.mode; }
    if (params.node) {
      q.node = params.node;
      st.nodeId = params.node;
      st.mode = 'node';
      // A namespace (or any node without a declaration site of its own) has no
      // raw file to show: always open the synthesized document for it, even if
      // the pane was left on Raw.
      var nn = state.nodeById && state.nodeById.get(params.node);
      if (!params.mode && !(nn && nn.file)) { q.mode = 'doc'; st.srcMode = 'doc'; }
    } else {
      q.file = params.file;
      q.line = params.line;
      if (params.to) q.to = params.to;
      else q.context = params.context || SOURCE_CONTEXT;
      st.nodeId = null;
      st.mode = 'window';
      st.line = params.line;
    }
    if (params.match !== undefined) st.match = params.match;
    showSourcePane();
    // Monaco owns its own host element for the lifetime of the pane; wiping
    // sourceBodyEl would delete it out from under a live editor.
    if (!monacoOn()) sourceBodyEl.textContent = '';
    st.lines = [];              // never let Copy hand out the previous file
    sourceNoteEl.textContent = 'Loading…';
    apiGet('/api/source', q)
      .then(function (d) {
        if (seq !== srcSeq) return;
        renderSource(d);
      })
      .catch(function (err) {
        if (seq !== srcSeq) return;
        sourceNoteEl.textContent = 'Could not load source: ' + err.message;
        toast('Source lookup failed: ' + err.message, 'error');
      });
  }

  // ================================================================== C# syntax highlighting

  var CS_KEYWORDS = ('abstract as base bool break byte case catch char checked class const continue ' +
    'decimal default delegate do double else enum event explicit extern false finally fixed float for ' +
    'foreach goto if implicit in int interface internal is lock long namespace new null object operator ' +
    'out override params private protected public readonly ref return sbyte sealed short sizeof ' +
    'stackalloc static string struct switch this throw true try typeof uint ulong unchecked unsafe ' +
    'ushort using virtual void volatile while var dynamic get set').split(' ');
  var CS_CONTROL = ('if else for foreach while do switch case default break continue return goto throw ' +
    'try catch finally using lock yield await async').split(' ');
  var CS_KEYWORD_SET = {}; CS_KEYWORDS.forEach(function (k) { CS_KEYWORD_SET[k] = 1; });
  var CS_CONTROL_SET = {}; CS_CONTROL.forEach(function (k) { CS_CONTROL_SET[k] = 1; });

  function isIdentStart(ch) { return /[A-Za-z_@]/.test(ch); }
  function isIdentChar(ch) { return /[A-Za-z0-9_]/.test(ch); }

  /**
   * Tiny C# tokenizer: returns [{cls, text}] for one line. `state.block` carries an
   * unterminated block comment across lines, so ranges stay correct.
   */
  function tokenizeCsLine(text, state) {
    var out = [];
    var i = 0, n = text.length;
    function push(cls, str) { if (str) out.push({ cls: cls, text: str }); }

    if (state.block) {
      var end = text.indexOf('*/');
      if (end < 0) { push('tok-comment', text); return out; }
      push('tok-comment', text.slice(0, end + 2));
      i = end + 2;
      state.block = false;
    }

    var ppMatch = /^\s*#/.exec(text);
    if (ppMatch && i === 0) { push('tok-preproc', text); return out; }

    while (i < n) {
      var ch = text[i], two = text.substr(i, 2);

      if (two === '//') { push('tok-comment', text.slice(i)); break; }
      if (two === '/*') {
        var close = text.indexOf('*/', i + 2);
        if (close < 0) { push('tok-comment', text.slice(i)); state.block = true; break; }
        push('tok-comment', text.slice(i, close + 2));
        i = close + 2;
        continue;
      }
      // strings: "…", @"…", $"…", $@"…"
      if (ch === '"' || two === '@"' || two === '$"' || text.substr(i, 3) === '$@"' || text.substr(i, 3) === '@$"') {
        var verbatim = two === '@"' || text.substr(i, 3) === '$@"' || text.substr(i, 3) === '@$"';
        var start = i;
        i += verbatim ? 2 : 1;
        if (text[start] === '$') i = start + (verbatim ? 3 : 2);
        while (i < n) {
          if (verbatim) {
            if (text[i] === '"' && text[i + 1] === '"') { i += 2; continue; }
            if (text[i] === '"') { i++; break; }
          } else {
            if (text[i] === '\\') { i += 2; continue; }
            if (text[i] === '"') { i++; break; }
          }
          i++;
        }
        push('tok-string', text.slice(start, i));
        continue;
      }
      if (ch === "'") {
        var st = i; i++;
        while (i < n) { if (text[i] === '\\') { i += 2; continue; } if (text[i] === "'") { i++; break; } i++; }
        push('tok-string', text.slice(st, i));
        continue;
      }
      if (/[0-9]/.test(ch)) {
        var ns = i;
        while (i < n && /[0-9a-fA-FxX._]/.test(text[i])) i++;
        push('tok-number', text.slice(ns, i));
        continue;
      }
      if (isIdentStart(ch)) {
        var is = i;
        i++;
        while (i < n && isIdentChar(text[i])) i++;
        var word = text.slice(is, i);
        var bare = word.charAt(0) === '@' ? word.slice(1) : word;
        var j = i;
        while (j < n && /\s/.test(text[j])) j++;
        var nextCh = text[j];
        var cls;
        if (CS_CONTROL_SET[bare]) cls = 'tok-control';
        else if (CS_KEYWORD_SET[bare]) cls = 'tok-keyword';
        else if (nextCh === '(') cls = 'tok-function';
        else if (/^I[A-Z]/.test(bare) && bare.length > 2) cls = 'tok-interface';
        else if (/^[A-Z]/.test(bare)) cls = 'tok-type';
        else cls = 'tok-variable';
        push(cls, word);
        continue;
      }
      if ('{}()[];,.<>+-*/%=&|!?:^~'.indexOf(ch) >= 0) {
        var os = i;
        while (i < n && '{}()[];,.<>+-*/%=&|!?:^~'.indexOf(text[i]) >= 0) i++;
        push('tok-op', text.slice(os, i));
        continue;
      }
      push('tok-plain', ch);
      i++;
    }
    return out;
  }

  /** True when the VSCode (Monaco) editor is the active renderer. */
  function monacoOn() {
    return !!(window.CsEditor && window.CsEditor.enabled());
  }

  function applySourceTheme(name) {
    var themes = ['darkplus', 'darkmodern', 'monokai', 'solarized'];
    if (themes.indexOf(name) < 0) name = 'darkplus';
    state.source.theme = name;
    sourcePaneEl.className = 'source-pane theme-' + name +
      (sourcePaneEl.classList.contains('hidden') ? ' hidden' : '');
    if (window.CsEditor) window.CsEditor.setTheme(name);
    if (sourceThemeEl) sourceThemeEl.value = name;
    try { window.localStorage.setItem('csgraph.theme', name); } catch (e) { /* ignore */ }
  }

  // ------------------------------------------------------------- UI theme
  // The whole-viewer theme. Unlike the source syntax theme (which is local to
  // the source pane) this restyles the chrome *and* the canvas, so its own
  // setting is stored in the user config file (~/.config/csgraph/settings.json)
  // rather than the per-folder viewer state.
  function applyUiTheme(name, opts) {
    if (!UI_THEMES[name]) name = DEFAULT_UI_THEME;
    uiThemeName = name;
    state.uiTheme = name;
    document.documentElement.setAttribute('data-ui-theme', name);
    if (uiThemeEl) uiThemeEl.value = name;
    chipMetricsCache.clear();
    if (!opts || !opts.silent) {
      apiPost('/api/settings', { uiTheme: name })
        .catch(function () { /* a theme choice is never worth a toast */ });
    }
  }

  function loadUiTheme() {
    return apiGet('/api/settings').then(function (d) {
      var s = (d && d.settings) || {};
      applyUiTheme(s.uiTheme || DEFAULT_UI_THEME, { silent: true });
    }).catch(function () {
      applyUiTheme(DEFAULT_UI_THEME, { silent: true });
    });
  }

  function renderSource(d) {
    var st = state.source;
    st.open = true;
    st.doc = !!d.doc;
    st.file = d.path || d.file || '';
    st.start = d.start || 0;
    st.end = d.end || 0;
    st.total = d.total || 0;
    if (d.line) st.line = d.line;
    if (st.doc) st.line = 0;      // the doc has its own numbering, not a file line
    // Prev/Next only makes sense inside the file that is actually displayed
    if (st.hits.length && st.hits[0].file !== st.file) { st.hits = []; st.hitIndex = -1; }

    var docLabel = (d.node && (d.node.full || d.node.id)) || d.node || '';
    st.nodeLabel = docLabel;
    st.rawAvailable = !st.doc || !!(d.node && d.node.file);
    sourceTitleEl.textContent = st.doc
      ? ('doc · ' + docLabel)
      : ((st.mode === 'node' && st.start)
         ? (st.file + ':' + st.start + '-' + st.end)
         : (st.file + (st.line ? ':' + st.line : '')));
    sourceTitleEl.title = st.doc && d.path ? d.path : sourceTitleEl.textContent;

    st.payload = d;
    var lines = d.lines || [];
    st.lines = lines;
    st.srcMode = d.mode || st.srcMode;
    var capped = monacoOn() ? lines.length
      : Math.min(lines.length, st.doc ? SOURCE_MAX_DOC_LINES : SOURCE_MAX_LINES);
    var frag = monacoOn() ? null : document.createDocumentFragment();
    if (st._file !== st.file) { st._file = st.file; st.blockComment = false; }
    var blockState = { block: st.blockComment || false };
    for (var i = 0; frag && i < capped; i++) {
      var ln = lines[i];
      var ctx = ln.ctx || 'body';
      if (ctx === 'part') blockState.block = false;   // new file: no carry-over
      var declared = (st.mode === 'node' && !st.doc && ctx === 'body' &&
                      ln.n >= st.start && ln.n <= st.end);
      var row = document.createElement('div');
      row.className = 'sline ctx-' + ctx +
        (ln.n && ln.n === st.line ? ' target' : '') + (declared ? ' decl' : '') +
        (ln.sec ? ' sec-' + ln.sec : '') +
        (ln.active === false ? ' inactive' : '');
      var num = document.createElement('span');
      num.className = 'ln' + (ln.n ? ' clickable' : '');
      num.textContent = ln.n ? String(ln.n) : '';
      if (ln.n) {
        num.title = ln.src
          ? (ln.src.file + ':' + ln.src.line)
          : ('Select the node for line ' + ln.n);
        if (ln.src) {
          num.addEventListener('click', function (l2) {
            return function () { selectLineNode(l2); };
          }(ln));
        } else {
          num.addEventListener('click', function (n2) {
            return function () { selectLineNode(n2); };
          }(ln));
        }
      } else {
        num.innerHTML = '&nbsp;';
      }
      var txt = document.createElement('span');
      txt.className = 'lt';
      var toks = tokenizeCsLine(ln.text, blockState);
      var needle = st.match || '';
      toks.forEach(function (tk) {
        var needleIdx = needle ? tk.text.indexOf(needle) : -1;
        if (needleIdx < 0) {
          var sp = document.createElement('span');
          sp.className = tk.cls;
          sp.textContent = tk.text;
          txt.appendChild(sp);
          return;
        }
        // emphasise the query inside the token, keeping the token colour
        if (needleIdx > 0) {
          var a = document.createElement('span');
          a.className = tk.cls;
          a.textContent = tk.text.slice(0, needleIdx);
          txt.appendChild(a);
        }
        var mark = document.createElement('mark');
        var inner = document.createElement('span');
        inner.className = tk.cls;
        inner.textContent = tk.text.slice(needleIdx, needleIdx + needle.length);
        mark.appendChild(inner);
        txt.appendChild(mark);
        var rest = tk.text.slice(needleIdx + needle.length);
        if (rest) {
          var b = document.createElement('span');
          b.className = tk.cls;
          b.textContent = rest;
          txt.appendChild(b);
        }
      });
      row.appendChild(num);
      row.appendChild(txt);
      frag.appendChild(row);
    }
    if (frag) {
      if (capped) st.blockComment = blockState.block;
      sourceBodyEl.innerHTML = '';
      sourceBodyEl.appendChild(frag);
    } else {
      // Monaco holds the buffer as a model and paints only the viewport, so a
      // whole-corpus document costs the same to open as a short file. That is
      // the whole point of v2: no per-line DOM, no full-document layout.
      window.CsEditor.show(lines, {
        match: st.match,
        target: st.line,
        onClickLine: selectLineNode
      });
    }

    var note;
    if (st.doc) {
      if (d.external) {
        // The engine answered with the symbol's usage sites because its own
        // declaration is not in the corpus. Say so, and point at the one action
        // that fixes it — adding the folder it lives in to the index.
        note = 'external symbol · ' + fmtNum(st.total) + ' reference' +
          (st.total === 1 ? '' : 's') + ' shown · add its folder (Browse → Index) to see the declaration';
        if (window.toast && st._externalNotified !== docLabel) {
          st._externalNotified = docLabel;
          toast(docLabel + ' is declared outside the indexed roots — Browse → Index its folder to include it', 'warn');
        }
      } else {
        note = 'generated document · ' + fmtNum(st.total) + ' lines';
        if (d.sites) note += ' · ' + d.sites + ' declaration sites';
        if (d.sections) note += ' · ' + d.sections + ' conditional sections';
        note += d.definesKnown
          ? ' · #if branches kept as sections'
          : ' · all #if branches kept (no symbols found)';
        if (d.path) note += ' · ' + d.path;
      }
    } else {
      note = 'lines ' + st.start + '–' + st.end + ' of ' + fmtNum(st.total);
      if (st.mode === 'node') note = 'whole declaration · ' + note;
      if (st.srcMode === 'effective') {
        note = 'effective' + (d.definesKnown ? '' : ' (no symbols found)') + ' · ' + note;
        if (d.parts && d.parts.length > 1) {
          note += ' · ' + d.parts.length + ' partial declarations';
        }
      }
    }
    if (lines.length > capped) note += ' · showing first ' + capped;
    if (st.hits.length) note += ' · hit ' + (st.hitIndex + 1) + '/' + st.hits.length + ' in this file';
    sourceNoteEl.textContent = note;

    sourceMatchTagEl.classList.toggle('hidden', !st.matchKind);
    if (st.matchKind) sourceMatchTagEl.textContent = st.matchKind === 'exact' ? 'fq' : 'short';

    renderSourceChips();
    updateSourceButtons();

    var target = frag ? sourceBodyEl.querySelector('.sline.target') : null;
    if (target && target.scrollIntoView) target.scrollIntoView({ block: 'center' });

    // Another window may be showing this document (a detached Source pane, or a
    // document window); tell it what the document now is.
    if (window.CSSync) window.CSSync.push();
  }

  /** Clicking a line number selects a sensible node for that line. */
  function selectLineNode(lnOrLine) {
    var st = state.source;
    var file = st.file, lineNo = lnOrLine;
    if (lnOrLine && typeof lnOrLine === 'object') {
      lineNo = (lnOrLine.src && lnOrLine.src.line) || lnOrLine.n;
      file = (lnOrLine.src && lnOrLine.src.file) || st.file;
    }
    var usages = (state.find.data && state.find.data.usages) || [];
    for (var i = 0; i < usages.length; i++) {
      var u = usages[i];
      if (u.file === file && u.line === lineNo && u.node && u.node.id) { goToNode(u.node.id); return; }
    }
    if (st.mode === 'node' && lineNo >= st.start && lineNo <= st.end && state.selected) {
      goToNode(state.selected);
      return;
    }
    if (state.selected) goToNode(state.selected);
  }

  function renderSourceChips() {
    sourceChipWrapEl.innerHTML = '';
    var n = state.source.usageNode;
    if (!n) return;
    var b = document.createElement('button');
    b.type = 'button';
    b.className = 'source-chip';
    b.title = 'Select this method in the graph';
    b.textContent = '⊕ ' + (n.label || n.name || n.id) + (n.kind ? ' · ' + n.kind : '');
    b.addEventListener('click', function () { openFindNode(n.id); });
    sourceChipWrapEl.appendChild(b);
  }

  function updateSourceButtons() {
    var st = state.source;
    var hasFileLine = !!st.file && !!st.line;
    $('#srcMethodBtn').disabled = !(st.usageNode || st.nodeId);
    $('#srcWindowBtn').disabled = !hasFileLine || st.doc;
    $('#srcPrevBtn').disabled = !st.hits.length;
    $('#srcNextBtn').disabled = !st.hits.length;
    $('#srcCopyBtn').disabled = !(st.file || (st.lines && st.lines.length));
    $('#srcRefBtn').disabled = !st.file;
    var modeBtn = $('#srcModeBtn');
    if (modeBtn) {
      modeBtn.textContent = st.srcMode === 'raw' ? 'Raw' : 'Doc';
      modeBtn.title = st.srcMode === 'raw'
        ? 'Showing the file as written — switch to the synthesized whole-node document'
        : 'Every declaration site gathered into one document (#if branches kept as sections) — switch to the file as written';
    }
    $('#srcMethodBtn').title = st.usageNode
      ? 'Show the whole declaration of ' + (st.usageNode.label || st.usageNode.id)
      : 'Show the whole declaration';
    var popBtn = $('#srcPopBtn');
    if (popBtn) popBtn.disabled = !(st.nodeId || st.usageNode || (st.file && st.line));
  }

  function sourceWholeMethod() {
    var st = state.source;
    if (st.usageNode) openSource({ node: st.usageNode.id });
    else if (st.nodeId) openSource({ node: st.nodeId });
  }

  /** Flip between the synthesized document and the file as written. */
  function toggleSourceMode() {
    var st = state.source;
    var next = st.srcMode === 'raw' ? 'doc' : 'raw';
    if (next === 'raw' && !st.rawAvailable) {
      toast('No single file for this node — it has no declaration site of its own', 'warn');
      return;
    }
    if (st.usageNode) openSource({ node: st.usageNode.id, mode: next });
    else if (st.nodeId) openSource({ node: st.nodeId, mode: next });
    else if (st.file && st.line) {
      openSource({ file: st.file, line: st.line, context: SOURCE_CONTEXT, mode: next });
    } else {
      st.srcMode = next;
      updateSourceButtons();
    }
  }

  function sourceStep(dir) {
    var st = state.source;
    if (!st.hits.length) return;
    var idx = st.hitIndex + dir;
    if (idx < 0) idx = st.hits.length - 1;
    if (idx >= st.hits.length) idx = 0;
    st.hitIndex = idx;
    applyUsage(st.hits[idx], true);
  }

  function copyText(text, label) {
    if (!text) return false;
    var done = function () { toast('Copied ' + label, 'ok'); };
    var fallback = function () {
      var ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      try { document.execCommand('copy'); done(); }
      catch (e) { toast('Copy failed', 'error'); }
      ta.remove();
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, fallback);
    } else {
      fallback();
    }
    return true;
  }

  /**
   * Copy the code exactly as shown: line numbers and the partial-declaration
   * separator are viewer chrome, not source, so they are left out.
   */
  function copySourceCode() {
    var st = state.source;
    var lines = (st.lines || []).filter(function (l) { return l.ctx !== 'part'; });
    if (!lines.length) { toast('Nothing to copy', 'error'); return; }
    var text = lines.map(function (l) { return l.text; }).join('\n').replace(/\s+$/, '');
    copyText(text, lines.length + ' lines of code');
  }

  function copySourceRef() {
    var st = state.source;
    if (!st.file) return;
    copyText(st.file + (st.line ? ':' + st.line : ''), st.file);
  }

