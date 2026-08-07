# Window-owned tab sidebar

The original dashboard experiment was removed. The fork now exposes a single
window-owned sidebar that renders the tabs belonging to that window.

## Architecture

1. `TermWindow` snapshots tab and pane metadata after mux notifications.
2. A synchronous `format-tab-sidebar(tabs)` Lua callback may decorate that
   snapshot. Callback data is validated; errors use native entries.
3. Rust creates one cached sidebar model and one native `UIItem` list per
   window. Native mouse hit testing activates tabs immediately.
4. The WebGpu renderer composites that model once per frame. There is no
   dashboard pane, no mux split, and no Lua work in the paint path.

`refresh_after_ms` is optional and schedules one guarded, one-shot rebuild;
there is no polling timer. The feature requires the WebGpu frontend. Other
frontends leave the sidebar entirely disabled so no blank input region exists.
