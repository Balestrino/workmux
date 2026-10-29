# Unified Worktree View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One dashboard view listing every worktree with its agents as sub-rows (agents outside worktrees grouped at the bottom), Enter jumping to an agent, and the Agents tab hidden behind a fork option.

**Architecture:** A pure module `src/command/dashboard/unified.rs` assigns agents to worktrees, orders them and builds a flat `Vec<DisplayRow>`. The App keeps that vector as the single row model of the Worktrees table: navigation, selection, Enter, worktree actions, rendering and mouse hit-testing all go through it. With `dashboard.agents_tab: true` the vector holds only worktree and orphan rows, reproducing today's behaviour.

**Tech Stack:** Rust 2024, ratatui, serde; tests with `cargo test --bin workmux`.

**Spec:** `docs/superpowers/specs/2026-10-29-unified-worktree-view-design.md`

## Global Constraints

- Work in `/home/tore/workspaces/workmux/feat-unified-view` (branch `feat/unified-view`); build with `CARGO_TARGET_DIR=/home/tore/workspaces/workmux/primary-clone/target`. Never install the binary.
- Fork-only config key `dashboard.agents_tab: bool`, default `false`; `true` must behave exactly as before this branch (two tabs, no sub-rows, agent summary column per `worktree_columns`).
- Unified mode ignores the persisted session scope (`scope_mode`) and `hide_stale`; every reconciled agent is placed. The Agents tab keeps both when enabled.
- Assignment: deepest listed-worktree path that is an ancestor-or-equal of the canonicalised `AgentPane.path`, computed against `all_worktrees` (before the `/` filter).
- Sub-row order: Waiting, Working (not background), Working background, Done, no status; ties by most recent `activity_ts` first.
- Sub-row text: `└ ` + status icon (Agents-tab icon, colour and spinner rules; background uses `status_icons.background()` without spinner) + elapsed (Agents-tab format) + pane title; outside-group rows append ` (<dir with $HOME shortened to ~>)`; stale agents dimmed.
- Group header text: `Outside worktrees`; never selectable.
- Outside-group worktree actions show the status message `not in a worktree`.
- `1`-`9` select the n-th worktree row; agent sub-rows are not numbered.
- Every commit ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; `cargo fmt --all -- --check` and `cargo clippy --all-targets` clean before each commit.

## Review Focus

- A linked worktree nested inside its primary clone → an agent inside the linked worktree appears only under the linked one (Task 1 test `deepest_worktree_wins`).
- An agent in a subdirectory of a worktree → Enter on the worktree row jumps to it (Task 2 test `enter_on_worktree_targets_agent_in_subdirectory`).
- Refresh while an agent sub-row is selected and rows shift → selection stays on the same agent (Task 2 test `selection_follows_agent_across_rebuild`).
- The `/` filter hides a worktree whose agent is selected → selection falls back to the first selectable row, never the header (Task 2 test `selection_never_lands_on_header`).
- `agents_tab: true` → rows, navigation and rendering unchanged (Task 2 test `agents_tab_mode_has_no_agent_rows`, Task 3 test `agents_tab_mode_keeps_tab_header_and_agent_column`).

---

### Task 1: Assignment, ordering and display rows

**Files:**
- Create: `src/command/dashboard/unified.rs` (register `mod unified;` in `src/command/dashboard/mod.rs`)
- Modify: `src/config.rs` (`DashboardConfig.agents_tab: Option<bool>`, `fn agents_tab(&self) -> bool` default `false`, merge like the other dashboard keys)

