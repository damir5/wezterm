//! Lua bindings for the `GuiPane` dashboard surface.
//!
//! A `LuaUi` is handed to a `wezterm.on("render-gui-pane", function(pane, ui))`
//! handler. The handler describes the dashboard with widget + container calls;
//! `LuaUi` accumulates them into an owned `UiNode` tree that the egui render
//! pass replays each frame.
//!
//! Containers (`card`, `collapsing_header`, `horizontal`, ...) take a Lua
//! callback; nested widget calls land in that container's children via a
//! shared child-list stack. This gives true nested immediate-mode layout
//! without ever handing a live `&mut egui::Ui` across the mlua boundary.

use super::*;
use mux::guipane::{Color, GuiPane, UiNode, UiStyle, UiTheme};
use mux::pane::Pane;
use mux::tab::{SplitDirection, SplitRequest, SplitSize};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Builder userdata that accumulates a `UiNode` tree from Lua calls.
#[derive(Clone)]
pub struct LuaUi {
    /// Stack of child lists; `stack[0]` is the pane root. The top of the
    /// stack is the container currently being filled.
    stack: Arc<Mutex<Vec<Vec<UiNode>>>>,
    /// Pending style overrides applied to the next pushed widget, then
    /// cleared. Mirrors egui's "current style" semantics.
    pending: Arc<Mutex<UiStyle>>,
    /// Theme set by the handler this fire, if any.
    theme: Arc<Mutex<Option<UiTheme>>>,
    /// Widget ids reported as activated by the last egui render pass;
    /// `ui:clicked(id)` lets the handler react with one-frame latency.
    events: Arc<Mutex<HashSet<String>>>,
    /// Latest widget values (sliders) from the last render pass, read back by
    /// `ui:value(id)`. Seeded from the GuiPane at the start of each fire.
    values: Arc<Mutex<HashMap<String, f64>>>,
}

