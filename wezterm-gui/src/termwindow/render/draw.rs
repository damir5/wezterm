use crate::colorease::ColorEaseUniform;
use crate::termwindow::sidebar_ui::{self, PaintMode, UiLayout};
use crate::termwindow::tab_sidebar::SidebarHover;
use crate::termwindow::webgpu::ShaderUniform;
use crate::termwindow::RenderFrame;
use crate::uniforms::UniformBuilder;
use ::window::glium;
use ::window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use ::window::WindowOps;
use anyhow::Context;
use config::FreeTypeLoadTarget;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::time::Duration;

fn rebuilds_cached_frame(cache_frame: bool, use_cached_terminal: bool) -> bool {
    cache_frame && !use_cached_terminal
}

struct SidebarCaptureSpec {
    index: usize,
    path: PathBuf,
    width: u32,
    height: u32,
    bytes_per_row: u32,
    format: wgpu::TextureFormat,
}

impl crate::TermWindow {
    pub fn call_draw(&mut self, frame: &mut RenderFrame, sidebar_only: bool) -> anyhow::Result<()> {
        match frame {
            RenderFrame::Glium(ref mut frame) => self.call_draw_glium(frame),
            RenderFrame::WebGpu => self.call_draw_webgpu(sidebar_only),
        }
    }

