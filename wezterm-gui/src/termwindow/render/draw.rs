use crate::colorease::ColorEaseUniform;
use crate::termwindow::tab_sidebar::{SidebarDropTarget, SidebarHover, SidebarRow};
use crate::termwindow::sidebar_ui::{self, UiLayout};
use crate::termwindow::webgpu::ShaderUniform;
use crate::termwindow::RenderFrame;
use crate::uniforms::UniformBuilder;
use ::window::glium;
use ::window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use anyhow::Context;
use config::FreeTypeLoadTarget;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::time::Duration;

struct SidebarCaptureSpec {
    index: usize,
    offset_ms: u64,
    path: PathBuf,
    width: u32,
    height: u32,
    bytes_per_row: u32,
    format: wgpu::TextureFormat,
}

impl crate::TermWindow {
    pub fn call_draw(&mut self, frame: &mut RenderFrame) -> anyhow::Result<()> {
        match frame {
            RenderFrame::Glium(ref mut frame) => self.call_draw_glium(frame),
            RenderFrame::WebGpu => self.call_draw_webgpu(),
        }
    }

    fn call_draw_webgpu(&mut self) -> anyhow::Result<()> {
        use crate::termwindow::webgpu::WebGpuTexture;

        let pending_capture = self.sidebar_capture_spec();
        let screenshot_hover = self
            .sidebar_screenshot
            .as_ref()
            .and_then(|request| request.hover);
        if let Some((x, y)) = screenshot_hover {
            self.set_sidebar_hover_at(x, y);
        }
        let webgpu = self.webgpu.as_ref().unwrap();
        let render_state = self.render_state.as_ref().unwrap();

        let output = webgpu.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = webgpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });
        let tex = render_state.glyph_cache.borrow().atlas.texture();
        let tex = tex.downcast_ref::<WebGpuTexture>().unwrap();
        let texture_view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        let texture_linear_bind_group =
            webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &webgpu.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&webgpu.texture_linear_sampler),
                    },
                ],
                label: Some("linear bind group"),
            });

        let texture_nearest_bind_group =
            webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &webgpu.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&webgpu.texture_nearest_sampler),
                    },
                ],
                label: Some("nearest bind group"),
            });

        let mut cleared = false;
        let foreground_text_hsb = self.config.foreground_text_hsb;
        let foreground_text_hsb = [
            foreground_text_hsb.hue,
            foreground_text_hsb.saturation,
            foreground_text_hsb.brightness,
        ];

        let milliseconds = self.created.elapsed().as_millis() as u32;
        let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
            -(self.dimensions.pixel_width as f32) / 2.0,
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
            -(self.dimensions.pixel_height as f32) / 2.0,
            -1.0,
            1.0,
        )
        .to_arrays_transposed();

        for layer in render_state.layers.borrow().iter() {
            for idx in 0..3 {
                let vb = &layer.vb.borrow()[idx];
                let (vertex_count, index_count) = vb.vertex_index_count();
                let vertex_buffer;
                let uniforms;
                if vertex_count > 0 {
                    let mut vertices = vb.current_vb_mut();
                    let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("Render Pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: if cleared {
                                    wgpu::LoadOp::Load
                                } else {
                                    wgpu::LoadOp::Clear(wgpu::Color {
                                        r: 0.,
                                        g: 0.,
                                        b: 0.,
                                        a: 0.,
                                    })
                                },
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        occlusion_query_set: None,
                        timestamp_writes: None,
                    });
                    cleared = true;

                    uniforms = webgpu.create_uniform(ShaderUniform {
                        foreground_text_hsb,
                        milliseconds,
                        projection,
                    });

                    render_pass.set_pipeline(&webgpu.render_pipeline);
                    render_pass.set_bind_group(0, &uniforms, &[]);
                    render_pass.set_bind_group(1, &texture_linear_bind_group, &[]);
                    render_pass.set_bind_group(2, &texture_nearest_bind_group, &[]);
                    vertex_buffer = vertices.webgpu_mut().recreate();
                    vertex_buffer.unmap();
                    render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                    render_pass
                        .set_index_buffer(vb.indices.webgpu().slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..index_count as _, 0, 0..1);
                }

                vb.next_index();
            }
        }

        // The sidebar has one egui context per terminal window.  It is window
        // chrome, so its cached model is rendered once rather than once per
        // mux pane.
        let egui_cmd_bufs = if self.tab_sidebar_enabled {
            let config = webgpu.config.borrow();
            let linear_format = config.format.remove_srgb_suffix();
            let egui_format = if config.view_formats.contains(&linear_format) {
                linear_format
            } else {
                config.format
            };
            drop(config);
            let egui_view = output.texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(egui_format),
                ..Default::default()
            });
            let rows = self.tab_sidebar_rows.clone();
            let ui_layout = self.tab_sidebar.ui_layout.clone();
            let hovered = self.tab_sidebar.hovered.clone();
            let drop_target = self
                .tab_sidebar
                .drag
                .as_ref()
                .and_then(|drag| drag.target.clone());
            let sidebar_width = self.tab_sidebar_width_pixels() as u32;
            composite_tab_sidebar(
                &mut self.egui_ctx,
                &mut self.egui_renderer,
                &rows,
                self.tab_sidebar.scroll_rows,
                self.tab_sidebar.compact,
                hovered.as_ref(),
                drop_target.as_ref(),
                ui_layout.as_ref(),
                pending_capture
                    .as_ref()
                    .map(|spec| spec.offset_ms as f64 / 1000.0),
                sidebar_width,
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
                (self.dimensions.dpi as f32 / 96.0).max(1.0),
                &webgpu.device,
                &webgpu.queue,
                egui_format,
                &egui_view,
                &mut encoder,
                &mut self.sidebar_images,
            )?
        } else {
            Vec::new()
        };

        let capture_buffer = pending_capture.as_ref().and_then(|spec| {
            let can_copy = webgpu
                .config
                .borrow()
                .usage
                .contains(wgpu::TextureUsages::COPY_SRC);
            if !can_copy || spec.width == 0 || spec.height == 0 {
                return None;
            }
            let buffer = webgpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("sidebar screenshot readback"),
                size: spec.bytes_per_row as u64 * spec.height as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &output.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(spec.bytes_per_row),
                        rows_per_image: Some(spec.height),
                    },
                },
                wgpu::Extent3d {
                    width: spec.width,
                    height: spec.height,
                    depth_or_array_layers: 1,
                },
            );
            Some(buffer)
        });

        // Submit order matches the canonical egui-wgpu flow: the callback
        // command buffers from update_buffers first, then the encoded pass.
        webgpu
            .queue
            .submit(egui_cmd_bufs.into_iter().chain(std::iter::once(encoder.finish())));
        let capture_result = match (pending_capture, capture_buffer) {
            (Some(spec), Some(buffer)) => {
                let result = read_sidebar_png(webgpu, buffer, &spec);
                Some((spec, result))
            }
            (Some(spec), None) => Some((
                spec,
                Err(anyhow::anyhow!(
                    "sidebar screenshots are not supported by this WebGpu surface"
                )),
            )),
            (None, None) => None,
            (None, Some(_)) => unreachable!("a screenshot buffer without a screenshot request"),
        };
        output.present();
        if let Some((spec, result)) = capture_result {
            self.finish_sidebar_capture(spec, result);
        }

        Ok(())
    }

    fn sidebar_capture_spec(&self) -> Option<SidebarCaptureSpec> {
        let request = self.sidebar_screenshot.as_ref()?;
        let offset_ms = *request.offsets_ms.get(request.next)?;
        if request.started.elapsed() < Duration::from_millis(offset_ms) {
            return None;
        }
        let width = self.tab_sidebar_width_pixels() as u32;
        let height = self.dimensions.pixel_height as u32;
        let bytes_per_row = (width.saturating_mul(4) + 255) / 256 * 256;
        let format = self.webgpu.as_ref()?.config.borrow().format;
        Some(SidebarCaptureSpec {
            index: request.next,
            offset_ms,
            path: screenshot_path(&request.path, request.next, offset_ms),
            width,
            height,
            bytes_per_row,
            format,
        })
    }

    fn finish_sidebar_capture(
        &mut self,
        spec: SidebarCaptureSpec,
        result: anyhow::Result<()>,
    ) {
        let Some(mut request) = self.sidebar_screenshot.take() else {
            return;
        };
        match result {
            Ok(()) => {
                request.outputs.push(spec.path.to_string_lossy().into_owned());
                request.next = spec.index + 1;
                if request.next == request.offsets_ms.len() {
                    request.tx.try_send(Ok(request.outputs)).ok();
                } else {
                    self.sidebar_screenshot = Some(request);
                    self.schedule_sidebar_screenshot();
                }
            }
            Err(err) => {
                request.tx.try_send(Err(err)).ok();
            }
        }
    }

    fn call_draw_glium(&mut self, frame: &mut glium::Frame) -> anyhow::Result<()> {
        use window::glium::texture::SrgbTexture2d;

        let gl_state = self.render_state.as_ref().unwrap();
        let tex = gl_state.glyph_cache.borrow().atlas.texture();
        let tex = tex.downcast_ref::<SrgbTexture2d>().unwrap();

        frame.clear_color(0., 0., 0., 0.);

        let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
            -(self.dimensions.pixel_width as f32) / 2.0,
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
            -(self.dimensions.pixel_height as f32) / 2.0,
            -1.0,
            1.0,
        )
        .to_arrays_transposed();

        let use_subpixel = match self
            .config
            .freetype_render_target
            .unwrap_or(self.config.freetype_load_target)
        {
            FreeTypeLoadTarget::HorizontalLcd | FreeTypeLoadTarget::VerticalLcd => true,
            _ => false,
        };

        let dual_source_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceOneColor,
                    destination: LinearBlendingFactor::OneMinusSourceOneColor,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceOneColor,
                    destination: LinearBlendingFactor::OneMinusSourceOneColor,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },

            ..Default::default()
        };

        let alpha_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceAlpha,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::One,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },
            ..Default::default()
        };

        // Clamp and use the nearest texel rather than interpolate.
        // This prevents things like the box cursor outlines from
        // being randomly doubled in width or height
        let atlas_nearest_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Nearest)
            .minify_filter(MinifySamplerFilter::Nearest);

        let atlas_linear_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Linear)
            .minify_filter(MinifySamplerFilter::Linear);

        let foreground_text_hsb = self.config.foreground_text_hsb;
        let foreground_text_hsb = (
            foreground_text_hsb.hue,
            foreground_text_hsb.saturation,
            foreground_text_hsb.brightness,
        );

        let milliseconds = self.created.elapsed().as_millis() as u32;

        let cursor_blink: ColorEaseUniform = (*self.cursor_blink_state.borrow()).into();
        let blink: ColorEaseUniform = (*self.blink_state.borrow()).into();
        let rapid_blink: ColorEaseUniform = (*self.rapid_blink_state.borrow()).into();

        for layer in gl_state.layers.borrow().iter() {
            for idx in 0..3 {
                let vb = &layer.vb.borrow()[idx];
                let (vertex_count, index_count) = vb.vertex_index_count();
                if vertex_count > 0 {
                    let vertices = vb.current_vb_mut();
                    let subpixel_aa = use_subpixel && idx == 1;

                    let mut uniforms = UniformBuilder::default();

                    uniforms.add("projection", &projection);
                    uniforms.add("atlas_nearest_sampler", &atlas_nearest_sampler);
                    uniforms.add("atlas_linear_sampler", &atlas_linear_sampler);
                    uniforms.add("foreground_text_hsb", &foreground_text_hsb);
                    uniforms.add("subpixel_aa", &subpixel_aa);
                    uniforms.add("milliseconds", &milliseconds);
                    uniforms.add_struct("cursor_blink", &cursor_blink);
                    uniforms.add_struct("blink", &blink);
                    uniforms.add_struct("rapid_blink", &rapid_blink);

                    frame.draw(
                        vertices.glium().slice(0..vertex_count).unwrap(),
                        vb.indices.glium().slice(0..index_count).unwrap(),
                        gl_state.glyph_prog.as_ref().unwrap(),
                        &uniforms,
                        if subpixel_aa {
                            &dual_source_blending
                        } else {
                            &alpha_blending
                        },
                    )?;
                }

                vb.next_index();
            }
        }

        Ok(())
    }
}

