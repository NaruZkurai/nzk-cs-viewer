/* csgraph v2 — a pane detached into its own operating-system window.
 *
 * Part of the RUST-ONLY v2 viewer. Not loaded by v1.
 *
 * Unity's editor panels can be dragged out of the main window and keep working
 * as real windows on the desktop. This is that window: `panels.js` asks
 * `CsWindows.openPane()` for it, the Rust host builds a `WebviewWindow` and
 * injects `window.__CS_WIN__`, and here the requested pane is moved out of the
 * app layout into `#paneRoot`, which fills the viewport.
 *
 * The window is a full viewer process of its own (its own webview, its own JS
 * main thread, its own Monaco worker) that has loaded the same graph from the
 * shared in-process engine, so every pane action — selecting a node, expanding a
 * relation, walking a chain, opening a document — works here exactly as it does
 * in the main window. `CSSync` keeps the two in step.
 */
'use strict';

window.CsPaneWin = (function () {
  var SELECTOR = {
    side: '.side',
    source: '#sourcePane',
    nodeDrawer: '#nodeDrawer',
    findPanel: '#findPanel'
  };

  /** Each pane's Close control: in a pane window it redocks instead of hiding. */
  var CLOSE_BTN = {
    source: '#srcCloseBtn',
    nodeDrawer: '#nodeDrawerClose',
    findPanel: '#findClose'
  };

  var TITLE = {
    side: 'Sidebar',
    source: 'Source',
    nodeDrawer: 'Node',
    findPanel: 'Find'
  };

  var listening = false;
  var findInputEl = null;

  function target() {
    return window.CsWindows ? window.CsWindows.paneWindow() : null;
  }

  function active() { return !!target(); }

  function paneId() {
    var t = target();
    return t ? t.pane : null;
  }

  /**
   * Turn this window into a single pane. Must run after `wireEvents()` so the
   * Close button can be replaced (with its original listener) instead of having
   * the "hide the pane" handler stacked under the redock one.
   */
  function boot() {
    var t = target();
    if (!t) return false;

    document.documentElement.setAttribute('data-win', 'pane');
    document.documentElement.setAttribute('data-pane', t.pane);

    var el = document.querySelector(SELECTOR[t.pane] || '');
    if (!el) return true;

    var root = document.createElement('div');
    root.id = 'paneRoot';
    root.setAttribute('data-pane', t.pane);
    document.body.appendChild(root);
    root.appendChild(el);
    el.classList.remove('hidden');

    retargetClose(t.pane);
    if (t.pane === 'findPanel') addFindInput();
    if (TITLE[t.pane]) document.title = 'CS Viewer — ' + TITLE[t.pane];
    return true;
  }

  /**
   * The find pane is results-only in the app: the query box lives in the top bar,
   * which a pane window does not have. Give it its own so the detached pane is
   * usable, and keep it in step with the graph window's box.
   */
  function addFindInput() {
    var panel = document.querySelector('#findPanel');
    if (!panel || panel.querySelector('.pane-find-input')) return;
    var wrap = document.createElement('div');
    wrap.className = 'pane-find-input';
    var input = document.createElement('input');
    input.type = 'text';
    input.className = 'text-input';
    input.spellcheck = false;
    input.autocomplete = 'off';
    input.placeholder = 'Find symbol or code…';
    input.value = (typeof state === 'object' && state.find.query) || '';
    var timer = 0;
    input.addEventListener('input', function () {
      window.clearTimeout(timer);
      var q = input.value.trim();
      timer = window.setTimeout(function () {
        if (typeof runFind === 'function') runFind(q);
      }, 250);
    });
    input.addEventListener('keydown', function (ev) {
      if (ev.key === 'Enter') {
        window.clearTimeout(timer);
        if (typeof runFind === 'function') runFind(input.value.trim());
      }
    });
    wrap.appendChild(input);
    var head = panel.querySelector('.drawer-head');
    if (head && head.parentNode) head.parentNode.insertBefore(wrap, head.nextSibling);
    else panel.insertBefore(wrap, panel.firstChild);
    findInputEl = input;
  }

  /** Mirror the graph window's query into this pane's box, unless the user is
   *  typing in it. */
  function syncFindInput(query) {
    if (!findInputEl || document.activeElement === findInputEl) return;
    if (findInputEl.value !== query) findInputEl.value = query || '';
  }

  /** Replace a pane's Close with "redock" (close this window). */
  function retargetClose(pane) {
    var sel = CLOSE_BTN[pane];
    if (!sel) return;
    var btn = document.querySelector(sel);
    if (!btn || !btn.parentNode) return;
    var replacement = btn.cloneNode(true);
    btn.parentNode.replaceChild(replacement, btn);
    replacement.title = 'Redock this pane (close its window)';
    replacement.addEventListener('click', function (ev) {
      ev.preventDefault();
      ev.stopPropagation();
      if (window.CsWindows) window.CsWindows.closePane(pane);
    });
  }

  /**
   * Populate the window and show the pane.
   *
   * The graph model is loaded even though the canvas is hidden: the panes are
   * the app's own code (the filters call `computeView`, the drawer walks
   * `nodeById`, relations expand through the graph model), so a pane window
   * needs the same model the main window has. The engine is shared and
   * in-process, so this is a second view of one graph, not a second parse.
   */
  function start(appState) {
    var t = target();
    if (!t) return Promise.resolve(false);

    var ready = Promise.resolve(null);
    if (appState && appState.indexed && typeof loadNamespaces === 'function') {
      ready = Promise.resolve(loadNamespaces()).catch(function () { return null; });
    }
    return ready.then(function () {
      var applied = applyInitial();
      listen();
      // A pane that was handed the graph window's view state publishes from the
      // start (it has something to say). One started without it — `--open-pane`
      // on the command line — has nothing to publish yet: it listens until
      // something changes here, so it cannot overwrite the main window's view
      // with its own empty one.
      if (window.CSSync) {
        if (applied) window.CSSync.enable();
        else if (window.CSSync.listen) {
          window.CSSync.listen();
          // Nothing was handed over: ask the window that is already running what
          // it is showing.
          window.CsWindows.emit('cs:request', { pane: paneId() });
        }
      }
      return true;
    });
  }

  /** The view state handed over when the pane was detached. */
  function applyInitial() {
    var t = target();
    if (!t || !t.state) return false;
    if (window.CSSync && window.CSSync.apply) {
      window.CSSync.apply(t.state, { initial: true });
      return true;
    }
    return false;
  }

  /**
   * Live updates from the other windows. The view state itself is applied by
   * `CSSync.listen()` (enabled in `start()`); this only mirrors the find query
   * into the pane's own query box.
   */
  function listen() {
    if (listening || !window.CsWindows) return;
    listening = true;
    window.CsWindows.on('cs:sync', function (msg) {
      if (!msg || !msg.view) return;
      if (typeof msg.view.find === 'string') syncFindInput(msg.view.find);
    });
  }

  return {
    active: active,
    target: target,
    pane: paneId,
    boot: boot,
    start: start,
    applyInitial: applyInitial
  };
})();
