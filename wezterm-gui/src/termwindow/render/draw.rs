use crate::colorease::ColorEaseUniform;
use crate::termwindow::render::guipane_ui;
use crate::termwindow::webgpu::ShaderUniform;
use crate::termwindow::RenderFrame;
use crate::uniforms::UniformBuilder;
use ::window::glium;
use ::window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use config::FreeTypeLoadTarget;
use mux::guipane::GuiPane;
use mux::pane::Pane;
use std::collections::HashSet;
use std::sync::Arc;

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

        // egui compositing pass for GuiPane dashboards: replay each pane's
        // widget tree into a real egui frame and record it into the same
        // surface view via a second LoadOp::Load render pass. Skipped when no
        // GuiPane is visible this frame.
        let egui_cmd_bufs: Vec<wgpu::CommandBuffer> = if !self.gui_render_list.is_empty() {
            let format = webgpu.config.borrow().format;
            composite_egui_panes(
                &mut self.egui_ctx,
                &mut self.egui_renderer,
                &self.gui_render_list,
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
                (self.dimensions.dpi as f32 / 96.0).max(1.0),
                &webgpu.device,
                &webgpu.queue,
                format,
                &view,
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

/// Drive egui for every visible `GuiPane` and record its paint jobs into the
/// given encoder + surface view. Lazily initializes the egui context and
/// wgpu renderer on first use. Returns auxiliary command buffers produced by
/// `egui_wgpu::Renderer::update_buffers` that must be submitted alongside the
/// main encoder.
///
/// ponytail: `RawInput` carries no pointer/keyboard events yet, so widgets
/// render but do not interact. Forwarding winit events here (the plan's
/// "Event Interception" step) makes the deferred `clicked` plumbing live;
/// events accumulate on each pane and are drained per refresh tick, so clicks
/// won't be lost to the frame/refresh rate mismatch.
fn composite_egui_panes(
    egui_ctx: &mut Option<egui::Context>,
    egui_renderer: &mut Option<egui_wgpu::Renderer>,
    render_list: &[(euclid::default::Rect<f32>, Arc<dyn Pane>)],
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
    let raw_input = egui::RawInput {
        screen_rect: Some(screen_rect),
        ..Default::default()
    };
    ctx.begin_frame(raw_input);

    for (rect, pane) in render_list.iter() {
        let Some(g) = pane.downcast_ref::<GuiPane>() else {
            continue;
        };
        let theme = g.theme();
        let nodes = g.nodes();
        let mut clicked: HashSet<String> = HashSet::new();
        let pos = egui::pos2(rect.origin.x / pixels_per_point, rect.origin.y / pixels_per_point);
        egui::Area::new(egui::Id::new(pane.pane_id()))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .interactable(true)
            .show(ctx, |ui| {
                // Theme + clip are scoped to this pane's Ui so concurrent
                // GuiPanes don't bleed visuals into one another, and overflow
                // scrolls instead of spilling onto neighbouring panes.
                guipane_ui::apply_theme(ui, &theme);
                let size = egui::vec2(
                    rect.size.width / pixels_per_point,
                    rect.size.height / pixels_per_point,
                );
                egui::ScrollArea::vertical()
                    .max_width(size.x)
                    .max_height(size.y)
                    .show(ui, |ui| {
                        guipane_ui::render_nodes(ui, &nodes, &theme, &mut clicked);
                    });
            });
        g.set_events(clicked);
    }

    let full_output = ctx.end_frame();
    let shapes = full_output.shapes;
    let textures_delta = full_output.textures_delta;
    let paint_jobs = ctx.tessellate(shapes, pixels_per_point);

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
        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
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
