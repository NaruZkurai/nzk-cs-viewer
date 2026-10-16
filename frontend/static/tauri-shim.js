/*
 * CS Viewer — native transport shim.
 *
 * The packaged app has no HTTP server. This shim is loaded before app.js and
 * rewrites same-origin /api/* fetches into Tauri IPC calls handled by the Rust
 * host, which talks to the csgraph bridge over pipes. Everything else (and any
 * non-Tauri context, e.g. the plain web server) falls through to real fetch.
 *
 * app.js is unmodified: it builds same-origin URLs and consumes Response
 * objects, so a synthetic Response is all it needs.
 */
(function () {
  'use strict';

  var tauri = window.__TAURI__;
  if (!tauri || !tauri.core || typeof tauri.core.invoke !== 'function') {
    return;                       // web/server build: leave fetch alone
  }

  var invoke = tauri.core.invoke;
  var realFetch = window.fetch ? window.fetch.bind(window) : null;

  function passthrough(input, init, err) {
    if (realFetch) return realFetch(input, init);
    return Promise.reject(err || new Error('fetch is unavailable'));
  }

  window.fetch = function (input, init) {
    var url;
    try {
      var raw = (typeof input === 'string') ? input : ((input && input.url) || '');
      url = new URL(raw, window.location.origin);
    } catch (e) {
      return passthrough(input, init, e);
    }
    if (url.origin !== window.location.origin || url.pathname.indexOf('/api/') !== 0) {
      return passthrough(input, init);
    }

    var opts = init || {};
    if (!opts.method && input && input.method) opts.method = input.method;
    var method = String(opts.method || 'GET').toUpperCase();

    var query = {};
    url.searchParams.forEach(function (value, key) { query[key] = value; });

    var body = null;
    if (typeof opts.body === 'string' && opts.body.length) {
      try { body = JSON.parse(opts.body); } catch (e) { body = null; }
    }

    return invoke('api', {
      method: method,
      path: url.pathname,
      query: query,
      body: body
    }).then(function (reply) {
      var payload = (reply && typeof reply === 'object' && 'body' in reply) ? reply.body : reply;
      var status = (reply && reply.status) || 200;
      return new Response(JSON.stringify(payload), {
        status: status,
        statusText: status === 200 ? 'OK' : 'Error',
        headers: { 'Content-Type': 'application/json; charset=utf-8' }
      });
    });
  };

  window.__CSVIEWER_NATIVE__ = true;
})();
