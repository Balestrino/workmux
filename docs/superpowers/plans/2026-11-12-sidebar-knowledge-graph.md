# Sidebar knowledge-graph line Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a fourth line in every sidebar agent tile showing the CodeGraph state of that agent: `No knowledge graph`, `○ codegraph`, `⚠ codegraph` or a blinking green `● codegraph`.

**Architecture:** a pure module (`src/knowledge_graph.rs`) decides the state from three facts: the index root, whether the server process is connected, and how fresh the index is. A new sidebar-daemon worker thread gathers those facts. It walks `/proc` for the connection and runs `codegraph status --json` at most once a minute per index root. It publishes one status per pane in the snapshot. Clients render a new `{knowledge_graph}` template token.

**Tech Stack:** Rust 2024, ratatui 0.30, serde/serde_json, std threads and `std::process::Command`. No new crates.

**Spec:** `docs/superpowers/specs/2026-11-12-sidebar-knowledge-graph-design.md`

## Global Constraints

- **Read-only.** The sidebar never creates, syncs or repairs an index and never starts a CodeGraph daemon. `codegraph status --json` is the one accepted probe. It opens the database and creates then removes SQLite's `-wal`/`-shm` files, which the person accepted on 2026-11-12. Run no other `codegraph` subcommand.
- **Probe limits.** At most one probe per index root every 60 s. Only index roots used by a listed, non-remote agent are probed. Run it as `nice -n 19 ionice -c3 codegraph status --json <root>`, without `ionice` when it is missing, with a 10 s timeout, and kill it on timeout. Measured cost: 1.5 s and 168 MB on a 197 MB index.
- **Index.** An index is the nearest ancestor of `agent.path`, the agent itself included, that contains `.codegraph/codegraph.db`. A bare `.codegraph/` does not count, because CodeGraph also keeps a global `~/.codegraph/`. A path ending in ` (deleted)` or missing on disk has no index.
- **Connected.** A descendant of the pane's process whose `/proc/<pid>/cmdline` has an argument ending in `codegraph` or `codegraph.js`, followed somewhere by the arguments `serve` and `--mcp`. When the pane pid is unknown or `/proc` is unreadable, the connection is `Unknown`.
- **Stale.** The index is stale when any of these holds:
  - `index.reindexRecommended == true`;
  - `index.state` is non-null and not `"complete"`;
  - `pendingChanges.added + modified + removed > 0` in two consecutive probes whose times are at least 120 s apart. The first probe with pending changes starts the clock; a probe with none resets it.
- **States and text** (copy is fixed):
  - `None` → `No knowledge graph`, dimmed;
  - `Available` → `○ codegraph`, dimmed;
  - `Attention` → `⚠ codegraph` (U+26A0 with no U+FE0F), warning colour;
  - `Active` → `● codegraph`, success colour, alternating with dimmed every 500 ms.
- **Decision table** (`Unknown` connection never yields `Attention` for a missing server):

  | index | connected    | stale | state     |
  | ----- | ------------ | ----- | --------- |
  | none  | yes          | –     | Available |
  | none  | no / unknown | –     | None      |
  | yes   | –            | yes   | Attention |
  | yes   | yes          | no    | Active    |
  | yes   | no           | no    | Attention |
  | yes   | unknown      | no    | Available |

- Until the first probe of a root has finished, its index counts as fresh. A failed or timed-out probe keeps the previous freshness, or fresh when there is none.
- Remote agents (`agent.remote.is_some()`) get no status: the token renders empty.
- Only the sidebar changes. The horizontal (top/bottom) default templates keep their three lines.
- Fork rules: public repository, so no host names, user paths or credentials. Feature notes go in FORK.md. Build at low priority, one at a time.

## Review Focus

1. **Several agents sharing one index root**, for example two sessions in the same primary clone. Each agent gets its own connection state, but the root is probed once per minute, not once per agent. Pinned by the test `worker_probes_a_shared_root_once` (Task 3).
2. **An agent whose worktree was removed under it** (tmux reports `… (deleted)`). It shows `No knowledge graph` or `○ codegraph`, never an error, and no probe runs. Pinned by `deleted_directory_has_no_index` (Task 1).
3. **`codegraph` missing from PATH, or the probe failing.** The line falls back to the connection and index facts and never turns `⚠` because of the failure alone. Pinned by `failed_probe_keeps_previous_freshness` (Task 1).
4. **Narrow sidebar (25 columns) and the selected row's background.** The line fits, keeps the stripe and gets no wide-cell artefacts. Pinned by `knowledge_graph_line_fits_a_narrow_sidebar` (Task 4).
5. **Mixed daemon and client versions during an upgrade.** An old snapshot without the field still deserializes, and the line shows nothing. Pinned by `snapshot_without_knowledge_graph_deserializes` (Task 3).