**Interfaces:**
- Produces (in `unified.rs`, all `pub(super)` or `pub(crate)`):
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum DisplayRow { Worktree(usize), Agent { pane_id: String, parent: Option<usize> }, Orphan(usize), OutsideHeader }` — `Worktree(i)`/`parent: Some(i)` index `App.worktrees` (the visible, filtered list); `Orphan(i)` indexes `App.visible_orphans`
  - `pub fn assign_agents(worktree_paths: &[PathBuf], agents: &[AgentPane]) -> Vec<Option<usize>>` — per agent, the index into `worktree_paths` of the deepest ancestor-or-equal (canonicalise both sides with `canon_or_self` as `src/workflow/list.rs` does), `None` when unassigned
  - `pub fn agent_order(a: &AgentPane, b: &AgentPane) -> std::cmp::Ordering` — the Global Constraints order
  - `pub struct RowInputs<'a> { pub visible_worktrees: &'a [PathBuf], pub all_worktrees: &'a [PathBuf], pub agents: &'a [AgentPane], pub orphan_count: usize, pub filter: &'a str, pub outside_group: bool }`
  - `pub fn build_display_rows(inputs: &RowInputs) -> Vec<DisplayRow>` — assignment against `all_worktrees`; each visible worktree row followed by its agents (sorted); then `Orphan(0..orphan_count)`; then, when `outside_group` is true and at least one unassigned agent passes the filter (empty filter, or lowercase title or path contains the lowercase filter), `OutsideHeader` + those agents sorted. Agents assigned to a worktree that is not visible are dropped.
  - `pub fn worktree_visible_by_agent(path: &Path, agents: &[AgentPane], all_worktrees: &[PathBuf], filter: &str) -> bool` — true when an agent assigned to `path` has a title containing the lowercase filter (used by the App's `/` filter)
  - `pub fn is_selectable(row: &DisplayRow) -> bool` — false only for `OutsideHeader`

- [ ] **Step 1: Write the failing tests** in `unified.rs` (use real temp dirs only if canonicalisation needs them; `canon_or_self` falls back to the raw path):
  - `deepest_worktree_wins`: worktrees `/ws/repo/primary-clone` and `/ws/repo/primary-clone/feat-x`; agent at `/ws/repo/primary-clone/feat-x/src` → `Some(1)`; agent at `/ws/repo/primary-clone/docs` → `Some(0)`.
  - `subdirectory_and_unassigned`: agent at `/ws/repo/feat-y/backend` → index of `/ws/repo/feat-y`; agent at `/home/u/.claude/projects/x/memory` → `None`; `/ws/repo/feat-yy` does not match `/ws/repo/feat-y` (component-wise prefix).
  - `agents_sorted_by_status_then_recency`: Done(ts 50), Working-background(ts 40), Waiting(ts 10), Working(ts 30), Working(ts 35), None → Waiting, Working 35, Working 30, background, Done, None.
  - `rows_interleave_agents_orphans_and_outside_group`: 2 visible worktrees, agent A under worktree 1, agent B unassigned, 1 orphan, `outside_group: true` → `[Worktree(0), Worktree(1), Agent{A, Some(1)}, Orphan(0), OutsideHeader, Agent{B, None}]`.
  - `outside_group_hidden_when_disabled_or_filtered_out`: same inputs with `outside_group: false` → no header/B; with filter `zzz` → no header/B; with filter matching B's title → header + B.
  - `agents_of_hidden_worktrees_are_dropped`: worktree in `all_worktrees` but not visible, its agent → absent and not in the outside group.
  - `header_is_not_selectable`.
  - `worktree_visible_by_agent_title`: agent titled "Fork workmux" under `/ws/w` → true for filter `fork`, false for `zzz`.
  - config: `agents_tab_defaults_to_false` and YAML `dashboard: {agents_tab: true}` → `true`.
- [ ] **Step 2: Run** `cargo test --bin workmux unified` — expected: compile errors.
- [ ] **Step 3: Implement** the module and the config key.
- [ ] **Step 4: Run** `cargo test --bin workmux` — expected: all pass.
- [ ] **Step 5: Commit** `feat(dashboard): agent assignment and display rows for the unified view`.

### Task 2: App row model, selection, Enter and actions

**Files:**
- Modify: `src/command/dashboard/app/mod.rs` (fields, `refresh`, keymap context), `src/command/dashboard/app/worktrees.rs` (row model, navigation, selection, Enter, actions), `src/command/dashboard/app/types.rs` (`MouseRowId::WorktreeAgent(String)` if needed by Task 3), `src/command/dashboard/mod.rs` (`--tab agents` ignored when `agents_tab` is false; initial tab Worktrees), `src/command/dashboard/actions.rs` (`SwitchTab` no-op when `agents_tab` is false)

**Interfaces:**
- Consumes: Task 1 `DisplayRow`, `build_display_rows`, `RowInputs`, `worktree_visible_by_agent`, `is_selectable`, `Config::dashboard.agents_tab()`.
- Produces:
  - `App.unified: bool` (= `!config.dashboard.agents_tab()`), `App.host_agents: Vec<AgentPane>` (reconciled agents **before** `scope_mode.retain_agents`; refreshed in `refresh()`), `App.display_rows: Vec<DisplayRow>`
  - `fn rebuild_display_rows(&mut self)` — called at the end of `apply_worktree_filters()` and after `refresh()` reloads agents; in non-unified mode produces `Worktree(0..n)` + `Orphan(0..m)` only; `outside_group = worktree_project_override.is_none()`
  - `pub fn worktree_row_count(&self) -> usize` returns `display_rows.len()`; navigation (`worktree_next/previous`, wrap) skips non-selectable rows
  - `pub fn selected_row(&self) -> Option<&DisplayRow>`
  - `pub fn selected_worktree_index(&self) -> Option<usize>` — `Worktree(i)` → `i`; `Agent { parent: Some(i) }` → `i`; otherwise `None`
  - every existing use of `self.worktrees.get(selected)`, `row_kind(...)` and `RowKind` in `app/worktrees.rs` goes through `selected_row`/`selected_worktree_index` (remove `row_kind`/`RowKind` once unused)
  - selection identity: `selected_worktree_path: Option<PathBuf>` plus `selected_agent_pane: Option<String>`; after a rebuild restore by pane id for agent rows, by path for worktree/orphan rows, else the first selectable row
  - Enter (`jump_to_selected_worktree`): `Agent { pane_id, .. }` → `switch_to_pane_and_track(&pane_id)`; `Worktree(i)` → the first `Agent` row following it (its first sub-row) if any, else the existing window/open logic; in non-unified mode keep today's code path but match agents with the Task 1 assignment instead of `a.path == worktree.path`
  - worktree actions on an outside-group `Agent { parent: None }` → `status_message = "not in a worktree"`
  - `worktree_jump_to_index(n)` selects the n-th `Worktree` row in `display_rows`
  - `/` filter in unified mode: `apply_worktree_filters` keeps a worktree when it matches today's rule **or** `worktree_visible_by_agent(...)`
  - unified mode ignores `hide_stale` and `scope_mode` for `host_agents`; the Agents-tab `all_agents` path is unchanged

- [ ] **Step 1: Write the failing tests.** Where an `App` is too heavy to build, extract the logic into pure helpers next to the methods and test those:
  - `selection_follows_agent_across_rebuild`: rows `[W0, A(p1, 0), W1]` with `p1` selected; rebuild to `[W0, W1, A(p1, 1)]` → selection index 2.
  - `selection_never_lands_on_header`: rebuild leaves only `[Orphan(0), OutsideHeader, A(p2, None)]` with the previous selection gone → index 0; navigation from `Orphan(0)` down → `A(p2)` (index 2), skipping the header.
  - `enter_on_worktree_targets_first_agent_row`: rows `[W0, A(p1,0), A(p2,0)]`, selection W0 → target pane `p1`.
  - `enter_on_worktree_targets_agent_in_subdirectory`: non-unified helper — worktree `/ws/r/feat`, agent path `/ws/r/feat/backend` → that agent's pane (today's exact match would miss it).
  - `actions_from_agent_row_use_parent_worktree`: `selected_worktree_index` for `A(p1, Some(1))` → `Some(1)`; for `A(p2, None)` → `None`.
  - `jump_to_index_counts_worktree_rows_only`: rows `[W0, A, W1, A, W2]`, n=2 → index 4.
  - `agents_tab_mode_has_no_agent_rows`: `unified == false` → rows are `Worktree(..)` then `Orphan(..)` only, for the same inputs.
