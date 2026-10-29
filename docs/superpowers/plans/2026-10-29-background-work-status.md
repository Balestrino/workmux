# Background Work Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show a distinct background state (⏳) while an agent's main turn has ended but work it launched is still running, and end in `done` only when that work is over, for Claude Code and Codex.

**Architecture:** A pure `src/state/background.rs` module holds the tracked items, the expiry rules, the `<task-notification>` parser, the transcript scan and the event transition. `set-window-status` turns the hook payload into a `HookEvent`; the transition runs under the existing agent-state lock and returns the stored status plus the derived background flag, which every renderer reads from `AgentPane.background`.

**Tech Stack:** Rust 2024, serde/serde_json, ratatui; tests with `cargo test --bin workmux`.

**Spec:** `docs/superpowers/specs/2026-10-29-background-work-status-design.md`

## Global Constraints

- Work in the worktree `/home/tore/workspaces/workmux/feat-background-status` (branch `feat/background-status`); build with `CARGO_TARGET_DIR=/home/tore/workspaces/workmux/primary-clone/target`.
- `AgentStatus` keeps exactly `Working`, `Waiting`, `Done`; background is derived: stored `Working` + `turn_active == false` + at least one item with `expires_ts > now`.
- New `AgentState` fields are `#[serde(default, skip_serializing_if = ...)]`; existing state files must load unchanged.
- `workmux status` keeps the labels `working`, `waiting`, `done` and adds `background: true` only for entries in the background state.
- Expiry: subagent = last activity + 1800 s; monitor = start + `timeoutMs`/1000 + 120 s, `persistent: true` → start + 86400 s; shell = start + 43200 s.
- Transcript scan reads at most the last 8 MiB (8 * 1024 * 1024 bytes).
- Default background icon `⏳`, config key `status_icons.background`; set on the tmux window with auto-clear **off**.
- Sandbox RPC (`src/sandbox/rpc.rs`) behaviour is unchanged (passes no event).
- Every commit ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; `cargo fmt --all -- --check` and `cargo clippy --all-targets` clean before each commit.

## Review Focus

- A notification delivered inside the turn and written to the transcript just before `Stop` → removed at `Stop`, agent ends `done` (Task 3 test `stop_scans_transcript_for_queued_notifications`).
- Claude's `SubagentStop` for internal agents after every `Stop` → ignored, never flips to `done`/background wrongly (Task 3 test `untracked_subagent_stop_is_ignored`).
- A foreground Claude subagent (tool calls with `agent_id`, no async launch) → never tracked, `Stop` gives `done` (Task 3 test `foreground_subagent_is_not_tracked`).
- An item whose completion never arrives → stops counting after expiry with no hook (Task 1 test `expired_items_do_not_count`).
- A transcript missing, unreadable or larger than 8 MiB → scan degrades without panicking and keeps items (Task 3 test `scan_tolerates_missing_and_huge_transcripts`).

---

### Task 1: Background state model

