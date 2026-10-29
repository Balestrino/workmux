# Workspaces Dashboard and External Worktree Info Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `workmux dashboard` shows every worktree of every repository under the configured workspace directories, with correct project names, and shows environment info (status, ID, URLs, orphans) from an external command. The Octolabs Worktree kit provides that command.

**Architecture:** New global-only `dashboard` settings. A `workspaces` module discovers repositories and names them. The Worktrees tab fetches every discovered repository in one background job. A `worktree_info` module runs the provider command on its own timer and indexes its JSON by canonical path. Rendering adds an Env column, orphan rows and Info panel lines. On the kit side, `overview --workmux-info` emits the JSON from the lifecycle registry.

**Tech Stack:** Rust 1.95 and ratatui (workmux); Node ≥ 22 and `node --test` (kit).

**Spec:** `docs/superpowers/specs/2026-10-24-workspaces-dashboard-design.md` (fork `Balestrino/workmux`, branch `feat/workspaces-dashboard`)

**Where:** Tasks 1–6 and 8 run on host `tore-dev-1` in `~/workmux`, with `source ~/.cargo/env` first. Task 7 runs on the Octolabs VM, in a kit worktree at `/home/tore/workspaces/octolabs-worktree/feat-workmux-info`, branch `feat/workmux-info`, based on `main`.

## Global Constraints

- With none of the new settings present, behavior is identical to upstream. Existing tests must stay green.
- `dashboard.workspaces`, `dashboard.worktree_info_command` and `dashboard.worktree_info_interval` are global-only. In project config they are ignored with `tracing::warn!("dashboard.<field> in project config (.workmux.yaml) is ignored -- move it to your global config (~/.config/workmux/config.yaml)")`.
- `worktree_info_interval`: default 15 s, minimum 2 s. Provider timeout: 10 s, then the child is killed. At most one provider run at a time.
- Discovery: a repository is a directory at depth 1 or 2 below a workspace directory whose `.git` is a **directory**. Do not descend into a found repository. Skip hidden directories. Deduplicate by `repo_key` (`git::get_repository_key_in`).
- Project name: the first path component below the workspace directory. When several repositories share it, use the full relative path (`cross-repo-sync/commons`). Outside any workspace, use the existing `extract_project_name`.
- Provider JSON `schemaVersion` must be `1`, otherwise it counts as an error. Tones are `success | warning | error | info | muted`, and an unknown tone becomes `muted`. Unknown fields are ignored. When one path has several entries, the first one wins. Paths are compared after canonicalization.
- No Octolabs names, paths or kit concepts in workmux code, defaults or docs examples. Use neutral examples such as `my-env-tool status --json`.
- Before every commit: `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` must be clean, and `cargo test` must pass.
- Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Actions on a row from another repository.** The dashboard is opened from repo A and the user opens, closes, jumps to or adds a worktree on a row from repo B. The action must target B. Today `open_selected_worktree` uses `WorkflowContext::new` (the cwd repository), and `close_selected_worktree_window` uses the handle-only `get_worktree_*` helpers. Owned by Task 4.
2. **Orphan row selected, then an action key or a double-click.** Nothing happens, and nothing panics on an out-of-range index. Owned by Task 6.
3. **The provider prints invalid JSON, hangs, or is missing after a good run.** The last good data stays on screen and the UI never blocks. Owned by Task 5.
4. **Two repositories with the same handle** (every main checkout is `primary-clone`). Selection stays stable across refreshes, which match by path and not by handle, and the picker lists both. Owned by Tasks 3 and 6.
5. **A workspace directory that is missing or unreadable, or a repository whose `git worktree list` fails.** The other repositories still show. Owned by Tasks 2 and 3.

---

### Task 1: Config settings and Env column enum

**Files:**

- Modify: `src/config.rs` (`DashboardConfig` ~L246, `impl DashboardConfig` ~L385, `WorktreeColumn` ~L346, `DEFAULT_WORKTREE_COLUMNS` ~L360, dashboard merge ~L3240, example config text ~L3787)
- Modify: the docs page that documents `worktree_columns` (find it with `grep -rn worktree_columns docs README.md`)
- Test: `src/config.rs` tests module

