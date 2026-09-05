"""Private mux/tmux paste regression; build both binaries first. Optional file argument."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

BIN = Path(__file__).resolve().parents[2] / "target" / "debug"
PAYLOAD = Path(sys.argv[1]).read_bytes() if len(sys.argv) > 1 else (
    'line " č ✓ $HOME ; \\ end\nsecond\r\nthird\r' * 2000
).encode()


def check(bracketed, split=False):
    with tempfile.TemporaryDirectory(prefix="paste-mux-") as directory:
        path = Path(directory)
        name = f"paste-mux-{os.getpid()}"
        socket = str(path / "sock")
        receiver = path / "read.py"
        mode = b"\x1b[?2004h" if bracketed else b""
        receiver.write_text(
            "import os, tty, pathlib\ntty.setraw(0)\n"
            f"os.write(1, {mode!r})\n"
            f"pathlib.Path({str(path / 'ready')!r}).touch()\n"
            f"with open({str(path / 'out')!r}, 'wb', buffering=0) as output:\n"
            " while True: output.write(os.read(0, 65536))\n"
        )
        config = path / "config.lua"
        config.write_text('return {initial_cols=120,initial_rows=40,unix_domains={{name="fixture",socket_path="' + socket + '"}}}')
        env = dict(os.environ, WEZTERM_UNIX_SOCKET=socket)
        env.pop("WEZTERM_PANE", None)
        cli = [str(BIN / "wezterm"), "--config-file", str(config), "cli"]

        def tmux(*args):
            return subprocess.check_output(["tmux", "-L", name, *args])

        mux = None
        tmux("new-session", "-d", "-x", "80", "-y", "24", "-s", "test", f"python3 {receiver}")
        try:
            if split:
                tmux("split-window", "-h", "-t", "test", "sleep 60")
            for _ in range(100):
                if (path / "ready").exists():
                    break
                time.sleep(0.02)
            assert (path / "ready").exists(), "receiver did not start"
            # Enable mode BEFORE attachment: surviving applications do not replay it.
            time.sleep(0.2)
            with open(path / "mux.log", "w") as log:
                mux = subprocess.Popen(
                    [str(BIN / "wezterm-mux-server"), "--config-file", str(config),
                     "--", "tmux", "-L", name, "-CC", "attach-session", "-t", "test"],
                    env=env, stdout=log, stderr=log,
                )
                panes = []
                for _ in range(200):
                    time.sleep(0.05)
                    if not Path(socket).exists():
                        continue
                    panes = json.loads(subprocess.check_output(
                        cli + ["list", "--format", "json"], env=env, timeout=5))
                    if len(panes) == (3 if split else 2):
                        break
                remote = next(p for p in panes if p["tty_name"] is None)
                anchor = next(p for p in panes if p["tty_name"] is not None)
                for _ in range(100):
                    remote_size = tmux("display-message", "-p", "-t", "test", "#{window_width}x#{window_height}").strip()
                    if remote_size == b"120x40":
                        break
                    time.sleep(0.01)
                assert remote_size == b"120x40", ("initial attach size", remote_size)
                if split:
                    widths = [int(width) for width in tmux("list-panes", "-t", "test", "-F", "#{pane_width}").splitlines()]
                    assert sum(widths) + 1 == 120 and min(widths) > 40, widths
                start = time.monotonic()
                subprocess.run(cli + ["send-text", "--pane-id", str(remote["pane_id"])],
                               input=PAYLOAD, env=env, timeout=15, check=True)
                expected = b"\x1b[200~" + PAYLOAD + b"\x1b[201~" if bracketed else PAYLOAD
                for _ in range(500):
                    if (path / "out").stat().st_size >= len(expected):
                        break
                    time.sleep(0.01)
                received = (path / "out").read_bytes()
                assert received == expected, (len(received), len(expected))
                assert not tmux("list-buffers"), "private paste buffer was not cleaned up"
                for target, payload, error in [
                    (anchor, b"test", b"connection pane"),
                    (remote, b"before\0after", b"NUL"),
                ]:
                    result = subprocess.run(
                        cli + ["send-text", "--pane-id", str(target["pane_id"])],
                        input=payload, env=env, capture_output=True, timeout=5)
                    assert result.returncode != 0 and error in result.stderr, result.stderr
                print(f"bracketed={bracketed}, split={split}: {len(received)} bytes intact, "
                      f"{time.monotonic() - start:.2f}s, sha256={hashlib.sha256(received).hexdigest()}")
        finally:
            if mux is not None:
                mux.terminate()
                mux.wait(timeout=5)
            tmux("kill-server")


check(True)
check(False)
check(True, split=True)
