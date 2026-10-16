/* csgraph v2 — source-pane navigation: history, breadcrumbs, side peek, IDE.
 *
 * Part of the RUST-ONLY v2 viewer.
 *
 * (a) Alt+Left / Alt+Right walk a private source-pane history (up to 100
 *     entries, consecutive duplicates removed) and fall back to the graph's own
 *     `navigateBack()` when the pane stack has nothing behind it. Monaco's
 *     `navigateBack` / `navigateForward` editor actions do not exist in this
 *     build, so the stack is the viewer's own.
 * (b) A breadcrumb bar under the source-pane header, built from `/api/decls`
 *     plus `CsLang`, for both synthesized documents (namespace → type → member)
 *     and raw file windows (… → file:line). Every segment is clickable except
 *     the current one.
 * (c) "Def to side" (Alt+O): `editor.action.revealDefinitionAside` when the
 *     declaration is inside the open document, otherwise a second document
 *     window via `CsWindows` (the windows.js/CSSync mechanism).
 * (d) "Open in IDE" (Alt+I): `open_external` in the native (Tauri) build, an
 *     honest toast in the plain-web build.
 *
 * This Monaco build has no built-in breadcrumbs (`grep -c breadcrumbs
 * vendor/monaco/monaco.d.ts` is 0), so (b) is a real implementation rather than
 * a configuration flag.
 */
'use strict';

