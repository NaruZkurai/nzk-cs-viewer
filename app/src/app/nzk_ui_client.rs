//! The host half of the `nzk_ui --serve` protocol.
//!
//! # Why this exists
//!
//! The original integration spawned `nzk_ui` **twice per nzk_ui rebuild** —
//! once for the PNG and once for `--boxes` — and each spawn re-parsed
//! `index.html`, re-ran the Stylo cascade and re-ran Taffy layout. Measured on
//! this machine at 1600x1000:
//!
//! | step | cost |
//! |---|---|
//! | process spawn (`--help`, no work) | ~1ms |
//! | parse + style + layout | ~33ms |
//! | warm repaint of an unchanged document | ~8-11ms |
//! | **one-shot `--out` (spawn+parse+paint+encode)** | **~50ms** |
//! | **one-shot `--boxes` (spawn+parse+serialise)** | **~34ms** |
//!
//! So the two-spawn path cost ~84ms of which only ~11ms was actual rendering:
//! the rest was paying for the same parse twice, in two processes. And because
//! geometry came from a *different layout pass* than the pixels, the two could
//! disagree — the drift the D2 guard exists to catch.
//!
//! This client keeps ONE child alive and sends it one request per rebuild. The
//! child answers geometry, the scroll report, the paint and a checksum of the
//! painted bytes **from a single layout pass** (`nzk_ui --serve`), so:
//!
//! * the second spawn disappears entirely (~34ms saved),
//! * the re-parse is amortised (~33ms saved on every frame after the first),
//! * geometry and pixels are the same frame *by construction*, not by luck.
//!
//! # Orphan safety
//!
//! A long-lived child is a leak waiting to happen, so every path converges on
//! the child dying:
//!
//! 1. **In-band shutdown.** [`NzkUiClient::shutdown`] sends `QUIT`, then drops
//!    the stdin handle.
//! 2. **stdin EOF, the load-bearing path.** The child's request loop returns as
//!    soon as a read on stdin returns 0. The host holds the only write end, so
//!    closing it — including implicitly, when the host process dies for *any*
//!    reason, because the kernel closes every descriptor — terminates the child.
//!    This is what makes the child safe against `SIGKILL`, a panic, or the OOM
//!    killer, none of which run `Drop`.
//! 3. **Own process group.** The child is spawned with `process_group(0)`, so it
//!    never receives the host terminal's `SIGINT`/`SIGTSTP` as part of the
//!    host's group and cannot be left job-controlled and orphaned.
//! 4. **`Drop`** additionally sends `QUIT` and reaps, so the ordinary
//!    end-of-process path is clean rather than relying on 2 alone.
//!
//! This module is deliberately **`unsafe`-free and FFI-free**. The project rule
//! permits exactly ONE `unsafe` C FFI call in the whole tree (the parallelizer),
//! and that budget is already spent elsewhere, so nothing here may take it. A
//! `prctl(PR_SET_PDEATHSIG)` guard would need FFI, so it is *not* used: parent
//! death is detected through the stdin pipe instead, which is ordinary `std`.
//! The trade-off is that a child whose stdin is held open by a *third* process
//! could outlive the host; nothing in this app does that, and
//! `tools/serve-proto.py` verifies the ordinary path.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Parsed response to one request.
#[derive(Debug)]
pub struct Frame {
    pub png: Vec<u8>,
    pub tsv: String,
    pub width: u32,
    pub height: u32,
}

/// A live `nzk_ui --serve` child plus the pipes to talk to it.
pub struct NzkUiClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    bin: PathBuf,
    /// Requests served by this child, for the rebuild log.
    pub frames: u64,
    /// The child advertises the additive `PROBE=` field in its banner. Probing
    /// answers hit-testing from the cached layout without repainting/encoding a
    /// frame, which is what keeps a press (and hover) from paying for a second
    /// full chrome render.
    probe_support: bool,
}

/// Hard cap on a frame's PNG, mirroring the child's own limit.
const MAX_PNG: usize = 512 * 1024 * 1024;
const MAX_TSV: usize = 64 * 1024 * 1024;

