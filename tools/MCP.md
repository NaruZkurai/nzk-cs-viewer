# csgraph MCP server (`csgraph --mcp`)

A **multithreaded Model Context Protocol (MCP) server** built into the Rust engine.
It holds one pre-indexed graph in memory for the lifetime of the process and
exposes the **entire `/api/*` surface of the app** — every read *and* every write
command — as MCP tools over stdio, so an agent can drive the indexed project
directly instead of reading files by hand.

No HTTP, no port, no daemon: the harness spawns the process, speaks JSON-RPC 2.0
on stdin/stdout, and the process exits when stdin closes.

---

## Launch

```
csgraph --mcp --repo <path> [--repo <path>]... [--root <rel>]... [--exclude PAT]... [--threads N]
```

`csgraph --mcp --help` (or `-h`) prints this usage to stdout and exits `0`
without starting the server or building an index; the same flags print MCP usage
when combined with `--repo <path>`. Unknown options exit `2`, as does a missing
`--repo`. The tool list is exactly what the running server advertises through
`tools/list`.

| flag | required | default | meaning |
|---|---|---|---|
| `--repo <path>` | yes | — | Repository root the engine works in (absolute, or relative to the cwd of the spawned process). **Repeatable**; the first occurrence is the **primary** (default) repo and the rest are selectable per call. Duplicate canonical paths collapse into one entry (first wins, no error). |
| `--root <rel>` | no | *none* | Root inside a repo to **pre-index at startup**. Repeatable. **Global** — every bound repo uses the same roots. Omitting it starts **cold** (see [Pre-indexed startup](#pre-indexed-startup)). |
| `--exclude <pat>` | no | engine default list | Path component to exclude from discovery. Repeatable. **Global** across every bound repo. Default: `.git`, `obj`, `bin`, `tmp`, `node_modules`, `_bak`, `_bak_nzk_consolidate`, `_bak_v6_originals`, `_superseded`, `testing`, `.csgraph-copy`. |
| `--threads <N>` | no | `min(4, cores)`, never below 2 | Worker slots **per repo** (each holds an `Index` replica). `serverInfo.threads` reports the resolved value. |

The binary is the same `csgraph` used for `version`, `dump`, `dump-parser`,
`dump-doc`, `dump-reorg`, `dump-compile` and `doc`; those subcommands are
unchanged.

Build it offline (vendored crates, no network, no new dependencies):

```bash
cd "cs viewer builder/rust-only-csviewer"
source ../rust-env.sh                      # project-local toolchain + target dir
cargo build --offline -p csgraph-core      # binary: $CARGO_TARGET_DIR/debug/csgraph
```

### Harness configuration

Any MCP client that can spawn a stdio server works. A typical client config
(Claude Desktop / compatible `mcpServers` map):

```json
{
  "mcpServers": {
    "csgraph": {
      "command": "/abs/path/to/target/debug/csgraph",
      "args": [
        "--mcp",
        "--repo", "/abs/path/to/project",
        "--root", ".",
        "--threads", "4"
      ]
    }
  }
}
```

The important properties for a generic harness are:

* **transport**: `stdio` — the harness owns three pipes (`stdin`, `stdout`, `stderr`)
* **command**: the absolute path to the `csgraph` binary
* **args**: exactly the argv above
* **framing**: newline-delimited JSON (one JSON-RPC message per line). If your
  harness wants Content-Length framing instead, wrap the pipes with a tiny
  translator — the messages themselves are standard JSON-RPC 2.0.

From a shell, the same thing is:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"shell"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"state","arguments":{}}}' \
  | target/debug/csgraph --mcp --repo /abs/path/to/project --root .
```

---

## Repository selection — several repos, one server

The server can bind **more than one repository** in a single process: repeat
`--repo`. The **first** occurrence is the **primary** (the default repo). Every
tool accepts an optional `repo` argument that selects which bound repo the call
acts on:

| selector form | example | notes |
|---|---|---|
| 1-based index | `"2"` (string) or `2` (integer) | indexes the `--repo` order |
| path | `"/abs/Other Repo"` or a cwd-relative path | canonicalised before comparison |
| unique basename | `"Other Repo"` | case-insensitive; must be unique |

* **Omitted — or `null` — means the primary repo.** `repo:null` is deliberately
  treated exactly like an absent `repo`; it is not an error.
* An **unknown or ambiguous** selector returns an MCP-only error
  (`isError:true`) whose message lists **every** bound repo with its 1-based
  index. A basename that matches two repos is ambiguous; pass the path or index.
* The `repo` key is **consumed by the server and stripped before dispatch**, so
  it never reaches `resolve_call`, `dispatch_call`, or the edit/aux dispatch. A
  routed result with no `repo` argument is byte-identical to a single-repo
  server.
* `index` and `index_cancel` act on the **selected repo only**; other repos are
  untouched. `--root`/`--exclude` are global.
* Only the primary is pre-indexed at startup (when `--root` is given); secondary
  repos start **cold** and build on their first call, exactly like an explicit
  `index`.
* `workspace` reports the whole bound set (see below).

```jsonc
// one server, two repos — the first is primary:
// csgraph --mcp --repo /path/to/vrcCS --repo "/path/to/Other Repo" --root . --threads 4

{"q":"SpsSocketBaker"}                        // primary repo
{"q":"SpsSocketBaker","repo":"2"}             // the second --repo, by 1-based index
{"q":"SpsSocketBaker","repo":"/path/to/Other Repo"}
{"q":"SpsSocketBaker","repo":"Other Repo"}    // unique basename
{"q":"SpsSocketBaker","repo":null}            // == absent == primary