impl LuaUi {
    pub fn new() -> Self {
        Self {
            stack: Arc::new(Mutex::new(vec![Vec::new()])),
            pending: Arc::new(Mutex::new(UiStyle::default())),
            theme: Arc::new(Mutex::new(None)),
            events: Arc::new(Mutex::new(HashSet::new())),
            values: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Pre-load the click/activation set collected by the last render pass,
    /// so `ui:clicked(id)` reflects the most recent interaction.
    pub fn set_events(&self, events: HashSet<String>) {
        *self.events.lock() = events;
    }

    /// Pre-load the latest widget values (sliders) so `ui:value(id)` reflects
    /// the most recent render pass during this fire.
    pub fn set_values(&self, values: HashMap<String, f64>) {
        *self.values.lock() = values;
    }

    /// Direct accessor for the latest value of a stateful widget (slider),
    /// keyed by id. The `ui:value(id)` Lua method delegates here.
    pub fn value(&self, id: &str) -> Option<f64> {
        self.values.lock().get(id).copied()
    }

    fn push_leaf(&self, mut node: UiNode) {
        // Compose: explicit widget style wins where set, pending fills gaps.
        let pending = self.pending.lock().clone();
        if let Some(s) = style_mut(&mut node) {
            merge_into(s, &pending);
        }
        self.stack.lock().last_mut().unwrap().push(node);
    }

    /// Open a child scope, run a Lua callback that fills it, then wrap the
    /// collected children into a container node pushed onto the parent.
    fn container<F>(
        &self,
        cb: mlua::Function,
        style: UiStyle,
        build: F,
    ) -> mlua::Result<()>
    where
        F: FnOnce(Vec<UiNode>, UiStyle) -> UiNode,
    {
        // Compose container style the same way as leaves.
        let mut style = style;
        let pending = self.pending.lock().clone();
        merge_into(&mut style, &pending);

        {
            let mut stack = self.stack.lock();
            stack.push(Vec::new());
        }
        // Nested widget calls target the new top of the shared stack. Pop the
        // scope exactly once on both the success and error paths so a failed
        // callback can't leave an abandoned container on the stack.
        let cb_result = cb.call::<LuaUi, ()>(self.clone());
        let children = self.stack.lock().pop().unwrap();
        cb_result?;
        let node = build(children, style);
        self.stack.lock().last_mut().unwrap().push(node);
        Ok(())
    }

    /// Drain the assembled tree + theme for this frame, resetting the builder.
    pub fn take(&self) -> (Vec<UiNode>, Option<UiTheme>) {
        let mut stack = self.stack.lock();
        let root = stack.get_mut(0).map(std::mem::take).unwrap_or_default();
        // Reset for the next fire: a fresh root scope.
        stack.clear();
        stack.push(Vec::new());
        drop(stack);
        *self.pending.lock() = UiStyle::default();
        let theme = self.theme.lock().take();
        (root, theme)
    }
}

/// Borrow the mutable style slot of a node, if it has one.
/// `Spacing` carries no style and returns `None`.
fn style_mut(node: &mut UiNode) -> Option<&mut UiStyle> {
    match node {
        UiNode::Metric { style, .. }
        | UiNode::Label { style, .. }
        | UiNode::Heading { style, .. }
        | UiNode::Button { style, .. }
        | UiNode::Hyperlink { style, .. }
        | UiNode::Checkbox { style, .. }
        | UiNode::Radio { style, .. }
        | UiNode::Toggle { style, .. }
        | UiNode::Slider { style, .. }
        | UiNode::ProgressBar { style, .. }
        | UiNode::Spinner { style, .. }
        | UiNode::Separator { style, .. }
        | UiNode::Sparkline { style, .. }
        | UiNode::Card { style, .. }
        | UiNode::CollapsingHeader { style, .. }
        | UiNode::Frame { style, .. }
        | UiNode::Horizontal { style, .. }
        | UiNode::Vertical { style, .. }
        | UiNode::Columns { style, .. }
        | UiNode::Grid { style, .. } => Some(style),
        UiNode::Spacing { .. } | UiNode::Image { .. } => None,
    }
}

/// Copy every `Some` field of `pending` into `dst` only where `dst` is `None`.
fn merge_into(_dst: &mut UiStyle, _pending: &UiStyle) {
    macro_rules! merge {
        ($f:ident) => {
            if _dst.$f.is_none() {
                _dst.$f = _pending.$f.clone();
            }
        };
    }
    merge!(color);
    merge!(background);
    merge!(accent);
    merge!(size);
    merge!(strong);
    merge!(italics);
    merge!(monospace);
    merge!(code);
    merge!(underline);
    merge!(strikethrough);
    merge!(rounding);
    merge!(fill);
    merge!(stroke);
    merge!(margin);
    merge!(spacing);
    merge!(expand);
    merge!(tooltip);
}

/// Parse a `#rrggbb` hex string into linear-ish RGBA.
/// ponytail: skips gamma correction; the renderer treats values as linear.
fn parse_hex_color(s: &str) -> Color {
    let hex = s.trim_start_matches('#');
    let (r, g, b) = if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        (r, g, b)
    } else {
        (0x39, 0xff, 0x14)
    };
    [
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        1.0,
    ]
}

fn color_opt(tbl: &mlua::Table, key: &str) -> Option<Color> {
    let s: Option<String> = tbl.get(key).ok().flatten();
    s.map(|s| parse_hex_color(&s))
}

fn opt<T>(tbl: &mlua::Table, key: &str) -> Option<T>
where
    T: for<'lua> mlua::FromLua<'lua>,
{
    tbl.get::<_, Option<T>>(key).ok().flatten()
}

/// Read a Lua options table into `UiStyle`. All fields optional.
fn parse_style(tbl: Option<&mlua::Table>) -> UiStyle {
    let mut s = UiStyle::default();
    let Some(tbl) = tbl else {
        return s;
    };
    if let Some(c) = color_opt(tbl, "color") {
        s.color = Some(c);
    }
    if let Some(c) = color_opt(tbl, "background") {
        s.background = Some(c);
    }
    if let Some(c) = color_opt(tbl, "fill") {
        s.fill = Some(c);
    }
    s.accent = opt(tbl, "accent");
    s.size = opt(tbl, "size");
    s.strong = opt(tbl, "strong");
    s.italics = opt(tbl, "italics");
    s.monospace = opt(tbl, "monospace");
    s.code = opt(tbl, "code");
    s.underline = opt(tbl, "underline");
    s.strikethrough = opt(tbl, "strikethrough");
    s.rounding = opt(tbl, "rounding");
    s.margin = opt(tbl, "margin");
    s.spacing = opt(tbl, "spacing");
    s.expand = opt(tbl, "expand");
    if let (Some(c), w) = (color_opt(tbl, "stroke_color"), opt::<f32>(tbl, "stroke_width"))
    {
        s.stroke = Some((c, w.unwrap_or(1.0)));
    }
    s.tooltip = opt(tbl, "tooltip");
    s
}

impl UserData for LuaUi {
    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, _this, _: ()| {
            Ok("LuaUi")
        });

