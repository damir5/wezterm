#!/bin/sh
# Upgrade the WezTeam GUI or mux server without losing panes by accident.
#
#   upgrade.sh gui          build, start a new GUI, stop the old one once the
#                           new one shows every pane
#   upgrade.sh pin-server   build and pin the mux server under $RUNTIME/bin;
#                           the running server keeps running
#   upgrade.sh server       pin, then restart the mux server. Kills every
#                           local pane, so it requires CONFIRM=yes
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd)
APP=/Applications/WezTeam.app
RUNTIME="$HOME/.local/share/wezteam"
BUILT_SERVER="$REPO/target/debug/wezterm-mux-server"
PINNED_SERVER="$RUNTIME/bin/wezterm-mux-server"
CLI="$REPO/target/debug/wezterm"
export WEZTERM_UNIX_SOCKET="$RUNTIME/sock"

json_len() { python3 -c 'import json,sys; print(len(json.load(sys.stdin)))'; }
client_pids() {
	"$CLI" cli list-clients --format json |
		python3 -c 'import json,sys; print(" ".join(str(c["pid"]) for c in json.load(sys.stdin)))'
}
gui_pids() {
	for pid in $(client_pids); do
		case "$(ps -o comm= -p "$pid" 2>/dev/null)" in
			*wezteam-gui | *wezterm-gui) echo "$pid" ;;
		esac
	done
}

upgrade_gui() {
	cargo build --manifest-path "$REPO/Cargo.toml" -p wezterm-gui -p wezterm
	old=$(gui_pids)
	panes=$("$CLI" cli list --format json | json_len)
	open -n -a "$APP"
	# ponytail: fixed 60s wait; a slow cold start just needs a rerun
	i=0
	while [ "$i" -lt 60 ]; do
		sleep 1
		i=$((i + 1))
		for pid in $(gui_pids); do
			case " $old " in *" $pid "*) continue ;; esac
			shown=$(WEZTERM_UNIX_SOCKET="$RUNTIME/gui-sock-$pid" "$CLI" cli list --format json 2>/dev/null | json_len) || continue
			[ "$shown" -eq "$panes" ] || continue
			[ -n "$old" ] && kill -TERM $old
			echo "New GUI $pid shows all $panes panes; stopped old GUI: ${old:-none}"
			if [ "$BUILT_SERVER" -nt "$PINNED_SERVER" ]; then
				echo "Note: the built mux server is newer than the pinned one. Run 'make upgrade-server' when local panes can be closed."
			fi
			return 0
		done
	done
	echo "No new GUI showed all $panes panes within 60s; old GUI left running: ${old:-none}" >&2
	return 1
}

pin_server() {
	cargo build --manifest-path "$REPO/Cargo.toml" -p wezterm-mux-server
	mkdir -p "$RUNTIME/bin"
	# Copy then rename: overwriting a binary in place breaks a running server.
	cp "$BUILT_SERVER" "$PINNED_SERVER.new"
	mv "$PINNED_SERVER.new" "$PINNED_SERVER"
	echo "Pinned $PINNED_SERVER"
}

upgrade_server() {
	# tmux panes have no local tty; they live on their host and survive.
	local_panes=$("$CLI" cli list --format json |
		python3 -c 'import json,sys; [print("  %s  %s" % (p["pane_id"], p["title"])) for p in json.load(sys.stdin) if p.get("tty_name")]')
	if [ "${CONFIRM:-}" != yes ]; then
		echo "Restarting the mux server kills these local panes:" >&2
		echo "$local_panes" >&2
		echo "Rerun with CONFIRM=yes to proceed." >&2
		return 1
	fi
	pin_server
	pid=$(cat "$RUNTIME/pid")
	kill -TERM "$pid"
	i=0
	while kill -0 "$pid" 2>/dev/null; do
		[ "$i" -lt 100 ] || { echo "Server $pid did not exit" >&2; return 1; }
		sleep 0.1
		i=$((i + 1))
	done
	"$PINNED_SERVER" --daemonize
	i=0
	until "$CLI" cli list --format json >/dev/null 2>&1; do
		[ "$i" -lt 100 ] || { echo "New server did not start; see $RUNTIME/log" >&2; return 1; }
		sleep 0.1
		i=$((i + 1))
	done
	echo "Mux server restarted as $(cat "$RUNTIME/pid"). GUIs reconnect on their own;"
	echo "if a GUI stays blank (codec change), run 'make swap-gui'."
}

case "${1:-}" in
	gui) upgrade_gui ;;
	pin-server) pin_server ;;
	server) upgrade_server ;;
	*) echo "usage: $0 gui|pin-server|server" >&2; exit 2 ;;
esac
