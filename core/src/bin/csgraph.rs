//! csgraph CLI — the engine without a GUI.
//!
//! Used for the differential clone check and as a headless way to query a
//! corpus. Every subcommand prints JSON on stdout.
//!
//! `dump-parser` emits the canonical parse + graph document that
//! `tools/oracle_parser.py` returns from the frozen Python engine; the two are
//! compared by `tools/diff_parser.py`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// `csgraph --help` / `csgraph -h` / `csgraph help` — top-level usage. Printed
/// to stdout with exit 0; the server is never started.
const TOP_USAGE: &str = concat!(
    "usage: csgraph [version|help|dump-parser|dump-doc|dump-reorg|dump-compile|parse-check|compile-diff|dedupe|dump|doc|reorg]\n",
    "       csgraph --mcp --repo DIR [--repo DIR]... [--root REL]... [--exclude PAT]... [--threads N]\n",
    "\n",
    "csgraph is the headless csgraph-core engine; every subcommand prints JSON on stdout.\n",
    "\n",
    "Subcommands:\n",
    "  version        print the csgraph-core version\n",
    "  help           print this help\n",
    "  dump-parser    canonical parse + graph document (tools/oracle_parser.py)\n",
    "  dump           canonical clone document (tools/oracle.py)\n",
    "  dump-doc       synthesize one document per node\n",
    "  dump-reorg     plan + emit + verify reorg specs\n",
    "  dump-compile   differential Roslyn report for group=single\n",
    "  parse-check    offline Roslyn parse gate (0 clean, 1 diagnostics, 3 skipped)\n",
    "  compile-diff   baseline vs candidate diagnostics (offline csc)\n",
    "  dedupe         deterministic duplicate-code report\n",
    "  doc            doc synthesis\n",
    "  reorg          reorganisation\n",
    "\n",
    "MCP server (--mcp): stdout is JSON-RPC only; all logs go to stderr.\n",
    "  --repo DIR       repository root (required; repeatable — the first is the\n",
    "                   primary/default, others are bound secondaries; duplicates\n",
    "                   collapse. Every tool also takes an optional `repo` argument)\n",
    "  --root REL       one root to index; repeatable and global (every repo)\n",
    "  --exclude PAT    glob or path to exclude; repeatable and global\n",
    "  --threads N      worker threads (0 or omitted = all cores)\n",
    "  --help, -h       print the MCP usage and exit without starting the server\n",
    "The MCP tool list is exactly what the server advertises via `tools/list`.\n",
);

/// `csgraph --mcp --help` / `csgraph --mcp -h` — MCP usage. Printed to stdout
/// with exit 0, before any argument validation, indexing, or stdio server.
const MCP_USAGE: &str = concat!(
    "usage: csgraph --mcp --repo DIR [--repo DIR]... [--root REL]... [--exclude PAT]... [--threads N]\n",
    "\n",
    "Run the csgraph Model Context Protocol server over stdio.\n",
    "stdout is JSON-RPC only; every log and diagnostic goes to stderr.\n",
    "The available tools are the ones advertised by `tools/list`.\n",
    "\n",
    "Options:\n",
    "  --repo DIR       repository root (required; repeatable — the first is the\n",
    "                   primary/default, every other one is bound as a secondary and\n",
    "                   built on its first call. Duplicate canonical paths collapse)\n",
    "  --root REL       one root to index; repeatable and global (applies to every repo)\n",
    "  --exclude PAT    glob or path to exclude; repeatable and global\n",
    "  --threads N      worker threads (0 or omitted = all cores)\n",
    "  --help, -h       print this usage and exit without starting the server\n",
    "\n",
    "Every tool takes an optional `repo` argument selecting which bound repository the\n",
    "call runs against: a 1-based index (\"2\" or 2), an absolute or cwd-relative path,\n",
    "or a unique repo basename. Omitted = the primary repo. `index`/`index_cancel`\n",
    "apply to the selected repo only; `workspace` lists every bound repo.\n",
);

