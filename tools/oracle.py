#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""oracle.py - dump everything the v1 engine knows, canonically.

This is the cloning contract for the Rust v2 engine. It imports the frozen v1
modules *by path* (they are never edited) and emits one deterministic JSON
document covering every query the API can answer. `csgraph dump` in the Rust
core must produce the same document; `tools/diff_clone.py` diffs them.

Canonicalisation, so the comparison is about behaviour and not incidental
ordering or machine paths:

  * every absolute path under the repo becomes repo-relative
  * nodes are sorted by id, edges by (source, target, kind), everything else by
    its natural key
  * timing fields (`parseMs`) and cache state (`fromCache`) are dropped
  * floats are not used anywhere in the payload

    python3 tools/oracle.py --repo /path/to/vrcCS --root _sealed_src -o gold.json
"""

import argparse
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)                 # rust-only-csviewer/
DEFAULT_REPO = os.path.dirname(os.path.dirname(ROOT))

# Queries chosen to exercise each resolver branch: a fully-qualified method, a
# bare identifier, an ambiguous simple name, a string literal, a type.
FIND_QUERIES = [
    'NZK.Core.MA.NormaliseName',
    'NormaliseName',
    'SetDirty',
    'errorcode text',
    'BaseMeshName',
]

# Nodes whose synthesized document is compared: the whole-corpus namespace, a
# big type, a member, and a small type.
DOC_NODES = ['#ns:NZK', 'NZK.Core.MA', 'NZK.Core.MA.NormaliseName', 'NZK.AD']

REORG_SPECS = [
    {'group': 'single'},
    {'group': 'size', 'size': 4000},
    {'granularity': 'leaf', 'group': 'size', 'size': 3000},
    {'group': 'namespace'},
    {'group': 'owner'},
]

_DROP_NODE_KEYS = {'_bstart', '_bend', '_dstart', '_dend'}


def _rel(value, repo):
    if not isinstance(value, str) or not value:
        return value
    if value.startswith(repo):
        rel = value[len(repo):].lstrip('/')
        return rel or '.'
    return value


def _norm(obj, repo):
    """Recursively rewrite absolute repo paths to relative ones."""
    if isinstance(obj, dict):
        return {k: _norm(v, repo) for k, v in obj.items()}
    if isinstance(obj, list):
        return [_norm(v, repo) for v in obj]
    if isinstance(obj, str):
        return _rel(obj, repo)
    return obj


def _clean_node(n):
    out = {}
    for k, v in n.items():
        if k in _DROP_NODE_KEYS:
            continue
        out[k] = v
    return out


def build(repo, roots, excludes, cfg):
    """Build a v1 Index exactly the way server.py's /api/index does."""
    sys.path.insert(0, os.path.join(repo, 'tools', 'csgraph'))
    import server as S                      # noqa: E402  (v1, read-only)

    index = S.Index(repo)
    index.build(roots, excludes, cfg)
    return index, S


