//! `source_shot` — headless PNG evidence for the native source pane.
//!
//! This is a separate binary (not part of the window shell) on purpose: it
//! renders the source pane offscreen with the exact same technique task-6's
//! `render.rs` uses — egui is tessellated in software, drawn by `egui-wgpu`
//! into an offscreen `Rgba8UnormSrgb` texture, read back through a mapped
//! buffer and written with the `image` crate — without touching the shell's
//! files. No window, no display, no browser engine.
//!
//! ```text
//! source_shot --repo DIR [--root REL]... --node ID --out FILE
//!             [--size WxH|auto] [--theme darkplus|darkmodern]
//! ```
//!
//! `--size auto` measures the documented content first (a throwaway egui
//! layout pass) and renders exactly that size, so a 100-line document is not
//! silently cropped.

// The two pane modules are shared verbatim with the window binary. They are
// reached through the package's library half (`native_csviewer::app`), not by
// `#[path]`-including a subset of files here: `source_pane` now depends on
// `crate::app::codelens_wire` and `crate::model`, so the standalone-subset
// approach stopped compiling. The library re-uses the same module roots as
// `main.rs`, which keeps a single source of truth and makes the whole-package
// `cargo build` work again.
use native_csviewer::app::source_pane::{self, SourceState, SourceTheme};
use scratch_rs_ide_addon::gui_scratch::{self, GuiState, PaletteSpec};
use scratch_rs_ide_addon::idelink_scratch::{self, IdeView, LinkState, ScratchDoc};

use std::path::PathBuf;
use std::process::ExitCode;

use csgraph_core::config::{self, Config};
use csgraph_core::index::Index;

struct Cli {
    repo: PathBuf,
    roots: Vec<String>,
    node: String,
    out: PathBuf,
    size: Option<(u32, u32)>,
    theme: String,
    view: View,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    /// The interactive pane (head + scrolled, centred body) at an explicit size.
    Pane,
    /// The whole document, unscrolled; `--size auto` fits it exactly.
    Doc,
}

fn usage() -> String {
    "source_shot — offscreen PNG of the native source pane (engine-free)\n\
     \n\
     USAGE:\n\
     \x20 source_shot --repo DIR --node ID --out FILE [OPTIONS]\n\
     \n\
     OPTIONS:\n\
     \x20 --repo DIR        repository/folder to index\n\
     \x20 --root PATH       root to index (repeatable; default: the repo's config roots)\n\
     \x20 --node ID         node id / full name / label whose document to render\n\
     \x20 --out FILE        output PNG path\n\
     \x20 --view MODE       pane (interactive pane, default 1200x420) | doc (whole document)\n\
     \x20 --size WxH|auto   image size (doc view default: auto = the document's own size)\n\
     \x20 --theme NAME      darkplus (default) | darkmodern\n\
     \x20 -h, --help        this message\n"
        .to_string()
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn parse_args() -> Result<Cli, String> {
    let mut args = std::env::args().skip(1);
    let mut repo: Option<PathBuf> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut node: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut size: Option<(u32, u32)> = None;
    let mut theme = "darkplus".to_string();
    let mut view = View::Pane;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--repo" => repo = Some(PathBuf::from(args.next().ok_or("--repo needs a value")?)),
            "--root" => roots.push(args.next().ok_or("--root needs a value")?),
            "--node" => node = Some(args.next().ok_or("--node needs a value")?),
            "--out" => out = Some(PathBuf::from(args.next().ok_or("--out needs a value")?)),
            "--view" => {
                view = match args.next().ok_or("--view needs pane|doc")?.as_str() {
                    "pane" => View::Pane,
                    "doc" => View::Doc,
                    other => return Err(format!("unknown --view {other:?} (pane | doc)")),
                }
            }
            "--size" => {
                let v = args.next().ok_or("--size needs WxH or auto")?;
                if v != "auto" {
                    size = Some(parse_size(&v).ok_or_else(|| format!("bad --size {v:?}"))?);
                }
            }
            "--theme" => theme = args.next().ok_or("--theme needs a value")?,
            "-h" | "--help" => {
                print!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n\n{}", usage())),
        }
    }
    Ok(Cli {
        repo: repo.ok_or("--repo is required")?,
        roots,
        node: node.ok_or("--node is required")?,
        out: out.ok_or("--out is required")?,
        size,
        theme,
        view,
    })
}

fn load_config(repo: &std::path::Path) -> Config {
    for c in [
        repo.join("tools/csgraph/csgraph.config.json"),
        repo.join("csgraph.config.json"),
    ] {
        if c.is_file() {
            return config::load_config(&c);
        }
    }
    Config::default()
}

fn resolve_roots(repo: &std::path::Path, cli: &[String], cfg: &Config) -> Vec<String> {
    if !cli.is_empty() {
        return cli.to_vec();
    }
    let from_cfg: Vec<String> = cfg
        .roots
        .iter()
        .filter(|r| repo.join(r).exists())
        .cloned()
        .collect();
    if !from_cfg.is_empty() {
        return from_cfg;
    }
    vec![repo.to_string_lossy().into_owned()]
}