**Interfaces:**

- Produces:
  - `DashboardConfig.workspaces: Option<Vec<String>>`
  - `DashboardConfig.worktree_info_command: Option<String>`
  - `DashboardConfig.worktree_info_interval: Option<u64>`
  - `DashboardConfig::workspaces(&self) -> Vec<PathBuf>`, with `~` expanded and an empty Vec when unset
  - `DashboardConfig::worktree_info_command(&self) -> Option<&str>`, where a blank string means None
  - `DashboardConfig::worktree_info_interval(&self) -> Duration`
  - `WorktreeColumn::Env`, serde name `env`
  - `DEFAULT_WORKTREE_COLUMNS` becomes `[Number, Project, Worktree, Git, Pr, Env, Mux, Age, Agent]` (length 9)

- [ ] **Step 1: Write failing tests**
  - `dashboard_workspaces_global_only`: the merge of global `workspaces: ["~/ws"]` with project `workspaces: ["/evil"]` gives `workspaces() == [home.join("ws")]`.
  - `dashboard_worktree_info_command_global_only`: when only the project config sets the command, `worktree_info_command() == None`.
  - `dashboard_worktree_info_interval_default_and_floor`: unset → 15 s, `1` → 2 s, `30` → 30 s.
  - `worktree_columns_parse_env`: YAML `worktree_columns: [env]` → `[WorktreeColumn::Env]`.
- [ ] **Step 2: Run them and confirm they fail.** Run `cargo test config::tests::dashboard_ -- --nocapture`. Expected: compile errors or FAIL.
- [ ] **Step 3: Implement.** Follow the `host_commands` global-only pattern (~L3364): take the global value and warn if the project value is `Some`. Expand `~` with the same helper the config already uses for paths (grep `expand_tilde` or `shellexpand`).
- [ ] **Step 4: Add the documentation.** Add the three settings and the `env` column to the example config text and to the docs page, using neutral examples.
- [ ] **Step 5: Run `cargo test config` and the full check.** Expected: PASS, and fmt and clippy clean.
- [ ] **Step 6: Commit** with message `feat(config): dashboard workspaces and worktree info provider settings`.

### Task 2: Workspace discovery and project naming

**Files:**

- Create: `src/workspaces.rs`, and register it with `mod workspaces;` in `src/main.rs`
- Test: inline `#[cfg(test)]` in `src/workspaces.rs`, using `crate::test_support::{init_repo, run_git}` and `tempfile`

**Interfaces:**

- Consumes: `DashboardConfig::workspaces()` (Task 1), `git::get_repository_key_in`, `agent_display::extract_project_name`
- Produces:

  ```rust
  pub struct WorkspaceRepo { pub repo_key: PathBuf, pub main_root: PathBuf, pub project: String }
  /// Repositories under the workspace dirs, sorted by project name.
  pub fn discover_repos(workspaces: &[PathBuf]) -> Vec<WorkspaceRepo>
  /// Project name for each main root (the key is the canonical main root).
  pub fn project_names(workspaces: &[PathBuf], main_roots: &[PathBuf]) -> HashMap<PathBuf, String>
  ```

  `discover_repos` names its results through `project_names`. Callers also use `project_names` for agent-tracked roots outside the workspaces.

- [ ] **Step 1: Write failing tests**, each building a fixture under a `tempdir` `ws`:
  - `discovers_primary_clone_layout`: `ws/alpha/primary-clone` is a git repo → one repo, project `alpha`, and `main_root` ends with `alpha/primary-clone`.
  - `discovers_depth_one_repo`: `ws/beta` is a repo → project `beta`.
  - `shared_first_component_uses_relative_path`: `ws/sync/one` and `ws/sync/two` are repos → projects `sync/one` and `sync/two`.
  - `ignores_linked_worktrees_and_hidden`: `ws/alpha/feature` is a linked worktree (`.git` is a file) and `ws/.cache/x` is a repo → only `alpha` is found.
  - `does_not_descend_into_repo`: `ws/gamma` is a repo containing `ws/gamma/vendor/dep`, also a repo → only `gamma`.
  - `missing_workspace_dir_is_skipped`: workspaces `[ws, ws/nope]` → still finds the repos in `ws`.
  - `outside_workspace_falls_back`: `project_names(&[ws], &[other/delta])` → `delta`.
