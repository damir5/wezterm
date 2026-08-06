# WezTerm Native GUI Engine Integration & Dual-Instance Activity Plan

This document outlines the architectural plan for integrating a Rust-based GUI framework (`egui` / `GPUI`) into WezTerm, exporting GUI rendering capabilities to Lua, and configuring dual-instance (System vs. Experimental) WezTerm activity tracking.

---

## 1. Architectural Architecture & Design Decisions

### 1.1 GUI Framework Selection: `egui` vs `GPUI`

| Metric | `egui` (Recommended) | `GPUI` |
| :--- | :--- | :--- |
| **Paradigm** | Pure Immediate-Mode | Hybrid Immediate / Reactive |
| **Window Ownership** | None (Integrates into existing `winit` loop) | High (Expects `gpui::App` / `WindowContext`) |
| **Render Integration** | Single WebGPU/OpenGL render pass over `wezterm-gui` | Offscreen GPU texture compositing |
| **Lua Bindings (`mlua`)** | Procedural frame calls (`ui:button()`) | Reactive Virtual DOM / Declarative tree |
| **Est. Effort** | 3–4 Weeks | 8–12 Weeks |

### 1.2 The `GuiPane` & Render Pipeline

To allow dynamic dashboards alongside standard PTY panes, WezTerm requires a new `GuiPane` struct implementing the `Pane` trait:

1. **`wezterm-gui/src/pane/guipane.rs`**: Implements `Pane` without binding to a PTY process.
2. **Render Pass**: During `TermWindow::paint()`, `wezterm-gui` delegates rendering of `GuiPane` bounds to the selected GUI engine pass (e.g., `egui_wgpu` pass) instead of rasterizing font glyph cells.
3. **Event Interception**: `winit` mouse and keyboard events targeting a `GuiPane` are intercepted and forwarded directly to the GUI context rather than sent over a PTY.

---

## 2. Lua API Design for Dynamic Dashboards

Lua scripts will define custom dashboard panes using a high-level component API:

```lua
wezterm.on("render-gui-pane", function(pane, ui)
    ui:theme("catppuccin-mocha")
    
    ui:card({ title = "Agent Activity", glass = true }, function(card)
        card:metric({ label = "Active Agents", value = "3", status = "running" })
        card:sparkline({ data = { 10, 20, 15, 30, 45, 42 }, color = "#39ff14" })
    end)
end)
```

---

## 3. Dual-Instance Activity Tracking (System vs. Experimental)

Both system-installed `/Applications/WezTerm.app` and custom experimental builds in `~/dev/vendor/wezterm` can run concurrently and share your existing activity/tray tracking scripts (`attention.lua`, `tray.lua`).

### 3.1 Setup & Shared State Architecture

```
┌─────────────────────────────────┐      ┌─────────────────────────────────┐
│     System WezTerm.app          │      │   Experimental WezTerm Build    │
│  (/Applications/WezTerm.app)    │      │  (~/dev/vendor/wezterm/target)  │
└────────────────┬────────────────┘      └────────────────┬────────────────┘
                 │                                        │
                 │   wezterm.on('user-var-changed')       │
                 │   wezterm.on('update-status')          │
                 │                                        │
                 ▼                                        ▼
┌──────────────────────────────────────────────────────────────────────────┐
│                      Shared State & Lock Mechanisms                      │
│                                                                          │
│ • State Snapshot:  ~/.local/share/wezterm/tray.json (Atomic Write)       │
│ • Tray Lock File:  ~/.local/share/wezterm/tray.lock (Flock Singleton)   │
│ • Config Shared:   ~/.config/wezterm/wezterm.lua or WEZTERM_CONFIG_FILE  │
└──────────────────────────────────────────────────────────────────────────┘
```

### 3.2 Key Configuration Rules for Dual Execution

1. **Shared Configuration:** Point both binaries to `~/.config/wezterm/wezterm.lua` (or pass custom `--config-file` when launching experimental builds).
2. **Unique Socket Isolation (If Multiplexing Separately):**
   * Default: Both GUIs can run independently.
   * If running separate mux daemons, set `WEZTERM_UNIX_SOCKET=/tmp/wezterm-exp.sock` for the experimental instance to prevent unix domain socket collisions.
