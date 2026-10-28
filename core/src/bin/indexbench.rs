//! `indexbench` — engine-only index benchmark vehicle.
//!
//! Builds an `Index` over `--repo`/`--root` and exits, so engine phases
//! (`CSGRAPH_BUILD_TIMING=1`) and wall/user/sys can be measured with `taskset`
//! without the GUI app crate. This exists because the fork binary's build is
//! coupled to `app/src`, and because an engine hotspot should be measurable by
//! the engine alone.
//!
//! ```text
//! indexbench --repo DIR [--root REL]... [--quiet]
//! ```

use std::path::PathBuf;

fn main() {
    let mut repo: Option<PathBuf> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut quiet = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--repo" => repo = args.next().map(PathBuf::from),
            "--root" => {
                if let Some(r) = args.next() {
                    roots.push(r);
                }
            }
            "--quiet" => quiet = true,
            "-h" | "--help" => {
                println!("indexbench --repo DIR [--root REL]... [--quiet]");
                return;
            }
            other => {
                eprintln!("indexbench: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    let repo = repo.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if roots.is_empty() {
        roots.push(".".to_string());
    }
    let cfg = csgraph_core::config::load_config(&repo.join("csgraph.config.json"));
    let mut ix = csgraph_core::index::Index::new(repo);
    let t = std::time::Instant::now();
    let state = ix.build(&roots, &cfg.excludes, &cfg);
    let ms = t.elapsed().as_millis();
    if !quiet {
        eprintln!(
            "indexbench: {ms}ms files={} nodes={} edges={}",
            state.get("fileCount").and_then(|v| v.as_u64()).unwrap_or(0),
            state
                .get("counts")
                .and_then(|c| c.get("nodes"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            state
                .get("counts")
                .and_then(|c| c.get("edges"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        );
    }
}
