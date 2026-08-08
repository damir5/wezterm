use anyhow::{anyhow, bail, Context};
use luahelper::lua_value_to_dynamic;
use mlua::{Table, Value};
use std::collections::HashMap;
use std::time::Duration;
use taffy::geometry::{Point as TaffyPoint, Rect as TaffyRect};
use taffy::prelude::*;
use taffy::style::Overflow;
use wezterm_dynamic::Value as DynamicValue;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Background {
    Solid([u8; 4]),
    LinearGradient {
        start: [u8; 4],
        end: [u8; 4],
        horizontal: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiAnimation {
    Spin {
        frames: Vec<String>,
        fps: f32,
    },
    /// Opacity (and optionally height) breathing, used for the urgency capsule
    /// and the u2 row ring.
    Pulse {
        period: f32,
        min: f32,
        max: f32,
        scale_min: Option<f32>,
    },
    /// Continuous rotation, used by the running spinner. Vector only.
    Rotate {
        period: f32,
    },
}

/// Cross-axis alignment. Mirrors the subset of flexbox the sidebar needs; the
/// taffy default is Stretch, which is why every badge used to fill its row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Align {
    Stretch,
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Vector marks. Deliberately not font glyphs: the activity and harness marks
/// are shape language, and a rotating arc cannot be expressed as a codepoint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShapeKind {
    Ring,
    Arc,
    Bars,
    Cross,
    Dot,
    Triangle,
    Hexagon,
    Asterisk,
    Chevron,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiStyle {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub min_width: Option<f32>,
    pub min_height: Option<f32>,
    pub max_width: Option<f32>,
    pub max_height: Option<f32>,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Option<f32>,
    pub padding: Edges,
    pub margin: Edges,
    pub background: Option<Background>,
    pub hover_background: Option<Background>,
    pub border_width: Edges,
    pub border_color: Option<[u8; 4]>,
    pub border_radius: f32,
    pub color: Option<[u8; 4]>,
    pub font_size: Option<f32>,
    pub font_family: Option<String>,
    pub font_weight: Option<f32>,
    pub letter_spacing: f32,
    pub text_align: TextAlign,
    pub animation: Option<UiAnimation>,
    pub row: bool,
    pub gap: f32,
    pub align_items: Option<Align>,
    pub align_self: Option<Align>,
    pub absolute: bool,
    pub inset_left: Option<f32>,
    pub inset_right: Option<f32>,
    pub inset_top: Option<f32>,
    pub inset_bottom: Option<f32>,
    pub shape: Option<ShapeKind>,
    pub stroke_width: Option<f32>,
    pub track_color: Option<[u8; 4]>,
    pub fill: bool,
    pub rotation: f32,
    pub glow: bool,
    pub wrap: bool,
}

#[derive(Clone, Debug)]
pub struct UiNode {
    pub id: String,
    pub kind: String,
    pub text: Option<String>,
    pub image: Option<String>,
    pub style: UiStyle,
    pub children: Vec<UiNode>,
    pub on_click: Option<DynamicValue>,
    pub on_hover: Option<DynamicValue>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x <= self.x + self.width && y <= self.y + self.height
    }

    pub fn bottom(self) -> f32 {
        self.y + self.height
    }

    fn intersection(self, other: Self) -> Option<Self> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        (right > left && bottom > top).then_some(Self {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutNode {
    pub id: String,
    pub kind: String,
    pub text: Option<String>,
    pub image: Option<String>,
    pub style: UiStyle,
    pub rect: Rect,
    pub scrollable: bool,
    pub clip_rect: Option<Rect>,
    pub on_click: Option<DynamicValue>,
    pub on_hover: Option<DynamicValue>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiLayout {
    pub nodes: Vec<LayoutNode>,
    pub scroll_max: f32,
}

impl UiLayout {
    pub fn hit_test_scrolled(&self, x: f32, y: f32, scroll_offset: f32) -> Option<&LayoutNode> {
        self.nodes.iter().rev().find(|node| {
            let mut rect = node.rect;
            if node.scrollable {
                rect.y -= scroll_offset;
            }
            rect.contains(x, y)
                && node
                    .clip_rect
                    .map(|clip| clip.contains(x, y))
                    .unwrap_or(true)
        })
    }
}

fn number(value: Value, name: &str) -> anyhow::Result<f32> {
    match value {
        Value::Integer(value) => Ok(value as f32),
        Value::Number(value) if value.is_finite() => Ok(value as f32),
        _ => bail!("{name} must be a finite number"),
    }
}

fn optional_number(table: &Table, name: &str) -> anyhow::Result<Option<f32>> {
    match table.get::<_, Value>(name)? {
        Value::Nil => Ok(None),
        value => Ok(Some(number(value, name)?)),
    }
}

fn parse_color(value: &str, name: &str) -> anyhow::Result<[u8; 4]> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    let hex = match hex.len() {
        6 => format!("{hex}ff"),
        8 => hex.to_string(),
        _ => bail!("{name} must be #RRGGBB or #RRGGBBAA"),
    };
    let mut color = [0; 4];
    for (index, byte) in color.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .with_context(|| format!("invalid {name} color"))?;
    }
    Ok(color)
}

fn optional_color(table: &Table, name: &str) -> anyhow::Result<Option<[u8; 4]>> {
    match table.get::<_, Value>(name)? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(parse_color(value.to_str()?.as_ref(), name)?)),
        _ => bail!("{name} must be a color string"),
    }
}

fn edges(value: Value, name: &str) -> anyhow::Result<Edges> {
    match value {
        Value::Nil => Ok(Edges::default()),
        Value::Integer(_) | Value::Number(_) => {
            let value = number(value, name)?;
            Ok(Edges {
                left: value,
                right: value,
                top: value,
                bottom: value,
            })
        }
        Value::Table(value) => Ok(Edges {
            left: value
                .get::<_, Option<f32>>("left")?
                .or(value.get::<_, Option<f32>>("x")?)
                .unwrap_or(0.0),
            right: value
                .get::<_, Option<f32>>("right")?
                .or(value.get::<_, Option<f32>>("x")?)
                .unwrap_or(0.0),
            top: value
                .get::<_, Option<f32>>("top")?
                .or(value.get::<_, Option<f32>>("y")?)
                .unwrap_or(0.0),
            bottom: value
                .get::<_, Option<f32>>("bottom")?
                .or(value.get::<_, Option<f32>>("y")?)
                .unwrap_or(0.0),
        }),
        _ => bail!("{name} must be a number or edge table"),
    }
}

fn background(table: &Table, name: &str) -> anyhow::Result<Option<Background>> {
    match table.get::<_, Value>(name)? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(Background::Solid(parse_color(
            value.to_str()?.as_ref(),
            name,
        )?))),
        Value::Table(value) => {
            let kind = value
                .get::<_, Option<String>>("type")?
                .unwrap_or_else(|| "linear-gradient".into());
            if kind != "linear-gradient" {
                bail!("{name}.type must be linear-gradient")
            }
            let start = value
                .get::<_, String>("from")
                .or_else(|_| value.get::<_, String>("start"))?;
            let end = value
                .get::<_, String>("to")
                .or_else(|_| value.get::<_, String>("end"))?;
            Ok(Some(Background::LinearGradient {
                start: parse_color(&start, name)?,
                end: parse_color(&end, name)?,
                horizontal: value
                    .get::<_, Option<String>>("direction")?
                    .map(|direction| direction == "horizontal")
                    .unwrap_or(true),
            }))
        }
        _ => bail!("{name} must be a color or gradient table"),
    }
}

