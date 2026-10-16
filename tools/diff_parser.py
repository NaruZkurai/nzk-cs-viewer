#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""diff_parser.py - differential check of the Rust cs_parser port.

Runs two producers of the same canonical document:

  * `tools/oracle_parser.py`, which executes the frozen Python `cs_parser.py`
  * `csgraph dump-parser`, the Rust port

parses both JSON documents and reports the first N structural differences with
a path to the offending key (`nodes[41].label`, `files[3].members[7].refs[0].qual`),
plus node/edge/file counts from each side.

    python3 tools/diff_parser.py --repo /path/to/vrcCS --root _sealed_src

Exit status is 0 when the documents are equal and 1 otherwise.
"""

import argparse
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
WORKSPACE = os.path.dirname(HERE)
BUILDER = os.path.dirname(WORKSPACE)
DEFAULT_REPO = os.path.dirname(BUILDER)


def find_rust_bin(explicit=None):
    if explicit:
        return explicit
    candidates = [
        os.path.join(BUILDER, 'target', 'debug', 'csgraph'),
        os.path.join(WORKSPACE, 'target', 'debug', 'csgraph'),
        os.path.join(BUILDER, 'target', 'release', 'csgraph'),
    ]
    for c in candidates:
        if os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    return None


def run(cmd, **kw):
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kw)
    if proc.returncode != 0:
        sys.stderr.write('command failed (%d): %s\n' % (proc.returncode, ' '.join(cmd)))
        sys.stderr.write(proc.stderr.decode('utf-8', 'replace'))
        raise SystemExit(2)
    return proc.stdout


def load_json(raw, what):
    try:
        return json.loads(raw.decode('utf-8'))
    except Exception as exc:                       # noqa: BLE001
        raise SystemExit('diff_parser: %s produced invalid JSON: %s' % (what, exc))


def walk(a, b, path, out, limit):
    """Collect up to `limit` path-annotated structural differences."""
    if len(out) >= limit:
        return
    if isinstance(a, bool) != isinstance(b, bool) or type(a) is not type(b):
        out.append((path, a, b))
        return
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if len(out) >= limit:
                return
            sub = ('%s.%s' % (path, k)) if path else k
            if k not in a:
                out.append((sub, '<missing>', b[k]))
            elif k not in b:
                out.append((sub, a[k], '<missing>'))
            else:
                walk(a[k], b[k], sub, out, limit)
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(('%s.__len__' % path, len(a), len(b)))
        for i, (x, y) in enumerate(zip(a, b)):
            if len(out) >= limit:
                return
            walk(x, y, '%s[%d]' % (path, i), out, limit)
    else:
        if a != b:
            out.append((path, a, b))


def _short(v):
    try:
        s = json.dumps(v, ensure_ascii=False, sort_keys=True)
    except Exception:                              # noqa: BLE001
        s = repr(v)
    if len(s) > 160:
        s = s[:157] + '...'
    return s


def counts(doc):
    return {
        'files': len(doc.get('files', [])),
        'nodes': len(doc.get('nodes', [])),
        'edges': len(doc.get('edges', [])),
        'graphWarnings': len(doc.get('graphWarnings', [])),
    }


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--root', action='append', default=None)
    ap.add_argument('--exclude', action='append', default=None)
    ap.add_argument('--limit', type=int, default=50,
                    help='stop after this many differences (default 50)')
    ap.add_argument('--rust-bin', default=None)
    ap.add_argument('--dump-dir', default=None,
                    help='keep both JSON documents in this directory for inspection')
    args = ap.parse_args(argv)

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    excludes = args.exclude if args.exclude is not None else []

    py_cmd = [sys.executable, os.path.join(HERE, 'oracle_parser.py'), '--repo', repo]
    for r in roots:
        py_cmd += ['--root', r]
    for e in excludes:
        py_cmd += ['--exclude', e]

    rust_bin = find_rust_bin(args.rust_bin)
    if rust_bin is None:
        raise SystemExit('diff_parser: csgraph binary not found; build it with\n'
                         '  cargo build --offline -p csgraph-core\n'
                         'or pass --rust-bin PATH')
    rs_cmd = [rust_bin, 'dump-parser', '--repo', repo]
    for r in roots:
        rs_cmd += ['--root', r]
    for e in excludes:
        rs_cmd += ['--exclude', e]

    print('python: %s' % ' '.join(py_cmd))
    print('rust:   %s' % ' '.join(rs_cmd))
    py_raw = run(py_cmd)
    rs_raw = run(rs_cmd)

    if args.dump_dir:
        os.makedirs(args.dump_dir, exist_ok=True)
        with open(os.path.join(args.dump_dir, 'oracle_parser.json'), 'wb') as fh:
            fh.write(py_raw)
        with open(os.path.join(args.dump_dir, 'rust_parser.json'), 'wb') as fh:
            fh.write(rs_raw)

    py_doc = load_json(py_raw, 'oracle_parser.py')
    rs_doc = load_json(rs_raw, 'csgraph dump-parser')

    py_counts = counts(py_doc)
    rs_counts = counts(rs_doc)
    print('counts python: %s' % py_counts)
    print('counts rust:   %s' % rs_counts)

    diffs = []
    walk(py_doc, rs_doc, '', diffs, args.limit)

    if not diffs:
        print('OK: 0 differences')
        return 0

    print('FAIL: %d difference(s) (limit %d)%s'
          % (len(diffs), args.limit, ' [truncated]' if len(diffs) >= args.limit else ''))
    for path, a, b in diffs:
        print('  %s\n    python: %s\n    rust:   %s' % (path, _short(a), _short(b)))
    return 1


if __name__ == '__main__':
    sys.exit(main())
