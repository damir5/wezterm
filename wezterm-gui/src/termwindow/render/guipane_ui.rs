//! Translates the plain-Rust `UiNode` tree (built by the Lua `render-gui-pane`
//! handler via `LuaUi`) into live egui calls.
//!
//! egui is immediate-mode, so each call here records the widget into the
//! `&mut egui::Ui` for the current frame. Interaction (clicks/toggles) is
//! collected into a `HashSet` of widget ids and surfaced back to the Lua
//! handler on the next `render-gui-pane` fire.
//!
//! ponytail: input is forwarded from winit (`forward_mouse_to_egui` /
//! `forward_key_to_egui`) into egui's `RawInput` each frame. Interaction
//! (clicks/toggles/slider changes) is collected into `clicked` and surfaced
//! to the next `render-gui-pane` fire; slider values are read back via
//! `ui:value(id)`.

use egui::{Color32, CornerRadius, Frame, Id, Response, RichText, Shape, Stroke, TextureHandle, Ui, Vec2};
use mux::guipane::{Color, FrameAnim, UiNode, UiStyle, UiTheme};
use std::collections::{HashMap, HashSet};

/// Convert a linear-ish RGBA (0.0..=1.0) to an egui `Color32`.
/// ponytail: no gamma correction; dashboard colors are approximate.
fn c(col: Color) -> Color32 {
    let f = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(f(col[0]), f(col[1]), f(col[2]), f(col[3]))
}

/// Apply a theme preset to this pane's `Ui` (scoped via `set_style`, so
/// concurrent GuiPanes keep independent visuals).
pub fn apply_theme(ui: &mut Ui, theme: &UiTheme) {
    let mut style: egui::Style = ui.style().as_ref().clone();
    let mut visuals = if theme.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = c(theme.background);
    visuals.window_fill = c(theme.background);
    visuals.faint_bg_color = c(theme.background);
    visuals.extreme_bg_color = c(theme.background);
    visuals.selection.bg_fill = c(theme.accent);
    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(theme.spacing, theme.spacing * 0.7);
    style.spacing.button_padding = Vec2::new(theme.spacing * 0.6, theme.spacing * 0.3);
    style.spacing.window_margin = theme.spacing.into();
    ui.set_style(style);
}

/// Build an egui `RichText` from a string + style, inheriting theme defaults.
fn styled_text(text: String, style: &UiStyle, theme: &UiTheme) -> RichText {
    let mut rt = RichText::new(text).color(style.color.map(c).unwrap_or(c(theme.foreground)));
    if let Some(size) = style.size {
        rt = rt.size(size);
    }
    if style.strong.unwrap_or(false) {
        rt = rt.strong();
    }
    if style.italics.unwrap_or(false) {
        rt = rt.italics();
    }
    if style.monospace.unwrap_or(false) {
        rt = rt.monospace();
    }
    if style.code.unwrap_or(false) {
        rt = rt.code();
    }
    if style.underline.unwrap_or(false) {
        rt = rt.underline();
    }
    if style.strikethrough.unwrap_or(false) {
        rt = rt.strikethrough();
    }
    rt
}

fn status_color(status: &Option<String>, theme: &UiTheme) -> Color32 {
    match status.as_deref() {
        Some("error") | Some("failed") => c(theme.error),
        Some("running") | Some("ok") => c(theme.success),
        Some("idle") | Some("paused") | Some("warning") => c(theme.warning),
        _ => c(theme.accent),
    }
}

/// Build a `Frame` from a style override + theme, optionally translucent.
fn frame_for(style: &UiStyle, theme: &UiTheme, glass: bool) -> Frame {
    let fill = style.fill.unwrap_or({
        let mut bg = theme.background;
        if glass {
            bg[3] = 0.55;
        }
        bg
    });
    let rounding = style.rounding.unwrap_or(theme.rounding);
    let stroke = style
        .stroke
        .map(|(col, w)| Stroke::new(w, c(col)))
        .unwrap_or(Stroke::new(1.0, c(theme.dim)));
    Frame {
        inner_margin: style.margin.unwrap_or(theme.spacing).into(),
        fill: c(fill),
        stroke,
        corner_radius: CornerRadius::same(rounding as u8),
        ..Default::default()
    }
}

/// Attach a tooltip if the style declares one. Returns the new response.
fn with_tooltip(response: Response, style: &UiStyle) -> Response {
    if let Some(tip) = &style.tooltip {
        response.on_hover_text(tip.clone())
    } else {
        response
    }
}

