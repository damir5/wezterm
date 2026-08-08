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

`refresh_after_ms` is optional and schedules one guarded, one-shot rebuild;
there is no polling timer. The feature requires the WebGpu frontend. Other
frontends leave the sidebar entirely disabled so no blank input region exists.