- [ ] **Step 2: Run** `cargo test --bin workmux dashboard` — expected: compile errors / failures.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --bin workmux` — expected: all pass (existing dashboard tests included).
- [ ] **Step 5: Commit** `feat(dashboard): unified row model with agent sub-rows`.

### Task 3: Rendering, mouse and tab chrome

**Files:**
- Modify: `src/command/dashboard/ui/worktree.rs` (table rows from `display_rows`; agent and header rows; agent column hidden in unified mode), `src/command/dashboard/ui/dashboard.rs` (tab header and footer), `src/command/dashboard/mouse.rs` (row hit-testing and ids over `display_rows`, footer items, tab clicks), `src/command/dashboard/keymap.rs` + `src/command/dashboard/ui/help.rs` + `src/command/dashboard/actions.rs` palette (no Tab entry in unified mode)

**Interfaces:**
- Consumes: `App.display_rows`, `App.unified`, `App.host_agents`, the Agents-tab status rendering (`App::get_status_display` / `get_elapsed` / `is_stale`) — reuse, do not duplicate the icon rules.
- Produces:
  - `fn agent_row_cells(app: &App, agent: &AgentPane, outside: bool, columns: &[WorktreeColumn]) -> Row` — first column holds `└ ` + status spans + elapsed + title (+ ` (~/…)` when `outside`), remaining cells empty; dimmed when stale
  - header row: `Outside worktrees` in the first column, dimmed/bold per palette, not highlighted by selection
  - unified mode: `agent` column filtered out of `visible_columns`; tab header renders only `Worktrees`; footer and help/palette omit Tab; clicking where the Agents tab label was does nothing
  - mouse: row index → `display_rows[index]`; `MouseRowId::WorktreeAgent(pane_id)` for agent rows; double-click on an agent row jumps like Enter; header rows are not clickable

- [ ] **Step 1: Write the failing tests** (ratatui `TestBackend`, like the existing `ui/worktree.rs` tests):
  - `agent_row_shows_indent_icon_and_title`: a Working agent titled "Fork workmux" → first cell starts with `└ ` and contains the working icon and `Fork workmux`.
  - `outside_agent_row_shows_short_dir`: agent path under `$HOME` → contains `(~/`.
  - `unified_table_omits_agent_column`: columns `[Project, Worktree, Agent]` in unified mode → header has no `Agent`.
  - `agents_tab_mode_keeps_tab_header_and_agent_column`: non-unified → tab header contains `Agents` and `Worktrees`, table header contains `Agent`.
  - `footer_has_no_tab_entry_in_unified_mode` (mouse footer items or rendered footer line).
  - `mouse_row_maps_to_display_row`: click on the screen row of `A(p1)` → `MouseRowId::WorktreeAgent("p1")`; click on the header row → none.
- [ ] **Step 2: Run** `cargo test --bin workmux` — expected: failures.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --bin workmux`, fmt, clippy — expected: clean.
- [ ] **Step 5: Commit** `feat(dashboard): render agents as sub-rows and hide the Agents tab`.

### Task 4: Documentation and real dashboard check

**Files:**
- Modify: `FORK.md`, `docs/src/content/docs/guide/dashboard/index.mdx`, the dashboard configuration page that documents `worktree_columns` (find it under `docs/src/content/docs/guide/dashboard/`), kit `/home/tore/workspaces/octolabs-worktree/feat-workmux-info/docs/workmux.md` (Italian, one bullet in the fork section)

- [ ] **Step 1: Document** the unified view (rows, outside group, Enter, actions from sub-rows, filter/picker rules), the `dashboard.agents_tab` key and what it restores, in `FORK.md` and the dashboard docs; `cd docs && npx -y oxfmt@latest --check <changed files>`.
- [ ] **Step 2: Real check** without touching the user's running dashboard: build the debug binary, start it in an isolated window of the default tmux server only if needed for agent panes — preferred: `tmux new-session -d -s wmunified -x 200 -y 45 "<debug binary> dashboard"` on the default server (it reads the real agents and worktrees read-only), capture the pane with `tmux capture-pane -p`, and verify: agents appear under their worktrees (compare with `~/.local/state/workmux/agents/*.json` `cwd`), the `Outside worktrees` group lists agents whose `cwd` is in no worktree, no Tab header. Do **not** press Enter (it would switch the user's client). Kill the `wmunified` session afterwards.
- [ ] **Step 3: Kit note**: commit the bullet in the kit worktree (`npm run check` there); do not push.
- [ ] **Step 4: Commit** `docs: unified worktree view`.