window.CsNav = (function () {
  var MAX_HISTORY = 100;
  var RESTORE_GRACE = 2000;     // ms during which a model change is a restore

  var ctx = null;
  var history = [];
  var index = -1;
  var restoringUntil = 0;
  var windowBound = false;

  var crumbEl = null;
  var crumbSeq = 0;
  var declsCache = {};
  var lastCrumbs = [];
  var asideBtn = null, ideBtn = null;

  function apiGet() {
    var fn = (ctx && ctx.apiGet) || window.apiGet;
    return (typeof fn === 'function') ? fn : null;
  }

  function toast(msg, kind) {
    var fn = (ctx && ctx.toast) || window.toast;
    if (typeof fn === 'function') fn(msg, kind || 'warn');
  }

  function srcState() {
    return (window.state && window.state.source) || (ctx && ctx.state && ctx.state.source) || {};
  }

  // -------------------------------------------------------------- (a) history

  function entryLabel(e) {
    if (e.label) return e.label;
    if (e.nodeId) return e.nodeId;
    return (e.file || '') + (e.line ? ':' + e.line : '');
  }

  /** The document the pane is showing right now, as a history entry. */
  function currentEntry() {
    var st = srcState();
    var model = ctx && ctx.model ? ctx.model() : (window.CsEditor && window.CsEditor.model());
    var fileOf = null;
    if (ctx && ctx.lang && ctx.lang.fileOfModel) {
      try { fileOf = ctx.lang.fileOfModel(model); } catch (e) { fileOf = null; }
    }
    if (fileOf) {
      var line = 1;
      if (ctx && ctx.editor && ctx.editor.getPosition) {
        var pos = ctx.editor.getPosition();
        if (pos) line = pos.lineNumber;
      }
      return { file: fileOf, line: line, mode: 'raw', nodeId: null,
               start: line, end: line, label: fileOf + ':' + line };
    }
    var usageId = st.usageNode && st.usageNode.id;
    var label = st.nodeLabel || (st.nodeId ? st.nodeId : '') || st.file || usageId || '';
    if (st.doc) {
      return {
        nodeId: st.nodeId || usageId || null, file: st.file || '', line: 0,
        mode: 'doc', label: label || 'document'
      };
    }
    return {
      nodeId: st.nodeId || null, file: st.file || '', line: st.line || 0,
      mode: st.srcMode || 'raw', start: st.start || 0, end: st.end || 0,
      label: label ? (label + (st.file ? ':' + (st.line || '') : ''))
                   : ((st.file || '') + ':' + (st.line || ''))
    };
  }

  function sigOf(e) {
    return [e.mode || '', e.nodeId || '', e.file || '', e.line || 0,
            e.start || 0, e.end || 0].join('|');
  }

  function push(e) {
    if (!e) return;
    var s = sigOf(e);
    if (index >= 0 && history[index] && sigOf(history[index]) === s) return;  // dedupe
    if (index < history.length - 1) history = history.slice(0, index + 1);    // truncate forward
    history.push(e);
    if (history.length > MAX_HISTORY) history.shift();
    index = history.length - 1;
  }

  function apply(e) {
    if (!e || !ctx || !ctx.openSource) return;
    restoringUntil = Date.now() + RESTORE_GRACE;
    if (e.nodeId) {
      ctx.openSource({ node: e.nodeId, mode: e.mode === 'raw' ? 'raw' : 'doc' });
    } else if (e.file) {
      ctx.openSource({ file: e.file, line: e.line || 1, mode: e.mode || 'raw' });
    }
  }

  /** Alt+Left: the previous pane document, else the graph's own back stack. */
  function back() {
    if (index > 0) {
      index--;
      apply(history[index]);
      return history[index];
    }
    if (typeof window.navigateBack === 'function') {
      window.navigateBack();
      return null;
    }
    toast('Nothing to go back to', 'warn');
    return null;
  }

  /** Alt+Right: forward through the pane stack only. */
  function forward() {
    if (index >= 0 && index < history.length - 1) {
      index++;
      apply(history[index]);
      return history[index];
    }
    return null;
  }

  function onModel() {
    if (Date.now() >= restoringUntil) push(currentEntry());
    renderCrumbs();
  }

  // ----------------------------------------------------------- (b) breadcrumbs

  function injectStyle() {
    if (document.getElementById('cs-nav-style')) return;
    var css = document.createElement('style');
    css.id = 'cs-nav-style';
    css.textContent = [
      '#cs-nav-crumbs { display: flex; align-items: center; flex-wrap: nowrap;',
      '  gap: 2px; overflow-x: auto; padding: 3px 10px; min-height: 24px;',
      '  background: var(--bg-panel, #161625); border-bottom: 1px solid var(--border-soft, #232c42);',
      '  font-family: var(--mono, monospace); font-size: 10.5px; scrollbar-width: thin; }',
      '#cs-nav-crumbs.hidden { display: none; }',
      '#cs-nav-crumbs .cs-crumb-sep { flex: none; color: var(--text-faint, #6b7897); padding: 0 1px; }',
      '#cs-nav-crumbs .cs-crumb { flex: none; max-width: 32ch; overflow: hidden;',
      '  text-overflow: ellipsis; white-space: nowrap; padding: 1px 5px; border: 0;',
      '  border-radius: 4px; background: transparent; color: var(--text-dim, #9aa6c2);',
      '  font: inherit; cursor: pointer; }',
      '#cs-nav-crumbs .cs-crumb:hover:not(:disabled) { background: rgba(134,170,249,.14);',
      '  color: var(--text, #e3e8f4); }',
      '#cs-nav-crumbs .cs-crumb.cs-crumb-current { color: var(--text, #e3e8f4); font-weight: 600; cursor: default; }',
      '#cs-nav-crumbs .cs-crumb.cs-crumb-static { cursor: default; }',
      '#cs-nav-crumbs .cs-crumb-file { color: var(--vt-info, #42c3d0); }'
    ].join('\n');
    document.head.appendChild(css);
  }

  function ensureDom() {
    injectStyle();
    if (!crumbEl) {
      var pane = document.getElementById('sourcePane');
      var head = pane && pane.querySelector('.source-head');
      if (head && head.parentNode) {
        crumbEl = document.createElement('div');
        crumbEl.id = 'cs-nav-crumbs';
        crumbEl.className = 'cs-crumbs hidden';
        crumbEl.setAttribute('aria-label', 'Breadcrumbs');
        head.parentNode.insertBefore(crumbEl, head.nextSibling);
      }
    }
    var headEl = document.querySelector('#sourcePane .source-head');
    if (headEl && !asideBtn) {
      var closeBtn = document.getElementById('srcCloseBtn');
      asideBtn = document.createElement('button');
      asideBtn.id = 'csAsideBtn';
      asideBtn.type = 'button';
      asideBtn.className = 'btn btn-ghost';
      asideBtn.textContent = 'Def to side';
      asideBtn.title = 'Reveal the definition in a side peek, or open it in a second window (Alt+O)';
      asideBtn.addEventListener('click', function () { definitionAside(); });

      ideBtn = document.createElement('button');
      ideBtn.id = 'csOpenIdeBtn';
      ideBtn.type = 'button';
      ideBtn.className = 'btn btn-ghost';
      ideBtn.textContent = 'Open in IDE';
      ideBtn.title = 'Open this file at this line in your real editor (Alt+I)';
      ideBtn.addEventListener('click', function () { openInIde(); });

      headEl.insertBefore(ideBtn, closeBtn || null);
      headEl.insertBefore(asideBtn, ideBtn);
    }
  }

  function declsKey(params) {
    return params.node ? ('n:' + params.node) : ('f:' + params.file);
  }

  function fetchDecls(params) {
    var api = apiGet();
    var key = declsKey(params);
    if (declsCache[key]) return Promise.resolve(declsCache[key]);
    if (!api) return Promise.resolve([]);
    return api('/api/decls', params).then(function (d) {
      var rows = (d && d.decls) || [];
      declsCache[key] = rows;
      var keys = Object.keys(declsCache);
      if (keys.length > 48) delete declsCache[keys[0]];
      return rows;
    }).catch(function () { return []; });
  }

  function findRowByFull(decls, prefix) {
    var i, r;
    for (i = 0; i < decls.length; i++) {
      r = decls[i];
      if ((r.full || '') === prefix) return r;
    }
    for (i = 0; i < decls.length; i++) {
      r = decls[i];
      var full = r.full || '';
      if (full.length > prefix.length && full.indexOf(prefix) === 0 && full.charAt(prefix.length) === '(') return r;
    }
    return null;
  }

  function innermost(decls, file, line) {
    var best = null, bestSpan = Infinity;
    for (var i = 0; i < decls.length; i++) {
      var r = decls[i];
      if (r.file && file && r.file !== file) continue;
      if (!r.line || r.line > line) continue;
      var end = r.endLine || r.line;
      if (line > end) continue;
      var span = end - r.line;
      if (span < bestSpan || (span === bestSpan && best && r.line > best.line)) {
        best = r; bestSpan = span;
      }
    }
    if (best) return best;
    // No enclosing declaration: the nearest one above the line.
    for (var j = 0; j < decls.length; j++) {
      var c = decls[j];
      if (c.line && c.line <= line && (!best || c.line > best.line)) best = c;
    }
    return best;
  }

  /**
   * Pure breadcrumb builder (test hook): decls rows + the current node id or
   * file:line -> an ordered list of segments.
   */
  function buildCrumbs(decls, rootId, file, line) {
    decls = decls || [];
    var root = null, i;
    if (rootId) {
      for (i = 0; i < decls.length; i++) {
        if (decls[i].id === rootId) { root = decls[i]; break; }
      }
    } else if (file) {
      root = innermost(decls, file, line || 1);
    }
    var parts = [];
    if (root) {
      var full = String(root.full || root.label || root.name || root.id || '');
      var bits = full.split('.').filter(Boolean);
      for (i = 1; i <= bits.length; i++) {
        var prefix = bits.slice(0, i).join('.');
        var match = (i === bits.length && root.full === prefix) ? root : findRowByFull(decls, prefix);
        var label = match ? (match.name || match.label || bits[i - 1]) : bits[i - 1];
        parts.push({
          label: label, id: match ? match.id : null, kind: (match && match.kind) || '',
          full: prefix, current: false
        });
      }
    }
    if (file) {
      parts.push({
        label: (file.split('/').pop() || file) + (line ? ':' + line : ''),
        file: file, line: line || 1, id: null, kind: 'file', current: true
      });
    } else if (parts.length) {
      parts[parts.length - 1].current = true;
    }
    return parts;
  }

  function setCrumbs(list) {
    lastCrumbs = list || [];
    if (!crumbEl) return;
    crumbEl.innerHTML = '';
    if (!lastCrumbs.length) { crumbEl.classList.add('hidden'); return; }
    crumbEl.classList.remove('hidden');
    var frag = document.createDocumentFragment();
    lastCrumbs.forEach(function (seg, i) {
      if (i) {
        var sep = document.createElement('span');
        sep.className = 'cs-crumb-sep';
        sep.textContent = '›';
        frag.appendChild(sep);
      }
      var b = document.createElement('button');
      b.type = 'button';
      b.className = 'cs-crumb' + (seg.current ? ' cs-crumb-current' : '') +
        (seg.kind === 'file' ? ' cs-crumb-file' : '');
      b.textContent = seg.label;
      b.title = seg.full || (seg.file ? seg.file + ':' + seg.line : seg.label);
      if (seg.current || (!seg.id && !seg.file)) {
        b.disabled = true;
        b.classList.add('cs-crumb-static');
      } else {
        b.addEventListener('click', function () { openCrumb(seg); });
      }
      frag.appendChild(b);
    });
    crumbEl.appendChild(frag);
  }

  function openCrumb(seg) {
    if (!ctx || !ctx.openSource) return;
    if (seg.file) { ctx.openSource({ file: seg.file, line: seg.line || 1, mode: 'raw' }); return; }
    if (seg.id) {
      ctx.openSource({ node: seg.id, mode: 'doc' });
      if (ctx.goToNode) { try { ctx.goToNode(seg.id); } catch (e) { /* ignore */ } }
    }
  }

  /** Rebuild the breadcrumb bar for whatever the pane is showing now. */
  function renderCrumbs() {
    ensureDom();
    if (!crumbEl) return;
    var st = srcState();
    if (!st.open) { setCrumbs([]); return; }
    var mine = ++crumbSeq;
    if (st.doc) {
      var id = st.nodeId || (st.usageNode && st.usageNode.id) || null;
      if (!id) { setCrumbs([]); return; }
      fetchDecls({ node: id }).then(function (decls) {
        if (mine !== crumbSeq) return;
        setCrumbs(buildCrumbs(decls, id, null, 0));
      });
    } else {
      var file = st.file, line = st.line || 1;
      if (!file) { setCrumbs([]); return; }
      fetchDecls({ file: file }).then(function (decls) {
        if (mine !== crumbSeq) return;
        setCrumbs(buildCrumbs(decls, null, file, line));
      });
    }
  }

  // ------------------------------------------------------ (c) definition aside

  function definitionAside() {
    if (!ctx || !ctx.editor || !ctx.lang) return Promise.resolve(null);
    var model = ctx.model();
    var pos = ctx.editor.getPosition ? ctx.editor.getPosition() : null;
    if (!model || !pos) { toast('Put the cursor on a symbol first', 'warn'); return Promise.resolve(null); }
    var info = ctx.lang.infoAt(model, pos);
    if (!info || !info.word) { toast('There is no identifier under the cursor', 'warn'); return Promise.resolve(null); }
    return ctx.lang.resolveSymbol(info).then(function (def) {
      if (!def || !def.node) {
        toast('No indexed definition for “' + info.word + '”', 'warn');
        return null;
      }
      var node = def.node;
      var here = null;
      try { here = ctx.lang.docLocFor(model, node.file, node.line, info.word); } catch (e) { here = null; }
      if (here) {
        // The declaration is already in this document: VSCode's side peek.
        ctx.editor.setPosition(pos);
        ctx.editor.focus();
        ctx.editor.trigger('cs-nav', 'editor.action.revealDefinitionAside', {});
        return { via: 'doc', node: node.id };
      }
      if (window.CsWindows) {
        if (node.id) window.CsWindows.openDoc(node.id, 'doc');
        else window.CsWindows.openFile(node.file, node.line || 1, 'raw');
        return { via: 'window', node: node.id, file: node.file, line: node.line };
      }
      toast('Opening a second window is not available here', 'warn');
      return null;
    }).catch(function (err) {
      toast('Could not open the definition: ' + (err && err.message ? err.message : err), 'error');
      return null;
    });
  }

  // ------------------------------------------------------------ (d) open in IDE

  /** file/line to hand to the real editor, from the current source state. */
  function ideTarget() {
    var st = srcState();
    var node = st.payload && st.payload.node;
    if (st.doc && node) return { file: node.file || node.path || st.file || '', line: node.line || 0 };
    return { file: st.file || '', line: st.line || st.start || 0 };
  }

  function openInIde() {
    var t = ideTarget();
    if (!t.file) { toast('No file is open in the source pane', 'warn'); return null; }
    var line = t.line || 1;
    var core = window.__TAURI__ && window.__TAURI__.core;
    if (core && typeof core.invoke === 'function') {
      core.invoke('open_external', { file: t.file, line: line }).catch(function (err) {
        toast('open_external failed: ' + (err && err.message ? err.message : err), 'error');
      });
      return { file: t.file, line: line, native: true };
    }
    toast('Only available in the native build', 'warn');
    return { file: t.file, line: line, native: false };
  }

  // -------------------------------------------------------------- keybindings

  function onWindowKeyDown(ev) {
    // While the quick-open palette is up it owns the keyboard.
    if (window.CsQuickOpen && typeof window.CsQuickOpen.isOpen === 'function' && window.CsQuickOpen.isOpen()) return;
    if (ev.altKey && !ev.ctrlKey && !ev.metaKey && !ev.shiftKey) {
      if (ev.key === 'ArrowLeft') { ev.preventDefault(); ev.stopPropagation(); back(); return; }
      if (ev.key === 'ArrowRight') { ev.preventDefault(); ev.stopPropagation(); forward(); return; }
      if (ev.key === 'o' || ev.key === 'O') { ev.preventDefault(); ev.stopPropagation(); definitionAside(); return; }
      if (ev.key === 'i' || ev.key === 'I') { ev.preventDefault(); ev.stopPropagation(); openInIde(); return; }
    }
  }

  function bindWindow() {
    if (windowBound) return;
    windowBound = true;
    window.addEventListener('keydown', onWindowKeyDown, true);
  }

  // ------------------------------------------------------------------- attach

  function attach(c) {
    ctx = c;
    ensureDom();
    bindWindow();
    if (c && c.monaco && c.editor && c.editor.addCommand) {
      var M = c.monaco;
      try {
        c.editor.addCommand(M.KeyMod.Alt | M.KeyCode.KeyO, function () { definitionAside(); });
        c.editor.addCommand(M.KeyMod.Alt | M.KeyCode.KeyI, function () { openInIde(); });
      } catch (e) { /* the window binding still covers it */ }
    }
    if (c && c.on) c.on('model', onModel);
  }

  return {
    attach: attach,
    // test hooks
    history: function () { return history.slice(); },
    historyIndex: function () { return index; },
    push: push,
    back: back,
    forward: forward,
    crumbs: function () { return lastCrumbs.slice(); },
    buildCrumbs: buildCrumbs,
    render: renderCrumbs,
    open: renderCrumbs,
    openAside: definitionAside,
    openInIde: openInIde,
    ideTarget: ideTarget,
    current: currentEntry,
    clear: function () {
      history = []; index = -1; declsCache = {}; restoringUntil = 0;
      lastCrumbs = []; if (crumbEl) { crumbEl.innerHTML = ''; crumbEl.classList.add('hidden'); }
    }
  };
})();

window.CsEditorExtension({ name: 'nav', attach: window.CsNav.attach });