- [ ] **Step 2: Run `cargo test workspaces::` and confirm the tests fail.**
- [ ] **Step 3: Implement.** Use `std::fs::read_dir` to depth 2. Canonicalize with `crate::util::canon_or_self`. Skip, without error, unreadable directories and repositories whose `repo_key` fails.
- [ ] **Step 4: Run `cargo test workspaces::`.** Expected: PASS.
- [ ] **Step 5: Commit** with message `feat: discover workspace repositories and derive project names`.

### Task 3: Worktrees tab lists all repositories; correct project names; picker

**Files:**

- Modify: `src/command/dashboard/app/worktrees.rs` (`spawn_worktree_fetch` ~L228, project picker ~L693–790)
- Modify: `src/command/dashboard/app/types.rs` (`AppEvent::WorktreeList`, `ProjectEntry`)
- Modify: `src/command/dashboard/app/events.rs` (the `WorktreeList` arm)
- Modify: `src/command/dashboard/app/mod.rs` (new field `project_names`)
- Modify: `src/command/dashboard/ui/worktree.rs` (row `project` ~L56, Info panel title ~L374)
- Modify: the Agents tab project cell (grep `extract_project_name` in `src/command/dashboard/`)
- Test: inline tests in `app/worktrees.rs` or `app/types.rs`, and the render tests in `ui/worktree.rs`

**Interfaces:**

- Consumes: `workspaces::{discover_repos, project_names, WorkspaceRepo}` (Task 2), `DashboardConfig::workspaces()`
- Produces:
  - `AppEvent::WorktreeList(Vec<WorktreeInfo>, HashMap<PathBuf, String>)`, where the map is `repo_key → project`
  - `App.project_names: HashMap<PathBuf, String>`, keyed by `repo_key`
  - `App::project_for(&self, repo_key: &Path, path: &Path) -> String`, which looks up the map and falls back to `extract_project_name(path)`
  - `ProjectEntry { name: String, path: PathBuf, repo_key: Option<PathBuf> }`, where `repo_key == None` is the "All projects" entry
  - free function `fn list_all_repos(config: &Config, mux: &dyn Multiplexer, roots: &[PathBuf]) -> Vec<WorktreeInfo>` in `app/worktrees.rs`

- [ ] **Step 1: Write failing tests**
  - `list_all_repos_concatenates_and_skips_failures`: a fixture with two repos plus one bogus path → the worktrees of both repos are returned and there is no error. Use `crate::multiplexer` test double if one exists (grep `impl Multiplexer for` under `test`); otherwise test the root-selection helper instead: `fn fetch_roots(workspaces: &[WorkspaceRepo], agent_roots: &[PathBuf]) -> Vec<PathBuf>` dedupes by canonical path.
  - `project_picker_keeps_same_named_repos_apart`: two repo keys whose fallback names are both `primary-clone` → two entries, and "All projects" comes first when workspaces are configured.
  - Render test `worktree_table_uses_project_map`: a row with `repo_key` K and `project_names[K] = "alpha"` renders `alpha` in the Project column.
  - Render test `info_title_for_main_checkout`: a main checkout with handle `primary-clone`, branch `dev` and project `alpha` → the panel title contains `alpha · dev`.
- [ ] **Step 2: Run the tests and confirm they fail.**
- [ ] **Step 3: Implement the fetch.** In `spawn_worktree_fetch`, behave as follows:
  - With `workspaces()` non-empty and no override: compute the roots from `discover_repos` plus the agent `repo_roots` values, and for each root load its config with `Config::load_with_location_from(root, None)` and call `workflow::list_in(.., Some(root))`. Errors per root are skipped.
  - Build the names map with `project_names` for all roots, keyed by `repo_key`.
  - Otherwise keep the upstream path, sending a names map built for the single repo.
