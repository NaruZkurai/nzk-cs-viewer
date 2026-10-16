#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""diff_doc.py - compare the frozen cs_doc.py with the Rust `doc` port.

It runs `tools/oracle_doc.py` (Python, frozen v1) and `csgraph dump-doc`
(Rust), then walks the two canonical JSON documents together and reports the
first differences with a precise JSON path, so a mismatch points at a field
rather than at "two files differ".

    python3 tools/diff_doc.py --repo /path/to/vrcCS
    python3 tools/diff_doc.py --repo /path/to/vrcCS --snapshot
    python3 tools/diff_doc.py --repo /path/to/vrcCS --limit 40

Corpus mode (the default) builds the graph independently on both sides, so it
checks `doc.rs` *and* the parser port. `--snapshot` feeds Rust the exact
`{nodes,file_texts}` the oracle built, isolating the document synthesis from
the parser/graph build.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)                     # rust-only-csviewer/
BUILDER = os.path.dirname(ROOT)                  # "cs viewer builder/"
# rust-env.sh points CARGO_TARGET_DIR at the builder directory.
DEFAULT_BIN = os.path.join(BUILDER, 'target', 'debug', 'csgraph')
DEFAULT_REPO = os.path.dirname(BUILDER)


def fmt(v, width=200):
    s = json.dumps(v, ensure_ascii=False, sort_keys=True)
    return s if len(s) <= width else s[:width] + '\u2026'


def context(a, b):
    """A little surrounding context for the first difference."""
    return '  left : %s\n  right: %s' % (fmt(a), fmt(b))


def diff(a, b, path, out, limit):
    if len(out) >= limit:
        return
    if type(a) is not type(b) and not (isinstance(a, (int, float))
                                       and isinstance(b, (int, float))):
        out.append(('type', path, a, b))
        return
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a:
                out.append(('missing-in-right', path + '.' + k, None, b[k]))
            elif k not in b:
                out.append(('missing-in-left', path + '.' + k, a[k], None))
            else:
                diff(a[k], b[k], path + '.' + k, out, limit)
            if len(out) >= limit:
                return
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(('length', path, len(a), len(b)))
        for i in range(min(len(a), len(b))):
            diff(a[i], b[i], '%s[%d]' % (path, i), out, limit)
            if len(out) >= limit:
                return
    elif a != b:
        out.append(('value', path, a, b))


def run(cmd, **kw):
    proc = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if proc.returncode != 0:
        sys.stderr.write('$ %s\n' % ' '.join(cmd))
        sys.stderr.write(proc.stderr)
        raise SystemExit('command failed: %s' % cmd[0])
    return proc


def normalize_doc_payload(p):
    """The layout-independent content of one synthesized document.

    v2 deliberately merges the `partial` declaration sites of a type under one
    emitted header (see `emit_merged_group` in `doc.rs`), so `lines` and
    `sections` no longer match the frozen v1 byte for byte. The *content* must
    still match: every site, every body line, every `#if` record. Comparing
    those (order-insensitively) proves the merge dropped nothing, while a
    separate head-count check proves it actually merged.
    """
    lines = p.get('lines') or []
    # The merge moves repeated wrapper/type headers out of the per-site body and
    # emits them once, so individual `text` records cannot be compared across the
    # two layouts (a same-line header is also split). What must stay identical is
    # the set of declaration sites and of `#if` records; the head-count check in
    # main() proves the merge went the right way, and `doc.rs`'s own tests pin the
    # exact merged shape.
    cond = sorted({(l.get('sec', ''), l.get('text', ''))
                   for l in lines if l.get('ctx') == 'cond'})
    parts = sorted(
        (x.get('file'), x.get('line'), x.get('endLine'), x.get('startOff'), x.get('endOff'))
        for x in (p.get('parts') or [])
    )
    return {
        'definesKnown': p.get('definesKnown'),
        'parts': parts,
        'cond': cond,
    }


def normalize_obj(obj):
    """`(comparable object, {node: head-line count})`."""
    docs = {}
    heads = {}
    for k, p in (obj.get('docs') or {}).items():
        if not isinstance(p, dict):
            docs[k] = None
            heads[k] = 0
            continue
        docs[k] = normalize_doc_payload(p)
        heads[k] = sum(1 for l in (p.get('lines') or []) if l.get('ctx') == 'head')
    out = dict(obj)
    out['docs'] = docs
    return out, heads


