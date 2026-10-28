//! `nzk_ui --serve` — the persistent-child protocol.
//!
//! One child process stays alive across rebuilds; the host sends one request per
//! rebuild and gets geometry, the painted raster and a correspondence token back
//! from **one** layout pass. That is the whole point: the old path spawned this
//! binary twice (a paint pass and a `--boxes` pass), so geometry and pixels came
//! from two independent layout runs and cost two full parses.
//!
//! # Framing
//!
//! Requests and responses are newline-delimited; the payload is the only
//! multi-line part and is length-prefixed so it never needs escaping.
//!
//! Request, from the host:
//! ```text
//! REQ <WxH> <facts_len> <token:0|1> [HITS=x,y;x,y] [RECTS=k=sel;k=sel] [PROBE=1] <repo...>\n
//! [<facts_len bytes of `dom::Facts` JSON>\n]     <- only when facts_len > 0
//! ```
//!
//! `HITS=`, `RECTS=` and `PROBE=` are optional header fields. `PROBE=1` answers
//! geometry + hits from the cached layout **without painting or PNG-encoding**:
//! the response's `png_len` is 0. The host uses it for press/hover routing so a
//! pointer move is ~1ms instead of a full chrome repaint. The banner advertises
//! the additive capability as `protocol 1 probe`, so an old host that only
//! checks `protocol 1` still accepts the child and a new host can fall back to
//! a repainting request against an old child.
//! `repo` is the last header field and is re-joined from the remainder, so a
//! repository path containing spaces survives. `facts_len == 0` means "use the
//! fixture counts" (the pre-`--facts` behaviour). A `RECTS` selector may not
//! contain spaces (use `>` child combinators) because the field is
//! space-delimited like `HITS`.
//!
//! Response, written to stdout and flushed only when complete:
//! ```text
//! OK <w> <h> <png_len> <tsv_len>\n
//! <png bytes, png_len>\n
//! <tsv text, tsv_len>\n
//! ```
//! or `ERR <len>\n<message>\n`.
//!
//! The TSV is byte-identical to the one-shot `--boxes` output (same
//! `PROBE_SELECTORS` order, same `@scroll` rows) plus:
//! * `@hit\t<x>\t<y>\t<node>\t<tag>\t<id>\t<class>\t<data-key>\t<data-close>\t<data-id>\t<data-nav>\t<ancestors>\t<title>`
//!   (or `...\tnone`; `<title>` — the nearest `title` a browser would show as a
//!   hover tooltip — is the additive thirteenth column),
//! * `@hitbox\t<x>\t<y>\t<bx>\t<by>\t<bw>\t<b h>` — the measured box of the row the
//!   hit routes to (the nearest `data-key`/`data-id`/`data-nav` ancestor, else
//!   the hit element); `none` when the layout has no box. Additive, emitted only
//!   when a request carries `HITS=`, and what the host paints hover feedback on.
//! * `@rect\t<key>\t<data-id>\t<x>\t<y>\t<w>\t<h>` for every element matching a
//!   selector the request's `RECTS` field named, keyed by that field's key and
//!   carrying the nearest `data-id` (empty when the element has none). This is
//!   the **measured geometry channel** for the Blocks canvas: the host asks for
//!   the painted `.port-dot`/`.bc-ports` boxes and anchors its edges to them
//!   instead of to stylesheet constants. `@rect` rows are additive and only
//!   emitted when a request asks for them, so a frame with no `RECTS` field is
//!   byte-identical to the pre-channel output, and
//!
//! The four trailing routing columns are the **nearest** `data-key`/
//! `data-close`/`data-id`/`data-nav` on the hit element or one of its ancestors
//! (`Hit::data_key` / `Hit::data_close` / `Hit::data_id` / `Hit::data_nav`).
//! They are additive — a frame with no hit request emits no
//! `@hit` rows at all, so the paint and the `@frame` token cannot move — and an
//! absent payload is the empty field, never a dropped column. This is the
//! routing payload for the runtime-created `#cs-tabs` bar and folder-picker
//! rows, which have no static box-map entry and can only be answered from the
//! hit chain (`shell.js:1250-1266`, `shell.js:58`). The same literal shape is
//! pinned by `app/src/nzk_ui.rs::shell_hit_rows_carry_the_tab_bar_payload`.
//!
//! The **last** `@hit` column is the ancestor `id`/`class` trail
//! (`Hit::ancestors`): `id@class` tokens joined by `|`, nearest first. The
//! reference resolves clicks with `ev.target.closest(...)`, but a single hit
//! node cannot answer a class match on an ancestor — and the label of a
//! `<button>` is painted as an anonymous inline box with no identity at all, so
//! without this column a press on a button's text (or on a row's child span)
//! was dropped. See `app/src/nzk_ui.rs::hit_chain_from_tsv`.
//! * `@frame\t<w>\t<h>\t<sha256hex|none>` as the final row.
//!
//! Everything human-readable goes to **stderr** so it can never corrupt the
//! stdout framing.
//!
//! # Why the host can trust "one layout pass"
//!
//! `NzkUi` is built once per request and the same `&NzkUi` answers geometry
//! (`NzkUi::boxes`), the scroll report and the raster (`NzkUi::paint_into`).
//! There is no second parse between them, so geometry cannot drift from pixels.
//! The optional `@frame` row carries a checksum of the exact RGBA bytes painted,
//! letting the host prove it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