- [ ] **Step 4: Implement naming and the picker.**
  - Use `project_for` in the Worktrees row, the Info title and the Agents project cell. Agents already have `repo_keys` in `App`.
  - Build the picker from discovered repos plus `repo_roots`, deduped by `repo_key`.
  - Selecting "All projects" sets `worktree_project_override = None` and refetches.
- [ ] **Step 5: Run `cargo test dashboard`.** Expected: PASS. Then run the full check.
- [ ] **Step 6: Commit** with message `feat(dashboard): list worktrees of all workspace repositories`.

### Task 4: Row actions target the row's repository

**Files:**

- Modify: `src/command/dashboard/app/worktrees.rs`, in these functions:
  - `open_selected_worktree` ~L937
  - `close_selected_worktree_window` ~L490
  - `worktree_repo_path` ~L994
  - `show_base_branch_picker` ~L790
  - sweep (`start_sweep` ~L553): check how it resolves the repository
- Test: inline in `app/worktrees.rs`

**Interfaces:**

- Consumes: `WorktreeInfo.path` and `WorktreeInfo.repo_key`
- Produces: `fn worktree_repo_path(&self) -> Option<PathBuf>`. It returns the override path; otherwise, when `workspaces()` is non-empty, the selected row's `path`; otherwise the current upstream behavior.

- [ ] **Step 1: Write the failing test** `repo_path_follows_selected_row_in_all_mode`: `App` with workspaces configured, no override, the selected row in repo B, and `current_worktree` in repo A → `worktree_repo_path()` is B's row path. If constructing `App` in tests is impractical, extract the decision into `fn choose_repo_path(override_path, all_mode, selected_row_path, current) -> Option<PathBuf>` and test that.
- [ ] **Step 2: Run the test and confirm it fails.**
- [ ] **Step 3: Implement.**
  - In `open_selected_worktree`: load the config with `Config::load_with_location_from(&worktree.path, None)` and build the context with `WorkflowContext::new_in(&worktree.path, config, mux, location)`, the same pattern `remove_worktree` (~L185) already uses.
  - In `close_selected_worktree_window`: use the `_in(handle, Some(&worktree.path))` variants of `get_worktree_target_window`, `get_worktree_window_session` and `get_worktree_window_token`.
  - In base picker and sweep: resolve git calls through the row path in the same way.
- [ ] **Step 4: Run the tests and the full check.** Expected: PASS.
- [ ] **Step 5: Commit** with message `fix(dashboard): run worktree actions in the row's repository`.

### Task 5: Worktree info provider (parse, run, index, schedule)

**Files:**

- Create: `src/command/dashboard/worktree_info.rs`, and register it in `src/command/dashboard/mod.rs`
- Modify: `app/types.rs` (new `AppEvent` variant), `app/mod.rs` (fields and tick), `app/events.rs`, `app/worktrees.rs` (`switch_tab`)
- Test: inline in `worktree_info.rs`

**Interfaces:**

