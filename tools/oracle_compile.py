#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""oracle_compile.py - canonical JSON of the frozen compile_check.differential.

This is the *runnable* half of the compile-check cloning contract. It loads the
frozen `tools/csgraph/` engine (`cs_parser.py` / `cs_doc.py` / `cs_reorg.py` /
`compile_check.py`) **by path** - that directory is frozen and is never imported
from `sys.path` - rebuilds the `_sealed_src` corpus exactly the way
`server.py::Index.build` does, plans and emits the `group=single`
reorganisation, and calls the frozen `compile_check.differential` on the two
trees.

The `ok` override `server.py` applies to the differential result is mirrored
here so the document is directly comparable with `csgraph dump-compile`:

    {
      "ok": ..., "skipped": ...,
      "baseline": {...}, "candidate": {...},
      "newDiagnostics": [...], "worseDiagnostics": [...],
      "structuralDiagnostics": [...], "resolvedDiagnostics": [...]
    }

Pass `--data DIR` to force a specific Unity `.../Editor/Data` directory; pass
`--data /nonexistent` to exercise the `skipped` degradation deterministically
on a machine that *does* have Unity installed.

    python3 tools/oracle_compile.py --repo /path/to/vrcCS
"""

import argparse
import importlib.util
import json
import os
import sys

# `tools/csgraph/` is frozen: never let an import drop a `__pycache__` into it.
sys.dont_write_bytecode = True

HERE = os.path.dirname(os.path.abspath(__file__))
WORKSPACE = os.path.dirname(HERE)                       # rust-only-csviewer/
BUILDER = os.path.dirname(WORKSPACE)
DEFAULT_REPO = os.path.dirname(BUILDER)

PARSER_REL = os.path.join('tools', 'csgraph', 'cs_parser.py')
DOC_REL = os.path.join('tools', 'csgraph', 'cs_doc.py')
REORG_REL = os.path.join('tools', 'csgraph', 'cs_reorg.py')
COMPILE_REL = os.path.join('tools', 'csgraph', 'compile_check.py')


def load_module(name, path):
    """Import a frozen module by path, under its real import name."""
    if not os.path.isfile(path):
        raise SystemExit('oracle_compile: no such file: %s' % path)
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


def load_engine(repo):
    """Frozen cs_parser + cs_doc + cs_reorg + compile_check, wired by path."""
    C = load_module('cs_parser', os.path.join(repo, PARSER_REL))
    load_module('cs_doc', os.path.join(repo, DOC_REL))
    RG = load_module('cs_reorg', os.path.join(repo, REORG_REL))
    CC = load_module('compile_check', os.path.join(repo, COMPILE_REL))
    return C, RG, CC


def display(path, repo):
    """server.py::Index.display - repo-relative when inside the repo."""
    rel = os.path.relpath(path, repo)
    if rel.startswith('..'):
        return os.path.normpath(path).replace(os.sep, '/')
    return rel.replace(os.sep, '/')


def build(repo, roots, excludes):
    """Build the graph + file texts exactly the way Index.build does."""
    C, RG, CC = load_engine(repo)
    files = C.discover_files(repo, roots, list(excludes))
    parses = [C.parse_source(f, display(f, repo), None) for f in files]
    graph = C.build_graph(parses)

    file_texts = {}
    for f in files:
        rel = display(f, repo)
        with open(f, encoding='utf-8', errors='replace') as fh:
            file_texts[rel] = fh.read()

    # `normalise_nodes` accepts the raw graph nodes (`_parts`) directly, which
    # is exactly what `tools/oracle_reorg.py` does; the Rust side uses
    # `reorg::corpus_graph`, and `tools/diff_reorg.py` proves the two agree.
    engine = RG.Reorg(graph.nodes, file_texts)
    return engine, file_texts, CC


def dump(repo, roots, excludes, data=None):
    engine, file_texts, CC = build(repo, roots, excludes)
    plan = engine.plan({'group': 'single'})
    files = engine.emit(plan, header=True)
    report = engine.verify(files, plan=plan)

    res = CC.differential(file_texts, files, data=data,
                          defines=tuple(CC.DEFAULT_DEFINES))
    # server.py: res['ok'] = bool(res.get('ok')) and report['ok']
    res['ok'] = bool(res.get('ok')) and bool(report['ok'])
    return res


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--root', action='append', default=None,
                    help='root relative to --repo (repeatable; default _sealed_src)')
    ap.add_argument('--exclude', action='append', default=None,
                    help='discovery exclude (repeatable)')
    ap.add_argument('--data', default=None,
                    help='Unity .../Editor/Data directory (default: discovery)')
    ap.add_argument('-o', '--out', default='-')
    args = ap.parse_args(argv)

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    excludes = args.exclude if args.exclude is not None else []

    payload = dump(repo, roots, excludes, data=args.data)
    text = json.dumps(payload, sort_keys=True, ensure_ascii=False)
    if args.out == '-':
        sys.stdout.write(text + '\n')
    else:
        with open(args.out, 'w', encoding='utf-8') as fh:
            fh.write(text + '\n')
        sys.stderr.write('wrote %s (%d bytes)\n' % (args.out, len(text)))


if __name__ == '__main__':
    main()
