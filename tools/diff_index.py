#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""diff_index.py - the one-command clone check for the Rust index + API.

Runs the two producers of the canonical document and then compares them:

  1. `tools/oracle.py` (the frozen v1 Python `Index`)  -> gold.json
  2. `csgraph dump`    (the Rust `csgraph_core::index`) -> rust.json
  3. the same structural walk `tools/diff_clone.py` uses, printing node/edge
     counts and the first differing JSON keys.

The `reorg` section is skipped by default: it is owned by `csgraph_core::reorg`
and `tools/diff_clone.py --skip reorg` is the documented split. Every other key
is still compared, so a mismatch in the API document surfaces here.

    python3 tools/diff_index.py --repo /path/to/vrcCS
    python3 tools/diff_index.py --repo /path/to/vrcCS --limit 40 --keep /tmp/cc
    python3 tools/diff_index.py --repo /path/to/vrcCS --skip docs
"""

import argparse
import json
import os
import sqlite3
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
WORKSPACE = os.path.dirname(HERE)               # rust-only-csviewer/
BUILDER = os.path.dirname(WORKSPACE)            # "cs viewer builder/"
DEFAULT_REPO = os.path.dirname(BUILDER)         # the checkout

# The skip scope this check is allowed to tolerate: the reorganisation engine
# is ported and verified separately (tools/diff_clone.py --skip reorg).
DEFAULT_SKIP = ['reorg']

sys.path.insert(0, HERE)
import diff_clone  # noqa: E402  (the walk we already trust)


def legacy_viewer_state(repo, roots):
    """True when a v1 `.csgraph.db` still holds a viewer_state row.

    The v1 viewer keeps its camera/theme/positions in SQLite; v2 keeps the same
    document in `.csgraph.v2/viewer.json` (see `core/src/cache.rs`). `oracle.py`
    drops the `cache` block for exactly this reason, but it cannot drop
    `state.viewer` — so a working copy that has run the v1 browser once shows a
    viewer-state difference that is not an engine difference. Read-only check.
    """
    for r in roots:
        base = r if os.path.isabs(r) else os.path.join(repo, r)
        if os.path.isfile(base):
            base = os.path.dirname(base)
        path = os.path.join(base, '.csgraph.db')
        if not os.path.isfile(path):
            continue
        try:
            con = sqlite3.connect('file:%s?mode=ro' % path, uri=True)
            try:
                row = con.execute(
                    "SELECT v FROM meta WHERE k='viewer_state'").fetchone()
            finally:
                con.close()
            if row:
                return path
        except sqlite3.Error:
            continue
    return None


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


def run(cmd):
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if proc.returncode != 0:
        sys.stderr.write('diff_index: command failed (%d): %s\n'
                         % (proc.returncode, ' '.join(cmd)))
        sys.stderr.write(proc.stderr.decode('utf-8', 'replace'))
        raise SystemExit(2)
    if proc.stderr:
        sys.stderr.write(proc.stderr.decode('utf-8', 'replace'))
    return proc.stdout


def load(path):
    with open(path, encoding='utf-8') as fh:
        return json.load(fh)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--root', action='append', default=None)
    ap.add_argument('--exclude', action='append', default=None)
    ap.add_argument('--limit', type=int, default=40,
                    help='stop after this many differing keys (default 40)')
    ap.add_argument('--skip', action='append', default=None,
                    help='substring of a JSON path to ignore (repeatable)')
    ap.add_argument('--no-default-skip', action='store_true',
                    help='also compare the reorg section')
    ap.add_argument('--rust-bin', default=None)
    ap.add_argument('--keep', default=None,
                    help='directory to keep gold.json/rust.json in (default: a temp dir)')
    args = ap.parse_args()

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    excludes = args.exclude                      # None = the oracle's default list
    skip = list(args.skip or [])
    if not args.no_default_skip:
        skip = DEFAULT_SKIP + skip
        legacy = legacy_viewer_state(repo, roots)
        if legacy and 'state.viewer' not in skip:
            skip = skip + ['state.viewer']
            print('note: %s still holds a v1 viewer_state; skipping state.viewer\n'
                  '      (v2 stores the same document in .csgraph.v2/viewer.json)'
                  % legacy)

    rust_bin = find_rust_bin(args.rust_bin)
    if rust_bin is None:
        raise SystemExit('diff_index: csgraph binary not found; build it with\n'
                         '  cargo build --offline -p csgraph-core\n'
                         'or pass --rust-bin PATH')

    out_dir = args.keep or tempfile.mkdtemp(prefix='csgraph-clonecheck-')
    os.makedirs(out_dir, exist_ok=True)
    gold = os.path.join(out_dir, 'gold.json')
    rust = os.path.join(out_dir, 'rust.json')

    py_cmd = [sys.executable, os.path.join(HERE, 'oracle.py'),
              '--repo', repo, '-o', gold]
    for r in roots:
        py_cmd += ['--root', r]
    for e in (excludes or []):
        py_cmd += ['--exclude', e]

    rs_cmd = [rust_bin, 'dump', '--repo', repo, '-o', rust]
    for r in roots:
        rs_cmd += ['--root', r]
    for e in (excludes or []):
        rs_cmd += ['--exclude', e]

    print('python: %s' % ' '.join(py_cmd))
    print('rust:   %s' % ' '.join(rs_cmd))
    run(py_cmd)
    run(rs_cmd)

    a = load(gold)
    b = load(rust)
    for key in ('nodes', 'edges'):
        print('%s: python=%d rust=%d'
              % (key, len(a.get(key) or []), len(b.get(key) or [])))
    if sorted(a) != sorted(b):
        print('top-level keys differ: python=%s rust=%s' % (sorted(a), sorted(b)))

    out = []
    diff_clone.diff(a, b, '$', out, args.limit, skip)
    if not out:
        print('\nCLONE OK: no differences outside the skipped scope%s'
              % (' (skipped: %s)' % ', '.join(skip) if skip else ''))
        return 0

    print('\n%d differing key(s) (showing up to %d; skipped: %s):'
          % (len(out), args.limit, ', '.join(skip) or 'none'))
    for kind, path, l, r in out:
        print('  [%s] %s' % (kind, path))
        if kind == 'length':
            print('      python len=%s rust len=%s' % (l, r))
        else:
            print('      python: %s' % diff_clone.fmt(l))
            print('      rust  : %s' % diff_clone.fmt(r))
    print('\ndumps kept in %s' % out_dir)
    return 1


if __name__ == '__main__':
    sys.exit(main())
