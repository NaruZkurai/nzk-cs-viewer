/* csgraph v2 — every secondary window the viewer can open.
 *
 * Part of the RUST-ONLY v2 viewer. Not loaded by v1.
 *
 * Two kinds of window:
 *
 *   * a DOCUMENT window (`⧉ New window`) — one document, nothing else. Each
 *     Tauri WebviewWindow runs its own webview and its own JavaScript main
 *     thread, so a pathological document in one window cannot stall the graph
 *     canvas or another document; each owns a separate Monaco instance, and
 *     Monaco tokenizes in its own Web Worker;
 *   * a PANE window — a pane detached from the main window (Sidebar, Source,
 *     Node, Find) into a real operating-system window that renders outside the
 *     main window, the way Unity's editor panels do. `panels.js` asks for one
 *     and `panewin.js` renders it.
 *
 * Windows stay in step over `CsWindows.bus`: Tauri events in the native build,
 * a BroadcastChannel in the plain-web build.
 *
 * In the plain-web build (no Tauri) `openDoc` degrades to `window.open` and
 * pane detaching degrades to the in-page `#floatLayer`, which is all a browser
 * can do.
 */
'use strict';

window.CsWindows = (function () {
  var SELF = Math.random().toString(36).slice(2);
  var CHANNEL = 'csviewer.windows';

  function tauri() { return window.__TAURI__ || null; }
  function tauriInvoke() {
    var t = tauri();
    return (t && t.core && typeof t.core.invoke === 'function') ? t.core.invoke : null;
  }
  function tauriEvents() {
    var t = tauri();
    return (t && t.event && typeof t.event.emit === 'function') ? t.event : null;
  }

  /** True when this build can open real operating-system windows. */
  function native() { return !!tauriInvoke(); }

  // ------------------------------------------------------------------ bus

  var listeners = {};
  var busReady = false;
  var channel = null;

  function broadcastChannel() {
    if (channel === null) {
      try {
        channel = ('BroadcastChannel' in window) ? new BroadcastChannel(CHANNEL) : false;
      } catch (e) { channel = false; }
    }
    return channel;
  }

  function deliver(name, payload) {
    var fns = listeners[name];
    if (!fns) return;
    fns.forEach(function (fn) {
      try { fn(payload); } catch (e) { /* one bad listener must not stop the rest */ }
    });
  }

  /** Listen for a bus message. Safe to call before the transport exists. */
  function on(name, fn) {
    (listeners[name] = listeners[name] || []).push(fn);
    wire();
  }

  /** Broadcast a bus message to the other windows (never to this one). */
  function emit(name, payload) {
    var msg = payload || {};
    msg.from = SELF;
    var ev = tauriEvents();
    if (ev) {
      var sent = ev.emit(name, msg);
      if (sent && typeof sent.catch === 'function') {
        sent.catch(function () { /* no listener on the other side is fine */ });
      }
      return;
    }
    var ch = broadcastChannel();
    if (ch) ch.postMessage({ name: name, payload: msg });
  }

  var EVENT_NAMES = ['cs:sync', 'cs:request', 'cs:action', 'cs:pane-open', 'cs:win-closed'];

  function wire() {
    if (busReady) return;
    busReady = true;
    var ev = tauriEvents();
    if (ev && typeof ev.listen === 'function') {
      EVENT_NAMES.forEach(function (name) {
        ev.listen(name, function (e) {
          var payload = (e && e.payload) || {};
          if (payload.from === SELF) return;      // never react to ourselves
          deliver(name, payload);
        });
      });
      return;
    }
    var ch = broadcastChannel();
    if (ch) {
      ch.onmessage = function (e) {
        var d = e.data || {};
        if (d.payload && d.payload.from === SELF) return;
        deliver(d.name, d.payload);
      };
    }
  }

  // ------------------------------------------------------------------ doc windows

  /**
   * The target handed to this window.
   *
   * The native host injects `window.__CS_DOC__` before any page script runs,
   * because a Tauri `WebviewUrl::App` path is run through `Url::join`, which is
   * allowed to normalise or drop a query. The URL query is still honoured (the
   * plain-web build has nothing else), but the injected value wins: a document
   * window must always know what it was asked to show.
   */
  function target() {
    var injected = window.__CS_DOC__;
    if (injected && typeof injected === 'object') {
      return {
        node: injected.node || null,
        file: injected.file || null,
        line: injected.line ? Number(injected.line) : 0,
        mode: injected.mode || 'doc'
      };
    }
    var q;
    try { q = new URLSearchParams(window.location.search); } catch (e) { q = new URLSearchParams(''); }
    return {
      node: q.get('node'),
      file: q.get('file'),
      line: Number(q.get('line')) || 0,
      mode: q.get('mode') || 'doc'
    };
  }

  function isDocWindow() {
    if (window.__CS_WIN__ && window.__CS_WIN__.kind === 'pane') return false;
    if (window.__CS_DOC__ && typeof window.__CS_DOC__ === 'object') return true;
    try {
      return new URLSearchParams(window.location.search).get('win') === 'doc';
    } catch (e) { return false; }
  }

  /** Split a `path:line` string into a file window target. */
  function splitFileLine(s) {
    if (!s) return null;
    var i = s.lastIndexOf(':');
    if (i <= 0) return null;
    var line = parseInt(s.slice(i + 1), 10);
    if (!isFinite(line) || line <= 0) return null;
    return { file: s.slice(0, i), line: line };
  }

  function docQuery(node, mode) {
    var q = new URLSearchParams();
    q.set('win', 'doc');
    if (node) q.set('node', node);
    q.set('mode', mode || 'doc');
    return q;
  }

  /** Open `node` in a new, fully isolated window. */
  function openDoc(node, mode) {
    var invoke = tauriInvoke();
    if (invoke) {
      return invoke('open_doc_window', { node: node || null, mode: mode || 'doc' })
        .catch(function (err) {
          if (window.toast) toast('Could not open a document window: ' + err, 'error');
          return null;
        });
    }
    var url = '/?' + docQuery(node, mode).toString();
    window.open(url, '_blank', 'width=1180,height=860,noopener');
    return Promise.resolve(null);
  }

  /** Open a `file:line` window target (a usage site) in its own window too. */
  function openFile(file, line, mode) {
    var invoke = tauriInvoke();
    if (invoke) {
      return invoke('open_doc_window', {
        node: null,
        file: file || null,
        line: line || 0,
        mode: mode || 'raw'
      }).catch(function (err) {
        if (window.toast) toast('Could not open a document window: ' + err, 'error');
        return null;
      });
    }
    var q = new URLSearchParams();
    q.set('win', 'doc');
    if (file) q.set('file', file);
    if (line) q.set('line', String(line));
    q.set('mode', mode || 'raw');
    window.open('/?' + q.toString(), '_blank', 'width=1180,height=860,noopener');
    return Promise.resolve(null);
  }

  // ------------------------------------------------------------------ pane windows

  /**
   * Detach `pane` into its own operating-system window (or focus the one that is
   * already open). `view` is the pane's current view state — focus node, source
   * target, find query, filters, theme — so the new window paints what the main
   * window was showing instead of booting blank.
   *
   * Resolves to the window label, or `null` when this build cannot open native
   * windows (the caller then falls back to the in-page float layer).
   */
  function openPane(pane, view) {
    var invoke = tauriInvoke();
    if (!invoke) return Promise.resolve(null);
    return invoke('open_pane_window', { pane: pane, state: view || null })
      .catch(function (err) {
        if (window.toast) toast('Could not detach ' + pane + ': ' + err, 'error');
        return null;
      });
  }

  /** Redock a detached pane by closing its window (which raises `cs:win-closed`). */
  function closePane(pane) {
    var invoke = tauriInvoke();
    if (!invoke) return Promise.resolve(null);
    return invoke('close_pane_window', { pane: pane }).catch(function () { return null; });
  }

  /** `{pane, state}` when this window *is* a detached pane, else null. */
  function paneWindow() {
    var injected = window.__CS_WIN__;
    if (injected && injected.kind === 'pane') return injected;
    try {
      var q = new URLSearchParams(window.location.search);
      if (q.get('win') === 'pane') return { kind: 'pane', pane: q.get('pane'), state: null };
    } catch (e) { /* ignore */ }
    return null;
  }

  function injectStyle() {
    if (document.getElementById('cs-docwin-style')) return;
    var css = document.createElement('style');
    css.id = 'cs-docwin-style';
    // A document window is the document and nothing else. Every piece of the
    // graph chrome — the top bar, the sidebar, the canvas, the overlays and the
    // panel splitters — is hidden; only the source pane remains, full height.
    css.textContent = [
      'html[data-win="doc"] .topbar,',
      'html[data-win="doc"] .side,',
      'html[data-win="doc"] #findPanel,',
      'html[data-win="doc"] #nodeDrawer,',
      'html[data-win="doc"] #canvasWrap,',
      'html[data-win="doc"] .pane-splitter,',
      'html[data-win="doc"] .dock-hint,',
      'html[data-win="doc"] #floatLayer { display: none !important; }',
      'html[data-win="doc"],',
      'html[data-win="doc"] body { overflow: hidden; }',
      'html[data-win="doc"] .layout { display: block !important; height: 100vh !important; }',
      'html[data-win="doc"] .source-pane { height: 100vh; border-top: 0; }',
      'html[data-win="doc"] #sourceBody { overflow: hidden; }'
    ].join('\n');
    document.head.appendChild(css);
  }

  /** A document window must never sit blank: say what went wrong, in the pane. */
  function showDocMessage(title, detail) {
    // The source pane starts hidden; a window with no document still has to open
    // it, or "nothing to show" is indistinguishable from "failed to render".
    if (typeof showSourcePane === 'function') showSourcePane();
    if (typeof sourceTitleEl !== 'undefined' && sourceTitleEl) sourceTitleEl.textContent = title;
    if (typeof sourceNoteEl !== 'undefined' && sourceNoteEl) sourceNoteEl.textContent = detail || '';
    var editorLive = window.CsEditor && window.CsEditor.editor && window.CsEditor.editor();
    if (!editorLive && typeof sourceBodyEl !== 'undefined' && sourceBodyEl) {
      sourceBodyEl.classList.remove('monaco-on');
      sourceBodyEl.textContent = detail || '';
    }
  }

  /**
   * Called once the app has indexed and the node table is populated (see the
   * hook in app.js). A doc window shows nothing but the requested document.
   *
   * Returns true whenever this *is* a document window, even when the target is
   * missing, so app.js never falls through to loading the graph (whose chrome
   * is hidden, which would leave the window visually empty).
   */
  function bootDocWindow() {
    if (!isDocWindow()) return false;
    injectStyle();
    document.documentElement.setAttribute('data-win', 'doc');

    var t = target();
    if (typeof openSource !== 'function') {
      showDocMessage('Document', 'The viewer is still loading.');
      return true;
    }

    if (t.node) {
      // Older pop-out links hand over `file:line` as if it were a node id; the
      // engine accepts that too, but keep the intent explicit here.
      var fl = splitFileLine(t.node);
      if (fl && !t.file) {
        openSource({ file: fl.file, line: fl.line, mode: t.mode === 'doc' ? 'raw' : t.mode });
      } else {
        openSource({ node: t.node, mode: t.mode });
      }
      return true;
    }
    if (t.file) {
      openSource({ file: t.file, line: t.line || 1, mode: t.mode });
      return true;
    }
    showDocMessage('Document', 'No document or file was requested for this window.');
    return true;
  }

  wire();

  return {
    // document windows
    isDocWindow: isDocWindow,
    openDoc: openDoc,
    openFile: openFile,
    bootDocWindow: bootDocWindow,
    // pane windows
    native: native,
    openPane: openPane,
    closePane: closePane,
    paneWindow: paneWindow,
    // cross-window bus
    on: on,
    emit: emit,
    self: function () { return SELF; }
  };
})();
