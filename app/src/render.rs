//! GPU bring-up and the **offscreen** renderer used for headless evidence.
//!
//! The windowed path in [`crate::app`] and the offscreen path here share the
//! same `egui` UI ([`crate::app::build_ui`]) and the same `GraphView`, so a PNG
//! produced on a machine with no display exercises the real graph drawing code.
//!
//! No browser engine is involved anywhere: `wgpu` talks to Vulkan (or GL), and
//! `egui-wgpu` rasterises the tessellated egui meshes.

use std::path::Path;

use egui_wgpu::{RendererOptions, ScreenDescriptor};

use crate::app::build_ui;
use crate::view::GraphView;

/// Instance + adapter + device/queue used by either presentation path.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// Preferred backends: Vulkan first, GL as the fallback (`wgpu` picks the
    /// first adapter that satisfies the request).
    pub fn instance() -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        })
    }

    pub async fn new(compatible_surface: Option<&wgpu::Surface<'_>>) -> Result<Gpu, String> {
        let instance = Self::instance();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no wgpu adapter (Vulkan/GL): {e}"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("native-csviewer"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("request_device failed: {e}"))?;
        Ok(Gpu { instance, adapter, device, queue })
    }

    pub fn blocking(compatible_surface: Option<&wgpu::Surface<'_>>) -> Result<Gpu, String> {
        pollster::block_on(Self::new(compatible_surface))
    }
}

fn align_to(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

/// Frame-path phase timer, enabled by `CSVIEWER_FRAME_TIMING=1`.
///
/// The render/update path is the app-side serial work that dilutes the engine's
/// measured parallelism; these marks say which phase actually costs what.
/// `pub(crate)` so `graphviz.rs` can mark the layout sub-phases with the same
/// switch.
pub(crate) fn frame_timing_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CSVIEWER_FRAME_TIMING").is_some())
}

pub(crate) fn frame_mark(phase: &str, t: std::time::Instant) {
    if frame_timing_on() {
        eprintln!("[frame] {phase} {}ms", t.elapsed().as_millis());
    }
}

/// Report the source pane's galley-cache probe counters, under the same
/// `CSVIEWER_FRAME_TIMING=1` switch as the phase marks.
///
/// The counters are the honest evidence that the cache is doing work: `hits`
/// counts frames whose galleys were reused, `misses` counts the re-layouts that
/// genuinely had to happen (document, theme, highlight toggle or width changed).
/// A steady-state window should show misses flat and hits climbing; if `misses`
/// keeps rising on a still screen, something is invalidating the key every frame
/// and the cache has become pure overhead.
pub(crate) fn frame_mark_cache(hits: u64, misses: u64) {
    if frame_timing_on() {
        eprintln!("[frame] source_galley_cache hits={hits} misses={misses}");
    }
}