fn read_sidebar_png(
    webgpu: &crate::termwindow::webgpu::WebGpuState,
    buffer: wgpu::Buffer,
    spec: &SidebarCaptureSpec,
) -> anyhow::Result<()> {
    let slice = buffer.slice(..);
    let (tx, rx) = sync_channel(1);
    slice.map_async(wgpu::MapMode::Read, move |result| {
        tx.send(result).ok();
    });
    webgpu.device.poll(wgpu::PollType::Wait)?;
    rx.recv().context("waiting for sidebar screenshot readback")??;

    let mapped = slice.get_mapped_range();
    let row_bytes = spec.width as usize * 4;
    let mut pixels = vec![0; row_bytes * spec.height as usize];
    for row in 0..spec.height as usize {
        let source = &mapped[row * spec.bytes_per_row as usize..][..row_bytes];
        let target = &mut pixels[row * row_bytes..][..row_bytes];
        target.copy_from_slice(source);
        if matches!(
            spec.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        ) {
            for pixel in target.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
    }
    drop(mapped);
    buffer.unmap();

    let image = image::RgbaImage::from_raw(spec.width, spec.height, pixels)
        .context("creating sidebar screenshot image")?;
    image
        .save_with_format(&spec.path, image::ImageFormat::Png)
        .with_context(|| format!("saving sidebar screenshot to {}", spec.path.display()))?;
    Ok(())
}

fn screenshot_path(base: &Path, index: usize, offset_ms: u64) -> PathBuf {
    if index == 0 {
        return base.to_path_buf();
    }
    let stem = base
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "sidebar".into());
    let suffix = format!("-{offset_ms}ms");
    let filename = match base.extension().map(|value| value.to_string_lossy()) {
        Some(extension) => format!("{stem}{suffix}.{extension}"),
        None => format!("{stem}{suffix}"),
    };
    base.with_file_name(filename)
}

