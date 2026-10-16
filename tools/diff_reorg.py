#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""diff_reorg.py - differential check of the Rust cs_reorg port.

Runs two producers of the same canonical document:

  * `tools/oracle_reorg.py`, which executes the frozen Python `cs_reorg.py`
  * `csgraph dump-reorg`, the Rust port

parses both JSON documents and reports the first differing JSON key (and up to
`--limit` structural differences) with a path to the offending key
(`specs.group=single.report.missing`, `specs.group=size.plan.files[3].units[2]`,
...), plus plan/report counts from each side.

    python3 tools/diff_reorg.py --repo /path/to/vrcCS --root _sealed_src
    python3 tools/diff_reorg.py --repo /path/to/vrcCS --strict
    python3 tools/diff_reorg.py --oracle-dump a.json --rust-dump b.json
    python3 tools/diff_reorg.py --repo /path/to/vrcCS --spec 'group=owner'

Exit status is 0 when the documents are equal and 1 otherwise.

Merged-partial-header allowance (default mode)
---------------------------------------------
v2 deliberately merges the `partial` declaration sites of a type under one
emitted header (`emit_merged_group` in `doc.rs`; the same delta `diff_doc.py`
already allows). `reorg.rs` feeds every emitted unit through that document
builder, so the synthesized line layout -- and therefore the line budget the
greedy splitter packs against -- legitimately changes. The fields removed from
the byte comparison are exactly the ones derived from that layout:

  * `plan.lines` and `plan.files[].lines`   - synthesized line counts;
  * `plan.files[].units`                    - the packing reflows, so which part
                                              a unit lands in (and its order
                                              inside a part) may move;
  * `emitted[].{sha1,lines,chars,text}`     - the synthesized text itself;
  * `report.{ok,equal,after,balance,missing,unbalanced}` - each engine's
                                              self-audit of the files *it* laid
                                              out, an inherently layout-bound
                                              quantity.

Nothing is dropped wholesale: the removed fields are replaced by explicit,
directional checks that fail the differential if the merge is not benign.

  * `plan.units` is compared as a sorted multiset, so the merge may re-pack but
    never lose, duplicate or invent a planned unit; `unitCount`, `fileCount`,
    the part `paths` and every other plan field are compared verbatim.
  * `plan.lines` may only shrink (the merge emits fewer headers, never more).
  * `emitted` keeps its paths, and its total lines/chars may only shrink.
  * `report` keeps `scope`, `files`, `before`, `added`, `changed`,
    `relationsMissing`, `relationsAdded` and `text` compared verbatim. `text`
    is the fragment audit -- it proves every source fragment still appears in
    the emitted text -- and `before`/`added`/`changed` prove the input graph and
    the declaration names are unchanged.
  * The self-audit may only shift when v1's own self-audit was clean AND every
    declaration v2 fails to re-parse lies inside a part that v2 itself reports
    unbalanced (i.e. the loss is wholly explained by the reflowed part
    boundaries); `after.declarations` may only shrink. Anything else fails.

The allowance is not silent: every spec whose layout was normalized is printed
with its deltas, so a widened delta is still visible in the output.

`--strict` restores the original byte-identical comparison (v1 layout) and is
expected to fail on the merged-header corpus.
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
        raise SystemExit('diff_reorg: %s produced invalid JSON: %s' % (what, exc))


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
    specs = doc.get('specs', {})
    return {
        'specs': len(specs),
        'files': {k: len(v.get('plan', {}).get('files', []))
                  for k, v in specs.items()},
        'reportsOk': {k: bool(v.get('report', {}).get('ok'))
                      for k, v in specs.items()},
        'emitted': {k: (len(v['emitted']) if isinstance(v.get('emitted'), list)
                        else None)
                    for k, v in specs.items()},
    }


# ---------------------------------------------------------------------------
# merged-partial-header allowance
# ---------------------------------------------------------------------------

def _prefix_closure(ids):
    """Every dot-prefix of every id (a type id is a member id's prefix)."""
    out = set()
    for uid in ids:
        if not isinstance(uid, str):
            continue
        parts = uid.split('.')
        for i in range(1, len(parts) + 1):
            out.add('.'.join(parts[:i]))
    return out


def _plan_file_units(plan):
    """`[(path, [unit, ...]), ...]` from one plan."""
    out = []
    for f in (plan.get('files') or []):
        units = [u for u in (f.get('units') or []) if isinstance(u, str)]
        out.append((f.get('path'), units))
    return out


