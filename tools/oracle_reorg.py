#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""oracle_reorg.py - canonical JSON of the frozen cs_reorg.py engine.

This is the *reorganisation* half of the cloning contract: it loads
`tools/csgraph/cs_reorg.py` (and the `cs_parser.py` / `cs_doc.py` it imports)
**by path** (that directory is frozen and is never imported from `sys.path`),
builds the graph over the requested roots exactly the way `server.py`'s
`Index.build` does, then dumps one deterministic JSON document:

    {
      "specs": {
        "group=single": {
          "plan":     <Reorg.plan>,
          "report":   <Reorg.verify>,
          "emitted":  [ {path, sha1, lines, chars, text}, ... ]   # only for
                      the `single` spec; null otherwise
        },
        ...
      }
    }

The specs are the ones `tools/oracle.py` already used for the reorg surface.
Because every `file` field is the repo-relative display path, the document
contains no absolute paths; any that survive (e.g. in a warning) are rewritten
to repo-relative form. The timing field `ms` is dropped from every report.

    python3 tools/oracle_reorg.py --repo /path/to/vrcCS --root _sealed_src
    python3 tools/oracle_reorg.py --repo /path/to/vrcCS --spec 'group=owner' --time
"""

import argparse
import hashlib
import importlib.util
import json
import os
import sys
import time

# `tools/csgraph/` is frozen: never let an import drop a `__pycache__` into it.
sys.dont_write_bytecode = True

HERE = os.path.dirname(os.path.abspath(__file__))
WORKSPACE = os.path.dirname(HERE)                       # rust-only-csviewer/
BUILDER = os.path.dirname(WORKSPACE)
DEFAULT_REPO = os.path.dirname(BUILDER)

PARSER_REL = os.path.join('tools', 'csgraph', 'cs_parser.py')
DOC_REL = os.path.join('tools', 'csgraph', 'cs_doc.py')
REORG_REL = os.path.join('tools', 'csgraph', 'cs_reorg.py')

# the exact specs tools/oracle.py drives the reorg surface with
REORG_SPECS = [
    {'group': 'single'},
    {'group': 'size', 'size': 4000},
    {'granularity': 'leaf', 'group': 'size', 'size': 3000},
    {'group': 'namespace'},
    {'group': 'owner'},
]


def load_module(name, path):
    """Import a frozen module by path, under its real import name."""
    if not os.path.isfile(path):
        raise SystemExit('oracle_reorg: no such file: %s' % path)
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


def load_engine(repo):
    """Frozen cs_parser + cs_doc + cs_reorg, wired together by path."""
    C = load_module('cs_parser', os.path.join(repo, PARSER_REL))
    load_module('cs_doc', os.path.join(repo, DOC_REL))
    RG = load_module('cs_reorg', os.path.join(repo, REORG_REL))
    return C, RG


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


def spec_key(spec):
    """The stable label `csgraph dump-reorg` also computes."""
    def rep(v):
        if v is True:
            return 'True'
        if v is False:
            return 'False'
        if v is None:
            return 'None'
        return str(v)
    return '|'.join('%s=%s' % (k, rep(spec[k])) for k in sorted(spec))


def build(repo, roots, excludes):
    """Build the graph + file texts exactly the way Index.build does."""
    C, RG = load_engine(repo)
    files = C.discover_files(repo, roots, list(excludes))
    parses = [C.parse_source(f, display(f, repo), None) for f in files]
    graph = C.build_graph(parses)

    file_texts = {}
    for f in files:
        rel = display(f, repo)
        with open(f, encoding='utf-8', errors='replace') as fh:
            file_texts[rel] = fh.read()

    # `normalise_nodes` accepts the raw graph nodes (`_parts`) directly; passing
    # them keeps this oracle independent of server.py's re-shaping.
    engine = RG.Reorg(graph.nodes, file_texts)
    return engine


def dump(repo, roots, excludes, wanted=None, timing=False):
    t_graph = time.time()
    engine = build(repo, roots, excludes)
    if timing:
        sys.stderr.write('[oracle_reorg] graph %.2fs\n' % (time.time() - t_graph))
    specs = {}
    for spec in REORG_SPECS:
        key = spec_key(spec)
        if wanted and key not in wanted:
            continue
        t_plan = time.time()
        plan = engine.plan(spec)
        d_plan = time.time() - t_plan
        t_emit = time.time()
        files = engine.emit(plan, header=spec.get('header', True))
        d_emit = time.time() - t_emit
        t_verify = time.time()
        report = engine.verify(files, plan=plan)
        d_verify = time.time() - t_verify
        if timing:
            sys.stderr.write(
                '[oracle_reorg] %s: plan %.2fs, emit %.2fs, verify %.2fs\n'
                % (key, d_plan, d_emit, d_verify))
        report = dict(report)
        report.pop('ms', None)

        entry = {'plan': plan, 'report': report, 'emitted': None}
        if key == 'group=single':
            listed = []
            for path in sorted(files):
                text = files[path]
                listed.append({
                    'path': path,
                    'sha1': hashlib.sha1(text.encode('utf-8')).hexdigest(),
                    'lines': len(text.split('\n')),
                    'chars': len(text),
                    'text': text,
                })
            entry['emitted'] = listed
        specs[key] = entry

    return canon({'specs': specs}, repo)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--root', action='append', default=None,
                    help='root relative to --repo (repeatable; default _sealed_src)')
    ap.add_argument('--exclude', action='append', default=None,
                    help='discovery exclude (repeatable)')
    ap.add_argument('-o', '--out', default='-')
    ap.add_argument('--spec', action='append', default=None,
                    help='only run this spec key (repeatable; default: all five)')
    ap.add_argument('--time', action='store_true',
                    help='print per-phase timings to stderr')
    args = ap.parse_args(argv)

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    excludes = args.exclude if args.exclude is not None else []

    payload = dump(repo, roots, excludes, wanted=args.spec, timing=args.time)
    text = json.dumps(payload, sort_keys=True, ensure_ascii=False)
    if args.out == '-':
        sys.stdout.write(text + '\n')
    else:
        with open(args.out, 'w', encoding='utf-8') as fh:
            fh.write(text + '\n')
        sys.stderr.write('wrote %s (%d bytes)\n' % (args.out, len(text)))


if __name__ == '__main__':
    main()
