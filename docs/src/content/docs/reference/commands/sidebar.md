---
title: "sidebar"
description: Control a live agent status sidebar in tmux
---

Controls a live agent status sidebar on the left or top edge of tmux windows. By default, each sidebar pane shows active agents across all tmux
sessions with live status updates. Use `workmux sidebar filter session` to show
only agents in the current tmux session.

```bash
workmux sidebar                         # Toggle sidebar on/off (all sessions)
workmux sidebar on                      # Ensure the global sidebar is running
workmux sidebar off                     # Ensure the global sidebar is stopped
workmux sidebar --session on            # Ensure it is running in this session
workmux sidebar --session off           # Ensure it is stopped in this session
workmux sidebar on --position top       # Enable with a top sidebar
workmux sidebar on --width 40           # Set left sidebar width in columns
workmux sidebar on --height 3           # Set top sidebar height in rows
```

## What it shows

Each agent row displays:

- Status icon (working/waiting/done with spinner animation)
- Project and worktree name (e.g. `myproject/fix-bug`)
- Elapsed time since last status change
- Git diff stats, plus optional pull request number and check status in custom templates

## Keybindings

| Key     | Action                    |
| ------- | ------------------------- |
| `j`/`k` | Navigate up/down          |
| `Enter` | Run the configured action |
| `o`     | Focus the agent pane      |
| `g`/`G` | Jump to first/last        |
| `v`     | Toggle layout mode        |
| `f`     | Toggle session filter     |
| `V`     | Cycle named views         |
| `z`     | Toggle sleeping on agent  |
| `t`     | Toggle grouping           |
| `h`/`l` | Unfold/fold current group |
| `s`     | Toggle the current fold   |
| `S`     | Fold/unfold every group   |
| `?`     | Show these keys           |
| `q`     | Open quit confirmation    |

## Mouse support

With tmux mouse mode enabled (`set -g mouse on`), click an agent row or top-bar
chip to jump to its pane, or scroll to navigate the list.

## Navigation commands

Switch between agents from any tmux pane, in the same order shown in the
sidebar. Navigation uses the same filter mode as the sidebar view, so it cycles
through all sessions by default:

| Command                         | Action                                               |
| ------------------------------- | ---------------------------------------------------- |
| `workmux sidebar next`          | Switch to the next agent (wraps)                     |
| `workmux sidebar prev`          | Switch to the previous agent (wraps)                 |
| `workmux sidebar jump <N>`      | Jump to the Nth agent (1-indexed)                    |
| `workmux sidebar filter`        | Toggle session filter (none/session)                 |
| `workmux sidebar filter <MODE>` | Set filter mode: `none`/`all` or `session`/`project` |
| `workmux sidebar group`         | Toggle grouping between grouped and flat             |
| `workmux sidebar group <MODE>`  | Set grouping: `none`/`off`, `project` or `session`   |
| `workmux sidebar group --clear` | Drop the runtime grouping and follow config again    |

### Example tmux keybindings

```bash
# Alt+j / Alt+k to cycle agents (no prefix needed)
bind -n M-j run-shell "workmux sidebar next"
bind -n M-k run-shell "workmux sidebar prev"

# Alt+1..9 to jump directly
bind -n M-1 run-shell "workmux sidebar jump 1"
bind -n M-2 run-shell "workmux sidebar jump 2"
bind -n M-3 run-shell "workmux sidebar jump 3"
# ...

# Or with prefix key (avoids terminal conflicts)
bind C-j run-shell "workmux sidebar next"
bind C-k run-shell "workmux sidebar prev"
```

## Configuration

```yaml
stale_after: 5h # dashboard and sidebar timeout; default: 1h

sidebar:
  position: left # "left" (default) or "top"
  width: 40 # left width in columns (default: "10%", clamped 25-50)
  # width: "15%"
  layout: tiles # left only: "compact" or "tiles" (default)
  enter_action: focus # "focus" (default) or "select"
  git_status: true # collect Git status and GitHub PR/check data (default)
  dim_stale: true # dim stale agents using stale_after
  group_by: project # "project" or "session"; unset keeps one flat list
  collapse_stale: true # while grouped, fold stale agents behind a toggle
```

Set `enter_action: select` to show the selected agent's window while keeping
focus in its sidebar, so `j`/`k` can continue scanning agents. Press `o` to
focus the selected agent's pane. If the target window has no sidebar, Enter
falls back to focusing the agent pane. Mouse clicks always focus the pane.

For a horizontal top bar:

```yaml
sidebar:
  position: top
  height: 3 # top height in rows
  horizontal:
    item_width: 24 # horizontal chip width in columns (default, clamped 12-80)
  templates:
    horizontal:
      - "{status_icon} {primary} {pane_suffix} {fill} {elapsed}"
      - "{secondary} {fill} {git_stats}"
      - "{pane_title}"
```

Set `sidebar.git_status: false` in the global config to stop the sidebar daemon
from watching worktrees and collecting Git status. Git status, pull request, and
check tokens then render empty. This setting is daemon-wide and does not affect
the dashboard's independent Git status updates.

Explicit width values bypass the default 25-50 column clamp (minimum 1
column). Layout preference can also be toggled at runtime with `v` and is
persisted across restarts. Height only applies to `position: top`; set it as a
row count for the number of horizontal lines you want to show. The top bar uses a
horizontal chip layout, so `v` has no effect there. Horizontal templates render
as many configured lines as the current height allows. `horizontal.item_width`
controls each chip width and is clamped between 12 and 80 columns. Position
changes take effect when the sidebar is enabled. Use
`workmux sidebar on --position top` or `--position left` to override the
configured placement. Width and height also accept percentages, such as
`workmux sidebar on --width 15%`. Repeating `on` with appearance options applies
them without creating duplicate panes. The `off` action rejects position and
dimension flags because they have no effect while stopping the sidebar.