/// Paint the cached, per-window sidebar into the same surface view as the
/// terminal.  Input is intentionally handled by native UIItem hit testing.
fn composite_tab_sidebar(
    egui_ctx: &mut Option<egui::Context>,
    egui_renderer: &mut Option<egui_wgpu::Renderer>,
    rows: &[SidebarRow],
    scroll_rows: usize,
    compact: bool,
    hovered: Option<&SidebarHover>,
    drop_target: Option<&SidebarDropTarget>,
    ui_layout: Option<&UiLayout>,
    animation_time: Option<f64>,
    sidebar_width: u32,
    pixel_w: u32,
    pixel_h: u32,
    pixels_per_point: f32,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
    view: &wgpu::TextureView,
    encoder: &mut wgpu::CommandEncoder,
    images: &mut HashMap<String, egui::TextureHandle>,
) -> anyhow::Result<Vec<wgpu::CommandBuffer>> {
    if egui_ctx.is_none() {
        *egui_ctx = Some(egui::Context::default());
        let ctx = egui_ctx.as_ref().unwrap();
        register_egui_fonts(ctx);
    }
    let ctx = egui_ctx.as_ref().unwrap();
    if egui_renderer.is_none() {
        *egui_renderer = Some(egui_wgpu::Renderer::new(
            device, format, None, 1, false,
        ));
    }
    let renderer = egui_renderer.as_mut().unwrap();

    ctx.set_pixels_per_point(pixels_per_point);
    let screen_rect = egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(pixel_w as f32 / pixels_per_point, pixel_h as f32 / pixels_per_point),
    );
    ctx.begin_pass(egui::RawInput {
        time: animation_time,
        screen_rect: Some(screen_rect),
        ..Default::default()
    });
    let rect = egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(sidebar_width as f32 / pixels_per_point, screen_rect.height()),
    );
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("tab-sidebar"),
    ));

    // FleetView palette -------------------------------------------------------
    let bg = egui::Color32::from_rgb(34, 37, 44);
    let sep = egui::Color32::from_rgb(47, 51, 60);
    let label = egui::Color32::from_rgb(235, 235, 240);
    let label2 = egui::Color32::from_rgb(154, 157, 170);
    let label3 = egui::Color32::from_rgb(107, 110, 122);
    let label4 = egui::Color32::from_rgb(74, 77, 87);
    let now = ctx.input(|i| i.time) as f32;

    painter.rect_filled(rect, 0.0, bg);
    painter.line_segment(
        [rect.right_top(), rect.right_bottom()],
        egui::Stroke::new(1.0_f32, sep),
    );

    if let Some(layout) = ui_layout {
        let hovered = match hovered {
            Some(SidebarHover::Node(id)) => Some(id.as_str()),
            _ => None,
        };
        sidebar_ui::paint(&painter, ctx, layout, hovered, images);
    }

    // UIItem rectangles use physical pixels; convert the shared row-height
    // contract to egui points so visual and hit-test geometry stay identical.
    let row_height = crate::termwindow::tab_sidebar::ROW_HEIGHT_PX as f32 / pixels_per_point;
    let font = egui::FontId::monospace(13.0);
    let label_font = egui::FontId::proportional(13.0);
    let small_font = egui::FontId::proportional(11.0);
    let tiny_font = egui::FontId::proportional(10.0);

    // FleetView helpers -------------------------------------------------------
    fn state_color(key: &str) -> egui::Color32 {
        match key {
            "running" => egui::Color32::from_rgb(100, 168, 240),
            "waiting" => egui::Color32::from_rgb(226, 192, 123),
            "blocked" => egui::Color32::from_rgb(240, 162, 90),
            "error" => egui::Color32::from_rgb(240, 112, 122),
            _ => egui::Color32::from_rgb(107, 110, 122),
        }
    }
    fn state_rank(key: &str) -> u8 {
        match key {
            "error" => 4,
            "blocked" => 3,
            "waiting" => 2,
            "running" => 1,
            _ => 0,
        }
    }
    fn state_badge_glyph(key: &str) -> &'static str {
        match key {
            "error" => "×",
            "blocked" => "‖",
            "waiting" => "○",
            "running" => "◐",
            _ => "·",
        }
    }
    fn urgency_color(urgency: u8, state_key: &str) -> egui::Color32 {
        if urgency == 2 { state_color("error") }
        else if urgency == 1 { state_color(state_key) }
        else { egui::Color32::TRANSPARENT }
    }
    fn breathe(t: f32) -> f32 {
        let phase = (t % 2.4) / 2.4;
        let tri = 1.0 - (phase * 2.0 - 1.0).abs();
        let eased = tri * tri * (3.0 - 2.0 * tri);
        0.42 + eased * 0.58
    }

    // Top summary strip in regular mode.
    let mut content_top = rect.top();
    if !compact {
        let summary_h = crate::termwindow::tab_sidebar::SUMMARY_HEIGHT_PX as f32 / pixels_per_point;
        let summary_rect = egui::Rect::from_min_size(rect.left_top(), egui::vec2(rect.width(), summary_h));
        painter.line_segment(
            [summary_rect.left_bottom(), summary_rect.right_bottom()],
            egui::Stroke::new(1.0_f32, sep),
        );
        let total = rows.iter().filter(|r| matches!(r, SidebarRow::Tab(_))).count();
        let counts: HashMap<String, usize> = rows.iter().filter_map(|r| match r {
            SidebarRow::Tab(e) if !e.status_key.is_empty() => Some(e.status_key.clone()),
            _ => None,
        }).fold(HashMap::new(), |mut m, k| { *m.entry(k).or_insert(0) += 1; m });
        let chips: [(String, String, egui::Color32); 5] = [
            ("All".into(), total.to_string(), label2),
            ("Errors".into(), counts.get("error").copied().unwrap_or(0).to_string(), state_color("error")),
            ("Blocked".into(), counts.get("blocked").copied().unwrap_or(0).to_string(), state_color("blocked")),
            ("Waiting".into(), counts.get("waiting").copied().unwrap_or(0).to_string(), state_color("waiting")),
            ("Running".into(), counts.get("running").copied().unwrap_or(0).to_string(), state_color("running")),
        ];
        let mut chip_x = summary_rect.left() + 8.0;
        for (name, value, color) in chips {
            if value == "0" && name != "All" { continue; }
            let text = format!("{} {}", name, value);
            let size = painter.layout(text.clone(), small_font.clone(), color, f32::INFINITY).size();
            let chip_w = size.x + 12.0;
            let chip = egui::Rect::from_min_size(
                egui::pos2(chip_x, summary_rect.center().y - 10.0),
                egui::vec2(chip_w, 20.0),
            );
            painter.rect_filled(chip, 10.0, egui::Color32::from_rgb(43, 47, 56));
            painter.text(chip.center(), egui::Align2::CENTER_CENTER, &text, small_font.clone(), color);
            chip_x += chip_w + 6.0;
        }
        content_top = summary_rect.bottom();
    }

    if ui_layout.is_none() {
    for (row, item) in rows.iter().skip(scroll_rows).enumerate() {
        let y = content_top + row as f32 * row_height;
        if y >= rect.height() {
            break;
        }
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(0.0, y),
            egui::vec2(rect.width(), row_height),
        );

        if compact {
            let SidebarRow::Group(group) = item else { continue; };
            let tile = row_rect.shrink2(egui::vec2(3.0, 2.0));
            let hovered = matches!(hovered, Some(SidebarHover::Group(key)) if key == &group.key);
            painter.rect_filled(
                tile,
                6.0,
                if hovered { egui::Color32::from_rgb(52, 57, 67) } else { egui::Color32::from_rgb(43, 47, 56) },
            );
            let name = if group.key == "@local" {
                "MAC".to_string()
            } else {
                group.label.rsplit('@').next().unwrap_or(&group.label)
                    .chars().filter(|ch| ch.is_alphanumeric()).take(2)
                    .collect::<String>().to_uppercase()
            };
            painter.text(
                tile.center(),
                egui::Align2::CENTER_CENTER,
                name,
                egui::FontId::monospace(13.0),
                egui::Color32::from_gray(190),
            );
            if group.urgency > 0 {
                let color = urgency_color(group.urgency, "error");
                let breathe_alpha = if group.urgency == 2 { breathe(now) } else { 1.0 };
                painter.circle_filled(
                    tile.right_top() - egui::vec2(5.0, -5.0),
                    3.0,
                    color.gamma_multiply(breathe_alpha),
                );
            }
            continue;
        }

        match item {
            SidebarRow::Group(group) => {
                let is_top = group.depth == 0;
                let inner = row_rect.shrink2(egui::vec2(if is_top { 8.0 } else { 10.0 }, 2.0));
                let row_hovered = matches!(hovered, Some(SidebarHover::Group(key)) if key == &group.key);
                if row_hovered {
                    painter.rect_filled(inner, 4.0, egui::Color32::from_rgb(40, 44, 52));
                }
                let tri = if is_top { "▾" } else { "›" };
                let name_color = if is_top { label } else { label3 };
                let mut x = inner.left() + (group.depth as f32 * 12.0);
                painter.text(
                    egui::pos2(x, inner.center().y),
                    egui::Align2::LEFT_CENTER,
                    tri,
                    small_font.clone(),
                    label3,
                );
                x += 12.0;
                let name = group.label.to_uppercase();
                painter.text(
                    egui::pos2(x, inner.center().y),
                    egui::Align2::LEFT_CENTER,
                    &name,
                    if is_top { small_font.clone() } else { tiny_font.clone() },
                    name_color,
                );
                let name_size = painter.layout(name, if is_top { small_font.clone() } else { tiny_font.clone() }, name_color, f32::INFINITY).size();
                x += name_size.x + 6.0;
                // Host label only for top-level groups.
                if is_top && !group.host.is_empty() {
                    let host = group.host.to_lowercase();
                    painter.text(
                        egui::pos2(x, inner.center().y),
                        egui::Align2::LEFT_CENTER,
                        &host,
                        tiny_font.clone(),
                        label4,
                    );
                }
                // Worktree tag for sub-groups.
                if !is_top && !group.worktree.is_empty() {
                    let wt = group.worktree.clone();
                    let wt_size = painter.layout(wt.clone(), tiny_font.clone(), label4, f32::INFINITY).size();
                    let tag = egui::Rect::from_min_size(
                        egui::pos2(x, inner.center().y - 6.0),
                        egui::vec2(wt_size.x + 6.0, 12.0),
                    );
                    painter.rect_filled(tag, 3.0, egui::Color32::from_rgb(50, 54, 63));
                    painter.text(tag.center(), egui::Align2::CENTER_CENTER, &wt, tiny_font.clone(), label4);
                }
                // Rollup badges: error > blocked > waiting > running.
                let mut badge_right = inner.right() - 4.0;
                let mut badge_keys: Vec<&str> = group.counts.keys().map(|s| s.as_str()).collect();
                badge_keys.sort_by_key(|k| std::cmp::Reverse(state_rank(k)));
                for key in badge_keys {
                    let count = group.counts[key];
                    let color = state_color(key);
                    let text = format!("{} {}", state_badge_glyph(key), count);
                    let size = painter.layout(text.clone(), tiny_font.clone(), color, f32::INFINITY).size();
                    let badge_w = size.x + 8.0;
                    badge_right -= badge_w;
                    let badge = egui::Rect::from_min_size(
                        egui::pos2(badge_right, inner.center().y - 7.0),
                        egui::vec2(badge_w, 14.0),
                    );
                    painter.rect_filled(badge, 7.0, color.gamma_multiply(0.16));
                    painter.text(badge.center(), egui::Align2::CENTER_CENTER, &text, tiny_font.clone(), color);
                    badge_right -= 5.0;
                }
            }
            SidebarRow::Tab(entry) => {
                let inner = row_rect.shrink2(egui::vec2(6.0, 2.0));
                let row_hovered = hovered == Some(&SidebarHover::Tab(entry.tab_id));
                let accent = state_color(&entry.status_key);
                let urgency_color = urgency_color(entry.urgency, &entry.status_key);

                if entry.active {
                    painter.rect_filled(inner, 6.0, accent);
                } else if entry.urgency > 0 {
                    let tint = if entry.urgency == 2 {
                        egui::Color32::from_rgb(55, 37, 43)
                    } else {
                        egui::Color32::from_rgb(43, 45, 52)
                    };
                    painter.rect_filled(inner, 6.0, tint);
                } else if row_hovered {
                    painter.rect_filled(inner, 6.0, egui::Color32::from_rgb(38, 42, 50));
                }

                // Leading urgency capsule.
                if entry.urgency > 0 {
                    let capsule_h = inner.height() * if entry.urgency == 2 { 0.55 + 0.22 * breathe(now) } else { 0.55 };
                    let capsule = egui::Rect::from_min_size(
                        egui::pos2(inner.left() + 3.0, inner.center().y - capsule_h * 0.5),
                        egui::vec2(3.0, capsule_h),
                    );
                    painter.rect_filled(capsule, 2.0, urgency_color);
                }

                // Active connector tongue.
                if entry.active {
                    let tongue = egui::Rect::from_min_size(
                        egui::pos2(inner.right() - 2.0, inner.center().y - inner.height() * 0.22),
                        egui::vec2(10.0, inner.height() * 0.44),
                    );
                    painter.rect_filled(tongue, 4.0, accent);
                    painter.rect_stroke(tongue, 4.0, egui::Stroke::new(2.0_f32, egui::Color32::WHITE), egui::StrokeKind::Outside);
                    painter.rect_stroke(inner, 6.0, egui::Stroke::new(2.0_f32, egui::Color32::WHITE), egui::StrokeKind::Outside);
                }

                let text_color = if entry.active { egui::Color32::WHITE } else { label };
                let meta_color = if entry.active { egui::Color32::WHITE } else { label3 };
                let indent = entry.groups.len().saturating_sub(1) as f32 * 12.0;
                let mut x = inner.left() + 12.0 + indent;

                if !entry.harness_glyph.is_empty() {
                    painter.text(egui::pos2(x, inner.center().y), egui::Align2::LEFT_CENTER, &entry.harness_glyph, font.clone(), label3);
                    x += 16.0;
                }
                if !entry.status_glyph.is_empty() {
                    painter.text(egui::pos2(x, inner.center().y), egui::Align2::LEFT_CENTER, &entry.status_glyph, font.clone(), accent);
                    x += 18.0;
                }

                let title = if entry.title.len() > 28 { format!("{}…", &entry.title[..27]) } else { entry.title.clone() };
                painter.text(egui::pos2(x, inner.center().y), egui::Align2::LEFT_CENTER, &title, label_font.clone(), text_color);
                let title_size = painter.layout(title, label_font.clone(), text_color, f32::INFINITY).size();
                x += title_size.x + 8.0;

                if !entry.progress.is_empty() {
                    let size = painter.layout(entry.progress.clone(), tiny_font.clone(), meta_color, f32::INFINITY).size();
                    let pill = egui::Rect::from_min_size(
                        egui::pos2(x, inner.center().y - 7.0),
                        egui::vec2(size.x + 8.0, 14.0),
                    );
                    painter.rect_filled(pill, 3.0, egui::Color32::from_rgb(60, 64, 74));
                    painter.text(pill.center(), egui::Align2::CENTER_CENTER, &entry.progress, tiny_font.clone(), meta_color);
                }

                if !entry.right.is_empty() {
                    painter.text(
                        egui::pos2(inner.right() - 8.0, inner.center().y),
                        egui::Align2::RIGHT_CENTER,
                        &entry.right,
                        tiny_font.clone(),
                        if entry.active { egui::Color32::WHITE } else { label2 },
                    );
                }
            }
        }

        if let (SidebarRow::Tab(entry), Some(target)) = (item, drop_target) {
            if entry.tab_id == target.tab_id {
                let y = if target.before { row_rect.top() } else { row_rect.bottom() };
                painter.line_segment(
                    [egui::pos2(row_rect.left() + 6.0, y), egui::pos2(row_rect.right() - 6.0, y)],
                    egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(24, 132, 245)),
                );
            }
        }
    }
    }

    let full_output = ctx.end_pass();
    let textures_delta = full_output.textures_delta;
    let paint_jobs = ctx.tessellate(full_output.shapes, pixels_per_point);

    let screen_descriptor = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [pixel_w, pixel_h],
        pixels_per_point,
    };

    for (id, delta) in &textures_delta.set {
        renderer.update_texture(device, queue, *id, delta);
    }
    let user_cmd_bufs =
        renderer.update_buffers(device, queue, encoder, &paint_jobs, &screen_descriptor);
    for id in &textures_delta.free {
        renderer.free_texture(id);
    }

    // Second pass over the surface view, preserving WezTerm's drawn pixels.
    {
        let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("egui pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
        });
        let mut render_pass = render_pass.forget_lifetime();
        renderer.render(&mut render_pass, &paint_jobs, &screen_descriptor);
    }

    Ok(user_cmd_bufs)
}