3. **Atomic File Locks:** Your existing `tray.lua` uses atomic JSON writes (`tray.atomic_write`) and process locking (`bin/agent-tray` flocking). This ensures neither instance corrupts the tray state when updates fire simultaneously.

---

## 4. Implementation Roadmap

- [x] **Step 1: `GuiPane` Stub** → verify: `wezterm` compiles with `GuiPane` implementing `Pane` trait.
  - Done. `mux/src/guipane.rs` implements the full `mux::pane::Pane` trait (required methods implemented, others use trait defaults) and carries the widget model: an owned `UiNode` tree + `UiTheme`/`UiStyle` (all widgets, containers, theming). `mux` has no egui dependency; the tree is plain Rust so the lower layer stays decoupled. `cargo build -p mux` + `cargo test -p mux --lib guipane` pass.
- [x] **Step 2: Render Pass Integration** → verify: `GuiPane` draws via egui inside a split pane.
  - Done with real `egui` 0.32 + `egui-wgpu` 0.32 (both pin `wgpu ^25`, matching the workspace). `wezterm-gui/src/termwindow/render/pane.rs::paint_pane` downcasts a `GuiPane` to `paint_gui_pane`, which fills the pane background via the native quad renderer and stashes `(rect, pane)` for the egui pass. `wezterm-gui/src/termwindow/render/draw.rs::call_draw_webgpu` runs a second `LoadOp::Load` render pass after WezTerm's quads: it lazily initializes an `egui::Context` + `egui_wgpu::Renderer`, replays each pane's `UiNode` tree through `guipane_ui::render_nodes`, tessellates, and records the egui paint jobs into the surface view. The Glium (OpenGL) backend is unsupported for GuiPane (egui-wgpu is WebGpu-only).
- [x] **Step 3: `mlua` Exposing `LuaUi` Context** → verify: Lua script callback builds a widget tree inside `GuiPane`.
  - Done. `lua-api-crates/mux/src/gui.rs` defines `LuaUi` with the full immediate-mode surface: leaves (`metric`, `label`, `heading`, `button`, `hyperlink`, `checkbox`, `toggle`, `radio`, `slider`, `progress_bar`, `spinner`, `separator`, `spacing`, `sparkline`), containers that take a Lua callback and nest via a child-list stack (`card`, `collapsing_header`, `frame`, `horizontal`, `vertical`, `columns`, `grid`), theming presets (`dark`, `light`, `catppuccin-mocha/latte`, `nord`, `dracula`, `gruvbox`, `tokyo-night`, `solarized-dark`), and per-widget `ui:style({...})` overrides. `wezterm.gui.new_ui()` constructs one. Deferred interaction: ids egui reports as clicked are stored on the `GuiPane` and surfaced to the next fire via `ui:clicked(id)`. Verified by `cargo test -p mux-lua --lib gui` (a nested themed dashboard + style-persistence tests).
- [x] **Step 4: Dual-Instance Activity Verification** → verify: Both `WezTerm.app` and `target/release/wezterm` update `tray.json` atomically.
  - Confirmed by inspection of `~/.config/wezterm/tray.lua` + `wezterm.lua`. Both instances load the same config and call `tray.atomic_write(snapshot_path, json)`, which writes a temp file then `os.rename` (atomic on POSIX). Concurrent writers cannot corrupt the snapshot: each produces a complete JSON and the rename is atomic, so the file is always valid (last-writer-wins). Socket isolation for separate mux daemons: set `WEZTERM_UNIX_SOCKET=/tmp/wezterm-exp.sock` on the experimental instance.

### Event wiring & spawn path

- `wezterm.on("render-gui-pane", function(window, pane, ui))` is emitted by `TermWindow::refresh_gui_panes` (hooked into the periodic `update_title_post_status` path, throttled to ~10 Hz). Each fire builds a fresh `LuaUi`, runs the handler, and flushes the tree onto the pane via `mux_lua::flush_ui_to_pane`. A per-pane generation counter guards the async flush so a slow, older handler can't overwrite a fresher tree.
- `wezterm.gui.split_dashboard({ title=..., size=50 })` (`mux_lua::split_dashboard`) creates a `GuiPane` and split-inserts it into the active tab so a dashboard can actually appear.
- Example handler:
  ```lua
  wezterm.on("render-gui-pane", function(window, pane, ui)
    ui:theme("catppuccin-mocha")
    ui:card({ title = "Agent Activity", glass = true }, function(ui)
      ui:metric({ label = "Active", value = "3", status = "running" })
      ui:sparkline({ data = { 10, 20, 15, 30, 45 }, color = "#39ff14", fill = true })
      ui:slider({ id = "workers", value = 4, min = 1, max = 16 })
      ui:image({ path = "/opt/logo.png", width = 24, height = 24 })
      if ui:clicked("save") then print("saved!") end
    end)
  end)
  ```