// re-index ONLY repo 2 over different roots; repo 1's graph is untouched
{"roots":["Assets","Packages"],"repo":2}
```

### Harness configuration for several repos

The DSH plugin bundle (`dsh_plugin/`) defaults to **one** server with repeated
`--repo` for every `CSGRAPH_REPOS` entry (argv
`--mcp --repo A --repo B ... --root R --threads T`, cwd = the first repo). The
old one-server-per-repo split is opt-in: `--split-servers` when running
`install.mjs`, or `CSGRAPH_SPLIT_SERVERS=1` (equivalently
`options.splitServers: true`). Adding or removing a server needs a profile
reload.

A generic harness can do the same:

```json
{
  "mcpServers": {
    "csgraph": {
      "command": "/abs/path/to/target/debug/csgraph",
      "args": [
        "--mcp",
        "--repo", "/abs/path/to/project",
        "--repo", "/abs/path/to/other-project",
        "--root", ".",
        "--threads", "4"
      ]
    }
  }
}
```

---

## Tools

`tools/list` returns **42 tools**. Twenty-two of them are typed convenience
wrappers around exactly one `/api/*` route; `api_call` is the generic escape
hatch that reaches **every** route, including `GET /api/index`, the
`/api/index/cancel` write, and the static fallback. The remaining **twenty**
bypass `/api/*` entirely: twenty MCP-only tools — fourteen refactor/query/edit tools (see
[MCP-only tools](#mcp-only-tools-beyond-apic)), the read-only `preview_change`
signature/call-site preview (M3 slice 2), plus the five aux tools — the
`parse_check` validity gate, the `compile_diff` differential Roslyn compile
(no Unity process or licence), the `find_duplicates` report, the first-class
`counts` and the `workspace` scope report. No aux tool is an edit tool, so the
app's `/api/edit` panel never offers a non-edit operation, and `preview_change`
has no write path at all.

**Every tool in the table below also accepts an optional `repo` argument**
(1-based index, path, or unique basename; absent/`null` = primary) — it is listed
per tool only where the surrounding behaviour needs it. See
[Repository selection](#repository-selection--several-repos-one-server).

Every tool result is a single text content block containing a JSON envelope.
The **success signal is unified**: every result's text carries a boolean `ok`.
Routed tools additionally carry `status` and `body`:

```json
{"status": <http status the HTTP host would send>, "ok": <status < 400>, "body": <the route body>}
```

MCP-only tools return their body directly, which already starts with `ok` (for
example `{"ok": true, "results": [...]}`). `isError` is `true` when a routed
`status >= 400` or an MCP-only `ok` is `false`; the envelope is still valid JSON,
so an agent can read the error as data. A malformed call (unknown tool, missing
required argument) returns `isError: true` with a plain-text message instead.

| tool | maps to | parameters | kind |
|---|---|---|---|
| `api_call` | any `method` + `path` through `api::handle` / `api::handle_payload` | `method` (`GET`\|`POST`, default `GET`), `path` (required), `query` (object or raw string), `body` (object) | read + **write** |
| `state` | `GET /api/state` | — | read |
| `index` | `POST /api/index` | `roots` (array or comma string), `repo` (optional — rebuilds **only** that repo) | **write** (rebuild) |
| `index_cancel` | `POST /api/index/cancel` | `repo` (optional — cancels **only** that repo) | **write** (stop flag) |
| `browse` | `GET /api/browse` | `path` | read |
| `search` | `GET /api/search` | `q` (required), `limit` (default 30) | read |
| `node` | `GET /api/node` | `id` (required) | read |
| `relation` | `GET /api/relation` | `node`, `key` (required), `offset`, `limit` | read |
| `find` | `GET /api/find` | `q` (required), `limit` (default 400) | read |
| `refs` | `GET /api/refs` | `q` (required), `limit` (default 400) | read |
| `source` | `GET /api/source` | `file`, `line`, `context`, `node`, `to`, `mode` | read |
| `doc` | `GET /api/doc` | `node` (required) | read |
| `decls` | `GET /api/decls` | `node`, `file` | read |
| `chain` | `GET /api/chain` | `id` (required), `depth`, `dir`, `max` | read |
| `kinds` | `GET /api/kinds` | — | read |
| `definitions` | `GET /api/definitions` | `scope`, `limit` | read |
| `subgraph` | `GET /api/subgraph` | `id`, `depth`, `max` | read |
| `export` | `GET /api/export.json` | — | read (whole graph) |
| `projects` | `GET /api/projects` | `files`, `file` | read |
| `viewer_state` | `GET /api/viewer_state`, or `POST` when `body` is present | `body` | read + **write** |
| `settings` | `GET /api/settings`, or `POST` when `body` is present | `body` | read + **write** |
| `reorg` | `POST /api/reorg` | `action` (`plan`\|`verify`\|`zip`\|`write`), `spec`, `dest`, `overwrite` | read + **write** (`write` emits a tree) |
| `fuzzy_search` | — (MCP-only) | `q` (required), `kind`, `limit` (default 30) | read |
| `resolve_chain` | — (MCP-only) | `symbol` (required), `depth`, `dir` (`forward`\|`reverse`\|`both`), `max` | read |
| `code_block` | — (MCP-only) | `node` / `nodes[]` / `q`, `limit`, `mode` (`doc`\|`effective`\|`raw`), `path_only` | read |
| `rename_symbol` | — (MCP-only) | `from`, `to` (required), `apply`, `scope`, `include_declarations` | read + **write** (`apply`) |
| `qualify_symbols` | — (MCP-only) | `files`, `remove_usings`, `apply`, `max_edits` | read + **write** (`apply`) |
| `expand_namespace` | — (MCP-only) | `files`, `namespace`, `apply`, `dest` | read + **write** (`apply` / `dest`) |
| `write_file` | — (MCP-only) | `path`, `content` (required), `expected_sha256`, `create`, `apply` | read + **write** (`apply`) |
| `apply_edits` | — (MCP-only) | `edits:[{file, old, new, count?}]`, `apply` | read + **write** (`apply`) |
| `select_decls` | — (MCP-only) | `like`, `mtype`, `kind`, `limit`, `text` | read |
| `move_decls` | — (MCP-only) | selector (`like`/`mtype`/`kind`/`limit` or `from`) + target (`to` = `"<Ns>.<Class>.<member>"`, or `newfile`/`dir`/`class`/`modifiers`/`prefix`/`namespace`), `allow_unresolved`, `allow_invalid`, `apply` | read + **write** (`apply`) |
| `grep` | — (MCP-only) | `pattern` (required), `glob`, `path`/`paths`, `limit`, `max_per_file`, `case`, `literal`, `count_only`, `context` | read |
| `reformat` | — (MCP-only) | `file` (required), `decl`, `require_shape`, `require_validated`, `allow_invalid`, `apply` | read + **write** (`apply`) |
| `functionize` | — (MCP-only) | `cluster` / `members`/`ids`, `newfile`/`dir`, `class`, `member`/`name`, `namespace`/`prefix`, `modifiers`, `allow_unresolved`, `require_validated`, `allow_invalid`, `apply` | read + **write** (`apply`) |
| `copy_path` | — (MCP-only) | `from`/`src`/`source`, `to`/`dest` (required), `overwrite`, `apply`, `action` (`copy`\|`recover`), `recover`, `max_files`, `max_bytes` | read + **write** (`apply`) |
| `preview_change` | — (MCP-only) | `node`/`id`/`symbol`/`from`, `op` (`rename`\|`retype`), `to`, `include_declarations` | read (never writes) |
| `parse_check` | — (MCP-only, aux) | `files`/`paths`, `exclude`, `timeout` | read (runs the Roslyn gate) |
| `compile_diff` | — (MCP-only, aux) | `files`/`paths`, `exclude`, `baseline_exclude`, `data`, `refs`, `define`, `timeout` | read (differential Roslyn compile; no Unity process, licence or focus) |
| `find_duplicates` | — (MCP-only, aux) | `min_lines`, `threshold`, `limit`, `roots`/`paths`, `exclude`, `max_clusters`/`max_members`/`max_blocks` | read (runs the dedupe report) |
| `counts` | — (MCP-only, aux) | `q`/`node` (optional; omitted = whole-index summary), `roots`/`paths`, `exclude`, `file` | read |
| `workspace` | — (MCP-only, aux) | `repo` (optional — selects the `selected`/`roots`/counts fields) | read (reports every bound repo) |

### MCP-only tools (beyond `/api/*`)

These fourteen are implemented in `core/src/edit.rs` (with the M2 reshaping
tools in `core/src/reshape.rs`) and are dispatched **before**
`resolve_call`/`dispatch_call`; they deliberately do not touch `core/src/api.rs`
or the frozen route table. `preview_change` (M3 slice 2) is implemented in
`core/src/preview.rs` and dispatched the same way, but is strictly read-only.
Every write tool is a **dry run unless `apply` is
passed**, refuses to write outside the indexed repo, and writes through a
sibling temp file + `rename`. Ordering is always deterministic (file, then
line).

* **`fuzzy_search{q, kind?, limit=30}`** — ranked match over node full/label/name:
  exact full > case-insensitive full > exact simple > prefix > subsequence, ties
  by `degree` desc then `full` asc. Returns
  `{ok, query, total, results:[{id, full, kind, label, file, line, score, match}]}`.
* **`resolve_chain{symbol, depth=3, dir=forward|reverse|both, max=300}`** —
  fuzzy-resolves `symbol` (node id or name) to **exactly one** node; an ambiguous
  or unknown symbol returns `{ok:false, candidates:[…]}`. On success it returns
  the `/api/chain` payload with an extra `resolved:{id,full,kind,label,file,line}`.
* **`code_block{node | nodes[] | q, limit=8, mode=doc|effective|raw, path_only?}`** —
  one code block assembled from `Index::doc_source` / `Index::source_shared`,
  keeping the `// ── node · file:line-endLine ──` part markers (one is synthesized
  when a single-part node has none). Returns `{ok, nodes, text, lines, parts, files}`.
* **`rename_symbol{from, to, apply=false, scope?, include_declarations=true}`** —
  resolves `from` to one declaration (refusing ambiguity), validates `to` as a
  C# identifier, and plans conservative line-level identifier-token replacements
  over declaration slices plus graph-attributed usage sites (incoming
  `calls`/`constructs`/`uses`/`assigns`/`inherits` edges). String literals and
  comments are skipped. Returns `{ok, from_node, to, ambiguity, files_planned,
  edits:[{file,line,before,after}], applied, written}`.
* **`qualify_symbols{files?, remove_usings=true, apply=false, max_edits=2000}`** —
  lists `using` / `using static` / `using Alias =` directives for deletion, then
  rewrites bare identifiers of **uniquely** declared types to their full name.
  Simple names declared more than once are reported in `skipped_ambiguous` and
  never rewritten. Returns `{ok, files_planned, remove_usings, edits,
  skipped_ambiguous, applied, written}`.
* **`expand_namespace{files?, namespace?, apply=false, dest?}`** — normalises
  each target file's `namespace X` header to the namespace the graph recorded for
  its declarations. With `dest` it delegates to
  `reorg action=write spec {"group":"namespace"}` and returns that report.
* **`write_file{path, content, expected_sha256?, create=false, apply=false}`** —
  atomically create or overwrite one file inside the indexed repo. Dry-run
  returns `{ok, path, create, bytes, old_sha256, new_sha256, applied:false}` and
  writes nothing; `apply` stages a same-directory temp file, `fsync`s it and
  renames it over the target (never deletes anything). `create=true` is required
  to create a new file, and `expected_sha256` refuses a write when the current
  bytes no longer match. Paths outside the repo and existing symlink targets are
  refused.
* **`apply_edits{edits:[{file, old, new, count?=1}], apply=false}`** — replaces
  the exact `old` string in each file (each edit's match count must equal its
  `count`, so a no-op is refused, never silent). All edits are validated before
  anything is staged, then every file is written temp+rename, giving
  all-or-nothing across the batch. Returns
  `{ok, edits:[{file, matched, replaced, old_sha256, new_sha256}], files_planned,
  applied, written}`.
* **`select_decls{like?, mtype?, kind?, limit?=200, text?}`** — the selection
  half of `nzkcs`: case-insensitive **substring** match over a declaration's
  `name`/`full` (not `fuzzy_search`'s subsequence match), `mtype` filter on
  `ret`, `kind` filter, and `text:true` adds `blockStart` plus the exact slice
  from the leading comment/attribute/`#if` block through `endLine`. Read-only.
* **`move_decls{like?/mtype?/kind?/limit? | from, to? | newfile?/dir?/class?/modifiers?/prefix?/namespace?, allow_unresolved?, apply=false}`** —
  the write half of `nzkcs`, composing `select_decls` → a fully validated plan →
  the atomic write path. Two target grammars:
  * **extract** — `newfile` (optionally `dir` + bare file name) moves every
    selected slice, *carrying its leading comment/attribute/`#if` block*, into
    that file and re-namespaces it under `prefix`/`namespace`. A **member**
    selection is wrapped in the toolkit dialect opener
    `namespace <NS>{  public static partial class <Class>{`, closed by `}}`
    (`modifiers` overrides the default `public static partial`). The destination
    type is `class`, else inferred from a qualified `newfile` stem
    (`NZK.VF.cs` → `VF`); if neither is available the call is **refused** rather
    than emitting a namespace-level member. A **type** selection keeps the
    namespace-only wrapper. When the destination file already exists, its
    current text is loaded and the append is committed in the **same**
    all-or-nothing batch as the origin delete (never a delete without a write).
  * **rename-into** — `to = "<Namespace>.<Class>.<member>"` moves exactly one
    declaration (select it with `from`, or any selector that yields one),
    creating the class and namespace when absent and renaming the member.

  **Selection honesty.** `from` is exact-only — node id, exact `full`/`label`,
  then a *unique* exact simple name; there is no substring/fuzzy fallback, so a
  stale `from` for a symbol that has moved returns `ok:false` with `near:[…]`
  instead of silently moving a similarly-named sibling. A recorded slice whose
  declaration no longer mentions its own name on disk (an `apply` without a
  following `index`) is likewise refused. Members selected from several owner
  types that would collide as the same `name(parameter list)` in the destination
  type are refused with a `duplicates` list (CS0111); legal overloads still move.

  **Resolution honesty.** Originals are deleted and every attributed call site is
  rewritten to the new fully-qualified name. The moved block's own dependencies
  (siblings left behind) are re-referenced to their unique fully-qualified name
  via the same token scanner. Identifiers **declared inside** the moved slice —
  the declaration and type names, generic parameters, parameter names,
  `var`/`for`/`foreach`/`using`/`catch`/typed locals and lambda parameters — are
  never re-qualified (qualifying a parameter produced invalid C# such as
  `Has(string NZK.S.P.a, string b)`). When a dependency name cannot be uniquely
  resolved across the corpus, the call is **refused** by default with the list in
  `unresolved`; `allow_unresolved:true` is the explicit escape hatch that emits
  anyway and records a loud `warnings` entry. The dry run exposes the destination
  file's full text as `destination.content` **and** on the `create`/`insert`
  edit's `content`, so a caller can review it before `apply`. All deletions,
  insertions and rewrites are validated before a single byte is staged; `apply`
  writes temp+fsync+rename and rolls back renamed files on failure.

  **Validity gate.** On `apply`, the candidate bytes of every file the commit
  would change are parked in the OS temp dir and parsed by the real Roslyn
  driver (`compile_check`, the same gate as `parse_check`). Any parse diagnostic
  **refuses the whole batch** — nothing is written — unless `allow_invalid:true`
  overrides it with a loud `warnings` entry. `validated` is `"roslyn"` when the
  gate actually ran and passed, or `"skipped:<reason>"` when no
  toolchain/driver is available (it then proceeds, but never claims a pass).
  `"not-run"` means a dry run. Returns
  `{ok, mode, target, selected, files, edits, ambiguous, unresolved, warnings,
  duplicates?, near?, diagnostics?, destination, validated, applied, written,
  created, reindex_required}` — **after an apply, call `index` to re-sync the
  graph** (`reindex_required: true` makes that explicit).
* **`grep{pattern, glob?, path?/paths?, limit=200, max_per_file=50, case=true,
  literal=false, count_only=false, context=0}`** — regex/text search over the
  **indexed corpus texts** (`Index::file_paths`/`file_text_of`; it never walks
  the disk independently), so a shell `grep -n` one-liner is one MCP call.
  `pattern` is a Rust/RE2-style regex — linear time, no backtracking, so there is
  no catastrophic-regex hazard; `literal:true` escapes it and `case` defaults to
  `true` (*ignore* case), with `case:false` case-sensitive. `glob` is shell-style
  (`*` within a segment, `**` across, `?` one char; a pattern without `/`
  matches the basename), and `path`/`paths` are repo-relative file or directory
  scopes. `limit` caps returned rows (files when `count_only`), `max_per_file`
  caps rows per file while `count` keeps the true per-file count, and
  `context:N` adds `before`/`after` lines. Returns
  `{ok, pattern, literal, case, count_only, total_matches, file_count,
  files:[{file, count, matches:[{line, col, text, before?, after?}]}],
  truncated}` in deterministic file/line order.
* **`reformat{file, decl?, require_shape=false, require_validated=false,
  allow_invalid=false, apply=false}`** — normalises one file (or one declaration
  inside it, `decl` = node id / exact full/label/name) to the toolkit dialect
  documented in the repo README:
  `/* */` comments only (`//` and `///` converted with the text preserved
  verbatim), no `using` directives, fully-qualified type names, the
  `namespace NS{  public static partial class X{` opener with 2-space
  indentation, and trivial one-expression bodies collapsed onto the signature
  line. Dry-run by default and byte-deterministic: the plan carries
  `{file, scope, declaration, changed, before, after, hunks, passes:{comments,
  usings, qualified, collapsed, shape}, skipped_ambiguous, validated,
  unverified, noop}` and never writes. `apply` writes atomically
  (`write_atomic_sync`: sibling temp + fsync + rename) and runs the Roslyn parse
  gate over the candidate bytes. When the text already matches the target there
  is nothing to write: the call reports `applied:false, noop:true` (a no-op, not
  a failure), so a caller keying off `applied` is never misled on a
  toolchain-less box.
