# Octolabs fork of workmux

This repository (`Balestrino/workmux`) is a product fork of
[raine/workmux](https://github.com/raine/workmux), customised for the Octolabs
multi-agent workflow (see
[octolabs-worktree](https://github.com/Octolabs-Livorno/octolabs-worktree)).
Changes are not proposed upstream.

## Branches and remotes

- `origin` is this fork, `upstream` is `raine/workmux`.
- `main` is the fork's product line: upstream plus our features. New work
  starts from `main` in a feature branch and is merged back with `--no-ff`.
- Upstream is brought in by **merging** `upstream/main` into `main`, never by
  rebasing, so the fork history is never rewritten. Merge when a fix or
  feature is needed, not necessarily at every upstream release:

  ```bash
  git fetch upstream
  git merge upstream/main
  cargo test --bin workmux && cargo clippy --all-targets
  git push origin main
  ```

## Keeping merges cheap

- Put fork features in their own modules (e.g. `src/workspaces.rs`,
  `src/command/dashboard/worktree_info.rs`) and keep the hooks into upstream
  code small.
- Do not edit upstream files only for fork documentation: fork-specific notes
  live here or in `docs/superpowers/`. Config keys read only by the fork are
  documented with the feature.
- Cutting the upstream link for good (GitHub fork detach, crate rename) is
  deferred until merging costs more than it brings.

## Fork features

- **Workspaces dashboard** (`dashboard.workspaces`): lists the worktrees of
  every Git repository under the configured workspace roots, with stable
  project names, env/agent/unpushed columns and orphan rows.
- **External worktree info provider** (`dashboard.worktree_info_command`,
  `dashboard.worktree_extra_columns`): per-worktree runtime data from an
  external command (the octolabs-worktree CLI) shown as an info panel or as
  table columns.
- **Full-height worktree table**: the Worktrees tab starts with the table
  alone; `i` toggles the info panel and `l` the git log panel under it
  (upstream always shows both). Panels are closed again at every start.
- **Agents located by their reported directory**: status hooks store the
  `cwd` that Claude and Codex pass on every event as the agent's `cwd`, next
  to the pane's `workdir`. Everything that locates an agent (Worktrees `Agent`
  column, Agents tab project/worktree, sidebar, `workmux status` `workdir`,
  worktree-targeted commands) follows that directory, so an agent started in
  `~` shows up on the worktree it `cd`s into. Resurrection still resumes in
  the pane's `workdir`, where the agent session was started. A `cwd` inside
  `dashboard.ignore_agent_dirs` (default `~/.claude`, where agents write
  memory) keeps the agent's last other directory.
- **Background work status** (`status_icons.background`, default ⏳): an
  agent whose turn has ended while a background subagent, shell command or
  Monitor (Claude Code) or subagent (Codex) it launched is still running is
  shown as background instead of done, everywhere the status is shown. It
  becomes done when the last item finishes (which also fixes Codex staying
  `working` after its last `SubagentStop`); every item expires. The stored
  status stays `working`; `workmux status --json` adds `background: true`.
- **Unified worktree view** (`dashboard.agents_tab`, default `false`): the
  dashboard is one worktree view; each worktree row is followed by sub-rows
  (`└`, status icon, elapsed time, title) for the agents working in it, using
  the deepest listed worktree containing the agent's directory. Agents in no
  worktree are grouped at the bottom under "Outside worktrees" with their
  directory (shown only once the worktree list has loaded and with no project
  picked). Enter on a sub-row jumps to the agent, on a worktree row to its
  first agent, else its window; worktree actions on a sub-row act on its
  parent and show `not in a worktree` in the outside group; the `i`/`l`
  panels toggle on every row. The view ignores the session scope and
  `hide_stale` (and `--session`), and hides the `agent` column. The Agents
  tab is only hidden: `dashboard.agents_tab: true` restores the old two-tab
  dashboard, with one intended change: Enter on a worktree row also finds an
  agent working in a subdirectory of it (deepest-worktree assignment instead
  of an exact path match).

Design and plan: `docs/superpowers/`.

## Upstream level and differences from upstream

`main` contains upstream up to **v0.1.281** (merged 2026-11-10; the build
reports `workmux 0.1.281`). Besides the features above, the fork departs from
upstream behaviour in these points:

- **A hook event is never dropped by a snapshot conflict.** Since upstream
  `5429719e` a status write takes the pane snapshot outside the agent-state
  lock and is skipped after three conflicting attempts. A plain status
  survives that (the next hook repeats it), but the background state is built
  from hook events (a launch, a stop, a subagent start) that nothing repeats:
  a lost one leaves an agent done with work still running, or working after
  its turn ended. Measured on an isolated tmux server with the merged build:
  5 simultaneous hooks lost 13 of 40 background launches and 16 lost 32 of
  40, where 0.1.277 lost none. A write that carries a hook event (Claude
  Code, Codex) therefore makes one last attempt with the snapshot taken under
  the lock, as 0.1.277 did; with it 24 simultaneous hooks lost 0 of 48.
  Writes without an event keep upstream's behaviour. Cost: in that rare case
  the lock is held for one tmux query. Decided 2026-11-10; it is one commit
  (`fix(state): do not drop a hook event after three snapshot conflicts`) and
  can be reverted on its own.
- **Worktree footer clicks** include upstream's new `Enter Open` entry, which
  upstream v0.1.281 draws but leaves out of the click map (every footer click
  lands one entry to the right there).
- `GitStatus.outside_repository` (fork) is written only when set, so the
  cached observation of a remote workspace keeps upstream's pinned format.
- **A status hook stores the state before it sets the window icon**, because
  the background state decides the icon; upstream sets the icon first. Nothing
  in the binary depends on the order, but upstream's test helper
  `mark_agent_state` did (it waited for the state file, then read the window):
  in the fork it also waits for the command to end.
- **Expired background work counts as the state the agent was in.** A status
  write compares against what the agent showed, not the stored `working`: the
  prompt after an expiry is a new transition with its own timestamp (without
  it `workmux send --wait` reported `send_stalled`), a late `Stop` keeps the
  expiry time, and a late tool call of an expired subagent leaves the agent
  done.
- **Emoji with the variation selector U+FE0F draw in place** in the sidebar
  and the dashboard. The sidebar's fallback dirty icon is one (`⚠️`, used
  without Nerd Font), and the row with the git counts came out corrupted and
  stayed so until a full redraw, such as a click. There were two causes:
  - text width was summed per character, so `⚠️` counted one cell where it
    draws two, and the row came out one cell wider than the pane; its last
    character wrapped onto the next row. `display_width` and
    `truncate_with_ellipsis` (`src/util.rs`) and the sidebar's
    `truncate_to_width` now measure per grapheme, as ratatui and tmux do.
  - on a redraw, ratatui 0.30 re-sends the blank trailing cell of such an
    emoji, and its crossterm backend writes it without moving the cursor, as
    if the emoji were one cell wide. In tmux the blank and the rest of the run
    landed one column to the right (`⚠️  +44`, `-28-  2`), and an emoji
    moving one column over its old copy was erased. `src/wide_cell_backend.rs`
    wraps the backend: it drops a blank inside a wide symbol just drawn, and
    blanks the cell an emoji will widen into before drawing it.

  Measured on an isolated tmux server (tmux 3.5a): 600 random rows redrawn
  through ratatui's backend came out wrong 452 times, 0 through the wrapper;
  the real 25-column sidebar now matches a fresh redraw at every step. Fixed
  2026-11-12; revertible on its own. The dashboard's diff view keeps its own
  per-character measuring.

Upstream changes worth knowing when using the fork:

- `workmux send`, `capture` and `run` refuse a worktree with more than one
  agent (`ambiguous_target`) instead of picking the first. With agents located
  by their reported directory, a second agent that `cd`s into a worktree is
  enough: target the pane ID (`pane_id` of `workmux status --json`).
- Remote workspaces, the zellij sidebar and Copilot support come with the
  merge but are not used or tried here. The unified view shows a remote
  agent's sub-row with its `handle@remote` label and, when the remote is
  unreachable or the execution ended, that state; Enter on one that no local
  pane relays opens its window, as in the Agents tab. Known limit: remote
  agents never show the background state, because the node's observation
  carries only the status.

## Checks

`just check` needs `checkle`, `bun`, `ruff` and `pyright`. Without them the
same checks (`checkle.toml`) can be run by hand; versions as in upstream's CI
(`.github/workflows/ci.yml`: `ruff==0.14.4`, `bun` 1.3.14):

```bash
cargo fmt --all -- --check && cargo clippy --all-targets && cargo test --bin workmux
uv venv tests/venv && uv pip install --python tests/venv/bin/python \
  -r tests/requirements.txt pyright 'ruff==0.14.4'
tests/venv/bin/ruff format tests --check && tests/venv/bin/ruff check tests
(source tests/venv/bin/activate && pyright tests)
bun test tests/opencode_status_plugin.test.ts tests/pi_status_extension.test.ts \
  tests/omp_status_extension.test.ts
bun test tests/opencode_status_plugin_v2.test.ts tests/opencode_status_tracker_v2.test.ts
(cd docs && bun install --frozen-lockfile && bun run format:check \
  && node scripts/check-content.mjs)
```

Integration tests (upstream's, `tests/`, tmux backend) run the debug build at
`target/debug/workmux` in private tmux servers and temporary homes:

```bash
cargo build --all
source tests/venv/bin/activate
WORKMUX_TEST=1 env -u TMUX -u TMUX_PANE pytest tests/ -n 4 \
  --ignore=tests/test_sidebar_zellij.py --ignore=tests/test_sidebar_zellij_live.py \
  --ignore=tests/test_workmux_remote --basetemp=/var/tmp/<short-path>
```

Keep `--basetemp` short: with a long path the shell prompt wraps and
`test_terminal_echo` cannot find its marker. The zellij files need zellij;
`tests/test_workmux_remote` (remote workspaces over a loopback node) filled
several GB of temporary files within minutes and was not completed here.

## Installing

A single binary is built from `main` and installed as `workmux`, so agent
hooks calling `~/.local/bin/workmux` use the fork build:

```bash
cargo build --release
install -m 755 target/release/workmux ~/.local/bin/workmux
```