/// True when the argument is one of the recognised help spellings.
fn is_help_flag(a: &str) -> bool {
    a == "--help" || a == "-h"
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("version") | None => {
            println!("csgraph-core {}", csgraph_core::VERSION);
        }
        Some("help") | Some("--help") | Some("-h") => {
            print!("{TOP_USAGE}");
        }
        Some("--mcp") => {
            // `csgraph --mcp` — a multithreaded Model Context Protocol server
            // over stdio. stdout is protocol-only; all logs go to stderr.
            // A help flag anywhere after `--mcp` prints the MCP usage and exits
            // before `parse_mcp_opts`/`mcp::run`, so no index is built and no
            // stdio server starts.
            if args[1..].iter().any(|a| is_help_flag(a)) {
                print!("{MCP_USAGE}");
                return;
            }
            let opts = match parse_mcp_opts(&args[1..]) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("csgraph --mcp: {e}");
                    eprintln!(
                        "usage: csgraph --mcp --repo DIR [--repo DIR]... \
                         [--root REL]... [--exclude PAT]... [--threads N]"
                    );
                    std::process::exit(2);
                }
            };
            if let Err(e) = csgraph_core::mcp::run(opts) {
                eprintln!("csgraph --mcp: {e}");
                std::process::exit(1);
            }
        }
        Some("dump-parser") => {
            let opts = match parse_opts(&args[1..]) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("csgraph dump-parser: {e}");
                    eprintln!("usage: csgraph dump-parser --repo DIR [--root REL]... [--exclude PAT]...");
                    std::process::exit(2);
                }
            };
            let doc = dump_parser(&opts.repo, &opts.roots, &opts.excludes);
            println!("{}", serde_json::to_string(&doc).unwrap());
        }
        Some("doc") => {
            // Placeholder until the doc module lands.
            println!("{{\"ok\":false,\"error\":\"doc not implemented yet\"}}");
        }
        Some("dump-doc") => {
            // Synthesize one document per node; see csgraph_core::doc.
            if let Err(e) = csgraph_core::doc::cli_dump_doc(&args[1..]) {
                eprintln!("csgraph dump-doc: {e}");
                eprintln!(
                    "usage: csgraph dump-doc --repo DIR [--root REL]... \
                     [--exclude PAT]... [--define SYM]... [--node ID]..."
                );
                std::process::exit(2);
            }
        }
        Some("dump-reorg") => {
            // Plan + emit + verify every reorg spec by default; `--spec KEY`
            // limits a run to named specs so each call stays inside a budget,
            // and `--time` prints per-phase timings to stderr.
            if let Err(e) = csgraph_core::reorg::cli_dump_reorg(&args[1..], None) {
                eprintln!("csgraph dump-reorg: {e}");
                eprintln!(
                    "usage: csgraph dump-reorg --repo DIR [--root REL]... \
                     [--exclude PAT]... [--spec KEY]... [--time]"
                );
                std::process::exit(2);
            }
        }
        Some("dump-compile") => {
            // The differential Roslyn report for `group=single`; see
            // `tools/oracle_compile.py` / `tools/diff_compile.py`.
            if let Err(e) = dump_compile(&args[1..]) {
                eprintln!("csgraph dump-compile: {e}");
                eprintln!(
                    "usage: csgraph dump-compile --repo DIR [--root REL]... \
                     [--exclude PAT]... [--data UNITY_DATA_DIR]"
                );
                std::process::exit(2);
            }
        }
        Some("parse-check") => {
            // Local, offline Roslyn parse gate (M0 "always valid"). Exit code:
            // 0 = clean, 1 = parse diagnostics, 3 = skipped (no false pass).
            match parse_check_cmd(&args[1..]) {
                Ok(0) => {}
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("csgraph parse-check: {e}");
                    eprintln!(
                        "usage: csgraph parse-check --repo DIR [--root REL]... \
                         [--exclude PAT]... [--files PATH]... [--json] [--timeout SECS]"
                    );
                    std::process::exit(2);
                }
            }
        }
        Some("dedupe") => {
            // Deterministic duplicate-code report (exact / near / repeated
            // blocks); see csgraph_core::dedupe.
            if let Err(e) = dedupe_cmd(&args[1..]) {
                eprintln!("csgraph dedupe: {e}");
                eprintln!(
                    "usage: csgraph dedupe --repo DIR [--root REL]... \
                     [--exclude PAT]... [--min-lines N] [--threshold T] [--json]"
                );
                std::process::exit(2);
            }
        }
        Some("dump") => {
            // The full canonical document `tools/oracle.py` emits, from the
            // Rust Index; `tools/diff_clone.py` compares the two.
            if let Err(e) = dump_index(&args[1..]) {
                eprintln!("csgraph dump: {e}");
                eprintln!(
                    "usage: csgraph dump --repo DIR [--root REL]... \
                     [--exclude PAT]... [-o FILE]"
                );
                std::process::exit(2);
            }
        }
        Some("compile-diff") => {
            // Compile-level differential: baseline vs candidate diagnostics via
            // the SDK's own Roslyn csc (offline). Exit 0 = no new diagnostics,
            // 1 = new/worse, 3 = honest `skipped` (no toolchain), 2 = usage.
            match csgraph_core::compile_check::cli_compile_diff(&args[1..]) {
                Ok(0) => {}
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("csgraph compile-diff: {e}");
                    eprintln!(
                        "usage: csgraph compile-diff --baseline DIR... --candidate DIR... \
                         [--define SYM]... [--data DIR] [--timeout SECS]"
                    );
                    std::process::exit(2);
                }
            }
        }
        Some(other) => {
            eprintln!("csgraph: unknown command {other:?}");
            eprintln!("usage: csgraph [version|dump-parser|dump-doc|dump-reorg|dump-compile|parse-check|compile-diff|dedupe|dump|doc|reorg]");
            std::process::exit(2);
        }
    }
}