* **`functionize{cluster | members/ids, newfile (+ dir), class?, member/name,
  namespace?/prefix?, modifiers="public static", allow_unresolved=false,
  require_validated=false, allow_invalid=false, apply=false}`** — takes a dedupe
  cluster (a `find_duplicates` exact cluster object, or explicit node ids /
  `{file,line}` references), proves the members' bodies are token-identical
  (`dedupe::body_fingerprint`) and their parameter lists and return types match,
  extracts the common body into one new shared static helper in
  `namespace NS{  public static partial class X{`, and rewrites **every**
  duplicate site to call it (`return Helper.Member(args);`, or
  `Helper.Member(args);` for a `void`/constructor). Returns
  `{helper:{file, created, namespace, class, classFull, member, returnType,
  params, signature, call, content}, selected, files, edits, destinationEdits,
  unresolved, warnings, validated, unverified, ...}`. Dry-run by default; `apply`
  is the same all-or-nothing atomic commit `move_decls` uses.
* **`preview_change{node|id|symbol|from, op:"rename"|"retype", to,
  include_declarations=true}`** — M3 slice 2: before any write, show exactly
  which signatures and call sites a change to **one** declaration would touch.
  The selector resolves exactly one declaration; an unknown or ambiguous
  selector is refused (`ok:false`, and for ambiguity an `ambiguity:[…]` list of
  candidate ids). `op:"rename"` plans through the real `rename_symbol` dry run,
  so the preview cannot drift from what an apply would do; `op:"retype"`
  replaces the declaration's declared type span (methods and fields). The body
  carries the `currentSignature`, the `proposedSignature`,
  `affectedDeclarations` (incoming callers/uses with `file`+`line`), every
  `callSites` row with line numbers, the exact `edits` (before/after) an apply
  would make, `counts` and `truncated`. It is **read-only by construction**:
  `readOnly:true`, `applied:false`, `writes:[]`, there is no `apply` argument
  and no write path. The app parity route is `GET|POST /api/preview` (also
  read-only), rendered by the read-only preview panel in the viewer.