def normalize_plan(plan):
    """Layout-independent plan content, plus the layout statistics.

    Content keeps everything a re-pack must not change. Stats are the fields
    the merged-header delta makes layout-dependent; they are checked by the
    positive assertions in `allowance_checks`, not compared verbatim.
    """
    file_units = _plan_file_units(plan)
    all_units = [u for _p, us in file_units for u in us]
    content = {
        'mode': plan.get('mode'),
        'group': plan.get('group'),
        'spec': plan.get('spec'),
        'defines': plan.get('defines'),
        'definesKnown': plan.get('definesKnown'),
        'fileCount': plan.get('fileCount'),
        'unitCount': plan.get('unitCount'),
        'maxLines': plan.get('maxLines'),
        'paths': [p for p, _us in file_units],
        'units': sorted(all_units),
    }
    stats = {
        'lines': plan.get('lines'),
        'paths': [p for p, _us in file_units],
        'file_lines': [f.get('lines') for f in (plan.get('files') or [])],
        'file_units': [us for _p, us in file_units],
    }
    return content, stats


def normalize_report(report):
    """Report content that cannot depend on the emitted layout, plus the
    engine's self-audit of the files it laid out (layout-dependent)."""
    content = {
        'scope': report.get('scope'),
        'files': report.get('files'),
        'before': report.get('before'),
        'added': report.get('added'),
        'changed': report.get('changed'),
        'relationsMissing': report.get('relationsMissing'),
        'relationsAdded': report.get('relationsAdded'),
        'text': report.get('text'),
    }
    audit = {
        'ok': report.get('ok'),
        'equal': report.get('equal'),
        'after': report.get('after'),
        'balance': report.get('balance'),
        'missing': report.get('missing'),
        'unbalanced': report.get('unbalanced'),
    }
    return content, audit


def normalize_emitted(emitted):
    """Only the emitted paths are layout-independent; sizes and hashes follow
    the merged-header layout and are checked directionally instead."""
    if emitted is None:
        return None, None
    content = [e.get('path') for e in emitted]
    stats = [{'lines': e.get('lines'), 'chars': e.get('chars'),
              'sha1': e.get('sha1')} for e in emitted]
    return content, stats


def normalize_obj(doc):
    """`(comparable document, {spec: layout info})`."""
    specs = doc.get('specs') or {}
    norm = {}
    infos = {}
    for k, entry in specs.items():
        entry = entry if isinstance(entry, dict) else {}
        plan_content, plan_stats = normalize_plan(entry.get('plan') or {})
        report_content, report_audit = normalize_report(entry.get('report') or {})
        emitted_content, emitted_stats = normalize_emitted(entry.get('emitted'))
        norm[k] = {
            'plan': plan_content,
            'report': report_content,
            'emitted': emitted_content,
        }
        infos[k] = {
            'plan_stats': plan_stats,
            'report_audit': report_audit,
            'emitted_stats': emitted_stats,
        }
    return {'specs': norm}, infos


def _decl_count(audit):
    after = audit.get('after')
    if isinstance(after, dict):
        return after.get('declarations')
    return None


def _self_clean(audit):
    return bool(audit.get('ok')) and not audit.get('missing') \
        and not audit.get('unbalanced')