- Consumes: `DashboardConfig::{worktree_info_command, worktree_info_interval}` (Task 1)
- Produces:

  ```rust
  #[derive(Clone, Debug, Deserialize, PartialEq)] pub enum Tone { Success, Warning, Error, Info, Muted } // serde lowercase, unknown → Muted
  pub struct InfoStatus { pub label: String, pub tone: Tone }
  pub struct InfoDetail { pub label: String, pub value: String }
  pub struct InfoEntry { pub path: PathBuf, pub status: InfoStatus, pub id: Option<String>, pub details: Vec<InfoDetail> }
  pub struct OrphanEntry { pub project: String, pub name: String, pub path: PathBuf, pub status: InfoStatus, pub id: Option<String>, pub details: Vec<InfoDetail>, pub hint: Option<String> }
  pub struct InfoReport { pub worktrees: Vec<InfoEntry>, pub orphans: Vec<OrphanEntry> }
  pub fn parse_report(json: &str) -> anyhow::Result<InfoReport>
  pub fn run_provider(command: &str, timeout: Duration) -> anyhow::Result<InfoReport> // sh -c, stdout only, kill on timeout
  pub struct InfoIndex { pub by_path: HashMap<PathBuf, InfoEntry>, pub orphans: Vec<OrphanEntry> }
  pub fn index_report(report: &InfoReport, known_paths: &[PathBuf]) -> InfoIndex
  ```

  - `AppEvent::WorktreeInfo(Result<InfoReport, String>)`
  - App fields: `worktree_info: Option<InfoReport>` (last good), `worktree_info_index: InfoIndex`, `worktree_info_error: Option<String>`, `is_info_fetching: Arc<AtomicBool>`, `last_info_fetch: Option<Instant>`
  - `App::reindex_worktree_info(&mut self)`, called after `WorktreeList` and `WorktreeInfo` events

- [ ] **Step 1: Write failing tests**
  - `parse_full_report`: the JSON example from the spec → 1 entry with 2 details, and 1 orphan with the hint.
  - `parse_rejects_other_schema`: `{"schemaVersion":2,...}` → Err.
  - `parse_unknown_tone_is_muted` and `parse_ignores_unknown_fields`.
  - `index_first_entry_wins`: two entries for the same path → the first label is kept.
  - `index_attaches_orphan_matching_known_path`: an orphan whose path is in `known_paths` → it moves to `by_path` and leaves `orphans`.
  - `index_canonicalizes_paths`: an entry path given through a symlink to a known worktree dir → it matches.
  - `run_provider_timeout`: `run_provider("sleep 5", 200ms)` → Err within ~1 s.
  - `run_provider_nonzero_exit`: `run_provider("echo '{}'; exit 3", 2s)` → Err.
  - `run_provider_ok`: `run_provider("printf '%s' '<spec json>'", 2s)` → Ok.
- [ ] **Step 2: Run `cargo test worktree_info` and confirm the tests fail.**
- [ ] **Step 3: Implement `worktree_info.rs`.** Use `std::process::Command::new("sh").arg("-c")` with stdout piped and stderr null, and poll `try_wait` until the deadline, then `kill`. Check `src/cmd.rs` first for an existing timeout helper and reuse it if present.
- [ ] **Step 4: Wire it into the App.**
  - In the tick (next to the worktree fetch ~L397): if a command is set and either `last_info_fetch` is None or elapsed ≥ interval, spawn a thread. Use the same `compare_exchange` and `ResetFlag` pattern as `spawn_worktree_fetch`.
  - On `Ok`, replace `worktree_info` and clear the error. On `Err`, keep the old report and set the error.
  - `switch_tab` to Worktrees triggers a run when none has happened yet.
- [ ] **Step 5: Run `cargo test dashboard` and the full check.** Expected: PASS.
- [ ] **Step 6: Commit** with message `feat(dashboard): external worktree info provider`.

### Task 6: Render Env column, orphan rows, Info panel; orphan-safe selection

**Files:**

- Modify: `src/command/dashboard/ui/worktree.rs` (table ~L23–345, `render_info_panel` ~L365, git log ~L595)
- Modify: `src/command/dashboard/app/worktrees.rs` (`select_worktree`, `worktree_next`, `worktree_previous`, `worktree_jump_to_index` ~L324–372, selection restore in `apply_worktree_filters` ~L284)
- Modify: `src/command/dashboard/mouse.rs` if row-click bounds use `worktrees.len()`
- Test: render tests in `ui/worktree.rs` (follow `worktree_table_reorders_headers_and_cells` ~L706), selection tests in `app/worktrees.rs`

**Interfaces:**

