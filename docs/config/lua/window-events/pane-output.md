# `pane-output`

The `pane-output` event is emitted when a pane in the GUI window receives
terminal output. Its arguments are the GUI `window` and the pane that produced
the output. Events are coalesced while a prior callback is running, so a busy
pane produces at most one queued follow-up callback.

The event also applies to panes in inactive tabs in that GUI window. It is a
notification that output arrived, not a complete lifecycle signal: use a timer
as a fallback for process changes and states that require time-based
confirmation.

```lua
wezterm.on('pane-output', function(window, pane)
  wezterm.log_info('output from pane ' .. pane:pane_id())
end)
```