### Interaction, fonts, and images (implemented)

- **Input forwarding**: `TermWindow` forwards winit mouse (move/press/release/wheel) and keyboard events into the egui `Context`. Mouse events are hit-tested against `gui_render_list` and converted from physical pixels to egui points; key events forward only while a `GuiPane` is the active pane (terminal input is untouched otherwise). `Response::clicked()`/`changed()` fire; interactions accumulate on the pane via `set_events` and surface to Lua next fire via `ui:clicked(id)`.
- **Nerd Font / typography**: JetBrainsMono and SymbolsNerdFontMono are registered into the egui `FontDefinitions` (embedded at compile time via `include_bytes!`), so powerline and Nerd Font glyphs render in dashboards.
- **Images**: `ui:image({ path=..., bytes=..., id=..., width=, height= })` decodes PNG/JPEG/etc. (via the `image` crate) into an egui texture cached in context temp data, keyed by `id` so frames don't re-decode/re-upload.

### Known limitations

- **Glium (OpenGL) backend**: GuiPanes render only the pane background under OpenGL (egui-wgpu is WebGpu-only). WebGpu is WezTerm's default on macOS and most Linux setups.
- **Radio-group state**: each `radio` node persists only its own selected flag; selecting one does not clear siblings in the same logical group (needs cross-node coordination). Checkbox/toggle/slider value persistence and readback (`ui:value(id)`) are handled.
- **Interactive click-test**: input forwarding is wired (`forward_mouse_to_egui` / `forward_key_to_egui`) and on the correct dispatch path, but was not exercised end-to-end because synthesizing clicks into wezterm requires macOS Accessibility permission. Render of the full widget set is verified under WebGpu.

---

## 5. Next phase: dashboard component coverage & gaps

The egui integration is functional. The following maps the intended dashboard
elements onto the existing widget set, then lists the gaps the design still
needs from the fork.

### Covered by the existing widget set

| Element | Node |
| :--- | :--- |
| Machine / dir group | `collapsing_header` |
| Agent row | `frame` + `horizontal` |
| Urgency bar & tint | `style{fill, stroke, rounding}` |
| Harness / activity glyph | `label` (Nerd Font) or `image` node |
| Progress `2/3` | `label` + `monospace` |
| Summary chips | `horizontal` + `metric` |
| Card grid | `columns` + `card` |
| Health arc | `progress_bar` (a true arc needs a node) |
| Row activation | `button` + `ui:clicked(id)` |
| Overflow | already wrapped in `ScrollArea` |

### Gaps this design needs from the fork

| Need | Why |
| :--- | :--- |
| `ui:width()` | Size classes are unbuildable without the pane's pixel width in the render callback. Smallest, highest-value addition on this list. |
| `ui:collapsed(id)` | egui owns collapse state in its own memory. Lua can't persist the layout across a config reload, or skip building a closed subtree. |
| Frame time / clock | Breathing and spinner phase need a monotonic `t`. Either drive it from Rust or expose elapsed seconds to Lua. |
| Full-row hit target | `button` is a button. A source-list row is a borderless full-width click region — needs a `selectable` node or `frame{clickable=true, id=…}`. |
| Animated position | Urgency ordering needs interpolated row positions, or it snaps. egui can do it; the `UiNode` tree has nowhere to say so. |
| Arc / donut node | For the card health ring. Optional — a stacked bar substitutes. |

---
*Summary of Architectural Decision:* `egui` is embedded into `wezterm-gui` via `egui-wgpu`, composited in a second render pass over the WebGpu surface. Lua drives it through a deferred `UiNode` tree built by `LuaUi` and replayed each frame, with mouse/keyboard input forwarded back into egui. Dual-instance activity tracking works out of the box using file locks and atomic JSON snapshots.