/// Register JetBrainsMono and SymbolsNerdFontMono into the egui context so
/// Nerd Font / powerline glyphs render in the sidebar. Embeds the same vendored
/// assets WezTerm uses for its terminal fonts (compile-time, no runtime fs).
/// Called once when the egui context is first created.
fn register_egui_fonts(ctx: &egui::Context) {
    use std::sync::Arc;

    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "JetBrainsMono".to_string(),
        Arc::new(egui::FontData::from_owned(
            include_bytes!("../../../../assets/fonts/JetBrainsMono-Regular.ttf").to_vec(),
        )),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_insert_with(Vec::new)
            .insert(0, "JetBrainsMono".to_string());
    }

    fonts.font_data.insert(
        "SymbolsNerdFontMono".to_string(),
        Arc::new(egui::FontData::from_owned(
            include_bytes!("../../../../assets/fonts/SymbolsNerdFontMono-Regular.ttf").to_vec(),
        )),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_insert_with(Vec::new)
        .push("SymbolsNerdFontMono".to_string());

    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod test {
    use std::path::Path;

    #[test]
    fn sidebar_compact_width_is_smaller_than_regular_width() {
        assert!(crate::termwindow::tab_sidebar::COMPACT_WIDTH_CELLS < 34);
    }

    #[test]
    fn sidebar_screenshot_offsets_get_stable_names() {
        assert_eq!(
            super::screenshot_path(Path::new("/tmp/sidebar.png"), 0, 0),
            Path::new("/tmp/sidebar.png")
        );
        assert_eq!(
            super::screenshot_path(Path::new("/tmp/sidebar.png"), 1, 100),
            Path::new("/tmp/sidebar-100ms.png")
        );
    }
}