/// Render a slice of nodes into `ui`, appending any activated widget ids to
/// `clicked`, recording slider values into `values`, and collapsing-header
/// states (id → collapsed) into `collapsed`. Container nodes recurse.
pub fn render_nodes(
    ui: &mut Ui,
    nodes: &[UiNode],
    theme: &UiTheme,
    clicked: &mut HashSet<String>,
    values: &mut HashMap<String, f64>,
    collapsed: &mut HashMap<String, bool>,
) {
    for node in nodes {
        render_node(ui, node, theme, clicked, values, collapsed);
    }
}

fn render_node(
    ui: &mut Ui,
    node: &UiNode,
    theme: &UiTheme,
    clicked: &mut HashSet<String>,
    values: &mut HashMap<String, f64>,
    collapsed: &mut HashMap<String, bool>,
) {
    match node {
        UiNode::Metric {
            label,
            value,
            status,
            style,
        } => {
            ui.horizontal(|ui| {
                ui.label(styled_text(label.clone(), style, theme).color(c(theme.dim)));
                ui.label(
                    styled_text(value.clone(), style, theme)
                        .color(status_color(status, theme))
                        .strong(),
                );
            });
        }
        UiNode::Label { text, style } => {
            let r = ui.label(styled_text(text.clone(), style, theme));
            with_tooltip(r, style);
        }
        UiNode::Heading { text, size, style } => {
            let mut s = style.clone();
            s.size.get_or_insert(size.unwrap_or(theme.font_size * 1.4));
            s.strong = Some(true);
            ui.label(styled_text(text.clone(), &s, theme));
            ui.separator();
        }
        UiNode::Button { text, id, style } => {
            let mut btn = egui::Button::new(styled_text(text.clone(), style, theme));
            if style.accent.unwrap_or(false) {
                btn = btn.fill(c(theme.accent));
            }
            if let Some(r) = style.rounding {
                btn = btn.corner_radius(r);
            }
            let r = ui.add(btn);
            if r.clicked() {
                clicked.insert(id.clone());
            }
            with_tooltip(r, style);
        }
        UiNode::Hyperlink { text, url, style } => {
            let r = ui.hyperlink_to(styled_text(text.clone(), style, theme), url);
            with_tooltip(r, style);
        }
        UiNode::Checkbox {
            id,
            label,
            checked,
            style,
        } => {
            // Prefer persisted state (1.0/0.0) so toggles persist between Lua
            // refreshes and are read back via `ui:value(id)`.
            let mut v = values.get(id).map(|x| *x != 0.0).unwrap_or(*checked);
            let r = ui.checkbox(&mut v, styled_text(label.clone(), style, theme));
            values.insert(id.clone(), if v { 1.0 } else { 0.0 });
            if r.changed() {
                clicked.insert(id.clone());
            }
            with_tooltip(r, style);
        }
        // ponytail: toggle renders as a checkbox; egui has no native switch.
        UiNode::Toggle {
            id,
            label,
            on,
            style,
        } => {
            let mut v = values.get(id).map(|x| *x != 0.0).unwrap_or(*on);
            let r = ui.checkbox(&mut v, styled_text(label.clone(), style, theme));
            values.insert(id.clone(), if v { 1.0 } else { 0.0 });
            if r.changed() {
                clicked.insert(id.clone());
            }
        }
        UiNode::Radio {
            id,
            label,
            selected,
            style,
        } => {
            let mut sel = *selected;
            let r = ui.radio_value(&mut sel, true, styled_text(label.clone(), style, theme));
            if r.clicked() {
                clicked.insert(id.clone());
            }
        }
        UiNode::Slider {
            id,
            value,
            min,
            max,
            step,
            label,
            style,
        } => {
            // Prefer the value persisted by the last render pass over the
            // (possibly stale, up to ~100 ms old) tree value. This lets an
            // active drag stay smooth frame-to-frame instead of snapping back
            // to the last-flushed Lua value; Lua catches up via `ui:value(id)`.
            let mut v = values.get(id).copied().unwrap_or(*value);
            let mut s = egui::Slider::new(&mut v, (*min)..=(*max));
            if let Some(step) = step {
                s = s.step_by(*step);
            }
            if let Some(text) = label {
                s = s.text(text);
            }
            let r = ui.add(s);
            // Always record the rendered value so it survives until the next
            // Lua refresh, and report a change so Lua can react.
            values.insert(id.clone(), v);
            if r.changed() {
                clicked.insert(id.clone());
            }
            with_tooltip(r, style);
        }
        UiNode::ProgressBar {
            fraction,
            animated: _,
            style,
        } => {
            let r = ui.add(egui::ProgressBar::new(*fraction));
            with_tooltip(r, style);
        }
        UiNode::Spinner { style } => {
            let r = ui.add(egui::Spinner::new());
            with_tooltip(r, style);
        }
        UiNode::Separator { style } => {
            let _ = style;
            ui.separator();
        }
        UiNode::Spacing { amount } => ui.add_space(*amount),
        UiNode::Sparkline {
            values,
            color,
            fill,
            height,
            style: _,
        } => render_sparkline(ui, values, *color, *fill, *height, theme),
        UiNode::Image {
            id,
            bytes,
            width,
            height,
        } => render_image(ui, id, bytes, *width, *height),
        UiNode::Card {
            title,
            glass,
            style,
            children,
        } => {
            frame_for(style, theme, *glass).show(ui, |ui| {
                if let Some(title) = title {
                    ui.heading(title);
                    ui.add_space(2.0);
                }
                render_nodes(ui, children, theme, clicked, values, collapsed);
            });
        }
        UiNode::CollapsingHeader {
            id,
            title,
            default_open,
            style: _,
            children,
        } => {
            // The header id is the egui state key and the id Lua reads back via
            // ui:collapsed(id); default to the title so state survives a reload
            // even when Lua omits an explicit id.
            let hid = id.clone().unwrap_or_else(|| title.clone());
            let resp = egui::CollapsingHeader::new(title)
                .id_salt(&hid)
                .default_open(*default_open)
                .show(ui, |ui| {
                    render_nodes(ui, children, theme, clicked, values, collapsed);
                });
            // body_response is None iff collapsed.
            collapsed.insert(hid, resp.body_response.is_none());
        }
        UiNode::Frame {
            clickable,
            anim,
            style,
            children,
        } => {
            // Animate a vertical lead offset toward `target`; egui owns the
            // eased value so it advances every frame, smooth between refreshes.
            if let Some(anim) = anim {
                ui.add_space(frame_anim_offset(ui.ctx(), anim));
            }
            let inner = frame_for(style, theme, false).show(ui, |ui| {
                render_nodes(ui, children, theme, clicked, values, collapsed);
            });
            // Whole-rect hit target (a borderless source-list row). Drawn after
            // children so the frame is visible; the interact adds a click region
            // over it without displacing child widgets.
            if let Some(id) = clickable {
                let rect = inner.response.rect;
                let r = ui.interact(rect, Id::new(id.as_str()), egui::Sense::click());
                if r.clicked() {
                    clicked.insert(id.clone());
                }
            }
        }
        UiNode::Arc {
            value,
            label,
            color,
            thickness,
            style,
        } => render_arc(ui, *value, label.as_deref(), *color, *thickness, theme, style),
        UiNode::Horizontal {
            style: _,
            children,
        } => {
            ui.horizontal(|ui| {
                render_nodes(ui, children, theme, clicked, values, collapsed);
            });
        }
        UiNode::Vertical {
            style: _,
            children,
        } => {
            ui.vertical(|ui| {
                render_nodes(ui, children, theme, clicked, values, collapsed);
            });
        }
        UiNode::Columns {
            n,
            style: _,
            children,
        } => {
            // Distribute children round-robin across `n` columns.
            let n = (*n).max(1);
            ui.columns(n, |uis| {
                for (col, cui) in uis.iter_mut().enumerate() {
                    for child in children.iter().skip(col).step_by(n) {
                        render_node(cui, child, theme, clicked, values, collapsed);
                    }
                }
            });
        }
        UiNode::Grid {
            id: _,
            style: _,
            children,
        } => {
            ui.horizontal_wrapped(|ui| {
                render_nodes(ui, children, theme, clicked, values, collapsed);
            });
        }
    }
}