        // --- Theme & global style --------------------------------------

        // ui:theme("catppuccin-mocha")
        methods.add_method("theme", |_, this, name: String| {
            *this.theme.lock() = Some(UiTheme::for_name(&name));
            Ok(())
        });

        // ui:style({ accent=true, color="#fff", rounding=8 }) -> sets the
        // "current style"; applies to every subsequently pushed widget until
        // the next ui:style() call. Mirrors egui's style stack.
        // A per-widget options table overrides the current style for that one.
        methods.add_method("style", |_, this, opts: Option<mlua::Table>| {
            *this.pending.lock() = parse_style(opts.as_ref());
            Ok(())
        });

        // ui:clicked("save") -> bool; reflects the last egui render pass.
        methods.add_method("clicked", |_, this, id: String| {
            Ok(this.events.lock().contains(&id))
        });

        // ui:value("workers") -> number|nil; the latest value of a stateful
        // widget (slider) from the last render pass, or nil if never set. Lets a
        // handler feed the live value back into the widget so drags persist:
        //   local v = ui:value("workers") or DEFAULT
        //   ui:slider({ id = "workers", value = v, min = 1, max = 16 })
        methods.add_method("value", |_, this, id: String| Ok(this.value(&id)));

        // --- Leaf widgets ----------------------------------------------

        methods.add_method(
            "metric",
            |_, this, (opts,): (mlua::Table,)| {
                let label: String = opts.get("label").unwrap_or_default();
                let value: String = opts.get("value").unwrap_or_default();
                let status: Option<String> = opts.get("status").ok().flatten();
                let style = parse_style(Some(&opts));
                this.push_leaf(UiNode::Metric {
                    label,
                    value,
                    status,
                    style,
                });
                Ok(())
            },
        );

        methods.add_method("label", |_, this, (text, opts): (String, Option<mlua::Table>)| {
            this.push_leaf(UiNode::Label {
                text,
                style: parse_style(opts.as_ref()),
            });
            Ok(())
        });

        methods.add_method(
            "heading",
            |_, this, (text, opts): (String, Option<mlua::Table>)| {
                let style = parse_style(opts.as_ref());
                let size = style.size;
                this.push_leaf(UiNode::Heading { text, size, style });
                Ok(())
            },
        );

        methods.add_method(
            "button",
            |_, this, (text, opts): (String, Option<mlua::Table>)| {
                let style = parse_style(opts.as_ref());
                let id = opts
                    .as_ref()
                    .and_then(|o| opt::<String>(o, "id"))
                    .unwrap_or_else(|| text.clone());
                this.push_leaf(UiNode::Button { text, id, style });
                Ok(())
            },
        );

        methods.add_method(
            "hyperlink",
            |_, this, (text, url, opts): (String, String, Option<mlua::Table>)| {
                this.push_leaf(UiNode::Hyperlink {
                    text,
                    url,
                    style: parse_style(opts.as_ref()),
                });
                Ok(())
            },
        );

        methods.add_method(
            "checkbox",
            |_, this, (opts,): (mlua::Table,)| {
                let id: String = opts.get("id").unwrap_or_else(|_| "checkbox".into());
                let label: String = opts.get("label").unwrap_or_default();
                let checked: bool = opts.get("checked").unwrap_or(false);
                let style = parse_style(Some(&opts));
                this.push_leaf(UiNode::Checkbox {
                    id,
                    label,
                    checked,
                    style,
                });
                Ok(())
            },
        );

