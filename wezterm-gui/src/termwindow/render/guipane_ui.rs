//! Translates the plain-Rust `UiNode` tree (built by the Lua `render-gui-pane`
//! handler via `LuaUi`) into live egui calls.
//!
//! egui is immediate-mode, so each call here records the widget into the
//! `&mut egui::Ui` for the current frame. Interaction (clicks/toggles) is
//! collected into a `HashSet` of widget ids and surfaced back to the Lua
//! handler on the next `render-gui-pane` fire.
//!
//! ponytail: input events (mouse/keyboard) are not yet forwarded from winit
//! into egui's `RawInput`, so widgets render but do not yet react. The
//! `clicked` plumbing is wired end-to-end; enabling interaction only needs
//! `RawInput` population in `call_draw_webgpu`.

use egui::{Color32, CornerRadius, Frame, Response, RichText, Shape, Stroke, Ui, Vec2};
use mux::guipane::{Color, UiNode, UiStyle, UiTheme};
use std::collections::HashSet;

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
/// `clicked`. Container nodes recurse into their children.
pub fn render_nodes(ui: &mut Ui, nodes: &[UiNode], theme: &UiTheme, clicked: &mut HashSet<String>) {
    for node in nodes {
        render_node(ui, node, theme, clicked);
    }
}

fn render_node(ui: &mut Ui, node: &UiNode, theme: &UiTheme, clicked: &mut HashSet<String>) {
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
            let mut v = *checked;
            let r = ui.checkbox(&mut v, styled_text(label.clone(), style, theme));
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
            let mut v = *on;
            let r = ui.checkbox(&mut v, styled_text(label.clone(), style, theme));
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
            let mut v = *value;
            let mut s = egui::Slider::new(&mut v, (*min)..=(*max));
            if let Some(step) = step {
                s = s.step_by(*step);
            }
            if let Some(text) = label {
                s = s.text(text);
            }
            let r = ui.add(s);
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
                render_nodes(ui, children, theme, clicked);
            });
        }
        UiNode::CollapsingHeader {
            title,
            default_open,
            style: _,
            children,
        } => {
            egui::CollapsingHeader::new(title)
                .default_open(*default_open)
                .show(ui, |ui| {
                    render_nodes(ui, children, theme, clicked);
                });
        }
        UiNode::Frame { style, children } => {
            frame_for(style, theme, false).show(ui, |ui| {
                render_nodes(ui, children, theme, clicked);
            });
        }
        UiNode::Horizontal {
            style: _,
            children,
        } => {
            ui.horizontal(|ui| {
                render_nodes(ui, children, theme, clicked);
            });
        }
        UiNode::Vertical {
            style: _,
            children,
        } => {
            ui.vertical(|ui| {
                render_nodes(ui, children, theme, clicked);
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
                        render_node(cui, child, theme, clicked);
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
                render_nodes(ui, children, theme, clicked);
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