## How it works

When enabled, a background daemon polls tmux state every 2 seconds and pushes
snapshots to each sidebar pane over a Unix socket. The sidebar creates a tmux
pane on the configured edge of every existing window. A tmux hook
(`after-new-window`) ensures newly created windows also get a sidebar
automatically.

Running `workmux sidebar` again disables the sidebar globally, killing all
sidebar panes, the daemon, and removing hooks. `workmux sidebar on` and
`workmux sidebar off` provide idempotent alternatives for scripts and tmux
configuration.

### Session-scoped mode

By default, the sidebar appears in all tmux sessions. Use `--session` to scope
it to the current session only, leaving other sessions untouched:

```bash
workmux sidebar --session on   # Ensure it is enabled in the current session
workmux sidebar --session off  # Ensure it is disabled in the current session
```

You can enable session-scoped sidebars in multiple sessions independently. Each
session can be toggled or explicitly enabled and disabled without affecting
others.

If the global sidebar is already active, `workmux sidebar --session` hides the
sidebar in the current tmux session only. Run it again to show the sidebar in
that session again while other sessions remain globally managed.

Starting global mode still replaces any session-scoped sidebars.

## Limitations

- tmux only (other backends are not supported yet)

## Example tmux binding

```bash
bind C-t run-shell "workmux sidebar"
```

## Named views

Define `sidebar.views` in your **global** config (`~/.config/workmux/config.yaml`,
or under `$XDG_CONFIG_HOME`). Project configs do not override views. Views scope
which agents appear; `sidebar.group_by` still controls how those rows are grouped.

```yaml
sidebar:
  views:
    - name: Named sessions
      sessions: [foo, bar, xyz]
    - name: Code
      project_dirs: [~/code]
    - name: Work
      project_dirs: [~/work]
    - name: Other
      other: true
```

Press **`V`** (uppercase) to cycle **All → Named sessions → Code → Work → Other →
All**. Lowercase `v` still changes the layout. The active view appears above the
rows in both left and top sidebars. Empty views show “No matching agents”; they
do not fall back to All. A top sidebar needs at least two rows to show both the
scope and agents.

### Matching and selection

- Session names are exact and case-sensitive: `foo` does not match `foobar` or
  `Foo`. No glob or regular-expression syntax is used.
- `project_dirs` matches a directory itself and its descendants, not siblings
  with the same prefix: `~/code` does not match `~/code-old`.
- Paths must be absolute or begin with `~/` (`~` alone is also supported).
  Home expansion uses the user running workmux. Existing paths resolve symlinks;
  other paths are normalized lexically. Environment variables are not expanded.
- A project's location is its main Git worktree root, including for linked
  worktrees stored elsewhere. For bare repositories it is the bare root. If Git
  discovery fails (including non-Git agents), the recorded agent working
  directory is used instead. Git discovery is cached for up to 30 seconds.
- Entries within a list, and `sessions` and `project_dirs` within a view, use
  **OR**. Views may overlap; there is no first-match assignment.
- `other: true` matches agents excluded by **every** regular view, including
  session-name filters. It cannot have filters of its own. At most one Other
  view may be configured; its name and position are your choice. Without it,
  unmatched agents are available only in All.
- All is always first, followed by configuration order. Names must be unique,
  nonempty, single-line, and have no leading/trailing whitespace. `All` is
  reserved. Each regular view needs at least one filter.
- Selection is shared by all sidebars in the current tmux server and survives
  sidebar off/on, but not a tmux server restart. Initial selection is All. If the
  selected view is removed or renamed on config reload, the scope becomes All.
- The `f` / `sidebar filter session` filter is independent and **intersects** the
  selected view with each sidebar's host session. Selecting All does not clear
  the session filter. `sidebar --session` still controls where sidebars exist,
  not which named view is selected.
- `next`, `prev`, and `jump` use the scoped agent inventory after the daemon
  refreshes. Existing stale-agent navigation rules still apply.
- With no views configured, the sidebar appearance and existing filters are
  unchanged; cycling views has no effect on the agent list.

### CLI and popup chooser

```bash
workmux sidebar view --list             # All, then configured names, one per line
workmux sidebar view 'Named sessions'   # Select an exact name
workmux sidebar view Code
workmux sidebar view All                # Clear only the named scope
workmux sidebar view                    # Cycle, like V
```

Listing does not require a running sidebar or tmux server. Selection requires
tmux but can be made before starting the sidebar. Unknown names fail without
changing the selection. Use `workmux sidebar view -- "$name"` in scripts to
support names beginning with a dash.

The repository includes a Bash chooser, `scripts/workmux-sidebar-view`, which
lists views by number and applies the selection. It requires only Bash and
workmux, not fzf. From a workmux checkout, install it:

```bash
mkdir -p ~/.local/bin
cp scripts/workmux-sidebar-view ~/.local/bin/workmux-sidebar-view
chmod +x ~/.local/bin/workmux-sidebar-view
```

Add this to `~/.tmux.conf`, then reload the tmux config:

```tmux
bind-key V display-popup -E -w 60 -h 16 '~/.local/bin/workmux-sidebar-view'
```

Press **prefix + V**, type a view number, and press Enter. Ctrl-D cancels without
changing the view. Ensure `workmux` is on the PATH inherited by the tmux server.