/// Serialize one `--serve` request: the header line plus the optional facts body.
///
/// The wire format is `serve.rs::handle_request`'s, and the field ORDER is not
/// an accident:
///
/// ```text
/// REQ <WxH> <facts_len> <token:0|1> [HITS=x,y;x,y] [RECTS=k=sel;k=sel] <repo>\n
/// <facts body, exactly facts_len bytes, then one \n — only when facts_len > 0>
/// ```
///
/// The child peels the `name=value` fields off the tail and re-joins the
/// remainder as the repo, because a repository path may contain spaces (the
/// fork's own root does). The optional fields therefore have to sit BETWEEN the
/// token and the repo. They used to be spliced in before the header's newline —
/// i.e. AFTER the repo — which puts them past the child's `splitn(6, ' ')`: it
/// never saw the field, emitted no `@hit`/`@rect` rows, and every runtime-created
/// control (sidebar facets, the tab bar, drawer rows, blocks cards) silently
/// stopped resolving. Keeping the whole request built in one place, in protocol
/// order, is what makes a regression here impossible to hide.
///
/// With no `HITS`/`RECTS` and no body this is byte-identical to the pre-channel
/// request, so the painted frame (and its `@frame` token) cannot move.
fn request_bytes(
    size: (u32, u32),
    repo: &str,
    facts: Option<&str>,
    hits: &[(f32, f32)],
    rects: &[(&str, &str)],
    probe: bool,
) -> Vec<u8> {
    let body = facts.unwrap_or("");
    // Both optional fields live in the HEADER, not on following lines: the host
    // writes the whole request and then blocks reading the response, so the child
    // must never wait for input that has not been sent yet.
    let mut optional = String::new();
    if !hits.is_empty() {
        optional.push_str(" HITS=");
        for (i, (x, y)) in hits.iter().enumerate() {
            if i > 0 {
                optional.push(';');
            }
            optional.push_str(&format!("{x},{y}"));
        }
    }
    if !rects.is_empty() {
        // Selectors are space-free (`>` child combinators), which is what keeps
        // this field parsable without escaping.
        optional.push_str(" RECTS=");
        for (i, (key, selector)) in rects.iter().enumerate() {
            if i > 0 {
                optional.push(';');
            }
            optional.push_str(&format!("{key}={selector}"));
        }
    }
    if probe {
        optional.push_str(" PROBE=1");
    }
    let mut out = format!("REQ {}x{} {} 1", size.0, size.1, body.len()).into_bytes();
    out.extend_from_slice(optional.as_bytes());
    out.push(b' ');
    out.extend_from_slice(repo.as_bytes());
    out.push(b'\n');
    if !body.is_empty() {
        out.extend_from_slice(body.as_bytes());
        out.push(b'\n');
    }
    out
}