fn event(table: &Table, name: &str) -> anyhow::Result<Option<DynamicValue>> {
    match table.get::<_, Value>(name)? {
        Value::Nil => Ok(None),
        value => Ok(Some(lua_value_to_dynamic(value).with_context(|| {
            format!("{name} must be a string, number, boolean, or table")
        })?)),
    }
}

fn align(table: &Table, name: &str) -> anyhow::Result<Option<Align>> {
    Ok(match table.get::<_, Option<String>>(name)?.as_deref() {
        None => None,
        Some("stretch") => Some(Align::Stretch),
        Some("start") | Some("flex-start") => Some(Align::Start),
        Some("center") => Some(Align::Center),
        Some("end") | Some("flex-end") => Some(Align::End),
        Some(other) => bail!("{name} must be stretch, start, center or end, not {other:?}"),
    })
}

fn text_align(table: &Table) -> anyhow::Result<TextAlign> {
    Ok(match table.get::<_, Option<String>>("text_align")?.as_deref() {
        None | Some("left") => TextAlign::Left,
        Some("center") => TextAlign::Center,
        Some("right") => TextAlign::Right,
        Some(other) => bail!("text_align must be left, center or right, not {other:?}"),
    })
}

fn shape_kind(table: &Table) -> anyhow::Result<Option<ShapeKind>> {
    Ok(match table.get::<_, Option<String>>("shape")?.as_deref() {
        None => None,
        Some("ring") => Some(ShapeKind::Ring),
        Some("arc") => Some(ShapeKind::Arc),
        Some("bars") => Some(ShapeKind::Bars),
        Some("cross") => Some(ShapeKind::Cross),
        Some("dot") => Some(ShapeKind::Dot),
        Some("triangle") => Some(ShapeKind::Triangle),
        Some("hexagon") => Some(ShapeKind::Hexagon),
        Some("asterisk") => Some(ShapeKind::Asterisk),
        Some("chevron") => Some(ShapeKind::Chevron),
        Some(other) => bail!("unknown shape {other:?}"),
    })
}

fn font_weight(table: &Table) -> anyhow::Result<Option<f32>> {
    match table.get::<_, Value>("font_weight")? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(match value.to_str()?.as_ref() {
            "regular" | "normal" => 400.0,
            "medium" => 500.0,
            "semibold" | "bold" => 700.0,
            other => bail!("font_weight must be a number or regular/medium/bold, not {other:?}"),
        })),
        value => Ok(Some(number(value, "font_weight")?)),
    }
}

fn animation(table: &Table) -> anyhow::Result<Option<UiAnimation>> {
    let Value::Table(value) = table.get::<_, Value>("animation")? else {
        return Ok(None);
    };
    let kind = value
        .get::<_, Option<String>>("type")?
        .unwrap_or_else(|| "pulse".into());
    match kind.as_str() {
        "spin" => Ok(Some(UiAnimation::Spin {
            frames: match value.get::<_, Value>("frames")? {
                Value::Table(frames) => frames
                    .sequence_values::<String>()
                    .collect::<mlua::Result<Vec<_>>>()?,
                Value::Nil => vec![],
                _ => bail!("animation.frames must be a sequence"),
            },
            fps: value.get::<_, Option<f32>>("fps")?.unwrap_or(6.0).max(0.1),
        })),
        "pulse" => Ok(Some(UiAnimation::Pulse {
            period: value
                .get::<_, Option<f32>>("period")?
                .unwrap_or(2.4)
                .max(0.1),
            min: value
                .get::<_, Option<f32>>("min")?
                .unwrap_or(0.42)
                .clamp(0.0, 1.0),
            max: value
                .get::<_, Option<f32>>("max")?
                .unwrap_or(1.0)
                .clamp(0.0, 1.0),
            scale_min: value
                .get::<_, Option<f32>>("scale_min")?
                .map(|scale| scale.clamp(0.05, 1.0)),
        })),
        "rotate" => Ok(Some(UiAnimation::Rotate {
            period: value
                .get::<_, Option<f32>>("period")?
                .unwrap_or(1.15)
                .max(0.05),
        })),
        _ => bail!("animation.type must be spin, pulse or rotate"),
    }
}