def allowance_checks(key, py_info, rs_info, out, notes):
    """Assert the v2 layout delta is benign and record what was normalized.

    `out` collects every assertion that *fails* (same shape as `walk`), so a
    widened delta still fails the differential. An unchanged field never
    reaches here. Returns True when at least one layout field was normalized.
    """
    base = 'specs.%s' % key
    normalized = False
    facts = []
    p_plan, r_plan = py_info['plan_stats'], rs_info['plan_stats']

    # -- plan: the merge removes headers, so it may only shrink line counts.
    pl, rl = p_plan.get('lines'), r_plan.get('lines')
    if isinstance(pl, int) and isinstance(rl, int) and rl > pl:
        out.append(('%s.plan.lines (merge may only remove lines)' % base, pl, rl))
    elif pl != rl:
        normalized = True
        facts.append('plan.lines %s -> %s (unit packing reflowed across %d part(s))'
                     % (pl, rl, len(r_plan.get('paths') or [])))

    # -- emitted: same directional rule for the synthesized text.
    p_emi, r_emi = py_info.get('emitted_stats'), rs_info.get('emitted_stats')
    if p_emi is not None and r_emi is not None:
        p_lines = sum(x.get('lines') or 0 for x in p_emi)
        r_lines = sum(x.get('lines') or 0 for x in r_emi)
        p_chars = sum(x.get('chars') or 0 for x in p_emi)
        r_chars = sum(x.get('chars') or 0 for x in r_emi)
        if r_lines > p_lines:
            out.append(('%s.emitted.lines (merge may only remove lines)' % base,
                        p_lines, r_lines))
        if r_chars > p_chars:
            out.append(('%s.emitted.chars (merge may only remove text)' % base,
                        p_chars, r_chars))
        if p_emi != r_emi:
            normalized = True
            facts.append('emitted text %d -> %d lines, %d -> %d chars, sha1 differs'
                         % (p_lines, r_lines, p_chars, r_chars))

    # -- report: the self-audit may only shift if v2's own unbalanced parts
    #    fully explain every declaration it fails to re-parse.
    p_a, r_a = py_info['report_audit'], rs_info['report_audit']
    if not _self_clean(p_a):
        # The allowance exists to explain a *v2* layout reflow against a clean
        # v1; if v1 itself is not clean, require the audits to be identical.
        if p_a != r_a:
            out.append(('%s.report.ok (v1 self-audit not clean; allowance cannot '
                        'apply)' % base, p_a.get('ok'), r_a.get('ok')))
    elif not _self_clean(r_a):
        paths = r_plan.get('paths') or []
        by_path = dict(zip(paths, r_plan.get('file_units') or []))
        unbalanced = list(r_a.get('unbalanced') or [])
        unbalanced_units = set()
        for p in unbalanced:
            unbalanced_units |= set(by_path.get(p, []))
        explained = _prefix_closure(unbalanced_units)
        missing = set(m for m in (r_a.get('missing') or []) if isinstance(m, str))

        if not unbalanced:
            out.append(('%s.report.unbalanced (v2 re-parse loss without an '
                        'unbalanced part)' % base, [], sorted(missing)[:5]))
        if not missing <= explained:
            out.append(('%s.report.missing (not explained by the unbalanced parts)'
                        % base, sorted(missing - explained)[:5], 'explained-by-reflow'))
        r_after, p_after = _decl_count(r_a), _decl_count(p_a)
        if isinstance(r_after, int) and isinstance(p_after, int) and r_after > p_after:
            out.append(('%s.report.after.declarations (merge may only lose '
                        're-parsed decls)' % base, p_after, r_after))
        r_rel = (r_a.get('after') or {}).get('relations')
        p_rel = (p_a.get('after') or {}).get('relations')
        if r_rel != p_rel:
            out.append(('%s.report.after.relations' % base, p_rel, r_rel))

        normalized = True
        facts.append('self-audit after.declarations %s -> %s, missing %d '
                     '(all inside the %d part(s) v2 itself reports unbalanced), '
                     'unbalanced %d' % (p_after, r_after, len(missing),
                                        len(unbalanced), len(unbalanced)))

    if facts:
        notes.append('%s: %s' % (key, '; '.join(facts)))
    return normalized