impl NzkUiClient {
    /// Spawn the child and complete the handshake.
    ///
    /// Returns `Err` if the binary is missing, dies during startup, or speaks a
    /// protocol this host does not know — in which case the caller falls back to
    /// the one-shot path rather than showing nothing.
    pub fn spawn(bin: &Path, size: (u32, u32), repo: &str) -> Result<Self, String> {
        let mut cmd = Command::new(bin);
        cmd.env("CSVIEWER_FRONTEND_DIR", crate::app::nzk_ui::viewer_frontend_dir())
            .arg("--serve")
            .arg("--size")
            .arg(format!("{}x{}", size.0, size.1))
            .arg("--repo")
            .arg(repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Human-readable progress must not corrupt the stdout framing, and
            // the child must never block on a full stderr pipe.
            .stderr(Stdio::null());
        // Own process group: the child can never receive the terminal's
        // Ctrl-C/SIGINT along with the host, and a group kill is possible if a
        // caller ever needs it. stdin EOF remains the primary shutdown signal.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", bin.display()))?;
        let stdin = child.stdin.take().ok_or("child stdin not piped")?;
        let stdout = child.stdout.take().ok_or("child stdout not piped")?;
        let mut client = Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            bin: bin.to_path_buf(),
            frames: 0,
            probe_support: false,
        };
        // Handshake: the child announces itself before reading any request.
        let mut banner = String::new();
        match client.stdout.read_line(&mut banner) {
            Ok(0) => {
                let _ = client.child.wait();
                return Err(format!(
                    "{} exited during startup",
                    bin.display()
                ));
            }
            Ok(_) => {}
            Err(e) => return Err(format!("read banner: {e}")),
        }
        if !banner.starts_with("nzk_ui ") || !banner.contains("protocol 1") {
            let _ = client.shutdown();
            return Err(format!("unexpected banner {banner:?}"));
        }
        // Additive capability, advertised by the banner: an old child keeps
        // working and simply pays for a repaint on every hit request.
        client.probe_support = banner.contains(" probe");
        Ok(client)
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// One request → one layout pass → PNG + geometry.
    ///
    /// `facts` is the serialized `dom::Facts` document, or `None` for the
    /// fixture. `rects` is the optional `RECTS=` key/selector list: each pair
    /// asks the child to emit `@rect` rows for every matching element, measured
    /// in the same layout pass as the pixels. Pass `&[]` to keep the frame
    /// byte-identical to the pre-channel protocol and emit no `@rect` rows.
    /// Answers `Err` on any protocol error so the caller can fall back;
    /// the child is left usable for a retry when the error came from a single
    /// bad request (the child reports those as `ERR` and keeps serving).
    pub fn request(
        &mut self,
        size: (u32, u32),
        repo: &str,
        facts: Option<&str>,
        hits: &[(f32, f32)],
        rects: &[(&str, &str)],
    ) -> Result<Frame, String> {
        self.request_impl(size, repo, facts, hits, rects, false)
    }

    /// Hit-test/probe only: the child answers from its cached layout and does
    /// **not** paint or PNG-encode, returning an empty `png`.
    ///
    /// Falls back to a normal (repainting) request when the child predates the
    /// additive `PROBE=` field, so an old binary keeps working — it just costs
    /// the old price.
    pub fn probe(
        &mut self,
        size: (u32, u32),
        repo: &str,
        facts: Option<&str>,
        hits: &[(f32, f32)],
    ) -> Result<Frame, String> {
        self.request_impl(size, repo, facts, hits, &[], self.probe_support)
    }

    fn request_impl(
        &mut self,
        size: (u32, u32),
        repo: &str,
        facts: Option<&str>,
        hits: &[(f32, f32)],
        rects: &[(&str, &str)],
        probe: bool,
    ) -> Result<Frame, String> {
        let out = request_bytes(size, repo, facts, hits, rects, probe);

        let stdin = self.stdin.as_mut().ok_or("child stdin already closed")?;
        stdin.write_all(&out).map_err(|e| format!("write request: {e}"))?;
        stdin.flush().map_err(|e| format!("flush request: {e}"))?;

        let mut header = String::new();
        if self
            .stdout
            .read_line(&mut header)
            .map_err(|e| format!("read response: {e}"))?
            == 0
        {
            return Err("child closed stdout".to_string());
        }
        let header = header.trim_end();
        if let Some(rest) = header.strip_prefix("ERR ") {
            let n: usize = rest.trim().parse().map_err(|_| format!("bad ERR {header:?}"))?;
            let mut msg = vec![0u8; n.min(MAX_TSV)];
            self.stdout
                .read_exact(&mut msg)
                .map_err(|e| format!("read ERR body: {e}"))?;
            let mut nl = [0u8; 1];
            let _ = self.stdout.read_exact(&mut nl);
            return Err(String::from_utf8_lossy(&msg).into_owned());
        }
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() != 5 || parts[0] != "OK" {
            return Err(format!("bad response header {header:?}"));
        }
        let w: u32 = parts[1].parse().map_err(|_| "bad width".to_string())?;
        let h: u32 = parts[2].parse().map_err(|_| "bad height".to_string())?;
        let png_len: usize = parts[3].parse().map_err(|_| "bad png_len".to_string())?;
        let tsv_len: usize = parts[4].parse().map_err(|_| "bad tsv_len".to_string())?;
        if png_len > MAX_PNG || tsv_len > MAX_TSV {
            return Err(format!("oversized frame png={png_len} tsv={tsv_len}"));
        }

        let mut png = vec![0u8; png_len];
        self.stdout
            .read_exact(&mut png)
            .map_err(|e| format!("read png: {e}"))?;
        let mut sep = [0u8; 1];
        self.stdout
            .read_exact(&mut sep)
            .map_err(|e| format!("read png separator: {e}"))?;
        let mut tsv = vec![0u8; tsv_len];
        self.stdout
            .read_exact(&mut tsv)
            .map_err(|e| format!("read tsv: {e}"))?;
        let _ = self.stdout.read_exact(&mut sep);
        self.frames += 1;

        Ok(Frame {
            png,
            tsv: String::from_utf8_lossy(&tsv).into_owned(),
            width: w,
            height: h,
        })
    }

    /// Clean shutdown: `QUIT`, then reap. Safe to call more than once.
    pub fn shutdown(&mut self) -> std::io::Result<()> {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.write_all(b"QUIT\n");
            let _ = s.flush();
            // Dropping the stdin handle closes the pipe, which is the child's
            // second, unconditional exit path.
            drop(s);
        }
        match self.child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => {
                let _ = self.child.wait();
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Is the child still running?
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for NzkUiClient {
    fn drop(&mut self) {
        // Best-effort: the child also dies on stdin EOF, which taking `stdin`
        // here guarantees.
        let _ = self.shutdown();
    }
}

/// Split a `--boxes` TSV body into geometry rows and the `@`-prefixed metadata
/// rows the single-pass protocol adds.
///
/// The geometry rows are byte-identical to the one-shot `--boxes` output, so
/// the existing `BTreeMap<String, Rect>` contract is unchanged.
pub struct ParsedTsv {
    pub scroll_max_y: f64,
    /// `@frame\t<w>\t<h>\t<sha256hex|none>` — the paint↔geometry token.
    pub paint_digest: Option<String>,
}

pub fn parse_meta(tsv: &str) -> ParsedTsv {
    let mut scroll_max_y = 0.0f64;
    let mut paint_digest = None;
    for line in tsv.lines() {
        let mut it = line.split('\t');
        match it.next() {
            Some("@scroll") => {
                let _sel = it.next();
                let _x = it.next();
                let _y = it.next();
                let _max_x = it.next();
                if let Some(v) = it.next().and_then(|v| v.trim().parse::<f64>().ok()) {
                    scroll_max_y = scroll_max_y.max(v);
                }
            }
            Some("@frame") => {
                let _w = it.next();
                let _h = it.next();
                let tok = it.next().unwrap_or("").trim();
                // `none` means "no token available". That is not a mismatch,
                // and must not be reported as one.
                if !tok.is_empty() && tok != "none" {
                    paint_digest = Some(tok.to_string());
                }
            }
            _ => {}
        }
    }
    ParsedTsv {
        scroll_max_y,
        paint_digest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The child's own header parse (`nzk_ui/src/serve.rs::handle_request`),
    /// reproduced here on purpose: a host/child drift in field ORDER is exactly
    /// the bug this pins, and the only way to catch it from the host side is to
    /// run the child's algorithm over the bytes the host would write.
    ///
    /// Returns `(facts_len, token, hits, rects, repo)`.
    #[allow(clippy::type_complexity)]
    fn parse_header_like_the_child(
        header: &str,
    ) -> (usize, bool, Vec<(f32, f32)>, Vec<(String, String)>, String) {
        let mut it = header.splitn(5, ' ');
        assert_eq!(it.next(), Some("REQ"), "verb");
        let _size = it.next().expect("size");
        let facts_len: usize = it.next().expect("facts_len").parse().expect("facts_len int");
        let token = match it.next() {
            Some("1") => true,
            Some("0") => false,
            other => panic!("bad token flag {other:?}"),
        };
        let mut rest = it.next().unwrap_or("").trim_end();
        let mut hits = Vec::new();
        let mut rects = Vec::new();
        loop {
            if let Some(r) = rest.strip_prefix("HITS=") {
                let (value, tail) = match r.split_once(' ') {
                    Some((v, t)) => (v, t.trim_start()),
                    None => (r, ""),
                };
                for p in value.split(';').filter(|p| !p.is_empty()) {
                    let (a, b) = p.split_once(',').expect("x,y");
                    hits.push((a.parse().expect("x"), b.parse().expect("y")));
                }
                rest = tail;
            } else if let Some(r) = rest.strip_prefix("RECTS=") {
                let (value, tail) = match r.split_once(' ') {
                    Some((v, t)) => (v, t.trim_start()),
                    None => (r, ""),
                };
                for p in value.split(';').filter(|p| !p.is_empty()) {
                    let (k, sel) = p.split_once('=').expect("k=sel");
                    rects.push((k.to_string(), sel.to_string()));
                }
                rest = tail;
            } else {
                break;
            }
        }
        (facts_len, token, hits, rects, rest.to_string())
    }

    /// A `HITS=` field must reach the child. It used to be spliced in after the
    /// repo, past the child's `splitn(6, ' ')`, so the child answered with no
    /// `@hit` rows and every runtime-created control was dead. The repo here
    /// contains spaces (the fork's own root does), which is why the repo must be
    /// the tail and not a fixed-position field.
    #[test]
    fn hits_reach_the_child_before_the_repo() {
        let repo = "/2tb/@/nzk/git/vrcCS/cs viewer builder";
        let bytes = request_bytes((1600, 1000), repo, None, &[(212.0, 75.0)], &[], false);
        let text = String::from_utf8(bytes).expect("utf8");
        let header = text.split('\n').next().expect("header");
        let (facts_len, token, hits, rects, parsed_repo) = parse_header_like_the_child(header);
        assert_eq!(facts_len, 0);
        assert!(token);
        assert_eq!(hits, vec![(212.0, 75.0)], "HITS must survive the child parse");
        assert!(rects.is_empty());
        assert_eq!(parsed_repo, repo, "the whole repo (spaces included) is the tail");
    }

    /// `RECTS=` has the same ordering requirement as `HITS=`: it is the channel
    /// the Blocks canvas reads painted card/port geometry from, so a dropped
    /// field is a silent feature loss, not a cosmetic one.
    #[test]
    fn rects_reach_the_child_before_the_repo() {
        let repo = "/10tb/TODO/Decompiled_EnncryptedNames";
        let bytes = request_bytes(
            (1600, 1000),
            repo,
            None,
            &[],
            &[("graph", "#graph"), ("card0", ".block-card[data-id=\"0\"]")],
            false,
        );
        let header = String::from_utf8(bytes).expect("utf8");
        let header = header.split('\n').next().expect("header");
        let (_, _, hits, rects, parsed_repo) = parse_header_like_the_child(header);
        assert!(hits.is_empty());
        assert_eq!(
            rects,
            vec![
                ("graph".to_string(), "#graph".to_string()),
                ("card0".to_string(), ".block-card[data-id=\"0\"]".to_string()),
            ]
        );
        assert_eq!(parsed_repo, repo);
    }

    /// Both optional fields together, still inside the child's field window.
    #[test]
    fn hits_and_rects_together_stay_inside_the_header() {
        let bytes = request_bytes(
            (1440, 900),
            "/repo",
            None,
            &[(10.0, 20.0), (30.5, 40.25)],
            &[("g", "#graph")],
            false,
        );
        let header = String::from_utf8(bytes).expect("utf8");
        let header = header.split('\n').next().expect("header");
        let (_, _, hits, rects, repo) = parse_header_like_the_child(header);
        assert_eq!(hits, vec![(10.0, 20.0), (30.5, 40.25)]);
        assert_eq!(rects, vec![("g".to_string(), "#graph".to_string())]);
        assert_eq!(repo, "/repo");
    }

    /// With no optional fields and no facts body the request must be byte-for-byte
    /// the pre-channel one: the painted frame and its `@frame` token depend on it.
    #[test]
    fn a_plain_request_is_unchanged() {
        let bytes = request_bytes((1600, 1000), "/tmp/fid-corpus", None, &[], &[], false);
        assert_eq!(bytes, b"REQ 1600x1000 0 1 /tmp/fid-corpus\n");
    }

    /// The facts body follows the header, length-prefixed and newline-delimited.
    #[test]
    fn the_facts_body_follows_the_header() {
        let facts = "{\"a\":1}";
        let bytes = request_bytes((800, 600), "/repo", Some(facts), &[(1.0, 2.0)], &[], false);
        let text = String::from_utf8(bytes).expect("utf8");
        let mut lines = text.split('\n');
        let header = lines.next().expect("header");
        assert_eq!(header, "REQ 800x600 7 1 HITS=1,2 /repo");
        assert_eq!(lines.next(), Some(facts));
        let (facts_len, _, hits, _, repo) = parse_header_like_the_child(header);
        assert_eq!(facts_len, facts.len());
        assert_eq!(hits, vec![(1.0, 2.0)]);
        assert_eq!(repo, "/repo");
    }

    /// The token row is optional metadata; geometry rows are unaffected by it.
    #[test]
    fn parses_frame_token_and_scroll() {
        let tsv = "#graph\t0.00\t52.00\t1240.00\t948.00\n\
                   #fitBtn\t193.00\t62.00\t41.00\t26.00\n\
                   @scroll\t.side\t0.00\t0.00\t0.00\t412.50\n\
                   @frame\t1600\t1000\tabc123\n";
        let m = parse_meta(tsv);
        assert_eq!(m.paint_digest.as_deref(), Some("abc123"));
        assert_eq!(m.scroll_max_y, 412.5);
    }

    /// `none` means the renderer supplied no token. That is "unproven", and must
    /// NOT be surfaced as a token — a caller comparing it would report drift.
    #[test]
    fn absent_token_is_none_not_a_digest() {
        for row in ["@frame\t1600\t1000\tnone\n", "@frame\t1600\t1000\t\n", ""] {
            assert_eq!(parse_meta(row).paint_digest, None, "row {row:?}");
        }
    }

    #[test]
    fn picks_the_largest_scroll_max() {
        let tsv = "@scroll\t.a\t0\t0\t0\t10.00\n@scroll\t.b\t0\t0\t0\t99.00\n";
        assert_eq!(parse_meta(tsv).scroll_max_y, 99.0);
    }

    /// Locate the built `nzk_ui` binary for the live-child tests, or `None`
    /// when the crate has not been built (the tests then skip honestly).
    ///
    /// `BIN_ENV` wins when it names a real file. Otherwise derive it from the
    /// test executable: `.../native-csviewer/target/release/deps/native-csviewer-<hash>`
    /// — three pops reach the `native-csviewer` root, then the crate's own
    /// target. (The old two-pop path resolved to `target/nzk_ui/...`, which
    /// never exists, so every live-child test silently skipped and the
    /// `--serve` protocol was never exercised end to end.)
    fn live_nzk_ui() -> Option<PathBuf> {
        crate::app::nzk_ui::env_bin()
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                let mut c = std::env::current_exe().ok()?;
                c.pop();
                c.pop();
                c.pop();
                c.push("nzk_ui/target/release/nzk_ui");
                c.is_file().then_some(c)
            })
    }

    /// A ready-made child must answer geometry, pixels AND the token from the
    /// same pass, and must die on `shutdown`. Skips (like the sibling nzk_ui
    /// tests) when nzk_ui has not been built.
    #[test]
    fn live_child_answers_one_pass_and_exits() {
        let Some(bin) = live_nzk_ui() else {
            eprintln!("nzk_ui_client: skip (nzk_ui not built)");
            return;
        };
        let mut c = NzkUiClient::spawn(&bin, (1600, 1000), "/tmp/fid-corpus")
            .expect("spawn nzk_ui --serve");
        assert!(c.is_alive(), "child should be alive after the handshake");

        let f = c.request((1600, 1000), "/tmp/fid-corpus", None, &[], &[("graph", "#graph")])
            .expect("frame");
        assert_eq!((f.width, f.height), (1600, 1000));
        assert!(!f.png.is_empty(), "expected PNG bytes");
        assert_eq!(&f.png[..8], b"\x89PNG\r\n\x1a\n", "expected a PNG");
        // One layout pass must have produced geometry AND the token.
        assert!(f.tsv.contains("#graph\t"), "geometry rows missing");
        // ...and the measured-rect channel answers from the same pass.
        assert!(
            f.tsv.contains("@rect\tgraph\t"),
            "requested @rect row missing:\n{}",
            f.tsv
        );
        let m = parse_meta(&f.tsv);
        let tok = m.paint_digest.expect("token present unless --no-token");
        assert_eq!(tok.len(), 64, "sha256 hex should be 64 chars: {tok:?}");

        // A second request reuses the resident child.
        let f2 = c.request((1600, 1000), "/tmp/fid-corpus", None, &[], &[]).expect("frame 2");
        assert_eq!(f2.png, f.png, "an unchanged document must repaint identically");
        assert_eq!(c.frames, 2);

        c.shutdown().expect("clean shutdown");
        assert!(!c.is_alive(), "child must exit on QUIT");
    }

    /// The DOM hit chain must survive the wire. This is the end-to-end regression
    /// test for the round-9..r38 bug where the host appended `HITS=` AFTER the
    /// repo and the child never saw it: every runtime-created control resolved
    /// from the chain (sidebar facet/edgekey chips, tab bar, drawer rows, blocks
    /// cards, lens rows) was dead, while every `--boxes` control kept working —
    /// the exact symptom reported as "only the drop-downs work".
    ///
    /// A grid of points is used rather than one guessed coordinate so the test
    /// does not encode a layout that may legitimately move.
    #[test]
    fn live_child_answers_the_dom_hit_chain() {
        let Some(bin) = live_nzk_ui() else {
            eprintln!("nzk_ui_client: skip (nzk_ui not built)");
            return;
        };
        let mut c = NzkUiClient::spawn(&bin, (1600, 1000), "/tmp/fid-corpus")
            .expect("spawn nzk_ui --serve");
        // One layout pass first: the hit query replays the same facts/size and is
        // answered from the cached document.
        let frame = c
            .request((1600, 1000), "/tmp/fid-corpus", None, &[], &[])
            .expect("frame");
        assert!(frame.tsv.contains("#graph\t"), "geometry rows missing");

        let mut points = Vec::new();
        for x in (0..1600).step_by(40) {
            for y in (0..1000).step_by(40) {
                points.push((x as f32, y as f32));
            }
        }
        let hits = c
            .request((1600, 1000), "/tmp/fid-corpus", None, &points, &[])
            .expect("hit frame");
        let rows: Vec<&str> = hits
            .tsv
            .lines()
            .filter(|l| l.starts_with("@hit\t"))
            .collect();
        assert_eq!(
            rows.len(),
            points.len(),
            "every requested point must get one @hit row (a dropped HITS= field \
             yields zero rows)"
        );
        let named = rows.iter().filter(|r| !r.ends_with("\tnone")).count();
        assert!(
            named > 0,
            "the shell returned only 'none' hits over the whole viewport — the \
             hit test is not answering:\n{}",
            hits.tsv
        );
        // The chain must carry a real element identity, not just a node number:
        // the host's resolvers match on the id/class/data-* columns.
        assert!(
            rows.iter().any(|r| {
                let f: Vec<&str> = r.split('\t').collect();
                f.len() >= 9
                    && (f.get(4).is_some_and(|s| !s.is_empty())
                        || f.get(5).is_some_and(|s| !s.is_empty())
                        || f.get(6).is_some_and(|s| !s.is_empty()))
            }),
            "no @hit row carried an id/class/data-* payload:\n{}",
            hits.tsv
        );
        // And the host decoder must see the same rows (same crate, no drift).
        let decoded = crate::app::nzk_ui::hits_from_tsv(&hits.tsv);
        assert_eq!(decoded.len(), points.len());
        assert!(decoded.iter().any(|(_, _, n)| n.is_from_shell()));
        c.shutdown().ok();
    }

    /// A request the child rejects must come back as `Err` rather than hanging,
    /// and must leave the child usable.
    #[test]
    fn bad_request_errors_without_killing_the_child() {
        let Some(bin) = live_nzk_ui() else {
            eprintln!("nzk_ui_client: skip (nzk_ui not built)");
            return;
        };
        let mut c = NzkUiClient::spawn(&bin, (1600, 1000), "/tmp/fid-corpus").expect("spawn");
        // A zero size is rejected by the child's own size parser.
        let bad = c.request((0, 0), "/tmp/fid-corpus", None, &[], &[]);
        assert!(bad.is_err(), "a 0x0 request should be refused");
        assert!(c.is_alive(), "a refused request must not kill the child");
        // ...and the child still serves a good request afterwards.
        let ok = c.request((1600, 1000), "/tmp/fid-corpus", None, &[], &[]);
        assert!(ok.is_ok(), "child must remain usable: {:?}", ok.err());
        c.shutdown().ok();
    }
}