        methods.add_method(
            "toggle",
            |_, this, (opts,): (mlua::Table,)| {
                let id: String = opts.get("id").unwrap_or_else(|_| "toggle".into());
                let label: String = opts.get("label").unwrap_or_default();
                let on: bool = opts.get("on").unwrap_or(false);
                let style = parse_style(Some(&opts));
                this.push_leaf(UiNode::Toggle {
                    id,
                    label,
                    on,
                    style,
                });
                Ok(())
            },
        );

        methods.add_method(
            "radio",
            |_, this, (opts,): (mlua::Table,)| {
                let id: String = opts.get("id").unwrap_or_else(|_| "radio".into());
                let label: String = opts.get("label").unwrap_or_default();
                let selected: bool = opts.get("selected").unwrap_or(false);
                let style = parse_style(Some(&opts));
                this.push_leaf(UiNode::Radio {
                    id,
                    label,
                    selected,
                    style,
                });
                Ok(())
            },
        );

        methods.add_method(
            "slider",
            |_, this, (opts,): (mlua::Table,)| {
                let id: String = opts.get("id").unwrap_or_else(|_| "slider".into());
                let value: f64 = opts.get("value").unwrap_or(0.0);
                let min: f64 = opts.get("min").unwrap_or(0.0);
                let max: f64 = opts.get("max").unwrap_or(100.0);
                let step: Option<f64> = opts.get("step").ok().flatten();
                let label: Option<String> = opts.get("label").ok().flatten();
                let style = parse_style(Some(&opts));
                this.push_leaf(UiNode::Slider {
                    id,
                    value,
                    min,
                    max,
                    step,
                    label,
                    style,
                });
                Ok(())
            },
        );

        methods.add_method(
            "progress_bar",
            |_, this, (opts,): (mlua::Table,)| {
                let fraction: f32 = opts.get("fraction").unwrap_or(0.0);
                let animated: bool = opts.get("animated").unwrap_or(true);
                let style = parse_style(Some(&opts));
                this.push_leaf(UiNode::ProgressBar {
                    fraction,
                    animated,
                    style,
                });
                Ok(())
            },
        );

        methods.add_method("spinner", |_, this, opts: Option<mlua::Table>| {
            this.push_leaf(UiNode::Spinner {
                style: parse_style(opts.as_ref()),
            });
            Ok(())
        });

        methods.add_method("separator", |_, this, opts: Option<mlua::Table>| {
            this.push_leaf(UiNode::Separator {
                style: parse_style(opts.as_ref()),
            });
            Ok(())
        });

        methods.add_method("spacing", |_, this, amount: Option<f32>| {
            this.push_leaf(UiNode::Spacing {
                amount: amount.unwrap_or(8.0),
            });
            Ok(())
        });

        methods.add_method("sparkline", |_, this, opts: mlua::Table| {
            let values: Vec<f32> = opts
                .get::<_, mlua::Table>("data")
                .ok()
                .map(|t| {
                    t.sequence_values::<f32>()
                        .filter_map(|v| v.ok())
                        .collect()
                })
                .unwrap_or_default();
            let color = color_opt(&opts, "color");
            let fill: bool = opts.get("fill").unwrap_or(false);
            let height = opt::<f32>(&opts, "height");
            let style = parse_style(Some(&opts));
            this.push_leaf(UiNode::Sparkline {
                values,
                color,
                fill,
                height,
                style,
            });
            Ok(())
        });

        // ui:image({ path="...", bytes=..., id=..., width=, height= })
        // `path` reads a file (PNG/JPEG/etc.); `bytes` takes raw encoded bytes
        // (read as a Lua byte string, so binary formats like PNG work).
        // `id` is the texture cache key — change it when the content changes.
        methods.add_method("image", |_, this, opts: mlua::Table| {
            let path: Option<String> = opts.get("path").ok().flatten();
            let bytes_lua: Option<mlua::String> = opts.get("bytes").ok().flatten();
            let bytes: Arc<[u8]> = match (path, bytes_lua) {
                (Some(p), _) => std::fs::read(&p)
                    .map_err(|e| mlua::Error::external(format!("image path {p:?}: {e}")))?
                    .into(),
                (None, Some(s)) => s.as_bytes().to_vec().into(),
                (None, None) => {
                    return Err(mlua::Error::external(
                        "ui:image requires `path` or `bytes`",
                    ))
                }
            };
            let id: String = opts.get("id").ok().flatten().unwrap_or_else(|| {
                // Default id from a small hash of the bytes so identical images
                // share a texture without caller bookkeeping.
                let mut h: u64 = 0xcbf29ce484222325;
                for b in bytes.iter() {
                    h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
                }
                format!("img-{h:x}")
            });
            let width = opt::<f32>(&opts, "width");
            let height = opt::<f32>(&opts, "height");
            this.push_leaf(UiNode::Image {
                id,
                bytes,
                width,
                height,
            });
            Ok(())
        });

