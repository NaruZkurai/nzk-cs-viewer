#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""convert.py — monolith -> one C# FILE PER MEMBER (folder mirrors nest).

Every real TYPE (class / struct / interface / enum) is opened as a folder under
the output root; every MEMBER of that type becomes its own leaf .cs file.
Each leaf re-declares ONLY its owning type chain as C# `partial` so the many
split files truly recompose the original type when compiled together.

KEY FIX vs old build:
  * Containers keep their REAL kind + `static`-ness from the source.  Only types
    that ORIGINALLY were `static class` (containers of statics such as Vars,
    Systems, NaNimate, Meshes, registry helpers) are emitted `static partial
    class`.  Data types (fields/props/instance methods, MonoBehaviour, Editor,
    structs — originally NON-static) are emitted `partial class` / `partial
    struct`.  No more blanket-`static`, so CS0708/CS0721 vanish.
  * Atomic types (enum, header-only) are written in ONE leaf nested under the
    enclosing (partial) type — never shadowed by a same-named folder, so
    CS0542 (member name == enclosing type) is gone.
  * Field/property/method leaves carry NO artificial wrapper class — they sit
    directly inside the owning `partial struct X { … }`.

Outcome: C#-valid (partials fan one type across many files) while still giving
one file per declaration inside per-type folders.

usage:
  python3 convert.py [in.cs] [outdir] [namespace]
  python3 convert.py [in.cs] -o OUTDIR [namespace]
  python3 convert.py -o OUTDIR [in.cs] [namespace]
  python3 convert.py -d [in.cs] [outdir]
  python3 convert.py -bc [in.cs] [outdir] [namespace]   # by class (one file per class)
  python3 convert.py -u [in.cs] [outdir] [namespace]    # update mode: append to existing

options:
  -o, --out DIR        output directory (overrides the positional outdir).
  -d, --delete, --deletedestfirst
                       if DIR already exists, DELETE it first so the result
                       is a clean regenerated tree (no stale leaves).  By
                       default the dir is NOT deleted (files overwrite).
  -u, --update         UPDATE mode: a top-level type whose name already exists on
                       disk is written as the NEXT partial segment file
                       (<Name>_pN.cs) instead of overwriting <Name>.cs, so the
                       same partial type accumulated across several convert.py
                       runs (one per .nzk input) folds into one C# type.
  -bc, --by-class      BY-CLASS mode: emit ONE dir + ONE .cs per TOP-LEVEL type
                       (class/struct/interface/enum).  The whole type slice
                       (header + every member + nested type) stays together
                       VERBATIM in that one file => no member shredding and no
                       brace surgery, so the stray-duplicate-`{`, missing-`;`,
                       and broken-`#if` leaves cannot occur.  A repeated partial
                       top-level type becomes <Name>.cs / <Name>_pN.cs segments.
  -ns, --name-space NS force this namespace on every leaf (beats a positional
                       namespace override).
  -h, --help           show this help.
notes:
  `in.cs` may also be an input directory whose every *.cs is converted.
  Optional namespace overrides the per-source namespace on every leaf.
  When -o/--out is given the positional outdir slot is skipped, so the
  trailing positional (if any) is the namespace override.
