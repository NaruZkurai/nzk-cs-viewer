#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""oracle_parser.py - canonical JSON of the frozen cs_parser.py engine.

This is the *parser/graph* half of the cloning contract: it loads
`tools/csgraph/cs_parser.py` **by path** (that directory is frozen and is never
imported from `sys.path`), runs `parse_source` for every file `discover_files`
finds under the requested roots, builds the graph with `build_graph`, and dumps
one deterministic JSON document:

    {
      "files":         [ {file, types, members, inherits, warnings}, ... ],
      "graphWarnings": [ ... ],                  # GraphBuilder.warnings
      "nodes":         [ ... ],                  # sorted by id
      "edges":         [ ... ]                   # sorted by (source, target, kind)
    }

Because every `file` field is the repo-relative display path, the document
contains no absolute paths; any that survive (e.g. in a warning) are rewritten
to repo-relative form. There are no timing fields because `parse_source` and
`build_graph` produce none.

`defines=None` is passed on purpose: the ported surface is `cs_parser.py` only,
and "no define source" is the documented behaviour that keeps every conditional
branch ("a plain C# folder is never silently gutted").

    python3 tools/oracle_parser.py --repo /path/to/vrcCS --root _sealed_src
"""

import argparse
import importlib.util
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
WORKSPACE = os.path.dirname(HERE)                       # rust-only-csviewer/
DEFAULT_REPO = os.path.dirname(os.path.dirname(WORKSPACE))

PARSER_REL = os.path.join('tools', 'csgraph', 'cs_parser.py')


def load_parser(repo):
    """Import the frozen cs_parser.py by path (never by sys.path)."""
    path = os.path.join(repo, PARSER_REL)
    if not os.path.isfile(path):
        raise SystemExit('oracle_parser: no such file: %s' % path)
    spec = importlib.util.spec_from_file_location('cs_parser_frozen', path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def display(path, repo):
    """server.py::Index.display - repo-relative when inside the repo."""
    rel = os.path.relpath(path, repo)
    if rel.startswith('..'):
        return os.path.normpath(path).replace(os.sep, '/')
    return rel.replace(os.sep, '/')


def _rel(value, repo):
    if not isinstance(value, str) or not value:
        return value
    if value.startswith(repo):
        rel = value[len(repo):].lstrip('/')
        return rel or '.'
    return value


def canon(obj, repo):
    """Recursively rewrite absolute repo paths to repo-relative ones."""
    if isinstance(obj, dict):
        return {k: canon(v, repo) for k, v in obj.items()}
    if isinstance(obj, list):
        return [canon(v, repo) for v in obj]
    if isinstance(obj, str):
        return _rel(obj, repo)
    return obj


def build(repo, roots, excludes):
    """Run the frozen parser + graph builder over the corpus."""
    C = load_parser(repo)
    files = C.discover_files(repo, roots, list(excludes))
    parses = [C.parse_source(f, display(f, repo), None) for f in files]
    graph = C.build_graph(parses)

    files_json = [
        {
            'file': pr['file'],
            'types': pr['types'],
            'members': pr['members'],
            'inherits': pr['inherits'],
            'warnings': pr['warnings'],
        }
        for pr in parses
    ]
    files_json.sort(key=lambda r: r['file'])

    nodes = [graph.nodes[nid] for nid in sorted(graph.nodes)]
    edges = sorted(graph.edges, key=lambda e: (e['source'], e['target'], e['kind']))
    payload = {
        'files': files_json,
        'graphWarnings': list(graph.warnings),
        'nodes': nodes,
        'edges': edges,
    }
    return canon(payload, repo), files


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--root', action='append', default=None,
                    help='root relative to --repo (repeatable; default _sealed_src)')
    ap.add_argument('--exclude', action='append', default=None,
                    help='discovery exclude (repeatable)')
    ap.add_argument('-o', '--out', default='-')
    args = ap.parse_args(argv)

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    excludes = args.exclude if args.exclude is not None else []

    payload, _files = build(repo, roots, excludes)
    text = json.dumps(payload, sort_keys=True, ensure_ascii=False)
    if args.out == '-':
        sys.stdout.write(text + '\n')
    else:
        with open(args.out, 'w', encoding='utf-8') as fh:
            fh.write(text + '\n')
        sys.stderr.write('wrote %s (%d bytes)\n' % (args.out, len(text)))


if __name__ == '__main__':
    main()