        // --- Containers (take a callback that fills the scope) ----------

        // ui:card({ title=..., glass=... }, function(ui) ... end)
        methods.add_method(
            "card",
            |_, this, (opts, cb): (Option<mlua::Table>, mlua::Function)| {
                let title = opts.as_ref().and_then(|o| opt::<String>(o, "title"));
                let glass = opts.as_ref().map(|o| o.get("glass").unwrap_or(false)).unwrap_or(false);
                let style = parse_style(opts.as_ref());
                this.container(cb, style, |children, style| UiNode::Card {
                    title,
                    glass,
                    style,
                    children,
                })
            },
        );

        methods.add_method(
            "collapsing_header",
            |_, this, (title, opts, cb): (String, Option<mlua::Table>, mlua::Function)| {
                let default_open = opts
                    .as_ref()
                    .map(|o| o.get("default_open").unwrap_or(false))
                    .unwrap_or(false);
                let style = parse_style(opts.as_ref());
                this.container(cb, style, |children, style| UiNode::CollapsingHeader {
                    title,
                    default_open,
                    style,
                    children,
                })
            },
        );

        methods.add_method("frame", |_, this, (opts, cb): (Option<mlua::Table>, mlua::Function)| {
            let style = parse_style(opts.as_ref());
            this.container(cb, style, |children, style| UiNode::Frame { style, children })
        });

        methods.add_method("horizontal", |_, this, (opts, cb): (Option<mlua::Table>, mlua::Function)| {
            let style = parse_style(opts.as_ref());
            this.container(cb, style, |children, style| UiNode::Horizontal { style, children })
        });

        methods.add_method("vertical", |_, this, (opts, cb): (Option<mlua::Table>, mlua::Function)| {
            let style = parse_style(opts.as_ref());
            this.container(cb, style, |children, style| UiNode::Vertical { style, children })
        });

        methods.add_method(
            "columns",
            |_, this, (n, opts, cb): (usize, Option<mlua::Table>, mlua::Function)| {
                let style = parse_style(opts.as_ref());
                this.container(cb, style, move |children, style| UiNode::Columns {
                    n: n.max(1),
                    style,
                    children,
                })
            },
        );

        methods.add_method(
            "grid",
            |_, this, (id, opts, cb): (String, Option<mlua::Table>, mlua::Function)| {
                let style = parse_style(opts.as_ref());
                this.container(cb, style, |children, style| UiNode::Grid {
                    id,
                    style,
                    children,
                })
            },
        );
    }
}

/// Run a `LuaUi` through the assembled tree and flush it onto a `GuiPane`,
/// as the `render-gui-pane` wiring does each fire.
pub fn flush_ui_to_pane(ui: &LuaUi, pane: &GuiPane) {
    let (nodes, theme) = ui.take();
    if let Some(theme) = theme {
        pane.set_theme(theme);
    }
    pane.set_nodes(nodes);
}