fn style(table: &Table, kind: &str) -> anyhow::Result<UiStyle> {
    let mut style = UiStyle {
        row: kind == "row",
        ..Default::default()
    };
    style.width = optional_number(table, "width")?;
    style.height = optional_number(table, "height")?;
    style.min_width = optional_number(table, "min_width")?;
    style.min_height = optional_number(table, "min_height")?;
    style.max_width = optional_number(table, "max_width")?;
    style.max_height = optional_number(table, "max_height")?;
    style.flex_grow = optional_number(table, "flex_grow")?.unwrap_or(0.0).max(0.0);
    style.flex_shrink = optional_number(table, "flex_shrink")?
        .unwrap_or(1.0)
        .max(0.0);
    style.flex_basis = optional_number(table, "flex_basis")?;
    style.padding = edges(table.get("padding")?, "padding")?;
    style.margin = edges(table.get("margin")?, "margin")?;
    style.border_width = edges(table.get("border_width")?, "border_width")?;
    style.border_color = optional_color(table, "border_color")?;
    style.border_radius = optional_number(table, "border_radius")?
        .unwrap_or(0.0)
        .max(0.0);
    style.background = background(table, "background")?;
    style.hover_background = background(table, "hover_background")?;
    style.color = optional_color(table, "color")?;
    style.font_size = optional_number(table, "font_size")?;
    style.font_family = table.get::<_, Option<String>>("font_family")?;
    style.font_weight = font_weight(table)?;
    style.letter_spacing = optional_number(table, "letter_spacing")?.unwrap_or(0.0);
    style.text_align = text_align(table)?;
    style.animation = animation(table)?;
    style.gap = optional_number(table, "gap")?.unwrap_or(0.0).max(0.0);
    style.align_items = align(table, "align_items")?;
    style.align_self = align(table, "align_self")?;
    style.absolute = table.get::<_, Option<String>>("position")?.as_deref() == Some("absolute");
    style.inset_left = optional_number(table, "left")?;
    style.inset_right = optional_number(table, "right")?;
    style.inset_top = optional_number(table, "top")?;
    style.inset_bottom = optional_number(table, "bottom")?;
    style.shape = shape_kind(table)?;
    style.stroke_width = optional_number(table, "stroke_width")?;
    style.track_color = optional_color(table, "track_color")?;
    style.fill = table.get::<_, Option<bool>>("fill")?.unwrap_or(false);
    style.rotation = optional_number(table, "rotation")?.unwrap_or(0.0);
    style.glow = table.get::<_, Option<bool>>("glow")?.unwrap_or(false);
    style.wrap = table.get::<_, Option<String>>("flex_wrap")?.as_deref() == Some("wrap");
    style.row = table
        .get::<_, Option<String>>("flex_direction")?
        .map(|direction| direction == "row")
        .unwrap_or(style.row);
    Ok(style)
}