---

### Task 1: State model, index lookup and freshness tracker

**Files:**

- Create: `src/knowledge_graph.rs`
- Modify: `src/main.rs` (add `mod knowledge_graph;` next to `mod wide_cell_backend;`)

**Interfaces:**

- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] pub enum KnowledgeGraphState { None, Available, Attention, Active }`
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Connection { Yes, No, Unknown }`
  - `pub fn decide(has_index: bool, connection: Connection, stale: bool) -> KnowledgeGraphState`. It implements the decision table.
  - `pub fn index_root(path: &Path) -> Option<PathBuf>`
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct ProbeReport { pub pending: u64, pub reindex_recommended: bool, pub state: Option<String> }`
  - `pub fn parse_status_json(json: &str) -> Option<ProbeReport>`. It returns `None` when `initialized` is false or the JSON is malformed.
  - `#[derive(Debug, Default)] pub struct Freshness { pending_since: Option<Instant>, stale: bool }`
  - `impl Freshness { pub fn record(&mut self, report: Option<&ProbeReport>, now: Instant); pub fn is_stale(&self) -> bool }`. A `None` report, meaning the probe failed, leaves the state unchanged.

- [ ] **Step 1: Write the failing tests** in `#[cfg(test)] mod tests` of `src/knowledge_graph.rs`:
  - `decide_follows_the_table`: all six rows of the table.
  - `index_root_needs_the_database`: a temp dir with `.codegraph/` and no db gives `None`; adding `.codegraph/codegraph.db` gives `Some(root)`; a nested `a/b` under the root resolves to the root.
  - `deleted_directory_has_no_index`: `index_root(Path::new("/x/y (deleted)"))` gives `None`, and so does a path that does not exist.
  - `parse_status_json_reads_the_fields`: on the measured sample `{"initialized":true,"pendingChanges":{"added":89,"modified":107,"removed":0},"index":{"reindexRecommended":false,"state":"complete"}}` it gives `ProbeReport { pending: 196, reindex_recommended: false, state: Some("complete") }`; `{"initialized":false}` gives `None`; `"garbage"` gives `None`.
  - `pending_changes_turn_stale_after_120_seconds`: pending at t0 is not stale; still pending at t0+119 s is not stale; still pending at t0+120 s is stale; a report with 0 pending is not stale again.
  - `reindex_or_partial_is_stale_at_once`: `reindex_recommended: true` is stale; `state: Some("partial")` is stale; `state: None` is not stale.
  - `failed_probe_keeps_previous_freshness`: stale, then `record(None, …)`, is still stale; a fresh `Freshness` with `record(None, …)` is not stale.
- [ ] **Step 2: Run** `nice -n 19 cargo test --bin workmux knowledge_graph::`. Expected: compile failure, the items are not defined.
- [ ] **Step 3: Implement** the interfaces above. Parse with `serde_json::Value`, treating missing numbers as 0. Walk ancestors with `Path::ancestors()` and check `is_file()` on `.codegraph/codegraph.db`.
- [ ] **Step 4: Run** the same command. Expected: all tests in `knowledge_graph::` pass.
- [ ] **Step 5: Commit** with the message `feat(sidebar): knowledge-graph state model`.

### Task 2: Server connection from `/proc`

**Files:**

- Modify: `src/knowledge_graph.rs`

**Interfaces:**

- Consumes: `Connection` (Task 1).
- Produces:
  - `pub struct ProcTable { children: HashMap<u32, Vec<u32>>, cmdlines: HashMap<u32, Vec<String>> }`
  - `impl ProcTable { pub fn read(proc_root: &Path) -> Option<ProcTable>; pub fn connection(&self, pane_pid: Option<u32>) -> Connection }`
  - `pub fn is_codegraph_server(args: &[String]) -> bool`
- `read` scans `<proc_root>/<pid>/stat` for the ppid. The field comes after the last `)`; this is already done in the fork's tests and scripts. It also reads `<proc_root>/<pid>/cmdline`, split on NUL. It returns `None` if `proc_root` cannot be listed. Read the table once per worker pass, not once per agent.

