use super::sidebar_ui::{self, UiLayout, UiNode};
use super::{TabInformation, TermWindow, UIItemType};
use config::ConfigHandle;
use mlua::Value;
use mux::pane::PaneId;
use mux::tab::TabId;
use mux::Mux;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use wezterm_dynamic::Value as DynamicValue;
use window::WindowOps;
use window::{CursorIcon, MouseEvent, MouseEventKind, MousePress};

pub const COMPACT_WIDTH_CELLS: usize = 6;
pub const ROW_HEIGHT_PX: usize = 40;
pub const MAX_SIDEBAR_WIDTH_CELLS: usize = 60;
pub const RESIZE_EDGE_PX: usize = 5;
pub const COMPACT_MAX_WIDTH_PT: f32 = 120.0;
pub const REGULAR_MIN_WIDTH_PT: f32 = 240.0;
pub const REGULAR_MAX_WIDTH_PT: f32 = 520.0;

fn activate_tab_id(action: &DynamicValue) -> Option<TabId> {
    let DynamicValue::Object(object) = action else {
        return None;
    };
    matches!(object.get_by_str("action"), Some(DynamicValue::String(action)) if action == "activate-tab")
        .then(|| object.get_by_str("tab_id").and_then(DynamicValue::coerce_unsigned))
        .flatten()
        .map(|tab_id| tab_id as TabId)
}

fn activate_pane_id(action: &DynamicValue) -> Option<PaneId> {
    let DynamicValue::Object(object) = action else {
        return None;
    };
    matches!(object.get_by_str("action"), Some(DynamicValue::String(action)) if action == "activate-pane")
        .then(|| object.get_by_str("pane_id").and_then(DynamicValue::coerce_unsigned))
        .flatten()
        .map(|pane_id| pane_id as PaneId)
}

#[derive(Debug, PartialEq, Eq)]
enum SpawnHostTabTarget {
    DefaultDomain,
    Pane(PaneId),
}

fn spawn_host_tab_target(action: &DynamicValue) -> Option<SpawnHostTabTarget> {
    let DynamicValue::Object(object) = action else {
        return None;
    };
    if !matches!(
        object.get_by_str("action"),
        Some(DynamicValue::String(action)) if action == "spawn-host-tab")
    {
        return None;
    }
    if matches!(
        object.get_by_str("domain"),
        Some(DynamicValue::String(domain)) if domain == "default")
    {
        return Some(SpawnHostTabTarget::DefaultDomain);
    }
    object
        .get_by_str("pane_id")
        .and_then(DynamicValue::coerce_unsigned)
        .map(|pane_id| SpawnHostTabTarget::Pane(pane_id as PaneId))
}