struct Opts {
    repo: PathBuf,
    roots: Vec<String>,
    excludes: Vec<String>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut repo: Option<PathBuf> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", args[*i - 1]))
        };
        match a {
            "--repo" => repo = Some(PathBuf::from(take(&mut i)?)),
            "--root" => roots.push(take(&mut i)?),
            "--exclude" => excludes.push(take(&mut i)?),
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 1;
    }
    let repo = repo.ok_or("--repo is required")?;
    if roots.is_empty() {
        roots.push("_sealed_src".to_string());
    }
    Ok(Opts {
        repo,
        roots,
        excludes,
    })
}

/// `csgraph --mcp` options: `--repo DIR [--repo DIR]... [--root REL]...
/// [--exclude PAT]... [--threads N]`.
fn parse_mcp_opts(args: &[String]) -> Result<csgraph_core::mcp::McpOptions, String> {
    let mut repos: Vec<PathBuf> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut threads: usize = 0;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", args[*i - 1]))
        };
        match a {
            "--repo" => repos.push(PathBuf::from(take(&mut i)?)),
            "--root" => roots.push(take(&mut i)?),
            "--exclude" => excludes.push(take(&mut i)?),
            "--threads" => {
                let raw = take(&mut i)?;
                threads = raw
                    .parse::<usize>()
                    .map_err(|_| format!("--threads needs a non-negative integer, got {raw:?}"))?;
            }
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 1;
    }
    if repos.is_empty() {
        return Err("--repo is required".to_string());
    }
    // `--threads 0` (and the omitted default, which arrives here as 0) means
    // "all cores", exactly as the usage text promises. Resolve it here: the
    // engine treats any value > 0 as literal, so passing 0 through would clamp
    // to a smaller pool instead.
    if threads == 0 {
        threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
    }
    Ok(csgraph_core::mcp::McpOptions {
        repos,
        roots,
        excludes,
        threads,
    })
}

