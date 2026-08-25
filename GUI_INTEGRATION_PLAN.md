# Window-owned tab sidebar

The original dashboard experiment was removed. The fork now exposes a single
window-owned sidebar that renders the tabs belonging to that window.

## Architecture

1. `TermWindow` snapshots tab and pane metadata after mux notifications.
2. Synchronous `format-tab-sidebar(tabs)` and `render-sidebar(tabs, context)`
   Lua callbacks provide validated metadata and a responsive retained tree.
3. Rust owns the cached tree, CSS-like layout, retained transitions, scrolling,
   hit testing, and animation invalidation.
4. The WebGpu renderer composites that tree once per frame. There is no
   native row fallback, dashboard pane, mux split, or Lua work in the paint path.

## Sidebar UI tooltips

Any retained sidebar node may set `tooltip` to a string or another retained UI
node. Strings wrap and elide inside the sidebar. A UI node may use the same
layout, typography, color, image, and vector-shape properties as sidebar
content; give its root an explicit width and height. Tooltips use native hover
tracking, disappear when the pointer leaves the node, and do not make a
tooltip-only node clickable.

```lua
ui.text('JACK 2', {
  tooltip = ui.column {
    width = 280, height = 48, gap = 6,
    children = {
      ui.text('LOCAL FORWARDS', { color = '#e2c07b' }),
      ui.text('8080 → 127.0.0.1:3000'),
    },
  },
})
```

`refresh_after_ms` is optional and schedules one guarded, one-shot rebuild;
there is no polling timer. The feature requires the WebGpu frontend. Other
frontends leave the sidebar entirely disabled so no blank input region exists.
