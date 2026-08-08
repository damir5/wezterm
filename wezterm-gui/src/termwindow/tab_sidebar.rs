use super::{TabInformation, TermWindow, UIItem, UIItemType};
use config::ConfigHandle;
use mlua::Value;
use mux::pane::PaneId;
use mux::tab::TabId;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use window::WindowOps;
use window::{MouseCursor, MouseEvent, MouseEventKind, MousePress};

pub const COMPACT_WIDTH_CELLS: usize = 6;
pub const ROW_HEIGHT_PX: usize = 40;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarGroup {
    pub key: String,
    pub label: String,
    pub depth: usize,
    pub active: bool,
    pub urgency: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarEntry {
    pub tab_id: TabId,
    pub controller_pane_id: Option<PaneId>,
    pub title: String,
    pub right: String,
    pub status_glyph: String,
    pub status_color: Option<String>,
    pub urgency: u8,
    pub groups: Vec<SidebarGroup>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SidebarHover {
    Group(String),
    Tab(TabId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SidebarDropTarget {
    pub(super) tab_id: TabId,
    pub(super) before: bool,
}

#[derive(Clone, Debug)]
pub(super) struct SidebarDrag {
    pub(super) tab_id: TabId,
    pub(super) start_x: isize,
    pub(super) start_y: isize,
    pub(super) active: bool,
    pub(super) target: Option<SidebarDropTarget>,
}

#[derive(Default)]
pub struct TabSidebar {
    pub entries: Vec<SidebarEntry>,
    pub dirty: bool,
    pub compact: bool,
    pub scroll_rows: usize,
    pub collapsed: std::collections::HashSet<String>,
    pub(super) hovered: Option<SidebarHover>,
    pub(super) drag: Option<SidebarDrag>,
    pub(super) refresh_generation: u64,
}

impl TabSidebar {
    pub fn width_cells(&self, config: &ConfigHandle) -> usize {
        if self.compact {
            COMPACT_WIDTH_CELLS
        } else {
            config.tab_sidebar_width.max(COMPACT_WIDTH_CELLS + 1)
        }
    }
}

impl TermWindow {
    pub fn tab_sidebar_width_pixels(&self) -> usize {
        if self.tab_sidebar_enabled {
            self.tab_sidebar.width_cells(&self.config) * self.render_metrics.cell_size.width as usize
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
        let mut entries = native_entries(tabs);
        let refresh_after = match callback_entries(tabs) {
            Ok(callback) => {
                let valid_tabs = entries.iter().map(|entry| entry.tab_id).collect::<std::collections::HashSet<_>>();
                if let Err(err) = validate_callback_entries(&valid_tabs, &callback.entries) {
                    log::warn!("format-tab-sidebar: ignoring all callback output: {err:#}");
                    None
                } else {
                    let metadata = callback.entries;
                for entry in &mut entries {
                    if let Some(metadata) = metadata.get(&entry.tab_id) {
                        entry.title = metadata.title.clone();
                        entry.right = metadata.right.clone();
                        entry.status_glyph = metadata.status_glyph.clone();
                        entry.status_color = metadata.status_color.clone();
                        entry.urgency = metadata.urgency;
                        entry.groups = metadata.groups.clone();
                    }
                }
                    callback.refresh_after
                }
            }
            Err(err) => {
                log::warn!("format-tab-sidebar: {err:#}");
                None
            }
        };

        self.tab_sidebar.entries = entries;
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

    pub fn set_tab_sidebar_active(&mut self, tab_id: TabId) {
        for entry in &mut self.tab_sidebar.entries {
            entry.active = entry.tab_id == tab_id;
        }
        self.tab_sidebar_rows = sidebar_rows(&self.tab_sidebar);
    }

    pub fn expand_tab_sidebar_group(&mut self, group: &str) {
        self.tab_sidebar.compact = false;
        self.tab_sidebar.collapsed.remove(group);
        self.tab_sidebar.scroll_rows = sidebar_rows(&self.tab_sidebar)
            .iter()
            .position(|row| matches!(row, SidebarRow::Group(item) if item.key == group))
            .unwrap_or(0);
    }

    pub fn tab_sidebar_scroll(&mut self, rows: isize) -> bool {
        if !self.tab_sidebar_enabled || rows == 0 {
            return false;
        }
        let max = max_scroll_rows(
            sidebar_rows(&self.tab_sidebar).len(),
            self.dimensions.pixel_height,
            ROW_HEIGHT_PX,
        );
        let previous = self.tab_sidebar.scroll_rows;
        self.tab_sidebar.scroll_rows = if rows < 0 {
            self.tab_sidebar.scroll_rows.saturating_sub(rows.unsigned_abs())
        } else {
            self.tab_sidebar.scroll_rows.saturating_add(rows as usize).min(max)
        };
        self.tab_sidebar.scroll_rows != previous
    }

    fn sidebar_item_at(&self, event: &MouseEvent) -> Option<UIItemType> {
        self.ui_items
            .iter()
            .rev()
            .find(|item| item.hit_test(event.coords.x, event.coords.y))
            .map(|item| item.item_type.clone())
    }

    fn sidebar_hover_at(&self, event: &MouseEvent) -> Option<SidebarHover> {
        match self.sidebar_item_at(event) {
            Some(UIItemType::TabSidebar(tab_id)) => Some(SidebarHover::Tab(tab_id)),
            Some(UIItemType::TabSidebarGroup(group)) => Some(SidebarHover::Group(group)),
            _ => None,
        }
    }

    fn sidebar_drop_target(
        &self,
        source_id: TabId,
        event: &MouseEvent,
    ) -> Option<SidebarDropTarget> {
        let item = self
            .ui_items
            .iter()
            .rev()
            .find(|item| item.hit_test(event.coords.x, event.coords.y))?;
        let UIItemType::TabSidebar(target_id) = item.item_type else {
            return None;
        };
        if source_id == target_id {
            return None;
        }
        let source = self
            .tab_sidebar
            .entries
            .iter()
            .find(|entry| entry.tab_id == source_id)?;
        let target = self
            .tab_sidebar
            .entries
            .iter()
            .find(|entry| entry.tab_id == target_id)?;
        if !same_drop_scope(source, target) {
            return None;
        }
        Some(SidebarDropTarget {
            tab_id: target_id,
            before: event.coords.y < (item.y + item.height / 2) as isize,
        })
    }

    fn reorder_sidebar_tab(&mut self, source: TabId, target: SidebarDropTarget) -> bool {
        let Some(source_entry) = self
            .tab_sidebar
            .entries
            .iter()
            .find(|entry| entry.tab_id == source)
            .cloned()
        else {
            return false;
        };
        let Some(target_entry) = self
            .tab_sidebar
            .entries
            .iter()
            .find(|entry| entry.tab_id == target.tab_id)
            .cloned()
        else {
            return false;
        };
        if !same_drop_scope(&source_entry, &target_entry) {
            return false;
        }

        let mux = mux::Mux::get();
        if let Some(controller) = source_entry.controller_pane_id {
            let queued = mux.iter_domains().into_iter().any(|domain| {
                domain
                    .downcast_ref::<mux::tmux::TmuxDomain>()
                    .filter(|domain| domain.controller_pane_id() == controller)
                    .map(|domain| domain.reorder_tab(source, target.tab_id, target.before))
                    .unwrap_or(false)
            });
            if !queued {
                return false;
            }
        }

        let moved = mux
            .get_window_mut(self.mux_window_id)
            .map(|mut window| window.move_tab_by_id(source, target.tab_id, target.before))
            .unwrap_or(false);
        if moved {
            self.mark_tab_sidebar_dirty();
            self.emit_status_event();
        }
        moved
    }

    pub fn handle_tab_sidebar_mouse_event(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> bool {
        if !self.tab_sidebar_enabled {
            return false;
        }
        let inside =
            event.coords.x >= 0 && (event.coords.x as usize) < self.tab_sidebar_width_pixels();
        if !inside && self.tab_sidebar.drag.is_none() {
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
            MouseEventKind::Press(MousePress::Left) if inside => {
                match self.sidebar_item_at(event) {
                    Some(UIItemType::TabSidebar(tab_id)) => {
                        self.tab_sidebar.drag = Some(SidebarDrag {
                            tab_id,
                            start_x: event.coords.x,
                            start_y: event.coords.y,
                            active: false,
                            target: None,
                        });
                        invalidate = true;
                    }
                    Some(UIItemType::TabSidebarGroup(group)) => {
                        if self.tab_sidebar.compact {
                            self.expand_tab_sidebar_group(&group);
                            self.config_was_reloaded();
                        } else if !self.tab_sidebar.collapsed.insert(group.clone()) {
                            self.tab_sidebar.collapsed.remove(&group);
                        }
                        invalidate = true;
                    }
                    _ => {}
                }
            }
            MouseEventKind::Move => {
                if let Some(mut drag) = self.tab_sidebar.drag.take() {
                    let dx = event.coords.x - drag.start_x;
                    let dy = event.coords.y - drag.start_y;
                    drag.active |= dx * dx + dy * dy >= 36;
                    if drag.active {
                        if inside && event.coords.y < ROW_HEIGHT_PX as isize {
                            invalidate |= self.tab_sidebar_scroll(-1);
                        } else if inside
                            && event.coords.y
                                > self.dimensions.pixel_height.saturating_sub(ROW_HEIGHT_PX)
                                    as isize
                        {
                            invalidate |= self.tab_sidebar_scroll(1);
                        }
                        let target = self.sidebar_drop_target(drag.tab_id, event);
                        invalidate |= drag.target != target;
                        drag.target = target;
                    }
                    self.tab_sidebar.drag = Some(drag);
                }
                let hovered = if inside {
                    self.sidebar_hover_at(event)
                } else {
                    None
                };
                invalidate |= self.tab_sidebar.hovered != hovered;
                self.tab_sidebar.hovered = hovered;
            }
            MouseEventKind::Release(MousePress::Left) => {
                if let Some(drag) = self.tab_sidebar.drag.take() {
                    if drag.active {
                        if let Some(target) = drag.target {
                            self.reorder_sidebar_tab(drag.tab_id, target);
                        }
                    } else {
                        self.activate_sidebar_tab(drag.tab_id);
                    }
                    invalidate = true;
                }
            }
            _ => {}
        }

        let interactive =
            self.tab_sidebar.drag.is_some() || matches!(self.tab_sidebar.hovered, Some(_));
        context.set_cursor(Some(if interactive {
            MouseCursor::Hand
        } else {
            MouseCursor::Arrow
        }));
        if invalidate {
            context.invalidate();
        }
        true
    }
}

#[derive(Clone, Debug)]
pub enum SidebarRow {
    Group(SidebarGroup),
    Tab(SidebarEntry),
}

pub fn ui_items_for_rows(
    rows: &[SidebarRow],
    scroll_rows: usize,
    top: usize,
    row_height: usize,
    width: usize,
    height: usize,
) -> Vec<UIItem> {
    rows.iter()
        .skip(scroll_rows)
        .enumerate()
        .take(height.div_ceil(row_height))
        .map(|(row, item)| UIItem {
            x: 0,
            y: top + row * row_height,
            width,
            height: row_height,
            item_type: match item {
                SidebarRow::Tab(entry) => UIItemType::TabSidebar(entry.tab_id),
                SidebarRow::Group(group) => UIItemType::TabSidebarGroup(group.key.clone()),
            },
        })
        .collect()
}

pub fn sidebar_rows(sidebar: &TabSidebar) -> Vec<SidebarRow> {
    let group_state = sidebar.entries.iter().flat_map(|entry| {
        entry_groups(entry).into_iter().map(move |group| (group.key, entry.active, entry.urgency))
    }).fold(HashMap::new(), |mut states, (key, active, urgency)| {
        let state = states.entry(key).or_insert((false, 0));
        state.0 |= active;
        state.1 = state.1.max(urgency);
        states
    });
    if sidebar.compact {
        let mut groups = Vec::new();
        for entry in &sidebar.entries {
            let mut group = entry_groups(entry).into_iter().next().unwrap();
            if !groups.iter().any(|existing: &SidebarGroup| existing.key == group.key) {
                let (active, urgency) = group_state[&group.key];
                group.active = active;
                group.urgency = urgency;
                groups.push(group);
            }
        }
        return groups
            .iter()
            .map(|group| SidebarRow::Group(group.clone()))
            .collect();
    }
    let mut rows = Vec::new();
    let mut previous_groups: Vec<SidebarGroup> = Vec::new();
    for entry in &sidebar.entries {
        let groups = entry_groups(entry);
        let common = previous_groups.iter().zip(&groups)
            .take_while(|(previous, group)| previous.key == group.key)
            .count();
        let mut hidden = groups.iter().take(common).any(|group| sidebar.collapsed.contains(&group.key));
        for group in groups.iter().skip(common) {
            if !hidden {
                let mut group = group.clone();
                let (active, urgency) = group_state[&group.key];
                group.active = active;
                group.urgency = urgency;
                rows.push(SidebarRow::Group(group));
            }
            hidden |= sidebar.collapsed.contains(&group.key);
        }
        if !hidden {
            rows.push(SidebarRow::Tab(entry.clone()));
        }
        previous_groups = groups;
    }
    rows
}

fn entry_groups(entry: &SidebarEntry) -> Vec<SidebarGroup> {
    let groups = if entry.groups.is_empty() {
        vec![SidebarGroup {
            key: "local".into(),
            label: "THIS MAC".into(),
            ..Default::default()
        }]
    } else {
        entry.groups.clone()
    };
    let mut path = String::new();
    groups.into_iter().enumerate().map(|(depth, mut group)| {
        if !path.is_empty() {
            path.push('\u{1f}');
        }
        path.push_str(&group.key);
        group.key = path.clone();
        group.depth = depth;
        group
    }).collect()
}

fn same_drop_scope(source: &SidebarEntry, target: &SidebarEntry) -> bool {
    source.controller_pane_id == target.controller_pane_id
        && same_group_path(&source.groups, &target.groups)
}

fn same_group_path(source: &[SidebarGroup], target: &[SidebarGroup]) -> bool {
    match (source.is_empty(), target.is_empty()) {
        (true, true) => true,
        (true, false) => target.len() == 1 && target[0].key == "@local",
        (false, true) => source.len() == 1 && source[0].key == "@local",
        (false, false) => {
            source.len() == target.len()
                && source
                    .iter()
                    .zip(target)
                    .all(|(source, target)| source.key == target.key)
        }
    }
}

fn native_entries(tabs: &[TabInformation]) -> Vec<SidebarEntry> {
    tabs.iter()
        .map(|tab| SidebarEntry {
            tab_id: tab.tab_id,
            controller_pane_id: tab.panes.iter().find_map(|pane| pane.controller_pane_id),
            title: if tab.tab_title.is_empty() {
                tab.active_pane
                    .as_ref()
                    .map(|pane| pane.title.clone())
                    .unwrap_or_default()
            } else {
                tab.tab_title.clone()
            },
            active: tab.is_active,
            ..Default::default()
        })
        .collect()
}

struct CallbackResult {
    entries: HashMap<TabId, SidebarEntry>,
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

fn max_scroll_rows(row_count: usize, height: usize, row_height: usize) -> usize {
    row_count.saturating_sub(height.div_ceil(row_height))
}

fn callback_entries(tabs: &[TabInformation]) -> anyhow::Result<CallbackResult> {
    config::run_immediate_with_lua_config(|lua| {
        let Some(lua) = lua else {
            return Ok(CallbackResult { entries: HashMap::new(), refresh_after: None });
        };
        let tabs = lua.create_sequence_from(tabs.iter().cloned())?;
        let value = config::lua::emit_sync_callback(
            &lua,
            ("format-tab-sidebar".to_string(), tabs),
        )?;
        decode_callback(&lua, value)
    })
}

fn decode_callback(lua: &mlua::Lua, value: Value) -> anyhow::Result<CallbackResult> {
    let Value::Table(result) = value else {
        return Ok(CallbackResult { entries: HashMap::new(), refresh_after: None });
    };
    let refresh_after = match result.get::<_, Value>("refresh_after_ms")? {
        Value::Nil => None,
        Value::Integer(ms) if ms >= 0 => Some(Duration::from_millis((ms as u64).max(100))),
        Value::Number(ms) if ms.is_finite() && ms >= 0.0 => Some(Duration::from_millis((ms as u64).max(100))),
        _ => anyhow::bail!("format-tab-sidebar refresh_after_ms must be a non-negative number"),
    };
    let entries = match result.get::<_, Value>("entries")? {
        Value::Nil => result,
        Value::Table(entries) => entries,
        _ => anyhow::bail!("format-tab-sidebar must return {{ entries = {{...}} }}")
    };
    let mut decoded = HashMap::new();
    for value in entries.sequence_values::<Value>() {
        let Value::Table(entry) = value? else {
            anyhow::bail!("format-tab-sidebar entries must be tables");
        };
        let tab_id = entry.get::<_, TabId>("tab_id")?;
        let title = entry.get::<_, Option<String>>("title")?.unwrap_or_default();
        let right = entry.get::<_, Option<String>>("right")?.unwrap_or_default();
        let urgency = entry.get::<_, Option<u8>>("urgency")?.unwrap_or(0).min(2);
        let (status_glyph, status_color) = match entry.get::<_, Value>("status")? {
            Value::Table(status) => (
                status.get::<_, Option<String>>("glyph")?.unwrap_or_default(),
                status.get::<_, Option<String>>("color")?,
            ),
            Value::Nil => (String::new(), None),
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
                    status_glyph,
                    status_color,
                    urgency,
                    groups,
                    active: false,
                },
            )
            .is_some()
        {
            anyhow::bail!("format-tab-sidebar returned duplicate tab_id {tab_id}");
        }
    }
    Ok(CallbackResult { entries: decoded, refresh_after })
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
                    label: group.get::<_, Option<String>>("label")?.unwrap_or_else(|| key.clone()),
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

    fn entry(tab_id: TabId, groups: &[&str]) -> SidebarEntry {
        SidebarEntry {
            tab_id,
            title: format!("tab-{tab_id}"),
            groups: groups
                .iter()
                .map(|label| SidebarGroup {
                    key: (*label).into(),
                    label: (*label).into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

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
    fn rows_keep_nested_groups_and_hide_collapsed_descendants() {
        let mut sidebar = TabSidebar {
            entries: vec![
                entry(1, &["remote", "project-a"]),
                entry(2, &["remote", "project-b"]),
            ],
            ..Default::default()
        };
        let labels = sidebar_rows(&sidebar)
            .into_iter()
            .map(|row| match row {
                SidebarRow::Group(group) => group.label,
                SidebarRow::Tab(tab) => tab.title,
            })
            .collect::<Vec<_>>();
        assert_eq!(labels, ["remote", "project-a", "tab-1", "project-b", "tab-2"]);

        sidebar.collapsed.insert("remote\u{1f}project-a".into());
        let labels = sidebar_rows(&sidebar)
            .into_iter()
            .map(|row| match row {
                SidebarRow::Group(group) => group.label,
                SidebarRow::Tab(tab) => tab.title,
            })
            .collect::<Vec<_>>();
        assert_eq!(labels, ["remote", "project-a", "project-b", "tab-2"]);
    }

    #[test]
    fn compact_rows_roll_up_state_and_ui_rows_match_shared_height() {
        let mut first = entry(1, &["remote", "project-a"]);
        first.active = true;
        let mut second = entry(2, &["remote", "project-b"]);
        second.urgency = 2;
        let sidebar = TabSidebar {
            entries: vec![first, second],
            compact: true,
            ..Default::default()
        };
        let rows = sidebar_rows(&sidebar);
        assert_eq!(rows.len(), 1);
        let SidebarRow::Group(group) = &rows[0] else {
            panic!("expected group")
        };
        assert!(group.active);
        assert_eq!(group.urgency, 2);

        let items = ui_items_for_rows(&rows, 0, 0, ROW_HEIGHT_PX, 120, ROW_HEIGHT_PX);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].y, 0);
        assert_eq!(items[0].height, ROW_HEIGHT_PX);
        assert_eq!(max_scroll_rows(5, ROW_HEIGHT_PX, ROW_HEIGHT_PX), 4);
    }

    #[test]
    fn drop_scope_requires_the_same_leaf_group_and_controller() {
        let mut source = entry(1, &["remote", "project-a"]);
        source.controller_pane_id = Some(10);
        let mut target = entry(2, &["remote", "project-a"]);
        target.controller_pane_id = Some(10);
        assert!(same_drop_scope(&source, &target));

        target.controller_pane_id = Some(11);
        assert!(!same_drop_scope(&source, &target));

        target.controller_pane_id = Some(10);
        target.groups[1].key = "project-b".to_string();
        assert!(!same_drop_scope(&source, &target));

        target.groups[1].key = "project-a".to_string();
        target.groups[0].key = "other-remote".to_string();
        assert!(!same_drop_scope(&source, &target));

        let implicit_local = entry(3, &[]);
        let explicit_local = entry(4, &["@local"]);
        assert!(same_drop_scope(&implicit_local, &explicit_local));
    }
}