def dump(repo, roots, excludes, cfg):
    index, S = build(repo, roots, excludes, cfg)

    state = dict(index.state())
    # Non-deterministic or deliberately-relocated fields, dropped for the same
    # reason `parseMs` is: they are not engine answers.
    #   parseMs/fromCache/cache - timing and cache position
    #   viewer                  - the UI's saved theme/camera/positions. v1
    #                             keeps them in the corpus' SQLite file; v2
    #                             keeps them in its own store (cache.rs), so a
    #                             leftover v1 row would show up here as a
    #                             difference in *storage*, not in behaviour.
    state.pop('parseMs', None)
    state.pop('fromCache', None)
    state.pop('cache', None)
    state.pop('viewer', None)
    state = _norm(state, repo)

    nodes = [_clean_node(index.nodes[nid]) for nid in sorted(index.nodes)]
    nodes = _norm(nodes, repo)

    edges = sorted(index.edges, key=lambda e: (e['source'], e['target'], e['kind']))
    edges = _norm(edges, repo)

    out = {'repo': os.path.basename(repo.rstrip('/')), 'roots': list(roots),
           'state': state, 'nodes': nodes, 'edges': edges}

    # ---- per-node detail + relation buckets for a determinable sample --------
    detail = {}
    for nid in sorted(index.nodes):
        n = index.nodes[nid]
        if n['kind'] in ('string', 'external'):
            continue
        try:
            detail[nid] = _norm(index.node_detail(nid), repo)
        except Exception as e:              # a bad node must not kill the oracle
            detail[nid] = {'error': '%s: %s' % (type(e).__name__, e)}
    out['nodeDetail'] = detail

    # ---- find / refs --------------------------------------------------------
    out['find'] = {}
    for q in FIND_QUERIES:
        try:
            out['find'][q] = _norm(index.find(q, limit=400), repo)
        except Exception as e:
            out['find'][q] = {'error': '%s: %s' % (type(e).__name__, e)}

    # ---- kinds / definitions ------------------------------------------------
    out['kinds'] = _norm(index.kinds(), repo)
    out['definitions'] = {
        'public': _norm(index.definitions('public', limit=400), repo),
        'types': _norm(index.definitions('types', limit=400), repo),
    }

    # ---- chain --------------------------------------------------------------
    out['chain'] = {}
    for nid in DOC_NODES:
        if nid in index.nodes:
            for direction in ('forward', 'backward', 'both'):
                key = '%s|%s' % (nid, direction)
                try:
                    out['chain'][key] = _norm(
                        index.chain(nid, depth=3, direction=direction, max_nodes=300), repo)
                except Exception as e:
                    out['chain'][key] = {'error': '%s: %s' % (type(e).__name__, e)}

    # ---- synthesized documents ---------------------------------------------
    out['docs'] = {}
    for nid in DOC_NODES:
        if nid not in index.nodes:
            continue
        try:
            payload = index.doc_source(nid)
        except Exception as e:
            payload = {'error': '%s: %s' % (type(e).__name__, e)}
        if payload is None:
            payload = None
        out['docs'][nid] = _norm(_doc_digest(payload), repo)

    # ---- reorg plan + verify (no Roslyn: that is a separate, opt-in check) --
    out['reorg'] = {}
    for i, spec in enumerate(REORG_SPECS):
        key = json.dumps(spec, sort_keys=True)
        try:
            payload = _norm(index.reorg(spec, action='verify'), repo)
            # `report.ms` is wall-clock duration; the two engines cannot and
            # must not agree on how long they took.
            _strip_timing(payload)
            out['reorg'][key] = payload
        except Exception as e:
            out['reorg'][key] = {'error': '%s: %s' % (type(e).__name__, e)}
    return out


def _strip_timing(obj):
    """Remove every `ms` key, recursively (timing is not an engine answer)."""
    if isinstance(obj, dict):
        obj.pop('ms', None)
        for v in obj.values():
            _strip_timing(v)
    elif isinstance(obj, list):
        for v in obj:
            _strip_timing(v)
    return obj


def _doc_digest(payload):
    """Keep the whole doc payload but shrink `lines` to a hash + sample."""
    if not isinstance(payload, dict):
        return payload
    import hashlib
    out = dict(payload)
    lines = out.pop('lines', None)
    if isinstance(lines, list):
        blob = '\n'.join('%s\x1f%s\x1f%s\x1f%s' % (
            l.get('n'), l.get('ctx'), l.get('sec') or '',
            l.get('text') or '') for l in lines)
        out['lineCount'] = len(lines)
        out['linesSha256'] = hashlib.sha256(blob.encode('utf-8', 'replace')).hexdigest()
        out['linesHead'] = lines[:40]
        out['linesTail'] = lines[-40:]
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--root', action='append', default=None)
    ap.add_argument('--exclude', action='append', default=None)
    ap.add_argument('-o', '--out', default='-')
    args = ap.parse_args()

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    sys.path.insert(0, os.path.join(repo, 'tools', 'csgraph'))
    import server as S
    cfg = dict(S.DEFAULT_CONFIG) if hasattr(S, 'DEFAULT_CONFIG') else {}
    cfg = {'roots': roots, 'excludes': [], 'defines': [], 'player': False}
    excludes = args.exclude if args.exclude is not None else [
        '.git', 'obj', 'bin', 'tmp', 'node_modules', '_bak', 'testing']

    payload = dump(repo, roots, excludes, cfg)
    text = json.dumps(payload, indent=1, sort_keys=True, ensure_ascii=False)
    if args.out == '-':
        sys.stdout.write(text + '\n')
    else:
        with open(args.out, 'w', encoding='utf-8') as fh:
            fh.write(text + '\n')
        sys.stderr.write('wrote %s (%d bytes)\n' % (args.out, len(text)))


if __name__ == '__main__':
    main()
