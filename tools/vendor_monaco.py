#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""vendor_monaco.py - prove the vendored Monaco tree is complete and untouched.

`vendor/monaco/` holds the whole published Monaco source distribution
(esm/ + dev/ + min/ + monaco.d.ts + licenses) so the viewer can run the real
VSCode editor with no package manager involved at build or run time.

The tree is immutable by contract. This script is the contract's check: it
recomputes a deterministic hash over every file and compares it with the values
recorded in VENDOR.json at vendoring time.

    python3 tools/vendor_monaco.py --verify     # exit 1 on any drift
    python3 tools/vendor_monaco.py --print      # recompute and show
    python3 tools/vendor_monaco.py --manifest   # write MANIFEST.sha256

stdlib only.
"""

import argparse
import hashlib
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)                     # the rust-only-csviewer tree
VENDOR = os.path.join(ROOT, 'vendor', 'monaco')  # the immutable clone
META = os.path.join(VENDOR, 'VENDOR.json')
MANIFEST = os.path.join(VENDOR, 'MANIFEST.sha256')

# Our own provenance files live in the tree but are not part of Monaco; the
# recorded file count (1918) and byte count come from npm's own metadata, so
# they must not include them.
OUR_FILES = {'VENDOR.json', 'MANIFEST.sha256'}


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, 'rb') as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()


def walk(root):
    """Every upstream file in deterministic sorted-DFS order (dirs then files,
    each sorted by name), with POSIX separators in the relative path. This is
    the order the recorded `tree_sha256` was taken in; do not re-sort by full
    path string, which orders `a/b` differently from `a.txt`."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for fn in sorted(filenames):
            if fn in OUR_FILES:
                continue
            p = os.path.join(dirpath, fn)
            rel = os.path.relpath(p, root).replace(os.sep, '/')
            out.append((rel, p))
    return out


def tree_hash(root):
    """The recorded scheme: sha256 over sorted `rel\\0size\\0sha256(content)\\n`."""
    h = hashlib.sha256()
    files = walk(root)
    total = 0
    for rel, path in files:
        size = os.path.getsize(path)
        total += size
        h.update(rel.encode('utf-8') + b'\0')
        h.update(str(size).encode('ascii') + b'\0')
        h.update(bytes.fromhex(sha256_file(path)))
        h.update(b'\n')
    return h.hexdigest(), len(files), total


def load_meta():
    if not os.path.isfile(META):
        return None, 'missing %s' % META
    try:
        with open(META, encoding='utf-8') as fh:
            return json.load(fh), None
    except (OSError, ValueError) as e:
        return None, 'unreadable %s: %s' % (META, e)


def cmd_verify():
    meta, err = load_meta()
    if err:
        print('FAIL: %s' % err, file=sys.stderr)
        return 1
    want = meta.get('integrity_self') or {}
    got_hash, got_files, got_bytes = tree_hash(VENDOR)
    ok = True
    checks = [
        ('files', want.get('files'), got_files),
        ('bytes', want.get('bytes'), got_bytes),
        ('tree_sha256', want.get('tree_sha256'), got_hash),
    ]
    for label, exp, act in checks:
        mark = 'ok  ' if exp == act else 'DIFF'
        if exp != act:
            ok = False
        print('%s %-12s expected=%s actual=%s' % (mark, label, exp, act))
    if not ok:
        print('FAIL: vendored Monaco has drifted from VENDOR.json', file=sys.stderr)
        return 1
    entry = os.path.join(VENDOR, (meta.get('runtime_entry') or '').replace('/', os.sep))
    if not os.path.isfile(entry):
        print('FAIL: runtime entry missing: %s' % entry, file=sys.stderr)
        return 1
    print('PASS: monaco-editor %s intact (%d files, %d bytes)'
          % (meta.get('version'), got_files, got_bytes))
    return 0


def cmd_print():
    h, files, total = tree_hash(VENDOR)
    print('files=%d bytes=%d tree_sha256=%s' % (files, total, h))
    return 0


def cmd_manifest():
    h, files, total = tree_hash(VENDOR)
    with open(MANIFEST, 'w', encoding='utf-8') as out:
        for rel, path in walk(VENDOR):
            out.write('%s  %s\n' % (sha256_file(path), rel))
    print('wrote %s (%d entries, tree %s)' % (MANIFEST, files, h))
    return 0


def cmd_stage(dest=None):
    """Copy only the self-contained runtime (min/vs + notices) into the frontend.

    The full clone stays in vendor/; the app ships the min AMD build. Staging is
    a copy, never a transformation, so the staged tree is byte-identical to the
    clone's min/ and can be re-derived at any time.
    """
    import shutil
    if dest is None:
        dest = os.path.join(ROOT, 'frontend', 'vendor', 'monaco')
    src_min = os.path.join(VENDOR, 'min')
    if not os.path.isdir(src_min):
        print('FAIL: %s missing; the clone is incomplete' % src_min, file=sys.stderr)
        return 1
    if os.path.isdir(dest):
        shutil.rmtree(dest)
    os.makedirs(dest, exist_ok=True)
    shutil.copytree(src_min, os.path.join(dest, 'min'))
    for name in ('LICENSE', 'ThirdPartyNotices.txt', 'package.json', 'VENDOR.json'):
        src = os.path.join(VENDOR, name)
        if os.path.isfile(src):
            shutil.copy2(src, os.path.join(dest, name))
    h, files, total = tree_hash(os.path.join(dest, 'min'))
    hs, filess, totals = tree_hash(src_min)
    if (h, files, total) != (hs, filess, totals):
        print('FAIL: staged min/ does not match the clone', file=sys.stderr)
        return 1
    print('staged %s (min: %d files, %d bytes, tree %s)' % (dest, files, total, h))
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    g = ap.add_mutually_exclusive_group()
    g.add_argument('--verify', action='store_true', help='recompute and compare (default)')
    g.add_argument('--print', action='store_true', help='recompute and print the hash')
    g.add_argument('--manifest', action='store_true', help='write a per-file sha256 manifest')
    g.add_argument('--stage', nargs='?', const='', default=None, metavar='DIR',
                   help='copy min/vs into the frontend (default frontend/vendor/monaco)')
    args = ap.parse_args()
    if args.print:
        return cmd_print()
    if args.manifest:
        return cmd_manifest()
    if args.stage is not None:
        return cmd_stage(args.stage or None)
    return cmd_verify()


if __name__ == '__main__':
    sys.exit(main())
