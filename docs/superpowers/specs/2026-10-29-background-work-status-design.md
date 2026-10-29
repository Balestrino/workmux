# Background Work Status — Design

Date: 2026-10-29
Fork: `Balestrino/workmux`, branch `feat/background-status` off fork `main`
(`a40dcd19`, upstream v0.1.277 plus the fork features in `FORK.md`)
Agents in scope: Claude Code 2.1.312, Codex CLI 0.164.0

## Intent

workmux shows an agent as working (🤖), waiting (💬) or done (✅) from the
status hooks the agent runs. Both agents we use can leave work running after
the main turn has ended, and in that case the status is wrong:

- **Claude Code** marks the agent `done` at the end of the main turn even when
  a background subagent, a background shell command or a Monitor is still
  running. A background subagent flips it back to `working` only on its next
  tool call, so the agent shows ✅ while that subagent thinks or writes, and a
  background shell command or Monitor shows ✅ for its whole run.
- **Codex** has the opposite problem: when the main turn ends before its
  subagents finish, the last `SubagentStop` leaves the agent `working`
  forever, because Codex does not wake the main agent afterwards.

The user wants to see at a glance that an agent is not busy with its own turn
but has work running in the background, with a **distinct icon**, for both
Claude Code and Codex.

## Evidence

Collected on 2026-10-29 with hook payloads captured in isolated sessions (a
separate tmux server and a scratch project with capture hooks), plus the
transcripts and the workmux log of real sessions.

Claude Code 2.1.312, one turn that launched a background Bash command, a
background Agent and a Monitor, then ended:

| Event | Payload facts |
| --- | --- |
| `PostToolUse` Bash, `run_in_background: true` | `tool_response.backgroundTaskId` (`"b2djt8l8t"`) |
| `PostToolUse` Agent, background | `tool_response`: `isAsync: true`, `status: "async_launched"`, `agentId` |
| `SubagentStart` | `agent_id` equal to the Agent `agentId` above |
| `PostToolUse` Monitor | `tool_response`: `taskId`, `timeoutMs`, `persistent` |
| `PostToolUse` from inside a subagent | carries `agent_id` |
| `Stop` | main turn only; no `agent_id` |
| `SubagentStop` | `agent_id`; also fires after **every** `Stop` for internal agents that never had a `SubagentStart` |
| completion of each background item | `UserPromptSubmit` whose `prompt` starts with `<task-notification>` and holds `<task-id>ID</task-id>` (the id from the rows above) and `<status>` |

Every payload has `hook_event_name`. A `<task-notification>` that arrives
while the main agent is idle wakes it: `UserPromptSubmit` → turn → `Stop`.
One that arrives **during** a turn is not a `UserPromptSubmit`: Claude queues
it into the running turn as a `queued_command` attachment in the transcript,
with the same `<task-id>` and `<status>` text. In this design session 11 of 14
notifications arrived that way. Intermediate Monitor events are notifications
without `<status>`. The user-level Claude hooks
installed by the kit (`SessionStart`, `UserPromptSubmit`, `Notification`,
`PostToolUse`, `Stop`) already deliver all the events above that matter;
`SubagentStart`/`SubagentStop` are **not** registered for Claude.

Codex 0.164.0, one turn that spawned two subagents without waiting:

| Event | Payload facts |
| --- | --- |
| `PostToolUse` `collaborationspawn_agent` | `tool_response` `{"task_name": ...}`, no agent id |
| `SubagentStart` | `agent_id`, `agent_type` |
| `Stop` | main turn only, fired while both subagents were running |
| `PostToolUse` from a subagent | carries `agent_id` |
| `SubagentStop` | `agent_id`, `agent_transcript_path` |
| after the last `SubagentStop` | nothing: no `UserPromptSubmit`, no further `Stop` |

The workmux state stayed `working` after the test. The Codex hooks installed
by the kit already include `SubagentStart` and `SubagentStop` (both
`set-window-status working`).

## Goals

1. An agent whose main turn has ended while work it launched is still running
   shows a distinct **background** state and icon (default ⏳) in the tmux
   window name, the dashboard (Agents tab, Worktrees `Agent` column), the
   sidebar and `workmux list`.