/// `csgraph dump` — the canonical clone document (see `tools/oracle.py`).
///
/// Defaults mirror the oracle exactly: root `_sealed_src`, the oracle's
/// exclude list, no defines, not a player build.
fn dump_index(args: &[String]) -> Result<(), String> {
    let mut repo: Option<PathBuf> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut exclude_given = false;
    let mut out: Option<PathBuf> = None;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", args[*i - 1]))
        };
        match a {
            "--repo" => repo = Some(PathBuf::from(take(&mut i)?)),
            "--root" => roots.push(take(&mut i)?),
            "--exclude" => {
                exclude_given = true;
                excludes.push(take(&mut i)?);
            }
            "-o" | "--out" => out = Some(PathBuf::from(take(&mut i)?)),
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 1;
    }
    let repo = repo.ok_or("--repo is required")?;
    if roots.is_empty() {
        roots.push("_sealed_src".to_string());
    }
    if !exclude_given {
        // Use the same bounded default set as the `index` route. The previous
        // short fallback omitted `cache`/`target`/`build`/`Library`/..., so
        // `csgraph dump --root .` walked multi-GB trees and blew the budget.
        excludes = csgraph_core::config::Config::default().excludes;
    }
    let cfg = csgraph_core::config::Config {
        roots: roots.clone(),
        excludes: Vec::new(),
        defines: Vec::new(),
        player: false,
        ..Default::default()
    };
    let canonical = roots.len() == 1 && roots[0] == "_sealed_src";
    let started = std::time::Instant::now();
    let mut index = csgraph_core::index::Index::new(repo);
    let build_state = index.build(&roots, &excludes, &cfg);
    // `build` reports a refused discovery (the bounded-walk guard) as
    // `ok:false` + `error` instead of building a partial graph. Surface that as
    // a refusal rather than dumping an empty document.
    if build_state.get("error").is_some() || build_state.get("ok") == Some(&Value::Bool(false)) {
        let why = build_state
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("index build refused");
        return Err(format!("refused: {why}"));
    }
    if !canonical {
        // The canonical clone document (`_sealed_src`) is always allowed: it is
        // the differential gate. Any other root gets a projected-cost refusal,
        // so a broad root returns promptly with a clear message instead of
        // racing the 30 s budget through document synthesis.
        let nodes = index.nodes().len();
        let build_secs = started.elapsed().as_secs_f64();
        let rate_ms: f64 = std::env::var("CSGRAPH_DUMP_NODE_MS")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(4.0);
        let budget: f64 = std::env::var("CSGRAPH_DUMP_BUDGET_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(28.0);
        let projected = build_secs + nodes as f64 * rate_ms / 1000.0;
        if projected > budget {
            let files = build_state
                .get("fileCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            return Err(format!(
                "refused: roots {roots:?} discover {files} files / {nodes} nodes; synthesizing \
                 the canonical document would take ~{projected:.1}s (budget {budget:.1}s at \
                 {rate_ms:.1} ms/node). Narrow --root/--exclude, or raise \
                 CSGRAPH_DUMP_BUDGET_SECS."
            ));
        }
    }
    let doc = index.oracle_dump(&roots);
    let text = serde_json::to_string(&doc).map_err(|e| e.to_string())?;
    match out {
        Some(path) => {
            std::fs::write(&path, format!("{text}\n")).map_err(|e| e.to_string())?;
            eprintln!("wrote {} ({} bytes)", path.display(), text.len());
        }
        None => println!("{text}"),
    }
    Ok(())
}