fn relative_sidebar_tab_id(
    tab_ids: &[TabId],
    active_tab_id: TabId,
    delta: isize,
    wrap: bool,
) -> Option<TabId> {
    let active = tab_ids.iter().position(|&tab_id| tab_id == active_tab_id)? as isize;
    let last = tab_ids.len().checked_sub(1)? as isize;
    let index = if wrap {
        (active + delta).rem_euclid(last + 1)
    } else {
        (active + delta).clamp(0, last)
    };
    tab_ids.get(index as usize).copied()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarGroup {
    pub key: String,
    pub label: String,
    pub host: String,
    pub worktree: String,
    pub depth: usize,
    pub active: bool,
    pub urgency: u8,
    pub counts: HashMap<String, usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarEntry {
    pub tab_id: TabId,
    pub controller_pane_id: Option<PaneId>,
    pub title: String,
    pub right: String,
    pub harness_glyph: String,
    pub status_glyph: String,
    pub status_color: Option<String>,
    pub status_key: String,
    pub progress: String,
    pub urgency: u8,
    pub groups: Vec<SidebarGroup>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SidebarHover {
    Node(String),
}

#[derive(Clone, Debug)]
pub(super) struct SidebarResize;

#[derive(Default)]
pub struct TabSidebar {
    pub ui_tree: Option<UiNode>,
    pub ui_layout: Option<UiLayout>,
    pub ui_target_layout: Option<UiLayout>,
    pub tab_ids: Vec<TabId>,
    pub ui_layout_size: Option<(usize, usize)>,
    pub ui_animation_started: Option<Instant>,
    pub ui_scroll_offset: f32,
    pub ui_scroll_max: f32,
    pub dirty: bool,
    pub width_cells_override: Option<usize>,
    pub expanded_width_cells: Option<usize>,
    pub(super) hovered: Option<SidebarHover>,
    pub(super) resize: Option<SidebarResize>,
    pub(super) refresh_generation: u64,
}

impl TabSidebar {
    fn relative_tab_id(&self, active_tab_id: TabId, delta: isize, wrap: bool) -> Option<TabId> {
        relative_sidebar_tab_id(&self.tab_ids, active_tab_id, delta, wrap)
    }

    pub fn width_cells(&self, config: &ConfigHandle, cell_width: f32, dpi: u32) -> usize {
        responsive_width_cells(
            self.width_cells_override
                .unwrap_or(config.tab_sidebar_width),
            cell_width,
            dpi,
        )
    }

    pub fn is_compact(&self, config: &ConfigHandle, cell_width: f32, dpi: u32) -> bool {
        let width = self.width_cells(config, cell_width, dpi) as f32 * cell_width;
        width * 96.0 / dpi.max(1) as f32 <= COMPACT_MAX_WIDTH_PT
    }

    pub fn is_resizing(&self) -> bool {
        self.resize.is_some()
    }
}

pub fn responsive_width_cells(requested: usize, cell_width: f32, dpi: u32) -> usize {
    let requested = requested.max(COMPACT_WIDTH_CELLS);
    let points = requested as f32 * cell_width * 96.0 / dpi.max(1) as f32;
    if points <= COMPACT_MAX_WIDTH_PT {
        return requested;
    }
    let points = points.clamp(REGULAR_MIN_WIDTH_PT, REGULAR_MAX_WIDTH_PT);
    (points * dpi.max(1) as f32 / 96.0 / cell_width.max(1.0))
        .round()
        .clamp(
            (COMPACT_WIDTH_CELLS + 1) as f32,
            MAX_SIDEBAR_WIDTH_CELLS as f32,
        ) as usize
}

fn toggled_sidebar_width(
    compact: bool,
    current: usize,
    expanded: Option<usize>,
    configured: usize,
    cell_width: f32,
    dpi: u32,
) -> (usize, Option<usize>) {
    if compact {
        let first_regular_cell =
            (COMPACT_MAX_WIDTH_PT * dpi.max(1) as f32 / 96.0 / cell_width.max(1.0)).floor()
                as usize
                + 1;
        let requested = expanded.unwrap_or(configured).max(first_regular_cell);
        (responsive_width_cells(requested, cell_width, dpi), expanded)
    } else {
        (COMPACT_WIDTH_CELLS, Some(current))
    }
}
impl TermWindow {
    pub fn sidebar_relative_tab_id(&self, delta: isize, wrap: bool) -> Option<TabId> {
        let active_tab_id = Mux::get()
            .get_window(self.mux_window_id)
            .and_then(|window| window.get_active_tab().map(|tab| tab.tab_id()))?;
        self.tab_sidebar
            .relative_tab_id(active_tab_id, delta, wrap)
    }

    pub fn tab_sidebar_width_pixels(&self) -> usize {
        if self.tab_sidebar_enabled {
            self.tab_sidebar.width_cells(
                &self.config,
                self.render_metrics.cell_size.width as f32,
                self.dimensions.dpi as u32,
            ) * self.render_metrics.cell_size.width as usize
        } else {
            0
        }
    }

    pub fn mark_tab_sidebar_dirty(&mut self) {
        if self.tab_sidebar_enabled {
            self.tab_sidebar.dirty = true;
            if !self.tab_sidebar_refresh_queued {
                self.tab_sidebar_refresh_queued = true;
                if let Some(window) = self.window.as_ref() {
                    window.notify(super::TermWindowNotif::Apply(Box::new(|term| {
                        term.tab_sidebar_refresh_queued = false;
                        let tabs = term.get_tab_information();
                        term.refresh_tab_sidebar(&tabs);
                    })));
                }
            }
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    pub fn refresh_tab_sidebar(&mut self, tabs: &[TabInformation]) {
        if !self.tab_sidebar_enabled || !self.tab_sidebar.dirty {
            return;
        }

        let started = std::time::Instant::now();
        let result = callback_entries(
            tabs,
            self.tab_sidebar.is_compact(
                &self.config,
                self.render_metrics.cell_size.width as f32,
                self.dimensions.dpi as u32,
            ),
            self.tab_sidebar_width_pixels(),
            self.dimensions.pixel_height,
            self.dimensions.dpi as u32,
        )
        .and_then(|callback| {
            let valid_tabs = tabs
                .iter()
                .map(|tab| tab.tab_id)
                .collect::<std::collections::HashSet<_>>();
            install_callback(&mut self.tab_sidebar, &valid_tabs, callback)
        });
        let refresh_after = match result {
            Ok(refresh_after) => {
                self.sidebar_images.clear();
                refresh_after
            }
            Err(err) => {
                log::warn!("format-tab-sidebar: keeping last good output: {err:#}");
                Some(Duration::from_secs(1))
            }
        };
        self.tab_sidebar.dirty = false;
        self.tab_sidebar.refresh_generation = self.tab_sidebar.refresh_generation.wrapping_add(1);
        if let Some(refresh_after) = refresh_after {
            self.schedule_tab_sidebar_refresh(refresh_after);
        }
        metrics::histogram!("tab_sidebar.model_rebuild").record(started.elapsed());
    }

    fn schedule_tab_sidebar_refresh(&self, refresh_after: Duration) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let generation = self.tab_sidebar.refresh_generation;
        let target = Instant::now() + refresh_after;
        promise::spawn::spawn(async move {
            smol::Timer::at(target).await;
            let invalidate = window.clone();
            window.notify(super::TermWindowNotif::Apply(Box::new(move |term| {
                if term.tab_sidebar.refresh_generation == generation {
                    term.tab_sidebar.dirty = true;
                    let tabs = term.get_tab_information();
                    term.refresh_tab_sidebar(&tabs);
                    invalidate.invalidate();
                }
            })));
        })
        .detach();
    }

    pub fn tab_sidebar_scroll(&mut self, rows: isize) -> bool {
        if !self.tab_sidebar_enabled || rows == 0 {
            return false;
        }
        let pixels_per_point = (self.dimensions.dpi as f32 / 96.0).max(1.0);
        let step = ROW_HEIGHT_PX as f32 / pixels_per_point;
        let previous = self.tab_sidebar.ui_scroll_offset;
        self.tab_sidebar.ui_scroll_offset =
            (previous + rows as f32 * step).clamp(0.0, self.tab_sidebar.ui_scroll_max);
        self.tab_sidebar.ui_scroll_offset != previous
    }

    fn sidebar_item_at(&self, event: &MouseEvent) -> Option<UIItemType> {
        let pixels_per_point = (self.dimensions.dpi as f32 / 96.0).max(1.0);
        if let Some(node) = self.tab_sidebar.ui_layout.as_ref().and_then(|layout| {
            layout.interactive_at(
                event.coords.x as f32 / pixels_per_point,
                event.coords.y as f32 / pixels_per_point,
                self.tab_sidebar.ui_scroll_offset,
            )
        }) {
            return Some(UIItemType::SidebarNode(node.id.clone()));
        }
        self.ui_items
            .iter()
            .rev()
            .find(|item| item.hit_test(event.coords.x, event.coords.y))
            .map(|item| item.item_type.clone())
    }

    fn sidebar_clickable_item_at(&self, event: &MouseEvent) -> Option<UIItemType> {
        let pixels_per_point = (self.dimensions.dpi as f32 / 96.0).max(1.0);
        self.tab_sidebar
            .ui_layout
            .as_ref()
            .and_then(|layout| {
                layout.clickable_at(
                    event.coords.x as f32 / pixels_per_point,
                    event.coords.y as f32 / pixels_per_point,
                    self.tab_sidebar.ui_scroll_offset,
                )
            })
            .map(|node| UIItemType::SidebarNode(node.id.clone()))
            .or_else(|| {
                self.ui_items
                    .iter()
                    .rev()
                    .find(|item| item.hit_test(event.coords.x, event.coords.y))
                    .map(|item| item.item_type.clone())
            })
    }

    fn sidebar_hover_at(&self, event: &MouseEvent) -> Option<SidebarHover> {
        match self.sidebar_item_at(event) {
            Some(UIItemType::SidebarNode(id)) => Some(SidebarHover::Node(id)),
            _ => None,
        }
    }

    pub(super) fn set_sidebar_hover_at(&mut self, x: isize, y: isize) {
        let pixels_per_point = (self.dimensions.dpi as f32 / 96.0).max(1.0);
        let layout_hover = self
            .tab_sidebar
            .ui_layout
            .as_ref()
            .and_then(|layout| {
                layout.hit_test_scrolled(
                    x as f32 / pixels_per_point,
                    y as f32 / pixels_per_point,
                    self.tab_sidebar.ui_scroll_offset,
                )
            })
            .filter(|node| node.is_interactive())
            .map(|node| SidebarHover::Node(node.id.clone()));
        if layout_hover.is_some() {
            self.tab_sidebar.hovered = layout_hover;
            return;
        }
        self.tab_sidebar.hovered = self
            .ui_items
            .iter()
            .rev()
            .find(|item| item.hit_test(x, y))
            .and_then(|item| match &item.item_type {
                UIItemType::SidebarNode(id) => Some(SidebarHover::Node(id.clone())),
                _ => None,
            });
    }

    fn activate_sidebar_node(&mut self, id: &str) {
        let action = self
            .tab_sidebar
            .ui_layout
            .as_ref()
            .and_then(|layout| layout.nodes.iter().find(|node| node.id == id))
            .and_then(|node| node.on_click.clone());
        let Some(action) = action else { return };
        if matches!(
            &action,
            DynamicValue::Object(object)
                if matches!(object.get_by_str("action"),
                    Some(DynamicValue::String(name)) if name == "toggle-sidebar-width")
        ) {
            let cell_width = self.render_metrics.cell_size.width as f32;
            let dpi = self.dimensions.dpi as u32;
            let current = self.tab_sidebar.width_cells(&self.config, cell_width, dpi);
            let compact = self.tab_sidebar.is_compact(&self.config, cell_width, dpi);
            let (width, expanded) = toggled_sidebar_width(
                compact,
                current,
                self.tab_sidebar.expanded_width_cells,
                self.config.tab_sidebar_width,
                cell_width,
                dpi,
            );
            self.tab_sidebar.width_cells_override = Some(width);
            self.tab_sidebar.expanded_width_cells = expanded;
            if let Some(window) = self.window.clone() {
                let dimensions = self.dimensions;
                self.apply_dimensions(&dimensions, None, &window);
            }
            self.mark_tab_sidebar_dirty();
            return;
        }
        if let Some(tab_id) = activate_tab_id(&action) {
            self.activate_sidebar_tab(tab_id);
            return;
        }
        if let Some(pane_id) = activate_pane_id(&action) {
            if let Err(err) = Mux::get().focus_pane_and_containing_tab(pane_id) {
                log::warn!("activate-pane {pane_id}: {err:#}");
                return;
            }
            self.request_terminal_repaint();
            self.mark_tab_sidebar_dirty();
            self.emit_status_event();
            return;
        }
        if let Some(target) = spawn_host_tab_target(&action) {
            let domain = match target {
                SpawnHostTabTarget::DefaultDomain => {
                    config::keyassignment::SpawnTabDomain::DefaultDomain
                }
                SpawnHostTabTarget::Pane(pane_id) => {
                    let Some(pane) = Mux::get().get_pane(pane_id) else {
                        log::warn!("spawn-host-tab pane {pane_id} is no longer available");
                        return;
                    };
                    config::keyassignment::SpawnTabDomain::DomainId(pane.domain_id_for_spawn())
                }
            };
            self.spawn_tab(&domain); // @fdb:fleet-navigation-and-gui-qa
            return;
        }
        self.emit_sidebar_action(action);
    }

    pub fn handle_tab_sidebar_mouse_event(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> bool {
        if !self.tab_sidebar_enabled {
            return false;
        }
        let width = self.tab_sidebar_width_pixels();
        let inside = event.coords.x >= 0 && (event.coords.x as usize) < width;
        let on_edge = !self.tab_sidebar.is_compact(
            &self.config,
            self.render_metrics.cell_size.width as f32,
            self.dimensions.dpi as u32,
        ) && event.coords.x >= 0
            && (event.coords.x as usize).saturating_add(RESIZE_EDGE_PX) >= width
            && (event.coords.x as usize) <= width + RESIZE_EDGE_PX;
        if !inside && self.tab_sidebar.resize.is_none() {
            if self.tab_sidebar.hovered.take().is_some() {
                context.invalidate();
            }
            return false;
        }

        let mut invalidate = false;
        match event.kind {
            MouseEventKind::VertWheel(delta) if inside => {
                invalidate |= self.tab_sidebar_scroll((-delta).signum() as isize);
            }
            MouseEventKind::Press(MousePress::Left) => {
                if on_edge {
                    self.tab_sidebar.resize = Some(SidebarResize);
                    invalidate = true;
                } else if inside {
                    match self.sidebar_clickable_item_at(event) {
                        Some(UIItemType::SidebarNode(id)) => {
                            self.activate_sidebar_node(&id);
                            invalidate = true;
                        }
                        _ => {}
                    }
                }
            }
            MouseEventKind::Move => {
                if let Some(_resize) = self.tab_sidebar.resize.as_ref() {
                    let cell_width = self.render_metrics.cell_size.width.max(1) as usize;
                    let new_cells = (event.coords.x as usize / cell_width)
                        .clamp(COMPACT_WIDTH_CELLS + 1, MAX_SIDEBAR_WIDTH_CELLS);
                    if Some(new_cells) != self.tab_sidebar.width_cells_override {
                        let was_compact = self.tab_sidebar.is_compact(
                            &self.config,
                            self.render_metrics.cell_size.width as f32,
                            self.dimensions.dpi as u32,
                        );
                        self.tab_sidebar.width_cells_override = Some(new_cells);
                        let is_compact = self.tab_sidebar.is_compact(
                            &self.config,
                            self.render_metrics.cell_size.width as f32,
                            self.dimensions.dpi as u32,
                        );
                        if was_compact != is_compact {
                            self.mark_tab_sidebar_dirty();
                        }
                    }
                    invalidate = true;
                }
                let hovered = if inside {
                    self.sidebar_hover_at(event)
                } else {
                    None
                };
                let hover_action = match hovered.as_ref() {
                    Some(SidebarHover::Node(id)) => self
                        .tab_sidebar
                        .ui_layout
                        .as_ref()
                        .and_then(|layout| layout.nodes.iter().find(|node| node.id == *id))
                        .and_then(|node| node.on_hover.clone()),
                    _ => None,
                };
                let hover_changed = self.tab_sidebar.hovered != hovered;
                invalidate |= hover_changed;
                self.tab_sidebar.hovered = hovered;
                if hover_changed {
                    if let Some(action) = hover_action {
                        self.emit_sidebar_action(action);
                    }
                }
            }
            MouseEventKind::Release(MousePress::Left) => {
                if self.tab_sidebar.resize.take().is_some() {
                    invalidate = true;
                }
            }
            _ => {}
        }

        let hovered_clickable = match self.tab_sidebar.hovered.as_ref() {
            Some(SidebarHover::Node(id)) => self
                .tab_sidebar
                .ui_layout
                .as_ref()
                .and_then(|layout| layout.nodes.iter().find(|node| node.id == *id))
                .is_some_and(|node| node.is_clickable()),
            _ => false,
        };
        let cursor = if self.tab_sidebar.resize.is_some() || on_edge {
            CursorIcon::EwResize
        } else if hovered_clickable {
            CursorIcon::Pointer
        } else {
            CursorIcon::Default
        };
        context.set_cursor(Some(cursor));
        if invalidate {
            context.invalidate();
        }
        true
    }
}

struct CallbackResult {
    entries: HashMap<TabId, SidebarEntry>,
    tab_ids: Vec<TabId>,
    ui_tree: Option<UiNode>,
    refresh_after: Option<Duration>,
}

fn validate_callback_entries(
    valid_tabs: &std::collections::HashSet<TabId>,
    entries: &HashMap<TabId, SidebarEntry>,
) -> anyhow::Result<()> {
    if let Some(unknown) = entries.keys().find(|tab_id| !valid_tabs.contains(tab_id)) {
        anyhow::bail!("unknown tab_id {unknown}");
    }
    Ok(())
}

fn install_callback(
    sidebar: &mut TabSidebar,
    valid_tabs: &std::collections::HashSet<TabId>,
    callback: CallbackResult,
) -> anyhow::Result<Option<Duration>> {
    validate_callback_entries(valid_tabs, &callback.entries)?;
    sidebar.ui_tree = callback.ui_tree;
    sidebar.tab_ids = callback.tab_ids;
    sidebar.ui_target_layout = None;
    sidebar.ui_layout_size = None;
    Ok(callback.refresh_after)
}

fn callback_entries(
    tabs: &[TabInformation],
    compact: bool,
    width: usize,
    height: usize,
    dpi: u32,
) -> anyhow::Result<CallbackResult> {
    config::run_immediate_with_lua_config(|lua| {
        let Some(lua) = lua else {
            return Ok(CallbackResult {
                entries: HashMap::new(),
                tab_ids: vec![],
                ui_tree: None,
                refresh_after: None,
            });
        };
        let context = lua.create_table()?;
        context.set("mode", if compact { "compact" } else { "regular" })?;
        context.set("width", width as f32 * 96.0 / dpi.max(1) as f32)?;
        context.set("height", height as f32 * 96.0 / dpi.max(1) as f32)?;
        context.set("dpi", dpi)?;
        let format_tabs = lua.create_sequence_from(tabs.iter().cloned())?;
        let value =
            config::lua::emit_sync_callback(&lua, ("format-tab-sidebar".to_string(), format_tabs))?;
        let mut callback = decode_callback(&lua, value)?;
        let tree_tabs = lua.create_sequence_from(tabs.iter().cloned())?;
        let tree = config::lua::emit_sync_callback(
            &lua,
            ("render-sidebar".to_string(), (tree_tabs, context)),
        )?;
        callback.ui_tree = sidebar_ui::decode(tree, &lua)?;
        Ok(callback)
    })
}

fn decode_callback(lua: &mlua::Lua, value: Value) -> anyhow::Result<CallbackResult> {
    let Value::Table(result) = value else {
        return Ok(CallbackResult {
            entries: HashMap::new(),
            tab_ids: vec![],
            ui_tree: None,
            refresh_after: None,
        });
    };
    let refresh_after = match result.get::<_, Value>("refresh_after_ms")? {
        Value::Nil => None,
        Value::Integer(ms) if ms >= 0 => Some(Duration::from_millis((ms as u64).max(100))),
        Value::Number(ms) if ms.is_finite() && ms >= 0.0 => {
            Some(Duration::from_millis((ms as u64).max(100)))
        }
        _ => anyhow::bail!("format-tab-sidebar refresh_after_ms must be a non-negative number"),
    };
    let entries = match result.get::<_, Value>("entries")? {
        Value::Nil => result,
        Value::Table(entries) => entries,
        _ => anyhow::bail!("format-tab-sidebar must return {{ entries = {{...}} }}"),
    };
    let mut decoded = HashMap::new();
    let mut tab_ids = vec![];
    for value in entries.sequence_values::<Value>() {
        let Value::Table(entry) = value? else {
            anyhow::bail!("format-tab-sidebar entries must be tables");
        };
        let tab_id = entry.get::<_, TabId>("tab_id")?;
        let title = entry.get::<_, Option<String>>("title")?.unwrap_or_default();
        let right = entry.get::<_, Option<String>>("right")?.unwrap_or_default();
        let harness_glyph = entry
            .get::<_, Option<String>>("harness")?
            .unwrap_or_default();
        let progress = entry
            .get::<_, Option<String>>("progress")?
            .unwrap_or_default();
        let urgency = entry.get::<_, Option<u8>>("urgency")?.unwrap_or(0).min(2);
        let (status_key, status_glyph, status_color) = match entry.get::<_, Value>("status")? {
            Value::Table(status) => (
                status.get::<_, Option<String>>("key")?.unwrap_or_default(),
                status
                    .get::<_, Option<String>>("glyph")?
                    .unwrap_or_default(),
                status.get::<_, Option<String>>("color")?,
            ),
            Value::Nil => (String::new(), String::new(), None),
            _ => anyhow::bail!("format-tab-sidebar status must be a table"),
        };
        let groups = decode_groups(lua, entry.get::<_, Value>("group_path")?)?;
        if decoded
            .insert(
                tab_id,
                SidebarEntry {
                    tab_id,
                    controller_pane_id: None,
                    title,
                    right,
                    harness_glyph,
                    status_glyph,
                    status_color,
                    status_key,
                    progress,
                    urgency,
                    groups,
                    active: false,
                },
            )
            .is_some()
        {
            anyhow::bail!("format-tab-sidebar returned duplicate tab_id {tab_id}");
        }
        tab_ids.push(tab_id);
    }
    Ok(CallbackResult {
        entries: decoded,
        tab_ids,
        ui_tree: None,
        refresh_after,
    })
}

fn decode_groups(_lua: &mlua::Lua, value: Value) -> anyhow::Result<Vec<SidebarGroup>> {
    let Value::Table(groups) = value else {
        return if matches!(value, Value::Nil) {
            Ok(vec![])
        } else {
            anyhow::bail!("format-tab-sidebar group_path must be a sequence")
        };
    };
    groups
        .sequence_values::<Value>()
        .map(|value| match value? {
            Value::String(value) => {
                let value = value.to_str()?.to_string();
                Ok(SidebarGroup {
                    key: value.clone(),
                    label: value,
                    ..Default::default()
                })
            }
            Value::Table(group) => {
                let key = group.get::<_, String>("key")?;
                Ok(SidebarGroup {
                    label: group
                        .get::<_, Option<String>>("label")?
                        .unwrap_or_else(|| key.clone()),
                    host: group.get::<_, Option<String>>("host")?.unwrap_or_default(),
                    worktree: group
                        .get::<_, Option<String>>("worktree")?
                        .unwrap_or_default(),
                    key,
                    ..Default::default()
                })
            }
            _ => anyhow::bail!("format-tab-sidebar group_path entries must be strings or tables"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_decode_falls_back_and_rejects_duplicates() {
        let lua = mlua::Lua::new();
        let fallback = decode_callback(&lua, Value::Nil).unwrap();
        assert!(fallback.entries.is_empty());

        let duplicate = lua
            .load("return { entries = {{tab_id=1}, {tab_id=1}} }")
            .eval()
            .unwrap();
        assert!(decode_callback(&lua, duplicate).is_err());

        let malformed = lua.load("return { entries = 1 }").eval().unwrap();
        assert!(decode_callback(&lua, malformed).is_err());
    }

    #[test]
    fn callback_rejects_unknown_tabs_and_clamps_refresh_deadline() {
        let lua = mlua::Lua::new();
        let value = lua
            .load("return { refresh_after_ms = 1, entries = {{tab_id=2}} }")
            .eval()
            .unwrap();
        let callback = decode_callback(&lua, value).unwrap();
        assert_eq!(callback.refresh_after, Some(Duration::from_millis(100)));
        let valid = [1usize]
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert!(validate_callback_entries(&valid, &callback.entries).is_err());
    }

    #[test]
    fn invalid_callback_keeps_last_good_sidebar_state() {
        let lua = mlua::Lua::new();
        let value = lua
            .load("return { entries = {{tab_id=2}} }")
            .eval()
            .unwrap();
        let callback = decode_callback(&lua, value).unwrap();
        let valid = [1usize]
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let mut sidebar = TabSidebar {
            tab_ids: vec![1],
            ..Default::default()
        };
        assert!(install_callback(&mut sidebar, &valid, callback).is_err());
        assert_eq!(sidebar.tab_ids, vec![1]);
    }

    #[test]
    fn responsive_width_uses_compact_and_regular_breakpoints() {
        assert_eq!(responsive_width_cells(6, 10.0, 96), 6);
        assert_eq!(responsive_width_cells(13, 10.0, 96), 24);
        assert_eq!(responsive_width_cells(60, 10.0, 96), 52);
    }

    #[test]
    fn sidebar_button_collapses_and_restores_regular_width() {
        assert_eq!(
            toggled_sidebar_width(false, 32, None, 32, 10.0, 96),
            (COMPACT_WIDTH_CELLS, Some(32))
        );
        assert_eq!(
            toggled_sidebar_width(true, COMPACT_WIDTH_CELLS, Some(32), 32, 10.0, 96),
            (32, Some(32))
        );
        assert_eq!(
            toggled_sidebar_width(true, COMPACT_WIDTH_CELLS, None, 6, 10.0, 96),
            (24, None)
        );
    }

    #[test]
    fn relative_tab_navigation_follows_sidebar_row_order() {
        let tabs = [30, 10, 20];
        assert_eq!(relative_sidebar_tab_id(&tabs, 30, 1, true), Some(10));
        assert_eq!(relative_sidebar_tab_id(&tabs, 10, -1, true), Some(30));
        assert_eq!(relative_sidebar_tab_id(&tabs, 30, -1, true), Some(20));
        assert_eq!(relative_sidebar_tab_id(&tabs, 30, -1, false), Some(30));
    }

    #[test]
    fn callback_order_wins_over_duplicated_layout_rows() {
        let lua = mlua::Lua::new();
        let value = lua
            .load("return { entries = {{tab_id=10}, {tab_id=20}, {tab_id=30}} }")
            .eval()
            .unwrap();
        let callback = decode_callback(&lua, value).unwrap();
        let mut layout_tabs = vec![];
        for tab_id in [20, 10, 20, 30] {
            if !layout_tabs.contains(&tab_id) {
                layout_tabs.push(tab_id);
            }
        }
        assert_eq!(relative_sidebar_tab_id(&layout_tabs, 10, 1, true), Some(30));
        let sidebar = TabSidebar {
            tab_ids: callback.tab_ids,
            ..Default::default()
        };
        assert_eq!(sidebar.relative_tab_id(10, 1, true), Some(20));
    }

    #[test]
    fn host_tab_action_selects_default_or_exact_pane() {
        let lua = mlua::Lua::new();
        let local = lua
            .load("return {action='spawn-host-tab', domain='default'}")
            .eval::<Value>()
            .unwrap();
        let remote = lua
            .load("return {action='spawn-host-tab', pane_id=42}")
            .eval::<Value>()
            .unwrap();
        assert_eq!(
            spawn_host_tab_target(&luahelper::lua_value_to_dynamic(local).unwrap()),
            Some(SpawnHostTabTarget::DefaultDomain)
        );
        assert_eq!(
            spawn_host_tab_target(&luahelper::lua_value_to_dynamic(remote).unwrap()),
            Some(SpawnHostTabTarget::Pane(42))
        );
    }

    #[test]
    fn pane_action_targets_the_exact_split() {
        let lua = mlua::Lua::new();
        let action = lua
            .load("return {action='activate-pane', pane_id=42}")
            .eval::<Value>()
            .unwrap();
        assert_eq!(
            activate_pane_id(&luahelper::lua_value_to_dynamic(action).unwrap()),
            Some(42)
        );
    }
}
