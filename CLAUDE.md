# wezterm (fork)

Fork of wezterm adding a window-owned tab sidebar. It is terminal chrome, not
a pane: it never creates splits, mux panes, or forwards input into egui.

Upstream base: `4b1c3c1`. Branch: `feat/egui-guipane-dashboards`.

## Tab sidebar

- Enable with `enable_tab_sidebar = true` and `front_end = 'WebGpu'`.
- `tab_sidebar_width` selects the automatic responsive width class in terminal
  cells; compact is at most 120pt and regular is clamped to 240–520pt.
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

## Running and installing

The macOS app is `/Applications/WezTeam.app` (`make install-app`). Its shim
execs `target/debug/wezterm-gui` at every launch, so **a rebuild never
requires reinstalling the app** — relaunch WezTeam and the new binary runs.
Run `make install-app` only when the bundle itself changes: icon, shim, or
Info.plist. The debug profile is the daily driver (`opt-level = 2`,
`debug = 1`, assertions off); release builds are not maintained. The fork's
sockets, logs, and gui-sock discovery live under `~/.local/share/wezteam`,
deliberately separate from stock wezterm (`~/.local/share/wezterm`) — the two
mux codecs are incompatible and must never share a socket.

The sidebar is deliberately disabled for non-WebGpu windows; it must not reserve
space or intercept input if it cannot be rendered.