- Consumes: `App.worktree_info_index: InfoIndex`, `App.worktree_info: Option<InfoReport>`, and `Tone` (Task 5); `App::project_for` (Task 3)
- Produces:
  - `App::worktree_row_count(&self) -> usize`, equal to `worktrees.len() + orphans.len()`
  - `App::selected_orphan(&self) -> Option<&OrphanEntry>`
  - tone → palette mapping `fn tone_color(tone: &Tone, palette: &ThemePalette) -> Color`: success → `success`, warning → `accent`, error → `danger`, info → `info`, muted → `dimmed`. If one of these palette fields does not exist, pick the nearest existing one and say so in the commit message.

- [ ] **Step 1: Write failing render tests**
  - `env_column_hidden_without_report`: `worktree_info = None` → the header has no `Env`.
  - `env_column_shows_status`: a row whose path is in the index with label `ready` → the cell text is `● ready`; a row without an entry shows `-`.
  - `orphan_rows_render_after_worktrees`: 2 worktrees and 1 orphan (`project: "alpha"`, `name: "gone"`) → line 3 contains `alpha`, `gone (missing)` and the status label, and has no jump number.
  - `info_panel_env_lines`: the selected row has an entry with id `00000000-0000-…` and details backend/console → the panel contains `Env`, `ready · 00000000`, `backend http://127.0.0.1:18600`.
  - `info_panel_orphan`: the selected index points at the orphan → the panel shows Project, Path, Env, and the hint line; the git log pane shows `(worktree missing)`.
- [ ] **Step 2: Write failing selection tests**
  - `navigation_reaches_orphans`: 2 worktrees and 1 orphan → `worktree_next` twice from index 0 lands on 2; `selected_orphan()` is Some.
  - `actions_noop_on_orphan`: with the orphan selected, `remove_selected_worktree()` leaves `pending_remove == None` and `open_selected_worktree()` / `close_selected_worktree_window()` return without panic.
- [ ] **Step 3: Run `cargo test dashboard::ui::worktree` and the selection tests, and confirm they fail.**
- [ ] **Step 4: Implement.**
  - The Env column is hidden like the PR column when `worktree_info.is_none()`.
  - Orphan rows are dimmed; `(missing)` uses the error color; other cells are empty.
  - The Info panel appends `Env     <label> · <id[..8]>` and one line per detail, with labels padded to the existing 8-char label width.
  - Selection bounds use `worktree_row_count()`. Keyboard jump numbers stay 1–9 over real rows only.
- [ ] **Step 5: Run `cargo test` and the full check.** Expected: PASS.
- [ ] **Step 6: Commit** with message `feat(dashboard): env column, orphan rows and env info panel`.

### Task 7: Kit — `overview --workmux-info` and docs (Octolabs VM)

**Files:**

- Modify: `src/overview.mjs` (new exported functions), `src/cli.mjs` (flag `workmux-info`, mutually exclusive with `--json` and `--watch`)
- Modify: `docs/workmux.md` (Workmux fork section), `docs/runbook.md` ("dove guardare"); check `grep -rn "overview --watch" docs README*` and update every mention
- Test: `tests/overview.test.mjs`

**Interfaces:**

- Consumes: `environmentsOf` (LifecycleService `list()`), `defaultWorkspaces`, environment fields `worktree`, `worktreePresent`, `state`, `reason`, `environmentId`, `urls`
- Produces:
  - `export function buildWorkmuxInfo({ workspaces, environments }) -> { schemaVersion: 1, worktrees, orphans }`
  - `export async function collectWorkmuxInfo({ workspaces, stateDirectory })`, which neither runs git nor calls workmux
  - `export function projectNameFor(workspaces, worktreePath) -> string`: the first component relative to `workspaces`. When `<workspaces>/<first>` holds more than one depth-2 `.git` directory, it uses the first two components, the same rule as workmux. When the path is outside `workspaces`, it uses the basename of the parent dir.

