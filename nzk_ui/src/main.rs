//! `nzk_ui` — CLI over the `nzk_ui` library.
//!
//! ```text
//! nzk_ui --out shot.png [--size WxH] [--repo PATH]
//!           [--html PATH] [--css PATH]
//!           [--boxes PATH] [--hit X,Y] [--repeat N] [--dump-tree]
//! ```
//!
//! The library entry point (`nzk_ui::NzkUi`) is what an integrator should
//! use: parse+style+layout once, then ask for geometry and repaint on demand.

use std::path::PathBuf;
use std::process::ExitCode;

use nzk_ui::NzkUi;

struct Cli {
    out: Option<PathBuf>,
    size: (u32, u32),
    repo: String,
    html: Option<PathBuf>,
    css: Option<PathBuf>,
    boxes: Option<PathBuf>,
    hit: Option<(f32, f32)>,
    repeat: u32,
    dump_tree: bool,
    status: Option<String>,
    facts: Option<PathBuf>,
    /// `--serve`: stay alive and answer requests on stdin/stdout.
    serve: bool,
    /// Emit the `@frame` paint-checksum row (serve/one-shot).
    token: bool,
    /// Write the exact resolved HTML document (for the Chromium oracle).
    dump_html: Option<PathBuf>,
    /// Write every laid-out node's absolute box as TSV (for box diffing).
    dump_boxes: Option<PathBuf>,
    /// Run the self-tests and exit; the crate's `cargo test` path cannot link
    /// in this environment (see README), so correctness is proven here.
    selftest: bool,
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Correctness checks that must run in a build this environment *can* link.
///
/// The crate's `cargo test` path cannot resolve its vendored dev profile here
/// (see README "Testing in this environment"), so the SHA-256 vectors are run
/// from the binary instead of silently skipped. Exit code is the contract:
/// non-zero means a check failed.
fn selftest() -> anyhow::Result<()> {
    use nzk_ui::sha256;
    let mut checks = 0usize;

    // 1. SHA-256 against the FIPS 180-4 vectors.
    let vectors: &[(&[u8], &str)] = &[
        (b"", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        (b"abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        (
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        ),
    ];
    for (input, want) in vectors {
        let got = sha256::hex(input);
        anyhow::ensure!(got == *want, "sha256({input:?}) = {got}, want {want}");
        checks += 1;
    }
    // 2. The 1,000,000 x 'a' vector (multi-block streaming path).
    let mut h = sha256::Sha256::new();
    for _ in 0..1000 {
        h.update(&[b'a'; 1000]);
    }
    let got = h.finish_hex();
    let want = "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";
    anyhow::ensure!(got == want, "1e6 a's = {got}, want {want}");
    checks += 1;
    // 3. Chunked == one-shot on every tail length (padding/alignment edges).
    let data: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
    for n in 0..=data.len() {
        let one = sha256::hex(&data[..n]);
        let mut h = sha256::Sha256::new();
        let (a, b) = data[..n].split_at(n / 3);
        h.update(a);
        h.update(b);
        anyhow::ensure!(one == h.finish_hex(), "chunked mismatch at len {n}");
        checks += 1;
    }
    // 4. A frame-sized buffer (6.4 MB) hashes through the multi-block path.
    let frame = vec![7u8; 1600 * 1000 * 4];
    let a = sha256::hex(&frame);
    let mut h = sha256::Sha256::new();
    for c in frame.chunks(7919) {
        h.update(c);
    }
    anyhow::ensure!(a == h.finish_hex(), "frame-sized chunked mismatch");
    checks += 1;

    // 5. The `@frame` wire digest is FNV-1a-64 WITH a length fold, 16 lowercase
    // hex chars. The host recomputes this in `digest_rgba`, so a drift here is
    // silently reported as "geometry and paint are different frames".
    let d = sha256::frame_digest(&[0u8; 4]);
    anyhow::ensure!(d.len() == 16, "frame_digest must be 16 hex, got {d:?}");
    anyhow::ensure!(
        d.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "frame_digest must be lowercase hex: {d:?}"
    );
    let plain = {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for _ in 0..4 {
            h ^= 0u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:016x}")
    };
    anyhow::ensure!(d != plain, "frame_digest is missing the length fold");
    anyhow::ensure!(
        sha256::frame_digest(&[1, 2, 3]) != sha256::frame_digest(&[1, 2, 3, 0]),
        "length fold does not separate trailing zeroes"
    );
    let mut pix = vec![0u8; 6400];
    let base = sha256::frame_digest(&pix);
    pix[4321] = 1;
    anyhow::ensure!(base != sha256::frame_digest(&pix), "pixel change not detected");
    checks += 3;

    println!("selftest: OK ({checks} checks)");
    Ok(())
}

fn usage() -> String {
    "nzk_ui — CSS-driven native nzk_ui painter for the CS Viewer\n\
     \n\
     USAGE:\n\
     \x20 nzk_ui [--out PATH] [OPTIONS]\n\
     \n\
     OPTIONS:\n\
     \x20 --out PATH        PNG to write\n\
     \x20 --size WxH        viewport size (default 1600x1000)\n\
     \x20 --repo PATH       repository shown in the top bar (default /tmp/fid-corpus)\n\
     \x20 --html PATH       A/B override: parse this reference document instead\n\
     \x20                   of the Rust-built skeleton (default: Rust skeleton)\n\
     \x20 --css PATH        style.css (default: the real frontend/static/style.css)\n\
     \x20 --boxes PATH      write resolved border-box geometry as TSV ('-' = stdout)\n\
     \x20 --hit X,Y         print the element under that point and exit\n\
     \x20 --repeat N        paint N times in one process and report each timing\n\
     \x20 --status TEXT    override the status-line text (engine data)\n\
     \x20 --facts PATH     JSON corpus facts to render instead of the fixture\n\
     \x20                  (same shape as dom::Facts; see README)\n\
     \x20 --serve          stay resident; one request = one layout pass\n\
     \x20                  answering paint + geometry + hit (see serve.rs)\n\
     \x20 --no-token       omit the @frame paint-checksum row under --serve\n\
     \x20 --selftest       run the built-in correctness checks and exit\n\
     \x20 --dump-tree       print the resolved DOM tree\n\
     \x20 -h, --help        this message\n"
        .to_string()
}

fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
        out: None,
        size: (1600, 1000),
        repo: "/tmp/fid-corpus".to_string(),
        html: None,
        css: None,
        boxes: None,
        hit: None,
        repeat: 1,
        dump_tree: false,
        status: None,
        facts: None,
        serve: false,
        token: true,
        dump_html: None,
        dump_boxes: None,
        selftest: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => cli.out = Some(PathBuf::from(args.next().ok_or("--out needs a value")?)),
            "--size" => {
                let v = args.next().ok_or("--size needs WxH")?;
                cli.size = parse_size(&v).ok_or_else(|| format!("bad --size {v:?}"))?;
            }
            "--repo" => cli.repo = args.next().ok_or("--repo needs a value")?,
            "--html" => cli.html = Some(PathBuf::from(args.next().ok_or("--html needs a value")?)),
            "--css" => cli.css = Some(PathBuf::from(args.next().ok_or("--css needs a value")?)),
            "--boxes" => {
                cli.boxes = Some(PathBuf::from(args.next().ok_or("--boxes needs a value")?))
            }
            "--hit" => {
                let v = args.next().ok_or("--hit needs X,Y")?;
                let (x, y) = v.split_once(',').ok_or("--hit needs X,Y")?;
                cli.hit = Some((
                    x.trim().parse().map_err(|_| "bad --hit x")?,
                    y.trim().parse().map_err(|_| "bad --hit y")?,
                ));
            }
            "--repeat" => {
                cli.repeat = args
                    .next()
                    .ok_or("--repeat needs a count")?
                    .parse()
                    .map_err(|_| "bad --repeat")?
            }
            "--status" => cli.status = Some(args.next().ok_or("--status needs a value")?),
            "--facts" => cli.facts = Some(PathBuf::from(args.next().ok_or("--facts needs a value")?)),
            "--dump-tree" => cli.dump_tree = true,
            "--dump-html" => {
                cli.dump_html = Some(PathBuf::from(args.next().ok_or("--dump-html needs a value")?))
            }
            "--dump-boxes" => {
                cli.dump_boxes =
                    Some(PathBuf::from(args.next().ok_or("--dump-boxes needs a value")?))
            }
            "--serve" => cli.serve = true,
            "--no-token" => cli.token = false,
            "--selftest" => cli.selftest = true,
            "-h" | "--help" => {
                print!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n\n{}", usage())),
        }
    }
    Ok(cli)
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if cli.selftest {
        return match selftest() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[nzk_ui] selftest FAILED: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if cli.serve {
        let opts = nzk_ui::serve::ServeOpts {
            size: cli.size,
            repo: cli.repo.clone(),
            html: cli.html.clone(),
            css: cli.css.clone(),
            token: cli.token,
            hits: cli.hit.into_iter().collect(),
            rects: Vec::new(),
        };
        return match nzk_ui::serve::serve(&opts) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[nzk_ui] serve: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[nzk_ui] {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<()> {
    let fe = NzkUi::frontend_dir();
    // The static skeleton is Rust data now; only an explicit `--html PATH`
    // reads a reference document off disk (the A/B escape hatch).
    let html_path = cli.html.clone();
    let css_path = cli
        .css
        .clone()
        .unwrap_or_else(|| fe.join("static/style.css"));

    let t0 = std::time::Instant::now();
    let raw_html = match &html_path {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?,
        ),
        None => None,
    };
    let css = std::fs::read_to_string(&css_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", css_path.display()))?;
    let mut shell = match (&cli.facts, raw_html.as_deref()) {
        (Some(path), raw) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
            // Exactly the serialized dom::Facts shape, so a host can emit
            // serde_json::to_string(&facts) with no adapter.
            let facts: nzk_ui::Facts = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parse {} as dom::Facts: {e}", path.display()))?;
            eprintln!("[nzk_ui] facts from {}", path.display());
            match raw {
                Some(html) => {
                    NzkUi::from_source_with_facts(html, &css_path, &css, cli.size, &facts)?
                }
                None => NzkUi::from_skeleton_with_facts(&css_path, &css, cli.size, &facts)?,
            }
        }
        (None, raw) => match raw {
            Some(html) => NzkUi::from_source_with_status(
                html,
                &css_path,
                &css,
                cli.size,
                &cli.repo,
                cli.status.as_deref(),
            )?,
            None => NzkUi::from_skeleton_with_status(
                &css_path,
                &css,
                cli.size,
                &cli.repo,
                cli.status.as_deref(),
            )?,
        },
    };
    let load_ms = t0.elapsed().as_millis();
    let to = shell.timings();
    match &html_path {
        Some(path) => eprintln!(
            "[nzk_ui] {} ({}) + {} ({} bytes)",
            path.display(),
            raw_html.as_ref().map(|s| s.len()).unwrap_or(0),
            css_path.display(),
            css.len(),
        ),
        None => eprintln!(
            "[nzk_ui] rust skeleton + {} ({} bytes)",
            css_path.display(),
            css.len(),
        ),
    }
    eprintln!(
        "[nzk_ui] parse+style+layout {load_ms}ms (in-library {}ms)",
        to.parse_style_layout.as_millis()
    );

    if cli.dump_tree {
        shell.print_tree();
    }

    if let Some(path) = &cli.dump_html {
        use blitz_dom::NodeData;
        let doc = shell.document();
        let mut root = None;
        for (_, node) in doc.tree().iter() {
            if let NodeData::Element(el) = &node.data {
                if el.name.local.as_ref() == "html" {
                    root = Some(node);
                    break;
                }
            }
        }
        let html = match root {
            Some(node) => format!("<!doctype html>\n{}", node.outer_html()),
            None => shell.resolved_html().to_string(),
        };
        std::fs::write(path, html)?;
        eprintln!("html (live DOM): {}", path.display());
    }

    if let Some(path) = &cli.dump_boxes {
        use blitz_dom::NodeData;
        let doc = shell.document();
        let mut out = String::from("node\ttag\tid\tclass\tx\ty\tw\th\n");
        for (id, node) in doc.tree().iter() {
            let (tag, idattr, class) = match &node.data {
                NodeData::Element(el) => {
                    let attr = |n: &str| {
                        el.attrs
                            .iter()
                            .find(|a| a.name.local.as_ref() == n)
                            .map(|a| a.value.clone())
                            .unwrap_or_default()
                    };
                    (
                        el.name.local.to_string(),
                        attr("id"),
                        attr("class"),
                    )
                }
                NodeData::AnonymousBlock(_) => ("#anon".to_string(), String::new(), String::new()),
                _ => continue,
            };
            let pos = node.absolute_position(0.0, 0.0);
            let size = node.final_layout.size;
            out.push_str(&format!(
                "{id}\t{tag}\t{idattr}\t{class}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\n",
                pos.x, pos.y, size.width, size.height
            ));
        }
        if path.as_os_str() == "-" {
            print!("{out}");
        } else {
            std::fs::write(path, out)?;
            eprintln!("boxes-all: {}", path.display());
        }
    }

    if let Some(p) = &cli.hit {
        match shell.hit(p.0, p.1) {
            Some(h) => println!(
                "hit {} {} #{} .{}",
                h.node_id,
                h.tag,
                h.id.unwrap_or_default(),
                h.class.unwrap_or_default()
            ),
            None => println!("hit none"),
        }
    }

    if let Some(path) = &cli.boxes {
        let mut out = String::new();
        for (sel, b) in shell.boxes() {
            match b {
                Some(b) => out.push_str(&format!(
                    "{sel}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\n",
                    b.x, b.y, b.w, b.h
                )),
                None => out.push_str(&format!("{sel}\tMISSING\n")),
            }
        }
        // `@scroll` rows carry the engine-clamped scroll offset and maximum for
        // the containers `ui.scroll` named. Five-column rows above are unchanged,
        // so an existing `--boxes` parser is unaffected.
        for r in shell.scroll() {
            out.push_str(&format!(
                "@scroll\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\n",
                r.selector, r.x, r.y, r.max_x, r.max_y
            ));
        }
        if path.as_os_str() == "-" {
            print!("{out}");
        } else {
            std::fs::write(path, out)?;
            eprintln!("boxes: {}", path.display());
        }
    }

    if cli.out.is_none() && cli.repeat <= 1 {
        return Ok(());
    }

    for i in 1..=cli.repeat.max(1) {
        let t = std::time::Instant::now();
        let (rgba, timings) = shell.paint_timed();
        let total = t.elapsed();
        eprintln!(
            "[nzk_ui] paint #{i}: {}ms total (blitz-paint commands {}ms, vello_cpu raster {}ms)",
            total.as_millis(),
            timings.paint_commands.as_millis(),
            timings.raster.as_millis()
        );
        if let Some(path) = &cli.out {
            let (w, h) = shell.size();
            let img = image::RgbaImage::from_raw(w, h, rgba)
                .ok_or_else(|| anyhow::anyhow!("buffer is not a {w}x{h} RGBA image"))?;
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            img.save(path)?;
            if i == 1 {
                println!("screenshot: {} {}x{}", path.display(), w, h);
            }
        }
    }
    Ok(())
}