use anyhow::Context;
use crate::{sha256, NzkUi};

/// Hard ceiling on one frame's PNG, so a corrupt/hostile length can never make
/// the host allocate unbounded memory.
const MAX_PNG: usize = 512 * 1024 * 1024;
/// Hard ceiling on the facts JSON document.
const MAX_FACTS: usize = 64 * 1024 * 1024;

pub struct ServeOpts {
    pub size: (u32, u32),
    pub repo: String,
    pub html: Option<PathBuf>,
    pub css: Option<PathBuf>,
    /// Emit the `@frame` paint-checksum row.
    pub token: bool,
    /// Fallback hit points used when a request does not carry its own
    /// `HITS` line. Per-request points (see [`handle_request`]) always win, so
    /// the host can hit-test arbitrary points without a respawn.
    pub hits: Vec<(f32, f32)>,
    /// Fallback `RECTS` key/selector pairs, used when a request carries no
    /// `RECTS` field. Per-request fields always win, so the host can ask for
    /// different geometry without respawning the child.
    pub rects: Vec<(String, String)>,
}

/// Format one `@hit` row.
///
/// The single owner of the wire shape documented in this module's header: the
/// render loop calls it and `hit_rows_pin_the_thirteen_column_shape` pins it, so
/// the two halves (here and `app/src/nzk_ui.rs::hit_chain_from_tsv`) cannot
/// drift.
///
/// The final column is the ancestor `id`/`class` trail: `id@class` tokens joined
/// by `|`, nearest first. `@` and `|` never occur in an id or a CSS class (both
/// are HTML/CSS identifiers), so the encoding cannot be ambiguous and needs no
/// escaping. An ancestor with only one of the two keeps the empty half (`@row`
/// for a class-only row container, `nodeProps@` for an id-only one); an ancestor
/// with neither was an anonymous box and is not emitted at all.
///
/// The **thirteenth** column is the nearest `title` — the text a browser would
/// draw as its own hover tooltip. It is appended AFTER the ancestor trail, so an
/// older host that reads eleven columns keeps parsing the same fields, and a
/// newer host reads the title only when the child actually sent one (`get(11)`).
/// Tabs, CR and LF are collapsed to single spaces because the row is TSV: a
/// tooltip is prose and may be wrapped across lines in the document, and a raw
/// newline would split one `@hit` row into two.
fn hit_row(x: f32, y: f32, h: &crate::Hit) -> String {
    let ancestors = h
        .ancestors
        .iter()
        .map(|a| {
            format!(
                "{}@{}",
                a.id.as_deref().unwrap_or_default(),
                a.class.as_deref().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    let title = h
        .title
        .as_deref()
        .unwrap_or_default()
        .chars()
        .map(|c| if matches!(c, '\t' | '\r' | '\n') { ' ' } else { c })
        .collect::<String>();
    format!(
        "@hit\t{:.2}\t{:.2}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        x,
        y,
        h.node_id,
        h.tag,
        h.id.as_deref().unwrap_or_default(),
        h.class.as_deref().unwrap_or_default(),
        h.data_key.as_deref().unwrap_or_default(),
        h.data_close.as_deref().unwrap_or_default(),
        h.data_id.as_deref().unwrap_or_default(),
        h.data_nav.as_deref().unwrap_or_default(),
        ancestors,
        title,
    )
}

/// Format the measured box of the element a hit routes to (`@hitbox`).
///
/// This is additive to the `@hit` contract: it answers the one
/// question that contract cannot, namely *where the clickable row is* when the
/// pointer landed on a child label. `None` means the layout could not report a
/// box, never an invented rectangle.
fn hitbox_row(x: f32, y: f32, h: &crate::Hit) -> String {
    match h.target_rect {
        Some(b) => format!(
            "@hitbox\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\n",
            x, y, b.x, b.y, b.w, b.h
        ),
        None => format!("@hitbox\t{:.2}\t{:.2}\tnone\n", x, y),
    }
}

/// Format one `@rect` row.
///
/// The single owner of the measured-geometry wire shape documented in this
/// module's header. The host half is `app/src/blocks_wire.rs::geometry_from_tsv`;
/// both sides pin the literal in a test, so the two cannot drift.
fn rect_row(key: &str, data_id: &str, b: &crate::Box2) -> String {
    format!(
        "@rect\t{key}\t{data_id}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\n",
        b.x, b.y, b.w, b.h
    )
}

/// Read exactly `n` bytes, returning an error on early EOF.
fn read_exact_vec(r: &mut impl Read, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// The handshake the host reads once at startup to know the child is ready and
/// speaking this protocol version.
pub fn banner() -> String {
    format!(
        "nzk_ui {} protocol 1 probe\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// Serve requests until stdin closes or `QUIT` arrives.
///
/// Returns `Ok(())` on a clean shutdown. Every error is reported on the wire as
/// `ERR` and then the loop *continues*, so one bad request cannot desync the
/// stream or kill a host that is still using the child.
pub fn serve(opts: &ServeOpts) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let stdin = std::io::stdin();
    let mut inp = BufReader::new(stdin.lock());

    // Read the real sources once. They do not change between rebuilds in the
    // app's lifetime, and re-reading them per request would reintroduce I/O the
    // whole change is meant to remove.
    //
    // The static skeleton is built in Rust now, so `index.html` is NOT read by
    // default; an explicit `--html PATH` keeps the A/B escape hatch of
    // rendering an alternative reference document.
    let fe = NzkUi::frontend_dir();
    let css_path = opts
        .css
        .clone()
        .unwrap_or_else(|| fe.join("static/style.css"));
    let raw_html = match &opts.html {
        Some(html_path) => Some(
            std::fs::read_to_string(html_path)
                .with_context(|| format!("read {}", html_path.display()))?,
        ),
        None => None,
    };
    let css = std::fs::read_to_string(&css_path)
        .with_context(|| format!("read {}", css_path.display()))?;

    // Handshake: the host blocks on this line, so it learns about a missing
    // font/stylesheet failure at spawn time rather than on the first frame.
    write!(out, "{}", banner())?;
    out.flush()?;

    let mut line = String::new();
    let mut frames: u64 = 0;
    // The parsed+styled+laid-out document, reused across requests. This is the
    // whole reason the child is persistent: `NzkUi::build` costs ~33ms
    // (html5ever parse -> Stylo cascade -> Taffy layout) while a repaint of an
    // unchanged document is ~8-9ms. Without this cache a resident child still
    // pays the parse per request and saves only the ~1ms spawn.
    let mut cache: Option<CacheEntry> = None;
    loop {
        line.clear();
        // EOF here is the normal shutdown path: the host closed our stdin.
        match inp.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(anyhow::anyhow!("read request: {e}")),
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "QUIT" {
            return Ok(());
        }
        // `PING` lets the host check liveness without paying for a layout.
        if trimmed == "PING" {
            write!(out, "PONG {frames}\n")?;
            out.flush()?;
            continue;
        }
        match handle_request(
            trimmed,
            &mut inp,
            opts,
            raw_html.as_deref(),
            &css_path,
            &css,
            &mut cache,
        ) {
            Ok((w, h, png, tsv)) => {
                write!(out, "OK {w} {h} {} {}\n", png.len(), tsv.len())?;
                out.write_all(&png)?;
                out.write_all(b"\n")?;
                out.write_all(tsv.as_bytes())?;
                out.write_all(b"\n")?;
                out.flush()?;
                frames += 1;
            }
            Err(e) => {
                // Never leave the host waiting: always answer.
                let msg = format!("{e:#}");
                write!(out, "ERR {}\n{}\n", msg.len(), msg)?;
                out.flush()?;
                eprintln!("[nzk_ui] request failed: {msg}");
            }
        }
    }
}

/// A laid-out document plus the request inputs that produced it.
struct CacheEntry {
    shell: NzkUi,
    /// Fingerprint of the `Facts` document this layout was built from. Using a
    /// digest keeps `dom::Facts` free of a `PartialEq` requirement (it is owned
    /// by another task) while still forcing a rebuild on any real change.
    facts_key: u64,
    size: (u32, u32),
}

/// A stable fingerprint of a facts document. The JSON is already canonical
/// enough here (it is produced by one `serde_json::to_string` in the host), and
/// a digest is only used to decide *whether* to reuse a layout, never to prove
/// correctness of one.
fn facts_key(facts: &crate::Facts) -> u64 {
    let text = serde_json::to_string(facts).unwrap_or_default();
    // FNV-1a: adequate for change detection, no dependency needed.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Parse one request and answer it from ONE layout pass.
fn handle_request(
    header: &str,
    inp: &mut impl BufRead,
    opts: &ServeOpts,
    raw_html: Option<&str>,
    css_path: &std::path::Path,
    css: &str,
    cache: &mut Option<CacheEntry>,
) -> anyhow::Result<(u32, u32, Vec<u8>, String)> {
    // Header layout — everything on ONE line, by design:
    //   REQ <WxH> <facts_len> <token:0|1> [HITS=x,y;x,y] [RECTS=k=sel;k=sel] <repo...>
    // `repo` is LAST and re-joined from the remainder, because a repository path
    // may contain spaces (the fork's own repo root contains one). Every optional
    // field is `name=value` so it is recognisable without counting positions.
    //
    // The split is FIVE fields, not six: the fifth is the whole tail (optional
    // fields followed by the repo), and `rest` peels it. `splitn(6, ' ')` handed
    // `rest` only the fifth TOKEN, so a request carrying `HITS=`/`RECTS=` parsed
    // them but truncated a repo containing a space to its first word, and a
    // request without optional fields lost everything after the repo's first
    // word. The repo is only consulted for the fixture body (facts_len == 0), so
    // that truncation was invisible in production and still a lie in the
    // protocol; four fields + the remainder is what the documented layout means.
    let mut it = header.splitn(5, ' ');
    let verb = it.next().unwrap_or_default();
    if verb != "REQ" {
        anyhow::bail!("bad request verb {verb:?} (expected REQ)");
    }
    let size_s = it.next().ok_or_else(|| anyhow::anyhow!("missing size"))?;
    let facts_len: usize = it
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing facts_len"))?
        .parse()
        .context("bad facts_len")?;
    let token = match it.next() {
        Some("1") => true,
        Some("0") => false,
        Some(other) => anyhow::bail!("bad token flag {other:?}"),
        None => opts.token,
    };
    // The remaining tail is `[HITS=...] [RECTS=...] <repo>`; peel the known
    // `name=value` prefixes off one at a time so a repo containing the literal
    // text `HITS=` or `RECTS=` is still safe. Neither value may contain a space
    // (selectors use `>` instead of descendant combinators), which is what makes
    // this split unambiguous.
    let mut rest = it.next().unwrap_or("").trim_end();
    let mut hit_field = None;
    let mut rect_field = None;
    let mut probe = false;
    loop {
        if let Some(r) = rest.strip_prefix("HITS=") {
            let (value, tail) = match r.split_once(' ') {
                Some((v, t)) => (v, t.trim_start()),
                None => (r, ""),
            };
            hit_field = Some(value);
            rest = tail;
        } else if let Some(r) = rest.strip_prefix("RECTS=") {
            let (value, tail) = match r.split_once(' ') {
                Some((v, t)) => (v, t.trim_start()),
                None => (r, ""),
            };
            rect_field = Some(value);
            rest = tail;
        } else if let Some(r) = rest.strip_prefix("PROBE=") {
            let (value, tail) = match r.split_once(' ') {
                Some((v, t)) => (v, t.trim_start()),
                None => (r, ""),
            };
            probe = matches!(value, "1" | "true");
            rest = tail;
        } else {
            break;
        }
    }
    let repo = rest;

    if facts_len > MAX_FACTS {
        anyhow::bail!("facts_len {facts_len} exceeds {MAX_FACTS}");
    }
    let (w, h) = size_s
        .split_once(['x', 'X'])
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .ok_or_else(|| anyhow::anyhow!("bad size {size_s:?}"))?;

    // The facts body follows the header line, exactly `facts_len` bytes plus a
    // newline delimiter.
    let facts: crate::Facts = if facts_len > 0 {
        let body = read_exact_vec(inp, facts_len).context("read facts body")?;
        if let Ok(path) = std::env::var("CSVIEWER_DUMP_FACTS") {
            let _ = std::fs::write(&path, &body);
        }
        // The host always writes exactly one `\n` after the facts body. Read it
        // unconditionally: peeking with `fill_buf()` would block until the host
        // sent *more* input, and the host is at that moment blocked reading our
        // response — a deadlock. The declared length is the framing; this byte
        // is only a terminator.
        let mut nl = [0u8; 1];
        inp.read_exact(&mut nl).context("consume facts terminator")?;
        serde_json::from_slice(&body).context("parse facts JSON")?
    } else {
        let mut f = crate::Facts::for_corpus(repo);
        f.status_override = None;
        f
    };

    // Optional per-request hit points, carried IN THE HEADER as a trailing
    // `HITS=x,y;x,y` field.
    //
    // This must not be a separate line read with `fill_buf()`: the host sends
    // one request and then blocks reading the response, so any attempt to peek
    // for a line the host has not sent yet deadlocks both processes. Keeping
    // every field on the single header line means the child reads exactly what
    // the host wrote and never waits for more.
    let mut hits = opts.hits.clone();
    if let Some(list) = hit_field {
        hits = list
            .split(';')
            .filter_map(|p| {
                let (a, b) = p.split_once(',')?;
                Some((a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?))
            })
            .collect();
    }

    // Per-request measured-geometry selectors, in the same header field style as
    // `HITS` (`RECTS=key=selector;key=selector`). Empty ⇒ use the spawn-time
    // fallback, which the CLI leaves empty, so a plain `--serve` frame emits no
    // `@rect` rows and stays byte-identical.
    let rects: Vec<(String, String)> = match rect_field {
        Some(list) => list
            .split(';')
            .filter_map(|p| {
                let (k, sel) = p.split_once('=')?;
                (!k.is_empty() && !sel.is_empty()).then(|| (k.to_string(), sel.to_string()))
            })
            .collect(),
        None => opts.rects.clone(),
    };

    // ---- the one and only layout pass, or a reuse of the cached one ----
    //
    // Reuse is safe exactly when the inputs that shaped the document are
    // unchanged. `Facts` is compared structurally (it is serde/PartialEq), so a
    // real corpus change forces a rebuild rather than silently serving a stale
    // frame. A size-only change is cheaper still: `NzkUi::resize` re-runs
    // style+layout without re-parsing the HTML (`NzkUi` keeps the resolved DOM).
    let fk = facts_key(&facts);
    let reuse = matches!(cache, Some(c) if c.facts_key == fk && c.size == (w, h));
    let resize_only = !reuse && matches!(cache, Some(c) if c.facts_key == fk);
    if reuse {
        // nothing to do: the cached document is already correct
    } else if resize_only {
        if let Some(c) = cache.as_mut() {
            c.shell.resize(w, h);
            c.size = (w, h);
        }
    } else {
        let shell = match raw_html {
            Some(html) => NzkUi::from_source_with_facts(html, css_path, css, (w, h), &facts)?,
            None => NzkUi::from_skeleton_with_facts(css_path, css, (w, h), &facts)?,
        };
        *cache = Some(CacheEntry {
            shell,
            facts_key: fk,
            size: (w, h),
        });
    }
    let entry = cache.as_mut().expect("cache populated above");
    let shell: &NzkUi = &entry.shell;
    // Distinguish "paid for layout this request" from "reused", so the host's
    // timing log cannot be read as if every frame cost a full parse. The
    // cached `NzkUi` keeps the timings of the build that produced it, so a
    // reused frame must not report that stale figure as its own cost.
    let (stage, parse_ms) = if reuse {
        ("reuse", 0u128)
    } else if resize_only {
        ("resize", shell.timings().parse_style_layout.as_millis())
    } else {
        ("layout", shell.timings().parse_style_layout.as_millis())
    };

    // Geometry + scroll, from that same NzkUi.
    let mut tsv = String::new();
    for (sel, b) in shell.boxes() {
        match b {
            Some(b) => tsv.push_str(&format!(
                "{sel}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\n",
                b.x, b.y, b.w, b.h
            )),
            None => tsv.push_str(&format!("{sel}\tMISSING\n")),
        }
    }
    for r in shell.scroll() {
        tsv.push_str(&format!(
            "@scroll\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\n",
            r.selector, r.x, r.y, r.max_x, r.max_y
        ));
    }
    // Hit-testing is answered in-process from the same NzkUi, so the host never
    // needs a third child invocation for it.
    for (x, y) in &hits {
        match shell.hit(*x, *y) {
            Some(h) => {
                tsv.push_str(&hit_row(*x, *y, &h));
                tsv.push_str(&hitbox_row(*x, *y, &h));
            }
            None => tsv.push_str(&format!("@hit\t{:.2}\t{:.2}\tnone\n", x, y)),
        }
    }
    // Measured per-element rectangles, from the same layout pass. Each row is
    // keyed by the caller's `RECTS` key and carries the nearest `data-id`, so the
    // Blocks model can map a `.port-dot` row back to its card without the host
    // having to encode every card id into a selector.
    for (key, selector) in &rects {
        for m in shell.rects_of(selector) {
            tsv.push_str(&rect_row(
                key,
                m.data_id.as_deref().unwrap_or_default(),
                &m.rect,
            ));
        }
    }

    // A PROBE request answers geometry + hit-testing only. The layout is the
    // same cached one that painted the frame on screen (the facts replay is what
    // makes that true), so a press or a hover no longer pays to repaint and
    // PNG-encode the whole chrome just to learn what is under the pointer.
    if probe {
        tsv.push_str(&format!("@frame\t{w}\t{h}\tnone\n"));
        eprintln!(
            "[nzk_ui] probe {}x{} stage={} parse+layout {}ms hits {}",
            w, h, stage, parse_ms, hits.len()
        );
        return Ok((w, h, Vec::new(), tsv));
    }

    let t0_paint = std::time::Instant::now();
    let rgba = shell.paint();
    if rgba.len() != (w as usize) * (h as usize) * 4 {
        anyhow::bail!(
            "paint returned {} bytes, expected {}",
            rgba.len(),
            (w as usize) * (h as usize) * 4
        );
    }
    let paint_ms = t0_paint.elapsed().as_millis();

    // The token is over the EXACT bytes encoded into the PNG below, so a host
    // that decodes the PNG and hashes it gets the same value. The function is
    // FNV-1a-64 to match the host's `digest_rgba` — see `sha256::frame_digest`.
    if token {
        tsv.push_str(&format!(
            "@frame\t{w}\t{h}\t{}\n",
            sha256::frame_digest(&rgba)
        ));
    } else {
        tsv.push_str(&format!("@frame\t{w}\t{h}\tnone\n"));
    }

    let img = image::RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| anyhow::anyhow!("buffer is not a {w}x{h} RGBA image"))?;
    let t0_png = std::time::Instant::now();
    let mut png: Vec<u8> = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encode PNG")?;
    if png.len() > MAX_PNG {
        anyhow::bail!("PNG {} bytes exceeds {MAX_PNG}", png.len());
    }
    let png_ms = t0_png.elapsed().as_millis();

    eprintln!(
        "[nzk_ui] frame {}x{} stage={} parse+layout {}ms paint {}ms png_encode {}ms png {}B",
        w,
        h,
        stage,
        parse_ms,
        paint_ms,
        png_ms,
        png.len()
    );
    Ok((w, h, png, tsv))
}

#[cfg(test)]
mod tests {
    /// The banner names the additive probe capability, and an old host that
    /// only checks for `protocol 1` still accepts it.
    #[test]
    fn banner_advertises_the_additive_probe_capability() {
        let b = super::banner();
        assert!(b.starts_with("nzk_ui "));
        assert!(b.contains("protocol 1"));
        assert!(b.contains(" probe"));
        assert!(b.ends_with('\n'));
    }

    /// `@hitbox` carries the measured row (not the label) and an honest `none`
    /// when the layout has no box.
    #[test]
    fn hitbox_rows_carry_the_measured_row_box() {
        let h = crate::Hit {
            node_id: 7,
            target_rect: Some(crate::Box2 { x: 14.0, y: 30.0, w: 220.5, h: 18.0 }),
            tag: "button".into(),
            id: None,
            class: Some("rel-node".into()),
            data_key: Some("n:1".into()),
            data_close: None,
            data_id: None,
            data_nav: None,
            title: None,
            ancestors: vec![],
        };
        assert_eq!(
            super::hitbox_row(10.0, 20.0, &h),
            "@hitbox\t10.00\t20.00\t14.00\t30.00\t220.50\t18.00\n"
        );
        let mut none = h.clone();
        none.target_rect = None;
        assert_eq!(
            super::hitbox_row(10.0, 20.0, &none),
            "@hitbox\t10.00\t20.00\tnone\n"
        );
    }

    /// The `@hit` row is a wire contract with `app/src/nzk_ui.rs`'s
    /// `hits_from_tsv` / `hit_chain_from_tsv`; both sides pin this exact literal.
    /// The payload columns are additive: the first ten fields are the round-7
    /// shape; the tenth (`data-id`, round 9) is the suggestion-row commit
    /// payload, the eleventh (`data-nav`, folder picker) is the row's absolute
    /// folder path, the twelfth is the ancestor `id`/`class` trail a
    /// `closest()`-style resolver needs, and the thirteenth is the nearest
    /// `title` (the browser tooltip text), appended last so an older host still
    /// reads every earlier column at the same index.
    #[test]
    fn hit_rows_pin_the_thirteen_column_shape() {
        let h = crate::Hit {
            node_id: 311,
            target_rect: None,
            tag: "button".into(),
            id: None,
            class: Some("cs-tab-x".into()),
            data_key: None,
            data_close: Some("node:A".into()),
            data_id: None,
            data_nav: None,
            title: None,
            ancestors: vec![],
        };
        assert_eq!(
            super::hit_row(795.0, 40.0, &h),
            "@hit\t795.00\t40.00\t311\tbutton\t\tcs-tab-x\t\tnode:A\t\t\t\t\n"
        );
        let sug = crate::Hit {
            node_id: 412,
            target_rect: None,
            tag: "button".into(),
            id: None,
            class: Some("sug".into()),
            data_key: None,
            data_close: None,
            data_id: Some("NZK.A.m".into()),
            data_nav: None,
            title: None,
            ancestors: vec![],
        };
        assert_eq!(
            super::hit_row(320.0, 92.0, &sug),
            "@hit\t320.00\t92.00\t412\tbutton\t\tsug\t\t\tNZK.A.m\t\t\t\n"
        );
        let pick = crate::Hit {
            node_id: 77,
            target_rect: None,
            tag: "button".into(),
            id: None,
            class: Some("pick".into()),
            data_key: None,
            data_close: None,
            data_id: None,
            data_nav: Some("/home/u/Assets".into()),
            title: None,
            ancestors: vec![],
        };
        assert_eq!(
            super::hit_row(10.0, 20.0, &pick),
            "@hit\t10.00\t20.00\t77\tbutton\t\tpick\t\t\t\t/home/u/Assets\t\t\n"
        );
        // An anonymous inline box (a button's label): the hit itself has no
        // identity, and the trail is the only place the enclosing control's
        // class exists. `@`-joined halves keep a class-only container (`@row`)
        // distinct from an id-only one (`nodeProps@`).
        let label = crate::Hit {
            node_id: 842,
            target_rect: None,
            tag: "div".into(),
            id: None,
            class: None,
            data_key: Some("act:expand:#ns:VF.Inspector".into()),
            data_close: None,
            data_id: None,
            data_nav: None,
            title: None,
            ancestors: vec![
                crate::HitAncestor {
                    id: None,
                    class: Some("btn btn-ghost prop-action".into()),
                },
                crate::HitAncestor {
                    id: Some("nodeProps".into()),
                    class: None,
                },
            ],
        };
        assert_eq!(
            super::hit_row(49.0, 228.0, &label),
            "@hit\t49.00\t228.00\t842\tdiv\t\t\tact:expand:#ns:VF.Inspector\t\t\t\t@btn btn-ghost prop-action|nodeProps@\t\n"
        );
        // The thirteenth column is prose, so tabs/newlines collapse to spaces:
        // a wrapped title must not split one TSV row into two.
        let titled = crate::Hit {
            node_id: 901,
            target_rect: None,
            tag: "span".into(),
            id: None,
            class: Some("flag".into()),
            data_key: None,
            data_close: None,
            data_id: None,
            data_nav: None,
            title: Some("degree\n(number of neighbours)\t".into()),
            ancestors: vec![],
        };
        assert_eq!(
            super::hit_row(12.0, 34.0, &titled),
            "@hit\t12.00\t34.00\t901\tspan\t\tflag\t\t\t\t\t\tdegree (number of neighbours) \n"
        );
    }

    /// The `@rect` row is the measured-geometry contract with
    /// `app/src/blocks_wire.rs::geometry_from_tsv`: seven tab-separated fields,
    /// two decimals, and an empty `data-id` field (never a dropped column) when
    /// the element has no `data-id` ancestor.
    #[test]
    fn rect_rows_pin_the_seven_column_shape() {
        let b = crate::Box2 { x: 12.5, y: 20.0, w: 232.0, h: 167.0 };
        assert_eq!(
            super::rect_row("card", "T:Demo.Widget", &b),
            "@rect\tcard\tT:Demo.Widget\t12.50\t20.00\t232.00\t167.00\n"
        );
        assert_eq!(
            super::rect_row("origin", "", &b),
            "@rect\torigin\t\t12.50\t20.00\t232.00\t167.00\n"
        );
    }
}