"""
import os, re, sys, shutil

BVCLASS = False   # True when -bc/--by-class given (one whole file per type).

SP = '  '

# namespace wrapper emitted on every leaf; derived from each source file at
# convert_one() time, not hard-coded (keep as the usual default).
LIVE_NS = 'Generated'
LIVE_NS = 'NZK.Core'

NS_GIVEN = False   # True when the user passed an explicit namespace (argv[3])

# ---------------------------------------------------------------------------
# lexer
# ---------------------------------------------------------------------------
def skip_ws_c(s, i, end):
    while i < end:
        c = s[i]
        if c == '/' and i + 1 < end and s[i + 1] == '/':
            j = s.find('\n', i, end); i = j if j >= 0 else end
        elif c == '/' and i + 1 < end and s[i + 1] == '*':
            j = s.find('*/', i + 2, end); i = (j + 2) if j >= 0 else end
        elif c in ' \t\r\n':
            i += 1
        else:
            break
    return i

def scan_lit(s, i, end):
    if s[i] == "'":
        i += 1
        while i < end:
            if s[i] == '\\': i += 2
            elif s[i] == "'": i += 1; break
            else: i += 1
        return i
    j = i; verb = False
    while j < end and s[j] in '@$':
        if s[j] == '@': verb = True
        j += 1
    if j >= end or s[j] != '"':
        return i + 1
    i = j + 1
    while i < end:
        e = s[i]
        if verb:
            if e == '"':
                if i + 1 < end and s[i + 1] == '"': i += 2; continue
                i += 1; break
        else:
            if e == '\\': i += 2; continue
            if e == '"': i += 1; break
        i += 1
    return i

def match_block(s, o, end):
    i = o + 1; d = 1
    while i < end:
        c = s[i]
        if c in '"\'': i = scan_lit(s, i, end); continue
        if c == '/' and i + 1 < end and s[i + 1] == '/':
            j = s.find('\n', i, end); i = j if j >= 0 else end; continue
        if c == '/' and i + 1 < end and s[i + 1] == '*':
            j = s.find('*/', i + 2, end); i = (j + 2) if j >= 0 else end; continue
        if c == '{': d += 1
        elif c == '}':
            d -= 1
            if d == 0: return i
        i += 1
    return end

def match_bracket(s, o, end):
    i = o + 1; d = 1
    while i < end:
        c = s[i]
        if c in '"\'': i = scan_lit(s, i, end); continue
        if c == '/' and i + 1 < end and s[i + 1] == '/':
            j = s.find('\n', i, end); i = j if j >= 0 else end; continue
        if c == '/' and i + 1 < end and s[i + 1] == '*':
            j = s.find('*/', i + 2, end); i = (j + 2) if j >= 0 else end; continue
        if c == '[': d += 1
        elif c == ']':
            d -= 1
            if d == 0: return i
        i += 1
    return end

PPRE = re.compile(r'(?m)^[ \t]*#\s*(if|ifdef|ifndef|elif|else|endif)\b([^\n]*)')

def build_pp(s):
    st, out = [], {}
    for mm in PPRE.finditer(s):
        off, k = mm.start(), mm.group(1)
        raw = mm.group(0).strip()
        if k.startswith('if'):
            st.append((off, raw))
        elif k == 'endif' and st:
            o, t = st.pop()
            out[o] = (t, off)
    return out

def wrapping_guards(pp, a, b):
    return [g for o, (g, e) in pp.items() if o < a and e > b]

# ---------------------------------------------------------------------------
# type-header parsing: (kind, name, modifiersPrefix)
# ---------------------------------------------------------------------------
TYPE_RE = re.compile(r'\b(class|struct|interface|enum)\b')

def parse_head(head):
    mm = TYPE_RE.search(head)
    if not mm:
        return None, None, None
    pre = head[:mm.start()].strip()
    kind = mm.group(1)
    rest = head[mm.end():]
    nm = re.search(r'([A-Za-z_]\w*)', rest)
    return kind, (nm.group(1) if nm else 'Member'), pre

TOK_SPLIT = re.compile(r'[^\w]')

# --- base-class / interface extraction -----------------------------------
# When a type header carries `class X : Base, IBar`, the per-member partial leaf
# MUST repeat that declaration (a partial without `: Base` loses the override
# target -> "CS0115 no suitable method found to override" for Editor methods).
# base_of returns the text after the FIRST top-level ':' (generics/wheres aware)
# or '' if none.  Each every type node then carries it as 4th chain element.
def base_of(head):
    depth = 0
    at = -1
    i = 0
    n = len(head)
    while i < n:
        c = head[i]
        if c == '<': depth += 1
        elif c == '>': depth = max(0, depth - 1)
        elif c == ':' and depth == 0:
            at = i
            break
        i += 1
    if at < 0:
        return ''
    tail = head[at + 1:]
    # cut at any `where` constraint (base list cannot legally contain 'where')
    m = re.search(r'\bwhere\b', tail)
    if m:
        tail = tail[:m.start()]
    return ' : ' + tail.strip()

def prefix_has(pre, word):
    toks = [t for t in TOK_SPLIT.split(pre) if t]
    if word in toks:
        return True
    # 'static partial', 'public static partial class' handled; but modifiers may
    # come as one word token only when separated by spaces. fine.
    return word in pre.split()

def _scan_member_name(head):
    i = 0; end = len(head); pending = None; cand = None; depth = 0
    while i < end:
        c = head[i]
        if c.isalnum() or c == '_':
            j = i
            while j < end and (head[j].isalnum() or head[j] == '_'): j += 1
            pending = head[i:j]; i = j; continue
        if c.isspace(): i += 1; continue
        if c == '<':
            d = 1; i += 1
            while i < end and d:
                if head[i] == '<': d += 1
                elif head[i] == '>': d -= 1
                i += 1
            continue
        if c == '(':
            if depth == 0 and pending: cand = pending
            depth += 1; pending = None; i += 1; continue
        if c == ')': depth -= 1; i += 1; continue
        pending = None; i += 1
    return cand

def mem_name(head):
    before = head.split('=>')[0]
    nm = _scan_member_name(before)
    if nm: return nm
    if '=>' in head:
        mm = list(re.finditer(r'([A-Za-z_]\w*)\s*=>', head))
        if mm: return mm[-1].group(1)
    eq = re.findall(r'(?<=[\s;,(])([A-Za-z_]\w*)\s*=(?!=|>|<|~)', head)
    if eq: return eq[-1]
    mm = re.search(r'([A-Za-z_]\w*)\s*=\s*(?:\{|[A-Za-z_])', head)
    if mm: return mm.group(1)
    mm = re.findall(r'\b([A-Za-z_]\w*)\b', head)
    if mm: return mm[-1]
    return 'member'

def sf(n):
    return re.sub(r'[^\w]', '_', n) or 'member'

def braced_token_pos(s, i, end):
    pos = i
    while pos < end:
        c = s[pos]
        if c in '"\'': pos = scan_lit(s, pos, end); continue
        if c == '/' and pos + 1 < end and s[pos + 1] == '/':
            j = s.find('\n', pos, end); pos = j if j >= 0 else end; continue
        if c == '/' and pos + 1 < end and s[pos + 1] == '*':
            j = s.find('*/', pos + 2, end); pos = (j + 2) if j >= 0 else end; continue
        if c == '[': pos = match_bracket(s, pos, end) + 1; continue
        if c in '{;': return pos
        pos += 1
    return end

def _arrow_end(s, pos, end):
    # EDGECASE-d: expression/arrow-bodied members `T N(p) => <object-init>;` end
    # at the very FIRST top-level `;` after `=>`.  Object/collection/array
    # initializers `new X { ... }` all close before that `;`; any `;` inside a
    # nested lambda sits at depth>=1, so first top-level `;` is the true end.
    # Scanning ONLY for that `;` guarantees the trailing `;` is always kept
    # (old bug dropped it -> "CS1002 ; expected" on every arrow+initializer).
    dep = 0
    while pos < end:
        c = s[pos]
        if c in '"\'': pos = scan_lit(s, pos, end); continue
        if c == '/' and pos + 1 < end and s[pos + 1] == '/':
            j = s.find('\n', pos, end); pos = j if j >= 0 else end; continue
        if c == '/' and pos + 1 < end and s[pos + 1] == '*':
            j = s.find('*/', pos + 2, end); pos = (j + 2) if j >= 0 else end; continue
        if c in '({': dep += 1
        elif c in ')}': dep = max(0, dep - 1)
        elif c == ';':
            if dep == 0: return pos + 1
        pos += 1
    return end

def member_body_end(s, i, end):
    # EDGECASE-a (real body): a depth-0 `{` that is NOT the RHS of a `=` is the
    #   opening of a METHOD / CTOR / PROPERTY / INDEXER body. We return right
    #   past its matched closing `}` (whole body preserved, statements never
    #   spill out as bare members -> fixes CS1519/CS1513/CS8803 + truncation).
    # EDGECASE-b (initializer): a depth-0 `{` reached AFTER a top-level `=` is an
    #   object/collection/array initializer `… = new X {…}` or `… = {…}`; that
    #   is *not* a body, so keep scanning to the trailing `;` (fixes "CS1002 ;").
    # EDGECASE-c (auto-property): `int P { get; set; }` closes its body then a
    #   `;` follows -> include the trailing `;` so the leaf compiles.
    # EDGECASE-d (arrow): `R N(p) => <expr>;` handed to _arrow_end (guarantees ;).
    # EDGECASE-e (string of method w/ default args): `=` inside `(T a = v)` only
    #   counts once a `(` has opened AND closed at depth 0 (assign latched on the
    #   FIRST unbalanced `=` that is not inside an already-closed call).  We keep
    #   it simple: treat any depth-0 `{` not preceded by `=`-continuation as body.
    pos = i; on_assign = False
    while pos < end:
        c = s[pos]
        if c in '"\'': pos = scan_lit(s, pos, end); continue
        if c == '/' and pos + 1 < end and s[pos + 1] == '/':
            j = s.find('\n', pos, end); pos = j if j >= 0 else end; continue
        if c == '/' and pos + 1 < end and s[pos + 1] == '*':
            j = s.find('*/', pos + 2, end); pos = (j + 2) if j >= 0 else end; continue
        if c == '{':
            if on_assign:
                # initializer `= {` (or `= new … {`) -> RHS expression; skip its
                # balanced run, keep scanning until the statement `;`.
                pos = match_block(s, pos, end) + 1
                continue
            # real member body / property body / indexer body
            cl = match_block(s, pos, end)
            nxt = cl + 1
            # auto-property `{ get; set; }` -> swallow the trailing `;`
            q = skip_ws_c(s, nxt, end)
            if q < end and s[q] == ';':
                return q + 1
            return nxt
        elif c == '}':
            # shouldn't reach here at member level
            return pos + 1
        elif c == ';':
            return pos + 1
        elif c == '(':
            # Skip the entire balanced `( … )` group.  A `=` inside a parameter
            # list (default-arg) or inside a call's arguments is NOT a field /
            # property initializer, so clear `on_assign` after the group closes.
            bal = 1; k = pos + 1
            while k < end and bal:
                e2 = s[k]
                if e2 in '"\'': k = scan_lit(s, k, end); continue
                if e2 == '(': bal += 1
                elif e2 == ')': bal -= 1
                k += 1
            # any `=` we crossed was inside a call/param list, so it is not an
            # initializer -> reset.
            pos = k
            on_assign = False
            continue
        elif c == ')':
            pos += 1
            continue
        elif c == '=':
            if pos + 1 < end and s[pos + 1] == '>':
                # expression-bodied member -> guarantee its trailing `;`
                return _arrow_end(s, pos + 2, end)
            if pos + 1 < end and s[pos + 1] in '=<>!':
                pos += 1
                continue
            on_assign = True
            # ignore downstream '=' until the statement ends
            pos += 1
            continue
        elif c == ',' or c == '.' or c == '+' or c == '-':
            # punctuation that would only be continuations of an init RHS
            pass
        pos += 1
    return end

def is_pp_line(s, tl, b):
    p = tl
    return (p < b) and (s[p] == '#')

def leaf_txt_is_empty(t):
    t = re.sub(r'(?m)//[^\n]*', '', t)
    t = re.sub(r'(?s)/\*.*?\*/', '', t)
    return t.strip() in ('', ';')

def leaf_starts_decl(t):
    t = t.lstrip()
    if not t: return False
    return t[0] not in ');},.'

# ---------------------------------------------------------------------------
# writer — chainmeta: [(name, kind, is_static), …] outermost..innermost
# ---------------------------------------------------------------------------
wrote = {}

# EDGECASE-e: pure preprocessor lines that leak into a *member* body from
#   file/namespace/class scope (their matching open lives in a sibling file).
#   Sanitise so per-leaf guards stay balanced: any standalone trailing
#   `#else`/`#elif`/`#endif` that has no open inside THIS code is dropped;
#   `#region/#endregion` carried entire are folded to nothing.
def san_pp(code):
    import re as _re
    out = []
    stack = 0
    for ln in code.split('\n'):
        m = _re.match(r'^[ \t]*(#[ \t]*(?:if|ifdef|ifndef)\b)', ln)
        e = _re.match(r'^[ \t]*(#[ \t]*endif\b)', ln)
        b = _re.match(r'^[ \t]*(#[ \t]*(?:else|elif)\b)', ln)
        rg = _re.match(r'^[ \t]*#region\b', ln)
        re2 = _re.match(r'^[ \t]*#endregion\b', ln)
        if rg or re2:
            continue                      # #region never wraps our sealed blocks
        if e:
            if stack > 0:
                stack -= 1
                out.append(ln)
            # else: unmatched orphan #endif (opened in sibling file) - drop
            continue
        if b:
            if stack > 0:
                out.append(ln)
            continue                      # orphan #else/#elif w/o open -> drop
        if m:
            stack += 1
            out.append(ln)
            continue
        out.append(ln)
    return '\n'.join(out)

def _compact(ns, heads, body):
    MAX = 200
    def one(l):
        return re.sub(r'\s+', ' ', l).strip()
    PP_RE = re.compile(r'^\s*#')
    blines = [one(x) for x in body.split('\n') if x.strip()]
    # a row that is (or ends in) a bare preprocessing directive must NOT be
    # followed on the SAME physical line by the closing `}` braces: `#endif } }`
    # is CS1025 ("single-line comment or end-of-line expected") and the glued
    # `}` get swallowed as directive text so the leaf ends under-closed (CS1513).
    def is_pprow(l):
        return bool(PP_RE.match(l))
    if len(heads) == 1 and len(blines) <= 1 and not (blines and is_pprow(blines[0])):
        stmt = blines[0] if blines else ''
        tail = ('{ ' + (stmt + ' ' if stmt else '') + '}  }').rstrip()
        return 'namespace ' + ns + '{  ' + heads[0] + '\n' + tail
    # generic branch: class body-open braces cuddle at the END of each header row;
    # member lines drop straight inside (no phantom leading '{ '); closers close
    # exactly the opens WE added (namespace + one per class header).
    rows = ['namespace ' + ns + '{  ' + heads[0] + '{']
    for h in heads[1:]:
        rows.append(h + '{')
    rows.extend(blines)
    nclose = 1 + len(heads)          # namespace + each header class
    tail = '}'
    for i in range(1, nclose):
        tail += ('  }' if i == nclose - 1 else ' }')
    if rows:
        last = rows[-1]
        merge = not is_pprow(last) and len(last) + 1 + len(tail) <= MAX and bool(last)
        if merge:
            rows[-1] = last + ' ' + tail
        else:
            rows.append(tail)
    else:
        rows = [tail]
    return '\n'.join(rows)


def emit_file(outroot, chainmeta, guards, code, leafname=None):
    """leafname: desired .cs base for THIS leaf (member/type's own name).
    None -> fall back to innermost type node (type-level partial leaf).
    folder is ALWAYS the enclosing type chain (nm_nodes), unchanged."""
    nm_nodes = [n for (n, k, st, *_) in chainmeta]
    folder = os.path.join(outroot, *nm_nodes)
    os.makedirs(folder, exist_ok=True)
    name0 = leafname if leafname else chainmeta[-1][0]
    n = 1; base = name0; keypath = os.path.join(folder, base + '.cs')
    while keypath in wrote:
        n += 1; base = '%s_%d' % (name0, n); keypath = os.path.join(folder, base + '.cs')
    wrote[keypath] = True
    body = san_pp(code)
    # If the body still holds unclosed `#if` opens (their matching #endif sat in
    # a sibling top-level type's leaf that the recursive by-class split peeled
    # off), close them HERE so the emitted leaf is standalone guard-balanced
    # (avoids CS1027 orphan-#if).  `san_pp` already dropped orphan #endif/#else.
    _open = _body_pp_open(body)
    while _open > 0:
        body += '\n#endif'
        _open -= 1
    heads = []
    for idx, nd in enumerate(chainmeta):
        name, kind, is_st = nd[0], nd[1], nd[2]
        bs = nd[3] if len(nd) > 3 else ''
        is_last = (idx == len(chainmeta) - 1)
        mods = 'public '
        if is_st and kind == 'class':
            mods += 'static '
        # an enum cannot be `partial`; when it is the innermost (leaf) node only
        # the enclosing partial classes carry `partial`.  (Legacy never puts an
        # enum last, so this only fires for recursive by-class enum leaves.)
        if not (is_last and kind == 'enum'):
            mods += 'partial '
        mods += kind + ' ' + re.sub(r'\W', '_', name)
        if bs:
            mods += bs
        heads.append(mods)
    text = _compact(LIVE_NS, heads, body)
    L = list(guards) + [text] + ['#endif' for _ in guards]
    with open(keypath, 'w', encoding='utf-8') as fh:
        fh.write('\n'.join(l for l in L if l != '') + '\n')

def emit_bare(outroot, folder, guard_cond, code, stats):
    """atomic type (enum / header-only) with no enclosing partial folding;
       folder = tuple naming the output subfolder (== type name)."""
    import os as _os
    fo = _os.path.join(*folder)
    d = _os.path.join(outroot, fo)
    _os.makedirs(d, exist_ok=True)
    base = folder[-1]
    n = 1; key = _os.path.join(d, base + '.cs')
    while key in wrote:
        n += 1; key = _os.path.join(d, '%s_%d.cs' % (base, n))
    wrote[key] = True
    L = []
    for g in guard_cond: L.append(g)
    L.append('namespace ' + LIVE_NS)
    L.append('{')
    L.append(SP + code)
    L.append('}')
    for g in guard_cond: L.append('#endif')
    with open(key, 'w', encoding='utf-8') as fh:
        fh.write('\n'.join(L) + '\n')

def _body_pp_open(code):
    """net unmatched `#if/ifdef/ifndef` at end of a body slice (opened minus
       closed).  >0  => inner guards whose #endif fall just after the slice."""
    n = 0
    for ln in code.split('\n'):
        m = re.match(r'^[ \t]*#[ \t]*(if|ifdef|ifndef)\b', ln)
        e = re.match(r'^[ \t]*#[ \t]*endif\b', ln)
        if m: n += 1
        elif e: n = max(0, n - 1)
    return n

def _absorb_trailing_endifs(src, s, cl, end):
    """If a whole-type slice ends (at `cl`) with interior `#if`s still open,
       absorb the immediately-following `#endif` lines (they close guards that
       the source author wrote AFTER the struct's `}`) so the emitted leaf is
       guard-balanced (no orphan `#if` -> no CS1027)."""
    open_need = _body_pp_open(src[s:cl + 1])
    if open_need <= 0:
        return cl
    pos = cl + 1
    cnt = 0
    # absorb contiguous trailing #endif / blanks while we owe closers
    while pos < end and cnt < open_need:
        q = skip_ws_c(src, pos, end)
        if q >= end or src[q] != '#':
            break
        if re.match(r'^[ \t]*#[ \t]*endif\b', src[q:]):
            nxt = src.find('\n', q, end)
            pos = (nxt + 1) if nxt >= 0 else end
            cnt += 1
        else:
            break
    # only extend past the code we can actually pair
    return pos - 1 if cnt > 0 else cl

def emit_byclass(outroot, kind, pre, head, name, body, guards, stats):
    """-bc (by class): ONE dir + ONE .cs per TOP-LEVEL type, WHOLE type slice
       (header + full body incl. every nested type and member) kept together
       VERBATIM -> brace balance is guaranteed and no inner parsing is needed.

       Repeated/partial segments of the same top-level name (a later input, or
       a -u append across processes) become SIBLING segment files in the SAME
       dir: `name.cs`, `name_p1.cs`, `name_p2.cs`, ...  Each is declared
       `partial` under the same namespace so C# folds them into ONE type.
       RC-6: a class merge segment (idx>=1) is forced `static partial` so all
       segments of the partial agree (toolkit's non-static Core becomes static).
       Enum/atomic types are written non-partial."""
    import os as _os
    d = _os.path.join(outroot, name)
    _os.makedirs(d, exist_ok=True)
    # pick next free segment index:
    #   -u  -> append after what is ALREADY ON DISK (survives across processes).
    #   else-> fresh authoritative emit -> name.cs (in-process repeats -> _pN).
    if UPDATE_MODE:
        idx = 0
        if _os.path.exists(_os.path.join(d, name + '.cs')):
            idx = 1
        while _os.path.exists(_os.path.join(d, '%s_p%d.cs' % (name, idx))):
            idx += 1
    else:
        idx = 0
        if _os.path.join(d, name + '.cs') in wrote:
            idx = 1
            while _os.path.join(d, '%s_p%d.cs' % (name, idx)) in wrote:
                idx += 1
    fname = (name + '.cs') if idx == 0 else ('%s_p%d.cs' % (name, idx))
    key = _os.path.join(d, fname)
    wrote[key] = True
    if kind == 'enum':
        headln = 'public enum ' + name + base_of(head)
    else:
        want_static = (kind == 'class') and (prefix_has(pre, 'static') or idx >= 1)
        headln = ('public ' + ('static ' if want_static else '')
                  + 'partial ' + kind + ' ' + name + base_of(head))
    L = [g for g in guards]
    L.append('namespace ' + LIVE_NS)
    L.append('{')
    # open the type's OWN body (it was stripped off the slice), then close it
    # after the verbatim body, then close the namespace.
    L.append(SP + headln + ' {')
    if name == 'Core':
        nested = re.match(r'\s*public\s+static\s+partial\s+class\s+Core\s*\{', body)
        if nested:
            openbrace = nested.end() - 1
            closebrace = match_block(body, openbrace, len(body))
            if closebrace < len(body):
                body = body[openbrace + 1:closebrace]
    L.extend(_indent(body))
    L.append('}')
    L.append('}')
    L += ['#endif' for _ in guards]
    with open(key, 'w', encoding='utf-8') as fh:
        fh.write('\n'.join(x for x in L if x != '') + '\n')
    stats[0] += 1

def _indent(code):
    # Preserve the source's internal relative indentation unchanged and shift the
    # whole slice down by ONE level (SP) so it sits cleanly bodily inside the
    # namespace.  No reflow of inner depth -> class/member/bracket relations are
    # byte-identical to the (already valid) monolith, so balance is guaranteed.
    return [SP + ln if ln.strip() else ln for ln in code.split('\n')]

# ---------------------------------------------------------------------------
# -bc DEEP SPLIT (current -bc behaviour): mirror nested types into a directory
# tree, ONE canonical leaf per type holding its own non-type members, nested
# children peeled into their own dirs recursively.  Cross-source partial
# segments (Core.cs / Core_p1.cs / Core_p2.cs) fold into one type by C#.
#
# Design:
#   * Every TYPE gets its own directory = mirror of its nesting path, and its
#     canonical own-file <Name>.cs (or <Name>_pN.cs under -u) in that dir.
#   * LEAF type (no nested children): its whole declaration (header + body) is
#     written VERBATIM - no header surgery, byte-balanced by construction.
#   * CONTAINER type (has nested children): `partial` is injected into its
#     header (legal, additive; lets the type be re-opened across files) and the
#     body is split - the container's own non-type members (the body minus the
#     child blocks, i.e. all the gaps BETWEEN its nested children, which may be
#     interleaved) go into its canonical leaf; each child recurses into a child
#     dir.  Every ancestor on the path is re-declared as a CLEAN scaffolding
#     partial (no attributes / no base / public) so only the canonical leaf
#     carries the original attributes + `: Base` clause (avoids duplicate
#     attributes CS0579 and conflicting bases CS0263).  The top-level type's
#     static-ness is forced `static` for later -u merge segments (RC-6) and
#     propagated down the whole chain so all leaves agree.
#   * enum / header-only types are atomic -> leaf (verbatim).
#   * Same-file repeated partial containers (e.g. several `partial Systems`
#     blocks in one source) each become their own canonical leaf (<Name>.cs,
#     <Name>_p1.cs, ...) and fold by C# partial.
# ---------------------------------------------------------------------------

def _ensure_mods(head, kind, want_static):
    """Return <head> carrying `partial` (and `static` when want_static) in the
       canonical order `... [access] static partial <kind> ...`, preserving any
       attribute groups and the base clause already in <head>.  Lets a source
       `static class Vars` be split as `static partial class Vars`, and forces a
       later -u merge segment of a class to `static` (RC-6) so every partial
       part of the folded type agrees."""
    if re.search(r'\bpartial\b', head) and (not want_static or re.search(r'\bstatic\b', head)):
        return head  # modifiers already correct
    head2 = re.sub(r'\b(?:partial|static)\b', '', head)
    head2 = re.sub(r'[ \t]+', ' ', head2)
    ins = []
    if want_static:
        ins.append('static')
    ins.append('partial')
    m = re.search(r'\b(%s)\b' % kind, head2)
    if not m:
        return head
    return head2[:m.start()] + ' '.join(ins) + ' ' + head2[m.start():]

def _scaffold_head(kind, name, is_static):
    s = 'public '
    if is_static and kind == 'class':
        s += 'static '
    if kind != 'enum':
        s += 'partial '
    s += kind + ' ' + name
    return s

def _next_seg(dirpath, name):
    """Pick the next free canonical leaf filename for <name> in <dirpath>.
       -u  -> append after what is ALREADY ON DISK (survives across processes).
       else-> fresh authoritative emit -> name.cs (in-process repeats -> _pN)."""
    if UPDATE_MODE:
        idx = 0
        if os.path.exists(os.path.join(dirpath, name + '.cs')):
            idx = 1
        while os.path.exists(os.path.join(dirpath, '%s_p%d.cs' % (name, idx))):
            idx += 1
    else:
        idx = 0
        if os.path.join(dirpath, name + '.cs') in wrote:
            idx = 1
            while os.path.join(dirpath, '%s_p%d.cs' % (name, idx)) in wrote:
                idx += 1
    return (name + '.cs') if idx == 0 else ('%s_p%d.cs' % (name, idx))

def _bc_deep_leaf(dirpath, ns, scaffold_nodes, canonical_head, body_text,
                  guards, stats, leaf_name):
    """Write one canonical leaf.  scaffold_nodes = (kind,name,is_static) for the
       ancestors to re-open (outermost first), NOT including the canonical type.
       canonical_head None => body_text is a WHOLE VERBATIM declaration slice
       (already carries its own balanced braces); else canonical_head is the
       container header and body_text its own non-type members."""
    os.makedirs(dirpath, exist_ok=True)
    key = os.path.join(dirpath, _next_seg(dirpath, leaf_name))
    wrote[key] = True
    L = [g for g in guards]
    L.append('namespace ' + ns)
    L.append('{')
    for (k, nm, is_st) in scaffold_nodes:
        L.append(_scaffold_head(k, nm, is_st) + ' {')
    if canonical_head is not None:
        L.append(canonical_head + ' {')
        L.extend(body_text.split('\n'))
    else:
        L.extend(body_text.split('\n'))
    nclose = 1 + len(scaffold_nodes) + (1 if canonical_head is not None else 0)
    for _ in range(nclose):
        L.append('}')
    L += ['#endif' for _ in guards]
    with open(key, 'w', encoding='utf-8') as fh:
        fh.write('\n'.join(x for x in L if x != '') + '\n')
    stats[0] += 1

def _bc_deep_emit(dirpath, ns, src, span, scaffold_nodes, pp, stats,
                  forced_static):
    """Process one type declaration whose span=(s,he,cl) [header start, `{`,
       matching `}`].  dirpath = this type's OWN mirror directory (its canonical
       leaf lives here; children get child subdirs)."""
    s, he, cl = span
    head = src[s:he].strip()
    kind, name, pre = parse_head(head)
    nm = name.split('`')[0]
    is_static = (kind == 'class') and (prefix_has(pre, 'static') or forced_static)

    # ---- scan the body interior: collect direct child spans + own-member gaps
    children = []      # (cs, che, ccl, kind, name, pre, has_brace)
    gap_parts = []
    own_start = he + 1
    i = he + 1
    while i < cl:
        p = skip_ws_c(src, i, cl)
        if p >= cl:
            break
        if is_pp_line(src, p, cl):
            j = src.find('\n', p, cl)
            i = (j + 1) if j >= 0 else cl
            continue
        he2 = braced_token_pos(src, p, cl)
        h2 = src[p:he2].strip()
        if leaf_txt_is_empty(h2) or h2 == '':
            i = he2 + 1
            continue
        if h2.startswith('namespace '):
            j = src.find('\n', he2, cl)
            i = (j + 1) if j >= 0 else cl
            continue
        k2, n2, pr2 = parse_head(h2)
        has2 = he2 < cl and src[he2] == '{'
        if k2 in ('class', 'struct', 'interface', 'enum'):
            c2 = match_block(src, he2, cl) if has2 else he2
            gap_parts.append(src[own_start:p])
            children.append((p, he2, c2, k2, n2, pr2, has2))
            own_start = (c2 + 1) if has2 else (he2 + 1)
            i = own_start
            continue
        # non-type member statement: skip its balanced span (stays in the gap)
        i = member_body_end(src, p, cl)
    gap_parts.append(src[own_start:cl])
    is_container = bool(children)

    if not is_container:
        # LEAF: whole declaration verbatim (byte-exact, balanced by construction).
        verbatim = src[s:cl + 1]
        openn = _body_pp_open(verbatim)
        while openn > 0:
            verbatim += '\n#endif'
            openn -= 1
        guards = wrapping_guards(pp, s, cl)
        _bc_deep_leaf(dirpath, ns, scaffold_nodes, None, verbatim, guards,
                      stats, nm)
        return

    # CONTAINER: canonical own-leaf (partial/static-injected head + own members)
    canonical_head = _ensure_mods(head, kind, is_static)
    own_text = san_pp(''.join(gap_parts))
    openn = _body_pp_open(own_text)
    while openn > 0:
        own_text += '\n#endif'
        openn -= 1
    guards = wrapping_guards(pp, s, cl)
    _bc_deep_leaf(dirpath, ns, scaffold_nodes, canonical_head, own_text,
                  guards, stats, nm)
    # ... then recurse into each child (its own dir under this one).
    for (cs, che, ccl, k2, n2, pr2, has2) in children:
        cnm = n2.split('`')[0]
        child_dir = os.path.join(dirpath, cnm)
        _bc_deep_emit(child_dir, ns, src, (cs, che, ccl),
                      scaffold_nodes + [(kind, nm, is_static)], pp, stats,
                      False)

def _bc_deep_run(src, a, b, outroot, pp, stats):
    """-bc driver: iterate the namespace body's TOP-LEVEL types and deep-split
       each into a mirrored dir tree under /out/<TypeName>/."""
    i = a
    while i < b:
        s = skip_ws_c(src, i, b)
        if s >= b:
            break
        if is_pp_line(src, s, b):
            j = src.find('\n', s, b)
            i = (j + 1) if j >= 0 else b
            continue
        he = braced_token_pos(src, s, b)
        head = src[s:he].strip()
        if leaf_txt_is_empty(head) or head == '':
            i = he + 1
            continue
        if head.startswith('namespace '):
            if he < b and src[he] == '{':
                i = match_block(src, he, b) + 1
            else:
                j = src.find('\n', he, b)
                i = (j + 1) if j >= 0 else b
            continue
        kind, name, pre = parse_head(head)
        has_brace = he < b and src[he] == '{'
        if kind in ('class', 'struct', 'interface', 'enum'):
            nm = name.split('`')[0]
            if has_brace:
                cl = match_block(src, he, b)
                forced_static = ((kind == 'class')
                                 and (prefix_has(pre, 'static')
                                      or _top_seg_idx(outroot, nm) >= 1))
                _bc_deep_emit(os.path.join(outroot, nm), LIVE_NS, src,
                              (s, he, cl), [], pp, stats, forced_static)
                i = cl + 1
            else:
                # header-only top-level type (rare) -> verbatim leaf
                _bc_deep_leaf(os.path.join(outroot, nm), LIVE_NS, [], None,
                              src[s:he + 1], wrapping_guards(pp, s, he),
                              stats, nm)
                i = he + 1
            continue
        i = member_body_end(src, s, b)

def _top_seg_idx(outroot, name):
    """How many partial segments of top-level <name> already exist on disk
       (only meaningful under -u across processes).  0 if <name>.cs absent."""
    if not UPDATE_MODE:
        return 0
    d = os.path.join(outroot, name)
    if not os.path.exists(os.path.join(d, name + '.cs')):
        return 0
    idx = 1
    while os.path.exists(os.path.join(d, '%s_p%d.cs' % (name, idx))):
        idx += 1
    return idx

def walk_byclass(src, a, b, outroot, pp, stats):
    """-bc (by class): emit ONE WHOLE-SLICE .cs per TOP-LEVEL type under
       /out/<TypeName>/.  The whole original type body (all nested types and
       members) is copied VERBATIM into a single leaf, so braces are never
       duplicated and every file is byte-balanced.

       Compact-brace safe (RC-4): only the type's OWN opening `{` is located
       (braced_token_pos) and its matching close (match_block); no inner parsing
       is needed, so `namespace NZK{ public static partial class Core{ ... } }`
       splits cleanly into Core/Core.cs.  A top-level type seen again (another
       input, or a -u append) becomes a new `_pN` partial-segment file folded
       into the same type (see emit_byclass).
       Nested types stay INSIDE their owning top-level file (requirement's
       "under Core" branch) rather than being peeled into sibling dirs."""
    i = a
    while i < b:
        s = skip_ws_c(src, i, b)
        if s >= b: break
        if is_pp_line(src, s, b):
            j = src.find('\n', s, b); i = (j + 1) if j >= 0 else b; continue
        he = braced_token_pos(src, s, b)
        head = src[s:he].strip()
        if leaf_txt_is_empty(head) or head == '':
            i = he + 1; continue
        kind, name, pre = parse_head(head)
        if head.startswith('namespace '):
            # nested namespace: skip its whole block intact (rare at top level)
            if he < b and src[he] == '{':
                cl = match_block(src, he, b); i = cl + 1
            else:
                j = src.find('\n', he, b); i = (j + 1) if j >= 0 else b
            continue
        has_brace = he < b and src[he] == '{'
        if kind in ('class', 'struct', 'interface', 'enum'):
            nm = name.split('`')[0]
            if has_brace:
                cl = match_block(src, he, b)
                cl = _absorb_trailing_endifs(src, s, cl, b)
                body = src[he + 1:cl]
                emit_byclass(outroot, kind, pre, head, nm, body,
                             wrapping_guards(pp, s, cl), stats)
                i = cl + 1
            else:
                # header-only type with no body cannot be re-opened safely;
                # skip (does not occur in the NZK sources).
                i = he + 1
            continue
        # stray top-level statement (not a type): skip intact
        i = member_body_end(src, s, b)

def _bc_body(outroot, src, a, b, chain, pp, stats):
    """Recursively split ONE type body (span a..b) for by-class output.

    `chain` carries the FULL nested path INCLUDING this type as its last node
    (outermost..innermost).  Each leaf file re-declares that whole partial chain
    and holds ONLY this type's own contiguous non-class members — nested child
    types are peeled out into their own deeper dir + file, recursively.

    Output folders mirror the type chain (emit_file folder = join(*chain-names)),
    so:
      VRCAD{ Set{..} Get{..} testing() }  ->
        /VRCAD/VRCAD.cs            {partial VRCAD{ testing() } }
        /VRCAD/Set/Set.cs          {partial VRCAD{ partial Set{..} } }
        /VRCAD/Get/Get.cs          {partial VRCAD{ partial Get{..} } }
    Nested enum -> sibling leaf under its owner (ancestors partial, enum atomic).
    """
    i = a
    run_a = None     # start source offset of this type's current own-member run
    run_b = None     # end (exclusive) offset of that run
    last_mem = None  # end offset of the last real member (guard span end)

    def flush():
        nonlocal run_a, run_b, last_mem
        if run_a is None:
            return
        txt = src[run_a:run_b]
        if not leaf_txt_is_empty(txt):
            stats[0] += 1
            gb = last_mem if last_mem is not None else run_b
            guards = wrapping_guards(pp, run_a, gb)
            emit_file(outroot, chain, guards, txt, leafname=chain[-1][0])
        run_a = run_b = last_mem = None

    while i < b:
        s = skip_ws_c(src, i, b)
        if s >= b:
            break
        if is_pp_line(src, s, b):
            j = src.find('\n', s, b)
            e = (j + 1) if j >= 0 else b
            if run_a is None:
                run_a = s
            run_b = e
            i = e
            continue
        he = braced_token_pos(src, s, b)
        head = src[s:he].strip()
        if leaf_txt_is_empty(head) or head == '':
            i = he + 1
            continue
        kind, name, pre = parse_head(head)
        if head.startswith('namespace '):
            j = src.find('\n', he, b); i = (j + 1) if j >= 0 else b; continue
        has_brace = he < b and src[he] == '{'
        if kind in ('class', 'struct', 'interface'):
            flush()          # nested TYPE boundary -> stop THIS type's own run
            if has_brace:
                cl = match_block(src, he, b)
                is_st = prefix_has(pre, 'static')
                sub = (name.split('`')[0], kind, is_st, base_of(head))
                _bc_body(outroot, src, he + 1, cl, chain + (sub,), pp, stats)
                i = cl + 1
            else:
                code = src[s:he + 1]
                if not leaf_txt_is_empty(code):
                    is_st = prefix_has(pre, 'static')
                    sub = (name.split('`')[0], kind, is_st, base_of(head))
                    stats[0] += 1
                    emit_file(outroot, chain + (sub,),
                              wrapping_guards(pp, s, he), code, leafname=sub[0])
                i = he + 1
            continue
        if kind == 'enum':
            flush()
            is_st = prefix_has(pre, 'static')
            sub = (name.split('`')[0], kind, is_st, base_of(head))
            if has_brace:
                cl = match_block(src, he, b)
                # leaf body = the enum's INNER members only; the `public enum <N>`
                # head is re-declared (atomic, no `partial`) by emit_file.
                code = src[he + 1:cl]
                if not leaf_txt_is_empty(code):
                    stats[0] += 1
                    emit_file(outroot, chain + (sub,),
                              wrapping_guards(pp, s, cl), code, leafname=sub[0])
                i = cl + 1
            else:
                # header-only enum declaration (no braces) -> atomic own leaf
                code = src[s:he + 1]
                if not leaf_txt_is_empty(code):
                    stats[0] += 1
                    emit_file(outroot, chain + (sub,),
                              wrapping_guards(pp, s, he), code, leafname=sub[0])
                i = he + 1
            continue
        # plain member statement -> extend THIS type's own contiguous run
        endpos = member_body_end(src, s, b)
        if leaf_starts_decl(src[s:endpos]):
            if run_a is None:
                run_a = s
            run_b = endpos
            last_mem = endpos
        i = endpos
    # trailing own-members (no further nested type to flush them at) still pending
    flush()

# ---------------------------------------------------------------------------
# walkers
# ---------------------------------------------------------------------------
def walk(src, a, b, outroot, chainmeta, pp, stats):
    i = a
    while i < b:
        s = skip_ws_c(src, i, b)
        if s >= b: break
        if is_pp_line(src, s, b):
            j = src.find('\n', s, b); i = (j + 1) if j >= 0 else b; continue
        he = braced_token_pos(src, s, b)
        head = src[s:he].strip()
        if leaf_txt_is_empty(head) or head == '':
            i = he + 1; continue
        kind, name, pre = parse_head(head)
        if head.startswith('namespace '):
            # nested namespace shouldn't happen inside a type; bail out safely
            j = src.find('\n', he, b); i = (j + 1) if j >= 0 else b; continue
        has_brace = he < b and src[he] == '{'
        if kind in ('class', 'struct', 'interface'):
            if has_brace:
                cl = match_block(src, he, b)
                is_st = prefix_has(pre, 'static')
                # base clause rides THIS node so member leaves repeat `: Base`
                walk(src, he + 1, cl, outroot,
                     chainmeta + ((name, kind, is_st, base_of(head)),), pp, stats)
                i = cl + 1
            else:
                code = src[s:he + 1]
                if not leaf_txt_is_empty(code):
                    stats[0] += 1
                    emit_file(outroot, chainmeta, wrapping_guards(pp, s, he), code,
                              leafname=sf(name))
                i = he + 1
            continue
        if kind == 'enum':
            cl = match_block(src, he, b) if has_brace else he
            code = (src[s:cl + 1] if has_brace else src[s:he + 1])
            if not leaf_txt_is_empty(code):
                stats[0] += 1
                emit_file(outroot, chainmeta, wrapping_guards(pp, s, cl), code,
                          leafname=sf(name))
            i = (cl + 1) if has_brace else (he + 1)
            continue
        endpos = member_body_end(src, s, b)
        code = src[s:endpos]
        if leaf_txt_is_empty(code) or not leaf_starts_decl(code):
            i = endpos
            continue
        stats[0] += 1
        emit_file(outroot, chainmeta, wrapping_guards(pp, s, endpos - 1), code,
                  leafname=sf(mem_name(head)))
        i = endpos

def walk_namespace(src, a, b, outroot, pp, stats):
    i = a
    while i < b:
        s = skip_ws_c(src, i, b)
        if s >= b: break
        if is_pp_line(src, s, b):
            j = src.find('\n', s, b); i = (j + 1) if j >= 0 else b; continue
        he = braced_token_pos(src, s, b)
        head = src[s:he].strip()
        if leaf_txt_is_empty(head) or head == '':
            i = he + 1; continue
        kind, name, pre = parse_head(head)
        if head.startswith('namespace '):
            j = src.find('\n', he, b); i = (j + 1) if j >= 0 else b; continue
        has_brace = he < b and src[he] == '{'
        if kind in ('class', 'struct', 'interface'):
            if has_brace:
                cl = match_block(src, he, b)
                is_st = prefix_has(pre, 'static')
                # top-level type = head of chain; repeat its `: Base` clause so
                # every member leaf keeps the override owner (e.g. `: Editor`)
                walk(src, he + 1, cl, outroot,
                     ((name, kind, is_st, base_of(head)),), pp, stats)
                i = cl + 1
            else:
                if not leaf_txt_is_empty(src[s:he + 1]):
                    stats[0] += 1
                    emit_file(outroot, ((name, kind, False, base_of(head)),),
                              wrapping_guards(pp, s, he), src[s:he + 1])
                i = he + 1
            continue
        if kind == 'enum':
            cl = match_block(src, he, b) if has_brace else he
            code = (src[s:cl + 1] if has_brace else src[s:he + 1])
            if not leaf_txt_is_empty(code):
                n0 = name.split('`')[0]
                emit_bare(outroot, (n0,), wrapping_guards(pp, s, cl), code, stats)
            i = (cl + 1) if has_brace else (he + 1)
            continue
        # stray top-level member -> drop; none expected at namespace scope
        i = member_body_end(src, s, b)

# ---------------------------------------------------------------------------
# UPDATE MODE: seam-finding and in-place merge helpers
# ---------------------------------------------------------------------------
def _innermost_insertion_point(text, ntype_opens):
    """Find the closing brace of the innermost type body after descending
    `ntype_opens` nested bodies from the namespace block. Returns the index
    just before that closing brace (insertion point), or None on parse failure."""
    nm = re.search(r'\bnamespace\s+', text)
    if not nm:
        return None
    i = skip_ws_c(text, nm.end(), len(text))
    if i >= len(text) or text[i] != '{':
        return None
    ns_open = i
    ns_close = match_block(text, i, len(text))
    if ns_close >= len(text):
        return None

    pos = ns_open + 1
    hi = ns_close

    for _ in range(ntype_opens):
        # skip whitespace and comments to find next header
        p = skip_ws_c(text, pos, hi)
        q = p
        while q < hi:
            c = text[q]
            if c == '{':
                break
            if c == ';':
                return None  # header without body -> cannot descend further
            if c == '}':
                return None  # unexpected close before header
            if c in '"\'':
                q = scan_lit(text, q, hi)
                continue
            if c == '/' and q + 1 < hi and text[q + 1] == '/':
                j = text.find('\n', q, hi)
                q = (j + 1) if j >= 0 else hi
                continue
            if c == '/' and q + 1 < hi and text[q + 1] == '*':
                j = text.find('*/', q + 2, hi)
                q = (j + 2) if j >= 0 else hi
                continue
            q += 1

        if q >= hi or text[q] != '{':
            return None

        # found nested body open; descend into it
        ob = q
        cb = match_block(text, ob, hi)
        pos = ob + 1
        hi = cb

    return hi  # insertion point is just before the innermost body's closing brace


def _seam_insertion_point_sealed_text(sealed_text, ntype_opens):
    """For sealed _compact text (namespace NS{ heads[0]{...heads[n] {...} }}),
    find insertion point inside innermost type body. Returns index or None."""
    # sealed_text format: "namespace NS{  public static partial class Core\n{ int a = 1; }  }"
    # We need to descend ntype_opens bodies from namespace {
    nm = re.search(r'\bnamespace\s+', sealed_text)
    if not nm:
        return None
    i = skip_ws_c(sealed_text, nm.end(), len(sealed_text))
    if i >= len(sealed_text) or sealed_text[i] != '{':
        return None
    ns_close = match_block(sealed_text, i, len(sealed_text))
    if ns_close >= len(sealed_text):
        return None

    pos = i + 1
    hi = ns_close

    for _ in range(ntype_opens):
        p = skip_ws_c(sealed_text, pos, hi)
        q = p
        while q < hi:
            c = sealed_text[q]
            if c == '{':
                break
            if c == ';':
                return None
            if c == '}':
                return None
            if c in '"\'':
                q = scan_lit(sealed_text, q, hi)
                continue
            if c == '/' and q + 1 < hi and sealed_text[q + 1] == '/':
                j = sealed_text.find('\n', q, hi)
                q = (j + 1) if j >= 0 else hi
                continue
            if c == '/' and q + 1 < hi and sealed_text[q + 1] == '*':
                j = sealed_text.find('*/', q + 2, hi)
                q = (j + 2) if j >= 0 else hi
                continue
            q += 1

        if q >= hi or sealed_text[q] != '{':
            return None

        ob = q
        cb = match_block(sealed_text, ob, hi)
        pos = ob + 1
        hi = cb

    return hi


def _extract_member_body_lines(code):
    """Extract bare member-body lines from a code snippet (strips outer braces).
    Returns list of non-empty lines."""
    # Find first { and matching }
    brace_start = code.find('{')
    if brace_start < 0:
        return []
    end_pos = match_block(code, brace_start, len(code))
    if end_pos < 0:
        return []
    inner = code[brace_start + 1:end_pos]
    lines = [ln for ln in inner.split('\n') if ln.strip()]
    return lines


def _update_or_write(path, sealed_text, member_body_lines, ntype_opens, delete_guard=False):
    """Unified update-or-write helper.

    Args:
        path: final output file path (already deduped via wrote[] bookkeeping)
        sealed_text: the full text the writer would write fresh (for reference/backup)
        member_body_lines: list of lines to splice into existing file's innermost body
        ntype_opens: number of type bodies to descend from namespace {
        delete_guard: if True, prepend #if guard around new content

    Behavior:
        If UPDATE mode is OFF or file doesn't exist: write sealed_text fresh.
        If UPDATE mode ON and file exists AND parses successfully:
            - read existing content
            - locate insertion point in innermost type body
            - if member_body_lines are empty/whitespace-only: skip (no change)
            - if parsing fails at any seam: warn, write sibling _2.cs instead
            - otherwise: splice new lines before the closing brace, preserving guards
        If file exists but is not parseable as expected: write sibling _2.cs + warning.
    """
    global UPDATE_MODE

    # Determine final path (same dedup logic as writer)
    nm_nodes = [n for (n, k, st, *_) in chainmeta]
    folder = os.path.join(outroot, *nm_nodes)
    os.makedirs(folder, exist_ok=True)
    name0 = leafname if leafname else chainmeta[-1][0]
    n = 1; base = name0; keypath = os.path.join(folder, base + '.cs')
    while keypath in wrote:
        n += 1; base = '%s_%d' % (name0, n); keypath = os.path.join(folder, base + '.cs')

    if not UPDATE_MODE or not os.path.isfile(keypath):
        # Fresh write — same as before
        body = san_pp(sealed_text)
        _open = _body_pp_open(body)
        while _open > 0:
            body += '\n#endif'
            _open -= 1
        heads = []
        for idx, nd in enumerate(chainmeta):
            name, kind, is_st = nd[0], nd[1], nd[2]
            bs = nd[3] if len(nd) > 3 else ''
            is_last = (idx == len(chainmeta) - 1)
            mods = 'public '
            if is_st and kind == 'class':
                mods += 'static '
            if not (is_last and kind == 'enum'):
                mods += 'partial '
            mods += kind + ' ' + re.sub(r'\W', '_', name)
            if bs:
                mods += bs
            heads.append(mods)
        text = _compact(LIVE_NS, heads, body)
        L = list(wrapping_guards(pp, run_a, gb)) + [text] + ['#endif' for _ in wrapping_guards(pp, run_a, gb)]
        with open(keypath, 'w', encoding='utf-8') as fh:
            fh.write('\n'.join(l for l in L if l != '') + '\n')
        return

    # UPDATE mode: try to merge into existing file
    try:
        with open(keypath, 'r', encoding='utf-8') as f:
            existing = f.read()
    except (IOError, OSError):
        _warn_merge_failure(keypath, "cannot read")
        sibling = _make_sibling_path(keypath)
        if not os.path.exists(sibling):
            with open(sibling, 'w', encoding='utf-8') as fh:
                fh.write(sealed_text + '\n')
        return

    # Check for empty/whitespace-only member body — skip update, no write at all
    stripped = '\n'.join(member_body_lines).strip()
    if not stripped or stripped in (';', '  ', '   '):
        return  # nothing new to add; file unchanged

    # Attempt seam-based insertion
    insert_pos = _innermost_insertion_point(existing, ntype_opens)
    if insert_pos is None:
        _warn_merge_failure(keypath, "seam parse failed")
        sibling = _make_sibling_path(keypath)
        if not os.path.exists(sibling):
            with open(sibling, 'w', encoding='utf-8') as fh:
                fh.write(sealed_text + '\n')
        return

    # Check if members already exist at insertion point (dedup by line comparison)
    existing_parts = existing.split('\n')
    before = existing_parts[:insert_pos]
    after = existing_parts[insert_pos:]

    # Build candidate merged content to check for byte-equality
    new_lines = [ln.strip() for ln in member_body_lines if ln.strip()]
    if not new_lines:
        return  # nothing to add

    # Check if these lines already exist contiguously after insert_pos
    existing_after_stripped = '\n'.join(after).strip()
    new_content = '\n'.join(new_lines) + '\n'
    if new_content in existing_after_stripped:
        return  # already present; skip write

    # Splice in place
    merged_parts = before + new_lines + after
    merged_text = '\n'.join(merged_parts)

    with open(keypath, 'w', encoding='utf-8') as fh:
        fh.write(merged_text + '\n')

    print(f"  [UPDATE] spliced {len(new_lines)} member(s) into {keypath}")


def _make_sibling_path(path):
    """Return sibling path with '_2' suffix on filename."""
    dirn, fn = os.path.split(path)
    stem = fn[:-3] if fn.endswith('.cs') else fn
    return os.path.join(dirn, stem + '_2.cs')


def _warn_merge_failure(path, reason):
    """Log warning for merge failure; write sibling _2.cs."""
    print(f"  [WARN] Merge into '{path}' failed ({reason}) — writing sibling _2.cs")
    sibling = _make_sibling_path(path)
    if not os.path.exists(sibling):
        with open(sibling, 'w', encoding='utf-8') as fh:
            fh.write(sealed_text + '\n')


# Global flag for update mode
UPDATE_MODE = False

# ---------------------------------------------------------------------------
# convert_one
# ---------------------------------------------------------------------------
# Original structure restored:
#   - walk_byclass calls emit_file directly
#   - walk_namespace calls emit_file/emit_bare directly
#   - convert_one calls walk_byclass/walk_namespace with outroot, pp, stats

def convert_one(inp, outdir):
    """convert a single .cs into per-member leaves under outdir (relative tree mirror).
    Returns leaf count."""
    global LIVE_NS, NS_GIVEN

    with open(inp, encoding='utf-8', errors='replace') as fh:
        src = fh.read()

    # ALWAYS structurally locate the namespace block so the walkers operate on
    # the namespace BODY only, not the whole file.  RC-4: this was previously
    # skipped whenever -ns was passed (NS_GIVEN), so with a compact header like
    # `namespace NZK{ public static partial class Core { ...` the whole-file
    # walker's `namespace` branch jumped PAST the Core header (same line) and
    # flattened Core into nothing.  -ns only overrides the emitted namespace
    # STRING (LIVE_NS); it must not change the structural parse.
    mns = re.search(r'\bnamespace\s+([A-Za-z_]\w*(?:\.(?:[A-Za-z_]\w*))*)\s*\{', src)
    if mns:
        if not NS_GIVEN:
            LIVE_NS = mns.group(1)
        openbrace = mns.end() - 1
        endbrace = match_block(src, openbrace, len(src))
        body_a, body_b = openbrace + 1, endbrace
    else:
        body_a, body_b = 0, len(src)

    pp = build_pp(src)
    os.makedirs(outdir, exist_ok=True)
    stats = [0]

    if BVCLASS:
        _bc_deep_run(src, body_a, body_b, outdir, pp, stats)
    else:
        walk_namespace(src, body_a, body_b, outdir, pp, stats)

    return stats[0]


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
def _walk_cs(d):
    for root, _dirs, files in os.walk(d):
        for fn in files:
            if fn.endswith('.cs'):
                yield os.path.join(root, fn)


def main():
    global LIVE_NS, NS_GIVEN, UPDATE_MODE

    args = sys.argv[1:]
    outdir_opt = None
    delete_flag = False
    ns_opt = None
    global BVCLASS
    BVCLASS = False
    pos = []
    i = 0
    while i < len(args):
        a = args[i]
        if a in ('-o', '--out'):
            if i + 1 >= len(args):
                sys.exit('error: %s requires a directory argument' % a)
            outdir_opt = args[i + 1]
            i += 2
        elif a in ('-ns', '--name-space'):
            if i + 1 >= len(args):
                sys.exit('error: %s requires a namespace argument' % a)
            ns_opt = args[i + 1]
            i += 2
        elif a in ('-d', '--delete', '--deletedestfirst'):
            delete_flag = True
            i += 1
        elif a in ('-bc', '--by-class'):
            BVCLASS = True
            i += 1
        elif a in ('-u', '--update'):
            UPDATE_MODE = True
            i += 1
        elif a in ('-h', '--help'):
            print(__doc__.strip())
            return
        else:
            pos.append(a)
            i += 1

    if outdir_opt is not None:
        outdir = outdir_opt
        ns = pos[1] if len(pos) > 1 else None
    else:
        outdir = pos[1] if len(pos) > 1 else './Converted'
        ns = pos[2] if len(pos) > 2 else None

    if ns_opt is not None:
        ns = ns_opt

    inp = pos[0] if len(pos) > 0 else 'nzktoolkit_monolith_compilable.cs'

    if ns is not None:
        LIVE_NS = ns
        NS_GIVEN = True

    if delete_flag and os.path.isdir(outdir):
        shutil.rmtree(outdir, ignore_errors=True)

    if os.path.isdir(inp):
        rootsrc = inp.rstrip('/')
        files = sorted(_walk_cs(rootsrc))
        os.makedirs(outdir, exist_ok=True)
        total = 0
        for fu in files:
            parent = os.path.dirname(fu)
            rel = os.path.relpath(parent, rootsrc)
            sub = os.path.join(outdir, rel) if rel and rel != '.' else outdir
            total += convert_one(fu, sub)
        print('files=%d leaves=%d' % (len(files), total))
        print('-> %s' % os.path.abspath(outdir))
        return

    n = convert_one(inp, outdir)
    print('leaves=%d' % n)
    print('-> %s' % os.path.abspath(outdir))


if __name__ == '__main__':
    main()

