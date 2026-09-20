"""Attaching a populated multi-window tmux session survives client churn.

Connects and abruptly kills mux clients while the tmux -CC layout sync is
in flight and after it completes, then verifies that the domain stays
attached, input round-trips to a remote pane, and spawning into the tmux
domain still works.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

BIN = Path(__file__).resolve().parents[2] / "target" / "debug"


def main():
    with tempfile.TemporaryDirectory(prefix="attach-churn-") as directory:
        path = Path(directory)
        name = f"attach-churn-{os.getpid()}"
        socket = str(path / "sock")
        config = path / "config.lua"
        config.write_text(
            'return {exit_behavior="Hold",'
            'unix_domains={{name="fixture",socket_path="' + socket + '"}}}'
        )
        env = dict(os.environ, WEZTERM_UNIX_SOCKET=socket)
        env.pop("WEZTERM_PANE", None)
        cli = [
            str(BIN / "wezterm"),
            "--config-file",
            str(config),
            "cli",
            "--no-auto-start",
            "--prefer-mux",
        ]
        tmux = ["tmux", "-L", name]
        # The controller stacks two ptys with output post-processing (like
        # ssh inside a pane pty), so every protocol line reaches the mux
        # with a doubled trailing CR. The line parser must tolerate that;
        # before the fix a single misparsed line detached the whole domain.
        controller = path / "controller.py"
        controller.write_text(
            "import os, pty, select, sys, termios\n"
            "flags = termios.tcgetattr(0)\n"
            "flags[3] &= ~termios.ECHO\n"
            "termios.tcsetattr(0, termios.TCSANOW, flags)\n"
            "pid, fd = pty.fork()\n"
            "if pid == 0:\n"
            "    os.execvp(sys.argv[1], sys.argv[1:])\n"
            "while True:\n"
            "    r, _, _ = select.select([0, fd], [], [], 1.0)\n"
            "    if 0 in r:\n"
            "        data = os.read(0, 65536)\n"
            "        if data:\n"
            "            os.write(fd, data)\n"
            "    if fd in r:\n"
            "        try:\n"
            "            data = os.read(fd, 65536)\n"
            "        except OSError:\n"
            "            break\n"
            "        if not data:\n"
            "            break\n"
            "        os.write(1, data)\n"
        )
        controller_cmd = ["python3", str(controller)] + tmux + [
            "-CC", "attach-session", "-t", "test",
        ]
        # Populated session: a live-output pane, a shell window with a
        # split, and a third window.
        subprocess.run(
            tmux + [
                "new-session", "-d", "-s", "test", "-n", "w0",
                "sh -c 'while true; do echo tick; sleep 1; done'",
            ],
            check=True,
        )
        subprocess.run(tmux + ["new-window", "-d", "-t", "test", "-n", "w1", "sh"], check=True)
        subprocess.run(tmux + ["split-window", "-d", "-t", "test:w1", "sh"], check=True)
        subprocess.run(tmux + ["new-window", "-d", "-t", "test", "-n", "w2", "sh"], check=True)
        mux = None
        try:
            with (path / "mux.log").open("w") as log:
                mux = subprocess.Popen(
                    [
                        str(BIN / "wezterm-mux-server"),
                        "--config-file",
                        str(config),
                        "--",
                        *controller_cmd,
                    ],
                    env=env,
                    stdout=log,
                    stderr=log,
                )

                def panes(timeout=10):
                    return json.loads(
                        subprocess.check_output(
                            cli + ["list", "--format", "json"], env=env, timeout=timeout
                        )
                    )

                def remote_panes():
                    return [p for p in panes() if p["tty_name"] is None]

                # Attach while clients connect and are SIGKILLed mid-flight.
                while not Path(socket).exists():
                    if mux.poll() is not None:
                        raise AssertionError("mux server exited during startup")
                    time.sleep(0.05)
                deadline = time.time() + 20
                remote = []
                while time.time() < deadline:
                    try:
                        remote = remote_panes()
                    except subprocess.TimeoutExpired:
                        raise AssertionError("cli list wedged during attach")
                    victim = subprocess.Popen(
                        cli + ["list"],
                        env=env,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    victim.kill()
                    victim.wait()
                    if remote:
                        break
                    time.sleep(0.05)
                assert remote, "remote panes never attached"

                # Post-attach churn: repeated connect/disconnect must not
                # disturb the attached domain.
                count = len(remote)
                for _ in range(5):
                    assert len(remote_panes()) == count, "pane count changed across churn"
                    time.sleep(0.05)
                time.sleep(1.0)
                assert len(remote_panes()) == count, "tmux domain detached across churn"
                assert b"domain detached" not in (path / "mux.log").read_bytes(), (
                    "tmux domain was detached by client churn"
                )

                # Input round-trips through the controller into the remote
                # session.
                marker = f"CHURN_OK_{os.getpid()}"
                for pane in remote:
                    subprocess.run(
                        cli + ["send-text", "--pane-id", str(pane["pane_id"]), f"echo {marker}\n"],
                        env=env,
                        timeout=10,
                        check=True,
                    )
                deadline = time.time() + 10
                seen = False
                while time.time() < deadline and not seen:
                    for pane in remote:
                        text = subprocess.check_output(
                            cli + ["get-text", "--pane-id", str(pane["pane_id"])],
                            env=env,
                            timeout=10,
                        ).decode(errors="replace")
                        if marker in text:
                            seen = True
                    if not seen:
                        time.sleep(0.1)
                assert seen, "input did not round-trip to a remote pane"

                # The server still services spawn requests after churn.
                out = subprocess.check_output(
                    cli + ["spawn", "--pane-id", str(remote[0]["pane_id"]), "sh"],
                    env=env,
                    timeout=15,
                )
                new_pane = int(out.split()[-1])
                deadline = time.time() + 10
                while time.time() < deadline:
                    if any(p["pane_id"] == new_pane for p in panes()):
                        break
                    time.sleep(0.1)
                else:
                    raise AssertionError("spawned tmux pane never attached")

                print(
                    "attach with churn: remote panes attached; domain stayed "
                    "attached; input round-tripped; spawn into tmux domain ok"
                )
        finally:
            if mux is not None:
                mux.terminate()
                mux.wait(timeout=5)
            subprocess.run(tmux + ["kill-server"], check=True)


main()
