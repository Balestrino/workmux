"""Named sidebar scopes through the real CLI, daemon, TUI and popup."""

import os
import shlex
from pathlib import Path

import pytest

from .conftest import TmuxEnvironment, poll_until
from .test_popup_navigation import attached_client

pytestmark = pytest.mark.tmux_only


@pytest.mark.parametrize("position", ["left", "top"])
def test_sidebar_view_chooser(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path, position: str
):
    env = mux_server
    # Unix sidebar sockets must fit macOS sockaddr_un, unlike pytest paths.
    env.env["TMPDIR"] = "/tmp"
    env.tmux(["set-environment", "-g", "TMPDIR", "/tmp"])
    config = env.home_path / ".config/workmux/config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        "sidebar:\n  git_status: false\n  views:\n"
        "    - name: Named sessions\n      sessions: [test, foo, bar, xyz]\n"
        f"    - name: Code\n      project_dirs: [{repo_path.parent}]\n"
        "    - name: Empty\n      sessions: [absent]\n"
        "    - name: Other\n      other: true\n"
    )
    env.env["TMUX"] = f"{env.socket_path},0,0"
    assert env.runner_pane_id is not None
    env.env["TMUX_PANE"] = env.runner_pane_id

    def cli(*args: str, check: bool = True):
        result = env.run_command(
            [str(workmux_exe_path), "sidebar", *args], cwd=repo_path, check=False
        )
        if check:
            assert result.returncode == 0, result.stderr
        return result

    assert cli("view", "--list").stdout.splitlines() == [
        "All",
        "Named sessions",
        "Code",
        "Empty",
        "Other",
    ]
    assert cli("view", "missing", check=False).returncode != 0
    cli("view")
    assert (
        env.tmux(["show-option", "-gqv", "@workmux_sidebar_view"]).stdout.strip()
        == "Named sessions"
    )
    cli("view")
    assert (
        env.tmux(["show-option", "-gqv", "@workmux_sidebar_view"]).stdout.strip()
        == "Code"
    )

    # Register a real agent so daemon filtering and navigation can be observed.
    env.run_command(
        [str(workmux_exe_path), "set-window-status", "working"], cwd=repo_path
    )
    with attached_client(env) as (master, _):
        cli("on", "--position", position, "--width", "40", "--height", "4")
        try:

            def inventory():
                return env.tmux(
                    ["show-option", "-gqv", "@workmux_sidebar_agents"]
                ).stdout.strip()

            assert poll_until(
                lambda: env.runner_pane_id in inventory().split(), timeout=10
            )
            panes = env.tmux(
                ["list-panes", "-t", "test:", "-F", "#{pane_id}"]
            ).stdout.splitlines()
            sidebar = next(pane for pane in panes if pane != env.runner_pane_id)

            def screen():
                return env.tmux(["capture-pane", "-p", "-t", sidebar]).stdout

            assert poll_until(lambda: "[view: Code]" in screen())
            cli("view", "Empty")
            assert poll_until(
                lambda: "No matching agents" in screen() and not inventory()
            )
            assert "[view: Empty]" in screen()
            cli("view", "Other")
            assert poll_until(lambda: "[view: Other]" in screen() and not inventory())
            cli("view")
            assert poll_until(lambda: "[view: All]" in screen() and bool(inventory()))

            env.tmux(["select-pane", "-t", sidebar])
            env.tmux(["send-keys", "-t", sidebar, "V"])
            assert poll_until(lambda: "[view: Named sessions]" in screen())

            # Run the shipped chooser in a real popup, selecting its fourth entry.
            script = (
                Path(__file__).resolve().parents[1] / "scripts/workmux-sidebar-view"
            )
            ready = env.tmp_path / "chooser-ready"
            command = f"touch {shlex.quote(str(ready))}; PATH={shlex.quote(str(workmux_exe_path.parent))}:$PATH bash {shlex.quote(str(script))}"
            env.tmux(["bind-key", "r", "display-popup", "-E", command])
            os.write(master, b"\x02r")
            assert poll_until(ready.exists)
            os.write(master, b"4\r")
            assert poll_until(lambda: "[view: Empty]" in screen() and not inventory())
            # A removed selection falls back to All after live config reload.
            config.write_text(
                config.read_text().replace("name: Empty", "name: Renamed")
            )
            assert poll_until(
                lambda: "[view: All]" in screen() and bool(inventory()), timeout=10
            )
            config.write_text("sidebar:\n  git_status: false\n")
            assert poll_until(
                lambda: "[view:" not in screen() and bool(inventory()), timeout=10
            )
            assert cli("view", "--list").stdout.strip() == "All"
        finally:
            cli("off")
