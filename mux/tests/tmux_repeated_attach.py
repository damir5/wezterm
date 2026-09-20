"""Repeated tmux -CC attach cycles in one pane keep typing working.

The jack flow attaches control mode in a pane, loses the connection, then
attaches again in the SAME pane shell; later attaches reuse and reset the
existing tmux domain. After every cycle the remote panes must accept input
and the controller pane must accept the next attach command, whether the
previous session ended with an orderly detach or with the control client
being killed outright.
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

BIN = Path(__file__).resolve().parents[2] / "target" / "debug"
CYCLES = 4


def main():
    with tempfile.TemporaryDirectory(prefix="attach-repeat-") as directory:
        path = Path(directory)
        name = f"attach-repeat-{os.getpid()}"
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
        subprocess.run(
            tmux + ["new-session", "-d", "-s", "test", "-n", "w0", "sh"], check=True
        )
        subprocess.run(
            tmux + ["new-window", "-d", "-t", "test", "-n", "w1", "sh"], check=True
        )
        mux = None
        try:
            with (path / "mux.log").open("w") as log:
                mux = subprocess.Popen(
                    [
                        str(BIN / "wezterm-mux-server"),
                        "--config-file",
                        str(config),
                        "--",
                        "sh",
                    ],
                    env=env,
                    stdout=log,
                    stderr=log,
                )

                def panes():
                    return json.loads(
                        subprocess.check_output(
                            cli + ["list", "--format", "json"], env=env, timeout=10
                        )
                    )

                def remote():
                    return [p for p in panes() if p["tty_name"] is None]

                def send(pane_id, text):
                    subprocess.run(
                        cli + ["send-text", "--pane-id", str(pane_id), text],
                        env=env,
                        timeout=10,
                        check=True,
                    )

                def text(pane_id):
                    return subprocess.check_output(
                        cli + ["get-text", "--pane-id", str(pane_id)],
                        env=env,
                        timeout=10,
                    ).decode(errors="replace")

                def wait(cond, what, timeout=15):
                    deadline = time.time() + timeout
                    while time.time() < deadline:
                        if cond():
                            return True
                        time.sleep(0.1)
                    return False

                while not Path(socket).exists():
                    if mux.poll() is not None:
                        raise AssertionError("mux server exited during startup")
                    time.sleep(0.05)
                assert wait(panes, "bootstrap pane"), "bootstrap pane never appeared"
                console = panes()[0]["pane_id"]
                console_tty = os.path.basename(panes()[0]["tty_name"])

                def control_client_pids():
                    out = subprocess.run(
                        ["ps", "-t", console_tty, "-o", "pid=,command="],
                        capture_output=True,
                        text=True,
                    ).stdout
                    return [
                        int(line.split()[0])
                        for line in out.splitlines()
                        if "tmux" in line and "-CC" in line
                    ]

                for cycle in range(1, CYCLES + 1):
                    marker = f"REPEAT_OK_{cycle}"
                    send(console, f"tmux -L {name} -CC attach -t test\n")
                    assert wait(remote, f"attach {cycle}"), (
                        f"cycle {cycle}: remote panes never attached\n" + text(console)
                    )
                    attached = remote()

                    # Input must reach the remote session on every cycle.
                    target = attached[-1]["pane_id"]
                    send(target, f"echo {marker}\n")
                    assert wait(lambda: marker in text(target), f"typing {cycle}"), (
                        f"cycle {cycle}: typing into remote pane failed\n" + text(target)
                    )

                    # Alternate an orderly detach with an abrupt death of the
                    # control client, which is what a dropped ssh chain does.
                    if cycle % 2:
                        subprocess.run(
                            tmux + ["detach-client", "-s", "test"],
                            check=True,
                            capture_output=True,
                        )
                        how = "detach"
                    else:
                        for pid in control_client_pids():
                            os.kill(pid, signal.SIGKILL)
                        how = "kill"
                    assert wait(
                        lambda: not remote(), f"teardown {cycle}"
                    ), f"cycle {cycle}: remote panes outlived the {how}"

                    # The controller pane must leave control mode, or the next
                    # attach cannot be typed into it.
                    alive = f"SHELL_ALIVE_{cycle}"
                    send(console, f"echo {alive}\n")
                    assert wait(
                        lambda: alive in text(console), f"controller input {cycle}"
                    ), (
                        f"cycle {cycle}: controller pane stopped accepting input "
                        f"after {how}\n" + text(console)
                    )

                reused = (path / "mux.log").read_text(errors="replace").count(
                    "reusing existing tmux domain"
                )
                print(
                    f"{CYCLES} attach cycles ok: typing round-tripped every time; "
                    f"tmux domain reused on {reused} re-attaches"
                )
        finally:
            if mux is not None:
                mux.terminate()
                mux.wait(timeout=5)
            subprocess.run(tmux + ["kill-server"], check=False, capture_output=True)


main()
