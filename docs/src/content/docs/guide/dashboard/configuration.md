---
title: "Configuration"
description: Customize dashboard commands and layout
---

The dashboard can be customized in your `.workmux.yaml`:

```yaml
dashboard:
  commit: "Commit staged changes with a descriptive message"
  merge: "!workmux merge"
  layout: stacked
  preview_size: 60
  agent_columns:
    [number, project, worktree, git, pr, window, status, time, title]
  worktree_columns: [number, project, worktree, git, pr, mux, age, agent]
  close_on_jump: true
  input_exit_key: Esc
```

The `commit` and `merge` values are text sent to the agent's pane. Use the `!` prefix to run shell commands (supported by Claude, Gemini, and other agents).

## Defaults

| Option                   | Default value                                                | Description                                                                              |
| ------------------------ | ------------------------------------------------------------ | ---------------------------------------------------------------------------------------- |
| `commit`                 | `Commit staged changes with a descriptive message`           | Natural language prompt                                                                  |
| `merge`                  | `!workmux merge`                                             | Shell command via agent                                                                  |
| `layout`                 | `stacked`                                                    | `stacked` (table above preview) or `side_by_side` (preview left, table right)            |
| `preview_size`           | `60`                                                         | Preview height or width as percentage (10-90)                                            |
| `agent_columns`          | `[number, agent, project, worktree, git, pr, status, time, title]` | Agents table columns, in display order                                                   |
| `worktree_columns`       | `[number, project, worktree, git, pr, env, mux, age, agent]` | Worktree table columns, in display order                                                 |
| `worktree_extra_columns` | unset                                                        | Opt-in worktree columns inserted after `env` (see [worktree columns](#worktree-columns)) |
| `workspaces`             | unset                                                        | Show every git repository one or two levels below these directories (global config only) |
| `worktree_info_command`  | unset                                                        | Command printing per-worktree environment info as JSON (global config only)              |
| `worktree_info_interval` | `15`                                                         | Seconds between runs of the info command, minimum `2` (global config only)               |
| `close_on_jump`          | `true`                                                       | Close the dashboard after a jump                                                         |
| `input_exit_key`         | `Esc`                                                        | Key to exit input mode (`Esc` or `Ctrl+<letter>`)                                        |

## Input mode exit key

`input_exit_key` accepts `Esc` (the default) or `Ctrl+<letter>` (`a`–`z`), case-insensitively. For example, to exit input mode with `Ctrl+q` and send `Esc` to the agent:

```yaml
dashboard:
  input_exit_key: Ctrl+q
```

The exit key is consumed by the dashboard, not sent to the agent. Escape bindings in other dashboard modes are unchanged. Project configuration overrides the global setting.

Choose a key your terminal delivers distinctly. Some Ctrl-letter keys are indistinguishable from Tab, Enter, or Backspace, and terminal flow control can intercept `Ctrl+q` or `Ctrl+s`.

## Staying open after a jump

Jumping to an agent or worktree closes the dashboard. This includes `Enter`, `1`-`9`, `Bksp`, command-palette jumps, and double-clicking a row. Set `close_on_jump: false` to keep the dashboard open while moving between agents and worktrees:

```yaml
dashboard:
  close_on_jump: false
```

The jump still updates the pane history used by `Bksp`. The `p` peek action always keeps the dashboard open and does not update pane history. Multiplexers such as Zellij that keep the dashboard open after jumps retain that behavior regardless of this setting.

## Columns

`agent_columns` sets which columns the agents table shows and in what order. For example, to read the title first and keep the elapsed time out of the way:

```yaml
dashboard:
  agent_columns:
    [number, window, status, title, project, worktree, git, pr, time]
```

| Column      | Content                                          |
| ----------- | ------------------------------------------------ |
| `number`    | Jump key of the row, shown under the `#` header  |
| `agent`     | Agent kind, such as Claude, Codex or Vibe        |
| `project`   | Project name                                     |
| `worktree`  | Worktree name, with a pane number when it splits |
| `git`       | Branch state, staged and unstaged changes        |
| `pr`        | Pull request number and check status             |
| `pr_title`  | Pull request title                               |
| `pr_issues` | Issues closed by the pull request                |
| `window`    | Multiplexer window index                         |
| `status`    | Agent status icons                               |
| `time`      | Time since the last status change                |
| `title`     | Agent session title                              |
| `prompt`    | Latest prompt you sent to the agent              |

A column left out of the list is not rendered, so `agent_columns: [worktree, status, title]` gives a table of just those three. Repeating a column has no effect, and an empty list falls back to the default order.

The `agent` column names the agent in its brand color: the kind recorded by the status hooks, otherwise the one recognized from the launch command or the pane's foreground command (`vibe-rs` counts as Vibe). An unrecognized agent shows a dimmed `-`. Builds without this column reject `agent` in `agent_columns`; it is in the default list, so a config that leaves `agent_columns` unset shows it without naming it.

Dropping `number` hides the jump key, and `1`-`9` still jump to the first nine rows. The `pr` column appears only while at least one agent has a pull request or checks to report, wherever it is placed in the list. The opt-in `pr_title` and `pr_issues` columns stay visible and leave the cell blank when the branch has no pull request or closing issues. `pr_issues` uses GitHub's closing issue references, not arbitrary issue mentions in pull request text. Same-repository issues appear as `#123`; cross-repository issues include the repository, such as `owner/repo#123`. The opt-in `window` column shows the tmux window index used by `prefix + <n>`; backends without window indexes leave the cell blank. A trailing `title`, `prompt`, `pr_title`, or `pr_issues` takes the width left over by the other columns; anywhere else it sizes to its content, and the leftover width sits at the right edge of the table.

#### Prompt column

The opt-in `prompt` column shows the last prompt you sent to each agent, on one line and cut with `…` to fit. Status hooks store the prompt only while the `prompt` column or a [`{prompt}` sidebar token](/guide/sidebar/customization/#tokens) is configured, evaluated with the agent's project configuration. Removing both stops capture, and each agent's stored prompt is deleted at its next status update.

Prompts come from Claude Code, Codex, Gemini CLI, Copilot CLI, Grok, OpenCode, pi, and oh-my-pi. Prompts sent to subagents and messages the agent injects itself are ignored. A prompt with no text, such as an image alone, keeps the previous prompt. A prompt passed when the agent starts, such as `workmux add -p`, may not be shown until you send the next one. OpenCode, pi, and oh-my-pi report prompts once `workmux setup` updates their status plugin.

### Worktree columns

`worktree_columns` controls the Worktrees tab independently of `agent_columns`. To hide Git status and give the other columns more room:

```yaml
dashboard:
  worktree_columns: [number, project, worktree, pr, mux, age, agent]
```

| Column        | Content                                                         |
| ------------- | --------------------------------------------------------------- |
| `number`      | Jump key of the row, shown under the `#` header                 |
| `project`     | Project name                                                    |
| `worktree`    | Worktree name, with the branch when different                   |
| `git`         | Branch state and uncommitted changes                            |
| `pr`          | Pull request number and check status                            |
| `pr_title`    | Pull request title                                              |
| `pr_issues`   | Issues closed by the pull request                               |
| `base`        | Base branch the diff is computed against (opt-in)               |
| `env`         | Environment info from `worktree_info_command`                   |
| `env_id`      | First 8 characters of the reported environment id (opt-in)      |
| `env:<label>` | Value of the reported detail with that label, such as `env:cpu` |
| `mux`         | Whether a multiplexer window is open                            |
| `age`         | Time since worktree creation                                    |
| `agent`       | Agent status summary                                            |

The list sets display order; omitted columns are hidden and duplicates are ignored. An absent or empty list uses the default order. Project configuration overrides the global list, and an explicit empty project list resets it to defaults.

A trailing `agent`, `pr_title`, or `pr_issues` fills the remaining width. Elsewhere it sizes to its content, leaving extra space at the right edge. The `pr` column is hidden until a worktree has a pull request or checks, except when it is the only configured column: like the agents table, it stays visible to avoid an empty table. The opt-in PR metadata columns use the same closing-reference and blank-cell behavior as the Agents table.

The `git` cell reads left to right: `→base` when the base is not `main` or `master`, the committed diff against the base (dimmed), the uncommitted changes after the diff icon (`⚠️` without Nerd Fonts) in bright colors, a conflict or rebase marker, then `↑N` commits not pushed and `↓N` commits to pull from the upstream branch (as of the last fetch). A branch with commits against its base but no upstream shows `no upstream`, since it has never been pushed. A directory outside any git repository, such as an agent started in your home directory, shows a dimmed `?`. The info panel shows the same comparison on its `Remote` line (`↑N ↓N`, `in sync`, `no upstream` or `detached`), under the `Base` line.

The opt-in `base`, `env_id` and `env:<label>` columns put the info panel data in the table. An `env:<label>` column shows the `details` entry whose `label` matches (several entries with the same label are joined with `, `), cut to 32 characters; the header is the label, capitalized, or in capitals when it has three letters or fewer (`env:cpu` is shown as `CPU`). `env_id` and each `env:<label>` column appear only while at least one row has a value. When the table is wider than the pane, these opt-in columns are dropped starting from the last one in the list, so their order is also their priority; the other columns are never dropped. Quote the entries in YAML when in doubt (`"env:cpu"`).

The opt-in columns can also be listed in `worktree_extra_columns`, which inserts them right after `env` (at the end when `env` is not shown), in the listed order, skipping any already in `worktree_columns`. Use it when the same config file is read by a workmux build without these columns or without `env`: such a build rejects unknown names in `worktree_columns`, but ignores the `worktree_extra_columns` key.

```yaml
dashboard:
  worktree_extra_columns: ["env:cpu", "env:mem", "env:disk", base, env_id, "env:console"]
```

`workspaces`, `worktree_info_command`, and `worktree_info_interval` are read only from the global config (`~/.config/workmux/config.yaml`). Setting them in a project's `.workmux.yaml` logs a warning and is ignored.

```yaml
dashboard:
  workspaces: ["~/code"]
  worktree_info_command: "my-env-tool status --json"
  worktree_info_interval: 15
```

Hiding `number` does not disable `1`-`9` jumps. Hiding `git` does not disable Git status updates or remove the selected worktree's Git details from the preview.

## Workspaces

With `workspaces` set, the Worktrees tab lists the worktrees of every git repository found one or two levels below the listed directories, plus the repositories of running agents. A repository is a directory whose `.git` is a directory; hidden directories and the inside of a repository are not searched. `~` is expanded.

The project name is the first path component below the workspace directory, so `~/code/app/main` is shown as `app`. When several repositories share that component, each gets its full relative path instead, such as `sync/one` and `sync/two`. Every worktree of a repository, and every agent working in it, shows the same name. Repositories outside the workspaces keep the usual name derived from their path.

Press `p` on the Worktrees tab to pick a project. The first entry, **All projects**, lists every repository; any other entry narrows the tab to that repository. Row actions (open, close, remove, base branch, add worktree) act in the selected row's repository and use that repository's configuration. On the Agents tab, the command palette's **Add worktree** adds to the selected agent's repository.

Without `workspaces` the tab lists the current repository only, the picker has no **All projects** entry, and project names are derived from each worktree's path.

## Worktree info provider

`worktree_info_command` adds environment information from your own tooling to the Worktrees tab. The command runs with `sh -c` in the background: when the dashboard opens, then every `worktree_info_interval` seconds (default `15`, minimum `2`). Only one run happens at a time, and a run that takes longer than 10 seconds is killed. The command prints JSON on stdout; stderr is ignored.

```json
{
  "schemaVersion": 1,
  "worktrees": [
    {
      "path": "/home/me/code/app/feature-x",
      "status": { "label": "ready", "tone": "success" },
      "id": "00000000-0000-4000-8000-000000000001",
      "details": [
        { "label": "backend", "value": "http://127.0.0.1:8080" },
        { "label": "console", "value": "http://dev-host.example:8081" }
      ]
    }
  ],
  "orphans": [
    {
      "project": "app",
      "name": "old-feature",
      "path": "/home/me/code/app/old-feature",
      "status": { "label": "unknown", "tone": "warning" },
      "id": "00000000-0000-4000-8000-000000000002",
      "hint": "my-env-tool forget 00000000"
    }
  ]
}
```

| Field                      | Required | Content                                                 |
| -------------------------- | -------- | ------------------------------------------------------- |
| `schemaVersion`            | yes      | Must be `1`; any other value counts as a failed run     |
| `worktrees`                | no       | Entries for existing worktrees                          |
| `worktrees[].path`         | yes      | Worktree path                                           |
| `worktrees[].status.label` | yes      | Text shown in the `env` column and the info panel       |
| `worktrees[].status.tone`  | no       | `success`, `warning`, `error`, `info` or `muted`        |
| `worktrees[].id`           | no       | Identifier; the info panel shows its first 8 characters |
| `worktrees[].details`      | no       | `label` and `value` pairs, one info panel line each     |
| `orphans`                  | no       | Environments whose worktree no longer exists            |
| `orphans[].project`        | yes      | Project shown in the row                                |
| `orphans[].name`           | yes      | Name shown with `(missing)`                             |
| `orphans[].path`           | yes      | Path of the missing worktree                            |
| `orphans[].status`         | yes      | As for worktrees                                        |
| `orphans[].id`, `.details` | no       | As for worktrees                                        |
| `orphans[].hint`           | no       | Last line of the info panel, such as a cleanup command  |

- An unknown or missing `tone` is shown as `muted`. Tones use the theme's success, warning, danger, info and dimmed colors.
- Unknown fields are ignored.
- Paths are compared after canonicalization, so a symlinked path matches the worktree it points to.
- When several entries have the same path, the first one wins. An orphan whose path matches a listed worktree is shown on that worktree.
- A failed run (non-zero exit, timeout, invalid JSON, another `schemaVersion`) keeps the data of the last successful run on screen.

The `env` column appears once a report has arrived; a worktree without an entry shows `-`. The info panel adds an `Env` line with the status and the short id, followed by the details. The same id and details can be shown as table columns with `env_id` and `env:<label>` (see [columns](#worktree-columns) above).

Orphans are listed after the worktrees as `name (missing)`, with only the project and the status filled in. They follow the project picker (only orphans whose `project` equals the picked project's name) and the text filter (matching project or name). You can select an orphan to see its path, details and hint in the info panel, but it has no jump number and row actions such as open, close, remove or add worktree do nothing on it.

## Layout

The default `stacked` layout places the table above the preview. To give the preview the full content height, with the table on the right:

```yaml
dashboard:
  layout: side_by_side
```

The layout applies to both the Agents and Worktrees tabs. The tab header and footer remain above and below the content. Backends without preview support show only the table regardless of this setting.

Side-by-side leaves less width for each pane. On narrow terminals, use the stacked layout or reduce the table's configured columns. Captured agent output is clipped to the preview width, not rewrapped.

## Preview size

The `preview_size` option controls the preview as a percentage of the content area: height in `stacked` layout, width in `side_by_side` layout. A higher value means more space for the preview and less for the table.

You can also adjust the preview size interactively with `+`/`-` keys. These adjustments persist across dashboard sessions in the workmux state directory.

The CLI flag `--preview-size` (`-P`) overrides both the config and saved preference for that session.

## Examples

```yaml
# Use Claude skill for merge (see skills guide)
dashboard:
  merge: "/merge"

# Custom shell commands
dashboard:
  merge: "!workmux merge --rebase --notification"

# Natural language prompts
dashboard:
  commit: "Create a commit with a conventional commit message"
  merge: "Rebase onto main and run workmux merge"
```

## Using skills

For complex workflows, [skills](/guide/skills/) are more powerful than simple prompts or shell commands. A skill can encode detailed, multi-step instructions that the agent follows intelligently.

```yaml
dashboard:
  merge: "/merge"
```

See the [skills guide](/guide/skills/) for the `/merge` skill you can copy.
