//! `native-csviewer` — the engine-free native CS Viewer fork.
//!
//! ```text
//! native-csviewer [--repo PATH] [--root PATH]...
//!                 [--screenshot PATH] [--size WxH]
//!                 [--control SOCK] [--snap-demo PATH] [--threads N]
//! ```
//!
//! Without `--screenshot` it opens real winit windows and renders with wgpu;
//! panes detach into their own OS windows (F2–F5) and snap/dock (see
//! `app/src/windows.rs`). With `--screenshot` it never opens a window: the same
//! UI is rendered into an offscreen texture and written as a PNG.
//!
//! `--threads N` pins the process to `N` CPUs with `sched_setaffinity`, which
//! is what makes the engine's `available_parallelism()`-sized parse worker pool
//! actually `N` — core hardcodes the worker count, so this is the only way to
//! vary it without editing the engine.

mod actions;
mod app;
mod blocks_peek;
mod chords;
mod control;
mod doc_engine;
mod drawer_wire;
mod edit_args;
mod edit_host;
mod edit_transport;
mod edit_wire;
mod find_wire;
mod form_wire;
mod graphviz;
mod model;
mod outline;
mod picker;
mod pov;
mod preview_wire;
mod render;
mod reorg_wire;
mod search;
mod tokens;
mod typography;
mod view;
mod windows;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

struct Cli {
    repo: Option<PathBuf>,
    roots: Vec<String>,
    screenshot: Option<PathBuf>,
    size: (u32, u32),
    snap_demo: Option<PathBuf>,
    /// Opt-in Unix-socket control channel (`--control PATH`, default off).
    control: Option<PathBuf>,
    /// 0 = all cores (no affinity change).
    threads: usize,
    /// Index, print the definition-token table, then exit (no window).
    tokens: bool,
    /// Print the three addon modules compiled into this binary, then exit.
    modules: bool,
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn usage() -> String {
    "native-csviewer — engine-free graph viewer (winit + wgpu + egui)\n\
     \n\
     USAGE:\n\
     \x20 native-csviewer [OPTIONS]\n\
     \n\
     OPTIONS:\n\
     \x20 --repo PATH        repository/folder to index (default: cwd / $CSVIEWER_REPO)\n\
     \x20 --root PATH        root to index (repeatable; default: the repo's config roots)\n\
     \x20 --screenshot PATH  render offscreen to PNG and exit (no window, no display)\n\
     \x20 --control SOCK     opt-in Unix-socket control channel (one JSON object per\n\
     \x20                    line; ops ping/state/click/quit). Headless mode serves it.\n\
     \x20 --size WxH         screenshot size in pixels (default 1600x1000)\n\
     \x20 --snap-demo PATH   drive detach/move/snap/dock on real windows, write a JSON\n\
     \x20                    transcript to PATH, then exit\n\
     \x20 --tokens           index, print the def2mcpfnnumber token table and the\n\
     \x20                    C# shorthand (ternary/??/?.…) report, then exit\n\
     \x20                    (no window; used by the packaged smoke test)\n\
     \x20 --modules          print the tokenizer / Scratch / nzkide modules this\n\
     \x20                    binary was built with, then exit (no index, no window)\n\
     \x20 --threads N        pin the process to N CPUs (0 = all); the engine's parse\n\
     \x20                    worker pool follows, so this measures index scaling\n\
     \x20 -h, --help         this message\n"
        .to_string()
}

fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
        repo: None,
        roots: Vec::new(),
        screenshot: None,
        size: (1600, 1000),
        snap_demo: None,
        control: None,
        threads: 0,
        tokens: false,
        modules: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--repo" => {
                cli.repo = Some(PathBuf::from(args.next().ok_or("--repo needs a value")?));
            }
            "--root" => {
                cli.roots.push(args.next().ok_or("--root needs a value")?);
            }
            "--screenshot" => {
                cli.screenshot =
                    Some(PathBuf::from(args.next().ok_or("--screenshot needs a value")?));
            }
            "--snap-demo" => {
                cli.snap_demo =
                    Some(PathBuf::from(args.next().ok_or("--snap-demo needs a value")?));
            }
            "--control" => {
                cli.control =
                    Some(PathBuf::from(args.next().ok_or("--control needs a socket path")?));
            }
            "--tokens" => cli.tokens = true,
            "--modules" => cli.modules = true,
            "--threads" => {
                let v = args.next().ok_or("--threads needs a number")?;
                cli.threads = v.parse().map_err(|_| format!("bad --threads {v:?}"))?;
            }
            "--size" => {
                let v = args.next().ok_or("--size needs WxH")?;
                cli.size = parse_size(&v).ok_or_else(|| format!("bad --size {v:?}"))?;
            }
            "-V" | "--version" => {
                // task-21: build stamp from app/src/buildinfo.rs
                println!("{}", crate::app::buildinfo::version_line());
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n\n{}", usage())),
        }
    }
    Ok(cli)
}