- [ ] **Step 1: Write the failing tests:**
  - `codegraph_server_args_match`: true for `["/opt/x/node","--liftoff-only","/opt/x/lib/dist/bin/codegraph.js","serve","--mcp"]` and for `["/usr/local/bin/codegraph","serve","--mcp"]`; false for `["node","-e","…codegraph.js…"]`, which is the watchdog with `-e`, for `["codegraph","status"]` and for `["grep","codegraph serve --mcp"]`.
  - `connection_finds_a_grandchild_server`: a fake proc root in a temp dir, with pids 10 (the pane shell), 11 (`claude`, ppid 10) and 12 (the server, ppid 11), gives `Yes` for pane 10. A pane 20 with no server child gives `No`. `connection(None)` gives `Unknown`.
  - `unreadable_proc_is_unknown`: `ProcTable::read(Path::new("/nonexistent"))` is `None`.
- [ ] **Step 2: Run** `nice -n 19 cargo test --bin workmux knowledge_graph::`. Expected: the new tests fail to compile.
- [ ] **Step 3: Implement** `connection` as a breadth-first walk over `children` from `pane_pid`. Guard against cycles with a visited set.
- [ ] **Step 4: Run** the tests. Expected: they pass.
- [ ] **Step 5: Commit** with the message `feat(sidebar): detect a connected codegraph server`.

### Task 3: Daemon worker and snapshot field

**Files:**

- Modify: `src/command/sidebar/daemon.rs`: a new `spawn_knowledge_graph_worker` next to `spawn_git_worker` (~line 1400); start it next to `spawn_git_worker` (~line 2357); send entries next to `git_path_tx.send` (~line 2650); copy the cache into `output.snapshot` after `compute_tick`, next to `window_views` (~line 2590).
- Modify: `src/command/sidebar/snapshot.rs`: `SidebarSnapshot` gets `#[serde(default)] pub knowledge_graph: HashMap<String, KnowledgeGraphState>`, keyed by `pane_id`.
- Modify: `src/knowledge_graph.rs`: the worker's pure core.

**Interfaces:**

- Consumes: everything from Tasks 1–2.
- Produces:
  - `pub struct KgAgent { pub pane_id: String, pub path: PathBuf, pub pane_pid: Option<u32> }`
  - `pub trait Prober { fn probe(&mut self, root: &Path) -> Option<ProbeReport>; }` with `pub struct CodegraphProber`, which runs the command from Global Constraints with its timeout and parses the output with `parse_status_json`.
  - `pub struct KgWorkerState { last_probe: HashMap<PathBuf, Instant>, freshness: HashMap<PathBuf, Freshness> }`
  - `impl KgWorkerState { pub fn pass(&mut self, agents: &[KgAgent], procs: Option<&ProcTable>, prober: &mut dyn Prober, now: Instant) -> HashMap<String, KnowledgeGraphState> }`. It probes a root only when its `last_probe` is unset or at least 60 s old. It drops the state of roots that no agent uses any more.
- **Worker thread:** it receives `Vec<KgAgent>` on a channel and runs `pass` at most every 5 s and at once when the agent list changes. When the map changes it stores the map, sets the dirty flag and wakes the daemon, the same way `spawn_git_worker` does. The daemon fills `pane_pid` from `tmux_state.live_panes[pane_id].pid` and skips agents with `remote.is_some()`.

- [ ] **Step 1: Write the failing tests** in `src/knowledge_graph.rs`, using a `FakeProber` that counts calls and returns a set report:
  - `worker_probes_a_shared_root_once`: two agents on the same root, two passes 10 s apart, give 1 probe call; a third pass at +61 s gives 2.
  - `worker_skips_agents_without_index`: an agent with no index gives 0 probe calls and the state `None` or `Available`, depending on the connection.
  - `worker_reports_active_and_attention`: the root has an index; agent A's pane has a server child, so A is `Active`; agent B's pane has none, so B is `Attention`.

  In `src/command/sidebar/snapshot.rs` tests:
  - `snapshot_without_knowledge_graph_deserializes`: a serialized snapshot with the `knowledge_graph` key removed deserializes with an empty map.

- [ ] **Step 2: Run** `nice -n 19 cargo test --bin workmux knowledge_graph:: snapshot::`. Expected: the new tests fail.
- [ ] **Step 3: Implement** `KgWorkerState::pass`, `CodegraphProber`, the snapshot field and the daemon wiring. In `CodegraphProber`, check once whether `ionice` is on PATH and cache the answer.
- [ ] **Step 4: Run** the tests, then `nice -n 19 cargo test --bin workmux sidebar::`. Expected: all pass.
- [ ] **Step 5: Commit** with the message `feat(sidebar): knowledge-graph worker in the daemon`.

### Task 4: `{knowledge_graph}` token, blink and default templates

**Files:**

