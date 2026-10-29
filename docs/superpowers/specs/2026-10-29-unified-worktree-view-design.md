# Unified Worktree View — Design

Date: 2026-10-29
Fork: `Balestrino/workmux`, branch `feat/unified-view` off fork `main`
(`4f39b32a`: upstream v0.1.277 plus the fork features in `FORK.md`, including
agents located by hook `cwd` and the background state ⏳)

## Intent

The dashboard has two tabs. Agents lists the agent panes; Worktrees lists the
worktrees of every workspace repository with git, PR and environment columns
and a one-glyph `Agent` summary. The user sees 5 agents in one tab and 3 in
the other (two agents work in `~/.claude/projects/-home-tore/memory`, which is
no worktree) and has to switch tabs to connect the two.

What the user wants from the dashboard, in their words: know **all the
projects/worktrees open on the host, where each one stands, and which agents
are working on them**. From an agent, the only action they use is **Enter, to
jump into it and see what it is doing**; nothing else from the Agents tab is
missed ("Niente, basta Enter").

They picked layout A from three mockups: one view, agents shown as sub-rows
under the worktree they work in, agents outside any worktree grouped at the
bottom.

## Goals

1. `workmux dashboard` opens on a single worktree view; no Agents tab, no Tab
   switching.
2. Under each worktree row, one sub-row per agent working in it: status icon
   (🤖 with spinner, 💬, ⏳, ✅, as the Agents tab draws them), elapsed time,
   pane title.
3. Agents whose directory is in no listed worktree appear in a bottom group
   "Outside worktrees", each with its directory (home shortened to `~`).
4. Enter on an agent sub-row jumps to that agent's pane; Enter on a worktree
   row jumps to its first agent sub-row, else to its window as today.

## Non-goals

- The Agents tab's other features (terminal preview, input mode, diff and
  patch views, commit/merge triggers, kill, peek, stale/session filters,
  agent sort modes) are not carried into the unified view.
- `workmux list`, `workmux status`, the sidebar and the hooks are unchanged.
- No change to the agent-to-directory data: agents are located by
  `AgentPane.path` (the hook `cwd` when reported, else the pane directory).

## Anti-goals

- Deleting the Agents tab code: it stays, hidden, so upstream merges stay
  cheap and it can be turned back on.
- An agent counted under two worktrees (a linked worktree nested inside its
  primary clone contains paths of both).
- Worktree actions silently hitting the wrong worktree from an agent sub-row.

## Design

### Switch

A fork-only config key `dashboard.agents_tab: bool`, default `false`
(upstream ignores unknown keys, so the shared config file stays valid for
upstream). With `false`:

- the dashboard always shows the Worktrees view (`--tab agents` is ignored);
- the tab header shows only the Worktrees title, Tab does nothing, and Tab
  disappears from the footer, the help overlay and the command palette;
- the unified rows below are used.

With `true` the dashboard behaves exactly as today (two tabs, no sub-rows).

### Rows

The table is a list of rows of these kinds, in this order:

1. each worktree row as today, immediately followed by its agent sub-rows;
2. the existing environment orphan rows (unchanged);
3. if any agent is unassigned: a group header row "Outside worktrees"
   (not selectable) followed by those agents' sub-rows.

Assignment: an agent belongs to the listed worktree whose path is the
**deepest** ancestor-or-equal of `AgentPane.path` (canonicalised, like the
existing matching in `src/workflow/list.rs`); no match → unassigned. Agents
come from `App.all_agents`, which `refresh()` already reloads every cycle
independent of the active tab.

Agent set (decided for this view, from "all … on the host"): unified mode
ignores the persisted session scope (`scope_mode`) and the persisted
`hide_stale` setting; every reconciled agent is placed. The Agents tab keeps
both settings when it is turned back on.

Sub-row order inside a worktree and inside the outside group: 💬 waiting,
🤖 working, ⏳ background, ✅ done, no status last; ties by most recent
activity first.

Sub-row content: an indent marker (`└`), the status icon with the same
icon/colour/spinner rules as the Agents tab status display, the elapsed time
in the Agents tab format, then the pane title. Outside-group sub-rows append
the directory in parentheses. The text starts in the first column and may
span the row; columns that only make sense for worktrees stay empty. Stale
agents (existing staleness rule) are drawn dimmed, not hidden.

In unified mode the worktree row's `agent` summary column is not shown (the
sub-rows replace it), whatever `worktree_columns` says.

### Selection and keys

- `j`/`k`, arrows, `Ctrl-n`/`Ctrl-p` and the mouse move over every
  selectable row (worktree rows, agent sub-rows, orphan rows); the group
  header is skipped.
- Enter: agent sub-row → that agent's pane (the existing
  `switch_to_pane_and_track`); worktree row → its first agent sub-row's pane,
  else the existing window jump. This also fixes today's exact-path match,
  which misses agents working in a subdirectory of the worktree.
- Double-click jumps like Enter; right-click on a sub-row offers the same
  actions as its parent worktree row (or none in the outside group).
- Worktree actions (`r`, `c`, `o`, `O`, `b`, the `i`/`l` panels and their
  content) on an agent sub-row act on its parent worktree. In the outside
  group they do nothing and show a short status message
  ("not in a worktree").
- `1`-`9` keep jumping to the n-th worktree row (agent sub-rows are not
  numbered).
- Filter `/`: a worktree row is shown when it matches as today **or** one of
  its agents' titles contains the filter; it always carries all its
  sub-rows. Outside-group agents are shown when the filter is empty or their
  title or directory contains it.
- Project picker `p`: with "all projects" (or when no workspaces are
  configured, i.e. single-repository mode) the outside group is shown. With a
  specific project picked, only that project's worktrees and their agents are
  shown; the outside group and agents of other projects' worktrees are
  hidden. Assignment therefore runs against **all** discovered worktrees
  before the picker and filter are applied, so an agent of a hidden worktree
  never falls into the outside group.

### Documentation

`FORK.md` (fork features), the dashboard guide (unified view, the new key,
what happened to the Agents tab), the configuration reference for
`dashboard.agents_tab`, and one bullet in the kit's `docs/workmux.md` fork
section.

## Left to the builder

- How rows are modelled (a display-row index with a mapping to worktree,
  agent, orphan and header entries, keeping `1`-`9` on worktree rows;
  extending the existing worktree/orphan `RowKind`
  with agent sub-rows and a group header is the natural fit) and how the
  selection is preserved across refreshes (by pane id for sub-rows, by path
  for worktree rows).
- Exact truncation of titles and directories at narrow widths.
- Tests: assignment (deepest match, nested worktree, subdirectory,
  unassigned), ordering, row-kind mapping and navigation skipping the header,
  Enter targets, actions from sub-rows, filter and project-picker rules, and
  the `agents_tab: true` path unchanged; a manual check in the real
  dashboard before merge.