/// Render the real graph UI into an offscreen texture and write `path` as PNG.
///
/// Returns `(width, height, file_bytes)`.
pub fn screenshot(
    view: &mut GraphView,
    width: u32,
    height: u32,
    path: &Path,
) -> Result<(u32, u32, u64), String> {
    let t_all = std::time::Instant::now();
    let t = std::time::Instant::now();
    let gpu = Gpu::blocking(None)?;
    frame_mark("gpu_init", t);
    let device = &gpu.device;
    let queue = &gpu.queue;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("graph-screenshot"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    // --- egui pass ---------------------------------------------------------
    let ctx = egui::Context::default();
    // The measured reference faces (Liberation Mono / Noto Sans Mono / Noto
    // Sans) must be in place before this pass lays out any text.
    crate::typography::install(&ctx);
    ctx.set_pixels_per_point(1.0);
    let raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(width as f32, height as f32),
        )),
        ..Default::default()
    };
    let t = std::time::Instant::now();
    let output = ctx.run_ui(raw_input, |ui| build_ui(ui, view));
    frame_mark("egui_ui_layout_text", t);
    let mut texts = output.textures_delta;
    let t = std::time::Instant::now();
    let paint_jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
    frame_mark("tessellate", t);
    let screen = ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point: output.pixels_per_point,
    };

    // `RendererOptions::default()` sets `dithering: true`, which adds ordered
    // ±1 noise to flat fills to hide banding. The original (Chromium/Skia) does
    // not dither, so a #1e1e1e canvas came out as a lattice of 29/30/31 and
    // disagreed with the reference on ~9% of every flat pixel. Turn it off:
    // the fork must reproduce the requested colour exactly, not approximately.
    let t = std::time::Instant::now();
    let mut renderer = egui_wgpu::Renderer::new(
        device,
        format,
        RendererOptions { dithering: false, ..RendererOptions::default() },
    );
    for (id, deltas) in &texts.set {
        for delta in deltas {
            renderer.update_texture(device, queue, *id, delta);
        }
    }
    frame_mark("renderer_textures", t);

    // --- copy-out buffer ---------------------------------------------------
    let unpadded = width * 4;
    let padded = align_to(unpadded, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("graph-readback"),
        size: (padded * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    // --- record ------------------------------------------------------------
    let t = std::time::Instant::now();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("graph-offscreen"),
    });
    let pre = renderer.update_buffers(device, queue, &mut encoder, &paint_jobs, &screen);
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("graph-offscreen-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &texture_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.07, g: 0.08, b: 0.09, a: 1.0 }),
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
    frame_mark("render_submit", t);

    // --- read back ---------------------------------------------------------
    let t = std::time::Instant::now();
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

    let t = std::time::Instant::now();
    // Strip the 256-byte row padding in parallel: 6.4 MB for 1600x1000, and the
    // copy is pure per row. Chunks are contiguous row ranges, so the result is
    // byte-identical to the serial loop.
    let mut pixels = vec![0u8; (unpadded * height) as usize];
    {
        let data = slice
            .get_mapped_range()
            .map_err(|e| format!("get_mapped_range: {e}"))?;
        let src: &[u8] = &data;
        let workers = std::thread::available_parallelism()
            .map(|v| v.get())
            .unwrap_or(1)
            .min(height as usize)
            .max(1);
        if workers <= 1 || height < 64 {
            for row in 0..height {
                let start = (row * padded) as usize;
                let dst = (row * unpadded) as usize;
                pixels[dst..dst + unpadded as usize]
                    .copy_from_slice(&src[start..start + unpadded as usize]);
            }
        } else {
            let rows_per = (height as usize).div_ceil(workers);
            std::thread::scope(|scope| {
                for (ci, chunk) in pixels.chunks_mut(unpadded as usize * rows_per).enumerate() {
                    let first = ci * rows_per;
                    scope.spawn(move || {
                        for (ri, row) in chunk.chunks_mut(unpadded as usize).enumerate() {
                            let r = first + ri;
                            if r >= height as usize {
                                break;
                            }
                            let s = (r as u32 * padded) as usize;
                            row.copy_from_slice(&src[s..s + unpadded as usize]);
                        }
                    });
                }
            });
        }
    }
    buffer.unmap();
    frame_mark("readback_poll", t);

    let t = std::time::Instant::now();
    // PNG is lossless, so the filter/compression choice cannot change a single
    // decoded pixel — only the file size and the encode time. The evidence
    // comparison (ab.sh) decodes pixels, so `Fast` + `NoFilter` is safe and
    // measurably cheaper than the crate default's adaptive filter scan.
    {
        use image::{ExtendedColorType, ImageEncoder as _};
        let file = std::fs::File::create(path)
            .map_err(|e| format!("PNG create {}: {e}", path.display()))?;
        let enc = image::codecs::png::PngEncoder::new_with_quality(
            std::io::BufWriter::new(file),
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Up,
        );
        enc.write_image(&pixels, width, height, ExtendedColorType::Rgba8)
            .map_err(|e| format!("PNG write {}: {e}", path.display()))?;
    }
    frame_mark("png_encode_save", t);
    frame_mark("total_screenshot", t_all);
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    for id in &texts.free {
        renderer.free_texture(id);
    }
    texts.clear();
    Ok((width, height, bytes))
}