2. The agent becomes `done` when the main turn has ended **and** no tracked
   background work is left, for both agents. For Codex this fixes the stuck
   `working`.
3. No change to the hooks the kit installs: Claude is handled from
   `PostToolUse` and `UserPromptSubmit`, Codex from `SubagentStart` and
   `SubagentStop`, all already registered.

## Non-goals

- Fixing the existing precedence between `waiting` and later `working`
  events: as today, any `working` event (including a subagent's) replaces a
  stored `waiting`.
- Detecting background processes the agent did not launch through its tools
  (for example a server started with `&` inside a foreground command).
- Agent teams (`TaskCreated`, `TaskCompleted`, `TeammateIdle`) and agents
  other than Claude Code and Codex. Their behaviour stays as today.
- Showing which items are running or their count in the UI beyond the icon.
- Sandbox guests (`run_via_rpc`): they keep today's behaviour.

## Anti-goals

- An agent stuck in the background state with nothing running. Every tracked
  item must expire, and expired items must stop counting even if no hook
  arrives.
- Flicker of ✅ for Claude when a background item finishes: the
  `<task-notification>` that removes the last item also starts a turn, so the
  agent must go straight to `working`, never through `done`.
- Breaking consumers of `workmux status` (the kit's `overview` reads it):
  the `status` labels stay `working`, `waiting`, `done`.

## Design

### State

`AgentState` (one JSON file per agent) gains two fields, both
`#[serde(default)]` and omitted when empty so older files load unchanged:

- `turn_active: bool`: the main turn is running. Set by `UserPromptSubmit`
  and by `PostToolUse` without `agent_id`; cleared by `Stop`.
- `background: Vec<BackgroundItem>` with `id: String`, `kind`
  (`subagent` | `shell` | `monitor`), `started_ts: u64`, `expires_ts: u64`
  and `transcript_offset: Option<u64>` (length of the hook's
  `transcript_path` when the item was added, Claude only).

`AgentStatus` keeps its three values. **Background is derived**, not stored:
an agent is in the background state when its stored status is `Working`,
`turn_active` is false and at least one item has `expires_ts` in the future.
This keeps sorting, filters, `last-done` and every exhaustive `match` on
`AgentStatus` unchanged, and keeps the `workmux status` labels.

Writes without `turn_active` information (sandbox RPC, `register-agent`,
title updates) preserve both fields from the existing record, under the same
same-process rule that already preserves the prompt and `cwd`.

### Event handling

`set-window-status` reads `hook_event_name`, `agent_id`, `tool_name`,
`tool_input` and `tool_response` from the hook input (all optional, parsed
leniently like the existing fields). The command it was invoked with
(`working`, `waiting`, `done`) stays the fallback when `hook_event_name` is
missing, so other agents behave as today.

| Event | Effect |
| --- | --- |
| `SessionStart` (`register-agent`) | clear `background`, `turn_active = false` (new or resumed conversation: old notifications will not arrive) |
| `UserPromptSubmit` | `turn_active = true`; if the prompt is a `<task-notification>` with a terminal `<status>`, remove the item with that `<task-id>`; status `working` |
| `PostToolUse` without `agent_id` | `turn_active = true`; add an item when the response launched background work, remove one when the tool stopped a task (below); status `working` |
| `PostToolUse` with `agent_id` | refresh the expiry of the subagent item with that id **if tracked**; never add one (a foreground Claude subagent has no completion event and would linger); status follows the derived rule |
| `SubagentStart` | add a `subagent` item for `agent_id`; status follows the derived rule |
| `SubagentStop` | remove the item for `agent_id` **only if tracked** (Claude's internal agents are ignored); then: `turn_active` or live items → stay `working`, otherwise `done` |
| `Notification` / `PermissionRequest` (`waiting`) | status `waiting`, items and `turn_active` unchanged |
| `Stop` | first remove the items completed during the turn (transcript scan below); then `turn_active = false`; status `working` (shown as background) if live items remain, otherwise `done` |

Background launches recognised in a main-turn `PostToolUse` (Claude):

| `tool_name` | Condition | Item |
| --- | --- | --- |
| `Bash` | `tool_response.backgroundTaskId` present (covers `run_in_background` and commands moved to the background by the user) | `shell`, id = that value |
| `Agent` (also `Task`) | `tool_response.status == "async_launched"` | `subagent`, id = `tool_response.agentId` |
| `Monitor` | `tool_response.taskId` present | `monitor`, id = that value |

Background stops recognised in a main-turn `PostToolUse` (Claude): `TaskStop`
or `KillShell` remove the item whose id is in `tool_input` (`task_id` or
`shell_id`). The killed task's own `<task-notification>` removes it too.

Ids are unique keys: adding an id that is already tracked only refreshes its
expiry, so `SubagentStart` and the Agent `PostToolUse` describe one item.

Transcript scan at `Stop` (Claude): for the items that have a
`transcript_offset`, read `transcript_path` from the smallest offset to the
end, at most the last 8 MiB, and remove every item whose `<task-id>` appears
in a `<task-notification>` with a terminal `<status>`. This catches the
notifications Claude delivered inside the turn. Codex items have no offset
and are removed only by `SubagentStop`.

Incremental re-scan: each item also stores how far the transcript has been
scanned, and every later hook of the same agent that has items with an offset
scans only the bytes appended since (same matching rule). This covers a
notification written after the `Stop` hook ran. Claude appends the
`queued_command` attachment before the API request that consumes it, so it
normally precedes `Stop`; the builder confirms this in the manual check.

A `<task-notification>` without `<status>`, or with `running`, is an
intermediate Monitor event and removes nothing. The terminal values seen in
Claude Code are not all documented; any other `<status>` value counts as
terminal.

### Expiry

| Kind | `expires_ts` |
| --- | --- |
| `subagent` | last activity (start, or its own `PostToolUse`) + 30 min |
| `monitor` | start + `timeoutMs` + 2 min; `persistent: true` → start + 24 h |
| `shell` | start + 12 h (Claude reports no timeout for background commands) |

Expired items are ignored by the derived state on every read and dropped on
the next write. The values are constants in this version (tunable later if
needed).

### Interruption detection

The sidebar daemon's `InactivityTracker` marks a `Working` agent as
interrupted after 10 s without pane output, and the dashboard reads that set
when the daemon runs. An agent in the background state is idle at its prompt
by design, so it is **excluded** from inactivity detection and from the
interrupted set; its correctness rests on the removal paths above and on the
expiry.

### Display

- New icon `status_icons.background`, default `⏳`, documented with the other
  icons.
- The hook that leaves the agent in the background state sets that icon on
  the tmux window, without auto-clear on focus (unlike ✅ and 💬): the work is
  still running. If items expire without a hook, the window icon stays ⏳
  until the next hook; the dashboard and sidebar already show the right state
  from the derived rule.
- Dashboard Agents tab, Worktrees `Agent` column, sidebar and `workmux list`
  render the background icon where they render the working icon today; the
  elapsed time keeps counting from the status timestamp.
- `workmux status` keeps `status: "working"` and adds `background: true` to
  entries in the background state.

### Kit and documentation

No kit code change: the needed hooks are already installed for both agents.
The fork's `FORK.md` and the dashboard and status-tracking docs describe the
background state; the kit's `docs/workmux.md` fork section gets one line about
it in the same PR as the next kit change.

## Left to the builder

- The exact module split (a small `background` module in `src/state/` is
  suggested) and how the derived rule is exposed to the renderers.
- Parsing details of `<task-notification>` (tolerate whitespace, JSON
  escaping in the transcript and surrounding `<system-reminder>` text; pair
  each `<task-id>` with the `<status>` of the same notification).
- Whether `last-done` and the sidebar's sort treat the background state as
  working (expected) — follow what the derived `Working` status gives.
- Tests: unit tests for each event row, the expiry rules, Claude's untracked
  `SubagentStop`, and the Codex stuck-`working` scenario replayed from the
  captured payloads; a manual check with a real Claude and Codex session in
  an isolated tmux server before merge.