fn decode_node(lua: &mlua::Lua, table: Table, path: &str) -> anyhow::Result<UiNode> {
    let kind = table
        .get::<_, Option<String>>("type")?
        .unwrap_or_else(|| "box".into());
    match kind.as_str() {
        "box" | "row" | "column" | "scroll" | "text" | "image" | "icon" | "shape" | "summary"
        | "group" | "subgroup" => {}
        _ => bail!("unknown sidebar UI node type {kind:?}"),
    }
    let id = table
        .get::<_, Option<String>>("id")?
        .unwrap_or_else(|| path.to_string());
    let children = match table.get::<_, Value>("children")? {
        Value::Nil => Vec::new(),
        Value::Table(children) => children
            .sequence_values::<Value>()
            .enumerate()
            .map(|(index, value)| {
                let value = value?;
                let Value::Table(value) = value else {
                    bail!("sidebar UI child {path}.{index} must be a table")
                };
                decode_node(lua, value, &format!("{path}.{index}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        _ => bail!("sidebar UI node {path} children must be a sequence"),
    };
    Ok(UiNode {
        id,
        kind: kind.clone(),
        text: table.get::<_, Option<String>>("text")?,
        image: table
            .get::<_, Option<String>>("src")?
            .or(table.get::<_, Option<String>>("path")?)
            .or(table.get::<_, Option<String>>("name")?),
        style: style(&table, &kind)?,
        children,
        on_click: event(&table, "on_click")?,
        on_hover: event(&table, "on_hover")?,
    })
}

pub fn decode(value: Value, lua: &mlua::Lua) -> anyhow::Result<Option<UiNode>> {
    let Value::Table(table) = value else {
        if matches!(value, Value::Nil) {
            return Ok(None);
        }
        bail!("render-sidebar must return a UI node table")
    };
    Ok(Some(decode_node(lua, table, "root")?))
}

#[derive(Clone, Copy)]
struct Measure {
    width: f32,
    height: f32,
}

fn dimension(value: Option<f32>) -> Dimension {
    value.map(Dimension::from_length).unwrap_or(Dimension::AUTO)
}

fn taffy_align(align: Option<Align>) -> Option<AlignItems> {
    align.map(|align| match align {
        Align::Stretch => AlignItems::Stretch,
        Align::Start => AlignItems::FlexStart,
        Align::Center => AlignItems::Center,
        Align::End => AlignItems::FlexEnd,
    })
}

fn taffy_style(node: &UiNode) -> Style {
    let style = &node.style;
    Style {
        display: Display::Flex,
        position: if style.absolute {
            Position::Absolute
        } else {
            Position::Relative
        },
        inset: TaffyRect {
            left: style
                .inset_left
                .map(LengthPercentageAuto::from_length)
                .unwrap_or(LengthPercentageAuto::AUTO),
            right: style
                .inset_right
                .map(LengthPercentageAuto::from_length)
                .unwrap_or(LengthPercentageAuto::AUTO),
            top: style
                .inset_top
                .map(LengthPercentageAuto::from_length)
                .unwrap_or(LengthPercentageAuto::AUTO),
            bottom: style
                .inset_bottom
                .map(LengthPercentageAuto::from_length)
                .unwrap_or(LengthPercentageAuto::AUTO),
        },
        gap: Size {
            width: LengthPercentage::from_length(if style.row { style.gap } else { 0.0 }),
            // A wrapping row needs the same gap between its lines.
            height: LengthPercentage::from_length(if style.row && !style.wrap {
                0.0
            } else {
                style.gap
            }),
        },
        flex_wrap: if style.wrap {
            FlexWrap::Wrap
        } else {
            FlexWrap::NoWrap
        },
        align_items: taffy_align(style.align_items),
        align_self: taffy_align(style.align_self),
        flex_direction: if style.row {
            FlexDirection::Row
        } else {
            FlexDirection::Column
        },
        overflow: if node.kind == "scroll" {
            TaffyPoint {
                x: Overflow::Hidden,
                y: Overflow::Scroll,
            }
        } else {
            TaffyPoint::default()
        },
        flex_grow: style.flex_grow,
        flex_shrink: style.flex_shrink,
        flex_basis: dimension(style.flex_basis),
        size: Size {
            width: dimension(style.width),
            height: dimension(style.height),
        },
        min_size: Size {
            width: dimension(style.min_width),
            height: dimension(style.min_height),
        },
        max_size: Size {
            width: dimension(style.max_width),
            height: dimension(style.max_height),
        },
        padding: TaffyRect {
            left: LengthPercentage::from_length(style.padding.left),
            right: LengthPercentage::from_length(style.padding.right),
            top: LengthPercentage::from_length(style.padding.top),
            bottom: LengthPercentage::from_length(style.padding.bottom),
        },
        margin: TaffyRect {
            left: LengthPercentageAuto::from_length(style.margin.left),
            right: LengthPercentageAuto::from_length(style.margin.right),
            top: LengthPercentageAuto::from_length(style.margin.top),
            bottom: LengthPercentageAuto::from_length(style.margin.bottom),
        },
        ..Default::default()
    }
}

/// Lay a node's text out with the real font, so widths, right alignment and
/// elision agree with what gets painted.
fn galley(
    ctx: &egui::Context,
    style: &UiStyle,
    text: &str,
    max_width: f32,
    color: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_string(),
        egui::TextFormat {
            font_id: font_id(style, style.font_size.unwrap_or(13.0)),
            extra_letter_spacing: style.letter_spacing,
            color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ctx.fonts(|fonts| fonts.layout_job(job))
}

fn measure(ctx: &egui::Context, node: &UiNode) -> Measure {
    let size = node.style.font_size.unwrap_or(13.0);
    let text = node.text.as_deref().filter(|text| !text.is_empty());
    let measured = match node.kind.as_str() {
        "image" | "icon" | "shape" => Measure {
            width: node.style.width.unwrap_or(size),
            height: node.style.height.unwrap_or(size),
        },
        "text" => {
            let galley = text
                .map(|text| galley(ctx, &node.style, text, f32::INFINITY, egui::Color32::WHITE));
            Measure {
                width: galley.as_ref().map(|galley| galley.size().x).unwrap_or(0.0)
                    + node.style.padding.left
                    + node.style.padding.right,
                height: node.style.height.unwrap_or(size * 1.35),
            }
        }
        _ => Measure {
            width: 0.0,
            height: node.style.height.unwrap_or(size * 1.35),
        },
    };
    measured
}

fn add_to_tree(
    ctx: &egui::Context,
    node: &UiNode,
    taffy: &mut TaffyTree<Measure>,
    nodes: &mut HashMap<NodeId, UiNode>,
) -> anyhow::Result<NodeId> {
    let children = node
        .children
        .iter()
        .map(|child| add_to_tree(ctx, child, taffy, nodes))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let id = if children.is_empty() {
        taffy.new_leaf_with_context(taffy_style(node), measure(ctx, node))?
    } else {
        taffy.new_with_children(taffy_style(node), &children)?
    };
    nodes.insert(id, node.clone());
    Ok(id)
}

fn walk(
    taffy: &TaffyTree<Measure>,
    node_id: NodeId,
    nodes: &HashMap<NodeId, UiNode>,
    origin: (f32, f32),
    scrollable: bool,
    clip_rect: Option<Rect>,
    output: &mut Vec<LayoutNode>,
) -> anyhow::Result<()> {
    let node = nodes
        .get(&node_id)
        .ok_or_else(|| anyhow!("missing sidebar UI node context"))?;
    let layout = taffy.layout(node_id)?;
    let x = origin.0 + layout.location.x;
    let y = origin.1 + layout.location.y;
    output.push(LayoutNode {
        id: node.id.clone(),
        kind: node.kind.clone(),
        text: node.text.clone(),
        image: node.image.clone(),
        style: node.style.clone(),
        rect: Rect {
            x,
            y,
            width: layout.size.width,
            height: layout.size.height,
        },
        scrollable,
        clip_rect,
        on_click: node.on_click.clone(),
        on_hover: node.on_hover.clone(),
    });
    let next_scrollable = scrollable || node.kind == "scroll";
    let next_clip_rect = if node.kind == "scroll" {
        Some(Rect {
            x,
            y,
            width: layout.size.width,
            height: layout.size.height,
        })
    } else {
        clip_rect
    };
    for child in taffy.children(node_id)? {
        walk(
            taffy,
            child,
            nodes,
            (x, y),
            next_scrollable,
            next_clip_rect,
            output,
        )?;
    }
    Ok(())
}

pub fn layout(
    ctx: &egui::Context,
    root: &UiNode,
    width: f32,
    height: f32,
) -> anyhow::Result<UiLayout> {
    let mut taffy = TaffyTree::<Measure>::new();
    let mut nodes = HashMap::new();
    let root_id = add_to_tree(ctx, root, &mut taffy, &mut nodes)?;
    // The sidebar is a fixed viewport, not a shrink-to-fit box. Without this the
    // root sizes to max-content and every row is laid out wider than the panel.
    let mut root_style = taffy.style(root_id)?.clone();
    root_style.size = Size {
        width: Dimension::length(width),
        height: Dimension::length(height),
    };
    taffy.set_style(root_id, root_style)?;
    taffy.compute_layout_with_measure(
        root_id,
        Size {
            width: AvailableSpace::Definite(width),
            height: AvailableSpace::Definite(height),
        },
        |known, available, _node_id, context, _style| {
            let measured = context.as_deref().copied().unwrap_or(Measure {
                width: 0.0,
                height: 0.0,
            });
            // Text ellipsizes rather than forcing its row wider, so its
            // min-content width is zero: the CSS the mockup relies on is
            // `min-width:0` plus `text-overflow:ellipsis`. Anything that needs
            // a floor asks for it with min_width.
            let width = known.width.unwrap_or(match available.width {
                AvailableSpace::Definite(space) => measured.width.min(space),
                AvailableSpace::MinContent => 0.0,
                AvailableSpace::MaxContent => measured.width,
            });
            Size {
                width,
                height: known.height.unwrap_or(measured.height),
            }
        },
    )?;
    let mut output = Vec::new();
    walk(
        &taffy,
        root_id,
        &nodes,
        (0.0, 0.0),
        false,
        None,
        &mut output,
    )?;
    let scroll_max = output
        .iter()
        .filter_map(|node| {
            node.clip_rect
                .map(|clip| node.rect.bottom() - clip.bottom())
        })
        .fold(0.0, f32::max);
    Ok(UiLayout {
        nodes: output,
        scroll_max: scroll_max.max(0.0),
    })
}

pub fn interpolate(from: Option<&UiLayout>, to: &UiLayout, progress: f32) -> UiLayout {
    let progress = progress.clamp(0.0, 1.0);
    let mut layout = to.clone();
    let Some(from) = from else {
        return layout;
    };
    for node in &mut layout.nodes {
        let Some(previous) = from.nodes.iter().find(|item| item.id == node.id) else {
            continue;
        };
        node.rect.x = previous.rect.x + (node.rect.x - previous.rect.x) * progress;
        node.rect.y = previous.rect.y + (node.rect.y - previous.rect.y) * progress;
        node.rect.width = previous.rect.width + (node.rect.width - previous.rect.width) * progress;
        node.rect.height =
            previous.rect.height + (node.rect.height - previous.rect.height) * progress;
    }
    layout
}

pub fn animation_frame_delay(layout: &UiLayout) -> Option<Duration> {
    layout
        .nodes
        .iter()
        .filter_map(|node| {
            node.style
                .animation
                .as_ref()
                .map(|animation| match animation {
                    UiAnimation::Pulse { .. } | UiAnimation::Rotate { .. } => {
                        Duration::from_millis(16)
                    }
                    UiAnimation::Spin { fps, frames } if !frames.is_empty() => {
                        Duration::from_secs_f32(1.0 / *fps)
                    }
                    UiAnimation::Spin { .. } => Duration::ZERO,
                })
        })
        .filter(|delay| !delay.is_zero())
        .min()
}

pub fn ui_items_for_layout(
    layout: &UiLayout,
    scroll_offset: f32,
    pixels_per_point: f32,
    width: usize,
    height: usize,
) -> Vec<super::UIItem> {
    layout
        .nodes
        .iter()
        .filter(|node| node.on_click.is_some() || node.on_hover.is_some())
        .filter_map(|node| {
            let mut node_rect = node.rect;
            if node.scrollable {
                node_rect.y -= scroll_offset;
            }
            let rect = node
                .clip_rect
                .and_then(|clip| node_rect.intersection(clip))
                .unwrap_or(node_rect);
            let x = (rect.x * pixels_per_point).max(0.0) as usize;
            let y = (rect.y * pixels_per_point).max(0.0) as usize;
            let node_width = (rect.width * pixels_per_point).max(0.0) as usize;
            let node_height = (rect.height * pixels_per_point).max(0.0) as usize;
            let node_width = node_width.min(width.saturating_sub(x));
            let node_height = node_height.min(height.saturating_sub(y));
            (node_width > 0 && node_height > 0).then(|| super::UIItem {
                x,
                y,
                width: node_width,
                height: node_height,
                item_type: super::UIItemType::SidebarNode(node.id.clone()),
            })
        })
        .collect()
}

/// Draw one vector mark centred in `rect`. `turn` is a full-turn fraction, so a
/// rotating spinner is just `time / period`.
fn paint_shape(
    painter: &egui::Painter,
    kind: ShapeKind,
    rect: egui::Rect,
    color: egui::Color32,
    track: Option<egui::Color32>,
    stroke_width: f32,
    turn: f32,
    fill: bool,
) {
    let size = rect.width().min(rect.height());
    if size <= 0.0 {
        return;
    }
    let center = rect.center();
    let width = if stroke_width > 0.0 {
        stroke_width
    } else {
        (size * 0.14).max(1.0)
    };
    let stroke = egui::Stroke::new(width, color);
    let radius = size * 0.5 - width * 0.5;
    let angle = turn * std::f32::consts::TAU;
    let point = |distance: f32, at: f32| {
        let at = at + angle;
        center + egui::vec2(at.cos() * distance, at.sin() * distance)
    };
    let polygon = |sides: usize, start: f32| {
        (0..sides)
            .map(|index| {
                point(
                    radius,
                    start + index as f32 / sides as f32 * std::f32::consts::TAU,
                )
            })
            .collect::<Vec<_>>()
    };
    match kind {
        ShapeKind::Ring => {
            painter.circle_stroke(center, radius, stroke);
        }
        ShapeKind::Arc => {
            if let Some(track) = track {
                painter.circle_stroke(center, radius, egui::Stroke::new(width, track));
            }
            // A quarter turn of bright arc over the dim track reads as
            // indeterminate progress at 12px in a way a full ring cannot.
            let steps = 12;
            let points = (0..=steps)
                .map(|index| {
                    point(
                        radius,
                        index as f32 / steps as f32 * std::f32::consts::FRAC_PI_2
                            - std::f32::consts::FRAC_PI_2,
                    )
                })
                .collect::<Vec<_>>();
            painter.add(egui::Shape::line(
                points,
                egui::Stroke::new(width, color),
            ));
        }
        ShapeKind::Bars => {
            let bar = egui::vec2(size * 0.2, size * 0.64);
            let offset = size * 0.18;
            for side in [-offset, offset] {
                painter.rect_filled(
                    egui::Rect::from_center_size(center + egui::vec2(side, 0.0), bar),
                    bar.x * 0.5,
                    color,
                );
            }
        }
        ShapeKind::Cross => {
            let arm = size * 0.31;
            for base in [std::f32::consts::FRAC_PI_4, -std::f32::consts::FRAC_PI_4] {
                painter.add(egui::Shape::line_segment(
                    [point(arm, base), point(arm, base + std::f32::consts::PI)],
                    stroke,
                ));
            }
        }
        ShapeKind::Dot => {
            let radius = size * 0.26;
            if fill {
                painter.circle_filled(center, radius, color);
            } else {
                painter.circle_stroke(center, radius, stroke);
            }
        }
        ShapeKind::Triangle | ShapeKind::Hexagon => {
            let sides = if kind == ShapeKind::Triangle { 3 } else { 6 };
            let points = polygon(sides, -std::f32::consts::FRAC_PI_2);
            painter.add(if fill {
                egui::Shape::convex_polygon(points, color, egui::Stroke::NONE)
            } else {
                egui::Shape::closed_line(points, stroke)
            });
        }
        ShapeKind::Asterisk => {
            let arm = size * 0.42;
            for index in 0..3 {
                let base = index as f32 / 3.0 * std::f32::consts::PI;
                painter.add(egui::Shape::line_segment(
                    [point(arm, base), point(arm, base + std::f32::consts::PI)],
                    stroke,
                ));
            }
        }
        ShapeKind::Chevron => {
            let arm = size * 0.34;
            let tip = point(arm, 0.0);
            painter.add(egui::Shape::line(
                vec![
                    point(arm, -std::f32::consts::FRAC_PI_2 - 0.6),
                    tip,
                    point(arm, std::f32::consts::FRAC_PI_2 + 0.6),
                ],
                stroke,
            ));
        }
    }
}

pub fn paint(
    painter: &egui::Painter,
    ctx: &egui::Context,
    layout: &UiLayout,
    hovered: Option<&str>,
    images: &mut HashMap<String, egui::TextureHandle>,
    scroll_offset: f32,
    time: f64,
) {
    for node in &layout.nodes {
        let mut node_rect = node.rect;
        if node.scrollable {
            node_rect.y -= scroll_offset;
        }
        let visible = match node.clip_rect {
            Some(clip) => node_rect.intersection(clip),
            None => Some(node_rect),
        };
        let Some(visible) = visible else {
            continue;
        };
        let mut rect = egui::Rect::from_min_size(
            egui::pos2(visible.x, visible.y),
            egui::vec2(visible.width, visible.height),
        );
        let clip_to = node
            .clip_rect
            .map(|clip| {
                egui::Rect::from_min_size(
                    egui::pos2(clip.x, clip.y),
                    egui::vec2(clip.width, clip.height),
                )
            })
            .unwrap_or(egui::Rect::EVERYTHING);
        let background = if hovered == Some(node.id.as_str()) {
            node.style.hover_background.or(node.style.background)
        } else {
            node.style.background
        };
        let mut phase = 0.0_f32;
        let pulse = node
            .style
            .animation
            .as_ref()
            .and_then(|animation| match animation {
                UiAnimation::Pulse {
                    period,
                    min,
                    max,
                    scale_min,
                } => {
                    let progress = (time as f32 % period) / period;
                    let eased = (progress * std::f32::consts::TAU).sin() * 0.5 + 0.5;
                    phase = eased;
                    if let Some(scale_min) = scale_min {
                        // The urgency capsule breathes in height as well as
                        // opacity, matching the mockup's scaleY.
                        let scale = scale_min + (1.0 - scale_min) * eased;
                        let height = rect.height() * scale;
                        rect = egui::Rect::from_center_size(
                            rect.center(),
                            egui::vec2(rect.width(), height),
                        );
                    }
                    Some(min + (max - min) * eased)
                }
                UiAnimation::Spin { .. } | UiAnimation::Rotate { .. } => None,
            });
        if let Some(background) = background {
            match background {
                Background::Solid(color) => {
                    let color = pulse
                        .map(|amount| color32(color).gamma_multiply(amount))
                        .unwrap_or_else(|| color32(color));
                    painter.rect_filled(rect, node.style.border_radius, color);
                }
                Background::LinearGradient {
                    start,
                    end,
                    horizontal,
                } => {
                    let mut mesh = egui::Mesh::default();
                    let (first, second) = if horizontal {
                        (color32(start), color32(end))
                    } else {
                        (color32(start), color32(end))
                    };
                    mesh.vertices = if horizontal {
                        vec![
                            egui::epaint::Vertex {
                                pos: rect.left_top(),
                                uv: egui::epaint::WHITE_UV,
                                color: first,
                            },
                            egui::epaint::Vertex {
                                pos: rect.right_top(),
                                uv: egui::epaint::WHITE_UV,
                                color: second,
                            },
                            egui::epaint::Vertex {
                                pos: rect.right_bottom(),
                                uv: egui::epaint::WHITE_UV,
                                color: second,
                            },
                            egui::epaint::Vertex {
                                pos: rect.left_bottom(),
                                uv: egui::epaint::WHITE_UV,
                                color: first,
                            },
                        ]
                    } else {
                        vec![
                            egui::epaint::Vertex {
                                pos: rect.left_top(),
                                uv: egui::epaint::WHITE_UV,
                                color: first,
                            },
                            egui::epaint::Vertex {
                                pos: rect.right_top(),
                                uv: egui::epaint::WHITE_UV,
                                color: first,
                            },
                            egui::epaint::Vertex {
                                pos: rect.right_bottom(),
                                uv: egui::epaint::WHITE_UV,
                                color: second,
                            },
                            egui::epaint::Vertex {
                                pos: rect.left_bottom(),
                                uv: egui::epaint::WHITE_UV,
                                color: second,
                            },
                        ]
                    };
                    mesh.indices = vec![0, 1, 2, 0, 2, 3];
                    painter.add(egui::Shape::mesh(mesh));
                }
            }
        }
        if let Some(color) = node.style.border_color {
            let color = pulse
                .map(|amount| color32(color).gamma_multiply(amount))
                .unwrap_or_else(|| color32(color));
            let stroke = egui::Stroke::new(node.style.border_width.left.max(1.0), color);
            painter.rect_stroke(
                rect,
                node.style.border_radius,
                stroke,
                egui::StrokeKind::Inside,
            );
            if node.style.glow {
                // Three fading strokes stand in for a blurred outer shadow; a
                // real blur would cost a render target for two pixels of light.
                for step in 1..=3 {
                    let grow = step as f32 * 1.6;
                    painter.rect_stroke(
                        rect.expand(grow),
                        node.style.border_radius + grow,
                        egui::Stroke::new(
                            1.0_f32,
                            color.gamma_multiply(0.30 * phase / step as f32),
                        ),
                        egui::StrokeKind::Outside,
                    );
                }
            }
        }

        if node.kind == "shape" {
            if let Some(shape) = node.style.shape {
                let turn = match &node.style.animation {
                    Some(UiAnimation::Rotate { period }) => (time as f32 % period) / period,
                    _ => 0.0,
                } + node.style.rotation / 360.0;
                let color = color32(node.style.color.unwrap_or([235, 235, 240, 255]))
                    .gamma_multiply(pulse.unwrap_or(1.0));
                paint_shape(
                    &painter.with_clip_rect(clip_to),
                    shape,
                    rect,
                    color,
                    node.style.track_color.map(color32),
                    node.style.stroke_width.unwrap_or(0.0),
                    turn,
                    node.style.fill,
                );
            }
            continue;
        }

        if node.kind == "image" {
            let Some(image) = &node.image else { continue };
            let texture = if let Some(texture) = images.get(image) {
                Some(texture.clone())
            } else {
                let key = image.clone();
                match ::image::open(&key) {
                    Ok(decoded) => {
                        let image = decoded.to_rgba8();
                        let size = [image.width() as usize, image.height() as usize];
                        let texture = ctx.load_texture(
                            key.clone(),
                            egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
                            egui::TextureOptions::LINEAR,
                        );
                        images.insert(key, texture.clone());
                        Some(texture)
                    }
                    Err(err) => {
                        log::warn!("sidebar image {image:?}: {err}");
                        None
                    }
                }
            };
            if let Some(texture) = texture {
                painter.image(
                    texture.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        }
        let text = node
            .text
            .as_deref()
            .or_else(|| (node.kind == "icon").then_some(node.image.as_deref().unwrap_or("")));
        let text = match (&node.style.animation, text) {
            (Some(UiAnimation::Spin { frames, fps }), Some(_)) if !frames.is_empty() => {
                Some(frames[((time as f32 * fps).floor() as usize) % frames.len()].as_str())
            }
            (_, text) => text,
        };
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            let color = color32(node.style.color.unwrap_or([235, 235, 240, 255]))
                .gamma_multiply(pulse.unwrap_or(1.0));
            let inner = egui::Rect::from_min_max(
                rect.left_top() + egui::vec2(node.style.padding.left, 0.0),
                rect.right_bottom() - egui::vec2(node.style.padding.right, 0.0),
            );
            let galley = galley(ctx, &node.style, text, inner.width().max(0.0), color);
            let x = match node.style.text_align {
                TextAlign::Left => inner.left(),
                TextAlign::Center => inner.center().x - galley.size().x * 0.5,
                TextAlign::Right => inner.right() - galley.size().x,
            };
            painter.with_clip_rect(clip_to.intersect(rect)).galley(
                egui::pos2(x, inner.center().y - galley.size().y * 0.5),
                galley,
                color,
            );
        }
    }
}

fn color32(color: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3])
}

/// Map family plus numeric weight onto the families registered in
/// `register_egui_fonts`. Weight is a separate face, not a synthetic bold.
fn font_id(style: &UiStyle, size: f32) -> egui::FontId {
    let weight = style.font_weight.unwrap_or(400.0);
    let family = match style.font_family.as_deref() {
        Some("monospace") => {
            if weight >= 600.0 {
                egui::FontFamily::Name("mono-bold".into())
            } else {
                egui::FontFamily::Monospace
            }
        }
        Some("proportional") | None => {
            if weight >= 600.0 {
                egui::FontFamily::Name("bold".into())
            } else if weight >= 500.0 {
                egui::FontFamily::Name("medium".into())
            } else {
                egui::FontFamily::Proportional
            }
        }
        Some(name) => egui::FontFamily::Name(name.to_owned().into()),
    };
    egui::FontId::new(size, family)
}

/// Register the sidebar's font families. Proportional is a UI sans in three
/// weights; monospace is only for hosts, worktrees and progress counters.
pub fn register_fonts(ctx: &egui::Context) {
    use std::sync::Arc;

    let mut fonts = egui::FontDefinitions::default();
    let mut add = |name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(
            name.to_string(),
            Arc::new(egui::FontData::from_static(bytes)),
        );
    };
    add(
        "Roboto",
        include_bytes!("../../../assets/fonts/Roboto-Regular.ttf"),
    );
    add(
        "Roboto-Medium",
        include_bytes!("../../../assets/fonts/Roboto-Medium.ttf"),
    );
    add(
        "Roboto-Bold",
        include_bytes!("../../../assets/fonts/Roboto-Bold.ttf"),
    );
    add(
        "JetBrainsMono",
        include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"),
    );
    add(
        "JetBrainsMono-Bold",
        include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf"),
    );

    for (family, faces) in [
        (egui::FontFamily::Proportional, vec!["Roboto"]),
        (egui::FontFamily::Name("medium".into()), vec!["Roboto-Medium"]),
        (egui::FontFamily::Name("bold".into()), vec!["Roboto-Bold"]),
        (egui::FontFamily::Monospace, vec!["JetBrainsMono"]),
        (
            egui::FontFamily::Name("mono-bold".into()),
            vec!["JetBrainsMono-Bold"],
        ),
    ] {
        let entry = fonts.families.entry(family).or_insert_with(Vec::new);
        for (index, face) in faces.into_iter().enumerate() {
            entry.insert(index, face.to_string());
        }
    }

    ctx.set_fonts(fonts);
}

/// The layout pass needs the same fonts the paint pass uses, so both share one
/// context created on first use.
pub fn context(slot: &mut Option<egui::Context>) -> egui::Context {
    if slot.is_none() {
        let ctx = egui::Context::default();
        register_fonts(&ctx);
        // Fonts are only realised by a pass, and the layout pass runs before
        // the first paint pass, so prime it here.
        let _ = ctx.run(Default::default(), |_| {});
        *slot = Some(ctx);
    }
    slot.as_ref().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> egui::Context {
        context(&mut None)
    }

    #[test]
    fn flex_layout_respects_padding_and_direction() {
        let lua = mlua::Lua::new();
        let root = lua
            .load(
                r#"
return { type = 'row', width = 100, height = 40, padding = 10,
  children = {
    { type = 'text', text = 'a', width = 20, height = 20 },
    { type = 'text', text = 'b', width = 30, height = 20 },
  }
}
"#,
            )
            .eval::<Value>()
            .unwrap();
        let root = decode(root, &lua).unwrap().unwrap();
        let layout = layout(&test_context(), &root, 100.0, 40.0).unwrap();
        assert_eq!(layout.nodes[1].rect.x, 10.0);
        assert_eq!(layout.nodes[2].rect.x, 30.0);
        assert_eq!(layout.nodes[1].rect.y, 10.0);
    }

    #[test]
    fn hit_test_prefers_deepest_node() {
        let root = UiNode {
            id: "root".into(),
            kind: "column".into(),
            text: None,
            image: None,
            style: UiStyle {
                width: Some(20.0),
                height: Some(20.0),
                ..Default::default()
            },
            children: vec![UiNode {
                id: "child".into(),
                kind: "text".into(),
                text: Some("x".into()),
                image: None,
                style: UiStyle {
                    width: Some(10.0),
                    height: Some(10.0),
                    ..Default::default()
                },
                children: vec![],
                on_click: None,
                on_hover: None,
            }],
            on_click: None,
            on_hover: None,
        };
        let layout = layout(&test_context(), &root, 20.0, 20.0).unwrap();
        assert_eq!(layout.hit_test_scrolled(1.0, 1.0, 0.0).unwrap().id, "child");
    }

    #[test]
    fn scroll_layout_preserves_content_and_animation_metadata() {
        let lua = mlua::Lua::new();
        let root = lua
            .load(
                r#"
return { type = 'scroll', width = 100, height = 40, children = {
  { type = 'text', text = 'a', height = 30,
    animation = { type = 'spin', frames = {'◐', '◓'}, fps = 6 } },
  { type = 'text', text = 'b', height = 30 },
} }
"#,
            )
            .eval::<Value>()
            .unwrap();
        let root = decode(root, &lua).unwrap().unwrap();
        let layout = layout(&test_context(), &root, 100.0, 40.0).unwrap();
        assert!(layout.scroll_max > 0.0);
        let delay = animation_frame_delay(&layout).unwrap();
        assert!(delay > Duration::from_millis(100));
        assert!(delay < Duration::from_millis(200));
        assert!(layout.hit_test_scrolled(10.0, 45.0, 0.0).is_none());
    }
}
