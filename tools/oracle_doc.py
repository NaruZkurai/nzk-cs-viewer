#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""oracle_doc.py - dump the synthesized documents the frozen cs_doc.py builds.

This is the cloning contract for `core/src/doc.rs`. It imports the frozen v1
modules *by path* (never edited), builds the same graph `server.py` would,
then for a set of nodes dumps the **full** `cs_doc.prepare()` payload as
canonical JSON with absolute repo paths made repo-relative.

`csgraph dump-doc` prints the same document and `tools/diff_doc.py` compares
them.

Node selection: the explicit ids always win; then the first nodes that add a
missing `ctx` kind (`part`, `head`, `body`, `cond`, `tail`) or a missing node
class (namespace/type/member) are appended, deterministically by id order.
Pass `--node` (repeatable) to pin the list exactly.

    python3 tools/oracle_doc.py --repo /path/to/vrcCS -o gold.json
    python3 tools/oracle_doc.py --repo /path/to/vrcCS --graph-out snap.json
"""

import argparse
import hashlib
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)                     # rust-only-csviewer/
DEFAULT_REPO = os.path.dirname(os.path.dirname(ROOT))

TYPE_KINDS = ('class', 'struct', 'interface', 'enum', 'record')
CTX_KINDS = ('part', 'head', 'body', 'cond', 'tail')

# The pinned acceptance nodes. In this corpus every declaration sits under the
# single namespace `NZK` (the nested `NZK.Core`/`NZK.Core.MA` levels are types,
# not namespaces), so `#ns:NZK.Core` has no node; it is still listed, and both
# sides then agree on `null` for it.
PINNED_NODES = ('#ns:NZK', '#ns:NZK.Core', 'NZK.Core',
                'NZK.Core.MeshesMerge.MergedRootName')

# Absolute paths are made repo-relative, exactly like tools/oracle.py.
_CS_ROOT = os.path.abspath(DEFAULT_REPO)


def _rel(value, repo):
    if not isinstance(value, str) or not value:
        return value
    if value.startswith(repo):
        rel = value[len(repo):].lstrip('/')
        return rel or '.'
    return value


def _norm(obj, repo):
    if isinstance(obj, dict):
        return {k: _norm(v, repo) for k, v in obj.items()}
    if isinstance(obj, list):
        return [_norm(v, repo) for v in obj]
    if isinstance(obj, str):
        return _rel(obj, repo)
    return obj


def load_v1(repo):
    """Import the frozen cs_parser + cs_doc (and server, for resolve_defines)."""
    sys.dont_write_bytecode = True          # never touch the frozen __pycache__
    path = os.path.join(repo, 'tools', 'csgraph')
    if path not in sys.path:
        sys.path.insert(0, path)
    import cs_parser as C                   # noqa: E402
    import cs_doc as CD                     # noqa: E402
    try:
        import server as S                  # noqa: E402
    except Exception:                       # a missing GUI dep must not stop us
        S = None
    return C, CD, S


def display(repo, path):
    """server.Index.display — repo-relative when inside the repo."""
    rel = os.path.relpath(path, repo)
    if rel.startswith('..'):
        return os.path.normpath(path).replace(os.sep, '/')
    return rel.replace(os.sep, '/')


def resolve_defines(S, repo, cfg, roots):
    if S is not None and hasattr(S, 'resolve_defines'):
        try:
            return S.resolve_defines(repo, cfg, roots)
        except Exception:
            pass
    return set(), False


def build_corpus(repo, roots, excludes, cfg, pinned=None):
    """The `nodes` / `file_texts` maps cs_doc.prepare consumes, plus defines."""
    C, CD, S = load_v1(repo)
    if pinned is not None:
        defines = set(pinned)
        known = True
    else:
        syms, known = resolve_defines(S, repo, cfg, roots)
        defines = syms if known else None

    files = C.discover_files(repo, roots, excludes)
    parses = []
    file_texts = {}
    for f in files:
        rel = display(repo, f)
        try:
            parses.append(C.parse_source(f, rel, defines))
        except Exception as exc:            # one bad file must not kill the scan
            sys.stderr.write('oracle_doc: parse %s: %s\n' % (rel, exc))
            continue
        try:
            with open(f, encoding='utf-8', errors='replace') as fh:
                file_texts[rel] = fh.read()
        except OSError as exc:
            sys.stderr.write('oracle_doc: read %s: %s\n' % (rel, exc))

    g = C.build_graph(parses)

    # server.py's public node shape (the fields doc.rs reads).
    nodes = {}
    for nid, n in g.nodes.items():
        nodes[nid] = {
            'id': n['id'], 'name': n.get('name', ''), 'label': n.get('label', ''),
            'full': n.get('full', n['id']), 'kind': n['kind'], 'ns': n.get('ns', ''),
            'owner': n.get('owner', ''), 'ownerKind': n.get('ownerKind', ''),
            'parent': n.get('parent', ''),
            'file': n.get('file', ''), 'line': n.get('line', 0),
            'endLine': n.get('endLine', 0), 'signature': n.get('signature', ''),
            'ret': n.get('ret', ''), 'params': n.get('params', []),
            'access': n.get('access', ''), 'static': n.get('static', False),
            'head': list(n.get('head') or []),
            'tail': list(n.get('tail') or []),
            'startOff': n.get('startOff'), 'endOff': n.get('endOff'),
            'parts': [dict(p) for p in (n.get('_parts') or [])],
        }
    return CD, nodes, file_texts, defines, known


def node_class(node):
    kind = node.get('kind')
    if kind == 'namespace':
        return 'namespace'
    if kind in TYPE_KINDS:
        return 'type'
    if kind in ('string', 'external'):
        return 'literal'
    return 'member'


def choose_nodes(CD, nodes, file_texts, defines, known, cache, explicit):
    """Deterministic node list covering every ctx kind and node class."""
    builder = CD.DocBuilder(nodes, file_texts, defines, known, cache)
    chosen = []
    chosen_set = set()
    covered = set()
    classes = set()

    def consider(nid, force=False):
        if nid in chosen_set:
            return False
        node = nodes.get(nid)
        if node is None:
            return False
        try:
            doc = builder.build(nid)
        except Exception:
            return False
        if doc is None:
            return False
        ctxs = set(l.get('ctx') for l in doc['lines'])
        add = force or bool(ctxs - covered) or (node_class(node) not in classes)
        if not add:
            return False
        chosen.append(nid)
        chosen_set.add(nid)
        covered.update(ctxs)
        classes.add(node_class(node))
        return True

    for nid in explicit:
        # Pinned ids are always listed, even when the corpus has no such node:
        # both sides then report `null` for it.
        if nid in chosen_set:
            continue
        if nodes.get(nid) is None:
            chosen.append(nid)
            chosen_set.add(nid)
            continue
        consider(nid, force=True)
    for nid in sorted(nodes):
        if len(covered) >= len(CTX_KINDS) and \
                {'namespace', 'type', 'member'} <= classes:
            break
        if nodes[nid].get('kind') == 'namespace':
            continue
        consider(nid)
    return chosen, covered, classes


def dump_docs(CD, nodes, file_texts, defines, known, node_ids):
    cache = {}
    docs = {}
    for nid in node_ids:
        payload = None
        if nid in nodes:
            try:
                payload = CD.prepare(nodes, file_texts, nid, defines=defines,
                                     defines_known=known, cache=cache)
            except Exception as exc:        # server.py catches per node
                sys.stderr.write('oracle_doc: doc %s: %s\n' % (nid, exc))
                payload = None
        docs[nid] = payload
    return docs


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--root', action='append', default=None)
    ap.add_argument('--exclude', action='append', default=None)
    ap.add_argument('--node', action='append', default=None,
                    help='pin a node id (repeatable); default is auto-selected')
    ap.add_argument('--define', action='append', default=None,
                    help='pin a conditional symbol (repeatable); forces known')
    ap.add_argument('--graph-out', default=None,
                    help='also write the {nodes,file_texts,...} snapshot')
    ap.add_argument('-o', '--out', default='-')
    args = ap.parse_args()

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    excludes = args.exclude if args.exclude is not None else [
        '.git', 'obj', 'bin', 'tmp', 'node_modules', '_bak', 'testing']
    cfg = {'roots': roots, 'excludes': [], 'defines': [], 'player': False}

    pinned = sorted(set(str(s) for s in args.define)) if args.define else None
    CD, nodes, file_texts, defines, known = build_corpus(
        repo, roots, excludes, cfg, pinned)

    if args.node:
        node_ids = [nid for nid in args.node]
        missing = [nid for nid in node_ids if nid not in nodes]
        if missing:
            sys.stderr.write('oracle_doc: unknown node(s): %s\n' % ', '.join(missing))
    else:
        explicit = list(PINNED_NODES)
        cache = {}
        node_ids, covered, classes = choose_nodes(
            CD, nodes, file_texts, defines, known, cache, explicit)
        sys.stderr.write('oracle_doc: %d nodes; ctx=%s classes=%s\n' % (
            len(node_ids), sorted(covered), sorted(classes)))
        missing_ctx = [c for c in CTX_KINDS if c not in covered]
        if missing_ctx:
            sys.stderr.write('oracle_doc: ctx kinds not found: %s\n'
                             % ', '.join(missing_ctx))

    docs = dump_docs(CD, nodes, file_texts, defines, known, node_ids)
    docs = {nid: (_norm(payload, repo) if payload is not None else None)
            for nid, payload in docs.items()}

    payload = {
        'repo': os.path.basename(repo.rstrip('/')),
        'defines': sorted(defines) if defines is not None else None,
        'definesKnown': known,
        'nodeIds': node_ids,
        'docs': docs,
    }
    text = json.dumps(payload, sort_keys=True, ensure_ascii=False)
    if args.out == '-':
        sys.stdout.write(text + '\n')
    else:
        with open(args.out, 'w', encoding='utf-8') as fh:
            fh.write(text + '\n')
        sys.stderr.write('wrote %s (%d bytes)\n' % (args.out, len(text)))

    if args.graph_out:
        graph = {
            'repo': repo,
            'defines': sorted(defines) if defines is not None else None,
            'definesKnown': known,
            'nodeIds': node_ids,
            'nodes': nodes,
            'file_texts': file_texts,
        }
        gtext = json.dumps(graph, sort_keys=True, ensure_ascii=False)
        with open(args.graph_out, 'w', encoding='utf-8') as fh:
            fh.write(gtext)
        sys.stderr.write('wrote %s (%d bytes)\n' % (args.graph_out, len(gtext)))


if __name__ == '__main__':
    main()