fn align_to(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let theme = match cli.theme.as_str() {
        "darkplus" => SourceTheme::darkplus(),
        "darkmodern" => SourceTheme::darkmodern(),
        other => {
            eprintln!("unknown --theme {other:?} (darkplus | darkmodern)");
            return ExitCode::from(2);
        }
    };

    let repo = csgraph_core::index::abspath(&cli.repo);
    let cfg = load_config(&repo);
    let roots = resolve_roots(&repo, &cli.roots, &cfg);
    let mut index = Index::new(repo.clone());
    let t0 = std::time::Instant::now();
    let summary = index.build(&roots, &cfg.excludes, &cfg);
    eprintln!(
        "[scratch_shot] repo={} roots={roots:?} indexed in {}ms ({} files)",
        repo.display(),
        t0.elapsed().as_millis(),
        summary.get("fileCount").and_then(|v| v.as_u64()).unwrap_or(0)
    );

    let mut source = SourceState::default();
    if !source.show_node(&mut index, &cli.node) {
        eprintln!(
            "[scratch_shot] no document for node {:?}: {}",
            cli.node,
            source.error.clone().unwrap_or_default()
        );
        return ExitCode::FAILURE;
    }
    let view = {
        let d = source.doc.as_ref().expect("show_node set the document");
        let text = d
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        IdeView {
            repo: repo.display().to_string(),
            node_id: d.node_ref.clone(),
            title: d.title.clone(),
            kind: String::new(),
            file: d.file.clone(),
            line: d.line,
            end_line: d.end_line,
            source: text,
            ret: None,
            from_source_pane: true,
        }
    };
    let doc = match idelink_scratch::build(&view) {
        LinkState::Ready(d) => d,
        other => {
            eprintln!("[scratch_shot] no block document: {other:?}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "[scratch_shot] node={} blocks={} warnings={}",
        doc.title,
        doc.count(),
        doc.warnings.len()
    );

    let mut gui = GuiState::new();
    gui.spec = Some(PaletteSpec {
        bg: theme.bg,
        head_bg: theme.head_bg,
        fg: theme.fg,
        dim: theme.text_dim,
        border: theme.border,
        accent: theme.accent,
        comment: theme.comment,
        keyword: theme.keyword,
        control: theme.control,
        ty: theme.ty,
        iface: theme.iface,
        string: theme.string,
        number: theme.number,
        preproc: theme.preproc,
        function: theme.function,
        variable: theme.variable,
        op: theme.op,
    });
    let (width, height) = cli.size.unwrap_or((1100, 820));
    match shot(&doc, &mut gui, &theme, width, height, &cli.out) {
        Ok(bytes) => {
            println!(
                "scratch-shot: {} {}x{} {} bytes ({} blocks)",
                cli.out.display(),
                width,
                height,
                bytes,
                doc.count()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[scratch_shot] render failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Offscreen egui-wgpu render of the pane to a PNG.
fn shot(
    doc: &ScratchDoc,
    gui: &mut GuiState,
    theme: &SourceTheme,
    width: u32,
    height: u32,
    path: &std::path::Path,
) -> Result<u64, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let gpu = pollster::block_on(async {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no wgpu adapter (Vulkan/GL): {e}"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("source_shot"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("request_device failed: {e}"))?;
        Ok::<_, String>((adapter, device, queue))
    })?;
    let (adapter, device, queue) = gpu;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("source-screenshot"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.0);
    let raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(width as f32, height as f32),
        )),
        ..Default::default()
    };
    let output = ctx.run_ui(raw_input, |ui| {
        ui.painter().rect_filled(ui.clip_rect(), 0.0, theme.bg);
        gui_scratch::ui(ui, doc, gui);
    });
    let mut texts = output.textures_delta;
    let paint_jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point: output.pixels_per_point,
    };

    let mut renderer = egui_wgpu::Renderer::new(
        &device,
        format,
        egui_wgpu::RendererOptions::default(),
    );
    for (id, deltas) in &texts.set {
        for delta in deltas {
            renderer.update_texture(&device, &queue, *id, delta);
        }
    }

    let unpadded = width * 4;
    let padded = align_to(unpadded, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("source-readback"),
        size: (padded * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("source-offscreen"),
    });
    let pre = renderer.update_buffers(&device, &queue, &mut encoder, &paint_jobs, &screen);
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("source-offscreen-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &texture_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let mut pass = pass.forget_lifetime();
        renderer.render(&mut pass, &paint_jobs, &screen);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    queue.submit(pre.into_iter().chain(std::iter::once(encoder.finish())));

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
        .map_err(|e| format!("device.poll(Wait) failed: {e}"))?;
    rx.recv()
        .map_err(|e| format!("map_async channel closed: {e}"))?
        .map_err(|e| format!("buffer map failed: {e}"))?;

    let mut pixels = Vec::with_capacity((unpadded * height) as usize);
    {
        let data = slice.get_mapped_range().map_err(|e| format!("get_mapped_range: {e}"))?;
        for row in 0..height {
            let start = (row * padded) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded as usize]);
        }
    }
    buffer.unmap();

    let img = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| "readback size mismatch".to_string())?;
    img.save(path).map_err(|e| format!("PNG write {}: {e}", path.display()))?;
    eprintln!(
        "[source_shot] adapter={:?} backend={:?}",
        adapter.get_info().name,
        adapter.get_info().backend
    );
    for id in &texts.free {
        renderer.free_texture(id);
    }
    texts.clear();
    Ok(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
}