- Modify: `src/command/sidebar/template/parser.rs`: add `TokenId::KnowledgeGraph`, named `"knowledge_graph"` in both name maps and in the token list (~lines 21-53, 100, 250, 471).
- Modify: `src/command/sidebar/template/context.rs`: add `RowContext.knowledge_graph: Option<KnowledgeGraphState>`, filled in `RowContext::new` (~line 160) from `app.knowledge_graph.get(&agent.pane_id)` and set to `None` for remote agents; add `pub fn knowledge_graph_spans(&self) -> (Vec<(String, Style)>, usize)`, dispatched in `natural_width` and in the span builder next to `PrChecks`.
- Modify: `src/command/sidebar/app.rs`: add the field `knowledge_graph: HashMap<String, KnowledgeGraphState>`, copied from the snapshot next to `check_statuses` (~line 583); `refresh_interval` returns `ANIMATION_INTERVAL` when any listed agent's state is `Active`; add `"{knowledge_graph}"` as the last line of `DEFAULT_TILE_TEMPLATES` and of `DEFAULT_GROUPED_TILE_TEMPLATES`. `DEFAULT_HORIZONTAL_TEMPLATES` and `DEFAULT_COMPACT_TEMPLATE` stay unchanged.

**Interfaces:**

- Consumes: `KnowledgeGraphState` (Task 1) and `SidebarSnapshot.knowledge_graph` (Task 3).
- Produces: the token `{knowledge_graph}` for user templates.

- [ ] **Step 1: Write the failing tests:**
  - `knowledge_graph_token_text` (context.rs): with no field set, the text is empty and the width 0; for each state the text is exactly the copy in Global Constraints. `Active` uses `palette.success` when `(spinner_frame / 2) % 2 == 0` and `palette.dimmed` otherwise; `Attention` uses `palette.warning`.
  - `knowledge_graph_line_fits_a_narrow_sidebar` (ui.rs, with the existing `drawn_row` helper): a 25-column sidebar with one agent in each state draws the line inside the pane, with the `▌` stripe first and `No knowledge graph` whole.
  - `active_knowledge_graph_animates` (app.rs): an app whose only agent is idle with an `Active` state gets `refresh_interval() == Some(250 ms)`.
- [ ] **Step 2: Run** `nice -n 19 cargo test --bin workmux sidebar::`. Expected: the new tests fail.
- [ ] **Step 3: Implement** the changes. Let the stale post-pass in `intrinsic_style` keep dimming stale agents, as it does for the other tokens.
- [ ] **Step 4: Run** `nice -n 19 cargo test --bin workmux`. Expected: everything passes. Existing tests that count tile lines may need their expected height raised by one; change only those.
- [ ] **Step 5: Commit** with the message `feat(sidebar): knowledge_graph token and line`.

### Task 5: Docs, FORK.md, checks and isolated try

**Files:**

- Modify: `docs/src/content/docs/guide/sidebar/customization.md`: the token in the token list, the default tile templates, and the four states.
- Modify: `FORK.md`: under "Fork features", add «Knowledge-graph line» (what it shows, the read-only probe and its accepted SQLite side files); a planned follow-up: Graphify, Serena and similar tools as configurable adapters (index marker, server signature, freshness probe).
- Modify: the spec, setting its status to `accepted 2026-11-12` and recording that the person chose `codegraph status` despite the SQLite side files.

- [ ] **Step 1: Write the docs and FORK.md entries** above.
- [ ] **Step 2: Run** `cargo fmt --all -- --check`, then `nice -n 19 cargo clippy --all-targets` (expected: 0 warnings), then `nice -n 19 cargo test --bin workmux` (expected: all pass).
- [ ] **Step 3: Run the integration suite.** The sidebar's behaviour changes. Expected: no new failures. Failures under load that pass when rerun alone are known.
- [ ] **Step 4: Try it on an isolated tmux server** (`tmux -L` with a private `HOME`, as in FORK.md "Checks"). Use three fake agents:
  1. one in a temp dir containing `.codegraph/codegraph.db` and a child process whose argv is `codegraph serve --mcp`, e.g. a Python script run through a symlink named `codegraph`. Expected: `●`, blinking.
  2. the same dir with no child. Expected: `⚠`.
  3. a plain temp dir with a server child. Expected: `○`.

  Put a stub `codegraph` script first in PATH that prints the measured JSON with no pending changes. Capture the sidebar.

- [ ] **Step 5: Commit** with the message `docs(fork): knowledge-graph line`. Then open a PR to `Balestrino/workmux` `main`, get an independent review, and merge with `--merge`.
