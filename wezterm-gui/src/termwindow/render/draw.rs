use crate::colorease::ColorEaseUniform;
use crate::termwindow::tab_sidebar::SidebarRow;
use crate::termwindow::webgpu::ShaderUniform;
use crate::termwindow::RenderFrame;
use crate::uniforms::UniformBuilder;
use ::window::glium;
use ::window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use config::FreeTypeLoadTarget;

impl crate::TermWindow {
    pub fn call_draw(&mut self, frame: &mut RenderFrame) -> anyhow::Result<()> {
        match frame {
            RenderFrame::Glium(ref mut frame) => self.call_draw_glium(frame),
            RenderFrame::WebGpu => self.call_draw_webgpu(),
        }
    }

    fn call_draw_webgpu(&mut self) -> anyhow::Result<()> {
        use crate::termwindow::webgpu::WebGpuTexture;

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
            let sidebar_width = self.tab_sidebar_width_pixels() as u32;
            composite_tab_sidebar(
                &mut self.egui_ctx,
                &mut self.egui_renderer,
                &rows,
                self.tab_sidebar.scroll_rows,
                self.tab_sidebar.compact,
                sidebar_width,
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
                (self.dimensions.dpi as f32 / 96.0).max(1.0),
                &webgpu.device,
                &webgpu.queue,
                egui_format,
                &egui_view,
                &mut encoder,
            )?
        } else {
            Vec::new()
        };

        // Submit order matches the canonical egui-wgpu flow: the callback
        // command buffers from update_buffers first, then the encoded pass.
        webgpu.queue.submit(egui_cmd_bufs.into_iter().chain(std::iter::once(encoder.finish())));
        output.present();

        Ok(())
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

/// Paint the cached, per-window sidebar into the same surface view as the
/// terminal.  Input is intentionally handled by native UIItem hit testing.
fn composite_tab_sidebar(
    egui_ctx: &mut Option<egui::Context>,
    egui_renderer: &mut Option<egui_wgpu::Renderer>,
    rows: &[SidebarRow],
    scroll_rows: usize,
    compact: bool,
    sidebar_width: u32,
    pixel_w: u32,
    pixel_h: u32,
    pixels_per_point: f32,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
    view: &wgpu::TextureView,
    encoder: &mut wgpu::CommandEncoder,
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
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(25, 27, 33));
    painter.line_segment(
        [rect.right_top(), rect.right_bottom()],
        egui::Stroke::new(1.0_f32, egui::Color32::from_gray(70)),
    );
    // UIItem rectangles use physical pixels; convert the shared 24 px row
    // contract to egui points so visual and hit-test geometry stay identical.
    let row_height = 24.0 / pixels_per_point;
    let font = egui::FontId::monospace(13.0);
    for (row, item) in rows.iter().skip(scroll_rows).enumerate() {
        let y = row as f32 * row_height;
        if y >= rect.height() {
            break;
        }
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(0.0, y),
            egui::vec2(rect.width(), row_height),
        );
        let (label, right, active, urgency, status_color, is_group, indent) = match item {
            SidebarRow::Group(group) => (
                group.label.clone(), String::new(), group.active, group.urgency, None, true,
                group.depth as f32 * 12.0,
            ),
            SidebarRow::Tab(entry) => (
                if compact { entry.title.chars().take(2).collect() } else { format!("{} {}", entry.status_glyph, entry.title) },
                entry.right.clone(), entry.active, entry.urgency, entry.status_color.clone(), false, 0.0,
            ),
        };
        if active {
            painter.rect_filled(row_rect.shrink2(egui::vec2(3.0, 2.0)), 4.0, egui::Color32::from_rgb(30, 120, 230));
        }
        let color = status_color.as_deref().and_then(parse_color).unwrap_or_else(|| {
            if urgency == 2 { egui::Color32::from_rgb(255, 104, 110) }
            else if urgency == 1 { egui::Color32::from_rgb(235, 185, 80) }
            else if is_group { egui::Color32::from_gray(145) }
            else { egui::Color32::from_rgb(220, 222, 228) }
        });
        painter.text(
            row_rect.left_center() + egui::vec2(8.0 + indent, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            font.clone(),
            color,
        );
        if !compact && !right.is_empty() {
            painter.text(
                row_rect.right_center() - egui::vec2(8.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                &right,
                font.clone(),
                egui::Color32::from_gray(150),
            );
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

fn parse_color(value: &str) -> Option<egui::Color32> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 { return None; }
    Some(egui::Color32::from_rgb(
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
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
    #[test]
    fn sidebar_compact_width_is_smaller_than_regular_width() {
        assert!(crate::termwindow::tab_sidebar::COMPACT_WIDTH_CELLS < 34);
    }
}
