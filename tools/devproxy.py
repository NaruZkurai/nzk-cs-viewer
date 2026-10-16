#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""devproxy.py - serve the v2 frontend and proxy /api/* to a running v1 server.

Development and verification only: the shipped v2 talks to the Rust engine
in-process, exactly like the partial-Rust build talks to bridge.py. But while
the Rust engine is being ported, this lets the v2 UI be driven end-to-end
against the v1 Python API without editing a single v1 file.

    python3 tools/devproxy.py --port 8799 --upstream 127.0.0.1:8790

GET/HEAD of anything outside /api/ is served from ../frontend/; /api/* is
forwarded verbatim (method, body, headers) to the upstream csgraph server.
"""

import argparse
import http.client
import http.server
import mimetypes
import os
import sys
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FRONTEND = os.path.join(ROOT, 'frontend')

UPSTREAM = ('127.0.0.1', 8790)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    server_version = 'csviewer-v2-devproxy'

    def log_message(self, fmt, *args):   # keep the console readable
        sys.stderr.write('[devproxy] %s\n' % (fmt % args))

    # ------------------------------------------------------------------ proxy
    def _proxy(self):
        length = int(self.headers.get('Content-Length') or 0)
        body = self.rfile.read(length) if length else None
        conn = http.client.HTTPConnection(UPSTREAM[0], UPSTREAM[1], timeout=900)
        headers = {k: v for k, v in self.headers.items()
                   if k.lower() not in ('host', 'connection')}
        try:
            conn.request(self.command, self.path, body=body, headers=headers)
            resp = conn.getresponse()
            data = resp.read()
        except OSError as e:
            self.send_error(502, 'upstream %s:%d unreachable: %s' % (UPSTREAM[0], UPSTREAM[1], e))
            return
        self.send_response(resp.status)
        for k, v in resp.getheaders():
            if k.lower() in ('transfer-encoding', 'connection', 'content-encoding'):
                continue
            self.send_header(k, v)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        if self.command != 'HEAD':
            self.wfile.write(data)

    # ------------------------------------------------------------------- file
    def _file(self):
        path = urllib.parse.urlparse(self.path).path
        if path in ('', '/'):
            path = '/index.html'
        rel = os.path.normpath(urllib.parse.unquote(path).lstrip('/'))
        if rel.startswith('..') or os.path.isabs(rel):
            self.send_error(403, 'forbidden')
            return
        fp = os.path.join(FRONTEND, rel)
        if not os.path.isfile(fp):
            self.send_error(404, 'not found: %s' % rel)
            return
        try:
            with open(fp, 'rb') as fh:
                data = fh.read()
        except OSError as e:
            self.send_error(500, str(e))
            return
        ctype = mimetypes.guess_type(fp)[0] or 'application/octet-stream'
        if ctype.startswith('text/') or ctype in ('application/javascript', 'application/json'):
            ctype += '; charset=utf-8'
        self.send_response(200)
        self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        if self.command != 'HEAD':
            self.wfile.write(data)

    def _dispatch(self):
        path = urllib.parse.urlparse(self.path).path
        if path == '/api' or path.startswith('/api/'):
            self._proxy()
        else:
            self._file()

    do_GET = _dispatch
    do_HEAD = _dispatch
    do_POST = _dispatch


def main():
    global UPSTREAM
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--port', type=int, default=8799)
    ap.add_argument('--host', default='127.0.0.1')
    ap.add_argument('--upstream', default='127.0.0.1:8790',
                    help='host:port of the v1 csgraph server (default 8790)')
    args = ap.parse_args()
    host, _, port = args.upstream.partition(':')
    UPSTREAM = (host, int(port or 8765))
    if not os.path.isfile(os.path.join(FRONTEND, 'index.html')):
        sys.exit('err: %s/index.html missing' % FRONTEND)
    httpd = http.server.ThreadingHTTPServer((args.host, args.port), Handler)
    sys.stderr.write('[devproxy] http://%s:%d/ -> frontend=%s upstream=%s:%d\n'
                     % (args.host, args.port, FRONTEND, UPSTREAM[0], UPSTREAM[1]))
    httpd.serve_forever()


if __name__ == '__main__':
    main()
