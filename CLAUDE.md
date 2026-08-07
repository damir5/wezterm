# wezterm (fork)

Fork of wezterm adding a **`GuiPane`** — a pane whose contents are an egui widget tree
built from Lua — so a config can render dashboards inside the terminal.

Upstream base: `4b1c3c1`. Branch: `feat/egui-guipane-dashboards`.

## What the fork adds

| Area | File |
| --- | --- |
| The pane + `UiNode` tree | `mux/src/guipane.rs` |
| Lua bindings (`LuaUi`, `split_dashboard`, `set_split_size`) | `lua-api-crates/mux/src/gui.rs` |
| `UiNode` -> egui | `wezterm-gui/src/termwindow/render/guipane_ui.rs` |
| Input forwarding into egui | `wezterm-gui/src/termwindow/{mouseevent,keyevent}.rs` |
| `render-gui-pane` emit, 10 Hz throttle | `wezterm-gui/src/termwindow/mod.rs` |

Lua sees `wezterm.on('render-gui-pane', function(window, pane, ui) ... end)`. The `ui`
object accumulates a `UiNode` tree that the egui pass replays each frame; interaction
comes back on the *next* fire via `ui:clicked(id)` / `ui:value(id)`.

## Checking a change

```sh
cargo check -p mux-lua && cargo test -p mux-lua   # fast, covers mux + the Lua bindings
cargo build --release                             # the real thing; needs submodules
```

`cargo test -p mux-lua` does not need the freetype/harfbuzz submodules, so it is the
cheap loop for anything in `gui.rs` or `guipane.rs`.

## Conventions worth keeping

- **New `split_dashboard` options must default to the previous behaviour.** It already
  takes `side`, `size_cols`, `top_level`, `focus` and `pane_id`; every default
  reproduces the original right-edge, percent-sized, focus-stealing split so existing
  callers are unaffected.
- `set_split_size` exists because `AdjustPaneSize` only moves the *active* pane's edge,
  so a sidebar could not resize without first stealing focus. Keep it addressing the
  split by pane id.
- Widgets are described by Lua and rendered by Rust; do not hand a live `&mut egui::Ui`
  across the mlua boundary. Containers take a Lua callback and push onto a child-list
  stack instead.
- A `GuiPane` must be registered with `mux.add_pane` or the tab's `prune_dead_panes`
  removes it on first paint (wezterm #4030).

## Consumer

`../../test/wezterm-agent-poc` drives this API (FleetView sidebar). It feature-detects,
so it still runs on stock wezterm — do not assume the fork is present when changing the
Lua-facing surface, but do keep it additive.
