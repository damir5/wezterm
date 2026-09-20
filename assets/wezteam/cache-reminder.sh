#!/bin/sh
# Weekly nudge: report the build cache size and the cleanup target.
size=$(du -sh "$HOME/dev/vendor/wezterm/target" 2>/dev/null | cut -f1)
[ -n "$size" ] && osascript -e "display notification \"target/ is $size — run 'make cache-gc' in the fork\" with title \"wezteam build cache\"" >/dev/null 2>&1
