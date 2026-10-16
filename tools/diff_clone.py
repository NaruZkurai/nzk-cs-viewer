#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""diff_clone.py - compare the v1 oracle dump with the Rust engine's dump.

Both sides write the same canonical JSON (see oracle.py). This walks them
together and reports the first N differences with a precise JSON path, so a
mismatch points at a field rather than at "43 MB of files differ".

The `docs` and `reorg` sections of that dump are deliberately layout-different:
v2 merges the repeated `partial` headers of a type into one, so synthesized
head/tail lines, line counts, the synthesized-file hash and the greedy packing
the splitter plans against legitimately move (see `doc.rs`, `reorg.rs` and the
viewer README's "Known, deliberate deltas"). Those two subtrees are compared,
with the merge-aware allowance, by `tools/diff_doc.py` and
`tools/diff_reorg.py` (the `document clone` and five `reorg clone` units), so
the default here skips them and compares every other section exactly. Pass
`--strict` to compare the whole document byte-for-byte.

    python3 tools/diff_clone.py /tmp/gold.json /tmp/rust.json
    python3 tools/diff_clone.py /tmp/gold.json /tmp/rust.json --strict
    python3 tools/diff_clone.py /tmp/gold.json /tmp/rust.json --limit 40 --skip nodes
"""

import argparse
import json
import sys


def fmt(v, width=160):
    s = json.dumps(v, ensure_ascii=False, sort_keys=True)
    return s if len(s) <= width else s[:width] + '…'


def diff(a, b, path, out, limit, skip):
    if len(out) >= limit:
        return
    if any(seg in path for seg in skip):
        return
    if type(a) is not type(b) and not (isinstance(a, (int, float)) and isinstance(b, (int, float))):
        out.append(('type', path, a, b))
        return
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a:
                out.append(('missing-in-left', path + '.' + k, None, b[k]))
            elif k not in b:
                out.append(('missing-in-right', path + '.' + k, a[k], None))
            else:
                diff(a[k], b[k], path + '.' + k, out, limit, skip)
            if len(out) >= limit:
                return
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(('length', path, len(a), len(b)))
        for i in range(min(len(a), len(b))):
            diff(a[i], b[i], '%s[%d]' % (path, i), out, limit, skip)
            if len(out) >= limit:
                return
    elif a != b:
        out.append(('value', path, a, b))


def summarize(a, b):
    """Cheap shape summary, so a diff can't hide behind an early --limit."""
    out = {}

    def count(node, skip):
        if any(s in skip for s in ()):
            pass
        return node

    for key in ('nodes', 'edges'):
        la = a.get(key) or []
        lb = b.get(key) or []
        out[key] = {'left': len(la), 'right': len(lb)}
    out['topKeys'] = {'left': sorted(a), 'right': sorted(b)}
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('left', help='the oracle dump (v1 Python)')
    ap.add_argument('right', help='the Rust dump')
    ap.add_argument('--limit', type=int, default=25)
    ap.add_argument('--strict', action='store_true',
                    help='also compare the deliberately layout-different `docs` '
                         'and `reorg` sections (pre-merge v1 layout)')
    ap.add_argument('--skip', action='append', default=[],
                    help='substring of a JSON path to ignore (repeatable)')
    args = ap.parse_args()

    with open(args.left, encoding='utf-8') as fh:
        a = json.load(fh)
    with open(args.right, encoding='utf-8') as fh:
        b = json.load(fh)

    scope_note = None
    if not args.strict:
        for key in ('docs', 'reorg'):
            a.pop(key, None)
            b.pop(key, None)
        scope_note = ('scope: every section except `docs` and `reorg`; both are '
                      'deliberately layout-different (v2 merges partial headers) and '
                      'are compared with the merge-aware allowance by tools/diff_doc.py '
                      'and tools/diff_reorg.py — pass --strict to compare them here')

    out = []
    diff(a, b, '$', out, args.limit, args.skip)

    print('left : %s' % args.left)
    print('right: %s' % args.right)
    if scope_note:
        print(scope_note)
    summary = summarize(a, b)
    print('nodes: left=%d right=%d' % (summary['nodes']['left'], summary['nodes']['right']))
    print('edges: left=%d right=%d' % (summary['edges']['left'], summary['edges']['right']))
    if summary['topKeys']['left'] != summary['topKeys']['right']:
        print('top-level keys differ: left=%s right=%s'
              % (summary['topKeys']['left'], summary['topKeys']['right']))

    if not out:
        print('\nCLONE OK: no differences in the compared scope')
        return 0

    print('\n%d difference(s) (showing up to %d):' % (len(out), args.limit))
    for kind, path, l, r in out:
        print('  [%s] %s' % (kind, path))
        if kind in ('missing-in-left', 'missing-in-right'):
            print('      left : %s' % fmt(l))
            print('      right: %s' % fmt(r))
        elif kind == 'length':
            print('      left len=%s right len=%s' % (l, r))
        else:
            print('      left : %s' % fmt(l))
            print('      right: %s' % fmt(r))
    return 1


if __name__ == '__main__':
    sys.exit(main())