* **`copy_path{from|src|source, to|dest, overwrite=false, apply=false,
  action="copy"|"recover", recover=false, max_files=20000,
  max_bytes=536870912}`** — copy one **file or directory tree** to a new path
  inside the indexed repo, so an agent no longer needs a shell `cp` to bootstrap
  a fixture or a bench. Dry-run (the default) returns the plan and **never
  writes or creates a directory**:
  `{ok, tool:"copy_path", from, to, kind:"file"|"dir", apply, applied:false,
  overwrite, dirs:[…], files:[{from, to, bytes, sha256,
  action:"create"|"overwrite"}], created, overwritten, totalBytes,
  skipped:[{path, reason:"symlink"}], written:[]}`.
  It refuses a symlinked source, a symlinked destination or any symlink on the
  destination's existing parent chain, a `..`/absolute path outside the repo, and
  a `to` equal to or inside `from`; symlinks encountered inside a source tree are
  skipped and reported. A **hardlinked** source or target (unix `nlink > 1`) is
  refused too, with the path named, so a copy never silently duplicates or
  destroys an existing hardlink. To narrow the plan/apply race, sources are
  opened with `O_NOFOLLOW` and the opened descriptor is re-checked against the
  plan-time `(dev, ino)` (and regular-file/nlink state) before the displacing
  rename, so a path swapped after planning becomes an `ok:false` refusal instead
  of a write to the wrong file. `overwrite:false` (default) refuses a request whose
  destination files already exist, returning `collisions:[…]` and writing
  nothing; `overwrite:true` replaces exactly those files and keeps unrelated
  destination files. The honest `max_files`/`max_bytes` guards refuse an
  oversized request before reading or hashing. Empty directories are recreated.
  After an `apply`, call `index` to re-sync the graph (`.csgraph-copy` is an
  engine default exclude, so the journal is never indexed).

  **Durable apply + recovery.** An `apply` is recoverable across process
  boundaries, not just an in-process rollback. Before the first mutation the
  engine writes a fsynced journal at `<repo>/.csgraph-copy/<nonce>.json` and, per
  overwritten target, renames the existing bytes to a same-directory backup
  `<target>.csgraph-bak-<nonce>` (so the rename is atomic on one filesystem). On
  success it deletes the backups, the journal and the now-empty journal dir; on a
  failure it recovers immediately, reports `rolled_back`, and cleans up.
  `action:"recover"` **obeys the same dry-run convention as every other write**:
  without `apply:true` it scans the pending journals and returns the plan
  (`apply:false, applied:false, dryRun:true`, planned
  `restored`/`removed`/`removedDirs` counts and a per-entry `plan:[…]`) while
  changing nothing; with `apply:true` it restores — backup exists ⇒ rename it
  back over the target, `existed:false` ⇒ remove the target — then removes created
  files, removes created dirs deepest-first only when empty, and deletes the
  journals. It is **idempotent**; a second recover is a no-op. Result:
  `{ok, action:"recover", journals, restored, removed, removedDirs, incomplete}`.
  A journal is only acted on when it is **engine-shaped**: `tool:"copy_path"`,
  `version:1`, a non-empty `nonce`, and a `repo` equal to the canonical repo id,
  all written by the engine. An `existed:false` target is removed only if its
  on-disk bytes still hash to the `sha256After` the journal recorded; an
  `existed:true` backup must exist as a regular file *and* be named exactly
  `<target>.csgraph-bak-<nonce>` (the engine's own sibling artifact) before it is
  renamed back. Anything else yields `incomplete:true` and deletes nothing. A
  foreign or tampered journal is therefore refused rather than obeyed, and the
  journal is kept for inspection. These checks stop a crafted journal from
  *amplifying* into an unrelated move/delete; they are deliberately not a
  privilege boundary. A writer who can already put files under
  `<repo>/.csgraph-copy/` is inside the repo's trust domain and could touch the
  target files directly, so a fully engine-shaped `existed:false` entry naming a
  known in-repo file is a documented, accepted residual rather than a security
  gap. An `apply:true` with other journals pending is refused with
  `unrecovered:[journal paths]` unless `recover:true` (recover first, then
  proceed).

  **Fault injection (test-only).** `CSGRAPH_COPY_FAULT` ∈
  `create-dirs` | `after-rename:<N>` | `before-journal-delete` |
  `after-commit:<N>` makes the apply trip at that point and report
  `{ok:false, error, fault}` while **leaving the crash state on disk** (no
  auto-rollback), so a *second process* can prove `action:"recover"`. It is
  never set by shipped configuration.

The **aux** tools are MCP-only but, unlike the fourteen above, are **not** in
`edit::TOOLS` (`core/src/edit.rs` keeps a separate `AUX_TOOLS`), so the app's
`/api/edit` panel never offers them:

* **`parse_check{files?/paths?, exclude?, timeout?=25}`** — runs the local
  **Roslyn validity gate** (`core/src/compile_check.rs`, the exact call
  `csgraph parse-check` makes; no re-implementation) over the indexed corpus, or
  the named subset. `files`/`paths` scope the selection (repo-relative
  file/directory) and `exclude` subtracts shell globs or prefixes; `timeout` is
  seconds (1..30). Returns the gate's report verbatim:
  `{ok, toolchain, files, count, diagnostics:[{severity,id,file,line,col,message}],
  skipped}`. `ok:false` whenever any parse diagnostic exists, and a concrete
  non-null `skipped` reason (never a false `ok:true`) when no Roslyn
  toolchain/driver is available. Use it **before `apply`** so an edit cannot land
  invalid C#. Read-only; it reads the file list from `Index::file_paths` and
  never walks the disk.

The second aux tool is the owner's priority-(2) duplicate-code analysis:

* **`find_duplicates{min_lines?, threshold?, limit?, roots?/paths?, exclude?,
  max_clusters?/max_members?/max_blocks?}`** — runs the dedupe engine
  (`core/src/dedupe.rs`; the exact function `csgraph dedupe` prints) and returns
  that JSON **verbatim**, so the MCP body is byte-identical to the CLI `--json`
  for the same repo/root. `exact` clusters are token-normalised
  method/constructor bodies (whitespace/comments ignored); `near` clusters are
  Jaccard-similar above `threshold`; `blocks` are recurring runs of
  `>= min_lines` non-trivial lines. `min_lines`/`threshold` map to the CLI flags,
  `limit` caps returned clusters/blocks (`max_*` override individually), and
  `roots`/`paths`/`exclude` scope the scan (default: the indexed roots). Returns
  `{ok, repo, roots, files, methodBodies, units, minLines, threshold, exact,
  near, blocks, stats:{exactClusters, nearClusters, blockGroups,
  pairComparisons}, truncated}` in deterministic order. A bounded report is
  surfaced honestly through `truncated` and `stats` — never a false empty
  result. Read-only.

The third aux tool is the owner's gap-4 "counts are not first-class" fix, kept
MCP-only exactly so no field is ever added to the frozen `find`/`refs` payloads:

* **`counts{q?/node?, roots?/paths?, exclude?, file?}`** — with no argument, a
  whole-index summary:
  `{ok, scope:"corpus", repo, roots, files, nodes, declarations,
  byKind:{kind:count}, edges, byEdgeKind:{kind:count}}`. With `q` (node id,
  exact full/label/simple name, then a unique declaration resolve), exactly one
  symbol's attributed graph counts:
  `{ok, scope:"symbol", symbol:{id,full,label,name,kind,file,line,degree},
  definitions, matches, usages, incomingTotal, outgoingTotal,
  incoming:{kind:count}, outgoing:{kind:count}, callers, usageLines}`.
  `definitions` counts declaration nodes that share the full name, `usages`
  counts incoming edges of the graph's usage kinds
  (`calls`/`constructs`/`uses`/`assigns`/`inherits`), `callers` counts distinct
  call sources, and `usageLines` sums the annotated usage lines. An ambiguous
  `q` is refused with `candidates`.

  **Per-root filtering.** `roots` (alias `paths`) narrows every corpus count to
  those repo-relative prefixes and `file` is sugar for one file; `exclude`
  subtracts globs or prefixes. A scoped corpus response keeps the same keys and
  adds `roots` (the requested scope), `scoped:true`, `filter:{roots, exclude}`
  and `byRoot:{root:{files, nodes, edges, declarations}}` — each per-root entry
  is computed independently (an edge counts for a root when either endpoint's
  own file is inside that root), so `byRoot[x]` equals a single-root
  `counts{roots:[x]}`. With both `q` and a scope, the symbol counts are
  restricted the same way and the body carries `filter`. The unscoped body is
  byte-identical to before the scoping existed (no `filter`/`scoped`/`byRoot`
  keys). Read-only, one call, no route.

The fourth aux tool is the owner's gap-2 scope report:

* **`workspace{repo?}`** — reports exactly what this server holds:
  `{ok, tool, repo (primary), roots, indexed, files, nodes, servers:"one process,
  N bound repo(s)", selected, defaultRepo,
  repos:[{index, repo, roots, loaded, slots, default}],
  scoping:{multiRoot, reScope, repoSelector}, limitation}`.
  `repos` lists every bound `--repo` with its 1-based index, roots, whether it
  has `loaded`, its slot count and whether it is the `default` (primary);
  `selected` is the repo this call selected (the `repo` argument; absent =
  primary), and the `roots`/`indexed`/`files`/`nodes` fields describe that
  selection. `scoping.multiRoot` describes the global repeatable `--root`;
  `reScope` is the exact `index {roots:[…], repo:…}` call that re-indexes the
  selected repo; `repoSelector` documents the per-call `repo` argument and that
  `index`/`index_cancel` act on the selected repo only. `limitation` states that
  every repo shares one process and that `--root`/`--exclude` are global.
  Read-only, no route.

### Several repos in one server

A single `csgraph --mcp` process can bind **several** repositories: repeat
`--repo`, and the first is the primary/default. Every tool then takes the
optional `repo` argument documented under
[Repository selection](#repository-selection--several-repos-one-server); `index`
and `index_cancel` are scoped to the selected repo, and `workspace.repos`
enumerates the bound set. Only the primary is pre-indexed at startup; secondary
repos start cold and build on their first call.

What one session *also* spans is several roots of one repo: `--root` is
repeatable and global, so every bound repo uses the same roots. Because a repo's
own graph is shared, a node from root A can resolve into root B (a call site in
`_sealed_src` naming a type declared under `tools/fixtures`), which is the
interesting case for an agent.

```bash
# two repos, one process, one server entry (the DSH plugin default shape)
csgraph --mcp --repo /path/to/vrcCS --repo "/path/to/Other Repo" \
        --root _sealed_src --root "tools/fixtures" --threads 4

# or re-scope the selected repo over a different root set
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"index",
 "arguments":{"roots":["_sealed_src","tools/fixtures"],"repo":"2"}}}
```

### The frozen route contract and the plugin

> **Superseded by the multi-repo section above.** The text below is kept only as
> the historical one-repo model; the current behaviour is a single process
> binding several `--repo` with a per-call `repo` selector.

A `csgraph --mcp` process is bound to **one** `--repo`. That is the scope of the
`/api/*` surface, `find`/`refs`, and the pre-indexed graph; the frozen route
table has no repo parameter and the clone contract forbids adding one.

What one session *can* span is several roots of that repo:

```bash
# two roots of the same repo, one process, one graph
csgraph --mcp --repo /path/to/vrcCS --root _sealed_src --root "tools/fixtures" --threads 4

# or re-scope the running server over a different root set
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"index",
 "arguments":{"roots":["_sealed_src","tools/fixtures"]}}}
```

`workspace` reports the current `repo`+`roots`; `counts {}` reports the same
`repo`/`roots` alongside the corpus totals. Because the graph is shared, a node
from root A can resolve into root B (a call site in `_sealed_src` naming a type
declared under `tools/fixtures`), which is the interesting case for an agent.

A second repo is now bound into the **same** server with another `--repo` (see
[Repository selection](#repository-selection--several-repos-one-server)); the
historical model was a separate process:

```bash
csgraph --mcp --repo "/path/to/Other Repo" --root "Assets/NZK toolkit v5" --threads 4
```

The DSH plugin bundle (`dsh_plugin/`) now binds every `CSGRAPH_REPOS` entry
(comma-separated or JSON array) to **one** server by default (repeated `--repo`),
with the historical one-entry-per-repo split available opt-in via
`--split-servers`/`CSGRAPH_SPLIT_SERVERS=1`. With only `CSGRAPH_REPO` set the
result is exactly one entry. Adding a server needs a profile reload.

The **frozen `/api/*` route table still has no repo parameter** and the clone
contract forbids adding one: `repo` is a server-level selector that is stripped
from the argument map before dispatch, so routed payloads stay byte-identical. A
secondary repo is selected lazily (its own `RepoSlot`), so the primary's
pre-indexed "never re-parse" contract is unaffected. See
[Repository selection](#repository-selection--several-repos-one-server).

**Limitations, deliberately accepted.** The pass is line/token based, not a
Roslyn rewrite: `qualify_symbols` qualifies a declaration whose simple name is
unique across the corpus — unique *types* and unique *static members* — while a
name preceded by `.` (ordinary member access) is left alone; interpolated-string
`{…}` holes are scanned as code, not as string text. The in-memory graph is not
rebuilt after `apply` (call `index` to re-sync). `rename_symbol` can
over-replace a same-named symbol that appears inside an attributed usage line.
`expand_namespace` only normalises a file with exactly one namespace header.
`apply_edits` is a textual match, not a parse: callers own the exact strings, and
if a rename fails in the final phase the files already written are rolled back
to their original bytes (an incomplete rollback is reported explicitly).
`move_decls` is likewise textual: it rewrites call sites attributed by the graph,
not by scope analysis, so a simple name declared more than once is reported
ambiguous instead of rewritten. Its dependency pass only qualifies
`static`/`const` members and types (the ones reachable through their owner type),
and it re-qualifies a bare dependency even when the origin context made it
implicit; instance members are left for a real compiler.
`reformat` **refuses rather than guesses**: a `//`/`///` comment whose body would
contain `*/`, a raw string literal (`"""…"""`), an unterminated single-line
literal, or a `using` directive it cannot parse each abort the whole call;
multi-line verbatim strings (`@"…"`/`$@"…"`) are supported but qualification is
skipped on any line that touches one. "Fully-qualified" means "as far as the
graph proves": only unique declared names are rewritten, ambiguous ones are
reported in `skipped_ambiguous` and left bare, framework types are never
invented, and `#` directives are never rewritten. Body collapse is conservative
(one brace-free, comment-free statement line on a `(`-bearing signature);
`shape` applies only to exactly one top-level namespace holding exactly one
`static` class with no base list and no `sealed`/`abstract`, otherwise it reports
`skipped:<reason>` (`require_shape:true` turns that into a refusal). The produced
text is idempotent, and a dry run is byte-deterministic. `functionize` requires
an **exact** dedupe cluster (token-identical bodies), identical parameter-list
text and return type, and method/constructor members (generic members are
refused); it refuses when a body identifier cannot be uniquely qualified
(`allow_unresolved:true` downgrades that to loud `warnings`), treats the
cluster's own member names as unresolved so it cannot silently recurse, and
refuses a stale slice that no longer declares its own name (re-run `index`).
A skipped validity gate is never a silent pass: `apply` on either reshaping tool
(or `move_decls`) reports `validated:"skipped:<reason>"` + `unverified:true`, and
`require_validated:true` refuses the write instead.

### Write commands

Everything the app can mutate is reachable:

* `index` / `api_call POST /api/index` — re-parse and install a new graph for the
  **selected repo**. The call is **broadcast to every worker slot of that repo**,
  so all of its replicas answer from the new graph afterwards; every other bound
  repo is untouched.
* `index_cancel` — ask an in-flight build to unwind. It is answered out of band
  (never queued behind the rebuild), sets only the **selected repo's** stop flag
  (shared by that repo's slots), and interrupts a rebuild that is already holding
  the write gate.
* `viewer_state` with `body` — writes `<root>/.csgraph.v2/viewer.json`.
* `settings` with `body` — writes the harness user's `~/.config/csgraph/settings.json`.
* `reorg` with `action: "write"` — writes the reorganised tree under the
  caller's absolute `dest`.
* `api_call` with any POST route — the generic path for all of the above.

### Route coverage

The tool descriptions name the route each tool executes. The unit test
`mcp::tests::tools_list_has_one_entry_per_tool_and_covers_every_route` reads
`core/src/api.rs` with `include_str!`, extracts every `/api/...` literal outside
the test module, and fails if any route is not named by a tool. The probe does
the same extraction and additionally **executes every route** through `api_call`.
The current set is 21 routes:

```
GET  /api/state /api/index /api/browse /api/search /api/node /api/relation
     /api/find /api/refs /api/source /api/doc /api/decls /api/chain /api/kinds
     /api/definitions /api/subgraph /api/export.json /api/viewer_state
     /api/settings /api/projects
POST /api/index /api/index/cancel /api/viewer_state /api/settings /api/reorg
```

(`GET /api/index` is the app's build route too, so `api_call` GET reaches it.)

---

## Concurrency and threading

* **Worker pool.** The process starts `--threads` worker threads. Each worker
  own a slot: a `Mutex<Index>`. A slot is handed to a worker through a bounded
  channel of free slot ids, so at most `threads` calls run at once.
* **Each slot is a complete index.** The first slot builds the graph at
  startup; the rest load the same graph from the on-disk cache. That makes reads
  on different slots genuinely parallel — two independent reads are **not**
  serialised behind each other and never share a lock.
* **The read loop never blocks.** The stdio reader only parses lines and pushes
  `tools/call` requests onto an **unbounded** job queue. Protocol methods
  (`initialize`, `ping`, `tools/list`, notifications) are answered immediately
  on the reader thread, so they stay responsive while a multi-second tool call
  is in flight.
* **Writes.** `/api/index` takes a write gate, waits until every slot is idle,
  applies the same call to all slots, and only then answers. A read arriving
  during a rebuild waits for a slot — deliberately: no caller may ever observe a
  half-built graph. `/api/index/cancel` is answered **out of band, before the
  gate**: it stores one shared stop flag (cloned into every slot through
  `Index::set_cancel_flag`) and replies immediately, so it can interrupt a
  rebuild that holds the gate. The broadcast loop stops applying the rebuild to
  the remaining slots as soon as a slot reports `cancelled: true` (or the flag is
  set); those slots keep the previous graph, and the next `index` re-syncs them
  (`Index::build` clears the flag at its start). All other writes (`settings`,
  `viewer_state`, `reorg`) run on one slot; the first two write a file that
  every slot reads back from disk, so the mutation is immediately visible.
* **Responses** are written under a stdout mutex, one complete JSON line at a
  time; concurrent calls may be answered out of order (legal for JSON-RPC) and
  are matched by `id`.
* **Memory.** `--threads K` holds roughly `K ×` the graph in memory. Use
  `--threads 1` for a very large corpus; use `--threads 4` (the default on a
  4-core machine) for parallel agent reads.

Measured on the probe fixture (250 files, ~4.5k nodes, 4 threads): a
`/api/export.json` call that serialises 3.4 MB of graph takes **0.358 s** on one
slot while 12 `state` reads plus a `ping` are all answered in the same window
(batch wall **0.051 s**).

Stop is out of band: on a cold rebuild that took **2.5 s**, `index_cancel`
answered in **0.1–0.3 ms** while the rebuild held the write gate and every
slot, and the rebuild itself returned at **+0.15 s** with
`body.cancelled == true` instead of running to completion. A follow-up `index`
after adding a file reported `fileCount 251` on 8 concurrent reads, proving the
new graph reached every slot.

---

## Pre-indexed startup

* **With `--root`:** the **primary** repo (the first `--repo`) is indexed
  **once** at startup and the graph is held in memory; secondary repos start
  cold and build on their first call. Repeated tool calls never re-parse. The
  build reuses the on-disk graph cache (`<root>/.csgraph.v2`) when the file set
  and mtimes match, so a restart is a cache load, not a parse:
  `state.body.cache.fromCache` is `true` and `state.body.parseMs` is ~0.
* **Without `--root`:** the server starts **cold** (`state.indexed == false`).
  Nothing is parsed until the agent calls `index` (or `POST /api/index`).
* **Re-indexing:** call the `index` tool with `{"roots": ["."]}`, optionally with
  a `repo` selector (absent = the primary). The build runs synchronously on a
  worker of the selected repo, is broadcast to all of that repo's slots, and the
  response is the selected repo's new `/api/state`.
* **Stop an in-flight build:** call `index_cancel` (optionally with `repo`) while
  `index` is in flight. It replies immediately
  (`{"status":200,"body":{"ok":true,"cancelled":true}}`) even though the rebuild
  holds the write gate; the rebuild unwinds without installing a partial graph
  and returns the previous state with `cancelled: true`. Slots of that repo that
  had not been rebuilt yet keep their previous graph, so no caller sees a
  half-built one; the next `index` re-syncs them all.

---

## Worked transcript

Real output from a 1-file fixture, verbatim. `$` is a client request line, `<-`
a server response line.

```text
$ {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "my-harness", "version": "1.0"}}}
<- {"id": 1, "jsonrpc": "2.0", "result": {"capabilities": {"tools": {}}, "instructions": "csgraph exposes the whole /api/* surface of the running engine. Use `api_call` for any route; the typed tools map 1:1 onto the common routes. Tool results carry {\"status\": <http code>, \"body\": <route body>}; isError is set when status >= 400. `index` re-builds the graph, `reorg` mutates a destination tree, `settings`/`viewer_state` write state.", "protocolVersion": "2025-06-18", "serverInfo": {"name": "csgraph", "threads": 2, "version": "2.0.0"}}}

$ {"jsonrpc": "2.0", "method": "notifications/initialized"}

$ {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
<- {"id": 2, "jsonrpc": "2.0", "result": {"tools": [ ... 41 entries ... ]}}
```

First entry of `result.tools`:

```json
{"description": "Generic escape hatch: call ANY route the app can execute, reads and writes. Maps to method + path, routed through api::handle/handle_payload. GET reaches every GET route; POST reaches every POST route (including /api/index, /api/index/cancel, /api/settings, /api/viewer_state, /api/reorg).", "inputSchema": {"additionalProperties": true, "properties": {"body": {"description": "JSON body for POST routes.", "type": "object"}, "method": {"default": "GET", "description": "HTTP verb the route is registered under.", "enum": ["GET", "POST"], "type": "string"}, "path": {"description": "Route path, e.g. /api/state or /api/reorg.", "type": "string"}, "query": {"description": "Query parameters as an object ({\"node\": \"...\"}) or a raw query string.", "type": ["object", "string"]}}, "required": ["path"], "type": "object"}, "name": "api_call"}
```

A **read** call:

```text
$ {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "search", "arguments": {"q": "C.A", "limit": 5}}}
<- {"id": 3, "jsonrpc": "2.0", "result": {"content": [{"text": "{\"body\":{\"ok\":true,\"results\":[{\"access\":\"public\",\"degree\":2,\"endLine\":5,\"endOff\":79,\"external\":false,\"file\":\"a.cs\",\"full\":\"N.C.A\",\"id\":\"N.C.A(int)\",\"inDeg\":1,\"kind\":\"method\",\"label\":\"C.A\",\"line\":5,\"literal\":false,\"name\":\"A\",\"ns\":\"N\",\"outDeg\":1,\"owner\":\"N.C\",\"ownerKind\":\"class\",\"parent\":\"N.C\",\"ret\":\"int\",\"startOff\":43,\"static\":false}]},\"status\":200}", "type": "text"}], "isError": false}}
```

A **write** call (POST `/api/viewer_state`) followed by its read-back — the
mutation is real and lands in `<repo>/.csgraph.v2/viewer.json`:

```text
$ {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "viewer_state", "arguments": {"body": {"uiTheme": "nzk-dark+"}}}}
<- {"id": 4, "jsonrpc": "2.0", "result": {"content": [{"text": "{\"body\":{\"ok\":true},\"status\":200}", "type": "text"}], "isError": false}}

$ {"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "viewer_state", "arguments": {}}}
<- {"id": 5, "jsonrpc": "2.0", "result": {"content": [{"text": "{\"body\":{\"ok\":true,\"state\":{\"uiTheme\":\"nzk-dark+\"}},\"status\":200}", "type": "text"}], "isError": false}}
```

The generic form of any route, including writes:

```text
$ {"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "api_call", "arguments": {"method": "POST", "path": "/api/reorg", "body": {"action": "plan", "spec": {"group": "single"}}}}}
```

---

## Lifecycle

* **Framing:** newline-delimited JSON-RPC 2.0. One message per line, no
  `Content-Length` headers.
* **stdout is protocol-only.** Not a single diagnostic is printed there — every
  log line (including the engine's `[build-timing]` lines) goes to **stderr**.
  A parsing client that sees non-JSON on stdout should check the harness is not
  merging the two pipes.
* **stdin EOF:** the server stops accepting calls, lets in-flight calls finish
  (up to 60 s), drains the worker threads and exits `0`.
* **SIGTERM:** terminates the process through the default handler; nothing is
  left running.
* **Notifications** (`notifications/initialized`, `notifications/cancelled`)
  produce no response. Unknown *requests* return JSON-RPC `-32601`; malformed
  JSON returns `-32700`; missing `method` returns `-32600`.
* **Protocol negotiation:** the requested `protocolVersion` is echoed when it
  is one of `2025-06-18`, `2025-03-26`, `2024-11-05`; otherwise the server
  answers `2025-06-18`.
* **Never a daemon.** It has no listening socket and no pid file; it is a child
  process of the harness and dies with its stdin.

---

## Verifying the build

```bash
cd "cs viewer builder/rust-only-csviewer"
source ../rust-env.sh
cargo build --offline -p csgraph-core        # clean, ~5–10 s incremental
cargo test  --offline -p csgraph-core        # includes the mcp framing + repo-resolution tests
python3 tools/probe_mcp.py                   # 31 checks, sharded so each unit is <10 s:
                                             #   --shard read 14/14 4.7s
                                             #   --shard concurrency 5/5 2.8s
                                             #   --shard cancel 9/9 5.7s
                                             #   --shard restart 3/3 4.5s
python3 tools/probe_edit_mcp.py              # 76/76 MCP-only checks (42 tools), 3.3 s
python3 tools/probe_multirepo.py             # 10/10 multi-repo checks (repo arg + scoping), 0.07 s
python3 tools/probe_copy_recover.py          # 26/26 durable copy_path (fault + recover + journals), 0.47 s
python3 tools/probe_reshape.py               # 12/12 reformat + functionize checks
python3 tools/probe_reshape_corpus.py        # 7/7 corpus-defect regression fixtures (WS1)
python3 tools/probe_preview.py               # preview_change: read-only, deterministic, refusals, zero writes
tools/with_serve.sh -- node tools/probe_preview.mjs   # /api/preview + the read-only panel
python3 tools/probe_dedupe.py                # dedupe: fixture + _sealed_src + MCP==CLI, ~10 s
python3 tools/probe_validity.py              # Roslyn parse gate; never a false ok
```

A worked `move_decls` transcript. First a **hermetic single-file fixture** run
(the real `NZKNaNimateMeshFolder.cs.nzk` copied alone into a scratch repo, so
every sibling dependency is unique — this is the run `probe_edit_mcp.py`
performs). The dry run shows the class wrapper and the re-qualified siblings in
`destination.content`, and `apply` writes both files:

```text
$ {"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"move_decls","arguments":{"from":"NZK.Core.NZKNaNimateMeshFolder.FolderFor","newfile":"NZK.VF.cs","prefix":"NZK.VF.NS"}}}
<- {"id":7,...,"isError":false}
   # ok:true mode:extract target:{namespace:"NZK.VF.NS",newfile:"NZK.VF.cs",class:"VF",modifiers:"public static partial",created_file:true}
   # selected:[{id:"NZK.Core.NZKNaNimateMeshFolder.FolderFor(string, string)",newFull:"NZK.VF.NS.VF.FolderFor"}]
   # files:["NZK.VF.cs","NZKNaNimateMeshFolder.cs.nzk"]
   # ambiguous:[] unresolved:[] warnings:[] applied:false reindex_required:true

$ {"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"move_decls","arguments":{"from":"NZK.Core.NZKNaNimateMeshFolder.FolderFor","newfile":"NZK.VF.cs","prefix":"NZK.VF.NS","apply":true}}}
<- {"id":8,...,"isError":false}   # applied:true validated:"roslyn" written:["NZK.VF.cs","NZKNaNimateMeshFolder.cs.nzk"] created:["NZK.VF.cs"]

$ {"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"parse_check","arguments":{}}}
<- {"id":9,...,"isError":false}   # the same gate the apply ran internally

$ {"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"index","arguments":{}}}
<- {"id":10,...,"isError":false}   # re-sync the graph with the moved files
```

The applied `NZK.VF.cs` is legal, dialect-correct **and resolving**:

```csharp
namespace NZK.VF.NS{  public static partial class VF{
    /* Project-relative folder for a model's frozen meshes. */
    public static string FolderFor(string modelAssetPath,string avatarName){return NZK.Core.NZKNaNimateMeshFolder.AvatarRoot(NZK.B.NoE(avatarName)?NZK.Core.NZKNaNimateMeshFolder.LeafNameOf(modelAssetPath):avatarName)+"/"+NZK.Core.NZKNaNimateMeshFolder.MeshesFolderName;}
}}
```

**On the full 52-file corpus the same call refuses by default**, because
`AvatarRoot`, `LeafNameOf` and `MeshesFolderName` are each declared on more than
one type there and cannot be qualified without guessing:

```text
$ {... "from":"NZK.Core.NZKNaNimateMeshFolder.FolderFor","newfile":"NZK.VF.cs","prefix":"NZK.VF.NS"}
<- {"ok":false,"error":"3 moved-block dependency name(s) cannot be uniquely resolved (AvatarRoot, LeafNameOf, MeshesFolderName); qualify them, or pass `allow_unresolved:true` to emit with warnings","unresolved":["AvatarRoot","LeafNameOf","MeshesFolderName"]}
```

`allow_unresolved:true` is the explicit escape hatch: it writes anyway, leaves
those siblings bare and records a loud `warnings` entry. Re-running a `from`
whose symbol has moved is refused too — with `near` matches, never a guess:

```text
$ {... "from":"NZK.Core.NZKNaNimateMeshFolder.FolderFor" (after it moved), ...}
<- {"ok":false,"error":"\"NZK.Core.NZKNaNimateMeshFolder.FolderFor\" does not name a declaration (it may have already moved)","near":[{"full":"NZK.Core.NZKNaNimateMeshFolder.PrefabFolderFor","id":"NZK.Core.NZKNaNimateMeshFolder.PrefabFolderFor(string, string)",...}]}
# and the sibling PrefabFolderFor is untouched
```

A second move into the *same, now-existing* destination file reports
`written:["NZK.VF.cs","NZKNaNimateMeshFolder.cs.nzk"]` and `created:[]`: the
origin delete and the destination append are one all-or-nothing commit, so there
is no delete without a write.

The Roslyn gate refuses a candidate the compiler rejects. Extracting
`N.B.Eq(T, T)` from its one-line `namespace N{ public static partial class B{…}}`
(the slice carries the namespace/class opener on the same line) would wrap it
into an invalid shape, so the write is refused before anything is staged:

```text
$ {... "from":"N.B.Eq(T, T)","newfile":"NZK.X3.cs","prefix":"NZK.X.NS","apply":true}
<- {"ok":false,"validated":"roslyn","error":"the commit would leave 2 Roslyn parse diagnostic(s); nothing written (pass `allow_invalid:true` to override)","diagnostics":[{"severity":"error","id":"CS1513","file":"NZK.X3.cs","line":1,"col":…,"message":"} expected"},…],"files":["NZK.X3.cs","Shorthand.cs.nzk"]}
# origin and destination are both untouched; `allow_invalid:true` would write anyway with a warning
```

Declared identifiers stay bare: extracting
`NZK.B.OIC.Has(string, string)` emits
`public static bool Has(string a,string b){ … }` — the parameters `a`/`b` are
**not** rewritten to the same-named member `NZK.S.P.a`, and
`validated:"roslyn"` confirms the candidate parses clean.

A `grep` transcript (the shell one-liner the owner wanted expressible as a
structured call), against `--repo /2tb/@/nzk/git/vrcCS --root _sealed_src`:

```text
$ {"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"grep","arguments":{"pattern":"public static|public class","glob":"NZKNaNimateMeshFolder.cs.nzk"}}}
<- {"id":10,...,"result":{"content":[{"text":"{\"ok\":true,\"pattern\":\"public static|public class\",\"literal\":false,\"case\":true,\"count_only\":false,\"total_matches\":10,\"file_count\":1,\"files\":[{\"file\":\"_sealed_src/NZKNaNimateMeshFolder.cs.nzk\",\"count\":10,\"matches\":[{\"line\":4,\"col\":1,\"text\":\"public static partial class Core\"},{\"line\":6,\"col\":1,\"text\":\"public static class NZKNaNimateMeshFolder{\"},{\"line\":37,\"col\":5,\"text\":\"    public static string FolderFor(string modelAssetPath,string avatarName){...}\"},...]}],\"truncated\":false}","type":"text"}],"isError":false}}

$ {"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"grep","arguments":{"pattern":"TODO|FIXME","count_only":true,"glob":"**/*.nzk"}}}
<- {"id":11,...,"result":{"content":[{"text":"{\"ok\":true,\"pattern\":\"TODO|FIXME\",\"count_only\":true,\"total_matches\":7,\"file_count\":2,\"files\":[{\"file\":\"_sealed_src/ErrorCodes.cs.nzk\",\"count\":2,\"matches\":[]},{\"file\":\"_sealed_src/nzktoolkit_monolith_compilable.cs.nzk\",\"count\":5,\"matches\":[]}],\"truncated\":false}","type":"text"}],"isError":false}}
```

The validity gate through MCP — the same call as `csgraph parse-check`, on the
52-file `_sealed_src` corpus (`--repo <vrcCS> --root _sealed_src`):

```text
$ {"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"parse_check","arguments":{}}}
<- {"id":12,...,"result":{"content":[{"text":"{\"ok\":true,\"toolchain\":\"dotnet-roslyn 4.12.0.0\",\"files\":52,\"diagnostics\":[],\"count\":0,\"skipped\":null}","type":"text"}],"isError":false}}

$ {"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"parse_check","arguments":{"files":["NZKNaNimateMeshFolder.cs.nzk"]}}}
<- {"id":13,...,"isError":false}   # ok:true, files:1, count:0

# no Roslyn toolchain / driver -> a concrete skipped reason, never a false pass:
<- {"ok":false,"toolchain":null,"files":52,"diagnostics":[],"count":0,"skipped":"no Unity/Roslyn toolchain found"}
```

The duplicate-code report through MCP — the same call as `csgraph dedupe`, on
the 52-file `_sealed_src` corpus. `tools/probe_dedupe.py` asserts this body is
**byte-identical** to the CLI `--json` (same `stats`, same cluster ordering):

```text
$ {"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"find_duplicates","arguments":{}}}
<- {"id":14,...,"result":{"content":[{"text":"{\"ok\":true,\"repo\":\"/2tb/@/nzk/git/vrcCS\",\"roots\":[\"_sealed_src\"],\"files\":52,\"methodBodies\":2156,\"units\":1063,\"minLines\":4,\"threshold\":0.8,\"exact\":[{\"count\":4,\"hash\":\"…\",\"members\":[{\"file\":\"…/nzktoolkit_monolith_compilable.cs\",\"line\":8727,\"name\":\"CopyTransitionProperties\",…}]},…],\"near\":[…],\"blocks\":[…],\"stats\":{\"exactClusters\":345,\"nearClusters\":9,\"blockGroups\":945,\"pairComparisons\":24876},\"truncated\":true}","type":"text"}],"isError":false}}

$ {... "arguments":{"limit":5,"min_lines":6} ...}
<- {"id":15,...,"isError":false}   # ok:true, capped exact/blocks arrays, truncated still reported honestly
```

`tools/probe_mcp.py` spawns the built binary, completes the handshake, checks
that `tools/list` declares every route in `core/src/api.rs`, executes every
route through `api_call`, verifies a read and a real on-disk write, proves reads
and `ping` are answered while a 0.4 s `/api/export.json` call is in flight
(with wall times), then closes stdin/SIGTERMs the server and asserts no process
remains.

---

## Troubleshooting

| symptom | cause / fix |
|---|---|
| `csgraph: unknown command "--mcp"` | The MCP mode is newer than the binary on disk. Rebuild: `source ../rust-env.sh && cargo build --offline -p csgraph-core`. |
| Build fails offline | Run from `rust-only-csviewer/` after `source ../rust-env.sh`; the crates are vendored and `--offline` must be used. The MCP server added **no** dependencies (`serde_json` + the already-present `crossbeam-channel`). |
| Client chokes on a non-JSON line | stdout purity. Redirect stderr to a file (`... 2>/tmp/mcp.log`) and inspect; never merge stderr into stdout. |
| "Is it on a port?" | No. `--mcp` is stdio-only — it is not `csviewer --serve` and does not use `tools/with_serve.sh`. There is no URL and no daemon to clean up. |
| `state.indexed == false` | The server was started without `--root`. Call the `index` tool (or `POST /api/index`) to build, or relaunch with `--root <rel>`. |
| First `initialize` is slow | The startup build is synchronous; it waits for the index. With `--root` and a warm `.csgraph.v2` cache it is a load, not a parse. |
| High memory use | `--threads K` keeps `K` index replicas. Use `--threads 1` for a very large corpus. |
| A call times out | Operations are expected to stay under 30 s (a build under 90 s). Check the stderr log for `[build-timing]` lines and the route involved. |

---

## Security

The MCP server is **exactly as powerful as the app**: `api_call` and the typed
tools execute the whole `/api/*` surface, including:

* `index` / `index_cancel` — re-parse and replace the in-memory graph;
* `reorg` with `action=write` — write a reorganised C# tree under **any
  absolute destination** the process can write to;
* `settings` / `viewer_state` — overwrite the harness user's settings file and
  the repo's `.csgraph.v2/viewer.json`;
* everything the HTTP host can do, reachable through one generic tool.

Treat the MCP client (and the harness that spawns it) as fully trusted. Run the
process as a user with no more filesystem access than the project needs, and do
not expose its stdio to an untrusted agent. There is no authentication or
sandbox: the `--repo` path(s) and the process's user account are the only limits.