/// `wezterm.gui.split_dashboard({ title=..., size=50 })`: split a `GuiPane`
/// into the active tab of the first window so a dashboard can actually appear.
/// Returns the new pane. The `render-gui-pane` handler (wired in `wezterm-gui`)
/// populates its widget tree.
pub fn split_dashboard(opts: Option<mlua::Table>) -> mlua::Result<MuxPane> {
    let mux = get_mux()?;
    let title: String = opts
        .as_ref()
        .and_then(|o| o.get::<_, Option<String>>("title").ok().flatten())
        .unwrap_or_else(|| "Dashboard".into());
    let pct: u8 = opts
        .as_ref()
        .and_then(|o| o.get::<_, Option<u8>>("size").ok().flatten())
        .unwrap_or(50)
        .min(95)
        .max(5);

    let win_id = mux
        .iter_windows()
        .into_iter()
        .next()
        .ok_or_else(|| mlua::Error::external("no open window to split"))?;
    let window = mux
        .get_window(win_id)
        .ok_or_else(|| mlua::Error::external("window vanished"))?;
    let tab = window
        .get_active()
        .ok_or_else(|| mlua::Error::external("no active tab"))?;

    let panes = tab.iter_panes();
    let active_index = panes.iter().position(|p| p.is_active).unwrap_or(0);
    let dims = panes
        .get(active_index)
        .map(|p| p.pane.get_dimensions())
        .unwrap_or_else(|| mux::renderable::RenderableDimensions {
            cols: tab.get_size().cols,
            viewport_rows: tab.get_size().rows,
            scrollback_rows: tab.get_size().rows,
            ..Default::default()
        });

    let gui: Arc<dyn Pane> = GuiPane::new(title, dims);
    // Register the GuiPane with the mux's pane map. The tab's prune_dead_panes
    // treats any pane not in the mux as dead and removes it (wezterm #4030);
    // normal PTY panes are registered by their spawn domain, but a GuiPane is
    // created directly, so register it ourselves or it's pruned on first paint.
    mux.add_pane(&gui)
        .map_err(|e| mlua::Error::external(format!("add_pane: {e:#}")))?;
    let request = SplitRequest {
        direction: SplitDirection::Horizontal,
        target_is_second: true,
        top_level: false,
        size: SplitSize::Percent(pct),
    };
    let new_idx = tab
        .split_and_insert(active_index, request, Arc::clone(&gui))
        .map_err(|e| mlua::Error::external(format!("{e:#}")))?;
    tab.set_active_idx(new_idx);
    Ok(MuxPane(gui.pane_id()))
}

#[cfg(test)]
mod test {
    use super::*;

    fn fresh_ui() -> LuaUi {
        let ui = LuaUi::new();
        // Simulate interaction state from the previous render pass.
        ui.set_events(["save".to_string()].into_iter().collect());
        ui
    }

    #[test]
    fn nested_themed_dashboard() -> anyhow::Result<()> {
        let lua = Lua::new();
        let ui = fresh_ui();
        lua.globals().set("ui", ui.clone())?;

        lua.load(
            r##"
            ui:theme("catppuccin-mocha")
            ui:style({ accent = true, rounding = 10 })

            ui:card({ title = "Agent Activity", glass = true }, function(ui)
              ui:metric({ label = "Active", value = "3", status = "running" })
              ui:metric({ label = "Queued", value = "1", status = "idle" })

              ui:collapsing_header("Pools", { default_open = true }, function(ui)
                ui:horizontal(nil, function(ui)
                  ui:button("Save", { id = "save" })
                  ui:button("Cancel", { id = "cancel" })
                end)
                ui:slider({ id = "workers", value = 4, min = 1, max = 16 })
                ui:checkbox({ id = "auto", label = "autoscale", checked = true })
                ui:progress_bar({ fraction = 0.6 })
              end)

              ui:sparkline({ data = { 10, 20, 15, 30, 45 }, color = "#39ff14", fill = true })
              ui:separator(nil)
              ui:label("done", { color = "#888888" })
            end)

            -- Click state from last frame is visible.
            assert(ui:clicked("save"))
            assert(not ui:clicked("cancel"))
            "##,
        )
        .exec()?;

        // Flush onto a GuiPane as the wiring does, then inspect the tree there
        // (take() drains the builder, so we read from the pane after flush).
        let dims = mux::renderable::RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            scrollback_rows: 24,
            physical_top: 0,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 800,
            pixel_height: 600,
            reverse_video: false,
        };
        let pane = GuiPane::new("Activity", dims);
        flush_ui_to_pane(&ui, &pane);
        assert_eq!(pane.theme().name, "catppuccin-mocha");