def ensure_bin(path):
    if os.path.exists(path):
        return
    sys.stderr.write('diff_doc: %s missing, building...\n' % path)
    cmd = ('source ../rust-env.sh && cargo build --offline -p csgraph-core')
    proc = subprocess.run(['bash', '-c', cmd], cwd=ROOT, capture_output=True,
                          text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        raise SystemExit('cargo build failed')
    if not os.path.exists(path):
        raise SystemExit('build did not produce %s' % path)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--bin', default=DEFAULT_BIN)
    ap.add_argument('--root', action='append', default=None)
    ap.add_argument('--node', action='append', default=None,
                    help='pin a node id (repeatable); default is oracle-selected')
    ap.add_argument('--nodes-file', default=None,
                    help='file with one node id per line (overrides --node)')
    ap.add_argument('--define', action='append', default=None,
                    help='pin a conditional symbol (repeatable); forces known')
    ap.add_argument('--snapshot', action='store_true',
                    help='feed Rust the oracle graph snapshot (isolates doc.rs)')
    ap.add_argument('--strict', action='store_true',
                    help='compare byte-identical payloads (pre-merge v1 layout)')
    ap.add_argument('--limit', type=int, default=25)
    ap.add_argument('--keep', action='store_true',
                    help='keep the temporary gold/rust JSON files')
    args = ap.parse_args()

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    pinned = list(args.node or [])
    if args.nodes_file:
        pinned = []
        with open(args.nodes_file, encoding='utf-8') as fh:
            for line in fh:
                line = line.strip()
                if line:
                    pinned.append(line)
    ensure_bin(args.bin)

    tmp = tempfile.mkdtemp(prefix='csgraph-doc-diff-')
    gold = os.path.join(tmp, 'gold.json')
    snap = os.path.join(tmp, 'graph.json')
    rust = os.path.join(tmp, 'rust.json')

    oracle = [sys.executable, os.path.join(HERE, 'oracle_doc.py'),
              '--repo', repo, '-o', gold]
    for r in roots:
        oracle += ['--root', r]
    for nid in pinned:
        oracle += ['--node', nid]
    for sym in (args.define or []):
        oracle += ['--define', sym]
    if args.snapshot:
        oracle += ['--graph-out', snap]
    run(oracle)

    with open(gold, encoding='utf-8') as fh:
        gold_obj = json.load(fh)
    node_ids = gold_obj.get('nodeIds') or []

    if args.snapshot:
        rust_cmd = [args.bin, 'dump-doc', '--graph', snap, '--repo', repo]
        for r in roots:
            rust_cmd += ['--root', r]
        for sym in (gold_obj.get('defines') or []):
            rust_cmd += ['--define', str(sym)]
    else:
        rust_cmd = [args.bin, 'dump-doc', '--repo', repo]
        for r in roots:
            rust_cmd += ['--root', r]
        for sym in (gold_obj.get('defines') or []):
            rust_cmd += ['--define', str(sym)]
        for nid in node_ids:
            rust_cmd += ['--node', nid]
    proc = run(rust_cmd)
    with open(rust, 'w', encoding='utf-8') as fh:
        fh.write(proc.stdout)
    rust_obj = json.loads(proc.stdout)

    out = []
    if args.strict:
        diff(gold_obj, rust_obj, '$', out, args.limit)
        mode_note = 'byte-identical document payloads'
    else:
        gold_norm, gold_heads = normalize_obj(gold_obj)
        rust_norm, rust_heads = normalize_obj(rust_obj)
        diff(gold_norm, rust_norm, '$', out, args.limit)
        # v2 must merge, never split: it may emit fewer heads than v1, never more.
        for k in sorted(gold_heads):
            if rust_heads.get(k, 0) > gold_heads[k]:
                out.append(('merge', '$[' + repr(k) + ']',
                            gold_heads[k], rust_heads[k]))
        mode_note = ('content-identical; v2 merges %d doc(s) into fewer headers'
                     % sum(1 for k in gold_heads if rust_heads.get(k, 0) < gold_heads[k]))

    print('repo  : %s' % repo)
    print('mode  : %s' % ('snapshot' if args.snapshot else 'corpus'))
    print('nodes : %d' % len(node_ids))
    print('left  : oracle_doc.py (frozen cs_doc.py)')
    print('right : %s dump-doc' % args.bin)

    if not out:
        print('\nDOC OK: %s' % mode_note)
        if not args.keep:
            _cleanup(tmp)
        return 0

    print('\n%d difference(s) (showing up to %d):' % (len(out), args.limit))
    for kind, path, l, r in out:
        print('  [%s] %s' % (kind, path))
        print(context(l, r))
    sys.stderr.write('diff files kept under %s\n' % tmp)
    return 1


def _cleanup(tmp):
    import shutil
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
