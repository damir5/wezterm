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
    Spin { frames: Vec<String>, fps: f32 },
    Pulse { period: f32, min: f32, max: f32 },
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
    pub animation: Option<UiAnimation>,
    pub row: bool,
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
        })),
        _ => bail!("animation.type must be spin or pulse"),
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
    style.animation = animation(table)?;
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
        "box" | "row" | "column" | "scroll" | "text" | "image" | "icon" | "summary" | "group"
        | "subgroup" => {}
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

fn taffy_style(node: &UiNode) -> Style {
    let style = &node.style;
    Style {
        display: Display::Flex,
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

fn measure(node: &UiNode) -> Measure {
    let size = node.style.font_size.unwrap_or(13.0);
    let width = match node.kind.as_str() {
        "image" | "icon" => node.style.width.unwrap_or(size * 1.25),
        "text" => node
            .text
            .as_deref()
            .map(|text| text.chars().count() as f32 * size * 0.6)
            .unwrap_or(0.0),
        _ => 0.0,
    };
    Measure {
        width,
        height: node.style.height.unwrap_or(size * 1.35),
    }
}

fn add_to_tree(
    node: &UiNode,
    taffy: &mut TaffyTree<Measure>,
    nodes: &mut HashMap<NodeId, UiNode>,
) -> anyhow::Result<NodeId> {
    let children = node
        .children
        .iter()
        .map(|child| add_to_tree(child, taffy, nodes))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let id = if children.is_empty() {
        taffy.new_leaf_with_context(taffy_style(node), measure(node))?
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

pub fn layout(root: &UiNode, width: f32, height: f32) -> anyhow::Result<UiLayout> {
    let mut taffy = TaffyTree::<Measure>::new();
    let mut nodes = HashMap::new();
    let root_id = add_to_tree(root, &mut taffy, &mut nodes)?;
    taffy.compute_layout_with_measure(
        root_id,
        Size {
            width: AvailableSpace::Definite(width),
            height: AvailableSpace::Definite(height),
        },
        |known, _available, _node_id, context, _style| {
            let measured = context.as_deref().copied().unwrap_or(Measure {
                width: 0.0,
                height: 0.0,
            });
            Size {
                width: known.width.unwrap_or(measured.width),
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
                    UiAnimation::Pulse { .. } => Duration::from_millis(16),
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
        let rect = egui::Rect::from_min_size(
            egui::pos2(visible.x, visible.y),
            egui::vec2(visible.width, visible.height),
        );
        let background = if hovered == Some(node.id.as_str()) {
            node.style.hover_background.or(node.style.background)
        } else {
            node.style.background
        };
        let pulse = node
            .style
            .animation
            .as_ref()
            .and_then(|animation| match animation {
                UiAnimation::Pulse { period, min, max } => {
                    let phase = (time as f32 % period) / period;
                    let eased = (phase * std::f32::consts::TAU).sin() * 0.5 + 0.5;
                    Some(min + (max - min) * eased)
                }
                UiAnimation::Spin { .. } => None,
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
            let stroke = egui::Stroke::new(node.style.border_width.left.max(1.0), color32(color));
            painter.rect_stroke(
                rect,
                node.style.border_radius,
                stroke,
                egui::StrokeKind::Inside,
            );
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
            let font_size = node.style.font_size.unwrap_or(13.0);
            let color = color32(node.style.color.unwrap_or([235, 235, 240, 255]))
                .gamma_multiply(pulse.unwrap_or(1.0));
            painter.text(
                rect.left_center() + egui::vec2(node.style.padding.left, 0.0),
                egui::Align2::LEFT_CENTER,
                text,
                font_id(&node.style, font_size),
                color,
            );
        }
    }
}

fn color32(color: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3])
}

fn font_id(style: &UiStyle, size: f32) -> egui::FontId {
    let family = match style.font_family.as_deref() {
        Some("monospace") => egui::FontFamily::Monospace,
        Some("proportional") | None => egui::FontFamily::Proportional,
        Some(name) => egui::FontFamily::Name(name.to_owned().into()),
    };
    egui::FontId::new(size, family)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let layout = layout(&root, 100.0, 40.0).unwrap();
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
        let layout = layout(&root, 20.0, 20.0).unwrap();
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
        let layout = layout(&root, 100.0, 40.0).unwrap();
        assert!(layout.scroll_max > 0.0);
        let delay = animation_frame_delay(&layout).unwrap();
        assert!(delay > Duration::from_millis(100));
        assert!(delay < Duration::from_millis(200));
        assert!(layout.hit_test_scrolled(10.0, 45.0, 0.0).is_none());
    }
}