        let nodes = pane.nodes();
        assert_eq!(nodes.len(), 1, "one root card");
        let UiNode::Card { title, glass, children, .. } = &nodes[0] else {
            panic!("root is a card");
        };
        assert_eq!(title.as_deref(), Some("Agent Activity"));
        assert!(*glass, "glass enabled");
        assert_eq!(children.len(), 6);
        // Collapsing header holds the horizontal/slider/checkbox/progress row.
        let header = children.iter().find_map(|c| match c {
            UiNode::CollapsingHeader { children, .. } => Some(children),
            _ => None,
        });
        let header_children = header.expect("collapsing header present");
        assert!(header_children.iter().any(|c| matches!(c, UiNode::Horizontal { .. })));
        assert!(header_children.iter().any(|c| matches!(c, UiNode::Slider { .. })));
        Ok(())
    }

    #[test]
    fn style_current_style_persists() -> anyhow::Result<()> {
        let lua = Lua::new();
        let ui = LuaUi::new();
        lua.globals().set("ui", ui.clone())?;
        lua.load(
            r##"
            ui:label("plain")
            ui:style({ strong = true })
            ui:label("bold")
            ui:label("still-bold")
            ui:label("explicitly-off", { strong = false })
            "##,
        )
        .exec()?;
        let (nodes, _) = ui.take();
        let strongs: Vec<Option<bool>> = nodes
            .iter()
            .map(|n| match n {
                UiNode::Label { style, .. } => style.strong,
                _ => None,
            })
            .collect();
        // Current style (strong=true) applies to every widget after the call,
        // until overridden; the first label predates it, the last opts out.
        assert_eq!(strongs, vec![None, Some(true), Some(true), Some(false)]);
        Ok(())
    }

    #[test]
    fn image_node_builds() -> anyhow::Result<()> {
        let lua = Lua::new();
        let ui = LuaUi::new();
        lua.globals().set("ui", ui.clone())?;
        lua.load(
            r##"
            ui:image({ bytes = "\137PNG\r\n\10placeholder", id = "logo", width = 64, height = 64 })
            "##,
        )
        .exec()?;
        let (nodes, _) = ui.take();
        match &nodes[0] {
            UiNode::Image {
                id,
                bytes,
                width,
                height,
            } => {
                assert_eq!(id, "logo");
                assert_eq!(width, &Some(64.0));
                assert!(bytes.starts_with(b"\x89PNG"));
            }
            other => panic!("expected Image, got {:?}", std::mem::discriminant(other)),
        }
        Ok(())
    }

    #[test]
    fn value_readback_round_trips_to_lua() -> anyhow::Result<()> {
        // Simulate the render pass writing a slider value onto the pane, then a
        // refresh handing that snapshot to a fresh LuaUi. Lua must read it back
        // via ui:value(id), and an unknown id returns nil.
        let pane = GuiPane::new(
            "x",
            mux::renderable::RenderableDimensions::default(),
        );
        let mut vals = HashMap::new();
        vals.insert("workers".to_string(), 9.0);
        pane.set_values(vals);

        let ui = LuaUi::new();
        ui.set_values(pane.values_snapshot());

        assert_eq!(ui.value("workers"), Some(9.0));
        assert_eq!(ui.value("missing"), None);
        Ok(())
    }

    #[test]
    fn container_error_does_not_corrupt_stack() -> anyhow::Result<()> {
        // A callback that errors mid-container must not strand the child scope:
        // a sibling widget pushed afterwards still lands at the root level.
        let lua = Lua::new();
        let ui = LuaUi::new();
        lua.globals().set("ui", ui.clone())?;
        let _ = lua
            .load(
                r##"
                pcall(function()
                  ui:card(nil, function(ui)
                    ui:label("inside")
                    error("boom")
                  end)
                end)
                ui:label("after")
                "##,
            )
            .exec();
        let (nodes, _) = ui.take();
        // The post-error sibling is a top-level node (stack was restored).
        assert!(
            nodes.iter().any(|n| matches!(n, UiNode::Label { text, .. } if text == "after")),
            "sibling after a failed container reached the root: {:?}",
            nodes.iter().map(|n| format!("{:?}", std::mem::discriminant(n))).collect::<Vec<_>>()
        );
        Ok(())
    }
}