/// `--threads N` is **advisory**.
///
/// The engine's parse worker pool is sized by
/// `std::thread::available_parallelism()` inside
/// `csgraph_core::parser::parse_files_parallel`; `Index::build` exposes no
/// thread-count parameter, and core is read-only for this task. Earlier this
/// flag pinned the process with `libc::sched_setaffinity` to make that std call
/// report `N`; that hack was removed for not being worth it. (The owner's rule
/// is *one* permitted `unsafe` C FFI call, reserved for the parallelizer — see
/// the rule in `notes/csviewer-v3.md` — so C FFI is not blanket-forbidden, but
/// an affinity call is not the sanctioned exception.) The flag is still parsed
/// and reported, but it does not
/// change the worker count — use `taskset -c ...` (an external, standard tool)
/// if a CPU-count experiment is needed.
fn worker_note(requested: usize, observed: usize) -> String {
    if requested == 0 {
        format!("effective_workers={observed} (all cores; --threads not given)")
    } else if requested == observed {
        format!("effective_workers={observed} (--threads {requested} matches)")
    } else {
        format!(
            "effective_workers={observed} (--threads {requested} is ADVISORY and had no effect: \
             the engine has no thread-count parameter)"
        )
    }
}

fn main() -> ExitCode {
    // Process start, so the fork's `first-frame-ms` includes indexing.
    let t_proc = std::time::Instant::now();
    let cli = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    if cli.modules {
        // The packaged build's module report: proves all three addons link and
        // prints the exact versions this binary was compiled with.
        println!("v3 toolkit modules:");
        println!("  tokenizer  def2mcpfnnumber {}", def2mcpfnnumber::VERSION);
        println!("  scratch    scratch-rs-ide-addon {}", scratch_rs_ide_addon::VERSION);
        println!("  nzkide     nzkide-module {}", nzkide_module::VERSION);
        println!("  host       native-csviewer {}", crate::app::buildinfo::BUILD);
        return ExitCode::SUCCESS;
    }

    let workers = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1);
    eprintln!("[native-csviewer] {}", worker_note(cli.threads, workers));

    // Decide the repo BEFORE anything indexes. Precedence: `--repo`, then
    // `$CSVIEWER_REPO` (both unchanged), then the persisted last project, then
    // cwd. A no-arg launch must never index cwd first and reopen the saved repo
    // afterwards — that would double-index, and the first frame would already
    // be wrong.
    let env_repo = std::env::var("CSVIEWER_REPO")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let mut restored = false;
    let mut saved_roots: Vec<String> = Vec::new();
    let repo = if let Some(p) = cli.repo.clone() {
        p
    } else if let Some(p) = env_repo {
        p
    } else if let Some((p, roots)) = model::load_last_project() {
        restored = true;
        saved_roots = roots;
        p
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        eprintln!(
            "[native-csviewer] no saved project; indexing cwd {}",
            cwd.display()
        );
        cwd
    };
    let cfg = model::load_config(&repo);
    // `--root` always wins; a restored project reuses its saved roots while they
    // still exist, otherwise falls back to the repo's config/root defaults.
    let roots = if !cli.roots.is_empty() {
        cli.roots.clone()
    } else if restored && !saved_roots.is_empty() {
        let existing: Vec<String> = saved_roots
            .iter()
            .filter(|r| {
                let p = if Path::new(r).is_absolute() {
                    PathBuf::from(r)
                } else {
                    repo.join(r)
                };
                p.exists()
            })
            .cloned()
            .collect();
        if existing.is_empty() {
            model::resolve_roots(&repo, &[], &cfg)
        } else {
            existing
        }
    } else {
        model::resolve_roots(&repo, &cli.roots, &cfg)
    };
    if restored {
        eprintln!(
            "[native-csviewer] restored last project repo={} roots={roots:?}",
            repo.display()
        );
    }
    // Seed/refresh the pointer from the repo actually used. Skipping it for a
    // non-directory `--repo` keeps a typo from replacing a good saved project.
    if repo.is_dir() {
        model::save_last_project(&repo, &roots);
    }
    eprintln!("[native-csviewer] repo={}", repo.display());
    eprintln!("[native-csviewer] roots={roots:?}");

    let t0 = std::time::Instant::now();
    // Keep the engine index: the source pane serves documents from it for the
    // app's whole lifetime (task-16). `load` alone would drop it.
    let (graph, index) = match model::load_with_index(&repo, &roots) {
        Ok((g, ix)) => (g, ix),
        Err(e) => {
            eprintln!("[native-csviewer] index failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "[native-csviewer] indexed in {}ms: {} nodes, {} edges ({} skipped)",
        t0.elapsed().as_millis(),
        graph.nodes.len(),
        graph.edges.len(),
        graph.skipped.iter().map(|(_, c)| *c).sum::<usize>()
    );

    if cli.tokens {
        // The addon is compiled into the shipped binary; this mode is the
        // packaged smoke test: it proves the dependency links and runs on a
        // real corpus without needing a display.
        print!("{}", tokens::report(&graph));
        return ExitCode::SUCCESS;
    }

    let mut gview = view::GraphView::new(graph);

    // Opt-in control channel: no `--control` means no socket and no thread.
    // Bind BEFORE the first render so the path exists as early as possible. The
    // endpoint is installed process-wide so BOTH loops serve it — the headless
    // serve loop below and the windowed winit frame drain (`control::drain_pending`
    // from `App::frame`) — and so `pov::record_json` advertises
    // `"control": "<path>"`, which is the field MCP discovery reads with no
    // explicit path.
    if let Some(sock) = cli.control.as_deref() {
        if let Err(e) = control::start(sock, &gview, cli.screenshot.is_some()) {
            eprintln!("[native-csviewer] control channel failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    if let Some(path) = cli.screenshot {
        let (w, h) = cli.size;
        let t1 = std::time::Instant::now();
        match render::screenshot(&mut gview, w, h, &path) {
            Ok((w, h, bytes)) => {
                println!(
                    "screenshot: {} {}x{} {} bytes ({}ms)",
                    path.display(),
                    w,
                    h,
                    bytes,
                    t1.elapsed().as_millis()
                );
                // Headless control serves between passes, on the same main
                // thread that just painted, so a `click` resolves against the
                // geometry that painted the PNG.
                if control::is_active() {
                    control::serve_headless(&mut gview, std::time::Duration::from_secs(120));
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("[native-csviewer] screenshot failed: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        let opts = app::RunOptions {
            snap_demo: cli.snap_demo,
            start: Some(t_proc),
            corpus: repo.display().to_string(),
        };
        match app::run(gview, index, opts) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[native-csviewer] {e}");
                ExitCode::FAILURE
            }
        }
    }
}