/// Draw a sparkline directly with the egui painter (no plot dependency).
fn render_sparkline(
    ui: &mut Ui,
    values: &[f32],
    color: Option<Color>,
    fill: bool,
    height: Option<f32>,
    theme: &UiTheme,
) {
    let h = height.unwrap_or(60.0).max(1.0);
    let (rect, _) = ui.allocate_at_least(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    if values.len() < 2 {
        return;
    }
    let max = values.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let min = values.iter().cloned().fold(f32::INFINITY, f32::min);
    let range = (max - min).max(1e-6);
    let denom = (values.len() - 1) as f32;
    let pts: Vec<egui::Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + (i as f32 / denom) * rect.width();
            let y = rect.bottom() - ((v - min) / range) * rect.height();
            egui::pos2(x, y)
        })
        .collect();
    let col = color.map(c).unwrap_or(c(theme.accent));
    let painter = ui.painter();
    if fill {
        let mut poly = pts.clone();
        poly.push(egui::pos2(rect.right(), rect.bottom()));
        poly.push(egui::pos2(rect.left(), rect.bottom()));
        painter.add(Shape::convex_polygon(poly, col.linear_multiply(0.25), Stroke::NONE));
    }
    painter.add(Shape::line(pts, Stroke::new(1.5, col)));
}