/// The canonical document also produced by `tools/oracle_parser.py`.
fn dump_parser(repo: &Path, roots: &[String], excludes: &[String]) -> Value {
    let files = csgraph_core::parser::discover_files(repo, roots, excludes);
    // `defines=None` keeps every conditional branch, exactly as the oracle calls
    // `parse_source(path, display, None)`. Files are independent, so this fans
    // out across cores while preserving file order.
    let parses = csgraph_core::parser::parse_files_parallel(&files, repo, None);
    let gb = csgraph_core::graph::build_graph(&parses);

    let mut files_json: Vec<Value> = parses
        .iter()
        .map(|pr| {
            json!({
                "file": pr["file"],
                "types": pr["types"],
                "members": pr["members"],
                "inherits": pr["inherits"],
                "warnings": pr["warnings"],
            })
        })
        .collect();
    files_json.sort_by(|a, b| {
        a["file"]
            .as_str()
            .unwrap_or("")
            .cmp(b["file"].as_str().unwrap_or(""))
    });

    let mut nodes = gb.nodes.clone();
    nodes.sort_by(|a, b| {
        a["id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["id"].as_str().unwrap_or(""))
    });

    let mut edges = gb.edges.clone();
    edges.sort_by(|a, b| {
        let key = |e: &Value| {
            (
                e["source"].as_str().unwrap_or("").to_string(),
                e["target"].as_str().unwrap_or("").to_string(),
                e["kind"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });

    json!({
        "files": files_json,
        "graphWarnings": gb.warnings,
        "nodes": nodes,
        "edges": edges,
    })
}

/// `csgraph dump-compile` — the differential Roslyn report for `group=single`.
///
/// This mirrors `tools/oracle_compile.py` step for step: it rebuilds the
/// corpus with `reorg::corpus_graph` (the same `discover_files` / `parse_source`
/// / `build_graph` path the reorg oracle uses), plans and emits `group=single`,
/// then runs `compile_check::differential` over the indexed texts and the
/// emitted tree. The `ok` override `server.py` applies is mirrored too, so the
/// document is directly comparable with the Python oracle.
fn dump_compile(args: &[String]) -> Result<(), String> {
    let mut repo: Option<String> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut data: Option<String> = None;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", args[*i - 1]))
        };
        match a {
            "--repo" => repo = Some(take(&mut i)?),
            "--root" => roots.push(take(&mut i)?),
            "--exclude" => excludes.push(take(&mut i)?),
            "--data" => data = Some(take(&mut i)?),
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 1;
    }
    let repo = repo.ok_or("--repo is required")?;
    if roots.is_empty() {
        roots.push("_sealed_src".to_string());
    }

    let (nodes, file_texts) = csgraph_core::reorg::corpus_graph(&repo, &roots, &excludes);
    let engine = csgraph_core::reorg::Reorg::new(&nodes, &file_texts, None, false, None);
    let spec = json!({"group": "single"});
    let plan = engine.plan(Some(&spec))?;
    let files = engine.emit(&plan, true);
    let report = engine.verify(&files, None, Some(&plan));

    let defines: Vec<String> = csgraph_core::compile_check::DEFAULT_DEFINES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let mut res = csgraph_core::compile_check::differential(
        &file_texts,
        &files,
        data.as_deref(),
        &defines,
        &[],
        900,
        None,
    );
    let report_ok = report.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let diff_ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if let Value::Object(ref mut m) = res {
        m.insert("ok".to_string(), json!(diff_ok && report_ok));
    }
    println!("{}", serde_json::to_string(&res).map_err(|e| e.to_string())?);
    Ok(())
}

/// `csgraph parse-check` — the local, offline Roslyn parse gate.
///
/// Discovers `*.cs` / `*.cs.nzk` under `--root` (default `_sealed_src`) unless
/// explicit `--files` are given, then runs the Roslyn driver from
/// `compile_check`. Always prints JSON; the exit code distinguishes the three
/// outcomes so a caller can never mistake `skipped` for a pass:
///
/// * `0` — no parse diagnostics (`ok:true`)
/// * `1` — parse diagnostics found (`ok:false`)
/// * `3` — `skipped: <reason>` (`ok:false`, no toolchain/driver)
fn parse_check_cmd(args: &[String]) -> Result<i32, String> {
    let mut repo: Option<PathBuf> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut explicit: Vec<String> = Vec::new();
    let mut timeout: u64 = 25;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", args[*i - 1]))
        };
        match a {
            "--repo" => repo = Some(PathBuf::from(take(&mut i)?)),
            "--root" => roots.push(take(&mut i)?),
            "--exclude" => excludes.push(take(&mut i)?),
            "--files" => {
                let raw = take(&mut i)?;
                for part in raw.split(',') {
                    let part = part.trim();
                    if !part.is_empty() {
                        explicit.push(part.to_string());
                    }
                }
            }
            // stdout is JSON already; the flag is accepted for explicit callers
            "--json" => {}
            "--timeout" => {
                let raw = take(&mut i)?;
                timeout = raw
                    .parse::<u64>()
                    .map_err(|_| format!("--timeout needs a positive integer, got {raw:?}"))?;
            }
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 1;
    }
    let repo = repo.ok_or("--repo is required")?;
    let files: Vec<String> = if explicit.is_empty() {
        if roots.is_empty() {
            roots.push("_sealed_src".to_string());
        }
        csgraph_core::parser::discover_files(&repo, &roots, &excludes)
    } else {
        explicit
            .iter()
            .map(|f| {
                if Path::new(f).is_absolute() {
                    f.clone()
                } else {
                    repo.join(f).to_string_lossy().into_owned()
                }
            })
            .collect()
    };
    let timeout = timeout.clamp(1, 30);
    let report = csgraph_core::compile_check::parse_check(&files, timeout);
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|e| e.to_string())?
    );
    let skipped = !report["skipped"].is_null();
    let ok = report["ok"].as_bool().unwrap_or(false);
    Ok(if skipped {
        3
    } else if ok {
        0
    } else {
        1
    })
}

