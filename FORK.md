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

## Installing

A single binary is built from `main` and installed as `workmux`, so agent
hooks calling `~/.local/bin/workmux` use the fork build:

```bash
cargo build --release
install -m 755 target/release/workmux ~/.local/bin/workmux
```