    fn ensure_terminal_cache(
        &mut self,
        webgpu: &crate::termwindow::webgpu::WebGpuState,
    ) -> anyhow::Result<()> {
        let config = webgpu.config.borrow();
        let size = (config.width, config.height, config.format);
        if self.terminal_cache_size == Some(size)
            && self.terminal_cache.is_some()
            && self.terminal_cache_bind_group.is_some()
        {
            return Ok(());
        }

        let texture = webgpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terminal frame cache"),
            size: wgpu::Extent3d {
                width: config.width,
                height: config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &config.view_formats,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &webgpu.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&webgpu.texture_linear_sampler),
                },
            ],
            label: Some("terminal frame cache bind group"),
        });
        self.terminal_cache = Some(texture);
        self.terminal_cache_size = Some(size);
        self.terminal_cache_bind_group = Some(bind_group);
        Ok(())
    }

    fn blit_terminal_cache(
        &self,
        webgpu: &crate::termwindow::webgpu::WebGpuState,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
    ) {
        let mut render_pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terminal frame cache blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            })
            .forget_lifetime();
        render_pass.set_pipeline(&webgpu.blit_pipeline);
        render_pass.set_bind_group(0, self.terminal_cache_bind_group.as_ref().unwrap(), &[]);
        render_pass.draw(0..3, 0..1);
    }

    fn call_draw_webgpu(&mut self, sidebar_only: bool) -> anyhow::Result<()> {
        use crate::termwindow::webgpu::WebGpuTexture;

        let pixels_per_point = (self.dimensions.dpi as f32 / 96.0).max(1.0);
        let (padding_left, padding_top) = self.padding_left_top();
        let border = self.get_os_border();
        let top_bar_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let pane_origin_y = top_bar_height + padding_top + border.top.get() as f32;
        let pane_rects = self
            .get_panes_to_render()
            .into_iter()
            .map(|pane| {
                let min = egui::pos2(
                    (padding_left
                        + border.left.get() as f32
                        + pane.left as f32 * self.render_metrics.cell_size.width as f32)
                        / pixels_per_point,
                    (pane_origin_y + pane.top as f32 * self.render_metrics.cell_size.height as f32)
                        / pixels_per_point,
                );
                crate::frontend::InputStackPaneRect {
                    pane_id: pane.pane.pane_id(),
                    rect: egui::Rect::from_min_size(
                        min,
                        egui::vec2(
                            pane.pixel_width as f32 / pixels_per_point,
                            pane.pixel_height as f32 / pixels_per_point,
                        ),
                    ),
                }
            })
            .collect::<Vec<_>>();
        let pending_capture = self.sidebar_capture_spec();
        let screenshot_hover = self
            .sidebar_screenshot
            .as_ref()
            .and_then(|request| request.hover);
        if let Some((x, y)) = screenshot_hover {
            self.set_sidebar_hover_at(x, y);
        }
        let webgpu = std::rc::Rc::clone(self.webgpu.as_ref().unwrap());

        let output = webgpu.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let cache_key = {
            let config = webgpu.config.borrow();
            (config.width, config.height, config.format)
        };
        let cache_valid = self.terminal_cache_size == Some(cache_key)
            && self.terminal_cache.is_some()
            && self.terminal_cache_bind_group.is_some()
            && self.terminal_cache_valid;
        let cache_frame = self.sidebar_cache_needed || sidebar_only;
        if cache_frame {
            self.ensure_terminal_cache(&webgpu)?;
        }
        let use_cached_terminal = sidebar_only && cache_valid;
        let terminal_view = cache_frame.then(|| {
            self.terminal_cache
                .as_ref()
                .unwrap()
                .create_view(&wgpu::TextureViewDescriptor::default())
        });
        let terminal_target = terminal_view.as_ref().unwrap_or(&view);
        let mut encoder = webgpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });
        let mut terminal_cleared = false;
        if !use_cached_terminal {
            let render_state = self.render_state.as_ref().unwrap();
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
                            resource: wgpu::BindingResource::Sampler(
                                &webgpu.texture_linear_sampler,
                            ),
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
                            resource: wgpu::BindingResource::Sampler(
                                &webgpu.texture_nearest_sampler,
                            ),
                        },
                    ],
                    label: Some("nearest bind group"),
                });
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
                        let mut render_pass =
                            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("Render Pass"),
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view: terminal_target,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        load: if terminal_cleared {
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
                        terminal_cleared = true;

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
                        render_pass.set_index_buffer(
                            vb.indices.webgpu().slice(..),
                            wgpu::IndexFormat::Uint32,
                        );
                        render_pass.draw_indexed(0..index_count as _, 0, 0..1);
                    }

                    vb.next_index();
                }
            }
        }

        if !use_cached_terminal && !terminal_cleared {
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Empty terminal clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: terminal_target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
        }

        let mut egui_cmd_bufs = Vec::new();
        if rebuilds_cached_frame(cache_frame, use_cached_terminal) && self.tab_sidebar_enabled {
            let config = webgpu.config.borrow();
            let linear_format = config.format.remove_srgb_suffix();
            let egui_format = if config.view_formats.contains(&linear_format) {
                linear_format
            } else {
                config.format
            };
            drop(config);
            let egui_view =
                self.terminal_cache
                    .as_ref()
                    .unwrap()
                    .create_view(&wgpu::TextureViewDescriptor {
                        format: Some(egui_format),
                        ..Default::default()
                    });
            let ui_layout = self.tab_sidebar.ui_layout.as_ref();
            let hovered = self.tab_sidebar.hovered.clone();
            let sidebar_width = self.tab_sidebar_width_pixels() as u32;
            egui_cmd_bufs.extend(composite_tab_sidebar(
                &mut self.egui_ctx,
                &mut self.egui_renderer,
                hovered.as_ref(),
                ui_layout,
                self.tab_sidebar.ui_scroll_offset,
                Some(self.created.elapsed().as_secs_f64()),
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
                PaintMode::Static,
                true,
            )?);
        }

        if cache_frame {
            self.terminal_cache_valid = true;
            self.blit_terminal_cache(&webgpu, &mut encoder, &view);
        }

        // The sidebar has one egui context per terminal window.  It is window
        // chrome, so its cached model is rendered once rather than once per
        // mux pane.
        if self.tab_sidebar_enabled {
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
            let ui_layout = self.tab_sidebar.ui_layout.as_ref();
            let hovered = self.tab_sidebar.hovered.clone();
            let sidebar_width = self.tab_sidebar_width_pixels() as u32;
            egui_cmd_bufs.extend(composite_tab_sidebar(
                if cache_frame {
                    &mut self.egui_animation_ctx
                } else {
                    &mut self.egui_ctx
                },
                if cache_frame {
                    &mut self.egui_animation_renderer
                } else {
                    &mut self.egui_renderer
                },
                hovered.as_ref(),
                ui_layout,
                self.tab_sidebar.ui_scroll_offset,
                Some(self.created.elapsed().as_secs_f64()),
                sidebar_width,
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
                (self.dimensions.dpi as f32 / 96.0).max(1.0),
                &webgpu.device,
                &webgpu.queue,
                egui_format,
                &egui_view,
                &mut encoder,
                if cache_frame {
                    &mut self.sidebar_animation_images
                } else {
                    &mut self.sidebar_images
                },
                if cache_frame {
                    PaintMode::Animated
                } else {
                    PaintMode::All
                },
                !cache_frame,
            )?);
        }

        let front_end = crate::frontend::front_end();
        if front_end.input_stack_ui_is_active(self.mux_window_id)
            || front_end.has_input_stack_for_panes(&pane_rects)
            || front_end.has_paste_feedback(&pane_rects)
        {
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
            egui_cmd_bufs.extend(composite_input_stack(
                &mut self.input_stack_egui_ctx,
                &mut self.input_stack_egui_renderer,
                self.mux_window_id,
                &pane_rects,
                self.window.as_ref().unwrap(),
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
                pixels_per_point,
                &webgpu.device,
                &webgpu.queue,
                egui_format,
                &egui_view,
                &mut encoder,
            )?);
        }

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
        webgpu.queue.submit(
            egui_cmd_bufs
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        let capture_result = match (pending_capture, capture_buffer) {
            (Some(spec), Some(buffer)) => {
                let result = read_sidebar_png(&webgpu, buffer, &spec);
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
            path: screenshot_path(&request.path, request.next, offset_ms),
            width,
            height,
            bytes_per_row,
            format,
        })
    }

    fn finish_sidebar_capture(&mut self, spec: SidebarCaptureSpec, result: anyhow::Result<()>) {
        let Some(mut request) = self.sidebar_screenshot.take() else {
            return;
        };
        match result {
            Ok(()) => {
                request
                    .outputs
                    .push(spec.path.to_string_lossy().into_owned());
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
    rx.recv()
        .context("waiting for sidebar screenshot readback")??;

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

fn composite_input_stack(
    egui_ctx: &mut Option<egui::Context>,
    egui_renderer: &mut Option<egui_wgpu::Renderer>,
    mux_window_id: mux::window::WindowId,
    panes: &[crate::frontend::InputStackPaneRect],
    os_window: &window::Window,
    pixel_w: u32,
    pixel_h: u32,
    pixels_per_point: f32,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
    view: &wgpu::TextureView,
    encoder: &mut wgpu::CommandEncoder,
) -> anyhow::Result<Vec<wgpu::CommandBuffer>> {
    let ctx = sidebar_ui::context(egui_ctx);
    if egui_renderer.is_none() {
        *egui_renderer = Some(egui_wgpu::Renderer::new(device, format, None, 1, false));
    }
    let renderer = egui_renderer.as_mut().unwrap();
    ctx.set_pixels_per_point(pixels_per_point);
    let front_end = crate::frontend::front_end();
    ctx.begin_pass(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(
                pixel_w as f32 / pixels_per_point,
                pixel_h as f32 / pixels_per_point,
            ),
        )),
        events: front_end.take_input_stack_events(mux_window_id),
        ..Default::default()
    });
    front_end.paint_input_stack(&ctx, mux_window_id, panes, os_window);
    front_end.paint_paste_feedback(&ctx, panes);
    let full_output = ctx.end_pass();
    for command in &full_output.platform_output.commands {
        if let egui::OutputCommand::CopyText(text) = command {
            os_window.set_clipboard(window::Clipboard::Clipboard, text.clone());
        }
    }
    let textures_delta = full_output.textures_delta;
    let paint_jobs = ctx.tessellate(full_output.shapes, pixels_per_point);
    let screen_descriptor = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [pixel_w, pixel_h],
        pixels_per_point,
    };
    for (id, delta) in &textures_delta.set {
        renderer.update_texture(device, queue, *id, delta);
    }
    let command_buffers =
        renderer.update_buffers(device, queue, encoder, &paint_jobs, &screen_descriptor);
    for id in &textures_delta.free {
        renderer.free_texture(id);
    }
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("input stack egui"),
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
        let mut pass = pass.forget_lifetime();
        renderer.render(&mut pass, &paint_jobs, &screen_descriptor);
    }
    Ok(command_buffers)
}

