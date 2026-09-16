"""Abrupt control-client exit must remove local panes and preserve remote tmux."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

BIN = Path(__file__).resolve().parents[2] / "target" / "debug"


def check(shell_survives, detach=False):
    with tempfile.TemporaryDirectory(prefix="disconnect-mux-") as directory:
        path = Path(directory)
        name = f"disconnect-{os.getpid()}"
        socket = str(path / "sock")
        config = path / "config.lua"
        config.write_text((
            "local w = require 'wezterm'\n"
            "w.on('mux-startup', function()\n"
            " local function poll()\n"
            f"  local file = io.open({json.dumps(str(path / 'disconnect'))})\n"
            "  if file then\n"
            "   file:close()\n"
            "   for _, domain in ipairs(w.mux.all_domains()) do\n"
            "    if domain:name() == 'tmux' then domain:detach() end\n"
            "   end\n"
            "  else w.time.call_after(.05, poll) end\n"
            " end\n"
            " w.time.call_after(.05, poll)\n"
            "end)\n"
            if detach else ""
        ) + 'return {exit_behavior="Hold",unix_domains={{name="fixture",socket_path="' + socket + '"}}}')
        controller = path / "controller.py"
        controller.write_text(
            "import os, pathlib, subprocess, time\n"
            f"child = subprocess.Popen(['tmux', '-L', {name!r}, '-CC', 'attach-session', '-t', 'test'])\n"
            + ("" if detach else
               f"while not pathlib.Path({str(path / 'disconnect')!r}).exists(): time.sleep(.02)\nchild.kill()\n")
            + "child.wait()\n"
            + ("os.write(1, b'Connection closed.\\r\\nSHELL_READY> ')\ntime.sleep(60)\n" if shell_survives else "")
        )
        env = dict(os.environ, WEZTERM_UNIX_SOCKET=socket)
        env.pop("WEZTERM_PANE", None)
        cli = [str(BIN / "wezterm"), "--config-file", str(config), "cli", "--no-auto-start", "--prefer-mux"]
        tmux = ["tmux", "-L", name]
        subprocess.run(tmux + ["new-session", "-d", "-s", "test", "printf REMOTE_READY; sleep 60"], check=True)
        mux = None
        try:
            with (path / "mux.log").open("w") as log:
                mux = subprocess.Popen(
                    [str(BIN / "wezterm-mux-server"), "--config-file", str(config), "--", "python3", str(controller)],
                    env=env, stdout=log, stderr=log,
                )

                def panes():
                    return json.loads(subprocess.check_output(cli + ["list", "--format", "json"], env=env, timeout=5))

                for _ in range(200):
                    time.sleep(.05)
                    if Path(socket).exists():
                        attached = panes()
                        if any(p["tty_name"] is None for p in attached):
                            break
                else:
                    raise AssertionError("remote pane never attached")
                anchor = next(p for p in attached if p["tty_name"] is not None)
                (path / "disconnect").touch()
                for _ in range(100):
                    time.sleep(.05)
                    remaining = panes()
                    if not any(p["tty_name"] is None for p in remaining):
                        break
                assert not any(p["tty_name"] is None for p in remaining), "stale remote pane after transport exit"
                subprocess.run(tmux + ["has-session", "-t", "test"], check=True)
                if shell_survives:
                    text = subprocess.check_output(cli + ["get-text", "--pane-id", str(anchor["pane_id"])], env=env, timeout=5)
                    assert b"SHELL_READY>" in text, "local shell output swallowed after transport exit"
                print(f"shell_survives={shell_survives}, detach={detach}: local panes removed; remote session preserved")
        finally:
            if mux is not None:
                mux.terminate()
                mux.wait(timeout=5)
            subprocess.run(tmux + ["kill-server"], check=True)


check(True)
check(False)
check(True, detach=True)