- [ ] **Step 1: Create the kit worktree.** Run `git -C /home/tore/octolabs-worktree worktree add /home/tore/workspaces/octolabs-worktree/feat-workmux-info -b feat/workmux-info main`.
- [ ] **Step 2: Write failing tests** in `tests/overview.test.mjs`
  - `buildWorkmuxInfo maps present environment`: state `ready`, urls `{backend, console}` → a `worktrees[0]` entry with `status {label:'ready', tone:'success'}`, `details` in urls order, and `id` set.
  - `buildWorkmuxInfo maps missing worktree to orphan`: `worktreePresent: false`, `worktree: <ws>/alpha/gone` → an orphan with `project: 'alpha'`, `name: 'gone'`, `hint: 'forget --environment <id>'`, and a `warning` tone for state `unknown`.
  - `tone mapping`: ready → success; starting, stopping, preparing → info; failed, error → error; anything else → warning.
  - `label includes reason`: state `unknown` with reason `worktree-missing-or-replaced` → label `unknown`, and a `details` entry `{label:'reason', value:'worktree-missing-or-replaced'}`.
  - `cli overview --workmux-info rejects --json together`: a usage error.
- [ ] **Step 3: Run `node --test tests/overview.test.mjs` and confirm the tests fail.**
- [ ] **Step 4: Implement.** The CLI prints `JSON.stringify(result)` to stdout. Errors exit non-zero, so workmux keeps its last good data.
- [ ] **Step 5: Update the docs (Italian, matching existing style).**
  - Describe the Balestrino/workmux fork and the global config block: `workspaces: [~/workspaces]`, `worktree_info_command: node "$OCTOLABS_WORKTREE_CLI" overview --workmux-info`.
  - State that the dashboard replaces `overview --watch` for daily use.
  - Note that the binary is installed by hand for now.
- [ ] **Step 6: Run `npm test` and `npm run check`.** Expected: PASS.
- [ ] **Step 7: Commit** with message `feat(overview): --workmux-info for the workmux dashboard`.

### Task 8: End-to-end check on tore-dev-1 and push

**Files:** none (verification only)

- [ ] **Step 1: Build a fixture workspace.** Under `~/wm-fixture/ws`, create:
  - `alpha/primary-clone`, with a linked worktree `alpha/feat-x` and a nested one `alpha/primary-clone/.worktrees/nested`;
  - `beta` (depth 1);
  - `sync/one` and `sync/two`;
  - an external worktree of `alpha` at `~/wm-fixture/elsewhere/ext`.
- [ ] **Step 2: Write `~/wm-fixture/provider.sh`.** It prints spec-shaped JSON: `ready` for `alpha/feat-x`, an orphan `alpha/gone`, and a second orphan whose path equals `beta`, which must attach to the beta row.
- [ ] **Step 3: Point a test config at the fixture.** Use `XDG_CONFIG_HOME=~/wm-fixture/cfg` with `dashboard: {workspaces: [~/wm-fixture/ws], worktree_info_command: ~/wm-fixture/provider.sh, worktree_info_interval: 3}`.
- [ ] **Step 4: Run the dashboard.** Inside a tmux session started from `~` (not a repo), run `cargo run -- dashboard --tab worktrees`. Capture it with `tmux capture-pane -p`. Expected:
  - rows for all 7 worktrees (alpha ×4, beta, sync/one, sync/two) with Project values `alpha`, `beta`, `sync/one`, `sync/two`;
  - Env `● ready` on `feat-x`;
  - the beta row shows the attached status;
  - one `gone (missing)` row.
- [ ] **Step 5: Test provider failures.** Edit `provider.sh` to `exit 1` and wait at least 6 s. Expected: the Env data is still shown.
- [ ] **Step 6: Run the regression check.** Without `XDG_CONFIG_HOME`, from inside `alpha/primary-clone`, run `cargo run -- dashboard --tab worktrees`. Expected: only alpha's worktrees and no Env column, as upstream.
- [ ] **Step 7: Run `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.** Expected: all clean.
- [ ] **Step 8: Push the branch.** Run `git push -u origin feat/workspaces-dashboard`, to the Balestrino fork only. Do not open a PR to upstream.