/// Paint the cached, per-window sidebar into the same surface view as the
/// terminal.  Input is intentionally handled by native UIItem hit testing.
fn composite_tab_sidebar(
    egui_ctx: &mut Option<egui::Context>,
    egui_renderer: &mut Option<egui_wgpu::Renderer>,
    hovered: Option<&SidebarHover>,
    ui_layout: Option<&UiLayout>,
    scroll_offset: f32,
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
    mode: PaintMode,
    draw_backdrop: bool,
) -> anyhow::Result<Vec<wgpu::CommandBuffer>> {
    let ctx = sidebar_ui::context(egui_ctx);
    let ctx = &ctx;
    if egui_renderer.is_none() {
        *egui_renderer = Some(egui_wgpu::Renderer::new(device, format, None, 1, false));
    }
    let renderer = egui_renderer.as_mut().unwrap();

    ctx.set_pixels_per_point(pixels_per_point);
    let screen_rect = egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(
            pixel_w as f32 / pixels_per_point,
            pixel_h as f32 / pixels_per_point,
        ),
    );
    ctx.begin_pass(egui::RawInput {
        time: animation_time,
        screen_rect: Some(screen_rect),
        ..Default::default()
    });
    let rect = egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(
            sidebar_width as f32 / pixels_per_point,
            screen_rect.height(),
        ),
    );
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("tab-sidebar"),
    ));

    let bg = egui::Color32::from_rgb(34, 37, 44);
    let sep = egui::Color32::from_rgb(47, 51, 60);

    if draw_backdrop {
        painter.rect_filled(rect, 0.0, bg);
        painter.line_segment(
            [rect.right_top(), rect.right_bottom()],
            egui::Stroke::new(1.0_f32, sep),
        );
    }

    if let Some(layout) = ui_layout {
        let hovered = match hovered {
            Some(SidebarHover::Node(id)) => Some(id.as_str()),
            _ => None,
        };
        sidebar_ui::paint(
            &painter,
            ctx,
            layout,
            hovered,
            images,
            scroll_offset,
            animation_time.unwrap_or(0.0),
            mode,
            rect,
        );
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

    #[test]
    fn stale_sidebar_only_cache_rebuilds_static_sidebar() {
        assert!(super::rebuilds_cached_frame(true, false));
        assert!(!super::rebuilds_cached_frame(true, true));
        assert!(!super::rebuilds_cached_frame(false, false));
    }
}