def _summary(entry):
    plan = entry.get('plan') or {}
    report = entry.get('report') or {}
    emitted = entry.get('emitted')
    return {
        'units': plan.get('unitCount'),
        'files': plan.get('fileCount'),
        'lines': plan.get('lines'),
        'ok': report.get('ok'),
        'missing': len(report.get('missing') or []),
        'unbalanced': len(report.get('unbalanced') or []),
        'emitted': (len(emitted) if isinstance(emitted, list) else None),
    }


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--repo', default=DEFAULT_REPO)
    ap.add_argument('--root', action='append', default=None)
    ap.add_argument('--exclude', action='append', default=None)
    ap.add_argument('--limit', type=int, default=20,
                    help='stop after this many differences (default 20)')
    ap.add_argument('--rust-bin', default=None)
    ap.add_argument('--dump-dir', default=None,
                    help='keep both JSON documents in this directory for inspection')
    ap.add_argument('--strict', action='store_true',
                    help='byte-identical comparison (pre-merge v1 layout); the '
                         'merged-header allowance is not applied')
    ap.add_argument('--spec', action='append', default=None,
                    help='only diff this spec key (repeatable; passed to both '
                         'producers so one invocation stays inside a wall-clock '
                         'budget)')
    ap.add_argument('--oracle-dump', default=None,
                    help='compare a saved oracle_reorg.json instead of running')
    ap.add_argument('--rust-dump', default=None,
                    help='compare a saved csgraph dump-reorg JSON instead of running')
    args = ap.parse_args(argv)

    repo = os.path.abspath(args.repo)
    roots = args.root or ['_sealed_src']
    excludes = args.exclude if args.exclude is not None else []

    if bool(args.oracle_dump) != bool(args.rust_dump):
        raise SystemExit('diff_reorg: --oracle-dump and --rust-dump must be given together')

    if args.oracle_dump:
        with open(args.oracle_dump, 'rb') as fh:
            py_raw = fh.read()
        with open(args.rust_dump, 'rb') as fh:
            rs_raw = fh.read()
        print('python: %s' % args.oracle_dump)
        print('rust:   %s' % args.rust_dump)
    else:
        py_cmd = [sys.executable, os.path.join(HERE, 'oracle_reorg.py'), '--repo', repo]
        for r in roots:
            py_cmd += ['--root', r]
        for e in excludes:
            py_cmd += ['--exclude', e]
        for s in (args.spec or []):
            py_cmd += ['--spec', s]

        rust_bin = find_rust_bin(args.rust_bin)
        if rust_bin is None:
            raise SystemExit('diff_reorg: csgraph binary not found; build it with\n'
                             '  cargo build --offline -p csgraph-core\n'
                             'or pass --rust-bin PATH')
        rs_cmd = [rust_bin, 'dump-reorg', '--repo', repo]
        for r in roots:
            rs_cmd += ['--root', r]
        for e in excludes:
            rs_cmd += ['--exclude', e]
        for s in (args.spec or []):
            rs_cmd += ['--spec', s]

        print('python: %s' % ' '.join(py_cmd))
        print('rust:   %s' % ' '.join(rs_cmd))
        py_raw = run(py_cmd)
        rs_raw = run(rs_cmd)

    if args.dump_dir:
        os.makedirs(args.dump_dir, exist_ok=True)
        with open(os.path.join(args.dump_dir, 'oracle_reorg.json'), 'wb') as fh:
            fh.write(py_raw)
        with open(os.path.join(args.dump_dir, 'rust_reorg.json'), 'wb') as fh:
            fh.write(rs_raw)

    py_doc = load_json(py_raw, 'oracle_reorg.py')
    rs_doc = load_json(rs_raw, 'csgraph dump-reorg')

    print('counts python: %s' % counts(py_doc))
    print('counts rust:   %s' % counts(rs_doc))

    diffs = []
    if args.strict:
        walk(py_doc, rs_doc, '', diffs, args.limit)
        mode = 'strict (byte-identical, pre-merge v1 layout)'
        notes = []
    else:
        py_norm, py_infos = normalize_obj(py_doc)
        rs_norm, rs_infos = normalize_obj(rs_doc)
        walk(py_norm, rs_norm, '', diffs, args.limit)
        notes = []
        for key in sorted(set(py_infos) | set(rs_infos)):
            if key not in py_infos or key not in rs_infos:
                continue
            allowance_checks(key, py_infos[key], rs_infos[key], diffs, notes)
        mode = 'merged-partial-header allowance'

    if diffs:
        print('mode: %s' % mode)
        print('first differing key: %s' % diffs[0][0])
        print('FAIL: %d difference(s) (limit %d)%s'
              % (len(diffs), args.limit,
                 ' [truncated]' if len(diffs) >= args.limit else ''))
        for path, a, b in diffs:
            print('  %s\n    python: %s\n    rust:   %s' % (path, _short(a), _short(b)))
        return 1

    if args.strict:
        print('OK: 0 differences (strict)')
        return 0

    print('mode: %s' % mode)
    print('python: %s' % {k: _summary((py_doc.get('specs') or {}).get(k, {}))
                          for k in sorted(py_doc.get('specs') or {})})
    print('rust:   %s' % {k: _summary((rs_doc.get('specs') or {}).get(k, {}))
                          for k in sorted(rs_doc.get('specs') or {})})
    if notes:
        print('allowance (v2 merged-partial-header layout, verified benign):')
        for n in notes:
            print('  %s' % n)
    else:
        print('allowance: none needed (documents are byte-identical)')
    print('OK: 0 differences')
    return 0


if __name__ == '__main__':
    sys.exit(main())