/// Eased vertical offset for an animated Frame, advanced every frame by egui
/// (so motion is smooth between the ~10 Hz Lua refreshes). `target` is the
/// offset in points.
fn frame_anim_offset(ctx: &egui::Context, anim: &FrameAnim) -> f32 {
    let dur = anim.duration.unwrap_or(0.2).max(0.0);
    ctx.animate_value_with_time(Id::new(anim.id.as_str()), anim.target, dur)
}

/// Draw a donut/ring gauge: a dim track plus a colored arc for `value`
/// (0..=1), with an optional centered label.
/// ponytail: arc is a thick stroked polyline over a stroked track ring; no
/// custom tessellator. Add anti-aliased fill wedges only if needed.
fn render_arc(
    ui: &mut Ui,
    value: f32,
    label: Option<&str>,
    color: Option<Color>,
    thickness: Option<f32>,
    theme: &UiTheme,
    style: &UiStyle,
) {
    let frac = value.clamp(0.0, 1.0);
    let size = style.size.unwrap_or(48.0).max(8.0);
    let (rect, _) = ui.allocate_at_least(Vec2::splat(size), egui::Sense::hover());
    let painter = ui.painter();
    let center = rect.center();
    let radius = rect.width().min(rect.height()) * 0.5;
    let stroke_w = thickness.unwrap_or((radius * 0.22).max(3.0));
    let col = color.map(c).unwrap_or(c(theme.accent));

    // Dim full ring as the background track.
    painter.add(Shape::circle_stroke(
        center,
        radius,
        Stroke::new(stroke_w, c(theme.dim).linear_multiply(0.5)),
    ));
    // Colored arc on top, swept clockwise from 12 o'clock.
    if frac > 0.0 {
        let segments = (frac * 64.0).round() as usize + 1;
        let sweep = frac * std::f32::consts::TAU;
        let pts: Vec<egui::Pos2> = (0..segments)
            .map(|i| {
                let a = -std::f32::consts::FRAC_PI_2
                    + (i as f32 / (segments - 1).max(1) as f32) * sweep;
                egui::pos2(center.x + radius * a.cos(), center.y + radius * a.sin())
            })
            .collect();
        painter.add(Shape::line(pts, Stroke::new(stroke_w, col)));
    }
    if let Some(label) = label {
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(radius * 0.7),
            c(theme.foreground),
        );
    }
}

/// egui Context temp-data key for the per-window GuiPane image texture cache.
const IMAGE_CACHE: &str = "guipane-images";

/// Decode `bytes` (PNG/JPEG/etc. via the `image` crate) into an egui texture.
fn load_image_texture(ctx: &egui::Context, bytes: &[u8]) -> Option<TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::epaint::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture("guipane-image", color, Default::default()))
}

/// Render an image node, caching decoded textures in egui context temp data so
/// repeated frames don't re-decode/re-upload. Keyed by the node's `id`.
fn render_image(ui: &mut Ui, id: &str, bytes: &[u8], width: Option<f32>, height: Option<f32>) {
    let ctx = ui.ctx().clone();
    let cache_id = Id::new(IMAGE_CACHE);

    let missing = ctx.data(|d| {
        d.get_temp::<HashMap<String, TextureHandle>>(cache_id)
            .map(|c| !c.contains_key(id))
            .unwrap_or(true)
    });
    if missing {
        if let Some(tex) = load_image_texture(&ctx, bytes) {
            ctx.data_mut(|d| {
                d.get_temp_mut_or_default::<HashMap<String, TextureHandle>>(cache_id)
                    .insert(id.to_string(), tex);
            });
        }
    }

    let tex = ctx.data(|d| {
        d.get_temp::<HashMap<String, TextureHandle>>(cache_id)
            .and_then(|c| c.get(id).cloned())
    });
    if let Some(tex) = tex {
        let nat = tex.size_vec2();
        let size = Vec2::new(width.unwrap_or(nat.x), height.unwrap_or(nat.y));
        ui.add(egui::Image::from_texture(&tex).max_size(size));
    } else {
        ui.label("[image decode failed]");
    }
}
