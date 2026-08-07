# wezterm (fork)

Fork of wezterm adding a window-owned tab sidebar. It is terminal chrome, not
a pane: it never creates splits, mux panes, or forwards input into egui.

Upstream base: `4b1c3c1`. Branch: `feat/egui-guipane-dashboards`.

## Tab sidebar

- Enable with `enable_tab_sidebar = true` and `front_end = 'WebGpu'`.
- `tab_sidebar_width` is the regular width in terminal cells; compact mode is
  six cells and is toggled with `ToggleTabSidebarMode`.
- Lua may optionally implement synchronous `format-tab-sidebar(tabs)`, returning
  `{ entries = {...}, refresh_after_ms = n }`. Entries are keyed by an existing
  `tab_id`; malformed data falls back to the native snapshot.
- Rust owns snapshots, grouping, scrolling, hit rectangles, activation, and
  painting. The callback runs only after queued mux changes or an explicitly
  requested one-shot refresh.

Relevant files:

| Area | File |
| --- | --- |
| Sidebar model/callback decoding | `wezterm-gui/src/termwindow/tab_sidebar.rs` |
| Native input/activation | `wezterm-gui/src/termwindow/mouseevent.rs` |
| WebGpu compositing | `wezterm-gui/src/termwindow/render/draw.rs` |
| Lua snapshot fields | `wezterm-gui/src/termwindow/mod.rs` |

## Checking a change

```sh
cargo test -p wezterm-gui tab_sidebar
cargo check -p wezterm-gui -p mux -p mux-lua
```

The sidebar is deliberately disabled for non-WebGpu windows; it must not reserve
space or intercept input if it cannot be rendered.