**Files:**
- Create: `src/state/background.rs`
- Modify: `src/state/mod.rs` (add `pub mod background;`), `src/state/types.rs` (`AgentState` fields, `to_agent_pane`), `src/multiplexer/types.rs` (`AgentPane.background`), every `AgentState { .. }` / `AgentPane { .. }` literal the compiler flags (test fixtures: add the new fields with defaults)

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "lowercase")] pub enum BackgroundKind { Subagent, Shell, Monitor }`
  - `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] pub struct BackgroundItem { pub id: String, pub kind: BackgroundKind, pub started_ts: u64, pub expires_ts: u64, #[serde(default, skip_serializing_if = "Option::is_none")] pub transcript_offset: Option<u64> }` — `transcript_offset` is the next byte to scan.
  - constants `SUBAGENT_IDLE_SECS: u64 = 1800`, `MONITOR_GRACE_SECS: u64 = 120`, `PERSISTENT_MONITOR_SECS: u64 = 86400`, `SHELL_SECS: u64 = 43200`
  - `pub fn has_live(items: &[BackgroundItem], now: u64) -> bool`
  - `pub fn prune_expired(items: &mut Vec<BackgroundItem>, now: u64)`
  - `pub fn upsert(items: &mut Vec<BackgroundItem>, item: BackgroundItem)` — same id: keep `started_ts`, take the later `expires_ts`, keep the existing `transcript_offset` if the new one is `None`
  - `pub fn remove(items: &mut Vec<BackgroundItem>, id: &str) -> bool`
  - `AgentState.turn_active: bool` (`#[serde(default, skip_serializing_if = "std::ops::Not::not")]`), `AgentState.background: Vec<BackgroundItem>` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`)
  - `impl AgentState { pub fn in_background(&self, now: u64) -> bool }` — the derived rule from Global Constraints
  - `AgentPane.background: bool` (`#[serde(default)]`), filled by `to_agent_pane` from `in_background(now)` with `now` = current Unix seconds

- [ ] **Step 1: Write the failing tests** in `src/state/background.rs` (`#[cfg(test)] mod tests`) and `src/state/types.rs`:
  - `upsert_keeps_start_and_extends_expiry`: upsert id `a` (started 10, expires 100), then id `a` (started 50, expires 200) → one item, `started_ts == 10`, `expires_ts == 200`.
  - `expired_items_do_not_count`: items expiring at 100 → `has_live(.., 99)` true, `has_live(.., 100)` false; `prune_expired(.., 100)` empties the vec.
  - `remove_reports_whether_tracked`: `remove(&mut v, "x")` false on empty, true after upsert of `x`.
  - `in_background_requires_working_idle_turn_and_live_item`: state with status `Working`, `turn_active false`, one item expiring at 100 → true at 50; false when `turn_active` true, when status `Done`, when `now` is 100.
  - `legacy_state_loads_without_background_fields`: the existing `legacy_state(...)` fixture deserializes with `turn_active == false`, empty `background`, and re-serializes without the keys `turn_active` and `background`.
- [ ] **Step 2: Run** `cargo test --bin workmux background` — expected: compile errors for the missing module and fields.
- [ ] **Step 3: Implement** the items above; `to_agent_pane` computes `now` with `SystemTime::now()` like the rest of `state/`.
- [ ] **Step 4: Run** `cargo test --bin workmux` — expected: all pass.
- [ ] **Step 5: Commit** `feat(state): track background work items per agent`.

### Task 2: Hook events and task notifications

**Files:**
- Modify: `src/command/set_window_status.rs` (`HookInput` fields, `HookInput::event`), `src/state/background.rs` (notification parser)
- Test: same files; payload fixtures copied from the captured sessions (shapes in the spec's Evidence tables)

**Interfaces:**
- Consumes: `BackgroundKind` (Task 1).
- Produces:
  - in `background.rs`: `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Launch { pub id: String, pub kind: BackgroundKind, pub timeout_secs: Option<u64>, pub persistent: bool }`
  - in `background.rs`: `#[derive(Debug, Clone, PartialEq, Eq)] pub enum HookEvent { SessionStart, PromptSubmit { prompt: Option<String> }, ToolUse { agent_id: Option<String>, launch: Option<Launch>, stopped: Option<String> }, SubagentStart { agent_id: String }, SubagentStop { agent_id: String }, Waiting, Stop }`
  - in `background.rs`: `pub fn terminal_task_ids(text: &str) -> Vec<String>` — for each `<task-notification>…</task-notification>` block (also when JSON-escaped, `\n` and `\"` present), the `<task-id>` when the same block has a `<status>` whose value is not `running`
  - in `set_window_status.rs`: `HookInput` gains `hook_event_name: Option<String>`, `tool_name: Option<String>` (both `lenient_string`), `tool_input: Option<serde_json::Value>`, `tool_response: Option<serde_json::Value>` (`#[serde(default)]`); `agent_id` already exists
  - `impl HookInput { fn event(&self) -> Option<HookEvent> }` — `None` when `hook_event_name` is missing or not one of `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `SubagentStart`, `SubagentStop`, `Notification`, `PermissionRequest`, `Stop`; `Notification` and `PermissionRequest` → `Waiting`; `SubagentStart`/`SubagentStop` without a non-empty `agent_id` → `None`
  - launch detection for `PostToolUse` without `agent_id`: `Bash` + `tool_response.backgroundTaskId` → `Shell`; `Agent` or `Task` + `tool_response.status == "async_launched"` + `agentId` → `Subagent`; `Monitor` + `tool_response.taskId` → `Monitor` with `timeout_secs = timeoutMs / 1000`, `persistent`; `tool_response` may arrive as a JSON string (Codex) — parse it when it is a string
  - stop detection: `TaskStop` or `KillShell` → `stopped` = `tool_input.task_id` or `tool_input.shell_id`

- [ ] **Step 1: Write the failing tests:**
  - `terminal_task_ids_pairs_id_with_status`: a text with two blocks, `<task-id>b1</task-id>…<status>completed</status>` and `<task-id>m1</task-id>…<event>tick</event>` (no status) → `["b1"]`; a block with `<status>running</status>` → excluded; `<status>killed</status>` → included.
  - `terminal_task_ids_reads_json_escaped_transcript`: the same block serialized with `serde_json::to_string` → `["b1"]`.
  - `event_detects_claude_background_launches`: the three captured `PostToolUse` payloads (Bash `backgroundTaskId: "b2djt8l8t"`; Agent `isAsync/async_launched/agentId: "a8565d3b98e2c0bb8"`; Monitor `taskId: "b84rmmfs7", timeoutMs: 60000, persistent: false`) → `ToolUse { agent_id: None, launch: Some(..kind, id..), stopped: None }`, Monitor `timeout_secs == Some(60)`.
  - `event_reads_subagent_tool_use_and_lifecycle`: Codex `PostToolUse` with `agent_id` → `ToolUse { agent_id: Some(..), launch: None, .. }`; `SubagentStart`/`SubagentStop` with `agent_id` → the variants; `SubagentStop` with empty `agent_id` → `None`.
  - `event_maps_waiting_stop_and_unknown`: `Notification` → `Waiting`, `PermissionRequest` → `Waiting`, `Stop` → `Stop`, missing `hook_event_name` → `None`, `TaskStop` with `tool_input.task_id: "b1"` → `stopped: Some("b1")`.
- [ ] **Step 2: Run** `cargo test --bin workmux set_window_status background` — expected: compile errors.
- [ ] **Step 3: Implement** the parser and `HookInput::event`.
- [ ] **Step 4: Run** `cargo test --bin workmux` — expected: all pass.
- [ ] **Step 5: Commit** `feat(status): parse hook events and task notifications`.

### Task 3: Event transition and transcript scan

**Files:**
- Modify: `src/state/background.rs`

**Interfaces:**
- Consumes: Task 1 items, Task 2 `HookEvent`, `Launch`, `terminal_task_ids`.
- Produces:
  - `pub fn transition(turn_active: &mut bool, items: &mut Vec<BackgroundItem>, event: &HookEvent, previous: Option<AgentStatus>, transcript: Option<&Path>, now: u64) -> AgentStatus` — returns the status to store (`Working`, `Waiting` or `Done`); `previous` is the stored status before the event
  - `fn scan_transcript(items: &mut Vec<BackgroundItem>, transcript: &Path)` — for items with `transcript_offset`, read from the smallest offset (clamped to the last 8 MiB) to EOF, remove items listed by `terminal_task_ids`, then set every remaining offset to the file length; any I/O error leaves items untouched
- Rules (spec "Event handling"; implement exactly):
  - always `prune_expired` first; when `transcript` is `Some` and any item has an offset, run `scan_transcript` before applying the event (incremental re-scan on every hook)
  - `SessionStart` → clear items, `turn_active = false`, return `Done`
  - `PromptSubmit` → remove `terminal_task_ids(prompt)`, `turn_active = true`, `Working`
  - `ToolUse { agent_id: None, .. }` → `turn_active = true`; `launch` → `upsert` with expiry per Global Constraints and `transcript_offset` = current transcript length when `transcript` is `Some`; `stopped` → `remove`; `Working`
  - `ToolUse { agent_id: Some(id), .. }` → if tracked, extend its `expires_ts` to `now + SUBAGENT_IDLE_SECS`; never add; `Working`
  - `SubagentStart` → `upsert` subagent `now + SUBAGENT_IDLE_SECS`, no offset; `Working`
  - `SubagentStop` → if `remove` returned false (untracked): return `previous.unwrap_or(Done)` unchanged; if it removed → `Working` when `turn_active || has_live`, else `Done`
  - `Waiting` → `Waiting`, nothing else changes
  - `Stop` → `turn_active = false`; `Working` if `has_live`, else `Done`

- [ ] **Step 1: Write the failing tests** (temp transcripts via `tempfile`, already a dev-dependency — check `Cargo.toml`, else write under `std::env::temp_dir()` with a unique name):
  - `claude_background_turn_shows_background_then_done`: replay — `PromptSubmit`, `ToolUse` Bash launch `b1`, `ToolUse` Agent launch `a1`, `Stop` → `Working` with `turn_active == false` and `has_live`; `PromptSubmit` with a terminal notification for `a1` → `Working`, `turn_active`; `Stop` → still background (`b1` live); `PromptSubmit` notification `b1`, `Stop` → `Done`, no items.
  - `codex_last_subagent_stop_after_turn_end_is_done`: `PromptSubmit`, `SubagentStart s1`, `SubagentStart s2`, `Stop` → `Working` (background); `ToolUse { agent_id: s1 }` → `Working`; `SubagentStop s1` → `Working`; `SubagentStop s2` → `Done`.
  - `untracked_subagent_stop_is_ignored`: after `Stop` with no items, `SubagentStop x` with `previous = Some(Done)` → `Done`, items still empty.
  - `foreground_subagent_is_not_tracked`: `PromptSubmit`, `ToolUse { agent_id: f1 }` ×2, `Stop` → `Done`.
  - `stop_scans_transcript_for_queued_notifications`: transcript file with some bytes; `ToolUse` Bash launch `b1` (offset = len); append a JSON line holding a `queued_command` prompt `<task-notification><task-id>b1</task-id><status>completed</status></task-notification>`; `Stop` → `Done`.
  - `later_hook_rescans_only_new_bytes`: after `Stop` left `b1` live, append its notification, then a `Waiting` event → `b1` removed; offsets equal the file length afterwards.
  - `scan_tolerates_missing_and_huge_transcripts`: transcript path that does not exist → items kept, no panic; a 9 MiB file whose notification for `b1` sits in the first 1 MiB with `b1` offset 0 → `b1` kept (outside the last 8 MiB), a notification for `b2` in the last 1 MiB → `b2` removed.
  - `task_stop_removes_item` and `session_start_clears_everything`.
  - `expiry_values_follow_launch_kind`: Monitor `timeout_secs: Some(60)` at now 1000 → `expires_ts == 1180`; persistent → `87400`; Shell → `44200`; Subagent → `2800`.
- [ ] **Step 2: Run** `cargo test --bin workmux background` — expected: compile errors / failures.
- [ ] **Step 3: Implement** `transition` and `scan_transcript`.
- [ ] **Step 4: Run** `cargo test --bin workmux` — expected: all pass.
- [ ] **Step 5: Commit** `feat(state): background work transitions with transcript scan`.

### Task 4: Wire the transition into status hooks

**Files:**
- Modify: `src/state/mod.rs` (`persist_agent_update`, `persist_agent_registration`, `persist_agent_snapshot(_locked)`), `src/command/set_window_status.rs` (`run`, `register_agent`, `apply_status_update`), `src/sandbox/rpc.rs` (pass `None`), `src/config.rs` (`StatusIcons.background`)

**Interfaces:**
- Consumes: `transition`, `HookEvent`, `HookInput::event`, `HookInput::transcript_path` (exists).
- Produces:
  - `pub struct StatusEvent { pub event: HookEvent, pub transcript: Option<PathBuf> }` in `src/state/mod.rs`
  - `persist_agent_update(..existing params.., event: Option<StatusEvent>) -> Option<ResolvedStatus>` with `pub struct ResolvedStatus { pub status: AgentStatus, pub background: bool }`; with `event: None` the stored status is the passed `status` exactly as today and `turn_active`/`background` are preserved from `existing`
  - with `event: Some`, `persist_agent_snapshot_locked` runs `transition` on the fields copied from `existing` (empty when there is no same-process record) with `previous = existing.status`; the returned status replaces the passed one
  - `persist_agent_registration` (SessionStart): fresh record, `turn_active false`, no items (already true: `preserve_existing == false`)
  - `StatusIcons.background: Option<String>`, `fn background(&self) -> &str` default `"⏳"`
  - `apply_status_update` builds `StatusEvent` from the hook, calls persist first, then sets the window icon from `ResolvedStatus`: background → `(background icon, auto_clear false)`, else the existing mapping; when persist returns `None` fall back to today's icon for the command

- [ ] **Step 1: Write the failing tests** in `src/state/mod.rs` for the pure part: extract `fn resolve_background(existing: Option<&AgentState>, status: Option<AgentStatus>, event: Option<&StatusEvent>, now: u64) -> (Option<AgentStatus>, bool, Vec<BackgroundItem>)` and test:
  - `no_event_keeps_status_and_background_fields`: existing with `turn_active false` and one live item, status `Some(Done)`, no event → `(Some(Done), false, [item])`.
  - `event_overrides_status_from_transition`: existing `Working`, `turn_active true`, one live item, event `Stop` → `(Some(Working), false, [item])`.
  - `config_background_icon_defaults_to_hourglass`: `StatusIcons::default().background() == "⏳"`, and a YAML `status_icons: {background: "B"}` gives `"B"`.
- [ ] **Step 2: Run** `cargo test --bin workmux` — expected: compile errors.
- [ ] **Step 3: Implement** the wiring; keep `#[allow(clippy::too_many_arguments)]` where the parameter lists grow.
- [ ] **Step 4: Run** `cargo test --bin workmux` — expected: all pass; then an isolated smoke test: `tmux -L wmtest`, a pane running `bash`, pipe the captured Codex sequence (`UserPromptSubmit`, `SubagentStart s1`, `Stop`, `SubagentStop s1`) as hook JSON into `target/debug/workmux set-window-status working|done` from that pane, read `~/.local/state/workmux/agents/*wmtest*` after each step: background after `Stop`, `done` after `SubagentStop`; kill the server and delete those files afterwards.
- [ ] **Step 5: Commit** `feat(status): apply background transitions in status hooks`.

### Task 5: Render the background state

**Files:**
- Modify: `src/command/dashboard/app/agents.rs` (`get_status_display`), `src/command/dashboard/ui/format.rs` (`count_agent_statuses`, `format_agent_status_summary`), `src/workflow/types.rs` (`AgentStatusSummary`), `src/workflow/list.rs` (summary building), `src/command/list.rs` (`format_status_label`, `format_agent_status`), `src/command/status.rs` (`StatusEntry`), `src/command/sidebar/ui.rs` (`status_icon_and_style`), `src/command/sidebar/snapshot.rs`, `src/command/sidebar/daemon.rs` (`InactivityTracker::check_with`), `src/multiplexer/mod.rs:838` if it maps status to icons

**Interfaces:**
- Consumes: `AgentPane.background`, `StatusIcons::background()`.
- Produces:
  - `AgentStatusSummary { pub statuses: Vec<AgentStatus>, pub background: usize }` — `background` counts matched agents with `AgentPane.background`; their status stays `Working` in `statuses`
  - `count_agent_statuses` reports `working` excluding background agents and a new `background` count; summary renders the background icon (palette `info`, no spinner) after working and before waiting
  - `StatusEntry.background: bool` with `#[serde(skip_serializing_if = "std::ops::Not::not")]`
  - `InactivityTracker::check_with` considers only agents with `status == Working && !background`
  - every place that shows the working icon shows `status_icons.background()` when `AgentPane.background` (dashboard Agents tab without spinner; sidebar without spinner; `workmux list` icon or label `background` when piped)

- [ ] **Step 1: Write the failing tests:**
  - `format.rs`: `summary_shows_background_icon_without_spinner` — summary `{statuses: [Working, Working], background: 1}` → spans contain `icons.working()` once and `icons.background()` once.
  - `list.rs`: `piped_label_for_background_agent_is_background`.
  - `status.rs`: `background_entry_serializes_flag` — entry with `background: true` has `"background":true`; with false the key is absent.
  - `daemon.rs`: `background_agents_are_not_interrupted` — existing tracker test pattern with an agent `Working`, `background: true`, unchanged capture past the 10 s timeout → not in the returned set.
- [ ] **Step 2: Run** `cargo test --bin workmux` — expected: compile errors / failures.
- [ ] **Step 3: Implement** the rendering changes.
- [ ] **Step 4: Run** `cargo test --bin workmux`, `cargo fmt --all -- --check`, `cargo clippy --all-targets` — expected: all clean.
- [ ] **Step 5: Commit** `feat(dashboard): show background work with its own icon`.

### Task 6: Documentation and real-agent check

**Files:**
- Modify: `docs/src/content/docs/guide/status-tracking.mdx`, `docs/src/content/docs/guide/configuration.md` (`status_icons.background`), `FORK.md` (fork features)

- [ ] **Step 1: Document** the background state (what counts, ⏳, expiry values, Claude and Codex behaviour) in `status-tracking.mdx`, the new icon key in `configuration.md`, one bullet in `FORK.md`. Check formatting with `cd docs && npx -y oxfmt@latest --check <changed .md/.mdx files>`.
- [ ] **Step 2: Real Claude check** in an isolated tmux server (`tmux -L wmtest`) with `~/.local/bin/workmux` temporarily **not** replaced: run the debug binary through a scratch project's `.claude/settings.json` hooks (same events as the user hooks, binary = `target/debug/workmux`), `claude --model haiku --dangerously-skip-permissions`, prompt it to launch a background Bash `sleep 25`, a Monitor `sleep 15`, a background Agent `sleep 8`, end the turn. Expected in the agent state file: background after the first `Stop`, `done` after the last notification turn; tmux window name shows ⏳ in between.
- [ ] **Step 3: Real Codex check** the same way with a scratch `.codex/hooks.json` and two `spawn_agent` subagents running `sleep 12` without waiting. Expected: background after `Stop`, `done` after the second `SubagentStop`. Remove the scratch project's trust entries from `~/.codex/config.toml` afterwards and delete the `wmtest` state files.
- [ ] **Step 4: Kit note**: in `/home/tore/workspaces/octolabs-worktree/feat-workmux-info/docs/workmux.md` (fork section, PR #34) add one bullet about the background state next to the panels and `cwd` bullets; run `npm run check` there; commit and push that branch.
- [ ] **Step 5: Commit** `docs: background work status`.