/// `csgraph dedupe` — the deterministic duplicate-code report.
///
/// Defaults mirror a conservative first slice: root `_sealed_src`,
/// `--min-lines 4`, `--threshold 0.80`. Output is always JSON.
fn dedupe_cmd(args: &[String]) -> Result<(), String> {
    let mut repo: Option<PathBuf> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut opts = csgraph_core::dedupe::DedupeOptions::default();
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", args[*i - 1]))
        };
        match a {
            "--repo" => repo = Some(PathBuf::from(take(&mut i)?)),
            "--root" => roots.push(take(&mut i)?),
            "--exclude" => excludes.push(take(&mut i)?),
            "--min-lines" => {
                let raw = take(&mut i)?;
                let n = raw
                    .parse::<usize>()
                    .map_err(|_| format!("--min-lines needs a positive integer, got {raw:?}"))?;
                if n == 0 {
                    return Err("--min-lines must be at least 1".to_string());
                }
                opts.min_lines = n;
            }
            "--threshold" => {
                let raw = take(&mut i)?;
                let t = raw.parse::<f64>().map_err(|_| {
                    format!("--threshold needs a number between 0 and 1, got {raw:?}")
                })?;
                if !(0.0..=1.0).contains(&t) || t.is_nan() {
                    return Err(format!("--threshold must be within 0..=1, got {raw:?}"));
                }
                opts.threshold = t;
            }
            // stdout is JSON already; accepted for explicit callers
            "--json" => {}
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 1;
    }
    let repo = repo.ok_or("--repo is required")?;
    if roots.is_empty() {
        roots.push("_sealed_src".to_string());
    }
    let report = csgraph_core::dedupe::dedupe(&repo, &roots, &excludes, &opts);
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|e| e.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_mcp_opts;

    fn args(extra: &[&str]) -> Vec<String> {
        let mut v = vec!["--repo".to_string(), ".".to_string()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn threads_zero_means_all_cores_and_negatives_are_rejected() {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        // `--threads 0` resolves to every core, exactly as the usage promises.
        let o = parse_mcp_opts(&args(&["--threads", "0"])).unwrap();
        assert_eq!(o.threads, cores);
        assert!(o.threads >= 1);
        // Omitting the flag carries the same promise.
        let o = parse_mcp_opts(&args(&[])).unwrap();
        assert_eq!(o.threads, cores);
        // An explicit positive value stays literal.
        let o = parse_mcp_opts(&args(&["--threads", "3"])).unwrap();
        assert_eq!(o.threads, 3);
        // Negatives and non-numeric values are still refused.
        assert!(parse_mcp_opts(&args(&["--threads", "-1"])).is_err());
        assert!(parse_mcp_opts(&args(&["--threads", "many"])).is_err());
        // And `--repo` remains required.
        assert!(parse_mcp_opts(&["--threads".to_string(), "1".to_string()]).is_err());
    }
}
