//! A non-PTY pane whose content is rendered by an external GUI engine
//! (egui; see GUI_INTEGRATION_PLAN.md) rather than the terminal cell grid.
//!
//! `GuiPane` satisfies the `Pane` trait so it can live in a tab/split
//! like any other pane, but it owns no process and has no real scrollback:
//! the cell-grid surface methods return blank lines, and the actual pixels
//! are produced by the egui render pass wired up in `wezterm-gui`.
//!
//! The widget model here is a plain-Rust `UiNode` tree + `UiTheme`/`UiStyle`,
//! intentionally free of any egui types so the lower `mux` layer need not
//! depend on the GUI crate. The render crate translates the tree into live
//! egui calls each frame.

use crate::domain::DomainId;
use crate::pane::{
    impl_for_each_logical_line_via_get_logical_lines, impl_get_logical_lines_via_get_lines,
    impl_with_lines_via_get_lines, CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId,
    WithPaneLines,
};
use crate::renderable::{RenderableDimensions, StableCursorPosition};
use async_trait::async_trait;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use std::collections::HashSet;
use std::io::{Result as IoResult, Write};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo, SEQ_ZERO};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

/// Discards everything written to it; a GuiPane has no PTY to feed.
struct Sink(Vec<u8>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

/// Linear RGBA, each channel in 0.0..=1.0.
pub type Color = [f32; 4];

fn rgb(r: u8, g: u8, b: u8) -> Color {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

/// Per-widget styling overrides. `None` means "inherit the theme".
/// Every field is optional so a Lua caller can override only what they need;
/// the renderer merges this onto the active theme when drawing the node.
#[derive(Clone, Debug, Default)]
pub struct UiStyle {
    pub color: Option<Color>,
    pub background: Option<Color>,
    pub accent: Option<bool>,
    pub size: Option<f32>,
    pub strong: Option<bool>,
    pub italics: Option<bool>,
    pub monospace: Option<bool>,
    pub code: Option<bool>,
    pub underline: Option<bool>,
    pub strikethrough: Option<bool>,
    pub rounding: Option<f32>,
    pub fill: Option<Color>,
    pub stroke: Option<(Color, f32)>,
    pub margin: Option<f32>,
    pub spacing: Option<f32>,
    pub expand: Option<bool>,
    pub tooltip: Option<String>,
}

/// Named theme preset resolved to a concrete palette the renderer applies to
/// egui.
///
/// ponytail: presets are hardcoded RGB triples, not parsed from a config
/// file; the set is small and fixed. Add a preset by extending `for_name`.
#[derive(Clone, Debug)]
pub struct UiTheme {
    pub name: String,
    pub dark: bool,
    pub background: Color,
    pub foreground: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub dim: Color,
    pub rounding: f32,
    pub spacing: f32,
    pub font_size: f32,
}

impl Default for UiTheme {
    fn default() -> Self {
        Self::for_name("dark")
    }
}

impl UiTheme {
    /// Resolve a preset by name. Unknown names fall back to `dark` (never
    /// `None`) so `ui:theme("bogus")` cannot panic the render loop.
    pub fn for_name(name: &str) -> Self {
        let (dark, background, foreground, accent, success, warning, error, dim) = match name {
            "dark" => (
                true,
                rgb(0x12, 0x12, 0x16),
                rgb(0xd4, 0xd4, 0xd8),
                rgb(0x60, 0x8c, 0xff),
                rgb(0x39, 0xa0, 0xff),
                rgb(0xe0, 0xaf, 0x68),
                rgb(0xf7, 0x76, 0x8e),
                rgb(0x6c, 0x6f, 0x7c),
            ),
            "light" => (
                false,
                rgb(0xfa, 0xfa, 0xfa),
                rgb(0x2e, 0x2e, 0x33),
                rgb(0x2e, 0x6f, 0xdf),
                rgb(0x2e, 0x8b, 0x57),
                rgb(0xb8, 0x86, 0x0b),
                rgb(0xc0, 0x39, 0x2b),
                rgb(0x9a, 0x9a, 0xa0),
            ),
            "catppuccin-mocha" => (
                true,
                rgb(0x1e, 0x1e, 0x2e),
                rgb(0xcd, 0xd6, 0xf4),
                rgb(0xca, 0x9e, 0xe6),
                rgb(0xa6, 0xe3, 0xa1),
                rgb(0xf9, 0xe2, 0xaf),
                rgb(0xf3, 0x8b, 0xa8),
                rgb(0x66, 0x6c, 0x8a),
            ),
            "catppuccin-latte" => (
                false,
                rgb(0xef, 0xf1, 0xf5),
                rgb(0x4c, 0x4f, 0x69),
                rgb(0x88, 0x39, 0xef),
                rgb(0x40, 0xa0, 0x2b),
                rgb(0xdf, 0x8e, 0x1d),
                rgb(0xd2, 0x0f, 0x39),
                rgb(0x9c, 0xa0, 0xb0),
            ),
            "nord" => (
                true,
                rgb(0x2e, 0x34, 0x40),
                rgb(0xd8, 0xde, 0xe9),
                rgb(0x88, 0xc0, 0xd0),
                rgb(0xa3, 0xbe, 0x8c),
                rgb(0xeb, 0xcb, 0x8b),
                rgb(0xbf, 0x61, 0x6a),
                rgb(0x4c, 0x56, 0x6a),
            ),
            "dracula" => (
                true,
                rgb(0x28, 0x2a, 0x36),
                rgb(0xf8, 0xf8, 0xf2),
                rgb(0xbd, 0x93, 0xf9),
                rgb(0x50, 0xfa, 0x7b),
                rgb(0xf1, 0xfa, 0x8c),
                rgb(0xff, 0x55, 0x55),
                rgb(0x62, 0x72, 0xa4),
            ),
            "gruvbox" => (
                true,
                rgb(0x28, 0x28, 0x28),
                rgb(0xeb, 0xdb, 0xb2),
                rgb(0xfe, 0x80, 0x19),
                rgb(0xb8, 0xbb, 0x26),
                rgb(0xfa, 0xbd, 0x2f),
                rgb(0xfb, 0x49, 0x34),
                rgb(0x92, 0x83, 0x74),
            ),
            "tokyo-night" => (
                true,
                rgb(0x1a, 0x1b, 0x26),
                rgb(0xa9, 0xb1, 0xd6),
                rgb(0x7a, 0xa2, 0xf7),
                rgb(0x9e, 0xce, 0x6a),
                rgb(0xe0, 0xaf, 0x68),
                rgb(0xf7, 0x76, 0x8e),
                rgb(0x56, 0x5f, 0x89),
            ),
            "solarized-dark" => (
                true,
                rgb(0x00, 0x2b, 0x36),
                rgb(0x93, 0xa1, 0xa1),
                rgb(0x26, 0x8b, 0xd2),
                rgb(0x85, 0x99, 0x00),
                rgb(0xb5, 0x89, 0x00),
                rgb(0xdc, 0x32, 0x2f),
                rgb(0x58, 0x6e, 0x75),
            ),
            _ => return Self::for_name("dark"),
        };
        Self {
            name: name.to_string(),
            dark,
            background,
            foreground,
            accent,
            success,
            warning,
            error,
            dim,
            rounding: 6.0,
            spacing: 8.0,
            font_size: 14.0,
        }
    }
}

/// One drawable element produced by the Lua `render-gui-pane` callback.
/// Container variants own their children inline; the tree is fully owned so
/// the render crate can traverse it with no locking.
#[derive(Clone, Debug)]
pub enum UiNode {
    /// Status-aware labelled value, the dashboard primitive.
    Metric {
        label: String,
        value: String,
        status: Option<String>,
        style: UiStyle,
    },
    Label {
        text: String,
        style: UiStyle,
    },
    Heading {
        text: String,
        size: Option<f32>,
        style: UiStyle,
    },
    Button {
        text: String,
        /// Stable id used to report clicks back to Lua next frame.
        id: String,
        style: UiStyle,
    },
    Hyperlink {
        text: String,
        url: String,
        style: UiStyle,
    },
    Checkbox {
        id: String,
        label: String,
        checked: bool,
        style: UiStyle,
    },
    Radio {
        id: String,
        label: String,
        selected: bool,
        style: UiStyle,
    },
    Toggle {
        id: String,
        label: String,
        on: bool,
        style: UiStyle,
    },
    Slider {
        id: String,
        value: f64,
        min: f64,
        max: f64,
        step: Option<f64>,
        label: Option<String>,
        style: UiStyle,
    },
    ProgressBar {
        fraction: f32,
        animated: bool,
        style: UiStyle,
    },
    Spinner {
        style: UiStyle,
    },
    Separator {
        style: UiStyle,
    },
    Spacing {
        amount: f32,
    },
    /// Line/sparkline plot of a value series.
    Sparkline {
        values: Vec<f32>,
        color: Option<Color>,
        fill: bool,
        height: Option<f32>,
        style: UiStyle,
    },
    /// A raster image (decoded bytes, e.g. PNG/JPEG). `id` is the cache key;
    /// change it when the content changes. Optional width/height are in egui
    /// points.
    Image {
        id: String,
        bytes: Arc<[u8]>,
        width: Option<f32>,
        height: Option<f32>,
    },
    /// Container: framed card with optional title and translucent "glass".
    Card {
        title: Option<String>,
        glass: bool,
        style: UiStyle,
        children: Vec<UiNode>,
    },
    /// Collapsible section.
    CollapsingHeader {
        title: String,
        default_open: bool,
        style: UiStyle,
        children: Vec<UiNode>,
    },
    /// Generic framed region.
    Frame {
        style: UiStyle,
        children: Vec<UiNode>,
    },
    /// Lay children out left-to-right.
    Horizontal {
        style: UiStyle,
        children: Vec<UiNode>,
    },
    /// Lay children out top-to-bottom.
    Vertical {
        style: UiStyle,
        children: Vec<UiNode>,
    },
    /// Distribute children across `n` equal columns.
    Columns {
        n: usize,
        style: UiStyle,
        children: Vec<UiNode>,
    },
    /// Aligned grid (labels left, values right, etc.).
    Grid {
        id: String,
        style: UiStyle,
        children: Vec<UiNode>,
    },
}

impl UiNode {
    /// Children of a container variant, or empty for leaves.
    pub fn children(&self) -> &[UiNode] {
        match self {
            UiNode::Card { children, .. }
            | UiNode::CollapsingHeader { children, .. }
            | UiNode::Frame { children, .. }
            | UiNode::Horizontal { children, .. }
            | UiNode::Vertical { children, .. }
            | UiNode::Columns { children, .. }
            | UiNode::Grid { children, .. } => children,
            _ => &[],
        }
    }
}

/// A pane that renders an arbitrary GUI surface instead of terminal cells.
pub struct GuiPane {
    pane_id: PaneId,
    title: Mutex<String>,
    dims: Mutex<RenderableDimensions>,
    palette: Mutex<ColorPalette>,
    seqno: AtomicU64,
    sink: Mutex<Sink>,
    nodes: Mutex<Vec<UiNode>>,
    theme: Mutex<UiTheme>,
    /// Widget ids egui reported as clicked/changed during the last render
    /// pass, surfaced to the next `render-gui-pane` fire via `clicked()`.
    /// ponytail: one-frame latency is inherent to deferred immediate-mode.
    events: Mutex<HashSet<String>>,
    /// Monotonic generation bumped each time a `render-gui-pane` fire is
    /// scheduled. The async flush checks it so a slow, older handler can't
    /// overwrite a newer widget tree (out-of-order flush guard).
    generation: AtomicU64,
}

impl GuiPane {
    pub fn new(title: impl Into<String>, dims: RenderableDimensions) -> Arc<Self> {
        Arc::new(Self {
            pane_id: crate::pane::alloc_pane_id(),
            title: Mutex::new(title.into()),
            dims: Mutex::new(dims),
            palette: Mutex::new(ColorPalette::default()),
            seqno: AtomicU64::new(1),
            sink: Mutex::new(Sink(Vec::new())),
            nodes: Mutex::new(Vec::new()),
            theme: Mutex::new(UiTheme::default()),
            events: Mutex::new(HashSet::new()),
            generation: AtomicU64::new(0),
        })
    }

    /// Bump the change sequence so renderers know to repaint.
    pub fn bump(&self) {
        self.seqno.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of the widget tree to draw this frame.
    pub fn nodes(&self) -> Vec<UiNode> {
        self.nodes.lock().clone()
    }

    /// Replace the widget tree (called from the LuaUi context each frame).
    pub fn set_nodes(&self, nodes: Vec<UiNode>) {
        *self.nodes.lock() = nodes;
        self.bump();
    }

    pub fn theme(&self) -> UiTheme {
        self.theme.lock().clone()
    }

    pub fn set_theme(&self, theme: UiTheme) {
        *self.theme.lock() = theme;
        self.bump();
    }

    /// Drain widget ids clicked/activated across egui render passes since the
    /// last call. The `render-gui-pane` fire consumes these to react to
    /// interaction; each click is reported exactly once.
    pub fn drain_clicked(&self) -> HashSet<String> {
        std::mem::take(&mut *self.events.lock())
    }

    /// Accumulate interaction events collected by an egui render pass.
    /// Accumulates (extends) rather than overwriting so clicks survive the
    /// ~100 ms between refresh ticks even though render runs every frame.
    pub fn set_events(&self, events: HashSet<String>) {
        self.events.lock().extend(events);
    }

    /// Bump and return the generation; captured by an async flush to detect
    /// that a newer refresh superseded it.
    pub fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

#[async_trait(?Send)]
impl Pane for GuiPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        // No text cursor; the GUI engine draws its own focus indicator.
        StableCursorPosition::default()
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.seqno.load(Ordering::Relaxed) as SequenceNo
    }

    fn get_metadata(&self) -> wezterm_dynamic::Value {
        wezterm_dynamic::Value::Null
    }

    fn get_changed_since(
        &self,
        _lines: Range<StableRowIndex>,
        _seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        // The cell grid is always blank, so it never reports dirty rows.
        RangeSet::new()
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let count = (lines.end - lines.start).max(0) as usize;
        (lines.start, vec![Line::new(SEQ_ZERO); count])
    }

    fn with_lines_mut(
        &self,
        lines: Range<StableRowIndex>,
        with_lines: &mut dyn WithPaneLines,
    ) {
        impl_with_lines_via_get_lines(self, lines, with_lines)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.dims.lock().clone()
    }

    fn get_title(&self) -> String {
        self.title.lock().clone()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        let _ = self.sink.lock().write_all(text.as_bytes());
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.sink.lock(), |sink| sink as &mut dyn std::io::Write)
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        let mut dims = self.dims.lock();
        dims.cols = size.cols as usize;
        dims.viewport_rows = size.rows as usize;
        dims.scrollback_rows = dims.viewport_rows;
        self.bump();
        Ok(())
    }

    fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
        Ok(())
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn palette(&self) -> ColorPalette {
        self.palette.lock().clone()
    }

    fn domain_id(&self) -> DomainId {
        // GUI panes belong to no spawn domain; 0 is the local sentinel.
        0
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        KeyboardEncoding::Xterm
    }

    fn is_mouse_grabbed(&self) -> bool {
        false
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        None
    }

    fn can_close_without_prompting(&self, _reason: crate::pane::CloseReason) -> bool {
        // No process to lose; safe to close immediately.
        true
    }

    async fn search(
        &self,
        _pattern: crate::pane::Pattern,
        _range: Range<StableRowIndex>,
        _limit: Option<u32>,
    ) -> anyhow::Result<Vec<crate::pane::SearchResult>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn theme_presets_resolve() {
        assert!(UiTheme::for_name("catppuccin-mocha").dark);
        assert!(!UiTheme::for_name("light").dark);
        assert!(UiTheme::for_name("dracula").accent != UiTheme::default().accent);
        // Unknown name falls back to dark, not panic.
        assert!(UiTheme::for_name("nope").dark);
    }

    #[test]
    fn node_children_only_for_containers() {
        let card = UiNode::Card {
            title: None,
            glass: false,
            style: UiStyle::default(),
            children: vec![UiNode::Label {
                text: "x".into(),
                style: UiStyle::default(),
            }],
        };
        assert_eq!(card.children().len(), 1);
        assert!(UiNode::Separator {
            style: UiStyle::default()
        }
        .children()
        .is_empty());
    }
}
